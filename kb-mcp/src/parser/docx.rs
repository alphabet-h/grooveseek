//! docx (`.docx`) parser. zip + quick-xml で `word/document.xml` を読む。
use std::io::Cursor;

use anyhow::{Result, anyhow};
use quick_xml::events::{BytesStart, Event};
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
        // 文書単位の累積展開済みバイト数。word/document.xml + docProps/core.xml
        // の両読み出しで共有し、累積が cap を超えたら Err にする (codex P2,
        // PR #70 round 2 zip-bomb hardening: 個々のエントリが cap 未満でも
        // 積算で無制限に膨らむのを防ぐ)。
        let mut budget: u64 = 0;
        let doc_xml = super::ooxml::read_zip_entry(&mut zip, "word/document.xml", &mut budget)?
            .ok_or_else(|| anyhow!("{path_hint}: word/document.xml missing"))?;
        // frontmatter を先に取得し、context の title に使う (取得順を入れ替え)。
        let frontmatter = super::ooxml::core_xml_frontmatter(&mut zip, path_hint, &mut budget)?;
        let chunks = parse_document_xml(&doc_xml, exclude_headings, frontmatter.title.as_deref());
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
fn parse_document_xml(xml: &[u8], excludes: &[&str], title: Option<&str>) -> Vec<Chunk> {
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

    let mut para_style: Option<u8> = None; // HeadingN → level (2..=6)
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
            }
        })
        .collect()
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
        let doc = DocxParser
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
        let doc = DocxParser.parse_bytes(&bytes, "t.docx", &[]).unwrap();
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
        let doc = DocxParser.parse_bytes(&bytes, "docs/a.docx", &[]).unwrap();
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
        let doc = DocxParser.parse_bytes(&bytes, "a.docx", &[]).unwrap();
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
        let doc = DocxParser.parse_bytes(&bytes, "docs/a.docx", &[]).unwrap();
        assert_eq!(doc.chunks[0].heading.as_deref(), Some("章1"));
        assert_eq!(doc.chunks[0].level, Some(2));
        assert_eq!(doc.chunks[1].heading.as_deref(), Some("小節1.1.1"));
        assert_eq!(doc.chunks[1].level, Some(4));
        assert_eq!(
            doc.chunks[1].context.as_deref(),
            Some("a > 章1 > 小節1.1.1")
        );
    }
}
