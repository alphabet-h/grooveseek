//! docx (`.docx`) parser. zip + quick-xml で `word/document.xml` を読み、段落が見出しかどうかは
//! `word/styles.xml` の style 定義で決める (feature-62)。
use std::collections::{HashMap, HashSet};
use std::io::Cursor;

use anyhow::{Result, anyhow};
use quick_xml::events::{BytesStart, Event};
use quick_xml::reader::Reader;

use super::{Chunk, ParsedDocument, Parser, single_text_chunk};

pub struct DocxParser {
    /// (feature-61) 文書 1 本の展開後合計の上限 (`[index].max_decompressed_size`)。
    decompressed_budget: u64,
}

impl DocxParser {
    /// (feature-61) 展開 budget を `budget` bytes にした parser。[`super::Registry`] が
    /// `[index].max_decompressed_size` から作る。
    pub fn with_budget(budget: u64) -> Self {
        Self {
            decompressed_budget: budget,
        }
    }
}

impl Default for DocxParser {
    /// 展開 budget = [`super::DEFAULT_MAX_DECOMPRESSED_BYTES`]。
    fn default() -> Self {
        Self::with_budget(super::DEFAULT_MAX_DECOMPRESSED_BYTES)
    }
}

impl Parser for DocxParser {
    fn extension(&self) -> &'static str {
        "docx"
    }

    fn is_binary(&self) -> bool {
        true
    }

    fn parse(&self, raw: &str, path_hint: &str, _exclude_headings: &[&str]) -> ParsedDocument {
        single_text_chunk(raw, path_hint)
    }

    fn parse_bytes_inner(
        &self,
        bytes: &[u8],
        path_hint: &str,
        exclude_headings: &[&str],
    ) -> Result<ParsedDocument> {
        parse_with_budget(bytes, path_hint, exclude_headings, self.decompressed_budget)
    }

    /// (feature-61) The read path keeps the built-in budget whatever `[index]`
    /// set this parser up with ([`super::Parser::parse_bytes_for_read_inner`]).
    fn parse_bytes_for_read_inner(
        &self,
        bytes: &[u8],
        path_hint: &str,
        exclude_headings: &[&str],
    ) -> Result<ParsedDocument> {
        parse_with_budget(
            bytes,
            path_hint,
            exclude_headings,
            super::DEFAULT_MAX_DECOMPRESSED_BYTES,
        )
    }
}

/// The one docx parse; the two [`super::Parser`] entries differ only in the
/// decompression budget they pass as `cap`.
fn parse_with_budget(
    bytes: &[u8],
    path_hint: &str,
    exclude_headings: &[&str],
    cap: u64,
) -> Result<ParsedDocument> {
    let mut zip = zip::ZipArchive::new(Cursor::new(bytes))
        .map_err(|e| anyhow!("{path_hint}: cannot open docx zip (corrupt or encrypted): {e}"))?;
    // 文書単位の累積展開済みバイト数。word/document.xml + docProps/core.xml
    // の両読み出しで共有し、累積が cap を超えたら Err にする (codex P2,
    // PR #70 round 2 zip-bomb hardening: 個々のエントリが cap 未満でも
    // 積算で無制限に膨らむのを防ぐ)。(feature-62) word/styles.xml も同じ budget で
    // 読むが、収まらなければ文書を諦めずに styles.xml だけを読み飛ばす。
    let mut budget: u64 = 0;
    let doc_xml =
        super::ooxml::read_zip_part(&mut zip, path_hint, "word/document.xml", &mut budget, cap)?
            .ok_or_else(|| anyhow!("{path_hint}: word/document.xml missing"))?;
    // frontmatter を先に取得し、context の title に使う (取得順を入れ替え)。
    let frontmatter = super::ooxml::core_xml_frontmatter(&mut zip, path_hint, &mut budget, cap)?;
    // (feature-62) After core.xml, so the parts that were always read take the budget in the
    // order they always did, and styles.xml gets what they leave.
    let styles = read_style_table(&mut zip, path_hint, &mut budget, cap);
    super::ooxml::warn_if_truncated(path_hint, "word/document.xml", &doc_xml);
    let chunks = parse_document_xml(
        &doc_xml,
        exclude_headings,
        frontmatter.title.as_deref(),
        styles.as_ref(),
    );
    let raw_content = super::join_chunk_bodies(&chunks);
    Ok(ParsedDocument {
        frontmatter,
        chunks,
        raw_content,
        frontmatter_error: None,
    })
}

// ---------------------------------------------------------------------------
// word/styles.xml → style table (feature-62)
// ---------------------------------------------------------------------------

/// Where a docx keeps its styles. A fixed path, like `word/document.xml`'s: a document whose
/// styles part is named otherwise is read the way one without styles is.
const STYLES_PART: &str = "word/styles.xml";

/// What a document whose styles part is left out loses, as the stderr lines say it.
const STYLES_SKIPPED_MEANS: &str = "headings fall back to style IDs";

/// `word/styles.xml` as a [`StyleTable`], or `None` when the document has no usable one: the
/// part is missing, over the decompression budget on its own, would take the document past
/// it ([`super::ooxml::read_optional_zip_part`]), or is not readable as a styles part
/// ([`StyleTable::parse`]). Each case but a missing part is named on stderr in one line; a part
/// the zip layer cannot open or inflate falls back the same way without a line, as
/// [`super::ooxml::read_zip_part`] always has. With `None` every paragraph's heading is decided
/// by its style ID's spelling, as before.
fn read_style_table(
    zip: &mut zip::ZipArchive<Cursor<&[u8]>>,
    path_hint: &str,
    budget: &mut u64,
    cap: u64,
) -> Option<StyleTable> {
    let xml = super::ooxml::read_optional_zip_part(
        zip,
        path_hint,
        STYLES_PART,
        budget,
        cap,
        STYLES_SKIPPED_MEANS,
    )?;
    match StyleTable::parse(&xml) {
        Ok(table) => Some(table),
        Err(reason) => {
            eprintln!("{}", unreadable_styles_message(path_hint, &reason));
            None
        }
    }
}

/// The line a `word/styles.xml` that is not used gets on stderr. ASCII only. Not
/// [`super::ooxml::warn_if_truncated`]'s line: that one says "the text is cut here and the
/// rest is used", and a styles part is used whole or not at all.
fn unreadable_styles_message(path_hint: &str, reason: &StylesUnusable) -> String {
    format!(
        "warning: {path_hint}: {STYLES_PART} is not a readable styles part ({reason}); {STYLES_SKIPPED_MEANS}"
    )
}

/// The paragraph styles one `word/styles.xml` defines, by style ID.
///
/// A paragraph names its style by an ID the writing application chooses
/// (`<w:pStyle w:val="1">`): Word in Japanese writes `1` .. `6` for its built-in headings
/// and names them `heading 1` .. `heading 6`. Whether a style is a heading is what its
/// definition here says -- its name and the styles it is based on ([`StyleTable::verdict`])
/// -- and not the ID's spelling, which decides only an ID this table does not hold.
/// ADR-0027 records the rule.
#[derive(Debug)]
struct StyleTable {
    styles: HashMap<Vec<u8>, StyleDef>,
}

/// What one paragraph style's definition says that the heading rule reads.
#[derive(Debug, Default)]
struct StyleDef {
    /// `w:name/@w:val`, entities resolved. `None` when the element or its `w:val` is missing
    /// or the value is blank: the style ID then stands in for the name.
    name: Option<String>,
    /// `w:basedOn/@w:val`, as a [`style_id_key`].
    based_on: Option<Vec<u8>>,
    /// The `w:outlineLvl/@w:val` directly under the style's own `w:pPr`, when it is a whole
    /// number from 0 to 9; any other value counts as not set.
    outline_lvl: Option<u8>,
}

/// What [`StyleTable::verdict`] says about the paragraphs one style ID styles.
#[derive(Debug, PartialEq)]
enum StyleVerdict {
    /// The table holds no paragraph style by this ID: the ID's spelling decides, as before.
    NotDefined,
    /// Body text.
    Body,
    /// A heading at this chunk level ([`chunk_level`]).
    Heading(u8),
}

/// How many styles [`StyleTable::verdict`] looks at, from the paragraph's own style through
/// its `w:basedOn` parents, the paragraph's style included. A cycle ends here too, which is
/// why the walk keeps no visited set. Word's built-in headings are two deep (`1` based on
/// `a`).
const MAX_STYLE_CHAIN: usize = 16;

/// Why a `word/styles.xml` is not used ([`StyleTable::parse`]). The `Display` is the reason
/// [`unreadable_styles_message`] puts in parentheses, ASCII only.
#[derive(Debug, PartialEq)]
enum StylesUnusable {
    /// quick-xml stopped with an error, such as a tag cut short.
    Xml(String),
    /// The input ended with the root still open, and this many elements open in all.
    Unclosed(usize),
    /// There is no element at all.
    NoRoot,
    /// The first element is not `w:styles`.
    RootNotStyles,
}

impl std::fmt::Display for StylesUnusable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Xml(e) => write!(f, "XML error: {e}"),
            Self::Unclosed(n) => write!(f, "ended with {n} element(s) still open"),
            Self::NoRoot => f.write_str("no root element"),
            Self::RootNotStyles => f.write_str("root element is not w:styles"),
        }
    }
}

impl StyleTable {
    /// Read `xml` as a `word/styles.xml`, or say why it is not used.
    ///
    /// A part that is not wholly readable is not used at all (ADR-0027): a table that stops at
    /// the break would end a `w:basedOn` walk early and call "body" a paragraph the spelling
    /// rule calls a heading, and nothing in the result would show which. The root is the
    /// first element -- a declaration, a comment or a byte-order mark before it is not -- and
    /// it has to be `w:styles` and to close before the input ends. An attribute quick-xml
    /// rejects, on any element of the part, is an XML error too ([`StylesWalk::element`]).
    ///
    /// Only the root's direct `<w:style>` children are styles, and of each only its own
    /// `w:type` / `w:styleId`, its direct `w:name` / `w:basedOn` and the `w:outlineLvl`
    /// directly under its own `w:pPr` are read, not what `w:rPr`, `w:tblStylePr` or
    /// `w:latentStyles` hold. The first definition of a style ID wins whatever its type, and
    /// only paragraph styles (`w:type="paragraph"`, or no `w:type`) are kept.
    fn parse(xml: &[u8]) -> Result<Self, StylesUnusable> {
        let mut reader = Reader::from_reader(xml);
        let mut buf = Vec::new();
        let mut walk = StylesWalk::default();
        loop {
            match reader.read_event_into(&mut buf) {
                Ok(Event::Start(e)) => walk.element(&e, true)?,
                Ok(Event::Empty(e)) => walk.element(&e, false)?,
                Ok(Event::End(_)) => walk.end(),
                Ok(Event::Eof) => return walk.finish(),
                Err(e) => return Err(StylesUnusable::Xml(e.to_string())),
                _ => {}
            }
            buf.clear();
        }
    }

    /// Whether the paragraphs styled `style_id` (a [`style_id_key`]) are a heading.
    ///
    /// The walk starts at that style and follows `w:basedOn` for at most [`MAX_STYLE_CHAIN`]
    /// styles, stopping at a style with no parent or whose parent the table does not hold (a
    /// missing style, or one that is not a paragraph style). The first style whose name -- or
    /// style ID, when it has no name -- reads as a [`heading_number`] is the heading ancestor
    /// and gives the level. Of the styles before it, from the paragraph's own on, the first
    /// that sets an outline level decides whether it counts: 9 turns the paragraph back into
    /// body text (Word's `TOC Heading` does this), 0 to 8 leaves it a heading. The heading
    /// ancestor's own outline level and those above it are not read, and an outline level
    /// never makes a heading by itself. No heading ancestor within the bound is body text,
    /// not a fallback to the spelling.
    fn verdict(&self, style_id: &[u8]) -> StyleVerdict {
        let Some(mut style) = self.styles.get(style_id) else {
            return StyleVerdict::NotDefined;
        };
        let mut id = style_id;
        let mut nearest_outline: Option<u8> = None;
        for _ in 0..MAX_STYLE_CHAIN {
            let spelled;
            let label = match &style.name {
                Some(name) => name.as_str(),
                None => {
                    spelled = String::from_utf8_lossy(id);
                    &*spelled
                }
            };
            if let Some(n) = heading_number(label) {
                return if nearest_outline == Some(9) {
                    StyleVerdict::Body
                } else {
                    StyleVerdict::Heading(chunk_level(n))
                };
            }
            if nearest_outline.is_none() {
                nearest_outline = style.outline_lvl;
            }
            let Some(parent_id) = style.based_on.as_deref() else {
                return StyleVerdict::Body;
            };
            let Some(parent) = self.styles.get(parent_id) else {
                return StyleVerdict::Body;
            };
            id = parent_id;
            style = parent;
        }
        StyleVerdict::Body
    }
}

/// One pass over a `word/styles.xml` ([`StyleTable::parse`]).
#[derive(Default)]
struct StylesWalk {
    /// Elements open right now: 1 inside the root, 2 inside a `<w:style>`, 3 inside one of its
    /// children.
    depth: usize,
    root_seen: bool,
    root_closed: bool,
    /// Every style ID defined so far, whatever its type: the first definition wins.
    seen_ids: HashSet<Vec<u8>>,
    styles: HashMap<Vec<u8>, StyleDef>,
    /// The `<w:style>` open right now.
    current: Option<PendingStyle>,
    /// The local name of the `<w:style>` child open right now, so an outline level is taken
    /// only from directly under the style's own `w:pPr`.
    open_child: Option<Vec<u8>>,
}

/// A `<w:style>` being read.
#[derive(Default)]
struct PendingStyle {
    /// `w:styleId`, as a [`style_id_key`]. A style without one cannot be referred to.
    id: Option<Vec<u8>>,
    /// `w:type` is `paragraph` or absent.
    paragraph: bool,
    /// A `w:name` was met: the first one is the name, blank or not.
    name_read: bool,
    def: StyleDef,
}

