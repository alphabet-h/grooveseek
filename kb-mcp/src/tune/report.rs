//! Rendering a sweep result for stdout.
//!
//! Per the CLI output convention, `tune` writes its result to **stdout** and
//! its progress to stderr — and the text branch uses `print!`, not
//! `println!`, which is what a subprocess test has to know to read it.
//!
//! Split out of `tune.rs` in AU-31. Contents are byte-identical.

use super::*;

// ---------------------------------------------------------------------------
// Formatters (stdout = 結果、CLI 出力規約)
// ---------------------------------------------------------------------------

/// 採用推奨時に stdout へ出す、そのまま `kb-mcp.toml` に貼れるスニペット。
pub fn toml_snippet(c: Condition) -> String {
    let p = c.to_params();
    // 整数値も **必ず小数点付き** で出す。TOML の `10` は integer リテラルで
    // あり、serde は f32 フィールドへの integer を受け付けず
    // "invalid type: integer `10`, expected f32" で落ちる。
    let fmt = |v: f32| {
        if v.fract() == 0.0 {
            format!("{v:.1}")
        } else {
            format!("{v}")
        }
    };
    format!(
        "[search.fusion]\n\
         rrf_k = {}\n\
         bm25_heading_weight = {}\n\
         bm25_context_weight = {}\n\
         bm25_content_weight = {}\n",
        fmt(p.rrf_k),
        fmt(p.bm25_heading_weight),
        fmt(p.bm25_context_weight),
        fmt(p.bm25_content_weight)
    )
}

