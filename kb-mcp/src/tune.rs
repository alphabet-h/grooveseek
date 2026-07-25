//! `kb-mcp tune` — fusion パラメータ (RRF k / FTS5 bm25 列重み) の測定ツール。
//!
//! golden query セットに対して grid search を実行し、その KB における
//! fusion パラメータの効き方を **統計的ガード付きで** レポートする。
//! **何も自動では適用しない** — 出力は toml に貼れる推奨スニペットと、
//! 「default 維持を推奨」という結論のどちらかである。
//!
//! 設計の要点 (spec feature-47 D-8〜D-11):
//! - FTS は `sanitize_fts_query` がクエリ全体を単一 phrase 化するため実質
//!   verbatim 部分文字列検索であり、**query が逐語で 2 件以上の chunk に
//!   出現する場合にしか fusion パラメータは効かない**。効く query 数を
//!   「実効 N」として先に測り、0 なら掃引せず exit 2 で終わる
//! - grid は「query embedding 一括 → vec 候補 query あたり 1 回 → FTS 候補
//!   bm25 条件ごと 1 回 → rrf_k はメモリ内」の 4 層に因数分解される
//! - 小 golden set の argmax は overfit するので、nested leave-one-query-out
//!   CV + paired SE + sign test + selection stability で採否を判定する

use anyhow::Result;
use std::collections::HashMap;
use std::path::PathBuf;

use crate::db::{Database, FusionParams, SearchFilters};
use crate::embedder::ModelChoice;
use crate::eval::{ExpectedHit, GoldenSet, HitRecord};

// ---------------------------------------------------------------------------
// Grid (D-9)
// ---------------------------------------------------------------------------

/// RRF 定数項の探索空間 (対数グリッド)。
///
/// 60 より下を厚く取ってある: pool 下限 50 では k=60 の重み比が最大 1.8 倍
/// しかなく上げ方向はほぼ無風なのに対し、k を ~8 以下へ下げると「片方の
/// 検索器が確信を持って 1 位に出した文書」が合意組を逆転し始めるため。
pub const RRF_K_GRID: [f32; 6] = [5.0, 10.0, 20.0, 30.0, 60.0, 100.0];

/// bm25 列重みの探索空間 (対数グリッド)。
///
/// weight は tf 側に入って飽和関数を通るため、2 倍にしてもスコアは 2 倍に
/// ならず逓減する。したがって線形グリッドは無駄が多い。
pub const BM25_WEIGHT_GRID: [f32; 4] = [0.5, 1.0, 2.0, 4.0];

/// 重み 3 軸 × rrf_k の全条件数。
pub const TOTAL_CONDITIONS: usize =
    BM25_WEIGHT_GRID.len() * BM25_WEIGHT_GRID.len() * BM25_WEIGHT_GRID.len() * RRF_K_GRID.len();

/// bm25 重みの組み合わせ数 (= SQL 往復が必要な条件数)。
pub const WEIGHT_CONDITIONS: usize =
    BM25_WEIGHT_GRID.len() * BM25_WEIGHT_GRID.len() * BM25_WEIGHT_GRID.len();

/// grid 上の 1 条件。各フィールドは対応するグリッド配列の添字。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Condition {
    pub h: usize,
    pub ctx: usize,
    pub content: usize,
    pub k: usize,
}

impl Condition {
    /// ビルトイン既定条件 (heading 2.0 / context 1.0 / content 1.0 / k 60)。
    /// すべての差分はこの条件を基準に測る。
    pub fn builtin_default() -> Self {
        Self {
            h: 2,       // BM25_WEIGHT_GRID[2] == 2.0
            ctx: 1,     // == 1.0
            content: 1, // == 1.0
            k: 4,       // RRF_K_GRID[4] == 60.0
        }
    }

