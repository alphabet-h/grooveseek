//! feature-69: re-indexing a changed document sends only the chunks whose embedding input
//! changed to the endpoint, and keeps every other chunk's vector as the bytes the index already
//! holds.
//!
//! Each test drives `groove index` over a generated knowledge base against the mock endpoint of
//! [`crate::common::embed_cli`], so nothing downloads a model. The mock answers each input with
//! a vector derived from its text alone, which is what makes "the same bytes as before" a claim
//! about reuse only once the fixture's sensitivity has been checked (see
//! `an_unchanged_chunk_keeps_its_stored_bytes`).

mod common;

use common::embed_cli::{Fixture, REJECT_MARKER, fixture, note, rejects_marker, stderr_of};
use common::embed_mock::{DOC_MODEL, assert_dir_empty};
use grooveseek::db::{Database, StoredChunkVector};
use std::process::Output;

const S1: (&str, &str) = (
    "Install",
    "Download the archive and unpack it into the tools folder before anything else.",
);
const S2: (&str, &str) = (
    "Configure",
    "Point the config file at the shared cache directory on the build machine.",
);
const S3: (&str, &str) = (
    "Verify",
    "Run the smoke check against the staging endpoint and compare the counts.",
);
const S4: (&str, &str) = (
    "Rollback",
    "Restore the previous release from the archive when the smoke check fails.",
);
const INS: (&str, &str) = (
    "Monitor",
    "Watch the queue depth dashboard during the first hour after rollout.",
);
/// A body two sections of `duplicate_chunks_share_one_stored_vector_and_the_counts_add_up` share.
const DUP: &str = "Keep the spare keys in the grey cabinet beside the loading dock door.";

const RUNBOOK: &str = "runbook.md";

/// A Markdown document titled `Runbook` with one `##` section per `(heading, body)`.
fn runbook(sections: &[(&str, &str)]) -> String {
    let mut md = String::from("---\ntitle: Runbook\n---\n\n");
    for (heading, body) in sections {
        md.push_str(&format!("## {heading}\n\n{body}\n\n"));
    }
    md
}

/// [`S3`] with `staging` changed to `production`.
fn s3_changed() -> String {
    S3.1.replace("staging", "production")
}

/// The texts the mock was asked to embed as documents since request `before`.
fn embedded_since(fx: &Fixture, before: usize) -> Vec<String> {
    fx.requests_since(before)
        .iter()
        .filter(|r| r.model() == Some(DOC_MODEL))
        .flat_map(|r| r.inputs())
        .collect()
}

fn open_index(fx: &Fixture) -> Database {
    let path = grooveseek::resolve_db_path(fx.kb());
    Database::open(path.to_str().expect("a UTF-8 index path")).expect("open the index")
}

/// `rel`'s chunks as stored, in chunk order.
fn vectors(fx: &Fixture, rel: &str) -> Vec<StoredChunkVector> {
    open_index(fx)
        .stored_chunk_vectors_for_path(rel)
        .expect("read the stored vectors")
}

/// The stored vector of the chunk whose content is `content`.
fn blob_of<'a>(rows: &'a [StoredChunkVector], content: &str) -> &'a [u8] {
    &rows
        .iter()
        .find(|r| r.content == content)
        .unwrap_or_else(|| panic!("no stored chunk {content:?}"))
        .embedding
}

/// The contents of `rows`, in order.
fn contents(rows: &[StoredChunkVector]) -> Vec<&str> {
    rows.iter().map(|r| r.content.as_str()).collect()
}

/// Every chunk has a vector and an FTS row, and every vector and FTS row has a chunk.
fn assert_index_consistent(fx: &Fixture) {
    let db = open_index(fx);
    for (name, scan) in [
        ("chunks without embedding", db.chunks_without_embedding(5)),
        ("embeddings without chunk", db.embeddings_without_chunk(5)),
        ("chunks without fts", db.chunks_without_fts(5)),
        ("fts without chunk", db.fts_without_chunk(5)),
    ] {
        let scan = scan.unwrap_or_else(|e| panic!("{name}: {e}"));
        assert!(scan.is_clean(), "{name}: {scan:?}");
    }
}

/// The `Done in` line of `out`.
fn summary(out: &Output) -> String {
    stderr_of(out)
        .lines()
        .find(|l| l.contains("Done in"))
        .unwrap_or_else(|| panic!("no Done in line:\n{}", stderr_of(out)))
        .to_string()
}

