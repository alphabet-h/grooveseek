//! Retrieval for [`Database`]: vector KNN, FTS5 candidates, and the RRF fusion
//! that merges them.
//!
//! This is the half of the module whose behaviour is observable as a *number* —
//! `kb-mcp eval` and `kb-mcp tune` measure exactly what these methods return —
//! so it is worth keeping apart from the storage code that feeds it.
//!
//! Split out of `db.rs` in AU-25 (PR-2). The methods are byte-identical to
//! their previous form and keep their existing visibility: an inherent method's
//! `pub` / `pub(crate)` does not depend on which module its `impl` block sits
//! in, so no call site outside this crate's `db` module changed.
//!
//! What stayed behind in `db.rs`, and why: `sanitize_fts_query` (six callers
//! outside this file), `VEC_KNN_MAX_K` (also used by the storage half), and
//! `FTS_CANDIDATE_CALLS` — that one is named by path from `tune.rs`
//! (`crate::db::FTS_CANDIDATE_CALLS`), so moving it would break the AU-22
//! round-trip guard that counts FTS calls during a sweep.

// The parent module is what this file was carved out of, so it keeps seeing
// exactly what it saw before. A hand-written list would be a second thing to
// maintain and, on a move this size, a place to silently drop a name.
use super::*;
/// filter (category / topic) を Rust 側で適用する際の KNN / FTS の over-fetch 倍率。
/// filter が選択的な場合に target `limit` 件に届くよう多めに候補を取る。
const FILTER_OVERFETCH_FACTOR: u32 = 10;
const FILTER_OVERFETCH_CAP: u32 = 10_000;

/// `any_pool` が空なら常に pass (= フィルタ無効)。
fn matches_tags_any(hit_tags: &[String], any_pool: &[String]) -> bool {
    if any_pool.is_empty() {
        return true;
    }
    any_pool.iter().any(|t| hit_tags.contains(t))
}

/// `tags_all` フィルタ: `all_pool` に含まれるすべての tag を hit が持っていれば pass。
/// `all_pool` が空なら常に pass。
fn matches_tags_all(hit_tags: &[String], all_pool: &[String]) -> bool {
    if all_pool.is_empty() {
        return true;
    }
    all_pool.iter().all(|t| hit_tags.contains(t))
}

/// date filter: hit の date 文字列が `[from, to]` の範囲内 (lex 比較)。
/// from / to が両方 None なら常に pass。date 欠損 (None) は strict に reject。
fn matches_date_range(hit_date: Option<&str>, from: Option<&str>, to: Option<&str>) -> bool {
    if from.is_none() && to.is_none() {
        return true;
    }
    let Some(d) = hit_date else {
        return false; // 欠損は strict 排除
    };
    if let Some(f) = from
        && d < f
    {
        return false;
    }
    if let Some(t) = to
        && d > t
    {
        return false;
    }
    true
}

impl Database {
    /// ベクトル単体類似検索。最大 `limit` 件を距離昇順 (小さい = より類似) で返す。
    ///
    /// `search_hybrid` とのロジック統一のため、内部では [`Self::search_vec_candidates`]
    /// に委譲し、`chunk_id` を剥いだ `SearchResult` のみを返す。
    /// 主に単体ベクトル検索のテスト / ツール用途で残している。
    pub fn search_similar(
        &self,
        query_embedding: &[f32],
        limit: u32,
        filters: &SearchFilters<'_>,
    ) -> Result<Vec<SearchResult>> {
        let hits = self.search_vec_candidates(query_embedding, limit, filters)?;
        Ok(hits.into_iter().map(|(_, r)| r).collect())
    }

