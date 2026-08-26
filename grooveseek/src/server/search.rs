//! The search half of the MCP server: the tool body, the pipeline it runs, and
//! the limits the request is held to before either of them sees it.
//!
//! Split out of `server.rs` in audit L-1 (PR-1), the same way `db.rs` was split
//! in AU-25. The bodies are byte-identical to their previous form and appear in
//! the order they appeared there.
//!
//! **Two things did change, both of them visibility.** `search_blocking` is
//! called by `impl KbServer` in the parent, and `compute_reranker_input_limit`
//! and `merge_disjoint_spans` are named by the parent's `mod tests`; all three
//! were private, so each is now `pub(super)` — the smallest widening that keeps
//! the call sites working. `db.rs` needed none of this because the methods it
//! moved were already `pub` or `pub(crate)`: an inherent method's visibility is
//! independent of which module its `impl` block sits in only when it had some
//! to begin with.
//!
//! What stayed in `server.rs`: everything the tool surface is made of — the
//! `#[tool_router]` and `#[tool_handler]` impls, the parameter and response
//! types, and `mod tests`, which is not split for the reason `db.rs` gives —
//! splitting it would mean editing tests to prove a refactor changed nothing.

// The parent module is what this file was carved out of, so it keeps seeing
// exactly what it saw before. A hand-written list would be a second thing to
// maintain and, on a move this size, a place to silently drop a name.
use super::*;

