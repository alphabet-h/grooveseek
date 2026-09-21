//! The path `search` hands back is the path `get_document` opens.
//!
//! `get_document` opens a document only under the exact string the index
//! stores for it, so the two sides have to spell a path the same way. They
//! share one function for it now (`grooveseek::indexer::index_rel_path`); the
//! tests here are the ones that go through the real index, the real `search`
//! and the real `get_document` rather than through that function, so they
//! would still fail if one side stopped calling it.
//!
//! The lightweight half -- the spelling the index walk stores, fed to the
//! `get_document` validator, with no embedding model -- lives next to the
//! validator's tests in `src/server.rs` and runs on every pull request. These
//! build an index, so they are `#[ignore]`.

mod common;
use common::mcp::{build_index, mcp_initialize, mcp_tool_call, spawn_mcp_server};
use common::temp::TempKbLayout;

fn body(marker: &str) -> String {
    format!(
        "---\ntitle: Round trip fixture\n---\n\n## body\n\nContent carrying {marker}, long \
         enough to be a real chunk rather than something the quality filter hides.\n"
    )
}

/// Index `layout`, serve it, search for `marker`, and return the first hit's
/// `path` together with what `get_document` answers for that exact string.
fn search_then_open(layout: &TempKbLayout, marker: &str) -> (String, serde_json::Value) {
    build_index(layout.kb());
    let cfg = layout.root().join("groove.toml");
    std::fs::write(&cfg, "[watch]\nenabled = false\n").expect("write groove.toml");
    let (_guard, base) = spawn_mcp_server(layout.kb(), &cfg);
    let session = mcp_initialize(&base);

    let hits = mcp_tool_call(
        &base,
        &session,
        "search",
        serde_json::json!({"query": marker, "limit": 3, "mmr": false}),
    );
    let path = hits["results"][0]["path"]
        .as_str()
        .unwrap_or_else(|| panic!("search returned no hit with a path: {hits}"))
        .to_string();
    let opened = mcp_tool_call(
        &base,
        &session,
        "get_document",
        serde_json::json!({"path": path}),
    );
    (path, opened)
}

/// Every platform: a nested document. On Windows this is the fold that does
/// happen -- the walk sees `docs\deep\a.md` and the index stores `/`.
#[test]
#[ignore = "builds an index; downloads BGE-small on first run"]
fn a_nested_document_opens_under_the_path_search_returned() {
    let layout = TempKbLayout::new("groove-path-roundtrip");
    layout.write("docs/deep/a.md", &body("zqxwnested"));

    let (path, opened) = search_then_open(&layout, "zqxwnested");
    assert_eq!(path, "docs/deep/a.md");
    assert!(
        opened["content"]
            .as_str()
            .unwrap_or_default()
            .contains("zqxwnested"),
        "get_document must open the path search returned ({path:?}): {opened}"
    );
}

/// Unix only, because only there can a filename contain `\`. The index used to
/// fold it unconditionally and store `secret/pay.md`, a string `get_document`
/// could not open, while the real name opened and matched no rule written
/// against the index (codex P2 round 4 on PR #310).
#[cfg(unix)]
#[test]
#[ignore = "builds an index; downloads BGE-small on first run"]
fn a_literal_backslash_name_opens_under_the_path_search_returned_on_unix() {
    let layout = TempKbLayout::new("groove-path-roundtrip-backslash");
    layout.write("secret\\pay.md", &body("zqxwbackslash"));

    let (path, opened) = search_then_open(&layout, "zqxwbackslash");
    assert_eq!(
        path, "secret\\pay.md",
        "the index must store the file under its own name"
    );
    assert!(
        opened["content"]
            .as_str()
            .unwrap_or_default()
            .contains("zqxwbackslash"),
        "get_document must open the path search returned ({path:?}): {opened}"
    );
}
