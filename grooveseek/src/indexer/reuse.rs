//! (AX-40) Keeping what the index already holds for a document: the stored vectors a
//! changed document's chunks can reuse ([`plan_vector_reuse`]), the meta-only write for
//! chunks that still match, the table-row decision, and the one-time re-read passes
//! ([`Reindex`]) that ask the same questions of unchanged rows.

use super::*;

/// (feature-69) How an updated document's chunks got their vectors: embedded this run, or
/// reused from the store. The one place these two counts are worked out (AGENTS.md "One
/// question gets one implementation"), and handed on as one value so the two cannot be
/// swapped. What the document did not send to the embedder was kept from the store.
///
/// (kuriya 621) Taken from the document's own plan ([`ChunkVectorCounts::of_plan`]), which
/// [`index_single_disk_entry_explained`] hands back through [`counted`], not from the
/// difference of the embedder's run-wide counter across the call.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct ChunkVectorCounts {
    pub(crate) embedded: u32,
    pub(crate) reused: u32,
}

impl ChunkVectorCounts {
    /// Splits a document's `chunks` given the inputs the embedder was sent for it. More
    /// inputs than chunks count as every chunk embedded.
    pub(crate) fn from_texts(chunks: u32, texts_embedded: u64) -> Self {
        let embedded = u32::try_from(texts_embedded).map_or(chunks, |texts| texts.min(chunks));
        Self {
            embedded,
            reused: chunks - embedded,
        }
    }

    /// (kuriya 621) The counts of a document written under `plan`: one chunk per entry of
    /// [`ReusePlan::sources`], of which the ones in [`ReusePlan::to_embed`] were embedded.
    pub(super) fn of_plan(plan: &ReusePlan) -> Self {
        let chunks = u32::try_from(plan.sources.len()).unwrap_or(u32::MAX);
        Self::from_texts(chunks, plan.to_embed.len() as u64)
    }

    /// (kuriya 621) The counts of a document whose `chunks` all kept their stored vectors (the
    /// meta-only write, which embeds nothing).
    pub(super) fn all_reused(chunks: u32) -> Self {
        Self::from_texts(chunks, 0)
    }
}

/// (kuriya 621) [`Reindexed`] with the [`ChunkVectorCounts`] of the document, for the callers in
/// this crate that report them ([`rebuild_index`], the watcher through
/// [`reindex_single_file_counted`]). Not a field of the public [`Reindexed`], whose shape stays
/// as it is.
#[derive(Debug)]
pub(crate) struct CountedReindex {
    pub(crate) reindexed: Reindexed,
    /// Zero unless [`Reindexed::result`] is [`SingleResult::Updated`].
    pub(crate) counts: ChunkVectorCounts,
}

/// (kuriya 621) Run the given indexing body, one that takes a `&mut` [`ChunkVectorCounts`], and
/// pair its result with the counts it set. The counts are kept only for a
/// [`SingleResult::Updated`] result, so a body that set them and then did not update the
/// document cannot report chunks it did not write.
pub(super) fn counted(
    index: impl FnOnce(&mut ChunkVectorCounts) -> Result<Reindexed>,
) -> Result<CountedReindex> {
    let mut counts = ChunkVectorCounts::default();
    let reindexed = index(&mut counts)?;
    let counts = if matches!(reindexed.result, SingleResult::Updated { .. }) {
        counts
    } else {
        ChunkVectorCounts::default()
    };
    Ok(CountedReindex { reindexed, counts })
}

/// embedding 入力を組む (feature-46 §4.5)。Static かつ context ありなら
/// `context\n\ncontent`、それ以外 (Off / context なし) は content のみ。
/// Anthropic 原典 cookbook の結合形 `f"{context}\n\n{chunk}"` に忠実。
pub(super) fn embed_input_for(chunk: &crate::parser::Chunk, mode: ContextMode) -> String {
    embed_input_from(&chunk.content, chunk.context.as_deref(), mode)
}

