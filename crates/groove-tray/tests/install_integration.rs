//! Integration tests for the tray autostart install/uninstall scripts.
//!
//! These tests actually invoke `powershell.exe` and write to the user's
//! `%APPDATA%\Microsoft\Windows\Start Menu\Programs\Startup\` folder,
//! so they are gated behind `#[ignore]` and a unique service-name suffix
//! per process id to avoid collisions. Run with:
//!
//! ```sh
//! cargo test --package groove-tray --test install_integration -- --ignored
//! ```

#![cfg(target_os = "windows")]

use groove_tray::install::{build_install_script, build_uninstall_script};
use groove_tray::powershell::{decode_diagnostic, decode_value, with_utf8_output};
use std::path::PathBuf;
use std::process::Command;

/// Mirrors `install::run_ps`, including the UTF-8 output prelude.
///
/// Without the prelude this harness reproduces the very bug the production
/// helper was fixed for: the `.lnk` path comes back in the active code page,
/// `Path::new(&lnk_path).exists()` is then false, and the install test fails on
/// exactly the accounts it most needs to cover — those whose profile directory
/// contains non-ASCII characters.
fn run_ps(script: &str) -> (i32, String, String) {
    let out = Command::new("powershell.exe")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            &with_utf8_output(script),
        ])
        .output()
        .expect("spawn powershell");
    (
        out.status.code().unwrap_or(-1),
        decode_value(&out.stdout, "powershell stdout").expect("stdout must decode as UTF-8"),
        decode_diagnostic(&out.stderr),
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
    let service = format!("groove-test-{}", std::process::id());

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
    let service = format!("groove-test-noexist-{}", std::process::id());
    let uscript = build_uninstall_script(&service);
    let (code, _, stderr) = run_ps(&uscript);
    assert_eq!(
        code, 0,
        "uninstall on missing shortcut should succeed (idempotent): stderr={stderr}"
    );
}
