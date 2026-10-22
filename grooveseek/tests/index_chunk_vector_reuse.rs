//! feature-69: re-indexing a changed document sends only the chunks whose embedding input
//! changed to the endpoint, and keeps every other chunk's vector as the bytes the index already
//! holds.
//!
//! Each test drives `groove index` over a generated knowledge base against the mock endpoint of
//! [`crate::common::embed_cli`], so nothing downloads a model. The mock answers each input with
//! a vector derived from its text alone, which is what makes "the same bytes as before" a claim
//! about reuse only once the fixture's sensitivity has been checked (see
//! [`crate::an_unchanged_chunk_keeps_its_stored_bytes`]).

mod common;

use common::embed_cli::{Fixture, REJECT_MARKER, fixture, note, rejects_marker, stderr_of};
use common::embed_mock::{DOC_MODEL, assert_dir_empty, hermetic};
use common::mcp::{mcp_initialize, mcp_tool_call, spawn_serve_with};
use common::temp::{TempKbLayout, TempRoot};
use grooveseek::config::Config;
use grooveseek::db::{Database, StoredChunkVector};
use grooveseek::embedder::Embedder;
use grooveseek::indexer::{RenameOutcome, rename_single_file};
use std::process::{Command, Output};

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
/// A body two sections of [`duplicate_chunks_share_one_stored_vector_and_the_counts_add_up`] share.
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

/// The `groove.toml` lines that turn on the static context ([`ContextMode::Static`]).
///
/// [`ContextMode::Static`]: grooveseek::db::ContextMode::Static
const STATIC: &str = "[contextual]\nenabled = true\n";

/// [`indexed_runbook`] under [`STATIC`]: each chunk is embedded as
/// `Runbook > {heading}\n\n{body}`.
fn indexed_static_runbook(prefix: &str, sections: &[(&str, &str)]) -> Fixture {
    let fx = fixture(prefix, &[(RUNBOOK, &runbook(sections))], STATIC);
    index_ok(&fx);
    fx
}

/// The embedding input of `(heading, body)` under [`STATIC`] in a document titled `title`.
fn static_input(title: &str, (heading, body): (&str, &str)) -> String {
    format!("{title} > {heading}\n\n{body}")
}

#[test]
fn static_mode_title_change_recomputes_every_chunk() {
    let fx = indexed_static_runbook("f69-static-title", &[S1, S2, S3, S4]);

    let retitled = runbook(&[S1, S2, S3, S4]).replacen("title: Runbook\n", "title: Playbook\n", 1);
    fx.layout.write(RUNBOOK, &retitled);
    let before = fx.mock.requests().len();
    let line = index_ok(&fx);
    let sent = embedded_since(&fx, before);
    assert_eq!(sent.len(), 4, "{sent:?}");
    assert!(
        sent.iter().all(|t| t.starts_with("Playbook > ")),
        "{sent:?}"
    );
    assert!(line.contains("(this run: 4 embedded, 0 reused)"), "{line}");

    assert_index_consistent(&fx);
    assert_dir_empty(&fx.cache);
}

#[test]
fn static_mode_heading_change_recomputes_that_chunk() {
    let fx = indexed_static_runbook("f69-static-heading", &[S1, S2, S3, S4]);

    let renamed = ("Configuration", S2.1);
    fx.layout.write(RUNBOOK, &runbook(&[S1, renamed, S3, S4]));
    let before = fx.mock.requests().len();
    let line = index_ok(&fx);
    assert_eq!(
        embedded_since(&fx, before),
        vec![static_input("Runbook", renamed)]
    );
    assert!(line.contains("(this run: 1 embedded, 3 reused)"), "{line}");

    assert_index_consistent(&fx);
    assert_dir_empty(&fx.cache);
}

#[test]
fn static_mode_reuses_the_unchanged_chunks() {
    let fx = indexed_static_runbook("f69-static-reuse", &[S1, S2, S3, S4]);

    let s3 = s3_changed();
    fx.layout
        .write(RUNBOOK, &runbook(&[S1, S2, ("Verify", &s3), S4]));
    let before = fx.mock.requests().len();
    let line = index_ok(&fx);
    assert_eq!(
        embedded_since(&fx, before),
        vec![static_input("Runbook", ("Verify", &s3))]
    );
    assert!(line.contains("(this run: 1 embedded, 3 reused)"), "{line}");

    assert_index_consistent(&fx);
    assert_dir_empty(&fx.cache);
}

