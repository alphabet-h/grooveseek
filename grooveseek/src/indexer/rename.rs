//! (AX-40) Renames: the pairs a full run detects ([`detect_renames`]), a rename across
//! parsers and its settling, and the watcher's [`rename_single_file`].

use super::*;

/// (audit AX-02) What [`rename_single_file`] does with [`resolves_inside_kb`]'s answer for the
/// destination `full`: `Ok(None)` to go on and read it.
///
/// - outside: the old row is deleted and nothing is read
///   ([`RenameOutcome::MovedOutOfTheKnowledgeBase`])
/// - (review Low-3) the look failed: the row moves to `new_rel` first and the error is
///   returned, the state a failed read of the destination leaves
///
/// Split out so a test can hand it a failed look, which a real file system rarely produces
/// after `exists()` has just succeeded.
pub(super) fn settle_rename_destination(
    db: &Database,
    old_rel: &str,
    new_rel: &str,
    full: &Path,
    inside: std::io::Result<bool>,
) -> Result<Option<RenameOutcome>> {
    match inside {
        Ok(true) => Ok(None),
        Ok(false) => {
            eprintln!("{}", outside_kb_line(new_rel));
            db.delete_document(old_rel)?;
            Ok(Some(RenameOutcome::MovedOutOfTheKnowledgeBase))
        }
        Err(e) => {
            db.rename_document(old_rel, new_rel, FrontmatterTopic::Unread)?;
            Err(e).with_context(|| format!("failed to resolve {}", full.display()))
        }
    }
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
pub(super) fn detect_renames(
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

/// Whether renaming `old_rel` to `new_rel` moves the file to a different parser
/// (feature-58, codex P2 round 5 on PR #291).
///
/// A same-hash rename normally skips re-parsing on both paths that see one --
/// [`rebuild_index`]'s rename-detection loop and the watcher's
/// [`rename_single_file`] -- because the bytes did not change, so there is
/// nothing new to embed. That shortcut is wrong when the rename also crosses
/// parsers: `document_fields`, `title`, and `tags` all came from whichever
/// parser last read the file, and a `.txt` renamed to `.md` (or back) with the
/// same bytes needs the *other* parser's reading, not none. [`Registry::by_extension`]
/// (case-insensitive, same lookup [`indexed_hash_for`] and the rename
/// code already use) resolving to `None` on one side and `Some` on the other
/// counts as a crossing too -- an unregistered extension has no parser at
/// all, which is as different from any registered one as two registered
/// parsers are from each other.
///
/// One predicate rather than the check duplicated at each of the two call
/// sites (AGENTS "one question gets one implementation"): the parser-crossing
/// decision has one definition, and both rename paths ask it the same
/// question.
pub(crate) fn rename_crosses_a_parser(registry: &Registry, old_rel: &str, new_rel: &str) -> bool {
    let parser_of = |rel: &str| {
        let ext = Path::new(rel)
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("");
        registry.by_extension(ext).map(|p| p.extension())
    };
    parser_of(old_rel) != parser_of(new_rel)
}

/// After a rename [`rename_crosses_a_parser`] said crosses parsers, the caller reparses the
/// file under the destination parser -- forced when the bytes did not change (codex P2 round 5
/// on PR #291), or through the ordinary changed-content arm when they did (codex P2 round 10:
/// [`rename_single_file`] settles both, not only the same-byte case). When that reparse does
/// not end in [`SingleResult::Updated`] -- the new parser refuses the bytes
/// ([`SingleResult::Refused`]), fails to parse them (a [`SingleResult::Skipped`] whose reason is
/// "parse failed"), or finds nothing to chunk (a [`SingleResult::Skipped`] whose reason is
/// [`SKIPPED_NO_CHUNKS`]) -- the document row at `rel` is still whatever the *old* parser last
/// wrote there: chunks, metadata, `document_fields`, none of which the new path's parser stands
/// behind (codex P2 round 9 on PR #291, a regression the round 5 fix introduced: it forces the
/// reparse but never checked what the reparse came back with). Delete the row so it matches
/// what a fresh file the new parser cannot read already gets: nothing.
///
/// Shared by [`rebuild_index`]'s rename loop and [`rename_single_file`], the only two callers
/// of [`rename_crosses_a_parser`], so the decision has one home (AGENTS.md "one question gets
/// one implementation") rather than being reimplemented at each call site.
///
/// (#544, D5) Except a skip for bytes that were not the ones the scan (or the rename's first
/// read) hashed ([`SKIPPED_CHANGED_DURING_READ`]): nothing was learned about the file, so the
/// row stays, under [`HASH_AWAITING_REPARSE`] so that no later fast path keeps the old parser's
/// chunks.
///
/// A further read of a crossed rename this kept awaiting -- [`read_crossed_rename_again_explained`] in
/// [`rebuild_index`], or the watcher's reread ([`crate::watcher`]) -- does not come here: it is
/// settled by [`settle_further_crossed_read`] (#544, local Codex r4/r5). `Ok(true)` when the
/// row was dropped.
///
/// (#552) Both callers overwrite the old row's hash with [`HASH_AWAITING_REPARSE`] before the
/// rename moves it: [`rebuild_index`] for every pair that crosses a parser, and
/// [`rename_single_file`] for its one rename (which then reaches this through its refusal exits
/// and through [`reindex_renamed_entry`]). So when either calls this the row already holds the
/// sentinel; what this does with the outcome is the same as before the mark.
pub(crate) fn settle_cross_parser_rename(
    db: &Database,
    rel: &str,
    outcome: &SingleResult,
) -> Result<bool> {
    let keep_awaiting = match outcome {
        SingleResult::Updated { .. } => return Ok(false),
        SingleResult::Skipped { reason, .. } => *reason == SKIPPED_CHANGED_DURING_READ,
        SingleResult::Unchanged
        | SingleResult::MetadataRefreshed { .. }
        | SingleResult::Refused
        | SingleResult::OutsideKnowledgeBaseDeindexed
        | SingleResult::NothingToIndex { .. } => false,
    };
    if keep_awaiting {
        db.overwrite_content_hash(&[rel], HASH_AWAITING_REPARSE)?;
    } else {
        db.delete_document(rel)?;
    }
    Ok(!keep_awaiting)
}

/// (#544, local Codex r4/r5, D5) The settlement of a further read of a crossed rename this
/// branch adds after the first read kept the row awaiting: [`read_crossed_rename_again_explained`] in
/// [`rebuild_index`], and the watcher's crossed rereads ([`crate::watcher`]). Both ask the read
/// to check its bytes against its own hash before parsing (`row_dropped_unless_updated` on
/// [`index_single_disk_entry`]), and only results of such a read come here.
///
/// It names the outcomes that may drop the row; every other one keeps it under
/// [`HASH_AWAITING_REPARSE`], for the next event or run. The row is dropped for:
///
/// - a result reached after the bytes were checked: [`SingleResult::NothingToIndex`] (the read
///   already removed the row), and a [`SingleResult::Skipped`] for [`SKIPPED_PARSE_FAILED`],
///   [`SKIPPED_NO_CHUNKS`] or [`SKIPPED_EMBED_REJECTED`]
/// - a destination established gone: [`SKIPPED_FILE_GONE`] (#544 D1), and
///   [`SingleResult::OutsideKnowledgeBaseDeindexed`], a path reached through a link, whose row
///   the read already removed
/// - a fact [`crate::links::read_checked`] or the size look established about the file (J3): [`SKIPPED_TOO_LARGE`]
///   and [`SingleResult::Refused`] (over the cap, a hard link, not a regular file; a failed
///   read is an error of its own, never a refusal)
///
/// Kept: [`SKIPPED_READ_FAILED`], [`SKIPPED_CHANGED_DURING_READ`], any other skip reason,
/// [`SingleResult::Unchanged`], [`SingleResult::MetadataRefreshed`]. [`SingleResult::Updated`]
/// wrote the row and is left alone. A new variant is a compile error here; a new skip reason
/// keeps the row until it is named. `Ok(true)` when the row was dropped.
pub(crate) fn settle_further_crossed_read(
    db: &Database,
    rel: &str,
    outcome: &SingleResult,
) -> Result<bool> {
    let drop = match outcome {
        SingleResult::Updated { .. } => return Ok(false),
        SingleResult::NothingToIndex { .. }
        | SingleResult::OutsideKnowledgeBaseDeindexed
        | SingleResult::Refused => true,
        SingleResult::Skipped { reason, .. } => matches!(
            *reason,
            SKIPPED_PARSE_FAILED
                | SKIPPED_NO_CHUNKS
                | SKIPPED_EMBED_REJECTED
                | SKIPPED_FILE_GONE
                | SKIPPED_TOO_LARGE
        ),
        SingleResult::Unchanged | SingleResult::MetadataRefreshed { .. } => false,
    };
    if drop {
        db.delete_document(rel)?;
    } else {
        db.overwrite_content_hash(&[rel], HASH_AWAITING_REPARSE)?;
    }
    Ok(drop)
}

/// (#544, local Codex r1) The further read [`rebuild_index`] gives, once, before it returns, a
/// crossed rename its loop kept under [`HASH_AWAITING_REPARSE`] ([`settle_cross_parser_rename`]
/// on a [`SKIPPED_CHANGED_DURING_READ`] skip): the file is read and hashed again, and those
/// bytes are indexed as a watcher event indexes a file ([`Reindex::Event`]), under the run's
/// declared set and context mode.
///
/// Why [`Reindex::Event`]: the row's hash is the sentinel, so no mode takes the fast path, and
/// this one is the only mode that checks the bytes it parses against the hash just taken (a
/// changed read is a [`SKIPPED_CHANGED_DURING_READ`] skip again) while writing the hash on a
/// meta-only update. Not [`Reindex::Reparse`], whose matching-chunks exit writes no hash and
/// leaves the sentinel over chunks it found current. The check runs before the parse, as for
/// the rename's first read, so a failed parse is always one of the bytes this read hashed. A
/// read that fails is the skip the loop gives one ([`SKIPPED_READ_FAILED`]), so the run goes on.
///
/// (kuriya 670) The meta-only update is not taken here after all: the row holds the old
/// parser's chunks, so even when their text matches, the row is rewritten whole, as the
/// rename's first read ([`Reindex::Renamed`]) rewrites it, with the stored vectors reused
/// ([`index_single_disk_entry_explained`] decides this from `row_dropped_unless_updated`).
///
/// (#544, local Codex r3, J3) The caller settles the result with
/// [`settle_further_crossed_read`], as the loop settled the first read: the row is written from
/// these bytes, or dropped when the new parser cannot index them (a refused read, a failed
/// parse, an endpoint's rejection, nothing to embed), and kept under the sentinel for bytes
/// that changed again and (local Codex r4, D5) for a read that failed, which checked no bytes.
/// A refusal records the length it measured first; the row it lands on is then deleted, which
/// leaves nothing behind.
///
/// (#660) [`rebuild_index`] calls [`read_crossed_rename_again_explained`]; this wrapper is kept
/// for the tests that read the result alone.
#[cfg(test)]
pub(super) fn read_crossed_rename_again(
    db: &Database,
    embedder: &mut Embedder,
    entry: &DiskEntry,
    exclude_headings: Option<&[String]>,
    registry: &Registry,
    context_mode: ContextMode,
    declared: DeclaredSet<'_>,
) -> Result<SingleResult> {
    read_crossed_rename_again_explained(
        db,
        embedder,
        entry,
        exclude_headings,
        registry,
        context_mode,
        declared,
        &mut ChunkVectorCounts::default(),
    )
    .map(|r| r.result)
}

/// The re-read of a file whose rename crossed a parser, with the reason the file was not
/// indexed (#660): the body, which the test-only wrapper above calls and drops the reason of. A
/// refused or failed re-read gives its own reason (spec R2 / R3); the rest come from
/// [`index_single_disk_entry_explained`], which also sets `counts` (kuriya 621).
#[allow(clippy::too_many_arguments)]
pub(super) fn read_crossed_rename_again_explained(
    db: &Database,
    embedder: &mut Embedder,
    entry: &DiskEntry,
    exclude_headings: Option<&[String]>,
    registry: &Registry,
    context_mode: ContextMode,
    declared: DeclaredSet<'_>,
    counts: &mut ChunkVectorCounts,
) -> Result<Reindexed> {
    let ext = entry
        .full
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("");
    let is_binary = registry.by_extension(ext).is_some_and(|p| p.is_binary());
    let cap = applicable_cap(
        is_binary,
        registry.limits().binary.cap_bytes(),
        registry.limits().text.cap_bytes(),
    );
    let key = size_cap_key(is_binary);
    let bytes = match read_for_index_under(&entry.full, &entry.rel, cap, key) {
        Ok((Some(bytes), _)) => bytes,
        Ok((None, refused)) => {
            // (#544, GitHub codex P2 on PR #396) The file can have grown past the cap since
            // the scan: this read is then the only current measurement, and the kept row
            // would otherwise go on advertising the scan's servable size.
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
    let again = DiskEntry {
        hash: sha256_hex_bytes(&bytes),
        size: bytes.len() as u64,
        ..entry.clone()
    };
    index_single_disk_entry_explained(
        db,
        embedder,
        &again,
        exclude_headings,
        registry,
        Reindex::Event,
        context_mode,
        declared,
        // The caller settles the result with `settle_cross_parser_rename`: bytes other than the
        // ones hashed above are a changed-during-read skip before they are parsed, so a parse
        // failure is always one of the verified bytes.
        true,
        counts,
    )
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
    /// (BU-20) 旧 path が DB に無く、新 path を新規として index しようとしたら
    /// **refuse された**。
    ///
    /// `OldPathMissing` と分けるのは、あちらが「新 path を index した」を
    /// 意味するため (codex P2 round 2 on PR #157)。DB に row は作られていない
    /// のに watcher が「indexed」と報告するのは、その後の調査を狂わせる。
    ///
    /// (AW-04) embedding endpoint が新 path の入力を拒否した skip ([`crate::indexer`] の
    /// private const `SKIPPED_EMBED_REJECTED` を reason に持つもの) もここに入る。
    /// 同じく row は作られていない。
    OldPathMissingAndRefused,
    /// path は UPDATE 済だが、新 path の binary size が cap 超過のため
    /// hash 再計算 / reindex はスキップした (codex P2 round 3)。DB の
    /// content_hash は旧内容のまま据え置き、次回 full rebuild の
    /// `scan_disk_entries` の size-cap 判定に委ねる (§4.2 skip 統一原則)。
    RenamedSizeCapped,
    /// (codex P2 round 11 / 12 on PR #291) [`RenameOutcome::RenamedSizeCapped`], but the rename
    /// crossed a parser, so the old parser's row was **dropped** rather than left under the
    /// new extension. A variant of its own because the watcher's diagnostic says what happened
    /// to the document, and "hash check skipped, content kept" would be false here.
    RenamedSizeCappedAndDropped,
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
    /// (codex P2 round 11 / 12 / 13 on PR #291) The rename crossed a parser and the new
    /// parser could not index the bytes -- it refused them at either read, failed to parse
    /// them, or found nothing to chunk -- so the old parser's row was **dropped**; "content
    /// left as it was" would be false, hence the separate variant.
    RenamedButRefusedAndDropped,
    /// (#544) The rename crossed a parser and the destination's second read was not the bytes
    /// the first read hashed, so nothing was learned: the old parser's row stays under the new
    /// path with [`HASH_AWAITING_REPARSE`], which the next event or run re-reads.
    RenamedAcrossParsersAwaitingReparse,
    /// (local Codex on PR #291 after round 13) path は UPDATE 済で、同じ parser の再 parse
    /// (Static モードが同 bytes の rename に強制するもの、または内容が変わった通常の parse) が
    /// [`SingleResult::Skipped`] で終わった: 行は書き換えられず、**以前の内容** (Static なら以前の path 由来の
    /// breadcrumb も) が新 path の下に残っている。行は消さない (skip の理由は stderr に出ている)
    /// が、[`RenameOutcome::Renamed`] (= 内容同一で path だけ更新) と区別して watcher が「古い内容のまま」と言える
    /// ようにする。
    RenamedButNotReindexed,
    /// (audit AX-02) The new path is reached through a link, to outside the knowledge base or
    /// (GitHub codex r1 on PR #391) to elsewhere inside it. The old row was **deleted** and
    /// nothing at the new path was read: the document is no longer in the knowledge base under
    /// any name the full index would give it.
    MovedOutOfTheKnowledgeBase,
}

/// 単一ファイルの rename を処理する。
/// - `old_rel` / `new_rel` とも forward-slash、`kb_path` 相対
/// - DB 側の path を UPDATE し、必要なら再 index (内容変更がある場合)
/// - **size cap**: 新 path が registry の上限 ([`Registry::limits`]) を超えていれば hash
///   再計算のための `fs::read` をスキップする ([`size_cap_exceeded`]、codex P2 round 3。
///   これで scan / reindex / rename の 3 read 経路すべてが同じ size-cap guard を通る)
/// - **path 由来の列** (AX-01、[`Database::rename_document`]): 行の topic は、その行の chunk を
///   作った bytes (か同じ hash の bytes) から作る。新 path の bytes を読めて hash が行と一致した
///   移動は、`category` を新 path から、topic をその bytes の frontmatter の topic か、無ければ
///   新 path から作る。読めなかった移動 (上限超え・読み込みの失敗・読み込みの拒否) は `category`
///   だけを新 path から作り、topic は保存値のまま残す (frontmatter を読めないので、path 由来の
///   topic と frontmatter の topic を区別できない)。旧 path 由来の topic は、file が再び読める
///   ようになった後の `--force` か内容の編集 (= 再 parse) で直る。bytes が同じままの通常の
///   full run は hash が一致して行に触らないので直さない。ただし parser をまたぐ移動
///   ([`rename_crosses_a_parser`]) で上限超えか読み込みの拒否なら、旧 chunk が新 parser にとって
///   無効なので行ごと消える ([`settle_cross_parser_rename`]。結果は
///   [`RenameOutcome::RenamedSizeCappedAndDropped`] か
///   [`RenameOutcome::RenamedButRefusedAndDropped`])。読み込みの失敗 (`Err`) は parser を
///   またいでも行を消さない (番兵の hash で残る、#552)
/// - 中身も変わった移動 (hash が行と違う) も、移す時点では `category` だけを新 path から作り、
///   topic は保存値のまま残す (codex P2 round 4 on PR #388)。続く reindex が行を書き換えれば、
///   topic は index 時の規則で新しい bytes から書かれる。書き換えなければ (chunk が 0・2 度目の
///   読み込みの拒否・parse の失敗)、同じ parser の間の移動なら行は旧 hash・旧 chunk・それと対の
///   topic のまま残る。parser をまたぐ移動 ([`rename_crosses_a_parser`]) は旧 chunk が新 parser に
///   とって無効なので残せず、reindex が [`SingleResult::Updated`] で終わらなければ行ごと消える
///   ([`settle_cross_parser_rename`])。ただし 2 度目の読み込みが 1 度目と違う bytes だった時
///   (#544) は何も分からないので、行は番兵の hash で残る
///   ([`RenameOutcome::RenamedAcrossParsersAwaitingReparse`])
/// - parser をまたぐ移動は、行を移す前に旧行の hash を番兵 ([`HASH_AWAITING_REPARSE`]) に
///   する (#552)。移した後の reindex か後始末が `Err` で終わっても、行は番兵で残り、次の
///   event / run が読み直す。`Ok` で終わる経路は hash を書き直すか行を消すので、結果は
///   今までと同じ
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
        // 新 path を新規として reindex する。**結果を捨てない** — refuse された
        // 場合に `OldPathMissing` を返すと「index した」と報告してしまう
        // (codex P2 round 2 on PR #157)。
        return Ok(
            match reindex_single_file(db, embedder, kb_path, new_rel, exclude_headings, registry)? {
                // (AW-04) An endpoint rejection writes no row either, so it is not "indexed".
                // (AX-02) A path outside the knowledge base creates no row either.
                SingleResult::Refused
                | SingleResult::OutsideKnowledgeBaseDeindexed
                | SingleResult::Skipped {
                    reason: SKIPPED_EMBED_REJECTED,
                    ..
                } => RenameOutcome::OldPathMissingAndRefused,
                // (#543) `NothingToIndex` read the file and dropped any row under it, as a
                // skip of a new file leaves none.
                SingleResult::Updated { .. }
                | SingleResult::Unchanged
                | SingleResult::Skipped { .. }
                | SingleResult::NothingToIndex { .. }
                | SingleResult::MetadataRefreshed { .. } => RenameOutcome::OldPathMissing,
            },
        );
    };

    // (local Codex on PR #388) The row moves once the new path's bytes have been read, so a
    // same-hash move rebuilds its topic from the bytes this function hashes below -- the same
    // bytes the same-hash decision is made on, not a second read. The exits that read nothing
    // (size cap, failed read, refused read, missing file) move it with `FrontmatterTopic::Unread`
    // first: the category follows the new path, the stored topic stays beside the stored chunks,
    // since without the bytes a frontmatter topic cannot be told from the old path's (local Codex
    // r3). A move whose bytes changed moves with `Unread` too (codex P2 round 4): see below.

    // (codex P2 round 11 on PR #291) Decided here, before any of the early returns below: a
    // rename that crosses a parser leaves the *old* parser's row under the new path until
    // something rewrites it, and the size-cap and read-refusal exits below rewrite nothing.
    // Each of them settles the crossing the same way the reparse arms further down do
    // (`settle_cross_parser_rename`'s doc), so the row is dropped rather than kept as a stale
    // hit a later full index would preserve as an oversized skip.
    let crosses_a_parser = rename_crosses_a_parser(registry, old_rel, new_rel);

    // 新 path の実体 hash を読み直し、DB 側 (= old_hash) と比較
    let full = kb_path.join(new_rel);
    match look_at_single_path(kb_path, new_rel, &full)
        .with_context(|| format!("failed to stat {}", full.display()))?
    {
        SinglePathLook::There => {}
        // (local Codex after PR #391 r1) `exists` follows links: a destination through a link
        // whose target is gone is settled as one reached through a link -- the old row and
        // whatever the index holds for the destination (its row and its folded legacy row)
        // go, as the full index drops them. (#544) So is one whose stat failed outright with a
        // link on the way ([`look_at_single_path`]).
        SinglePathLook::ThroughALink => {
            eprintln!("{}", outside_kb_line(new_rel));
            db.delete_document(old_rel)?;
            deindex_outside_kb(db, kb_path, new_rel)?;
            return Ok(RenameOutcome::MovedOutOfTheKnowledgeBase);
        }
        SinglePathLook::Missing => {
            // 新 path のファイルも無い。通常起こらないが起きたら DB も掃除
            db.rename_document(old_rel, new_rel, FrontmatterTopic::Unread)?;
            db.delete_document(new_rel)?;
            // The file is gone under every spelling, so an older version's folded row for the
            // destination goes too (a removal's sweep); the watcher's sweep after a rename asks
            // for the real key, which was just deleted, and would keep it.
            sweep_legacy_folded_row(db, kb_path, new_rel, false)?;
            return Ok(RenameOutcome::Renamed); // path は UPDATE 済 (後で delete)
        }
    }
    // (#552) A rename that crosses a parser moves a row holding the old parser's chunks, and
    // until the reparse below settles it the row would keep the hash of bytes that may be the
    // ones on disk. Every exit from here on that moves the row (the link check's failed look,
    // the size cap, a failed or refused read, the move before the reparse) comes after this
    // mark, so one that ends in `Err` after the move leaves a row no fast path keeps; the next
    // event or run reads it again. The `Ok` exits rewrite the hash or drop the row, as before.
    // `same_hash` below is still decided on `old_hash`, read before the mark.
    if crosses_a_parser {
        db.overwrite_content_hash(&[old_rel], HASH_AWAITING_REPARSE)?;
    }
    // (audit AX-02) A destination reached through a directory link is not in the knowledge
    // base, so the document has left it: the row is dropped, as for a move into an excluded
    // directory, and nothing at the new path is read.
    if let Some(outcome) = settle_rename_destination(
        db,
        old_rel,
        new_rel,
        &full,
        readable_as_the_walk_reads(kb_path, new_rel, &full),
    )? {
        // (local Codex r2) Whatever the index already held for the destination -- a row from
        // before its directory became a link, or an older version's folded row -- describes a
        // path outside the knowledge base too, and the full index drops it. The old path's
        // folded row is the watcher's sweep of `old_rel`, as for any removal.
        if outcome == RenameOutcome::MovedOutOfTheKnowledgeBase {
            deindex_outside_kb(db, kb_path, new_rel)?;
        }
        return Ok(outcome);
    }

    // size-cap ガード: fs::read する前に判定する。超過なら DB の path だけ
    // rename_document (topic は Unread) で UPDATE し、内容には触らない (= 旧 hash のまま
    // 新 path に残る)。hash 再計算 / reindex は次回 full rebuild に委ねる。
    let ext = full.extension().and_then(|e| e.to_str()).unwrap_or("");
    let is_binary_ext = registry
        .binary_extensions()
        .iter()
        .any(|e| e.eq_ignore_ascii_case(ext));
    if let Ok(Some((len, cap))) = size_cap_exceeded(
        &full,
        is_binary_ext,
        registry.limits().binary.cap_bytes(),
        registry.limits().text.cap_bytes(),
    ) {
        let (kind, key) = (size_cap_kind(is_binary_ext), size_cap_key(is_binary_ext));
        eprintln!(
            "Skipping {new_rel}: {kind} file too large ({len} bytes > {cap} limit); raise [index].{} to admit it",
            key.key()
        );
        db.rename_document(old_rel, new_rel, FrontmatterTopic::Unread)?;
        // (codex P2 round 11 on PR #291) Crossed a parser: the row is the old parser's
        // and nothing below will rewrite it. Settle before recording a size for it.
        if crosses_a_parser {
            settle_cross_parser_rename(db, new_rel, &SingleResult::Refused)?;
            return Ok(RenameOutcome::RenamedSizeCappedAndDropped);
        }
        // (codex P2 round 7) The rename has already been applied, so the row is
        // under `new_rel` with the size it had when it was small enough to
        // index. Measuring the file and returning without writing leaves it
        // listed and linked while a read refuses it — the same hole the reindex
        // guard closes, in the one place still missing it.
        if let Err(e) = db.record_document_sizes(&[(new_rel, len)]) {
            tracing::warn!("failed to record the grown size of {new_rel}: {e}");
        }
        return Ok(RenameOutcome::RenamedSizeCapped);
    }

    let cap = applicable_cap(
        is_binary_ext,
        registry.limits().binary.cap_bytes(),
        registry.limits().text.cap_bytes(),
    );
    let read = read_for_index_under(&full, new_rel, cap, size_cap_key(is_binary_ext));
    let (maybe_bytes, refused) = match read {
        Ok(read) => read,
        Err(e) => {
            // Before the move waited for the bytes, a failed read found the row already moved.
            // It moves here too, so a failed read leaves the state it always left.
            db.rename_document(old_rel, new_rel, FrontmatterTopic::Unread)?;
            return Err(e).with_context(|| format!("failed to read {}", full.display()));
        }
    };
    let Some(new_bytes) = maybe_bytes else {
        db.rename_document(old_rel, new_rel, FrontmatterTopic::Unread)?;
        // (codex P2 round 6) The rename target has the same stat-then-read
        // window as every other reader, and this was the last caller still
        // dropping the length the refusal measured.
        // (codex P2 round 11 on PR #291) Same as the size-cap exit above: a crossed
        // parser's stale row is dropped rather than left under the new path.
        if crosses_a_parser {
            settle_cross_parser_rename(db, new_rel, &SingleResult::Refused)?;
            return Ok(RenameOutcome::RenamedButRefusedAndDropped);
        }
        if let Some(len) = measured(&refused)
            && let Err(e) = db.record_document_sizes(&[(new_rel, len)])
        {
            tracing::warn!("failed to record the grown size of {new_rel}: {e}");
        }
        return Ok(RenameOutcome::RenamedButRefused);
    };
    let new_hash = sha256_hex_bytes(&new_bytes);
    let same_hash = new_hash == old_hash;
    // (local Codex on PR #388) The row's topic is built from the bytes its chunks came from, or
    // bytes of the same hash. When the bytes just hashed match the row's hash they are such
    // bytes, and their frontmatter topic is the row's (the fast path below keeps it). (codex P2
    // round 4 on PR #388) When they do not, they are not committed yet: the row moves with
    // `FrontmatterTopic::Unread`, and the reindex below writes their topic only if it rewrites
    // the row. If it writes nothing (no chunks, a second-read refusal, a failed parse), the row
    // keeps its old hash, its old chunks and the topic those chunks were built with.
    let read_topic = same_hash
        .then(|| frontmatter_topic_of(registry, new_rel, &new_bytes))
        .flatten();
    let topic = if same_hash {
        FrontmatterTopic::Read(read_topic.as_deref())
    } else {
        FrontmatterTopic::Unread
    };
    db.rename_document(old_rel, new_rel, topic)?;
    // 測った側が記録する (上の reindex と同じ理由)。
    if let Err(e) = db.record_document_sizes(&[(new_rel, new_bytes.len() as u64)]) {
        tracing::warn!("failed to record the size of {new_rel}: {e}");
    }

    // codex P2 round 2 (finding A): watcher は config-desired を持たないので
    // DB 側モードに従う (`reindex_single_file` と同じ E-11 の規則)。
    // rebuild_index の一括 rename (F3, PR #73) と同じ理由で、Static モードでは
    // same-hash fast path を無効化する: rename は内容 (hash) を変えないが、
    // Static モードでは frontmatter title が無い文書の context breadcrumb が
    // filename stem 由来 (E-1) のため、再 parse しない限り旧 filename のまま
    // stale 化してしまう。Off モードは context を embed に使わないため無害 =
    // 従来通り fast path を維持する。
    // (codex P2 round 5 on PR #291) **parser を跨ぐ rename も同様に無効化する**:
    // mode を問わず、`rename_crosses_a_parser` (`rebuild_index` の一括 rename と
    // 共有する 1 つの判定) が true なら fast path から外す。bytes は同じでも
    // 旧 parser が書いた `document_fields` / title / tags が新 parser の読みへ
    // 更新されないまま残ってしまうため。
    let context_mode = db.read_context_mode()?.unwrap_or(ContextMode::Off);
    if same_hash && context_mode != ContextMode::Static && !crosses_a_parser {
        return Ok(RenameOutcome::Renamed);
    }

    // 内容が変わっている、または (Static モードの same-hash rename として)
    // breadcrumb 更新のため読み直す (入力が変わった chunk だけを再 embed する)。size-cap 判定は既に上で
    // 済んでいるので、読み込み済みの bytes/hash をそのまま再利用して
    // 二重 read を避ける (`reindex_single_file` を経由すると全 read をやり直す)。
    let entry = DiskEntry {
        rel: new_rel.to_string(),
        hash: new_hash,
        full,
        size: new_bytes.len() as u64,
    };
    // same_hash (= Static モードでの強制、または parser を跨いだ rename) の
    // 場合のみ hash 一致 fast path をバイパスする。内容が変わって
    // いる場合は通常の Incremental 経路 (frontmatter-only skip 判定含む) に任せる。
    // (kuriya 608) `Force` ではなく `Renamed`: 保存 vector を読み、入力が変わらなかった chunk は
    // それを使う。行はこの時点で new_rel に移っているので、読むのは移った行の vector。
    let mode = if same_hash {
        Reindex::Renamed
    } else {
        Reindex::Incremental {
            check_frontmatter: false,
            refresh_fields: false,
        }
    };
    reindex_renamed_entry(
        db,
        embedder,
        &entry,
        mode,
        context_mode,
        exclude_headings,
        registry,
        crosses_a_parser,
    )
}

/// The reparse [`rename_single_file`] ends with, once the row has moved to the entry's [`DiskEntry::rel`] and the
/// first read's bytes and hash are in `entry`: [`index_single_disk_entry`] in `mode`, the
/// settlement of a rename that crossed a parser ([`settle_cross_parser_rename`]), and the
/// outcome that says what became of the row. A function of its own so that a test can hand it
/// an entry whose hash is not the bytes the second read finds (#544 PR-2, J5).
#[allow(clippy::too_many_arguments)]
pub(super) fn reindex_renamed_entry(
    db: &Database,
    embedder: &mut Embedder,
    entry: &DiskEntry,
    mode: Reindex,
    context_mode: ContextMode,
    exclude_headings: Option<&[String]>,
    registry: &Registry,
    crosses_a_parser: bool,
) -> Result<RenameOutcome> {
    let new_rel = entry.rel.as_str();
    // (feature-58) The watcher paths write what the last completed `rebuild_index`
    // recorded; they do not read the schema themselves -- and (local Codex after round
    // 12) do not read the generation here either: `index_single_disk_entry` resolves
    // `DeclaredSet::FromIndex` inside its write transaction, see `reindex_single_file`.
    let single_result = index_single_disk_entry(
        db,
        embedder,
        entry,
        exclude_headings,
        registry,
        mode,
        context_mode,
        DeclaredSet::FromIndex,
        crosses_a_parser,
    )?;
    // (codex P2 round 9 on PR #291) Crossed a parser: whatever the reparse came back with,
    // settle it the same way `rebuild_index`'s rename loop does. See
    // `settle_cross_parser_rename`'s doc. (codex P2 round 10) On `crosses_a_parser` alone,
    // not `same_hash && crosses_a_parser`: a watcher rename whose bytes also changed goes
    // through the `Incremental` arm above instead of `Force`, but when the destination
    // parser then refuses or fails to parse those bytes the row under `new_rel` is still
    // whatever the *old* parser wrote -- the same stale row the same-byte case left, reached
    // by a different arm. `rebuild_index`'s loop has no such gap: `detect_renames` only pairs
    // paths by equal hash, so every rename it settles is same-byte by construction.
    if crosses_a_parser {
        settle_cross_parser_rename(db, new_rel, &single_result)?;
    }
    // (codex P2 round 13 on PR #291) Once the settlement above dropped the row, the outcome
    // must say so whatever the reparse came back with: the second-read refusal and a parse
    // that failed or found nothing all leave the same state -- no document under the new
    // path -- and `RenamedButRefused` / `Renamed` would both tell the watcher's reader the
    // content was kept. (#544 PR-2) Except the changed-during-read skip, which the settlement
    // keeps under the awaiting-reparse hash: "dropped" would be false there.
    if crosses_a_parser {
        match single_result {
            SingleResult::Updated { .. } => {}
            SingleResult::Skipped {
                reason: SKIPPED_CHANGED_DURING_READ,
                ..
            } => return Ok(RenameOutcome::RenamedAcrossParsersAwaitingReparse),
            SingleResult::Skipped { .. }
            | SingleResult::Unchanged
            | SingleResult::MetadataRefreshed { .. }
            | SingleResult::Refused
            | SingleResult::OutsideKnowledgeBaseDeindexed
            | SingleResult::NothingToIndex { .. } => {
                return Ok(RenameOutcome::RenamedButRefusedAndDropped);
            }
        }
    }
    match single_result {
        SingleResult::Updated { chunks, .. } => Ok(RenameOutcome::RenamedAndReindexed { chunks }),
        // (codex P2 round 1 on PR #157) `index_single_disk_entry` reads the file
        // a **second** time, and the whole premise of this guard is that a path
        // can change between two reads. Letting that refusal fall into the
        // catch-all below would report a successful rename while the database
        // kept the old content under the new path.
        SingleResult::Refused => Ok(RenameOutcome::RenamedButRefused),
        // Only `reindex_single_file` returns it, never the shared entry called above; mapped
        // with the refusal it is a case of, and named so a new variant stays a compile error.
        SingleResult::OutsideKnowledgeBaseDeindexed => Ok(RenameOutcome::RenamedButRefused),
        // (#543) Only `reindex_single_file` returns it too (`Event` is not a mode used above);
        // mapped with the skip it is the watcher's form of, and named for the same reason.
        SingleResult::NothingToIndex { .. } => Ok(RenameOutcome::RenamedButNotReindexed),
        // Spelled out rather than `_`: a catch-all is what swallowed the
        // refusal in the first place, and it would swallow the next variant too.
        // `MetadataRefreshed` cannot come back here (the check is off on this
        // path) and is named so that adding a variant stays a compile error.
        // (local Codex on PR #291 after round 13) `Skipped` is not `Renamed`: the reparse
        // this arm was reached through -- forced by Static mode for a same-byte rename, or
        // the ordinary changed-content parse -- wrote nothing, so the row under the new
        // path still holds the previous content and, in Static mode, the previous path's
        // breadcrumb. The row is kept (the bytes it was built from were once indexable and
        // the reason for the skip is already on stderr), but the outcome says so, and the
        // watcher's line no longer reports a plain success over stale content.
        SingleResult::Unchanged | SingleResult::MetadataRefreshed { .. } => {
            Ok(RenameOutcome::Renamed)
        }
        SingleResult::Skipped { .. } => Ok(RenameOutcome::RenamedButNotReindexed),
    }
}
