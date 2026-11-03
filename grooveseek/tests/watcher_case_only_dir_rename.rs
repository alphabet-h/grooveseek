//! (audit AX-14, kuriya 610) A directory renamed so that only the case of its name changes
//! (`Docs/` to `docs/`), with the watcher running.
//!
//! On a case-insensitive volume (Windows NTFS, macOS APFS as the CI runners format it: the
//! macOS-only `a_case_only_rename_leaves_one_row` of the watcher row-survival integration tests
//! renames `Old.md` to `old.md` there) the old spelling still opens the renamed directory, so
//! this is the case where the source of a directory rename looks present and the destination is
//! the same directory. The rows are keyed by their byte spelling
//! ([`grooveseek::db::Database::document_paths_under`] lists a BINARY range), so every one of
//! them has to leave `Docs/` and land under `docs/`.
//!
//! The path each OS takes to that end state:
//!
//! - Windows: observed (see the test's doc comment): the `Remove` of `Docs` sweeps its rows, and
//!   `docs` arrives as a new directory that is scanned.
//! - macOS: estimated from reading the code, not observed. On a case-insensitive APFS `Docs`
//!   still stats, so notify-debouncer-full 0.6.0 takes the `Name(Any)` records down its
//!   rename-to branch (`handle_rename_to`) and does not pair them; both `Docs` and `docs` reach
//!   groove as arrived directories, and the arrival of `Docs`, a directory not on disk under that
//!   spelling, drops the rows under it (`ArrivedDirectory::Dropped` in [`grooveseek::watcher`],
//!   a subtree deindex), while `docs` is scanned. The end state is expected to match Windows; the
//!   CI macOS leg checks it.
//! - Linux (case-sensitive): estimated, not observed. `Docs` and `docs` are two names, inotify
//!   pairs the rename through its cookie, and it is an ordinary paired directory rename that moves
//!   the rows. The expected end state is the same, so the test runs there too and pins that the
//!   two spellings are never folded together.
//!
//! Drives `groove serve` with the watcher on against the mock endpoint of
//! [`crate::common::embed_cli`], so nothing downloads a model, and reads the rows the server left
//! straight from its index (the same harness as the watcher row-survival integration tests).

mod common;

use common::ansi::strip_ansi;
use common::embed_cli::fixture;
use common::embed_mock::hermetic;
use common::mcp::{ServerGuard, spawn_serve_with};
use common::rename::rename_retrying;
use grooveseek::db::Database;
use std::collections::BTreeMap;
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// How long the test waits for the watcher to reach the rows it expects.
const SETTLE: Duration = Duration::from_secs(30);
/// How long the test keeps looking after the rows were reached, to see that they stay: ten
/// debounce windows of the `debounce_ms = 300` the fixture sets.
const HOLD: Duration = Duration::from_secs(3);
/// How long a look that only needs one successful read (before the rename, after [`HOLD`])
/// keeps trying.
const READ_BUDGET: Duration = Duration::from_secs(10);
/// The pause between two reads of a poll.
const POLL: Duration = Duration::from_millis(100);

/// A Markdown note titled `title` with one section holding `marker`.
fn note(title: &str, marker: &str) -> String {
    format!(
        "---\ntitle: {title}\n---\n\n## {title}\n\nThis note carries the marker {marker} in its only section.\n"
    )
}

/// Every document path in the index, sorted.
fn paths(db: &Database) -> Result<Vec<String>, String> {
    let mut all = db
        .all_document_paths()
        .map_err(|e| format!("listing the paths failed: {e:#}"))?;
    all.sort();
    Ok(all)
}

/// The chunk texts stored for `rel`, joined (empty when there is no such row).
fn text_of(db: &Database, rel: &str) -> Result<String, String> {
    Ok(db
        .chunk_texts_for_path(rel)
        .map_err(|e| format!("reading the chunks of {rel} failed: {e:#}"))?
        .into_iter()
        .map(|(_, content)| content)
        .collect::<Vec<_>>()
        .join("\n"))
}

/// What one read of the index saw: every path, and the text stored for each path asked for.
#[derive(Debug)]
struct Snapshot {
    paths: Vec<String>,
    texts: BTreeMap<String, String>,
}

