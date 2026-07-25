use anyhow::{Context, Result};
use rusqlite::{Connection, TransactionBehavior, params};
use std::collections::HashMap;
use std::sync::Once;
use std::sync::atomic::{AtomicU64, Ordering};

/// filter (category / topic) を Rust 側で適用する際の KNN / FTS の over-fetch 倍率。
/// filter が選択的な場合に target `limit` 件に届くよう多めに候補を取る。
const FILTER_OVERFETCH_FACTOR: u32 = 10;
const FILTER_OVERFETCH_CAP: u32 = 10_000;

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

/// Topic/category grouping returned by [`Database::list_topics`].
#[derive(Debug, Clone)]
pub struct TopicInfo {
    pub category: Option<String>,
    pub topic: Option<String>,
    pub file_count: u32,
    pub last_updated: Option<String>,
    pub titles: Vec<String>,
}

/// FTS5 クエリ用にユーザ入力をサニタイズする。
///
/// - trim 後に空、または 3 文字未満 (trigram の下限未満) なら `None` を返し
///   呼び出し側で vector-only にフォールバックさせる
/// - ダブルクォートを 2 連化してフレーズ全体をクォートで囲み、`AND` / `OR` /
///   `NOT` / `NEAR` / `*` / `:` 等の予約構文を中立化する
fn sanitize_fts_query(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed.chars().count() < 3 {
        return None;
    }
    let escaped = trimmed.replace('"', "\"\"");
    Some(format!("\"{escaped}\""))
}

/// `CREATE VIRTUAL TABLE ... USING vec0(... embedding float[384] ...)` 形式の
/// SQL から次元数を抽出する。失敗時は `None`。
fn parse_dim_from_create_sql(sql: &str) -> Option<u32> {
    let start = sql.find("float[")? + "float[".len();
    let rest = &sql[start..];
    let end = rest.find(']')?;
    rest[..end].trim().parse().ok()
}

/// `fts_chunks` に context 列があるか。`&Connection` を受けるので tx 内からも呼べる
/// (`rusqlite::Transaction` は `Deref<Target = Connection>` なので deref coercion で通る)。
fn fts_chunks_has_context_column_conn(conn: &Connection) -> Result<bool> {
    let has = conn
        .prepare("PRAGMA table_info(fts_chunks)")?
        .query_map([], |row| row.get::<_, String>(1))?
        .filter_map(std::result::Result::ok)
        .any(|name| name == "context");
    Ok(has)
}