pub fn format_text(report: &TuneReport, use_color: bool) -> String {
    use std::fmt::Write;
    let (bold, dim, reset) = if use_color {
        ("\x1b[1m", "\x1b[90m", "\x1b[0m")
    } else {
        ("", "", "")
    };
    let mut s = String::new();
    let n_eff = report.effective.len();

    writeln!(s, "{bold}kb-mcp tune{reset} — {}", report.kb_path.display()).unwrap();
    writeln!(
        s,
        "  golden: {}   model: {}   reranker: none (tune always measures the plain RRF stage)",
        report.golden_path.display(),
        report.model
    )
    .unwrap();
    writeln!(
        s,
        "  limit: {}   candidate pool: {} (floor, not a cap)   chunks in index: {}",
        report.limit, report.pool_size, report.chunk_total
    )
    .unwrap();
    writeln!(
        s,
        "  queries: {} total / {} effective (FTS candidates >= 2)   primary metric: nDCG@{}",
        report.query_count, n_eff, PRIMARY_K
    )
    .unwrap();
    if report.context_axis_noop {
        writeln!(
            s,
            "  WARNING: every chunk has an empty context column, so the bm25_context_weight axis \
             is a no-op here (contextual retrieval is off) — read its rows as \"not measured\"."
        )
        .unwrap();
    }
    writeln!(s).unwrap();

    // --- 5 / 6: 診断 ---
    writeln!(s, "{bold}## Query diagnostics{reset}").unwrap();
    writeln!(
        s,
        "  {:<28} {:>5} {:>8} {:>8} {:>6} {:>5} {:>5}",
        "query", "fts", "docfreq", "overlap", "bm25?", "idf", "ties"
    )
    .unwrap();
    for (id, d) in &report.diagnostics {
        let short: String = id.chars().take(28).collect();
        writeln!(
            s,
            "  {:<28} {:>5} {:>8} {:>8} {:>6} {:>5} {:>5}",
            short,
            d.fts_candidates,
            d.fts_total_matches,
            d.vec_fts_overlap,
            if d.bm25_sensitive { "yes" } else { "no" },
            if d.idf_clamped { "CLMP" } else { "ok" },
            d.rrf_ties
        )
        .unwrap();
    }
    writeln!(
        s,
        "{dim}  fts = FTS candidates in pool; docfreq = chunks matching the whole phrase;\n  \
         overlap = chunks present in BOTH the vec pool and the FTS list (0 means rrf_k cannot\n  \
         change the ranking for that query); bm25? = ranking moved between the grid's\n  \
         heading-heavy and content-heavy extremes; idf = CLMP means the phrase occurs in >= half\n  \
         of all chunks, so FTS5 clamps its IDF to 1e-6 and no weight can revive it;\n  \
         ties = adjacent equal f32 RRF scores at the default condition (informational).{reset}"
    )
    .unwrap();
    writeln!(s).unwrap();

    // --- grid ---
    writeln!(
        s,
        "{bold}## Grid (coordinate descent over {} effective queries){reset}",
        n_eff
    )
    .unwrap();
    writeln!(s, "  Phase W — top 5 bm25 weight sets at k=60:").unwrap();
    for (c, v) in &report.top_weight_conditions {
        writeln!(s, "    {:<34} nDCG@{PRIMARY_K} {v:.4}", c.label()).unwrap();
    }
    writeln!(s, "  Phase K — rrf_k sweep at the winning weights:").unwrap();
    for (c, v) in &report.top_k_conditions {
        writeln!(s, "    {:<34} nDCG@{PRIMARY_K} {v:.4}", c.label()).unwrap();
    }
    writeln!(s).unwrap();

    // --- 1 / 2: nested LOO ---
    let m = mean(&report.outcome.diffs);
    let se = paired_se(&report.outcome.diffs);
    writeln!(s, "{bold}## Nested leave-one-query-out CV{reset}").unwrap();
    writeln!(
        s,
        "  refit (all {n_eff} queries): {}",
        report.outcome.refit.label()
    )
    .unwrap();
    writeln!(s, "  folds: {}", report.outcome.fold_selections.len()).unwrap();
    writeln!(
        s,
        "  held-out mean delta vs default: {m:+.4}   (threshold {ADOPT_MIN_MEAN_DELTA:.4})"
    )
    .unwrap();
    writeln!(
        s,
        "  paired SE (SD/sqrt(N)): {se:.4}   {ADOPT_SE_MULTIPLIER:.1} x SE: {:.4}",
        ADOPT_SE_MULTIPLIER * se
    )
    .unwrap();
    writeln!(
        s,
        "  sign test: {} improved / {} degraded / {} tied   two-sided p = {:.4}",
        report.sign.positive, report.sign.negative, report.sign.ties, report.sign.p_value
    )
    .unwrap();
    writeln!(
        s,
        "  selection stability: {:.2} of folds picked the refit condition (threshold > {STABILITY_MIN:.2})",
        report.outcome.stability
    )
    .unwrap();
    writeln!(s).unwrap();

    // --- 3: 全指標非悪化 ---
    writeln!(
        s,
        "{bold}## Secondary metrics (all {} golden queries){reset}",
        report.query_count
    )
    .unwrap();
    for &k in &report.k_values {
        let b = report.baseline.recall_at_k.get(&k).copied().unwrap_or(0.0);
        let c = report
            .refit_aggregate
            .recall_at_k
            .get(&k)
            .copied()
            .unwrap_or(0.0);
        writeln!(
            s,
            "  recall@{k:<3} default {b:.4} -> refit {c:.4} ({:+.4})",
            c - b
        )
        .unwrap();
    }
    writeln!(
        s,
        "  MRR      default {:.4} -> refit {:.4} ({:+.4})",
        report.baseline.mrr,
        report.refit_aggregate.mrr,
        report.refit_aggregate.mrr - report.baseline.mrr
    )
    .unwrap();
    if report.violations.is_empty() {
        writeln!(s, "  no secondary metric degraded").unwrap();
    } else {
        for v in &report.violations {
            writeln!(s, "  DEGRADED: {v}").unwrap();
        }
    }
    writeln!(s).unwrap();

    // --- 4: per-query 内訳 ---
    writeln!(s, "{bold}## Per-query impact (refit vs default){reset}").unwrap();
    writeln!(
        s,
        "  improved: {}   degraded: {}   worst delta: {:+.4}{}",
        report.impact.improved,
        report.impact.degraded,
        report.impact.worst_delta,
        match report.impact.worst_query {
            Some(q) => format!(
                " ({})",
                report
                    .diagnostics
                    .get(q)
                    .map(|d| d.0.as_str())
                    .unwrap_or("?")
            ),
            None => String::new(),
        }
    )
    .unwrap();
    writeln!(
        s,
        "{dim}  Rank fusion routinely hides per-query losses behind an average gain\n  \
         (Benham & Culpepper), so read this line before the average.{reset}"
    )
    .unwrap();
    writeln!(s).unwrap();

    // --- verdict ---
    writeln!(s, "{bold}## Verdict{reset}").unwrap();
    match &report.verdict {
        Verdict::KeepDefault { reasons } => {
            writeln!(
                s,
                "  Recommendation: keep the built-in defaults ({}).",
                Condition::builtin_default().label()
            )
            .unwrap();
            for r in reasons {
                writeln!(s, "    - {r}").unwrap();
            }
            writeln!(
                s,
                "{dim}  This is a normal and expected outcome. RRF is documented as requiring no\n  \
                 tuning, and in-domain tuned constants are known not to transfer.{reset}"
            )
            .unwrap();
        }
        Verdict::Adopt(c) => {
            writeln!(s, "  Recommendation: {}", c.label()).unwrap();
            writeln!(
                s,
                "  All adoption conditions passed. Paste this into kb-mcp.toml, then re-run\n  \
                 `kb-mcp eval` WITH your reranker (and `[contextual]`) enabled to confirm the\n  \
                 gain survives the full pipeline before keeping it."
            )
            .unwrap();
            writeln!(s).unwrap();
            write!(s, "{}", toml_snippet(*c)).unwrap();
        }
    }
    s
}

