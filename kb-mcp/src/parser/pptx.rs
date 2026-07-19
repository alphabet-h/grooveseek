//! pptx (`.pptx`) parser. zip + quick-xml でスライド単位に `ppt/slides/slideN.xml`
//! を読む。
use std::io::Cursor;

use anyhow::{Result, anyhow};
use quick_xml::events::Event;
use quick_xml::reader::Reader;

use super::{Chunk, ParsedDocument, Parser, single_text_chunk};

pub struct PptxParser;

impl Parser for PptxParser {
    fn extension(&self) -> &'static str {
        "pptx"
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
        _exclude_headings: &[&str],
    ) -> Result<ParsedDocument> {
        parse_bytes_impl(bytes, path_hint)
    }
}

// ---------------------------------------------------------------------------
// ppt/slides/slideN.xml (+ ppt/notesSlides/notesSlideN.xml) → slide-wise chunks
// ---------------------------------------------------------------------------

fn parse_bytes_impl(bytes: &[u8], path_hint: &str) -> Result<ParsedDocument> {
    let mut zip = zip::ZipArchive::new(Cursor::new(bytes))
        .map_err(|e| anyhow!("{path_hint}: cannot open pptx zip (corrupt or encrypted): {e}"))?;

    // slide エントリを番号順に集める (zip 内順序非依存)。
    let mut slide_nums: Vec<usize> = zip
        .file_names()
        .filter_map(|n| slide_number(n, "ppt/slides/slide", ".xml"))
        .collect();
    slide_nums.sort_unstable();

    let mut chunks = Vec::new();
    for n in slide_nums {
        let slide_xml =
            match super::ooxml::read_zip_entry(&mut zip, &format!("ppt/slides/slide{n}.xml")) {
                Some(b) => b,
                None => continue,
            };
        let (title, body) = parse_slide_xml(&slide_xml);
        let mut content = body;
        // notes: 同番号 heuristic (spec §4.5 で許可。rels 解決はしない)。
        if let Some(notes_xml) = super::ooxml::read_zip_entry(
            &mut zip,
            &format!("ppt/notesSlides/notesSlide{n}.xml"),
        ) {
            let notes_text = collect_a_t(&notes_xml);
            if !notes_text.trim().is_empty() {
                content.push_str("\n\n[notes]\n");
                content.push_str(notes_text.trim());
            }
        }
        if content.trim().is_empty() && title.is_none() {
            continue;
        }
        let heading = match &title {
            Some(t) if !t.trim().is_empty() => format!("Slide {n}: {}", t.trim()),
            _ => format!("Slide {n}"),
        };
        chunks.push(Chunk {
            index: chunks.len(),
            heading: Some(heading),
            level: Some(2),
            content: content.trim().to_string(),
        });
    }

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

/// `ppt/slides/slide12.xml` から prefix/suffix を除いた整数 12 を取る。
fn slide_number(name: &str, prefix: &str, suffix: &str) -> Option<usize> {
    let mid = name.strip_prefix(prefix)?.strip_suffix(suffix)?;
    mid.parse::<usize>().ok()
}

/// スライド XML から (title, body) を返す。
///
/// - title: 最初の title placeholder (`<p:ph type="title"/>` または
///   `<p:ph type="ctrTitle"/>` — ECMA-376 では表紙スライド (title slide
///   layout) の title placeholder は `ctrTitle` になる) を含む `<p:sp>` の
///   a:t 連結テキスト。
/// - body: title placeholder 配下を除く全ての `<a:p>` (段落) の a:t 連結
///   テキストを、段落単位の改行区切りで連結したもの。`<p:sp>` 内の通常
///   テキストだけでなく `<p:graphicFrame><a:tbl>` (表) セル内の a:t も同じ
///   `<a:p>` 構造 (`a:tc > a:txBody > a:p`) を持つため区別なく拾う。
///
///   (旧実装は `<p:sp>` の Start/End でのみ本文バッファを flush していたため、
///   sp の外側にある表のテキストが「次の sp の Start で握り潰される」/
///   「最後の sp の後ろだと一度も flush されない」のいずれかで silent drop
///   されるバグがあった。`<a:p>` 単位の flush に変えることで解消している。)
fn parse_slide_xml(xml: &[u8]) -> (Option<String>, String) {
    let mut reader = Reader::from_reader(xml);
    reader.config_mut().trim_text(false);
    let mut buf = Vec::new();

    let mut body = String::new();
    let mut title: Option<String> = None;

    // `<p:sp>` スコープ状態。title placeholder 判定にのみ使う (本文の
    // flush 単位はもはや sp ではなく `<a:p>`)。
    let mut in_sp = false;
    let mut sp_is_title = false;

    // `<a:p>` (段落) スコープ状態。sp 内外を問わず段落単位でテキストを蓄積し、
    // `</a:p>` で title / body への振り分けを行う。
    let mut para_text = String::new();
    let mut in_text = false;

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => match super::ooxml_local(e.name().as_ref()) {
                b"sp" => {
                    in_sp = true;
                    sp_is_title = false;
                }
                b"ph" => {
                    if in_sp && ph_type_is_title(&e) {
                        sp_is_title = true;
                    }
                }
                b"p" => para_text.clear(),
                b"t" => in_text = true,
                _ => {}
            },
            Ok(Event::Empty(e)) => {
                if super::ooxml_local(e.name().as_ref()) == b"ph" && in_sp && ph_type_is_title(&e)
                {
                    sp_is_title = true;
                }
            }
            // quick-xml 0.41 は `BytesText::unescape()` を廃止し、encoding decode
            // (`decode()`) と entity unescape (`escape::unescape()`) を分離した
            // (docx.rs Task 3.4 前例踏襲)。
            Ok(Event::Text(t)) if in_text => {
                para_text.push_str(&decode_text(&t));
            }
            // quick-xml 0.38+ は entity 参照 (`&amp;` 等) を `Event::Text` に含めず
            // `Event::GeneralRef` として別 event で届ける。ここを処理しないと
            // `<a:t>A&amp;B</a:t>` の "&" が欠落する (docx.rs 同様の必須処理)。
            Ok(Event::GeneralRef(r)) if in_text => {
                para_text.push_str(&super::ooxml::resolve_general_ref(&r));
            }
            Ok(Event::End(e)) => match super::ooxml_local(e.name().as_ref()) {
                b"t" => in_text = false,
                b"p" => {
                    let trimmed = para_text.trim();
                    if !trimmed.is_empty() {
                        if in_sp && sp_is_title {
                            if title.is_none() {
                                title = Some(trimmed.to_string());
                            }
                        } else {
                            if !body.is_empty() {
                                body.push('\n');
                            }
                            body.push_str(trimmed);
                        }
                    }
                }
                b"sp" => in_sp = false,
                _ => {}
            },
            Ok(Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
        buf.clear();
    }
    (title, body)
}

/// `<p:ph>` の `type` 属性が title placeholder (`"title"` / `"ctrTitle"`) か
/// どうか。`ctrTitle` は ECMA-376 で表紙スライド (title slide layout) の
/// title placeholder に使われる値。
fn ph_type_is_title(e: &quick_xml::events::BytesStart) -> bool {
    e.attributes().flatten().any(|attr| {
        super::ooxml_local(attr.key.as_ref()) == b"type"
            && matches!(attr.value.as_ref(), b"title" | b"ctrTitle")
    })
}

/// XML から全 `a:t` テキストを連結する (notes 用の簡易版。要素間はスペース区切り)。
fn collect_a_t(xml: &[u8]) -> String {
    let mut reader = Reader::from_reader(xml);
    reader.config_mut().trim_text(false);
    let mut buf = Vec::new();
    let mut out = String::new();
    let mut in_text = false;
    let mut run_text = String::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) if super::ooxml_local(e.name().as_ref()) == b"t" => {
                in_text = true;
                run_text.clear();
            }
            Ok(Event::Text(t)) if in_text => {
                run_text.push_str(&decode_text(&t));
            }
            Ok(Event::GeneralRef(r)) if in_text => {
                run_text.push_str(&super::ooxml::resolve_general_ref(&r));
            }
            Ok(Event::End(e)) if super::ooxml_local(e.name().as_ref()) == b"t" => {
                in_text = false;
                out.push_str(&run_text);
                out.push(' ');
            }
            Ok(Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
        buf.clear();
    }
    out
}

