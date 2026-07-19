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
        // notes: `ppt/slides/_rels/slide{n}.xml.rels` の notesSlide
        // relationship を解決する (dry-run (Task 3.7) で同番号 heuristic の
        // 誤帰属が実証されたため廃止。rels が無い / notesSlide relationship
        // が無い slide は notes なしとし、フォールバック heuristic はしない
        // — 誤帰属ゼロを優先する)。
        let notes_path =
            super::ooxml::read_zip_entry(&mut zip, &format!("ppt/slides/_rels/slide{n}.xml.rels"))
                .and_then(|rels_xml| resolve_notes_path(&rels_xml));
        if let Some(path) = notes_path
            && let Some(notes_xml) = super::ooxml::read_zip_entry(&mut zip, &path)
        {
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

/// `ppt/slides/_rels/slideN.xml.rels` から notesSlide relationship
/// (`Type` が `.../relationships/notesSlide` で終わる `<Relationship>`) の
/// `Target` を解決し、zip 内の絶対パス (例: `ppt/notesSlides/notesSlide1.xml`)
/// を返す。該当する relationship が無ければ None (= 呼び出し側は notes なし
/// として扱う。同番号 heuristic へのフォールバックはしない)。
fn resolve_notes_path(rels_xml: &[u8]) -> Option<String> {
    let mut reader = Reader::from_reader(rels_xml);
    reader.config_mut().trim_text(true);
    let mut buf = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => {
                if super::ooxml_local(e.name().as_ref()) != b"Relationship" {
                    continue;
                }
                let mut rel_type: Option<String> = None;
                let mut target: Option<String> = None;
                for attr in e.attributes().flatten() {
                    match super::ooxml_local(attr.key.as_ref()) {
                        b"Type" => {
                            rel_type = Some(String::from_utf8_lossy(&attr.value).into_owned());
                        }
                        b"Target" => {
                            target = Some(String::from_utf8_lossy(&attr.value).into_owned());
                        }
                        _ => {}
                    }
                }
                if let (Some(rel_type), Some(target)) = (rel_type, target)
                    && rel_type.ends_with("/notesSlide")
                {
                    return Some(resolve_relative_target("ppt/slides", &target));
                }
            }
            Ok(Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
        buf.clear();
    }
    None
}

/// `base_dir` (例: `"ppt/slides"`、末尾スラッシュ無し) を基準に OOXML の
/// relationship `Target` (例: `"../notesSlides/notesSlide1.xml"`) を解決し、
/// zip 内の絶対パス (例: `"ppt/notesSlides/notesSlide1.xml"`) を返す。
/// `Target` が `/` 始まりの絶対パス (`"/ppt/..."`) の場合は先頭の `/` を
/// 除いてそのまま使う (OPC 仕様上のパッケージルート基準パス)。
fn resolve_relative_target(base_dir: &str, target: &str) -> String {
    if let Some(abs) = target.strip_prefix('/') {
        return abs.to_string();
    }
    let mut stack: Vec<&str> = base_dir.split('/').filter(|s| !s.is_empty()).collect();
    for seg in target.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                stack.pop();
            }
            s => stack.push(s),
        }
    }
    stack.join("/")
}

