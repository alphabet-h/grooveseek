use anyhow::{Context, Result};
use std::time::Duration;

/// `kb-mcp-<service>` is the Task Scheduler task name registered by
/// `kb-mcp service install` (= feature-43 `kb-mcp/src/service/windows.rs`
/// `task_name` helper, line 31-32). PowerShell `Start-ScheduledTask
/// -TaskName <name>` accepts the bare name without a TaskPath prefix
/// (= codex P2 round 1 on PR #62: prefixing with `\` makes the cmdlet
/// search for a path that doesn't exist and daemon control fails).
pub fn task_name(service_name: &str) -> String {
    format!("kb-mcp-{}", service_name)
}

/// PowerShell single-quoted literal escape (= each `'` becomes `''`).
/// Reused from feature-43 (windows.rs) to keep task names with apostrophes
/// safe (= e.g. service names that contain `'` somehow, or unicode that
/// PowerShell would otherwise misinterpret).
pub fn escape_single_quote(s: &str) -> String {
    s.replace('\'', "''")
}

/// Async daemon start via PowerShell `Start-ScheduledTask`. Non-blocking
/// thanks to `tokio::process::Command`, so the event loop is not stalled
/// while PowerShell spins up.
pub async fn start(service_name: &str) -> Result<()> {
    let task = escape_single_quote(&task_name(service_name));
    run_powershell(&format!("Start-ScheduledTask -TaskName '{}'", task)).await
}

/// PowerShell that terminates `pid`, but only while it is still a `kb-mcp`
/// process.
///
/// The name guard matters because the pid comes from a status response that may
/// be a few seconds old, and Windows recycles pids — an unguarded
/// `Stop-Process` could kill whatever unrelated process inherited the number.
///
/// The exit-code plumbing is less obvious and was wrong twice before measuring
/// it (2026-07-26):
///
/// - `Get-Process -ErrorAction SilentlyContinue` suppresses the error *output*
///   for a pid that has already exited, but PowerShell still exits **1**,
///   because `-Command` derives its exit code from the last command's `$?`.
/// - Wrapping it in `try`/`catch` does not help either: the caught failure
///   still leaves `$?` false.
///
/// Both forms made `stop` fail whenever the daemon was already down, and since
/// [`restart`] propagates that with `?`, pressing Restart on a stopped daemon
/// would have aborted without ever starting it. Closing with an explicit
/// `exit 0` fixes the no-op paths.
///
/// The kill keeps `-ErrorAction Stop` so a genuine failure stays terminating
/// and aborts before `exit 0` is reached. But "the process is gone" must not
/// count as a failure even when it disappears *between* the lookup and the
/// kill: `handle_menu` spawns an independent task per click, so two overlapping
/// Stop/Restart clicks can both clear the lookup while only the first
/// terminates anything (codex P2 round 1 on PR #89). The catch therefore
/// re-checks and only rethrows if the process is still running.
///
/// Touching `$p.Handle` right after the lookup is what actually makes the name
/// guard binding. `Get-Process` hands back a `System.Diagnostics.Process` that
/// holds no OS handle, and reading `ProcessName` does not open one, so both
/// `Stop-Process -Id` and `Stop-Process -InputObject` end up re-opening the
/// process by its stored pid at kill time — a pid recycled after the guard
/// would be terminated in the replacement's place (codex P2 rounds 2 and 3).
/// Windows will not reuse a pid while a handle to that process is open, so
/// pinning one first means whatever gets validated is what gets killed.
///
/// Opening the handle can fail for two very different reasons, and only one of
/// them is benign. If the process is simply gone the stop has already happened
/// and the script falls through to `exit 0`; if it is still running — the tray
/// unelevated against an elevated daemon, or any other access restriction —
/// swallowing that would report a successful stop while the daemon keeps
/// serving, which is the exact silent success this whole function exists to
/// eliminate (codex P2 round 4). So the catch re-probes and rethrows whenever
/// the process is still there. Re-resolving the pid is safe here because the
/// result only chooses between failing loudly and doing nothing; nothing is
/// ever terminated through it.
pub fn stop_by_pid_script(pid: u32) -> String {
    format!(
        "$p = $null; \
         try {{ $p = Get-Process -Id {pid} -ErrorAction Stop }} catch {{ }}; \
         if ($p) \
         {{ try {{ $null = $p.Handle }} \
         catch {{ if (Get-Process -Id {pid} -ErrorAction SilentlyContinue) {{ throw }}; $p = $null }} }}; \
         if ($p -and $p.ProcessName -eq 'kb-mcp') \
         {{ try {{ $p | Stop-Process -Force -ErrorAction Stop }} \
         catch {{ if (-not $p.HasExited) {{ throw }} }} }}; \
         exit 0"
    )
}

