//! Subprocess tests for #251: a Markdown file whose YAML frontmatter does not
//! parse is named on stderr, counted in the `Done in` summary, indexed with the
//! `frontmatter:unparsed` tag, and -- under `[index].fail_on_frontmatter_error`
//! -- turns the exit code non-zero after the run has finished.

use std::path::{Path, PathBuf};
use std::process::Command;

mod common;
use common::ansi::strip_ansi;
use common::temp::TempKbLayout;

const GOOD: &str =
    "---\ntitle: Good\ntags: [ok]\n---\n\n# Good\n\nEnough content here to stand as a chunk.\n";
/// The only defect is the unterminated flow sequence in `title`, which makes
/// the whole block unparsable -- the same shape as the issue's `title: a: b`.
const BROKEN: &str =
    "---\ntitle: [unclosed\n---\n\n# Broken\n\nEnough content here too, to stand as a chunk.\n";
const REPAIRED: &str =
    "---\ntitle: Repaired\n---\n\n# Broken\n\nEnough content here too, to stand as a chunk.\n";

fn grooveseek_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_groove"))
}

/// Run `groove [--config <cfg>] index --kb-path <kb> <args...>`, stripping
/// the ANSI colour Windows `tracing-subscriber` puts on stderr.
fn run_index(
    kb: &Path,
    config: Option<&Path>,
    args: &[&str],
) -> (String, std::process::ExitStatus) {
    let mut cmd = Command::new(grooveseek_binary());
    if let Some(cfg) = config {
        cmd.arg("--config").arg(cfg);
    }
    cmd.arg("index").arg("--kb-path").arg(kb);
    for a in args {
        cmd.arg(a);
    }
    let output = cmd.output().expect("failed to spawn groove index");
    let stderr = strip_ansi(&String::from_utf8_lossy(&output.stderr));
    (stderr, output.status)
}

fn document_row(kb: &TempKbLayout, rel: &str) -> (Option<String>, String) {
    let conn = rusqlite::Connection::open(kb.root().join(".groove.db")).expect("open db");
    conn.query_row(
        "SELECT title, tags FROM documents WHERE path = ?1",
        [rel],
        |row| Ok((row.get::<_, Option<String>>(0)?, row.get::<_, String>(1)?)),
    )
    .unwrap_or_else(|e| panic!("no documents row for {rel}: {e}"))
}

fn build_kb(prefix: &str) -> TempKbLayout {
    let kb = TempKbLayout::new(prefix);
    kb.write("good.md", GOOD);
    kb.write("broken.md", BROKEN);
    kb
}

#[test]
fn test_index_names_the_file_whose_frontmatter_failed() {
    let kb = build_kb("groove-fm-names");
    let (stderr, status) = run_index(kb.kb(), None, &[]);
    assert!(
        status.success(),
        "exit failed: {status:?}\nstderr:\n{stderr}"
    );
    assert!(
        stderr.contains("warning: broken.md: failed to parse YAML frontmatter: "),
        "expected the warning to name broken.md, got:\n{stderr}"
    );
    assert!(
        !stderr.contains("warning: good.md"),
        "good.md must not be warned about:\n{stderr}"
    );
    assert!(
        stderr.contains(", 1 frontmatter unparsed), "),
        "expected the summary to count it, got:\n{stderr}"
    );
}

#[test]
fn test_index_quiet_still_prints_the_frontmatter_warning() {
    let kb = build_kb("groove-fm-quiet");
    let (stderr, status) = run_index(kb.kb(), None, &["--quiet"]);
    assert!(
        status.success(),
        "exit failed: {status:?}\nstderr:\n{stderr}"
    );
    assert!(
        stderr.contains("warning: broken.md: failed to parse YAML frontmatter: "),
        "--quiet suppresses progress, not warnings:\n{stderr}"
    );
    assert!(
        !stderr.contains("  indexed:"),
        "--quiet must still hide per-file progress:\n{stderr}"
    );
}

