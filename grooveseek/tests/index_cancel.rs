//! feature-60: an indexing run can be stopped through a
//! [`grooveseek::indexer::progress::CancelToken`], and the scan reports its
//! progress.
//!
//! These run [`grooveseek::indexer::rebuild_index`] in-process against
//! [`crate::common::embed_mock`], each test body in a hermetic child of this
//! test binary, the way `index_progress_callback.rs` does -- its module doc
//! says why. The recorder here keeps every event,
//! [`grooveseek::indexer::progress::ProgressEvent::Scanning`] included, which
//! is why these live in a file of their own: the recorder there leaves the
//! scan out so its exact sequences stay as they were.

mod common;

use common::embed_mock::{EmbedMock, assert_dir_empty, hermetic, openai_config_toml};
use common::temp::{TempKbLayout, TempRoot};

use grooveseek::config::Config;
use grooveseek::db::{ContextMode, Database};
use grooveseek::embedder::Embedder;
use grooveseek::indexer::progress::{
    CancelToken, ProgressCallback, ProgressEvent, ProgressMode, ProgressReporter,
};
use grooveseek::indexer::{IndexResult, load_declared_schema, rebuild_index};

use std::path::PathBuf;
use std::process::Command;
use std::sync::{Arc, Mutex};

/// The mock's vector length. Small, since nothing here ranks anything.
const DIM: usize = 8;

/// Set on the child [`run_in_hermetic_child`] starts, so the child runs the
/// test body instead of starting another child.
const HERMETIC_CHILD: &str = "GROOVE_F60_HERMETIC_CHILD";

/// Run the test named `name` again in a child of this test binary, under the
/// environment [`crate::common::embed_mock::hermetic`] pins. Returns `true` in
/// the parent, after asserting the child passed exactly one test, so the
/// caller returns; `false` in the child, which then runs the body.
fn run_in_hermetic_child(name: &str) -> bool {
    if std::env::var_os(HERMETIC_CHILD).is_some() {
        return false;
    }
    let cache = TempRoot::new("groove-f60-fastembed");
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

/// One [`ProgressEvent`], owned so it can outlive the call that delivered it.
/// `Indexed` leaves out `chunks`, which belongs to the parser.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Ev {
    Started(usize),
    Scanning(usize, usize),
    Indexed(String, usize, usize),
    Unchanged(String, usize, usize),
    Renamed(String, String),
    Deleted(String),
    Finished,
    Cancelled(usize, usize),
}

fn indexed(rel: &str, done: usize, total: usize) -> Ev {
    Ev::Indexed(rel.to_string(), done, total)
}

fn unchanged(rel: &str, done: usize, total: usize) -> Ev {
    Ev::Unchanged(rel.to_string(), done, total)
}

/// `Scanning(1, total)` .. `Scanning(total, total)`: a scan that visited every
/// file.
fn scanning(total: usize) -> Vec<Ev> {
    (1..=total).map(|done| Ev::Scanning(done, total)).collect()
}

/// Decides, from inside the callback, whether to set the run's token after an
/// event.
type CancelWhen = Box<dyn Fn(&Ev) -> bool + Send>;

/// A callback that logs every event and sets `token` after each event `when`
/// answers `true` for, and the log.
fn recorder(token: CancelToken, when: CancelWhen) -> (Arc<Mutex<Vec<Ev>>>, ProgressCallback) {
    let log = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&log);
    let f: ProgressCallback = Box::new(move |ev| {
        let owned = match ev {
            ProgressEvent::Started { total } => Ev::Started(total),
            ProgressEvent::Scanning { done, total } => Ev::Scanning(done, total),
            ProgressEvent::Indexed {
                rel, done, total, ..
            } => indexed(rel, done, total),
            ProgressEvent::Unchanged { rel, done, total } => unchanged(rel, done, total),
            ProgressEvent::Renamed { old, new } => Ev::Renamed(old.to_string(), new.to_string()),
            ProgressEvent::Deleted { rel } => Ev::Deleted(rel.to_string()),
            ProgressEvent::Finished => Ev::Finished,
            ProgressEvent::Cancelled { done, total } => Ev::Cancelled(done, total),
        };
        if when(&owned) {
            token.cancel();
        }
        sink.lock().expect("recorder mutex").push(owned);
    });
    (log, f)
}

