//! `groove doctor` as a command: where it writes, and what it exits with.
//!
//! The findings themselves are unit-tested in `src/doctor.rs`. What only a
//! subprocess can check is the contract CI depends on — **stdout** carries the
//! report (CLAUDE.md's CLI output convention puts a command's result there;
//! `index` and `status` are the stderr-only ones), and the exit code
//! distinguishes "clean" from "found something" from "could not look".
//!
//! No embedding model is involved: the fixtures are built by writing rows
//! through the library, so these run under a plain `cargo test`.

use std::path::Path;
use std::process::Command;

mod common;
use common::mcp::grooveseek_bin;
use common::temp::TempKbLayout;

/// Build an index directly, without going through `groove index` — that would
/// need the embedding model, and none of what `doctor` looks at depends on the
/// embedding being real.
fn seed_index(kb: &Path) {
    let db_path = grooveseek::resolve_db_path(kb);
    let db = grooveseek::db::Database::open(&db_path.to_string_lossy()).expect("open db");
    db.verify_embedding_meta("bge-small-en-v1.5", 384)
        .expect("meta");
    let doc = db
        .upsert_document(
            "notes/a.md",
            Some("A"),
            None,
            None,
            None,
            &[],
            None,
            "h",
            42,
        )
        .expect("upsert");
    db.insert_chunk(doc, 0, Some("H"), None, "body", None, &vec![0.1; 384], 1.0)
        .expect("chunk");
}

