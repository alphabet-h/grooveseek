//! End-to-end integration test for `groove eval`.
//!
//! `#[ignore]` にしている: 実モデル DL (BGE-small ~130MB) + index 作成を伴う。
//! 手動 / CI で `cargo test --test eval_cli -- --ignored` で回す。
//!
//! 通常の `cargo test` では skip されるため、依存の重いモデル DL は発生しない。

use std::path::{Path, PathBuf};
use std::process::Command;

// ---------------------------------------------------------------------------
// Helpers (tests/validate_cli.rs と揃えた形。tempdir crate 依存なし)
// ---------------------------------------------------------------------------

/// Locate the groove binary under test. Cargo sets `CARGO_BIN_EXE_<name>` for
/// integration tests automatically — no manual `target/<profile>/...` juggling.
fn grooveseek_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_groove"))
}

/// Temporary directory with a `Drop` guard to clean up after the test.
/// Holds a root (for KB + sibling `.groove.db`) and exposes a `kb/` subdir
/// so the DB (which lands at `kb_path.parent()`) ends up inside the temp
/// tree and gets cleaned by our own `Drop`.
struct TempKb {
    root: PathBuf,
    kb: PathBuf,
}

impl TempKb {
    fn new(prefix: &str) -> Self {
        // PID + nanos alone is not unique within one test binary: its tests run
        // on parallel threads of a single process.
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let pid = std::process::id();
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let root = std::env::temp_dir().join(format!("{prefix}-{pid}-{nonce}-{seq}"));
        let kb = root.join("kb");
        std::fs::create_dir_all(&kb).unwrap();
        Self { root, kb }
    }

    fn kb(&self) -> &Path {
        &self.kb
    }

    fn write(&self, rel: &str, content: &str) {
        let full = self.kb.join(rel);
        if let Some(parent) = full.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(full, content).unwrap();
    }
}

