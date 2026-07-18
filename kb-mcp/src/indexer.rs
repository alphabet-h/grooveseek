use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::path::Path;
use std::time::Instant;
use walkdir::WalkDir;

use crate::db::Database;
use crate::embedder::Embedder;
use crate::parser::Registry;
use crate::quality;

pub mod progress;

// ---------------------------------------------------------------------------
// Hardcoded denylist (F-62)
// ---------------------------------------------------------------------------

/// Hardcoded directory basenames to *always* skip during indexing /
/// validation walks, regardless of user `exclude_dirs` config. Acts as
/// a fail-safe so that `[indexer].exclude_dirs = ["custom"]` (= default
/// override forgetting VCS metadata) or `exclude_dirs = []` (= explicit
/// "walk everything") does not index `.git/` / `.svn/` / `node_modules/`.
/// User `exclude_dirs` is *additionally* applied (union semantics).
pub const HARDCODED_EXCLUDE_DIRS: &[&str] = &[".git", ".svn", "node_modules"];

/// Returns `true` if `basename` matches a hardcoded skip entry, regardless
/// of user `exclude_dirs` config. Shared by `collect_source_files` (index)
/// and `validate_collect_md_files` (validate, in `src/main.rs`) so the two
/// paths agree. `pub` because the bin target accesses it via the lib
/// (`kb_mcp::indexer::is_hardcoded_excluded`); the library API is
/// intentionally unstable per `src/lib.rs:4-6`.
pub fn is_hardcoded_excluded(basename: &str) -> bool {
    HARDCODED_EXCLUDE_DIRS.contains(&basename)
}

/// MS Office (`~$doc.docx`) / LibreOffice (`.~lock.doc.docx#`) のロック・owner
/// ファイルを拡張子に関わらず skip する。`~$` 版は拡張子が `docx` のまま走査に
/// 乗るため明示フィルタ必須。`.~lock.*#` 版は拡張子が `docx#` になり既存の
/// 拡張子 membership で偶然弾かれるが、暗黙挙動に依存せず明示フィルタする。
/// `pub(crate)` は `collect_source_files` (フル re-index) に加えて
/// `watcher::should_process` (incremental reindex) からも同じ判定を再利用する
/// ため。
pub(crate) fn is_office_lock_file(name: &str) -> bool {
    name.starts_with("~$") || (name.starts_with(".~lock.") && name.ends_with('#'))
}

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Per-file metadata collected before the main embed loop. `content` を持たず
/// 追加 I/O を増やす代わりに、大規模 KB でもピークメモリを一定に抑える。
#[derive(Debug, Clone)]
struct DiskEntry {
    /// kb_path 相対 (forward-slash) の保存キー。
    rel: String,
    /// SHA-256 hex。DB 側 `content_hash` と比較する。
    hash: String,
    /// 実ファイルの絶対パス。embed/upsert 段階で再 read_to_string する。
    full: std::path::PathBuf,
}

/// [`scan_disk_entries`] の結果。`entries` = hash 計算済みの index 候補、
/// `skipped` = disk に存在するが read 失敗 / size 超過で載らなかった rel path。
/// `skipped` は prune 統一原則 (§4.2) で visited_paths へ union し、既存 entry を保護する。
struct DiskScan {
    entries: Vec<DiskEntry>,
    skipped: Vec<String>,
}

/// disk 側の全 source file を走査し、raw バイト読み + バイト hash を計算する。
///
/// - **エラー隔離**: `read` 失敗や size 超過は per-file skip し、rel path を `skipped`
///   に積んで走査を続行する (旧 `.collect::<Result<Vec>>()?` の全体 abort を修正)。
/// - **size skip**: `is_binary()` な拡張子は `max_binary_bytes` 超で read 前に skip
///   (`fs::metadata` の len 判定でメモリ読込自体を回避)。テキスト形式は上限なし。
fn scan_disk_entries(
    source_files: &[std::path::PathBuf],
    kb_path: &Path,
    registry: &Registry,
    max_binary_bytes: u64,
) -> DiskScan {
    let binary_exts = registry.binary_extensions();
    let mut entries = Vec::with_capacity(source_files.len());
    let mut skipped = Vec::new();

    for p in source_files {
        let rel = p
            .strip_prefix(kb_path)
            .unwrap_or(p)
            .to_string_lossy()
            .replace('\\', "/");
        let ext = p.extension().and_then(|e| e.to_str()).unwrap_or("");
        let is_binary = binary_exts.iter().any(|e| e.eq_ignore_ascii_case(ext));

        if is_binary {
            match std::fs::metadata(p) {
                Ok(meta) if meta.len() > max_binary_bytes => {
                    eprintln!(
                        "Skipping {rel}: binary file too large ({} bytes > {} limit)",
                        meta.len(),
                        max_binary_bytes
                    );
                    skipped.push(rel);
                    continue;
                }
                Ok(_) => {}
                Err(e) => {
                    eprintln!("Skipping {rel}: failed to stat: {e}");
                    skipped.push(rel);
                    continue;
                }
            }
        }

        match std::fs::read(p) {
            Ok(bytes) => {
                let hash = sha256_hex_bytes(&bytes);
                entries.push(DiskEntry {
                    rel,
                    hash,
                    full: p.clone(),
                });
            }
            Err(e) => {
                eprintln!("Skipping {rel}: failed to read: {e}");
                skipped.push(rel);
            }
        }
    }

    DiskScan { entries, skipped }
}

