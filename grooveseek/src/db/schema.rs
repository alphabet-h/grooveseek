//! Schema creation and forward migrations for [`Database`].
//!
//! Everything here runs from `Database::init`, which every constructor calls,
//! so opening a database is what upgrades it. `groove eval` and `search` are
//! therefore **not read-only**: pointing an older schema at a newer binary
//! migrates it in place, with no way back (see
//! `.dev/knowledge/dogfood-eval-live-db-pitfalls.md`).
//!
//! Each `ensure_*` is idempotent and additive — an `ALTER TABLE ... ADD COLUMN`
//! guarded by a column-existence check — because a user's index is not
//! disposable and the alternative to migrating it is asking them to re-embed a
//! whole knowledge base.
//!
//! Split out of `db.rs` in AU-25: this is the one group whose methods are
//! called almost entirely by each other, which made it the cleanest seam to cut
//! first. The methods are unchanged; only their visibility widened, from
//! private to `pub(super)`, for the six that `db.rs` or its tests already
//! called.

use super::Database;
use anyhow::Result;
use rusqlite::Connection;

/// `CREATE VIRTUAL TABLE ... USING vec0(... embedding float[384] ...)` 形式の
/// SQL から次元数を抽出する。失敗時は `None`。
pub(super) fn parse_dim_from_create_sql(sql: &str) -> Option<u32> {
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
impl Database {
    pub(super) fn init(&self) -> Result<()> {
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
                last_indexed TEXT NOT NULL,
                -- Bytes on disk when the row was written. NULL means it was
                -- never recorded, which is the state of every row in a
                -- database written before feature-51 until the next index
                -- run backfills it.
                size_bytes   INTEGER
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

        // (BU-33) 起点ドキュメントのチャンクを chunk_index 順に読む索引。
        // 既存 DB でも open のたびに張られる (IF NOT EXISTS)。
        self.ensure_chunk_order_index()?;

        // legacy DB 互換: chunks.level 列が無ければ ALTER で追加する
        // (NULL のまま — 値は再 index 時に埋まる)。
        self.ensure_chunk_level_column()?;

        // legacy DB 互換: chunks.context_text 列が無ければ ALTER で追加する
        // (feature-46。NULL のまま — 値は PR-2 の context_mode 導入後、再 index で埋まる)。
        self.ensure_context_text_column()?;

        // legacy DB 互換: documents.size_bytes 列が無ければ ALTER で追加する
        // (feature-51。NULL のまま — 値は次の index 実行時に backfill される)。
        self.ensure_document_size_column()?;

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

    /// `chunks(document_id, chunk_index)` の複合インデックスを張る (idempotent、
    /// BU-33)。
    ///
    /// これが無いと `chunks_for_path_capped` の `LIMIT` が**データベースの仕事を
    /// 縛れない**。`EXPLAIN QUERY PLAN` が `SCAN c` + `USE TEMP B-TREE FOR
    /// ORDER BY` を返す = SQLite は chunks を全走査して全一致行を整列してから
    /// 先頭 `cap + 1` 行を返す。つまり `LIMIT` が減らせるのは「返した行の
    /// materialize」(embedding の `vec_to_json` → JSON parse、本文の複製) だけで、
    /// 走査自体は KB 全体に比例したままだった。
    ///
    /// 実測 (9,419 チャンク、160 チャンクの文書を `cap = 32` で読む、30 回の中央値):
    /// **8.00 ms → 0.22 ms**。索引後の plan は `SEARCH c USING INDEX` で TEMP
    /// B-TREE も消える。索引の構築は 17 ms、DB サイズへの影響は測定誤差内。
    /// 効くのは絶対値より**次数**で、索引後の読み取りは KB の大きさではなく
    /// `cap` に比例する。
    fn ensure_chunk_order_index(&self) -> Result<()> {
        self.conn.execute_batch(
            "CREATE INDEX IF NOT EXISTS idx_chunks_doc_order ON chunks(document_id, chunk_index);",
        )?;
        Ok(())
    }

    /// `chunks.level` 列が存在しなければ追加する (idempotent)。
    /// legacy DB を開いても失敗しないよう init 経路から呼ぶ。
    /// 新規 DB では `CREATE TABLE` 時点で列があるので no-op。
    /// 既存行の `level` は NULL のまま (再 index で埋まる)。
    /// race 条件 (2 プロセス同時 open) の場合は duplicate column エラーを吸収。
    pub(super) fn ensure_chunk_level_column(&self) -> Result<()> {
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
    pub(super) fn ensure_context_text_column(&self) -> Result<()> {
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

    /// `documents.size_bytes` 列が存在しなければ追加する (idempotent、feature-51)。
    /// legacy DB を開いても失敗しないよう init 経路から呼ぶ。新規 DB では
    /// `CREATE TABLE` 時点で列があるので no-op。race 条件は duplicate column を吸収。
    ///
    /// 既存行は **NULL のまま**で、値は次の index 実行時に埋まる
    /// (`record_document_sizes`)。NULL を「読めない」と解釈すると、新 binary で
    /// 開いた瞬間に KB 全体が `resources` から消えるので、**NULL は提示を許す側**に
    /// 倒す。未記録が何件あるかは `groove doctor` が報告する。
    pub(super) fn ensure_document_size_column(&self) -> Result<()> {
        let has_col: bool = self
            .conn
            .prepare("PRAGMA table_info(documents)")?
            .query_map([], |row| row.get::<_, String>(1))?
            .filter_map(std::result::Result::ok)
            .any(|name| name == "size_bytes");
        if !has_col {
            match self
                .conn
                .execute_batch("ALTER TABLE documents ADD COLUMN size_bytes INTEGER;")
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
    pub(super) fn current_vec_dim(&self) -> Result<Option<u32>> {
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
    pub(super) fn ensure_vec_chunks_table(&self, dim: u32) -> Result<()> {
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

    /// `vec_chunks` を DROP して指定 `dim` で再生成する。
    /// 呼び出し側で `chunks` / `documents` の整合を別途管理すること
    /// (通常は [`Database::reset_for_model`] 経由で呼ぶ)。
    pub(super) fn recreate_vec_chunks(&self, dim: u32) -> Result<()> {
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
}
