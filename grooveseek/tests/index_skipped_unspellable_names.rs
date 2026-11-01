//! (#660, ADR-0023) On Windows, a name the index cannot hold is in
//! [`grooveseek::indexer::IndexResult::skipped_files`] as
//! [`grooveseek::indexer::SkipReason::UnspellableName`], counted in both
//! [`grooveseek::indexer::IndexResult::skipped`] and
//! [`grooveseek::indexer::IndexResult::unspellable`]. A directory whose name the index cannot hold is one entry, whatever it holds,
//! and its contents are never walked.
//!
//! These call [`grooveseek::indexer::rebuild_index`] in-process against the mock endpoint of
//! [`crate::common::embed_cli`], each test body in a hermetic child of this test binary. The
//! knowledge base is the canonical (verbatim, `\\?\`) path, the only spelling under which
//! `CON.md` is a file rather than the console.
#![cfg(windows)]

mod common;

use common::embed_cli::{Fixture, fixture};
use common::embed_mock::assert_dir_empty;
use common::in_process::{open_in_process, run_in_hermetic_child};

use grooveseek::config::Config;
use grooveseek::db::ContextMode;
use grooveseek::indexer::progress::{ProgressMode, ProgressReporter};
use grooveseek::indexer::{IndexResult, SkipReason, load_declared_schema, rebuild_index};

use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// Removes the scratch root through the verbatim prefix, the only spelling under which `CON.md`
/// and `dir.` can be removed. Declared after the fixture so it runs first.
struct VerbatimCleanup(PathBuf);
impl Drop for VerbatimCleanup {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

const BODY: &str = "# Title\n\nEnough content here to stand as a chunk.\n";

/// The fixture's knowledge base, canonical and verbatim, and the guard that removes its root.
fn verbatim_kb(fx: &Fixture) -> (PathBuf, VerbatimCleanup) {
    let kb = fx.kb().canonicalize().expect("canonical kb");
    assert!(
        kb.to_string_lossy().starts_with(r"\\?\"),
        "the fixture needs the verbatim prefix: {}",
        kb.display()
    );
    let root = kb.parent().expect("the kb has a parent").to_path_buf();
    (kb, VerbatimCleanup(root))
}

/// Write `body` to `kb\rel` (backslashes: under `\\?\` a `/` is not a separator) and check a real
/// file landed there.
fn write_file(kb: &Path, rel: &str, body: &str) {
    std::fs::write(kb.join(rel), body).expect("write a fixture file");
    assert!(
        std::fs::metadata(kb.join(rel)).is_ok_and(|m| m.is_file()),
        "the fixture must hold a real file at {rel:?}"
    );
}

/// One full run with a quiet reporter, over the canonical `kb`.
fn rebuild(fx: &Fixture, kb: &Path) -> IndexResult {
    let cfg = Config::load_from(&fx.config).expect("load groove.toml");
    let registry = cfg.build_parser_registry(kb).expect("parser registry");
    let schema = load_declared_schema(kb).expect("groove-schema.toml");
    let (db, mut embedder) = open_in_process(fx, &cfg);
    rebuild_index(
        &db,
        &mut embedder,
        kb,
        schema,
        false,
        cfg.exclude_headings.as_deref(),
        &cfg.resolve_exclude_dirs(),
        &registry,
        ProgressReporter::new(ProgressMode::Quiet),
        ContextMode::Off,
    )
    .expect("rebuild_index")
}

fn listed(result: &IndexResult) -> Vec<(String, SkipReason)> {
    result
        .skipped_files
        .iter()
        .map(|s| (s.path.clone(), s.reason))
        .collect()
}

/// Spec §6 I1, I2, I6 and I7 on one value [`rebuild_index`] returned (the same checks as the
/// helper of the other skipped-file test crate; each test crate carries its own).
fn assert_skip_ledger(result: &IndexResult) {
    assert_eq!(
        result.skipped as usize,
        result.skipped_files.len(),
        "I1: skipped == skipped_files.len(): {result:?}"
    );
    let paths: HashSet<&str> = result
        .skipped_files
        .iter()
        .map(|s| s.path.as_str())
        .collect();
    assert_eq!(
        paths.len(),
        result.skipped_files.len(),
        "I2: no path twice: {result:?}"
    );
    assert!(
        result
            .skipped_files
            .windows(2)
            .all(|w| w[0].path.as_bytes() < w[1].path.as_bytes()),
        "I6: path order: {result:?}"
    );
    let unspellable = result
        .skipped_files
        .iter()
        .filter(|s| matches!(s.reason, SkipReason::UnspellableName { .. }))
        .count();
    assert_eq!(
        result.unspellable as usize, unspellable,
        "unspellable counts the UnspellableName entries: {result:?}"
    );
    assert!(
        result.unspellable <= result.skipped,
        "I7: unspellable <= skipped: {result:?}"
    );
}

/// T-L5: `CON.md` is listed as a file whose name the index cannot hold, counted once in
/// [`IndexResult::unspellable`] and in [`IndexResult::skipped`], and the Done line still names it the way it did.
#[test]
fn an_unspellable_file_is_listed_and_counted() {
    if run_in_hermetic_child("an_unspellable_file_is_listed_and_counted") {
        return;
    }
    let fx = fixture("groove-660-unspellable-file", &[], "");
    let (kb, _cleanup) = verbatim_kb(&fx);
    write_file(&kb, "ok.md", BODY);
    write_file(&kb, "CON.md", BODY);

    let result = rebuild(&fx, &kb);
    assert_eq!(
        listed(&result),
        vec![(
            "CON.md".to_string(),
            SkipReason::UnspellableName { directory: false }
        )],
        "{result:?}"
    );
    assert_eq!(result.unspellable, 1, "{result:?}");
    assert!(result.skipped >= result.unspellable, "{result:?}");
    assert_eq!(result.updated, 1, "ok.md is indexed: {result:?}");
    let line = result.summary_line();
    assert!(line.contains(", 1 not indexed ("), "{line}");
    assert_skip_ledger(&result);
    assert_dir_empty(&fx.cache);
}

/// T-L5b (spec §5.5 S6a, decision (a)): an empty directory and a directory of images, neither
/// holding a file the index would read, are each one [`SkipReason::UnspellableName`] entry with
/// `directory: true`; the files inside are never walked, so none of them is listed.
#[test]
fn an_unspellable_directory_counts_once_without_its_contents() {
    if run_in_hermetic_child("an_unspellable_directory_counts_once_without_its_contents") {
        return;
    }
    let fx = fixture("groove-660-unspellable-dir", &[], "");
    let (kb, _cleanup) = verbatim_kb(&fx);
    write_file(&kb, "ok.md", BODY);
    std::fs::create_dir(kb.join("dir.")).expect("create dir.");
    assert!(
        std::fs::metadata(kb.join("dir.")).is_ok_and(|m| m.is_dir()),
        "the fixture must hold a real directory named dir."
    );
    std::fs::create_dir(kb.join("pics ")).expect("create pics ");
    write_file(&kb, r"pics \a.png", "not really a png");
    write_file(&kb, r"pics \b.png", "not really a png");

    let result = rebuild(&fx, &kb);
    let directory = SkipReason::UnspellableName { directory: true };
    assert_eq!(
        listed(&result),
        vec![
            ("dir.".to_string(), directory),
            ("pics ".to_string(), directory),
        ],
        "one entry per directory, nothing from inside: {result:?}"
    );
    assert_eq!((result.skipped, result.unspellable), (2, 2), "{result:?}");
    assert_skip_ledger(&result);
    assert_dir_empty(&fx.cache);
}
