//! No page tells you to run a shortened copy of a command list it already has.
//!
//! `CONTRIBUTING.md` used to end its "Submitting changes" list with its own
//! shorter spelling of the block above it, naming one clippy leg where CI runs
//! two. The copy was rewritten to add the second leg, and review found the
//! rewrite still wrong -- `cargo check --all-targets` and the single-threaded
//! `index_progress_cli` run were missing from it too. Rewriting a copy is
//! writing the next drift's opening value; the only fix that ends it is
//! deleting the copy and pointing at the original. That is the third time a
//! defect caused by duplication was repaired by duplicating, which is the count
//! at which this project stops relying on remembering.
//!
//! # What is walked
//!
//! The corpus `common::docs::markdown_files` collects: every `.md` file in the
//! repository except build output, the search fixtures, unpublished
//! dot-directories and `*.local.md`. The link guard walks the same set through
//! the same function.
//!
//! # What is checked
//!
//! Within one page: an inline `` `a && b` `` chain whose commands are a subset
//! of some fenced shell block's commands on the same page. Commands are compared
//! by identity -- the program and the word after it -- and not by their flags,
//! because a copy that drifts drifts in its flags first, and `cargo fmt --all`
//! against `cargo fmt --all -- --check` is exactly the difference that has to
//! register as "the same command" for the copy to be visible at all. A program
//! named by path is the same program: `/usr/local/bin/groove index` and
//! `groove index` are one instruction written for two installations.
//!
//! # What this cannot catch
//!
//! **Block against block, and page against page.** Two fenced blocks on one page
//! stand in this relation fifteen times in this repository today -- `AGENTS.md`
//! lists three commands to run while working and five that reproduce CI,
//! `docs/usage.md` shows one `groove` subcommand at a time and then all three
//! under `RUST_LOG` -- and every one of them is deliberate. A rule symmetric
//! enough to catch a copied block would fail on all fifteen, and a guard that
//! cries wolf gets switched off. Copies that span pages are held by
//! `docs_commands_pinned.rs`; copies between an English page and its Japanese
//! twin by `docs_commands_twins.rs`. Neither of those discovers a new copy: they
//! hold the ones already registered.

mod common;

use common::docs::{
    command_lines, heads_of, inline_chains, markdown_files, repo_root, shell_blocks,
};
use std::collections::BTreeSet;
use std::path::Path;

/// Every place a page tells the reader to run a subset of what one of its own
/// blocks tells them to run.
///
/// Returned rather than asserted so the corpus walk and the frozen excerpts
/// below run the same predicate.
fn shortened_copies(markdown: &str) -> Vec<String> {
    let blocks: Vec<(usize, BTreeSet<String>)> = shell_blocks(markdown)
        .iter()
        .map(|b| (b.line, heads_of(&command_lines(&b.body))))
        .filter(|(_, heads)| !heads.is_empty())
        .collect();

    let mut found = Vec::new();
    for chain in inline_chains(markdown) {
        let heads = heads_of(&chain.commands);
        for (line, block) in &blocks {
            if heads.is_subset(block) {
                found.push(format!(
                    "line {} repeats the block at line {}: {}",
                    chain.line,
                    line,
                    heads.iter().cloned().collect::<Vec<_>>().join(", ")
                ));
            }
        }
    }
    found
}

/// How many chain-against-block comparisons a page actually made.
fn comparisons_in(markdown: &str) -> usize {
    let blocks = shell_blocks(markdown)
        .iter()
        .filter(|b| !heads_of(&command_lines(&b.body)).is_empty())
        .count();
    inline_chains(markdown).len() * blocks
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("could not read {}: {e}", path.display()))
        .replace("\r\n", "\n")
}

