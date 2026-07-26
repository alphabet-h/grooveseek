//! `kb-mcp eval` — retrieval quality evaluation subcommand.
//!
//! Opt-in パワーユーザ向け機能。Golden query YAML を読み、`db::search_hybrid`
//! で検索し、recall@k / MRR / nDCG@k を計算する。直前実行との diff を表示する。

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::path::{Path, PathBuf};

// ---------- Golden ----------

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GoldenSet {
    #[serde(default)]
    pub defaults: Option<GoldenDefaults>,
    pub queries: Vec<GoldenQuery>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GoldenDefaults {
    pub limit: Option<u32>,
    pub rerank: Option<bool>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GoldenQuery {
    pub id: Option<String>,
    pub query: String,
    pub expected: Vec<ExpectedHit>,
    #[serde(default)]
    pub tags: Option<Vec<String>>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ExpectedHit {
    pub path: String,
    #[serde(default)]
    pub heading: Option<String>,
}

impl GoldenSet {
    /// Golden YAML を読み込む。欠損時は hint 付きエラー。
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            anyhow::bail!(
                "no golden file at {} (hint: pass --golden or create <kb>/.kb-mcp-eval.yml)",
                path.display()
            );
        }
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read golden file: {}", path.display()))?;
        let gs: Self = serde_yaml_bw::from_str(&text)
            .with_context(|| format!("failed to parse golden file: {}", path.display()))?;
        Ok(gs)
    }

    /// Golden ファイルの生バイト列を sha256 ハッシュ化 (fingerprint 用)。
    pub fn hash_bytes(bytes: &[u8]) -> String {
        use sha2::{Digest, Sha256};
        let mut h = Sha256::new();
        h.update(bytes);
        format!("{:x}", h.finalize())
    }
}

// ---------- Metrics ----------

/// Heading 比較用の正規化: 前後空白 trim + 小文字化。
fn normalize_heading(s: &str) -> String {
    s.trim().to_lowercase()
}

/// ヒット判定: path は完全一致、heading は指定があれば正規化後一致。
pub fn is_hit(expected: &ExpectedHit, hit: &HitRecord) -> bool {
    if expected.path != hit.path {
        return false;
    }
    match (&expected.heading, &hit.heading) {
        (Some(e), Some(h)) => normalize_heading(e) == normalize_heading(h),
        (Some(_), None) => false,
        (None, _) => true,
    }
}

/// recall@k = |expected ∩ top[..k]| / |expected|。
/// expected 0 件または top 0 件では 0.0。
pub fn recall_at_k(expected: &[ExpectedHit], top: &[HitRecord], k: usize) -> f64 {
    if expected.is_empty() || top.is_empty() {
        return 0.0;
    }
    let window = top.iter().take(k);
    let mut matched = 0usize;
    for e in expected {
        if window.clone().any(|h| is_hit(e, h)) {
            matched += 1;
        }
    }
    matched as f64 / expected.len() as f64
}

/// MRR 用: 最初にヒットした expected の rank の逆数。無ければ 0.0。
/// rank は 1-origin を期待。万一 rank=0 が渡された場合は 0.0 を返し
/// 1.0/0.0 = inf 汚染を防ぐ (HitRecord は pub なので外部経路の防衛線として残す)。
pub fn reciprocal_rank(expected: &[ExpectedHit], top: &[HitRecord]) -> f64 {
    if expected.is_empty() || top.is_empty() {
        return 0.0;
    }
    for h in top {
        if expected.iter().any(|e| is_hit(e, h)) {
            if h.rank == 0 {
                tracing::warn!(
                    "reciprocal_rank: encountered HitRecord with rank=0 (must be 1-origin); returning 0.0 to avoid inf"
                );
                return 0.0;
            }
            return 1.0 / h.rank as f64;
        }
    }
    0.0
}

/// nDCG@k (binary relevance, value range [0, 1])。
///
/// DCG  = Σ_{e ∈ expected} 1 / log2(first_hit_rank(e) + 1)  (rank ≤ k に制限、無ければ寄与 0)
/// IDCG = Σ_{i=1..=min(|expected|, k)} 1 / log2(i + 1)
///
/// hit を rank 順に走査し、未消費の expected と 1:1 で貪欲マッチして gain を積む実装。
/// 1 hit = 最大 1 gain なので、同一 path の複数 chunk が top-k に並ぶケースに加え、
/// expected 側に同一 path が重複するケースや path-only expected と heading 指定
/// expected が同一 hit にマッチするケースでも DCG ≤ IDCG が保たれる
/// (i 番目にマッチした hit の rank は i 以上のため、gain は ideal の第 i 項を超えない)。
/// 同一 hit に複数 expected がマッチする場合は heading 指定側を優先消費し、
/// path-only expected を後続 hit に譲る (同順位内は expected の記載順)。
pub fn ndcg_at_k(expected: &[ExpectedHit], top: &[HitRecord], k: usize) -> f64 {
    if expected.is_empty() || top.is_empty() || k == 0 {
        return 0.0;
    }
    let mut consumed = vec![false; expected.len()];
    let mut dcg = 0.0;
    for h in top.iter().take(k) {
        let candidate = (0..expected.len())
            .filter(|&i| !consumed[i] && is_hit(&expected[i], h))
            .min_by_key(|&i| (expected[i].heading.is_none(), i));
        if let Some(i) = candidate {
            consumed[i] = true;
            dcg += 1.0 / ((h.rank as f64 + 1.0).log2());
        }
    }
    let ideal_count = expected.len().min(k);
    let idcg: f64 = (1..=ideal_count)
        .map(|i| 1.0 / ((i as f64 + 1.0).log2()))
        .sum();
    if idcg == 0.0 { 0.0 } else { dcg / idcg }
}

/// クエリ単位で recall@k / RR / nDCG@k をまとめて計算する。
pub fn compute_query_metrics(
    expected: &[ExpectedHit],
    top: &[HitRecord],
    k_values: &[usize],
) -> QueryMetrics {
    let mut recall_at_k_map = std::collections::BTreeMap::new();
    let mut ndcg_at_k_map = std::collections::BTreeMap::new();
    for &k in k_values {
        recall_at_k_map.insert(k, recall_at_k(expected, top, k));
        ndcg_at_k_map.insert(k, ndcg_at_k(expected, top, k));
    }
    QueryMetrics {
        recall_at_k: recall_at_k_map,
        reciprocal_rank: reciprocal_rank(expected, top),
        ndcg_at_k: ndcg_at_k_map,
    }
}

/// 全クエリにわたる平均を取る。expected 0 件のクエリはスキップする。
pub fn aggregate_metrics(per_query: &[QueryResult], k_values: &[usize]) -> AggregateMetrics {
    let valid: Vec<&QueryResult> = per_query
        .iter()
        .filter(|q| !q.expected.is_empty())
        .collect();
    let n = valid.len();
    if n == 0 {
        return AggregateMetrics::default();
    }
    let mut recall_at_k_map = std::collections::BTreeMap::new();
    let mut ndcg_at_k_map = std::collections::BTreeMap::new();
    for &k in k_values {
        let sum_r: f64 = valid
            .iter()
            .map(|q| q.metrics.recall_at_k.get(&k).copied().unwrap_or(0.0))
            .sum();
        let sum_n: f64 = valid
            .iter()
            .map(|q| q.metrics.ndcg_at_k.get(&k).copied().unwrap_or(0.0))
            .sum();
        recall_at_k_map.insert(k, sum_r / n as f64);
        ndcg_at_k_map.insert(k, sum_n / n as f64);
    }
    let mrr: f64 = valid.iter().map(|q| q.metrics.reciprocal_rank).sum::<f64>() / n as f64;
    AggregateMetrics {
        recall_at_k: recall_at_k_map,
        mrr,
        ndcg_at_k: ndcg_at_k_map,
        query_count: n,
    }
}

// ---------- Result ----------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvalRun {
    pub timestamp: DateTime<Utc>,
    pub fingerprint: ConfigFingerprint,
    pub per_query: Vec<QueryResult>,
    pub aggregate: AggregateMetrics,
}

/// 現行の metric 実装 version。recall / MRR / nDCG の計算式を修正するたびに
/// +1 する。[`ConfigFingerprint::metric_version`] を参照。
pub const METRIC_VERSION: u32 = 2;

fn legacy_metric_version() -> u32 {
    1
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ConfigFingerprint {
    pub model: String,
    pub reranker: Option<String>,
    pub limit: u32,
    pub k_values: Vec<usize>,
    pub golden_hash: String,

    /// Metric 実装の version。計算式の fix (例: v2 = nDCG の expected 重複
    /// 多重計上 fix) 前後で同じ検索結果から異なる数値が出得るため、旧 history
    /// JSON (field なし = serde default で 1) とは PartialEq 不一致になり
    /// [`History::previous_compatible`] の比較対象から自動的に外れる
    /// (= 意図的な式修正を `--fail-on-regression` が retrieval regression と
    /// 誤検出しない)。
    /// - v1: 初版〜v0.12.0 (expected ごとに first-hit gain を加算)
    /// - v2: hit 主導の 1:1 貪欲マッチ (expected 側重複の多重計上 fix)
    #[serde(default = "legacy_metric_version")]
    pub metric_version: u32,

    /// MMR が有効な場合のみ Some。off (default) なら None で旧 history JSON
    /// と互換維持。enabled=true でのみ lambda + same_doc_penalty を fingerprint
    /// に含めることで、MMR off の状態は古い baseline と直接比較可能。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mmr: Option<MmrFingerprint>,

    /// Parent retriever が有効な場合のみ Some。off (default) なら None で旧
    /// history JSON との互換維持。enabled=true でのみ
    /// `whole_doc_threshold_tokens` と `max_expanded_tokens` を fingerprint に
    /// 含める (これらが変われば baseline は別物として扱う)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_retriever: Option<ParentRetrieverFingerprint>,

    /// `[search.fusion]` がビルトイン既定値 (60 / 2.0 / 1.0 / 1.0) から
    /// 変更されている場合のみ Some。既定なら None で、feature-47 以前の
    /// history JSON / 凍結 baseline との `PartialEq` 互換を維持する
    /// (mmr / parent_retriever と同じ論理、feature-47 D-7 / E-10)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fusion: Option<FusionFingerprint>,
}

/// `[search.mmr]` の effective config を fingerprint に含めるための snapshot。
/// MMR が enabled=true のときだけ [`ConfigFingerprint::mmr`] に Some で入る。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MmrFingerprint {
    pub lambda: f32,
    pub same_doc_penalty: f32,
}

