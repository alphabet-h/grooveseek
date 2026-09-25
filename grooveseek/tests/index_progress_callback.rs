//! AW-08: [`grooveseek::indexer::rebuild_index`] drives a callback reporter.
//!
//! [`grooveseek::indexer::progress`]'s own tests call the reporter's methods
//! by hand, and the CLI progress tests only read the stderr lines of the
//! other modes. Neither notices when [`grooveseek::indexer::rebuild_index`]
//! stops calling one of those methods, and the application that embeds this
//! crate (grooveseek-desktop) draws its progress from nothing else. These run
//! [`grooveseek::indexer::rebuild_index`] in-process with
//! [`grooveseek::indexer::progress::ProgressReporter::with_callback`] and
//! read back what the callback was handed.
//!
//! The embedder is the OpenAI-compatible provider pointed at
//! [`crate::common::embed_mock`], so nothing is downloaded and nothing is
//! `#[ignore]`d.

mod common;

use common::embed_mock::{EmbedMock, MockResponse, default_response, openai_config_toml};
use common::temp::TempKbLayout;

use grooveseek::config::Config;
use grooveseek::db::{ContextMode, Database};
use grooveseek::embedder::Embedder;
use grooveseek::indexer::progress::{ProgressCallback, ProgressEvent, ProgressReporter};
use grooveseek::indexer::{IndexResult, load_declared_schema, rebuild_index};

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

/// The mock's vector length. Small, since nothing here ranks anything.
const DIM: usize = 8;

/// One [`ProgressEvent`], owned so it can outlive the call that delivered it.
///
/// `Indexed` leaves out `chunks`: how many chunks a file is cut into belongs
/// to the parser, and nothing here is about that.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Ev {
    Started(usize),
    Indexed(String, usize, usize),
    Unchanged(String, usize, usize),
    Renamed(String, String),
    Deleted(String),
    Finished,
}

fn indexed(rel: &str, done: usize, total: usize) -> Ev {
    Ev::Indexed(rel.to_string(), done, total)
}

fn unchanged(rel: &str, done: usize, total: usize) -> Ev {
    Ev::Unchanged(rel.to_string(), done, total)
}

/// What a test runs inside the callback, after the event is logged.
type Hook = Box<dyn Fn(&Ev) + Send>;

/// A callback that appends every event to a log and then hands it to `hook`,
/// and the log.
fn recorder(hook: Hook) -> (Arc<Mutex<Vec<Ev>>>, ProgressCallback) {
    let log = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&log);
    let f: ProgressCallback = Box::new(move |ev| {
        let owned = match ev {
            ProgressEvent::Started { total } => Ev::Started(total),
            ProgressEvent::Indexed {
                rel, done, total, ..
            } => indexed(rel, done, total),
            ProgressEvent::Unchanged { rel, done, total } => unchanged(rel, done, total),
            ProgressEvent::Renamed { old, new } => Ev::Renamed(old.to_string(), new.to_string()),
            ProgressEvent::Deleted { rel } => Ev::Deleted(rel.to_string()),
            ProgressEvent::Finished => Ev::Finished,
        };
        sink.lock().expect("recorder mutex").push(owned.clone());
        hook(&owned);
    });
    (log, f)
}

/// A knowledge base, the mock it embeds through, and the `groove.toml` that
/// points at the mock.
///
/// The config sits in [`TempKbLayout::root`], next to `.groove.db` and
/// outside the knowledge base, so it is never one of the files the scan
/// counts.
struct Fixture {
    layout: TempKbLayout,
    mock: EmbedMock,
    config: PathBuf,
}

impl Fixture {
    fn new(prefix: &str, mock: EmbedMock) -> Self {
        let layout = TempKbLayout::new(prefix);
        let config = layout.root().join("groove.toml");
        std::fs::write(&config, openai_config_toml(&mock.endpoint(), None, DIM))
            .expect("write groove.toml");
        Self {
            layout,
            mock,
            config,
        }
    }

    /// One incremental [`grooveseek::indexer::rebuild_index`] run, wired the
    /// way `groove index` wires it, with a callback reporter; its result and
    /// every event the callback received. The reporter is the one argument
    /// that differs from the command line's: no CLI flag selects a callback,
    /// and it is what is under test.
    fn run(&self) -> (anyhow::Result<IndexResult>, Vec<Ev>) {
        self.run_with(Box::new(|_| {}))
    }

