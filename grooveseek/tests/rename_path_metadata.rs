//! AX-01: a rename whose bytes did not change moves the row's path-derived `category` and
//! `topic` along with its path, on both of the paths a rename reaches the index by -- the full
//! run's rename detection (`groove index`) and the watcher's
//! [`grooveseek::indexer::rename_single_file`].
//!
//! Under the default `context_mode = "off"` both take the same-hash fast path: no re-parse, no
//! embedding. Before the fix that path wrote the new `path` alone, so the row kept the old
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
