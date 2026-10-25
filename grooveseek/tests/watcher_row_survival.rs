//! Issue #544 (PR-1): the live watcher keeps, writes or moves a document's row only for what is
//! on disk when the event is handled.
//!
//! These drive `groove serve` with the watcher on against the mock endpoint of
//! [`crate::common::embed_cli`], so nothing downloads a model, and read the rows the server left
//! straight from its index. They were added before the fix, to record what each OS did then.
//!
//! | test, or the scene it drives | spec id | OS |
//! |---|---|---|
//! | [`crate::an_overwriting_rename_moves_the_source_row_onto_the_destination`] | P1-E1 | all |
//! | [`crate::a_file_replaced_by_a_directory_loses_its_row`] | P1-E5 | all |
//! | a rename that reaches the watcher as two unpaired events | P1-E3 | macOS |
//! | a rename that changes only the case of a name | P1-E4 | macOS |
//! | a Japanese file name through an edit and a rename | P1-E6 | macOS |
//! | a directory with a Japanese name that arrives while the server runs | P1-E6 (1) | macOS |
//! | a directory renamed onto a Japanese name | P1-E6 (2) | macOS |
//!
//! The tests of the macOS rows are compiled on macOS only, so a link to them would not resolve
//! elsewhere; each row names its scene, and the test's doc comment carries the same spec id.
//! Every name is written as a Rust literal, which is NFC.

mod common;

use common::ansi::strip_ansi;
use common::embed_cli::{Fixture, fixture};
use common::embed_mock::{hermetic, wait_until};
use common::mcp::{ServerGuard, spawn_serve_with};
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
/// [`HOLD`].
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
    std::thread::sleep(HOLD);
    let later = paths(fx);
    assert!(
        later == want && holds(fx),
        "{what}: the index held {want:?}, then {later:?}\nstderr:\n{}",
        stderr_lines(guard).join("\n")
    );
}

/// (P1-E1, spec C1) Renaming `b.md` over an indexed `a.md` leaves one row, `a.md`, holding what
/// `b.md` said. Before the fix the move of the row ran into the destination's row on the
/// `documents.path` unique key and both rows stayed until a full index.
#[test]
fn an_overwriting_rename_moves_the_source_row_onto_the_destination() {
    let (fx, guard) = served(
        "i544-overwrite",
        &[
            ("a.md", &note("Alpha", "alphamarker")),
            ("b.md", &note("Bravo", "bravomarker")),
        ],
    );
    assert_eq!(paths(&fx), ["a.md", "b.md"]);

    std::fs::rename(fx.kb().join("b.md"), fx.kb().join("a.md")).expect("rename b.md over a.md");

    assert_settles_on(&fx, &guard, &["a.md"], "overwriting rename", |fx| {
        let text = text_of(fx, "a.md");
        text.contains("bravomarker") && !text.contains("alphamarker")
    });
    drop(guard);
}

/// (P1-E5, spec A2) A file removed and replaced by a directory of the same name: the file's row
/// goes and the file in the directory is indexed.
#[test]
fn a_file_replaced_by_a_directory_loses_its_row() {
    let (fx, guard) = served(
        "i544-file-to-dir",
        &[("a.md", &note("Alpha", "alphamarker"))],
    );
    assert_eq!(paths(&fx), ["a.md"]);

    std::fs::remove_file(fx.kb().join("a.md")).expect("remove a.md");
    std::fs::create_dir(fx.kb().join("a.md")).expect("create the directory a.md");
    fx.layout.write("a.md/x.md", &note("Xray", "xraymarker"));

    assert_settles_on(
        &fx,
        &guard,
        &["a.md/x.md"],
        "file replaced by a directory",
        |fx| text_of(fx, "a.md/x.md").contains("xraymarker"),
    );
    drop(guard);
}

/// (P1-E3, spec A4) On macOS a rename can reach the watcher as two unpaired events, each naming
/// one path. The old name's row goes; only the new name's stays.
#[cfg(target_os = "macos")]
#[test]
fn an_unpaired_rename_drops_the_old_row() {
    let (fx, guard) = served("i544-unpaired", &[("a.md", &note("Alpha", "alphamarker"))]);
    assert_eq!(paths(&fx), ["a.md"]);

    std::fs::rename(fx.kb().join("a.md"), fx.kb().join("b.md")).expect("rename a.md to b.md");

    assert_settles_on(&fx, &guard, &["b.md"], "unpaired rename", |fx| {
        text_of(fx, "b.md").contains("alphamarker")
    });
    drop(guard);
}