/// `groove index`, which must succeed; its `Done in` line.
fn index_ok(fx: &Fixture) -> String {
    let out = fx.run_index();
    assert!(
        out.status.success(),
        "groove index failed:\n{}",
        stderr_of(&out)
    );
    summary(&out)
}

/// A knowledge base holding [`RUNBOOK`] with `sections`, indexed once.
fn indexed_runbook(prefix: &str, sections: &[(&str, &str)]) -> Fixture {
    let fx = fixture(prefix, &[(RUNBOOK, &runbook(sections))], "");
    index_ok(&fx);
    fx
}

#[test]
fn an_unchanged_chunk_keeps_its_stored_bytes() {
    let fx = indexed_runbook("f69-bytes", &[S1, S2, S3, S4]);
    let before = vectors(&fx, RUNBOOK);

    // The fixture's sensitivity: a vector that went through JSON at six decimals would not be
    // these bytes, so equal bytes below say the stored vector was kept.
    let s1 = blob_of(&before, S1.1);
    let rounded: Vec<u8> = s1
        .as_chunks::<4>()
        .0
        .iter()
        .map(|b| f32::from_le_bytes(*b))
        .map(|x| format!("{x:.6}").parse::<f32>().expect("a float"))
        .flat_map(f32::to_le_bytes)
        .collect();
    assert_ne!(
        rounded, s1,
        "rounding to six decimals must change the vector"
    );

    let s3 = s3_changed();
    fx.layout
        .write(RUNBOOK, &runbook(&[S1, S2, ("Verify", &s3), S4]));
    index_ok(&fx);

    let after = vectors(&fx, RUNBOOK);
    for body in [S1.1, S2.1, S4.1] {
        assert_eq!(blob_of(&after, body), blob_of(&before, body), "{body}");
    }
    assert_index_consistent(&fx);
    assert_dir_empty(&fx.cache);
}

#[test]
fn only_the_changed_chunk_is_sent_to_the_endpoint() {
    let fx = fixture("f69-only", &[(RUNBOOK, &runbook(&[S1, S2, S3, S4]))], "");
    let first = index_ok(&fx);
    assert!(
        first.contains(" 4 chunks (this run: 4 embedded, 0 reused)"),
        "{first}"
    );

    let s3 = s3_changed();
    fx.layout
        .write(RUNBOOK, &runbook(&[S1, S2, ("Verify", &s3), S4]));
    let before = fx.mock.requests().len();
    let line = index_ok(&fx);
    assert_eq!(embedded_since(&fx, before), vec![s3.clone()]);
    assert!(line.contains("(this run: 1 embedded, 3 reused)"), "{line}");

    let before = fx.mock.requests().len();
    let line = index_ok(&fx);
    assert!(!line.contains("this run:"), "{line}");
    assert!(embedded_since(&fx, before).is_empty());

    assert_index_consistent(&fx);
    assert_dir_empty(&fx.cache);
}

#[test]
fn an_inserted_section_leaves_the_sections_after_it_reused() {
    let fx = indexed_runbook("f69-insert", &[S1, S2, S3, S4]);

    fx.layout.write(RUNBOOK, &runbook(&[S1, INS, S2, S3, S4]));
    let before = fx.mock.requests().len();
    let line = index_ok(&fx);
    assert_eq!(embedded_since(&fx, before), vec![INS.1.to_string()]);
    assert!(line.contains("(this run: 1 embedded, 4 reused)"), "{line}");

    assert_index_consistent(&fx);
    assert_dir_empty(&fx.cache);
}

#[test]
fn duplicate_chunks_share_one_stored_vector_and_the_counts_add_up() {
    let sections = [S1, ("Spare keys A", DUP), ("Spare keys B", DUP), S3];
    let fx = indexed_runbook("f69-dup", &sections);
    let before_rows = vectors(&fx, RUNBOOK);
    let dup_before = blob_of(&before_rows, DUP).to_vec();

    let s1 = S1.1.replace("tools folder", "vendor folder");
    fx.layout.write(
        RUNBOOK,
        &runbook(&[("Install", &s1), sections[1], sections[2], sections[3]]),
    );
    let before = fx.mock.requests().len();
    let line = index_ok(&fx);
    assert_eq!(embedded_since(&fx, before), vec![s1]);
    assert!(line.contains("(this run: 1 embedded, 3 reused)"), "{line}");

    let after = vectors(&fx, RUNBOOK);
    let dups: Vec<&[u8]> = after
        .iter()
        .filter(|r| r.content == DUP)
        .map(|r| r.embedding.as_slice())
        .collect();
    assert_eq!(dups.len(), 2);
    assert_eq!(dups[0], dup_before.as_slice());
    assert_eq!(dups[1], dup_before.as_slice());

    assert_index_consistent(&fx);
    assert_dir_empty(&fx.cache);
}

