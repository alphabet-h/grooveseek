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
}
