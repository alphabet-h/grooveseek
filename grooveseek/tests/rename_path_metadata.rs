//! AX-01: a rename whose bytes did not change moves the row's path-derived `category` and
//! `topic` along with its path, on both of the paths a rename reaches the index by -- the full
//! run's rename detection (`groove index`) and the watcher's
//! [`grooveseek::indexer::rename_single_file`].
//!
//! Under the default `context_mode = "off"` both take the same-hash fast path: no re-parse, no
//! embedding. Before the fix that path wrote only the new path, so the row kept the old
//! directory's category and topic, and since the content hash still matched, no later
//! incremental run rewrote them either.
//!
//! Everything runs against [`crate::common::embed_mock`], so nothing downloads a model. The
//! watcher test calls the library function the watcher calls, in a hermetic child
//! ([`crate::common::in_process`]), rather than driving a live file watcher.

mod common;

use common::embed_cli::{Fixture, fixture, stderr_of};
use common::embed_mock::assert_dir_empty;
use common::in_process::{open_in_process, run_in_hermetic_child};
use grooveseek::config::Config;
use grooveseek::indexer::{RenameOutcome, rename_single_file};

/// No frontmatter topic: its topic is the path's.
const PLAIN: &str = concat!(
    "---\ntitle: Plain\n---\n\n## Plain\n\n",
    "The lighthouse keeper logs every passing ship in the harbour ledger at dusk.\n",
);
/// A frontmatter topic, which wins over the path's and must survive the rename.
const PINNED: &str = concat!(
    "---\ntitle: Pinned\ntopic: pinned\n---\n\n## Pinned\n\n",
    "The lighthouse keeper logs every passing ship in the harbour ledger at dawn.\n",
);
const QUERY: &str = "lighthouse keeper harbour ledger";

/// The knowledge base before the move: both notes under `a/x/`, indexed once.
fn indexed(prefix: &str) -> Fixture {
    let fx = fixture(
        prefix,
        &[("a/x/plain.md", PLAIN), ("a/x/pinned.md", PINNED)],
        "",
    );
    fx.index();
    fx
}

/// Move both notes from `a/x/` to `b/y/` on disk.
fn move_on_disk(fx: &Fixture) {
    std::fs::create_dir_all(fx.kb().join("b/y")).expect("create b/y");
    for name in ["plain.md", "pinned.md"] {
        std::fs::rename(
            fx.kb().join("a/x").join(name),
            fx.kb().join("b/y").join(name),
        )
        .expect("move a note");
    }
}

/// The sorted paths `groove search` returns under `filter` (`["--category", "b"]` and so on).
fn hits(fx: &Fixture, filter: &[&str]) -> Vec<String> {
    let out = fx
        .cmd()
        .args(["search", QUERY, "--kb-path"])
        .arg(fx.kb())
        .args(filter)
        .args([
            "--format",
            "json",
            "--limit",
            "10",
            "--include-low-quality",
            "--min-confidence-ratio",
            "0",
        ])
        .output()
        .expect("spawn groove search");
    assert!(
        out.status.success(),
        "groove search failed:\n{}",
        stderr_of(&out)
    );
    let json: serde_json::Value = serde_json::from_slice(&out.stdout).expect("search JSON");
    let mut paths: Vec<String> = json["results"]
        .as_array()
        .expect("results array")
        .iter()
        .map(|r| r["path"].as_str().expect("a path").to_string())
        .collect();
    paths.sort();
    paths.dedup();
    paths
}

/// What both tests check once the notes have moved and the index has caught up.
fn assert_filters_follow_the_move(fx: &Fixture) {
    let both = vec!["b/y/pinned.md".to_string(), "b/y/plain.md".to_string()];
    assert_eq!(hits(fx, &["--category", "b"]), both, "category b");
    assert!(
        hits(fx, &["--category", "a"]).is_empty(),
        "category a must hold nothing after the move"
    );
    assert_eq!(
        hits(fx, &["--topic", "y"]),
        vec!["b/y/plain.md".to_string()],
        "the path topic follows the path"
    );
    assert!(
        hits(fx, &["--topic", "x"]).is_empty(),
        "topic x must hold nothing after the move"
    );
    assert_eq!(
        hits(fx, &["--topic", "pinned"]),
        vec!["b/y/pinned.md".to_string()],
        "a frontmatter topic is kept"
    );
}

