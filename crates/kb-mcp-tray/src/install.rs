#![cfg(target_os = "windows")]

//! Shell:startup `.lnk` shortcut install/uninstall via PowerShell
//! `WScript.Shell` COM. Re-uses the same `powershell.exe` invocation path
//! established by feature-43 (kb-mcp/src/service/windows.rs), so no new
//! dependency is required.

use anyhow::{Context, Result, anyhow};
use std::path::{Path, PathBuf};

/// Build the PowerShell script that creates a shell:startup `.lnk` shortcut
/// pointing to `tray_exe_path` with `--service-name <service>` as Arguments.
/// `working_directory` is set on the shortcut so the tray's logs / config
/// resolution start from a deterministic CWD.
pub fn build_install_script(
    service_name: &str,
    tray_exe_path: &Path,
    working_directory: &Path,
) -> String {
    let lnk_name = ps_quote(&format!("kb-mcp-tray-{}.lnk", service_name));
    let target = ps_quote(&tray_exe_path.display().to_string());
    let args = ps_quote(&format!("--service-name {}", service_name));
    let wd = ps_quote(&working_directory.display().to_string());
    let icon = ps_quote(&format!("{},0", tray_exe_path.display()));
    let desc = ps_quote(&format!("kb-mcp tray monitor for service {}", service_name));

    format!(
        r#"$ErrorActionPreference='Stop'
$startup = [Environment]::GetFolderPath('Startup')
$lnk = Join-Path $startup {lnk_name}
$shell = New-Object -ComObject WScript.Shell
$shortcut = $shell.CreateShortcut($lnk)
$shortcut.TargetPath = {target}
$shortcut.Arguments = {args}
$shortcut.WorkingDirectory = {wd}
$shortcut.IconLocation = {icon}
$shortcut.WindowStyle = 7
$shortcut.Description = {desc}
$shortcut.Save()
Write-Output $lnk
"#
    )
}

/// Build the PowerShell script that removes the shell:startup `.lnk`.
/// Idempotent: no-op if the file does not exist.
pub fn build_uninstall_script(service_name: &str) -> String {
    let lnk_name = ps_quote(&format!("kb-mcp-tray-{}.lnk", service_name));
    format!(
        r#"$ErrorActionPreference='Stop'
$startup = [Environment]::GetFolderPath('Startup')
$lnk = Join-Path $startup {lnk_name}
if (Test-Path $lnk) {{ Remove-Item $lnk -Force }}
Write-Output 'ok'
"#
    )
}

