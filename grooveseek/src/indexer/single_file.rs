//! (AX-40) Indexing one file: the core [`crate::indexer::single_file::index_single_disk_entry`]
//! that [`crate::indexer::rebuild_index`] and the watcher share, the watcher's entry points
//! ([`crate::indexer::single_file::reindex_single_file`] and its siblings), deindexing, and
//! the legacy folded spelling sweep.

use super::*;

/// What the first look of a single-file path ([`look_at_single_path`]) found.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum SinglePathLook {
    /// `full` can be stat'ed. Whether it may be read is the next question
    /// ([`readable_as_the_walk_reads`]).
    There,
    /// The stat did not reach a file (gone, or a stat that failed), and a component of `rel` is
    /// a link ([`a_link_is_on_the_way`]): the path is not one the full walk collects, whatever
    /// the stat would have said.
    ThroughALink,
    /// Not there ([`stat_error_means_gone`]) and no link on the way.
    Missing,
}

/// (#544, GitHub codex r1 P2 on PR #395) [`path_is_there`] for `full` (`kb_path` joined with
/// `rel`), with a failed stat settled by the links on the way. A link anywhere below `kb_path`
/// decides on its own that the path is not in the index, so it is asked before a stat error is
/// returned, not only after a stat that read as gone: an indexed directory replaced by a link
/// to itself fails the stat with a loop error, which says nothing about being gone, and
/// returning it kept the row of every file the walk no longer collects there. The one place
/// both [`reindex_single_file`] and [`rename_single_file`] take this first look.
pub(super) fn look_at_single_path(
    kb_path: &Path,
    rel: &str,
    full: &Path,
) -> std::io::Result<SinglePathLook> {
    sort_single_path_look(path_is_there(full), || a_link_is_on_the_way(kb_path, rel))
}

/// [`look_at_single_path`]'s decision, apart from the disk so a test can hand it any stat
/// answer. The link is asked only when the stat did not find the path there.
pub(super) fn sort_single_path_look(
    there: std::io::Result<bool>,
    link_on_the_way: impl FnOnce() -> bool,
) -> std::io::Result<SinglePathLook> {
    if let Ok(true) = there {
        return Ok(SinglePathLook::There);
    }
    // `Ok(false)` or a stat error: the link first, then the stat's own answer.
    if link_on_the_way() {
        return Ok(SinglePathLook::ThroughALink);
    }
    there.map(|_| SinglePathLook::Missing)
}

/// (audit AX-02, GitHub codex r1 on PR #391) Whether the watcher may read `rel` (at `full`) and
/// write or keep its row: the invariant is the full walk's, **no component below `kb_path` is a
/// link** ([`a_link_is_on_the_way`]), so a link to another directory inside the knowledge base
/// does not make a second name for its files either. [`resolves_inside_kb`] is asked as well;
/// with no link on the way it holds, so it is a second guard, not a second rule. `false` is
/// handled as a path outside the knowledge base: refused, and its rows dropped
/// ([`deindex_outside_kb`]).
///
/// Cost: one `symlink_metadata` per component of `rel` on every reindex and rename destination,
/// existing files included, plus the canonicalization of the root and of `full`.
pub(super) fn readable_as_the_walk_reads(
    kb_path: &Path,
    rel: &str,
    full: &Path,
) -> std::io::Result<bool> {
    if a_link_is_on_the_way(kb_path, rel) {
        return Ok(false);
    }
    resolves_inside_kb(kb_path, full)
}

/// (audit AX-02, local Codex r2) Drop what the index holds for `rel`, a path that now resolves
/// outside the knowledge base: the row under `rel` and the row an older version stored under its
/// [`legacy_folded_spelling`], as a removal does ([`sweep_legacy_folded_row`] with
/// `require_real_key = false`, so a folded row goes even when no row is under `rel`; its guard
/// against a real document at the folded path stays). The full index's walk does not follow the
/// link, so it drops both. `Ok(true)` when either was there.
pub(super) fn deindex_outside_kb(db: &Database, kb_path: &Path, rel: &str) -> Result<bool> {
    let exact = deindex_single_file(db, rel)?;
    let folded = sweep_legacy_folded_row(db, kb_path, rel, false)?;
    Ok(exact || folded)
}

