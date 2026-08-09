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
    /// `kb-mcp tune` が「`bm25_context_weight` 軸をこの KB で測定できるか」を
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

        let tx = self.conn.unchecked_transaction()?;
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
            // 各 field を**長さ前置**で流す。区切り文字方式は、その文字が
            // データ側に現れた瞬間に境界が曖昧になる。NUL も妥当な UTF-8 文字
            // なので、`(heading="a", content="\0b")` と
            // `(heading="a\0", content="b")` が同じバイト列になってしまう
            // (codex P3)。長さ前置なら区切りに使える文字を仮定しない。
            for field in [
                path.as_str(),
                &index.to_string(),
                heading.as_deref().unwrap_or(""),
                content.as_str(),
                context.as_deref().unwrap_or(""),
            ] {
                hasher.update((field.len() as u64).to_le_bytes());
                hasher.update(field.as_bytes());
            }
        }
        let digest = format!("{:x}", hasher.finalize());
        drop(rows);
        drop(stmt);
        // 読み取り専用なので commit も rollback も等価。Drop の rollback に任せず
        // 明示して「書いていない」ことを読み手に示す。
        tx.rollback()?;
        Ok((documents, chunks, digest))
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
