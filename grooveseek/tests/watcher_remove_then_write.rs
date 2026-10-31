//! Issue #543: a document deleted and written again under the same name keeps its stored
//! vectors through the live watcher.
//!
//! Some editors save by removing the file and writing a new one (Claude Code's Edit tool arrives
//! at the watcher as a `Remove` and a `Create` of the same path). The watcher used to deindex on
//! the `Remove` first, so the reindex that followed found no stored vector to reuse and embedded
//! every chunk again. Now a removal whose path holds an indexable file again by the time the
//! event is handled is reindexed in place, and the unchanged chunks keep their vectors.
//!
//! Both tests drive `groove serve` with the watcher on against the mock endpoint of
//! [`crate::common::embed_cli`], so nothing downloads a model.

mod common;

use common::ansi::strip_ansi;
use common::embed_cli::{Fixture, fixture};
use common::embed_mock::{DOC_MODEL, hermetic, wait_until};
use common::mcp::{ServerGuard, spawn_serve_with};
use std::time::Duration;

const RUNBOOK: &str = "runbook.md";

const SECTIONS: [(&str, &str); 4] = [
    (
        "Install",
        "Download the archive and unpack it into the tools folder before anything else.",
    ),
    (
        "Configure",
        "Point the config file at the shared cache directory on the build machine.",
    ),
    (
        "Verify",
        "Run the smoke check against the staging endpoint and compare the counts.",
    ),
    (
        "Rollback",
        "Restore the previous release from the archive when the smoke check fails.",
    ),
];
const ADDED: (&str, &str) = (
    "Monitor",
    "Watch the queue depth dashboard during the first hour after rollout.",
);

/// A Markdown document titled `Runbook` with one `##` section per `(heading, body)`.
fn runbook(sections: &[(&str, &str)]) -> String {
    let mut md = String::from("---\ntitle: Runbook\n---\n\n");
    for (heading, body) in sections {
        md.push_str(&format!("## {heading}\n\n{body}\n\n"));
    }
    md
}

/// A knowledge base holding [`RUNBOOK`], indexed once, served with the watcher on.
fn served_runbook(prefix: &str) -> (Fixture, ServerGuard) {
    let fx = fixture(
        prefix,
        &[(RUNBOOK, &runbook(&SECTIONS))],
        "\n[watch]\nenabled = true\ndebounce_ms = 300\n",
    );
    fx.index();
    // `spawn_serve_with(.., true, ..)` returns only after `watcher: watching`, so the edits
    // below cannot fall before the debouncer is armed.
    let (guard, _base) = spawn_serve_with(fx.kb(), &fx.config, true, |c| {
        hermetic(c, &fx.cache);
    });
    (fx, guard)
}

fn stderr_lines(guard: &ServerGuard) -> Vec<String> {
    guard
        .stderr()
        .lines()
        .iter()
        .map(|l| strip_ansi(l))
        .collect()
}

#[test]
fn a_file_removed_and_written_again_is_reindexed_with_its_vectors_reused() {
    let (fx, guard) = served_runbook("i543-replace");
    let before = fx.mock.requests().len();

    // Remove, then write under the same name: the order an editor that saves by replacing the
    // file produces. One section is added so the reindex has exactly one chunk to embed.
    let path = fx.kb().join(RUNBOOK);
    std::fs::remove_file(&path).expect("remove the runbook");
    let mut sections = SECTIONS.to_vec();
    sections.push(ADDED);
    fx.layout.write(RUNBOOK, &runbook(&sections));

    let reindexed = wait_until(Duration::from_secs(30), || {
        stderr_lines(&guard)
            .iter()
            .any(|l| l.contains("watcher: reindexed runbook.md"))
    });
    let lines = stderr_lines(&guard);
    assert!(
        reindexed,
        "no `watcher: reindexed runbook.md` line:\n{}",
        lines.join("\n")
    );
    assert!(
        !lines
            .iter()
            .any(|l| l.contains("watcher: deindexed runbook.md")),
        "the file was there again when the removal was handled, yet it was deindexed:\n{}",
        lines.join("\n")
    );
    let line = lines
        .iter()
        .find(|l| l.contains("watcher: reindexed runbook.md"))
        .expect("checked above");
    assert!(
        line.contains("(5 chunks, 4 reused)"),
        "the unchanged sections were not reused: {line}\n{}",
        lines.join("\n")
    );

    let sent: Vec<String> = fx
        .requests_since(before)
        .iter()
        .filter(|r| r.model() == Some(DOC_MODEL))
        .flat_map(|r| r.inputs())
        .collect();
    assert_eq!(
        sent.len(),
        1,
        "only the added section is embedded: {sent:?}"
    );
    assert!(sent[0].contains(ADDED.1), "{sent:?}");
    drop(guard);
}