/// The embedding input for a chunk's `content` and `context` under `mode`, as
/// [`embed_input_for`] describes it. (feature-69) A newly parsed chunk and a row the index
/// already holds both go through here, so the two sides of [`plan_vector_reuse`] are built
/// the same way (spec R1, AGENTS.md "One question gets one implementation").
pub(super) fn embed_input_from(content: &str, context: Option<&str>, mode: ContextMode) -> String {
    match (mode, context) {
        (ContextMode::Static, Some(ctx)) if !ctx.trim().is_empty() => {
            format!("{ctx}\n\n{content}")
        }
        _ => content.to_string(),
    }
}

/// Where a new chunk's vector comes from (feature-69).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum VectorSource {
    /// The vector of the `j`-th stored row, kept as it is.
    Stored(usize),
    /// The `k`-th vector the embedder returns for [`ReusePlan::to_embed`].
    Computed(usize),
}

/// What [`plan_vector_reuse`] decided for a document's new chunks (feature-69).
#[derive(Debug, PartialEq, Eq)]
pub(super) struct ReusePlan {
    /// One entry per new chunk, in chunk order.
    pub(super) sources: Vec<VectorSource>,
    /// The new chunks still to embed: entry `k` is the chunk that takes
    /// [`VectorSource::Computed`] with `k`.
    pub(super) to_embed: Vec<usize>,
}

/// Matches each new chunk's embedding input against the stored ones (feature-69, spec R1, D1).
///
/// The match is on the input's bytes, not on the chunk's position: a chunk inserted above
/// shifts every later index and still keeps the vectors of the ones that did not change. When
/// the stored side holds the same input more than once, the first (the smallest `chunk_index`)
/// is taken; every copy carries the vector of the same string. New chunks that share an input
/// are not merged: each one is matched or embedded on its own, so [`ChunkVectorCounts`] counts
/// reused and embedded vectors per chunk.
pub(super) fn plan_vector_reuse(new_inputs: &[String], stored_inputs: &[String]) -> ReusePlan {
    let mut stored: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    for (j, input) in stored_inputs.iter().enumerate() {
        stored.entry(input.as_str()).or_insert(j);
    }
    let mut sources = Vec::with_capacity(new_inputs.len());
    let mut to_embed = Vec::new();
    for (i, input) in new_inputs.iter().enumerate() {
        match stored.get(input.as_str()) {
            Some(&j) => sources.push(VectorSource::Stored(j)),
            None => {
                sources.push(VectorSource::Computed(to_embed.len()));
                to_embed.push(i);
            }
        }
    }
    ReusePlan { sources, to_embed }
}

/// Whether the chunks the index holds for `rel` are `chunks`, compared the way `context_mode`
/// embeds them: heading and content under [`ContextMode::Off`], and the context too under
/// [`ContextMode::Static`], which embeds it. The level is not compared. `false` when the index
/// holds no chunk for `rel`, or the read fails.
///
/// A changed file whose new chunks match keeps its embeddings and has only its metadata
/// rewritten ([`index_single_disk_entry`]).
///
/// (feature-62) The one-time pass (ADR-0029, [`Reindex::Reparse`]) asks the same question of
/// an unchanged document it re-read: matching chunks mean the row already splits the way the
/// new rule does (AGENTS.md "One question gets one implementation").
pub(super) fn stored_chunks_match(
    db: &Database,
    rel: &str,
    chunks: &[crate::parser::Chunk],
    context_mode: ContextMode,
) -> bool {
    if context_mode == ContextMode::Static {
        db.chunk_texts_with_context_for_path(rel)
            .map(|existing| {
                !existing.is_empty()
                    && existing.len() == chunks.len()
                    && existing.iter().zip(chunks).all(|((eh, ec, ectx), c)| {
                        eh.as_deref() == c.heading.as_deref()
                            && *ec == c.content
                            && ectx.as_deref() == c.context.as_deref()
                    })
            })
            .unwrap_or(false)
    } else {
        db.chunk_texts_for_path(rel)
            .map(|existing| {
                !existing.is_empty()
                    && existing.len() == chunks.len()
                    && existing.iter().zip(chunks).all(|((eh, ec), c)| {
                        eh.as_deref() == c.heading.as_deref() && *ec == c.content
                    })
            })
            .unwrap_or(false)
    }
}