/// Build the PowerShell script that detects whether tray autostart is
/// already configured via any of three mechanisms: shell:startup .lnk,
/// HKCU\...\Run registry value, or Task Scheduler task. feature-44 only
/// uses the first; the other two are guarded against to avoid silently
/// overwriting a user's manual configuration.
pub fn build_duplicate_check_script(service_name: &str) -> String {
    let lnk_name = ps_quote(&format!("kb-mcp-tray-{}.lnk", service_name));
    let run_name = ps_quote(&format!("kb-mcp-tray-{}", service_name));
    let task_name = ps_quote(&format!(r"\kb-mcp-tray-{}", service_name));
    format!(
        r#"$ErrorActionPreference='SilentlyContinue'
$startup = [Environment]::GetFolderPath('Startup')
$lnk = Join-Path $startup {lnk_name}
$startup_exists = Test-Path $lnk
$run_exists = $null -ne (Get-ItemProperty -Path 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Run' -Name {run_name} -ErrorAction SilentlyContinue)
$task_exists = $null -ne (Get-ScheduledTask -TaskName {task_name} -ErrorAction SilentlyContinue)
@{{startup=$startup_exists; run=$run_exists; task=$task_exists}} | ConvertTo-Json -Compress
"#
    )
}

/// PowerShell single-quoted literal. Each embedded `'` is doubled.
fn ps_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// Preflight check: verify that `tray_exe_path` exists and that there is
/// no pre-existing autostart entry for this service (unless `force` is
/// set). Used by `kb-mcp service install --with-tray` to validate the
/// tray side BEFORE registering the daemon, so a tray failure does not
/// leave a half-installed service (codex P2 round 1 on PR #63).
pub fn preflight_check(service_name: &str, tray_exe_path: &Path, force: bool) -> Result<()> {
    if !tray_exe_path.exists() {
        return Err(anyhow!(
            "{} not found. Install kb-mcp-tray.exe from the v0.9.0 release zip into the same directory as kb-mcp.exe.",
            tray_exe_path.display()
        ));
    }
    if !force {
        let check = build_duplicate_check_script(service_name);
        let out = run_ps(&check)?;
        let v: serde_json::Value =
            serde_json::from_str(out.trim()).context("parse duplicate check JSON")?;
        if v["startup"].as_bool().unwrap_or(false)
            || v["run"].as_bool().unwrap_or(false)
            || v["task"].as_bool().unwrap_or(false)
        {
            return Err(anyhow!(
                "tray autostart entry already exists for service '{}'. Use --force to overwrite.",
                service_name
            ));
        }
    }
    Ok(())
}

/// Install the tray autostart shortcut. `force=true` skips the
/// duplicate-check. Returns the absolute path of the created `.lnk`.
pub fn install_autostart(
    service_name: &str,
    tray_exe_path: &Path,
    working_directory: &Path,
    force: bool,
) -> Result<PathBuf> {
    if !force {
        let check = build_duplicate_check_script(service_name);
        let out = run_ps(&check)?;
        let v: serde_json::Value =
            serde_json::from_str(out.trim()).context("parse duplicate check JSON")?;
        if v["startup"].as_bool().unwrap_or(false)
            || v["run"].as_bool().unwrap_or(false)
            || v["task"].as_bool().unwrap_or(false)
        {
            return Err(anyhow!(
                "tray autostart entry already exists for service '{}'. Use --force to overwrite.",
                service_name
            ));
        }
    }
    let script = build_install_script(service_name, tray_exe_path, working_directory);
    let lnk = run_ps(&script)?;
    Ok(PathBuf::from(lnk.trim()))
}

/// Remove the tray autostart shortcut. Idempotent — returns Ok(()) even
/// if the shortcut never existed.
pub fn uninstall_autostart(service_name: &str) -> Result<()> {
    let script = build_uninstall_script(service_name);
    let _ = run_ps(&script)?;
    Ok(())
}

fn run_ps(script: &str) -> Result<String> {
    let out = std::process::Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-Command", script])
        .output()
        .context("spawn powershell")?;
    if !out.status.success() {
        anyhow::bail!(
            "powershell failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn install_script_contains_required_lines() {
        let s = build_install_script(
            "kb-mcp",
            &PathBuf::from(r"C:\Users\x\.cargo\bin\kb-mcp-tray.exe"),
            &PathBuf::from(r"C:\Users\x\AppData\Roaming\kb-mcp\kb-mcp"),
        );
        assert!(s.contains("WScript.Shell"));
        assert!(s.contains("$shortcut.TargetPath ="));
        assert!(s.contains("$shortcut.Arguments ="));
        assert!(s.contains("$shortcut.WorkingDirectory ="));
        assert!(s.contains("--service-name kb-mcp"));
        assert!(s.contains("kb-mcp-tray-kb-mcp.lnk"));
        assert!(s.contains("WindowStyle = 7"));
    }

    #[test]
    fn install_script_escapes_apostrophe() {
        let s = build_install_script(
            "kb-mcp",
            &PathBuf::from(r"C:\Users\O'Brien\bin\kb-mcp-tray.exe"),
            &PathBuf::from(r"C:\Users\O'Brien\AppData\Roaming\kb-mcp\kb-mcp"),
        );
        // PowerShell single-quote escape: each ' becomes ''
        assert!(s.contains("O''Brien"));
        // Make sure the path is wrapped in single quotes (not broken open
        // by an unescaped apostrophe).
        let target_line = s
            .lines()
            .find(|l| l.contains("TargetPath"))
            .expect("TargetPath line");
        let single_quote_count = target_line.matches('\'').count();
        // Each ' is doubled, plus 2 outer quotes — so an even count.
        assert!(
            single_quote_count % 2 == 0,
            "unbalanced quotes: {target_line}"
        );
    }

    #[test]
    fn uninstall_script_is_idempotent() {
        let s = build_uninstall_script("work");
        assert!(s.contains("Test-Path"));
        assert!(s.contains("Remove-Item"));
        assert!(s.contains("kb-mcp-tray-work.lnk"));
    }

    #[test]
    fn duplicate_check_emits_three_signals() {
        let s = build_duplicate_check_script("kb-mcp");
        assert!(s.contains("$startup_exists"));
        assert!(s.contains("$run_exists"));
        assert!(s.contains("$task_exists"));
        assert!(s.contains("HKCU:\\Software\\Microsoft\\Windows\\CurrentVersion\\Run"));
        assert!(s.contains("Get-ScheduledTask"));
    }
}
