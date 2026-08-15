// AU-25. `impl Database` had grown to ~1670 lines across roughly fifty
// methods. Schema creation and the forward migrations are cut out first
// because they are the group that talks mostly to itself: every one of them is
// reached through `init`, and only six are called from anywhere else.
mod fts_query;
mod meta;
mod schema;
mod search;
mod storage;

// feature-48. The FTS half of the hybrid search compiles a user query into an
// FTS5 MATCH expression; that compilation is a self-contained string-to-string
// problem with no database access, so it lives in its own module with its own
// tests. Imported here (not `#[cfg(test)]`, unlike `parse_dim_from_create_sql`
// below) because `search.rs` reaches it through `use super::*`.
use fts_query::build_fts_query;
// `server::compute_match_spans` が citation の offset を求めるのに同じ分割規則を使う。
pub(crate) use fts_query::query_phrases;

// `parse_dim_from_create_sql` moved with the rest of the schema code, but its
// unit test did not: `mod tests` below is one module, and splitting it would
// mean editing tests to prove a refactor changed nothing. Re-imported here so
// the test's `use super::*` still reaches it. `#[cfg(test)]` because no
// production code in this module calls it any more.
#[cfg(test)]
use schema::parse_dim_from_create_sql;

use anyhow::{Context, Result};
use rusqlite::{Connection, TransactionBehavior, params};
use std::collections::HashMap;
use std::sync::Once;
use std::sync::atomic::{AtomicU64, Ordering};

/// sqlite-vec の KNN query が受理する `k` の固定上限。これを超えると
/// `k value in knn query too large, provided N and the limit is 4096` の
/// SQL error になるため、`fetch_k` は必ずこの値以下に clamp する。
/// (full-audit 2026-07-26: `FILTER_OVERFETCH_CAP` がこの上限を超えており、
/// 既定 quality filter 有効時に `--limit 82` 以上が全滅していた)
const VEC_KNN_MAX_K: u32 = 4096;

// Calls to `Database::search_fts_candidates` on the current thread.
//
// Thread-local rather than a process-wide atomic on purpose: `cargo test` runs
// tests on parallel threads and several of them search, so a shared counter
// would be polluted by whatever happens to run alongside. The sweep in
// `tune::build_metric_table` calls the function synchronously from the caller's
// thread, so a thread-local sees exactly its own round trips.
//
// Reset it before the call being measured; nothing clears it for you.
//
// (A doc comment cannot be used here — `thread_local!` is a macro, and rustc
// warns that the comment would document nothing.)
#[cfg(test)]
thread_local! {
    pub(crate) static FTS_CANDIDATE_CALLS: std::cell::Cell<usize> =
        const { std::cell::Cell::new(0) };
}

/// Fusion (RRF + FTS5 bm25 column weight) の実行時パラメータ。
///
/// feature-47 以前はすべてコンパイル時定数だった。`[search.fusion]` から
/// 設定できるようにするため、`SearchFilters` (feature-26) と同じ
/// 「引数 1 個に集約して渡す」方式で db 層に注入する。`Database` の
/// フィールドにはしない — `Database` は drop 順序に依存する手動 `Drop`
/// impl を持っており (db.rs の struct 宣言コメント参照)、フィールド追加は
/// その制約と干渉するため。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FusionParams {
    /// RRF の定数項 k。小さいほど「片方の検索器が確信を持って 1 位に出した
    /// 文書」を上位へ救い、大きいほど両リスト掲載 (合意) を重視する。
    pub rrf_k: f32,
    /// FTS5 bm25 の heading 列重み。
    pub bm25_heading_weight: f32,
    /// 同 context 列重み。
    pub bm25_context_weight: f32,
    /// 同 content 列重み。
    pub bm25_content_weight: f32,
}

impl Default for FusionParams {
    /// k=60 は RRF 原論文および主要実装 (Elasticsearch `rank_constant` /
    /// Milvus / Vespa / LanceDB) の慣例値。bm25 の列順・既定重みは
    /// `fts_chunks` の CREATE 順 (heading, context, content) と一致させる。
    fn default() -> Self {
        Self {
            rrf_k: 60.0,
            bm25_heading_weight: 2.0,
            bm25_context_weight: 1.0,
            bm25_content_weight: 1.0,
        }
    }
}

/// `fetch_embeddings_by_chunk_ids` の IN 句 batch サイズ。
/// SQLite `SQLITE_MAX_VARIABLE_NUMBER` は modern SQLite で 32766 だが、
/// 余裕を持たせ + prepared statement の準備コストとのバランスで 500 を採用。
/// 典型的な MMR pool (≤ 500) では 1 round-trip で済み、高 limit (limit=10000
/// で pool=50000 等) でも 100 回程度の round-trip で完了する。
const EMBEDDING_FETCH_BATCH: usize = 500;

// ---------------------------------------------------------------------------
// Public result types
// ---------------------------------------------------------------------------

/// A single vector-search hit returned by [`Database::search_similar`].
#[derive(Debug, Clone)]
pub struct SearchResult {
    pub score: f32,
    pub content: String,
    pub heading: Option<String>,
    /// F-41 PR-2: `chunks.document_id` を SQL の `SELECT` で carry し、
    /// MMR pool 構築時の N+1 lookup (`lookup_document_id_by_path`) を回避。
    /// rename race の `unwrap_or(0)` collision (F-44) も同時に消える。
    pub document_id: i64,
    pub path: String,
    pub title: Option<String>,
    pub topic: Option<String>,
    pub date: Option<String>,
    pub tags: Vec<String>,
    /// feature-46: contextualized retrieval 用の context prefix (chunk 生成時に
    /// LLM が付与)。`None` = context 機能 off の DB / context なし chunk。
    /// reranker 入力合成にのみ使い、`SearchHit` へは carry しない (API 不変)。
    pub context_text: Option<String>,
}

/// `SearchHit.content` (UTF-8) 内の byte offset 範囲。
/// `start` / `end` は **必ず char (UTF-8 codepoint) 境界に一致**することを
/// 計算側が保証する。クライアントは `content.get(start..end).unwrap_or("")`
/// で安全に slice すべき。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct MatchSpan {
    pub start: usize,
    pub end: usize,
}

/// Parent retriever が `SearchHit.content` を表示拡張した範囲のメタデータ。
/// `Option<ExpandedRange>` として `SearchHit` に持たせる。
///
/// - `None` (or 不在 — `skip_serializing_if`): Parent retriever off or 元 chunk のまま
/// - `Adjacent { from_index, to_index }`: 隣接 chunk と merge。`from_index` /
///   `to_index` は `chunks.chunk_index` (DB 列値、0-indexed)。inclusive range
/// - `WholeDocument { total_chunks }`: 同 doc 全 chunk を連結。`total_chunks`
///   は doc 内 chunk 数 (variant payload からは derive 不能なので保持)
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ExpandedRange {
    Adjacent { from_index: usize, to_index: usize },
    WholeDocument { total_chunks: usize },
}

/// JSON-serializable view of [`SearchResult`]. DB 層 (rusqlite) は `serde` 非依存
/// のままにしておき、API / CLI への露出はこの型を経由する。
///
/// フィールドは `SearchResult` と同形。`From<SearchResult>` で移し替えるだけ。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SearchHit {
    pub score: f32,
    pub path: String,
    pub title: Option<String>,
    pub heading: Option<String>,
    pub topic: Option<String>,
    pub date: Option<String>,
    pub tags: Vec<String>,
    pub content: String,

    /// `null` (省略) = 未計算 (機能非対応 — non-ASCII term を含む query) /
    /// `[]` = 計算済みだが一致なし / `[{...}]` = 計算済みでマッチあり。
    /// **Serialize 時は `None` で key 不在になる** (`null` ではない)。
    /// Deserialize 側は `null` と key 不在を区別しない (どちらも None)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub match_spans: Option<Vec<MatchSpan>>,

    /// Parent retriever expansion metadata. None when expansion is off
    /// or the hit chunk was not expanded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expanded_from: Option<ExpandedRange>,
}

impl From<SearchResult> for SearchHit {
    fn from(r: SearchResult) -> Self {
        Self {
            score: r.score,
            path: r.path,
            title: r.title,
            heading: r.heading,
            topic: r.topic,
            date: r.date,
            tags: r.tags,
            content: r.content,
            match_spans: None,
            expanded_from: None,
        }
    }
}

/// Parent retriever 用 chunk row 抜粋。`fetch_chunks_by_index_range` の戻り値要素。
///
/// Display-time content expansion で隣接 chunk を読み取るために必要な
/// 最小フィールドのみ (`chunk_index` / `content` / `token_count` / `level`)。
/// `level` は legacy DB (feature-28 以前) では NULL になる可能性があるため、
/// `Option<u8>` として返す。
#[derive(Debug, Clone)]
pub struct ChunkRow {
    pub chunk_index: i64,
    pub content: String,
    pub token_count: Option<i64>,
    pub level: Option<u8>,
}

/// index の context 適用状態 (feature-46)。`index_meta.context_mode` に永続化する。
/// - `Off`: context を embedding / FTS に使わない (legacy DB は grandfather でここ)
/// - `Static`: parser 生成の静的 context を embedding + FTS + reranker に注入
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContextMode {
    Off,
    Static,
}

impl ContextMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Static => "static",
        }
    }
    pub fn from_str_opt(s: &str) -> Option<Self> {
        match s {
            "off" => Some(Self::Off),
            "static" => Some(Self::Static),
            _ => None,
        }
    }
}

/// Search 系 API に渡す filter 引数の集約。
///
/// 既存の category / topic / min_quality に加え、feature-26 で path_globs /
/// tags_any / tags_all / date_from / date_to を追加した。引数が増えすぎて
/// `clippy::too_many_arguments` 連発と可読性悪化を招くため、構造体 1 個に統合。
///
/// `Default` 実装で「すべてフィルタ無効」を表現する。
#[derive(Debug, Default, Clone)]
pub struct SearchFilters<'a> {
    pub category: Option<&'a str>,
    pub topic: Option<&'a str>,
    pub min_quality: f32,
    pub path_globs: Option<&'a CompiledPathGlobs>,
    pub tags_any: &'a [String],
    pub tags_all: &'a [String],
    pub date_from: Option<&'a str>,
    pub date_to: Option<&'a str>,
}

impl<'a> SearchFilters<'a> {
    /// いずれかのフィルタが指定されているか。over-fetch 判定で使う。
    ///
    /// 注: `min_quality > 0.0` も含める。feature-25 以前は category/topic
    /// だけで判定していたが、feature-26 で新フィルタ (path_globs/tags/date)
    /// と一緒に扱う形に統合した。`min_quality` 単体指定でも over-fetch
    /// が発動する (`FILTER_OVERFETCH_CAP` で頭打ち、害は低い)。
    pub fn has_any(&self) -> bool {
        self.category.is_some()
            || self.topic.is_some()
            || self.min_quality > 0.0
            || self.path_globs.is_some()
            || !self.tags_any.is_empty()
            || !self.tags_all.is_empty()
            || self.date_from.is_some()
            || self.date_to.is_some()
    }
}

/// `path_globs` の include / exclude を 2 本の GlobSet に分けてコンパイル
/// したもの。Task 3 で実体化される。Task 1 では空のスタブ。
#[derive(Debug, Default, Clone)]
pub struct CompiledPathGlobs {
    pub include: Option<globset::GlobSet>,
    pub exclude: Option<globset::GlobSet>,
}

impl CompiledPathGlobs {
    pub fn matches(&self, path: &str) -> bool {
        let included = match &self.include {
            Some(set) => set.is_match(path),
            None => true,
        };
        let excluded = match &self.exclude {
            Some(set) => set.is_match(path),
            None => false,
        };
        included && !excluded
    }
}

impl Database {
    /// Run one statement directly, for tests that need to put the index into a
    /// state no code path produces on purpose.
    ///
    /// `doctor`'s whole job is noticing that `chunks`, `vec_chunks` and
    /// `fts_chunks` have stopped agreeing, and every write path in this module
    /// exists to keep them agreeing — so the only way to test the detection is
    /// to break the invariant deliberately. Test-only, like
    /// `KbServerShared::for_test`.
    #[cfg(any(test, feature = "test-helpers"))]
    pub fn execute_for_test(&self, sql: &str) -> Result<()> {
        self.conn.execute_batch(sql)?;
        Ok(())
    }
}

/// The answer to one of `kb-mcp doctor`'s integrity questions (feature-51).
///
/// `count` is every row that matched; `samples` is the first few, so a report
/// can name something concrete without printing an unbounded list.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IntegrityScan {
    pub count: u64,
    pub samples: Vec<String>,
}

impl IntegrityScan {
    pub fn is_clean(&self) -> bool {
        self.count == 0
    }
}

/// Topic/category grouping returned by [`Database::list_topics`].
#[derive(Debug, Clone)]
pub struct TopicInfo {
    pub category: Option<String>,
    pub topic: Option<String>,
    pub file_count: u32,
    pub last_updated: Option<String>,
    pub titles: Vec<String>,
}

// feature-48: `sanitize_fts_query` はここにあったが、クエリ全体を単一 phrase 化する
// その規則こそが「自然文クエリで FTS 候補が 0 件になる」原因だったので、
// `db/fts_query.rs` の `build_fts_query` に置き換えた。旧規則自体は
// `fallback_whole_query` として同モジュールに残っている (token 化で phrase が
// 1 つも作れなかったときの逃げ道)。

// ---------------------------------------------------------------------------
// Extension loading (once per process)
// ---------------------------------------------------------------------------

static INIT_VEC: Once = Once::new();

// sqlite-vec crate (0.1.x) は `lib.rs` で `fn sqlite3_vec_init()` を引数なし
// として宣言しているため、そのまま `sqlite3_auto_extension` に渡すには
// transmute が必要だった。ここでは SQLite 拡張エントリポイントの正しい ABI
// で同シンボルを再宣言することで、transmute を用いずに関数ポインタとして
// 渡せるようにする。
//
// `#[link(name = "sqlite_vec0")]` は sqlite-vec crate 側の build.rs で用意
// される静的ライブラリを引くためのもの。sqlite-vec crate 側の関数を直接
// 呼ばなくなると dead-code eliminate でリンクから落ちることがあるため、
// こちらでも同じ lib を link 指定する。
//
// `kind = "static"` は sqlite-vec 0.1.x の build.rs が `cc::Build::compile()`
// で静的 .lib を emit している前提に揃えている。将来 sqlite-vec が dylib に
// 切り替えたら rustc が link 種別衝突エラーを出すので、その時点でこちらも
// 追随する。
#[link(name = "sqlite_vec0", kind = "static")]
unsafe extern "C" {
    fn sqlite3_vec_init(
        db: *mut rusqlite::ffi::sqlite3,
        pz_err_msg: *mut *mut std::ffi::c_char,
        p_api: *const rusqlite::ffi::sqlite3_api_routines,
    ) -> std::ffi::c_int;
}

fn ensure_vec_extension() {
    INIT_VEC.call_once(|| unsafe {
        rusqlite::ffi::sqlite3_auto_extension(Some(sqlite3_vec_init));
    });
}

// ---------------------------------------------------------------------------
// Database
// ---------------------------------------------------------------------------

/// Thin wrapper around a rusqlite [`Connection`] that owns the SQLite schema
/// (documents, chunks, vec_chunks, index_meta) and exposes CRUD + vector-search
/// helpers.
pub struct Database {
    // F-63: field 宣言順は **`conn` を第 1、`tags_parse_failures` を第 2 に固定**。
    // Rust の drop 順序は宣言順の逆 = `tags_parse_failures` (AtomicU64) が先に
    // drop され、`conn` (rusqlite::Connection) は後で drop される。本 struct の
    // 手動 `Drop` impl は `self.conn.execute(...)` で counter を `index_meta` に
    // best-effort flush するため、`conn` が生存している必要がある。field 順序を
    // 逆転させると、`Connection::drop` が先に走って ROLLBACK が発火する罠が出る
    // (= 本 spec を必ず再 review すること)。
    conn: Connection,
    /// `parse_tags_json` 失敗カウンタ (F-63)。session 中は atomic increment、
    /// `Database::open` 起動時に `index_meta` から read、`Database::drop` で
    /// best-effort flush。silent fail-open の visibility 確保。
    tags_parse_failures: AtomicU64,
}

/// RRF score の HashMap と row HashMap を受け取り、score DESC + id ASC の
/// 順序で `Vec<(i64, SearchResult)>` を返す。`limit=Some(n)` で上位 n 件に
/// truncate、`None` なら全件返す (MMR-off bypass / `_unbounded` で利用)。
///
/// HashMap iteration の非決定性に依存しないよう、tie-break で id を使い
/// プラットフォーム / 入力順に依存しない出力を保証する (invariant #1)。
///
/// production 経路は feature-47 で [`fuse_rrf`] へ移行した。本関数は
/// `fuse_rrf` の等価性を照合する **oracle** としてテストからのみ参照される
/// (同ファイルの `dummy_search_result_for_id` と同じ扱い)。
#[cfg(test)]
fn rrf_topk(
    mut scores: HashMap<i64, f32>,
    mut rows: HashMap<i64, SearchResult>,
    limit: Option<u32>,
) -> Vec<(i64, SearchResult)> {
    let mut merged: Vec<(i64, f32)> = scores.drain().collect();
    merged.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.0.cmp(&b.0))
    });
    if let Some(n) = limit {
        merged.truncate(n as usize);
    }
    merged
        .into_iter()
        .filter_map(|(id, rrf)| {
            rows.remove(&id).map(|mut r| {
                r.score = rrf;
                (id, r)
            })
        })
        .collect()
}

/// RRF の中核演算。2 本の rank list (要素は `chunk_id`、先頭が rank 0) を
/// `1 / (k + rank + 1)` で加算融合し、**score DESC + chunk_id ASC** の順に
/// 並べた `(chunk_id, rrf_score)` を返す。`limit=Some(n)` で上位 n 件に
/// truncate、`None` なら全件。
///
/// `SearchResult` に触れないので、`kb-mcp tune` が同一の候補リストへ複数の
/// `rrf_k` を再適用するときに候補プールを clone せずに済む (feature-47 D-5 /
/// D-10)。スコア累算は production 挙動を変えないため **f32 のまま**。
pub(crate) fn fuse_rrf_ids(
    vec_ids: &[i64],
    fts_ids: &[i64],
    rrf_k: f32,
    limit: Option<u32>,
) -> Vec<(i64, f32)> {
    let mut scores: HashMap<i64, f32> = HashMap::new();
    for (rank, id) in vec_ids.iter().enumerate() {
        *scores.entry(*id).or_insert(0.0) += 1.0 / (rrf_k + (rank as f32) + 1.0);
    }
    for (rank, id) in fts_ids.iter().enumerate() {
        *scores.entry(*id).or_insert(0.0) += 1.0 / (rrf_k + (rank as f32) + 1.0);
    }
    let mut merged: Vec<(i64, f32)> = scores.into_iter().collect();
    // HashMap iteration の非決定性に依存しないよう id で tie-break する
    // (rrf_topk と同一の全順序、invariant #1)。
    merged.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.0.cmp(&b.0))
    });
    if let Some(n) = limit {
        merged.truncate(n as usize);
    }
    merged
}

/// [`fuse_rrf_ids`] の結果に `SearchResult` を貼り直すラッパ。
///
/// **truncate 後の勝者だけを clone する** ので、production 経路で増える
/// allocation は最大 `limit` 件 (既定 10) に収まる (feature-47 D-5)。
/// 同一 chunk が両リストに現れた場合の row は **vec 側を優先** する
/// (旧 inline 実装が vec → fts の順に `rows.entry(id).or_insert(row)` を
/// 回していた挙動と同一)。
fn fuse_rrf(
    vec_hits: &[(i64, SearchResult)],
    fts_hits: &[(i64, SearchResult)],
    rrf_k: f32,
    limit: Option<u32>,
) -> Vec<(i64, SearchResult)> {
    let vec_ids: Vec<i64> = vec_hits.iter().map(|(id, _)| *id).collect();
    let fts_ids: Vec<i64> = fts_hits.iter().map(|(id, _)| *id).collect();
    let ranked = fuse_rrf_ids(&vec_ids, &fts_ids, rrf_k, limit);

    let mut rows: HashMap<i64, &SearchResult> = HashMap::new();
    for (id, row) in vec_hits.iter().chain(fts_hits.iter()) {
        rows.entry(*id).or_insert(row);
    }

    ranked
        .into_iter()
        .filter_map(|(id, rrf)| {
            rows.get(&id).map(|row| {
                let mut r = (*row).clone();
                r.score = rrf;
                (id, r)
            })
        })
        .collect()
}

/// [`Database::chunk_texts_with_context_for_path`] の戻り値の要素型:
/// `(heading, content, context_text)`。
pub type ChunkTextWithContext = (Option<String>, String, Option<String>);

/// 融合前の候補リスト 1 本分 (`(chunk_id, SearchResult)` の列)。
/// `clippy::type_complexity` 回避のための alias
/// (`parser::markdown::RawChunk` と同じ扱い)。
pub(crate) type CandidateHits = Vec<(i64, SearchResult)>;

impl Database {
    /// Open (or create) a file-backed database at `path`.
    pub fn open(path: &str) -> Result<Self> {
        ensure_vec_extension();
        let conn =
            Connection::open(path).with_context(|| format!("failed to open database at {path}"))?;
        // F-63: AtomicU64 は **session-local delta** として 0 で start。
        // 過去 session の永続値は `tags_parse_failure_count()` が DB read 時に
        // 直接合算するため、startup restore は不要 (= codex P2 fix、
        // multi-instance での last-writer-wins を防ぐ)。
        let db = Self {
            conn,
            tags_parse_failures: AtomicU64::new(0),
        };
        db.init()?;
        Ok(db)
    }

    /// Open an in-memory database (useful for tests).
    pub fn open_in_memory() -> Result<Self> {
        ensure_vec_extension();
        let conn = Connection::open_in_memory().context("failed to open in-memory database")?;
        // F-63: AtomicU64 は session-local delta、startup restore 不要 (= codex P2 fix)
        let db = Self {
            conn,
            tags_parse_failures: AtomicU64::new(0),
        };
        db.init()?;
        Ok(db)
    }

