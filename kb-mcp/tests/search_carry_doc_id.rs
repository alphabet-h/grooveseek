//! F-41 PR-2: integration test for SearchResult.document_id carry.
//!
//! Indexes a 1-doc / 2-chunk KB, runs search_hybrid_candidates, and
//! asserts every returned SearchResult has document_id matching the
//! upserted documents.id (= no `unwrap_or(0)` fallback path remains).

mod common;

use common::temp::TempKbLayout;
use kb_mcp::db::{Database, SearchFilters};

#[test]
fn search_result_carries_document_id() {
    let layout = TempKbLayout::new("kbmcp-test-search-carry-doc-id");
    let db_path = layout.root().join(".kb-mcp.db");
    let db_path_str = db_path.to_str().expect("db path utf-8");
    let db = Database::open(db_path_str).expect("open db");
    // dim が決定するまで vec_chunks が遅延生成 = bench/test では明示初期化が要
    db.verify_embedding_meta("bge-small-en-v1.5", 384)
        .expect("verify_embedding_meta");

    let doc_id = db
        .upsert_document(
            "test/path.md",
            Some("test path"),
            Some("rust"),
            None,
            None,
            &[],
            None,
            "hash123",
        )
        .expect("upsert_document");

    db.insert_chunk(
        doc_id,
        0,
        Some("Heading 1"),
        Some(1),
        "rust async runtime",
        &[0.1_f32; 384],
        1.0,
    )
    .expect("insert_chunk 0");
    db.insert_chunk(
        doc_id,
        1,
        Some("Heading 2"),
        Some(1),
        "tokio runtime details",
        &[0.2_f32; 384],
        1.0,
    )
    .expect("insert_chunk 1");

    let hits = db
        .search_hybrid_candidates("rust", &[0.1_f32; 384], 10, &SearchFilters::default())
        .expect("search_hybrid_candidates");

    assert!(!hits.is_empty(), "search must return at least one hit");
    for (_chunk_id, sr) in hits {
        assert_eq!(
            sr.document_id, doc_id,
            "SearchResult.document_id must match upserted doc"
        );
    }
}