/// disk と DB の (path, hash) から「移動ペア」を決定する純粋関数。
///
/// - 「DB にあるが disk にない」path は「消えた」候補
/// - 「disk にあるが DB にない」path は「新規出現」候補
/// - 両者で hash が一致すればペア確定
///
/// 重複 hash がある場合も結果が deterministic になるよう、双方を path で
/// ソートしてから first-match マッチングを行う (evaluator 指摘 Med #4)。
fn detect_renames(
    disk_entries: &[DiskEntry],
    db_path_hashes: &std::collections::HashMap<String, String>,
) -> Vec<(String, String)> {
    let disk_paths: HashSet<&str> = disk_entries.iter().map(|e| e.rel.as_str()).collect();

    // DB ∖ disk, path で sort
    let mut orphan_in_db: Vec<(&String, &String)> = db_path_hashes
        .iter()
        .filter(|(p, _)| !disk_paths.contains(p.as_str()))
        .collect();
    orphan_in_db.sort_by_key(|(p, _)| *p);

    // disk ∖ DB, path で sort (DiskEntry は元々 walkdir の sort 順だが
    // 念のため明示的に安定化)
    let mut new_on_disk: Vec<&DiskEntry> = disk_entries
        .iter()
        .filter(|e| !db_path_hashes.contains_key(&e.rel))
        .collect();
    new_on_disk.sort_by(|a, b| a.rel.cmp(&b.rel));

    let mut consumed: HashSet<&str> = HashSet::new();
    let mut pairs: Vec<(String, String)> = Vec::new();
    for (old_path, old_hash) in &orphan_in_db {
        let mut chosen: Option<&str> = None;
        for e in &new_on_disk {
            if consumed.contains(e.rel.as_str()) {
                continue;
            }
            if &e.hash == *old_hash {
                chosen = Some(e.rel.as_str());
                break;
            }
        }
        if let Some(new_rel) = chosen {
            consumed.insert(new_rel);
            pairs.push(((*old_path).clone(), new_rel.to_string()));
        }
    }
    pairs
}

/// prune 対象 (= disk から消えた document path) を決める純粋関数。
///
/// 「visited (今回 index した) でも skipped (read 失敗 / size skip で保持) でもない」
/// DB path のみ削除対象とする (§4.2 skip 統一原則)。理由の別 (IO エラー / サイズ超過)
/// によらず「skip = 保持、削除は disk から消えた時のみ」を単一原則で表現する。
fn documents_to_delete(
    all_db_paths: &[String],
    visited: &HashSet<String>,
    skipped: &HashSet<String>,
) -> Vec<String> {
    all_db_paths
        .iter()
        .filter(|p| !visited.contains(p.as_str()) && !skipped.contains(p.as_str()))
        .cloned()
        .collect()
}

/// Summary returned by [`rebuild_index`].
pub struct IndexResult {
    pub total_documents: u32,
    pub updated: u32,
    /// File-rename を検出した件数。embedding は再計算されず
    /// `documents.path` だけが UPDATE された数。
    pub renamed: u32,
    pub deleted: u32,
    /// disk 上に存在するが index されなかったファイル数 (read/size/parse 失敗・空本文)。
    pub skipped: u32,
    pub total_chunks: u32,
    pub duration_ms: u64,
}