/// A knowledge base, the mock it embeds through, and the `groove.toml` that
/// points at the mock, beside the knowledge base so the scan never counts it.
struct Fixture {
    layout: TempKbLayout,
    mock: EmbedMock,
    config: PathBuf,
}

impl Fixture {
    fn new(prefix: &str) -> Self {
        Self::with_extra_config(prefix, "")
    }

    /// [`Fixture::new`] with `extra` appended to the `groove.toml`.
    fn with_extra_config(prefix: &str, extra: &str) -> Self {
        let mock = EmbedMock::start(DIM);
        let layout = TempKbLayout::new(prefix);
        let config = layout.root().join("groove.toml");
        let toml = format!("{}{extra}", openai_config_toml(&mock.endpoint(), None, DIM));
        std::fs::write(&config, toml).expect("write groove.toml");
        Self {
            layout,
            mock,
            config,
        }
    }

    /// A run with a callback reporter and no token at all: the path every
    /// existing caller takes.
    fn run(&self, force: bool) -> (anyhow::Result<IndexResult>, Vec<Ev>) {
        let (log, f) = recorder(CancelToken::new(), Box::new(|_| false));
        let result = self.rebuild(force, ProgressReporter::with_callback(f));
        let events = log.lock().expect("recorder mutex").clone();
        (result, events)
    }

    /// A run whose reporter carries a token, set from inside the callback
    /// right after the first event `when` answers `true` for.
    fn run_cancelling(
        &self,
        force: bool,
        when: CancelWhen,
    ) -> (anyhow::Result<IndexResult>, Vec<Ev>) {
        let token = CancelToken::new();
        let (log, f) = recorder(token.clone(), when);
        let result = self.rebuild(force, ProgressReporter::with_callback(f).with_cancel(token));
        let events = log.lock().expect("recorder mutex").clone();
        (result, events)
    }

    /// [`grooveseek::indexer::rebuild_index`] wired the way `groove index`
    /// wires it, with `progress` as the reporter.
    fn rebuild(&self, force: bool, progress: ProgressReporter) -> anyhow::Result<IndexResult> {
        let kb = self.layout.kb();
        let cfg = Config::load_from(&self.config).expect("load groove.toml");
        let embedding = cfg.resolve_embedding(None).expect("resolve [embedding]");
        let registry = cfg.build_parser_registry(kb).expect("parser registry");
        let schema = load_declared_schema(kb).expect("groove-schema.toml");
        let db = self.db();
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
        rebuild_index(
            &db,
            &mut embedder,
            kb,
            schema,
            force,
            cfg.exclude_headings.as_deref(),
            &exclude_dirs,
            &registry,
            progress,
            context_mode,
        )
    }

    /// A fresh connection to the index this fixture's runs write.
    fn db(&self) -> Database {
        let db_path = grooveseek::resolve_db_path(self.layout.kb());
        Database::open(&db_path.to_string_lossy()).expect("open the index")
    }

    /// Every `documents.path`, sorted.
    fn paths(&self) -> Vec<String> {
        self.db().all_document_paths().expect("document paths")
    }

    /// How many embedding requests the mock has received so far.
    fn requests(&self) -> usize {
        self.mock.requests().len()
    }
}

fn doc(title: &str, body: &str) -> String {
    format!("---\ntitle: {title}\n---\n\n## {title}\n\n{body}\n")
}

