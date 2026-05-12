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

/// v0.8.1 hot-fix smoke test: actually invokes `schtasks /Create /XML` with
/// the produced UTF-16 LE BOM bytes against the live Task Scheduler service,
/// then deletes the resulting throwaway task.
///
/// **Requires an elevated shell.** `schtasks /Create` at the root task path
/// (`\`) returns "Access is denied" from non-elevated contexts (cargo test
/// runs un-elevated by default). The byte-level test
/// `windows_task_xml_is_utf16_le_with_bom` is the primary CI regression
/// guard; this smoke test is for manual end-to-end verification from an
/// elevated PowerShell:
///
/// ```text
/// cargo test --test service_install_integration windows_task_xml_smoke_test_schtasks_create -- --ignored
/// ```
#[cfg(target_os = "windows")]
#[test]
#[ignore = "mutates Windows Task Scheduler and requires elevated shell — run manually"]
fn windows_task_xml_smoke_test_schtasks_create() {
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
    let tmp = std::env::temp_dir().join(format!("kbmcp-utf16-smoke-{}.xml", unique));
    std::fs::write(&tmp, bytes).unwrap();

    let task_name = format!("kb-mcp-{}", svc_name);
    let create = Command::new("schtasks")
        .args(["/Create", "/TN", &task_name, "/XML"])
        .arg(&tmp)
        .output()
        .expect("schtasks /Create invocation failed");

    // Best-effort cleanup before assert so a failed test still leaves no junk.
    let _ = Command::new("schtasks")
        .args(["/Delete", "/TN", &task_name, "/F"])
        .output();
    let _ = std::fs::remove_file(&tmp);

    assert!(
        create.status.success(),
        "schtasks /Create /XML failed (status: {:?})\nstderr: {}\nstdout: {}",
        create.status.code(),
        String::from_utf8_lossy(&create.stderr),
        String::from_utf8_lossy(&create.stdout),
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
