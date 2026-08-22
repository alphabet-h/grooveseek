//! `groove tune` — fusion パラメータ (RRF k / FTS5 bm25 列重み) の測定ツール。
//!
//! golden query セットに対して grid search を実行し、その KB における
//! fusion パラメータの効き方を **統計的ガード付きで** レポートする。
//! **何も自動では適用しない** — 出力は toml に貼れる推奨スニペットと、
//! 「default 維持を推奨」という結論のどちらかである。
//!
//! 設計の要点 (spec feature-47 D-8〜D-11):
//! - fusion パラメータが効くのは **FTS が 2 件以上の候補を返す query だけ**である。
//!   効く query 数を「実効 N」として先に測り、0 なら掃引せず exit 2 で終わる。
//!   v0.15.x までは `sanitize_fts_query` がクエリ全体を単一 phrase 化していたため
//!   これは「query が逐語で出現する場合だけ」を意味したが、feature-48 (v0.16.0) で
//!   `db::fts_query::build_fts_query` が文字種 token の OR 式にコンパイルするように
//!   なったので、自然文 query でも実効になり得る
//! - grid は「query embedding 一括 → vec 候補 query あたり 1 回 → FTS 候補
//!   bm25 条件ごと 1 回 → rrf_k はメモリ内」の 4 層に因数分解される
//! - 小 golden set の argmax は overfit するので、nested leave-one-query-out
//!   CV + paired SE + selection stability + 副指標の非悪化で採否を判定する
//!   (sign test も算出して report に載せるが、`decide` は参照しない)

use anyhow::{Context, Result};
use std::collections::HashMap;
use std::path::PathBuf;

use crate::db::{Database, FusionParams, SearchFilters};
use crate::embedder::{Embedder, ModelChoice};
use crate::eval::{ExpectedHit, GoldenSet, HitRecord};

// AU-31: `tune.rs` was 3073 lines. Three groups came out — the parameter
// space, the statistics, and the rendering — leaving the orchestration here.
mod grid;
mod report;
mod stats;
pub use grid::*;
pub use report::*;
pub use stats::*;

// ---------------------------------------------------------------------------
// Pre-flight (D-8)
// ---------------------------------------------------------------------------

/// metric に寄与する golden query (= `expected` が非空) を golden 記載順で返す。
///
/// `eval::aggregate_metrics` が expected 空の query を平均から外すのと同じ
/// 扱いを、tune では入口で 1 回だけ適用する。`preflight_from_embeddings` に
/// 渡す embedding の順序・件数はこの関数の結果と一致していなければならない。
pub fn usable_queries(golden: &GoldenSet) -> Vec<&crate::eval::GoldenQuery> {
    for q in &golden.queries {
        if q.expected.is_empty() {
            tracing::warn!(query = %q.query, "skipping golden query with no expected hits");
        }
    }
    usable_queries_quiet(golden)
}

/// [`usable_queries`] と同じ絞り込みを警告なしで行う。
///
/// `run` は embedding を作るために `usable_queries` を先に呼び、その直後に
/// `preflight_from_embeddings` が同じ絞り込みを再実行する。警告を両方で出すと
/// 同じ query について 2 行出るため、内部再実行はこちらを使う。
fn usable_queries_quiet(golden: &GoldenSet) -> Vec<&crate::eval::GoldenQuery> {
    golden
        .queries
        .iter()
        .filter(|q| !q.expected.is_empty())
        .collect()
}

/// 掃引前に全 query の FTS 候補数を測り、実効 N と診断値を確定する。
///
/// ここで vec 候補リストも確定させる (fusion パラメータに不依存なので
/// grid 全体で使い回す、D-10-2)。
///
/// `embeddings` は [`usable_queries`] と **同順・同数**であること。
/// `Embedder` をここで持たないのは、実モデル DL 無しに pre-flight 本体を
/// テストできるようにするため (embedding の一括計算は `run` の責務)。
pub fn preflight_from_embeddings(
    db: &Database,
    golden: &GoldenSet,
    embeddings: &[Vec<f32>],
    limit: u32,
    meta: &mut HashMap<i64, HitMeta>,
) -> Result<Preflight> {
    let pool_size = limit.saturating_mul(5).max(50);
    let chunk_total = db.chunk_count()?;
    let filters = SearchFilters::default();
    let default_params = FusionParams::default();

    let usable = usable_queries_quiet(golden);
    if usable.len() != embeddings.len() {
        anyhow::bail!(
            "embedding count mismatch: {} usable golden queries but {} embeddings supplied",
            usable.len(),
            embeddings.len()
        );
    }

    let mut queries = Vec::with_capacity(usable.len());
    for (q, emb) in usable.iter().zip(embeddings.iter()) {
        let (vec_hits, fts_hits) =
            db.search_split_candidates(&q.query, emb, pool_size, &filters, default_params)?;

        for (id, sr) in vec_hits.iter().chain(fts_hits.iter()) {
            meta.entry(*id).or_insert_with(|| HitMeta {
                path: sr.path.clone(),
                heading: sr.heading.clone(),
            });
        }

        let vec_ids: Vec<i64> = vec_hits.iter().map(|(id, _)| *id).collect();
        let fts_ids: Vec<i64> = fts_hits.iter().map(|(id, _)| *id).collect();
        let vec_set: std::collections::HashSet<i64> = vec_ids.iter().copied().collect();
        let overlap = fts_ids.iter().filter(|id| vec_set.contains(id)).count();

        let total_matches = db.count_fts_matches(&q.query)?;
        let diag = QueryDiagnostics {
            fts_candidates: fts_ids.len(),
            fts_total_matches: total_matches,
            vec_fts_overlap: overlap,
            // bm25_sensitive / rrf_ties は grid 掃引中に埋める。
            bm25_sensitive: false,
            idf_clamped: chunk_total > 0 && total_matches * 2 >= i64::from(chunk_total),
            rrf_ties: 0,
        };

        queries.push(PreparedQuery {
            id: q
                .id
                .clone()
                .unwrap_or_else(|| q.query.chars().take(32).collect()),
            query: q.query.clone(),
            expected: q.expected.clone(),
            vec_ids,
            diag,
        });
    }

    let effective: Vec<usize> = queries
        .iter()
        .enumerate()
        .filter(|(_, q)| q.diag.is_effective())
        .map(|(i, _)| i)
        .collect();

    Ok(Preflight {
        queries,
        effective,
        chunk_total,
        pool_size,
    })
}