#[test]
fn a_refused_chunk_leaves_the_document_as_it_was() {
    const OTHER: &str = "The ferry timetable changes on the first Monday of each month.";
    let fx = fixture(
        "f69-refused",
        &[
            (RUNBOOK, &runbook(&[S1, S2, S3, S4])),
            ("other.md", &note("Other", OTHER)),
        ],
        "",
    );
    index_ok(&fx);
    let before = vectors(&fx, RUNBOOK);

    let s3 = format!("{} {REJECT_MARKER}", S3.1);
    fx.layout
        .write(RUNBOOK, &runbook(&[S1, S2, ("Verify", &s3), S4]));
    fx.layout.write(
        "other.md",
        &note("Other", &OTHER.replace("first Monday", "last Friday")),
    );
    fx.answer_with(rejects_marker(413));
    let out = fx.run_index();
    let stderr = stderr_of(&out);
    assert!(out.status.success(), "groove index failed:\n{stderr}");
    let line = summary(&out);
    assert!(line.contains("1 updated"), "{line}");
    assert!(line.contains("1 skipped"), "{line}");
    assert!(
        stderr.contains("warning: runbook.md: embedding endpoint rejected the input (HTTP 413)"),
        "{stderr}"
    );
    assert_eq!(vectors(&fx, RUNBOOK), before);

    assert_index_consistent(&fx);
    assert_dir_empty(&fx.cache);
}

/// Set on the child [`run_in_hermetic_child`] starts, so the child runs the test body.
const HERMETIC_CHILD: &str = "GROOVE_F69_HERMETIC_CHILD";

/// Run the test `name` again in a child of this test binary under the environment
/// [`common::embed_mock::hermetic`] pins. `true` in the parent, after the child passed exactly
/// one test; `false` in the child.
///
/// Copy of the helper of the same name in the PPTX table-policy index test: opening up
/// another test crate's private helper would edit an existing test.
fn run_in_hermetic_child(name: &str) -> bool {
    if std::env::var_os(HERMETIC_CHILD).is_some() {
        return false;
    }
    let cache = TempRoot::new("groove-f69-fastembed");
    let mut cmd = Command::new(std::env::current_exe().expect("this test binary"));
    cmd.args([name, "--exact", "--nocapture", "--test-threads=1"])
        .env(HERMETIC_CHILD, "1");
    hermetic(&mut cmd, cache.path());
    let out = cmd.output().expect("run the test in a child");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "{name} failed in the child:\n{stdout}\n{stderr}"
    );
    assert!(
        stdout.contains("test result: ok. 1 passed"),
        "the child ran no test named {name}:\n{stdout}\n{stderr}"
    );
    assert_dir_empty(cache.path());
    true
}

/// The index and the embedder under `cfg`, opened the way `groove index` opens them.
///
/// Copy of the helper of the same name in the PPTX table-policy index test, for the reason
/// [`run_in_hermetic_child`] gives.
fn open_in_process(fx: &Fixture, cfg: &Config) -> (Database, Embedder) {
    let embedding = cfg.resolve_embedding(None).expect("resolve [embedding]");
    let db_path = grooveseek::resolve_db_path(fx.kb());
    let db = Database::open(&db_path.to_string_lossy()).expect("open the index");
    db.verify_embedding_meta(embedding.model_id(), embedding.dimension() as u32)
        .expect("embedding meta");
    let embedder = Embedder::with_settings(embedding).expect("build the embedder");
    (db, embedder)
}

/// Spec R7 on the watcher's side: a same-bytes rename under the static context re-parses the
/// document with `Force`, which reads no stored vector, so every chunk is embedded again even
/// though the title keeps every input as it was and an incremental pass would reuse them all.
#[test]
fn a_static_same_bytes_rename_recomputes_every_chunk() {
    if run_in_hermetic_child("a_static_same_bytes_rename_recomputes_every_chunk") {
        return;
    }
    let fx = indexed_static_runbook("f69-static-rename", &[S1, S2, S3, S4]);
    std::fs::rename(fx.kb().join(RUNBOOK), fx.kb().join("renamed.md")).expect("rename");

    let cfg = Config::load_from(&fx.config).expect("load groove.toml");
    let registry = cfg.build_parser_registry(fx.kb()).expect("parser registry");
    let kb = fx.kb().canonicalize().expect("canonical kb");
    let (db, mut embedder) = open_in_process(&fx, &cfg);
    let before = fx.mock.requests().len();
    let outcome = rename_single_file(
        &db,
        &mut embedder,
        &kb,
        RUNBOOK,
        "renamed.md",
        None,
        &registry,
    )
    .expect("rename the runbook");
    assert_eq!(outcome, RenameOutcome::RenamedAndReindexed { chunks: 4 });
    let mut sent = embedded_since(&fx, before);
    sent.sort();
    let mut inputs: Vec<String> = [S1, S2, S3, S4]
        .into_iter()
        .map(|s| static_input("Runbook", s))
        .collect();
    inputs.sort();
    assert_eq!(sent, inputs);

    drop(db);
    assert_index_consistent(&fx);
    assert_dir_empty(&fx.cache);
}

