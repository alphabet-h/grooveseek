use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use rmcp::handler::server::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::schemars;
use rmcp::{tool, tool_router};
use serde::{Deserialize, Serialize};

use crate::db::{Database, SearchHit};
use crate::embedder::{Embedder, ModelChoice, Reranker, RerankerChoice};
use crate::graph::{self, GraphOptions, SeedStrategy};
use crate::parser::Registry;
use crate::{indexer, markdown};

// ---------------------------------------------------------------------------
// Server struct
// ---------------------------------------------------------------------------

pub struct KbServer {
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
    #[allow(dead_code)]
    tool_router: ToolRouter<Self>,
}

// ---------------------------------------------------------------------------
// Tool parameter types
// ---------------------------------------------------------------------------

#[derive(Deserialize, schemars::JsonSchema, Default)]
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
struct GetDocumentParams {
    /// Relative path to the document within knowledge-base/ (e.g. "deep-dive/mcp/overview.md")
    path: String,
}

#[derive(Deserialize, schemars::JsonSchema, Default)]
struct GetBestPracticeParams {
    /// Target name (e.g. "claude-code")
    target: String,
    /// Optional: extract only this h2 section (case-insensitive match)
    category: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema, Default)]
struct RebuildIndexParams {
    /// Force full re-index ignoring existing hashes
    force: Option<bool>,
}

#[derive(Deserialize, schemars::JsonSchema, Default)]
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
    /// Seed strategy: "all_chunks" (default, expand from every chunk of
    /// the start doc) or "centroid" (average the start doc's embeddings).
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