#[test]
fn a_file_that_stays_removed_is_still_deindexed() {
    let (fx, guard) = served_runbook("i543-remove");

    std::fs::remove_file(fx.kb().join(RUNBOOK)).expect("remove the runbook");

    let deindexed = wait_until(Duration::from_secs(30), || {
        stderr_lines(&guard)
            .iter()
            .any(|l| l.contains("watcher: deindexed runbook.md"))
    });
    let lines = stderr_lines(&guard);
    assert!(
        deindexed,
        "no `watcher: deindexed runbook.md` line:\n{}",
        lines.join("\n")
    );
    assert!(
        !lines
            .iter()
            .any(|l| l.contains("watcher: reindexed runbook.md")),
        "{}",
        lines.join("\n")
    );
    drop(guard);
    assert_eq!(fx.documents(), 0, "the removed document is still indexed");
}

/// The line the watcher prints when a removed path holds a file again that the reindex did
/// not index, and the rows were dropped as a removal.
const NOT_INDEXED_LINE: &str =
    "watcher: runbook.md was removed and its replacement was not indexed";

/// (#543, GitHub codex r1 P2) Remove [`RUNBOOK`], write `replacement` under the same name, and
/// require the old document to be gone from the index: a replacement the reindex skips (an
/// empty file, a frontmatter-only stub) leaves no row standing for the old text.
///
/// Not on macOS: there FSEvents folds the removal and the write into one change (kuriya trap
/// #525), so the watcher never sees a removal and this path is not taken. PR #393's macOS CI
/// job showed it: its stderr carried only `watcher: skipped runbook.md (no embeddable chunks)`
/// lines, no removal line. What a macOS save of this shape does is covered by
/// [`an_in_place_write_of_an_unindexable_file_drops_the_row`].
#[cfg(not(target_os = "macos"))]
fn a_removal_replaced_by_an_unindexable_file_drops_the_row(prefix: &str, replacement: &str) {
    let (fx, guard) = served_runbook(prefix);
    assert_eq!(fx.documents(), 1);

    std::fs::remove_file(fx.kb().join(RUNBOOK)).expect("remove the runbook");
    fx.layout.write(RUNBOOK, replacement);

    let reported = wait_until(Duration::from_secs(30), || {
        stderr_lines(&guard)
            .iter()
            .any(|l| l.contains(NOT_INDEXED_LINE))
    });
    let lines = stderr_lines(&guard);
    drop(guard);
    assert_eq!(
        fx.documents(),
        0,
        "the removed document's old chunks are still indexed:\n{}",
        lines.join("\n")
    );
    assert!(
        reported,
        "no `{NOT_INDEXED_LINE}` line:\n{}",
        lines.join("\n")
    );
    assert!(
        !lines
            .iter()
            .any(|l| l.contains("watcher: reindexed runbook.md")),
        "{}",
        lines.join("\n")
    );
}

#[test]
#[cfg(not(target_os = "macos"))]
fn a_file_removed_and_written_again_empty_is_deindexed() {
    a_removal_replaced_by_an_unindexable_file_drops_the_row("i543-empty", "");
}

#[test]
#[cfg(not(target_os = "macos"))]
fn a_file_removed_and_written_again_as_a_frontmatter_stub_is_deindexed() {
    a_removal_replaced_by_an_unindexable_file_drops_the_row(
        "i543-stub",
        "---\ntitle: Runbook\n---\n",
    );
}

/// The line the watcher prints when a create, modify or rename finds a file it had indexed
/// holding nothing to embed, and drops its rows.
const EMPTIED_LINE: &str = "watcher: runbook.md has nothing to embed now; deindexed it";

/// (#543) Write `replacement` over [`RUNBOOK`] in place -- no removal -- and require the old
/// document to be gone from the index, as `groove index --force` leaves no row for such a file.
/// This is also what a remove-and-write becomes on macOS, where FSEvents folds the two into one
/// change (kuriya trap #525).
fn an_in_place_write_of_an_unindexable_file_drops_the_row(prefix: &str, replacement: &str) {
    let (fx, guard) = served_runbook(prefix);
    assert_eq!(fx.documents(), 1);

    fx.layout.write(RUNBOOK, replacement);

    let handled = wait_until(Duration::from_secs(30), || {
        stderr_lines(&guard)
            .iter()
            .any(|l| l.contains(EMPTIED_LINE) || l.contains("watcher: skipped runbook.md"))
    });
    let lines = stderr_lines(&guard);
    drop(guard);
    assert!(
        handled,
        "the watcher never handled the write:\n{}",
        lines.join("\n")
    );
    assert_eq!(
        fx.documents(),
        0,
        "the earlier text's chunks are still indexed:\n{}",
        lines.join("\n")
    );
    assert!(
        lines.iter().any(|l| l.contains(EMPTIED_LINE)),
        "no `{EMPTIED_LINE}` line:\n{}",
        lines.join("\n")
    );
    assert!(
        !lines.iter().any(|l| l.contains(NOT_INDEXED_LINE)),
        "nothing was removed, yet the line says so:\n{}",
        lines.join("\n")
    );
}

#[test]
fn a_file_emptied_in_place_is_deindexed() {
    an_in_place_write_of_an_unindexable_file_drops_the_row("i543-inplace-empty", "");
}