/// Stop the daemon.
///
/// `Stop-ScheduledTask` used to be the whole implementation, and since v0.9.1 it
/// could not stop the daemon at all. The Windows Action now points at
/// `kb-mcp-svc.exe`, which detach-spawns the daemon and exits immediately, so
/// the scheduler treats the task as finished and has nothing left to stop — the
/// cmdlet **returned success while the daemon kept running**, and the tray
/// reported the stop as done.
///
/// Measured 2026-07-26 with a probe task: stopping a task whose own process was
/// still running killed that process and left its child alive. The scheduler's
/// reach never extends to descendants, so keeping the launcher alive would not
/// have rescued the cmdlet either.
///
/// So the pid reported by `/api/admin/status` is the primary path. It also
/// covers pre-v0.9.1 installs, where the daemon *is* the task's own process and
/// terminating it directly works just as well — which leaves
/// `Stop-ScheduledTask` as the fallback for when the pid cannot be read at all
/// (daemon unreachable, or too old to report it). Demoting it also means a
/// missing or unregistered task no longer blocks the path that actually works.
///
/// The pid is read **before** stopping anything, because the status endpoint
/// dies with the daemon.
pub async fn stop(service_name: &str, status_url: &str) -> Result<()> {
    let task = escape_single_quote(&task_name(service_name));
    let stop_task = format!("Stop-ScheduledTask -TaskName '{}'", task);
    match daemon_pid(status_url).await {
        Some(pid) => {
            run_powershell(&stop_by_pid_script(pid)).await?;
            // Best effort: on a pre-v0.9.1 install this clears the task
            // instance the terminated process belonged to. Nothing depends on
            // it succeeding, so a missing task must not fail the stop.
            if let Err(e) = run_powershell(&stop_task).await {
                tracing::debug!("Stop-ScheduledTask after pid stop failed (ignored): {e}");
            }
            Ok(())
        }
        None => {
            tracing::debug!(
                "daemon pid unavailable (daemon already down, or older than the release that \
                 reports it); falling back to Stop-ScheduledTask"
            );
            run_powershell(&stop_task).await
        }
    }
}

/// Best-effort read of the daemon's pid from the admin status endpoint. Any
/// failure — daemon already down, a daemon too old to report the field, a
/// malformed body — yields `None` so [`stop`] degrades to the scheduler path
/// instead of failing outright.
async fn daemon_pid(status_url: &str) -> Option<u32> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(3))
        .build()
        .ok()?;
    let resp = client.get(status_url).send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    resp.json::<crate::state::AdminStatus>()
        .await
        .ok()?
        .daemon
        .pid
}

/// Stop then start, with an 800ms grace period for the daemon process to
/// fully exit before relaunching. Polling loop will pick up the recovery
/// within the next ~5 seconds.
pub async fn restart(service_name: &str, status_url: &str) -> Result<()> {
    stop(service_name, status_url).await?;
    tokio::time::sleep(Duration::from_millis(800)).await;
    start(service_name).await
}