/// (feature-66) Stage one of a write of `chunks.table_rows` that re-embeds nothing, decided from
/// what the index holds for a document whose chunk text matches the parse
/// ([`stored_chunks_match`]): the content hash the row held when this was read, whether its
/// stored table rows already are the parse's ([`stored_table_rows_match`]), and the rows to
/// write, one per chunk.
///
/// Stage two reads the row again inside the transaction that writes, under `BEGIN IMMEDIATE`,
/// and writes only while the row still holds `stored_hash` and the chunk text
/// ([`table_rows_still_hold`]): [`write_reparsed_table_rows`] and [`write_matching_chunks_meta`].
/// So no row is left with table rows computed from a text it does not hold (spec R7.7).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct TableRowsDecision {
    pub(super) stored_hash: String,
    pub(super) matches: bool,
    pub(super) rows: Vec<Option<Vec<crate::parser::TableRowSpan>>>,
}

impl TableRowsDecision {
    /// [`Self::rows`] as [`Database::update_chunk_table_rows`] takes them.
    fn row_refs(&self) -> Vec<Option<&[crate::parser::TableRowSpan]>> {
        self.rows.iter().map(Option::as_deref).collect()
    }
}

/// (feature-66) Whether the index holds, for every chunk of `rel`, the table rows `chunks`
/// carry: NULL for `None`, the JSON of the same spans for `Some` (`[]` for `Some(vec![])`).
/// A value that is not JSON matches nothing. `false` when the counts differ or the read fails.
///
/// Asked after [`stored_chunks_match`] said the text matches, never instead of it: a chunk
/// whose table rows alone differ keeps its embedding.
fn stored_table_rows_match(db: &Database, rel: &str, chunks: &[crate::parser::Chunk]) -> bool {
    db.chunk_table_rows_for_path(rel)
        .map(|stored| {
            stored.len() == chunks.len()
                && stored.iter().zip(chunks).all(|(raw, c)| match raw {
                    None => c.table_rows.is_none(),
                    Some(json) => matches!(
                        serde_json::from_str::<Vec<crate::parser::TableRowSpan>>(json),
                        Ok(spans) if c.table_rows.as_ref() == Some(&spans)
                    ),
                })
        })
        .unwrap_or(false)
}

/// (feature-66) Stage one ([`TableRowsDecision`]) for `rel`, whose chunk text the index holds
/// as `chunks`. `None` when no chunk carries table rows and `rel` is not a `.docx` or `.pptx`
/// ([`marks_table_rows`]) -- every other format, whose column is left as it is -- or the index
/// holds no row for `rel`.
///
/// A `.docx` / `.pptx` whose chunks all carry `None` (an offset past `u32`) still gets a
/// decision, of NULL rows: its column is then written NULL, never left holding rows an older
/// text produced under the hash this write stores.
pub(super) fn decide_table_rows(
    db: &Database,
    rel: &str,
    chunks: &[crate::parser::Chunk],
) -> Result<Option<TableRowsDecision>> {
    if chunks.iter().all(|c| c.table_rows.is_none()) && !marks_table_rows(rel) {
        return Ok(None);
    }
    let Some(stored_hash) = db.get_document_hash(rel)? else {
        return Ok(None);
    };
    Ok(Some(TableRowsDecision {
        stored_hash,
        matches: stored_table_rows_match(db, rel, chunks),
        rows: chunks.iter().map(|c| c.table_rows.clone()).collect(),
    }))
}

/// (feature-66) Whether `rel` is a document whose parser marks table rows: a `.docx` or a
/// `.pptx`, by its extension, compared without case as [`Registry::by_extension`] picks the
/// parser.
fn marks_table_rows(rel: &str) -> bool {
    Path::new(rel)
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("docx") || ext.eq_ignore_ascii_case("pptx"))
}