/// Criteria 3 and 4: a run with no token reports one `Scanning` per file the
/// walk found -- the one the scan declines for its size too, so `done`
/// reaches `total` -- between `Started` and the first document event, and is
/// otherwise the sequence it was before. The document `done` still stops at
/// the files the loop processed.
#[test]
fn scan_reports_every_visited_file_including_declined_ones() {
    if run_in_hermetic_child("scan_reports_every_visited_file_including_declined_ones") {
        return;
    }
    let fx = Fixture::new("groove-f60-scan");
    fx.layout.write("a.md", &doc("Alpha", "alpha body text"));
    fx.layout.write("b.md", &doc("Beta", "beta body text"));
    let huge = std::fs::File::create(fx.layout.kb().join("huge.md")).expect("create huge.md");
    huge.set_len(grooveseek::parser::MAX_RAW_TEXT_BYTES + 1)
        .expect("grow huge.md past the text cap");
    drop(huge);

    let (result, events) = fx.run(false);
    let result = result.expect("run");
    assert!(!result.cancelled, "{result:?}");
    assert_eq!((result.updated, result.skipped), (2, 1), "{result:?}");
    let mut expected = vec![Ev::Started(3)];
    expected.extend(scanning(3));
    expected.extend([indexed("a.md", 1, 3), indexed("b.md", 2, 3), Ev::Finished]);
    assert_eq!(
        events, expected,
        "huge.md is scanned (Scanning reaches 3/3) but reaches no document event (done ends at 2)"
    );
}

/// The reporter's promise that `total == 0` reports no `Scanning`, pinned at
/// the only place that produces the event: a run over an empty knowledge base
/// goes from `Started { total: 0 }` straight to its terminal event.
#[test]
fn a_scan_with_no_files_emits_no_scanning_event() {
    if run_in_hermetic_child("a_scan_with_no_files_emits_no_scanning_event") {
        return;
    }
    let fx = Fixture::new("groove-f60-empty-scan");

    let (result, events) = fx.run(false);
    let result = result.expect("run");
    assert!(!result.cancelled, "{result:?}");
    assert!(
        !events.iter().any(|e| matches!(e, Ev::Scanning(..))),
        "an empty scan must report no Scanning: {events:?}"
    );
    assert_eq!(events, vec![Ev::Started(0), Ev::Finished]);
}

/// Criterion 5 (C1): a token set from the first `Scanning` stops the run
/// before the next file is read. Nothing is embedded, the index holds what it
/// held before the run, and the run returns `Ok` with `cancelled`.
#[test]
fn cancel_during_scan_embeds_nothing() {
    if run_in_hermetic_child("cancel_during_scan_embeds_nothing") {
        return;
    }
    let fx = Fixture::new("groove-f60-scan-stop");
    fx.layout.write("a.md", &doc("Alpha", "alpha body text"));
    fx.run(false).0.expect("first run");
    assert_eq!(fx.paths(), ["a.md"]);
    let requests_before = fx.requests();
    fx.layout.write("b.md", &doc("Beta", "beta body text"));
    fx.layout.write("c.md", &doc("Gamma", "gamma body text"));

    let (result, events) = fx.run_cancelling(false, Box::new(|ev| *ev == Ev::Scanning(1, 3)));
    let result = result.expect("a stopped run returns Ok");
    assert!(result.cancelled, "{result:?}");
    assert_eq!(
        (
            result.updated,
            result.renamed,
            result.deleted,
            result.embedded
        ),
        (0, 0, 0, 0),
        "{result:?}"
    );
    assert_eq!(result.total_documents, 1, "{result:?}");
    assert_eq!(
        events,
        vec![Ev::Started(3), Ev::Scanning(1, 3), Ev::Cancelled(0, 3)]
    );
    assert_eq!(
        fx.requests(),
        requests_before,
        "a run stopped during the scan must not embed anything"
    );
    assert_eq!(
        fx.paths(),
        ["a.md"],
        "the index holds what it held before the run"
    );
}