/// Spec §6.2: the copy of the stored vectors, the delete of the old rows and the insert of the
/// new ones share one transaction, so an insert that fails after the reused chunks went in
/// leaves the document's old rows.
#[test]
fn a_write_that_fails_midway_leaves_the_document_as_it_was() {
    let fx = indexed_runbook("f69-midway", &[S1, S2, S3, S4]);
    {
        let conn = rusqlite::Connection::open(grooveseek::resolve_db_path(fx.kb()))
            .expect("open the index");
        conn.execute_batch("CREATE TRIGGER inject_chunk_failure BEFORE INSERT ON chunks WHEN NEW.content LIKE '%injectfail%' BEGIN SELECT RAISE(ABORT, 'injected'); END;")
            .expect("create the trigger");
        let probe = conn
            .execute(
                "INSERT INTO chunks (content) VALUES ('probe injectfail')",
                [],
            )
            .expect_err("premise: the trigger stops a chunk that carries the word");
        assert!(probe.to_string().contains("injected"), "{probe}");
    }
    let before = vectors(&fx, RUNBOOK);

    let s3 = format!("{} injectfail", S3.1);
    fx.layout
        .write(RUNBOOK, &runbook(&[S1, S2, ("Verify", &s3), S4]));
    let out = fx.run_index();
    let stderr = stderr_of(&out);
    assert!(!out.status.success(), "groove index must fail:\n{stderr}");
    assert!(stderr.contains("injected"), "{stderr}");
    assert_eq!(vectors(&fx, RUNBOOK), before);

    assert_index_consistent(&fx);
    assert_dir_empty(&fx.cache);
}

/// `key` of the MCP reply `resp`, which must be a non-negative integer.
fn count_in(resp: &serde_json::Value, key: &str) -> u64 {
    resp.get(key)
        .and_then(|v| v.as_u64())
        .unwrap_or_else(|| panic!("no count {key}: {resp}"))
}

/// Spec R9 (§8 d): the MCP tool that runs [`grooveseek::indexer::rebuild_index`] answers with the run's chunk counts beside its other
/// counters, and an unchanged second run still carries both keys, at zero.
#[test]
fn mcp_rebuild_index_returns_the_chunk_counts() {
    let fx = indexed_runbook("f69-mcp", &[S1, S2, S3, S4]);
    let s3 = s3_changed();
    fx.layout
        .write(RUNBOOK, &runbook(&[S1, S2, ("Verify", &s3), S4]));

    let (guard, base) = spawn_serve_with(fx.kb(), &fx.config, false, |c| {
        hermetic(c, &fx.cache);
    });
    let session = mcp_initialize(&base);
    let first = mcp_tool_call(&base, &session, "rebuild_index", serde_json::json!({}));
    let second = mcp_tool_call(&base, &session, "rebuild_index", serde_json::json!({}));
    drop(guard);

    assert!(first.get("error").is_none(), "{first}");
    assert_eq!(count_in(&first, "updated"), 1, "{first}");
    assert_eq!(count_in(&first, "chunks_embedded"), 1, "{first}");
    assert_eq!(count_in(&first, "chunks_reused"), 3, "{first}");

    assert!(second.get("error").is_none(), "{second}");
    assert_eq!(count_in(&second, "chunks_embedded"), 0, "{second}");
    assert_eq!(count_in(&second, "chunks_reused"), 0, "{second}");

    assert_index_consistent(&fx);
    assert_dir_empty(&fx.cache);
}

/// `groove.toml` for the real-model test: BGE-small with no context, as
/// `CONTEXTUAL_OFF` in the contextual-retrieval CLI test.
const BGE_SMALL_NO_CONTEXT: &str = "model = \"bge-small-en-v1.5\"\n[contextual]\nenabled = false\n";

