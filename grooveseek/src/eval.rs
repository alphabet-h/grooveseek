//! `groove eval` — retrieval quality evaluation subcommand.
//!
//! Opt-in パワーユーザ向け機能。Golden query YAML を読み、`db::search_hybrid`
//! で検索し、recall@k / MRR / nDCG@k を計算する。直前実行との diff を表示する。

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::path::{Path, PathBuf};

/// Refuse to read more than this much of the golden file.
///
/// It defaults to living **inside the knowledge base**, at
/// `<kb>/.groove-eval.yml`, so it is written by whoever writes the notes, and a
/// stray binary that lands on that name should not be parsed whole.
/// `.grooveignore` has the same argument and the same shape of cap.
///
/// A megabyte is far past what the file is for: the golden in
/// `tests/fixtures/` is 5.5 KB for 25 queries, so this leaves room for
/// thousands.
pub const MAX_GOLDEN_FILE_BYTES: u64 = 1024 * 1024;

/// The same for the history — and **much larger, for a different reason**.
///
/// The history is not written by a person. `groove eval` writes it, pruned to
/// `history_size` runs, and each run carries every golden query with its
/// `top_k` hits. Measured by serialising the real structures:
///
/// | golden queries | `limit` | 10 runs |
/// |---|---|---|
/// | 25 (this repo's fixture) | 10 | 0.598 MiB |
/// | 25 | 20 | **1.049 MiB** |
/// | 100 | 10 | 2.379 MiB |
///
/// So a megabyte is *inside* the ordinary range, not past it. Sharing the
/// golden's cap would refuse a perfectly valid history — and, before
/// [`History::load`] was made to fail on a refusal, the next run would then
/// have saved a fresh history over it and taken every baseline with it
/// (codex P1 on PR #203, measured above).
///
/// 64 MiB is roughly 25 times the largest of those, and still bounds a file
/// that is not a history at all.
pub const MAX_HISTORY_FILE_BYTES: u64 = 64 * 1024 * 1024;

/// Whether **the name** is taken, which is not the same as whether a file can
/// be read through it.
///
/// `Path::exists` follows a symlink and answers about the target, so a link
/// whose target is gone reads as absent. For these two files "absent" is the
/// answer with consequences — the golden's absence is a hint and the history's
/// absence is an empty history that then gets **saved over the link** — so the
/// question has to be about the name, and anything else is left to
/// [`read_bounded`] to refuse (codex P2 on PR #203).
///
/// **`NotFound` is an answer; anything else is a failure to ask.** An ACL, an
/// unreadable parent, a filesystem that went away — returning `false` for
/// those makes the history empty, and if the condition clears before the save
/// the new run replaces the baselines without ever comparing (codex P2 on
/// PR #206). The doctor half of the original PR had the same defect in the
/// same shape.
fn is_present(path: &Path) -> Result<bool> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(anyhow::Error::new(e))
            .with_context(|| format!("could not look at {}", path.display())),
    }
}

/// Read one of those two files, bounded, from the handle the check was made on.
///
/// [`crate::links::read_checked`] is the same route `.grooveignore` takes, so a
/// hard link, a FIFO, something that is not a regular file, or a file over the
/// cap is a **refusal** rather than an unbounded read. The refusal message
/// names the path and the reason.
///
/// **The symlink half is Unix-only**, and that is `links`'s decision rather
/// than an omission here: its module documentation records that making one on
/// Windows needs a privilege the threat model's attacker does not have, and
/// that refusing reparse points there would refuse every OneDrive and Dropbox
/// placeholder. Saying "a symlink is refused" without that scope would be a
/// promise this does not keep (codex P2 on PR #203).
fn read_bounded(path: &Path, cap: u64, what: &str) -> Result<Vec<u8>> {
    match crate::links::read_checked(path, cap) {
        Ok(crate::links::Content::Bytes(b)) => Ok(b),
        Ok(crate::links::Content::Refused(r)) => {
            anyhow::bail!("refusing to read the {what}: {}", r.log_line(path))
        }
        Err(e) => Err(anyhow::Error::new(e))
            .with_context(|| format!("failed to read the {what}: {}", path.display())),
    }
}

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
        Ok(Self::load_with_bytes(path)?.0)
    }

    /// The same read, handing back the bytes the fingerprint is taken over.
    ///
    /// `eval::run` needs both and used to do its own `fs::read` and parse
    /// beside this one: two reads of one file, two error messages, and — once
    /// a cap existed — two places for it to disagree with itself. `tune` only
    /// ever went through [`Self::load`], so a bound added to the other copy
    /// would have missed it entirely.
    ///
    /// A golden holding a query that cannot be searched — one made only of
    /// `-term` exclusions — is refused here, at the read, rather than scored
    /// as a miss. [`QueryResult`] has nowhere to record "this query was never
    /// run", so a query that quietly returned nothing would land in the
    /// recall and MRR denominators as a genuine zero and drag the reported
    /// numbers down without saying why. It is a broken golden in the same
    /// sense an unknown field is, and fixing it changes `golden_hash`, which
    /// is exactly what should happen to a history comparison across the fix.
    pub fn load_with_bytes(path: &Path) -> Result<(Self, Vec<u8>)> {
        // See [`is_present`]: a dangling symlink is a name that is taken, and
        // "there is no golden file" would be the wrong thing to say about it.
        if !is_present(path)? {
            anyhow::bail!(
                "no golden file at {} (hint: pass --golden or create <kb>/.groove-eval.yml)",
                path.display()
            );
        }
        let bytes = read_bounded(path, MAX_GOLDEN_FILE_BYTES, "golden file")?;
        let gs: Self = serde_yaml_bw::from_slice(&bytes)
            .with_context(|| format!("failed to parse golden file: {}", path.display()))?;
        for q in &gs.queries {
            crate::db::parse_query(&q.query)
                .require_positive()
                .with_context(|| format!("golden query {:?} cannot be searched", query_id(q)))?;
        }
        Ok((gs, bytes))
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

    /// この run が測った index の状態 (AU-71)。
    ///
    /// **意図的に [`ConfigFingerprint`] の外に置いている。** fingerprint は
    /// [`History::previous_compatible`] の `PartialEq` に使われるので、corpus を
    /// そこに入れると **KB に文書を 1 つ足すたびに diff が無効化される**。
    /// この KB は自動 agent が毎日文書を足すため、それでは
    /// `--fail-on-regression` が恒久的に無力化される — 直そうとしたバグより悪い。
    /// `EvalRun` は `PartialEq` を derive していないので、ここに置けば
    /// **比較可能性はそのままで、変化を報告だけできる**。
    ///
    /// これが無いと何が起きるか: `golden_hash` は golden YAML のバイト列の
    /// hash **だけ**なので、文書を足しても fingerprint は不変。
    /// `previous_compatible` は「互換」と判定し、競合が増えて順位が動いただけの
    /// 差を `--fail-on-regression` が **retrieval regression として報告する**。
    /// AU-61 が `[contextual].enabled` について塞いだのと同じ穴の、corpus 版。
    ///
    /// この field を持たない旧 history JSON では `None`。`serde(default)` は
    /// 必須で、無いと [`History::load`] が deserialize 失敗を握り潰して
    /// **保存済みの baseline を全部捨てる**。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub corpus: Option<CorpusSnapshot>,

    pub per_query: Vec<QueryResult>,
    pub aggregate: AggregateMetrics,

    /// golden query の混入検出の所見 (feature-52)。通常は空。
    ///
    /// **意図的に [`ConfigFingerprint`] の外に置いている** — `corpus` と同じ理由
    /// (上記) で、所見が増減しただけで `previous_compatible` が「非互換」に
    /// 倒れると、`--fail-on-regression` の比較対象が消える。報告はするが
    /// 比較可能性は動かさない。
    ///
    /// この field を持たない旧 history JSON では空になる。**`serde(default)` は
    /// 必須** — 無いと [`History::load`] が deserialize 失敗を握り潰して
    /// **保存済みの baseline を全部捨てる**。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub findings: Vec<QuoteFinding>,
}

/// run 時点の index の状態 (AU-71)。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CorpusSnapshot {
    pub documents: u32,
    pub chunks: u32,
    /// 全 document の `path` + `content_hash` を path 順に並べた sha256。
    ///
    /// 件数だけでは足りない: この KB では agent が既存ファイルを**書き換える**
    /// ので、**件数が変わらないまま中身が変わる**のが普通に起きる。
    /// 件数だけを見ていると、その run に対して「corpus は変わっていない」と
    /// 嘘をつくことになる。
    ///
    /// 逆に digest だけでも足りない: `content_hash` は**ファイルのバイト列**の
    /// hash なので、chunk 分割の設定が変わっても動かない。その場合に動くのは
    /// `chunks` の方。だから変化の判定は 3 field 全体の `PartialEq` で行う。
    ///
    /// 保証ではなく best-effort である点に注意: `indexer.rs` には frontmatter
    /// のみの更新で `content_hash` を意図的に据え置く経路があり、そこでは
    /// 変化を取りこぼす。
    pub digest: String,
}

/// corpus の変化を 1 つの句で説明する。変化が無い / 判定できないなら `None`。
///
/// `None` が意味するのは 3 通り — 比較対象が無い、比較対象がこの field を持たない
/// 旧 run、実際に同一。**2 番目を「変わった」に丸めてはならない**: 丸めると、
/// baseline が 1 世代入れ替わるまでこの機能の出す信号が全部偽陽性になる。
///
/// `format_text` と `main.rs` の両方から呼ぶ。この条件分岐を 2 箇所に書き写すと
/// 必ず片方だけ直されて食い違う。
pub fn describe_corpus_change(
    now: Option<&CorpusSnapshot>,
    prev: Option<&CorpusSnapshot>,
) -> Option<String> {
    let (now, prev) = (now?, prev?);
    if now == prev {
        return None;
    }
    if now.documents == prev.documents && now.chunks == prev.chunks {
        return Some("same document and chunk counts, different contents".to_string());
    }
    Some(format!(
        "{} -> {} documents, {} -> {} chunks",
        prev.documents, now.documents, prev.chunks, now.chunks
    ))
}

/// `--k` も `[eval].k_values` も無いときに報告する k のリスト。
///
/// `eval` と `tune` の両方の解決 (`main.rs`) と、docs の出力例を formatter に
/// pin する test (`tests/docs_eval_transcript.rs`) がここを読む。3 箇所で別々に
/// `[1, 5, 10]` と書くと、既定を変えた日に test だけが古いラベルで緑のまま
/// docs と食い違う (codex P1 on PR #232)。
pub const DEFAULT_K_VALUES: [usize; 3] = [1, 5, 10];

/// `[eval].regression_threshold` が無いときの閾値。この幅を超えた低下が
/// 「劣化」で、超えない動きはノイズとして灰色で出る。
pub const DEFAULT_REGRESSION_THRESHOLD: f64 = 0.05;

/// 現行の metric 実装 version。recall / MRR / nDCG の計算式を修正するたびに
/// +1 する。[`ConfigFingerprint::metric_version`] を参照。
pub const METRIC_VERSION: u32 = 2;

fn legacy_metric_version() -> u32 {
    1
}