impl KbCore {
    pub(super) fn search_blocking(&self, params: SearchParams) -> String {
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

        // F-4: `-term` の解析。**1 リクエストにつきここで 1 回**行い、以降は
        // この `ParsedQuery` を持ち回る。除外しか書かれていないクエリは探す
        // ものが無いので、1 KiB 超と同じ経路で断る — モデルにも DB にも触る
        // 前に。断り文句は [`crate::db::ParsedQuery::require_positive`] が
        // 持っており、CLI と golden の load も同じ 1 文を読む。
        let parsed = crate::db::parse_query(&params.query);
        if let Err(e) = parsed.require_positive() {
            return serde_json::to_string_pretty(&ErrorResponse {
                error: e.to_string(),
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
            match embedder.embed_single(parsed.positive_text()) {
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
        let use_rerank = should_rerank(
            params.rerank,
            Some(self.rerank_by_default),
            reranker_guard.is_some(),
        );

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
            &parsed,
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
            h.match_spans = compute_match_spans(parsed.positive_text(), &h.content);
        }

        // The empty-list rule lives in `new`, shared with the command line.
        let echo = SearchFilterEcho::new(
            params.category.clone(),
            params.topic.clone(),
            params.path_globs.clone(),
            params.tags_any.clone(),
            params.tags_all.clone(),
            params.date_from.clone(),
            params.date_to.clone(),
            params.min_confidence_ratio,
            parsed.exclude().to_vec(),
        );

        // The `uri` on a hit and the URIs `resources/list` offers have to be the
        // same set, so both go through `ServableRules` rather than each testing
        // the registry on its own. The lock is still held from the search above.
        let rules = match db.documents_larger_than(GET_DOCUMENT_MAX_BYTES) {
            Ok(oversized) => ServableRules::new(&self.parser_registry, oversized),
            Err(e) => {
                // A hit without a `uri` is a hit a client cannot follow, which
                // is a smaller loss than failing the search outright — and a
                // smaller one than a link a read refuses.
                tracing::warn!("could not read document sizes; search hits lose their uri: {e}");
                ServableRules::sizes_unavailable(&self.parser_registry)
            }
        };

        let resp = SearchResponse {
            results: hits
                .into_iter()
                .map(|h| HitWithUri::new(h, &rules))
                .collect(),
            low_confidence,
            filter_applied: echo,
        };
        serde_json::to_string_pretty(&resp).unwrap_or_default()
    }
}

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
pub(super) fn compute_reranker_input_limit(mmr_enabled: bool, pool_size: usize, limit: u32) -> u32 {
    if mmr_enabled {
        u32::try_from(pool_size).unwrap_or(u32::MAX)
    } else {
        limit
    }
}

/// Shared MMR-aware search pipeline. Used by:
/// - MCP `SearchTool::search` (server.rs)
/// - CLI `groove search` (main.rs)
/// - CLI `groove eval` (eval.rs)
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
/// The query arrives already parsed, as a [`crate::db::ParsedQuery`] rather
/// than a `&str`. That is the whole reason the type exists: a `-term` group
/// means one thing to the database (rows to drop) and another to the reranker
/// (text that is not part of what the caller asked for), and a `&str` cannot
/// tell a caller which of the two it is holding. Each of the three callers
/// parses once, at the point it can still refuse the query, and hands the
/// result down.
///
/// Range validation for `mmr_lambda` / `mmr_same_doc_penalty` is performed
/// here so that all 3 callers reject `1.5` / `-0.1` / `NaN` consistently.
/// Caller-side early reject (e.g. for a richer error response shape) is OK
/// — this is belt-and-suspenders.
#[allow(clippy::too_many_arguments)] // 8 cohesive inputs; struct-of-args adds noise without grouping
pub fn run_search_pipeline(
    db: &Database,
    reranker: Option<&mut Reranker>,
    query: &crate::db::ParsedQuery<'_>,
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
    //
    //    **The database gets the raw query; everything else gets the positive
    //    text.** Both legs need the exclusions — dropping the rows that match
    //    them is the whole job — so the db layer keeps its `&str` API and
    //    parses for itself in `search_split_candidates`. The reranker below is
    //    scoring a hit against what the caller asked for, and a term they ruled
    //    out is not that.
    let mmr_pool_size = limit.saturating_mul(5).max(50);
    let candidates_pool: Vec<(i64, crate::db::SearchResult)> = if resolved.mmr_enabled {
        db.search_hybrid_candidates_unbounded(
            query.raw(),
            query_embedding,
            mmr_pool_size,
            filters,
            fusion,
        )?
    } else if use_rerank {
        db.search_hybrid_candidates(
            query.raw(),
            query_embedding,
            limit.saturating_mul(5).max(50),
            filters,
            fusion,
        )?
    } else {
        db.search_hybrid_candidates(query.raw(), query_embedding, limit, filters, fusion)?
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
        Some(r) => r.rerank_candidates_with_ids(
            query.positive_text(),
            candidates_pool,
            reranker_input_limit,
        )?,
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
/// What `rerank_by_default` means when nobody sets it.
///
/// Named rather than spelled `true` at each site: `serve` resolves the standing
/// value, [`should_rerank`] falls back to it, and the documentation states it.
/// Three copies of a bare literal are three chances to disagree.
pub const RERANK_BY_DEFAULT: bool = true;

/// Whether one search reranks — decided here for both surfaces.
///
/// The two surfaces express the per-call override differently: over MCP it is
/// the `rerank` parameter, and on the command line it is naming a model with
/// `--reranker`, which says "for this query" the way every other CLI argument
/// does. What they must not do is disagree about what an override *means*, or
/// about what happens when there is none — which is exactly what had happened:
/// the CLI reranked whenever a reranker was configured and never read
/// `rerank_by_default` at all, so one `groove.toml` produced two answers.
///
/// So each caller converts its own spelling into `per_call`, and the rest of
/// the decision is this expression.
///
/// - `per_call` — this query's override, or `None` to take the standing value.
/// - `standing` — the `rerank_by_default` key, or `None` for
///   [`RERANK_BY_DEFAULT`].
/// - `reranker_available` — whether a reranker exists to run at all. Last,
///   because no override can conjure one.
pub fn should_rerank(
    per_call: Option<bool>,
    standing: Option<bool>,
    reranker_available: bool,
) -> bool {
    per_call.unwrap_or(standing.unwrap_or(RERANK_BY_DEFAULT)) && reranker_available
}

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
pub(super) fn merge_disjoint_spans(
    mut spans: Vec<crate::db::MatchSpan>,
) -> Vec<crate::db::MatchSpan> {
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
/// `limit` ではなく `groove search` の繰り返しか MMR pool 側で扱う。
pub const SEARCH_LIMIT_MAX: u32 = 1000;

/// `limit` を [`SEARCH_LIMIT_MAX`] に丸める。エラーにせず clamp するのは、
/// 「多めに要求すると落ちる」より「多めに要求すると上限で返る」方が
/// MCP client (LLM) にとって回復しやすいため。
pub fn clamp_search_limit(limit: u32) -> u32 {
    limit.min(SEARCH_LIMIT_MAX)
}
