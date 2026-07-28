use crate::process::{StopOutcome, stop_process_if_image_matches};
use anyhow::{Context, Result, bail};
use kb_mcp_tray::powershell::{CREATE_NO_WINDOW, decode_diagnostic, with_utf8_output};
use std::time::Duration;

/// File name of the daemon executable. Every stop is gated on the target
/// process actually being this program.
const DAEMON_IMAGE: &str = "kb-mcp.exe";

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

/// What the status endpoint told us about the daemon.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DaemonProbe {
    /// It answered and reported this pid.
    Pid(u32),
    /// It answered but reported no pid — a daemon older than the release that
    /// started publishing the field. It is definitely running.
    NoPid,
    /// It did not answer.
    Unreachable,
}

/// Stop the daemon.
///
/// `Stop-ScheduledTask` used to be the whole implementation, and since v0.9.1
/// it could not stop the daemon at all. The Windows Action points at
/// `kb-mcp-svc.exe`, which detach-spawns the daemon and exits immediately, so
/// the scheduler treats the task as finished and has nothing left to stop — the
/// cmdlet **returned success while the daemon kept running**. Keeping the
/// launcher alive would not have helped: measured on a probe task, stopping a
/// running task killed its own process and left the child alive.
///
/// So the pid from `/api/admin/status` drives the stop, terminated through
/// [`stop_process_if_image_matches`]. That covers pre-v0.9.1 installs too,
/// where the daemon *is* the task's own process, leaving `Stop-ScheduledTask`
/// as a best-effort tidy-up rather than the mechanism.
///
/// Whatever path runs, the outcome is confirmed against the daemon itself
/// before returning — see [`confirm_stopped`]. The pid is read first, because
/// the status endpoint dies with the daemon.
pub async fn stop(service_name: &str, status_url: &str, bind: &str) -> Result<()> {
    // Nothing here decides success. Each mechanism is attempted, and
    // `confirm_stopped` is the only authority on whether the daemon is gone —
    // including when a mechanism errors. Letting the fallback's error
    // propagate directly made stopping an already-stopped daemon fail whenever
    // the scheduled task was missing, which `restart` would then turn into
    // "never started" (caught by the end-to-end test below).
    let mut fallback_error: Option<anyhow::Error> = None;
    match probe_daemon(status_url).await {
        DaemonProbe::Pid(pid) => {
            match stop_process_if_image_matches(pid, DAEMON_IMAGE)? {
                StopOutcome::Terminated => {}
                StopOutcome::NotRunning => {
                    tracing::debug!("pid {pid} had already exited");
                }
                StopOutcome::NotOurProcess => {
                    // A stale status response whose pid has been recycled. The
                    // daemon may still be running under a different pid, which
                    // `confirm_stopped` will catch.
                    tracing::warn!(
                        "pid {pid} from the status endpoint is not {DAEMON_IMAGE}; \
                         leaving it alone"
                    );
                }
            }
            // On a pre-v0.9.1 install this clears the task instance the
            // terminated process belonged to. Nothing depends on it, so a
            // missing or unregistered task must not fail the stop.
            if let Err(e) = stop_scheduled_task(service_name).await {
                tracing::debug!("Stop-ScheduledTask after the pid stop failed (ignored): {e}");
            }
        }
        probe => {
            tracing::debug!("no usable pid ({probe:?}); falling back to Stop-ScheduledTask");
            if let Err(e) = stop_scheduled_task(service_name).await {
                tracing::debug!("fallback Stop-ScheduledTask failed: {e}");
                fallback_error = Some(e);
            }
        }
    }
    confirm_stopped(bind)
        .await
        .map_err(|e| match fallback_error {
            Some(fe) => e.context(format!("Stop-ScheduledTask also failed: {fe}")),
            None => e,
        })
}

/// Stop then start, with a short grace period so the listening socket is
/// released before the new instance binds it. The polling loop picks up the
/// recovery within the next ~5 seconds.
///
/// A failed stop propagates, which is the point: starting a second instance
/// while the first still holds the port produces a daemon that dies on bind and
/// a tray that reports it as started.
pub async fn restart(service_name: &str, status_url: &str, bind: &str) -> Result<()> {
    stop(service_name, status_url, bind).await?;
    tokio::time::sleep(Duration::from_millis(800)).await;
    start(service_name).await
}