    /// [`Fixture::run`], with `hook` called from inside the callback after
    /// each event is logged.
    fn run_with(&self, hook: Hook) -> (anyhow::Result<IndexResult>, Vec<Ev>) {
        let kb = self.layout.kb();
        let cfg = Config::load_from(&self.config).expect("load groove.toml");
        let embedding = cfg.resolve_embedding(None).expect("resolve [embedding]");
        let registry = cfg.build_parser_registry(kb).expect("parser registry");
        let schema = load_declared_schema(kb).expect("groove-schema.toml");
        let db_path = grooveseek::resolve_db_path(kb);
        let db = Database::open(&db_path.to_string_lossy()).expect("open the index");
        // `groove index` does this before building the embedder, and it is
        // what creates the vector table on a fresh index.
        db.verify_embedding_meta(embedding.model_id(), embedding.dimension() as u32)
            .expect("embedding meta");
        let mut embedder = Embedder::with_settings(embedding).expect("build the embedder");
        let exclude_dirs = cfg.resolve_exclude_dirs();
        let context_mode = if cfg.contextual.as_ref().map(|c| c.enabled).unwrap_or(false) {
            ContextMode::Static
        } else {
            ContextMode::Off
        };
        let (log, f) = recorder(hook);
        let result = rebuild_index(
            &db,
            &mut embedder,
            kb,
            schema,
            false,
            cfg.exclude_headings.as_deref(),
            &exclude_dirs,
            &registry,
            ProgressReporter::with_callback(f),
            context_mode,
        );
        let events = log.lock().expect("recorder mutex").clone();
        (result, events)
    }
}

fn doc(title: &str, body: &str) -> String {
    format!("---\ntitle: {title}\n---\n\n## {title}\n\n{body}\n")
}

/// Every file the scan hands the loop is reported exactly once, as
/// `Indexed` when it was embedded and as `Unchanged` when it was not, and
/// the done count goes up one file at a time against the total `Started`
/// announced.
///
/// `stub.md` is frontmatter and nothing else, so the loop skips it for having
/// no chunks; that skip is still one step of the run. The second run finds
/// every hash unchanged. Red if [`grooveseek::indexer::rebuild_index`] stops
/// calling [`grooveseek::indexer::progress::ProgressReporter::start_indexing`],
/// [`grooveseek::indexer::progress::ProgressReporter::report_indexed`],
/// [`grooveseek::indexer::progress::ProgressReporter::finish`], or
/// [`grooveseek::indexer::progress::ProgressReporter::report_unchanged`] in
/// the arm for a skipped file or for an unchanged one.
#[test]
fn rebuild_index_reports_each_scanned_file_once_through_the_callback() {
    let fx = Fixture::new("groove-aw08-each", EmbedMock::start(DIM));
    fx.layout.write("a.md", &doc("Alpha", "alpha body text"));
    fx.layout.write("b.md", &doc("Beta", "beta body text"));
    fx.layout.write("stub.md", "---\ntitle: Stub\n---\n");

    let (result, events) = fx.run();
    let result = result.expect("first run");
    assert_eq!((result.updated, result.skipped), (2, 1), "{result:?}");
    assert_eq!(
        events,
        vec![
            Ev::Started(3),
            indexed("a.md", 1, 3),
            indexed("b.md", 2, 3),
            unchanged("stub.md", 3, 3),
            Ev::Finished,
        ],
        "first run"
    );

    let (result, events) = fx.run();
    let result = result.expect("second run");
    assert_eq!(result.updated, 0, "{result:?}");
    assert_eq!(
        events,
        vec![
            Ev::Started(3),
            unchanged("a.md", 1, 3),
            unchanged("b.md", 2, 3),
            unchanged("stub.md", 3, 3),
            Ev::Finished,
        ],
        "second run: nothing changed on disk"
    );
}