/// The stderr line for a file [`resolves_inside_kb`] turned away. ASCII. It does not say
/// which kind of link took the path out (a directory on the way, or the file itself), since
/// the check does not know.
pub(super) fn outside_kb_line(rel: &str) -> String {
    format!(
        "Skipping {rel}: it is reached through a link (to elsewhere in or outside the \
         knowledge base), and links are not followed"
    )
}

/// 単一 [`DiskEntry`] を index する内部関数。
/// [`rebuild_index`] 本体と、watcher が呼ぶ [`reindex_single_file`] の
/// 両方で共通利用される核の処理。embedder は `&mut` で要求する (fastembed は
/// 同時呼び出し不可)。呼び出し側で Mutex 経由の相互排他を保証すること。
///
/// `declared` says where the declared-field set comes from -- see [`DeclaredSet`]. With
/// [`DeclaredSet::Known`] every `document_fields` write below uses that list (an empty list
/// still replaces stale rows from an undeclared key). With [`DeclaredSet::FromIndex`] the
/// list is read off `index_meta` **inside each write transaction**, right before the rows
/// are written, and an absent generation (pending: a [`rebuild_index`] refresh has cleared
/// the key and not yet rewritten it, or has not run at all) leaves the rows untouched and
/// marks the running pass instead ([`write_declared_rows_or_mark_dirty`]).
///
/// `row_dropped_unless_updated` is true for the destination of a rename that
/// [`rename_crosses_a_parser`]: the caller then settles it with
/// [`settle_cross_parser_rename`], which drops the row the old parser left under this path
/// unless the result is [`SingleResult::Updated`]. A refusal is worded for that final
/// state -- not in the index -- rather than for the row it sees now (local Codex before
/// round 2 on PR #329). Every other caller passes false.
///
/// (#544, D5) The hash written with a row is the hash of the bytes this function read and
/// parsed, not the scan's: the two reads can see different bytes, and a row whose hash names
/// bytes other than its chunks' would let those bytes, once back on disk, take the unchanged
/// fast path over chunks they did not produce.
#[allow(clippy::too_many_arguments)]
pub(super) fn index_single_disk_entry(
    db: &Database,
    embedder: &mut Embedder,
    entry: &DiskEntry,
    exclude_headings: Option<&[String]>,
    registry: &Registry,
    mode: Reindex,
    context_mode: ContextMode,
    declared: DeclaredSet<'_>,
    row_dropped_unless_updated: bool,
) -> Result<SingleResult> {
    index_single_disk_entry_explained(
        db,
        embedder,
        entry,
        exclude_headings,
        registry,
        mode,
        context_mode,
        declared,
        row_dropped_unless_updated,
        &mut ChunkVectorCounts::default(),
    )
    .map(|r| r.result)
}

