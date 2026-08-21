//! Retrieval quality gate over the committed `kb-eval` fixture corpus (BU-11).
//!
//! Until this existed, nothing in CI measured whether a change made retrieval
//! *worse*. The recall drop that feature-48 introduced was noticed only by
//! hand, on a private knowledge base, after the release. This file indexes a
//! committed 20-document Japanese/English corpus, runs the committed golden
//! query set through `groove eval`, and fails when the aggregate metrics fall
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
//! # The golden has two groups, and they are averaged separately
//!
//! Twenty-five queries name one document each; five name two. **Their scores
//! are never blended.** A query with two right answers caps recall@1 at 0.5, so
//! averaging the groups together moves the headline number by an amount that
//! depends on how many multi-answer queries the golden happens to hold and not
//! at all on whether retrieval got worse. `aggregate` in the eval JSON is that
//! blend; this file computes its own means from `per_query` instead.
//!
//! # Baseline, measured 2026-08-21 (30 queries, 20 documents, 60 chunks)
//!
//! Single-answer group, 25 queries. Unchanged from the 2026-08-14 measurement,
//! which is the point: adding queries cannot move them, because the golden is
//! never copied into the corpus.
//!
//! | | recall@1 | recall@5 | MRR |
//! |---|---|---|---|
//! | BGE-small, as shipped | 0.92 | 0.96 | 0.940 |
//! | BGE-small, FTS leg forced silent | 0.80 | 0.88 | 0.835 |
//! | BGE-M3, as shipped | 1.00 | 1.00 | 1.000 |
//! | BGE-M3, FTS leg forced silent | 1.00 | 1.00 | 1.000 |
//!
//! Multi-answer group, 5 queries, two expected documents each. Three
//! deliberate breakages rather than one, because the first two moved the other
//! group and left this one where it was.
//!
//! | | recall@1 | recall@5 | MRR | nDCG@5 |
//! |---|---|---|---|---|
//! | BGE-small, as shipped | 0.50 | 0.80 | 1.000 | 0.821 |
//! | BGE-small, FTS leg forced silent | 0.20 | 0.80 | 0.567 | 0.593 |
//! | BGE-small, vector leg forced silent | 0.30 | 0.80 | 0.800 | 0.684 |
//! | BGE-small, candidate over-fetch removed | 0.40 | 0.80 | 0.900 | 0.775 |
//! | BGE-M3, as shipped | 0.50 | 1.00 | 1.000 | 0.927 |
//! | BGE-M3, FTS leg forced silent | 0.50 | 0.90 | 1.000 | 0.861 |
//! | BGE-M3, vector leg forced silent | 0.30 | 0.80 | 0.800 | 0.684 |
//! | BGE-M3, candidate over-fetch removed | 0.50 | 1.00 | 1.000 | 0.913 |
//!
//! The broken rows come from scratch builds: `build_fts_query` returning
//! `None`, `search_split_candidates` returning an empty vector-leg list, and
//! `search_hybrid_candidates` asking for `limit` candidates instead of
//! `limit * 5`. The two models agree exactly with the vector leg silent, which
//! is the check that the probe silenced what it meant to.
//!
//! Six conclusions are baked into the thresholds below:
//!
//! 1. **BGE-small is the sensitive leg.** Killing the keyword half moves it by
//!    0.12 recall@1 / 0.105 MRR. Four queries degrade, three of them Japanese
//!    natural-language ones — the feature-48 class exactly.
//! 2. **BGE-M3 is blind to it at this corpus size.** Twenty semantically
//!    distinct documents are separable by the vector leg alone, so BGE-M3
//!    answers every query, keyword half or no keyword half. Its gate therefore
//!    guards the Japanese *semantic* path and catches gross regressions; it is
//!    not, and cannot be here, an FTS regression detector.
//! 3. **recall@5 is not asserted on the single-answer group.** Healthy 0.96 and
//!    FTS-dead 0.88 are only two queries apart, so any threshold loose enough
//!    to survive ordinary drift is also loose enough to sit below the broken
//!    state. It is printed in the failure report instead.
//! 4. **recall@5 is the only metric that carries information on the
//!    multi-answer group, and it is the one asserted there.** recall@1 sits at
//!    its ceiling of 0.50 on both models, and MRR is 1.000 on both, because
//!    every one of the five puts one of its two documents at rank 1 — which is
//!    exactly why these queries were needed: the metrics that gate the other
//!    group are blind to whether the *second* right answer came back at all.
//!    recall@5 separates the models 0.80 against 1.00, where on the
//!    single-answer group the same metric separates them 0.96 against 1.00.
//! 5. **BGE-small's multi recall@5 is 0.80 in every state measured**, healthy
//!    and with either retrieval leg dead. Its floor is a pin, not a detector:
//!    it records a level so that a future change dropping it is a named
//!    failure, in the way `KB_EVAL_FILES` records a file list. The metric that
//!    does move for BGE-small here is nDCG@5 (0.821 → 0.593 → 0.684 → 0.775),
//!    but every one of those breakages already fails this file's BGE-small
//!    recall@1 floor, so asserting it too would add a second way to hear about
//!    something already heard.
//! 6. **BGE-M3's multi floor is the one that can fail on its own.** A dead
//!    vector leg takes it from 1.00 to 0.80, below the 0.90 floor. In that
//!    particular run the recall@1 assertion fires first and reports it, so
//!    what the multi floor adds there is the *shape* — printed by
//!    `incomplete_report`, which names the document that left the top 5 — and
//!    not the detection. What it would catch alone is a change that costs the
//!    second right answer while leaving the first at rank 1, which is the one
//!    kind of regression the other two assertions cannot see at all.
//!
//! None of the three breakages above was needed to show the multi assertion
//! fires: raising `BGE_M3_MIN_MULTI_RECALL_AT_5` to 1.01 against the shipped
//! build does that, and the message it prints is
//! `multi-answer recall@5 1.000 < 1.010`.
//!
//! Single-answer thresholds allow **two queries of drift and trip on the
//! third**, which is the slack those scores need: RRF fusion runs on `f32`, and
//! near-ties can come apart differently on another architecture (the same
//! reason `common::mcp::extract_path_heading_order` compares paths instead of
//! scores). A real retrieval regression moves many queries at once.
//!
//! **The multi-answer floors allow one half-answer, not two.** Five queries
//! average less than twenty-five do, so the same absolute slack would put the
//! floor at 0.60 for BGE-small — under 0.50 + one half-answer, which is to say
//! under a search that returns exactly one document of every pair. Measurement
//! is what makes the tighter slack safe: this metric held at 0.80 across three
//! deliberate breakages, so it is not a number that drifts.

