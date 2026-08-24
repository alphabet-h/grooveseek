//! An English page and its Japanese twin tell you to run the same commands.
//!
//! The pages are translations of each other, so their prose differs by design
//! and their commands do not. When only one half of a pair is edited, the other
//! half keeps instructing its readers to do the old thing, and nothing about
//! either page looks wrong: each is internally consistent, and the two are never
//! read side by side. That is how `CONTRIBUTING.ja.md` came to still carry the
//! shortened command copy that `CONTRIBUTING.md` deleted -- the repair landed on
//! one twin.
//!
//! # What is walked
//!
//! Every `X.ja.md` in the corpus `common::docs::markdown_files` collects,
//! paired with the `X.md` beside it.
//!
//! # What is checked
//!
//! Two things, and only about commands. The sequence of fenced shell blocks:
//! same number, and the i-th block on each side naming the same commands in the
//! same order. And the multiset of inline `a && b` chains. Comparison is by
//! normalised command line, so a translated trailing comment is not a
//! difference and a changed flag is.
//!
//! Single commands named in running prose are not compared. Which command a
//! sentence points at is the translator's call, and pinning it would make every
//! rewritten sentence a failure.
//!
//! # What this cannot catch
//!
//! **A pair that drifts on both sides at once**, and anything outside a shell
//! fence: untagged blocks here are transcripts of output, whose Japanese half is
//! legitimately a translation, and `docs/eval.ja.md` differs from
//! `docs/eval.md` inside one of them today by dropping a whole section. That is
//! a real defect and a different one -- a missing paragraph, not a drifted
//! command -- and it is filed rather than caught here.

mod common;

use common::docs::{
    command_lines, english_counterpart, inline_chains, markdown_files, repo_root, shell_blocks,
};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

fn read(path: &Path) -> String {
    std::fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("could not read {}: {e}", path.display()))
        .replace("\r\n", "\n")
}

fn shown(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .display()
        .to_string()
        .replace('\\', "/")
}

/// The commands each fenced shell block names, in source order.
fn block_commands(markdown: &str) -> Vec<(usize, Vec<String>)> {
    shell_blocks(markdown)
        .into_iter()
        .map(|b| (b.line, command_lines(&b.body)))
        .collect()
}

/// Everything the two halves of a pair disagree about, block by block.
///
/// Every way the sequences can differ has to produce a line here. Reporting
/// only the commands one side is missing would let a block that names the same
/// commands in a different order pass **after** the comparison had already
/// noticed it was not equal, and order is the instruction: `CONTRIBUTING.md`
/// runs one test target before the full suite on purpose.
fn block_differences(
    en_name: &str,
    en_blocks: &[(usize, Vec<String>)],
    ja_name: &str,
    ja_blocks: &[(usize, Vec<String>)],
) -> Vec<String> {
    if en_blocks.len() != ja_blocks.len() {
        return vec![format!(
            "{en_name} has {} shell block(s), {ja_name} has {}",
            en_blocks.len(),
            ja_blocks.len()
        )];
    }
    let mut found = Vec::new();
    for ((en_line, en_cmds), (ja_line, ja_cmds)) in en_blocks.iter().zip(ja_blocks) {
        if en_cmds == ja_cmds {
            continue;
        }
        let before = found.len();
        for command in en_cmds {
            if !ja_cmds.contains(command) {
                found.push(format!(
                    "{en_name}:{en_line} has `{command}`, {ja_name}:{ja_line} does not"
                ));
            }
        }
        for command in ja_cmds {
            if !en_cmds.contains(command) {
                found.push(format!(
                    "{ja_name}:{ja_line} has `{command}`, {en_name}:{en_line} does not"
                ));
            }
        }
        if found.len() == before {
            found.push(format!(
                "{en_name}:{en_line} and {ja_name}:{ja_line} name the same commands in a \
                 different order or a different number of times"
            ));
        }
    }
    found
}

