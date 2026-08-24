//! The corpus of Markdown this repository publishes, and how to read commands
//! out of it.
//!
//! Two guards ask the same question of the same files -- "which pages are this
//! repository's documentation?" -- and `AGENTS.md` says two places answering one
//! question must call one implementation. The walk started in
//! `tests/docs_links_resolve.rs`; it lives here now so the command-copy guards
//! can use it without writing a second copy. Moving it was part of adding them,
//! not a follow-up.
//!
//! # Why the `.ja.md` classifier is here and not shared with the binary
//!
//! `grooveseek/src/main.rs` already has one, in `mod documented_flags`, and its
//! documentation says it is the only place that reads the suffix. That module is
//! `#[cfg(test)]` inside the **binary**, and an integration test links the
//! library, so it cannot be reached from here. `src/test_support.rs` is
//! `#[cfg(test)]` too. Widening either to share ~40 lines would put test-only
//! code in the shipping library, which this project has deliberately avoided.
//! So there are two classifiers, and the claim in `main.rs` is scoped to its own
//! module rather than the tree.

#![allow(dead_code)] // each guard uses a different part of this module

use pulldown_cmark::Options;
use std::path::{Path, PathBuf};

/// The extensions GitHub renders these pages with, and only those.
///
/// `Options::all()` is the shorthand, and it is wrong in two ways that would
/// never announce themselves. `ENABLE_SMART_PUNCTUATION` rewrites `--` as an en
/// dash before the slug is taken, and an en dash is stripped where a hyphen is
/// kept -- so a heading containing `--` would slug one way here and another on
/// GitHub. `ENABLE_HEADING_ATTRIBUTES` reads `## Foo {#bar}` as an id, which
/// GitHub does not honour: there the braces are part of the text the slug comes
/// from. Neither shape is in this corpus today, which is exactly why the choice
/// has to be written down rather than discovered later.
///
/// The metadata-block extension stays on. `.claude/skills/*/SKILL.md` carries
/// YAML frontmatter, and without it the closing `---` turns the block above into
/// a setext heading and invents an anchor for it.
pub fn github_flavour() -> Options {
    Options::ENABLE_TABLES
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_FOOTNOTES
        | Options::ENABLE_YAML_STYLE_METADATA_BLOCKS
}

/// This crate lives at `<repo>/grooveseek`, and the documentation it checks is
/// published from `<repo>`.
pub fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("CARGO_MANIFEST_DIR is <repo root>/grooveseek, which has a parent")
        .to_path_buf()
}

/// Directories the walk refuses to enter, by their path from the repository
/// root, each with the reason -- so that removing one is an argument rather than
/// a deletion.
pub const SKIPPED_PATHS: &[(&str, &str)] = &[
    ("target", "build output, including vendored sources"),
    (
        "grooveseek/tests/fixtures",
        "knowledge bases under test, not documentation",
    ),
];

/// Dot-directories this repository actually publishes. Everything else that
/// starts with a dot is some tool's state.
pub const PUBLISHED_DOT_DIRS: &[&str] = &[".claude", ".github"];

/// Whether the walk stops here, given the directory's path from the root.
///
/// The dot rule is a shape rather than a list on purpose. `.git` is one case,
/// but so are `.dev` (a nested private repository, present on the maintainer's
/// machine and absent from a fresh clone) and `.remember` (one machine's
/// session notes), and the next tool to leave Markdown in a dot-directory has
/// not been installed yet. Naming them one at a time means the guard checks a
/// different corpus on every machine until someone notices.
pub fn is_skipped_dir(relative: &Path) -> bool {
    if SKIPPED_PATHS
        .iter()
        .any(|(path, _)| relative == Path::new(path))
    {
        return true;
    }
    relative
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|name| name.starts_with('.') && !PUBLISHED_DOT_DIRS.contains(&name))
}

/// Every Markdown page this repository publishes, in a stable order.
///
/// A walk error is fatal rather than skipped. `.flatten()` would drop the
/// unreadable directory and carry on, and the corpus would quietly lose whatever
/// was under it -- a guard that checks less than it says it checks, which is the
/// one failure this test has no way to report. The named pages each guard asserts
/// on catch a skip rule that grew too wide; they cannot catch a directory the
/// filesystem refused halfway down.
pub fn markdown_files(root: &Path) -> Vec<PathBuf> {
    let mut found: Vec<PathBuf> = walkdir::WalkDir::new(root)
        .into_iter()
        .filter_entry(|e| {
            e.depth() == 0
                || !e.file_type().is_dir()
                || !is_skipped_dir(e.path().strip_prefix(root).unwrap_or(e.path()))
        })
        .map(|entry| {
            entry.unwrap_or_else(|e| {
                panic!(
                    "the walk from {} failed partway through, so the corpus it \
                     collected is smaller than this test claims to check: {e}",
                    root.display()
                )
            })
        })
        .map(|e| e.into_path())
        .filter(|p| p.extension().is_some_and(|e| e == "md"))
        .filter(|p| {
            // `*.local.md` is gitignored, so it is on one machine only.
            !p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.ends_with(".local.md"))
        })
        .collect();
    found.sort();
    found
}
