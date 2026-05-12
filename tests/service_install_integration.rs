// tests/service_install_integration.rs
// Integration tests for `kb-mcp service install/uninstall/status/list`.
// Unit-level tests (= no OS service register) run on every `cargo test`.
// Dangerous tests (= OS service register, marked #[ignore]) run only on
// `cargo test -- --ignored`.

mod common;
use common::temp::TempRoot;

#[test]
fn install_resolves_kb_path_from_flag() {
    let tmp = TempRoot::new("install_flag");
    let kb = tmp.path().join("my-kb");
    std::fs::create_dir_all(&kb).unwrap();
    let result = kb_mcp::service::install::resolve_kb_path(
        Some(kb.clone()),
        None,
    ).unwrap();
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
    std::fs::write(&toml_path, format!("[index]\nkb_path = '{}'\n", kb.display())).unwrap();
    let result = kb_mcp::service::install::resolve_kb_path(
        None,
        Some(toml_path),
    ).unwrap();
    assert_eq!(result, kb);
}

#[test]
fn install_resolve_kb_path_errors_when_neither_provided() {
    assert!(kb_mcp::service::install::resolve_kb_path(None, None).is_err());
}
