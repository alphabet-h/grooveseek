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
        let scope = node.map(|n| scope_chain(n, text)).unwrap_or_default();
        defs.push(Def {
            kind,
            name,
            covered: start..tag.range.end,
            scope,
            children: Vec::new(),
            depth: 0,
        });
    }

    link_containment(&mut defs);

    let mut pieces: Vec<Piece> = Vec::new();
    let roots: Vec<usize> = (0..defs.len()).filter(|i| defs[*i].depth == 0).collect();
    for i in &roots {
        emit_def(*i, &defs, text, budget, &title, &mut pieces);
    }
    fill_gaps(&roots, &defs, text, budget, &title, &mut pieces);

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

/// Names of the enclosing scopes, outermost first.
///
/// Walks real parents rather than the definition tree because the two disagree: Rust's tags
/// query captures `impl` blocks as references, not definitions, so `impl Database` never
/// becomes a definition node — yet it is exactly the context that tells two `open` methods
/// apart.
fn scope_chain(node: Node, text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cursor = node.parent();
    while let Some(parent) = cursor {
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
    out
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
        for range in split_by_lines(text, &def.covered, budget) {
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
            push_gap(cursor..span.start, &context_parts, text, budget, out);
        }
        cursor = span.end.max(cursor);
    }
    if cursor < text.len() {
        push_gap(cursor..text.len(), &context_parts, text, budget, out);
    }
}

fn push_gap(
    range: Range<usize>,
    context_parts: &[String],
    text: &str,
    budget: usize,
    out: &mut Vec<Piece>,
) {
    for r in split_by_lines(text, &range, budget) {
        out.push(Piece {
            range: r,
            heading: None,
            level: None,
            symbol_kind: None,
            context_parts: context_parts.to_vec(),
            droppable: true,
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