async fn stop_scheduled_task(service_name: &str) -> Result<()> {
    let task = escape_single_quote(&task_name(service_name));
    run_powershell(&format!("Stop-ScheduledTask -TaskName '{}'", task)).await
}

/// Ask the daemon whether it is still there, rather than trusting whichever
/// mechanism just ran.
///
/// Every silent failure this module has had was a stop that reported success
/// without checking — `Stop-ScheduledTask` against a detached daemon, a
/// swallowed PowerShell error, a pid that could not be read. Confirming the
/// outcome settles all of them at once, and it is what makes [`restart`] safe.
///
/// Only [`Liveness::Gone`] — the port being free — counts, and it has to hold
/// several times running so a momentary blip cannot pass for a stop. Anything
/// else resets the run: an inconclusive probe is not progress towards proof
/// (codex P1 rounds 6 and 7 on PR #89, where a single failed request, and then
/// a run of timeouts, were each treated as evidence).
///
/// The socket takes a moment to be released after the process exits, which is
/// why this polls rather than checking once. Failing to establish the stop is
/// an error: the caller must not be told the daemon is down when that could not
/// be shown.
async fn confirm_stopped(bind: &str) -> Result<()> {
    /// Enough that a lone blip cannot be mistaken for a stop.
    const REQUIRED_CONSECUTIVE_FREE_PROBES: u32 = 3;
    const ATTEMPTS: u32 = 12;

    let authority = bind;
    let mut free_probes = 0;
    let mut last = Liveness::Inconclusive;
    for _ in 0..ATTEMPTS {
        last = probe_liveness(authority);
        match last {
            Liveness::Gone => {
                free_probes += 1;
                if free_probes >= REQUIRED_CONSECUTIVE_FREE_PROBES {
                    return Ok(());
                }
            }
            _ => free_probes = 0,
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    match last {
        Liveness::Occupied => {
            bail!("{authority} is still in use after the stop attempt; the daemon was not stopped")
        }
        _ => bail!(
            "could not confirm the daemon on {authority} stopped: the port could be \
             neither bound nor shown to be in use, so it may still be running"
        ),
    }
}

/// Read the daemon's pid, distinguishing "answered without a pid" from "did not
/// answer".
///
/// The difference matters: a daemon that answers is definitely running, so
/// falling back to `Stop-ScheduledTask` there is a guess that used to be
/// reported as success (codex P1 round 5 on PR #89). Both cases still take the
/// fallback, but [`confirm_stopped`] no longer lets either one pass unverified.
async fn probe_daemon(status_url: &str) -> DaemonProbe {
    let Ok(client) = status_client() else {
        return DaemonProbe::Unreachable;
    };
    let Ok(resp) = client.get(status_url).send().await else {
        return DaemonProbe::Unreachable;
    };
    if !resp.status().is_success() {
        return DaemonProbe::Unreachable;
    }
    match resp.json::<crate::state::AdminStatus>().await {
        Ok(status) => match status.daemon.pid {
            Some(pid) => DaemonProbe::Pid(pid),
            None => DaemonProbe::NoPid,
        },
        Err(_) => DaemonProbe::Unreachable,
    }
}

/// What a single probe of the daemon's port proves.
///
/// Established by **binding** the port rather than connecting to it. Two
/// earlier attempts were measured and rejected:
///
/// - an HTTP request, classifying refusals with `reqwest::Error::is_connect()`
///   — no refusal was ever classified that way, so a stopped daemon could never
///   be confirmed;
/// - a TCP connect, treating `ConnectionRefused` as the stop signal — on a host
///   whose firewall drops packets to closed ports instead of sending a reset,
///   every probe times out and, again, nothing can be confirmed. Measured
///   exactly that on the development machine.
///
/// Binding has neither problem. It is a local operation the network stack
/// answers definitively, and "can something else bind this port" is precisely
/// what [`restart`] needs to know before starting a new instance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Liveness {
    /// The port is taken: something still owns it.
    Occupied,
    /// The port is free, so nothing is serving on it. The only result that is
    /// evidence of a stop.
    Gone,
    /// Neither could be established — a permission problem, an address that is
    /// not local. Not evidence of anything, and in particular not of a stop
    /// (codex P1 rounds 6 and 7 on PR #89).
    Inconclusive,
}

fn probe_liveness(authority: &str) -> Liveness {
    // Binding and immediately dropping. Windows does not set SO_REUSEADDR on
    // listeners, so a port another socket is listening on genuinely refuses the
    // bind rather than quietly sharing it.
    match std::net::TcpListener::bind(authority) {
        Ok(_) => Liveness::Gone,
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => Liveness::Occupied,
        Err(_) => Liveness::Inconclusive,
    }
}

fn status_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(3))
        .build()
        .context("build status http client")
}

