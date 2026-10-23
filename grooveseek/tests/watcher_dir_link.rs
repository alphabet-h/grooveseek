//! AX-02: the watcher's single-file paths do not read a file through a directory link that
//! leads out of the knowledge base.
//!
//! An event names a file as `kb_path.join(rel)`, and the checks before this one looked at the
//! final component only (`symlink_metadata`, the `O_NOFOLLOW` open), so `notes/secret.md` with
//! `notes -> <outside>` was read and indexed under an in-KB name. On Linux such events arrived
//! because notify followed directory symlinks when it set up its watches; the watcher now turns
//! that off (its own unit tests), and [`grooveseek::indexer::reindex_single_file`] /
//! [`grooveseek::indexer::rename_single_file`] check where the file resolves before reading it.
//!
//! These call the library functions the watcher calls, in a hermetic child
//! ([`crate::common::in_process`]), against [`crate::common::embed_mock`]. The link is a symlink
//! on Unix and a junction on Windows, where `mklink /J` needs no privilege.

mod common;

use common::embed_cli::{Fixture, fixture};
use common::embed_mock::assert_dir_empty;
use common::in_process::{open_in_process, run_in_hermetic_child};
use grooveseek::config::Config;
use grooveseek::indexer::{RenameOutcome, SingleResult, reindex_single_file, rename_single_file};

use std::path::Path;

const NOTE: &str = concat!(
    "---\ntitle: Note\n---\n\n## Note\n\n",
    "The lighthouse keeper logs every passing ship in the harbour ledger at dusk.\n",
);
const SECRET: &str = concat!(
    "---\ntitle: Secret\n---\n\n## Secret\n\n",
    "The quartermaster keeps the vault combination in the second drawer of the desk.\n",
);

/// A directory link `link` -> `target`: a junction on Windows, a symlink on Unix. Asserted
/// rather than skipped.
fn dir_link(link: &Path, target: &Path) {
    #[cfg(windows)]
    {
        let made = std::process::Command::new("cmd")
            .args([
                "/c",
                "mklink",
                "/J",
                &link.display().to_string(),
                &target.display().to_string(),
            ])
            .status()
            .expect("mklink must be runnable");
        assert!(made.success(), "mklink /J needs no elevation");
    }
    #[cfg(unix)]
    std::os::unix::fs::symlink(target, link).expect("symlinks need no privilege on unix");
}

/// `in.md` indexed, `outside/secret.md` beside the knowledge base, and `kb/notes` a directory
/// link to `outside/`, made after the index so the full run never saw it.
fn linked(prefix: &str) -> Fixture {
    let fx = fixture(prefix, &[("in.md", NOTE)], "");
    fx.index();
    let outside = fx.layout.root().join("outside");
    std::fs::create_dir_all(&outside).expect("create outside/");
    std::fs::write(outside.join("secret.md"), SECRET).expect("write the secret");
    dir_link(&fx.kb().join("notes"), &outside);
    fx
}

#[test]
fn a_file_reached_through_a_directory_link_is_not_indexed() {
    if run_in_hermetic_child("a_file_reached_through_a_directory_link_is_not_indexed") {
        return;
    }
    let fx = linked("ax02-reindex");
    let cfg = Config::load_from(&fx.config).expect("load groove.toml");
    let registry = cfg.build_parser_registry(fx.kb()).expect("parser registry");
    let kb = fx.kb().canonicalize().expect("canonical kb");
    let (db, mut embedder) = open_in_process(&fx, &cfg);

    let before = fx.mock.requests().len();
    let outcome = reindex_single_file(&db, &mut embedder, &kb, "notes/secret.md", None, &registry)
        .expect("reindex");
    assert_eq!(outcome, SingleResult::Refused);
    assert_eq!(fx.mock.requests().len(), before, "nothing was embedded");
    assert_eq!(
        db.all_document_paths().expect("paths"),
        vec!["in.md".to_string()]
    );

    // Control: a new file in the knowledge base itself is indexed, through the root as the
    // fixture spells it (not canonical on every platform), so the check canonicalizes the root
    // rather than requiring it.
    fx.layout.write("fresh.md", NOTE);
    let outcome = reindex_single_file(&db, &mut embedder, fx.kb(), "fresh.md", None, &registry)
        .expect("reindex");
    assert!(
        matches!(outcome, SingleResult::Updated { .. }),
        "{outcome:?}"
    );
    assert_eq!(
        db.all_document_paths().expect("paths"),
        vec!["fresh.md".to_string(), "in.md".to_string()]
    );
    drop(db);
    assert_dir_empty(&fx.cache);
}

#[test]
fn a_rename_into_a_directory_link_takes_the_document_out_of_the_index() {
    if run_in_hermetic_child("a_rename_into_a_directory_link_takes_the_document_out_of_the_index") {
        return;
    }
    let fx = linked("ax02-rename");
    let cfg = Config::load_from(&fx.config).expect("load groove.toml");
    let registry = cfg.build_parser_registry(fx.kb()).expect("parser registry");
    let kb = fx.kb().canonicalize().expect("canonical kb");
    let (db, mut embedder) = open_in_process(&fx, &cfg);

    // On disk the note now lives in `outside/`, under the in-KB name `notes/in.md`.
    std::fs::rename(kb.join("in.md"), kb.join("notes").join("in.md")).expect("move the note");
    let before = fx.mock.requests().len();
    let outcome = rename_single_file(
        &db,
        &mut embedder,
        &kb,
        "in.md",
        "notes/in.md",
        None,
        &registry,
    )
    .expect("rename");
    assert_eq!(outcome, RenameOutcome::MovedOutOfTheKnowledgeBase);
    assert_eq!(fx.mock.requests().len(), before, "nothing was embedded");
    assert!(
        db.all_document_paths().expect("paths").is_empty(),
        "the row neither stays under the old name nor moves to the linked one"
    );
    drop(db);
    assert_dir_empty(&fx.cache);
}
