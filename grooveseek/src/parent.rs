//! Parent document retriever (display-time content expansion).
//!
//! After relevance is finalized (RRF / reranker / MMR), this stage rewrites
//! `SearchHit.content` to include surrounding context. Two strategies:
//!
//! - small chunk (`token_count < whole_doc_threshold_tokens`) → whole-document fallback
//! - otherwise → adjacent merge `[N-1, N, N+1]` bounded by document edges
//!
//! Invariants:
//! - `SearchHit.score` is NOT modified (relevance reflects original chunk)
//! - `quality_filter` is NOT applied (low-score neighbors are kept as context)
//! - NULL `chunks.level` (legacy DBs) → adjacent merge fallback (Task 3.4)

use crate::db::{ChunkRow, Database, ExpandedRange, SearchHit};

/// chunk の token 数を推定する。`token_count` 列が `Some(n)` なら n を、
/// `None` (legacy DB 行で NULL) なら content から `bytes / 4` で逆算する
/// (`Database::insert_chunk` 内の計算式と整合)。負値は 0 に clamp。
///
/// **目的**: `max_expanded_tokens` cap が NULL token_count 行で bypass される
/// 脆弱性 (codex P1) を防ぐ。estimate なので overhead は無視できる。
fn estimated_tokens(chunk: &ChunkRow) -> u32 {
    match chunk.token_count {
        Some(n) if n > 0 => n as u32,
        _ => (chunk.content.len() / 4) as u32,
    }
}

/// `token_count` が `threshold` 未満のとき small chunk と判定する。
/// `<` strict less than = `token == threshold` のとき small ではない (= adjacent merge path)。
/// `token_count = None` (legacy DB / 計測失敗) は false (= small ではない、adjacent path)。
///
/// **設計判断 (F-52)**: 既存 `expand_parent` 内 inline ロジック (`is_small =
/// token_count.map(|t| (t as u32) < threshold).unwrap_or(false)`) を抽出。
/// 直接 proptest できる pure fn にすることで境界値 (token == threshold で
/// is_small == false) の regression を catch する (= F-49 の
/// `compute_reranker_input_limit` 抽出 pattern と整合)。
pub(crate) fn is_small_chunk(token_count: Option<i64>, threshold: u32) -> bool {
    token_count.map(|t| (t as u32) < threshold).unwrap_or(false)
}

/// 連結した `rows` が覆う 1-based inclusive の行範囲。**全 row が範囲を持つとき
/// だけ** `(Some(min), Some(max))` を返し、1 つでも欠けるか空なら `(None, None)`。
///
/// **all-or-nothing の理由 (AV-08)**: 範囲を持つ row だけで min/max を取ると、
/// 範囲の外にある本文が `content` に混ざったまま「返した本文はこの範囲から来て
/// いる」と主張することになる。`docs/behavior.md` が約束しているのはその主張
/// そのものなので、揃わないときは**主張しない** — [`SearchHit`] の 3 つの行
/// フィールドは `skip_serializing_if` 付きなので、`None` は「キーごと不在」= 既に
/// 散文 hit が取っている形になり、client から見た shape は変わらない。
///
/// 散文だけの KB では 3 列が全 NULL なので必ず `(None, None)` を返す = 代入の
/// 前後で値が変わらない。行メタは ranking にも入らないので、eval の
/// 「parent retriever は metric に触らない」invariant はこの関数を足しても保たれる。
fn merged_line_bounds(rows: &[ChunkRow]) -> (Option<u32>, Option<u32>) {
    let mut min_start = None::<u32>;
    let mut max_end = None::<u32>;
    for row in rows {
        let (Some(start), Some(end)) = (row.start_line, row.end_line) else {
            return (None, None);
        };
        min_start = Some(min_start.map_or(start, |m| m.min(start)));
        max_end = Some(max_end.map_or(end, |m| m.max(end)));
    }
    (min_start, max_end)
}

/// Parent retriever 設定。groove.toml `[search.parent_retriever]` と
/// 1:1 対応する。Task 3.5 で `ParentRetrieverConfig` を加えた後はこの構造体を
/// 該当 config から構築する。
#[derive(Debug, Clone, Copy)]
pub struct ParentRetrieverParams {
    pub whole_doc_threshold_tokens: u32,
    pub max_expanded_tokens: u32,
}

/// `Vec<(chunk_id, SearchHit)>` 全件に対して Parent retriever を適用する。
///
/// - `enabled = false` なら入力をそのまま `Vec<SearchHit>` に変換して返す
///   (chunk_id を捨てるだけで content / expanded_from は触らない)。
/// - `enabled = true` なら各 hit に対して `expand_parent` を呼び、`content`
///   と `expanded_from` を rewrite した結果を返す。
///
/// 失敗 (DB error 等) があった hit は元の hit を返す (best-effort 拡張)。
/// `expand_parent` は `SearchHit.match_spans` を defensive に `None` に
/// クリアするので、呼び出し側は拡張後 `content` に対して
/// `compute_match_spans` を再計算する責務がある。
///
/// 3 caller (MCP server / CLI search / eval) で同じ wire を 3 回書かないため
/// の共通 helper。eval は match_spans を計算しないが、本 helper の責務外
/// なので問題ない。
pub fn apply_parent_retriever(
    hits: Vec<(i64, SearchHit)>,
    db: &Database,
    enabled: bool,
    params: ParentRetrieverParams,
) -> Vec<SearchHit> {
    if !enabled {
        return hits.into_iter().map(|(_, h)| h).collect();
    }
    hits.into_iter()
        .map(|(chunk_id, hit)| {
            let fallback = hit.clone();
            expand_parent(hit, chunk_id, db, params).unwrap_or(fallback)
        })
        .collect()
}

/// 1 hit の `content` を P4 戦略で拡張する。`chunk_id` を起点に DB から
/// 前後 chunk (or 同 doc 全 chunk) を引き、連結後の `SearchHit` を返す。
///
/// 入力 `hit` は MMR / reranker 後の relevance 順位を持つもの。`score` /
/// `match_spans` には触らない (match_spans は呼び出し側で再計算する)。
pub fn expand_parent(
    hit: SearchHit,
    chunk_id: i64,
    db: &Database,
    params: ParentRetrieverParams,
) -> anyhow::Result<SearchHit> {
    let (doc_id, chunk_idx, token_count) = db.get_chunk_meta(chunk_id)?;

    // small chunk: token_count が threshold 未満なら whole-doc fallback。
    // token_count = None (legacy / 計測失敗) は adjacent merge にフォールバック
    // (保守的: small かどうか判断不能なので展開しすぎないよう adjacent を選ぶ)。
    let is_small = is_small_chunk(token_count, params.whole_doc_threshold_tokens);

    if is_small {
        expand_whole_document(hit, doc_id, chunk_idx, db, params)
    } else {
        expand_adjacent(hit, doc_id, chunk_idx, db, params)
    }
}

