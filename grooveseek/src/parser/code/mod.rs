//! (feature-56) Source code parsed into one chunk per definition.
//!
//! The unit is a `@definition.*` capture from the grammar's `tags.scm`, not a heading: a
//! function, a struct, a class. Everything a definition does not cover — imports, top-level
//! statements, the `impl` block's own braces, regions the parser could not understand — is
//! filled in with line-based chunks, so a file contributes every byte it has to the index.
//!
//! Why gap-filling rather than falling back to the plain-text parser when a file fails to
//! parse cleanly: [`crate::parser::TxtParser`] turns a whole file into a single chunk, which is
//! the shape this design exists to avoid. A file with a syntax error still has definitions
//! around the broken region, and those stay useful.
//!
//! The language-specific knowledge lives entirely in the grammar and its tags query. This
//! module walks the tree by field name (`name` / `type` / `trait`) and by node kind substring
//! (`comment`), both of which hold across grammars, so adding a language stays a matter of
//! supplying data rather than code.

// Deliberately not behind `grammar-rust`: which grammars are compiled in and whether groove
// can load one from disk are separate questions, and a build with no compiled-in grammar is
// exactly the build that has nothing but plugins to offer.
pub(crate) mod plugin;
#[cfg(feature = "grammar-rust")]
pub(crate) mod static_rust;

use std::ops::Range;
use std::sync::Arc;

use anyhow::{Context, Result};
use tree_sitter::{Language, Node};
use tree_sitter_tags::{TagsConfiguration, TagsContext};

use super::{Chunk, Frontmatter, ParsedDocument, Parser, build_context};

/// Raw-byte ceiling for a source file (1 MiB).
///
/// Deliberately the same value as `get_document`'s cap, so "the code parser refuses this file"
/// and "you cannot read this file back" agree. The comparison is `>` for the same reason: a
/// file of exactly the cap is readable, so it must also be parseable.
///
/// This is the defence against tree-sitter's allocator, which calls `abort()` on OOM rather
/// than unwinding — [`crate::parser::ParserExt`]'s panic guard cannot catch that, so the file
/// has to be refused before the parser ever sees it.
pub(crate) const MAX_RAW_CODE_BYTES: u64 = 1024 * 1024;

/// Chunks one file may contribute before the parser gives up and warns.
///
/// A policy number, not a measured one: in a prose knowledge base with some code mixed in, a
/// file that wants more than this is a file that should not have been indexed.
const MAX_CHUNKS_PER_FILE: usize = 512;

/// Default budget for one chunk, counted in non-whitespace characters.
///
/// Above the largest definition the spike measured (3404), so ordinary functions stay whole:
/// a hard split cuts a function in half, and half a function retrieved on its own has lost
/// the context that made it worth retrieving.
pub(crate) const DEFAULT_MAX_CHUNK_CHARS: usize = 3500;

/// Ancestors a definition may sit under before the file is chunked by lines instead.
///
/// [`scope_chain`] walks from a definition up to the root for every definition in the file,
/// and `Node::parent` is a search rather than a pointer, so the cost grows with the cube of
/// the nesting depth. On one line of `mod a{` repeated, indexing took 316 ms at 125 levels,
/// 8,191 ms at 500 and 63,989 ms at 1000 — while the file stayed under 10 KB and so never came
/// near [`MAX_RAW_CODE_BYTES`], the only bound that existed. [`crate::indexer::rebuild_index`]
/// holds the embedder and database locks for its whole run, so one such file in a knowledge
/// base stops every request the server has.
///
/// The bound is on the input, not on a clock: a wall-clock budget would let the same file
/// produce different chunks on different machines, and those chunks are the index. Definitions
/// in this repository's own sources sit under at most 8 ancestors, so this leaves real code
/// eight times the room it uses.
//
// Measured 2026-09-03, release build, both binaries run alternately on the same machine:
//   depth:  target/release/groove.exe --config <cfg> index --force  (fixtures of `mod a{` x N)
//   real code: parse every file under grooveseek/src and record the longest walk (62 files)
const MAX_DEFINITION_SCOPE_DEPTH: usize = 64;

/// Below this many characters (after trimming) a fragment is not worth a chunk of its own.
///
/// The same threshold the quality filter uses for "too short to be worth much", reused rather
/// than invented: the filter alone is not enough, because a two-line fragment under the
/// threshold still scores above the default cutoff and would survive.
const MIN_FRAGMENT_CHARS: usize = 30;

/// A grammar plus the tags query that goes with it, ready to parse.
///
/// Built once per registry. Both fields come from the same source — a compiled-in grammar
/// crate today, a plugin later — which is what keeps a query from being applied to a grammar
/// it was not written for.
pub(crate) struct LoadedGrammar {
    /// Lowercase language name, used in the `lang:` tag (`"rust"`).
    pub(crate) name: &'static str,
    language: Language,
    config: TagsConfiguration,
}

impl LoadedGrammar {
    // A build with no grammar compiled in has no way to construct one of these, but the
    // chunker still compiles — which is the point: turning a language on is a Cargo feature,
    // not a code change. The plugin loader will be a second caller.
    #[cfg_attr(not(feature = "grammar-rust"), allow(dead_code))]
    pub(crate) fn new(name: &'static str, language: Language, tags_query: &str) -> Result<Self> {
        let config = TagsConfiguration::new(language.clone(), tags_query, "")
            .map_err(|e| anyhow::anyhow!("grammar {name}: tags query rejected: {e:?}"))?;
        Ok(Self {
            name,
            language,
            config,
        })
    }
}

/// One parser instance per extension, holding the grammar it parses with.
pub struct CodeParser {
    grammar: Arc<LoadedGrammar>,
    extension: &'static str,
    max_chunk_chars: usize,
}

