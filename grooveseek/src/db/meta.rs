//! Index-level metadata, statistics, and whole-index maintenance for
//! [`Database`].
//!
//! Three things that are separate concerns but share one property: they are
//! about the index as a whole rather than about any one document. Reading and
//! writing `index_meta` (embedding model, dimension, context mode, the tags
//! parse-failure counter), counting what is stored, and the operations that
//! rewrite or relabel the whole index — `backfill_fts`, `backfill_quality`,
//! `reset_for_model`, the renames.
//!
//! `reset_for_model` is the sharpest of these: five writes (three DELETEs, the
//! `vec_chunks` rebuild, the `index_meta` update) that have to land as one
//! transaction, because a partial failure leaves a state no re-run repairs —
//! documents present with no chunks, or `vec_chunks` at a new dimension while
//! `index_meta` still names the old model.
//!
//! Split out of `db.rs` in AU-25 (PR-4), completing the item. The methods are
//! byte-identical and keep their visibility.

use super::*;

impl Database {
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

    /// context (contextual retrieval, feature-46) が実際に入っている chunk 数。
    ///
    /// `groove tune` が「`bm25_context_weight` 軸をこの KB で測定できるか」を
    /// 判定するために使う (feature-47)。`[contextual]` を有効化せずに index した
    /// KB では列が全て NULL / 空になり、context 重みを振っても bm25 スコアが
    /// 1 bit も動かない = 掃引結果が「効かない」ではなく「測れていない」。
    pub(crate) fn count_chunks_with_context(&self) -> Result<u32> {
        let count: u32 = self.conn.query_row(
            "SELECT COUNT(*) FROM chunks WHERE context_text IS NOT NULL AND context_text != ''",
            [],
            |row| row.get(0),
        )?;
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

    /// 現在の `tags_parse_failures` cumulative 値を返す (F-63、`groove status` 表示用)。
    ///
    /// `index_meta` の永続値 (= 過去 session までの累計) と本 session の AtomicU64
    /// delta (= 本 session 中に増えた失敗数) を合算する。codex P2 fix:
    /// **AtomicU64 は session-local delta** として持つ設計で、multi-instance で
    /// 同 SQLite file を開いた場合の last-writer-wins を回避する。
    ///
    /// DB read が失敗した場合 (= I/O エラー / schema 不整合等) は session delta だけを
    /// 返す best-effort 表示。`groove status` は人間向け診断なので panic より degrade。
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
                 Run `groove index --kb-path <path> --force --model {model}` to rebuild the index, \
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

    /// 既存 `documents` 行の `size_bytes` を**上書き**する (feature-51)。
    ///
    /// 走査したパスについて **groove が最後に測った実サイズ**を書く。条件付きの
    /// 「NULL のときだけ埋める」版があったが、それでは *古い記録* を直せず、
    /// 「cap を超えて成長した」「元に戻って cap 内になった」の**どちらの向きにも**
    /// stale な行が残った (codex P2 round 4-5)。この列は「read がこのファイルを
    /// 返せるか」を答えるためにあるので、真は常に**最後の実測値**。
    ///
    /// **これが無いと新列は事実上埋まらない**点も引き継ぐ: `rebuild_index` は
    /// content hash が一致する文書を `SingleResult::Unchanged` で返し、
    /// `upsert_document` も `update_document_meta` も通らないので、既存 KB の
    /// 大多数は列が追加されたことに気付かないまま NULL で残る。
    ///
    /// 行が無い path は何も更新しない (未索引ファイル / 初回登録の前段)。
    pub fn record_document_sizes(&self, sizes: &[(&str, u64)]) -> Result<u32> {
        let mut stmt = self
            .conn
            .prepare("UPDATE documents SET size_bytes = ?2 WHERE path = ?1")?;
        let mut count = 0u32;
        for (path, size) in sizes {
            count += stmt.execute(params![path, *size as i64])? as u32;
        }
        Ok(count)
    }

    /// 記録済み `size_bytes` が `min_bytes` を **超える** document を
    /// `(path, size_bytes)` で返す (feature-51)。
    ///
    /// 呼び出し側は「read cap のうち最小のもの」を渡し、返ってきた短いリストに
    /// 拡張子ごとの cap を当てる。cap の分岐は SQL では表現できない一方、
    /// **cap を超え得る文書だけを Rust に上げれば済む**ので、全行を読む必要はない。
    /// 実測では参照 KB 666 件中 0 件が該当する。
    ///
    /// `size_bytes` が NULL の行は `>` が真にならないので**返らない** = 未記録の
    /// 文書は「大きすぎる」と判定されない。これは意図した fail open で、
    /// 移行直後に KB 全体が提示から消えるのを防ぐ。
    pub fn documents_larger_than(&self, min_bytes: u64) -> Result<Vec<(String, u64)>> {
        let mut stmt = self.conn.prepare(
            "SELECT path, size_bytes FROM documents WHERE size_bytes > ?1 ORDER BY path",
        )?;
        let rows = stmt.query_map(params![min_bytes as i64], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)? as u64))
        })?;
        rows.into_iter()
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    /// `size_bytes` が未記録 (NULL) の document 数 (feature-51)。
    /// `groove doctor` が「1 回 index すれば埋まる」件数として報告する。
    pub fn documents_without_recorded_size(&self) -> Result<u32> {
        let count: u32 = self.conn.query_row(
            "SELECT COUNT(*) FROM documents WHERE size_bytes IS NULL",
            [],
            |row| row.get(0),
        )?;
        Ok(count)
    }

    // -- feature-51: `groove doctor` の整合性検査 -----------------------------
    //
    // 索引は 3 つのテーブルが 1 つの chunk について一致していることを前提に
    // 検索する: `chunks` が本文、`vec_chunks` が embedding、`fts_chunks` が
    // 全文検索行。**ずれても検索はエラーにならず、静かに取りこぼすだけ**なので、
    // 問える手段が要る。`backfill_fts` が存在すること自体が「fts 行の欠損は
    // 実際に起きる」の証拠で、これまでは full index を回すまで気付けなかった。

    /// 指定名のテーブル (仮想テーブルを含む) が存在するか。
    /// `vec_chunks` は embedding meta が書かれるまで作られないので、
    /// 検査の前に必ず確認する。
    fn has_table(&self, name: &str) -> Result<bool> {
        let n: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE name = ?1",
            params![name],
            |row| row.get(0),
        )?;
        Ok(n > 0)
    }

    /// 1 本の検査 SQL を「全件数」と「先頭 `sample_limit` 件のラベル」で答える。
    ///
    /// `label_sql` は **本モジュール内のリテラルのみ**を渡す (呼び出し側の入力を
    /// 混ぜない) ので、`format!` による組み立てに injection の面は無い。
    fn scan(&self, label_sql: &str, sample_limit: usize) -> Result<IntegrityScan> {
        // rusqlite has no `FromSql for u64`; SQLite counts are i64 and never
        // negative, so the cast is the narrowing one it looks like.
        let count: i64 =
            self.conn
                .query_row(&format!("SELECT COUNT(*) FROM ({label_sql})"), [], |row| {
                    row.get(0)
                })?;
        let count = count as u64;
        if count == 0 {
            return Ok(IntegrityScan::default());
        }
        let mut stmt = self.conn.prepare(&format!("{label_sql} LIMIT ?1"))?;
        let samples = stmt
            .query_map(params![sample_limit as i64], |row| row.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(IntegrityScan { count, samples })
    }

    /// `vec_chunks` テーブル自体が無いのに chunk が存在するか (feature-51)。
    ///
    /// この状態では**ベクトル検索が 1 件も返せない**が、下の 2 つの scan は
    /// 「走査対象が無い」= clean を返すしかない。テーブル欠損を独立した所見に
    /// しないと、FTS 行が揃っていれば `doctor` が "No issues found" と言って
    /// しまう (codex P1 round 1)。
    pub fn vector_table_missing_with_chunks(&self) -> Result<Option<u32>> {
        if self.has_table("vec_chunks")? {
            return Ok(None);
        }
        let chunks = self.chunk_count()?;
        Ok((chunks > 0).then_some(chunks))
    }

    /// 本文はあるが embedding が無い chunk。ベクトル検索から抜け落ちる。
    ///
    /// テーブルごと無い場合は 0 件を返す (走査できないため)。その状態は
    /// [`Self::vector_table_missing_with_chunks`] が別の所見として報告する。
    pub fn chunks_without_embedding(&self, sample_limit: usize) -> Result<IntegrityScan> {
        if !self.has_table("vec_chunks")? {
            return Ok(IntegrityScan::default());
        }
        self.scan(
            "SELECT d.path || ' #' || c.chunk_index
             FROM chunks c JOIN documents d ON d.id = c.document_id
             WHERE c.id NOT IN (SELECT chunk_id FROM vec_chunks)
             ORDER BY d.path, c.chunk_index",
            sample_limit,
        )
    }

    /// 対応する chunk が消えたのに残っている embedding。KNN が実体の無い
    /// chunk_id を返し、JOIN で落ちるので検索結果が黙って減る。
    pub fn embeddings_without_chunk(&self, sample_limit: usize) -> Result<IntegrityScan> {
        if !self.has_table("vec_chunks")? {
            return Ok(IntegrityScan::default());
        }
        self.scan(
            "SELECT 'vec_chunks chunk_id ' || chunk_id FROM vec_chunks
             WHERE chunk_id NOT IN (SELECT id FROM chunks)
             ORDER BY chunk_id",
            sample_limit,
        )
    }

    /// FTS 行が無い chunk。全文検索側からだけ見えなくなる
    /// (`backfill_fts` が次の index で補充する)。
    pub fn chunks_without_fts(&self, sample_limit: usize) -> Result<IntegrityScan> {
        self.scan(
            "SELECT d.path || ' #' || c.chunk_index
             FROM chunks c JOIN documents d ON d.id = c.document_id
             WHERE c.id NOT IN (SELECT rowid FROM fts_chunks)
             ORDER BY d.path, c.chunk_index",
            sample_limit,
        )
    }

    /// 対応する chunk が消えたのに残っている FTS 行。
    pub fn fts_without_chunk(&self, sample_limit: usize) -> Result<IntegrityScan> {
        self.scan(
            "SELECT 'fts_chunks rowid ' || rowid FROM fts_chunks
             WHERE rowid NOT IN (SELECT id FROM chunks)
             ORDER BY rowid",
            sample_limit,
        )
    }

    /// `document_id` に対応する `documents` 行が無い chunk (codex P2 round 6)。
    ///
    /// 外部キーは宣言してあるが、`PRAGMA foreign_keys` を切った別接続や破損で
    /// 実際に起き得る。**他のどの検査にも映らない**のが問題で、chunk 側の 2 つは
    /// `documents` を INNER JOIN するので、この chunk は走査対象から落ちる —
    /// vec / FTS 行が揃っていれば `doctor` は「異常なし」と言い、検索は
    /// document join で毎回この chunk を落とす。
    /// [`Self::documents_without_chunks`] のちょうど裏返し。
    pub fn chunks_without_document(&self, sample_limit: usize) -> Result<IntegrityScan> {
        self.scan(
            "SELECT 'chunk ' || c.id || ' (document_id ' || c.document_id || ')'
             FROM chunks c
             WHERE c.document_id NOT IN (SELECT id FROM documents)
             ORDER BY c.id",
            sample_limit,
        )
    }

    /// chunk を 1 つも持たない document。検索には決して出ない。
    pub fn documents_without_chunks(&self, sample_limit: usize) -> Result<IntegrityScan> {
        self.scan(
            "SELECT path FROM documents
             WHERE id NOT IN (SELECT DISTINCT document_id FROM chunks)
             ORDER BY path",
            sample_limit,
        )
    }

    /// legacy / 前回 index 済み DB で `quality_score` が DEFAULT 1.0 のままの
    /// チャンクを検出し、[`crate::quality::chunk_quality_score`] で再計算して
    /// UPDATE する (冪等)。
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

    /// `--force` 時の破壊的再初期化: `documents` / `chunks` / `vec_chunks`
    /// を全消ししてから新しい `(model, dim)` を記録する。`indexer::rebuild_index`
    /// が直後にすべての文書を再インデックスすることを前提とする。
    ///
    /// 5 つの書き込み (DELETE ×3 / vec_chunks 再生成 / index_meta 更新) は
    /// **1 つの transaction にまとめる**。途中で失敗ないし中断すると、
    /// 「documents は残っているのに chunks が空」「`vec_chunks` が新しい次元
    /// なのに `index_meta` は旧 model」といった、どの再実行経路でも自動修復
    /// されない状態が残るため。最悪なのは `recreate_vec_chunks` で DROP が
    /// 通って CREATE が落ちる場合で、`vec_chunks` が消えたまま何も代わりが
    /// 無くなる (dim が vec0 の上限 8192 を超えると実際に起きる)。
    ///
    /// 呼び出し側が既に transaction を張っている場合は自分では張らず、
    /// 親 transaction にそのまま参加する。SQLite は真のネスト transaction を
    /// 持たないため (`db-transaction-composition-pattern.md` 罠 1、
    /// `upsert_document` と同じ形)。
    pub fn reset_for_model(&self, model: &str, dim: u32) -> Result<()> {
        let local_tx = if self.conn.is_autocommit() {
            Some(self.conn.unchecked_transaction()?)
        } else {
            None
        };
        self.conn.execute_batch(
            "DELETE FROM fts_chunks; \
             DELETE FROM chunks; \
             DELETE FROM documents;",
        )?;
        self.recreate_vec_chunks(dim)?;
        self.write_embedding_meta(model, dim)?;
        // `?` で早期 return した場合は `local_tx` の Drop が ROLLBACK する。
        if let Some(tx) = local_tx {
            tx.commit()?;
        }
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

    /// index の状態を **1 回の読み取りとして** 取る (AU-71)。
    ///
    /// 返すのは `(documents, chunks, digest)`。
    ///
    /// ## なぜ 1 トランザクションなのか
    ///
    /// WAL では autocommit の文が**それぞれ別のスナップショット**を見る。
    /// `serve` の watcher が横で index している間に個別の COUNT を 3 回撃つと、
    /// documents は commit A、chunks は commit B の値を混ぜた
    /// **どの時点にも存在しなかった index** を記録し得る。
    /// DEFERRED tx で全部を同じスナップショットに揃える。
    ///
    /// ## なぜ chunk 本文を hash するのか
    ///
    /// `documents.content_hash` は**ファイルのバイト列**の hash なので、
    /// 「ソースは同じだが取り込まれ方が変わった」を捉えられない。
    /// 例: `exclude_headings` を別の見出しに変えて `--force` で貼り直すと、
    /// 索引される chunk は入れ替わるのに content_hash は全件不変で、
    /// chunk 数まで偶然一致すれば「変化なし」と報告してしまう。
    /// **検索されているのは source ではなく chunk** なので、chunk 側を測る。
    ///
    /// 完全ではない: frontmatter だけの変更は (off モードでは) chunk 本文に
    /// 出ないので digest が動かない。保証ではなく best-effort。
    ///
    /// **この digest の作り方を将来変えるなら、値に version を付けること。**
    /// 付けずに変えると、旧方式で記録された history と必ず食い違い、
    /// 実際には何も変わっていない run が 1 回だけ「corpus が変わった」と
    /// 報告される。今は初出なので比較対象が `None` しか無く問題にならない。
    pub fn corpus_snapshot(&self) -> Result<(u32, u32, String)> {
        use sha2::{Digest, Sha256};

        // 呼び出し側が既に read tx を開いているなら**それに乗る**。SQLite に
        // 真のネストトランザクションは無いので、ここで無条件に開くと
        // 「eval 全体を 1 スナップショットに固定する」呼び出し側の意図を壊す。
        // `storage.rs` の書き込み系と同じ `is_autocommit()` gate。
        let local_tx = if self.conn.is_autocommit() {
            Some(self.conn.unchecked_transaction()?)
        } else {
            None
        };
        let tx = &self.conn;
        let documents: u32 = tx.query_row("SELECT COUNT(*) FROM documents", [], |r| r.get(0))?;
        let chunks: u32 = tx.query_row("SELECT COUNT(*) FROM chunks", [], |r| r.get(0))?;

        // ORDER BY を SQL 側に持たせる。ここで整列しないと行順が実行計画依存に
        // なり、同一 index に対して run ごとに違う digest が出る。
        let mut stmt = tx.prepare(
            "SELECT d.path, c.chunk_index, c.heading, c.content, c.context_text
             FROM chunks c
             JOIN documents d ON d.id = c.document_id
             ORDER BY d.path, c.chunk_index",
        )?;
        let mut rows = stmt.query([])?;
        // 逐次 update する。コーパス全体の本文を 1 本の String に積むと
        // 数十 MB を無駄に確保することになる。
        let mut hasher = Sha256::new();
        while let Some(row) = rows.next()? {
            let path: String = row.get(0)?;
            let index: i64 = row.get(1)?;
            let heading: Option<String> = row.get(2)?;
            let content: String = row.get(3)?;
            let context: Option<String> = row.get(4)?;
            // 各 field を **NULL 判別子 + 長さ前置**で流す。
            //
            // 区切り文字方式にしないのは、その文字がデータ側に現れた瞬間に
            // 境界が曖昧になるため。NUL も妥当な UTF-8 文字なので、
            // `(heading="a", content="\0b")` と `(heading="a\0", content="b")` が
            // 同じバイト列になってしまう (codex P3 round 2)。
            //
            // `None` を `""` に潰さないのは、それが**索引状態の実際の差**だから。
            // docx の空段落が Heading style と通常 style の間で変わると、
            // parser は `Some("")` と `None` を出し分ける。しかも
            // `eval::is_hit` は `(Some(_), None)` を false、
            // `(Some(""), Some(""))` を true にするので、**recall に出る差**
            // でありながら digest が動かない、という状態になっていた
            // (codex P3 round 3)。
            let index_str = index.to_string();
            for field in [
                Some(path.as_str()),
                Some(index_str.as_str()),
                heading.as_deref(),
                Some(content.as_str()),
                context.as_deref(),
            ] {
                match field {
                    None => hasher.update([0u8]),
                    Some(s) => {
                        hasher.update([1u8]);
                        hasher.update((s.len() as u64).to_le_bytes());
                        hasher.update(s.as_bytes());
                    }
                }
            }
        }
        let digest = format!("{:x}", hasher.finalize());
        drop(rows);
        drop(stmt);
        // 自分で開いた tx だけを畳む。呼び出し側の tx をここで閉じてはならない。
        // 読み取り専用なので commit も rollback も等価だが、明示して
        // 「書いていない」ことを読み手に示す。
        if let Some(tx) = local_tx {
            tx.rollback()?;
        }
        Ok((documents, chunks, digest))
    }

    /// 索引済みの検索対象テキストを `(path, text)` で 1 パス流す (feature-52)。
    ///
    /// `groove eval` の golden query 混入検出が、コーパス全体に対して逐語一致を
    /// 探すために使う。**照合規則そのものはここに持たない** — 何を「含む」と
    /// みなすか (正規化・最小長・件数の閾値) は `eval` 側の純粋関数が全部持ち、
    /// この関数は行を渡すだけにする。規則が db と eval に分かれると、query 側と
    /// 本文側で別々に育って静かに食い違う。
    ///
    /// **chunk ごとに 1 回ではなく、検索対象フィールドごとに 1 回呼ぶ。**
    /// 流すのは `fts_chunks` が索引している 3 列 (`heading` / `context` /
    /// `content`) と同じもので、**この一致が唯一の選定規則**である。
    /// 探しているのは「検索で正解を押しのけ得るテキスト」なので、
    /// 押しのける力を持つ列と走査する列がずれた時点で嘘になる。
    ///
    /// 3 列がそれぞれ別に要る理由は、どれも他の 2 つに現れないテキストを持つから:
    ///
    /// - `heading` — Markdown parser は見出し行を content から**取り除いて**
    ///   ここに入れる (`parser/markdown.rs`)。しかも FTS は heading を本文より
    ///   重く索引する。content だけを見ると、golden query を `##` 見出しに
    ///   並べたノート (テストを記録する最も自然な形) が丸ごと見えない
    /// - `context` — パンくずの先頭は **frontmatter の title、無ければ
    ///   ファイル名**である (`markdown.rs` の `[title, ...ancestry, heading]`)。
    ///   title は heading でも content でもないので、ここを飛ばすと
    ///   「title にだけ query が入っている文書」が見えない。contextual indexing
    ///   が off の索引では空なので、その場合は自動的に何も増えない
    ///
    /// 連結せずフィールドごとに渡すのは、見出しの末尾と本文の先頭が隣接した
    /// 1 つの文字列に見えるのを避けるため (chunk をまたがない理由と同じ)。
    ///
    /// 行順は `(path, chunk_index)` で固定する。呼び出し側は集約するので順序に
    /// 依存しないが、実行計画依存の順序で流すと**再現しないバグを作れる**ように
    /// なるだけで、得るものが無い。
    ///
    /// 呼び出し側が read transaction を開いていればその上で流れる
    /// (`eval::run` は run 全体を 1 スナップショットに固定している)。
    pub fn for_each_indexed_text<F>(&self, mut f: F) -> Result<()>
    where
        F: FnMut(&str, &str),
    {
        let mut stmt = self.conn.prepare(
            "SELECT d.path, c.heading, c.context_text, c.content
             FROM chunks c
             JOIN documents d ON d.id = c.document_id
             ORDER BY d.path, c.chunk_index",
        )?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let path: String = row.get(0)?;
            let heading: Option<String> = row.get(1)?;
            let context: Option<String> = row.get(2)?;
            let content: String = row.get(3)?;
            for text in [heading.as_deref(), context.as_deref()]
                .into_iter()
                .flatten()
                .filter(|t| !t.is_empty())
            {
                f(&path, text);
            }
            f(&path, &content);
        }
        Ok(())
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
}
