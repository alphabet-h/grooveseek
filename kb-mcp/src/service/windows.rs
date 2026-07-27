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
    };
    match binary_path.parent() {
        Some(dir) => {
            let svc = dir.join("kb-mcp-svc.exe");
            if svc.exists() {
                ActionTarget {
                    execute_path: svc,
                    argument_clause: String::new(),
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