impl CodeParser {
    #[cfg_attr(not(feature = "grammar-rust"), allow(dead_code))]
    pub(crate) fn new(
        grammar: Arc<LoadedGrammar>,
        extension: &'static str,
        max_chunk_chars: usize,
    ) -> Self {
        Self {
            grammar,
            extension,
            max_chunk_chars,
        }
    }
}

impl Parser for CodeParser {
    fn extension(&self) -> &'static str {
        self.extension
    }

    /// Trait-contract fallback: in production this parser is only ever reached through
    /// [`crate::parser::ParserExt::parse_bytes`], which calls
    /// [`Parser::parse_bytes_inner`].
    ///
    /// Unlike the binary parsers, this returns an *empty* document rather than
    /// [`super::single_text_chunk`]: wrapping a whole source file into one chunk is the shape
    /// this module exists to avoid, so it must not be reachable by accident. Does not panic.
    fn parse(&self, _raw: &str, _path_hint: &str, _exclude_headings: &[&str]) -> ParsedDocument {
        ParsedDocument {
            frontmatter: Frontmatter::default(),
            chunks: Vec::new(),
            raw_content: String::new(),
        }
    }

    fn parse_bytes_inner(
        &self,
        bytes: &[u8],
        path_hint: &str,
        _exclude_headings: &[&str],
    ) -> Result<ParsedDocument> {
        if bytes.len() as u64 > MAX_RAW_CODE_BYTES {
            anyhow::bail!(
                "{path_hint}: source file is {} bytes, over the {} byte limit for code",
                bytes.len(),
                MAX_RAW_CODE_BYTES
            );
        }
        // Validated up front so the rest can slice on byte offsets from the tree without
        // re-checking. Non-UTF-8 is a per-file skip, matching the default implementation.
        let text =
            std::str::from_utf8(bytes).with_context(|| format!("{path_hint}: not valid UTF-8"))?;
        chunk_source(&self.grammar, self.max_chunk_chars, bytes, text, path_hint)
    }
}

/// A definition and the byte range it owns.
struct Def {
    kind: String,
    name: String,
    /// The definition node plus any doc comment immediately above it. Gap-filling works off this, so a
    /// doc comment cannot end up in both the definition's chunk and a gap chunk.
    covered: Range<usize>,
    scope: Vec<String>,
    children: Vec<usize>,
    depth: usize,
}

/// A chunk-to-be, before indices and line numbers are assigned.
#[derive(Clone)]
struct Piece {
    range: Range<usize>,
    heading: Option<String>,
    level: Option<u8>,
    symbol_kind: Option<String>,
    context_parts: Vec<String>,
    /// Fragments (gaps, interstitial bits inside a large definition) are droppable; whole
    /// definitions are not.
    droppable: bool,
}

fn chunk_source(
    grammar: &LoadedGrammar,
    budget: usize,
    bytes: &[u8],
    text: &str,
    path_hint: &str,
) -> Result<ParsedDocument> {
    chunk_source_capped(
        grammar,
        budget,
        bytes,
        text,
        path_hint,
        MAX_DEFINITION_SCOPE_DEPTH,
    )
}

