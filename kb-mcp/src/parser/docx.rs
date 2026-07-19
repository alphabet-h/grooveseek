//! docx (`.docx`) parser. zip + quick-xml で `word/document.xml` を読む。
use std::io::Cursor;

use anyhow::{Result, anyhow};
use quick_xml::events::{BytesRef, BytesStart, Event};
use quick_xml::reader::Reader;

use super::{Chunk, ParsedDocument, Parser, single_text_chunk};

pub struct DocxParser;

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

    fn parse_bytes(
        &self,
        bytes: &[u8],
        path_hint: &str,
        exclude_headings: &[&str],
    ) -> Result<ParsedDocument> {
        let mut zip = zip::ZipArchive::new(Cursor::new(bytes)).map_err(|e| {
            anyhow!("{path_hint}: cannot open docx zip (corrupt or encrypted): {e}")
        })?;
        let doc_xml = super::ooxml::read_zip_entry(&mut zip, "word/document.xml")
            .ok_or_else(|| anyhow!("{path_hint}: word/document.xml missing"))?;
        let chunks = parse_document_xml(&doc_xml, exclude_headings);
        let frontmatter = super::ooxml::core_xml_frontmatter(&mut zip, path_hint);
        let raw_content = chunks
            .iter()
            .map(|c| c.content.as_str())
            .collect::<Vec<_>>()
            .join("\n\n");
        Ok(ParsedDocument {
            frontmatter,
            chunks,
            raw_content,
        })
    }
}

// ---------------------------------------------------------------------------
// word/document.xml → heading-hierarchy chunks
// ---------------------------------------------------------------------------

