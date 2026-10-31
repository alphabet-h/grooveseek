//! AX-08 (kuriya 607): what 1.20.1 promised about a `.docx` / `.pptx` part that `zip` opens
//! but cannot read as declared (CHANGELOG, `[1.20.1]` Security), pinned through the parser and
//! through `groove index`:
//!
//! 1. a part that holds more than it declares, or whose CRC does not match, is skipped with a
//!    warning that names it, and the document is read without it;
//! 2. when that part is `word/document.xml`, the document fails as it does when the part is
//!    missing;
//! 3. a part `zip` cannot open at all, such as an encrypted one, is dropped without a line;
//! 4. the warning is `warning: <file>: <part> cannot be read as declared (it declares N bytes);
//!    it holds more than it declares, or is corrupt; skipping this part`, on stderr.
//!
//! The archives are written with `zip` and then forged byte by byte: the declared size, the CRC
//! or the encryption flag of one entry, in both its local file header and its central directory
//! header. The CLI tests run against the mock endpoint of [`crate::common::embed_cli`], so
//! nothing downloads a model.

mod common;

use common::embed_cli::{DIM, Fixture, fixture, stderr_of};
use common::embed_mock::{assert_dir_empty, openai_config_toml};

use grooveseek::parser::{DocxParser, ParsedDocument, ParserExt, PptxParser};
use std::io::{Cursor, Write};
use zip::write::SimpleFileOptions;

/// The WordprocessingML namespace.
const W_NS: &str = "http://schemas.openxmlformats.org/wordprocessingml/2006/main";
/// The PresentationML and DrawingML namespaces.
const P_NS: &str = "http://schemas.openxmlformats.org/presentationml/2006/main";
const A_NS: &str = "http://schemas.openxmlformats.org/drawingml/2006/main";

const DOCUMENT: &str = "word/document.xml";
const STYLES: &str = "word/styles.xml";
const SLIDE_2: &str = "ppt/slides/slide2.xml";

/// The three slides of [`deck`]: title and body. No word is shared between them.
const SLIDES: [(&str, &str); 3] = [
    ("Harbor", "kettle passage on the opening slide"),
    ("Lantern", "violin passage on the middle slide"),
    ("Orchard", "glacier passage on the closing slide"),
];

/// What is done to one entry of an archive.
#[derive(Clone, Copy, Debug)]
enum Forgery {
    /// Declare an uncompressed size of 1 byte. The entry is deflated and holds more, so
    /// `zip` 9.0.1 and later fail the read at the declared size (zip-rs/zip2#1022).
    DeclaresLess,
    /// Flip every bit of the CRC-32, so the read fails when it reaches the end.
    BadCrc,
    /// Set bit 0 of the general purpose flag (traditional PKWARE encryption), so `zip` refuses
    /// to open the entry without a password.
    Encrypted,
}

/// A zip of `parts`, in the order given, each deflated.
fn zip_of(parts: &[(&str, &[u8])]) -> Vec<u8> {
    let mut buf = Vec::new();
    {
        let mut zip = zip::ZipWriter::new(Cursor::new(&mut buf));
        let opt = SimpleFileOptions::default();
        for (name, bytes) in parts {
            zip.start_file(*name, opt).expect("start a part");
            zip.write_all(bytes).expect("write a part");
        }
        zip.finish().expect("finish the zip");
    }
    buf
}

/// The offsets of the local file header and of the central directory header of the entry
/// called `name`. Panics unless there is exactly one of each.
fn headers_of(zip: &[u8], name: &str) -> (usize, usize) {
    let u16_at = |i: usize| usize::from(u16::from_le_bytes([zip[i], zip[i + 1]]));
    let (mut local, mut central) = (Vec::new(), Vec::new());
    let mut i = 0;
    while i + 4 <= zip.len() {
        if zip[i..i + 4] == *b"PK\x03\x04" && i + 30 <= zip.len() {
            let len = u16_at(i + 26);
            if zip.get(i + 30..i + 30 + len) == Some(name.as_bytes()) {
                local.push(i);
            }
        } else if zip[i..i + 4] == *b"PK\x01\x02" && i + 46 <= zip.len() {
            let len = u16_at(i + 28);
            if zip.get(i + 46..i + 46 + len) == Some(name.as_bytes()) {
                central.push(i);
            }
        }
        i += 1;
    }
    assert_eq!(
        (local.len(), central.len()),
        (1, 1),
        "expected one local and one central header for {name}"
    );
    (local[0], central[0])
}

