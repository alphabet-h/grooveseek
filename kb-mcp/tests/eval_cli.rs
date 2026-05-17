//! End-to-end integration test for `kb-mcp eval`.
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

/// Locate the kb-mcp binary under test. Cargo sets `CARGO_BIN_EXE_<name>` for
/// integration tests automatically — no manual `target/<profile>/...` juggling.
fn kb_mcp_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_kb-mcp"))
}

/// Temporary directory with a `Drop` guard to clean up after the test.
/// Holds a root (for KB + sibling `.kb-mcp.db`) and exposes a `kb/` subdir
/// so the DB (which lands at `kb_path.parent()`) ends up inside the temp
/// tree and gets cleaned by our own `Drop`.
struct TempKb {
    root: PathBuf,
    kb: PathBuf,
}

impl TempKb {
    fn new(prefix: &str) -> Self {
        let pid = std::process::id();
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let root = std::env::temp_dir().join(format!("{prefix}-{pid}-{nonce}"));
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
    let kb = TempKb::new("kb-mcp-eval-it");
    kb.write(
        "rrf.md",
        "# RRF\n\nRRF is Reciprocal Rank Fusion with constant k=60.\n",
    );
    kb.write(
        "chunks.md",
        "# Chunks\n\nChunks are deduplicated by SHA-256 of content.\n",
    );

    let bin = kb_mcp_bin();
    let kb_path = kb.kb();

    // 1) Build the index (BGE-small; small + fast).
    let status = Command::new(&bin)
        .arg("index")
        .arg("--kb-path")
        .arg(kb_path)
        .arg("--model")
        .arg("bge-small-en-v1.5")
        .status()
        .expect("spawn kb-mcp index");
    assert!(status.success(), "index failed");

    // 2) Write a minimal golden file.
    // Use concat! instead of `\` line continuation in a string literal:
    // line continuation collapses leading whitespace of the next line, which
    // would break YAML indentation.
    let golden = kb_path.join(".kb-mcp-eval.yml");
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
        .expect("spawn kb-mcp eval (1)");
    assert!(
        out.status.success(),
        "eval (1st run) failed: stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("kb-mcp eval"),
        "expected banner 'kb-mcp eval' in output: {stdout}"
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
    let hist = kb_path.join(".kb-mcp-eval-history.json");
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
        .expect("spawn kb-mcp eval (2)");
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

#[test]
#[ignore]
fn eval_errors_when_golden_missing() {
    let kb = TempKb::new("kb-mcp-eval-it-missing");
    kb.write(
        "doc.md",
        "# Doc\n\nA minimal placeholder document so index has something to ingest.\n",
    );

    let bin = kb_mcp_bin();
    let kb_path = kb.kb();

    // Build the index — but intentionally skip writing the golden file.
    let status = Command::new(&bin)
        .arg("index")
        .arg("--kb-path")
        .arg(kb_path)
        .arg("--model")
        .arg("bge-small-en-v1.5")
        .status()
        .expect("spawn kb-mcp index");
    assert!(status.success(), "index failed");

    let out = Command::new(&bin)
        .arg("eval")
        .arg("--kb-path")
        .arg(kb_path)
        .arg("--model")
        .arg("bge-small-en-v1.5")
        .output()
        .expect("spawn kb-mcp eval");
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
