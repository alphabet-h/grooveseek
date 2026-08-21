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

// (audit L-1) The search half lives in `server/search.rs`. Named imports rather
// than a glob: this list is what the parent and its `mod tests` still reach for
// by name, and a glob would hide the day one of them stops being reachable.
//
// `pub` / `pub(crate)` mirrors each item's own visibility, because these are the
// paths callers outside this module already use -- `crate::server::X` and
// `grooveseek::server::X` -- and a move is not the place to make them change.
mod documents;
mod kb_uri;

// (audit L-1 PR-3) `ServableRules` is the name production code outside this
// module reaches by path: `doctor.rs` builds one to report what the corpus
// would actually serve. The rest are named by this module, its siblings
// through their own `use super::*`, and its tests.
pub(crate) use kb_uri::ServableRules;
use kb_uri::{HitWithUri, LoadFailure, resource_error};
// `resource_text` warned as unused in the plain library build: its production
// caller moved with it, and what remains is `mod tests`.
#[cfg(test)]
use kb_uri::resource_text;
mod search;

// (audit L-1 PR-2) `GET_DOCUMENT_MAX_BYTES` is the one name here that
// production code outside this module reaches by path -- `doctor.rs` reports
// the limit in three of its checks -- so it keeps `pub(crate)`. The rest are
// named by this module and its tests only.
pub(crate) use documents::GET_DOCUMENT_MAX_BYTES;
use documents::{EXTRACTED_TEXT_MAX_BYTES, max_bytes_for};

// Named only by `mod tests`, and again the compiler is what said so: left
// unconditional, every name below warned as unused in the plain library build.
// Their production callers moved with them, which is the point -- these eight
// are the surface the tests hold the moved code to, not a surface the parent
// still uses.
#[cfg(test)]
use documents::{
    ResolveOutcome, ValidatePathOutcome, best_practice_not_found_message, build_document_response,
    path_probe_failed, resolve_best_practice_path, truncate_on_char_boundary,
    validate_get_document_path,
};

pub use search::{
    RERANK_BY_DEFAULT, SEARCH_LIMIT_MAX, clamp_search_limit, compile_path_globs,
    compute_low_confidence, compute_match_spans, run_search_pipeline, should_rerank,
    validate_filter_list,
};

// The rest are reached only from tests, and the compiler is what said so: left
// unconditional, every name below warned as unused in the plain library build
// and stopped warning under `cfg(test)`. `db.rs` does the same for
// `parse_dim_from_create_sql`, for the same reason — no production code in this
// module names them any more, and pretending otherwise hides the day one is
// dropped entirely.
//
// `pub(crate)` on the first line because `transport::http`'s own tests reach
// two of them by path (`crate::server::FILTER_LIST_MAX_ITEMS`).
// `MATCH_SPAN_MAX_TERMS` is deliberately absent: it survives only inside doc
// comments and assertion messages, so re-importing it would be a name kept
// alive by prose.
#[cfg(test)]
pub(crate) use search::{FILTER_ITEM_MAX_BYTES, FILTER_LIST_MAX_ITEMS, SEARCH_QUERY_MAX_BYTES};
#[cfg(test)]
use search::{
    MATCH_SPAN_CONTENT_MAX_BYTES, MATCH_SPAN_MAX_COUNT, compute_reranker_input_limit,
    merge_disjoint_spans,
};

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
    /// ものを読む。groove.toml 未指定時は legacy 既定
    /// `["best-practices/{target}/PERFECT.md"]`。
    best_practice_templates: Vec<String>,
    /// Parser registry: index 対象の拡張子レジストリ。`rebuild_index` MCP ツール
    /// から `indexer::rebuild_index` に渡す。`groove.toml` の
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
    /// omitted, the server default (from `groove.toml` / CLI) is used.
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
    /// `null` falls back to the server default (`groove.toml` / CLI);
    /// `0.0` disables the cutoff for this query.
    min_confidence_ratio: Option<f32>,

    // ----- MMR / Parent retriever (per-call overrides) -----
    /// (v0.7.0+) Enable MMR diversity re-rank. When `null`, falls back to
    /// `[search.mmr].enabled` from groove.toml. Setting `true` / `false`
    /// per call overrides the toml default for that call.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    mmr: Option<bool>,

    /// (v0.7.0+) MMR lambda (relevance vs. diversity tradeoff). Must be in
    /// `[0.0, 1.0]`; values outside that range are rejected. `1.0` is
    /// equivalent to MMR off; lower values lean toward exploration. When
    /// `null`, falls back to `[search.mmr].lambda` from groove.toml.
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
    /// Named `start` rather than `path` because it is the seed of a walk, not
    /// the document being fetched — `groove graph --start` calls it the same
    /// thing, and `get_document.path` keeps `path` for the fetch.
    start: String,
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

/// Read `get_connection_graph`'s `seed_strategy` value.
///
/// The accepted spellings come from [`SeedStrategy::SPELLINGS`], which the
/// command line's parser reads too — a value that one surface takes and the
/// other rejects fails the call rather than costing a lookup, so there is one
/// table and not two lists to keep in step.
fn parse_seed_strategy(value: Option<&str>) -> Result<SeedStrategy, String> {
    let Some(text) = value else {
        return Ok(SeedStrategy::default());
    };
    SeedStrategy::parse(text).ok_or_else(|| {
        format!(
            "unknown seed_strategy '{text}' (expected {})",
            SeedStrategy::accepted_spellings()
        )
    })
}

