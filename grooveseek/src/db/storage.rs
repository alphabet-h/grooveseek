//! Documents and chunks: the storage half of [`Database`].
//!
//! Writing a document is a multi-table operation — `documents`, `chunks`,
//! `fts_chunks`, `vec_chunks` all have to agree — and several of these methods
//! wrap their body in a transaction *only when the caller is not already inside
//! one*. SQLite has no true nested transaction, so composing with the indexer's
//! `begin_transaction()` means checking `is_autocommit()` rather than opening
//! unconditionally (`.dev/knowledge/db-transaction-composition-pattern.md`).
//!
//! Split out of `db.rs` in AU-25 (PR-3). The methods are byte-identical and
//! keep their visibility; an inherent method's `pub` does not depend on which
//! module its `impl` block sits in.

use super::*;

/// 1 チャンクを「BFS のシード」として扱う時の形: `(chunk_id, embedding, 中身)`。
/// `chunks_for_path*` の戻り値の要素型。
pub type SeedChunk = (i64, Vec<f32>, SearchResult);

/// (feature-56) What a chunk knows about the source file it came from.
///
/// Empty for every chunk a prose parser produces, which is why it defaults to nothing rather
/// than being required at the call site.
#[derive(Debug, Clone, Copy, Default)]
pub struct CodeMeta<'a> {
    /// 1-based inclusive line range of the chunk within its file.
    pub line_range: Option<(u32, u32)>,
    /// The grammar's own word for what kind of definition this is.
    pub symbol_kind: Option<&'a str>,
}