#[test]
fn a_full_run_rename_moves_the_category_and_the_path_topic() {
    let fx = indexed("ax01-full");
    move_on_disk(&fx);

    let before = fx.mock.requests().len();
    fx.index();
    // The premise: both moves went through rename detection's fast path, which embeds nothing.
    // Had either been seen as a delete plus a new file, the new rows would have been written
    // from a fresh parse and would pass the checks below without the fix.
    assert!(
        fx.requests_since(before)
            .iter()
            .all(|r| r.model() != Some(common::embed_mock::DOC_MODEL)),
        "a same-bytes rename must not re-embed under context_mode off"
    );

    assert_filters_follow_the_move(&fx);
    assert_dir_empty(&fx.cache);
}

#[test]
fn a_watcher_rename_moves_the_category_and_the_path_topic() {
    if run_in_hermetic_child("a_watcher_rename_moves_the_category_and_the_path_topic") {
        return;
    }
    let fx = indexed("ax01-watch");
    move_on_disk(&fx);

    let cfg = Config::load_from(&fx.config).expect("load groove.toml");
    let registry = cfg.build_parser_registry(fx.kb()).expect("parser registry");
    let kb = fx.kb().canonicalize().expect("canonical kb");
    let (db, mut embedder) = open_in_process(&fx, &cfg);
    let before = fx.mock.requests().len();
    for name in ["plain.md", "pinned.md"] {
        let outcome = rename_single_file(
            &db,
            &mut embedder,
            &kb,
            &format!("a/x/{name}"),
            &format!("b/y/{name}"),
            None,
            &registry,
        )
        .expect("rename a note");
        // The same-hash fast path: the path moved, nothing was re-parsed.
        assert_eq!(outcome, RenameOutcome::Renamed, "{name}");
    }
    assert_eq!(fx.mock.requests().len(), before, "nothing was embedded");
    drop(db);

    assert_filters_follow_the_move(&fx);
    assert_dir_empty(&fx.cache);
}

/// A frontmatter topic that happens to equal the old directory's name. Only the bytes can tell it
/// from a path-derived `x`, so a rename that compares the stored value with the old path's turns
/// it into `y` (local Codex on PR #388).
const PINNED_AS_DIR: &str = concat!(
    "---\ntitle: Pinned as the directory\ntopic: x\n---\n\n## Pinned\n\n",
    "The lighthouse keeper logs every passing ship in the harbour ledger at noon.\n",
);

/// The knowledge base before the move: `a/x/note.md` with `topic: x` in its frontmatter.
fn indexed_pinned_as_dir(prefix: &str) -> Fixture {
    let fx = fixture(prefix, &[("a/x/note.md", PINNED_AS_DIR)], "");
    fx.index();
    fx
}

/// Move `a/x/note.md` to `b/y/note.md` on disk.
fn move_pinned_as_dir(fx: &Fixture) {
    std::fs::create_dir_all(fx.kb().join("b/y")).expect("create b/y");
    std::fs::rename(fx.kb().join("a/x/note.md"), fx.kb().join("b/y/note.md"))
        .expect("move the note");
}

/// What both tests check once the note has moved: the frontmatter's `x` stayed, the new
/// directory's `y` was not taken, and the category still follows the path.
fn assert_the_frontmatter_topic_stays(fx: &Fixture) {
    let moved = vec!["b/y/note.md".to_string()];
    assert_eq!(
        hits(fx, &["--topic", "x"]),
        moved,
        "a frontmatter topic equal to the old directory is kept"
    );
    assert!(
        hits(fx, &["--topic", "y"]).is_empty(),
        "the new directory's topic must not replace a frontmatter topic"
    );
    assert_eq!(hits(fx, &["--category", "b"]), moved, "category b");
}

#[test]
fn a_full_run_rename_keeps_a_frontmatter_topic_equal_to_the_old_directory() {
    let fx = indexed_pinned_as_dir("ax01-pin-full");
    move_pinned_as_dir(&fx);

    let before = fx.mock.requests().len();
    fx.index();
    assert!(
        fx.requests_since(before)
            .iter()
            .all(|r| r.model() != Some(common::embed_mock::DOC_MODEL)),
        "a same-bytes rename must not re-embed under context_mode off"
    );

    assert_the_frontmatter_topic_stays(&fx);
    assert_dir_empty(&fx.cache);
}