/// スライド XML から (title, body) を返す。
///
/// - title: 最初の title placeholder (`<p:ph type="title"/>` または
///   `<p:ph type="ctrTitle"/>` — ECMA-376 では表紙スライド (title slide
///   layout) の title placeholder は `ctrTitle` になる) を含む `<p:sp>` の
///   a:t 連結テキスト。
/// - body: **全ての** `<a:p>` (段落) の a:t 連結テキストを、段落単位の
///   改行区切りで連結したもの (title placeholder 配下も含む — indexer
///   (`indexer.rs`) が embed するのは `Chunk::content` のみで `heading` は
///   embed 対象外なので、title 段落を body から除外すると title-only
///   スライドが semantic search で拾えなくなる。よって title/body は
///   排他ではなく、title 段落は heading にも content にも両方現れる)。
///   `<p:sp>` 内の通常テキストだけでなく `<p:graphicFrame><a:tbl>` (表)
///   セル内の a:t も同じ `<a:p>` 構造 (`a:tc > a:txBody > a:p`) を持つため
///   区別なく拾う。
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
                if super::ooxml_local(e.name().as_ref()) == b"ph" && in_sp && ph_type_is_title(&e) {
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
                        // title 判定と body 追加は排他にしない: title 段落の
                        // テキストも embed 対象の content (body) に残す (上の
                        // doc comment 参照)。
                        if in_sp && sp_is_title && title.is_none() {
                            title = Some(trimmed.to_string());
                        }
                        if !body.is_empty() {
                            body.push('\n');
                        }
                        body.push_str(trimmed);
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

    /// slides = [(title_opt, body, notes_opt)]。notes が Some の slide には
    /// `ppt/slides/_rels/slide{n}.xml.rels` に notesSlide relationship
    /// (Type=`.../notesSlide`, Target=`../notesSlides/notesSlide{n}.xml`) も
    /// 書く (同番号 heuristic 廃止後は rels 経由でしか notes を解決しない
    /// ため、fixture 側で明示的に対応させる必要がある)。
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
                    Some(t) => format!(
                        r#"<p:sp><p:nvSpPr><p:nvPr><p:ph type="title"/></p:nvPr></p:nvSpPr><p:txBody><a:p><a:r><a:t>{t}</a:t></a:r></a:p></p:txBody></p:sp>"#
                    ),
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
                    let rels_xml = format!(
                        r#"<?xml version="1.0"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/notesSlide" Target="../notesSlides/notesSlide{n}.xml"/></Relationships>"#
                    );
                    zip.start_file(format!("ppt/slides/_rels/slide{n}.xml.rels"), opt)
                        .unwrap();
                    zip.write_all(rels_xml.as_bytes()).unwrap();
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
        let doc = PptxParser
            .parse_bytes(&bytes, "docs/deck.pptx", &[])
            .unwrap();
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
        assert_eq!(
            doc.chunks[0].heading.as_deref(),
            Some("Slide 1: 表紙タイトル")
        );
        // title 段落のテキストは body (= embed 対象の content) にも残す。
        // indexer は chunk.content のみを embed し heading は embed 対象外
        // (indexer.rs::embed_texts が `c.content` だけ収集する) なので、
        // title を content から排他すると title-only スライドが検索に
        // 引っかからなくなる (semantic recall 劣化の回帰 guard)。
        assert!(
            doc.chunks[0].content.contains("表紙タイトル"),
            "title text must also survive in content for embedding recall, got: {:?}",
            doc.chunks[0].content
        );
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
    fn test_pptx_notes_resolved_via_rels_not_same_number() {
        // dry-run (Task 3.7) で実証された誤帰属回帰テスト: 同番号 heuristic の
        // ままだと、notesSlide1.xml が (実際は rels 上 slide4 からのみ参照
        // されているにもかかわらず) 同番号の slide1 に付いてしまう。
        // rels 解決に切り替えたことで、notesSlide1.xml が実際に参照されて
        // いる slide4 の chunk にだけ notes が付くことを検証する。
        let mut buf = Vec::new();
        {
            let mut zip = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
            let opt = SimpleFileOptions::default();
            zip.start_file("[Content_Types].xml", opt).unwrap();
            zip.write_all(br#"<?xml version="1.0"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"/>"#).unwrap();

            for n in 1..=4 {
                let slide_xml = format!(
                    r#"<?xml version="1.0"?><p:sld xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main" xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"><p:cSld><p:spTree><p:sp><p:txBody><a:p><a:r><a:t>本文スライド{n}</a:t></a:r></a:p></p:txBody></p:sp></p:spTree></p:cSld></p:sld>"#
                );
                zip.start_file(format!("ppt/slides/slide{n}.xml"), opt)
                    .unwrap();
                zip.write_all(slide_xml.as_bytes()).unwrap();
            }

            // slide1: rels はあるが notesSlide relationship を持たない
            // (slideLayout relationship のみ = 「rels はあるが notes 対応は
            // 無い」ケースも同時に検証する)。
            zip.start_file("ppt/slides/_rels/slide1.xml.rels", opt)
                .unwrap();
            zip.write_all(br#"<?xml version="1.0"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slideLayout" Target="../slideLayouts/slideLayout1.xml"/></Relationships>"#).unwrap();

            // slide2/slide3: rels ファイル自体が無い。

            // slide4 の rels だけが notesSlide1.xml を参照する
            // (= 同番号 heuristic なら slide1 の notes になるはずだが、
            // 実際の所有者は slide4)。
            zip.start_file("ppt/slides/_rels/slide4.xml.rels", opt)
                .unwrap();
            zip.write_all(br#"<?xml version="1.0"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/notesSlide" Target="../notesSlides/notesSlide1.xml"/></Relationships>"#).unwrap();

            zip.start_file("ppt/notesSlides/notesSlide1.xml", opt)
                .unwrap();
            // 非 ASCII (日本語) を含むため raw byte string (`br#"..."#`) は使えない
            // (raw byte string literal は ASCII 限定): `r#"..."#.as_bytes()` で代用。
            zip.write_all(r#"<?xml version="1.0"?><p:notes xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main" xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"><p:cSld><p:spTree><p:sp><p:txBody><a:p><a:r><a:t>発表者ノート</a:t></a:r></a:p></p:txBody></p:sp></p:spTree></p:cSld></p:notes>"#.as_bytes()).unwrap();

            zip.finish().unwrap();
        }

        let doc = PptxParser.parse_bytes(&buf, "misattr.pptx", &[]).unwrap();
        assert_eq!(doc.chunks.len(), 4);
        assert!(
            !doc.chunks[0].content.contains("発表者ノート"),
            "slide1 (rels has no notesSlide relationship) must NOT get slide4's \
             notes via same-number heuristic, got: {:?}",
            doc.chunks[0].content
        );
        assert!(
            !doc.chunks[1].content.contains("発表者ノート"),
            "slide2 (no rels file at all) must not get notes, got: {:?}",
            doc.chunks[1].content
        );
        assert!(
            !doc.chunks[2].content.contains("発表者ノート"),
            "slide3 (no rels file at all) must not get notes, got: {:?}",
            doc.chunks[2].content
        );
        assert!(
            doc.chunks[3].content.contains("[notes]"),
            "slide4 (rels references notesSlide1.xml) must get the notes, got: {:?}",
            doc.chunks[3].content
        );
        assert!(
            doc.chunks[3].content.contains("発表者ノート"),
            "slide4 (rels references notesSlide1.xml) must get the notes, got: {:?}",
            doc.chunks[3].content
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
