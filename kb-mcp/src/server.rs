use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use rmcp::handler::server::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{Implementation, ProtocolVersion, ServerCapabilities, ServerInfo};
use rmcp::schemars;
use rmcp::{ServerHandler, prompt_handler, tool, tool_handler, tool_router};
use serde::{Deserialize, Serialize};

use crate::db::{Database, SearchHit};
use crate::embedder::{Embedder, ModelChoice, Reranker, RerankerChoice};
use crate::graph::{self, GraphOptions, SeedStrategy};
use crate::parser::{ParserExt, Registry};
use crate::poison::{recover, recover_db, recover_db_try, recover_try};
use crate::{indexer, markdown};

// ---------------------------------------------------------------------------
// Server struct
// ---------------------------------------------------------------------------

/// Request-independent server state.
///
/// (BU-06) Split out of [`KbServer`] so a tool handler can hand the whole
/// thing to `spawn_blocking` with a single `Arc::clone`. Every handler body
/// is synchronous and unbounded — embedding inference, SQLite queries, a
/// full index rebuild — and running that directly on a tokio worker thread
/// starves the runtime: with one such call in flight on a single-worker
/// runtime `/healthz` took 651 ms, versus 0.9 ms once the same work ran on
/// the blocking pool. Saturating all workers (16 concurrent calls on a
/// 16-core box) stalled `/healthz` for 602 ms.
///
/// Note that a request timeout cannot substitute for this. `tower`'s
/// `Timeout` polls the inner future first and the deadline `Sleep` only
/// afterwards, so while the inner future owns its thread the `Sleep` is
/// never polled: a 200 ms deadline over an 800 ms thread-blocking body
/// returns `Ok` at 800 ms. The same deadline over an offloaded body elapses
/// at 208 ms. Offloading is what makes every other mitigation possible.
pub(crate) struct KbCore {
    /// watcher と共有するため `Arc<Mutex<_>>` で保持。
    db: Arc<Mutex<Database>>,
    embedder: Arc<Mutex<Embedder>>,
    /// HTTP トランスポートの service factory でセッションごとに
    /// `KbServer` を clone するため Arc 化。Option なのは reranker 無効のケース。
    reranker: Arc<Mutex<Option<Reranker>>>,
    rerank_by_default: bool,
    kb_path: PathBuf,
    /// `rebuild_index` ツールで markdown パース時に使う除外見出し。
    /// `None` のときは [`markdown::DEFAULT_EXCLUDED_HEADINGS`] を使う。
    exclude_headings: Option<Vec<String>>,
    /// `rebuild_index` ツールで walkdir 時にスキップするディレクトリ basename。
    exclude_dirs: Vec<String>,
    /// Quality filter: 既定の品質フィルタしきい値。`search` / graph で適用。
    /// 0.0 ならフィルタ無効。
    quality_threshold: f32,
    /// Best-practice resolver: `get_best_practice` のパス候補テンプレート。
    /// 先頭から順に `{target}` を置換してファイルを探し、最初に存在した
    /// ものを読む。kb-mcp.toml 未指定時は legacy 既定
    /// `["best-practices/{target}/PERFECT.md"]`。
    best_practice_templates: Vec<String>,
    /// Parser registry: index 対象の拡張子レジストリ。`rebuild_index` MCP ツール
    /// から `indexer::rebuild_index` に渡す。`kb-mcp.toml` の
    /// `[parsers].enabled` が無ければ `Registry::defaults()` = `["md"]` のみ。
    /// watcher とも共有するため Arc。
    parser_registry: Arc<Registry>,
    /// `search` ツール既定の rank-based low_confidence ratio 閾値。
    /// 0.0 = 判定無効。SearchParams.min_confidence_ratio が指定されたら override。
    min_confidence_ratio: f32,
    /// `[search]` セクション (toml) のスナップショット。MMR / parent_retriever
    /// の per-call override 解決時に `SearchOverrides::resolve(&search_config)`
    /// で参照する。toml に section が無ければ `SearchConfig::default()` (MMR off)。
    search_config: crate::config::SearchConfig,
    /// feature-46: `rebuild_index` MCP tool の force 時 adopt 値 (§4.8)。
    context_mode_desired: crate::db::ContextMode,
    /// Shared indexing-state slot — `rebuild_index` flips it Some/None so
    /// `/api/admin/status` (= `KbServerShared.indexing_state`) reflects the
    /// in-process index operation (codex P2 round 1 on PR #57).
    indexing_state: Arc<Mutex<Option<IndexingState>>>,
}

/// The rmcp tool surface. Holds nothing but an `Arc` to the state, so the
/// per-session clones the HTTP service factory makes stay cheap and a
/// handler can move a reference to [`KbCore`] onto the blocking pool.
pub struct KbServer {
    core: Arc<KbCore>,
    /// Unused by rmcp 1.4 — `#[tool_handler]` expands to
    /// `Self::tool_router().call(..)`, i.e. it calls the generated *static*
    /// fn and never reads this field. Kept so the struct still records that
    /// a router belongs to it.
    #[allow(dead_code)]
    tool_router: ToolRouter<Self>,
}

// ---------------------------------------------------------------------------
// Tool parameter types
// ---------------------------------------------------------------------------

#[derive(Deserialize, schemars::JsonSchema, Default)]
#[schemars(transform = crate::schema_compat::ClientCompat)]
struct SearchParams {
    /// The search query text
    query: String,
    /// Maximum number of results to return (default: 5)
    limit: Option<u32>,
    /// Filter by category (legacy, single value; e.g. "deep-dive",
    /// "ai-news", "tech-watch"). Prefer `path_globs` / `tags_any` /
    /// `tags_all` for new clients.
    category: Option<String>,
    /// Filter by topic (legacy, single value; e.g. "mcp", "chromadb").
    /// Prefer `path_globs` / `tags_any` / `tags_all` for new clients.
    topic: Option<String>,
    /// Override the server default for reranking. Requires the server to have
    /// been started with `--reranker <model>` (otherwise ignored).
    rerank: Option<bool>,
    /// Override the quality filter threshold for this query (0.0-1.0). If
    /// omitted, the server default (from `kb-mcp.toml` / CLI) is used.
    min_quality: Option<f32>,
    /// If true, disable the quality filter for this query (equivalent to
    /// `min_quality: 0.0`, but more explicit).
    include_low_quality: Option<bool>,

    // ----- structured filter set (path / tags / date) -----
    /// Path glob patterns. `!` prefix marks an exclude pattern,
    /// e.g. `["docs/**", "!docs/draft/**"]`. An empty array `[]`
    /// is rejected — pass `null` (omit the field) to disable, or
    /// `["**", "!a/**"]` to express exclude-only intent.
    path_globs: Option<Vec<String>>,
    /// Hit passes if it carries any of these tags (OR semantics).
    tags_any: Option<Vec<String>>,
    /// Hit passes only if it carries every one of these tags (AND).
    tags_all: Option<Vec<String>>,
    /// Inclusive lower bound on `frontmatter.date` (lexicographic, ISO-8601 friendly).
    date_from: Option<String>,
    /// Inclusive upper bound on `frontmatter.date` (lexicographic, ISO-8601 friendly).
    date_to: Option<String>,

    // ----- low-confidence cutoff -----
    /// Rank-based ratio threshold for trimming low-confidence tail results.
    /// `null` falls back to the server default (`kb-mcp.toml` / CLI);
    /// `0.0` disables the cutoff for this query.
    min_confidence_ratio: Option<f32>,

    // ----- MMR / Parent retriever (per-call overrides) -----
    /// (v0.7.0+) Enable MMR diversity re-rank. When `null`, falls back to
    /// `[search.mmr].enabled` from kb-mcp.toml. Setting `true` / `false`
    /// per call overrides the toml default for that call.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    mmr: Option<bool>,

    /// (v0.7.0+) MMR lambda (relevance vs. diversity tradeoff). Must be in
    /// `[0.0, 1.0]`; values outside that range are rejected. `1.0` is
    /// equivalent to MMR off; lower values lean toward exploration. When
    /// `null`, falls back to `[search.mmr].lambda` from kb-mcp.toml.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    mmr_lambda: Option<f32>,

    /// (v0.7.0+) Extra cost when an already-selected chunk lives in the
    /// same document. Must be in `[0.0, 1.0]`. `0.0` is pure MMR; raise to
    /// actively deduplicate same-document chunks. When `null`, falls back
    /// to `[search.mmr].same_doc_penalty`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    mmr_same_doc_penalty: Option<f32>,

    /// (v0.7.0+) Enable parent retriever content expansion. When `true`,
    /// short hit chunks are expanded to adjacent siblings or the whole
    /// document; the score, rank, path, and `match_spans` of the hit are
    /// preserved (only `content` and the new `expanded_from` field
    /// change). When `null`, falls back to
    /// `[search.parent_retriever].enabled`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    parent_retriever: Option<bool>,
}

impl From<&SearchParams> for crate::config::SearchOverrides {
    fn from(p: &SearchParams) -> Self {
        Self {
            mmr: p.mmr,
            mmr_lambda: p.mmr_lambda,
            mmr_same_doc_penalty: p.mmr_same_doc_penalty,
            parent_retriever: p.parent_retriever,
        }
    }
}

#[derive(Deserialize, schemars::JsonSchema, Default)]
#[schemars(transform = crate::schema_compat::ClientCompat)]
struct GetDocumentParams {
    /// Relative path to the document within knowledge-base/ (e.g. "deep-dive/mcp/overview.md")
    path: String,
}

#[derive(Deserialize, schemars::JsonSchema, Default)]
#[schemars(transform = crate::schema_compat::ClientCompat)]
struct GetBestPracticeParams {
    /// Target name (e.g. "claude-code")
    target: String,
    /// Optional: extract only this h2 section (case-insensitive match)
    category: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema, Default)]
#[schemars(transform = crate::schema_compat::ClientCompat)]
struct RebuildIndexParams {
    /// Force full re-index ignoring existing hashes
    force: Option<bool>,
}

#[derive(Deserialize, schemars::JsonSchema, Default)]
#[schemars(transform = crate::schema_compat::ClientCompat)]
struct GetConnectionGraphParams {
    /// Relative path of the starting document within knowledge-base/
    /// (e.g. "deep-dive/mcp/overview.md"). Must be already indexed.
    path: String,
    /// BFS depth. 1 = direct neighbors only, 2 = neighbors of neighbors (default: 2, max: 3)
    depth: Option<u32>,
    /// Max neighbors fanned out per node at each hop (default: 5, max: 20)
    fan_out: Option<u32>,
    /// Minimum cosine similarity (0.0-1.0) for a neighbor to be included
    /// (default: 0.3). Lower = looser chain.
    min_similarity: Option<f32>,
    /// Seed strategy: "all_chunks" (default, expand from each seeded chunk of
    /// the start doc) or "centroid" (average their embeddings into a single
    /// seed node, so all of max_nodes except that one node is left for
    /// connections). Both read the first max_seed_chunks chunks only.
    seed_strategy: Option<String>,
    /// Filter by category (applied to all discovered nodes)
    category: Option<String>,
    /// Filter by topic
    topic: Option<String>,
    /// Paths to exclude from results. The start path itself is always excluded.
    exclude_paths: Option<Vec<String>>,
    /// If true, collapse same-path hits so each document appears at most once.
    /// Default: false (allow multiple chunks from the same doc).
    dedup_by_path: Option<bool>,
    /// Max nodes in the returned graph; also caps how many KNN queries the walk
    /// runs (default: 100, max: 2000). When it bites, the response carries
    /// `truncated: true` and a `truncation` entry with reason `node_budget`.
    max_nodes: Option<u32>,
    /// Max chunks of the start document read and used to seed the walk
    /// (default: 32, max: 1000). Raise it to cover more of a long start
    /// document. It bounds the read, so seed_strategy "centroid" averages the
    /// same capped prefix -- centroid frees the node budget for connections
    /// but does not recover chunks this cap dropped.
    max_seed_chunks: Option<u32>,
}

// ---------------------------------------------------------------------------
// Response types (serialized as JSON text)
// ---------------------------------------------------------------------------
//
// `search` ツールの出力形状は `db::SearchHit` に統一しているので、ここでは
// 個別に定義しない (CLI の `search` サブコマンドと schema 一致)。

#[derive(Serialize)]
struct TopicEntry {
    category: Option<String>,
    topic: Option<String>,
    file_count: u32,
    last_updated: Option<String>,
    titles: Vec<String>,
}

#[derive(Debug, Serialize)]
struct DocumentResponse {
    path: String,
    title: Option<String>,
    date: Option<String>,
    topic: Option<String>,
    tags: Vec<String>,
    content: String,
    /// 抽出テキストが 1 MiB を超え truncate された場合 true (既存応答は常に false)。
    #[serde(default)]
    truncated: bool,
}

#[derive(Serialize)]
struct BestPracticeResponse {
    target: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    category: Option<String>,
    content: String,
}

#[derive(Serialize)]
struct IndexStats {
    total_documents: u32,
    updated: u32,
    /// File-rename を検出して path だけ UPDATE した件数。
    #[serde(default)]
    renamed: u32,
    deleted: u32,
    /// disk 上に存在するが index されなかったファイル数 (read/size/parse 失敗・空本文)。
    #[serde(default)]
    skipped: u32,
    total_chunks: u32,
    duration_ms: u64,
}

#[derive(Serialize, Debug)]
pub(crate) struct ErrorResponse {
    error: String,
}

/// `search` MCP ツールの新出力 (feature-26、wrapper 形)。
#[derive(Serialize)]
struct SearchResponse {
    results: Vec<HitWithUri>,
    low_confidence: bool,
    /// 入力 filter のうち non-default のものだけ正規化後の値で echo back。
    filter_applied: SearchFilterEcho,
}

/// Whether a failed load says something about the **document** or about the
/// **server**.
///
/// `get_document` does not need the distinction — it answers with one JSON
/// error envelope either way. `resources/read` does: MCP gives it two codes,
/// and a client that cannot tell "there is no such resource" from "the index is
/// unreadable" will retry the wrong one, or stop retrying the one it should.
/// `list_resources` already reported a failed index query as an internal error,
/// so collapsing everything here also made the two disagree about the same
/// failure (codex P2 round 3 on PR #162).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LoadFailure {
    /// Nothing here to hand over: absent, or outside what this server serves.
    NotServed,
    /// The server could not answer. Not a claim about the document.
    Internal,
}

/// (feature-50) A hit, plus the `kb://doc/...` URI that names its document as a
/// resource.
///
/// Flattened, so the hit's own fields keep the shape and the position they had
/// — one new key, nothing moved. The MCP result stays a single text content
/// block carrying this JSON, which is what keeps every existing client working:
/// adding a `resource_link` content block instead would have changed the length
/// of the `content` array.
///
/// The specification permits handing back links to documents that
/// `resources/list` never enumerated, which is what makes the topic-group
/// listing and per-document addressing coexist.
///
/// The key is **omitted** for a hit whose extension the active parser registry
/// no longer covers. Such a row stays in the index on purpose (AU-06) and stays
/// in the search results, but neither `get_document` nor `resources/read` will
/// open it — so the honest answer is no link, not a broken one.
#[derive(Serialize)]
struct HitWithUri {
    #[serde(flatten)]
    hit: crate::db::SearchHit,
    #[serde(skip_serializing_if = "Option::is_none")]
    uri: Option<String>,
}

impl HitWithUri {
    fn new(hit: crate::db::SearchHit, registry: &Registry) -> Self {
        let uri = crate::indexer::extension_is_registered(&hit.path, registry)
            .then(|| crate::resources::doc_uri(&hit.path));
        Self { hit, uri }
    }
}

/// 入力 filter のうち non-default のものだけ echo。`null`/空配列の項目は
/// `skip_serializing_if` で JSON から省略される。
#[derive(Serialize, Default)]
struct SearchFilterEcho {
    #[serde(skip_serializing_if = "Option::is_none")]
    category: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    topic: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    path_globs: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tags_any: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tags_all: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    date_from: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    date_to: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    min_confidence_ratio: Option<f32>,
}

// ---------------------------------------------------------------------------
// Tool implementations
// ---------------------------------------------------------------------------

/// The synchronous bodies behind the tool surface.
///
/// (BU-06) Every method here blocks its thread — that is the point. They are
/// only ever reached through the `spawn_blocking` wrappers in
/// `impl KbServer` below, which is what keeps the tokio workers free.
impl KbCore {
    fn search_blocking(&self, params: SearchParams) -> String {
        // AU-01: 上限なしの `limit` は候補プール → `Vec::with_capacity` へ
        // 生で流れて allocation abort を起こす。MCP boundary で clamp する。
        let limit = clamp_search_limit(params.limit.unwrap_or(5));

        // feature-28 Task 2.7: per-call MMR override の範囲チェック。
        // 1.5 / -0.1 等の outside-range は MCP boundary で early reject し、
        // resolve / mmr_select に届ける前に弾く。NaN も `(0.0..=1.0).contains`
        // が false になるので同経路で reject される。
        if let Some(l) = params.mmr_lambda
            && !(0.0..=1.0).contains(&l)
        {
            return serde_json::to_string_pretty(&ErrorResponse {
                error: format!("mmr_lambda out of range: {l} (must be 0.0..=1.0)"),
            })
            .unwrap_or_default();
        }
        if let Some(p) = params.mmr_same_doc_penalty
            && !(0.0..=1.0).contains(&p)
        {
            return serde_json::to_string_pretty(&ErrorResponse {
                error: format!("mmr_same_doc_penalty out of range: {p} (must be 0.0..=1.0)"),
            })
            .unwrap_or_default();
        }

        // F-35: query length cap (1 KiB)。上限超えは early reject。
        // embedder / FTS5 layer の内部 truncate に任せる手もあるが、上流で
        // reject した方が「なぜ結果が変なのか」分かりやすく、`compute_match_spans`
        // の O(N×M) cost も query 側から抑制できる。
        if params.query.len() > SEARCH_QUERY_MAX_BYTES {
            return serde_json::to_string_pretty(&ErrorResponse {
                error: format!(
                    "query is too large: {} bytes (max {SEARCH_QUERY_MAX_BYTES} bytes). \
                     For long-form retrieval, slice the query or use multiple smaller calls.",
                    params.query.len()
                ),
            })
            .unwrap_or_default();
        }

        // AU-17: list 型 filter の件数・要素長の上限。`query` にだけ cap が
        // あって、同じリクエストに載る list は無制限という非対称を埋める。
        // 3 つの上限をここで並べて読めるようにしてある (`path_globs` は
        // `compile_path_globs` の内側でも検査され、そちらが CLI を守る)。
        for (name, items) in [
            ("path_globs", params.path_globs.as_deref().unwrap_or(&[])),
            ("tags_any", params.tags_any.as_deref().unwrap_or(&[])),
            ("tags_all", params.tags_all.as_deref().unwrap_or(&[])),
        ] {
            if let Err(e) = validate_filter_list(name, items) {
                return serde_json::to_string_pretty(&ErrorResponse {
                    error: e.to_string(),
                })
                .unwrap_or_default();
            }
        }

        // path_globs を事前 compile。エラー時は ErrorResponse を返却。
        let cpg = match params.path_globs.as_ref() {
            Some(globs) => match compile_path_globs(globs) {
                Ok(c) => Some(c),
                Err(e) => {
                    return serde_json::to_string_pretty(&ErrorResponse {
                        error: format!("invalid path_globs: {e}"),
                    })
                    .unwrap_or_default();
                }
            },
            None => None,
        };

        // query embedding
        let query_embedding = {
            let mut embedder = recover(self.embedder.lock(), "embedder");
            match embedder.embed_single(&params.query) {
                Ok(emb) => emb,
                Err(e) => {
                    return serde_json::to_string_pretty(&ErrorResponse {
                        error: format!("Failed to embed query: {e}"),
                    })
                    .unwrap_or_default();
                }
            }
        };

        let mut reranker_guard = recover(self.reranker.lock(), "reranker");
        let use_rerank =
            params.rerank.unwrap_or(self.rerank_by_default) && reranker_guard.is_some();

        let effective_min_quality = crate::quality::resolve_effective_threshold(
            params.include_low_quality.unwrap_or(false),
            params.min_quality,
            self.quality_threshold,
        );

        let tags_any: &[String] = params.tags_any.as_deref().unwrap_or(&[]);
        let tags_all: &[String] = params.tags_all.as_deref().unwrap_or(&[]);

        let filters = crate::db::SearchFilters {
            category: params.category.as_deref(),
            topic: params.topic.as_deref(),
            min_quality: effective_min_quality,
            path_globs: cpg.as_ref(),
            tags_any,
            tags_all,
            date_from: params.date_from.as_deref(),
            date_to: params.date_to.as_deref(),
        };

        // feature-28 Task 2.9: MMR / parent_retriever の effective config を解決し、
        // 共有の MMR-aware パイプラインに渡す。per-call mmr_lambda /
        // mmr_same_doc_penalty の range check は上で済ませてあるが、
        // run_search_pipeline 側でも belt-and-suspenders で再検証される。
        let overrides: crate::config::SearchOverrides = (&params).into();

        let db = recover_db(self.db.lock());
        let reranker_arg: Option<&mut Reranker> = if use_rerank {
            Some(
                reranker_guard
                    .as_mut()
                    .expect("reranker Some checked above"),
            )
        } else {
            None
        };

        let after_mmr = match run_search_pipeline(
            &db,
            reranker_arg,
            &params.query,
            &query_embedding,
            limit,
            &filters,
            &overrides,
            &self.search_config,
        ) {
            Ok(r) => r,
            Err(e) => {
                return serde_json::to_string_pretty(&ErrorResponse {
                    error: format!("Search failed: {e}. Try running rebuild_index first."),
                })
                .unwrap_or_default();
            }
        };

        // chunk_id を維持したまま SearchHit に変換 (Parent retriever 用)。
        // Parent retriever は relevance を変えないので scores は元 chunk
        // (= 拡張前) のもので確定させる。
        let hits_with_id: Vec<(i64, crate::db::SearchHit)> = after_mmr
            .into_iter()
            .map(|(id, sr)| (id, sr.into()))
            .collect();

        let scores: Vec<f32> = hits_with_id.iter().map(|(_, h)| h.score).collect();

        let effective_ratio = match params.min_confidence_ratio {
            Some(v) if v.is_finite() => v.max(0.0),
            Some(_) => {
                tracing::warn!(
                    "min_confidence_ratio={:?} is not finite; falling back to server default",
                    params.min_confidence_ratio
                );
                self.min_confidence_ratio
            }
            None => self.min_confidence_ratio,
        };
        let low_confidence = compute_low_confidence(&scores, effective_ratio);

        // Parent retriever 段。enabled = false なら chunk_id を剥がすだけで
        // content / expanded_from は触らない (= v0.6.1 と bit-exact 互換)。
        let resolved = overrides.resolve(&self.search_config);
        let parent_params = crate::parent::ParentRetrieverParams {
            whole_doc_threshold_tokens: resolved.parent_whole_doc_threshold_tokens,
            max_expanded_tokens: resolved.parent_max_expanded_tokens,
        };
        let mut hits: Vec<SearchHit> = crate::parent::apply_parent_retriever(
            hits_with_id,
            &db,
            resolved.parent_retriever_enabled,
            parent_params,
        );
        // match_spans は Parent retriever 拡張後の content に対して計算する
        // (`expand_parent` は defensive に None クリアするので必ず再計算が要る)。
        for h in &mut hits {
            h.match_spans = compute_match_spans(&params.query, &h.content);
        }

        let echo = SearchFilterEcho {
            category: params.category.clone(),
            topic: params.topic.clone(),
            path_globs: params.path_globs.clone().filter(|v| !v.is_empty()),
            tags_any: params.tags_any.clone().filter(|v| !v.is_empty()),
            tags_all: params.tags_all.clone().filter(|v| !v.is_empty()),
            date_from: params.date_from.clone(),
            date_to: params.date_to.clone(),
            min_confidence_ratio: params.min_confidence_ratio,
        };

        let resp = SearchResponse {
            results: hits
                .into_iter()
                .map(|h| HitWithUri::new(h, &self.parser_registry))
                .collect(),
            low_confidence,
            filter_applied: echo,
        };
        serde_json::to_string_pretty(&resp).unwrap_or_default()
    }

