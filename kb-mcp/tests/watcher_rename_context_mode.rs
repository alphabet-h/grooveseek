//! codex P2 round 2 (finding A) regression: watcher-driven single-file
//! rename (`indexer::rename_single_file`) must not take the same-hash
//! fast path in Static context mode, for the same reason as the F3 fix to
//! the batch `rebuild_index` rename path (PR #73 round 1, see
//! `tests/contextual_retrieval_cli.rs`): a frontmatter-title-less
//! document's `context_text` breadcrumb falls back to the filename stem
//! (E-1), so skipping re-parse on rename leaves it stale forever.
//!
//! This calls `indexer::rename_single_file` directly -- the exact function
//! `src/watcher.rs`'s `dispatch_rename` invokes on a live Rename event --
//! rather than driving a real filesystem watcher. That keeps the test
//! deterministic (no debounce timing / notify backend flakiness, see
//! `tests/watcher_e2e.rs`'s own caveats about Windows/macOS) while still
//! exercising the fix at the exact layer where the bug lived.
//!
//! `#[ignore]` because it loads the BGE-small embedding model (~130 MB on
//! a cold cache) -- same policy as `tests/contextual_retrieval_cli.rs`.
//! Run with:
//! ```text
//! cargo test --test watcher_rename_context_mode -- --ignored
//! ```

mod common;
use common::temp::TempKbLayout;

use kb_mcp::db::{ContextMode, Database};
use kb_mcp::embedder::{Embedder, ModelChoice};
use kb_mcp::indexer::progress::{ProgressMode, ProgressReporter};
use kb_mcp::indexer::{self, RenameOutcome};
use kb_mcp::parser::Registry;
use std::path::{Path, PathBuf};

/// No frontmatter at all -- context-title falls back to the filename stem
/// (`parser::txt::derive_title_pub`, E-1). Mirrors
/// `tests/contextual_retrieval_cli.rs::NO_TITLE_MD`.
const NO_TITLE_MD: &str = concat!(
    "## Section\n",
    "\n",
    "Body content that is long enough to pass the quality filter comfortably, ",
    "mentioning watcher rename regression testing details for the finding A fix.\n",
);

/// Read `(chunks.id, chunks.context_text)` for the sole chunk of `path`.
/// Same shape as `tests/contextual_retrieval_cli.rs::chunk_row_for_path`.
fn chunk_row_for_path(db_path: &Path, path: &str) -> (i64, Option<String>) {
    let conn = rusqlite::Connection::open(db_path).expect("open db for inspection");
    conn.query_row(
        "SELECT c.id, c.context_text FROM chunks c \
         JOIN documents d ON d.id = c.document_id WHERE d.path = ?1",
        [path],
        |row| Ok((row.get::<_, i64>(0)?, row.get::<_, Option<String>>(1)?)),
    )
    .expect("chunk row must exist")
}

/// Build the initial index in-process (no subprocess) so the same
/// `Embedder` instance (and its loaded model) can be reused for the
/// subsequent `rename_single_file` call below.
fn build_initial_index(
    layout: &TempKbLayout,
    context_mode: ContextMode,
) -> (Database, Embedder, Registry, PathBuf) {
    let db_path = layout.root().join(".kb-mcp.db");
    let db = Database::open(db_path.to_str().expect("db path utf-8")).expect("open db");
    // Mirrors main.rs's `Commands::Index` non-force path: verify_embedding_meta
    // must run before any embedding/insert_chunk call so `vec_chunks` exists.
    db.verify_embedding_meta(
        ModelChoice::BgeSmallEnV15.model_id(),
        ModelChoice::BgeSmallEnV15.dimension() as u32,
    )
    .expect("verify_embedding_meta");
    let mut embedder = Embedder::with_model(ModelChoice::BgeSmallEnV15).expect("load embedder");
    let registry = Registry::defaults();
    indexer::rebuild_index(
        &db,
        &mut embedder,
        layout.kb(),
        false,
        None,
        &[],
        &registry,
        ProgressReporter::new(ProgressMode::Quiet),
        context_mode,
    )
    .expect("initial rebuild_index");
    (db, embedder, registry, db_path)
}

