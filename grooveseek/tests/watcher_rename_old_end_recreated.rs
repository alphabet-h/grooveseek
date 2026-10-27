//! (kuriya 560) A rename does not look at its old end, and a file that turns up there again is
//! indexed all the same.
//!
//! When the watcher handles a paired rename it moves or drops the row of the old end without
//! looking at that path on disk. A file found there by then is another file: the one that left
//! took its text along. Its own create event is queued apart from the rename (notify's debouncer
//! moves the source's queue to the destination, so a later create at the old path starts a new
//! queue) and comes out after it in time order, so that event writes its row. Nothing in the
//! code says so beyond that reading; these tests pin it, for a file rename, for a rename into
//! an excluded directory and for a directory rename.
//!
//! Each test drives `groove serve` with the watcher on against the mock endpoint of
//! [`crate::common::embed_cli`], so nothing downloads a model, and reads the rows the server left
//! straight from its index.
//!
//! Not on macOS: FSEvents does not say which end of a rename a path is, and with both ends on
//! disk the debouncer never pairs the rename (kuriya trap 525). The old end then arrives as a
//! removal on its own, which looks at the path; the paths these tests pin are not taken.
#![cfg(not(target_os = "macos"))]

mod common;

use common::ansi::strip_ansi;
use common::embed_cli::{Fixture, fixture};
use common::embed_mock::{hermetic, wait_until};
use common::mcp::{ServerGuard, spawn_serve_with};
use common::rename::rename_retrying;
use grooveseek::db::Database;
use std::time::Duration;

/// How long a test waits for the watcher to reach the rows it expects.
const SETTLE: Duration = Duration::from_secs(30);
/// How long a test keeps looking after the rows were reached, to see that they stay: ten
/// debounce windows of the `debounce_ms = 300` the fixture sets.
const HOLD: Duration = Duration::from_secs(3);

/// A Markdown note titled `title` with one section holding `marker`.
fn note(title: &str, marker: &str) -> String {
    format!(
        "---\ntitle: {title}\n---\n\n## {title}\n\nThis note carries the marker {marker} in its only section.\n"
    )
}

/// A knowledge base holding `files`, indexed once, served with the watcher on.
fn served(prefix: &str, files: &[(&str, &str)]) -> (Fixture, ServerGuard) {
    let fx = fixture(
        prefix,
        files,
        "\n[watch]\nenabled = true\ndebounce_ms = 300\n",
    );
    fx.index();
    // `spawn_serve_with(.., true, ..)` returns only after `watcher: watching`, so the edits a
    // test makes next cannot fall before the debouncer is armed.
    let (guard, _base) = spawn_serve_with(fx.kb(), &fx.config, true, |c| {
        hermetic(c, &fx.cache);
    });
    (fx, guard)
}

/// The index the server writes, opened beside it (WAL, with a busy timeout), and closed again
/// when the caller drops it.
fn open_index(fx: &Fixture) -> Database {
    let db_path = grooveseek::resolve_db_path(fx.kb());
    Database::open(&db_path.to_string_lossy()).expect("open the index")
}

/// Every document path in the index, sorted.
fn paths(fx: &Fixture) -> Vec<String> {
    let mut all = open_index(fx).all_document_paths().expect("paths");
    all.sort();
    all
}

/// The chunk texts stored for `rel`, joined.
fn text_of(fx: &Fixture, rel: &str) -> String {
    open_index(fx)
        .chunk_texts_for_path(rel)
        .expect("chunk texts")
        .into_iter()
        .map(|(_, content)| content)
        .collect::<Vec<_>>()
        .join("\n")
}

fn stderr_lines(guard: &ServerGuard) -> Vec<String> {
    guard
        .stderr()
        .lines()
        .iter()
        .map(|l| strip_ansi(l))
        .collect()
}

/// Wait until the index holds exactly `want` and `holds` accepts it; the last paths seen.
fn wait_for_paths(
    fx: &Fixture,
    want: &[&str],
    mut holds: impl FnMut(&Fixture) -> bool,
) -> (bool, Vec<String>) {
    let mut last = Vec::new();
    let reached = wait_until(SETTLE, || {
        last = paths(fx);
        last == want && holds(fx)
    });
    (reached, last)
}

