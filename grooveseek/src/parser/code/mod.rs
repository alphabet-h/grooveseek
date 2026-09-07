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

use std::borrow::Cow;
use std::collections::HashSet;
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

/// Chunks one file may contribute.
///
/// A file that wants more is chunked by lines instead, and the line budget is widened just
/// enough that the result fits — so this is a bound the index actually keeps, and the file
/// still contributes every byte it has, which is what ADR-0012 requires of any file the
/// chunker gives up on. What the file loses is granularity, not content.
///
/// It used to be applied by keeping the first this-many chunks and dropping the rest. Since
/// the pieces are sorted by position, that silently left the tail of a wide file out of the
/// index while `get_document`, served from [`crate::server`], went on returning the whole
/// file.
//
// Measured 2026-09-06 against this repository's own sources: 62 `.rs` files copied into a
// scratch knowledge base, indexed with `target/release/groove.exe index --config
// groove.toml --force`, then counted with
//   SELECT d.path, COUNT(c.id) FROM documents d
//     JOIN chunks c ON c.document_id = d.id GROUP BY d.id ORDER BY 2 DESC
// The widest file was `transport/http.rs` at 297 chunks, so real code has room to grow by
// about three-quarters before it meets this.
const MAX_CHUNKS_PER_FILE: usize = 512;

/// Tag every document this parser produces carries.
///
/// It does not prove one, though: `tags` is frontmatter, so a note can declare it by hand.
/// What separates a source file from a note that says the same words is the line range on
/// its chunks, which comes from the parser rather than from the document.
const TAG_CODE: &str = "code";

/// Tag on a document whose grammar could not read part of it.
const TAG_PARSE_DEGRADED: &str = "parse:degraded";

/// Tag on a document chunked by lines because a definition sat past the scope bound.
const TAG_PARSE_TOO_DEEP: &str = "parse:too-deep";

/// Tag on a document chunked by lines because its definitions wanted more chunks than
/// [`MAX_CHUNKS_PER_FILE`].
const TAG_PARSE_TOO_MANY_CHUNKS: &str = "parse:too-many-chunks";

/// The tags that say a document holds no definition metadata because groove declined to
/// produce it. Whoever reports on that state reads this rather than listing the tags again.
///
/// [`TAG_PARSE_DEGRADED`] is deliberately not one of them. A file the grammar could not fully
/// read still contributes the definitions around the break, which is the whole point of
/// gap-filling; what it lost is a region, not its definitions.
pub(crate) const TAGS_WITHOUT_DEFINITIONS: &[&str] =
    &[TAG_PARSE_TOO_DEEP, TAG_PARSE_TOO_MANY_CHUNKS];

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
    // not a code change. The plugin loader is the second caller.
    //
    // The name is taken either way a grammar arrives: the compiled-in one hands a `&'static
    // str`, a plugin hands the `String` it copied out of the library. An owned name is leaked
    // to `'static` -- `CodeParser` and every chunk key on it for the rest of the process --
    // but only once the tags query has compiled, so a refused grammar keeps nothing. That is
    // the same rule the plugin loader applies to the library itself (`mem::forget` after the
    // last check, drop on any refusal).
    #[cfg_attr(not(feature = "grammar-rust"), allow(dead_code))]
    pub(crate) fn new(
        name: impl Into<Cow<'static, str>>,
        language: Language,
        tags_query: &str,
    ) -> Result<Self> {
        let name = name.into();
        let config = TagsConfiguration::new(language.clone(), tags_query, "")
            .map_err(|e| anyhow::anyhow!("grammar {name}: tags query rejected: {e:?}"))?;
        let name: &'static str = match name {
            Cow::Borrowed(name) => name,
            Cow::Owned(name) => String::leak(name),
        };
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
    chunk_source_capped(grammar, budget, bytes, text, path_hint, Bounds::SHIPPED)
}

/// The two bounds that decide when the chunker stops chunking at definitions.
///
/// One struct rather than two `usize` parameters, because they are both counts: passing them
/// in the wrong order would compile and quietly change which bound a test was about.
#[derive(Debug, Clone, Copy)]
struct Bounds {
    /// Ancestors a definition may sit under before the file is chunked by lines.
    scope_depth: usize,
    /// Chunks one file may contribute before it is chunked by lines.
    chunks: usize,
}

impl Bounds {
    /// What production runs with.
    const SHIPPED: Self = Self {
        scope_depth: MAX_DEFINITION_SCOPE_DEPTH,
        chunks: MAX_CHUNKS_PER_FILE,
    };
}

