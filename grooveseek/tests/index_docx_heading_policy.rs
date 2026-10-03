//! feature-62: `.docx` headings through `groove index`, the binary an operator runs, rather
//! than through the parser alone.
//!
//! Each test drives a knowledge base of generated `.docx` files against the mock endpoint of
//! [`crate::common::embed_cli`], so nothing downloads a model. The documents are built here,
//! not by the parser's `cfg(test)` fixture, which an integration test cannot reach; they copy
//! its shapes and its words. The file name, the title, the headings and the bodies share no
//! word, so a search cannot pass by matching the wrong one.

mod common;

use common::embed_cli::{DIM, Fixture, fixture, stderr_of};
use common::embed_mock::{assert_dir_empty, openai_config_toml};

use std::io::Write;
use zip::write::SimpleFileOptions;

/// The WordprocessingML namespace.
const W_NS: &str = "http://schemas.openxmlformats.org/wordprocessingml/2006/main";

const TITLE: &str = "Almanac";
const PREFACE: &str = "walrus preface opening the pages before any chapter begins";
const H_FIRST: &str = "Harbor";
const B_FIRST: &str = "kettle passage under the opening chapter with enough words";
const H_NESTED: &str = "Lantern";
const B_NESTED: &str = "violin passage under the nested section with enough words";
const H_SECOND: &str = "Orchard";
const B_SECOND: &str = "glacier passage under the closing chapter with enough words";

/// `[parsers].enabled` for every test here but the one about a knowledge base without docx.
const MD_AND_DOCX: &str = r#"["md", "docx"]"#;

/// A zip of `parts`, in the order given, each deflated.
fn docx(parts: &[(&str, &[u8])]) -> Vec<u8> {
    let mut buf = Vec::new();
    {
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
        let opt = SimpleFileOptions::default();
        for (name, bytes) in parts {
            zip.start_file(*name, opt).expect("start a part");
            zip.write_all(bytes).expect("write a part");
        }
        zip.finish().expect("finish the zip");
    }
    buf
}