use std::path::{Path, PathBuf};
use std::process::Command;

mod common;
use common::temp::TempKbLayout;

use grooveseek::eval::{ExpectedHit, GoldenSet, HitRecord, is_hit};

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
const GOLDEN_QUERY_COUNT: usize = 30;

/// And how that total splits, because the two groups are averaged separately
/// and neither mean is comparable to the recorded baseline if its population
/// changed. Pinning only the total would let a multi-answer query be rewritten
/// into a single-answer one without anything noticing — which is the cheapest
/// way to make this file's hardest queries disappear.
const GOLDEN_SINGLE_ANSWER_QUERY_COUNT: usize = 25;
const GOLDEN_MULTI_ANSWER_QUERY_COUNT: usize = 5;

/// And how many documents each multi-answer query names. Pinned because the
/// multi floors are stated in half-answers: five queries naming two documents
/// each is ten of them, so one half-answer of drift is 0.1 and the floors below
/// are one step under their measured baselines. A sixth expectation on some
/// query, or the same path written twice, changes that arithmetic — the
/// recall denominator with it — while leaving every count above satisfied.
///
/// The duplicate is the worse of the two: `recall_at_k` scores each expectation
/// independently, so two identical entries are both satisfied by one retrieved
/// document and the query reports perfect recall for half the work.
const MULTI_ANSWER_EXPECTED_PER_QUERY: usize = 2;

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