/// 現行の FTS クエリコンパイル規則の version。クエリ文字列から FTS5 の MATCH 式を
/// 作る規則 ([`crate::db::parse_query`]) の**出力が変わる**変更のたびに +1 する。
/// [`ConfigFingerprint::fts_query_version`] を参照。
///
/// [`METRIC_VERSION`] とは責務が違う。あちらは recall / MRR / nDCG の**計算式**専用で
/// 「同じ検索結果から違う数値が出る」ケース、こちらは**検索結果そのもの**が変わるケース。
///
/// - 2: feature-48 (v0.16.0) — クエリを文字種 token の OR 式にコンパイルする
/// - 3: feature-55 (v1.1.0) — group 先頭の `-` が除外になり、式が `(正) NOT (負)` を取り得る
pub const FTS_QUERY_VERSION: u32 = 3;

fn legacy_fts_query_version() -> u32 {
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

    /// FTS クエリのコンパイル規則の version (feature-48)。
    ///
    /// クエリ文字列を FTS5 の MATCH 式に変換する規則が変わると、**同じ index・同じ
    /// golden・同じ設定でも検索結果そのものが変わる**。旧 history JSON (field なし =
    /// serde default で 1) とは `PartialEq` 不一致になり
    /// [`History::previous_compatible`] の比較対象から自動的に外れるので、
    /// 規則変更が `--fail-on-regression` で retrieval regression として誤検出されない。
    ///
    /// mmr / parent_retriever / fusion / context が `Option` なのは「既定なら旧 baseline と
    /// 比較可能」を表すためだが、この規則には off の状態が無いので versioned int にしている。
    /// - v1: 初版〜v0.15.x (クエリ全体を単一 quoted phrase = 実質 verbatim 部分文字列検索)
    /// - v2: v0.16.0〜 (文字種 run に分割して OR 結合、全断片が短い場合は v1 の式へ fallback)
    #[serde(default = "legacy_fts_query_version")]
    pub fts_query_version: u32,

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

    /// index が context 付きで構築されている場合のみ Some (full-audit
    /// 2026-07-26 AU-61)。
    ///
    /// `[contextual].enabled` は chunk の embedding・FTS の格納内容・reranker
    /// 入力をすべて変える (切り替えには `--force` 再 index が要る) ので、
    /// 前後の run を比較するのは model を変えたのと同じくらい無意味。それでも
    /// model も golden_hash も同じままなので、この field が無いと
    /// [`History::previous_compatible`] が「互換」と判定し
    /// `--fail-on-regression` が両者を突き合わせてしまう。
    ///
    /// off (既定) と、mode を記録していない legacy DB は `None`。これにより
    /// **context を使っていない大多数の既存 history JSON とは PartialEq が
    /// 保たれる** (mmr / parent_retriever / fusion と同じ論理)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<ContextFingerprint>,
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

/// `index_meta.context_mode` の snapshot。context を使って index された場合のみ
/// [`ConfigFingerprint::context`] に Some で入る。値は
/// [`crate::db::ContextMode::as_str`] と同じ文字列 (`"static"`)。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ContextFingerprint {
    pub mode: String,
}

