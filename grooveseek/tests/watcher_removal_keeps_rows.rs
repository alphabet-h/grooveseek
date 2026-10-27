//! (kuriya 548, kuriya 549) A remove event leaves a document's rows alone when the disk may
//! still hold the file.
//!
//! Some editors and scripts save by removing a file and writing a new one under the same name,
//! so the live watcher sees a removal for a path that is in fact there again. Two cases dropped
//! the old document although `groove index` keeps it in the same situation:
//!
//! - the file is there again but the reindex did not index it (over the size cap, the embedding
//!   endpoint failing): the rows went as for a removal;
//! - the watcher could not look at the path at all (a parent directory it may not search): the
//!   rows went as for a removal as well.
//!
//! Each test drives `groove serve` with the watcher on against the mock endpoint of
//! [`crate::common::embed_cli`], so nothing downloads a model.
//!
//! Not on macOS: there FSEvents folds the removal and the write into one change (kuriya trap
//! 525), so the watcher never sees a removal and these paths are not taken
//! (the remove-then-write end-to-end tests say the same for their removal tests).
#![cfg(not(target_os = "macos"))]

mod common;

use common::ansi::strip_ansi;
use common::embed_cli::{Fixture, fixture, reply};
use common::embed_mock::{hermetic, wait_until};
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

/// Turns the watcher on with a short debounce. Appended after the `[embedding]` keys.
const WATCH_TOML: &str = "\n[watch]\nenabled = true\ndebounce_ms = 300\n";

/// The start of every line the watcher prints for a removal of `runbook.md` that found the path
/// holding a file again: the line that says the rows were dropped and the one that says they
/// were kept both begin with it.
const REMOVED_LINE: &str = "runbook.md was removed and";

/// A Markdown document titled `Runbook` with one `##` section per `(heading, body)`.
fn runbook(sections: &[(&str, &str)]) -> String {
    let mut md = String::from("---\ntitle: Runbook\n---\n\n");
    for (heading, body) in sections {
        md.push_str(&format!("## {heading}\n\n{body}\n\n"));
    }
    md
}

/// `groove index` once, then `groove serve` with the watcher on.
fn serve(fx: &Fixture) -> ServerGuard {
    fx.index();
    // `spawn_serve_with(.., true, ..)` returns only after `watcher: watching`, so the edits a
    // test makes next cannot fall before the debouncer is armed.
    let (guard, _base) = spawn_serve_with(fx.kb(), &fx.config, true, |c| {
        hermetic(c, &fx.cache);
    });
    guard
}

fn stderr_lines(guard: &ServerGuard) -> Vec<String> {
    guard
        .stderr()
        .lines()
        .iter()
        .map(|l| strip_ansi(l))
        .collect()
}

/// Wait up to 30 s for a stderr line holding `needle`; every line seen so far comes back too.
fn wait_for_line(guard: &ServerGuard, needle: &str) -> (bool, Vec<String>) {
    let seen = wait_until(Duration::from_secs(30), || {
        stderr_lines(guard).iter().any(|l| l.contains(needle))
    });
    (seen, stderr_lines(guard))
}

/// The `content` of every hit `groove search` returns for `query`.
fn hit_contents(fx: &Fixture, query: &str) -> Vec<String> {
    let hits = fx.search_json(query);
    hits.get("results")
        .and_then(|r| r.as_array())
        .unwrap_or_else(|| panic!("search JSON has no results array: {hits}"))
        .iter()
        .filter_map(|h| h.get("content").and_then(|c| c.as_str()).map(str::to_owned))
        .collect()
}

/// (kuriya 549) A file removed and written again over `[index].max_text_file_size` keeps its
/// previous rows, as `groove index` keeps them for a file over the cap (the behavior docs:
/// "an edit that leaves the file over the cap keeps the old text searchable").
#[test]
fn a_removal_replaced_by_a_file_over_the_cap_keeps_the_row() {
    let fx = fixture(
        "i549-cap",
        &[(RUNBOOK, &runbook(&SECTIONS))],
        &format!("\n[index]\nmax_text_file_size = 4096\n{WATCH_TOML}"),
    );
    let guard = serve(&fx);
    assert_eq!(fx.documents(), 1);

    // The replacement shares no text with the old document, so a hit on the old text can only
    // come from the rows the watcher left.
    let padding: Vec<(String, String)> = (0..80)
        .map(|i| {
            (
                format!("Appendix {i}"),
                format!("Observatory schedule entry {i} lists the telescope slots for the night."),
            )
        })
        .collect();
    let padding: Vec<(&str, &str)> = padding
        .iter()
        .map(|(h, b)| (h.as_str(), b.as_str()))
        .collect();
    let replacement = runbook(&padding);
    assert!(replacement.len() > 4096, "{}", replacement.len());

    std::fs::remove_file(fx.kb().join(RUNBOOK)).expect("remove the runbook");
    fx.layout.write(RUNBOOK, &replacement);

    let (reported, lines) = wait_for_line(&guard, REMOVED_LINE);
    drop(guard);
    assert!(
        reported,
        "no line holding `{REMOVED_LINE}`:\n{}",
        lines.join("\n")
    );
    assert_eq!(
        fx.documents(),
        1,
        "the removal dropped the document although the file is there, over the cap:\n{}",
        lines.join("\n")
    );
    let contents = hit_contents(&fx, "archive tools folder");
    assert!(
        contents.iter().any(|c| c.contains(SECTIONS[0].1)),
        "the previous text is not searchable: {contents:?}"
    );
    assert!(
        !contents.iter().any(|c| c.contains("Observatory schedule")),
        "the file over the cap was indexed: {contents:?}"
    );

    // The full run agrees: it skips the file over the cap and keeps the row.
    fx.index();
    assert_eq!(
        fx.documents(),
        1,
        "groove index dropped the document over the cap"
    );
}

