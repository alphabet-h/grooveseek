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
//! The corpus [`common::docs::markdown_files`] collects: every `.md` file in the
//! repository except build output, the search fixtures, unpublished
//! dot-directories and `*.local.md`. The link guard walks the same set through
//! the same function.
//!
//! # What is checked
//!
//! Within one page: an inline chain -- `` `a && b` ``, `` `a || b` `` or
//! `` `a ; b` `` -- naming two or more different commands, all of which some
//! fenced shell block on the same page also names. Commands are compared
//! by identity -- the program and the word after it -- and not by their flags,
//! because a copy that drifts drifts in its flags first, and `cargo fmt --all`
//! against `cargo fmt --all -- --check` is exactly the difference that has to
//! register as "the same command" for the copy to be visible at all. A program
//! named by path is the same program: `/usr/local/bin/groove index` and
//! `groove index` are one instruction written for two installations.
//!
//! Underneath that, two guards on the reader itself, because every defect this
//! file's history records took the same shape -- a line the reader did not
//! understand was dropped, both translations dropped it alike, and the guards
//! comparing them agreed by both holding nothing. Every fenced shell block in
//! the corpus has to name at least one command, and every line of every such
//! block has to be placed: an instruction, a continuation, a heredoc payload,
//! a blank, a comment, grammar -- or reported, with the reason.
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
    command_lines, half_read_chains, heads_of, inline_chains, markdown_files, read, repo_root,
    shell_blocks, shown,
};
use std::collections::BTreeSet;

