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
//! is found in more than one place, and that all of a group's members carry
//! the same value.
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
//! The one it most wants and does not have is
//! `.github/workflows/ci.yml`, which is what the CI command block is a copy
//! *of*. Both halves of that pin can drift away from the workflow together and
//! agree with each other the whole way down.

mod common;

use common::docs::{command_lines, markdown_files, pin_sites, repo_root};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

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
    },
];

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

/// A member's value, or the reason it could not be read as one.
fn normalise(shape: Shape, body: &str) -> Result<String, String> {
    match shape {
        Shape::ShellCommands => Ok(command_lines(body).join("\n")),
        Shape::Json => serde_json::from_str::<serde_json::Value>(body)
            .map(|value| value.to_string())
            .map_err(|e| format!("is not JSON: {e}")),
    }
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
    let sites = pin_sites("<!-- groove-pin: a -->\n<!-- groove-pin: b -->\n```bash\ncargo test\n```\n");
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
    let one_line = normalise(Shape::Json, "{ \"type\": \"command\", \"command\": \"groove index\" }");
    let spread = normalise(
        Shape::Json,
        "{\n  \"command\": \"groove index\",\n  \"type\": \"command\"\n}",
    );
    assert_eq!(one_line, spread);
    assert!(normalise(Shape::Json, "not json").is_err());
}