/// (kuriya 549) A file removed and written again whose reindex fails (the embedding endpoint
/// answers 503) keeps its previous rows, as an in-place write that fails the same way does.
#[test]
fn a_removal_whose_replacement_fails_to_embed_keeps_the_row() {
    let fx = fixture(
        "i549-embed-err",
        &[(RUNBOOK, &runbook(&SECTIONS))],
        &format!("max_retries = 0\n{WATCH_TOML}"),
    );
    let guard = serve(&fx);
    assert_eq!(fx.documents(), 1);

    // Only the added section needs a vector (the others reuse theirs), and every request now
    // fails, so the reindex ends in an error.
    fx.answer_with(|_| reply(503, &[("Retry-After", "0")]));
    std::fs::remove_file(fx.kb().join(RUNBOOK)).expect("remove the runbook");
    let mut sections = SECTIONS.to_vec();
    sections.push(ADDED);
    fx.layout.write(RUNBOOK, &runbook(&sections));

    let (reported, lines) = wait_for_line(&guard, REMOVED_LINE);
    fx.answer_normally();
    drop(guard);
    assert!(
        reported,
        "no line holding `{REMOVED_LINE}`:\n{}",
        lines.join("\n")
    );
    assert_eq!(
        fx.documents(),
        1,
        "the removal dropped the document although the file is there and only the embed failed:\n{}",
        lines.join("\n")
    );
    let contents = hit_contents(&fx, "archive tools folder");
    assert!(
        contents.iter().any(|c| c.contains(SECTIONS[0].1)),
        "the previous text is not searchable: {contents:?}"
    );
    assert!(
        !contents.iter().any(|c| c.contains(ADDED.1)),
        "the section whose embed failed is indexed: {contents:?}"
    );
}

/// (kuriya 548) A removal whose path the watcher cannot look at (its parent directory has lost
/// the search permission by the time the event is handled) keeps the rows: the watcher cannot
/// tell the file is gone, and `groove index` does not drop a row it cannot rule out either.
///
/// Linux only: the stat fails with `EACCES` there. On Windows a stat that meets a sharing
/// violation or access denied is retried through the directory listing, and macOS folds the
/// removal away (see the top of this file). When the process can stat through the mode (root),
/// the test says so on stderr and checks nothing.
#[test]
#[cfg(target_os = "linux")]
fn a_removal_whose_path_cannot_be_looked_at_keeps_the_row() {
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;

    /// Gives the directory its search permission back, so the fixture can remove it.
    struct RestoreMode(PathBuf);
    impl Drop for RestoreMode {
        fn drop(&mut self) {
            let _ = std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(0o755));
        }
    }

    const NOTE: &str = "sub/a.md";
    const UNKNOWN_LINE: &str = "could not tell whether sub/a.md is still there";
    let fx = fixture("i548-unknown", &[(NOTE, &runbook(&SECTIONS))], WATCH_TOML);
    let guard = serve(&fx);
    assert_eq!(fx.documents(), 1);

    let sub = fx.kb().join("sub");
    // Declared after the fixture and the server, so it drops first.
    let restore = RestoreMode(sub.clone());
    std::fs::remove_file(sub.join("a.md")).expect("remove the note");
    let mut sections = SECTIONS.to_vec();
    sections.push(ADDED);
    fx.layout.write(NOTE, &runbook(&sections));
    std::fs::set_permissions(&sub, std::fs::Permissions::from_mode(0o000))
        .expect("take the search permission away");
    if std::fs::metadata(sub.join("a.md")).is_ok() {
        drop(restore);
        eprintln!("skipped: this process can stat through a directory of mode 000 (root?)");
        return;
    }

    let (reported, lines) = wait_for_line(&guard, UNKNOWN_LINE);
    drop(restore);
    drop(guard);
    assert!(
        reported,
        "no line holding `{UNKNOWN_LINE}`:\n{}",
        lines.join("\n")
    );
    assert_eq!(
        fx.documents(),
        1,
        "the removal dropped the document although it could not tell the file was gone:\n{}",
        lines.join("\n")
    );
}
