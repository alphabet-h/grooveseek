//! `kb-mcp search` CLI integration test。wrapper 形式の出力 + 新フィルタ引数の sanity。

use std::path::{Path, PathBuf};
use std::process::Command;

/// Locate the kb-mcp binary under test. Cargo sets `CARGO_BIN_EXE_<name>` for
/// integration tests automatically (same pattern as `tests/eval_cli.rs`).
fn bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_kb-mcp"))
}

/// Temporary directory with a `Drop` guard. `root/` is the cleanup boundary,
/// `root/kb/` is what we pass as `--kb-path`. The DB (which lands at
/// `kb_path.parent() == root/.kb-mcp.db`) thus stays inside the temp tree and
/// is cleaned up by `Drop`. **Important**: passing the unique tempdir directly
/// as `--kb-path` would put `.kb-mcp.db` in `temp_dir()` itself, making it
/// shared across tests and causing race conditions under cargo's parallel
/// runner.
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

#[test]
#[ignore] // requires built binary + embedding model download
fn cli_search_returns_wrapper_json() {
    let kb = TempKb::new("kb-mcp-search-cli");
    kb.write(
        "a.md",
        "---\ntitle: A\ntags: [rust]\n---\n# heading\n\nrust async tokio body\n",
    );

    // Index first
    let st = Command::new(bin())
        .args(["index", "--kb-path", kb.kb().to_str().unwrap()])
        .status()
        .expect("kb-mcp index");
    assert!(st.success());

    // Search with --format json
    let out = Command::new(bin())
        .args([
            "search",
            "rust",
            "--kb-path",
            kb.kb().to_str().unwrap(),
            "--format",
            "json",
        ])
        .output()
        .expect("kb-mcp search");
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    // wrapper 形式の特徴を検証
    assert!(stdout.contains("\"results\""), "must wrap in 'results'");
    assert!(
        stdout.contains("\"low_confidence\""),
        "must include 'low_confidence'"
    );
    assert!(
        stdout.contains("\"filter_applied\""),
        "must include 'filter_applied'"
    );
}

#[test]
#[ignore]
fn cli_search_with_path_glob_filter_excludes() {
    let kb = TempKb::new("kb-mcp-search-cli-pg");
    // 既定の quality_filter (threshold 0.3) を通すため、十分な長さの本文にする。
    // 短すぎる ("rust body" 等) と低品質扱いで除外される。
    kb.write(
        "docs/a.md",
        "---\ntitle: Rust under docs\n---\n\n# rust async\n\nThis is the docs version describing tokio runtime, async/await, and rust concurrency primitives in detail.\n",
    );
    kb.write(
        "notes/b.md",
        "---\ntitle: Rust under notes\n---\n\n# rust async\n\nThis is the notes version describing tokio runtime, async/await, and rust concurrency primitives in detail.\n",
    );

    let st = Command::new(bin())
        .args(["index", "--kb-path", kb.kb().to_str().unwrap()])
        .status()
        .unwrap();
    assert!(st.success());

    let out = Command::new(bin())
        .args([
            "search",
            "rust",
            "--kb-path",
            kb.kb().to_str().unwrap(),
            "--path-glob",
            "docs/**",
            "--format",
            "json",
        ])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("docs/a.md"));
    assert!(!stdout.contains("notes/b.md"));
}

#[test]
fn test_search_cli_rejects_mmr_lambda_above_one() {
    let kb = TempKb::new("kb-mcp-mmr-lambda-above");
    let output = std::process::Command::new(bin())
        .args([
            "search",
            "--kb-path",
            kb.kb().to_str().unwrap(),
            "--mmr-lambda",
            "1.5",
            "query",
        ])
        .output()
        .expect("kb-mcp binary should run");
    assert!(!output.status.success(), "should fail with non-zero exit");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("must be in [0.0, 1.0]"),
        "stderr should contain parser error message; got: {stderr}"
    );
}

#[test]
fn test_search_cli_rejects_mmr_lambda_below_zero() {
    let kb = TempKb::new("kb-mcp-mmr-lambda-below");
    let output = std::process::Command::new(bin())
        .args([
            "search",
            "--kb-path",
            kb.kb().to_str().unwrap(),
            "--mmr-lambda",
            "-0.1",
            "query",
        ])
        .output()
        .expect("kb-mcp binary should run");
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("must be in [0.0, 1.0]"),
        "stderr should contain parser error message; got: {stderr}"
    );
}

#[test]
fn test_search_cli_rejects_mmr_same_doc_penalty_above_one() {
    let kb = TempKb::new("kb-mcp-mmr-penalty-above");
    let output = std::process::Command::new(bin())
        .args([
            "search",
            "--kb-path",
            kb.kb().to_str().unwrap(),
            "--mmr-same-doc-penalty",
            "1.5",
            "query",
        ])
        .output()
        .expect("kb-mcp binary should run");
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("must be in [0.0, 1.0]"),
        "stderr should contain parser error message; got: {stderr}"
    );
}
