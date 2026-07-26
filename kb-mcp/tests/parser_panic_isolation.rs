//! Adversarial-input battery for the binary parsers (full-audit 2026-07-26 AU-21).
//!
//! `Parser::parse_bytes` must **never unwind into its caller**: `indexer.rs`
//! indexes files sequentially, so a panic escaping one file aborts the whole
//! `kb-mcp index` run instead of skipping that single file. The mechanism is
//! unit-tested in `parser::panic_guard` / `parser::tests` with a fake parser
//! that always panics; this file is the other half — real crafted documents
//! fed through the **public** entry point, so a future refactor that bypasses
//! the guard (e.g. a parser overriding `parse_bytes` instead of
//! `parse_bytes_inner`) is caught with realistic input.
//!
//! Every case asserts the same contract: `parse_bytes` returns — `Ok` or
//! `Err`, both are acceptable outcomes for a malformed document. A panic
//! escaping any of these calls fails the test (libtest reports the unwind),
//! which is exactly the regression we are guarding against.
//!
//! Note on why the corpus alone is not the test: whether a given crafted file
//! actually panics inside calamine / zip / quick-xml depends on the crate
//! version and on the build profile (integer overflow panics only with
//! `debug-assertions`). A regression test may not depend on that, so the
//! isolation guarantee itself is pinned by the fake-parser unit tests, and
//! this file keeps an adversarial corpus running through the real code paths.

use std::io::Write;

use kb_mcp::parser::{DocxParser, Parser, ParserExt, PdfParser, PptxParser, XlsParser, XlsxParser};
use zip::write::SimpleFileOptions;

/// Build a zip from `(name, contents)` pairs. Used to assemble deliberately
/// malformed OOXML packages.
fn zip_of(parts: &[(&str, &[u8])]) -> Vec<u8> {
    let mut buf = Vec::new();
    {
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
        let opt = SimpleFileOptions::default();
        for (name, body) in parts {
            zip.start_file(*name, opt).unwrap();
            zip.write_all(body).unwrap();
        }
        zip.finish().unwrap();
    }
    buf
}

/// Feed `bytes` to `parser`. The contract under test is that control returns
/// here at all — `Ok` and `Err` are both acceptable outcomes for a malformed
/// document, so each branch only checks that the returned value is coherent.
fn must_not_unwind(parser: &dyn Parser, bytes: &[u8], case: &str) {
    match parser.parse_bytes(bytes, &format!("docs/{case}"), &[]) {
        Ok(doc) => {
            for (i, chunk) in doc.chunks.iter().enumerate() {
                assert_eq!(
                    chunk.index, i,
                    "{case}: chunk indices must stay sequential even for salvaged input"
                );
            }
        }
        Err(e) => assert!(
            !e.to_string().is_empty(),
            "{case}: a rejection must carry a message for the indexer's skip log"
        ),
    }
}

const XLSX_CONTENT_TYPES: &[u8] = br#"<?xml version="1.0"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/><Override PartName="/xl/worksheets/sheet1.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/></Types>"#;

const ROOT_RELS: &[u8] = br#"<?xml version="1.0"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/></Relationships>"#;

const XLSX_WORKBOOK: &[u8] = br#"<?xml version="1.0"?><workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><sheets><sheet name="S1" sheetId="1" r:id="rId1"/></sheets></workbook>"#;

const XLSX_WORKBOOK_RELS: &[u8] = br#"<?xml version="1.0"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/></Relationships>"#;

/// Assemble an xlsx whose `sheet1.xml` body is caller-supplied.
fn xlsx_with_sheet(sheet_xml: &[u8]) -> Vec<u8> {
    zip_of(&[
        ("[Content_Types].xml", XLSX_CONTENT_TYPES),
        ("_rels/.rels", ROOT_RELS),
        ("xl/workbook.xml", XLSX_WORKBOOK),
        ("xl/_rels/workbook.xml.rels", XLSX_WORKBOOK_RELS),
        ("xl/worksheets/sheet1.xml", sheet_xml),
    ])
}

