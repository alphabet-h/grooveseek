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

/// A frontmatter block with nothing after it: broken YAML, no chunks.
const STUB: &str = "---\ntitle: [unclosed\n---\n";

/// (codex P2, round 1) A stub is skipped for having no chunks, but its
/// frontmatter still failed to parse, so it is named and counted -- and under
/// strict mode it fails the run like any other broken file.
#[test]
fn test_stub_with_broken_frontmatter_is_counted_although_skipped() {
    let kb = TempKbLayout::new("groove-fm-stub");
    kb.write("good.md", GOOD);
    kb.write("stub.md", STUB);
    let (stderr, status) = run_index(kb.kb(), None, &[]);
    assert!(status.success(), "exit failed: {status:?}\n{stderr}");
    assert!(
        stderr.contains("warning: stub.md: failed to parse YAML frontmatter: "),
        "{stderr}"
    );
    assert!(
        stderr.contains(", 1 skipped, 1 frontmatter unparsed), "),
        "skipped and counted are not exclusive:\n{stderr}"
    );

    let cfg = strict_config(&kb);
    let (stderr, status) = run_index(kb.kb(), Some(&cfg), &[]);
    assert_eq!(status.code(), Some(1), "{stderr}");
}

/// (codex P1, round 1) An index written before this version holds broken
/// documents with a matching content hash and no tag. The first run of this
/// version re-reads the frontmatter of unchanged Markdown once, names and tags
/// what it finds, and records that it has looked so the next run does not.
#[test]
fn test_first_run_after_upgrade_tags_unchanged_legacy_documents() {
    let kb = build_kb("groove-fm-legacy");
    let (first, status) = run_index(kb.kb(), None, &[]);
    assert!(status.success(), "first run failed: {status:?}\n{first}");

    // Make the index look like one an older version wrote: the row is there
    // with an empty tag list, and nothing says the frontmatter was ever checked.
    let db = kb.root().join(".groove.db");
    let conn = rusqlite::Connection::open(&db).expect("open db");
    conn.execute(
        "UPDATE documents SET tags = '[]' WHERE path = 'broken.md'",
        [],
    )
    .unwrap();
    conn.execute(
        "DELETE FROM index_meta WHERE key = 'frontmatter_policy'",
        [],
    )
    .unwrap();
    drop(conn);

    let (second, status) = run_index(kb.kb(), None, &[]);
    assert!(status.success(), "second run failed: {status:?}\n{second}");
    assert!(
        second.contains("warning: broken.md: failed to parse YAML frontmatter: "),
        "the legacy document must be named:\n{second}"
    );
    assert!(
        second.contains("(0 updated, ") && second.contains(", 1 frontmatter unparsed), "),
        "tagged without re-embedding:\n{second}"
    );
    let (title, tags) = document_row(&kb, "broken.md");
    assert_eq!(title, None);
    assert!(tags.contains("frontmatter:unparsed"), "{tags}");

    // Looked once; the third run is an ordinary no-op.
    let (third, status) = run_index(kb.kb(), None, &[]);
    assert!(status.success(), "third run failed: {status:?}\n{third}");
    assert!(third.contains(", 0 frontmatter unparsed), "), "{third}");
    assert!(!third.contains("warning: broken.md"), "{third}");
}

/// Put the index back into the shape an older version leaves: the row for
/// `rel` carries no tag and nothing says the frontmatter was ever checked.
fn make_legacy(kb: &TempKbLayout, rel: &str) {
    let conn = rusqlite::Connection::open(kb.root().join(".groove.db")).expect("open db");
    conn.execute("UPDATE documents SET tags = '[]' WHERE path = ?1", [rel])
        .unwrap();
    conn.execute(
        "DELETE FROM index_meta WHERE key = 'frontmatter_policy'",
        [],
    )
    .unwrap();
}

fn frontmatter_policy(kb: &TempKbLayout) -> Option<String> {
    use rusqlite::OptionalExtension;
    let conn = rusqlite::Connection::open(kb.root().join(".groove.db")).expect("open db");
    conn.query_row(
        "SELECT value FROM index_meta WHERE key = 'frontmatter_policy'",
        [],
        |row| row.get(0),
    )
    .optional()
    .unwrap()
}

/// (codex P1, round 2) The scan and the registry take `.MD` as Markdown, so
/// the one-time check has to as well, or the upper-case file is recorded as
/// checked without ever being read.
#[test]
fn test_upgrade_check_recognises_uppercase_markdown_extension() {
    let kb = TempKbLayout::new("groove-fm-upper");
    kb.write("good.md", GOOD);
    kb.write("UPPER.MD", BROKEN);
    let (first, status) = run_index(kb.kb(), None, &[]);
    assert!(status.success(), "first run failed: {status:?}\n{first}");
    make_legacy(&kb, "UPPER.MD");

    let (second, status) = run_index(kb.kb(), None, &[]);
    assert!(status.success(), "second run failed: {status:?}\n{second}");
    assert!(
        second.contains("warning: UPPER.MD: failed to parse YAML frontmatter: "),
        "{second}"
    );
    let (_, tags) = document_row(&kb, "UPPER.MD");
    assert!(tags.contains("frontmatter:unparsed"), "{tags}");
    assert_eq!(frontmatter_policy(&kb).as_deref(), Some("tag-unparsed"));
}

/// (codex P2, round 2) A legacy Markdown file the check could not read or
/// parse leaves the check pending, so a later run looks again once the file
/// is back; recording "checked" over a file that was never read would hide it
/// behind the fast path for good.
#[test]
fn test_upgrade_check_stays_pending_when_a_legacy_file_cannot_be_parsed() {
    let kb = build_kb("groove-fm-pending");
    let (first, status) = run_index(kb.kb(), None, &[]);
    assert!(status.success(), "first run failed: {status:?}\n{first}");
    make_legacy(&kb, "broken.md");

    // Not valid UTF-8: the run cannot parse it, and the row is retained.
    std::fs::write(kb.kb().join("broken.md"), [0xff, 0xfe, 0xfd]).unwrap();
    let (second, status) = run_index(kb.kb(), None, &[]);
    assert!(status.success(), "second run failed: {status:?}\n{second}");
    assert_eq!(
        frontmatter_policy(&kb),
        None,
        "a Markdown document the check could not read must keep it pending:\n{second}"
    );

    // Back to the content the row was indexed from: same hash, so only the
    // pending check can reach it.
    kb.write("broken.md", BROKEN);
    let (third, status) = run_index(kb.kb(), None, &[]);
    assert!(status.success(), "third run failed: {status:?}\n{third}");
    assert!(
        third.contains("warning: broken.md: failed to parse YAML frontmatter: "),
        "{third}"
    );
    let (_, tags) = document_row(&kb, "broken.md");
    assert!(tags.contains("frontmatter:unparsed"), "{tags}");
    assert_eq!(frontmatter_policy(&kb).as_deref(), Some("tag-unparsed"));
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