/// (feature-66) Whether the row of `rel` still holds what stage one read: the content hash
/// `stored_hash` and the chunk text `chunks` ([`stored_chunks_match`]). Read first inside a
/// stage-two transaction, which is `BEGIN IMMEDIATE`, so no other writer can change the answer
/// before that transaction commits.
fn table_rows_still_hold(
    db: &Database,
    rel: &str,
    stored_hash: &str,
    chunks: &[crate::parser::Chunk],
    context_mode: ContextMode,
) -> Result<bool> {
    Ok(db.get_document_hash(rel)?.as_deref() == Some(stored_hash)
        && stored_chunks_match(db, rel, chunks, context_mode))
}

/// (feature-66) Stage two for a re-read ([`Reindex::Reparse`]) whose chunk text matches the row
/// and whose table rows do not: the parse's table rows are written, and nothing else, and the
/// document is [`SingleResult::Unchanged`] -- it was not re-embedded, so it is not counted as
/// updated. When the row no longer holds what stage one read, nothing is written and the
/// document is a [`SKIPPED_CHANGED_DURING_READ`] skip, which the run leaves unsettled.
pub(super) fn write_reparsed_table_rows(
    db: &Database,
    rel: &str,
    decision: &TableRowsDecision,
    chunks: &[crate::parser::Chunk],
    context_mode: ContextMode,
    frontmatter_unparsed: bool,
) -> Result<SingleResult> {
    let tx = db.begin_immediate_transaction()?;
    if !table_rows_still_hold(db, rel, &decision.stored_hash, chunks, context_mode)? {
        drop(tx);
        return Ok(SingleResult::Skipped {
            reason: SKIPPED_CHANGED_DURING_READ,
            frontmatter_unparsed,
        });
    }
    db.update_chunk_table_rows(rel, &decision.row_refs())?;
    tx.commit()?;
    Ok(SingleResult::Unchanged)
}

/// (#660, spec R4) The reason for what [`write_reparsed_table_rows`] returned, and only for that:
/// its one skip, [`SKIPPED_CHANGED_DURING_READ`], is [`SkipReason::ChangedDuringRead`]; its other
/// result, [`SingleResult::Unchanged`], has none. Not a general map from [`SingleResult`] to a
/// reason -- a [`SingleResult::Refused`] there could be either of two reasons, or none (spec
/// §5.7).
pub(super) fn explain_reparsed_table_rows(result: SingleResult) -> Reindexed {
    let reason = match &result {
        SingleResult::Skipped { reason, .. } if *reason == SKIPPED_CHANGED_DURING_READ => {
            Some(SkipReason::ChangedDuringRead)
        }
        _ => None,
    };
    Reindexed { result, reason }
}

