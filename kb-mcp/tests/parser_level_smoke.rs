//! Smoke test for feature-28 PR-1: end-to-end verification that the chunker's
//! h2/h3 distinction (`Chunk.level`) is propagated through `insert_chunk` and
//! persisted in the `chunks.level` column.
//!
//! Spawns `kb-mcp index` against a tiny KB containing one h2 + one h3 heading
//! and asserts that the `chunks` table contains both `level = 2` and
//! `level = 3` rows. Pure schema-and-pipeline check; retrieval is not exercised.
//!
//! `#[ignore]` because `kb-mcp index` initialises the embedding model
//! (BGE-small-en-v1.5 by default, ~130 MB on first run). Same policy as
//! `tests/eval_cli.rs` and `tests/search_cli.rs` — runs under
//! `cargo test --test parser_level_smoke -- --ignored` and in `nightly.yml`
//! with `--include-ignored`.
//!
//! On a warm fastembed cache hit this test still pays the model-load cost
//! (~3-5 s on dev hardware), which is enough to push it out of the default
//! `cargo test` budget.
//!
//! cargo's integration-test harness compiles each `tests/<name>.rs` as a
//! separate crate, so `mod common;` must be declared even if the helper
//! tree is shared.

use std::process::Command;

mod common;
use common::temp::TempKbLayout;

#[test]
#[ignore = "spawns `kb-mcp index` which loads the embedding model"]
fn index_persists_chunk_level_for_h2_h3() {
    let layout = TempKbLayout::new("kb-mcp-parser-level-smoke");
    // Each section's body exceeds the chunker's 50-char merge threshold so the
    // h2 and h3 chunks survive as separate rows (otherwise they would be
    // merged and the merged chunk would inherit the h2 level only).
    layout.write(
        "doc.md",
        "---\ntitle: t\ntopic: x\ntags: [a]\n---\n\n## H2 Heading\n\nThis h2 body is comfortably longer than the fifty-character merge threshold to keep the chunk standalone.\n\n### H3 Sub\n\nThis h3 body is also comfortably longer than the fifty-character merge threshold to keep the chunk standalone.\n",
    );

    let exe = env!("CARGO_BIN_EXE_kb-mcp");
    let status = Command::new(exe)
        .args(["index", "--kb-path", &layout.kb().display().to_string()])
        .status()
        .expect("spawn kb-mcp index");
    assert!(status.success(), "kb-mcp index failed: {status:?}");

    // The DB lands at `kb_path.parent()/.kb-mcp.db` (= layout.root()/.kb-mcp.db).
    let db_path = layout.root().join(".kb-mcp.db");
    assert!(
        db_path.exists(),
        "expected SQLite DB at {}",
        db_path.display()
    );
    let conn = rusqlite::Connection::open(&db_path).expect("open db");
    let levels: Vec<Option<i64>> = conn
        .prepare("SELECT level FROM chunks ORDER BY chunk_index ASC")
        .expect("prepare select")
        .query_map([], |r| r.get::<_, Option<i64>>(0))
        .expect("query_map")
        .filter_map(Result::ok)
        .collect();

    assert!(
        levels.contains(&Some(2)),
        "expected an h2 chunk with level=2, got: {levels:?}"
    );
    assert!(
        levels.contains(&Some(3)),
        "expected an h3 chunk with level=3, got: {levels:?}"
    );
}