/// [`chunk_source`] with the scope bound injected, so a unit test can take the fallback
/// without building a source deep enough to take it for real (the same split
/// [`super::pdf`] uses for its page budgets).
fn chunk_source_capped(
    grammar: &LoadedGrammar,
    budget: usize,
    bytes: &[u8],
    text: &str,
    path_hint: &str,
    scope_limit: usize,
) -> Result<ParsedDocument> {
    let title = super::txt::derive_title_pub(path_hint);
    let mut ts = tree_sitter::Parser::new();
    ts.set_language(&grammar.language)
        .map_err(|e| anyhow::anyhow!("{path_hint}: grammar rejected by the runtime: {e}"))?;
    let tree = ts
        .parse(bytes, None)
        .ok_or_else(|| anyhow::anyhow!("{path_hint}: parse returned no tree"))?;
    let root = tree.root_node();

    let mut ctx = TagsContext::new();
    let (tags, has_error) = ctx
        .generate_tags(&grammar.config, bytes, None)
        .map_err(|e| anyhow::anyhow!("{path_hint}: tags query failed: {e:?}"))?;

    let mut defs: Vec<Def> = Vec::new();
    let mut too_deep = false;
    for tag in tags {
        let tag = tag.map_err(|e| anyhow::anyhow!("{path_hint}: tag: {e:?}"))?;
        if !tag.is_definition {
            continue;
        }
        let kind = grammar
            .config
            .syntax_type_name(tag.syntax_type_id)
            .to_string();
        let name = text
            .get(tag.name_range.clone())
            .unwrap_or_default()
            .to_string();
        let node = root.descendant_for_byte_range(tag.range.start, tag.range.end);
        let start = node
            .map(|n| doc_comment_start(n, bytes))
            .unwrap_or(tag.range.start);
        // Stop at the first definition nested past the bound rather than finishing the file:
        // every remaining definition would pay the same walk, and the answer is already known.
        let scope = match node {
            Some(n) => match scope_chain(n, text, scope_limit) {
                Some(scope) => scope,
                None => {
                    too_deep = true;
                    break;
                }
            },
            None => Vec::new(),
        };
        defs.push(Def {
            kind,
            name,
            covered: start..tag.range.end,
            scope,
            children: Vec::new(),
            depth: 0,
        });
    }

    let mut pieces: Vec<Piece> = Vec::new();
    if too_deep {
        // The file still contributes every byte it has (ADR-0012); it contributes them as
        // lines, which is the shape a region no definition covers already gets. Falling back
        // to the plain-text parser instead would make the whole file one chunk, the shape this
        // module exists to avoid.
        tracing::warn!(
            path = path_hint,
            limit = scope_limit,
            "a definition is nested deeper than the limit; chunking this file by lines instead"
        );
        defs.clear();
        let context_parts: Vec<String> = title.iter().cloned().collect();
        // Not droppable, unlike an ordinary gap. A gap is the frame around content that was
        // chunked as a definition; here there is no such content, so a thin last piece is the
        // end of the file rather than a stray closing brace beside something that survived.
        push_line_pieces(
            0..text.len(),
            &context_parts,
            text,
            budget,
            false,
            &mut pieces,
        );
    } else {
        link_containment(&mut defs);
        let roots: Vec<usize> = (0..defs.len()).filter(|i| defs[*i].depth == 0).collect();
        for i in &roots {
            emit_def(*i, &defs, text, budget, &title, &mut pieces);
        }
        fill_gaps(&roots, &defs, text, budget, &title, &mut pieces);
    }

    pieces.sort_by_key(|p| p.range.start);
    let kept = drop_thin_fragments(pieces, text);
    let (kept, truncated) = if kept.len() > MAX_CHUNKS_PER_FILE {
        (kept[..MAX_CHUNKS_PER_FILE].to_vec(), true)
    } else {
        (kept, false)
    };
    if truncated {
        tracing::warn!(
            path = path_hint,
            limit = MAX_CHUNKS_PER_FILE,
            "code file produced more chunks than the per-file limit; keeping the first ones"
        );
    }

    let starts = line_starts(text);
    let chunks: Vec<Chunk> = kept
        .into_iter()
        .enumerate()
        .map(|(index, p)| {
            let content = text
                .get(p.range.clone())
                .unwrap_or_default()
                .trim_end()
                .to_string();
            let parts: Vec<&str> = p.context_parts.iter().map(|s| s.as_str()).collect();
            Chunk {
                index,
                heading: p.heading,
                level: p.level,
                content,
                context: build_context(&parts),
                line_range: Some((
                    line_of(&starts, p.range.start),
                    line_of(&starts, p.range.end.saturating_sub(1)),
                )),
                symbol_kind: p.symbol_kind,
            }
        })
        .collect();

    let mut tags_out = vec!["code".to_string(), format!("lang:{}", grammar.name)];
    if has_error {
        tags_out.push("parse:degraded".to_string());
    }
    // A separate tag from `parse:degraded`, which answers a different question: that one says
    // the grammar could not read part of the file, this one says groove declined to chunk a
    // file it could read. A caller filtering for one does not want the other.
    if too_deep {
        tags_out.push("parse:too-deep".to_string());
    }
    // The source, verbatim -- not the chunks rejoined, which is what the prose parsers do.
    // Rejoining is right when chunks are a lossy view of the document, because then it is the
    // only text that matches what was indexed. Here the chunks already cover every byte, so
    // rejoining would only add blank lines and trim indentation off the ends: `get_document`
    // would hand back something that no longer compiles.
    let raw_content = text.to_string();
    Ok(ParsedDocument {
        frontmatter: Frontmatter {
            title,
            tags: tags_out,
            ..Frontmatter::default()
        },
        chunks,
        raw_content,
    })
}

/// Extend a definition's start backwards over the doc comment written directly above it.
///
/// The tags query does not capture doc comments for any grammar shipped upstream (`Tag::docs`
/// comes back empty), so they have to be picked up from the tree. A blank line ends the run:
/// a comment separated from the definition is commentary on the file, not on the definition.
fn doc_comment_start(node: Node, src: &[u8]) -> usize {
    let mut start = node.start_byte();
    let mut cursor = node.prev_sibling();
    while let Some(prev) = cursor {
        if !prev.kind().contains("comment") {
            break;
        }
        let between = src.get(prev.end_byte()..start).unwrap_or_default();
        if between.iter().filter(|b| **b == b'\n').count() > 1 {
            break;
        }
        start = prev.start_byte();
        cursor = prev.prev_sibling();
    }
    start
}

/// Names of the enclosing scopes, outermost first, or `None` when the definition sits under
/// more than `limit` ancestors.
///
/// Walks real parents rather than the definition tree because the two disagree: Rust's tags
/// query captures `impl` blocks as references, not definitions, so `impl Database` never
/// becomes a definition node — yet it is exactly the context that tells two `open` methods
/// apart.
///
/// This walk is also what makes deeply nested sources expensive, which is why it counts its
/// steps rather than trusting the input: see [`MAX_DEFINITION_SCOPE_DEPTH`].
fn scope_chain(node: Node, text: &str, limit: usize) -> Option<Vec<String>> {
    let mut out = Vec::new();
    let mut steps = 0usize;
    let mut cursor = node.parent();
    while let Some(parent) = cursor {
        steps += 1;
        if steps > limit {
            return None;
        }
        for field in ["name", "type", "trait"] {
            if let Some(child) = parent.child_by_field_name(field) {
                if let Some(s) = text.get(child.byte_range()) {
                    out.push(s.trim().to_string());
                }
                break;
            }
        }
        cursor = parent.parent();
    }
    out.reverse();
    Some(out)
}

/// Turn the flat definition list into a containment forest.
fn link_containment(defs: &mut [Def]) {
    let mut order: Vec<usize> = (0..defs.len()).collect();
    order.sort_by(|a, b| {
        defs[*a]
            .covered
            .start
            .cmp(&defs[*b].covered.start)
            .then(defs[*b].covered.end.cmp(&defs[*a].covered.end))
    });
    let mut stack: Vec<usize> = Vec::new();
    for i in order {
        while let Some(&top) = stack.last() {
            if defs[top].covered.end <= defs[i].covered.start {
                stack.pop();
            } else {
                break;
            }
        }
        if let Some(&parent) = stack.last() {
            defs[parent].children.push(i);
            defs[i].depth = defs[parent].depth + 1;
        }
        stack.push(i);
    }
}

