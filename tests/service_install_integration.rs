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

#[cfg(target_os = "windows")]
#[test]
fn windows_task_xml_renders_correctly() {
    use kb_mcp::service::*;
    let ctx = InstallContext {
        service_name: "kb-mcp".into(),
        kb_path: PathBuf::from(r"C:\Users\me\kb"),
        bind: "127.0.0.1:3100".into(),
        config_home: PathBuf::from(r"C:\Users\me\AppData\Roaming\kb-mcp\kb-mcp"),
        binary_path: PathBuf::from(r"C:\Users\me\.cargo\bin\kb-mcp.exe"),
        auto_start: true,
        force: false,
    };
    let xml = kb_mcp::service::windows::render_task_xml(&ctx);
    assert!(xml.contains("LogonTrigger"));
    assert!(xml.contains("LeastPrivilege"));
    assert!(xml.contains(r"C:\Users\me\.cargo\bin\kb-mcp.exe"));
    assert!(xml.contains(r"C:\Users\me\AppData\Roaming\kb-mcp\kb-mcp"));
    assert!(xml.contains("kb-mcp-kb-mcp"));
    // v0.8.1 hot-fix: XML declares UTF-16 (= matches the BOM-prefixed UTF-16 LE
    // bytes written by `encode_utf16_le_bom`).
    assert!(xml.contains(r#"encoding="UTF-16""#));
}

/// v0.8.2 hot-fix smoke test: invokes `Register-ScheduledTask -Xml` via
/// PowerShell with the produced UTF-16 LE BOM bytes — matches the v0.8.2
/// production install path. Verifies user-level root-path registration
/// works without elevation (= the spec § Q4 "Phase 1 = no admin" promise).
/// Cleans up via `Unregister-ScheduledTask` regardless of test outcome.
///
/// **Must be run from an interactive logon session** (= the user's own
/// `cmd.exe` / `powershell.exe` / Windows Terminal, NOT a network logon /
/// service-style session). PowerShell scheduled-task APIs need an
/// interactive token to talk to the Task Scheduler service for the current
/// user — calls from NTLM / service logon sessions (cargo-spawned shells
/// inside CI runners, SSH sessions, WSL-bridged shells, etc.) hit
/// "Access is denied" even though the user is the same.
///
/// Run manually from an interactive shell:
/// ```text
/// cargo test --test service_install_integration windows_register_scheduledtask_smoke_test -- --ignored
/// ```
///
/// The byte-level test `windows_task_xml_is_utf16_le_with_bom` is the primary
/// CI regression guard for the XML shape; this smoke test is end-to-end OS
/// integration and explicitly opt-in (= no CI coverage by design).
#[cfg(target_os = "windows")]
#[test]
#[ignore = "mutates Windows Task Scheduler — run manually for end-to-end verify"]
fn windows_register_scheduledtask_smoke_test() {
    use kb_mcp::service::*;
    use std::process::Command;

    let unique = std::process::id();
    let svc_name = format!("smoke{}", unique);
    let ctx = InstallContext {
        service_name: svc_name.clone(),
        kb_path: PathBuf::from(r"C:\nonexistent\kb"),
        bind: "127.0.0.1:9999".into(),
        config_home: PathBuf::from(r"C:\nonexistent\cfg"),
        binary_path: PathBuf::from(r"C:\nonexistent\kb-mcp.exe"),
        auto_start: false,
        force: false,
    };
    let xml = kb_mcp::service::windows::render_task_xml(&ctx);
    let bytes = kb_mcp::service::windows::encode_utf16_le_bom(&xml);
    let tmp = std::env::temp_dir().join(format!("kbmcp-ps-smoke-{}.xml", unique));
    std::fs::write(&tmp, bytes).unwrap();

    let task_name = format!("kb-mcp-{}", svc_name);
    // codex P2 round 1 on PR #59: match the production helper's PowerShell
    // single-quote escaping so usernames containing apostrophes (= O'Brien)
    // don't break the inline literal.
    let escaped_path = tmp.display().to_string().replace('\'', "''");
    let register_script = format!(
        "$ErrorActionPreference='Stop'; \
         $xml = [System.IO.File]::ReadAllText('{path}'); \
         Register-ScheduledTask -TaskName '{name}' -Xml $xml -Force | Out-Null",
        path = escaped_path,
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
    let _ = std::fs::remove_file(&tmp);

    assert!(
        register.status.success(),
        "Register-ScheduledTask failed (status: {:?})\nstderr: {}\nstdout: {}",
        register.status.code(),
        String::from_utf8_lossy(&register.stderr),
        String::from_utf8_lossy(&register.stdout),
    );
}

/// v0.8.1 hot-fix regression: schtasks /XML on Japanese-locale Windows
/// requires UTF-16 LE bytes with `0xFF 0xFE` BOM. Re-emitting plain UTF-8
/// (= the v0.8.0 code path) caused "エンコードを切り替えることができません".
/// This test pins the exact byte sequence so a future "encoding cleanup"
/// can't silently revert.
#[cfg(target_os = "windows")]
#[test]
fn windows_task_xml_is_utf16_le_with_bom() {
    use kb_mcp::service::*;
    let ctx = InstallContext {
        service_name: "kb-mcp".into(),
        kb_path: PathBuf::from(r"C:\kb"),
        bind: "127.0.0.1:3100".into(),
        config_home: PathBuf::from(r"C:\cfg"),
        binary_path: PathBuf::from(r"C:\bin\kb-mcp.exe"),
        auto_start: true,
        force: false,
    };
    let xml = kb_mcp::service::windows::render_task_xml(&ctx);
    let bytes = kb_mcp::service::windows::encode_utf16_le_bom(&xml);

    // 1) BOM in the first two bytes (0xFF 0xFE = UTF-16 LE)
    assert_eq!(&bytes[0..2], &[0xFF, 0xFE], "missing UTF-16 LE BOM");

    // 2) Total byte length = 2 (BOM) + 2 * codepoint count (= xml is ASCII so
    //    `encode_utf16().count()` equals `chars().count()`)
    let codepoints = xml.encode_utf16().count();
    assert_eq!(
        bytes.len(),
        2 + codepoints * 2,
        "UTF-16 LE encoding length mismatch"
    );

    // 3) Round-trip: drop the BOM, decode as UTF-16 LE, must match `xml`.
    let utf16_units: Vec<u16> = bytes[2..]
        .chunks_exact(2)
        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
        .collect();
    let decoded = String::from_utf16(&utf16_units).expect("decoded UTF-16 must be valid");
    assert_eq!(decoded, xml, "UTF-16 LE round-trip mismatch");
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
