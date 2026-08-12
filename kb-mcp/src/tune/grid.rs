//! The parameter space `kb-mcp tune` sweeps, and the per-query state it
//! carries through a sweep.
//!
//! Split out of `tune.rs` in AU-31. Contents are byte-identical; only
//! visibility widened where the parent already reached in.

use super::*;

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
    /// クエリがマッチする chunk が総数の半分以上 = FTS5 の IDF クランプ域。
    ///
    /// feature-48 以降、この値は **OR 集合の和集合**の大きさから決まる。FTS5 は
    /// IDF を phrase ごとに計算してクランプするので、これは個々の phrase の
    /// doc-freq の**上界**でしかない: `false` なら「どの phrase もクランプされて
    /// いない」の健全な証拠だが、`true` は「互いに素な希少 phrase が積み上がって
    /// 和集合が半分を超えた」だけの場合もある (どれもクランプされておらず重みは効く)。
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