fn expand_adjacent(
    mut hit: SearchHit,
    doc_id: i64,
    chunk_idx: i64,
    db: &Database,
    params: ParentRetrieverParams,
) -> anyhow::Result<SearchHit> {
    // Adjacent merge は最大 3 行 (前 / hit / 後) が想定だが、SQLite LIMIT には
    // 余裕を持って 16 行を渡す。万一 chunk_index に gap があっても安全側に倒れる。
    let neighbors = db.fetch_chunks_by_index_range(doc_id, chunk_idx - 1, chunk_idx + 1, 16)?;
    if neighbors.is_empty() {
        return Ok(hit);
    }
    // max_expanded_tokens cap (Task 3.4 invariant #1):
    // adjacent neighbor 群の token_count 合計が cap を超えるなら拡張を放棄し、
    // hit chunk のみを返す (Adjacent { from = to = chunk_idx })。
    // **codex P1 (#43826e9 後の review)**: token_count = None (legacy DB 行で
    // NULL) を 0 扱いすると cap が bypass され巨大 chunk が merge される。
    // 安全側に倒すため content から estimate (= insert_chunk 内の token_count
    // 計算式 `content.len() / 4` と整合)。これで NULL row も実 byte 数で
    // 評価され、cap を保つ。
    // **2026-05-03 audit Code C2**: 累積を `u64` で行うことで `u32` 加算 wrap
    // による cap bypass (極端に大きい chunk が連続する場合の理論上の risk) を
    // 防ぐ。realistic な KB では trigger されないが defense-in-depth。
    let total_tokens: u64 = neighbors.iter().map(|c| estimated_tokens(c) as u64).sum();
    if total_tokens > params.max_expanded_tokens as u64 {
        // hit chunk が neighbors に含まれていれば content を復元する。
        // find が None になるのは DB inconsistency (chunk_idx と fetch range の
        // 不整合) という rare case のみで、その場合 hit.content は元のまま残す
        // (= 未定義 content での書き換え回避の defensive ガード継続)。
        if let Some(c) = neighbors.into_iter().find(|c| c.chunk_index == chunk_idx) {
            hit.content = c.content;
        }
        // F-51 (audit-todos): cap-degrade した事実を always 通知する invariant。
        // caller (run_search_pipeline) は expanded_from を見て match_spans の
        // 再計算判断をするため、find 成否に関わらず clear / set する。これで
        // find 失敗時も拡張前 match_spans が stale で残らず、observability
        // 上 cap-exceeded path 通過が常に検出できる。
        hit.match_spans = None;
        // AV-08: **この経路では行メタを触らない。** 上の `find` が当たれば
        // `hit.content` に入るのは hit chunk 自身の content で、search が返した
        // ものと同じ文字列 (どちらも `chunks.content` を素で読む。context 合成も
        // スニペット化も挟まない)。当たらなければ `hit.content` は書き換わらない。
        // どちらの答えでも本文は hit chunk のものなので、行範囲は正しいまま —
        // ここで `None` にするのは情報を捨てるだけになる。
        //
        // 「content を差し替えたら派生メタも無効」という `match_spans` の推論は
        // **content が実際に差し替わった経路にしか当てはまらない**。
        hit.expanded_from = Some(ExpandedRange::Adjacent {
            from_index: chunk_idx as usize,
            to_index: chunk_idx as usize,
        });
        return Ok(hit);
    }

    let mut sorted = neighbors;
    sorted.sort_by_key(|c| c.chunk_index);
    let from_idx = sorted
        .iter()
        .map(|c| c.chunk_index)
        .min()
        .unwrap_or(chunk_idx) as usize;
    let to_idx = sorted
        .iter()
        .map(|c| c.chunk_index)
        .max()
        .unwrap_or(chunk_idx) as usize;
    let merged: String = sorted
        .iter()
        .map(|c| c.content.clone())
        .collect::<Vec<_>>()
        .join("\n\n");
    hit.content = merged;
    // content が拡張されたので元 chunk 基準の byte offset で計算済み
    // match_spans は invalidate する。呼び出し側 (run_search_pipeline) が
    // 拡張後 content に対して compute_match_spans を再計算する責務だが、
    // ここで defensive に None クリアしておけば「再計算忘れ」で stale offset
    // が leak することを防げる。
    hit.match_spans = None;
    // AV-08: 行メタは content と同じ材料 (`sorted`) から作り直す。継承しない。
    //
    // **`match_spans` と扱いが違う理由**: あちらは無条件 `None` でよい —
    // 呼び出し側に「拡張後 content に対して再計算する」受け皿があるので、
    // ここでの None は「まだ計算していない」を意味する。行メタには受け皿が
    // 無いので、None は「範囲を主張できない」を意味する終端の答えになる。
    // 同じ「派生メタは無効」という推論から、片方は clear、もう片方は
    // recompute という違う結論が出る。
    (hit.start_line, hit.end_line) = merged_line_bounds(&sorted);
    // 複数の定義を跨いだので「この chunk が持つ定義の種別」に単一の答えが無い。
    hit.symbol_kind = None;
    hit.expanded_from = Some(ExpandedRange::Adjacent {
        from_index: from_idx,
        to_index: to_idx,
    });
    Ok(hit)
}