impl ConfigFingerprint {
    /// `groove.toml` Config と eval 実行時の引数から fingerprint を構築。
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
        // `[contextual].enabled` は index 時の設定なので、DB を持たないこの
        // 構築経路では toml の意図をそのまま写す。実際に index された mode を
        // 知っている `run()` は `index_meta.context_mode` を使う。
        let context = cfg
            .contextual
            .as_ref()
            .filter(|c| c.enabled)
            .map(|_| ContextFingerprint {
                mode: crate::db::ContextMode::Static.as_str().to_string(),
            });
        Self {
            model,
            reranker,
            limit,
            k_values,
            golden_hash,
            metric_version: METRIC_VERSION,
            fts_query_version: FTS_QUERY_VERSION,
            mmr,
            parent_retriever,
            fusion,
            context,
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

// ---------- Golden query の混入検出 (feature-52 / D-12) ----------
//
// 評価対象の KB と、評価について書き残す場所が同じコーパスだと、golden query の
// 文面を逐語で引用したノートが **そのクエリの上位を奪い、本来の正解を押し下げる**。
// 実測で発見した事故 (2026-07-26) だが、気付いたのは人間が結果を眺めていて偶然で、
// 問える手段が無かった。
//
// **なぜ「2 件以上」なのか** (実装前に 662 文書 / 26 golden で実測した):
//
// | 案 | 健全な KB での発火 |
// |---|---|
// | query と embedding 高類似 かつ expected でない | 上位 hit は定義上すべて高類似 = 閾値が引けない |
// | 逐語含有が 1 件でも報告 | **8 件、全部偽陽性** |
// | top_k の hit だけを見る + 2 件以上 | **0 件** (下記) |
// | コーパス全体 + 2 件以上 | **1 件、真陽性のみ** |
//
// 偽陽性の正体は、golden query の多くが `cross-encoder` のような **トピック名
// そのもの**で、その解説文書に逐語で出るのが当たり前だということ。1 件で報告する
// 規則に戻すと、健全な KB で 8 件鳴り続けて誰も読まなくなる。
//
// top_k に絞ると発火しないのは、2 件目の query の上位が 1 文書の chunk で
// 埋まってしまい、引用ノートがそもそも top_k に入らないため (実測: ある query では
// 9 位に入り、別の query では 1-10 位すべて別文書の chunk だった)。
// **実害が出ている範囲より広く探さないと、実害の原因を指させない。**

/// 照合対象にする query の最小長 (正規化後の **char 数**、byte 数ではない)。
///
/// これ未満の query は `MCP とは` のように多数の文書へ偶然含まれるので、
/// 「2 件以上」規則の分母を汚す。実測では **8 でも 12 でも報告は同じ 1 件**で、
/// **16 にすると真陽性が消える** (golden が短いキーワード主体のため)。
/// 両側から挟めているので設定キーにはせず、この 1 箇所に固定する。
pub const MIN_QUERY_CHARS: usize = 12;

/// 1 文書を所見にするのに必要な distinct な golden query の数。
pub const MIN_DISTINCT_QUERIES: usize = 2;

/// 所見の名前。**観測した事実**で名付けている: この検査が言えるのは
/// 「この文書は golden query を複数、逐語で含む」までで、それが混入なのか
/// golden の `expected` 漏れなのかは判定していない。
pub const CHECK_GOLDEN_QUERIES_QUOTED: &str = "golden-queries-quoted";

/// 逐語照合用の正規化: 連続する空白 (改行・タブ・全角空白を含む) を 1 つに
/// 畳んで前後を落とし、小文字化する。
///
/// **query 側と本文側の両方に同じ関数を当てる。** 別々に正規化すると、
/// 折り返しやインデントの違いだけで一致しなくなる。
fn normalize_for_quote(s: &str) -> String {
    s.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// レポートに出す query の識別子。`id` が無い golden では文面の先頭を使う。
///
/// [`run`] の per-query ループと所見の両方がこれを呼ぶ。2 箇所で別々に組み立てると
/// 同じ query が違う名前で出る。`tests/docs_eval_transcript.rs` も docs の出力例の
/// 行 id をここから作る (例が別の規則で id を書くのを防ぐ)。
pub fn query_id(q: &GoldenQuery) -> String {
    q.id.clone()
        .unwrap_or_else(|| q.query.chars().take(32).collect())
}

/// 走査対象になる query を `(golden 内の index, 正規化済み文面)` で返す。
/// 短すぎる query はここで落ちる。
///
/// **正規化後に同じ文面になる golden entry は 1 件にまとめる。** golden loader は
/// query 文の重複を弾かないので、大文字小文字や空白だけが違う 2 件をそのまま
/// 別々に数えると、**1 回の引用が 2 件に見えて 2 件閾値を自分で満たす** —
/// この閾値が防ぐために存在している偽陽性が、閾値の数え方から生まれる。
pub fn quote_needles(golden: &[GoldenQuery]) -> Vec<QuoteNeedle> {
    let mut needles: Vec<QuoteNeedle> = Vec::new();
    for (i, q) in golden.iter().enumerate() {
        let text = normalize_for_quote(&q.query);
        if text.chars().count() < MIN_QUERY_CHARS {
            continue;
        }
        match needles.iter_mut().find(|n| n.text == text) {
            Some(existing) => existing.golden.push(i),
            None => needles.push(QuoteNeedle {
                text,
                golden: vec![i],
            }),
        }
    }
    needles
}

/// 走査対象の query 1 件ぶん。正規化後の文面が同じ golden entry は
/// ここで 1 件に畳まれている ([`quote_needles`])。
#[derive(Debug, Clone, PartialEq)]
pub struct QuoteNeedle {
    /// 正規化済みの文面。
    pub text: String,
    /// この文面を持つ golden entry の index。先頭が報告に使う代表。
    /// 空にはならない。
    pub golden: Vec<usize>,
}

/// コーパス全体を 1 パス走査し、各文書が逐語で含んでいた needle を
/// **`needles` 内の index** で返す。1 つも含まない文書は返さない。
///
/// 照合は索引された**テキストフィールド単位** (chunk 本文・見出し・パンくずを
/// それぞれ別に見る)、集約は文書単位。理由は
/// [`crate::db::Database::for_each_indexed_text`] の doc comment を参照。
pub fn scan_quoted_documents(
    db: &crate::db::Database,
    needles: &[QuoteNeedle],
) -> Result<Vec<(String, Vec<usize>)>> {
    if needles.is_empty() {
        return Ok(Vec::new());
    }
    let mut found: std::collections::BTreeMap<String, std::collections::BTreeSet<usize>> =
        std::collections::BTreeMap::new();
    db.for_each_indexed_text(|path, text| {
        let hay = normalize_for_quote(text);
        for (ni, needle) in needles.iter().enumerate() {
            if hay.contains(needle.text.as_str()) {
                found.entry(path.to_string()).or_default().insert(ni);
            }
        }
    })?;
    Ok(found
        .into_iter()
        .map(|(path, set)| (path, set.into_iter().collect()))
        .collect())
}

/// 走査結果に規則を当てて所見にする。DB を触らない純粋関数。
///
/// `scan` が持つのは `needles` 内の index。`per_query` は `golden` と **同じ順**
/// であることを前提にする ([`run`] のループがそう作る)。順位の注記にしか
/// 使わないので、欠けていても所見は出る。
pub fn detect_quoted_queries(
    golden: &[GoldenQuery],
    needles: &[QuoteNeedle],
    scan: &[(String, Vec<usize>)],
    per_query: &[QueryResult],
) -> Vec<QuoteFinding> {
    let mut findings = Vec::new();
    for (path, quoted_indices) in scan {
        // **別の一致で説明が付く一致は、独立した証拠として数えない。**
        // golden に `cross-encoder reranking` と
        // `how does cross-encoder reranking work?` の両方があると、長い方を
        // 1 回引用しただけの文書が両方に一致し、**1 回の引用で 2 件閾値を
        // 満たす** — 正規化同値の重複で塞いだのと同じ穴が、文面が違うまま
        // 開いている (codex review round 5)。他の一致の部分文字列になっている
        // 一致を落とす。
        //
        // 出現位置を持たない以上、「長い方とは別の場所で短い方も引用されて
        // いる」場合を取りこぼす。**取りこぼす側に倒すのはこの検査の一貫した
        // 選択**で、2 件閾値そのものが同じ理由で存在している。
        // 正規化後に同一の文面は [`quote_needles`] が既に 1 件に畳んでいるので、
        // ここで比べる文面は互いに異なる = 「部分文字列」は真部分文字列。
        let independent: Vec<usize> = quoted_indices
            .iter()
            .copied()
            .filter(|&ni| {
                let Some(n) = needles.get(ni) else {
                    return false;
                };
                !quoted_indices.iter().any(|&other| {
                    other != ni
                        && needles
                            .get(other)
                            .is_some_and(|o| o.text.contains(n.text.as_str()))
                })
            })
            .collect();

        let mut quoted = Vec::new();
        for ni in independent {
            let Some(needle) = needles.get(ni) else {
                continue;
            };
            // その query の正解として挙がっている文書が query 語を含むのは
            // 当たり前で、混入ではない。数えると全 golden が所見になる。
            //
            // **免除は文書単位で、`expected` が heading を指していても変えない。**
            // 章単位に狭めると「正解の文書の *別の章* にトピック名が出ている」が
            // 数えられるようになるが、それは 1 件規則が実測 8 件の偽陽性を出した
            // 母集団そのもの (トピック名はその解説文書のあちこちに出る)。
            // 所見も 2 件閾値も文書単位である以上、免除だけ章単位にすると
            // 規則の粒度が揃わない。取りこぼす形は
            // 「heading 指定された正解文書の別章に、その query が引用されている」
            // で、これは承知の上の代償。
            //
            // 同じ文面の golden entry が複数あるときは、**どれか 1 つでもこの
            // 文書を正解にしていれば免除する**。文面が同じなら「この文書は
            // この言い回しの正解である」は等しく成り立つ。
            let exempt = needle.golden.iter().any(|&gi| {
                golden
                    .get(gi)
                    .is_some_and(|q| q.expected.iter().any(|e| &e.path == path))
            });
            if exempt {
                continue;
            }
            // 報告は代表 entry の名前と順位で行う。
            let Some(&gi) = needle.golden.first() else {
                continue;
            };
            let Some(q) = golden.get(gi) else { continue };
            let rank_in_top_k = per_query
                .get(gi)
                .and_then(|r| r.top_k.iter().find(|h| &h.path == path))
                .map(|h| h.rank);
            quoted.push(QuotedQuery {
                query_id: query_id(q),
                rank_in_top_k,
            });
        }
        if quoted.len() >= MIN_DISTINCT_QUERIES {
            findings.push(QuoteFinding {
                check: CHECK_GOLDEN_QUERIES_QUOTED.to_string(),
                path: path.clone(),
                quoted,
            });
        }
    }
    findings
}

/// 「この文書は golden query を複数、逐語で含む」という所見。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct QuoteFinding {
    /// 何を検査したか ([`CHECK_GOLDEN_QUERIES_QUOTED`])。将来別の検査が
    /// 同じ配列に入っても、消費側が種類で振り分けられるようにしている。
    pub check: String,
    pub path: String,
    pub quoted: Vec<QuotedQuery>,
}

/// 所見 1 件が含んでいた golden query 1 つぶんの内訳。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct QuotedQuery {
    pub query_id: String,
    /// その query の `top_k` に実際に出ていれば順位。`None` は「コーパスには
    /// 居るが、この run では上位に出ていない」= **まだ候補枠を奪っていない**。
    ///
    /// `None` でも key を落とさない (`skip_serializing_if` を付けない): これは
    /// 欠測ではなく**測った上での事実**で、`findings` を 0 件でも出すのと同じ
    /// 理由。key の不在で表すと、消費側が「この版は順位を見ていない」と
    /// 区別できなくなる。`serde(default)` は旧 JSON を読むためだけに要る。
    #[serde(default)]
    pub rank_in_top_k: Option<usize>,
}

/// 所見を人間向けの警告文にする。0 件なら `None`。
///
/// 文字列を返して **main.rs が stderr に出す**: 結果は stdout、診断は stderr
/// という CLI 出力規約に従いつつ、stderr を捕まえずに unit test できる。
///
/// **ASCII だけで書く。** これは stderr に出る = 日本語 Windows では CP932 の
/// コンソールに出るので、`⚠️` や `→` は化ける。stdout 側の [`format_text`] が
/// それらを使っているのは、あちらがリダイレクト前提の結果出力だから
/// (同じ判断が `main.rs` の `--fail-on-regression` の文言にもある)。
pub fn format_findings_warning(findings: &[QuoteFinding]) -> Option<String> {
    if findings.is_empty() {
        return None;
    }
    use std::fmt::Write;
    let mut s = String::new();
    writeln!(
        s,
        "groove eval: {} document(s) quote {} or more golden queries verbatim ({}).",
        findings.len(),
        MIN_DISTINCT_QUERIES,
        CHECK_GOLDEN_QUERIES_QUOTED
    )
    .unwrap();
    for f in findings {
        writeln!(s, "  {}", f.path).unwrap();
        for q in &f.quoted {
            let seen = match q.rank_in_top_k {
                Some(r) => format!("rank {r}"),
                None => "not in top_k".to_string(),
            };
            writeln!(s, "    {} ({})", q.query_id, seen).unwrap();
        }
    }
    // 原因を 1 つに決めつけない。どちらなのかは golden を書いた人しか知らない。
    writeln!(
        s,
        "  Either these notes leaked into the corpus, or the queries came from them"
    )
    .unwrap();
    writeln!(
        s,
        "  and the documents belong in `expected`. groove eval changes neither."
    )
    .unwrap();
    Some(s)
}

// ---------- History ----------

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct History {
    pub runs: VecDeque<EvalRun>,
}

impl History {
    /// JSON ファイルから履歴を読む。不在・破損時は warn を出して空 History を返す。
    ///
    /// **"Could not read it" and "read it, and it was not a history" are not
    /// the same answer.**
    ///
    /// An empty history is not inert: `groove eval` pushes the new run onto
    /// whatever this returns and saves the result over the same path, so
    /// answering "empty" for a file that is intact and unread **replaces every
    /// baseline with one run**, and `--fail-on-regression` then passes without
    /// having compared anything. The `corpus` field's own documentation
    /// records that hazard for the deserializing half; this is the reading
    /// half of it.
    ///
    /// So a file that could not be read at all — refused as a symlink, a hard
    /// link, something that is not a regular file, or over
    /// [`MAX_HISTORY_FILE_BYTES`] — is an **error**, and `eval` stops before
    /// it can save. `--no-history` is the way past it. Content that was read
    /// and does not parse keeps starting fresh: there was never a baseline in
    /// those bytes to lose.
    pub fn load(path: &Path) -> Result<Self> {
        // `symlink_metadata`, not `exists()`. A **dangling** symlink at this
        // name — a history kept on a volume that is not mounted right now —
        // makes `exists()` answer false, and "absent" is the one answer that
        // leads straight to `save` renaming a fresh file over the link and
        // disconnecting the baselines for good (codex P2 on PR #203). Asking
        // about the name rather than the target sends it to the checked read,
        // which refuses it and stops the run.
        if !is_present(path)? {
            return Ok(Self::default());
        }
        let bytes = read_bounded(path, MAX_HISTORY_FILE_BYTES, "eval history")?;
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

    /// Serialise, dropping the **oldest** runs until the reader will take it
    /// back.
    ///
    /// `history_size` bounds the run *count* and nothing bounded the bytes, so
    /// a large golden or a high `limit` could write a file
    /// [`Self::load`] then refuses — `groove eval` producing something only it
    /// can no longer read, and needing the user to delete a file it wrote
    /// before it would run again (codex P2 on PR #203).
    ///
    /// Retention by count is still the setting; this is the floor under it.
    /// Dropping the oldest is the same direction `push_front` already prunes
    /// in, so the run being compared against is the last to go.
    fn bytes_within(&self, cap: u64) -> Result<Vec<u8>> {
        /// The newest `keep` runs, serialised.
        fn encode(runs: &VecDeque<EvalRun>, keep: usize) -> Result<Vec<u8>> {
            let slice = History {
                runs: runs.iter().take(keep).cloned().collect(),
            };
            serde_json::to_vec_pretty(&slice).context("failed to serialize eval history")
        }

        let total = self.runs.len();
        if total == 0 {
            return encode(&self.runs, 0);
        }

        let whole = encode(&self.runs, total)?;
        if whole.len() as u64 <= cap {
            return Ok(whole);
        }

        // **Binary search, not one drop at a time.** Size is monotonic in run
        // count, so the largest count that fits can be found in log₂(n)
        // encodings. Dropping one at a time re-encodes the whole deque each
        // round: a history holding thousands of small runs plus one large one
        // would encode gigabytes on its way to the answer and look like a hang
        // (codex P2 on PR #203).
        let (mut lo, mut hi) = (1usize, total);
        let mut best: Option<Vec<u8>> = None;
        while lo <= hi {
            let mid = lo + (hi - lo) / 2;
            let bytes = encode(&self.runs, mid)?;
            if bytes.len() as u64 <= cap {
                best = Some(bytes);
                lo = mid + 1;
            } else if mid == 1 {
                break;
            } else {
                hi = mid - 1;
            }
        }

        let Some(bytes) = best else {
            // One run on its own is over the cap, so there is nothing left to
            // drop. Saying so now is better than writing a file the next run
            // refuses: the numbers for this run have already been printed.
            let one = encode(&self.runs, 1)?;
            anyhow::bail!(
                "a single eval run serialises to {} bytes, over the {cap} byte cap, so the \
                 history cannot be written in a form the next run could read. This means a \
                 very large golden set or a very high --limit. Reduce either, or pass \
                 --no-history to skip the file.",
                one.len()
            );
        };
        // `best` came from a `mid` that fit; recovering that count from the
        // bytes is not worth a second parse, so count what the search settled
        // on: `lo` is one past it.
        tracing::warn!(
            "eval history trimmed to {} run(s) to stay under the {cap} byte cap; \
             [eval].history_size asked for more",
            lo - 1
        );
        Ok(bytes)
    }

    /// atomic rename で書き出す。
    pub fn save(&self, path: &Path) -> Result<()> {
        let bytes = self.bytes_within(MAX_HISTORY_FILE_BYTES)?;
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

/// retrieval 品質が直前 run から退化したか判定する。F-40 で `groove eval
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
    // `corpus` は `run` / `previous` の中に載って出るが、両者を突き合わせるのを
    // 消費側に強いると「見落とす」= AU-71 で直したかった状態に戻る。判定済みの
    // bool を 1 つ出す。比較対象が無い (初回 run / 旧 history) 場合は null で、
    // false (= 変わっていない) と区別する。
    let corpus_changed = match (&run.corpus, previous.and_then(|p| p.corpus.as_ref())) {
        (Some(now), Some(prev)) => serde_json::json!(now != prev),
        _ => serde_json::Value::Null,
    };
    serde_json::json!({
        "timestamp": run.timestamp,
        "fingerprint": run.fingerprint,
        "corpus": run.corpus,
        "corpus_changed": corpus_changed,
        "aggregate": run.aggregate,
        "per_query": run.per_query,
        // 混入検出の所見 (feature-52)。0 件でも key は出す — 「検査していない」と
        // 「検査して 0 件だった」を消費側が区別できなくなるため。
        "findings": run.findings,
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
    writeln!(s, "groove eval — {}", run.timestamp.to_rfc3339()).unwrap();
    let rr = run.fingerprint.reranker.as_deref().unwrap_or("none");
    writeln!(
        s,
        "  model: {}    reranker: {}    limit: {}    queries: {}",
        run.fingerprint.model, rr, run.fingerprint.limit, run.aggregate.query_count
    )
    .unwrap();
    // corpus は banner に出す (AU-71)。**下の `match previous` の中に入れては
    // ならない** — あの match は「diff を無効化した理由」を 1 つだけ選ぶ構造で、
    // corpus は diff が有効なままでも報告する必要がある。むしろ diff が有効な
    // ときこそ「その低下は文書が増えたせいかもしれない」と伝える相手がいる。
    if let Some(c) = &run.corpus {
        writeln!(s, "  corpus: {} docs / {} chunks", c.documents, c.chunks).unwrap();
        if let Some(change) =
            describe_corpus_change(Some(c), previous.and_then(|p| p.corpus.as_ref()))
        {
            // 変化した側を `prev -> now` で明示する。括弧に数字だけを置くと
            // 「それが現在値」と読まれる。
            writeln!(s, "    ⚠️ corpus changed since last run ({change})").unwrap();
            writeln!(s, "       a delta below may reflect that, not retrieval").unwrap();
        }
    }
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

/// Default path for the history file: `<kb_path>/.groove-eval-history.json`.
pub fn default_history_path(kb_path: &Path) -> PathBuf {
    kb_path.join(".groove-eval-history.json")
}

/// eval が実際に使う `limit` と報告する `k` のリストを、検索経路の実効値に
/// 揃える。
///
/// `run_search_pipeline` は `limit` を [`crate::server::SEARCH_LIMIT_MAX`] に
/// clamp するので、eval のメタデータだけ生の値を持つと (codex P2 on PR #81)
/// `--limit 1001` と `--limit 2000` が同一 retrieval なのに fingerprint 違いで
/// history 比較から外れ、上限超えの `k` は「`@k` と表示されているが実際は
/// 上限件数から計算した」metric として無警告で出る。
///
/// ただし `k` は **グローバル上限に対してのみ** 丸める。要求 `limit` で丸めては
/// いけない — `max_k` は「要求された k が limit より大きければ取得深度を
/// 引き上げる」設計なので `--limit 5 --k 10` は 10 件取得して @10 を報告するのが
/// 正しい (codex P2 round 3 で一度この退行を入れた)。
///
/// 丸めで重複した k は sort + dedup する。`[1001, 2000]` が `[1000, 1000]` の
/// まま fingerprint に残ると、同一結果になる `[1000]` の run と history 互換に
/// ならないため。
pub fn normalize_eval_limit_and_k(limit: u32, k_values: &[usize]) -> (u32, Vec<usize>) {
    let cap = crate::server::SEARCH_LIMIT_MAX as usize;
    let limit = crate::server::clamp_search_limit(limit);
    let mut k_values: Vec<usize> = k_values.iter().map(|k| (*k).min(cap)).collect();
    k_values.sort_unstable();
    k_values.dedup();
    (limit, k_values)
}

/// Golden を読み、search_hybrid で評価し、EvalRun を返す。履歴書き込みは呼び出し側責務。
pub fn run(opts: &RunOpts) -> Result<EvalRun> {
    let (gs, golden_bytes) = GoldenSet::load_with_bytes(&opts.golden_path)?;
    let golden_hash = GoldenSet::hash_bytes(&golden_bytes);

    let db_path = crate::resolve_db_path(&opts.kb_path);
    if !db_path.exists() {
        anyhow::bail!(
            "No index found at {}. Run `groove index --kb-path {}` first.",
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
            "groove eval: note: limit {} exceeds the retrieval cap; using {limit}",
            opts.limit
        );
    }
    let max_k = k_values
        .iter()
        .copied()
        .max()
        .unwrap_or(10)
        .max(limit as usize);
    // ここから corpus を読み終えるまでを **1 つの read スナップショット**に固定
    // する (AU-71 review round 4)。WAL では文ごとにスナップショットが変わるので、
    // これが無いと `serve` の watcher が横で commit したとき、**検索は index の
    // 版 A と B を測り、記録には版 C が載る**。記録が「この数値を出した index」を
    // 指さなくなり、corpus 変化の注記が偽になったり出なくなったりする。
    //
    // DEFERRED なので実際のスナップショットは最初の read (= 最初の検索) で確定
    // する。`verify_embedding_meta` は index_meta に書き得るため、**その後**に
    // 開くこと。読み取り専用なので Drop の rollback で閉じてよい。
    //
    // 代償: eval の間 WAL の checkpoint が進まない。golden 数十件の run なら
    // 数秒〜数分で、その間に watcher が書いた分だけ WAL が伸びる。
    // 「数値がどの index のものか」を確定させる対価としては安い。
    let snapshot_tx = db.begin_transaction()?;

    let mut per_query = Vec::with_capacity(gs.queries.len());
    for q in &gs.queries {
        let qid = query_id(q);
        // The embedder sees what the caller is looking for, not what they
        // ruled out; the database gets the raw query and drops the excluded
        // rows itself. `load_with_bytes` has already refused a golden whose
        // query is nothing but exclusions, so `positive_text` is non-empty
        // here for the same reason it is at the other two entry points.
        let parsed = crate::db::parse_query(&q.query);
        let qe = embedder.embed_single(parsed.positive_text())?;
        // Eval shares the MMR-aware pipeline with MCP / CLI search so the
        // golden YAML reflects the actual production retrieval (e.g. when
        // `[search.mmr] enabled = true`).
        let pipeline = crate::server::run_search_pipeline(
            &db,
            reranker.as_mut(),
            &parsed,
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
    // context は index 時に確定する性質なので、toml の意図ではなく **DB に
    // 記録された実際の mode** を見る (AU-61)。off / 未記録 (legacy DB) は None。
    let context_fp = match db.read_context_mode()? {
        Some(crate::db::ContextMode::Static) => Some(ContextFingerprint {
            mode: crate::db::ContextMode::Static.as_str().to_string(),
        }),
        Some(crate::db::ContextMode::Off) | None => None,
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

    // corpus も context と同じく **index に記録された事実**を見る (AU-71)。
    // 3 値を個別に取らないのは、WAL では文ごとにスナップショットが変わり、
    // watcher が横で書いていると「どの時点にも存在しなかった index」を
    // 記録し得るため (`corpus_snapshot` の doc を参照)。
    let (documents, chunks, digest) = db.corpus_snapshot()?;
    let corpus = Some(CorpusSnapshot {
        documents,
        chunks,
        digest,
    });
    // 混入検出 (feature-52) も同じスナップショットの中で走らせる。**per-query
    // ループの外**なのは、「1 文書が golden query を 2 件以上含む」という規則が
    // 文書単位の集約を要求するため。検索と同じ index の版を見ていないと、
    // 「その run が測ったコーパス」ではないものを報告することになる。
    let needles = quote_needles(&gs.queries);
    let scan = scan_quoted_documents(&db, &needles)?;
    let findings = detect_quoted_queries(&gs.queries, &needles, &scan, &per_query);

    // 固定はここまで。read-only なので rollback は「何も書いていない」の宣言。
    snapshot_tx.rollback()?;

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
            fts_query_version: FTS_QUERY_VERSION,
            mmr: mmr_fp,
            parent_retriever: parent_fp,
            fusion: fusion_fp,
            context: context_fp,
        },
        corpus,
        per_query,
        aggregate,
        findings,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    // ---------- 混入検出 (feature-52) ----------

    fn golden_q(id: &str, query: &str, expected: &[&str]) -> GoldenQuery {
        GoldenQuery {
            id: Some(id.to_string()),
            query: query.to_string(),
            expected: expected
                .iter()
                .map(|p| ExpectedHit {
                    path: (*p).to_string(),
                    heading: None,
                })
                .collect(),
            tags: None,
        }
    }

    /// `top_k` に path を並べただけの結果 (順位の注記だけに使う)。
    fn result_with_hits(id: &str, paths: &[&str]) -> QueryResult {
        QueryResult {
            id: id.to_string(),
            query: String::new(),
            expected: Vec::new(),
            top_k: paths
                .iter()
                .enumerate()
                .map(|(i, p)| HitRecord {
                    rank: i + 1,
                    path: (*p).to_string(),
                    heading: None,
                    score: 1.0,
                })
                .collect(),
            metrics: QueryMetrics::default(),
        }
    }

    #[test]
    fn test_normalize_for_quote_collapses_whitespace_and_case() {
        // 引用は折り返されたりインデントされたりする。query 側と本文側に同じ
        // 正規化を当てるので、両者が一致する形に畳めていることを確かめる。
        assert_eq!(
            normalize_for_quote("  Claude Code\n   の\tcontext 管理  "),
            "claude code の context 管理"
        );
        // 全角空白も空白として畳む (日本語の散文で普通に出てくる)。
        assert_eq!(normalize_for_quote("A\u{3000}B"), "a b");
    }

    /// 最小長は **char 数**で数える。`.len()` (byte 数) で書くと、日本語 4 文字で
    /// 12 byte に達してしまい、短くて危険な query がそのまま照合対象になる。
    #[test]
    fn test_quote_needles_measures_the_minimum_in_chars_not_bytes() {
        let golden = vec![
            golden_q("short-ascii", "MCP とは", &[]),
            // 6 文字 / 18 byte。byte で数えると通ってしまう。
            golden_q("short-japanese", "日本語クエリ", &[]),
            // 13 文字。
            golden_q("long-japanese", "レコードユーザーの権限設計", &[]),
            golden_q("long-ascii", "cross-encoder reranking", &[]),
        ];
        assert!(
            "日本語クエリ".len() >= MIN_QUERY_CHARS,
            "前提: byte 数では通る"
        );

        let needles = quote_needles(&golden);
        let ids: Vec<Vec<usize>> = needles.iter().map(|n| n.golden.clone()).collect();
        assert_eq!(
            ids,
            vec![vec![2], vec![3]],
            "短い 2 件が落ちて長い 2 件だけが残る"
        );
        assert_eq!(needles[1].text, "cross-encoder reranking", "正規化して渡す");
    }

    /// 正規化後に同じ文面になる golden entry は 1 件に畳む。畳まないと、
    /// **1 回の引用が 2 件に数えられて 2 件閾値を自分で満たす** — この閾値が
    /// 防ぐために存在している偽陽性が、閾値の数え方から生まれる
    /// (codex review round 4)。golden loader は query 文の重複を弾かない。
    #[test]
    fn test_quote_needles_folds_queries_that_normalize_to_the_same_text() {
        let golden = vec![
            golden_q("a", "cross-encoder reranking", &[]),
            // 大文字小文字と空白だけが違う = 正規化すると同一。
            golden_q("a-dup", "Cross-Encoder   Reranking", &[]),
            golden_q("b", "torch.compile guide", &[]),
        ];
        let needles = quote_needles(&golden);
        assert_eq!(needles.len(), 2, "3 entry が 2 needle に畳まれる");
        assert_eq!(
            needles[0].golden,
            vec![0, 1],
            "重複は 1 つの needle に集まる"
        );
        assert_eq!(needles[1].golden, vec![2]);

        // その 1 文面を 1 回引用しただけの文書は所見にならない。
        let scan = vec![("notes/topic.md".to_string(), vec![0])];
        assert!(
            detect_quoted_queries(&golden, &needles, &scan, &[]).is_empty(),
            "one quotation of one wording must not satisfy a two-query threshold"
        );
    }

    /// 片方が他方の部分文字列になっている query は、長い方を 1 回引用すると
    /// 両方に一致する。正規化同値の重複 (round 4) と同じ穴が、文面が違うまま
    /// 開いていた形 (codex review round 5)。
    #[test]
    fn test_detect_quoted_queries_ignores_matches_explained_by_a_longer_match() {
        let golden = vec![
            golden_q("short", "cross-encoder reranking", &[]),
            golden_q("long", "how does cross-encoder reranking work?", &[]),
            golden_q("other", "torch.compile guide", &[]),
        ];
        let needles = quote_needles(&golden);
        assert_eq!(needles.len(), 3, "文面が違うので畳まれはしない");

        // 長い方を 1 回引用しただけ = 短い方も機械的に一致する。
        let scan = vec![("notes/topic.md".to_string(), vec![0, 1])];
        assert!(
            detect_quoted_queries(&golden, &needles, &scan, &[]).is_empty(),
            "one quotation cannot be two pieces of evidence just because one \
             golden query contains another"
        );

        // 独立した 2 件目 (包含関係にない query) があれば従来どおり報告する。
        let scan = vec![("notes/topic.md".to_string(), vec![0, 1, 2])];
        let findings = detect_quoted_queries(&golden, &needles, &scan, &[]);
        assert_eq!(findings.len(), 1);
        let ids: Vec<&str> = findings[0]
            .quoted
            .iter()
            .map(|q| q.query_id.as_str())
            .collect();
        assert_eq!(ids, vec!["long", "other"], "説明の付かない一致だけが残る");
    }

    /// 1 件しか含まない文書は報告しない。golden query の多くは `cross-encoder`
    /// のようなトピック名そのもので、その解説文書に逐語で出るのは当たり前。
    /// 1 件で報告する規則は実測で 8 件の偽陽性を出した。
    #[test]
    fn test_detect_quoted_queries_needs_more_than_one_query() {
        let golden = vec![
            golden_q("a", "cross-encoder reranking", &[]),
            golden_q("b", "torch.compile guide", &[]),
        ];
        let needles = quote_needles(&golden);
        let one = vec![("notes/topic.md".to_string(), vec![0])];
        assert!(detect_quoted_queries(&golden, &needles, &one, &[]).is_empty());

        let two = vec![("notes/about-the-eval.md".to_string(), vec![0, 1])];
        let findings = detect_quoted_queries(&golden, &needles, &two, &[]);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].check, CHECK_GOLDEN_QUERIES_QUOTED);
        assert_eq!(findings[0].path, "notes/about-the-eval.md");
        let ids: Vec<&str> = findings[0]
            .quoted
            .iter()
            .map(|q| q.query_id.as_str())
            .collect();
        assert_eq!(ids, vec!["a", "b"]);
    }

    /// 正解として挙がっている文書がその query 語を含むのは当たり前で、混入では
    /// ない。数えてしまうと、逐語一致を狙って書いた golden がまるごと所見になる。
    #[test]
    fn test_detect_quoted_queries_ignores_the_documents_a_query_expects() {
        let golden = vec![
            golden_q("a", "cross-encoder reranking", &["notes/both.md"]),
            golden_q("b", "torch.compile guide", &[]),
        ];
        // 2 件含んでいるが、片方は自分の正解なので 1 件ぶんしか数えない。
        let needles = quote_needles(&golden);
        let scan = vec![("notes/both.md".to_string(), vec![0, 1])];
        assert!(detect_quoted_queries(&golden, &needles, &scan, &[]).is_empty());
    }

    /// 免除は **`expected` が heading を指していても文書単位**。狭める案は
    /// codex review round 3 で提案されたが採らなかった (理由は
    /// `detect_quoted_queries` の該当箇所)。**意図的な選択なのでここで固定する。**
    #[test]
    fn test_detect_quoted_queries_exempts_the_whole_document_even_with_a_pinned_heading() {
        let golden = vec![
            GoldenQuery {
                id: Some("a".to_string()),
                query: "cross-encoder reranking".to_string(),
                expected: vec![ExpectedHit {
                    path: "notes/topic.md".to_string(),
                    heading: Some("Overview".to_string()),
                }],
                tags: None,
            },
            golden_q("b", "torch.compile guide", &[]),
        ];
        // その文書は query a の正解 (章まで指定) で、query b も引用している。
        // 章単位の免除にすると a も数えて 2 件 = 所見になるが、
        // 「正解の文書の別の章にトピック名が出ている」は混入の証拠ではない。
        let needles = quote_needles(&golden);
        let scan = vec![("notes/topic.md".to_string(), vec![0, 1])];
        assert!(
            detect_quoted_queries(&golden, &needles, &scan, &[]).is_empty(),
            "a document that answers the query is not evidence of a leak, \
             whichever section of it was labelled"
        );
    }

    /// 順位の注記は `top_k` から引く。`None` は「コーパスには居るが、この run では
    /// 上位に出ていない」= まだ候補枠を奪っていない、を意味する。
    #[test]
    fn test_detect_quoted_queries_annotates_the_rank_only_when_it_reached_top_k() {
        let golden = vec![
            golden_q("a", "cross-encoder reranking", &[]),
            golden_q("b", "torch.compile guide", &[]),
        ];
        let per_query = vec![
            result_with_hits("a", &["other.md", "notes/leak.md"]),
            result_with_hits("b", &["other.md"]),
        ];
        let needles = quote_needles(&golden);
        let scan = vec![("notes/leak.md".to_string(), vec![0, 1])];
        let findings = detect_quoted_queries(&golden, &needles, &scan, &per_query);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].quoted[0].rank_in_top_k, Some(2));
        assert_eq!(findings[0].quoted[1].rank_in_top_k, None);
    }

    #[test]
    fn test_format_findings_warning_is_none_when_there_is_nothing_to_report() {
        assert!(format_findings_warning(&[]).is_none());

        let text = format_findings_warning(&[QuoteFinding {
            check: CHECK_GOLDEN_QUERIES_QUOTED.to_string(),
            path: "notes/leak.md".to_string(),
            quoted: vec![
                QuotedQuery {
                    query_id: "a".to_string(),
                    rank_in_top_k: Some(9),
                },
                QuotedQuery {
                    query_id: "b".to_string(),
                    rank_in_top_k: None,
                },
            ],
        }])
        .expect("one finding reports");
        assert!(text.contains("notes/leak.md"));
        assert!(text.contains("rank 9"));
        assert!(text.contains("not in top_k"));
        // 原因を 1 つに決めつけない (混入か expected 漏れかは判定していない)。
        assert!(text.contains("expected"));
        // stderr に出る = 日本語 Windows では CP932 のコンソールに出るので、
        // 非 ASCII を混ぜると化ける。
        assert!(
            text.is_ascii(),
            "the stderr warning must stay ASCII: {text}"
        );
    }

    #[test]
    fn test_types_compile() {
        // 型が互いに整合していることの最小確認。後続 Task でテストを足していく。
        let _ = ExpectedHit {
            path: "x".into(),
            heading: None,
        };
    }

    fn write_yaml(name: &str, content: &str) -> PathBuf {
        // Suffix goes before the extension: `.yml` has to stay last.
        let path = std::env::temp_dir().join(format!(
            "{name}-{}.yml",
            crate::test_support::unique_suffix()
        ));
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

    /// A golden holding a query nobody can search is a broken golden, and it
    /// is refused at the read rather than scored as a miss.
    ///
    /// Both routes in are checked, because they are two functions and only one
    /// of them carries the loop: `tune` reaches the golden through
    /// [`GoldenSet::load`] and `eval` through [`GoldenSet::load_with_bytes`],
    /// exactly as `both_ways_into_the_golden_share_the_bound` says of the size
    /// cap. The context names the query, because a golden with forty entries
    /// and one bad line is otherwise a hunt.
    #[test]
    fn a_golden_query_without_a_positive_term_is_refused_at_load() {
        let path = write_yaml(
            "eval-golden-exclusion-only",
            "queries:\n\
             - id: \"findable\"\n  query: \"rust async\"\n  expected:\n  - path: \"a.md\"\n\
             - id: \"nothing-left\"\n  query: \"-foo\"\n  expected:\n  - path: \"b.md\"\n",
        );

        for msg in [
            format!(
                "{:#}",
                GoldenSet::load(&path).expect_err("the tune route must refuse it")
            ),
            format!(
                "{:#}",
                GoldenSet::load_with_bytes(&path).expect_err("the eval route must refuse it")
            ),
        ] {
            assert!(
                msg.contains("cannot be searched"),
                "the refusal must say the golden is unusable, not merely that \
                 something failed: {msg}"
            );
            assert!(
                msg.contains("nothing-left"),
                "the refusal must name the offending query: {msg}"
            );
            assert!(
                msg.contains("query has no positive term"),
                "the reason is the one sentence all three entry points share: {msg}"
            );
        }

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
            findings: Vec::new(),
            corpus: None,
            timestamp: Utc.timestamp_opt(ts_secs, 0).unwrap(),
            fingerprint: ConfigFingerprint {
                model: "bge-m3".into(),
                reranker: None,
                limit: 10,
                k_values: vec![1, 5, 10],
                golden_hash: "deadbeef".into(),
                metric_version: METRIC_VERSION,
                fts_query_version: FTS_QUERY_VERSION,
                mmr: None,
                parent_retriever: None,
                fusion: None,
                context: None,
            },
            per_query: vec![],
            aggregate: agg,
        }
    }

    #[test]
    fn test_history_load_missing_returns_empty() {
        let path = std::env::temp_dir().join("groove-hist-missing.json");
        let _ = std::fs::remove_file(&path);
        let h = History::load(&path).unwrap();
        assert!(h.runs.is_empty());
    }

    #[test]
    fn test_history_load_corrupt_returns_empty_with_warn() {
        // PID alone does not separate two tests in the same process, and both
        // this and the round-trip test below write history JSON to temp.
        let path = std::env::temp_dir().join(format!(
            "groove-hist-corrupt-{}.json",
            crate::test_support::unique_suffix()
        ));
        std::fs::write(&path, "{not json").unwrap();
        let h = History::load(&path).unwrap();
        assert!(h.runs.is_empty());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_history_save_and_reload_round_trip() {
        let path = std::env::temp_dir().join(format!(
            "groove-hist-rt-{}.json",
            crate::test_support::unique_suffix()
        ));
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
            findings: Vec::new(),
            corpus: None,
            timestamp: Utc::now(),
            fingerprint: ConfigFingerprint {
                model: "bge-m3".into(),
                reranker: None,
                limit: 10,
                k_values: vec![1, 5],
                golden_hash: "h".into(),
                metric_version: METRIC_VERSION,
                fts_query_version: FTS_QUERY_VERSION,
                mmr: None,
                parent_retriever: None,
                fusion: None,
                context: None,
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
            fts_query_version: FTS_QUERY_VERSION,
            mmr: None,
            parent_retriever: None,
            fusion: None,
            context: None,
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
            findings: Vec::new(),
            corpus: None,
            timestamp: Utc::now(),
            fingerprint: fp.clone(),
            per_query: vec![],
            aggregate: a_now,
        };
        let prev = EvalRun {
            findings: Vec::new(),
            corpus: None,
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
            fts_query_version: FTS_QUERY_VERSION,
            mmr: None,
            parent_retriever: None,
            fusion: None,
            context: None,
        };
        let fp_prev = ConfigFingerprint {
            golden_hash: "BBB".into(),
            ..fp_now.clone()
        };
        let mut agg = AggregateMetrics::default();
        agg.recall_at_k.insert(5, 0.8);
        agg.query_count = 1;
        let now = EvalRun {
            findings: Vec::new(),
            corpus: None,
            timestamp: Utc::now(),
            fingerprint: fp_now,
            per_query: vec![],
            aggregate: agg.clone(),
        };
        let prev = EvalRun {
            findings: Vec::new(),
            corpus: None,
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
            findings: Vec::new(),
            corpus: None,
            timestamp: Utc::now(),
            fingerprint: ConfigFingerprint {
                model: "bge-m3".into(),
                reranker: None,
                limit: 10,
                k_values: vec![1, 5],
                golden_hash: "abc".into(),
                metric_version: METRIC_VERSION,
                fts_query_version: FTS_QUERY_VERSION,
                mmr: None,
                parent_retriever: None,
                fusion: None,
                context: None,
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
            fts_query_version: FTS_QUERY_VERSION,
            mmr: None,
            parent_retriever: None,
            fusion: None,
            context: None,
        };
        let now = EvalRun {
            findings: Vec::new(),
            corpus: None,
            timestamp: Utc::now(),
            fingerprint: fp.clone(),
            per_query: vec![],
            aggregate: a1,
        };
        let prev = EvalRun {
            findings: Vec::new(),
            corpus: None,
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
            fts_query_version: FTS_QUERY_VERSION,
            mmr: None,
            parent_retriever: None,
            fusion: None,
            context: None,
        };
        let fp_prev = ConfigFingerprint {
            metric_version: 1,
            ..fp_now.clone()
        };
        let now = EvalRun {
            findings: Vec::new(),
            corpus: None,
            timestamp: Utc::now(),
            fingerprint: fp_now,
            per_query: vec![],
            aggregate: a1,
        };
        let prev = EvalRun {
            findings: Vec::new(),
            corpus: None,
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
            fts_query_version: FTS_QUERY_VERSION,
            mmr: None,
            parent_retriever: None,
            fusion: None,
            context: None,
        };
        let fp_prev = ConfigFingerprint {
            metric_version: 1,
            ..fp_now.clone()
        };
        let now = EvalRun {
            findings: Vec::new(),
            corpus: None,
            timestamp: Utc::now(),
            fingerprint: fp_now,
            per_query: vec![],
            aggregate: a1,
        };
        let prev = EvalRun {
            findings: Vec::new(),
            corpus: None,
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
            findings: Vec::new(),
            corpus: None,
            timestamp: Utc::now(),
            fingerprint: ConfigFingerprint {
                model: "bge-small-en-v1.5".into(),
                reranker: None,
                limit: 10,
                k_values: recall.keys().copied().collect(),
                golden_hash: golden_hash.into(),
                metric_version: METRIC_VERSION,
                fts_query_version: FTS_QUERY_VERSION,
                mmr: None,
                parent_retriever: None,
                fusion: None,
                context: None,
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

    /// Regression (codex P2 round 3 on PR #81): `k` を要求 `limit` に対して
    /// 丸めてはいけない。`max_k` は「k > limit なら取得深度を引き上げる」設計
    /// なので、`--limit 5 --k 10` は 10 件取得して @10 を報告するのが正しい。
    #[test]
    fn test_normalize_eval_keeps_k_above_the_requested_limit() {
        let (limit, k) = normalize_eval_limit_and_k(5, &[10]);
        assert_eq!(limit, 5);
        assert_eq!(
            k,
            vec![10],
            "k must not be clamped down to the requested limit"
        );
    }

    /// Regression (codex P2 round 3 on PR #81): 上限で丸めた結果できる重複は
    /// 潰す。`[1001, 2000]` が `[1000, 1000]` のまま fingerprint に残ると、
    /// 同一結果の `[1000]` run と history 互換にならない。
    #[test]
    fn test_normalize_eval_dedups_k_collapsed_by_the_cap() {
        let cap = crate::server::SEARCH_LIMIT_MAX as usize;
        let (_, collapsed) = normalize_eval_limit_and_k(10, &[1001, 2000]);
        let (_, plain) = normalize_eval_limit_and_k(10, &[cap]);
        assert_eq!(collapsed, vec![cap]);
        assert_eq!(
            collapsed, plain,
            "equivalent k lists must normalize identically"
        );
        // 順序も正規化する (同じ集合なら同じ fingerprint になるように)。
        let (_, unsorted) = normalize_eval_limit_and_k(10, &[10, 1, 5, 5]);
        assert_eq!(unsorted, vec![1, 5, 10]);
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

    /// full-audit 2026-08-12 テスト軸 H-6: `fts_query_version` の doc は
    /// 「`previous_compatible` の比較対象から自動的に外れる」と主張するが、
    /// feature-48 が足したのは `assert_ne!(fingerprint)` と serde 出力だけで、
    /// **互換判定そのものを通っていなかった** (`metric_version` には
    /// `test_previous_compatible_rejects_old_metric_version` がある = 非対称)。
    /// `previous_compatible` に短絡が入れば regression の誤検出が復活する。
    #[test]
    fn test_previous_compatible_rejects_old_fts_query_version() {
        let mut h = History::default();
        let mut prev = synthetic_run(map_one(5, 0.9), 0.9, map_one(10, 0.9), "golden_xyz");
        prev.fingerprint.fts_query_version = 1;
        let now = synthetic_run(map_one(5, 0.5), 0.5, map_one(10, 0.5), "golden_xyz");
        h.push_front(prev, 10);
        assert!(h.previous_compatible(&now).is_none());
    }

    /// 同じ世代どうしなら比較できること (上の test が「常に None」で通って
    /// しまわないことの対) 。
    #[test]
    fn test_previous_compatible_accepts_the_same_fts_query_version() {
        let mut h = History::default();
        let prev = synthetic_run(map_one(5, 0.9), 0.9, map_one(10, 0.9), "golden_xyz");
        let now = synthetic_run(map_one(5, 0.5), 0.5, map_one(10, 0.5), "golden_xyz");
        h.push_front(prev, 10);
        assert!(h.previous_compatible(&now).is_some());
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
    fn test_fingerprint_context_is_none_when_contextual_is_off() {
        // AU-61: 既定 (contextual off) なら None = feature-46 以前の history
        // JSON / 凍結 baseline と PartialEq 互換のまま。
        let cfg = crate::config::Config::default();
        let fp = ConfigFingerprint::from_config(
            &cfg,
            "bge-small-en-v1.5".into(),
            None,
            10,
            vec![1, 5, 10],
            "hash".into(),
        );
        assert!(
            fp.context.is_none(),
            "contextual off must not be recorded, so old baselines stay comparable"
        );

        let toml = "[contextual]
enabled = false
";
        let cfg: crate::config::Config = toml::from_str(toml).unwrap();
        let fp = ConfigFingerprint::from_config(
            &cfg,
            "bge-small-en-v1.5".into(),
            None,
            10,
            vec![1, 5, 10],
            "hash".into(),
        );
        assert!(fp.context.is_none(), "explicit off is still off");
    }

    #[test]
    fn test_fingerprint_context_on_is_incompatible_with_context_off() {
        // AU-61 の核: `[contextual].enabled` は embedding / FTS / reranker 入力を
        // すべて変える (= `--force` 再 index が要る) のに、model も golden_hash も
        // 変わらない。fingerprint に入っていないと `previous_compatible` が
        // 「比較可能」と判定し、`--fail-on-regression` が別物どうしを突き合わせる。
        let toml = "[contextual]
enabled = true
";
        let cfg: crate::config::Config = toml::from_str(toml).unwrap();
        let with_context = ConfigFingerprint::from_config(
            &cfg,
            "bge-small-en-v1.5".into(),
            None,
            10,
            vec![1, 5, 10],
            "hash".into(),
        );
        let ctx = with_context
            .context
            .as_ref()
            .expect("contextual on must be recorded");
        assert_eq!(ctx.mode, "static");

        let without_context = ConfigFingerprint::from_config(
            &crate::config::Config::default(),
            "bge-small-en-v1.5".into(),
            None,
            10,
            vec![1, 5, 10],
            "hash".into(),
        );
        assert_ne!(
            with_context, without_context,
            "runs on either side of a context-mode switch must not compare as compatible"
        );
    }

    #[test]
    fn test_fingerprint_without_context_field_deserializes() {
        // 旧 history JSON (context field なし) が読めて、かつ context off の
        // 現行 fingerprint と PartialEq 互換であること = serde(default)。
        let json = r#"{
            "model": "bge-small-en-v1.5",
            "reranker": null,
            "limit": 10,
            "k_values": [1, 5, 10],
            "golden_hash": "abc",
            "metric_version": 2
        }"#;
        let fp: ConfigFingerprint = serde_json::from_str(json).unwrap();
        assert!(fp.context.is_none());

        let mut now = ConfigFingerprint::from_config(
            &crate::config::Config::default(),
            "bge-small-en-v1.5".into(),
            None,
            10,
            vec![1, 5, 10],
            "abc".into(),
        );
        // feature-48: pre-feature-46 の history は fts_query_version も持たない (= v1)。
        // ここで見たいのは context の serde(default) なので、世代マーカーだけ揃えて
        // 比較する。世代が違えば非互換になること自体は
        // `test_fingerprint_without_fts_query_version_is_read_as_v1` が別に pin している。
        now.fts_query_version = 1;
        assert_eq!(
            fp, now,
            "a context-off run must stay comparable with pre-feature-46 history"
        );
    }

    #[test]
    fn test_fingerprint_context_serializes_only_when_present() {
        // skip_serializing_if: off の run が書き出す JSON に key を足さない
        // (= 既存 baseline ファイルとの diff / PartialEq を壊さない)。
        let off = ConfigFingerprint::from_config(
            &crate::config::Config::default(),
            "bge-small-en-v1.5".into(),
            None,
            10,
            vec![1, 5, 10],
            "abc".into(),
        );
        let v = serde_json::to_value(&off).unwrap();
        assert!(v.get("context").is_none(), "off must not serialize the key");

        let toml = "[contextual]
enabled = true
";
        let cfg: crate::config::Config = toml::from_str(toml).unwrap();
        let on = ConfigFingerprint::from_config(
            &cfg,
            "bge-small-en-v1.5".into(),
            None,
            10,
            vec![1, 5, 10],
            "abc".into(),
        );
        let v = serde_json::to_value(&on).unwrap();
        assert_eq!(v["context"]["mode"], "static");
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
            // JSON 側にも field が無い = serde default の 1。この test が見ているのは
            // fusion の default 互換なので、両辺を同じ世代に揃える。
            fts_query_version: 1,
            mmr: None,
            parent_retriever: None,
            fusion: None,
            context: None,
        };
        assert_eq!(fp, now);
    }

    #[test]
    fn test_fingerprint_without_fts_query_version_is_read_as_v1() {
        // feature-48 より前に書かれた history JSON は field を持たない。
        let json = r#"{
            "model": "bge-small-en-v1.5",
            "reranker": null,
            "limit": 10,
            "k_values": [1, 5, 10],
            "golden_hash": "abc",
            "metric_version": 2
        }"#;
        let old: ConfigFingerprint = serde_json::from_str(json).unwrap();
        assert_eq!(old.fts_query_version, 1);

        // v2 の run とは非互換 = 旧 baseline が比較対象から外れる。クエリの
        // コンパイル規則が変われば検索結果そのものが変わるので、これは
        // retrieval regression ではなく「世代が違う」として扱われなければならない。
        let mut now = old.clone();
        now.fts_query_version = FTS_QUERY_VERSION;
        assert_ne!(old, now);
    }

    #[test]
    fn test_fingerprint_always_serializes_fts_query_version() {
        // skip_serializing_if を付けていないので、新しい run は必ず値を書く。
        // 書かないと次回の run が「旧世代 = v1」と読んで誤って互換判定してしまう。
        let fp = ConfigFingerprint {
            model: "m".into(),
            reranker: None,
            limit: 10,
            k_values: vec![5],
            golden_hash: "h".into(),
            metric_version: METRIC_VERSION,
            fts_query_version: FTS_QUERY_VERSION,
            mmr: None,
            parent_retriever: None,
            fusion: None,
            context: None,
        };
        let v = serde_json::to_value(&fp).unwrap();
        assert_eq!(v["fts_query_version"], serde_json::json!(FTS_QUERY_VERSION));
    }

    /// feature-55 (F-4) の bump を忘れる mutation の**唯一**の検出器。
    ///
    /// 先頭 `-` が除外になったことで、同じクエリ文字列から出る MATCH 式が変わり得る =
    /// 検索結果そのものが変わる。bump しないと、規則が違う run どうしを eval が
    /// 比較可能とみなし、構文変更による差を retrieval regression として報告する。
    /// 既存のテストは値 2 を pin していないので、ここだけが気付ける。
    #[test]
    fn fts_query_version_covers_exclusion_syntax() {
        // `let` を 1 つ挟むのは `clippy::assertions_on_constants` を避けるためで、
        // 見ているのは const の値そのもの。
        let recorded = FTS_QUERY_VERSION;
        assert!(
            recorded >= 3,
            "the leading-hyphen exclusion syntax changes what a query compiles to, \
             so runs from before it must not be compared against runs from after it"
        );
    }

    // -----------------------------------------------------------------------
    // AU-71: the corpus is recorded and reported, but never gates comparability
    // -----------------------------------------------------------------------

    fn corpus(documents: u32, chunks: u32, digest: &str) -> CorpusSnapshot {
        CorpusSnapshot {
            documents,
            chunks,
            digest: digest.into(),
        }
    }

    /// A corpus change must NOT make two runs incomparable.
    ///
    /// This is the whole design decision in one assertion. Moving `corpus` into
    /// `ConfigFingerprint` — the obvious place for it — would make every KB
    /// edit disable the diff, and this knowledge base grows daily, so
    /// `--fail-on-regression` would never evaluate anything again. That failure
    /// is invisible: the gate just silently stops gating.
    #[test]
    fn test_corpus_change_does_not_disable_the_diff() {
        let mut now = sample_run(200, 0.50);
        let mut prev = sample_run(100, 0.90);
        now.corpus = Some(corpus(646, 11_215, "aaa"));
        prev.corpus = Some(corpus(642, 11_090, "bbb"));

        let mut h = History::default();
        h.push_front(prev.clone(), 10);
        assert!(
            h.previous_compatible(&now).is_some(),
            "a corpus change must leave the runs comparable"
        );

        // ...and the rendered diff must still be live, not just the predicate.
        let out = format_text(&now, Some(&prev), false, 0.05);
        assert!(
            !out.contains("diff disabled"),
            "corpus change must not disable the diff: {out}"
        );
        assert!(
            out.contains("↓"),
            "the 0.90 -> 0.50 drop must still be shown: {out}"
        );
    }

    #[test]
    fn test_format_text_names_a_corpus_size_change() {
        let mut now = sample_run(200, 0.90);
        let mut prev = sample_run(100, 0.90);
        now.corpus = Some(corpus(646, 11_215, "aaa"));
        prev.corpus = Some(corpus(642, 11_090, "bbb"));

        let out = format_text(&now, Some(&prev), false, 0.05);
        assert!(out.contains("646 docs / 11215 chunks"), "{out}");
        assert!(out.contains("corpus changed since last run"), "{out}");
        // Both sides must appear, in prev -> now order, or the reader cannot
        // tell which way it moved.
        assert!(out.contains("642 -> 646 documents"), "{out}");
        assert!(out.contains("11090 -> 11215 chunks"), "{out}");
    }

    /// Equal counts with different contents must still be reported.
    ///
    /// This is the case counts alone hide, and it is the common one here:
    /// research agents rewrite existing files in place, so the document count
    /// stays put while what the corpus *contains* changes underneath.
    #[test]
    fn test_format_text_names_a_content_only_corpus_change() {
        let mut now = sample_run(200, 0.90);
        let mut prev = sample_run(100, 0.90);
        now.corpus = Some(corpus(642, 11_090, "aaa"));
        prev.corpus = Some(corpus(642, 11_090, "bbb"));

        let out = format_text(&now, Some(&prev), false, 0.05);
        assert!(out.contains("corpus changed since last run"), "{out}");
        assert!(out.contains("different contents"), "{out}");
    }

    #[test]
    fn test_format_text_is_silent_when_the_corpus_is_unchanged() {
        let mut now = sample_run(200, 0.90);
        let mut prev = sample_run(100, 0.90);
        now.corpus = Some(corpus(642, 11_090, "same"));
        prev.corpus = Some(corpus(642, 11_090, "same"));

        let out = format_text(&now, Some(&prev), false, 0.05);
        assert!(out.contains("642 docs / 11090 chunks"), "{out}");
        assert!(
            !out.contains("corpus changed"),
            "an unchanged corpus must not be announced as changed: {out}"
        );
    }

    #[test]
    fn test_history_load_handles_old_json_without_corpus_field() {
        // 旧 history JSON (corpus field なし) が読めること = serde(default)。
        // これが無いと History::load が deserialize 失敗を握り潰して
        // **保存済みの baseline を全部捨てる** (失敗が warn 1 行にしか出ない)。
        let json = r#"{
            "runs": [{
                "timestamp": "2026-07-28T00:00:00Z",
                "fingerprint": {
                    "model": "bge-small-en-v1.5",
                    "reranker": null,
                    "limit": 10,
                    "k_values": [1, 5, 10],
                    "golden_hash": "abc",
                    "metric_version": 2
                },
                "per_query": [],
                "aggregate": {
                    "recall_at_k": {},
                    "ndcg_at_k": {},
                    "mrr": 0.0,
                    "query_count": 0
                }
            }]
        }"#;
        let h: History = serde_json::from_str(json).expect("old history JSON must still load");
        let prev = h.previous().expect("the run must survive the load");
        assert!(prev.corpus.is_none());

        // そして旧 run は今の run と「互換」のままでなければならない。
        let mut now = sample_run(200, 0.9);
        now.fingerprint = prev.fingerprint.clone();
        now.corpus = Some(corpus(646, 11_215, "aaa"));
        assert!(
            h.previous_compatible(&now).is_some(),
            "a run recorded before this field existed must still be comparable"
        );
    }

    /// feature-52 の `findings` にも `corpus` と同じ 2 つの性質が要る:
    /// ① field を持たない旧 history が読めること (無いと baseline を全部捨てる)
    /// ② 所見の有無が `previous_compatible` を左右しないこと (左右すると、
    ///    混入を報告した run だけ `--fail-on-regression` の比較対象を失う)。
    #[test]
    fn test_history_load_handles_old_json_without_findings_field() {
        let json = r#"{
            "runs": [{
                "timestamp": "2026-07-28T00:00:00Z",
                "fingerprint": {
                    "model": "bge-small-en-v1.5",
                    "reranker": null,
                    "limit": 10,
                    "k_values": [1, 5, 10],
                    "golden_hash": "abc",
                    "metric_version": 2
                },
                "per_query": [],
                "aggregate": {
                    "recall_at_k": {},
                    "ndcg_at_k": {},
                    "mrr": 0.0,
                    "query_count": 0
                }
            }]
        }"#;
        let h: History = serde_json::from_str(json).expect("old history JSON must still load");
        let prev = h.previous().expect("the run must survive the load");
        assert!(prev.findings.is_empty());

        let mut now = sample_run(200, 0.9);
        now.fingerprint = prev.fingerprint.clone();
        now.findings = vec![QuoteFinding {
            check: CHECK_GOLDEN_QUERIES_QUOTED.to_string(),
            path: "notes/leak.md".to_string(),
            quoted: vec![
                QuotedQuery {
                    query_id: "a".to_string(),
                    rank_in_top_k: Some(1),
                },
                QuotedQuery {
                    query_id: "b".to_string(),
                    rank_in_top_k: None,
                },
            ],
        }];
        assert!(
            h.previous_compatible(&now).is_some(),
            "reporting a finding must not cost the run its baseline"
        );
    }

    /// 0 件でも key を出す。key の不在で表すと、消費側は「検査していない古い
    /// 出力」と「検査して 0 件だった」を区別できない。
    #[test]
    fn test_format_json_always_carries_the_findings_key() {
        let run = sample_run(100, 0.9);
        let v = format_json(&run, None);
        assert_eq!(v["findings"], serde_json::json!([]));

        let mut with_finding = sample_run(200, 0.9);
        with_finding.findings = vec![QuoteFinding {
            check: CHECK_GOLDEN_QUERIES_QUOTED.to_string(),
            path: "notes/leak.md".to_string(),
            quoted: vec![
                QuotedQuery {
                    query_id: "a".to_string(),
                    rank_in_top_k: Some(9),
                },
                QuotedQuery {
                    query_id: "b".to_string(),
                    rank_in_top_k: None,
                },
            ],
        }];
        let v = format_json(&with_finding, None);
        assert_eq!(
            v["findings"][0]["check"],
            serde_json::json!("golden-queries-quoted")
        );
        assert_eq!(v["findings"][0]["path"], serde_json::json!("notes/leak.md"));
        assert_eq!(
            v["findings"][0]["quoted"][0]["rank_in_top_k"],
            serde_json::json!(9)
        );
        // 「top_k に居ない」は欠測ではなく測った結果なので、key を落とさず
        // null で出す (key の不在だと「順位を見ていない版」と区別できない)。
        assert_eq!(
            v["findings"][0]["quoted"][1]["rank_in_top_k"],
            serde_json::Value::Null
        );
        assert!(
            v["findings"][0]["quoted"][1]
                .as_object()
                .expect("quoted entry is an object")
                .contains_key("rank_in_top_k"),
            "the key itself must be present: {v}"
        );
    }

    #[test]
    fn test_format_json_reports_corpus_changed() {
        let mut now = sample_run(200, 0.9);
        let mut prev = sample_run(100, 0.9);
        now.corpus = Some(corpus(646, 11_215, "aaa"));
        prev.corpus = Some(corpus(642, 11_090, "bbb"));
        let v = format_json(&now, Some(&prev));
        assert_eq!(v["corpus_changed"], serde_json::json!(true));
        assert_eq!(v["corpus"]["documents"], serde_json::json!(646));

        prev.corpus = now.corpus.clone();
        let v = format_json(&now, Some(&prev));
        assert_eq!(v["corpus_changed"], serde_json::json!(false));

        // 比較対象が無いときは null。false (= 変わっていない) と混同させない。
        let v = format_json(&now, None);
        assert_eq!(v["corpus_changed"], serde_json::Value::Null);
    }

    // ---------- bounded reads (L-9) ----------

    /// A directory that cleans itself up, for the two files `eval` keeps.
    struct TempDir(PathBuf);
    impl TempDir {
        fn new(prefix: &str) -> Self {
            let p = crate::test_support::unique_temp_path(&format!("groove-eval-{prefix}"));
            std::fs::create_dir_all(&p).expect("create temp dir");
            Self(p)
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// A syntactically valid golden, one query long, padded past
    /// `MAX_EVAL_FILE_BYTES`.
    ///
    /// **Valid on purpose.** If the file were junk, removing the bound would
    /// still leave the load failing — at the parser — and the test would pass
    /// while the file was being read whole, which is the thing it exists to
    /// prevent. Padding one query keeps it parseable at any size.
    ///
    /// One `repeat` rather than a loop that appends until it passes the cap:
    /// the size follows the constant either way, and a loop that builds a
    /// gigabyte three lines at a time is a slow way to learn someone raised it.
    fn oversize_golden(path: &std::path::Path) {
        let pad = "a".repeat(usize::try_from(MAX_GOLDEN_FILE_BYTES).unwrap_or(0));
        let body =
            format!("queries:\n  - query: \"{pad}\"\n    expected:\n      - path: \"a.md\"\n");
        assert!(
            body.len() as u64 > MAX_GOLDEN_FILE_BYTES,
            "the fixture has to be over the cap for the test to mean anything"
        );
        std::fs::write(path, body).expect("write the oversize golden");
    }

    #[test]
    fn a_golden_over_the_cap_is_refused_rather_than_read() {
        let dir = TempDir::new("big-golden");
        let path = dir.0.join(".groove-eval.yml");
        oversize_golden(&path);

        let err = GoldenSet::load(&path).expect_err("an oversize golden must not be read");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("refusing to read"),
            "the refusal must say it refused, got: {msg}"
        );
        assert!(
            msg.contains("over the"),
            "the refusal must say the cap is why, got: {msg}"
        );
    }

    /// `tune` reaches the golden only through [`GoldenSet::load`], and `eval`
    /// only through [`GoldenSet::load_with_bytes`]. The bound has to be on the
    /// route both take, not on the one the audit happened to name — before
    /// this, `eval` had its own `fs::read` and `tune` had none.
    #[test]
    fn both_ways_into_the_golden_share_the_bound() {
        let dir = TempDir::new("shared-bound");
        let path = dir.0.join(".groove-eval.yml");
        oversize_golden(&path);

        assert!(GoldenSet::load(&path).is_err(), "the tune route");
        assert!(GoldenSet::load_with_bytes(&path).is_err(), "the eval route");
    }

    #[test]
    fn a_golden_under_the_cap_is_read_normally() {
        let dir = TempDir::new("small-golden");
        let path = dir.0.join(".groove-eval.yml");
        std::fs::write(
            &path,
            "queries:\n  - query: \"a question\"\n    expected:\n      - path: \"a.md\"\n",
        )
        .expect("write");

        let (gs, bytes) = GoldenSet::load_with_bytes(&path).expect("a small golden loads");
        assert_eq!(gs.queries.len(), 1);
        // The bytes are the ones the fingerprint is taken over, so they have to
        // be the file's, not a re-serialisation of the parsed form.
        assert_eq!(bytes, std::fs::read(&path).expect("read back"));
    }

    /// A real history with one run in it, padded past `cap` with a field
    /// `History` ignores, written to `path`.
    ///
    /// **The run is the point.** A fixture whose `runs` were already empty
    /// would come back empty whether the bound fired or not — measured: the
    /// first version of this test passed with the bound removed.
    fn oversize_history(path: &std::path::Path, cap: u64) {
        let mut real = History::default();
        real.push_front(sample_run(100, 0.5), 10);
        real.save(path).expect("save a real history");
        let saved = std::fs::read_to_string(path).expect("read it back");
        let pad = "x".repeat(usize::try_from(cap).unwrap_or(0));
        let padded = format!(
            "{{\"pad\":\"{pad}\",{}",
            saved
                .trim()
                .strip_prefix('{')
                .expect("history is an object")
        );
        assert!(padded.len() as u64 > cap);
        assert_eq!(
            serde_json::from_str::<History>(&padded)
                .expect("the padded fixture is still valid history")
                .runs
                .len(),
            1,
            "the fixture has to parse when it is allowed to, or the refusal \
             below could be the parser"
        );
        std::fs::write(path, &padded).expect("write the oversize history");
    }

    #[test]
    fn a_history_that_cannot_be_read_stops_the_run_instead_of_emptying_it() {
        // The golden refuses and the history used to start fresh. Starting
        // fresh is what makes this dangerous: `main.rs` pushes the new run
        // onto whatever `load` returns and saves it back over the same path,
        // so "empty" for a file that is intact and unread replaces every
        // baseline with one run — and `--fail-on-regression` then passes
        // without having compared anything. `?` at the call site is what stops
        // it, and `?` needs an `Err`.
        let dir = TempDir::new("unreadable-history");
        let path = dir.0.join(".groove-eval-history.json");
        oversize_history(&path, MAX_HISTORY_FILE_BYTES);

        let err = History::load(&path).expect_err("an unreadable history must not read as empty");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("refusing to read"),
            "the error must say it refused, got: {msg}"
        );

        // Nothing was written over it. Reading does not write, so this is the
        // weaker half — the strong half is that `load` returned `Err` at all,
        // because `main.rs` reaches `save` only past a `?`.
        assert!(
            std::fs::metadata(&path).expect("still there").len() as u64 > MAX_HISTORY_FILE_BYTES,
            "the history must be left exactly as it was"
        );
    }

    /// A dangling link is a name that is taken, and "absent" is the dangerous
    /// answer.
    ///
    /// `Path::exists` follows the link and says no, which sends `eval` down
    /// the empty-history path — and `save` then renames a fresh file **over
    /// the link**, disconnecting whatever it pointed at (codex P2 on
    /// PR #203). Asking about the name instead sends it to the checked read,
    /// which refuses it and stops the run.
    ///
    /// Unix only: creating a symlink on Windows needs a privilege that is not
    /// there by default, which is the same reason `links` guards the two
    /// platforms differently.
    #[cfg(unix)]
    #[test]
    fn a_dangling_history_link_is_refused_rather_than_read_as_absent() {
        let dir = TempDir::new("dangling-history");
        let path = dir.0.join(".groove-eval-history.json");
        let missing = dir.0.join("on-a-volume-that-is-not-mounted.json");
        if std::os::unix::fs::symlink(&missing, &path).is_err() {
            return;
        }
        assert!(
            !path.exists(),
            "the link must dangle for this to mean anything"
        );

        let err = History::load(&path).expect_err("a dangling link is not an absent history");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("refusing to read"),
            "the refusal must say it refused, got: {msg}"
        );
    }

    #[test]
    fn a_history_that_is_merely_not_history_still_starts_fresh() {
        // The other half of the distinction: these bytes were read, and they
        // hold no baseline to lose. `test_history_load_corrupt_returns_empty_
        // with_warn` covers the same rule from before this change; this one
        // states why the two answers differ.
        let dir = TempDir::new("not-history");
        let path = dir.0.join(".groove-eval-history.json");
        std::fs::write(&path, "{not json").expect("write");

        let history = History::load(&path).expect("unparseable content is not a read failure");
        assert!(history.runs.is_empty());
    }

    /// A history of `runs` runs, each carrying `queries` queries of `limit`
    /// hits — the shape that decides how big the file gets.
    fn history_of(runs: usize, queries: usize, limit: usize) -> History {
        let mut h = History::default();
        for i in 0..runs {
            let mut run = sample_run(1000 + i as i64, 0.9);
            run.per_query = (0..queries)
                .map(|q| QueryResult {
                    id: format!("q-{q}"),
                    query: "本番に出した変更を前のバージョンへ戻したい".to_string(),
                    expected: vec![ExpectedHit {
                        path: format!("ops/doc-{q}.ja.md"),
                        heading: None,
                    }],
                    top_k: (0..limit)
                        .map(|r| HitRecord {
                            rank: r,
                            path: format!("ops/doc-{r}.ja.md"),
                            heading: Some("切り戻しの手順".to_string()),
                            score: 0.0123,
                        })
                        .collect(),
                    metrics: QueryMetrics::default(),
                })
                .collect();
            h.push_front(run, runs);
        }
        h
    }

    #[test]
    fn whatever_save_writes_is_something_load_will_take_back() {
        // codex P2 on PR #203: `history_size` bounds the run count and nothing
        // bounded the bytes, so eval could write a file it then refused —
        // needing the user to delete something eval itself produced before it
        // would run again.
        //
        // A small cap stands in for a large golden here. The relationship is
        // what matters, and driving it from the real constant would need a
        // 64 MiB fixture to say the same thing.
        let cap = 600 * 1024;
        let big = history_of(10, 100, 10);
        let one = history_of(1, 100, 10);
        // The cap has to sit **between** one run and ten, or this exercises the
        // other branch: below one run there is nothing to drop, and above ten
        // there is nothing to trim. Measured here at roughly 0.21 MiB and
        // 2.1 MiB.
        assert!(
            (serde_json::to_vec_pretty(&one).expect("serialize").len() as u64) < cap
                && serde_json::to_vec_pretty(&big).expect("serialize").len() as u64 > cap,
            "the fixture has to straddle the cap for this to mean anything"
        );

        let bytes = big
            .bytes_within(cap)
            .expect("a history that can be trimmed is written");
        assert!(
            bytes.len() as u64 <= cap,
            "written bytes are within the cap"
        );
        let back: History = serde_json::from_slice(&bytes).expect("what was written parses");
        assert!(
            !back.runs.is_empty() && back.runs.len() < big.runs.len(),
            "the oldest runs were dropped, not all of them: {} of {}",
            back.runs.len(),
            big.runs.len()
        );
        // Oldest first, so the run the next diff compares against survives.
        assert_eq!(
            back.runs.front().map(|r| r.timestamp),
            big.runs.front().map(|r| r.timestamp)
        );
    }

    #[test]
    fn a_single_run_over_the_cap_is_reported_rather_than_written() {
        // Nothing left to drop. Saying so now beats writing a file the next
        // run refuses — the numbers for this run have already been printed.
        let cap = 1024;
        let one = history_of(1, 100, 10);
        let err = one
            .bytes_within(cap)
            .expect_err("a single oversize run cannot be made to fit");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("--no-history"),
            "the error must name the way past it, got: {msg}"
        );
    }

    #[test]
    fn a_history_the_ordinary_way_round_is_well_under_the_cap() {
        // Why the history does not share the golden's megabyte. This shape —
        // 25 queries, 10 hits, 10 runs, this repo's own golden — serialises to
        // about 0.52 MiB here; with per-query metrics filled in as a real run
        // fills them, the same shape measured 0.598 MiB, and 25 queries at
        // `limit = 20` measured 1.049 MiB. A megabyte is inside the ordinary
        // range, not past it.
        let mut h = History::default();
        for i in 0..10 {
            let mut run = sample_run(1000 + i, 0.9);
            run.per_query = (0..25)
                .map(|q| QueryResult {
                    id: format!("q-{q}"),
                    query: "本番に出した変更を前のバージョンへ戻したい".to_string(),
                    expected: vec![ExpectedHit {
                        path: format!("ops/doc-{q}.ja.md"),
                        heading: None,
                    }],
                    top_k: (0..10)
                        .map(|r| HitRecord {
                            rank: r,
                            path: format!("ops/doc-{r}.ja.md"),
                            heading: Some("切り戻しの手順".to_string()),
                            score: 0.0123,
                        })
                        .collect(),
                    metrics: QueryMetrics::default(),
                })
                .collect();
            h.push_front(run, 10);
        }
        let size = serde_json::to_vec_pretty(&h).expect("serialize").len() as u64;
        assert!(
            size > MAX_GOLDEN_FILE_BYTES / 4,
            "if an ordinary history got this small, the argument for a \
             separate cap is gone and this should be revisited: {size} bytes"
        );
        assert!(
            size < MAX_HISTORY_FILE_BYTES / 8,
            "an ordinary history is nowhere near the history cap: {size} bytes"
        );
    }
}