/// The chains this guard can say anything about.
///
/// A chain naming one program -- `schtasks /End ... ; schtasks /Delete ...`, or
/// `cargo test && cargo test --release` -- is a subset of any block that runs
/// that program, so treating it as a possible copy would report every page that
/// mentions a command twice in one breath. Two distinct commands is the floor
/// here and nowhere else: the twin guard compares those same chains and needs
/// them, since running one program twice is still two instructions.
fn comparable_chains(markdown: &str) -> Vec<(usize, BTreeSet<String>)> {
    inline_chains(markdown)
        .into_iter()
        .map(|chain| (chain.line, heads_of(&chain.commands)))
        .filter(|(_, heads)| heads.len() >= 2)
        .collect()
}

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
    for (chain_line, heads) in comparable_chains(markdown) {
        for (line, block) in &blocks {
            if heads.is_subset(block) {
                found.push(format!(
                    "line {} repeats the block at line {}: {}",
                    chain_line,
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
    comparable_chains(markdown).len() * blocks
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
        let shown = shown(&root, file);
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
//
// They are excerpts, not transcripts. **Every command line and every fence is
// verbatim**, because those are what the predicate reads; the prose between
// them is shortened, since a paragraph explaining why CI needs two clippy legs
// changes nothing about which commands the page names. Saying "copied by hand"
// without saying that would be the kind of claim this whole pull request exists
// to catch.
// ---------------------------------------------------------------------------

/// `git show 5e6b645^:CONTRIBUTING.md`: the CI block and the step that copied
/// it, with the prose between them shortened.
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

/// `git show 5e6b645:CONTRIBUTING.md`: the same two regions after the copy was
/// deleted and replaced with a reference, shortened the same way.
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
    // sudo takes its own options and then accepts assignments, so the two kinds
    // of wrapper interleave. Stripping each once in a fixed order leaves the
    // probe pointing at whichever came second, and the line is dropped without
    // anything being said about it.
    assert_eq!(
        head_of("sudo -u groove RUST_LOG=debug /usr/local/bin/groove serve").as_deref(),
        Some("groove serve")
    );
    assert_eq!(
        head_of("RUST_LOG=debug sudo -u groove groove index").as_deref(),
        Some("groove index")
    );
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
fn an_assignment_that_runs_a_command_keeps_the_command() {
    use common::docs::head_of;
    // Splitting on whitespace makes `NEXT_ID=$(jq` one token, and dropping it
    // as an environment prefix took `jq` with it -- leaving fragments that name
    // no program, so the line fell out of the corpus without a word. The page
    // that wrote it has stopped; the shape is what this holds.
    assert_eq!(
        head_of("NEXT_ID=$(jq '.features | map(.id) | max + 1' .dev/features.json)").as_deref(),
        Some("jq")
    );
    assert_eq!(
        head_of("OUT=\"$(groove status --kb-path ./kb)\"").as_deref(),
        Some("groove status")
    );
    // An assignment that runs nothing is still just an assignment.
    assert_eq!(
        head_of("RUST_LOG=debug groove serve").as_deref(),
        Some("groove serve")
    );
    // A substitution with no arguments is one token, so the closing bracket is
    // still stuck to the program. `PWD=$(pwd)` is the commoner spelling of the
    // two and was still falling out after the arguments case was fixed.
    assert_eq!(head_of("PWD=$(pwd)").as_deref(), Some("pwd"));
    assert_eq!(head_of("STAMP=`date`").as_deref(), Some("date"));
    assert_eq!(head_of("OUT=\"$(pwd)\"").as_deref(), Some("pwd"));
}

#[test]
fn setting_a_variable_is_an_instruction_even_with_no_program_in_it() {
    // `.claude/skills/codex-review/SKILL.md` opens a block with `S=<scratchpad>`
    // and then uses `$S`. The value is a step the reader performs and a thing a
    // translation can get wrong, and it was being dropped: the block still had
    // other lines, so no guard noticed it had stopped being compared.
    let lines = command_lines("S=<scratchpad>\nbash script.sh $S/out\n");
    assert_eq!(
        lines,
        vec![
            "S=<scratchpad>".to_string(),
            "bash script.sh $S/out".to_string()
        ],
        "{lines:?}"
    );
    // A shell array assignment is the same shape and was dropped the same way.
    let array = command_lines("cargo test\nFILES=(a b)\n");
    assert_eq!(array.len(), 2, "{array:?}");
    // And a chain of nothing but assignments is still two instructions.
    assert_eq!(inline_chains("`FOO=1 && BAR=2`").len(), 1);
}

#[test]
fn a_span_the_reader_only_half_understands_is_reported_rather_than_dropped() {
    use common::docs::half_read_chains;
    // `&&` is shell and nothing else, so failing on half of one is worth saying.
    let mixed = half_read_chains("Run `groove index && ???unreadable???`.");
    assert_eq!(mixed.len(), 1, "{mixed:?}");
    // Reading all of it, or none of it, is not the same thing at all.
    assert!(half_read_chains("`groove index && groove status`").is_empty());
    assert!(half_read_chains("`??? && ???`").is_empty());
}

#[test]
fn a_semicolon_alone_is_not_evidence_of_a_shell_chain() {
    use common::docs::half_read_chains;
    // All three are in the tree, inside backticks, and none is a command chain.
    for not_a_chain in [
        "`text/plain; charset=utf-8`",
        "`vec![Data::default(); rows * cols]`",
        "`[Console]::OutputEncoding=[Text.Encoding]::UTF8; Write-Output x`",
    ] {
        assert!(
            half_read_chains(not_a_chain).is_empty(),
            "reported as half-read: {not_a_chain}"
        );
        assert!(
            inline_chains(not_a_chain).is_empty(),
            "treated as a chain: {not_a_chain}"
        );
    }
    // A `;` span the reader understands end to end is still a chain, which is
    // what the Windows migration line needs.
    assert_eq!(
        inline_chains("`schtasks /End /TN x ; schtasks /Delete /TN x /F`").len(),
        1
    );
}

#[test]
fn a_subshell_bracket_is_not_the_program() {
    use common::docs::head_of;
    assert_eq!(
        head_of(
            "(gh run list --json event | ConvertFrom-Json) | Where-Object { $_.event -eq 'push' }"
        )
        .as_deref(),
        Some("gh run")
    );
}

#[test]
fn a_heredoc_body_cannot_swallow_its_own_terminator() {
    // The continuation join used to run over every raw line before the heredoc
    // was noticed, so a payload line ending in a backslash absorbed the line
    // after it. When that line was the terminator, the skip never ended and
    // every command below it vanished from a block that still looked read.
    let body = "cat > f <<'EOF'\npayload \\\nEOF\ngroove index --kb-path ./kb\n";
    let lines = command_lines(body);
    assert_eq!(
        lines,
        vec![
            "cat > f <<'EOF'".to_string(),
            "groove index --kb-path ./kb".to_string()
        ],
        "{lines:?}"
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
fn every_line_of_a_block_is_placed_and_the_place_is_its_page_line() {
    use common::docs::{LineRead, fenced_blocks, read_block};
    // A fence at the top level and one inside a list item, both with a blank
    // line inside: pulldown-cmark strips the item's indent from the body, and
    // this pins that it keeps the blank line, so `block.line + 1 + index` is the
    // page line of the raw line -- the number a failure has to name.
    let markdown = "# T\n\n```bash\ncargo test\n\ncargo doc\n```\n\n- item\n\n   ```bash\n   groove index\n\n   groove status\n   ```\n";
    let page: Vec<&str> = markdown.lines().collect();
    let blocks = fenced_blocks(markdown);
    assert_eq!(blocks.len(), 2, "{blocks:?}");
    for block in &blocks {
        let lines = read_block(&block.body);
        assert_eq!(lines.len(), 3, "{lines:?}");
        for line in &lines {
            let at = block.line + 1 + line.index;
            assert_eq!(page[at - 1].trim(), line.raw.trim(), "{block:?} {line:?}");
        }
        assert_eq!(lines[1].read, LineRead::Blank, "{lines:?}");
    }
    // Every class the reader can answer, on one body, in the order the lines
    // come: a comment, a blank, a heredoc opener and its payload and terminator,
    // a continued instruction and the line it continues onto.
    let placed = read_block(
        "# only a comment\n\ncat <<EOF\npayload\nEOF\ngroove index \\\n  --kb-path ./kb\n",
    );
    let reads: Vec<&LineRead> = placed.iter().map(|l| &l.read).collect();
    assert_eq!(
        reads,
        vec![
            &LineRead::Comment,
            &LineRead::Blank,
            &LineRead::Instruction("cat <<EOF".to_string()),
            &LineRead::HeredocBody,
            &LineRead::HeredocBody,
            &LineRead::Instruction("groove index --kb-path ./kb".to_string()),
            &LineRead::Continued,
        ],
        "{placed:?}"
    );
}

#[test]
fn an_argument_that_continues_onto_the_next_line_is_part_of_the_instruction() {
    use common::docs::{LineRead, heads_of, read_block};
    // `.claude/skills/windows-quirks/SKILL.md` opens `python -c "` and closes
    // the quote three lines down. The payload is what the reader runs, so it is
    // part of the instruction -- the same reason `RUST_LOG=...` is kept -- and
    // the lines it spans are continuations, not instructions of their own.
    // Before this, `b=open(...)` inside the quote read as an assignment.
    let placed = read_block("python -c \"\nb=1\n\"\n");
    let reads: Vec<&LineRead> = placed.iter().map(|l| &l.read).collect();
    assert_eq!(
        reads,
        vec![
            &LineRead::Instruction("python -c \" b=1 \"".to_string()),
            &LineRead::Continued,
            &LineRead::Continued,
        ],
        "{placed:?}"
    );
    // A quoted jq filter can run past the end of its line and chain `&& mv`
    // after the closing quote. The chain is outside the quote, so both
    // programs are named.
    let lines =
        command_lines("jq '.a += [\n  {\n    \"id\": 1\n  }\n]' f > /tmp/g && mv /tmp/g f\n");
    assert_eq!(
        lines,
        vec!["jq '.a += [ { \"id\": 1 } ]' f > /tmp/g && mv /tmp/g f".to_string()],
        "{lines:?}"
    );
    let heads = heads_of(&lines);
    assert!(heads.contains("jq") && heads.contains("mv"), "{heads:?}");
    // A translated payload is a drifted instruction, which the twin guard
    // compares whole lines to see.
    assert_ne!(
        command_lines("python -c \"\na\n\"\n"),
        command_lines("python -c \"\nb\n\"\n")
    );
}

#[test]
fn a_hash_inside_an_open_quote_is_not_a_comment_and_a_quote_inside_a_comment_opens_nothing() {
    // Inside the quote, `#` is payload.
    let lines = command_lines("python -c \"\n# not a comment\n\"\n");
    assert_eq!(
        lines,
        vec!["python -c \" # not a comment \"".to_string()],
        "{lines:?}"
    );
    // Inside a comment, an apostrophe is prose. `docs/usage.md` writes
    // `# default 'groove'` after a command, and a comment that carried its
    // quote to the next line would swallow the rest of the block.
    let lines = command_lines("cargo test # it's\ncargo doc\n");
    assert_eq!(
        lines,
        vec!["cargo test".to_string(), "cargo doc".to_string()],
        "{lines:?}"
    );
}

#[test]
fn a_backslash_inside_an_open_quote_is_a_character() {
    // Outside quotes a trailing backslash continues the line and is dropped.
    // Inside, it is part of the argument, and the line continues anyway
    // because the quote is still open.
    let lines = command_lines("python -c \"\nprint(1) \\\n\"\n");
    assert_eq!(
        lines,
        vec!["python -c \" print(1) \\ \"".to_string()],
        "{lines:?}"
    );
}

#[test]
fn a_quote_that_never_closes_is_reported_rather_than_swallowed() {
    use common::docs::{LineRead, read_block};
    let placed = read_block("echo \"a\ncargo test\n");
    assert!(
        matches!(&placed[0].read, LineRead::Unread(why) if why.contains("never closes")),
        "{placed:?}"
    );
    assert_eq!(placed[1].read, LineRead::Continued, "{placed:?}");
    assert!(command_lines("echo \"a\ncargo test\n").is_empty());
}

#[test]
fn a_heredoc_opener_with_a_quote_still_open_is_not_finished() {
    use common::docs::{LineRead, read_block};
    // The shell reads the body after the line that completes the command, and
    // a line with a quote still open has not completed it. So the quote
    // continues (codex P2 on #233: closing the group at the opener discarded
    // the quote and let a malformed block pass), the body starts after the
    // line that closes it, and a quote that never closes is reported -- which
    // is also what keeps a terminator from being swallowed in silence.
    let lines = command_lines("cat <<EOF \"\n\"\nbody\nEOF\ngroove index\n");
    assert_eq!(
        lines,
        vec!["cat <<EOF \" \"".to_string(), "groove index".to_string()],
        "{lines:?}"
    );
    let placed = read_block("cat <<'EOF' \"\nbody\nEOF\ngroove index\n");
    assert!(
        matches!(&placed[0].read, LineRead::Unread(why) if why.contains("never closes")),
        "{placed:?}"
    );
    assert!(command_lines("cat <<'EOF' \"\nbody\nEOF\ngroove index\n").is_empty());
}

#[test]
fn a_heredoc_that_never_terminates_is_reported_rather_than_swallowed() {
    use common::docs::{LineRead, read_block};
    let placed = read_block("cat <<EOF\nbody\ngroove index\n");
    assert!(
        matches!(&placed[0].read, LineRead::Unread(why) if why.contains("never terminates")),
        "{placed:?}"
    );
    assert!(command_lines("cat <<EOF\nbody\ngroove index\n").is_empty());
}

#[test]
fn a_case_arm_is_read_as_the_instruction_it_runs() {
    use common::docs::{LineRead, heads_of, read_block};
    // `.claude/commands/feature-flow.md` commits `.dev` inside a `case` on the
    // repository root. The pattern is a branch condition, not an instruction,
    // and `;;` is grammar; what stands between them is what the reader runs,
    // and it was invisible to every guard.
    let lines = command_lines(
        "case x in\n  */.dev) git -C .dev push ;;\n  *) echo \"a; b\" >&2; exit 1 ;;\nesac\n",
    );
    assert_eq!(
        lines,
        vec![
            "case x in".to_string(),
            "git -C .dev push".to_string(),
            "echo \"a; b\" >&2; exit 1".to_string(),
        ],
        "{lines:?}"
    );
    let heads = heads_of(&lines);
    for head in ["git", "echo", "exit"] {
        assert!(heads.contains(head), "{heads:?}");
    }
    // A label on a line of its own, a `;;` on its own and `esac` are grammar:
    // placed, contributing nothing, and not unread.
    let placed = read_block("case x in\n  a)\n    ls\n    ;;\n  *) ;;\nesac\n");
    let reads: Vec<&LineRead> = placed.iter().map(|l| &l.read).collect();
    assert_eq!(
        reads,
        vec![
            &LineRead::Instruction("case x in".to_string()),
            &LineRead::Syntax,
            &LineRead::Instruction("ls".to_string()),
            &LineRead::Syntax,
            &LineRead::Syntax,
            &LineRead::Syntax,
        ],
        "{placed:?}"
    );
    // Outside a `case`, a word ending in `)` is not a label.
    let outside = read_block("foo) bar\n");
    assert!(
        matches!(outside[0].read, LineRead::Unread(_)),
        "{outside:?}"
    );
}

#[test]
fn every_word_in_syntax_only_reads_as_syntax_on_a_line_of_its_own() {
    use common::docs::{LineRead, SYNTAX_ONLY, read_block};
    // The list is exact whole-line matches, so each entry is a class of its own
    // that nothing else exercises; each has to be seen to be placed.
    for word in SYNTAX_ONLY {
        let placed = read_block(&format!("{word}\n"));
        assert_eq!(placed[0].read, LineRead::Syntax, "{word}");
    }
}

#[test]
fn a_powershell_assignment_is_an_instruction_and_names_the_cmdlet_it_runs() {
    use common::docs::{LineRead, head_of, read_block};
    // `CHANGELOG.md`'s service migration note assigns the result of a cmdlet:
    // `$action = New-ScheduledTaskAction ...`. The `=` is its own token, unlike
    // POSIX `NAME=value`, and the line runs the cmdlet, so it keeps the program
    // the way `NEXT_ID=$(jq ...)` does.
    assert_eq!(
        head_of("$action = New-ScheduledTaskAction -Execute x").as_deref(),
        Some("New-ScheduledTaskAction")
    );
    // Setting a variable to a value is still an instruction, with no program.
    assert_eq!(command_lines("$x = 5\n"), vec!["$x = 5".to_string()]);
    assert_eq!(head_of("$x = 5"), None);
    // A comparison is not an assignment.
    let compared = read_block("$x == 5\n");
    assert!(
        matches!(compared[0].read, LineRead::Unread(_)),
        "{compared:?}"
    );
    // The POSIX shape is unchanged.
    assert_eq!(
        command_lines("S=<scratchpad>\n"),
        vec!["S=<scratchpad>".to_string()]
    );
}

#[test]
fn a_continuation_on_the_last_line_is_reported_rather_than_dropped() {
    use common::docs::{LineRead, read_block};
    // A trailing backslash with nothing after it used to leave the joined text
    // in a buffer the loop never flushed, so the line vanished from the block
    // without a word. It is still not a command; now it is said to be nothing.
    let placed = read_block("cargo test \\\n");
    assert_eq!(placed.len(), 1, "{placed:?}");
    assert!(
        matches!(&placed[0].read, LineRead::Unread(why) if why.contains("continuation")),
        "{placed:?}"
    );
    assert!(command_lines("cargo test \\\n").is_empty());
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
    // the floor of two commands, every one of them would be a candidate.
    assert!(inline_chains("Run `groove service install` to set it up.").is_empty());
    assert_eq!(
        inline_chains("Run `systemctl --user disable groove && rm ~/.config/groove`.").len(),
        1
    );
}

#[test]
fn one_program_run_twice_is_two_instructions_but_not_a_possible_copy() {
    // `docs/usage.md`'s Windows migration line ends a task and then deletes it,
    // beside a Linux line and a macOS line that chain with `&&`. All three are
    // instructions a reader follows, so all three are chains.
    let windows = inline_chains(
        "- Windows: `schtasks /End /TN '\\groove' ; schtasks /Delete /TN '\\groove' /F`",
    );
    assert_eq!(windows.len(), 1, "{windows:?}");
    assert_eq!(windows[0].commands.len(), 2, "{windows:?}");

    // The subset guard cannot say anything about them, though: a chain naming
    // one program is a subset of every block that runs it, so it would report
    // any page that mentions a command twice in one breath. That floor lives in
    // this guard, not in the extractor the twin guard shares.
    assert!(comparable_chains("`cargo test && cargo test --release`").is_empty());
    assert_eq!(
        comparable_chains("`systemctl --user disable groove && rm ~/.config/groove`").len(),
        1
    );
}

/// Every shell block in this repository names at least one command.
///
/// This is the guard for the class of defect the rest of this pull request kept
/// finding one instance at a time: a line the reader does not understand is
/// dropped, both translations drop the same line, and the two then agree by both
/// holding nothing. Six of those were found by review -- a command invoked by
/// path, an environment prefix, a `sudo -u` option, a `;` chain, wrappers in
/// either order, and an assignment that runs a command. Each was fixed, and the
/// seventh would have arrived the same way.
///
/// A block tagged as shell and yielding no command means the reader failed to
/// read it. Whether that block is compared against anything today is beside the
/// point: it is the shape that goes silent, and it cannot be in the tree.
///
/// This is the coarse half. A block that yields one command and drops three is
/// read as far as this guard can tell; the line guard below asks about the
/// three.
///
/// The rule for an exception, if one is ever needed: it goes in a list with the
/// reason, the way `common::docs::SKIPPED_PATHS` does. There is nothing to
/// exempt today.
#[test]
fn every_shell_block_in_the_corpus_names_a_command() {
    let root = repo_root();
    let mut silent: Vec<String> = Vec::new();
    let mut pages_read: BTreeSet<String> = BTreeSet::new();
    for file in markdown_files(&root) {
        let shown = shown(&root, &file);
        let markdown = read(&file);
        for block in shell_blocks(&markdown) {
            let commands = command_lines(&block.body);
            if commands.is_empty() {
                let first = block
                    .body
                    .lines()
                    .find(|l| !l.trim().is_empty())
                    .unwrap_or("");
                silent.push(format!("{shown}:{} starts `{}`", block.line, first.trim()));
            } else {
                pages_read.insert(shown.clone());
            }
        }
    }
    // Named pages rather than a count, for the reason the other guards in this
    // file name theirs: "some block somewhere was read" still passes when a skip
    // rule grows wide enough to drop a whole directory, and this guard exists
    // precisely to notice reading less than it claims to.
    for required in [
        "AGENTS.md",
        "CONTRIBUTING.md",
        "docs/usage.md",
        "grooveseek/examples/deployments/intranet-http/README.md",
    ] {
        assert!(
            pages_read.contains(required),
            "no shell block in {required} was read, so either the walk stopped \
             reaching it or the reader stopped understanding it. Reached {} \
             page(s) with shell blocks",
            pages_read.len()
        );
    }
    silent.sort();
    assert!(
        silent.is_empty(),
        "these blocks are tagged as shell and the reader found no command in \
         them, so every guard that compares them is comparing nothing and will \
         say so by staying quiet:\n  {}\n\
         Teach the reader the shape rather than the page. Every entry that has \
         appeared here so far was a syntax it did not know, and each one was \
         invisible to all three guards until someone read the block by hand.",
        silent.join("\n  ")
    );
}

/// No line of a shell block is left unread.
///
/// The block guard above fails when a block yields nothing. It passes a block
/// that yields one command and drops the other three, and both translations
/// drop the same three, so the guards that compare them compare nothing there
/// and stay quiet. This asks the reader to account for every line: an
/// instruction, a continuation, a heredoc payload, a blank, a comment -- and a
/// line it cannot place is reported here with the reason.
///
/// A new variant of [`common::docs::LineRead`] has to be placed in the `match`
/// below by hand:
/// there is no wildcard arm, so the compiler asks whether the new class is
/// reported or accounted for, which is the question a silent class skips.
#[test]
fn no_line_in_a_shell_block_in_the_corpus_is_left_unread() {
    use common::docs::{LineRead, read_block};
    use std::collections::BTreeMap;
    let root = repo_root();
    let mut unread: Vec<String> = Vec::new();
    let mut seen: BTreeMap<&str, BTreeSet<String>> = BTreeMap::new();
    for file in markdown_files(&root) {
        let shown = shown(&root, &file);
        let markdown = read(&file);
        for block in shell_blocks(&markdown) {
            for line in read_block(&block.body) {
                let at = block.line + 1 + line.index;
                let class = match &line.read {
                    LineRead::Instruction(_) => "instruction",
                    LineRead::Continued => "continued",
                    LineRead::HeredocBody => "heredoc",
                    LineRead::Blank => "blank",
                    LineRead::Comment => "comment",
                    LineRead::Syntax => "syntax",
                    LineRead::Unread(why) => {
                        unread.push(format!("{shown}:{at} `{}` -- {why}", line.raw.trim()));
                        continue;
                    }
                };
                seen.entry(class).or_default().insert(shown.clone());
            }
        }
    }
    // Every class the reader can answer is answered on a page known to need
    // it. A class that stops being produced is a reader that stopped seeing a
    // shape, and "some line somewhere was continued" would not notice.
    for (class, page) in [
        ("instruction", "AGENTS.md"),
        // A `\` continuation, and a quote that closes lines later.
        ("continued", "docs/usage.md"),
        ("continued", ".claude/skills/windows-quirks/SKILL.md"),
        ("heredoc", "docs/usage.md"),
        ("comment", "docs/usage.md"),
        ("blank", "docs/usage.md"),
        // `esac`, and the arms of the one `case` in the tree.
        ("syntax", ".claude/commands/feature-flow.md"),
    ] {
        assert!(
            seen.get(class).is_some_and(|pages| pages.contains(page)),
            "no line in {page} was read as {class}, so either the walk stopped \
             reaching it or the reader stopped seeing that shape"
        );
    }
    unread.sort();
    assert!(
        unread.is_empty(),
        "these lines sit in blocks tagged as shell and the reader could not \
         place them, so no guard is comparing them and none will say so:\n  {}\n\
         Teach the reader the shape rather than the page.",
        unread.join("\n  ")
    );
}

/// No inline chain is read halfway.
///
/// The sibling of the block guard above, for the half of the corpus it does not
/// reach. A span whose parts are all commands is a chain and gets compared; a
/// span where none of them are is not one -- prose about boolean operators, a
/// line of Rust -- and is rightly ignored. In between is the silence: the reader
/// recognises one part, fails on the next, and drops the span whole, taking the
/// part it understood with it. Nothing is then being compared and nothing says
/// so, which is this pull request's subject stated once more.
#[test]
fn no_inline_chain_in_the_corpus_is_read_halfway() {
    let root = repo_root();
    let mut partial: Vec<String> = Vec::new();
    for file in markdown_files(&root) {
        let shown = shown(&root, &file);
        let markdown = read(&file);
        for chain in half_read_chains(&markdown) {
            partial.push(format!("{shown}:{} `{}`", chain.line, chain.text));
        }
    }
    partial.sort();
    assert!(
        partial.is_empty(),
        "the reader understood part of each of these spans and not the rest, so \
         it dropped them whole and the part it did understand went with them:\n  \
         {}\n\
         Teach it the syntax it stumbled on. Leaving the span out is the failure \
         mode every finding in this pull request has had.",
        partial.join("\n  ")
    );
}

/// Not a guard: it counts the relation this file's header says it refuses to
/// check, so the number quoted there is measured against the extractor that
/// ships rather than an earlier one.
#[test]
fn the_block_subset_relation_this_guard_refuses_is_still_common() {
    let root = repo_root();
    let mut pairs: Vec<String> = Vec::new();
    for file in markdown_files(&root) {
        let shown = shown(&root, &file);
        let markdown = read(&file);
        let blocks: Vec<(usize, BTreeSet<String>)> = shell_blocks(&markdown)
            .iter()
            .map(|b| (b.line, heads_of(&command_lines(&b.body))))
            .filter(|(_, heads)| !heads.is_empty())
            .collect();
        for (a_line, a) in &blocks {
            for (b_line, b) in &blocks {
                if a_line != b_line && a != b && a.is_subset(b) {
                    pairs.push(format!("{shown}:{a_line} in {shown}:{b_line}"));
                }
            }
        }
    }
    // The header says "fifteen times". If this count moves, that sentence is
    // wrong and has to move with it -- a number written beside a claim goes
    // stale on its own, and this file exists because of that class of defect.
    // It has moved both ways: up when the reader learned to follow a quoted
    // argument across lines, so a block running one program became a subset of
    // the block that chained a second after a closing quote; and back down when
    // the page carrying that pair stopped shipping the blocks at all. Neither
    // time was the guard wrong -- the corpus moved under it.
    assert_eq!(pairs.len(), 15, "{pairs:#?}");
}