    /// FTS5 側の候補検索。最大 `limit` 件を bm25 昇順 (小さい = 関連度高) で返す。
    /// 返値は `(chunk_id, SearchResult の雛形)` のタプル列 (`score` は bm25)。
    ///
    /// `search_vec_candidates` と同様に、category / topic フィルタが指定されて
    /// いる場合は `FILTER_OVERFETCH_FACTOR` 倍を取りに行き、Rust 側で絞り込む。
    ///
    /// `fusion` の bm25 列重みが `bm25()` の第 2〜4 引数として渡る (feature-47)。
    ///
    /// **`fusion.rrf_k` はここでは読まない。** `tune::build_metric_table` はその
    /// 前提で、同じ重み組の 6 つの `rrf_k` 条件に対して 1 回しか本関数を呼ばず、
    /// 384 条件を 64 往復で埋める。読むようになれば掃引結果は静かに誤りになる
    /// ので、`tune` 側に不変条件そのものを述べる回帰テストがある (AU-22)。
    pub(crate) fn search_fts_candidates(
        &self,
        query_text: &str,
        limit: u32,
        filters: &SearchFilters<'_>,
        fusion: FusionParams,
    ) -> Result<Vec<(i64, SearchResult)>> {
        #[cfg(test)]
        FTS_CANDIDATE_CALLS.with(|c| c.set(c.get() + 1));
        let Some(fts_query) = sanitize_fts_query(query_text) else {
            return Ok(Vec::new());
        };

        // filter 指定があれば over-fetch する (詳細は SearchFilters::has_any)。
        let fetch_limit = if filters.has_any() {
            limit
                .saturating_mul(FILTER_OVERFETCH_FACTOR)
                .min(FILTER_OVERFETCH_CAP)
        } else {
            limit
        };

        // bm25 に column weight を与え、見出し一致を優遇する。
        // 引数順は FTS5 の CREATE VIRTUAL TABLE の列宣言順 (heading, context, content)。
        //
        // feature-47 D-4: 重みは **番号付き** bind parameter で渡す。匿名 `?` は
        // SELECT と ORDER BY で別々に採番されて既存の `?1`/`?2` と衝突し
        // "statement uses 6, 5 supplied" になるため使ってはならない。
        // NaN / inf は bind 経路を silent に通ってしまうので、値域の防波堤は
        // `Config::validate()` 唯一 (D-2 / E-2)。
        let sql = "
            SELECT c.id, bm25(fts_chunks, ?3, ?4, ?5) AS score,
                   c.content, c.heading, c.quality_score, c.document_id,
                   d.path, d.title, d.topic, d.date, d.category, d.tags, c.context_text
            FROM fts_chunks f
            JOIN chunks c ON c.id = f.rowid
            JOIN documents d ON d.id = c.document_id
            WHERE fts_chunks MATCH ?1
            ORDER BY bm25(fts_chunks, ?3, ?4, ?5)
            LIMIT ?2
            ";
        let mut stmt = self.conn.prepare(sql)?;
        // f64 へ拡幅してから bind する。sqlite3_value_double が受けるのは double
        // であり、f32 → f64 は値を変えない (2.0 / 1.0 / 0.5 / 4.0 いずれも厳密表現)。
        let rows = stmt.query_map(
            params![
                fts_query,
                fetch_limit,
                fusion.bm25_heading_weight as f64,
                fusion.bm25_context_weight as f64,
                fusion.bm25_content_weight as f64
            ],
            |row| {
                let chunk_id: i64 = row.get(0)?;
                let score: f32 = row.get(1)?;
                Ok((
                    chunk_id,
                    score,
                    row.get::<_, String>(2)?,          // content
                    row.get::<_, Option<String>>(3)?,  // heading
                    row.get::<_, f32>(4)?,             // quality_score
                    row.get::<_, i64>(5)?,             // document_id (F-41)
                    row.get::<_, String>(6)?,          // path
                    row.get::<_, Option<String>>(7)?,  // title
                    row.get::<_, Option<String>>(8)?,  // topic
                    row.get::<_, Option<String>>(9)?,  // date
                    row.get::<_, Option<String>>(10)?, // category
                    row.get::<_, Option<String>>(11)?, // tags (JSON)
                    row.get::<_, Option<String>>(12)?, // context_text
                ))
            },
        )?;

        let mut results = Vec::new();
        for row in rows {
            let (
                chunk_id,
                score,
                content,
                heading,
                quality_score,
                doc_id,
                path,
                title,
                r_topic,
                date,
                r_category,
                tags_json,
                context_text,
            ) = row?;
            if filters.min_quality > 0.0 && quality_score < filters.min_quality {
                continue;
            }
            if let Some(cat) = filters.category
                && r_category.as_deref() != Some(cat)
            {
                continue;
            }
            if let Some(t) = filters.topic
                && r_topic.as_deref() != Some(t)
            {
                continue;
            }
            // path_globs filter (Task 3 追加)
            if let Some(cpg) = filters.path_globs
                && !cpg.matches(&path)
            {
                continue;
            }
            // tags_any / tags_all filter (Task 3 追加)
            let hit_tags = self.parse_tags_json_recording(tags_json);
            if !matches_tags_any(&hit_tags, filters.tags_any) {
                continue;
            }
            if !matches_tags_all(&hit_tags, filters.tags_all) {
                continue;
            }
            // date_from / date_to filter (Task 3 追加)
            if !matches_date_range(date.as_deref(), filters.date_from, filters.date_to) {
                continue;
            }
            results.push((
                chunk_id,
                SearchResult {
                    score, // 一旦 bm25 を入れておく (呼び出し側で RRF に上書き)
                    content,
                    heading,
                    document_id: doc_id,
                    path,
                    title,
                    topic: r_topic,
                    date,
                    tags: hit_tags,
                    context_text,
                },
            ));
            if results.len() >= limit as usize {
                break;
            }
        }
        Ok(results)
    }

