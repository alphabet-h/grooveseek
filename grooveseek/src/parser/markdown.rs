//! Markdown (`.md`) parser. Moved from the old `src/markdown.rs` and adapted
//! to the `Parser` trait. Behaviour is identical to legacy.

use std::collections::BTreeMap;
use std::fmt;

use serde::de::{self, EnumAccess, IgnoredAny, MapAccess, SeqAccess, VariantAccess, Visitor};
use serde::{Deserialize, Deserializer};

use super::{Chunk, FieldValue, Frontmatter, ParsedDocument, Parser};

/// Markdown parser. Handles YAML frontmatter + heading-based chunking using
/// [`pulldown-cmark`](https://crates.io/crates/pulldown-cmark) rules informally
/// (we only split on `## ` / `### ` prefixes, we do not traverse the AST).
pub struct MarkdownParser;

impl Parser for MarkdownParser {
    fn extension(&self) -> &'static str {
        "md"
    }

    fn parse(&self, raw: &str, path_hint: &str, exclude_headings: &[&str]) -> ParsedDocument {
        let (frontmatter, body, frontmatter_error) = extract_frontmatter(raw);
        // context 用 title: frontmatter.title (非空) → filename stem fallback (E-1)。
        // documents.title は frontmatter.title のまま (ここでは context 生成にのみ使う)。
        let ctx_title = frontmatter
            .title
            .as_deref()
            .filter(|t| !t.trim().is_empty())
            .map(str::to_string)
            .or_else(|| super::txt::derive_title_pub(path_hint));
        let chunks = chunk_body(&body, exclude_headings, ctx_title.as_deref());
        ParsedDocument {
            frontmatter,
            chunks,
            raw_content: raw.to_string(),
            frontmatter_error,
        }
    }
}

/// Tag on a Markdown document whose YAML frontmatter could not be parsed (#251).
///
/// Such a document goes into the index with `title`, `date`, `topic` and
/// `depth` empty, so a filter on any of them silently drops it; this tag is
/// what makes it findable again (`tags_any: ["frontmatter:unparsed"]`). Like
/// every tag it is frontmatter, so a note with valid YAML can declare it by
/// hand; [`ParsedDocument::frontmatter_error`] is the parser's own word.
pub const TAG_FRONTMATTER_UNPARSED: &str = "frontmatter:unparsed";

// ---------------------------------------------------------------------------
// Internal: serde helper for flexible YAML deserialization
// ---------------------------------------------------------------------------

/// The YAML merge key. It arrives as a key like any other and is dropped here,
/// as it was before [`Frontmatter::extra`] existed: kept, it would show up as a literal `"<<"`
/// entry that every strict run reports as undeclared. Merge expansion runs only
/// on the fallback path a successful direct deserialize never takes, so the
/// value behind it is unexpanded and is not read either.
const MERGE_KEY: &str = "<<";

/// Intermediate representation for serde_yaml_bw deserialization.
/// `date` is captured as `serde_yaml_bw::Value` so it works regardless of whether
/// the YAML encodes it as a string (`"2026-04-10"`) or a native date value.
/// [`RawFrontmatter::extra`] (feature-57) receives every other top-level key.
///
/// `Deserialize` is written out below rather than derived with
/// `#[serde(flatten)]`: flatten routes the whole struct through serde's
/// buffering path, which materialises every unknown value into a `Content`
/// tree before anything decides it is opaque. That made a deep mapping or a
/// wide anchor graph under an unknown key cost what walking it costs, and a
/// document 1.8.0 indexed fine could be refused outright. The visitor reads
/// each unknown value for its shape only and skips the nesting with
/// `IgnoredAny`, so retaining a key costs what the YAML parser already paid.
struct RawFrontmatter {
    title: Option<String>,
    date: Option<serde_yaml_bw::Value>,
    topic: Option<String>,
    depth: Option<serde_yaml_bw::Value>,
    tags: Vec<String>,
    extra: BTreeMap<String, FieldValue>,
}

impl<'de> Deserialize<'de> for RawFrontmatter {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        // `deserialize_map`, not `deserialize_struct` with a field list:
        // a struct's field list is what filters unknown keys out before the
        // visitor sees them, and every one of those is what `extra` is for.
        deserializer.deserialize_map(RawFrontmatterVisitor)
    }
}

struct RawFrontmatterVisitor;