impl Drop for TempKb {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
#[ignore]
fn eval_runs_end_to_end_and_writes_history() {
    let kb = TempKb::new("groove-eval-it");
    kb.write(
        "rrf.md",
        "# RRF\n\nRRF is Reciprocal Rank Fusion with constant k=60.\n",
    );
    kb.write(
        "chunks.md",
        "# Chunks\n\nChunks are deduplicated by SHA-256 of content.\n",
    );

    let bin = grooveseek_bin();
    let kb_path = kb.kb();

    // 1) Build the index (BGE-small; small + fast).
    let status = Command::new(&bin)
        .arg("index")
        .arg("--kb-path")
        .arg(kb_path)
        .arg("--model")
        .arg("bge-small-en-v1.5")
        .status()
        .expect("spawn groove index");
    assert!(status.success(), "index failed");

    // 2) Write a minimal golden file.
    // Use concat! instead of `\` line continuation in a string literal:
    // line continuation collapses leading whitespace of the next line, which
    // would break YAML indentation.
    let golden = kb_path.join(".groove-eval.yml");
    let golden_yml = concat!(
        "queries:\n",
        "  - id: rrf-q\n",
        "    query: \"What is RRF?\"\n",
        "    expected:\n",
        "      - path: \"rrf.md\"\n",
        "  - id: chunks-q\n",
        "    query: \"How are chunks deduplicated?\"\n",
        "    expected:\n",
        "      - path: \"chunks.md\"\n",
    );
    std::fs::write(&golden, golden_yml).unwrap();

    // 3) 1st run: text output, history file does not yet exist.
    let out = Command::new(&bin)
        .arg("eval")
        .arg("--kb-path")
        .arg(kb_path)
        .arg("--model")
        .arg("bge-small-en-v1.5")
        .arg("--no-color")
        .output()
        .expect("spawn groove eval (1)");
    assert!(
        out.status.success(),
        "eval (1st run) failed: stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("groove eval"),
        "expected banner 'groove eval' in output: {stdout}"
    );
    assert!(
        stdout.contains("recall@1") || stdout.contains("recall@5") || stdout.contains("recall@10"),
        "expected at least one recall@k metric in output: {stdout}"
    );
    // No previous run yet → the diff header must not appear.
    assert!(
        !stdout.contains("previous run"),
        "1st run must not show previous-run diff: {stdout}"
    );

    // 4) History file must be written after the 1st run.
    let hist = kb_path.join(".groove-eval-history.json");
    assert!(
        hist.exists(),
        "history file not written at {}",
        hist.display()
    );

    // 5) 2nd run: JSON output, `previous` must be populated from step 4.
    let out2 = Command::new(&bin)
        .arg("eval")
        .arg("--kb-path")
        .arg(kb_path)
        .arg("--model")
        .arg("bge-small-en-v1.5")
        .arg("--format")
        .arg("json")
        .output()
        .expect("spawn groove eval (2)");
    assert!(
        out2.status.success(),
        "eval (2nd run) failed: stderr={}",
        String::from_utf8_lossy(&out2.stderr)
    );
    let v: serde_json::Value =
        serde_json::from_slice(&out2.stdout).expect("valid JSON from `eval --format json`");
    let q_count = v["aggregate"]["query_count"]
        .as_u64()
        .expect("aggregate.query_count must be a number");
    assert!(
        q_count >= 1,
        "expected aggregate.query_count >= 1, got {q_count}"
    );
    assert!(
        !v["previous"].is_null(),
        "previous must be present on 2nd run: {v}"
    );
}

/// feature-52: a note that quotes the golden queries verbatim is reported, and
/// the same corpus without that note reports nothing.
///
/// **Both halves matter.** The rule this pins is "two or more distinct golden
/// queries in one document"; the reason it is not "one" is that golden queries
/// are often topic names (`cross-encoder`), which appear verbatim in the very
/// documents that explain them — measured at 8 false positives on a healthy
/// 662-document corpus. A test that only checks the positive case would still
/// pass with that noisy rule restored.
#[test]
#[ignore]
fn eval_reports_documents_that_quote_several_golden_queries() {
    let kb = TempKb::new("groove-eval-it-quoted");
    kb.write(
        "rrf.md",
        "# RRF\n\nRRF is Reciprocal Rank Fusion with constant k=60.\n",
    );
    kb.write(
        "chunks.md",
        "# Chunks\n\nChunks are deduplicated by SHA-256 of content.\n",
    );

    let bin = grooveseek_bin();
    let kb_path = kb.kb();

    let golden = kb_path.join(".groove-eval.yml");
    let golden_yml = concat!(
        "queries:\n",
        "  - id: rrf-q\n",
        "    query: \"What is Reciprocal Rank Fusion?\"\n",
        "    expected:\n",
        "      - path: \"rrf.md\"\n",
        "  - id: chunks-q\n",
        "    query: \"How are chunks deduplicated?\"\n",
        "    expected:\n",
        "      - path: \"chunks.md\"\n",
    );
    std::fs::write(&golden, golden_yml).unwrap();

    let index = |bin: &PathBuf| {
        let status = Command::new(bin)
            .arg("index")
            .arg("--kb-path")
            .arg(kb_path)
            .arg("--model")
            .arg("bge-small-en-v1.5")
            .status()
            .expect("spawn groove index");
        assert!(status.success(), "index failed");
    };
    let eval_json = |bin: &PathBuf| {
        let out = Command::new(bin)
            .arg("eval")
            .arg("--kb-path")
            .arg(kb_path)
            .arg("--model")
            .arg("bge-small-en-v1.5")
            .arg("--no-history")
            .arg("--format")
            .arg("json")
            .output()
            .expect("spawn groove eval");
        assert!(
            out.status.success(),
            "eval must still exit 0 when it reports a finding: stderr={}",
            String::from_utf8_lossy(&out.stderr)
        );
        let v: serde_json::Value =
            serde_json::from_slice(&out.stdout).expect("valid JSON from `eval --format json`");
        (v, String::from_utf8_lossy(&out.stderr).into_owned())
    };

    // 1) Clean corpus: each document contains only its own query's wording.
    index(&bin);
    let (v, stderr) = eval_json(&bin);
    assert_eq!(
        v["findings"],
        serde_json::json!([]),
        "a corpus with nothing quoted must report an empty array, not a missing key: {v}"
    );
    assert!(
        !stderr.contains("golden-queries-quoted"),
        "a clean corpus must not warn: {stderr}"
    );

    // 2) Add the note that quotes both queries verbatim — the shape of the
    //    real incident, where a note *about* the evaluation joined the corpus.
    //
    //    One query is quoted **only in a heading** and the other **only in the
    //    body**, so the finding requires both fields to be scanned. The
    //    Markdown parser strips the heading line out of the chunk body and
    //    stores it separately, while FTS weights headings above body text — a
    //    content-only scan misses the most natural way to document a test.
    kb.write(
        "about-the-eval.md",
        "# Notes on the eval\n\n\
         The second golden entry uses the wording `How are chunks deduplicated?`,\n\
         which is quoted here so the labelling decision is on the record.\n\n\
         ## What is Reciprocal Rank Fusion?\n\n\
         That heading is the first golden query verbatim. This paragraph exists\n\
         so the section stays above the chunk merge threshold.\n",
    );
    index(&bin);
    let (v, stderr) = eval_json(&bin);

    let findings = v["findings"].as_array().expect("findings must be an array");
    assert_eq!(findings.len(), 1, "expected exactly one finding: {v}");
    assert_eq!(findings[0]["check"], "golden-queries-quoted");
    assert_eq!(findings[0]["path"], "about-the-eval.md");
    let ids: Vec<&str> = findings[0]["quoted"]
        .as_array()
        .expect("quoted must be an array")
        .iter()
        .map(|q| q["query_id"].as_str().expect("query_id is a string"))
        .collect();
    assert_eq!(ids, vec!["rrf-q", "chunks-q"]);

    assert!(
        stderr.contains("golden-queries-quoted") && stderr.contains("about-the-eval.md"),
        "the warning belongs on stderr: {stderr}"
    );
    // The result itself stays on stdout; the diagnostic must not leak into it.
    assert!(
        v["aggregate"]["query_count"].as_u64().unwrap_or(0) >= 1,
        "the report itself must still be intact: {v}"
    );
}

#[test]
#[ignore]
fn eval_errors_when_golden_missing() {
    let kb = TempKb::new("groove-eval-it-missing");
    kb.write(
        "doc.md",
        "# Doc\n\nA minimal placeholder document so index has something to ingest.\n",
    );

    let bin = grooveseek_bin();
    let kb_path = kb.kb();

    // Build the index — but intentionally skip writing the golden file.
    let status = Command::new(&bin)
        .arg("index")
        .arg("--kb-path")
        .arg(kb_path)
        .arg("--model")
        .arg("bge-small-en-v1.5")
        .status()
        .expect("spawn groove index");
    assert!(status.success(), "index failed");

    let out = Command::new(&bin)
        .arg("eval")
        .arg("--kb-path")
        .arg(kb_path)
        .arg("--model")
        .arg("bge-small-en-v1.5")
        .output()
        .expect("spawn groove eval");
    assert!(
        !out.status.success(),
        "eval must exit non-zero when golden file is missing"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("golden"),
        "stderr should mention 'golden' when golden file is missing: {stderr}"
    );
}