#[test]
fn test_xlsx_adversarial_inputs_do_not_unwind() {
    // 1. Inverted dimension: `get_dimension` computes `end - start` on u32,
    //    which panics with debug assertions and silently wraps without them.
    let inverted_dimension = xlsx_with_sheet(
        br#"<?xml version="1.0"?><worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><dimension ref="B2:A1"/><sheetData><row r="1"><c r="A1" t="inlineStr"><is><t>x</t></is></c></row></sheetData></worksheet>"#,
    );
    must_not_unwind(&XlsxParser, &inverted_dimension, "inverted-dimension.xlsx");

    // 2. Shared-string reference with no sharedStrings.xml part at all.
    let dangling_shared_string = xlsx_with_sheet(
        br#"<?xml version="1.0"?><worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData><row r="1"><c r="A1" t="s"><v>4294967295</v></c></row></sheetData></worksheet>"#,
    );
    must_not_unwind(
        &XlsxParser,
        &dangling_shared_string,
        "dangling-shared-string.xlsx",
    );

    // 3. Cell/row references that are out of range or not references at all.
    let bogus_refs = xlsx_with_sheet(
        br#"<?xml version="1.0"?><worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData><row r="4294967295"><c r="ZZZZZZ99999999999" t="inlineStr"><is><t>x</t></is></c><c r="" t="n"><v>not-a-number</v></c></row></sheetData></worksheet>"#,
    );
    must_not_unwind(&XlsxParser, &bogus_refs, "bogus-refs.xlsx");

    // 4. Truncated sheet XML (unclosed elements, EOF mid-document).
    let truncated = xlsx_with_sheet(
        br#"<?xml version="1.0"?><worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData><row r="1"><c r="A1" t="inlineStr"><is><t>unter"#,
    );
    must_not_unwind(&XlsxParser, &truncated, "truncated-sheet.xlsx");

    // 5. Worksheet target that resolves to a zip-root path with no folder
    //    component — several calamine paths do `path.rfind('/')` on it.
    let rootless_target = zip_of(&[
        ("[Content_Types].xml", XLSX_CONTENT_TYPES),
        ("_rels/.rels", ROOT_RELS),
        ("xl/workbook.xml", XLSX_WORKBOOK),
        (
            "xl/_rels/workbook.xml.rels",
            br#"<?xml version="1.0"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="/sheet1.xml"/></Relationships>"#,
        ),
        (
            "sheet1.xml",
            br#"<?xml version="1.0"?><worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData><row r="1"><c r="A1" t="inlineStr"><is><t>x</t></is></c></row></sheetData></worksheet>"#,
        ),
    ]);
    must_not_unwind(&XlsxParser, &rootless_target, "rootless-target.xlsx");

    // 6. Container-level garbage: not a zip, empty, zip with no OOXML parts.
    must_not_unwind(&XlsxParser, b"", "empty.xlsx");
    must_not_unwind(&XlsxParser, &[0xff; 512], "high-bytes.xlsx");
    must_not_unwind(&XlsxParser, &zip_of(&[("junk.txt", b"x")]), "no-parts.xlsx");

    // 7. The xls (BIFF) reader takes a completely different code path.
    must_not_unwind(&XlsParser, &inverted_dimension, "xlsx-bytes-as.xls");
    must_not_unwind(
        &XlsParser,
        &[0xd0, 0xcf, 0x11, 0xe0, 0x00, 0x00],
        "cfb-stub.xls",
    );
}

#[test]
fn test_docx_adversarial_inputs_do_not_unwind() {
    let docx_of = |document_xml: &[u8], core_xml: &[u8]| {
        zip_of(&[
            ("word/document.xml", document_xml),
            ("docProps/core.xml", core_xml),
        ])
    };

    // 1. Multibyte character straddling the byte offset that the ISO-date
    //    prefix used to slice at (`ooxml::iso_date_prefix`, fixed in #70 —
    //    kept here so the isolation layer is exercised on the same shape).
    let multibyte_date = docx_of(
        br#"<?xml version="1.0"?><w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:p><w:r><w:t>hello</w:t></w:r></w:p></w:body></w:document>"#,
        r#"<?xml version="1.0"?><cp:coreProperties xmlns:cp="x" xmlns:dc="y" xmlns:dcterms="z"><dc:title>T</dc:title><dcterms:created>2026-07-1é09:00</dcterms:created></cp:coreProperties>"#.as_bytes(),
    );
    must_not_unwind(&DocxParser, &multibyte_date, "multibyte-date.docx");

    // 2. Truncated body / unbalanced elements.
    let truncated = docx_of(
        br#"<?xml version="1.0"?><w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:p><w:r><w:t>unter"#,
        br#"<?xml version="1.0"?><cp:coreProperties xmlns:cp="x"/>"#,
    );
    must_not_unwind(&DocxParser, &truncated, "truncated-body.docx");

    // 3. Entity soup and an undefined entity reference in both parts.
    let entities = docx_of(
        br#"<?xml version="1.0"?><w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:p><w:r><w:t>&amp;&undefined;&#x110000;</w:t></w:r></w:p></w:body></w:document>"#,
        br#"<?xml version="1.0"?><cp:coreProperties xmlns:cp="x" xmlns:dc="y"><dc:title>&undefined;</dc:title></cp:coreProperties>"#,
    );
    must_not_unwind(&DocxParser, &entities, "entity-soup.docx");

    // 4. Deep nesting (recursive-descent style stack growth in the reader).
    let mut deep = Vec::from(
        &br#"<?xml version="1.0"?><w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body>"#[..],
    );
    for _ in 0..2000 {
        deep.extend_from_slice(b"<w:p>");
    }
    deep.extend_from_slice(b"<w:r><w:t>deep</w:t></w:r>");
    for _ in 0..2000 {
        deep.extend_from_slice(b"</w:p>");
    }
    deep.extend_from_slice(b"</w:body></w:document>");
    let deep_docx = docx_of(
        &deep,
        br#"<?xml version="1.0"?><cp:coreProperties xmlns:cp="x"/>"#,
    );
    must_not_unwind(&DocxParser, &deep_docx, "deeply-nested.docx");

    // 5. Container-level garbage.
    must_not_unwind(&DocxParser, b"", "empty.docx");
    must_not_unwind(&DocxParser, b"PK\x03\x04 truncated", "truncated-zip.docx");
    must_not_unwind(&DocxParser, &zip_of(&[("junk.txt", b"x")]), "no-parts.docx");
}

