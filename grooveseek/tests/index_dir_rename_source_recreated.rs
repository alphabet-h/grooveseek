//! (kuriya 627, AX-42 item 7) A directory renamed and then created again under its old name: the
//! next full `groove index` keeps the files now under both names and leaves no row the disk no
//! longer backs.
//!
//! The watcher handles the same edit by moving the rows it lists off the old prefix, and it stats
//! the old directory before sweeping that prefix, so a recreated directory stops the sweep (the
//! end-to-end watcher test of the recreated old end pins that, outside macOS). A row the watcher
//! did not move then sits under a directory that exists again. The full index is what removes
//! such a row: its prune ([`grooveseek::indexer::rebuild_index`], and the private
//! `documents_to_delete` of [`grooveseek::indexer`]) compares each indexed path with the files the
//! walk visited, path by path, so a directory that exists again protects none of the rows under
//! it. Rename detection (the private `detect_renames` of [`grooveseek::indexer`]) pairs only rows
//! whose path is gone from disk, so the recreated `docs/a.md` is read in place as an edit rather
//! than taken for the far end of a rename.
//!
//! The fixture makes each remover carry one row: `docs/b.md` can only leave by the rename (its
//! bytes moved unchanged to `notes/b.md`), and `docs/c.md` only by the prune (its moved copy was
//! edited, so no file on disk has its hash). The test runs `groove index` alone, with no watcher,
//! so it does not depend on how a platform reports file events.

mod common;

use common::embed_cli::{fixture, index_ok, note, rows};
use common::embed_mock::assert_dir_empty;
use common::rename::rename_retrying;

/// A [`note`] titled `title` whose only section holds `marker`.
fn marked(title: &str, marker: &str) -> String {
    note(
        title,
        &format!("This note carries the marker {marker} in its only section."),
    )
}

/// The markers each indexed path's text carries, of the ones this test writes.
fn markers(rows: &[(String, String)]) -> Vec<(String, Vec<&'static str>)> {
    const ALL: [&str; 6] = [
        "alphamarker",
        "bravomarker",
        "charliemarker",
        "charlieeditedmarker",
        "gammamarker",
        "deltamarker",
    ];
    rows.iter()
        .map(|(p, text)| {
            // `charliemarker` is not a substring of `charlieeditedmarker`, so each is told apart.
            let found = ALL.iter().copied().filter(|m| text.contains(m)).collect();
            (p.clone(), found)
        })
        .collect()
}

#[test]
fn a_full_index_after_a_dir_rename_and_recreate_leaves_no_stale_row() {
    let fx = fixture(
        "ax42-dir-recreated",
        &[
            ("docs/a.md", &marked("Alpha", "alphamarker")),
            ("docs/b.md", &marked("Bravo", "bravomarker")),
            ("docs/c.md", &marked("Charlie", "charliemarker")),
        ],
        "",
    );
    fx.index();
    assert_eq!(
        markers(&rows(&fx)),
        vec![
            ("docs/a.md".to_string(), vec!["alphamarker"]),
            ("docs/b.md".to_string(), vec!["bravomarker"]),
            ("docs/c.md".to_string(), vec!["charliemarker"]),
        ],
        "premise: the first index"
    );

    let docs = fx.kb().join("docs");
    rename_retrying(&docs, &fx.kb().join("notes"));
    std::fs::create_dir(&docs).expect("create docs again");
    // The old name again, with another file's text; and a name the directory never held.
    fx.layout
        .write("docs/a.md", &marked("Gamma", "gammamarker"));
    fx.layout
        .write("docs/d.md", &marked("Delta", "deltamarker"));
    // The moved copy of `c.md` is edited, so no file on disk has the hash of the row `docs/c.md`.
    fx.layout
        .write("notes/c.md", &marked("Charlie", "charlieeditedmarker"));

    let line = index_ok(&fx);

    assert_eq!(
        markers(&rows(&fx)),
        vec![
            ("docs/a.md".to_string(), vec!["gammamarker"]),
            ("docs/d.md".to_string(), vec!["deltamarker"]),
            ("notes/a.md".to_string(), vec!["alphamarker"]),
            ("notes/b.md".to_string(), vec!["bravomarker"]),
            ("notes/c.md".to_string(), vec!["charlieeditedmarker"]),
        ],
        "{line}"
    );
    // `docs/b.md` left by the rename to `notes/b.md`, and `docs/c.md` by the prune: each path
    // asserted, so an end state reached the other way round fails here.
    assert!(line.contains(" 1 renamed,"), "{line}");
    assert!(line.contains(" 1 deleted,"), "{line}");

    // A second run finds nothing left to do: no row was kept that a later run still removes.
    let again = index_ok(&fx);
    assert!(again.contains(" 0 renamed, 0 deleted,"), "{again}");
    assert_dir_empty(&fx.cache);
}