/// Criterion 8: a token set from the last `Scanning` stops the run before the
/// renames it detected are applied, so neither `Renamed` nor any document
/// event is reported and the moved document keeps its old path. The next run
/// applies the move.
///
/// Where it stops is C1 on the last file: the observer sees the token right
/// after that file's `Scanning`. C2 is the same point reached by a token set
/// from another thread after the observer's last look, and no callback can
/// set a token in that gap, so C2 cannot be told apart from here. What this
/// pins is the property both share: nothing past the scan runs. The first
/// file's `Scanning` stopping the run (C1 mid-scan) is
/// `cancel_during_scan_embeds_nothing`.
#[test]
fn cancel_after_the_last_scanned_file_applies_no_rename() {
    if run_in_hermetic_child("cancel_after_the_last_scanned_file_applies_no_rename") {
        return;
    }
    let fx = Fixture::new("groove-f60-no-rename");
    fx.layout.write("a.md", &doc("Alpha", "alpha body text"));
    fx.layout.write("b.md", &doc("Beta", "beta body text"));
    fx.run(false).0.expect("first run");
    let kb = fx.layout.kb();
    std::fs::rename(kb.join("a.md"), kb.join("moved.md")).expect("move a.md");

    let (result, events) = fx.run_cancelling(false, Box::new(|ev| *ev == Ev::Scanning(2, 2)));
    let result = result.expect("a stopped run returns Ok");
    assert!(result.cancelled, "{result:?}");
    assert_eq!(result.renamed, 0, "{result:?}");
    assert_eq!(
        events,
        vec![
            Ev::Started(2),
            Ev::Scanning(1, 2),
            Ev::Scanning(2, 2),
            Ev::Cancelled(0, 2),
        ]
    );
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, Ev::Renamed(..) | Ev::Indexed(..) | Ev::Unchanged(..))),
        "nothing after the scan may run: {events:?}"
    );
    assert_eq!(fx.paths(), ["a.md", "b.md"], "no rename was applied");

    let (result, events) = fx.run(false);
    let result = result.expect("the next run");
    assert_eq!(result.renamed, 1, "{result:?}");
    assert!(
        events.contains(&Ev::Renamed("a.md".to_string(), "moved.md".to_string())),
        "{events:?}"
    );
    assert_eq!(events.last(), Some(&Ev::Finished), "{events:?}");
}

/// Criterion 14: on an empty knowledge base there is no file to scan, so no
/// `Scanning` and no C1; a token set from `Started { total: 0 }` is still
/// honoured.
///
/// The run stops at C2 -- the first check point after `Started` when there is
/// nothing to scan. From outside, C4 would give the same sequence here (no
/// rename, no document, and C4 also comes before the sweep), so this pins
/// that an empty run can be stopped, not which of the two points did it. C2
/// exists for this case and for a token set from another thread after the
/// last file; neither is reachable in a fixed order from a callback.
#[test]
fn cancel_on_an_empty_kb_returns_cancelled() {
    if run_in_hermetic_child("cancel_on_an_empty_kb_returns_cancelled") {
        return;
    }
    let fx = Fixture::new("groove-f60-empty");

    let (result, events) = fx.run_cancelling(false, Box::new(|ev| *ev == Ev::Started(0)));
    let result = result.expect("a stopped run returns Ok");
    assert!(result.cancelled, "{result:?}");
    assert_eq!(events, vec![Ev::Started(0), Ev::Cancelled(0, 0)]);
}

/// Review Focus 2: a token already set when the run starts. The run still
/// scans one file -- the first check point is after it -- and stops there,
/// having embedded nothing.
#[test]
fn a_token_set_before_the_run_stops_it_after_the_first_scanned_file() {
    if run_in_hermetic_child("a_token_set_before_the_run_stops_it_after_the_first_scanned_file") {
        return;
    }
    let fx = Fixture::new("groove-f60-preset");
    fx.layout.write("a.md", &doc("Alpha", "alpha body text"));
    fx.layout.write("b.md", &doc("Beta", "beta body text"));

    let token = CancelToken::new();
    token.cancel();
    let (log, f) = recorder(token.clone(), Box::new(|_| false));
    let result = fx
        .rebuild(false, ProgressReporter::with_callback(f).with_cancel(token))
        .expect("a stopped run returns Ok");
    let events = log.lock().expect("recorder mutex").clone();
    assert!(result.cancelled, "{result:?}");
    assert_eq!(
        events,
        vec![Ev::Started(2), Ev::Scanning(1, 2), Ev::Cancelled(0, 2)]
    );
    assert_eq!(fx.requests(), 0, "nothing may be embedded");
    assert!(fx.paths().is_empty(), "{:?}", fx.paths());
}

