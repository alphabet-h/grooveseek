//! #660 (spec 5.4, T-M4): MCP `rebuild_index` names each file it skipped,
//! with its reason, over a real `groove serve`.
//!
//! The server embeds through the in-process mock
//! ([`crate::common::embed_mock`]) under the shared fixture
//! ([`crate::common::embed_cli`]), so nothing is downloaded.

mod common;

use common::embed_cli::{fixture, note};
use common::embed_mock::hermetic;
use common::mcp::{mcp_initialize, mcp_tool_call, spawn_serve_with};

use std::path::Path;

/// An empty note in a subdirectory with a Japanese name comes back in
/// `skipped_files` under its knowledge-base path: relative, `/`-separated,
/// UTF-8 as written, never the server's absolute path.
///
/// The knowledge base is indexed once before the server starts, as every
/// other `spawn_serve_with` test does; the empty note is skipped again on
/// the second run, so it is listed then too.
///
/// Red if the reply drops `skipped_files`, lists the absolute path or a
/// `\`-separated one, or names the reason other than `no_text`.
#[test]
fn rebuild_index_reply_lists_an_empty_note_by_its_kb_path() {
    let ok = note("Ok", "A note with a body, so the run embeds something.");
    let fx = fixture(
        "groove-660-mcp",
        &[("ok.md", ok.as_str()), ("資料/議事録.md", "")],
        "",
    );
    fx.index();
    let (guard, base) = spawn_serve_with(fx.kb(), &fx.config, false, |c| {
        hermetic(c, &fx.cache);
    });
    let session = mcp_initialize(&base);
    let resp = mcp_tool_call(&base, &session, "rebuild_index", serde_json::json!({}));
    drop(guard);

    assert_eq!(
        resp.get("skipped_files"),
        Some(&serde_json::json!([{"path": "資料/議事録.md", "reason": "no_text"}])),
        "{resp}"
    );
    assert_eq!(
        resp.get("skipped_files_omitted").and_then(|v| v.as_u64()),
        Some(0),
        "{resp}"
    );
    assert_eq!(
        resp.get("skipped").and_then(|v| v.as_u64()),
        Some(1),
        "{resp}"
    );
    let listed = resp["skipped_files"].as_array().expect("an array");
    for entry in listed {
        let p = entry["path"].as_str().expect("path is a string");
        assert!(!Path::new(p).is_absolute(), "absolute path in reply: {p}");
        assert!(!p.contains('\\'), "backslash in reply path: {p}");
    }
}
