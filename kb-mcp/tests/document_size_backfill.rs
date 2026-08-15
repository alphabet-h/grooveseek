//! feature-51: `documents.size_bytes` survives the incremental fast path.
//!
//! The interesting case is not "a fresh index records sizes" — the write
//! sits right next to `content_hash`, so that could hardly fail. It is the
//! **upgrade**: a knowledge base indexed by an older kb-mcp has the column
//! added on open, with every row NULL, and the obvious next step (`kb-mcp
//! index`) walks a tree in which nothing changed. `rebuild_index` answers
//! `SingleResult::Unchanged` for each of those files and never reaches
//! `upsert_document` or `update_document_meta`, so **a migration that only
//! writes from those two paths never happens at all**. Only
//! `record_document_sizes`, called for every path the scan measured, closes
//! that, and this test is what says so.
//!
//! `#[ignore]` because `kb-mcp index` loads the BGE-small embedding model
//! (~130 MB on a cold cache) — same policy as `tests/binary_formats_cli.rs`.
//! Run with:
//! ```text
//! cargo test --test document_size_backfill -- --ignored
//! ```

use std::path::Path;
use std::process::Command;

mod common;
use common::mcp::kb_mcp_bin;
use common::temp::TempKbLayout;

const NOTE_A: &str = "---\ntitle: A\n---\n\n# A\n\nAlpha content for the size test.\n";
const NOTE_B: &str =
    "---\ntitle: B\n---\n\n# B\n\nBeta content, deliberately a different length.\n";

fn run_index(bin: &Path, kb: &Path) -> String {
    let out = Command::new(bin)
        .args(["index", "--kb-path", &kb.display().to_string()])
        .output()
        .expect("kb-mcp index");
    assert!(
        out.status.success(),
        "index failed:\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// Read `(path, size_bytes)` straight out of the file with rusqlite rather
/// than through `Database::open`, which would run the migration this test is
/// trying to observe.
fn sizes(db_path: &Path) -> Vec<(String, Option<i64>)> {
    let conn = rusqlite::Connection::open(db_path).expect("open db");
    let mut stmt = conn
        .prepare("SELECT path, size_bytes FROM documents ORDER BY path")
        .expect("prepare");
    stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .expect("query")
        .collect::<Result<Vec<_>, _>>()
        .expect("collect")
}

#[test]
#[ignore = "requires embedding model download"]
fn an_index_run_records_sizes_and_fills_them_in_for_documents_it_skips() {
    let layout = TempKbLayout::new("kb-mcp-size-backfill");
    layout.write("a.md", NOTE_A);
    layout.write("b.md", NOTE_B);
    let bin = kb_mcp_bin();
    let db_path = kb_mcp::resolve_db_path(layout.kb());

    run_index(&bin, layout.kb());

    let first = sizes(&db_path);
    assert_eq!(first.len(), 2, "both notes should be indexed: {first:?}");
    assert_eq!(
        first,
        vec![
            ("a.md".to_string(), Some(NOTE_A.len() as i64)),
            ("b.md".to_string(), Some(NOTE_B.len() as i64)),
        ],
        "a fresh index records the bytes it read"
    );

    // Now make it look like a database written before the column existed:
    // the rows are all there, none of them knows its size. This is exactly
    // the state `ensure_document_size_column` leaves an upgraded index in.
    {
        let conn = rusqlite::Connection::open(&db_path).expect("open db");
        conn.execute_batch("UPDATE documents SET size_bytes = NULL")
            .expect("blank the sizes");
    }
    assert!(
        sizes(&db_path).iter().all(|(_, s)| s.is_none()),
        "precondition: every row is unrecorded before the second run"
    );

    // The files on disk have not changed, so every one of them takes the
    // content-hash fast path. Without the backfill this run writes nothing
    // and the sizes stay NULL forever.
    run_index(&bin, layout.kb());

    assert_eq!(
        sizes(&db_path),
        first,
        "an index run over unchanged files must still record their sizes"
    );
}

/// codex P2 round 1: the backfill keys on the path the **scan** saw, which is
/// the new one, while the row still carries the old path until rename detection
/// applies. Run before that and a file renamed in the same run as the migration
/// matches nothing — and then takes the same-hash fast path, which writes no
/// document row either, so its size stays NULL until some later full index.
#[test]
#[ignore = "requires embedding model download"]
fn a_file_renamed_in_the_migrating_run_still_gets_its_size_recorded() {
    let layout = TempKbLayout::new("kb-mcp-size-rename");
    layout.write("a.md", NOTE_A);
    layout.write("b.md", NOTE_B);
    let bin = kb_mcp_bin();
    let db_path = kb_mcp::resolve_db_path(layout.kb());

    run_index(&bin, layout.kb());

    // Back to the pre-feature-51 state.
    {
        let conn = rusqlite::Connection::open(&db_path).expect("open db");
        conn.execute_batch("UPDATE documents SET size_bytes = NULL")
            .expect("blank the sizes");
    }

    // Move one of them. The content is untouched, so this is a rename: the row
    // is relabelled rather than re-embedded.
    std::fs::rename(layout.kb().join("a.md"), layout.kb().join("renamed.md")).expect("rename");

    run_index(&bin, layout.kb());

    assert_eq!(
        sizes(&db_path),
        vec![
            ("b.md".to_string(), Some(NOTE_B.len() as i64)),
            ("renamed.md".to_string(), Some(NOTE_A.len() as i64)),
        ],
        "the renamed document must be backfilled in the same run that renamed it"
    );
}
