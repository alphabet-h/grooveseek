//! The copies this repository cannot delete stay identical to each other.
//!
//! Some snippets are published in more than one place on purpose. `AGENTS.md`
//! and `CONTRIBUTING.md` both spell out the commands that reproduce CI, for a
//! reviewer and for a contributor. `README.md` shows the `.mcp.json` a reader
//! needs before they have any reason to open `docs/clients.md`.
//! `grooveseek/examples/` exists to be copied verbatim. Markdown has no
//! include, so "delete the copy and link to it" costs the reader the one thing
//! the copy is there to give them: the text in front of them.
//!
//! What is left is to stop them drifting. Each such copy carries a
//! `<!-- groove-pin: id -->` marker, invisible on GitHub, and every marker with
//! the same id has to hold the same thing.
//!
//! # What is checked
//!
//! That every marker resolves to a fenced block, that every id is registered
//! below with the reason its copies cannot be collapsed, that a registered id
//! is found in more than one place, that all of a group's members carry the
//! same value, and that a group which says it reproduces a workflow names the
//! same set of commands as that workflow's `run:` steps.
//!
//! Value, not text: `grooveseek/examples/hooks/settings.snippet.json` is a real
//! file the hook recipes point at, and it writes the same object across four
//! lines where the documentation writes it across one. Comparing the characters
//! would fail today and teach whoever hit it to reformat a working file.
//!
//! # What this cannot catch
//!
//! **A copy nobody marked.** This holds registered groups; it does not discover
//! new members. The three groups below were found by hand, and a fourth copy of
//! any of them, written without the marker, is invisible here. It is a pin, not
//! a detector.
//!
//! **What the workflow does around a command.** The CI block is compared with
//! the `run:` steps of `.github/workflows/ci.yml` as a set of commands, and
//! only that. Not the order: the block is written cheapest-first for a person
//! reproducing CI by hand, and the workflow's jobs run in parallel, so neither
//! order is the other's. Not the environment (`FASTEMBED_CACHE_DIR`), the OS
//! matrix, the `if:` on the doc step, or which job a command runs in -- a
//! command moved between jobs, or run on one OS out of three, compares equal.
//! `nightly.yml` and `release.yml` are compared with nothing here: the block
//! does not claim to reproduce them.

mod common;

use common::docs::{command_lines, markdown_files, pin_sites, read, repo_root, shown};
use common::workflow::{RunStep, run_steps};
use std::collections::{BTreeMap, BTreeSet};

/// How a group's members are compared.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Shape {
    /// Command lines, normalised the way the other command guards normalise
    /// them.
    ShellCommands,
    /// A JSON value. Formatting is not the contract; the object is.
    Json,
}

/// A copy that cannot be collapsed, and why not.
///
/// The reason is required, and it is printed in the failure. Removing a pin
/// should be an argument about that sentence rather than a deletion -- if the
/// reason has stopped being true, the copy should go, not the pin.
struct Pin {
    id: &'static str,
    why: &'static str,
    shape: Shape,
    /// The pages this group is known to have, by path from the repository root.
    ///
    /// Listed rather than counted, and required rather than exhaustive. A
    /// marker deleted from one member of a four-member group leaves three that
    /// still agree, so a check that only asks "two or more, and equal" reports
    /// success for a copy it has stopped watching. Extra members are welcome:
    /// a snippet copied along with its marker joins its own group and is
    /// compared from then on.
    members: &'static [&'static str],
    /// Members outside the Markdown corpus, by path from the repository root.
    /// The walk only collects `.md`, so these are read directly.
    beyond_markdown: &'static [&'static str],
    /// The workflows this block claims to reproduce, by path from the
    /// repository root. The block's set of commands has to equal the set of
    /// `run:` steps there, order aside: the block is written cheapest-first
    /// for a person, and the workflow's jobs run in parallel. Only a block of
    /// shell commands can claim this, and the registration is checked for it.
    reproduces: &'static [&'static str],
}