/// A `word/document.xml` with one `<w:p>` per entry; `Some(id)` styles it with an empty
/// `<w:pStyle w:val="id"/>`, the form Word writes.
fn document_xml(paragraphs: &[(Option<&str>, &str)]) -> String {
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

/// A `word/styles.xml` holding `styles`, after a `w:docDefaults` and a `w:latentStyles`
/// whose `w:lsdException` entries name `heading 1` .. `heading 3` the way Word writes
/// them, so a heading name always appears somewhere that is not a style definition.
fn styles_xml(styles: &str) -> String {
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

/// The styles of a document Word 2010 with a Japanese UI writes: the default paragraph
/// style `a` (`Normal`), headings with the style IDs `1` .. `3` named `heading 1` ..
/// `heading 3`, based on `a`, with outline levels 0 .. 2, a `Title` (`af`) based on `a`,
/// and a character and a table style. A paragraph refers to them with an empty
/// `<w:pStyle/>`.
fn word2010_ja_styles() -> String {
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
fn core_xml(title: &str) -> String {
    format!(
        r#"<?xml version="1.0"?><cp:coreProperties xmlns:cp="x" xmlns:dc="y"><dc:title>{title}</dc:title></cp:coreProperties>"#
    )
}

/// The `word/document.xml` of the parser tests' AC1 document: a preface, then [`H_FIRST`]
/// (style `1`), [`H_NESTED`] (style `2`) and [`H_SECOND`] (style `1`), each over its body.
fn numeric_heading_document_xml() -> String {
    document_xml(&[
        (None, PREFACE),
        (Some("1"), H_FIRST),
        (None, B_FIRST),
        (Some("2"), H_NESTED),
        (None, B_NESTED),
        (Some("1"), H_SECOND),
        (None, B_SECOND),
    ])
}

/// The AC1 document ([`numeric_heading_document_xml`]) styled by [`word2010_ja_styles`],
/// titled `title`, with `extra` parts after its own.
fn numeric_heading_docx(title: &str, extra: &[(&str, &[u8])]) -> Vec<u8> {
    let doc = numeric_heading_document_xml();
    let styles = styles_xml(&word2010_ja_styles());
    let core = core_xml(title);
    let mut parts: Vec<(&str, &[u8])> = vec![
        ("word/document.xml", doc.as_bytes()),
        ("word/styles.xml", styles.as_bytes()),
        ("docProps/core.xml", core.as_bytes()),
    ];
    parts.extend_from_slice(extra);
    docx(&parts)
}

/// Point `fx`'s `groove.toml` at its mock with `parsers` enabled. `top` goes before the first
/// table (a top-level key such as `exclude_headings`), `tables` after `[parsers]`. The file
/// sits beside the knowledge base and is passed with `--config`, so its `[index]` caps count.
fn configure(fx: &Fixture, top: &str, parsers: &str, tables: &str) {
    let toml = format!(
        "{top}{}[parsers]\nenabled = {parsers}\n{tables}",
        openai_config_toml(&fx.mock.endpoint(), None, DIM)
    );
    std::fs::write(&fx.config, toml).expect("write groove.toml");
}

/// An empty knowledge base, a mock, and a `groove.toml` enabling Markdown and docx.
fn docx_kb(prefix: &str) -> Fixture {
    let fx = fixture(prefix, &[], "");
    configure(&fx, "", MD_AND_DOCX, "");
    fx
}

/// Write `bytes` to `rel` in the knowledge base.
fn write_bytes(fx: &Fixture, rel: &str, bytes: &[u8]) {
    std::fs::write(fx.kb().join(rel), bytes).expect("write a document");
}

/// `groove index`, which must succeed; its stderr without colour.
fn index_stderr(fx: &Fixture) -> String {
    let out = fx.run_index();
    let stderr = stderr_of(&out);
    assert!(out.status.success(), "groove index failed:\n{stderr}");
    stderr
}

/// feature-62 C1 (AC20): Word-in-Japanese headings split the document through the binary,
/// and a word only the nested section's body holds finds that section first.
#[test]
fn a_numeric_style_docx_is_indexed_in_sections() {
    let fx = docx_kb("groove-f62-c1");
    write_bytes(&fx, "quarry.docx", &numeric_heading_docx(TITLE, &[]));
    let stderr = index_stderr(&fx);
    assert!(
        stderr.contains("indexed: quarry.docx (4 chunks)"),
        "expected four sections:\n{stderr}"
    );

    let hits = fx.search_json("violin");
    assert_eq!(
        hits.pointer("/results/0/path").and_then(|v| v.as_str()),
        Some("quarry.docx"),
        "{hits}"
    );
    assert_eq!(
        hits.pointer("/results/0/heading").and_then(|v| v.as_str()),
        Some(H_NESTED),
        "{hits}"
    );
    assert_dir_empty(&fx.cache);
}

/// The decompression budget C2 indexes under.
const CAP: usize = 8192;

/// `xml` with an XML comment inserted before the last `before`, so it is exactly `len` bytes.
fn pad_to(xml: &str, before: &str, len: usize) -> String {
    let at = xml.rfind(before).expect("the insertion point");
    let filler = len
        .checked_sub(xml.len() + "<!---->".len())
        .expect("room for the comment");
    let padded = format!("{}<!--{}-->{}", &xml[..at], "x".repeat(filler), &xml[at..]);
    assert_eq!(padded.len(), len);
    padded
}

/// The stderr lines about `name`'s `word/styles.xml`.
fn styles_lines<'a>(stderr: &'a str, name: &str) -> Vec<&'a str> {
    let about = format!("{name}: word/styles.xml");
    stderr.lines().filter(|l| l.contains(&about)).collect()
}

/// feature-62 C2 (AC12-AC15): each way a styles part goes unused is named on stderr in one
/// line of its own, and `<w:styles/>`, which is usable, is not named at all. Every document is
/// still indexed. The cap is passed with `--config`: a `groove.toml` found beside the knowledge
/// base cannot set it.
#[test]
fn broken_or_oversized_styles_parts_are_named_on_stderr() {
    let fx = docx_kb("groove-f62-c2");
    configure(
        &fx,
        "",
        MD_AND_DOCX,
        &format!("[index]\nmax_decompressed_size = {CAP}\n"),
    );
    let doc = document_xml(&[(None, PREFACE), (Some("1"), H_FIRST), (None, B_FIRST)]);
    let heading_1 =
        r#"<w:style w:type="paragraph" w:styleId="1"><w:name w:val="heading 1"/></w:style>"#;
    let word = styles_xml(&word2010_ja_styles());
    assert!(
        word.len() < CAP && doc.len() <= CAP - word.len(),
        "fixture: the styles part fits the cap with room to pad the document"
    );
    let parts: Vec<(&str, String, String)> = vec![
        (
            "eof.docx",
            doc.clone(),
            format!(
                r#"<?xml version="1.0"?><w:styles xmlns:w="{W_NS}">{heading_1}<w:style w:type="paragraph" w:styleId="2"><w:name w:val="heading 2"/>"#
            ),
        ),
        (
            "cut.docx",
            doc.clone(),
            format!(r#"<w:styles xmlns:w="{W_NS}">{heading_1}<w:sty"#),
        ),
        ("prose.docx", doc.clone(), "not xml at all".to_string()),
        (
            "elsewhere.docx",
            doc.clone(),
            format!(r#"<w:document xmlns:w="{W_NS}">{heading_1}</w:document>"#),
        ),
        (
            "nostyles.docx",
            doc.clone(),
            format!(r#"<w:styles xmlns:w="{W_NS}"/>"#),
        ),
        (
            "single.docx",
            doc.clone(),
            pad_to(&word, "</w:styles>", CAP + 1),
        ),
        (
            "sum.docx",
            pad_to(&doc, "</w:body>", CAP - word.len() + 1),
            word.clone(),
        ),
    ];
    for (name, document, styles) in &parts {
        write_bytes(
            &fx,
            name,
            &docx(&[
                ("word/document.xml", document.as_bytes()),
                ("word/styles.xml", styles.as_bytes()),
            ]),
        );
    }

    let stderr = index_stderr(&fx);
    for (name, _, _) in &parts {
        assert!(
            stderr.contains(&format!("indexed: {name} (")),
            "{name} is still indexed:\n{stderr}"
        );
    }
    let expect_one = |name: &str, says: &str| {
        let lines = styles_lines(&stderr, name);
        assert_eq!(lines.len(), 1, "one line about {name}:\n{stderr}");
        assert!(lines[0].contains(says), "{name}: {}", lines[0]);
        assert!(lines[0].is_ascii(), "{}", lines[0]);
    };
    expect_one("eof.docx", "is not a readable styles part (ended with ");
    expect_one("cut.docx", "is not a readable styles part (XML error:");
    expect_one(
        "prose.docx",
        "is not a readable styles part (no root element)",
    );
    expect_one(
        "elsewhere.docx",
        "is not a readable styles part (root element is not w:styles)",
    );
    expect_one("single.docx", "exceeds [index].max_decompressed_size");
    expect_one(
        "sum.docx",
        "would take the document past [index].max_decompressed_size",
    );
    assert!(
        styles_lines(&stderr, "nostyles.docx").is_empty(),
        "an empty styles part is usable:\n{stderr}"
    );
    assert!(
        !styles_lines(&stderr, "single.docx")[0].contains("would take the document past"),
        "a part over the cap on its own gets the per-part line, not the total's"
    );
    assert!(
        !styles_lines(&stderr, "sum.docx")[0].contains("exceeds"),
        "a part under the cap that the total refuses gets the total's line"
    );
    assert!(
        !stderr.contains("extracted text is truncated here")
            && !stderr.contains("element(s) still open; extracted"),
        "the truncation warning of word/document.xml is not the styles part's:\n{stderr}"
    );
    assert_dir_empty(&fx.cache);
}
