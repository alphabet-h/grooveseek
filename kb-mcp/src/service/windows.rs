//! Windows Task Scheduler backend for kb-mcp service.
//!
//! Module-level `#[cfg(target_os = "windows")]` lives on the `pub mod windows;`
//! declaration in `src/service/mod.rs`; no inner `#![cfg]` needed.
//!
//! ## Why PowerShell cmdlets, not `schtasks` / `-Xml`
//!
//! The Phase 1 install needs to register a task at the root path (`\<name>`)
//! under the user's normal (non-elevated) shell — spec § Q4 promised "no admin
//! required". The three rejected approaches:
//!
//! 1. **`schtasks /Create /XML`** (v0.8.0 / v0.8.1 attempts) — even with a
//!    correctly UTF-16 LE BOM-encoded XML, returns "Access is denied" on
//!    root-path registration from a non-elevated shell. The legacy CLI
//!    apparently doesn't go through the COM API path used by the PowerShell
//!    module.
//! 2. **`Register-ScheduledTask -Xml`** (v0.8.2 attempt) — XML parameter set
//!    doesn't auto-populate a `<UserId>` in the Principal, so Task Scheduler
//!    falls back to a user-ambiguous principal that needs admin. Returns
//!    HRESULT 0x80070005 from the same non-elevated shell.
//! 3. **`Register-ScheduledTask -Action -Trigger -Settings`** (v0.8.3, current)
//!    — cmdlet auto-builds the Principal from the current logon identity, so
//!    user-level registration just works.

use super::powershell::{decode_diagnostic, with_utf8_output};
use super::{InstallContext, ServiceBackend, ServiceState};
use anyhow::{Context, Result, anyhow};
use std::path::{Path, PathBuf};
use std::process::Command;

pub(crate) struct TaskScheduler;

fn task_name(service_name: &str) -> String {
    format!("kb-mcp-{}", service_name)
}

fn run_schtasks(args: &[&str]) -> Result<()> {
    let status = Command::new("schtasks")
        .args(args)
        .status()
        .with_context(|| format!("schtasks {} 実行失敗", args.join(" ")))?;
    if !status.success() {
        return Err(anyhow!(
            "schtasks {} 失敗 (status: {})",
            args.join(" "),
            status
        ));
    }
    Ok(())
}

/// The `-Argument` clause a console-binary Action has to carry.
///
/// **Invariant, enforced jointly with `crates/kb-mcp-svc/src/main.rs`:** exactly
/// one side supplies `serve`. `kb-mcp-svc.exe` prepends it unconditionally, so
/// an Action aimed at the svc launcher must pass **no** argument, while an
/// Action aimed at `kb-mcp.exe` must pass this clause. Breaking either half
/// produces `kb-mcp.exe serve serve` (or a bare `kb-mcp.exe` with no
/// subcommand) — and only at the *next logon*, long after the install command
/// reported success. Both halves are unit-tested for that reason.
const LEGACY_SERVE_ARGUMENT: &str = " -Argument 'serve'";

/// Which binary the AtLogOn Action executes, plus the `-Argument` clause that
/// must accompany it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActionTarget {
    pub execute_path: PathBuf,
    /// Empty when `execute_path` is the svc launcher (it adds `serve` itself),
    /// otherwise `" -Argument 'serve'"`.
    pub argument_clause: String,
    /// `false` when the svc launcher was missing and the console-visible
    /// fallback was taken.
    ///
    /// Carried in the return value rather than reported from inside
    /// [`resolve_action_target`] so that function stays pure — which is what
    /// makes it testable without a real install layout (AU-07 / AU-08). The
    /// caller is responsible for telling the user; leaving the fallback silent
    /// is what AU-67 was about.
    pub used_svc_launcher: bool,
}