#[test]
fn no_page_tells_you_to_run_a_shortened_copy_of_its_own_command_block() {
    let root = repo_root();
    let files = markdown_files(&root);

    let mut offenders: Vec<String> = Vec::new();
    let mut comparisons = 0usize;
    let mut pages_with_chains: BTreeSet<String> = BTreeSet::new();
    let mut pages_with_blocks: BTreeSet<String> = BTreeSet::new();

    for file in &files {
        let shown = file
            .strip_prefix(&root)
            .unwrap_or(file)
            .display()
            .to_string()
            .replace('\\', "/");
        let markdown = read(file);
        if !inline_chains(&markdown).is_empty() {
            pages_with_chains.insert(shown.clone());
        }
        if !shell_blocks(&markdown).is_empty() {
            pages_with_blocks.insert(shown.clone());
        }
        comparisons += comparisons_in(&markdown);
        for finding in shortened_copies(&markdown) {
            offenders.push(format!("{shown}: {finding}"));
        }
    }

    // A walk that finds nothing passes, so say what was walked. These two pages
    // chain commands because removing a service means disabling the unit and
    // then deleting files, which stays true after the copy this guard was built
    // for is gone. The page that carries that copy is deliberately not named
    // here: it stops being an example the moment it is fixed.
    for required in ["docs/usage.md", "docs/usage.ja.md"] {
        assert!(
            pages_with_chains.contains(required),
            "no inline command chain was found in {required}, so the half of \
             this check that reads prose found nothing to read. Either the \
             extraction stopped working or the walk no longer reaches the page. \
             Walked {} files",
            files.len()
        );
    }
    // And these carry the blocks a chain would be measured against.
    for required in [
        "AGENTS.md",
        "CONTRIBUTING.md",
        "CONTRIBUTING.ja.md",
        "docs/usage.md",
    ] {
        assert!(
            pages_with_blocks.contains(required),
            "no fenced shell block was found in {required}, so there is nothing \
             for an inline chain on that page to be a copy of"
        );
    }
    assert!(
        comparisons > 0,
        "chains and blocks were both found, yet no chain was ever measured \
         against a block, so this test asserted nothing"
    );

    offenders.sort();
    assert!(
        offenders.is_empty(),
        "these pages tell a reader to run a shortened copy of a command list \
         they already publish, and a copy drifts from the original without \
         either one looking wrong:\n  {}\n\
         Delete the copy and point at the block instead. Rewriting the copy to \
         be correct today is what produced the copies above: the fix that ends \
         it is the one that leaves no second place to update.",
        offenders.join("\n  ")
    );
}

// ---------------------------------------------------------------------------
// The same predicate against the defect it was built for, frozen at three
// points in this repository's history.
//
// Copied out of `git show` by hand, and the expectations below are written by
// hand too. Deriving either from the extractor would make these tests agree
// with whatever it does rather than with what the pages said.
// ---------------------------------------------------------------------------

/// `git show 5e6b645^:CONTRIBUTING.md`, the two regions that matter.
const STEP_3_BEFORE: &str = r#"
To reproduce what CI runs, all of these have to pass:

```bash
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo clippy --all-targets --features test-helpers,heavy-bench -- -D warnings
cargo check --all-targets
cargo test --test index_progress_cli -- --test-threads=1   # first, and single-threaded
cargo test
cargo doc --no-deps --workspace --all-features --document-private-items
```

## Submitting changes

1. Fork the repo and branch from `main`
2. Add tests for new behavior (unit tests inline, integration tests under `tests/`)
3. `cargo fmt --all && cargo clippy --all-targets -- -D warnings && cargo test && cargo doc --no-deps --workspace --all-features --document-private-items`
4. Open a PR describing the problem and the change; link any related issues
"#;

/// `git show 5e6b645:CONTRIBUTING.md`, the same two regions after the copy was
/// deleted and replaced with a reference.
const STEP_3_AFTER: &str = r#"
To reproduce what CI runs, all of these have to pass:

```bash
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo clippy --all-targets --features test-helpers,heavy-bench -- -D warnings
cargo check --all-targets
cargo test --test index_progress_cli -- --test-threads=1   # first, and single-threaded
cargo test
cargo doc --no-deps --workspace --all-features --document-private-items
```

## Submitting changes