    fn list_topics_blocking(&self) -> String {
        let db = recover_db(self.db.lock());
        match db.list_topics() {
            Ok(topics) => {
                let entries: Vec<TopicEntry> = topics
                    .into_iter()
                    .map(|t| TopicEntry {
                        category: t.category,
                        topic: t.topic,
                        file_count: t.file_count,
                        last_updated: t.last_updated,
                        titles: t.titles,
                    })
                    .collect();
                serde_json::to_string_pretty(&entries).unwrap_or_default()
            }
            Err(e) => serde_json::to_string_pretty(&ErrorResponse {
                error: format!("Failed to list topics: {e}"),
            })
            .unwrap_or_default(),
        }
    }

    /// The indexed documents this server can actually hand over.
    ///
    /// Not the same set as `all_document_paths`. Narrowing `[parsers].enabled`
    /// without reindexing deliberately **keeps** those rows — `run_server` warns
    /// about them instead of deleting them, because a narrowed setting is often
    /// temporary — and `load_document_blocking` then refuses their extension.
    /// Advertising them would be offering a link the very next call rejects.
    ///
    /// So the filter lives on the one query the whole resource surface asks,
    /// rather than at each of the three places that emit a URI. That is the
    /// same reason `load_document_blocking` exists at all.
    fn servable_document_paths(&self) -> Result<Vec<String>, String> {
        let paths = {
            let db = recover_db(self.db.lock());
            db.all_document_paths()
                .map_err(|e| format!("failed to list indexed documents: {e}"))?
        };
        Ok(paths
            .into_iter()
            .filter(|p| crate::indexer::extension_is_registered(p, &self.parser_registry))
            .collect())
    }

    /// The topic groups `resources/list` answers from.
    ///
    /// Built from the servable paths, which is the same list a read is checked
    /// against — so a URI this listing offers cannot fail membership when the
    /// client reads it back.
    fn topic_groups_blocking(&self) -> Result<Vec<crate::resources::TopicGroup>, String> {
        self.servable_document_paths()
            .map(|paths| crate::resources::topic_groups(&paths))
    }

    /// What a `resources/read` of a document produces: its text, and the media
    /// type that text actually is.
    ///
    /// Not the media type of the file on disk. A PDF or a spreadsheet is served
    /// as the text the parser extracted, because that is what an MCP client can
    /// use, so calling it `application/pdf` would be a lie about the bytes it is
    /// holding.
    fn resource_mime_for(ext: &str) -> &'static str {
        if ext.eq_ignore_ascii_case("md") {
            "text/markdown"
        } else {
            "text/plain"
        }
    }

    /// Serve one `kb://` URI, or say why not. Returns `(text, mime)`.
    fn read_resource_blocking(
        &self,
        parsed: &crate::resources::ResourceUri,
        uri: &str,
    ) -> Result<(String, &'static str), (LoadFailure, String)> {
        let paths = self
            .servable_document_paths()
            .map_err(|e| (LoadFailure::Internal, e))?;

        match parsed {
            crate::resources::ResourceUri::Topic(prefix) => {
                let group = crate::resources::topic_groups(&paths)
                    .into_iter()
                    .find(|g| &g.prefix == prefix)
                    .ok_or_else(|| {
                        (
                            LoadFailure::NotServed,
                            format!("no such topic group: {uri}"),
                        )
                    })?;
                let mut out = format!("# {}\n\n{}\n\n", group.display_name(), group.description());
                for p in &group.paths {
                    out.push_str(&format!("- `{p}` — {}\n", crate::resources::doc_uri(p)));
                }
                Ok((out, "text/markdown"))
            }
            crate::resources::ResourceUri::Doc(rel) => {
                // Membership first. A document that is not indexed was never
                // offered, and `resources/read` is for what was offered — this
                // is strictly narrower than `get_document`, so it cannot widen
                // what is reachable.
                if !paths.iter().any(|p| p == rel) {
                    return Err((
                        LoadFailure::NotServed,
                        format!("not an indexed document: {uri}"),
                    ));
                }
                // Then the guards, by sharing the body `get_document` uses
                // rather than re-deriving them: symlink and hard-link refusal,
                // traversal, extension membership, size cap, handle-bound read.
                // A second sequence is how the two would come to disagree.
                let (doc, ext) = self
                    .load_document_blocking(rel)
                    .map_err(|(kind, e)| (kind, format!("{}: {uri}", e.error)))?;
                Ok((doc.content, Self::resource_mime_for(&ext)))
            }
        }
    }

    fn get_document_blocking(&self, params: GetDocumentParams) -> String {
        match self.load_document_blocking(&params.path) {
            Ok((doc, _ext)) => serde_json::to_string_pretty(&doc).unwrap_or_default(),
            // The category is for `resources/read`, which has two codes to
            // choose between. This tool has one envelope either way, so its
            // output is unchanged by carrying it.
            Err((_kind, e)) => serde_json::to_string_pretty(&e).unwrap_or_default(),
        }
    }

    /// Every guard a document has to clear, in one place, plus the extraction.
    ///
    /// Shared by `get_document` — which wraps the result in its JSON envelope —
    /// and `resources/read`, which returns the extracted text. Two call sites
    /// with two copies of this sequence is how a guard ends up applying to one
    /// of them; `max_bytes_for` exists for the same reason one level down.
    ///
    /// The extension is handed back because the caller needs it and it must be
    /// the **canonical** one the checks used, not the one from the requested
    /// path (BU-22: Windows 8.3 short names make those differ).
    fn load_document_blocking(
        &self,
        rel: &str,
    ) -> Result<(DocumentResponse, String), (LoadFailure, ErrorResponse)> {
        // (BU-22) Both caps go in; `validate_get_document_path` picks between
        // them from the canonical extension, which is the same one its
        // registry-membership check uses.
        let canonical = match validate_get_document_path(
            &self.kb_path,
            rel,
            &self.parser_registry,
            GET_DOCUMENT_MAX_BYTES,
            crate::parser::MAX_RAW_BINARY_BYTES,
        ) {
            ValidatePathOutcome::Found(p) => p,
            // Both say something about the path: it is absent, or it is not
            // something this server hands over. Neither says the server failed.
            ValidatePathOutcome::NotFound(e) | ValidatePathOutcome::Denied(e) => {
                return Err((LoadFailure::NotServed, e));
            }
        };
        let ext = canonical.extension().and_then(|e| e.to_str()).unwrap_or("");
        // (BU-20) The validation above checked a path; this checks the handle
        // the bytes actually come from, so nothing renamed over that path in
        // between is read. The cap comes from the shared chooser rather than
        // being recomputed, so the two steps cannot enforce different limits.
        let cap = max_bytes_for(
            &self.parser_registry,
            ext,
            crate::parser::MAX_RAW_BINARY_BYTES,
            GET_DOCUMENT_MAX_BYTES,
        );
        match crate::links::read_checked(&canonical, cap) {
            Ok(crate::links::Content::Bytes(bytes)) => {
                match build_document_response(&self.parser_registry, rel, ext, &bytes) {
                    Ok(resp) => Ok((resp, ext.to_string())),
                    // The document is there; producing text from it failed.
                    // That is the server's problem, not a missing resource.
                    Err(e) => Err((
                        LoadFailure::Internal,
                        ErrorResponse {
                            error: format!("Failed to extract document: {e}"),
                        },
                    )),
                }
            }
            Ok(crate::links::Content::Refused(refused)) => {
                tracing::warn!("{}", refused.log_line(&canonical));
                Err((
                    LoadFailure::NotServed,
                    ErrorResponse {
                        error: refused.client_message().to_string(),
                    },
                ))
            }
            Err(e) => Err((
                LoadFailure::Internal,
                ErrorResponse {
                    error: format!("Failed to read file: {e}"),
                },
            )),
        }
    }

    fn get_best_practice_blocking(&self, params: GetBestPracticeParams) -> String {
        if self.best_practice_templates.is_empty() {
            return serde_json::to_string_pretty(&ErrorResponse {
                error: "get_best_practice is not configured. Add `[best_practice].path_templates` to kb-mcp.toml (for example: `path_templates = [\"best-practices/{target}/PERFECT.md\"]`) to enable this tool.".to_string(),
            })
            .unwrap_or_default();
        }
        let canonical = match resolve_best_practice_path(
            &self.kb_path,
            &self.best_practice_templates,
            &params.target,
            &self.parser_registry,
            GET_DOCUMENT_MAX_BYTES,
        ) {
            ResolveOutcome::Found(p) => p,
            ResolveOutcome::NotFound(tried) => {
                // (BU-23) The candidate paths are built from
                // `[best_practice].path_templates`, so echoing them back hands
                // an unauthenticated caller the server's configured layout —
                // directory names it may not otherwise know exist. The count
                // is enough for the caller to tell "no template matched" from
                // "the tool is not configured"; the operator gets the paths
                // themselves on stderr.
                tracing::debug!(
                    target = %params.target,
                    tried = ?tried,
                    "get_best_practice found no matching template"
                );
                return serde_json::to_string_pretty(&ErrorResponse {
                    error: best_practice_not_found_message(&params.target, &tried),
                })
                .unwrap_or_default();
            }
            ResolveOutcome::Denied(err) => {
                return serde_json::to_string_pretty(&err).unwrap_or_default();
            }
        };

        // (BU-20) Same handle-checked read as `get_document`; the templates
        // resolve to a path, and a path is what can be swapped. The cap is the
        // one `resolve_best_practice_path` already applied to this file.
        let content = match crate::links::read_checked(&canonical, GET_DOCUMENT_MAX_BYTES) {
            Ok(crate::links::Content::Bytes(bytes)) => match String::from_utf8(bytes) {
                Ok(s) => Ok(s),
                Err(_) => Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "stream did not contain valid UTF-8",
                )),
            },
            Ok(crate::links::Content::Refused(refused)) => {
                tracing::warn!("{}", refused.log_line(&canonical));
                return serde_json::to_string_pretty(&ErrorResponse {
                    error: refused.client_message().to_string(),
                })
                .unwrap_or_default();
            }
            Err(e) => Err(e),
        };

        match content {
            Ok(content) => {
                if let Some(ref cat) = params.category {
                    // Extract a specific h2 section
                    match extract_section(&content, cat) {
                        Some(section) => {
                            let resp = BestPracticeResponse {
                                target: params.target,
                                category: Some(cat.clone()),
                                content: section,
                            };
                            serde_json::to_string_pretty(&resp).unwrap_or_default()
                        }
                        None => {
                            // Return available sections as guidance
                            let sections = list_h2_sections(&content);
                            serde_json::to_string_pretty(&ErrorResponse {
                                error: format!(
                                    "Section '{}' not found. Available sections: {}",
                                    cat,
                                    sections.join(", ")
                                ),
                            })
                            .unwrap_or_default()
                        }
                    }
                } else {
                    // Return TOC + full content
                    let sections = list_h2_sections(&content);
                    let resp = BestPracticeResponse {
                        target: params.target,
                        category: None,
                        content: format!(
                            "## Sections\n{}\n\n---\n\n{}",
                            sections
                                .iter()
                                .map(|s| format!("- {s}"))
                                .collect::<Vec<_>>()
                                .join("\n"),
                            content
                        ),
                    };
                    serde_json::to_string_pretty(&resp).unwrap_or_default()
                }
            }
            Err(e) => serde_json::to_string_pretty(&ErrorResponse {
                error: format!("Failed to read best-practices file: {e}"),
            })
            .unwrap_or_default(),
        }
    }

    fn rebuild_index_blocking(&self, params: RebuildIndexParams) -> String {
        let force = params.force.unwrap_or(false);

        // codex P2 round 1 + P2 round 4 on PR #57: flip `indexing_state` to
        // `Some` while the rebuild runs so `/api/admin/status` reports
        // indexing.active=true. Use a refcount (`active_count`) so concurrent
        // rebuild_index calls don't clear each other's state — first caller
        // sets Some(count=1), subsequent callers ++count; on Drop, --count
        // and clear the slot to None only when reaching 0.
        //
        // (BU-18) Both halves recover from a poisoned lock rather than skipping.
        // Skipping the decrement is the worse failure: the slot keeps a count
        // no rebuild owns, so `/api/admin/status` reports `indexing.active=true`
        // for the rest of the process's life. The payload is plain data, so
        // there is nothing to repair before using it.
        struct IndexingGuard(Arc<Mutex<Option<IndexingState>>>);
        impl Drop for IndexingGuard {
            fn drop(&mut self) {
                let mut guard = recover(self.0.lock(), "indexing_state");
                if let Some(s) = guard.as_mut() {
                    s.active_count = s.active_count.saturating_sub(1);
                    if s.active_count == 0 {
                        *guard = None;
                    }
                }
            }
        }
        {
            let mut guard = recover(self.indexing_state.lock(), "indexing_state");
            match guard.as_mut() {
                Some(s) => s.active_count += 1,
                None => {
                    *guard = Some(IndexingState {
                        started_at: std::time::SystemTime::now(),
                        progress: None,
                        active_count: 1,
                    });
                }
            }
        }
        let _indexing_guard = IndexingGuard(Arc::clone(&self.indexing_state));

        // Lock order: embedder first, then db (consistent with search)
        let mut embedder = recover(self.embedder.lock(), "embedder");
        let db = recover_db(self.db.lock());

        match indexer::rebuild_index(
            &db,
            &mut embedder,
            &self.kb_path,
            force,
            self.exclude_headings.as_deref(),
            &self.exclude_dirs,
            &self.parser_registry,
            indexer::progress::ProgressReporter::new(indexer::progress::ProgressMode::Quiet),
            self.context_mode_desired,
        ) {
            Ok(result) => {
                let stats = IndexStats {
                    total_documents: result.total_documents,
                    updated: result.updated,
                    renamed: result.renamed,
                    deleted: result.deleted,
                    skipped: result.skipped,
                    total_chunks: result.total_chunks,
                    duration_ms: result.duration_ms,
                };
                serde_json::to_string_pretty(&stats).unwrap_or_default()
            }
            Err(e) => serde_json::to_string_pretty(&ErrorResponse {
                error: format!("Rebuild failed: {e}"),
            })
            .unwrap_or_default(),
        }
    }

    fn get_connection_graph_blocking(&self, params: GetConnectionGraphParams) -> String {
        // パラメータ検証 + 上限クランプ
        let depth = params
            .depth
            .unwrap_or(graph::DEFAULT_DEPTH)
            .min(graph::MAX_DEPTH);
        let fan_out = params
            .fan_out
            .unwrap_or(graph::DEFAULT_FAN_OUT)
            .min(graph::MAX_FAN_OUT);
        let min_similarity = params
            .min_similarity
            .unwrap_or(graph::DEFAULT_MIN_SIMILARITY)
            .clamp(0.0, 1.0);
        let seed_strategy = match params.seed_strategy.as_deref() {
            Some("centroid") => SeedStrategy::Centroid,
            Some("all_chunks") | None => SeedStrategy::AllChunks,
            Some(other) => {
                return serde_json::to_string_pretty(&ErrorResponse {
                    error: format!(
                        "unknown seed_strategy '{other}' (expected 'all_chunks' or 'centroid')"
                    ),
                })
                .unwrap_or_default();
            }
        };

        // (BU-05) `exclude_paths` is bounded inside `build_connection_graph`
        // rather than here, so `kb-mcp graph --exclude` gets the same limit.

        let opts = GraphOptions {
            depth,
            fan_out,
            min_similarity,
            seed_strategy,
            category: params.category,
            topic: params.topic,
            exclude_paths: params.exclude_paths.unwrap_or_default(),
            dedup_by_path: params.dedup_by_path.unwrap_or(false),
            min_quality: self.quality_threshold,
            // (BU-33) 上限は拒否せずクランプする。`depth` / `fan_out` /
            // `min_similarity` と同じ流儀 (`clamp_search_limit` の doc も参照)。
            max_nodes: graph::clamp_max_nodes(params.max_nodes.unwrap_or(graph::DEFAULT_MAX_NODES)),
            max_seed_chunks: graph::clamp_max_seed_chunks(
                params
                    .max_seed_chunks
                    .unwrap_or(graph::DEFAULT_MAX_SEED_CHUNKS),
            ),
        };

        let db = recover_db(self.db.lock());
        match graph::build_connection_graph(&db, &params.path, &opts) {
            Ok(g) => serde_json::to_string_pretty(&g).unwrap_or_default(),
            Err(e) => serde_json::to_string_pretty(&ErrorResponse {
                error: format!("get_connection_graph failed: {e}"),
            })
            .unwrap_or_default(),
        }
    }
}

// ---------------------------------------------------------------------------
// Tool surface — thin async wrappers over the blocking bodies (BU-06)
// ---------------------------------------------------------------------------

/// (BU-06) Run one tool body on tokio's blocking pool and render a `JoinError`
/// as the same `ErrorResponse` JSON every other failure path uses.
///
/// A `JoinError` here means the body panicked (the handle is never aborted;
/// `spawn_blocking` work is not cancellable anyway). Before the split a panic
/// unwound through the rmcp request task; now it is caught and reported, so a
/// single bad request no longer tears down the caller's session.
async fn run_blocking<F>(tool: &'static str, f: F) -> String
where
    F: FnOnce() -> String + Send + 'static,
{
    match tokio::task::spawn_blocking(f).await {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(tool, error = %e, "tool body panicked on the blocking pool");
            serde_json::to_string_pretty(&ErrorResponse {
                error: format!("{tool} failed: the request panicked ({e})"),
            })
            .unwrap_or_default()
        }
    }
}

/// The MCP tool surface.
///
/// (BU-06) Each handler does nothing but move its arguments onto the blocking
/// pool. Keep it that way: a handler that touches `db` / `embedder` /
/// `reranker` directly re-introduces the runtime starvation described on
/// [`KbCore`], and `tool_handlers_do_not_block_the_runtime` will fail.
#[tool_router]
impl KbServer {
    #[tool(
        name = "search",
        description = "Hybrid search (vector + FTS5 full-text, merged via Reciprocal Rank Fusion) over the knowledge base. Returns a wrapper with results, low_confidence flag, and filter_applied echo. The `score` field is the RRF score (or cross-encoder score when reranker is enabled). `match_spans` field (when present) gives byte offsets into `content` for ASCII query phrases. The full-text half splits the query into per-token phrases combined with OR, so a sentence-shaped query matches documents containing any of its terms; wrap a substring in double quotes to require it verbatim, e.g. `\"Foundry Local\" setup`. Fragments shorter than three characters are merged into a neighbouring token, or dropped when they stand alone (quote them to keep them)."
    )]
    pub(crate) async fn search(&self, Parameters(params): Parameters<SearchParams>) -> String {
        let core = Arc::clone(&self.core);
        run_blocking("search", move || core.search_blocking(params)).await
    }

    #[tool(
        name = "list_topics",
        description = "List all indexed topics and categories with document counts."
    )]
    async fn list_topics(&self) -> String {
        let core = Arc::clone(&self.core);
        run_blocking("list_topics", move || core.list_topics_blocking()).await
    }

    #[tool(
        name = "get_document",
        description = "Get the full content and metadata of a document by its relative path within knowledge-base/."
    )]
    async fn get_document(&self, Parameters(params): Parameters<GetDocumentParams>) -> String {
        let core = Arc::clone(&self.core);
        run_blocking("get_document", move || core.get_document_blocking(params)).await
    }

    #[tool(
        name = "get_best_practice",
        description = "Get a best-practices document for the given target, optionally extracting a specific h2 section by category name. Opt-in: requires `[best_practice].path_templates` to be configured in kb-mcp.toml (e.g. `path_templates = [\"best-practices/{target}/PERFECT.md\"]`); returns a 'not configured' error otherwise."
    )]
    async fn get_best_practice(
        &self,
        Parameters(params): Parameters<GetBestPracticeParams>,
    ) -> String {
        let core = Arc::clone(&self.core);
        run_blocking("get_best_practice", move || {
            core.get_best_practice_blocking(params)
        })
        .await
    }

    #[tool(
        name = "rebuild_index",
        description = "Rebuild the search index by scanning all source files in the knowledge base (Markdown plus any other extensions enabled via `[parsers].enabled` in kb-mcp.toml)."
    )]
    async fn rebuild_index(&self, Parameters(params): Parameters<RebuildIndexParams>) -> String {
        let core = Arc::clone(&self.core);
        run_blocking("rebuild_index", move || core.rebuild_index_blocking(params)).await
    }

    #[tool(
        name = "get_connection_graph",
        description = "BFS-expand semantically related chunks starting from a \
                       document path. Returns a flat list of nodes with \
                       parent_id / depth / score, useful for chained context \
                       discovery by an LLM agent."
    )]
    async fn get_connection_graph(
        &self,
        Parameters(params): Parameters<GetConnectionGraphParams>,
    ) -> String {
        let core = Arc::clone(&self.core);
        run_blocking("get_connection_graph", move || {
            core.get_connection_graph_blocking(params)
        })
        .await
    }
}