#[derive(Serialize)]
struct DocumentResponse {
    path: String,
    title: Option<String>,
    date: Option<String>,
    topic: Option<String>,
    tags: Vec<String>,
    content: String,
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
    results: Vec<crate::db::SearchHit>,
    low_confidence: bool,
    /// 入力 filter のうち non-default のものだけ正規化後の値で echo back。
    filter_applied: SearchFilterEcho,
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

#[tool_router(server_handler)]
impl KbServer {
    #[tool(
        name = "search",
        description = "Hybrid search (vector + FTS5 full-text, merged via Reciprocal Rank Fusion) over the knowledge base. Returns a wrapper with results, low_confidence flag, and filter_applied echo. The `score` field is the RRF score (or cross-encoder score when reranker is enabled). `match_spans` field (when present) gives byte offsets into `content` for ASCII query terms."
    )]
    async fn search(&self, Parameters(params): Parameters<SearchParams>) -> String {
        let limit = params.limit.unwrap_or(5);

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
            let mut embedder = self.embedder.lock().unwrap();
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

        let mut reranker_guard = self.reranker.lock().unwrap();
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

        let db = self.db.lock().unwrap();
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
            results: hits,
            low_confidence,
            filter_applied: echo,
        };
        serde_json::to_string_pretty(&resp).unwrap_or_default()
    }

    #[tool(
        name = "list_topics",
        description = "List all indexed topics and categories with document counts."
    )]
    async fn list_topics(&self) -> String {
        let db = self.db.lock().unwrap();
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

    #[tool(
        name = "get_document",
        description = "Get the full content and metadata of a document by its relative path within knowledge-base/."
    )]
    async fn get_document(&self, Parameters(params): Parameters<GetDocumentParams>) -> String {
        let canonical = match validate_get_document_path(
            &self.kb_path,
            &params.path,
            &self.parser_registry,
            GET_DOCUMENT_MAX_BYTES,
        ) {
            ValidatePathOutcome::Found(p) => p,
            ValidatePathOutcome::NotFound(e) => {
                return serde_json::to_string_pretty(&e).unwrap_or_default();
            }
            ValidatePathOutcome::Denied(e) => {
                return serde_json::to_string_pretty(&e).unwrap_or_default();
            }
        };
        let ext = canonical.extension().and_then(|e| e.to_str()).unwrap_or("");
        match std::fs::read_to_string(&canonical) {
            Ok(raw) => {
                let resp = build_document_response(&self.parser_registry, &params.path, ext, raw);
                serde_json::to_string_pretty(&resp).unwrap_or_default()
            }
            Err(e) => serde_json::to_string_pretty(&ErrorResponse {
                error: format!("Failed to read file: {e}"),
            })
            .unwrap_or_default(),
        }
    }

    #[tool(
        name = "get_best_practice",
        description = "Get a best-practices document for the given target, optionally extracting a specific h2 section by category name. Opt-in: requires `[best_practice].path_templates` to be configured in kb-mcp.toml (e.g. `path_templates = [\"best-practices/{target}/PERFECT.md\"]`); returns a 'not configured' error otherwise."
    )]
    async fn get_best_practice(
        &self,
        Parameters(params): Parameters<GetBestPracticeParams>,
    ) -> String {
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
                return serde_json::to_string_pretty(&ErrorResponse {
                    error: format!(
                        "Best-practices document for target '{}' not found. Tried: [{}]",
                        params.target,
                        tried.join(", ")
                    ),
                })
                .unwrap_or_default();
            }
            ResolveOutcome::Denied(err) => {
                return serde_json::to_string_pretty(&err).unwrap_or_default();
            }
        };

        match std::fs::read_to_string(&canonical) {
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

    #[tool(
        name = "rebuild_index",
        description = "Rebuild the search index by scanning all source files in the knowledge base (Markdown plus any other extensions enabled via `[parsers].enabled` in kb-mcp.toml)."
    )]
    async fn rebuild_index(&self, Parameters(params): Parameters<RebuildIndexParams>) -> String {
        let force = params.force.unwrap_or(false);

        // Lock order: embedder first, then db (consistent with search)
        let mut embedder = self.embedder.lock().unwrap();
        let db = self.db.lock().unwrap();

        match indexer::rebuild_index(
            &db,
            &mut embedder,
            &self.kb_path,
            force,
            self.exclude_headings.as_deref(),
            &self.exclude_dirs,
            &self.parser_registry,
            indexer::progress::ProgressReporter::new(indexer::progress::ProgressMode::Quiet),
        ) {
            Ok(result) => {
                let stats = IndexStats {
                    total_documents: result.total_documents,
                    updated: result.updated,
                    renamed: result.renamed,
                    deleted: result.deleted,
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
        };

        let db = self.db.lock().unwrap();
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

    let resolved = overrides.resolve(toml_search);
    let use_rerank = reranker.is_some();

    // 1. RRF candidate pool. MMR on → unbounded (MMR が候補プール全件から
    //    多様化選抜、user の `limit` を反映して overfetch を計算)、reranker
    //    on → overfetch (`limit*5.max(50)`)、どちらも off → 最小コストで
    //    `limit` 件 (invariant #3 の bit-exact path)。
    let mmr_pool_size = limit.saturating_mul(5).max(50);
    let candidates_pool: Vec<(i64, crate::db::SearchResult)> = if resolved.mmr_enabled {
        db.search_hybrid_candidates_unbounded(query, query_embedding, mmr_pool_size, filters)?
    } else if use_rerank {
        db.search_hybrid_candidates(
            query,
            query_embedding,
            limit.saturating_mul(5).max(50),
            filters,
        )?
    } else {
        db.search_hybrid_candidates(query, query_embedding, limit, filters)?
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

/// query を whitespace で分割し、全 term が ASCII の場合のみ chunk 内で
/// case-insensitive な substring 検索を行う。byte offset (UTF-8 char boundary 保証) を返す。
///
/// 戻り値:
/// - `None` — query 全体に non-ASCII を 1 つでも含む / 空 query / content
///   が `MATCH_SPAN_CONTENT_MAX_BYTES` を超える (= 計算しない)
/// - `Some(vec![])` — 計算したが一致なし
/// - `Some(spans)` — 計算済みでマッチあり (start byte 順にソート + 重複除去、
///   `MATCH_SPAN_MAX_COUNT` 件で打ち切り)
///
/// `pub` (lib crate API) で CLI (`src/main.rs`) / benches からも再利用できるようにしておく。
pub fn compute_match_spans(query: &str, content: &str) -> Option<Vec<crate::db::MatchSpan>> {
    let trimmed = query.trim();
    if trimmed.is_empty() {
        return None;
    }
    let terms: Vec<&str> = trimmed.split_whitespace().collect();
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

    let content_lower = content.to_ascii_lowercase();
    let mut spans: Vec<crate::db::MatchSpan> = Vec::new();
    'outer: for term in &terms {
        let term_lower = term.to_ascii_lowercase();
        if term_lower.is_empty() {
            continue;
        }
        for (start, _) in content_lower.match_indices(&term_lower) {
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
            // F-35: span 数の上限。dedup 前にカウントする (小さい cap=100 に
            // 対して dedup 後でも 100 を保つには push 段階で抑制で十分、
            // dedup によって減ることはあっても増えない)。
            if spans.len() >= MATCH_SPAN_MAX_COUNT {
                break 'outer;
            }
        }
    }
    spans.sort_by_key(|s| (s.start, s.end));
    spans.dedup_by(|a, b| a.start == b.start && a.end == b.end);
    Some(spans)
}

/// `get_document` ツール用に、拡張子に対応する Parser で
/// frontmatter (title/date/topic/tags) を抽出し DocumentResponse を組む。
/// 純粋関数化してテスト可能にしている。
/// `get_document` の最大バイト数。1 MiB を超える文書は read_to_string
/// 一括読みでのメモリ膨張・レスポンス過大を避けるため拒否する。
pub(crate) const GET_DOCUMENT_MAX_BYTES: u64 = 1024 * 1024;

/// `search` MCP tool が受理する query 文字列の最大バイト数 (1 KiB)。
/// 上限超えは ErrorResponse で reject する。embedder / FTS5 layer は内部で
/// truncate するが、上流で reject した方がレスポンスが予測可能になり、
/// `compute_match_spans` の O(N×M) を query 側からも抑制できる。F-35。
pub(crate) const SEARCH_QUERY_MAX_BYTES: usize = 1024;

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

/// `get_document` のパス検証 + size cap。成功時は canonical な絶対パスを返す。
/// 拒否時は `ErrorResponse` を返し、呼び出し側が JSON 化する。
///
/// 防御の順序:
/// 1. **symlink reject** — `canonicalize` の前に拾う必要がある
/// 2. **canonicalize + starts_with(kb_path)** — `..` 抜け道を defeat
/// 3. **extension membership** — indexer と同じ拡張子セットに限定
///    (`.git/config` や excluded_dirs 配下の bypass を遮断)
/// 4. **size cap** — RAM-OOM を防ぐ
pub(crate) fn validate_get_document_path(
    kb_path: &std::path::Path,
    rel_path: &str,
    registry: &Registry,
    max_bytes: u64,
) -> ValidatePathOutcome {
    let file_path = kb_path.join(rel_path);

    // 1. Symlink reject (canonicalize の前に判定)
    match std::fs::symlink_metadata(&file_path) {
        Ok(meta) if meta.file_type().is_symlink() => {
            return ValidatePathOutcome::Denied(ErrorResponse {
                error: "Access denied: symlinks are not allowed.".to_string(),
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

    // 4. Size cap
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

///
/// 登録されていない拡張子はフォールバックで Markdown parser を使う (pre-
/// feature-20 と同じ挙動)。`.txt` はファイル名から title を derive するため
/// `path_hint` を必ず渡す。
fn build_document_response(
    registry: &Registry,
    path_hint: &str,
    ext: &str,
    raw: String,
) -> DocumentResponse {
    let parsed = match registry.by_extension(ext) {
        Some(p) => p.parse(&raw, path_hint, &[]),
        None => markdown::parse(&raw),
    };
    DocumentResponse {
        path: path_hint.to_string(),
        title: parsed.frontmatter.title,
        date: parsed.frontmatter.date,
        topic: parsed.frontmatter.topic,
        tags: parsed.frontmatter.tags,
        content: raw,
    }
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
        match validate_get_document_path(kb_path, &rel, registry, max_bytes) {
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
}

impl KbServer {
    /// Shared state から新しい `KbServer` を組み立てる。
    /// Arc::clone で軽量、embedder / reranker モデルの重複ロードは起きない。
    pub fn from_shared(shared: &KbServerShared) -> Self {
        Self {
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
            tool_router: KbServer::tool_router(),
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
) -> Result<()> {
    let db_path = crate::resolve_db_path(kb_path);
    let db = Database::open(&db_path.to_string_lossy())?;

    // モデル DL の前に meta 整合性を確認。不整合ならここで止めて DL を回避。
    db.verify_embedding_meta(model.model_id(), model.dimension() as u32)?;
    let embedder = Embedder::with_model(model)?;
    let reranker = Reranker::try_new(reranker_choice)?;

    let kb_path = kb_path
        .canonicalize()
        .unwrap_or_else(|_| kb_path.to_path_buf());

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
    };

    // watcher をバックグラウンドで並走。
    let watcher_state = crate::watcher::WatcherState {
        kb_path: kb_path.clone(),
        db: Arc::clone(&shared.db),
        embedder: Arc::clone(&shared.embedder),
        registry: Arc::clone(&shared.parser_registry),
        exclude_headings: shared.exclude_headings.clone(),
        exclude_dirs: shared.exclude_dirs.clone(),
        config: watch_config,
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
        } => {
            // move shared to http runner (no clone needed — stdio branch
            // consumes it only by reference and is mutually exclusive).
            crate::transport::http::run_http(addr, allowed_hosts, healthz_public, shared).await
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
            let pid = std::process::id();
            let nonce = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            let path = std::env::temp_dir().join(format!("kb-mcp-srvtest-{prefix}-{pid}-{nonce}"));
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
            "kb-mcp-srvtest-outside-{}-{}.md",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
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
            "kb-mcp-srvtest-bp-outside-{}-{}.md",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
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
        let resp = build_document_response(&reg, "notes/hello.md", "md", md.to_string());
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
            raw.to_string(),
        );
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
        let resp = build_document_response(&reg, "a.unknown", "unknown", raw.to_string());
        // markdown::parse が frontmatter を拾う
        assert_eq!(resp.title.as_deref(), Some("x"));
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
        let r = validate_get_document_path(&kb.path, "docs/a.md", &md_only_registry(), 1024 * 1024);
        assert!(
            matches!(r, ValidatePathOutcome::Found(_)),
            "normal .md should pass: {r:?}"
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
        let err = match validate_get_document_path(&kb.path, "big.md", &md_only_registry(), 1024) {
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
            "kb-mcp-srvtest-outside-gd-{}-{}.md",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        fs::write(&outside, "secret").unwrap();
        let rel = format!("../{}", outside.file_name().unwrap().to_string_lossy());
        let err = match validate_get_document_path(&kb.path, &rel, &md_only_registry(), 1024 * 1024)
        {
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
        let err =
            match validate_get_document_path(&kb.path, "link.md", &md_only_registry(), 1024 * 1024)
            {
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
}