/// `[search.parent_retriever]` の effective config を fingerprint に含めるための
/// snapshot。Parent retriever が enabled=true のときだけ
/// [`ConfigFingerprint::parent_retriever`] に Some で入る。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ParentRetrieverFingerprint {
    pub whole_doc_threshold_tokens: u32,
    pub max_expanded_tokens: u32,
}

/// `[search.fusion]` の effective config を fingerprint に含めるための snapshot。
/// ビルトイン既定値から変更されているときだけ [`ConfigFingerprint::fusion`] に
/// Some で入る。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FusionFingerprint {
    pub rrf_k: f32,
    pub bm25_heading_weight: f32,
    pub bm25_context_weight: f32,
    pub bm25_content_weight: f32,
}

impl ConfigFingerprint {
    /// `kb-mcp.toml` Config と eval 実行時の引数から fingerprint を構築。
    /// MMR / Parent retriever が effective on なら `Some(_)` を作り、off なら
    /// `None` のままにすることで旧 history JSON (該当 field なし) と直接比較
    /// できる PartialEq を維持する。
    pub fn from_config(
        cfg: &crate::config::Config,
        model: String,
        reranker: Option<String>,
        limit: u32,
        k_values: Vec<usize>,
        golden_hash: String,
    ) -> Self {
        let mmr = cfg
            .search
            .as_ref()
            .filter(|s| s.mmr.enabled)
            .map(|s| MmrFingerprint {
                lambda: s.mmr.lambda,
                same_doc_penalty: s.mmr.same_doc_penalty,
            });
        let parent_retriever = cfg
            .search
            .as_ref()
            .filter(|s| s.parent_retriever.enabled)
            .map(|s| ParentRetrieverFingerprint {
                whole_doc_threshold_tokens: s.parent_retriever.whole_doc_threshold_tokens,
                max_expanded_tokens: s.parent_retriever.max_expanded_tokens,
            });
        let fusion = cfg
            .search
            .as_ref()
            .filter(|s| !s.fusion.is_builtin_default())
            .map(|s| FusionFingerprint {
                rrf_k: s.fusion.rrf_k,
                bm25_heading_weight: s.fusion.bm25_heading_weight,
                bm25_context_weight: s.fusion.bm25_context_weight,
                bm25_content_weight: s.fusion.bm25_content_weight,
            });
        Self {
            model,
            reranker,
            limit,
            k_values,
            golden_hash,
            metric_version: METRIC_VERSION,
            mmr,
            parent_retriever,
            fusion,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueryResult {
    pub id: String,
    pub query: String,
    pub expected: Vec<ExpectedHit>,
    pub top_k: Vec<HitRecord>,
    pub metrics: QueryMetrics,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HitRecord {
    pub rank: usize,
    pub path: String,
    pub heading: Option<String>,
    pub score: f32,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct QueryMetrics {
    /// k -> recall
    pub recall_at_k: std::collections::BTreeMap<usize, f64>,
    pub reciprocal_rank: f64,
    /// k -> nDCG
    pub ndcg_at_k: std::collections::BTreeMap<usize, f64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AggregateMetrics {
    pub recall_at_k: std::collections::BTreeMap<usize, f64>,
    pub mrr: f64,
    pub ndcg_at_k: std::collections::BTreeMap<usize, f64>,
    pub query_count: usize,
}

// ---------- History ----------

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct History {
    pub runs: VecDeque<EvalRun>,
}

impl History {
    /// JSON ファイルから履歴を読む。不在・破損時は warn を出して空 History を返す。
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let bytes = match std::fs::read(path) {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!("failed to read eval history {}: {}", path.display(), e);
                return Ok(Self::default());
            }
        };
        match serde_json::from_slice::<Self>(&bytes) {
            Ok(h) => Ok(h),
            Err(e) => {
                tracing::warn!("eval history corrupted ({}), starting fresh", e);
                Ok(Self::default())
            }
        }
    }

    /// 最新の run を front に積み、`size` 件を超えたら末尾を切り落とす。
    pub fn push_front(&mut self, run: EvalRun, size: usize) {
        self.runs.push_front(run);
        while self.runs.len() > size {
            self.runs.pop_back();
        }
    }

    /// 直前の run (= front) を取得する。
    pub fn previous(&self) -> Option<&EvalRun> {
        self.runs.front()
    }

    /// 直前の run のうち、`fingerprint` が `now` と互換なものを返す。
    /// `is_regression` の前提として「同じ条件 (model / reranker / k_values
    /// / golden_hash 等) で取った数値だけ比較する」ことを保証するための
    /// helper。fingerprint が違えば apple-to-orange 比較になるので
    /// regression 判定対象外。
    pub fn previous_compatible(&self, now: &EvalRun) -> Option<&EvalRun> {
        self.runs
            .front()
            .filter(|p| p.fingerprint == now.fingerprint)
    }

    /// atomic rename で書き出す。
    pub fn save(&self, path: &Path) -> Result<()> {
        let bytes = serde_json::to_vec_pretty(self).context("failed to serialize eval history")?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, &bytes)
            .with_context(|| format!("failed to write temp history: {}", tmp.display()))?;
        std::fs::rename(&tmp, path).with_context(|| {
            format!(
                "failed to rename temp history into place: {}",
                path.display()
            )
        })?;
        Ok(())
    }
}

// ---------- Regression detection ----------

/// retrieval 品質が直前 run から退化したか判定する。F-40 で `kb-mcp eval
/// --fail-on-regression` を CI に組み込めるようにするための core ロジック。
///
/// 「退化」の定義: 集計指標 (recall@k 各 k / MRR / nDCG@k 各 k) のうち
/// **少なくとも 1 つ** が `prev_v - now_v > threshold` を満たすこと。
/// 改善は当然 false。同値や僅かな低下 (threshold 内) も false。
///
/// 値が NaN/Inf の混入経路は v0.4.3 以降ガード済 (proptest invariants で
/// `[0.0, 1.0]` 固定) だが、保険として `prev` 側で NaN/Inf を含む場合は
/// 「比較不能」とみなして false (= 安全側、CI を fail にしない) を返す。
///
/// `now` と `prev` は **fingerprint が一致** していることを呼び出し側で
/// 確認済の前提 ([`History::previous_compatible`] を参照)。fingerprint
/// 違いで誤検出を起こさないための分業。
pub fn is_regression(now: &EvalRun, prev: &EvalRun, threshold: f64) -> bool {
    // recall@k: 各 k で比較
    for (k, now_v) in &now.aggregate.recall_at_k {
        let prev_v = prev.aggregate.recall_at_k.get(k).copied().unwrap_or(0.0);
        if !prev_v.is_finite() || !now_v.is_finite() {
            continue;
        }
        if prev_v - now_v > threshold {
            return true;
        }
    }

    // MRR
    let (now_mrr, prev_mrr) = (now.aggregate.mrr, prev.aggregate.mrr);
    if now_mrr.is_finite() && prev_mrr.is_finite() && prev_mrr - now_mrr > threshold {
        return true;
    }

    // nDCG@k: 各 k で比較
    for (k, now_v) in &now.aggregate.ndcg_at_k {
        let prev_v = prev.aggregate.ndcg_at_k.get(k).copied().unwrap_or(0.0);
        if !prev_v.is_finite() || !now_v.is_finite() {
            continue;
        }
        if prev_v - now_v > threshold {
            return true;
        }
    }

    false
}

// ---------- Options ----------

pub struct RunOpts {
    pub kb_path: PathBuf,
    pub golden_path: PathBuf,
    pub model_choice: crate::embedder::ModelChoice,
    pub reranker_choice: crate::embedder::RerankerChoice,
    pub k_values: Vec<usize>,
    pub limit: u32,
    pub write_history: bool,
    pub history_size: usize,
    pub regression_threshold: f64,
    /// per-call overrides (CLI `--mmr` / `--mmr-lambda` etc).
    /// CLI builds this from `EvalCliArgs`. Programmatic callers can pass
    /// `SearchOverrides::default()` to get toml-only behavior.
    pub overrides: crate::config::SearchOverrides,
    /// `[search]` toml section snapshot. Combined with `overrides` to
    /// resolve the effective MMR / parent_retriever config per query.
    /// Programmatic callers can pass `SearchConfig::default()` to get
    /// MMR-off behavior.
    pub search_config: crate::config::SearchConfig,
}

// ---------- Formatters ----------

/// JSON 形式で 1 run を整形する。`previous` が渡され fingerprint 互換なら `diff` を付ける。
pub fn format_json(run: &EvalRun, previous: Option<&EvalRun>) -> serde_json::Value {
    // serde_json は f64 の Inf / NaN をシリアライズできず Err を返す。過去 history に
    // それらが混入していた場合に panic するのを避け、null に倒す。
    let prev_val = previous
        .and_then(|p| serde_json::to_value(p).ok())
        .unwrap_or(serde_json::Value::Null);
    // 表示 diff も full fingerprint 互換でのみ有効化する (golden_hash 単独だと
    // metric_version / model 等が違う旧 run との apple-to-orange 差分を出してしまう)。
    let diff_val = match previous {
        Some(p) if p.fingerprint == run.fingerprint => {
            let mut recall_diff = serde_json::Map::new();
            for (k, v) in &run.aggregate.recall_at_k {
                let prev_v = p.aggregate.recall_at_k.get(k).copied().unwrap_or(0.0);
                recall_diff.insert(k.to_string(), serde_json::json!(v - prev_v));
            }
            let mut ndcg_diff = serde_json::Map::new();
            for (k, v) in &run.aggregate.ndcg_at_k {
                let prev_v = p.aggregate.ndcg_at_k.get(k).copied().unwrap_or(0.0);
                ndcg_diff.insert(k.to_string(), serde_json::json!(v - prev_v));
            }
            serde_json::json!({
                "recall_at_k": recall_diff,
                "ndcg_at_k": ndcg_diff,
                "mrr": run.aggregate.mrr - p.aggregate.mrr,
            })
        }
        _ => serde_json::Value::Null,
    };
    serde_json::json!({
        "timestamp": run.timestamp,
        "fingerprint": run.fingerprint,
        "aggregate": run.aggregate,
        "per_query": run.per_query,
        "previous": prev_val,
        "diff": diff_val,
    })
}

/// Text 形式の出力。`use_color=true` のとき ANSI で色付けする。
/// TTY 検出は呼び出し側 (main.rs) で行う。
pub fn format_text(
    run: &EvalRun,
    previous: Option<&EvalRun>,
    use_color: bool,
    regression_threshold: f64,
) -> String {
    use std::fmt::Write;
    let mut s = String::new();
    writeln!(s, "kb-mcp eval — {}", run.timestamp.to_rfc3339()).unwrap();
    let rr = run.fingerprint.reranker.as_deref().unwrap_or("none");
    writeln!(
        s,
        "  model: {}    reranker: {}    limit: {}    queries: {}",
        run.fingerprint.model, rr, run.fingerprint.limit, run.aggregate.query_count
    )
    .unwrap();
    writeln!(s).unwrap();

    // Fingerprint mismatch は diff を無効化 (golden_hash 単独ではなく full 比較。
    // metric_version / model 等が違う旧 run との表示 diff も apple-to-orange)
    let diff_enabled = match previous {
        Some(p) => p.fingerprint == run.fingerprint,
        None => false,
    };

    match previous {
        Some(p) if diff_enabled => {
            writeln!(s, "Aggregate (previous run: {})", p.timestamp.to_rfc3339()).unwrap();
        }
        Some(p) if p.fingerprint.golden_hash != run.fingerprint.golden_hash => {
            writeln!(s, "⚠️ golden changed since last run, diff disabled").unwrap();
            writeln!(s, "Aggregate").unwrap();
        }
        Some(_) => {
            writeln!(
                s,
                "⚠️ config or metric version changed since last run, diff disabled"
            )
            .unwrap();
            writeln!(s, "Aggregate").unwrap();
        }
        None => {
            writeln!(s, "Aggregate").unwrap();
        }
    }

    // recall@k
    for k in &run.fingerprint.k_values {
        let v = run.aggregate.recall_at_k.get(k).copied().unwrap_or(0.0);
        let label = format!("recall@{k}");
        let diff = if diff_enabled {
            previous.map(|p| v - p.aggregate.recall_at_k.get(k).copied().unwrap_or(0.0))
        } else {
            None
        };
        writeln!(
            s,
            "  {:<11}{:.3}{}",
            label,
            v,
            render_diff(diff, regression_threshold, use_color)
        )
        .unwrap();
    }
    // MRR
    let mrr = run.aggregate.mrr;
    let mrr_diff = if diff_enabled {
        previous.map(|p| mrr - p.aggregate.mrr)
    } else {
        None
    };
    writeln!(
        s,
        "  {:<11}{:.3}{}",
        "MRR",
        mrr,
        render_diff(mrr_diff, regression_threshold, use_color)
    )
    .unwrap();
    // nDCG@k (最大 k のみ表示)
    if let Some(&kmax) = run.fingerprint.k_values.iter().max() {
        let v = run.aggregate.ndcg_at_k.get(&kmax).copied().unwrap_or(0.0);
        let label = format!("nDCG@{kmax}");
        let diff = if diff_enabled {
            previous.map(|p| v - p.aggregate.ndcg_at_k.get(&kmax).copied().unwrap_or(0.0))
        } else {
            None
        };
        writeln!(
            s,
            "  {:<11}{:.3}{}",
            label,
            v,
            render_diff(diff, regression_threshold, use_color)
        )
        .unwrap();
    }

    // Per-query: regression / miss のみ表示
    let mut rows: Vec<String> = Vec::new();
    let kmax = run.fingerprint.k_values.iter().max().copied().unwrap_or(10);
    for q in &run.per_query {
        let now_r = q.metrics.recall_at_k.get(&kmax).copied().unwrap_or(0.0);
        let prev_r = if diff_enabled {
            previous
                .and_then(|p| p.per_query.iter().find(|pq| pq.id == q.id))
                .map(|pq| pq.metrics.recall_at_k.get(&kmax).copied().unwrap_or(0.0))
        } else {
            None
        };
        let is_miss = q.expected.is_empty() || now_r == 0.0;
        let regressed = prev_r.is_some_and(|pr| pr - now_r > regression_threshold);
        if is_miss || regressed {
            let arrow = if is_miss && now_r == 0.0 {
                "✗"
            } else if regressed {
                "↓"
            } else {
                "·"
            };
            let prefix = if let Some(pr) = prev_r {
                format!("{:.2} → {:.2}", pr, now_r)
            } else {
                format!("{:.2}", now_r)
            };
            rows.push(format!(
                "  {} {:<24} recall@{kmax}: {}",
                arrow, q.id, prefix
            ));
        }
    }
    if !rows.is_empty() {
        writeln!(s).unwrap();
        writeln!(
            s,
            "Per-query (regressions and misses, {} of {})",
            rows.len(),
            run.per_query.len()
        )
        .unwrap();
        for r in rows {
            writeln!(s, "{}", r).unwrap();
        }
    }

    s
}

fn render_diff(diff: Option<f64>, threshold: f64, use_color: bool) -> String {
    match diff {
        None => String::new(),
        Some(d) if d.abs() < 1e-9 => format!("  (— {:>6})", ""),
        Some(d) => {
            let arrow = if d > 0.0 { "↑" } else { "↓" };
            let color = if !use_color {
                ""
            } else if d < -threshold {
                "\x1b[31m" // red
            } else if d > threshold {
                "\x1b[32m" // green
            } else {
                "\x1b[90m" // gray
            };
            let reset = if use_color { "\x1b[0m" } else { "" };
            format!("  ({}{} {:.3}{})", color, arrow, d.abs(), reset)
        }
    }
}

// ---------- Orchestration ----------

/// Default path for the history file: `<kb_path>/.kb-mcp-eval-history.json`.
pub fn default_history_path(kb_path: &Path) -> PathBuf {
    kb_path.join(".kb-mcp-eval-history.json")
}

/// Golden を読み、search_hybrid で評価し、EvalRun を返す。履歴書き込みは呼び出し側責務。
/// eval が実際に使う `limit` と報告する `k` のリストを、検索経路の実効値に
/// 揃える。
///
/// `run_search_pipeline` は `limit` を [`crate::server::SEARCH_LIMIT_MAX`] に
/// clamp するので、eval のメタデータだけ生の値を持つと (codex P2 on PR #81):
///
/// - `--limit 1001` と `--limit 2000` は同一の retrieval なのに fingerprint が
///   異なり、history の比較対象から外れる
/// - 上限を超える `k` について「`@k` と表示されているが実際は上限件数から
///   計算した」metric が無警告で出る
pub fn normalize_eval_limit_and_k(limit: u32, k_values: &[usize]) -> (u32, Vec<usize>) {
    let limit = crate::server::clamp_search_limit(limit);
    let k_values = k_values.iter().map(|k| (*k).min(limit as usize)).collect();
    (limit, k_values)
}

pub fn run(opts: &RunOpts) -> Result<EvalRun> {
    let golden_bytes = std::fs::read(&opts.golden_path)
        .with_context(|| format!("failed to read golden file: {}", opts.golden_path.display()))?;
    let gs: GoldenSet = serde_yaml_bw::from_slice(&golden_bytes).with_context(|| {
        format!(
            "failed to parse golden file: {}",
            opts.golden_path.display()
        )
    })?;
    let golden_hash = GoldenSet::hash_bytes(&golden_bytes);

    let db_path = crate::resolve_db_path(&opts.kb_path);
    if !db_path.exists() {
        anyhow::bail!(
            "No index found at {}. Run `kb-mcp index --kb-path {}` first.",
            db_path.display(),
            opts.kb_path.display()
        );
    }
    let db = crate::db::Database::open(&db_path.to_string_lossy())?;
    db.verify_embedding_meta(
        opts.model_choice.model_id(),
        opts.model_choice.dimension() as u32,
    )?;
    let mut embedder = crate::embedder::Embedder::with_model(opts.model_choice)?;
    let mut reranker = if opts.reranker_choice.is_enabled() {
        crate::embedder::Reranker::try_new(opts.reranker_choice)?
    } else {
        None
    };

    let (limit, k_values) = normalize_eval_limit_and_k(opts.limit, &opts.k_values);
    if limit != opts.limit {
        eprintln!(
            "kb-mcp eval: note — limit {} exceeds the retrieval cap; using {limit}",
            opts.limit
        );
    }
    let max_k = k_values
        .iter()
        .copied()
        .max()
        .unwrap_or(10)
        .max(limit as usize);
    let mut per_query = Vec::with_capacity(gs.queries.len());
    for q in &gs.queries {
        let qid =
            q.id.clone()
                .unwrap_or_else(|| q.query.chars().take(32).collect());
        let qe = embedder.embed_single(&q.query)?;
        // Eval shares the MMR-aware pipeline with MCP / CLI search so the
        // golden YAML reflects the actual production retrieval (e.g. when
        // `[search.mmr] enabled = true`).
        let pipeline = crate::server::run_search_pipeline(
            &db,
            reranker.as_mut(),
            &q.query,
            &qe,
            max_k as u32,
            &crate::db::SearchFilters::default(),
            &opts.overrides,
            &opts.search_config,
        )?;

        // chunk_id を維持したまま SearchHit に変換し、Parent retriever 段を
        // 適用する (enabled = false なら content / expanded_from は触らない)。
        // eval は match_spans を計算しないので、Parent retriever 後の content
        // のみ使う。HitRecord は path / heading / score / rank しか見ないため
        // 表示拡張された content / expanded_from は読み捨てるが、retrieval
        // pipeline 全段を実本番と揃えることで「eval 上は良いが production で
        // parent enabled にすると挙動が変わる」を防ぐ。
        let hits_with_id: Vec<(i64, crate::db::SearchHit)> = pipeline
            .into_iter()
            .map(|(id, sr)| (id, sr.into()))
            .collect();
        let resolved = opts.overrides.resolve(&opts.search_config);
        let parent_params = crate::parent::ParentRetrieverParams {
            whole_doc_threshold_tokens: resolved.parent_whole_doc_threshold_tokens,
            max_expanded_tokens: resolved.parent_max_expanded_tokens,
        };
        let hits: Vec<crate::db::SearchHit> = crate::parent::apply_parent_retriever(
            hits_with_id,
            &db,
            resolved.parent_retriever_enabled,
            parent_params,
        );
        let top_k: Vec<HitRecord> = hits
            .into_iter()
            .enumerate()
            .map(|(i, h)| HitRecord {
                rank: i + 1,
                path: h.path,
                heading: h.heading,
                score: h.score,
            })
            .collect();
        let metrics = compute_query_metrics(&q.expected, &top_k, &k_values);
        per_query.push(QueryResult {
            id: qid,
            query: q.query.clone(),
            expected: q.expected.clone(),
            top_k,
            metrics,
        });
    }

    let aggregate = aggregate_metrics(&per_query, &k_values);

    // ConfigFingerprint.{mmr,parent_retriever} are built from the **effective**
    // resolved config (toml + per-call overrides), not just the toml. This
    // matches what the pipeline actually executed for each query, so a
    // `--mmr true` CLI flag gets recorded and a future re-run with the flag
    // dropped (= MMR off) does not silently get treated as a "compatible"
    // baseline. Parent retriever has no per-call override (toml-only), but is
    // surfaced symmetrically here so future overrides can hook in cleanly.
    let resolved = opts.overrides.resolve(&opts.search_config);
    let mmr_fp = if resolved.mmr_enabled {
        Some(MmrFingerprint {
            lambda: resolved.mmr_lambda,
            same_doc_penalty: resolved.mmr_same_doc_penalty,
        })
    } else {
        None
    };
    let parent_fp = if resolved.parent_retriever_enabled {
        Some(ParentRetrieverFingerprint {
            whole_doc_threshold_tokens: resolved.parent_whole_doc_threshold_tokens,
            max_expanded_tokens: resolved.parent_max_expanded_tokens,
        })
    } else {
        None
    };
    // fusion は per-call override を持たない (D-6) ので toml をそのまま見る。
    let fusion_fp = if opts.search_config.fusion.is_builtin_default() {
        None
    } else {
        Some(FusionFingerprint {
            rrf_k: opts.search_config.fusion.rrf_k,
            bm25_heading_weight: opts.search_config.fusion.bm25_heading_weight,
            bm25_context_weight: opts.search_config.fusion.bm25_context_weight,
            bm25_content_weight: opts.search_config.fusion.bm25_content_weight,
        })
    };

    Ok(EvalRun {
        timestamp: Utc::now(),
        fingerprint: ConfigFingerprint {
            model: opts.model_choice.model_id().to_string(),
            reranker: if opts.reranker_choice.is_enabled() {
                Some(opts.reranker_choice.model_id().to_string())
            } else {
                None
            },
            limit,
            k_values: k_values.clone(),
            golden_hash,
            metric_version: METRIC_VERSION,
            mmr: mmr_fp,
            parent_retriever: parent_fp,
            fusion: fusion_fp,
        },
        per_query,
        aggregate,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    #[test]
    fn test_types_compile() {
        // 型が互いに整合していることの最小確認。後続 Task でテストを足していく。
        let _ = ExpectedHit {
            path: "x".into(),
            heading: None,
        };
    }

    fn write_yaml(name: &str, content: &str) -> PathBuf {
        let pid = std::process::id();
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let path = std::env::temp_dir().join(format!("{name}-{pid}-{nonce}.yml"));
        std::fs::write(&path, content).unwrap();
        path
    }

    #[test]
    fn test_golden_minimal_parse() {
        let path = write_yaml(
            "eval-golden-min",
            "queries:\n- query: \"hello\"\n  expected:\n  - path: \"a.md\"\n",
        );
        let gs = GoldenSet::load(&path).unwrap();
        assert_eq!(gs.queries.len(), 1);
        assert_eq!(gs.queries[0].query, "hello");
        assert_eq!(gs.queries[0].expected[0].path, "a.md");
        assert!(gs.queries[0].expected[0].heading.is_none());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_golden_with_heading_and_id_and_tags() {
        let path = write_yaml(
            "eval-golden-full",
            "defaults:\n  limit: 5\n  rerank: true\nqueries:\n- id: \"q1\"\n  query: \"RRF の k\"\n  expected:\n  - path: \"docs/arch.md\"\n    heading: \"Data flow\"\n  - path: \"src/db.rs\"\n  tags: [\"retrieval\"]\n",
        );
        let gs = GoldenSet::load(&path).unwrap();
        let d = gs.defaults.as_ref().unwrap();
        assert_eq!(d.limit, Some(5));
        assert_eq!(d.rerank, Some(true));
        let q = &gs.queries[0];
        assert_eq!(q.id.as_deref(), Some("q1"));
        assert_eq!(q.expected[0].heading.as_deref(), Some("Data flow"));
        assert!(q.expected[1].heading.is_none());
        assert_eq!(q.tags.as_deref(), Some(&["retrieval".to_string()][..]));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_golden_rejects_unknown_field() {
        let path = write_yaml(
            "eval-golden-bad",
            "queries:\n- query: \"x\"\n  expected: []\n  bogus: 1\n",
        );
        let err = GoldenSet::load(&path).expect_err("unknown field must reject");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("bogus") || msg.contains("unknown"),
            "error chain should mention bogus/unknown, got: {msg}"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_golden_missing_file_is_error() {
        let path = std::env::temp_dir().join("nonexistent-eval-golden.yml");
        let _ = std::fs::remove_file(&path);
        let err = GoldenSet::load(&path).expect_err("missing file must error");
        assert!(err.to_string().contains("no golden file"));
    }

    fn hit(rank: usize, path: &str, heading: Option<&str>) -> HitRecord {
        HitRecord {
            rank,
            path: path.into(),
            heading: heading.map(|s| s.into()),
            score: 1.0,
        }
    }
    fn exp(path: &str, heading: Option<&str>) -> ExpectedHit {
        ExpectedHit {
            path: path.into(),
            heading: heading.map(|s| s.into()),
        }
    }

    #[test]
    fn test_is_hit_path_only() {
        assert!(is_hit(&exp("a.md", None), &hit(1, "a.md", Some("H1"))));
        assert!(!is_hit(&exp("a.md", None), &hit(1, "b.md", Some("H1"))));
    }

    #[test]
    fn test_is_hit_heading_match_case_and_whitespace() {
        assert!(is_hit(
            &exp("a.md", Some("Data Flow")),
            &hit(1, "a.md", Some("  data flow "))
        ));
    }

    #[test]
    fn test_is_hit_heading_mismatch() {
        assert!(!is_hit(&exp("a.md", Some("X")), &hit(1, "a.md", Some("Y"))));
    }

    #[test]
    fn test_recall_at_k_all_hit() {
        let expected = vec![exp("a.md", None), exp("b.md", None)];
        let top = vec![
            hit(1, "a.md", None),
            hit(2, "b.md", None),
            hit(3, "c.md", None),
        ];
        assert!((recall_at_k(&expected, &top, 5) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn test_recall_at_k_partial_within_k() {
        let expected = vec![exp("a.md", None), exp("b.md", None)];
        let top = vec![
            hit(1, "a.md", None),
            hit(2, "x.md", None),
            hit(3, "b.md", None),
        ];
        assert!((recall_at_k(&expected, &top, 2) - 0.5).abs() < 1e-9);
    }

    #[test]
    fn test_recall_at_k_no_expected_is_nan_sentinel() {
        let top = vec![hit(1, "a.md", None)];
        assert_eq!(recall_at_k(&[], &top, 5), 0.0);
    }

    #[test]
    fn test_recall_at_k_empty_top() {
        let expected = vec![exp("a.md", None)];
        assert_eq!(recall_at_k(&expected, &[], 5), 0.0);
    }

    #[test]
    fn test_reciprocal_rank_first_hit() {
        let expected = vec![exp("a.md", None)];
        let top = vec![
            hit(1, "x.md", None),
            hit(2, "a.md", None),
            hit(3, "b.md", None),
        ];
        assert!((reciprocal_rank(&expected, &top) - 0.5).abs() < 1e-9);
    }

    #[test]
    fn test_reciprocal_rank_no_hit() {
        let expected = vec![exp("a.md", None)];
        let top = vec![hit(1, "x.md", None)];
        assert_eq!(reciprocal_rank(&expected, &top), 0.0);
    }

    #[test]
    fn test_reciprocal_rank_empty() {
        assert_eq!(reciprocal_rank(&[], &[]), 0.0);
    }

    /// Regression: rank=0 が万一渡されても 1.0/0.0 = inf にせず 0.0 を返す。
    /// HitRecord が pub なので外部経路防衛線として残す。
    #[test]
    fn test_reciprocal_rank_rank_zero_returns_zero_not_inf() {
        let expected = vec![exp("a.md", None)];
        let top = vec![hit(0, "a.md", None)];
        let r = reciprocal_rank(&expected, &top);
        assert_eq!(r, 0.0);
        assert!(r.is_finite());
    }

    #[test]
    fn test_ndcg_ideal_order() {
        let expected = vec![exp("a.md", None), exp("b.md", None)];
        let top = vec![
            hit(1, "a.md", None),
            hit(2, "b.md", None),
            hit(3, "x.md", None),
        ];
        assert!((ndcg_at_k(&expected, &top, 5) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn test_ndcg_reversed() {
        let expected = vec![exp("a.md", None), exp("b.md", None)];
        let top = vec![
            hit(1, "x.md", None),
            hit(2, "a.md", None),
            hit(3, "b.md", None),
        ];
        let score = ndcg_at_k(&expected, &top, 5);
        assert!(
            score > 0.0 && score < 1.0,
            "expected 0<score<1, got {score}"
        );
    }

    #[test]
    fn test_ndcg_no_hit() {
        let expected = vec![exp("a.md", None)];
        let top = vec![hit(1, "x.md", None), hit(2, "y.md", None)];
        assert_eq!(ndcg_at_k(&expected, &top, 5), 0.0);
    }

    #[test]
    fn test_ndcg_empty_expected() {
        let top = vec![hit(1, "a.md", None)];
        assert_eq!(ndcg_at_k(&[], &top, 5), 0.0);
    }

    /// Regression: 同一 expected (heading None) に対して同 path の異 heading hit が
    /// top-k に複数並ぶシナリオで nDCG が 1.0 を超えてはならない。
    /// 旧実装は top 側 loop で多重カウントし >1.0 を返していた。
    #[test]
    fn test_ndcg_multi_chunk_per_expected_capped_at_one() {
        let expected = vec![exp("docs/X.md", None)];
        let top = vec![
            hit(1, "docs/X.md", Some("Section A")),
            hit(2, "docs/X.md", Some("Section B")),
            hit(3, "docs/X.md", Some("Section C")),
            hit(4, "other.md", None),
            hit(5, "other2.md", None),
        ];
        let score = ndcg_at_k(&expected, &top, 10);
        assert!(score <= 1.0 + 1e-9, "nDCG must not exceed 1.0, got {score}");
        // 最初の hit は rank 1 (ideal) なので 1.0 ぴったり。
        assert!(
            (score - 1.0).abs() < 1e-9,
            "expected exactly 1.0, got {score}"
        );
    }

    /// Regression (mixed): 1 件目 expected は rank 2 で初 hit、2 件目 expected は
    /// 同 path の別 chunk (rank 1) で hit。各 expected は最も rank の小さい hit
    /// で 1 回ずつカウントされ、上限 1.0 を超えない。
    #[test]
    fn test_ndcg_two_expected_one_with_multiple_chunk_hits() {
        let expected = vec![
            exp("a.md", None), // ← path-only、複数 chunk が hit する
            exp("b.md", None),
        ];
        let top = vec![
            hit(1, "a.md", Some("Intro")),
            hit(2, "a.md", Some("Body")),
            hit(3, "b.md", Some("Concl")),
            hit(4, "x.md", None),
        ];
        let score = ndcg_at_k(&expected, &top, 5);
        assert!(score <= 1.0 + 1e-9, "nDCG must not exceed 1.0, got {score}");
    }

    /// Regression: expected 側に同一 path が重複していても、1 つの hit は
    /// 最大 1 回しか gain にならない (DCG ≤ IDCG)。golden yml に同じ path を
    /// 二重記載したケース + prop_ndcg_at_k_in_unit_range の flaky 要因。
    #[test]
    fn test_ndcg_duplicate_expected_entries_capped_at_one() {
        let expected = vec![exp("a.md", None), exp("a.md", None)];
        let top = vec![hit(1, "a.md", None)];
        let score = ndcg_at_k(&expected, &top, 5);
        assert!(score <= 1.0 + 1e-9, "nDCG must not exceed 1.0, got {score}");
        // 1:1 マッチでは gain は rank 1 の 1 回分のみ: 1.0 / (1/log2(2) + 1/log2(3))
        let idcg = 1.0 / 2f64.log2() + 1.0 / 3f64.log2();
        assert!(
            (score - 1.0 / idcg).abs() < 1e-9,
            "expected {}, got {score}",
            1.0 / idcg
        );
    }

    /// Regression: path-only expected と同 path の heading 指定 expected が
    /// 同一 hit にマッチし得るケース。1 hit = 1 gain の 1:1 マッチで上限 1.0 を守る。
    #[test]
    fn test_ndcg_path_only_and_heading_expected_share_single_hit() {
        let expected = vec![exp("a.md", None), exp("a.md", Some("X"))];
        let top = vec![hit(1, "a.md", Some("X"))];
        let score = ndcg_at_k(&expected, &top, 5);
        assert!(score <= 1.0 + 1e-9, "nDCG must not exceed 1.0, got {score}");
    }

    /// 1:1 マッチの割当品質: heading 指定 expected を優先消費することで、
    /// path-only expected が後続 hit に回り、両 expected が credit を得る。
    #[test]
    fn test_ndcg_heading_expected_preferred_over_path_only() {
        let expected = vec![exp("a.md", None), exp("a.md", Some("X"))];
        let top = vec![hit(1, "a.md", Some("X")), hit(2, "a.md", Some("Y"))];
        // rank 1 は heading 指定の exp("a.md","X") が消費し、path-only は rank 2 で hit
        // → DCG = 1/log2(2) + 1/log2(3) = IDCG → nDCG = 1.0
        let score = ndcg_at_k(&expected, &top, 5);
        assert!(
            (score - 1.0).abs() < 1e-9,
            "expected exactly 1.0, got {score}"
        );
    }

    // -----------------------------------------------------------------------
    // F-37: f64 invariant property tests
    // recall_at_k / ndcg_at_k は binary relevance metric なので、入力に
    // 関わらず必ず [0.0, 1.0] の値域を持つ。proptest で多様な expected /
    // top の組合せを投げて値域違反 (nDCG > 1.0 級の regression) を機械的に
    // catch する。
    // -----------------------------------------------------------------------

    proptest::proptest! {
        #![proptest_config(proptest::test_runner::Config {
            cases: 256,
            ..proptest::test_runner::Config::default()
        })]

        /// recall_at_k の値域 invariant: 任意の expected / top / k に対して
        /// 結果は [0.0, 1.0] に収まり、有限値である。
        #[test]
        fn prop_recall_at_k_in_unit_range(
            expected_paths in proptest::collection::vec("[a-z]{1,4}\\.md", 0..6),
            top_paths in proptest::collection::vec("[a-z]{1,4}\\.md", 0..12),
            k in 0usize..15,
        ) {
            let expected: Vec<ExpectedHit> = expected_paths
                .iter()
                .map(|p| exp(p, None))
                .collect();
            let top: Vec<HitRecord> = top_paths
                .iter()
                .enumerate()
                .map(|(i, p)| hit(i + 1, p, None))
                .collect();
            let score = recall_at_k(&expected, &top, k);
            proptest::prop_assert!(
                score.is_finite() && (0.0..=1.0).contains(&score),
                "recall@{} must be in [0.0, 1.0] and finite, got {}",
                k,
                score
            );
        }

        /// ndcg_at_k の値域 invariant: 任意の expected / top / k に対して
        /// 結果は [0.0, 1.0] に収まり、有限値である。同 path 多 chunk
        /// (multi-heading) のシナリオでも DCG が IDCG を超えないことを
        /// 含意する (v0.4.2 で fix した regression の永続防御)。
        #[test]
        fn prop_ndcg_at_k_in_unit_range(
            expected_paths in proptest::collection::vec("[a-z]{1,4}\\.md", 0..6),
            top_entries in proptest::collection::vec(
                ("[a-z]{1,4}\\.md", proptest::option::of("[A-Z]{1,4}")),
                0..12,
            ),
            k in 0usize..15,
        ) {
            let expected: Vec<ExpectedHit> = expected_paths
                .iter()
                .map(|p| exp(p, None))
                .collect();
            let top: Vec<HitRecord> = top_entries
                .iter()
                .enumerate()
                .map(|(i, (p, h))| hit(i + 1, p, h.as_deref()))
                .collect();
            let score = ndcg_at_k(&expected, &top, k);
            proptest::prop_assert!(
                score.is_finite() && (0.0..=1.0).contains(&score),
                "nDCG@{} must be in [0.0, 1.0] and finite, got {}",
                k,
                score
            );
        }

        /// reciprocal_rank の値域 invariant: 任意入力に対して [0.0, 1.0]
        /// に収まり、有限値である (rank=0 は内部 guard で 0.0 に倒れる)。
        #[test]
        fn prop_reciprocal_rank_in_unit_range(
            expected_paths in proptest::collection::vec("[a-z]{1,4}\\.md", 0..6),
            top_paths in proptest::collection::vec("[a-z]{1,4}\\.md", 0..12),
        ) {
            let expected: Vec<ExpectedHit> = expected_paths
                .iter()
                .map(|p| exp(p, None))
                .collect();
            let top: Vec<HitRecord> = top_paths
                .iter()
                .enumerate()
                .map(|(i, p)| hit(i + 1, p, None))
                .collect();
            let rr = reciprocal_rank(&expected, &top);
            proptest::prop_assert!(
                rr.is_finite() && (0.0..=1.0).contains(&rr),
                "reciprocal_rank must be in [0.0, 1.0] and finite, got {}",
                rr
            );
        }
    }

    #[test]
    fn test_compute_query_metrics() {
        let expected = vec![exp("a.md", None), exp("b.md", None)];
        let top = vec![
            hit(1, "a.md", None),
            hit(2, "x.md", None),
            hit(3, "b.md", None),
        ];
        let m = compute_query_metrics(&expected, &top, &[1, 3, 5]);
        assert!((m.recall_at_k[&1] - 0.5).abs() < 1e-9);
        assert!((m.recall_at_k[&3] - 1.0).abs() < 1e-9);
        assert!((m.reciprocal_rank - 1.0).abs() < 1e-9);
        let ndcg3 = m.ndcg_at_k[&3];
        assert!(ndcg3 > 0.7 && ndcg3 < 1.0, "ndcg@3 = {ndcg3}");
    }

    #[test]
    fn test_aggregate_metrics_mean() {
        let q1 = QueryResult {
            id: "1".into(),
            query: "q1".into(),
            expected: vec![exp("a.md", None)],
            top_k: vec![hit(1, "a.md", None)],
            metrics: compute_query_metrics(&[exp("a.md", None)], &[hit(1, "a.md", None)], &[1, 5]),
        };
        let q2 = QueryResult {
            id: "2".into(),
            query: "q2".into(),
            expected: vec![exp("b.md", None)],
            top_k: vec![hit(1, "x.md", None)],
            metrics: compute_query_metrics(&[exp("b.md", None)], &[hit(1, "x.md", None)], &[1, 5]),
        };
        let agg = aggregate_metrics(&[q1, q2], &[1, 5]);
        assert!((agg.recall_at_k[&1] - 0.5).abs() < 1e-9);
        assert!((agg.mrr - 0.5).abs() < 1e-9);
        assert_eq!(agg.query_count, 2);
    }

    fn sample_run(ts_secs: i64, recall10: f64) -> EvalRun {
        use chrono::TimeZone;
        let mut agg = AggregateMetrics::default();
        agg.recall_at_k.insert(10, recall10);
        agg.query_count = 1;
        EvalRun {
            timestamp: Utc.timestamp_opt(ts_secs, 0).unwrap(),
            fingerprint: ConfigFingerprint {
                model: "bge-m3".into(),
                reranker: None,
                limit: 10,
                k_values: vec![1, 5, 10],
                golden_hash: "deadbeef".into(),
                metric_version: METRIC_VERSION,
                mmr: None,
                parent_retriever: None,
                fusion: None,
            },
            per_query: vec![],
            aggregate: agg,
        }
    }

    #[test]
    fn test_history_load_missing_returns_empty() {
        let path = std::env::temp_dir().join("kb-mcp-hist-missing.json");
        let _ = std::fs::remove_file(&path);
        let h = History::load(&path).unwrap();
        assert!(h.runs.is_empty());
    }

    #[test]
    fn test_history_load_corrupt_returns_empty_with_warn() {
        let pid = std::process::id();
        let path = std::env::temp_dir().join(format!("kb-mcp-hist-corrupt-{pid}.json"));
        std::fs::write(&path, "{not json").unwrap();
        let h = History::load(&path).unwrap();
        assert!(h.runs.is_empty());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_history_save_and_reload_round_trip() {
        let pid = std::process::id();
        let path = std::env::temp_dir().join(format!("kb-mcp-hist-rt-{pid}.json"));
        let _ = std::fs::remove_file(&path);
        let mut h = History::default();
        h.push_front(sample_run(100, 0.5), 10);
        h.save(&path).unwrap();
        let reloaded = History::load(&path).unwrap();
        assert_eq!(reloaded.runs.len(), 1);
        assert!((reloaded.runs[0].aggregate.recall_at_k[&10] - 0.5).abs() < 1e-9);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_history_push_front_truncates_to_size() {
        let mut h = History::default();
        for i in 0..15 {
            h.push_front(sample_run(i as i64, 0.0), 10);
        }
        assert_eq!(h.runs.len(), 10);
        assert_eq!(h.runs.front().unwrap().timestamp.timestamp(), 14);
    }

    #[test]
    fn test_format_text_single_run_has_aggregate_header() {
        let mut agg = AggregateMetrics::default();
        agg.recall_at_k.insert(1, 0.5);
        agg.recall_at_k.insert(5, 0.8);
        agg.ndcg_at_k.insert(5, 0.7);
        agg.mrr = 0.6;
        agg.query_count = 2;
        let run = EvalRun {
            timestamp: Utc::now(),
            fingerprint: ConfigFingerprint {
                model: "bge-m3".into(),
                reranker: None,
                limit: 10,
                k_values: vec![1, 5],
                golden_hash: "h".into(),
                metric_version: METRIC_VERSION,
                mmr: None,
                parent_retriever: None,
                fusion: None,
            },
            per_query: vec![],
            aggregate: agg,
        };
        let out = format_text(&run, None, false, 0.05);
        assert!(out.contains("model: bge-m3"));
        assert!(out.contains("queries: 2"));
        assert!(out.contains("recall@1"));
        assert!(out.contains("recall@5"));
        assert!(out.contains("MRR"));
        assert!(out.contains("nDCG@5"));
        assert!(!out.contains("previous run"));
    }

    #[test]
    fn test_format_text_diff_arrows() {
        let fp = ConfigFingerprint {
            model: "m".into(),
            reranker: None,
            limit: 10,
            k_values: vec![5],
            golden_hash: "h".into(),
            metric_version: METRIC_VERSION,
            mmr: None,
            parent_retriever: None,
            fusion: None,
        };
        let mut a_now = AggregateMetrics::default();
        a_now.recall_at_k.insert(5, 0.8);
        a_now.ndcg_at_k.insert(5, 0.7);
        a_now.query_count = 1;
        let mut a_prev = AggregateMetrics::default();
        a_prev.recall_at_k.insert(5, 0.6);
        a_prev.ndcg_at_k.insert(5, 0.7);
        a_prev.query_count = 1;
        let now = EvalRun {
            timestamp: Utc::now(),
            fingerprint: fp.clone(),
            per_query: vec![],
            aggregate: a_now,
        };
        let prev = EvalRun {
            timestamp: Utc::now(),
            fingerprint: fp,
            per_query: vec![],
            aggregate: a_prev,
        };
        let out = format_text(&now, Some(&prev), false, 0.05);
        // 改善矢印 (↑) があるか、または絶対値の形で diff が含まれるか
        assert!(out.contains("↑") || out.contains("0.200"));
        assert!(out.contains("previous run"));
    }

    #[test]
    fn test_format_text_fingerprint_mismatch_shows_warning() {
        let fp_now = ConfigFingerprint {
            model: "m".into(),
            reranker: None,
            limit: 10,
            k_values: vec![5],
            golden_hash: "AAA".into(),
            metric_version: METRIC_VERSION,
            mmr: None,
            parent_retriever: None,
            fusion: None,
        };
        let fp_prev = ConfigFingerprint {
            golden_hash: "BBB".into(),
            ..fp_now.clone()
        };
        let mut agg = AggregateMetrics::default();
        agg.recall_at_k.insert(5, 0.8);
        agg.query_count = 1;
        let now = EvalRun {
            timestamp: Utc::now(),
            fingerprint: fp_now,
            per_query: vec![],
            aggregate: agg.clone(),
        };
        let prev = EvalRun {
            timestamp: Utc::now(),
            fingerprint: fp_prev,
            per_query: vec![],
            aggregate: agg,
        };
        let out = format_text(&now, Some(&prev), false, 0.05);
        assert!(out.contains("golden changed"));
    }

    #[test]
    fn test_format_json_shape() {
        let mut agg = AggregateMetrics::default();
        agg.recall_at_k.insert(1, 0.5);
        agg.recall_at_k.insert(5, 0.8);
        agg.mrr = 0.75;
        agg.ndcg_at_k.insert(5, 0.7);
        agg.query_count = 2;
        let run = EvalRun {
            timestamp: Utc::now(),
            fingerprint: ConfigFingerprint {
                model: "bge-m3".into(),
                reranker: None,
                limit: 10,
                k_values: vec![1, 5],
                golden_hash: "abc".into(),
                metric_version: METRIC_VERSION,
                mmr: None,
                parent_retriever: None,
                fusion: None,
            },
            per_query: vec![],
            aggregate: agg,
        };
        let v = format_json(&run, None);
        assert_eq!(v["aggregate"]["mrr"].as_f64().unwrap(), 0.75);
        assert_eq!(v["aggregate"]["recall_at_k"]["5"].as_f64().unwrap(), 0.8);
        assert_eq!(v["fingerprint"]["model"].as_str().unwrap(), "bge-m3");
        assert!(v["previous"].is_null());
        assert!(v["diff"].is_null());
    }

    #[test]
    fn test_format_json_with_previous() {
        let mut a1 = AggregateMetrics::default();
        a1.recall_at_k.insert(5, 0.8);
        let mut a0 = AggregateMetrics::default();
        a0.recall_at_k.insert(5, 0.6);
        let fp = ConfigFingerprint {
            model: "m".into(),
            reranker: None,
            limit: 10,
            k_values: vec![5],
            golden_hash: "h".into(),
            metric_version: METRIC_VERSION,
            mmr: None,
            parent_retriever: None,
            fusion: None,
        };
        let now = EvalRun {
            timestamp: Utc::now(),
            fingerprint: fp.clone(),
            per_query: vec![],
            aggregate: a1,
        };
        let prev = EvalRun {
            timestamp: Utc::now(),
            fingerprint: fp,
            per_query: vec![],
            aggregate: a0,
        };
        let v = format_json(&now, Some(&prev));
        assert!(!v["previous"].is_null());
        let diff5 = v["diff"]["recall_at_k"]["5"].as_f64().unwrap();
        assert!((diff5 - 0.2).abs() < 1e-9);
    }

    /// Metric version 違いの previous とは表示 diff も無効化する (codex P2 round 2
    /// on PR #76)。--fail-on-regression 側は除外済みでも、表示側が golden_hash
    /// のみで gate すると旧式との赤 delta が出てしまう。
    #[test]
    fn test_format_json_metric_version_mismatch_disables_diff() {
        let mut a1 = AggregateMetrics::default();
        a1.recall_at_k.insert(5, 0.6);
        let mut a0 = AggregateMetrics::default();
        a0.recall_at_k.insert(5, 0.8);
        let fp_now = ConfigFingerprint {
            model: "m".into(),
            reranker: None,
            limit: 10,
            k_values: vec![5],
            golden_hash: "h".into(),
            metric_version: METRIC_VERSION,
            mmr: None,
            parent_retriever: None,
            fusion: None,
        };
        let fp_prev = ConfigFingerprint {
            metric_version: 1,
            ..fp_now.clone()
        };
        let now = EvalRun {
            timestamp: Utc::now(),
            fingerprint: fp_now,
            per_query: vec![],
            aggregate: a1,
        };
        let prev = EvalRun {
            timestamp: Utc::now(),
            fingerprint: fp_prev,
            per_query: vec![],
            aggregate: a0,
        };
        let v = format_json(&now, Some(&prev));
        assert!(
            v["diff"].is_null(),
            "diff must be null across metric versions"
        );
    }

    /// format_text も metric version 違いでは diff を無効化し、golden 変更とは
    /// 区別されたメッセージを出す。
    #[test]
    fn test_format_text_metric_version_mismatch_disables_diff() {
        let mut a1 = AggregateMetrics::default();
        a1.recall_at_k.insert(5, 0.6);
        a1.query_count = 1;
        let mut a0 = AggregateMetrics::default();
        a0.recall_at_k.insert(5, 0.8);
        a0.query_count = 1;
        let fp_now = ConfigFingerprint {
            model: "m".into(),
            reranker: None,
            limit: 10,
            k_values: vec![5],
            golden_hash: "h".into(),
            metric_version: METRIC_VERSION,
            mmr: None,
            parent_retriever: None,
            fusion: None,
        };
        let fp_prev = ConfigFingerprint {
            metric_version: 1,
            ..fp_now.clone()
        };
        let now = EvalRun {
            timestamp: Utc::now(),
            fingerprint: fp_now,
            per_query: vec![],
            aggregate: a1,
        };
        let prev = EvalRun {
            timestamp: Utc::now(),
            fingerprint: fp_prev,
            per_query: vec![],
            aggregate: a0,
        };
        let out = format_text(&now, Some(&prev), false, 0.05);
        assert!(out.contains("diff disabled"), "got: {out}");
        assert!(!out.contains("golden changed"), "got: {out}");
        assert!(
            !out.contains("↓"),
            "must not render downward delta, got: {out}"
        );
    }

    #[test]
    fn test_aggregate_metrics_skips_empty_expected() {
        let q_empty = QueryResult {
            id: "e".into(),
            query: "q".into(),
            expected: vec![],
            top_k: vec![hit(1, "a.md", None)],
            metrics: compute_query_metrics(&[], &[hit(1, "a.md", None)], &[1]),
        };
        let q_ok = QueryResult {
            id: "o".into(),
            query: "q".into(),
            expected: vec![exp("a.md", None)],
            top_k: vec![hit(1, "a.md", None)],
            metrics: compute_query_metrics(&[exp("a.md", None)], &[hit(1, "a.md", None)], &[1]),
        };
        let agg = aggregate_metrics(&[q_empty, q_ok], &[1]);
        assert_eq!(agg.query_count, 1);
        assert!((agg.recall_at_k[&1] - 1.0).abs() < 1e-9);
    }

    // ------------------------------------------------------------------
    // F-40: regression detection helpers
    // ------------------------------------------------------------------

    /// Build a synthetic `EvalRun` with the given aggregate values. Other
    /// fields are minimum viable so equality / fingerprint logic in callers
    /// is exercised, but per_query is left empty.
    fn synthetic_run(
        recall: BTreeMap<usize, f64>,
        mrr: f64,
        ndcg: BTreeMap<usize, f64>,
        golden_hash: &str,
    ) -> EvalRun {
        EvalRun {
            timestamp: Utc::now(),
            fingerprint: ConfigFingerprint {
                model: "bge-small-en-v1.5".into(),
                reranker: None,
                limit: 10,
                k_values: recall.keys().copied().collect(),
                golden_hash: golden_hash.into(),
                metric_version: METRIC_VERSION,
                mmr: None,
                parent_retriever: None,
                fusion: None,
            },
            per_query: vec![],
            aggregate: AggregateMetrics {
                recall_at_k: recall,
                mrr,
                ndcg_at_k: ndcg,
                query_count: 0,
            },
        }
    }

    fn map_one(k: usize, v: f64) -> BTreeMap<usize, f64> {
        let mut m = BTreeMap::new();
        m.insert(k, v);
        m
    }

    /// 改善: prev=0.7, now=0.8 → regression false。
    #[test]
    fn test_is_regression_improvement_returns_false() {
        let prev = synthetic_run(map_one(5, 0.7), 0.6, map_one(10, 0.5), "h");
        let now = synthetic_run(map_one(5, 0.8), 0.7, map_one(10, 0.6), "h");
        assert!(!is_regression(&now, &prev, 0.05));
    }

    /// 同値: prev == now → regression false。
    #[test]
    fn test_is_regression_no_change_returns_false() {
        let prev = synthetic_run(map_one(5, 0.7), 0.6, map_one(10, 0.5), "h");
        let now = synthetic_run(map_one(5, 0.7), 0.6, map_one(10, 0.5), "h");
        assert!(!is_regression(&now, &prev, 0.05));
    }

    /// threshold 内の僅かな低下 (0.7 → 0.66、threshold 0.05) → false。
    #[test]
    fn test_is_regression_within_threshold_returns_false() {
        let prev = synthetic_run(map_one(5, 0.7), 0.6, map_one(10, 0.5), "h");
        let now = synthetic_run(map_one(5, 0.66), 0.6, map_one(10, 0.5), "h");
        assert!(!is_regression(&now, &prev, 0.05));
    }

    /// recall@k で threshold 超え (0.8 → 0.6) → true。
    #[test]
    fn test_is_regression_recall_drop_returns_true() {
        let prev = synthetic_run(map_one(5, 0.8), 0.6, map_one(10, 0.5), "h");
        let now = synthetic_run(map_one(5, 0.6), 0.6, map_one(10, 0.5), "h");
        assert!(is_regression(&now, &prev, 0.05));
    }

    /// MRR で threshold 超え → true (recall / nDCG は不変)。
    #[test]
    fn test_is_regression_mrr_drop_returns_true() {
        let prev = synthetic_run(map_one(5, 0.7), 0.9, map_one(10, 0.5), "h");
        let now = synthetic_run(map_one(5, 0.7), 0.8, map_one(10, 0.5), "h");
        assert!(is_regression(&now, &prev, 0.05));
    }

    /// nDCG@k で threshold 超え → true (recall / MRR は不変)。
    #[test]
    fn test_is_regression_ndcg_drop_returns_true() {
        let prev = synthetic_run(map_one(5, 0.7), 0.6, map_one(10, 0.9), "h");
        let now = synthetic_run(map_one(5, 0.7), 0.6, map_one(10, 0.7), "h");
        assert!(is_regression(&now, &prev, 0.05));
    }

    /// NaN/Inf を含む場合は比較不能 = false (CI を fail にしない安全側)。
    /// proptest で値域は固定されているが防御的に確認。
    #[test]
    fn test_is_regression_non_finite_returns_false() {
        let prev = synthetic_run(map_one(5, f64::NAN), 0.6, map_one(10, 0.5), "h");
        let now = synthetic_run(map_one(5, 0.0), 0.6, map_one(10, 0.5), "h");
        assert!(!is_regression(&now, &prev, 0.05));
    }

    /// Regression (codex P2 on PR #81): eval のメタデータは検索経路の実効値に
    /// 揃える。揃えないと `--limit 1001` と `--limit 2000` が同じ retrieval なのに
    /// fingerprint 違いで history 比較から外れ、上限超えの `k` は「@k と表示
    /// されているが実際は上限件数から計算した」metric になる。
    #[test]
    fn test_normalize_eval_limit_and_k_matches_the_retrieval_cap() {
        let cap = crate::server::SEARCH_LIMIT_MAX;
        // 上限超えの limit と k は cap に丸まる = 1001 と 2000 が同一値になる。
        let (l1, k1) = normalize_eval_limit_and_k(1001, &[1, 5, 2000]);
        let (l2, k2) = normalize_eval_limit_and_k(2000, &[1, 5, 5000]);
        assert_eq!(l1, cap);
        assert_eq!(l1, l2, "equivalent runs must share the effective limit");
        assert_eq!(k1, vec![1, 5, cap as usize]);
        assert_eq!(k1, k2, "equivalent runs must share the effective k list");
        // 上限内はそのまま素通し。
        let (l3, k3) = normalize_eval_limit_and_k(10, &[1, 5, 10]);
        assert_eq!(l3, 10);
        assert_eq!(k3, vec![1, 5, 10]);
    }

    /// History::previous_compatible: fingerprint 一致 → Some。
    #[test]
    fn test_previous_compatible_matching_fingerprint() {
        let mut h = History::default();
        let prev = synthetic_run(map_one(5, 0.7), 0.6, map_one(10, 0.5), "golden_xyz");
        let now = synthetic_run(map_one(5, 0.6), 0.6, map_one(10, 0.5), "golden_xyz");
        h.push_front(prev, 10);
        assert!(h.previous_compatible(&now).is_some());
    }

    /// History::previous_compatible: fingerprint 違い (golden_hash 変更) → None。
    /// CI 文脈では「golden YAML を更新したら勝手に regression 扱いになる」を回避する。
    #[test]
    fn test_previous_compatible_mismatched_fingerprint_returns_none() {
        let mut h = History::default();
        let prev = synthetic_run(map_one(5, 0.9), 0.9, map_one(10, 0.9), "golden_OLD");
        let now = synthetic_run(map_one(5, 0.5), 0.5, map_one(10, 0.5), "golden_NEW");
        h.push_front(prev, 10);
        assert!(h.previous_compatible(&now).is_none());
    }

    /// 旧 history JSON (metric_version field なし) は legacy = 1 として読まれる。
    #[test]
    fn test_fingerprint_metric_version_defaults_to_one_on_old_json() {
        let json =
            r#"{"model":"bge-m3","reranker":null,"limit":10,"k_values":[5],"golden_hash":"h"}"#;
        let fp: ConfigFingerprint = serde_json::from_str(json).unwrap();
        assert_eq!(fp.metric_version, 1);
    }

    /// Metric 式修正 (= METRIC_VERSION bump) 後は旧 version で記録された run と
    /// 比較しない。旧 nDCG の数値と比べて --fail-on-regression が式修正を
    /// retrieval regression と誤検出するのを防ぐ (codex P2 on PR #76)。
    #[test]
    fn test_previous_compatible_rejects_old_metric_version() {
        let mut h = History::default();
        let mut prev = synthetic_run(map_one(5, 0.9), 0.9, map_one(10, 0.9), "golden_xyz");
        prev.fingerprint.metric_version = 1;
        let now = synthetic_run(map_one(5, 0.5), 0.5, map_one(10, 0.5), "golden_xyz");
        h.push_front(prev, 10);
        assert!(h.previous_compatible(&now).is_none());
    }

    /// from_config は常に現行 METRIC_VERSION を書き込む。
    #[test]
    fn test_fingerprint_from_config_sets_current_metric_version() {
        let cfg: crate::config::Config = toml::from_str("").unwrap();
        let fp = ConfigFingerprint::from_config(
            &cfg,
            "bge-m3".to_string(),
            None,
            10,
            vec![1, 5, 10],
            "deadbeef".to_string(),
        );
        assert_eq!(fp.metric_version, METRIC_VERSION);
    }

    // ------------------------------------------------------------------
    // feature-28 PR-2: ConfigFingerprint.mmr (Option<MmrFingerprint>)
    // ------------------------------------------------------------------

    #[test]
    fn test_fingerprint_mmr_off_serializes_as_none() {
        // MMR が off の Config から ConfigFingerprint を構築すると
        // mmr field は None
        let toml = r#"
[search.mmr]
enabled = false
"#;
        let cfg: crate::config::Config = toml::from_str(toml).unwrap();
        let fp = ConfigFingerprint::from_config(
            &cfg,
            "bge-m3".to_string(),
            None,
            10,
            vec![1, 5, 10],
            "deadbeef".to_string(),
        );
        assert!(fp.mmr.is_none());
    }

    #[test]
    fn test_fingerprint_mmr_on_serializes_as_some() {
        let toml = r#"
[search.mmr]
enabled = true
lambda = 0.5
same_doc_penalty = 0.1
"#;
        let cfg: crate::config::Config = toml::from_str(toml).unwrap();
        let fp = ConfigFingerprint::from_config(
            &cfg,
            "bge-m3".to_string(),
            None,
            10,
            vec![1, 5, 10],
            "deadbeef".to_string(),
        );
        let mmr = fp.mmr.expect("mmr should be Some");
        assert!((mmr.lambda - 0.5).abs() < 1e-6);
        assert!((mmr.same_doc_penalty - 0.1).abs() < 1e-6);
    }

    #[test]
    fn test_history_load_handles_old_json_without_mmr_field() {
        // 旧 JSON history (mmr field なし) を deserialize しても fail しない
        let old_json = serde_json::json!({
            "model": "bge-m3",
            "reranker": null,
            "limit": 10,
            "k_values": [1, 5, 10],
            "golden_hash": "abc"
        });
        let fp: ConfigFingerprint = serde_json::from_value(old_json).expect("load old");
        assert!(fp.mmr.is_none());
    }

    // ------------------------------------------------------------------
    // feature-28 PR-3: ConfigFingerprint.parent_retriever
    // (Option<ParentRetrieverFingerprint>)
    // ------------------------------------------------------------------

    #[test]
    fn test_fingerprint_parent_retriever_off_serializes_as_none() {
        let toml = r#"
[search.parent_retriever]
enabled = false
"#;
        let cfg: crate::config::Config = toml::from_str(toml).unwrap();
        let fp = ConfigFingerprint::from_config(
            &cfg,
            "bge-m3".into(),
            None,
            10,
            vec![1, 5, 10],
            "deadbeef".into(),
        );
        assert!(fp.parent_retriever.is_none());
    }

    #[test]
    fn test_fingerprint_parent_retriever_on_serializes_as_some() {
        let toml = r#"
[search.parent_retriever]
enabled = true
whole_doc_threshold_tokens = 50
max_expanded_tokens = 1500
"#;
        let cfg: crate::config::Config = toml::from_str(toml).unwrap();
        let fp = ConfigFingerprint::from_config(
            &cfg,
            "bge-m3".into(),
            None,
            10,
            vec![1, 5, 10],
            "deadbeef".into(),
        );
        let p = fp
            .parent_retriever
            .expect("parent_retriever should be Some");
        assert_eq!(p.whole_doc_threshold_tokens, 50);
        assert_eq!(p.max_expanded_tokens, 1500);
    }

    #[test]
    fn test_history_load_handles_old_json_without_parent_retriever_field() {
        // 旧 JSON history (parent_retriever field なし) を deserialize しても fail しない
        let old_json = serde_json::json!({
            "model": "bge-m3",
            "reranker": null,
            "limit": 10,
            "k_values": [1, 5, 10],
            "golden_hash": "abc"
        });
        let fp: ConfigFingerprint = serde_json::from_value(old_json).expect("load old");
        assert!(fp.parent_retriever.is_none());
        assert!(fp.mmr.is_none());
    }

    #[test]
    fn test_fingerprint_fusion_is_none_for_builtin_default() {
        // D-7 / E-10: 既定値なら fusion は None = 旧 history JSON と PartialEq 互換。
        let cfg = crate::config::Config::default();
        let fp = ConfigFingerprint::from_config(
            &cfg,
            "bge-small-en-v1.5".into(),
            None,
            10,
            vec![1, 5, 10],
            "hash".into(),
        );
        assert!(fp.fusion.is_none(), "builtin default must not be recorded");

        // 既定値を明示指定した場合も None
        let toml = concat!(
            "[search.fusion]\n",
            "rrf_k = 60.0\n",
            "bm25_heading_weight = 2.0\n",
        );
        let cfg: crate::config::Config = toml::from_str(toml).unwrap();
        let fp = ConfigFingerprint::from_config(
            &cfg,
            "bge-small-en-v1.5".into(),
            None,
            10,
            vec![1, 5, 10],
            "hash".into(),
        );
        assert!(fp.fusion.is_none());
    }

    #[test]
    fn test_fingerprint_fusion_is_recorded_when_tuned() {
        let toml = "[search.fusion]\nrrf_k = 10.0\nbm25_heading_weight = 4.0\n";
        let cfg: crate::config::Config = toml::from_str(toml).unwrap();
        let fp = ConfigFingerprint::from_config(
            &cfg,
            "bge-small-en-v1.5".into(),
            None,
            10,
            vec![1, 5, 10],
            "hash".into(),
        );
        let f = fp.fusion.as_ref().expect("tuned fusion must be recorded");
        assert_eq!(f.rrf_k, 10.0);
        assert_eq!(f.bm25_heading_weight, 4.0);
        assert_eq!(f.bm25_context_weight, 1.0);
        assert_eq!(f.bm25_content_weight, 1.0);
    }

    #[test]
    fn test_fingerprint_without_fusion_field_deserializes() {
        // 旧 history JSON (fusion field なし) が読めること = serde(default)。
        let json = r#"{
            "model": "bge-small-en-v1.5",
            "reranker": null,
            "limit": 10,
            "k_values": [1, 5, 10],
            "golden_hash": "abc",
            "metric_version": 2
        }"#;
        let fp: ConfigFingerprint = serde_json::from_str(json).unwrap();
        assert!(fp.fusion.is_none());

        // かつ、既定 fusion の現行 fingerprint と PartialEq 互換であること
        let now = ConfigFingerprint {
            model: "bge-small-en-v1.5".into(),
            reranker: None,
            limit: 10,
            k_values: vec![1, 5, 10],
            golden_hash: "abc".into(),
            metric_version: 2,
            mmr: None,
            parent_retriever: None,
            fusion: None,
        };
        assert_eq!(fp, now);
    }
}
