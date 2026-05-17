//! Integration tests for the tray autostart install/uninstall scripts.
//!
//! These tests actually invoke `powershell.exe` and write to the user's
//! `%APPDATA%\Microsoft\Windows\Start Menu\Programs\Startup\` folder,
//! so they are gated behind `#[ignore]` and a unique service-name suffix
//! per process id to avoid collisions. Run with:
//!
//! ```sh
//! cargo test --package kb-mcp-tray --test install_integration -- --ignored
//! ```

#![cfg(target_os = "windows")]

use kb_mcp_tray::install::{build_install_script, build_uninstall_script};
use std::path::PathBuf;
use std::process::Command;

fn run_ps(script: &str) -> (i32, String, String) {
    let out = Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-Command", script])
        .output()
        .expect("spawn powershell");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).to_string(),
        String::from_utf8_lossy(&out.stderr).to_string(),
    )
}

#[test]
#[ignore = "writes to %APPDATA%\\...\\Startup; run with: cargo test -- --ignored"]
fn install_autostart_creates_lnk_then_uninstall_removes_it() {
    // Use notepad.exe as a benign existing target so the .lnk validates
    // without us shipping a test binary. Unique service name per pid so
    // parallel test runs don't collide on the same .lnk path.
    let exe = PathBuf::from(r"C:\Windows\System32\notepad.exe");
    let wd = std::env::temp_dir();
    let service = format!("kb-mcp-test-{}", std::process::id());

    // Install
    let script = build_install_script(&service, &exe, &wd);
    let (code, stdout, stderr) = run_ps(&script);
    assert_eq!(code, 0, "install failed: stderr={stderr}");
    let lnk_path = stdout.trim().to_string();
    assert!(
        !lnk_path.is_empty() && std::path::Path::new(&lnk_path).exists(),
        "lnk not created: stdout={lnk_path}, stderr={stderr}"
    );

    // Uninstall
    let uscript = build_uninstall_script(&service);
    let (code, _, stderr) = run_ps(&uscript);
    assert_eq!(code, 0, "uninstall failed: stderr={stderr}");
    assert!(
        !std::path::Path::new(&lnk_path).exists(),
        "lnk still present after uninstall: {lnk_path}"
    );
}

#[test]
#[ignore = "invokes powershell; run with: cargo test -- --ignored"]
fn uninstall_is_idempotent_when_lnk_missing() {
    // Use a service-name that definitely has no shortcut on disk.
    let service = format!("kb-mcp-test-noexist-{}", std::process::id());
    let uscript = build_uninstall_script(&service);
    let (code, _, stderr) = run_ps(&uscript);
    assert_eq!(
        code, 0,
        "uninstall on missing shortcut should succeed (idempotent): stderr={stderr}"
    );
}
