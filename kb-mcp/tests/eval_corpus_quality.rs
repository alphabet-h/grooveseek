//! Retrieval quality gate over the committed `kb-eval` fixture corpus (BU-11).
//!
//! Until this existed, nothing in CI measured whether a change made retrieval
//! *worse*. The recall drop that feature-48 introduced was noticed only by
//! hand, on a private knowledge base, after the release. This file indexes a
//! committed 20-document Japanese/English corpus, runs the committed golden
//! query set through `kb-mcp eval`, and fails when the aggregate metrics fall
//! below a floor derived from measurement.
//!
//! # Layers
//!
//! [`kb_eval_corpus_and_golden_stay_in_sync`] needs no model and runs in the
//! ordinary `cargo test` (= the PR gate). It only checks that the corpus, the
//! manifest below, and the golden still describe the same documents — a
//! renamed fixture is caught there as a named failure instead of surfacing a
//! day later as an unexplained recall drop.
//!
//! The two `#[ignore]` tests do the actual retrieval and are picked up by
//! `nightly.yml`, which runs `cargo test -- --include-ignored` on ubuntu and
//! windows. They need no workflow change beyond the Windows skip for the
//! BGE-M3 one (~2.3 GB, same reason as the two pre-existing skips).
//!
//! # Baseline, measured 2026-08-14 (25 queries, 20 documents, 60 chunks)
//!
//! | | recall@1 | recall@5 | MRR |
//! |---|---|---|---|
//! | BGE-small, as shipped | 0.92 | 0.96 | 0.940 |
//! | BGE-small, FTS leg forced silent | 0.80 | 0.88 | 0.835 |
//! | BGE-M3, as shipped | 1.00 | 1.00 | 1.000 |
//! | BGE-M3, FTS leg forced silent | 1.00 | 1.00 | 1.000 |
//!
//! The "FTS leg forced silent" rows were produced by making `build_fts_query`
//! return `None` in a scratch build — i.e. the exact failure mode this gate
//! exists to catch. Three conclusions are baked into the thresholds below:
//!
//! 1. **BGE-small is the sensitive leg.** Killing the keyword half moves it by
//!    0.12 recall@1 / 0.105 MRR. Four queries degrade, three of them Japanese
//!    natural-language ones — the feature-48 class exactly.
//! 2. **BGE-M3 is blind to it at this corpus size.** Twenty semantically
//!    distinct documents are separable by the vector leg alone, so BGE-M3
//!    answers every query, keyword half or no keyword half. Its gate therefore
//!    guards the Japanese *semantic* path and catches gross regressions; it is
//!    not, and cannot be here, an FTS regression detector.
//! 3. **recall@5 is not asserted.** Healthy 0.96 and FTS-dead 0.88 are only
//!    two queries apart, so any threshold loose enough to survive ordinary
//!    drift is also loose enough to sit below the broken state. It is printed
//!    in the failure report instead.
//!
//! Thresholds allow **two queries of drift and trip on the third**, which is
//! the slack the scores need: RRF fusion runs on `f32`, and near-ties can come
//! apart differently on another architecture (the same reason
//! `common::mcp::extract_path_heading_order` compares paths instead of
//! scores). A real retrieval regression moves many queries at once.

use std::path::{Path, PathBuf};
use std::process::Command;

mod common;
use common::temp::TempKbLayout;

use kb_mcp::eval::GoldenSet;

/// Every file under `tests/fixtures/kb-eval/`, relative to that directory and
/// spelled with `/` the way the indexer stores paths (`indexer.rs` normalises
/// `\` away, which is what lets one golden file work on both platforms).
///
/// Listed by hand so that adding, renaming, or deleting a fixture has to be a
/// deliberate edit here as well. Without it, deleting a document silently
/// turns its query into a permanent miss and the corpus quietly shrinks.
const KB_EVAL_FILES: &[&str] = &[
    "guide/branching.md",
    "guide/code-review.ja.md",
    "guide/feature-flags.md",
    "guide/local-setup.md",
    "guide/onboarding.ja.md",
    "guide/testing-strategy.md",
    "guide/writing-docs.ja.md",
    "ops/cost-monitoring.ja.md",
    "ops/database-backup.md",
    "ops/database-restore.md",
    "ops/deploy-canary.ja.md",
    "ops/deploy-rollback.ja.md",
    "ops/incident-postmortem.md",
    "ops/oncall-escalation.ja.md",
    "ref/auth-api-keys.md",
    "ref/auth-oauth.md",
    "ref/cache-invalidation.ja.md",
    "ref/error-codes.ja.md",
    "ref/logging-format.md",
    "ref/rate-limiting.md",
];