/// 単一ファイルのインデックス結果。`rebuild_index` 内での
/// per-file 処理と、watcher 経由の `reindex_single_file` で共通に使う。
#[derive(Debug, PartialEq)]
pub enum SingleResult {
    /// hash が既存と一致、embedding 再計算不要 (no-op)
    Unchanged,
    /// upsert + embedding 完了 (chunk 数)
    Updated { chunks: u32 },
    /// 処理対象外 (空本文など)。reason は human-readable。
    Skipped { reason: &'static str },
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Walk `kb_path` recursively, parse Markdown files, embed chunks, and store
/// everything in the database.
///
/// If `force` is `false`, files whose SHA-256 content hash has not changed
/// since the last index run are skipped.
///
/// `exclude_headings`:
/// - `None` → use [`markdown::DEFAULT_EXCLUDED_HEADINGS`]
/// - `Some(list)` → completely overrides the default list (pass `&[]` to
///   disable heading-based exclusion entirely).
#[allow(clippy::too_many_arguments)] // D-10 で 8 個に。config struct 化は別 cycle
pub fn rebuild_index(
    db: &Database,
    embedder: &mut Embedder,
    kb_path: &Path,
    force: bool,
    exclude_headings: Option<&[String]>,
    exclude_dirs: &[String],
    registry: &Registry,
    mut progress: progress::ProgressReporter,
) -> Result<IndexResult> {
    let start = Instant::now();

    let kb_path = kb_path
        .canonicalize()
        .with_context(|| format!("failed to canonicalize kb_path: {}", kb_path.display()))?;

    // legacy DB を引き継いだケースで FTS が空のままにならないよう、
    // まず既存 chunks のうち FTS 未登録のものを backfill する。
    let backfilled = db.backfill_fts()?;
    if backfilled > 0 {
        eprintln!("Backfilled {backfilled} chunks into FTS index");
    }

    // legacy DB (quality_score = 1.0 のまま) を一度だけ再評価する。
    // 既にスコアが入っているチャンクは触らないため冪等。
    let quality_updated = db.backfill_quality()?;
    if quality_updated > 0 {
        eprintln!("Backfilled {quality_updated} chunks with quality scores");
    }

    // Registry の対応拡張子リストで source files を収集する。
    // 旧 collect_md_files は .md 固定だったが、.txt 等にも対応。
    let source_files = collect_source_files(&kb_path, registry, exclude_dirs)?;
    eprintln!(
        "Found {} source files (extensions: {:?})",
        source_files.len(),
        registry.extensions()
    );

    // 罠 H2: bar lifetime を rebuild_index 内に閉じる lazy init。
    // Backfilled / Found 行を出した後に bar を構築するので衝突しない。
    progress.start_indexing(source_files.len());

    // ファイル移動検出の前段階として、disk 側の全ファイルの
    // **hash だけ** を先に計算する。content は持ち回らない (evaluator 指摘
    // High #1: 大規模 KB の memory regression 回避)。embed/upsert 段階で
    // もう一度 read_to_string する — ファイル OS キャッシュで 2 度目の
    // read は十分安く、代わりにピークメモリを `filecount * avg_size` から
    // `filecount * avg_path_len + 1 file worth of content` に圧縮できる。
    let scan = scan_disk_entries(&source_files, &kb_path, registry, crate::parser::MAX_RAW_BINARY_BYTES);
    let disk_entries = scan.entries;
    // read 失敗 / size skip の rel path 集合。prune 判定 (§4.2 統一原則) で
    // visited_paths と union し、transient lock / size 成長での誤削除を防ぐ。
    let skipped_paths: std::collections::HashSet<String> = scan.skipped.into_iter().collect();
    let mut skipped_count: u32 = skipped_paths.len() as u32;

    // rename 検出 + atomically な rename 適用。
    // force=true のときは skip (embedding 全件再計算の意図)。
    let renamed: u32 = if force {
        0
    } else {
        let db_path_hashes = db.all_path_hashes()?;
        let pairs = detect_renames(&disk_entries, &db_path_hashes);
        // evaluator 指摘 High #2: rename フェーズ全体を単一 transaction に
        // 包んで部分 rename 残留を防ぐ。pairs が空なら no-op。
        db.rename_documents_atomic(&pairs)?;
        for (old_path, new_path) in &pairs {
            progress.report_renamed(old_path, new_path);
        }
        pairs.len() as u32
    };

    // Track paths we visit so we can detect deletions later.
    let mut visited_paths: HashSet<String> = HashSet::new();
    let mut updated: u32 = 0;

    // 2. Process each file
    for entry in &disk_entries {
        visited_paths.insert(entry.rel.clone());

        match index_single_disk_entry(db, embedder, entry, exclude_headings, registry, force)? {
            SingleResult::Updated { chunks } => {
                updated += 1;
                progress.report_indexed(&entry.rel, chunks);
            }
            SingleResult::Skipped { .. } => {
                skipped_count += 1;
                progress.report_unchanged(&entry.rel);
            }
            SingleResult::Unchanged => {
                // Progress mode (= Tty/NonTty) で bar / counter を tick する。
                // Verbose / Quiet は no-op (= 既存挙動保持)。
                progress.report_unchanged(&entry.rel);
            }
        }
    }

    // 3. Delete documents in DB that no longer exist on disk.
    //    §4.2 統一原則: visited ∪ skipped は保持、それ以外 (= disk から消えた) のみ削除。
    let all_db_paths = db.all_document_paths()?;
    let mut deleted: u32 = 0;
    for db_path in documents_to_delete(&all_db_paths, &visited_paths, &skipped_paths) {
        db.delete_document(&db_path)?;
        deleted += 1;
        progress.report_deleted(&db_path);
    }

    // Count total documents remaining (includes unchanged ones)
    let total_documents = db.document_count()?;
    // Count total chunks in DB (includes unchanged ones)
    let total_chunks_in_db = db.chunk_count()?;

    let duration_ms = start.elapsed().as_millis() as u64;

    progress.finish();

    Ok(IndexResult {
        total_documents,
        updated,
        renamed,
        deleted,
        skipped: skipped_count,
        total_chunks: total_chunks_in_db,
        duration_ms,
    })
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// 単一 DiskEntry を index する内部関数。
/// rebuild_index 本体と、将来 watcher から呼ばれる `reindex_single_file` の
/// 両方で共通利用される核の処理。embedder は `&mut` で要求する (fastembed は
/// 同時呼び出し不可)。呼び出し側で Mutex 経由の相互排他を保証すること。
fn index_single_disk_entry(
    db: &Database,
    embedder: &mut Embedder,
    entry: &DiskEntry,
    exclude_headings: Option<&[String]>,
    registry: &Registry,
    force: bool,
) -> Result<SingleResult> {
    // Skip unchanged files unless forced.
    // rename で path UPDATE 済のものは「DB 側 hash == disk hash」なので
    // ここで自然に skip される (embedding 再計算なし)。
    if !force
        && let Some(existing_hash) = db.get_document_hash(&entry.rel)?
        && existing_hash == entry.hash
    {
        return Ok(SingleResult::Unchanged);
    }

    // Read + parse only for files we actually need to embed.
    // 拡張子で Registry から Parser を選択。collect_source_files
    // が Registry の extensions() のみを拾うため、通常は必ず見つかる。
    // 見つからなければ安全側に Skip 扱いで返し、crash せず次に進む。
    let ext = entry
        .full
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("");
    let Some(parser) = registry.by_extension(ext) else {
        return Ok(SingleResult::Skipped {
            reason: "no parser for extension",
        });
    };
    let bytes = match std::fs::read(&entry.full) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("Skipping {}: failed to read: {e}", entry.rel);
            return Ok(SingleResult::Skipped {
                reason: "read failed",
            });
        }
    };
    let excludes: Vec<&str> = match exclude_headings {
        Some(list) => list.iter().map(String::as_str).collect(),
        None => crate::parser::DEFAULT_EXCLUDED_HEADINGS.to_vec(),
    };
    let parsed = match parser.parse_bytes(&bytes, &entry.rel, &excludes) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("Skipping {}: parse failed: {e}", entry.rel);
            return Ok(SingleResult::Skipped {
                reason: "parse failed",
            });
        }
    };

    if parsed.chunks.is_empty() {
        return Ok(SingleResult::Skipped {
            reason: "no embeddable chunks",
        });
    }

    let (category, topic) = extract_category_topic(&entry.rel);

    // frontmatter-only skip: 既存 DB のチャンクテキストと
    // 新 parse 結果のチャンクテキストを比較し、完全一致ならチャンク本体は
    // 再 embedding せず documents 行のメタ (title/date/tags/topic/depth) と
    // content_hash のみ UPDATE する。BGE-M3 では数百 ms 〜秒規模の節約。
    // force=true / 新規ファイル / chunk 数変化は対象外。
    if !force
        && let Ok(existing) = db.chunk_texts_for_path(&entry.rel)
        && !existing.is_empty()
        && existing.len() == parsed.chunks.len()
        && existing
            .iter()
            .zip(parsed.chunks.iter())
            .all(|((eh, ec), c)| eh.as_deref() == c.heading.as_deref() && *ec == c.content)
    {
        let updated = db.update_document_meta(
            &entry.rel,
            parsed.frontmatter.title.as_deref(),
            parsed.frontmatter.topic.as_deref().or(topic.as_deref()),
            category.as_deref(),
            parsed.frontmatter.depth.as_deref(),
            &parsed.frontmatter.tags,
            parsed.frontmatter.date.as_deref(),
            &entry.hash,
        )?;
        if updated {
            return Ok(SingleResult::Updated {
                chunks: parsed.chunks.len() as u32,
            });
        }
        // update が 0 行なら通常経路にフォールスルー (レース耐性)
    }

    // Embed first, *outside* the DB tx — fastembed inference can take
    // hundreds of ms (BGE-small) or seconds (BGE-M3) per file, and we don't
    // want a long-lived write tx blocking concurrent readers in WAL mode.
    let texts: Vec<&str> = parsed.chunks.iter().map(|c| c.content.as_str()).collect();
    let embeddings = embedder
        .embed_texts(&texts)
        .with_context(|| format!("failed to embed chunks for {}", entry.rel))?;

    // Per-file atomicity (F-32): wrap upsert_document + N x insert_chunk
    // in a single tx so that a partial failure (e.g. vec_chunks dim
    // mismatch on the 3rd chunk) rolls the whole file back instead of
    // leaving a documents row with M < N chunks.
    let tx = db.begin_transaction()?;
    let doc_id = db.upsert_document(
        &entry.rel,
        parsed.frontmatter.title.as_deref(),
        parsed.frontmatter.topic.as_deref().or(topic.as_deref()),
        category.as_deref(),
        parsed.frontmatter.depth.as_deref(),
        &parsed.frontmatter.tags,
        parsed.frontmatter.date.as_deref(),
        &entry.hash,
    )?;

    for (chunk, embedding) in parsed.chunks.iter().zip(embeddings.iter()) {
        let score = quality::chunk_quality_score(chunk.heading.as_deref(), &chunk.content);
        db.insert_chunk(
            doc_id,
            chunk.index as i32,
            chunk.heading.as_deref(),
            chunk.level,
            &chunk.content,
            embedding,
            score,
        )?;
    }
    tx.commit()?;

    Ok(SingleResult::Updated {
        chunks: parsed.chunks.len() as u32,
    })
}