impl<'de> Visitor<'de> for RawFrontmatterVisitor {
    type Value = RawFrontmatter;

    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("a YAML mapping of frontmatter keys")
    }

    fn visit_map<M: MapAccess<'de>>(self, mut map: M) -> Result<Self::Value, M::Error> {
        let mut title: Option<Option<String>> = None;
        let mut date: Option<Option<serde_yaml_bw::Value>> = None;
        let mut topic: Option<Option<String>> = None;
        let mut depth: Option<Option<serde_yaml_bw::Value>> = None;
        let mut tags: Option<Vec<String>> = None;
        let mut extra: BTreeMap<String, FieldValue> = BTreeMap::new();

        while let Some(key) = map.next_key::<String>()? {
            // The five named keys keep the types they always had, so a value
            // the wrong shape for one of them is refused exactly as before.
            match key.as_str() {
                "title" => {
                    if title.is_some() {
                        return Err(de::Error::duplicate_field("title"));
                    }
                    title = Some(map.next_value()?);
                }
                "date" => {
                    if date.is_some() {
                        return Err(de::Error::duplicate_field("date"));
                    }
                    date = Some(map.next_value()?);
                }
                "topic" => {
                    if topic.is_some() {
                        return Err(de::Error::duplicate_field("topic"));
                    }
                    topic = Some(map.next_value()?);
                }
                "depth" => {
                    if depth.is_some() {
                        return Err(de::Error::duplicate_field("depth"));
                    }
                    depth = Some(map.next_value()?);
                }
                "tags" => {
                    if tags.is_some() {
                        return Err(de::Error::duplicate_field("tags"));
                    }
                    tags = Some(map.next_value()?);
                }
                MERGE_KEY => {
                    map.next_value::<IgnoredAny>()?;
                }
                _ => {
                    let value = map.next_value::<FieldValue>()?;
                    extra.insert(key, value);
                }
            }
        }

        Ok(RawFrontmatter {
            title: title.flatten(),
            date: date.flatten(),
            topic: topic.flatten(),
            depth: depth.flatten(),
            tags: tags.unwrap_or_default(),
            extra,
        })
    }
}

/// The text a number is held as. It goes through `serde_yaml_bw::Number` so
/// that a float prints the way YAML writes one (`1.0`, not Rust's `1`), which
/// is what the `Value`-based classifier this replaced did.
fn number_text(n: impl Into<serde_yaml_bw::Number>) -> String {
    n.into().to_string()
}

/// Reads a retained value for its **shape**, never its contents (feature-57).
///
/// Every nested thing is drained with `IgnoredAny`: a mapping's entries and
/// a non-scalar sequence element are skipped rather than built, so an unknown
/// key costs the same walk the YAML parser was already doing. Aliases and
/// standard tags are resolved by the deserializer before a visitor sees them,
/// so there is no alias or tagged shape here -- [`FieldValue::Other`] is exactly `"mapping"`,
/// `"nested sequence"`, `"binary"` or [`FieldValue::NULL`].
struct FieldValueVisitor;

impl<'de> Deserialize<'de> for FieldValue {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(FieldValueVisitor)
    }
}

impl<'de> Visitor<'de> for FieldValueVisitor {
    type Value = FieldValue;

    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("any YAML value")
    }

    fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
        Ok(FieldValue::Scalar(v.to_string()))
    }

    fn visit_string<E: de::Error>(self, v: String) -> Result<Self::Value, E> {
        Ok(FieldValue::Scalar(v))
    }

    fn visit_bool<E: de::Error>(self, v: bool) -> Result<Self::Value, E> {
        Ok(FieldValue::Scalar(v.to_string()))
    }

    fn visit_i64<E: de::Error>(self, v: i64) -> Result<Self::Value, E> {
        Ok(FieldValue::Scalar(number_text(v)))
    }

    fn visit_u64<E: de::Error>(self, v: u64) -> Result<Self::Value, E> {
        Ok(FieldValue::Scalar(number_text(v)))
    }

    fn visit_f64<E: de::Error>(self, v: f64) -> Result<Self::Value, E> {
        Ok(FieldValue::Scalar(number_text(v)))
    }

    /// An integer too wide for `i64` / `u64`. `serde_yaml_bw` hands those over
    /// as 128-bit rather than as text, and serde's default for these is an
    /// error -- which would refuse a whole block over a key nobody declared.
    fn visit_i128<E: de::Error>(self, v: i128) -> Result<Self::Value, E> {
        Ok(FieldValue::Scalar(v.to_string()))
    }

    fn visit_u128<E: de::Error>(self, v: u128) -> Result<Self::Value, E> {
        Ok(FieldValue::Scalar(v.to_string()))
    }

    /// A key written with no value (`status:`, `~`, `null`).
    fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
        Ok(FieldValue::Other(FieldValue::NULL))
    }

    fn visit_none<E: de::Error>(self) -> Result<Self::Value, E> {
        Ok(FieldValue::Other(FieldValue::NULL))
    }

    /// An explicit `!!binary`, which `serde_yaml_bw` base64-decodes and hands
    /// over as bytes. It is opaque like a mapping: the decoded bytes are not a
    /// string and are not kept. Without this arm serde's default is an error,
    /// which would refuse a whole block over a key nobody declared -- 1.8.0
    /// dropped the key and indexed the document.
    fn visit_bytes<E: de::Error>(self, _v: &[u8]) -> Result<Self::Value, E> {
        Ok(FieldValue::Other("binary"))
    }

    fn visit_byte_buf<E: de::Error>(self, _v: Vec<u8>) -> Result<Self::Value, E> {
        Ok(FieldValue::Other("binary"))
    }

    fn visit_map<M: MapAccess<'de>>(self, mut map: M) -> Result<Self::Value, M::Error> {
        while map.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
        Ok(FieldValue::Other("mapping"))
    }

    fn visit_seq<S: SeqAccess<'de>>(self, mut seq: S) -> Result<Self::Value, S::Error> {
        let mut items = Vec::new();
        let mut opaque = false;
        // Draining continues past the first non-scalar: stopping mid-sequence
        // would leave the deserializer's own position inside it.
        while let Some(SeqElement(text)) = seq.next_element::<SeqElement>()? {
            match text {
                Some(s) => items.push(s),
                None => opaque = true,
            }
        }
        if opaque {
            Ok(FieldValue::Other("nested sequence"))
        } else {
            Ok(FieldValue::List(items))
        }
    }

    /// A value carrying a custom tag (`!Kind value`) reaches `deserialize_any`
    /// as an enum. The tag is dropped and the value it wraps decides the shape,
    /// the way a standard tag (`!!str x`) is resolved before it gets here.
    fn visit_enum<A: EnumAccess<'de>>(self, data: A) -> Result<Self::Value, A::Error> {
        let (_tag, variant) = data.variant::<IgnoredAny>()?;
        variant.newtype_variant::<FieldValue>()
    }
}