    /// 上位レイヤ (indexer / watcher) が「複数の `upsert_document` /
    /// `insert_chunk` 呼び出しを 1 つの atomic 単位として扱いたい」時に
    /// 使う tx ハンドル。返り値の `Transaction` を保持している間、各 db API
    /// 呼び出しは同じ tx に participate する (autocommit-aware なので
    /// `upsert_document` / `insert_chunk` は内側でネスト tx を張らない)。
    ///
    /// 通常の Drop は **rollback**。成功時は `tx.commit()` を必ず呼ぶこと。
    pub fn begin_transaction(&self) -> Result<rusqlite::Transaction<'_>> {
        Ok(self.conn.unchecked_transaction()?)
    }

    /// 開いたまま残った transaction を巻き戻す (BU-18)。
    ///
    /// 通常、unwind は `Transaction` の Drop を走らせるので ROLLBACK は自動で
    /// 発行される。**残るのはそれが失敗した時**で、rusqlite は Drop 中の
    /// エラーを握り潰すため、呼び出し側からは成功と区別がつかない
    /// (`mem::forget` 等でハンドルごと落とした場合も同じ状態になる)。
    ///
    /// mutex 越しに共有された `Database` はスレッドが panic しても drop され
    /// ないので、開きっぱなしの transaction はプロセスが終わるまで残る。
    /// `&self` API は autocommit-aware で「開いていれば参加する」設計なので、
    /// 以後の書き込みは全部そこに吸い込まれ、誰も commit しないまま消える。
    ///
    /// `true` = 実際に巻き戻した。**poison から復帰した直後にだけ呼ぶ**想定
    /// (`crate::poison::recover_db`)。呼び出し側が保持中の transaction を
    /// 誤って潰さないよう、通常経路からは呼ばないこと。
    pub fn rollback_if_transaction_open(&self) -> Result<bool> {
        if self.conn.is_autocommit() {
            return Ok(false);
        }
        self.conn.execute_batch("ROLLBACK")?;
        Ok(true)
    }

    /// IMMEDIATE (RESERVED lock) トランザクションを開始する (feature-46)。
    /// FTS 3 列 migration の double-checked locking (§4.4) で書き手を単一化する
    /// ために使う (`ensure_fts_context_column` が消費)。`&self` で呼べるよう
    /// `unchecked_transaction` 系の `Transaction::new_unchecked` を behavior=Immediate
    /// で使う (`transaction_with_behavior` は `&mut Connection` 要求で不可)。
    /// 通常 Drop は rollback。成功時は `tx.commit()` を呼ぶこと。
    fn begin_immediate_tx(&self) -> Result<rusqlite::Transaction<'_>> {
        Ok(rusqlite::Transaction::new_unchecked(
            &self.conn,
            TransactionBehavior::Immediate,
        )?)
    }
}

impl Drop for Database {
    /// F-63: session shutdown 時に `tags_parse_failures` の最新値を
    /// `index_meta` に best-effort flush する。
    ///
    /// **設計上の注意**:
    /// - drop 中の panic は process abort になるため、`expect` / `unwrap` は禁止。
    ///   SQLite write が失敗しても `tracing::warn!` で log するだけで握り潰す。
    /// - `Database` struct の field 宣言順 (= `conn` 第 1、`tags_parse_failures`
    ///   第 2) に依存している。Rust の drop 順序は宣言順の逆なので、本 impl が
    ///   走るタイミングでは `conn` (= `rusqlite::Connection`) はまだ生存している。
    fn drop(&mut self) {
        let delta = self.tags_parse_failures.load(Ordering::Relaxed);
        if delta == 0 {
            // session 中に increment ゼロなら SQL roundtrip skip。
            return;
        }
        // codex P2 fix: last-writer-wins ではなく atomic SQL increment で flush。
        // INSERT 時 (= 既存 row なし) は delta を初期値、UPDATE 時 (= 既存 row あり) は
        // 既存 value に delta を加算。両 placeholder ともに本 session の delta を渡す。
        // multi-instance で同 SQLite file を開く運用 (= long-lived `serve` daemon +
        // 別 CLI 並行) でも、各 session の delta が漏れなく加算される。
        let delta_signed: i64 = delta.try_into().unwrap_or(i64::MAX);
        let result = self.conn.execute(
            "INSERT INTO index_meta (key, value) VALUES ('tags_parse_failures', ?1) \
             ON CONFLICT(key) DO UPDATE SET value = CAST(value AS INTEGER) + ?2",
            params![delta.to_string(), delta_signed],
        );
        if let Err(e) = result {
            tracing::warn!(
                error = %e,
                "failed to flush tags_parse_failures delta to index_meta on drop"
            );
        }
    }
}

// ===========================================================================
// Tests
// ===========================================================================