fn run_doctor(kb: &Path, json: bool) -> (i32, String, String) {
    let mut args = vec![
        "doctor".to_string(),
        "--kb-path".to_string(),
        kb.display().to_string(),
    ];
    if json {
        args.push("--format".to_string());
        args.push("json".to_string());
    }
    let out = Command::new(grooveseek_bin())
        .args(&args)
        .output()
        .expect("groove doctor");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// `doctor` with an explicit `--config`, and the config it is given.
///
/// The migration-period group reads `exclude_dirs`, which config discovery
/// could otherwise supply from a `groove.toml` sitting above wherever the test
/// binary runs. An empty file pins it to the defaults instead.
fn isolated_config(layout: &TempKbLayout) -> std::path::PathBuf {
    let cfg = layout.root().join("groove.toml");
    std::fs::write(&cfg, "").expect("write config");
    cfg
}

fn run_doctor_with(layout: &TempKbLayout, cfg: &Path) -> (i32, String, String) {
    let args = vec![
        "doctor".to_string(),
        "--kb-path".to_string(),
        layout.kb().display().to_string(),
        "--config".to_string(),
        cfg.display().to_string(),
        "--format".to_string(),
        "json".to_string(),
    ];
    let out = Command::new(grooveseek_bin())
        .args(&args)
        .output()
        .expect("groove doctor");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

fn documents_in(stdout: &str) -> u64 {
    let parsed: serde_json::Value = serde_json::from_str(stdout)
        .unwrap_or_else(|e| panic!("--format json must emit JSON on stdout ({e}): {stdout}"));
    parsed["documents"]
        .as_u64()
        .unwrap_or_else(|| panic!("the report always carries a document count: {stdout}"))
}

fn checks_in(stdout: &str) -> Vec<String> {
    let parsed: serde_json::Value = serde_json::from_str(stdout)
        .unwrap_or_else(|e| panic!("--format json must emit JSON on stdout ({e}): {stdout}"));
    parsed["findings"]
        .as_array()
        .expect("findings array")
        .iter()
        .filter_map(|f| f["check"].as_str())
        .map(str::to_string)
        .collect()
}

#[test]
fn a_healthy_index_exits_zero_and_says_so_on_stdout() {
    let layout = TempKbLayout::new("groove-doctor-clean");
    layout.write("notes/a.md", "# A\n\nbody\n");
    seed_index(layout.kb());

    let (code, stdout, _) = run_doctor(layout.kb(), false);
    assert_eq!(code, 0, "a healthy index must not fail a CI gate");
    assert!(
        stdout.contains("No issues found"),
        "the report belongs on stdout, got: {stdout}"
    );
}

#[test]
fn a_broken_index_exits_one_and_names_the_check() {
    let layout = TempKbLayout::new("groove-doctor-broken");
    layout.write("notes/a.md", "# A\n\nbody\n");
    seed_index(layout.kb());
    {
        let db_path = grooveseek::resolve_db_path(layout.kb());
        let conn = rusqlite::Connection::open(&db_path).expect("open");
        conn.execute_batch("DELETE FROM fts_chunks").expect("break");
    }

    let (code, stdout, _) = run_doctor(layout.kb(), true);
    assert_eq!(code, 1, "findings must be distinguishable from a clean run");

    let parsed: serde_json::Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("--format json must emit JSON on stdout ({e}): {stdout}"));
    let checks: Vec<&str> = parsed["findings"]
        .as_array()
        .expect("findings array")
        .iter()
        .filter_map(|f| f["check"].as_str())
        .collect();
    assert!(
        checks.contains(&"missing-fts-row"),
        "expected the missing FTS row to be named, got {checks:?}"
    );
}

/// codex P2 round 2: `require_kb_path` used to run before the exit-code
/// mapping, so the most ordinary setup mistake — no `--kb-path` anywhere —
/// exited 1, the code reserved for "inspected it, found something".
#[test]
fn no_kb_path_at_all_exits_two_rather_than_looking_like_a_finding() {
    let layout = TempKbLayout::new("groove-doctor-nokbpath");
    // An explicit config with no `kb_path`, so the result does not depend on
    // whether the machine running the test happens to have one discoverable.
    let cfg = layout.root().join("groove.toml");
    std::fs::write(&cfg, "model = \"bge-small-en-v1.5\"\n").expect("write config");

    let out = Command::new(grooveseek_bin())
        .args(["--config", &cfg.to_string_lossy(), "doctor"])
        .output()
        .expect("groove doctor");
    assert_eq!(
        out.status.code().unwrap_or(-1),
        2,
        "a missing --kb-path is a failure to run, not a finding"
    );
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("--kb-path is required"),
        "the reason belongs on stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// codex P2 round 3: config discovery runs before the subcommand arm, so a
/// configuration that will not parse used to exit 1 — again the code that
/// means "inspected it, found something", for a run that never inspected
/// anything.
#[test]
fn a_configuration_that_will_not_load_exits_two() {
    let layout = TempKbLayout::new("groove-doctor-badcfg");
    let cfg = layout.root().join("groove.toml");
    std::fs::write(&cfg, "model = \"bge-small-en-v1.5\"\nthis is not toml\n")
        .expect("write config");

    let out = Command::new(grooveseek_bin())
        .args([
            "--config",
            &cfg.to_string_lossy(),
            "doctor",
            "--kb-path",
            &layout.kb().display().to_string(),
        ])
        .output()
        .expect("groove doctor");
    assert_eq!(
        out.status.code().unwrap_or(-1),
        2,
        "a config that will not parse is a failure to run"
    );
}

#[test]
fn a_missing_index_exits_two_rather_than_reporting_a_clean_bill() {
    // "I could not look" and "I looked and found nothing" are different
    // answers, and a CI gate that conflates them passes on a machine where the
    // index was never built.
    let layout = TempKbLayout::new("groove-doctor-noindex");
    layout.write("notes/a.md", "# A\n\nbody\n");

    let (code, stdout, stderr) = run_doctor(layout.kb(), false);
    assert_eq!(code, 2, "no index is a failure to run, not a finding");
    assert!(
        stdout.is_empty(),
        "there is no report to print, so stdout stays empty: {stdout}"
    );
    assert!(
        stderr.contains("No index found"),
        "the reason belongs on stderr: {stderr}"
    );
}

/// audit L-4. ADR-0007 renamed the ignore file with no alias, so a
/// `.kb-mcpignore` left behind keeps nothing out — and until now nothing said
/// so. The literal name is spelled here rather than imported: what this pins is
/// the string an operator's knowledge base actually contains.
#[test]
fn a_document_the_old_ignore_file_names_is_reported_by_the_command() {
    let layout = TempKbLayout::new("groove-doctor-legacy");
    layout.write("notes/a.md", "# A\n\nbody\n");
    layout.write(".kb-mcpignore", "notes/\n");
    seed_index(layout.kb());

    let cfg = isolated_config(&layout);
    let (code, stdout, _) = run_doctor_with(&layout, &cfg);
    assert_eq!(
        code, 1,
        "a document the old file would have kept out is a finding"
    );

    let checks = checks_in(&stdout);
    assert!(
        checks.iter().any(|c| c == "indexed-despite-legacy-ignore"),
        "expected the old ignore file to be named, got {checks:?}"
    );
    assert!(
        stdout.contains("notes/a.md"),
        "the finding is worth having because it carries the path: {stdout}"
    );
}

/// The remedy, run end to end: rename the file, index once, and both the
/// document and the finding are gone.
///
/// `#[ignore]` because this is the only test in this file that runs a real
/// `groove index`, which needs the embedding model. Nightly runs it with
/// `-- --ignored`.
///
/// It exists because a remedy that cannot work is its own defect — `doctor`
/// already carries one comment about a remedy that had to be withdrawn for
/// naming a command that refuses the identical file. The two halves of this one
/// are covered separately in `src/indexer.rs` (a newly ignored file leaves the
/// walk, and what leaves the walk is deleted from the database); this checks
/// that the two together are what the sentence in the report promises.
#[test]
#[ignore]
fn renaming_the_old_file_and_reindexing_clears_the_finding() {
    let layout = TempKbLayout::new("groove-doctor-legacy-e2e");
    layout.write("notes/a.md", "# A\n\nbody\n");
    layout.write(".kb-mcpignore", "notes/\n");
    let cfg = isolated_config(&layout);

    let index = |what: &str| {
        let args = vec![
            "index".to_string(),
            "--kb-path".to_string(),
            layout.kb().display().to_string(),
            "--config".to_string(),
            cfg.display().to_string(),
        ];
        let out = Command::new(grooveseek_bin())
            .args(&args)
            .output()
            .expect("groove index");
        assert!(
            out.status.success(),
            "groove index ({what}) failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    };

    index("with the old name still in place");
    let (code, stdout, _) = run_doctor_with(&layout, &cfg);
    assert_eq!(code, 1, "the premise: it is reported first. {stdout}");
    assert!(
        checks_in(&stdout)
            .iter()
            .any(|c| c == "indexed-despite-legacy-ignore"),
        "the premise: {stdout}"
    );
    assert_eq!(
        documents_in(&stdout),
        1,
        "the premise: the document really is in the index. {stdout}"
    );

    std::fs::rename(
        layout.kb().join(".kb-mcpignore"),
        layout.kb().join(".grooveignore"),
    )
    .expect("rename");
    index("after the rename the remedy asks for");

    let (code, stdout, _) = run_doctor_with(&layout, &cfg);
    // The document count, not just the exit code. Renaming the file on its own
    // silences the finding — there is no old file left to read — so a test that
    // stopped at "clean" would pass without the index ever being corrected, and
    // would say nothing about whether `groove index` is the right second half
    // of the remedy.
    assert_eq!(
        documents_in(&stdout),
        0,
        "the remedy has to remove the document, not just the finding: {stdout}"
    );
    assert_eq!(
        code, 0,
        "and nothing else is left to report afterwards: {stdout}"
    );
}