/// One element of a retained sequence: `Some(text)` for a scalar, `None` for a
/// shape that makes the whole sequence opaque. The non-scalar is drained, not
/// read -- that is what keeps a nested structure from being walked.
struct SeqElement(Option<String>);

impl<'de> Deserialize<'de> for SeqElement {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(SeqElementVisitor)
    }
}

struct SeqElementVisitor;

impl<'de> Visitor<'de> for SeqElementVisitor {
    type Value = SeqElement;

    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("any YAML value")
    }

    fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
        Ok(SeqElement(Some(v.to_string())))
    }

    fn visit_string<E: de::Error>(self, v: String) -> Result<Self::Value, E> {
        Ok(SeqElement(Some(v)))
    }

    fn visit_bool<E: de::Error>(self, v: bool) -> Result<Self::Value, E> {
        Ok(SeqElement(Some(v.to_string())))
    }

    fn visit_i64<E: de::Error>(self, v: i64) -> Result<Self::Value, E> {
        Ok(SeqElement(Some(number_text(v))))
    }

    fn visit_u64<E: de::Error>(self, v: u64) -> Result<Self::Value, E> {
        Ok(SeqElement(Some(number_text(v))))
    }

    fn visit_f64<E: de::Error>(self, v: f64) -> Result<Self::Value, E> {
        Ok(SeqElement(Some(number_text(v))))
    }

    fn visit_i128<E: de::Error>(self, v: i128) -> Result<Self::Value, E> {
        Ok(SeqElement(Some(v.to_string())))
    }

    fn visit_u128<E: de::Error>(self, v: u128) -> Result<Self::Value, E> {
        Ok(SeqElement(Some(v.to_string())))
    }

    fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
        Ok(SeqElement(None))
    }

    fn visit_none<E: de::Error>(self) -> Result<Self::Value, E> {
        Ok(SeqElement(None))
    }

    /// `!!binary` is not a scalar the list can hold, so it makes the whole
    /// sequence opaque the way a mapping element does.
    fn visit_bytes<E: de::Error>(self, _v: &[u8]) -> Result<Self::Value, E> {
        Ok(SeqElement(None))
    }

    fn visit_byte_buf<E: de::Error>(self, _v: Vec<u8>) -> Result<Self::Value, E> {
        Ok(SeqElement(None))
    }

    fn visit_map<M: MapAccess<'de>>(self, mut map: M) -> Result<Self::Value, M::Error> {
        while map.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
        Ok(SeqElement(None))
    }

    fn visit_seq<S: SeqAccess<'de>>(self, mut seq: S) -> Result<Self::Value, S::Error> {
        while seq.next_element::<IgnoredAny>()?.is_some() {}
        Ok(SeqElement(None))
    }

    fn visit_enum<A: EnumAccess<'de>>(self, data: A) -> Result<Self::Value, A::Error> {
        let (_tag, variant) = data.variant::<IgnoredAny>()?;
        variant.newtype_variant::<SeqElement>()
    }
}

impl From<RawFrontmatter> for Frontmatter {
    fn from(raw: RawFrontmatter) -> Self {
        // serde_yaml_bw::Value は (value, tag) の 2-field 。tag はここでは使わない
        // ので `_` で無視する。
        let date = raw.date.map(|v| match v {
            serde_yaml_bw::Value::String(s, _) => s,
            other => {
                let s = format!("{other:?}");
                s.trim_matches('"').to_string()
            }
        });

        let depth = raw.depth.map(|v| match v {
            serde_yaml_bw::Value::String(s, _) => s,
            serde_yaml_bw::Value::Number(n, _) => n.to_string(),
            other => format!("{other:?}"),
        });

        Frontmatter {
            title: raw.title,
            date,
            topic: raw.topic,
            depth,
            tags: raw.tags,
            extra: raw.extra,
        }
    }
}