    pub fn to_params(self) -> FusionParams {
        FusionParams {
            rrf_k: RRF_K_GRID[self.k],
            bm25_heading_weight: BM25_WEIGHT_GRID[self.h],
            bm25_context_weight: BM25_WEIGHT_GRID[self.ctx],
            bm25_content_weight: BM25_WEIGHT_GRID[self.content],
        }
    }

    /// `MetricTable` の列添字。重み組を上位、rrf_k を下位に置く
    /// (= 同じ FTS 結果を共有する 6 条件が連続する)。
    pub fn index(self) -> usize {
        self.weight_index() * RRF_K_GRID.len() + self.k
    }

    /// 重み組だけの添字 (0..WEIGHT_CONDITIONS)。SQL 往復の単位。
    pub fn weight_index(self) -> usize {
        (self.h * BM25_WEIGHT_GRID.len() + self.ctx) * BM25_WEIGHT_GRID.len() + self.content
    }

    /// 全条件を grid 順に列挙する。
    pub fn all() -> impl Iterator<Item = Condition> {
        (0..BM25_WEIGHT_GRID.len()).flat_map(|h| {
            (0..BM25_WEIGHT_GRID.len()).flat_map(move |ctx| {
                (0..BM25_WEIGHT_GRID.len()).flat_map(move |content| {
                    (0..RRF_K_GRID.len()).map(move |k| Condition { h, ctx, content, k })
                })
            })
        })
    }

    /// 人が読む形。`h=4.0 ctx=1.0 content=0.5 k=10` のように出す。
    pub fn label(self) -> String {
        let p = self.to_params();
        format!(
            "h={:.1} ctx={:.1} content={:.1} k={:.0}",
            p.bm25_heading_weight, p.bm25_context_weight, p.bm25_content_weight, p.rrf_k
        )
    }
}

// ---------------------------------------------------------------------------
// Options / per-query state
// ---------------------------------------------------------------------------

pub struct TuneOpts {
    pub kb_path: PathBuf,
    pub golden_path: PathBuf,
    pub model_choice: ModelChoice,
    /// 報告する k のリスト。`primary_k` (=5) は常に含まれる。
    pub k_values: Vec<usize>,
    /// 1 query あたりの取得件数。production の `run_search_pipeline`
    /// (MMR off / reranker off) と同じく pool は `limit*5 max 50`。
    pub limit: u32,
}

/// 採否判定の主指標に使う k。閾値 0.02 はこの k を前提に較正されている。
pub const PRIMARY_K: usize = 5;

/// chunk_id → metric 計算に必要な最小限のメタデータ。全条件で共有し、
/// `SearchResult` (content 文字列込み) は条件ごとに即捨てる。
/// これをやらないと 384 条件 × N query × pool 50 件の content が
/// メモリに載る。
#[derive(Debug, Clone)]
pub struct HitMeta {
    pub path: String,
    pub heading: Option<String>,
}

/// query 単位の診断値 (D-11-5 / D-11-6 / E-7)。
#[derive(Debug, Clone, Default)]
pub struct QueryDiagnostics {
    /// 既定重みでの FTS 候補数 (pool 内)。実効 query の判定に使う。
    pub fts_candidates: usize,
    /// phrase 全体の doc-freq (LIMIT なし)。IDF クランプ診断に使う。
    pub fts_total_matches: i64,
    /// vec pool と FTS list の重複 chunk 数。**0 なら全スコアが単項
    /// `1/(k+r+1)` になり順位が rrf_k 不変** = rrf_k 軸が測定不能。
    pub vec_fts_overlap: usize,
    /// grid 端の重み (heading 偏重 vs content 偏重) で FTS 順位が変わったか。
    pub bm25_sensitive: bool,
    /// phrase doc-freq が chunk 総数の半分以上 = IDF が 1e-6 に潰れている。
    pub idf_clamped: bool,
    /// 参考出力: 既定条件の融合結果に現れた f32 同点の隣接ペア数 (E-7)。
    pub rrf_ties: usize,
}