#[test]
fn test_unparsed_document_carries_the_tag_in_the_index() {
    let kb = build_kb("groove-fm-tag");
    let (stderr, status) = run_index(kb.kb(), None, &[]);
    assert!(
        status.success(),
        "exit failed: {status:?}\nstderr:\n{stderr}"
    );
    let (title, tags) = document_row(&kb, "broken.md");
    assert_eq!(title, None, "a broken block leaves title empty");
    assert!(
        tags.contains("frontmatter:unparsed"),
        "expected the tag in documents.tags, got {tags}"
    );
    let (good_title, good_tags) = document_row(&kb, "good.md");
    assert_eq!(good_title.as_deref(), Some("Good"));
    assert!(!good_tags.contains("frontmatter:unparsed"));
}

#[test]
fn test_second_run_reports_zero_because_unchanged_files_are_not_reparsed() {
    let kb = build_kb("groove-fm-second");
    let (first, status) = run_index(kb.kb(), None, &[]);
    assert!(status.success(), "first run failed: {status:?}\n{first}");
    let (second, status) = run_index(kb.kb(), None, &[]);
    assert!(status.success(), "second run failed: {status:?}\n{second}");
    assert!(
        second.contains(", 0 frontmatter unparsed), "),
        "an unchanged file is not re-parsed, so it is not re-counted:\n{second}"
    );
    assert!(
        !second.contains("warning: broken.md"),
        "no re-parse, no warning:\n{second}"
    );
}

/// Write a strict `groove.toml` outside `kb/` (so it is not indexed) and
/// return its path; passed with `--config`, which makes it trusted.
fn strict_config(kb: &TempKbLayout) -> PathBuf {
    let path = kb.root().join("groove.toml");
    std::fs::write(&path, "[index]\nfail_on_frontmatter_error = true\n").expect("write config");
    path
}

#[test]
fn test_strict_mode_fails_after_indexing_everything() {
    let kb = build_kb("groove-fm-strict");
    let cfg = strict_config(&kb);
    let (stderr, status) = run_index(kb.kb(), Some(&cfg), &[]);
    assert_eq!(
        status.code(),
        Some(1),
        "expected exit 1, got {status:?}\n{stderr}"
    );
    // The run finished: every file was indexed and the summary was printed
    // before the failure was reported.
    assert!(stderr.contains("  indexed: good.md"), "{stderr}");
    assert!(stderr.contains("  indexed: broken.md"), "{stderr}");
    assert!(stderr.contains(", 1 frontmatter unparsed), "), "{stderr}");
    assert!(
        stderr.contains("fail_on_frontmatter_error"),
        "the failure must name the switch that caused it:\n{stderr}"
    );
    let (_, tags) = document_row(&kb, "broken.md");
    assert!(tags.contains("frontmatter:unparsed"), "{tags}");
    let (good_title, _) = document_row(&kb, "good.md");
    assert_eq!(good_title.as_deref(), Some("Good"));
}

#[test]
fn test_strict_mode_passes_a_clean_kb() {
    let kb = TempKbLayout::new("groove-fm-strict-clean");
    kb.write("good.md", GOOD);
    let cfg = strict_config(&kb);
    let (stderr, status) = run_index(kb.kb(), Some(&cfg), &[]);
    assert!(status.success(), "exit failed: {status:?}\n{stderr}");
    assert!(stderr.contains(", 0 frontmatter unparsed), "), "{stderr}");
}

#[test]
fn test_repairing_the_frontmatter_removes_the_tag() {
    let kb = build_kb("groove-fm-repair");
    let (first, status) = run_index(kb.kb(), None, &[]);
    assert!(status.success(), "first run failed: {status:?}\n{first}");
    // Same body, fixed block: this takes the frontmatter-only fast path, which
    // must rewrite tags as well as title.
    kb.write("broken.md", REPAIRED);
    let (second, status) = run_index(kb.kb(), None, &[]);
    assert!(status.success(), "second run failed: {status:?}\n{second}");
    assert!(second.contains(", 0 frontmatter unparsed), "), "{second}");
    let (title, tags) = document_row(&kb, "broken.md");
    assert_eq!(title.as_deref(), Some("Repaired"));
    assert!(
        !tags.contains("frontmatter:unparsed"),
        "the tag must go when the YAML is fixed, got {tags}"
    );
}
