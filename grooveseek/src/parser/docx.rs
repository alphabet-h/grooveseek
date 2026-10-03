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
/// ([`StyleTable::parse`]). Each case but a missing part is named on stderr in one line. With
/// `None` every paragraph's heading is decided by its style ID's spelling, as before.
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
    /// it has to be `w:styles` and to close before the input ends.
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
    /// A `Start` (`opens`) or `Empty` element, met with `self.depth` elements open.
    fn element(&mut self, e: &BytesStart, opens: bool) -> Result<(), StylesUnusable> {
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
                    id: attr_value(e, b"styleId").map(|v| style_id_key(&v)),
                    paragraph: attr_value(e, b"type").is_none_or(|t| t == b"paragraph"),
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
                            style.def.name = attr_value(e, b"val").and_then(|v| style_name(&v));
                        }
                        b"basedOn" if style.def.based_on.is_none() => {
                            style.def.based_on = attr_value(e, b"val").map(|v| style_id_key(&v));
                        }
                        _ => {}
                    }
                }
            }
            3 if name == b"outlineLvl" && self.open_child.as_deref() == Some(b"pPr".as_slice()) => {
                if let Some(style) = self.current.as_mut()
                    && style.def.outline_lvl.is_none()
                {
                    style.def.outline_lvl = attr_value(e, b"val").and_then(|v| outline_level(&v));
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
fn attr_value(e: &BytesStart, local: &[u8]) -> Option<Vec<u8>> {
    e.attributes()
        .flatten()
        .find(|a| super::ooxml_local(a.key.as_ref()) == local)
        .map(|a| a.value.into_owned())
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

/// `word/document.xml` を段落 (`<w:p>`) 単位で読み、見出し段落を見出し境界として Markdown 同様の
/// 階層チャンクに変換する。段落が見出しかどうかは、その `<w:pStyle>` を
/// [`heading_level_from_attr`] が `styles` (`word/styles.xml` の style 表、使えなければ `None`)
/// で決める (feature-62)。
///
/// 表 (`w:tbl`) 内のテキストも専用ハンドリングはしない: OOXML 上は
/// `w:tbl > w:tr > w:tc > w:p > w:r > w:t` と入れ子になっているだけなので、
/// 通常の `<w:p>` 境界処理だけで現在のセクション本文に自然に取り込まれる。
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
    struct Section {
        heading: Option<String>,
        level: Option<u8>,
        ancestry: Vec<String>,
        body: String,
    }
    let mut sections: Vec<Section> = vec![Section {
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

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => match super::ooxml_local(e.name().as_ref()) {
                b"p" => {
                    para_style = None;
                    para_text.clear();
                }
                b"pStyle" => para_style = heading_level_from_attr(&e, styles),
                b"t" => in_text = true,
                // `<w:br></w:br>` の形で来ることもある。Empty 版と同じ扱い。
                name => push_intra_paragraph_separator(name, &mut para_text),
            },
            Ok(Event::Empty(e)) => {
                // `e.name()` は一時値なので、`as_ref()` の借用元を束縛しておく。
                let qname = e.name();
                let name = super::ooxml_local(qname.as_ref());
                // `<w:pStyle w:val="Heading1"/>` は自己終端タグで来ることが多い。
                if name == b"pStyle" {
                    para_style = heading_level_from_attr(&e, styles);
                } else {
                    push_intra_paragraph_separator(name, &mut para_text);
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
                    if let Some(level) = para_style {
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
                            sections.push(Section {
                                heading: Some(text),
                                level: Some(level),
                                ancestry,
                                body: String::new(),
                            });
                        }
                    } else if !excluded && !text.is_empty() {
                        let last = sections.last_mut().expect("sections is never empty");
                        if !last.body.is_empty() {
                            last.body.push('\n');
                        }
                        last.body.push_str(&text);
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

    sections
        .into_iter()
        .filter(|s| s.heading.is_some() || !s.body.trim().is_empty())
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

/// (feature-62) docx bytes for the unit tests of this parser, of `crate::server` and of
/// `crate::indexer`, so the three build a document one way. The integration tests under
/// `grooveseek/tests/` cannot reach a `cfg(test)` module and carry a copy of their own.
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
    /// `test_docx_unclosed_root_at_eof_is_detected_as_truncation` does.
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
    /// next to `heading_digits`.
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
}