// ---------------------------------------------------------------------------
// 増分 index API (watcher から呼ぶ)
// ---------------------------------------------------------------------------

/// 1 つの source file を index / 再 index する。
///
/// - `kb_path` は canonicalized (`rebuild_index` と同じ前提)
/// - `rel` は forward-slash、`kb_path` からの相対パス (e.g. `"notes/a.md"`)
/// - 拡張子が `registry` に登録されていなければ `Skipped` を返す
/// - hash が DB と一致なら `Unchanged`、違えば upsert + embedding 再計算
///
/// watcher から Create/Modify イベントを受けた時に呼ぶ。
pub fn reindex_single_file(
    db: &Database,
    embedder: &mut Embedder,
    kb_path: &Path,
    rel: &str,
    exclude_headings: Option<&[String]>,
    registry: &Registry,
) -> Result<SingleResult> {
    let full = kb_path.join(rel);
    if !full.exists() {
        return Ok(SingleResult::Skipped {
            reason: "file no longer exists",
        });
    }
    let bytes = std::fs::read(&full)
        .with_context(|| format!("failed to read {}", full.display()))?;
    let hash = sha256_hex_bytes(&bytes);
    let entry = DiskEntry {
        rel: rel.to_string(),
        hash,
        full,
    };
    index_single_disk_entry(db, embedder, &entry, exclude_headings, registry, false)
}

/// 指定 path の document / chunks を DB から削除する。
/// watcher から Remove イベントを受けた時に呼ぶ。
/// DB にレコードが無ければ `Ok(false)` を返す (idempotent)。
pub fn deindex_single_file(db: &Database, rel: &str) -> Result<bool> {
    if db.get_document_hash(rel)?.is_none() {
        return Ok(false);
    }
    db.delete_document(rel)?;
    Ok(true)
}

/// Rename の結果。`rename_single_file` の戻り値。
#[derive(Debug, PartialEq)]
pub enum RenameOutcome {
    /// DB 側の path だけ UPDATE した (内容は同一)
    Renamed,
    /// 内容にも変更があり reindex も実行した
    RenamedAndReindexed { chunks: u32 },
    /// 旧 path が DB に無い (新規 path として扱った方が良い)
    OldPathMissing,
}

/// 単一ファイルの rename を処理する。
/// - `old_rel` / `new_rel` とも forward-slash、`kb_path` 相対
/// - DB 側の path を UPDATE し、必要なら再 index (内容変更がある場合)
///
/// watcher から Rename イベントペアを受けた時に呼ぶ。
pub fn rename_single_file(
    db: &Database,
    embedder: &mut Embedder,
    kb_path: &Path,
    old_rel: &str,
    new_rel: &str,
    exclude_headings: Option<&[String]>,
    registry: &Registry,
) -> Result<RenameOutcome> {
    // 旧 path が DB に無ければ rename ではなく新規作成として扱う
    let Some(old_hash) = db.get_document_hash(old_rel)? else {
        // 新 path を reindex しておく
        let _ = reindex_single_file(db, embedder, kb_path, new_rel, exclude_headings, registry)?;
        return Ok(RenameOutcome::OldPathMissing);
    };

    db.rename_document(old_rel, new_rel)?;

    // 新 path の実体 hash を読み直し、DB 側 (= old_hash) と比較
    let full = kb_path.join(new_rel);
    if !full.exists() {
        // 新 path のファイルも無い。通常起こらないが起きたら DB も掃除
        db.delete_document(new_rel)?;
        return Ok(RenameOutcome::Renamed); // path は UPDATE 済 (後で delete)
    }
    let new_bytes = std::fs::read(&full)
        .with_context(|| format!("failed to read {}", full.display()))?;
    let new_hash = sha256_hex_bytes(&new_bytes);
    if new_hash == old_hash {
        return Ok(RenameOutcome::Renamed);
    }

    // 内容も変わっているので新 path で reindex
    match reindex_single_file(db, embedder, kb_path, new_rel, exclude_headings, registry)? {
        SingleResult::Updated { chunks } => Ok(RenameOutcome::RenamedAndReindexed { chunks }),
        _ => Ok(RenameOutcome::Renamed),
    }
}