/// What the server says about itself, and which capabilities it declares.
///
/// Written out rather than macro-generated. `#[tool_router(server_handler)]`
/// generates the whole `impl ServerHandler`, and a generated impl cannot be
/// extended — so the `get_info` it supplies is whatever rmcp's default is, and
/// there is nowhere to add a second capability later. That default was
/// visible on the wire: `initialize` answered
/// `serverInfo {"name":"rmcp","version":"3.1.2"}`, because the macro fills it
/// from **rmcp's** build environment. Clients display that.
///
/// Splitting it into `#[tool_router]` (which still generates the router) plus a
/// hand-written `#[tool_handler] impl` changes nothing else: measured before
/// and after the split, `tools/list` is byte-identical — all six tools, their
/// schemas, and the `ttlMs` / `cacheScope` hints the macro already sets — and
/// `initialize` differs only in the two `serverInfo` fields this fixes.
///
/// **Adding a capability here is adding an obligation, not a method.** Measured
/// on this same build: `prompts/list`, `resources/list` and
/// `resources/templates/list` already answer successfully with empty arrays
/// even though nothing is declared, because `ServerHandler`'s defaults return
/// `Ok(default())`. Declaring a capability does not make a method appear; it
/// makes clients start asking. Declaring one whose list stays empty is strictly
/// worse than not declaring it — round-trips that return nothing.
///
/// A hand-written list handler must also set the caching hints itself. The spec
/// requires `ttlMs` and `cacheScope` on any result carrying
/// `resultType: "complete"`; rmcp models them as `Option` and leaves them
/// `None` (its own doc says "Required by spec version 2026-07-28, but optional
/// here"), so only the paths that set them are conforming. The tool macro does.
/// The trait defaults do not.
#[tool_handler]
#[prompt_handler]
impl ServerHandler for KbServer {
    fn get_info(&self) -> ServerInfo {
        // `ServerInfo` and `Implementation` are `#[non_exhaustive]`, so a
        // struct literal will not compile against them from this crate: build
        // from `Default` and assign.
        let mut me = Implementation::default();
        me.name = env!("CARGO_PKG_NAME").to_string();
        me.version = env!("CARGO_PKG_VERSION").to_string();

        let mut info = ServerInfo::default();
        info.protocol_version = ProtocolVersion::LATEST;
        info.capabilities = ServerCapabilities::builder()
            .enable_tools()
            .enable_prompts()
            .enable_resources()
            .build();
        info.server_info = me;
        info
    }

