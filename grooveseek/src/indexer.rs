use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::path::Path;
use std::time::Instant;
use walkdir::WalkDir;

use crate::db::{CasDelete, ChunkVector, Database, FrontmatterTopic, PathIdHash};
use crate::embedder::Embedder;
use crate::parser::{ParserExt, Registry};
use crate::quality;

pub mod progress;
pub mod skip;

pub use skip::{LimitKey, ReadFailure, SkipReason, SkippedFile};

// (AX-40) The parts of indexing split out of this file. Every item that was reachable as
// `crate::indexer::<name>` before the split still is, through the re-exports below.
mod rename;
mod reuse;
mod single_file;

pub use rename::{RenameOutcome, rename_single_file};
pub use single_file::{
    deindex_single_file, deindex_subtree, legacy_folded_spelling, reindex_single_file_explained,
    sweep_legacy_folded_row, write_parsed_document,
};

pub(crate) use rename::{
    rename_crosses_a_parser, settle_cross_parser_rename, settle_further_crossed_read,
};
pub(crate) use reuse::{ChunkVectorCounts, CountedReindex};
pub(crate) use single_file::{reindex_single_file_checked, reindex_single_file_counted};

use rename::{detect_renames, read_crossed_rename_again_explained};
use reuse::{
    OncePass, Reindex, VectorSource, counted, decide_table_rows, docx_reread_notice,
    docx_rows_reread_notice, docx_table_reread_notice, embed_input_for, embed_input_from,
    explain_reparsed_table_rows, indexed_hash_for, plan_vector_reuse, pptx_reread_notice,
    pptx_rows_reread_notice, rereads_unchanged_as, rereads_unchanged_docx, stale_row,
    stored_chunks_match, write_matching_chunks_meta, write_reparsed_table_rows,
};
use single_file::{
    SinglePathLook, deindex_outside_kb, index_single_disk_entry, look_at_single_path,
    outside_kb_line, readable_as_the_walk_reads, reindex_single_file_checked_explained,
    write_parsed_document_with_vectors,
};

// ---------------------------------------------------------------------------
// Hardcoded denylist (F-62)
// ---------------------------------------------------------------------------

/// Hardcoded directory basenames to *always* skip during indexing /
/// validation walks, regardless of user `exclude_dirs` config. Acts as
/// a fail-safe so that `exclude_dirs = ["custom"]` (= default
/// override forgetting VCS metadata) or `exclude_dirs = []` (= explicit
/// "walk everything") does not index `.git/` / `.svn/` / `node_modules/`.
/// User `exclude_dirs` is *additionally* applied (union semantics).
pub const HARDCODED_EXCLUDE_DIRS: &[&str] = &[".git", ".svn", "node_modules"];

/// Returns `true` if `basename` matches a hardcoded skip entry, regardless
/// of user `exclude_dirs` config. Shared by `collect_source_files` (index)
/// and `validate_collect_md_files` (validate, in `src/main.rs`) so the two
/// paths agree. `pub` because the bin target accesses it via the lib
/// (`grooveseek::indexer::is_hardcoded_excluded`); the library API is
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
    /// 走査時に読んだバイト数。hash を取るために全バイトを読んでいるので
    /// 追加 I/O はゼロ。`documents.size_bytes` に記録され、`resources` が
    /// 「read が拒むサイズの文書を提示しない」判定に使う (feature-51)。
    size: u64,
}

/// [`scan_disk_entries_observed`] の結果。`entries` = hash 計算済みの index 候補、
/// `skipped` = disk に存在するが read 失敗 / size 超過で載らなかった rel path。
/// `skipped` は prune 統一原則 (§4.2) で visited_paths へ union し、既存 entry を保護する。
struct DiskScan {
    entries: Vec<DiskEntry>,
    skipped: Vec<String>,
    /// (#660) Why each file in [`DiskScan::skipped`] was declined, at the same index. Written only through
    /// [`DiskScan::skip`], which pushes both, so the two never come apart.
    skipped_why: Vec<SkipReason>,
    /// size cap で撥ねたファイルの **実測サイズ** (rel path, bytes)。
    ///
    /// 行は §4.2 で保持されるが、記録済みの `size_bytes` は「最後に索引できた
    /// 時」の小さい値のまま = 提示され続け、read は現在のファイルを見て拒む。
    /// ここで測った値を持ち帰って上書きする (codex P2 round 4)。**索引後に
    /// 消えたファイル等と違い、これは知り得る** — たった今 stat したのだから。
    oversize: Vec<(String, u64)>,
    /// (local Codex on PR #388) The frontmatter topic of each entry the scan's caller called a
    /// rename candidate, keyed by its rel path, read from **the bytes the entry's hash was
    /// taken from** ([`crate::parser::Parser::frontmatter_topic_inner`]). Rename detection pairs
    /// paths by that hash, so these are the bytes the rename is decided on, and the rename
    /// rebuilds the topic from them without a second read
    /// ([`Database::rename_documents_checked`]). Only candidates, because [`DiskEntry`] keeps no
    /// content and the rest of the run reads each file again.
    frontmatter_topics: std::collections::HashMap<String, Option<String>>,
}

impl DiskScan {
    /// Decline `rel` for `why`: the one way into [`Self::skipped`], so entry `i` of it and entry
    /// `i` of [`Self::skipped_why`] are always the same file (#660).
    fn skip(&mut self, rel: String, why: SkipReason) {
        self.skipped.push(rel);
        self.skipped_why.push(why);
    }
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
/// `Ok((None, refused))` means "skip this file, and say why on stderr"; `Err` keeps
/// `std::fs::read`'s meaning so existing read-error handling is unchanged.
///
/// The second element is the refusal itself (#660), so a caller can both name why
/// ([`reason_of_unread`]) and take the length **when the refusal was about size**
/// ([`measured`], codex P2 round 5). [`size_cap_exceeded`] stats the path and this reads the
/// handle, so a file can cross the cap *between* the two — and then this check
/// is the one that catches it. A wrapper that dropped the length existed for
/// one round; it turned out every caller wanted it, because a row that says a
/// file is small enough to serve when the read has just proved otherwise is
/// what makes the resource surface offer something unreadable.
///
/// The last argument names the `[index]` key that set `cap` (from
/// [`size_cap_key`]): a size refusal names it, so the warning says what to
/// raise, the way the decompression-side messages already do.
fn read_for_index_under(
    full: &Path,
    rel: &str,
    cap: u64,
    key: LimitKey,
) -> std::io::Result<(Option<Vec<u8>>, Option<crate::links::Refused>)> {
    match crate::links::read_checked(full, cap)? {
        crate::links::Content::Bytes(bytes) => Ok((Some(bytes), None)),
        crate::links::Content::Refused(refused) => {
            let line = refused.log_line(full);
            let refused = Some(refused);
            if measured(&refused).is_some() {
                eprintln!(
                    "Skipping {rel}: {line}; raise [index].{} to admit it",
                    key.key()
                );
            } else {
                eprintln!("Skipping {rel}: {line}");
            }
            Ok((None, refused))
        }
    }
}

/// The length a refusal measured on the handle: `Some(len)` for a size refusal, `None` for
/// the others, which say nothing about size, so the row keeps what it had (codex P2 round 5).
fn measured(refused: &Option<crate::links::Refused>) -> Option<u64> {
    match refused {
        Some(crate::links::Refused::TooLarge { len, .. }) => Some(*len),
        Some(crate::links::Refused::MultiplyLinked | crate::links::Refused::NotAPlainFile)
        | None => None,
    }
}

/// (#660, spec R2) The reason a read-time refusal gives: a hard link, something other than a
/// regular file, or a file over the cap of the given [`LimitKey`], measured on the handle.
fn reason_of_refusal(r: crate::links::Refused, key: LimitKey) -> SkipReason {
    match r {
        crate::links::Refused::MultiplyLinked => SkipReason::HardLink,
        crate::links::Refused::NotAPlainFile => SkipReason::NotPlainFile,
        crate::links::Refused::TooLarge { len, cap } => SkipReason::TooLarge {
            size: len,
            limit: cap,
            key,
        },
    }
}

/// (#660) The reason for a [`read_for_index_under`] that returned no bytes. The one place
/// every `Ok((None, refused))` arm takes its reason from.
///
/// [`read_for_index_under`] never returns `(None, None)` (its body: it returns `None` bytes only
/// in the [`crate::links::Content::Refused`] arm, and there a refusal is always present). The `None` arm exists
/// only so the match over the type is exhaustive.
fn reason_of_unread(refused: Option<crate::links::Refused>, key: LimitKey) -> SkipReason {
    refused.map_or(
        SkipReason::Unreadable {
            cause: ReadFailure::Other,
        },
        |r| reason_of_refusal(r, key),
    )
}

/// (audit AX-02) Whether the file the watcher was told about at `full` resolves
/// inside the knowledge base, every link on the way followed
/// ([`crate::links::canonical_within`]).
///
/// The watcher builds `full` as `kb_path.join(rel)` from an event path, and an
/// event can name a file through a directory link inside the KB -- on Linux
/// notify followed such links when it set up its watches until AX-02, and a
/// directory can be swapped for a link at any time. The walk never goes through
/// one (`follow_links(false)`), and `get_document` ([`crate::server`]) refuses one, so the
/// single-file paths must not be the place that does. `kb_path` is canonicalized
/// here rather than trusted to be: the comparison needs both sides spelled the
/// same, and a caller that passed a non-canonical root would otherwise have every
/// file refused.
fn resolves_inside_kb(kb_path: &Path, full: &Path) -> std::io::Result<bool> {
    let root = kb_path.canonicalize()?;
    Ok(crate::links::canonical_within(&root, full)?.is_some())
}

/// (audit AX-02, local Codex r3) Whether a component of `rel` below `kb_path` -- an ancestor
/// directory or the leaf -- is itself a link (a symlink, or on Windows a junction), found by
/// descending one component at a time with `symlink_metadata`. `kb_path` itself is not asked:
/// a knowledge base reached through a link is still the one the operator chose. A component
/// that is missing, or that cannot be examined, ends the descent with `false`, so a file that
/// is simply gone keeps the ordinary "no longer exists" answer.
///
/// Only a name-surrogate reparse point counts as a link on Windows (`FileType::is_symlink`):
/// a symlink or a junction, not an OneDrive / Dropbox placeholder, whose cloud tag is not one,
/// so such a file or directory is not refused (see [`crate::links`] for why it must not be).
fn a_link_is_on_the_way(kb_path: &Path, rel: &str) -> bool {
    let mut at = kb_path.to_path_buf();
    for segment in rel.split('/') {
        at.push(segment);
        match std::fs::symlink_metadata(&at) {
            Ok(meta) if meta.file_type().is_symlink() => return true,
            Ok(meta) if meta.file_type().is_dir() => {}
            Ok(_) | Err(_) => return false,
        }
    }
    false
}

/// (#544 C2/C3) Whether a failed stat or listing says the path is not there: only "not found"
/// and "not a directory" (a file where a directory on the way was) do. Anything else --
/// permission denied, an I/O error, a sharing violation while an editor holds the file -- says
/// nothing about whether it is still there. The one place this question is answered, for the
/// single-file paths here ([`path_is_there`]) and for the watcher's looks at the disk.
pub(crate) fn stat_error_means_gone(kind: std::io::ErrorKind) -> bool {
    matches!(
        kind,
        std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
    )
}

/// (#544 C2/C3) `Path::exists` without its swallowed error: `Ok(true)` when `full` can be
/// stat'ed (links followed, as `exists` does), `Ok(false)` when the stat failed in a way
/// [`stat_error_means_gone`] reads as "not there", and the error otherwise. `exists` read every
/// failed stat as a missing file, so a file an editor briefly held, or one behind a directory
/// that could not be read, was skipped as gone.
fn path_is_there(full: &Path) -> std::io::Result<bool> {
    match std::fs::metadata(full) {
        Ok(_) => Ok(true),
        Err(e) if stat_error_means_gone(e.kind()) => Ok(false),
        Err(e) => Err(e),
    }
}

/// [`read_for_index_under`] under the text cap's key, for the tests that read
/// a Markdown file through it.
#[cfg(test)]
fn read_for_index(
    full: &Path,
    rel: &str,
    cap: u64,
) -> std::io::Result<(Option<Vec<u8>>, Option<u64>)> {
    read_for_index_under(full, rel, cap, size_cap_key(false))
        .map(|(bytes, refused)| (bytes, measured(&refused)))
}

/// 超過警告に使う「binary」/「text」の語。cap が拡張子で決まるので、
/// 警告文も同じ分岐で選ぶ (どちらの上限に当たったか読み手に分かるように)。
fn size_cap_kind(is_binary_ext: bool) -> &'static str {
    if is_binary_ext { "binary" } else { "text" }
}

/// The `[index]` key that sets the cap a file of this kind is held to, chosen
/// by the same branch as [`size_cap_kind`], so a size warning can say which key
/// admits the file.
fn size_cap_key(is_binary_ext: bool) -> LimitKey {
    if is_binary_ext {
        LimitKey::MaxBinaryFileSize
    } else {
        LimitKey::MaxTextFileSize
    }
}

/// Whether `\` separates path components on this platform, or is an ordinary
/// filename character.
///
/// Its own function because the fold below is not the only thing that has to
/// know: [`crate::resources`] refuses a `\` in a `kb://doc/` URI exactly where
/// this says it is a separator, and the two would otherwise each carry a
/// `cfg!(windows)` that has to stay the same one.
pub fn backslash_separates_components() -> bool {
    cfg!(windows)
}

/// The separator fold, and the only place it is written.
///
/// `\` becomes `/` **only where `\` separates components**. On Unix it is an
/// ordinary filename character: a file named `secret\pay.md` sits in the KB
/// root, and folding it would store the spelling of a different path --
/// `secret/pay.md` -- that `get_document` cannot open, while the real name
/// stays reachable and matches no rule a gateway wrote against the index
/// (codex P2 round 4 on PR #310).
fn fold_separators(spelled: &str) -> String {
    if backslash_separates_components() {
        spelled.replace('\\', "/")
    } else {
        spelled.to_string()
    }
}

/// The spelling the index holds for `full`: relative to `kb_path`, components
/// joined with `/`. `None` when `full` is not under `kb_path`.
///
/// Everything that writes `documents.path`, decides exclusion, or compares a
/// request against the index asks this question, and they have to agree --
/// the `get_document` tool ([`crate::server`]) opens a document only under the
/// exact string the index stores ([`index_rel_path_exact`]). So the answer has one implementation
/// (AGENTS.md, "One question gets one implementation"), in three shapes that
/// differ only in what they do when there is no clean answer:
///
/// - this one spells a name that is not UTF-8 lossily (U+FFFD), which is what
///   the index has always stored for such a file;
/// - [`index_rel_path_exact`] returns `None` for it instead;
/// - [`index_rel_path_or_whole`] falls back to the whole path when `full` is
///   outside `kb_path`.
pub fn index_rel_path(kb_path: &Path, full: &Path) -> Option<String> {
    full.strip_prefix(kb_path)
        .ok()
        .map(|rel| fold_separators(&rel.to_string_lossy()))
}

/// [`index_rel_path`], but `None` for a name that is not UTF-8 as well.
///
/// For a caller that compares the result against a string it was handed, where
/// a lossy spelling would be compared as if it were the real one.
pub fn index_rel_path_exact(kb_path: &Path, full: &Path) -> Option<String> {
    full.strip_prefix(kb_path)
        .ok()
        .and_then(|rel| rel.to_str())
        .map(fold_separators)
}

/// [`index_rel_path`], or the whole of `full` spelled the same way when it is
/// not under `kb_path`.
///
/// The fallback is what the index walk, `groove validate` and
/// [`crate::exclusion::rel_key`] each did on their own before they shared
/// this. Their inputs come from a walk rooted inside `kb_path`, so it is not
/// expected to be taken.
pub fn index_rel_path_or_whole(kb_path: &Path, full: &Path) -> String {
    index_rel_path(kb_path, full).unwrap_or_else(|| fold_separators(&full.to_string_lossy()))
}

/// The `documents.path` a full index run would store for every file it would
/// index under `kb_path`, without embedding anything.
///
/// Test-only, and deliberately built from the walk and the scan rather than
/// from [`index_rel_path`]: a test that asked the helper directly would keep
/// passing after the scan stopped calling it.
#[cfg(test)]
pub(crate) fn scanned_rel_paths(kb_path: &Path, registry: &Registry) -> Vec<String> {
    let rules = crate::exclusion::ExclusionRules::load(kb_path, Vec::new());
    let files = collect_source_files(kb_path, registry, &rules).expect("walking the test KB");
    scan_disk_entries(
        &files,
        kb_path,
        registry,
        registry.limits().binary.cap_bytes(),
        registry.limits().text.cap_bytes(),
    )
    .entries
    .into_iter()
    .map(|e| e.rel)
    .collect()
}

/// disk 側の全 source file を走査し、raw バイト読み + バイト hash を計算する。
///
/// - **エラー隔離**: `read` 失敗や size 超過は per-file skip し、rel path を `skipped`
///   に積んで走査を続行する (旧 `.collect::<Result<Vec>>()?` の全体 abort を修正)。
/// - **size skip**: 拡張子ごとの cap (`max_binary_bytes` / `max_text_bytes`) を
///   超えるファイルは read 前に skip する ([`size_cap_exceeded`] の
///   `fs::metadata` 判定でメモリ読込自体を回避)。
/// - (feature-60) Test-only entry point; the production path is [`scan_disk_entries_observed`].
#[cfg(test)]
fn scan_disk_entries(
    source_files: &[std::path::PathBuf],
    kb_path: &Path,
    registry: &Registry,
    max_binary_bytes: u64,
    max_text_bytes: u64,
) -> DiskScan {
    scan_disk_entries_observed(
        source_files,
        kb_path,
        registry,
        max_binary_bytes,
        max_text_bytes,
        &|_, _| false,
        &mut || std::ops::ControlFlow::Continue(()),
    )
    .0
}

/// (feature-60) The scan: hash every file in `source_files`, calling `on_file` once for every
/// file it visits -- declined or not, after that file's outcome is recorded -- and stopping as
/// soon as `on_file` answers `Break`. Returns the scan as far as it got and whether it was
/// stopped, so a stopped run can still count the files it declined. The one implementation of
/// the scan (AGENTS.md "One question gets one implementation"): [`rebuild_index`] passes an
/// observer that reports the file and looks at the cancel token, and the test-only wrapper
/// next to it in this module (not linked: it is `#[cfg(test)]`, which rustdoc does not see)
/// passes one that never stops it.
///
/// `rename_candidate(rel, hash)` says which entries get their frontmatter topic read from the
/// scan's own bytes into [`DiskScan::frontmatter_topics`] (local Codex on PR #388).
fn scan_disk_entries_observed(
    source_files: &[std::path::PathBuf],
    kb_path: &Path,
    registry: &Registry,
    max_binary_bytes: u64,
    max_text_bytes: u64,
    rename_candidate: &dyn Fn(&str, &str) -> bool,
    on_file: &mut dyn FnMut() -> std::ops::ControlFlow<()>,
) -> (DiskScan, std::ops::ControlFlow<()>) {
    let binary_exts = registry.binary_extensions();
    let mut scan = DiskScan {
        entries: Vec::with_capacity(source_files.len()),
        skipped: Vec::new(),
        skipped_why: Vec::new(),
        oversize: Vec::new(),
        frontmatter_topics: std::collections::HashMap::new(),
    };
    for p in source_files {
        scan_one_file(
            p,
            kb_path,
            registry,
            &binary_exts,
            max_binary_bytes,
            max_text_bytes,
            rename_candidate,
            &mut scan,
        );
        if on_file().is_break() {
            return (scan, std::ops::ControlFlow::Break(()));
        }
    }
    (scan, std::ops::ControlFlow::Continue(()))
}

