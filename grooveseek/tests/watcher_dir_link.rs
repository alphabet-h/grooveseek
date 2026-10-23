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

/// (review P2-1 on AX-02) A directory that was indexed and is then replaced by a link to
/// elsewhere: the full walk (`follow_links(false)`) no longer collects its files and drops their
/// rows, so the watcher's reindex of one of them must drop the row too rather than keep it.
#[test]
fn a_document_whose_directory_became_a_link_leaves_the_index() {
    if run_in_hermetic_child("a_document_whose_directory_became_a_link_leaves_the_index") {
        return;
    }
    let fx = fixture(
        "ax02-swapped",
        &[("in.md", NOTE), ("sub/note.md", SECRET)],
        "",
    );
    fx.index();
    let cfg = Config::load_from(&fx.config).expect("load groove.toml");
    let registry = cfg.build_parser_registry(fx.kb()).expect("parser registry");
    let kb = fx.kb().canonicalize().expect("canonical kb");
    let (db, mut embedder) = open_in_process(&fx, &cfg);
    assert_eq!(
        db.all_document_paths().expect("paths"),
        vec!["in.md".to_string(), "sub/note.md".to_string()]
    );

    // `sub/` moves out of the knowledge base and a link to it takes its place.
    let moved = fx.layout.root().join("moved-out");
    std::fs::rename(kb.join("sub"), &moved).expect("move sub/ out");
    dir_link(&kb.join("sub"), &moved);

    let before = fx.mock.requests().len();
    let outcome = reindex_single_file(&db, &mut embedder, &kb, "sub/note.md", None, &registry)
        .expect("reindex");
    assert_eq!(outcome, SingleResult::OutsideKnowledgeBaseDeindexed);
    assert_eq!(fx.mock.requests().len(), before, "nothing was embedded");
    assert_eq!(
        db.all_document_paths().expect("paths"),
        vec!["in.md".to_string()],
        "the row of a file that now resolves outside the knowledge base must go, as the \
         full index drops it"
    );
    drop(db);
    assert_dir_empty(&fx.cache);
}

/// (local Codex r2 on AX-02) A row a version up to 1.12.0 stored under the folded spelling
/// (`notes/secret/pay.md` for a file named `secret\pay.md`) goes too when the file now resolves
/// outside the knowledge base, even with no row under the current spelling: the full index
/// does not follow the link and drops it. Unix only, where `\` is a filename character.
#[cfg(unix)]
#[test]
fn a_folded_legacy_row_under_a_directory_link_leaves_the_index() {
    if run_in_hermetic_child("a_folded_legacy_row_under_a_directory_link_leaves_the_index") {
        return;
    }
    let fx = linked("ax02-legacy");
    std::fs::write(
        fx.layout.root().join("outside").join("secret\\pay.md"),
        SECRET,
    )
    .expect("write the backslash-named file");
    let cfg = Config::load_from(&fx.config).expect("load groove.toml");
    let registry = cfg.build_parser_registry(fx.kb()).expect("parser registry");
    let kb = fx.kb().canonicalize().expect("canonical kb");
    let (db, mut embedder) = open_in_process(&fx, &cfg);
    db.upsert_document(
        "notes/secret/pay.md",
        None,
        None,
        None,
        None,
        &[],
        None,
        "legacy",
        0,
    )
    .expect("seed the folded row");

    let outcome = reindex_single_file(
        &db,
        &mut embedder,
        &kb,
        "notes/secret\\pay.md",
        None,
        &registry,
    )
    .expect("reindex");
    assert_eq!(outcome, SingleResult::OutsideKnowledgeBaseDeindexed);
    assert_eq!(
        db.all_document_paths().expect("paths"),
        vec!["in.md".to_string()],
        "the folded legacy row must go with the file"
    );
    drop(db);
    assert_dir_empty(&fx.cache);
}

/// (local Codex r2 on AX-02) A rename onto a path that resolves outside the knowledge base drops
/// the row the index already held for that destination (indexed before its directory became a
/// link) as well as the moved document's, as the full index would.
#[test]
fn a_rename_into_a_directory_link_drops_the_destination_row_too() {
    if run_in_hermetic_child("a_rename_into_a_directory_link_drops_the_destination_row_too") {
        return;
    }
    let fx = fixture(
        "ax02-rename-dest",
        &[("in.md", NOTE), ("sub/in.md", SECRET)],
        "",
    );
    fx.index();
    let cfg = Config::load_from(&fx.config).expect("load groove.toml");
    let registry = cfg.build_parser_registry(fx.kb()).expect("parser registry");
    let kb = fx.kb().canonicalize().expect("canonical kb");
    let (db, mut embedder) = open_in_process(&fx, &cfg);
    assert_eq!(
        db.all_document_paths().expect("paths"),
        vec!["in.md".to_string(), "sub/in.md".to_string()]
    );

    let moved = fx.layout.root().join("moved-out");
    std::fs::rename(kb.join("sub"), &moved).expect("move sub/ out");
    dir_link(&kb.join("sub"), &moved);
    std::fs::rename(kb.join("in.md"), kb.join("sub").join("in.md")).expect("move the note");

    let outcome = rename_single_file(
        &db,
        &mut embedder,
        &kb,
        "in.md",
        "sub/in.md",
        None,
        &registry,
    )
    .expect("rename");
    assert_eq!(outcome, RenameOutcome::MovedOutOfTheKnowledgeBase);
    assert!(
        db.all_document_paths().expect("paths").is_empty(),
        "neither the moved row nor the destination's old row may stay"
    );
    drop(db);
    assert_dir_empty(&fx.cache);
}