fn emit_def(
    idx: usize,
    defs: &[Def],
    text: &str,
    budget: usize,
    title: &Option<String>,
    out: &mut Vec<Piece>,
) {
    let def = &defs[idx];
    let heading = format!("{} {}", def.kind, def.name);
    let level = u8::try_from(def.depth.saturating_add(2)).unwrap_or(u8::MAX);
    let mut context_parts: Vec<String> = Vec::new();
    if let Some(t) = title {
        context_parts.push(t.clone());
    }
    context_parts.extend(def.scope.iter().cloned());
    context_parts.push(heading.clone());

    let whole = || Piece {
        range: def.covered.clone(),
        heading: Some(heading.clone()),
        level: Some(level),
        symbol_kind: Some(def.kind.clone()),
        context_parts: context_parts.clone(),
        droppable: false,
    };

    if non_ws(text, &def.covered) <= budget {
        out.push(whole());
        return;
    }

    if def.children.is_empty() {
        // Methods and functions hold no nested definitions, so this is the common path for an
        // oversized body rather than an exceptional one.
        for range in split_definition_by_lines(text, &def.covered, budget) {
            out.push(Piece {
                range,
                heading: Some(heading.clone()),
                level: Some(level),
                symbol_kind: Some(def.kind.clone()),
                context_parts: context_parts.clone(),
                droppable: false,
            });
        }
        return;
    }

    let mut children: Vec<usize> = def.children.clone();
    children.sort_by_key(|c| defs[*c].covered.start);
    let mut cursor = def.covered.start;
    for child in children {
        let child_start = defs[child].covered.start;
        if cursor < child_start {
            push_interstitial(
                cursor..child_start,
                &heading,
                level,
                &def.kind,
                &context_parts,
                text,
                budget,
                out,
            );
        }
        emit_def(child, defs, text, budget, title, out);
        cursor = defs[child].covered.end.max(cursor);
    }
    if cursor < def.covered.end {
        push_interstitial(
            cursor..def.covered.end,
            &heading,
            level,
            &def.kind,
            &context_parts,
            text,
            budget,
            out,
        );
    }
}

/// The parts of a large definition that its nested definitions do not cover: the signature
/// above the first one, whatever sits between them, the closing brace below the last.
#[allow(clippy::too_many_arguments)]
fn push_interstitial(
    range: Range<usize>,
    heading: &str,
    level: u8,
    kind: &str,
    context_parts: &[String],
    text: &str,
    budget: usize,
    out: &mut Vec<Piece>,
) {
    for r in split_by_lines(text, &range, budget) {
        out.push(Piece {
            range: r,
            heading: Some(heading.to_string()),
            level: Some(level),
            symbol_kind: Some(kind.to_string()),
            context_parts: context_parts.to_vec(),
            droppable: true,
        });
    }
}

/// Everything no definition covers: imports, top-level statements, the frame of an `impl`
/// block, regions the grammar could not parse.
fn fill_gaps(
    roots: &[usize],
    defs: &[Def],
    text: &str,
    budget: usize,
    title: &Option<String>,
    out: &mut Vec<Piece>,
) {
    let mut spans: Vec<Range<usize>> = roots.iter().map(|i| defs[*i].covered.clone()).collect();
    spans.sort_by_key(|r| r.start);
    let context_parts: Vec<String> = title.iter().cloned().collect();
    let mut cursor = 0usize;
    for span in spans {
        if cursor < span.start {
            push_line_pieces(cursor..span.start, &context_parts, text, budget, true, out);
        }
        cursor = span.end.max(cursor);
    }
    if cursor < text.len() {
        push_line_pieces(cursor..text.len(), &context_parts, text, budget, true, out);
    }
}

/// Headingless pieces covering `range`, split to the budget.
///
/// `droppable` decides what [`drop_thin_fragments`] may take back: a gap is droppable because
/// a closing brace beside a definition that survived is noise, but the same pieces standing in
/// for the whole file are the file.
fn push_line_pieces(
    range: Range<usize>,
    context_parts: &[String],
    text: &str,
    budget: usize,
    droppable: bool,
    out: &mut Vec<Piece>,
) {
    for r in split_by_lines(text, &range, budget) {
        out.push(Piece {
            range: r,
            heading: None,
            level: None,
            symbol_kind: None,
            context_parts: context_parts.to_vec(),
            droppable,
        });
    }
}

/// Drop fragments too small to be worth a chunk — unless dropping them would leave the file
/// with nothing at all, in which case they are all it has.
fn drop_thin_fragments(pieces: Vec<Piece>, text: &str) -> Vec<Piece> {
    let kept: Vec<Piece> = pieces
        .iter()
        .filter(|p| !p.droppable || fragment_chars(text, &p.range) >= MIN_FRAGMENT_CHARS)
        .cloned()
        .collect();
    if kept.is_empty() {
        pieces
            .into_iter()
            .filter(|p| fragment_chars(text, &p.range) > 0)
            .collect()
    } else {
        kept
    }
}

fn fragment_chars(text: &str, range: &Range<usize>) -> usize {
    text.get(range.clone())
        .unwrap_or_default()
        .trim()
        .chars()
        .count()
}

fn non_ws(text: &str, range: &Range<usize>) -> usize {
    text.get(range.clone())
        .unwrap_or_default()
        .chars()
        .filter(|c| !c.is_whitespace())
        .count()
}

/// Split a byte range on line boundaries so that no piece exceeds the budget.
///
/// A line longer than the budget on its own is kept whole: cutting mid-line would produce a
/// chunk that starts in the middle of a token.
fn split_by_lines(text: &str, range: &Range<usize>, budget: usize) -> Vec<Range<usize>> {
    let slice = match text.get(range.clone()) {
        Some(s) if !s.trim().is_empty() => s,
        _ => return Vec::new(),
    };
    let mut out = Vec::new();
    let mut start = range.start;
    let mut used = 0usize;
    let mut cursor = range.start;
    for line in slice.split_inclusive('\n') {
        let weight = line.chars().filter(|c| !c.is_whitespace()).count();
        if used > 0 && used + weight > budget {
            out.push(start..cursor);
            start = cursor;
            used = 0;
        }
        used += weight;
        cursor += line.len();
    }
    if start < range.end {
        out.push(start..range.end);
    }
    out
}