/// [`index_single_disk_entry`] with the reason the file was not indexed (#660): the body. Each
/// return point that does not index the file names its reason (spec §6.1, R1-R7, R11).
///
/// (kuriya 621) `counts` is set where the document ends in [`SingleResult::Updated`], from the
/// plan that decided which chunks to embed ([`ChunkVectorCounts::of_plan`]) or, for the
/// meta-only write, as every chunk kept ([`ChunkVectorCounts::all_reused`]). Callers that want
/// it go through [`counted`], which keeps it only for an `Updated` result.
#[allow(clippy::too_many_arguments)]
pub(super) fn index_single_disk_entry_explained(
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

/// Write a parsed document into the index: its `documents` row, then one chunk row per parsed
/// chunk, scored under the quality profile the parser's kind and the chunk's
/// [`crate::parser::Chunk::symbol_kind`] decide and carrying the chunk's
/// [`crate::parser::Chunk::line_range`] and [`crate::parser::Chunk::symbol_kind`] as
/// [`crate::db::CodeMeta`].
///
/// (feature-66) And [`crate::parser::Chunk::table_rows`], in the same insert as the text it
/// describes.
///
/// This is the one place a parsed chunk becomes a row. Indexing calls it with the embeddings
/// the model produced; `tests/code_formats_light.rs` calls it with constants, so that what a
/// pull request checks is the same write path the binary runs and not a copy of it (codex P1
/// on PR #273, under `AGENTS.md`'s "One question gets one implementation"). Anything the
/// indexer decides *before* a chunk is written — the unchanged check, the frontmatter-only
/// update, reading and parsing the file — stays with the indexer; anything about how a chunk
/// is written belongs here.
///
/// `embeddings` holds one vector per chunk, in chunk order; a mismatch is an error rather
/// than a silent truncation, since a document whose last chunks were never written would
/// pass every count that reads `documents` and fail only a search for what they held.
///
/// The transaction is the caller's: the two write helpers this uses are autocommit-aware,
/// so a caller that wants the file written atomically opens one around the call (indexing
/// does) and a caller that does not, need not.
///
/// `topic` and `category` are the values derived from the path ([`extract_category_topic`]);
/// a topic in the frontmatter wins over the derived one, as it always has.
///
/// (feature-69) The write itself has one implementation, which also takes vectors carried over
/// from the document's previous chunks; this entry point hands it every vector as computed.
#[allow(clippy::too_many_arguments)]
pub fn write_parsed_document(
    db: &Database,
    rel: &str,
    parsed: &crate::parser::ParsedDocument,
    parser: &dyn crate::parser::Parser,
    embeddings: &[Vec<f32>],
    context_mode: ContextMode,
    content_hash: &str,
    size_bytes: u64,
    topic: Option<&str>,
    category: Option<&str>,
) -> Result<i64> {
    let vectors: Vec<ChunkVector<'_>> = embeddings
        .iter()
        .map(|e| ChunkVector::Computed(e))
        .collect();
    write_parsed_document_with_vectors(
        db,
        rel,
        parsed,
        parser,
        &vectors,
        context_mode,
        content_hash,
        size_bytes,
        topic,
        category,
    )
}

/// (feature-69) [`write_parsed_document`] with each chunk's vector either computed this run or
/// carried over as the bytes the index already held for an unchanged chunk
/// ([`Database::stored_chunk_vectors_for_path`]). `vectors` holds one per chunk, in chunk
/// order, under the same mismatch rule as `embeddings` there.
#[allow(clippy::too_many_arguments)]
pub(crate) fn write_parsed_document_with_vectors(
    db: &Database,
    rel: &str,
    parsed: &crate::parser::ParsedDocument,
    parser: &dyn crate::parser::Parser,
    vectors: &[ChunkVector<'_>],
    context_mode: ContextMode,
    content_hash: &str,
    size_bytes: u64,
    topic: Option<&str>,
    category: Option<&str>,
) -> Result<i64> {
    anyhow::ensure!(
        vectors.len() == parsed.chunks.len(),
        "{rel}: {} chunks but {} embeddings",
        parsed.chunks.len(),
        vectors.len()
    );
    let doc_id = db.upsert_document(
        rel,
        parsed.frontmatter.title.as_deref(),
        parsed.frontmatter.topic.as_deref().or(topic),
        category,
        parsed.frontmatter.depth.as_deref(),
        &parsed.frontmatter.tags,
        parsed.frontmatter.date.as_deref(),
        content_hash,
        size_bytes,
    )?;

    for (chunk, vector) in parsed.chunks.iter().zip(vectors.iter()) {
        let score = quality::chunk_quality_score(
            chunk.heading.as_deref(),
            &chunk.content,
            quality::QualityProfile::of(parser.is_binary(), chunk.symbol_kind.is_some()),
        );
        let context = match context_mode {
            ContextMode::Static => chunk.context.as_deref(),
            ContextMode::Off => None,
        };
        db.insert_chunk_with_vector(
            doc_id,
            chunk.index as i32,
            chunk.heading.as_deref(),
            chunk.level,
            &chunk.content,
            context,
            *vector,
            score,
            crate::db::CodeMeta {
                line_range: chunk.line_range,
                symbol_kind: chunk.symbol_kind.as_deref(),
            },
            chunk.table_rows.as_deref(),
        )?;
    }
    Ok(doc_id)
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

/// [`reindex_single_file`] with the reason the file was not indexed (#660): see [`Reindexed`]
/// for when [`Reindexed::reason`] is `Some`.
pub fn reindex_single_file_explained(
    db: &Database,
    embedder: &mut Embedder,
    kb_path: &Path,
    rel: &str,
    exclude_headings: Option<&[String]>,
    registry: &Registry,
) -> Result<Reindexed> {
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
}

/// (kuriya 621) [`reindex_single_file_explained`] with the [`ChunkVectorCounts`] of an updated
/// document, for the watcher's `reindexed` line: taken from the document's own plan, not from
/// the embedder's counter.
pub(crate) fn reindex_single_file_counted(
    db: &Database,
    embedder: &mut Embedder,
    kb_path: &Path,
    rel: &str,
    exclude_headings: Option<&[String]>,
    registry: &Registry,
) -> Result<CountedReindex> {
    counted(|counts| {
        reindex_single_file_checked_explained(
            db,
            embedder,
            kb_path,
            rel,
            exclude_headings,
            registry,
            false,
            counts,
        )
    })
}

/// (#544, local Codex r3, D5) [`reindex_single_file`] for the destination of a rename across a
/// parser, whose result the caller settles with [`settle_cross_parser_rename`]
/// (`row_dropped_unless_updated`, as [`index_single_disk_entry`] names it): bytes other than the
/// ones this function's first read hashed are a changed-during-read skip before they are
/// parsed, so a parse failure that drops the row is always one of the bytes checked. Every
/// other caller passes false, through [`reindex_single_file`].
pub(crate) fn reindex_single_file_checked(
    db: &Database,
    embedder: &mut Embedder,
    kb_path: &Path,
    rel: &str,
    exclude_headings: Option<&[String]>,
    registry: &Registry,
    row_dropped_unless_updated: bool,
) -> Result<SingleResult> {
    reindex_single_file_checked_explained(
        db,
        embedder,
        kb_path,
        rel,
        exclude_headings,
        registry,
        row_dropped_unless_updated,
        &mut ChunkVectorCounts::default(),
    )
    .map(|r| r.result)
}

/// [`reindex_single_file_checked`] with the reason the file was not indexed (#660): the body.
/// The watcher's entry gives R8 (gone), R9 (over the cap by the stat) and R2 (its first read
/// refused) itself, and `None` for a path through a link (R10, spec §6.1); the rest come from
/// [`index_single_disk_entry_explained`], which also sets `counts` (kuriya 621).
#[allow(clippy::too_many_arguments)]
fn reindex_single_file_checked_explained(
    db: &Database,
    embedder: &mut Embedder,
    kb_path: &Path,
    rel: &str,
    exclude_headings: Option<&[String]>,
    registry: &Registry,
    row_dropped_unless_updated: bool,
    counts: &mut ChunkVectorCounts,
) -> Result<Reindexed> {
    let full = kb_path.join(rel);
    match look_at_single_path(kb_path, rel, &full)
        .with_context(|| format!("failed to stat {}", full.display()))?
    {
        SinglePathLook::There => {}
        // (local Codex r3) `exists` follows links, so a path through a link whose target is
        // gone -- a directory replaced by a dangling link -- reads as missing. The full index
        // does not follow the link and drops the row, so this does too. The same component
        // check as for an existing file below ([`readable_as_the_walk_reads`]). (#544) A stat
        // that failed outright -- a directory replaced by a link to itself -- is settled the
        // same way when a link is on the way ([`look_at_single_path`]).
        SinglePathLook::ThroughALink => {
            eprintln!("{}", outside_kb_line(rel));
            return Ok(Reindexed {
                result: if deindex_outside_kb(db, kb_path, rel)? {
                    SingleResult::OutsideKnowledgeBaseDeindexed
                } else {
                    SingleResult::Refused
                },
                reason: None,
            });
        }
        SinglePathLook::Missing => {
            return Ok(Reindexed {
                result: SingleResult::Skipped {
                    reason: SKIPPED_FILE_GONE,
                    frontmatter_unparsed: false,
                },
                reason: Some(SkipReason::Unreadable {
                    cause: ReadFailure::Gone,
                }),
            });
        }
    }
    // (audit AX-02) Before the size stat and the read, both of which follow every link on
    // the way. Nothing is read or written for the file. (review P2-1) A row already under
    // `rel` -- its directory was indexed, then replaced by a link -- is dropped, as the full
    // index drops it (its walk does not follow the link, so the file is not collected).
    // (GitHub codex r1 on PR #391) A link to elsewhere *inside* the KB is refused the same way:
    // the walk does not follow it either, so its files would be indexed under a second name.
    if !readable_as_the_walk_reads(kb_path, rel, &full)
        .with_context(|| format!("failed to resolve {}", full.display()))?
    {
        eprintln!("{}", outside_kb_line(rel));
        return Ok(Reindexed {
            result: if deindex_outside_kb(db, kb_path, rel)? {
                SingleResult::OutsideKnowledgeBaseDeindexed
            } else {
                SingleResult::Refused
            },
            reason: None,
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
        registry.limits().binary.cap_bytes(),
        registry.limits().text.cap_bytes(),
    ) {
        let (kind, key) = (size_cap_kind(is_binary_ext), size_cap_key(is_binary_ext));
        eprintln!(
            "Skipping {rel}: {kind} file too large ({len} bytes > {cap} limit); raise [index].{} to admit it",
            key.key()
        );
        // (codex P2 round 4) Same as the full scan: the row stays, so without
        // this its recorded size is the last one small enough to index and the
        // resource surface keeps offering a file a read now refuses. Doing it
        // here as well is what keeps the watcher and the full run agreeing —
        // leaving it to the next full index is how the two come apart.
        if let Err(e) = db.record_document_sizes(&[(rel, len)]) {
            tracing::warn!("failed to record the grown size of {rel}: {e}");
        }
        return Ok(Reindexed {
            result: SingleResult::Skipped {
                reason: SKIPPED_TOO_LARGE,
                frontmatter_unparsed: false,
            },
            reason: Some(SkipReason::TooLarge {
                size: len,
                limit: cap,
                key,
            }),
        });
    }

    let cap = applicable_cap(
        is_binary_ext,
        registry.limits().binary.cap_bytes(),
        registry.limits().text.cap_bytes(),
    );
    let key = size_cap_key(is_binary_ext);
    let (maybe_bytes, refused) = read_for_index_under(&full, rel, cap, key)
        .with_context(|| format!("failed to read {}", full.display()))?;
    let Some(bytes) = maybe_bytes else {
        // (codex P2 round 5) The watcher has the same stat-then-read window.
        if let Some(len) = measured(&refused)
            && let Err(e) = db.record_document_sizes(&[(rel, len)])
        {
            tracing::warn!("failed to record the grown size of {rel}: {e}");
        }
        return Ok(Reindexed {
            result: SingleResult::Refused,
            reason: Some(reason_of_unread(refused, key)),
        });
    };
    let hash = sha256_hex_bytes(&bytes);
    let size = bytes.len() as u64;
    // (codex P2 round 6) **測った側が記録する**。`index_single_disk_entry` は
    // hash 一致で `Unchanged` を返し、その経路は何も書かない — full index なら
    // 走査の一括記録が拾うが、watcher にはそれが無い。cap 超えだった文書が
    // 元の内容に戻された時、ここで書かないと「read は受け付けるのに提示され
    // ない」が次の full index まで残る。
    if let Err(e) = db.record_document_sizes(&[(rel, size)]) {
        tracing::warn!("failed to record the size of {rel}: {e}");
    }
    let entry = DiskEntry {
        rel: rel.to_string(),
        hash,
        full,
        size,
    };
    // watcher は config-desired を持たないので DB 側モードに従う (E-11)。
    let context_mode = db.read_context_mode()?.unwrap_or(ContextMode::Off);
    // (feature-58) The watcher paths write what the last completed `rebuild_index`
    // recorded; they do not read the schema themselves. (codex P2 round 9 on PR #291,
    // local Codex after round 12) Resolved by `index_single_disk_entry` **inside its
    // write transaction**, not read here ahead of the file I/O and the embedding: a
    // `rebuild_index` in another process can clear the generation in that window, and a
    // list read before it would be written over rows the new pass never sees again.
    // (#251) The one-time frontmatter check belongs to `rebuild_index`.
    // (#543) `Event`, not `Incremental`: a file that now yields no chunks loses its row here.
    index_single_disk_entry_explained(
        db,
        embedder,
        &entry,
        exclude_headings,
        registry,
        Reindex::Event,
        context_mode,
        DeclaredSet::FromIndex,
        row_dropped_unless_updated,
        counts,
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

/// (feature-67) Delete every document indexed under the directory `rel`
/// (`rel/...`, any depth) and return the deleted paths. `rel` itself is not
/// touched; that is [`deindex_single_file`]'s job. See
/// [`Database::delete_documents_under`] for the predicate and the transaction.
pub fn deindex_subtree(db: &Database, rel: &str) -> Result<Vec<String>> {
    db.delete_documents_under(rel)
}

/// The key a version up to 1.12.0 stored for `rel` on a platform where `\` is
/// a filename character: `rel` with every `\` folded into `/`. `None` when
/// that is the same key -- no `\` in `rel`, or a platform where `\` separates
/// components and the fold is still what the index does.
///
/// **This exists for migration only.** Those versions folded `\` everywhere,
/// so an index they built holds a file named `secret\pay.md` under
/// `secret/pay.md`. The full index run re-keys such a row by itself
/// ([`detect_renames`]); the watcher, which touches one path at a time, has to
/// be told where the old row is, and this is the one place that says.
pub fn legacy_folded_spelling(rel: &str) -> Option<String> {
    if backslash_separates_components() || !rel.contains('\\') {
        return None;
    }
    Some(rel.replace('\\', "/"))
}

/// Whether `key` has the shape of a path the index walk could have produced:
/// not empty, and every `/`-separated segment a normal component -- none
/// empty (which also rules out a leading or trailing `/`), none `.` or `..`.
///
/// A string check on purpose. `Path::components` would quietly collapse the
/// `//` and the `.` this has to notice, and it reads the string by the
/// platform's rules while an index key is `/`-separated everywhere.
///
/// Not the traversal check in [`crate::resources`], which asks about a `kb://` URI and treats
/// `\` by platform; this one asks about an index key, where `\` is whatever
/// character it was on disk.
pub(super) fn names_a_walked_path(key: &str) -> bool {
    !key.is_empty()
        && key
            .split('/')
            .all(|seg| !seg.is_empty() && seg != "." && seg != "..")
}

/// Remove the row an older version stored for `rel` under its
/// [`legacy_folded_spelling`], if there is one and it is unambiguous.
/// `Ok(true)` when a row was removed.
///
/// The watcher calls this after it has written or removed the real key.
/// Without it a modify leaves two rows for one file, and a delete leaves the
/// old one behind for good -- findable, carrying a resource link, and
/// impossible to open -- until the next full index run.
///
/// **Unambiguous** means the index walk could not have a document of its own
/// at the folded path ([`a_walked_document_may_be_at`]). A real
/// `secret/pay.md` owns that key, and its row is a different document's.
///
/// The one rule behind every case: **nothing outside the knowledge base
/// decides.** The folded spelling is looked up only if it
/// [`names_a_walked_path`] -- `\foo.md` folds to `/foo.md`, which `Path::join`
/// would resolve by discarding `kb_path`, and `a\..\..\x.md` folds to
/// something that climbs out -- and then only by descending through real
/// directories, so a symlinked ancestor cannot send the question elsewhere
/// either. Where the walk could not have gone there is no document, and the
/// row is removed without asking what is on the other side.
///
/// `require_real_key` is for a caller that has just *written* `rel`: the sweep
/// then runs only if `rel` is in the index. A reindex that was refused or
/// skipped leaves the index as it was on purpose, and for such a file the
/// folded row is the index as it was -- the sweep follows the real key in,
/// never ahead of it. A removal passes `false`: the file is gone under every
/// spelling. A rename that has already moved the real key away passes `false`
/// too, without looking at `rel`; the folded spelling is still looked at below.
///
/// **The decision and the delete it justifies share one `BEGIN IMMEDIATE`
/// transaction** ([`Database::begin_immediate_transaction`]; kuriya 620: the
/// caller's, when one is open, which this then joins rather than failing on a
/// nested `BEGIN`; the watcher opens one to delete a removed path's rows
/// together). Other processes
/// write this database too -- a `groove index` run, a second server -- and the
/// caller's mutex does not serialize them. With the write lock held from
/// before the first read, such a writer either committed a real document at
/// the folded path before the look, in which case its file is there and the
/// row is kept, or commits after the delete and puts its row back. Looking
/// first and deleting afterwards would let it land in between and lose a row
/// that was right. Nothing is opened when there is no folded spelling, so an
/// ordinary path -- every path on Windows -- costs neither a lock nor a stat.
///
/// If the lock cannot be had within the connection's busy timeout (30 s, set
/// in [`Database::init`]) this returns the error and deletes nothing; the row
/// waits for the file's next event or the next full run. While the lock is
/// held the work is one stat per path component and one document's deletes.
pub fn sweep_legacy_folded_row(
    db: &Database,
    kb_path: &Path,
    rel: &str,
    require_real_key: bool,
) -> Result<bool> {
    let Some(legacy) = legacy_folded_spelling(rel) else {
        return Ok(false);
    };
    // Every early return below drops `tx`, which rolls back; nothing has been
    // written by then. (kuriya 620, AX-32) Inside a caller's transaction there is
    // no `tx` of its own: the caller's, which must be `BEGIN IMMEDIATE` for the
    // guarantee below, holds the decision and the delete, and an early return
    // leaves it as it was.
    let tx = db.begin_local_immediate()?;
    if require_real_key && db.get_document_hash(rel)?.is_none() {
        return Ok(false);
    }
    if names_a_walked_path(&legacy) && a_walked_document_may_be_at(kb_path, &legacy) {
        return Ok(false);
    }
    let removed = deindex_single_file(db, &legacy)?;
    if let Some(tx) = tx {
        tx.commit()?;
    }
    Ok(removed)
}

/// Whether the index walk could have a document at `key` under `kb_path`,
/// decided by descending one component at a time from `kb_path` and never
/// through anything but a real directory. `key` must already satisfy
/// [`names_a_walked_path`].
///
/// A single `symlink_metadata(kb_path.join(key))` is not this: it declines to
/// follow only the *last* component, so with `kb/alias -> /outside` a lookup of
/// `alias/pay.md` is answered by `/outside/pay.md`. The walk never goes there
/// ([`collect_source_files_under`] runs with `follow_links(false)` and takes
/// only entries whose own type is a regular file, so a symlink is neither
/// descended into nor indexed; the watcher's event filter refuses symlinks
/// too), so whatever is there is no document of this index.
///
/// What each stat says, and how it is read:
///
/// | found                                   | at an ancestor  | at the leaf     |
/// |-----------------------------------------|-----------------|-----------------|
/// | a real directory                        | keep descending | no document     |
/// | a regular file                          | no document     | **may be one**  |
/// | a symlink, or any other kind of entry   | no document     | no document     |
/// | `NotFound` / `NotADirectory`            | no document     | no document     |
/// | any other error (permissions, I/O)      | **may be one**  | **may be one**  |
///
/// "May be one" keeps the row. The error split is the one `get_document`
/// already uses ([`crate::server::path_probe_failed`]): those two kinds say the
/// path is not there, everything else says the look itself failed, and a
/// failed look is no reason to delete. A regular file at the leaf keeps the row
/// even if the walk would refuse it (a hard link, an excluded name): it is
/// inside the knowledge base, so leaving that to the next full run breaks no
/// rule, and deleting a row that was right is the worse mistake.
///
/// `kb_path` itself is taken as given. `groove serve` canonicalizes it before
/// the watcher sees it, and a knowledge base reached through a symlink is
/// still the knowledge base the operator chose. A mount point below it is a
/// real directory and is descended like any other. On a case-insensitive
/// volume the folded spelling can land on a file whose name differs in case;
/// that reads as "may be one" and keeps the row, the cautious side. The stats
/// are separate calls, so a directory swapped for a symlink between two of
/// them can still mislead one answer -- about one index row, which the next
/// event or full run corrects.
fn a_walked_document_may_be_at(kb_path: &Path, key: &str) -> bool {
    let mut at = kb_path.to_path_buf();
    let mut segments = key.split('/').peekable();
    while let Some(segment) = segments.next() {
        at.push(segment);
        let is_leaf = segments.peek().is_none();
        match std::fs::symlink_metadata(&at) {
            Ok(meta) if is_leaf => return meta.file_type().is_file(),
            Ok(meta) if meta.file_type().is_dir() => {}
            Ok(_) => return false,
            Err(e) => return crate::server::path_probe_failed(&e),
        }
    }
    // Unreachable for a key that `names_a_walked_path`; an empty key names no
    // document.
    false
}
