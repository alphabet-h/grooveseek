//! The `groove eval` transcript in the quick start is what
//! [`grooveseek::eval::format_text`] prints.
//!
//! The block was typed by hand. It drifted from the formatter in four places at
//! once while both pages looked fine -- no `corpus:` line, a trailing phrase the
//! formatter has no code for, a timestamp offset the binary cannot write, and an
//! aggregate that could not have come from its own per-query row -- and the
//! Japanese half lost its `Per-query` section on top. A transcript is not a
//! translation; its source of truth is the formatter, so that is what it is
//! compared with.
//!
//! # What is checked
//!
//! On each of `docs/eval.md` and `docs/eval.ja.md`: the one `yaml` fence whose
//! body starts with `queries:` is parsed as the golden set the page shows, a
//! run is built from it under the retrieval the example assumes (both hits of
//! the first query found, at ranks 1 and 3; the second query missed), and the
//! one untagged fence whose body starts with `groove eval — ` has to equal
//! [`grooveseek::eval::format_text`] over that run, character for character.
//! The row id, the
//! padding, the numbers and the layout all follow from the golden and the
//! formatter; none of them is written down here.
//!
//! # What this cannot catch
//!
//! The ranks are the example's premise, not a measurement: nothing here runs a
//! search, so a golden whose answers a real index would not find at those ranks
//! passes. Every other untagged block on every other page is still unread.

mod common;

use chrono::{TimeZone, Utc};
use common::docs::{fenced_blocks, read, repo_root, shown};
use grooveseek::eval::{
    ConfigFingerprint, CorpusSnapshot, DEFAULT_K_VALUES, DEFAULT_REGRESSION_THRESHOLD, EvalRun,
    FTS_QUERY_VERSION, GoldenSet, HitRecord, METRIC_VERSION, QueryResult, aggregate_metrics,
    compute_query_metrics, format_text, query_id,
};

// The example is the documented invocation with no `--k` and no `--limit`, so
// the k values, the limit (the largest k, as `main.rs` resolves it) and the
// threshold are the binary's defaults, read from where the binary reads them.
// A copy of `[1, 5, 10]` here would keep this guard green after the default
// moved, while the page showed labels the binary no longer prints.

fn hit(rank: usize, path: &str, heading: Option<&str>) -> HitRecord {
    HitRecord {
        rank,
        path: path.to_string(),
        heading: heading.map(str::to_string),
        score: 1.0,
    }
}

/// The run the quick start shows: its golden, under the retrieval the example
/// assumes. Ranks are 1-based, as [`grooveseek::eval::reciprocal_rank`] requires.
fn quick_start_run(golden: &GoldenSet) -> EvalRun {
    let [q1, q2] = golden.queries.as_slice() else {
        panic!(
            "the quick start golden has two queries, found {}",
            golden.queries.len()
        );
    };
    assert_eq!(
        q1.expected.len(),
        2,
        "the first quick start query expects two hits, so the example can show a partial recall@1"
    );
    assert_eq!(
        q2.expected.len(),
        1,
        "the second quick start query expects one hit, which the example misses"
    );
    assert!(
        q2.id.is_none(),
        "the second quick start query has no id, so its row id is derived from its text"
    );

    let e = &q1.expected;
    let top1 = vec![
        hit(1, &e[0].path, e[0].heading.as_deref()),
        hit(2, "docs/usage.md", Some("Searching")),
        hit(3, &e[1].path, Some("Schema")),
    ];
    let top2 = vec![hit(1, "docs/usage.md", Some("Indexing"))];
    let per_query = vec![
        QueryResult {
            id: query_id(q1),
            query: q1.query.clone(),
            expected: q1.expected.clone(),
            metrics: compute_query_metrics(&q1.expected, &top1, &DEFAULT_K_VALUES),
            top_k: top1,
        },
        QueryResult {
            id: query_id(q2),
            query: q2.query.clone(),
            expected: q2.expected.clone(),
            metrics: compute_query_metrics(&q2.expected, &top2, &DEFAULT_K_VALUES),
            top_k: top2,
        },
    ];
    let aggregate = aggregate_metrics(&per_query, &DEFAULT_K_VALUES);
    let limit = *DEFAULT_K_VALUES
        .iter()
        .max()
        .expect("the default k list is not empty") as u32;
    EvalRun {
        timestamp: Utc.with_ymd_and_hms(2026, 4, 24, 5, 32, 1).unwrap(),
        fingerprint: ConfigFingerprint {
            model: "bge-m3".to_string(),
            reranker: None,
            limit,
            k_values: DEFAULT_K_VALUES.to_vec(),
            golden_hash: "not printed".to_string(),
            metric_version: METRIC_VERSION,
            fts_query_version: FTS_QUERY_VERSION,
            mmr: None,
            parent_retriever: None,
            fusion: None,
            context: None,
        },
        // The same index the page's later sections show, so one knowledge base
        // runs through the whole page.
        corpus: Some(CorpusSnapshot {
            documents: 646,
            chunks: 11_215,
            digest: "not printed".to_string(),
        }),
        per_query,
        aggregate,
        findings: Vec::new(),
    }
}

fn transcript_is_what_format_text_prints(page: &str) {
    let root = repo_root();
    let path = root.join(page);
    let where_ = shown(&root, &path);
    let markdown = read(&path);
    let blocks = fenced_blocks(&markdown);

    let goldens: Vec<_> = blocks
        .iter()
        .filter(|b| b.tag == "yaml" && b.body.starts_with("queries:"))
        .collect();
    assert_eq!(
        goldens.len(),
        1,
        "{where_}: expected one yaml fence starting with `queries:`, the quick start golden"
    );
    let golden: GoldenSet = serde_yaml_bw::from_str(&goldens[0].body).unwrap_or_else(|e| {
        panic!(
            "{where_}:{}: the quick start golden does not parse as a golden set: {e}",
            goldens[0].line
        )
    });

    let transcripts: Vec<_> = blocks
        .iter()
        .filter(|b| b.tag.is_empty() && b.body.starts_with("groove eval \u{2014} "))
        .collect();
    assert_eq!(
        transcripts.len(),
        1,
        "{where_}: expected one untagged fence starting with the `groove eval` banner"
    );

    let expected = format_text(
        &quick_start_run(&golden),
        None,
        false,
        DEFAULT_REGRESSION_THRESHOLD,
    );
    assert_eq!(
        transcripts[0].body, expected,
        "{where_}:{}: the quick start transcript is not what format_text prints over the golden above it",
        transcripts[0].line
    );
}

#[test]
fn the_english_quick_start_transcript_is_what_format_text_prints() {
    transcript_is_what_format_text_prints("docs/eval.md");
}

#[test]
fn the_japanese_quick_start_transcript_is_what_format_text_prints() {
    transcript_is_what_format_text_prints("docs/eval.ja.md");
}