    /// One resource per topic group, never one per document.
    ///
    /// The reasoning is in [`crate::resources`] and ADR-0004; the short version
    /// is that a listing is a promise the client pays for on every connect, and
    /// a knowledge base has hundreds of documents but tens of groups. Individual
    /// documents are reachable through the `kb://doc/{path}` template and the
    /// `uri` on every `search` hit, which the specification permits explicitly.
    ///
    /// Single page. `nextCursor` is omitted, which is a conforming single-page
    /// implementation and sidesteps the client-side pagination bugs the survey
    /// turned up.
    async fn list_resources(
        &self,
        _request: Option<rmcp::model::PaginatedRequestParams>,
        _context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> Result<rmcp::model::ListResourcesResult, rmcp::ErrorData> {
        let core = Arc::clone(&self.core);
        // Two layers: the outer one is "did the blocking task survive", the
        // inner one is "did the query succeed".
        let groups = run_blocking_typed("list_resources", move || core.topic_groups_blocking())
            .await
            .map_err(internal_error)?
            .map_err(internal_error)?;

        let resources = groups
            .into_iter()
            .map(|g| {
                let mut r = rmcp::model::Resource::new(g.uri(), g.display_name());
                r.description = Some(g.description());
                r.mime_type = Some("text/markdown".to_string());
                r
            })
            .collect();
        Ok(with_cache_hints(
            rmcp::model::ListResourcesResult::with_all_items(resources),
        ))
    }

    /// The one template: every indexed document, by its relative path.
    async fn list_resource_templates(
        &self,
        _request: Option<rmcp::model::PaginatedRequestParams>,
        _context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> Result<rmcp::model::ListResourceTemplatesResult, rmcp::ErrorData> {
        let mut t = rmcp::model::ResourceTemplate::new(
            crate::resources::doc_uri_template(),
            "Indexed document",
        );
        t.description = Some(
            "One indexed document, addressed by its path relative to the knowledge base. \
             Every `search` hit carries the matching `uri`. Served as text: a PDF or a \
             spreadsheet comes back as the text kb-mcp extracted from it, not as the \
             original bytes."
                .to_string(),
        );
        // No `mime_type` on the template. It varies per document — `text/markdown`
        // for `.md`, `text/plain` for everything served as extracted text — and a
        // single value here would be wrong for most of what the template matches.
        // Each read states its own.
        Ok(with_cache_hints(
            rmcp::model::ListResourceTemplatesResult::with_all_items(vec![t]),
        ))
    }

    /// Read one `kb://` URI.
    ///
    /// A document is served only if it is **in the index** and then only through
    /// the same four checks `get_document` applies. That is deliberately
    /// narrower than `get_document`, and the reasoning is not the one ADR-0003
    /// rejected: this does not trust a file inside the knowledge base to police
    /// the knowledge base, it trusts kb-mcp's own database. A resource is
    /// something the server offered; reading a URI that was never on offer is a
    /// different operation from fetching a path the caller already knew.
    async fn read_resource(
        &self,
        request: rmcp::model::ReadResourceRequestParams,
        _context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> Result<rmcp::model::ReadResourceResponse, rmcp::ErrorData> {
        let uri = request.uri.clone();
        let parsed = crate::resources::parse(&uri).ok_or_else(|| {
            rmcp::ErrorData::resource_not_found(
                format!("not a resource this server serves: {uri}"),
                None,
            )
        })?;

        let core = Arc::clone(&self.core);
        let uri_for_body = uri.clone();
        let text = run_blocking_typed("read_resource", move || {
            core.read_resource_blocking(&parsed, &uri_for_body)
        })
        .await
        .map_err(internal_error)?
        .map_err(|(kind, message)| resource_error(kind, message))?;

        let (text, mime) = text;
        let content = rmcp::model::ResourceContents::text(text, uri).with_mime_type(mime);
        Ok(rmcp::model::ReadResourceResult::new(vec![content]).into())
    }
}

/// The caching hints the specification requires on any result carrying
/// `resultType: "complete"`.
///
/// rmcp models them as `Option` and leaves them `None` — its own doc says
/// "Required by spec version 2026-07-28, but optional here" — so only the paths
/// that set them conform. The tool and prompt macros do; a hand-written handler
/// has to. `0` means "treat as stale immediately", which is the honest answer
/// for a listing backed by an index a watcher keeps changing.
fn with_cache_hints<T: CacheHinted>(result: T) -> T {
    result
        .with_ttl_ms(0)
        .with_cache_scope(rmcp::model::CacheScope::Public)
}

/// The two setters every paginated list result carries, so [`with_cache_hints`]
/// can be written once.
trait CacheHinted: Sized {
    fn with_ttl_ms(self, ttl_ms: u64) -> Self;
    fn with_cache_scope(self, scope: rmcp::model::CacheScope) -> Self;
}

macro_rules! impl_cache_hinted {
    ($($t:ty),+ $(,)?) => {$(
        impl CacheHinted for $t {
            fn with_ttl_ms(self, ttl_ms: u64) -> Self { <$t>::with_ttl_ms(self, ttl_ms) }
            fn with_cache_scope(self, scope: rmcp::model::CacheScope) -> Self {
                <$t>::with_cache_scope(self, scope)
            }
        }
    )+};
}
impl_cache_hinted!(
    rmcp::model::ListResourcesResult,
    rmcp::model::ListResourceTemplatesResult,
);

fn internal_error(e: String) -> rmcp::ErrorData {
    rmcp::ErrorData::internal_error(e, None)
}

/// Turn a [`LoadFailure`] into the JSON-RPC error that says the same thing.
///
/// Only a statement about the resource gets the not-found code; a failure of
/// the server's own stays an internal error, which is what `list_resources`
/// already reports for the identical unreadable index. Written as a function so
/// the mapping can be asserted directly — the two codes mean different things
/// to a retrying client, and nothing else would notice them being collapsed.
fn resource_error(kind: LoadFailure, message: String) -> rmcp::ErrorData {
    match kind {
        LoadFailure::NotServed => rmcp::ErrorData::resource_not_found(message, None),
        LoadFailure::Internal => internal_error(message),
    }
}

/// [`run_blocking`] for handlers that return something other than a JSON string.
///
/// Same obligation as every tool handler (BU-06): the body does its work on the
/// blocking pool, never on an async worker.
async fn run_blocking_typed<T, F>(what: &'static str, f: F) -> Result<T, String>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    tokio::task::spawn_blocking(f).await.map_err(|e| {
        tracing::error!(handler = what, error = %e, "handler body panicked on the blocking pool");
        format!("{what} failed: the request panicked ({e})")
    })
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Decide the reranker's input-limit from the candidate-pool size and the
/// caller's `limit`, depending on whether MMR is enabled.
///
/// When MMR is on, the reranker should rerank *every* candidate in the
/// pool because MMR will then greedily down-select to `limit`. When MMR
/// is off, the reranker only needs `limit` rows (the pipeline returns
/// `reranked.take(limit)` directly).
///
/// The `usize → u32` saturate cast (via `u32::try_from`) is the core
/// guard against codex-review trap #1 (passing `u32::MAX` to
/// `Vec::with_capacity` used to OOM). Even if a future caller mistakenly
/// passes a `pool_size` larger than `u32::MAX`, this helper bounds it
/// at `u32::MAX` rather than panicking or wrapping.
fn compute_reranker_input_limit(mmr_enabled: bool, pool_size: usize, limit: u32) -> u32 {
    if mmr_enabled {
        u32::try_from(pool_size).unwrap_or(u32::MAX)
    } else {
        limit
    }
}

/// Shared MMR-aware search pipeline. Used by:
/// - MCP `SearchTool::search` (server.rs)
/// - CLI `kb-mcp search` (main.rs)
/// - CLI `kb-mcp eval` (eval.rs)
///
/// Steps:
/// 1. RRF candidate pool (unbounded if MMR on, overfetch if reranker on,
///    bounded `limit` otherwise — invariant #3: MMR off + reranker off
///    matches the legacy `db.search_hybrid(.., limit, ..)` path bit-exactly).
/// 2. Optional cross-encoder reranker (`rerank_candidates_with_ids` to
///    preserve chunk_id for downstream MMR).
/// 3. Optional MMR diversification (`mmr_select`) with min-max relevance
///    normalization (`mmr.rs` contract: relevance in `[0, 1]`).
///
/// Returns `Vec<(chunk_id, SearchResult)>` so callers can apply their own
/// final formatting (match_spans, JSON wrapper, eval metrics, etc.).
///
/// Range validation for `mmr_lambda` / `mmr_same_doc_penalty` is performed
/// here so that all 3 callers reject `1.5` / `-0.1` / `NaN` consistently.
/// Caller-side early reject (e.g. for a richer error response shape) is OK
/// — this is belt-and-suspenders.
#[allow(clippy::too_many_arguments)] // 8 cohesive inputs; struct-of-args adds noise without grouping
pub fn run_search_pipeline(
    db: &Database,
    reranker: Option<&mut Reranker>,
    query: &str,
    query_embedding: &[f32],
    limit: u32,
    filters: &crate::db::SearchFilters<'_>,
    overrides: &crate::config::SearchOverrides,
    toml_search: &crate::config::SearchConfig,
) -> anyhow::Result<Vec<(i64, crate::db::SearchResult)>> {
    // Range validation. NaN は `(0.0..=1.0).contains` が false なので同経路で reject。
    if let Some(l) = overrides.mmr_lambda
        && !(0.0..=1.0).contains(&l)
    {
        anyhow::bail!("mmr_lambda out of range: {l} (must be 0.0..=1.0)");
    }
    if let Some(p) = overrides.mmr_same_doc_penalty
        && !(0.0..=1.0).contains(&p)
    {
        anyhow::bail!("mmr_same_doc_penalty out of range: {p} (must be 0.0..=1.0)");
    }

    // AU-01: `limit` の clamp は **この関数**で行う。呼び出し側 (MCP search /
    // CLI search / CLI eval) の各境界で clamp する形にすると、追加した caller が
    // 漏れる — 実際 codex P1 (PR #81) が「eval だけ生の値を渡していて、
    // reranker on + MMR off のとき `compute_reranker_input_limit` がそれを
    // そのまま返し `rerank_candidates_with_ids` の `Vec::with_capacity` で
    // 落ちる」経路を検出した。3 caller が必ず通る唯一の choke point で閉じる。
    let limit = clamp_search_limit(limit);

    let resolved = overrides.resolve(toml_search);
    // fusion は per-call override を持たない (MMR と違い resolve 機構を
    // 通さない、feature-47 D-6)。toml をそのまま db 層へ渡す。
    let fusion = crate::db::FusionParams::from(&toml_search.fusion);
    let use_rerank = reranker.is_some();

    // 1. RRF candidate pool. MMR on → unbounded (MMR が候補プール全件から
    //    多様化選抜、user の `limit` を反映して overfetch を計算)、reranker
    //    on → overfetch (`limit*5.max(50)`)、どちらも off → 最小コストで
    //    `limit` 件 (invariant #3 の bit-exact path)。
    let mmr_pool_size = limit.saturating_mul(5).max(50);
    let candidates_pool: Vec<(i64, crate::db::SearchResult)> = if resolved.mmr_enabled {
        db.search_hybrid_candidates_unbounded(
            query,
            query_embedding,
            mmr_pool_size,
            filters,
            fusion,
        )?
    } else if use_rerank {
        db.search_hybrid_candidates(
            query,
            query_embedding,
            limit.saturating_mul(5).max(50),
            filters,
            fusion,
        )?
    } else {
        db.search_hybrid_candidates(query, query_embedding, limit, filters, fusion)?
    };

    // 2. Optional reranker。MMR off の reranker 入力 limit は `limit` (元の挙動
    //    保持)、MMR on のときは MMR 側が select するので候補プール全体を保持
    //    する。**P1 fix**: ここで `u32::MAX` を渡すと `Vec::with_capacity(u32::MAX)`
    //    で OOM 直行するので、候補プールサイズを上限とする
    //    (`limit*5.max(50)` で実用上 limit に追従)。saturate cast
    //    (`u32::try_from(...).unwrap_or(u32::MAX)`) は helper の中に押し込み済み。
    let reranker_input_limit =
        compute_reranker_input_limit(resolved.mmr_enabled, candidates_pool.len(), limit);
    let reranked: Vec<(i64, crate::db::SearchResult)> = match reranker {
        Some(r) => r.rerank_candidates_with_ids(query, candidates_pool, reranker_input_limit)?,
        None => candidates_pool,
    };

    // 3. MMR re-rank (on の時のみ)。off なら reranked の先頭 `limit` 件を返す
    //    (= 既存挙動 bit-exact)。
    if !resolved.mmr_enabled {
        return Ok(reranked.into_iter().take(limit as usize).collect());
    }

    // MmrCandidate を構築するため chunk_id 群の embedding を一括取得。
    // F-41 PR-2: path → documents.id の N+1 lookup は廃止、SearchResult.document_id を
    // candidate SQL で carry 済 (rename race の unwrap_or(0) collision = F-44 も同時消失)。
    let chunk_ids: Vec<i64> = reranked.iter().map(|(id, _)| *id).collect();
    let emb_map = {
        use anyhow::Context;
        db.fetch_embeddings_by_chunk_ids(&chunk_ids)
            .context("MMR fetch_embeddings_by_chunk_ids failed")?
    };

    let mut mmr_cands: Vec<crate::mmr::MmrCandidate> = reranked
        .iter()
        .filter_map(|(id, sr)| {
            let emb = emb_map.get(id).cloned()?;
            Some(crate::mmr::MmrCandidate {
                chunk_id: *id,
                document_id: sr.document_id,
                embedding: emb,
                relevance_score: sr.score,
            })
        })
        .collect();

    // mmr.rs の contract: relevance_score は [0, 1] に正規化済み前提。
    // RRF スコアは ~0.01-0.03、cross-encoder スコアは ~[-10, 10] の arbitrary
    // range を取るため、ここで pool 内 min-max 正規化する。
    if !mmr_cands.is_empty() {
        let (min_rel, max_rel) = mmr_cands
            .iter()
            .fold((f32::INFINITY, f32::NEG_INFINITY), |(lo, hi), c| {
                (lo.min(c.relevance_score), hi.max(c.relevance_score))
            });
        let range = max_rel - min_rel;
        if range > f32::EPSILON {
            for c in &mut mmr_cands {
                c.relevance_score = (c.relevance_score - min_rel) / range;
            }
        } else {
            for c in &mut mmr_cands {
                c.relevance_score = 0.0;
            }
        }
    }

    let selected = crate::mmr::mmr_select(
        &mmr_cands,
        resolved.mmr_lambda,
        resolved.mmr_same_doc_penalty,
        limit as usize,
    );

    // mmr_cands と reranked は filter_map で skip した chunk_id が
    // mmr_cands に存在しないので、selected の i (mmr_cands index) から
    // chunk_id を引いて reranked に当てる方が安全。
    let by_id: std::collections::HashMap<i64, &(i64, crate::db::SearchResult)> =
        reranked.iter().map(|t| (t.0, t)).collect();
    let after_mmr: Vec<(i64, crate::db::SearchResult)> = selected
        .into_iter()
        .filter_map(|i| {
            let cid = mmr_cands.get(i)?.chunk_id;
            by_id.get(&cid).map(|t| (*t).clone())
        })
        .collect();

    // 4. Parent retriever は呼び出し側 (`apply_parent_retriever`) が
    //    SearchHit 化後に適用する。`run_search_pipeline` の戻り値型
    //    (`Vec<(i64, SearchResult)>`) を変えずに 3 caller (MCP / CLI / eval)
    //    で wiring を共有するため、ここでは noop。
    Ok(after_mmr)
}

/// Convert the user-facing `path_globs` input
/// (e.g. `["docs/**", "!docs/draft/**"]`) into a [`crate::db::CompiledPathGlobs`].
///
/// Patterns prefixed with `!` are routed into the exclude `GlobSet`; the rest
/// build the include set. An empty input array is an explicit error — callers
/// should pass `None` to disable filtering, or `["**", "!a/**"]` to express
/// exclude-only intent. Inputs consisting entirely of `!`-prefixed patterns
/// are accepted: `include` stays `None` (interpreted as "match everything")
/// and the excludes apply on top.
///
/// Visible to the crate so the CLI (`src/main.rs`) can reuse the same
/// validation path.
pub fn compile_path_globs(patterns: &[String]) -> anyhow::Result<crate::db::CompiledPathGlobs> {
    use anyhow::Context;
    if patterns.is_empty() {
        anyhow::bail!(
            "path_globs cannot be empty. Use null to disable, or [\"**\", \"!a/**\"] for exclude-only."
        );
    }
    // AU-17: 件数・要素長の上限。ここに置くと CLI (`src/main.rs`) を含む
    // 全 caller が同じ上限を得る。MCP の入口でも同じ検査をしているが、
    // そちらは 3 つの list を 1 箇所で読めるようにするためのもの。
    validate_filter_list("path_globs", patterns)?;
    let mut include_b = globset::GlobSetBuilder::new();
    let mut exclude_b = globset::GlobSetBuilder::new();
    let mut has_include = false;
    let mut has_exclude = false;
    for raw in patterns {
        let (target, pat, is_exclude) = if let Some(rest) = raw.strip_prefix('!') {
            (&mut exclude_b, rest, true)
        } else {
            (&mut include_b, raw.as_str(), false)
        };
        let glob = globset::Glob::new(pat)
            .with_context(|| format!("invalid path_glob pattern: {raw:?}"))?;
        target.add(glob);
        if is_exclude {
            has_exclude = true;
        } else {
            has_include = true;
        }
    }
    let include = if has_include {
        Some(include_b.build()?)
    } else {
        None
    };
    let exclude = if has_exclude {
        Some(exclude_b.build()?)
    } else {
        None
    };
    Ok(crate::db::CompiledPathGlobs { include, exclude })
}

/// rank-based low_confidence 判定。
///
/// - `scores.len() < 2` のとき false (比較対象なし)
/// - `mean(scores) <= 0.0` のとき false (フォールバック)
/// - `min_ratio == 0.0` のとき false (判定無効)
/// - `max(scores) / mean(scores) < min_ratio` のとき true
///
/// `scores` は順序非依存。relevance ピークは「ranking 順序ではなく score
/// 自体の最大値」で決定する。MMR (diversity 補正) 後の hits は score 降順
/// ではなく selection order に並ぶため、`scores[0]` を top1 とみなす旧実装
/// では低 confidence 判定が壊れていた (codex review の指摘)。`max` で取る
/// 実装は MMR off / on どちらでも同一結果を返す (NaN は std::f32 の
/// `partial_cmp` 順守、`fold(NEG_INFINITY, f32::max)` で安定)。
///
/// `pub` (lib crate API) で CLI (`src/main.rs`) / benches からも再利用できるようにしておく。
pub fn compute_low_confidence(scores: &[f32], min_ratio: f32) -> bool {
    if scores.len() < 2 || min_ratio == 0.0 {
        return false;
    }
    let sum: f32 = scores.iter().sum();
    let mean = sum / scores.len() as f32;
    if mean <= 0.0 {
        return false;
    }
    let top1 = scores.iter().copied().fold(f32::NEG_INFINITY, f32::max);
    (top1 / mean) < min_ratio
}

/// `compute_match_spans` が計算対象とする content の最大バイト数 (256 KiB)。
/// 通常の chunk は heading 単位で数 KiB だが、frontmatter のみ巨大ファイル等
/// 異常入力で O(N×M) になり得るため定義域を切る。F-35。
pub(crate) const MATCH_SPAN_CONTENT_MAX_BYTES: usize = 256 * 1024;

/// 1 chunk あたりが返す span の最大件数。一致が大量に出る query (例: 1 文字
/// term × 大き目 content) で span 配列が肥大するのを抑える。F-35。
pub(crate) const MATCH_SPAN_MAX_COUNT: usize = 100;

/// (BU-10) span 計算で見る term の最大数。
///
/// `query_phrases` は 32 で cap 済みなので、これが効くのは phrase を作れない
/// クエリが落ちる whitespace fallback 側だけ。そこは今まで無制限で、5000 語の
/// クエリなら 5000 term を全部走査していた。
pub(crate) const MATCH_SPAN_MAX_TERMS: usize = 100;

/// 全 term が ASCII の場合のみ chunk 内で case-insensitive な substring 検索を
/// 行い、byte offset (UTF-8 char boundary 保証) を返す。
///
/// 戻り値:
/// - `None` — query 全体に non-ASCII を 1 つでも含む / 空 query / content
///   が `MATCH_SPAN_CONTENT_MAX_BYTES` を超える (= 計算しない)
/// - `Some(vec![])` — 計算したが一致なし
/// - `Some(spans)` — 下記の契約を満たす span 列
///
/// # 契約 (BU-09 / BU-10)
///
/// 1. **disjoint かつ昇順**: `spans[i].end <= spans[i+1].start`。重なった一致は
///    和集合に畳む (`next.start < cur.end` のときだけ結合 = **strict**)。
///    隣接 (`next.start == cur.end`) は結合しない
/// 2. **非空**: すべての span が `start < end`
/// 3. **冪等**: 出力にもう一度同じ畳み込みを掛けても変わらない
/// 4. **件数上限**: `MATCH_SPAN_MAX_COUNT` (100) 件以下
/// 5. **語順非依存**: クエリ内の語順を入れ替えてもバイト単位で同じ配列を返す。
///    **ただし `query_phrases` の 32 phrase 上限に当たっていない場合に限る** —
///    上限に当たると `dedup_and_cap_counted` が「クエリ順で先頭 32 個」を残すので、
///    語順を変えると **FTS が検索する phrase 集合そのもの**が変わる。これは
///    ハイライトではなく検索の挙動なので、ここでは直せない (codex P2、PR #142)
/// 6. **カバレッジ**: term が k 個 (k ≤ 100) あって各々が 1 回以上出現するなら、
///    **すべての term** が最低 1 つの span に覆われる
///
/// 5 と 6 は各 term に `MAX_COUNT / k` 件 (最低 1) の予算を与え、その範囲で
/// 出現順に取ることで出す。余った予算は**再配分しない** — 配分すると「どの
/// term が追加分を得るか」が term 順に依存し、6 が消しにいった順序依存が縁に
/// 戻るため。k=32 なら 96 件で止まる (100 件に届かない) が、それが代償。
///
/// `MATCH_SPAN_MAX_TERMS` で切る前に term 列を dedup + ソートするのも 5 のため。
/// 素朴に先頭 100 個を取ると、101 個以上の token を並べたクエリで語順が cutoff に
/// 効いてしまう。
///
/// ## なぜこの形か (実測)
///
/// feature-48 以前は term = whitespace 分割で、`break 'outer` が 100 件目で
/// 全体を打ち切っていた。feature-48 で term が `query_phrases` 由来 (最大 32、
/// 入れ子あり) になった結果:
/// - `"Foundry Local" Foundry` が `(0,7)` と `(0,13)` の**重なった** span を返す
/// - 先頭 phrase が 100 件出すと後続 phrase は 1 件も載らず、しかもその順序は
///   コンパイラ内部の生成順
///
/// 検討した代替案「全出現を集めてから出現順位で上位 100 を選ぶ」は、実測で
/// **100〜450 倍**遅く (密な 32 phrase × 256 KiB で 157 µs → 33.1 ms、
/// `limit` 最大 1000 なら 1 検索 33 秒)、かつ正しさが早期終了条件に依存して
/// **テストで固定できない**ため退けた。本方式は現実的なチャンク (4〜16 KiB)
/// で 1.0〜1.2 倍、256 KiB × 32 phrase の病的入力でも約 2〜3 倍 (≈120 µs)。
///
/// `pub` (lib crate API) で CLI (`src/main.rs`) / benches からも再利用できるようにしておく。
pub fn compute_match_spans(query: &str, content: &str) -> Option<Vec<crate::db::MatchSpan>> {
    let trimmed = query.trim();
    if trimmed.is_empty() {
        return None;
    }
    // feature-48: FTS へ投げる phrase と同じ分割を使う。独自に whitespace 分割すると
    // `"Foundry Local"` のような quote 付きクエリで `"Foundry` / `Local"` を探しに行き、
    // FTS は当たっているのに span だけ空になる (codex review P2、PR #134)。
    // token 化で phrase を作れないクエリ (`ab` 等) は FTS 自体が使われないので、
    // ハイライトのためだけに従来どおり whitespace 分割へ落とす。
    let phrases = crate::db::query_phrases(trimmed);
    let terms: Vec<&str> = if phrases.is_empty() {
        trimmed.split_whitespace().collect()
    } else {
        phrases.iter().map(String::as_str).collect()
    };
    if terms.is_empty() {
        return None;
    }
    if terms.iter().any(|t| !t.is_ascii()) {
        return None;
    }

    // F-35: content size cap。通常 chunk (見出し単位、数 KiB) は影響なし、
    // 異常な巨大入力に対する O(N×M) ガード。
    if content.len() > MATCH_SPAN_CONTENT_MAX_BYTES {
        return None;
    }

    // (BU-10) term 数を切る。`query_phrases` 側は 32 で cap 済みなので、
    // 効くのは whitespace fallback だけ。
    //
    // **切る前に正規化する** (codex P2、PR #142)。素朴に `take(100)` すると
    // 「クエリ内で先に書いた 100 個」が残るので、101 個以上の短い token を
    // 並べたクエリ (2 byte token なら 1 KiB 上限に十分収まる) では語順を
    // 入れ替えるだけで残る term が変わり、語順非依存の保証が破れる。
    // dedup + ソートで cutoff を語順から切り離す。
    //
    // ソートは cutoff にしか影響しない: 各 term は独立に予算を持ち、span は
    // 最後にまとめて畳むので、走査順は出力を変えない。
    //
    // **照合と同じ ASCII fold をかけてから** dedup する (codex P2、PR #142)。
    // 大文字小文字だけ違う term は同じ位置に同じ span を出すので、別 term と
    // して数えると予算が二重取りされて無駄になる (`Rust rust` なら各 50 件 →
    // 畳んで 50 件、使えるはずの 100 件に届かない)。fallback 側では case 違いが
    // 100 term の枠を食って、本当に別の term を締め出す。
    let mut terms: Vec<String> = terms.iter().map(|t| t.to_ascii_lowercase()).collect();
    terms.sort_unstable();
    terms.dedup();
    terms.truncate(MATCH_SPAN_MAX_TERMS);

    let content_lower = content.to_ascii_lowercase();
    // (BU-10) 1 term あたりの予算。floor なので合計は必ず cap 以下になる
    // (ceil だと k=32 で 4×32=128 になり、公開済みの「100 件以下」を破る)。
    let term_count = terms.iter().filter(|t| !t.is_empty()).count().max(1);
    let budget = (MATCH_SPAN_MAX_COUNT / term_count).max(1);

    let mut spans: Vec<crate::db::MatchSpan> = Vec::new();
    for term_lower in &terms {
        if term_lower.is_empty() {
            continue;
        }
        // `take(budget)` は遅延なので、予算に達した時点でその term の走査も
        // 止まる。全一致を数え上げてから選ぶ方式にしないのはこのため。
        for (start, _) in content_lower
            .match_indices(term_lower.as_str())
            .take(budget)
        {
            let end = start + term_lower.len();
            // ASCII-only term + ASCII lowercasing なので byte 長は変わらず、
            // content 側の byte offset も自動的に char boundary に揃う。
            // debug_assert で不変条件を担保 (リリースでは noop、テストで logic
            // regression を panic 検出)。
            debug_assert!(
                content.is_char_boundary(start) && content.is_char_boundary(end),
                "ASCII-only invariant broke: span ({start}, {end}) not on char boundary in content"
            );
            spans.push(crate::db::MatchSpan { start, end });
        }
    }
    Some(merge_disjoint_spans(spans))
}

/// (BU-09) 重なった span を和集合に畳んで、昇順・disjoint・非空の列にする。
///
/// 結合条件は **strict** な `next.start < cur.end`。`<=` にすると隣接しただけの
/// span まで繋がり、`test_compute_match_spans_count_capped` の入力
/// (`"a"` × 500 に対する 100 個の 1 byte span) が 1 個に潰れる。それでも
/// `len() <= 100` は通るのでテストは緑のまま、cap の検査だけが無意味になる —
/// 実測で確認した (非 strict → 1 span、strict → 100 span)。
fn merge_disjoint_spans(mut spans: Vec<crate::db::MatchSpan>) -> Vec<crate::db::MatchSpan> {
    spans.sort_by_key(|s| (s.start, s.end));
    let mut merged: Vec<crate::db::MatchSpan> = Vec::with_capacity(spans.len());
    for s in spans {
        match merged.last_mut() {
            Some(last) if s.start < last.end => last.end = last.end.max(s.end),
            _ => merged.push(s),
        }
    }
    merged
}

/// `get_document` ツール用に、拡張子に対応する Parser で
/// frontmatter (title/date/topic/tags) を抽出し DocumentResponse を組む。
/// 純粋関数化してテスト可能にしている。
/// `get_document` の最大バイト数。1 MiB を超える文書は `fs::read` による
/// バイト一括読みでのメモリ膨張・レスポンス過大を避けるため拒否する。
pub(crate) const GET_DOCUMENT_MAX_BYTES: u64 = 1024 * 1024;

/// get_document がバイナリ形式で応答する抽出テキストの上限 (1 MiB)。超過分は
/// char 境界で truncate し `DocumentResponse.truncated = true` を立てる (§4.4)。
pub(crate) const EXTRACTED_TEXT_MAX_BYTES: usize = 1024 * 1024;

/// `s` を UTF-8 char 境界を保って最大 `max_bytes` バイトに truncate する。
/// truncate したら `true`、無切り詰めなら `false`。
fn truncate_on_char_boundary(s: &mut String, max_bytes: usize) -> bool {
    if s.len() <= max_bytes {
        return false;
    }
    let mut boundary = max_bytes;
    while boundary > 0 && !s.is_char_boundary(boundary) {
        boundary -= 1;
    }
    s.truncate(boundary);
    true
}

/// `search` MCP tool が受理する query 文字列の最大バイト数 (1 KiB)。
/// 上限超えは ErrorResponse で reject する。embedder / FTS5 layer は内部で
/// truncate するが、上流で reject した方がレスポンスが予測可能になり、
/// `compute_match_spans` の O(N×M) を query 側からも抑制できる。F-35。
pub(crate) const SEARCH_QUERY_MAX_BYTES: usize = 1024;

/// `search` の list 型 filter (`path_globs` / `tags_any` / `tags_all`) が
/// 受理する要素数の上限。
///
/// full-audit 2026-07-26 AU-17: `query` だけ 1 KiB cap が入っていて、同じ
/// リクエストに載る list 型 filter は件数も長さも無制限だった。HTTP transport
/// には body size 上限も設定していないので、1 リクエストで CPU を焼ける。
/// debug build での実測:
///
/// | 入力 | コスト |
/// |---|---|
/// | `path_globs` 64 本 | 2.8 ms |
/// | `path_globs` 100,000 本 | 1.65 s |
/// | 100,000 文字の glob 1 本 | 0.50 s |
/// | `tags_any` 100,000 件 × 候補 1,000 | 8.2 s |
/// | `tags_any` 1,000,000 件 × 候補 1,000 | **85 s** |
///
/// `tags_*` が最も悪い。SQL ではなく候補ごとの線形照合
/// (`db::matches_tags_any`) なので、コストは 件数 × 候補数 で伸びる。
/// `limit` は [`SEARCH_LIMIT_MAX`] で抑えてあるが、候補数はその数倍になる。
///
/// 64 は「実用上ありえる指定数」より十分大きく、かつ compile コストが
/// 数 ms に収まる点として選んだ。
pub(crate) const FILTER_LIST_MAX_ITEMS: usize = 64;

/// list 型 filter の各要素のバイト数上限。`query` と同じ 1 KiB。
///
/// 件数だけ絞っても、1 本の巨大な glob で同じことができる (上表の
/// 「100,000 文字の glob 1 本」)。globset は 1,000,000 文字でようやく自前で
/// エラーにするが、そこに至るまで 2.8 s かかる。
pub(crate) const FILTER_ITEM_MAX_BYTES: usize = SEARCH_QUERY_MAX_BYTES;

/// list 型 filter の件数・要素長を検証する (AU-17)。
///
/// `compile_path_globs` の内側と MCP の入口の両方から呼ぶ。前者は CLI を
/// 含む全経路を、後者は `tags_*` を含めて 3 つの上限を 1 箇所で読めるように
/// するため。
pub fn validate_filter_list(name: &str, items: &[String]) -> anyhow::Result<()> {
    if items.len() > FILTER_LIST_MAX_ITEMS {
        anyhow::bail!(
            "{name} has too many entries: {} (max {FILTER_LIST_MAX_ITEMS}). \
             Narrow the filter, or issue several calls.",
            items.len()
        );
    }
    if let Some(too_long) = items.iter().find(|s| s.len() > FILTER_ITEM_MAX_BYTES) {
        anyhow::bail!(
            "{name} has an entry that is too large: {} bytes (max {FILTER_ITEM_MAX_BYTES} bytes).",
            too_long.len()
        );
    }
    Ok(())
}

/// `search` が受理する `limit` の上限。
///
/// full-audit 2026-07-26 AU-01 (Critical): `limit` は候補プール算出
/// (`limit * 5`) を経て `Vec::with_capacity` まで生で流れるため、上限が無いと
/// `{"query":"a","limit":4294967295}` の 1 リクエストで allocation abort
/// (= panic ではなく catch 不能なプロセス即死) を起こせる。HTTP transport では
/// 全接続が落ちる。`tune` 側は `MAX_TUNE_K` で同じ罠を塞いである
/// (feature-47 codex P2 round 4) が、より古い search 経路に残っていた。
///
/// 値は「実用上のページング上限」として 1000。KB 全件走査のような用途は
/// `limit` ではなく `kb-mcp search` の繰り返しか MMR pool 側で扱う。
pub const SEARCH_LIMIT_MAX: u32 = 1000;

/// `limit` を [`SEARCH_LIMIT_MAX`] に丸める。エラーにせず clamp するのは、
/// 「多めに要求すると落ちる」より「多めに要求すると上限で返る」方が
/// MCP client (LLM) にとって回復しやすいため。
pub fn clamp_search_limit(limit: u32) -> u32 {
    limit.min(SEARCH_LIMIT_MAX)
}

/// `validate_get_document_path` の結果。各 fail variant に既存の
/// `ErrorResponse` を内蔵することで、caller (`get_document` /
/// `resolve_best_practice_path`) は文言生成や prefix 追加なしで
/// `ErrorResponse` を直接 JSON 化できる (= 既存 5 unit test の
/// `err.error.contains("...")` assertion 完全保持)。
///
/// - `Found(PathBuf)` — 4 段階防御を通過、canonical な絶対パス
/// - `NotFound(ErrorResponse)` — file-not-found / canonicalize-failed /
///   outside-kb / extension-denied / size-exceeded の総称。`get_best_practice`
///   の template loop では「次 template を試す」価値ありと解釈
/// - `Denied(ErrorResponse)` — symlink hit のみ (security event)。
///   `get_best_practice` の template loop では即 break = 攻撃 indicator を
///   surface
#[derive(Debug)]
pub(crate) enum ValidatePathOutcome {
    Found(PathBuf),
    NotFound(ErrorResponse),
    Denied(ErrorResponse),
}

/// (BU-23) `get_best_practice` の「見つからなかった」応答。
///
/// **`tried` の中身をクライアントへ返さないこと**が本 fn の存在理由。候補パスは
/// `[best_practice].path_templates` から作られるので、そのまま返すと未認証の
/// 呼び出し元にサーバの設定した配置 (存在すら知らないはずのディレクトリ名を含む)
/// を渡すことになる。件数だけあれば「どのテンプレートにも当たらなかった」と
/// 「そもそも未設定」は呼び出し元にも区別できる。実際のパスは operator が
/// `RUST_LOG=kb_mcp=debug` で stderr から見る。
fn best_practice_not_found_message(target: &str, tried: &[String]) -> String {
    format!(
        "Best-practices document for target '{}' not found ({} template{} tried). \
         Check `[best_practice].path_templates` in kb-mcp.toml, or run the server \
         with `RUST_LOG=kb_mcp=debug` to see which paths were probed.",
        target,
        tried.len(),
        if tried.len() == 1 { "" } else { "s" }
    )
}

/// `get_document` のパス検証 + size cap。成功時は canonical な絶対パスを返す。
/// 拒否時は `ErrorResponse` を返し、呼び出し側が JSON 化する。
///
/// 防御の順序:
/// 1. **symlink reject** — `canonicalize` の前に拾う必要がある
/// 2. **canonicalize + starts_with(kb_path)** — `..` 抜け道を defeat
/// 3. **extension membership** — indexer と同じ拡張子セットに限定。
///    `.git/config` のように registry に無い拡張子のファイルは読めない
/// 4. **size cap** — RAM-OOM を防ぐ。**どちらの上限を使うかは canonical
///    パスの拡張子から決める** (BU-08 と同じく、3 と同じ情報源を使う)
///
/// (BU-22) 以前は cap の選択だけ呼び出し側が **canonicalize 前のリクエスト
/// パス**の拡張子から行い、membership check は canonical 側を見ていた。両者が
/// 食い違うと上限が入れ替わる。Windows の 8.3 短縮名がまさにそれで、
/// `presentation-deck.pptx` は `PRESEN~1.PPT` になる (この開発機で実測)。
/// 拡張子は 3 文字に切られるので `.pptx`/`.xlsx`/`.docx` はいずれも registry に
/// 無い legacy 拡張子に化け、text 上限 (1 MiB) が binary 上限 (50 MiB) の代わりに
/// 適用される。1 MiB 超の Office 文書が短縮名経由で「File too large」になっていた。
///
/// (BU-08) **`exclude_dirs` はここに効かない**。この fn は `exclude_dirs` を
/// 引数に取っておらず、`.obsidian/note.md` のように「除外ディレクトリ配下だが
/// 拡張子は registry にある」ファイルは `get_document` から読める。
/// `exclude_dirs` の契約は「**索引しない**」であって「読ませない」ではない
/// — 検索には出ないがパスを知っていれば取得できる。kb_path 配下に置いた時点で
/// 読める前提の設計なので、読ませたくないものは kb_path の外に置くこと。
/// `document_in_excluded_dir_is_still_readable` が現契約として pin している。
///
/// (feature-49) **`.kb-mcpignore` も同じくここには効かない**。同じ契約を意図的に
/// 踏襲している: KB に書ける者は `.kb-mcpignore` を消すこともできるので、
/// 木の中に置いたルールがその木を守る境界にはなり得ない。`.kb-mcpignore` に
/// 書いたパスは索引されず `search` にも `get_connection_graph` にも出ないが、
/// パスを知っていれば `get_document` で読める。
/// Which of the two caps applies to `ext` (BU-22).
///
/// (BU-20) Shared, because the number is now needed twice: once by
/// [`validate_get_document_path`], which stats the path, and once by the read
/// that follows it, which enforces the same limit on the handle it reads from.
/// Recomputing it at the second site is how the two would come to disagree.
pub(crate) fn max_bytes_for(
    registry: &Registry,
    ext: &str,
    binary_max_bytes: u64,
    text_max_bytes: u64,
) -> u64 {
    if registry
        .by_extension(ext)
        .map(|p| p.is_binary())
        .unwrap_or(false)
    {
        binary_max_bytes
    } else {
        text_max_bytes
    }
}

pub(crate) fn validate_get_document_path(
    kb_path: &std::path::Path,
    rel_path: &str,
    registry: &Registry,
    text_max_bytes: u64,
    binary_max_bytes: u64,
) -> ValidatePathOutcome {
    let file_path = kb_path.join(rel_path);

    // 1. Symlink reject (canonicalize の前に判定)
    match std::fs::symlink_metadata(&file_path) {
        Ok(meta) if meta.file_type().is_symlink() => {
            return ValidatePathOutcome::Denied(ErrorResponse {
                error: "Access denied: symlinks are not allowed.".to_string(),
            });
        }
        // (BU-20) A hard link reaches the same content without being a symlink,
        // and `canonicalize` below cannot help: a hard link has no target, so
        // it canonicalizes to itself, inside the KB. The index refuses these
        // too, but `get_document` is reachable by path without going through
        // the index at all.
        Ok(_) if crate::links::is_multiply_linked(&file_path) => {
            tracing::warn!("{}", crate::links::refusal_reason(&file_path));
            return ValidatePathOutcome::Denied(ErrorResponse {
                // One literal for both moments a hard link can be refused —
                // here and at the read that follows (BU-20).
                error: crate::links::HARD_LINK_DENIED.to_string(),
            });
        }
        Ok(_) => {}
        Err(_) => {
            return ValidatePathOutcome::NotFound(ErrorResponse {
                error: format!(
                    "File not found: {rel_path}. Path should be relative to knowledge-base/ (e.g. \"deep-dive/mcp/overview.md\")."
                ),
            });
        }
    }

    // 2. Path traversal prevention
    let canonical = match file_path.canonicalize() {
        Ok(p) => p,
        Err(_) => {
            return ValidatePathOutcome::NotFound(ErrorResponse {
                error: format!(
                    "File not found: {rel_path}. Path should be relative to knowledge-base/ (e.g. \"deep-dive/mcp/overview.md\")."
                ),
            });
        }
    };
    if !canonical.starts_with(kb_path) {
        return ValidatePathOutcome::NotFound(ErrorResponse {
            error: "Access denied: path is outside the knowledge base.".to_string(),
        });
    }

    // 3. Extension membership check
    let ext = canonical.extension().and_then(|e| e.to_str()).unwrap_or("");
    if !registry.has_extension(ext) {
        return ValidatePathOutcome::NotFound(ErrorResponse {
            error: format!(
                "Access denied: extension {ext:?} is not in the indexed parser registry. Allowed: {:?}",
                registry.extensions()
            ),
        });
    }

    // 4. Size cap — chosen from the same `ext` that step 3 just accepted, so
    // the two cannot disagree (BU-22).
    let max_bytes = max_bytes_for(registry, ext, binary_max_bytes, text_max_bytes);
    match std::fs::metadata(&canonical) {
        Ok(meta) if meta.len() > max_bytes => {
            return ValidatePathOutcome::NotFound(ErrorResponse {
                error: format!(
                    "File too large: {} bytes (max {} bytes).",
                    meta.len(),
                    max_bytes
                ),
            });
        }
        Ok(_) => {}
        Err(e) => {
            return ValidatePathOutcome::NotFound(ErrorResponse {
                error: format!("Failed to stat file: {e}"),
            });
        }
    }

    ValidatePathOutcome::Found(canonical)
}

/// `get_document` ツール用に、拡張子に対応する Parser で `parse_bytes` を呼び、
/// frontmatter + 抽出テキストから DocumentResponse を組む。抽出失敗 (不正 UTF-8 /
/// 暗号化 PDF 等) は `Err` にして handler が既存のエラー応答形式へ流す。
/// 登録されていない拡張子はフォールバックで Markdown parser を使う (pre-feature-20 挙動)。
fn build_document_response(
    registry: &Registry,
    path_hint: &str,
    ext: &str,
    bytes: &[u8],
) -> anyhow::Result<DocumentResponse> {
    let parsed = match registry.by_extension(ext) {
        Some(p) => p.parse_bytes(bytes, path_hint, &[])?,
        None => {
            let s = std::str::from_utf8(bytes)
                .map_err(|e| anyhow::anyhow!("{path_hint}: not valid UTF-8: {e}"))?;
            markdown::parse(s)
        }
    };
    // text 形式: raw_content = ファイル全体 (既存 `content: raw` と一致)。
    // binary 形式: raw_content = 抽出テキスト全体。1 MiB 超は truncate。
    let mut content = parsed.raw_content;
    let truncated = truncate_on_char_boundary(&mut content, EXTRACTED_TEXT_MAX_BYTES);
    Ok(DocumentResponse {
        path: path_hint.to_string(),
        title: parsed.frontmatter.title,
        date: parsed.frontmatter.date,
        topic: parsed.frontmatter.topic,
        tags: parsed.frontmatter.tags,
        content,
        truncated,
    })
}

/// `get_best_practice` のパス解決結果。
#[derive(Debug)]
enum ResolveOutcome {
    /// `canonicalize` 済みのファイル絶対パス。
    Found(PathBuf),
    /// どのテンプレートにもマッチしなかった。試行した相対パス列。
    NotFound(Vec<String>),
    /// security event (= symlink hit) で即 break した。`validate_get_document_path`
    /// から bubble up した `ErrorResponse` を内蔵し、handler は文言生成や prefix 追加
    /// なしで `serde_json::to_string_pretty(&err)` で直接 client に返却する。
    Denied(ErrorResponse),
}

/// Best-practice resolver: テンプレート列に `{target}` を置換してファイルを探す。
/// 先頭から順に試し、`validate_get_document_path` の 4 段階防御 (symlink reject /
/// canonicalize+starts_with / extension membership / size cap) を通過した最初の
/// 候補を返す。`kb_path` は呼び出し側で既に canonicalize されている前提
/// (`run_server` / tests で事前処理)。
///
/// fail 種別の挙動 (F-45):
/// - `Found(p)` → 即 return
/// - `NotFound(_)` (file not found / canonicalize failed / outside-kb / extension
///   denied / size exceeded) → 次 template を試行 (err 文言は捨てて `tried` に
///   rel path のみ記録、info leak ゼロ)
/// - `Denied(err)` (symlink hit = security event) → 即 return `ResolveOutcome::Denied(err)`
///   (= 文言保持、template ordering より security event 優先)
fn resolve_best_practice_path(
    kb_path: &std::path::Path,
    templates: &[String],
    target: &str,
    registry: &Registry,
    max_bytes: u64,
) -> ResolveOutcome {
    let mut tried: Vec<String> = Vec::new();
    for tmpl in templates {
        let rel = tmpl.replace("{target}", target);
        tried.push(rel.clone());
        // Best-practice templates resolve to prose documents; pass the same cap
        // for both classes so this path keeps the single limit it always had.
        match validate_get_document_path(kb_path, &rel, registry, max_bytes, max_bytes) {
            ValidatePathOutcome::Found(p) => return ResolveOutcome::Found(p),
            ValidatePathOutcome::NotFound(_) => continue,
            ValidatePathOutcome::Denied(err) => return ResolveOutcome::Denied(err),
        }
    }
    ResolveOutcome::NotFound(tried)
}

/// Extract the h2 section whose heading contains `category_lower` (case-insensitive).
/// Returns all text from that heading until the next h2 heading.
fn extract_section(content: &str, category: &str) -> Option<String> {
    let cat_lower = category.to_lowercase();
    let mut lines = content.lines();
    let mut found = false;
    let mut section_lines: Vec<&str> = Vec::new();

    for line in &mut lines {
        if line.starts_with("## ") {
            if found {
                // We've hit the next h2 — stop collecting
                break;
            }
            let heading_text = line.trim_start_matches("## ").trim();
            if heading_text.to_lowercase().contains(&cat_lower) {
                found = true;
                section_lines.push(line);
                continue;
            }
        }
        if found {
            section_lines.push(line);
        }
    }

    if found {
        Some(section_lines.join("\n").trim().to_string())
    } else {
        None
    }
}

/// List all h2 headings in the content.
fn list_h2_sections(content: &str) -> Vec<String> {
    content
        .lines()
        .filter(|line| line.starts_with("## "))
        .map(|line| line.trim_start_matches("## ").trim().to_string())
        .collect()
}

// ---------------------------------------------------------------------------
// Server bootstrap
// ---------------------------------------------------------------------------

/// `KbServer` を構成する共有リソース。HTTP トランスポートの
/// service factory が session ごとに `KbServer` を生成するため、重いリソース
/// (DB / embedder / reranker / registry) を 1 回だけロードして Arc で共有する。
#[derive(Clone)]
pub struct KbServerShared {
    pub db: Arc<Mutex<Database>>,
    pub embedder: Arc<Mutex<Embedder>>,
    pub reranker: Arc<Mutex<Option<Reranker>>>,
    pub rerank_by_default: bool,
    pub kb_path: PathBuf,
    pub exclude_headings: Option<Vec<String>>,
    pub exclude_dirs: Vec<String>,
    pub quality_threshold: f32,
    pub best_practice_templates: Vec<String>,
    pub parser_registry: Arc<Registry>,
    pub min_confidence_ratio: f32,
    /// `[search]` セクション (toml) のスナップショット。serve 起動時に Config
    /// から取り出し、shutdown まで不変。`KbServer::from_shared` で clone する。
    pub search_config: crate::config::SearchConfig,
    /// feature-46: index 時の desired context mode (config `[contextual].enabled`
    /// から算出)。`rebuild_index` MCP tool が force 時の adopt 値に使う。非 force は
    /// DB 側モードが優先されるため、この値は force 移行時のみ効く。
    pub context_mode_desired: crate::db::ContextMode,

    // (v0.8.0+, feature-43 PR-2) Fields surfaced by `/api/admin/status`.
    /// Wall-clock daemon start time, used for ISO formatting in admin status.
    pub started_at: std::time::SystemTime,
    /// Monotonic daemon start time, used for uptime calculation (NTP-jump safe).
    pub started_instant: std::time::Instant,
    /// Current indexing operation state, or `None` when idle.
    pub indexing_state: std::sync::Arc<std::sync::Mutex<Option<IndexingState>>>,
    /// `true` while the file watcher loop is running.
    pub watcher_active: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// Watcher debounce window (= `[watch].debounce_ms` from config).
    pub watcher_debounce_ms: u64,
    /// Human-readable label of which source `Config::discover()` used.
    /// e.g. "Explicit" / "Cwd" / "GitRoot" / "AlongsideBinary" / "NotFound".
    pub config_source_label: String,
    /// Hosts allowed by the admin sub-router Host header check.
    /// Always includes the loopback aliases; HTTP transport also adds its bind addr.
    pub allowed_admin_hosts: Vec<String>,
}

#[derive(Debug)]
pub struct IndexingState {
    pub started_at: std::time::SystemTime,
    pub progress: Option<IndexingProgress>,
    /// (codex P2 round 4 on PR #57) Concurrent rebuild_index refcount.
    /// Two HTTP clients can both reach `rebuild_index` before the first
    /// finishes — without a refcount, the first caller's Drop guard would
    /// clear the shared slot while the second is still running. Now: start
    /// = `Some(count=1)` or `+=1` on existing; drop = `-=1` and clear to
    /// `None` only when reaching 0.
    pub active_count: u32,
}

#[derive(Clone, Debug)]
pub struct IndexingProgress {
    pub current: u64,
    pub total: u64,
}

impl KbServer {
    /// Shared state から新しい `KbServer` を組み立てる。
    /// Arc::clone で軽量、embedder / reranker モデルの重複ロードは起きない。
    pub fn from_shared(shared: &KbServerShared) -> Self {
        Self {
            core: Arc::new(KbCore {
                db: Arc::clone(&shared.db),
                embedder: Arc::clone(&shared.embedder),
                reranker: Arc::clone(&shared.reranker),
                rerank_by_default: shared.rerank_by_default,
                kb_path: shared.kb_path.clone(),
                exclude_headings: shared.exclude_headings.clone(),
                exclude_dirs: shared.exclude_dirs.clone(),
                quality_threshold: shared.quality_threshold,
                best_practice_templates: shared.best_practice_templates.clone(),
                parser_registry: Arc::clone(&shared.parser_registry),
                min_confidence_ratio: shared.min_confidence_ratio,
                search_config: shared.search_config.clone(),
                context_mode_desired: shared.context_mode_desired,
                indexing_state: Arc::clone(&shared.indexing_state),
            }),
            tool_router: KbServer::tool_router(),
        }
    }
}

/// (feature-43 PR-2) Snapshot of KB-level info for the admin
/// `/api/admin/status` endpoint. Counts are `Option` because `rebuild_index`
/// holds the db / embedder mutex for the duration of the rebuild, so a
/// concurrent admin-status request must not block — it returns `None` to
/// signal "unavailable, retry after indexing completes" (codex P2 round 2
/// on PR #57).
#[derive(Debug, Clone, serde::Serialize)]
pub struct KbInfo {
    pub path: String,
    pub documents: Option<u64>,
    pub chunks: Option<u64>,
    pub model: Option<String>,
}

impl KbServerShared {
    /// (feature-43 PR-2) Best-effort snapshot of KB stats. Uses `try_lock`
    /// on `db` / `embedder` so a long-running `rebuild_index` does not stall
    /// the admin status response — busy locks yield `None` instead of waiting.
    ///
    /// (BU-18) A poisoned lock is *not* the same as a busy one, and `.ok()`
    /// treated them alike: after any panic under these mutexes, admin status
    /// would report `documents: null` forever and read as "a rebuild is
    /// running". `recover_*_try` keeps `None` meaning busy.
    pub fn kb_info(&self) -> Result<KbInfo> {
        let (documents, chunks) = match recover_db_try(self.db.try_lock()) {
            Some(db) => {
                let docs = db.document_count().ok().map(|n| n as u64);
                let chks = db.chunk_count().ok().map(|n| n as u64);
                (docs, chks)
            }
            None => (None, None),
        };
        let model =
            recover_try(self.embedder.try_lock(), "embedder").map(|e| e.model_id().to_string());
        Ok(KbInfo {
            path: self.kb_path.display().to_string(),
            documents,
            chunks,
            model,
        })
    }
}

/// (feature-43 PR-2) Plain-JSON search entry for the WebUI `/api/search` POST.
///
/// Constructs a minimal `SearchParams` (= query + limit only) and dispatches
/// through the same `KbServer::search` tool method the MCP clients use.
/// Returns the raw JSON string from `KbServer::search` (already
/// pretty-printed `SearchResponse` or `ErrorResponse`).
///
/// Defined as a free function (instead of a method on `KbServerShared`) so
/// the private `SearchParams` type does not need to leak to the public API
/// of `KbServerShared`. Same-module access to `SearchParams` + `pub(crate)`
/// `KbServer::search` keeps the MCP tool surface untouched.
pub async fn web_search(shared: &KbServerShared, query: String, limit: Option<u32>) -> String {
    let kb_server = KbServer::from_shared(shared);
    let params = SearchParams {
        query,
        limit,
        ..Default::default()
    };
    kb_server.search(Parameters(params)).await
}

#[cfg(any(test, feature = "test-helpers"))]
impl KbServerShared {
    /// (feature-43 PR-2) Test-only minimal constructor. Production code paths
    /// go through `run_server`; this helper exists so integration tests can
    /// build a `KbServerShared` without booting the full transport stack.
    /// All non-essential fields are filled with defaults (loopback-only admin
    /// allow-list, no reranker, no watcher).
    pub fn for_test(db: Database, embedder: Embedder, kb_path: PathBuf) -> Self {
        use std::sync::atomic::AtomicBool;
        use std::time::{Instant, SystemTime};
        Self {
            db: Arc::new(Mutex::new(db)),
            embedder: Arc::new(Mutex::new(embedder)),
            reranker: Arc::new(Mutex::new(None)),
            rerank_by_default: false,
            kb_path,
            exclude_headings: None,
            exclude_dirs: vec![],
            quality_threshold: 0.0,
            best_practice_templates: vec![],
            parser_registry: Arc::new(Registry::default()),
            min_confidence_ratio: 1.5,
            search_config: crate::config::SearchConfig::default(),
            context_mode_desired: crate::db::ContextMode::Off,
            started_at: SystemTime::now(),
            started_instant: Instant::now(),
            indexing_state: Arc::new(Mutex::new(None)),
            watcher_active: Arc::new(AtomicBool::new(false)),
            watcher_debounce_ms: 500,
            config_source_label: "TestStub".into(),
            allowed_admin_hosts: vec!["127.0.0.1".into(), "::1".into(), "localhost".into()],
        }
    }
}

/// Run the MCP server on the selected transport.
#[allow(clippy::too_many_arguments)]
pub async fn run_server(
    kb_path: &std::path::Path,
    model: ModelChoice,
    reranker_choice: RerankerChoice,
    rerank_by_default: bool,
    exclude_headings: Option<Vec<String>>,
    exclude_dirs: Vec<String>,
    quality_threshold: f32,
    best_practice_templates: Vec<String>,
    parser_registry: Registry,
    watch_config: crate::watcher::WatchConfig,
    transport: crate::transport::Transport,
    min_confidence_ratio: f32,
    search_config: crate::config::SearchConfig,
    config_source: crate::config::ConfigSource,
    context_mode_desired: crate::db::ContextMode,
) -> Result<()> {
    use std::sync::atomic::AtomicBool;
    use std::time::{Instant, SystemTime};

    let db_path = crate::resolve_db_path(kb_path);
    let db = Database::open(&db_path.to_string_lossy())?;

    // モデル DL の前に meta 整合性を確認。不整合ならここで止めて DL を回避。
    db.verify_embedding_meta(model.model_id(), model.dimension() as u32)?;

    // codex P2 (PR #73 F2): fresh DB (chunk 0 件、`index` 未実行のまま `serve`
    // 起動) かつ `[contextual] enabled = true` の場合、watcher 経由の
    // `reindex_single_file` は `read_context_mode()` が `None` を返すたびに
    // `Off` へ fallback していた (= grandfather 判定は「レコードが無い」を
    // legacy DB とみなす設計だが、fresh DB では正しくない)。watcher が動く前に
    // ここで一度 resolve しておけば、fresh DB では desired mode が記録され、
    // 既存 DB では従来通り grandfather / warning 挙動のままになる。watcher 自体は
    // 「DB に記録された mode に追従する」設計を変えない (呼ぶのはここ 1 回だけ)。
    indexer::resolve_context_mode(&db, context_mode_desired, false)?;

    // AU-06 (codex P2): `[parsers].enabled` を狭めた後、その拡張子の行は DB に
    // 残る。`kb-mcp index` なら prune されるが `serve` は index しないので、
    // serve しか使わない運用では残り続け、**search には出るのに
    // `get_document` が拒否する** hit になる (AU-02 と同じ「見つかるのに
    // 開けない」)。`.xls` の取り下げでこの経路に入る人が実際に出るので、
    // 起動時に一度だけ数えて知らせる。**消しはしない** — 狭めた設定は一時的な
    // こともあり、起動のたびに黙って行を消す方が危ない。
    if let Ok(all_paths) = db.all_document_paths() {
        let stale = indexer::paths_with_unregistered_extension(&all_paths, &parser_registry);
        if !stale.is_empty() {
            let sample: Vec<&str> = stale.iter().take(3).map(String::as_str).collect();
            tracing::warn!(
                "{} indexed document(s) have an extension that [parsers].enabled no longer \
                 covers (e.g. {}). They still appear in search results but get_document \
                 rejects them. Run `kb-mcp index` to remove them from the index.",
                stale.len(),
                sample.join(", ")
            );
        }
    }

    let embedder = Embedder::with_model(model)?;
    let reranker = Reranker::try_new(reranker_choice)?;

    let kb_path = kb_path
        .canonicalize()
        .unwrap_or_else(|_| kb_path.to_path_buf());

    // (feature-43 PR-2) prepare admin/status auxiliary state before the
    // KbServerShared literal so we can pass an Arc::clone into the watcher.
    let watcher_active = Arc::new(AtomicBool::new(false));
    let watcher_debounce_ms = watch_config.debounce_ms;
    let allowed_admin_hosts = {
        let mut hosts = vec![
            "127.0.0.1".to_string(),
            "::1".to_string(),
            "localhost".to_string(),
        ];
        // codex P1 round 4 on PR #57: only include the bind addr when it is
        // a loopback IP. Otherwise a non-loopback bind (e.g. 192.168.1.10:3100
        // or 0.0.0.0:3100) would let LAN browsers reach /ui + /api/admin/status
        // via the bind addr Host header — that contradicts the spec § 7
        // "admin is loopback-only" decision and the install-time Note that
        // promises LAN browsers see 403.
        if let crate::transport::Transport::Http { addr, .. } = &transport
            && addr.ip().is_loopback()
        {
            let bind_str = addr.to_string();
            let ip_str = addr.ip().to_string();
            if !hosts.contains(&bind_str) {
                hosts.push(bind_str);
            }
            if !hosts.contains(&ip_str) {
                hosts.push(ip_str);
            }
        }
        hosts
    };

    // watcher と共有するため Arc 化。
    // HTTP service factory でも共有するため KbServerShared にまとめる。
    let shared = KbServerShared {
        db: Arc::new(Mutex::new(db)),
        embedder: Arc::new(Mutex::new(embedder)),
        reranker: Arc::new(Mutex::new(reranker)),
        rerank_by_default,
        kb_path: kb_path.clone(),
        exclude_headings,
        exclude_dirs,
        quality_threshold,
        best_practice_templates,
        parser_registry: Arc::new(parser_registry),
        min_confidence_ratio,
        search_config,
        context_mode_desired,
        started_at: SystemTime::now(),
        started_instant: Instant::now(),
        indexing_state: Arc::new(Mutex::new(None)),
        watcher_active: Arc::clone(&watcher_active),
        watcher_debounce_ms,
        config_source_label: format!("{:?}", config_source),
        allowed_admin_hosts,
    };

    // watcher をバックグラウンドで並走。
    let watcher_state = crate::watcher::WatcherState {
        kb_path: kb_path.clone(),
        db: Arc::clone(&shared.db),
        embedder: Arc::clone(&shared.embedder),
        registry: Arc::clone(&shared.parser_registry),
        exclude_headings: shared.exclude_headings.clone(),
        // (feature-49) `.kb-mcpignore` を起動時に 1 度読む。以後、ファイル自体が
        // 書き換わったら watcher が組み直す。`rebuild_index` 側は毎回読み直すので
        // ここの値は共有しない。
        rules: crate::exclusion::ExclusionRules::load(&kb_path, shared.exclude_dirs.clone()),
        config: watch_config,
        watcher_active: Arc::clone(&watcher_active),
    };
    let watcher_handle = tokio::spawn(async move {
        if let Err(e) = crate::watcher::run_watch_loop(watcher_state).await {
            eprintln!("watcher exited with error: {e}");
        }
    });

    let result = match transport {
        crate::transport::Transport::Stdio => crate::transport::stdio::run_stdio(&shared).await,
        crate::transport::Transport::Http {
            addr,
            allowed_hosts,
            healthz_public,
            max_sessions,
        } => {
            // move shared to http runner (no clone needed — stdio branch
            // consumes it only by reference and is mutually exclusive).
            crate::transport::http::run_http(
                addr,
                allowed_hosts,
                healthz_public,
                max_sessions,
                shared,
            )
            .await
        }
    };
    watcher_handle.abort();
    result
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// 一意な tempdir を作って kb_path として返す。Drop で削除。
    struct TempKb {
        path: PathBuf,
    }
    impl TempKb {
        fn new(prefix: &str) -> Self {
            let path = crate::test_support::unique_temp_path(&format!("kb-mcp-srvtest-{prefix}"));
            fs::create_dir_all(&path).unwrap();
            let canon = path.canonicalize().unwrap();
            Self { path: canon }
        }
        fn write(&self, rel: &str, content: &str) -> PathBuf {
            let full = self.path.join(rel);
            if let Some(parent) = full.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::write(&full, content).unwrap();
            full
        }
    }
    impl Drop for TempKb {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    #[test]
    fn test_resolve_best_practice_first_template_hit() {
        let kb = TempKb::new("bp1");
        kb.write("best-practices/claude-code/PERFECT.md", "# CC\n");
        let templates = vec!["best-practices/{target}/PERFECT.md".to_string()];
        let r = resolve_best_practice_path(
            &kb.path,
            &templates,
            "claude-code",
            &md_only_registry(),
            1024 * 1024,
        );
        match r {
            ResolveOutcome::Found(p) => {
                assert!(
                    p.ends_with("best-practices/claude-code/PERFECT.md")
                        || p.ends_with("best-practices\\claude-code\\PERFECT.md")
                );
            }
            other => panic!("expected Found, got {other:?}"),
        }
    }

    #[test]
    fn test_resolve_best_practice_falls_through_to_second_template() {
        let kb = TempKb::new("bp2");
        kb.write("docs/cursor.md", "# cursor\n");
        let templates = vec![
            "best-practices/{target}/PERFECT.md".to_string(), // 不存在
            "docs/{target}.md".to_string(),                   // ヒット
        ];
        let r = resolve_best_practice_path(
            &kb.path,
            &templates,
            "cursor",
            &md_only_registry(),
            1024 * 1024,
        );
        match r {
            ResolveOutcome::Found(p) => {
                assert!(p.ends_with("docs/cursor.md") || p.ends_with("docs\\cursor.md"))
            }
            other => panic!("expected Found, got {other:?}"),
        }
    }

    #[test]
    fn test_resolve_best_practice_traversal_rejected() {
        let kb = TempKb::new("bp3");
        // kb_path の外側にファイルを作る (親ディレクトリに)
        let outside = kb.path.parent().unwrap().join(format!(
            "kb-mcp-srvtest-outside-{}.md",
            crate::test_support::unique_suffix()
        ));
        fs::write(&outside, "secret").unwrap();

        // `{target}` に `../<ファイル名>` を入れて kb 外を指す
        let target_rel = format!("../{}", outside.file_name().unwrap().to_string_lossy());
        let templates = vec!["{target}".to_string()];
        let r = resolve_best_practice_path(
            &kb.path,
            &templates,
            &target_rel,
            &md_only_registry(),
            1024 * 1024,
        );
        // 実ファイルは存在するが kb_path 配下ではないので拒否される
        match r {
            ResolveOutcome::NotFound(tried) => {
                assert_eq!(tried.len(), 1);
            }
            other => panic!("traversal was not rejected: {other:?}"),
        }
        let _ = fs::remove_file(&outside);
    }

    #[test]
    fn test_resolve_best_practice_all_missing_returns_tried_list() {
        let kb = TempKb::new("bp4");
        let templates = vec!["a/{target}.md".to_string(), "b/{target}.md".to_string()];
        let r = resolve_best_practice_path(
            &kb.path,
            &templates,
            "nope",
            &md_only_registry(),
            1024 * 1024,
        );
        match r {
            ResolveOutcome::NotFound(tried) => {
                assert_eq!(
                    tried,
                    vec!["a/nope.md".to_string(), "b/nope.md".to_string()]
                );
            }
            other => panic!("expected NotFound, got {other:?}"),
        }
    }

    #[test]
    fn test_resolve_best_practice_empty_templates_returns_empty_tried() {
        let kb = TempKb::new("bp5");
        let r = resolve_best_practice_path(&kb.path, &[], "any", &md_only_registry(), 1024 * 1024);
        match r {
            ResolveOutcome::NotFound(tried) => assert!(tried.is_empty()),
            other => panic!("expected NotFound, got {other:?}"),
        }
    }

    // -----------------------------------------------------------------------
    // F-45: get_best_practice hardening (4 段階防御 integration smoke)
    //
    // 役割分担: validate_get_document_path の specific branch evidence は
    // 既存 5 `test_validate_get_document_path_*` (`err.error.contains("...")`
    // で branch 識別) でカバー済。本 4 test は resolve_best_practice_path の
    // template loop semantics (NotFound → try next / Denied → break) が
    // 正しく動作する integration smoke。
    // -----------------------------------------------------------------------

    #[cfg(unix)]
    #[test]
    fn test_resolve_best_practice_rejects_symlink_template() {
        // best-practice template が symlink を指す場合は Denied で即 break
        let kb = TempKb::new("bp-sym");
        let target = kb.write("real.md", "# real\n");
        let link = kb.path.join("link.md");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let templates = vec!["link.md".to_string()];
        let r = resolve_best_practice_path(
            &kb.path,
            &templates,
            "any",
            &md_only_registry(),
            1024 * 1024,
        );
        match r {
            ResolveOutcome::Denied(err) => {
                assert!(
                    err.error.contains("symlinks are not allowed"),
                    "expected symlink Denied, got: {}",
                    err.error
                );
            }
            other => panic!("expected Denied, got {other:?}"),
        }
    }

    #[test]
    fn test_resolve_best_practice_rejects_oversized_file() {
        // max_bytes=1024 + 2 KiB ファイル で size branch を踏ませる
        // (= NotFound 経由 try next / 全 template fail で NotFound(tried))
        let kb = TempKb::new("bp-size");
        let big = "a".repeat(2 * 1024);
        kb.write("docs/big.md", &big);
        let templates = vec!["docs/big.md".to_string()];
        let r = resolve_best_practice_path(
            &kb.path,
            &templates,
            "any",
            &md_only_registry(),
            1024, // max_bytes を small にして size cap を発火
        );
        match r {
            ResolveOutcome::NotFound(tried) => {
                assert_eq!(tried.len(), 1);
                assert_eq!(tried[0], "docs/big.md");
            }
            other => panic!("expected NotFound, got {other:?}"),
        }
    }

    #[test]
    fn test_resolve_best_practice_rejects_extension_outside_registry() {
        // registry が .md のみ、template が .txt を指す → NotFound 経由 try next
        let kb = TempKb::new("bp-ext");
        kb.write("notes.txt", "plain text\n");
        let templates = vec!["notes.txt".to_string()];
        let r = resolve_best_practice_path(
            &kb.path,
            &templates,
            "any",
            &md_only_registry(),
            1024 * 1024,
        );
        match r {
            ResolveOutcome::NotFound(tried) => {
                assert_eq!(tried.len(), 1);
                assert_eq!(tried[0], "notes.txt");
            }
            other => panic!("expected NotFound, got {other:?}"),
        }
    }

    #[test]
    fn test_resolve_best_practice_rejects_traversal_outside_kb() {
        // kb_path 外に実ファイル + template に "../<filename>" 形式
        // (canonicalize 成功 → starts_with 失敗 branch、Windows でも portable)
        let kb = TempKb::new("bp-trav");
        let outside = kb.path.parent().unwrap().join(format!(
            "kb-mcp-srvtest-bp-outside-{}.md",
            crate::test_support::unique_suffix()
        ));
        fs::write(&outside, "secret").unwrap();
        let target_rel = format!("../{}", outside.file_name().unwrap().to_string_lossy());
        let templates = vec!["{target}".to_string()];
        let r = resolve_best_practice_path(
            &kb.path,
            &templates,
            &target_rel,
            &md_only_registry(),
            1024 * 1024,
        );
        match r {
            ResolveOutcome::NotFound(tried) => {
                assert_eq!(tried.len(), 1);
            }
            other => panic!("traversal was not rejected: {other:?}"),
        }
        let _ = fs::remove_file(&outside);
    }

    // -----------------------------------------------------------------------
    // build_document_response の拡張子認識
    // evaluator 指摘 High #1: .txt で title が落ちる不整合を防ぐ回帰テスト。
    // -----------------------------------------------------------------------

    #[test]
    fn test_build_document_response_md_with_frontmatter() {
        let reg = Registry::from_enabled(&["md".into(), "txt".into()]).unwrap();
        let md = "---\ntitle: Hello\ntags: [a, b]\n---\n\n# body";
        let resp = build_document_response(&reg, "notes/hello.md", "md", md.as_bytes()).unwrap();
        assert_eq!(resp.title.as_deref(), Some("Hello"));
        assert_eq!(resp.tags, vec!["a".to_string(), "b".to_string()]);
        assert_eq!(resp.path, "notes/hello.md");
        assert!(resp.content.contains("# body"));
    }

    #[test]
    fn test_build_document_response_txt_derives_title_from_filename() {
        let reg = Registry::from_enabled(&["md".into(), "txt".into()]).unwrap();
        let raw = "forest ecosystem notes body.";
        let resp = build_document_response(
            &reg,
            "nature/forest-ecosystem-notes.txt",
            "txt",
            raw.as_bytes(),
        )
        .unwrap();
        // .txt has no frontmatter — title must come from the filename
        assert_eq!(
            resp.title.as_deref(),
            Some("forest ecosystem notes"),
            "search and get_document must return the same derived title"
        );
        assert!(resp.date.is_none());
        assert!(resp.tags.is_empty());
        assert_eq!(resp.content, raw);
    }

    #[test]
    fn test_build_document_response_unknown_ext_falls_back_to_markdown() {
        // 登録外の拡張子は markdown::parse にフォールバック (legacy 相当)。
        // 通常は collect_source_files が registry の extensions しか拾わないため
        // 到達しないが、外部からの直接 path 指定でも落ちないように。
        let reg = Registry::defaults(); // md only
        let raw = "---\ntitle: x\n---\n\nbody";
        let resp = build_document_response(&reg, "a.unknown", "unknown", raw.as_bytes()).unwrap();
        // markdown::parse が frontmatter を拾う
        assert_eq!(resp.title.as_deref(), Some("x"));
    }

    #[test]
    fn test_truncate_on_char_boundary_respects_multibyte() {
        // "あ" = 3 bytes。max_bytes=4 → 1 文字 (3 bytes) で止まり panic しない。
        let mut s = "あああ".to_string(); // 9 bytes
        let truncated = truncate_on_char_boundary(&mut s, 4);
        assert!(truncated);
        assert_eq!(s, "あ");
        // 上限が長さ以上なら無切り詰め。
        let mut s2 = "abc".to_string();
        assert!(!truncate_on_char_boundary(&mut s2, 100));
        assert_eq!(s2, "abc");
    }

    #[test]
    fn test_build_document_response_text_format_unchanged() {
        let reg = Registry::from_enabled(&["md".into()]).unwrap();
        let raw = "---\ntitle: T\n---\n\n## H\n\nbody enough enough enough enough enough";
        let resp = build_document_response(&reg, "a.md", "md", raw.as_bytes()).unwrap();
        assert_eq!(resp.title.as_deref(), Some("T"));
        assert_eq!(
            resp.content, raw,
            "text content must be the full raw file (unchanged)"
        );
        assert!(!resp.truncated);
    }

    #[test]
    fn test_build_document_response_invalid_utf8_text_is_err() {
        let reg = Registry::from_enabled(&["md".into()]).unwrap();
        let err = build_document_response(&reg, "a.md", "md", &[0xff, 0xfe]).unwrap_err();
        assert!(err.to_string().contains("not valid UTF-8"));
    }

    // -----------------------------------------------------------------------
    // compile_path_globs: SearchParams.path_globs -> CompiledPathGlobs
    // -----------------------------------------------------------------------

    #[test]
    fn test_compile_path_globs_include_only() {
        let cpg = compile_path_globs(&["docs/**".into()]).unwrap();
        assert!(cpg.matches("docs/a.md"));
        assert!(!cpg.matches("notes/a.md"));
    }

    #[test]
    fn test_compile_path_globs_with_exclude() {
        let cpg = compile_path_globs(&["docs/**".into(), "!docs/draft/**".into()]).unwrap();
        assert!(cpg.matches("docs/a.md"));
        assert!(!cpg.matches("docs/draft/b.md"));
        assert!(!cpg.matches("notes/c.md"));
    }

    #[test]
    fn test_compile_path_globs_empty_array_is_error() {
        let err = compile_path_globs(&[]).unwrap_err();
        assert!(err.to_string().contains("path_globs cannot be empty"));
    }

    // ---- AU-17: list 型 filter の上限 ----

    #[test]
    fn a_filter_list_at_the_limit_is_accepted() {
        let items: Vec<String> = (0..FILTER_LIST_MAX_ITEMS)
            .map(|i| format!("docs/d{i}/**"))
            .collect();
        assert!(validate_filter_list("path_globs", &items).is_ok());
        assert!(compile_path_globs(&items).is_ok());
    }

    #[test]
    fn a_filter_list_over_the_limit_is_refused() {
        let items: Vec<String> = (0..FILTER_LIST_MAX_ITEMS + 1)
            .map(|i| format!("docs/d{i}/**"))
            .collect();
        let err = validate_filter_list("tags_any", &items).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("tags_any"),
            "message should name the field: {msg}"
        );
        assert!(
            msg.contains("too many entries"),
            "message should say what is wrong: {msg}"
        );
        // path_globs は compile 側でも同じ上限に当たる (= CLI もここで守られる)。
        assert!(compile_path_globs(&items).is_err());
    }

    #[test]
    fn a_single_oversized_entry_is_refused() {
        // 件数だけ絞っても、巨大な glob 1 本で同じだけ CPU を焼ける。
        let huge = format!("a{}b", "*?".repeat(FILTER_ITEM_MAX_BYTES));
        let err = validate_filter_list("path_globs", std::slice::from_ref(&huge)).unwrap_err();
        assert!(
            err.to_string().contains("too large"),
            "unexpected message: {err}"
        );
        assert!(compile_path_globs(&[huge]).is_err());
    }

    #[test]
    fn an_entry_exactly_at_the_byte_limit_is_accepted() {
        let at_limit = "a".repeat(FILTER_ITEM_MAX_BYTES);
        assert_eq!(at_limit.len(), FILTER_ITEM_MAX_BYTES);
        assert!(validate_filter_list("tags_all", &[at_limit]).is_ok());
    }

    #[test]
    fn an_empty_filter_list_passes_validation() {
        // 「filter 無効」を表す空配列は、上限の観点では常に OK。
        // (`path_globs` の空配列は compile_path_globs 側で別途エラーになる)
        assert!(validate_filter_list("tags_any", &[]).is_ok());
    }

    #[test]
    fn test_compile_path_globs_only_excludes_warns() {
        // include なし (全部 `!` prefix) は実装としてはエラーにしない、
        // 「全件 include + これらを exclude」と解釈する。
        let cpg = compile_path_globs(&["!docs/draft/**".into()]).unwrap();
        assert!(cpg.matches("docs/a.md")); // include 無 = 全 include
        assert!(!cpg.matches("docs/draft/b.md")); // exclude 効く
    }

    // -----------------------------------------------------------------------
    // compute_match_spans: ASCII-only highlight offset computation
    // -----------------------------------------------------------------------

    #[test]
    fn test_compute_match_spans_ascii_basic() {
        let spans = compute_match_spans("tokio spawn", "use tokio::spawn for async");
        let s = spans.expect("ASCII query -> Some");
        assert_eq!(s.len(), 2);
        assert_eq!(&"use tokio::spawn for async"[s[0].start..s[0].end], "tokio");
        assert_eq!(&"use tokio::spawn for async"[s[1].start..s[1].end], "spawn");
    }

    /// feature-48 (codex review P2, PR #134): quote 構文は FTS 側では逐語 phrase に
    /// なるので、span 側も同じ 1 語として探さなければならない。生クエリを whitespace で
    /// 割ると `"Foundry` / `Local"` を探して 0 件になり、FTS は当たっているのに citation の
    /// offset だけが消える。
    #[test]
    fn test_compute_match_spans_follows_the_quote_syntax() {
        let content = "Foundry Local runs models on device";
        let spans = compute_match_spans("\"Foundry Local\"", content).expect("ASCII query -> Some");
        assert_eq!(spans.len(), 1, "quoted region is one phrase, not two terms");
        assert_eq!(&content[spans[0].start..spans[0].end], "Foundry Local");
    }

    /// quote を使わないクエリの span は従来どおり語ごと。
    #[test]
    fn test_compute_match_spans_still_splits_an_unquoted_query() {
        let content = "Foundry Local runs models on device";
        let spans = compute_match_spans("Foundry Local", content).unwrap();
        assert_eq!(spans.len(), 2);
        assert_eq!(&content[spans[0].start..spans[0].end], "Foundry");
        assert_eq!(&content[spans[1].start..spans[1].end], "Local");
    }

    #[test]
    fn test_compute_match_spans_case_insensitive_ascii() {
        let spans = compute_match_spans("Rust", "RUST is rusty").unwrap();
        assert_eq!(spans.len(), 2);
        assert_eq!(&"RUST is rusty"[spans[0].start..spans[0].end], "RUST");
        assert_eq!(&"RUST is rusty"[spans[1].start..spans[1].end], "rust");
    }

    #[test]
    fn test_compute_match_spans_non_ascii_query_returns_none() {
        // 日本語 (non-ASCII) を含む query は計算しない。
        let spans = compute_match_spans("rust 日本語", "rust と日本語");
        assert!(spans.is_none());
    }

    #[test]
    fn test_compute_match_spans_ascii_query_in_utf8_chunk() {
        // 日本語混じり chunk に ASCII term。byte offset が char boundary を満たすこと。
        let chunk = "前置 tokio 後ろ";
        let spans = compute_match_spans("tokio", chunk).unwrap();
        assert_eq!(spans.len(), 1);
        let s = &spans[0];
        assert!(chunk.is_char_boundary(s.start));
        assert!(chunk.is_char_boundary(s.end));
        assert_eq!(&chunk[s.start..s.end], "tokio");
    }

    #[test]
    fn test_compute_match_spans_empty_query_returns_none() {
        // 空クエリは Some(vec![]) でも None でもよいが、None を採用 (計算未実施扱い)
        let spans = compute_match_spans("", "anything");
        assert!(spans.is_none());
    }

    #[test]
    fn test_compute_match_spans_no_match_returns_empty_vec() {
        let spans = compute_match_spans("nonexistent", "rust").unwrap();
        assert_eq!(spans.len(), 0);
    }

    /// F-35: content size cap。`MATCH_SPAN_CONTENT_MAX_BYTES` を超える content
    /// は計算対象外として `None` を返す (= 計算未実施扱い)。
    #[test]
    fn test_compute_match_spans_oversize_content_returns_none() {
        let huge_content = "rust ".repeat(MATCH_SPAN_CONTENT_MAX_BYTES); // 5x cap 以上
        let spans = compute_match_spans("rust", &huge_content);
        assert!(spans.is_none());
    }

    /// F-35: content がちょうど cap 以下なら計算する (境界値)。
    #[test]
    fn test_compute_match_spans_at_cap_content_succeeds() {
        // 全部 'a' で cap ジャストを作る。query "a" は無数にヒットするが、
        // span 数 cap (`MATCH_SPAN_MAX_COUNT`) で打ち切られることを次の test で確認。
        let content = "a".repeat(MATCH_SPAN_CONTENT_MAX_BYTES);
        let spans = compute_match_spans("a", &content);
        assert!(spans.is_some(), "exactly at cap should be processed");
    }

    /// F-35: span 数の上限。1 文字 term × 巨大 content で出る大量一致を
    /// `MATCH_SPAN_MAX_COUNT` で打ち切る。
    #[test]
    fn test_compute_match_spans_count_capped() {
        // 'a' を MATCH_SPAN_MAX_COUNT * 5 個並べる (素朴に伸ばすと cap 超え
        // するので、cap 以内に収める)。
        let count = MATCH_SPAN_MAX_COUNT * 5;
        assert!(
            count <= MATCH_SPAN_CONTENT_MAX_BYTES,
            "test setup precondition"
        );
        let content = "a".repeat(count);
        let spans = compute_match_spans("a", &content).unwrap();
        // dedup で減ることはあるが、cap (= 100) を超えないことだけ保証する。
        assert!(
            spans.len() <= MATCH_SPAN_MAX_COUNT,
            "spans.len()={} should be <= cap={}",
            spans.len(),
            MATCH_SPAN_MAX_COUNT
        );
    }

    // -----------------------------------------------------------------------
    // BU-09 / BU-10: the match-span contract
    // -----------------------------------------------------------------------

    /// `MatchSpan` intentionally has no `PartialEq` (it is a serde wire type),
    /// so tests compare tuples.
    fn tuples(spans: &[crate::db::MatchSpan]) -> Vec<(usize, usize)> {
        spans.iter().map(|s| (s.start, s.end)).collect()
    }

    /// Assert the whole structural contract at once, so every case below gets
    /// all of it rather than whichever clause the author remembered.
    fn assert_span_contract(query: &str, content: &str) -> Vec<crate::db::MatchSpan> {
        let spans = match compute_match_spans(query, content) {
            Some(s) => s,
            None => return Vec::new(),
        };
        for w in spans.windows(2) {
            assert!(
                w[0].end <= w[1].start,
                "spans must be sorted and disjoint, got {:?} then {:?} for query {query:?}",
                w[0],
                w[1]
            );
        }
        for s in &spans {
            assert!(s.start < s.end, "empty span {s:?} for query {query:?}");
            assert!(
                content.is_char_boundary(s.start) && content.is_char_boundary(s.end),
                "span {s:?} is not on a char boundary for query {query:?}"
            );
        }
        assert!(
            spans.len() <= MATCH_SPAN_MAX_COUNT,
            "spans.len()={} exceeds the published cap {MATCH_SPAN_MAX_COUNT} for query {query:?}",
            spans.len()
        );
        spans
    }

    /// (BU-09) Overlapping matches are folded into their union.
    ///
    /// feature-48 made the term list come from `query_phrases`, which emits
    /// nested phrases — a quoted phrase and a bare word that is a prefix of it
    /// both end up as terms. The old `dedup_by` only removed byte-identical
    /// spans, so a client received `(0,7)` and `(0,13)` for the same text and
    /// had to guess what to do with them.
    #[test]
    fn overlapping_matches_are_merged_into_disjoint_spans() {
        /// `(query, content, expected spans)`.
        type OverlapCase = (&'static str, &'static str, &'static [(usize, usize)]);
        let cases: &[OverlapCase] = &[
            // the audit's example: quoted phrase + the bare word inside it
            (
                "\"Foundry Local\" Foundry",
                "Foundry Local runs the Foundry stack",
                &[(0, 13), (23, 30)],
            ),
            // one term is a prefix of another
            (
                "index indexing",
                "indexing rebuilds the index",
                &[(0, 8), (22, 27)],
            ),
            // partial overlap, neither contained in the other
            ("kb-mcp mcp-server", "see kb-mcp-server here", &[(4, 17)]),
            // three nested terms at the same start
            ("abc abcd abcde", "abcde", &[(0, 5)]),
            // the whitespace-fallback path (no phrase reaches MIN_PHRASE_CHARS)
            ("a ab", "ab", &[(0, 2)]),
        ];
        for (query, content, expected) in cases {
            let spans = assert_span_contract(query, content);
            let got: Vec<(usize, usize)> = spans.iter().map(|s| (s.start, s.end)).collect();
            assert_eq!(got, *expected, "query {query:?} over {content:?}");
        }
    }

    /// The merge predicate is STRICT (`next.start < cur.end`), so spans that
    /// merely touch stay separate.
    ///
    /// This is load-bearing rather than stylistic. With `<=`, the 100 adjacent
    /// one-byte spans of `test_compute_match_spans_count_capped` collapse into
    /// a single span. That test asserts only `len() <= cap`, so it would still
    /// pass — the cap check would silently stop checking anything.
    #[test]
    fn adjacent_spans_are_not_merged() {
        let spans = assert_span_contract("a", &"a".repeat(500));
        assert_eq!(
            spans.len(),
            MATCH_SPAN_MAX_COUNT,
            "100 adjacent single-byte matches must stay 100 separate spans; \
             collapsing them would make the cap test vacuous"
        );
        assert!(spans.iter().all(|s| s.end - s.start == 1));
    }

    /// Folding the contract over its own output changes nothing.
    #[test]
    fn the_span_contract_is_idempotent() {
        for (query, content) in [
            (
                "\"Foundry Local\" Foundry",
                "Foundry Local runs the Foundry stack",
            ),
            ("abc abcd abcde", "abcde abcd abc"),
            ("a", &"a".repeat(500)[..]),
        ] {
            let once = assert_span_contract(query, content);
            let twice = merge_disjoint_spans(once.clone());
            assert_eq!(
                tuples(&once),
                tuples(&twice),
                "query {query:?} is not idempotent"
            );
        }
    }

    /// (BU-10) Reordering the words of a query does not change the answer.
    ///
    /// The old `break 'outer` stopped the whole scan at 100 spans, so whichever
    /// phrase the compiler happened to emit first could consume the entire
    /// budget. Which spans survived was therefore a function of an internal
    /// ordering that feature-48 had just changed.
    #[test]
    fn span_selection_does_not_depend_on_term_order() {
        let content = format!("{}{}", "xyz ".repeat(300), "alpha beta gamma delta epsil");
        let orders = [
            "xyz alpha beta gamma delta epsil",
            "epsil delta gamma beta alpha xyz",
            "gamma xyz epsil alpha delta beta",
        ];
        let first = assert_span_contract(orders[0], &content);
        for q in &orders[1..] {
            let other = assert_span_contract(q, &content);
            assert_eq!(
                tuples(&first),
                tuples(&other),
                "permuting the query changed the spans: {:?} vs {q:?}",
                orders[0]
            );
        }
    }

    /// (BU-10) Every searched phrase that occurs gets at least one span.
    ///
    /// Before, one dense term could eat the whole 100-span budget and the five
    /// rare terms the user also asked about were highlighted nowhere.
    #[test]
    fn every_matching_term_is_covered_by_some_span() {
        let content = format!("{}{}", "xyz ".repeat(300), "alpha beta gamma delta epsil");
        let terms = ["xyz", "alpha", "beta", "gamma", "delta", "epsil"];
        let spans = assert_span_contract(&terms.join(" "), &content);
        for t in terms {
            let covered = content
                .match_indices(t)
                .any(|(at, _)| spans.iter().any(|s| s.start <= at && at < s.end));
            assert!(
                covered,
                "term {t:?} occurs in the content but no span covers any of its \
                 occurrences; the budget was not shared across terms"
            );
        }
    }

    /// The published cap holds when every term is dense.
    ///
    /// The per-term budget must be `floor(cap / k)`, not `ceil`: with 32
    /// phrases `ceil(100/32) = 4` and `4 * 32 = 128` spans, breaking the "at
    /// most 100" promise in README and docs/citations.md. That is a real
    /// mistake made while writing this — the first draft used `div_ceil` and
    /// produced 128 spans on this exact shape. None of the other tests here
    /// reach the cap, so without this one the error ships.
    #[test]
    fn the_cap_holds_when_every_term_is_dense() {
        // 32 distinct 3-char phrases, each occurring ~1000 times. 32 is the
        // `MAX_PHRASES` ceiling in fts_query, so this is the widest term list
        // the phrase path can produce.
        let words: Vec<String> = (0..32).map(|i| format!("w{i:02}")).collect();
        let query = words.join(" ");
        let content = words.join(" ").repeat(1000);
        let spans = assert_span_contract(&query, &content);
        assert!(
            spans.len() > MATCH_SPAN_MAX_COUNT / 2,
            "the budget should still be mostly spent, got {} spans",
            spans.len()
        );
    }

    /// (codex P2 on PR #142) Terms that differ only in case share one budget
    /// slot.
    ///
    /// Matching lowercases both sides, so `Rust` and `rust` find exactly the
    /// same positions. Counting them as two terms halves each one's budget and
    /// then merges the duplicate spans away, so the caller receives 50 spans
    /// where 100 were available. On the fallback path the case variants also
    /// eat slots in the 100-term cutoff, excluding terms that are genuinely
    /// different.
    #[test]
    fn terms_differing_only_in_case_share_a_budget() {
        let content = "rust ".repeat(400);
        let one = assert_span_contract("rust", &content);
        let two = assert_span_contract("Rust rust", &content);
        assert_eq!(
            tuples(&one),
            tuples(&two),
            "`Rust rust` must behave exactly like `rust`; counting the case \
             variants separately wastes half the span budget"
        );
        assert_eq!(
            one.len(),
            MATCH_SPAN_MAX_COUNT,
            "the single-term case should spend the whole budget, or this test \
             is not measuring what it claims"
        );
    }

    /// (codex P2 on PR #142) The term cutoff itself must not depend on word
    /// order.
    ///
    /// `span_selection_does_not_depend_on_term_order` uses six terms, so it
    /// never reaches `MATCH_SPAN_MAX_TERMS` and cannot see this. With more
    /// terms than the clamp allows, a naive `take(100)` keeps whichever were
    /// typed first: reorder the query and a term crosses the cutoff, losing
    /// its highlight. Sorting before truncating is what makes the guarantee
    /// hold at the boundary too.
    #[test]
    fn the_term_cutoff_does_not_depend_on_word_order() {
        let words: Vec<String> = (0..150)
            .map(|i| {
                format!(
                    "{}{}",
                    (b'a' + (i / 26) as u8) as char,
                    (b'a' + (i % 26) as u8) as char
                )
            })
            .collect();
        let content = words.join(" ");
        let forward = words.join(" ");
        let reversed = words.iter().rev().cloned().collect::<Vec<_>>().join(" ");
        assert!(
            crate::db::query_phrases(&forward).is_empty(),
            "precondition: this must exercise the whitespace fallback"
        );
        assert_eq!(
            tuples(&assert_span_contract(&forward, &content)),
            tuples(&assert_span_contract(&reversed, &content)),
            "reversing a query with more terms than MATCH_SPAN_MAX_TERMS changed \
             the spans; the cutoff is following word order"
        );
    }

    /// The cap holds on the whitespace-fallback path, which has no term limit
    /// of its own.
    ///
    /// `query_phrases` caps phrases at 32, but it does **not** apply
    /// `fallback_whole_query` — that is `build_fts_query`'s job — so a query
    /// whose fragments are all below the trigram floor yields no phrases at
    /// all and `compute_match_spans` falls back to `split_whitespace`, which
    /// is unbounded. With a per-term budget of at least one span, 150 terms
    /// would mean 150 spans unless the term list is clamped. Without
    /// `MATCH_SPAN_MAX_TERMS` this test is the only thing between a long
    /// query and a broken cap.
    #[test]
    fn the_cap_holds_on_the_unbounded_whitespace_fallback() {
        let words: Vec<String> = (0..150)
            .map(|i| {
                format!(
                    "{}{}",
                    (b'a' + (i / 26) as u8) as char,
                    (b'a' + (i % 26) as u8) as char
                )
            })
            .collect();
        let query = words.join(" ");
        // Anti-vacuity: this must actually take the fallback path. If
        // `query_phrases` ever starts returning phrases here, the test is
        // exercising something else and should be rewritten, not deleted.
        assert!(
            crate::db::query_phrases(&query).is_empty(),
            "precondition: every fragment is below the trigram floor, so no \
             phrase should be produced and the whitespace fallback should run"
        );
        let content = words.join(" ");
        let spans = assert_span_contract(&query, &content);
        assert!(
            !spans.is_empty(),
            "the terms all occur in the content, so something must be highlighted"
        );
    }

    /// A term that matches everywhere must not turn the whole chunk into one
    /// span. Highlighting 100% of a chunk tells the caller nothing, and the
    /// CLI renders span slices verbatim.
    #[test]
    fn merging_does_not_swallow_the_whole_chunk() {
        let content = "ab".repeat(4096);
        let spans = assert_span_contract("ab ba", &content);
        let highlighted: usize = spans.iter().map(|s| s.end - s.start).sum();
        assert!(
            highlighted * 4 < content.len(),
            "spans cover {highlighted} of {} bytes; merging must not approximate \
             \"highlight everything\"",
            content.len()
        );
    }

    // -----------------------------------------------------------------------
    // compute_low_confidence: rank-based ratio judgment
    // -----------------------------------------------------------------------

    #[test]
    fn test_compute_low_confidence_top1_dominant_is_false() {
        // top1=0.6, others=0.1 -> mean=0.225 -> ratio=2.66... > 1.5 -> false
        let scores = [0.6_f32, 0.1, 0.1, 0.1];
        assert!(!compute_low_confidence(&scores, 1.5));
    }

    #[test]
    fn test_compute_low_confidence_flat_distribution_is_true() {
        // 全部同じ -> ratio=1.0 < 1.5 -> true
        let scores = [0.3_f32, 0.3, 0.3, 0.3];
        assert!(compute_low_confidence(&scores, 1.5));
    }

    #[test]
    fn test_compute_low_confidence_single_hit_is_false() {
        // results.len() < 2 -> 判定 skip -> false
        let scores = [0.001_f32];
        assert!(!compute_low_confidence(&scores, 1.5));
    }

    #[test]
    fn test_compute_low_confidence_zero_results_is_false() {
        let scores: [f32; 0] = [];
        assert!(!compute_low_confidence(&scores, 1.5));
    }

    #[test]
    fn test_compute_low_confidence_mean_zero_is_false() {
        // mean <= 0.0 -> フォールバック skip
        let scores = [0.0_f32, 0.0];
        assert!(!compute_low_confidence(&scores, 1.5));
    }

    #[test]
    fn test_compute_low_confidence_ratio_zero_disables_judgment() {
        // ratio=0.0 -> 常に false
        let scores = [0.3_f32, 0.3, 0.3];
        assert!(!compute_low_confidence(&scores, 0.0));
    }

    #[test]
    fn test_compute_low_confidence_order_independent_for_mmr() {
        // MMR (diversity 補正) 後は selection 順 ≠ score 降順。
        // 旧実装は scores[0] を top1 とみなしていたため、低 score の chunk
        // が先頭に来ると false positive / negative を起こした。
        // codex review の指摘: PR #36 の compute_low_confidence は順序非依存
        // (max(scores) を使う) であるべき。
        let sorted = [0.9_f32, 0.5, 0.4]; // score 降順 (MMR off の典型)
        let mmr_reordered = [0.5_f32, 0.9, 0.4]; // MMR で diversity 順に並び替え
        // 同じスコア集合なので結果は一致するはず
        assert_eq!(
            compute_low_confidence(&sorted, 1.5),
            compute_low_confidence(&mmr_reordered, 1.5),
            "compute_low_confidence must be order-independent (MMR safety)"
        );
    }

    /// `prop_compute_low_confidence_order_invariant` の score 上限と
    /// swap_indices 長を単一 source-of-truth で定義する。将来上限を広げる時に
    /// 片方だけ更新して biased shuffle を生むバグを予防 (= `unwrap_or(0)` の
    /// fallback が常に no-op であることの契約)。
    const ORDER_INVARIANT_PROPTEST_MAX_LEN: usize = 20;

    proptest::proptest! {
        /// codex 罠 4 (order-dependent low_confidence) cluster の 2 件目防御。
        /// 任意の score 配列を deterministic shuffle (Fisher-Yates) しても同 result を proptest で固定。
        /// `rand` crate には依存せず、proptest が生成する usize 配列を swap index として使う。
        ///
        /// 既存の example-based test (`test_compute_low_confidence_order_independent_for_mmr`)
        /// と相補的: example test は MMR 由来の具体ケースを documentation 兼で残し、
        /// 本 proptest は default 256 cases で機械的に同 invariant を網羅する。
        #[test]
        fn prop_compute_low_confidence_order_invariant(
            scores in proptest::collection::vec(0.0_f32..=10.0, 0..=ORDER_INVARIANT_PROPTEST_MAX_LEN),
            min_ratio in 0.0_f32..=10.0,
            swap_indices in proptest::collection::vec(
                proptest::prelude::any::<usize>(),
                ORDER_INVARIANT_PROPTEST_MAX_LEN,
            ),
        ) {
            let mut shuffled = scores.clone();
            let n = shuffled.len();
            // Durstenfeld variant of Fisher-Yates: i = n-1, n-2, ..., 1 で
            // j = swap_indices[i] % (i+1) ∈ [0, i] と swap。
            // swap_indices.len() == ORDER_INVARIANT_PROPTEST_MAX_LEN なので
            // i < n ≤ MAX_LEN を満たす範囲で `get(i)` は常に Some。
            // `unwrap_or(0)` は契約違反時の defensive fallback (現状到達不能)。
            for i in (1..n).rev() {
                let j = swap_indices.get(i).copied().unwrap_or(0) % (i + 1);
                shuffled.swap(i, j);
            }
            let original_result = compute_low_confidence(&scores, min_ratio);
            let shuffled_result = compute_low_confidence(&shuffled, min_ratio);
            proptest::prop_assert_eq!(original_result, shuffled_result);
        }
    }

    // -----------------------------------------------------------------------
    // validate_get_document_path: F-28 hardening
    // -----------------------------------------------------------------------

    fn md_only_registry() -> Registry {
        Registry::defaults()
    }

    #[test]
    fn test_validate_get_document_path_normal_md_passes() {
        let kb = TempKb::new("gd-ok");
        kb.write("docs/a.md", "# A\nbody\n");
        let r = validate_get_document_path(
            &kb.path,
            "docs/a.md",
            &md_only_registry(),
            1024 * 1024,
            1024 * 1024,
        );
        assert!(
            matches!(r, ValidatePathOutcome::Found(_)),
            "normal .md should pass: {r:?}"
        );
    }

    /// (BU-22) The size cap follows the canonical extension, not the one the
    /// caller typed.
    ///
    /// The two used to be decided in different places from different strings.
    /// Windows 8.3 aliasing makes them disagree for every Office format —
    /// `presentation-deck.pptx` is also reachable as `PRESEN~1.PPT`, and
    /// `.ppt` is not a registered extension, so the text cap was applied to a
    /// file the registry classifies as binary. This asserts the property
    /// without needing 8.3: a binary-class file between the two caps is
    /// accepted, which is only true if the binary cap won.
    #[test]
    fn the_size_cap_follows_the_canonical_extension() {
        let kb = TempKb::new("gd-cap-class");
        let registry =
            crate::parser::Registry::from_enabled(&["md".to_string(), "pdf".to_string()])
                .expect("md + pdf is a valid registry");
        // Contents are irrelevant: validation only stats the file.
        kb.write("big.pdf", &"x".repeat(4096));

        let r = validate_get_document_path(&kb.path, "big.pdf", &registry, 1024, 8192);
        assert!(
            matches!(r, ValidatePathOutcome::Found(_)),
            "a .pdf of 4096 bytes sits above the 1024-byte text cap and below the \
             8192-byte binary cap, so it must be accepted — applying the text cap \
             to a binary-class file is BU-22: {r:?}"
        );

        // ...and the binary cap is a real cap, not an escape hatch.
        let over = validate_get_document_path(&kb.path, "big.pdf", &registry, 1024, 2048);
        assert!(
            matches!(over, ValidatePathOutcome::NotFound(_)),
            "4096 bytes must still be rejected once the binary cap is 2048: {over:?}"
        );
    }

    /// (BU-23) The "not found" answer must not echo the configured templates.
    #[test]
    fn best_practice_miss_does_not_leak_the_configured_paths() {
        let tried = vec![
            "best-practices/rust/PERFECT.md".to_string(),
            "internal/team-only/rust.md".to_string(),
        ];
        let msg = best_practice_not_found_message("rust", &tried);

        for path in &tried {
            assert!(
                !msg.contains(path.as_str()),
                "the reply leaks a configured template path ({path}): {msg}"
            );
        }
        assert!(
            !msg.contains("team-only") && !msg.contains('/'),
            "the reply must not carry any fragment of the configured layout: {msg}"
        );
        assert!(
            msg.contains("2 templates tried"),
            "the caller still needs to tell 'no template matched' from 'not configured': {msg}"
        );
    }

    /// (BU-08) `exclude_dirs` means "not indexed", not "not readable".
    ///
    /// A `.md` file under a default-excluded directory never shows up in search
    /// results, but `get_document` still returns it to a caller who knows the
    /// path — `validate_get_document_path` does not take `exclude_dirs` at all.
    /// That is the intended contract (anything under `kb_path` is readable);
    /// this pins it so the doc comment above and the code cannot drift apart
    /// again, and so a future change to the contract has to be deliberate.
    #[test]
    fn document_in_excluded_dir_is_still_readable() {
        let kb = TempKb::new("gd-excluded");
        // `.obsidian` is in HARDCODED_EXCLUDE_DIRS, so the indexer skips it.
        kb.write(".obsidian/workspace-notes.md", "# Private\nnot indexed\n");
        let r = validate_get_document_path(
            &kb.path,
            ".obsidian/workspace-notes.md",
            &md_only_registry(),
            1024 * 1024,
            1024 * 1024,
        );
        assert!(
            matches!(r, ValidatePathOutcome::Found(_)),
            "a .md under an excluded dir is still readable via get_document — \
             exclude_dirs bounds indexing, not access: {r:?}"
        );
    }

    /// (BU-20) A hard link is a second name for a file that may be outside the
    /// KB, and it defeats every check `get_document` had: it is not a symlink,
    /// it is a regular file, and it canonicalizes to itself — inside the KB.
    /// Creating one needs no read access to the target and no privilege.
    #[test]
    fn a_hard_linked_document_is_refused() {
        let kb = TempKb::new("gd-hardlink");
        kb.write("secret-source.md", "# Secret\nssh-rsa AAAA...\n");
        let link = kb.path.join("notes.md");
        std::fs::hard_link(kb.path.join("secret-source.md"), &link)
            .expect("hard links need no privilege");

        let r = validate_get_document_path(
            &kb.path,
            "notes.md",
            &md_only_registry(),
            1024 * 1024,
            1024 * 1024,
        );
        assert!(
            matches!(r, ValidatePathOutcome::Denied(_)),
            "a hard link must be refused the way a symlink is: {r:?}"
        );

        // And the guard is not simply refusing everything: the same file with
        // one name is readable.
        std::fs::remove_file(&link).unwrap();
        let r = validate_get_document_path(
            &kb.path,
            "secret-source.md",
            &md_only_registry(),
            1024 * 1024,
            1024 * 1024,
        );
        assert!(
            matches!(r, ValidatePathOutcome::Found(_)),
            "an ordinary document must stay readable: {r:?}"
        );
    }

    #[test]
    fn test_validate_get_document_path_rejects_extension_outside_registry() {
        let kb = TempKb::new("gd-ext");
        // .git/config を作って read 可能にしてみる
        kb.write(".git/config", "[user]\n  email = test@example.com\n");
        let err = match validate_get_document_path(
            &kb.path,
            ".git/config",
            &md_only_registry(),
            1024 * 1024,
            1024 * 1024,
        ) {
            ValidatePathOutcome::NotFound(e) => e,
            other => panic!("expected NotFound, got {other:?}"),
        };
        assert!(
            err.error.contains("not in the indexed parser registry"),
            "expected extension reject, got: {}",
            err.error
        );
    }

    #[test]
    fn test_validate_get_document_path_rejects_oversized_file() {
        let kb = TempKb::new("gd-size");
        // max を 1 KiB にして 2 KiB のファイルで超過させる
        let big = "a".repeat(2 * 1024);
        kb.write("big.md", &big);
        let err =
            match validate_get_document_path(&kb.path, "big.md", &md_only_registry(), 1024, 1024) {
                ValidatePathOutcome::NotFound(e) => e,
                other => panic!("expected NotFound, got {other:?}"),
            };
        assert!(
            err.error.contains("File too large"),
            "expected size reject, got: {}",
            err.error
        );
    }

    #[test]
    fn test_validate_get_document_path_rejects_traversal() {
        let kb = TempKb::new("gd-trav");
        // kb_path 外側にファイル作成
        let outside = kb.path.parent().unwrap().join(format!(
            "kb-mcp-srvtest-outside-gd-{}.md",
            crate::test_support::unique_suffix()
        ));
        fs::write(&outside, "secret").unwrap();
        let rel = format!("../{}", outside.file_name().unwrap().to_string_lossy());
        let err = match validate_get_document_path(
            &kb.path,
            &rel,
            &md_only_registry(),
            1024 * 1024,
            1024 * 1024,
        ) {
            ValidatePathOutcome::NotFound(e) => e,
            other => panic!("expected NotFound, got {other:?}"),
        };
        // Either "outside the knowledge base" (canonicalize succeeded) or
        // "File not found" (canonicalize failed because traversal escaped before existing).
        assert!(
            err.error.contains("outside the knowledge base")
                || err.error.contains("File not found"),
            "expected traversal reject, got: {}",
            err.error
        );
        let _ = fs::remove_file(&outside);
    }

    #[cfg(unix)]
    #[test]
    fn test_validate_get_document_path_rejects_symlink() {
        let kb = TempKb::new("gd-sym");
        let target = kb.write("target.md", "# target\n");
        let link = kb.path.join("link.md");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let err = match validate_get_document_path(
            &kb.path,
            "link.md",
            &md_only_registry(),
            1024 * 1024,
            1024 * 1024,
        ) {
            ValidatePathOutcome::Denied(e) => e,
            other => panic!("expected Denied, got {other:?}"),
        };
        assert!(
            err.error.contains("symlinks are not allowed"),
            "expected symlink reject, got: {}",
            err.error
        );
    }

    // -----------------------------------------------------------------------
    // feature-28 Task 2.7: SearchParams MMR fields + From<&SearchParams>
    // -----------------------------------------------------------------------

    #[test]
    fn test_search_params_from_default_overrides_to_none() {
        let p = SearchParams::default();
        let o: crate::config::SearchOverrides = (&p).into();
        assert_eq!(o.mmr, None);
        assert_eq!(o.mmr_lambda, None);
        assert_eq!(o.mmr_same_doc_penalty, None);
        assert_eq!(o.parent_retriever, None);
    }

    #[test]
    fn test_search_params_from_with_overrides() {
        let p = SearchParams {
            mmr: Some(true),
            mmr_lambda: Some(0.5),
            ..SearchParams::default()
        };
        let o: crate::config::SearchOverrides = (&p).into();
        assert_eq!(o.mmr, Some(true));
        assert_eq!(o.mmr_lambda, Some(0.5));
        assert_eq!(o.mmr_same_doc_penalty, None);
        assert_eq!(o.parent_retriever, None);
    }

    #[test]
    fn test_search_params_from_full_overrides() {
        // 全フィールド個別に指定したケースが From で漏れず通ることを確認。
        let p = SearchParams {
            mmr: Some(false),
            mmr_lambda: Some(0.25),
            mmr_same_doc_penalty: Some(0.75),
            parent_retriever: Some(true),
            ..SearchParams::default()
        };
        let o: crate::config::SearchOverrides = (&p).into();
        assert_eq!(o.mmr, Some(false));
        assert_eq!(o.mmr_lambda, Some(0.25));
        assert_eq!(o.mmr_same_doc_penalty, Some(0.75));
        assert_eq!(o.parent_retriever, Some(true));
    }

    // -----------------------------------------------------------------------
    // run_search_pipeline: shared MMR-aware pipeline used by MCP / CLI / eval
    // -----------------------------------------------------------------------

    #[test]
    fn test_compute_reranker_input_limit_mmr_on_returns_pool_size() {
        // codex 罠 1 (Vec::with_capacity(u32::MAX) OOM) cluster の核心防御:
        // MMR on の経路で reranker_input_limit がそのまま pool_size を返すことの
        // 直接検証 (= caller が candidates_pool.len() を渡せば、reranker は
        // 全候補をスコアリングする)。
        assert_eq!(compute_reranker_input_limit(true, 0, 10), 0);
        assert_eq!(compute_reranker_input_limit(true, 50, 10), 50);
        assert_eq!(compute_reranker_input_limit(true, 5000, 10), 5000);
    }

    #[test]
    fn test_compute_reranker_input_limit_mmr_off_returns_limit() {
        // MMR off では pool_size を無視して limit を返すこと。
        assert_eq!(compute_reranker_input_limit(false, 0, 10), 10);
        assert_eq!(compute_reranker_input_limit(false, 50, 10), 10);
        assert_eq!(compute_reranker_input_limit(false, 5000, 10), 10);
    }

    #[test]
    fn test_compute_reranker_input_limit_saturates_at_u32_max() {
        // codex 罠 1 cluster 2 件目防御: pool_size: usize が u32::MAX を超えても
        // u32::MAX で saturate されることを直接 assert。
        // 万一 future caller が usize::MAX を渡しても OOM せず u32::MAX で bound される。
        assert_eq!(compute_reranker_input_limit(true, usize::MAX, 10), u32::MAX);
    }

    #[test]
    fn test_compute_reranker_input_limit_mmr_off_ignores_pool_size() {
        // MMR off では saturate path に入らない。pool=usize::MAX でも limit を返す。
        assert_eq!(compute_reranker_input_limit(false, usize::MAX, 10), 10);
    }

    /// Range validation must fire **before** any DB access — so an
    /// invalid `mmr_lambda` is rejected even when the helper is called with
    /// an empty in-memory DB. This is the unit-level proof that CLI flags
    /// reach the helper: the CLI binds `--mmr-lambda` into
    /// `SearchOverrides.mmr_lambda` and the helper validates here. If the
    /// flag were silently dropped (= the previous P2 bug), an out-of-range
    /// value would never produce an error.
    #[test]
    fn test_run_search_pipeline_rejects_lambda_out_of_range() {
        let db = crate::db::Database::open_in_memory().expect("in-memory db");
        let overrides = crate::config::SearchOverrides {
            mmr: Some(true),
            mmr_lambda: Some(1.5),
            mmr_same_doc_penalty: None,
            parent_retriever: None,
        };
        let toml = crate::config::SearchConfig::default();
        let filters = crate::db::SearchFilters::default();
        let qe = vec![0.0_f32; 384];
        let err = run_search_pipeline(&db, None, "q", &qe, 5, &filters, &overrides, &toml)
            .expect_err("out-of-range lambda must error");
        assert!(
            err.to_string().contains("mmr_lambda out of range"),
            "expected mmr_lambda out-of-range error, got: {err}"
        );
    }

    #[test]
    fn test_run_search_pipeline_rejects_same_doc_penalty_out_of_range() {
        let db = crate::db::Database::open_in_memory().expect("in-memory db");
        let overrides = crate::config::SearchOverrides {
            mmr: Some(true),
            mmr_lambda: None,
            mmr_same_doc_penalty: Some(-0.1),
            parent_retriever: None,
        };
        let toml = crate::config::SearchConfig::default();
        let filters = crate::db::SearchFilters::default();
        let qe = vec![0.0_f32; 384];
        let err = run_search_pipeline(&db, None, "q", &qe, 5, &filters, &overrides, &toml)
            .expect_err("out-of-range penalty must error");
        assert!(
            err.to_string()
                .contains("mmr_same_doc_penalty out of range"),
            "expected mmr_same_doc_penalty out-of-range error, got: {err}"
        );
    }

    #[test]
    fn test_run_search_pipeline_rejects_nan_lambda() {
        // NaN is treated identically to out-of-range (the (0.0..=1.0).contains
        // predicate returns false for NaN). Belt-and-suspenders: the MCP
        // boundary also rejects, but the helper must reject for CLI/eval.
        let db = crate::db::Database::open_in_memory().expect("in-memory db");
        let overrides = crate::config::SearchOverrides {
            mmr: Some(true),
            mmr_lambda: Some(f32::NAN),
            mmr_same_doc_penalty: None,
            parent_retriever: None,
        };
        let toml = crate::config::SearchConfig::default();
        let filters = crate::db::SearchFilters::default();
        let qe = vec![0.0_f32; 384];
        let err = run_search_pipeline(&db, None, "q", &qe, 5, &filters, &overrides, &toml)
            .expect_err("NaN lambda must error");
        assert!(
            err.to_string().contains("mmr_lambda out of range"),
            "expected NaN lambda to be reported as out-of-range, got: {err}"
        );
    }

    /// codex P2 on PR #73 (F2) regression: fresh DB (chunk 0 件, `kb-mcp
    /// index` 未実行のまま `serve` 起動) かつ `[contextual] enabled = true`
    /// だと、watcher 経由の `reindex_single_file` が
    /// `db.read_context_mode()?.unwrap_or(ContextMode::Off)` で常に `None`
    /// を引いて silent に `Off` へ fallback していた (grandfather 判定は
    /// 「レコード不在 = legacy DB」前提だが、fresh DB では誤り)。
    ///
    /// `run_server` は DB open 直後・watcher 起動前に
    /// `indexer::resolve_context_mode(&db, context_mode_desired, false)` を
    /// 一度呼ぶよう修正済み (このテストが検証する呼び出しパターンそのもの)。
    /// ここでは `run_server` を丸ごと起動せず (Embedder / listener 不要)、
    /// その呼び出しパターンを直接再現して DB に記録される値と、
    /// `reindex_single_file` が使うのと同じ fallback 式の結果を検証する。
    #[test]
    fn test_fresh_db_resolve_before_watcher_records_desired_mode() {
        let db = crate::db::Database::open_in_memory().expect("in-memory db");
        db.verify_embedding_meta("bge-small-en-v1.5", 384).unwrap();
        assert_eq!(db.chunk_count().unwrap(), 0, "precondition: fresh DB");
        assert_eq!(
            db.read_context_mode().unwrap(),
            None,
            "precondition: no context_mode recorded yet"
        );

        // run_server が watcher 起動前に行うのと同じ呼び出し。
        crate::indexer::resolve_context_mode(&db, crate::db::ContextMode::Static, false)
            .expect("resolve_context_mode");

        assert_eq!(
            db.read_context_mode().unwrap(),
            Some(crate::db::ContextMode::Static),
            "fresh DB must adopt the desired mode once run_server resolves it up front"
        );

        // reindex_single_file (indexer.rs) が使うのと同じ fallback 式。fix 前は
        // ここで None.unwrap_or(Off) が発火し、watcher が silent に Off 化していた。
        let mode_seen_by_watcher = db
            .read_context_mode()
            .unwrap()
            .unwrap_or(crate::db::ContextMode::Off);
        assert_eq!(
            mode_seen_by_watcher,
            crate::db::ContextMode::Static,
            "watcher's fallback read must now see Static, not silently fall back to Off"
        );
    }

    #[test]
    fn test_run_search_pipeline_honors_toml_fusion_rrf_k() {
        // D-6: [search.fusion] が db 層まで届いていることの配線テスト。
        //
        // **rrf_k で検証する理由**: RRF スコアは必ず 1/(k + rank + 1) の形なので、
        // k を 60 -> 5 に振れば順位に依らず top1 の score が桁で動く。一方
        // bm25 重みの A/B で順位交代を assert する形にすると、対称な候補集合では
        // 1/61 + 1/62 と 1/62 + 1/61 が IEEE754 的に厳密一致してしまい、
        // 順位が入れ替わらず assert_ne! が決定的に fail する。
        //
        // **bm25 側の配線を重複検証しない理由**: `fusion` は run_search_pipeline 内の
        // 単一ローカル変数として 3 つの db 呼び出しすべてへ渡るので、rrf_k が
        // 届いていれば bm25 重みも同じ経路で届いている。bm25 重みが実際に順位を
        // 動かすことは db.rs の test_fts_bm25_weights_are_bound_and_effective が
        // 担保する。
        let db = crate::db::Database::open_in_memory().expect("open_in_memory");
        db.verify_embedding_meta("bge-small-en-v1.5", 384)
            .expect("verify_embedding_meta");
        let emb = |v: f32| vec![v; 384];

        let doc = db
            .upsert_document("a.md", Some("A"), None, None, None, &[], None, "ha")
            .unwrap();
        db.insert_chunk(
            doc,
            0,
            Some("zebrafish"),
            None,
            "zebrafish larvae are used in screening assays",
            None,
            &emb(0.2),
            1.0,
        )
        .unwrap();

        let mk = |k: f32| crate::config::SearchConfig {
            fusion: crate::config::FusionConfig {
                rrf_k: k,
                ..crate::config::FusionConfig::default()
            },
            ..crate::config::SearchConfig::default()
        };
        let overrides = crate::config::SearchOverrides::default();
        let filters = crate::db::SearchFilters::default();
        let go = |cfg: &crate::config::SearchConfig| {
            run_search_pipeline(
                &db,
                None,
                "zebrafish",
                &emb(0.2),
                5,
                &filters,
                &overrides,
                cfg,
            )
            .unwrap()
        };

        let k60 = go(&mk(60.0));
        let k5 = go(&mk(5.0));

        assert!(!k60.is_empty(), "fixture must return at least one hit");
        assert_eq!(k60[0].0, k5[0].0, "the same chunk should top both runs");
        // vec / FTS 双方で rank 0 なら 2/(60+1)=0.0328 vs 2/(5+1)=0.3333。
        // vec-only でも 0.0164 vs 0.1667 で、いずれにせよ差は 1e-3 を大きく超える。
        assert!(
            (k60[0].1.score - k5[0].1.score).abs() > 1e-3,
            "rrf_k from [search.fusion] must reach the db layer: k=60 score {} vs k=5 score {}",
            k60[0].1.score,
            k5[0].1.score
        );
    }

    /// Regression (full-audit 2026-07-26 AU-01, Critical): `limit` は
    /// `Vec::with_capacity(limit as usize)` (`db.rs`) まで生で流れるため、
    /// 上限が無いと MCP の `{"query":"a","limit":4294967295}` 1 発で
    /// allocation abort (= panic ではなく catch 不能な即死) を起こせる。
    /// 実機再現: `memory allocation of 927712935720 bytes failed`。
    /// tune 側は `MAX_TUNE_K` で同じ罠を塞いである (feature-47 codex P2 round 4)。
    #[test]
    fn test_clamp_search_limit_bounds_absurd_values() {
        assert_eq!(clamp_search_limit(u32::MAX), SEARCH_LIMIT_MAX);
        assert_eq!(clamp_search_limit(SEARCH_LIMIT_MAX + 1), SEARCH_LIMIT_MAX);
        // 上限ちょうどと通常値は素通し。
        assert_eq!(clamp_search_limit(SEARCH_LIMIT_MAX), SEARCH_LIMIT_MAX);
        assert_eq!(clamp_search_limit(5), 5);
        assert_eq!(clamp_search_limit(0), 0);
    }

    /// 巨大 limit を実際に pipeline へ通しても abort せず、結果件数が
    /// 上限に収まること (helper だけでなく経路自体を踏む — codex P2 round 4 で
    /// 「helper しか触っていない」と指摘されたのと同じ穴を作らないため)。
    #[test]
    fn test_run_search_pipeline_survives_absurd_limit() {
        let db = crate::db::Database::open_in_memory().expect("in-memory db");
        db.verify_embedding_meta("bge-small-en-v1.5", 384)
            .expect("verify_embedding_meta");
        let emb = |v: f32| vec![v; 384];
        let doc = db
            .upsert_document("a.md", Some("A"), None, None, None, &[], None, "ha")
            .unwrap();
        db.insert_chunk(
            doc,
            0,
            Some("zebrafish"),
            None,
            "zebrafish",
            None,
            &emb(0.2),
            1.0,
        )
        .unwrap();

        let overrides = crate::config::SearchOverrides::default();
        let toml = crate::config::SearchConfig::default();
        let filters = crate::db::SearchFilters::default();
        // clamp 済みの値ではなく **生の u32::MAX** を渡す。境界の clamp
        // (`clamp_search_limit`) が外れても db 層の `VEC_KNN_MAX_K` が
        // 効くこと = 多層防御が成立していることまで含めて固定する。
        let hits = run_search_pipeline(
            &db,
            None,
            "zebrafish",
            &emb(0.2),
            u32::MAX,
            &filters,
            &overrides,
            &toml,
        )
        .expect("pipeline must not abort on an absurd limit");
        assert!(hits.len() <= SEARCH_LIMIT_MAX as usize);
    }

    /// Regression (codex P1 on PR #81): reranker on + MMR off のとき
    /// `compute_reranker_input_limit` は `limit` をそのまま返し、それが
    /// `rerank_candidates_with_ids` の `Vec::with_capacity` に届く。
    /// clamp を各呼び出し境界ではなく `run_search_pipeline` 内に置くことで、
    /// この分岐に生の値が入らないことを型ではなく値で固定する。
    /// (実 reranker はモデル DL が要るため、helper の値だけを直接検証する)
    #[test]
    fn test_reranker_input_limit_is_bounded_for_clamped_limit() {
        let clamped = clamp_search_limit(u32::MAX);
        // MMR off = `limit` 素通し経路。clamp 済みなので上限以下でなければならない。
        assert_eq!(
            compute_reranker_input_limit(false, 50, clamped),
            SEARCH_LIMIT_MAX
        );
        // MMR on 側は pool_size 由来 (feature-28 P1 fix) なので元から有界。
        assert_eq!(compute_reranker_input_limit(true, 50, clamped), 50);
    }

    /// Every tool parameter schema kb-mcp advertises must stay inside the
    /// conservative subset described in `crate::schema_compat`: no `null` unions
    /// and no Rust-width `format` values. Runtimes that compile the schema into a
    /// decoding grammar break on both, and the model then emits its raw tool-call
    /// template as text that never reaches the server (issue #75).
    ///
    /// This asserts on the schema `rmcp` actually serves, so a parameter struct
    /// added later without `#[schemars(transform = ...)]` fails here.
    #[test]
    fn test_advertised_tool_schemas_avoid_client_hostile_constructs() {
        fn assert_clean(tool: &str, node: &serde_json::Value, path: &str) {
            match node {
                serde_json::Value::Object(map) => {
                    if let Some(serde_json::Value::Array(types)) = map.get("type") {
                        assert!(
                            !types.iter().any(|t| t.as_str() == Some("null")),
                            "{tool}: {path}/type advertises the null union {types:?} -- optionality belongs in `required`, not in the type array"
                        );
                    }
                    if let Some(serde_json::Value::String(format)) = map.get("format") {
                        assert!(
                            !crate::schema_compat::NONSTANDARD_FORMATS.contains(&format.as_str()),
                            "{tool}: {path}/format is the non-standard value {format:?} -- numeric bounds belong in `minimum` / `maximum`"
                        );
                    }
                    for (key, value) in map {
                        assert_clean(tool, value, &format!("{path}/{key}"));
                    }
                }
                serde_json::Value::Array(items) => {
                    for (i, value) in items.iter().enumerate() {
                        assert_clean(tool, value, &format!("{path}/{i}"));
                    }
                }
                _ => {}
            }
        }

        use rmcp::handler::server::common::schema_for_type;
        let schemas = [
            ("search", schema_for_type::<SearchParams>()),
            ("get_document", schema_for_type::<GetDocumentParams>()),
            (
                "get_best_practice",
                schema_for_type::<GetBestPracticeParams>(),
            ),
            ("rebuild_index", schema_for_type::<RebuildIndexParams>()),
            (
                "get_connection_graph",
                schema_for_type::<GetConnectionGraphParams>(),
            ),
        ];
        for (tool, schema) in schemas {
            let value = serde_json::Value::Object((*schema).clone());
            assert_clean(tool, &value, "");
        }
    }

    /// (BU-33) The advertised schema is the only description an LLM client
    /// ever reads, so a remedy that is wrong here is wrong where it matters
    /// most — and it is the surface easiest to forget, because fixing the same
    /// claim in the response, the README and the changelog leaves it untouched
    /// (which is exactly what happened in review).
    ///
    /// `max_seed_chunks` bounds the **read**, so `centroid` averages the same
    /// capped prefix. Advertising it as "folds the whole document" tells an
    /// agent to make a call that cannot do what the schema promises.
    #[test]
    fn the_graph_schema_does_not_promise_centroid_covers_the_whole_document() {
        use rmcp::handler::server::common::schema_for_type;
        let schema = schema_for_type::<GetConnectionGraphParams>();
        let value = serde_json::Value::Object((*schema).clone());
        let desc = value["properties"]["max_seed_chunks"]["description"]
            .as_str()
            .expect("max_seed_chunks must carry a description");

        assert!(
            !desc.contains("whole document"),
            "the cap is on the read; centroid cannot cover the whole document: {desc}"
        );
        assert!(
            desc.contains("does not recover"),
            "the schema must state centroid's limitation, not just omit the claim: {desc}"
        );

        // The node budget's own description must stay accurate about what it
        // bounds — it caps the query count as well as the response size.
        let nodes_desc = value["properties"]["max_nodes"]["description"]
            .as_str()
            .expect("max_nodes must carry a description");
        assert!(
            nodes_desc.contains("KNN"),
            "max_nodes bounds the query count too, and a caller cannot infer that: {nodes_desc}"
        );

        // The seed cap makes "every chunk of the start document" false for
        // both strategies. Four separate surfaces carried that claim and each
        // was corrected one review round after the last, so the whole schema
        // is swept rather than the one property that was wrong most recently.
        let all: Vec<String> = value["properties"]
            .as_object()
            .expect("properties")
            .iter()
            .filter_map(|(k, v)| {
                v["description"]
                    .as_str()
                    .map(|d| format!("{k}: {}", d.replace('\n', " ")))
            })
            .collect();
        for d in &all {
            let lower = d.to_lowercase();
            assert!(
                !lower.contains("every chunk") && !lower.contains("all chunks of"),
                "the seed cap means no strategy sees every chunk: {d}"
            );
        }
        assert!(
            all.iter()
                .any(|d| d.starts_with("seed_strategy") && d.contains("max_seed_chunks")),
            "seed_strategy must say the cap applies to it: {all:?}"
        );
    }

    // -----------------------------------------------------------------------
    // BU-06: tool bodies must not run on the async worker threads
    // -----------------------------------------------------------------------

    /// Time how long a second task waits to be polled while a first task is
    /// busy for `BUSY` — on a runtime with exactly **one** worker thread.
    ///
    /// One worker makes the result a fact about scheduling rather than about
    /// timing luck: if the busy task owns the worker, the second task cannot
    /// be polled at all until it lets go.
    #[cfg(test)]
    fn latency_behind_busy_task(offload: bool) -> std::time::Duration {
        const BUSY: std::time::Duration = std::time::Duration::from_millis(400);
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async move {
            let started = std::time::Instant::now();
            let busy = tokio::spawn(async move {
                if offload {
                    run_blocking("test", move || {
                        std::thread::sleep(BUSY);
                        String::new()
                    })
                    .await
                } else {
                    // The shape every tool handler had before BU-06.
                    std::thread::sleep(BUSY);
                    String::new()
                }
            });
            let second = tokio::spawn(async move { started.elapsed() });
            let (_, waited) = tokio::join!(busy, second);
            waited.unwrap()
        })
    }

    /// A tool body running through [`run_blocking`] must leave the async
    /// workers free to serve `/healthz`, `/api/admin/status` and every other
    /// request.
    ///
    /// The inline arm is asserted too, and that is the point: it is the shape
    /// the handlers had before BU-06, and it shows this test can fail. Without
    /// it the offloaded assertion would also pass on a runtime with spare
    /// workers, i.e. for the wrong reason.
    #[test]
    fn run_blocking_leaves_the_async_workers_free() {
        let inline = latency_behind_busy_task(false);
        let offloaded = latency_behind_busy_task(true);

        assert!(
            inline >= std::time::Duration::from_millis(300),
            "control arm did not reproduce the stall: a task queued behind an \
             inline 400ms body was polled after only {inline:?}. Either the \
             runtime got a second worker or the measurement is broken — the \
             offloaded assertion below would then prove nothing."
        );
        assert!(
            offloaded < std::time::Duration::from_millis(100),
            "run_blocking did not free the worker: a task queued behind an \
             offloaded 400ms body waited {offloaded:?} (inline arm: {inline:?}). \
             Tool bodies are starving the async runtime again (BU-06)."
        );
    }

    /// Every `#[tool]` handler must delegate to [`run_blocking`] and do no
    /// work of its own.
    ///
    /// `run_blocking_leaves_the_async_workers_free` proves the mechanism; this
    /// proves the mechanism is actually *used*, including by handlers added
    /// after BU-06. A handler that locks `db` / `embedder` inline compiles and
    /// passes every behavioural test in this file, because none of them run on
    /// a saturated runtime.
    #[test]
    fn tool_handlers_do_not_block_the_runtime() {
        // Normalise line endings first: a checkout with `core.autocrlf=true`
        // would otherwise break every `\n`-anchored marker below and the
        // extraction would silently yield an empty block.
        let src = include_str!("server.rs").replace("\r\n", "\n");

        const MARKER: &str = "#[tool_router]\nimpl KbServer {";
        let start = src
            .find(MARKER)
            .expect("the `#[tool_router] impl KbServer` block moved or was renamed");
        let rest = &src[start + MARKER.len()..];
        // The impl block ends at the first `}` in column 0; everything nested
        // inside it is indented.
        let end = rest
            .find("\n}\n")
            .expect("could not find the end of the tool-surface impl block");
        let block = &rest[..end];

        let handlers: Vec<&str> = block.split("#[tool(").skip(1).collect();
        assert_eq!(
            handlers.len(),
            6,
            "expected 6 `#[tool]` handlers in the tool-surface block, found {}. \
             If a tool was added or removed on purpose, update this count; if it \
             is 0 the block extraction above broke and this test is vacuous.",
            handlers.len()
        );

        for handler in &handlers {
            let name = handler
                .split_once("name = \"")
                .and_then(|(_, r)| r.split_once('"'))
                .map(|(n, _)| n)
                .unwrap_or("<unnamed>");
            assert!(
                handler.contains("run_blocking("),
                "tool `{name}` does not delegate to run_blocking(). Its body \
                 runs on a tokio worker thread, which starves the runtime for \
                 every other request (BU-06). Move the work into a \
                 `*_blocking` method on KbCore."
            );
            assert!(
                !handler.contains(".lock()"),
                "tool `{name}` takes a mutex on the async worker thread (BU-06). \
                 Lock inside the `*_blocking` body on KbCore instead."
            );
        }

        // Anti-vacuity: the work really did move, rather than the scan looking
        // at a block that no longer contains anything.
        let core_start = src
            .find("\nimpl KbCore {")
            .expect("the `impl KbCore` block moved or was renamed");
        let core_block = &src[core_start..];
        let core_end = core_block[1..]
            .find("\n}\n")
            .expect("could not find the end of the KbCore impl block");
        assert!(
            core_block[..core_end].matches(".lock()").count() >= 4,
            "the blocking bodies no longer take any locks — either they moved \
             somewhere this test cannot see, or the extraction is broken."
        );
    }

    /// "There is no such resource" and "this server is broken" are different
    /// answers, and a client acts on them differently: the first is final, the
    /// second is worth retrying. `read_resource` collapsed both into
    /// `resource_not_found`, which also made it disagree with `list_resources`
    /// about the identical unreadable index (codex P2 round 3 on PR #162).
    #[test]
    fn a_broken_server_and_a_missing_resource_do_not_share_a_code() {
        let missing = resource_error(LoadFailure::NotServed, "no such topic group".to_string());
        let broken = resource_error(
            LoadFailure::Internal,
            "failed to list documents".to_string(),
        );

        assert_eq!(missing.code, rmcp::model::ErrorCode::RESOURCE_NOT_FOUND);
        assert_eq!(broken.code, rmcp::model::ErrorCode::INTERNAL_ERROR);
        assert_ne!(
            missing.code, broken.code,
            "a failure of the server must not be reported as an absent resource"
        );
        assert_eq!(
            missing.message, "no such topic group",
            "the message must survive the mapping"
        );
    }

    /// `all_document_paths` is the raw index, and the raw index is not what this
    /// server can serve: narrowing `[parsers].enabled` without reindexing keeps
    /// those rows on purpose (AU-06) while `load_document_blocking` refuses
    /// their extension. Everything the resource surface advertises therefore has
    /// to come through `servable_document_paths`. A second, direct call is
    /// exactly how `resources/list` came to offer a `kb://doc/...` that
    /// `resources/read` then rejected.
    #[test]
    fn the_resource_surface_reads_the_index_through_the_registry_filter() {
        let src = include_str!("server.rs").replace("\r\n", "\n");

        let core_start = src
            .find("\nimpl KbCore {")
            .expect("the `impl KbCore` block moved or was renamed");
        let core_block = &src[core_start..];
        let core_end = core_block[1..]
            .find("\n}\n")
            .expect("could not find the end of the KbCore impl block");
        let core = &core_block[..core_end];

        // Anti-vacuity: this really is the block that holds both the filter and
        // the bodies that must not go around it.
        for needed in [
            "fn servable_document_paths",
            "fn topic_groups_blocking",
            "fn read_resource_blocking",
        ] {
            assert!(
                core.contains(needed),
                "`{needed}` is not in the block this test scanned — the \
                 extraction broke and the assertion below is vacuous."
            );
        }

        assert_eq!(
            core.matches("all_document_paths()").count(),
            1,
            "the raw index query must appear exactly once inside `impl KbCore`, \
             in `servable_document_paths`. Any other call site skips the parser \
             registry filter and advertises a URI a read will refuse."
        );
    }
}