/// How many of each inline chain a page carries.
///
/// Keyed on the commands, so a chain written with `;` on one page and `&&` on
/// the other still counts as the same instruction; the span as written is
/// carried alongside so a failure can quote what the page actually says.
fn chain_multiset(markdown: &str) -> BTreeMap<Vec<String>, (usize, String)> {
    let mut counted: BTreeMap<Vec<String>, (usize, String)> = BTreeMap::new();
    for chain in inline_chains(markdown) {
        let entry = counted.entry(chain.commands).or_insert((0, chain.text));
        entry.0 += 1;
    }
    counted
}

#[test]
fn a_page_and_its_japanese_twin_name_the_same_commands() {
    let root = repo_root();
    let files = markdown_files(&root);
    let present: BTreeSet<PathBuf> = files.iter().cloned().collect();

    let mut japanese = 0usize;
    let mut pairs: Vec<(PathBuf, PathBuf)> = Vec::new();
    for file in &files {
        let Some(english) = english_counterpart(file) else {
            continue;
        };
        japanese += 1;
        if present.contains(&english) {
            pairs.push((english, file.clone()));
        }
    }

    // A Japanese page with no English page beside it is one this guard cannot
    // compare, and a guard that reports success for a file it never opened is
    // reporting the wrong thing. This is about being able to ask the question;
    // whether `docs/` *must* be bilingual is a policy `main.rs` holds.
    assert_eq!(
        pairs.len(),
        japanese,
        "{} Japanese page(s) have no English page beside them, so their \
         commands are compared against nothing",
        japanese - pairs.len()
    );

    let mut offenders: Vec<String> = Vec::new();
    let mut pairs_with_blocks: BTreeSet<String> = BTreeSet::new();
    let mut pairs_with_chains = 0usize;

    for (english, japanese) in &pairs {
        let en_name = shown(&root, english);
        let ja_name = shown(&root, japanese);
        let en = read(english);
        let ja = read(japanese);

        let en_blocks = block_commands(&en);
        let ja_blocks = block_commands(&ja);
        if !en_blocks.is_empty() {
            pairs_with_blocks.insert(en_name.clone());
        }

        offenders.extend(block_differences(
            &en_name, &en_blocks, &ja_name, &ja_blocks,
        ));

        let en_chains = chain_multiset(&en);
        let ja_chains = chain_multiset(&ja);
        if !en_chains.is_empty() || !ja_chains.is_empty() {
            pairs_with_chains += 1;
        }
        for (chain, (count, text)) in &en_chains {
            let there = ja_chains.get(chain).map(|(n, _)| *n).unwrap_or(0);
            if there != *count {
                offenders.push(format!("{en_name} chains `{text}`, {ja_name} does not"));
            }
        }
        for (chain, (count, text)) in &ja_chains {
            let there = en_chains.get(chain).map(|(n, _)| *n).unwrap_or(0);
            if there != *count {
                offenders.push(format!("{ja_name} chains `{text}`, {en_name} does not"));
            }
        }
    }

    // A walk that finds nothing passes, so say what was walked.
    // Named pages rather than a count. These three are corners of the corpus:
    // the contributor entry point, the reference page with the most blocks, and
    // a page under `grooveseek/examples/` that the walk has to reach.
    for required in [
        "CONTRIBUTING.md",
        "docs/usage.md",
        "grooveseek/examples/deployments/intranet-http/README.md",
    ] {
        assert!(
            pairs_with_blocks.contains(required),
            "no pair carrying shell blocks was found for {required}, so the \
             block half of this check compared nothing. Walked {} pair(s)",
            pairs.len()
        );
    }
    // The chain half needs its own floor. Fixing the copy this guard reports
    // removes the chain from CONTRIBUTING.ja.md, and docs/usage.md's two
    // service-removal chains are then the only ones left: if they go too, this
    // half starts comparing empty against empty and passes without looking.
    assert!(
        pairs_with_chains > 0,
        "no pair carried an inline command chain, so the chain half of this \
         check compared nothing at all"
    );

    offenders.sort();
    offenders.dedup();
    assert!(
        offenders.is_empty(),
        "these pages and their translations disagree about which commands to \
         run, so one language's readers are being told to do something the \
         other language stopped telling them:\n  {}\n\
         Carry the edit to both halves. A repair that lands on one twin leaves \
         the other reading like the correct instruction, which is how the \
         differences above got here.",
        offenders.join("\n  ")
    );
}

