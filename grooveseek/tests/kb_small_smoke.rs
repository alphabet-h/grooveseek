//! Smoke test for `tests/fixtures/kb-small/` (feature-34 / F-56). Validates
//! that the shared 6-doc fixture indexes correctly and that an end-to-end
//! Japanese-CJK query reaches the right document via the MCP HTTP transport.
//!
//! Both tests are `#[ignore]` because they spawn `groove index` /
//! `groove serve`, which load the BGE-small embedding model (~130 MB) and
//! bind a free TCP port. Same policy as
//! `tests/search_mmr_integration.rs` and `tests/parser_level_smoke.rs`:
//! run on demand with `cargo test --test kb_small_smoke -- --ignored`.

use std::process::Command;

mod common;
use common::mcp::{build_index, grooveseek_bin, mcp_initialize, mcp_search_call, spawn_mcp_server};
use common::temp::TempKbLayout;

/// 6 fixture files in `tests/fixtures/kb-small/`. Listed explicitly so a
/// drift between fixture directory and the test (e.g. someone added a
/// 7th doc but forgot the assertion below) shows up as a build / count
/// mismatch.
const KB_SMALL_FILES: &[&str] = &[
    "intro.md",
    "architecture.md",
    "getting-started.ja.md",
    "troubleshooting.ja.md",
    "notes.md",
    "legacy.md",
];

/// Copy the 6 read-only fixture files from `tests/fixtures/kb-small/` into
/// `layout.kb()`. `TempKbLayout::new` already created `kb()` via
/// `create_dir_all`, so no extra `mkdir` is needed.
///
/// Includes a drift guard (codex review P3 follow-up): the directory
/// contents must exactly match `KB_SMALL_FILES`. If a 7th file is added
/// to the fixture without updating `KB_SMALL_FILES` (and hence the
/// `Documents: <N>` assertion in the smoke test), this assertion fires
/// before indexing — preventing the fixture corpus from evolving while
/// the test silently keeps passing on stale expectations.
fn setup_kb_small(layout: &TempKbLayout) {
    let src_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("kb-small");

    // Drift guard: actual fixture directory entries vs the hard-coded list.
    let mut actual: Vec<String> = std::fs::read_dir(&src_root)
        .unwrap_or_else(|e| panic!("read fixture dir {}: {e}", src_root.display()))
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    actual.sort();
    let mut expected: Vec<String> = KB_SMALL_FILES.iter().map(|s| (*s).to_string()).collect();
    expected.sort();
    assert_eq!(
        actual,
        expected,
        "kb-small fixture directory contents drifted from KB_SMALL_FILES; \
         either add new files to KB_SMALL_FILES (and update the `Documents: <N>` \
         assertion in test_kb_small_indexes_six_documents) or remove the \
         unexpected file from {}",
        src_root.display()
    );

    for name in KB_SMALL_FILES {
        let src = src_root.join(name);
        let dst = layout.kb().join(name);
        std::fs::copy(&src, &dst).unwrap_or_else(|e| {
            panic!(
                "kb-small fixture copy failed: {} -> {}: {e}",
                src.display(),
                dst.display()
            )
        });
    }
}

#[test]
#[ignore = "spawns groove index which loads the embedding model"]
fn test_kb_small_indexes_six_documents() {
    let layout = TempKbLayout::new("groove-kb-small-index");
    setup_kb_small(&layout);

    build_index(layout.kb());

    // `groove status` prints a human-readable block starting with `Documents: <N>`
    // to **stdout**: the counts are what the command was asked for, and ADR-0010
    // settles that a result belongs on stdout even when it is meant for a human.
    // `Commands::Index` still reports its progress on stderr, and so does the
    // "No index found" branch of `Commands::Status` -- neither is an answer.
    let bin = grooveseek_bin();
    let out = Command::new(&bin)
        .args(["status", "--kb-path", &layout.kb().display().to_string()])
        .output()
        .expect("groove status");
    assert!(out.status.success(), "groove status failed: {:?}", out);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("Documents: 6"),
        "expected `Documents: 6` in status stdout, got:\n{stdout}"
    );
}

#[test]
#[ignore = "spawns groove serve which loads embedding model and binds a port"]
fn test_kb_small_search_finds_japanese_query() {
    let layout = TempKbLayout::new("groove-kb-small-search");
    setup_kb_small(&layout);
    build_index(layout.kb());

    // Empty config: lets groove use defaults (BGE-small, RRF on, etc.).
    let config_path = layout.root().join("groove.toml");
    std::fs::write(&config_path, "").expect("write empty groove.toml");

    let (_guard, base) = spawn_mcp_server(layout.kb(), &config_path);
    let session_id = mcp_initialize(&base);
    let resp = mcp_search_call(
        &base,
        &session_id,
        serde_json::json!({
            "query": "トラブルシューティング",
            "limit": 5,
        }),
    );

    let results = resp["results"]
        .as_array()
        .unwrap_or_else(|| panic!("expected `results` array in search response, got: {resp}"));
    let troubleshooting_hits = results
        .iter()
        .filter(|hit| {
            hit["path"]
                .as_str()
                .map(|p| p.ends_with("troubleshooting.ja.md"))
                .unwrap_or(false)
        })
        .count();
    assert!(
        troubleshooting_hits >= 1,
        "expected at least 1 hit from troubleshooting.ja.md, got 0; results: {results:?}"
    );
}