async fn run_powershell(script: &str) -> Result<()> {
    let out = tokio::process::Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-Command", script])
        .output()
        .await
        .context("spawn powershell")?;
    if !out.status.success() {
        anyhow::bail!(
            "powershell failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_name_uses_kb_mcp_prefix() {
        assert_eq!(task_name("kb-mcp"), "kb-mcp-kb-mcp");
        assert_eq!(task_name("work"), "kb-mcp-work");
        assert_eq!(task_name("a-b"), "kb-mcp-a-b");
    }

    #[test]
    fn escape_doubles_each_apostrophe() {
        assert_eq!(escape_single_quote("O'Brien"), "O''Brien");
        assert_eq!(escape_single_quote("plain"), "plain");
        assert_eq!(escape_single_quote("a'b'c"), "a''b''c");
        assert_eq!(escape_single_quote("''"), "''''");
    }

    /// The pid must reach both the lookup and the kill, and the kill must be
    /// gated on the process still being kb-mcp — an unguarded `Stop-Process`
    /// would kill whatever unrelated process recycled the pid.
    #[test]
    fn stop_by_pid_script_guards_on_process_name() {
        let script = stop_by_pid_script(4242);
        assert!(script.contains("Get-Process -Id 4242"));
        assert!(script.contains("$p | Stop-Process -Force"));
        assert!(script.contains("$p.ProcessName -eq 'kb-mcp'"));
    }

    /// An OS handle must be pinned before the name is checked. Without it the
    /// guard proves nothing: `Get-Process` retains no handle and reading
    /// `ProcessName` does not open one, so the kill re-opens the process by pid
    /// and a recycled pid would be terminated in the replacement's place (codex
    /// P2 rounds 2 and 3 on PR #89). Holding a handle is what stops Windows
    /// from reusing the pid at all.
    #[test]
    fn stop_by_pid_script_pins_a_handle_before_validating() {
        let script = stop_by_pid_script(4242);
        let handle = script.find("$p.Handle").expect("handle pinned");
        let guard = script.find("ProcessName").expect("guard present");
        assert!(
            handle < guard,
            "the handle must be opened before the name is checked: {script}"
        );
    }

    /// Nothing may ever be terminated through a raw pid. The pid resolves the
    /// process and later re-probes whether it is still alive, but every kill
    /// goes through the object whose handle was pinned — `Stop-Process -Id`
    /// would reopen by number and could hit a replacement.
    #[test]
    fn stop_by_pid_script_never_kills_through_a_raw_pid() {
        let script = stop_by_pid_script(4242);
        assert!(
            !script.contains("Stop-Process -Id"),
            "the kill must go through the pinned object, not the number: {script}"
        );
        assert!(script.contains("$p | Stop-Process -Force"));
    }

    /// A handle that cannot be opened while the process is still running is a
    /// real failure — the tray unelevated against an elevated daemon, say.
    /// Swallowing it would report a successful stop while the daemon keeps
    /// serving, which is the silent success this function exists to remove
    /// (codex P2 round 4 on PR #89). Only a process that has actually gone away
    /// may become a no-op. Verified by forcing the handle open to fail: exit 1
    /// with the process left running, exit 0 once it was gone.
    #[test]
    fn stop_by_pid_script_rethrows_a_handle_failure_on_a_live_process() {
        let script = stop_by_pid_script(4242);
        let handle = script.find("$p.Handle").expect("handle pinned");
        let guard = script.find("ProcessName").expect("guard present");
        let between = &script[handle..guard];
        assert!(
            between.contains("throw"),
            "a handle failure on a live process must rethrow: {script}"
        );
        assert!(
            between.contains("Get-Process -Id 4242"),
            "the rethrow must be gated on the process still existing: {script}"
        );
        assert!(
            between.contains("$p = $null"),
            "a process that is gone must clear $p so the guard skips: {script}"
        );
    }

    /// Stopping an already-stopped daemon has to succeed. Measured
    /// 2026-07-26: with `-ErrorAction SilentlyContinue` on the lookup — and
    /// also when merely wrapping it in `try`/`catch` — PowerShell exits 1 for a
    /// pid that no longer exists, which `run_powershell` turns into an error
    /// and `restart` propagates with `?`, so Restart on a stopped daemon would
    /// never reach `start`. The explicit `exit 0` is what makes the no-op paths
    /// succeed.
    #[test]
    fn stop_by_pid_script_exits_zero_when_there_is_nothing_to_kill() {
        let script = stop_by_pid_script(4242);
        assert!(
            script.trim_end().ends_with("exit 0"),
            "script must close with an explicit exit 0: {script}"
        );
        let lookup = script.find("Get-Process").expect("lookup present");
        let guard = script.find("ProcessName").expect("guard present");
        assert!(
            script[lookup..guard].contains("-ErrorAction Stop"),
            "the lookup must be terminating and caught; SilentlyContinue there \
             still exits 1 on a dead pid: {script}"
        );
    }

    /// Two overlapping Stop/Restart clicks can both clear the lookup while only
    /// the first terminates anything. The second must treat the process being
    /// gone as an already-successful stop, or it fails and `restart` returns
    /// before ever calling `start` (codex P2 on PR #89). Verified by running
    /// the catch branch against an exited pid: exit 0.
    #[test]
    fn stop_by_pid_script_tolerates_the_process_vanishing_mid_kill() {
        let script = stop_by_pid_script(4242);
        let kill = script.find("Stop-Process").expect("kill present");
        let tail = &script[kill..];
        assert!(
            tail.contains("catch") && tail.contains("$p.HasExited"),
            "the kill must be wrapped in a catch that re-checks the validated \
             process object: {script}"
        );
    }

    /// A kill that genuinely fails must still surface: the re-check finds the
    /// process still running and rethrows, which aborts before `exit 0`.
    /// Verified by driving the catch branch with a live process: exit 1.
    #[test]
    fn stop_by_pid_script_keeps_a_real_kill_failure_fatal() {
        let script = stop_by_pid_script(4242);
        let kill = script.find("Stop-Process").expect("kill present");
        let exit = script.find("exit 0").expect("exit present");
        let between = &script[kill..exit];
        assert!(
            between.contains("-ErrorAction Stop"),
            "the kill must be terminating: {script}"
        );
        assert!(
            between.contains("throw"),
            "a still-running process after a failed kill must rethrow: {script}"
        );
    }

    /// Pin the exact text. Every behaviour above was verified by running this
    /// literal string through `powershell.exe -NoProfile -NonInteractive
    /// -Command` on 2026-07-26 — live kb-mcp stopped (exit 0), pid already
    /// exited (exit 0), pid vanished mid-kill (exit 0), kill failed with the
    /// process still present (exit 1), foreign live pid untouched (exit 0).
    /// Substring assertions alone would not catch a spacing change from a
    /// `\`-continued literal swallowing the wrong run of whitespace, so the
    /// expected value is written on one line deliberately.
    #[test]
    fn stop_by_pid_script_matches_the_empirically_verified_text() {
        assert_eq!(
            stop_by_pid_script(4242),
            "$p = $null; try { $p = Get-Process -Id 4242 -ErrorAction Stop } catch { }; if ($p) { try { $null = $p.Handle } catch { if (Get-Process -Id 4242 -ErrorAction SilentlyContinue) { throw }; $p = $null } }; if ($p -and $p.ProcessName -eq 'kb-mcp') { try { $p | Stop-Process -Force -ErrorAction Stop } catch { if (-not $p.HasExited) { throw } } }; exit 0"
        );
    }

    /// The guard has to precede the kill; a script that terminates first and
    /// checks afterwards would defeat the whole point.
    #[test]
    fn stop_by_pid_script_checks_before_it_kills() {
        let script = stop_by_pid_script(7);
        let guard = script.find("ProcessName").expect("guard present");
        let kill = script.find("Stop-Process").expect("kill present");
        assert!(guard < kill, "guard must come before the kill: {script}");
    }
}