/// Number of queries in `tests/fixtures/kb-eval-golden.yml`. Pinned because
/// every aggregate metric is an average over it: dropping the hard half of the
/// golden would raise all three numbers and read as an improvement.
const GOLDEN_QUERY_COUNT: usize = 25;

/// The corpus and the golden are required to stay bilingual (BU-11 asks for a
/// mixed Japanese/English set). Minimums rather than exact counts, so the set
/// can grow without editing this file — but neither language can drain away.
const MIN_JA_DOCS: usize = 9;
const MIN_CJK_QUERIES: usize = 9;
const MIN_NON_CJK_QUERIES: usize = 11;

/// BGE-small floors. Baseline 0.92 / 0.940; FTS-dead 0.80 / 0.835 (see the
/// module docs). Both floors sit above the broken state and below two queries
/// of drift.
const BGE_SMALL_MIN_RECALL_AT_1: f64 = 0.84;
const BGE_SMALL_MIN_MRR: f64 = 0.88;

/// BGE-M3 floors. Baseline is a clean sweep (1.00 / 1.000), so the same
/// "two queries of slack" rule puts the floors here.
const BGE_M3_MIN_RECALL_AT_1: f64 = 0.92;
const BGE_M3_MIN_MRR: f64 = 0.95;

// ---------------------------------------------------------------------------
// Paths
// ---------------------------------------------------------------------------

fn fixtures_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
}

fn corpus_root() -> PathBuf {
    fixtures_root().join("kb-eval")
}

/// The golden lives *beside* the corpus, not inside it, so that editing the
/// query set can never change the document set being measured.
fn golden_file() -> PathBuf {
    fixtures_root().join("kb-eval-golden.yml")
}

fn kb_mcp_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_kb-mcp"))
}

// ---------------------------------------------------------------------------
// Corpus helpers
// ---------------------------------------------------------------------------

/// Relative paths of every file under `root`, `/`-separated and sorted.
fn collect_relative_files(root: &Path) -> Vec<String> {
    fn walk(dir: &Path, base: &Path, out: &mut Vec<String>) {
        let entries = std::fs::read_dir(dir)
            .unwrap_or_else(|e| panic!("read fixture directory {}: {e}", dir.display()));
        for entry in entries {
            let entry = entry.unwrap_or_else(|e| panic!("read entry under {}: {e}", dir.display()));
            let path = entry.path();
            if path.is_dir() {
                walk(&path, base, out);
            } else {
                let rel = path
                    .strip_prefix(base)
                    .unwrap_or(&path)
                    .to_string_lossy()
                    .replace('\\', "/");
                out.push(rel);
            }
        }
    }

    let mut out = Vec::new();
    walk(root, root, &mut out);
    out.sort();
    out
}

/// Assert the fixture directory still holds exactly [`KB_EVAL_FILES`], and
/// return that list.
fn corpus_files() -> Vec<String> {
    let root = corpus_root();
    let actual = collect_relative_files(&root);
    let mut expected: Vec<String> = KB_EVAL_FILES.iter().map(|s| (*s).to_string()).collect();
    expected.sort();
    assert_eq!(
        actual,
        expected,
        "the kb-eval fixture corpus drifted from KB_EVAL_FILES. Update the \
         manifest in tests/eval_corpus_quality.rs and add or remove the \
         matching golden query, then re-measure the baseline recorded in this \
         file's module docs. Corpus root: {}",
        root.display()
    );
    actual
}

/// Copy the fixture corpus into `layout.kb()`, recreating its subdirectories.
fn setup_corpus(layout: &TempKbLayout) {
    let root = corpus_root();
    for rel in corpus_files() {
        let dst = layout.kb().join(&rel);
        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent)
                .unwrap_or_else(|e| panic!("create {}: {e}", parent.display()));
        }
        let src = root.join(&rel);
        std::fs::copy(&src, &dst)
            .unwrap_or_else(|e| panic!("copy fixture {} -> {}: {e}", src.display(), dst.display()));
    }
}

/// Write an empty `kb-mcp.toml` and return its path, to be passed as
/// `--config`.
///
/// Without it the run would take whatever `kb-mcp.toml` config discovery finds
/// from the test process's working directory upwards. That file is user-local
/// and git-ignored, so a developer who has one with, say, `[search.mmr]`
/// enabled would measure a different pipeline than CI and see this gate fail
/// for a reason that has nothing to do with their change.
fn pinned_config(layout: &TempKbLayout) -> PathBuf {
    let path = layout.root().join("kb-mcp.toml");
    std::fs::write(&path, "").expect("write empty kb-mcp.toml");
    path
}

fn has_cjk(s: &str) -> bool {
    s.chars().any(|c| {
        matches!(c,
            '\u{3040}'..='\u{30FF}'   // kana
            | '\u{4E00}'..='\u{9FFF}' // CJK unified ideographs
            | '\u{3400}'..='\u{4DBF}' // extension A
        )
    })
}

