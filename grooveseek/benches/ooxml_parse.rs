//! (AX-19) Large Office documents through the parsers: `docx_parse/1000_tables` and
//! `pptx_parse/500_slides`.
//!
//! The other benches time Markdown chunking, MMR, search and string helpers. Nothing timed the
//! binary formats, where one document can carry thousands of table rows (feature-64 / feature-65
//! write a table one row per line and record each row's span), and where a regression in the
//! zip read or the XML walk grows with the document rather than with the corpus.
//!
//! Each document is generated in memory before the timing starts, so the bench needs no
//! fixture on disk and no model. The timed body is
//! [`grooveseek::parser::ParserExt::parse_bytes`], the entry the indexer calls (panic
//! isolation included). Throughput is reported per input byte, and the
//! chunk count is printed once, as a `bench-count:` line the nightly job lifts into its
//! summary, so a parser that silently stops reading the tables shows as a count change and
//! not as a faster bench.

#[path = "../tests/common/ooxml.rs"]
#[allow(dead_code, unused_imports)]
mod ooxml;

use criterion::{BenchmarkId, Criterion, Throughput, black_box, criterion_group, criterion_main};
use grooveseek::parser::{DocxParser, ParserExt, PptxParser};
use ooxml::zip_parts;

/// Tables in the generated `.docx`.
const DOCX_TABLES: usize = 1000;
/// Rows per table, and cells per row, in the generated `.docx`.
const DOCX_ROWS: usize = 5;
const DOCX_CELLS: usize = 3;
/// A Heading1 opens a new section every this many tables, so the document has several
/// chunks rather than one 1000-table chunk.
const DOCX_TABLES_PER_SECTION: usize = 20;

/// Slides in the generated `.pptx`.
const PPTX_SLIDES: usize = 500;
/// Rows per table on each slide, and cells per row.
const PPTX_ROWS: usize = 4;
const PPTX_CELLS: usize = 3;

const W_NS: &str = "http://schemas.openxmlformats.org/wordprocessingml/2006/main";
const P_NS: &str = "http://schemas.openxmlformats.org/presentationml/2006/main";
const A_NS: &str = "http://schemas.openxmlformats.org/drawingml/2006/main";
const R_NS: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships";
const REL_NS: &str = "http://schemas.openxmlformats.org/package/2006/relationships";
const CT_NS: &str = "http://schemas.openxmlformats.org/package/2006/content-types";

/// A `.docx` of [`DOCX_TABLES`] tables of [`DOCX_ROWS`] x [`DOCX_CELLS`], a Heading1 and a
/// body paragraph before every [`DOCX_TABLES_PER_SECTION`] of them.
fn make_docx() -> Vec<u8> {
    let mut body = String::new();
    for t in 0..DOCX_TABLES {
        if t % DOCX_TABLES_PER_SECTION == 0 {
            let section = t / DOCX_TABLES_PER_SECTION;
            body.push_str(&format!(
                r#"<w:p><w:pPr><w:pStyle w:val="Heading1"/></w:pPr><w:r><w:t>Section {section}</w:t></w:r></w:p><w:p><w:r><w:t>Tables {t} onwards list the quarterly figures of section {section}.</w:t></w:r></w:p>"#
            ));
        }
        body.push_str("<w:tbl>");
        for r in 0..DOCX_ROWS {
            body.push_str("<w:tr>");
            for c in 0..DOCX_CELLS {
                body.push_str(&format!(
                    "<w:tc><w:p><w:r><w:t>table {t} row {r} cell {c}</w:t></w:r></w:p></w:tc>"
                ));
            }
            body.push_str("</w:tr>");
        }
        body.push_str("</w:tbl>");
    }
    let document = format!(
        r#"<?xml version="1.0"?><w:document xmlns:w="{W_NS}"><w:body>{body}</w:body></w:document>"#
    );
    let content_types = format!(
        r#"<?xml version="1.0"?><Types xmlns="{CT_NS}"><Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/></Types>"#
    );
    zip_parts(&[
        ("[Content_Types].xml", &content_types),
        ("word/document.xml", &document),
    ])
}