/// `zip` with `forgery` applied to the entry `name`, in both of its headers.
fn forge(zip: &[u8], name: &str, forgery: Forgery) -> Vec<u8> {
    let (local, central) = headers_of(zip, name);
    let mut data = zip.to_vec();
    match forgery {
        Forgery::DeclaresLess => {
            for at in [local + 22, central + 24] {
                data[at..at + 4].copy_from_slice(&1u32.to_le_bytes());
            }
        }
        Forgery::BadCrc => {
            for at in [local + 14, central + 16] {
                for byte in &mut data[at..at + 4] {
                    *byte = !*byte;
                }
            }
        }
        Forgery::Encrypted => {
            for at in [local + 6, central + 8] {
                data[at] |= 1;
            }
        }
    }
    let mut archive = zip::ZipArchive::new(Cursor::new(data.as_slice()))
        .unwrap_or_else(|e| panic!("premise: a {forgery:?} archive still opens: {e}"));
    match forgery {
        Forgery::DeclaresLess => {
            let part = archive.by_name(name).expect("premise: the part opens");
            assert_eq!(part.size(), 1, "premise: {name} declares 1 byte");
            assert_eq!(
                part.compression(),
                zip::CompressionMethod::Deflated,
                "premise: {name} is deflated"
            );
        }
        Forgery::BadCrc => {
            archive.by_name(name).expect("premise: the part opens");
        }
        Forgery::Encrypted => {
            assert!(
                archive.by_name(name).is_err(),
                "premise: zip cannot open an encrypted {name} without a password"
            );
        }
    }
    data
}

/// The size `name` declares in `zip`.
fn declared_size(zip: &[u8], name: &str) -> u64 {
    zip::ZipArchive::new(Cursor::new(zip))
        .expect("open the zip")
        .by_name(name)
        .expect("open the part")
        .size()
}

/// A slide titled `title` over `body`.
fn slide_xml(title: &str, body: &str) -> String {
    format!(
        concat!(
            r#"<?xml version="1.0"?><p:sld xmlns:p="{p}" xmlns:a="{a}"><p:cSld><p:spTree>"#,
            r#"<p:sp><p:nvSpPr><p:nvPr><p:ph type="title"/></p:nvPr></p:nvSpPr><p:txBody><a:p><a:r><a:t>{title}</a:t></a:r></a:p></p:txBody></p:sp>"#,
            r#"<p:sp><p:txBody><a:p><a:r><a:t>{body}</a:t></a:r></a:p></p:txBody></p:sp>"#,
            r#"</p:spTree></p:cSld></p:sld>"#,
        ),
        p = P_NS,
        a = A_NS,
        title = title,
        body = body,
    )
}

/// A `.pptx` of [`SLIDES`], without `ppt/presentation.xml`, so the slides are read in the
/// order of their file numbers.
fn deck() -> Vec<u8> {
    let slides: Vec<(String, String)> = SLIDES
        .iter()
        .enumerate()
        .map(|(i, (title, body))| {
            (
                format!("ppt/slides/slide{}.xml", i + 1),
                slide_xml(title, body),
            )
        })
        .collect();
    let mut parts: Vec<(&str, &[u8])> = vec![(
        "[Content_Types].xml",
        br#"<?xml version="1.0"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"/>"#,
    )];
    parts.extend(slides.iter().map(|(n, x)| (n.as_str(), x.as_bytes())));
    zip_of(&parts)
}

/// The `word/document.xml` of a document whose headings use the style IDs `1` and `2`, the way
/// Word in Japanese writes them: only `word/styles.xml` says they are headings.
fn document_xml() -> String {
    let paragraphs: [(Option<&str>, &str); 7] = [
        (None, "walrus preface opening the pages before any chapter"),
        (Some("1"), "Harbor"),
        (None, "kettle passage under the opening chapter"),
        (Some("2"), "Lantern"),
        (None, "violin passage under the nested section"),
        (Some("1"), "Orchard"),
        (None, "glacier passage under the closing chapter"),
    ];
    let mut body = String::new();
    for (style, text) in paragraphs {
        let ppr = style
            .map(|id| format!(r#"<w:pPr><w:pStyle w:val="{id}"/></w:pPr>"#))
            .unwrap_or_default();
        body.push_str(&format!("<w:p>{ppr}<w:r><w:t>{text}</w:t></w:r></w:p>"));
    }
    format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><w:document xmlns:w="{W_NS}"><w:body>{body}</w:body></w:document>"#
    )
}

/// A `word/styles.xml` naming the style IDs `1` and `2` `heading 1` and `heading 2`.
fn styles_xml() -> String {
    let mut styles = String::from(
        r#"<w:style w:type="paragraph" w:default="1" w:styleId="a"><w:name w:val="Normal"/></w:style>"#,
    );
    for (id, outline) in [("1", "0"), ("2", "1")] {
        styles.push_str(&format!(
            r#"<w:style w:type="paragraph" w:styleId="{id}"><w:name w:val="heading {id}"/><w:basedOn w:val="a"/><w:pPr><w:outlineLvl w:val="{outline}"/></w:pPr></w:style>"#
        ));
    }
    format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><w:styles xmlns:w="{W_NS}">{styles}</w:styles>"#
    )
}