/// (v0.9.1 hot-fix) Decide what the AtLogOn Action should execute.
///
/// Prefer the windows-subsystem `kb-mcp-svc.exe` sibling when present so Task
/// Scheduler spawns a console-less launcher that detach-spawns
/// `kb-mcp.exe serve`. Without this the bare `kb-mcp.exe` Action surfaces a
/// visible console window on every login — Windows allocates conhost before the
/// process starts, so even `-WindowStyle Hidden` / `FreeConsole()` only hide it
/// *after* a ~1 sec flash. The svc binary has
/// `#![windows_subsystem = "windows"]`, so the kernel never allocates a console
/// for the parent and the child kb-mcp inherits an empty console handle = a true
/// 0-flash launch. When the sibling is absent (e.g. `cargo install --path
/// kb-mcp` installs only the main binary) fall back to the legacy
/// console-visible Action so a dev install still yields a working daemon.
///
/// This is the only part of the registration path that touches the filesystem;
/// keeping it separate from [`build_register_script`] is what makes the script
/// itself testable without a real install layout.
pub fn resolve_action_target(binary_path: &Path) -> ActionTarget {
    let legacy = || ActionTarget {
        execute_path: binary_path.to_path_buf(),
        argument_clause: LEGACY_SERVE_ARGUMENT.to_string(),
        used_svc_launcher: false,
    };
    match binary_path.parent() {
        Some(dir) => {
            let svc = dir.join("kb-mcp-svc.exe");
            if svc.exists() {
                ActionTarget {
                    execute_path: svc,
                    argument_clause: String::new(),
                    used_svc_launcher: true,
                }
            } else {
                legacy()
            }
        }
        None => legacy(),
    }
}

/// (v0.8.3 hot-fix) Render the PowerShell script that registers the task at the
/// root path via the `Register-ScheduledTask` cmdlet's **Action / Trigger /
/// Settings** parameter set. That parameter set is the only proven path that
/// works under a user-level (non-elevated) shell — see the module-level
/// doc-comment for the history of the `schtasks /Create` and
/// `Register-ScheduledTask -Xml` failures.
///
/// `service_name` is upstream-validated by `validate_service_name`
/// (= `[a-zA-Z0-9_-]+`) so it cannot contain `'`. Paths from `InstallContext`
/// may include the user's profile directory — accounts like `O'Brien` would
/// produce paths with `'`, which we double-escape per PowerShell single-quote
/// string rules (= `'` → `''`).
///
/// Pure: no filesystem access, no process spawn. The sibling probe lives in
/// [`resolve_action_target`].
pub fn build_register_script(
    service_name: &str,
    target: &ActionTarget,
    config_home: &Path,
    auto_start: bool,
    force: bool,
) -> String {
    let task = task_name(service_name);
    let bin_escaped = target
        .execute_path
        .display()
        .to_string()
        .replace('\'', "''");
    let home_escaped = config_home.display().to_string().replace('\'', "''");
    let auto_start_val = if auto_start { "$true" } else { "$false" };
    let force_clause = if force { " -Force" } else { "" };

    // `$ErrorActionPreference='Stop'` ensures cmdlet failures propagate as
    // non-zero exit codes. `$trigger.Enabled = $false` honors --no-auto-start
    // at the OS layer (= the LogonTrigger is registered but inert). The
    // `-User "$env:USERDOMAIN\$env:USERNAME"` on the trigger pins the
    // logon target to the current user (= matches the principal the cmdlet
    // auto-constructs for registration, no admin needed).
    format!(
        "$ErrorActionPreference='Stop'; \
         $action = New-ScheduledTaskAction -Execute '{bin}'{argument} -WorkingDirectory '{home}'; \
         $trigger = New-ScheduledTaskTrigger -AtLogOn -User \"$env:USERDOMAIN\\$env:USERNAME\"; \
         $trigger.Enabled = {auto_start}; \
         $settings = New-ScheduledTaskSettingsSet -RestartCount 3 -RestartInterval (New-TimeSpan -Minutes 1) -Priority 7; \
         Register-ScheduledTask -TaskName '{name}' -Action $action -Trigger $trigger -Settings $settings -RunLevel Limited -Description 'kb-mcp loopback HTTP MCP server ({name})'{force} | Out-Null",
        bin = bin_escaped,
        argument = target.argument_clause,
        home = home_escaped,
        auto_start = auto_start_val,
        name = task,
        force = force_clause,
    )
}