/// Review Focus 3: a token on a reporter that draws nothing -- `Quiet`, the
/// mode the MCP tool uses -- stops the run at the same point.
#[test]
fn a_quiet_reporter_with_a_set_token_stops_without_embedding() {
    if run_in_hermetic_child("a_quiet_reporter_with_a_set_token_stops_without_embedding") {
        return;
    }
    let fx = Fixture::new("groove-f60-quiet");
    fx.layout.write("a.md", &doc("Alpha", "alpha body text"));
    fx.layout.write("b.md", &doc("Beta", "beta body text"));

    let token = CancelToken::new();
    token.cancel();
    let result = fx
        .rebuild(
            false,
            ProgressReporter::new(ProgressMode::Quiet).with_cancel(token),
        )
        .expect("a stopped run returns Ok");
    assert!(result.cancelled, "{result:?}");
    assert_eq!(fx.requests(), 0, "nothing may be embedded");
    assert!(fx.paths().is_empty(), "{:?}", fx.paths());
}

/// Review Focus 5 (C1 with a declined file): the scan stopped after the file
/// it declined for its size, and `skipped` counts that file -- the scan hands
/// back what it had when it stopped. `0-huge.md` sorts first, so it is the
/// only file scanned.
#[test]
fn cancel_mid_scan_counts_the_files_it_declined_as_skipped() {
    if run_in_hermetic_child("cancel_mid_scan_counts_the_files_it_declined_as_skipped") {
        return;
    }
    let fx = Fixture::new("groove-f60-declined");
    fx.layout.write("a.md", &doc("Alpha", "alpha body text"));
    let huge = std::fs::File::create(fx.layout.kb().join("0-huge.md")).expect("create 0-huge.md");
    huge.set_len(grooveseek::parser::MAX_RAW_TEXT_BYTES + 1)
        .expect("grow 0-huge.md past the text cap");
    drop(huge);

    let (result, events) = fx.run_cancelling(false, Box::new(|ev| *ev == Ev::Scanning(1, 2)));
    let result = result.expect("a stopped run returns Ok");
    assert!(result.cancelled, "{result:?}");
    assert_eq!((result.skipped, result.updated), (1, 0), "{result:?}");
    assert_eq!(
        events,
        vec![Ev::Started(2), Ev::Scanning(1, 2), Ev::Cancelled(0, 2)]
    );
}