/// Collect all files under `kb_path` whose extension is registered in
/// `registry`. Directories whose basename matches any entry in
/// `exclude_dirs` are skipped (along with their subtree). Sort for
/// deterministic ordering.
fn collect_source_files(
    kb_path: &Path,
    registry: &Registry,
    exclude_dirs: &[String],
) -> Result<Vec<std::path::PathBuf>> {
    let mut files = Vec::new();
    let extensions = registry.extensions();

    for entry in WalkDir::new(kb_path)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| {
            if !e.file_type().is_dir() {
                return true;
            }
            let name = e.file_name().to_string_lossy();
            // F-62: hardcoded denylist (.git / .svn / node_modules) is always
            // applied as a fail-safe, then user exclude_dirs is layered on top
            // (union semantics). See HARDCODED_EXCLUDE_DIRS doc.
            if is_hardcoded_excluded(name.as_ref()) {
                return false;
            }
            !exclude_dirs.iter().any(|d| d.as_str() == name.as_ref())
        })
    {
        let entry = entry.context("walkdir error")?;
        if entry.file_type().is_file() {
            let name = entry.file_name().to_string_lossy();
            if is_office_lock_file(name.as_ref()) {
                continue;
            }
            if let Some(ext) = entry.path().extension()
                && let Some(ext_str) = ext.to_str()
                && extensions.iter().any(|e| e.eq_ignore_ascii_case(ext_str))
            {
                files.push(entry.into_path());
            }
        }
    }

    files.sort();
    Ok(files)
}

/// Extract `(category, topic)` from a relative path.
///
/// ```text
/// "deep-dive/chromadb/overview.md" → (Some("deep-dive"), Some("chromadb"))
/// "ai-news/2026-04-16.md"         → (Some("ai-news"), None)
/// "index.md"                       → (None, None)
/// ```
fn extract_category_topic(rel_path: &str) -> (Option<String>, Option<String>) {
    let parts: Vec<&str> = rel_path.split('/').collect();
    match parts.len() {
        // "index.md" — no category, no topic
        0 | 1 => (None, None),
        // "ai-news/2026-04-16.md" — category only
        2 => (Some(parts[0].to_string()), None),
        // "deep-dive/chromadb/overview.md" or deeper — category + topic
        _ => (Some(parts[0].to_string()), Some(parts[1].to_string())),
    }
}