/// The one place this backend starts `powershell.exe`.
///
/// Concentrating the spawn here is what keeps the UTF-8 output prelude from
/// being forgotten: a new script reaches PowerShell only through this function,
/// and this function always applies [`with_utf8_output`]. Without it every
/// message below is decoded from the active code page as if it were UTF-8 — see
/// [`super::powershell`] for the measurements.
fn run_powershell_capture(script: &str, what: &str) -> Result<std::process::Output> {
    Command::new("powershell")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            &with_utf8_output(script),
        ])
        .output()
        .with_context(|| format!("powershell {what} invocation failed"))
}

/// (AU-67) What to print when the svc launcher is missing and the
/// console-visible Action was registered instead.
///
/// The fallback produces a working install, so this is a warning and not an
/// error — but it must not stay silent. `kb-mcp-svc.exe` was not attached to a
/// release at all until v0.14.0, so **every** installation from a release
/// archive between v0.9.0 and v0.13.1 took this branch: users saw a console
/// window at each logon while the v0.9.1 fix meant to prevent it appeared to be
/// in place. Nothing reported the substitution, and the distribution bug stayed
/// invisible for eight releases. Silence about an "optional optimisation" not
/// applying is what made a detectable defect undetectable.
///
/// Kept separate from the printing so the wording — specifically the archive
/// name a user has to go find — is pinned by a test.
///
/// `auto_start` decides when the console actually appears. `--no-auto-start`
/// registers the task with `$trigger.Enabled = $false`, so it does not run at
/// logon at all; promising a window "at every logon" there would describe a
/// symptom the user will never see and make the rest of the advice look wrong.
fn svc_fallback_warning(binary_path: &Path, auto_start: bool) -> String {
    let when = if auto_start {
        "at every logon"
    } else {
        "each time the task is started"
    };
    format!(
        "warning: kb-mcp-svc.exe was not found next to {}.\n         \
         Registered the task to run kb-mcp.exe directly, which shows a\n         \
         console window {when}. To avoid it, extract\n         \
         kb-mcp-svc-x86_64-pc-windows-msvc.zip from the release next to\n         \
         kb-mcp.exe and run `kb-mcp service install --force` again.",
        binary_path.display()
    )
}

/// Register the scheduled task: resolve the Action target, render the script,
/// hand it to PowerShell.
fn register_via_powershell(
    service_name: &str,
    binary_path: &Path,
    config_home: &Path,
    auto_start: bool,
    force: bool,
) -> Result<()> {
    let target = resolve_action_target(binary_path);
    let script = build_register_script(service_name, &target, config_home, auto_start, force);
    let out = run_powershell_capture(&script, "Register-ScheduledTask")?;
    if !out.status.success() {
        // Diagnostic decode: a failed registration must still report why it
        // failed, so an undecodable byte must not turn into a second error that
        // replaces the first.
        let stderr = decode_diagnostic(&out.stderr);
        let stdout = decode_diagnostic(&out.stdout);
        return Err(anyhow!(
            "PowerShell Register-ScheduledTask failed (status: {})\nstderr: {}\nstdout: {}",
            out.status,
            stderr.trim(),
            stdout.trim(),
        ));
    }
    // AU-67, after the success check: the warning is worded in the past tense
    // ("Registered the logon task ..."), which is only true once the cmdlet has
    // actually run. Printed before it, a registration that then fails — the
    // reproducible case being an existing task without `--force` — would emit
    // "Registered ..." immediately followed by "Register-ScheduledTask failed",
    // and nothing would have been registered at all. A failed install has one
    // actionable message, and it is not this one.
    if !target.used_svc_launcher {
        eprintln!("{}", svc_fallback_warning(binary_path, auto_start));
    }
    Ok(())
}