// ---------------------------------------------------------------------------
// Frontmatter extraction
// ---------------------------------------------------------------------------

/// Split `raw` into frontmatter, body and, when a `---` block was found but
/// its YAML was refused, the parser's error text.
///
/// The three shapes a file can take are told apart by the caller only through
/// that third element: no block and an unterminated block both come back as
/// `(default, body, None)`, as before; a block that does not parse comes back
/// with the [`TAG_FRONTMATTER_UNPARSED`] tag and `Some(reason)`. Nothing is
/// printed here -- see [`ParsedDocument::frontmatter_error`] for why.
fn extract_frontmatter(raw: &str) -> (Frontmatter, String, Option<String>) {
    let trimmed = raw.trim_start_matches('\u{feff}'); // strip BOM if present

    if !trimmed.starts_with("---") {
        return (Frontmatter::default(), trimmed.to_string(), None);
    }

    let after_first = &trimmed[3..];
    let after_first = after_first.trim_start_matches('\r'); // handle \r\n
    let after_first = after_first.strip_prefix('\n').unwrap_or(after_first);

    if let Some(end) = after_first.find("\n---") {
        let yaml_raw = &after_first[..end];
        let body_start = end + 4; // skip the `\n---`
        let rest = &after_first[body_start..];
        let body = rest.trim_start_matches(['\r', '\n']).to_string();

        // Windows 生成の `.md` で `\r\n` 改行のとき、yaml_raw 各行末に `\r` が
        // 残って serde_yaml_bw のパース結果の文字列 value にリークするので
        // パース前に CRLF → LF へ正規化する。
        let yaml_normalized;
        let yaml_str: &str = if yaml_raw.contains('\r') {
            yaml_normalized = yaml_raw.replace("\r\n", "\n").replace('\r', "\n");
            &yaml_normalized
        } else {
            yaml_raw
        };

        match serde_yaml_bw::from_str::<RawFrontmatter>(yaml_str) {
            Ok(raw_fm) => (Frontmatter::from(raw_fm), body, None),
            Err(e) => {
                let fm = Frontmatter {
                    tags: vec![TAG_FRONTMATTER_UNPARSED.to_string()],
                    ..Frontmatter::default()
                };
                (fm, body, Some(e.to_string()))
            }
        }
    } else {
        (Frontmatter::default(), trimmed.to_string(), None)
    }
}

// ---------------------------------------------------------------------------
// Heading-based chunking
// ---------------------------------------------------------------------------

/// (heading (level, text), 祖先見出しスナップショット, content)。
/// clippy::type_complexity 回避のための alias。
type RawChunk = (Option<(u8, String)>, Vec<String>, String);

fn chunk_body(body: &str, excludes: &[&str], title: Option<&str>) -> Vec<Chunk> {
    // raw_chunks: (heading, 祖先見出しスナップショット, content)
    let mut raw_chunks: Vec<RawChunk> = Vec::new();
    let mut current_heading: Option<(u8, String)> = None;
    let mut current_ancestry: Vec<String> = Vec::new();
    let mut current_lines: Vec<&str> = Vec::new();
    let mut excluded = false;
    // ancestry stack: 現在位置より浅い見出しの列 (level ASC)。exclude 見出しも積む (E-6)。
    let mut stack: Vec<(u8, String)> = Vec::new();

    for line in body.lines() {
        if let Some((level, heading_text)) = strip_heading(line) {
            // 1) 直前 chunk を flush (heading/ancestry は「今開いている chunk」のもの)
            if !excluded {
                let content = current_lines.join("\n").trim().to_string();
                if !content.is_empty() || current_heading.is_some() {
                    raw_chunks.push((current_heading.clone(), current_ancestry.clone(), content));
                }
            }
            current_lines.clear();

            // 2) この level に対して stack を pop → 残りが自見出しの祖先
            while let Some((l, _)) = stack.last() {
                if *l >= level {
                    stack.pop();
                } else {
                    break;
                }
            }
            let ancestry: Vec<String> = stack.iter().map(|(_, h)| h.clone()).collect();
            // 3) この見出しを stack へ (exclude されても積む = E-6)
            stack.push((level, heading_text.clone()));

            // 4) 次 chunk の state を確定
            if excludes.iter().any(|ex| heading_text.contains(ex)) {
                excluded = true;
                current_heading = None;
                current_ancestry = Vec::new();
            } else {
                excluded = false;
                current_heading = Some((level, heading_text));
                current_ancestry = ancestry;
            }
        } else if !excluded {
            current_lines.push(line);
        }
    }
    if !excluded {
        let content = current_lines.join("\n").trim().to_string();
        if !content.is_empty() || current_heading.is_some() {
            raw_chunks.push((current_heading, current_ancestry, content));
        }
    }

    // 50-char 未満 chunk は直前 chunk に merge。heading / level / **ancestry** は
    // 最初に出現した heading のものを維持 (legacy 挙動 + E-9 の ancestry 引継ぎ)。
    let mut merged: Vec<RawChunk> = Vec::new();
    for (heading, ancestry, content) in raw_chunks {
        if content.len() < 50 && !merged.is_empty() {
            let prev = merged.last_mut().unwrap();
            if !prev.2.is_empty() {
                prev.2.push_str("\n\n");
            }
            if let Some((_lvl, ref h)) = heading {
                prev.2.push_str(&format!("## {h}\n\n"));
            }
            prev.2.push_str(&content);
        } else {
            merged.push((heading, ancestry, content));
        }
    }

    merged
        .into_iter()
        .enumerate()
        .map(|(i, (heading_pair, ancestry, content))| {
            let (level, heading) = match heading_pair {
                Some((lvl, text)) => (Some(lvl), Some(text)),
                None => (None, None),
            };
            // context parts: [title, ...ancestry, heading]
            let mut parts: Vec<&str> = Vec::with_capacity(ancestry.len() + 2);
            if let Some(t) = title {
                parts.push(t);
            }
            for a in &ancestry {
                parts.push(a);
            }
            if let Some(h) = &heading {
                parts.push(h);
            }
            let context = super::build_context(&parts);
            Chunk {
                index: i,
                heading,
                level,
                content,
                context,
                line_range: None,
                symbol_kind: None,
            }
        })
        .collect()
}