/// `tags_any` フィルタ: hit の tags のいずれかが `any_pool` に含まれていれば pass。
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

    // -- private init --------------------------------------------------------

    fn init(&self) -> Result<()> {
        // WAL mode + foreign keys
        self.conn.execute_batch("PRAGMA journal_mode = WAL;")?;
        self.conn.execute_batch("PRAGMA foreign_keys = ON;")?;
        // feature-46: FTS 3 列 migration の repopulate は数秒〜十数秒 lock を保持する。
        // busy_timeout 未設定 (default 0) だと serve 常駐中の別プロセス search/status が
        // 即 SQLITE_BUSY で失敗する。30 秒待たせて migration 完了後に成功させる (spec §4.4)。
        // 10s→30s に引き上げ済み: dogfood KB (574 docs / 10,002 chunks) を embedding +
        // reranker モデル同時ロード中の並行負荷下で計測したところ migration が
        // 9.7〜12.3s かかり、4 trial 中 2 trial で旧 10s を実際に超過した実測に基づく
        // (`.dev/knowledge/eval-baseline-2026-07-20-context.md`)。
        self.conn
            .busy_timeout(std::time::Duration::from_millis(30_000))?;

        // vec_chunks は dim が未知の段階では作れないので遅延生成にする。
        // meta に dim が記録されていれば init 時に作るが、無ければ
        // `verify_embedding_meta` が実行時に決定した dim で作る。
        self.conn.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS index_meta (
                key   TEXT PRIMARY KEY,
                value TEXT
            );

            CREATE TABLE IF NOT EXISTS documents (
                id           INTEGER PRIMARY KEY AUTOINCREMENT,
                path         TEXT UNIQUE NOT NULL,
                title        TEXT,
                topic        TEXT,
                category     TEXT,
                depth        TEXT,
                tags         TEXT,
                date         TEXT,
                content_hash TEXT NOT NULL,
                last_indexed TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS chunks (
                id            INTEGER PRIMARY KEY AUTOINCREMENT,
                document_id   INTEGER NOT NULL REFERENCES documents(id) ON DELETE CASCADE,
                chunk_index   INTEGER NOT NULL,
                heading       TEXT,
                level         INTEGER,
                content       TEXT NOT NULL,
                token_count   INTEGER,
                quality_score REAL NOT NULL DEFAULT 1.0,
                context_text  TEXT
            );
            -- quality_score のインデックスは `ensure_quality_score_column` で
            -- 列存在保証の後にまとめて作成する (legacy DB は ALTER が
            -- 先に走る必要があるため、ここでは列だけ用意する)。
            ",
        )?;

        // FTS5 仮想テーブル: contentless + trigram tokenizer。
        // - contentless (content=''): chunks 側で本文を保持するのでメタ同期で十分
        // - contentless_delete=1: rowid 指定の DELETE を許可 (SQLite 3.43+)
        // - trigram: 日本語を含む任意言語で 3-gram ヒットが効く (SQLite 3.34+)
        // - rowid = chunks.id で統一 (INSERT 時に明示)
        self.conn.execute_batch(
            "CREATE VIRTUAL TABLE IF NOT EXISTS fts_chunks USING fts5(
                heading,
                context,
                content,
                content='',
                contentless_delete=1,
                tokenize = \"trigram remove_diacritics 1 case_sensitive 0\"
            );",
        )?;

        // meta に dim が記録されていれば vec_chunks を復元
        if let Some((_, dim)) = self.read_embedding_meta()? {
            self.ensure_vec_chunks_table(dim)?;
        }

        // legacy DB 互換: chunks.quality_score 列が無ければ ALTER で
        // 追加する (DEFAULT 1.0 で既存行は全件「通過」扱い)。
        self.ensure_quality_score_column()?;

        // legacy DB 互換: chunks.level 列が無ければ ALTER で追加する
        // (NULL のまま — 値は再 index 時に埋まる)。
        self.ensure_chunk_level_column()?;

        // legacy DB 互換: chunks.context_text 列が無ければ ALTER で追加する
        // (feature-46。NULL のまま — 値は PR-2 の context_mode 導入後、再 index で埋まる)。
        self.ensure_context_text_column()?;

        // legacy DB 互換: fts_chunks が旧 2 列 schema なら 3 列へ rebuild migration
        // する (feature-46)。context_text 列の存在を前提に repopulate するため、
        // 必ず `ensure_context_text_column` の後に呼ぶこと。
        self.ensure_fts_context_column()?;

        Ok(())
    }

    /// `chunks.quality_score` 列が存在しなければ追加する (idempotent)。
    /// legacy DB を開いても失敗しないよう init 経路から
    /// 呼ぶ。新規 DB では `CREATE TABLE` 時点で列があるので no-op。
    ///
    /// 2 プロセスが同時に open して race した場合、後着プロセスの ALTER が
    /// `duplicate column name: quality_score` を返すので、このエラーだけは
    /// 吸収して正常復帰する (他の SQLite エラーはそのまま伝播)。
    fn ensure_quality_score_column(&self) -> Result<()> {
        let has_col: bool = self
            .conn
            .prepare("PRAGMA table_info(chunks)")?
            .query_map([], |row| row.get::<_, String>(1))?
            .filter_map(std::result::Result::ok)
            .any(|name| name == "quality_score");
        if !has_col {
            match self.conn.execute_batch(
                "ALTER TABLE chunks ADD COLUMN quality_score REAL NOT NULL DEFAULT 1.0;",
            ) {
                Ok(()) => {}
                // 他プロセスが先に ALTER した場合 (race) はエラーを飲み込んで継続。
                Err(e) if e.to_string().contains("duplicate column") => {}
                Err(e) => return Err(e.into()),
            }
        }
        // 新規 DB でも legacy DB でも、列が確保された後に同じ
        // INDEX (IF NOT EXISTS) を必ず張る。
        //
        // KNN / FTS 経由の search は vec_chunks / fts_chunks 駆動で chunks を
        // JOIN 後に Rust 側で filter するため、このインデックスは検索パス
        // では使われない。`chunk_count_by_quality` (status 表示) および
        // 将来の「低品質チャンクだけ一覧」クエリ用の副次インデックス。
        self.conn.execute_batch(
            "CREATE INDEX IF NOT EXISTS idx_chunks_quality ON chunks(quality_score);",
        )?;
        Ok(())
    }

    /// `chunks.level` 列が存在しなければ追加する (idempotent)。
    /// legacy DB を開いても失敗しないよう init 経路から呼ぶ。
    /// 新規 DB では `CREATE TABLE` 時点で列があるので no-op。
    /// 既存行の `level` は NULL のまま (再 index で埋まる)。
    /// race 条件 (2 プロセス同時 open) の場合は duplicate column エラーを吸収。
    fn ensure_chunk_level_column(&self) -> Result<()> {
        let has_col: bool = self
            .conn
            .prepare("PRAGMA table_info(chunks)")?
            .query_map([], |row| row.get::<_, String>(1))?
            .filter_map(std::result::Result::ok)
            .any(|name| name == "level");
        if !has_col {
            match self
                .conn
                .execute_batch("ALTER TABLE chunks ADD COLUMN level INTEGER;")
            {
                Ok(()) => {}
                // 他プロセスが先に ALTER した場合 (race) はエラーを飲み込んで継続。
                Err(e) if e.to_string().contains("duplicate column") => {}
                Err(e) => return Err(e.into()),
            }
        }
        Ok(())
    }

    /// `chunks.context_text` 列が存在しなければ追加する (idempotent、feature-46)。
    /// legacy DB を開いても失敗しないよう init 経路から呼ぶ。新規 DB では
    /// `CREATE TABLE` 時点で列があるので no-op。既存行は NULL (再 index で埋まる)。
    /// race 条件 (2 プロセス同時 open) は duplicate column エラーを吸収。
    fn ensure_context_text_column(&self) -> Result<()> {
        let has_col: bool = self
            .conn
            .prepare("PRAGMA table_info(chunks)")?
            .query_map([], |row| row.get::<_, String>(1))?
            .filter_map(std::result::Result::ok)
            .any(|name| name == "context_text");
        if !has_col {
            match self
                .conn
                .execute_batch("ALTER TABLE chunks ADD COLUMN context_text TEXT;")
            {
                Ok(()) => {}
                Err(e) if e.to_string().contains("duplicate column") => {}
                Err(e) => return Err(e.into()),
            }
        }
        Ok(())
    }

    /// `fts_chunks` に context 列が無ければ 3 列 schema へ rebuild migration する
    /// (feature-46、init 内 one-time)。status / search / serve は rebuild_index を
    /// 経由しないため、init で全 entry point の schema を保証する。table_info ガードで
    /// 2 回目以降 O(1) no-op。DROP+CREATE+repopulate は BEGIN IMMEDIATE + double-checked
    /// locking で multi-process race を防ぐ (spec §4.4)。**`ensure_context_text_column`
    /// の後に呼ぶこと** (repopulate が chunks.context_text を読むため)。
    ///
    /// `backfill_fts` (rebuild_index 冒頭の欠損 rowid 補充) とは責務が別 (schema 変換 vs
    /// 欠損補充)。本 fn は schema を 2→3 列へ変換するだけ。
    fn ensure_fts_context_column(&self) -> Result<()> {
        // 1) 高速パス: context 列が既にあれば no-op (O(1))
        if self.fts_chunks_has_context_column()? {
            return Ok(());
        }
        // 2) IMMEDIATE tx (RESERVED lock) で書き手を単一化
        let tx = self.begin_immediate_tx()?;
        // 3) double-checked: lock 取得後に再チェック (他プロセスが migration 済みなら no-op)
        if fts_chunks_has_context_column_conn(&tx)? {
            tx.commit()?;
            return Ok(());
        }
        eprintln!("Migrating FTS index to 3-column schema (heading/context/content)...");
        // 4) DROP + CREATE 3 列 + chunks から全 repopulate (原子的: DDL はトランザクショナル)
        tx.execute_batch(
            "DROP TABLE fts_chunks;
             CREATE VIRTUAL TABLE fts_chunks USING fts5(
                heading, context, content,
                content='', contentless_delete=1,
                tokenize = \"trigram remove_diacritics 1 case_sensitive 0\"
             );
             INSERT INTO fts_chunks (rowid, heading, context, content)
                SELECT id, heading, COALESCE(context_text, ''), content FROM chunks;",
        )?;
        tx.commit()?;
        Ok(())
    }

    /// `fts_chunks` に context 列があるか (self.conn 版)。
    fn fts_chunks_has_context_column(&self) -> Result<bool> {
        fts_chunks_has_context_column_conn(&self.conn)
    }

    /// 現存する `vec_chunks` の宣言済み次元を返す。テーブルが無い or
    /// `CREATE` 文から次元を抜き出せない場合は `None`。
    fn current_vec_dim(&self) -> Result<Option<u32>> {
        use rusqlite::OptionalExtension;
        let sql: Option<String> = self
            .conn
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type='table' AND name='vec_chunks'",
                [],
                |row| row.get(0),
            )
            .optional()?;
        Ok(sql.as_deref().and_then(parse_dim_from_create_sql))
    }

    /// 指定 `dim` の `vec_chunks` が存在することを保証する。
    /// 既存テーブルが別次元なら error (再構築は `recreate_vec_chunks` 経由)。
    fn ensure_vec_chunks_table(&self, dim: u32) -> Result<()> {
        if let Some(existing) = self.current_vec_dim()? {
            if existing == dim {
                return Ok(());
            }
            anyhow::bail!(
                "vec_chunks declared float[{existing}] but runtime dim is {dim}. \
                 Run index with --force to rebuild."
            );
        }
        let sql = format!(
            "CREATE VIRTUAL TABLE vec_chunks USING vec0(
                 chunk_id INTEGER PRIMARY KEY,
                 embedding float[{dim}]
             )"
        );
        self.conn.execute_batch(&sql)?;
        Ok(())
    }

    // -- public API ----------------------------------------------------------

    /// Insert or update a document row. On update the old chunks (and their
    /// vec_chunks entries) are deleted so the caller can re-insert fresh ones.
    ///
    /// The UPDATE branch performs four mutating statements (DELETE vec_chunks /
    /// DELETE fts_chunks / DELETE chunks / UPDATE documents) which must be
    /// applied atomically so that a partial failure does not leave dangling
    /// vec / FTS rows. We wrap the body in a tx — but only when the caller is
    /// not already inside one (autocommit-aware), so wrapping callers
    /// (`begin_transaction()` users such as the indexer) can compose without
    /// triggering "cannot start a transaction within a transaction".
    ///
    /// Returns the document `id`.
    #[allow(clippy::too_many_arguments)]
    pub fn upsert_document(
        &self,
        path: &str,
        title: Option<&str>,
        topic: Option<&str>,
        category: Option<&str>,
        depth: Option<&str>,
        tags: &[String],
        date: Option<&str>,
        content_hash: &str,
    ) -> Result<i64> {
        let now = chrono::Utc::now().to_rfc3339();
        let tags_json = serde_json::to_string(tags)?;

        // Open a local tx only if we are in autocommit (no caller-managed tx).
        // Drop guard rolls back automatically; we commit at the end on success.
        let local_tx = if self.conn.is_autocommit() {
            Some(self.conn.unchecked_transaction()?)
        } else {
            None
        };

        // Check if document already exists
        use rusqlite::OptionalExtension;
        let existing_id: Option<i64> = self
            .conn
            .query_row(
                "SELECT id FROM documents WHERE path = ?1",
                params![path],
                |row| row.get(0),
            )
            .optional()?;

        let doc_id = if let Some(doc_id) = existing_id {
            // Delete old vector / FTS entries for chunks that belong to this document
            self.conn.execute(
                "DELETE FROM vec_chunks WHERE chunk_id IN \
                 (SELECT id FROM chunks WHERE document_id = ?1)",
                params![doc_id],
            )?;
            self.conn.execute(
                "DELETE FROM fts_chunks WHERE rowid IN \
                 (SELECT id FROM chunks WHERE document_id = ?1)",
                params![doc_id],
            )?;
            // Cascade will handle chunks when we update the document,
            // but we delete explicitly to be safe before the UPDATE
            self.conn
                .execute("DELETE FROM chunks WHERE document_id = ?1", params![doc_id])?;
            // Update the document row
            self.conn.execute(
                "UPDATE documents SET title = ?1, topic = ?2, category = ?3,
                 depth = ?4, tags = ?5, date = ?6, content_hash = ?7,
                 last_indexed = ?8 WHERE id = ?9",
                params![
                    title,
                    topic,
                    category,
                    depth,
                    tags_json,
                    date,
                    content_hash,
                    now,
                    doc_id
                ],
            )?;
            doc_id
        } else {
            self.conn.execute(
                "INSERT INTO documents (path, title, topic, category, depth, tags, date, content_hash, last_indexed)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                params![path, title, topic, category, depth, tags_json, date, content_hash, now],
            )?;
            self.conn.last_insert_rowid()
        };

        if let Some(tx) = local_tx {
            tx.commit()?;
        }
        Ok(doc_id)
    }

    /// Insert a chunk row **and** its corresponding vec_chunks embedding + FTS row.
    ///
    /// `embedding` の長さは現在の `vec_chunks` の宣言次元 (`ModelChoice` に連動、
    /// BGE-small-en-v1.5 で 384 / BGE-M3 で 1024) と一致する必要がある。
    /// `quality_score` は the quality filterで使われる (0.0-1.0、
    /// `crate::quality::chunk_quality_score` で算出)。
    /// Returns the chunk `id`.
    /// Insert a chunk row plus its `vec_chunks` embedding and `fts_chunks`
    /// row. The three statements must commit together: a partial write would
    /// leave a chunk visible to FTS but invisible to vector search, or vice
    /// versa. The body is wrapped in an autocommit-aware tx — same composition
    /// pattern as [`Self::upsert_document`] so a caller can group multiple
    /// `insert_chunk` calls under one outer tx via `begin_transaction()`.
    #[allow(clippy::too_many_arguments)]
    pub fn insert_chunk(
        &self,
        document_id: i64,
        chunk_index: i32,
        heading: Option<&str>,
        level: Option<u8>,
        content: &str,
        context: Option<&str>,
        embedding: &[f32],
        quality_score: f32,
    ) -> Result<i64> {
        // Rough token estimate: 1 token ~= 4 chars (English average).
        // F-46: saturate at i32::MAX rather than wrap on the rare 8 GiB+
        // content path (chunker is hard-capped well below this in practice;
        // defense-in-depth for diagnosing oversize indexing failures).
        let token_count = i32::try_from(content.len() / 4).unwrap_or(i32::MAX);

        let local_tx = if self.conn.is_autocommit() {
            Some(self.conn.unchecked_transaction()?)
        } else {
            None
        };

        // SQLite has no native u8; widen to i64 for the bind. NULL is stored
        // when `level` is None, matching the column's NULL-able definition
        // and the legacy-row migration path (chunks indexed before
        // feature-28 keep `level = NULL` until re-indexed).
        let level_bind = level.map(|l| l as i64);
        self.conn.execute(
            "INSERT INTO chunks (document_id, chunk_index, heading, level, content, context_text, token_count, quality_score)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![document_id, chunk_index, heading, level_bind, content, context, token_count, quality_score],
        )?;
        let chunk_id = self.conn.last_insert_rowid();

        // sqlite-vec accepts embeddings as a JSON array string
        let embedding_json = serde_json::to_string(embedding)?;
        self.conn.execute(
            "INSERT INTO vec_chunks (chunk_id, embedding) VALUES (?1, ?2)",
            params![chunk_id, embedding_json],
        )?;

        self.conn.execute(
            "INSERT INTO fts_chunks (rowid, heading, context, content) VALUES (?1, ?2, ?3, ?4)",
            params![chunk_id, heading, context, content],
        )?;

        if let Some(tx) = local_tx {
            tx.commit()?;
        }
        Ok(chunk_id)
    }

    /// Return the stored `content_hash` for a document path, or `None` if the
    /// path is not indexed yet.
    pub fn get_document_hash(&self, path: &str) -> Result<Option<String>> {
        use rusqlite::OptionalExtension;
        let result = self
            .conn
            .query_row(
                "SELECT content_hash FROM documents WHERE path = ?1",
                params![path],
                |row| row.get(0),
            )
            .optional()?;
        Ok(result)
    }

    /// 指定 path の chunk 本文 (heading, content) を
    /// chunk_index 順に返す。frontmatter のみ変更かどうかを判定するために
    /// 既存 chunks のテキストだけを読む。embedding は取得しない (軽量)。
    pub fn chunk_texts_for_path(&self, path: &str) -> Result<Vec<(Option<String>, String)>> {
        let sql = "
            SELECT c.heading, c.content
            FROM chunks c
            JOIN documents d ON d.id = c.document_id
            WHERE d.path = ?1
            ORDER BY c.chunk_index
        ";
        let mut stmt = self.conn.prepare(sql)?;
        let rows = stmt.query_map(params![path], |row| {
            Ok((row.get::<_, Option<String>>(0)?, row.get::<_, String>(1)?))
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// [`Self::chunk_texts_for_path`] の `context_text` 込み版
    /// (heading, content, context_text)。
    ///
    /// codex P2 round 2 (finding B): Static context mode の frontmatter-only
    /// skip 判定は `context_text` (= ancestry breadcrumb) の変化も検知する
    /// 必要があるため、既存 `chunk_texts_for_path` (Off モード用、2-tuple) とは
    /// 別の専用メソッドとして追加した。既存の呼び出し元・テストに影響しない
    /// ようシグネチャを分けている。
    pub fn chunk_texts_with_context_for_path(
        &self,
        path: &str,
    ) -> Result<Vec<ChunkTextWithContext>> {
        let sql = "
            SELECT c.heading, c.content, c.context_text
            FROM chunks c
            JOIN documents d ON d.id = c.document_id
            WHERE d.path = ?1
            ORDER BY c.chunk_index
        ";
        let mut stmt = self.conn.prepare(sql)?;
        let rows = stmt.query_map(params![path], |row| {
            Ok((
                row.get::<_, Option<String>>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
            ))
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// frontmatter-only change 用の document meta 更新。
    /// chunks は触らず、documents 行の title / date / topic / category /
    /// depth / tags / content_hash のみ UPDATE する。存在しなければ no-op で
    /// `Ok(false)`。
    #[allow(clippy::too_many_arguments)]
    pub fn update_document_meta(
        &self,
        path: &str,
        title: Option<&str>,
        topic: Option<&str>,
        category: Option<&str>,
        depth: Option<&str>,
        tags: &[String],
        date: Option<&str>,
        content_hash: &str,
    ) -> Result<bool> {
        let tags_json = serde_json::to_string(tags)?;
        let updated_at = chrono::Utc::now().to_rfc3339();
        let n = self.conn.execute(
            "UPDATE documents
                SET title = ?1,
                    topic = ?2,
                    category = ?3,
                    depth = ?4,
                    tags = ?5,
                    date = ?6,
                    content_hash = ?7,
                    last_indexed = ?8
              WHERE path = ?9",
            params![
                title,
                topic,
                category,
                depth,
                tags_json,
                date,
                content_hash,
                updated_at,
                path
            ],
        )?;
        Ok(n > 0)
    }

    /// 指定 `path` に属するチャンクを (chunk_id, embedding, SearchResult) で返す。
    /// Connection Graph の起点シード取得用。存在しなければ empty Vec。
    ///
    /// `embedding` は `vec_to_json` で JSON 文字列として取り出し、serde_json で
    /// `Vec<f32>` に復元する。`SearchResult.score` はシード node 用に 1.0 を入れる
    /// (BFS 結果のスコアと同じ意味 = cos sim 換算値の上限)。
    pub fn chunks_for_path(&self, path: &str) -> Result<Vec<(i64, Vec<f32>, SearchResult)>> {
        let sql = "
            SELECT c.id, vec_to_json(v.embedding),
                   c.content, c.heading, c.document_id,
                   d.path, d.title, d.topic, d.date, d.tags
            FROM chunks c
            JOIN documents d ON d.id = c.document_id
            JOIN vec_chunks v ON v.chunk_id = c.id
            WHERE d.path = ?1
            ORDER BY c.chunk_index
        ";
        let mut stmt = self.conn.prepare(sql)?;
        let rows = stmt.query_map(params![path], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, i64>(4)?, // document_id
                row.get::<_, String>(5)?,
                row.get::<_, Option<String>>(6)?,
                row.get::<_, Option<String>>(7)?,
                row.get::<_, Option<String>>(8)?,
                row.get::<_, Option<String>>(9)?,
            ))
        })?;
        let mut out = Vec::new();
        for r in rows {
            let (id, embedding_json, content, heading, doc_id, path, title, topic, date, tags_json) =
                r?;
            let embedding: Vec<f32> = serde_json::from_str(&embedding_json)
                .with_context(|| format!("failed to parse embedding json for chunk {id}"))?;
            out.push((
                id,
                embedding,
                SearchResult {
                    score: 1.0,
                    content,
                    heading,
                    document_id: doc_id,
                    path,
                    title,
                    topic,
                    date,
                    tags: self.parse_tags_json_recording(tags_json),
                    // graph seed は rerank しないので context 合成は不要。
                    context_text: None,
                },
            ));
        }
        Ok(out)
    }

    /// 指定 `chunk_id` の embedding を取り出す。存在しなければ `None`。
    /// BFS の 2-hop 目以降で「親チャンクの embedding を起点に KNN を実行」する
    /// ために使う。
    pub fn get_chunk_embedding(&self, chunk_id: i64) -> Result<Option<Vec<f32>>> {
        use rusqlite::OptionalExtension;
        let sql = "SELECT vec_to_json(embedding) FROM vec_chunks WHERE chunk_id = ?1";
        let row: Option<String> = self
            .conn
            .query_row(sql, params![chunk_id], |row| row.get(0))
            .optional()?;
        match row {
            Some(json) => {
                let v: Vec<f32> = serde_json::from_str(&json).with_context(|| {
                    format!("failed to parse embedding json for chunk {chunk_id}")
                })?;
                Ok(Some(v))
            }
            None => Ok(None),
        }
    }

    /// 指定 `chunk_ids` 群の embedding を一括取得する。`get_chunk_embedding` の
    /// IN 句版で、MMR の候補プール (RRF で得た FTS 単独 hit + vec 単独 hit の
    /// merge 結果) に対して pairwise 類似度を計算するときに使う。
    ///
    /// SQL の IN 句は順序非保証なので、戻り値は `HashMap<i64, Vec<f32>>` で
    /// 返し、呼び出し側で reorder すること。
    ///
    /// 存在しない `chunk_id` は単に結果から除外される (エラーにしない)。index
    /// 中の race / 削除済 chunk_id を query に含む可能性があるので silently
    /// skip が望ましい。
    ///
    /// **SQLite host parameter limit**: `SQLITE_MAX_VARIABLE_NUMBER` は modern
    /// SQLite (3.32+) で 32766。bundled SQLite 3.47+ でもこの値が default。
    /// 高 limit MMR (例: `--limit 10000` で pool = 50000 chunk_ids) で IN 句が
    /// この上限を超えるため、内部で [`EMBEDDING_FETCH_BATCH`] (= 500) ごとに
    /// 分割して複数 query を発行する。500 は SQLite の上限に十分余裕を持た
    /// せつつ、典型的な MMR pool (≤ 500) では 1 round-trip で済む値。
    pub fn fetch_embeddings_by_chunk_ids(
        &self,
        chunk_ids: &[i64],
    ) -> Result<HashMap<i64, Vec<f32>>> {
        if chunk_ids.is_empty() {
            return Ok(HashMap::new());
        }
        let mut out = HashMap::with_capacity(chunk_ids.len());
        for batch in chunk_ids.chunks(EMBEDDING_FETCH_BATCH) {
            // IN 句のプレースホルダを動的生成 (?1, ?2, ...)
            let placeholders: String = (1..=batch.len())
                .map(|i| format!("?{i}"))
                .collect::<Vec<_>>()
                .join(",");
            let sql = format!(
                "SELECT chunk_id, vec_to_json(embedding) \
                 FROM vec_chunks WHERE chunk_id IN ({placeholders})"
            );
            let mut stmt = self.conn.prepare(&sql)?;
            let params_iter: Vec<&dyn rusqlite::ToSql> =
                batch.iter().map(|id| id as &dyn rusqlite::ToSql).collect();
            let rows = stmt.query_map(rusqlite::params_from_iter(params_iter), |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
            })?;
            for r in rows {
                let (id, json) = r?;
                let emb: Vec<f32> = serde_json::from_str(&json)
                    .with_context(|| format!("failed to parse embedding json for chunk {id}"))?;
                out.insert(id, emb);
            }
        }
        Ok(out)
    }

    // F-41 PR-2: lookup_document_id_by_path was the per-candidate N+1 lookup
    // used by the MMR pool builder. SearchResult.document_id is now carried by
    // the candidate SQLs (search_vec_candidates / search_fts_candidates /
    // chunks_for_path), so the helper is removed entirely. Side effect: the
    // `unwrap_or(0)` rename-race collision flagged as F-44 also disappears
    // (no fallback path = no collision).

    /// Parent retriever 用: `chunk_id` から `(document_id, chunk_index, token_count)`
    /// を引く軽量 lookup。`token_count` は legacy 行で NULL になり得るので
    /// `Option<i64>` として返す。
    pub fn get_chunk_meta(&self, chunk_id: i64) -> Result<(i64, i64, Option<i64>)> {
        Ok(self.conn.query_row(
            "SELECT document_id, chunk_index, token_count FROM chunks WHERE id = ?1",
            rusqlite::params![chunk_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?)
    }

    /// Parent retriever 用: 同一 doc 内の `chunk_index` 範囲 `[from, to]` (inclusive)
    /// に該当する chunk を `chunk_index` ASC で返す。
    ///
    /// `from` が負だったり `to` が doc 末尾を超える場合は単に該当行が無い扱いに
    /// なる (SQLite range filter で自然にトリム)。adjacent merge では
    /// `[hit_idx - 1, hit_idx + 1]` のような呼び出しを想定しており、左右の
    /// 端で自動的にバウンドされる前提。
    pub fn fetch_chunks_by_index_range(
        &self,
        doc_id: i64,
        from: i64,
        to: i64,
        max_rows: u32,
    ) -> Result<Vec<ChunkRow>> {
        // `max_rows` cap: defense-in-depth so a pathological document
        // (e.g. tens of thousands of chunks) cannot force whole-doc
        // expansion to materialize an unbounded `Vec<ChunkRow>` before
        // the caller's per-chunk token cap can kick in. Caller is
        // responsible for picking a reasonable bound (adjacent merge
        // can pass a small constant, whole-doc passes a heuristic
        // derived from `max_expanded_tokens`). `max_rows = 0` is
        // treated as "no rows", matching SQLite LIMIT 0 semantics.
        let mut stmt = self.conn.prepare(
            "SELECT chunk_index, content, token_count, level FROM chunks
             WHERE document_id = ?1 AND chunk_index >= ?2 AND chunk_index <= ?3
             ORDER BY chunk_index ASC
             LIMIT ?4",
        )?;
        let rows = stmt.query_map(rusqlite::params![doc_id, from, to, max_rows], |row| {
            Ok(ChunkRow {
                chunk_index: row.get(0)?,
                content: row.get(1)?,
                token_count: row.get(2)?,
                level: row.get::<_, Option<i64>>(3)?.map(|v| v as u8),
            })
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// Delete a document and all associated chunks / vectors / FTS rows.
    pub fn delete_document(&self, path: &str) -> Result<()> {
        // Delete vector entries first (no FK from virtual table)
        self.conn.execute(
            "DELETE FROM vec_chunks WHERE chunk_id IN \
             (SELECT c.id FROM chunks c JOIN documents d ON c.document_id = d.id WHERE d.path = ?1)",
            params![path],
        )?;
        // FTS5 contentless: rowid ベースで削除
        self.conn.execute(
            "DELETE FROM fts_chunks WHERE rowid IN \
             (SELECT c.id FROM chunks c JOIN documents d ON c.document_id = d.id WHERE d.path = ?1)",
            params![path],
        )?;
        // Delete chunks (cascade would handle this, but be explicit)
        self.conn.execute(
            "DELETE FROM chunks WHERE document_id IN \
             (SELECT id FROM documents WHERE path = ?1)",
            params![path],
        )?;
        // Delete the document row
        self.conn
            .execute("DELETE FROM documents WHERE path = ?1", params![path])?;
        Ok(())
    }

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
    pub(crate) fn search_fts_candidates(
        &self,
        query_text: &str,
        limit: u32,
        filters: &SearchFilters<'_>,
        fusion: FusionParams,
    ) -> Result<Vec<(i64, SearchResult)>> {
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
        };
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

        let mut out = Vec::with_capacity(limit as usize);
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

    /// List all indexed topics grouped by (category, topic).
    pub fn list_topics(&self) -> Result<Vec<TopicInfo>> {
        // タイトルは json_group_array で集めて JSON 配列として受ける。
        // 旧実装は GROUP_CONCAT(title, '||') + split を使っていたが、
        // タイトル中に "||" を含む doc が紛れると誤分割していた。
        let sql = "
            SELECT category, topic,
                   COUNT(*) AS file_count,
                   MAX(last_indexed) AS last_updated,
                   json_group_array(title) AS titles_json
            FROM documents
            GROUP BY category, topic
            ORDER BY category, topic
        ";
        let mut stmt = self.conn.prepare(sql)?;
        let rows = stmt.query_map([], |row| {
            let titles_json: Option<String> = row.get(4)?;
            let titles: Vec<String> = titles_json
                .as_deref()
                .and_then(|s| serde_json::from_str::<Vec<Option<String>>>(s).ok())
                .map(|v| v.into_iter().flatten().collect())
                .unwrap_or_default();
            Ok(TopicInfo {
                category: row.get(0)?,
                topic: row.get(1)?,
                file_count: row.get(2)?,
                last_updated: row.get(3)?,
                titles,
            })
        })?;
        rows.into_iter()
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    /// Total number of indexed documents.
    pub fn document_count(&self) -> Result<u32> {
        let count: u32 = self
            .conn
            .query_row("SELECT COUNT(*) FROM documents", [], |row| row.get(0))?;
        Ok(count)
    }

    /// Total number of chunks across all documents.
    pub fn chunk_count(&self) -> Result<u32> {
        let count: u32 = self
            .conn
            .query_row("SELECT COUNT(*) FROM chunks", [], |row| row.get(0))?;
        Ok(count)
    }

    /// Read `(model, dim)` from `index_meta`. Returns `None` if either key is
    /// missing or malformed (treated as "no meta recorded yet").
    pub fn read_embedding_meta(&self) -> Result<Option<(String, u32)>> {
        use rusqlite::OptionalExtension;
        let model: Option<String> = self
            .conn
            .query_row(
                "SELECT value FROM index_meta WHERE key = 'embedding_model'",
                [],
                |row| row.get(0),
            )
            .optional()?;
        let dim_raw: Option<String> = self
            .conn
            .query_row(
                "SELECT value FROM index_meta WHERE key = 'embedding_dim'",
                [],
                |row| row.get(0),
            )
            .optional()?;
        match (model, dim_raw) {
            (Some(m), Some(d)) => match d.parse::<u32>() {
                Ok(dim) => Ok(Some((m, dim))),
                Err(_) => Ok(None),
            },
            _ => Ok(None),
        }
    }

    /// Insert or replace the `(embedding_model, embedding_dim)` entries in
    /// `index_meta`.
    pub fn write_embedding_meta(&self, model: &str, dim: u32) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO index_meta (key, value) VALUES ('embedding_model', ?1)",
            params![model],
        )?;
        self.conn.execute(
            "INSERT OR REPLACE INTO index_meta (key, value) VALUES ('embedding_dim', ?1)",
            params![dim.to_string()],
        )?;
        Ok(())
    }

    /// `index_meta.context_mode` を読む。key 不在 / 未知値は `None` (= grandfather 判定へ)。
    pub fn read_context_mode(&self) -> Result<Option<ContextMode>> {
        use rusqlite::OptionalExtension;
        let raw: Option<String> = self
            .conn
            .query_row(
                "SELECT value FROM index_meta WHERE key = 'context_mode'",
                [],
                |row| row.get(0),
            )
            .optional()?;
        Ok(raw.as_deref().and_then(ContextMode::from_str_opt))
    }

    /// `index_meta.context_mode` を記録する (INSERT OR REPLACE)。
    pub fn write_context_mode(&self, mode: ContextMode) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO index_meta (key, value) VALUES ('context_mode', ?1)",
            params![mode.as_str()],
        )?;
        Ok(())
    }

    /// 指定 path の documents.title を読む (E-8 の title 変更検知用)。
    /// 未 index / title NULL は `None`。Task 2.7 の frontmatter-only skip title gate で消費される。
    pub fn get_document_title(&self, path: &str) -> Result<Option<String>> {
        use rusqlite::OptionalExtension;
        let title: Option<Option<String>> = self
            .conn
            .query_row(
                "SELECT title FROM documents WHERE path = ?1",
                params![path],
                |row| row.get(0),
            )
            .optional()?;
        Ok(title.flatten())
    }

    /// `index_meta` から `tags_parse_failures` key を read する (F-63)。
    /// 値が無い / `u64::from_str` に失敗する malformed 値は `None` 扱い
    /// (= 起動時 restore で 0 にフォールバック)。
    fn read_tags_parse_failure_count(&self) -> Result<Option<u64>> {
        use rusqlite::OptionalExtension;
        let raw: Option<String> = self
            .conn
            .query_row(
                "SELECT value FROM index_meta WHERE key = 'tags_parse_failures'",
                [],
                |row| row.get(0),
            )
            .optional()?;
        Ok(raw.and_then(|s| s.parse::<u64>().ok()))
    }

    /// `documents.tags` 列 (JSON 文字列) を `Vec<String>` に展開する。
    /// NULL / 空文字 / 不正 JSON は空 Vec として扱う (検索フィルタでヒット 0 件に
    /// なるだけで、エラーで検索を中断させない)。
    /// 不正 JSON 時は `tags_parse_failures` カウンタを atomic increment し、
    /// `tracing::warn!` も併発する (F-63: silent fail-open 可視化)。
    pub(crate) fn parse_tags_json_recording(&self, json: Option<String>) -> Vec<String> {
        match json {
            Some(s) if !s.is_empty() => match serde_json::from_str(&s) {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!(error = %e, "malformed documents.tags JSON, treating as empty");
                    self.tags_parse_failures.fetch_add(1, Ordering::Relaxed);
                    Vec::new()
                }
            },
            _ => Vec::new(),
        }
    }

    /// 現在の `tags_parse_failures` cumulative 値を返す (F-63、`kb-mcp status` 表示用)。
    ///
    /// `index_meta` の永続値 (= 過去 session までの累計) と本 session の AtomicU64
    /// delta (= 本 session 中に増えた失敗数) を合算する。codex P2 fix:
    /// **AtomicU64 は session-local delta** として持つ設計で、multi-instance で
    /// 同 SQLite file を開いた場合の last-writer-wins を回避する。
    ///
    /// DB read が失敗した場合 (= I/O エラー / schema 不整合等) は session delta だけを
    /// 返す best-effort 表示。`kb-mcp status` は人間向け診断なので panic より degrade。
    pub fn tags_parse_failure_count(&self) -> u64 {
        let persisted = self
            .read_tags_parse_failure_count()
            .ok()
            .flatten()
            .unwrap_or(0);
        let delta = self.tags_parse_failures.load(Ordering::Relaxed);
        persisted.saturating_add(delta)
    }

    /// Verify the runtime `(model, dim)` matches the values recorded in
    /// `index_meta`.
    ///
    /// * Empty meta + empty DB → record current values (fresh DB).
    /// * Empty meta + non-empty DB → migrate a legacy DB by recording
    ///   the current values, with a one-time log message.
    /// * Matching meta → no-op.
    /// * Mismatching meta → return an actionable error.
    pub fn verify_embedding_meta(&self, model: &str, dim: u32) -> Result<()> {
        match self.read_embedding_meta()? {
            None => {
                if self.chunk_count()? > 0 {
                    eprintln!(
                        "Migrating pre-meta index: recording ({model}, {dim}) into index_meta"
                    );
                }
                self.write_embedding_meta(model, dim)?;
                self.ensure_vec_chunks_table(dim)
            }
            Some((db_model, db_dim)) if db_model == model && db_dim == dim => {
                // init 時に meta が無くて vec_chunks を作れなかったケースをここで補う。
                self.ensure_vec_chunks_table(dim)
            }
            Some((db_model, db_dim)) => anyhow::bail!(
                "embedding model mismatch.\n  \
                 DB was indexed with: {db_model} ({db_dim} dim)\n  \
                 Current runtime:     {model} ({dim} dim)\n\n\
                 Run `kb-mcp index --kb-path <path> --force --model {model}` to rebuild the index, \
                 or switch back to the previous model."
            ),
        }
    }

    /// FTS に未登録の `chunks` を拾って `fts_chunks` に埋め直す。
    /// 主に legacy DB のマイグレーション経路で呼ばれる。
    /// 埋め込み再計算は行わないので高速 (既存 content を INSERT するだけ)。
    pub fn backfill_fts(&self) -> Result<u32> {
        let sql = "
            SELECT id, heading, context_text, content
            FROM chunks
            WHERE id NOT IN (SELECT rowid FROM fts_chunks)
        ";
        let mut stmt = self.conn.prepare(sql)?;
        let rows: Vec<(i64, Option<String>, Option<String>, String)> = stmt
            .query_map([], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        let mut count = 0u32;
        for (id, heading, context, content) in rows {
            self.conn.execute(
                "INSERT INTO fts_chunks (rowid, heading, context, content) VALUES (?1, ?2, ?3, ?4)",
                params![id, heading, context, content],
            )?;
            count += 1;
        }
        Ok(count)
    }

    /// legacy / 前回 index 済み DB で `quality_score` が DEFAULT 1.0 のままの
    /// チャンクを検出し、[`quality::chunk_quality_score`] で再計算して UPDATE する (冪等)。
    ///
    /// `binary_exts` = is_binary な parser の拡張子集合。document の path 拡張子が
    /// これに含まれれば `is_binary=true` で再計算し、length/structure penalty を免除する。
    /// これを怠ると初回 index で免除された binary chunk が 2 回目 backfill で penalty
    /// 転落する (§4.8 P0)。
    pub fn backfill_quality(&self, binary_exts: &[&str]) -> Result<u32> {
        // 旧 DB (= default 1.0 のまま) のみを対象にする: score != 1.0 の行は
        // 既に計算済みとみなしてスキップ。初期値 1.0 で再計算結果も 1.0 の
        // 正当な行は再 UPDATE されないが、冪等性のためには十分 (挙動上同じ)。
        let sql = "SELECT c.id, c.heading, c.content, d.path
                   FROM chunks c JOIN documents d ON d.id = c.document_id
                   WHERE c.quality_score = 1.0";
        let mut stmt = self.conn.prepare(sql)?;
        let rows: Vec<(i64, Option<String>, String, String)> = stmt
            .query_map([], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        let mut updated = 0u32;
        for (id, heading, content, path) in rows {
            let ext = std::path::Path::new(&path)
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("");
            let is_binary = binary_exts.iter().any(|e| e.eq_ignore_ascii_case(ext));
            let score =
                crate::quality::chunk_quality_score(heading.as_deref(), &content, is_binary);
            if (score - 1.0).abs() < f32::EPSILON {
                // 再計算でも 1.0 (高品質) → UPDATE 不要
                continue;
            }
            self.conn.execute(
                "UPDATE chunks SET quality_score = ?1 WHERE id = ?2",
                params![score, id],
            )?;
            updated += 1;
        }
        Ok(updated)
    }

    /// `threshold` 以上 / 未満のチャンク数を `(above, below)` で返す。
    /// `status` コマンドで「フィルタで N 件除外されている」を表示する用途。
    pub fn chunk_count_by_quality(&self, threshold: f32) -> Result<(u32, u32)> {
        let above: u32 = self.conn.query_row(
            "SELECT COUNT(*) FROM chunks WHERE quality_score >= ?1",
            params![threshold],
            |row| row.get(0),
        )?;
        let below: u32 = self.conn.query_row(
            "SELECT COUNT(*) FROM chunks WHERE quality_score < ?1",
            params![threshold],
            |row| row.get(0),
        )?;
        Ok((above, below))
    }

    /// `vec_chunks` を DROP して指定 `dim` で再生成する。
    /// 呼び出し側で `chunks` / `documents` の整合を別途管理すること
    /// (通常は [`Database::reset_for_model`] 経由で呼ぶ)。
    fn recreate_vec_chunks(&self, dim: u32) -> Result<()> {
        self.conn
            .execute_batch("DROP TABLE IF EXISTS vec_chunks;")?;
        let sql = format!(
            "CREATE VIRTUAL TABLE vec_chunks USING vec0(
                 chunk_id INTEGER PRIMARY KEY,
                 embedding float[{dim}]
             )"
        );
        self.conn.execute_batch(&sql)?;
        Ok(())
    }

    /// `--force` 時の破壊的再初期化: `documents` / `chunks` / `vec_chunks`
    /// を全消ししてから新しい `(model, dim)` を記録する。`indexer::rebuild_index`
    /// が直後にすべての文書を再インデックスすることを前提とする。
    pub fn reset_for_model(&self, model: &str, dim: u32) -> Result<()> {
        self.conn.execute_batch(
            "DELETE FROM fts_chunks; \
             DELETE FROM chunks; \
             DELETE FROM documents;",
        )?;
        self.recreate_vec_chunks(dim)?;
        self.write_embedding_meta(model, dim)?;
        Ok(())
    }

    /// Return every indexed document path.
    pub fn all_document_paths(&self) -> Result<Vec<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT path FROM documents ORDER BY path")?;
        let rows = stmt.query_map([], |row| row.get(0))?;
        rows.into_iter()
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    /// `documents.path` と `content_hash` の全対応を取得する。
    /// File rename detection で、disk 側 hash と突き合わせて
    /// 「embedding 再利用 + path だけ UPDATE」判定に使う。
    pub fn all_path_hashes(&self) -> Result<HashMap<String, String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT path, content_hash FROM documents")?;
        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        let mut out = HashMap::new();
        for row in rows {
            let (p, h) = row?;
            out.insert(p, h);
        }
        Ok(out)
    }

    /// 既存ドキュメントのパスを書き換える。
    /// `chunks` / `vec_chunks` / `fts_chunks` は `document_id` 経由で紐付いて
    /// いるため、`documents.path` のみを UPDATE すれば embedding の再計算は
    /// 不要。移動先 path が既に使われている場合は UNIQUE 制約違反でエラー。
    pub fn rename_document(&self, old_path: &str, new_path: &str) -> Result<()> {
        let updated = self
            .conn
            .execute(
                "UPDATE documents SET path = ?1 WHERE path = ?2",
                params![new_path, old_path],
            )
            .with_context(|| {
                format!(
                    "rename_document: UPDATE documents SET path='{new_path}' WHERE path='{old_path}' (maybe new path already exists in documents)"
                )
            })?;
        if updated == 0 {
            anyhow::bail!("rename_document: no document with path '{old_path}' (rows updated: 0)");
        }
        Ok(())
    }

    /// 複数の rename を **単一 transaction** で適用する (evaluator
    /// 指摘 High #2)。途中失敗したらすべて rollback されるので「部分 rename
    /// 残留」が発生しない。`pairs` が空なら no-op。
    ///
    /// 内部実装は手動 `BEGIN/COMMIT/ROLLBACK` ではなく
    /// `Connection::unchecked_transaction()` を使用 (F-32)。Drop guard で
    /// rollback が担保されるので、`?` early-return パスでも DB が中途半端な
    /// state に置かれない。
    pub fn rename_documents_atomic(&self, pairs: &[(String, String)]) -> Result<()> {
        if pairs.is_empty() {
            return Ok(());
        }
        let tx = self.conn.unchecked_transaction()?;
        for (old, new) in pairs {
            self.rename_document(old, new)?; // Drop on tx rolls back on error
        }
        tx.commit()
            .context("rename_documents_atomic: COMMIT failed")?;
        Ok(())
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
            let pid = std::process::id();
            let nonce = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            let p = std::env::temp_dir().join(format!("kb-mcp-dbtest-{prefix}-{pid}-{nonce}"));
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
            .upsert_document("q.md", Some("Q"), None, None, None, &[], None, "h")
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
            .upsert_document("b.md", None, None, None, None, &[], None, "h")
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
        db.upsert_document("a.md", None, None, None, None, &[], None, "h_a")
            .unwrap();
        db.upsert_document("b.md", None, None, None, None, &[], None, "h_b")
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
        db.upsert_document("a.md", None, None, None, None, &[], None, "h_a")
            .unwrap();
        db.upsert_document("b.md", None, None, None, None, &[], None, "h_b")
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
            .upsert_document("a.md", Some("a"), None, None, None, &[], None, "h_initial")
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
            db.upsert_document("a.md", Some("a"), None, None, None, &[], None, "h_NEW")
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
            .upsert_document("a.md", Some("a"), None, None, None, &[], None, "h_initial")
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
            db.upsert_document("a.md", Some("a"), None, None, None, &[], None, "h_NEW")
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

        let pid = std::process::id();
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let tmp_dir =
            std::env::temp_dir().join(format!("kb-mcp-test-immediate-lock-{pid}-{nonce}"));
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
        db.upsert_document("a.md", None, None, None, None, &[], None, "h_a")
            .unwrap();
        db.upsert_document("b.md", None, None, None, None, &[], None, "h_b")
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
            .upsert_document("c.md", None, None, None, None, &[], None, "h")
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

    #[test]
    fn test_get_chunk_embedding_roundtrip() {
        let db = db_with_384();
        let doc_id = db
            .upsert_document("a.md", None, None, None, None, &[], None, "h1")
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
            .upsert_document("a.md", None, None, None, None, &[], None, "h_a")
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
            .upsert_document("b.md", None, None, None, None, &[], None, "h_b")
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
            .upsert_document("a.md", None, None, None, None, &[], None, "h_a")
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
                .upsert_document("/big.md", Some("big"), Some("topic"), None, None, &[], None, "h")
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

    #[test]
    fn test_sanitize_fts_query() {
        assert_eq!(sanitize_fts_query("E0382"), Some("\"E0382\"".to_string()));
        assert_eq!(
            sanitize_fts_query("foo \"bar\" AND"),
            Some("\"foo \"\"bar\"\" AND\"".to_string())
        );
        assert_eq!(sanitize_fts_query(""), None);
        assert_eq!(sanitize_fts_query("   "), None);
        assert_eq!(sanitize_fts_query("ab"), None, "trigram 3 文字未満は None");
        assert_eq!(
            sanitize_fts_query("エラー"),
            Some("\"エラー\"".to_string()),
            "日本語 3 文字は通る"
        );
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
            .upsert_document("x.md", Some("x"), None, None, None, &[], None, "h")
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
            .upsert_document("a.md", Some("a"), None, None, None, &[], None, "h")
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
            .upsert_document("a.md", Some("a"), None, None, None, &[], None, "h")
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
            .upsert_document("a.md", Some("a"), None, None, None, &[], None, "h1")
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
        db.upsert_document("a.md", Some("a"), None, None, None, &[], None, "h2")
            .unwrap();
        assert_eq!(fts_count(&db), 0);
    }

    #[test]
    fn test_search_hybrid_fts_exact_match_ranks_higher() {
        let db = db_with_384();
        let doc_id = db
            .upsert_document("doc.md", Some("doc"), None, None, None, &[], None, "h")
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
            .upsert_document("a.md", Some("a"), None, None, None, &[], None, "h")
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
            .upsert_document("a.md", Some("a"), None, None, None, &[], None, "h")
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
            .upsert_document("a.md", Some("a"), None, None, None, &[], None, "h")
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
                .upsert_document(&path, Some("x"), None, Some(cat), None, &[], None, "h")
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
            .upsert_document("ja.md", Some("ja"), None, None, None, &[], None, "h")
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

    #[test]
    fn test_backfill_fts_hydrates_preexisting_db() {
        let db = db_with_384();
        let doc_id = db
            .upsert_document("a.md", Some("a"), None, None, None, &[], None, "h")
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
            .upsert_document("n/a.md", Some("T"), None, None, None, &[], None, "h")
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
            .upsert_document("n/b.md", Some("T"), None, None, None, &[], None, "h")
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
            .upsert_document("a.md", Some("a"), None, None, None, &[], None, "h")
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
            .upsert_document("b.md", Some("b"), None, None, None, &[], None, "h")
            .unwrap();
        let emb: Vec<f32> = vec![0.2; 1024];
        db.insert_chunk(doc_id2, 0, None, None, "hi2", None, &emb, 1.0)
            .unwrap();
        assert_eq!(db.chunk_count().unwrap(), 1);
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
        db.upsert_document("n/a.md", Some("My Title"), None, None, None, &[], None, "h")
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
                .upsert_document(p, Some("t"), None, None, None, &[], None, &format!("h{i}"))
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
                .upsert_document(p, Some("t"), None, None, None, &[], None, &format!("h{i}"))
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
                .upsert_document(p, Some("t"), None, None, None, &[], *d, &format!("h{i}"))
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
                .upsert_document(p, Some("t"), None, None, None, &[], None, &format!("h{i}"))
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
        let pid = std::process::id();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let p = std::env::temp_dir().join(format!("kb-mcp-test-{pid}-{nanos}"));
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
            .upsert_document("n/b.md", Some("T"), None, None, None, &[], None, "h")
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
            let pid = std::process::id();
            let nonce = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            let path = std::env::temp_dir().join(format!("{prefix}-{pid}-{nonce}"));
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
            .upsert_document("a.md", Some("A"), None, None, None, &[], None, "ha")
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
            .upsert_document("b.md", Some("B"), None, None, None, &[], None, "hb")
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
}