/// Require the index to reach exactly `want` (with `holds` true of it) and to keep it for
/// [`HOLD`], checked over and over through it, so a row that goes stale for a moment and is put
/// back by a later event fails too.
fn assert_settles_on(
    fx: &Fixture,
    guard: &ServerGuard,
    want: &[&str],
    what: &str,
    mut holds: impl FnMut(&Fixture) -> bool,
) {
    let (reached, last) = wait_for_paths(fx, want, &mut holds);
    assert!(
        reached,
        "{what}: the index never held exactly {want:?}; last seen {last:?}\nstderr:\n{}",
        stderr_lines(guard).join("\n")
    );
    let until = std::time::Instant::now() + HOLD;
    while std::time::Instant::now() < until {
        let later = paths(fx);
        assert!(
            later == want && holds(fx),
            "{what}: the index held {want:?}, then {later:?}\nstderr:\n{}",
            stderr_lines(guard).join("\n")
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// (E1) A file renamed and then written again under its old name, inside one debounce window:
/// both names end up indexed, each with its own text.
///
/// The paired rename moves the row of `a.md` (the text of the file that left) to `b.md` without
/// looking at `a.md`. The new `a.md` has its own create event, queued after the rename, which
/// writes its row. Pins the old end of the rows N1, N2 and N7 of the table in kuriya 560 (type E).
///
/// The paired path is asserted through its stderr line: green through an unpaired removal of
/// `a.md` would pin nothing about the rename. What is pinned is that the end state is right
/// while the old end is not looked at, not that it must never be: a look added there later would
/// keep this green, and would not be wrong.
#[test]
fn a_file_renamed_then_written_again_under_its_old_name_keeps_both() {
    let (fx, guard) = served("i560-file", &[("a.md", &note("Alpha", "alphamarker"))]);
    assert_eq!(paths(&fx), ["a.md"]);

    std::fs::rename(fx.kb().join("a.md"), fx.kb().join("b.md")).expect("rename a.md to b.md");
    fx.layout.write("a.md", &note("Gamma", "gammamarker"));

    assert_settles_on(
        &fx,
        &guard,
        &["a.md", "b.md"],
        "file renamed, then written again under its old name",
        |fx| {
            let a = text_of(fx, "a.md");
            let b = text_of(fx, "b.md");
            a.contains("gammamarker")
                && !a.contains("alphamarker")
                && b.contains("alphamarker")
                && !b.contains("gammamarker")
        },
    );
    let lines = stderr_lines(&guard);
    assert!(
        lines
            .iter()
            .any(|l| l.contains("watcher: renamed") && l.contains("a.md -> b.md")),
        "the end state was reached without the paired rename path; stderr:\n{}",
        lines.join("\n")
    );
    drop(guard);
}

/// (E2) A file moved into an excluded directory and then written again under its old name: the
/// old name is indexed with the new text, and nothing under the excluded directory is.
///
/// `node_modules` is on the watcher's hardcoded denylist, so the paired rename drops the row of
/// `a.md` without looking at it. The new `a.md` has its own create event, which writes its row.
/// Pins the row N1 of the table in kuriya 560 (type E). `keep.txt` is there only so that the
/// directory exists before the server starts; its extension keeps it out of the index.
///
/// On Linux the test also asserts the line of the dropped row. That line is printed by the paired
/// rename and by an unpaired rename-from event alike, and neither looks at the old name, so it
/// pins that the old name went through a drop that does not look; it does not tell which of the
/// two it was. On Windows the watcher printed no such line when this was written: the stderr held
/// only the reindex of `a.md`, so the old name went through the removal that looks at the path.
/// The reading (not checked) is that Windows reports a move between directories as a removal and
/// an addition rather than as the two names of a rename. The end state is the same, and is
/// asserted on both.
#[test]
fn a_file_moved_into_an_excluded_directory_then_written_again_is_indexed() {
    let (fx, guard) = served(
        "i560-excluded",
        &[
            ("a.md", &note("Alpha", "alphamarker")),
            ("node_modules/keep.txt", "kept only to make the directory"),
        ],
    );
    assert_eq!(paths(&fx), ["a.md"]);

    std::fs::rename(
        fx.kb().join("a.md"),
        fx.kb().join("node_modules").join("a.md"),
    )
    .expect("rename a.md into node_modules");
    fx.layout.write("a.md", &note("Gamma", "gammamarker"));

    assert_settles_on(
        &fx,
        &guard,
        &["a.md"],
        "file moved into an excluded directory, then written again",
        |fx| {
            let a = text_of(fx, "a.md");
            a.contains("gammamarker") && !a.contains("alphamarker")
        },
    );
    #[cfg(target_os = "linux")]
    {
        let lines = stderr_lines(&guard);
        assert!(
            lines.iter().any(|l| l.contains("watcher: deindexed a.md")),
            "the end state was reached without a drop of a.md that does not look; stderr:\n{}",
            lines.join("\n")
        );
    }
    drop(guard);
}

/// (E3) A directory renamed and then created again with a regular file in it: the moved rows
/// stay under the new name, and the file in the recreated directory is indexed.
///
/// The directory rename moves the listed rows off the old prefix without reading the files now
/// under it. The directory itself is statted before any sweep of the old prefix, so a recreated
/// directory is not swept. The recreated `docs/a.md` is another file; the arrival scan of `docs` or its own create writes
/// its row. Pins the rows D1, D3 and D6 of the table in kuriya 560 (type E). The watcher end-to-end
/// test that recreates the source with a hard link checks only that the old rows go; this one
/// checks that a regular file there comes back.
///
/// The directory-rename path is asserted through its stderr line, as in that test.
#[test]
fn a_directory_renamed_then_created_again_with_a_file_indexes_the_new_file() {
    let (fx, guard) = served(
        "i560-dir",
        &[
            ("docs/a.md", &note("Alpha", "alphamarker")),
            ("docs/b.md", &note("Bravo", "bravomarker")),
        ],
    );
    assert_eq!(paths(&fx), ["docs/a.md", "docs/b.md"]);

    let docs = fx.kb().join("docs");
    let notes = fx.kb().join("notes");
    rename_retrying(&docs, &notes);
    std::fs::create_dir(&docs).expect("create docs again");
    fx.layout.write("docs/a.md", &note("Gamma", "gammamarker"));

    assert_settles_on(
        &fx,
        &guard,
        &["docs/a.md", "notes/a.md", "notes/b.md"],
        "directory renamed, then created again with a file",
        |fx| {
            let docs_a = text_of(fx, "docs/a.md");
            let notes_a = text_of(fx, "notes/a.md");
            docs_a.contains("gammamarker")
                && !docs_a.contains("alphamarker")
                && notes_a.contains("alphamarker")
                && !notes_a.contains("gammamarker")
                && text_of(fx, "notes/b.md").contains("bravomarker")
        },
    );
    let lines = stderr_lines(&guard);
    assert!(
        lines
            .iter()
            .any(|l| l.contains("watcher: a renamed directory held 2 indexed documents")),
        "the end state was reached without the directory-rename path; stderr:\n{}",
        lines.join("\n")
    );
    drop(guard);
}
