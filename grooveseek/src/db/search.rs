//! Retrieval for [`Database`]: vector KNN, FTS5 candidates, and the RRF fusion
//! that merges them.
//!
//! This is the half of the module whose behaviour is observable as a *number* —
//! `groove eval` and `groove tune` measure exactly what these methods return —
//! so it is worth keeping apart from the storage code that feeds it.
//!
//! Split out of `db.rs` in AU-25 (PR-2). The methods are byte-identical to
//! their previous form and keep their existing visibility: an inherent method's
//! `pub` / `pub(crate)` does not depend on which module its `impl` block sits
//! in, so no call site outside this crate's `db` module changed.
//!
//! What stayed behind in `db.rs`, and why: `VEC_KNN_MAX_K` (also used by the
//! storage half) and `FTS_CANDIDATE_CALLS` — that one is named by path from
//! `tune.rs` (`crate::db::FTS_CANDIDATE_CALLS`), so moving it would break the
//! AU-22 round-trip guard that counts FTS calls during a sweep.
//!
//! Parsing a user query — the FTS5 MATCH expression it compiles to, the terms
//! it excludes, and the text an embedder should see — lives in the sibling
//! module [`super::fts_query`] (feature-48, v0.16.0; exclusions in feature-55).
//! `db.rs` re-exports [`crate::db::ParsedQuery`] and [`crate::db::parse_query`]
//! from it, and this module sees both through the `use super::*;` below.
//!
//! A hybrid search parses **once inside this layer**:
//! [`Database::search_split_candidates`] parses the raw string it was given and
//! hands the same [`ParsedQuery`] to both halves, so the phrases the full-text
//! half searched for and the ids the vector half dropped cannot come from two
//! different readings of it. The request as a whole is parsed at the entry
//! point as well — that parse is what the embedder, the reranker and the spans
//! see — and the two readings agree because [`crate::db::parse_query`] is pure,
//! not because there is only one of them. The single-leg entry points
//! ([`crate::db::Database::search_fts_candidates`],
//! [`crate::db::Database::count_fts_matches`]) parse for themselves — they are
//! [`crate::tune`]'s, and have no second half to agree with.

// The parent module is what this file was carved out of, so it keeps seeing
// exactly what it saw before. A hand-written list would be a second thing to
// maintain and, on a move this size, a place to silently drop a name.
use super::*;
/// filter (category / topic) を Rust 側で適用する際の KNN / FTS の over-fetch 倍率。
/// filter が選択的な場合に target `limit` 件に届くよう多めに候補を取る。
///
/// `pub(super)` なのは、除外の再取得テストが「最初の枠を埋め尽くす数」をこの値から
/// 導くため — 10 を書き写すと、倍率を変えた日にテストが黙って意味を失う。
pub(super) const FILTER_OVERFETCH_FACTOR: u32 = 10;
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

