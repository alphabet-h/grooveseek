//! The source tree this workspace ships, as the manifest defines it.
//!
//! Two guards ask "which files are this program?" -- the stderr guard walks
//! them for the words they write, and the layout guard walks them for the rows
//! that describe them -- and `AGENTS.md` says two places answering one question
//! call one implementation. The members list started in
//! `tests/diagnostics_stay_ascii.rs`; it lives here now so the second guard
//! did not write a second copy.
//!
//! The members list is the workspace's own answer to what this program is, so
//! a crate added later joins every check by being added to the workspace. An
//! earlier version walked `grooveseek/src` and `crates/*/src`: the same set
//! today, and one that would quietly skip a member placed anywhere else -- the
//! shape of mistake these guards exist to stop.

use std::collections::BTreeSet;
use std::path::PathBuf;

use super::docs::{repo_root, shown};

/// Every `[workspace].members` entry of the root manifest, as written there.
///
/// A manifest with no members, a member that is not a string, a member
/// written with glob syntax (`*`, `?`, `[`), or a list that does not name
/// `grooveseek` -- the crate these tests belong to -- fails here rather than
/// contributing nothing: a walk over zero members finds zero files and every
/// check downstream passes on an empty tree.
pub fn workspace_members() -> Vec<String> {
    let manifest_path = repo_root().join("Cargo.toml");
    let text = std::fs::read_to_string(&manifest_path)
        .unwrap_or_else(|e| panic!("could not read {}: {e}", manifest_path.display()));
    let manifest: toml::Value = toml::from_str(&text)
        .unwrap_or_else(|e| panic!("could not parse {}: {e}", manifest_path.display()));
    let members = manifest
        .get("workspace")
        .and_then(|workspace| workspace.get("members"))
        .and_then(|members| members.as_array())
        .unwrap_or_else(|| panic!("{} declares no workspace members", manifest_path.display()));

    let members: Vec<String> = members
        .iter()
        .map(|member| {
            let name = member
                .as_str()
                .unwrap_or_else(|| panic!("a workspace member that is not a path: {member}"));
            assert!(
                !name.contains(['*', '?', '[']),
                "a workspace member written as a glob cannot be walked as a path, so \
                 nothing under it would be checked: {name}"
            );
            name.to_string()
        })
        .collect();
    assert!(
        members.iter().any(|m| m == "grooveseek"),
        "{} does not list grooveseek among its workspace members, so the crate \
         these tests belong to would not be walked: {members:?}",
        manifest_path.display()
    );
    members
}

/// The source directory of every workspace member, under the repository root.
///
/// A member whose `src` is missing -- a moved crate, a renamed directory --
/// fails here rather than contributing nothing.
pub fn source_dirs() -> Vec<PathBuf> {
    let root = repo_root();
    let mut dirs: Vec<PathBuf> = workspace_members()
        .iter()
        .map(|member| root.join(member).join("src"))
        .collect();
    for dir in &dirs {
        assert!(
            dir.is_dir(),
            "a workspace member has no src directory, so nothing of it would be checked: {}",
            dir.display()
        );
    }
    dirs.sort();
    dirs
}

/// Every file under every member's `src`, whatever its extension, named the
/// way a failure message names it (see [`shown`]): relative to the repository
/// root, with `/`.
///
/// The filesystem is walked, not `git ls-files`: the CI checkout is one commit
/// deep and a guard that sees different things locally and in CI is the one
/// property `docs_links_resolve.rs` refuses. So a file that is in `src` and not
/// in git is reported like any other, which is the right answer for a stray
/// `.orig` or a scratch module -- it is in the tree the binary is built from.
///
/// Nothing is filtered here, dot-prefixed names included. The stderr guard
/// reads every `.rs` the compiler might, and `#[path = ".generated.rs"]` is
/// one; a walk that dropped it would let that module's diagnostics go
/// unchecked (codex P2 on PR #231). A caller that wants machine-local
/// artifacts such as `.DS_Store` out of its answer drops them itself.
///
/// A walk error is fatal rather than skipped, for the reason
/// [`super::docs::markdown_files`] gives: `.flatten()` would drop the
/// unreadable directory and the tree would quietly lose whatever was under it.
pub fn source_tree() -> BTreeSet<String> {
    let root = repo_root();
    let mut found = BTreeSet::new();
    for dir in source_dirs() {
        for entry in walkdir::WalkDir::new(&dir) {
            let entry = entry.unwrap_or_else(|e| {
                panic!(
                    "the walk from {} failed partway through, so the tree it \
                     collected is smaller than this test claims to check: {e}",
                    dir.display()
                )
            });
            if entry.file_type().is_file() {
                found.insert(shown(&root, entry.path()));
            }
        }
    }
    found
}