/// `word/document.xml` を段落 (`<w:p>`) 単位で読み、`<w:pStyle w:val="HeadingN">`
/// を見出し境界として Markdown 同様の階層チャンクに変換する。
///
/// 表 (`w:tbl`) 内のテキストも専用ハンドリングはしない: OOXML 上は
/// `w:tbl > w:tr > w:tc > w:p > w:r > w:t` と入れ子になっているだけなので、
/// 通常の `<w:p>` 境界処理だけで現在のセクション本文に自然に取り込まれる。
fn parse_document_xml(xml: &[u8], excludes: &[&str]) -> Vec<Chunk> {
    let mut reader = Reader::from_reader(xml);
    reader.config_mut().trim_text(false);
    let mut buf = Vec::new();

    // (heading, level, body) の raw セクション列を組む。見出し前本文は先頭の
    // heading=None セクションに溜まる。
    struct Section {
        heading: Option<String>,
        level: Option<u8>,
        body: String,
    }
    let mut sections: Vec<Section> = vec![Section {
        heading: None,
        level: None,
        body: String::new(),
    }];

    let mut para_style: Option<u8> = None; // HeadingN → level (2..=6)
    let mut para_text = String::new();
    let mut in_text = false;

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => match super::ooxml_local(e.name().as_ref()) {
                b"p" => {
                    para_style = None;
                    para_text.clear();
                }
                b"pStyle" => para_style = heading_level_from_attr(&e),
                b"t" => in_text = true,
                _ => {}
            },
            Ok(Event::Empty(e)) => {
                // `<w:pStyle w:val="Heading1"/>` は自己終端タグで来ることが多い。
                if super::ooxml_local(e.name().as_ref()) == b"pStyle" {
                    para_style = heading_level_from_attr(&e);
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
                para_text.push_str(&resolve_general_ref(&r));
            }
            Ok(Event::End(e)) => match super::ooxml_local(e.name().as_ref()) {
                b"t" => in_text = false,
                b"p" => {
                    let text = para_text.trim().to_string();
                    if let Some(level) = para_style {
                        // 見出し段落: 新セクション開始 (exclude 対象なら見出し
                        // なし = 本文なしセクションとして開始し、後続本文は
                        // 破棄される — MarkdownParser の chunk_body と同じ挙動)。
                        if excludes.iter().any(|ex| text.contains(ex)) {
                            sections.push(Section {
                                heading: None,
                                level: None,
                                body: String::new(),
                            });
                        } else {
                            sections.push(Section {
                                heading: Some(text),
                                level: Some(level),
                                body: String::new(),
                            });
                        }
                    } else if !text.is_empty() {
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
            Err(_) => break,
            _ => {}
        }
        buf.clear();
    }

    sections
        .into_iter()
        .filter(|s| s.heading.is_some() || !s.body.trim().is_empty())
        .enumerate()
        .map(|(i, s)| Chunk {
            index: i,
            heading: s.heading,
            level: s.level,
            content: s.body,
        })
        .collect()
}

/// `Event::GeneralRef` (`&ref;` / `&#NN;`) を解決した文字列に変換する。数値参照
/// (`&#38;` / `&#x26;`) と XML 定義済み 5 entity (`amp`/`lt`/`gt`/`apos`/`quot`)
/// を解決する。未知の named entity (docx では実質発生しない、カスタム DTD 前提)
/// は best-effort でリテラル `&name;` として残す。
fn resolve_general_ref(r: &BytesRef) -> String {
    if let Ok(Some(ch)) = r.resolve_char_ref() {
        return ch.to_string();
    }
    match r.decode() {
        Ok(name) => match quick_xml::escape::resolve_xml_entity(&name) {
            Some(s) => s.to_string(),
            None => format!("&{name};"),
        },
        Err(_) => String::new(),
    }
}

/// `<w:pStyle w:val="HeadingN">` の N から chunk level を返す (Heading1→2, ...,
/// Heading5→6、Heading6 以上は 6 に cap)。ロケール別名 `"heading 1"` (空白入り)
/// 等も許容 (小文字化 + prefix 除去後に trim するため)。`w:val` が見出しスタイル
/// でなければ (`Normal`/`Title` 等) None。
fn heading_level_from_attr(e: &BytesStart) -> Option<u8> {
    for attr in e.attributes().flatten() {
        if super::ooxml_local(attr.key.as_ref()) != b"val" {
            continue;
        }
        let val = String::from_utf8_lossy(&attr.value).to_ascii_lowercase();
        let Some(n) = val
            .strip_prefix("heading")
            .and_then(|rest| rest.trim().parse::<u8>().ok())
        else {
            continue;
        };
        return match n {
            1..=5 => Some(n + 1),
            6.. => Some(6),
            0 => None,
        };
    }
    None
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
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
            body.push_str(&format!(r#"<w:p>{pstyle}<w:r><w:t>{text}</w:t></w:r></w:p>"#));
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

    #[test]
    fn test_docx_parser_is_binary() {
        assert!(DocxParser.is_binary());
        assert_eq!(DocxParser.extension(), "docx");
    }

    // NOTE: skeleton 時点の `not_yet_implemented` 固定文言 assert は、本 task で
    // parse_bytes を実本実装したため意味が失われた。xlsx (Task 3.3) の前例に
    // 倣い、garbage 入力が real error path (zip open 失敗) で panic せず Err に
    // なることを検証するテストに更新する (controller 事前承認済み)。
    #[test]
    fn test_docx_parse_bytes_garbage_is_err() {
        let err = DocxParser
            .parse_bytes(b"not a real docx", "x.docx", &[])
            .expect_err("garbage bytes must be Err");
        assert!(err.to_string().contains("cannot open docx zip"));
    }

    #[test]
    fn test_docx_parse_fallback_wraps_raw_text() {
        let doc = DocxParser.parse("hello world content here", "x.docx", &[]);
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
        let doc = DocxParser.parse_bytes(&bytes, "docs/a.docx", &[]).unwrap();
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
            (None, "前書き これは十分な長さの前書きですよ十分な長さの前書き"),
            (Some("Heading1"), "章1"),
            (None, "本文 これは十分な長さの本文ですよ十分な長さの本文ですよ"),
        ]);
        let doc = DocxParser.parse_bytes(&bytes, "a.docx", &[]).unwrap();
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
        let err = DocxParser
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
        let doc = DocxParser.parse_bytes(&bytes, "e.docx", &[]).unwrap();
        assert!(
            doc.chunks[0].content.contains("A&B"),
            "entity reference must resolve, got: {:?}",
            doc.chunks[0].content
        );
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
        let doc = DocxParser.parse_bytes(&bytes, "t.docx", &[]).unwrap();
        assert_eq!(doc.chunks.len(), 1);
        assert_eq!(doc.chunks[0].heading.as_deref(), Some("章1"));
        assert!(doc.chunks[0].content.contains("表内セル"));
    }
}