impl Database {
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
        size_bytes: u64,
    ) -> Result<i64> {
        let now = chrono::Utc::now().to_rfc3339();
        let tags_json = serde_json::to_string(tags)?;
        // SQLite has no unsigned integer type. A file large enough to overflow
        // i64 cannot exist, and both size caps refuse long before that, so the
        // cast is lossless for everything that reaches here.
        let size = size_bytes as i64;

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
                 last_indexed = ?8, size_bytes = ?9 WHERE id = ?10",
                params![
                    title,
                    topic,
                    category,
                    depth,
                    tags_json,
                    date,
                    content_hash,
                    now,
                    size,
                    doc_id
                ],
            )?;
            doc_id
        } else {
            self.conn.execute(
                "INSERT INTO documents (path, title, topic, category, depth, tags, date, content_hash, last_indexed, size_bytes)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                params![path, title, topic, category, depth, tags_json, date, content_hash, now, size],
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
        self.insert_chunk_with_code(
            document_id,
            chunk_index,
            heading,
            level,
            content,
            context,
            embedding,
            quality_score,
            CodeMeta::default(),
        )
    }

    /// (feature-56) Same, for a chunk that came from a source file and therefore knows where
    /// in that file it lives.
    ///
    /// A second constructor rather than three more parameters on [`Self::insert_chunk`]: that
    /// signature is called from over a hundred places, nearly all of them tests that have no
    /// opinion about source lines, and widening it would rewrite all of them to say `None`
    /// three times. The prose path keeps the call it already had.
    #[allow(clippy::too_many_arguments)]
    pub fn insert_chunk_with_code(
        &self,
        document_id: i64,
        chunk_index: i32,
        heading: Option<&str>,
        level: Option<u8>,
        content: &str,
        context: Option<&str>,
        embedding: &[f32],
        quality_score: f32,
        code: CodeMeta<'_>,
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
        let (start_line, end_line) = match code.line_range {
            Some((s, e)) => (Some(i64::from(s)), Some(i64::from(e))),
            None => (None, None),
        };
        self.conn.execute(
            "INSERT INTO chunks (document_id, chunk_index, heading, level, content, context_text, token_count, quality_score, start_line, end_line, symbol_kind)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![document_id, chunk_index, heading, level_bind, content, context, token_count, quality_score, start_line, end_line, code.symbol_kind],
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
        size_bytes: u64,
    ) -> Result<bool> {
        let tags_json = serde_json::to_string(tags)?;
        let updated_at = chrono::Utc::now().to_rfc3339();
        // This path is taken when the bytes changed but the chunks did not, so
        // the size can have changed too: writing everything else the new bytes
        // imply and leaving `size_bytes` behind would make the recorded size
        // disagree with the recorded hash.
        let size = size_bytes as i64;
        let n = self.conn.execute(
            "UPDATE documents
                SET title = ?1,
                    topic = ?2,
                    category = ?3,
                    depth = ?4,
                    tags = ?5,
                    date = ?6,
                    content_hash = ?7,
                    last_indexed = ?8,
                    size_bytes = ?9
              WHERE path = ?10",
            params![
                title,
                topic,
                category,
                depth,
                tags_json,
                date,
                content_hash,
                updated_at,
                size,
                path
            ],
        )?;
        Ok(n > 0)
    }

    /// 指定 `path` に属するチャンクを (chunk_id, embedding, SearchResult) で返す。
    /// Connection Graph の起点シード取得用。存在しなければ empty Vec。
    ///
    /// 上限なし。**新規の呼び出しは [`Self::chunks_for_path_limited`] を使うこと**
    /// (BU-33: 1 文書のチャンク数に上限が無く、160 チャンクの文書が 160 個の
    /// シードになって BFS のコストを決めていた)。本メソッドは既存呼び出し互換の
    /// ために残した薄い委譲。
    pub fn chunks_for_path(&self, path: &str) -> Result<Vec<SeedChunk>> {
        self.chunks_for_path_limited(path, u32::MAX)
    }

    /// 起点チャンクを **最大 `cap` 件**返し、「まだ続きがあるか」を第 2 要素で返す
    /// (BU-33)。
    ///
    /// `cap + 1` 件を SQL に要求して 1 件多く返ってきたかで判定する。呼び出し側で
    /// `+1` を書かせないのは、そこが「上限を SQL に降ろす」唯一の接点だからで、
    /// 引数を素通しにすると *cap を無視した読み取り* が結果を変えずに書けてしまう
    /// (= テストで検出できない退行になる)。
    ///
    /// **代償はプローブ行 1 行**: 打ち切りが起きている時、`cap + 1` 行目は
    /// embedding を JSON 経由で `Vec<f32>` に復元し本文も複製してから捨てられる
    /// (1024 次元で約 4 KB)。「上限を超えた行は読まない」は**厳密には 1 行ぶん
    /// 嘘**なので、docs でもそう書いている。追加クエリ 1 本 (`COUNT(*)`) との
    /// 交換で、1 行の無駄読みの方が安いと判断した。
    pub fn chunks_for_path_capped(&self, path: &str, cap: u32) -> Result<(Vec<SeedChunk>, bool)> {
        let mut rows = self.chunks_for_path_limited(path, cap.saturating_add(1))?;
        let has_more = rows.len() > cap as usize;
        rows.truncate(cap as usize);
        Ok((rows, has_more))
    }

    /// [`Self::chunks_for_path`] に SQL 側の `LIMIT` を付けたもの。
    ///
    /// 打ち切りを **SQL に降ろす**のが要点で、Rust 側の `.take(n)` では意味が無い
    /// (行はすべて `vec_to_json` でテキスト化され、`Vec<f32>` に parse され、
    /// チャンク本文ごと materialize されてから捨てられる = BU-33 が名指しした
    /// 「上限の無い読み取り」がそのまま残る)。
    ///
    /// `embedding` は `vec_to_json` で JSON 文字列として取り出し、serde_json で
    /// `Vec<f32>` に復元する。`SearchResult.score` はシード node 用に 1.0 を入れる
    /// (BFS 結果のスコアと同じ意味 = cos sim 換算値の上限)。
    fn chunks_for_path_limited(&self, path: &str, limit: u32) -> Result<Vec<SeedChunk>> {
        let sql = "
            SELECT c.id, vec_to_json(v.embedding),
                   c.content, c.heading, c.document_id,
                   d.path, d.title, d.topic, d.date, d.tags
            FROM chunks c
            JOIN documents d ON d.id = c.document_id
            JOIN vec_chunks v ON v.chunk_id = c.id
            WHERE d.path = ?1
            ORDER BY c.chunk_index
            LIMIT ?2
        ";
        let mut stmt = self.conn.prepare(sql)?;
        let rows = stmt.query_map(params![path, limit as i64], |row| {
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
                    // (feature-56) The graph surface does not carry definition metadata —
                    // `GraphNode` has no place to put it — so it is not selected here either.
                    start_line: None,
                    end_line: None,
                    symbol_kind: None,
                },
            ));
        }
        Ok(out)
    }

    /// (feature-56) Rewrite the code columns of a document's chunks, in chunk order.
    ///
    /// Exists for the path where a file changed but its chunk *text* did not: inserting a
    /// blank line above a function, or editing a comment short enough to be dropped as a thin
    /// gap, moves every definition below it without changing a single chunk body. That path
    /// deliberately skips re-embedding — the vectors are still correct — but the line numbers
    /// are not, and a stale line number is worse than none: it sends a reader to the wrong
    /// place with no sign that anything is off.
    ///
    /// Positional by `chunk_index`, which is sound precisely because the caller has just
    /// established that the chunk texts match one for one.
    pub fn update_chunk_code_meta(&self, path: &str, metas: &[CodeMeta<'_>]) -> Result<()> {
        let local_tx = if self.conn.is_autocommit() {
            Some(self.conn.unchecked_transaction()?)
        } else {
            None
        };
        let mut stmt = self.conn.prepare(
            "UPDATE chunks SET start_line = ?1, end_line = ?2, symbol_kind = ?3
             WHERE chunk_index = ?4
               AND document_id = (SELECT id FROM documents WHERE path = ?5)",
        )?;
        for (index, meta) in metas.iter().enumerate() {
            let (start, end) = match meta.line_range {
                Some((s, e)) => (Some(i64::from(s)), Some(i64::from(e))),
                None => (None, None),
            };
            stmt.execute(params![start, end, meta.symbol_kind, index as i64, path])?;
        }
        drop(stmt);
        if let Some(tx) = local_tx {
            tx.commit()?;
        }
        Ok(())
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
            "SELECT chunk_index, content, token_count, level, start_line, end_line, symbol_kind
               FROM chunks
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
                // (feature-56) NULL on every prose chunk, and on code chunks
                // written before these columns existed — same read as the
                // search path does at `db/search.rs`.
                start_line: row.get::<_, Option<u32>>(4)?,
                end_line: row.get::<_, Option<u32>>(5)?,
                symbol_kind: row.get::<_, Option<String>>(6)?,
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
}