/// The parameter names a client actually sees for `tool`, read out of the very
/// schema the server advertises. `None` for a name that is not a tool.
///
/// This exists for the binary's `cli_and_mcp_names_stay_paired` test. The CLI
/// half of that comparison lives in `main.rs`, which cannot see the structs
/// above, and a list copied by hand would keep passing while the two surfaces
/// drifted apart — which is the one failure the test is there to catch.
pub fn advertised_param_names(tool: &str) -> Option<Vec<String>> {
    use rmcp::handler::server::common::schema_for_type;
    let schema = match tool {
        "search" => schema_for_type::<SearchParams>(),
        "get_document" => schema_for_type::<GetDocumentParams>(),
        "get_best_practice" => schema_for_type::<GetBestPracticeParams>(),
        "rebuild_index" => schema_for_type::<RebuildIndexParams>(),
        "get_connection_graph" => schema_for_type::<GetConnectionGraphParams>(),
        _ => return None,
    };
    let value = serde_json::Value::Object((*schema).clone());
    let mut names: Vec<String> = value
        .get("properties")?
        .as_object()?
        .keys()
        .cloned()
        .collect();
    names.sort();
    Some(names)
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

/// The wrapper both search surfaces answer with (feature-26).
///
/// Generic over the hit type because the two surfaces differ in exactly one
/// place: an MCP hit carries a `uri` and a command-line hit does not
/// ([`HitWithUri`] versus [`crate::db::SearchHit`]). Everything else about the
/// shape — the key names, which fields are omitted, how the echo is built — is
/// one implementation rather than two, so the surfaces cannot come to disagree
/// about the response that `docs/stability.md` freezes for both of them.
///
/// It used to be two: this type served `/mcp`, and `main.rs` assembled its own
/// object with `serde_json::json!`. A field renamed on one side would have left
/// the other unchanged and every test passing.
#[derive(Serialize)]
pub struct SearchResponse<H> {
    pub results: Vec<H>,
    pub low_confidence: bool,
    /// 入力 filter のうち non-default のものだけ正規化後の値で echo back。
    pub filter_applied: SearchFilterEcho,
}

/// 入力 filter のうち non-default のものだけ echo。`null`/空配列の項目は
/// `skip_serializing_if` で JSON から省略される。
#[derive(Serialize, Default)]
pub struct SearchFilterEcho {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub topic: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path_globs: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tags_any: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tags_all: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub date_from: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub date_to: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min_confidence_ratio: Option<f32>,
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

    /// The slot is claimed by the caller and handed over, not taken here.
    ///
    /// It has to be, because this body does not run when the request arrives.
    /// `run_blocking` gives the closure to Tokio's blocking pool, and a
    /// saturated pool queues it — a check made in here would run whenever the
    /// closure was finally scheduled, which can be *after* the rebuild it should
    /// have been refused for finished. The second rebuild would then be accepted
    /// and the outage doubled, which is the thing the slot exists to prevent
    /// (codex P2 on PR #187).
    ///
    /// Holding the slot for the length of this call is also what
    /// `/api/admin/status` reads to report indexing.active=true (codex P2 rounds
    /// 1 and 4 on PR #57).
    fn rebuild_index_blocking(&self, params: RebuildIndexParams, _slot: RebuildSlot) -> String {
        let force = params.force.unwrap_or(false);

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
        let seed_strategy = match parse_seed_strategy(params.seed_strategy.as_deref()) {
            Ok(s) => s,
            Err(error) => {
                return serde_json::to_string_pretty(&ErrorResponse { error }).unwrap_or_default();
            }
        };

        // (BU-05) `exclude_paths` is bounded inside `build_connection_graph`
        // rather than here, so `groove graph --exclude-paths` gets the same limit.

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
        match graph::build_connection_graph(&db, &params.start, &opts) {
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
        description = "Get a best-practices document for the given target, optionally extracting a specific h2 section by category name. Opt-in: requires `[best_practice].path_templates` to be configured in groove.toml (e.g. `path_templates = [\"best-practices/{target}/PERFECT.md\"]`); returns a 'not configured' error otherwise."
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
        description = "Rebuild the search index by scanning all source files in the knowledge base (Markdown plus any other extensions enabled via `[parsers].enabled` in groove.toml)."
    )]
    async fn rebuild_index(&self, Parameters(params): Parameters<RebuildIndexParams>) -> String {
        // Claimed here, in the handler, rather than inside the closure below —
        // see `rebuild_index_blocking`. The refusal has to be decided when the
        // request arrives, not whenever the blocking pool gets round to it.
        //
        // This does not violate BU-06, which says a handler must not touch
        // `db` / `embedder` / `reranker`: `indexing_state` is a mutex over a
        // two-field struct, held for the length of one comparison.
        let slot = match claim_rebuild_slot(&self.core.indexing_state) {
            Ok(slot) => slot,
            Err(started_at) => return rebuild_already_running(started_at),
        };
        let core = Arc::clone(&self.core);
        run_blocking("rebuild_index", move || {
            core.rebuild_index_blocking(params, slot)
        })
        .await
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
             spreadsheet comes back as the text groove extracted from it, not as the \
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
    /// the knowledge base, it trusts groove's own database. A resource is
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
        // A read is a complete result too, so it owes the same hints the two
        // listings give. rmcp's constructor leaves them `None`; a hand-written
        // handler has to set them, and this one is hand-written.
        Ok(with_cache_hints(rmcp::model::ReadResourceResult::new(vec![content])).into())
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
    rmcp::model::ReadResourceResult,
);

fn internal_error(e: String) -> rmcp::ErrorData {
    rmcp::ErrorData::internal_error(e, None)
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

/// The one rebuild in progress, or `None`.
///
/// This used to carry a refcount, because two HTTP clients could both reach
/// `rebuild_index` before the first finished and the first caller's guard would
/// otherwise clear a slot the second still occupied (codex P2 round 4 on
/// PR #57). Counting was the right answer while both were allowed to run; now
/// only one is, so a count that can never exceed 1 would only describe a
/// situation the code refuses to create. See [`claim_rebuild_slot`].
#[derive(Debug)]
pub struct IndexingState {
    pub started_at: std::time::SystemTime,
    pub progress: Option<IndexingProgress>,
}

/// An RAII claim on the single rebuild slot. Dropping it frees the slot.
pub struct RebuildSlot(Arc<Mutex<Option<IndexingState>>>);

impl Drop for RebuildSlot {
    fn drop(&mut self) {
        // (BU-18) Recover from a poisoned lock rather than skipping. Skipping
        // is much the worse failure here: the slot would stay occupied for the
        // rest of the process's life, `/api/admin/status` would report an index
        // in progress forever, and every later rebuild would be refused by a
        // rebuild that is not running. The payload is plain data, so there is
        // nothing to repair before using it.
        *recover(self.0.lock(), "indexing_state") = None;
    }
}

/// What a caller is told when the slot is already taken.
///
/// The elapsed time is the useful part: "wait" without a number gives no way to
/// tell a rebuild that is nearly done from one that just started.
fn rebuild_already_running(started_at: std::time::SystemTime) -> String {
    let secs = started_at.elapsed().map(|d| d.as_secs()).unwrap_or(0);
    serde_json::to_string_pretty(&ErrorResponse {
        error: format!(
            "A rebuild is already running (started {secs}s ago). Wait for it to \
             finish and call again: each rebuild re-embeds the whole corpus, and \
             search is unavailable while one runs."
        ),
    })
    .unwrap_or_default()
}

/// Take the rebuild slot, or report when the rebuild already holding it began.
///
/// `rebuild_index` re-embeds the whole corpus while holding the embedder and
/// the database, so a second call does not run alongside the first — it queues
/// behind it, and search is unavailable for the sum of the two. Nothing bounded
/// how many could queue: `mcp_session_gate` lets every non-`initialize` request
/// past without taking a seat, so `max_sessions` does not apply, and
/// `spawn_blocking` cannot be aborted, so closing the connection does not stop
/// one either. A few dozen bytes of request bought a full re-vectorisation of
/// the corpus, as many times over as the caller liked.
///
/// Refusing the second caller is the whole fix, and it is worth noting what it
/// is not: this bounds the MCP tool. `groove index` runs in its own process and
/// calls the indexer directly, so a CLI rebuild still overlaps a served one.
///
/// The check and the claim share one critical section deliberately. Two callers
/// that both read the slot before either wrote to it would both find it empty.
fn claim_rebuild_slot(
    slot: &Arc<Mutex<Option<IndexingState>>>,
) -> Result<RebuildSlot, std::time::SystemTime> {
    let mut guard = recover(slot.lock(), "indexing_state");
    if let Some(running) = guard.as_ref() {
        return Err(running.started_at);
    }
    *guard = Some(IndexingState {
        started_at: std::time::SystemTime::now(),
        progress: None,
    });
    drop(guard);
    Ok(RebuildSlot(Arc::clone(slot)))
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
    // (L-8) No `path`. It used to carry the knowledge base's absolute path,
    // which on Windows is `C:\Users\<name>\...` — the operator's OS account
    // name, in a JSON body and, because `/ui` printed it in the status band, in
    // every screenshot of that page.
    //
    // Nothing needed it. The tray reads `daemon.pid` and `indexing.active` and
    // nothing else (`crates/groove-tray/src/state.rs`), `docs/clients.md`
    // already described the band without it, and `/ui` only displayed it. What
    // identifies a knowledge base to the person looking at it is below:
    // document and chunk counts, and the model.
    //
    // ADR-0008 puts this surface outside the 1.0 freeze, which is what makes
    // removing the field a thing to do now rather than a thing to regret later.
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
            documents,
            chunks,
            model,
        })
    }
}