/// (feature-60) One file of [`scan_disk_entries_observed`]: hash it into the scan's
/// [`DiskScan::entries`], or record in [`DiskScan::skipped`] (and, for a size refusal,
/// [`DiskScan::oversize`]) why it was declined.
/// Split out of the loop so the observer runs after every outcome, including the ones that
/// end early here.
#[allow(clippy::too_many_arguments)]
fn scan_one_file(
    p: &std::path::Path,
    kb_path: &Path,
    registry: &Registry,
    binary_exts: &[&str],
    max_binary_bytes: u64,
    max_text_bytes: u64,
    rename_candidate: &dyn Fn(&str, &str) -> bool,
    scan: &mut DiskScan,
) {
    let rel = index_rel_path_or_whole(kb_path, p);
    let ext = p.extension().and_then(|e| e.to_str()).unwrap_or("");
    let is_binary = binary_exts.iter().any(|e| e.eq_ignore_ascii_case(ext));

    match size_cap_exceeded(p, is_binary, max_binary_bytes, max_text_bytes) {
        Ok(Some((len, cap))) => {
            let (kind, key) = (size_cap_kind(is_binary), size_cap_key(is_binary));
            eprintln!(
                "Skipping {rel}: {kind} file too large ({len} bytes > {cap} limit); raise [index].{} to admit it",
                key.key()
            );
            scan.oversize.push((rel.clone(), len));
            scan.skip(
                rel,
                SkipReason::TooLarge {
                    size: len,
                    limit: cap,
                    key,
                },
            );
            return;
        }
        Ok(None) => {}
        Err(e) => {
            eprintln!("Skipping {rel}: failed to stat: {e}");
            scan.skip(
                rel,
                SkipReason::Unreadable {
                    cause: skip::read_failure_of(&e),
                },
            );
            return;
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
    let key = size_cap_key(is_binary);
    match read_for_index_under(p, &rel, cap, key) {
        Ok((Some(bytes), _)) => {
            let hash = sha256_hex_bytes(&bytes);
            if rename_candidate(&rel, &hash) {
                scan.frontmatter_topics
                    .insert(rel.clone(), frontmatter_topic_of(registry, &rel, &bytes));
            }
            scan.entries.push(DiskEntry {
                rel,
                hash,
                full: p.to_path_buf(),
                size: bytes.len() as u64,
            });
        }
        Ok((None, refused)) => {
            // (codex P2 round 5) The file can cross the cap between the
            // stat above and this read, and then it is *this* check that
            // catches it — so the length has to come from here too, or the
            // row keeps a size the read has just disproved.
            if let Some(len) = measured(&refused) {
                scan.oversize.push((rel.clone(), len));
            }
            scan.skip(rel, reason_of_unread(refused, key));
        }
        Err(e) => {
            eprintln!("Skipping {rel}: failed to read: {e}");
            scan.skip(
                rel,
                SkipReason::Unreadable {
                    cause: skip::read_failure_of(&e),
                },
            );
        }
    }
}

/// The frontmatter topic `bytes` give the document at `rel`, read by the parser the index would
/// pick for `rel` ([`Registry::by_extension`], the lookup [`rename_crosses_a_parser`] uses) --
/// what the index would merge as `frontmatter.topic.or(path-derived)` (local Codex on PR #388).
///
/// Both rename paths ask this one question of the bytes they decided the rename on: the full
/// run's scan for each rename candidate, and [`rename_single_file`] for the bytes it hashed.
/// `None` for an extension with no parser, as for a parser whose format carries no topic.
fn frontmatter_topic_of(registry: &Registry, rel: &str, bytes: &[u8]) -> Option<String> {
    let ext = Path::new(rel)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("");
    registry
        .by_extension(ext)
        .and_then(|parser| parser.frontmatter_topic(bytes, rel))
}

// (AX-40) These two stay in this file while the rest of single-file indexing lives in
// `single_file`: `tests/doctor_cli.rs` reads this file's source to check that the shared
// insertion path resolves the chunking policy and `reindex_single_file` does not.

/// [`index_single_disk_entry`] with the reason the file was not indexed (#660): the body. Each
/// return point that does not index the file names its reason (spec §6.1, R1-R7, R11).
///
/// (kuriya 621) `counts` is set where the document ends in [`SingleResult::Updated`], from the
/// plan that decided which chunks to embed ([`ChunkVectorCounts::of_plan`]) or, for the
/// meta-only write, as every chunk kept ([`ChunkVectorCounts::all_reused`]). Callers that want
/// it go through [`counted`], which keeps it only for an `Updated` result.
#[allow(clippy::too_many_arguments)]
fn index_single_disk_entry_explained(
    db: &Database,
    embedder: &mut Embedder,
    entry: &DiskEntry,
    exclude_headings: Option<&[String]>,
    registry: &Registry,
    mode: Reindex,
    context_mode: ContextMode,
    declared: DeclaredSet<'_>,
    row_dropped_unless_updated: bool,
    counts: &mut ChunkVectorCounts,
) -> Result<Reindexed> {
    let force = mode == Reindex::Force;
    // (feature-62) A re-read the one-time docx pass asked for: past the fast path below, but
    // not a forced one.
    let reparse = mode == Reindex::Reparse;
    // (kuriya 608) The re-read of a same-bytes rename: past the fast path, not forced either.
    let renamed = mode == Reindex::Renamed;
    let (refresh_frontmatter, refresh_fields) = match mode {
        Reindex::Incremental {
            check_frontmatter,
            refresh_fields,
        } => (check_frontmatter, refresh_fields),
        Reindex::Force | Reindex::Renamed | Reindex::Reparse | Reindex::Event => (false, false),
    };
    // (AV-12) Every path that can put a document into an index arrives here, which is why the
    // chunking policy is resolved here rather than at each caller: the first attempt covered
    // `reindex_single_file` and missed the rename branch, which reaches this function directly
    // (codex P2, round 5). Ahead of the unchanged check on purpose -- what it has to answer is
    // "was there a source file here before this run", and after the insert there would be.
    resolve_code_chunk_policy(db, false)?;

    // 拡張子で Registry から Parser を選択。collect_source_files
    // が Registry の extensions() のみを拾うため、通常は必ず見つかる。
    // 見つからなければ安全側に Skip 扱いで返し、crash せず次に進む。
    let ext = entry
        .full
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("");

    // Skip unchanged files unless forced.
    // rename で path UPDATE 済のものは「DB 側 hash == disk hash」なので
    // ここで自然に skip される (embedding 再計算なし)。
    //
    // (#251) The one exception is the one-time upgrade check: an unchanged Markdown
    // document is read and parsed once more, and only its metadata may be rewritten.
    //
    // Resolved ahead of the unchanged check so the check can ask the parser, not the
    // spelling: the scan and [`Registry::by_extension`] both take `.MD` as Markdown, and a
    // comparison against the literal did not (codex P1, round 2).
    let Some(parser) = registry.by_extension(ext) else {
        return Ok(Reindexed {
            result: SingleResult::Skipped {
                reason: "no parser for extension",
                frontmatter_unparsed: false,
            },
            reason: Some(SkipReason::CorruptOrUnsupported),
        });
    };
    let unchanged = !force
        && !reparse
        && !renamed
        && db
            .get_document_hash(&entry.rel)?
            .is_some_and(|existing| existing == entry.hash);
    let refresh_only =
        unchanged && (refresh_frontmatter || refresh_fields) && parser.extension() == "md";
    if unchanged && !refresh_only {
        return Ok(Reindexed {
            result: SingleResult::Unchanged,
            reason: None,
        });
    }

    // Read + parse only for files we actually need to embed.
    // (BU-20) The bytes that become chunks come from a handle whose link count,
    // file type and size were all read off that same handle — this is the read
    // a swapped-in hard link has to get past, and cannot.
    let cap = applicable_cap(
        parser.is_binary(),
        registry.limits().binary.cap_bytes(),
        registry.limits().text.cap_bytes(),
    );
    let key = size_cap_key(parser.is_binary());
    let bytes = match read_for_index_under(&entry.full, &entry.rel, cap, key) {
        Ok((Some(b), _)) => b,
        Ok((None, refused)) => {
            // (codex P2 round 5) Same window as the scan: if the handle check
            // is what refused it, that length is the only current measurement
            // there is, and the row would otherwise keep a stale, servable one.
            if let Some(len) = measured(&refused)
                && let Err(e) = db.record_document_sizes(&[(entry.rel.as_str(), len)])
            {
                tracing::warn!("failed to record the grown size of {}: {e}", entry.rel);
            }
            return Ok(Reindexed {
                result: SingleResult::Refused,
                reason: Some(reason_of_unread(refused, key)),
            });
        }
        Err(e) => {
            eprintln!("Skipping {}: failed to read: {e}", entry.rel);
            return Ok(Reindexed {
                result: SingleResult::Skipped {
                    reason: SKIPPED_READ_FAILED,
                    frontmatter_unparsed: false,
                },
                reason: Some(SkipReason::Unreadable {
                    cause: skip::read_failure_of(&e),
                }),
            });
        }
    };
    // (feature-51, codex P2 round 1) Record the size of **these** bytes, not the
    // scan's. The scan read the file to hash it; this is a second read, and a
    // file that grew past the read cap in between would otherwise be stored with
    // the old, servable size beside chunks built from the new content — the
    // resource surface would then advertise a URI the filesystem read refuses.
    // The size and the chunks now come from one buffer.
    let size_bytes = bytes.len() as u64;
    // (#544, D5) Likewise the hash: what is written with a row names these bytes, the ones its
    // chunks are about to come from. `entry.hash` stays the scan's, for the questions about
    // the scan (the fast path above, the checks against it below).
    let read_hash = sha256_hex_bytes(&bytes);
    let written = DiskEntry {
        hash: read_hash.clone(),
        ..entry.clone()
    };
    // (#544, D5) A cross-parser rename drops its row on any result but `Updated`, so bytes that
    // are not the ones the scan (or the rename's first read) hashed must not get as far as the
    // parse: what they fail or yield says nothing about the file the rename was decided on.
    // Before the parse, so nothing was parsed and no frontmatter warning was written.
    if row_dropped_unless_updated && read_hash != entry.hash {
        return Ok(Reindexed {
            result: SingleResult::Skipped {
                reason: SKIPPED_CHANGED_DURING_READ,
                frontmatter_unparsed: false,
            },
            reason: Some(SkipReason::ChangedDuringRead),
        });
    }
    let excludes: Vec<&str> = match exclude_headings {
        Some(list) => list.iter().map(String::as_str).collect(),
        None => crate::parser::DEFAULT_EXCLUDED_HEADINGS.to_vec(),
    };
    let parsed = match parser.parse_bytes(&bytes, &entry.rel, &excludes) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("Skipping {}: parse failed: {e}", entry.rel);
            return Ok(Reindexed {
                result: SingleResult::Skipped {
                    reason: SKIPPED_PARSE_FAILED,
                    frontmatter_unparsed: false,
                },
                reason: Some(skip::reason_of_parse_error(&e)),
            });
        }
    };

    // (#251) The parser hands the refused YAML back as data; this is the one
    // place that knows the file, so this is where it is named. Written
    // directly, like `Skipping ...` above, so `--quiet` and `RUST_LOG` do not
    // hide it. Before the empty-chunks check on purpose: a frontmatter-only
    // stub with broken YAML is still worth naming even though it is skipped.
    if let Some(e) = &parsed.frontmatter_error {
        eprintln!(
            "warning: {}: failed to parse YAML frontmatter: {e}",
            entry.rel
        );
    }

    // (feature-62) The pass re-reads a file because the scan's hash matched its row. Bytes that
    // are not those bytes say nothing about the row -- the reason the frontmatter check below
    // gives (#251, codex P2 round 5) -- so the row is left for the next run.
    // (#543, local Codex, medium) A watcher event is held to the same check: its hash came from
    // [`reindex_single_file`]'s first read, and bytes that only looked empty to this second one
    // (an editor that truncates before it writes) would otherwise drop the row below. The row is
    // left for the event that write raises.
    if (reparse || mode == Reindex::Event) && read_hash != entry.hash {
        return Ok(Reindexed {
            result: SingleResult::Skipped {
                reason: SKIPPED_CHANGED_DURING_READ,
                frontmatter_unparsed: parsed.frontmatter_error.is_some(),
            },
            reason: Some(SkipReason::ChangedDuringRead),
        });
    }

    let (category, topic) = extract_category_topic(&entry.rel);

    // (#251) The upgrade check: the content is what the index already holds, so the only
    // question is whether this document should have carried the tag. A clean one is left
    // exactly as it was; a broken one gets its metadata -- which is where the tag lives --
    // written again, and nothing else is touched.
    if refresh_only {
        // "Unchanged" was decided from the scan's hash; the bytes just parsed came from a
        // second read. If they differ the row's bytes were not what was read, and saying
        // "checked" would let the originals slip past the fast path when they come back.
        // The warning above already named the file if what was read failed to parse,
        // and a named failure is a counted one (codex P2, round 6).
        if read_hash != entry.hash {
            return Ok(Reindexed {
                result: SingleResult::Skipped {
                    reason: SKIPPED_CHANGED_DURING_READ,
                    frontmatter_unparsed: parsed.frontmatter_error.is_some(),
                },
                reason: Some(SkipReason::ChangedDuringRead),
            });
        }
        // (#251 / feature-58) Two reasons open an unchanged file, and each decides
        // its own write: a broken frontmatter gets its metadata -- which is where
        // the tag lives -- written again; a changed declared-key set gets the
        // document's rows written again. A clean file under the first reason
        // alone is left exactly as it was.
        let broken = parsed.frontmatter_error.is_some();
        let tag_it = broken && refresh_frontmatter;
        if !tag_it && !refresh_fields {
            return Ok(Reindexed {
                result: SingleResult::Unchanged,
                reason: None,
            });
        }
        let tx = db.begin_transaction()?;
        if tag_it {
            db.update_document_meta(
                &entry.rel,
                parsed.frontmatter.title.as_deref(),
                parsed.frontmatter.topic.as_deref().or(topic.as_deref()),
                category.as_deref(),
                parsed.frontmatter.depth.as_deref(),
                &parsed.frontmatter.tags,
                parsed.frontmatter.date.as_deref(),
                &written.hash,
                size_bytes,
            )?;
        }
        // (codex P2 round 9 on PR #291) `declared_fields: None` means the caller does not yet
        // know the current set (see this fn's doc) -- leave the row as the last completed pass
        // left it rather than write nothing where something declared may already exist.
        if refresh_fields {
            write_declared_rows_or_mark_dirty(db, &entry.rel, &parsed.frontmatter.extra, declared)?;
        }
        tx.commit()?;
        return Ok(Reindexed {
            result: SingleResult::MetadataRefreshed {
                frontmatter_unparsed: tag_it,
            },
            reason: None,
        });
    }

    if parsed.chunks.is_empty() {
        // (feature-62) Under the new heading rule this document has nothing to index -- say
        // `exclude_headings` now covers every section. The row was written by the old rule and
        // `--force` would leave none, so it goes, the way `settle_cross_parser_rename` settles
        // a row the new parser has nothing for. The caller still counts a skip.
        if reparse {
            db.delete_document(&entry.rel)?;
        }
        // (#543) A watcher event leaves what `--force` would leave too: no row. The row the
        // earlier text wrote goes, and the result says whether there was one.
        if mode == Reindex::Event {
            return Ok(Reindexed {
                result: SingleResult::NothingToIndex {
                    deindexed: deindex_single_file(db, &entry.rel)?,
                    frontmatter_unparsed: parsed.frontmatter_error.is_some(),
                },
                reason: Some(SkipReason::NoText),
            });
        }
        return Ok(Reindexed {
            result: SingleResult::Skipped {
                reason: SKIPPED_NO_CHUNKS,
                frontmatter_unparsed: parsed.frontmatter_error.is_some(),
            },
            reason: Some(SkipReason::NoText),
        });
    }

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
    let chunks_unchanged = stored_chunks_match(db, &entry.rel, &parsed.chunks, context_mode);
    // (kuriya 670) The row under this path was written by another parser when the caller settles
    // a rename across parsers (`row_dropped_unless_updated`): the rename's first read --
    // [`Reindex::Renamed`] for same bytes, but [`Reindex::Incremental`] when the watcher's
    // [`rename_single_file`] finds the bytes changed with the rename -- and the further reads of
    // one kept awaiting -- the full run's ([`read_crossed_rename_again_explained`],
    // [`Reindex::Event`]) and the watcher's ([`reindex_single_file_checked`]). Before this, the
    // `Incremental` first read and both further reads could take the meta-only write. Matching
    // chunk text says nothing about the columns only
    // the old parser writes (a code chunk's line range, which the meta-only write below leaves
    // in place when the new parser gives none), so such a read rewrites the row whole, as
    // [`Reindex::Renamed`] does, with the stored vectors reused.
    let rewrites_whole = renamed || row_dropped_unless_updated;
    // (feature-66) Stage one of a table-rows write that re-embeds nothing, read beside the
    // comparison above; stage two reads the row again where it writes. Gated as the meta-only
    // write below is (`!force`; a re-read is not forced), so `--force` reads nothing extra.
    // (kuriya 608) Nor does a rename's re-read, which takes neither write that reads it.
    let table_rows = if !force && !rewrites_whole && chunks_unchanged {
        decide_table_rows(db, &entry.rel, &parsed.chunks)?
    } else {
        None
    };

    // (feature-62) The pass found the row already split the way this parser splits it: nothing
    // to write, and the hash already matches.
    // (feature-66) Unless its table rows are not the ones this parser marks: that column alone
    // is written, and the document still counts as unchanged. Only under the hash of the bytes
    // just parsed: the pass re-reads a row because its hash matched the scan's, and a row another
    // writer rehashed since (newer bytes, same text) would otherwise keep rows computed from the
    // older bytes under its new hash, which the fast path then never revisits (spec R7.7).
    if reparse && chunks_unchanged {
        return match &table_rows {
            Some(decision) if !decision.matches && decision.stored_hash != entry.hash => {
                Ok(Reindexed {
                    result: SingleResult::Skipped {
                        reason: SKIPPED_CHANGED_DURING_READ,
                        frontmatter_unparsed: parsed.frontmatter_error.is_some(),
                    },
                    reason: Some(SkipReason::ChangedDuringRead),
                })
            }
            Some(decision) if !decision.matches => {
                Ok(explain_reparsed_table_rows(write_reparsed_table_rows(
                    db,
                    &entry.rel,
                    decision,
                    &parsed.chunks,
                    context_mode,
                    parsed.frontmatter_error.is_some(),
                )?))
            }
            _ => Ok(Reindexed {
                result: SingleResult::Unchanged,
                reason: None,
            }),
        };
    }

    // (kuriya 608) Not for a rename's re-read: its row is rewritten whole (see
    // [`Reindex::Renamed`]), with the stored vectors reused below. (kuriya 670) Nor for any read
    // of a row another parser wrote (`rewrites_whole` above).
    if !force
        && !rewrites_whole
        && chunks_unchanged
        && let Some(done) = write_matching_chunks_meta(
            db,
            &written,
            &parsed,
            table_rows.as_ref(),
            context_mode,
            declared,
            size_bytes,
            topic.as_deref(),
            category.as_deref(),
        )?
    {
        // (kuriya 621) Nothing was sent to the embedder: every chunk kept its vector.
        *counts = ChunkVectorCounts::all_reused(parsed.chunks.len() as u32);
        return Ok(Reindexed {
            result: done,
            reason: None,
        });
    }

    // Embed first, *outside* the DB tx — fastembed inference can take
    // hundreds of ms (BGE-small) or seconds (BGE-M3) per file, and we don't
    // want a long-lived write tx blocking concurrent readers in WAL mode.
    // feature-46: Static モードでは context を前置して embed する (§4.5)。
    //
    // (feature-69) Only the chunks whose embedding input is new are embedded. Each new input is
    // matched byte for byte against the inputs of this document's stored rows -- no other
    // document's -- rebuilt under the same `context_mode` ([`plan_vector_reuse`]). A matched
    // chunk is written with the stored vector as its BLOB, not through JSON: `vec_to_json`
    // prints with `%f` and would round it. The stored rows are read here, outside the tx; the
    // copy, the delete of the old rows and the insert of the new ones all happen in the one tx
    // below. What a document sent and kept is counted by [`ChunkVectorCounts`], from this plan.
    // `--force` reads nothing and embeds every chunk (spec R7). (kuriya 608) The re-read of a
    // rename ([`Reindex::Renamed`]) reads the stored vectors like any other changed file: the
    // rows have moved to the new path by then, and only a chunk whose input changed with the
    // path (a breadcrumb taken from the file name) is embedded again. This replaces spec R7's
    // rule that a rename's forced re-parse embeds every chunk.
    let embed_inputs: Vec<String> = parsed
        .chunks
        .iter()
        .map(|c| embed_input_for(c, context_mode))
        .collect();
    let stored = if force {
        Vec::new()
    } else {
        db.stored_chunk_vectors_for_path(&entry.rel)
            .unwrap_or_else(|e| {
                eprintln!(
                    concat!(
                        "warning: {}: could not read the stored vectors, ",
                        "so every chunk is embedded again: {}"
                    ),
                    entry.rel, e
                );
                Vec::new()
            })
    };
    let stored_inputs: Vec<String> = stored
        .iter()
        .map(|row| embed_input_from(&row.content, row.context_text.as_deref(), context_mode))
        .collect();
    let plan = plan_vector_reuse(&embed_inputs, &stored_inputs);
    let texts: Vec<&str> = plan
        .to_embed
        .iter()
        .map(|&i| embed_inputs[i].as_str())
        .collect();
    let embed_result = if texts.is_empty() {
        Ok(Vec::new())
    } else {
        embedder.embed_texts(&texts)
    };
    let embeddings = match embed_result {
        Ok(embeddings) => embeddings,
        Err(e) => {
            // (AW-04) A refusal of this file's input is this file's problem: skip it and
            // let the run go on. Only the status is printed -- the error's text carries
            // the endpoint's response body (trap #275).
            if let Some(rejected) = e.downcast_ref::<crate::embedder::EmbedInputRejected>() {
                // Worded for the row's state once the caller is done (codex P1 on PR #329):
                // a file with a row keeps it; one without -- every file in a forced rebuild,
                // whose reset emptied the index, and a new file -- is simply not in it, and
                // so is the destination of a cross-parser rename, whose row the caller drops.
                let outcome =
                    if !row_dropped_unless_updated && db.get_document_hash(&entry.rel)?.is_some() {
                        "the index keeps what it had for this file"
                    } else {
                        "this file is not in the index"
                    };
                eprintln!(
                    "warning: {}: embedding endpoint rejected the input (HTTP {}); skipped, \
                     {outcome}",
                    entry.rel, rejected.status
                );
                return Ok(Reindexed {
                    result: SingleResult::Skipped {
                        reason: SKIPPED_EMBED_REJECTED,
                        frontmatter_unparsed: parsed.frontmatter_error.is_some(),
                    },
                    reason: Some(SkipReason::EmbedRejected {
                        http_status: rejected.status,
                    }),
                });
            }
            return Err(e).with_context(|| format!("failed to embed chunks for {}", entry.rel));
        }
    };

    anyhow::ensure!(
        embeddings.len() == plan.to_embed.len(),
        "{}: {} chunks to embed but {} embeddings",
        entry.rel,
        plan.to_embed.len(),
        embeddings.len()
    );
    let vectors: Vec<ChunkVector<'_>> = plan
        .sources
        .iter()
        .map(|source| match *source {
            VectorSource::Stored(j) => ChunkVector::Stored(&stored[j].embedding),
            VectorSource::Computed(k) => ChunkVector::Computed(&embeddings[k]),
        })
        .collect();

    // Per-file atomicity (F-32): wrap upsert_document + N x insert_chunk
    // in a single tx so that a partial failure (e.g. vec_chunks dim
    // mismatch on the 3rd chunk) rolls the whole file back instead of
    // leaving a documents row with M < N chunks. Immediate because the first
    // statement (`upsert_document`) reads: a deferred read cannot wait for the
    // write lock another connection holds.
    let tx = db.begin_immediate_transaction()?;
    write_parsed_document_with_vectors(
        db,
        &entry.rel,
        &parsed,
        parser,
        &vectors,
        context_mode,
        &written.hash,
        size_bytes,
        topic.as_deref(),
        category.as_deref(),
    )?;
    // (codex P2 round 9 on PR #291) Same "pending means leave it" rule as the two paths above:
    // a `None` here writes nothing, so a freshly written or newly changed document simply has
    // no `document_fields` rows until a completed [`rebuild_index`] pass gives a real answer --
    // preferable to a wrong one guessed as empty.
    write_declared_rows_or_mark_dirty(db, &entry.rel, &parsed.frontmatter.extra, declared)?;
    tx.commit()?;

    // (kuriya 621) After the commit: a document whose write failed was not updated.
    *counts = ChunkVectorCounts::of_plan(&plan);
    Ok(Reindexed {
        result: SingleResult::Updated {
            chunks: parsed.chunks.len() as u32,
            frontmatter_unparsed: parsed.frontmatter_error.is_some(),
        },
        reason: None,
    })
}