/// `<a:t>` テキストの entity-unescape 済み文字列を取り出す (docx.rs と同じ
/// `decode()` → `escape::unescape()` の 2 段パターン。parse_slide_xml /
/// collect_a_t の 2 箇所で使うため pptx.rs 内 helper として共通化する)。
fn decode_text(t: &quick_xml::events::BytesText) -> String {
    let decoded = t.decode().unwrap_or_default();
    quick_xml::escape::unescape(&decoded)
        .map(|c| c.into_owned())
        .unwrap_or_else(|_| decoded.into_owned())
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use zip::write::SimpleFileOptions;

    /// slides = [(title_opt, body, notes_opt)]。
    fn make_minimal_pptx(slides: &[(Option<&str>, &str, Option<&str>)]) -> Vec<u8> {
        let mut buf = Vec::new();
        {
            let mut zip = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
            let opt = SimpleFileOptions::default();
            zip.start_file("[Content_Types].xml", opt).unwrap();
            zip.write_all(br#"<?xml version="1.0"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"/>"#).unwrap();
            for (i, (title, body, notes)) in slides.iter().enumerate() {
                let n = i + 1;
                let title_shape = match title {
                    Some(t) => format!(r#"<p:sp><p:nvSpPr><p:nvPr><p:ph type="title"/></p:nvPr></p:nvSpPr><p:txBody><a:p><a:r><a:t>{t}</a:t></a:r></a:p></p:txBody></p:sp>"#),
                    None => String::new(),
                };
                let slide_xml = format!(
                    r#"<?xml version="1.0"?><p:sld xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main" xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"><p:cSld><p:spTree>{title_shape}<p:sp><p:txBody><a:p><a:r><a:t>{body}</a:t></a:r></a:p></p:txBody></p:sp></p:spTree></p:cSld></p:sld>"#
                );
                zip.start_file(format!("ppt/slides/slide{n}.xml"), opt)
                    .unwrap();
                zip.write_all(slide_xml.as_bytes()).unwrap();
                if let Some(note) = notes {
                    let notes_xml = format!(
                        r#"<?xml version="1.0"?><p:notes xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main" xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"><p:cSld><p:spTree><p:sp><p:txBody><a:p><a:r><a:t>{note}</a:t></a:r></a:p></p:txBody></p:sp></p:spTree></p:cSld></p:notes>"#
                    );
                    zip.start_file(format!("ppt/notesSlides/notesSlide{n}.xml"), opt)
                        .unwrap();
                    zip.write_all(notes_xml.as_bytes()).unwrap();
                }
            }
            zip.finish().unwrap();
        }
        buf
    }

    #[test]
    fn test_pptx_parser_is_binary() {
        assert!(PptxParser.is_binary());
        assert_eq!(PptxParser.extension(), "pptx");
    }

    // NOTE: skeleton 時点の `not_yet_implemented` 固定文言 assert は、本 task で
    // parse_bytes を実本実装したため意味が失われた。docx (Task 3.4) の前例に
    // 倣い、garbage 入力が real error path (zip open 失敗) で panic せず Err に
    // なることを検証するテストに更新する (controller 事前承認済み)。
    #[test]
    fn test_pptx_parse_bytes_garbage_is_err() {
        let err = PptxParser
            .parse_bytes(b"not a real pptx", "x.pptx", &[])
            .expect_err("garbage bytes must be Err");
        assert!(err.to_string().contains("cannot open pptx zip"));
    }

    #[test]
    fn test_pptx_parse_fallback_wraps_raw_text() {
        let doc = PptxParser.parse("hello world content here", "x.pptx", &[]);
        assert_eq!(doc.chunks.len(), 1);
        assert!(doc.chunks[0].content.contains("hello world"));
    }

    #[test]
    fn test_pptx_slide_chunks_with_title_and_notes() {
        let bytes = make_minimal_pptx(&[
            (Some("概要"), "本文スライド1", Some("発表ノート1")),
            (None, "本文スライド2", None),
        ]);
        let doc = PptxParser.parse_bytes(&bytes, "docs/deck.pptx", &[]).unwrap();
        assert_eq!(doc.chunks.len(), 2);
        assert_eq!(doc.chunks[0].heading.as_deref(), Some("Slide 1: 概要"));
        assert_eq!(doc.chunks[0].level, Some(2));
        assert!(doc.chunks[0].content.contains("本文スライド1"));
        assert!(doc.chunks[0].content.contains("[notes]"));
        assert!(doc.chunks[0].content.contains("発表ノート1"));
        // title placeholder 無しは "Slide {n}" fallback。
        assert_eq!(doc.chunks[1].heading.as_deref(), Some("Slide 2"));
    }

    #[test]
    fn test_pptx_slides_sorted_numerically_not_zip_order() {
        // slide10 が slide2 より前に zip へ書かれても番号順に並ぶこと。
        // make_minimal_pptx は 1..N 順に書くため、ここでは 11 枚作って
        // chunk[9] が "Slide 10" になることで数値ソートを確認。
        let slides: Vec<(Option<&str>, &str, Option<&str>)> =
            (0..11).map(|_| (None, "body", None)).collect();
        let bytes = make_minimal_pptx(&slides);
        let doc = PptxParser.parse_bytes(&bytes, "d.pptx", &[]).unwrap();
        assert_eq!(doc.chunks.len(), 11);
        assert_eq!(doc.chunks[9].heading.as_deref(), Some("Slide 10"));
        assert_eq!(doc.chunks[10].heading.as_deref(), Some("Slide 11"));
    }

    /// 単一スライドの pptx zip を組み立てる (`slide_xml` = `<p:sld>...</p:sld>`
    /// 全体)。ctrTitle / 表 (graphicFrame) 等、`make_minimal_pptx` のテンプレート
    /// では組めない XML 構造を検証するテストで使う。
    fn make_pptx_single_slide(slide_xml: &str) -> Vec<u8> {
        let mut buf = Vec::new();
        {
            let mut zip = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
            let opt = SimpleFileOptions::default();
            zip.start_file("[Content_Types].xml", opt).unwrap();
            zip.write_all(br#"<?xml version="1.0"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"/>"#).unwrap();
            zip.start_file("ppt/slides/slide1.xml", opt).unwrap();
            zip.write_all(slide_xml.as_bytes()).unwrap();
            zip.finish().unwrap();
        }
        buf
    }

    #[test]
    fn test_pptx_ctr_title_placeholder_is_picked_up_as_title() {
        // ECMA-376: 表紙スライド (title slide layout) の title placeholder は
        // `type="ctrTitle"` になる (通常スライドの `type="title"` とは別値)。
        let slide_xml = r#"<?xml version="1.0"?><p:sld xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main" xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"><p:cSld><p:spTree><p:sp><p:nvSpPr><p:nvPr><p:ph type="ctrTitle"/></p:nvPr></p:nvSpPr><p:txBody><a:p><a:r><a:t>表紙タイトル</a:t></a:r></a:p></p:txBody></p:sp></p:spTree></p:cSld></p:sld>"#;
        let bytes = make_pptx_single_slide(slide_xml);
        let doc = PptxParser.parse_bytes(&bytes, "cover.pptx", &[]).unwrap();
        assert_eq!(doc.chunks.len(), 1);
        assert_eq!(doc.chunks[0].heading.as_deref(), Some("Slide 1: 表紙タイトル"));
    }

    #[test]
    fn test_pptx_table_text_included_in_body() {
        // `<p:graphicFrame><a:tbl>` (表) 内の a:t は `<p:sp>` の外側にある。
        // sp 単位でのみ本文バッファを flush する実装だと、表セルのテキストが
        // 「次の sp の Start で握り潰される」/「最後の sp の後ろだと一度も
        // flush されない」のいずれかで silent drop される (回帰テスト)。
        let slide_xml = r#"<?xml version="1.0"?><p:sld xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main" xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"><p:cSld><p:spTree><p:sp><p:txBody><a:p><a:r><a:t>本文</a:t></a:r></a:p></p:txBody></p:sp><p:graphicFrame><a:graphic><a:graphicData><a:tbl><a:tr><a:tc><a:txBody><a:p><a:r><a:t>セルA</a:t></a:r></a:p></a:txBody></a:tc></a:tr><a:tr><a:tc><a:txBody><a:p><a:r><a:t>セルB</a:t></a:r></a:p></a:txBody></a:tc></a:tr></a:tbl></a:graphicData></a:graphic></p:graphicFrame></p:spTree></p:cSld></p:sld>"#;
        let bytes = make_pptx_single_slide(slide_xml);
        let doc = PptxParser.parse_bytes(&bytes, "table.pptx", &[]).unwrap();
        assert_eq!(doc.chunks.len(), 1);
        assert!(
            doc.chunks[0].content.contains("本文"),
            "shape body text must survive, got: {:?}",
            doc.chunks[0].content
        );
        assert!(
            doc.chunks[0].content.contains("セルA"),
            "table cell text must not be dropped, got: {:?}",
            doc.chunks[0].content
        );
        assert!(
            doc.chunks[0].content.contains("セルB"),
            "table cell text must not be dropped, got: {:?}",
            doc.chunks[0].content
        );
    }

    #[test]
    fn test_pptx_entity_reference_in_text_is_preserved() {
        // quick-xml 0.38+ は entity 参照を Text event に含めず `Event::GeneralRef`
        // として別 event で届ける (docx.rs 同様の回帰テスト)。本文・notes 双方の
        // 経路 (parse_slide_xml / collect_a_t) で "&" が欠落しないことを確認する。
        let bytes = make_minimal_pptx(&[(None, "A&amp;B", Some("N&amp;M"))]);
        let doc = PptxParser.parse_bytes(&bytes, "e.pptx", &[]).unwrap();
        assert!(
            doc.chunks[0].content.contains("A&B"),
            "slide body entity reference must resolve, got: {:?}",
            doc.chunks[0].content
        );
        assert!(
            doc.chunks[0].content.contains("N&M"),
            "notes entity reference must resolve, got: {:?}",
            doc.chunks[0].content
        );
    }
}