const PINS: &[Pin] = &[
    Pin {
        id: "ci-command-block",
        why: "AGENTS.md is what a reviewer reads and CONTRIBUTING.md is what a \
              contributor reads; a list of commands you are meant to run is \
              worth nothing behind a link to the other audience's document",
        shape: Shape::ShellCommands,
        members: &["AGENTS.md", "CONTRIBUTING.md"],
        beyond_markdown: &[],
        reproduces: &[".github/workflows/ci.yml"],
    },
    Pin {
        id: "mcp-stdio-snippet",
        why: "README.md has to get a reader running without leaving the page, \
              and docs/clients.md is where the same snippet is explained",
        shape: Shape::Json,
        members: &[
            "README.md",
            "README.ja.md",
            "docs/clients.md",
            "docs/clients.ja.md",
        ],
        beyond_markdown: &[],
        reproduces: &[],
    },
    Pin {
        id: "posttooluse-hook-snippet",
        why: "grooveseek/examples/ exists to be copied verbatim, so a recipe \
              that links elsewhere for its own contents has stopped being a \
              recipe; settings.snippet.json is the file those recipes point at",
        shape: Shape::Json,
        members: &[
            "docs/clients.md",
            "docs/clients.ja.md",
            "grooveseek/examples/hooks/README.md",
            "grooveseek/examples/hooks/README.ja.md",
        ],
        beyond_markdown: &["grooveseek/examples/hooks/settings.snippet.json"],
        reproduces: &[],
    },
];

/// A member's value, or the reason it could not be read as one.
fn normalise(shape: Shape, body: &str) -> Result<String, String> {
    match shape {
        Shape::ShellCommands => Ok(command_lines(body).join("\n")),
        Shape::Json => serde_json::from_str::<serde_json::Value>(body)
            .map(|value| value.to_string())
            .map_err(|e| format!("is not JSON: {e}")),
    }
}

/// Everything a block and the workflow it claims to reproduce disagree about,
/// one line each, naming the command, the side that has it alone, and where.
///
/// A set on each side, not a sequence and not a multiset. Two jobs that run
/// the same command are one thing a reader is told to run, and the block's
/// own order is compared with its copies elsewhere, not with the workflow.
///
/// A step whose `run:` yields no command, and a line of a `run:` the reader
/// could not place, are reported rather than skipped: either one is a command
/// the workflow may run that is being compared with nothing. Returned rather
/// than asserted so the fixtures below run the same predicate as the corpus.
fn reproduction_gap(
    id: &str,
    why: &str,
    block_where: &str,
    block_value: &str,
    workflow: &str,
    steps: &[RunStep],
) -> Vec<String> {
    let mut out = Vec::new();
    let block: BTreeSet<&str> = block_value.lines().collect();
    let mut ran: BTreeMap<&str, Vec<String>> = BTreeMap::new();
    for step in steps {
        if step.commands.is_empty() {
            out.push(format!(
                "{workflow}: pin `{id}`: {} has a `run:` that reads as no \
                 command, so whatever it runs is not being compared with \
                 {block_where}",
                step.position()
            ));
        }
        for line in &step.unread {
            out.push(format!(
                "{workflow}: pin `{id}`: {}, line {} of its `run:`, `{}`: {}. \
                 A line the reader cannot place is not being compared with \
                 {block_where}, so it is reported here rather than dropped",
                step.position(),
                line.index + 1,
                line.raw.trim(),
                line.why
            ));
        }
        for command in &step.commands {
            ran.entry(command.as_str())
                .or_default()
                .push(step.position());
        }
    }
    for command in block.iter().filter(|c| !ran.contains_key(*c)) {
        out.push(format!(
            "pin `{id}`: {block_where} tells a reader to run `{command}`, and \
             {workflow} has no `run:` step that runs it. Either the block is \
             stale or CI stopped checking it; carry the edit to every member \
             of the block, or to the workflow. Registered because: {why}"
        ));
    }
    for (command, at) in ran.iter().filter(|(c, _)| !block.contains(*c)) {
        out.push(format!(
            "pin `{id}`: {workflow} runs `{command}` at {} and {block_where} \
             does not list it. The block says it is everything CI checks; add \
             the command to every member, or, if it is setup a contributor \
             already has rather than a check, teach this reader to tell the \
             two apart here rather than dropping the step from the comparison",
            at.join(", ")
        ));
    }
    out
}