// ---------------------------------------------------------------------------
// Running the pipeline
// ---------------------------------------------------------------------------

fn index_corpus(kb: &Path, config: &Path, model: &str) {
    let out = Command::new(kb_mcp_bin())
        .arg("index")
        .arg("--kb-path")
        .arg(kb)
        .arg("--config")
        .arg(config)
        .arg("--model")
        .arg(model)
        .output()
        .expect("spawn kb-mcp index");
    assert!(
        out.status.success(),
        "kb-mcp index failed for {model}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Run `kb-mcp eval` over the committed golden and return its JSON.
///
/// `--no-history` keeps the run stateless: no `.kb-mcp-eval-history.json` is
/// written or read, so the gate measures this build only and never compares
/// against a stale neighbouring run.
fn run_eval(kb: &Path, config: &Path, model: &str) -> serde_json::Value {
    let out = Command::new(kb_mcp_bin())
        .arg("eval")
        .arg("--kb-path")
        .arg(kb)
        .arg("--config")
        .arg(config)
        .arg("--golden")
        .arg(golden_file())
        .arg("--model")
        .arg(model)
        .arg("--format")
        .arg("json")
        .arg("--no-history")
        .output()
        .expect("spawn kb-mcp eval");
    assert!(
        out.status.success(),
        "kb-mcp eval failed for {model}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
        panic!(
            "kb-mcp eval did not print JSON for {model}: {e}\nstdout was:\n{}",
            String::from_utf8_lossy(&out.stdout)
        )
    })
}

fn metric(run: &serde_json::Value, pointer: &str) -> f64 {
    run.pointer(pointer)
        .and_then(|v| v.as_f64())
        .unwrap_or_else(|| panic!("no numeric metric at {pointer} in eval JSON:\n{run}"))
}

/// Human-readable list of the queries that did not rank their expected
/// document first. Included in every failure message: a nightly failure has to
/// be diagnosable from the log alone, without re-running a 2.3 GB model.
fn ranking_report(run: &serde_json::Value) -> String {
    let mut report = String::new();
    for q in run["per_query"].as_array().into_iter().flatten() {
        let rr = q["metrics"]["reciprocal_rank"].as_f64().unwrap_or(0.0);
        if rr >= 1.0 {
            continue;
        }
        let id = q["id"].as_str().unwrap_or("<no id>");
        // Every expected path, not just the first: `reciprocal_rank` is the
        // rank of the *earliest* expected hit, so naming one of several would
        // leave the reader guessing which one the rank refers to.
        let expected: Vec<&str> = q["expected"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|e| e["path"].as_str())
            .collect();
        let expected = if expected.is_empty() {
            "<none>".to_string()
        } else {
            expected.join(", ")
        };
        let top1 = q["top_k"][0]["path"]
            .as_str()
            .unwrap_or("<nothing returned>");
        let position = if rr > 0.0 {
            // `reciprocal_rank` is 1/rank of the first expected hit inside the
            // retrieved window; 0.0 means no expected hit was retrieved at all.
            format!("rank {}", (1.0 / rr).round() as i64)
        } else {
            "outside the retrieved window".to_string()
        };
        report.push_str(&format!(
            "  {id}: expected {expected} at {position}; top-1 was {top1}\n"
        ));
    }
    if report.is_empty() {
        report.push_str("  (every query ranked its expected document first)\n");
    }
    report
}

/// The gate itself. `min_recall_at_1` / `min_mrr` come from the per-model
/// constants; everything else is shared.
fn assert_retrieval_quality(
    run: &serde_json::Value,
    model: &str,
    min_recall_at_1: f64,
    min_mrr: f64,
) {
    let query_count = metric(run, "/aggregate/query_count") as usize;
    assert_eq!(
        query_count, GOLDEN_QUERY_COUNT,
        "{model}: eval measured {query_count} queries but the golden holds \
         {GOLDEN_QUERY_COUNT}; the averages below are not comparable to the \
         recorded baseline"
    );

    let recall_at_1 = metric(run, "/aggregate/recall_at_k/1");
    let recall_at_5 = metric(run, "/aggregate/recall_at_k/5");
    let mrr = metric(run, "/aggregate/mrr");
    let context = format!(
        "{model} over the kb-eval corpus: recall@1={recall_at_1:.3} \
         recall@5={recall_at_5:.3} MRR={mrr:.3}\nqueries that missed rank 1:\n{}",
        ranking_report(run)
    );

    assert!(
        recall_at_1 >= min_recall_at_1,
        "retrieval quality regressed: recall@1 {recall_at_1:.3} < {min_recall_at_1:.3}\n{context}"
    );
    assert!(
        mrr >= min_mrr,
        "retrieval quality regressed: MRR {mrr:.3} < {min_mrr:.3}\n{context}"
    );
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Structural check only — no embedding model, so this one gates pull
/// requests along with the rest of the light suite.
#[test]
fn kb_eval_corpus_and_golden_stay_in_sync() {
    let files = corpus_files();

    let ja_docs = files.iter().filter(|f| f.contains(".ja.")).count();
    assert!(
        ja_docs >= MIN_JA_DOCS,
        "the kb-eval corpus has to stay bilingual (BU-11): {ja_docs} Japanese \
         documents, expected at least {MIN_JA_DOCS}"
    );

    let golden = GoldenSet::load(&golden_file()).expect("load the kb-eval golden");
    assert_eq!(
        golden.queries.len(),
        GOLDEN_QUERY_COUNT,
        "GOLDEN_QUERY_COUNT is stale; re-measure the baseline in this file's \
         module docs after changing the query set"
    );

    let mut ids: Vec<&str> = Vec::with_capacity(golden.queries.len());
    let mut covered: Vec<&str> = Vec::new();
    let mut cjk_queries = 0usize;
    for q in &golden.queries {
        let id = q.id.as_deref().unwrap_or_else(|| {
            panic!("every kb-eval golden query needs an id (offending query: {q:?})")
        });
        ids.push(id);
        if has_cjk(&q.query) {
            cjk_queries += 1;
        }
        assert!(
            !q.expected.is_empty(),
            "golden query {id} expects nothing, so it can never fail"
        );
        for hit in &q.expected {
            assert!(
                files.iter().any(|f| f == &hit.path),
                "golden query {id} expects {}, which is not in the kb-eval \
                 corpus. Every expected path is compared verbatim against the \
                 indexed path, so a typo here is a permanent miss rather than \
                 an error.",
                hit.path
            );
            covered.push(&hit.path);
        }
    }

    let mut sorted_ids = ids.clone();
    sorted_ids.sort_unstable();
    sorted_ids.dedup();
    assert_eq!(
        sorted_ids.len(),
        ids.len(),
        "golden query ids must be unique: {ids:?}"
    );

    let uncovered: Vec<&String> = files
        .iter()
        .filter(|f| !covered.iter().any(|c| c == &f.as_str()))
        .collect();
    assert!(
        uncovered.is_empty(),
        "these kb-eval documents are not the expected answer of any golden \
         query, so nothing would notice if they stopped being retrievable: \
         {uncovered:?}"
    );

    assert!(
        cjk_queries >= MIN_CJK_QUERIES,
        "{cjk_queries} golden queries contain Japanese, expected at least \
         {MIN_CJK_QUERIES} (BU-11 asks for a mixed-language gate)"
    );
    let non_cjk = golden.queries.len() - cjk_queries;
    assert!(
        non_cjk >= MIN_NON_CJK_QUERIES,
        "{non_cjk} golden queries are free of Japanese, expected at least \
         {MIN_NON_CJK_QUERIES} (BU-11 asks for a mixed-language gate)"
    );
}

/// The sensitive leg: BGE-small is English-only, so the Japanese half of the
/// corpus is carried by the FTS trigram leg, which is exactly what a change to
/// query compilation or fusion can break.
#[test]
#[ignore = "indexes the kb-eval corpus with BGE-small (~130 MB model download on first run)"]
fn kb_eval_retrieval_quality_bge_small() {
    let layout = TempKbLayout::new("kb-mcp-eval-quality-small");
    setup_corpus(&layout);
    let config = pinned_config(&layout);

    index_corpus(layout.kb(), &config, "bge-small-en-v1.5");
    let run = run_eval(layout.kb(), &config, "bge-small-en-v1.5");

    assert_retrieval_quality(
        &run,
        "bge-small-en-v1.5",
        BGE_SMALL_MIN_RECALL_AT_1,
        BGE_SMALL_MIN_MRR,
    );
}

/// The Japanese semantic path, on the model a Japanese knowledge base actually
/// runs. Skipped on the Windows nightly leg for the same disk / cache reasons
/// as the other two BGE-M3 tests — see the `skip_args` comment in
/// `.github/workflows/nightly.yml`.
#[test]
#[ignore = "indexes the kb-eval corpus with BGE-M3 (~2.3 GB model download on first run)"]
fn kb_eval_retrieval_quality_bge_m3() {
    let layout = TempKbLayout::new("kb-mcp-eval-quality-m3");
    setup_corpus(&layout);
    let config = pinned_config(&layout);

    index_corpus(layout.kb(), &config, "bge-m3");
    let run = run_eval(layout.kb(), &config, "bge-m3");

    assert_retrieval_quality(&run, "bge-m3", BGE_M3_MIN_RECALL_AT_1, BGE_M3_MIN_MRR);
}