/// (P1-E4, spec A3) A rename that changes only the case of a name, on a case-insensitive
/// volume: one row stays, under the new spelling.
#[cfg(target_os = "macos")]
#[test]
fn a_case_only_rename_leaves_one_row() {
    let (fx, guard) = served("i544-case-only", &[("Old.md", &note("Old", "oldmarker"))]);
    assert_eq!(paths(&fx), ["Old.md"]);

    std::fs::rename(fx.kb().join("Old.md"), fx.kb().join("old.md")).expect("rename Old.md");

    assert_settles_on(&fx, &guard, &["old.md"], "case-only rename", |fx| {
        text_of(fx, "old.md").contains("oldmarker")
    });
    drop(guard);
}

/// (P1-E6) A Japanese name (with a dakuten, which NFD splits) keeps its row through an edit and
/// a rename: after the edit under the old name, after the rename under the new one.
#[cfg(target_os = "macos")]
#[test]
fn a_japanese_name_survives_edit_and_rename() {
    let (fx, guard) = served(
        "i544-ja-file",
        &[("ガイド.md", &note("Guide", "guidemarker"))],
    );
    assert_eq!(paths(&fx), ["ガイド.md"]);

    let mut body = note("Guide", "guidemarker");
    body.push_str("\n## Added\n\nThe appended section carries addedmarker.\n");
    fx.layout.write("ガイド.md", &body);
    assert_settles_on(
        &fx,
        &guard,
        &["ガイド.md"],
        "edit of a Japanese name",
        |fx| text_of(fx, "ガイド.md").contains("addedmarker"),
    );

    std::fs::rename(fx.kb().join("ガイド.md"), fx.kb().join("ガイド2.md"))
        .expect("rename the Japanese name");
    assert_settles_on(
        &fx,
        &guard,
        &["ガイド2.md"],
        "rename of a Japanese name",
        |fx| text_of(fx, "ガイド2.md").contains("addedmarker"),
    );
    drop(guard);
}

/// (P1-E6, branch 1) A directory with a Japanese name that arrives while the server runs: the
/// file written into it is indexed and its row stays.
#[cfg(target_os = "macos")]
#[test]
fn a_japanese_directory_that_arrives_keeps_its_file() {
    let (fx, guard) = served("i544-ja-dir", &[("in.md", &note("In", "inmarker"))]);
    assert_eq!(paths(&fx), ["in.md"]);

    std::fs::create_dir(fx.kb().join("資料ガイド")).expect("create the Japanese directory");
    fx.layout
        .write("資料ガイド/a.md", &note("Alpha", "alphamarker"));

    assert_settles_on(
        &fx,
        &guard,
        &["in.md", "資料ガイド/a.md"],
        "arrival of a Japanese directory",
        |fx| text_of(fx, "資料ガイド/a.md").contains("alphamarker"),
    );
    drop(guard);
}

/// (P1-E6, branch 2) A directory renamed onto a Japanese name: its rows move and stay.
#[cfg(target_os = "macos")]
#[test]
fn a_directory_renamed_onto_a_japanese_name_keeps_its_rows() {
    let (fx, guard) = served(
        "i544-ja-dir-rename",
        &[
            ("docs/a.md", &note("Alpha", "alphamarker")),
            ("docs/b.md", &note("Bravo", "bravomarker")),
        ],
    );
    assert_eq!(paths(&fx), ["docs/a.md", "docs/b.md"]);

    std::fs::rename(fx.kb().join("docs"), fx.kb().join("ガイド集"))
        .expect("rename docs onto a Japanese name");

    assert_settles_on(
        &fx,
        &guard,
        &["ガイド集/a.md", "ガイド集/b.md"],
        "directory rename onto a Japanese name",
        |fx| {
            text_of(fx, "ガイド集/a.md").contains("alphamarker")
                && text_of(fx, "ガイド集/b.md").contains("bravomarker")
        },
    );
    drop(guard);
}