/// The write for a changed file whose new chunks match the row's ([`stored_chunks_match`]):
/// the document's metadata and hash, its `document_fields` rows, its code positions, without
/// re-embedding. `None` when the metadata update touched no row (it went away in between): the
/// caller then writes the document the ordinary way.
///
/// (feature-66) And its table rows, when stage one ([`decide_table_rows`]) was read for it
/// (`table_rows`, a `.docx` or `.pptx`): the transaction is then `BEGIN IMMEDIATE` and first asks
/// whether the row still holds the hash and the text stage one saw ([`table_rows_still_hold`]).
/// If so the parse's table rows are written; if another writer moved the row in between they are
/// written NULL (not computed), since this write replaces the hash and a value it could not check
/// would otherwise stay behind the fast path. The metadata, the hash and the code positions are
/// written either way, as before. Every other format keeps the deferred transaction and its
/// column as it is.
#[allow(clippy::too_many_arguments)]
pub(super) fn write_matching_chunks_meta(
    db: &Database,
    entry: &DiskEntry,
    parsed: &crate::parser::ParsedDocument,
    table_rows: Option<&TableRowsDecision>,
    context_mode: ContextMode,
    declared: DeclaredSet<'_>,
    size_bytes: u64,
    topic: Option<&str>,
    category: Option<&str>,
) -> Result<Option<SingleResult>> {
    let tx = if table_rows.is_some() {
        db.begin_immediate_transaction()?
    } else {
        db.begin_transaction()?
    };
    let rows_hold = match table_rows {
        Some(decision) => table_rows_still_hold(
            db,
            &entry.rel,
            &decision.stored_hash,
            &parsed.chunks,
            context_mode,
        )?,
        None => false,
    };
    let updated = db.update_document_meta(
        &entry.rel,
        parsed.frontmatter.title.as_deref(),
        parsed.frontmatter.topic.as_deref().or(topic),
        category,
        parsed.frontmatter.depth.as_deref(),
        &parsed.frontmatter.tags,
        parsed.frontmatter.date.as_deref(),
        &entry.hash,
        size_bytes,
    )?;
    if !updated {
        // update が 0 行なら通常経路にフォールスルー (レース耐性)
        tx.commit()?;
        return Ok(None);
    }
    // (codex P2 round 9 on PR #291) Same "pending means leave it" rule as the
    // frontmatter-only path: a `None` here must not overwrite a document's rows
    // with an empty set just because the caller does not yet know the real one.
    write_declared_rows_or_mark_dirty(db, &entry.rel, &parsed.frontmatter.extra, declared)?;
    // (feature-56) The chunk texts match, so the embeddings still stand — but the
    // *positions* may not. Inserting a blank line above a function, or trimming a
    // comment short enough to be dropped as a thin gap, moves every definition below
    // it while leaving every chunk body identical. Without this the stored line
    // numbers would keep pointing at the previous version of the file until someone
    // re-indexed with --force, and a line number that is confidently wrong is worse
    // than one that is absent.
    if parsed.chunks.iter().any(|c| c.line_range.is_some()) {
        let metas: Vec<crate::db::CodeMeta<'_>> = parsed
            .chunks
            .iter()
            .map(|c| crate::db::CodeMeta {
                line_range: c.line_range,
                symbol_kind: c.symbol_kind.as_deref(),
            })
            .collect();
        db.update_chunk_code_meta(&entry.rel, &metas)?;
    }
    // (feature-66) Outside the code gate above: a `.docx` / `.pptx` chunk has no line range,
    // and its table rows can move while its text stays (a paragraph `a\tb` becoming a row).
    if let Some(decision) = table_rows {
        if rows_hold {
            db.update_chunk_table_rows(&entry.rel, &decision.row_refs())?;
        } else {
            db.update_chunk_table_rows(&entry.rel, &vec![None; decision.rows.len()])?;
        }
    }
    tx.commit()?;
    Ok(Some(SingleResult::Updated {
        chunks: parsed.chunks.len() as u32,
        frontmatter_unparsed: parsed.frontmatter_error.is_some(),
    }))
}

/// The stored content hash of `rel` when the parser the registry would hand it is the one for
/// `extension` -- by the parser, not the spelling, so `.MD` counts as `"md"` -- and the index
/// already holds a row for it; `None` otherwise.
///
/// The one-time frontmatter check (#251) asks this with `"md"` about every file it did not
/// index: only such a file can come back later with a matching hash and slip past the check
/// for good, and the hash is what says whether the bytes that were read this run are the ones
/// the row was written from.
///
/// (feature-62) Each one-time pass (ADR-0029) asks it with its format's extension: which rows
/// it re-reads ([`rereads_unchanged_as`]) and, at the end of the run, which rows it could not
/// settle ([`stale_row`]).
pub(super) fn indexed_hash_for(
    db: &Database,
    registry: &Registry,
    rel: &str,
    extension: &str,
) -> Result<Option<String>> {
    let ext = Path::new(rel)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("");
    let parsed_by_it = registry
        .by_extension(ext)
        .is_some_and(|p| p.extension() == extension);
    if !parsed_by_it {
        return Ok(None);
    }
    db.get_document_hash(rel)
}