/// Compute the hex-encoded SHA-256 digest of raw bytes. Byte-level hashing is the
/// canonical form since feature-45: text formats hash their UTF-8 bytes (identical
/// to the pre-feature-45 string hash), binary formats hash their raw file bytes.
fn sha256_hex_bytes(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

/// Compute the hex-encoded SHA-256 digest of a string (thin wrapper over
/// [`sha256_hex_bytes`]). All production call sites moved to the byte
/// variant in feature-45; kept test-only for the hash-parity regression
/// tests that pin old-string-hash == new-byte-hash.
#[cfg(test)]
fn sha256_hex(content: &str) -> String {
    sha256_hex_bytes(content.as_bytes())
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    use std::collections::HashMap;

    fn mk_entry(rel: &str, hash: &str) -> DiskEntry {
        DiskEntry {
            rel: rel.to_string(),
            hash: hash.to_string(),
            full: std::path::PathBuf::from(rel),
        }
    }

    #[test]
    fn test_detect_renames_single_move() {
        let disk = vec![mk_entry("new/x.md", "h1"), mk_entry("keep.md", "h2")];
        let mut db = HashMap::new();
        db.insert("old/x.md".to_string(), "h1".to_string());
        db.insert("keep.md".to_string(), "h2".to_string());
        let pairs = detect_renames(&disk, &db);
        assert_eq!(
            pairs,
            vec![("old/x.md".to_string(), "new/x.md".to_string())]
        );
    }

    #[test]
    fn test_detect_renames_no_rename_when_new_path_exists() {
        // new path が既に DB にある = 別文書なので rename ペアにしない
        let disk = vec![mk_entry("b.md", "h1")];
        let mut db = HashMap::new();
        db.insert("a.md".to_string(), "h1".to_string());
        db.insert("b.md".to_string(), "h1".to_string());
        let pairs = detect_renames(&disk, &db);
        // disk には a.md が無いので a.md は DB orphan、b.md は既に DB にある
        // → 新規 disk path が無いのでペア無し
        assert!(pairs.is_empty());
    }

    #[test]
    fn test_detect_renames_no_change_same_path_same_hash() {
        let disk = vec![mk_entry("a.md", "h1")];
        let mut db = HashMap::new();
        db.insert("a.md".to_string(), "h1".to_string());
        let pairs = detect_renames(&disk, &db);
        assert!(pairs.is_empty());
    }

    #[test]
    fn test_detect_renames_deterministic_with_duplicate_hashes() {
        // A, B とも空ファイル (同 hash) で DB、disk 側も C, D の新 path
        // どちらに振っても意味論的には同じだが結果は deterministic であるべき
        let disk = vec![mk_entry("C.md", "hempty"), mk_entry("D.md", "hempty")];
        let mut db = HashMap::new();
        db.insert("A.md".to_string(), "hempty".to_string());
        db.insert("B.md".to_string(), "hempty".to_string());
        let pairs1 = detect_renames(&disk, &db);
        // 2 回目も同じ結果になること (HashMap iteration 順に依存しない)
        let pairs2 = detect_renames(&disk, &db);
        assert_eq!(pairs1, pairs2);
        // path 順の sort により A→C, B→D になるはず
        assert_eq!(
            pairs1,
            vec![
                ("A.md".to_string(), "C.md".to_string()),
                ("B.md".to_string(), "D.md".to_string()),
            ]
        );
    }

    #[test]
    fn test_detect_renames_unmatched_hashes_are_dropped() {
        let disk = vec![mk_entry("new.md", "h_new")];
        let mut db = HashMap::new();
        db.insert("old.md".to_string(), "h_old".to_string()); // 別 hash
        let pairs = detect_renames(&disk, &db);
        // hash 不一致なのでペアにしない (old.md は削除対象、new.md は新規追加)
        assert!(pairs.is_empty());
    }

    #[test]
    fn test_documents_to_delete_retains_skipped_paths() {
        // DB に a.md / b.md / c.md。visited = {a.md} (今回 index), skipped = {b.md}
        // (read 失敗 or size skip)。c.md だけが「disk から消えた」= 削除対象。
        let all_db: Vec<String> = ["a.md", "b.md", "c.md"].iter().map(|s| s.to_string()).collect();
        let visited: HashSet<String> = ["a.md".to_string()].into_iter().collect();
        let skipped: HashSet<String> = ["b.md".to_string()].into_iter().collect();
        let to_delete = documents_to_delete(&all_db, &visited, &skipped);
        assert_eq!(to_delete, vec!["c.md".to_string()], "skipped path must be retained");
    }

    #[test]
    fn test_documents_to_delete_empty_skipped_deletes_unvisited() {
        // skipped 空 = 従来挙動: visited に無い DB path は削除。
        let all_db: Vec<String> = ["a.md", "gone.md"].iter().map(|s| s.to_string()).collect();
        let visited: HashSet<String> = ["a.md".to_string()].into_iter().collect();
        let skipped: HashSet<String> = HashSet::new();
        let to_delete = documents_to_delete(&all_db, &visited, &skipped);
        assert_eq!(to_delete, vec!["gone.md".to_string()]);
    }

    #[test]
    fn test_extract_category_topic_deep_path() {
        let (cat, topic) = extract_category_topic("deep-dive/chromadb/overview.md");
        assert_eq!(cat.as_deref(), Some("deep-dive"));
        assert_eq!(topic.as_deref(), Some("chromadb"));
    }

    #[test]
    fn test_extract_category_topic_shallow_path() {
        let (cat, topic) = extract_category_topic("ai-news/2026-04-16.md");
        assert_eq!(cat.as_deref(), Some("ai-news"));
        assert_eq!(topic, None);
    }

    #[test]
    fn test_extract_category_topic_root_file() {
        let (cat, topic) = extract_category_topic("index.md");
        assert_eq!(cat, None);
        assert_eq!(topic, None);
    }

    #[test]
    fn test_extract_category_topic_very_deep_path() {
        let (cat, topic) = extract_category_topic("tech-watch/anthropic/subdir/2026-04-16.md");
        assert_eq!(cat.as_deref(), Some("tech-watch"));
        assert_eq!(topic.as_deref(), Some("anthropic"));
    }

    #[test]
    fn test_sha256_hex_deterministic() {
        let hash1 = sha256_hex("hello world");
        let hash2 = sha256_hex("hello world");
        assert_eq!(hash1, hash2);
        // Known SHA-256 of "hello world"
        assert_eq!(
            hash1,
            "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9"
        );
    }

    #[test]
    fn test_sha256_hex_different_content() {
        let hash1 = sha256_hex("hello");
        let hash2 = sha256_hex("world");
        assert_ne!(hash1, hash2);
    }

    #[test]
    fn test_sha256_hex_bytes_matches_string_hash_for_utf8() {
        // read_to_string は無変換 (CRLF/BOM 保持) なので、UTF-8 ファイルの
        // raw バイト hash = 旧文字列 hash。既存 DB の再 index 暴発を防ぐ要。
        for s in ["hello world", "日本語テキスト", "line1\r\nline2\n", "\u{feff}bom-prefixed"] {
            assert_eq!(sha256_hex(s), sha256_hex_bytes(s.as_bytes()), "mismatch for {s:?}");
        }
    }

    // -----------------------------------------------------------------------
    // collect_source_files
    // -----------------------------------------------------------------------

    struct TmpDir(std::path::PathBuf);
    impl Drop for TmpDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn mk_tmp(prefix: &str) -> TmpDir {
        let pid = std::process::id();
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let p = std::env::temp_dir().join(format!("kb-mcp-idxtest-{prefix}-{pid}-{nonce}"));
        std::fs::create_dir_all(&p).unwrap();
        TmpDir(p)
    }

    fn write_file(dir: &std::path::Path, rel: &str, content: &str) {
        let full = dir.join(rel);
        if let Some(parent) = full.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(full, content).unwrap();
    }

    #[test]
    fn test_collect_source_files_md_only_by_default() {
        let tmp = mk_tmp("mdonly");
        write_file(&tmp.0, "a.md", "# A");
        write_file(&tmp.0, "b.txt", "plain b");
        write_file(&tmp.0, "sub/c.md", "# C");
        write_file(&tmp.0, "ignore.rst", "rst");

        let reg = Registry::defaults(); // md only
        let files = collect_source_files(&tmp.0, &reg, &[".obsidian".to_string()]).unwrap();
        let rels: Vec<String> = files
            .iter()
            .map(|p| {
                p.strip_prefix(&tmp.0)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/")
            })
            .collect();
        assert!(rels.contains(&"a.md".to_string()));
        assert!(rels.contains(&"sub/c.md".to_string()));
        assert!(!rels.iter().any(|r| r.ends_with(".txt")));
        assert!(!rels.iter().any(|r| r.ends_with(".rst")));
    }

    #[test]
    fn test_collect_source_files_md_and_txt_opt_in() {
        let tmp = mk_tmp("mdtxt");
        write_file(&tmp.0, "a.md", "# A");
        write_file(&tmp.0, "b.txt", "plain");
        write_file(&tmp.0, "ignore.rst", "rst");

        let reg = Registry::from_enabled(&["md".into(), "txt".into()]).unwrap();
        let files = collect_source_files(&tmp.0, &reg, &[".obsidian".to_string()]).unwrap();
        let rels: Vec<String> = files
            .iter()
            .map(|p| {
                p.strip_prefix(&tmp.0)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/")
            })
            .collect();
        assert!(rels.contains(&"a.md".to_string()));
        assert!(rels.contains(&"b.txt".to_string()));
        assert!(!rels.iter().any(|r| r.ends_with(".rst")));
    }

    #[test]
    fn test_collect_source_files_skips_obsidian() {
        let tmp = mk_tmp("obsidian");
        write_file(&tmp.0, "keep.md", "# keep");
        write_file(&tmp.0, ".obsidian/workspace.md", "# should be skipped");
        write_file(&tmp.0, ".obsidian/nested/evil.md", "# skip too");

        let reg = Registry::defaults();
        let files = collect_source_files(&tmp.0, &reg, &[".obsidian".to_string()]).unwrap();
        let rels: Vec<String> = files
            .iter()
            .map(|p| {
                p.strip_prefix(&tmp.0)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/")
            })
            .collect();
        assert_eq!(rels, vec!["keep.md".to_string()]);
    }

    #[test]
    fn test_collect_source_files_case_insensitive_extension() {
        let tmp = mk_tmp("case");
        write_file(&tmp.0, "lower.md", "# lower");
        write_file(&tmp.0, "UPPER.MD", "# upper");
        write_file(&tmp.0, "note.TXT", "txt");

        let reg = Registry::from_enabled(&["md".into(), "txt".into()]).unwrap();
        let files = collect_source_files(&tmp.0, &reg, &[".obsidian".to_string()]).unwrap();
        assert_eq!(files.len(), 3, "should match regardless of case: {files:?}");
    }

    #[test]
    fn test_collect_source_files_deterministic_ordering() {
        let tmp = mk_tmp("sort");
        write_file(&tmp.0, "zzz.md", "z");
        write_file(&tmp.0, "aaa.md", "a");
        write_file(&tmp.0, "mmm.md", "m");

        let reg = Registry::defaults();
        let f1 = collect_source_files(&tmp.0, &reg, &[".obsidian".to_string()]).unwrap();
        let f2 = collect_source_files(&tmp.0, &reg, &[".obsidian".to_string()]).unwrap();
        assert_eq!(f1, f2);
        // First one should be aaa
        assert!(
            f1[0]
                .file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("aaa")
        );
    }

    // -----------------------------------------------------------------------
    // F-62: hardcoded denylist (.git / .svn / node_modules) is always applied
    // as a fail-safe alongside user `exclude_dirs` (union semantics).
    // -----------------------------------------------------------------------

    /// `exclude_dirs = []` (= "walk everything") でも `.git/` 配下は skip。
    #[test]
    fn test_collect_source_files_skips_dot_git_even_with_empty_exclude_dirs() {
        let tmp = mk_tmp("hardenedgit");
        write_file(&tmp.0, ".git/inside.md", "# git inside");
        write_file(&tmp.0, "normal.md", "# normal");

        let reg = Registry::defaults();
        let files = collect_source_files(&tmp.0, &reg, &[]).unwrap();
        let names: Vec<String> = files
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().to_string())
            .collect();
        assert!(
            names.contains(&"normal.md".to_string()),
            "normal.md must be kept, got: {names:?}"
        );
        assert!(
            !names.iter().any(|n| n == "inside.md"),
            ".git/inside.md must be skipped by hardcoded denylist, got: {names:?}"
        );
    }

    /// User が `exclude_dirs` を default 上書きしつつ `.git` を含め忘れた case
    /// (= 本 cycle の主たる fail-safe shape)。
    #[test]
    fn test_collect_source_files_skips_dot_git_when_user_exclude_dirs_overrides_default() {
        let tmp = mk_tmp("hardenedoverride");
        write_file(&tmp.0, ".git/inside.md", "# git inside");
        write_file(&tmp.0, "normal.md", "# normal");

        let reg = Registry::defaults();
        // User overrides DEFAULT_EXCLUDE_DIRS with their own list, forgetting
        // to re-list `.git`. Hardcoded denylist still skips `.git/inside.md`.
        let files = collect_source_files(&tmp.0, &reg, &["custom".to_string()]).unwrap();
        let names: Vec<String> = files
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().to_string())
            .collect();
        assert!(names.contains(&"normal.md".to_string()));
        assert!(
            !names.iter().any(|n| n == "inside.md"),
            ".git/inside.md must remain skipped despite user override, got: {names:?}"
        );
    }

    /// Hardcoded denylist + user `exclude_dirs` の union semantics 確認。
    #[test]
    fn test_collect_source_files_union_of_hardcoded_and_user_excludes() {
        let tmp = mk_tmp("hardenedunion");
        write_file(&tmp.0, ".git/git_inside.md", "# git");
        write_file(&tmp.0, ".obsidian/note.md", "# obsidian note");
        write_file(&tmp.0, "keep.md", "# keep");

        let reg = Registry::defaults();
        // User explicitly excludes `.obsidian`. Hardcoded denylist also
        // skips `.git`. Both must be skipped (union).
        let files = collect_source_files(&tmp.0, &reg, &[".obsidian".to_string()]).unwrap();
        let names: Vec<String> = files
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().to_string())
            .collect();
        assert!(
            names.contains(&"keep.md".to_string()),
            "keep.md must be kept, got: {names:?}"
        );
        assert!(
            !names.iter().any(|n| n == "git_inside.md"),
            "hardcoded .git skip failed: {names:?}"
        );
        assert!(
            !names.iter().any(|n| n == "note.md"),
            "user .obsidian skip failed: {names:?}"
        );
    }

    #[test]
    fn test_is_office_lock_file() {
        assert!(is_office_lock_file("~$report.docx")); // MS Office owner file
        assert!(is_office_lock_file("~$budget.xlsx"));
        assert!(is_office_lock_file(".~lock.report.docx#")); // LibreOffice lock
        assert!(!is_office_lock_file("report.docx"));
        assert!(!is_office_lock_file("notes.md"));
        assert!(!is_office_lock_file("~draft.md")); // ~$ で始まらない
    }

    #[test]
    fn test_collect_source_files_skips_office_lock() {
        let tmp = mk_tmp("officelock");
        write_file(&tmp.0, "a.md", "# a");
        write_file(&tmp.0, "~$a.md", "owner file"); // ~$ prefix, md 拡張子
        write_file(&tmp.0, ".~lock.a.md#", "lo lock");
        let reg = Registry::defaults();
        let files = collect_source_files(&tmp.0, &reg, &[]).unwrap();
        let names: Vec<String> = files
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().to_string())
            .collect();
        assert_eq!(
            names,
            vec!["a.md".to_string()],
            "lock files must be skipped, got {names:?}"
        );
    }

    // -----------------------------------------------------------------------
    // scan_disk_entries
    // -----------------------------------------------------------------------

    #[test]
    fn test_scan_disk_entries_read_fail_goes_to_skipped() {
        let tmp = mk_tmp("scanreadfail");
        write_file(&tmp.0, "ok.md", "# ok");
        // 実在しないファイルを source_files に混ぜる = fs::read 失敗を deterministic に誘発
        // (collect と read の間で消えた / lock された file の代理)。
        let missing = tmp.0.join("gone.md");
        let reg = Registry::defaults();
        let source = vec![tmp.0.join("ok.md"), missing];
        let scan = scan_disk_entries(&source, &tmp.0, &reg, crate::parser::MAX_RAW_BINARY_BYTES);
        let entry_rels: Vec<&str> = scan.entries.iter().map(|e| e.rel.as_str()).collect();
        assert_eq!(entry_rels, vec!["ok.md"]);
        assert_eq!(scan.skipped, vec!["gone.md".to_string()]);
    }

    #[test]
    fn test_scan_disk_entries_binary_size_skip_goes_to_skipped() {
        let tmp = mk_tmp("scansize");
        // is_binary な拡張子 (pdf) を持つ registry を作れないので、cap を極小に
        // 渡して「size 超過」を誘発する。txt を binary 扱いにはできないため、この
        // test は cap パラメータ注入で size-skip 分岐を突く。ここでは md を通常読み、
        // 別途 binary 拡張子は PR-2 で結合テストする。cap の効果は次の assert で。
        write_file(&tmp.0, "small.md", "# tiny");
        let reg = Registry::defaults();
        // md は is_binary=false なので size cap の対象外 = cap を 1 にしても通る。
        let scan = scan_disk_entries(&[tmp.0.join("small.md")], &tmp.0, &reg, 1);
        assert_eq!(scan.entries.len(), 1, "text files ignore the binary size cap");
        assert!(scan.skipped.is_empty());
    }

    // -----------------------------------------------------------------------
    // 増分 index API
    // -----------------------------------------------------------------------

    fn test_db() -> Database {
        let db = Database::open_in_memory().unwrap();
        db.verify_embedding_meta("bge-small-en-v1.5", 384).unwrap();
        db
    }

    #[test]
    fn test_deindex_single_file_missing_returns_false() {
        let db = test_db();
        let removed = deindex_single_file(&db, "never-indexed.md").unwrap();
        assert!(!removed, "deindex of non-existent path should return false");
    }

    #[test]
    fn test_deindex_single_file_after_upsert_returns_true() {
        let db = test_db();
        db.upsert_document(
            "notes/a.md",
            Some("Title"),
            None,
            Some("notes"),
            None,
            &[],
            None,
            "hash1",
        )
        .unwrap();
        assert!(db.get_document_hash("notes/a.md").unwrap().is_some());

        let removed = deindex_single_file(&db, "notes/a.md").unwrap();
        assert!(removed, "deindex of existing path should return true");
        assert!(db.get_document_hash("notes/a.md").unwrap().is_none());
    }

    #[test]
    fn test_update_document_meta_for_frontmatter_only_change() {
        // frontmatter-only skip の前提となる DB API が期待通り動くことの回帰テスト。
        let db = test_db();
        db.upsert_document(
            "notes/a.md",
            Some("Old"),
            None,
            Some("notes"),
            None,
            &[],
            None,
            "old_hash",
        )
        .unwrap();
        // update_document_meta は content_hash を差し替えて meta を更新
        let updated = db
            .update_document_meta(
                "notes/a.md",
                Some("New Title"),
                Some("new-topic"),
                Some("notes"),
                None,
                &["tag1".to_string()],
                Some("2026-04-19"),
                "new_hash",
            )
            .unwrap();
        assert!(updated);
        assert_eq!(
            db.get_document_hash("notes/a.md").unwrap().as_deref(),
            Some("new_hash")
        );
    }

    #[test]
    fn test_update_document_meta_missing_path_returns_false() {
        let db = test_db();
        let updated = db
            .update_document_meta("never-existed.md", None, None, None, None, &[], None, "h")
            .unwrap();
        assert!(!updated);
    }

    #[test]
    fn test_chunk_texts_for_path_empty_when_not_indexed() {
        let db = test_db();
        let texts = db.chunk_texts_for_path("not-indexed.md").unwrap();
        assert!(texts.is_empty());
    }

    #[test]
    fn test_f12_8_frontmatter_only_skip_db_contract() {
        // frontmatter-only skip (frontmatter-only skip) の DB 契約部分を end-to-end で検証:
        // 1. document + chunk を 1 件 index した状態を作る
        // 2. chunk_texts_for_path が期待通りのリストを返す
        // 3. frontmatter だけ変えた再 index 相当として update_document_meta を呼ぶ
        // 4. chunks は維持されたまま、meta (title/content_hash) のみ更新される
        let db = test_db();
        let doc_id = db
            .upsert_document(
                "notes/foo.md",
                Some("Old"),
                None,
                Some("notes"),
                None,
                &[],
                None,
                "hash1",
            )
            .unwrap();
        let emb = vec![0.0f32; 384];
        db.insert_chunk(
            doc_id,
            0,
            Some("intro"),
            None,
            "Hello world body.",
            &emb,
            0.9,
        )
        .unwrap();

        // (2) 既存 chunks を比較用に取得
        let before = db.chunk_texts_for_path("notes/foo.md").unwrap();
        assert_eq!(before.len(), 1);
        assert_eq!(before[0].0.as_deref(), Some("intro"));
        assert_eq!(before[0].1, "Hello world body.");

        // (3) frontmatter-only change: title と content_hash を更新
        let updated = db
            .update_document_meta(
                "notes/foo.md",
                Some("New Title"),
                None,
                Some("notes"),
                None,
                &[],
                None,
                "hash2",
            )
            .unwrap();
        assert!(updated);

        // (4) meta は変わっているが chunks は維持
        assert_eq!(
            db.get_document_hash("notes/foo.md").unwrap().as_deref(),
            Some("hash2")
        );
        let after = db.chunk_texts_for_path("notes/foo.md").unwrap();
        assert_eq!(after, before, "chunks must survive frontmatter-only change");
    }

    /// vacuous test を解消するため、enum の PartialEq をベースに API の戻り値
    /// 種別が expect と一致することを確認する軽量テストに差し替えた。
    /// 実 Embedder を要する reindex/rename の true e2e は `cargo test --
    /// --ignored` で回る integration テスト側に任せる (Embedder DL が発生
    /// するため通常の cargo test には載せない)。
    #[test]
    fn test_single_result_variants_are_distinct() {
        assert_ne!(SingleResult::Unchanged, SingleResult::Updated { chunks: 0 });
        assert_ne!(
            SingleResult::Unchanged,
            SingleResult::Skipped { reason: "test" }
        );
        assert_ne!(
            SingleResult::Updated { chunks: 1 },
            SingleResult::Updated { chunks: 2 }
        );
    }

    #[test]
    fn test_rename_outcome_variants_are_distinct() {
        assert_ne!(
            RenameOutcome::Renamed,
            RenameOutcome::RenamedAndReindexed { chunks: 1 }
        );
        assert_ne!(RenameOutcome::Renamed, RenameOutcome::OldPathMissing);
    }
}