#[test]
fn every_copy_that_cannot_be_deleted_is_pinned_to_the_others() {
    let root = repo_root();
    let files = markdown_files(&root);

    let mut groups: BTreeMap<String, Vec<(String, String)>> = BTreeMap::new();
    let mut pages_by_id: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut unresolved: Vec<String> = Vec::new();

    for file in &files {
        let name = shown(&root, file);
        let markdown = read(file);
        for site in pin_sites(&markdown) {
            let where_ = format!("{name}:{}", site.line);
            match site.block {
                Some(block) => {
                    pages_by_id
                        .entry(site.id.clone())
                        .or_default()
                        .insert(name.clone());
                    groups
                        .entry(site.id)
                        .or_default()
                        .push((where_, block.body));
                }
                None => unresolved.push(format!("{where_}  {}", site.id)),
            }
        }
    }

    let registered: BTreeMap<&str, &Pin> = PINS.iter().map(|p| (p.id, p)).collect();

    let mut offenders: Vec<String> = Vec::new();

    for site in &unresolved {
        let id = site.split_whitespace().last().unwrap_or_default();
        let why = registered
            .get(id)
            .map(|p| p.why)
            .unwrap_or("this id is not registered");
        offenders.push(format!(
            "{site}: the marker is followed by something other than a fenced \
             code block. A pin names the block on the next line; if the block \
             moved, move the marker with it, and if the copy is gone, delete \
             the pin in the same commit. Registered because: {why}"
        ));
    }

    for (id, members) in &groups {
        let Some(pin) = registered.get(id.as_str()) else {
            offenders.push(format!(
                "{id}: found at {} and registered nowhere. Add it to PINS with \
                 the reason its copies cannot be collapsed into one, or delete \
                 the copy and link to the original instead",
                members
                    .iter()
                    .map(|(w, _)| w.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
            continue;
        };

        let mut values: Vec<(String, String)> = Vec::new();
        for (where_, body) in members {
            match normalise(pin.shape, body) {
                Ok(value) if value.trim().is_empty() => offenders.push(format!(
                    "{where_}: pin `{id}` reads as nothing. Two empty values \
                     always match, so this member is checking nothing"
                )),
                Ok(value) => values.push((where_.clone(), value)),
                Err(why) => offenders.push(format!("{where_}: pin `{id}` {why}")),
            }
        }
        for extra in pin.beyond_markdown {
            let path = root.join(extra);
            assert!(
                path.is_file(),
                "pin `{id}` names {extra}, which is not in the tree. \
                 Registered because: {}",
                pin.why
            );
            match normalise(pin.shape, &read(&path)) {
                Ok(value) => values.push(((*extra).to_string(), value)),
                Err(why) => offenders.push(format!("{extra}: pin `{id}` {why}")),
            }
        }

        if values.len() < 2 {
            offenders.push(format!(
                "pin `{id}` holds {} member(s), so it is comparing a copy \
                 against nothing. Registered because: {}",
                values.len(),
                pin.why
            ));
            continue;
        }
        let (first_where, first_value) = &values[0];
        for (where_, value) in &values[1..] {
            if value != first_value {
                offenders.push(format!(
                    "pin `{id}`: {where_} and {first_where} have drifted apart. \
                     Registered because: {}",
                    pin.why
                ));
            }
        }

        // Against the first member only. The loop above has already reported
        // every other member that differs from it, so a comparison of each
        // member with the workflow would repeat one gap once per copy.
        for workflow in pin.reproduces {
            assert_eq!(
                pin.shape,
                Shape::ShellCommands,
                "pin `{id}` says it reproduces {workflow}, and only a block of \
                 shell commands can. Registered because: {}",
                pin.why
            );
            let path = root.join(workflow);
            assert!(
                path.is_file(),
                "pin `{id}` says it reproduces {workflow}, which is not in the \
                 tree. Registered because: {}",
                pin.why
            );
            match run_steps(&read(&path)) {
                Ok(steps) => offenders.extend(reproduction_gap(
                    id,
                    pin.why,
                    first_where,
                    first_value,
                    workflow,
                    &steps,
                )),
                Err(why) => offenders.push(format!("{workflow}: pin `{id}` {why}")),
            }
        }
    }

    // A walk that finds nothing passes, so every registered pin has to have
    // been seen. A typo in an id would otherwise leave its group silently
    // unchecked while the marker sat in the page looking like protection.
    // A member that lost its marker leaves the rest of its group agreeing with
    // each other, so "two or more, and equal" would report success for a copy
    // that had stopped being watched.
    for pin in PINS {
        let seen = pages_by_id.get(pin.id).cloned().unwrap_or_default();
        for member in pin.members {
            if !seen.contains(*member) {
                offenders.push(format!(
                    "pin `{}` names {member}, and no marker for it was found \
                     there. If the copy is gone, take the page out of PINS in \
                     the same commit; if it is still there, it is no longer \
                     being compared. Registered because: {}",
                    pin.id, pin.why
                ));
            }
        }
    }

    for pin in PINS {
        assert!(
            groups.contains_key(pin.id),
            "pin `{}` is registered and no marker for it was found in the \
             corpus. Either the marker was removed without its registration, \
             or the id does not match what the page carries. Registered \
             because: {}",
            pin.id,
            pin.why
        );
    }

    offenders.sort();
    offenders.dedup();
    assert!(
        offenders.is_empty(),
        "these copies are published in more than one place on purpose, and \
         they no longer agree:\n  {}\n\
         Carry the edit to every member. Relaxing this check restores the \
         state where the second copy was quietly the older one.",
        offenders.join("\n  ")
    );
}

// ---------------------------------------------------------------------------
// The resolution rule, against pulldown-cmark rather than against reasoning
// about it. Every shape below was run through the parser at the version
// Cargo.lock pins before it was written down.
// ---------------------------------------------------------------------------

#[test]
fn a_marker_names_the_block_on_the_next_line() {
    let sites = pin_sites("<!-- groove-pin: x -->\n```bash\ncargo test\n```\n");
    assert_eq!(sites.len(), 1);
    assert_eq!(sites[0].id, "x");
    assert_eq!(
        sites[0].block.as_ref().map(|b| b.body.as_str()),
        Some("cargo test\n")
    );
}

#[test]
fn a_blank_line_between_the_marker_and_the_block_is_still_the_next_line() {
    // The parser emits the same events either way; only the offsets move.
    let sites = pin_sites("<!-- groove-pin: x -->\n\n```bash\ncargo test\n```\n");
    assert!(sites[0].block.is_some(), "{sites:?}");
}

#[test]
fn a_paragraph_between_them_is_not() {
    let sites = pin_sites("<!-- groove-pin: x -->\n\nwords\n\n```bash\ncargo test\n```\n");
    assert_eq!(sites.len(), 1);
    assert!(sites[0].block.is_none(), "{sites:?}");
}

#[test]
fn a_marker_written_into_a_sentence_resolves_to_nothing() {
    // Inline HTML never opens an HTML block, so it can never be adjacent to
    // one. Reported rather than ignored: a marker that looks like protection
    // and provides none is worse than no marker.
    let sites = pin_sites("prose <!-- groove-pin: x --> more\n\n```bash\ncargo test\n```\n");
    assert_eq!(sites.len(), 1);
    assert!(sites[0].block.is_none(), "{sites:?}");
}

#[test]
fn two_markers_on_one_block_leave_the_first_one_pointing_at_nothing() {
    let sites =
        pin_sites("<!-- groove-pin: a -->\n<!-- groove-pin: b -->\n```bash\ncargo test\n```\n");
    assert_eq!(sites.len(), 2);
    let a = sites.iter().find(|s| s.id == "a").expect("a");
    let b = sites.iter().find(|s| s.id == "b").expect("b");
    assert!(a.block.is_none(), "{sites:?}");
    assert!(b.block.is_some(), "{sites:?}");
}

#[test]
fn a_marker_inside_a_list_item_names_the_block_in_the_same_item() {
    // CHANGELOG.md indents a fence inside a list item, so this shape is not
    // hypothetical.
    let sites = pin_sites("- item\n\n  <!-- groove-pin: x -->\n  ```bash\n  cargo test\n  ```\n");
    assert_eq!(sites.len(), 1);
    assert!(sites[0].block.is_some(), "{sites:?}");
}

#[test]
fn a_marker_outside_the_list_does_not_reach_a_block_inside_it() {
    let sites = pin_sites("<!-- groove-pin: x -->\n\n- item\n\n  ```bash\n  cargo test\n  ```\n");
    assert!(sites[0].block.is_none(), "{sites:?}");
}

#[test]
fn an_indented_block_is_not_the_block_a_pin_can_name() {
    // `Tag::CodeBlock` covers the indented form too. These guards do not read
    // indented blocks, and a pin that reached one would hand a body with no
    // language tag to a reader expecting a tagged one.
    let sites = pin_sites("<!-- groove-pin: x -->\n\n    cargo test\n");
    assert_eq!(sites.len(), 1);
    assert!(sites[0].block.is_none(), "{sites:?}");
}

#[test]
fn json_members_are_compared_as_values_and_not_as_characters() {
    let one_line = normalise(
        Shape::Json,
        "{ \"type\": \"command\", \"command\": \"groove index\" }",
    );
    let spread = normalise(
        Shape::Json,
        "{\n  \"command\": \"groove index\",\n  \"type\": \"command\"\n}",
    );
    assert_eq!(one_line, spread);
    assert!(normalise(Shape::Json, "not json").is_err());
}

// ---------------------------------------------------------------------------
// The workflow side. A pass over the corpus proves nothing about the reader
// if the workflow happens to be the shape it expects, so the anchor below
// names what it expects, and the fixtures run the same predicate the corpus
// runs on shapes the corpus does not have today.
// ---------------------------------------------------------------------------

/// The workflow the CI block reproduces is still one this reader reads.
///
/// Named jobs rather than a count, for the reason `docs_commands_subset.rs`
/// names its pages: "some step somewhere was read" still passes when the
/// reader stops understanding a job. `cargo test` is named literally because
/// the block and the workflow losing that line *together* is a set equality
/// that holds, and this is the one place that would notice.
#[test]
fn the_ci_workflow_is_still_the_shape_this_reader_reads() {
    let path = repo_root().join(".github/workflows/ci.yml");
    let steps =
        run_steps(&read(&path)).unwrap_or_else(|why| panic!(".github/workflows/ci.yml {why}"));
    for job in ["test", "clippy", "fmt"] {
        assert!(
            steps.iter().any(|s| s.job == job),
            "no `run:` step was read from jobs.{job} of .github/workflows/ci.yml. \
             If the job was renamed, rename it here; if its checks moved to \
             `uses:`, the reader has stopped seeing them and the block is \
             being compared with less than CI runs. Read: {:?}",
            steps.iter().map(RunStep::position).collect::<Vec<_>>()
        );
    }
    assert!(
        steps
            .iter()
            .any(|s| s.commands.iter().any(|c| c == "cargo test")),
        "no `run:` step of .github/workflows/ci.yml reads as `cargo test`. The \
         comparison with the block passes when both sides lose a line; this \
         is the one that notices. Read: {:?}",
        steps
            .iter()
            .flat_map(|s| s.commands.clone())
            .collect::<Vec<_>>()
    );
}

/// The reader's output for a one-job, one-step workflow around `run`.
fn one_run(run: &str) -> Result<Vec<RunStep>, String> {
    run_steps(&format!(
        "jobs:\n  x:\n    steps:\n      - uses: a/b@v1\n      - run: {run}\n"
    ))
}

#[test]
fn a_run_step_goes_through_the_same_normaliser_as_a_fenced_block() {
    // A comment, a blank, a continuation and a heredoc, in one block scalar.
    let body =
        "# setup\ncargo check \\\n  --all-targets\n\ncat <<EOF\nnot a command\nEOF\ncargo test\n";
    let indented = body
        .lines()
        .map(|l| format!("          {l}"))
        .collect::<Vec<_>>()
        .join("\n");
    let steps = one_run(&format!("|\n{indented}\n")).expect("reads");
    assert_eq!(steps.len(), 1, "{steps:?}");
    assert_eq!(steps[0].commands, command_lines(body));
    // The opener is a command (`cat`); its payload and terminator are not.
    assert_eq!(
        steps[0].commands,
        vec![
            "cargo check --all-targets".to_string(),
            "cat <<EOF".to_string(),
            "cargo test".to_string()
        ]
    );
    assert!(steps[0].unread.is_empty(), "{:?}", steps[0].unread);
    assert_eq!(steps[0].position(), "jobs.x.steps[1]");
}

#[test]
fn a_folded_run_is_one_command() {
    let steps = one_run(">\n          cargo test\n          --bench a\n          --bench b\n")
        .expect("reads");
    assert_eq!(
        steps[0].commands,
        vec!["cargo test --bench a --bench b".to_string()]
    );
}

#[test]
fn a_named_step_is_located_by_index_and_name() {
    let steps = run_steps(
        "jobs:\n  fmt:\n    steps:\n      - name: check formatting\n        run: cargo fmt\n",
    )
    .expect("reads");
    assert_eq!(steps[0].position(), "jobs.fmt.steps[0] (check formatting)");
}

#[test]
fn a_step_with_neither_run_nor_uses_is_reported_not_skipped() {
    let err =
        run_steps("jobs:\n  x:\n    steps:\n      - run: cargo test\n      - name: nothing\n")
            .expect_err("a step that runs nothing is not a step this reader knows");
    assert!(err.contains("jobs.x.steps[1]"), "{err}");
    assert!(err.contains("neither `run` nor `uses`"), "{err}");
}

#[test]
fn a_step_with_both_run_and_uses_is_reported() {
    let err = run_steps("jobs:\n  x:\n    steps:\n      - uses: a/b@v1\n        run: cargo test\n")
        .expect_err("both");
    assert!(err.contains("jobs.x.steps[0]"), "{err}");
    assert!(err.contains("both `run` and `uses`"), "{err}");
}

#[test]
fn a_workflow_without_jobs_is_reported() {
    let err = run_steps("name: CI\non: push\njob:\n  x:\n    steps: []\n").expect_err("no jobs");
    assert!(err.contains("no `jobs` mapping"), "{err}");
    let err = run_steps("jobs:\n  x:\n    uses: ./.github/workflows/other.yml\n")
        .expect_err("a job without steps");
    assert!(err.contains("jobs.x has no `steps` sequence"), "{err}");
}

#[test]
fn the_top_level_on_key_does_not_break_the_read() {
    // YAML 1.1 reads a bare `on` as a boolean; the parser here keeps it a
    // string, and either way `jobs` is looked up by name.
    let steps = run_steps(
        "on:\n  push:\n    branches: [main]\njobs:\n  x:\n    steps:\n      - run: cargo test\n",
    )
    .expect("reads");
    assert_eq!(steps[0].commands, vec!["cargo test".to_string()]);
}

#[test]
fn a_run_that_reads_as_nothing_is_reported() {
    let steps = one_run("\"# only a comment\"").expect("reads");
    assert!(steps[0].commands.is_empty(), "{steps:?}");
    let gap = reproduction_gap("p", "why", "A.md:1", "cargo test", "w.yml", &steps);
    // Two lines: the step that reads as nothing, and the block's command that
    // no step runs. The second is true on its own; the first says why.
    assert_eq!(gap.len(), 2, "{gap:?}");
    assert!(
        gap.iter()
            .any(|g| g.contains("jobs.x.steps[1] has a `run:` that reads as no command")),
        "{gap:?}"
    );
    assert!(
        gap.iter()
            .any(|g| g.contains("A.md:1 tells a reader to run `cargo test`")),
        "{gap:?}"
    );
}

#[test]
fn a_line_of_a_run_the_reader_cannot_place_is_reported() {
    // `%%%` cannot start a command. The step still yields `cargo test`, so a
    // reader that dropped the line would compare equal and say nothing.
    let steps = one_run("|\n          cargo test\n          %%% not shell\n").expect("reads");
    assert_eq!(steps[0].commands, vec!["cargo test".to_string()]);
    assert_eq!(steps[0].unread.len(), 1, "{:?}", steps[0].unread);
    let gap = reproduction_gap("p", "why", "A.md:1", "cargo test", "w.yml", &steps);
    assert_eq!(gap.len(), 1, "{gap:?}");
    assert!(
        gap[0].contains("jobs.x.steps[1], line 2 of its `run:`, `%%% not shell`"),
        "{gap:?}"
    );
    assert!(gap[0].contains("cannot start a command"), "{gap:?}");
}

#[test]
fn order_between_the_block_and_the_workflow_is_not_a_difference() {
    let steps = run_steps(
        "jobs:\n  a:\n    steps:\n      - run: cargo test\n  b:\n    steps:\n      - run: cargo fmt\n      - run: cargo test\n",
    )
    .expect("reads");
    // The block lists them in the other order, and one of them twice in the
    // workflow: neither is a gap.
    let gap = reproduction_gap(
        "p",
        "why",
        "A.md:1",
        "cargo fmt\ncargo test",
        "w.yml",
        &steps,
    );
    assert!(gap.is_empty(), "{gap:?}");
}

#[test]
fn a_command_on_one_side_only_names_the_side_and_the_step() {
    let steps = run_steps(
        "jobs:\n  x:\n    steps:\n      - run: cargo test\n      - name: audit\n        run: cargo deny check\n",
    )
    .expect("reads");
    let gap = reproduction_gap(
        "p",
        "why",
        "A.md:1",
        "cargo test\ncargo doc",
        "w.yml",
        &steps,
    );
    assert_eq!(gap.len(), 2, "{gap:?}");
    let docs_only = gap
        .iter()
        .find(|g| g.contains("A.md:1 tells a reader to run `cargo doc`"))
        .unwrap_or_else(|| panic!("{gap:?}"));
    assert!(
        docs_only.contains("w.yml has no `run:` step that runs it"),
        "{docs_only}"
    );
    let ci_only = gap
        .iter()
        .find(|g| g.contains("w.yml runs `cargo deny check` at jobs.x.steps[1] (audit)"))
        .unwrap_or_else(|| panic!("{gap:?}"));
    assert!(ci_only.contains("A.md:1 does not list it"), "{ci_only}");
}
