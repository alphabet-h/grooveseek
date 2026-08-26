//! `groove search` CLI integration test。wrapper 形式の出力 + 新フィルタ引数の sanity。

mod common;
use common::ansi::strip_ansi;

use std::path::{Path, PathBuf};
use std::process::Command;

/// Locate the groove binary under test. Cargo sets `CARGO_BIN_EXE_<name>` for
/// integration tests automatically (same pattern as `tests/eval_cli.rs`).
fn bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_groove"))
}

/// Temporary directory with a `Drop` guard. `root/` is the cleanup boundary,
/// `root/kb/` is what we pass as `--kb-path`. The DB (which lands at
/// `kb_path.parent() == root/.groove.db`) thus stays inside the temp tree and
/// is cleaned up by `Drop`. **Important**: passing the unique tempdir directly
/// as `--kb-path` would put `.groove.db` in `temp_dir()` itself, making it
/// shared across tests and causing race conditions under cargo's parallel
/// runner.
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

#[test]
#[ignore] // requires built binary + embedding model download
fn cli_search_returns_wrapper_json() {
    let kb = TempKb::new("groove-search-cli");
    kb.write(
        "a.md",
        "---\ntitle: A\ntags: [rust]\n---\n# heading\n\nrust async tokio body\n",
    );

    // Index first
    let st = Command::new(bin())
        .args(["index", "--kb-path", kb.kb().to_str().unwrap()])
        .status()
        .expect("groove index");
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
        .expect("groove search");
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
    let kb = TempKb::new("groove-search-cli-pg");
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
    let kb = TempKb::new("groove-mmr-lambda-above");
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
        .expect("groove binary should run");
    assert!(!output.status.success(), "should fail with non-zero exit");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("must be in [0.0, 1.0]"),
        "stderr should contain parser error message; got: {stderr}"
    );
}

#[test]
fn test_search_cli_rejects_mmr_lambda_below_zero() {
    let kb = TempKb::new("groove-mmr-lambda-below");
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
        .expect("groove binary should run");
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("must be in [0.0, 1.0]"),
        "stderr should contain parser error message; got: {stderr}"
    );
}

#[test]
fn test_search_cli_rejects_mmr_same_doc_penalty_above_one() {
    let kb = TempKb::new("groove-mmr-penalty-above");
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
        .expect("groove binary should run");
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("must be in [0.0, 1.0]"),
        "stderr should contain parser error message; got: {stderr}"
    );
}

// ---------------------------------------------------------------------------
// Boundary inputs (audit L-27)
//
// Three shapes nothing exercised: a knowledge base with nothing in it, a query
// far past the 1 KiB the MCP surface refuses, and a query made only of
// characters outside the BMP.
//
// All three go through `groove search`, because that is the surface where they
// arrive untouched. The MCP tool rejects the long one at the door
// (`SEARCH_QUERY_MAX_BYTES`) and the command line has no such cap, so what the
// pipeline actually does with these inputs is only observable from here.
// ---------------------------------------------------------------------------

/// Index `kb` and hand back the status, so each test can say what it expects
/// rather than unwrapping in the middle of itself.
fn index(kb: &TempKb) -> std::process::ExitStatus {
    Command::new(bin())
        .args(["index", "--kb-path", kb.kb().to_str().unwrap()])
        .status()
        .expect("groove index")
}

fn search_json(kb: &TempKb, query: &str) -> std::process::Output {
    Command::new(bin())
        .args([
            "search",
            query,
            "--kb-path",
            kb.kb().to_str().unwrap(),
            "--format",
            "json",
        ])
        .output()
        .expect("groove search")
}