#[test]
fn a_reordered_document_sends_nothing_to_the_endpoint() {
    let fx = indexed_runbook("f69-reorder", &[S1, S2, S3, S4]);

    fx.layout.write(RUNBOOK, &runbook(&[S2, S1, S3, S4]));
    let before = fx.mock.requests().len();
    let out = fx.run_index();
    let stderr = stderr_of(&out);
    assert!(out.status.success(), "groove index failed:\n{stderr}");
    let line = summary(&out);
    assert!(line.contains("1 updated"), "{line}");
    assert!(line.contains("(this run: 0 embedded, 4 reused)"), "{line}");
    let doc_requests = fx
        .requests_since(before)
        .iter()
        .filter(|r| r.model() == Some(DOC_MODEL))
        .count();
    assert_eq!(
        doc_requests, 0,
        "no document request, not even an empty one"
    );
    assert_eq!(
        contents(&vectors(&fx, RUNBOOK)),
        vec![S2.1, S1.1, S3.1, S4.1]
    );

    assert_index_consistent(&fx);
    assert_dir_empty(&fx.cache);
}

#[test]
fn force_recomputes_every_vector() {
    let fx = indexed_runbook("f69-force", &[S1, S2, S3, S4]);
    let s3 = s3_changed();
    fx.layout
        .write(RUNBOOK, &runbook(&[S1, S2, ("Verify", &s3), S4]));
    index_ok(&fx);

    let before = fx.mock.requests().len();
    let out = fx.index_force();
    assert!(
        out.status.success(),
        "groove index --force failed:\n{}",
        stderr_of(&out)
    );
    let line = summary(&out);
    assert!(line.contains("(this run: 4 embedded, 0 reused)"), "{line}");
    let mut sent: Vec<String> = embedded_since(&fx, before)
        .into_iter()
        .filter(|t| t != grooveseek::embedder::ENDPOINT_PROBE_TEXT)
        .collect();
    sent.sort();
    let mut bodies = vec![
        S1.1.to_string(),
        S2.1.to_string(),
        s3.clone(),
        S4.1.to_string(),
    ];
    bodies.sort();
    assert_eq!(sent, bodies);

    assert_index_consistent(&fx);
    assert_dir_empty(&fx.cache);
}

#[test]
fn a_tags_only_change_counts_every_chunk_reused() {
    let fx = indexed_runbook("f69-tags", &[S1, S2, S3, S4]);

    let tagged =
        runbook(&[S1, S2, S3, S4]).replacen("title: Runbook\n", "title: Runbook\ntags: [ops]\n", 1);
    fx.layout.write(RUNBOOK, &tagged);
    let before = fx.mock.requests().len();
    let line = index_ok(&fx);
    assert!(line.contains("1 updated"), "{line}");
    assert!(line.contains("(this run: 0 embedded, 4 reused)"), "{line}");
    assert!(embedded_since(&fx, before).is_empty());

    assert_index_consistent(&fx);
    assert_dir_empty(&fx.cache);
}

#[test]
fn a_refusal_beside_a_fully_reused_document_fails_the_run() {
    const OTHER: &str = "The ferry timetable changes on the first Monday of each month.";
    let fx = fixture(
        "f69-refusal",
        &[
            (RUNBOOK, &runbook(&[S1, S2, S3, S4])),
            ("other.md", &note("Other", OTHER)),
        ],
        "",
    );
    index_ok(&fx);

    fx.layout.write(RUNBOOK, &runbook(&[S2, S1, S3, S4]));
    fx.layout.write(
        "other.md",
        &note("Other", &format!("{OTHER} {REJECT_MARKER}")),
    );
    fx.answer_with(rejects_marker(413));
    let out = fx.run_index();
    let stderr = stderr_of(&out);
    assert!(!out.status.success(), "groove index must fail:\n{stderr}");
    assert!(
        stderr.contains("the embedding endpoint rejected every input this run tried to embed"),
        "{stderr}"
    );
    let line = summary(&out);
    assert!(line.contains("(this run: 0 embedded, 4 reused)"), "{line}");
    assert_eq!(
        contents(&vectors(&fx, RUNBOOK)),
        vec![S2.1, S1.1, S3.1, S4.1]
    );

    assert_index_consistent(&fx);
    assert_dir_empty(&fx.cache);
}
