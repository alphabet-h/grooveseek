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
    let out = Command::new("powershell")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            &script,
        ])
        .output()
        .context("powershell Register-ScheduledTask invocation failed")?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let stdout = String::from_utf8_lossy(&out.stdout);
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