#[test]
#[ignore] // requires built binary + embedding model download
fn an_empty_knowledge_base_searches_to_an_empty_result_set() {
    // Not an error: there is an index, it just holds nothing. The wrapper has
    // to come out whole so a caller parsing it does not need a special case
    // for the empty corpus.
    let kb = TempKb::new("groove-boundary-empty");
    assert!(index(&kb).success(), "indexing an empty directory succeeds");

    let out = search_json(&kb, "anything at all");
    assert!(
        out.status.success(),
        "searching an empty index is not a failure; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("the wrapper is still valid JSON");
    assert_eq!(
        v["results"].as_array().map(Vec::len),
        Some(0),
        "no documents means no results: {v}"
    );
    assert_eq!(
        v["low_confidence"], false,
        "with fewer than two scores there is no ratio to judge, so the flag is \
         false rather than a guess: {v}"
    );
}

#[test]
#[ignore]
fn a_query_far_past_the_mcp_cap_is_answered_rather_than_refused() {
    // The MCP tool refuses anything over 1 KiB; `groove search` has no such
    // cap. This records what that means instead of assuming it is harmless.
    // What keeps it finite is `MAX_PHRASES` in the FTS compiler and the
    // tokenizer's own truncation — neither of them a limit on the input.
    let kb = TempKb::new("groove-boundary-long");
    kb.write(
        "a.md",
        "---\ntitle: Tokio\n---\n\n# rust async\n\nThis document describes the \
         tokio runtime, async/await, and rust concurrency primitives in detail.\n",
    );
    assert!(index(&kb).success());

    // 8 KiB — eight times what the MCP surface accepts, and the largest round
    // number that is portable.
    //
    // **The command line has an earlier bound than groove does, and it belongs
    // to the OS.** Measured: 64 KiB here fails before the process starts, with
    // `Os { code: 206, kind: InvalidFilename }` — Windows caps the whole
    // command line at 32,767 characters. So on this surface the practical
    // ceiling is the spawn, not anything in the search path, and a test that
    // reached for a genuinely huge query would be measuring `CreateProcess`.
    let long = "rust async tokio concurrency ".repeat(300);
    assert!(long.len() > 8 * 1024 && long.len() < 16 * 1024);

    let started = std::time::Instant::now();
    let out = search_json(&kb, &long);
    assert!(
        out.status.success(),
        "a long query is answered, not refused, on the command line; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice::<serde_json::Value>(&out.stdout)
        .expect("the answer is still valid JSON");
    // A tripwire, not a benchmark. If the work ever became linear in the input
    // length this would be minutes rather than seconds, and the number is loose
    // enough that a slow shared runner does not trip it.
    assert!(
        started.elapsed() < std::time::Duration::from_secs(60),
        "an 8 KiB query took {:?}; the phrase cap is supposed to keep the work \
         bounded by the corpus rather than by the query",
        started.elapsed()
    );
}

#[test]
#[ignore]
fn a_query_of_only_astral_characters_is_answered_cleanly() {
    // Every character here is a surrogate *pair* in UTF-16 and four bytes in
    // UTF-8. The trigram and CJK paths slice by character while `match_spans`
    // reports byte offsets, so a query where those two units differ by four is
    // where an off-by-one would show.
    let kb = TempKb::new("groove-boundary-astral");
    kb.write(
        "a.md",
        "---\ntitle: Emoji\n---\n\n# symbols\n\nThis document is about the \
         tokio runtime and has no emoji in it at all, deliberately.\n",
    );
    assert!(index(&kb).success());

    let out = search_json(&kb, "🐙🦀🚀🎉🧭");
    assert!(
        out.status.success(),
        "an astral-only query is answered, not a crash; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("the answer is still valid JSON");
    assert!(
        v["results"].is_array(),
        "the wrapper comes out whole even when nothing matches: {v}"
    );
}

// ---------------------------------------------------------------------------
// `rerank_by_default` on the command line (audit L-25)
//
// The decision is a pure function with its own tests (`cli_should_rerank` in
// main.rs). What none of them can see is whether `groove search` hands it the
// key from `groove.toml` at all — that join is one line, and a line that
// stopped passing `cfg.rerank_by_default` would leave every one of those tests
// green while the file silently stopped counting.
//
// The observable is the **shape of `score`**. Without rerank a score is an RRF
// sum, bounded by `2 / (rrf_k + 1)` ≈ 0.033 at the default `rrf_k = 60`. With
// rerank the score is replaced by the cross-encoder's logit, which is on a
// different scale entirely and is usually negative for a weak match. So the
// two runs are told apart by the number, without needing to reason about which
// ordering is "better".
// ---------------------------------------------------------------------------

/// The largest an RRF score can be: both retrieval legs ranking a chunk first.
const RRF_CEILING: f64 = 2.0 / 61.0;

/// Search with an explicit config file, which is what makes it trusted.
fn search_json_with_config(kb: &TempKb, query: &str, config: &Path) -> std::process::Output {
    Command::new(bin())
        .args([
            "search",
            query,
            "--kb-path",
            kb.kb().to_str().unwrap(),
            "--config",
            config.to_str().unwrap(),
            "--format",
            "json",
        ])
        .output()
        .expect("groove search")
}

fn scores(out: &std::process::Output) -> Vec<f64> {
    let v: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("the answer is valid JSON");
    v["results"]
        .as_array()
        .expect("results is an array")
        .iter()
        .map(|h| h["score"].as_f64().expect("score is a number"))
        .collect()
}

#[test]
#[ignore] // requires the built binary, the embedding model, and the reranker (~2.3 GB)
fn groove_search_reads_rerank_by_default_from_the_config_file() {
    let kb = TempKb::new("groove-rerank-by-default");
    for (name, title) in [("a.md", "Tokio runtime"), ("b.md", "Async primitives")] {
        kb.write(
            name,
            &format!(
                "---\ntitle: {title}\n---\n\n# rust async\n\nThis document describes the \
                 tokio runtime, async/await, and rust concurrency primitives in detail, \
                 at enough length to pass the quality filter.\n"
            ),
        );
    }
    assert!(index(&kb).success());

    // `bge-v2-m3` rather than the 280 MB `bge-base`, so this shares a cache
    // entry with `test_bge_reranker_v2_m3_reorders_ja` instead of adding a
    // second reranker to every nightly. It comes with that test's treatment
    // too: the Windows leg skips it by name in `nightly.yml`, for the reasons
    // written there. What is under test is OS-independent wiring, and the
    // Linux leg runs it.
    let off = kb.kb().join("rerank-off.toml");
    std::fs::write(
        &off,
        "reranker = \"bge-v2-m3\"\nrerank_by_default = false\n",
    )
    .expect("write config");
    let on = kb.kb().join("rerank-on.toml");
    std::fs::write(&on, "reranker = \"bge-v2-m3\"\nrerank_by_default = true\n")
        .expect("write config");

    let without = search_json_with_config(&kb, "rust async runtime", &off);
    assert!(
        without.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&without.stderr)
    );
    let plain = scores(&without);
    assert!(!plain.is_empty(), "the corpus has matching documents");
    assert!(
        plain.iter().all(|s| *s > 0.0 && *s <= RRF_CEILING),
        "`rerank_by_default = false` must leave RRF scores in place, got {plain:?}"
    );

    let with = search_json_with_config(&kb, "rust async runtime", &on);
    assert!(
        with.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&with.stderr)
    );
    let reranked = scores(&with);
    assert!(
        reranked.iter().any(|s| *s > RRF_CEILING || *s <= 0.0),
        "`rerank_by_default = true` must replace the score with the \
         cross-encoder's, which is not on the RRF scale, got {reranked:?}"
    );
}

/// A query with nothing left to search for is refused before anything
/// expensive happens, and the refusal reaches stderr.
///
/// **Not `#[ignore]`d, and that is the assertion.** The refusal sits ahead of
/// `require_kb_path`, the model load and the database open, so this test needs
/// none of them — it points `--kb-path` at an empty directory that has never
/// been indexed and still expects the sentence. If the check ever drifted
/// below the model load, this test would start needing a 130 MB download to
/// pass, which is the failure showing up as a cost rather than as a red test.
///
/// The `--` is load-bearing: `-foo` is a value that starts with a hyphen, and
/// the argument parser reads it as flags (`-f -o -o`) and exits 2 long before
/// `main` sees a query. `allow_hyphen_values` is deliberately *not* set on the
/// argument — it would make `groove search -l 5` search for the string `-l`
/// instead of reporting a typo — so `--` is how a leading-hyphen query is
/// written on this surface.
#[test]
fn an_exclusion_only_query_fails_on_stderr_and_exits_non_zero() {
    let kb = TempKb::new("groove-exclusion-only-cli");

    let out = Command::new(bin())
        .args([
            "search",
            "--kb-path",
            kb.kb().to_str().unwrap(),
            "--",
            "-foo",
        ])
        .output()
        .expect("groove search");

    assert!(
        !out.status.success(),
        "a query that cannot be searched must not exit 0; stdout: {}",
        String::from_utf8_lossy(&out.stdout)
    );
    // Windows `tracing-subscriber` colours stderr, so the escapes come off
    // before the text is read.
    let stderr = strip_ansi(&String::from_utf8_lossy(&out.stderr));
    assert!(
        stderr.contains("Error: query has no positive term"),
        "the failure has to say what is wrong with the query, on stderr: {stderr}"
    );
    assert!(
        out.stdout.is_empty(),
        "a refusal writes no result to stdout: {}",
        String::from_utf8_lossy(&out.stdout)
    );
}

/// The command line reports what it excluded, in the same field the MCP tool
/// uses.
///
/// Both directions, because the absence is the half a caller has to be able to
/// read: `excluded_terms` present means rows were dropped, and the key missing
/// means none were — the rule `SearchFilterEcho::new` applies to every other
/// list in the echo.
#[test]
#[ignore] // requires built binary + embedding model download
fn the_command_line_echoes_excluded_terms_in_json() {
    let kb = TempKb::new("groove-exclusion-echo-cli");
    kb.write(
        "tokio.md",
        "---\ntitle: Tokio\n---\n\n# tokio runtime\n\nThe tokio runtime is an async \
         executor for rust that drives futures to completion on a work-stealing \
         scheduler.\n",
    );
    assert!(index(&kb).success());

    let out = search_json(&kb, "tokio runtime -rayon");
    assert!(
        out.status.success(),
        "a query with something left to search for is answered; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("the answer is valid JSON");
    assert_eq!(
        v["filter_applied"]["excluded_terms"],
        serde_json::json!(["rayon"]),
        "the echo names the phrases the search actually excluded: {v}"
    );

    let plain = search_json(&kb, "tokio runtime");
    assert!(
        plain.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&plain.stderr)
    );
    let p: serde_json::Value =
        serde_json::from_slice(&plain.stdout).expect("the answer is valid JSON");
    assert!(
        p["filter_applied"].get("excluded_terms").is_none(),
        "a query with no exclusions must leave no key behind: {p}"
    );
}