/// 1 つの source file を index / 再 index する。
///
/// - `kb_path` は watcher からは canonical (`groove serve` が canonicalize して渡す)。
///   canonical でなくても動く: KB の内外の判定 ([`resolves_inside_kb`]) は root を
///   canonicalize し直してから比べる (audit AX-02)
/// - `rel` は forward-slash、`kb_path` からの相対パス (e.g. `"notes/a.md"`)
/// - (audit AX-02) KB の外に解ける file は読まない。`rel` の行があれば消して
///   [`SingleResult::OutsideKnowledgeBaseDeindexed`]、無ければ [`SingleResult::Refused`]
///   (full index の walk も link を辿らず、その行を落とすのと揃える)
/// - 拡張子が `registry` に登録されていなければ `Skipped` を返す
/// - hash が DB と一致なら `Unchanged`、違えば upsert + embedding 再計算
/// - (#543) 読めて parse できたが embed する chunk が無い (空の file、frontmatter だけの stub) なら、
///   `rel` の行を消して [`SingleResult::NothingToIndex`] を返す (`--force` もその file の行を作らない)。
///   [`rebuild_index`] の増分 run は同じ file で行を残す ([`SingleResult::Skipped`])
/// - **size cap**: registry の上限 ([`Registry::limits`]、`[index]` で変えられる。既定は
///   [`crate::parser::MAX_RAW_BINARY_BYTES`] / [`crate::parser::MAX_RAW_TEXT_BYTES`]) を
///   超えていれば `fs::read` する前に skip する ([`size_cap_exceeded`]、codex P2 round 2:
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
    reindex_single_file_checked_explained(
        db,
        embedder,
        kb_path,
        rel,
        exclude_headings,
        registry,
        false,
        &mut ChunkVectorCounts::default(),
    )
    .map(|r| r.result)
}

/// この document path を現在の registry で開けるか。
///
/// 「どの行を提供できるか」と「どの行が registry から外れたか」(AU-06) を
/// 1 つの述語で表す。2 箇所で書くと、`resources/list` が出す URI と
/// `validate_get_document_path` の門番が食い違う — feature-50 で実際に
/// 起きた形 (listing が `.pdf` の `kb://doc/...` を出し、読み返しが拒否する)。
pub fn extension_is_registered(path: &str, registry: &crate::parser::Registry) -> bool {
    std::path::Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|ext| registry.has_extension(ext))
}

/// 現在の registry に載っていない拡張子を持つ document path を返す (AU-06)。
///
/// `[parsers].enabled` を狭めた後 (`.xls` の取り下げのように、狭めざるを得なく
/// なった場合を含む)、その拡張子の行は DB に残る。`groove index` を 1 度回せば
/// `documents_to_delete` が prune するが、`serve` は起動時に index しないので、
/// `serve` しか使わない運用では残り続ける。
///
/// 残ると **search には出るのに `get_document` が拒否する** hit になる
/// (`validate_get_document_path` が `registry.has_extension` で門番をするため)。
/// AU-02 で直したのと同じ「見つかるのに開けない」症状。
///
/// **削除はしない**。狭めた設定は一時的なこともあり、起動のたびに黙って行を
/// 消すのは破壊的すぎる。呼び出し側は件数を warn して `groove index` を促す。
pub fn paths_with_unregistered_extension(
    all_db_paths: &[String],
    registry: &crate::parser::Registry,
) -> Vec<String> {
    all_db_paths
        .iter()
        .filter(|p| !extension_is_registered(p, registry))
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

/// (ADR-0031) Candidates per immediate transaction of the prune. Wide enough that a large
/// prune takes the write lock a few times rather than once per row; the aim for one batch's
/// hold is about 100 ms, since another writer waits it out within the busy timeout.
///
/// Measured over 10 000 candidates of 3 chunks each, with 2-dimensional vectors (debug
/// build, Windows), the longest hold of one batch was 1015 ms at 64, 2084 ms at 128,
/// 4738 ms at 256, 8584 ms at 512 and 18116 ms at 1024: 16 to 19 ms a row for the longest
/// batch, and about 8 ms a row for the whole prune divided by its candidates. The aim was
/// not met: even at 64 one batch held the lock for about 1 s. A waiting writer does not fail
/// for it, as it keeps trying until the busy timeout runs out. 64 is the narrowest width
/// measured, and narrowing did not make the whole prune longer: it took 81 to 91 s at
/// every width.
pub(crate) const PRUNE_BATCH: usize = 64;

/// (ADR-0031) The prune of a completed run: `candidates` (sorted) in batches of `batch`,
/// each through [`Database::delete_documents_if_unchanged`] against the `(id, content_hash)`
/// the run read into the snapshot before its walk. `on_deleted` is called for each deleted
/// path after its batch commits. An `Err` from a batch is returned as is; that batch wrote
/// nothing, and the batches before it stay committed. The `kept_changed` those earlier batches
/// counted is dropped with the `Err`, so no note is printed for it; the next run judges those
/// rows again. The returned `kept` paths are rows this run did not read; at the end of the run
/// the caller marks those a stale one-time pass covers, and under `force` every one, with
/// [`HASH_AWAITING_REPARSE`] (ADR-0031). With `mark_kept` (a forced run) each batch also marks
/// its kept rows in its own transaction, so a later batch's `Err` cannot leave them unmarked.
fn prune_candidates(
    db: &Database,
    snapshot: &PathIdHash,
    candidates: &[String],
    batch: usize,
    mark_kept: bool,
    on_deleted: &mut dyn FnMut(&str),
) -> Result<CasDelete> {
    let mut out = CasDelete::default();
    for slice in candidates.chunks(batch.max(1)) {
        // Every candidate comes from the snapshot's keys; one that does not is not deleted.
        let expected: Vec<(&str, i64, &str)> = slice
            .iter()
            .filter_map(|p| {
                snapshot
                    .get(p)
                    .map(|(id, hash)| (p.as_str(), *id, hash.as_str()))
            })
            .collect();
        let done = db.delete_documents_if_unchanged(&expected, mark_kept)?;
        for path in &done.deleted {
            on_deleted(path);
        }
        out.kept_changed += done.kept_changed;
        out.kept.extend(done.kept);
        out.deleted.extend(done.deleted);
    }
    Ok(out)
}

/// (ADR-0031) The stderr line for the rows the prune kept because another writer changed
/// them during the run; `None` for 0. ASCII.
fn kept_changed_note(n: u32) -> Option<String> {
    (n > 0).then(|| {
        format!(
            concat!(
                "note: kept {} row(s) another writer changed during this run; ",
                "the next run checks them again."
            ),
            n
        )
    })
}

/// (ADR-0031) The stderr line for skipped renames; `None` for 0. ASCII.
fn renames_skipped_note(n: u32) -> Option<String> {
    (n > 0).then(|| {
        format!(
            concat!(
                "note: skipped {} rename(s) another writer changed during this run; ",
                "the new paths are indexed as ordinary files."
            ),
            n
        )
    })
}

/// (feature-60) The order the per-document loop in [`rebuild_index`] visits `entries`: those
/// whose `rel` is in `first` (the renames read again past the hash fast path,
/// [`Reindex::Renamed`]), then the rest, each group in
/// the order `entries` already has -- the walk's, which sorts `PathBuf`s component by
/// component. Split rather than re-sorted: sorting by the `rel` string would move `a-b/x.md`
/// ahead of `a/b.md` and change the order of runs that have no rename at all. Putting the
/// rename re-reads first bounds the stretch in which the loop does not stop for a cancel
/// (it never stops before one of them) to their number.
fn processing_order<'a>(entries: &'a [DiskEntry], first: &HashSet<String>) -> Vec<&'a DiskEntry> {
    let (mut order, rest): (Vec<&DiskEntry>, Vec<&DiskEntry>) =
        entries.iter().partition(|e| first.contains(&e.rel));
    order.extend(rest);
    order
}

/// (AW-04) Where [`IndexResult::all_inputs_rejected_message`] says the skipped
/// files were named, for `groove index`.
pub const REJECTIONS_NAMED_ABOVE: &str = "see the warnings above";
/// The same, for MCP `rebuild_index`, whose caller does not see the server's stderr.
pub const REJECTIONS_NAMED_ON_SERVER_STDERR: &str = "each named on the server's stderr";

/// Summary returned by [`rebuild_index`].
#[derive(Debug, Default, Clone)]
pub struct IndexResult {
    pub total_documents: u32,
    pub updated: u32,
    /// File-rename を検出した件数。embedding は再計算されず
    /// `documents.path` だけが UPDATE された数。
    pub renamed: u32,
    pub deleted: u32,
    /// disk 上に存在するが index されなかったファイル数 (read/size/parse 失敗・空本文、
    /// hard link、index が保持できない名前)。path 単位で数え、[`Self::skipped_files`] の
    /// 件数と常に等しい (#660)。[`Self::unspellable`] と [`Self::embed_rejected`] はこの内訳。
    pub skipped: u32,
    /// (#660) Every file counted in [`Self::skipped`], one entry each, sorted by
    /// [`SkippedFile::path`]. Its length equals [`Self::skipped`] for every value
    /// [`rebuild_index`] returns, completed or [`Self::cancelled`]. A hard link and a name the index cannot hold
    /// ([`Self::unspellable`]) are in both.
    pub skipped_files: Vec<SkippedFile>,
    /// Markdown files this run named with `warning: <path>: failed to parse
    /// YAML frontmatter` (#251): the ones indexed with empty metadata and the
    /// `frontmatter:unparsed` tag, the ones skipped for having no chunks after
    /// that (a frontmatter-only stub), and -- on the first run of this build
    /// over an older index -- the unchanged ones the one-time check tagged.
    /// Per run, like [`Self::updated`]: a file that has not changed is not
    /// re-parsed and so is not re-counted once the check has run; the `force`
    /// argument of [`rebuild_index`] re-parses everything.
    pub frontmatter_unparsed: u32,
    /// (ADR-0023) Entries the walk left out because their name is not one the
    /// index can hold ([`crate::resources::doc_is_addressable`]): a file, or a
    /// directory together with everything under it, counted once each and
    /// each named on stderr. (#660) Part of [`Self::skipped`]: the entries of
    /// [`Self::skipped_files`] whose reason is [`SkipReason::UnspellableName`],
    /// counted by path like the rest of the list.
    pub unspellable: u32,
    /// (AW-04) Files skipped because the embedding endpoint refused their
    /// input before accepting any of it. Also counted in [`Self::skipped`]. A
    /// file refused only after the endpoint accepted an earlier batch of it is
    /// skipped (and in [`Self::skipped`]) but not counted here: the accepted
    /// batch shows the configuration works; it is counted in
    /// [`Self::embedded`] instead.
    pub embed_rejected: u32,
    /// (AW-04) Files for which the embedding endpoint accepted at least one
    /// batch this run: every file embedded whole, plus every file refused
    /// only after an earlier batch of it was accepted (skipped, and in
    /// [`Self::skipped`]). Either shows the endpoint and model work. Not
    /// [`Self::updated`]: a metadata-only update counts there without
    /// embedding anything.
    pub embedded: u32,
    /// (AW-04, codex P1 on PR #329) In a forced rebuild, the files the embedding
    /// endpoint refused, outright or after an accepted batch. The rebuild emptied
    /// the index first, so each of them is missing from it. Zero in an
    /// incremental run, where a refused file keeps its previous row.
    pub forced_rejected: u32,
    /// (feature-69) Chunks of the documents this run ended in [`SingleResult::Updated`] whose
    /// vector was embedded this run. Counted with [`Self::chunks_reused`] per document from the
    /// embedder's running total of accepted inputs. For a [`Self::cancelled`] run, the count up to the stop.
    pub chunks_embedded: u32,
    /// (feature-69) Chunks of the documents this run ended in [`SingleResult::Updated`] whose
    /// stored vector was copied over, or kept as it was by the fast path for a document whose
    /// chunks all match. Always 0 under `force`, which embeds every chunk. For a
    /// [`Self::cancelled`] run, the count up to the stop.
    pub chunks_reused: u32,
    /// (ADR-0031) Rows the prune kept because another writer changed them during this run
    /// (their `(id, content_hash)` no longer matched what the run read before its walk). The
    /// next run judges them again. Always 0 for a [`Self::cancelled`] run, which does not prune.
    pub kept_changed: u32,
    /// (ADR-0031) Detected renames this run did not apply because another writer had changed the
    /// old row or written the new path during the run. Each new path is indexed as an ordinary
    /// file; each old row goes to the prune.
    pub renames_skipped: u32,
    pub total_chunks: u32,
    pub duration_ms: u64,
    /// (v1.14.0+) The run stopped at a check point because the
    /// [`progress::CancelToken`] on its reporter was set
    /// ([`progress::ProgressReporter::with_cancel`]). Always `false` for
    /// `groove index` and the MCP tool that rebuilds the index, which call
    /// [`rebuild_index`] with no token.
    ///
    /// The counts then cover what the run did before it stopped:
    /// [`Self::updated`], [`Self::skipped`], [`Self::embedded`] and the rest
    /// count up to the stop, [`Self::renamed`] is 0 if it stopped during the
    /// scan, [`Self::deleted`] is always 0, and [`Self::total_documents`] /
    /// [`Self::total_chunks`] are the index as it stands.
    ///
    /// That index is consistent per document: no document is half written,
    /// and a document whose move forced a re-parse was re-parsed before the
    /// stop. The next run resumes it -- documents committed here are skipped
    /// by their hash. Rows of files deleted from disk stay until a run
    /// completes, and so does the bookkeeping a completed run records (the
    /// frontmatter policy and the declared-field set; until the latter is
    /// recorded, field filters are refused and the next run refreshes the
    /// fields). A stopped run leaves no declared-field pass of its own open.
    /// After a stopped `force` run the index holds only the documents
    /// committed before the stop; an incremental run adds the rest, and with
    /// no schema it records the empty declared-field set the way a completed
    /// run does.
    ///
    /// [`Self::fails_strict_frontmatter`], [`Self::fails_all_inputs_rejected`]
    /// and [`Self::fails_forced_rebuild_rejections`] judge a completed run. Do
    /// not report a result whose [`Self::cancelled`] is `true` as a failed run
    /// because of them.
    pub cancelled: bool,
}

