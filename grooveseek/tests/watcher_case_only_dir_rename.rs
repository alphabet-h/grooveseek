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
use common::embed_cli::{Fixture, fixture};
use common::embed_mock::{hermetic, wait_until};
use common::mcp::{ServerGuard, spawn_serve_with};
use common::rename::rename_retrying;
use grooveseek::db::Database;
use std::time::Duration;

/// How long the test waits for the watcher to reach the rows it expects.
const SETTLE: Duration = Duration::from_secs(30);
/// How long the test keeps looking after the rows were reached, to see that they stay: ten
/// debounce windows of the `debounce_ms = 300` the fixture sets.
const HOLD: Duration = Duration::from_secs(3);

/// A Markdown note titled `title` with one section holding `marker`.
fn note(title: &str, marker: &str) -> String {
    format!(
        "---\ntitle: {title}\n---\n\n## {title}\n\nThis note carries the marker {marker} in its only section.\n"
    )
}

/// The index the server writes, opened beside it as a reader (`query_only`), the way the
/// server's own readers open it while it holds the writer lease (ADR-0031): the schema is
/// current once `groove index` ran, so no lease is asked for and nothing is written, and in
/// WAL a reader does not wait on the writer. Closed again when the caller drops it.
fn open_index(fx: &Fixture) -> Database {
    let db_path = grooveseek::resolve_db_path(fx.kb());
    Database::open_read_only(&db_path.to_string_lossy(), fx.kb()).expect("open the index")
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
    assert_eq!(
        paths(&fx),
        ["Docs-other/c.md", "Docs/a.md", "Docs/deep/b.md"]
    );

    rename_retrying(&fx.kb().join("Docs"), &fx.kb().join("docs"));

    let want = ["Docs-other/c.md", "docs/a.md", "docs/deep/b.md"];
    let holds = |fx: &Fixture| {
        text_of(fx, "docs/a.md").contains("alphamarker")
            && text_of(fx, "docs/deep/b.md").contains("bravomarker")
            && text_of(fx, "Docs-other/c.md").contains("charliemarker")
    };
    let mut last = Vec::new();
    let reached = wait_until(SETTLE, || {
        last = paths(&fx);
        last == want && holds(&fx)
    });
    assert!(
        reached,
        "case-only directory rename: the index never held exactly {want:?}; last seen {last:?}\nstderr:\n{}",
        stderr_text(&guard)
    );
    std::thread::sleep(HOLD);
    let later = paths(&fx);
    assert!(
        later == want && holds(&fx),
        "case-only directory rename: the index held {want:?}, then {later:?}\nstderr:\n{}",
        stderr_text(&guard)
    );
    drop(guard);
}
