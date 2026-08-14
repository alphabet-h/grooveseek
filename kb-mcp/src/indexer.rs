use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::path::Path;
use std::time::Instant;
use walkdir::WalkDir;

use crate::db::Database;
use crate::embedder::Embedder;
use crate::parser::{ParserExt, Registry};
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
/// (BU-19) Matched case-insensitively, like the extension checks a few lines
/// down. Windows and macOS resolve `.GIT` and `.git` to the same directory, so
/// an exact-match denylist there is a denylist with a trivial bypass — and the
/// fail-safe it is meant to be would silently index a repository's metadata.
/// The cost on Linux, where the two really are different directories, is that
/// a directory literally named `.GIT` is skipped as well; that is the safer
/// side to err on for a hardcoded VCS denylist.
///
/// ASCII folding is complete here because [`HARDCODED_EXCLUDE_DIRS`] is ASCII
/// by construction. [`is_user_excluded_dir`], which compares arbitrary
/// configured names, needs the Unicode-aware form instead.
pub fn is_hardcoded_excluded(basename: &str) -> bool {
    HARDCODED_EXCLUDE_DIRS
        .iter()
        .any(|d| d.eq_ignore_ascii_case(basename))
}

/// (BU-19) Does a single path component match a user `exclude_dirs` entry?
///
/// The one place that decides it, because there are three callers — the index
/// walk, the `validate` walk, and the live watcher — and they have drifted
/// apart before: AU-03 found the watcher missing the hardcoded denylist that
/// the other two applied, and BU-19 landed with two of the three switched to
/// case-insensitive matching, so the watcher would have incrementally indexed
/// a `Build/` that the index walk skipped. Route every such decision through
/// here.
///
/// Case-insensitive for the same reason as [`is_hardcoded_excluded`], but
/// unlike that function — whose entries are ASCII by construction — this one
/// compares arbitrary user input, so ASCII folding is not enough:
/// `exclude_dirs = ["résumé"]` has to match a directory stored as `RÉSUMÉ`
/// (codex P2 on PR #141).
///
/// **Exactly what this does**, because the obvious phrasing overstates it:
/// lowercase mapping via `str::to_lowercase`, then Greek final sigma folded to
/// medial sigma. That second step is needed because `to_lowercase` is
/// context-dependent there — measured: `"ΟΣ"` lowercases to `"ος"` while
/// `"οσ"` stays `"οσ"`, so without it a configured `οσ` misses a directory
/// named `ΟΣ` (codex P2, round 3).
///
/// **What it is not**: full Unicode case folding. Also measured, `"straße"`
/// and `"STRASSE"` stay distinct — `ß` lowercases to itself, and only case
/// *folding* maps it to `ss`. Getting that would mean taking on a
/// case-folding dependency for a case a knowledge-base directory name is not
/// going to hit; the limit is written down here instead of papered over.
///
/// It does **not** normalize either. A name written with combining marks
/// (`e` + U+0301) still differs from the precomposed `é`. Filesystems
/// generally hand back one consistent form, so that matters only if the
/// configured string and the directory on disk were typed on different
/// systems.
pub fn is_user_excluded_dir(name: &str, exclude_dirs: &[String]) -> bool {
    /// U+03C2 GREEK SMALL LETTER FINAL SIGMA → U+03C3 GREEK SMALL LETTER SIGMA.
    fn fold(s: &str) -> String {
        s.to_lowercase().replace('\u{03c2}', "\u{03c3}")
    }
    exclude_dirs.iter().any(|d| {
        // Fast path: identical bytes, which is what almost every call is.
        d.as_str() == name || fold(d) == fold(name)
    })
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
    /// 実ファイルの絶対パス。embed/upsert 段階で再 `fs::read` する。
    full: std::path::PathBuf,
}

/// [`scan_disk_entries`] の結果。`entries` = hash 計算済みの index 候補、
/// `skipped` = disk に存在するが read 失敗 / size 超過で載らなかった rel path。
/// `skipped` は prune 統一原則 (§4.2) で visited_paths へ union し、既存 entry を保護する。
struct DiskScan {
    entries: Vec<DiskEntry>,
    skipped: Vec<String>,
}

/// バイナリ拡張子ファイルが `max` bytes を超えているかを、内容を読まずに
/// `fs::metadata` だけで判定する共有ヘルパー。フル re-index の
/// `scan_disk_entries` と watcher 増分 index の `reindex_single_file` の
/// 両方が、実際に `fs::read` する前の size-cap ガードとして呼ぶ (codex P2
/// round 2: watcher 経路が `scan_disk_entries` の size-cap 保護をバイパス
/// して 50 MiB 超ファイルを全量 read/hash してしまう問題の修正)。
///
/// - 適用する cap は拡張子で決まる (`is_binary_ext` なら `max_binary`、
///   そうでなければ `max_text`)。**(BU-02) テキストにも cap がある** —
///   以前はテキストを無条件に `Ok(None)` で通しており、巨大な `.md` 1 本で
///   `fs::read` が OOM を起こせた (`rebuild_index` は MCP から叩ける)
/// - 超過していれば `Ok(Some((actual_len, applied_cap)))`、cap 内なら `Ok(None)`。
///   cap を返すのは、呼び出し側の警告文が「何 byte 制限を超えたか」を
///   自前で再計算しなくて済むようにするため
/// - `fs::metadata` 自体の失敗は `Err` としてそのまま伝播する。size cap の
///   判定とは別関心事なので、呼び出し側が既存の stat/read エラー処理に委ねる
fn size_cap_exceeded(
    path: &Path,
    is_binary_ext: bool,
    max_binary: u64,
    max_text: u64,
) -> std::io::Result<Option<(u64, u64)>> {
    let cap = applicable_cap(is_binary_ext, max_binary, max_text);
    let meta = std::fs::metadata(path)?;
    Ok(if meta.len() > cap {
        Some((meta.len(), cap))
    } else {
        None
    })
}

/// Which of the two caps applies to an extension.
///
/// (BU-20) Split out because the same number is now needed twice: once by
/// [`size_cap_exceeded`], which stats the **path** before deciding whether to
/// open it at all, and once by `links::read_checked`, which enforces it on the
/// **handle** the bytes come from. The path form is the cheap pre-check; the
/// handle form is the one that cannot be swapped past. They must agree on the
/// limit, so neither computes it itself.
fn applicable_cap(is_binary_ext: bool, max_binary: u64, max_text: u64) -> u64 {
    if is_binary_ext { max_binary } else { max_text }
}

/// Read a file for indexing through a handle whose identity has been checked
/// (BU-20), turning a refusal into the per-file skip every caller already does.
///
/// `Ok(None)` means "skip this file, and say why on stderr"; `Err` keeps
/// `std::fs::read`'s meaning so existing read-error handling is unchanged.
fn read_for_index(full: &Path, rel: &str, cap: u64) -> std::io::Result<Option<Vec<u8>>> {
    match crate::links::read_checked(full, cap)? {
        crate::links::Content::Bytes(bytes) => Ok(Some(bytes)),
        crate::links::Content::Refused(refused) => {
            eprintln!("Skipping {rel}: {}", refused.log_line(full));
            Ok(None)
        }
    }
}