    /// ベクトル検索 + FTS5 を Reciprocal Rank Fusion で統合するハイブリッド
    /// 検索。各側の順位だけを使うため、距離や bm25 の正規化は不要。
    ///
    /// FTS 側でヒットが 0 件 (trigram 下限以下のクエリや予約語のみ等) の場合は
    /// vec-only の順位で結果を返す (スコアは RRF 公式で計算)。
    ///
    /// `fusion` は RRF の k と bm25 列重み (feature-47)。production 経路は
    /// `[search.fusion]` 由来の値を渡す。テストや単発利用では
    /// `FusionParams::default()` でよい。
    pub fn search_hybrid(
        &self,
        query_text: &str,
        query_embedding: &[f32],
        limit: u32,
        filters: &SearchFilters<'_>,
        fusion: FusionParams,
    ) -> Result<Vec<SearchResult>> {
        let hits =
            self.search_hybrid_candidates(query_text, query_embedding, limit, filters, fusion)?;
        Ok(hits.into_iter().map(|(_, r)| r).collect())
    }

    /// `search_hybrid` と同じ RRF 計算を行うが、呼び出し側で再ランク等に
    /// 使うため `(chunk_id, SearchResult)` のタプル列を返す。
    /// `SearchResult.score` には RRF スコア (大きいほど良い) が入る。
    pub fn search_hybrid_candidates(
        &self,
        query_text: &str,
        query_embedding: &[f32],
        limit: u32,
        filters: &SearchFilters<'_>,
        fusion: FusionParams,
    ) -> Result<Vec<(i64, SearchResult)>> {
        let candidates = limit.saturating_mul(5).max(50);
        let (vec_hits, fts_hits) =
            self.search_split_candidates(query_text, query_embedding, candidates, filters, fusion)?;
        Ok(fuse_rrf(&vec_hits, &fts_hits, fusion.rrf_k, Some(limit)))
    }

    /// `search_hybrid_candidates` と同じ RRF を計算するが、最終 truncate を
    /// せず候補プール全件を返す。MMR の候補プール用。
    ///
    /// 既存 `search_hybrid_candidates` の戻り値型 `Vec<(i64, SearchResult)>`
    /// と互換 (truncate しないだけの違い)。candidates 取得幅は呼び出し側が
    /// `desired_candidates` で指定する (典型: `limit.saturating_mul(5).max(50)`、
    /// = bounded 側の overfetch ロジックと同じ)。
    ///
    /// MMR の場合、`top-k` が大きい (e.g. user が limit=100 を要求した) ケースに
    /// 対応するため、固定 50 ハードキャップは置かない。呼び出し側が pool size を
    /// 決める責務を持つ。
    pub fn search_hybrid_candidates_unbounded(
        &self,
        query_text: &str,
        query_embedding: &[f32],
        desired_candidates: u32,
        filters: &SearchFilters<'_>,
        fusion: FusionParams,
    ) -> Result<Vec<(i64, SearchResult)>> {
        let candidates = desired_candidates.max(50);
        let (vec_hits, fts_hits) =
            self.search_split_candidates(query_text, query_embedding, candidates, filters, fusion)?;
        Ok(fuse_rrf(&vec_hits, &fts_hits, fusion.rrf_k, None))
    }