fn expand_whole_document(
    hit: SearchHit,
    doc_id: i64,
    chunk_idx: i64,
    db: &Database,
    params: ParentRetrieverParams,
) -> anyhow::Result<SearchHit> {
    // 同 doc 全 chunks fetch、ただし `max_rows` 上限付き。
    // quality_filter は parent retriever expansion では適用しない (spec
    // invariant #6): 周辺 chunk が低 quality_score でも context として含む。
    // fetch_chunks_by_index_range は quality_score を filter 条件に持たない。
    //
    // **2026-05-03 audit Sec H-1+H-3**: 巨大 doc (例: 100 MiB の単一 .md) を
    // 不可避に Vec<ChunkRow> に materialize すると、cap check の前に大量メモリ
    // を消費する。max_expanded_tokens から上限 row 数を派生させ、SQL LIMIT
    // で上流ガードする。1 chunk あたり最低 4 byte / 1 token と仮定すると、
    // `max_expanded_tokens × 2 + 64` rows もあれば overshoot 判定に十分。
    // この cap に hit したら whole-doc 路は破綻と判断して adjacent fallback。
    let row_cap = params
        .max_expanded_tokens
        .saturating_mul(2)
        .saturating_add(64);
    let chunks = db.fetch_chunks_by_index_range(doc_id, 0, i64::MAX, row_cap)?;
    if chunks.is_empty() {
        return Ok(hit);
    }
    if chunks.len() as u32 >= row_cap {
        // row_cap で truncate された = doc が想定より遥かに大きい。
        // 全文 merge は無理なので adjacent merge にフォールバック。
        return expand_adjacent(hit, doc_id, chunk_idx, db, params);
    }
    // max_expanded_tokens cap (Task 3.4 invariant #1):
    // 全 doc 連結が cap を超えるなら adjacent merge にフォールバックする。
    // adjacent merge 自身も同 cap を持つので、最終的には hit chunk のみまで
    // 縮退し得る (= cap 超過時の strong guarantee)。
    // NULL token_count (legacy DB) でも cap を bypass できないよう
    // estimated_tokens で content から逆算する (codex P1 対応)。
    // **2026-05-03 audit Code C2**: `u64` 累積で u32 wrap による cap bypass
    // を防ぐ defense-in-depth。
    let total_tokens: u64 = chunks.iter().map(|c| estimated_tokens(c) as u64).sum();
    if total_tokens > params.max_expanded_tokens as u64 {
        return expand_adjacent(hit, doc_id, chunk_idx, db, params);
    }
    let total_chunks = chunks.len();
    let merged: String = chunks
        .iter()
        .map(|c| c.content.clone())
        .collect::<Vec<_>>()
        .join("\n\n");
    let mut hit = hit;
    hit.content = merged;
    // adjacent merge と同様に defensive クリア。呼び出し側 (run_search_pipeline)
    // が拡張後 content に対して compute_match_spans を再計算する責務。
    hit.match_spans = None;
    // AV-08: adjacent merge と同じ規則で `chunks` から作り直す。ここに到達した
    // 時点で `chunks` は truncate されていないその doc の全 chunk である —
    // row_cap で切られた場合と token cap を超えた場合はどちらも上で
    // `expand_adjacent` へ委譲して return しているので、この範囲は
    // 「文書全体」として正当。
    (hit.start_line, hit.end_line) = merged_line_bounds(&chunks);
    hit.symbol_kind = None;
    hit.expanded_from = Some(ExpandedRange::WholeDocument { total_chunks });
    Ok(hit)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{CodeMeta, Database};

    // tempdir helper: db.rs の test mod の TempPath パターンを踏襲。
    // (db.rs と parent.rs は同 crate 内、再利用するか自前定義するか
    // 状況に応じて、ここでは小型 helper 自前定義)
    struct TempPath(std::path::PathBuf);
    impl Drop for TempPath {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    fn tempdir_for_test() -> TempPath {
        let p = crate::test_support::unique_temp_path("groove-parent-test");
        std::fs::create_dir_all(&p).unwrap();
        TempPath(p)
    }

    fn dummy_emb_384() -> Vec<f32> {
        vec![0.1_f32; 384]
    }

    fn make_hit(path: &str, content: &str) -> SearchHit {
        SearchHit {
            start_line: None,
            end_line: None,
            symbol_kind: None,
            score: 1.0,
            path: path.into(),
            title: None,
            heading: None,
            topic: None,
            date: None,
            tags: vec![],
            content: content.into(),
            match_spans: None,
            expanded_from: None,
        }
    }

    fn params() -> ParentRetrieverParams {
        ParentRetrieverParams {
            whole_doc_threshold_tokens: 100,
            max_expanded_tokens: 2000,
        }
    }

    /// 3 chunks ([0, 1, 2]) を同 doc に insert、中間の chunk_id を hit にして
    /// adjacent merge が前後を含めて 3 chunks を連結することを確認。
    /// 各 chunk の content は token_count >= 100 (= ~400 byte) になるように
    /// 十分長くして whole-doc fallback を踏まないようにする。
    #[test]
    fn test_parent_adjacent_merge_3_chunks() {
        let tmp = tempdir_for_test();
        let path = tmp.0.join("test.db");
        let db = Database::open(path.to_str().unwrap()).expect("open");
        db.verify_embedding_meta("bge-small-en-v1.5", 384)
            .expect("vec_chunks");
        let doc_id = db
            .upsert_document(
                "/doc.md",
                Some("d"),
                Some("t"),
                None,
                None,
                &[],
                None,
                "h",
                0,
            )
            .expect("upsert");
        // 各 chunk content を ~900 byte (= token_count ~225) にして adjacent
        // 経路に確実に乗せる。"alpha" / "beta" / "gamma" を marker として残す。
        let alpha_body = format!("alpha {}", "body content body content ".repeat(40));
        let beta_body = format!("beta {}", "body content body content ".repeat(40));
        let gamma_body = format!("gamma {}", "body content body content ".repeat(40));
        let c0 = db
            .insert_chunk(
                doc_id,
                0,
                Some("h0"),
                None,
                &alpha_body,
                None,
                &dummy_emb_384(),
                1.0,
            )
            .expect("c0");
        let c1 = db
            .insert_chunk(
                doc_id,
                1,
                Some("h1"),
                None,
                &beta_body,
                None,
                &dummy_emb_384(),
                1.0,
            )
            .expect("c1");
        let _c2 = db
            .insert_chunk(
                doc_id,
                2,
                Some("h2"),
                None,
                &gamma_body,
                None,
                &dummy_emb_384(),
                1.0,
            )
            .expect("c2");

        let hit = make_hit("/doc.md", &beta_body);
        let expanded = expand_parent(hit, c1, &db, params()).expect("expand");
        assert!(expanded.content.contains("alpha"));
        assert!(expanded.content.contains("beta"));
        assert!(expanded.content.contains("gamma"));
        match expanded.expanded_from {
            Some(ExpandedRange::Adjacent {
                from_index: 0,
                to_index: 2,
            }) => {}
            other => panic!("expected Adjacent {{0,2}}, got {other:?}"),
        }
        // unused, drops c0 ref clean
        let _ = c0;
    }

    /// chunk_index = 0 (doc の左端) で hit、左拡張なしで [0, 1] のみ返ることを確認。
    /// content は token_count >= 100 にして whole-doc fallback を踏まないようにする。
    #[test]
    fn test_parent_adjacent_at_doc_boundary_left() {
        let tmp = tempdir_for_test();
        let path = tmp.0.join("test.db");
        let db = Database::open(path.to_str().unwrap()).expect("open");
        db.verify_embedding_meta("bge-small-en-v1.5", 384)
            .expect("vec_chunks");
        let doc_id = db
            .upsert_document(
                "/doc.md",
                Some("d"),
                Some("t"),
                None,
                None,
                &[],
                None,
                "h",
                0,
            )
            .expect("upsert");
        let alpha_body = format!("alpha {}", "body content body content ".repeat(40));
        let beta_body = format!("beta {}", "body content body content ".repeat(40));
        let c0 = db
            .insert_chunk(
                doc_id,
                0,
                Some("h0"),
                None,
                &alpha_body,
                None,
                &dummy_emb_384(),
                1.0,
            )
            .expect("c0");
        let _c1 = db
            .insert_chunk(
                doc_id,
                1,
                Some("h1"),
                None,
                &beta_body,
                None,
                &dummy_emb_384(),
                1.0,
            )
            .expect("c1");

        let hit = make_hit("/doc.md", &alpha_body);
        let expanded = expand_parent(hit, c0, &db, params()).expect("expand");
        match expanded.expanded_from {
            Some(ExpandedRange::Adjacent {
                from_index: 0,
                to_index: 1,
            }) => {}
            other => panic!("expected Adjacent {{0,1}}, got {other:?}"),
        }
    }

    /// token_count が whole_doc_threshold_tokens (= 100) 未満の chunk hit に
    /// 対しては whole document 全 chunks を連結して返す。
    /// expanded_from は WholeDocument variant。
    #[test]
    fn test_parent_whole_doc_for_small_chunk() {
        let tmp = tempdir_for_test();
        let path = tmp.0.join("test.db");
        let db = Database::open(path.to_str().unwrap()).expect("open");
        db.verify_embedding_meta("bge-small-en-v1.5", 384)
            .expect("vec_chunks");
        let doc_id = db
            .upsert_document(
                "/doc.md",
                Some("d"),
                Some("t"),
                None,
                None,
                &[],
                None,
                "h",
                0,
            )
            .expect("upsert");

        // chunk_index=0 が small (token_count=30 < 100), c1 / c2 は普通サイズ
        let c0 = db
            .insert_chunk(
                doc_id,
                0,
                Some("h0"),
                None,
                "header",
                None,
                &dummy_emb_384(),
                1.0,
            )
            .expect("c0");
        // 上記の insert_chunk は token_count を内部で content.len()/4 で計算する。
        // "header" = 6 byte なので token_count = 1。これは threshold (100) 未満。
        let _c1 = db
            .insert_chunk(
                doc_id,
                1,
                Some("h1"),
                None,
                // 200 tokens 相当の長文 (~800 byte content) を作る
                &"longer body content body content body content".repeat(20),
                None,
                &dummy_emb_384(),
                1.0,
            )
            .expect("c1");
        let _c2 = db
            .insert_chunk(
                doc_id,
                2,
                Some("h2"),
                None,
                &"another longer body block another body block".repeat(20),
                None,
                &dummy_emb_384(),
                1.0,
            )
            .expect("c2");

        let hit = make_hit("/doc.md", "header");
        let expanded = expand_parent(hit, c0, &db, params()).expect("expand");
        match expanded.expanded_from {
            Some(ExpandedRange::WholeDocument { total_chunks: 3 }) => {}
            other => panic!("expected WholeDocument {{total_chunks: 3}}, got {other:?}"),
        }
        assert!(expanded.content.contains("header"));
        assert!(expanded.content.contains("longer body"));
        assert!(expanded.content.contains("another longer body"));
    }

    /// quality_filter が parent retriever expansion で適用されないことを確認:
    /// 周辺 chunk が低 quality_score でも content に含まれる。
    #[test]
    fn test_parent_quality_filter_not_applied_in_expansion() {
        let tmp = tempdir_for_test();
        let path = tmp.0.join("test.db");
        let db = Database::open(path.to_str().unwrap()).expect("open");
        db.verify_embedding_meta("bge-small-en-v1.5", 384)
            .expect("vec_chunks");
        let doc_id = db
            .upsert_document(
                "/doc.md",
                Some("d"),
                Some("t"),
                None,
                None,
                &[],
                None,
                "h",
                0,
            )
            .expect("upsert");
        // c1 (hit chunk) は normal quality、c0 と c2 は超低 quality (0.05)
        let _c0 = db
            .insert_chunk(
                doc_id,
                0,
                Some("h0"),
                None,
                "low quality body content",
                None,
                &dummy_emb_384(),
                0.05, // 低 quality_score
            )
            .expect("c0");
        let c1 = db
            .insert_chunk(
                doc_id,
                1,
                Some("h1"),
                None,
                "hit body content body content body content body content body content",
                None,
                &dummy_emb_384(),
                1.0,
            )
            .expect("c1");
        let _c2 = db
            .insert_chunk(
                doc_id,
                2,
                Some("h2"),
                None,
                "another low quality body content",
                None,
                &dummy_emb_384(),
                0.05, // 低 quality_score
            )
            .expect("c2");

        let hit = make_hit("/doc.md", "hit body content");
        let expanded = expand_parent(hit, c1, &db, params()).expect("expand");
        // 周辺 chunk が低 quality_score でも content に含まれる (= filter 非適用)
        assert!(
            expanded.content.contains("low quality body content"),
            "low quality neighbor (left) should be included as context"
        );
        assert!(
            expanded
                .content
                .contains("another low quality body content"),
            "low quality neighbor (right) should be included as context"
        );
    }

    /// max_expanded_tokens cap: 巨大 chunk (token_count ~5000 each) で
    /// max_expanded = 2000 のとき、adjacent merge は cap を超えるので
    /// hit chunk のみ返す (Adjacent {from_index = to_index = chunk_idx})。
    #[test]
    fn test_parent_max_expanded_caps_at_threshold() {
        let tmp = tempdir_for_test();
        let path = tmp.0.join("test.db");
        let db = Database::open(path.to_str().unwrap()).expect("open");
        db.verify_embedding_meta("bge-small-en-v1.5", 384)
            .expect("vec_chunks");
        let doc_id = db
            .upsert_document(
                "/doc.md",
                Some("d"),
                Some("t"),
                None,
                None,
                &[],
                None,
                "h",
                0,
            )
            .expect("upsert");
        // 各 chunk content を ~20000 byte (= token_count ~5000) にする。
        let big_body = format!("big {}", "body content body content ".repeat(800));
        let _c0 = db
            .insert_chunk(
                doc_id,
                0,
                Some("h0"),
                None,
                &big_body,
                None,
                &dummy_emb_384(),
                1.0,
            )
            .expect("c0");
        let c1 = db
            .insert_chunk(
                doc_id,
                1,
                Some("h1"),
                None,
                &big_body,
                None,
                &dummy_emb_384(),
                1.0,
            )
            .expect("c1");
        let _c2 = db
            .insert_chunk(
                doc_id,
                2,
                Some("h2"),
                None,
                &big_body,
                None,
                &dummy_emb_384(),
                1.0,
            )
            .expect("c2");

        let hit = make_hit("/doc.md", "big body");
        let p = ParentRetrieverParams {
            whole_doc_threshold_tokens: 100,
            max_expanded_tokens: 2000,
        };
        let expanded = expand_parent(hit, c1, &db, p).expect("expand");
        // adjacent 3 chunks の合計 = ~15000 token > max=2000、cap で hit chunk のみ
        match expanded.expanded_from {
            Some(ExpandedRange::Adjacent {
                from_index: 1,
                to_index: 1,
            }) => {}
            other => panic!("expected Adjacent {{1,1}} (cap reduced), got {other:?}"),
        }
    }

    /// NULL chunk_level (legacy DB 行) でも adjacent merge が機能することを確認 (invariant #7 guard)。
    /// この test は実際には現実装で既に PASS する (adjacent merge は level を読まない)。
    /// regression guard として残す。
    #[test]
    fn test_parent_null_level_falls_back_to_adjacent() {
        let tmp = tempdir_for_test();
        let path = tmp.0.join("test.db");
        let db = Database::open(path.to_str().unwrap()).expect("open");
        db.verify_embedding_meta("bge-small-en-v1.5", 384)
            .expect("vec_chunks");
        let doc_id = db
            .upsert_document(
                "/doc.md",
                Some("d"),
                Some("t"),
                None,
                None,
                &[],
                None,
                "h",
                0,
            )
            .expect("upsert");
        // 全 chunk で level = None (= NULL) を明示的に渡す
        let body = format!("body {}", "content body content body ".repeat(40));
        let _c0 = db
            .insert_chunk(
                doc_id,
                0,
                Some("h0"),
                None,
                &body,
                None,
                &dummy_emb_384(),
                1.0,
            )
            .expect("c0");
        let c1 = db
            .insert_chunk(
                doc_id,
                1,
                Some("h1"),
                None,
                &body,
                None,
                &dummy_emb_384(),
                1.0,
            )
            .expect("c1");
        let _c2 = db
            .insert_chunk(
                doc_id,
                2,
                Some("h2"),
                None,
                &body,
                None,
                &dummy_emb_384(),
                1.0,
            )
            .expect("c2");

        let hit = make_hit("/doc.md", "body content");
        let expanded = expand_parent(hit, c1, &db, params()).expect("expand");
        // level NULL でも adjacent merge は機能する
        match expanded.expanded_from {
            Some(ExpandedRange::Adjacent {
                from_index: 0,
                to_index: 2,
            }) => {}
            other => panic!("expected Adjacent {{0,2}} for NULL-level chunks, got {other:?}"),
        }
    }

    /// CJK (日本語) を含む content で expand_parent が panic / char boundary 違反を
    /// 起こさない smoke test。compute_match_spans 自体は server.rs の既存 path で
    /// 検証済 (call site は run_search_pipeline、Task 3.6 で wire される)、ここは
    /// expand_parent が日本語入りの content を一度も切らずにそのまま渡せるかの確認。
    #[test]
    fn test_parent_cjk_content_no_panic() {
        let tmp = tempdir_for_test();
        let path = tmp.0.join("test.db");
        let db = Database::open(path.to_str().unwrap()).expect("open");
        db.verify_embedding_meta("bge-small-en-v1.5", 384)
            .expect("vec_chunks");
        let doc_id = db
            .upsert_document(
                "/doc.md",
                Some("d"),
                Some("t"),
                None,
                None,
                &[],
                None,
                "h",
                0,
            )
            .expect("upsert");
        // 日本語 + マルチバイト UTF-8 (絵文字、半角全角混在)
        let jp_body = format!("日本語の本文 {}", "テキストてきすと ".repeat(40));
        let _c0 = db
            .insert_chunk(
                doc_id,
                0,
                Some("h0"),
                None,
                &jp_body,
                None,
                &dummy_emb_384(),
                1.0,
            )
            .expect("c0");
        let c1 = db
            .insert_chunk(
                doc_id,
                1,
                Some("h1"),
                None,
                &jp_body,
                None,
                &dummy_emb_384(),
                1.0,
            )
            .expect("c1");
        let _c2 = db
            .insert_chunk(
                doc_id,
                2,
                Some("h2"),
                None,
                &jp_body,
                None,
                &dummy_emb_384(),
                1.0,
            )
            .expect("c2");

        let hit = make_hit("/doc.md", "日本語");
        let expanded = expand_parent(hit, c1, &db, params()).expect("expand");
        // panic しなければ OK。content に日本語が含まれること。
        assert!(expanded.content.contains("日本語の本文"));
        match expanded.expanded_from {
            Some(ExpandedRange::Adjacent { .. }) => {}
            other => panic!("expected Adjacent variant, got {other:?}"),
        }
    }

    /// codex P1 regression: NULL token_count (legacy DB 行) でも cap が
    /// bypass されないことを確認。content から estimate (`bytes / 4`) で
    /// 評価され、巨大 chunk なら hit only に degrade される。
    #[test]
    fn test_parent_max_expanded_cap_with_null_token_count() {
        let tmp = tempdir_for_test();
        let path = tmp.0.join("test.db");
        let db = Database::open(path.to_str().unwrap()).expect("open");
        db.verify_embedding_meta("bge-small-en-v1.5", 384)
            .expect("vec_chunks");
        let doc_id = db
            .upsert_document(
                "/doc.md",
                Some("d"),
                Some("t"),
                None,
                None,
                &[],
                None,
                "h",
                0,
            )
            .expect("upsert");
        // 各 chunk content を ~20000 byte (= estimated tokens ~5000) に。
        // legacy NULL token_count を simulate するため、insert 後に直接 SQL で
        // chunks.token_count = NULL を設定する。
        let big_body = format!("big {}", "body content body content ".repeat(800));
        let _c0 = db
            .insert_chunk(
                doc_id,
                0,
                Some("h0"),
                None,
                &big_body,
                None,
                &dummy_emb_384(),
                1.0,
            )
            .expect("c0");
        let c1 = db
            .insert_chunk(
                doc_id,
                1,
                Some("h1"),
                None,
                &big_body,
                None,
                &dummy_emb_384(),
                1.0,
            )
            .expect("c1");
        let _c2 = db
            .insert_chunk(
                doc_id,
                2,
                Some("h2"),
                None,
                &big_body,
                None,
                &dummy_emb_384(),
                1.0,
            )
            .expect("c2");

        // legacy DB 行を simulate: 全 chunks の token_count を NULL にする
        let conn = rusqlite::Connection::open(path.to_str().unwrap()).expect("re-open");
        conn.execute("UPDATE chunks SET token_count = NULL", [])
            .expect("null token_count");
        drop(conn);

        let hit = make_hit("/doc.md", "big body");
        let p = ParentRetrieverParams {
            whole_doc_threshold_tokens: 100,
            max_expanded_tokens: 2000,
        };
        let expanded = expand_parent(hit, c1, &db, p).expect("expand");
        // NULL でも estimated_tokens 経由で content から逆算され、cap (2000)
        // を超えると判定される → hit chunk only に degrade
        match expanded.expanded_from {
            Some(ExpandedRange::Adjacent {
                from_index: 1,
                to_index: 1,
            }) => {}
            other => panic!(
                "expected Adjacent {{1,1}} (cap-degrade with NULL token_count), got {other:?}"
            ),
        }
    }

    /// F-51 regression catcher: `expand_adjacent` cap-exceeded branch で
    /// `find(|c| c.chunk_index == chunk_idx)` が None を返す経路 (= DB
    /// inconsistency / fetch range が hit chunk を除外する rare case) で、
    /// `match_spans = None` clear と `expanded_from = Some(Adjacent {chunk_idx, chunk_idx})`
    /// set が **無条件** に行われることを assert する。
    ///
    /// **simulation**: c_prev (idx=0) と c_next (idx=2) のみ insert (各 ~5000 tokens)、
    /// hit chunk (idx=1) は **doc 内に存在しない gap** とする。test は `expand_adjacent`
    /// を直接呼び (Rust visibility ルール: 子 module は親 module の private アイテムに
    /// アクセス可)、`chunk_idx=1` を渡す:
    /// - `fetch_chunks_by_index_range(doc_id, 0, 2, 16)` → c_prev (idx=0) + c_next (idx=2) の 2 件返却
    /// - `total_tokens` ≈ 10000 > cap=2000 → cap-exceeded branch
    /// - `find(|c| c.chunk_index == 1)` → idx=0 / idx=2 マッチなし = None = find 失敗
    /// - F-51 fix 後: `match_spans=None` + `expanded_from=Some(Adjacent {1, 1})` 無条件 set
    ///
    /// fix 適用前: `if let Some` ガードで何もせず return → assert fail (= TDD red)
    /// fix 適用後: 無条件 set 実施 → assert pass (= TDD green)
    #[test]
    fn test_parent_cap_exceeded_with_missing_hit_chunk() {
        let tmp = tempdir_for_test();
        let path = tmp.0.join("test.db");
        let db = Database::open(path.to_str().unwrap()).expect("open");
        db.verify_embedding_meta("bge-small-en-v1.5", 384)
            .expect("vec_chunks");
        let doc_id = db
            .upsert_document(
                "/doc.md",
                Some("d"),
                Some("t"),
                None,
                None,
                &[],
                None,
                "h",
                0,
            )
            .expect("upsert");
        // c_prev (idx=0) と c_next (idx=2) のみ insert (idx=1 は gap)。各 ~20000 byte
        // (= token_count ~5000) で 2 chunks 合計 = 10000 > cap=2000 を満たす。
        let big_body = format!("big {}", "body content body content ".repeat(800));
        let _c_prev = db
            .insert_chunk(
                doc_id,
                0,
                Some("h0"),
                None,
                &big_body,
                None,
                &dummy_emb_384(),
                1.0,
            )
            .expect("c_prev");
        let _c_next = db
            .insert_chunk(
                doc_id,
                2,
                Some("h2"),
                None,
                &big_body,
                None,
                &dummy_emb_384(),
                1.0,
            )
            .expect("c_next");

        let original_marker = "ORIGINAL_HIT_CONTENT_MARKER";
        let hit = make_hit("/doc.md", original_marker);
        let p = ParentRetrieverParams {
            whole_doc_threshold_tokens: 100,
            max_expanded_tokens: 2000,
        };
        // expand_adjacent を直接呼ぶ (private fn、`mod tests` から super:: で呼出し可)。
        // chunk_idx=1 を artificially 渡す = doc 内に idx=1 chunk は存在しないので find 失敗を保証。
        let expanded = expand_adjacent(hit, doc_id, 1, &db, p).expect("expand_adjacent");

        // F-51 fix 適用前は以下の 3 assert がすべて fail する:
        // - expanded_from は None のまま (`if let Some` ガード内で set されないため)
        // - match_spans は None のまま (これは make_hit の初期値が None なので fix 前後で同じ、
        //   ただし `Some(...)` を仕込んだ test だと clear が無条件であることを区別できる -
        //   本 test では make_hit の初期値が None なので clear の 「無条件性」は expanded_from
        //   set の有無で代替検出する)
        // - content は元のまま (find 失敗で書き換えなし、fix 前後で同じ)
        match expanded.expanded_from {
            Some(ExpandedRange::Adjacent {
                from_index: 1,
                to_index: 1,
            }) => {}
            ref other => {
                panic!("expected Adjacent {{1, 1}} (cap-degrade with find-failure), got {other:?}")
            }
        }
        assert!(
            expanded.match_spans.is_none(),
            "match_spans must be None after cap-degrade (find failure path)"
        );
        // hit.content は make_hit 由来の original_marker のまま (find 失敗で `hit.content =
        // c.content` 経路は通らない、`if let Some(c)` ガード残存を担保)
        assert_eq!(
            expanded.content, original_marker,
            "hit.content must be unchanged when find fails (if let Some guard preserves it)"
        );
    }

    /// F-53: `apply_parent_retriever(enabled=false)` の pass-through 確認。
    /// `content` / `expanded_from` / `match_spans` の 3 field がすべて入力時から
    /// 不変であることを直接 assert する。enabled=false 経路 (line 59-61) は
    /// chunk_id を捨てるだけで他 field は触らない invariant を guard する
    /// regression catcher。
    ///
    /// **MatchSpan 比較**: `MatchSpan` (db.rs:52) は `PartialEq` 未 derive (= MCP
    /// serialize/deserialize の本質型なので加算 derive は別 cycle 判断)。本 test は
    /// field (start / end) 個別 assert で比較する。
    #[test]
    fn test_apply_parent_retriever_disabled_pass_through() {
        use crate::db::MatchSpan;

        let tmp = tempdir_for_test();
        let path = tmp.0.join("test.db");
        // Database::open + verify_embedding_meta は既存 helper pattern を流用。
        // enabled=false 経路は DB を参照しないが、将来 signature が変わって DB を触る経路が増えた
        // 場合の安全性のため initialize は省略しない。
        let db = Database::open(path.to_str().unwrap()).expect("open");
        db.verify_embedding_meta("bge-small-en-v1.5", 384)
            .expect("vec_chunks");

        // 入力 hit に 3 field を pre-set (= pass-through で全 field 不変であることを assert)
        let mut hit = make_hit("/doc.md", "input content");
        hit.expanded_from = Some(ExpandedRange::WholeDocument { total_chunks: 5 });
        hit.match_spans = Some(vec![MatchSpan { start: 10, end: 20 }]);

        let input: Vec<(i64, SearchHit)> = vec![(42, hit)];
        let result = apply_parent_retriever(input, &db, false, params());

        assert_eq!(result.len(), 1);
        let out = &result[0];
        assert_eq!(out.content, "input content");
        match out.expanded_from {
            Some(ExpandedRange::WholeDocument { total_chunks: 5 }) => {}
            ref other => panic!("expanded_from must be unchanged, got {other:?}"),
        }
        // MatchSpan は PartialEq 未 derive、field 個別 assert で比較
        let spans = out
            .match_spans
            .as_ref()
            .expect("match_spans must be Some after pass-through");
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].start, 10);
        assert_eq!(spans[0].end, 20);
    }

    // ---- AV-08: 行メタは展開後の content と一致していなければならない ----------
    //
    // 交差 (code chunk × parent retriever) はこの節が入るまでテストにも fixture にも
    // 一度も存在しなかった。`search_parent_integration.rs` の fixture は Markdown だけ、
    // `code_formats_cli.rs` は parent retriever を有効化しない。

    /// 行メタを持つ code hit を作る。既存 12 本が使う [`make_hit`] は
    /// `start_line: None` を明示しているので、**[`make_hit`] に引数を足すのではなく
    /// 別 helper を新設する** — 既存の呼び出しに一切触れないため。
    fn make_code_hit(
        path: &str,
        content: &str,
        start_line: u32,
        end_line: u32,
        symbol_kind: &str,
    ) -> SearchHit {
        SearchHit {
            start_line: Some(start_line),
            end_line: Some(end_line),
            symbol_kind: Some(symbol_kind.into()),
            ..make_hit(path, content)
        }
    }

    /// adjacent merge は 3 chunk を連結するので、行範囲は連結した全 chunk を
    /// 覆う `[min(start), max(end)]` でなければならない。`symbol_kind` は
    /// 複数定義を跨いだ時点で単一の答えを失うので落ちる。
    ///
    /// **fix 前の落ち方**: 入力 hit の `Some(41)` / `Some(80)` / `Some("function")` が
    /// そのまま残るので 3 つの assert がすべて落ちる。min↔max を取り違えた実装は
    /// `Some(81)` / `Some(40)` を返すので、start / end を個別に assert して分離する。
    #[test]
    fn test_parent_adjacent_merge_recomputes_line_bounds() {
        let tmp = tempdir_for_test();
        let path = tmp.0.join("test.db");
        let db = Database::open(path.to_str().unwrap()).expect("open");
        db.verify_embedding_meta("bge-small-en-v1.5", 384)
            .expect("vec_chunks");
        let doc_id = db
            .upsert_document(
                "/lib.rs",
                Some("d"),
                Some("t"),
                None,
                None,
                &[],
                None,
                "h",
                0,
            )
            .expect("upsert");
        // ~900 byte (= token_count ~225) で adjacent 経路に確実に乗せる。
        let alpha = format!("fn alpha() {{}} {}", "// body body body ".repeat(50));
        let beta = format!("fn beta() {{}} {}", "// body body body ".repeat(50));
        let gamma = format!("fn gamma() {{}} {}", "// body body body ".repeat(50));
        let _c0 = db
            .insert_chunk_with_code(
                doc_id,
                0,
                Some("alpha"),
                None,
                &alpha,
                None,
                &dummy_emb_384(),
                1.0,
                CodeMeta {
                    line_range: Some((1, 40)),
                    symbol_kind: Some("function"),
                },
            )
            .expect("c0");
        let c1 = db
            .insert_chunk_with_code(
                doc_id,
                1,
                Some("beta"),
                None,
                &beta,
                None,
                &dummy_emb_384(),
                1.0,
                CodeMeta {
                    line_range: Some((41, 80)),
                    symbol_kind: Some("function"),
                },
            )
            .expect("c1");
        let _c2 = db
            .insert_chunk_with_code(
                doc_id,
                2,
                Some("gamma"),
                None,
                &gamma,
                None,
                &dummy_emb_384(),
                1.0,
                CodeMeta {
                    line_range: Some((81, 120)),
                    symbol_kind: Some("function"),
                },
            )
            .expect("c2");

        let hit = make_code_hit("/lib.rs", &beta, 41, 80, "function");
        let expanded = expand_parent(hit, c1, &db, params()).expect("expand");
        // 拡張が実際に起きたことを先に固定する (assert が空振りしないため)。
        match expanded.expanded_from {
            Some(ExpandedRange::Adjacent {
                from_index: 0,
                to_index: 2,
            }) => {}
            ref other => panic!("expected Adjacent {{0,2}}, got {other:?}"),
        }
        assert!(expanded.content.contains("fn alpha"));
        assert!(expanded.content.contains("fn gamma"));
        // 行範囲は連結した 3 chunk を覆う。start / end を個別に見て swap を殺す。
        assert_eq!(expanded.start_line, Some(1));
        assert_eq!(expanded.end_line, Some(120));
        // 3 つの定義を跨いだので「この chunk の定義の種別」は答えを持たない。
        assert!(expanded.symbol_kind.is_none());
    }

    /// 隣接 chunk のうち 1 つでも行範囲を持たなければ、範囲は主張できない。
    /// Some を持つ row だけで min/max を取ると、範囲外の本文が `content` に
    /// 混ざったまま「本文はこの範囲から来ている」と言うことになる。
    ///
    /// **fix 前の落ち方**: 入力 hit の `Some(41)` が残るので `is_none()` が落ちる。
    /// 「Some の row だけで min/max」実装では `Some(1)` / `Some(80)` になるので、
    /// この 1 本が `||` の 2 つ目の答え (欠損あり) を単独で殺す。
    #[test]
    fn test_parent_adjacent_merge_drops_bounds_when_a_neighbor_has_none() {
        let tmp = tempdir_for_test();
        let path = tmp.0.join("test.db");
        let db = Database::open(path.to_str().unwrap()).expect("open");
        db.verify_embedding_meta("bge-small-en-v1.5", 384)
            .expect("vec_chunks");
        let doc_id = db
            .upsert_document(
                "/lib.rs",
                Some("d"),
                Some("t"),
                None,
                None,
                &[],
                None,
                "h",
                0,
            )
            .expect("upsert");
        let alpha = format!("fn alpha() {{}} {}", "// body body body ".repeat(50));
        let beta = format!("fn beta() {{}} {}", "// body body body ".repeat(50));
        // gap chunk: import 行や top-level 文と同じく行範囲を持たない。
        let gap = format!("use std::io; {}", "// body body body ".repeat(50));
        let _c0 = db
            .insert_chunk_with_code(
                doc_id,
                0,
                Some("alpha"),
                None,
                &alpha,
                None,
                &dummy_emb_384(),
                1.0,
                CodeMeta {
                    line_range: Some((1, 40)),
                    symbol_kind: Some("function"),
                },
            )
            .expect("c0");
        let c1 = db
            .insert_chunk_with_code(
                doc_id,
                1,
                Some("beta"),
                None,
                &beta,
                None,
                &dummy_emb_384(),
                1.0,
                CodeMeta {
                    line_range: Some((41, 80)),
                    symbol_kind: Some("function"),
                },
            )
            .expect("c1");
        // `insert_chunk` は `CodeMeta::default()` を渡すので行範囲は NULL。
        let _c2 = db
            .insert_chunk(
                doc_id,
                2,
                Some("gap"),
                None,
                &gap,
                None,
                &dummy_emb_384(),
                1.0,
            )
            .expect("c2");

        let hit = make_code_hit("/lib.rs", &beta, 41, 80, "function");
        let expanded = expand_parent(hit, c1, &db, params()).expect("expand");
        // 拡張は起きている (= 範囲を落とす理由が「拡張しなかった」ではない)。
        match expanded.expanded_from {
            Some(ExpandedRange::Adjacent {
                from_index: 0,
                to_index: 2,
            }) => {}
            ref other => panic!("expected Adjacent {{0,2}}, got {other:?}"),
        }
        assert!(expanded.content.contains("use std::io"));
        assert!(expanded.start_line.is_none());
        assert!(expanded.end_line.is_none());
        assert!(expanded.symbol_kind.is_none());
    }

    /// whole-document 展開でも同じ規則。row_cap で truncate された場合と
    /// token cap を超えた場合はどちらも [`expand_adjacent`] へ委譲して
    /// return するので、merge に到達した `chunks` は truncate されていない
    /// その doc の全 chunk = 範囲は「文書全体」として正当。
    ///
    /// **fix 前の落ち方**: hit は真ん中の chunk なので `Some(4)` / `Some(6)` が残り、
    /// start / end の両方が落ちる (端の chunk を hit にすると片側が偶然一致する)。
    #[test]
    fn test_parent_whole_doc_recomputes_line_bounds() {
        let tmp = tempdir_for_test();
        let path = tmp.0.join("test.db");
        let db = Database::open(path.to_str().unwrap()).expect("open");
        db.verify_embedding_meta("bge-small-en-v1.5", 384)
            .expect("vec_chunks");
        let doc_id = db
            .upsert_document(
                "/lib.rs",
                Some("d"),
                Some("t"),
                None,
                None,
                &[],
                None,
                "h",
                0,
            )
            .expect("upsert");
        // 全 chunk を small (token_count < 100) にして whole-doc 経路に乗せる。
        for (idx, (body, range)) in [
            ("type A = u8;", (1u32, 3u32)),
            ("type B = u16;", (4, 6)),
            ("type C = u32;", (7, 9)),
        ]
        .iter()
        .enumerate()
        {
            db.insert_chunk_with_code(
                doc_id,
                idx as i32,
                Some("alias"),
                None,
                body,
                None,
                &dummy_emb_384(),
                1.0,
                CodeMeta {
                    line_range: Some(*range),
                    symbol_kind: Some("class"),
                },
            )
            .expect("insert");
        }
        // 真ん中を hit にするので、その id を取り直す。
        let c1 = db
            .insert_chunk_with_code(
                doc_id,
                3,
                Some("alias"),
                None,
                "type D = u64;",
                None,
                &dummy_emb_384(),
                1.0,
                CodeMeta {
                    line_range: Some((10, 12)),
                    symbol_kind: Some("class"),
                },
            )
            .expect("c3");

        let hit = make_code_hit("/lib.rs", "type D = u64;", 10, 12, "class");
        let expanded = expand_parent(hit, c1, &db, params()).expect("expand");
        match expanded.expanded_from {
            Some(ExpandedRange::WholeDocument { total_chunks: 4 }) => {}
            ref other => panic!("expected WholeDocument {{total_chunks: 4}}, got {other:?}"),
        }
        assert_eq!(expanded.start_line, Some(1));
        assert_eq!(expanded.end_line, Some(12));
        assert!(expanded.symbol_kind.is_none());
    }

    /// cap-degrade は行メタを**触ってはいけない**。この経路で `hit.content` に
    /// 入るのは hit chunk 自身の content (`c.content` は search が返したものと
    /// 同一文字列) で、find が失敗する経路では content が一切書き換わらない。
    /// どちらの答えでも行範囲は正しいままなので、消すのは純粋な情報損失。
    ///
    /// **このテストは fix 前から緑**。守っているのは「3 経路すべてでクリアする」
    /// という素朴な実装 (= 台帳が指示していた形) が入ることで、そのとき単独で赤になる。
    #[test]
    fn test_parent_cap_degrade_keeps_the_hit_line_range() {
        let tmp = tempdir_for_test();
        let path = tmp.0.join("test.db");
        let db = Database::open(path.to_str().unwrap()).expect("open");
        db.verify_embedding_meta("bge-small-en-v1.5", 384)
            .expect("vec_chunks");
        let doc_id = db
            .upsert_document(
                "/lib.rs",
                Some("d"),
                Some("t"),
                None,
                None,
                &[],
                None,
                "h",
                0,
            )
            .expect("upsert");
        // 各 chunk ~20000 byte (= token_count ~5000) で cap 2000 を超えさせる。
        let big = format!("fn big() {{}} {}", "// body body body ".repeat(1200));
        let mut ids = Vec::new();
        for (idx, range) in [(0i32, (1u32, 500u32)), (1, (501, 1000)), (2, (1001, 1500))] {
            ids.push(
                db.insert_chunk_with_code(
                    doc_id,
                    idx,
                    Some("big"),
                    None,
                    &big,
                    None,
                    &dummy_emb_384(),
                    1.0,
                    CodeMeta {
                        line_range: Some(range),
                        symbol_kind: Some("function"),
                    },
                )
                .expect("insert"),
            );
        }
        let c1 = ids[1];

        let hit = make_code_hit("/lib.rs", &big, 501, 1000, "function");
        let expanded = expand_parent(hit, c1, &db, params()).expect("expand");
        // cap で hit chunk のみに縮退したことを固定する。
        match expanded.expanded_from {
            Some(ExpandedRange::Adjacent {
                from_index: 1,
                to_index: 1,
            }) => {}
            ref other => panic!("expected Adjacent {{1,1}} (cap reduced), got {other:?}"),
        }
        assert_eq!(expanded.start_line, Some(501));
        assert_eq!(expanded.end_line, Some(1000));
        assert_eq!(expanded.symbol_kind.as_deref(), Some("function"));
    }

    /// 空 slice では範囲を主張しない。[`merged_line_bounds`] の 2 つの呼び出し元は
    /// どちらも直前に `is_empty()` で早期 return する (`neighbors` / `chunks`) ので
    /// 統合経路からは到達できない = pure fn を直接呼ぶ以外にこの答えを覆う手が無い。
    #[test]
    fn test_merged_line_bounds_on_empty_slice_claims_nothing() {
        assert_eq!(merged_line_bounds(&[]), (None, None));
    }

    proptest::proptest! {
        /// F-52: `is_small_chunk` の `<` strict less than 境界値固定。
        /// token == threshold のとき is_small == false (= adjacent merge path) を proptest 化。
        /// 将来 `<=` や `<` の判定ロジックが書き換わる regression を catch する。
        ///
        /// **range**: `token in 0i64..100_000` は実運用 token_count 範囲 (= chunk content
        /// `bytes / 4`、典型的に < 10K) を十分カバー。`i64` 全域 / `i64::MAX` 級は `t as u32`
        /// cast truncation の挙動と乖離するため scope 外。
        /// `threshold in 1u32..1_000` は `whole_doc_threshold_tokens` の実運用範囲 (= default 100、typical 50-500)。
        #[test]
        fn prop_is_small_chunk_strict_less_than(
            token in 0i64..100_000_i64,
            threshold in 1u32..1_000_u32,
        ) {
            let is_small = is_small_chunk(Some(token), threshold);
            proptest::prop_assert_eq!(is_small, (token as u32) < threshold);
            // 境界: token == threshold は is_small == false
            if (token as u32) == threshold {
                proptest::prop_assert!(!is_small, "boundary: token == threshold must yield is_small == false");
            }
        }

        /// `token_count = None` のとき is_small は無条件 false (= adjacent path)。
        #[test]
        fn prop_is_small_chunk_none_yields_false(threshold in 1u32..10_000_u32) {
            proptest::prop_assert!(!is_small_chunk(None, threshold));
        }
    }
}