/// 超過警告に使う「binary」/「text」の語。cap が拡張子で決まるので、
/// 警告文も同じ分岐で選ぶ (どちらの上限に当たったか読み手に分かるように)。
fn size_cap_kind(is_binary_ext: bool) -> &'static str {
    if is_binary_ext { "binary" } else { "text" }
}

/// disk 側の全 source file を走査し、raw バイト読み + バイト hash を計算する。
///
/// - **エラー隔離**: `read` 失敗や size 超過は per-file skip し、rel path を `skipped`
///   に積んで走査を続行する (旧 `.collect::<Result<Vec>>()?` の全体 abort を修正)。
/// - **size skip**: 拡張子ごとの cap (`max_binary_bytes` / `max_text_bytes`) を
///   超えるファイルは read 前に skip する ([`size_cap_exceeded`] の
///   `fs::metadata` 判定でメモリ読込自体を回避)。
fn scan_disk_entries(
    source_files: &[std::path::PathBuf],
    kb_path: &Path,
    registry: &Registry,
    max_binary_bytes: u64,
    max_text_bytes: u64,
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

        match size_cap_exceeded(p, is_binary, max_binary_bytes, max_text_bytes) {
            Ok(Some((len, cap))) => {
                let kind = size_cap_kind(is_binary);
                eprintln!("Skipping {rel}: {kind} file too large ({len} bytes > {cap} limit)");
                skipped.push(rel);
                continue;
            }
            Ok(None) => {}
            Err(e) => {
                eprintln!("Skipping {rel}: failed to stat: {e}");
                skipped.push(rel);
                continue;
            }
        }

        // (BU-20) The walk already refused hard links; this catches one that
        // arrived between the walk and now. A refusal joins `skipped`, like
        // every other reason this loop declines a file: the *new* bytes are
        // what is being refused, the row already in the database came from
        // bytes that were legitimate when they were read, and the next full
        // run's walk-time check is what evicts it — which is where that
        // decision belongs (§4.2, skip preserves).
        let cap = applicable_cap(is_binary, max_binary_bytes, max_text_bytes);
        match read_for_index(p, &rel, cap) {
            Ok(Some(bytes)) => {
                let hash = sha256_hex_bytes(&bytes);
                entries.push(DiskEntry {
                    rel,
                    hash,
                    full: p.clone(),
                });
            }
            Ok(None) => {
                skipped.push(rel);
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
///
/// `skipped` = 今回の scan で read 失敗 / size 超過により `disk_entries` に
/// 載らなかった rel path 集合。これらは disk 上にまだ存在する可能性が高く
/// 「消えた」わけではないため、orphan 候補から除外する。除外しないと、
/// skip 中ファイルの DB 行が別の新規ファイルと同一 hash になった際に誤って
/// rename ペアとして扱われ、prune 保護 (§4.2 skip 統一原則) が効く前に
/// skip 中ファイルの DB 行が新 path へ書き換わってしまう (codex P2)。
fn detect_renames(
    disk_entries: &[DiskEntry],
    db_path_hashes: &std::collections::HashMap<String, String>,
    skipped: &HashSet<String>,
) -> Vec<(String, String)> {
    let disk_paths: HashSet<&str> = disk_entries.iter().map(|e| e.rel.as_str()).collect();

    // DB ∖ disk ∖ skipped, path で sort
    let mut orphan_in_db: Vec<(&String, &String)> = db_path_hashes
        .iter()
        .filter(|(p, _)| !disk_paths.contains(p.as_str()) && !skipped.contains(p.as_str()))
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

/// 現在の registry に載っていない拡張子を持つ document path を返す (AU-06)。
///
/// `[parsers].enabled` を狭めた後 (`.xls` の取り下げのように、狭めざるを得なく
/// なった場合を含む)、その拡張子の行は DB に残る。`kb-mcp index` を 1 度回せば
/// `documents_to_delete` が prune するが、`serve` は起動時に index しないので、
/// `serve` しか使わない運用では残り続ける。
///
/// 残ると **search には出るのに `get_document` が拒否する** hit になる
/// (`validate_get_document_path` が `registry.has_extension` で門番をするため)。
/// AU-02 で直したのと同じ「見つかるのに開けない」症状。
///
/// **削除はしない**。狭めた設定は一時的なこともあり、起動のたびに黙って行を
/// 消すのは破壊的すぎる。呼び出し側は件数を warn して `kb-mcp index` を促す。
pub fn paths_with_unregistered_extension(
    all_db_paths: &[String],
    registry: &crate::parser::Registry,
) -> Vec<String> {
    all_db_paths
        .iter()
        .filter(|p| {
            std::path::Path::new(p.as_str())
                .extension()
                .and_then(|e| e.to_str())
                .is_none_or(|ext| !registry.has_extension(ext))
        })
        .cloned()
        .collect()
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
    /// (BU-20) 開いた handle が「集めた時のファイルではない」と答えた
    /// (hardlink / symlink / 非通常ファイル / handle 側 size 超過)。
    ///
    /// **`Skipped` と別 variant にする理由** (codex P2 round 1 on PR #157):
    /// `rename_single_file` は hash 用に 1 回読んだ後 `index_single_disk_entry`
    /// が**もう一度**読む。2 回目で refusal が起きた時に `Skipped` を返すと、
    /// 呼び出し側の catch-all が `RenameOutcome::Renamed` に潰し、**DB には旧
    /// content が新 path のまま残るのに watcher は「rename 成功」と報告する**。
    /// 型で分けておけば、その分岐を書き忘れることができない。
    Refused,
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
    context_mode_desired: ContextMode,
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
    let quality_updated = db.backfill_quality(&registry.binary_extensions())?;
    if quality_updated > 0 {
        eprintln!("Backfilled {quality_updated} chunks with quality scores");
    }

    // feature-46: effective context mode を解決 (force / grandfather / warn を含む)。
    // codex P2 (PR #73 F1): force 時は resolve より前に必ず DB を reset する
    // (詳細は reset_and_resolve_context_mode のコメント参照)。
    let context_mode = reset_and_resolve_context_mode(
        db,
        embedder.model_id(),
        embedder.dimension() as u32,
        context_mode_desired,
        force,
    )?;

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
    // もう一度 `fs::read` する — ファイル OS キャッシュで 2 度目の
    // read は十分安く、代わりにピークメモリを `filecount * avg_size` から
    // `filecount * avg_path_len + 1 file worth of content` に圧縮できる。
    let scan = scan_disk_entries(
        &source_files,
        &kb_path,
        registry,
        crate::parser::MAX_RAW_BINARY_BYTES,
        crate::parser::MAX_RAW_TEXT_BYTES,
    );
    let disk_entries = scan.entries;
    // read 失敗 / size skip の rel path 集合。prune 判定 (§4.2 統一原則) で
    // visited_paths と union し、transient lock / size 成長での誤削除を防ぐ。
    let skipped_paths: std::collections::HashSet<String> = scan.skipped.into_iter().collect();
    let mut skipped_count: u32 = skipped_paths.len() as u32;

    // rename 検出 + atomically な rename 適用。
    // force=true のときは skip (embedding 全件再計算の意図)。
    // codex P2 (PR #73 F3): rename 先の new_path を集めておく。Static モードでは
    // rename された entry だけ下の loop で force 扱いにし、same-hash fast path
    // (index_single_disk_entry 冒頭の hash 一致 skip) を意図的に無効化する。
    // rename は path UPDATE のみで内容 (hash) は変わらないため、そのまま fast
    // path に乗ると、frontmatter title 無し文書で context 用 title が filename
    // stem 由来 (E-1) にもかかわらず再 parse されず、breadcrumb (chunk.context)
    // が旧 filename のまま stale 化する。Off モードは context を embed に使わない
    // ため無害 = 従来通り fast path を維持する。
    let mut renamed_new_paths: HashSet<String> = HashSet::new();
    let renamed: u32 = if force {
        0
    } else {
        let db_path_hashes = db.all_path_hashes()?;
        let pairs = detect_renames(&disk_entries, &db_path_hashes, &skipped_paths);
        // evaluator 指摘 High #2: rename フェーズ全体を単一 transaction に
        // 包んで部分 rename 残留を防ぐ。pairs が空なら no-op。
        db.rename_documents_atomic(&pairs)?;
        for (old_path, new_path) in &pairs {
            progress.report_renamed(old_path, new_path);
            if context_mode == ContextMode::Static {
                renamed_new_paths.insert(new_path.clone());
            }
        }
        pairs.len() as u32
    };

    // Track paths we visit so we can detect deletions later.
    let mut visited_paths: HashSet<String> = HashSet::new();
    let mut updated: u32 = 0;

    // 2. Process each file
    for entry in &disk_entries {
        visited_paths.insert(entry.rel.clone());

        // rename された entry (Static モードのみ) は force=true で再 parse/embed
        // させ、他の unchanged file の hash fast path はそのまま活かす。
        let entry_force = force || renamed_new_paths.contains(&entry.rel);

        match index_single_disk_entry(
            db,
            embedder,
            entry,
            exclude_headings,
            registry,
            entry_force,
            context_mode,
        )? {
            SingleResult::Updated { chunks } => {
                updated += 1;
                progress.report_indexed(&entry.rel, chunks);
            }
            // A refusal counts as a skip here for the same reason a size cap
            // does: the file is not indexed and the reason is already on stderr.
            SingleResult::Skipped { .. } | SingleResult::Refused => {
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

/// embedding 入力を組む (feature-46 §4.5)。Static かつ context ありなら
/// `context\n\ncontent`、それ以外 (Off / context なし) は content のみ。
/// Anthropic 原典 cookbook の結合形 `f"{context}\n\n{chunk}"` に忠実。
fn embed_input_for(chunk: &crate::parser::Chunk, mode: ContextMode) -> String {
    match (mode, chunk.context.as_deref()) {
        (ContextMode::Static, Some(ctx)) if !ctx.trim().is_empty() => {
            format!("{ctx}\n\n{}", chunk.content)
        }
        _ => chunk.content.clone(),
    }
}

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
    context_mode: ContextMode,
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
    // (BU-20) The bytes that become chunks come from a handle whose link count,
    // file type and size were all read off that same handle — this is the read
    // a swapped-in hard link has to get past, and cannot.
    let cap = applicable_cap(
        parser.is_binary(),
        crate::parser::MAX_RAW_BINARY_BYTES,
        crate::parser::MAX_RAW_TEXT_BYTES,
    );
    let bytes = match read_for_index(&entry.full, &entry.rel, cap) {
        Ok(Some(b)) => b,
        Ok(None) => {
            return Ok(SingleResult::Refused);
        }
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
    //
    // codex P2 round 2 (finding B, 根治): 旧実装は「frontmatter.title の変化」
    // だけを個別に検知する専用 gate (title_unchanged、`get_document_title` の
    // 追加 SELECT) を持っていたが、title 以外の要因で context breadcrumb が
    // 変わるケース (例: exclude_headings 変更で見出し構造が変わる) を検知
    // できなかった (E-8 の不完全な包摂)。Static モードでは (heading, content)
    // に加えて context_text も比較対象へ含めることで、context に影響し得る
    // あらゆる変化を一括して検知する (title 変化もこれに包摂される)。これに
    // より専用 title_unchanged gate と冗長な `get_document_title` SELECT は
    // 不要になったため撤去した。Off モードは context を embed/保存しないため、
    // 従来通り (heading, content) のみで比較する (挙動不変)。
    let chunks_unchanged = if context_mode == ContextMode::Static {
        db.chunk_texts_with_context_for_path(&entry.rel)
            .map(|existing| {
                !existing.is_empty()
                    && existing.len() == parsed.chunks.len()
                    && existing
                        .iter()
                        .zip(parsed.chunks.iter())
                        .all(|((eh, ec, ectx), c)| {
                            eh.as_deref() == c.heading.as_deref()
                                && *ec == c.content
                                && ectx.as_deref() == c.context.as_deref()
                        })
            })
            .unwrap_or(false)
    } else {
        db.chunk_texts_for_path(&entry.rel)
            .map(|existing| {
                !existing.is_empty()
                    && existing.len() == parsed.chunks.len()
                    && existing
                        .iter()
                        .zip(parsed.chunks.iter())
                        .all(|((eh, ec), c)| {
                            eh.as_deref() == c.heading.as_deref() && *ec == c.content
                        })
            })
            .unwrap_or(false)
    };

    if !force && chunks_unchanged {
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
    // feature-46: Static モードでは context を前置して embed する (§4.5)。
    let embed_inputs: Vec<String> = parsed
        .chunks
        .iter()
        .map(|c| embed_input_for(c, context_mode))
        .collect();
    let texts: Vec<&str> = embed_inputs.iter().map(String::as_str).collect();
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
        let score = quality::chunk_quality_score(
            chunk.heading.as_deref(),
            &chunk.content,
            parser.is_binary(),
        );
        let context = match context_mode {
            ContextMode::Static => chunk.context.as_deref(),
            ContextMode::Off => None,
        };
        db.insert_chunk(
            doc_id,
            chunk.index as i32,
            chunk.heading.as_deref(),
            chunk.level,
            &chunk.content,
            context,
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
/// - **size cap**: `is_binary()` な拡張子が `MAX_RAW_BINARY_BYTES` を超えていれば
///   `fs::read` する前に skip する ([`binary_size_exceeded`]、codex P2 round 2:
///   watcher の create/modify 経路は元々これをバイパスして全量 read/hash していた)
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

    // scan_disk_entries (フル rebuild) と同じ size-cap ガードを read 前に適用する。
    // metadata 自体の失敗はここでは無視し、直後の fs::read の既存エラー処理
    // (`?` 伝播) にフォールスルーさせる (size cap とは別関心事)。
    let ext = full.extension().and_then(|e| e.to_str()).unwrap_or("");
    let is_binary_ext = registry
        .binary_extensions()
        .iter()
        .any(|e| e.eq_ignore_ascii_case(ext));
    if let Ok(Some((len, cap))) = size_cap_exceeded(
        &full,
        is_binary_ext,
        crate::parser::MAX_RAW_BINARY_BYTES,
        crate::parser::MAX_RAW_TEXT_BYTES,
    ) {
        let kind = size_cap_kind(is_binary_ext);
        eprintln!("Skipping {rel}: {kind} file too large ({len} bytes > {cap} limit)");
        return Ok(SingleResult::Skipped {
            reason: "file too large",
        });
    }

    let cap = applicable_cap(
        is_binary_ext,
        crate::parser::MAX_RAW_BINARY_BYTES,
        crate::parser::MAX_RAW_TEXT_BYTES,
    );
    let Some(bytes) = read_for_index(&full, rel, cap)
        .with_context(|| format!("failed to read {}", full.display()))?
    else {
        return Ok(SingleResult::Refused);
    };
    let hash = sha256_hex_bytes(&bytes);
    let entry = DiskEntry {
        rel: rel.to_string(),
        hash,
        full,
    };
    // watcher は config-desired を持たないので DB 側モードに従う (E-11)。
    let context_mode = db.read_context_mode()?.unwrap_or(ContextMode::Off);
    index_single_disk_entry(
        db,
        embedder,
        &entry,
        exclude_headings,
        registry,
        false,
        context_mode,
    )
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
    /// path は UPDATE 済だが、新 path の binary size が cap 超過のため
    /// hash 再計算 / reindex はスキップした (codex P2 round 3)。DB の
    /// content_hash は旧内容のまま据え置き、次回 full rebuild の
    /// `scan_disk_entries` の size-cap 判定に委ねる (§4.2 skip 統一原則)。
    RenamedSizeCapped,
    /// (BU-20) path は UPDATE 済だが、新 path を開いた handle が
    /// 「集めた時のファイルではない」と答えた (hardlink / symlink / 非通常
    /// ファイル / handle 側の size 超過) ため hash 再計算 / reindex はスキップした。
    ///
    /// **`RenamedSizeCapped` と同じ形が要る理由**: `rename_document` は既に
    /// commit 済みなので、ここで `Err` を返すと「旧 content が新 path に付いた
    /// まま、ログ上は I/O 失敗と区別できない」状態になる。専用の variant に
    /// することで、呼び出し側と読み手の両方に「rename は成立、内容は据え置き、
    /// 理由は refusal」と伝わる。DB の content_hash は旧内容のままで、
    /// 次回 full rebuild の walk-time check が row ごと取り除く。
    RenamedButRefused,
}

/// 単一ファイルの rename を処理する。
/// - `old_rel` / `new_rel` とも forward-slash、`kb_path` 相対
/// - DB 側の path を UPDATE し、必要なら再 index (内容変更がある場合)
/// - **size cap**: `is_binary()` な拡張子の新 path が `MAX_RAW_BINARY_BYTES`
///   を超えていれば hash 再計算のための `fs::read` をスキップする
///   ([`binary_size_exceeded`]、codex P2 round 3。これで scan / reindex /
///   rename の 3 read 経路すべてが同じ size-cap guard を通るようになった)
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

    // size-cap ガード: fs::read する前に判定する。DB の path 自体は既に
    // rename_document で UPDATE 済なのでここでは触らない (= 旧 hash のまま
    // 新 path に残る)。hash 再計算 / reindex は次回 full rebuild に委ねる。
    let ext = full.extension().and_then(|e| e.to_str()).unwrap_or("");
    let is_binary_ext = registry
        .binary_extensions()
        .iter()
        .any(|e| e.eq_ignore_ascii_case(ext));
    if let Ok(Some((len, cap))) = size_cap_exceeded(
        &full,
        is_binary_ext,
        crate::parser::MAX_RAW_BINARY_BYTES,
        crate::parser::MAX_RAW_TEXT_BYTES,
    ) {
        let kind = size_cap_kind(is_binary_ext);
        eprintln!("Skipping {new_rel}: {kind} file too large ({len} bytes > {cap} limit)");
        return Ok(RenameOutcome::RenamedSizeCapped);
    }

    let cap = applicable_cap(
        is_binary_ext,
        crate::parser::MAX_RAW_BINARY_BYTES,
        crate::parser::MAX_RAW_TEXT_BYTES,
    );
    let Some(new_bytes) = read_for_index(&full, new_rel, cap)
        .with_context(|| format!("failed to read {}", full.display()))?
    else {
        return Ok(RenameOutcome::RenamedButRefused);
    };
    let new_hash = sha256_hex_bytes(&new_bytes);

    // codex P2 round 2 (finding A): watcher は config-desired を持たないので
    // DB 側モードに従う (`reindex_single_file` と同じ E-11 の規則)。
    // rebuild_index の一括 rename (F3, PR #73) と同じ理由で、Static モードでは
    // same-hash fast path を無効化する: rename は内容 (hash) を変えないが、
    // Static モードでは frontmatter title が無い文書の context breadcrumb が
    // filename stem 由来 (E-1) のため、再 parse しない限り旧 filename のまま
    // stale 化してしまう。Off モードは context を embed に使わないため無害 =
    // 従来通り fast path を維持する。
    let context_mode = db.read_context_mode()?.unwrap_or(ContextMode::Off);
    let same_hash = new_hash == old_hash;
    if same_hash && context_mode != ContextMode::Static {
        return Ok(RenameOutcome::Renamed);
    }

    // 内容が変わっている、または (Static モードの same-hash rename として)
    // breadcrumb 更新のため強制的に再 embed する。size-cap 判定は既に上で
    // 済んでいるので、読み込み済みの bytes/hash をそのまま再利用して
    // 二重 read を避ける (`reindex_single_file` を経由すると全 read をやり直す)。
    let entry = DiskEntry {
        rel: new_rel.to_string(),
        hash: new_hash,
        full,
    };
    // same_hash (= Static-mode-forced) の場合のみ force=true で
    // hash 一致 fast path をバイパスする。内容が変わっている場合は
    // 通常の force=false 経路 (frontmatter-only skip 判定含む) に任せる。
    match index_single_disk_entry(
        db,
        embedder,
        &entry,
        exclude_headings,
        registry,
        same_hash,
        context_mode,
    )? {
        SingleResult::Updated { chunks } => Ok(RenameOutcome::RenamedAndReindexed { chunks }),
        // (codex P2 round 1 on PR #157) `index_single_disk_entry` reads the file
        // a **second** time, and the whole premise of this guard is that a path
        // can change between two reads. Letting that refusal fall into the
        // catch-all below would report a successful rename while the database
        // kept the old content under the new path.
        SingleResult::Refused => Ok(RenameOutcome::RenamedButRefused),
        // Spelled out rather than `_`: a catch-all is what swallowed the
        // refusal in the first place, and it would swallow the next variant too.
        SingleResult::Unchanged | SingleResult::Skipped { .. } => Ok(RenameOutcome::Renamed),
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
            !is_user_excluded_dir(name.as_ref(), exclude_dirs)
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
                // (BU-20) A hard link is a second name for a file that may live
                // outside the KB, and unlike a symlink nothing in the walk
                // shows it: `follow_links(false)` has nothing to follow.
                // Skipping here also removes an already-indexed document that
                // gains a second name, since the deletion pass treats "not
                // collected" as gone.
                //
                // Checked last, after the extension filter: on Windows the link
                // count needs the file opened, and there is no reason to open
                // what we were never going to index.
                if crate::links::is_multiply_linked(entry.path()) {
                    tracing::warn!("{}", crate::links::refusal_reason(entry.path()));
                    continue;
                }
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

use crate::db::ContextMode;

/// force / config-desired / DB-stored から effective context mode を決める (feature-46 §4.8)。
/// 副作用: fresh/legacy/force のケースで `index_meta.context_mode` を記録する。
/// mismatch (config は static 期待だが DB は off 等) は、index が空でなければ
/// stderr へ warn し DB 側モードで継続する (embedding 空間の一貫性維持、混在
/// index を作らない)。index が空 (chunk 0 件) なら守るべき embedding 空間が
/// 存在しないため、代わりに desired を採用して記録を上書きする (codex P2
/// round 3)。
pub(crate) fn resolve_context_mode(
    db: &Database,
    desired: ContextMode,
    force: bool,
) -> Result<ContextMode> {
    if force {
        // reset_for_model 後 = DB は空。desired を採用して記録する。
        db.write_context_mode(desired)?;
        return Ok(desired);
    }
    match db.read_context_mode()? {
        Some(stored) => {
            if stored != desired {
                // codex P2 round 3: mode が記録済みでも index が空 (chunk 0 件)
                // なら、守るべき embedding 空間がそもそも存在しない。典型例は
                // F2 fix (round 1) の副作用: `serve` 起動時に resolve_context_mode
                // が fresh DB へ desired を書いた直後、まだ 1 件も index せずに
                // user が `[contextual].enabled` を反転して再起動したケース。
                // このとき stale な記録値を優先して `--force` を要求するのは
                // 不合理なので、desired をそのまま採用して上書きする。
                if db.chunk_count()? == 0 {
                    db.write_context_mode(desired)?;
                    eprintln!(
                        "info: index is empty; adopting '{}' for empty index (was '{}').",
                        desired.as_str(),
                        stored.as_str()
                    );
                    return Ok(desired);
                }
                eprintln!(
                    "warning: [contextual] config expects '{}' mode but this index was built \
                     with '{}'. Run `kb-mcp index --force` to migrate; continuing in '{}' mode.",
                    desired.as_str(),
                    stored.as_str(),
                    stored.as_str()
                );
            }
            Ok(stored)
        }
        None => {
            // key 不在: fresh DB (chunk 0) は desired、legacy DB (chunk > 0) は off に grandfather
            let mode = if db.chunk_count()? > 0 {
                ContextMode::Off
            } else {
                desired
            };
            db.write_context_mode(mode)?;
            if mode != desired {
                eprintln!(
                    "warning: existing index has no context data; grandfathering to '{}'. \
                     Run `kb-mcp index --force` to build a contextual index.",
                    mode.as_str()
                );
            }
            Ok(mode)
        }
    }
}

/// `force` 時に `resolve_context_mode` より前に必ず DB を reset してから解決する
/// ラッパー (codex P2 on PR #73, finding F1)。
///
/// `resolve_context_mode(force=true)` は「呼ばれる時点で DB は既に空 (=
/// `reset_for_model` 済み)」を前提に `desired` を即座に `index_meta` へ書く。
/// CLI 経路 (`main.rs` の `Commands::Index`) はその前提を呼び出し側で満たして
/// いたが、MCP `rebuild_index` tool (`server.rs`) はここに来るまで reset を
/// 挟まないまま `rebuild_index(force=true)` を呼んでいた。そのため「新 mode を
/// 記録した直後、まだ upsert していない旧 mode の chunk が残っている」状態で
/// rebuild が abort すると、mixed index (meta は新 mode、chunk は旧 mode) に
/// なり得た。呼び出し元 (`rebuild_index`) の先頭でこの関数を通すことで、
/// force 時は必ず reset → resolve の順序を DB 層で強制する。
///
/// `reset_for_model` の DELETE は冪等なので、CLI 経路のように呼び出し側で
/// 既に reset 済みの場合にここでもう一度呼んでも無害。
pub(crate) fn reset_and_resolve_context_mode(
    db: &Database,
    model_id: &str,
    dim: u32,
    desired: ContextMode,
    force: bool,
) -> Result<ContextMode> {
    if force {
        db.reset_for_model(model_id, dim)?;
    }
    resolve_context_mode(db, desired, force)
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
        let pairs = detect_renames(&disk, &db, &HashSet::new());
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
        let pairs = detect_renames(&disk, &db, &HashSet::new());
        // disk には a.md が無いので a.md は DB orphan、b.md は既に DB にある
        // → 新規 disk path が無いのでペア無し
        assert!(pairs.is_empty());
    }

    #[test]
    fn test_detect_renames_no_change_same_path_same_hash() {
        let disk = vec![mk_entry("a.md", "h1")];
        let mut db = HashMap::new();
        db.insert("a.md".to_string(), "h1".to_string());
        let pairs = detect_renames(&disk, &db, &HashSet::new());
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
        let pairs1 = detect_renames(&disk, &db, &HashSet::new());
        // 2 回目も同じ結果になること (HashMap iteration 順に依存しない)
        let pairs2 = detect_renames(&disk, &db, &HashSet::new());
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
        let pairs = detect_renames(&disk, &db, &HashSet::new());
        // hash 不一致なのでペアにしない (old.md は削除対象、new.md は新規追加)
        assert!(pairs.is_empty());
    }

    #[test]
    fn test_detect_renames_excludes_skipped_paths() {
        // DB に a.md (hash H)。disk 側は a.md が read 失敗 / size 超過で
        // skip され disk_entries に載らず、別の新規 b.md が同じ hash H を持つ。
        // a.md は disk から「消えた」のではなく単に今回未計上なだけなので、
        // rename ペア (a.md → b.md) にすり替わってはならない (codex P2)。
        let disk = vec![mk_entry("b.md", "H")];
        let mut db = HashMap::new();
        db.insert("a.md".to_string(), "H".to_string());
        let skipped: HashSet<String> = ["a.md".to_string()].into_iter().collect();
        let pairs = detect_renames(&disk, &db, &skipped);
        assert!(pairs.is_empty());
    }

    #[test]
    fn test_documents_to_delete_retains_skipped_paths() {
        // DB に a.md / b.md / c.md。visited = {a.md} (今回 index), skipped = {b.md}
        // (read 失敗 or size skip)。c.md だけが「disk から消えた」= 削除対象。
        let all_db: Vec<String> = ["a.md", "b.md", "c.md"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let visited: HashSet<String> = ["a.md".to_string()].into_iter().collect();
        let skipped: HashSet<String> = ["b.md".to_string()].into_iter().collect();
        let to_delete = documents_to_delete(&all_db, &visited, &skipped);
        assert_eq!(
            to_delete,
            vec!["c.md".to_string()],
            "skipped path must be retained"
        );
    }

    /// AU-06 (codex P2): `[parsers].enabled` を狭めた後に残る行を数えられること。
    /// `.xls` を取り下げたので、旧 index を持ったまま upgrade した人がこの状態に入る。
    #[test]
    fn unregistered_extensions_are_reported_but_not_deleted() {
        let registry =
            crate::parser::Registry::from_enabled(&["md".to_string(), "xlsx".to_string()]).unwrap();
        let all_db: Vec<String> = [
            "notes/a.md",
            "book.xlsx",
            "legacy/old.xls", // registry から外れた拡張子
            "REPORT.XLSX",    // 大文字でも registered (AU-02: 照合は case-insensitive)
            "no_extension",   // 拡張子なしも「載っていない」扱い
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();

        let stale = paths_with_unregistered_extension(&all_db, &registry);
        assert_eq!(
            stale,
            vec!["legacy/old.xls".to_string(), "no_extension".to_string()],
            "only paths the registry cannot serve should be reported"
        );

        // 「報告するだけ」であること: 入力は変えない (呼び出し側が warn するのみ)。
        assert_eq!(all_db.len(), 5, "the caller's list must not be mutated");
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
        for s in [
            "hello world",
            "日本語テキスト",
            "line1\r\nline2\n",
            "\u{feff}bom-prefixed",
        ] {
            assert_eq!(
                sha256_hex(s),
                sha256_hex_bytes(s.as_bytes()),
                "mismatch for {s:?}"
            );
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
        let p = crate::test_support::unique_temp_path(&format!("kb-mcp-idxtest-{prefix}"));
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

    /// (BU-19) Directory exclusion matches case-insensitively.
    ///
    /// On Windows and macOS `.GIT` and `.git` name the same directory, so an
    /// exact-match denylist is one a repository can walk straight past — and
    /// the hardcoded list exists precisely as a fail-safe. The user list has
    /// the same problem in the other direction: `exclude_dirs = ["build"]`
    /// missed a directory that happened to be created as `Build`.
    ///
    /// The case is checked here rather than via the filesystem so the test
    /// means the same thing on Linux, where the two directories really are
    /// distinct and the exclusion is a deliberate widening.
    #[test]
    fn dir_exclusion_ignores_case() {
        for name in [".GIT", ".Git", "NODE_MODULES", "Node_Modules", ".SVN"] {
            assert!(
                is_hardcoded_excluded(name),
                "{name} must hit the hardcoded denylist — on a case-insensitive \
                 filesystem it is the very directory the list names"
            );
        }
        for name in [".gitignore", "git", "node_modules_old", "svn"] {
            assert!(
                !is_hardcoded_excluded(name),
                "{name} is a different directory and must not be excluded"
            );
        }

        // Configured names are arbitrary user input, so folding has to cover
        // more than ASCII (codex P2 on PR #141). Normalization is explicitly
        // out of scope: the precomposed and decomposed spellings of `é` are
        // different strings and stay that way.
        let unicode = vec!["résumé".to_string(), "Ünterlagen".to_string()];
        for name in ["RÉSUMÉ", "Résumé", "résumé", "ÜNTERLAGEN", "ünterlagen"] {
            assert!(
                is_user_excluded_dir(name, &unicode),
                "{name} must match a configured non-ASCII exclusion"
            );
        }
        assert!(
            !is_user_excluded_dir("resume", &unicode),
            "an unaccented name is a different directory, not a case variant"
        );

        // Greek final sigma: `to_lowercase` is context-dependent, so `ΟΣ`
        // lowercases to `ος` while a configured `οσ` stays `οσ`. Folding the
        // final form closes that (codex P2, round 3).
        let greek = vec!["οσ".to_string()];
        for name in ["ΟΣ", "Οσ", "οΣ", "ος"] {
            assert!(
                is_user_excluded_dir(name, &greek),
                "{name} is a case variant of the configured οσ and must match"
            );
        }

        // And the limit this stops at, asserted so it is a decision rather
        // than a surprise: lowercase mapping is not full case folding, so `ß`
        // and `SS` remain different directories.
        let sharp_s = vec!["straße".to_string()];
        assert!(
            !is_user_excluded_dir("STRASSE", &sharp_s),
            "documented limit: `ß` lowercases to itself, so only full Unicode \
             case folding would match STRASSE — if this ever starts passing, \
             the doc comment on is_user_excluded_dir needs updating too"
        );
        assert!(
            is_user_excluded_dir("STRASSE", &["strasse".to_string()]),
            "the plain-ASCII spelling still folds normally"
        );

        let tmp = mk_tmp("excludecase");
        write_file(&tmp.0, "Build/inside.md", "# inside");
        write_file(&tmp.0, "normal.md", "# normal");
        let reg = Registry::defaults();
        let files = collect_source_files(&tmp.0, &reg, &["build".to_string()]).unwrap();
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
            "`exclude_dirs = [\"build\"]` must also skip a directory spelled \
             `Build`, got: {names:?}"
        );
    }

    /// (BU-20) hardlink は symlink と同じ脅威なのに、walk からは見えない
    /// (`follow_links(false)` に辿るべき link が無い)。full index からも
    /// 落ちることを pin する。**既に index 済みの文書が 2 つ目の名前を得た
    /// 場合も落ちる** = 削除 pass が「集められなかった = 消えた」と扱うので、
    /// index から取り除かれる。これは承認済みの代償 (log で説明される)。
    #[test]
    fn test_collect_source_files_skips_hard_links() {
        let tmp = mk_tmp("hardlink");
        write_file(&tmp.0, "normal.md", "# normal");
        write_file(&tmp.0, "secret.md", "# secret\nssh-rsa AAAA...");
        std::fs::hard_link(tmp.0.join("secret.md"), tmp.0.join("notes.md"))
            .expect("hard links need no privilege");

        let reg = Registry::defaults();
        let files = collect_source_files(&tmp.0, &reg, &[]).unwrap();
        let names: Vec<String> = files
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().to_string())
            .collect();
        assert!(
            names.contains(&"normal.md".to_string()),
            "a file with one name must still be indexed, got: {names:?}"
        );
        assert!(
            !names.iter().any(|n| n == "notes.md" || n == "secret.md"),
            "both names of a hard-linked file must be skipped, got: {names:?}"
        );
    }

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
        let scan = scan_disk_entries(
            &source,
            &tmp.0,
            &reg,
            crate::parser::MAX_RAW_BINARY_BYTES,
            crate::parser::MAX_RAW_TEXT_BYTES,
        );
        let entry_rels: Vec<&str> = scan.entries.iter().map(|e| e.rel.as_str()).collect();
        assert_eq!(entry_rels, vec!["ok.md"]);
        assert_eq!(scan.skipped, vec!["gone.md".to_string()]);
    }

    /// (BU-20) The walk refuses hard links, but it hands `scan_disk_entries` a
    /// list of *paths* and the bytes are read here — so a file that became a
    /// hard link in between has to be caught at the read. Passing the link in
    /// `source_files` is exactly that situation: the collect step accepted it.
    ///
    /// It joins `skipped`, not the silently-dropped set: the row already in the
    /// database was indexed from bytes that were legitimate when they were
    /// read, and evicting it is the walk's job on the next full run.
    #[test]
    fn test_scan_disk_entries_refuses_a_hard_link_that_appeared_after_the_walk() {
        let tmp = mk_tmp("scanhardlink");
        write_file(&tmp.0, "ok.md", "# ok");
        write_file(&tmp.0, "secret.txt", "ssh-rsa AAAA...");
        let linked = tmp.0.join("notes.md");
        std::fs::hard_link(tmp.0.join("secret.txt"), &linked)
            .expect("hard links need no privilege");

        let reg = Registry::defaults();
        let source = vec![tmp.0.join("ok.md"), linked];
        let scan = scan_disk_entries(
            &source,
            &tmp.0,
            &reg,
            crate::parser::MAX_RAW_BINARY_BYTES,
            crate::parser::MAX_RAW_TEXT_BYTES,
        );

        let entry_rels: Vec<&str> = scan.entries.iter().map(|e| e.rel.as_str()).collect();
        assert_eq!(
            entry_rels,
            vec!["ok.md"],
            "the hard link's bytes must never reach an entry"
        );
        assert_eq!(scan.skipped, vec!["notes.md".to_string()]);
    }

    /// The other half of the same wiring: a plain file still reads, so the test
    /// above is not passing because the read is broken for everything.
    #[test]
    fn test_read_for_index_returns_bytes_for_an_ordinary_file() {
        let tmp = mk_tmp("readforindex");
        write_file(&tmp.0, "a.md", "# A");
        let bytes = read_for_index(
            &tmp.0.join("a.md"),
            "a.md",
            crate::parser::MAX_RAW_TEXT_BYTES,
        )
        .expect("an ordinary file must not error")
        .expect("an ordinary file must not be refused");
        assert_eq!(bytes, b"# A");
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
        // md は is_binary=false なので **binary** cap の対象外 = 1 にしても通る。
        let scan = scan_disk_entries(
            &[tmp.0.join("small.md")],
            &tmp.0,
            &reg,
            1,
            crate::parser::MAX_RAW_TEXT_BYTES,
        );
        assert_eq!(
            scan.entries.len(),
            1,
            "text files ignore the binary size cap"
        );
        assert!(scan.skipped.is_empty());
    }

    /// (BU-02) テキストにも cap がある。上の test と対になっていて、
    /// 「binary cap は効かないが text cap は効く」を両側から挟む。
    #[test]
    fn test_scan_disk_entries_text_over_its_own_cap_goes_to_skipped() {
        let tmp = mk_tmp("scansizetext");
        write_file(&tmp.0, "big.md", "0123456789"); // 10 bytes
        let reg = Registry::defaults();
        let scan = scan_disk_entries(
            &[tmp.0.join("big.md")],
            &tmp.0,
            &reg,
            crate::parser::MAX_RAW_BINARY_BYTES,
            5,
        );
        assert!(
            scan.entries.is_empty(),
            "a text file over the text cap must not be read"
        );
        assert_eq!(scan.skipped, vec!["big.md".to_string()]);
    }

    // -----------------------------------------------------------------------
    // size_cap_exceeded
    // -----------------------------------------------------------------------

    /// (BU-02、**旧 `test_binary_size_exceeded_none_for_text_ext` を差し替え**)
    /// テキストは binary cap を無視するが、自分の cap は無視しない。
    ///
    /// 旧テストは `is_binary_ext=false` なら実サイズに関わらず `None` を返す
    /// ことを固定していた = 「テキストに上限なし」を契約として守っていた。
    /// これが BU-02 の欠陥そのもの (巨大な `.md` 1 本で `fs::read` が OOM。
    /// `rebuild_index` は MCP 経由でクライアントから叩ける) なので反転する。
    #[test]
    fn test_text_ext_uses_the_text_cap_not_the_binary_one() {
        let tmp = mk_tmp("sizecap-text");
        write_file(&tmp.0, "big.md", "0123456789"); // 10 bytes
        let path = tmp.0.join("big.md");
        // binary cap = 1 でも text なので当たらない。
        assert_eq!(size_cap_exceeded(&path, false, 1, 1024).unwrap(), None);
        // text cap = 5 なら当たる。返る cap は「適用された方」。
        assert_eq!(
            size_cap_exceeded(&path, false, 1024, 5).unwrap(),
            Some((10, 5))
        );
    }

    #[test]
    fn test_binary_size_exceeded_none_when_within_cap() {
        let tmp = mk_tmp("sizecap-ok");
        write_file(&tmp.0, "small.pdf", "tiny");
        let result = size_cap_exceeded(&tmp.0.join("small.pdf"), true, 1024, 1024).unwrap();
        assert_eq!(result, None);
    }

    #[test]
    fn test_binary_size_exceeded_some_when_over_cap() {
        let tmp = mk_tmp("sizecap-over");
        write_file(&tmp.0, "big.pdf", "0123456789"); // 10 bytes
        let result = size_cap_exceeded(&tmp.0.join("big.pdf"), true, 5, 1024).unwrap();
        assert_eq!(result, Some((10, 5)));
    }

    #[test]
    fn test_binary_size_exceeded_err_when_metadata_fails() {
        let tmp = mk_tmp("sizecap-missing");
        let missing = tmp.0.join("gone.pdf");
        assert!(size_cap_exceeded(&missing, true, 1024, 1024).is_err());
    }

    /// 警告文が「どちらの上限に当たったか」を言い分ける。
    #[test]
    fn test_size_cap_kind_names_the_applied_limit() {
        assert_eq!(size_cap_kind(true), "binary");
        assert_eq!(size_cap_kind(false), "text");
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
            None,
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

    #[test]
    fn test_rename_outcome_size_capped_variant_is_distinct() {
        // codex P2 round 3: 新 variant が既存 3 種と区別できることの回帰確認
        // (rename_single_file 自体は Embedder 必須で単体テスト不可のため、
        // ここでは enum の distinctness のみ確認する)。
        assert_ne!(RenameOutcome::RenamedSizeCapped, RenameOutcome::Renamed);
        assert_ne!(
            RenameOutcome::RenamedSizeCapped,
            RenameOutcome::RenamedAndReindexed { chunks: 1 }
        );
        assert_ne!(
            RenameOutcome::RenamedSizeCapped,
            RenameOutcome::OldPathMissing
        );
    }

    // -----------------------------------------------------------------------
    // resolve_context_mode (feature-46 Task 2.5)
    // -----------------------------------------------------------------------

    use crate::db::ContextMode;

    #[test]
    fn test_resolve_context_mode_fresh_db_adopts_desired() {
        let db = test_db(); // 空 (chunk 0)
        let m = resolve_context_mode(&db, ContextMode::Static, false).unwrap();
        assert_eq!(m, ContextMode::Static);
        assert_eq!(db.read_context_mode().unwrap(), Some(ContextMode::Static));
    }

    #[test]
    fn test_resolve_context_mode_legacy_db_grandfathers_to_off() {
        let db = test_db();
        // chunk を 1 件入れて legacy (key 不在 + chunk > 0) を作る
        let doc_id = db
            .upsert_document("a.md", Some("T"), None, None, None, &[], None, "h")
            .unwrap();
        db.insert_chunk(
            doc_id,
            0,
            Some("H"),
            Some(2),
            "body",
            None,
            &vec![0.0f32; 384],
            1.0,
        )
        .unwrap();
        let m = resolve_context_mode(&db, ContextMode::Static, false).unwrap();
        assert_eq!(m, ContextMode::Off, "legacy DB grandfathers to off");
        assert_eq!(db.read_context_mode().unwrap(), Some(ContextMode::Off));
    }

    #[test]
    fn test_resolve_context_mode_stored_wins_over_desired() {
        let db = test_db();
        // codex P2 round 3: 「stored が desired に勝つ」のは index が空でない
        // ときのみの挙動になったため (空 index は adopt する、次の test 参照)、
        // この test の前提を「chunk が実在する non-empty index」に機械的に
        // 更新する (assert! 文言 / 期待値は不変、fixture のみ追加)。
        let doc_id = db
            .upsert_document("a.md", Some("T"), None, None, None, &[], None, "h")
            .unwrap();
        db.insert_chunk(
            doc_id,
            0,
            Some("H"),
            Some(2),
            "body",
            None,
            &vec![0.0f32; 384],
            1.0,
        )
        .unwrap();
        db.write_context_mode(ContextMode::Off).unwrap();
        let m = resolve_context_mode(&db, ContextMode::Static, false).unwrap();
        assert_eq!(m, ContextMode::Off, "DB-stored mode wins on mismatch");
    }

    #[test]
    fn test_resolve_context_mode_empty_index_adopts_desired_on_mismatch() {
        // codex P2 round 3: mode が記録済みでも index が空 (chunk 0 件) なら、
        // 守るべき embedding 空間が存在しないので `--force` を要求せず desired
        // を採用して記録を上書きする。round 1 の F2 fix (server 起動時に
        // resolve_context_mode を 1 回呼ぶ) の副作用として、1 件も index せず
        // config を反転して再起動するケースを想定。
        let db = test_db(); // 空 (chunk 0)
        db.write_context_mode(ContextMode::Static).unwrap();
        let m = resolve_context_mode(&db, ContextMode::Off, false).unwrap();
        assert_eq!(
            m,
            ContextMode::Off,
            "empty index adopts desired, not stored"
        );
        assert_eq!(
            db.read_context_mode().unwrap(),
            Some(ContextMode::Off),
            "adopted desired must be persisted, overwriting the stale stored value"
        );
    }

    #[test]
    fn test_resolve_context_mode_empty_index_adopts_desired_on_mismatch_reverse() {
        // 逆方向 (stored=Off, desired=Static) でも同じ規則が適用されることの確認。
        let db = test_db();
        db.write_context_mode(ContextMode::Off).unwrap();
        let m = resolve_context_mode(&db, ContextMode::Static, false).unwrap();
        assert_eq!(
            m,
            ContextMode::Static,
            "empty index adopts desired, not stored"
        );
        assert_eq!(
            db.read_context_mode().unwrap(),
            Some(ContextMode::Static),
            "adopted desired must be persisted, overwriting the stale stored value"
        );
    }

    #[test]
    fn test_resolve_context_mode_force_adopts_desired() {
        let db = test_db();
        db.write_context_mode(ContextMode::Off).unwrap();
        // force: reset 済み前提で desired を採用 + 記録
        let m = resolve_context_mode(&db, ContextMode::Static, true).unwrap();
        assert_eq!(m, ContextMode::Static);
        assert_eq!(db.read_context_mode().unwrap(), Some(ContextMode::Static));
    }

    #[test]
    fn test_reset_and_resolve_context_mode_force_wipes_existing_chunks() {
        // codex P2 on PR #73 (F1) regression: MCP `rebuild_index` tool の force
        // 経路は (fix 前は) reset_for_model を呼ばずに resolve_context_mode だけ
        // 呼んでいたため、「新 mode を記録した直後もまだ旧 mode の chunk が
        // DB に残っている」瞬間が生じ得た。reset_and_resolve_context_mode は
        // force 時に resolve より前で必ず reset_for_model を呼ぶことでこれを防ぐ。
        // ここでは MCP 経路を model (Embedder 不要): 呼び出し側で reset を挟まず
        // 「旧 mode で index 済みの DB」に直接 force call するシナリオを再現する。
        let db = test_db();
        db.write_context_mode(ContextMode::Off).unwrap();
        let doc_id = db
            .upsert_document("stale.md", Some("Stale"), None, None, None, &[], None, "h")
            .unwrap();
        db.insert_chunk(
            doc_id,
            0,
            Some("H"),
            Some(2),
            "stale body",
            None,
            &vec![0.0f32; 384],
            1.0,
        )
        .unwrap();
        assert_eq!(db.chunk_count().unwrap(), 1, "precondition: DB has 1 chunk");

        let mode = reset_and_resolve_context_mode(
            &db,
            "bge-small-en-v1.5",
            384,
            ContextMode::Static,
            true,
        )
        .unwrap();

        assert_eq!(mode, ContextMode::Static);
        assert_eq!(db.read_context_mode().unwrap(), Some(ContextMode::Static));
        assert_eq!(
            db.chunk_count().unwrap(),
            0,
            "force must wipe pre-existing chunks before the new mode is recorded, \
             even when the caller (MCP rebuild_index tool) never called reset_for_model itself"
        );
        assert_eq!(
            db.document_count().unwrap(),
            0,
            "force must wipe pre-existing documents alongside chunks"
        );
    }

    #[test]
    fn test_reset_and_resolve_context_mode_non_force_does_not_wipe() {
        // 対照テスト: force=false では reset を経由せず resolve_context_mode の
        // 通常挙動 (DB-stored mode 優先、chunk 保持) のまま。
        let db = test_db();
        db.write_context_mode(ContextMode::Off).unwrap();
        let doc_id = db
            .upsert_document("kept.md", Some("Kept"), None, None, None, &[], None, "h")
            .unwrap();
        db.insert_chunk(
            doc_id,
            0,
            Some("H"),
            Some(2),
            "kept body",
            None,
            &vec![0.0f32; 384],
            1.0,
        )
        .unwrap();

        let mode = reset_and_resolve_context_mode(
            &db,
            "bge-small-en-v1.5",
            384,
            ContextMode::Static,
            false,
        )
        .unwrap();

        assert_eq!(mode, ContextMode::Off, "non-force keeps DB-stored mode");
        assert_eq!(
            db.chunk_count().unwrap(),
            1,
            "non-force must not wipe existing chunks"
        );
    }

    // -----------------------------------------------------------------------
    // embed_input_for (feature-46 Task 2.7)
    // -----------------------------------------------------------------------

    #[test]
    fn test_embed_input_static_prepends_context() {
        // clippy::field_reassign_with_default を避けるため struct literal で構築
        // (ロジック / assert 文言は brief のテストと不変)。
        let ch = crate::parser::Chunk {
            content: "body".to_string(),
            context: Some("T > H".to_string()),
            ..Default::default()
        };
        assert_eq!(embed_input_for(&ch, ContextMode::Static), "T > H\n\nbody");
    }

    #[test]
    fn test_embed_input_off_is_content_only() {
        let ch = crate::parser::Chunk {
            content: "body".to_string(),
            context: Some("T > H".to_string()),
            ..Default::default()
        };
        // Off モードは parser が context を生成していても content のみ
        assert_eq!(embed_input_for(&ch, ContextMode::Off), "body");
    }

    #[test]
    fn test_embed_input_static_none_context_is_content_only() {
        let ch = crate::parser::Chunk {
            content: "body".to_string(),
            context: None,
            ..Default::default()
        };
        assert_eq!(embed_input_for(&ch, ContextMode::Static), "body");
    }
}