#[test]
fn a_watcher_rename_keeps_a_frontmatter_topic_equal_to_the_old_directory() {
    if run_in_hermetic_child(
        "a_watcher_rename_keeps_a_frontmatter_topic_equal_to_the_old_directory",
    ) {
        return;
    }
    let fx = indexed_pinned_as_dir("ax01-pin-watch");
    move_pinned_as_dir(&fx);

    let cfg = Config::load_from(&fx.config).expect("load groove.toml");
    let registry = cfg.build_parser_registry(fx.kb()).expect("parser registry");
    let kb = fx.kb().canonicalize().expect("canonical kb");
    let (db, mut embedder) = open_in_process(&fx, &cfg);
    let before = fx.mock.requests().len();
    let outcome = rename_single_file(
        &db,
        &mut embedder,
        &kb,
        "a/x/note.md",
        "b/y/note.md",
        None,
        &registry,
    )
    .expect("rename the note");
    assert_eq!(outcome, RenameOutcome::Renamed);
    assert_eq!(fx.mock.requests().len(), before, "nothing was embedded");
    drop(db);

    assert_the_frontmatter_topic_stays(&fx);
    assert_dir_empty(&fx.cache);
}

/// A text cap below [`PLAIN`]'s size, appended to the fixture's `groove.toml` as its `[index]`.
const LOWERED_TEXT_CAP: &str = "\n[index]\nmax_text_file_size = 16\n";

/// (local Codex r3 on PR #388) A watcher rename the watcher cannot read -- the moved file is
/// over the size limit -- moves the category but keeps the stored topic: without the bytes, a
/// path-derived `x` cannot be told from a frontmatter `topic: x`, and overwriting it with the new
/// path's would break the latter. A full run under the same limit skips the file and keeps the
/// row as it is. Once the limit is raised again, `groove index --force` parses the file and the
/// topic follows the new path.
#[test]
fn a_watcher_rename_over_the_size_cap_keeps_the_stored_topic_until_the_file_is_indexed() {
    if run_in_hermetic_child(
        "a_watcher_rename_over_the_size_cap_keeps_the_stored_topic_until_the_file_is_indexed",
    ) {
        return;
    }
    let fx = fixture("ax01-capped", &[("a/x/n.md", PLAIN)], "");
    fx.index();

    // Lower the cap below the note's size, then move it.
    let original = std::fs::read_to_string(&fx.config).expect("read groove.toml");
    std::fs::write(&fx.config, format!("{original}{LOWERED_TEXT_CAP}"))
        .expect("lower the text cap");
    std::fs::create_dir_all(fx.kb().join("b/y")).expect("create b/y");
    std::fs::rename(fx.kb().join("a/x/n.md"), fx.kb().join("b/y/n.md")).expect("move the note");

    let cfg = Config::load_from(&fx.config).expect("load groove.toml");
    let registry = cfg.build_parser_registry(fx.kb()).expect("parser registry");
    let kb = fx.kb().canonicalize().expect("canonical kb");
    let (db, mut embedder) = open_in_process(&fx, &cfg);
    let before = fx.mock.requests().len();
    let outcome = rename_single_file(
        &db,
        &mut embedder,
        &kb,
        "a/x/n.md",
        "b/y/n.md",
        None,
        &registry,
    )
    .expect("rename the note");
    // The premise: the size-cap exit, which reads no bytes.
    assert_eq!(outcome, RenameOutcome::RenamedSizeCapped);
    drop(db);

    // A full run under the lowered cap skips the over-cap file and keeps its row.
    fx.index();
    assert!(
        fx.requests_since(before)
            .iter()
            .all(|r| r.model() != Some(common::embed_mock::DOC_MODEL)),
        "an over-cap rename must not re-embed"
    );
    let moved = vec!["b/y/n.md".to_string()];
    assert_eq!(fx.documents(), 1, "the row is kept");
    assert_eq!(
        hits(&fx, &["--category", "b"]),
        moved,
        "the category follows the path"
    );
    assert!(
        hits(&fx, &["--category", "a"]).is_empty(),
        "category a must hold nothing after the move"
    );
    assert_eq!(
        hits(&fx, &["--topic", "x"]),
        moved,
        "the stored topic is kept while the bytes cannot be read"
    );
    assert!(
        hits(&fx, &["--topic", "y"]).is_empty(),
        "the new path's topic is not guessed without the bytes"
    );

    // The recovery: raise the cap back and rebuild. The bytes are the ones the row was built
    // from, so a plain full run finds the hash unchanged and leaves the row alone; `--force` (or
    // an edit to the file) parses it again, and the topic is rebuilt from the new path.
    std::fs::write(&fx.config, &original).expect("restore the text cap");
    let out = fx.index_force();
    assert!(
        out.status.success(),
        "groove index --force failed:\n{}",
        stderr_of(&out)
    );
    assert_eq!(
        hits(&fx, &["--topic", "y"]),
        moved,
        "the topic follows the path once the file is indexed"
    );
    assert!(
        hits(&fx, &["--topic", "x"]).is_empty(),
        "topic x must hold nothing once the file is indexed"
    );
    assert_eq!(hits(&fx, &["--category", "b"]), moved, "category b");
    assert_dir_empty(&fx.cache);
}