/// (Phase 4 PR-2) Test-only. `/api/search` was removed from the running
/// server: `/ui` searches through `/mcp` now, and this entry only ever exposed
/// `query` and `limit` — 2 of the 17 parameters the `search` tool takes. It is
/// kept because `tests/runtime_starvation.rs` needs a handler that blocks on
/// the embedder lock, and that test must not be edited.
///
/// Plain-JSON search entry for the former WebUI `/api/search` POST.
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
#[cfg(any(test, feature = "test-helpers"))]
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
            // (codex P1 round 7 on PR #173) Shared alias set, not a copy — a
            // test stub that disagrees with production about which local names
            // count is a test that passes for the wrong reason.
            allowed_admin_hosts: crate::transport::http::DEFAULT_LOOPBACK_HOSTS
                .iter()
                .map(|h| (*h).to_string())
                .collect(),
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
    // 残る。`groove index` なら prune されるが `serve` は index しないので、
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
                 rejects them. Run `groove index` to remove them from the index.",
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
        // (codex P1 round 7 on PR #173) The same alias set `/healthz` Host
        // validation and the `/mcp` Origin defaults use. Round 6 replaced the
        // Origin copy and left this one, which is the worse half-state: an alias
        // added to the shared set would be accepted by `/mcp` and refused by
        // `/ui` — the two surfaces disagreeing about one browser.
        let mut hosts: Vec<String> = crate::transport::http::DEFAULT_LOOPBACK_HOSTS
            .iter()
            .map(|h| (*h).to_string())
            .collect();
        // codex P1 round 4 on PR #57: only include the bind addr when it is
        // a loopback IP. Otherwise a non-loopback bind (e.g. 192.168.1.10:3100
        // or 0.0.0.0:3100) would let LAN browsers reach /ui + /api/admin/status
        // via the bind addr Host header — that contradicts the spec § 7
        // "admin is loopback-only" decision and the install-time Note that
        // promises LAN browsers see 403.
        // (codex P2 round 4 on PR #173) Shared predicate. With
        // `IpAddr::is_loopback` here, a `[::ffff:127.0.0.1]` bind — which the
        // CLI now accepts as loopback — was left out of the allow-list, so the
        // operator's own Host got 403 from the admin routes.
        if let crate::transport::Transport::Http { addr, .. } = &transport
            && crate::transport::http::is_loopback_peer(addr.ip())
        {
            // (codex P2 round 5 on PR #173) Every spelling a client might use
            // for this address, not just the one Rust prints. `Ipv6Addr`'s
            // Display renders an IPv4-mapped address in the dotted form
            // (`::ffff:127.0.0.1`) while the WHATWG URL serializer browsers use
            // emits hex pieces (`::ffff:7f00:1`). `NormalizedAuthority` only
            // strips brackets and lowercases, so it cannot bridge the two, and
            // whichever one we omitted would 403.
            for host in crate::transport::http::client_host_forms(addr.ip()) {
                // Both `host:port` and the bare host: the allow-list is matched
                // with the port stripped, and the fallback path in
                // `NormalizedAuthority::from_allowed_entry` accepts an
                // unbracketed IPv6 entry too.
                let bare = host.trim_matches(['[', ']']).to_string();
                for entry in [format!("{host}:{}", addr.port()), bare] {
                    if !hosts.contains(&entry) {
                        hosts.push(entry);
                    }
                }
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
        // (feature-49) `.grooveignore` を起動時に 1 度読む。以後、ファイル自体が
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
            allowed_origins,
            healthz_public,
            max_sessions,
        } => {
            // move shared to http runner (no clone needed — stdio branch
            // consumes it only by reference and is mutually exclusive).
            crate::transport::http::run_http(
                addr,
                allowed_hosts,
                allowed_origins,
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
    use std::collections::BTreeSet;
    use std::fs;

    /// The half of the naming rule that values are held to: a caller who
    /// copies a spelling from the command line into a tool call must not get
    /// an error, because unlike a name, a value that differs fails the call.
    ///
    /// Driven by `SeedStrategy::SPELLINGS`, so a spelling added for one
    /// surface is required of this one too. The command-line half is
    /// `main.rs::naming_surface::every_accepted_seed_strategy_spelling_parses_on_the_command_line`.
    #[test]
    fn every_accepted_seed_strategy_spelling_parses_in_the_tool() {
        for spelling in SeedStrategy::SPELLINGS {
            assert_eq!(
                parse_seed_strategy(Some(spelling.text)),
                Ok(spelling.value),
                "a seed_strategy of {:?} must be understood by the tool",
                spelling.text
            );
        }
        assert_eq!(parse_seed_strategy(None), Ok(SeedStrategy::default()));

        // The error names everything that is accepted, not the subset one
        // surface advertises — the caller who got here spelled it wrong and
        // has no way to know which surface's list applies.
        let err = parse_seed_strategy(Some("all chunks")).unwrap_err();
        for spelling in SeedStrategy::SPELLINGS {
            assert!(
                err.contains(spelling.text),
                "the error must list every accepted spelling; {:?} is missing from: {err}",
                spelling.text
            );
        }
    }

    /// 一意な tempdir を作って kb_path として返す。Drop で削除。
    struct TempKb {
        path: PathBuf,
    }
    impl TempKb {
        fn new(prefix: &str) -> Self {
            let path = crate::test_support::unique_temp_path(&format!("groove-srvtest-{prefix}"));
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
            "groove-srvtest-outside-{}.md",
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
            "groove-srvtest-bp-outside-{}.md",
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
            (
                "groove-svc svc-server",
                "see groove-svc-server here",
                &[(4, 21)],
            ),
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
    /// most 100" promise in docs/usage.md and docs/citations.md. That is a real
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
    // claim_rebuild_slot: one rebuild at a time
    // -----------------------------------------------------------------------

    /// The slot is what makes `rebuild_index` single-flight, and the reason it
    /// needs to be is not throughput: a rebuild holds the embedder and the
    /// database for its whole duration, so a second one does not run beside the
    /// first, it queues behind it with search unavailable throughout. Nothing
    /// else bounds how many can queue — the session gate lets every
    /// non-`initialize` call past without a seat.
    #[test]
    fn the_second_caller_is_refused_and_told_when_the_first_began() {
        let slot: Arc<Mutex<Option<IndexingState>>> = Arc::new(Mutex::new(None));

        let first = claim_rebuild_slot(&slot).expect("the slot starts free");
        let started = match claim_rebuild_slot(&slot) {
            Ok(_) => panic!("a second rebuild took the slot while the first held it"),
            Err(started_at) => started_at,
        };

        // The refusal carries a time so the caller can say how long to wait.
        assert!(
            started.elapsed().is_ok(),
            "the refusal must report when the running rebuild started"
        );
        drop(first);
    }

    #[test]
    fn releasing_the_slot_lets_the_next_rebuild_in() {
        let slot: Arc<Mutex<Option<IndexingState>>> = Arc::new(Mutex::new(None));

        drop(claim_rebuild_slot(&slot).expect("the slot starts free"));

        claim_rebuild_slot(&slot).expect("a finished rebuild must not lock the slot shut");
    }

    /// The refusal is a tool result, so it has to be the error envelope every
    /// other failure uses — a client that can read one can read this.
    #[test]
    fn the_refusal_is_the_same_error_envelope_as_every_other_failure() {
        let started = std::time::SystemTime::now() - std::time::Duration::from_secs(42);
        let body = rebuild_already_running(started);

        let parsed: serde_json::Value =
            serde_json::from_str(&body).expect("the refusal must be JSON a client can read");
        let message = parsed["error"]
            .as_str()
            .expect("the refusal must use the `error` envelope");
        assert!(
            message.contains("42s ago"),
            "the refusal must say how long the running rebuild has been going, \
             or the caller cannot tell one that is nearly done from one that \
             just started: {message}"
        );
    }

    /// Holding the slot is also what `/api/admin/status` reads to say an index
    /// is in progress, so the two cannot drift: there is one piece of state.
    #[test]
    fn the_slot_is_occupied_exactly_while_a_rebuild_holds_it() {
        let slot: Arc<Mutex<Option<IndexingState>>> = Arc::new(Mutex::new(None));
        assert!(slot.lock().expect("fresh mutex").is_none());

        let held = claim_rebuild_slot(&slot).expect("the slot starts free");
        assert!(
            slot.lock().expect("fresh mutex").is_some(),
            "status would report no indexing while a rebuild runs"
        );

        drop(held);
        assert!(
            slot.lock().expect("fresh mutex").is_none(),
            "status would report indexing forever after one finished"
        );
    }

    // -----------------------------------------------------------------------
    // should_rerank: the one decision both search surfaces make
    // -----------------------------------------------------------------------

    /// The whole truth table, because the two surfaces reach this from
    /// different spellings and the point of the function is that they land in
    /// the same place. `groove search` translates "`--reranker` was named" into
    /// `Some(true)`; the tool passes its `rerank` parameter straight through.
    #[test]
    fn a_per_call_override_beats_the_standing_default_either_way() {
        assert!(should_rerank(Some(true), Some(false), true));
        assert!(!should_rerank(Some(false), Some(true), true));
    }

    #[test]
    fn without_an_override_the_standing_default_decides_and_absent_means_on() {
        assert!(should_rerank(None, Some(true), true));
        assert!(!should_rerank(None, Some(false), true));
        // No standing value either: RERANK_BY_DEFAULT decides, and it is on.
        assert!(should_rerank(None, None, true));
    }

    #[test]
    fn nothing_reranks_when_no_reranker_was_loaded() {
        for per_call in [None, Some(true), Some(false)] {
            for standing in [None, Some(true), Some(false)] {
                assert!(
                    !should_rerank(per_call, standing, false),
                    "no override may conjure a reranker: {per_call:?} / {standing:?}"
                );
            }
        }
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
            "groove-srvtest-outside-gd-{}.md",
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

    /// codex P2 on PR #73 (F2) regression: fresh DB (chunk 0 件, `groove
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
            .upsert_document("a.md", Some("A"), None, None, None, &[], None, "ha", 0)
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
            .upsert_document("a.md", Some("A"), None, None, None, &[], None, "ha", 0)
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

    /// Every tool parameter schema groove advertises must stay inside the
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

    /// "Absent" and "I could not look" are different answers, and only the
    /// first is about the path. A permission error reported as `NotFound` sends
    /// the caller hunting for a typo that does not exist — and, through
    /// `into_result`, reaches the client as `RESOURCE_NOT_FOUND` rather than an
    /// internal error (codex P2 round 6 on PR #162).
    ///
    /// All three of `validate_get_document_path`'s I/O probes go through this,
    /// so fixing one and leaving the others is not possible by accident.
    #[test]
    fn only_a_missing_path_reads_as_missing() {
        use std::io::{Error, ErrorKind};

        for absent in [ErrorKind::NotFound, ErrorKind::NotADirectory] {
            assert!(
                !path_probe_failed(&Error::from(absent)),
                "{absent:?} says the path cannot be there, which is about the path"
            );
        }
        for kind in [
            ErrorKind::PermissionDenied,
            ErrorKind::InvalidInput,
            ErrorKind::Other,
        ] {
            assert!(
                path_probe_failed(&Error::from(kind)),
                "{kind:?} says the probe failed, not that the path is absent"
            );
        }

        // And the outcome it produces has to reach the client as the server's
        // failure, not the document's.
        let unavailable = ValidatePathOutcome::Unavailable(ErrorResponse {
            error: "permission denied".to_string(),
        });
        assert!(
            matches!(unavailable.into_result(), Err((LoadFailure::Internal, _))),
            "an unexaminable path is an internal failure"
        );
        for absent in [
            ValidatePathOutcome::NotFound(ErrorResponse {
                error: "gone".to_string(),
            }),
            ValidatePathOutcome::Denied(ErrorResponse {
                error: "refused".to_string(),
            }),
        ] {
            assert!(
                matches!(absent.into_result(), Err((LoadFailure::NotServed, _))),
                "absence and refusal are both statements about the document"
            );
        }
    }

    /// `get_document` reports truncation in a field; a resource read has only
    /// the text, and returning the prefix bare presented it as the whole
    /// document. A client reading a large PDF got its first megabyte with
    /// nothing to say the rest existed (codex P2 round 5 on PR #162).
    #[test]
    fn a_truncated_resource_says_so_in_the_text_it_hands_over() {
        let whole = resource_text("all of it".to_string(), false);
        assert_eq!(whole, "all of it", "an untruncated read must be untouched");

        let part = resource_text("the first megabyte".to_string(), true);
        assert!(
            part.starts_with("the first megabyte"),
            "the text served must still come first: {part}"
        );
        assert!(
            part.contains("Truncated"),
            "the notice must be in the body, the only place a resource read has: {part}"
        );
        // The literal is written with a `\` line continuation, which eats the
        // newline *and* the indentation that follows it — miscount and the
        // sentence runs two words together while still compiling.
        assert!(
            part.contains("1 MiB. What is above"),
            "the continued literal must join with exactly one space: {part}"
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

    // -- feature-51: size is part of what "servable" means -------------------

    fn md_and_pdf_registry() -> Registry {
        Registry::from_enabled(&["md".to_string(), "pdf".to_string()]).expect("md + pdf registry")
    }

    #[test]
    fn a_text_document_past_the_read_cap_is_not_offered() {
        let registry = md_and_pdf_registry();
        let rules = ServableRules::new(
            &registry,
            vec![
                ("big.md".to_string(), GET_DOCUMENT_MAX_BYTES + 1),
                ("exact.md".to_string(), GET_DOCUMENT_MAX_BYTES),
            ],
        );
        assert!(
            !rules.allows("big.md"),
            "a read of this would be refused, so offering it is a broken link"
        );
        // The boundary belongs to the side that is served: `read_checked`
        // refuses what is *over* the cap, so a document exactly at it is fine.
        assert!(rules.allows("exact.md"));
        // Not in the oversized set at all: the common case.
        assert!(rules.allows("small.md"));
    }

    #[test]
    fn a_large_binary_document_is_still_offered_because_a_read_truncates_it() {
        let registry = md_and_pdf_registry();
        // Same byte count as the refused Markdown above. The difference is the
        // cap that applies: a PDF is read under the binary limit and its
        // extracted text is truncated with a notice, never refused.
        let rules = ServableRules::new(
            &registry,
            vec![("big.pdf".to_string(), GET_DOCUMENT_MAX_BYTES + 1)],
        );
        assert!(
            rules.allows("big.pdf"),
            "a binary document over the *text* cap is still readable"
        );
    }

    #[test]
    fn an_unrecorded_size_is_not_read_as_too_large() {
        let registry = md_and_pdf_registry();
        // `documents_larger_than` cannot return a NULL row, so an index written
        // before feature-51 produces an empty list and every document stays on
        // offer. Upgrading must not empty `resources/list`.
        let rules = ServableRules::new(&registry, Vec::new());
        assert!(rules.allows("legacy.md"));
    }

    /// codex P2 round 1: an empty oversized set means "there are none", which
    /// is the opposite of what a failed query knows. Sharing one representation
    /// let a size-read failure hand out every URI, including the ones a read
    /// refuses — the exact defect this feature exists to close.
    #[test]
    fn a_failed_size_lookup_withholds_uris_rather_than_handing_out_all_of_them() {
        let registry = md_and_pdf_registry();
        let unknown = ServableRules::sizes_unavailable(&registry);
        assert!(!unknown.allows("notes/a.md"));
        assert!(!unknown.allows("notes/huge.md"));

        // Same empty vector, but as an answer rather than a failure.
        let known = ServableRules::new(&registry, Vec::new());
        assert!(known.allows("notes/a.md"));
    }

    #[test]
    fn an_extension_the_registry_dropped_is_still_not_offered() {
        // The feature-50 rule has to survive the new one being added next to it.
        let registry = md_and_pdf_registry();
        let rules = ServableRules::new(&registry, Vec::new());
        assert!(!rules.allows("legacy/old.xls"));
        assert!(!rules.allows("no_extension"));
    }

    /// The three surfaces have to answer alike about the same document.
    ///
    /// `resources/list` builds its topic bodies from `servable_document_paths`,
    /// `search` stamps a `uri` per hit, and `resources/read` decides whether to
    /// serve. A document this test puts past the cap must be absent from all
    /// three, and its neighbour just under the cap present in all three.
    #[test]
    fn the_listing_the_search_uri_and_the_read_agree_about_one_document() {
        let registry = md_and_pdf_registry();
        let rows = vec![
            ("notes/huge.md".to_string(), GET_DOCUMENT_MAX_BYTES + 1),
            ("notes/fine.md".to_string(), GET_DOCUMENT_MAX_BYTES),
        ];
        let rules = ServableRules::new(&registry, rows);

        // 1. What a listing would enumerate.
        let all = [
            "notes/huge.md".to_string(),
            "notes/fine.md".to_string(),
            "notes/small.md".to_string(),
        ];
        let listed: Vec<&String> = all.iter().filter(|p| rules.allows(p)).collect();
        assert_eq!(
            listed,
            vec!["notes/fine.md", "notes/small.md"],
            "the listing drops only the document a read would refuse"
        );

        // 2. What `search` stamps on a hit — the same rules value, so the two
        //    cannot drift without this test failing.
        let hit = |path: &str| {
            let mut h = crate::db::SearchHit {
                score: 1.0,
                path: path.to_string(),
                title: None,
                heading: None,
                topic: None,
                date: None,
                tags: Vec::new(),
                content: String::new(),
                match_spans: None,
                expanded_from: None,
            };
            h.content = "x".to_string();
            HitWithUri::new(h, &rules).uri
        };
        assert_eq!(hit("notes/huge.md"), None, "no link a read would reject");
        assert_eq!(
            hit("notes/fine.md"),
            Some("kb://doc/notes/fine.md".to_string())
        );

        // 3. `read_resource_blocking` checks membership against the very list
        //    from step 1, so the refusal follows from the same predicate rather
        //    than from a second copy of the rule.
        assert!(
            !listed.iter().any(|p| *p == "notes/huge.md"),
            "a read is bounded by the listing, so dropping it there is what \
             makes the read refuse"
        );
    }

    // -----------------------------------------------------------------------
    // The search response and the contract that describes it
    // -----------------------------------------------------------------------

    /// The contract lives in `docs/stability.md`, which is published from the
    /// repository root while this crate sits one level down.
    fn repo_root() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("the crate directory always has a parent")
            .to_path_buf()
    }

    /// Every field the response can emit, as the dotted paths the contract
    /// uses.
    ///
    /// Paths rather than bare names because `topic` occurs under both a hit
    /// and `filter_applied` and means something different in each; a flat set
    /// would let one of them satisfy the check for the other.
    fn walk_fields(value: &serde_json::Value, prefix: &str, out: &mut BTreeSet<String>) {
        match value {
            serde_json::Value::Object(map) => {
                for (k, v) in map {
                    let path = if prefix.is_empty() {
                        k.clone()
                    } else {
                        format!("{prefix}.{k}")
                    };
                    out.insert(path.clone());
                    walk_fields(v, &path, out);
                }
            }
            // Every element, not just the first: `expanded_from` is a tagged
            // enum whose two variants carry different keys, so one sample
            // result can only ever show half of the shape.
            serde_json::Value::Array(items) => {
                for item in items {
                    walk_fields(item, &format!("{prefix}[]"), out);
                }
            }
            _ => {}
        }
    }

    /// The two surfaces, serialized.
    ///
    /// The contract freezes the MCP tool *and* `groove search --format json`,
    /// so checking one of them would leave the other free to drift while every
    /// test passed — which is what the command line did for as long as it
    /// assembled its own object (codex P2 on PR #201). They now share
    /// [`SearchResponse`], and this returns both instantiations so the checks
    /// below cover the pair.
    fn both_surfaces() -> (serde_json::Value, serde_json::Value) {
        let mcp = maximal_search_response();
        let cli = serde_json::to_value(SearchResponse {
            results: maximal_hits(),
            low_confidence: true,
            filter_applied: maximal_echo(),
        })
        .expect("the response type serializes");
        (mcp, cli)
    }

    /// Every field either surface can emit, **including the refusal**.
    ///
    /// A `search` answers with one of two shapes: the wrapper, or
    /// `{"error": …}` when the tool refuses or the search fails — nine return
    /// sites in `server/search.rs` alone. Walking only the successful one left
    /// `error` outside the contract while the section claimed to list
    /// everything (codex P2 round 2 on PR #201).
    fn all_emitted_fields() -> BTreeSet<String> {
        let (mcp, cli) = both_surfaces();
        let mut out = BTreeSet::new();
        walk_fields(&mcp, "", &mut out);
        walk_fields(&cli, "", &mut out);
        let refusal = serde_json::to_value(ErrorResponse {
            error: "why the call could not be answered".to_string(),
        })
        .expect("the error type serializes");
        walk_fields(&refusal, "", &mut out);
        out
    }

    /// Hits with every optional field populated, one per `expanded_from`
    /// variant — the two carry different keys, so a sample with one of them
    /// shows half the shape.
    fn maximal_hits() -> Vec<crate::db::SearchHit> {
        let hit = |expanded: crate::db::ExpandedRange| crate::db::SearchHit {
            score: 0.5,
            path: "notes/a.md".to_string(),
            title: Some("A".to_string()),
            heading: Some("H".to_string()),
            topic: Some("t".to_string()),
            date: Some("2026-01-01".to_string()),
            tags: vec!["x".to_string()],
            content: "body".to_string(),
            match_spans: Some(vec![crate::db::MatchSpan { start: 0, end: 1 }]),
            expanded_from: Some(expanded),
        };
        vec![
            hit(crate::db::ExpandedRange::Adjacent {
                from_index: 0,
                to_index: 1,
            }),
            hit(crate::db::ExpandedRange::WholeDocument { total_chunks: 3 }),
        ]
    }

    fn maximal_echo() -> SearchFilterEcho {
        SearchFilterEcho {
            category: Some("c".to_string()),
            topic: Some("t".to_string()),
            path_globs: Some(vec!["**".to_string()]),
            tags_any: Some(vec!["a".to_string()]),
            tags_all: Some(vec!["b".to_string()]),
            date_from: Some("2026-01-01".to_string()),
            date_to: Some("2026-12-31".to_string()),
            min_confidence_ratio: Some(1.5),
        }
    }

    /// A response with **every** optional field populated.
    ///
    /// The point is coverage, not realism: a field that is only ever emitted
    /// under some condition still has to appear in the contract, so the sample
    /// has to be the maximal shape rather than a typical one.
    fn maximal_search_response() -> serde_json::Value {
        // Built through the real constructor so the sample cannot claim a
        // shape the server does not produce. An empty oversized list plus a
        // registry that knows `.md` is what makes `uri` present.
        let registry = md_and_pdf_registry();
        let rules = ServableRules::new(&registry, Vec::new());
        let results: Vec<HitWithUri> = maximal_hits()
            .into_iter()
            .map(|h| {
                let h = HitWithUri::new(h, &rules);
                assert!(
                    h.uri.is_some(),
                    "the sample must carry a uri, or the maximal shape is not maximal"
                );
                h
            })
            .collect();
        let response = SearchResponse {
            results,
            low_confidence: true,
            filter_applied: maximal_echo(),
        };
        serde_json::to_value(&response).expect("the response type serializes")
    }

    /// The field names the contract table claims, from its first column.
    ///
    /// One table is the source: a second machine-readable list beside it would
    /// be a copy, and copies agree only until someone edits one of them.
    fn contract_fields() -> BTreeSet<String> {
        let path = repo_root().join("docs").join("stability.md");
        let text = fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("{} must be readable: {e}", path.display()));
        let section = text
            .split("### What a search answers")
            .nth(1)
            .expect("docs/stability.md must carry the search response contract")
            .split("\n### ")
            .next()
            .expect("splitting always yields a first part");
        section
            .lines()
            .filter_map(|l| l.strip_prefix("| `"))
            .filter_map(|rest| rest.split('`').next())
            .map(str::to_string)
            .collect()
    }

    /// Adding a field to the response without adding it to the contract makes
    /// the promise in `docs/stability.md` cover less than it appears to.
    #[test]
    fn every_field_a_search_can_answer_with_is_in_the_contract() {
        let emitted = all_emitted_fields();
        let documented = contract_fields();
        let missing: Vec<&String> = emitted.difference(&documented).collect();
        assert!(
            missing.is_empty(),
            "the search response emits fields the contract does not list: {missing:?}\n\
             Add them to the `### What a search answers` table in docs/stability.md \
             (and its Japanese counterpart), or the freeze silently excludes them."
        );
    }

    /// The other direction: a field removed from the response but left in the
    /// table turns the contract into a promise about something that no longer
    /// exists. `documented_flags` holds the command line to the same pair of
    /// checks for the same reason.
    #[test]
    fn the_contract_does_not_name_fields_a_search_cannot_answer_with() {
        let emitted = all_emitted_fields();
        let documented = contract_fields();
        let extra: Vec<&String> = documented.difference(&emitted).collect();
        assert!(
            extra.is_empty(),
            "the contract lists fields the search response cannot produce: {extra:?}\n\
             Either the field was removed and the table was not, or the sample in \
             `maximal_search_response` stopped covering it."
        );
    }

    /// The contract says `uri` is the one field the two surfaces differ on.
    /// That is a claim about both of them, so it is checked on both rather
    /// than inferred from the type that carries it.
    #[test]
    fn uri_is_the_only_field_the_two_surfaces_differ_on() {
        let (mcp_value, cli_value) = both_surfaces();
        let mut mcp = BTreeSet::new();
        walk_fields(&mcp_value, "", &mut mcp);
        let mut cli = BTreeSet::new();
        walk_fields(&cli_value, "", &mut cli);

        let only_mcp: Vec<&String> = mcp.difference(&cli).collect();
        assert_eq!(
            only_mcp,
            vec!["results[].uri"],
            "the contract says a uri is what an MCP hit adds, and nothing else"
        );
        let only_cli: Vec<&String> = cli.difference(&mcp).collect();
        assert!(
            only_cli.is_empty(),
            "the command line must not answer with a field the tool cannot: {only_cli:?}"
        );
    }

    /// Both languages have to carry the same set, because a reader of either
    /// one is reading the contract. The English page is what
    /// `contract_fields` parses, so this is what keeps the Japanese page from
    /// drifting away from it unnoticed.
    #[test]
    fn both_languages_state_the_same_contract() {
        let ja_path = repo_root().join("docs").join("stability.ja.md");
        let text = fs::read_to_string(&ja_path)
            .unwrap_or_else(|e| panic!("{} must be readable: {e}", ja_path.display()));
        let section = text
            .split("### search が返すもの")
            .nth(1)
            .expect("docs/stability.ja.md must carry the contract too")
            .split("\n### ")
            .next()
            .expect("splitting always yields a first part");
        let ja: BTreeSet<String> = section
            .lines()
            .filter_map(|l| l.strip_prefix("| `"))
            .filter_map(|rest| rest.split('`').next())
            .map(str::to_string)
            .collect();
        assert_eq!(
            ja,
            contract_fields(),
            "the two contract tables must name the same fields"
        );
    }
}