/// [`Database::fetch_vec_page`] が KNN 1 回から持ち帰るもの。
///
/// 3 要素の tuple にすると呼び出し側で `.0` / `.1` の意味が読めなくなる。どれも
/// 「もう一度引くべきか」を決める材料なので、名前を付けて 1 個で返す。
struct VecPage {
    /// クエリから読んだ行数。`fetch_k` に満たなければ corpus を読み切っている。
    rows_seen: usize,
    /// そのうち `excluded` に載っていたので落とした行数。**0 なら、足りないのは
    /// 除外のせいではない**。
    dropped_by_exclusion: usize,
    /// 行ごとの連言 (除外 → filter 群) を通った候補。
    hits: Vec<(i64, SearchResult)>,
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
        self.search_fts_candidates_parsed(&parse_query(query_text), limit, filters, fusion)
    }

    /// [`Self::search_fts_candidates`] の本体。解析済みのクエリを受ける形。
    ///
    /// [`Self::search_split_candidates`] は vector 半身と同じ [`ParsedQuery`] をここへ渡す。
    /// 文字列から作り直すと、除外に使う式と FTS が探す式が別々の解析結果になり得る。
    ///
    /// 除外がある場合の MATCH 式は `(正) NOT (負)` で、`NOT` は**式の中**にあるので
    /// SQL の `LIMIT` は除外**後**の行に効く (公式 docs に記述が無いので、`db.rs` の
    /// test module にある `the_limit_is_applied_after_the_exclusion_not_before` が
    /// 実 SQLite で固定している)。
    pub(crate) fn search_fts_candidates_parsed(
        &self,
        query: &ParsedQuery<'_>,
        limit: u32,
        filters: &SearchFilters<'_>,
        fusion: FusionParams,
    ) -> Result<Vec<(i64, SearchResult)>> {
        #[cfg(test)]
        FTS_CANDIDATE_CALLS.with(|c| c.set(c.get() + 1));
        // 切り詰めの警告は 1 検索 1 回。ここが「クエリを FTS に投げる」唯一の経路。
        query.warn_if_truncated();
        let Some(fts_query) = query.match_expr() else {
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
    /// `groove tune` が「vec 候補は query あたり 1 回・FTS 候補は bm25 条件
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
        let parsed = parse_query(query_text);
        let excluded = self.excluded_chunk_ids(parsed.negative_match().as_deref())?;
        let vec_hits =
            self.search_vec_candidates_excluding(query_embedding, candidates, filters, &excluded)?;
        let fts_hits = self.search_fts_candidates_parsed(&parsed, candidates, filters, fusion)?;
        Ok((vec_hits, fts_hits))
    }

    /// 負の式にマッチする chunk id。vector 半身から落とす集合 (F-4)。
    ///
    /// 「その chunk は除外語を含むか」の判定は FTS5 (trigram / case-insensitive /
    /// `remove_diacritics`) に任せ、FTS 半身の `NOT` と**同じ式文字列**を使う =
    /// 一つの問いに一つの実装。Rust 側で content を substring 検索すると、
    /// 大小・濁点・見出し列の扱いが 2 つに分かれる。
    ///
    /// 判定対象は FTS の行、すなわち `heading` / `context` / `content` の 3 列
    /// (正側が探すのと同じ text)。本文に無くても heading や生成された context に
    /// 除外語があれば、その chunk は落ちる。
    ///
    /// bm25 も `ORDER BY` も **`LIMIT` も無い**: 順位は要らず、`LIMIT` を付けると
    /// 除外漏れが静かに起きる。コストは負 phrase 群の rowid 走査 1 回で、cap は
    /// 置かない (phrase 数は [`ParsedQuery`] 側で 32 に bound されている)。
    pub(crate) fn excluded_chunk_ids(&self, negative: Option<&str>) -> Result<HashSet<i64>> {
        let Some(expr) = negative else {
            return Ok(HashSet::new());
        };
        let mut stmt = self
            .conn
            .prepare("SELECT rowid FROM fts_chunks WHERE fts_chunks MATCH ?1")?;
        let ids = stmt.query_map(params![expr], |row| row.get::<_, i64>(0))?;
        Ok(ids.collect::<std::result::Result<HashSet<_>, _>>()?)
    }

    /// クエリの phrase のいずれかにマッチする chunk 数 (LIMIT / filter なし)。
    /// `groove tune` の FTS 識別力診断専用 (feature-47 D-11-6 / feature-48)。
    ///
    /// **v0.16.0 で意味が変わった**: 旧実装はクエリ全体を 1 phrase にしていたので
    /// これは「その phrase の doc-freq」= FTS5 の IDF クランプ条件そのものだったが、
    /// 現在は [`parse_query`] が生む複数 phrase の**和集合**の大きさであり、
    /// 個々の phrase の doc-freq の**上界**でしかない
    /// (`tune::grid::QueryDiagnostics::idf_clamped` の注記を参照)。
    ///
    /// feature-55 以降、クエリが除外を含むなら数えるのは `(正) NOT (負)` の行数 =
    /// **除外語を含む行は数えない**。
    ///
    /// 有効な phrase が 1 つも作れないクエリは `Ok(0)`。**「3 文字未満なら」ではない** —
    /// `ab` は落ちるが `AI と ML` は全体 fallback で phrase になる。
    pub(crate) fn count_fts_matches(&self, query_text: &str) -> Result<i64> {
        let Some(fts_query) = parse_query(query_text).match_expr() else {
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
        self.search_vec_candidates_excluding(query_embedding, limit, filters, &HashSet::new())
    }

    /// [`Self::search_vec_candidates`] の本体に、除外する chunk id 集合を足した形 (F-4)。
    ///
    /// `excluded` は [`Self::excluded_chunk_ids`] が返す「負の FTS 式にマッチした行」で、
    /// filter と同じく Rust 側で 1 行ずつ落とす。**空集合なら
    /// [`Self::search_vec_candidates`] と bit-exact** — over-fetch の条件にも入らず、
    /// KNN も下の再取得ループを 1 周しかしない。
    ///
    /// # そのページが除外で行を落としたときだけ回す再取得ループ (codex review round 1・2、P2)
    ///
    /// 倍率をかけた枠は**除外だけで尽きうる**。`-について` のように大半の chunk に
    /// 当たる除外語なら、最も近い `limit` × [`FILTER_OVERFETCH_FACTOR`] 件が全部
    /// 除外語を持つことは普通に起きる。1 回きりの KNN だと、その 1 件後ろに適格な近傍がいても
    /// 0 件で返る。そこで [`Self::fetch_vec_page`] を呼び直し、次のどれかが成り立つまで
    /// `fetch_k` を倍にする:
    ///
    /// - `limit` 件埋まった
    /// - そのページが除外で 1 行も落としていない ([`VecPage::dropped_by_exclusion`] が 0)
    /// - KNN が `fetch_k` に満たない行数を返した = corpus を読み切った
    /// - `fetch_k` が [`VEC_KNN_MAX_K`] に達した
    ///
    /// 2 つ目が「`excluded` が空」ではないのが要点 (round 2)。category / path / date /
    /// quality の filter で `limit` に届かないのは feature-26 以来の既存挙動で、ここで
    /// 直す話ではない。「除外集合が空でなければ広げる」にすると、**取ってきた候補を 1 件も
    /// 落としていない** `-term` が、その filter 付きクエリの候補リストを変え、KNN を上限まで
    /// 引き延ばす — 同じクエリを除外なしで投げたときと挙動が違ってしまう。除外が原因で
    /// 足りないときだけ広げる。空集合はどの行も落とせないので、この条件が
    /// 「`excluded` が空なら 1 回」も兼ねる。
    ///
    /// KNN に cursor は無いので、各回は**最初から引き直して結果を作り直す** (追記すると
    /// 重複する)。最悪ケースは「除外語がほぼ全近傍に当たる」場合の、`k` に
    /// [`VEC_KNN_MAX_K`] を取った KNN 1 回ぶんで、そこで打ち切る。
    /// 2 回目以降の `fetch_k` は [`FILTER_OVERFETCH_CAP`] を超え得るが
    /// [`VEC_KNN_MAX_K`] 以下には収まる (AU-01 の事前確保の話は
    /// [`Self::fetch_vec_page`] 側の注記を参照)。
    pub(crate) fn search_vec_candidates_excluding(
        &self,
        query_embedding: &[f32],
        limit: u32,
        filters: &SearchFilters<'_>,
        excluded: &HashSet<i64>,
    ) -> Result<Vec<(i64, SearchResult)>> {
        // filter 指定があれば over-fetch する (詳細は SearchFilters::has_any)。
        // category/topic/path_globs/tags/date は Rust 側フィルタなので
        // 必ず over-fetch が必要、min_quality 単独でも fail-safe で広げる。
        // 除外も同じ理由で広げる — 最近傍が除外語を含むだけで limit が埋まらなくなる。
        let mut fetch_k = if filters.has_any() || !excluded.is_empty() {
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

        loop {
            let page = self.fetch_vec_page(&embedding_json, fetch_k, limit, filters, excluded)?;
            if page.hits.len() >= limit as usize
                || page.dropped_by_exclusion == 0
                || page.rows_seen < fetch_k as usize
                || fetch_k >= VEC_KNN_MAX_K
            {
                return Ok(page.hits);
            }
            fetch_k = fetch_k.saturating_mul(2).min(VEC_KNN_MAX_K);
        }
    }

    /// KNN を 1 回だけ引き、行ごとの連言 (除外 → filter 群) を通したものを返す。
    ///
    /// [`VecPage::rows_seen`] が `fetch_k` に満たなければ corpus を読み切ったということ
    /// なので、呼び出し側はそこで再取得をやめる。[`VecPage::dropped_by_exclusion`] が 0 なら
    /// 足りない原因は除外ではない (= filter) ので、やはりやめる。
    ///
    /// `limit` 件埋まった時点で読むのをやめるため、その場合はどちらの数も**途中まで**の
    /// 値になる。ただしそのときは呼び出し側の「埋まった」条件が先に成立するので、
    /// 2 つとも参照されない。
    fn fetch_vec_page(
        &self,
        embedding_json: &str,
        fetch_k: u32,
        limit: u32,
        filters: &SearchFilters<'_>,
        excluded: &HashSet<i64>,
    ) -> Result<VecPage> {
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
        #[cfg(test)]
        VEC_KNN_ATTEMPTS.with(|c| c.set(c.get() + 1));
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
        // `fetch_k` は呼び出し側で `VEC_KNN_MAX_K` に clamp 済みなので、再取得で
        // 倍にしていってもここは 4096 要素で頭打ちになる。
        let mut out = Vec::with_capacity(fetch_k.min(FILTER_OVERFETCH_CAP) as usize);
        let mut rows_seen = 0usize;
        let mut dropped_by_exclusion = 0usize;
        for row in rows {
            rows_seen += 1;
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
            // 除外 (F-4)。filter と同じ行ごとの連言なので、他の filter との順序は無関係。
            // 落とした数を数えるのは、枠を広げ直すべきかの判定に使うため — 順序が
            // 無関係でも**この判定だけは先頭**に置く必要がある。filter の後ろに回すと
            // 「filter で落ちた行が除外語も持っていた」ケースを数え損ねる。
            if excluded.contains(&chunk_id) {
                dropped_by_exclusion += 1;
                continue;
            }
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
        Ok(VecPage {
            rows_seen,
            dropped_by_exclusion,
            hits: out,
        })
    }
}