/// Criterion 6 (C3): a token set from the first document's event stops the
/// run before the second document's embed starts. The first document is
/// committed, and the next run finds it unchanged and does not embed it
/// again. A stopped run records no frontmatter policy; the completing run
/// does.
#[test]
fn cancel_after_a_document_keeps_it_and_the_next_run_skips_it() {
    if run_in_hermetic_child("cancel_after_a_document_keeps_it_and_the_next_run_skips_it") {
        return;
    }
    let fx = Fixture::new("groove-f60-c3");
    fx.layout.write("a.md", &doc("Alpha", "alpha body text"));
    fx.layout.write("b.md", &doc("Beta", "beta body text"));
    fx.layout.write("c.md", &doc("Gamma", "gamma body text"));

    let (result, events) = fx.run_cancelling(
        false,
        Box::new(|ev| matches!(ev, Ev::Indexed(rel, _, _) if rel == "a.md")),
    );
    let result = result.expect("a stopped run returns Ok");
    assert!(result.cancelled, "{result:?}");
    assert_eq!(result.updated, 1, "{result:?}");
    let mut expected = vec![Ev::Started(3)];
    expected.extend(scanning(3));
    expected.extend([indexed("a.md", 1, 3), Ev::Cancelled(1, 3)]);
    assert_eq!(events, expected);
    assert_eq!(fx.paths(), ["a.md"]);
    assert_eq!(
        fx.db().read_frontmatter_policy().expect("policy"),
        None,
        "a stopped run must not record the frontmatter policy"
    );

    let requests_before = fx.requests();
    let (result, events) = fx.run(false);
    let result = result.expect("the next run");
    assert!(!result.cancelled, "{result:?}");
    assert_eq!(result.updated, 2, "{result:?}");
    let mut expected = vec![Ev::Started(3)];
    expected.extend(scanning(3));
    expected.extend([
        unchanged("a.md", 1, 3),
        indexed("b.md", 2, 3),
        indexed("c.md", 3, 3),
        Ev::Finished,
    ]);
    assert_eq!(events, expected);
    assert!(
        fx.mock.requests()[requests_before..].iter().all(|req| req
            .inputs()
            .iter()
            .all(|input| !input.contains("alpha body text"))),
        "a.md was committed by the stopped run and must not be embedded again"
    );
    assert!(
        fx.db().read_frontmatter_policy().expect("policy").is_some(),
        "the completed run records the policy"
    );
}

/// Criterion 7 (C4): a token set from the last document's event is seen after
/// the loop, before the deletion sweep. The vanished file's row stays, no
/// generation key is written, and the pass this run opened for the new schema
/// is closed. The next run sweeps, records the declared set and finishes.
#[test]
fn cancel_after_the_last_document_leaves_deleted_rows_and_generation_keys() {
    if run_in_hermetic_child(
        "cancel_after_the_last_document_leaves_deleted_rows_and_generation_keys",
    ) {
        return;
    }
    let fx = Fixture::new("groove-f60-c4");
    fx.layout.write(
        "a.md",
        "---\ntitle: Alpha\nstatus: active\n---\n\n## Alpha\n\nalpha body text\n",
    );
    fx.layout.write("b.md", &doc("Beta", "beta body text"));
    fx.layout.write("gone.md", &doc("Gone", "gone body text"));
    fx.run(false).0.expect("first run");
    let policy_before = fx.db().read_frontmatter_policy().expect("policy");
    assert!(policy_before.is_some(), "the first run completed");
    assert_eq!(
        fx.db().read_declared_fields().expect("declared").as_deref(),
        Some("[]")
    );

    std::fs::remove_file(fx.layout.kb().join("gone.md")).expect("remove gone.md");
    // A schema change makes this run open a declared-field pass, which clears
    // the generation key before the loop.
    fx.layout.write(
        "groove-schema.toml",
        "[fields.status]\nenum = [\"active\", \"deprecated\"]\n",
    );

    let (result, events) = fx.run_cancelling(
        false,
        Box::new(|ev| {
            matches!(ev, Ev::Indexed(_, done, total) | Ev::Unchanged(_, done, total) if done == total)
        }),
    );
    let result = result.expect("a stopped run returns Ok");
    assert!(result.cancelled, "{result:?}");
    assert_eq!(result.deleted, 0, "{result:?}");
    let mut expected = vec![Ev::Started(2)];
    expected.extend(scanning(2));
    expected.extend([
        unchanged("a.md", 1, 2),
        unchanged("b.md", 2, 2),
        Ev::Cancelled(2, 2),
    ]);
    assert_eq!(events, expected, "no Deleted before Cancelled");
    assert_eq!(
        fx.paths(),
        ["a.md", "b.md", "gone.md"],
        "the vanished file's row waits for a completed run"
    );
    let db = fx.db();
    assert_eq!(
        db.read_frontmatter_policy().expect("policy"),
        policy_before,
        "the policy key is left as it was"
    );
    assert_eq!(
        db.read_declared_fields().expect("declared"),
        None,
        "the pass cleared the key and a stopped run does not record it"
    );
    assert_eq!(
        db.read_declared_fields_pass().expect("pass"),
        None,
        "a stopped run leaves no pass token of its own"
    );
    drop(db);

    let (result, events) = fx.run(false);
    let result = result.expect("the next run");
    assert_eq!(result.deleted, 1, "{result:?}");
    assert!(
        events.contains(&Ev::Deleted("gone.md".to_string())),
        "{events:?}"
    );
    assert_eq!(events.last(), Some(&Ev::Finished), "{events:?}");
    let db = fx.db();
    assert_eq!(
        db.read_declared_fields().expect("declared").as_deref(),
        Some("[\"status\"]")
    );
    assert_eq!(db.read_declared_fields_pass().expect("pass"), None);
}