/// F-65 (feature-40): per-id dummy `SearchResult` for `rrf_topk` proptest.
/// proptest 内で score / id 以外を default 値にした row を生成するための
/// module-private helper。production code から呼べない。
#[cfg(test)]
fn dummy_search_result_for_id(id: i64) -> SearchResult {
    SearchResult {
        score: 0.0, // overwritten by rrf_topk
        content: String::new(),
        heading: None,
        document_id: id,
        path: format!("dummy-{}.md", id),
        title: None,
        topic: None,
        date: None,
        tags: Vec::new(),
        context_text: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    /// feature-46: db.rs 専用の一時ディレクトリ helper (tempfile crate 禁止)。
    /// `config.rs::DirGuard` / `tests/config_discovery.rs::TempDir` と同型。
    struct TempDir(std::path::PathBuf);
    impl TempDir {
        fn new(prefix: &str) -> Self {
            let p = crate::test_support::unique_temp_path(&format!("kb-mcp-dbtest-{prefix}"));
            std::fs::create_dir_all(&p).unwrap();
            Self(p)
        }
        fn path(&self) -> &std::path::Path {
            &self.0
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Helper: create a dummy 384-dim embedding filled with `val`.
    fn dummy_embedding(val: f32) -> Vec<f32> {
        vec![val; 384]
    }

    /// Helper: create an in-memory DB and initialize its vec_chunks table
    /// with the legacy 384-dim schema. Most tests below operate on this
    /// setup to mirror a normal runtime where `verify_embedding_meta` has
    /// already run.
    fn db_with_384() -> Database {
        let db = Database::open_in_memory().unwrap();
        db.verify_embedding_meta("bge-small-en-v1.5", 384).unwrap();
        db
    }

    thread_local! {
        /// SQL statements traced off a connection while a test has tracing on.
        /// Lets a test count what SQLite actually executed rather than how many
        /// times a Rust wrapper was called (BU-03).
        static TRACED_SQL: std::cell::RefCell<Vec<String>> =
            const { std::cell::RefCell::new(Vec::new()) };
    }

    /// `trace_v2` takes a plain `fn` pointer, so the sink has to be a static
    /// thread-local rather than a captured closure.
    fn record_traced_sql(evt: rusqlite::trace::TraceEvent<'_>) {
        if let rusqlite::trace::TraceEvent::Stmt(_, sql) = evt {
            TRACED_SQL.with(|v| v.borrow_mut().push(sql.to_string()));
        }
    }

    #[test]
    fn test_schema_creation() {
        let db = Database::open_in_memory().expect("open_in_memory");
        assert_eq!(db.document_count().unwrap(), 0);
        assert_eq!(db.chunk_count().unwrap(), 0);
        println!("test_schema_creation: OK — 0 docs, 0 chunks after fresh init");
    }

    #[test]
    fn test_upsert_and_query_document() {
        let db = db_with_384();

        // First insert
        let id1 = db
            .upsert_document(
                "deep-dive/mcp/overview.md",
                Some("MCP Overview"),
                Some("mcp"),
                Some("deep-dive"),
                Some("1"),
                &["mcp".into(), "protocol".into()],
                Some("2026-04-16"),
                "hash_aaa",
                0,
            )
            .unwrap();
        println!("insert returned id={id1}");
        assert_eq!(db.document_count().unwrap(), 1);

        // Insert a chunk so we can verify cascade-on-upsert
        db.insert_chunk(
            id1,
            0,
            Some("Intro"),
            None,
            "Hello MCP",
            None,
            &dummy_embedding(0.1),
            1.0,
        )
        .unwrap();
        assert_eq!(db.chunk_count().unwrap(), 1);

        // Upsert same path with new hash — should still be 1 doc, 0 chunks
        let id2 = db
            .upsert_document(
                "deep-dive/mcp/overview.md",
                Some("MCP Overview v2"),
                Some("mcp"),
                Some("deep-dive"),
                Some("1"),
                &["mcp".into()],
                Some("2026-04-16"),
                "hash_bbb",
                0,
            )
            .unwrap();
        println!("upsert returned id={id2} (should equal {id1})");
        assert_eq!(id1, id2, "upsert must reuse the same row id");
        assert_eq!(db.document_count().unwrap(), 1, "still 1 document");
        assert_eq!(db.chunk_count().unwrap(), 0, "old chunks deleted on upsert");

        println!("test_upsert_and_query_document: OK");
    }

    #[test]
    fn test_content_hash_check() {
        let db = Database::open_in_memory().unwrap();

        // Non-existent path
        assert!(
            db.get_document_hash("does/not/exist.md").unwrap().is_none(),
            "non-existent path should return None"
        );

        // After insert
        db.upsert_document(
            "ai-news/2026-04-16.md",
            Some("AI News"),
            None,
            Some("ai-news"),
            None,
            &[],
            Some("2026-04-16"),
            "hash_xyz",
            0,
        )
        .unwrap();

        let hash = db
            .get_document_hash("ai-news/2026-04-16.md")
            .unwrap()
            .expect("should be Some");
        assert_eq!(hash, "hash_xyz");

        println!("test_content_hash_check: OK");
    }

    #[test]
    fn test_delete_document() {
        let db = db_with_384();

        let doc_id = db
            .upsert_document(
                "tech-watch/anthropic/2026-04-16.md",
                Some("Anthropic Watch"),
                Some("anthropic"),
                Some("tech-watch"),
                None,
                &["anthropic".into()],
                Some("2026-04-16"),
                "hash_del",
                0,
            )
            .unwrap();
        db.insert_chunk(
            doc_id,
            0,
            None,
            None,
            "some content",
            None,
            &dummy_embedding(0.5),
            1.0,
        )
        .unwrap();
        assert_eq!(db.document_count().unwrap(), 1);
        assert_eq!(db.chunk_count().unwrap(), 1);

        db.delete_document("tech-watch/anthropic/2026-04-16.md")
            .unwrap();
        assert_eq!(db.document_count().unwrap(), 0, "document deleted");
        assert_eq!(db.chunk_count().unwrap(), 0, "chunks deleted");

        println!("test_delete_document: OK");
    }

    #[test]
    fn test_search_similar_executes_knn_query() {
        // Regression: sqlite-vec requires `k = ?` (or literal LIMIT) on knn
        // queries. A bound `LIMIT ?` used to fail with "A LIMIT or 'k = ?'
        // constraint is required on vec0 knn queries".
        let db = db_with_384();

        let doc_id = db
            .upsert_document(
                "deep-dive/mcp/overview.md",
                Some("MCP Overview"),
                Some("mcp"),
                Some("deep-dive"),
                Some("1"),
                &[],
                Some("2026-04-16"),
                "h1",
                0,
            )
            .unwrap();
        db.insert_chunk(
            doc_id,
            0,
            Some("Intro"),
            None,
            "hello",
            None,
            &dummy_embedding(0.1),
            1.0,
        )
        .unwrap();
        db.insert_chunk(
            doc_id,
            1,
            Some("Body"),
            None,
            "world",
            None,
            &dummy_embedding(0.2),
            1.0,
        )
        .unwrap();

        // No filter path
        let hits = db
            .search_similar(&dummy_embedding(0.1), 5, &SearchFilters::default())
            .unwrap();
        assert_eq!(hits.len(), 2, "both chunks should be returned");

        // Filter path (category match)
        let hits = db
            .search_similar(
                &dummy_embedding(0.1),
                5,
                &SearchFilters {
                    category: Some("deep-dive"),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(hits.len(), 2);

        // Filter path (non-matching topic → empty)
        let hits = db
            .search_similar(
                &dummy_embedding(0.1),
                5,
                &SearchFilters {
                    topic: Some("no-such-topic"),
                    ..Default::default()
                },
            )
            .unwrap();
        assert!(hits.is_empty());
    }

    #[test]
    fn test_quality_filter_excludes_low_scored_chunks() {
        let db = db_with_384();
        let doc_id = db
            .upsert_document("q.md", Some("Q"), None, None, None, &[], None, "h", 0)
            .unwrap();
        // 高品質チャンク (1.0) と低品質チャンク (0.1)
        db.insert_chunk(
            doc_id,
            0,
            Some("high"),
            None,
            "rich body with plenty of content",
            None,
            &dummy_embedding(0.1),
            1.0,
        )
        .unwrap();
        db.insert_chunk(
            doc_id,
            1,
            Some("low"),
            None,
            "stub",
            None,
            &dummy_embedding(0.11),
            0.1,
        )
        .unwrap();

        // threshold=0.0: 両方返る (既存挙動)
        let hits = db
            .search_similar(&dummy_embedding(0.1), 5, &SearchFilters::default())
            .unwrap();
        assert_eq!(hits.len(), 2);

        // threshold=0.5: 高品質のみ
        let hits = db
            .search_similar(
                &dummy_embedding(0.1),
                5,
                &SearchFilters {
                    min_quality: 0.5,
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].heading.as_deref(), Some("high"));

        // hybrid でも同じ挙動
        let hits = db
            .search_hybrid(
                "rich",
                &dummy_embedding(0.1),
                5,
                &SearchFilters {
                    min_quality: 0.5,
                    ..Default::default()
                },
                FusionParams::default(),
            )
            .unwrap();
        assert!(hits.iter().all(|h| h.heading.as_deref() != Some("low")));
    }

    #[test]
    fn test_backfill_quality_is_idempotent() {
        // legacy DB を模倣: score=1.0 のまま低品質チャンクを挿入し、
        // backfill_quality が再評価するか、2 回目は no-op かを検証。
        let db = db_with_384();
        let doc_id = db
            .upsert_document("b.md", None, None, None, None, &[], None, "h", 0)
            .unwrap();
        // 本当はスタブ (短い定型) だが quality_score=1.0 で insert
        db.insert_chunk(
            doc_id,
            0,
            None,
            None,
            "TBD",
            None,
            &dummy_embedding(0.1),
            1.0,
        )
        .unwrap();
        db.insert_chunk(
            doc_id,
            1,
            None,
            None,
            "plenty of informative content indeed, long enough to avoid penalties",
            None,
            &dummy_embedding(0.2),
            1.0,
        )
        .unwrap();

        let updated1 = db.backfill_quality(&[]).unwrap();
        assert!(updated1 >= 1, "stub chunk must be updated, got {updated1}");
        let updated2 = db.backfill_quality(&[]).unwrap();
        assert_eq!(updated2, 0, "second call must be a no-op");
    }

    #[test]
    fn test_backfill_quality_exempts_binary_extension_and_is_stable() {
        let db = Database::open_in_memory().unwrap();
        db.verify_embedding_meta("bge-small-en-v1.5", 384).unwrap();
        // binary 由来を模した短い chunk (page/slide の表紙相当)。path 拡張子 = pdf。
        let doc_id = db
            .upsert_document(
                "docs/report.pdf",
                Some("R"),
                None,
                Some("docs"),
                None,
                &[],
                None,
                "h",
                0,
            )
            .unwrap();
        let emb = vec![0.0f32; 384];
        // quality_score は 1.0 で insert (免除された初回 index 相当)。
        db.insert_chunk(
            doc_id,
            0,
            Some("p.1"),
            None,
            "第3章 リスク管理",
            None,
            &emb,
            1.0,
        )
        .unwrap();

        // binary_exts に "pdf" を渡す → 免除で 1.0 維持。2 回連続でも安定。
        let u1 = db.backfill_quality(&["pdf"]).unwrap();
        let u2 = db.backfill_quality(&["pdf"]).unwrap();
        assert_eq!(u1, 0, "binary chunk must stay exempt (no update)");
        assert_eq!(u2, 0, "second backfill must be a no-op too");
        let (above, _below) = db.chunk_count_by_quality(0.3).unwrap();
        assert_eq!(above, 1, "exempt binary chunk must remain above threshold");
    }

    #[test]
    fn test_backfill_quality_penalizes_when_not_binary() {
        let db = Database::open_in_memory().unwrap();
        db.verify_embedding_meta("bge-small-en-v1.5", 384).unwrap();
        let doc_id = db
            .upsert_document(
                "notes/short.md",
                Some("S"),
                None,
                Some("notes"),
                None,
                &[],
                None,
                "h",
                0,
            )
            .unwrap();
        let emb = vec![0.0f32; 384];
        db.insert_chunk(doc_id, 0, Some("p.1"), None, "短い本文。", None, &emb, 1.0)
            .unwrap();
        // md は binary_exts に無い → penalty 適用で 1.0 未満へ。
        let updated = db.backfill_quality(&[]).unwrap();
        assert_eq!(updated, 1);
        let (_above, below) = db.chunk_count_by_quality(0.3).unwrap();
        assert_eq!(below, 1, "non-binary short chunk drops below threshold");
    }

    #[test]
    fn test_rename_document_preserves_chunks() {
        // File rename: rename_document は path だけ変え、chunks/vec/fts は維持する
        let db = db_with_384();
        let doc_id = db
            .upsert_document(
                "old/path.md",
                Some("T"),
                None,
                None,
                None,
                &[],
                None,
                "hash_same",
                0,
            )
            .unwrap();
        db.insert_chunk(
            doc_id,
            0,
            Some("H"),
            None,
            "content",
            None,
            &dummy_embedding(0.1),
            1.0,
        )
        .unwrap();
        assert_eq!(db.chunk_count().unwrap(), 1);

        // rename
        db.rename_document("old/path.md", "new/path.md").unwrap();

        // chunk 数は不変 (embedding 再計算されない)
        assert_eq!(db.chunk_count().unwrap(), 1);
        // hash は移動しても同じ
        assert_eq!(
            db.get_document_hash("new/path.md").unwrap().as_deref(),
            Some("hash_same")
        );
        assert!(db.get_document_hash("old/path.md").unwrap().is_none());
        // path -> hash map でも反映されている
        let map = db.all_path_hashes().unwrap();
        assert_eq!(map.get("new/path.md"), Some(&"hash_same".to_string()));
        assert!(!map.contains_key("old/path.md"));
    }

    #[test]
    fn test_rename_document_missing_source_errors() {
        let db = db_with_384();
        let err = db
            .rename_document("nope.md", "else.md")
            .expect_err("must error");
        assert!(err.to_string().contains("no document"));
    }

    #[test]
    fn test_rename_documents_atomic_rolls_back_on_failure() {
        // File rename: 途中で失敗したら rollback し、先行の rename も戻ること
        let db = db_with_384();
        db.upsert_document("a.md", None, None, None, None, &[], None, "h_a", 0)
            .unwrap();
        db.upsert_document("b.md", None, None, None, None, &[], None, "h_b", 0)
            .unwrap();

        // 1 件目: a.md -> a2.md (成功するはず)
        // 2 件目: nope.md -> x.md (source が無いので bail)
        let pairs = vec![
            ("a.md".to_string(), "a2.md".to_string()),
            ("nope.md".to_string(), "x.md".to_string()),
        ];
        let err = db
            .rename_documents_atomic(&pairs)
            .expect_err("second pair must fail");
        assert!(err.to_string().contains("no document"));

        // a.md は元の path に戻っていること (rollback)
        let map = db.all_path_hashes().unwrap();
        assert_eq!(map.get("a.md"), Some(&"h_a".to_string()));
        assert!(!map.contains_key("a2.md"));
    }

    #[test]
    fn test_rename_documents_atomic_commits_on_success() {
        let db = db_with_384();
        db.upsert_document("a.md", None, None, None, None, &[], None, "h_a", 0)
            .unwrap();
        db.upsert_document("b.md", None, None, None, None, &[], None, "h_b", 0)
            .unwrap();
        let pairs = vec![
            ("a.md".to_string(), "a2.md".to_string()),
            ("b.md".to_string(), "b2.md".to_string()),
        ];
        db.rename_documents_atomic(&pairs).unwrap();
        let map = db.all_path_hashes().unwrap();
        assert_eq!(map.get("a2.md"), Some(&"h_a".to_string()));
        assert_eq!(map.get("b2.md"), Some(&"h_b".to_string()));
        assert_eq!(map.len(), 2);
    }

    #[test]
    fn test_rename_documents_atomic_empty_pairs_is_noop() {
        let db = db_with_384();
        db.rename_documents_atomic(&[]).unwrap();
    }

    /// F-32 regression: dropping a `begin_transaction()` handle without
    /// `commit()` must roll back every write performed under it (upsert +
    /// insert_chunk). This is the contract the indexer relies on for
    /// per-file atomicity — a partial failure mid-loop must restore the
    /// previous DB state instead of leaving a doc with M < N chunks.
    #[test]
    fn test_begin_transaction_rolls_back_partial_writes_on_drop() {
        let db = db_with_384();
        let doc_id = db
            .upsert_document(
                "a.md",
                Some("a"),
                None,
                None,
                None,
                &[],
                None,
                "h_initial",
                0,
            )
            .unwrap();
        db.insert_chunk(
            doc_id,
            0,
            Some("intro"),
            None,
            "initial body",
            None,
            &dummy_embedding(0.1),
            1.0,
        )
        .unwrap();
        let docs_before = db.document_count().unwrap();
        let chunks_before = db.chunk_count().unwrap();

        {
            let tx = db.begin_transaction().unwrap();
            // UPDATE branch on existing path "a.md" — wipes old chunks/vec/fts
            // and stages a new content_hash. Without commit, all of this must
            // disappear when the tx is dropped.
            db.upsert_document("a.md", Some("a"), None, None, None, &[], None, "h_NEW", 0)
                .unwrap();
            db.insert_chunk(
                doc_id,
                0,
                Some("new"),
                None,
                "new body",
                None,
                &dummy_embedding(0.2),
                1.0,
            )
            .unwrap();
            // tx dropped here without commit → rollback
            drop(tx);
        }

        let map = db.all_path_hashes().unwrap();
        assert_eq!(
            map.get("a.md"),
            Some(&"h_initial".to_string()),
            "rollback should restore original content_hash"
        );
        assert_eq!(db.document_count().unwrap(), docs_before);
        assert_eq!(db.chunk_count().unwrap(), chunks_before);
    }

    /// F-32: explicit `tx.commit()` persists writes — symmetric counterpart
    /// to the rollback-on-drop test above.
    #[test]
    fn test_begin_transaction_commits_on_explicit_commit() {
        let db = db_with_384();
        let doc_id = db
            .upsert_document(
                "a.md",
                Some("a"),
                None,
                None,
                None,
                &[],
                None,
                "h_initial",
                0,
            )
            .unwrap();
        db.insert_chunk(
            doc_id,
            0,
            Some("intro"),
            None,
            "initial body",
            None,
            &dummy_embedding(0.1),
            1.0,
        )
        .unwrap();

        {
            let tx = db.begin_transaction().unwrap();
            db.upsert_document("a.md", Some("a"), None, None, None, &[], None, "h_NEW", 0)
                .unwrap();
            db.insert_chunk(
                doc_id,
                0,
                Some("new"),
                None,
                "new body",
                None,
                &dummy_embedding(0.2),
                1.0,
            )
            .unwrap();
            tx.commit().unwrap();
        }

        let map = db.all_path_hashes().unwrap();
        assert_eq!(map.get("a.md"), Some(&"h_NEW".to_string()));
    }

    #[test]
    fn test_begin_immediate_tx_takes_reserved_lock() {
        // IMMEDIATE tx は開始時点で RESERVED lock を取得する。
        // 別 connection からの書き込みが lock 取得まで待たされることを 2-connection で検証。
        // (Deferred tx では出現時点でのみ lock 取得なので、BEGIN 直後は競合しない)。

        // TmpDir パターン: PID + nanos で unique な一時ディレクトリ
        struct TmpDir(std::path::PathBuf);
        impl Drop for TmpDir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }

        let tmp_dir = crate::test_support::unique_temp_path("kb-mcp-test-immediate-lock");
        std::fs::create_dir_all(&tmp_dir).unwrap();
        let _guard = TmpDir(tmp_dir.clone());

        let db_path = tmp_dir.join("test.db").to_string_lossy().to_string();

        // conn A: Database wrapper で IMMEDIATE tx を開始 (未 commit)
        let db_a = Database::open(&db_path).unwrap();
        let _tx_a = db_a.begin_immediate_tx().unwrap();
        // _tx_a を保持したまま next block へ

        {
            // conn B: 同じ DB に raw rusqlite connection で接続、busy_timeout=0 (即失敗)
            let conn_b = rusqlite::Connection::open(&db_path).expect("failed to open db_path");
            conn_b
                .busy_timeout(std::time::Duration::ZERO)
                .expect("failed to set busy_timeout");

            // conn A が RESERVED lock を持っているため、conn B の BEGIN IMMEDIATE は
            // SQLITE_BUSY で失敗するはず
            let result = conn_b.execute("BEGIN IMMEDIATE", []);
            assert!(
                result.is_err(),
                "Expected SQLITE_BUSY when IMMEDIATE tx encounters held RESERVED lock, but succeeded"
            );
            let err_msg = result.unwrap_err().to_string();
            assert!(
                err_msg.contains("database is locked"),
                "Expected 'database is locked' error, got: {err_msg}"
            );
        }
        // conn_b は drop される (結果は無視)

        // _tx_a を drop (rollback) してから、新 connection が成功することを確認
        drop(_tx_a);

        {
            let conn_b =
                rusqlite::Connection::open(&db_path).expect("failed to open db_path for retry");
            conn_b
                .busy_timeout(std::time::Duration::ZERO)
                .expect("failed to set busy_timeout for retry");

            // lock が解放されたので BEGIN IMMEDIATE が成功するはず
            let result = conn_b.execute("BEGIN IMMEDIATE", []);
            assert!(
                result.is_ok(),
                "Expected BEGIN IMMEDIATE to succeed after IMMEDIATE tx rollback, but got: {:?}",
                result.unwrap_err()
            );
            // clean up: ROLLBACK を send
            let _ = conn_b.execute_batch("ROLLBACK");
        }
    }

    #[test]
    fn test_all_path_hashes_returns_all_rows() {
        let db = db_with_384();
        db.upsert_document("a.md", None, None, None, None, &[], None, "h_a", 0)
            .unwrap();
        db.upsert_document("b.md", None, None, None, None, &[], None, "h_b", 0)
            .unwrap();
        let map = db.all_path_hashes().unwrap();
        assert_eq!(map.len(), 2);
        assert_eq!(map.get("a.md"), Some(&"h_a".to_string()));
        assert_eq!(map.get("b.md"), Some(&"h_b".to_string()));
    }

    #[test]
    fn test_chunk_count_by_quality_splits_correctly() {
        let db = db_with_384();
        let doc_id = db
            .upsert_document("c.md", None, None, None, None, &[], None, "h", 0)
            .unwrap();
        db.insert_chunk(doc_id, 0, None, None, "x", None, &dummy_embedding(0.1), 0.9)
            .unwrap();
        db.insert_chunk(doc_id, 1, None, None, "y", None, &dummy_embedding(0.2), 0.1)
            .unwrap();
        let (above, below) = db.chunk_count_by_quality(0.5).unwrap();
        assert_eq!(above, 1);
        assert_eq!(below, 1);
    }

    #[test]
    fn test_chunks_for_path_returns_chunks_in_order() {
        let db = db_with_384();
        let doc_id = db
            .upsert_document(
                "deep-dive/mcp/overview.md",
                Some("MCP Overview"),
                Some("mcp"),
                Some("deep-dive"),
                Some("1"),
                &[],
                Some("2026-04-16"),
                "h1",
                0,
            )
            .unwrap();
        db.insert_chunk(
            doc_id,
            0,
            Some("Intro"),
            None,
            "hello",
            None,
            &dummy_embedding(0.1),
            1.0,
        )
        .unwrap();
        db.insert_chunk(
            doc_id,
            1,
            Some("Body"),
            None,
            "world",
            None,
            &dummy_embedding(0.2),
            1.0,
        )
        .unwrap();

        let out = db.chunks_for_path("deep-dive/mcp/overview.md").unwrap();
        assert_eq!(out.len(), 2);
        // chunk_index 順に返る
        assert_eq!(out[0].2.heading.as_deref(), Some("Intro"));
        assert_eq!(out[1].2.heading.as_deref(), Some("Body"));
        assert_eq!(out[0].1.len(), 384, "embedding dim must match");
        // 0.1 と 0.2 のはずだが、vec0 の f32 丸めがあるので許容誤差で比較。
        assert!((out[0].1[0] - 0.1).abs() < 1e-5);
        assert!((out[1].1[0] - 0.2).abs() < 1e-5);
        // seed node なので score は 1.0
        assert_eq!(out[0].2.score, 1.0);
        assert_eq!(out[0].2.path, "deep-dive/mcp/overview.md");
    }

    #[test]
    fn test_chunks_for_path_missing_returns_empty() {
        let db = db_with_384();
        let out = db.chunks_for_path("does/not/exist.md").unwrap();
        assert!(out.is_empty());
    }

    /// (BU-33) The seed read is bounded in SQL, and the caller learns whether
    /// anything was left behind without issuing a second query.
    #[test]
    fn the_capped_seed_read_reports_whether_more_chunks_exist() {
        let db = db_with_384();
        let doc_id = db
            .upsert_document("big.md", None, None, None, None, &[], None, "h1", 0)
            .unwrap();
        for i in 0..10 {
            db.insert_chunk(
                doc_id,
                i,
                Some(&format!("h{i}")),
                None,
                "body",
                None,
                &dummy_embedding(0.1 + 0.01 * i as f32),
                1.0,
            )
            .unwrap();
        }

        let (rows, more) = db.chunks_for_path_capped("big.md", 4).unwrap();
        assert_eq!(rows.len(), 4, "the cap must bound the rows that come back");
        assert!(more, "6 chunks were left behind");
        // The cap keeps chunk_index order, so it is the document's prefix.
        assert_eq!(rows[0].2.heading.as_deref(), Some("h0"));
        assert_eq!(rows[3].2.heading.as_deref(), Some("h3"));

        // cap == chunk count: everything fits, nothing is left behind.
        let (rows, more) = db.chunks_for_path_capped("big.md", 10).unwrap();
        assert_eq!(rows.len(), 10);
        assert!(
            !more,
            "a cap equal to the chunk count leaves nothing behind"
        );

        // cap above the chunk count behaves the same way.
        let (rows, more) = db.chunks_for_path_capped("big.md", 50).unwrap();
        assert_eq!(rows.len(), 10);
        assert!(!more);

        let (rows, more) = db.chunks_for_path_capped("does/not/exist.md", 4).unwrap();
        assert!(rows.is_empty());
        assert!(!more);
    }

    /// (BU-33) A `LIMIT` only bounds the *returned* rows. Without an index on
    /// `(document_id, chunk_index)` SQLite still scans every chunk and sorts
    /// the matches before taking the first `cap + 1`, so the seed read stays
    /// proportional to the whole knowledge base — measured at 8.00 ms vs
    /// 0.22 ms on 9,419 chunks. Assert the plan, not the clock: the clock is
    /// machine-dependent, the plan is the property that makes the bound real.
    #[test]
    fn the_capped_seed_read_is_index_backed_not_a_table_scan() {
        let db = db_with_384();
        let doc_id = db
            .upsert_document("d.md", None, None, None, None, &[], None, "h1", 0)
            .unwrap();
        for i in 0..5 {
            db.insert_chunk(
                doc_id,
                i,
                None,
                None,
                "body",
                None,
                &dummy_embedding(0.1),
                1.0,
            )
            .unwrap();
        }

        let plan: Vec<String> = db
            .conn
            .prepare(
                "EXPLAIN QUERY PLAN
                 SELECT c.id, c.content, c.heading, c.document_id,
                        d.path, d.title, d.topic, d.date, d.tags
                 FROM chunks c
                 JOIN documents d ON d.id = c.document_id
                 WHERE d.path = ?1
                 ORDER BY c.chunk_index
                 LIMIT ?2",
            )
            .unwrap()
            .query_map(params!["d.md", 4i64], |row| row.get::<_, String>(3))
            .unwrap()
            .filter_map(std::result::Result::ok)
            .collect();
        let plan = plan.join(" | ");

        assert!(
            plan.contains("idx_chunks_doc_order"),
            "the seed read must be index-backed, got: {plan}"
        );
        assert!(
            !plan.contains("SCAN c"),
            "a full scan of chunks makes the cap cost-free only in appearance: {plan}"
        );
        assert!(
            !plan.contains("TEMP B-TREE"),
            "sorting every match before the LIMIT is the work the cap should avoid: {plan}"
        );
    }

    #[test]
    fn test_get_chunk_embedding_roundtrip() {
        let db = db_with_384();
        let doc_id = db
            .upsert_document("a.md", None, None, None, None, &[], None, "h1", 0)
            .unwrap();
        db.insert_chunk(doc_id, 0, None, None, "x", None, &dummy_embedding(0.3), 1.0)
            .unwrap();

        let chunk_id: i64 = db
            .conn
            .query_row(
                "SELECT id FROM chunks WHERE document_id = ?1",
                params![doc_id],
                |row| row.get(0),
            )
            .unwrap();

        let emb = db
            .get_chunk_embedding(chunk_id)
            .unwrap()
            .expect("must exist");
        assert_eq!(emb.len(), 384);
        assert!((emb[0] - 0.3).abs() < 1e-5);

        // 存在しない chunk_id は None
        assert!(db.get_chunk_embedding(99_999).unwrap().is_none());
    }

    #[test]
    fn test_fetch_embeddings_by_chunk_ids_returns_hashmap() {
        let db = db_with_384();
        let doc1 = db
            .upsert_document("a.md", None, None, None, None, &[], None, "h_a", 0)
            .unwrap();
        let c1 = db
            .insert_chunk(
                doc1,
                0,
                Some("h1"),
                None,
                "alpha",
                None,
                &dummy_embedding(0.1),
                1.0,
            )
            .unwrap();
        let c2 = db
            .insert_chunk(
                doc1,
                1,
                Some("h2"),
                None,
                "beta",
                None,
                &dummy_embedding(0.2),
                1.0,
            )
            .unwrap();
        let doc2 = db
            .upsert_document("b.md", None, None, None, None, &[], None, "h_b", 0)
            .unwrap();
        let c3 = db
            .insert_chunk(
                doc2,
                0,
                Some("h3"),
                None,
                "gamma",
                None,
                &dummy_embedding(0.3),
                1.0,
            )
            .unwrap();

        let ids = vec![c1, c2, c3];
        let result = db.fetch_embeddings_by_chunk_ids(&ids).expect("fetch");
        assert_eq!(result.len(), 3);
        assert!(result.contains_key(&c1));
        assert!(result.contains_key(&c2));
        assert!(result.contains_key(&c3));

        // 各 embedding が 384 次元
        for emb in result.values() {
            assert_eq!(emb.len(), 384);
        }

        // Sanity: 値が正しく往復していること (insert 時の dummy_embedding 値に一致)
        assert!((result[&c1][0] - 0.1).abs() < 1e-5);
        assert!((result[&c2][0] - 0.2).abs() < 1e-5);
        assert!((result[&c3][0] - 0.3).abs() < 1e-5);
    }

    #[test]
    fn test_fetch_embeddings_by_chunk_ids_skips_missing() {
        let db = db_with_384();
        let doc1 = db
            .upsert_document("a.md", None, None, None, None, &[], None, "h_a", 0)
            .unwrap();
        let c1 = db
            .insert_chunk(
                doc1,
                0,
                Some("h1"),
                None,
                "alpha",
                None,
                &dummy_embedding(0.1),
                1.0,
            )
            .unwrap();

        let ids = vec![c1, 9999, 10000];
        let result = db.fetch_embeddings_by_chunk_ids(&ids).expect("fetch");
        assert_eq!(result.len(), 1, "missing ids should be silently skipped");
        assert!(result.contains_key(&c1));
    }

    #[test]
    fn test_fetch_embeddings_by_chunk_ids_empty_input() {
        let db = db_with_384();
        let result = db.fetch_embeddings_by_chunk_ids(&[]).expect("fetch");
        assert!(result.is_empty());
    }

    #[test]
    fn test_fetch_embeddings_by_chunk_ids_batches_above_sqlite_limit() {
        // SQLITE_MAX_VARIABLE_NUMBER (32766) を超える chunk_ids でも batch
        // 分割で正常動作することを確認 (codex review #5 の regression guard)。
        // 600 chunks (= EMBEDDING_FETCH_BATCH=500 を 1 batch 超える) を作る。
        let db = db_with_384();
        let doc_id = db
            .upsert_document(
                "/big.md",
                Some("big"),
                Some("topic"),
                None,
                None,
                &[],
                None,
                "h",
                0,
            )
            .expect("upsert");
        let n = 600;
        let mut ids = Vec::with_capacity(n);
        for i in 0..n {
            let cid = db
                .insert_chunk(
                    doc_id,
                    i as i32,
                    Some("h"),
                    None,
                    "c",
                    None,
                    &dummy_embedding((i as f32) * 0.001),
                    1.0,
                )
                .expect("insert");
            ids.push(cid);
        }
        let result = db.fetch_embeddings_by_chunk_ids(&ids).expect("fetch");
        assert_eq!(
            result.len(),
            n,
            "all {n} embeddings should be returned across batches"
        );
        for &id in &ids {
            assert!(
                result.contains_key(&id),
                "chunk_id {id} missing from batched fetch"
            );
        }
    }

    #[test]
    fn test_fetch_embeddings_by_chunk_ids_boundary_table() {
        // codex 罠 5 (SQLite IN limit) cluster の 2 件目防御。
        // EMBEDDING_FETCH_BATCH を境界とする 5 値を直接 round-trip 検証。
        // 値を `EMBEDDING_FETCH_BATCH` 定数に bind することで、
        // 将来 batch サイズを変えた時に boundary 値が連動するよう保証する
        // (= マジックナンバー 499/500/501/1500 を直書きしない)。
        // 3 * batch (現在 1500) で batch 跨ぎ + 複数 batch 連結を検証
        // (`32766` SQLite default MAX_VARIABLE_NUMBER 直前は CI cost が見合わないため out-of-scope)。
        let efb = EMBEDDING_FETCH_BATCH;
        for &n in &[0_usize, efb - 1, efb, efb + 1, 3 * efb] {
            let db = db_with_384();
            let doc_id = db
                .upsert_document(
                    "/big.md",
                    Some("big"),
                    Some("topic"),
                    None,
                    None,
                    &[],
                    None,
                    "h",
                    0,
                )
                .expect("upsert");
            let mut ids = Vec::with_capacity(n);
            for i in 0..n {
                let cid = db
                    .insert_chunk(
                        doc_id,
                        i as i32,
                        Some("h"),
                        None,
                        "c",
                        None,
                        &dummy_embedding((i as f32) * 0.001),
                        1.0,
                    )
                    .expect("insert");
                ids.push(cid);
            }
            let result = db.fetch_embeddings_by_chunk_ids(&ids).expect("fetch");
            assert_eq!(result.len(), n, "round-trip count mismatch for n={n}");
            for &id in &ids {
                assert!(result.contains_key(&id), "chunk_id {id} missing for n={n}");
            }
        }
    }

    proptest::proptest! {
        // proptest で 0..=200 の任意 N を sweep、round-trip 完全一致を assert。
        // PROPTEST_CASES = 64 で IO-heavy test の cost を抑制。
        #![proptest_config(proptest::test_runner::Config {
            cases: 64,
            ..proptest::test_runner::Config::default()
        })]

        #[test]
        fn prop_fetch_embeddings_by_chunk_ids_round_trip(n in 0_usize..=200) {
            let db = db_with_384();
            let doc_id = db
                .upsert_document("/big.md", Some("big"), Some("topic"), None, None, &[], None, "h", 0)
                .expect("upsert");
            let mut ids = Vec::with_capacity(n);
            for i in 0..n {
                let cid = db
                    .insert_chunk(
                        doc_id,
                        i as i32,
                        Some("h"),
                        None,
                        "c", None,
                        &dummy_embedding((i as f32) * 0.001),
                        1.0,
                    )
                    .expect("insert");
                ids.push(cid);
            }
            let result = db.fetch_embeddings_by_chunk_ids(&ids).expect("fetch");
            proptest::prop_assert_eq!(result.len(), n);
            for &id in &ids {
                proptest::prop_assert!(result.contains_key(&id), "chunk_id {id} missing");
            }
        }
    }

    #[test]
    fn test_fts_table_created_on_init() {
        let db = Database::open_in_memory().unwrap();
        let name: String = db
            .conn
            .query_row(
                "SELECT name FROM sqlite_master WHERE type='table' AND name='fts_chunks'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(name, "fts_chunks");
    }

    // feature-48: `test_sanitize_fts_query` はここにあった。旧 FTS クエリ加工契約
    // (クエリ全体を単一 phrase 化) を pin するテストであり、その契約の変更自体が
    // feature-48 の目的なので、同等以上の網羅を新契約で行う分割表テストを
    // `db/fts_query.rs` に置いた上で削除した。旧テストの 6 ケースのうち挙動が
    // 変わらない 3 つ (空 / 空白のみ / `エラー`) は
    // `old_contract_cases_that_must_not_change` が、残り 3 つ (`E0382` /
    // `foo "bar" AND` / `ab`) は分割表テストが引き継いでいる。

    // ---------------------------------------------------------------------
    // feature-48: クエリの token 化が DB 経路で効いていることの確認。
    // fts_query.rs の unit test は文字列しか見ないので、生成した式が実際に
    // SQLite に受理され、意図した chunk を引くことはここでしか確かめられない。
    // ---------------------------------------------------------------------

    /// feature-48 用の 1 doc 1 chunk ヘルパ (`tune.rs` の `add_doc` と同型)。
    fn add_fts_doc(db: &Database, path: &str, heading: &str, content: &str, e: f32) {
        let doc = db
            .upsert_document(path, Some(path), None, None, None, &[], None, path, 0)
            .unwrap();
        db.insert_chunk(
            doc,
            0,
            Some(heading),
            None,
            content,
            None,
            &vec![e; 384],
            1.0,
        )
        .unwrap();
    }

    #[test]
    fn a_japanese_natural_language_query_now_reaches_fts() {
        let db = db_with_384();
        add_fts_doc(
            &db,
            "a.md",
            "再ランキング",
            "再ランキングの評価をやり直す手順をここにまとめる。",
            0.1,
        );
        add_fts_doc(&db, "b.md", "無関係", "quokka husbandry notes", 0.2);

        // クエリ全体は 1 件にも逐語で出現しない。旧実装ではここが 0 件だった。
        let hits = db
            .search_fts_candidates(
                "再ランキングの評価について",
                10,
                &SearchFilters::default(),
                FusionParams::default(),
            )
            .unwrap();
        assert_eq!(hits.len(), 1, "token 化した FTS が本文を引けること");
        assert_eq!(hits[0].1.path, "a.md");

        // 同じ fixture でも、全体を quote すれば旧挙動 (逐語) に戻る = 逃げ道が効く。
        let verbatim = db
            .search_fts_candidates(
                "\"再ランキングの評価について\"",
                10,
                &SearchFilters::default(),
                FusionParams::default(),
            )
            .unwrap();
        assert!(
            verbatim.is_empty(),
            "quote で囲めば逐語検索のまま = 旧挙動を再現できる"
        );
    }

    #[test]
    fn fts_candidates_union_the_tokens_rather_than_intersecting_them() {
        let db = db_with_384();
        add_fts_doc(&db, "a.md", "A", "zebrafish larvae in assays", 0.1);
        add_fts_doc(&db, "b.md", "B", "completely unrelated prose here", 0.2);

        // どちらの chunk も両方の語は持たない。AND なら 0 件になる。
        let hits = db
            .search_fts_candidates(
                "zebrafish prose",
                10,
                &SearchFilters::default(),
                FusionParams::default(),
            )
            .unwrap();
        assert_eq!(hits.len(), 2, "token は OR で結ばれる (AND なら 0 件)");
    }

    #[test]
    fn fts_call_is_counted_even_when_the_query_compiles_to_nothing() {
        // AU-22 の往復会計は「呼んだ回数」で数える。`build_fts_query` が None を
        // 返すクエリも 1 往復として数える現行の並び順を pin する。
        let db = db_with_384();
        add_fts_doc(&db, "a.md", "A", "anything", 0.1);
        FTS_CANDIDATE_CALLS.with(|c| c.set(0));
        let _ = db
            .search_fts_candidates("ab", 10, &SearchFilters::default(), FusionParams::default())
            .unwrap();
        assert_eq!(FTS_CANDIDATE_CALLS.with(|c| c.get()), 1);
    }

    #[test]
    fn every_generated_expression_is_accepted_by_fts5() {
        // 文字列比較テストは「生成式が SQLite に受理されるか」を 1 度も見ていない。
        // escape 崩れ / NUL / 制御文字はここでしか捕まらない。
        let db = db_with_384();
        let inputs = [
            "再ランキングの評価について",
            "retry budget の設定",
            "\"Foundry Local\" の設定",
            "\"再ランキングの評価について\"",
            "E0382",
            "暗号化",
            "評価は",
            "システム化",
            "\"ab\" テスト",
            "ab",
            "",
            "   ",
            "エラー",
            "foo \"bar\" AND",
            "\"say \"\"hi\"\"\"",
            "\"ab\"\"cd\"",
            "\"\"\"\"",
            "abc \"def",
            "\"\" テスト",
            "abc abc def",
            "ランキング 再ランキング",
            "!!! ??? 、。",
            "sqlite-vec",
            "kb_mcp",
            "ＡＢＣ検索",
            "한국어 Привет",
            "評価・分析",
            "クロス・エンコーダ",
            "再ランキング化",
            "の評価は",
            "abcｶﾀｶﾅ",
            "サーバー",
            "代々木",
            "あ亜ア",
            "再 ランキング",
            "AI と ML",
            "heading:foo",
            "foo OR bar",
            "再\"ランキング\"",
            "\"ランキング\"化",
            "sagashiro-embed-r4 の",
            "の再ランキング",
            "\"ランキング\"",
            "---",
            "評価\u{FF65}分析",
            "\"a\0bc\"",
            "NEAR(a b) * ^x",
            "emoji 🦀 query",
            // proptest が縮小して見つけた形。NUL を挟んだ 2 群 + `¹` (U+00B9、
            // Unicode カテゴリ No なので `is_alphanumeric()` が true = 語構成文字)。
            // 明示的な入力として残しておく。
            "A0\u{00B9}\0Aa\u{00B9}",
        ];
        for raw in inputs {
            let hits = db.search_fts_candidates(
                raw,
                10,
                &SearchFilters::default(),
                FusionParams::default(),
            );
            assert!(
                hits.is_ok(),
                "FTS5 rejected the expression built from {raw:?}: {:?}",
                hits.err()
            );
        }
    }

    /// full-audit 2026-08-12 テスト軸 C-1: 上の `every_generated_expression_is_accepted_by_fts5`
    /// は 50 件の固定入力で、その中の `A0¹\0Aa¹` は「proptest が縮小して見つけた形を
    /// 凍結したもの」= generator は開発中に使って捨てられていた。
    ///
    /// `fts_query.rs` の property 3 本は Rust 側の不変条件しか見ないので、
    /// **生成した式が SQLite に受理されるか**は誰も生成器で試していない。escape 規約や
    /// 文字クラスを将来いじった時、固定リストに無い形が syntax error を起こすと
    /// `search_fts_candidates` が `Err` になり、**検索全体が落ちる**。
    /// in-memory DB なのでモデル DL は不要 = 既定の `cargo test` に載る。
    #[test]
    fn generated_expressions_stay_valid_fts5_for_arbitrary_queries() {
        let db = db_with_384();
        add_fts_doc(&db, "a.md", "A", "anything at all", 0.1);

        let mut runner = proptest::test_runner::TestRunner::new(proptest::test_runner::Config {
            cases: 256,
            failure_persistence: None,
            ..proptest::test_runner::Config::default()
        });
        runner
            .run(
                &proptest::string::string_regex(".{0,120}").unwrap(),
                |raw| {
                    let hits = db.search_fts_candidates(
                        &raw,
                        10,
                        &SearchFilters::default(),
                        FusionParams::default(),
                    );
                    proptest::prop_assert!(
                        hits.is_ok(),
                        "FTS5 rejected the expression built from {raw:?}: {:?}",
                        hits.err()
                    );
                    Ok(())
                },
            )
            .unwrap();
    }

    #[test]
    fn test_parse_dim_from_create_sql() {
        let sql = "CREATE VIRTUAL TABLE vec_chunks USING vec0(\
                   chunk_id INTEGER PRIMARY KEY, embedding float[1024])";
        assert_eq!(parse_dim_from_create_sql(sql), Some(1024));

        let sql2 = "CREATE VIRTUAL TABLE vec_chunks USING vec0(chunk_id, embedding float[384] )";
        assert_eq!(parse_dim_from_create_sql(sql2), Some(384));

        assert_eq!(parse_dim_from_create_sql("no float here"), None);
    }

    #[test]
    fn test_init_does_not_create_vec_chunks_without_meta() {
        let db = Database::open_in_memory().unwrap();
        assert_eq!(db.current_vec_dim().unwrap(), None);
    }

    #[test]
    fn test_verify_creates_vec_chunks_with_declared_dim() {
        let db = Database::open_in_memory().unwrap();
        db.verify_embedding_meta("bge-m3", 1024).unwrap();
        assert_eq!(db.current_vec_dim().unwrap(), Some(1024));

        // 1024-dim embedding を insert できることを確認
        let doc_id = db
            .upsert_document("x.md", Some("x"), None, None, None, &[], None, "h", 0)
            .unwrap();
        let emb: Vec<f32> = vec![0.1; 1024];
        db.insert_chunk(doc_id, 0, None, None, "hi", None, &emb, 1.0)
            .unwrap();
        assert_eq!(db.chunk_count().unwrap(), 1);
    }

    #[test]
    fn test_ensure_vec_chunks_rejects_mismatched_dim() {
        let db = Database::open_in_memory().unwrap();
        db.ensure_vec_chunks_table(384).unwrap();
        let err = db.ensure_vec_chunks_table(1024).expect_err("must reject");
        assert!(err.to_string().contains("float[384]"));
    }

    /// (BU-28) The reason `MIN_PHRASE_CHARS` is 3, asserted against the
    /// tokenizer rather than against the constant.
    ///
    /// `fts_query.rs` drops phrases shorter than three characters because the
    /// trigram tokenizer cannot match them — such a phrase is not an error, it
    /// simply returns nothing, forever. Every test in that module encodes that
    /// floor, and all of them would keep passing if the tokenizer were swapped
    /// for one with a different floor: they would agree with each other while
    /// silently disagreeing with SQLite.
    ///
    /// So this asks the tokenizer directly. If it ever starts matching a
    /// two-character phrase, this fails and `MIN_PHRASE_CHARS` is the thing to
    /// revisit — not this test.
    #[test]
    fn the_trigram_tokenizer_is_why_short_phrases_are_dropped() {
        let db = db_with_384();
        db.conn
            .execute(
                "INSERT INTO fts_chunks(rowid, heading, context, content) VALUES (1, '', '', ?1)",
                ["alpha beta gamma"],
            )
            .unwrap();

        let matches = |phrase: &str| -> u32 {
            db.conn
                .query_row(
                    "SELECT COUNT(*) FROM fts_chunks WHERE fts_chunks MATCH ?1",
                    [format!("\"{phrase}\"")],
                    |r| r.get(0),
                )
                .unwrap_or_else(|e| panic!("MATCH {phrase:?} failed: {e}"))
        };

        // Three characters and up: the tokenizer can see them.
        for phrase in ["alp", "alph", "alpha", "eta gam"] {
            assert_eq!(
                matches(phrase),
                1,
                "{phrase:?} is at least 3 characters and occurs in the content, \
                 so the trigram tokenizer must match it"
            );
        }

        // Below three: silently empty. Not an error — which is exactly why the
        // query compiler has to drop these itself.
        for phrase in ["a", "al", "ph"] {
            assert_eq!(
                matches(phrase),
                0,
                "{phrase:?} is below the trigram floor, so it must match nothing \
                 even though it is a substring of the content. If this now \
                 returns 1, the tokenizer changed and MIN_PHRASE_CHARS in \
                 fts_query.rs no longer has a reason to be 3"
            );
        }
    }

    /// Helper: FTS row count (contentless でも COUNT は通る)
    fn fts_count(db: &Database) -> u32 {
        db.conn
            .query_row("SELECT COUNT(*) FROM fts_chunks", [], |r| r.get(0))
            .unwrap()
    }

    #[test]
    fn test_insert_chunk_populates_fts() {
        let db = db_with_384();
        let doc_id = db
            .upsert_document("a.md", Some("a"), None, None, None, &[], None, "h", 0)
            .unwrap();
        let chunk_id = db
            .insert_chunk(
                doc_id,
                0,
                Some("Intro"),
                None,
                "hello world",
                None,
                &dummy_embedding(0.1),
                1.0,
            )
            .unwrap();
        assert_eq!(fts_count(&db), 1);

        // rowid が chunks.id と一致
        let fts_rowid: i64 = db
            .conn
            .query_row("SELECT rowid FROM fts_chunks", [], |r| r.get(0))
            .unwrap();
        assert_eq!(fts_rowid, chunk_id);
    }

    #[test]
    fn test_delete_document_cascades_to_fts() {
        let db = db_with_384();
        let doc_id = db
            .upsert_document("a.md", Some("a"), None, None, None, &[], None, "h", 0)
            .unwrap();
        db.insert_chunk(
            doc_id,
            0,
            None,
            None,
            "hi",
            None,
            &dummy_embedding(0.1),
            1.0,
        )
        .unwrap();
        assert_eq!(fts_count(&db), 1);

        db.delete_document("a.md").unwrap();
        assert_eq!(fts_count(&db), 0);
    }

    #[test]
    fn test_upsert_document_purges_old_fts() {
        let db = db_with_384();
        let doc_id = db
            .upsert_document("a.md", Some("a"), None, None, None, &[], None, "h1", 0)
            .unwrap();
        db.insert_chunk(
            doc_id,
            0,
            None,
            None,
            "old content",
            None,
            &dummy_embedding(0.1),
            1.0,
        )
        .unwrap();
        assert_eq!(fts_count(&db), 1);

        // 同一 path を異なる content_hash で再 upsert → 旧 chunk/FTS は消える
        db.upsert_document("a.md", Some("a"), None, None, None, &[], None, "h2", 0)
            .unwrap();
        assert_eq!(fts_count(&db), 0);
    }

    #[test]
    fn test_search_hybrid_fts_exact_match_ranks_higher() {
        let db = db_with_384();
        let doc_id = db
            .upsert_document("doc.md", Some("doc"), None, None, None, &[], None, "h", 0)
            .unwrap();
        // chunk A: 完全一致語 E0382 を含む。埋め込みはクエリから等距離
        let a_id = db
            .insert_chunk(
                doc_id,
                0,
                Some("Errors"),
                None,
                "E0382 is a move error",
                None,
                &dummy_embedding(0.5),
                1.0,
            )
            .unwrap();
        // chunk B: 完全一致語を含まない。埋め込みはクエリから等距離
        let b_id = db
            .insert_chunk(
                doc_id,
                1,
                Some("Other"),
                None,
                "unrelated content here",
                None,
                &dummy_embedding(0.5),
                1.0,
            )
            .unwrap();

        let hits = db
            .search_hybrid(
                "E0382",
                &dummy_embedding(0.5),
                5,
                &SearchFilters::default(),
                FusionParams::default(),
            )
            .unwrap();
        assert_eq!(hits.len(), 2);
        // FTS でヒットするのは A だけ → A が上位
        assert!(
            hits[0].content.contains("E0382"),
            "got: {:?}",
            hits[0].content
        );
        let _ = (a_id, b_id);
    }

    #[test]
    fn test_search_hybrid_falls_back_when_fts_query_empty() {
        let db = db_with_384();
        let doc_id = db
            .upsert_document("a.md", Some("a"), None, None, None, &[], None, "h", 0)
            .unwrap();
        db.insert_chunk(
            doc_id,
            0,
            None,
            None,
            "content",
            None,
            &dummy_embedding(0.1),
            1.0,
        )
        .unwrap();

        // 2 文字クエリ → sanitize が None → vec-only
        let hits = db
            .search_hybrid(
                "ab",
                &dummy_embedding(0.1),
                5,
                &SearchFilters::default(),
                FusionParams::default(),
            )
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert!(hits[0].score > 0.0, "RRF スコアは正の有限値");
    }

    #[test]
    fn test_search_hybrid_candidates_returns_chunk_ids() {
        let db = db_with_384();
        let doc_id = db
            .upsert_document("a.md", Some("a"), None, None, None, &[], None, "h", 0)
            .unwrap();
        let c1 = db
            .insert_chunk(
                doc_id,
                0,
                None,
                None,
                "E0382 moved value",
                None,
                &dummy_embedding(0.1),
                1.0,
            )
            .unwrap();
        let c2 = db
            .insert_chunk(
                doc_id,
                1,
                None,
                None,
                "unrelated note",
                None,
                &dummy_embedding(0.9),
                1.0,
            )
            .unwrap();

        let hits = db
            .search_hybrid_candidates(
                "E0382",
                &dummy_embedding(0.1),
                5,
                &SearchFilters::default(),
                FusionParams::default(),
            )
            .unwrap();
        assert!(!hits.is_empty());
        // 返ってきた chunk_id は insert 時の id と一致
        let ids: Vec<i64> = hits.iter().map(|(id, _)| *id).collect();
        assert!(ids.contains(&c1) || ids.contains(&c2));
    }

    #[test]
    fn test_search_hybrid_candidates_unbounded_returns_full_pool() {
        // 5+ docs / 多 chunks の小さな KB を作り、`_unbounded` が
        // bounded (limit=2) より多くの候補を返すことを確認する。
        // MMR の候補プール用 API なので truncate されないのが要件。
        let db = db_with_384();

        let mut chunk_ids: Vec<i64> = Vec::new();
        for i in 0..5 {
            let path = format!("doc_{i}.md");
            let doc_id = db
                .upsert_document(
                    &path,
                    Some("d"),
                    None,
                    None,
                    None,
                    &[],
                    None,
                    &format!("h_{i}"),
                    0,
                )
                .unwrap();
            // chunk 1: keyword を含む (FTS hit)
            let c_a = db
                .insert_chunk(
                    doc_id,
                    0,
                    None,
                    None,
                    &format!("alpha keyword text doc {i}"),
                    None,
                    &dummy_embedding(0.1 + (i as f32) * 0.01),
                    1.0,
                )
                .unwrap();
            chunk_ids.push(c_a);
            // 2 doc 目以降にもう 1 chunk 追加 → 合計 7+ chunks
            if i >= 2 {
                let c_b = db
                    .insert_chunk(
                        doc_id,
                        1,
                        None,
                        None,
                        &format!("secondary chunk content {i}"),
                        None,
                        &dummy_embedding(0.5 + (i as f32) * 0.01),
                        1.0,
                    )
                    .unwrap();
                chunk_ids.push(c_b);
            }
        }
        assert!(chunk_ids.len() >= 7, "fixture should have 7+ chunks");

        let query_emb = dummy_embedding(0.1);
        let query_text = "keyword";
        let filters = SearchFilters::default();

        let bounded = db
            .search_hybrid_candidates(query_text, &query_emb, 2, &filters, FusionParams::default())
            .unwrap();
        let unbounded = db
            .search_hybrid_candidates_unbounded(
                query_text,
                &query_emb,
                50,
                &filters,
                FusionParams::default(),
            )
            .unwrap();

        assert!(
            bounded.len() <= 2,
            "bounded must respect limit=2 (got {})",
            bounded.len()
        );
        assert!(
            unbounded.len() >= bounded.len(),
            "unbounded should return >= bounded: bounded={} unbounded={}",
            bounded.len(),
            unbounded.len()
        );
        // 候補プール全件: 上記 fixture では vec_chunks が 7+ 件あるので
        // unbounded は 2 件超を返すはず (limit 解除の差分が出ること)。
        assert!(
            unbounded.len() > bounded.len(),
            "unbounded should strictly exceed bounded with this fixture: \
             bounded={} unbounded={}",
            bounded.len(),
            unbounded.len()
        );
    }

    #[test]
    fn test_fts_bm25_heading_weighted_higher() {
        let db = db_with_384();
        let doc_id = db
            .upsert_document("a.md", Some("a"), None, None, None, &[], None, "h", 0)
            .unwrap();
        // chunk A: content に keyword。heading には無し
        let a_id = db
            .insert_chunk(
                doc_id,
                0,
                Some("Introduction"),
                None,
                "This paragraph contains the kibarashi_unique_keyword only in content text",
                None,
                &dummy_embedding(0.5),
                1.0,
            )
            .unwrap();
        // chunk B: heading に keyword。content にも軽く含む
        let b_id = db
            .insert_chunk(
                doc_id,
                1,
                Some("About kibarashi_unique_keyword"),
                None,
                "short body here.",
                None,
                &dummy_embedding(0.5),
                1.0,
            )
            .unwrap();

        // 直接 FTS 候補を取り、B が A より上位 (低 bm25) になることを確認
        let hits = db
            .search_fts_candidates(
                "kibarashi_unique_keyword",
                10,
                &SearchFilters::default(),
                FusionParams::default(),
            )
            .unwrap();
        assert_eq!(hits.len(), 2);
        let (top_id, _) = hits[0];
        assert_eq!(
            top_id, b_id,
            "heading hit (B) should rank higher than content-only hit (A). ids={a_id},{b_id}"
        );
    }

    #[test]
    fn test_search_hybrid_overfetches_when_filter_is_selective() {
        // filter で多数の候補が落ちるケース: BGE-small-en-v1.5 の 384 dim で
        // 20 ドキュメント挿入するが、category 一致は 1 件のみ。
        // limit=5 のとき、filter がなければ 5 件返るが、選択的な filter で
        // 1 件 しか残らない。over-fetch で target 側を 10 倍広げているため、
        // その 1 件を取りこぼさない。
        let db = db_with_384();
        for i in 0..20 {
            let path = format!("noise/doc_{i}.md");
            let cat = if i == 0 { "target" } else { "noise" };
            let doc_id = db
                .upsert_document(&path, Some("x"), None, Some(cat), None, &[], None, "h", 0)
                .unwrap();
            db.insert_chunk(
                doc_id,
                0,
                None,
                None,
                "content",
                None,
                &dummy_embedding(0.5),
                1.0,
            )
            .unwrap();
        }

        let hits = db
            .search_hybrid(
                "noexistent_query",
                &dummy_embedding(0.5),
                5,
                &SearchFilters {
                    category: Some("target"),
                    ..Default::default()
                },
                FusionParams::default(),
            )
            .unwrap();
        assert_eq!(hits.len(), 1, "target カテゴリの 1 件を取りこぼさない");
    }

    #[test]
    fn test_search_hybrid_japanese_trigram() {
        let db = db_with_384();
        let doc_id = db
            .upsert_document("ja.md", Some("ja"), None, None, None, &[], None, "h", 0)
            .unwrap();
        db.insert_chunk(
            doc_id,
            0,
            Some("見出し"),
            None,
            "E0382 は value moved エラーです",
            None,
            &dummy_embedding(0.7),
            1.0,
        )
        .unwrap();
        db.insert_chunk(
            doc_id,
            1,
            None,
            None,
            "unrelated",
            None,
            &dummy_embedding(0.9),
            1.0,
        )
        .unwrap();

        // 日本語 3 文字 "エラー" が trigram でヒットする
        let hits = db
            .search_hybrid(
                "エラー",
                &dummy_embedding(0.7),
                5,
                &SearchFilters::default(),
                FusionParams::default(),
            )
            .unwrap();
        assert!(!hits.is_empty());
        assert!(
            hits.iter().any(|h| h.content.contains("エラー")),
            "Japanese trigram should hit"
        );
    }

    /// (BU-04) 融合後の順位で FTS 側の寄与を見る。
    ///
    /// 既存の hybrid テストは、FTS が当たる chunk に**クエリ埋め込みと同じ
    /// ベクトル**を持たせていた。つまり FTS が完全に死んでいてもベクトル
    /// だけで 1 位になり、緑のまま通る。feature-48 が直した「日本語自然文で
    /// FTS 候補が 0 件」という欠陥が 15 リリース生き延びたのはこれが理由。
    ///
    /// ここでは配置を逆にする: **FTS が当たる側をベクトル的に遠く**し、
    /// 当たらない decoy をクエリ埋め込みと完全一致させる。ベクトルだけなら
    /// decoy が 1 位 (RRF で 1/61)。FTS が生きていれば target は
    /// 1/62 + 1/61 で decoy を上回る。**FTS の寄与が消えた瞬間に順位が入れ替わる。**
    #[test]
    fn fts_decides_the_top_rank_when_the_vector_leg_prefers_another_chunk() {
        let db = db_with_384();
        let doc_id = db
            .upsert_document("ja.md", Some("ja"), None, None, None, &[], None, "h", 0)
            .unwrap();
        // FTS で当たる側。ベクトルはクエリから遠い。
        db.insert_chunk(
            doc_id,
            0,
            None,
            None,
            "再ランキングの評価について測定した記録",
            None,
            &dummy_embedding(0.9),
            1.0,
        )
        .unwrap();
        // FTS では当たらない側。ベクトルはクエリと完全一致 = 単独なら 1 位。
        // クエリが生む phrase (再ランキング / ランキング / の評価 / について)
        // と trigram を共有しない語を選んでいる。
        db.insert_chunk(
            doc_id,
            1,
            None,
            None,
            "犬と猫が公園を走った",
            None,
            &dummy_embedding(0.5),
            1.0,
        )
        .unwrap();

        let hits = db
            .search_hybrid(
                "再ランキングの評価について",
                &dummy_embedding(0.5),
                5,
                &SearchFilters::default(),
                FusionParams::default(),
            )
            .unwrap();

        assert_eq!(hits.len(), 2, "both chunks should survive fusion");
        assert!(
            hits[0].content.contains("再ランキング"),
            "the FTS-matching chunk must outrank the vector-nearest decoy, \
             otherwise the full-text half is contributing nothing: got {:?}",
            hits.iter().map(|h| h.content.as_str()).collect::<Vec<_>>()
        );
    }

    /// 32 個の一般的な断片からなるクエリを組み立てる (BU-03 の想定攻撃形)。
    /// 空白区切りなので 1 語 = 1 群 = 1 phrase になり、`MAX_PHRASES` (32) 上限
    /// ちょうどの OR 式ができる。
    fn pathological_or_query() -> String {
        pathological_fragments().join(" ")
    }

    fn pathological_fragments() -> [&'static str; 32] {
        [
            "について",
            "における",
            "によって",
            "したがって",
            "ただし",
            "および",
            "または",
            "ならびに",
            "もしくは",
            "さらに",
            "しかし",
            "つまり",
            "すなわち",
            "たとえば",
            "ところで",
            "ちなみに",
            "おそらく",
            "たしかに",
            "とりわけ",
            "まったく",
            "ほとんど",
            "すべて",
            "いくつか",
            "それぞれ",
            "これら",
            "それら",
            "ただちに",
            "あるいは",
            "ゆえに",
            "むしろ",
            "とはいえ",
            "しかも",
        ]
    }

    /// (BU-03) OR 展開は候補集合の **和** であって積ではなく、phrase 数に
    /// 関わらず FTS への問い合わせは **1 文**である。
    ///
    /// 監査は「32 個の OR で照合母集団が爆発する」ことを懸念していた。実測
    /// (下の `#[ignore]` ガードのコメント参照) では母集団の上限は corpus 全体で、
    /// これは **feature-48 以前から 1 個の一般的な部分文字列で到達できた**。
    /// ここではその構造 — 和集合であること、1 クエリ 1 文であること — を固定する。
    /// これが崩れる (phrase ごとに 1 文発行する等) と初めて次数が変わる。
    #[test]
    fn fts_or_expansion_is_one_statement_over_the_union_of_its_phrases() {
        let db = db_with_384();
        let doc_id = db
            .upsert_document("u.md", Some("u"), None, None, None, &[], None, "h", 0)
            .unwrap();
        // 前半 10 行だけが「について」、後半 10 行だけが「における」を持つ =
        // 2 つの phrase の集合は交わらない。OR が和なら 20、積なら 0 になる。
        for i in 0..20 {
            let body = if i < 10 {
                format!("この文書について記した第 {i} 節")
            } else {
                format!("この文書における記述の第 {i} 節")
            };
            db.insert_chunk(
                doc_id,
                i,
                None,
                None,
                &body,
                None,
                &dummy_embedding(0.5),
                1.0,
            )
            .unwrap();
        }

        assert_eq!(db.count_fts_matches("について").unwrap(), 10);
        assert_eq!(db.count_fts_matches("における").unwrap(), 10);
        assert_eq!(
            db.count_fts_matches("について における").unwrap(),
            20,
            "OR must be a union of the per-phrase candidate sets, not an \
             intersection: the two halves share no row"
        );

        // 実際に発行された SQL を数える。`FTS_CANDIDATE_CALLS` は
        // `search_fts_candidates` の**入口**で 1 増えるだけなので、中で phrase ごとに
        // 1 文発行するようになっても 1 のままになる = この不変条件は測れない
        // (codex review P2、PR #136)。rusqlite の `trace` を dev-dependency 側だけで
        // 有効にして、MATCH を含む文の数を直接数える。
        TRACED_SQL.with(|v| v.borrow_mut().clear());
        db.conn.trace_v2(
            rusqlite::trace::TraceEventCodes::SQLITE_TRACE_STMT,
            Some(record_traced_sql),
        );
        let _ = db
            .search_fts_candidates(
                &pathological_or_query(),
                10,
                &SearchFilters::default(),
                FusionParams::default(),
            )
            .unwrap();
        db.conn
            .trace_v2(rusqlite::trace::TraceEventCodes::empty(), None);
        // SQLite は FTS5 の内部副問い合わせも trace に出すが、それらは `--` 前置
        // (subprogram 表記) で MATCH を含まないので、MATCH を含む文を数えれば
        // 呼び出し側が発行した本数と一致する。診断出力も同じ絞り込みを使う
        // (絞らないと内部副問い合わせが数百行出て読めない)。
        let match_statements: Vec<String> = TRACED_SQL.with(|v| {
            v.borrow()
                .iter()
                .filter(|sql| sql.contains("MATCH"))
                .cloned()
                .collect()
        });
        assert_eq!(
            match_statements.len(),
            1,
            "32 phrases must be one MATCH statement, not one per phrase; traced: {match_statements:#?}"
        );
    }

    /// (BU-03 measurement + guard) 病的クエリの実コストを測り、上限で縛る。
    /// `cargo test --release --lib bu03 -- --ignored --nocapture`
    ///
    /// **2026-08-13 の実測** (release、この機械、`BU03_N` で corpus を振った。
    /// 全 32 断片が全行に載っている = すべての arm が満杯の posting list を持つ
    /// 真の最悪形):
    ///
    /// | corpus | 1 phrase / 全行マッチ | 32 phrase / 全行マッチ | 倍率 |
    /// |---|---|---|---|
    /// | 5,000 | 4.25 ms | 46.9 ms | 11.0x |
    /// | 20,000 | 16.0 ms | 171 ms | 10.7x |
    /// | 40,000 | 32.8 ms | 329 ms | 10.1x |
    ///
    /// arm 数に対する伸び (20,000 行、arm 1/2/4/8/16/32):
    /// 17.6 / 22.9 / 34.4 / 44.0 / 81.5 / 172 ms = **arm 数にほぼ線形**。
    ///
    /// 読み取れること:
    ///
    /// 1. コストは照合行数にも arm 数にも **線形**で、超線形ではない
    /// 2. 倍率 ~10x は corpus が増えても増えない (10.1-11.0x)
    /// 3. 母集団の上限 (= 全行) は **1 phrase でも到達できる**。feature-48 は
    ///    新しい上限を作ったのではなく、普通のクエリでそこに届きやすくした。
    ///    ただし**コストの上限は約 10 倍に上がった**
    /// 4. **`LIMIT` を下げてもコストは変わらない** (40,000 行で limit=1 が
    ///    339ms、limit=100 が 329ms)。`ORDER BY bm25(...)` が全マッチ行を
    ///    評価してから `LIMIT` を適用するため。監査が挙げた「`fetch_limit` の
    ///    10,000 を下げる」は**この支配項に効かない** — 効くのは返却行の
    ///    実体化だけ (limit=10,000 で +42ms)
    /// 5. 効く lever は **`MAX_PHRASES`** (arm 数に線形なので、半分にすれば
    ///    最悪コストもほぼ半分)。**が、下げない判断をした** (BU-31): dogfood の
    ///    golden 37 件で phrase 数の最大は 9 で、上限 32 は実クエリに当たって
    ///    いない。下げると最悪コストは半減するが、静かな切り詰めが始まる
    ///    クエリ長も半減する。上限に当たったときの**静かな recall 低下**の方を
    ///    嫌って、値ではなく可視性 (warn) を直した
    ///
    /// 最初の測定は corpus に 32 断片のうち 2 つしか入れておらず、残り 30 arm が
    /// 空の posting list で即棄却されていたため **~2x と 5 倍過小に出ていた**
    /// (codex review P2、PR #136)。「最悪形を測っているつもりで、実は安い形を
    /// 測っていた」典型なので、arm を増やす測定では**全 arm が実際に効いているか**
    /// を先に確認すること。
    ///
    /// ガードは絶対時間ではなく **倍率**で書く。絶対値は機械と SQLite 版で動くが、
    /// 「OR 展開が単一 phrase の何倍か」は次数が変わらない限り安定する。
    #[test]
    #[ignore = "timing-based; run on the nightly leg or by hand"]
    fn bu03_or_expansion_stays_within_a_small_multiple_of_a_single_phrase() {
        use std::time::Instant;
        let n: i32 = std::env::var("BU03_N")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(5000);
        println!("corpus = {n} chunks");
        let db = db_with_384();
        let doc_id = db
            .upsert_document("m.md", Some("m"), None, None, None, &[], None, "h", 0)
            .unwrap();
        // codex review P2 (PR #136): 本文が 32 断片のうち 2 つしか含まないと、
        // 残り 30 arm は posting list が空で即棄却され、測っているのは
        // 「32 arm のうち 2 本だけが広い式」になる。**全 32 断片を全行に**入れて、
        // すべての arm が全行分の posting list を持つ真の最悪形にする。
        let body_prefix = pathological_fragments().join("");
        for i in 0..n {
            db.insert_chunk(
                doc_id,
                i,
                None,
                None,
                &format!("{body_prefix} 評価を記録した第 {i} 節の本文"),
                None,
                &dummy_embedding(0.5),
                1.0,
            )
            .unwrap();
        }

        let pathological = pathological_or_query();
        let mut timing = std::collections::HashMap::new();

        // 対照 3 本:
        //  - selective  : ほぼ当たらない (= 通常のクエリの下限コスト)
        //  - one_common : **1 個**の共通 phrase で全行マッチ (= 母集団は同じ、式は最小)
        //  - pathological: 32 phrase で全行マッチ
        // one_common と pathological の差が「phrase 数そのもののコスト」、
        // selective と one_common の差が「母集団のコスト」。
        for (label, q) in [
            ("selective", "ゐゑをんヴヷ"),
            ("one_common", "について"),
            ("pathological", pathological.as_str()),
        ] {
            let phrases = crate::db::query_phrases(q).len();
            let matches = db.count_fts_matches(q).unwrap();
            let mut best = std::time::Duration::MAX;
            for _ in 0..5 {
                let t = Instant::now();
                let _ = db
                    .search_fts_candidates(
                        q,
                        10,
                        &SearchFilters::default(),
                        FusionParams::default(),
                    )
                    .unwrap();
                best = best.min(t.elapsed());
            }
            println!("{label:>13}: phrases={phrases:2} matches={matches:5} best_of_5={best:?}");
            timing.insert(label, best);
        }

        // LIMIT を変えてもコストが変わらない = ORDER BY bm25 が全マッチ行を
        // 評価している、という主張の直接確認。
        for limit in [1u32, 10, 100, 10_000] {
            let t = Instant::now();
            let hits = db
                .search_fts_candidates(
                    &pathological,
                    limit,
                    &SearchFilters::default(),
                    FusionParams::default(),
                )
                .unwrap();
            println!(
                "  limit={limit:6} returned={:5} elapsed={:?}",
                hits.len(),
                t.elapsed()
            );
        }

        // arm 数に対する伸び。`MAX_PHRASES` を下げることが有効な対策なのかは
        // ここでしか分からない (線形なら効く、頭打ちなら効かない)。
        for arms in [1usize, 2, 4, 8, 16, 32] {
            let q = pathological_fragments()[..arms].join(" ");
            let mut best = std::time::Duration::MAX;
            for _ in 0..3 {
                let t = Instant::now();
                let _ = db
                    .search_fts_candidates(
                        &q,
                        10,
                        &SearchFilters::default(),
                        FusionParams::default(),
                    )
                    .unwrap();
                best = best.min(t.elapsed());
            }
            println!("  arms={arms:3} best_of_3={best:?}");
        }

        // --- ガード ---
        // 同じ母集団に対する 32 phrase / 1 phrase の倍率。実測 10.1-11.0 倍
        // (arm 数に線形) なので 20 倍を上限にする。arm 数に線形である限り
        // 倍率は corpus に依らず一定なので、ここを超えるのは次数が変わった時
        // (例: arm 数に二次) で、機械差では届かない幅を取ってある。
        let one = timing["one_common"].as_secs_f64();
        let many = timing["pathological"].as_secs_f64();
        let ratio = many / one;
        println!("ratio pathological/one_common = {ratio:.2}x (bound: 20x)");
        assert!(
            ratio < 20.0,
            "the 32-phrase OR now costs {ratio:.2}x a single-phrase query over the \
             same {n}-row population; measured 10.1-11.0x when this guard was \
             written, growing linearly with arm count. A jump here means arm count \
             started costing more than linearly."
        );
    }

    #[test]
    fn test_backfill_fts_hydrates_preexisting_db() {
        let db = db_with_384();
        let doc_id = db
            .upsert_document("a.md", Some("a"), None, None, None, &[], None, "h", 0)
            .unwrap();
        db.insert_chunk(
            doc_id,
            0,
            Some("H1"),
            None,
            "hello world",
            None,
            &dummy_embedding(0.1),
            1.0,
        )
        .unwrap();
        db.insert_chunk(
            doc_id,
            1,
            Some("H2"),
            None,
            "second chunk",
            None,
            &dummy_embedding(0.2),
            1.0,
        )
        .unwrap();
        assert_eq!(fts_count(&db), 2);

        // legacy DB を模擬: FTS だけ空にする
        db.conn.execute("DELETE FROM fts_chunks", []).unwrap();
        assert_eq!(fts_count(&db), 0);

        let n = db.backfill_fts().unwrap();
        assert_eq!(n, 2);
        assert_eq!(fts_count(&db), 2);

        // 冪等: 2 回目は 0 件
        let n2 = db.backfill_fts().unwrap();
        assert_eq!(n2, 0);
    }

    #[test]
    fn test_fts_context_column_is_searchable_via_insert_chunk() {
        let db = db_with_384();
        let doc_id = db
            .upsert_document("n/a.md", Some("T"), None, None, None, &[], None, "h", 0)
            .unwrap();
        let emb = dummy_embedding(0.1);
        // content には無いが context にだけある語彙 "パイプライン設計"
        db.insert_chunk(
            doc_id,
            0,
            Some("RRF"),
            Some(3),
            "本文テキスト",
            Some("設計ノート > パイプライン設計 > RRF"),
            &emb,
            1.0,
        )
        .unwrap();
        let hit: i64 = db
            .conn
            .query_row(
                "SELECT COUNT(*) FROM fts_chunks WHERE fts_chunks MATCH 'パイプライン設計'",
                [],
                |r| r.get(0),
            )
            .unwrap_or(0);
        assert!(hit >= 1, "context-only vocabulary must be FTS-searchable");
    }

    #[test]
    fn test_backfill_fts_repopulates_context_column() {
        // FTS から 1 行消して backfill が context 込みで再 index することを確認
        let db = db_with_384();
        let doc_id = db
            .upsert_document("n/b.md", Some("T"), None, None, None, &[], None, "h", 0)
            .unwrap();
        let emb = dummy_embedding(0.1);
        db.insert_chunk(
            doc_id,
            0,
            Some("H"),
            Some(2),
            "body",
            Some("T > H"),
            &emb,
            1.0,
        )
        .unwrap();
        db.conn.execute("DELETE FROM fts_chunks", []).unwrap();
        let n = db.backfill_fts().unwrap();
        assert_eq!(n, 1);
        let hit: i64 = db
            .conn
            .query_row(
                "SELECT COUNT(*) FROM fts_chunks WHERE fts_chunks MATCH 'context : \"T > H\"'",
                [],
                |r| r.get(0),
            )
            .unwrap_or(0);
        assert!(hit >= 1);
    }

    #[test]
    fn test_reset_for_model_switches_dim_and_wipes_data() {
        let db = db_with_384();
        let doc_id = db
            .upsert_document("a.md", Some("a"), None, None, None, &[], None, "h", 0)
            .unwrap();
        db.insert_chunk(
            doc_id,
            0,
            None,
            None,
            "hi",
            None,
            &dummy_embedding(0.1),
            1.0,
        )
        .unwrap();
        assert_eq!(db.chunk_count().unwrap(), 1);
        assert_eq!(db.document_count().unwrap(), 1);

        db.reset_for_model("bge-m3", 1024).unwrap();

        assert_eq!(db.chunk_count().unwrap(), 0);
        assert_eq!(db.document_count().unwrap(), 0);
        assert_eq!(db.current_vec_dim().unwrap(), Some(1024));
        assert_eq!(
            db.read_embedding_meta().unwrap(),
            Some(("bge-m3".to_string(), 1024))
        );

        // 1024-dim insert が通る
        let doc_id2 = db
            .upsert_document("b.md", Some("b"), None, None, None, &[], None, "h", 0)
            .unwrap();
        let emb: Vec<f32> = vec![0.2; 1024];
        db.insert_chunk(doc_id2, 0, None, None, "hi2", None, &emb, 1.0)
            .unwrap();
        assert_eq!(db.chunk_count().unwrap(), 1);

        // 自分で張った transaction は commit まで済んでいること (= 開いたまま
        // 残していない)。commit 忘れは Drop で黙って ROLLBACK される。
        assert!(db.conn.is_autocommit());
    }

    /// AU-11 の regression: 途中で落ちた `reset_for_model` が index を壊さないこと。
    ///
    /// `dim` が vec0 の上限 8192 を超えると `CREATE VIRTUAL TABLE` が
    /// "Dimension on vector column too large" で失敗する。transaction が
    /// 無いと、その手前の DELETE 3 文と `DROP TABLE vec_chunks` は既に
    /// 適用済みなので、documents も chunks も vec_chunks も消えた DB が残る。
    #[test]
    fn a_failed_reset_for_model_leaves_the_index_exactly_as_it_was() {
        let db = db_with_384();
        let doc_id = db
            .upsert_document("a.md", Some("a"), None, None, None, &[], None, "h", 0)
            .unwrap();
        db.insert_chunk(
            doc_id,
            0,
            None,
            None,
            "hi",
            None,
            &dummy_embedding(0.1),
            1.0,
        )
        .unwrap();
        let vec_rows_before: i64 = db
            .conn
            .query_row("SELECT count(*) FROM vec_chunks", [], |r| r.get(0))
            .unwrap();
        assert_eq!(vec_rows_before, 1);

        let err = db
            .reset_for_model("impossible", 100_000)
            .expect_err("vec0 rejects dimensions above 8192");
        assert!(
            err.to_string().contains("too large"),
            "unexpected failure mode: {err}"
        );

        assert_eq!(db.document_count().unwrap(), 1);
        assert_eq!(db.chunk_count().unwrap(), 1);
        assert_eq!(db.current_vec_dim().unwrap(), Some(384));
        assert_eq!(
            db.conn
                .query_row::<i64, _, _>("SELECT count(*) FROM vec_chunks", [], |r| r.get(0))
                .unwrap(),
            1
        );
        assert_eq!(
            db.read_embedding_meta().unwrap(),
            Some(("bge-small-en-v1.5".to_string(), 384))
        );

        // 残骸ではなく使える index であること。
        let doc_id2 = db
            .upsert_document("b.md", Some("b"), None, None, None, &[], None, "h2", 0)
            .unwrap();
        db.insert_chunk(
            doc_id2,
            0,
            None,
            None,
            "hi2",
            None,
            &dummy_embedding(0.2),
            1.0,
        )
        .unwrap();
        assert_eq!(db.chunk_count().unwrap(), 2);
    }

    /// 呼び出し側が transaction を張っていれば、`reset_for_model` は自分では
    /// 張らずに親へ参加する (SQLite に真のネスト transaction が無いため)。
    /// 親を rollback すると reset ごと巻き戻ることで確かめる。
    #[test]
    fn reset_for_model_joins_the_callers_transaction() {
        let db = db_with_384();
        let doc_id = db
            .upsert_document("a.md", Some("a"), None, None, None, &[], None, "h", 0)
            .unwrap();
        db.insert_chunk(
            doc_id,
            0,
            None,
            None,
            "hi",
            None,
            &dummy_embedding(0.1),
            1.0,
        )
        .unwrap();

        {
            let tx = db.conn.unchecked_transaction().unwrap();
            db.reset_for_model("bge-m3", 1024).unwrap();
            // reset は成功しているので、この時点では新しい姿になっている。
            assert_eq!(db.document_count().unwrap(), 0);
            assert_eq!(db.current_vec_dim().unwrap(), Some(1024));
            drop(tx); // commit しない = ROLLBACK
        }

        assert_eq!(db.document_count().unwrap(), 1);
        assert_eq!(db.chunk_count().unwrap(), 1);
        assert_eq!(db.current_vec_dim().unwrap(), Some(384));
        assert_eq!(
            db.read_embedding_meta().unwrap(),
            Some(("bge-small-en-v1.5".to_string(), 384))
        );
    }

    #[test]
    fn test_verify_embedding_meta_fresh_db() {
        let db = Database::open_in_memory().unwrap();
        assert!(db.read_embedding_meta().unwrap().is_none());

        db.verify_embedding_meta("bge-small-en-v1.5", 384).unwrap();

        let meta = db.read_embedding_meta().unwrap();
        assert_eq!(meta, Some(("bge-small-en-v1.5".to_string(), 384)));
    }

    #[test]
    fn test_verify_embedding_meta_migrates_preexisting_db() {
        // Simulate a legacy DB: chunks exist but meta is empty.
        // In legacy code `init()` always created vec_chunks with the
        // 384-dim literal. Reproduce that here by creating it manually.
        let db = Database::open_in_memory().unwrap();
        db.ensure_vec_chunks_table(384).unwrap();
        let doc_id = db
            .upsert_document(
                "deep-dive/mcp/overview.md",
                Some("MCP Overview"),
                Some("mcp"),
                Some("deep-dive"),
                None,
                &[],
                Some("2026-04-16"),
                "h",
                0,
            )
            .unwrap();
        db.insert_chunk(
            doc_id,
            0,
            None,
            None,
            "hi",
            None,
            &dummy_embedding(0.1),
            1.0,
        )
        .unwrap();
        assert!(db.read_embedding_meta().unwrap().is_none());

        db.verify_embedding_meta("bge-small-en-v1.5", 384).unwrap();

        assert_eq!(
            db.read_embedding_meta().unwrap(),
            Some(("bge-small-en-v1.5".to_string(), 384))
        );
    }

    #[test]
    fn test_verify_embedding_meta_idempotent_on_match() {
        let db = Database::open_in_memory().unwrap();
        db.verify_embedding_meta("bge-small-en-v1.5", 384).unwrap();
        // Second call with same args must succeed.
        db.verify_embedding_meta("bge-small-en-v1.5", 384).unwrap();
    }

    #[test]
    fn test_verify_embedding_meta_detects_mismatch() {
        let db = Database::open_in_memory().unwrap();
        db.verify_embedding_meta("bge-small-en-v1.5", 384).unwrap();

        let err = db
            .verify_embedding_meta("bge-m3", 1024)
            .expect_err("mismatch must be rejected");
        let msg = err.to_string();
        assert!(msg.contains("bge-small-en-v1.5"), "msg: {msg}");
        assert!(msg.contains("bge-m3"), "msg: {msg}");
        assert!(msg.contains("--force"), "msg: {msg}");
    }

    #[test]
    fn test_read_embedding_meta_returns_none_when_half_written() {
        let db = Database::open_in_memory().unwrap();
        db.conn
            .execute(
                "INSERT INTO index_meta (key, value) VALUES ('embedding_model', 'x')",
                [],
            )
            .unwrap();
        // dim missing → None (not an error, treated as unrecorded).
        assert!(db.read_embedding_meta().unwrap().is_none());
    }

    #[test]
    fn test_context_mode_round_trip() {
        let db = Database::open_in_memory().unwrap();
        assert!(db.read_context_mode().unwrap().is_none()); // key 不在
        db.write_context_mode(ContextMode::Static).unwrap();
        assert_eq!(db.read_context_mode().unwrap(), Some(ContextMode::Static));
        db.write_context_mode(ContextMode::Off).unwrap();
        assert_eq!(db.read_context_mode().unwrap(), Some(ContextMode::Off));
    }

    #[test]
    fn test_context_mode_malformed_is_none() {
        let db = Database::open_in_memory().unwrap();
        db.conn
            .execute(
                "INSERT INTO index_meta (key, value) VALUES ('context_mode', 'garbage')",
                [],
            )
            .unwrap();
        assert!(db.read_context_mode().unwrap().is_none());
    }

    #[test]
    fn test_get_document_title() {
        let db = db_with_384();
        db.upsert_document(
            "n/a.md",
            Some("My Title"),
            None,
            None,
            None,
            &[],
            None,
            "h",
            0,
        )
        .unwrap();
        assert_eq!(
            db.get_document_title("n/a.md").unwrap().as_deref(),
            Some("My Title")
        );
        assert!(db.get_document_title("missing.md").unwrap().is_none());
    }

    #[test]
    fn test_list_topics() {
        let db = Database::open_in_memory().unwrap();

        // 3 docs across 2 topic groups
        db.upsert_document(
            "deep-dive/mcp/overview.md",
            Some("MCP Overview"),
            Some("mcp"),
            Some("deep-dive"),
            Some("1"),
            &[],
            Some("2026-04-15"),
            "h1",
            0,
        )
        .unwrap();
        db.upsert_document(
            "deep-dive/mcp/features.md",
            Some("MCP Features"),
            Some("mcp"),
            Some("deep-dive"),
            Some("3"),
            &[],
            Some("2026-04-16"),
            "h2",
            0,
        )
        .unwrap();
        db.upsert_document(
            "ai-news/2026-04-16.md",
            Some("AI News Today"),
            None,
            Some("ai-news"),
            None,
            &[],
            Some("2026-04-16"),
            "h3",
            0,
        )
        .unwrap();

        let topics = db.list_topics().unwrap();
        println!("topics: {topics:#?}");

        assert_eq!(topics.len(), 2, "2 distinct (category,topic) groups");

        // Find the ai-news group (topic = None)
        let ai = topics
            .iter()
            .find(|t| t.category.as_deref() == Some("ai-news"))
            .expect("should have ai-news group");
        assert_eq!(ai.file_count, 1);
        assert!(ai.titles.contains(&"AI News Today".to_string()));

        // Find the deep-dive/mcp group
        let mcp = topics
            .iter()
            .find(|t| t.topic.as_deref() == Some("mcp"))
            .expect("should have mcp group");
        assert_eq!(mcp.file_count, 2);
        assert!(mcp.titles.contains(&"MCP Overview".to_string()));
        assert!(mcp.titles.contains(&"MCP Features".to_string()));

        println!("test_list_topics: OK");
    }

    /// Regression for F-30: title that contains the legacy `||` separator
    /// must not be split. Prior implementation used GROUP_CONCAT(title, '||')
    /// + .split("||"), which silently fragmented such titles.
    #[test]
    fn test_list_topics_title_with_double_pipe_is_not_split() {
        let db = Database::open_in_memory().unwrap();
        db.upsert_document(
            "deep-dive/x/a.md",
            Some("foo || bar"),
            Some("x"),
            Some("deep-dive"),
            None,
            &[],
            Some("2026-04-29"),
            "h1",
            0,
        )
        .unwrap();
        db.upsert_document(
            "deep-dive/x/b.md",
            Some("plain title"),
            Some("x"),
            Some("deep-dive"),
            None,
            &[],
            Some("2026-04-29"),
            "h2",
            0,
        )
        .unwrap();

        let topics = db.list_topics().unwrap();
        let group = topics
            .iter()
            .find(|t| t.topic.as_deref() == Some("x"))
            .expect("group exists");
        assert_eq!(group.file_count, 2);
        assert_eq!(
            group.titles.len(),
            2,
            "expected 2 titles, got {:?}",
            group.titles
        );
        assert!(
            group.titles.contains(&"foo || bar".to_string()),
            "title with || was fragmented: {:?}",
            group.titles
        );
        assert!(group.titles.contains(&"plain title".to_string()));
    }

    #[test]
    fn test_search_result_includes_tags() {
        let db = db_with_384();
        let doc_id = db
            .upsert_document(
                "tagged.md",
                Some("Tagged"),
                None,
                None,
                None,
                &["rust".into(), "async".into()],
                None,
                "h1",
                0,
            )
            .unwrap();
        db.insert_chunk(
            doc_id,
            0,
            None,
            None,
            "body",
            None,
            &dummy_embedding(0.1),
            1.0,
        )
        .unwrap();

        let hits = db
            .search_similar(&dummy_embedding(0.1), 5, &SearchFilters::default())
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].tags, vec!["rust".to_string(), "async".to_string()]);
    }

    #[test]
    fn test_filter_path_globs_include_only() {
        let db = db_with_384();
        for (i, p) in ["docs/a.md", "docs/b.md", "notes/c.md"].iter().enumerate() {
            let id = db
                .upsert_document(
                    p,
                    Some("t"),
                    None,
                    None,
                    None,
                    &[],
                    None,
                    &format!("h{i}"),
                    0,
                )
                .unwrap();
            db.insert_chunk(
                id,
                0,
                None,
                None,
                "body",
                None,
                &dummy_embedding(0.1 + i as f32 * 0.01),
                1.0,
            )
            .unwrap();
        }

        let include = globset::GlobSetBuilder::new()
            .add(globset::Glob::new("docs/**").unwrap())
            .build()
            .unwrap();
        let cpg = CompiledPathGlobs {
            include: Some(include),
            exclude: None,
        };
        let filters = SearchFilters {
            path_globs: Some(&cpg),
            ..Default::default()
        };
        let hits = db
            .search_similar(&dummy_embedding(0.1), 10, &filters)
            .unwrap();
        let paths: Vec<&str> = hits.iter().map(|h| h.path.as_str()).collect();
        assert!(paths.iter().all(|p| p.starts_with("docs/")));
        assert_eq!(paths.len(), 2);
    }

    #[test]
    fn test_filter_path_globs_with_exclude() {
        let db = db_with_384();
        for (i, p) in ["docs/a.md", "docs/draft/b.md", "docs/c.md"]
            .iter()
            .enumerate()
        {
            let id = db
                .upsert_document(
                    p,
                    Some("t"),
                    None,
                    None,
                    None,
                    &[],
                    None,
                    &format!("h{i}"),
                    0,
                )
                .unwrap();
            db.insert_chunk(
                id,
                0,
                None,
                None,
                "body",
                None,
                &dummy_embedding(0.1 + i as f32 * 0.01),
                1.0,
            )
            .unwrap();
        }

        let include = globset::GlobSetBuilder::new()
            .add(globset::Glob::new("docs/**").unwrap())
            .build()
            .unwrap();
        let exclude = globset::GlobSetBuilder::new()
            .add(globset::Glob::new("docs/draft/**").unwrap())
            .build()
            .unwrap();
        let cpg = CompiledPathGlobs {
            include: Some(include),
            exclude: Some(exclude),
        };
        let filters = SearchFilters {
            path_globs: Some(&cpg),
            ..Default::default()
        };
        let hits = db
            .search_similar(&dummy_embedding(0.1), 10, &filters)
            .unwrap();
        let paths: Vec<&str> = hits.iter().map(|h| h.path.as_str()).collect();
        assert!(!paths.iter().any(|p| p.starts_with("docs/draft/")));
        assert_eq!(paths.len(), 2);
    }

    #[test]
    fn test_filter_tags_all_and_tags_any_combined() {
        let db = db_with_384();
        let cases: &[(&str, &[&str])] = &[
            ("doc_a.md", &["x", "b"]), // tags_all=[x] OK, tags_any=[b,c] OK -> pass
            ("doc_b.md", &["x", "z"]), // tags_all=[x] OK, tags_any=[b,c] NG -> fail
            ("doc_c.md", &["b", "c"]), // tags_all=[x] NG -> fail
            ("doc_d.md", &["x", "c", "b"]), // both OK -> pass
        ];
        for (i, (p, tags)) in cases.iter().enumerate() {
            let tags_owned: Vec<String> = tags.iter().map(|s| s.to_string()).collect();
            let id = db
                .upsert_document(
                    p,
                    Some("t"),
                    None,
                    None,
                    None,
                    &tags_owned,
                    None,
                    &format!("h{i}"),
                    0,
                )
                .unwrap();
            db.insert_chunk(
                id,
                0,
                None,
                None,
                "body",
                None,
                &dummy_embedding(0.1 + i as f32 * 0.01),
                1.0,
            )
            .unwrap();
        }
        let any_pool: Vec<String> = vec!["b".into(), "c".into()];
        let all_pool: Vec<String> = vec!["x".into()];
        let filters = SearchFilters {
            tags_any: &any_pool,
            tags_all: &all_pool,
            ..Default::default()
        };
        let hits = db
            .search_similar(&dummy_embedding(0.1), 10, &filters)
            .unwrap();
        let paths: Vec<&str> = hits.iter().map(|h| h.path.as_str()).collect();
        assert!(paths.contains(&"doc_a.md"));
        assert!(paths.contains(&"doc_d.md"));
        assert!(!paths.contains(&"doc_b.md"));
        assert!(!paths.contains(&"doc_c.md"));
    }

    #[test]
    fn test_filter_date_range_strict_excludes_missing() {
        let db = db_with_384();
        let dates = &[
            ("a.md", Some("2026-01-15")),
            ("b.md", Some("2026-04-01")),
            ("c.md", Some("2025-12-31")),
            ("d.md", None),
        ];
        for (i, (p, d)) in dates.iter().enumerate() {
            let id = db
                .upsert_document(p, Some("t"), None, None, None, &[], *d, &format!("h{i}"), 0)
                .unwrap();
            db.insert_chunk(
                id,
                0,
                None,
                None,
                "body",
                None,
                &dummy_embedding(0.1 + i as f32 * 0.01),
                1.0,
            )
            .unwrap();
        }
        let filters = SearchFilters {
            date_from: Some("2026-01-01"),
            date_to: Some("2026-12-31"),
            ..Default::default()
        };
        let hits = db
            .search_similar(&dummy_embedding(0.1), 10, &filters)
            .unwrap();
        let paths: Vec<&str> = hits.iter().map(|h| h.path.as_str()).collect();
        assert!(paths.contains(&"a.md"));
        assert!(paths.contains(&"b.md"));
        assert!(!paths.contains(&"c.md"));
        assert!(
            !paths.contains(&"d.md"),
            "missing date is excluded (strict)"
        );
    }

    #[test]
    fn test_filter_has_any_triggers_overfetch_for_path_globs() {
        let db = db_with_384();
        // 19 件は query embedding (0.5) と完全一致する位置に置き、KNN 距離 0 で
        // 上位を独占する。`docs/keep.md` だけ query から離れた embedding (0.99)
        // にする。`limit=5` の素朴な KNN では `docs/keep.md` は決して上位 5 件に
        // 入らない (距離が常に他より大きい)。over-fetch (10x = 50 件) が効いて
        // ようやく拾える。over-fetch が効かなくなれば 0 件返るので、確定的に
        // この機構の動作を検証できる。
        for i in 0..20 {
            let (path, emb_seed) = if i == 0 {
                ("docs/keep.md".to_string(), 0.99_f32)
            } else {
                (format!("other/{i}.md"), 0.5_f32)
            };
            let id = db
                .upsert_document(
                    &path,
                    Some("t"),
                    None,
                    None,
                    None,
                    &[],
                    None,
                    &format!("h{i}"),
                    0,
                )
                .unwrap();
            db.insert_chunk(
                id,
                0,
                None,
                None,
                "body",
                None,
                &dummy_embedding(emb_seed),
                1.0,
            )
            .unwrap();
        }
        let include = globset::GlobSetBuilder::new()
            .add(globset::Glob::new("docs/**").unwrap())
            .build()
            .unwrap();
        let cpg = CompiledPathGlobs {
            include: Some(include),
            exclude: None,
        };
        let filters = SearchFilters {
            path_globs: Some(&cpg),
            ..Default::default()
        };
        // limit=5。素朴な KNN では `docs/keep.md` は他 19 件 (距離 0) より遠い
        // ので top-5 に入らず 0 件返るはず。over-fetch (50 件) で全件取り、
        // path_globs で他 19 件を弾いて `docs/keep.md` を 1 件返すのが正解。
        let hits = db
            .search_similar(&dummy_embedding(0.5), 5, &filters)
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].path, "docs/keep.md");
    }

    #[test]
    fn test_filter_path_globs_applies_to_fts_branch() {
        // search_vec_candidates と search_fts_candidates は同じフィルタブロックを
        // 重複実装している。`search_similar` 経由のテスト 4 つは vec branch しか
        // 通らない。string query を search_hybrid に通すと FTS branch が発火し、
        // FTS 側の path_globs 適用が確認できる。
        let db = db_with_384();
        for (i, p) in ["docs/a.md", "docs/b.md", "notes/c.md", "notes/d.md"]
            .iter()
            .enumerate()
        {
            let id = db
                .upsert_document(
                    p,
                    Some("t"),
                    None,
                    None,
                    None,
                    &[],
                    None,
                    &format!("h{i}"),
                    0,
                )
                .unwrap();
            // FTS にヒットさせる固有のキーワードを各 chunk に含める
            db.insert_chunk(
                id,
                0,
                None,
                None,
                "kibarashi_unique_keyword body",
                None,
                &dummy_embedding(0.1 + i as f32 * 0.01),
                1.0,
            )
            .unwrap();
        }

        let include = globset::GlobSetBuilder::new()
            .add(globset::Glob::new("docs/**").unwrap())
            .build()
            .unwrap();
        let cpg = CompiledPathGlobs {
            include: Some(include),
            exclude: None,
        };
        let filters = SearchFilters {
            path_globs: Some(&cpg),
            ..Default::default()
        };

        // search_hybrid は FTS と vec を融合する。FTS 側にも path_globs フィルタが
        // 効いていれば notes/ は返らない。
        let hits = db
            .search_hybrid(
                "kibarashi_unique_keyword",
                &dummy_embedding(0.1),
                10,
                &filters,
                FusionParams::default(),
            )
            .unwrap();
        let paths: Vec<&str> = hits.iter().map(|h| h.path.as_str()).collect();
        assert!(paths.iter().all(|p| p.starts_with("docs/")));
        assert_eq!(paths.len(), 2, "docs/a.md と docs/b.md のみ通る");
    }

    #[test]
    fn test_filter_tags_applies_to_fts_branch() {
        // 同じく FTS branch の tags フィルタを直接検証。
        let db = db_with_384();
        let cases: &[(&str, &[&str])] = &[
            ("doc_with_rust.md", &["rust"]),
            ("doc_with_other.md", &["python"]),
        ];
        for (i, (p, tags)) in cases.iter().enumerate() {
            let tags_owned: Vec<String> = tags.iter().map(|s| s.to_string()).collect();
            let id = db
                .upsert_document(
                    p,
                    Some("t"),
                    None,
                    None,
                    None,
                    &tags_owned,
                    None,
                    &format!("h{i}"),
                    0,
                )
                .unwrap();
            db.insert_chunk(
                id,
                0,
                None,
                None,
                "kibarashi_unique_keyword body",
                None,
                &dummy_embedding(0.1 + i as f32 * 0.01),
                1.0,
            )
            .unwrap();
        }
        let any_pool: Vec<String> = vec!["rust".into()];
        let filters = SearchFilters {
            tags_any: &any_pool,
            ..Default::default()
        };
        let hits = db
            .search_hybrid(
                "kibarashi_unique_keyword",
                &dummy_embedding(0.1),
                10,
                &filters,
                FusionParams::default(),
            )
            .unwrap();
        let paths: Vec<&str> = hits.iter().map(|h| h.path.as_str()).collect();
        assert_eq!(paths, vec!["doc_with_rust.md"]);
    }

    #[test]
    fn test_search_hit_has_match_spans_field_default_none() {
        // SearchResult から SearchHit に変換した直後は match_spans は None。
        // (具体的な計算は server レイヤで行う)
        let r = SearchResult {
            score: 0.1,
            content: "abc".into(),
            heading: None,
            document_id: 0,
            path: "x.md".into(),
            title: None,
            topic: None,
            date: None,
            tags: vec![],
            context_text: None,
        };
        let h: SearchHit = r.into();
        assert!(h.match_spans.is_none());
    }

    #[test]
    fn test_searchhit_does_not_serialize_context_text() {
        // context_text を持つ SearchResult を SearchHit に変換 → JSON に context が出ない
        let r = SearchResult {
            score: 1.0,
            content: "body".to_string(),
            heading: Some("H".to_string()),
            document_id: 1,
            path: "a.md".to_string(),
            title: Some("T".to_string()),
            topic: None,
            date: None,
            tags: vec![],
            context_text: Some("T > H".to_string()),
        };
        let hit: SearchHit = r.into();
        let json = serde_json::to_string(&hit).unwrap();
        assert!(
            !json.contains("context"),
            "context must not leak into SearchHit JSON: {json}"
        );
        assert!(!json.contains("T > H"));
    }

    /// Local helper: create a temp directory unique to this test process /
    /// invocation. Mirrors the pattern used in `tests/validate_cli.rs`
    /// (`tempfile` crate is intentionally avoided per project policy).
    struct TempPath {
        path: std::path::PathBuf,
    }

    impl Drop for TempPath {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    fn tempdir_for_test() -> TempPath {
        let p = crate::test_support::unique_temp_path("kb-mcp-test");
        std::fs::create_dir_all(&p).unwrap();
        TempPath { path: p }
    }

    #[test]
    fn test_ensure_chunk_level_column_idempotent() {
        let tmp = tempdir_for_test();
        let db_path = tmp.path.join("test.db");
        let db_path_str = db_path.to_str().expect("utf-8 path");
        // 新規作成 → ensure を 2 回呼ぶ (race / 重複呼びを模す)。
        // 1 回目は init で列が既に作られているので no-op、2 回目も no-op で成功。
        {
            let db = Database::open(db_path_str).expect("open");
            db.ensure_chunk_level_column().expect("first ensure");
            db.ensure_chunk_level_column().expect("idempotent ensure");
        }
        // 列が存在することを PRAGMA で確認 (db wrapper を経由せず直接 reopen)。
        let conn = rusqlite::Connection::open(&db_path).expect("re-open");
        let cols: Vec<String> = conn
            .prepare("PRAGMA table_info(chunks)")
            .unwrap()
            .query_map([], |r| r.get::<_, String>(1))
            .unwrap()
            .filter_map(Result::ok)
            .collect();
        assert!(
            cols.iter().any(|c| c == "level"),
            "level column missing: {cols:?}"
        );
    }

    /// Roundtrip: a `level` value passed to `insert_chunk` lands in the
    /// `chunks.level` column verbatim. Guards against future refactors of the
    /// INSERT SQL silently dropping the bind. Re-opens the DB with a raw
    /// rusqlite connection so we exercise the on-disk row, not just the
    /// in-memory wrapper.
    #[test]
    fn test_insert_chunk_persists_level() {
        let tmp = tempdir_for_test();
        let db_path = tmp.path.join("test.db");
        let db_path_str = db_path.to_str().expect("utf-8 path");

        let chunk_id = {
            let db = Database::open(db_path_str).expect("open");
            // vec_chunks (sqlite-vec virtual table) is created lazily by
            // `verify_embedding_meta`. Without it the INSERT into vec_chunks
            // inside `insert_chunk` fails with "no such table".
            db.verify_embedding_meta("bge-small-en-v1.5", 384)
                .expect("verify_embedding_meta");
            let doc_id = db
                .upsert_document(
                    "notes/level.md",
                    Some("Level Test"),
                    None,
                    None,
                    None,
                    &[],
                    None,
                    "hash_level",
                    0,
                )
                .expect("upsert document");
            db.insert_chunk(
                doc_id,
                0,
                Some("Sec"),
                Some(2),
                "body",
                None,
                &dummy_embedding(0.1),
                1.0,
            )
            .expect("insert chunk")
        };

        // Re-open via raw rusqlite to confirm the value is on disk.
        let conn = rusqlite::Connection::open(&db_path).expect("re-open");
        let level: Option<i64> = conn
            .query_row(
                "SELECT level FROM chunks WHERE id = ?1",
                rusqlite::params![chunk_id],
                |row| row.get(0),
            )
            .expect("select level");
        assert_eq!(level, Some(2));
    }

    #[test]
    fn test_context_text_column_round_trip() {
        let db = db_with_384();
        let doc_id = db
            .upsert_document(
                "notes/a.md",
                Some("T"),
                None,
                Some("notes"),
                None,
                &[],
                None,
                "h",
                0,
            )
            .unwrap();
        let emb = dummy_embedding(0.1);
        db.insert_chunk(
            doc_id,
            0,
            Some("H"),
            Some(2),
            "body",
            Some("T > H"),
            &emb,
            1.0,
        )
        .unwrap();
        let stored: Option<String> = db
            .conn
            .query_row(
                "SELECT context_text FROM chunks WHERE chunk_index = 0",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(stored.as_deref(), Some("T > H"));
    }

    #[test]
    fn test_insert_chunk_context_none_stores_null() {
        let db = db_with_384();
        let doc_id = db
            .upsert_document("n/b.md", Some("T"), None, None, None, &[], None, "h", 0)
            .unwrap();
        let emb = dummy_embedding(0.2);
        db.insert_chunk(doc_id, 0, Some("H"), Some(2), "body", None, &emb, 1.0)
            .unwrap();
        let stored: Option<String> = db
            .conn
            .query_row(
                "SELECT context_text FROM chunks WHERE chunk_index = 0",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(stored.is_none());
    }

    #[test]
    fn test_ensure_context_text_column_migrates_legacy_chunks() {
        // legacy DB (context_text 列なし) を模して、列を落としてから ensure を呼ぶ。
        let db = db_with_384();
        db.conn
            .execute_batch("DROP TABLE fts_chunks; DROP TABLE vec_chunks; DROP TABLE chunks;")
            .unwrap();
        // context_text 列を持たない古い chunks テーブルを再現
        db.conn
            .execute_batch(
                "CREATE TABLE chunks (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    document_id INTEGER NOT NULL,
                    chunk_index INTEGER NOT NULL,
                    heading TEXT, level INTEGER, content TEXT NOT NULL,
                    token_count INTEGER, quality_score REAL NOT NULL DEFAULT 1.0
                );",
            )
            .unwrap();
        // 列が無いことを確認 → ensure 後は有る
        db.ensure_context_text_column().unwrap();
        let has: bool = db
            .conn
            .prepare("PRAGMA table_info(chunks)")
            .unwrap()
            .query_map([], |r| r.get::<_, String>(1))
            .unwrap()
            .filter_map(Result::ok)
            .any(|n| n == "context_text");
        assert!(has, "context_text column must be added by migration");
        // 冪等: 2 回目は no-op
        db.ensure_context_text_column().unwrap();
    }

    // -- feature-51: documents.size_bytes ------------------------------------

    #[test]
    fn test_ensure_document_size_column_migrates_legacy_documents() {
        // legacy DB (size_bytes 列なし) を模して、列を落としてから ensure を呼ぶ。
        let db = db_with_384();
        db.conn
            .execute_batch(
                "DROP TABLE fts_chunks; DROP TABLE vec_chunks; DROP TABLE chunks; \
                 DROP TABLE documents;",
            )
            .unwrap();
        db.conn
            .execute_batch(
                "CREATE TABLE documents (
                    id INTEGER PRIMARY KEY AUTOINCREMENT, path TEXT UNIQUE NOT NULL,
                    title TEXT, topic TEXT, category TEXT, depth TEXT, tags TEXT,
                    date TEXT, content_hash TEXT NOT NULL, last_indexed TEXT NOT NULL
                );
                 INSERT INTO documents (path, content_hash, last_indexed)
                 VALUES ('legacy.md', 'h', '2026-01-01T00:00:00Z');",
            )
            .unwrap();

        db.ensure_document_size_column().unwrap();
        let has: bool = db
            .conn
            .prepare("PRAGMA table_info(documents)")
            .unwrap()
            .query_map([], |r| r.get::<_, String>(1))
            .unwrap()
            .filter_map(Result::ok)
            .any(|n| n == "size_bytes");
        assert!(has, "size_bytes column must be added by migration");

        // The row that predates the column keeps NULL — "not recorded", which is
        // what the backfill and `doctor` both key off. Filling it with 0 here
        // would claim an empty file.
        let stored: Option<i64> = db
            .conn
            .query_row(
                "SELECT size_bytes FROM documents WHERE path = 'legacy.md'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(stored.is_none(), "a pre-existing row must stay unrecorded");

        // 冪等: 2 回目は no-op
        db.ensure_document_size_column().unwrap();
    }

    #[test]
    fn test_upsert_document_records_the_size_it_was_given() {
        let db = db_with_384();
        db.upsert_document("a.md", None, None, None, None, &[], None, "h1", 4096)
            .unwrap();
        let read = |db: &Database| -> Option<i64> {
            db.conn
                .query_row(
                    "SELECT size_bytes FROM documents WHERE path = 'a.md'",
                    [],
                    |r| r.get(0),
                )
                .unwrap()
        };
        assert_eq!(read(&db), Some(4096));

        // The UPDATE branch has to carry it too, or an edit that shrinks a file
        // would leave the old size recorded next to the new hash.
        db.upsert_document("a.md", None, None, None, None, &[], None, "h2", 10)
            .unwrap();
        assert_eq!(read(&db), Some(10));

        // So does the frontmatter-only path, which is reached when the bytes
        // changed but the chunks did not.
        db.update_document_meta("a.md", None, None, None, None, &[], None, "h3", 77)
            .unwrap();
        assert_eq!(read(&db), Some(77));
    }

    #[test]
    fn test_backfill_document_sizes_fills_only_unrecorded_rows() {
        let db = db_with_384();
        db.upsert_document("recorded.md", None, None, None, None, &[], None, "h", 500)
            .unwrap();
        db.upsert_document("legacy.md", None, None, None, None, &[], None, "h", 0)
            .unwrap();
        // Simulate the legacy row: written before the column existed.
        db.conn
            .execute_batch("UPDATE documents SET size_bytes = NULL WHERE path = 'legacy.md'")
            .unwrap();

        let filled = db
            .backfill_document_sizes(&[("legacy.md", 123), ("recorded.md", 999)])
            .unwrap();
        assert_eq!(filled, 1, "only the unrecorded row counts as filled");

        let size_of = |p: &str| -> Option<i64> {
            db.conn
                .query_row(
                    "SELECT size_bytes FROM documents WHERE path = ?1",
                    params![p],
                    |r| r.get(0),
                )
                .unwrap()
        };
        assert_eq!(size_of("legacy.md"), Some(123));
        // A recorded size was written by the path that actually read those
        // bytes; the backfill must not talk over it.
        assert_eq!(size_of("recorded.md"), Some(500));

        // 2 回目は 1 行も該当しない (index のたびに走るので no-op であること)。
        assert_eq!(
            db.backfill_document_sizes(&[("legacy.md", 456)]).unwrap(),
            0
        );
        assert_eq!(size_of("legacy.md"), Some(123));
    }

    /// `kb-mcp doctor` compares the three tables that must agree about a chunk,
    /// which needs an unconstrained scan of each. `fts_chunks` is already known
    /// to allow one (`backfill_fts` reads `SELECT rowid FROM fts_chunks`), but
    /// `vec_chunks` is a `vec0` virtual table, and some vector extensions only
    /// answer queries that carry a KNN or rowid constraint. Measured here
    /// rather than assumed, and kept so an upgrade that takes the capability
    /// away is a failing test rather than a broken subcommand.
    #[test]
    fn test_vec_chunks_answers_an_unconstrained_scan() {
        let db = db_with_384();
        let doc = db
            .upsert_document("a.md", None, None, None, None, &[], None, "h", 0)
            .unwrap();
        db.insert_chunk(doc, 0, None, None, "body", None, &vec![0.1; 384], 1.0)
            .unwrap();

        let count: i64 = db
            .conn
            .query_row("SELECT COUNT(*) FROM vec_chunks", [], |r| r.get(0))
            .expect("vec_chunks must answer COUNT(*)");
        assert_eq!(count, 1);

        let ids: Vec<i64> = db
            .conn
            .prepare("SELECT chunk_id FROM vec_chunks")
            .expect("vec_chunks must answer an unconstrained SELECT")
            .query_map([], |r| r.get(0))
            .unwrap()
            .filter_map(Result::ok)
            .collect();
        assert_eq!(
            ids.len(),
            1,
            "the scan must return the row, not just count it"
        );
    }

    /// codex P2 round 4: a file that grows past the index cap is *skipped*, and
    /// §4.2 keeps its row — so the recorded size stays the last one small
    /// enough to index while the file on disk is now one a read refuses. The
    /// backfill cannot fix that (the row is not NULL), so the size-cap path
    /// needs a write that overwrites.
    #[test]
    fn test_record_document_sizes_overwrites_a_stale_recorded_size() {
        let db = db_with_384();
        db.upsert_document("grown.md", None, None, None, None, &[], None, "h", 900)
            .unwrap();
        let size_of = |p: &str| -> Option<i64> {
            db.conn
                .query_row(
                    "SELECT size_bytes FROM documents WHERE path = ?1",
                    params![p],
                    |r| r.get(0),
                )
                .unwrap()
        };
        assert_eq!(size_of("grown.md"), Some(900));

        // The backfill deliberately will not touch it — the value is not NULL.
        assert_eq!(
            db.backfill_document_sizes(&[("grown.md", 60_000_000)])
                .unwrap(),
            0
        );
        assert_eq!(size_of("grown.md"), Some(900));

        // This one is for exactly that case.
        assert_eq!(
            db.record_document_sizes(&[("grown.md", 60_000_000)])
                .unwrap(),
            1
        );
        assert_eq!(size_of("grown.md"), Some(60_000_000));

        // A path with no row is not an error: the file may never have been
        // indexed at all.
        assert_eq!(db.record_document_sizes(&[("absent.md", 1)]).unwrap(), 0);
    }

    #[test]
    fn test_backfill_document_sizes_ignores_paths_it_does_not_know() {
        let db = db_with_384();
        // The disk scan reports every file it read, including ones this index
        // run is about to add for the first time. A path with no row is not an
        // error; it simply updates nothing.
        assert_eq!(
            db.backfill_document_sizes(&[("never-indexed.md", 10)])
                .unwrap(),
            0
        );
    }

    /// Companion to the above: passing `None` for `level` stores SQL NULL
    /// (this is the path used by .txt and frontmatter-only / pre-heading
    /// chunks, and also by every test fixture site that doesn't care).
    #[test]
    fn test_insert_chunk_persists_level_none_as_null() {
        let tmp = tempdir_for_test();
        let db_path = tmp.path.join("test.db");
        let db_path_str = db_path.to_str().expect("utf-8 path");

        let chunk_id = {
            let db = Database::open(db_path_str).expect("open");
            db.verify_embedding_meta("bge-small-en-v1.5", 384)
                .expect("verify_embedding_meta");
            let doc_id = db
                .upsert_document(
                    "notes/level-none.md",
                    None,
                    None,
                    None,
                    None,
                    &[],
                    None,
                    "hash_level_none",
                    0,
                )
                .expect("upsert document");
            db.insert_chunk(
                doc_id,
                0,
                None,
                None,
                "body",
                None,
                &dummy_embedding(0.2),
                1.0,
            )
            .expect("insert chunk")
        };

        let conn = rusqlite::Connection::open(&db_path).expect("re-open");
        let level: Option<i64> = conn
            .query_row(
                "SELECT level FROM chunks WHERE id = ?1",
                rusqlite::params![chunk_id],
                |row| row.get(0),
            )
            .expect("select level");
        assert_eq!(level, None);
    }

    #[test]
    fn test_rrf_topk_tie_break_score_desc_id_asc() {
        // 同じ score を持つ複数 chunk_id がある場合、id ASC で安定 sort される
        use std::collections::HashMap;
        let mut scores: HashMap<i64, f32> = HashMap::new();
        scores.insert(3, 0.5);
        scores.insert(1, 0.5);
        scores.insert(2, 0.7); // top
        scores.insert(5, 0.5);
        let mut rows: HashMap<i64, SearchResult> = HashMap::new();
        for &id in &[1, 2, 3, 5] {
            rows.insert(
                id,
                SearchResult {
                    score: 0.0,
                    content: format!("c{id}"),
                    heading: None,
                    document_id: 0,
                    path: format!("p{id}"),
                    title: None,
                    topic: None,
                    date: None,
                    tags: vec![],
                    context_text: None,
                },
            );
        }
        let result = rrf_topk(scores, rows, Some(10));
        let ids: Vec<i64> = result.iter().map(|(id, _)| *id).collect();
        // top: id=2 (0.7), 同 score 0.5 は id ASC = 1, 3, 5
        assert_eq!(ids, vec![2, 1, 3, 5]);
    }

    #[test]
    fn test_rrf_topk_no_truncation_when_limit_none() {
        use std::collections::HashMap;
        let mut scores: HashMap<i64, f32> = HashMap::new();
        for id in 1..=10 {
            scores.insert(id, 1.0 / id as f32);
        }
        let mut rows: HashMap<i64, SearchResult> = HashMap::new();
        for id in 1..=10 {
            rows.insert(
                id,
                SearchResult {
                    score: 0.0,
                    content: format!("c{id}"),
                    heading: None,
                    document_id: 0,
                    path: format!("p{id}"),
                    title: None,
                    topic: None,
                    date: None,
                    tags: vec![],
                    context_text: None,
                },
            );
        }
        let result = rrf_topk(scores, rows, None);
        assert_eq!(result.len(), 10, "limit=None should not truncate");
    }

    #[test]
    fn test_expanded_range_serializes_with_kind_tag() {
        let adj = ExpandedRange::Adjacent {
            from_index: 1,
            to_index: 3,
        };
        let json = serde_json::to_string(&adj).unwrap();
        assert!(
            json.contains(r#""kind":"adjacent""#),
            "kind tag missing: {json}"
        );
        assert!(json.contains(r#""from_index":1"#));
        assert!(json.contains(r#""to_index":3"#));
    }

    #[test]
    fn test_expanded_range_whole_document_serializes() {
        let wd = ExpandedRange::WholeDocument { total_chunks: 7 };
        let json = serde_json::to_string(&wd).unwrap();
        assert!(
            json.contains(r#""kind":"whole_document""#),
            "kind tag missing: {json}"
        );
        assert!(json.contains(r#""total_chunks":7"#));
    }

    #[test]
    fn test_search_hit_expanded_from_omitted_when_none() {
        let hit = SearchHit {
            score: 1.0,
            path: "p".into(),
            title: None,
            heading: None,
            topic: None,
            date: None,
            tags: vec![],
            content: "c".into(),
            match_spans: None,
            expanded_from: None,
        };
        let json = serde_json::to_string(&hit).unwrap();
        assert!(
            !json.contains("expanded_from"),
            "None should omit field, got: {json}"
        );
    }

    #[test]
    fn test_token_count_saturates_at_i32_max() {
        // F-46 PR-2: 8 GiB+ content (現実には不発生だが defense-in-depth) で
        // 旧 (content.len() / 4) as i32 reinterpret cast は wrap、
        // 新 i32::try_from(...).unwrap_or(i32::MAX) は saturate。
        // production code は呼ばず、本 test は cast 挙動だけを直接 assert する
        // (F-29 / F-49 helper test と同じ pattern)。
        let huge_len: usize = i32::MAX as usize + 1;
        let result = i32::try_from(huge_len).unwrap_or(i32::MAX);
        assert_eq!(result, i32::MAX, "must saturate, not wrap");

        let normal_len: usize = 1024;
        let normal_result = i32::try_from(normal_len).unwrap_or(i32::MAX);
        assert_eq!(normal_result, 1024_i32);
    }

    proptest! {
        /// F-65: rrf_topk が任意 input に対して **score DESC + id ASC** の deterministic
        /// total order を返すことを fixation する。HashMap iteration の非決定性に依存
        /// しないことを保証 (invariant #1)。
        ///
        /// generator:
        /// - `entries`: `Vec<(i64, f32)>`、id は重複可だが HashMap で deduped、score は finite f32 のみ
        /// - `limit`: `Option<u32>`、None / Some(0..=200) を生成
        ///
        /// `partial_cmp` の NaN は `unwrap_or(Ordering::Equal)` で degraded されるが、本 test
        /// は finite f32 のみ generate するため NaN 道は踏まない (= 別 test corpus で扱う想定)。
        #[test]
        fn prop_rrf_topk_total_order_stable(
            entries in prop::collection::vec(
                (any::<i64>(), prop::num::f32::ANY.prop_filter("finite", |x| x.is_finite())),
                0..50,
            ),
            limit in prop::option::of(0u32..=200u32),
        ) {
            let scores: HashMap<i64, f32> = entries.iter().copied().collect();
            let rows: HashMap<i64, SearchResult> = scores.keys()
                .map(|&id| (id, dummy_search_result_for_id(id)))
                .collect();

            let result = rrf_topk(scores.clone(), rows, limit);

            // 1. score DESC + id ASC の total order を verify
            for window in result.windows(2) {
                let (a_id, a) = &window[0];
                let (b_id, b) = &window[1];
                let a_score = scores[a_id];
                let b_score = scores[b_id];
                prop_assert!(
                    a.score > b.score
                        || (a.score == b.score && a_id < b_id),
                    "ordering violated: ({}, {}) vs ({}, {}) scores=({}, {})",
                    a_id, a.score, b_id, b.score, a_score, b_score
                );
            }

            // 2. limit constraint
            if let Some(n) = limit {
                prop_assert!(result.len() <= n as usize);
            }

            // 3. result の score field が input score と一致 (rrf overwrite)
            for (id, r) in &result {
                prop_assert_eq!(r.score, scores[id]);
            }

            // 4. result の id 集合が input scores の **subset** (limit 適用後の任意の n 件)
            let result_ids: std::collections::HashSet<i64> = result.iter().map(|(id, _)| *id).collect();
            let scores_ids: std::collections::HashSet<i64> = scores.keys().copied().collect();
            prop_assert!(result_ids.is_subset(&scores_ids));
        }
    }

    // -----------------------------------------------------------------------
    // F-63: tags_parse_failures counter tests
    // -----------------------------------------------------------------------

    /// `tempfile` crate を避けるための file-internal temp dir helper
    /// (= CLAUDE.local.md 規約)。`std::env::temp_dir()` + PID + nanos で
    /// unique path を生成し、`Drop` で `remove_dir_all` cleanup する。
    struct F63TempDir {
        path: std::path::PathBuf,
    }

    impl F63TempDir {
        fn new(prefix: &str) -> Self {
            let path = crate::test_support::unique_temp_path(prefix);
            std::fs::create_dir_all(&path).expect("create temp dir");
            Self { path }
        }
    }

    impl Drop for F63TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    #[test]
    fn test_parse_tags_failure_counter_increments_on_malformed_json() {
        let db = Database::open_in_memory().unwrap();
        // counter は初期 0
        assert_eq!(db.tags_parse_failure_count(), 0);

        // malformed JSON を直接 method に渡して increment を観測
        let _ = db.parse_tags_json_recording(Some("not-a-json".into()));
        assert_eq!(db.tags_parse_failure_count(), 1);

        // もう 1 件 malformed → 2
        let _ = db.parse_tags_json_recording(Some("{broken".into()));
        assert_eq!(db.tags_parse_failure_count(), 2);
    }

    #[test]
    fn test_parse_tags_failure_counter_zero_for_valid_json() {
        let db = Database::open_in_memory().unwrap();
        assert_eq!(db.tags_parse_failure_count(), 0);

        // valid JSON
        let v = db.parse_tags_json_recording(Some(r#"["mcp","rust"]"#.into()));
        assert_eq!(v, vec!["mcp".to_string(), "rust".to_string()]);
        assert_eq!(db.tags_parse_failure_count(), 0);

        // NULL (None) も failure ではない
        let v = db.parse_tags_json_recording(None);
        assert!(v.is_empty());
        assert_eq!(db.tags_parse_failure_count(), 0);

        // 空文字も failure ではない
        let v = db.parse_tags_json_recording(Some(String::new()));
        assert!(v.is_empty());
        assert_eq!(db.tags_parse_failure_count(), 0);
    }

    #[test]
    fn test_parse_tags_failure_counter_persists_across_sessions() {
        let tmp = F63TempDir::new("kb-mcp-f63-persist");
        let db_path = tmp.path.join("kb.sqlite");
        let db_path_str = db_path.to_string_lossy().to_string();

        // session 1: counter を 5 に bump して drop (= flush)
        {
            let db = Database::open(&db_path_str).expect("open session 1");
            for _ in 0..5 {
                let _ = db.parse_tags_json_recording(Some("{malformed".into()));
            }
            assert_eq!(db.tags_parse_failure_count(), 5);
            // drop で index_meta に flush
        }

        // session 2: 再 open で前 session の値が復元される
        {
            let db = Database::open(&db_path_str).expect("open session 2");
            assert_eq!(
                db.tags_parse_failure_count(),
                5,
                "tags_parse_failures should be restored from index_meta after re-open"
            );

            // session 2 で +2 して合計 7
            let _ = db.parse_tags_json_recording(Some("[".into()));
            let _ = db.parse_tags_json_recording(Some("[".into()));
            assert_eq!(db.tags_parse_failure_count(), 7);
        }

        // session 3: 累計が伝播していること
        {
            let db = Database::open(&db_path_str).expect("open session 3");
            assert_eq!(db.tags_parse_failure_count(), 7);
        }
    }

    /// codex P2 regression catcher (PR #53): 同一 SQLite file を 2 つの `Database`
    /// instance が同時に open し、それぞれが独立に increment した場合、両 instance
    /// が drop された後の **再 open 値が両者の delta の和** であることを確認する。
    ///
    /// 旧設計 (= startup restore + `INSERT OR REPLACE` flush) では last-writer-wins で
    /// 後 drop した instance が前者の delta を上書きしていた。新設計 (= session-local
    /// delta + UPSERT atomic add) ではこれが起こらない。
    #[test]
    fn test_parse_tags_failure_counter_concurrent_instances_atomic_add() {
        let tmp = F63TempDir::new("kb-mcp-f63-concurrent");
        let db_path = tmp.path.join("kb.sqlite");
        let db_path_str = db_path.to_string_lossy().to_string();

        // pre-seed: index_meta に既存値 10 を持っている状態を simulate
        // (= 過去 session の累計が DB に残っている state を再現)
        {
            let db = Database::open(&db_path_str).expect("open seed");
            for _ in 0..10 {
                let _ = db.parse_tags_json_recording(Some("seed".into()));
            }
            assert_eq!(db.tags_parse_failure_count(), 10);
            // drop で 10 が `index_meta` に flush される
        }

        // 2 つの instance を同時に open し、独立に増分を持たせる
        let db_a = Database::open(&db_path_str).expect("open A");
        let db_b = Database::open(&db_path_str).expect("open B");

        // どちらも startup 値 10 を見ている
        assert_eq!(db_a.tags_parse_failure_count(), 10);
        assert_eq!(db_b.tags_parse_failure_count(), 10);

        // A: +3、B: +5 をそれぞれ独立に increment
        for _ in 0..3 {
            let _ = db_a.parse_tags_json_recording(Some("a".into()));
        }
        for _ in 0..5 {
            let _ = db_b.parse_tags_json_recording(Some("b".into()));
        }

        // それぞれ自セッションでは「永続 10 + 自 delta」を見る (= 他 instance の
        // delta は flush 前なので見えない、これは設計上の許容範囲)
        assert_eq!(db_a.tags_parse_failure_count(), 13);
        assert_eq!(db_b.tags_parse_failure_count(), 15);

        // 両者を drop (= 順序問わず両 delta が atomic add で flush される)
        drop(db_a);
        drop(db_b);

        // 再 open して累計を確認: 10 (seed) + 3 (A delta) + 5 (B delta) = 18
        // **これが旧設計では last-writer-wins で 13 or 15 にしかならなかった**
        let db_final = Database::open(&db_path_str).expect("open final");
        assert_eq!(
            db_final.tags_parse_failure_count(),
            18,
            "concurrent delta must be additively merged (no last-writer-wins)"
        );
    }

    // -- feature-46 PR-2 Task 2.2: FTS 3 列 migration ------------------------

    /// 旧 2 列 FTS schema の DB file を作る (v0.11.0 相当)。chunks に context_text
    /// 列はあり (PR-1 適用済み想定) だが FTS は 2 列 = PR-2 未適用状態を再現する。
    ///
    /// **brief からの逸脱 (main 承認済み)**: `PRAGMA journal_mode = WAL;` を明示的に
    /// 先行させる。kb-mcp が作成した DB は `Database::init()` が必ず最初に journal_mode
    /// を WAL へ切り替えて永続化するため、実運用では「一度でも kb-mcp が open した DB」
    /// は常に WAL 状態にある。ここで WAL を先に設定しないと `test_fts_migration_waits_out_concurrent_write_lock`
    /// が「migration の BEGIN IMMEDIATE 待機」ではなく「非 WAL→WAL の journal_mode 切替」
    /// で落ちてしまう (詳細は当該テストの NOTE を参照)。
    fn create_legacy_2col_fts_db(path: &str) {
        let conn = rusqlite::Connection::open(path).unwrap();
        conn.execute_batch("PRAGMA journal_mode = WAL;").unwrap();
        conn.execute_batch(
            "CREATE TABLE index_meta (key TEXT PRIMARY KEY, value TEXT);
             CREATE TABLE documents (id INTEGER PRIMARY KEY AUTOINCREMENT, path TEXT UNIQUE NOT NULL,
                title TEXT, topic TEXT, category TEXT, depth TEXT, tags TEXT, date TEXT,
                content_hash TEXT NOT NULL, last_indexed TEXT NOT NULL);
             CREATE TABLE chunks (id INTEGER PRIMARY KEY AUTOINCREMENT, document_id INTEGER NOT NULL,
                chunk_index INTEGER NOT NULL, heading TEXT, level INTEGER, content TEXT NOT NULL,
                token_count INTEGER, quality_score REAL NOT NULL DEFAULT 1.0, context_text TEXT);
             CREATE VIRTUAL TABLE fts_chunks USING fts5(heading, content, content='',
                contentless_delete=1, tokenize=\"trigram remove_diacritics 1 case_sensitive 0\");
             INSERT INTO documents (path, title, content_hash, last_indexed)
                VALUES ('a.md', 'A', 'h', '2026-01-01T00:00:00Z');
             INSERT INTO chunks (document_id, chunk_index, heading, content, context_text)
                VALUES (1, 0, 'H', 'body text here', 'A > H');
             INSERT INTO fts_chunks (rowid, heading, content) VALUES (1, 'H', 'body text here');",
        )
        .unwrap();
    }

    /// db.conn (private field) から fts_chunks が context 列を持つか判定する test helper。
    fn fts_has_context_col(db: &Database) -> bool {
        db.conn
            .prepare("PRAGMA table_info(fts_chunks)")
            .unwrap()
            .query_map([], |r| r.get::<_, String>(1))
            .unwrap()
            .filter_map(Result::ok)
            .any(|n| n == "context")
    }

    #[test]
    fn test_fts_migration_adds_context_column_and_repopulates() {
        let dir = TempDir::new("fts-migrate");
        let path = dir.path().join("k.db");
        let path_str = path.to_string_lossy().to_string();
        create_legacy_2col_fts_db(&path_str);
        // open → init が migration を走らせる
        let db = Database::open(&path_str).unwrap();
        assert!(
            fts_has_context_col(&db),
            "context column must exist after migration"
        );
        // repopulate: 既存 chunk が FTS に残っていること
        let cnt: i64 = db
            .conn
            .query_row("SELECT COUNT(*) FROM fts_chunks", [], |r| r.get(0))
            .unwrap();
        assert_eq!(cnt, 1);
        // context 列に 'A > H' が index されていること (MATCH でヒット)
        let hit: i64 = db
            .conn
            .query_row(
                "SELECT COUNT(*) FROM fts_chunks WHERE fts_chunks MATCH 'context : \"A > H\"'",
                [],
                |r| r.get(0),
            )
            .unwrap_or(0);
        assert!(hit >= 1, "context text must be searchable after repopulate");
    }

    #[test]
    fn test_fts_migration_idempotent_noop_second_open() {
        let dir = TempDir::new("fts-noop");
        let path = dir.path().join("k.db");
        let path_str = path.to_string_lossy().to_string();
        create_legacy_2col_fts_db(&path_str);
        let db1 = Database::open(&path_str).unwrap();
        assert!(fts_has_context_col(&db1));
        drop(db1);
        // 2 回目 open: table_info ガードで no-op (double-checked)
        let db2 = Database::open(&path_str).unwrap();
        assert!(fts_has_context_col(&db2));
    }

    #[test]
    fn test_fts_migration_waits_out_concurrent_write_lock() {
        // §10 確定 #4: mpsc 2 本で「holder が RESERVED lock 保持」→「opener が
        // open 試行」→「holder release」を決定的に順序付け。busy_timeout=30s 内の
        // 待機後に open が成功する (即 SQLITE_BUSY にならない) ことを検証。
        //
        // NOTE: holder は生 rusqlite::Connection + 手動 `BEGIN IMMEDIATE` で write
        // lock を握る (= migration の ensure_fts_context_column が実際に発行する
        // BEGIN IMMEDIATE との「同種ロック同士の競合」を厳密に再現するわけではない)。
        // 本テストが確かめているのは「busy_timeout を設定した接続が、他接続の write
        // lock 保持中でも待機して成功する」という busy_timeout 全般の待機動作であり、
        // migration の double-checked locking の正しさ自体は
        // test_fts_migration_idempotent_noop_second_open (再チェック no-op) で担保する。
        // 既知の隙間: 「lock 待機中に他プロセスが migration を完了し、lock 取得後の
        // 再チェックで no-op commit になる」真の競合 double-checked path は 3 テスト
        // (migrate / idempotent-noop / この lock-wait) のいずれも直接は再現していない。
        // 機能の正しさは逐次 no-op テスト + tx (DDL) の原子性で担保しており、この
        // 競合 path 専用の deterministic 再現は複雑さに見合わないと判断した。
        //
        // NOTE (fixture が WAL を事前設定する理由、実装中に発見): 非 WAL→WAL の
        // journal_mode 切替は SQLite 側で exclusive lock を要求し、busy_timeout /
        // busy handler を一切無視して即座に SQLITE_BUSY を返す。`create_legacy_2col_fts_db`
        // が journal_mode を明示せず rollback-journal のまま DB を作ると、opener の
        // `Database::open` が init() 冒頭の `PRAGMA journal_mode = WAL;` の時点で
        // (holder が RESERVED lock を保持している間) 即座に失敗し、本テストが本来
        // 検証したい `begin_immediate_tx` (BEGIN IMMEDIATE) の待機ロジックに到達すらしない。
        // kb-mcp が作成した DB は初回 open で WAL がファイルヘッダに永続化される
        // ("kb-mcp が一度でも open した DB は常に WAL" が実運用の不変条件) ため、
        // fixture 側で WAL を事前設定することが実運用条件に忠実な再現になる。次にこの
        // テストを触る人が同じ切り分けを繰り返さないための記録。
        let dir = TempDir::new("fts-lock");
        let path = dir.path().join("k.db");
        let path_str = path.to_string_lossy().to_string();
        create_legacy_2col_fts_db(&path_str);

        let (tx_locked, rx_locked) = std::sync::mpsc::channel::<()>();
        let (tx_release, rx_release) = std::sync::mpsc::channel::<()>();

        let holder_path = path_str.clone();
        let holder = std::thread::spawn(move || {
            let conn = rusqlite::Connection::open(&holder_path).unwrap();
            conn.busy_timeout(std::time::Duration::from_secs(10))
                .unwrap();
            // RESERVED write lock を実際に取る (INSERT で write intent)
            conn.execute_batch(
                "BEGIN IMMEDIATE; INSERT INTO index_meta (key, value) VALUES ('lock_probe', '1');",
            )
            .unwrap();
            tx_locked.send(()).unwrap(); // ロック保持を通知
            rx_release.recv().unwrap(); // release 指示を待つ
            conn.execute_batch("COMMIT;").unwrap();
        });

        rx_locked.recv().unwrap(); // holder が write lock を取るまで待つ
        let opener_path = path_str.clone();
        let opener = std::thread::spawn(move || Database::open(&opener_path));
        // opener が migration の BEGIN IMMEDIATE で block するのを少し待ってから release。
        std::thread::sleep(std::time::Duration::from_millis(300));
        tx_release.send(()).unwrap();
        holder.join().unwrap();

        let db = opener
            .join()
            .unwrap()
            .expect("open must succeed after lock released within busy_timeout");
        assert!(
            fts_has_context_col(&db),
            "migration must complete after lock wait"
        );
    }

    #[test]
    fn test_fusion_params_default_matches_legacy_constants() {
        // feature-47: config 化前のコンパイル時定数 (RRF_K=60.0 /
        // FTS_BM25_HEADING=2.0 / CONTEXT=1.0 / CONTENT=1.0) と
        // FusionParams::default() が完全一致することを固定する。
        // この既定値がずれると PR-1 の behavior-invariant 前提が崩れる。
        let f = FusionParams::default();
        assert_eq!(f.rrf_k, 60.0);
        assert_eq!(f.bm25_heading_weight, 2.0);
        assert_eq!(f.bm25_context_weight, 1.0);
        assert_eq!(f.bm25_content_weight, 1.0);
        // Copy + PartialEq が derive されていること (db API で値渡しするため)
        let g = f;
        assert_eq!(f, g);
    }

    #[test]
    fn test_fuse_rrf_matches_legacy_rrf_topk() {
        // feature-47 D-5: 括り出した fuse_rrf が、旧 inline 実装
        // (RRF ループ + rrf_topk) と同一の (chunk_id, score) 列を返すこと。
        // rrf_topk は #[cfg(test)] の oracle として残してある。
        let vec_hits: Vec<(i64, SearchResult)> = [3_i64, 1, 7, 2]
            .iter()
            .map(|id| (*id, dummy_search_result_for_id(*id)))
            .collect();
        let fts_hits: Vec<(i64, SearchResult)> = [7_i64, 5, 1]
            .iter()
            .map(|id| (*id, dummy_search_result_for_id(*id)))
            .collect();

        for limit in [None, Some(1_u32), Some(3), Some(100)] {
            // 旧 inline 実装をその場で再現する (db.rs:1371-1383 と同形)。
            let mut scores: HashMap<i64, f32> = HashMap::new();
            let mut rows: HashMap<i64, SearchResult> = HashMap::new();
            for (rank, (chunk_id, row)) in vec_hits.clone().into_iter().enumerate() {
                *scores.entry(chunk_id).or_insert(0.0) += 1.0 / (60.0 + (rank as f32) + 1.0);
                rows.entry(chunk_id).or_insert(row);
            }
            for (rank, (chunk_id, row)) in fts_hits.clone().into_iter().enumerate() {
                *scores.entry(chunk_id).or_insert(0.0) += 1.0 / (60.0 + (rank as f32) + 1.0);
                rows.entry(chunk_id).or_insert(row);
            }
            let legacy = rrf_topk(scores, rows, limit);
            let fused = fuse_rrf(&vec_hits, &fts_hits, 60.0, limit);

            let legacy_pairs: Vec<(i64, f32)> =
                legacy.iter().map(|(id, r)| (*id, r.score)).collect();
            let fused_pairs: Vec<(i64, f32)> = fused.iter().map(|(id, r)| (*id, r.score)).collect();
            assert_eq!(
                legacy_pairs, fused_pairs,
                "fuse_rrf must match the legacy rrf_topk path for limit={limit:?}"
            );
            // row の対応も一致すること (両リスト掲載 id は vec 側の row を採る)
            let legacy_paths: Vec<String> = legacy.iter().map(|(_, r)| r.path.clone()).collect();
            let fused_paths: Vec<String> = fused.iter().map(|(_, r)| r.path.clone()).collect();
            assert_eq!(legacy_paths, fused_paths, "row selection must match");
        }
    }

    #[test]
    fn test_fuse_rrf_ids_is_rank_only() {
        // rrf_k を変えても vec/fts の rank list さえあれば融合できること
        // (= tune がメモリ内で rrf_k を掃ける前提)。
        let vec_ids = [10_i64, 20, 30];
        let fts_ids = [30_i64, 40];

        let k60 = fuse_rrf_ids(&vec_ids, &fts_ids, 60.0, None);
        let k5 = fuse_rrf_ids(&vec_ids, &fts_ids, 5.0, None);

        // 両リスト掲載の 30 (vec rank 2 / fts rank 0) は合意ボーナスで 1 位を取る。
        // k=60: 1/62 + 1/61 = 0.0325 vs vec 1 位 (10) の 1/61 = 0.0164
        // k=5:  1/8  + 1/6  = 0.2917 vs vec 1 位 (10) の 1/6  = 0.1667
        // どちらの k でも 30 が 1 位 = **順位は変わらないがスコアの絶対値は
        // 大きく変わる**。これが「rrf_k はメモリ内で掃ける」ことの根拠になる。
        assert_eq!(k60[0].0, 30, "consensus doc wins at k=60: {k60:?}");
        assert_eq!(k5[0].0, 30, "consensus doc still wins at k=5: {k5:?}");
        assert!(
            k5[0].1 > k60[0].1,
            "smaller k must produce larger reciprocal-rank scores: {k5:?} vs {k60:?}"
        );
        // limit truncate
        let truncated = fuse_rrf_ids(&vec_ids, &fts_ids, 60.0, Some(2));
        assert_eq!(truncated.len(), 2);
    }

    #[test]
    fn test_fts_bm25_weights_are_bound_and_effective() {
        // feature-47 D-4: bm25 重みを番号付き bind parameter (?3/?4/?5) で
        // 渡す経路が生きていること。heading にだけ語を置いた doc と content
        // にだけ置いた doc を作り、heading 重みを振ると順位が入れ替わる。
        let db = db_with_384();
        let doc_a = db
            .upsert_document("a.md", Some("A"), None, None, None, &[], None, "ha", 0)
            .unwrap();
        db.insert_chunk(
            doc_a,
            0,
            Some("zebrafish"),
            None,
            "filler body text about nothing in particular",
            None,
            &dummy_embedding(0.2),
            1.0,
        )
        .unwrap();
        let doc_b = db
            .upsert_document("b.md", Some("B"), None, None, None, &[], None, "hb", 0)
            .unwrap();
        db.insert_chunk(
            doc_b,
            0,
            Some("unrelated heading"),
            None,
            "zebrafish zebrafish zebrafish in the body",
            None,
            &dummy_embedding(0.8),
            1.0,
        )
        .unwrap();

        let heading_heavy = FusionParams {
            bm25_heading_weight: 8.0,
            bm25_content_weight: 0.5,
            ..FusionParams::default()
        };
        let content_heavy = FusionParams {
            bm25_heading_weight: 0.5,
            bm25_content_weight: 8.0,
            ..FusionParams::default()
        };

        let h = db
            .search_fts_candidates("zebrafish", 10, &SearchFilters::default(), heading_heavy)
            .unwrap();
        let c = db
            .search_fts_candidates("zebrafish", 10, &SearchFilters::default(), content_heavy)
            .unwrap();
        assert_eq!(h.len(), 2, "both docs must match the phrase");
        assert_eq!(c.len(), 2);
        assert_ne!(
            h[0].1.path, c[0].1.path,
            "heading-heavy and content-heavy weights must pick different top hits"
        );
    }

    /// Regression (full-audit 2026-07-26): over-fetch 後の `fetch_k` は
    /// sqlite-vec の KNN 上限 (4096) を超えてはならない。超えると
    /// "k value in knn query too large" の SQL error になる。
    /// 既定の `min_quality = 0.3` は `has_any()` を true にするため、
    /// released v0.13.0 では **`--limit 82` 以上の検索が全て失敗**していた
    /// (82 * FILTER_OVERFETCH_FACTOR(10) * 5 = 4100 > 4096)。
    #[test]
    fn test_vec_candidates_clamp_fetch_k_to_sqlite_vec_limit() {
        let db = db_with_384();
        let doc = db
            .upsert_document("a.md", Some("A"), None, None, None, &[], None, "h", 0)
            .unwrap();
        db.insert_chunk(
            doc,
            0,
            None,
            None,
            "hello",
            None,
            &dummy_embedding(0.1),
            1.0,
        )
        .unwrap();

        // filter あり (min_quality > 0.0) で over-fetch を発動させ、
        // FILTER_OVERFETCH_CAP (10_000) まで膨らむ limit を渡す。
        let filters = SearchFilters {
            min_quality: 0.5,
            ..Default::default()
        };
        let hits = db
            .search_vec_candidates(&dummy_embedding(0.1), 100_000, &filters)
            .expect("fetch_k must be clamped below the sqlite-vec KNN limit");
        assert!(hits.len() <= VEC_KNN_MAX_K as usize);
    }

    #[test]
    fn test_fts_chunks_column_order_is_heading_context_content() {
        // feature-47 E-4: bm25(fts_chunks, ?3, ?4, ?5) の 3 引数は fts_chunks が
        // (heading, context, content) の 3 列であることに束縛されている。
        // FTS5 は重み個数のミスマッチを silent に処理する (不足は 1.0 補完 /
        // 過剰は無視) ので、init() の無条件 migration が保つこの不変条件を
        // 回帰テストで固定する。
        let db = db_with_384();
        let cols: Vec<String> = db
            .conn
            .prepare("PRAGMA table_info(fts_chunks)")
            .unwrap()
            .query_map([], |row| row.get::<_, String>(1))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        assert_eq!(
            cols,
            vec![
                "heading".to_string(),
                "context".to_string(),
                "content".to_string()
            ],
            "fts_chunks column order is load-bearing for the bm25 weight positions"
        );
    }

    // -----------------------------------------------------------------------
    // AU-71: corpus_snapshot
    // -----------------------------------------------------------------------

    /// Insert one document with one chunk. Returns the document id.
    fn doc_with_chunk(db: &Database, path: &str, content_hash: &str, chunk: &str) -> i64 {
        let id = db
            .upsert_document(
                path,
                Some("t"),
                None,
                None,
                None,
                &[],
                None,
                content_hash,
                0,
            )
            .unwrap();
        db.insert_chunk(id, 0, Some("H"), Some(1), chunk, None, &[0.0; 384], 1.0)
            .unwrap();
        id
    }

    /// The digest must be stable across repeated reads of one unchanged index.
    ///
    /// If it were not, every run would report "the corpus changed", which reads
    /// as noise and gets ignored — defeating the point of recording it. The
    /// row order therefore comes from SQL `ORDER BY`, not from whatever the
    /// query plan happens to produce.
    #[test]
    fn test_corpus_snapshot_is_stable_for_an_unchanged_index() {
        let db = db_with_384();
        doc_with_chunk(&db, "b.md", "hb", "beta text");
        doc_with_chunk(&db, "a.md", "ha", "alpha text");

        let first = db.corpus_snapshot().unwrap();
        let second = db.corpus_snapshot().unwrap();
        assert_eq!(first, second);
        assert_eq!(first.0, 2, "documents");
        assert_eq!(first.1, 2, "chunks");
    }

    /// The digest must follow the *indexed chunk*, not the source file hash.
    ///
    /// `documents.content_hash` hashes file bytes, so it cannot see a rebuild
    /// that parsed the same bytes differently (a changed `exclude_headings`,
    /// say). If the chunk count happens to match as well, a source-hash digest
    /// would call that "unchanged" while every chunk being searched had been
    /// replaced.
    #[test]
    fn test_corpus_snapshot_notices_chunk_text_changing_under_an_identical_source_hash() {
        let db = db_with_384();
        doc_with_chunk(&db, "a.md", "same-source-hash", "original chunk body");
        let before = db.corpus_snapshot().unwrap();

        // Same path, same content_hash, same chunk count — only the indexed
        // text differs, exactly as a re-parse under new settings would leave it.
        let db2 = db_with_384();
        doc_with_chunk(&db2, "a.md", "same-source-hash", "replaced chunk body");
        let after = db2.corpus_snapshot().unwrap();

        assert_eq!(
            (before.0, before.1),
            (after.0, after.1),
            "counts must match"
        );
        assert_ne!(before.2, after.2, "the digest must still notice");
    }

    /// Adjacent fields must not be able to trade characters across the join.
    #[test]
    fn test_corpus_snapshot_separates_adjacent_fields() {
        let a = db_with_384();
        doc_with_chunk(&a, "ab.md", "h", "cd");
        let b = db_with_384();
        doc_with_chunk(&b, "a.md", "h", "bcd");
        assert_ne!(
            a.corpus_snapshot().unwrap().2,
            b.corpus_snapshot().unwrap().2
        );
    }

    /// ...including when the indexed text itself contains the byte a
    /// separator-based framing would have used.
    ///
    /// NUL is a valid UTF-8 character, so a delimiter scheme stops being
    /// unambiguous the moment it appears in the data: with `\0` separators,
    /// `(heading "x", content "\0b")` and `(heading "x\0", content "b")` feed
    /// the hasher identical bytes, and two corpora holding different
    /// searchable text would report as unchanged. Length prefixes assume
    /// nothing about which bytes the data can hold.
    #[test]
    fn test_corpus_snapshot_frames_fields_even_when_text_contains_nul() {
        let nul = '\u{0}';
        let a = db_with_384();
        let id_a = a
            .upsert_document("a.md", Some("t"), None, None, None, &[], None, "h", 0)
            .unwrap();
        a.insert_chunk(
            id_a,
            0,
            Some("x"),
            Some(1),
            &format!("{nul}b"),
            None,
            &[0.0; 384],
            1.0,
        )
        .unwrap();

        let b = db_with_384();
        let id_b = b
            .upsert_document("a.md", Some("t"), None, None, None, &[], None, "h", 0)
            .unwrap();
        b.insert_chunk(
            id_b,
            0,
            Some(&format!("x{nul}")),
            Some(1),
            "b",
            None,
            &[0.0; 384],
            1.0,
        )
        .unwrap();

        // Guard the guard: if SQLite had truncated at the NUL, both rows would
        // be identical and the real assertion would pass for the wrong reason.
        let read = |db: &Database| -> String {
            db.conn
                .query_row("SELECT heading, content FROM chunks", [], |r| {
                    Ok(format!(
                        "{:?}|{:?}",
                        r.get::<_, Option<String>>(0)?,
                        r.get::<_, String>(1)?
                    ))
                })
                .unwrap()
        };
        assert_ne!(
            read(&a),
            read(&b),
            "the DB must have kept both NUL placements for this test to mean anything"
        );

        assert_ne!(
            a.corpus_snapshot().unwrap().2,
            b.corpus_snapshot().unwrap().2
        );
    }

    /// A missing heading and an empty heading are different indexed states, and
    /// the digest must not collapse them.
    ///
    /// They are also observably different downstream: `eval::is_hit` scores
    /// `(Some(_), None)` as a miss but `(Some(""), Some(""))` as a hit, so a
    /// golden entry with an empty heading changes recall across this edit while
    /// counts, content and context all hold. Collapsing the two with
    /// `unwrap_or("")` would leave `corpus_changed` false for a change that
    /// moved the numbers.
    #[test]
    fn test_corpus_snapshot_distinguishes_a_missing_heading_from_an_empty_one() {
        let a = db_with_384();
        let id_a = a
            .upsert_document("a.md", Some("t"), None, None, None, &[], None, "h", 0)
            .unwrap();
        a.insert_chunk(id_a, 0, None, Some(1), "body", None, &[0.0; 384], 1.0)
            .unwrap();

        let b = db_with_384();
        let id_b = b
            .upsert_document("a.md", Some("t"), None, None, None, &[], None, "h", 0)
            .unwrap();
        b.insert_chunk(id_b, 0, Some(""), Some(1), "body", None, &[0.0; 384], 1.0)
            .unwrap();

        let (sa, sb) = (a.corpus_snapshot().unwrap(), b.corpus_snapshot().unwrap());
        assert_eq!((sa.0, sa.1), (sb.0, sb.1), "counts are identical by design");
        assert_ne!(sa.2, sb.2, "the digest must still tell them apart");
    }

    /// `corpus_snapshot` must join a caller-held transaction rather than open
    /// its own.
    ///
    /// `eval::run` pins the whole evaluation — every search plus this read — to
    /// one snapshot, so that the numbers and the index they are recorded
    /// against cannot come from different commits while a watcher indexes
    /// alongside. SQLite has no true nested transaction, so a `corpus_snapshot`
    /// that always opened one would either error or silently end the caller's,
    /// releasing the very snapshot being held.
    /// (BU-18) The repair itself. `mem::forget` reproduces the state a failed
    /// `Drop`-time `ROLLBACK` leaves: the handle is gone, the transaction is
    /// not. Every `&self` write from here on would join it, and nobody would
    /// ever commit it.
    #[test]
    fn a_transaction_left_open_is_rolled_back_and_the_check_is_idempotent() {
        let db = db_with_384();
        assert!(
            !db.rollback_if_transaction_open().unwrap(),
            "a healthy connection has nothing to roll back"
        );

        std::mem::forget(db.begin_transaction().unwrap());
        assert!(
            !db.conn.is_autocommit(),
            "precondition: the transaction outlived its handle"
        );

        assert!(
            db.rollback_if_transaction_open().unwrap(),
            "the open transaction must be reported as rolled back"
        );
        assert!(
            db.conn.is_autocommit(),
            "the connection must be usable again"
        );
        assert!(
            !db.rollback_if_transaction_open().unwrap(),
            "calling it twice must not report a second rollback"
        );
    }

    /// (BU-18) The repair through the path production takes: a thread panics
    /// while holding the lock, and the next lock acquisition has to give back a
    /// connection that is not stuck inside someone else's transaction.
    #[test]
    fn recovering_a_poisoned_db_lock_also_closes_the_transaction() {
        let shared = std::sync::Arc::new(std::sync::Mutex::new(db_with_384()));

        let handle = {
            let shared = std::sync::Arc::clone(&shared);
            std::thread::spawn(move || {
                let db = shared.lock().unwrap();
                // The transaction survives the unwind here for the same reason
                // a failed Drop-time ROLLBACK would leave it open.
                std::mem::forget(db.begin_transaction().unwrap());
                panic!("panicking with a transaction open");
            })
        };
        assert!(handle.join().is_err(), "the thread was supposed to panic");
        assert!(shared.is_poisoned());

        let db = crate::poison::recover_db(shared.lock());
        assert!(
            db.conn.is_autocommit(),
            "recovering the lock must not hand back a connection whose next \
             write disappears into an abandoned transaction"
        );
    }

    #[test]
    fn test_corpus_snapshot_joins_a_caller_held_transaction() {
        let db = db_with_384();
        doc_with_chunk(&db, "a.md", "h", "body");

        let tx = db.begin_transaction().unwrap();
        assert!(!db.conn.is_autocommit(), "the caller's tx must be open");

        let snapshot = db.corpus_snapshot().unwrap();
        assert_eq!(snapshot.0, 1);

        // Still inside the caller's transaction: the snapshot the caller is
        // holding must survive the call.
        assert!(
            !db.conn.is_autocommit(),
            "corpus_snapshot must not end the caller's transaction"
        );
        tx.rollback().unwrap();
        assert!(db.conn.is_autocommit());

        // ...and it still works standalone, where it opens its own.
        assert_eq!(db.corpus_snapshot().unwrap(), snapshot);
    }
}