#[test]
fn the_japanese_half_of_a_pair_is_recognised_from_its_name() {
    let counterpart = english_counterpart(Path::new("docs/usage.ja.md"));
    assert_eq!(counterpart.as_deref(), Some(Path::new("docs/usage.md")));
    // An English page is not the Japanese half of anything.
    assert_eq!(english_counterpart(Path::new("docs/usage.md")), None);
    // Nor is a file that merely ends in the letters.
    assert_eq!(english_counterpart(Path::new("docs/ja.md")), None);
    assert_eq!(english_counterpart(Path::new("docs/notes.txt")), None);
}

#[test]
fn a_translated_comment_is_not_a_drifted_command() {
    let en = command_lines("cargo test --test index_progress_cli   # first, single-threaded\n");
    let ja = command_lines("cargo test --test index_progress_cli   # 先に、シングルスレッドで\n");
    assert_eq!(en, ja, "{en:?} vs {ja:?}");
}

#[test]
fn the_same_commands_in_a_different_order_are_a_difference() {
    // Membership alone would call these equal. The comparison has already
    // decided they are not, and the branch that decides what to say about it
    // must not be able to say nothing.
    let en = vec![(
        10usize,
        vec![
            "cargo test --test index_progress_cli".to_string(),
            "cargo test".to_string(),
        ],
    )];
    let ja = vec![(
        10usize,
        vec![
            "cargo test".to_string(),
            "cargo test --test index_progress_cli".to_string(),
        ],
    )];
    let found = block_differences("a.md", &en, "a.ja.md", &ja);
    assert_eq!(found.len(), 1, "{found:?}");
    assert!(found[0].contains("different order"), "{found:?}");
}

#[test]
fn a_command_repeated_on_one_side_only_is_a_difference() {
    let en = vec![(1usize, vec!["cargo test".to_string()])];
    let ja = vec![(
        1usize,
        vec!["cargo test".to_string(), "cargo test".to_string()],
    )];
    let found = block_differences("a.md", &en, "a.ja.md", &ja);
    assert_eq!(found.len(), 1, "{found:?}");
}

#[test]
fn a_missing_command_is_named_rather_than_summarised() {
    let en = vec![(1usize, vec!["cargo check --all-targets".to_string()])];
    let ja = vec![(1usize, vec!["cargo check".to_string()])];
    let found = block_differences("a.md", &en, "a.ja.md", &ja);
    assert_eq!(found.len(), 2, "{found:?}");
    assert!(
        found
            .iter()
            .any(|f| f.contains("cargo check --all-targets")),
        "{found:?}"
    );
}

#[test]
fn a_changed_flag_is_a_drifted_command() {
    let en = command_lines("cargo fmt --all -- --check\n");
    let ja = command_lines("cargo fmt --all\n");
    assert_ne!(en, ja);
}

#[test]
fn an_environment_assignment_is_part_of_the_instruction() {
    // `docs/usage.md` sets RUST_LOG on three lines. Stepping past the prefix to
    // find the program is right; dropping it from what gets compared would let
    // the Japanese page tell its readers to set a different level.
    let en = command_lines("RUST_LOG=grooveseek=debug groove serve --kb-path ./kb\n");
    let ja = command_lines("RUST_LOG=trace groove serve --kb-path ./kb\n");
    assert_ne!(en, ja, "{en:?}");
    assert_eq!(
        en,
        vec!["RUST_LOG=grooveseek=debug groove serve --kb-path ./kb".to_string()],
        "{en:?}"
    );
}

#[test]
fn the_user_a_command_runs_as_is_part_of_the_instruction() {
    let en = command_lines("sudo -u groove /usr/local/bin/groove index --kb-path /srv/kb\n");
    let ja = command_lines("sudo -u root /usr/local/bin/groove index --kb-path /srv/kb\n");
    assert_ne!(en, ja, "{en:?}");
}