/// (feature-62) Whether [`rebuild_index`] re-reads `entry` in the one-time `.docx` pass
/// ([`Reindex::Reparse`]): the pass is running (`refresh_docx`), the entry is not already
/// re-read past the hash fast path as a rename ([`Reindex::Renamed`], `renamed_new_paths`: a
/// rename under Static mode, or one across parsers),
/// and the index holds a `.docx` row for it whose hash is the scan's. A `.docx` whose content
/// changed is left to the ordinary path, which writes its new hash and metadata.
///
/// The one predicate both the per-document mode and the count in [`docx_reread_notice`] come
/// from; [`rebuild_index`] asks it once per entry and keeps the answer.
///
/// (feature-65) The `.docx` case of [`rereads_unchanged_as`], which every format's pass asks.
pub(super) fn rereads_unchanged_docx(
    refresh_docx: bool,
    db: &Database,
    registry: &Registry,
    entry: &DiskEntry,
    renamed_new_paths: &HashSet<String>,
) -> Result<bool> {
    rereads_unchanged_as("docx", refresh_docx, db, registry, entry, renamed_new_paths)
}

/// (feature-65) Whether the one-time pass of the format whose parser reports `extension`
/// re-reads `entry` ([`Reindex::Reparse`], ADR-0029): that pass is running (`stale`), the entry
/// is not already re-read as a rename ([`Reindex::Renamed`], `renamed_new_paths`), and the
/// index holds a row that
/// format's parser wrote for it ([`indexed_hash_for`]), whose hash is the scan's.
pub(super) fn rereads_unchanged_as(
    extension: &str,
    stale: bool,
    db: &Database,
    registry: &Registry,
    entry: &DiskEntry,
    renamed_new_paths: &HashSet<String>,
) -> Result<bool> {
    if !stale || renamed_new_paths.contains(&entry.rel) {
        return Ok(false);
    }
    let row = indexed_hash_for(db, registry, &entry.rel, extension)?;
    Ok(row.is_some_and(|row| row == entry.hash))
}

/// (feature-65) One format's one-time re-read pass in [`rebuild_index`] (ADR-0029): the parser
/// whose rows it re-reads, by [`crate::parser::Parser::extension`], and whether that format's
/// policy keys are stale (absent or another value) in this index.
#[derive(Clone, Copy, Debug)]
pub(super) struct OncePass {
    pub(super) extension: &'static str,
    pub(super) stale: bool,
}

/// (feature-65) Whether a row the run did not settle gets [`HASH_AWAITING_REPARSE`]: the index
/// holds a row for `rel` that the parser of a [stale](OncePass::stale) pass in `passes` wrote.
/// The rows of a format whose keys are at their value keep their hash, whatever another
/// format's pass does.
pub(super) fn stale_row(
    passes: &[OncePass],
    db: &Database,
    registry: &Registry,
    rel: &str,
) -> Result<bool> {
    for pass in passes.iter().filter(|p| p.stale) {
        if indexed_hash_for(db, registry, rel, pass.extension)?.is_some() {
            return Ok(true);
        }
    }
    Ok(false)
}

/// (feature-62) The line the one-time `.docx` pass writes before the document loop, through
/// [`progress::ProgressReporter::announce`], so a run that takes long re-embedding says why even
/// if it is stopped. ASCII only.
pub(super) fn docx_reread_notice(n: usize) -> String {
    format!(
        "Re-reading {n} unchanged .docx document(s) once: headings now come from word/styles.xml; only documents whose sections change are re-embedded"
    )
}

/// (feature-64) The line the one-time `.docx` pass writes instead of [`docx_reread_notice`] when
/// the heading and section keys are at their values and only [`DOCX_TABLE_POLICY`] is not, so
/// the reason it gives is the one that applies. ASCII only, and it opens the same way.
pub(super) fn docx_table_reread_notice(n: usize) -> String {
    format!(
        "Re-reading {n} unchanged .docx document(s) once: table rows are now one line each, cells separated by tabs; only documents whose sections change are re-embedded"
    )
}