    /// vec 側と FTS 側の候補リストを **融合前に** 返す (feature-47 D-3)。
    ///
    /// `kb-mcp tune` が「vec 候補は query あたり 1 回・FTS 候補は bm25 条件
    /// ごと 1 回・rrf_k はメモリ内」で grid を掃くための分離 API であり、
    /// production 側の 2 メソッドもここを通ることで融合経路が 1 本化される。
    ///
    /// `candidates` (pool サイズ) は **呼び出し側が算出して渡す**。bounded 経路は
    /// `limit.saturating_mul(5).max(50)`、unbounded 経路は `desired.max(50)` と
    /// 算出式が異なるため、ここでは一切補正しない。
    pub(crate) fn search_split_candidates(
        &self,
        query_text: &str,
        query_embedding: &[f32],
        candidates: u32,
        filters: &SearchFilters<'_>,
        fusion: FusionParams,
    ) -> Result<(CandidateHits, CandidateHits)> {
        let vec_hits = self.search_vec_candidates(query_embedding, candidates, filters)?;
        let fts_hits = self.search_fts_candidates(query_text, candidates, filters, fusion)?;
        Ok((vec_hits, fts_hits))
    }

    /// FTS5 phrase 全体にマッチする chunk 数 (LIMIT / filter なし)。
    /// `kb-mcp tune` の phrase doc-freq 診断専用 (feature-47 D-11-6)。
    /// sanitize 後のクエリが trigram 下限未満なら `Ok(0)`。
    pub(crate) fn count_fts_matches(&self, query_text: &str) -> Result<i64> {
        let Some(fts_query) = sanitize_fts_query(query_text) else {
            return Ok(0);
        };
        let n: i64 = self.conn.query_row(
            "SELECT count(*) FROM fts_chunks WHERE fts_chunks MATCH ?1",
            params![fts_query],
            |row| row.get(0),
        )?;
        Ok(n)
    }