/// One read of the index: [`paths`], then [`text_of`] for each of `rels`.
fn snapshot(db: &Database, rels: &[String]) -> Result<Snapshot, String> {
    let paths = paths(db)?;
    let mut texts = BTreeMap::new();
    for rel in rels {
        texts.insert(rel.clone(), text_of(db, rel)?);
    }
    Ok(Snapshot { paths, texts })
}

/// The index the server writes, read beside it from a thread of its own, so that no read can
/// hold the test past its deadline.
///
/// The one connection is opened before the rename, as a reader (`query_only`, the way the
/// server's own readers open it while it holds the writer lease, ADR-0031: with the schema
/// current no lease is asked for and nothing is written) and kept. Each read is a few
/// autocommit `SELECT`s whose statements are finalized before the read returns, so between
/// reads the connection holds no read transaction: it does not pin the WAL or hold up the
/// server's checkpoints, and every read sees what the server committed before it began. The
/// connection's busy timeout (30 s) is not the bound; [`IndexReader::read_by`] waits for an
/// answer only until the caller's deadline and reports a read still running then as a failure.
struct IndexReader {
    /// `None` once dropped, which ends the thread's loop.
    ask: Option<mpsc::Sender<()>>,
    answers: mpsc::Receiver<Result<Snapshot, String>>,
    /// The reader thread, which owns the connection; `None` once joined.
    thread: Option<std::thread::JoinHandle<()>>,
}

impl IndexReader {
    /// Move `db` to a reader thread that reads `rels` each time it is asked.
    fn spawn(db: Database, rels: &[&str]) -> Self {
        let rels: Vec<String> = rels.iter().map(|r| r.to_string()).collect();
        let (ask, asked) = mpsc::channel::<()>();
        let (answer, answers) = mpsc::channel();
        let thread = std::thread::spawn(move || {
            while asked.recv().is_ok() {
                if answer.send(snapshot(&db, &rels)).is_err() {
                    break;
                }
            }
            // `db` is dropped here, when the thread ends: [`IndexReader`]'s `Drop` joins it,
            // so the connection is closed before the fixture deletes the index's directory,
            // which Windows refuses while a handle is open.
        });
        Self {
            ask: Some(ask),
            answers,
            thread: Some(thread),
        }
    }

    /// One read, answered before `end` or reported as failed. A read that times out ends the
    /// poll it belongs to ([`poll_until`] stops at the same `end`), so its late answer is never
    /// taken for a later read's.
    fn read_by(&self, end: Instant) -> Result<Snapshot, String> {
        let left = end.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err("no time was left for a read".to_string());
        }
        self.ask
            .as_ref()
            .ok_or_else(|| "the reader was closed".to_string())?
            .send(())
            .map_err(|_| "the reader thread is gone".to_string())?;
        match self.answers.recv_timeout(left) {
            Ok(read) => read,
            Err(mpsc::RecvTimeoutError::Timeout) => Err(format!(
                "a read of the index did not return within the {left:?} left"
            )),
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                Err("the reader thread is gone".to_string())
            }
        }
    }
}

impl Drop for IndexReader {
    /// End the thread's loop and join it, so the connection is closed when this returns.
    ///
    /// The join has no time limit: a read still running finishes first, which at worst is the
    /// sum of its queries' 30 s busy timeouts. A slow teardown is preferred to a detached
    /// thread whose handle keeps the fixture's temp directory from being deleted on Windows.
    /// A panic of the thread is only reported here, never raised again (this may run while the
    /// test is already unwinding).
    fn drop(&mut self) {
        self.ask = None;
        if let Some(thread) = self.thread.take()
            && let Err(panic) = thread.join()
        {
            eprintln!("the index reader thread panicked: {panic:?}");
        }
    }
}

/// Read until a read is accepted, within `budget`. The time left is looked at before each
/// read and bounds the read itself. A failed read counts as "not yet"; when the budget runs
/// out, the error names the last outcome (the snapshot it held, or why it failed), so a read
/// that never succeeds is a red test with its reason rather than a silent pass.
fn poll_until(
    reader: &IndexReader,
    budget: Duration,
    accept: impl Fn(&Snapshot) -> bool,
) -> Result<Snapshot, String> {
    let end = Instant::now() + budget;
    let mut last = "no read was made".to_string();
    loop {
        if Instant::now() >= end {
            return Err(last);
        }
        match reader.read_by(end) {
            Ok(seen) if accept(&seen) => return Ok(seen),
            Ok(seen) => last = format!("the last read held {seen:?}"),
            Err(e) => last = format!("the last read failed: {e}"),
        }
        std::thread::sleep(POLL.min(end.saturating_duration_since(Instant::now())));
    }
}