impl IndexResult {
    /// Whether this run is a failure under `[index].fail_on_frontmatter_error`
    /// (#251). One decision, rendered twice: `groove index` turns `true` into
    /// exit 1, the MCP `rebuild_index` tool into an `error` beside the counts.
    /// The two surfaces call this rather than each comparing the count, so
    /// they cannot come to disagree (codex P1, round 4).
    pub fn fails_strict_frontmatter(&self, fail_on_frontmatter_error: bool) -> bool {
        fail_on_frontmatter_error && self.frontmatter_unparsed > 0
    }

    /// (AW-04) Whether this run had the endpoint refuse at least one file's
    /// input outright ([`Self::embed_rejected`]) and accept nothing at all
    /// ([`Self::embedded`], which counts a batch accepted before a refusal) --
    /// which points at the configuration (model, endpoint) rather than at the
    /// files. One decision, rendered twice:
    /// `groove index` exits non-zero, MCP `rebuild_index` answers with an
    /// `error`. The run itself completed, deletions included.
    pub fn fails_all_inputs_rejected(&self) -> bool {
        self.embed_rejected > 0 && self.embedded == 0
    }

    /// The error both surfaces give when [`Self::fails_all_inputs_rejected`]
    /// (spec 3.4). `named` says where the skipped files were named:
    /// [`REJECTIONS_NAMED_ABOVE`] for `groove index`, whose warnings are just
    /// above it, [`REJECTIONS_NAMED_ON_SERVER_STDERR`] for MCP, whose caller
    /// does not see them.
    pub fn all_inputs_rejected_message(&self, named: &str) -> String {
        format!(
            "the embedding endpoint rejected every input this run tried to embed ({} file(s), \
             {named}); nothing was embedded. Check [embedding] model / document_model and \
             endpoint. The rest of the run (deletions, bookkeeping) completed.",
            self.embed_rejected
        )
    }

    /// (AW-04, codex P1 on PR #329) Whether a forced rebuild had the endpoint
    /// refuse any file ([`Self::forced_rejected`]). The reset emptied the index,
    /// so unlike an incremental run no previous row stands in for the file: one
    /// refusal is a gap in the index, and the run fails once it has completed.
    /// Rendered like [`Self::fails_all_inputs_rejected`], which the surfaces
    /// check first.
    pub fn fails_forced_rebuild_rejections(&self) -> bool {
        self.forced_rejected > 0
    }

    /// The error both surfaces give when
    /// [`Self::fails_forced_rebuild_rejections`]. `named` is as for
    /// [`Self::all_inputs_rejected_message`].
    pub fn forced_rebuild_rejections_message(&self, named: &str) -> String {
        format!(
            "the embedding endpoint rejected the input of {} file(s) during a forced rebuild \
             ({named}); the rebuild emptied the index first, so those files are not in it. \
             Fix them or lower [embedding] max_input_chars, then run `groove index`. \
             The rest of the run (deletions, bookkeeping) completed.",
            self.forced_rejected
        )
    }

    /// The `Done in ...` line `groove index` prints. [`Self::unspellable`] is
    /// appended only when it is not zero, so the line an ordinary run prints
    /// -- which tests and scripts match -- does not change. (feature-69) This
    /// run's [`Self::chunks_embedded`] and [`Self::chunks_reused`] follow the
    /// chunk count only when either is not zero, for the same reason.
    pub fn summary_line(&self) -> String {
        let mut line = format!(
            "Done in {}ms: {} docs ({} updated, {} renamed, {} deleted, {} skipped, {} frontmatter unparsed), {} chunks",
            self.duration_ms,
            self.total_documents,
            self.updated,
            self.renamed,
            self.deleted,
            self.skipped,
            self.frontmatter_unparsed,
            self.total_chunks
        );
        if self.chunks_embedded > 0 || self.chunks_reused > 0 {
            line.push_str(&format!(
                " (this run: {} embedded, {} reused)",
                self.chunks_embedded, self.chunks_reused
            ));
        }
        if self.unspellable > 0 {
            line.push_str(&format!(
                ", {} not indexed (a name get_document cannot take, see the warnings above)",
                self.unspellable
            ));
        }
        line
    }
}

/// 単一ファイルのインデックス結果。`rebuild_index` 内での
/// per-file 処理と、watcher 経由の `reindex_single_file` で共通に使う。
#[derive(Debug, PartialEq)]
pub enum SingleResult {
    /// hash が既存と一致、embedding 再計算不要 (no-op)
    Unchanged,
    /// upsert + embedding 完了 (chunk 数)。`frontmatter_unparsed` は #251:
    /// この文書の YAML frontmatter が parse できず、metadata 空 +
    /// `frontmatter:unparsed` tag で書かれたことを示す。[`rebuild_index`] の
    /// summary はこれを数える。
    Updated {
        chunks: u32,
        frontmatter_unparsed: bool,
    },
    /// 処理対象外 (空本文など)。reason は human-readable。`frontmatter_unparsed`
    /// は #251: 索引されなかったが YAML frontmatter の parse には失敗していた
    /// (= frontmatter だけの stub)。warning で名指しした以上、summary でも数える。
    Skipped {
        reason: &'static str,
        frontmatter_unparsed: bool,
    },
    /// (#251 / feature-58) 内容は変わっていないが metadata だけ書き直した。embedding は触らない。
    /// [`rebuild_index`] の upgrade 経路でしか返らない。`frontmatter_unparsed` は
    /// **この文書の frontmatter が壊れていたか** — 集計はそれだけを
    /// [`IndexResult::frontmatter_unparsed`] に数える。宣言 key の書き直しだけで
    /// 通った文書は `false` で返り、`[index].fail_on_frontmatter_error` を動かさない。
    MetadataRefreshed { frontmatter_unparsed: bool },
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
    /// (audit AX-02, review P2-1) The file is reached through a link -- to outside the
    /// knowledge base, or (GitHub codex r1 on PR #391) to elsewhere inside it -- and the row
    /// that was under its path has been **deleted**. Only
    /// [`reindex_single_file`] returns it (when no row was there it returns
    /// [`SingleResult::Refused`]); [`rebuild_index`]'s walk does not follow links, so a file
    /// like this never reaches it.
    OutsideKnowledgeBaseDeindexed,
    /// (#543) The file was read and parsed and holds nothing to embed -- an empty file, a
    /// frontmatter-only stub -- and the row that was under its path, if any, has been
    /// **deleted**: `--force` leaves no row for such a file, and the row its earlier text left
    /// would keep answering searches with that text. `deindexed` says whether a row was there;
    /// `frontmatter_unparsed` is [`SingleResult::Skipped`]'s. Only [`reindex_single_file`]
    /// returns it ([`Reindex::Event`]); [`rebuild_index`] gets a [`SingleResult::Skipped`] for
    /// the same file and keeps the row.
    NothingToIndex {
        deindexed: bool,
        frontmatter_unparsed: bool,
    },
}

