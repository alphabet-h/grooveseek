//! End-to-end integration test for `kb-mcp tune`.
//!
//! `#[ignore]` にしている: 実モデル DL (BGE-small ~130MB) + index 作成を伴う。
//! 手動 / CI で `cargo test --test tune_cli -- --ignored` で回す。

use std::path::{Path, PathBuf};
use std::process::Command;

// ---------------------------------------------------------------------------
// Helpers (tests/eval_cli.rs と揃えた形。tempfile crate 依存なし)
// ---------------------------------------------------------------------------

fn kb_mcp_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_kb-mcp"))
}

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

fn index(bin: &Path, kb_path: &Path) {
    let status = Command::new(bin)
        .arg("index")
        .arg("--kb-path")
        .arg(kb_path)
        .arg("--model")
        .arg("bge-small-en-v1.5")
        .status()
        .expect("spawn kb-mcp index");
    assert!(status.success(), "index failed");
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
#[ignore]
fn tune_exits_2_when_no_query_is_fts_effective() {
    // spec D-8 の pre-flight ゲート: 全 query が FTS 不感なら grid を実行せず
    // exit 2。golden の query が KB に逐語出現しないよう作る。
    let kb = TempKb::new("kb-mcp-tune-it-preflight");
    kb.write(
        "alpha.md",
        "# Alpha\n\nThe alpha document discusses gradient descent optimisation.\n",
    );
    kb.write(
        "beta.md",
        "# Beta\n\nThe beta document discusses reciprocal rank fusion.\n",
    );

    let bin = kb_mcp_bin();
    index(&bin, kb.kb());

    // どちらの query も本文に逐語では現れない (単一 phrase 化されるため
    // FTS は 0 件になる)。
    let golden = kb.kb().join(".kb-mcp-eval.yml");
    let golden_yml = concat!(
        "queries:\n",
        "  - id: q-alpha\n",
        "    query: \"how does the first document explain optimisation methods\"\n",
        "    expected:\n",
        "      - path: \"alpha.md\"\n",
        "  - id: q-beta\n",
        "    query: \"what merges two ranked candidate lists together\"\n",
        "    expected:\n",
        "      - path: \"beta.md\"\n",
    );
    std::fs::write(&golden, golden_yml).unwrap();

    let out = Command::new(&bin)
        .arg("tune")
        .arg("--kb-path")
        .arg(kb.kb())
        .arg("--model")
        .arg("bge-small-en-v1.5")
        .arg("--no-color")
        .output()
        .expect("spawn kb-mcp tune");

    assert_eq!(
        out.status.code(),
        Some(2),
        "pre-flight must exit 2 when no query is FTS-effective; stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("fewer than 2 FTS candidates"),
        "stderr must explain why the sweep was skipped: {stderr}"
    );
    assert!(
        out.stdout.is_empty(),
        "no result must be written to stdout when the sweep is skipped"
    );
}

#[test]
#[ignore]
fn tune_reports_json_for_an_fts_effective_golden() {
    // verbatim 出現する語句を query にすれば FTS 候補 >= 2 になり、
    // grid が回って全セクションが出る。
    let kb = TempKb::new("kb-mcp-tune-it-effective");
    for i in 0..4 {
        kb.write(
            &format!("doc_{i}.md"),
            &format!(
                "# Zebrafish notes {i}\n\nZebrafish larvae are used in screening assays.\n\
                 This paragraph mentions zebrafish again for document {i}.\n"
            ),
        );
    }
    // FTS5 は語が chunk 総数の半数以上に出現すると IDF を 1e-6 にクランプし、
    // どんな bm25 重みでもスコアが復活しなくなる (spec D-11-6)。ヒット 4 件の
    // まま総 chunk 数が 5 だと phrase doc-freq が 4/5 でクランプ域に入り、
    // grid が順位を一切動かせない fixture になってしまう。無関係な doc を
    // 足して doc-freq を chunk 総数の 1/4 以下に薄める。
    for i in 0..10 {
        kb.write(
            &format!("filler_{i}.md"),
            &format!(
                "# Filler {i}\n\nAn unrelated note number {i} about entirely different \
                 subject matter with no shared vocabulary.\n"
            ),
        );
    }
    kb.write(
        "other.md",
        "# Other\n\nAn unrelated note about reciprocal rank fusion constants.\n",
    );

    let bin = kb_mcp_bin();
    index(&bin, kb.kb());

    let golden = kb.kb().join(".kb-mcp-eval.yml");
    let golden_yml = concat!(
        "queries:\n",
        "  - id: zebrafish\n",
        "    query: \"zebrafish larvae\"\n",
        "    expected:\n",
        "      - path: \"doc_0.md\"\n",
        "      - path: \"doc_1.md\"\n",
        "  - id: zebrafish-screen\n",
        "    query: \"screening assays\"\n",
        "    expected:\n",
        "      - path: \"doc_2.md\"\n",
    );
    std::fs::write(&golden, golden_yml).unwrap();

    let out = Command::new(&bin)
        .arg("tune")
        .arg("--kb-path")
        .arg(kb.kb())
        .arg("--model")
        .arg("bge-small-en-v1.5")
        .arg("--format")
        .arg("json")
        .output()
        .expect("spawn kb-mcp tune");
    assert!(
        out.status.success(),
        "tune failed: stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );

    let v: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("valid JSON from `tune --format json`");
    assert!(
        v["effective_query_count"].as_u64().unwrap_or(0) >= 1,
        "at least one query must be FTS-effective: {v}"
    );
    // filler で薄めたので IDF クランプに掛かっていないこと (掛かっていると
    // grid が順位を動かせず、この e2e が実質何も検証しなくなる)
    let clamped: Vec<&serde_json::Value> = v["diagnostics"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|d| {
            d["effective"].as_bool() == Some(true) && d["idf_clamped"].as_bool() == Some(true)
        })
        .collect();
    assert!(
        clamped.is_empty(),
        "effective queries must not be IDF-clamped in this fixture: {clamped:?}"
    );
    // D-11 の必須項目が JSON に載っていること
    assert!(v["loo"]["mean_delta"].is_number(), "{v}");
    assert!(v["loo"]["selection_stability"].is_number(), "{v}");
    assert!(v["loo"]["sign_test"]["improved"].is_number(), "{v}");
    assert!(v["per_query_impact"]["degraded"].is_number(), "{v}");
    assert!(v["diagnostics"].as_array().is_some(), "{v}");
    assert!(v["candidate_pool"].is_number(), "{v}");
    let decision = v["verdict"]["decision"].as_str().unwrap_or("");
    assert!(
        decision == "adopt" || decision == "keep_default",
        "verdict must be one of adopt / keep_default, got {decision:?}"
    );
}