/// A `.docx` of [`document_xml`], with [`styles_xml`] when `with_styles`, or without
/// `word/document.xml` when `with_document` is false.
fn docx(with_document: bool, with_styles: bool) -> Vec<u8> {
    let doc = document_xml();
    let styles = styles_xml();
    let mut parts: Vec<(&str, &[u8])> = Vec::new();
    if with_document {
        parts.push((DOCUMENT, doc.as_bytes()));
    }
    if with_styles {
        parts.push((STYLES, styles.as_bytes()));
    }
    parts.push((
        "docProps/core.xml",
        br#"<?xml version="1.0"?><cp:coreProperties xmlns:cp="x" xmlns:dc="y"><dc:title>Almanac</dc:title></cp:coreProperties>"#,
    ));
    zip_of(&parts)
}

/// Each chunk's heading and content.
fn sections(doc: &ParsedDocument) -> Vec<(Option<String>, String)> {
    doc.chunks
        .iter()
        .map(|c| (c.heading.clone(), c.content.clone()))
        .collect()
}

fn parse_pptx(bytes: &[u8]) -> ParsedDocument {
    PptxParser::default()
        .parse_bytes(bytes, "deck.pptx", &[])
        .expect("the deck parses")
}

fn parse_docx(bytes: &[u8], path_hint: &str) -> anyhow::Result<ParsedDocument> {
    DocxParser::default().parse_bytes(bytes, path_hint, &[])
}

/// The warning 1.20.1 promised for a part that cannot be read as declared.
fn unreadable_line(file: &str, part: &str, declared: u64) -> String {
    format!(
        "warning: {file}: {part} cannot be read as declared (it declares {declared} bytes); it holds more than it declares, or is corrupt; skipping this part"
    )
}

/// The slides a deck read without its second slide gives: the others, still labelled by their
/// place in the file order.
fn slides_one_and_three() -> Vec<Option<String>> {
    vec![
        Some(format!("Slide 1: {}", SLIDES[0].0)),
        Some(format!("Slide 3: {}", SLIDES[2].0)),
    ]
}

// ---------------------------------------------------------------------------
// The parser
// ---------------------------------------------------------------------------

/// Promise 1: a slide that holds more than it declares, or whose CRC does not match, is
/// skipped, and the other slides are read as they would be.
#[test]
fn a_pptx_slide_holding_more_than_it_declares_is_skipped_and_the_other_slides_are_indexed() {
    let intact = parse_pptx(&deck());
    assert_eq!(intact.chunks.len(), 3, "premise: three slides");
    for forgery in [Forgery::DeclaresLess, Forgery::BadCrc] {
        let doc = parse_pptx(&forge(&deck(), SLIDE_2, forgery));
        let headings: Vec<_> = doc.chunks.iter().map(|c| c.heading.clone()).collect();
        assert_eq!(headings, slides_one_and_three(), "{forgery:?}");
        assert!(
            !doc.raw_content.contains(SLIDES[1].1),
            "{forgery:?}: nothing of the skipped slide is indexed:\n{}",
            doc.raw_content
        );
        assert_eq!(
            doc.chunks[0].content, intact.chunks[0].content,
            "{forgery:?}"
        );
        assert_eq!(
            doc.chunks[1].content, intact.chunks[2].content,
            "{forgery:?}"
        );
    }
}