/// A run that only rewrites metadata still reports each file as a step.
///
/// Declaring a schema after the first run makes the second one read every
/// unchanged Markdown document again for its declared fields, without
/// re-embedding it. Red if [`grooveseek::indexer::rebuild_index`] stops
/// calling [`grooveseek::indexer::progress::ProgressReporter::report_unchanged`]
/// in the arm for a metadata-only refresh.
#[test]
fn rebuild_index_reports_a_metadata_only_refresh_as_unchanged() {
    let fx = Fixture::new("groove-aw08-refresh", EmbedMock::start(DIM));
    fx.layout.write(
        "a.md",
        "---\ntitle: Alpha\nstatus: active\n---\n\n## Alpha\n\nalpha body text\n",
    );
    fx.layout.write("b.md", &doc("Beta", "beta body text"));
    fx.run().0.expect("first run");
    let embeds_before = fx.mock.requests().len();

    fx.layout.write(
        "groove-schema.toml",
        "[fields.status]\nenum = [\"active\", \"deprecated\"]\n",
    );
    let (result, events) = fx.run();
    let result = result.expect("second run");
    assert_eq!(result.updated, 0, "{result:?}");
    assert_eq!(
        fx.mock.requests().len(),
        embeds_before,
        "a metadata-only refresh must not embed anything"
    );
    assert_eq!(
        events,
        vec![
            Ev::Started(2),
            unchanged("a.md", 1, 2),
            unchanged("b.md", 2, 2),
            Ev::Finished,
        ]
    );
}

/// A file moved with its bytes unchanged is reported as `Renamed`, and a
/// file gone from disk as `Deleted`, and neither advances the done count.
///
/// `Renamed` comes before the per-file loop and `Deleted` after it, which is
/// where [`grooveseek::indexer::rebuild_index`] detects each. Red if it stops
/// calling [`grooveseek::indexer::progress::ProgressReporter::report_renamed`]
/// or [`grooveseek::indexer::progress::ProgressReporter::report_deleted`].
#[test]
fn rebuild_index_reports_a_same_hash_move_as_renamed_and_a_vanished_file_as_deleted() {
    let fx = Fixture::new("groove-aw08-move", EmbedMock::start(DIM));
    fx.layout.write("a.md", &doc("Alpha", "alpha body text"));
    fx.layout.write("b.md", &doc("Beta", "beta body text"));
    fx.layout.write("c.md", &doc("Gamma", "gamma body text"));
    fx.run().0.expect("first run");

    let kb = fx.layout.kb();
    std::fs::rename(kb.join("a.md"), kb.join("moved.md")).expect("move a.md");
    std::fs::remove_file(kb.join("b.md")).expect("remove b.md");

    let (result, events) = fx.run();
    let result = result.expect("second run");
    assert_eq!(
        (result.renamed, result.deleted, result.updated),
        (1, 1, 0),
        "{result:?}"
    );
    assert_eq!(
        events,
        vec![
            Ev::Started(2),
            Ev::Renamed("a.md".to_string(), "moved.md".to_string()),
            unchanged("c.md", 1, 2),
            unchanged("moved.md", 2, 2),
            Ev::Deleted("b.md".to_string()),
            Ev::Finished,
        ]
    );
}

/// `Finished` arrives once, at the end of a run that returned `Ok`, and not
/// at all from one that returned `Err`.
///
/// An endpoint that answers 500 fails the first embed, and
/// [`grooveseek::indexer::rebuild_index`] returns through `?` after
/// `Started`. The consumer learns the run ended from that `Err`; a `Finished`
/// as well would tell it the run completed. A failure later in the run is
/// covered by
/// [`rebuild_index_emits_no_finished_when_an_embed_fails_after_the_first_file`]
/// and [`rebuild_index_emits_no_finished_when_the_last_step_before_it_fails`].
#[test]
fn rebuild_index_emits_finished_only_on_success() {
    let ok = Fixture::new("groove-aw08-finished-ok", EmbedMock::start(DIM));
    ok.layout.write("a.md", &doc("Alpha", "alpha body text"));
    let (result, events) = ok.run();
    result.expect("a run against a working endpoint");
    assert_eq!(
        events.iter().filter(|e| **e == Ev::Finished).count(),
        1,
        "{events:?}"
    );
    assert_eq!(events.last(), Some(&Ev::Finished), "{events:?}");

    let failing = Fixture::new(
        "groove-aw08-finished-err",
        EmbedMock::with_responder(|_| {
            MockResponse::json(
                500,
                &serde_json::json!({"error": {"message": "mock: down"}}),
            )
        }),
    );
    failing
        .layout
        .write("a.md", &doc("Alpha", "alpha body text"));
    let (result, events) = failing.run();
    assert!(result.is_err(), "the embed failure must reach the caller");
    assert_eq!(
        events,
        vec![Ev::Started(1)],
        "a run that returned Err must not report Finished"
    );
}