#[test]
fn test_pptx_adversarial_inputs_do_not_unwind() {
    const PRESENTATION: &[u8] = br#"<?xml version="1.0"?><p:presentation xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><p:sldIdLst><p:sldId id="256" r:id="rId1"/><p:sldId id="257" r:id="rId2"/></p:sldIdLst></p:presentation>"#;

    // 1. Slide list referencing relationship ids that have no rels entry, and
    //    a rels entry pointing at a part that is not in the package.
    let dangling = zip_of(&[
        ("ppt/presentation.xml", PRESENTATION),
        (
            "ppt/_rels/presentation.xml.rels",
            br#"<?xml version="1.0"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slide" Target="../../../etc/passwd"/></Relationships>"#,
        ),
    ]);
    must_not_unwind(&PptxParser, &dangling, "dangling-rels.pptx");

    // 2. Truncated slide XML plus a notesSlide that is not XML at all.
    let broken_slide = zip_of(&[
        ("ppt/presentation.xml", PRESENTATION),
        (
            "ppt/_rels/presentation.xml.rels",
            br#"<?xml version="1.0"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slide" Target="slides/slide1.xml"/></Relationships>"#,
        ),
        (
            "ppt/slides/slide1.xml",
            br#"<?xml version="1.0"?><p:sld xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"><a:t>unterminated"#,
        ),
        ("ppt/notesSlides/notesSlide1.xml", &[0xff, 0xfe, 0x00, 0x01]),
    ]);
    must_not_unwind(&PptxParser, &broken_slide, "broken-slide.pptx");

    // 3. Container-level garbage.
    must_not_unwind(&PptxParser, b"", "empty.pptx");
    must_not_unwind(&PptxParser, b"PK\x03\x04 truncated", "truncated-zip.pptx");
    must_not_unwind(&PptxParser, &zip_of(&[("junk.txt", b"x")]), "no-parts.pptx");
}

#[test]
fn test_pdf_adversarial_inputs_do_not_unwind() {
    // The PDF parser lost its private `catch_unwind` in AU-21 (the isolation
    // moved up to `Parser::parse_bytes`), so keep exercising malformed PDFs
    // through the public entry point.
    must_not_unwind(&PdfParser, b"", "empty.pdf");
    must_not_unwind(&PdfParser, b"%PDF-1.7\n%%EOF", "header-only.pdf");
    must_not_unwind(
        &PdfParser,
        b"%PDF-1.4\n1 0 obj\n<< /Type /Catalog /Pages 99999 0 R >>\nendobj\ntrailer\n<< /Root 1 0 R >>\nstartxref\n999999999\n%%EOF",
        "bogus-xref.pdf",
    );
    must_not_unwind(&PdfParser, &[0x00; 4096], "zero-bytes.pdf");
}

#[test]
fn test_parsers_do_not_unwind_on_cross_format_payloads() {
    // Extension/content mismatches are routine in real KBs (renamed files,
    // scanner output). Each parser must reject the other formats' bytes
    // without unwinding.
    let docx_bytes = zip_of(&[(
        "word/document.xml",
        br#"<?xml version="1.0"?><w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:p><w:r><w:t>hi</w:t></w:r></w:p></w:body></w:document>"#,
    )]);
    let xlsx_bytes = xlsx_with_sheet(
        br#"<?xml version="1.0"?><worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData><row r="1"><c r="A1" t="inlineStr"><is><t>x</t></is></c></row></sheetData></worksheet>"#,
    );

    let parsers: [&dyn Parser; 5] = [
        &DocxParser,
        &XlsxParser,
        &XlsParser,
        &PptxParser,
        &PdfParser,
    ];
    for parser in parsers {
        let ext = parser.extension();
        must_not_unwind(parser, &docx_bytes, &format!("docx-payload.{ext}"));
        must_not_unwind(parser, &xlsx_bytes, &format!("xlsx-payload.{ext}"));
        must_not_unwind(parser, b"%PDF-1.7\ngarbage", &format!("pdf-payload.{ext}"));
        must_not_unwind(
            parser,
            "日本語のプレーンテキスト".as_bytes(),
            &format!("text-payload.{ext}"),
        );
    }
}
