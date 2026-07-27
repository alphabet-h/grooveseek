//! Shell:startup `.lnk` shortcut install/uninstall via PowerShell
//! `WScript.Shell` COM. Re-uses the same `powershell.exe` invocation path
//! established by feature-43 (kb-mcp/src/service/windows.rs), so no new
//! dependency is required.

use crate::powershell::{decode_diagnostic, decode_value, with_utf8_output};

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
            "{} not found. Install kb-mcp-tray.exe from the kb-mcp-tray-x86_64-pc-windows-msvc.zip archive of the matching release, into the same directory as kb-mcp.exe.",
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

/// The one place this module starts `powershell.exe`.
///
/// Concentrating the spawn here is what keeps [`with_utf8_output`] from being
/// forgotten: a new script reaches PowerShell only through this function, and
/// this function always applies it. Without the prelude the returned string is
/// the active code page decoded as if it were UTF-8, which for the caller of
/// [`install_autostart`] means a `.lnk` path full of U+FFFD.
fn run_ps(script: &str) -> Result<String> {
    let out = std::process::Command::new("powershell.exe")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            &with_utf8_output(script),
        ])
        .output()
        .context("spawn powershell")?;
    if !out.status.success() {
        // Diagnostic decode: the failure message must survive even if the
        // prelude did not, so this must not itself fail on bad bytes.
        anyhow::bail!("powershell failed: {}", decode_diagnostic(&out.stderr));
    }
    // Value decode: this becomes a `PathBuf` (or JSON) in the caller.
    decode_value(&out.stdout, "powershell stdout")
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

    /// End-to-end evidence that the prelude changes what `run_ps` actually
    /// returns, taken at the site where a corrupted value did the damage: this
    /// string is what [`install_autostart`] hands to `PathBuf::from`.
    ///
    /// The path is assembled from codepoints inside PowerShell so that nothing
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
    fn run_ps_round_trips_a_non_ascii_path() {
        let script = r"Write-Output ('C:\Users\' + [char]::ConvertFromUtf32(0x5C71) + [char]::ConvertFromUtf32(0x7530) + '\x.lnk')";
        let out = run_ps(script).expect("powershell must run");
        assert_eq!(out.trim(), r"C:\Users\山田\x.lnk");
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