/// The moved file's new bytes: a frontmatter `topic: z` and no body, so nothing to chunk.
const FRONTMATTER_ONLY_Z: &str = "---\ntitle: Emptied\ntopic: z\n---\n";
/// The moved file's new bytes: a frontmatter `topic: z` and a body the reindex can chunk.
const PINNED_Z: &str = concat!(
    "---\ntitle: Pinned elsewhere\ntopic: z\n---\n\n## Pinned\n\n",
    "The lighthouse keeper logs every passing ship in the harbour ledger at midnight.\n",
);

/// Index `a/x/n.md` (frontmatter `topic: x`), move it to `b/y/n.md` and replace its bytes with
/// `new_body`, then hand the move to the watcher's [`rename_single_file`]. Returns the fixture and
/// the outcome.
fn watcher_rename_with_new_bytes(prefix: &str, new_body: &str) -> (Fixture, RenameOutcome) {
    let fx = fixture(prefix, &[("a/x/n.md", PINNED_AS_DIR)], "");
    fx.index();
    std::fs::create_dir_all(fx.kb().join("b/y")).expect("create b/y");
    std::fs::rename(fx.kb().join("a/x/n.md"), fx.kb().join("b/y/n.md")).expect("move the note");
    std::fs::write(fx.kb().join("b/y/n.md"), new_body).expect("rewrite the moved note");

    let cfg = Config::load_from(&fx.config).expect("load groove.toml");
    let registry = cfg.build_parser_registry(fx.kb()).expect("parser registry");
    let kb = fx.kb().canonicalize().expect("canonical kb");
    let (db, mut embedder) = open_in_process(&fx, &cfg);
    let outcome = rename_single_file(
        &db,
        &mut embedder,
        &kb,
        "a/x/n.md",
        "b/y/n.md",
        None,
        &registry,
    )
    .expect("rename the note");
    drop(db);
    (fx, outcome)
}

/// (codex P2 round 4 on PR #388) A watcher rename whose bytes also changed, where the reindex
/// then writes nothing -- the new bytes are frontmatter only, so there is nothing to chunk. The
/// row keeps its old hash and its old chunks, so its topic must stay the one those chunks were
/// built with (`x`), not the `z` of bytes that were never committed.
#[test]
fn a_watcher_rename_with_changed_bytes_that_index_nothing_keeps_the_stored_topic() {
    if run_in_hermetic_child(
        "a_watcher_rename_with_changed_bytes_that_index_nothing_keeps_the_stored_topic",
    ) {
        return;
    }
    let (fx, outcome) = watcher_rename_with_new_bytes("ax01-r4-empty", FRONTMATTER_ONLY_Z);
    // The premise: the reindex the changed bytes went through wrote nothing.
    assert_eq!(outcome, RenameOutcome::RenamedButNotReindexed);

    let moved = vec!["b/y/n.md".to_string()];
    // The old chunks are still under the new path, so the query still finds them.
    assert_eq!(
        hits(&fx, &["--category", "b"]),
        moved,
        "the category follows the path"
    );
    assert!(
        hits(&fx, &["--topic", "z"]).is_empty(),
        "a topic from bytes that were never indexed must not be written"
    );
    assert_eq!(
        hits(&fx, &["--topic", "x"]),
        moved,
        "the topic stays the one the stored chunks were built with"
    );
    assert_dir_empty(&fx.cache);
}

/// (codex P2 round 4 on PR #388) The counterpart: a watcher rename whose bytes changed and whose
/// reindex succeeds takes its topic from those bytes, as the index writes it.
#[test]
fn a_watcher_rename_with_changed_bytes_that_reindex_takes_their_topic() {
    if run_in_hermetic_child("a_watcher_rename_with_changed_bytes_that_reindex_takes_their_topic") {
        return;
    }
    let (fx, outcome) = watcher_rename_with_new_bytes("ax01-r4-body", PINNED_Z);
    assert!(
        matches!(outcome, RenameOutcome::RenamedAndReindexed { .. }),
        "the changed bytes were reindexed: {outcome:?}"
    );

    let moved = vec!["b/y/n.md".to_string()];
    assert_eq!(
        hits(&fx, &["--topic", "z"]),
        moved,
        "the reindex writes the frontmatter topic of the new bytes"
    );
    assert!(
        hits(&fx, &["--topic", "x"]).is_empty(),
        "topic x must hold nothing once the new bytes are indexed"
    );
    assert_eq!(hits(&fx, &["--category", "b"]), moved, "category b");
    assert_dir_empty(&fx.cache);
}