/// [`SingleResult`] and, when the file was not indexed, why (#660). [`Reindexed::reason`] is
/// `Some` exactly when [`rebuild_index`] would count this result in [`IndexResult::skipped`],
/// except for a path reached through a link (the file itself a symlink included): that is
/// [`SingleResult::Refused`] or [`SingleResult::OutsideKnowledgeBaseDeindexed`] with no reason,
/// because the full index leaves such a file out on purpose and lists it nowhere.
/// At the watcher's entry a failure to stat or resolve the file, or the first read of it, is
/// still an `Err`, as before. The second read (the one the indexing body does after the hash
/// check) failing is a [`SingleResult::Skipped`] with the reason [`SkipReason::Unreadable`] here
/// as in the full index, with any cause; it is also where a refusal of that read (hard link, not
/// a plain file, too large) comes from.
/// [`reindex_single_file_explained`] does not itself refuse a path whose name the index cannot
/// store or that `.grooveignore` excludes: that check is the caller's (the watcher makes it
/// before calling), so for such a path the reason here can differ from the one
/// [`rebuild_index`] lists.
#[derive(Debug, PartialEq)]
pub struct Reindexed {
    pub result: SingleResult,
    pub reason: Option<SkipReason>,
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
/// - `None` → use [`crate::parser::DEFAULT_EXCLUDED_HEADINGS`]
/// - `Some(list)` → completely overrides the default list (pass `&[]` to
///   disable heading-based exclusion entirely).
///
/// `schema` is [`load_declared_schema`]'s result, not a path (codex P2 round 3 on PR #291):
/// the caller reads `groove-schema.toml` once, before anything destructive it is about to do,
/// and hands the snapshot in here rather than letting this function read the file again after
/// the fact. See [`load_declared_schema`]'s doc for why a second read, on a schema a caller has
/// already validated, would only reopen the TOCTOU window `--force` closed (codex P1 round 1).
///
/// (v1.14.0+) A reporter carrying a [`progress::CancelToken`]
/// ([`progress::ProgressReporter::with_cancel`]) can stop the run at a check point: after each
/// scanned file, before the renames, before each document (after any rename re-reads past the hash fast path), and
/// before the deletion sweep. The run then returns `Ok` with [`IndexResult::cancelled`] set;
/// see [`progress::CancelToken`] for how late a stop can come and [`IndexResult::cancelled`]
/// for the index it leaves.
#[allow(clippy::too_many_arguments)] // D-10 で 8 個に。config struct 化は別 cycle
pub fn rebuild_index(
    db: &Database,
    embedder: &mut Embedder,
    kb_path: &Path,
    schema: Option<crate::schema::Schema>,
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

    // (AW-03, ADR-0024) `force` empties the index below, in a transaction of its own that
    // commits before the first document is embedded. Hear from the provider first, before
    // this function writes anything, so a wrong key or an endpoint that is down fails a run
    // that has removed nothing from the index. This is the only reset `index --force` and MCP
    // `rebuild_index {force: true}` both reach, so each sends exactly one probe.
    if force {
        embedder.probe_before_reset()?;
    }

    // legacy DB を引き継いだケースで FTS が空のままにならないよう、
    // まず既存 chunks のうち FTS 未登録のものを backfill する。
    //
    // **force のときは走らせない** (AW-03)。すぐ下の reset が既存 chunks ごと
    // 消すので仕事は捨てられるうえ、legacy DB で backfill が失敗すると、それを
    // 直すための `--force` まで止めてしまう。
    if !force {
        let backfilled = db.backfill_fts()?;
        if backfilled > 0 {
            eprintln!("Backfilled {backfilled} chunks into FTS index");
        }
    }

    // legacy DB のチャンクを一度だけ再評価する。既に正しいスコアが入っている行は
    // 触らないため冪等。
    //
    // **force のときは走らせない。** すぐ下の reset_and_resolve_context_mode が
    // force なら DB を消し、その後の再 index が全チャンクを作り直すので、ここでの
    // UPDATE は捨てられる行に対する仕事になる。加えて backfill は「短い定義チャンクを
    // 昇格させた」時に `groove index --force` を案内する warning を出すので、
    // **force で走っている最中に force を勧める**ことになる (codex P2 on PR #263)。
    if !force {
        let quality = db.backfill_quality(&registry.binary_extensions())?;
        if quality.updated > 0 {
            eprintln!("Backfilled {} chunks with quality scores", quality.updated);
        }
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

    // (feature-56) Notice when the code chunk budget has moved since these chunks were cut.
    if let Some(budget) = registry.code_max_chunk_chars() {
        resolve_code_chunk_budget(db, budget, force)?;
    }
    // (AV-12) And which policy cut them, which decides whether `doctor` can answer about the
    // files this run will not re-chunk. Not gated on a code parser being enabled: an index
    // built with one and re-indexed without it still holds those documents.
    resolve_code_chunk_policy(db, force)?;
    // (#251, codex P1 round 1) An index written before the parser tagged a broken frontmatter
    // holds such documents with a matching hash and no tag, and the unchanged fast path would
    // never look at them again. The first run of this build re-reads the frontmatter of every
    // unchanged Markdown document once; `--force` re-parses everything anyway.
    let refresh_frontmatter =
        !force && db.read_frontmatter_policy()?.as_deref() != Some(FRONTMATTER_POLICY);
    // (feature-62) The same kind of generation for `.docx`: an index whose `.docx` rows were
    // split before headings were read from `word/styles.xml` re-reads each unchanged one once
    // ([`DOCX_HEADING_POLICY`], ADR-0028). `--force` re-parses everything anyway.
    // (feature-63) So does one whose rows were cut before a heading with no body text was
    // folded into the next section ([`DOCX_SECTION_POLICY`]).
    // (feature-64) And one whose tables were written before each row became one line
    // ([`DOCX_TABLE_POLICY`]). When the other two keys are at their values and only this one is
    // not, the notice names the table change ([`docx_table_reread_notice`]). The four `.docx` keys
    // and the two `.pptx` keys are read here, once, before the loop; the run does not read them
    // again.
    let heading_current = db.read_docx_heading_policy()?.as_deref() == Some(DOCX_HEADING_POLICY);
    let section_current = db.read_docx_section_policy()?.as_deref() == Some(DOCX_SECTION_POLICY);
    let table_current = db.read_docx_table_policy()?.as_deref() == Some(DOCX_TABLE_POLICY);
    // (feature-66) And one whose `.docx` chunks were written before they carried their table
    // rows ([`DOCX_TABLE_ROWS_POLICY`]), the fourth `.docx` key (ADR-0030).
    let rows_docx_current =
        db.read_docx_table_rows_policy()?.as_deref() == Some(DOCX_TABLE_ROWS_POLICY);
    let refresh_docx =
        !force && !(heading_current && section_current && table_current && rows_docx_current);
    let tables_alone_stale = heading_current && section_current && !table_current;
    let rows_docx_alone_stale =
        heading_current && section_current && table_current && !rows_docx_current;
    // (feature-65) `.pptx` has a pass of its own, opened by its own key
    // ([`PPTX_TABLE_BREAK_POLICY`], ADR-0029): an index whose `.pptx` rows were written before
    // table rows became one line and line breaks were kept re-reads each unchanged one once.
    // Each pass re-reads and marks only its own format's rows, so a stale key of one format
    // never touches the other's.
    // (feature-66) Its second key, [`PPTX_TABLE_ROWS_POLICY`], opens the same pass.
    let pptx_current =
        db.read_pptx_table_break_policy()?.as_deref() == Some(PPTX_TABLE_BREAK_POLICY);
    let rows_pptx_current =
        db.read_pptx_table_rows_policy()?.as_deref() == Some(PPTX_TABLE_ROWS_POLICY);
    let refresh_pptx = !force && !(pptx_current && rows_pptx_current);
    let rows_pptx_alone_stale = pptx_current && !rows_pptx_current;
    let passes = [
        OncePass {
            extension: "docx",
            stale: refresh_docx,
        },
        OncePass {
            extension: "pptx",
            stale: refresh_pptx,
        },
    ];
    let any_stale = passes.iter().any(|p| p.stale);

    // (feature-58) The keys the schema declares are what `document_fields` holds.
    // `schema` was already loaded above, before the destructive reset, from the
    // same path `groove validate` uses -- silently indexing without it would
    // make every `--field` answer empty and say nothing. Compared with what the
    // last completed run recorded: on a difference every unchanged Markdown
    // document is read once more so its rows follow the schema, without
    // re-embedding.
    let declared_fields = declared_field_names(schema.as_ref());
    let declared_json = serde_json::to_string(&declared_fields)?;
    let stored_declared = db.read_declared_fields()?;
    // (codex P2 round 2 on PR #291) "absent generation key + empty declared set" is safe to
    // skip only when `document_fields` is actually empty, not merely assumed to be. A run that
    // declared keys, wrote some documents' rows via `replace_document_fields`, and was
    // interrupted before `write_declared_fields` recorded the set leaves both: no generation
    // key stored AND leftover rows. If the schema is then removed (`declared_json ==
    // DECLARED_FIELDS_NONE`), the shortcut must not fire -- it would record `[]` over rows the
    // refresh pass exists to clear, and `--field` would keep matching keys the completed index
    // no longer declares.
    let refresh_fields = !force
        && stored_declared.as_deref() != Some(declared_json.as_str())
        && !(stored_declared.is_none()
            && declared_json == DECLARED_FIELDS_NONE
            && db.document_fields_is_empty()?);
    // (codex P2 round 3 on PR #291) Clear the generation key before the refresh writes a single
    // `document_fields` row, not after. An interrupted pass -- some documents' rows committed,
    // the process dies before the end-of-run `write_declared_fields` below -- must leave the key
    // absent, so the next run refreshes again even if it finds the *old* schema restored: with
    // the key still holding the old value, stored-old == declared-old would compare equal and
    // skip the refresh, leaving the newer rows behind indefinitely. `force` rewrites every
    // document's rows the same way a refresh does, so an interruption mid-force leaves the same
    // kind of partial state and clears the key for the same reason. See
    // `Database::clear_declared_fields`'s doc for why "absent" is safe in both directions,
    // now that `Database::document_fields_is_empty` (round 2) backs the empty-set shortcut.
    // (codex P2 round 12 on PR #291) The clear is paired with a pass token, so a watcher in
    // another process that writes a document while this pass runs can say *which* pass it
    // wrote under -- see `begin_declared_fields_pass` / `finish_declared_fields_pass`.
    let pass_token = if refresh_fields || force {
        Some(begin_declared_fields_pass(db)?)
    } else {
        None
    };
    let refresh_any = refresh_frontmatter || refresh_fields;

    // (ADR-0031) Every row's `(id, content_hash)`, read once before the walk, also under
    // `force`. Under the writer lock no other writer runs; against a writer that does not take
    // it, the prune below compares each candidate with this snapshot and keeps the rows that
    // writer changed in the meantime. The rename detection's `(path, hash)` map is built from it.
    let snapshot: PathIdHash = db.all_path_ids_hashes()?;

    // (feature-49) `.grooveignore` は **毎回ここで読み直す**。CLI `index` と MCP
    // `rebuild_index` は同じこの関数を通るので、どちらも常に今のファイルを見る。
    // 起動時に 1 度解決して焼き込むと、daemon 側だけ古い規則で走り続ける。
    let rules = crate::exclusion::ExclusionRules::load(&kb_path, exclude_dirs.to_vec());
    if let Some(patterns) = rules.ignore_file_patterns() {
        eprintln!(
            "Applying {} from {} ({patterns} pattern{})",
            crate::exclusion::IGNORE_FILE_NAME,
            kb_path.display(),
            if patterns == 1 { "" } else { "s" }
        );
    }

    // Registry の対応拡張子リストで source files を収集する。
    // 旧 collect_md_files は .md 固定だったが、.txt 等にも対応。
    let collected = collect_source_files_counted(&kb_path, &kb_path, registry, &rules)?;
    let source_files = collected.files;
    // (#660, spec 6) The run's list of the files it did not index, one reason per path. Only
    // `upsert` and `remove` change it, at the write points W0-W7; `complete_index_result` turns
    // it into `skipped_files`, `skipped` and `unspellable` once (F), for a completed run and a
    // stopped one alike.
    let mut skip_ledger = skip::SkipLedger::default();
    // (#660, spec 5.5) The walk counts each name it leaves out as it pushes it to `left_out`,
    // before the ledger folds equal paths, so the two counts agree.
    debug_assert_eq!(
        collected.unspellable as usize,
        collected
            .left_out
            .iter()
            .filter(|f| matches!(f.reason, SkipReason::UnspellableName { .. }))
            .count(),
        "the walk's count of names left out disagrees with its list"
    );
    // W0: what the walk left out (hard links, names the index cannot hold). Not guarded from
    // the prune (A8): a hard link's row goes, as before.
    for left in &collected.left_out {
        skip_ledger.upsert(&left.path, left.reason);
    }
    eprintln!(
        "Found {} source files (extensions: {:?})",
        source_files.len(),
        registry.extensions()
    );
    // (feature-60) What this run has done so far, in the shape it returns. Counted here as the
    // run goes and completed by `complete_index_result`, whether the run finishes or stops at
    // a check point, so the two returns cannot count differently.
    let mut tally = IndexResult::default();
    // (feature-60) The declared-field pass a stopped run closes, built once and handed to every
    // check point (C1-C4). Only shared borrows: `pass_token`, `stored_declared` and
    // `declared_json` are read, never changed, for the rest of the run.
    let pass_to_close = PassToClose {
        token: pass_token.as_deref(),
        observed_at_start: stored_declared.as_deref(),
        declared_json: &declared_json,
    };

    // 罠 H2: bar lifetime を rebuild_index 内に閉じる lazy init。
    // Backfilled / Found 行を出した後に bar を構築するので衝突しない。
    progress.start_indexing(source_files.len());

    // ファイル移動検出の前段階として、disk 側の全ファイルの
    // **hash だけ** を先に計算する。content は持ち回らない (evaluator 指摘
    // High #1: 大規模 KB の memory regression 回避)。embed/upsert 段階で
    // もう一度 `fs::read` する — ファイル OS キャッシュで 2 度目の
    // read は十分安く、代わりにピークメモリを `filecount * avg_size` から
    // `filecount * avg_path_len + 1 file worth of content` に圧縮できる。
    //
    // (feature-60) Every file the scan visits is reported, declined or not, and is check point
    // C1: a set token stops the scan right after the file it just reported.
    //
    // (local Codex on PR #388) The rows' (path, hash) are read before the scan, not after it,
    // so the scan can tell which files rename detection may pair -- a path the index does not
    // hold whose hash a row does -- and read their frontmatter topic from the bytes it hashes.
    // (ADR-0031) Under the writer lock no other writer runs between here and the renames
    // below; against a writer that does not take it, `rename_documents_checked` checks each
    // pair against the snapshot and skips the ones that writer changed. `force` detects no
    // renames.
    let db_path_hashes: std::collections::HashMap<String, String> = if force {
        std::collections::HashMap::new()
    } else {
        snapshot
            .iter()
            .map(|(path, (_, hash))| (path.clone(), hash.clone()))
            .collect()
    };
    let db_hashes: HashSet<&str> = db_path_hashes.values().map(String::as_str).collect();
    let rename_candidate =
        |rel: &str, hash: &str| !db_path_hashes.contains_key(rel) && db_hashes.contains(hash);
    let (scan, scan_flow) = scan_disk_entries_observed(
        &source_files,
        &kb_path,
        registry,
        registry.limits().binary.cap_bytes(),
        registry.limits().text.cap_bytes(),
        &rename_candidate,
        &mut || {
            progress.report_scanned();
            if progress.is_cancelled() {
                std::ops::ControlFlow::Break(())
            } else {
                std::ops::ControlFlow::Continue(())
            }
        },
    );
    let disk_entries = scan.entries;
    let scan_oversize = scan.oversize;
    let frontmatter_topics = scan.frontmatter_topics;

    // read 失敗 / size skip の rel path 集合。prune 判定 (§4.2 統一原則) で
    // visited_paths と union し、transient lock / size 成長での誤削除を防ぐ。
    // W1 (#660): each file the scan declined, with its reason; a path declined twice (a lossy
    // spelling shared by two files) is one entry.
    for (rel, why) in scan.skipped.iter().zip(&scan.skipped_why) {
        skip_ledger.upsert(rel, *why);
    }
    let skipped_paths: std::collections::HashSet<String> = scan.skipped.into_iter().collect();
    // (#660, spec 6 I3) Every path the prune guards is listed.
    debug_assert!(
        skipped_paths.iter().all(|rel| skip_ledger.contains(rel)),
        "a path the scan skipped is not in the skip ledger"
    );

    // (feature-60) Check points C1 and C2. C1: the observer above stopped the scan at a file.
    // C2: the token was set after the last file -- or, on an empty knowledge base, where there
    // is no file and so no C1, from the `Started { total: 0 }` callback. The scan wrote
    // nothing, and the renames below are the next write, so this is the last point where the
    // index reads as it did before the scan.
    if scan_flow.is_break() || progress.is_cancelled() {
        return stop_cancelled_run(
            db,
            embedder,
            progress,
            pass_to_close,
            tally,
            skip_ledger,
            None,
            start,
        );
    }

    // rename 検出 + atomically な rename 適用。
    // force=true のときは skip (embedding 全件再計算の意図)。
    // codex P2 (PR #73 F3): rename 先の new_path を集めておく。Static モードでは
    // rename された entry だけ下の loop で hash の fast path を飛ばす読み直し
    // (`Reindex::Renamed`、保存 vector は再利用する) にし、same-hash fast path
    // (index_single_disk_entry 冒頭の hash 一致 skip) を意図的に無効化する。
    // rename は path UPDATE のみで内容 (hash) は変わらないため、そのまま fast
    // path に乗ると、frontmatter title 無し文書で context 用 title が filename
    // stem 由来 (E-1) にもかかわらず再 parse されず、breadcrumb (chunk.context)
    // が旧 filename のまま stale 化する。Off モードは context を embed に使わない
    // ため無害 = 従来通り fast path を維持する。
    // (codex P2 round 5 on PR #291) **parser を跨ぐ rename は mode を問わず force する。**
    // `.txt` ↔ `.md` のような rename は bytes が同じでも読む parser が変わるので、
    // 旧 parser が書いた `document_fields` / title / tags がそのまま残ってしまう
    // (Off モードで fast path に乗ると一生直らない -- hash も世代 key も一致するため)。
    // `rename_crosses_a_parser` は watcher 側の `rename_single_file` とも共有する
    // 1 つの判定 (AGENTS "one question gets one implementation")。
    let mut renamed_new_paths: HashSet<String> = HashSet::new();
    // (codex P2 round 9 on PR #291) A narrower set than `renamed_new_paths`: only the renames
    // that actually crossed a parser, not every rename Static mode also forces a reparse for.
    // `settle_cross_parser_rename` below must fire only for the former -- a same-parser rename
    // forced by Static mode that happens to end in `Skipped` (e.g. the content shrank to
    // nothing) is a pre-existing edit-time gap this round does not touch, not a stale
    // cross-parser row.
    let mut crossed_parser_renames: HashSet<String> = HashSet::new();
    if !force {
        let pairs = detect_renames(&disk_entries, &db_path_hashes, &skipped_paths);
        let crossed_old_paths: Vec<&str> = pairs
            .iter()
            .filter(|(old_path, new_path)| rename_crosses_a_parser(registry, old_path, new_path))
            .map(|(old_path, _)| old_path.as_str())
            .collect();
        // (ADR-0031) One immediate transaction, in which each pair is re-read and applied only
        // while its old row still has the snapshot's `(id, content_hash)` and no row is at its
        // new path; a pair another writer changed is skipped, not an error. Empty `pairs` opens
        // no transaction.
        // (#552) A pair that crosses a parser is re-read under the new parser below, and until
        // that read settles it the row holds the old parser's chunks under the hash of the
        // unchanged bytes. So an applied crossed pair's hash is overwritten with
        // [`HASH_AWAITING_REPARSE`] in the same transaction, and a run that fails anywhere
        // after the move leaves a row no fast path keeps; every settlement below rewrites the
        // hash (the bytes' hash, or the sentinel again for a read that changed under it) or
        // drops the row.
        // (local Codex on PR #388) Every pair's new path was a rename candidate in the scan, so
        // its topic comes from the bytes the pair was matched on.
        let outcome: crate::db::CheckedRenames = db.rename_documents_checked(
            &pairs,
            &snapshot,
            &crossed_old_paths,
            &frontmatter_topics,
        )?;
        // Only the applied pairs force a re-read or a crossed-rename settlement: a skipped
        // pair's new path holds another writer's row (or none), which the loop indexes as an
        // ordinary file; settling it as a crossed rename would delete that row.
        for (old_path, new_path) in &outcome.applied {
            let crossed = rename_crosses_a_parser(registry, old_path, new_path);
            if context_mode == ContextMode::Static || crossed {
                renamed_new_paths.insert(new_path.clone());
            }
            if crossed {
                crossed_parser_renames.insert(new_path.clone());
            }
            progress.report_renamed(old_path, new_path);
        }
        tally.renamed = outcome.applied.len() as u32;
        tally.renames_skipped = outcome.skipped.len() as u32;
    }
    // Ahead of check point C3, so a run stopped in the loop still says it.
    if let Some(note) = renames_skipped_note(tally.renames_skipped) {
        eprintln!("{note}");
    }

    // (feature-51) `documents.size_bytes` の欠損補充。`backfill_fts` と同じ思想で、embedding には触れない。
    //
    // **この 1 行が無いと新列は事実上埋まらない**: 下の loop は content hash が
    // 一致する文書を `SingleResult::Unchanged` で返し、書き込み経路
    // (`upsert_document` / `update_document_meta`) をどちらも通らないので、
    // 既存 KB の大多数は列が追加されたことに気付かないまま NULL で残る。
    // ここは走査で size が分かっている全 entry を対象にし、UPDATE 側の
    // `WHERE size_bytes IS NULL` が記録済みの行を守る。
    //
    // **rename 適用の後**に走らせる (codex P2 round 1)。走査 entry の key は
    // *新しい* path で、rename 前の DB 行はまだ古い path を持っている。先に
    // 走らせると、移行と同じ run で rename された文書だけ 1 行も一致せず、
    // その後 same-hash fast path が document 行を書かないので size は NULL の
    // まま次の full index まで残る (= その間 oversized なら提示され続ける)。
    // **走査したパスは 1 本の規則で書く** (codex P2 round 5)。当初は
    // 「NULL のときだけ埋める」だったが、それだと *古い記録* を直せない。
    // round 4 で「cap を超えたファイルは上書き」を足したところ、**逆向きが
    // 抜けた**: cap 超えだったファイルが元の内容に戻されると、scan は小さい
    // size を測るのに hash 一致で `Unchanged` になり書き手が居ないので、
    // 「read は受け付けるのに提示されない」が永久に残る。
    //
    // 直し方は分岐を増やすことではなく減らすこと: **scan が測ったものが真**。
    // 索引された文書は下の loop がパース済みバイトの長さで上書きするので、
    // ここが古い値を残すことはない。
    let scanned_sizes: Vec<(&str, u64)> = disk_entries
        .iter()
        .map(|e| (e.rel.as_str(), e.size))
        .chain(scan_oversize.iter().map(|(rel, len)| (rel.as_str(), *len)))
        .collect();
    match db.record_document_sizes(&scanned_sizes) {
        Ok(0) => {}
        Ok(n) => tracing::info!("recorded the current size of {n} document(s)"),
        // 記録に失敗しても index 自体は続ける。ずれた分は次回の index か
        // `groove doctor` が拾う。
        Err(e) => tracing::warn!("failed to record document sizes: {e}"),
    }

    // (feature-62) Which unchanged `.docx` documents the one-time pass re-reads: decided once,
    // after the renames above, since a renamed row is under its new path now.
    // (feature-65) And which unchanged `.pptx` ones, counted apart for their own notice.
    let mut rereads: HashSet<String> = HashSet::new();
    let (mut docx_rereads, mut pptx_rereads) = (0_usize, 0_usize);
    for entry in &disk_entries {
        if rereads_unchanged_docx(refresh_docx, db, registry, entry, &renamed_new_paths)? {
            rereads.insert(entry.rel.clone());
            docx_rereads += 1;
        } else if rereads_unchanged_as(
            "pptx",
            refresh_pptx,
            db,
            registry,
            entry,
            &renamed_new_paths,
        )? {
            rereads.insert(entry.rel.clone());
            pptx_rereads += 1;
        }
    }
    // Announced before the loop, so a run that takes long re-embedding says why even if it is
    // stopped: the `.docx` line first, then the `.pptx` one.
    if docx_rereads > 0 {
        let notice = if rows_docx_alone_stale {
            docx_rows_reread_notice(docx_rereads)
        } else if tables_alone_stale {
            docx_table_reread_notice(docx_rereads)
        } else {
            docx_reread_notice(docx_rereads)
        };
        progress.announce(&notice);
    }
    if pptx_rereads > 0 {
        let notice = if rows_pptx_alone_stale {
            pptx_rows_reread_notice(pptx_rereads)
        } else {
            pptx_reread_notice(pptx_rereads)
        };
        progress.announce(&notice);
    }

    // (AW-04) Files the endpoint accepted at least one batch of this run, measured on the
    // embedder rather than counted from `Updated`, which a metadata-only update also
    // returns. MCP holds the embedder's lock for the whole run (`server.rs`), so no
    // watcher embed falls in between.
    let embedded_since = EmbeddedBaseline::read(embedder);

    // Track paths we visit so we can detect deletions later.
    let mut visited_paths: HashSet<String> = HashSet::new();
    let mut refreshed: u32 = 0;
    let mut refreshed_fields: u32 = 0;
    // (#251, codex P2 round 2) The one-time check is recorded as done only when every
    // Markdown document it was meant to read was read. A legacy file the scan could not
    // open, or that this loop skips or refuses, keeps the check pending: its row is
    // retained, so when the file is back with the same content the fast path would
    // otherwise hide it for good.
    // (#544, local Codex r2) Held as the scan's part and the loop's rows apart, so a row the
    // further read of a crossed rename settles below stops holding the check pending; read as
    // one value only at the end of the run.
    let mut refresh_pending_scan = false;
    if refresh_any {
        for rel in &skipped_paths {
            if indexed_hash_for(db, registry, rel, "md")?.is_some() {
                refresh_pending_scan = true;
                break;
            }
        }
    }
    let mut refresh_pending_rows: HashSet<String> = HashSet::new();
    // (feature-62) The rows of a one-time pass (ADR-0029) that this run did not bring in line
    // with the new rules. Marked at the end, after the sweep -- never inside the loop, so a run
    // that stops midway leaves every hash as it was (I-4).
    // (feature-65) Rows of any stale pass ([`stale_row`]), not only `.docx` ones.
    let mut unsettled: HashSet<String> = HashSet::new();
    // (#544, local Codex r1) The crossed renames the loop kept under [`HASH_AWAITING_REPARSE`],
    // in loop order, for the further read after it.
    let mut crossed_awaiting: Vec<&DiskEntry> = Vec::new();

    // 2. Process each file
    //
    // (feature-60) The re-reads of renames (past the hash fast path, [`Reindex::Renamed`])
    // first, then the rest; see `processing_order`.
    for entry in processing_order(&disk_entries, &renamed_new_paths) {
        // (feature-60) Check point C3, before this document's embed starts. Not for the re-read
        // of a rename ([`Reindex::Renamed`]): the rename is already applied and the hash still matches,
        // so a run stopped here would leave the next one to take the fast path and keep the
        // old parser's title, tags and fields (or the old breadcrumb) until `--force`. Those
        // documents come first, so this holds a cancel back by at most
        // `renamed_new_paths.len()` documents.
        if !renamed_new_paths.contains(&entry.rel) && progress.is_cancelled() {
            return stop_cancelled_run(
                db,
                embedder,
                progress,
                pass_to_close,
                tally,
                skip_ledger,
                Some(embedded_since),
                start,
            );
        }
        visited_paths.insert(entry.rel.clone());

        // rename された entry (Static モード、または parser を跨ぐもの) は hash fast path を
        // 飛ばして再 parse させ、他の unchanged file の hash fast path はそのまま活かす。
        // (kuriya 608) `Force` ではなく `Renamed`: 保存 vector を読み、入力が変わらなかった
        // chunk はそれを使う (title のある文書なら全 chunk が再利用され、endpoint へは何も送らない)。
        let mode = if force {
            Reindex::Force
        } else if renamed_new_paths.contains(&entry.rel) {
            Reindex::Renamed
        } else if rereads.contains(&entry.rel) {
            Reindex::Reparse
        } else {
            Reindex::Incremental {
                check_frontmatter: refresh_frontmatter,
                refresh_fields,
            }
        };

        // (AW-04) Read before the call, so a refusal below can tell whether the endpoint
        // accepted part of this file first.
        let refused_after_accepting_before = embedder.documents_refused_after_accepting();
        let crossed = crossed_parser_renames.contains(&entry.rel);
        // (feature-69, kuriya 621) `counts` says how many of an updated document's chunks were
        // embedded and how many reused, from the document's own plan.
        let CountedReindex {
            reindexed:
                Reindexed {
                    result: single_result,
                    reason: skip_reason,
                },
            counts,
        } = counted(|counts| {
            index_single_disk_entry_explained(
                db,
                embedder,
                entry,
                exclude_headings,
                registry,
                mode,
                context_mode,
                DeclaredSet::Known {
                    list: &declared_fields,
                    pass: pass_token.as_deref(),
                    generation: &declared_json,
                },
                crossed,
                counts,
            )
        })?;
        // (codex P2 round 9 on PR #291) Before the match below decides what this entry counts
        // as: if this was a rename re-read across a parser boundary and it did not end in
        // `Updated`, the row is stale under the new path and must go -- unless the second read
        // changed under it, which keeps the row awaiting a reparse (#544). See
        // `settle_cross_parser_rename`'s doc for why.
        if crossed {
            settle_cross_parser_rename(db, &entry.rel, &single_result)?;
            // (#544, local Codex r1) Kept awaiting: read once more after the loop.
            if matches!(
                single_result,
                SingleResult::Skipped {
                    reason: SKIPPED_CHANGED_DURING_READ,
                    ..
                }
            ) {
                crossed_awaiting.push(entry);
            }
        }
        // (feature-62) Settled: written by this parser, re-read and found to match, or dropped
        // by the re-read. Anything else -- a parse failure, a refusal, bytes that changed under
        // the read, a skip on the ordinary path -- leaves a row the old rule wrote, which the end
        // of the run marks. A row already gone (a cross-parser rename settled above) needs none;
        // one the settlement kept awaiting a reparse (#544) is re-read whatever it is marked.
        if any_stale {
            let settled = match &single_result {
                SingleResult::Updated { .. } => true,
                SingleResult::Unchanged => mode == Reindex::Reparse,
                SingleResult::Skipped { reason, .. } => {
                    mode == Reindex::Reparse && *reason == SKIPPED_NO_CHUNKS
                }
                SingleResult::MetadataRefreshed { .. }
                | SingleResult::Refused
                | SingleResult::OutsideKnowledgeBaseDeindexed
                | SingleResult::NothingToIndex { .. } => false,
            };
            if !settled && stale_row(&passes, db, registry, &entry.rel)? {
                unsettled.insert(entry.rel.clone());
            }
        }
        match single_result {
            SingleResult::Updated {
                chunks,
                frontmatter_unparsed: fm_unparsed,
            } => {
                tally.updated += 1;
                if fm_unparsed {
                    tally.frontmatter_unparsed += 1;
                }
                tally.chunks_embedded = tally.chunks_embedded.saturating_add(counts.embedded);
                tally.chunks_reused = tally.chunks_reused.saturating_add(counts.reused);
                progress.report_indexed(&entry.rel, chunks);
            }
            // A skip that still failed to parse its frontmatter was named on
            // stderr, so it is counted like an indexed one (codex P2, round 1).
            SingleResult::Skipped {
                reason,
                frontmatter_unparsed: fm_unparsed,
            } => {
                // W2 (#660).
                list_skip(&mut skip_ledger, &entry.rel, skip_reason);
                // A refusal that came after an accepted batch of the same file proves the
                // endpoint works, so it is not counted toward "every input rejected".
                if reason == SKIPPED_EMBED_REJECTED
                    && embedder.documents_refused_after_accepting()
                        == refused_after_accepting_before
                {
                    tally.embed_rejected += 1;
                }
                // (codex P1 on PR #329) In a forced rebuild any refusal counts: the reset
                // emptied the index, so no previous row stands in for the file.
                if force && reason == SKIPPED_EMBED_REJECTED {
                    tally.forced_rejected += 1;
                }
                if fm_unparsed {
                    tally.frontmatter_unparsed += 1;
                }
                // What the check has to have read is the bytes the *row* was written
                // from, and a skip retains that row. A skip decided before parsing read
                // nothing; one decided after parsing (no chunks) read the file on disk,
                // which is the row's bytes only when the hashes agree. Otherwise the old
                // bytes can come back, match the retained hash, and hide behind the fast
                // path for good (codex P2, rounds 3 and 4).
                if refresh_any
                    && let Some(row_hash) = indexed_hash_for(db, registry, &entry.rel, "md")?
                    && (reason != SKIPPED_NO_CHUNKS || row_hash != entry.hash)
                {
                    refresh_pending_rows.insert(entry.rel.clone());
                }
                progress.report_unchanged(&entry.rel);
            }
            // A refusal counts as a skip here for the same reason a size cap
            // does: the file is not indexed and the reason is already on stderr.
            // `OutsideKnowledgeBaseDeindexed` comes only from `reindex_single_file`, never from
            // the shared entry this loop calls; named so a new variant stays a compile error.
            // (#543) So does `NothingToIndex` ([`Reindex::Event`] is not a mode this loop uses).
            SingleResult::Refused
            | SingleResult::OutsideKnowledgeBaseDeindexed
            | SingleResult::NothingToIndex { .. } => {
                // W3 (#660).
                list_skip(&mut skip_ledger, &entry.rel, skip_reason);
                if refresh_any && indexed_hash_for(db, registry, &entry.rel, "md")?.is_some() {
                    refresh_pending_rows.insert(entry.rel.clone());
                }
                progress.report_unchanged(&entry.rel);
            }
            SingleResult::MetadataRefreshed {
                frontmatter_unparsed: fm_unparsed,
            } => {
                if fm_unparsed {
                    tally.frontmatter_unparsed += 1;
                    refreshed += 1;
                }
                if refresh_fields {
                    refreshed_fields += 1;
                }
                progress.report_unchanged(&entry.rel);
            }
            SingleResult::Unchanged => {
                // Progress mode (= Tty/NonTty) で bar / counter を tick する。
                // Verbose / Quiet は no-op (= 既存挙動保持)。
                progress.report_unchanged(&entry.rel);
            }
        }
    }

    // (#544, local Codex r1) A crossed rename the loop kept awaiting a reparse holds the old
    // parser's chunks: it gets one further read of the bytes on disk before the run returns
    // ([`read_crossed_rename_again_explained`]), whose result alone decides the row and replaces
    // the skip the loop counted for it. Ahead of C4, so a cancel stops it between two reads: a
    // row not read again keeps the sentinel, which no fast path trusts. Not reported to
    // `progress`, which already counted the file.
    // (#660, spec 7 I-reread) W4 and W5-W8 run one after the other within an iteration, with no
    // cancel check and no early return but `?` between them, so at every break each crossed
    // path is listed either with the loop's `ChangedDuringRead` or with this read's outcome.
    for entry in crossed_awaiting {
        if progress.is_cancelled() {
            break;
        }
        let refused_after_accepting_before = embedder.documents_refused_after_accepting();
        let CountedReindex {
            reindexed:
                Reindexed {
                    result: again,
                    reason: again_reason,
                },
            counts,
        } = counted(|counts| {
            read_crossed_rename_again_explained(
                db,
                embedder,
                entry,
                exclude_headings,
                registry,
                context_mode,
                DeclaredSet::Known {
                    list: &declared_fields,
                    pass: pass_token.as_deref(),
                    generation: &declared_json,
                },
                counts,
            )
        })?;
        // (#544, local Codex r3, J3) Settled as the loop's read was: bytes this read verified
        // and the new parser cannot index drop the row; only bytes that changed again keep it
        // awaiting. A dropped row holds nothing open. (local Codex r4, D5) A read that failed
        // checked no bytes and keeps the row awaiting, unsettled and pending.
        if settle_further_crossed_read(db, &entry.rel, &again)? {
            unsettled.remove(&entry.rel);
            refresh_pending_rows.remove(&entry.rel);
        }
        // W4 (#660): this read's outcome replaces the loop's `ChangedDuringRead` -- unless the
        // scan also skipped this path (a lossy spelling shared with another file), which keeps
        // it guarded and so listed (I3).
        if !skipped_paths.contains(&entry.rel) {
            skip_ledger.remove(&entry.rel);
        }
        match again {
            SingleResult::Updated {
                frontmatter_unparsed: fm_unparsed,
                ..
            } => {
                unsettled.remove(&entry.rel);
                // (#544, local Codex r2) Written by this parser from the bytes on disk: the
                // one-time check has read what the row is now.
                refresh_pending_rows.remove(&entry.rel);
                tally.updated += 1;
                if fm_unparsed {
                    tally.frontmatter_unparsed += 1;
                }
                tally.chunks_embedded = tally.chunks_embedded.saturating_add(counts.embedded);
                tally.chunks_reused = tally.chunks_reused.saturating_add(counts.reused);
            }
            SingleResult::Skipped {
                reason,
                frontmatter_unparsed: fm_unparsed,
            } => {
                // W5 (#660).
                list_skip(&mut skip_ledger, &entry.rel, again_reason);
                if reason == SKIPPED_EMBED_REJECTED
                    && embedder.documents_refused_after_accepting()
                        == refused_after_accepting_before
                {
                    tally.embed_rejected += 1;
                }
                if fm_unparsed {
                    tally.frontmatter_unparsed += 1;
                }
            }
            SingleResult::NothingToIndex {
                frontmatter_unparsed: fm_unparsed,
                ..
            } => {
                // Dropped, by the read and by the settlement above. W6 (#660).
                list_skip(&mut skip_ledger, &entry.rel, again_reason);
                if fm_unparsed {
                    tally.frontmatter_unparsed += 1;
                }
            }
            SingleResult::Refused | SingleResult::OutsideKnowledgeBaseDeindexed => {
                // W7 (#660).
                list_skip(&mut skip_ledger, &entry.rel, again_reason);
            }
            // Neither comes back for a row under the sentinel: no hash matches it, and
            // `Reindex::Event` runs no one-time check. Named so a new variant is a compile error.
            // W8 (#660): nothing to list; W4 took the loop's entry off.
            SingleResult::Unchanged | SingleResult::MetadataRefreshed { .. } => {}
        }
    }

    // (feature-60) Check point C4: every document is done; the summary lines, the deletion
    // sweep and the generation keys are not. C3 looks before a document, so a token set from
    // the callback of the last document's event is seen only here. Past this point a cancel
    // is too late: the sweep and the keys run to the end and the run reports `Finished`.
    if progress.is_cancelled() {
        return stop_cancelled_run(
            db,
            embedder,
            progress,
            pass_to_close,
            tally,
            skip_ledger,
            Some(embedded_since),
            start,
        );
    }

    if refreshed > 0 {
        eprintln!(
            "Tagged {refreshed} unchanged Markdown document(s) whose YAML frontmatter had failed to parse \
             before this version recorded it (one-time check; nothing was re-embedded)"
        );
    }
    if refreshed_fields > 0 {
        eprintln!(
            "Recorded the declared frontmatter fields of {refreshed_fields} unchanged Markdown document(s) \
             (the schema's declared keys changed; nothing was re-embedded)"
        );
    }

    // 3. Delete documents in DB that no longer exist on disk.
    //    §4.2 統一原則: visited ∪ skipped は保持、それ以外 (= disk から消えた) のみ削除。
    //    (ADR-0031) The candidates are the snapshot's paths, sorted as `ORDER BY path` sorts
    //    them, so the batch boundaries are the same from run to run. A row written after the
    //    snapshot is not a candidate, and each batch deletes only the rows whose `(id, hash)`
    //    still match it; a deleted row is counted after its batch commits.
    let mut snapshot_paths: Vec<String> = snapshot.keys().cloned().collect();
    snapshot_paths.sort_unstable();
    // (#660, spec 6 I3) Again before the prune reads the guard: W4 changed the ledger since W1.
    debug_assert!(
        skipped_paths.iter().all(|rel| skip_ledger.contains(rel)),
        "a path the prune guards is not in the skip ledger"
    );
    let candidates = documents_to_delete(&snapshot_paths, &visited_paths, &skipped_paths);
    // (ADR-0031) Under `--force` each batch marks the rows it keeps in the transaction that
    // keeps them: the code chunk policy this forced run recorded before its walk claims every
    // row, and a kept row is one it never read, so no later failure, a later batch's included,
    // may leave it unmarked. A run without `--force` marks none here; one that fails before its
    // end writes no key that would claim the kept rows either.
    let pruned = prune_candidates(
        db,
        &snapshot,
        &candidates,
        PRUNE_BATCH,
        force,
        &mut |path| {
            tally.deleted += 1;
            progress.report_deleted(path);
        },
    )?;
    tally.kept_changed = pruned.kept_changed;
    // (ADR-0031) The rows the prune kept for another writer were not read by this run, so
    // nothing below that records a run-wide "settled" may count them: the ones a key written at
    // the end would claim are marked for a re-parse there, and the frontmatter policy and
    // declared-field set wait for a run that keeps none.
    let kept = pruned.kept;
    // Not through `progress`: a quiet or callback reporter would drop the count.
    if let Some(note) = kept_changed_note(tally.kept_changed) {
        eprintln!("{note}");
    }

    // (#251, codex P2 round 6 on PR #291) Every unchanged Markdown document has now been
    // looked at under this policy, so the next run can take the fast path again. Written
    // after the deletion sweep above, not merely after the loop: a Markdown file the schema
    // change is meant to cover can be absent from disk during this run (renamed away,
    // temporarily moved) and so is never visited by the loop at all -- only the sweep decides
    // its row is gone. A run that stops between the loop and the sweep must not claim the pass
    // completed, or a same-hash file restored later would take the fast path over a row the
    // sweep never got to prune. Written after the loop on purpose either way: a run that stops
    // halfway leaves the key absent and the next run looks again -- and so does one that could
    // not read a document it holds a row for.
    // (#544, local Codex r2) Read from the rows as they are after the further read of the
    // crossed renames, not as the loop left them.
    let refresh_pending = refresh_pending_scan || !refresh_pending_rows.is_empty();
    // (ADR-0031) Not over a row the prune kept, `--force` included: this build never read it.
    if kept.is_empty() && (force || (refresh_frontmatter && !refresh_pending)) {
        db.write_frontmatter_policy(FRONTMATTER_POLICY)?;
    }
    // (feature-58, codex P2 round 6 on PR #291) Recorded after the deletion sweep for the same
    // reason as the policy above, not only after the loop: a run that stopped halfway must not
    // claim the new set, and an orphaned document the loop never visited is only accounted for
    // once the sweep prunes it. Before round 3 (codex P2 on PR #291) stopping halfway meant
    // leaving the old value in place; now the key was already cleared, ahead of the loop,
    // whenever a refresh (or `force`) was going to touch a row -- see the `clear_declared_fields`
    // call above -- so a run that stopped halfway, whether inside the loop or between the loop and
    // the sweep, leaves it absent and the next run refreshes again rather than trusting a set
    // some documents (visited or pruned) were never brought in line with. Rewriting an unchanged
    // value is harmless, and it is how an index with nothing to declare gets its `[]` without a
    // pass. (codex P2 round 12) Recorded through `finish_declared_fields_pass`, which leaves
    // the generation pending instead when a watcher in another process wrote a document
    // under this pass's token -- that document's rows are the previous generation's, and
    // recording the new set over them would make them current for good. (ADR-0031) Nor over a
    // row the prune kept, whose fields another writer may have written under any set; that
    // includes `--force` and the empty-set shortcut. The next run records it.
    finish_declared_fields_pass(
        db,
        pass_token.as_deref(),
        stored_declared.as_deref(),
        &declared_json,
        kept.is_empty() && (force || !refresh_fields || !refresh_pending),
    )?;

    let result = complete_index_result(
        db,
        embedder,
        tally,
        skip_ledger,
        Some(embedded_since),
        start,
        false,
    )?;
    // (feature-62) Recorded last -- after the deletion sweep, for the reason the frontmatter
    // key is, and after every other step of the run that can fail, so a run that returns an
    // `Err` or stops at a check point never records it (I-4) -- together with the placeholder
    // over every row of a one-time pass (ADR-0029) this run could not settle: the ones the loop
    // marked, and the ones the scan skipped, which the loop never saw. One transaction, so the
    // key is never there without the marks (I-1). The marks are written over whatever the
    // rows hold now, unconditionally: a row another process wrote since the loop is marked too,
    // and re-read once more, rather than an old-rule row being kept (ADR-0028). `--force`
    // rewrote every row it read, so it marks none of those. The counts in `result` read rows,
    // not hashes, so the marks do not change them. (ADR-0031) The exception is a row the prune
    // kept for another writer, which this run did not read: it is marked as a skipped one is,
    // when a stale pass covers it, and under `--force`, whose keys claim every row, whatever its
    // format (a forced run already marked it in the prune batch that kept it; this writes it
    // again). The mark costs it no more than one more read, and a kept row deleted since is
    // skipped by the write.
    if force || any_stale {
        if any_stale {
            for rel in &skipped_paths {
                if stale_row(&passes, db, registry, rel)? {
                    unsettled.insert(rel.clone());
                }
            }
        }
        for rel in &kept {
            if force || stale_row(&passes, db, registry, rel)? {
                unsettled.insert(rel.clone());
            }
        }
        let marked: Vec<&str> = unsettled.iter().map(String::as_str).collect();
        let tx = db.begin_transaction()?;
        db.overwrite_content_hash(&marked, HASH_AWAITING_REPARSE)?;
        if force || refresh_docx {
            db.write_docx_heading_policy(DOCX_HEADING_POLICY)?;
            // (feature-63) After the heading key, in the same transaction: a run that fails on
            // either write records neither.
            db.write_docx_section_policy(DOCX_SECTION_POLICY)?;
            // (feature-64) After them, in the same transaction: a run that fails on an earlier
            // write, or on this one, records none of the `.docx` keys.
            db.write_docx_table_policy(DOCX_TABLE_POLICY)?;
            // (feature-66) After the table key, in the same transaction: a run that fails on an
            // earlier `.docx` key never reaches it, and one that fails on it records none.
            db.write_docx_table_rows_policy(DOCX_TABLE_ROWS_POLICY)?;
        }
        if force || refresh_pptx {
            // (feature-65) After the `.docx` keys, in the same transaction: a run that fails on
            // a `.docx` key or on this one records no key and no mark (ADR-0029).
            db.write_pptx_table_break_policy(PPTX_TABLE_BREAK_POLICY)?;
            // (feature-66) Last, in the same transaction (ADR-0030).
            db.write_pptx_table_rows_policy(PPTX_TABLE_ROWS_POLICY)?;
        }
        tx.commit()?;
    }
    progress.finish();
    Ok(result)
}

/// (feature-60) The two embedder counters a run's [`IndexResult::embedded`] is measured
/// against, read once before the per-document loop. Before that point nothing has been
/// embedded, which is why a run stopped earlier passes `None` to [`complete_index_result`].
#[derive(Clone, Copy)]
struct EmbeddedBaseline {
    embedded: u64,
    refused_after_accepting: u64,
}

impl EmbeddedBaseline {
    fn read(embedder: &Embedder) -> Self {
        Self {
            embedded: embedder.documents_embedded(),
            refused_after_accepting: embedder.documents_refused_after_accepting(),
        }
    }

    /// (AW-04) Files the endpoint accepted at least one batch of since `self` was read:
    /// every file embedded whole, plus every file refused only after an earlier batch of it
    /// was accepted.
    fn accepted_since(self, embedder: &Embedder) -> u32 {
        let accepted = (embedder.documents_embedded() - self.embedded)
            + (embedder.documents_refused_after_accepting() - self.refused_after_accepting);
        u32::try_from(accepted).unwrap_or(u32::MAX)
    }
}

/// (feature-60) Fill in what a run measures rather than counts -- the files the endpoint
/// accepted, the index's size, the time taken -- and whether it stopped early. The one place
/// an [`IndexResult`] is completed, for a run that finished and for one that stopped at a
/// check point alike (AGENTS.md "One question gets one implementation"). `embedded_since` is
/// `None` for a run stopped before the per-document loop, which embedded nothing.
///
/// (#660, spec 6 F) Also the one place [`IndexResult::skipped_files`], [`IndexResult::skipped`]
/// and [`IndexResult::unspellable`] are written: all three from `ledger`, the run's list of the
/// files it did not index, so the count and the list cannot disagree (I1, I7).
fn complete_index_result(
    db: &Database,
    embedder: &Embedder,
    mut tally: IndexResult,
    ledger: skip::SkipLedger,
    embedded_since: Option<EmbeddedBaseline>,
    start: Instant,
    cancelled: bool,
) -> Result<IndexResult> {
    let skipped_files = ledger.finish();
    tally.skipped = u32::try_from(skipped_files.len()).unwrap_or(u32::MAX);
    tally.unspellable = u32::try_from(
        skipped_files
            .iter()
            .filter(|f| matches!(f.reason, SkipReason::UnspellableName { .. }))
            .count(),
    )
    .unwrap_or(u32::MAX);
    tally.skipped_files = skipped_files;
    tally.embedded = embedded_since.map_or(0, |baseline| baseline.accepted_since(embedder));
    // Count total documents remaining (includes unchanged ones)
    tally.total_documents = db.document_count()?;
    // Count total chunks in DB (includes unchanged ones)
    tally.total_chunks = db.chunk_count()?;
    tally.duration_ms = start.elapsed().as_millis() as u64;
    tally.cancelled = cancelled;
    Ok(tally)
}

/// (feature-60) The three values [`finish_declared_fields_pass`] takes about this run's
/// declared-field pass, handed to [`stop_cancelled_run`] together. Built once in
/// [`rebuild_index`] and copied to each check point, so every stop closes the same pass.
#[derive(Clone, Copy)]
struct PassToClose<'a> {
    token: Option<&'a str>,
    observed_at_start: Option<&'a str>,
    declared_json: &'a str,
}

/// (feature-60) End a run that found its cancel token set at a check point: no deletion
/// sweep, no generation key, no summary line. If the run opened a declared-field pass, it is
/// closed by [`finish_declared_fields_pass`] with recording turned off -- this pass's
/// token and dirty mark are cleared and no
/// generation is written, or, if another run took the pass over, nothing is touched -- so a
/// stopped run never leaves its own token behind (without this, a stopped `force` over a
/// schema-less knowledge base would leave a token no later incremental run clears). That call
/// may log one of its existing warnings, which stay true after the stop; an `Err` from it is
/// returned as is, like the completed path's `?`, and then no terminal event is emitted. The
/// result is completed by [`complete_index_result`] before
/// [`progress::ProgressReporter::finish_cancelled`] emits
/// [`progress::ProgressEvent::Cancelled`].
#[allow(clippy::too_many_arguments)]
fn stop_cancelled_run(
    db: &Database,
    embedder: &Embedder,
    progress: progress::ProgressReporter,
    pass: PassToClose<'_>,
    tally: IndexResult,
    ledger: skip::SkipLedger,
    embedded_since: Option<EmbeddedBaseline>,
    start: Instant,
) -> Result<IndexResult> {
    if pass.token.is_some() {
        finish_declared_fields_pass(
            db,
            pass.token,
            pass.observed_at_start,
            pass.declared_json,
            false,
        )?;
    }
    let result = complete_index_result(db, embedder, tally, ledger, embedded_since, start, true)?;
    progress.finish_cancelled();
    Ok(result)
}

/// (#660, spec 6 W2/W3/W5-W7) List `rel` with the reason the indexing body gave a result the
/// run counts as skipped. That body names one for every such result (I4); a `None` here is a
/// return point that forgot to, caught in a debug build and otherwise left off the list (the
/// count follows the list, so the result stays consistent) with a warning naming the file.
fn list_skip(ledger: &mut skip::SkipLedger, rel: &str, reason: Option<SkipReason>) {
    debug_assert!(
        reason.is_some(),
        "{rel}: a result counted as skipped came back with no reason (I4)"
    );
    if let Some(reason) = reason {
        ledger.upsert(rel, reason);
    } else {
        tracing::warn!("{rel} was skipped with no reason given; it is left off the skipped list");
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// The one [`SingleResult::Skipped`] reason that is decided *after* the file was parsed and
/// counts as having read it. The one-time frontmatter check (#251) treats every other skip
/// as "not read" -- the ones that return before the parser runs, and
/// [`SKIPPED_EMBED_REJECTED`], which parsed the file but wrote nothing from it; this one
/// counts as read only when the bytes parsed are the bytes the retained row holds.
///
/// (feature-62) Under [`Reindex::Reparse`] it also means the row is gone: the document the old
/// heading rule split has nothing to index under the new one, and [`index_single_disk_entry`]
/// drops the row, the way `--force` would leave none.
///
/// (#543) The watcher names a [`SingleResult::NothingToIndex`] with it as well, so the line for
/// a file with nothing to embed reads the same whichever result came back.
pub(crate) const SKIPPED_NO_CHUNKS: &str = "no embeddable chunks";

/// The [`SingleResult::Skipped`] reason for a file whose input the embedding endpoint
/// refused ([`crate::embedder::EmbedInputRejected`], HTTP 400 / 413 / 422, AW-04). The
/// file is skipped on its own and the run goes on; the row it had stays.
const SKIPPED_EMBED_REJECTED: &str = "embedding endpoint rejected the input";

/// The [`SingleResult::Skipped`] reason for a file whose read failed (an I/O error: sharing,
/// permission, a file gone between the look and the read). No bytes were read, so nothing was
/// learned about the file. (#544, local Codex r4) [`settle_further_crossed_read`] keeps the
/// row on it.
const SKIPPED_READ_FAILED: &str = "read failed";

/// The [`SingleResult::Skipped`] reason for bytes the parser rejected. Returned after the
/// bytes were read and, for a caller that asked for it (`row_dropped_unless_updated` on
/// [`index_single_disk_entry`]), checked against the read's hash.
const SKIPPED_PARSE_FAILED: &str = "parse failed";

/// The [`SingleResult::Skipped`] reason [`reindex_single_file_checked`] gives for a path whose
/// look found no file there: the destination is established gone (#544 D1).
const SKIPPED_FILE_GONE: &str = "file no longer exists";

/// The [`SingleResult::Skipped`] reason [`reindex_single_file_checked`] gives for a file whose
/// measured size is over the cap, before it reads it.
const SKIPPED_TOO_LARGE: &str = "file too large";

/// The [`SingleResult::Skipped`] reason the one-time frontmatter check (#251) returns when
/// the bytes it read are not the bytes the scan hashed: the file was swapped between the
/// two reads, so nothing about the retained row has been learned, and the skip keeps the
/// check pending for the next run (codex P2, round 5).
///
/// (feature-62) The one-time pass (ADR-0029) returns it too, and there it means something else:
/// for a [`Reindex::Reparse`] entry the skip leaves the row unsettled, so the end-of-run step
/// gives it [`HASH_AWAITING_REPARSE`].
///
/// (#544, D5) For the destination of a rename that [`rename_crosses_a_parser`],
/// [`index_single_disk_entry`] returns it before parsing when the second read is not the bytes
/// the scan (or the rename's first read) hashed, and [`settle_cross_parser_rename`] then keeps
/// the row rather than dropping it, writing [`HASH_AWAITING_REPARSE`] over its hash.
const SKIPPED_CHANGED_DURING_READ: &str = "changed between scan and read";

// ---------------------------------------------------------------------------
// 増分 index API (watcher から呼ぶ)
// ---------------------------------------------------------------------------

/// Collect all files under `kb_path` whose extension is registered in
/// `registry`. Anything `rules` excludes is skipped — a directory along with
/// its whole subtree. Sort for deterministic ordering.
///
/// (feature-49) The decision moved into [`crate::exclusion::ExclusionRules`] so
/// that this walk, the `validate` walk and the live watcher cannot answer it
/// differently; the three had already drifted twice (AU-03, BU-19). The visible
/// change here is that **files are filtered too**: `exclude_dirs` only ever
/// named directories, but `.grooveignore` can name a file, so `filter_entry` no
/// longer waves every non-directory through.
/// `pub(crate)` only so `watcher.rs` can put this walk and `should_process` side
/// by side in one test and assert they answer the same way. Keeping them in
/// separate test modules is how they drifted apart in the first place.
///
/// Test-only since [`rebuild_index`] takes [`collect_source_files_counted`]
/// for the count it reports (ADR-0023).
#[cfg(test)]
pub(crate) fn collect_source_files(
    kb_path: &Path,
    registry: &Registry,
    rules: &crate::exclusion::ExclusionRules,
) -> Result<Vec<std::path::PathBuf>> {
    collect_source_files_under(kb_path, kb_path, registry, rules)
}

/// The same walk, started somewhere below `kb_path` instead of at it.
///
/// Exclusion still relativizes against `kb_path`, so a subtree answers exactly
/// what the full walk would answer for the same paths — that is the whole point
/// of routing both through one body rather than writing a second walk.
/// `start` is filtered like any other entry, so handing in an excluded
/// directory yields nothing.
///
/// The caller is the watcher. On Linux a file written into a directory that was
/// created microseconds earlier produces **no event on any watch**: inotify
/// watches are per-directory, and the file already exists by the time the new
/// directory's own watch can be registered (measured on Ubuntu 22.04: file
/// present 0.79 ms after `mkdir`, earliest possible watch 2.41 ms). Nothing
/// inside `notify` can recover it, so whoever starts watching a new directory
/// has to look inside it once. Windows does not have this hole —
/// `ReadDirectoryChangesW` watches the subtree from a single handle — which is
/// why it went unnoticed until a Linux-only CI failure.
pub(crate) fn collect_source_files_under(
    kb_path: &Path,
    start: &Path,
    registry: &Registry,
    rules: &crate::exclusion::ExclusionRules,
) -> Result<Vec<std::path::PathBuf>> {
    collect_source_files_counted(kb_path, start, registry, rules).map(|c| c.files)
}

/// Whether the walk leaves the given path out for its name (ADR-0023): it
/// lies under `kb_path` as spelled, and its relative path is one
/// [`crate::resources::doc_is_addressable`] refuses.
///
/// Only a path under `kb_path` as spelled is judged. The watcher hands in raw
/// event paths, and for one that does not share that spelling there is no
/// relative path to judge -- [`crate::exclusion::rel_key`] would fall back to
/// the whole absolute path, which no name passes, and refuse even `a.md`.
/// Those are left to the watcher's own relativizing. The root relativizes to
/// the empty string, which is no document name and must not be pruned.
fn name_is_left_out(kb_path: &Path, path: &Path) -> bool {
    index_rel_path(kb_path, path).is_some_and(|r| !crate::resources::rel_is_indexable_name(&r))
}

/// What [`collect_source_files_counted`] collected, how many entries it
/// left out for their names, and what it left out and why.
#[derive(Debug)]
pub(crate) struct CollectedSources {
    pub(crate) files: Vec<std::path::PathBuf>,
    /// See [`IndexResult::unspellable`].
    pub(crate) unspellable: u32,
    /// (#660) Each entry the walk left out, one per entry in walk order (no order is
    /// promised): a name the index cannot hold ([`SkipReason::UnspellableName`], a directory
    /// once) or a hard link ([`SkipReason::HardLink`]). The path is spelled by
    /// [`index_rel_path_or_whole`].
    pub(crate) left_out: Vec<SkippedFile>,
}

/// [`collect_source_files_under`], also counting what it left out because the
/// name is not one the index can hold.
///
/// (ADR-0023) A name [`crate::resources::doc_is_addressable`] refuses would be
/// a search hit that `get_document` of [`crate::server`] refuses and no
/// `kb://doc/` URI names --
/// on Windows `CON.md`, or anything under `dir./`, both of which the verbatim
/// `\\?\` prefix lets exist. A directory is pruned in `filter_entry`, so its
/// subtree is never read; a file is checked after the extension filter, so
/// only a file that would otherwise have been indexed is named. Each is one
/// warning, from [`crate::resources::unspellable_reason`].
///
/// Both levels ask [`name_is_left_out`], so a directory and a file are judged
/// by the same key.
pub(crate) fn collect_source_files_counted(
    kb_path: &Path,
    start: &Path,
    registry: &Registry,
    rules: &crate::exclusion::ExclusionRules,
) -> Result<CollectedSources> {
    let mut files = Vec::new();
    let extensions = registry.extensions();
    // A `Cell`, because `filter_entry` holds its closure for as long as the
    // loop body runs and both of them count.
    let unspellable = std::cell::Cell::new(0u32);
    // (#660) A `RefCell` for the same reason. Each push borrows within its own statement, so
    // no guard outlives it into the other caller.
    let left_out = std::cell::RefCell::new(Vec::<SkippedFile>::new());
    let leave_out = |path: &Path, is_dir: bool| {
        tracing::warn!("{}", crate::resources::unspellable_reason(path, is_dir));
        unspellable.set(unspellable.get() + 1);
        left_out.borrow_mut().push(SkippedFile {
            path: index_rel_path_or_whole(kb_path, path),
            reason: SkipReason::UnspellableName { directory: is_dir },
        });
    };

    // (AX-35) `follow_links(false)` leaves the root alone: walkdir follows a root that is a
    // link unless `follow_root_links(false)` says otherwise. The watcher's `start` is a name
    // inside the KB that a writer can swap for a link (or, on Windows, a junction) to a
    // directory outside it. The full index hands in the canonicalized KB root, which is never
    // a link, so this changes nothing there.
    for entry in WalkDir::new(start)
        .follow_links(false)
        .follow_root_links(false)
        .into_iter()
        .filter_entry(|e| {
            // `kb_path` itself relativizes to the empty string, which is never
            // excluded — otherwise a pattern like `*` would prune the root and
            // the walk would produce nothing.
            let rel = crate::exclusion::rel_key(kb_path, e.path());
            if rules.is_excluded(&rel, e.file_type().is_dir()) {
                return false;
            }
            if e.file_type().is_dir() && name_is_left_out(kb_path, e.path()) {
                leave_out(e.path(), true);
                return false;
            }
            true
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
                if name_is_left_out(kb_path, entry.path()) {
                    leave_out(entry.path(), false);
                    continue;
                }
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
                    left_out.borrow_mut().push(SkippedFile {
                        path: index_rel_path_or_whole(kb_path, entry.path()),
                        reason: SkipReason::HardLink,
                    });
                    continue;
                }
                files.push(entry.into_path());
            }
        }
    }

    files.sort();
    Ok(CollectedSources {
        files,
        unspellable: unspellable.get(),
        left_out: left_out.into_inner(),
    })
}

/// Extract `(category, topic)` from a relative path.
///
/// ```text
/// "deep-dive/chromadb/overview.md" → (Some("deep-dive"), Some("chromadb"))
/// "ai-news/2026-04-16.md"         → (Some("ai-news"), None)
/// "index.md"                       → (None, None)
/// ```
///
/// (AX-01) Also read by [`Database::rename_document`], which re-derives both from the new path
/// when a rename moves a row without re-embedding it -- the topic only when the rename's bytes
/// declare none, the way the index merges the two.
pub(crate) fn extract_category_topic(rel_path: &str) -> (Option<String>, Option<String>) {
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

/// (feature-56) Compare the configured code chunk budget with the one this index was built
/// with, and say so when they differ.
///
/// Changing `[parsers.code].max_chunk_chars` moves where chunks are cut, but a file whose
/// content has not changed never reaches the parser again — the content hash matches and the
/// walker skips it. So the new budget applies to files edited afterwards and to nothing else,
/// and the index quietly holds two generations of boundaries.
///
/// **The recorded value is deliberately not updated on a mismatch.** Writing it would silence
/// the warning on the next run while leaving the index exactly as inconsistent, which turns a
/// visible problem into an invisible one. It is written when the index is first built or
/// rebuilt with `force` — the moments when the whole index actually does match the setting.
pub(crate) fn resolve_code_chunk_budget(db: &Database, desired: usize, force: bool) -> Result<()> {
    if force {
        db.write_code_max_chunk_chars(desired)?;
        return Ok(());
    }
    match db.read_code_max_chunk_chars()? {
        Some(stored) if stored != desired => {
            eprintln!(
                "warning: [parsers.code].max_chunk_chars is {desired}, but the code chunks in this index were cut at {stored}. Files whose content has not changed keep their existing boundaries. Re-run with --force to re-chunk them."
            );
        }
        Some(_) => {}
        None => db.write_code_max_chunk_chars(desired)?,
    }
    Ok(())
}

/// What this build does to a code file that wants more chunks than one file may contribute.
///
/// The value is a generation rather than a version: it changes when the answer changes, and
/// `degrade` is the answer [ADR-0017] gave.
///
/// [ADR-0017]: https://github.com/alphabet-h/grooveseek/blob/main/docs/decisions/0017-bound-the-chunk-count-without-dropping-bytes.md
pub(crate) const CODE_CHUNK_POLICY: &str = "degrade";

/// (#251) Recorded in `index_meta.frontmatter_policy` once every unchanged Markdown document
/// in the index has been looked at by a build that tags a frontmatter it could not parse.
///
/// Absence means "not looked at yet": an index written before this existed may hold such
/// documents with a matching content hash and no tag, and the unchanged fast path in
/// [`rebuild_index`] would never read them again. The value is a generation, like
/// [`CODE_CHUNK_POLICY`]: it changes when what the parser writes changes.
pub(crate) const FRONTMATTER_POLICY: &str = "tag-unparsed";

/// (feature-62) Recorded in `index_meta.docx_heading_policy` once every `.docx` row of the
/// index has been settled under the heading rule that reads `word/styles.xml` (ADR-0027):
/// re-read and found to split the same way, rewritten, dropped, or marked with
/// [`HASH_AWAITING_REPARSE`] so the unchanged fast path cannot keep it.
///
/// Absence means "not looked at yet": an index written before that rule holds `.docx` rows
/// split by the style-ID spelling, with a matching content hash, and the unchanged fast path
/// in [`rebuild_index`] would never read them again. A generation, like
/// [`FRONTMATTER_POLICY`], and a key of its own, since the two passes cover different files and
/// do different work (ADR-0028).
///
/// (feature-63, feature-64) The pass also runs while [`DOCX_SECTION_POLICY`] or
/// [`DOCX_TABLE_POLICY`] is not recorded at its value (absent or another value): it runs while
/// any of the three keys is not at its value, and the final transaction writes all three, this
/// one first.
pub(crate) const DOCX_HEADING_POLICY: &str = "styles-name-basedon";

/// (feature-63) Recorded in `index_meta.docx_section_policy` beside [`DOCX_HEADING_POLICY`],
/// in the same transaction and after it, once every `.docx` row of the index has been settled
/// under the rule that folds a heading with no body text into the next section that has one
/// (instead of a chunk with an empty body). Absence or another value opens the same one-time
/// pass the heading key does (ADR-0028): [`rebuild_index`] runs it when any of the three keys,
/// this one, [`DOCX_HEADING_POLICY`] and [`DOCX_TABLE_POLICY`], is not at its value.
///
/// A key of its own rather than a new value of [`DOCX_HEADING_POLICY`], which stays what
/// v1.16.0 recorded; the two answer different questions about the same rows.
pub(crate) const DOCX_SECTION_POLICY: &str = "fold-empty-headings";

/// (feature-64) Recorded in `index_meta.docx_table_policy` beside [`DOCX_HEADING_POLICY`] and
/// [`DOCX_SECTION_POLICY`], in the same transaction and after them, once every `.docx` row of
/// the index has been settled under the rule that writes each table row as one line, its cells
/// separated by tabs. Absence or another value opens the same one-time pass the other two keys
/// do; when it is the only stale one, the pass announces itself with
/// [`docx_table_reread_notice`].
///
/// A key of its own rather than a new value of either older key, whose values tests pin and
/// whose absence alone is pinned to reopen the pass. ADR-0028 records the third key and why the
/// per-row column it names as the general form was not taken.
pub(crate) const DOCX_TABLE_POLICY: &str = "tab-joined-rows";

/// (feature-65) Recorded in `index_meta.pptx_table_break_policy` once every `.pptx` row of the
/// index has been settled under the rules that write each table row as one line, its cells
/// separated by tabs, and read `<a:br>` as a line break. Absence or another value opens the
/// one-time `.pptx` pass, which re-reads `.pptx` rows only and announces itself with
/// [`pptx_reread_notice`]; the final transaction writes the key after the `.docx` ones. A key per
/// format, so a stale key of one format never re-reads the other's rows (ADR-0029).
pub(crate) const PPTX_TABLE_BREAK_POLICY: &str = "tab-joined-rows-kept-breaks";

/// (feature-66) Recorded in `index_meta.docx_table_rows_policy` after the three earlier `.docx`
/// keys, in the same transaction, once every `.docx` row of the index carries its table rows
/// (`chunks.table_rows`). Absence or another value opens the one-time `.docx` pass; when it is
/// the only stale `.docx` key the pass announces itself with [`docx_rows_reread_notice`], and
/// the chunk text matches, so the pass writes that column alone and re-embeds nothing. A fourth
/// key of its own rather than a new value of an earlier one, whose values tests pin (ADR-0030).
pub(crate) const DOCX_TABLE_ROWS_POLICY: &str = "row-spans";

/// (feature-66) Recorded in `index_meta.pptx_table_rows_policy` after
/// [`PPTX_TABLE_BREAK_POLICY`], in the same transaction, once every `.pptx` row of the index
/// carries its table rows. Absence or another value opens the one-time `.pptx` pass, announced
/// with [`pptx_rows_reread_notice`] when this is the only stale `.pptx` key (ADR-0030).
pub(crate) const PPTX_TABLE_ROWS_POLICY: &str = "row-spans";

/// (feature-62 introduced it for the `.docx` pass) The `content_hash` a row of a one-time pass
/// (ADR-0029) carries when that pass could not settle it: skipped by the scan, failed to parse,
/// refused by the endpoint, or changed under the read. No file hashes to it -- a SHA-256 here is 64
/// lowercase hex digits ([`sha256_hex_bytes`]) -- so the unchanged fast path never matches it,
/// and every later run reads the file the way it reads a changed one until it can be indexed.
/// Rename detection also reads the hash; ADR-0028 records the
/// decision.
pub(crate) const HASH_AWAITING_REPARSE: &str = "awaiting-reparse";

/// Where [`index_single_disk_entry`] gets the declared-field set from (feature-58; codex P2
/// round 9 / 12 on PR #291, local Codex after round 12).
#[derive(Clone, Copy, Debug)]
pub(crate) enum DeclaredSet<'a> {
    /// The caller knows the set: [`rebuild_index`], which just read the schema. Written as
    /// given. `pass` is the token of the pass this run began ([`begin_declared_fields_pass`]),
    /// or `None` when it had nothing to refresh; either way, a write that finds **another**
    /// pass's token in the database marks that pass dirty (local Codex on PR #291 after
    /// round 12, second pass: two [`rebuild_index`] runs in separate processes can overlap,
    /// and the later one must not record its generation over rows the earlier one wrote
    /// after it had passed them).
    Known {
        list: &'a [String],
        pass: Option<&'a str>,
        /// The JSON of `list`, the generation this run will record at its finish (codex P2
        /// round 14 on PR #291): a write that finds **no** pass open but a *different*
        /// generation recorded has been overtaken by a pass that already finished -- its
        /// rows would sit under that generation -- and clears the key so the index reads
        /// as pending until the next run refreshes.
        generation: &'a str,
    },
    /// The caller does not know it and must not guess: the watcher paths
    /// ([`reindex_single_file`], [`rename_single_file`]), which write what the last
    /// completed pass recorded. Resolved **inside the write transaction**, after the
    /// document row was written and so under the write lock, never ahead of the file I/O
    /// and the embedding -- a [`rebuild_index`] in another process can clear the
    /// generation in that window, and a list read before it would then be written over
    /// rows the new pass has already processed and will never revisit (local Codex after
    /// round 12; before, the watcher read the list up front and passed `Option`).
    FromIndex,
}

/// (feature-58, codex P2 round 9 / 12 on PR #291) Write a document's `document_fields` rows
/// when the declared set is known, and when it is **not** -- [`DeclaredSet::FromIndex`]
/// resolving to an absent generation, because a [`rebuild_index`] refresh has it cleared
/// right now -- leave the rows alone and record that this document was written under the
/// pass that is running ([`Database::mark_declared_fields_dirty`]). The three write paths of
/// [`index_single_disk_entry`] all come through here so the rule has one home, and each
/// calls it inside its transaction after a document write, so the read of `index_meta` and
/// the rows it decides are one atomic unit against any other connection.
///
/// The mark is what closes the cross-process window round 12 found: the rebuild in another
/// process may already have processed this document; its rows are then the *previous*
/// generation's under a hash the rebuild considers current, and only the rebuild can act on
/// that -- by not recording the new generation ([`finish_declared_fields_pass`]).
fn write_declared_rows_or_mark_dirty(
    db: &Database,
    rel: &str,
    extra: &std::collections::BTreeMap<String, crate::parser::FieldValue>,
    declared: DeclaredSet<'_>,
) -> Result<()> {
    let (resolved, my_pass, my_generation): (Option<Vec<String>>, Option<&str>, Option<&str>) =
        match declared {
            DeclaredSet::Known {
                list,
                pass,
                generation,
            } => (Some(list.to_vec()), pass, Some(generation)),
            DeclaredSet::FromIndex => (declared_fields_recorded(db)?, None, None),
        };
    if let Some(list) = &resolved {
        db.replace_document_fields(rel, &declared_field_rows(extra, list))?;
    }
    // (local Codex on PR #291 after round 12, second pass) A pass that is not this caller's
    // is running -- another process's `rebuild_index`, overlapping this one or this
    // watcher -- and it may already have processed this document. The rows just written are
    // this caller's answer, not that pass's, so that pass must not record its generation
    // over them: mark it dirty, exactly as a watcher does for an absent generation.
    let current_pass = db.read_declared_fields_pass()?;
    let foreign_pass_running = match (current_pass.as_deref(), my_pass) {
        (Some(current), Some(mine)) => current != mine,
        (Some(_), None) => true,
        (None, _) => false,
    };
    if resolved.is_none() || foreign_pass_running {
        db.mark_declared_fields_dirty()?;
    }
    // (codex P2 round 14 on PR #291) No pass open, but the generation recorded is not the
    // one this run writes under: a pass that began after this run *finished* already --
    // recorded its set and cleared its token -- and the rows just written sit under that
    // set as if they belonged to it. Nothing is running to mark, so the key itself goes:
    // an absent generation refuses field filters and makes the next run refresh, and this
    // run's own finish, finding its observation or token gone, records nothing.
    if let Some(mine) = my_generation
        && current_pass.is_none()
        && db
            .read_declared_fields()?
            .is_some_and(|recorded| recorded != mine)
    {
        db.clear_declared_fields()?;
    }
    Ok(())
}

/// (codex P2 round 12 on PR #291) Start a declared-field refresh pass: clear the generation
/// key ([`Database::clear_declared_fields`], the round 3 rule) and record a token naming
/// this pass ([`Database::write_declared_fields_pass`]) in the same transaction, so a
/// watcher in another process that writes a document meanwhile can say it did so under
/// *this* pass. Returns the token for [`finish_declared_fields_pass`].
///
/// A previous pass's leftover token or dirty mark is replaced here: whatever that pass left
/// behind, this one rewrites every document's rows before it finishes. That includes a pass
/// that is **still running** in another process (local Codex on PR #291 after round 12,
/// second pass): refusing or waiting would let a crashed pass's token block every later
/// run for good, so the later pass takes over instead, and the earlier one finds at
/// [`finish_declared_fields_pass`] that it no longer owns the token and records nothing --
/// while any document it writes after this point marks the new pass dirty
/// ([`write_declared_rows_or_mark_dirty`]), so the new pass records nothing either and
/// the run after that refreshes again.
fn begin_declared_fields_pass(db: &Database) -> Result<String> {
    let token = format!(
        "{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    );
    let tx = db.begin_immediate_transaction()?;
    db.clear_declared_fields()?;
    db.clear_declared_fields_pass()?;
    db.write_declared_fields_pass(&token)?;
    tx.commit()?;
    Ok(token)
}

/// (codex P2 round 12 on PR #291) End a pass: remove its token and any dirty mark, then
/// record `declared_json` as the generation -- unless `record` is false (the caller's
/// existing reasons for leaving the key absent: a refresh that could not read every
/// document), or the dirty mark carries **this pass's** token, meaning a watcher in another
/// process wrote a document after this pass may have processed it. That document's rows are
/// the previous generation's under a hash this pass considers current; recording the new set
/// would make them current for good, so the key stays absent, the next run refreshes again
/// (absent != declared, the round 3 rule), and the operator is told why. Returns whether the
/// generation was recorded.
///
/// A dirty mark carrying an *older* token is a write under a pass that never finished; the
/// refresh that just ran rewrote that document's rows too, so it is dropped with the token.
///
/// The mark is read **inside** an immediate transaction ([`Database::begin_immediate_transaction`]):
/// the write lock is held from before the read until the decision is committed, so a
/// watcher in another process cannot commit a document and its mark between "no mark seen"
/// and "generation recorded" (local Codex after round 12 -- reading the mark first and
/// opening the transaction afterwards left exactly that gap).
fn finish_declared_fields_pass(
    db: &Database,
    token: Option<&str>,
    observed_at_start: Option<&str>,
    declared_json: &str,
    record: bool,
) -> Result<bool> {
    let tx = db.begin_immediate_transaction()?;
    // (codex P2 round 13 on PR #291) A run that began no pass (its schema matched the
    // recorded generation) still must not write over a generation some other process
    // recorded meanwhile: B changes the schema, refreshes every row, records `[b]` and
    // clears its token before A gets here, and A -- token-less, seeing no pass open --
    // would rewrite `[a]` over rows B produced. The evidence A has is the generation it
    // observed when it started; if the key no longer reads that, another run finished in
    // between and A's answer is stale.
    if token.is_none() && db.read_declared_fields()?.as_deref() != observed_at_start {
        tx.commit()?;
        tracing::warn!(
            "another `groove index` run recorded a different declared-field set while this \
             one was running; this run leaves that set in place"
        );
        return Ok(false);
    }
    // (local Codex on PR #291 after round 12, second pass) Ownership first: the token in the
    // database is this pass's only if no other process began a pass since. If another did,
    // its generation is the one in flight -- this run's rows were written under a set that
    // pass will rewrite, or already has -- so this run must neither clear that pass's token
    // and mark nor record a generation of its own; it leaves everything to the owner. A run
    // that never began a pass (`token == None`, nothing to refresh) likewise must not record
    // while some other process's pass is open: writing the key would end that pass's
    // "pending" from outside.
    let current = db.read_declared_fields_pass()?;
    let superseded = match (token, current.as_deref()) {
        (Some(mine), Some(theirs)) => mine != theirs,
        // This pass's token is gone: only another pass's begin (which replaces it) or its
        // finish (which, being the owner by then, clears it) can have removed it. Either way
        // that pass rewrote or is rewriting rows this run had already passed.
        (Some(_), None) => true,
        (None, Some(_)) => true,
        (None, None) => false,
    };
    if superseded {
        tx.commit()?;
        tracing::warn!(
            "another `groove index` run began while this one was refreshing the declared \
             fields; this run leaves the declared-field set to that run"
        );
        return Ok(false);
    }
    let dirty_under_this_pass = match (token, db.read_declared_fields_dirty()?) {
        (Some(mine), Some(dirty)) => mine == dirty,
        _ => false,
    };
    db.clear_declared_fields_pass()?;
    let recorded = record && !dirty_under_this_pass;
    if recorded {
        db.write_declared_fields(declared_json)?;
    }
    tx.commit()?;
    if dirty_under_this_pass {
        tracing::warn!(
            "another process updated a document while this run was refreshing the declared \
             fields; the declared-field set is left pending -- run `groove index` again"
        );
    }
    Ok(recorded)
}

/// (feature-58) What `index_meta.declared_fields` holds when the schema declares
/// no key beyond the five named ones, or there is no schema: the JSON of an
/// empty list. An absent generation key skips the refresh pass and is recorded
/// as this value directly only when there is nothing to write **and** nothing
/// left over from an earlier, interrupted run -- `document_fields` must be
/// empty too (codex P2 round 2 on PR #291; see
/// [`Database::document_fields_is_empty`]'s use in [`rebuild_index`]).
pub(crate) const DECLARED_FIELDS_NONE: &str = "[]";

/// The declared-field list the last completed [`rebuild_index`] recorded, read
/// back via [`Database::read_declared_fields`] and parsed out of its JSON --
/// for the watcher paths ([`reindex_single_file`], [`rename_single_file`]),
/// which do not derive their own list from a schema the way [`rebuild_index`]
/// does.
///
/// Three states, not two (codex P2 round 9 on PR #291): the generation key can be **absent**
/// (a refresh is in progress -- see [`Database::clear_declared_fields`]'s doc for when
/// [`rebuild_index`] clears it at the *start* of a pass -- or a run ended before writing it),
/// hold **`[]`** (the schema declares nothing, or there is no schema), or hold a **non-empty
/// list**. Only the first is genuinely unknown: `[]` is a completed answer meaning "no declared
/// keys", exactly like an empty list from a schema with none. Returning `None` for "absent" and
/// `Some(vec![])` for "declared nothing" is what lets [`index_single_disk_entry`]'s
/// `declared_fields` parameter tell "I do not know yet" from "there is nothing to write" --
/// before this fix, an absent key collapsed to an empty list here, and a watcher event that
/// landed while a refresh was clearing and rewriting the key would wipe a document's valid
/// `document_fields` rows down to nothing.
fn declared_fields_recorded(db: &Database) -> Result<Option<Vec<String>>> {
    db.read_declared_fields()?
        .as_deref()
        .map(crate::db::decode_declared_fields)
        .transpose()
}

/// Reads `<kb_path>/groove-schema.toml` — the same file `groove validate`
/// reads — and compiles it, without deriving anything from it yet.
///
/// (feature-58, codex P1 round 1, P2 round 3 on PR #291) Read once by the caller, before
/// anything destructive, and handed to [`rebuild_index`] rather than read again inside it: a
/// schema that does not load must stop the run before anything is deleted, the way a
/// `groove.toml` that does not load stops the binary before it opens the database.
/// [`rebuild_index`]'s own [`reset_and_resolve_context_mode`] call empties the index on
/// `--force` -- since AW-03 (ADR-0024) the only reset on either path. `groove index` and the
/// MCP tool of the same name as [`rebuild_index`] (`rebuild_index_blocking` in
/// [`crate::server`]) both call this before entering [`rebuild_index`], so a schema that does not load stops the
/// run before that reset, and both give it the same kind of snapshot.
///
/// A second read inside [`rebuild_index`], after the caller's own validation, would
/// only reopen the window between the two reads to a schema that changes out from under the
/// run — the file could be replaced with something malformed in between and the reset would
/// already be done. Reading once, before either side of that window, is what removes it, not
/// what happens to be closed after (codex P1 round 1); the reverse ordering is not something an
/// in-process test can force, since it requires a second process editing the file between two
/// reads this process makes microseconds apart.
///
/// (codex P2 round 9 on PR #291) The CLI's `Commands::Index` arm also calls this ahead of
/// [`Embedder::with_settings`], not only ahead of the resets: a malformed schema is refused before a
/// run that is already doomed pays for a model download or load (BGE-M3: ~2.3 GB) it was never
/// going to use, the same "cheap checks first" reasoning `main.rs` already applies to
/// `[parsers].enabled` validation.
pub fn load_declared_schema(kb_path: &Path) -> Result<Option<crate::schema::Schema>> {
    crate::schema::Schema::load_optional(&kb_path.join("groove-schema.toml"))
}

/// The keys `groove-schema.toml` declares that the parser keeps in
/// [`crate::parser::Frontmatter::extra`] — every `[fields.<name>]` except the
/// five the parser stores in their own fields, which have their own columns
/// and their own filters. Sorted, so its JSON is a stable generation key.
pub(crate) fn declared_field_names(schema: Option<&crate::schema::Schema>) -> Vec<String> {
    const NAMED: [&str; 5] = ["title", "date", "topic", "depth", "tags"];
    schema
        .map(|s| {
            s.fields
                .keys()
                .filter(|k| !NAMED.contains(&k.as_str()))
                .cloned()
                .collect()
        })
        .unwrap_or_default()
}

/// The `document_fields` rows of one document: for each declared key the
/// frontmatter holds, a scalar as one row and a list as one row per element.
/// An opaque shape and a null hold no value (ADR-0019) and write nothing;
/// an undeclared key is not the index's business.
pub(crate) fn declared_field_rows(
    extra: &std::collections::BTreeMap<String, crate::parser::FieldValue>,
    declared: &[String],
) -> Vec<(String, String)> {
    use crate::parser::FieldValue;
    let mut rows = Vec::new();
    for key in declared {
        match extra.get(key) {
            Some(FieldValue::Scalar(s)) => rows.push((key.clone(), s.clone())),
            Some(FieldValue::List(vs)) => rows.extend(vs.iter().map(|v| (key.clone(), v.clone()))),
            Some(FieldValue::Other(_)) | None => {}
        }
    }
    rows
}

/// What an index that was built before [`CODE_CHUNK_POLICY`] existed is recorded as.
///
/// Absence used to mean this. Recording it instead is what lets the question be settled
/// once: absence means "not looked at yet", and looking is the part that costs (codex P1,
/// round 7). The evidence is not erased by writing it down — it is written down.
pub(crate) const CODE_CHUNK_POLICY_LEGACY: &str = "truncate-pre-1.6";

/// Record which chunking policy this index was built under, when that can be said honestly.
///
/// An index written before v1.6.0 may hold code documents whose tails the old truncation cut
/// off, and nothing on those documents says so: the tag comes from the parser, and a file
/// whose content has not changed never reaches the parser again. So the answer cannot be
/// worked out later — it has to be recorded while it is still knowable, which is the first
/// time any run looks at the index.
///
/// Three states, and the key is written in all of them:
///
/// - `--force` re-chunks everything, so the index matches this build: [`CODE_CHUNK_POLICY`].
/// - Otherwise, an index holding no source file has nothing the old rule could have cut:
///   [`CODE_CHUNK_POLICY`] again.
/// - Otherwise it was built by something else and may be damaged:
///   [`CODE_CHUNK_POLICY_LEGACY`], which `groove doctor` reports.
///
/// **Emptiness is measured in source files, not in documents.** A Markdown-only knowledge
/// base that switches code parsing on has documents already, and every source file the run is
/// about to add is chunked by this build; asking [`Database::document_count`] there would
/// call it legacy over prose that was never in question and send its owner to an unnecessary
/// `--force` (codex P2, round 3). The population asked is the one the finding reads, through
/// the same predicate, so the two cannot come to disagree about who counts as a source file.
///
/// **Called once per indexed file**, so the answer has to be settled rather than recomputed.
/// Recording the legacy state is what settles it: an earlier version left the key absent
/// there, so the lookup that decides never short-circuited and every entry paid for a scan
/// over the prose ahead of the first source document (codex P1, rounds 6 and 7). Absence now
/// means only "not looked at yet".
pub(crate) fn resolve_code_chunk_policy(db: &Database, force: bool) -> Result<()> {
    if force {
        db.write_code_chunk_policy(CODE_CHUNK_POLICY)?;
        return Ok(());
    }
    if db.read_code_chunk_policy()?.is_some() {
        return Ok(());
    }
    let policy = if db.has_documents_with_line_numbers()? {
        CODE_CHUNK_POLICY_LEGACY
    } else {
        CODE_CHUNK_POLICY
    };
    db.write_code_chunk_policy(policy)?;
    Ok(())
}

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
                     with '{}'. Run `groove index --force` to migrate; continuing in '{}' mode.",
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
                     Run `groove index --force` to build a contextual index.",
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
/// 呼び出し側は reset しない (AW-03)。CLI の `index --force` もかつて先に
/// reset していたが、それは [`rebuild_index`] 冒頭の provider probe (ADR-0024)
/// より前に index を空にするので外した。force 時の reset はここだけ。
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
mod tests;
