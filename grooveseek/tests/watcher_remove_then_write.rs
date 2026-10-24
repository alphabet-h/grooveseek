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