/// [`split_by_lines`], with a final piece too thin to stand alone folded back onto the piece
/// before it.
///
/// A cut lands wherever the next line would overrun the budget, so the last piece can end up
/// holding nothing but a closing brace. For a piece of a **definition** that is a chunk worth
/// removing: it carries the definition's heading and kind — and bm25 weights the heading —
/// while its text says nothing. Since [ADR-0015] made a definition exempt from the length
/// penalties, the quality filter no longer hides it either.
///
/// **Not only the last piece.** A cut also lands *before* a line that overruns the budget on
/// its own, and what it pushes out is whatever had accumulated so far — which can be a single
/// short line. A signature followed by one very long line leaves the signature as a thin piece
/// with a piece after it, so every piece is checked rather than just the tail.
///
/// A thin piece is folded forward, onto the piece that follows it, and only the last one is
/// folded backwards, because nothing follows it to absorb it.
///
/// **Definitions only**, which is why this is not folded into [`split_by_lines`] itself. Of the
/// three callers, this is the only one whose thin pieces are *both* non-droppable and carrying
/// a [`crate::parser::Chunk::symbol_kind`], and it takes both to be a problem:
///
/// - [`push_interstitial`] does set a [`Piece::symbol_kind`], but its pieces are droppable, so
///   [`drop_thin_fragments`] removes a thin one before it ever reaches the index.
/// - The gap and fallback caller emits no [`crate::parser::Chunk::symbol_kind`], so a thin piece there keeps taking the
///   length penalties and is stored but never returned — and ADR-0012 wants it *kept*, which
///   this module's own `the_line_fallback_keeps_a_tail_too_thin_to_survive_as_a_gap` test pins.
///   Merging there would break that test, correctly.
///
/// The floor is [`MIN_FRAGMENT_CHARS`], the one [`drop_thin_fragments`] applies to gap
/// fragments. This merges where that drops, for the same ADR-0012 reason: a piece cut out of a
/// definition is not droppable, so its bytes have to go somewhere.
///
/// The invariant it buys: **a chunk carrying a [`crate::parser::Chunk::symbol_kind`] and shorter than
/// [`MIN_FRAGMENT_CHARS`] is a whole short definition.** That is the assumption the quality
/// filter's definition exemption rests on.
///
/// [ADR-0015]: https://github.com/alphabet-h/grooveseek/blob/main/docs/decisions/0015-let-a-definition-be-short.md
fn split_definition_by_lines(text: &str, range: &Range<usize>, budget: usize) -> Vec<Range<usize>> {
    let mut out: Vec<Range<usize>> = Vec::new();
    for piece in split_by_lines(text, range, budget) {
        match out.last_mut() {
            // The piece already collected is too thin to stand on its own, so this one extends
            // it rather than starting another. Repeated, this absorbs a run of thin pieces.
            Some(prev) if fragment_chars(text, prev) < MIN_FRAGMENT_CHARS => prev.end = piece.end,
            _ => out.push(piece),
        }
    }
    // Nothing follows the last piece, so it folds the other way.
    if out.len() > 1 && fragment_chars(text, &out[out.len() - 1]) < MIN_FRAGMENT_CHARS {
        let last = out.pop().expect("checked len > 1");
        let prev = out.len() - 1;
        out[prev].end = last.end;
    }
    out
}

fn line_starts(text: &str) -> Vec<usize> {
    let mut out = vec![0usize];
    for (i, b) in text.bytes().enumerate() {
        if b == b'\n' {
            out.push(i + 1);
        }
    }
    out
}

/// 1-based line number for a byte offset.
fn line_of(starts: &[usize], offset: usize) -> u32 {
    let idx = match starts.binary_search(&offset) {
        Ok(i) => i,
        Err(i) => i.saturating_sub(1),
    };
    u32::try_from(idx + 1).unwrap_or(u32::MAX)
}

#[cfg(all(test, feature = "grammar-rust"))]
mod tests {
    use super::*;
    use crate::parser::ParserExt;

    const SRC: &str = r#"use std::collections::BTreeMap;
use std::fmt::Debug;
use std::sync::Arc;

/// Adds two numbers together.
///
/// The doc comment belongs to the chunk, not to the gap above it.
pub fn add(a: usize, b: usize) -> usize {
    a + b
}

pub struct Counter {
    hits: usize,
}

impl Counter {
    /// Records one hit.
    pub fn bump(&mut self) {
        self.hits += 1;
    }

    pub fn total(&self) -> usize {
        self.hits
    }
}
"#;

    fn parse(src: &str, budget: usize) -> ParsedDocument {
        let grammar = static_rust::grammar().expect("rust grammar builds");
        let parser = CodeParser::new(grammar, "rs", budget);
        parser
            .parse_bytes(src.as_bytes(), "src/lib.rs", &[])
            .expect("parses")
    }

