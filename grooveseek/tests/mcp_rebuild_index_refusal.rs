//! (kuriya 627, AX-42 item 10a) The MCP tool that rebuilds the index refuses, over a real
//! `groove serve`, while another groove process holds the writer lease (ADR-0031), and the
//! refusal leaves the index as it was.
//!
//! The wording of each refusal is pinned in-process by the unit tests of
//! `grooveseek::server::writer_slot` and `grooveseek::server`, against a slot built in the test.
//! These tests pin what those cannot: that the served tool reaches the refusal through the real
//! lease file another process holds, that a refused call sends nothing to the embedding endpoint
//! and writes no row, and which of the two refusals is per call and which lasts for the life of
//! the server.
//!
//! The lease is taken by this test process ([`WriterLease::try_acquire`]), which is another
//! process from the server's point of view, as a `groove index` run would hold it. The server
//! embeds through the mock of [`crate::common::embed_cli`], so nothing is downloaded.

mod common;

use common::embed_cli::{Fixture, fixture, note};
use common::embed_mock::{DOC_MODEL, assert_dir_empty, hermetic};
use common::mcp::{mcp_initialize, mcp_tool_call, spawn_reader_serve_with, spawn_serve_with};
use grooveseek::db::Database;
use grooveseek::writer_lease::{Role, WriterLease};

const ALPHA: &str = "The ferry timetable changes on the first Monday of each month.";
const ALPHA_EDITED: &str = "The ferry timetable changes on the last Friday of each month.";
const BRAVO: &str = "Keep the spare keys in the grey cabinet beside the loading dock door.";

/// A knowledge base of two notes, indexed once, whose `alpha.md` is then edited on disk so a
/// rebuild that ran would change its row.
fn indexed_then_edited(prefix: &str) -> Fixture {
    let fx = fixture(
        prefix,
        &[
            ("alpha.md", &note("Alpha", ALPHA)),
            ("bravo.md", &note("Bravo", BRAVO)),
        ],
        "",
    );
    fx.index();
    fx.layout.write("alpha.md", &note("Alpha", ALPHA_EDITED));
    fx
}

/// The writer lease on `fx`'s index, held as a one-off `groove index` run would hold it.
fn hold_lease(fx: &Fixture) -> WriterLease {
    WriterLease::try_acquire(&grooveseek::resolve_db_path(fx.kb()), Role::Index, fx.kb())
        .expect("take the writer lease in the test")
}

/// Every `(path, chunk texts joined)` in the index, read through a reader connection
/// (`query_only`), so reading cannot be what changes the rows.
fn rows(fx: &Fixture) -> Vec<(String, String)> {
    let db_path = grooveseek::resolve_db_path(fx.kb());
    let db = Database::open_read_only(&db_path.to_string_lossy(), fx.kb())
        .unwrap_or_else(|e| panic!("open the index as a reader: {e:#}"));
    db.all_document_paths()
        .expect("paths")
        .into_iter()
        .map(|p| {
            let text = db
                .chunk_texts_for_path(&p)
                .expect("chunk texts")
                .into_iter()
                .map(|(_, content)| content)
                .collect::<Vec<_>>()
                .join("\n");
            (p, text)
        })
        .collect()
}

/// The number of document-embedding requests the mock received since request `before`.
fn doc_requests_since(fx: &Fixture, before: usize) -> usize {
    fx.requests_since(before)
        .iter()
        .filter(|r| r.model() == Some(DOC_MODEL))
        .count()
}

/// The `error` of an MCP reply, which must have one.
fn error_of(resp: &serde_json::Value) -> &str {
    resp.get("error")
        .and_then(|v| v.as_str())
        .unwrap_or_else(|| panic!("the reply carries no error: {resp}"))
}