/// Criterion 9: once the deletion sweep has begun a token is too late. The
/// run completes, reports `Finished`, and returns with `cancelled == false`.
#[test]
fn cancel_during_the_deletion_sweep_is_too_late_and_the_run_finishes() {
    if run_in_hermetic_child("cancel_during_the_deletion_sweep_is_too_late_and_the_run_finishes") {
        return;
    }
    let fx = Fixture::new("groove-f60-late");
    fx.layout.write("a.md", &doc("Alpha", "alpha body text"));
    fx.layout.write("b.md", &doc("Beta", "beta body text"));
    fx.layout.write("c.md", &doc("Gamma", "gamma body text"));
    fx.run(false).0.expect("first run");
    let kb = fx.layout.kb();
    std::fs::remove_file(kb.join("b.md")).expect("remove b.md");
    std::fs::remove_file(kb.join("c.md")).expect("remove c.md");

    let (result, events) = fx.run_cancelling(false, Box::new(|ev| matches!(ev, Ev::Deleted(_))));
    let result = result.expect("the run");
    assert!(!result.cancelled, "{result:?}");
    assert_eq!(result.deleted, 2, "{result:?}");
    assert_eq!(
        events,
        vec![
            Ev::Started(1),
            Ev::Scanning(1, 1),
            unchanged("a.md", 1, 1),
            Ev::Deleted("b.md".to_string()),
            Ev::Deleted("c.md".to_string()),
            Ev::Finished,
        ]
    );
    assert_eq!(fx.paths(), ["a.md"]);
}

/// Criterion 13, first half: a `force` run stopped after its first document
/// leaves only that document in the index (the reset came before it), and
/// the incremental run after it adds the rest as `Indexed`.
#[test]
fn cancel_mid_force_keeps_committed_documents_and_an_incremental_run_completes() {
    if run_in_hermetic_child(
        "cancel_mid_force_keeps_committed_documents_and_an_incremental_run_completes",
    ) {
        return;
    }
    let fx = Fixture::new("groove-f60-force");
    fx.layout.write("a.md", &doc("Alpha", "alpha body text"));
    fx.layout.write("b.md", &doc("Beta", "beta body text"));
    fx.layout.write("c.md", &doc("Gamma", "gamma body text"));
    fx.run(false).0.expect("first run");

    let (result, events) = fx.run_cancelling(
        true,
        Box::new(|ev| matches!(ev, Ev::Indexed(rel, _, _) if rel == "a.md")),
    );
    let result = result.expect("a stopped run returns Ok");
    assert!(result.cancelled, "{result:?}");
    let mut expected = vec![Ev::Started(3)];
    expected.extend(scanning(3));
    expected.extend([indexed("a.md", 1, 3), Ev::Cancelled(1, 3)]);
    assert_eq!(events, expected);
    assert_eq!(
        fx.paths(),
        ["a.md"],
        "only what was committed before the stop"
    );

    let (result, events) = fx.run(false);
    let result = result.expect("the incremental run");
    assert!(!result.cancelled, "{result:?}");
    let mut expected = vec![Ev::Started(3)];
    expected.extend(scanning(3));
    expected.extend([
        unchanged("a.md", 1, 3),
        indexed("b.md", 2, 3),
        indexed("c.md", 3, 3),
        Ev::Finished,
    ]);
    assert_eq!(events, expected);
    assert_eq!(fx.paths(), ["a.md", "b.md", "c.md"]);
}

