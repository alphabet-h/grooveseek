// tests/service_install_integration.rs
// Integration tests for `kb-mcp service install/uninstall/status/list`.
// Unit-level tests (= no OS service register) run on every `cargo test`.
// Dangerous tests (= OS service register, marked #[ignore]) run only on
// `cargo test -- --ignored`.

mod common;
use common::temp::TempRoot;

#[allow(unused_imports)]
use std::path::PathBuf;

#[test]
fn install_resolves_kb_path_from_flag() {
    let tmp = TempRoot::new("install_flag");
    let kb = tmp.path().join("my-kb");
    std::fs::create_dir_all(&kb).unwrap();
    let result = kb_mcp::service::install::resolve_kb_path(Some(kb.clone()), None).unwrap();
    assert_eq!(result, kb);
}

#[test]
fn install_resolves_kb_path_from_toml_when_no_flag() {
    let tmp = TempRoot::new("install_toml");
    let kb = tmp.path().join("toml-kb");
    std::fs::create_dir_all(&kb).unwrap();
    let toml_path = tmp.path().join("kb-mcp.toml");
    // TOML literal strings ('...') do not interpret backslash escapes, which
    // matters on Windows where path separators are `\` (a double-quoted TOML
    // string would treat `\U` as a unicode escape and fail to parse).
    // Match the Config schema (top-level `kb_path`, no `[index]` section)
    // — `Config` uses `deny_unknown_fields` so unrecognised tables would crash
    // `kb-mcp serve` at startup. TOML literal strings ('...') do not
    // interpret backslash escapes (Windows `\U` issue).
    std::fs::write(&toml_path, format!("kb_path = '{}'\n", kb.display())).unwrap();
    let result = kb_mcp::service::install::resolve_kb_path(None, Some(toml_path)).unwrap();
    assert_eq!(result, kb);
}

#[test]
fn install_resolve_kb_path_errors_when_neither_provided() {
    assert!(kb_mcp::service::install::resolve_kb_path(None, None).is_err());
}

#[cfg(target_os = "linux")]
#[test]
fn linux_unit_template_renders_correctly() {
    use kb_mcp::service::*;
    let ctx = InstallContext {
        service_name: "kb-mcp".into(),
        kb_path: PathBuf::from("/home/u/kb"),
        bind: "127.0.0.1:3100".into(),
        config_home: PathBuf::from("/home/u/.config/kb-mcp/kb-mcp"),
        binary_path: PathBuf::from("/home/u/.cargo/bin/kb-mcp"),
        auto_start: true,
        force: false,
    };
    let unit = kb_mcp::service::linux::render_unit(&ctx);
    assert!(unit.contains("[Unit]"));
    assert!(unit.contains("ExecStart=/home/u/.cargo/bin/kb-mcp serve"));
    assert!(unit.contains("WorkingDirectory=/home/u/.config/kb-mcp/kb-mcp"));
    assert!(unit.contains("Description=kb-mcp loopback HTTP MCP server (kb-mcp)"));
    assert!(unit.contains("Restart=on-failure"));
}

#[cfg(target_os = "macos")]
#[test]
fn macos_plist_template_renders_correctly() {
    use kb_mcp::service::*;
    let ctx = InstallContext {
        service_name: "kb-mcp".into(),
        kb_path: PathBuf::from("/Users/me/kb"),
        bind: "127.0.0.1:3100".into(),
        config_home: PathBuf::from("/Users/me/Library/Application Support/kb-mcp/kb-mcp"),
        binary_path: PathBuf::from("/Users/me/.cargo/bin/kb-mcp"),
        auto_start: true,
        force: false,
    };
    let plist = kb_mcp::service::macos::render_plist(&ctx);
    assert!(plist.contains("<key>Label</key>"));
    assert!(plist.contains("<string>com.kb-mcp.kb-mcp</string>"));
    assert!(plist.contains("<string>/Users/me/.cargo/bin/kb-mcp</string>"));
    assert!(plist.contains("<string>serve</string>"));
    assert!(plist.contains("<key>WorkingDirectory</key>"));
}

/// v0.8.3 hot-fix smoke test: invokes `Register-ScheduledTask` via PowerShell
/// using the **Action / Trigger / Settings** parameter set — matches the
/// v0.8.3 production install path. Validates user-level root-path
/// registration without admin elevation (= spec § Q4 promise).
/// Cleans up via `Unregister-ScheduledTask` regardless of outcome.
///
/// **Must be run from an interactive logon session** (= the user's own
/// `powershell.exe` / Windows Terminal, NOT a network logon / service-style
/// session). The Task Scheduler COM API needs an interactive token to
/// register tasks for the current user — calls from NTLM / service logon
/// sessions (cargo-spawned shells inside CI runners, SSH sessions, WSL-
/// bridged shells) hit "Access is denied" even though the user is the same.
///
/// Run manually from an interactive shell:
/// ```text
/// cargo test --test service_install_integration windows_register_scheduledtask_smoke_test -- --ignored
/// ```
///
/// This smoke test is opt-in (= no CI coverage by design). Compile-time
/// shape correctness is covered by `cargo check` of the production helper.
#[cfg(target_os = "windows")]
#[test]
#[ignore = "mutates Windows Task Scheduler — run manually for end-to-end verify"]
fn windows_register_scheduledtask_smoke_test() {
    use std::process::Command;

    let unique = std::process::id();
    let svc_name = format!("smoke{}", unique);
    let task_name = format!("kb-mcp-{}", svc_name);

    let register_script = format!(
        "$ErrorActionPreference='Stop'; \
         $action = New-ScheduledTaskAction -Execute 'C:\\nonexistent\\kb-mcp.exe' -Argument 'serve' -WorkingDirectory 'C:\\nonexistent\\cfg'; \
         $trigger = New-ScheduledTaskTrigger -AtLogOn -User \"$env:USERDOMAIN\\$env:USERNAME\"; \
         $trigger.Enabled = $false; \
         $settings = New-ScheduledTaskSettingsSet -RestartCount 3 -RestartInterval (New-TimeSpan -Minutes 1) -Priority 7; \
         Register-ScheduledTask -TaskName '{name}' -Action $action -Trigger $trigger -Settings $settings -RunLevel Limited -Force | Out-Null",
        name = task_name,
    );
    let register = Command::new("powershell")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            &register_script,
        ])
        .output()
        .expect("powershell Register-ScheduledTask invocation failed");

    // Best-effort cleanup before assert so a failed test leaves no junk.
    let unregister_script = format!(
        "$ErrorActionPreference='SilentlyContinue'; \
         Unregister-ScheduledTask -TaskName '{name}' -Confirm:$false | Out-Null",
        name = task_name,
    );
    let _ = Command::new("powershell")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            &unregister_script,
        ])
        .output();

    assert!(
        register.status.success(),
        "Register-ScheduledTask failed (status: {:?})\nstderr: {}\nstdout: {}",
        register.status.code(),
        String::from_utf8_lossy(&register.stderr),
        String::from_utf8_lossy(&register.stdout),
    );
}

#[test]
fn uninstall_purge_without_yes_returns_abort_msg() {
    let result = kb_mcp::service::uninstall::run(kb_mcp::service::uninstall::UninstallParams {
        service_name: "test".into(),
        purge: true,
        yes: false,
    });
    let err = result.unwrap_err().to_string();
    assert!(err.contains("--yes") || err.contains("confirm"));
}