/// (feature-65) The line the one-time `.pptx` pass writes before the document loop, after the
/// `.docx` one when both passes run. ASCII only, and it opens the way [`docx_reread_notice`]
/// does.
pub(super) fn pptx_reread_notice(n: usize) -> String {
    format!(
        "Re-reading {n} unchanged .pptx document(s) once: table rows are now one line each, cells separated by tabs, and line breaks are kept; only documents whose slides change are re-embedded"
    )
}

/// (feature-66) The line the one-time `.docx` pass writes instead of [`docx_reread_notice`] when
/// every `.docx` key but [`DOCX_TABLE_ROWS_POLICY`] is at its value. ASCII only, and it opens the
/// same way.
pub(super) fn docx_rows_reread_notice(n: usize) -> String {
    format!(
        "Re-reading {n} unchanged .docx document(s) once: table rows are now marked in each chunk; only documents whose sections change are re-embedded"
    )
}

/// (feature-66) The line the one-time `.pptx` pass writes instead of [`pptx_reread_notice`] when
/// [`PPTX_TABLE_BREAK_POLICY`] is at its value and [`PPTX_TABLE_ROWS_POLICY`] is not. ASCII only,
/// and it opens the same way.
pub(super) fn pptx_rows_reread_notice(n: usize) -> String {
    format!(
        "Re-reading {n} unchanged .pptx document(s) once: table rows are now marked in each chunk; only documents whose slides change are re-embedded"
    )
}

/// How [`index_single_disk_entry`] treats a file whose content hash the index
/// already holds.
///
/// `force` and the one-time frontmatter check (#251) are not two independent
/// switches: under `Force` every file is re-parsed and re-embedded, so there is
/// nothing left for the check to do. Folding them into one value says so in
/// the type instead of leaving a `(true, true)` that means the same as
/// `(true, false)`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) enum Reindex {
    /// Re-parse and re-embed regardless of the stored hash (`--force`).
    Force,
    /// (kuriya 608) The re-read of a renamed entry whose bytes did not change: a rename under
    /// static context mode, whose path-derived breadcrumb may have moved, or one across a
    /// parser. The hash fast path is skipped, as under [`Reindex::Force`], but the stored
    /// vectors are read and matched as for any changed file ([`plan_vector_reuse`]), so only a
    /// chunk whose embedding input changed with the path is embedded again. The meta-only
    /// write is not taken: the row is rewritten whole, and a file that yields chunks comes
    /// back [`SingleResult::Updated`]. Not [`Reindex::Reparse`], which writes nothing when the
    /// chunks match and so would leave the moved row's path-derived fields as they were.
    Renamed,
    /// Skip a file whose hash matches. With `check_frontmatter` set, an
    /// unchanged Markdown file is still read once so a frontmatter that fails
    /// to parse can be tagged without re-embedding ([`FRONTMATTER_POLICY`]).
    /// With `refresh_fields` set it is read once so its `document_fields`
    /// rows follow the keys the schema now declares (feature-58). Either
    /// reason opens the file; each decides its own write.
    Incremental {
        check_frontmatter: bool,
        refresh_fields: bool,
    },
    /// (feature-62) Read and parse a file whose hash matches: the one-time pass (ADR-0029) of
    /// [`rebuild_index`] ([`DOCX_HEADING_POLICY`], [`PPTX_TABLE_BREAK_POLICY`]). Unlike
    /// [`Reindex::Incremental`], a matching hash does not return early; unlike
    /// [`Reindex::Force`], chunks that still match the row's are neither re-embedded nor
    /// written. Chunks that differ are written the ordinary way, and no chunks at all drop the
    /// row, since `--force` would leave none.
    Reparse,
    /// (#543) One file a watcher event names ([`reindex_single_file`]): [`Reindex::Incremental`]
    /// with neither one-time check, except that a file which now yields no chunks loses its row,
    /// as under [`Reindex::Reparse`], and comes back as [`SingleResult::NothingToIndex`]. Without
    /// this, a document emptied in place (or, on macOS, removed and written again, which FSEvents
    /// folds into one change) kept its earlier text's chunks searchable.
    Event,
}