/// Promise 2: a `word/document.xml` that cannot be read as declared fails the document with
/// the error a `.docx` without that part gets.
#[test]
fn a_docx_whose_document_xml_lies_fails_like_a_missing_one() {
    let missing = parse_docx(&docx(false, true), "almanac.docx")
        .expect_err("premise: a docx without word/document.xml fails")
        .to_string();
    assert_eq!(missing, "almanac.docx: word/document.xml missing");
    for forgery in [Forgery::DeclaresLess, Forgery::BadCrc] {
        let err = parse_docx(&forge(&docx(true, true), DOCUMENT, forgery), "almanac.docx")
            .expect_err("a document.xml that cannot be read as declared fails the docx");
        assert_eq!(err.to_string(), missing, "{forgery:?}");
    }
}

/// Promise 1 for a part the document can do without: a `word/styles.xml` whose CRC does not
/// match, or that holds more than it declares, is left out, and the document is read as one
/// without styles.
#[test]
fn a_docx_styles_part_with_a_bad_crc_warns_and_falls_back() {
    let styled = sections(&parse_docx(&docx(true, true), "almanac.docx").unwrap());
    let unstyled = sections(&parse_docx(&docx(true, false), "almanac.docx").unwrap());
    assert_ne!(
        styled, unstyled,
        "premise: the styles part changes how the document is split"
    );
    for forgery in [Forgery::BadCrc, Forgery::DeclaresLess] {
        let doc = parse_docx(&forge(&docx(true, true), STYLES, forgery), "almanac.docx")
            .expect("a docx whose styles part cannot be read is still read");
        assert_eq!(sections(&doc), unstyled, "{forgery:?}");
    }
}

/// Promise 3: a part `zip` cannot open, such as an encrypted one, is dropped the way a missing
/// part is: a slide is left out, and a `word/document.xml` fails the document.
#[test]
fn an_encrypted_ooxml_part_is_dropped_like_a_missing_one() {
    let doc = parse_pptx(&forge(&deck(), SLIDE_2, Forgery::Encrypted));
    let headings: Vec<_> = doc.chunks.iter().map(|c| c.heading.clone()).collect();
    assert_eq!(headings, slides_one_and_three());

    let err = parse_docx(
        &forge(&docx(true, true), DOCUMENT, Forgery::Encrypted),
        "almanac.docx",
    )
    .expect_err("an encrypted document.xml fails the docx");
    assert_eq!(err.to_string(), "almanac.docx: word/document.xml missing");

    let doc = parse_docx(
        &forge(&docx(true, true), STYLES, Forgery::Encrypted),
        "almanac.docx",
    )
    .expect("a docx whose styles part is encrypted is still read");
    let unstyled = sections(&parse_docx(&docx(true, false), "almanac.docx").unwrap());
    assert_eq!(sections(&doc), unstyled);
}

// ---------------------------------------------------------------------------
// groove index
// ---------------------------------------------------------------------------

/// A knowledge base holding `files`, a mock, and a `groove.toml` enabling Markdown, docx and
/// pptx.
fn office_kb(prefix: &str, files: &[(&str, Vec<u8>)]) -> Fixture {
    let fx = fixture(prefix, &[], "");
    let toml = format!(
        "{}[parsers]\nenabled = [\"md\", \"docx\", \"pptx\"]\n",
        openai_config_toml(&fx.mock.endpoint(), None, DIM)
    );
    std::fs::write(&fx.config, toml).expect("write groove.toml");
    for (rel, bytes) in files {
        std::fs::write(fx.kb().join(rel), bytes).expect("write a document");
    }
    fx
}

/// `groove index`, which must succeed; its stderr without colour.
fn index_stderr(fx: &Fixture) -> String {
    let out = fx.run_index();
    let stderr = stderr_of(&out);
    assert!(out.status.success(), "groove index failed:\n{stderr}");
    stderr
}

fn has_line(stderr: &str, line: &str) -> bool {
    stderr.lines().any(|l| l.trim_end() == line)
}