fn strip_heading(line: &str) -> Option<(u8, String)> {
    let trimmed = line.trim();
    if let Some(rest) = trimmed.strip_prefix("### ") {
        Some((3, rest.trim().to_string()))
    } else {
        trimmed
            .strip_prefix("## ")
            .map(|rest| (2, rest.trim().to_string()))
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(md: &str) -> ParsedDocument {
        MarkdownParser.parse(md, "test.md", &[])
    }

    // #251: a frontmatter block that does not parse is reported as data, not
    // printed, and the document carries the tag that makes it findable again.
    #[test]
    fn test_broken_frontmatter_is_tagged_and_reports_error() {
        let md = "---\ntitle: [unclosed\n---\n\n# Broken\n\nBody long enough to stand as one chunk on its own here.\n";
        let doc = parse(md);
        assert!(doc.frontmatter_error.is_some());
        assert_eq!(doc.frontmatter.tags, vec![TAG_FRONTMATTER_UNPARSED]);
        assert_eq!(doc.frontmatter.title, None);
        assert_eq!(doc.chunks.len(), 1);
        assert!(!doc.chunks[0].content.contains("title:"));
    }

    #[test]
    fn test_valid_and_absent_frontmatter_carry_no_error() {
        let valid = parse(
            "---\ntitle: Fine\ntags: [ok]\n---\n\nBody long enough to stand as one chunk on its own here.\n",
        );
        assert_eq!(valid.frontmatter_error, None);
        assert_eq!(valid.frontmatter.tags, vec!["ok"]);

        let absent =
            parse("# No frontmatter\n\nBody long enough to stand as one chunk on its own here.\n");
        assert_eq!(absent.frontmatter_error, None);
        assert!(absent.frontmatter.tags.is_empty());
    }

    #[test]
    fn test_unterminated_frontmatter_is_not_an_error() {
        let doc = parse("---\ntitle: x\nno closing fence, so this is body text as before.\n");
        assert_eq!(doc.frontmatter_error, None);
        assert!(doc.frontmatter.tags.is_empty());
    }

    // feature-57: every key the five named fields do not claim is retained by
    // shape, so a schema can name it later.
    #[test]
    fn test_unknown_keys_are_retained_by_shape() {
        use crate::parser::FieldValue;
        let md = "---\ntitle: T\nstatus: active\nenvironment: [dev, test]\nflag: false\nn: 10\nratio: 1.5\nmeta: {a: 1}\nempty:\nnested: [[a]]\nlong: |\n  two\n  lines\ntagged: !!str yes\ninlist: [!!str a, b]\n---\n\nBody long enough to stand as one chunk on its own here.\n";
        let doc = parse(md);
        assert_eq!(doc.frontmatter_error, None);
        assert_eq!(doc.frontmatter.title.as_deref(), Some("T"));
        let e = &doc.frontmatter.extra;
        assert_eq!(e["status"], FieldValue::Scalar("active".into()));
        assert_eq!(
            e["environment"],
            FieldValue::List(vec!["dev".into(), "test".into()])
        );
        assert_eq!(e["flag"], FieldValue::Scalar("false".into()));
        assert_eq!(e["n"], FieldValue::Scalar("10".into()));
        assert_eq!(e["ratio"], FieldValue::Scalar("1.5".into()));
        assert_eq!(e["meta"], FieldValue::Other("mapping"));
        assert_eq!(e["empty"], FieldValue::Other("null"));
        assert_eq!(e["nested"], FieldValue::Other("nested sequence"));
        assert_eq!(e["long"], FieldValue::Scalar("two\nlines\n".into()));
        assert_eq!(e["tagged"], FieldValue::Scalar("yes".into()));
        assert_eq!(e["inlist"], FieldValue::List(vec!["a".into(), "b".into()]));
        assert!(!e.contains_key("title"), "a named field is not an extra");
        assert_eq!(e.len(), 11, "every unknown key and nothing else: {e:?}");
    }

    /// A document with no unknown keys has an empty map; the five named
    /// fields behave exactly as before the map existed.
    #[test]
    fn test_named_fields_only_leaves_extra_empty() {
        let doc = parse(
            "---\ntitle: T\ndate: 2026-09-09\ntopic: x\ndepth: 2\ntags: [a]\n---\n\nBody long enough to stand as one chunk on its own here.\n",
        );
        assert_eq!(doc.frontmatter_error, None);
        assert!(doc.frontmatter.extra.is_empty());
        assert_eq!(doc.frontmatter.date.as_deref(), Some("2026-09-09"));
        assert_eq!(doc.frontmatter.depth.as_deref(), Some("2"));
        assert_eq!(doc.frontmatter.tags, vec!["a"]);
    }

    /// The YAML merge key is never surfaced: retained, it would arrive as a
    /// literal `"<<"` key and be an undeclared field in every strict run. The
    /// document itself still parses -- dropping the key is not refusing it.
    #[test]
    fn test_merge_key_is_not_an_extra() {
        let doc = parse(
            "---\nbase: &b\n  status: active\n<<: *b\ntitle: T\n---\n\nBody long enough to stand as one chunk on its own here.\n",
        );
        assert_eq!(doc.frontmatter_error, None, "a merge key is valid YAML");
        assert!(
            !doc.frontmatter.extra.contains_key("<<"),
            "merge key leaked: {:?}",
            doc.frontmatter.extra
        );
        assert_eq!(
            doc.frontmatter.extra["base"],
            crate::parser::FieldValue::Other("mapping")
        );
    }

    /// Reading the block through a hand-written visitor is the mechanism most
    /// likely to change how a wrong-typed named field is refused. Pin that it
    /// still is. (A scalar number into a `String` field, e.g. `title: 123`, is
    /// coerced by `serde_yaml_bw` rather than refused, both before and after
    /// this change, so it is not a refusal case here -- see
    /// [`test_named_scalar_coercion_is_unchanged_with_extra`].)
    #[test]
    fn test_named_field_type_mismatch_is_still_refused_with_extra() {
        for yaml in ["topic: [a]", "tags: [[a]]", "tags: notalist"] {
            let doc = parse(&format!(
                "---\n{yaml}\nstatus: active\n---\n\nBody long enough to stand as one chunk on its own here.\n"
            ));
            assert!(
                doc.frontmatter_error.is_some(),
                "{yaml:?} must still be refused, got {:?}",
                doc.frontmatter
            );
            assert_eq!(doc.frontmatter.tags, vec![TAG_FRONTMATTER_UNPARSED]);
            assert!(
                doc.frontmatter.extra.is_empty(),
                "a refused block keeps nothing"
            );
        }
    }

    /// `title: 123` is not a refusal case (see the doc comment above): pin
    /// that the coercion itself, and [`Frontmatter::extra`] alongside it, are unaffected by
    /// retaining unknown keys.
    #[test]
    fn test_named_scalar_coercion_is_unchanged_with_extra() {
        use crate::parser::FieldValue;
        let doc = parse(
            "---\ntitle: 123\nstatus: active\n---\n\nBody long enough to stand as one chunk on its own here.\n",
        );
        assert_eq!(doc.frontmatter_error, None);
        assert_eq!(doc.frontmatter.title.as_deref(), Some("123"));
        assert_eq!(
            doc.frontmatter.extra["status"],
            FieldValue::Scalar("active".into())
        );
    }

    /// An anchor graph under an unknown key is the document 1.8.0 indexed: it
    /// parses, and the key is one opaque value. Retaining it must not cost
    /// more than the YAML parser already paid -- buffering the expansion is
    /// what would spend `serde_yaml_bw`'s repetition budget and refuse a
    /// document that used to be fine.
    #[test]
    fn test_alias_bomb_under_unknown_key_is_safe() {
        let bomb = "---\ntitle: bomb\nblob:\n  - &a x\n  - &b [*a, *a, *a, *a, *a, *a, *a, *a]\n  - &c [*b, *b, *b, *b, *b, *b, *b, *b]\n  - &d [*c, *c, *c, *c, *c, *c, *c, *c]\n  - &e [*d, *d, *d, *d, *d, *d, *d, *d]\n---\nbody\n";
        let doc = parse(bomb);
        assert_eq!(
            doc.frontmatter_error, None,
            "1.8.0 indexed this document; retaining a key must not refuse it"
        );
        assert_eq!(
            doc.frontmatter.extra["blob"],
            crate::parser::FieldValue::Other("nested sequence")
        );
    }

    /// A mapping nested far deeper than any schema would name is still one
    /// opaque value: the visitor skips what is under it, so the depth the
    /// parser tolerates is the depth it tolerated before [`Frontmatter::extra`] existed.
    #[test]
    fn test_deep_mapping_under_unknown_key_parses() {
        const DEPTH: usize = 130;
        let mut yaml = String::from("---\nblob:\n");
        for i in 1..=DEPTH {
            yaml.push_str(&"  ".repeat(i));
            yaml.push_str(&format!("k{i}:\n"));
        }
        yaml.push_str("---\nbody\n");
        let doc = parse(&yaml);
        assert_eq!(
            doc.frontmatter_error, None,
            "a {DEPTH}-deep mapping under an unknown key must still parse"
        );
        assert_eq!(
            doc.frontmatter.extra["blob"],
            crate::parser::FieldValue::Other("mapping")
        );

        // The same document with a title: the named field is read as usual.
        let with_title = yaml.replacen("---\nblob:\n", "---\ntitle: T\nblob:\n", 1);
        let doc = parse(&with_title);
        assert_eq!(doc.frontmatter_error, None);
        assert_eq!(doc.frontmatter.title.as_deref(), Some("T"));
        assert_eq!(
            doc.frontmatter.extra["blob"],
            crate::parser::FieldValue::Other("mapping")
        );
    }

    /// An explicit `!!binary` is base64-decoded into bytes, which no shape can
    /// hold as text. It is opaque rather than a refusal: 1.8.0 dropped the key
    /// and indexed the document, and retaining keys must not change that.
    #[test]
    fn test_binary_under_unknown_key_is_opaque() {
        use crate::parser::FieldValue;
        let doc = parse(
            "---\ntitle: T\nblob: !!binary \"aGVsbG8=\"\n---\n\nBody long enough to stand as one chunk on its own here.\n",
        );
        assert_eq!(doc.frontmatter_error, None);
        assert_eq!(doc.frontmatter.title.as_deref(), Some("T"));
        assert_eq!(doc.frontmatter.extra["blob"], FieldValue::Other("binary"));

        // In a sequence it is not a scalar, so the whole list goes opaque.
        let in_list = parse(
            "---\ntitle: T\nblob: [a, !!binary \"aGVsbG8=\"]\n---\n\nBody long enough to stand as one chunk on its own here.\n",
        );
        assert_eq!(in_list.frontmatter_error, None);
        assert_eq!(
            in_list.frontmatter.extra["blob"],
            FieldValue::Other("nested sequence")
        );
    }

    /// An alias under an unknown key arrives as the value it points at: the
    /// deserializer resolves it before any visitor sees it, so there is no
    /// alias shape to hold (feature-57, acceptance criterion 13).
    #[test]
    fn test_alias_under_unknown_key_is_resolved() {
        use crate::parser::FieldValue;
        let doc = parse(
            "---\nbase: &b active\nstatus: *b\ntitle: T\n---\n\nBody long enough to stand as one chunk on its own here.\n",
        );
        assert_eq!(doc.frontmatter_error, None);
        assert_eq!(
            doc.frontmatter.extra["status"],
            FieldValue::Scalar("active".into())
        );
    }

    #[test]
    fn test_strip_heading_returns_h2_level() {
        let result = strip_heading("## Foo");
        assert_eq!(result, Some((2, "Foo".to_string())));
    }

    #[test]
    fn test_strip_heading_returns_h3_level() {
        let result = strip_heading("### Bar");
        assert_eq!(result, Some((3, "Bar".to_string())));
    }

    #[test]
    fn test_strip_heading_no_heading_returns_none() {
        assert_eq!(strip_heading("plain text"), None);
        assert_eq!(strip_heading("# H1 ignored"), None);
        assert_eq!(strip_heading("#### too deep"), None);
    }

    #[test]
    fn test_chunk_with_h2_heading_has_level_2() {
        let md = "## Section\n\nbody enough body enough body enough body enough body";
        let doc = parse(md);
        assert_eq!(doc.chunks.len(), 1);
        assert_eq!(doc.chunks[0].heading.as_deref(), Some("Section"));
        assert_eq!(doc.chunks[0].level, Some(2));
    }

    #[test]
    fn test_chunk_with_h3_heading_has_level_3() {
        let md = "### Sub\n\nbody enough body enough body enough body enough body";
        let doc = parse(md);
        assert_eq!(doc.chunks.len(), 1);
        assert_eq!(doc.chunks[0].level, Some(3));
    }

    #[test]
    fn test_chunk_no_heading_has_level_none() {
        let md = "leading prose without heading enough body to avoid 50-char merge";
        let doc = parse(md);
        assert_eq!(doc.chunks.len(), 1);
        assert!(doc.chunks[0].heading.is_none());
        assert!(doc.chunks[0].level.is_none());
    }

    #[test]
    fn test_50char_merge_preserves_first_heading_level() {
        let md = "\
## Big

short.

### Sub

larger body enough enough enough enough enough enough enough.";
        let doc = parse(md);
        let merged = doc.chunks.first().expect("expected at least one chunk");
        assert_eq!(merged.heading.as_deref(), Some("Big"));
        assert_eq!(merged.level, Some(2));
    }

    // brief 提供時の fixture では excludes を渡す手段がなく E-6 が検証不能だったため
    // (team-lead 承認済み逸脱 1): excludes 引数を追加し全呼び出しをこちらに統一する。
    fn parse_ctx(md: &str, title: &str, excludes: &[&str]) -> ParsedDocument {
        MarkdownParser.parse(md, &format!("{title}.md"), excludes)
    }

    #[test]
    fn test_context_simple_hierarchy() {
        // title は path_hint (filename) 由来。h2 > h3 のパンくずが付く。
        // (team-lead 承認済み逸脱 2: brief 原文の body は 50 字未満で 50-char merge に
        // 飲まれ chunks[1] が存在しなくなるため、意図・期待 context 文字列は変えず
        // body の文字数のみ 50 字以上に伸ばしている)
        let md = "## 検索パイプライン\n\nintro body enough enough enough enough enough enough.\n\n### RRF の実装\n\nbody enough enough enough enough enough enough enough.";
        let doc = parse_ctx(md, "設計ノート", &[]);
        // preamble なし: chunk[0] = h2, chunk[1] = h3
        assert_eq!(doc.chunks[0].heading.as_deref(), Some("検索パイプライン"));
        assert_eq!(
            doc.chunks[0].context.as_deref(),
            Some("設計ノート > 検索パイプライン")
        );
        assert_eq!(doc.chunks[1].heading.as_deref(), Some("RRF の実装"));
        assert_eq!(
            doc.chunks[1].context.as_deref(),
            Some("設計ノート > 検索パイプライン > RRF の実装")
        );
    }

    #[test]
    fn test_context_preamble_is_title_only() {
        // E-3: heading なし preamble chunk は title のみ
        let md = "leading prose without heading enough body to avoid the 50-char merge here.";
        let doc = parse_ctx(md, "mydoc", &[]);
        assert!(doc.chunks[0].heading.is_none());
        assert_eq!(doc.chunks[0].context.as_deref(), Some("mydoc"));
    }

    #[test]
    fn test_context_level_skip_h2_to_h4() {
        // E-7: level 飛び (h2 → h3 相当の deeper) — groove は h2/h3 のみ認識。
        // h2 の直後に h3 が来た後、別 h2 が来ると h3 は pop され祖先から消える。
        // (team-lead 承認済み逸脱 2: A/B の body を 50 字以上に伸ばし、A1 に merge
        // されず A・B がそれぞれ独立 chunk として残るようにしている)
        let md = "## A\n\nbody enough enough enough enough enough enough enough.\n\n### A1\n\nbody enough enough enough enough enough.\n\n## B\n\nbody enough enough enough enough enough enough enough.";
        let doc = parse_ctx(md, "T", &[]);
        let b = doc
            .chunks
            .iter()
            .find(|c| c.heading.as_deref() == Some("B"))
            .unwrap();
        // B は h2 なので祖先は空、context = "T > B" (A/A1 は入らない)
        assert_eq!(b.context.as_deref(), Some("T > B"));
    }

    #[test]
    fn test_context_excluded_heading_still_on_stack() {
        // E-6: 除外見出し配下の非除外 subsection は、除外見出しを祖先に持つ。
        // (groove の exclude は「次見出しまで」= subtree 除外ではない)
        let md = "## Secret\n\nsecret intro enough enough enough enough.\n\n### Detail\n\ndetail body enough enough enough enough.";
        let doc = parse_ctx(md, "T", &["Secret"]);
        // Secret 自身の chunk は出ない (excluded)。Detail (h3) は出る。
        let detail = doc
            .chunks
            .iter()
            .find(|c| c.heading.as_deref() == Some("Detail"))
            .unwrap();
        // ancestry に "Secret" が乗る (= 除外されても stack には積まれる、E-6 の本質)
        assert_eq!(detail.context.as_deref(), Some("T > Secret > Detail"));
        assert!(
            !doc.chunks
                .iter()
                .any(|c| c.heading.as_deref() == Some("Secret"))
        );
    }

    #[test]
    fn test_context_50char_merge_keeps_first_ancestry() {
        // E-9: 50-char merge 後 chunk は最初に出現した heading の ancestry を維持
        // Sub の content は 50 字未満に保つ (merge 条件 `content.len() < 50` を
        // 実際に踏ませるため。50 字以上だと merge 自体が起きず本テストの意図が
        // 検証できない = レビュー指摘の Medium 修正)。
        let md = "## Big\n\nshort.\n\n### Sub\n\nlarger body enough enough enough enough.";
        let doc = parse_ctx(md, "T", &[]);
        // merge が実際に発生し、chunk が1個に collapse していることを確認
        assert_eq!(doc.chunks.len(), 1);
        let merged = doc.chunks.first().unwrap();
        assert_eq!(merged.heading.as_deref(), Some("Big"));
        // merge 後も Big の ancestry (= title のみ) を保持
        assert_eq!(merged.context.as_deref(), Some("T > Big"));
    }
}