/// A `.pptx` of [`PPTX_SLIDES`] slides, each with a title, a body paragraph and a
/// [`PPTX_ROWS`] x [`PPTX_CELLS`] table, in the visible order `ppt/presentation.xml` gives.
fn make_pptx() -> Vec<u8> {
    let mut slides = Vec::with_capacity(PPTX_SLIDES);
    for s in 1..=PPTX_SLIDES {
        let mut table = String::from("<a:tbl>");
        for r in 0..PPTX_ROWS {
            table.push_str("<a:tr>");
            for c in 0..PPTX_CELLS {
                table.push_str(&format!(
                    "<a:tc><a:txBody><a:p><a:r><a:t>slide {s} row {r} cell {c}</a:t></a:r></a:p></a:txBody></a:tc>"
                ));
            }
            table.push_str("</a:tr>");
        }
        table.push_str("</a:tbl>");
        slides.push(format!(
            r#"<?xml version="1.0"?><p:sld xmlns:p="{P_NS}" xmlns:a="{A_NS}"><p:cSld><p:spTree><p:sp><p:nvSpPr><p:nvPr><p:ph type="title"/></p:nvPr></p:nvSpPr><p:txBody><a:p><a:r><a:t>Slide {s} overview</a:t></a:r></a:p></p:txBody></p:sp><p:sp><p:txBody><a:p><a:r><a:t>Slide {s} walks through the figures in the table below.</a:t></a:r></a:p></p:txBody></p:sp><p:graphicFrame><a:graphic><a:graphicData>{table}</a:graphicData></a:graphic></p:graphicFrame></p:spTree></p:cSld></p:sld>"#
        ));
    }
    let mut sld_ids = String::new();
    let mut rels = String::new();
    for s in 1..=PPTX_SLIDES {
        sld_ids.push_str(&format!(r#"<p:sldId id="{}" r:id="rId{s}"/>"#, 255 + s));
        rels.push_str(&format!(
            r#"<Relationship Id="rId{s}" Type="{R_NS}/slide" Target="slides/slide{s}.xml"/>"#
        ));
    }
    let presentation = format!(
        r#"<?xml version="1.0"?><p:presentation xmlns:p="{P_NS}" xmlns:r="{R_NS}"><p:sldIdLst>{sld_ids}</p:sldIdLst></p:presentation>"#
    );
    let presentation_rels =
        format!(r#"<?xml version="1.0"?><Relationships xmlns="{REL_NS}">{rels}</Relationships>"#);
    let content_types = format!(r#"<?xml version="1.0"?><Types xmlns="{CT_NS}"/>"#);

    let names: Vec<String> = (1..=PPTX_SLIDES)
        .map(|s| format!("ppt/slides/slide{s}.xml"))
        .collect();
    let mut parts: Vec<(&str, &str)> = vec![
        ("[Content_Types].xml", &content_types),
        ("ppt/presentation.xml", &presentation),
        ("ppt/_rels/presentation.xml.rels", &presentation_rels),
    ];
    parts.extend(
        names
            .iter()
            .map(String::as_str)
            .zip(slides.iter().map(String::as_str)),
    );
    zip_parts(&parts)
}

fn bench_docx(c: &mut Criterion) {
    let bytes = make_docx();
    let parser = DocxParser::default();
    let parsed = parser
        .parse_bytes(&bytes, "bench.docx", &[])
        .expect("the generated .docx parses");
    let rows: usize = parsed
        .chunks
        .iter()
        .map(|ch| ch.table_rows.as_ref().map_or(0, Vec::len))
        .sum();
    // Anti-vacuity: a parser that stopped reading tables would time an empty walk.
    assert_eq!(
        rows,
        DOCX_TABLES * DOCX_ROWS,
        "every table row of the generated .docx is read as a table row"
    );
    println!(
        "bench-count: docx_parse/1000_tables: bytes={} chunks={} table_rows={rows}",
        bytes.len(),
        parsed.chunks.len()
    );

    let mut group = c.benchmark_group("docx_parse");
    group.throughput(Throughput::Bytes(bytes.len() as u64));
    group.bench_with_input(
        BenchmarkId::from_parameter(format!("{DOCX_TABLES}_tables")),
        &bytes,
        |b, bytes| {
            b.iter(|| {
                let parsed = parser
                    .parse_bytes(black_box(bytes), "bench.docx", &[])
                    .expect("parse");
                black_box(parsed);
            });
        },
    );
    group.finish();
}

fn bench_pptx(c: &mut Criterion) {
    let bytes = make_pptx();
    let parser = PptxParser::default();
    let parsed = parser
        .parse_bytes(&bytes, "bench.pptx", &[])
        .expect("the generated .pptx parses");
    let rows: usize = parsed
        .chunks
        .iter()
        .map(|ch| ch.table_rows.as_ref().map_or(0, Vec::len))
        .sum();
    assert_eq!(
        parsed.chunks.len(),
        PPTX_SLIDES,
        "one chunk per slide of the generated .pptx"
    );
    assert_eq!(
        rows,
        PPTX_SLIDES * PPTX_ROWS,
        "every table row of the generated .pptx is read as a table row"
    );
    println!(
        "bench-count: pptx_parse/500_slides: bytes={} chunks={} table_rows={rows}",
        bytes.len(),
        parsed.chunks.len()
    );

    let mut group = c.benchmark_group("pptx_parse");
    group.throughput(Throughput::Bytes(bytes.len() as u64));
    group.bench_with_input(
        BenchmarkId::from_parameter(format!("{PPTX_SLIDES}_slides")),
        &bytes,
        |b, bytes| {
            b.iter(|| {
                let parsed = parser
                    .parse_bytes(black_box(bytes), "bench.pptx", &[])
                    .expect("parse");
                black_box(parsed);
            });
        },
    );
    group.finish();
}

criterion_group!(benches, bench_docx, bench_pptx);
criterion_main!(benches);
