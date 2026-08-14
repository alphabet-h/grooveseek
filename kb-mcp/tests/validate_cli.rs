//! `kb-mcp validate` CLI の integration test。
//!
//! schema 読込 + walkdir + format dispatch + exit code の end-to-end を
//! 実バイナリで叩いて確認する。embedding DL 不要なので通常の `cargo test`
//! に載せる (`#[ignore]` なし)。
//!
//! `target/{debug|release}/kb-mcp(.exe)` が無いと skip する。

use std::path::{Path, PathBuf};
use std::process::Command;

// ---------------------------------------------------------------------------
// helpers (tests/http_transport.rs と類似、依存なし)
// ---------------------------------------------------------------------------

fn kb_mcp_bin() -> Option<PathBuf> {
    // Workspace 化 (feature-44 PR-1) 以降の fallback。CARGO_TARGET_DIR
    // override は維持、未設定なら CARGO_BIN_EXE_kb-mcp (cargo が test build
    // 時に absolute path を set する built-in env var、Cargo 1.39+) を使う。
    let bin: PathBuf = if let Ok(custom_target) = std::env::var("CARGO_TARGET_DIR") {
        let target = PathBuf::from(custom_target);
        let profile = if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        };
        #[cfg(windows)]
        let b = target.join(profile).join("kb-mcp.exe");
        #[cfg(not(windows))]
        let b = target.join(profile).join("kb-mcp");
        b
    } else {
        PathBuf::from(env!("CARGO_BIN_EXE_kb-mcp"))
    };
    if bin.exists() { Some(bin) } else { None }
}

struct TempKb {
    path: PathBuf,
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
        let path = std::env::temp_dir().join(format!("{prefix}-{pid}-{nonce}-{seq}"));
        std::fs::create_dir_all(&path).unwrap();
        Self { path }
    }
    fn write(&self, rel: &str, content: &str) {
        let full = self.path.join(rel);
        if let Some(parent) = full.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(full, content).unwrap();
    }
}

impl Drop for TempKb {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn run(bin: &Path, args: &[&str]) -> (i32, String, String) {
    let out = Command::new(bin).args(args).output().expect("kb-mcp spawn");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).to_string(),
        String::from_utf8_lossy(&out.stderr).to_string(),
    )
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn test_validate_no_schema_exits_zero() {
    let Some(bin) = kb_mcp_bin() else {
        eprintln!("kb-mcp binary not built — skipping");
        return;
    };
    let kb = TempKb::new("kb-validate-noschema");
    kb.write("a.md", "---\ntitle: X\n---\n# body\n");
    let (code, _out, err) = run(&bin, &["validate", "--kb-path", kb.path.to_str().unwrap()]);
    assert_eq!(
        code, 0,
        "exit should be 0 when schema is absent: stderr={err}"
    );
    assert!(
        err.contains("no schema found"),
        "expected info message, got: {err}"
    );
}

#[test]
fn test_validate_violations_exit_one_json_format() {
    let Some(bin) = kb_mcp_bin() else {
        eprintln!("kb-mcp binary not built — skipping");
        return;
    };
    let kb = TempKb::new("kb-validate-viol");
    kb.write(
        "good.md",
        "---\ntitle: OK\ndate: \"2026-04-19\"\ntopic: mcp\ntags: [a]\n---\n# body\n",
    );
    kb.write(
        "bad.md",
        "---\ndate: \"2026/04/19\"\ntopic: general\ntags: []\n---\n# body no title\n",
    );
    kb.write(
        "kb-mcp-schema.toml",
        r#"
[fields.title]
required = true
type = "string"

[fields.date]
required = true
type = "string"
pattern = '^\d{4}-\d{2}-\d{2}$'

[fields.topic]
required = true
type = "string"
enum = ["mcp", "rag"]

[fields.tags]
required = true
type = "array"
min_length = 1
"#,
    );
    let (code, out, _err) = run(
        &bin,
        &[
            "validate",
            "--kb-path",
            kb.path.to_str().unwrap(),
            "--format",
            "json",
        ],
    );
    assert_eq!(code, 1, "exit should be 1 when violations present");
    // JSON が valid で bad.md に違反が出ていること
    let v: serde_json::Value = serde_json::from_str(&out).expect("valid JSON output");
    assert_eq!(v["scanned"], 2);
    assert_eq!(v["violated"], 1);
    assert_eq!(v["ok"], 1);
    assert_eq!(v["files"].as_array().unwrap().len(), 1);
    assert_eq!(v["files"][0]["path"], "bad.md");
    let violations = v["files"][0]["violations"].as_array().unwrap();
    assert!(!violations.is_empty(), "expected at least 1 violation");
    // title missing + date pattern mismatch + topic enum + tags empty の 4 つ
    // すべてが出ることを確認
    let kinds: Vec<&str> = violations
        .iter()
        .map(|v| v["kind"].as_str().unwrap_or(""))
        .collect();
    assert!(kinds.contains(&"missing_required"));
    assert!(kinds.contains(&"pattern_mismatch"));
    assert!(kinds.contains(&"not_in_enum"));
}