#[test]
fn a_file_cut_down_to_its_frontmatter_in_place_is_deindexed() {
    an_in_place_write_of_an_unindexable_file_drops_the_row(
        "i543-inplace-stub",
        "---\ntitle: Runbook\n---\n",
    );
}

/// The body that replaces the `Verify` section's in
/// [`the_watcher_sends_only_the_edited_chunk_and_names_the_reused_ones`].
#[cfg(not(target_os = "macos"))]
const EDITED_VERIFY: &str =
    "Run the smoke check against the canary endpoint and diff the latency charts.";

/// (kuriya 606, AX-07) Edit one section's body in place -- the same file truncated and written,
/// no removal, no rename -- and require the reindex to embed that one chunk and reuse the stored
/// vectors of the others (feature-69). The removal path's reuse is pinned by
/// [`a_file_removed_and_written_again_is_reindexed_with_its_vectors_reused`]; this one walks
/// `dispatch_reindex` -> `reindex_after_write_locked` (private, in [`grooveseek::watcher`]) and
/// edits a section instead of adding one.
///
/// Not on macOS: FSEvents folds and reorders events there (kuriya traps #525 / #591), so how
/// many reindexes an in-place write produces, and what each one finds stored, is not what this
/// test pins. The reuse planning itself is the same code on every OS.
#[test]
#[cfg(not(target_os = "macos"))]
fn the_watcher_sends_only_the_edited_chunk_and_names_the_reused_ones() {
    let (fx, guard) = served_runbook("ax07-edit");
    let before = fx.mock.requests().len();

    let mut sections = SECTIONS.to_vec();
    sections[2].1 = EDITED_VERIFY;
    // `std::fs::write` truncates and writes the same file: a modify, not a remove and create.
    std::fs::write(fx.kb().join(RUNBOOK), runbook(&sections)).expect("edit the runbook");

    let reindexed = wait_until(Duration::from_secs(30), || {
        stderr_lines(&guard)
            .iter()
            .any(|l| l.contains("watcher: reindexed runbook.md"))
    });
    let lines = stderr_lines(&guard);
    assert!(
        reindexed,
        "no `watcher: reindexed runbook.md` line:\n{}",
        lines.join("\n")
    );
    assert!(
        !lines
            .iter()
            .any(|l| l.contains("watcher: deindexed runbook.md") || l.contains(NOT_INDEXED_LINE)),
        "an in-place edit went down the removal path:\n{}",
        lines.join("\n")
    );
    let line = lines
        .iter()
        .find(|l| l.contains("watcher: reindexed runbook.md"))
        .expect("checked above");
    let expected = format!("({} chunks, {} reused)", SECTIONS.len(), SECTIONS.len() - 1);
    assert!(
        line.contains(&expected),
        "the unedited sections were not reused, want `{expected}`: {line}\n{}",
        lines.join("\n")
    );

    let sent: Vec<String> = fx
        .requests_since(before)
        .iter()
        .filter(|r| r.model() == Some(DOC_MODEL))
        .flat_map(|r| r.inputs())
        .collect();
    assert_eq!(
        sent.len(),
        1,
        "only the edited section is embedded: {sent:?}"
    );
    // The one input must be exactly the edited chunk's: the `Verify` input the first index sent,
    // with the old body swapped for the new one. A regression that packed the unchanged sections
    // into the same input would still send one input, so `contains` alone would not catch it.
    let indexed: Vec<String> = fx.mock.requests()[..before]
        .iter()
        .filter(|r| r.model() == Some(DOC_MODEL))
        .flat_map(|r| r.inputs())
        .collect();
    let old_verify: Vec<&String> = indexed
        .iter()
        .filter(|i| i.contains(SECTIONS[2].1))
        .collect();
    assert_eq!(
        old_verify.len(),
        1,
        "the first index sent one `Verify` input: {indexed:?}"
    );
    let want = old_verify[0].replace(SECTIONS[2].1, EDITED_VERIFY);
    assert_eq!(sent[0], want, "the input is not exactly the edited chunk");
    for (heading, body) in SECTIONS.iter().filter(|(h, _)| *h != "Verify") {
        assert!(
            !sent[0].contains(body),
            "the unchanged `{heading}` section was sent again: {sent:?}"
        );
    }
    drop(guard);

    // The reindex replaced the edited chunk rather than adding one beside the old text.
    assert_eq!(fx.documents(), 1, "the edited document is not indexed");
    assert_eq!(
        fx.chunks(),
        SECTIONS.len() as u64,
        "the edit left stale or duplicate chunks in the index"
    );
    let hits = fx.search_json("smoke check endpoint");
    let contents: Vec<&str> = hits
        .pointer("/results")
        .and_then(|v| v.as_array())
        .unwrap_or_else(|| panic!("no results array: {hits}"))
        .iter()
        .filter_map(|h| h.pointer("/content").and_then(|v| v.as_str()))
        .collect();
    assert!(
        contents.iter().any(|c| c.contains(EDITED_VERIFY)),
        "the edited `Verify` text is not searchable: {hits}"
    );
    assert!(
        !contents.iter().any(|c| c.contains(SECTIONS[2].1)),
        "the old `Verify` text is still indexed: {hits}"
    );
}