/// [`chunk_source`] with the bounds injected, so a unit test can take either fallback without
/// building a source that reaches it for real (the same split [`super::pdf`] uses for its page
/// budgets).
fn chunk_source_capped(
    grammar: &LoadedGrammar,
    budget: usize,
    bytes: &[u8],
    text: &str,
    path_hint: &str,
    bounds: Bounds,
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
            Some(n) => match scope_chain(n, text, bounds.scope_depth) {
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

    // Chunk at the definitions — unless the scope bound already ended that, in which case
    // there are none and the file goes straight to the fallback below.
    let mut pieces: Vec<Piece> = Vec::new();
    if too_deep {
        tracing::warn!(
            path = path_hint,
            limit = bounds.scope_depth,
            "a definition is nested deeper than the limit; chunking this file by lines instead"
        );
    } else {
        drop_repeated_ranges(&mut defs);
        link_containment(&mut defs);
        // What bounds `emit_def`'s recursion. Containment nests no deeper than the syntax tree
        // once repeats are gone, and `scope_chain` already refused anything past the bound.
        debug_assert!(
            defs.iter().all(|d| d.depth <= bounds.scope_depth),
            "containment nested deeper than the scope bound allows"
        );
        let roots: Vec<usize> = (0..defs.len()).filter(|i| defs[*i].depth == 0).collect();
        for i in &roots {
            emit_def(*i, &defs, text, budget, &title, &mut pieces);
        }
        fill_gaps(&roots, &defs, text, budget, &title, &mut pieces);
    }

    let mut kept = settle(pieces, text);
    // Asked of the definition chunks, which is why a file the scope bound stopped answers no
    // rather than being excluded by hand: it produced none to count.
    let too_many = kept.len() > bounds.chunks;
    if too_many {
        tracing::warn!(
            path = path_hint,
            limit = bounds.chunks,
            chunks = kept.len(),
            "code file wants more chunks than the per-file limit; chunking it by lines instead"
        );
    }
    if too_deep || too_many {
        kept = settle(
            line_chunk_whole_file(text, budget, bounds.chunks, &title),
            text,
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

    let mut tags_out = vec![TAG_CODE.to_string(), format!("lang:{}", grammar.name)];
    if has_error {
        tags_out.push(TAG_PARSE_DEGRADED.to_string());
    }
    // A separate tag from `parse:degraded`, which answers a different question: that one says
    // the grammar could not read part of the file, this one says groove declined to chunk a
    // file it could read. A caller filtering for one does not want the other.
    if too_deep {
        tags_out.push(TAG_PARSE_TOO_DEEP.to_string());
    }
    // And separate again from `parse:too-deep`, which is the same outcome reached for the
    // other reason. Both are in `TAGS_WITHOUT_DEFINITIONS`; they are two tags rather than one
    // because the input to change differs -- flatten the nesting, or split the file.
    if too_many {
        tags_out.push(TAG_PARSE_TOO_MANY_CHUNKS.to_string());
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

/// Drop definitions that cover bytes an earlier one already covers.
///
/// A tags query may bind one `@name` per repeated child under a single `@definition.*` node.
/// `public $a, $b, $c;` in the PHP grammar this project publishes is one `property_declaration`
/// holding three names, and tree-sitter-tags keeps one tag per name, so the same byte range
/// arrives once per name. Those are one definition seen under several names rather than one
/// nested inside another, and [`link_containment`] cannot tell the two apart: a range never
/// ends before itself, so each repeat becomes the child of the one before it and the forest
/// degenerates into a chain as long as the declaration is wide.
///
/// Collapsing them here is what keeps [`emit_def`]'s recursion bounded, because containment
/// can then nest no deeper than the syntax tree does and [`MAX_DEFINITION_SCOPE_DEPTH`] already
/// refused anything past its own bound.
//
// Measured 2026-09-06, release build on Windows: a 22,923 byte `.php` file whose class declares
// 3,000 properties in one statement ended the run with STATUS_STACK_OVERFLOW (exit 0xC00000FD),
// against a 1 MiB raw-byte cap that never came close to firing. The same shape with 2,000
// properties indexed in 262 ms.
//   target/release/groove.exe --config <cfg> index --force
fn drop_repeated_ranges(defs: &mut Vec<Def>) {
    let mut seen: HashSet<(usize, usize)> = HashSet::new();
    defs.retain(|d| seen.insert((d.covered.start, d.covered.end)));
}

/// Turn the flat definition list into a containment forest.
///
/// Definitions covering the same bytes must already have been collapsed by
/// [`drop_repeated_ranges`]; this treats a range that does not end before another starts as
/// containing it, which is true of a repeat as well as of a genuinely nested definition.
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

/// Put the pieces in file order and drop the ones too thin to be worth a chunk.
///
/// Every path out of the chunker ends here, so a piece list is never turned into chunks
/// without passing the same two steps.
fn settle(mut pieces: Vec<Piece>, text: &str) -> Vec<Piece> {
    pieces.sort_by_key(|p| p.range.start);
    drop_thin_fragments(pieces, text)
}

/// The whole file as headingless line pieces — what both bounds fall back to.
///
/// The file still contributes every byte it has (ADR-0012); it contributes them as lines,
/// which is the shape a region no definition covers already gets. Falling back to the
/// plain-text parser instead would make the whole file one chunk, the shape this module
/// exists to avoid.
///
/// The pieces are **not** droppable, unlike an ordinary gap. A gap is the frame around content
/// that was chunked as a definition; here there is no such content, so a thin last piece is
/// the end of the file rather than a stray closing brace beside something that survived.
/// Taking `droppable` from the gap path instead is how an earlier version of the scope
/// fallback lost the tail of a file it was supposed to keep whole.
///
/// `limit` is applied by widening the budget rather than by dropping pieces, so this is the
/// one place that makes [`MAX_CHUNKS_PER_FILE`] true of every file: whichever bound sent a
/// file here, what comes back fits it.
fn line_chunk_whole_file(
    text: &str,
    budget: usize,
    limit: usize,
    title: &Option<String>,
) -> Vec<Piece> {
    let context_parts: Vec<String> = title.iter().cloned().collect();
    let mut pieces = Vec::new();
    push_line_pieces(
        0..text.len(),
        &context_parts,
        text,
        budget_for_at_most(text, budget, limit),
        false,
        &mut pieces,
    );
    pieces
}

/// A budget wide enough that [`split_by_lines`] cannot cut `text` into more than `limit` pieces.
///
/// [`split_by_lines`] starts a new piece only when the next line would overrun the budget, so
/// any two neighbouring pieces weigh more than it together. A text weighing `weight` therefore
/// yields fewer than `2 * weight / budget + 1` pieces, and asking for that to stay under
/// `limit` gives the budget below.
///
/// Widening is what lets the chunk bound and ADR-0012 hold at the same time: the file is cut
/// more coarsely rather than cut short, so every byte still reaches the index.
///
/// The configured budget is returned untouched unless it is too narrow for the file, which
/// takes a weight above `budget * (limit - 1) / 2`. At [`DEFAULT_MAX_CHUNK_CHARS`] and
/// [`MAX_CHUNKS_PER_FILE`] that is around 894,000 non-whitespace characters — reachable
/// under [`MAX_RAW_CODE_BYTES`], but only by a file that is nearly all code and nearly at
/// the cap. The ordinary way to reach it is a knowledge base that set
/// `[parsers.code].max_chunk_chars` far below the default.
fn budget_for_at_most(text: &str, budget: usize, limit: usize) -> usize {
    let weight = non_ws(text, &(0..text.len()));
    // `limit - 1` because the bound above is strict; the `max(1)` keeps a limit of one (or of
    // zero, which no caller passes) from dividing by nothing.
    let pairs = limit.saturating_sub(1).max(1);
    budget.max(weight.saturating_mul(2).div_ceil(pairs))
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
            Bounds {
                scope_depth: scope_limit,
                ..Bounds::SHIPPED
            },
        )
        .expect("a nested file still parses")
    }

    /// The other injected bound: how many chunks the file may contribute before it is chunked
    /// by lines. Injected for the same reason the scope one is — a source that reaches the
    /// shipped bound for real is a large fixture to carry around.
    fn parse_chunk_capped(src: &str, budget: usize, chunk_limit: usize) -> ParsedDocument {
        let grammar = static_rust::grammar().expect("rust grammar builds");
        chunk_source_capped(
            &grammar,
            budget,
            src.as_bytes(),
            src,
            "src/lib.rs",
            Bounds {
                chunks: chunk_limit,
                ..Bounds::SHIPPED
            },
        )
        .expect("a wide file still parses")
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

    /// `defs` one-line functions, one per line.
    ///
    /// Wide rather than deep: every definition sits at the top level, so the scope bound is
    /// nowhere near and the only thing that can send this file to the fallback is how many
    /// chunks it wants. Built here for the reason the other fixtures are — a file on disk
    /// would arrive with whatever line endings the checkout gives it.
    fn wide_source(defs: usize) -> String {
        let mut src = String::new();
        for i in 0..defs {
            src.push_str("pub fn f");
            src.push_str(&i.to_string());
            src.push_str("() -> u32 { ");
            src.push_str(&i.to_string());
            src.push_str(" }\n");
        }
        src
    }

    #[test]
    fn a_file_over_the_chunk_limit_is_chunked_by_lines_rather_than_truncated() {
        let src = wide_source(40);
        let doc = parse_chunk_capped(&src, DEFAULT_MAX_CHUNK_CHARS, 8);
        // Asserted first so this cannot quietly become a test of the definition path.
        assert!(
            doc.frontmatter
                .tags
                .iter()
                .any(|t| t == "parse:too-many-chunks"),
            "this fixture is supposed to take the fallback, tags were {:?}",
            doc.frontmatter.tags
        );
        assert!(
            !doc.frontmatter.tags.iter().any(|t| t == "parse:too-deep"),
            "nothing in this fixture is nested, tags were {:?}",
            doc.frontmatter.tags
        );
        assert!(
            doc.chunks.len() <= 8,
            "the fallback is supposed to fit the bound, got {} chunks",
            doc.chunks.len()
        );
        assert!(
            doc.chunks.iter().all(|c| c.symbol_kind.is_none()),
            "line chunks carry no definition kind, got {:?}",
            doc.chunks
                .iter()
                .map(|c| &c.symbol_kind)
                .collect::<Vec<_>>()
        );
        // The last definition in the file. Pieces are sorted by position, so keeping the first
        // `limit` of them is exactly what used to drop this one.
        let seen: String = doc.chunks.iter().map(|c| c.content.as_str()).collect();
        assert!(seen.contains("f39"), "the tail of the file is missing");
    }

    #[test]
    fn a_file_over_the_chunk_limit_still_covers_every_byte() {
        // The budget is injected narrow so the fallback produces several chunks. A fixture
        // that fits in one chunk cannot show that nothing was dropped between them, which is
        // how the coverage test for the scope fallback missed a real loss once already.
        let src = wide_source(40);
        let doc = parse_chunk_capped(&src, 40, 8);
        assert!(
            doc.frontmatter
                .tags
                .iter()
                .any(|t| t == "parse:too-many-chunks"),
            "this fixture is supposed to take the fallback, tags were {:?}",
            doc.frontmatter.tags
        );
        assert!(
            doc.chunks.len() > 1,
            "the split never happened, so this proves nothing about what falls between chunks"
        );
        let seen: String = doc.chunks.iter().map(|c| c.content.as_str()).collect();
        let seen_ws_free: String = seen.chars().filter(|c| !c.is_whitespace()).collect();
        let want: String = src.chars().filter(|c| !c.is_whitespace()).collect();
        assert_eq!(seen_ws_free, want, "the fallback dropped part of the file");
    }

    #[test]
    fn the_chunk_limit_is_what_decides_it_rather_than_the_source() {
        // One source, two bounds — the same shape as the scope test above.
        let src = wide_source(40);

        let inside = parse_chunk_capped(&src, DEFAULT_MAX_CHUNK_CHARS, MAX_CHUNKS_PER_FILE);
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
                .any(|t| t == "parse:too-many-chunks"),
            "tags were {:?}",
            inside.frontmatter.tags
        );

        let outside = parse_chunk_capped(&src, DEFAULT_MAX_CHUNK_CHARS, 8);
        assert!(
            outside.chunks.iter().all(|c| c.symbol_kind.is_none()),
            "expected line chunks only"
        );
        assert!(
            outside
                .frontmatter
                .tags
                .iter()
                .any(|t| t == "parse:too-many-chunks"),
            "tags were {:?}",
            outside.frontmatter.tags
        );
    }

    #[test]
    fn a_file_of_exactly_the_chunk_limit_is_left_at_its_definitions() {
        // The count is read off a run that cannot reach the bound rather than written down
        // here: how many chunks a source produces is the chunker's business, and hard-coding
        // it would turn this into a test of that number instead of of the comparison.
        let src = wide_source(40);
        let unbounded = parse_chunk_capped(&src, DEFAULT_MAX_CHUNK_CHARS, MAX_CHUNKS_PER_FILE);
        let exactly = unbounded.chunks.len();
        assert!(
            exactly > 1,
            "the fixture has to produce more than one chunk"
        );

        let fits = parse_chunk_capped(&src, DEFAULT_MAX_CHUNK_CHARS, exactly);
        assert!(
            !fits
                .frontmatter
                .tags
                .iter()
                .any(|t| t == "parse:too-many-chunks"),
            "a file that fits exactly is not over the bound, tags were {:?}",
            fits.frontmatter.tags
        );
        assert_eq!(fits.chunks.len(), exactly);

        let over = parse_chunk_capped(&src, DEFAULT_MAX_CHUNK_CHARS, exactly - 1);
        assert!(
            over.frontmatter
                .tags
                .iter()
                .any(|t| t == "parse:too-many-chunks"),
            "one under the count is over the bound, tags were {:?}",
            over.frontmatter.tags
        );
    }

    #[test]
    fn the_fallback_fits_the_bound_however_narrow_the_budget_is() {
        // The budget decides how finely the fallback cuts, and `[parsers.code].max_chunk_chars`
        // takes any number. Widening it rather than dropping pieces is what lets the bound and
        // ADR-0012's "every byte" hold at once, so both are asserted for each budget.
        let src = wide_source(60);
        let want: String = src.chars().filter(|c| !c.is_whitespace()).collect();
        for budget in [1usize, 5, 40] {
            let doc = parse_chunk_capped(&src, budget, 6);
            assert!(
                doc.chunks.len() <= 6,
                "budget {budget} produced {} chunks",
                doc.chunks.len()
            );
            let seen: String = doc.chunks.iter().map(|c| c.content.as_str()).collect();
            let seen_ws_free: String = seen.chars().filter(|c| !c.is_whitespace()).collect();
            assert_eq!(
                seen_ws_free, want,
                "budget {budget} dropped part of the file"
            );
        }
    }

    /// [`deeply_nested`] written one brace per line.
    ///
    /// The single-line version cannot be split at all, so any test about how many pieces the
    /// fallback produces would pass on a file of one chunk without exercising anything.
    fn deeply_nested_lines(levels: usize) -> String {
        let mut src = String::new();
        for i in 0..levels {
            src.push_str("mod a");
            src.push_str(&i.to_string());
            src.push_str(" {\n");
        }
        src.push_str("pub fn leaf() -> u32 { 7 }\n");
        for _ in 0..levels {
            src.push_str("}\n");
        }
        src
    }

    #[test]
    fn a_file_the_scope_bound_stopped_also_fits_the_chunk_bound() {
        // The truncation this replaced sat outside the branch, so it cut the scope fallback's
        // output as well — a deep file wide enough to overrun the bound lost its tail for the
        // second reason after losing its definitions for the first.
        let src = deeply_nested_lines(200);
        let doc = parse_chunk_capped(&src, 1, 4);
        assert!(
            doc.frontmatter.tags.iter().any(|t| t == "parse:too-deep"),
            "this fixture is supposed to take the scope fallback, tags were {:?}",
            doc.frontmatter.tags
        );
        assert!(
            doc.chunks.len() <= 4,
            "the scope fallback has to fit the chunk bound too, got {}",
            doc.chunks.len()
        );
        // The same guard its sibling above carries: a fixture that collapses to one chunk
        // makes the coverage assertion below vacuous, and nothing else pins the widening
        // constant or the fixture size to keep that from happening.
        assert!(
            doc.chunks.len() > 1,
            "the split never happened, so this proves nothing about what falls between chunks"
        );
        let seen: String = doc.chunks.iter().map(|c| c.content.as_str()).collect();
        let seen_ws_free: String = seen.chars().filter(|c| !c.is_whitespace()).collect();
        let want: String = src.chars().filter(|c| !c.is_whitespace()).collect();
        assert_eq!(
            seen_ws_free, want,
            "the scope fallback dropped part of the file"
        );
    }

    #[test]
    fn a_file_stopped_by_the_scope_bound_names_only_that_bound() {
        // The scope bound stops the walk at the first definition past it, so the file never
        // gets as far as producing definition chunks to count. Tagging it for both would claim
        // a decision groove never reached.
        let src = deeply_nested(200);
        let doc = parse_chunk_capped(&src, 1, 2);
        assert!(
            doc.frontmatter.tags.iter().any(|t| t == "parse:too-deep"),
            "tags were {:?}",
            doc.frontmatter.tags
        );
        assert!(
            !doc.frontmatter
                .tags
                .iter()
                .any(|t| t == "parse:too-many-chunks"),
            "tags were {:?}",
            doc.frontmatter.tags
        );
    }

    #[test]
    fn an_ordinary_file_is_not_tagged_as_one_the_chunker_gave_up_on() {
        // The reverse guard: a bound widened until it catches ordinary code would be worse
        // than the bug it replaced, and `doctor` now reports on exactly these tags.
        let doc = parse(SRC, DEFAULT_MAX_CHUNK_CHARS);
        assert!(
            !doc.frontmatter.tags.iter().any(|t| t.starts_with("parse:")),
            "tags were {:?}",
            doc.frontmatter.tags
        );
    }

    #[test]
    fn the_widened_budget_keeps_the_split_under_the_bound() {
        // The arithmetic `budget_for_at_most` is built on, checked against the function it is
        // built for rather than restated.
        for lines in [1usize, 7, 50, 500] {
            let src = wide_source(lines);
            for limit in [1usize, 2, 8, 64] {
                let budget = budget_for_at_most(&src, 1, limit);
                let pieces = split_by_lines(&src, &(0..src.len()), budget);
                assert!(
                    pieces.len() <= limit,
                    "{lines} line(s) at a bound of {limit} split into {}",
                    pieces.len()
                );
            }
        }
    }

    #[test]
    fn the_budget_is_left_alone_when_the_file_does_not_need_widening() {
        // Widening is for a knowledge base that set `max_chunk_chars` far below the default;
        // an ordinary file under the shipped settings must come out of it untouched.
        let src = wide_source(40);
        assert_eq!(
            budget_for_at_most(&src, DEFAULT_MAX_CHUNK_CHARS, MAX_CHUNKS_PER_FILE),
            DEFAULT_MAX_CHUNK_CHARS
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

    /// A grammar over the compiled-in Rust parse table with a hand-written tags query, so a
    /// test can produce tag shapes the shipped query never produces.
    fn grammar_with_query(query: &str) -> LoadedGrammar {
        LoadedGrammar::new(
            "rust",
            Language::from(static_rust::DESCRIPTOR.language),
            query,
        )
        .expect("the hand-written tags query compiles against the Rust grammar")
    }

    /// A name that arrived as an owned `String` (a plugin's) is named in the refusal, and the
    /// refusal is the one path on which [`LoadedGrammar::new`] keeps nothing: the leak is after
    /// the compile.
    #[test]
    fn an_owned_grammar_name_is_named_in_a_rejected_query() {
        let err = LoadedGrammar::new(
            String::from("plugin-lang"),
            Language::from(static_rust::DESCRIPTOR.language),
            "((",
        )
        .err()
        .expect("an unbalanced query must be refused");
        assert!(format!("{err}").contains("grammar plugin-lang:"), "{err}");
    }

    /// One `@definition.*` on the outer node, one `@name` per repeated inner child: the shape
    /// tree-sitter-php's `@definition.field` has for `public $a, $b;`.
    const FIELD_QUERY: &str = "(struct_item body: (field_declaration_list (field_declaration name: (field_identifier) @name))) @definition.class";

    fn wide_struct(fields: usize) -> String {
        let mut s = String::from("pub struct Row {\n");
        for i in 0..fields {
            s.push_str(&format!("    field_number_{i}: u32,\n"));
        }
        s.push('}');
        s.push('\n');
        s
    }

    fn def_at(name: &str, covered: Range<usize>) -> Def {
        Def {
            kind: "field".to_string(),
            name: name.to_string(),
            covered,
            scope: Vec::new(),
            children: Vec::new(),
            depth: 0,
        }
    }

    #[test]
    fn definitions_that_cover_the_same_bytes_collapse_to_the_first() {
        let mut defs = vec![
            def_at("a0", 10..50),
            def_at("a1", 10..50),
            def_at("a2", 10..50),
            def_at("later", 60..80),
        ];
        drop_repeated_ranges(&mut defs);
        let names: Vec<&str> = defs.iter().map(|d| d.name.as_str()).collect();
        // The leftmost name stands for the declaration, and a definition covering different
        // bytes is untouched -- this drops repeats, not neighbours.
        assert_eq!(names, ["a0", "later"]);
    }

    /// A repeat covers the same bytes at both ends. Two definitions that merely begin together
    /// -- which the sort in [`link_containment`] expects, since it breaks a tie on the start by
    /// putting the wider one first -- are a real nesting and both have to survive.
    #[test]
    fn a_definition_sharing_only_its_start_with_another_is_not_a_repeat() {
        let mut defs = vec![def_at("outer", 10..80), def_at("inner", 10..40)];
        drop_repeated_ranges(&mut defs);
        let names: Vec<&str> = defs.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, ["outer", "inner"]);
    }

    /// Without [`drop_repeated_ranges`] the repeats become a chain (each the child of the one
    /// before it), [`emit_def`] recurses once per link, and the heading of every chunk comes
    /// from the deepest link rather than the declaration's first name.
    #[test]
    fn a_wide_declaration_is_one_definition_rather_than_a_chain_of_them() {
        let grammar = grammar_with_query(FIELD_QUERY);
        let src = wide_struct(200);
        assert!(
            non_ws(&src, &(0..src.len())) > DEFAULT_MAX_CHUNK_CHARS,
            "the fixture has to outweigh the budget or emit_def never looks at the children"
        );
        let doc = chunk_source(
            &grammar,
            DEFAULT_MAX_CHUNK_CHARS,
            src.as_bytes(),
            &src,
            "src/lib.rs",
        )
        .expect("parses");
        assert!(
            doc.chunks.len() > 1,
            "the split never happened, so this proves nothing about which name won"
        );
        for chunk in &doc.chunks {
            assert_eq!(
                chunk.heading.as_deref(),
                Some("class field_number_0"),
                "every piece of one declaration is headed by its first name"
            );
        }
    }

    #[test]
    fn a_wide_declaration_still_covers_every_byte() {
        let grammar = grammar_with_query(FIELD_QUERY);
        let src = wide_struct(200);
        let doc = chunk_source(
            &grammar,
            DEFAULT_MAX_CHUNK_CHARS,
            src.as_bytes(),
            &src,
            "src/lib.rs",
        )
        .expect("parses");
        assert!(
            doc.chunks.len() > 1,
            "the split never happened, so this proves nothing about what falls between chunks"
        );
        let seen: String = doc.chunks.iter().map(|c| c.content.as_str()).collect();
        let seen_ws_free: String = seen.chars().filter(|c| !c.is_whitespace()).collect();
        let want: String = src.chars().filter(|c| !c.is_whitespace()).collect();
        assert_eq!(seen_ws_free, want, "collapsing the repeats dropped content");
    }

    /// One line the grammar has to read in full. The byte cap refuses anything over
    /// [`MAX_RAW_CODE_BYTES`] before tree-sitter sees it, so a case that tests the parser
    /// rather than the gate has to sit under it.
    #[test]
    fn a_source_file_that_is_one_enormous_line_still_yields_its_definition() {
        let src = format!("pub fn f() -> u32 {{ {} 7 }}\n", "1 + ".repeat(20_000));
        assert!(
            (src.len() as u64) < MAX_RAW_CODE_BYTES,
            "the fixture has to stay under the byte cap to reach the parser at all"
        );
        assert_eq!(
            src.lines().count(),
            1,
            "the point of the fixture is one line"
        );
        let doc = parse(&src, DEFAULT_MAX_CHUNK_CHARS);
        assert!(
            doc.chunks
                .iter()
                .any(|c| c.heading.as_deref() == Some("function f")),
            "got {:?}",
            doc.chunks.iter().map(|c| &c.heading).collect::<Vec<_>>()
        );
    }

    #[test]
    fn source_that_is_not_valid_utf8_is_refused_rather_than_parsed() {
        let grammar = static_rust::grammar().expect("rust grammar builds");
        let parser = CodeParser::new(grammar, "rs", DEFAULT_MAX_CHUNK_CHARS);
        let err = parser
            .parse_bytes(&[0xff, 0xfe, 0x00], "src/lib.rs", &[])
            .expect_err("not UTF-8");
        assert!(err.to_string().contains("not valid UTF-8"), "got {err}");
    }

    /// The same refusal for the shape a chunk boundary produces: a multibyte character cut in
    /// half, which is valid up to its last byte.
    #[test]
    fn a_truncated_multibyte_character_is_refused_like_any_other_bad_utf8() {
        let grammar = static_rust::grammar().expect("rust grammar builds");
        let parser = CodeParser::new(grammar, "rs", DEFAULT_MAX_CHUNK_CHARS);
        let mut bytes = "pub fn f() -> u32 { 7 } // ".as_bytes().to_vec();
        bytes.extend_from_slice(&[0xe3, 0x81]);
        let err = parser
            .parse_bytes(&bytes, "src/lib.rs", &[])
            .expect_err("cut in the middle of a character");
        assert!(err.to_string().contains("not valid UTF-8"), "got {err}");
    }

    /// A quote that is never closed swallows the rest of the file as far as the grammar is
    /// concerned. The definitions before it survive and the document says it was degraded.
    #[test]
    fn an_unterminated_string_literal_is_marked_degraded_rather_than_dropped() {
        let src = "pub fn good() {}\n\npub fn bad() { let s = \"oops;\n}\n";
        let doc = parse(src, DEFAULT_MAX_CHUNK_CHARS);
        assert!(
            doc.frontmatter.tags.iter().any(|t| t == "parse:degraded"),
            "tags were {:?}",
            doc.frontmatter.tags
        );
        assert!(
            doc.chunks
                .iter()
                .any(|c| c.heading.as_deref() == Some("function good")),
            "the definition before the break is still found"
        );
        let seen: String = doc.chunks.iter().map(|c| c.content.as_str()).collect();
        assert!(seen.contains("oops"), "the unreadable region is still text");
    }

    #[test]
    fn an_empty_source_file_yields_no_chunks() {
        let grammar = static_rust::grammar().expect("rust grammar builds");
        let parser = CodeParser::new(grammar, "rs", DEFAULT_MAX_CHUNK_CHARS);
        let doc = parser.parse_bytes(b"", "src/lib.rs", &[]).expect("parses");
        // The indexer skips a document with no chunks before it writes a row, so an empty
        // source never reaches the database rather than arriving there with nothing in it.
        assert!(doc.chunks.is_empty());
        assert_eq!(doc.raw_content, "");
    }

    #[test]
    fn a_file_of_only_whitespace_yields_no_chunks() {
        let doc = parse("   \n\n  \n", DEFAULT_MAX_CHUNK_CHARS);
        assert!(doc.chunks.is_empty());
    }

    #[test]
    fn a_file_of_only_comments_still_contributes_its_text() {
        let src = "// one comment that is long enough to stand alone\n// and a second one\n";
        let doc = parse(src, DEFAULT_MAX_CHUNK_CHARS);
        // No definition to head it, so it arrives as a gap chunk rather than not at all.
        assert!(
            !doc.chunks.is_empty(),
            "a file of comments is still content"
        );
        let seen: String = doc.chunks.iter().map(|c| c.content.as_str()).collect();
        assert!(seen.contains("one comment"), "got {seen:?}");
    }

    #[test]
    fn a_file_with_no_definitions_still_contributes_its_text() {
        let src = "use std::io;\nuse std::fmt;\nuse std::collections::HashMap;\n";
        let doc = parse(src, DEFAULT_MAX_CHUNK_CHARS);
        assert!(!doc.chunks.is_empty());
        let seen: String = doc.chunks.iter().map(|c| c.content.as_str()).collect();
        assert!(seen.contains("HashMap"), "got {seen:?}");
    }

    /// A file saved with a byte order mark is a file, not a broken one: the mark is valid
    /// UTF-8, so it reaches the grammar as a leading character and must not cost the first
    /// definition its heading.
    #[test]
    fn a_byte_order_mark_does_not_hide_the_definition_after_it() {
        let doc = parse(
            "\u{feff}pub fn first() -> u32 { 7 }\n",
            DEFAULT_MAX_CHUNK_CHARS,
        );
        assert!(
            doc.chunks
                .iter()
                .any(|c| c.heading.as_deref() == Some("function first")),
            "got {:?}",
            doc.chunks.iter().map(|c| &c.heading).collect::<Vec<_>>()
        );
    }

    // -----------------------------------------------------------------------
    // (AV-33) chunk-range invariants, over generated sources rather than fixtures
    // -----------------------------------------------------------------------

    /// Byte offset where 1-based line `line` starts, and where the line after it starts.
    fn line_bounds(starts: &[usize], text_len: usize, line: u32) -> (usize, usize) {
        let idx = usize::try_from(line)
            .expect("u32 fits usize")
            .saturating_sub(1);
        let from = starts.get(idx).copied().unwrap_or(text_len);
        let to = starts.get(idx + 1).copied().unwrap_or(text_len);
        (from, to)
    }

    proptest::proptest! {
        #![proptest_config(proptest::test_runner::Config {
            cases: 256,
            ..proptest::test_runner::Config::default()
        })]

        /// Over any Rust-flavoured source and any pair of bounds, the chunks come back in file
        /// order, do not overlap, describe a line range that runs forwards, and never outnumber
        /// the chunk bound.
        ///
        /// The example tests around this one each pin **one** fixture through **one** path
        /// (definition chunking, the scope fallback, the chunk-count fallback, the thin-piece
        /// merges). None of them says anything about what the pieces look like relative to
        /// *each other*, and the pieces are assembled by four writers ([`emit_def`],
        /// [`push_interstitial`], [`fill_gaps`], [`line_chunk_whole_file`]) that only meet in
        /// [`settle`] -- an off-by-one in any one of them is an overlap the fixtures cannot
        /// reach because it needs a particular neighbour. Generating the neighbours is what
        /// this test is for.
        ///
        /// A [`Chunk`] exposes no byte range, only [`Chunk::line_range`] and [`Chunk::content`],
        /// so the byte-level claim is checked through a witness instead: the content is the
        /// range's text with the end trimmed, so each chunk has to start somewhere on its own
        /// start line, at or past
        /// the smallest end its predecessor can have (its start plus its content), with the
        /// text there beginning with its content. Taking the *smallest* such offset for every
        /// chunk in turn is complete -- if the real ranges are sorted and disjoint, the walk
        /// succeeds -- so a failure is a real overlap or a real reordering, never a false
        /// alarm. The search stops *before* the next line's first byte: a range that begins
        /// on the reported line begins at or before that line's newline, which [`line_of`]
        /// counts as the last byte of the line, so even a gap chunk whose content opens with
        /// that newline is admitted, while a chunk whose reported line slipped back by one
        /// is not (codex P2 round 2 on PR #281). The end line is tied to the witness as well:
        /// no earlier than the line holding the content's last byte, no later than the line
        /// where the whitespace after it runs out -- a pair rather than an equality because
        /// [`Chunk::content`] is the piece with its end trimmed, so the exact end byte is not
        /// recoverable (codex P2 round 3). The line-level shadow of the same fact, that
        /// one chunk's last line is no later
        /// than the next chunk's first, is asserted as well because its message is the one a
        /// reader can check against the source by eye.
        ///
        /// Every generated definition is textually unique -- its identifier carries the token's
        /// index -- because the witness is only as strong as the text is unambiguous: two
        /// identical definitions on one line would let a chunk emitted twice match the second
        /// occurrence instead of the first and pass as disjoint (codex P2 on PR #281).
        ///
        /// Both fallbacks are reached: the smallest [`Bounds::scope_depth`] refuses anything
        /// nested, and a [`Bounds::chunks`] of one refuses any file with two definitions.
        #[test]
        fn prop_chunks_are_ordered_disjoint_and_within_the_chunk_bound(
            tokens in proptest::collection::vec(
                proptest::sample::select(vec![
                    "fn f{n}(){}\n",
                    "mod m{n}{\n",
                    "}\n",
                    "struct S{n};\n",
                    "// c\n",
                    "\n",
                    "const X{n}: u32 = 1;\n",
                    "type T{n} = u8;\n",
                    "fn g{n}(a: u32) -> u32 { a + 1 }",
                    "pub fn h{n}() {}",
                ]),
                0..64,
            ),
            budget in 1usize..4000,
            scope_depth in 1usize..70,
            chunks in 1usize..600,
        ) {
            // `{n}` is the token's position in the sequence, so no two definitions share a name.
            let src: String = tokens
                .iter()
                .enumerate()
                .map(|(n, t)| t.replace("{n}", &n.to_string()))
                .collect();
            let grammar = static_rust::grammar().expect("rust grammar builds");
            let doc = chunk_source_capped(
                &grammar,
                budget,
                src.as_bytes(),
                &src,
                "src/lib.rs",
                Bounds { scope_depth, chunks },
            )
            .expect("a generated file still parses");

            proptest::prop_assert!(
                doc.chunks.len() <= chunks,
                "{} chunks came back under a bound of {chunks} (source {src:?})",
                doc.chunks.len()
            );

            let starts = line_starts(&src);
            let mut min_end = 0usize;
            let mut prev_last_line = 1u32;
            for (i, chunk) in doc.chunks.iter().enumerate() {
                proptest::prop_assert!(
                    chunk.line_range.is_some(),
                    "chunk {i} has no line range (source {src:?})"
                );
                let (first, last) = chunk.line_range.expect("asserted above");
                proptest::prop_assert!(
                    first <= last,
                    "chunk {i} runs backwards, lines {first}..{last} (source {src:?})"
                );
                proptest::prop_assert!(
                    prev_last_line <= first,
                    "chunk {i} starts on line {first}, before chunk {} ended on line \
                     {prev_last_line} (source {src:?})",
                    i.wrapping_sub(1)
                );
                prev_last_line = last;

                proptest::prop_assert!(
                    !chunk.content.is_empty(),
                    "chunk {i} has no content (source {src:?})"
                );
                let (line_from, line_to) = line_bounds(&starts, src.len(), first);
                let candidates: Vec<usize> = (line_from.max(min_end)..line_to)
                    .filter(|&p| src.get(p..).is_some_and(|rest| rest.starts_with(&chunk.content)))
                    .collect();
                proptest::prop_assert!(
                    !candidates.is_empty(),
                    "chunk {i} ({:?}) fits nowhere on its line {first} at or past byte \
                     {min_end}, where the chunk before it ends at the earliest (source {src:?})",
                    chunk.content
                );
                let witness = candidates[0];
                // The piece ended somewhere in the whitespace after its content, so the reported
                // end line is at least the line of the content's last byte and at most the line
                // where that whitespace run stops -- measured from the last candidate, which
                // reaches furthest.
                let floor = line_of(&starts, witness + chunk.content.len() - 1);
                let run_from = candidates[candidates.len() - 1] + chunk.content.len();
                let run_to = src
                    .get(run_from..)
                    .and_then(|rest| rest.find(|c: char| !c.is_whitespace()))
                    .map_or(src.len(), |k| run_from + k);
                let ceiling = line_of(&starts, run_to.saturating_sub(1));
                proptest::prop_assert!(
                    floor <= last && last <= ceiling,
                    "chunk {i} ({:?}) reports its end on line {last}, but its content ends on \
                     line {floor} and the whitespace after it on line {ceiling} (source {src:?})",
                    chunk.content
                );
                min_end = witness + chunk.content.len();
            }
        }
    }
}

/// [`line_of`] needs no grammar, so its property lives outside the gated module and runs
/// under every feature set.
#[cfg(test)]
mod line_props {
    use super::{line_of, line_starts};

    proptest::proptest! {
        #![proptest_config(proptest::test_runner::Config {
            cases: 256,
            ..proptest::test_runner::Config::default()
        })]

        /// [`line_of`] is one plus the newlines before the offset, is never below one, and
        /// never decreases as the offset grows -- for offsets past the end of the text as well,
        /// which the chunker reaches through `range.end.saturating_sub(1)` on a range that ends
        /// at `text.len()`.
        ///
        /// The example tests exercise [`line_of`] only through whole chunks whose ranges land
        /// on definition boundaries; the binary search inside it has two arms (`Ok` for an
        /// offset that *is* a line start, `Err` for one inside a line) and the fixtures do not
        /// say which arm they took. Generated offsets land on both, on the first byte of the
        /// text, on the last, and beyond it.
        #[test]
        fn prop_line_of_counts_newlines_and_is_monotone(
            s in "[\\PC\\n]{0,512}",
            off in 0usize..600,
            step in 0usize..600,
        ) {
            let starts = line_starts(&s);
            let seen = s.as_bytes()[..off.min(s.len())]
                .iter()
                .filter(|b| **b == b'\n')
                .count();
            let line = line_of(&starts, off);
            proptest::prop_assert!(line >= 1, "line numbers are 1-based, got {line}");
            proptest::prop_assert_eq!(
                usize::try_from(line).expect("u32 fits usize"),
                seen + 1,
                "offset {} of {:?}",
                off,
                s
            );
            proptest::prop_assert!(
                line <= line_of(&starts, off + step),
                "line_of went backwards between offsets {} and {} in {:?}",
                off,
                off + step,
                s
            );
        }
    }
}