    fn find<'a>(doc: &'a ParsedDocument, heading: &str) -> &'a Chunk {
        doc.chunks
            .iter()
            .find(|c| c.heading.as_deref() == Some(heading))
            .unwrap_or_else(|| panic!("no chunk headed {heading:?}"))
    }

    #[test]
    fn every_definition_becomes_its_own_chunk() {
        let doc = parse(SRC, DEFAULT_MAX_CHUNK_CHARS);
        let headings: Vec<&str> = doc
            .chunks
            .iter()
            .filter_map(|c| c.heading.as_deref())
            .collect();
        assert!(headings.contains(&"function add"), "got {headings:?}");
        assert!(headings.contains(&"class Counter"), "got {headings:?}");
        assert!(headings.contains(&"method bump"), "got {headings:?}");
        assert!(headings.contains(&"method total"), "got {headings:?}");
    }

    #[test]
    fn the_symbol_kind_is_the_tags_vocabulary_not_the_rust_keyword() {
        let doc = parse(SRC, DEFAULT_MAX_CHUNK_CHARS);
        // `struct` is `class` to the tags query, which is the whole point of storing what the
        // grammar said rather than translating it.
        assert_eq!(
            find(&doc, "class Counter").symbol_kind.as_deref(),
            Some("class")
        );
        assert_eq!(
            find(&doc, "method bump").symbol_kind.as_deref(),
            Some("method")
        );
    }

    #[test]
    fn a_doc_comment_joins_its_definition_and_appears_nowhere_else() {
        let doc = parse(SRC, DEFAULT_MAX_CHUNK_CHARS);
        let add = find(&doc, "function add");
        assert!(
            add.content.contains("Adds two numbers together"),
            "{}",
            add.content
        );
        let elsewhere = doc
            .chunks
            .iter()
            .filter(|c| c.heading.as_deref() != Some("function add"))
            .filter(|c| c.content.contains("Adds two numbers together"))
            .count();
        assert_eq!(elsewhere, 0, "the doc comment leaked into another chunk");
    }

    #[test]
    fn the_line_range_covers_the_chunk_including_its_doc_comment() {
        let doc = parse(SRC, DEFAULT_MAX_CHUNK_CHARS);
        let add = find(&doc, "function add");
        let (start, end) = add.line_range.expect("code chunks carry a line range");
        // Line 5 is the first `///` line; the body ends on line 10.
        assert_eq!(start, 5, "chunk starts at the doc comment");
        assert_eq!(end, 10);
    }

    #[test]
    fn a_method_carries_the_impl_block_it_sits_in_as_context() {
        let doc = parse(SRC, DEFAULT_MAX_CHUNK_CHARS);
        let bump = find(&doc, "method bump");
        let context = bump.context.as_deref().unwrap_or_default();
        // `impl Counter` is a reference, not a definition, so this scope can only come from
        // walking the tree rather than from the tag list.
        assert!(context.contains("Counter"), "context was {context:?}");
    }

    #[test]
    fn the_imports_survive_as_a_gap_chunk() {
        let doc = parse(SRC, DEFAULT_MAX_CHUNK_CHARS);
        let gap = doc
            .chunks
            .iter()
            .find(|c| c.heading.is_none())
            .expect("the imports are covered by no definition");
        assert!(
            gap.content.contains("use std::sync::Arc;"),
            "{}",
            gap.content
        );
        assert_eq!(gap.symbol_kind, None);
    }

    /// A split must not leave a piece holding only the closing brace.
    ///
    /// Built to land exactly on that: at a budget of 40, the signature weighs 9 non-whitespace
    /// characters and the middle line is padded to weigh 31, so the two together fill the
    /// budget and the next line — `}`, weighing 1 — overruns it. Before the merge this
    /// produced a chunk whose content was `}` and whose heading and `symbol_kind` were the
    /// function's, which the quality filter used to hide and, once definitions became exempt
    /// from the length penalties (ADR-0015), would have started returning.
    /// The one test named from a doc comment in this file still exists.
    ///
    /// `split_definition_by_lines` names it in prose rather than as an intra-doc link, and that
    /// is not a style choice: a link into a `#[cfg(test)]` module is not checked. Linking a name
    /// that does not exist there leaves `cargo doc --no-deps` at exit 0 just as a real one does
    /// (measured both ways, 2026-09-04), so the link would look like a guarantee while checking
    /// nothing. This assertion is the check that link would only have appeared to be.
    #[test]
    fn a_test_named_by_a_doc_comment_in_this_file_still_exists() {
        const NAMED: &str = "the_line_fallback_keeps_a_tail_too_thin_to_survive_as_a_gap";
        let src = include_str!("mod.rs");
        assert!(
            src.contains(&format!("`{NAMED}`")),
            "no doc comment names {NAMED} any more - drop it from this guard too"
        );
        assert!(
            src.contains(&format!("fn {NAMED}()")),
            "a doc comment in this file names {NAMED}, which no longer exists"
        );
    }

    #[test]
    fn a_split_never_leaves_a_piece_too_thin_to_stand_alone() {
        // Two shapes, because a cut lands in two places and only one of them is the tail.
        //
        // `tail`: the budget fills exactly, so the closing brace starts the last piece.
        // `head`: the second line overruns the budget on its own, so the cut pushes out what
        //         had accumulated — the signature — as a piece with pieces after it.
        let tail = format!("pub fn f(){{\n    let x = {};\n}}\n", "1".repeat(25));
        let head = format!("pub fn f(){{\n    let x = {};\n}}\n", "1".repeat(50));
        for (name, src) in [("tail", &tail), ("head", &head)] {
            let doc = parse(src, 40);
            let defs: Vec<&Chunk> = doc
                .chunks
                .iter()
                .filter(|c| c.symbol_kind.is_some())
                .collect();
            assert!(
                !defs.is_empty(),
                "{name}: expected definition chunks: {:#?}",
                doc.chunks
            );
            for d in &defs {
                assert!(
                    d.content.trim().chars().count() >= MIN_FRAGMENT_CHARS,
                    "{name}: a definition chunk under the fragment floor can only be a whole \
                     short definition, got {:?} from a split of an oversized one",
                    d.content
                );
            }
            // The bytes are not lost, only moved onto a neighbour (ADR-0012).
            let joined: String = defs.iter().map(|d| d.content.as_str()).collect();
            assert!(
                joined.contains("pub fn f") && joined.contains('}'),
                "{name}: the signature and the closing brace both have to survive: {joined:?}"
            );
        }
    }

    #[test]
    fn an_oversized_body_is_split_by_lines_into_pieces_that_share_the_heading() {
        let mut src = String::from("pub fn wide() {\n");
        for i in 0..80 {
            src.push_str(&format!("    let value_{i} = compute_something_long(i);\n"));
        }
        src.push_str("}\n");
        let doc = parse(&src, 200);
        let pieces: Vec<&Chunk> = doc
            .chunks
            .iter()
            .filter(|c| c.heading.as_deref() == Some("function wide"))
            .collect();
        assert!(
            pieces.len() > 1,
            "expected a hard split, got {}",
            pieces.len()
        );
        for p in &pieces {
            assert_eq!(p.symbol_kind.as_deref(), Some("function"));
            assert!(p.line_range.is_some());
        }
        let first = pieces[0].line_range.expect("range").0;
        let last = pieces[pieces.len() - 1].line_range.expect("range").1;
        assert!(first < last, "pieces should describe their own line ranges");
    }

    #[test]
    fn a_file_over_the_byte_cap_is_refused_rather_than_parsed() {
        let grammar = static_rust::grammar().expect("rust grammar builds");
        let parser = CodeParser::new(grammar, "rs", DEFAULT_MAX_CHUNK_CHARS);
        let oversized = vec![b'a'; usize::try_from(MAX_RAW_CODE_BYTES).unwrap_or(0) + 1];
        let err = parser
            .parse_bytes(&oversized, "big.rs", &[])
            .expect_err("over the cap");
        assert!(err.to_string().contains("over the"), "{err}");
    }

    #[test]
    fn a_file_of_exactly_the_cap_is_accepted_because_get_document_accepts_it() {
        let grammar = static_rust::grammar().expect("rust grammar builds");
        let parser = CodeParser::new(grammar, "rs", DEFAULT_MAX_CHUNK_CHARS);
        let mut src = String::from("pub fn edge() {}\n");
        while src.len() < usize::try_from(MAX_RAW_CODE_BYTES).unwrap_or(0) {
            src.push_str("// pad\n");
        }
        src.truncate(usize::try_from(MAX_RAW_CODE_BYTES).unwrap_or(0));
        assert_eq!(src.len() as u64, MAX_RAW_CODE_BYTES);
        parser
            .parse_bytes(src.as_bytes(), "edge.rs", &[])
            .expect("a file of exactly the cap parses");
    }

    /// `levels` nested modules around one function, on a single line.
    ///
    /// Built here rather than committed for the same reason the other fixtures are: a file on
    /// disk would arrive with whatever line endings the checkout gives it.
    fn deeply_nested(levels: usize) -> String {
        let mut src = String::new();
        for i in 0..levels {
            src.push_str("mod a");
            src.push_str(&i.to_string());
            src.push('{');
        }
        src.push_str("pub fn leaf()->u32{7}");
        for _ in 0..levels {
            src.push('}');
        }
        src.push('\n');
        src
    }

    fn parse_capped(src: &str, scope_limit: usize) -> ParsedDocument {
        parse_capped_with(src, DEFAULT_MAX_CHUNK_CHARS, scope_limit)
    }

    fn parse_capped_with(src: &str, budget: usize, scope_limit: usize) -> ParsedDocument {
        let grammar = static_rust::grammar().expect("rust grammar builds");
        chunk_source_capped(
            &grammar,
            budget,
            src.as_bytes(),
            src,
            "src/lib.rs",
            scope_limit,
        )
        .expect("a nested file still parses")
    }

    #[test]
    fn a_definition_nested_past_the_scope_limit_is_chunked_by_lines_rather_than_by_definition() {
        let doc = parse(&deeply_nested(200), DEFAULT_MAX_CHUNK_CHARS);
        assert!(!doc.chunks.is_empty(), "the file still has content");
        assert!(
            doc.chunks.iter().all(|c| c.symbol_kind.is_none()),
            "line chunks carry no definition kind, got {:?}",
            doc.chunks
                .iter()
                .map(|c| &c.symbol_kind)
                .collect::<Vec<_>>()
        );
        assert!(
            doc.frontmatter.tags.iter().any(|t| t == "parse:too-deep"),
            "tags were {:?}",
            doc.frontmatter.tags
        );
    }

    #[test]
    fn the_scope_limit_is_what_decides_it_rather_than_the_source() {
        // One source, two bounds. Under the shipped bound it chunks at its definitions; under
        // a bound of one ancestor the inner function is already too deep and the file takes
        // the line fallback.
        let src = "pub mod outer {\n    pub fn inner() -> u32 {\n        7\n    }\n}\n";

        let inside = parse_capped(src, MAX_DEFINITION_SCOPE_DEPTH);
        assert!(
            inside.chunks.iter().any(|c| c.symbol_kind.is_some()),
            "expected definition chunks, got {:?}",
            inside.chunks.iter().map(|c| &c.heading).collect::<Vec<_>>()
        );
        assert!(
            !inside
                .frontmatter
                .tags
                .iter()
                .any(|t| t == "parse:too-deep"),
            "tags were {:?}",
            inside.frontmatter.tags
        );

        let outside = parse_capped(src, 1);
        assert!(
            outside.chunks.iter().all(|c| c.symbol_kind.is_none()),
            "expected line chunks only"
        );
        assert!(
            outside
                .frontmatter
                .tags
                .iter()
                .any(|t| t == "parse:too-deep"),
            "tags were {:?}",
            outside.frontmatter.tags
        );
    }

    #[test]
    fn the_line_fallback_keeps_a_tail_too_thin_to_survive_as_a_gap() {
        // A gap fragment under the short-content threshold is dropped on purpose, and the
        // fallback covers the whole file with the same kind of piece. Droppable pieces would
        // lose the tail here, and with it the promise that the file keeps every byte.
        //
        // The budget is injected so the split lands where the test needs it: each filler line
        // weighs exactly one budget, so the closing brace cannot join the piece before it. The
        // filler is also long enough to clear the short-content threshold on its own - if
        // every piece were thin, the rule that keeps a file from ending up with no chunks at
        // all would hide the bug this test is for.
        const FILLER: &str = "pub fn function_with_a_name()->u32{7}\n";
        let budget = non_ws(FILLER, &(0..FILLER.len()));
        let mut src = String::from("pub mod outer {\n");
        for _ in 0..3 {
            src.push_str(FILLER);
        }
        src.push_str("}\n");

        let doc = parse_capped_with(&src, budget, 1);
        assert!(
            doc.frontmatter.tags.iter().any(|t| t == "parse:too-deep"),
            "the fixture is supposed to take the fallback, tags were {:?}",
            doc.frontmatter.tags
        );
        assert!(
            doc.chunks.len() > 1,
            "the fixture is supposed to span more than one chunk, got {}",
            doc.chunks.len()
        );
        let last = doc.chunks.last().expect("chunks are not empty");
        assert!(
            last.content.trim().chars().count() < MIN_FRAGMENT_CHARS,
            "the last chunk is supposed to be the thin one, got {:?}",
            last.content
        );
        assert_eq!(last.content.trim(), "}", "the tail is the closing brace");
    }

    #[test]
    fn a_file_chunked_by_lines_still_covers_every_byte() {
        // ADR-0012 promises a file contributes every byte it has whether or not it parses.
        // The fallback has to keep that promise too, which is why it fills the file with gap
        // chunks rather than refusing it.
        let src = deeply_nested(200);
        let doc = parse(&src, DEFAULT_MAX_CHUNK_CHARS);
        // Asserted first so this cannot quietly become a test of the definition path.
        assert!(
            doc.frontmatter.tags.iter().any(|t| t == "parse:too-deep"),
            "this fixture is supposed to take the fallback, tags were {:?}",
            doc.frontmatter.tags
        );
        let seen: String = doc.chunks.iter().map(|c| c.content.as_str()).collect();
        let seen_ws_free: String = seen.chars().filter(|c| !c.is_whitespace()).collect();
        let want: String = src.chars().filter(|c| !c.is_whitespace()).collect();
        assert_eq!(seen_ws_free, want, "the fallback dropped part of the file");
    }

    #[test]
    fn a_syntax_error_still_yields_definitions_and_marks_the_document() {
        let broken = "fn good() {}\n\nfn broken( {\n\nfn also_good() {}\n";
        let doc = parse(broken, DEFAULT_MAX_CHUNK_CHARS);
        assert!(
            doc.frontmatter.tags.iter().any(|t| t == "parse:degraded"),
            "tags were {:?}",
            doc.frontmatter.tags
        );
        assert!(!doc.chunks.is_empty(), "a broken file still has content");
    }

    #[test]
    fn crlf_line_endings_do_not_shift_the_reported_lines() {
        let lf = parse(SRC, DEFAULT_MAX_CHUNK_CHARS);
        // Built here rather than committed: `.gitattributes` normalises CRLF away on checkout,
        // so a fixture file would silently arrive as LF and test nothing.
        let crlf_src = SRC.replace('\n', "\r\n");
        let crlf = parse(&crlf_src, DEFAULT_MAX_CHUNK_CHARS);
        assert_eq!(
            find(&lf, "function add").line_range,
            find(&crlf, "function add").line_range
        );
    }

    #[test]
    fn the_language_shows_up_as_a_tag_so_a_search_can_filter_on_it() {
        let doc = parse(SRC, DEFAULT_MAX_CHUNK_CHARS);
        assert!(doc.frontmatter.tags.iter().any(|t| t == "code"));
        assert!(doc.frontmatter.tags.iter().any(|t| t == "lang:rust"));
    }

    #[test]
    fn the_retained_source_is_the_file_itself_not_the_chunks_rejoined() {
        let doc = parse(SRC, DEFAULT_MAX_CHUNK_CHARS);
        // `get_document` hands this back. The prose parsers rejoin their chunks because their
        // chunks are a lossy view of the document; here the chunks already cover every byte,
        // so rejoining would only insert blank lines and trim indentation -- and return source
        // that no longer compiles.
        assert_eq!(doc.raw_content, SRC);
    }

    #[test]
    fn every_byte_of_the_file_is_covered_by_some_chunk() {
        let doc = parse(SRC, DEFAULT_MAX_CHUNK_CHARS);
        // Not a byte-for-byte reconstruction (chunk bodies are right-trimmed), but every
        // non-whitespace character has to appear somewhere: gap-filling exists so that nothing
        // is dropped for being outside a definition.
        let seen: String = doc.chunks.iter().map(|c| c.content.as_str()).collect();
        let seen_ws_free: String = seen.chars().filter(|c| !c.is_whitespace()).collect();
        for line in SRC.lines() {
            let want: String = line.chars().filter(|c| !c.is_whitespace()).collect();
            if want.len() < MIN_FRAGMENT_CHARS && !want.is_empty() {
                continue; // short gap fragments are dropped on purpose
            }
            if want.is_empty() {
                continue;
            }
            assert!(seen_ws_free.contains(&want), "line {line:?} is in no chunk");
        }
    }

    #[test]
    fn the_trait_contract_parse_returns_nothing_rather_than_one_giant_chunk() {
        let grammar = static_rust::grammar().expect("rust grammar builds");
        let parser = CodeParser::new(grammar, "rs", DEFAULT_MAX_CHUNK_CHARS);
        // Reachable only by a caller bypassing `parse_bytes`. Returning the whole file as one
        // chunk is the shape this module exists to avoid, so it must not be what happens.
        let doc = parser.parse(SRC, "src/lib.rs", &[]);
        assert!(doc.chunks.is_empty());
    }
}