    /// RRF 用: ベクトル検索の候補を `(chunk_id, SearchResult)` で返す。
    /// 既存の `search_similar` とロジックは同じだが chunk_id を外に出す。
    /// ベクトル検索で最大 `limit` 件の候補を `(chunk_id, SearchResult)` で返す。
    /// `score` フィールドには距離 (小さいほど類似) が入る。
    ///
    /// category / topic フィルタが指定されている場合は、Rust 側でフィルタが
    /// 適用されて候補が減る分を補うため `FILTER_OVERFETCH_FACTOR` 倍の
    /// KNN を SQLite へ投げる ([`FILTER_OVERFETCH_CAP`] 上限)。
    /// KNN 候補を `limit` 件返す。filter が効く場合は over-fetch してから
    /// Rust 側で刈り込む。Connection Graph (`crate::graph`) でも利用する。
    pub(crate) fn search_vec_candidates(
        &self,
        query_embedding: &[f32],
        limit: u32,
        filters: &SearchFilters<'_>,
    ) -> Result<Vec<(i64, SearchResult)>> {
        // filter 指定があれば over-fetch する (詳細は SearchFilters::has_any)。
        // category/topic/path_globs/tags/date は Rust 側フィルタなので
        // 必ず over-fetch が必要、min_quality 単独でも fail-safe で広げる。
        let fetch_k = if filters.has_any() {
            limit
                .saturating_mul(FILTER_OVERFETCH_FACTOR)
                .min(FILTER_OVERFETCH_CAP)
        } else {
            limit
        }
        // sqlite-vec の KNN は `k` に固定上限 (4096) を持ち、超えると
        // "k value in knn query too large" の SQL error になる。
        // full-audit 2026-07-26 で発覚: 既定の min_quality=0.3 は
        // `has_any()` を true にするので over-fetch が効き、`--limit 82` 以上の
        // 検索が released v0.13.0 でも常にこのエラーで失敗していた
        // (82*5*10 = 4100 > 4096)。候補が減るだけの degrade に倒す。
        .min(VEC_KNN_MAX_K);
        let embedding_json = serde_json::to_string(query_embedding)?;
        let sql = "
            SELECT v.chunk_id, v.distance,
                   c.content, c.heading, c.quality_score, c.document_id,
                   d.path, d.title, d.topic, d.date, d.category, d.tags, c.context_text
            FROM vec_chunks v
            JOIN chunks c ON c.id = v.chunk_id
            JOIN documents d ON d.id = c.document_id
            WHERE v.embedding MATCH ?1 AND k = ?2
            ORDER BY v.distance
        ";
        let mut stmt = self.conn.prepare(sql)?;
        let rows = stmt.query_map(params![embedding_json, fetch_k], |row| {
            let chunk_id: i64 = row.get(0)?;
            let distance: f32 = row.get(1)?;
            Ok((
                chunk_id,
                distance,
                row.get::<_, String>(2)?,          // content
                row.get::<_, Option<String>>(3)?,  // heading
                row.get::<_, f32>(4)?,             // quality_score
                row.get::<_, i64>(5)?,             // document_id (F-41)
                row.get::<_, String>(6)?,          // path
                row.get::<_, Option<String>>(7)?,  // title
                row.get::<_, Option<String>>(8)?,  // topic
                row.get::<_, Option<String>>(9)?,  // date
                row.get::<_, Option<String>>(10)?, // category
                row.get::<_, Option<String>>(11)?, // tags (JSON)
                row.get::<_, Option<String>>(12)?, // context_text
            ))
        })?;

        // 事前確保は **SQL に渡した `fetch_k`** を基準にする。`limit` は
        // 呼び出し側で cap されない値が来うるため、これを直接使うと
        // `Vec::with_capacity(u32::MAX)` = allocation abort になる
        // (full-audit 2026-07-26 AU-01: 実機で 927 GB 確保を試みて即死)。
        // `fetch_k` は上の `FILTER_OVERFETCH_CAP` clamp を通っており、
        // filter 無しの経路でも `FILTER_OVERFETCH_CAP` を上限として扱う。
        let mut out = Vec::with_capacity(fetch_k.min(FILTER_OVERFETCH_CAP) as usize);
        for row in rows {
            let (
                chunk_id,
                distance,
                content,
                heading,
                quality_score,
                doc_id,
                path,
                title,
                r_topic,
                date,
                r_category,
                tags_json,
                context_text,
            ) = row?;
            if filters.min_quality > 0.0 && quality_score < filters.min_quality {
                continue;
            }
            if let Some(cat) = filters.category
                && r_category.as_deref() != Some(cat)
            {
                continue;
            }
            if let Some(t) = filters.topic
                && r_topic.as_deref() != Some(t)
            {
                continue;
            }
            // path_globs filter (Task 3 追加)
            if let Some(cpg) = filters.path_globs
                && !cpg.matches(&path)
            {
                continue;
            }
            // tags_any / tags_all filter (Task 3 追加)
            let hit_tags = self.parse_tags_json_recording(tags_json);
            if !matches_tags_any(&hit_tags, filters.tags_any) {
                continue;
            }
            if !matches_tags_all(&hit_tags, filters.tags_all) {
                continue;
            }
            // date_from / date_to filter (Task 3 追加)
            if !matches_date_range(date.as_deref(), filters.date_from, filters.date_to) {
                continue;
            }
            out.push((
                chunk_id,
                SearchResult {
                    score: distance,
                    content,
                    heading,
                    document_id: doc_id,
                    path,
                    title,
                    topic: r_topic,
                    date,
                    tags: hit_tags,
                    context_text,
                },
            ));
            if out.len() >= limit as usize {
                break;
            }
        }
        Ok(out)
    }
}