impl QueryDiagnostics {
    /// 実効 query の定義 (E-6): FTS 候補 >= 2 件。
    /// 0 件なら vec-only fallback、1 件なら rank 固定で bm25 重みが不感。
    pub fn is_effective(&self) -> bool {
        self.fts_candidates >= 2
    }
}

/// grid 掃引の入力になる、query あたりの前処理済み状態。
#[derive(Debug)]
pub struct PreparedQuery {
    pub id: String,
    pub query: String,
    pub expected: Vec<ExpectedHit>,
    /// vec 側の候補 rank list。fusion パラメータに不依存なので 1 回だけ取る。
    pub vec_ids: Vec<i64>,
    pub diag: QueryDiagnostics,
}

#[derive(Debug)]
pub struct Preflight {
    pub queries: Vec<PreparedQuery>,
    /// `queries` の添字のうち実効なもの。
    pub effective: Vec<usize>,
    /// index 内の chunk 総数 (IDF クランプ診断の分母)。
    pub chunk_total: u32,
    /// 実際に使った候補プールサイズ (E-8: floor であって cap ではない)。
    pub pool_size: u32,
}

// ---------------------------------------------------------------------------
// Pre-flight (D-8)
// ---------------------------------------------------------------------------

/// metric に寄与する golden query (= `expected` が非空) を golden 記載順で返す。
///
/// `eval::aggregate_metrics` が expected 空の query を平均から外すのと同じ
/// 扱いを、tune では入口で 1 回だけ適用する。`preflight_from_embeddings` に
/// 渡す embedding の順序・件数はこの関数の結果と一致していなければならない。
pub fn usable_queries(golden: &GoldenSet) -> Vec<&crate::eval::GoldenQuery> {
    golden
        .queries
        .iter()
        .filter(|q| {
            if q.expected.is_empty() {
                tracing::warn!(query = %q.query, "skipping golden query with no expected hits");
                false
            } else {
                true
            }
        })
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

    let usable = usable_queries(golden);
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
// Statistics (D-11)
// ---------------------------------------------------------------------------

/// 採用に要求する held-out 平均改善の下限 (nDCG@5)。
/// RRF 原論文の実測 (k∈[30,100] で MAP 相対 0.4%) を踏まえ、
/// この程度の差が出なければ measurement noise と見なす。
pub const ADOPT_MIN_MEAN_DELTA: f64 = 0.02;

/// 採用に要求する selection stability の下限 (過半数)。
/// fold 間で勝者が割れるのは過学習の最も直接的な兆候。
pub const STABILITY_MIN: f64 = 0.5;

/// これ未満の実効 N は IR 慣行の下限未満として stderr に警告する。
pub const SMALL_N_WARN: usize = 50;

/// 標本平均。空なら 0.0。
pub fn mean(xs: &[f64]) -> f64 {
    if xs.is_empty() {
        return 0.0;
    }
    xs.iter().sum::<f64>() / xs.len() as f64
}

/// 不偏標本標準偏差 (分母 n-1)。n < 2 なら 0.0。
pub fn sample_sd(xs: &[f64]) -> f64 {
    let n = xs.len();
    if n < 2 {
        return 0.0;
    }
    let m = mean(xs);
    let var = xs.iter().map(|x| (x - m) * (x - m)).sum::<f64>() / (n as f64 - 1.0);
    var.sqrt()
}

/// paired per-query 差分から求める標準誤差 `SD({d_j}) / sqrt(N)` (D-11-2)。
///
/// **fold 平均の分散を SE と呼んではならない** — fold 平均は d_j のアフィン
/// 変換なので SD が 1/(N−1) に縮み、有意判定が壊れる。fold が query を共有
/// するため厳密には i.i.d. でない近似値だが、保守側 (過小評価しない) に働く。
/// N < 2 は判定不能として無限大を返し、採用条件を必ず落とす。
pub fn paired_se(diffs: &[f64]) -> f64 {
    if diffs.len() < 2 {
        return f64::INFINITY;
    }
    sample_sd(diffs) / (diffs.len() as f64).sqrt()
}

#[derive(Debug, Clone, PartialEq)]
pub struct SignTest {
    pub positive: usize,
    pub negative: usize,
    pub ties: usize,
    /// 帰無仮説 p=0.5 の二項分布による厳密両側 p 値。
    pub p_value: f64,
}

/// paired sign test (D-11-2 の参考情報)。同値は捨てる標準的な扱い。
pub fn sign_test(diffs: &[f64]) -> SignTest {
    const EPS: f64 = 1e-12;
    let positive = diffs.iter().filter(|d| **d > EPS).count();
    let negative = diffs.iter().filter(|d| **d < -EPS).count();
    let ties = diffs.len() - positive - negative;
    let n = positive + negative;
    if n == 0 {
        return SignTest {
            positive,
            negative,
            ties,
            p_value: 1.0,
        };
    }
    // 両側 p = 2 * P(X <= min(pos, neg)), X ~ Bin(n, 0.5)、1.0 でクランプ。
    let m = positive.min(negative);
    let mut tail = 0.0_f64;
    let mut coeff = 1.0_f64; // C(n, 0)
    for i in 0..=m {
        if i > 0 {
            coeff = coeff * ((n - i + 1) as f64) / (i as f64);
        }
        tail += coeff;
    }
    let p = (2.0 * tail / 2f64.powi(n as i32)).min(1.0);
    SignTest {
        positive,
        negative,
        ties,
        p_value: p,
    }
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
/// 3. `mean(d) > 2 * paired_se(d)`
/// 4. selection stability > `STABILITY_MIN` (過半数の fold が refit に一致)
/// 5. 全指標 (recall@k 各 k / MRR) が baseline を下回らない
pub fn decide(table: &MetricTable, outcome: &LooOutcome, all: &[usize]) -> Verdict {
    let mut reasons = Vec::new();
    let m = mean(&outcome.diffs);
    let se = paired_se(&outcome.diffs);

    if outcome.refit == Condition::builtin_default() {
        reasons.push("refit selected the built-in defaults".to_string());
    }
    if m <= ADOPT_MIN_MEAN_DELTA {
        reasons.push(format!(
            "held-out mean delta {m:+.4} <= threshold {ADOPT_MIN_MEAN_DELTA:.4}"
        ));
    }
    // `m > 2*se` の否定を `m <= 2*se` と書いてはならない: se は N<2 で
    // INFINITY、m は退化入力で NaN になり得るため、比較不能なケースを
    // 「採用しない」側 (= reason を積む) へ倒す必要がある。
    let two_se = 2.0 * se;
    if !matches!(m.partial_cmp(&two_se), Some(std::cmp::Ordering::Greater)) {
        reasons.push(format!(
            "held-out mean delta {m:+.4} <= 2 x paired SE {two_se:.4}"
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
            .upsert_document(path, Some(path), None, None, None, &[], None, path)
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

    #[test]
    fn test_count_fts_matches_counts_phrase_hits() {
        // D-11-6 の phrase doc-freq 診断の土台。
        let db = tune_db();
        add_doc(&db, "a.md", "Zebrafish", "zebrafish larvae in assays", 0.1);
        add_doc(&db, "b.md", "More", "the zebrafish larvae grow fast", 0.2);
        add_doc(&db, "c.md", "Other", "completely unrelated prose here", 0.3);

        assert_eq!(db.count_fts_matches("zebrafish larvae").unwrap(), 2);
        assert_eq!(db.count_fts_matches("unrelated prose").unwrap(), 1);
        assert_eq!(db.count_fts_matches("nonexistent phrase xyz").unwrap(), 0);
        // sanitize_fts_query の trigram 下限 (3 文字未満) は Ok(0) で早期 return
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
}