/// (local Codex r3 on AX-02) The same as above with a link whose target is gone: the file does
/// not exist through it, so the reindex used to stop at "file no longer exists" and keep the row,
/// while the full index (which does not follow the link) drops it.
#[test]
fn a_document_whose_directory_became_a_dangling_link_leaves_the_index() {
    if run_in_hermetic_child("a_document_whose_directory_became_a_dangling_link_leaves_the_index") {
        return;
    }
    let fx = fixture(
        "ax02-dangling",
        &[("in.md", NOTE), ("sub/note.md", SECRET)],
        "",
    );
    fx.index();
    let cfg = Config::load_from(&fx.config).expect("load groove.toml");
    let registry = cfg.build_parser_registry(fx.kb()).expect("parser registry");
    let kb = fx.kb().canonicalize().expect("canonical kb");
    let (db, mut embedder) = open_in_process(&fx, &cfg);

    let moved = fx.layout.root().join("moved-out");
    std::fs::rename(kb.join("sub"), &moved).expect("move sub/ out");
    dir_link(&kb.join("sub"), &moved);
    std::fs::remove_dir_all(&moved).expect("remove the link's target");
    assert!(!kb.join("sub/note.md").exists(), "the link must dangle");

    let outcome = reindex_single_file(&db, &mut embedder, &kb, "sub/note.md", None, &registry)
        .expect("reindex");
    assert_eq!(outcome, SingleResult::OutsideKnowledgeBaseDeindexed);
    assert_eq!(
        db.all_document_paths().expect("paths"),
        vec!["in.md".to_string()]
    );

    // Control: a file that is simply gone, with no link on the way, keeps the "no longer
    // exists" skip; its row is the removal path's to drop, not this one's.
    std::fs::remove_file(kb.join("in.md")).expect("remove in.md");
    let outcome =
        reindex_single_file(&db, &mut embedder, &kb, "in.md", None, &registry).expect("reindex");
    assert!(
        matches!(outcome, SingleResult::Skipped { .. }),
        "{outcome:?}"
    );
    assert_eq!(
        db.all_document_paths().expect("paths"),
        vec!["in.md".to_string()]
    );
    drop(db);
    assert_dir_empty(&fx.cache);
}

/// (GitHub codex r1 on PR #391) A link to another directory *inside* the knowledge base: the
/// file resolves inside, but the full walk does not follow the link, so the watcher must not
/// index it under the link's name either, and drops a row it holds under that name. The index
/// the watcher leaves is the one the full run built.
#[test]
fn a_file_reached_through_a_link_to_a_directory_inside_the_kb_is_not_indexed_twice() {
    if run_in_hermetic_child(
        "a_file_reached_through_a_link_to_a_directory_inside_the_kb_is_not_indexed_twice",
    ) {
        return;
    }
    let fx = fixture(
        "ax02-alias",
        &[("in.md", NOTE), ("real/note.md", SECRET)],
        "",
    );
    let kb = fx.kb().canonicalize().expect("canonical kb");
    dir_link(&kb.join("alias"), &kb.join("real"));
    fx.index();
    let cfg = Config::load_from(&fx.config).expect("load groove.toml");
    let registry = cfg.build_parser_registry(fx.kb()).expect("parser registry");
    let (db, mut embedder) = open_in_process(&fx, &cfg);
    let walked = db.all_document_paths().expect("paths");
    assert_eq!(
        walked,
        vec!["in.md".to_string(), "real/note.md".to_string()],
        "the full run does not follow the link"
    );

    let before = fx.mock.requests().len();
    let outcome = reindex_single_file(&db, &mut embedder, &kb, "alias/note.md", None, &registry)
        .expect("reindex");
    assert_eq!(outcome, SingleResult::Refused);
    assert_eq!(fx.mock.requests().len(), before, "nothing was embedded");
    assert_eq!(db.all_document_paths().expect("paths"), walked);

    // A row the watcher stored under the link's name before this fix goes.
    db.upsert_document("alias/note.md", None, None, None, None, &[], None, "h", 0)
        .expect("seed the alias row");
    let outcome = reindex_single_file(&db, &mut embedder, &kb, "alias/note.md", None, &registry)
        .expect("reindex");
    assert_eq!(outcome, SingleResult::OutsideKnowledgeBaseDeindexed);
    assert_eq!(db.all_document_paths().expect("paths"), walked);

    // The real name is untouched by any of it and still reindexes as an ordinary file.
    let outcome = reindex_single_file(&db, &mut embedder, &kb, "real/note.md", None, &registry)
        .expect("reindex");
    assert_eq!(outcome, SingleResult::Unchanged);
    drop(db);
    assert_dir_empty(&fx.cache);
}