#[test]
fn test_validate_schema_load_error_exit_two() {
    let Some(bin) = kb_mcp_bin() else {
        eprintln!("kb-mcp binary not built — skipping");
        return;
    };
    let kb = TempKb::new("kb-validate-badschema");
    kb.write("a.md", "---\ntitle: X\n---\n# body\n");
    // 不正な schema: pattern が壊れた正規表現
    kb.write(
        "kb-mcp-schema.toml",
        r#"
[fields.title]
pattern = '[unclosed'
"#,
    );
    let (code, _out, err) = run(&bin, &["validate", "--kb-path", kb.path.to_str().unwrap()]);
    assert_eq!(
        code, 2,
        "exit should be 2 on schema load error: stderr={err}"
    );
    assert!(err.contains("schema load error"));
}

#[test]
fn test_validate_ok_case_exit_zero() {
    let Some(bin) = kb_mcp_bin() else {
        eprintln!("kb-mcp binary not built — skipping");
        return;
    };
    let kb = TempKb::new("kb-validate-ok");
    kb.write(
        "a.md",
        "---\ntitle: X\ndate: \"2026-04-19\"\ntopic: mcp\ntags: [a]\n---\n# body\n",
    );
    kb.write(
        "kb-mcp-schema.toml",
        r#"
[fields.title]
required = true
type = "string"

[fields.date]
required = true
type = "string"
pattern = '^\d{4}-\d{2}-\d{2}$'

[fields.topic]
required = true
type = "string"
enum = ["mcp"]

[fields.tags]
required = true
type = "array"
min_length = 1
"#,
    );
    let (code, out, _err) = run(
        &bin,
        &[
            "validate",
            "--kb-path",
            kb.path.to_str().unwrap(),
            "--no-color",
        ],
    );
    assert_eq!(code, 0);
    assert!(out.contains("1 files OK"), "text summary: {out}");
}

#[test]
fn test_validate_strict_flag_accepted_as_noop() {
    // evaluator High #1: --strict は spec にあるが MVP では no-op。
    // CI スクリプトで付けても parse エラーにならず exit 0 であること。
    let Some(bin) = kb_mcp_bin() else {
        eprintln!("kb-mcp binary not built — skipping");
        return;
    };
    let kb = TempKb::new("kb-validate-strict");
    kb.write("a.md", "---\ntitle: X\n---\n# body\n");
    let (code, _out, _err) = run(
        &bin,
        &[
            "validate",
            "--kb-path",
            kb.path.to_str().unwrap(),
            "--strict",
        ],
    );
    assert_eq!(code, 0, "--strict must be accepted (no schema → exit 0)");
}

/// (feature-49) `validate` is the third exclusion surface, and the one that is
/// easiest to leave behind: `validate_collect_md_files` lives in the **binary**
/// target and reaches the shared decision through the library's public API, so
/// a change made in `src/` compiles without it. AU-03 and BU-19 were both a
/// surface that stopped agreeing with the others.
///
/// Run through the real binary, because that is the only way to reach that
/// function at all. No embedding model is involved, so this stays off
/// `#[ignore]`.
#[test]
fn validate_honours_kb_mcpignore() {
    let Some(bin) = kb_mcp_bin() else {
        eprintln!("kb-mcp binary not built — skipping");
        return;
    };
    let kb = TempKb::new("kb-validate-ignore");
    let front = "---\ntitle: X\n---\n# body\n";
    kb.write("good.md", front);
    kb.write("notes/a.md", front);
    kb.write("drafts/wip.md", front);
    kb.write("notes/b.tmp.md", front);
    kb.write(
        "kb-mcp-schema.toml",
        "[fields.title]\nrequired = true\ntype = \"string\"\n",
    );

    let args = [
        "validate",
        "--kb-path",
        kb.path.to_str().unwrap(),
        "--format",
        "json",
    ];

    let (code, out, err) = run(&bin, &args);
    assert_eq!(code, 0, "fixture should be clean: stderr={err}");
    let v: serde_json::Value = serde_json::from_str(&out).expect("valid JSON output");
    assert_eq!(
        v["scanned"], 4,
        "baseline: every .md is scanned before an ignore file exists"
    );

    kb.write(".kb-mcpignore", "drafts/\n*.tmp.md\n");

    let (code, out, err) = run(&bin, &args);
    assert_eq!(code, 0, "still clean after the ignore file: stderr={err}");
    let v: serde_json::Value = serde_json::from_str(&out).expect("valid JSON output");
    assert_eq!(
        v["scanned"], 2,
        "validate must skip what the index walk skips — a directory pattern and a \
         file pattern, leaving good.md and notes/a.md"
    );
}