#[test]
#[ignore = "requires embedding model download"]
fn test_static_mode_watcher_rename_reembeds_and_updates_context() {
    let layout = TempKbLayout::new("kb-mcp-watcher-rename-static");
    layout.write("old-widget-doc.md", NO_TITLE_MD);
    let (db, mut embedder, registry, db_path) = build_initial_index(&layout, ContextMode::Static);

    let (id_before, ctx_before) = chunk_row_for_path(&db_path, "old-widget-doc.md");
    assert!(
        ctx_before
            .as_deref()
            .is_some_and(|c| c.contains("old widget doc")),
        "expected context_text to carry the filename-derived title, got: {ctx_before:?}"
    );

    // Rename on disk first (content unchanged -> disk hash unchanged), then
    // call rename_single_file exactly as watcher.rs::dispatch_rename does.
    std::fs::rename(
        layout.kb().join("old-widget-doc.md"),
        layout.kb().join("new-gadget-doc.md"),
    )
    .expect("rename on disk");

    let outcome = indexer::rename_single_file(
        &db,
        &mut embedder,
        layout.kb(),
        "old-widget-doc.md",
        "new-gadget-doc.md",
        None,
        &registry,
    )
    .expect("rename_single_file");
    assert!(
        matches!(outcome, RenameOutcome::RenamedAndReindexed { .. }),
        "Static-mode watcher rename must re-embed (RenamedAndReindexed), got: {outcome:?}"
    );

    let (id_after, ctx_after) = chunk_row_for_path(&db_path, "new-gadget-doc.md");
    assert_ne!(
        id_before, id_after,
        "Static-mode watcher rename must re-embed (new chunks.id via full reparse), \
         but rowid stayed {id_before} -- the same-hash fast path was not disabled"
    );
    assert!(
        ctx_after
            .as_deref()
            .is_some_and(|c| c.contains("new gadget doc")),
        "expected context_text to carry the new filename-derived title, got: {ctx_after:?}"
    );
    assert!(
        ctx_after
            .as_deref()
            .is_some_and(|c| !c.contains("old widget doc")),
        "expected the stale filename-derived title to be gone, got: {ctx_after:?}"
    );
}

/// Off-mode control: the same watcher-rename scenario must still take the
/// same-hash fast path (no re-embed) -- Off mode never embeds context, so a
/// filename-derived title change is harmless and the perf optimization must
/// be preserved (finding A's fix must be Static-only, matching F3).
#[test]
#[ignore = "requires embedding model download"]
fn test_off_mode_watcher_rename_keeps_fast_path() {
    let layout = TempKbLayout::new("kb-mcp-watcher-rename-off");
    layout.write("old-widget-doc.md", NO_TITLE_MD);
    let (db, mut embedder, registry, db_path) = build_initial_index(&layout, ContextMode::Off);

    let (id_before, ctx_before) = chunk_row_for_path(&db_path, "old-widget-doc.md");
    assert!(
        ctx_before.is_none(),
        "Off mode never stores context_text, got: {ctx_before:?}"
    );

    std::fs::rename(
        layout.kb().join("old-widget-doc.md"),
        layout.kb().join("new-gadget-doc.md"),
    )
    .expect("rename on disk");

    let outcome = indexer::rename_single_file(
        &db,
        &mut embedder,
        layout.kb(),
        "old-widget-doc.md",
        "new-gadget-doc.md",
        None,
        &registry,
    )
    .expect("rename_single_file");
    assert_eq!(
        outcome,
        RenameOutcome::Renamed,
        "Off-mode watcher rename must keep the same-hash fast path (no reindex), got: {outcome:?}"
    );

    let (id_after, ctx_after) = chunk_row_for_path(&db_path, "new-gadget-doc.md");
    assert_eq!(
        id_before, id_after,
        "Off-mode rename must keep the same-hash fast path (chunks.id stable)"
    );
    assert!(
        ctx_after.is_none(),
        "Off mode never stores context_text, got: {ctx_after:?}"
    );
}