fn stderr_text(guard: &ServerGuard) -> String {
    guard
        .stderr()
        .lines()
        .iter()
        .map(|l| strip_ansi(l))
        .collect::<Vec<_>>()
        .join("\n")
}

/// (AX-14) Renaming the indexed `Docs/` to `docs/` moves every row under it, the nested one
/// included, to the new spelling; no row stays under `Docs/`, and the sibling whose name shares
/// the prefix without the `/` is not touched.
///
/// Observed on Windows 11 (local, 2026-11-03, four runs alike): the rename does not reach the
/// watcher as a paired directory rename. The `Remove` of `Docs` comes first, the look at its
/// spelling finds it gone (the disk holds only `docs`), and the rows under `Docs/` are swept
/// (`deindexed 2 documents under Docs/`); then `docs` arrives as a new directory and its two
/// files are read and embedded again. Only the end state is asserted, so the paired path, which
/// Linux takes and the unit test `test_dir_rename_steps_case_only_rename_moves_every_row` pins,
/// passes it as well. A spelling look that ignored case (the old name opens the new directory)
/// leaves the `Docs/` rows beside the `docs/` ones, and this test goes red.
#[test]
fn watcher_follows_a_case_only_directory_rename() {
    let fx = fixture(
        "ax14-case-only-dir",
        &[
            ("Docs/a.md", &note("Alpha", "alphamarker")),
            ("Docs/deep/b.md", &note("Bravo", "bravomarker")),
            ("Docs-other/c.md", &note("Charlie", "charliemarker")),
        ],
        "\n[watch]\nenabled = true\ndebounce_ms = 300\n",
    );
    fx.index();
    // `spawn_serve_with(.., true, ..)` returns only after `watcher: watching`, so the rename
    // cannot fall before the debouncer is armed.
    let (guard, _base) = spawn_serve_with(fx.kb(), &fx.config, true, |c| {
        hermetic(c, &fx.cache);
    });
    let want = ["Docs-other/c.md", "docs/a.md", "docs/deep/b.md"];
    let db_path = grooveseek::resolve_db_path(fx.kb());
    let db = Database::open_read_only(&db_path.to_string_lossy(), fx.kb())
        .unwrap_or_else(|e| panic!("open the index as a reader: {e:#}"));
    // Declared after `fx` and `guard`, so it is dropped before them, unwinding included (locals
    // drop in reverse order): the connection is closed before the fixture deletes its directory.
    let reader = IndexReader::spawn(db, &want);

    let before = poll_until(&reader, READ_BUDGET, |_| true).unwrap_or_else(|last| {
        panic!(
            "the index could not be read before the rename: {last}\nstderr:\n{}",
            stderr_text(&guard)
        )
    });
    assert_eq!(
        before.paths,
        ["Docs-other/c.md", "Docs/a.md", "Docs/deep/b.md"]
    );

    rename_retrying(&fx.kb().join("Docs"), &fx.kb().join("docs"));

    let settled = |s: &Snapshot| {
        let text = |rel: &str| s.texts.get(rel).map(String::as_str).unwrap_or("");
        s.paths == want
            && text("docs/a.md").contains("alphamarker")
            && text("docs/deep/b.md").contains("bravomarker")
            && text("Docs-other/c.md").contains("charliemarker")
    };
    if let Err(last) = poll_until(&reader, SETTLE, settled) {
        panic!(
            "case-only directory rename: the index never held exactly {want:?} within {SETTLE:?}; {last}\nstderr:\n{}",
            stderr_text(&guard)
        );
    }
    std::thread::sleep(HOLD);
    let later = poll_until(&reader, READ_BUDGET, |_| true).unwrap_or_else(|last| {
        panic!(
            "case-only directory rename: the index held {want:?}, then could not be read again: {last}\nstderr:\n{}",
            stderr_text(&guard)
        )
    });
    assert!(
        settled(&later),
        "case-only directory rename: the index held {want:?}, then {later:?}\nstderr:\n{}",
        stderr_text(&guard)
    );
    drop(reader);
    drop(guard);
}