/// The one place this module starts `powershell.exe`.
///
/// The prelude is applied here rather than by each caller so that a script
/// added later cannot miss it. Only stderr is read back, and only to report a
/// failure — but that failure message is a localized cmdlet error on a
/// non-English host, which is precisely what decodes to U+FFFD without it.
async fn run_powershell(script: &str) -> Result<()> {
    let out = tokio::process::Command::new("powershell.exe")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            &with_utf8_output(script),
        ])
        // Without this every Start / Stop / Restart from the tray menu flashes
        // a console window on screen; see [`CREATE_NO_WINDOW`].
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .await
        .context("spawn powershell")?;
    if !out.status.success() {
        // Diagnostic decode: reporting *why* the task control failed matters
        // more than rejecting bytes the prelude should have prevented.
        anyhow::bail!("powershell failed: {}", decode_diagnostic(&out.stderr));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Stdio;

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

    /// The stop targets the daemon binary, never the console-hiding launcher.
    /// `kb-mcp-svc.exe` has already exited by the time the daemon is running,
    /// and terminating it would not stop anything.
    #[test]
    fn daemon_image_is_the_server_not_the_launcher() {
        assert_eq!(DAEMON_IMAGE, "kb-mcp.exe");
    }

    /// A socket that is bound but never answers must never be read as a stop.
    /// This is the shape a busy daemon has, and the confirmation timing out
    /// against it used to be reported as success — after which `restart` would
    /// start a second instance against the still-occupied port.
    #[tokio::test]
    async fn confirm_stopped_refuses_a_port_that_is_still_bound() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        // Accept and hold connections open without ever writing a response.
        std::thread::spawn(move || {
            let mut held = Vec::new();
            for stream in listener.incoming() {
                match stream {
                    Ok(s) => held.push(s),
                    Err(_) => break,
                }
            }
        });

        let err = confirm_stopped(&addr.to_string())
            .await
            .expect_err("a bound port must not be reported as stopped");
        assert!(
            err.to_string().contains("still in use"),
            "unexpected message: {err}"
        );
    }

    /// The reason [`stop`] confirms against the **configured** bind rather than
    /// the tray's loopback-normalised admin URL.
    ///
    /// Measured on Windows: with a listener on `0.0.0.0:P`, binding
    /// `127.0.0.1:P` **succeeds** — the kernel does not treat a wildcard
    /// listener as a conflict for a specific address unless the listener asked
    /// for exclusivity, which Rust's does not. Probing loopback would therefore
    /// report a wildcard-bound daemon as stopped, and a wildcard bind is a
    /// documented configuration. Binding the same address it was configured
    /// with is what makes the answer right (codex P2 round 8 on PR #89).
    #[test]
    fn probing_loopback_misses_a_wildcard_listener_but_the_bind_address_sees_it() {
        let daemon = std::net::TcpListener::bind("0.0.0.0:0").expect("wildcard bind");
        let port = daemon.local_addr().expect("addr").port();

        assert_eq!(
            probe_liveness(&format!("127.0.0.1:{port}")),
            Liveness::Gone,
            "loopback looks free even though the wildcard listener owns the port"
        );
        assert_eq!(
            probe_liveness(&format!("0.0.0.0:{port}")),
            Liveness::Occupied,
            "the configured bind address is what reveals the listener"
        );
    }

    /// A free port is the one case that does establish a stop.
    #[tokio::test]
    async fn confirm_stopped_accepts_a_free_port() {
        // Bind to grab a free port, then drop it so connections are refused.
        let addr = {
            let probe = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
            probe.local_addr().expect("addr")
        };
        confirm_stopped(&addr.to_string())
            .await
            .expect("a free port confirms the stop");
    }

    /// End-to-end: a real daemon, stopped through the real `stop` path — pid
    /// read over HTTP, terminated through the Win32 handle, outcome confirmed
    /// against the endpoint.
    ///
    /// The unit tests above cover the pieces, but this is the only thing that
    /// proves the assembled flow works, and PR #89 spent five review rounds on
    /// exactly that gap. `#[ignore]` because it needs the sibling `kb-mcp.exe`
    /// and downloads an embedding model on a cold cache; the nightly
    /// `windows-latest` leg added in AU-09 passes `--include-ignored`, so it
    /// does run in CI.
    #[tokio::test]
    #[ignore = "spawns a real kb-mcp daemon (needs the sibling binary + model cache)"]
    async fn stop_terminates_a_real_daemon_and_confirms_it() {
        let Some(kb_mcp) = locate_kb_mcp() else {
            panic!("kb-mcp.exe not found next to the test binary; run `cargo build` first");
        };
        let dir = scratch_dir("kb-mcp-tray-stop-e2e");
        std::fs::create_dir_all(dir.join("kb")).expect("create kb dir");
        std::fs::write(dir.join("kb").join("note.md"), "# Note\n\nbody\n").expect("write note");
        // A port unlikely to collide with the personal-http default (3100).
        let bind = "127.0.0.1:3187";
        std::fs::write(
            dir.join("kb-mcp.toml"),
            format!(
                "kb_path = '{}'\n\n[transport]\nkind = \"http\"\n\n[transport.http]\nbind = \"{bind}\"\n",
                dir.join("kb").display()
            ),
        )
        .expect("write config");

        let mut child = std::process::Command::new(&kb_mcp)
            .args(["serve", "--config"])
            .arg(dir.join("kb-mcp.toml"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn daemon");

        let status_url = format!("http://{bind}/api/admin/status");
        let client = status_client().expect("client");
        // Readiness is an HTTP question — the socket is bound before the server
        // can answer — so this waits for a real response rather than a
        // connectable port.
        let mut ready = false;
        for _ in 0..200 {
            if matches!(client.get(&status_url).send().await, Ok(r) if r.status().is_success()) {
                ready = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        assert!(ready, "daemon never became ready on {bind}");

        // The pid must come from the daemon itself, not from our spawn handle —
        // that is the path the tray takes.
        assert_eq!(
            probe_daemon(&status_url).await,
            DaemonProbe::Pid(child.id()),
            "the daemon must report its own pid"
        );

        stop("kb-mcp-e2e-no-such-task", &status_url, bind)
            .await
            .expect("stop must succeed and confirm the daemon is gone");

        assert_eq!(
            probe_liveness(bind),
            Liveness::Gone,
            "the port must be free once stop returned Ok"
        );
        child.wait().expect("reap");

        // Stopping again is a no-op, which is what lets `restart` recover from
        // an already-stopped daemon.
        stop("kb-mcp-e2e-no-such-task", &status_url, bind)
            .await
            .expect("stopping an already-stopped daemon must succeed");

        let _ = std::fs::remove_dir_all(&dir);
    }

    fn scratch_dir(prefix: &str) -> std::path::PathBuf {
        crate::test_support::unique_temp_path(prefix)
    }

    /// `kb-mcp` lives in a sibling package, so `CARGO_BIN_EXE_` is unavailable.
    /// Walk up from the test binary (`target/<profile>/deps/`) to the profile
    /// directory and look for it there.
    fn locate_kb_mcp() -> Option<std::path::PathBuf> {
        let exe = std::env::current_exe().ok()?;
        exe.ancestors().skip(1).take(3).find_map(|dir| {
            let candidate = dir.join("kb-mcp.exe");
            candidate.is_file().then_some(candidate)
        })
    }
}