/// ランク付き `(chunk_id, score)` を eval の `HitRecord` 列へ変換する。
/// rank は 1-origin (eval の `reciprocal_rank` が 1-origin を前提とする)。
pub fn to_hit_records(ranked: &[(i64, f32)], meta: &HashMap<i64, HitMeta>) -> Vec<HitRecord> {
    ranked
        .iter()
        .enumerate()
        .filter_map(|(i, (id, score))| {
            meta.get(id).map(|m| HitRecord {
                rank: i + 1,
                path: m.path.clone(),
                heading: m.heading.clone(),
                score: *score,
            })
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Metric table (D-10)
// ---------------------------------------------------------------------------

/// `rows[query_index][condition_index]` の per-query metric キャッシュ。
///
/// nested LOO は fold ごとに coordinate descent の勝者が変わり得るため、
/// Phase K の全起点を含む **384 条件分** を先に埋めておく (D-10 の注)。
/// D-9 の「全直積は冗長」は *探索・報告する条件数* の話であり、CV の
/// キャッシュ量とは別問題。SQL 往復は 64 条件 × N query のまま増えない。
pub struct MetricTable {
    rows: Vec<Vec<crate::eval::QueryMetrics>>,
    k_values: Vec<usize>,
}

impl MetricTable {
    pub fn from_rows(rows: Vec<Vec<crate::eval::QueryMetrics>>, k_values: Vec<usize>) -> Self {
        Self { rows, k_values }
    }

    pub fn query_count(&self) -> usize {
        self.rows.len()
    }

    /// Test-only: the whole metric record behind one cell.
    ///
    /// [`Self::primary`] exposes nDCG@`PRIMARY_K` alone, but `recall_at_k` and
    /// `reciprocal_rank` travel on to [`Self::aggregate_for`] and from there to
    /// `non_degradation`, which decides adoption. An equivalence check reading
    /// only `primary` passes on a divergence confined to deeper ranks —
    /// measured, not assumed (codex P2 round 1 on PR #114).
    #[cfg(test)]
    pub(crate) fn cell(&self, c: Condition, q: usize) -> &crate::eval::QueryMetrics {
        &self.rows[q][c.index()]
    }

    pub fn k_values(&self) -> &[usize] {
        &self.k_values
    }

    /// 主指標 (nDCG@`PRIMARY_K`) の値。
    pub fn primary(&self, c: Condition, q: usize) -> f64 {
        self.rows[q][c.index()]
            .ndcg_at_k
            .get(&PRIMARY_K)
            .copied()
            .unwrap_or(0.0)
    }

    /// 指定 query 集合における主指標の平均。空集合は 0.0。
    pub fn mean_primary(&self, c: Condition, query_idx: &[usize]) -> f64 {
        if query_idx.is_empty() {
            return 0.0;
        }
        query_idx.iter().map(|&q| self.primary(c, q)).sum::<f64>() / query_idx.len() as f64
    }

    /// 指定 query 集合における全指標の集計 (非悪化判定 D-11-3 用)。
    /// `eval::aggregate_metrics` と同じ「単純平均」の定義。
    pub fn aggregate_for(
        &self,
        c: Condition,
        query_idx: &[usize],
    ) -> crate::eval::AggregateMetrics {
        let n = query_idx.len();
        if n == 0 {
            return crate::eval::AggregateMetrics::default();
        }
        let mut recall = std::collections::BTreeMap::new();
        let mut ndcg = std::collections::BTreeMap::new();
        for &k in &self.k_values {
            let sr: f64 = query_idx
                .iter()
                .map(|&q| {
                    self.rows[q][c.index()]
                        .recall_at_k
                        .get(&k)
                        .copied()
                        .unwrap_or(0.0)
                })
                .sum();
            let sn: f64 = query_idx
                .iter()
                .map(|&q| {
                    self.rows[q][c.index()]
                        .ndcg_at_k
                        .get(&k)
                        .copied()
                        .unwrap_or(0.0)
                })
                .sum();
            recall.insert(k, sr / n as f64);
            ndcg.insert(k, sn / n as f64);
        }
        let mrr: f64 = query_idx
            .iter()
            .map(|&q| self.rows[q][c.index()].reciprocal_rank)
            .sum::<f64>()
            / n as f64;
        crate::eval::AggregateMetrics {
            recall_at_k: recall,
            mrr,
            ndcg_at_k: ndcg,
            query_count: n,
        }
    }
}

/// grid を掃いて metric table を埋める (D-10)。
///
/// 1. bm25 重み 1 条件につき query あたり SQL 1 往復 (重みは FTS の LIMIT
///    通過集合を変えるので再実行が必要)
/// 2. その FTS rank list に対し 6 通りの `rrf_k` を **メモリ内で** 再適用
///    (`fuse_rrf_ids` は id と rank しか見ない)
///
/// `SearchResult` は `meta` へ path/heading を吸い出した直後に捨てる。
/// 384 条件 × N query × pool 件の content を保持すると数百 MB になるため。
pub fn build_metric_table(
    db: &Database,
    pre: &mut Preflight,
    meta: &mut HashMap<i64, HitMeta>,
    k_values: &[usize],
    limit: u32,
) -> Result<MetricTable> {
    let filters = SearchFilters::default();
    let pool = pre.pool_size;
    let default_weight_index = Condition::builtin_default().weight_index();
    // bm25 感度診断 (D-11-5) 用の grid 端 2 条件: heading 偏重 vs content 偏重。
    let heading_heavy = Condition {
        h: BM25_WEIGHT_GRID.len() - 1,
        ctx: 0,
        content: 0,
        k: 0,
    }
    .weight_index();
    let content_heavy = Condition {
        h: 0,
        ctx: 0,
        content: BM25_WEIGHT_GRID.len() - 1,
        k: 0,
    }
    .weight_index();

    let mut rows: Vec<Vec<crate::eval::QueryMetrics>> = Vec::with_capacity(pre.queries.len());

    // 診断値 (bm25_sensitive / rrf_ties) をループ内で `pre.queries[qi]` へ書き戻す
    // ため、`pre.queries` を borrow したまま回さず index で回す。ループ先頭で
    // 必要な入力だけ取り出しておく (`vec_ids` / `expected` は掃引中ずっと使う)。
    let n_queries = pre.queries.len();
    for qi in 0..n_queries {
        let (qid, query_text, vec_ids, expected) = {
            let pq = &pre.queries[qi];
            (
                pq.id.clone(),
                pq.query.clone(),
                pq.vec_ids.clone(),
                pq.expected.clone(),
            )
        };
        eprintln!(
            "  [{}/{}] sweeping {} weight conditions for {}",
            qi + 1,
            n_queries,
            WEIGHT_CONDITIONS,
            qid
        );
        let mut row: Vec<crate::eval::QueryMetrics> =
            vec![crate::eval::QueryMetrics::default(); TOTAL_CONDITIONS];
        let mut extreme_lists: HashMap<usize, Vec<i64>> = HashMap::new();
        let mut ties_at_default = 0usize;

        for c in Condition::all() {
            // 同じ重み組の 6 条件は FTS 結果を共有する。k==0 のときだけ引く。
            if c.k != 0 {
                // k != 0 の条件は下のブロックで一括処理済み
                continue;
            }
            let params = c.to_params();
            // rrf_k は search_fts_candidates では未使用 (bm25 重みだけが SQL に載る)
            let fts_hits = db.search_fts_candidates(&query_text, pool, &filters, params)?;
            let mut fts_ids = Vec::with_capacity(fts_hits.len());
            for (id, sr) in &fts_hits {
                meta.entry(*id).or_insert_with(|| HitMeta {
                    path: sr.path.clone(),
                    heading: sr.heading.clone(),
                });
                fts_ids.push(*id);
            }
            drop(fts_hits); // content 文字列をここで解放する

            let wi = c.weight_index();
            if wi == heading_heavy || wi == content_heavy {
                extreme_lists.insert(wi, fts_ids.clone());
            }

            for (k, &rrf_k) in RRF_K_GRID.iter().enumerate() {
                let cond = Condition { k, ..c };
                let ranked = crate::db::fuse_rrf_ids(&vec_ids, &fts_ids, rrf_k, Some(limit));
                if wi == default_weight_index && k == Condition::builtin_default().k {
                    ties_at_default = ranked.windows(2).filter(|w| w[0].1 == w[1].1).count();
                }
                let top = to_hit_records(&ranked, meta);
                row[cond.index()] = crate::eval::compute_query_metrics(&expected, &top, k_values);
            }
        }
        rows.push(row);

        // 診断値の後埋め
        let sensitive = match (
            extreme_lists.get(&heading_heavy),
            extreme_lists.get(&content_heavy),
        ) {
            (Some(a), Some(b)) => a != b,
            _ => false,
        };
        pre.queries[qi].diag.bm25_sensitive = sensitive;
        pre.queries[qi].diag.rrf_ties = ties_at_default;
    }

    Ok(MetricTable::from_rows(rows, k_values.to_vec()))
}

// ---------------------------------------------------------------------------
// Selection (D-9 / D-11)
// ---------------------------------------------------------------------------

/// プラトー (同点) 判定の許容幅。nDCG は `[0, 1]` なので 1e-9 は
/// 「浮動小数の丸め誤差の範囲で同点」を意味する。
const PLATEAU_EPS: f64 = 1e-9;

/// グリッド中央からの距離 (各軸の添字距離の総和)。
///
/// D-11-7 後半: **プラトーでは端ではなく中央の値を推奨する**。端の値は
/// golden の偶然に張り付いた結果であることが多く、同じスコアなら中央の方が
/// 汎化しやすい (presearch R2)。
fn distance_from_grid_center(c: Condition) -> f64 {
    let wc = (BM25_WEIGHT_GRID.len() - 1) as f64 / 2.0; // 4 要素 -> 1.5
    let kc = (RRF_K_GRID.len() - 1) as f64 / 2.0; // 6 要素 -> 2.5
    (c.h as f64 - wc).abs()
        + (c.ctx as f64 - wc).abs()
        + (c.content as f64 - wc).abs()
        + (c.k as f64 - kc).abs()
}

/// 同点集合から 1 条件を選ぶ。優先順位:
///
/// 1. **既定条件が同点で含まれていれば既定条件** (D-11-2 の default タイブレーク)
/// 2. さもなくばグリッド中央に最も近い条件 (D-11-7)
/// 3. なお同点ならグリッド順で最小の条件 (決定性の担保)
fn pick_from_plateau(plateau: &[Condition], base: Condition) -> Condition {
    if plateau.contains(&base) {
        return base;
    }
    let mut best = plateau[0];
    let mut best_d = distance_from_grid_center(best);
    for &c in &plateau[1..] {
        let d = distance_from_grid_center(c);
        if d < best_d - 1e-12 || ((d - best_d).abs() <= 1e-12 && c.index() < best.index()) {
            best = c;
            best_d = d;
        }
    }
    best
}

/// coordinate descent (D-9) を metric table 上で実行する。
///
/// - Phase W: `rrf_k` を既定 (60) に固定して 64 通りの重み組から最良を選ぶ
/// - Phase K: 勝った重み組を固定して 6 通りの `rrf_k` から最良を選ぶ
///
/// 全直積 (384) を argmax しないのは overfit + 冗長だからであり、
/// この手続き自体が「選択手続き」として nested LOO の評価対象になる。
///
/// 各 phase では最大値そのものではなく **同点集合 (プラトー)** を集め、
/// [`pick_from_plateau`] で「default 優先 → 中央優先 → グリッド順」の順に
/// 決める。初期値を既定条件に置いてあるので、完全に平坦な landscape では
/// 必ず既定条件が返る。
pub fn select_condition(table: &MetricTable, query_idx: &[usize]) -> Condition {
    let base = Condition::builtin_default();
    let mut best_score = table.mean_primary(base, query_idx);
    let mut plateau = vec![base];

    // Phase W
    for h in 0..BM25_WEIGHT_GRID.len() {
        for ctx in 0..BM25_WEIGHT_GRID.len() {
            for content in 0..BM25_WEIGHT_GRID.len() {
                let c = Condition {
                    h,
                    ctx,
                    content,
                    k: base.k,
                };
                let s = table.mean_primary(c, query_idx);
                if s > best_score + PLATEAU_EPS {
                    best_score = s;
                    plateau.clear();
                    plateau.push(c);
                } else if (s - best_score).abs() <= PLATEAU_EPS && !plateau.contains(&c) {
                    plateau.push(c);
                }
            }
        }
    }
    let best_w = pick_from_plateau(&plateau, base);

    // Phase K
    plateau.clear();
    plateau.push(best_w);
    for k in 0..RRF_K_GRID.len() {
        let c = Condition { k, ..best_w };
        let s = table.mean_primary(c, query_idx);
        if s > best_score + PLATEAU_EPS {
            best_score = s;
            plateau.clear();
            plateau.push(c);
        } else if (s - best_score).abs() <= PLATEAU_EPS && !plateau.contains(&c) {
            plateau.push(c);
        }
    }
    pick_from_plateau(&plateau, base)
}

pub struct LooOutcome {
    /// 全 N query での argmax (refit)。**これが最終推奨値**。
    /// nested LOO が出すのは「選択手続きの性能推定」であって単一の
    /// パラメータではない (D-11-2)。
    pub refit: Condition,
    /// fold j の held-out query における default 比の per-query 差分。
    pub diffs: Vec<f64>,
    /// fold ごとの選択結果。
    pub fold_selections: Vec<Condition>,
    /// refit と同じ条件を選んだ fold の割合 (selection stability)。
    pub stability: f64,
}

/// nested leave-one-query-out CV (D-11-1)。
///
/// fold j (query j を除外) ごとに N−1 query で `select_condition` を回し、
/// **除外した query j で評価**して差分 d_j を得る。選択バイアス
/// (Cawley & Talbot) をこの構成で吸収する。全 N argmax を fold で使い回す
/// 実装は選択バイアスの再導入なので禁止。
pub fn nested_loo(table: &MetricTable, effective: &[usize]) -> LooOutcome {
    let refit = select_condition(table, effective);
    let base = Condition::builtin_default();
    let mut diffs = Vec::with_capacity(effective.len());
    let mut fold_selections = Vec::with_capacity(effective.len());

    for (j, &held_out) in effective.iter().enumerate() {
        let train: Vec<usize> = effective
            .iter()
            .enumerate()
            .filter(|(i, _)| *i != j)
            .map(|(_, &q)| q)
            .collect();
        let sel = select_condition(table, &train);
        diffs.push(table.primary(sel, held_out) - table.primary(base, held_out));
        fold_selections.push(sel);
    }

    let stability = if fold_selections.is_empty() {
        0.0
    } else {
        fold_selections.iter().filter(|c| **c == refit).count() as f64
            / fold_selections.len() as f64
    };

    LooOutcome {
        refit,
        diffs,
        fold_selections,
        stability,
    }
}

/// 全指標の非悪化条件 (D-11-3)。主指標は nDCG@5 だが、recall@k / MRR が
/// baseline より悪化している候補は閾値を満たしても推奨しない。
///
/// 判定は **golden 全 query の集計** で行う (実効 query に限定しない)。
/// production で効くのは全体の集計値だからである。
pub fn non_degradation(table: &MetricTable, cand: Condition, all: &[usize]) -> (bool, Vec<String>) {
    let base = Condition::builtin_default();
    let a_cand = table.aggregate_for(cand, all);
    let a_base = table.aggregate_for(base, all);
    let mut violations = Vec::new();
    for &k in table.k_values() {
        let c = a_cand.recall_at_k.get(&k).copied().unwrap_or(0.0);
        let b = a_base.recall_at_k.get(&k).copied().unwrap_or(0.0);
        if c < b {
            violations.push(format!("recall@{k} {c:.4} < baseline {b:.4}"));
        }
    }
    if a_cand.mrr < a_base.mrr {
        violations.push(format!(
            "MRR {:.4} < baseline {:.4}",
            a_cand.mrr, a_base.mrr
        ));
    }
    (violations.is_empty(), violations)
}

/// per-query 内訳 (D-11-4)。rank fusion は平均改善が per-query 劣化を
/// 隠すため (Benham & Culpepper)、悪化 query 数と最大悪化幅を必ず出す。
#[derive(Debug, Clone, PartialEq)]
pub struct PerQueryImpact {
    pub improved: usize,
    pub degraded: usize,
    /// 最も悪化した query の差分 (負値)。悪化なしなら 0.0。
    pub worst_delta: f64,
    /// 最も悪化した query の添字。
    pub worst_query: Option<usize>,
}

pub fn per_query_impact(table: &MetricTable, cand: Condition, all: &[usize]) -> PerQueryImpact {
    const EPS: f64 = 1e-12;
    let base = Condition::builtin_default();
    let mut improved = 0;
    let mut degraded = 0;
    let mut worst_delta = 0.0_f64;
    let mut worst_query = None;
    for &q in all {
        let d = table.primary(cand, q) - table.primary(base, q);
        if d > EPS {
            improved += 1;
        } else if d < -EPS {
            degraded += 1;
            if d < worst_delta {
                worst_delta = d;
                worst_query = Some(q);
            }
        }
    }
    PerQueryImpact {
        improved,
        degraded,
        worst_delta,
        worst_query,
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Verdict {
    /// 全条件を満たしたので推奨する。
    Adopt(Condition),
    /// default 維持が結論。`reasons` に落ちた条件を列挙する。
    KeepDefault { reasons: Vec<String> },
}

/// 採否判定 (D-11-2 / D-11-3)。以下を **すべて** 満たしたときだけ採用:
///
/// 1. refit が既定条件と異なる
/// 2. `mean(d) > ADOPT_MIN_MEAN_DELTA`
/// 3. `mean(d) > ADOPT_SE_MULTIPLIER * paired_se(d)`
/// 4. selection stability > `STABILITY_MIN` (過半数の fold が refit に一致)
/// 5. 全指標 (recall@k 各 k / MRR) が baseline を下回らない
pub fn decide(table: &MetricTable, outcome: &LooOutcome, all: &[usize]) -> Verdict {
    decide_with(
        table,
        outcome,
        all,
        ADOPT_SE_MULTIPLIER,
        ADOPT_MIN_MEAN_DELTA,
    )
}

/// `decide` の 2 つの数値閾値だけを差し替えられるようにしたもの。
///
/// 存在理由は AU-68 の測定にある: 「係数をいくつにすると誤採用率が何 % に
/// なるか」は、`decide` と**同じ判定順序・同じ比較の向き**で測らないと意味が
/// ないため、テスト側に判定を書き写すのではなくここを共有する。`decide` は
/// 既定値を渡すだけの薄い wrapper なので、両者が食い違うことはない。
pub fn decide_with(
    table: &MetricTable,
    outcome: &LooOutcome,
    all: &[usize],
    se_multiplier: f64,
    min_mean_delta: f64,
) -> Verdict {
    let mut reasons = Vec::new();
    let m = mean(&outcome.diffs);
    let se = paired_se(&outcome.diffs);

    if outcome.refit == Condition::builtin_default() {
        reasons.push("refit selected the built-in defaults".to_string());
    }
    if m <= min_mean_delta {
        reasons.push(format!(
            "held-out mean delta {m:+.4} <= threshold {min_mean_delta:.4}"
        ));
    }
    // `m > mult*se` の否定を `m <= mult*se` と書いてはならない: se は N<2 で
    // INFINITY、m は退化入力で NaN になり得るため、比較不能なケースを
    // 「採用しない」側 (= reason を積む) へ倒す必要がある。
    let scaled_se = se_multiplier * se;
    if !matches!(m.partial_cmp(&scaled_se), Some(std::cmp::Ordering::Greater)) {
        reasons.push(format!(
            "held-out mean delta {m:+.4} <= {se_multiplier:.1} x paired SE {scaled_se:.4}"
        ));
    }
    if outcome.stability <= STABILITY_MIN {
        reasons.push(format!(
            "selection stability {:.2} <= {STABILITY_MIN:.2} (folds disagree = overfit signal)",
            outcome.stability
        ));
    }
    let (ok, violations) = non_degradation(table, outcome.refit, all);
    if !ok {
        reasons.push(format!(
            "secondary metrics degraded: {}",
            violations.join("; ")
        ));
    }

    if reasons.is_empty() {
        Verdict::Adopt(outcome.refit)
    } else {
        Verdict::KeepDefault { reasons }
    }
}

// ---------------------------------------------------------------------------
// Report / orchestration (D-8)
// ---------------------------------------------------------------------------

/// 実効 query が 1 件も無いときの exit code。
pub const EXIT_NO_EFFECTIVE_QUERIES: i32 = 2;

/// この KB で `bm25_context_weight` 軸が測定不能 (no-op) かどうか。
///
/// `[contextual]` を無効のまま index した KB では `chunks.context_text` が
/// 全件空になり、context 列の重みを 0.5 から 4.0 まで振っても bm25 スコアは
/// 一切動かない。掃引結果の「context 重みは効かなかった」は、その場合
/// **「効かない」ではなく「測っていない」** なので明示的に警告する。
pub fn context_axis_is_noop(db: &Database) -> Result<bool> {
    Ok(db.count_chunks_with_context()? == 0)
}

/// 報告する k のリストを正規化する。
///
/// 主指標 nDCG@[`PRIMARY_K`] を必ず含め、昇順・重複なしにする。採用閾値
/// `ADOPT_MIN_MEAN_DELTA` は nDCG@5 を前提に較正されているため、`--k 1,10`
/// のように 5 を外した指定でも主指標が欠落しないようにする。
pub fn normalize_k_values(k_values: &[usize]) -> Vec<usize> {
    let mut out: Vec<usize> = k_values.iter().copied().filter(|k| *k > 0).collect();
    if !out.contains(&PRIMARY_K) {
        out.push(PRIMARY_K);
    }
    out.sort_unstable();
    out.dedup();
    out
}

/// tune が受け付ける k / limit の上限。db 層の overfetch 上限
/// (`FILTER_OVERFETCH_CAP` = 10,000) と同値。これを超える値は wrap (as cast、
/// codex P2 round 3 on PR #79) や saturate (u32::MAX が下流の
/// `Vec::with_capacity` に流れて allocation abort、同 round 4) のどちらの
/// 変換でも壊れるため、変換せず reject する。
pub const MAX_TUNE_K: usize = 10_000;

/// `run` が実際に使う取得件数。limit が max(k) 未満だと fused ranking が
/// limit 件に切り詰められ、recall@k / nDCG@k がラベルより浅い候補から計算
/// されてしまうため、正規化後の k リスト最大値を下限として clamp する。
/// CLI resolver (`main.rs`) と同じ不変条件を public API 側でも保証する
/// (codex P2 round 2 on PR #79)。`MAX_TUNE_K` 超の k / limit は bail する。
pub fn effective_limit(k_values: &[usize], limit: u32) -> Result<u32> {
    let max_k = *k_values.iter().max().unwrap_or(&PRIMARY_K);
    if max_k > MAX_TUNE_K {
        anyhow::bail!("k value {max_k} exceeds the supported maximum {MAX_TUNE_K}");
    }
    if limit as usize > MAX_TUNE_K {
        anyhow::bail!("--limit {limit} exceeds the supported maximum {MAX_TUNE_K}");
    }
    Ok(limit.max(max_k as u32))
}

pub struct TuneReport {
    pub kb_path: PathBuf,
    pub golden_path: PathBuf,
    pub model: String,
    pub limit: u32,
    /// E-8: 実際に使った候補プール。`limit*5 max 50` の **floor** であって
    /// cap ではない。k との交互作用があるので出力ヘッダに必ず明記する。
    pub pool_size: u32,
    pub k_values: Vec<usize>,
    pub chunk_total: u32,
    /// `chunks.context_text` が全件空 = context 軸が測定不能。
    /// [`context_axis_is_noop`] 参照。
    pub context_axis_noop: bool,
    /// golden の有効 query 総数 (expected 空を除いた数)。
    pub query_count: usize,
    /// 実効 query (FTS 候補 >= 2) の添字。
    pub effective: Vec<usize>,
    /// `(query id, 診断値)` を golden 順に並べたもの。
    pub diagnostics: Vec<(String, QueryDiagnostics)>,
    /// 既定条件での全 query 集計。
    pub baseline: crate::eval::AggregateMetrics,
    /// refit 条件での全 query 集計。
    pub refit_aggregate: crate::eval::AggregateMetrics,
    /// Phase W の上位 5 件 `(条件, 実効 query 平均 nDCG@5)`。
    pub top_weight_conditions: Vec<(Condition, f64)>,
    /// Phase K の 6 件 `(条件, 実効 query 平均 nDCG@5)`。
    pub top_k_conditions: Vec<(Condition, f64)>,
    pub outcome: LooOutcome,
    pub sign: SignTest,
    pub impact: PerQueryImpact,
    pub violations: Vec<String>,
    pub verdict: Verdict,
}

/// `run` の結果。実効 query 0 件は **エラーではない早期終了** なので
/// `Err` ではなくこの enum の variant で表す (呼び出し側の main.rs が
/// `EXIT_NO_EFFECTIVE_QUERIES` で終了する)。
pub enum TuneOutcome {
    /// 実効 query が 0 件。grid は実行していない。
    NoEffectiveQueries {
        query_count: usize,
        diagnostics: Vec<(String, QueryDiagnostics)>,
    },
    Report(Box<TuneReport>),
}

/// golden を読み、pre-flight → grid 掃引 → 統計判定まで通す
/// (`eval::run` と対になる orchestration 入口)。
pub fn run(opts: &TuneOpts) -> Result<TuneOutcome> {
    let golden = GoldenSet::load(&opts.golden_path)?;

    let db_path = crate::resolve_db_path(&opts.kb_path);
    if !db_path.exists() {
        anyhow::bail!(
            "No index found at {}. Run `groove index --kb-path {}` first.",
            db_path.display(),
            opts.kb_path.display()
        );
    }
    let db = Database::open(&db_path.to_string_lossy())?;
    db.verify_embedding_meta(
        opts.model_choice.model_id(),
        opts.model_choice.dimension() as u32,
    )?;
    let mut embedder = Embedder::with_model(opts.model_choice)?;

    // 主指標 nDCG@PRIMARY_K は必ず計算対象に含める。`run` は pub なので
    // CLI 以外の呼び出しでもこの不変条件をここで保証する (CLI 側は limit の
    // 既定値導出のために同じ正規化を先出しで呼ぶ)。
    let k_values = normalize_k_values(&opts.k_values);
    // limit も同じ理由で public API 側で clamp する (codex P2 round 2 on PR #79):
    // limit < max(k) だと fused ranking が limit 件に切り詰められ、recall@k /
    // nDCG@k がラベルより浅い候補から計算されてしまう。
    let limit = effective_limit(&k_values, opts.limit)?;

    // query embedding をループ外で 1 回だけ (D-10-1)。現行 eval は query ごとに
    // `embed_single` を呼んでおりキャッシュも無いので、ここが tune の主な
    // 高速化ポイントになる。
    let embeddings = {
        let usable = usable_queries(&golden);
        let texts: Vec<&str> = usable.iter().map(|q| q.query.as_str()).collect();
        embedder
            .embed_texts(&texts)
            .context("failed to embed golden queries")?
    };

    let mut meta: HashMap<i64, HitMeta> = HashMap::new();
    eprintln!("groove tune: pre-flight (measuring FTS candidates per query)...");
    let mut pre = preflight_from_embeddings(&db, &golden, &embeddings, limit, &mut meta)?;

    let diagnostics: Vec<(String, QueryDiagnostics)> = pre
        .queries
        .iter()
        .map(|q| (q.id.clone(), q.diag.clone()))
        .collect();

    if pre.effective.is_empty() {
        return Ok(TuneOutcome::NoEffectiveQueries {
            query_count: pre.queries.len(),
            diagnostics,
        });
    }

    let context_axis_noop = context_axis_is_noop(&db)?;
    if context_axis_noop {
        eprintln!(
            "groove tune: WARNING: every chunk has an empty context column, so the \
             bm25_context_weight axis is a no-op on this KB (contextual retrieval is off). \
             Its rows below mean \"not measured\", not \"has no effect\"."
        );
    }

    eprintln!(
        "groove tune: {} of {} golden queries are effective (FTS candidates >= 2)",
        pre.effective.len(),
        pre.queries.len()
    );
    for (id, d) in &diagnostics {
        if !d.is_effective() {
            eprintln!(
                "  insensitive: {id} (FTS candidates = {}; fusion parameters cannot move it)",
                d.fts_candidates
            );
        }
    }
    if pre.effective.len() < SMALL_N_WARN {
        eprintln!(
            "groove tune: WARNING: effective N = {} is below the IR convention of {SMALL_N_WARN} \
             topics. Treat the numbers below as suggestive, not conclusive.",
            pre.effective.len()
        );
    }

    eprintln!(
        "groove tune: sweeping {WEIGHT_CONDITIONS} bm25 weight conditions x {} rrf_k values...",
        RRF_K_GRID.len()
    );
    let table = build_metric_table(&db, &mut pre, &mut meta, &k_values, limit)?;

    // 診断値は build_metric_table で後埋めされるので取り直す。
    let diagnostics: Vec<(String, QueryDiagnostics)> = pre
        .queries
        .iter()
        .map(|q| (q.id.clone(), q.diag.clone()))
        .collect();

    let all: Vec<usize> = (0..pre.queries.len()).collect();
    let outcome = nested_loo(&table, &pre.effective);
    let sign = sign_test(&outcome.diffs);
    let impact = per_query_impact(&table, outcome.refit, &all);
    let (_, violations) = non_degradation(&table, outcome.refit, &all);
    let verdict = decide(&table, &outcome, &all);

    // Phase W / Phase K の上位を報告用に取り出す (実効 query 平均で評価)。
    let base = Condition::builtin_default();
    let mut weights: Vec<(Condition, f64)> = (0..BM25_WEIGHT_GRID.len())
        .flat_map(|h| {
            (0..BM25_WEIGHT_GRID.len()).flat_map(move |ctx| {
                (0..BM25_WEIGHT_GRID.len()).map(move |content| Condition {
                    h,
                    ctx,
                    content,
                    k: base.k,
                })
            })
        })
        .map(|c| (c, table.mean_primary(c, &pre.effective)))
        .collect();
    weights.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    weights.truncate(5);

    let top_k_conditions: Vec<(Condition, f64)> = (0..RRF_K_GRID.len())
        .map(|k| {
            let c = Condition { k, ..outcome.refit };
            (c, table.mean_primary(c, &pre.effective))
        })
        .collect();

    Ok(TuneOutcome::Report(Box::new(TuneReport {
        kb_path: opts.kb_path.clone(),
        golden_path: opts.golden_path.clone(),
        model: opts.model_choice.model_id().to_string(),
        limit,
        pool_size: pre.pool_size,
        k_values,
        chunk_total: pre.chunk_total,
        context_axis_noop,
        query_count: pre.queries.len(),
        effective: pre.effective.clone(),
        diagnostics,
        baseline: table.aggregate_for(base, &all),
        refit_aggregate: table.aggregate_for(outcome.refit, &all),
        top_weight_conditions: weights,
        top_k_conditions,
        outcome,
        sign,
        impact,
        violations,
        verdict,
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_grid_shape_matches_spec() {
        // D-9: rrf_k 6 通り × bm25 重み 4 通り^3 = 384 条件。
        assert_eq!(RRF_K_GRID, [5.0, 10.0, 20.0, 30.0, 60.0, 100.0]);
        assert_eq!(BM25_WEIGHT_GRID, [0.5, 1.0, 2.0, 4.0]);
        assert_eq!(TOTAL_CONDITIONS, 384);
    }

    #[test]
    fn test_condition_index_is_a_bijection() {
        let mut seen = vec![false; TOTAL_CONDITIONS];
        for c in Condition::all() {
            let i = c.index();
            assert!(i < TOTAL_CONDITIONS, "index out of range: {c:?} -> {i}");
            assert!(!seen[i], "duplicate index for {c:?}");
            seen[i] = true;
        }
        assert!(seen.iter().all(|b| *b), "condition index must cover 0..384");
    }

    #[test]
    fn test_builtin_default_condition_maps_to_default_params() {
        // 既定条件 (2.0 / 1.0 / 1.0, k=60) が FusionParams::default() と一致する
        // こと。この対応が崩れると「default 比の差分」がすべて無意味になる。
        let c = Condition::builtin_default();
        assert_eq!(c.to_params(), crate::db::FusionParams::default());
    }

    /// tune のテスト用 fixture DB (db.rs の `db_with_384` 相当。あちらは
    /// db.rs の `mod tests` 内 private なのでここからは使えない)。
    fn tune_db() -> crate::db::Database {
        let db = crate::db::Database::open_in_memory().expect("open_in_memory");
        db.verify_embedding_meta("bge-small-en-v1.5", 384)
            .expect("verify_embedding_meta");
        db
    }

    fn emb(v: f32) -> Vec<f32> {
        vec![v; 384]
    }

    /// `path` / `heading` / `content` の 1 doc 1 chunk を足す。
    fn add_doc(db: &crate::db::Database, path: &str, heading: &str, content: &str, e: f32) {
        let doc = db
            .upsert_document(path, Some(path), None, None, None, &[], None, path, 0)
            .unwrap();
        db.insert_chunk(doc, 0, Some(heading), None, content, None, &emb(e), 1.0)
            .unwrap();
    }

    fn golden_with(queries: Vec<(&str, &str, Vec<&str>)>) -> crate::eval::GoldenSet {
        crate::eval::GoldenSet {
            defaults: None,
            queries: queries
                .into_iter()
                .map(|(id, q, expected)| crate::eval::GoldenQuery {
                    id: Some(id.to_string()),
                    query: q.to_string(),
                    expected: expected
                        .into_iter()
                        .map(|p| crate::eval::ExpectedHit {
                            path: p.to_string(),
                            heading: None,
                        })
                        .collect(),
                    tags: None,
                })
                .collect(),
        }
    }

    // -----------------------------------------------------------------------
    // AU-22: guards for the sweep optimisation in `build_metric_table`.
    //
    // The sweep fills TOTAL_CONDITIONS (384) cells with WEIGHT_CONDITIONS (64)
    // SQL round trips per query, by reusing one FTS candidate list across the
    // six `rrf_k` values that share a weight triple. That is only sound while
    // `search_fts_candidates` ignores `rrf_k`. Nothing asserted any of it.
    // -----------------------------------------------------------------------

    /// A corpus on which the bm25 column weights demonstrably change the
    /// ranking, which is what makes the guards below able to fail.
    ///
    /// Measured while writing them: a three-document corpus is **not** enough.
    /// Every trigram then appears in nearly every row, IDF collapses to ~0, and
    /// bm25 returns ~1e-6 whatever the weights are — so a test built on it
    /// passes even when the weights are wired wrong. This shape does discriminate:
    ///
    /// | heading weight | first hit |
    /// |---|---|
    /// | 0.5 | the content-only document |
    /// | 4.0 | the heading-only document |
    fn weight_sensitive_docs(db: &crate::db::Database) {
        // term only in the heading
        add_doc(
            db,
            "h.md",
            "zebrafish",
            "unrelated filler prose about widgets",
            0.1,
        );
        // term only in the content
        add_doc(
            db,
            "c.md",
            "widgets",
            "a study of zebrafish in the laboratory",
            0.2,
        );
        // filler, so the term is rare enough to carry IDF
        for i in 0..14 {
            add_doc(
                db,
                &format!("f{i}.md"),
                "widgets",
                "assorted filler prose about widgets and gears",
                0.3,
            );
        }
    }

    /// The corpus above plus a hand-built `Preflight`, so the sweep can run
    /// without an embedder: `vec_ids` is supplied directly rather than
    /// retrieved, which is the only part of the real pre-flight that embeds.
    fn sweep_fixture() -> (crate::db::Database, Preflight) {
        let db = tune_db();
        weight_sensitive_docs(&db);

        let mk = |id: &str, q: &str, expected: Vec<&str>, vec_ids: Vec<i64>| PreparedQuery {
            id: id.to_string(),
            query: q.to_string(),
            expected: expected
                .into_iter()
                .map(|p| crate::eval::ExpectedHit {
                    path: p.to_string(),
                    heading: None,
                })
                .collect(),
            vec_ids,
            diag: QueryDiagnostics::default(),
        };
        // Overlapping vec lists so RRF actually mixes the two rankings; with no
        // overlap every score collapses to 1/(k+r+1) and the rrf_k axis becomes
        // unmeasurable (see QueryDiagnostics::vec_fts_overlap).
        let pre = Preflight {
            queries: vec![
                mk("q0", "zebrafish", vec!["h.md", "c.md"], vec![1, 2, 3]),
                // `f6` / `f7` sit at ranks 9-10 of this query's fused list
                // (measured), so a divergence past rank 5 actually moves a
                // metric instead of landing on documents nobody expects.
                mk(
                    "q1",
                    "widgets",
                    vec!["c.md", "f6.md", "f7.md"],
                    vec![3, 1, 2],
                ),
            ],
            effective: vec![0, 1],
            chunk_total: 16,
            pool_size: 10,
        };
        (db, pre)
    }

    /// The invariant the optimisation rests on, stated directly: the FTS
    /// candidate list must not depend on `rrf_k`. If it ever did, sharing one
    /// list across the six `rrf_k` conditions would silently produce a wrong
    /// sweep — and no existing test looked at it.
    #[test]
    fn test_fts_candidates_do_not_depend_on_rrf_k() {
        let db = tune_db();
        weight_sensitive_docs(&db);
        let filters = crate::db::SearchFilters::default();
        let base = Condition::builtin_default().to_params();
        let probe = |p: crate::db::FusionParams| {
            db.search_fts_candidates("zebrafish", 10, &filters, p)
                .unwrap()
                .iter()
                .map(|(id, sr)| (*id, sr.score))
                .collect::<Vec<_>>()
        };

        let reference = probe(base);
        assert!(!reference.is_empty(), "fixture must produce FTS candidates");

        // Sanity check on the fixture itself: this corpus *does* respond to the
        // weights, so "nothing changed" below is evidence rather than an
        // artifact of an insensitive corpus. Without this the whole test passes
        // on a corpus where bm25 returns ~0 regardless.
        let heavier = probe(crate::db::FusionParams {
            bm25_heading_weight: 4.0,
            ..base
        });
        let lighter = probe(crate::db::FusionParams {
            bm25_heading_weight: 0.5,
            ..base
        });
        assert_ne!(
            heavier, lighter,
            "fixture is insensitive to the bm25 weights, so it cannot detect \
             a weight-dependent regression"
        );

        for &rrf_k in RRF_K_GRID.iter() {
            let got = probe(crate::db::FusionParams { rrf_k, ..base });
            assert_eq!(
                got, reference,
                "rrf_k={rrf_k} changed the FTS candidate list; \
                 build_metric_table shares one list across the whole rrf_k axis"
            );
        }
    }

    /// The sweep must cost one round trip per *weight* condition, not per
    /// condition. Losing the `c.k != 0` skip would still produce a correct
    /// table — six times more slowly — so correctness tests cannot catch it.
    #[test]
    fn test_sweep_makes_one_round_trip_per_weight_condition() {
        let (db, mut pre) = sweep_fixture();
        let n_queries = pre.queries.len();
        let mut meta = HashMap::new();

        crate::db::FTS_CANDIDATE_CALLS.with(|c| c.set(0));
        let table = build_metric_table(&db, &mut pre, &mut meta, &[PRIMARY_K], 10).unwrap();
        let calls = crate::db::FTS_CANDIDATE_CALLS.with(|c| c.get());

        assert_eq!(
            calls,
            WEIGHT_CONDITIONS * n_queries,
            "expected {WEIGHT_CONDITIONS} round trips per query over {n_queries} queries"
        );
        // And it really did fill every cell, rather than making fewer calls by
        // doing less work.
        assert_eq!(table.query_count(), n_queries);
    }

    /// Equivalence with the naive sweep: one `search_fts_candidates` per
    /// condition, all 384 of them. This is what says the reuse is not merely
    /// cheap but right.
    #[test]
    fn test_optimised_sweep_matches_the_naive_one() {
        let (db, mut pre) = sweep_fixture();
        // More than one k on purpose: with only PRIMARY_K the comparison below
        // sees a single depth, and a divergence confined to ranks past 5 shows
        // up nowhere.
        let k_values = [1_usize, PRIMARY_K, 10];
        let limit = 10_u32;
        let mut meta = HashMap::new();
        let optimised = build_metric_table(&db, &mut pre, &mut meta, &k_values, limit).unwrap();

        // Naive: no sharing at all — re-query for every condition, including
        // each rrf_k.
        let filters = crate::db::SearchFilters::default();
        let mut naive_meta = HashMap::new();
        let mut naive_rows = Vec::new();
        for pq in &pre.queries {
            let mut row = vec![crate::eval::QueryMetrics::default(); TOTAL_CONDITIONS];
            for c in Condition::all() {
                let params = c.to_params();
                let fts_hits = db
                    .search_fts_candidates(&pq.query, pre.pool_size, &filters, params)
                    .unwrap();
                let mut fts_ids = Vec::with_capacity(fts_hits.len());
                for (id, sr) in &fts_hits {
                    naive_meta.entry(*id).or_insert_with(|| HitMeta {
                        path: sr.path.clone(),
                        heading: sr.heading.clone(),
                    });
                    fts_ids.push(*id);
                }
                let ranked =
                    crate::db::fuse_rrf_ids(&pq.vec_ids, &fts_ids, params.rrf_k, Some(limit));
                let top = to_hit_records(&ranked, &naive_meta);
                row[c.index()] = crate::eval::compute_query_metrics(&pq.expected, &top, &k_values);
            }
            naive_rows.push(row);
        }
        let naive = MetricTable::from_rows(naive_rows, k_values.to_vec());

        for c in Condition::all() {
            for q in 0..pre.queries.len() {
                // The whole record, not just nDCG@PRIMARY_K: recall and MRR flow
                // into `aggregate_for` and then `non_degradation`, so a
                // divergence that leaves the primary metric intact would still
                // change what gets adopted (codex P2 round 1).
                let (o, n) = (optimised.cell(c, q), naive.cell(c, q));
                let at = format!("condition {c:?} query {q}");
                assert_eq!(o.ndcg_at_k, n.ndcg_at_k, "{at}: nDCG diverged");
                assert_eq!(
                    o.recall_at_k, n.recall_at_k,
                    "{at}: recall diverged - this feeds non_degradation"
                );
                assert_eq!(
                    o.reciprocal_rank, n.reciprocal_rank,
                    "{at}: MRR diverged - this feeds non_degradation"
                );
            }
        }
    }

    #[test]
    fn test_count_fts_matches_counts_phrase_hits() {
        // D-11-6 の doc-freq 診断の土台。feature-48 (v0.16.0) 以降、これは
        // 「単一 phrase の doc-freq」ではなく **OR 集合の和集合の大きさ** を数える。
        let db = tune_db();
        add_doc(&db, "a.md", "Zebrafish", "zebrafish larvae in assays", 0.1);
        add_doc(&db, "b.md", "More", "the zebrafish larvae grow fast", 0.2);
        add_doc(&db, "c.md", "Other", "completely unrelated prose here", 0.3);

        assert_eq!(db.count_fts_matches("zebrafish larvae").unwrap(), 2);
        assert_eq!(db.count_fts_matches("unrelated prose").unwrap(), 1);
        assert_eq!(db.count_fts_matches("nonexistent phrase xyz").unwrap(), 0);
        // クエリ全体は 1 件にも逐語で出現しないが、token に割れば
        // zebrafish が 2 件、prose が 1 件に当たる。旧実装ならここは 0 だった =
        // クエリのコンパイル規則が差し替わったことを DB 越しに判別する assert。
        assert_eq!(db.count_fts_matches("zebrafish prose").unwrap(), 3);
        // 有効 phrase が 1 つも作れないクエリは Ok(0)。「3 文字未満なら」ではない
        // (fallback があるので `ab` は落ちるが `AI と ML` は落ちない)。
        assert_eq!(db.count_fts_matches("ab").unwrap(), 0);
        assert_eq!(db.count_fts_matches("  ").unwrap(), 0);
    }

    #[test]
    fn test_to_hit_records_assigns_one_origin_ranks() {
        // eval::reciprocal_rank は 1-origin を前提とする (rank=0 は inf 汚染を
        // 避けるため 0.0 に倒される) ので、ここがずれると MRR が壊れる。
        let mut meta = HashMap::new();
        meta.insert(
            7_i64,
            HitMeta {
                path: "a.md".into(),
                heading: Some("H".into()),
            },
        );
        meta.insert(
            9_i64,
            HitMeta {
                path: "b.md".into(),
                heading: None,
            },
        );
        let ranked = [(7_i64, 0.5_f32), (9_i64, 0.25_f32)];
        let recs = to_hit_records(&ranked, &meta);

        assert_eq!(recs.len(), 2);
        assert_eq!(recs[0].rank, 1);
        assert_eq!(recs[0].path, "a.md");
        assert_eq!(recs[0].heading.as_deref(), Some("H"));
        assert_eq!(recs[0].score, 0.5);
        assert_eq!(recs[1].rank, 2);
        assert_eq!(recs[1].path, "b.md");
        assert!(recs[1].heading.is_none());
    }

    #[test]
    fn test_preflight_classifies_effective_and_insensitive_queries() {
        // E-6: 実効 query = FTS 候補 >= 2 件。0 件 (逐語出現なし) と
        // 1 件 (rank 固定) はどちらも fusion パラメータに不感。
        let db = tune_db();
        add_doc(&db, "a.md", "Zebrafish", "zebrafish larvae in assays", 0.10);
        add_doc(&db, "b.md", "More", "the zebrafish larvae grow fast", 0.11);
        add_doc(
            &db,
            "c.md",
            "Unique",
            "a solitary quokka appears once",
            0.12,
        );
        add_doc(&db, "d.md", "Filler", "nothing relevant in this note", 0.13);

        let golden = golden_with(vec![
            ("effective", "zebrafish larvae", vec!["a.md", "b.md"]),
            ("single-hit", "solitary quokka", vec!["c.md"]),
            (
                "no-hit",
                "a question phrased in natural language",
                vec!["d.md"],
            ),
            ("empty-expected", "zebrafish larvae", vec![]),
        ]);
        // expected 空の 1 件は落ちるので embedding は 3 本
        let embeddings = vec![emb(0.10), emb(0.12), emb(0.13)];

        let mut meta = HashMap::new();
        let pre = preflight_from_embeddings(&db, &golden, &embeddings, 10, &mut meta).unwrap();

        assert_eq!(pre.queries.len(), 3, "expected-less query must be dropped");
        assert_eq!(
            pre.effective,
            vec![0],
            "only the 2-candidate query is effective"
        );
        assert_eq!(pre.queries[0].diag.fts_candidates, 2);
        assert_eq!(pre.queries[1].diag.fts_candidates, 1);
        assert_eq!(pre.queries[2].diag.fts_candidates, 0);
        assert!(pre.queries[0].diag.is_effective());
        assert!(!pre.queries[1].diag.is_effective());

        // vec 候補は fusion 非依存に 1 回だけ取れていること
        assert!(!pre.queries[0].vec_ids.is_empty());
        // meta には vec / FTS 双方の chunk が入っていること
        assert!(
            meta.len() >= 2,
            "meta must carry path/heading for all candidates"
        );
        // E-8: pool は limit*5 max 50 の floor
        assert_eq!(pre.pool_size, 50);
        assert_eq!(pre.chunk_total, 4);
    }

    #[test]
    fn test_preflight_flags_idf_clamp_and_overlap() {
        // D-11-6: phrase が chunk 総数の半分以上に出れば IDF が 1e-6 に潰れる。
        // D-11-5: vec pool と FTS list の重複が 0 なら rrf_k は順位を動かせない。
        let db = tune_db();
        for i in 0..4 {
            add_doc(
                &db,
                &format!("common_{i}.md"),
                "Common",
                "shared boilerplate sentence appears everywhere",
                0.1 + i as f32 * 0.01,
            );
        }
        let golden = golden_with(vec![(
            "clamped",
            "shared boilerplate sentence",
            vec!["common_0.md"],
        )]);
        let mut meta = HashMap::new();
        let pre = preflight_from_embeddings(&db, &golden, &[emb(0.10)], 10, &mut meta).unwrap();

        assert_eq!(pre.queries[0].diag.fts_total_matches, 4);
        assert!(
            pre.queries[0].diag.idf_clamped,
            "4 of 4 chunks match the phrase, so IDF must be reported as clamped"
        );
        // 4 chunk しかないので vec pool (50) は全件を返し、FTS 4 件と全部重なる
        assert_eq!(pre.queries[0].diag.vec_fts_overlap, 4);
    }

    #[test]
    fn test_preflight_rejects_embedding_count_mismatch() {
        let db = tune_db();
        add_doc(&db, "a.md", "A", "zebrafish larvae in assays", 0.1);
        let golden = golden_with(vec![("q", "zebrafish larvae", vec!["a.md"])]);
        let mut meta = HashMap::new();
        let err = preflight_from_embeddings(&db, &golden, &[], 10, &mut meta).unwrap_err();
        assert!(
            err.to_string().contains("embedding"),
            "mismatch must be reported explicitly: {err}"
        );
    }

    /// テスト用: nDCG@PRIMARY_K だけを持つ MetricTable を組む。
    /// `values[query][condition_index]` が nDCG@5 になる。
    fn table_from_primary(values: Vec<Vec<f64>>) -> MetricTable {
        let rows = values
            .into_iter()
            .map(|row| {
                row.into_iter()
                    .map(|v| {
                        let mut m = crate::eval::QueryMetrics::default();
                        m.ndcg_at_k.insert(PRIMARY_K, v);
                        m.recall_at_k.insert(PRIMARY_K, v);
                        m.reciprocal_rank = v;
                        m
                    })
                    .collect()
            })
            .collect();
        MetricTable::from_rows(rows, vec![PRIMARY_K])
    }

    /// 全条件を既定値と同じスコアで埋めた表 (= landscape 完全平坦)。
    fn flat_table(n_queries: usize, value: f64) -> MetricTable {
        table_from_primary(vec![vec![value; TOTAL_CONDITIONS]; n_queries])
    }

    /// `decide` が `Verdict::Adopt` を返す表。
    ///
    /// `test_decide_adopts_a_large_consistent_gain` と同じ形。fold 完全一致 +
    /// 全指標非悪化にするため、勝者列だけを大きく上げ、SE > 0 を保つだけの
    /// 分散を入れる。
    fn adopting_table() -> (MetricTable, Condition, Vec<usize>) {
        let winner = Condition {
            h: 3,
            ctx: 1,
            content: 1,
            k: Condition::builtin_default().k,
        };
        let mut rows = vec![vec![0.20_f64; TOTAL_CONDITIONS]; 12];
        for (i, r) in rows.iter_mut().enumerate() {
            r[winner.index()] = 0.50 + (i % 3) as f64 * 0.01;
        }
        (table_from_primary(rows), winner, (0..12).collect())
    }

    /// 主指標だけが上がり、**二次指標が baseline を下回る**表。
    ///
    /// `table_from_primary` は nDCG / recall / MRR に同じ値を書くので、この
    /// 食い違いを表現できない。`non_degradation` が見ているのは recall と MRR
    /// なので、それを主指標と独立に置けないと違反 branch に到達しない。
    fn table_with_secondary_drop(cand: Condition, secondary: f64) -> (MetricTable, Vec<usize>) {
        let mut rows = vec![vec![0.20_f64; TOTAL_CONDITIONS]; 12];
        for (i, r) in rows.iter_mut().enumerate() {
            r[cand.index()] = 0.50 + (i % 3) as f64 * 0.01;
        }
        let built = rows
            .into_iter()
            .map(|row| {
                row.into_iter()
                    .enumerate()
                    .map(|(i, v)| {
                        let mut m = crate::eval::QueryMetrics::default();
                        m.ndcg_at_k.insert(PRIMARY_K, v);
                        // 勝者列だけ二次指標を落とす。他列は主指標と同じ。
                        let s = if i == cand.index() { secondary } else { v };
                        m.recall_at_k.insert(PRIMARY_K, s);
                        m.reciprocal_rank = s;
                        m
                    })
                    .collect()
            })
            .collect();
        (
            MetricTable::from_rows(built, vec![PRIMARY_K]),
            (0..12).collect(),
        )
    }

    #[test]
    fn test_metric_table_reads_primary_metric() {
        let mut rows = vec![vec![0.1_f64; TOTAL_CONDITIONS]; 2];
        let target = Condition {
            h: 3,
            ctx: 0,
            content: 0,
            k: 0,
        };
        rows[1][target.index()] = 0.9;
        let t = table_from_primary(rows);

        assert!((t.primary(target, 1) - 0.9).abs() < 1e-12);
        assert!((t.primary(target, 0) - 0.1).abs() < 1e-12);
        // mean は指定 query 集合の平均
        assert!((t.mean_primary(target, &[0, 1]) - 0.5).abs() < 1e-12);
        assert!((t.mean_primary(target, &[1]) - 0.9).abs() < 1e-12);
    }

    #[test]
    fn test_metric_table_aggregate_matches_manual_mean() {
        let t = flat_table(4, 0.25);
        let agg = t.aggregate_for(Condition::builtin_default(), &[0, 1, 2, 3]);
        assert_eq!(agg.query_count, 4);
        assert!((agg.ndcg_at_k[&PRIMARY_K] - 0.25).abs() < 1e-12);
        assert!((agg.recall_at_k[&PRIMARY_K] - 0.25).abs() < 1e-12);
        assert!((agg.mrr - 0.25).abs() < 1e-12);
    }

    /// `build_metric_table` を実 DB 経路で回すための fixture。
    /// heading にだけ語を置いた doc と content にだけ置いた doc を混ぜ、
    /// bm25 重みの grid 端で FTS 順位が動く状態を作る。
    fn sensitive_fixture() -> (crate::db::Database, Preflight, HashMap<i64, HitMeta>) {
        let db = tune_db();
        add_doc(
            &db,
            "h.md",
            "zebrafish",
            "body prose without the term",
            0.10,
        );
        add_doc(
            &db,
            "c.md",
            "unrelated",
            "zebrafish zebrafish zebrafish",
            0.11,
        );
        add_doc(
            &db,
            "m.md",
            "zebrafish notes",
            "zebrafish appears here too",
            0.12,
        );
        add_doc(&db, "x.md", "other", "nothing relevant in this note", 0.90);

        let golden = golden_with(vec![("zf", "zebrafish", vec!["h.md", "m.md"])]);
        let mut meta = HashMap::new();
        let pre = preflight_from_embeddings(&db, &golden, &[emb(0.10)], 10, &mut meta).unwrap();
        (db, pre, meta)
    }

    #[test]
    fn test_build_metric_table_fills_every_condition() {
        let (db, mut pre, mut meta) = sensitive_fixture();
        assert_eq!(
            pre.effective,
            vec![0],
            "fixture must have an effective query"
        );

        let table = build_metric_table(&db, &mut pre, &mut meta, &[PRIMARY_K], 10).unwrap();

        assert_eq!(table.query_count(), 1);
        // 384 条件すべてに metric が入っていること。この fixture は全 4 chunk が
        // 融合結果の top-5 に必ず入るので、どの条件でも expected 2 件が top-5 に
        // 現れ nDCG@5 > 0 になる。したがって 0.0 は「`unwrap_or(0.0)` に落ちた
        // 未充填セル」を意味する (= 0..=1 の範囲チェックでは検出できない)。
        for c in Condition::all() {
            let m = table.primary(c, 0);
            assert!(
                m > 0.0,
                "condition {} left an unfilled metric (fell back to unwrap_or(0.0)): {m}",
                c.label()
            );
        }
        // expected 2 件のうち少なくとも 1 件は top-5 に入る fixture なので
        // 既定条件の nDCG@5 は 0 より大きい。
        assert!(
            table.primary(Condition::builtin_default(), 0) > 0.0,
            "the default condition must retrieve at least one expected hit"
        );
    }

    #[test]
    fn test_build_metric_table_k_axis_cells_are_consistent() {
        // 同じ重み組の 6 つの rrf_k セルが正しい添字へ書き込まれていることの
        // 検証。この fixture は候補が 4 chunk しかなく、どの rrf_k でも融合順位
        // が変わらないので、6 セルすべてが同じ nDCG になるはずである。値がずれ
        // るのは k 軸の添字計算が壊れたか、条件ごとに別の FTS 結果を掴んだ場合。
        //
        // 既知の未カバー領域: 「重み組ごとに SQL 1 往復」という往復回数の最適化
        // 自体はここでは検証できない (往復回数を計測する手段がないため)。
        let (db, mut pre, mut meta) = sensitive_fixture();
        let table = build_metric_table(&db, &mut pre, &mut meta, &[PRIMARY_K], 10).unwrap();

        let base = Condition::builtin_default();
        let at = |k: usize| table.primary(Condition { k, ..base }, 0);
        for (k, &rrf_k) in RRF_K_GRID.iter().enumerate() {
            assert!(
                (at(k) - at(base.k)).abs() < 1e-12,
                "rrf_k={} produced a different metric than the default on a fixture \
                 where the ranking cannot change: {} vs {}",
                rrf_k,
                at(k),
                at(base.k)
            );
        }
    }

    #[test]
    fn test_build_metric_table_fills_bm25_sensitivity_diagnostic() {
        // D-11-5: grid 端 (heading 偏重 vs content 偏重) で FTS 順位が動いたか。
        // 本 fixture は heading のみ / content のみに語を置いてあるので立つ。
        let (db, mut pre, mut meta) = sensitive_fixture();
        assert!(
            !pre.queries[0].diag.bm25_sensitive,
            "pre-flight must leave the diagnostic unset"
        );

        let _ = build_metric_table(&db, &mut pre, &mut meta, &[PRIMARY_K], 10).unwrap();

        assert!(
            pre.queries[0].diag.bm25_sensitive,
            "heading-only vs content-only docs must reorder between the grid extremes"
        );
    }

    #[test]
    fn test_mean_and_sample_sd() {
        assert!((mean(&[1.0, 2.0, 3.0]) - 2.0).abs() < 1e-12);
        assert_eq!(mean(&[]), 0.0);
        // 不偏 SD (分母 n-1): [1,2,3] -> sqrt(1.0) == 1.0
        assert!((sample_sd(&[1.0, 2.0, 3.0]) - 1.0).abs() < 1e-12);
        assert_eq!(sample_sd(&[5.0]), 0.0);
    }

    /// Deterministic LCG + Box-Muller, so the simulation below is reproducible
    /// and needs no new dependency (the project does not pull `rand` in).
    struct Lcg(u64);
    impl Lcg {
        fn next_u64(&mut self) -> u64 {
            // Numerical Recipes constants.
            self.0 = self
                .0
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            self.0
        }
        fn unit(&mut self) -> f64 {
            // (0, 1], avoiding 0 so ln() stays finite.
            ((self.next_u64() >> 11) as f64 + 1.0) / ((1u64 << 53) as f64)
        }
        fn normal(&mut self) -> f64 {
            let (u1, u2) = (self.unit(), self.unit());
            (-2.0 * u1.ln()).sqrt() * (std::f64::consts::TAU * u2).cos()
        }
    }

    /// Does `paired_se` over-state or under-state the true standard error of
    /// the LOO mean delta? (AU-16)
    ///
    /// The doc comment on `paired_se` used to assert the estimate "errs
    /// conservatively (does not underestimate)" despite the folds sharing
    /// queries. That is an empirical claim about this estimator, so it is
    /// measured rather than argued: draw many independent golden sets from a
    /// known process, and compare
    ///
    /// - the **reported** SE, averaged over replications, with
    /// - the **true** SE, i.e. the spread of the mean delta across those
    ///   replications.
    ///
    /// A ratio below 1 means the reported SE is smaller than the real one, so
    /// the adoption gate `mean_delta > ADOPT_SE_MULTIPLIER * se` fires more
    /// easily than intended — the opposite of conservative.
    ///
    /// `#[ignore]`: a few hundred replications of a 26-query nested LOO over
    /// the full condition grid is far too slow for the default suite.
    #[test]
    #[ignore = "simulation; run with: cargo test --lib au16 -- --ignored --nocapture"]
    fn au16_paired_se_versus_the_true_standard_error() {
        const REPS: usize = 300;

        // Several settings, so the direction is not an artifact of one choice
        // of sizes, effect size or noise level.
        let settings: [(usize, f64, f64, f64, u64); 4] = [
            //  N,  true edge, per-query sd, per-cell sd, seed
            (26, 0.04, 0.05, 0.08, 0x5EED_0001),
            (26, 0.00, 0.05, 0.08, 0x5EED_0002), // no real winner at all
            (12, 0.04, 0.05, 0.08, 0x5EED_0003),
            (26, 0.04, 0.05, 0.03, 0x5EED_0004), // quieter cells
        ];

        // `k` must be the built-in value (codex P2 round 1). `select_condition`
        // runs in two phases: phase W scans the weight tuples with `k` pinned to
        // the default, and only then does phase K sweep `k` for the tuple it
        // chose. A winner at some other `k` is therefore invisible while the
        // weights are being picked, so the edge would only ever be reached in
        // the replications where noise happened to select that tuple anyway —
        // which quietly turns every "true edge" setting back into the null one.
        let winner = Condition {
            h: 3,
            ctx: 1,
            content: 1,
            k: Condition::builtin_default().k,
        };
        eprintln!("AU-16 simulation ({REPS} replications per setting)");
        for (n, edge, q_sd, cell_sd, seed) in settings {
            let mut rng = Lcg(seed);
            let mut means = vec![0.40_f64; TOTAL_CONDITIONS];
            means[winner.index()] = 0.40 + edge;

            let mut mean_deltas = Vec::with_capacity(REPS);
            let mut reported_ses = Vec::with_capacity(REPS);
            let mut stabilities: Vec<f64> = Vec::with_capacity(REPS);
            // How often each gate actually fires. A ratio of averages does not
            // determine this: `se` varies per replication and can correlate
            // with the observed `mean_delta`, so "the SE is 0.55x too small"
            // cannot be turned into a sigma level by arithmetic (codex P2
            // round 4). The rejection rate is the thing itself.
            let mut se_gate_fired = 0usize;
            let mut adopted = 0usize;
            // Which condition the refit picked, per replication. Fold agreement
            // *within* a replication does not by itself make the d_j
            // independent: the agreed condition is still chosen from the shared
            // training rows, so it is a random input every difference depends
            // on. What would remove that coupling is the selection being
            // effectively fixed *across* sampled golden sets — which is a
            // separate thing, and was being asserted rather than recorded
            // (codex P2 round 6). Recording it.
            // Record the *fold* selections, not `refit` (codex P2 round 7).
            // `refit` is chosen from all N rows; every `d_j` is generated from
            // `fold_selections[j]`, chosen from that fold's N-1 rows. A setting
            // can have one identical refit across all replications while the
            // fold selections still vary, so `refit` cannot answer whether the
            // selection generating the differences was fixed.
            let mut fold_picks: std::collections::HashSet<Condition> =
                std::collections::HashSet::new();
            let mut refits: std::collections::HashSet<Condition> = std::collections::HashSet::new();
            for _ in 0..REPS {
                let rows: Vec<Vec<f64>> = (0..n)
                    .map(|_| {
                        let per_query = q_sd * rng.normal(); // query difficulty
                        means
                            .iter()
                            .map(|m| m + per_query + cell_sd * rng.normal())
                            .collect()
                    })
                    .collect();
                let table = table_from_primary(rows);
                let idx: Vec<usize> = (0..n).collect();
                let out = nested_loo(&table, &idx);
                let m = mean(&out.diffs);
                let se = paired_se(&out.diffs);
                if m > 2.0 * se {
                    se_gate_fired += 1;
                }
                if matches!(decide(&table, &out, &idx), Verdict::Adopt(_)) {
                    adopted += 1;
                }
                refits.insert(out.refit);
                for c in &out.fold_selections {
                    fold_picks.insert(*c);
                }
                mean_deltas.push(m);
                reported_ses.push(se);
                stabilities.push(out.stability);
            }

            // The ratio is an ensemble quantity: it needs a spread of mean
            // deltas across replications, so it cannot be computed for a single
            // run. Reporting it next to `mean(stabilities)` therefore says
            // nothing about whether the replications *responsible* for the
            // understatement clear `decide`'s stability gate — the two numbers
            // are separate aggregates over the same set (codex P2 round 2).
            //
            // So stratify: recompute the ratio within the replications that
            // pass `stability > STABILITY_MIN`, which is the only subset
            // `decide` can ever adopt from.
            let ratio_over = |keep: &dyn Fn(usize) -> bool| -> Option<(usize, f64)> {
                let d: Vec<f64> = mean_deltas
                    .iter()
                    .enumerate()
                    .filter(|(i, _)| keep(*i))
                    .map(|(_, v)| *v)
                    .collect();
                let s: Vec<f64> = reported_ses
                    .iter()
                    .enumerate()
                    .filter(|(i, _)| keep(*i))
                    .map(|(_, v)| *v)
                    .collect();
                if d.len() < 2 {
                    return None;
                }
                let sd = sample_sd(&d);
                (sd > 0.0).then(|| (d.len(), mean(&s) / sd))
            };

            let all = ratio_over(&|_| true);
            let gated = ratio_over(&|i| stabilities[i] > STABILITY_MIN);
            let fmt = |r: Option<(usize, f64)>| match r {
                Some((k, v)) => format!("{v:.3} (n={k})"),
                None => "n/a".to_string(),
            };
            eprintln!(
                "  N={n:<3} edge={edge:<5} cell_sd={cell_sd:<5} | \
                 all reps: {:<14} passing the stability gate: {:<14} mean stability={:.2}\n      \
                 fired: `m > 2*se` {:>5.1}%   full decide() Adopt {:>5.1}%   \
                 distinct fold selections: {}  (refits: {})",
                fmt(all),
                fmt(gated),
                mean(&stabilities),
                100.0 * se_gate_fired as f64 / REPS as f64,
                100.0 * adopted as f64 / REPS as f64,
                fold_picks.len(),
                refits.len(),
            );
            let true_se = sample_sd(&mean_deltas);
            let avg_reported = mean(&reported_ses);
            assert!(true_se > 0.0, "degenerate simulation: no spread to measure");
            assert!(avg_reported.is_finite(), "reported SE must be finite");
        }

        // No threshold assertion: the point of this test is the numbers it
        // prints, and pinning a ratio would turn a measurement into a brittle
        // expectation.
    }

    /// What do the two numeric adoption thresholds actually buy? (AU-68)
    ///
    /// `au16_paired_se_versus_the_true_standard_error` established *that* the
    /// null adoption rate is 12.7% where a calibrated one-sided 2 sigma would
    /// be about 2.3%. It deliberately stopped there: moving a threshold
    /// changes what `groove tune` recommends to its users, which is a decision
    /// and not a doc fix. This test supplies the numbers that decision needs,
    /// by sweeping `ADOPT_SE_MULTIPLIER` against `ADOPT_MIN_MEAN_DELTA` and
    /// reporting the adoption rate in each cell.
    ///
    /// The same rate means different things per row:
    ///
    /// - `edge = 0.00` — no condition is genuinely better, so **every**
    ///   adoption is a false one. This is the number AU-68 wants lowered.
    /// - `edge > 0.00` — adoption is power, but only when the adopted
    ///   condition is the one that is genuinely better. Adopting some *other*
    ///   condition on a real-edge landscape is not a success, so the strict
    ///   count is reported in parentheses alongside the raw one.
    ///
    /// That the sweep measures the *deployed* rule rather than a paraphrase of
    /// it is checked per replication: whichever cell holds the shipping
    /// thresholds is asserted equal to what `decide` itself returns.
    ///
    /// The seeds and the first four settings also match the AU-16 test, so
    /// with `REPS` at 300 the `mult=2.0` column comes out at 35.0 / 12.7 /
    /// 20.0 / 99.0 — the adoption rates that shipped before AU-68, and the
    /// figures `docs/eval.md` quotes for the old gate. `REPS` is higher here
    /// because a rate near 3% cannot be resolved from 300 draws.
    ///
    /// `#[ignore]`: same cost as its AU-16 sibling, times the grid.
    #[test]
    #[ignore = "simulation; run with: cargo test --lib au68 -- --ignored --nocapture"]
    fn au68_adoption_rate_across_the_two_thresholds() {
        const REPS: usize = 2000;
        const MULTIPLIERS: [f64; 6] = [2.0, 2.5, 3.0, 3.5, 4.0, 5.0];
        const MIN_DELTAS: [f64; 3] = [0.02, 0.03, 0.04];

        // Find the shipping thresholds in the grid instead of assuming where
        // they sit. Pinning them to index 0 made this test fail the moment
        // AU-68 moved the very constant it exists to justify — and because it
        // is `#[ignore]`d, a green `cargo test --workspace` said nothing about
        // it.
        let anchor_i = MULTIPLIERS
            .iter()
            .position(|m| *m == ADOPT_SE_MULTIPLIER)
            .expect("the swept grid must contain the shipping SE multiplier");
        let anchor_j = MIN_DELTAS
            .iter()
            .position(|d| *d == ADOPT_MIN_MEAN_DELTA)
            .expect("the swept grid must contain the shipping mean-delta floor");

        // The four AU-16 settings with their seeds, plus a second null
        // landscape at the smaller N — the rate being tuned should not be
        // read off a single sample size.
        let settings: [(usize, f64, f64, f64, u64); 5] = [
            //  N,  true edge, per-query sd, per-cell sd, seed
            (26, 0.04, 0.05, 0.08, 0x5EED_0001),
            (26, 0.00, 0.05, 0.08, 0x5EED_0002), // no real winner at all
            (12, 0.04, 0.05, 0.08, 0x5EED_0003),
            (26, 0.04, 0.05, 0.03, 0x5EED_0004), // quieter cells
            (12, 0.00, 0.05, 0.08, 0x5EED_0005), // null, smaller N
        ];

        // Pinned to the built-in `k` for the reason spelled out in the AU-16
        // test: phase W picks the weights with `k` held at the default, so a
        // winner at any other `k` is invisible while the weights are chosen
        // and every "true edge" setting would quietly collapse to the null.
        let winner = Condition {
            h: 3,
            ctx: 1,
            content: 1,
            k: Condition::builtin_default().k,
        };

        eprintln!("AU-68 threshold sweep ({REPS} replications per setting)");
        for (n, edge, q_sd, cell_sd, seed) in settings {
            let mut rng = Lcg(seed);
            let mut means = vec![0.40_f64; TOTAL_CONDITIONS];
            means[winner.index()] = 0.40 + edge;

            let mut adopted = [[0usize; MIN_DELTAS.len()]; MULTIPLIERS.len()];
            let mut adopted_winner = [[0usize; MIN_DELTAS.len()]; MULTIPLIERS.len()];

            for _ in 0..REPS {
                let rows: Vec<Vec<f64>> = (0..n)
                    .map(|_| {
                        let per_query = q_sd * rng.normal(); // query difficulty
                        means
                            .iter()
                            .map(|m| m + per_query + cell_sd * rng.normal())
                            .collect()
                    })
                    .collect();
                let table = table_from_primary(rows);
                let idx: Vec<usize> = (0..n).collect();
                let out = nested_loo(&table, &idx);

                for (i, mult) in MULTIPLIERS.iter().enumerate() {
                    for (j, delta) in MIN_DELTAS.iter().enumerate() {
                        let verdict = decide_with(&table, &out, &idx, *mult, *delta);
                        if i == anchor_i && j == anchor_j {
                            assert_eq!(
                                verdict,
                                decide(&table, &out, &idx),
                                "the shipping cell must be `decide` itself"
                            );
                        }
                        if let Verdict::Adopt(c) = verdict {
                            adopted[i][j] += 1;
                            if c == winner {
                                adopted_winner[i][j] += 1;
                            }
                        }
                    }
                }
            }

            let pct = |c: usize| 100.0 * c as f64 / REPS as f64;
            let kind = if edge == 0.0 {
                "null: every adoption below is a false one"
            } else {
                "real edge: parenthesised = adopted the true winner"
            };
            eprintln!("\n  N={n:<3} edge={edge:<5} cell_sd={cell_sd:<5}  [{kind}]");
            eprint!("    se_mult \\ min_delta ");
            for d in MIN_DELTAS {
                eprint!("|  {d:.3}          ");
            }
            eprintln!();
            for (i, mult) in MULTIPLIERS.iter().enumerate() {
                eprint!("    {mult:>19.1} ");
                for j in 0..MIN_DELTAS.len() {
                    eprint!(
                        "| {:>5.1}% ({:>5.1}%) ",
                        pct(adopted[i][j]),
                        pct(adopted_winner[i][j])
                    );
                }
                eprintln!();
            }

            // Raising either threshold can only ever remove adoptions, so the
            // grid must be non-increasing along both axes. This is the check
            // that the sweep is a real family of decision rules: a flipped
            // comparison anywhere would still print a plausible-looking table,
            // and every number in it would be meaningless.
            for i in 0..MULTIPLIERS.len() {
                for j in 0..MIN_DELTAS.len() {
                    if i > 0 {
                        assert!(
                            adopted[i][j] <= adopted[i - 1][j],
                            "raising the SE multiplier to {} added adoptions at min_delta {}",
                            MULTIPLIERS[i],
                            MIN_DELTAS[j]
                        );
                    }
                    if j > 0 {
                        assert!(
                            adopted[i][j] <= adopted[i][j - 1],
                            "raising min_delta to {} added adoptions at multiplier {}",
                            MIN_DELTAS[j],
                            MULTIPLIERS[i]
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn test_paired_se_is_sd_over_sqrt_n() {
        // D-11-2: SE は paired per-query 差分の SD/sqrt(N)。
        // fold 平均の SD (1/(N-1) に縮む) を SE と呼んではならない。
        let d = [1.0, 2.0, 3.0, 4.0];
        let expected = sample_sd(&d) / 2.0; // sqrt(4) == 2
        assert!((paired_se(&d) - expected).abs() < 1e-12);
        // N < 2 は判定不能 = 無限大にして採用を必ず落とす (保守側)
        assert!(paired_se(&[0.5]).is_infinite());
        assert!(paired_se(&[]).is_infinite());
    }

    #[test]
    fn test_sign_test_counts_and_exact_p() {
        let r = sign_test(&[1.0, 1.0, -1.0]);
        assert_eq!((r.positive, r.negative, r.ties), (2, 1, 0));
        // 2 * P(X <= 1), X ~ Bin(3, 0.5) = 2 * (1+3)/8 = 1.0
        assert!((r.p_value - 1.0).abs() < 1e-12);

        let r = sign_test(&[1.0; 5]);
        assert_eq!((r.positive, r.negative), (5, 0));
        // 2 * P(X <= 0) = 2 * 1/32 = 0.0625
        assert!((r.p_value - 0.0625).abs() < 1e-12);

        // 同値は捨てる (標準的な sign test)
        let r = sign_test(&[0.0, 0.0, 1.0]);
        assert_eq!((r.positive, r.negative, r.ties), (1, 0, 2));
    }

    #[test]
    fn test_select_condition_prefers_default_on_a_flat_landscape() {
        // landscape が完全平坦なら既定条件を選ぶ (タイブレークは default 優先)。
        let t = flat_table(5, 0.4);
        assert_eq!(
            select_condition(&t, &[0, 1, 2, 3, 4]),
            Condition::builtin_default()
        );
    }

    #[test]
    fn test_select_condition_prefers_the_grid_centre_on_a_plateau() {
        // D-11-7 後半: プラトーでは端ではなく中央の値を推奨する。
        // 既定条件を含まない 2 条件を完全同点にし、中央寄りが選ばれることを見る。
        let base = Condition::builtin_default();
        let edge = Condition {
            h: 0,
            ctx: 0,
            content: 0,
            k: base.k,
        }; // 0.5 / 0.5 / 0.5 = グリッドの端
        let centre = Condition {
            h: 1,
            ctx: 1,
            content: 1,
            k: base.k,
        }; // 1.0 / 1.0 / 1.0 = 中央寄り
        let mut rows = vec![vec![0.20_f64; TOTAL_CONDITIONS]; 3];
        for r in rows.iter_mut() {
            r[edge.index()] = 0.60;
            r[centre.index()] = 0.60;
        }
        let t = table_from_primary(rows);
        // 距離: edge = 1.5*3 + 1.5 = 6.0 / centre = 0.5*3 + 1.5 = 3.0
        assert_eq!(select_condition(&t, &[0, 1, 2]), centre);
    }

    #[test]
    fn test_select_condition_prefers_default_over_the_centre_when_tied() {
        // 既定条件が同点集合に含まれるなら、中央距離に関わらず既定条件が勝つ
        // (D-11-2 の default タイブレークが D-11-7 より優先)。
        let base = Condition::builtin_default();
        let centre = Condition {
            h: 1,
            ctx: 1,
            content: 1,
            k: base.k,
        };
        // base = (2,1,1) の中央距離は 0.5*3 + 1.5 = 3.0 で centre と完全同値だが、
        // それでも「既定条件優先」の規則で base が返ること。
        let mut rows = vec![vec![0.20_f64; TOTAL_CONDITIONS]; 3];
        for r in rows.iter_mut() {
            r[base.index()] = 0.60;
            r[centre.index()] = 0.60;
        }
        let t = table_from_primary(rows);
        assert_eq!(select_condition(&t, &[0, 1, 2]), base);
    }

    #[test]
    fn test_select_condition_runs_coordinate_descent() {
        // D-9: Phase W (k=60 固定で重み 64 通り) → Phase K (勝者重みで k 6 通り)。
        // 「Phase W では負けるが、その重みの別 k では最良」という条件は
        // coordinate descent では **選ばれない** ことを固定する。
        let base = Condition::builtin_default();
        let winner_w = Condition {
            h: 3,
            ctx: 1,
            content: 1,
            k: base.k,
        };
        let winner_wk = Condition { k: 0, ..winner_w };
        // 全直積でしか届かない罠条件 (Phase W では k=60 の値が低い)
        let trap = Condition {
            h: 0,
            ctx: 0,
            content: 3,
            k: 1,
        };

        let mut rows = vec![vec![0.30_f64; TOTAL_CONDITIONS]; 3];
        for r in rows.iter_mut() {
            r[winner_w.index()] = 0.40;
            r[winner_wk.index()] = 0.55;
            r[trap.index()] = 0.90; // 全直積 argmax ならこれを選ぶ
        }
        let t = table_from_primary(rows);
        assert_eq!(select_condition(&t, &[0, 1, 2]), winner_wk);
    }

    #[test]
    fn test_nested_loo_reports_stability_and_paired_diffs() {
        // 全 query で同じ条件が勝つ landscape なら stability = 1.0、
        // 差分は per-query の実測差になる。
        let winner = Condition {
            h: 3,
            ctx: 1,
            content: 1,
            k: Condition::builtin_default().k,
        };
        let mut rows = vec![vec![0.20_f64; TOTAL_CONDITIONS]; 4];
        for r in rows.iter_mut() {
            r[winner.index()] = 0.50;
        }
        let t = table_from_primary(rows);
        let out = nested_loo(&t, &[0, 1, 2, 3]);

        assert_eq!(out.refit, winner);
        assert_eq!(out.fold_selections.len(), 4);
        assert!((out.stability - 1.0).abs() < 1e-12);
        assert_eq!(out.diffs.len(), 4);
        for d in &out.diffs {
            assert!((d - 0.30).abs() < 1e-12, "held-out delta must be 0.5 - 0.2");
        }
    }

    #[test]
    fn test_nested_loo_stability_falls_when_folds_disagree() {
        // fold ごとに違う条件が勝つ = 過学習の最も直接的な兆候。
        let a = Condition {
            h: 3,
            ctx: 1,
            content: 1,
            k: Condition::builtin_default().k,
        };
        let b = Condition {
            h: 0,
            ctx: 1,
            content: 1,
            k: Condition::builtin_default().k,
        };
        let mut rows = vec![vec![0.20_f64; TOTAL_CONDITIONS]; 4];
        // query 0,1 は a を、query 2,3 は b を強く支持する
        rows[0][a.index()] = 0.9;
        rows[1][a.index()] = 0.9;
        rows[2][b.index()] = 0.9;
        rows[3][b.index()] = 0.9;
        let t = table_from_primary(rows);
        let out = nested_loo(&t, &[0, 1, 2, 3]);
        assert!(
            out.stability < 1.0,
            "folds must disagree here, got stability={}",
            out.stability
        );
    }

    #[test]
    fn test_decide_keeps_default_when_gain_is_small() {
        // mean(d) が 0.02 に届かなければ default 維持 (D-11-2)。
        let winner = Condition {
            h: 3,
            ctx: 1,
            content: 1,
            k: Condition::builtin_default().k,
        };
        let mut rows = vec![vec![0.500_f64; TOTAL_CONDITIONS]; 6];
        for r in rows.iter_mut() {
            r[winner.index()] = 0.505; // +0.005 のみ
        }
        let t = table_from_primary(rows);
        let out = nested_loo(&t, &[0, 1, 2, 3, 4, 5]);
        let verdict = decide(&t, &out, &[0, 1, 2, 3, 4, 5]);
        match verdict {
            Verdict::KeepDefault { reasons } => {
                assert!(
                    reasons.iter().any(|r| r.contains("mean")),
                    "reason must mention the mean-delta threshold: {reasons:?}"
                );
            }
            Verdict::Adopt(c) => panic!("must not adopt a +0.005 gain, got {c:?}"),
        }
    }

    #[test]
    fn test_decide_keeps_default_on_a_flat_landscape() {
        let t = flat_table(12, 0.4);
        let idx: Vec<usize> = (0..12).collect();
        let out = nested_loo(&t, &idx);
        assert!(matches!(
            decide(&t, &out, &idx),
            Verdict::KeepDefault { .. }
        ));
    }

    #[test]
    fn test_decide_adopts_a_large_consistent_gain() {
        // 全 query で +0.30、fold 完全一致、全指標非悪化 → 採用。
        let winner = Condition {
            h: 3,
            ctx: 1,
            content: 1,
            k: Condition::builtin_default().k,
        };
        let mut rows = vec![vec![0.20_f64; TOTAL_CONDITIONS]; 12];
        for (i, r) in rows.iter_mut().enumerate() {
            // 分散をわずかに入れて SE > 0 にする (SD=0 の退化を避ける)
            r[winner.index()] = 0.50 + (i % 3) as f64 * 0.01;
        }
        let t = table_from_primary(rows);
        let idx: Vec<usize> = (0..12).collect();
        let out = nested_loo(&t, &idx);
        assert_eq!(decide(&t, &out, &idx), Verdict::Adopt(winner));
    }

    /// A gain that clears 2 x paired SE but not 3 x is rejected (AU-68).
    ///
    /// Nothing else in the suite pins `ADOPT_SE_MULTIPLIER`: every other
    /// `decide` fixture either misses the mean-delta floor or clears the SE
    /// gate by a wide margin, so the constant could be set to anything — or
    /// criterion 3 deleted outright — with all tests still green. AU-68 chose
    /// 3.0 by measuring the false-adoption rate it produces, and that choice
    /// needs something holding it in place.
    ///
    /// The fixture is placed deliberately *between* the two multipliers, and
    /// asserts from both sides: rejected at the shipping value, adopted at the
    /// old one. Landing outside the gap fails the second assertion, so the test
    /// cannot quietly decay into "some gate fired".
    #[test]
    fn test_decide_rejects_a_gain_between_two_and_three_paired_se() {
        let winner = Condition {
            h: 3,
            ctx: 1,
            content: 1,
            k: Condition::builtin_default().k,
        };
        // Per-query deltas of -0.02 / +0.05 / +0.12, four queries each:
        // mean 0.0500, sample SD 0.0606, so paired SE = 0.0202 over N=12 and
        // the mean sits at 2.47 x SE. Above the 0.02 floor, above 2 x SE,
        // below 3 x SE. Every fold still picks the winner (dropping any one
        // query leaves the training mean positive while all other conditions
        // stay flat), so stability is 1.0 and criterion 3 is the only one that
        // can bite.
        const DELTAS: [f64; 3] = [-0.02, 0.05, 0.12];
        let mut rows = vec![vec![0.30_f64; TOTAL_CONDITIONS]; 12];
        for (i, r) in rows.iter_mut().enumerate() {
            r[winner.index()] = 0.30 + DELTAS[i % DELTAS.len()];
        }
        let t = table_from_primary(rows);
        let idx: Vec<usize> = (0..12).collect();
        let out = nested_loo(&t, &idx);

        // Sanity: the fixture is where the comment says it is.
        let m = mean(&out.diffs);
        let se = paired_se(&out.diffs);
        assert!(
            m > 2.0 * se && m < 3.0 * se,
            "fixture must sit between 2x and 3x SE: mean {m:.4}, SE {se:.4}"
        );

        match decide(&t, &out, &idx) {
            Verdict::KeepDefault { reasons } => {
                assert!(
                    reasons.iter().any(|r| r.contains("paired SE")),
                    "criterion 3 must be the one that rejected it: {reasons:?}"
                );
                // If the mean-delta floor also fired, the fixture is testing
                // criterion 2 by accident and says nothing about the SE gate.
                assert!(
                    !reasons.iter().any(|r| r.contains("threshold")),
                    "the mean-delta floor must not be involved: {reasons:?}"
                );
            }
            Verdict::Adopt(c) => panic!("adopted a gain below 3 x paired SE: {c:?}"),
        }

        // ...and the same fixture was adopted at the multiplier AU-68 replaced,
        // which is what makes this a guard on the value and not just on the
        // gate's existence.
        assert_eq!(
            decide_with(&t, &out, &idx, 2.0, ADOPT_MIN_MEAN_DELTA),
            Verdict::Adopt(winner)
        );
    }

    #[test]
    fn test_per_query_impact_reports_worst_degradation() {
        let cand = Condition {
            h: 3,
            ctx: 1,
            content: 1,
            k: Condition::builtin_default().k,
        };
        let mut rows = vec![vec![0.40_f64; TOTAL_CONDITIONS]; 3];
        rows[0][cand.index()] = 0.80; // +0.40
        rows[1][cand.index()] = 0.15; // -0.25 (最大悪化)
        rows[2][cand.index()] = 0.40; // 同値
        let t = table_from_primary(rows);
        let impact = per_query_impact(&t, cand, &[0, 1, 2]);
        assert_eq!(impact.improved, 1);
        assert_eq!(impact.degraded, 1);
        assert!((impact.worst_delta + 0.25).abs() < 1e-12);
        assert_eq!(impact.worst_query, Some(1));
    }

    /// DB を使わずに TuneReport を組む (formatter だけをテストするため)。
    ///
    /// 平坦な表を渡すので verdict は必ず `KeepDefault` になる。**Adopt 側の
    /// 出力経路はこれでは踏めない**ので、表を差し替えられる
    /// [`synthetic_report_from`] を使う。
    fn synthetic_report() -> TuneReport {
        synthetic_report_from(flat_table(3, 0.4), vec![0_usize, 1, 2])
    }

    /// [`synthetic_report`] の表と対象 query を差し替えられる版。
    fn synthetic_report_from(table: MetricTable, idx: Vec<usize>) -> TuneReport {
        let outcome = nested_loo(&table, &idx);
        let verdict = decide(&table, &outcome, &idx);
        let impact = per_query_impact(&table, outcome.refit, &idx);
        let (_, violations) = non_degradation(&table, outcome.refit, &idx);
        TuneReport {
            kb_path: PathBuf::from("kb"),
            golden_path: PathBuf::from("kb/.groove-eval.yml"),
            model: "bge-small-en-v1.5".to_string(),
            limit: 10,
            pool_size: 50,
            k_values: vec![PRIMARY_K],
            chunk_total: 100,
            context_axis_noop: false,
            // Derived rather than fixed, so a caller passing a different table
            // does not get a report that contradicts itself. Unchanged for
            // `synthetic_report`, whose idx is still the same three queries.
            query_count: idx.len(),
            effective: idx.clone(),
            diagnostics: vec![
                (
                    "q0".to_string(),
                    QueryDiagnostics {
                        fts_candidates: 5,
                        fts_total_matches: 12,
                        vec_fts_overlap: 2,
                        bm25_sensitive: true,
                        idf_clamped: false,
                        rrf_ties: 0,
                    },
                ),
                (
                    "q1".to_string(),
                    QueryDiagnostics {
                        fts_candidates: 3,
                        fts_total_matches: 8,
                        vec_fts_overlap: 0,
                        bm25_sensitive: false,
                        idf_clamped: false,
                        rrf_ties: 1,
                    },
                ),
                (
                    "q2".to_string(),
                    QueryDiagnostics {
                        fts_candidates: 2,
                        fts_total_matches: 60,
                        vec_fts_overlap: 1,
                        bm25_sensitive: true,
                        idf_clamped: true,
                        rrf_ties: 0,
                    },
                ),
            ],
            baseline: table.aggregate_for(Condition::builtin_default(), &idx),
            refit_aggregate: table.aggregate_for(outcome.refit, &idx),
            top_weight_conditions: vec![(Condition::builtin_default(), 0.4)],
            top_k_conditions: vec![(Condition::builtin_default(), 0.4)],
            sign: sign_test(&outcome.diffs),
            impact,
            violations,
            outcome,
            verdict,
        }
    }

    /// Every formatter test built its report from a flat table, so `format_text`
    /// only ever ran its `KeepDefault` arm — the recommendation line and the
    /// TOML snippet, which are the whole point of a run that adopts, were never
    /// produced by any test.
    #[test]
    fn test_format_text_emits_the_recommendation_and_snippet_when_adopting() {
        let (table, winner, idx) = adopting_table();
        let report = synthetic_report_from(table, idx);
        assert!(
            matches!(report.verdict, Verdict::Adopt(c) if c == winner),
            "fixture must adopt, got {:?}",
            report.verdict
        );
        let out = format_text(&report, false);
        assert!(out.contains("Recommendation:"), "{out}");
        assert!(out.contains(&winner.label()), "{out}");
        // The snippet must be the *adopted* one, not merely a well-formed
        // `[search.fusion]` block: emitting the built-in default here while
        // recommending `winner` would have the user paste parameters that
        // contradict the recommendation, and a header-and-key check would not
        // notice (codex P2 round 1).
        assert!(
            out.contains(&toml_snippet(winner)),
            "text output must embed the snippet for the adopted condition\n\
             --- expected ---\n{}\n--- got ---\n{out}",
            toml_snippet(winner)
        );
        // And it must still be the advice to re-verify with the full pipeline.
        assert!(out.contains("groove eval"), "{out}");
    }

    /// Same gap on the JSON side: `"decision": "adopt"` and its `toml_snippet`
    /// had no coverage, so a consumer parsing the machine-readable output could
    /// have broken without any test noticing.
    #[test]
    fn test_format_json_reports_adopt_with_a_pasteable_snippet() {
        let (table, winner, idx) = adopting_table();
        let report = synthetic_report_from(table, idx);
        let v = format_json(&report);
        assert_eq!(v["verdict"]["decision"], "adopt", "{v}");
        // Assert the *values*, not just that a `condition` object exists: a
        // machine consumer reads this field, so serialising the wrong
        // condition here would hand it incorrect tuning parameters while the
        // separately generated snippet still looked right (codex P2 round 1).
        let p = winner.to_params();
        let cond = &v["verdict"]["condition"];
        assert_eq!(cond["rrf_k"], p.rrf_k, "{v}");
        assert_eq!(cond["bm25_heading_weight"], p.bm25_heading_weight, "{v}");
        assert_eq!(cond["bm25_context_weight"], p.bm25_context_weight, "{v}");
        assert_eq!(cond["bm25_content_weight"], p.bm25_content_weight, "{v}");
        let snippet = v["verdict"]["toml_snippet"]
            .as_str()
            .expect("adopt must carry a snippet");
        assert!(snippet.contains("[search.fusion]"), "{snippet}");
        // The snippet is only useful if it round-trips into a valid config.
        let cfg: crate::config::Config =
            toml::from_str(snippet).expect("snippet must be valid toml");
        assert!(cfg.validate().is_ok(), "snippet must pass validate()");
        assert_eq!(
            crate::db::FusionParams::from(&cfg.search.unwrap().fusion),
            winner.to_params()
        );
    }

    /// `non_degradation`'s violation branch: a candidate can win on the primary
    /// metric while a secondary one falls below baseline, and that must block
    /// adoption. No test reached this — `"secondary metrics degraded"` appeared
    /// only in the source.
    #[test]
    fn test_decide_keeps_default_when_a_secondary_metric_degrades() {
        let cand = Condition {
            h: 3,
            ctx: 1,
            content: 1,
            k: Condition::builtin_default().k,
        };
        // Primary rises to ~0.50 against a 0.20 baseline; recall and MRR drop
        // under it. Adoption on the primary alone would be the bug.
        let (table, idx) = table_with_secondary_drop(cand, 0.05);
        let (ok, violations) = non_degradation(&table, cand, &idx);
        assert!(!ok, "a secondary drop must count as a violation");
        assert!(
            violations.iter().any(|v| v.starts_with("recall@")),
            "{violations:?}"
        );
        assert!(
            violations.iter().any(|v| v.starts_with("MRR")),
            "{violations:?}"
        );

        let outcome = nested_loo(&table, &idx);
        match decide(&table, &outcome, &idx) {
            Verdict::KeepDefault { reasons } => assert!(
                reasons
                    .iter()
                    .any(|r| r.contains("secondary metrics degraded")),
                "{reasons:?}"
            ),
            other => panic!("a secondary degradation must block adoption, got {other:?}"),
        }
    }

    #[test]
    fn test_normalize_k_values_always_includes_the_primary_k() {
        // 採用閾値は nDCG@PRIMARY_K を前提に較正されているので、主指標が
        // 落ちる k 指定を許してはならない。
        assert_eq!(normalize_k_values(&[1, 10]), vec![1, PRIMARY_K, 10]);
        assert_eq!(normalize_k_values(&[]), vec![PRIMARY_K]);
        // 昇順 + 重複除去
        assert_eq!(normalize_k_values(&[10, 5, 1, 5]), vec![1, 5, 10]);
        // k=0 は nDCG@0 が定義上 0.0 に潰れるだけなので落とす
        assert_eq!(normalize_k_values(&[0, 3]), vec![3, PRIMARY_K]);
        // 既に正規形ならそのまま
        assert_eq!(normalize_k_values(&[1, 5, 10]), vec![1, 5, 10]);
    }

    /// Regression (codex P2 round 2 on PR #79): public API (`tune::run`) 経由でも
    /// limit は max(k) 未満に縮まない。CLI resolver だけで clamp すると、library
    /// caller が `TuneOpts { k_values: vec![10], limit: 1 }` を渡した時に fused
    /// ranking が 1 件に切り詰められ、nDCG@5/@10 がラベルより浅い候補から
    /// 計算される切り詰めバグが再現する。
    #[test]
    fn test_effective_limit_clamps_to_max_k() {
        assert_eq!(effective_limit(&[1, 5, 10], 1).unwrap(), 10);
        assert_eq!(effective_limit(&[1, 5], 100).unwrap(), 100);
        assert_eq!(effective_limit(&[PRIMARY_K], 3).unwrap(), PRIMARY_K as u32);
    }

    /// Regression (codex P2 round 3/4 on PR #79): u32::MAX 超の k は as cast だと
    /// wrap して limit 0 (round 3)、saturate だと u32::MAX が下流の
    /// `Vec::with_capacity` に流れて allocation abort (round 4)。変換ではなく
    /// MAX_TUNE_K 超えとして reject する。
    #[test]
    fn test_effective_limit_rejects_oversized_k_and_limit() {
        assert!(effective_limit(&[usize::MAX], 1).is_err());
        assert!(effective_limit(&[MAX_TUNE_K + 1], 1).is_err());
        assert!(effective_limit(&[1, 5], MAX_TUNE_K as u32 + 1).is_err());
        // 上限ちょうどは通る
        assert_eq!(
            effective_limit(&[MAX_TUNE_K], 1).unwrap(),
            MAX_TUNE_K as u32
        );
    }

    #[test]
    fn test_toml_snippet_is_pasteable() {
        let c = Condition {
            h: 3,
            ctx: 0,
            content: 1,
            k: 1,
        };
        let s = toml_snippet(c);
        assert!(s.contains("[search.fusion]"), "{s}");
        // 整数値も小数点付きで出すこと (TOML の integer は f32 に deserialize できない)
        assert!(s.contains("rrf_k = 10.0"), "{s}");
        assert!(s.contains("bm25_heading_weight = 4.0"), "{s}");
        assert!(s.contains("bm25_context_weight = 0.5"), "{s}");
        assert!(s.contains("bm25_content_weight = 1.0"), "{s}");
        // 貼り付けたものが実際に Config としてパースでき、validate も通ること
        let cfg: crate::config::Config = toml::from_str(&s).expect("snippet must be valid toml");
        assert!(cfg.validate().is_ok(), "snippet must pass validate()");
        assert_eq!(
            crate::db::FusionParams::from(&cfg.search.unwrap().fusion),
            c.to_params()
        );
    }

    #[test]
    fn test_format_text_emits_all_seven_required_sections() {
        let report = synthetic_report();
        let out = format_text(&report, false);
        for needle in [
            "Nested leave-one-query-out CV",
            "paired SE",
            "sign test",
            "selection stability",
            "Secondary metrics",
            "Per-query impact",
            "Query diagnostics",
            "candidate pool",
            "Verdict",
        ] {
            assert!(out.contains(needle), "missing {needle:?} in:\n{out}");
        }
    }

    #[test]
    fn test_format_text_keeps_default_verdict_without_snippet() {
        // default 維持のときは toml スニペットを出さない (誤って貼られるのを防ぐ)。
        let report = synthetic_report();
        assert!(matches!(report.verdict, Verdict::KeepDefault { .. }));
        let out = format_text(&report, false);
        assert!(
            !out.contains("[search.fusion]"),
            "keep-default output must not print a paste-ready snippet:\n{out}"
        );
        assert!(out.contains("keep the built-in defaults"), "{out}");
    }

    #[test]
    fn test_format_json_round_trips() {
        let report = synthetic_report();
        let v = format_json(&report);
        assert_eq!(v["effective_query_count"].as_u64(), Some(3));
        assert_eq!(v["verdict"]["decision"].as_str(), Some("keep_default"));
        assert!(v["diagnostics"].as_array().is_some());
        assert!(v["loo"]["paired_se"].is_number());
        // `serde_json` は NaN / Inf を Null に落とすので、to_string 自体は
        // 混入していても成功する (= これは混入検知ではない)。paired_se が
        // number であることの assert が実質の検知で、無限大は format_json 側の
        // `is_finite()` ガードで明示的に Null になる。ここでは「生成した
        // Value がそのまま文字列化できる」ことだけを確認する。
        serde_json::to_string(&v).expect("report JSON must serialize");
    }

    #[test]
    fn test_format_json_nulls_out_a_non_finite_paired_se() {
        // 実効 N=1 では paired_se が INFINITY になる。JSON には Null で出す。
        let table = flat_table(1, 0.4);
        let idx = vec![0_usize];
        let outcome = nested_loo(&table, &idx);
        assert!(paired_se(&outcome.diffs).is_infinite());

        let mut report = synthetic_report();
        report.effective = idx;
        report.outcome = outcome;
        let v = format_json(&report);
        assert!(
            v["loo"]["paired_se"].is_null(),
            "an infinite SE must serialize as null, got {}",
            v["loo"]["paired_se"]
        );
    }

    #[test]
    fn test_context_axis_noop_is_detected_and_surfaced() {
        // dogfood KB は contextual retrieval が off で context 列が全て空だった。
        // その状態では bm25_context_weight 軸が構造的に測定不能なので、
        // 「context 重みは効かなかった」と誤読されないよう診断として出す。
        let db = tune_db();
        add_doc(&db, "a.md", "A", "zebrafish larvae in assays", 0.1);
        assert!(
            context_axis_is_noop(&db).unwrap(),
            "a KB indexed without contextual retrieval must report the axis as a no-op"
        );

        // context を持つ chunk が 1 件でもあれば軸は生きている
        let doc = db
            .upsert_document("b.md", Some("B"), None, None, None, &[], None, "hb", 0)
            .unwrap();
        db.insert_chunk(
            doc,
            0,
            Some("B"),
            None,
            "body prose",
            Some("surrounding document context"),
            &emb(0.2),
            1.0,
        )
        .unwrap();
        assert!(!context_axis_is_noop(&db).unwrap());

        // text / JSON の双方に載ること
        let mut report = synthetic_report();
        report.context_axis_noop = true;
        let out = format_text(&report, false);
        assert!(
            out.contains("bm25_context_weight"),
            "the text report must warn about the dead context axis:\n{out}"
        );
        assert_eq!(
            format_json(&report)["context_axis_noop"].as_bool(),
            Some(true)
        );
    }
}