1. Fork the repo and branch from `main`
2. Add tests for new behavior (unit tests inline, integration tests under `tests/`)
3. Run **every** command in the block under [Build and test](#build-and-test),
   in the order it gives them. It is not repeated here on purpose.
4. Open a PR describing the problem and the change; link any related issues
"#;

#[test]
fn the_copy_this_guard_was_built_for_is_caught_where_it_stood() {
    let found = shortened_copies(STEP_3_BEFORE);
    assert_eq!(found.len(), 1, "{found:?}");
    let only = &found[0];
    for named in ["cargo fmt", "cargo clippy", "cargo test", "cargo doc"] {
        assert!(only.contains(named), "{only}");
    }
    // The block names one command the copy does not, which is the whole point.
    assert!(!only.contains("cargo check"), "{only}");
}

#[test]
fn deleting_the_copy_is_what_makes_the_page_pass() {
    assert!(
        shortened_copies(STEP_3_AFTER).is_empty(),
        "{:?}",
        shortened_copies(STEP_3_AFTER)
    );
}

// ---------------------------------------------------------------------------
// The pieces the predicate is built from, each against a case taken from this
// repository rather than from what the implementation happens to do.
// ---------------------------------------------------------------------------

#[test]
fn a_command_is_identified_by_its_program_and_subcommand_and_not_its_flags() {
    use common::docs::head_of;
    assert_eq!(head_of("cargo fmt --all").as_deref(), Some("cargo fmt"));
    assert_eq!(
        head_of("cargo fmt --all -- --check").as_deref(),
        Some("cargo fmt")
    );
    // An environment prefix is not the command, and neither is sudo.
    assert_eq!(
        head_of("RUST_LOG=grooveseek=debug groove serve --kb-path ./kb").as_deref(),
        Some("groove serve")
    );
    assert_eq!(
        head_of("sudo install -d -o groove /srv/groove").as_deref(),
        Some("install")
    );
    // JSON and prose are not commands.
    assert_eq!(head_of("{"), None);
    assert_eq!(head_of("\"command\": \"/path/to/groove\","), None);
    assert_eq!(head_of("- item"), None);
}

#[test]
fn a_command_invoked_by_path_is_the_same_command() {
    use common::docs::head_of;
    // A path says where the program is installed, not which program it is.
    for spelling in [
        "groove index --kb-path ./kb",
        "./groove index --kb-path ./kb",
        "/usr/local/bin/groove index --kb-path ./kb",
    ] {
        assert_eq!(
            head_of(spelling).as_deref(),
            Some("groove index"),
            "{spelling}"
        );
    }
    // The argument after the program is not a second program.
    assert_eq!(head_of("groove ./kb").as_deref(), Some("groove"));
}

#[test]
fn sudo_options_name_a_user_and_not_a_program() {
    use common::docs::head_of;
    // `grooveseek/examples/deployments/intranet-http/README.md` runs this, and
    // reading `groove` as the program would take the user to become for the
    // thing being run.
    assert_eq!(
        head_of("sudo -u groove /usr/local/bin/groove index --kb-path /srv/groove/kb").as_deref(),
        Some("groove index")
    );
    // The word after the program is a subcommand for `groove` and an argument
    // for `cp`, and this does not tell them apart -- deliberately, since being
    // wrong here splits one identity into two rather than merging two into one.
    assert_eq!(
        head_of("sudo cp groove.service /etc/systemd/system/").as_deref(),
        Some("cp groove.service")
    );
}

#[test]
fn a_line_run_through_sudo_is_kept_as_written() {
    // The deployment recipes were invisible to every one of these guards before
    // paths were read: both translations dropped the same line, so they agreed
    // by both holding nothing, and a change to one would not have been reported.
    let kept =
        command_lines("sudo -u groove /usr/local/bin/groove index --kb-path /srv/groove/kb\n");
    assert_eq!(
        kept,
        vec!["sudo -u groove /usr/local/bin/groove index --kb-path /srv/groove/kb".to_string()],
        "{kept:?}"
    );
}

#[test]
fn a_pipeline_is_one_command_and_a_chain_is_several() {
    use common::docs::split_chain;
    assert_eq!(
        split_chain("groove search \"x\" --format json | jq '.results[]'"),
        vec!["groove search \"x\" --format json | jq '.results[]'".to_string()]
    );
    assert_eq!(
        split_chain("systemctl --user disable groove && rm ~/.config/groove"),
        vec![
            "systemctl --user disable groove".to_string(),
            "rm ~/.config/groove".to_string()
        ]
    );
}

#[test]
fn a_heredoc_body_is_not_read_as_commands() {
    // docs/usage.md writes a golden file this way, and its payload is prose
    // that gets translated on the Japanese page.
    let body = "cat > golden.yaml <<'EOF'\nqueries:\n  - query: \"what does k do?\"\nEOF\ngroove eval --kb-path ./kb\n";
    let lines = command_lines(body);
    assert_eq!(
        lines,
        vec![
            "cat > golden.yaml <<'EOF'".to_string(),
            "groove eval --kb-path ./kb".to_string()
        ],
        "{lines:?}"
    );
}

#[test]
fn a_trailing_comment_goes_but_a_hash_inside_an_argument_stays() {
    let lines =
        command_lines("cargo test   # first, and single-threaded\ngroove search \"issue #12\"\n");
    assert_eq!(
        lines,
        vec![
            "cargo test".to_string(),
            "groove search \"issue #12\"".to_string()
        ],
        "{lines:?}"
    );
}

#[test]
fn a_continued_line_is_one_command() {
    let lines =
        command_lines("groove search \"tokio spawn\" \\\n  --kb-path ./kb \\\n  --limit 3\n");
    assert_eq!(
        lines,
        vec!["groove search \"tokio spawn\" --kb-path ./kb --limit 3".to_string()],
        "{lines:?}"
    );
}

#[test]
fn prose_naming_one_command_is_not_a_chain() {
    // `groove service install` appears dozens of times in running text. Without
    // the floor of two distinct commands, every one of them would be a
    // candidate.
    assert!(inline_chains("Run `groove service install` to set it up.").is_empty());
    assert_eq!(
        inline_chains("Run `systemctl --user disable groove && rm ~/.config/groove`.").len(),
        1
    );
    // Two spellings of the same command are one command, not a chain.
    assert!(inline_chains("`cargo test && cargo test --release`").is_empty());
}