/// (local Codex after PR #391 r1) A rename whose destination is missing because it is reached
/// through a dangling link: settled as one reached through a link -- the old row and the row
/// the index held for the destination both go, rather than the old row being moved onto it.
#[test]
fn a_rename_onto_a_path_through_a_dangling_link_drops_both_rows() {
    if run_in_hermetic_child("a_rename_onto_a_path_through_a_dangling_link_drops_both_rows") {
        return;
    }
    let fx = fixture("ax02-rename-dangling", &[("in.md", NOTE)], "");
    fx.index();
    let cfg = Config::load_from(&fx.config).expect("load groove.toml");
    let registry = cfg.build_parser_registry(fx.kb()).expect("parser registry");
    let kb = fx.kb().canonicalize().expect("canonical kb");
    let (db, mut embedder) = open_in_process(&fx, &cfg);

    let gone = fx.layout.root().join("gone");
    std::fs::create_dir_all(&gone).expect("create the link's target");
    dir_link(&kb.join("sub"), &gone);
    std::fs::remove_dir_all(&gone).expect("remove the link's target");
    db.upsert_document("sub/in.md", None, None, None, None, &[], None, "h", 0)
        .expect("seed the destination row");
    std::fs::remove_file(kb.join("in.md")).expect("the note left its old name");

    let outcome = rename_single_file(
        &db,
        &mut embedder,
        &kb,
        "in.md",
        "sub/in.md",
        None,
        &registry,
    )
    .expect("rename");
    assert_eq!(outcome, RenameOutcome::MovedOutOfTheKnowledgeBase);
    assert!(db.all_document_paths().expect("paths").is_empty());
    drop(db);
    assert_dir_empty(&fx.cache);
}

/// (local Codex after PR #391 r1) The folded legacy row of a missing rename destination goes,
/// both through a dangling link and with no link at all (the file is gone under every
/// spelling). Unix only, where `\` is a filename character.
#[cfg(unix)]
#[test]
fn a_missing_rename_destination_drops_its_folded_legacy_row() {
    if run_in_hermetic_child("a_missing_rename_destination_drops_its_folded_legacy_row") {
        return;
    }
    let fx = fixture(
        "ax02-rename-folded",
        &[("in.md", NOTE), ("two.md", SECRET)],
        "",
    );
    fx.index();
    let cfg = Config::load_from(&fx.config).expect("load groove.toml");
    let registry = cfg.build_parser_registry(fx.kb()).expect("parser registry");
    let kb = fx.kb().canonicalize().expect("canonical kb");
    let (db, mut embedder) = open_in_process(&fx, &cfg);
    let seed = |p: &str| {
        db.upsert_document(p, None, None, None, None, &[], None, "legacy", 0)
            .expect("seed a folded row");
    };

    // Through a dangling link.
    let gone = fx.layout.root().join("gone");
    std::fs::create_dir_all(&gone).expect("create the link's target");
    dir_link(&kb.join("sub"), &gone);
    std::fs::remove_dir_all(&gone).expect("remove the link's target");
    seed("sub/secret/pay.md");
    std::fs::remove_file(kb.join("in.md")).expect("remove in.md");
    let outcome = rename_single_file(
        &db,
        &mut embedder,
        &kb,
        "in.md",
        "sub/secret\\pay.md",
        None,
        &registry,
    )
    .expect("rename");
    assert_eq!(outcome, RenameOutcome::MovedOutOfTheKnowledgeBase);
    assert_eq!(
        db.all_document_paths().expect("paths"),
        vec!["two.md".to_string()]
    );

    // With no link: the destination is simply gone.
    seed("secret/pay.md");
    std::fs::remove_file(kb.join("two.md")).expect("remove two.md");
    let outcome = rename_single_file(
        &db,
        &mut embedder,
        &kb,
        "two.md",
        "secret\\pay.md",
        None,
        &registry,
    )
    .expect("rename");
    assert_eq!(outcome, RenameOutcome::Renamed);
    assert!(db.all_document_paths().expect("paths").is_empty());
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