impl StylesWalk {
    /// A `Start` (`opens`) or `Empty` element, met with [`Self::depth`] elements open.
    ///
    /// Every attribute of every element is checked first, the ones the walk does not read
    /// included (`w:rPr`, `w:latentStyles`, `w:docDefaults` and the rest): an attribute
    /// quick-xml rejects anywhere makes the part not wholly readable.
    fn element(&mut self, e: &BytesStart, opens: bool) -> Result<(), StylesUnusable> {
        for attr in e.attributes() {
            attr.map_err(|err| StylesUnusable::Xml(err.to_string()))?;
        }
        let qname = e.name();
        let name = super::ooxml_local(qname.as_ref());
        let depth = self.depth;
        if opens {
            self.depth += 1;
        }
        if !self.root_seen {
            if name != b"styles" {
                return Err(StylesUnusable::RootNotStyles);
            }
            self.root_seen = true;
            self.root_closed = !opens;
            return Ok(());
        }
        if self.root_closed {
            return Ok(());
        }
        match depth {
            1 if name == b"style" => {
                let style = PendingStyle {
                    id: attr_value(e, b"styleId")?.map(|v| style_id_key(&v)),
                    paragraph: attr_value(e, b"type")?.is_none_or(|t| t == b"paragraph"),
                    ..PendingStyle::default()
                };
                if opens {
                    self.current = Some(style);
                } else {
                    self.settle(style);
                }
            }
            2 => {
                if opens {
                    self.open_child = Some(name.to_vec());
                }
                if let Some(style) = self.current.as_mut() {
                    match name {
                        b"name" if !style.name_read => {
                            style.name_read = true;
                            style.def.name = attr_value(e, b"val")?.and_then(|v| style_name(&v));
                        }
                        b"basedOn" if style.def.based_on.is_none() => {
                            style.def.based_on = attr_value(e, b"val")?.map(|v| style_id_key(&v));
                        }
                        _ => {}
                    }
                }
            }
            3 if name == b"outlineLvl" && self.open_child.as_deref() == Some(b"pPr".as_slice()) => {
                if let Some(style) = self.current.as_mut()
                    && style.def.outline_lvl.is_none()
                {
                    style.def.outline_lvl = attr_value(e, b"val")?.and_then(|v| outline_level(&v));
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// An `End` event.
    fn end(&mut self) {
        self.depth = self.depth.saturating_sub(1);
        if self.root_closed {
            return;
        }
        match self.depth {
            0 => self.root_closed = true,
            1 => {
                if let Some(style) = self.current.take() {
                    self.settle(style);
                }
            }
            2 => self.open_child = None,
            _ => {}
        }
    }

    /// A finished `<w:style>`: its ID counts as defined whatever its type, and a paragraph
    /// style goes into the table unless that ID was defined before.
    fn settle(&mut self, style: PendingStyle) {
        let Some(id) = style.id else {
            return;
        };
        if self.seen_ids.insert(id.clone()) && style.paragraph {
            self.styles.insert(id, style.def);
        }
    }

    /// The end of the input.
    fn finish(self) -> Result<StyleTable, StylesUnusable> {
        if !self.root_seen {
            return Err(StylesUnusable::NoRoot);
        }
        if !self.root_closed {
            return Err(StylesUnusable::Unclosed(self.depth));
        }
        Ok(StyleTable {
            styles: self.styles,
        })
    }
}

/// The value of `e`'s attribute whose local name is `local`, as written (entities not
/// resolved), or `None` when it has none.
///
/// Every attribute of `e` is read, not only up to the one asked for, and an attribute
/// quick-xml rejects -- a name given twice, a value without quotes -- is a
/// [`StylesUnusable::Xml`], so [`StyleTable::parse`] drops the whole part rather than read a
/// style from a tag it could not read.
fn attr_value(e: &BytesStart, local: &[u8]) -> Result<Option<Vec<u8>>, StylesUnusable> {
    let mut found = None;
    for attr in e.attributes() {
        let attr = attr.map_err(|err| StylesUnusable::Xml(err.to_string()))?;
        if found.is_none() && super::ooxml_local(attr.key.as_ref()) == local {
            found = Some(attr.value.into_owned());
        }
    }
    Ok(found)
}

/// A style ID as the table compares it: the attribute's value with its XML entities resolved
/// (`&amp;` and `&#38;` are the same ID), or the value as written when it is not UTF-8 or holds
/// an entity that does not resolve. Compared byte for byte, case included: lower-casing would
/// merge `Heading1` and `heading1`, which a document may define as two styles, and a lossy
/// decode would merge every ID that is not UTF-8 into one. Both sides go through this one
/// function -- `w:styleId`, and the `w:basedOn` / `w:pStyle` values that refer to it.
fn style_id_key(raw: &[u8]) -> Vec<u8> {
    match std::str::from_utf8(raw) {
        Ok(text) => match quick_xml::escape::unescape(text) {
            Ok(unescaped) => unescaped.into_owned().into_bytes(),
            Err(_) => raw.to_vec(),
        },
        Err(_) => raw.to_vec(),
    }
}

/// A style's name from its `w:name/@w:val`: resolved like a [`style_id_key`], then text, and
/// `None` when blank.
fn style_name(raw: &[u8]) -> Option<String> {
    let name = String::from_utf8_lossy(&style_id_key(raw)).into_owned();
    (!name.trim().is_empty()).then_some(name)
}

/// A `w:outlineLvl/@w:val` that is a whole number from 0 to 9, or `None`.
fn outline_level(raw: &[u8]) -> Option<u8> {
    std::str::from_utf8(raw)
        .ok()?
        .parse::<u8>()
        .ok()
        .filter(|n| *n <= 9)
}

// ---------------------------------------------------------------------------
// word/document.xml → heading-hierarchy chunks
// ---------------------------------------------------------------------------

/// 段落内の区切り要素 (`<w:br/>` / `<w:cr/>` / `<w:tab/>`) を空白文字として
/// 積む (AU-13)。
///
/// これらは `<w:t>` の外側に兄弟として現れるので、無視すると前後の
/// `<w:t>` が直結して語が繋がる: `<w:t>行1</w:t><w:br/><w:t>行2</w:t>` が
/// `行1行2` になり、"行1" でも "行2" でも引っかからない造語ができる。
///
/// `<w:br/>` / `<w:cr/>` は改行、`<w:tab/>` はタブ。段落の確定時に
/// `trim()` されるので、先頭・末尾に付いても本文には残らない。
fn push_intra_paragraph_separator(local_name: &[u8], para_text: &mut String) {
    match local_name {
        b"br" | b"cr" => para_text.push('\n'),
        b"tab" => para_text.push('\t'),
        _ => {}
    }
}

/// One stretch of a document between headings, as [`parse_document_xml`] gathers it before it
/// becomes a [`Chunk`]: the heading that opens it (`None` for the text before the first
/// heading), that heading's chunk level, the headings above it when it was read, and its
/// body paragraphs, each trimmed, joined by `\n`.
struct DocxSection {
    heading: Option<String>,
    level: Option<u8>,
    ancestry: Vec<String>,
    body: String,
}

/// (feature-63) The sections of a document that become its chunks, in order.
///
/// A section whose body is empty once trimmed is not a chunk of its own. Its heading's text
/// is held and opens the body of the next section that has a body, one line per heading, in
/// document order, whatever the levels -- so the words of a chapter heading followed directly
/// by a section heading stay in the indexed content in every context mode, and the joined
/// bodies read them where they stood. A heading with no text adds no line. Headings still
/// held at the end, with no body after them, are dropped. The section that receives them
/// keeps its own heading, level and ancestry.
///
/// A document none of whose sections has a body is cut as before: a chunk per heading, each
/// with an empty body, so a document of headings alone stays in the index.
fn fold_empty_heading_sections(sections: Vec<DocxSection>) -> Vec<DocxSection> {
    if sections.iter().all(|s| s.body.trim().is_empty()) {
        return sections
            .into_iter()
            .filter(|s| s.heading.is_some())
            .collect();
    }
    let mut held: Vec<String> = Vec::new();
    let mut kept: Vec<DocxSection> = Vec::new();
    for mut section in sections {
        if section.body.trim().is_empty() {
            if let Some(heading) = section.heading
                && !heading.trim().is_empty()
            {
                held.push(heading);
            }
            continue;
        }
        if !held.is_empty() {
            held.push(std::mem::take(&mut section.body));
            section.body = held.join("\n");
            held.clear();
        }
        kept.push(section);
    }
    kept
}

/// (feature-64) The row being read at one table level of [`parse_document_xml`]: the field
/// each closed cell left and the cell that is open, if any.
#[derive(Default)]
struct TableRow {
    /// One field per closed cell, in document order: its paragraphs joined with a space, `""`
    /// for a cell with no text.
    fields: Vec<String>,
    /// The cell between its `<w:tc>` and `</w:tc>`.
    open: Option<OpenCell>,
}

/// (feature-64) A cell of a [`TableRow`] that has not closed yet.
#[derive(Default)]
struct OpenCell {
    /// Its paragraphs so far, each trimmed and not empty.
    paragraphs: Vec<String>,
    /// Whether part of the row was already written out while this cell was open, by a heading
    /// in it or a table nested in it. Such a cell adds a field at `</w:tc>` only when text
    /// came after that point.
    split: bool,
}

impl TableRow {
    /// `</w:tc>` (and a `<w:tc>` that finds a cell still open): the open cell becomes a field
    /// -- its paragraphs joined with a space, `""` when it had none -- unless it was
    /// [split](OpenCell::split) and nothing came after.
    fn close_cell(&mut self) {
        if let Some(cell) = self.open.take()
            && !(cell.split && cell.paragraphs.is_empty())
        {
            self.fields.push(cell.paragraphs.join(" "));
        }
    }

    /// `</w:tr>`: the row's fields, its open cell closed first, leaving the row empty.
    fn end_row(&mut self) -> Vec<String> {
        self.close_cell();
        std::mem::take(&mut self.fields)
    }

    /// (feature-64, R1.5 / R1.6 / R1.7) The fields gathered so far, taken out: the closed
    /// cells', then the open cell's paragraphs as one more field when it has any. The open cell
    /// stays open, marked [split](OpenCell::split), so it adds a field at `</w:tc>` only for
    /// text that comes after.
    fn take_so_far(&mut self) -> Vec<String> {
        let mut fields = std::mem::take(&mut self.fields);
        if let Some(cell) = self.open.as_mut() {
            if !cell.paragraphs.is_empty() {
                fields.push(cell.paragraphs.join(" "));
                cell.paragraphs.clear();
            }
            cell.split = true;
        }
        fields
    }
}

/// (feature-64) The line a table row adds to a section's body: its fields joined with a tab,
/// a single field being its text alone, or `None` when every field is empty.
fn row_line(fields: &[String]) -> Option<String> {
    if fields.iter().all(|field| field.is_empty()) {
        None
    } else {
        Some(fields.join("\t"))
    }
}

/// (feature-64) Add `line` to a section's `body` as one more line: after a `\n` when the body
/// has text already. A body paragraph of [`parse_document_xml`] and a table row
/// ([`push_table_line`]) are both added to the body through this.
fn push_body_line(body: &mut String, line: &str) {
    if !body.is_empty() {
        body.push('\n');
    }
    body.push_str(line);
}

/// (feature-64) Add the [`row_line`] of `fields` to the last section's body the way a body
/// paragraph is added in [`parse_document_xml`] ([`push_body_line`]), and not at all while
/// `excluded`.
fn push_table_line(sections: &mut [DocxSection], excluded: bool, fields: &[String]) {
    if excluded {
        return;
    }
    let Some(line) = row_line(fields) else {
        return;
    };
    let last = sections.last_mut().expect("sections is never empty");
    push_body_line(&mut last.body, &line);
}

/// (feature-64) [`push_intra_paragraph_separator`], except that inside an open cell (`in_cell`)
/// a `<w:br/>` / `<w:cr/>` is a space, so a table row stays one line. One space per element;
/// runs are not folded.
fn push_paragraph_separator(local_name: &[u8], in_cell: bool, para_text: &mut String) {
    match local_name {
        b"br" | b"cr" if in_cell => para_text.push(' '),
        _ => push_intra_paragraph_separator(local_name, para_text),
    }
}

/// `word/document.xml` を段落 (`<w:p>`) 単位で読み、見出し段落を見出し境界として Markdown 同様の
/// 階層チャンクに変換する。段落が見出しかどうかは、その `<w:pStyle>` を
/// [`heading_level_from_attr`] が `styles` (`word/styles.xml` の style 表、使えなければ `None`)
/// で決める (feature-62)。
///
/// (feature-64) A table (`w:tbl`) is written one row per line: each cell's paragraphs are
/// joined with a space into one field ([`OpenCell`]), the fields of a row with a tab
/// ([`row_line`]), an empty cell keeping its place as an empty field and a row of empty cells
/// adding nothing ([`push_table_line`]). A paragraph outside every `<w:tc>` takes the path a
/// body paragraph always took, so a document without a table is read as before.
///
/// (feature-63) Which sections become chunks, and where a heading with no body text goes, is
/// [`fold_empty_heading_sections`]'s.
fn parse_document_xml(
    xml: &[u8],
    excludes: &[&str],
    title: Option<&str>,
    styles: Option<&StyleTable>,
) -> Vec<Chunk> {
    let mut reader = Reader::from_reader(xml);
    reader.config_mut().trim_text(false);
    let mut buf = Vec::new();

    // (heading, level, 祖先見出しスナップショット, body) の raw セクション列を組む。
    // 見出し前本文は先頭の heading=None セクションに溜まる。
    let mut sections: Vec<DocxSection> = vec![DocxSection {
        heading: None,
        level: None,
        ancestry: Vec::new(),
        body: String::new(),
    }];

    let mut para_style: Option<u8> = None; // 見出し段落 → chunk level (2..=6)
    let mut para_text = String::new();
    let mut in_text = false;
    // 除外対象見出し配下かどうか。true の間は本文段落を一切 push しない (次の
    // 非除外見出しで解除、MarkdownParser::chunk_body と同じ excluded フラグ管理)。
    let mut excluded = false;
    // ancestry stack: 現在位置より浅い見出しの列 (level ASC)。markdown とは独立の
    // 実装 (docx はフラット段落列を集めてから chunk 化する構造のため)。exclude
    // された見出しも積む (E-6 の docx 版)。
    let mut stack: Vec<(u8, String)> = Vec::new();
    // (feature-64) One entry per `<w:tbl>` open around the reader, innermost last. Empty outside
    // tables, where every paragraph takes the path it always took.
    let mut tables: Vec<TableRow> = Vec::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => match super::ooxml_local(e.name().as_ref()) {
                b"p" => {
                    para_style = None;
                    para_text.clear();
                }
                b"pStyle" => para_style = heading_level_from_attr(&e, styles),
                b"t" => in_text = true,
                b"tbl" => {
                    // R1.6: a table inside a cell writes out the outer row so far first, so
                    // the text stays in document order.
                    if let Some(row) = tables.last_mut()
                        && row.open.is_some()
                    {
                        let so_far = row.take_so_far();
                        push_table_line(&mut sections, excluded, &so_far);
                    }
                    tables.push(TableRow::default());
                }
                b"tr" => {
                    // D3: a row left open before this one is written out, not dropped.
                    if let Some(row) = tables.last_mut() {
                        let left = row.end_row();
                        push_table_line(&mut sections, excluded, &left);
                    }
                }
                b"tc" => {
                    if let Some(row) = tables.last_mut() {
                        row.close_cell();
                        row.open = Some(OpenCell::default());
                    }
                }
                // `<w:br></w:br>` の形で来ることもある。Empty 版と同じ扱い。
                name => {
                    let in_cell = tables.last().is_some_and(|row| row.open.is_some());
                    push_paragraph_separator(name, in_cell, &mut para_text);
                }
            },
            Ok(Event::Empty(e)) => {
                // `e.name()` は一時値なので、`as_ref()` の借用元を束縛しておく。
                let qname = e.name();
                let name = super::ooxml_local(qname.as_ref());
                // `<w:pStyle w:val="Heading1"/>` は自己終端タグで来ることが多い。
                if name == b"pStyle" {
                    para_style = heading_level_from_attr(&e, styles);
                } else if name == b"tc" {
                    // (feature-64) `<w:tc/>`: an empty cell. `<w:tr/>` and `<w:tbl/>` add
                    // nothing and fall through below.
                    if let Some(row) = tables.last_mut() {
                        row.close_cell();
                        row.fields.push(String::new());
                    }
                } else {
                    let in_cell = tables.last().is_some_and(|row| row.open.is_some());
                    push_paragraph_separator(name, in_cell, &mut para_text);
                }
            }
            Ok(Event::Text(t)) if in_text => {
                // quick-xml 0.41 は `BytesText::unescape()` を廃止し、encoding
                // decode (`decode()`) と entity unescape (`escape::unescape()`)
                // を分離した (Task 3.2 前例踏襲)。
                let decoded = t.decode().unwrap_or_default();
                let text = quick_xml::escape::unescape(&decoded)
                    .map(|c| c.into_owned())
                    .unwrap_or_else(|_| decoded.into_owned());
                para_text.push_str(&text);
            }
            Ok(Event::GeneralRef(r)) if in_text => {
                // quick-xml 0.38+ は entity 参照 (`&amp;` 等) を `Event::Text`
                // に含めず `Event::GeneralRef` として別 event で届ける。ここを
                // 処理しないと `<w:t>A&amp;B</w:t>` の "&" が欠落する。
                para_text.push_str(&super::ooxml::resolve_general_ref(&r));
            }
            Ok(Event::End(e)) => match super::ooxml_local(e.name().as_ref()) {
                b"t" => in_text = false,
                b"p" => {
                    let text = para_text.trim().to_string();
                    // (feature-64) Inside an open cell a body paragraph is the cell's, not the
                    // section's.
                    let in_cell = tables.last().is_some_and(|row| row.open.is_some());
                    if let Some(level) = para_style {
                        // R1.5: a heading inside a cell writes out its row so far into the
                        // section it closes, before that section is closed.
                        if in_cell && let Some(row) = tables.last_mut() {
                            let so_far = row.take_so_far();
                            push_table_line(&mut sections, excluded, &so_far);
                        }
                        // stack を pop → 祖先確定 → この見出しを push (exclude でも積む = E-6)。
                        while let Some((l, _)) = stack.last() {
                            if *l >= level {
                                stack.pop();
                            } else {
                                break;
                            }
                        }
                        let ancestry: Vec<String> = stack.iter().map(|(_, h)| h.clone()).collect();
                        stack.push((level, text.clone()));

                        // 見出し段落。exclude 対象なら新規 section を作らず
                        // `excluded = true` にするだけ (= 次の非除外見出しまで
                        // 本文段落を丸ごと捨てる。無題 chunk としても残さない
                        // — MarkdownParser::chunk_body と同じ excluded フラグ管理)。
                        if excludes.iter().any(|ex| text.contains(ex)) {
                            excluded = true;
                        } else {
                            excluded = false;
                            sections.push(DocxSection {
                                heading: Some(text),
                                level: Some(level),
                                ancestry,
                                body: String::new(),
                            });
                        }
                    } else if in_cell && !text.is_empty() {
                        if let Some(cell) = tables.last_mut().and_then(|row| row.open.as_mut()) {
                            cell.paragraphs.push(text);
                        }
                    } else if !excluded && !text.is_empty() {
                        let last = sections.last_mut().expect("sections is never empty");
                        push_body_line(&mut last.body, &text);
                    }
                }
                b"tc" => {
                    if let Some(row) = tables.last_mut() {
                        row.close_cell();
                    }
                }
                b"tr" => {
                    if let Some(row) = tables.last_mut() {
                        let fields = row.end_row();
                        push_table_line(&mut sections, excluded, &fields);
                    }
                }
                b"tbl" => {
                    // D3: a row left open at `</w:tbl>` is written out, not dropped.
                    if let Some(mut row) = tables.pop() {
                        let left = row.end_row();
                        push_table_line(&mut sections, excluded, &left);
                    }
                }
                _ => {}
            },
            Ok(Event::Eof) => break,
            // 切れている場合の警告は `warn_if_truncated` (parse_bytes_inner の 1 パス) が出す。
            Err(_) => break,
            _ => {}
        }
        buf.clear();
    }

    // R1.7: a document cut off inside a table keeps what was read of it, innermost row first.
    while let Some(mut row) = tables.pop() {
        let so_far = row.take_so_far();
        push_table_line(&mut sections, excluded, &so_far);
    }

    fold_empty_heading_sections(sections)
        .into_iter()
        .enumerate()
        .map(|(i, s)| {
            // context parts: [title, ...ancestry, heading]
            let mut parts: Vec<&str> = Vec::with_capacity(s.ancestry.len() + 2);
            if let Some(t) = title {
                parts.push(t);
            }
            for a in &s.ancestry {
                parts.push(a);
            }
            if let Some(h) = &s.heading {
                parts.push(h);
            }
            let context = super::build_context(&parts);
            Chunk {
                index: i,
                heading: s.heading,
                level: s.level,
                content: s.body,
                context,
                line_range: None,
                symbol_kind: None,
            }
        })
        .collect()
}

/// The number `s` spells after `heading`, read the way groove has always read a
/// `<w:pStyle w:val>`: ASCII-lowercase it, take off a leading `heading`, trim what is left
/// (Unicode whitespace, U+3000 included) and read it as a `u8`. `heading 0` is `Some(0)`;
/// `heading`, `heading` with a full-width digit, `heading 1 char`, `heading 256` and
/// ` heading 1` (a space before it) are `None`.
///
/// (feature-62) The one reading of `heading N` in this parser; the spelling fallback in
/// [`heading_level_from_attr`] reads it directly, because it stops at a `heading 0` value
/// where it reads on past any other value that is not a heading. [`heading_number`] is it
/// with 0 left out, for a style name and for the style ID's spelling alike.
fn heading_digits(s: &str) -> Option<u8> {
    let lower = s.to_ascii_lowercase();
    lower
        .strip_prefix("heading")
        .and_then(|rest| rest.trim().parse::<u8>().ok())
}

/// The heading number `s` spells, 1 or more ([`heading_digits`] without 0): `heading 1`,
/// `Heading1` and `HEADING 1 ` are 1, and `heading 0` is not a heading.
fn heading_number(s: &str) -> Option<u8> {
    heading_digits(s).filter(|n| *n >= 1)
}

/// The chunk level of heading number `n`: one deeper than the heading, as Markdown's `#`
/// is a document title, up to 6 for every heading from 5 down. Matched rather than computed
/// as `n + 1`, so `heading 255` cannot overflow the `u8`.
fn chunk_level(n: u8) -> u8 {
    match n {
        0..=5 => n + 1,
        _ => 6,
    }
}

/// The chunk level of the paragraph a `<w:pStyle>` (`e`) styles, or `None` for body text.
///
/// (feature-62) With a usable `word/styles.xml` (`styles`), the style's definition decides
/// ([`StyleTable::verdict`]). The number the `w:val` spells ([`heading_digits`]), at
/// [`chunk_level`], decides only an ID that table does not hold, and every ID of a document
/// without one -- which is the whole rule before this feature: `Heading1` .. `Heading6` and
/// `heading 1` with a space count; `Normal`, `Title` and `1` do not, and a `heading 0` value
/// ends the search as body text. The spelling is read as written, entities left alone, as it
/// always was; the table is looked up by [`style_id_key`].
fn heading_level_from_attr(e: &BytesStart, styles: Option<&StyleTable>) -> Option<u8> {
    for attr in e.attributes().flatten() {
        if super::ooxml_local(attr.key.as_ref()) != b"val" {
            continue;
        }
        if let Some(table) = styles {
            match table.verdict(&style_id_key(&attr.value)) {
                StyleVerdict::Heading(level) => return Some(level),
                StyleVerdict::Body => return None,
                StyleVerdict::NotDefined => {}
            }
        }
        match heading_digits(&String::from_utf8_lossy(&attr.value)) {
            None => continue,
            Some(0) => return None,
            Some(n) => return Some(chunk_level(n)),
        }
    }
    None
}

/// (feature-62) docx bytes for the unit tests of this parser, of [`crate::server`] and of
/// [`crate::indexer`], so the three build a document one way. The integration tests (separate
/// test crates under the crate's tests directory) cannot reach a `cfg(test)` module and carry
/// a copy of their own.
#[cfg(test)]
pub(crate) mod fixture {
    use std::io::Write;
    use zip::write::SimpleFileOptions;

    /// The WordprocessingML namespace.
    pub(crate) const W_NS: &str = "http://schemas.openxmlformats.org/wordprocessingml/2006/main";

    /// The words of [`numeric_heading_docx`]. The file names the tests give it, the title, the
    /// headings and the bodies share no word, so a test cannot pass by finding one through
    /// another.
    pub(crate) const TITLE: &str = "Almanac";
    pub(crate) const PREFACE: &str = "walrus preface opening the pages before any chapter begins";
    pub(crate) const H_FIRST: &str = "Harbor";
    pub(crate) const B_FIRST: &str = "kettle passage under the opening chapter with enough words";
    pub(crate) const H_NESTED: &str = "Lantern";
    pub(crate) const B_NESTED: &str = "violin passage under the nested section with enough words";
    pub(crate) const H_SECOND: &str = "Orchard";
    pub(crate) const B_SECOND: &str = "glacier passage under the closing chapter with enough words";
    pub(crate) const HEADINGS: [&str; 3] = [H_FIRST, H_NESTED, H_SECOND];
    pub(crate) const BODIES: [&str; 4] = [PREFACE, B_FIRST, B_NESTED, B_SECOND];

    /// How a paragraph's `<w:pStyle>` is written.
    #[derive(Clone, Copy)]
    pub(crate) enum PStyle {
        /// `<w:pStyle w:val="1"/>`, the form Word writes.
        Empty,
        /// `<w:pStyle w:val="1"></w:pStyle>`.
        Start,
    }

    /// A zip of `parts`, in the order given, each deflated.
    pub(crate) fn docx_with_parts(parts: &[(&str, &[u8])]) -> Vec<u8> {
        let mut buf = Vec::new();
        {
            let mut zip = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
            let opt = SimpleFileOptions::default();
            for (name, bytes) in parts {
                zip.start_file(*name, opt).unwrap();
                zip.write_all(bytes).unwrap();
            }
            zip.finish().unwrap();
        }
        buf
    }

    /// A `word/document.xml` with one `<w:p>` per entry; `Some(id)` styles the paragraph `id`,
    /// written in `form`.
    pub(crate) fn document_xml(paragraphs: &[(Option<&str>, &str)], form: PStyle) -> String {
        let mut body = String::new();
        for (style, text) in paragraphs {
            let ppr = match (style, form) {
                (None, _) => String::new(),
                (Some(id), PStyle::Empty) => format!(r#"<w:pPr><w:pStyle w:val="{id}"/></w:pPr>"#),
                (Some(id), PStyle::Start) => {
                    format!(r#"<w:pPr><w:pStyle w:val="{id}"></w:pStyle></w:pPr>"#)
                }
            };
            body.push_str(&format!("<w:p>{ppr}<w:r><w:t>{text}</w:t></w:r></w:p>"));
        }
        document_xml_from_body(&body)
    }

    /// A `word/document.xml` around `body`, the inside of `<w:body>`.
    pub(crate) fn document_xml_from_body(body: &str) -> String {
        format!(
            r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><w:document xmlns:w="{W_NS}"><w:body>{body}</w:body></w:document>"#
        )
    }

    /// A `word/styles.xml` holding `styles`, after a `w:docDefaults` and a `w:latentStyles`
    /// whose `w:lsdException` entries name `heading 1` .. `heading 3` the way Word writes
    /// them, so a heading name always appears somewhere that is not a style definition.
    pub(crate) fn styles_xml(styles: &str) -> String {
        format!(
            concat!(
                r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>"#,
                r#"<w:styles xmlns:w="{ns}">"#,
                r#"<w:docDefaults><w:rPrDefault><w:rPr><w:sz w:val="21"/></w:rPr></w:rPrDefault><w:pPrDefault/></w:docDefaults>"#,
                r#"<w:latentStyles w:defLockedState="0" w:defUIPriority="99" w:count="267">"#,
                r#"<w:lsdException w:name="Normal" w:uiPriority="0" w:qFormat="1"/>"#,
                r#"<w:lsdException w:name="heading 1" w:uiPriority="9" w:qFormat="1"/>"#,
                r#"<w:lsdException w:name="heading 2" w:semiHidden="1" w:uiPriority="9" w:unhideWhenUsed="1" w:qFormat="1"/>"#,
                r#"<w:lsdException w:name="heading 3" w:semiHidden="1" w:uiPriority="9" w:unhideWhenUsed="1" w:qFormat="1"/>"#,
                r#"</w:latentStyles>"#,
                "{styles}",
                r#"</w:styles>"#,
            ),
            ns = W_NS,
            styles = styles,
        )
    }

    /// One paragraph style; `None` leaves that element out.
    pub(crate) fn paragraph_style(
        id: &str,
        name: Option<&str>,
        based_on: Option<&str>,
        outline_lvl: Option<&str>,
    ) -> String {
        let name = name
            .map(|n| format!(r#"<w:name w:val="{n}"/>"#))
            .unwrap_or_default();
        let based_on = based_on
            .map(|b| format!(r#"<w:basedOn w:val="{b}"/>"#))
            .unwrap_or_default();
        let ppr = outline_lvl
            .map(|o| format!(r#"<w:pPr><w:keepNext/><w:outlineLvl w:val="{o}"/></w:pPr>"#))
            .unwrap_or_default();
        format!(r#"<w:style w:type="paragraph" w:styleId="{id}">{name}{based_on}{ppr}</w:style>"#)
    }

    /// The styles of a document Word 2010 with a Japanese UI writes: the default paragraph
    /// style `a` (`Normal`), headings with the style IDs `1` .. `3` named `heading 1` ..
    /// `heading 3`, based on `a`, with outline levels 0 .. 2, a `Title` (`af`) based on `a`,
    /// and a character and a table style. A paragraph refers to them with an empty
    /// `<w:pStyle/>`.
    pub(crate) fn word2010_ja_styles() -> String {
        let mut styles = String::from(
            r#"<w:style w:type="paragraph" w:default="1" w:styleId="a"><w:name w:val="Normal"/><w:qFormat/><w:pPr><w:widowControl w:val="0"/><w:jc w:val="both"/></w:pPr></w:style>"#,
        );
        for (id, outline) in [("1", "0"), ("2", "1"), ("3", "2")] {
            styles.push_str(&format!(
                r#"<w:style w:type="paragraph" w:styleId="{id}"><w:name w:val="heading {id}"/><w:basedOn w:val="a"/><w:next w:val="a"/><w:uiPriority w:val="9"/><w:qFormat/><w:pPr><w:keepNext/><w:outlineLvl w:val="{outline}"/></w:pPr><w:rPr><w:rFonts w:asciiTheme="majorHAnsi"/><w:sz w:val="24"/></w:rPr></w:style>"#
            ));
        }
        styles.push_str(concat!(
            r#"<w:style w:type="paragraph" w:styleId="af"><w:name w:val="Title"/><w:basedOn w:val="a"/><w:next w:val="a"/><w:qFormat/><w:pPr><w:spacing w:before="240" w:after="120"/><w:jc w:val="center"/></w:pPr></w:style>"#,
            r#"<w:style w:type="character" w:default="1" w:styleId="a0"><w:name w:val="Default Paragraph Font"/><w:uiPriority w:val="1"/><w:semiHidden/></w:style>"#,
            r#"<w:style w:type="table" w:default="1" w:styleId="a1"><w:name w:val="Normal Table"/><w:semiHidden/><w:tblPr><w:tblInd w:w="0" w:type="dxa"/></w:tblPr></w:style>"#,
        ));
        styles
    }

    /// A `docProps/core.xml` titled `title`.
    pub(crate) fn core_xml(title: &str) -> String {
        format!(
            r#"<?xml version="1.0"?><cp:coreProperties xmlns:cp="x" xmlns:dc="y"><dc:title>{title}</dc:title></cp:coreProperties>"#
        )
    }

    /// The AC1 document: [`PREFACE`], then [`H_FIRST`] (style `1`) over [`B_FIRST`],
    /// [`H_NESTED`] (style `2`) over [`B_NESTED`], and [`H_SECOND`] (style `1`) over
    /// [`B_SECOND`], styled by [`word2010_ja_styles`] and titled [`TITLE`].
    pub(crate) fn numeric_heading_docx(form: PStyle) -> Vec<u8> {
        let doc = document_xml(
            &[
                (None, PREFACE),
                (Some("1"), H_FIRST),
                (None, B_FIRST),
                (Some("2"), H_NESTED),
                (None, B_NESTED),
                (Some("1"), H_SECOND),
                (None, B_SECOND),
            ],
            form,
        );
        let styles = styles_xml(&word2010_ja_styles());
        let core = core_xml(TITLE);
        docx_with_parts(&[
            ("word/document.xml", doc.as_bytes()),
            ("word/styles.xml", styles.as_bytes()),
            ("docProps/core.xml", core.as_bytes()),
        ])
    }

    /// How the heading paragraphs of [`folded_chapter_docx_as`] and [`rules_docx_as`] name
    /// their style.
    #[derive(Clone, Copy, Debug)]
    pub(crate) enum HeadingIds {
        /// `1` .. `3`, under [`word2010_ja_styles`], the form Word 2010 with a Japanese UI
        /// writes.
        Numbered,
        /// `Heading1` .. `Heading3`, with no styles part, which the spelling rule reads.
        Spelled,
    }

    impl HeadingIds {
        /// The styleId this form gives a heading of `level`: `1` when numbered, `Heading1`
        /// when spelled.
        pub(crate) fn style_id(self, level: impl std::fmt::Display) -> String {
            match self {
                HeadingIds::Numbered => level.to_string(),
                HeadingIds::Spelled => format!("Heading{level}"),
            }
        }
    }

    /// A document of `paragraphs`, whose heading styles are numbered `1` .. `3`, written with
    /// its headings named as `ids` says and titled `title`.
    fn headed_docx(paragraphs: &[(Option<&str>, &str)], ids: HeadingIds, title: &str) -> Vec<u8> {
        let renamed: Vec<(Option<String>, &str)> = paragraphs
            .iter()
            .map(|(id, text)| {
                let id = id.map(|n| ids.style_id(n));
                (id, *text)
            })
            .collect();
        let refs: Vec<(Option<&str>, &str)> = renamed
            .iter()
            .map(|(id, text)| (id.as_deref(), *text))
            .collect();
        let doc = document_xml(&refs, PStyle::Empty);
        let styles = styles_xml(&word2010_ja_styles());
        let core = core_xml(title);
        let mut parts: Vec<(&str, &[u8])> = vec![("word/document.xml", doc.as_bytes())];
        if let HeadingIds::Numbered = ids {
            parts.push(("word/styles.xml", styles.as_bytes()));
        }
        parts.push(("docProps/core.xml", core.as_bytes()));
        docx_with_parts(&parts)
    }

    /// The words of [`folded_chapter_docx`] (feature-63, AC1 / AC10), sharing none with each
    /// other or with the file names the tests give it.
    pub(crate) const FOLD_TITLE: &str = "Gazette";
    pub(crate) const FOLD_PREFACE: &str = "opening remarks about the harbour district";
    pub(crate) const FOLD_CHAPTER: &str = "Granite";
    pub(crate) const FOLD_FIRST: &str = "Plover";
    pub(crate) const FOLD_FIRST_BODY: &str = "kettle passage under the first section";
    pub(crate) const FOLD_SECOND: &str = "Heron";
    pub(crate) const FOLD_SECOND_BODY: &str = "violin passage under the second section";

    /// A preface, then the chapter [`FOLD_CHAPTER`] (style `1`) with no body of its own,
    /// followed by the sections [`FOLD_FIRST`] and [`FOLD_SECOND`] (style `2`), each over its
    /// body, styled by [`word2010_ja_styles`] and titled [`FOLD_TITLE`].
    pub(crate) fn folded_chapter_docx() -> Vec<u8> {
        folded_chapter_docx_as(HeadingIds::Numbered)
    }

    /// [`folded_chapter_docx`] with its headings named as `ids` says.
    pub(crate) fn folded_chapter_docx_as(ids: HeadingIds) -> Vec<u8> {
        headed_docx(
            &[
                (None, FOLD_PREFACE),
                (Some("1"), FOLD_CHAPTER),
                (Some("2"), FOLD_FIRST),
                (None, FOLD_FIRST_BODY),
                (Some("2"), FOLD_SECOND),
                (None, FOLD_SECOND_BODY),
            ],
            ids,
            FOLD_TITLE,
        )
    }

    /// The words of [`rules_docx`] (feature-63, AC12): a set of work rules, chapter then
    /// article, the shape kuriya #323 reported.
    pub(crate) const RULES_TITLE: &str = "Statute";
    pub(crate) const RULES_PREFACE: &str = "lantana preamble stating the purpose of these rules";
    pub(crate) const RULES_CHAPTERS: [&str; 3] =
        ["Chapter Granite", "Chapter Basalt", "Chapter Marble"];
    pub(crate) const RULES_ARTICLES: [[&str; 2]; 3] = [
        ["Article Plover", "Article Heron"],
        ["Article Egret", "Article Crane"],
        ["Article Stork", "Article Ibis"],
    ];
    pub(crate) const RULES_BODIES: [[&str; 2]; 3] = [
        [
            "first duty about punctual arrival each morning",
            "second duty about tidy desks at closing",
        ],
        [
            "third duty about leave requests in writing",
            "fourth duty about overtime approval beforehand",
        ],
        [
            "fifth duty about returning borrowed laptops",
            "sixth duty about reporting lost badges promptly",
        ],
    ];

    /// [`RULES_PREFACE`], then each of [`RULES_CHAPTERS`] (style `1`) with no body of its own,
    /// followed directly by its two [`RULES_ARTICLES`] (style `2`), each over its
    /// [`RULES_BODIES`] entry; styled by [`word2010_ja_styles`] and titled [`RULES_TITLE`].
    pub(crate) fn rules_docx() -> Vec<u8> {
        rules_docx_as(HeadingIds::Numbered)
    }

    /// [`rules_docx`] with its headings named as `ids` says.
    pub(crate) fn rules_docx_as(ids: HeadingIds) -> Vec<u8> {
        let mut paragraphs: Vec<(Option<&str>, &str)> = vec![(None, RULES_PREFACE)];
        for ((chapter, articles), bodies) in RULES_CHAPTERS
            .iter()
            .zip(&RULES_ARTICLES)
            .zip(&RULES_BODIES)
        {
            paragraphs.push((Some("1"), *chapter));
            for (article, body) in articles.iter().zip(bodies) {
                paragraphs.push((Some("2"), *article));
                paragraphs.push((None, *body));
            }
        }
        headed_docx(&paragraphs, ids, RULES_TITLE)
    }

    /// The heading of [`table_docx`] (feature-64, AC13).
    pub(crate) const TABLE_HEADING: &str = "Tariff";
    /// The cells of [`table_docx`], row by row, sharing no word with [`TABLE_HEADING`].
    pub(crate) const TABLE_ROWS: [[&str; 2]; 2] = [["alder", "birch"], ["cedar", "damson"]];

    /// [`TABLE_HEADING`] styled `Heading1`, with no styles part, over a table of
    /// [`TABLE_ROWS`], each cell one paragraph.
    pub(crate) fn table_docx() -> Vec<u8> {
        let mut body = format!(
            r#"<w:p><w:pPr><w:pStyle w:val="Heading1"/></w:pPr><w:r><w:t>{TABLE_HEADING}</w:t></w:r></w:p><w:tbl>"#
        );
        for row in TABLE_ROWS {
            body.push_str("<w:tr>");
            for cell in row {
                body.push_str(&format!(
                    "<w:tc><w:p><w:r><w:t>{cell}</w:t></w:r></w:p></w:tc>"
                ));
            }
            body.push_str("</w:tr>");
        }
        body.push_str("</w:tbl>");
        let doc = document_xml_from_body(&body);
        docx_with_parts(&[("word/document.xml", doc.as_bytes())])
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    // AU-21: `parse_bytes` は `Parser` ではなく blanket impl の
    // `ParserExt` 側にある (実装から override させないため)。テスト本体は
    // 従来どおり `parse_bytes` を呼ぶので、trait を scope に入れるだけ。
    use crate::parser::ParserExt;
    use std::io::Write;
    use zip::write::SimpleFileOptions;

    /// paragraphs = [(style_opt, text)]。style_opt=Some("Heading1") で見出し段落。
    fn make_minimal_docx(paragraphs: &[(Option<&str>, &str)]) -> Vec<u8> {
        let mut body = String::new();
        for (style, text) in paragraphs {
            let pstyle = match style {
                Some(s) => format!(r#"<w:pPr><w:pStyle w:val="{s}"/></w:pPr>"#),
                None => String::new(),
            };
            body.push_str(&format!(
                r#"<w:p>{pstyle}<w:r><w:t>{text}</w:t></w:r></w:p>"#
            ));
        }
        wrap_document_xml(&body)
    }

    /// `<w:body>` 中身を直接渡す版 (表など `<w:p>` 以外の要素を挟みたいテスト用)。
    fn wrap_document_xml(body: &str) -> Vec<u8> {
        let doc_xml = format!(
            r#"<?xml version="1.0"?><w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body>{body}</w:body></w:document>"#
        );
        let mut buf = Vec::new();
        {
            let mut zip = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
            let opt = SimpleFileOptions::default();
            zip.start_file("[Content_Types].xml", opt).unwrap();
            zip.write_all(br#"<?xml version="1.0"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/></Types>"#).unwrap();
            zip.start_file("word/document.xml", opt).unwrap();
            zip.write_all(doc_xml.as_bytes()).unwrap();
            zip.finish().unwrap();
        }
        buf
    }

    /// `word/document.xml` の中身をそのまま指定して zip を組む
    /// (壊れた XML を入れるテスト用。`wrap_document_xml` は必ず整形式に
    /// なるので、パースエラー経路を踏ませられない)。
    fn docx_with_raw_document_xml(doc_xml: &str) -> Vec<u8> {
        let mut buf = Vec::new();
        {
            let mut zip = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
            let opt = SimpleFileOptions::default();
            zip.start_file("word/document.xml", opt).unwrap();
            zip.write_all(doc_xml.as_bytes()).unwrap();
            zip.finish().unwrap();
        }
        buf
    }

    /// AU-13: XML が途中で壊れていても、そこまでに読めた本文は返す。
    /// 打ち切り自体は `warn_truncated_xml` が stderr に出す (文言の確認は
    /// `tests/binary_formats_cli.rs` の subprocess テスト側)。
    #[test]
    fn test_docx_truncated_xml_keeps_what_was_read_before_the_error() {
        // タグの途中で切れる形。quick-xml はこれを `Err` にする。
        let bytes = docx_with_raw_document_xml(concat!(
            r#"<?xml version="1.0"?>"#,
            r#"<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">"#,
            r#"<w:body>"#,
            r#"<w:p><w:r><w:t>kept before the break</w:t></w:r></w:p>"#,
            r#"<w:p><w:r><w:t"#,
        ));
        let doc = DocxParser::default()
            .parse_bytes(&bytes, "broken.docx", &[])
            .unwrap();
        assert!(
            doc.raw_content.contains("kept before the break"),
            "text read before the error should survive: {:?}",
            doc.raw_content
        );
    }

    /// AU-13 (codex P1): **閉じタグが無いまま終わる**切れ方は quick-xml が
    /// `Err` ではなく `Event::Eof` を返すため、`Err` だけ見ていると
    /// 取りこぼす。実際にはこちらの方が普通の切れ方 (完全なトークンの直後で
    /// ファイルが途切れる)。`warn_if_truncated` は開いたままの要素を数えて
    /// これを検知する。
    #[test]
    fn test_docx_unclosed_root_at_eof_is_detected_as_truncation() {
        let xml = concat!(
            r#"<?xml version="1.0"?>"#,
            r#"<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">"#,
            r#"<w:body>"#,
            r#"<w:p><w:r><w:t>kept</w:t></w:r></w:p>"#,
            // `</w:body></w:document>` が無いまま終わる = quick-xml は Eof を返す
        );
        // まず quick-xml がこれをエラーにしないことを確かめる (前提の固定)。
        let mut reader = quick_xml::reader::Reader::from_reader(xml.as_bytes());
        let mut buf = Vec::new();
        let mut errored = false;
        loop {
            match reader.read_event_into(&mut buf) {
                Ok(Event::Eof) => break,
                Err(_) => {
                    errored = true;
                    break;
                }
                _ => {}
            }
            buf.clear();
        }
        assert!(
            !errored,
            "quick-xml treats an unclosed root as Eof, not Err — that is why the \
             Err arm alone is not enough"
        );

        // 本文は読めているので、返り値としては成功のまま。
        let bytes = docx_with_raw_document_xml(xml);
        let doc = DocxParser::default()
            .parse_bytes(&bytes, "unclosed.docx", &[])
            .unwrap();
        assert!(doc.raw_content.contains("kept"));
    }

    /// AU-13: `<w:br/>` / `<w:tab/>` は `<w:t>` の外側に兄弟として現れる。
    /// 無視すると前後の `<w:t>` が直結し、どちらの語でも引っかからない
    /// 造語ができる。
    #[test]
    fn test_docx_line_break_and_tab_do_not_glue_words_together() {
        let bytes = wrap_document_xml(concat!(
            "<w:p><w:r>",
            "<w:t>alpha</w:t><w:br/><w:t>beta</w:t><w:tab/><w:t>gamma</w:t>",
            "</w:r></w:p>",
        ));
        let doc = DocxParser::default()
            .parse_bytes(&bytes, "breaks.docx", &[])
            .unwrap();
        assert!(
            !doc.raw_content.contains("alphabeta"),
            "a line break must not glue the words: {:?}",
            doc.raw_content
        );
        assert!(
            !doc.raw_content.contains("betagamma"),
            "a tab must not glue the words: {:?}",
            doc.raw_content
        );
        assert!(doc.raw_content.contains("alpha\nbeta"));
        assert!(doc.raw_content.contains("beta\tgamma"));
    }

    #[test]
    fn test_docx_parser_is_binary() {
        assert!(DocxParser::default().is_binary());
        assert_eq!(DocxParser::default().extension(), "docx");
    }

    // NOTE: skeleton 時点の `not_yet_implemented` 固定文言 assert は、本 task で
    // parse_bytes を実本実装したため意味が失われた。xlsx (Task 3.3) の前例に
    // 倣い、garbage 入力が real error path (zip open 失敗) で panic せず Err に
    // なることを検証するテストに更新する (controller 事前承認済み)。
    #[test]
    fn test_docx_parse_bytes_garbage_is_err() {
        let err = DocxParser::default()
            .parse_bytes(b"not a real docx", "x.docx", &[])
            .expect_err("garbage bytes must be Err");
        assert!(err.to_string().contains("cannot open docx zip"));
    }

    #[test]
    fn test_docx_parse_fallback_wraps_raw_text() {
        let doc = DocxParser::default().parse("hello world content here", "x.docx", &[]);
        assert_eq!(doc.chunks.len(), 1);
        assert!(doc.chunks[0].content.contains("hello world"));
    }

    #[test]
    fn test_docx_heading_hierarchy_chunks() {
        let bytes = make_minimal_docx(&[
            (Some("Heading1"), "章1"),
            (None, "本文A これは十分な長さの本文です十分な長さの本文です"),
            (Some("Heading2"), "節1.1"),
            (None, "本文B これは十分な長さの本文です十分な長さの本文です"),
        ]);
        let doc = DocxParser::default()
            .parse_bytes(&bytes, "docs/a.docx", &[])
            .unwrap();
        assert_eq!(doc.chunks.len(), 2);
        assert_eq!(doc.chunks[0].heading.as_deref(), Some("章1"));
        assert_eq!(doc.chunks[0].level, Some(2));
        assert!(doc.chunks[0].content.contains("本文A"));
        assert_eq!(doc.chunks[1].heading.as_deref(), Some("節1.1"));
        assert_eq!(doc.chunks[1].level, Some(3));
    }

    #[test]
    fn test_docx_leading_body_before_heading_is_none() {
        let bytes = make_minimal_docx(&[
            (
                None,
                "前書き これは十分な長さの前書きですよ十分な長さの前書き",
            ),
            (Some("Heading1"), "章1"),
            (
                None,
                "本文 これは十分な長さの本文ですよ十分な長さの本文ですよ",
            ),
        ]);
        let doc = DocxParser::default()
            .parse_bytes(&bytes, "a.docx", &[])
            .unwrap();
        assert_eq!(doc.chunks[0].heading, None);
        assert!(doc.chunks[0].level.is_none());
    }

    #[test]
    fn test_docx_missing_document_xml_is_err() {
        let mut buf = Vec::new();
        {
            let mut zip = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
            let opt = SimpleFileOptions::default();
            zip.start_file("[Content_Types].xml", opt).unwrap();
            zip.write_all(b"<Types/>").unwrap();
            zip.finish().unwrap();
        }
        let err = DocxParser::default()
            .parse_bytes(&buf, "empty.docx", &[])
            .expect_err("zip without word/document.xml must be Err");
        assert!(err.to_string().contains("word/document.xml missing"));
    }

    #[test]
    fn test_docx_entity_reference_in_text_is_preserved() {
        // quick-xml 0.38+ は entity 参照を Text event に含めず `Event::GeneralRef`
        // として別 event で届ける (Text("A") → GeneralRef("amp") → Text("B")
        // の 3 event に分割される)。ここでの本文欠落を防ぐ回帰テスト。
        let bytes = make_minimal_docx(&[(
            None,
            "A&amp;B これは十分な長さの本文ですこれは十分な長さの本文です",
        )]);
        let doc = DocxParser::default()
            .parse_bytes(&bytes, "e.docx", &[])
            .unwrap();
        assert!(
            doc.chunks[0].content.contains("A&B"),
            "entity reference must resolve, got: {:?}",
            doc.chunks[0].content
        );
    }

    #[test]
    fn test_docx_exclude_headings_discards_body() {
        // 除外対象見出し配下の本文は、無題 chunk としても含め一切 index に
        // 残ってはいけない (leak すると exclude_headings が機密除外用途で
        // 使い物にならない)。次の非除外見出し以降は通常通り拾われる。
        let bytes = make_minimal_docx(&[
            (Some("Heading1"), "Secret"),
            (
                None,
                "confidential body enough length enough length enough length",
            ),
            (Some("Heading1"), "Public"),
            (
                None,
                "public body enough length enough length enough length",
            ),
        ]);
        let doc = DocxParser::default()
            .parse_bytes(&bytes, "docs/s.docx", &["Secret"])
            .unwrap();
        let joined: String = doc
            .chunks
            .iter()
            .map(|c| format!("{:?} {}", c.heading, c.content))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            !joined.contains("confidential"),
            "excluded heading body must not leak into any chunk (incl. headless): {joined}"
        );
        assert_eq!(doc.chunks.len(), 1);
        assert_eq!(doc.chunks[0].heading.as_deref(), Some("Public"));
        assert!(doc.chunks[0].content.contains("public body"));
    }

    #[test]
    fn test_docx_table_text_included_in_body() {
        // 表 (`w:tbl`) 内テキストも専用ハンドリングなしで段落として自然に本文化
        // される (`w:tbl > w:tr > w:tc > w:p > w:r > w:t` の入れ子でも `<w:p>`
        // 境界処理だけで済む)。
        let body = concat!(
            r#"<w:p><w:pPr><w:pStyle w:val="Heading1"/></w:pPr><w:r><w:t>章1</w:t></w:r></w:p>"#,
            r#"<w:tbl><w:tr><w:tc><w:p><w:r><w:t>表内セル十分な長さのセル内容です十分な長さです</w:t></w:r></w:p></w:tc></w:tr></w:tbl>"#,
        );
        let bytes = wrap_document_xml(body);
        let doc = DocxParser::default()
            .parse_bytes(&bytes, "t.docx", &[])
            .unwrap();
        assert_eq!(doc.chunks.len(), 1);
        assert_eq!(doc.chunks[0].heading.as_deref(), Some("章1"));
        assert!(doc.chunks[0].content.contains("表内セル"));
    }

    #[test]
    fn test_docx_context_heading_hierarchy() {
        // 章1 (Heading1→level2) > 節1.1 (Heading2→level3)。title は core.xml 無しなので
        // filename fallback ("a")。
        let bytes = make_minimal_docx(&[
            (Some("Heading1"), "章1"),
            (None, "本文A これは十分な長さの本文です十分な長さの本文です"),
            (Some("Heading2"), "節1.1"),
            (None, "本文B これは十分な長さの本文です十分な長さの本文です"),
        ]);
        let doc = DocxParser::default()
            .parse_bytes(&bytes, "docs/a.docx", &[])
            .unwrap();
        assert_eq!(doc.chunks[0].context.as_deref(), Some("a > 章1"));
        assert_eq!(doc.chunks[1].context.as_deref(), Some("a > 章1 > 節1.1"));
    }

    #[test]
    fn test_docx_context_leading_body_is_title_only() {
        // E-3 相当: 見出し前本文 (heading None) は title のみ
        let bytes = make_minimal_docx(&[
            (
                None,
                "前書き これは十分な長さの前書きですよ十分な長さの前書き",
            ),
            (Some("Heading1"), "章1"),
            (
                None,
                "本文 これは十分な長さの本文ですよ十分な長さの本文ですよ",
            ),
        ]);
        let doc = DocxParser::default()
            .parse_bytes(&bytes, "a.docx", &[])
            .unwrap();
        assert_eq!(doc.chunks[0].heading, None);
        assert_eq!(doc.chunks[0].context.as_deref(), Some("a"));
    }

    #[test]
    fn test_docx_context_true_level_skip_heading1_to_heading3() {
        // E-7: docx は Heading1-6 → level 2-6 の全段階を持つため、markdown
        // (h2/h3 のみ) では起こらない「真の level 飛び」(Heading1 直後に
        // Heading2 を挟まず Heading3 が来る = level 2 → level 4) が実際に発生
        // する。この場合も Heading3 の祖先は直近の浅い見出し (章1) のみになる
        // ことを検証する。
        let bytes =
            make_minimal_docx(&[(Some("Heading1"), "章1"), (Some("Heading3"), "小節1.1.1")]);
        let doc = DocxParser::default()
            .parse_bytes(&bytes, "docs/a.docx", &[])
            .unwrap();
        assert_eq!(doc.chunks[0].heading.as_deref(), Some("章1"));
        assert_eq!(doc.chunks[0].level, Some(2));
        assert_eq!(doc.chunks[1].heading.as_deref(), Some("小節1.1.1"));
        assert_eq!(doc.chunks[1].level, Some(4));
        assert_eq!(
            doc.chunks[1].context.as_deref(),
            Some("a > 章1 > 小節1.1.1")
        );
    }

    /// feature-61 (AC5): the budget the parser was built with reaches the
    /// `docProps/core.xml` read as well. document.xml fits; document.xml plus
    /// core.xml is one byte over, so the parse fails at core.xml, which it
    /// would not if that read still used the 50 MiB constant.
    #[test]
    fn ooxml_budget_follows_the_docx_parser_it_was_built_with() {
        let doc_xml: &[u8] = br#"<?xml version="1.0"?><w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:p><w:r><w:t>body</w:t></w:r></w:p></w:body></w:document>"#;
        let core_xml: &[u8] = br#"<?xml version="1.0"?><cp:coreProperties xmlns:cp="x" xmlns:dc="y"><dc:title>Core</dc:title></cp:coreProperties>"#;
        let mut bytes = Vec::new();
        {
            let mut zip = zip::ZipWriter::new(std::io::Cursor::new(&mut bytes));
            let opt = SimpleFileOptions::default();
            zip.start_file("word/document.xml", opt).unwrap();
            zip.write_all(doc_xml).unwrap();
            zip.start_file("docProps/core.xml", opt).unwrap();
            zip.write_all(core_xml).unwrap();
            zip.finish().unwrap();
        }
        let both = (doc_xml.len() + core_xml.len()) as u64;

        let err = DocxParser::with_budget(both - 1)
            .parse_bytes(&bytes, "budget.docx", &[])
            .expect_err("document.xml plus core.xml is one byte over the budget");
        let msg = err.to_string();
        assert!(msg.contains("max_decompressed_size"), "{msg}");
        assert!(!msg.contains("file too large"), "{msg}");

        let doc = DocxParser::with_budget(both)
            .parse_bytes(&bytes, "budget.docx", &[])
            .expect("exactly the budget is allowed");
        assert_eq!(doc.frontmatter.title.as_deref(), Some("Core"));

        assert!(
            DocxParser::default()
                .parse_bytes(&bytes, "budget.docx", &[])
                .is_ok(),
            "the default is the previous 50 MiB"
        );
    }

    // -----------------------------------------------------------------------
    // feature-62: headings from word/styles.xml
    // -----------------------------------------------------------------------

    /// feature-62 T19 (AC18): one reading of the heading digits, shared by style
    /// names and by the style-ID spelling the fallback reads, and a chunk level
    /// that cannot overflow however large the number.
    #[test]
    fn heading_digits_reads_names_the_way_the_style_id_rule_did() {
        let headings: &[(&str, u8)] = &[
            ("heading 1", 1),
            ("Heading1", 1),
            ("HEADING 3", 3),
            ("heading 1 ", 1),
            ("heading\u{3000}1", 1),
            ("heading 9", 9),
            ("heading 10", 10),
            ("heading 255", 255),
        ];
        for (s, n) in headings {
            assert_eq!(heading_digits(s), Some(*n), "{s:?}");
        }
        for s in [
            "heading",
            "heading \u{FF11}",
            "heading 256",
            "Heading 1 Char",
            " heading 1",
            "Title",
            "1",
        ] {
            assert_eq!(heading_digits(s), None, "{s:?}");
        }
        for (n, level) in [(1, 2), (5, 6), (6, 6), (10, 6), (255, 6)] {
            assert_eq!(chunk_level(n), level, "heading {n}");
        }
    }

    /// feature-62 (r1 M1): the spelling fallback stops at a `heading 0` value as it always
    /// did, rather than reading on to another `val` attribute of the same `<w:pStyle>`. Two
    /// `val` attributes with different prefixes (`w:val`, `w14:val`) are well-formed; the
    /// same qualified name twice is not, and quick-xml drops the second.
    #[test]
    fn a_heading_zero_style_id_stops_the_fallback_at_that_attribute() {
        assert_eq!(heading_digits("heading 0"), Some(0));
        let bytes = wrap_document_xml(concat!(
            r#"<w:p><w:pPr><w:pStyle w:val="Heading0" w14:val="Heading2"/></w:pPr>"#,
            r#"<w:r><w:t>Zero styled line</w:t></w:r></w:p>"#,
            r#"<w:p><w:r><w:t>plain body text below it</w:t></w:r></w:p>"#,
        ));
        let doc = DocxParser::default()
            .parse_bytes(&bytes, "zero.docx", &[])
            .unwrap();
        assert_eq!(doc.chunks.len(), 1, "{:?}", doc.chunks);
        assert_eq!(doc.chunks[0].heading, None);
        assert!(doc.chunks[0].content.contains("Zero styled line"));
    }

    /// feature-62 (local review r2): a `<w:pStyle>` carrying the same qualified `w:val`
    /// twice, in a document without a styles part, keeps main's spelling fallback, which
    /// reads the attributes through `.flatten()` (plan D8). quick-xml yields the first
    /// `w:val` and reports the second as a duplicate error that `.flatten()` drops, so the
    /// first value alone decides: `Heading1` then `Heading2` is a level-2 `Heading1`, and
    /// `Normal` then `Heading1` is body text.
    #[test]
    fn a_duplicated_pstyle_val_keeps_the_spelling_fallback_of_main() {
        let bytes = wrap_document_xml(concat!(
            r#"<w:p><w:pPr><w:pStyle w:val="Heading1" w:val="Heading2"/></w:pPr>"#,
            r#"<w:r><w:t>Twice valued title</w:t></w:r></w:p>"#,
            r#"<w:p><w:r><w:t>prose under the doubled heading</w:t></w:r></w:p>"#,
        ));
        let doc = DocxParser::default()
            .parse_bytes(&bytes, "doubled.docx", &[])
            .unwrap();
        assert_eq!(doc.chunks.len(), 1, "{:?}", doc.chunks);
        assert_eq!(doc.chunks[0].heading.as_deref(), Some("Twice valued title"));
        assert_eq!(doc.chunks[0].level, Some(2));

        let bytes = wrap_document_xml(concat!(
            r#"<w:p><w:pPr><w:pStyle w:val="Normal" w:val="Heading1"/></w:pPr>"#,
            r#"<w:r><w:t>Plainly styled sentence</w:t></w:r></w:p>"#,
            r#"<w:p><w:r><w:t>more ordinary text after it</w:t></w:r></w:p>"#,
        ));
        let doc = DocxParser::default()
            .parse_bytes(&bytes, "normalfirst.docx", &[])
            .unwrap();
        assert_eq!(doc.chunks.len(), 1, "{:?}", doc.chunks);
        assert_eq!(doc.chunks[0].heading, None);
        assert!(doc.chunks[0].content.contains("Plainly styled sentence"));
    }

    use super::fixture::*;

    /// The (heading, level) of each chunk.
    fn outline(doc: &ParsedDocument) -> Vec<(Option<&str>, Option<u8>)> {
        doc.chunks
            .iter()
            .map(|c| (c.heading.as_deref(), c.level))
            .collect()
    }

    /// AC1: the sections of [`numeric_heading_docx`] -- a headless preface, then the three
    /// headings at their levels, with the heading words out of every body.
    fn assert_numeric_heading_sections(doc: &ParsedDocument) {
        assert_eq!(
            outline(doc),
            vec![
                (None, None),
                (Some(H_FIRST), Some(2)),
                (Some(H_NESTED), Some(3)),
                (Some(H_SECOND), Some(2)),
            ]
        );
        assert_eq!(
            doc.chunks[2].context.as_deref(),
            Some(format!("{TITLE} > {H_FIRST} > {H_NESTED}").as_str())
        );
        for heading in HEADINGS {
            assert!(
                doc.chunks.iter().all(|c| !c.content.contains(heading)),
                "{heading} must leave the body: {:?}",
                doc.chunks
            );
            assert!(
                !doc.raw_content.contains(heading),
                "{heading}: {:?}",
                doc.raw_content
            );
        }
        for body in BODIES {
            assert!(
                doc.raw_content.contains(body),
                "{body}: {:?}",
                doc.raw_content
            );
        }
    }

    /// feature-62 T1 (AC1): Word 2010 in Japanese names its headings `heading N` under the
    /// style IDs `1` .. `6`, and the style table reads them as headings.
    #[test]
    fn a_numeric_style_id_named_heading_is_a_heading() {
        let doc = DocxParser::default()
            .parse_bytes(&numeric_heading_docx(PStyle::Empty), "quarry.docx", &[])
            .unwrap();
        assert_numeric_heading_sections(&doc);
    }

    /// `styles` (the inside of a `word/styles.xml`, wrapped by [`styles_xml`]) as a table.
    fn table_of(styles: &str) -> StyleTable {
        StyleTable::parse(styles_xml(styles).as_bytes()).expect("a usable styles part")
    }

    /// What `table` says about the paragraphs whose `<w:pStyle w:val>` is `spelled`.
    fn verdict_for(table: &StyleTable, spelled: &str) -> StyleVerdict {
        table.verdict(&style_id_key(spelled.as_bytes()))
    }

    /// Whether quick-xml itself reports an error anywhere in `xml`: the premise the
    /// broken-part tests fix before they rely on it, the way
    /// [`test_docx_unclosed_root_at_eof_is_detected_as_truncation`] does.
    fn quick_xml_errs(xml: &[u8]) -> bool {
        let mut reader = Reader::from_reader(xml);
        let mut buf = Vec::new();
        loop {
            match reader.read_event_into(&mut buf) {
                Ok(Event::Eof) => return false,
                Err(_) => return true,
                _ => {}
            }
            buf.clear();
        }
    }

    /// feature-62 (R1.2): a style is a direct child of the root `<w:styles>`. One nested in
    /// another element or written after the root closed is not read, and a
    /// `w:lsdException` naming a heading is not a style.
    #[test]
    fn style_table_reads_only_the_styles_directly_under_the_root() {
        let nested = format!(
            "<w:extra>{}</w:extra>",
            paragraph_style("Nested", Some("heading 1"), None, None)
        );
        let direct = paragraph_style("Direct", Some("heading 2"), None, None);
        let late = paragraph_style("Late", Some("heading 1"), None, None);
        let xml = format!("{}{late}", styles_xml(&format!("{nested}{direct}")));
        assert!(
            !quick_xml_errs(xml.as_bytes()),
            "premise: quick-xml reads an element after the root without an error"
        );
        let table = StyleTable::parse(xml.as_bytes()).expect("the root closed, so it is usable");
        assert_eq!(verdict_for(&table, "Direct"), StyleVerdict::Heading(3));
        assert_eq!(verdict_for(&table, "Nested"), StyleVerdict::NotDefined);
        assert_eq!(verdict_for(&table, "Late"), StyleVerdict::NotDefined);
        assert_eq!(
            verdict_for(&table, "heading 1"),
            StyleVerdict::NotDefined,
            "a latent style is not a definition"
        );
    }

    /// feature-62 (R1.2, Review Focus 1): of a `<w:style>`, only its own attributes, its
    /// direct `w:name` / `w:basedOn` and the `w:outlineLvl` directly under its own `w:pPr`
    /// are read. An outline level inside `w:rPr` or `w:tblStylePr`, and the `w:type` of a
    /// `w:tblStylePr`, belong to something else.
    #[test]
    fn a_styles_own_properties_are_read_and_nested_ones_are_not() {
        let table = table_of(&format!(
            "{}{}{}",
            paragraph_style("H1", Some("heading 1"), None, None),
            concat!(
                r#"<w:style w:styleId="Nested"><w:name w:val="Quote Block"/><w:basedOn w:val="H1"/>"#,
                r#"<w:rPr><w:outlineLvl w:val="9"/></w:rPr>"#,
                r#"<w:tblStylePr w:type="firstRow"><w:pPr><w:outlineLvl w:val="9"/></w:pPr></w:tblStylePr>"#,
                r#"</w:style>"#,
            ),
            paragraph_style("Own", Some("Quiet Copy"), Some("H1"), Some("9")),
        ));
        assert_eq!(
            verdict_for(&table, "Nested"),
            StyleVerdict::Heading(2),
            "outline levels nested deeper are not the style's, and a w:tblStylePr's w:type is not its type"
        );
        assert_eq!(
            verdict_for(&table, "Own"),
            StyleVerdict::Body,
            "the style's own outline level 9 turns it back into body text"
        );
    }

    /// feature-62 (R2.3, Review Focus 4): a byte-order mark, the XML declaration and a
    /// comment before the root are not the root.
    #[test]
    fn a_styles_part_with_a_prologue_is_read() {
        let xml = format!(
            "\u{FEFF}<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\r\n<!-- written by hand -->\r\n<w:styles xmlns:w=\"{W_NS}\">{}</w:styles>",
            paragraph_style("1", Some("heading 1"), Some("a"), Some("0")),
        );
        let table = StyleTable::parse(xml.as_bytes()).expect("the prologue is not the root");
        assert_eq!(verdict_for(&table, "1"), StyleVerdict::Heading(2));
    }

    /// feature-62 (J22, Review Focus 5): a style ID is compared with its entities resolved,
    /// byte for byte and case by case, on both sides; an entity that does not resolve is
    /// compared as written.
    #[test]
    fn style_ids_match_after_unescaping_and_by_case() {
        let table = table_of(&format!(
            "{}{}{}{}",
            paragraph_style("A&amp;B", Some("heading 2"), None, None),
            paragraph_style("Heading1", Some("Body Copy"), None, None),
            paragraph_style("heading1", Some("heading 1"), None, None),
            paragraph_style("X&bogus;", Some("heading 3"), None, None),
        ));
        assert_eq!(verdict_for(&table, "A&amp;B"), StyleVerdict::Heading(3));
        assert_eq!(
            verdict_for(&table, "A&#38;B"),
            StyleVerdict::Heading(3),
            "the same ID, escaped another way"
        );
        assert_eq!(verdict_for(&table, "Heading1"), StyleVerdict::Body);
        assert_eq!(verdict_for(&table, "heading1"), StyleVerdict::Heading(2));
        assert_eq!(
            verdict_for(&table, "HEADING1"),
            StyleVerdict::NotDefined,
            "IDs are compared case by case"
        );
        assert_eq!(
            verdict_for(&table, "X&bogus;"),
            StyleVerdict::Heading(4),
            "an entity that does not resolve is compared as written"
        );
    }

    /// feature-62 (R1.2): the table holds paragraph styles -- `w:type="paragraph"`, or no
    /// `w:type` -- that have a style ID. Character, table and numbering styles are left out.
    #[test]
    fn only_paragraph_styles_with_an_id_are_in_the_table() {
        let table = table_of(concat!(
            r#"<w:style w:styleId="Untyped"><w:name w:val="heading 2"/></w:style>"#,
            r#"<w:style w:type="character" w:styleId="Chr"><w:name w:val="heading 1"/></w:style>"#,
            r#"<w:style w:type="table" w:styleId="Tbl"><w:name w:val="heading 1"/></w:style>"#,
            r#"<w:style w:type="numbering" w:styleId="Num"><w:name w:val="heading 1"/></w:style>"#,
            r#"<w:style w:type="paragraph"><w:name w:val="heading 1"/></w:style>"#,
        ));
        assert_eq!(verdict_for(&table, "Untyped"), StyleVerdict::Heading(3));
        for id in ["Chr", "Tbl", "Num"] {
            assert_eq!(verdict_for(&table, id), StyleVerdict::NotDefined, "{id}");
        }
    }

    /// feature-62 (R2.3): the four ways a styles part is not used, each with its reason, and
    /// the one way it looks empty but is usable (`<w:styles/>`).
    #[test]
    fn a_styles_part_that_is_not_wholly_readable_says_why() {
        let heading_1 = paragraph_style("1", Some("heading 1"), None, None);

        let unclosed = format!(
            r#"<w:styles xmlns:w="{W_NS}">{heading_1}<w:style w:type="paragraph" w:styleId="2"><w:name w:val="heading 2"/>"#
        );
        assert!(
            !quick_xml_errs(unclosed.as_bytes()),
            "premise: quick-xml ends this at Eof"
        );
        assert_eq!(
            StyleTable::parse(unclosed.as_bytes()).unwrap_err(),
            StylesUnusable::Unclosed(2)
        );

        let cut = format!(r#"<w:styles xmlns:w="{W_NS}">{heading_1}<w:sty"#);
        assert!(
            quick_xml_errs(cut.as_bytes()),
            "premise: quick-xml errs on a tag cut short"
        );
        assert!(matches!(
            StyleTable::parse(cut.as_bytes()),
            Err(StylesUnusable::Xml(_))
        ));

        for no_root in ["", "not xml at all"] {
            assert_eq!(
                StyleTable::parse(no_root.as_bytes()).unwrap_err(),
                StylesUnusable::NoRoot,
                "{no_root:?}"
            );
        }

        let elsewhere = format!(r#"<w:document xmlns:w="{W_NS}">{heading_1}</w:document>"#);
        assert_eq!(
            StyleTable::parse(elsewhere.as_bytes()).unwrap_err(),
            StylesUnusable::RootNotStyles
        );

        let empty = format!(r#"<w:styles xmlns:w="{W_NS}"/>"#);
        let table = StyleTable::parse(empty.as_bytes()).expect("no styles is not broken");
        assert_eq!(verdict_for(&table, "1"), StyleVerdict::NotDefined);
    }

    /// feature-62 T2 (AC2): a `<w:pStyle>` written as a start tag resolves the same way.
    #[test]
    fn a_start_element_pstyle_resolves_through_the_style_table() {
        let doc = DocxParser::default()
            .parse_bytes(&numeric_heading_docx(PStyle::Start), "quarry.docx", &[])
            .unwrap();
        assert_numeric_heading_sections(&doc);
    }

    /// feature-62 T19 (AC18): one reading of a heading number, shared by style names and by
    /// the style-ID spelling the fallback reads. The chunk level that cannot overflow is pinned
    /// next to [`heading_digits`].
    #[test]
    fn heading_number_reads_names_the_way_the_style_id_rule_did() {
        let headings: &[(&str, u8)] = &[
            ("heading 1", 1),
            ("Heading1", 1),
            ("HEADING 3", 3),
            ("heading 1 ", 1),
            ("heading\u{3000}1", 1),
            ("heading 9", 9),
            ("heading 10", 10),
            ("heading 255", 255),
        ];
        for (s, n) in headings {
            assert_eq!(heading_number(s), Some(*n), "{s:?}");
        }
        for s in [
            "heading 0",
            "heading",
            "heading \u{FF11}",
            "heading 256",
            "Heading 1 Char",
            " heading 1",
            "Title",
            "1",
        ] {
            assert_eq!(heading_number(s), None, "{s:?}");
        }
    }

    /// feature-62 (R2.3): the stderr line for a styles part that is not used names the part,
    /// the reason and the fallback, ASCII only.
    #[test]
    fn unreadable_styles_message_names_the_part_the_reason_and_the_fallback() {
        assert_eq!(
            unreadable_styles_message("docs/q.docx", &StylesUnusable::Unclosed(2)),
            "warning: docs/q.docx: word/styles.xml is not a readable styles part (ended with 2 element(s) still open); headings fall back to style IDs"
        );
        let reasons = [
            (StylesUnusable::Xml("bad".to_string()), "(XML error: bad)"),
            (StylesUnusable::NoRoot, "(no root element)"),
            (
                StylesUnusable::RootNotStyles,
                "(root element is not w:styles)",
            ),
        ];
        for (reason, said) in reasons {
            let msg = unreadable_styles_message("q.docx", &reason);
            assert!(msg.contains(said), "{msg}");
            assert!(msg.is_ascii(), "{msg}");
        }
    }

    /// The paragraph text whose level the probe documents report.
    const PROBE: &str = "Quince marker";
    const OPENING: &str = "opening paragraph of plain prose";
    const CLOSING: &str = "closing paragraph of plain prose";

    /// A document whose paragraph styled `pstyle` ([`PROBE`]) sits between two body
    /// paragraphs, with `styles` as its `word/styles.xml` when given and no core.xml. Returns
    /// the bytes and the length of its `word/document.xml`, for the budget tests.
    fn probe_docx(styles: Option<&[u8]>, pstyle: &str) -> (Vec<u8>, usize) {
        let doc = document_xml(
            &[(None, OPENING), (Some(pstyle), PROBE), (None, CLOSING)],
            PStyle::Empty,
        );
        let mut parts: Vec<(&str, &[u8])> = vec![("word/document.xml", doc.as_bytes())];
        if let Some(styles) = styles {
            parts.push(("word/styles.xml", styles));
        }
        (docx_with_parts(&parts), doc.len())
    }

    /// The level [`PROBE`] was given, or `None` when it stayed body text -- checking that a
    /// heading's text left the body and body text stayed in it.
    fn level_of(doc: &ParsedDocument) -> Option<u8> {
        match doc
            .chunks
            .iter()
            .find(|c| c.heading.as_deref() == Some(PROBE))
        {
            Some(chunk) => {
                assert!(!doc.raw_content.contains(PROBE), "{:?}", doc.raw_content);
                chunk.level
            }
            None => {
                assert!(doc.raw_content.contains(PROBE), "{:?}", doc.raw_content);
                None
            }
        }
    }

    /// feature-62 T14 (AC14): a styles part larger than the decompression budget on its own is
    /// left out like any part over it, and the document is read with the spelling rule.
    #[test]
    fn a_styles_part_over_the_per_entry_cap_falls_back() {
        let styles = styles_xml(&word2010_ja_styles());
        for (pstyle, want) in [("1", None), ("Heading1", Some(2))] {
            let (bytes, doc_len) = probe_docx(Some(styles.as_bytes()), pstyle);
            let cap = (doc_len + 64) as u64;
            assert!(
                styles.len() as u64 > cap,
                "fixture: styles.xml alone is over the cap"
            );
            let doc = DocxParser::with_budget(cap)
                .parse_bytes(&bytes, "probe.docx", &[])
                .expect("a part over the cap is skipped, not fatal");
            assert_eq!(level_of(&doc), want, "{pstyle}");
        }
    }

    /// feature-62 T15 (AC15 (a)(b)): styles.xml counts toward the document's budget after
    /// document.xml; exactly the budget is read, one byte short is skipped -- the document is
    /// still read, by the spelling rule -- rather than failing it.
    #[test]
    fn a_styles_part_that_would_exceed_the_document_budget_is_skipped() {
        let styles = styles_xml(&word2010_ja_styles());

        let (bytes, doc_len) = probe_docx(Some(styles.as_bytes()), "1");
        let both = (doc_len + styles.len()) as u64;
        let doc = DocxParser::with_budget(both)
            .parse_bytes(&bytes, "probe.docx", &[])
            .expect("exactly the budget is allowed");
        assert_eq!(level_of(&doc), Some(2), "(a) styles.xml read");

        assert!(
            (styles.len() as u64) < both - 1,
            "fixture: styles.xml alone fits the cap, so only the total can refuse it"
        );
        let doc = DocxParser::with_budget(both - 1)
            .parse_bytes(&bytes, "probe.docx", &[])
            .expect("styles.xml is skipped, the document is not");
        assert_eq!(
            level_of(&doc),
            None,
            "(b) the spelling `1` is not a heading"
        );

        let (bytes, doc_len) = probe_docx(Some(styles.as_bytes()), "Heading1");
        let doc = DocxParser::with_budget((doc_len + styles.len() - 1) as u64)
            .parse_bytes(&bytes, "probe.docx", &[])
            .expect("styles.xml is skipped, the document is not");
        assert_eq!(
            level_of(&doc),
            Some(2),
            "(b) the spelling `Heading1` still is"
        );
    }

    /// [`level_of`] the probe document parsed under the default budget, with `styles` as its
    /// `word/styles.xml` when given.
    fn level_with(styles: Option<&[u8]>, pstyle: &str) -> Option<u8> {
        let (bytes, _) = probe_docx(styles, pstyle);
        let doc = DocxParser::default()
            .parse_bytes(&bytes, "probe.docx", &[])
            .expect("a probe document parses");
        level_of(&doc)
    }

    /// [`level_with`] a styles part holding `styles`, wrapped by [`styles_xml`].
    fn level_under(styles: &str, pstyle: &str) -> Option<u8> {
        level_with(Some(styles_xml(styles).as_bytes()), pstyle)
    }

    /// feature-62 T3 (AC3): a style that is not named a heading takes the level of the nearest
    /// style it is based on that is.
    #[test]
    fn a_style_based_on_a_heading_takes_the_nearest_heading_level() {
        let styles = format!(
            "{}{}{}",
            paragraph_style("H1", Some("heading 1"), None, None),
            paragraph_style("H2", Some("heading 2"), Some("H1"), None),
            paragraph_style("X", Some("Callout"), Some("H2"), None),
        );
        assert_eq!(
            level_under(&styles, "X"),
            Some(3),
            "the nearest is heading 2"
        );
        assert_eq!(level_under(&styles, "H2"), Some(3));
        assert_eq!(level_under(&styles, "H1"), Some(2));
    }

    /// feature-62 T4 (AC4): of the styles between the paragraph's own and its heading
    /// ancestor, the first that sets an outline level decides -- 9 makes body text, 0 to 8
    /// leaves the heading at the level its name gives.
    #[test]
    fn a_derived_style_that_resets_the_outline_level_to_body_is_not_a_heading() {
        let heading_1 = paragraph_style("Heading1", Some("heading 1"), None, None);

        let toc = format!(
            "{heading_1}{}",
            paragraph_style(
                "TOCHeading",
                Some("TOC Heading"),
                Some("Heading1"),
                Some("9")
            )
        );
        assert_eq!(
            level_under(&toc, "TOCHeading"),
            None,
            "(a) Word's TOC Heading"
        );

        let derived = format!(
            "{heading_1}{}",
            paragraph_style("Derived", Some("Pull Quote"), Some("Heading1"), Some("2"))
        );
        assert_eq!(
            level_under(&derived, "Derived"),
            Some(2),
            "(b) outline level 2 neither cancels nor sets the level; heading 1 does"
        );

        let middle = paragraph_style("M", Some("Muted"), Some("Heading1"), Some("9"));
        let c = format!(
            "{heading_1}{middle}{}",
            paragraph_style("X", Some("Aside"), Some("M"), None)
        );
        assert_eq!(level_under(&c, "X"), None, "(c) X sets none, M sets 9");

        let d = format!(
            "{heading_1}{middle}{}",
            paragraph_style("X", Some("Aside"), Some("M"), Some("3"))
        );
        assert_eq!(
            level_under(&d, "X"),
            Some(2),
            "(d) X's 3 comes before M's 9"
        );
    }

    /// feature-62 T5 (AC5): a style named `heading 2` is a heading whatever outline level it
    /// sets itself; its own level is not in the range that can cancel it.
    #[test]
    fn a_heading_named_style_keeps_its_level_whatever_its_own_outline_level() {
        let styles = paragraph_style("S2", Some("heading 2"), None, Some("9"));
        assert_eq!(level_under(&styles, "S2"), Some(3));
    }

    /// feature-62 T6 (AC6, R1.6): an outline level never makes a heading -- not a style's when
    /// its name is not a heading's, and not one written on the paragraph itself.
    #[test]
    fn outline_level_alone_does_not_make_a_heading() {
        let styles = format!(
            "{}{}",
            paragraph_style("Plain", Some("Body Text"), None, Some("0")),
            word2010_ja_styles()
        );
        assert_eq!(
            level_under(&styles, "Plain"),
            None,
            "a style with outline level 0"
        );

        let doc = document_xml_from_body(&format!(
            concat!(
                r#"<w:p><w:pPr><w:outlineLvl w:val="0"/></w:pPr><w:r><w:t>{probe}</w:t></w:r></w:p>"#,
                r#"<w:p><w:r><w:t>{closing}</w:t></w:r></w:p>"#,
            ),
            probe = PROBE,
            closing = CLOSING,
        ));
        let styles_part = styles_xml(&word2010_ja_styles());
        let bytes = docx_with_parts(&[
            ("word/document.xml", doc.as_bytes()),
            ("word/styles.xml", styles_part.as_bytes()),
        ]);
        let parsed = DocxParser::default()
            .parse_bytes(&bytes, "probe.docx", &[])
            .unwrap();
        assert_eq!(
            level_of(&parsed),
            None,
            "a paragraph's own outline level is not read"
        );
    }

    /// feature-62 T7 (AC7, R2.1 f): a character style is not in the table, so it is neither a
    /// heading through its name nor a parent; a `w:pStyle` that names only a character style
    /// falls back to its spelling.
    #[test]
    fn a_character_style_is_never_a_paragraph_heading() {
        let styles = concat!(
            r#"<w:style w:type="character" w:styleId="C1"><w:name w:val="heading 1"/></w:style>"#,
            r#"<w:style w:type="paragraph" w:styleId="P"><w:name w:val="Lead"/><w:basedOn w:val="C1"/></w:style>"#,
            r#"<w:style w:type="character" w:styleId="Heading2"><w:name w:val="Body Char"/></w:style>"#,
        );
        assert_eq!(
            level_under(styles, "C1"),
            None,
            "`c1` is no heading by its spelling"
        );
        assert_eq!(
            level_under(styles, "P"),
            None,
            "a character style is not a parent"
        );
        assert_eq!(
            level_under(styles, "Heading2"),
            Some(3),
            "an ID only a character style has is not in the table, so its spelling decides"
        );
    }

    /// feature-62 T8 (AC8): the first definition of a style ID wins, in both orders, and
    /// whatever the type of the first.
    #[test]
    fn the_first_definition_of_a_duplicated_style_id_wins() {
        let heading_first = format!(
            "{}{}",
            paragraph_style("D", Some("heading 2"), None, None),
            paragraph_style("D", Some("Normal"), None, None)
        );
        assert_eq!(level_under(&heading_first, "D"), Some(3));

        let body_first = format!(
            "{}{}",
            paragraph_style("D", Some("Normal"), None, None),
            paragraph_style("D", Some("heading 2"), None, None)
        );
        assert_eq!(level_under(&body_first, "D"), None);

        let character_first = format!(
            "{}{}",
            r#"<w:style w:type="character" w:styleId="S"><w:name w:val="heading 1"/></w:style>"#,
            paragraph_style("S", Some("heading 2"), None, None)
        );
        assert_eq!(
            level_under(&character_first, "S"),
            None,
            "the character style came first, so `S` is not in the table and `s` is no heading"
        );
    }

    /// A `w:basedOn` chain of `len` paragraph styles, `c1` based on `c2` and so on, whose last
    /// is named `heading 1` and the rest are not.
    fn chain_of(len: usize) -> String {
        (1..=len)
            .map(|i| {
                let id = format!("c{i}");
                if i == len {
                    paragraph_style(&id, Some("heading 1"), None, None)
                } else {
                    let name = format!("Link {i}");
                    let parent = format!("c{}", i + 1);
                    paragraph_style(&id, Some(name.as_str()), Some(parent.as_str()), None)
                }
            })
            .collect()
    }

    /// feature-62 T9 (AC9, J5): a `w:basedOn` cycle ends, as body text, and so does a chain
    /// whose heading is past the bound of 16 styles, the paragraph's own counted.
    #[test]
    fn a_based_on_cycle_or_an_overlong_chain_ends_the_walk() {
        let cycle = format!(
            "{}{}",
            paragraph_style("A", Some("Alpha"), Some("B"), None),
            paragraph_style("B", Some("Beta"), Some("A"), None)
        );
        assert_eq!(level_under(&cycle, "A"), None, "a cycle ends at the bound");
        assert_eq!(
            level_under(&chain_of(16), "c1"),
            Some(2),
            "the 16th style is looked at"
        );
        assert_eq!(level_under(&chain_of(17), "c1"), None, "the 17th is not");
    }

    /// feature-62 T10 (AC10, J1, J4): an ID the table holds is decided by the table. A style
    /// without a usable name lets its ID stand in for the name; a parent the table does not
    /// hold ends the walk as body text, not as a fallback.
    #[test]
    fn a_style_id_the_table_resolves_is_decided_by_the_table() {
        assert_eq!(
            level_under(
                &paragraph_style("Heading1", Some("Body Copy"), None, None),
                "Heading1"
            ),
            None,
            "(a) the table says body, whatever the ID spells"
        );
        let nameless = [
            ("b", paragraph_style("Heading1", None, None, None)),
            (
                "c",
                r#"<w:style w:type="paragraph" w:styleId="Heading1"><w:name/></w:style>"#.to_string(),
            ),
            (
                "d",
                r#"<w:style w:type="paragraph" w:styleId="Heading1"><w:name w:val=""/></w:style>"#
                    .to_string(),
            ),
            (
                "e",
                r#"<w:style w:type="paragraph" w:styleId="Heading1"><w:name w:val="  "/></w:style>"#
                    .to_string(),
            ),
        ];
        for (case, styles) in &nameless {
            assert_eq!(level_under(styles, "Heading1"), Some(2), "({case})");
        }
        assert_eq!(
            level_under(&paragraph_style("7", None, None, None), "7"),
            None,
            "(f) the ID stands in, and `7` is no heading"
        );
        assert_eq!(
            level_under(
                &paragraph_style("Heading3", Some("Plain"), Some("Missing"), None),
                "Heading3"
            ),
            None,
            "(g) no fallback past a missing parent"
        );
    }

    /// feature-62 T11 (AC11): with a usable styles part, an ID it does not define falls back to
    /// its spelling.
    #[test]
    fn an_unresolved_style_id_falls_back_to_the_style_id_spelling() {
        let styles = word2010_ja_styles();
        assert_eq!(level_under(&styles, "Heading2"), Some(3));
        assert_eq!(level_under(&styles, "7"), None);
    }

    /// feature-62 T12 (AC12, J6): a styles part that ends with elements still open is not used
    /// at all -- not even the styles that closed before the end.
    #[test]
    fn a_styles_part_ending_with_open_elements_is_not_used() {
        let part = format!(
            r#"<?xml version="1.0"?><w:styles xmlns:w="{W_NS}">{}<w:style w:type="paragraph" w:styleId="2"><w:name w:val="heading 2"/>"#,
            paragraph_style("1", Some("heading 1"), None, None)
        );
        assert!(
            !quick_xml_errs(part.as_bytes()),
            "premise: quick-xml ends this at Eof, not with an error"
        );
        assert_eq!(
            level_with(Some(part.as_bytes()), "1"),
            None,
            "style 1 closed but is not used"
        );
        assert_eq!(level_with(Some(part.as_bytes()), "2"), None);
        assert_eq!(level_with(Some(part.as_bytes()), "Heading1"), Some(2));
    }

    /// feature-62 T13 (AC13): the other ways a styles part is not used -- (a) a tag cut short,
    /// (b) no element, (c) a root other than `w:styles` -- fall back for every paragraph, and
    /// the parse still succeeds; (d) `<w:styles/>` is usable and holds nothing, which reads the
    /// same (the stderr line tells them apart, which the CLI integration test C2 in the
    /// `index_docx_heading_policy` test crate pins).
    #[test]
    fn a_broken_styles_part_falls_back_for_every_paragraph() {
        let heading_1 = paragraph_style("1", Some("heading 1"), None, None);
        let cut = format!(r#"<w:styles xmlns:w="{W_NS}">{heading_1}<w:sty"#);
        assert!(
            quick_xml_errs(cut.as_bytes()),
            "premise: (a) quick-xml errs"
        );
        let no_root = "not xml at all".to_string();
        assert!(
            !quick_xml_errs(no_root.as_bytes()),
            "premise: (b) text, then Eof"
        );
        let elsewhere = format!(r#"<w:document xmlns:w="{W_NS}">{heading_1}</w:document>"#);
        let empty = format!(r#"<w:styles xmlns:w="{W_NS}"/>"#);
        for (case, part) in [
            ("a", &cut),
            ("b", &no_root),
            ("c", &elsewhere),
            ("d", &empty),
        ] {
            assert_eq!(level_with(Some(part.as_bytes()), "1"), None, "({case})");
            assert_eq!(
                level_with(Some(part.as_bytes()), "Heading1"),
                Some(2),
                "({case})"
            );
        }
    }

    /// feature-62 (R2.3, codex review): a `<w:style>` whose attribute quick-xml rejects -- here
    /// `w:type` given twice -- makes the whole part unusable, so the well-formed style `1`
    /// before it is not used and every paragraph falls back to the spelling rule. The reader
    /// itself does not err on the tag; only the attribute iterator does. The one stderr line
    /// for an unusable part is the CLI integration test C2's to observe (in the
    /// `index_docx_heading_policy` test crate), as for T13.
    #[test]
    fn a_styles_part_with_a_malformed_attribute_is_not_used() {
        let heading_1 = paragraph_style("1", Some("heading 1"), None, None);
        let part = |second_type: &str| {
            format!(
                r#"<w:styles xmlns:w="{W_NS}">{heading_1}<w:style w:type="paragraph"{second_type} w:styleId="2"><w:name w:val="heading 2"/></w:style></w:styles>"#
            )
        };
        let duplicated = part(r#" w:type="paragraph""#);
        let clean = part("");
        assert!(
            !quick_xml_errs(duplicated.as_bytes()),
            "premise: the reader reaches Eof; only the attributes err"
        );

        assert!(matches!(
            StyleTable::parse(duplicated.as_bytes()),
            Err(StylesUnusable::Xml(_))
        ));
        assert_eq!(level_with(Some(duplicated.as_bytes()), "1"), None);
        assert_eq!(level_with(Some(duplicated.as_bytes()), "Heading1"), Some(2));

        let table =
            StyleTable::parse(clean.as_bytes()).expect("without the duplicate it is usable");
        assert_eq!(verdict_for(&table, "1"), StyleVerdict::Heading(2));
        assert_eq!(level_with(Some(clean.as_bytes()), "1"), Some(2));
    }

    /// feature-62 (R2.3, codex review): the same holds for an element the walk never reads --
    /// here a `w:lsdException` under `w:latentStyles` with `w:name` given twice. The heading
    /// style itself is well-formed, and still the part is not used.
    #[test]
    fn a_malformed_attribute_anywhere_in_the_styles_part_is_not_used() {
        let heading_1 = paragraph_style("1", Some("heading 1"), None, None);
        let part = |second_name: &str| {
            format!(
                r#"<w:styles xmlns:w="{W_NS}"><w:latentStyles w:defQFormat="0"><w:lsdException w:name="Normal"{second_name} w:qFormat="1"/></w:latentStyles>{heading_1}</w:styles>"#
            )
        };
        let duplicated = part(r#" w:name="Normal""#);
        let clean = part("");
        assert!(
            !quick_xml_errs(duplicated.as_bytes()),
            "premise: the reader reaches Eof; only the attributes err"
        );

        assert!(matches!(
            StyleTable::parse(duplicated.as_bytes()),
            Err(StylesUnusable::Xml(_))
        ));
        assert_eq!(level_with(Some(duplicated.as_bytes()), "1"), None);
        assert_eq!(level_with(Some(duplicated.as_bytes()), "Heading1"), Some(2));

        let table =
            StyleTable::parse(clean.as_bytes()).expect("without the duplicate it is usable");
        assert_eq!(verdict_for(&table, "1"), StyleVerdict::Heading(2));
        assert_eq!(level_with(Some(clean.as_bytes()), "1"), Some(2));
    }

    /// feature-62 (R2.3, codex review round 3): the attribute check in [`StylesWalk`] runs
    /// before the root guard and before the guard for elements after the root, so a malformed
    /// attribute on the root itself, or on an element written after the root closed, also
    /// makes [`StyleTable::parse`] refuse the part. Without the duplicate each part is usable.
    #[test]
    fn a_malformed_attribute_on_the_root_or_after_it_is_not_used() {
        let heading_1 = paragraph_style("1", Some("heading 1"), None, None);
        let on_root = |second_x: &str| {
            format!(r#"<w:styles xmlns:w="{W_NS}" w:x="1"{second_x}>{heading_1}</w:styles>"#)
        };
        let after_root = |second_x: &str| {
            format!(
                r#"<w:styles xmlns:w="{W_NS}">{heading_1}</w:styles><w:late w:x="1"{second_x}/>"#
            )
        };
        let cases = [
            ("root", on_root(r#" w:x="2""#), on_root("")),
            ("after the root", after_root(r#" w:x="2""#), after_root("")),
        ];
        for (case, duplicated, clean) in &cases {
            assert!(
                !quick_xml_errs(duplicated.as_bytes()),
                "premise ({case}): the reader reaches Eof; only the attributes err"
            );
            assert!(
                matches!(
                    StyleTable::parse(duplicated.as_bytes()),
                    Err(StylesUnusable::Xml(_))
                ),
                "({case})"
            );
            assert_eq!(
                level_with(Some(duplicated.as_bytes()), "1"),
                None,
                "({case})"
            );
            assert_eq!(
                level_with(Some(duplicated.as_bytes()), "Heading1"),
                Some(2),
                "({case})"
            );

            let table = StyleTable::parse(clean.as_bytes())
                .unwrap_or_else(|_| panic!("({case}) without the duplicate it is usable"));
            assert_eq!(
                verdict_for(&table, "1"),
                StyleVerdict::Heading(2),
                "({case})"
            );
            assert_eq!(level_with(Some(clean.as_bytes()), "1"), Some(2), "({case})");
        }
    }

    type Section = (Option<String>, Option<u8>, String, Option<String>);

    /// Each chunk as (heading, level, content, context).
    fn sections(doc: &ParsedDocument) -> Vec<Section> {
        doc.chunks
            .iter()
            .map(|c| {
                (
                    c.heading.clone(),
                    c.level,
                    c.content.clone(),
                    c.context.clone(),
                )
            })
            .collect()
    }

    /// feature-62 T16 (AC16, R3): the read entry resolves styles the same way, and under the
    /// default budget whatever budget the parser was built with.
    #[test]
    fn the_read_entry_applies_the_style_table_too() {
        let bytes = numeric_heading_docx(PStyle::Empty);
        let index = DocxParser::default()
            .parse_bytes(&bytes, "quarry.docx", &[])
            .unwrap();
        let read = DocxParser::default()
            .parse_bytes_for_read(&bytes, "quarry.docx", &[])
            .unwrap();
        assert_eq!(sections(&read), sections(&index));
        assert_numeric_heading_sections(&read);

        let styles = styles_xml(&word2010_ja_styles());
        let (probe, doc_len) = probe_docx(Some(styles.as_bytes()), "1");
        let tight = DocxParser::with_budget((doc_len + styles.len() - 1) as u64);
        let indexed = tight.parse_bytes(&probe, "probe.docx", &[]).unwrap();
        assert_eq!(
            level_of(&indexed),
            None,
            "the index side skips styles.xml under its budget"
        );
        let read = tight
            .parse_bytes_for_read(&probe, "probe.docx", &[])
            .unwrap();
        assert_eq!(
            level_of(&read),
            Some(2),
            "the read side keeps the default budget"
        );
    }

    /// feature-62 T18 (AC17): an English-Word document -- style IDs `Heading1` / `Heading2`
    /// named `heading 1` / `heading 2`, based on `Normal` -- splits exactly as the same
    /// paragraphs without a styles part, which the spelling rule has always split.
    #[test]
    fn builtin_english_heading_styles_split_exactly_as_without_styles() {
        let paragraphs: &[(Option<&str>, &str)] = &[
            (Some("Heading1"), "章1"),
            (None, "本文A これは十分な長さの本文です十分な長さの本文です"),
            (Some("Heading2"), "節1.1"),
            (None, "本文B これは十分な長さの本文です十分な長さの本文です"),
        ];
        let doc = document_xml(paragraphs, PStyle::Empty);
        let styles = styles_xml(&format!(
            "{}{}{}",
            r#"<w:style w:type="paragraph" w:default="1" w:styleId="Normal"><w:name w:val="Normal"/><w:qFormat/></w:style>"#,
            paragraph_style("Heading1", Some("heading 1"), Some("Normal"), Some("0")),
            paragraph_style("Heading2", Some("heading 2"), Some("Normal"), Some("1")),
        ));
        let parse = |bytes: &[u8]| {
            DocxParser::default()
                .parse_bytes(bytes, "docs/a.docx", &[])
                .unwrap()
        };
        let with = parse(&docx_with_parts(&[
            ("word/document.xml", doc.as_bytes()),
            ("word/styles.xml", styles.as_bytes()),
        ]));
        let without = parse(&docx_with_parts(&[("word/document.xml", doc.as_bytes())]));
        assert_eq!(sections(&with), sections(&without));
        assert_eq!(
            outline(&with),
            vec![(Some("章1"), Some(2)), (Some("節1.1"), Some(3))]
        );
    }

    // -----------------------------------------------------------------------
    // feature-63: a heading with no body text folds into the next section
    // -----------------------------------------------------------------------

    /// `paragraphs` as their `w:pStyle` IDs: a heading level `n` (1 to 3) spelled `HeadingN`,
    /// or numbered `N` for [`word2010_ja_styles`]; `None` is a body paragraph.
    fn styled(
        paragraphs: &[(Option<u8>, &'static str)],
        numbered: bool,
    ) -> Vec<(Option<String>, &'static str)> {
        paragraphs
            .iter()
            .map(|(level, text)| {
                let ids = if numbered {
                    HeadingIds::Numbered
                } else {
                    HeadingIds::Spelled
                };
                let id = level.map(|n| ids.style_id(n));
                (id, *text)
            })
            .collect()
    }

    /// Parse `paragraphs` twice -- spelled `HeadingN` without a styles part, and numbered `N`
    /// under [`word2010_ja_styles`] -- under `excludes`, as `docs/ledger.docx` (title `ledger`,
    /// no core.xml). Both must split identically, whichever way the headings were found; the
    /// first is returned.
    fn parse_both(paragraphs: &[(Option<u8>, &'static str)], excludes: &[&str]) -> ParsedDocument {
        let styles = styles_xml(&word2010_ja_styles());
        let build = |numbered: bool| {
            let owned = styled(paragraphs, numbered);
            let refs: Vec<(Option<&str>, &str)> = owned
                .iter()
                .map(|(id, text)| (id.as_deref(), *text))
                .collect();
            let doc = document_xml(&refs, PStyle::Empty);
            let mut parts: Vec<(&str, &[u8])> = vec![("word/document.xml", doc.as_bytes())];
            if numbered {
                parts.push(("word/styles.xml", styles.as_bytes()));
            }
            let bytes = docx_with_parts(&parts);
            DocxParser::default()
                .parse_bytes(&bytes, "docs/ledger.docx", excludes)
                .expect("a generated document parses")
        };
        let spelled = build(false);
        let numbered = build(true);
        assert_eq!(
            sections(&spelled),
            sections(&numbered),
            "both heading paths split alike"
        );
        assert_eq!(spelled.raw_content, numbered.raw_content);
        spelled
    }

    /// [`parse_both`] for a `<w:body>` written out by hand: `body` names its heading styles
    /// `Heading1` .. `Heading3` and is parsed as written, without a styles part, and again with
    /// them numbered `1` .. `3` under [`word2010_ja_styles`], which defines no deeper level.
    /// Both must split identically; the first is returned.
    fn parse_raw_both(body: &str) -> ParsedDocument {
        let spelled = DocxParser::default()
            .parse_bytes(&wrap_document_xml(body), "docs/ledger.docx", &[])
            .expect("a generated document parses");
        let numbered_body = (1..=3).fold(body.to_string(), |acc, level| {
            acc.replace(
                &format!(r#"w:val="{}""#, HeadingIds::Spelled.style_id(level)),
                &format!(r#"w:val="{}""#, HeadingIds::Numbered.style_id(level)),
            )
        });
        assert_ne!(numbered_body, body, "the body names its heading styles");
        let doc = document_xml_from_body(&numbered_body);
        let styles = styles_xml(&word2010_ja_styles());
        let numbered = DocxParser::default()
            .parse_bytes(
                &docx_with_parts(&[
                    ("word/document.xml", doc.as_bytes()),
                    ("word/styles.xml", styles.as_bytes()),
                ]),
                "docs/ledger.docx",
                &[],
            )
            .expect("a generated document parses");
        assert_eq!(
            sections(&spelled),
            sections(&numbered),
            "both heading paths split alike"
        );
        assert_eq!(spelled.raw_content, numbered.raw_content);
        spelled
    }

    /// The content of each chunk, in order.
    fn contents(doc: &ParsedDocument) -> Vec<&str> {
        doc.chunks.iter().map(|c| c.content.as_str()).collect()
    }

    /// feature-63 T1 (AC1): an empty chapter is no chunk of its own; its title opens the body
    /// of the section after it, and the sections keep their own heading, level and context.
    #[test]
    fn an_empty_chapter_folds_into_the_section_after_it() {
        let doc = parse_both(
            &[
                (None, "opening remarks about the harbour district"),
                (Some(1), "Granite"),
                (Some(2), "Plover"),
                (None, "kettle passage under the first section"),
                (Some(2), "Heron"),
                (None, "violin passage under the second section"),
            ],
            &[],
        );
        assert_eq!(
            outline(&doc),
            vec![
                (None, None),
                (Some("Plover"), Some(3)),
                (Some("Heron"), Some(3))
            ]
        );
        assert_eq!(
            doc.chunks.iter().map(|c| c.index).collect::<Vec<_>>(),
            vec![0, 1, 2]
        );
        assert_eq!(
            contents(&doc),
            vec![
                "opening remarks about the harbour district",
                "Granite\nkettle passage under the first section",
                "violin passage under the second section",
            ]
        );
        assert_eq!(
            doc.chunks[1].context.as_deref(),
            Some("ledger > Granite > Plover")
        );
        assert_eq!(
            doc.chunks[2].context.as_deref(),
            Some("ledger > Granite > Heron")
        );
        assert_eq!(
            doc.raw_content,
            "opening remarks about the harbour district\n\nGranite\nkettle passage under the first section\n\nviolin passage under the second section"
        );
    }

    /// feature-63 T2 (AC2): consecutive empty headings fold in document order, one line each;
    /// a heading paragraph with no text adds no line.
    #[test]
    fn consecutive_empty_headings_fold_one_line_each() {
        let doc = parse_both(
            &[
                (Some(1), "Granite"),
                (Some(2), "Basalt"),
                (Some(3), "Plover"),
                (None, "kettle passage under the deepest section"),
            ],
            &[],
        );
        assert_eq!(outline(&doc), vec![(Some("Plover"), Some(4))]);
        assert_eq!(
            contents(&doc),
            vec!["Granite\nBasalt\nkettle passage under the deepest section"]
        );
        assert_eq!(
            doc.chunks[0].context.as_deref(),
            Some("ledger > Granite > Basalt > Plover")
        );

        let doc = parse_both(
            &[
                (Some(1), "Granite"),
                (Some(2), ""),
                (Some(3), "Plover"),
                (None, "kettle passage under the deepest section"),
            ],
            &[],
        );
        assert_eq!(
            contents(&doc),
            vec!["Granite\nkettle passage under the deepest section"]
        );
    }

    /// feature-63 T3 (AC3, AC4): an empty heading with nothing after it is dropped, and one
    /// followed by a sibling or a shallower heading folds into it all the same.
    #[test]
    fn a_trailing_empty_heading_drops_and_a_sibling_one_folds_forward() {
        let doc = parse_both(
            &[
                (Some(1), "Granite"),
                (None, "kettle passage under the only chapter"),
                (Some(1), "Marble"),
            ],
            &[],
        );
        assert_eq!(outline(&doc), vec![(Some("Granite"), Some(2))]);
        assert_eq!(
            contents(&doc),
            vec!["kettle passage under the only chapter"]
        );
        assert!(!doc.raw_content.contains("Marble"), "{:?}", doc.raw_content);

        let doc = parse_both(
            &[
                (Some(2), "Basalt"),
                (Some(2), "Plover"),
                (None, "violin passage under the sibling"),
            ],
            &[],
        );
        assert_eq!(outline(&doc), vec![(Some("Plover"), Some(3))]);
        assert_eq!(
            contents(&doc),
            vec!["Basalt\nviolin passage under the sibling"]
        );
        assert_eq!(doc.chunks[0].context.as_deref(), Some("ledger > Plover"));

        let doc = parse_both(
            &[
                (Some(3), "Basalt"),
                (Some(1), "Plover"),
                (None, "violin passage under the shallower heading"),
            ],
            &[],
        );
        assert_eq!(outline(&doc), vec![(Some("Plover"), Some(2))]);
        assert_eq!(
            contents(&doc),
            vec!["Basalt\nviolin passage under the shallower heading"]
        );
    }

    /// feature-63 T4 (AC5, AC6, AC7): a document of headings alone keeps a chunk per heading;
    /// one whose only body is the preface keeps the preface and drops the headings; an empty
    /// body has no chunk.
    #[test]
    fn headings_alone_keep_their_chunks_and_a_preface_alone_drops_them() {
        let doc = parse_both(&[(Some(1), "Granite"), (Some(3), "Plover")], &[]);
        assert_eq!(
            outline(&doc),
            vec![(Some("Granite"), Some(2)), (Some("Plover"), Some(4))]
        );
        assert_eq!(contents(&doc), vec!["", ""]);

        let doc = parse_both(
            &[
                (None, "opening remarks about the harbour district"),
                (Some(1), "Granite"),
                (Some(2), "Plover"),
            ],
            &[],
        );
        assert_eq!(outline(&doc), vec![(None, None)]);
        assert_eq!(
            doc.raw_content,
            "opening remarks about the harbour district"
        );

        let doc = parse_both(&[], &[]);
        assert!(doc.chunks.is_empty(), "{:?}", doc.chunks);
    }

    /// feature-63 T5 (AC8): a body of whitespace, ideographic spaces or empty table cells is no
    /// body, so the heading over it folds forward.
    #[test]
    fn whitespace_and_empty_cells_are_no_body() {
        let body = concat!(
            r#"<w:p><w:pPr><w:pStyle w:val="Heading1"/></w:pPr><w:r><w:t>Granite</w:t></w:r></w:p>"#,
            r#"<w:p><w:r><w:t xml:space="preserve">   </w:t></w:r></w:p>"#,
            "<w:p><w:r><w:t>\u{3000}\u{3000}</w:t></w:r></w:p>",
            r#"<w:tbl><w:tr><w:tc><w:p></w:p></w:tc><w:tc><w:p><w:r><w:t> </w:t></w:r></w:p></w:tc></w:tr></w:tbl>"#,
            r#"<w:p><w:pPr><w:pStyle w:val="Heading2"/></w:pPr><w:r><w:t>Plover</w:t></w:r></w:p>"#,
            r#"<w:p><w:r><w:t>kettle passage under the section</w:t></w:r></w:p>"#,
        );
        let doc = parse_raw_both(body);
        assert_eq!(outline(&doc), vec![(Some("Plover"), Some(3))]);
        assert_eq!(
            contents(&doc),
            vec!["Granite\nkettle passage under the section"]
        );
    }

    /// feature-63 T6 (AC9): an excluded heading is never folded nor folded into; an empty
    /// heading before it folds past it into the next section that has a body; with no such
    /// section the document falls back to a chunk per heading, and the excluded body is
    /// nowhere.
    #[test]
    fn excluded_headings_neither_fold_nor_receive() {
        let doc = parse_both(
            &[
                (Some(1), "Secret"),
                (None, "confidential clause kept out of the index"),
                (Some(1), "Granite"),
                (Some(2), "Plover"),
                (None, "kettle passage under the public section"),
            ],
            &["Secret"],
        );
        assert_eq!(outline(&doc), vec![(Some("Plover"), Some(3))]);
        assert_eq!(
            contents(&doc),
            vec!["Granite\nkettle passage under the public section"]
        );

        let doc = parse_both(
            &[
                (Some(1), "Granite"),
                (Some(2), "Secret"),
                (None, "confidential clause kept out of the index"),
                (Some(1), "Plover"),
                (None, "kettle passage under the public chapter"),
            ],
            &["Secret"],
        );
        assert_eq!(outline(&doc), vec![(Some("Plover"), Some(2))]);
        assert_eq!(
            contents(&doc),
            vec!["Granite\nkettle passage under the public chapter"]
        );
        assert!(!doc.raw_content.contains("Secret"), "{:?}", doc.raw_content);
        assert!(
            !doc.raw_content.contains("confidential"),
            "{:?}",
            doc.raw_content
        );

        let doc = parse_both(
            &[
                (Some(1), "Granite"),
                (Some(2), "Secret"),
                (None, "confidential clause kept out of the index"),
            ],
            &["Secret"],
        );
        assert_eq!(outline(&doc), vec![(Some("Granite"), Some(2))]);
        assert_eq!(contents(&doc), vec![""]);
        assert!(
            !doc.raw_content.contains("confidential"),
            "{:?}",
            doc.raw_content
        );
    }

    /// feature-63 T7 (AC10, parser side): the read entry folds the same way the index does,
    /// whether the headings are numbered under a styles part or spelled `HeadingN`.
    #[test]
    fn the_read_entry_folds_empty_headings_the_same_way() {
        let mut split = Vec::new();
        for (ids, bytes) in [
            (HeadingIds::Numbered, folded_chapter_docx()),
            (
                HeadingIds::Spelled,
                folded_chapter_docx_as(HeadingIds::Spelled),
            ),
        ] {
            let index = DocxParser::default()
                .parse_bytes(&bytes, "quarry.docx", &[])
                .unwrap();
            let read = DocxParser::default()
                .parse_bytes_for_read(&bytes, "quarry.docx", &[])
                .unwrap();
            assert_eq!(sections(&read), sections(&index), "{ids:?}");
            assert_eq!(read.raw_content, index.raw_content, "{ids:?}");
            assert_eq!(
                index.chunks[1].content,
                format!("{FOLD_CHAPTER}\n{FOLD_FIRST_BODY}"),
                "{ids:?}"
            );
            split.push(sections(&index));
        }
        assert_eq!(split[0], split[1], "both heading paths split alike");
    }

    /// feature-63 T8 (AC12): the work-rules shape kuriya #323 met has no empty chunk, and each
    /// chapter title is the first line of its first article and of no other chunk -- whether
    /// the headings are numbered under a styles part or spelled `HeadingN`.
    #[test]
    fn a_rules_document_has_no_empty_chunk_and_each_chapter_opens_its_first_article() {
        let mut split = Vec::new();
        for (ids, bytes) in [
            (HeadingIds::Numbered, rules_docx()),
            (HeadingIds::Spelled, rules_docx_as(HeadingIds::Spelled)),
        ] {
            let doc = DocxParser::default()
                .parse_bytes(&bytes, "rulebook.docx", &[])
                .unwrap();
            assert_eq!(doc.chunks.len(), 7, "{ids:?}: {:?}", outline(&doc));
            assert!(
                doc.chunks.iter().all(|c| !c.content.trim().is_empty()),
                "{ids:?}: {:?}",
                contents(&doc)
            );
            for (c, (chapter, articles)) in RULES_CHAPTERS.iter().zip(&RULES_ARTICLES).enumerate() {
                let first = 1 + 2 * c;
                let holders: Vec<usize> = doc
                    .chunks
                    .iter()
                    .enumerate()
                    .filter(|(_, chunk)| chunk.content.contains(chapter))
                    .map(|(i, _)| i)
                    .collect();
                assert_eq!(holders, vec![first], "{ids:?}: {chapter}");
                assert_eq!(
                    doc.chunks[first].content.lines().next(),
                    Some(*chapter),
                    "{ids:?}"
                );
                assert_eq!(
                    doc.chunks[first].heading.as_deref(),
                    Some(articles[0]),
                    "{ids:?}"
                );
            }
            split.push(sections(&doc));
        }
        assert_eq!(split[0], split[1], "both heading paths split alike");
    }

    /// feature-63 Review Focus 2: a document that opens with an empty chapter and no preface
    /// starts its content with the chapter's title, not with a blank line.
    #[test]
    fn a_document_opening_with_an_empty_chapter_starts_with_its_title() {
        let doc = parse_both(
            &[
                (Some(1), "Granite"),
                (Some(2), "Plover"),
                (None, "kettle passage under the first section"),
            ],
            &[],
        );
        assert_eq!(
            doc.raw_content,
            "Granite\nkettle passage under the first section"
        );
    }

    /// feature-63 Review Focus 3: a heading broken over two lines by `<w:br/>` folds with both
    /// lines.
    #[test]
    fn a_folded_heading_with_a_line_break_keeps_both_lines() {
        let body = concat!(
            r#"<w:p><w:pPr><w:pStyle w:val="Heading1"/></w:pPr><w:r><w:t>Granite</w:t><w:br/><w:t>Quarry</w:t></w:r></w:p>"#,
            r#"<w:p><w:pPr><w:pStyle w:val="Heading2"/></w:pPr><w:r><w:t>Plover</w:t></w:r></w:p>"#,
            r#"<w:p><w:r><w:t>kettle passage under the section</w:t></w:r></w:p>"#,
        );
        let doc = parse_raw_both(body);
        assert_eq!(
            contents(&doc),
            vec!["Granite\nQuarry\nkettle passage under the section"]
        );
    }

    /// feature-63 Review Focus 4: a section whose only body is a table is a section with a
    /// body, and an empty chapter before it folds into it.
    #[test]
    fn an_empty_chapter_folds_into_a_table_only_section() {
        let body = concat!(
            r#"<w:p><w:pPr><w:pStyle w:val="Heading1"/></w:pPr><w:r><w:t>Granite</w:t></w:r></w:p>"#,
            r#"<w:p><w:pPr><w:pStyle w:val="Heading2"/></w:pPr><w:r><w:t>Plover</w:t></w:r></w:p>"#,
            r#"<w:tbl><w:tr><w:tc><w:p><w:r><w:t>kettle cell inside the table</w:t></w:r></w:p></w:tc></w:tr></w:tbl>"#,
        );
        let doc = parse_raw_both(body);
        assert_eq!(outline(&doc), vec![(Some("Plover"), Some(3))]);
        assert_eq!(
            contents(&doc),
            vec!["Granite\nkettle cell inside the table"]
        );
    }

    /// feature-63 Review Focus 5: an empty heading whose text is its child's still folds; the
    /// context drops the repeat, the content keeps it.
    #[test]
    fn an_empty_heading_with_the_same_text_as_its_child_still_folds() {
        let doc = parse_both(
            &[
                (Some(1), "Overview"),
                (Some(2), "Overview"),
                (None, "kettle passage under the repeated heading"),
            ],
            &[],
        );
        assert_eq!(outline(&doc), vec![(Some("Overview"), Some(3))]);
        assert_eq!(
            contents(&doc),
            vec!["Overview\nkettle passage under the repeated heading"]
        );
    }

    /// feature-64: a `<w:p>` styled `Heading{level}`, which [`parse_raw_both`] also reads
    /// numbered under [`word2010_ja_styles`].
    fn heading_para(level: u8, text: &str) -> String {
        format!(
            r#"<w:p><w:pPr><w:pStyle w:val="Heading{level}"/></w:pPr><w:r><w:t>{text}</w:t></w:r></w:p>"#
        )
    }

    /// feature-64: an unstyled `<w:p>` holding `text` (`""` is a paragraph with no text).
    fn body_para(text: &str) -> String {
        format!("<w:p><w:r><w:t>{text}</w:t></w:r></w:p>")
    }

    /// feature-64: a `<w:tbl>` of `rows`, each cell one [`body_para`] of its text.
    fn grid_of(rows: &[&[&str]]) -> String {
        let mut xml = String::from("<w:tbl>");
        for row in rows {
            xml.push_str("<w:tr>");
            for cell in *row {
                xml.push_str(&format!("<w:tc>{}</w:tc>", body_para(cell)));
            }
            xml.push_str("</w:tr>");
        }
        xml.push_str("</w:tbl>");
        xml
    }

    /// feature-64 T1 (AC1): each row is one line, its cells separated by a tab.
    #[test]
    fn a_table_row_is_one_line_with_its_cells_tab_separated() {
        let body = format!(
            "{}{}",
            heading_para(1, "Tariff"),
            grid_of(&[&["alder", "birch"], &["cedar", "damson"]])
        );
        let doc = parse_raw_both(&body);
        assert_eq!(outline(&doc), vec![(Some("Tariff"), Some(2))]);
        assert_eq!(contents(&doc), vec!["alder\tbirch\ncedar\tdamson"]);
    }

    /// feature-64 T2 (AC2): an empty cell keeps its column as an empty field, at the start,
    /// in the middle or at the end of a row; a `w:vMerge` continue cell, whose paragraph is
    /// self-closing, is an empty field too.
    #[test]
    fn an_empty_cell_keeps_its_column() {
        let body = format!(
            "{}{}{}",
            heading_para(1, "Tariff"),
            grid_of(&[&["alder", "", "cedar"], &["", "birch"], &["damson", ""]]),
            concat!(
                "<w:tbl>",
                r#"<w:tr><w:tc><w:tcPr><w:vMerge w:val="restart"/></w:tcPr><w:p><w:r><w:t>elm</w:t></w:r></w:p></w:tc><w:tc><w:p><w:r><w:t>fir</w:t></w:r></w:p></w:tc></w:tr>"#,
                "<w:tr><w:tc><w:tcPr><w:vMerge/></w:tcPr><w:p/></w:tc><w:tc><w:p><w:r><w:t>gum</w:t></w:r></w:p></w:tc></w:tr>",
                "</w:tbl>",
            )
        );
        let doc = parse_raw_both(&body);
        assert_eq!(
            contents(&doc),
            vec!["alder\t\tcedar\n\tbirch\ndamson\t\nelm\tfir\n\tgum"]
        );
    }

    /// feature-64 T3 (AC3): a row whose cells are all empty or blank adds no line.
    #[test]
    fn a_row_of_empty_cells_adds_no_line() {
        let body = format!(
            "{}{}",
            heading_para(1, "Tariff"),
            grid_of(&[&["alder", "birch"], &["", " "], &["cedar", "damson"]])
        );
        let doc = parse_raw_both(&body);
        assert_eq!(contents(&doc), vec!["alder\tbirch\ncedar\tdamson"]);
    }

    /// feature-64 Review Focus 1: the property elements Word writes around a table -- table
    /// properties and grid, a header-row mark, a cell width and a horizontal span -- add
    /// neither text nor a field. A spanned cell is written once (R1.4).
    #[test]
    fn table_properties_add_nothing_to_the_row() {
        let body = format!(
            "{}{}",
            heading_para(1, "Tariff"),
            concat!(
                "<w:tbl>",
                r#"<w:tblPr><w:tblStyle w:val="TableGrid"/><w:tblW w:w="0" w:type="auto"/></w:tblPr>"#,
                r#"<w:tblGrid><w:gridCol w:w="4000"/><w:gridCol w:w="4000"/></w:tblGrid>"#,
                r#"<w:tr><w:trPr><w:tblHeader/></w:trPr>"#,
                r#"<w:tc><w:tcPr><w:tcW w:w="4000" w:type="dxa"/></w:tcPr><w:p><w:r><w:t>alder</w:t></w:r></w:p></w:tc>"#,
                r#"<w:tc><w:tcPr><w:tcW w:w="4000" w:type="dxa"/></w:tcPr><w:p><w:r><w:t>birch</w:t></w:r></w:p></w:tc>"#,
                "</w:tr>",
                r#"<w:tr><w:tc><w:tcPr><w:gridSpan w:val="2"/></w:tcPr><w:p><w:r><w:t>cedar</w:t></w:r></w:p></w:tc></w:tr>"#,
                "</w:tbl>",
            )
        );
        let doc = parse_raw_both(&body);
        assert_eq!(contents(&doc), vec!["alder\tbirch\ncedar"]);
    }

    /// feature-64 Review Focus 3: a cell inside a content control (`w:sdt`), as Word writes
    /// a form field, is still one field of its row; a self-closing `<w:tc/>` is an empty one
    /// (R1.1).
    #[test]
    fn a_cell_wrapped_in_a_content_control_is_still_a_cell() {
        let body = format!(
            "{}{}",
            heading_para(1, "Tariff"),
            concat!(
                "<w:tbl><w:tr>",
                "<w:sdt><w:sdtPr/><w:sdtContent><w:tc><w:p><w:r><w:t>alder</w:t></w:r></w:p></w:tc></w:sdtContent></w:sdt>",
                "<w:tc/>",
                "<w:tc><w:p><w:r><w:t>birch</w:t></w:r></w:p></w:tc>",
                "</w:tr><w:tr/></w:tbl>",
            )
        );
        let doc = parse_raw_both(&body);
        assert_eq!(contents(&doc), vec!["alder\t\tbirch"]);
    }

    /// feature-64 T4 (AC5): a cell's paragraphs join with a space, an empty one adds nothing,
    /// and a line break or carriage return inside a cell is a space, so the row stays one line.
    #[test]
    fn paragraphs_and_breaks_in_a_cell_join_with_a_space() {
        let body = format!(
            "{}{}",
            heading_para(1, "Tariff"),
            concat!(
                "<w:tbl><w:tr>",
                "<w:tc><w:p><w:r><w:t>alder</w:t></w:r></w:p><w:p></w:p><w:p><w:r><w:t>birch</w:t></w:r></w:p></w:tc>",
                "<w:tc><w:p><w:r><w:t>cedar</w:t><w:br/><w:t>damson</w:t><w:cr/><w:t>elm</w:t></w:r></w:p></w:tc>",
                "</w:tr></w:tbl>",
            )
        );
        let doc = parse_raw_both(&body);
        assert_eq!(contents(&doc), vec!["alder birch\tcedar damson elm"]);
    }

    /// feature-64 T5 (AC6): a tab inside a cell stays a tab, which a reader cannot tell from
    /// the cell boundary.
    #[test]
    fn a_tab_in_a_cell_stays_a_tab() {
        let body = format!(
            "{}{}",
            heading_para(1, "Tariff"),
            concat!(
                "<w:tbl><w:tr>",
                "<w:tc><w:p><w:r><w:t>alder</w:t><w:tab/><w:t>birch</w:t></w:r></w:p></w:tc>",
                "<w:tc><w:p><w:r><w:t>cedar</w:t></w:r></w:p></w:tc>",
                "</w:tr></w:tbl>",
            )
        );
        let doc = parse_raw_both(&body);
        assert_eq!(contents(&doc), vec!["alder\tbirch\tcedar"]);
    }

    /// feature-64 Review Focus 4: a cell holding only a tab trims to nothing and keeps its
    /// column as an empty field.
    #[test]
    fn a_cell_holding_only_a_tab_is_an_empty_field() {
        let body = format!(
            "{}{}",
            heading_para(1, "Tariff"),
            concat!(
                "<w:tbl><w:tr>",
                "<w:tc><w:p><w:r><w:t>alder</w:t></w:r></w:p></w:tc>",
                "<w:tc><w:p><w:r><w:tab/></w:r></w:p></w:tc>",
                "<w:tc><w:p><w:r><w:t>birch</w:t></w:r></w:p></w:tc>",
                "</w:tr></w:tbl>",
            )
        );
        let doc = parse_raw_both(&body);
        assert_eq!(contents(&doc), vec!["alder\t\tbirch"]);
    }

    /// feature-64 T6 (AC7): a heading alone in a cell writes out the row before it into the
    /// section it closes and opens its own; the cells after it are the new section's line,
    /// and the heading's own cell adds no field on either side.
    #[test]
    fn a_heading_in_a_cell_splits_the_row_where_it_stands() {
        let body = format!(
            "{}<w:tbl><w:tr><w:tc>{}</w:tc><w:tc>{}</w:tc><w:tc>{}</w:tc></w:tr></w:tbl>",
            heading_para(1, "Tariff"),
            body_para("alder"),
            heading_para(2, "Quota"),
            body_para("birch")
        );
        let doc = parse_raw_both(&body);
        assert_eq!(
            outline(&doc),
            vec![(Some("Tariff"), Some(2)), (Some("Quota"), Some(3))]
        );
        assert_eq!(contents(&doc), vec!["alder", "birch"]);
    }

    /// feature-64 T6b (AC7b): the paragraphs before a heading in its cell close the row on
    /// the old section's side; those after it open the new section's line.
    #[test]
    fn paragraphs_around_a_heading_in_a_cell_stay_on_their_side() {
        let body = format!(
            "{}<w:tbl><w:tr><w:tc>{}</w:tc><w:tc>{}{}{}</w:tc><w:tc>{}</w:tc></w:tr></w:tbl>",
            heading_para(1, "Tariff"),
            body_para("alder"),
            body_para("cedar"),
            heading_para(2, "Quota"),
            body_para("damson"),
            body_para("birch")
        );
        let doc = parse_raw_both(&body);
        assert_eq!(contents(&doc), vec!["alder\tcedar", "damson\tbirch"]);
    }

    /// feature-64 T6c (AC7c): a heading in a cell of a nested table splits that inner row at
    /// its own `</w:tr>`, and the outer row's rest follows; the text stays in document order.
    #[test]
    fn a_heading_in_a_nested_cell_keeps_document_order() {
        let inner = format!(
            "<w:tbl><w:tr><w:tc>{}{}{}</w:tc><w:tc>{}</w:tc></w:tr></w:tbl>",
            body_para("cedar"),
            heading_para(2, "Quota"),
            body_para("damson"),
            body_para("elm")
        );
        let body = format!(
            "{}<w:tbl><w:tr><w:tc>{}{inner}</w:tc><w:tc>{}</w:tc></w:tr></w:tbl>",
            heading_para(1, "Tariff"),
            body_para("alder"),
            body_para("birch")
        );
        let doc = parse_raw_both(&body);
        assert_eq!(
            outline(&doc),
            vec![(Some("Tariff"), Some(2)), (Some("Quota"), Some(3))]
        );
        assert_eq!(contents(&doc), vec!["alder\ncedar", "damson\telm\nbirch"]);
    }

    /// feature-64 T7 (AC8): a nested table loses no text, and its rows stand between the outer
    /// cell's text before it and the outer row's text after it. The separators are not pinned.
    #[test]
    fn a_nested_table_keeps_its_text_in_document_order() {
        let body = format!(
            "{}<w:tbl><w:tr><w:tc>{}{}</w:tc><w:tc>{}</w:tc></w:tr></w:tbl>",
            heading_para(1, "Tariff"),
            body_para("alder"),
            grid_of(&[&["cedar", "damson"]]),
            body_para("birch")
        );
        let doc = parse_raw_both(&body);
        let content = contents(&doc).concat();
        for word in ["alder", "cedar", "damson", "birch"] {
            assert_eq!(content.matches(word).count(), 1, "{word}: {content:?}");
        }
        let at = |word: &str| content.find(word).expect("present");
        assert!(at("alder") < at("cedar"), "{content:?}");
        assert!(at("damson") < at("birch"), "{content:?}");
    }

    /// feature-64 T8 (AC9): a table between two paragraphs is separated from each by one
    /// newline.
    #[test]
    fn a_table_sits_between_paragraphs_with_one_newline() {
        let body = format!(
            "{}{}{}{}",
            heading_para(1, "Tariff"),
            body_para("alder"),
            grid_of(&[&["birch", "cedar"]]),
            body_para("damson")
        );
        let doc = parse_raw_both(&body);
        assert_eq!(contents(&doc), vec!["alder\nbirch\tcedar\ndamson"]);
    }

    /// feature-64 T9 (AC10): outside a cell nothing changes -- a line break is a newline and a
    /// tab a tab, in a body paragraph and in a paragraph that sits in a table but outside
    /// every `<w:tc>` (R1.7).
    #[test]
    fn breaks_and_tabs_outside_a_table_are_unchanged() {
        let body = format!(
            "{}{}{}",
            heading_para(1, "Tariff"),
            "<w:p><w:r><w:t>alder</w:t><w:br/><w:t>birch</w:t><w:tab/><w:t>cedar</w:t></w:r></w:p>",
            concat!(
                "<w:tbl>",
                "<w:p><w:r><w:t>elm</w:t><w:br/><w:t>fir</w:t></w:r></w:p>",
                "<w:tr><w:tc><w:p><w:r><w:t>gum</w:t></w:r></w:p></w:tc></w:tr>",
                "</w:tbl>",
            )
        );
        let doc = parse_raw_both(&body);
        assert_eq!(contents(&doc), vec!["alder\nbirch\tcedar\nelm\nfir\ngum"]);
    }

    /// feature-64 T10 (AC11) and Review Focus 5: the rows under an excluded heading are in no
    /// chunk and not in `raw_content`; an excluded heading inside a cell drops the cells after
    /// it and keeps the ones before it in the section it closes.
    #[test]
    fn rows_under_an_excluded_heading_are_dropped() {
        let body = format!(
            "{}{}{}{}{}{}",
            heading_para(1, "Tariff"),
            grid_of(&[&["alder", "birch"]]),
            heading_para(1, "Secret"),
            grid_of(&[&["cedar", "damson"]]),
            heading_para(1, "Quota"),
            grid_of(&[&["elm", "fir"]])
        );
        let doc = DocxParser::default()
            .parse_bytes(&wrap_document_xml(&body), "docs/ledger.docx", &["Secret"])
            .expect("a generated document parses");
        assert_eq!(
            outline(&doc),
            vec![(Some("Tariff"), Some(2)), (Some("Quota"), Some(2))]
        );
        assert_eq!(contents(&doc), vec!["alder\tbirch", "elm\tfir"]);
        for word in ["cedar", "damson"] {
            assert!(
                !doc.raw_content.contains(word),
                "{word}: {:?}",
                doc.raw_content
            );
        }

        let in_cell = format!(
            "{}<w:tbl><w:tr><w:tc>{}</w:tc><w:tc>{}</w:tc><w:tc>{}</w:tc></w:tr></w:tbl>{}{}",
            heading_para(1, "Tariff"),
            body_para("alder"),
            heading_para(2, "Secret"),
            body_para("cedar"),
            heading_para(1, "Quota"),
            grid_of(&[&["elm"]])
        );
        let doc = DocxParser::default()
            .parse_bytes(
                &wrap_document_xml(&in_cell),
                "docs/ledger.docx",
                &["Secret"],
            )
            .expect("a generated document parses");
        assert_eq!(contents(&doc), vec!["alder", "elm"]);
        assert!(!doc.raw_content.contains("cedar"), "{:?}", doc.raw_content);
    }

    /// feature-64 T11 (AC12): XML that ends inside a table keeps the closed cells and the open
    /// cell's closed paragraphs, whether quick-xml stops with an error (a tag cut short) or at
    /// the end of input (no closing tags); a paragraph cut short adds nothing. Cut inside a
    /// nested table, the inner row comes after what the outer row wrote out before it.
    #[test]
    fn a_row_cut_short_by_broken_xml_keeps_its_closed_cells() {
        let head = concat!(
            r#"<?xml version="1.0"?>"#,
            r#"<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">"#,
            "<w:body><w:tbl><w:tr>",
            "<w:tc><w:p><w:r><w:t>alder</w:t></w:r></w:p></w:tc>",
            "<w:tc><w:p><w:r><w:t>birch</w:t></w:r></w:p>",
        );
        for tail in ["<w:p><w:r><w:t>cedar", "<w:p><w:r><w:t"] {
            let bytes = docx_with_raw_document_xml(&format!("{head}{tail}"));
            let doc = DocxParser::default()
                .parse_bytes(&bytes, "broken.docx", &[])
                .expect("a cut document still parses");
            assert_eq!(contents(&doc), vec!["alder\tbirch"], "tail {tail:?}");
        }

        let nested = concat!(
            r#"<?xml version="1.0"?>"#,
            r#"<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">"#,
            "<w:body><w:tbl><w:tr>",
            "<w:tc><w:p><w:r><w:t>elm</w:t></w:r></w:p></w:tc>",
            "<w:tc><w:tbl><w:tr>",
            "<w:tc><w:p><w:r><w:t>fir</w:t></w:r></w:p></w:tc>",
            "<w:tc><w:p><w:r><w:t>gum</w:t></w:r></w:p>",
        );
        let doc = DocxParser::default()
            .parse_bytes(&docx_with_raw_document_xml(nested), "broken.docx", &[])
            .expect("a cut document still parses");
        assert_eq!(contents(&doc), vec!["elm\nfir\tgum"]);
    }

    /// feature-64 Review Focus 6 (D3): a table whose structure is off -- a `<w:tc>` that starts
    /// before the open one closes, a `<w:tr>` that starts before the open one closes, a cell
    /// with no `<w:tr>` around it -- loses no text: each word appears exactly once. The open
    /// cell closes at the next `<w:tc>`, the open row is written out at the next `<w:tr>`, and
    /// a row still open at `</w:tbl>` is written out there.
    #[test]
    fn unclosed_rows_and_cells_lose_no_text() {
        let body = format!(
            "{}{}",
            heading_para(1, "Tariff"),
            concat!(
                "<w:tbl><w:tr><w:tc><w:p><w:r><w:t>alder</w:t></w:r></w:p>",
                "<w:tc><w:p><w:r><w:t>birch</w:t></w:r></w:p></w:tc>",
                "</w:tc></w:tr></w:tbl>",
                "<w:tbl><w:tr><w:tc><w:p><w:r><w:t>cedar</w:t></w:r></w:p></w:tc>",
                "<w:tr><w:tc><w:p><w:r><w:t>damson</w:t></w:r></w:p></w:tc></w:tr>",
                "</w:tr></w:tbl>",
                "<w:tbl><w:tc><w:p><w:r><w:t>elm</w:t></w:r></w:p></w:tc></w:tbl>",
            )
        );
        let doc = parse_raw_both(&body);
        assert_eq!(contents(&doc), vec!["alder\tbirch\ncedar\ndamson\nelm"]);
    }

    /// feature-64 T12r (AC13, D5): the read entry writes a table's rows the way the index
    /// entry does.
    #[test]
    fn the_read_entry_writes_table_rows_the_same_way() {
        let bytes = table_docx();
        let index = DocxParser::default()
            .parse_bytes(&bytes, "tariff.docx", &[])
            .unwrap();
        let read = DocxParser::default()
            .parse_bytes_for_read(&bytes, "tariff.docx", &[])
            .unwrap();
        assert_eq!(outline(&read), outline(&index));
        assert_eq!(contents(&read), contents(&index));
        assert_eq!(contents(&read), vec!["alder\tbirch\ncedar\tdamson"]);
    }
}