/// Multi-answer floors, on recall@5 — the only metric that is not saturated
/// there (module docs, conclusion 4). Baselines 0.80 and 1.00 over ten
/// half-answers, and **one** half-answer of slack rather than two: with five
/// queries there is less averaging, and two would put the BGE-small floor
/// below what a search returning one document of every pair scores.
const BGE_SMALL_MIN_MULTI_RECALL_AT_5: f64 = 0.70;
const BGE_M3_MIN_MULTI_RECALL_AT_5: f64 = 0.90;

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

fn grooveseek_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_groove"))
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

/// Write an empty `groove.toml` and return its path, to be passed as
/// `--config`.
///
/// Without it the run would take whatever `groove.toml` config discovery finds
/// from the test process's working directory upwards. That file is user-local
/// and git-ignored, so a developer who has one with, say, `[search.mmr]`
/// enabled would measure a different pipeline than CI and see this gate fail
/// for a reason that has nothing to do with their change.
fn pinned_config(layout: &TempKbLayout) -> PathBuf {
    let path = layout.root().join("groove.toml");
    std::fs::write(&path, "").expect("write empty groove.toml");
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
    let out = Command::new(grooveseek_bin())
        .arg("index")
        .arg("--kb-path")
        .arg(kb)
        .arg("--config")
        .arg(config)
        .arg("--model")
        .arg(model)
        .output()
        .expect("spawn groove index");
    assert!(
        out.status.success(),
        "groove index failed for {model}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Run `groove eval` over the committed golden and return its JSON.
///
/// `--no-history` keeps the run stateless: no `.groove-eval-history.json` is
/// written or read, so the gate measures this build only and never compares
/// against a stale neighbouring run.
fn run_eval(kb: &Path, config: &Path, model: &str) -> serde_json::Value {
    let out = Command::new(grooveseek_bin())
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
        .expect("spawn groove eval");
    assert!(
        out.status.success(),
        "groove eval failed for {model}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
        panic!(
            "groove eval did not print JSON for {model}: {e}\nstdout was:\n{}",
            String::from_utf8_lossy(&out.stdout)
        )
    })
}

fn metric(run: &serde_json::Value, pointer: &str) -> f64 {
    run.pointer(pointer)
        .and_then(|v| v.as_f64())
        .unwrap_or_else(|| panic!("no numeric metric at {pointer} in eval JSON:\n{run}"))
}

/// **The** definition of which group a golden query belongs to.
///
/// Both callers reach this one: the model-free sync test, which has
/// `GoldenQuery` values, and the gate, which has the JSON `groove eval`
/// printed. Two spellings of the same predicate could disagree after a change
/// to what "multi-answer" means — say, exactly two expectations rather than at
/// least two — and the shape of that disagreement is a pull request passing
/// while the nightly averages a different population.
fn is_multi_answer(expected_count: usize) -> bool {
    expected_count > 1
}

/// How many documents a `per_query` entry from the eval JSON expects. Read
/// from the run rather than from the golden file, so the split is the one
/// `groove eval` actually scored.
fn expected_count(q: &serde_json::Value) -> usize {
    q["expected"].as_array().map_or(0, |e| e.len())
}

/// The mean of one per-query metric over one group, and that group's size.
///
/// Not `aggregate`: that is the blend of both groups, and the module docs say
/// why this file must not read it. `pointer` is relative to a `per_query`
/// entry, e.g. `/metrics/recall_at_k/5`.
fn group_mean(run: &serde_json::Value, multi: bool, pointer: &str) -> (usize, f64) {
    let values: Vec<f64> = run["per_query"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|q| is_multi_answer(expected_count(q)) == multi)
        .map(|q| {
            q.pointer(pointer)
                .and_then(|v| v.as_f64())
                .unwrap_or_else(|| {
                    panic!("no numeric metric at {pointer} in per-query entry:\n{q}")
                })
        })
        .collect();
    assert!(
        !values.is_empty(),
        "the eval run has no {} queries, so its mean at {pointer} would be an \
         empty average rather than a measurement",
        if multi {
            "multi-answer"
        } else {
            "single-answer"
        }
    );
    let n = values.len();
    (n, values.iter().sum::<f64>() / n as f64)
}

/// Human-readable list of the multi-answer queries that did not get every
/// expected document into the top 5.
///
/// The list above it cannot show these: a multi-answer query that returns one
/// of its two documents at rank 1 has a reciprocal rank of 1.0 and looks
/// perfect there, which is the whole reason this group exists.
fn incomplete_report(run: &serde_json::Value) -> String {
    let mut report = String::new();
    for q in run["per_query"].as_array().into_iter().flatten() {
        if !is_multi_answer(expected_count(q)) {
            continue;
        }
        let at_5 = q["metrics"]["recall_at_k"]["5"].as_f64().unwrap_or(0.0);
        if at_5 >= 1.0 {
            continue;
        }
        let id = q["id"].as_str().unwrap_or("<no id>");
        // `eval::is_hit`, the predicate `recall_at_k` scored with, over the
        // same window it used. Comparing paths here instead would be a second
        // and weaker definition of a hit: it calls a chunk from the right file
        // under the wrong heading "returned", so this report would leave out
        // the very expectation that caused the number it is explaining.
        let expected: Vec<ExpectedHit> = serde_json::from_value(q["expected"].clone())
            .unwrap_or_else(|e| panic!("per-query `expected` did not parse: {e}\n{q}"));
        let top: Vec<HitRecord> = serde_json::from_value(q["top_k"].clone())
            .unwrap_or_else(|e| panic!("per-query `top_k` did not parse: {e}\n{q}"));
        let missing: Vec<String> = expected
            .iter()
            .filter(|e| !top.iter().take(5).any(|h| is_hit(e, h)))
            .map(|e| match &e.heading {
                Some(h) => format!("{} ({h})", e.path),
                None => e.path.clone(),
            })
            .collect();
        report.push_str(&format!(
            "  {id}: recall@5 {at_5:.2}; missing from the top 5: {}\n",
            missing.join(", ")
        ));
    }
    if report.is_empty() {
        report.push_str("  (every multi-answer query returned both documents in the top 5)\n");
    }
    report
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
    min_multi_recall_at_5: f64,
) {
    let query_count = metric(run, "/aggregate/query_count") as usize;
    assert_eq!(
        query_count, GOLDEN_QUERY_COUNT,
        "{model}: eval measured {query_count} queries but the golden holds \
         {GOLDEN_QUERY_COUNT}; the averages below are not comparable to the \
         recorded baseline"
    );

    // Each group's own mean. `aggregate` blends them, and a blend of a metric
    // whose ceiling differs between the two groups is not a measurement of
    // anything (module docs, "The golden has two groups").
    let (single_count, recall_at_1) = group_mean(run, false, "/metrics/recall_at_k/1");
    let (_, recall_at_5) = group_mean(run, false, "/metrics/recall_at_k/5");
    let (_, mrr) = group_mean(run, false, "/metrics/reciprocal_rank");
    let (multi_count, multi_recall_at_5) = group_mean(run, true, "/metrics/recall_at_k/5");
    let (_, multi_recall_at_1) = group_mean(run, true, "/metrics/recall_at_k/1");
    let (_, multi_mrr) = group_mean(run, true, "/metrics/reciprocal_rank");

    assert_eq!(
        single_count, GOLDEN_SINGLE_ANSWER_QUERY_COUNT,
        "{model}: {single_count} single-answer queries were scored but the \
         golden holds {GOLDEN_SINGLE_ANSWER_QUERY_COUNT}; the floors below were \
         measured over the recorded population"
    );
    assert_eq!(
        multi_count, GOLDEN_MULTI_ANSWER_QUERY_COUNT,
        "{model}: {multi_count} multi-answer queries were scored but the golden \
         holds {GOLDEN_MULTI_ANSWER_QUERY_COUNT}; the floors below were measured \
         over the recorded population"
    );

    let context = format!(
        "{model} over the kb-eval corpus\n  single-answer ({single_count}): \
         recall@1={recall_at_1:.3} recall@5={recall_at_5:.3} MRR={mrr:.3}\n  \
         multi-answer ({multi_count}): recall@1={multi_recall_at_1:.3} \
         recall@5={multi_recall_at_5:.3} MRR={multi_mrr:.3}\n\
         queries that missed rank 1:\n{}\
         multi-answer queries that did not return everything expected:\n{}",
        ranking_report(run),
        incomplete_report(run)
    );

    assert!(
        recall_at_1 >= min_recall_at_1,
        "retrieval quality regressed: recall@1 {recall_at_1:.3} < {min_recall_at_1:.3}\n{context}"
    );
    assert!(
        mrr >= min_mrr,
        "retrieval quality regressed: MRR {mrr:.3} < {min_mrr:.3}\n{context}"
    );
    // The one metric the other two cannot stand in for: with two right answers
    // recall@1 is capped at 0.5 and a reciprocal rank of 1.0 is earned by
    // returning either one of them, so both are silent about the second.
    assert!(
        multi_recall_at_5 >= min_multi_recall_at_5,
        "retrieval quality regressed: multi-answer recall@5 {multi_recall_at_5:.3} \
         < {min_multi_recall_at_5:.3}\n{context}"
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

    // The split, checked here as well as in the gate, because this test needs
    // no model and so runs on every pull request. Rewriting a multi-answer
    // query down to one expected document would otherwise only be caught by
    // the nightly, and it is the edit that quietly removes what the
    // multi-answer group measures — recall@1 and MRR cannot see the second
    // right answer, so nothing else in the suite would notice.
    let multi = golden
        .queries
        .iter()
        .filter(|q| is_multi_answer(q.expected.len()))
        .count();
    assert_eq!(
        multi, GOLDEN_MULTI_ANSWER_QUERY_COUNT,
        "{multi} golden queries name more than one document, expected \
         {GOLDEN_MULTI_ANSWER_QUERY_COUNT}"
    );
    assert_eq!(
        golden.queries.len() - multi,
        GOLDEN_SINGLE_ANSWER_QUERY_COUNT,
        "the single-answer group changed size; the floors in this file were \
         measured over {GOLDEN_SINGLE_ANSWER_QUERY_COUNT} queries"
    );

    for q in golden
        .queries
        .iter()
        .filter(|q| is_multi_answer(q.expected.len()))
    {
        let id = q.id.as_deref().unwrap_or("<no id>");
        assert_eq!(
            q.expected.len(),
            MULTI_ANSWER_EXPECTED_PER_QUERY,
            "golden query {id} names {} documents; the multi-answer floors are \
             stated in half-answers and assume {MULTI_ANSWER_EXPECTED_PER_QUERY} \
             per query, so this needs a re-measurement rather than a passing count",
            q.expected.len()
        );
        for (i, e) in q.expected.iter().enumerate() {
            assert!(
                !q.expected[..i].contains(e),
                "golden query {id} expects {} twice; both entries would be \
                 satisfied by one retrieved document, so the query would report \
                 perfect recall for half the work",
                e.path
            );
        }
    }

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
    let layout = TempKbLayout::new("groove-eval-quality-small");
    setup_corpus(&layout);
    let config = pinned_config(&layout);

    index_corpus(layout.kb(), &config, "bge-small-en-v1.5");
    let run = run_eval(layout.kb(), &config, "bge-small-en-v1.5");

    assert_retrieval_quality(
        &run,
        "bge-small-en-v1.5",
        BGE_SMALL_MIN_RECALL_AT_1,
        BGE_SMALL_MIN_MRR,
        BGE_SMALL_MIN_MULTI_RECALL_AT_5,
    );
}

/// The Japanese semantic path, on the model a Japanese knowledge base actually
/// runs. Skipped on the Windows nightly leg for the same disk / cache reasons
/// as the other two BGE-M3 tests — see the `skip_args` comment in
/// `.github/workflows/nightly.yml`.
#[test]
#[ignore = "indexes the kb-eval corpus with BGE-M3 (~2.3 GB model download on first run)"]
fn kb_eval_retrieval_quality_bge_m3() {
    let layout = TempKbLayout::new("groove-eval-quality-m3");
    setup_corpus(&layout);
    let config = pinned_config(&layout);

    index_corpus(layout.kb(), &config, "bge-m3");
    let run = run_eval(layout.kb(), &config, "bge-m3");

    assert_retrieval_quality(
        &run,
        "bge-m3",
        BGE_M3_MIN_RECALL_AT_1,
        BGE_M3_MIN_MRR,
        BGE_M3_MIN_MULTI_RECALL_AT_5,
    );
}