/// Promises 1, 2 and 4 through the binary: each part that cannot be read as declared is named
/// on stderr in the promised words, the document is indexed without it, and a docx whose
/// `word/document.xml` is such a part is skipped with the error a docx without one gets.
#[test]
fn groove_index_names_an_ooxml_part_unreadable_as_declared_on_stderr() {
    let styles_crc = forge(&docx(true, true), STYLES, Forgery::BadCrc);
    let styles_declared = declared_size(&styles_crc, STYLES);
    let unstyled_chunks = parse_docx(&docx(true, false), "x.docx")
        .unwrap()
        .chunks
        .len();
    let styled_chunks = parse_docx(&docx(true, true), "x.docx")
        .unwrap()
        .chunks
        .len();
    assert_ne!(
        unstyled_chunks, styled_chunks,
        "premise: the chunk count tells a styled reading from an unstyled one"
    );
    let fx = office_kb(
        "groove-ax08-unreadable",
        &[
            ("deck.pptx", forge(&deck(), SLIDE_2, Forgery::DeclaresLess)),
            ("crc-deck.pptx", forge(&deck(), SLIDE_2, Forgery::BadCrc)),
            ("styles.docx", styles_crc),
            (
                "lies.docx",
                forge(&docx(true, true), DOCUMENT, Forgery::DeclaresLess),
            ),
            ("missing.docx", docx(false, true)),
        ],
    );
    let stderr = index_stderr(&fx);

    for line in [
        unreadable_line("deck.pptx", SLIDE_2, 1),
        unreadable_line("crc-deck.pptx", SLIDE_2, declared_size(&deck(), SLIDE_2)),
        unreadable_line("styles.docx", STYLES, styles_declared),
        unreadable_line("lies.docx", DOCUMENT, 1),
        "  indexed: deck.pptx (2 chunks)".to_string(),
        "  indexed: crc-deck.pptx (2 chunks)".to_string(),
        format!("  indexed: styles.docx ({unstyled_chunks} chunks)"),
        "Skipping lies.docx: parse failed: lies.docx: word/document.xml missing".to_string(),
        "Skipping missing.docx: parse failed: missing.docx: word/document.xml missing".to_string(),
    ] {
        assert!(
            has_line(&stderr, &line),
            "expected the line {line:?}:\n{stderr}"
        );
    }
    assert!(
        stderr.is_ascii(),
        "the warnings are ASCII (AGENTS.md):\n{stderr}"
    );
    assert!(
        !stderr.contains("max_decompressed_size"),
        "no key is named, since raising one does not make such a part readable:\n{stderr}"
    );
    assert_eq!(
        fx.documents(),
        3,
        "the two decks and styles.docx are indexed"
    );
    assert_eq!(fx.chunks(), 2 + 2 + unstyled_chunks as u64);
    // Of each deck, only the skipped slide held "violin"; no chunk of a deck does. (The docx
    // holds the word too, so a hit there says nothing.)
    let hits = fx.search_json("violin");
    let results = hits
        .pointer("/results")
        .and_then(|v| v.as_array())
        .unwrap_or_else(|| panic!("no results array: {hits}"));
    let decks: Vec<_> = results
        .iter()
        .filter(|h| {
            h.pointer("/path")
                .and_then(|v| v.as_str())
                .is_some_and(|p| p.ends_with(".pptx"))
        })
        .collect();
    assert!(!decks.is_empty(), "premise: the decks are searched: {hits}");
    for hit in decks {
        let content = hit.pointer("/content").and_then(|v| v.as_str());
        assert!(
            content.is_some_and(|c| !c.contains("violin")),
            "a skipped slide reached the index: {hit}"
        );
    }
    assert_dir_empty(&fx.cache);
}

/// Promise 3 through the binary: an encrypted slide and an encrypted `word/document.xml` get
/// no line naming them; the deck is indexed without the slide and the docx is skipped as one
/// without the part.
#[test]
fn groove_index_drops_an_encrypted_ooxml_part_without_a_word() {
    let fx = office_kb(
        "groove-ax08-encrypted",
        &[
            ("locked.pptx", forge(&deck(), SLIDE_2, Forgery::Encrypted)),
            (
                "sealed.docx",
                forge(&docx(true, true), DOCUMENT, Forgery::Encrypted),
            ),
        ],
    );
    let stderr = index_stderr(&fx);
    for line in [
        "  indexed: locked.pptx (2 chunks)",
        "Skipping sealed.docx: parse failed: sealed.docx: word/document.xml missing",
    ] {
        assert!(
            has_line(&stderr, line),
            "expected the line {line:?}:\n{stderr}"
        );
    }
    for quiet in [
        SLIDE_2,
        "cannot be read as declared",
        "warning: sealed.docx",
    ] {
        assert!(
            !stderr.contains(quiet),
            "an encrypted part is dropped without a line ({quiet:?}):\n{stderr}"
        );
    }
    assert_eq!(fx.documents(), 1, "only the deck is indexed");
    assert_dir_empty(&fx.cache);
}