pub fn format_json(report: &TuneReport) -> serde_json::Value {
    let m = mean(&report.outcome.diffs);
    let se = paired_se(&report.outcome.diffs);
    // 実効 N=1 では SE が INFINITY になる。`json!` の中に if 式を埋めると
    // 読みづらいので手前で束縛しておく。
    let se_json = if se.is_finite() {
        serde_json::json!(se)
    } else {
        serde_json::Value::Null
    };
    let cond_json = |c: Condition| {
        let p = c.to_params();
        serde_json::json!({
            "rrf_k": p.rrf_k,
            "bm25_heading_weight": p.bm25_heading_weight,
            "bm25_context_weight": p.bm25_context_weight,
            "bm25_content_weight": p.bm25_content_weight,
        })
    };
    let verdict = match &report.verdict {
        Verdict::Adopt(c) => serde_json::json!({
            "decision": "adopt",
            "condition": cond_json(*c),
            "toml_snippet": toml_snippet(*c),
        }),
        Verdict::KeepDefault { reasons } => serde_json::json!({
            "decision": "keep_default",
            "reasons": reasons,
        }),
    };
    serde_json::json!({
        "kb_path": report.kb_path.to_string_lossy(),
        "golden_path": report.golden_path.to_string_lossy(),
        "model": report.model,
        "reranker": serde_json::Value::Null,
        "limit": report.limit,
        "candidate_pool": report.pool_size,
        "primary_k": PRIMARY_K,
        "k_values": report.k_values,
        "chunk_total": report.chunk_total,
        "context_axis_noop": report.context_axis_noop,
        "query_count": report.query_count,
        "effective_query_count": report.effective.len(),
        "diagnostics": report.diagnostics.iter().map(|(id, d)| serde_json::json!({
            "id": id,
            "fts_candidates": d.fts_candidates,
            "fts_total_matches": d.fts_total_matches,
            "vec_fts_overlap": d.vec_fts_overlap,
            "bm25_sensitive": d.bm25_sensitive,
            "idf_clamped": d.idf_clamped,
            "rrf_ties": d.rrf_ties,
            "effective": d.is_effective(),
        })).collect::<Vec<_>>(),
        "grid": {
            "rrf_k": RRF_K_GRID,
            "bm25_weights": BM25_WEIGHT_GRID,
            "top_weight_conditions": report.top_weight_conditions.iter()
                .map(|(c, v)| serde_json::json!({"condition": cond_json(*c), "ndcg": v}))
                .collect::<Vec<_>>(),
            "rrf_k_sweep": report.top_k_conditions.iter()
                .map(|(c, v)| serde_json::json!({"condition": cond_json(*c), "ndcg": v}))
                .collect::<Vec<_>>(),
        },
        "loo": {
            "refit": cond_json(report.outcome.refit),
            "folds": report.outcome.fold_selections.len(),
            "held_out_diffs": report.outcome.diffs,
            "mean_delta": m,
            "paired_se": se_json,
            "selection_stability": report.outcome.stability,
            "sign_test": {
                "improved": report.sign.positive,
                "degraded": report.sign.negative,
                "tied": report.sign.ties,
                "p_value": report.sign.p_value,
            },
        },
        "secondary_metrics": {
            "baseline": report.baseline,
            "refit": report.refit_aggregate,
            "violations": report.violations,
        },
        "per_query_impact": {
            "improved": report.impact.improved,
            "degraded": report.impact.degraded,
            "worst_delta": report.impact.worst_delta,
        },
        "verdict": verdict,
    })
}