impl ServiceBackend for TaskScheduler {
    fn install(&self, ctx: &InstallContext) -> Result<()> {
        // v0.8.3: skip XML entirely — pass Action/Trigger/Settings directly
        // to Register-ScheduledTask (= the parameter set that auto-populates
        // the Principal from the current logon identity, the only path that
        // works without admin elevation on a non-elevated shell).
        register_via_powershell(
            &ctx.service_name,
            &ctx.binary_path,
            &ctx.config_home,
            ctx.auto_start,
            ctx.force,
        )?;
        if ctx.auto_start {
            run_schtasks(&["/Run", "/TN", &task_name(&ctx.service_name)])?;
        }
        Ok(())
    }
    fn uninstall(&self, service_name: &str) -> Result<()> {
        let task = task_name(service_name);
        let _ = run_schtasks(&["/End", "/TN", &task]);
        let _ = run_schtasks(&["/Delete", "/TN", &task, "/F"]);
        Ok(())
    }
    fn status(&self, service_name: &str) -> Result<ServiceState> {
        let task = task_name(service_name);
        let out = Command::new("schtasks")
            .args(["/Query", "/TN", &task, "/FO", "CSV", "/NH"])
            .output()
            .context("schtasks /Query 実行失敗")?;
        if !out.status.success() {
            return Ok(ServiceState::NotFound);
        }
        // `schtasks` is not PowerShell, so the UTF-8 prelude cannot reach it and
        // this CSV really is active-code-page encoded. Lossy decoding is
        // nonetheless safe *for this parse*: the only fields read are the task
        // name and the literal `Running`, both ASCII, and no CP932 trail byte is
        // `,` or `"` (the trail ranges are 0x40-0x7E and 0x80-0xFC), so field
        // splitting cannot be thrown off by a Japanese task name elsewhere in
        // the row. The status parse below is wrong for other reasons — see
        // `list`.
        let stdout = String::from_utf8_lossy(&out.stdout);
        Ok(if stdout.contains("Running") {
            ServiceState::Running {
                uptime_secs: 0,
                bind: None,
                kb_path: None,
                model: None,
            }
        } else {
            ServiceState::Stopped {
                bind: None,
                kb_path: None,
            }
        })
    }
    fn list(&self) -> Result<Vec<(String, ServiceState)>> {
        let out = Command::new("schtasks")
            .args(["/Query", "/FO", "CSV", "/NH"])
            .output()
            .context("schtasks /Query 全体 実行失敗")?;
        // Same as `status`: `schtasks` output is code-page encoded and out of
        // reach of the PowerShell prelude, but only the ASCII `kb-mcp-` prefix
        // is matched here. A separate defect does live on this path — a service
        // literally named `Running` would report Running forever, because the
        // status check greps the whole CSV row rather than the state column.
        // Moving both calls to `Get-ScheduledTask`, whose `TaskState` is a typed
        // value rather than a localized CSV, retires the encoding question and
        // that defect together.
        let stdout = String::from_utf8_lossy(&out.stdout);
        let mut result = Vec::new();
        for line in stdout.lines() {
            if let Some(name_field) = line.split(',').next() {
                let cleaned = name_field.trim_matches('"').trim_start_matches('\\');
                if let Some(rest) = cleaned.strip_prefix("kb-mcp-") {
                    let state = self.status(rest)?;
                    result.push((rest.to_string(), state));
                }
            }
        }
        Ok(result)
    }
    fn stop(&self, service_name: &str) -> Result<()> {
        run_schtasks(&["/End", "/TN", &task_name(service_name)])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::service::powershell::decode_value;

    /// AU-67. The point of the warning is that a user can act on it, which
    /// means naming the archive to download — a message saying only "svc not
    /// found" would leave them exactly where the silent fallback did.
    #[test]
    fn svc_fallback_warning_tells_the_user_what_to_do() {
        let msg = svc_fallback_warning(Path::new("C:\\bin\\kb-mcp.exe"), true);

        assert!(
            msg.contains("kb-mcp-svc-x86_64-pc-windows-msvc.zip"),
            "must name the archive to extract: {msg}"
        );
        assert!(
            msg.contains("console window"),
            "must name the symptom the user will otherwise see: {msg}"
        );
        assert!(
            msg.contains("--force"),
            "must say how to redo the install: {msg}"
        );
        assert!(
            msg.contains("C:\\bin\\kb-mcp.exe"),
            "must say where it looked: {msg}"
        );
    }

    /// The message is one literal split across source lines with `\` joins,
    /// which silently eat the newline *and* the following indentation — a
    /// miscount still compiles and produces ragged output. Pin the shape.
    /// `--no-auto-start` registers the task with `$trigger.Enabled = $false`,
    /// so it never fires at logon. Promising a window "at every logon" there
    /// describes a symptom the user will never see, which makes the rest of the
    /// advice look wrong too.
    #[test]
    fn svc_fallback_warning_matches_when_the_task_actually_runs() {
        let path = Path::new("C:\\bin\\kb-mcp.exe");

        let at_logon = svc_fallback_warning(path, true);
        assert!(
            at_logon.contains("at every logon"),
            "an auto-start install does flash on each logon: {at_logon}"
        );

        let on_demand = svc_fallback_warning(path, false);
        assert!(
            !on_demand.contains("logon"),
            "--no-auto-start never fires at logon, so the message must not say so: {on_demand}"
        );
        assert!(
            on_demand.contains("each time the task is started"),
            "it must still say when the window appears: {on_demand}"
        );

        // The actionable half is identical either way.
        for msg in [&at_logon, &on_demand] {
            assert!(msg.contains("kb-mcp-svc-x86_64-pc-windows-msvc.zip"));
            assert!(msg.contains("--force"));
        }
    }

    #[test]
    fn svc_fallback_warning_lines_are_aligned() {
        let msg = svc_fallback_warning(Path::new("C:\\bin\\kb-mcp.exe"), true);
        let mut lines = msg.lines();

        assert!(
            lines.next().is_some_and(|l| l.starts_with("warning: ")),
            "first line carries the prefix: {msg}"
        );
        for line in lines {
            assert!(
                line.starts_with("         ") && !line.starts_with("          "),
                "continuation lines are indented to exactly 9 columns: {line:?}"
            );
        }
    }

    #[test]
    fn resolve_action_target_reports_which_branch_it_took() {
        // No `kb-mcp-svc.exe` can exist next to a path under a directory that
        // does not exist, so this exercises the fallback without a fixture.
        let missing = Path::new("C:\\kb-mcp-au67-no-such-dir\\kb-mcp.exe");
        let target = resolve_action_target(missing);
        assert!(
            !target.used_svc_launcher,
            "fallback must be reported, not silent"
        );
        assert_eq!(target.execute_path, missing);
        assert_eq!(target.argument_clause, LEGACY_SERVE_ARGUMENT);
    }

    /// End-to-end evidence that the prelude changes what actually comes back
    /// through the pipe. Asserting that the script string contains the prelude
    /// would only restate the code; this spawns a real `powershell.exe`.
    ///
    /// The text is built from codepoints inside PowerShell so that nothing
    /// between this file and the child re-encodes it on the way in, leaving the
    /// pipe as the only thing under test. With the prelude the answer is UTF-8
    /// on any code page; without it a ja-JP host emits CP932 (rejected by
    /// `decode_value`) and a host on a Latin code page emits best-fit `??`
    /// (decodes cleanly but compares unequal), so on either the fix is what
    /// makes this pass.
    ///
    /// It does **not** guard the prelude everywhere. A host whose active code
    /// page is already UTF-8 (CP65001, which Windows 11's "Beta: Use Unicode
    /// UTF-8" option turns on) emits UTF-8 with or without it, and there this
    /// passes either way. That still covers exactly the hosts the fix exists
    /// for — but if the Windows CI runner ever moves to a UTF-8 code page, this
    /// stops being a regression guard without saying so.
    #[test]
    fn powershell_output_round_trips_non_ascii() {
        let script =
            "Write-Output ([char]::ConvertFromUtf32(0x65E5) + [char]::ConvertFromUtf32(0x672C))";
        let out = run_powershell_capture(script, "encoding self-test").expect("spawn powershell");
        assert!(
            out.status.success(),
            "powershell failed: {}",
            decode_diagnostic(&out.stderr)
        );
        let stdout = decode_value(&out.stdout, "stdout").expect("stdout must decode as UTF-8");
        assert_eq!(stdout.trim(), "日本");
    }
}