/// A server started with `--no-watch` takes no lease at start, so it claims the lease per call:
/// a call made while another process holds it is refused naming that process, plain or
/// forced, before anything reaches the endpoint or the index; the same server rebuilds once
/// that process has let go.
///
/// Red if the served tool skips the claim (the rows change, or the endpoint is asked), if a
/// forced call reaches the reset before the claim (the index is emptied), or if a refusal
/// sticks to a server that never became a reader.
#[test]
fn a_no_watch_server_refuses_rebuild_while_another_process_holds_the_lease() {
    let fx = indexed_then_edited("ax42-refuse-busy");
    let before_rows = rows(&fx);
    assert!(
        before_rows
            .iter()
            .any(|(p, t)| p == "alpha.md" && t.contains("first Monday")),
        "premise: the index holds the text before the edit: {before_rows:?}"
    );
    let (guard, base) = spawn_serve_with(fx.kb(), &fx.config, false, |c| {
        hermetic(c, &fx.cache);
    });
    let session = mcp_initialize(&base);

    let lease = hold_lease(&fx);
    let before = fx.mock.requests().len();
    let plain = mcp_tool_call(&base, &session, "rebuild_index", serde_json::json!({}));
    let forced = mcp_tool_call(
        &base,
        &session,
        "rebuild_index",
        serde_json::json!({"force": true}),
    );
    let sent = doc_requests_since(&fx, before);
    let during = rows(&fx);
    drop(lease);

    let head = format!(
        "Another groove process is writing this index (pid {}, ",
        std::process::id()
    );
    for resp in [&plain, &forced] {
        let error = error_of(resp);
        assert!(error.starts_with(&head), "{resp}");
        assert!(error.ends_with("groove index with --wait can."), "{resp}");
        assert!(
            resp.get("updated").is_none(),
            "a refusal has no counts: {resp}"
        );
    }
    assert_eq!(sent, 0, "a refused call asked the endpoint");
    assert_eq!(during, before_rows, "a refused call changed the index");

    let after = mcp_tool_call(&base, &session, "rebuild_index", serde_json::json!({}));
    drop(guard);
    assert!(after.get("error").is_none(), "{after}");
    assert_eq!(
        after.get("updated").and_then(|v| v.as_u64()),
        Some(1),
        "{after}"
    );
    let after_rows = rows(&fx);
    assert!(
        after_rows.iter().any(|(p, t)| p == "alpha.md"
            && t.contains("last Friday")
            && !t.contains("first Monday")),
        "{after_rows:?}"
    );
    assert_dir_empty(&fx.cache);
}

/// A server started with the watch on while another process holds the lease starts as a
/// reader, and its rebuild is refused for the life of the server: while that process holds the
/// lease and after it has let go, with the reason in the error and the reader's notice in
/// `writer_notice`. Nothing reaches the endpoint or the index.
///
/// Red if a reader's call claims the lease (it would rebuild once the holder is gone), or if
/// the served reply drops the notice that says why.
#[test]
fn a_server_started_while_another_process_holds_the_lease_never_rebuilds() {
    let fx = indexed_then_edited("ax42-refuse-reader");
    let before_rows = rows(&fx);
    let lease = hold_lease(&fx);
    let (guard, base) = spawn_reader_serve_with(fx.kb(), &fx.config, |c| {
        hermetic(c, &fx.cache);
    });
    assert!(
        guard
            .stderr()
            .contains("answers searches but does not update the index"),
        "premise: the server started as a reader; stderr:\n{}",
        guard.stderr().lines().join("\n")
    );
    let session = mcp_initialize(&base);

    let before = fx.mock.requests().len();
    let held = mcp_tool_call(&base, &session, "rebuild_index", serde_json::json!({}));
    drop(lease);
    let released = mcp_tool_call(&base, &session, "rebuild_index", serde_json::json!({}));
    let forced = mcp_tool_call(
        &base,
        &session,
        "rebuild_index",
        serde_json::json!({"force": true}),
    );
    let sent = doc_requests_since(&fx, before);
    drop(guard);

    for resp in [&held, &released, &forced] {
        assert_eq!(
            error_of(resp),
            "This server cannot rebuild the index: it started while another groove process was \
             writing it, so it only answers searches until it is restarted (see writer_notice).",
            "{resp}"
        );
        let notice = resp
            .get("writer_notice")
            .and_then(|v| v.as_str())
            .unwrap_or_else(|| panic!("no writer_notice: {resp}"));
        assert!(
            notice.contains("this server is not updating the index at"),
            "{resp}"
        );
    }
    assert_eq!(sent, 0, "a refused call asked the endpoint");
    assert_eq!(rows(&fx), before_rows, "a refused call changed the index");
    assert_dir_empty(&fx.cache);
}