/// Criterion 13, second half: with no schema, a stopped `force` run still
/// closes the pass it opened, so the next incremental run -- which opens no
/// pass of its own -- records the empty declared set and the index recovers
/// without another `--force`.
#[test]
fn cancel_mid_force_without_a_schema_leaves_no_pass_token() {
    if run_in_hermetic_child("cancel_mid_force_without_a_schema_leaves_no_pass_token") {
        return;
    }
    let fx = Fixture::new("groove-f60-force-noschema");
    fx.layout.write("a.md", &doc("Alpha", "alpha body text"));
    fx.layout.write("b.md", &doc("Beta", "beta body text"));
    fx.run(false).0.expect("first run");

    let (result, _events) = fx.run_cancelling(
        true,
        Box::new(|ev| matches!(ev, Ev::Indexed(rel, _, _) if rel == "a.md")),
    );
    assert!(result.expect("a stopped run returns Ok").cancelled);
    let db = fx.db();
    assert_eq!(
        db.read_declared_fields_pass().expect("pass"),
        None,
        "the stopped run must close its own pass"
    );
    assert_eq!(db.read_declared_fields().expect("declared"), None);
    drop(db);

    let (result, events) = fx.run(false);
    assert!(!result.expect("the incremental run").cancelled);
    assert_eq!(events.last(), Some(&Ev::Finished), "{events:?}");
    let db = fx.db();
    assert_eq!(
        db.read_declared_fields().expect("declared").as_deref(),
        Some("[]"),
        "the incremental run records the empty declared set"
    );
    assert_eq!(db.read_declared_fields_pass().expect("pass"), None);
}

/// Criterion 15: a `.txt` to `.md` rename forces a re-parse under the `.md`
/// parser. A token set from `Renamed` does not stop the run before that
/// re-parse: the renamed document comes first -- ahead of `a-new.md`, which
/// the walk puts before it -- is re-parsed as `Indexed`, and only then does
/// the run stop, leaving the new document for the next run.
#[test]
fn cancel_after_a_cross_parser_rename_reparses_it_first() {
    if run_in_hermetic_child("cancel_after_a_cross_parser_rename_reparses_it_first") {
        return;
    }
    let fx = Fixture::with_extra_config(
        "groove-f60-xparser",
        "[parsers]\nenabled = [\"md\", \"txt\"]\n",
    );
    fx.layout.write("note.txt", &doc("Note", "note body text"));
    fx.run(false).0.expect("first run");
    let kb = fx.layout.kb();
    std::fs::rename(kb.join("note.txt"), kb.join("note.md")).expect("rename note.txt");
    fx.layout.write("a-new.md", &doc("New", "new body text"));

    let (result, events) = fx.run_cancelling(false, Box::new(|ev| matches!(ev, Ev::Renamed(..))));
    let result = result.expect("a stopped run returns Ok");
    assert!(result.cancelled, "{result:?}");
    assert_eq!((result.renamed, result.updated), (1, 1), "{result:?}");
    let mut expected = vec![Ev::Started(2)];
    expected.extend(scanning(2));
    expected.extend([
        Ev::Renamed("note.txt".to_string(), "note.md".to_string()),
        indexed("note.md", 1, 2),
        Ev::Cancelled(1, 2),
    ]);
    assert_eq!(events, expected);
    assert_eq!(
        fx.paths(),
        ["note.md"],
        "the new document was not processed"
    );

    let (result, events) = fx.run(false);
    assert!(!result.expect("the next run").cancelled);
    let mut expected = vec![Ev::Started(2)];
    expected.extend(scanning(2));
    expected.extend([
        indexed("a-new.md", 1, 2),
        unchanged("note.md", 2, 2),
        Ev::Finished,
    ]);
    assert_eq!(events, expected, "no rename this time, so the walk order");
}