/// A run that fails partway through the per-file loop, after a file was
/// already reported, emits no `Finished` either.
///
/// The endpoint answers the first embed request and fails every later one.
/// Each small file here is one request, so exactly one file is `Indexed`
/// before the second fails; which file that is depends on the walk's order,
/// and the assertions do not.
#[test]
fn rebuild_index_emits_no_finished_when_an_embed_fails_after_the_first_file() {
    let served = AtomicUsize::new(0);
    let fx = Fixture::new(
        "groove-aw08-midway",
        EmbedMock::with_responder(move |req| {
            if served.fetch_add(1, Ordering::SeqCst) == 0 {
                default_response(req, DIM)
            } else {
                MockResponse::json(
                    500,
                    &serde_json::json!({"error": {"message": "mock: down"}}),
                )
            }
        }),
    );
    fx.layout.write("a.md", &doc("Alpha", "alpha body text"));
    fx.layout.write("b.md", &doc("Beta", "beta body text"));

    let (result, events) = fx.run();
    assert!(
        result.is_err(),
        "the second embed failure must reach the caller"
    );
    assert_eq!(events.len(), 2, "{events:?}");
    assert_eq!(events[0], Ev::Started(2), "{events:?}");
    assert!(
        matches!(&events[1], Ev::Indexed(rel, 1, 2) if rel == "a.md" || rel == "b.md"),
        "one file is reported before the failure: {events:?}"
    );
    assert!(!events.contains(&Ev::Finished), "{events:?}");
}

/// A run that fails after the per-file loop, in the last fallible step before
/// the reporter is finished, emits no `Finished`.
///
/// The hook drops the `chunks` table from a second connection as the last
/// file is reported. Nothing between that report and the chunk count
/// [`grooveseek::indexer::rebuild_index`] takes just before finishing the
/// reporter reads that table (no file vanished, so the deletion sweep removes
/// nothing), so the count is the step that fails. Red if the reporter is
/// finished ahead of any fallible step after the loop.
#[test]
fn rebuild_index_emits_no_finished_when_the_last_step_before_it_fails() {
    let fx = Fixture::new("groove-aw08-tail", EmbedMock::start(DIM));
    fx.layout.write("a.md", &doc("Alpha", "alpha body text"));
    fx.layout.write("b.md", &doc("Beta", "beta body text"));
    let db_path = grooveseek::resolve_db_path(fx.layout.kb());

    let (result, events) = fx.run_with(Box::new(move |ev| {
        if let Ev::Indexed(_, done, total) = ev
            && done == total
        {
            rusqlite::Connection::open(&db_path)
                .expect("second connection")
                .execute_batch("DROP TABLE chunks")
                .expect("drop chunks");
        }
    }));
    let err = format!("{:#}", result.expect_err("the chunk count must fail"));
    assert!(err.contains("chunks"), "failed somewhere else: {err}");
    assert_eq!(
        events,
        vec![Ev::Started(2), indexed("a.md", 1, 2), indexed("b.md", 2, 2)],
        "a run that returned Err must not report Finished"
    );
}

/// The done count stops short of the total by exactly the files the scan
/// declined.
///
/// The total is what the walk found; a file over the size cap is declined by
/// the scan before the loop, so it reaches neither `Indexed` nor `Unchanged`
/// and the done count never includes it. That is the contract the rustdoc on
/// [`grooveseek::indexer::progress::ProgressEvent::Indexed`] states, so a
/// consumer does not read a done count below the total at `Finished` as a
/// failure.
/// `set_len` makes the oversized file without writing its bytes: the scan
/// decides from its metadata and never reads it.
#[test]
fn callback_done_stops_short_of_total_exactly_by_the_files_the_scan_declined() {
    let fx = Fixture::new("groove-aw08-short", EmbedMock::start(DIM));
    fx.layout.write("a.md", &doc("Alpha", "alpha body text"));
    fx.layout.write("b.md", &doc("Beta", "beta body text"));
    let huge = std::fs::File::create(fx.layout.kb().join("huge.md")).expect("create huge.md");
    huge.set_len(grooveseek::parser::MAX_RAW_TEXT_BYTES + 1)
        .expect("grow huge.md past the text cap");
    drop(huge);

    let (result, events) = fx.run();
    let result = result.expect("run");
    assert_eq!((result.updated, result.skipped), (2, 1), "{result:?}");
    assert_eq!(
        events,
        vec![
            Ev::Started(3),
            indexed("a.md", 1, 3),
            indexed("b.md", 2, 3),
            Ev::Finished,
        ],
        "huge.md is in total but in no per-file event, so done ends at total - 1"
    );
}