/// `groove --config <cfg> index --kb-path <kb> [--force]` with the real model, which must
/// succeed; its `Done in` line.
fn index_real(layout: &TempKbLayout, cfg: &std::path::Path, force: bool) -> String {
    let mut cmd = Command::new(common::mcp::grooveseek_bin());
    cmd.arg("--config")
        .arg(cfg)
        .args(["index", "--kb-path"])
        .arg(layout.kb());
    if force {
        cmd.arg("--force");
    }
    let out = cmd.output().expect("groove index");
    assert!(
        out.status.success(),
        "groove index failed:\n{}",
        stderr_of(&out)
    );
    summary(&out)
}

/// The `M` of `(this run: N embedded, M reused)` in `line`.
fn reused_in(line: &str) -> u64 {
    line.split("embedded, ")
        .nth(1)
        .and_then(|rest| rest.split(" reused").next())
        .and_then(|n| n.parse().ok())
        .unwrap_or_else(|| panic!("no reused count: {line}"))
}

/// `(path, heading, score)` of each hit of `query` in the top five, with the quality filter and
/// the confidence cutoff off so both indexes answer from the same candidates.
fn top5_real(
    layout: &TempKbLayout,
    cfg: &std::path::Path,
    query: &str,
) -> Vec<(String, String, f64)> {
    let out = Command::new(common::mcp::grooveseek_bin())
        .arg("--config")
        .arg(cfg)
        .args(["search", query, "--kb-path"])
        .arg(layout.kb())
        .args([
            "--format",
            "json",
            "--limit",
            "5",
            "--include-low-quality",
            "--min-confidence-ratio",
            "0",
        ])
        .output()
        .expect("groove search");
    assert!(
        out.status.success(),
        "groove search failed:\n{}",
        stderr_of(&out)
    );
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
        panic!(
            "search stdout is not JSON ({e}): {}",
            String::from_utf8_lossy(&out.stdout)
        )
    });
    v["results"]
        .as_array()
        .unwrap_or_else(|| panic!("no results array: {v}"))
        .iter()
        .map(|h| {
            (
                h["path"].as_str().unwrap_or_default().to_string(),
                h["heading"].as_str().unwrap_or_default().to_string(),
                h["score"]
                    .as_f64()
                    .unwrap_or_else(|| panic!("no score: {h}")),
            )
        })
        .collect()
}

/// Spec §8 g (D11): an index that reused stored vectors ranks like one built from scratch with
/// `--force` over the same final content, under the real BGE-small model.
#[test]
#[ignore = "requires embedding model download"]
fn reused_vectors_rank_like_a_forced_rebuild() {
    let notes = note(
        "Notes",
        "The ferry timetable changes on the first Monday of each month.",
    );
    let s3 = s3_changed();
    let edited = runbook(&[S1, INS, S2, ("Verify", &s3), S4]);

    let reused = TempKbLayout::new("f69-real-reuse");
    let reused_cfg = reused.root().join("groove.toml");
    std::fs::write(&reused_cfg, BGE_SMALL_NO_CONTEXT).expect("write groove.toml");
    reused.write(RUNBOOK, &runbook(&[S1, S2, S3, S4]));
    reused.write("notes.md", &notes);
    index_real(&reused, &reused_cfg, false);
    reused.write(RUNBOOK, &edited);
    let line = index_real(&reused, &reused_cfg, false);
    assert!(reused_in(&line) >= 1, "{line}");

    let forced = TempKbLayout::new("f69-real-force");
    let forced_cfg = forced.root().join("groove.toml");
    std::fs::write(&forced_cfg, BGE_SMALL_NO_CONTEXT).expect("write groove.toml");
    forced.write(RUNBOOK, &edited);
    forced.write("notes.md", &notes);
    index_real(&forced, &forced_cfg, true);

    for query in [
        "smoke check endpoint",
        "queue depth dashboard",
        "ferry timetable",
    ] {
        let a = top5_real(&reused, &reused_cfg, query);
        let b = top5_real(&forced, &forced_cfg, query);
        let keys = |hits: &[(String, String, f64)]| -> Vec<(String, String)> {
            hits.iter()
                .map(|(p, h, _)| (p.clone(), h.clone()))
                .collect()
        };
        assert!(!a.is_empty(), "{query}: no hits");
        assert_eq!(keys(&a), keys(&b), "{query}: reused {a:?} / forced {b:?}");
        for (x, y) in a.iter().zip(&b) {
            assert!(
                (x.2 - y.2).abs() <= 1e-4,
                "{query}: score {} vs {} at {}/{}",
                x.2,
                y.2,
                x.0,
                x.1
            );
        }
    }
}
