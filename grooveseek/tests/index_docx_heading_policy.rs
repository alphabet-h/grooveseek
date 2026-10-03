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
    // Well-formed up to a style whose start tag names `w:type` twice: the reader passes it,
    // and the attribute iterator, not the reader, reports the XML error.
    let twin = styles_xml(&format!(
        r#"{}<w:style w:type="paragraph" w:type="paragraph" w:styleId="b"><w:name w:val="Quote"/></w:style>"#,
        word2010_ja_styles()
    ));
    assert!(
        twin.len() < CAP,
        "fixture: the twin styles part fits the cap"
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
        ("twin.docx", doc.clone(), twin.clone()),
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
    expect_one("twin.docx", "is not a readable styles part (XML error:");
    assert!(
        stderr.contains("indexed: twin.docx (1 chunks)"),
        "an unread styles part leaves style 1 to the spelling, which is not a heading:\n{stderr}"
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

/// feature-62 PR-B: an index whose `.docx` rows were split by the style-ID spelling re-reads
/// each unchanged `.docx` once, through `groove index`, and records that it has (ADR-0028).
/// "Rewriting" a row here means an UPDATE of `chunks.heading` or `chunks.context_text` in the
/// index: the embeddings and full-text rows are left as they are, so a run that does not
/// re-read the document leaves the rewrite visible. Every test removes the policy key itself
/// before the run it is about, so it holds whether or not an earlier run recorded the key.
mod reread_pass {
    use super::*;
    use crate::common::embed_cli::note;
    use crate::common::embed_mock::DOC_MODEL;
    use rusqlite::{Connection, OptionalExtension};
    use sha2::{Digest, Sha256};

    const POLICY_KEY: &str = "docx_heading_policy";
    const NOTICE: &str = "Re-reading 1 unchanged .docx document(s) once:";
    const POLICY: &str = "styles-name-basedon";
    /// What a row the pass could not settle carries instead of its hash.
    const AWAITING: &str = "awaiting-reparse";
    /// A top-level `exclude_headings` naming every heading of the AC1 document.
    const EXCLUDE_ALL: &str = "exclude_headings = [\"Harbor\", \"Lantern\", \"Orchard\"]\n";

    fn db(fx: &Fixture) -> Connection {
        Connection::open(fx.layout.root().join(".groove.db")).expect("open the index")
    }

    fn delete_meta(fx: &Fixture, key: &str) {
        db(fx)
            .execute("DELETE FROM index_meta WHERE key = ?1", [key])
            .expect("delete an index_meta key");
    }

    /// One text column of `rel`'s chunks, in chunk order.
    fn column(fx: &Fixture, rel: &str, name: &str) -> Vec<Option<String>> {
        let conn = db(fx);
        let mut stmt = conn
            .prepare(&format!(
                "SELECT c.{name} FROM chunks c JOIN documents d ON d.id = c.document_id WHERE d.path = ?1 ORDER BY c.chunk_index"
            ))
            .expect("prepare");
        stmt.query_map([rel], |r| r.get(0))
            .expect("query")
            .collect::<Result<_, _>>()
            .expect("rows")
    }

    /// Rewrite the context of every chunk of `rel` to `context`.
    fn set_contexts(fx: &Fixture, rel: &str, context: &str) {
        db(fx)
            .execute(
                "UPDATE chunks SET context_text = ?1 WHERE document_id = (SELECT id FROM documents WHERE path = ?2)",
                [context, rel],
            )
            .expect("rewrite contexts");
    }

    fn contexts(fx: &Fixture, rel: &str) -> Vec<Option<String>> {
        column(fx, rel, "context_text")
    }

    /// Rewrite every non-empty heading of `rel`'s chunks to `heading`.
    fn set_headings(fx: &Fixture, rel: &str, heading: &str) {
        db(fx)
            .execute(
                "UPDATE chunks SET heading = ?1 WHERE heading IS NOT NULL AND document_id = (SELECT id FROM documents WHERE path = ?2)",
                [heading, rel],
            )
            .expect("rewrite headings");
    }

    fn headings(fx: &Fixture, rel: &str) -> Vec<Option<String>> {
        column(fx, rel, "heading")
    }

    /// The headings of the AC1 document's chunks, preface first.
    fn expected_headings() -> Vec<Option<String>> {
        vec![
            None,
            Some(H_FIRST.to_string()),
            Some(H_NESTED.to_string()),
            Some(H_SECOND.to_string()),
        ]
    }

    fn content_hash(fx: &Fixture, rel: &str) -> Option<String> {
        db(fx)
            .query_row(
                "SELECT content_hash FROM documents WHERE path = ?1",
                [rel],
                |r| r.get(0),
            )
            .optional()
            .expect("read content_hash")
    }

    fn sha256_hex(bytes: &[u8]) -> String {
        format!("{:x}", Sha256::digest(bytes))
    }

    fn meta(fx: &Fixture, key: &str) -> Option<String> {
        db(fx)
            .query_row("SELECT value FROM index_meta WHERE key = ?1", [key], |r| {
                r.get(0)
            })
            .optional()
            .expect("read index_meta")
    }

    fn has_row(fx: &Fixture, rel: &str) -> bool {
        content_hash(fx, rel).is_some()
    }

    /// The `path` of every hit.
    fn hit_paths(hits: &serde_json::Value) -> Vec<String> {
        hits.get("results")
            .and_then(|r| r.as_array())
            .unwrap_or_else(|| panic!("search JSON has no results array: {hits}"))
            .iter()
            .filter_map(|h| h.get("path").and_then(|p| p.as_str()).map(str::to_owned))
            .collect()
    }

    /// A Markdown note beside the docx, which the pass must not touch.
    fn write_meadow(fx: &Fixture) {
        fx.layout.write(
            "meadow.md",
            &note("Meadow", "fern moss lichen growing beside the stream bank"),
        );
    }

    /// feature-62 I1s (AC23): under Static mode a changed context alone makes the re-read
    /// rewrite the document.
    #[test]
    fn a_static_index_compares_the_context_when_it_rereads_a_docx() {
        let fx = docx_kb("groove-f62-i1s");
        configure(&fx, "", MD_AND_DOCX, "[contextual]\nenabled = true\n");
        write_bytes(&fx, "quarry.docx", &numeric_heading_docx(TITLE, &[]));
        index_stderr(&fx);
        let written = contexts(&fx, "quarry.docx");
        set_contexts(&fx, "quarry.docx", "Rewritten context");
        delete_meta(&fx, POLICY_KEY);

        let second = index_stderr(&fx);
        assert!(second.contains("(1 updated, "), "{second}");
        assert_eq!(contexts(&fx, "quarry.docx"), written);
    }

    /// I2 / I2s: a re-read whose sections match is announced, counted as nothing, embeds
    /// nothing, and leaves the real hash.
    fn a_matching_reread_embeds_nothing(prefix: &str, tables: &str) {
        let fx = docx_kb(prefix);
        configure(&fx, "", MD_AND_DOCX, tables);
        let bytes = numeric_heading_docx(TITLE, &[]);
        write_bytes(&fx, "quarry.docx", &bytes);
        index_stderr(&fx);
        delete_meta(&fx, POLICY_KEY);

        let before = fx.mock.requests().len();
        let second = index_stderr(&fx);
        assert!(second.contains(NOTICE), "{second}");
        assert!(second.contains("(0 updated, "), "{second}");
        assert!(
            fx.requests_since(before)
                .iter()
                .all(|r| r.model() != Some(DOC_MODEL)),
            "nothing was re-embedded"
        );
        assert_eq!(meta(&fx, POLICY_KEY).as_deref(), Some(POLICY));
        assert_eq!(
            content_hash(&fx, "quarry.docx"),
            Some(sha256_hex(&bytes)),
            "the hash is the bytes', not a placeholder"
        );
        assert_dir_empty(&fx.cache);
    }

    /// feature-62 I2 (AC22): Off mode.
    #[test]
    fn an_unchanged_docx_whose_sections_match_is_not_re_embedded() {
        a_matching_reread_embeds_nothing("groove-f62-i2", "");
    }

    /// feature-62 I2s (AC22): Static mode, whose comparison includes the context.
    #[test]
    fn an_unchanged_docx_whose_sections_match_is_not_re_embedded_in_static_mode() {
        a_matching_reread_embeds_nothing("groove-f62-i2s", "[contextual]\nenabled = true\n");
    }

    /// feature-62 (R4.6, J25, Review Focus 3): `--quiet` promises start, `Found` and
    /// `Done in` lines, so the notice stays off; the pass re-reads all the same.
    #[test]
    fn the_reread_notice_stays_off_under_quiet() {
        let fx = docx_kb("groove-f62-quiet");
        write_bytes(&fx, "quarry.docx", &numeric_heading_docx(TITLE, &[]));
        index_stderr(&fx);
        set_headings(&fx, "quarry.docx", "Rewritten");
        delete_meta(&fx, POLICY_KEY);

        let out = fx
            .cmd()
            .args(["index", "--quiet", "--kb-path"])
            .arg(fx.kb())
            .output()
            .expect("spawn groove index --quiet");
        let stderr = stderr_of(&out);
        assert!(out.status.success(), "{stderr}");
        assert!(!stderr.contains("Re-reading"), "{stderr}");
        assert_eq!(
            headings(&fx, "quarry.docx"),
            expected_headings(),
            "the pass ran under --quiet too"
        );
    }

    /// feature-62 I1 (AC21, I-3): the first run re-splits an unchanged docx once and leaves
    /// Markdown alone; the run after it does not re-read again.
    #[test]
    fn the_first_run_after_upgrade_resplits_unchanged_docx_once() {
        let fx = docx_kb("groove-f62-i1");
        write_bytes(&fx, "quarry.docx", &numeric_heading_docx(TITLE, &[]));
        write_meadow(&fx);
        index_stderr(&fx);
        set_headings(&fx, "quarry.docx", "Rewritten");
        set_headings(&fx, "meadow.md", "Rewritten");
        delete_meta(&fx, POLICY_KEY);

        let second = index_stderr(&fx);
        assert!(second.contains(NOTICE), "{second}");
        assert!(second.contains("(1 updated, "), "{second}");
        assert_eq!(headings(&fx, "quarry.docx"), expected_headings());
        assert_eq!(
            headings(&fx, "meadow.md"),
            vec![Some("Rewritten".to_string())],
            "Markdown is not re-read"
        );
        assert_eq!(meta(&fx, POLICY_KEY).as_deref(), Some(POLICY));

        set_headings(&fx, "quarry.docx", "Rewritten");
        let third = index_stderr(&fx);
        assert!(!third.contains("Re-reading"), "{third}");
        assert_eq!(
            headings(&fx, "quarry.docx")[1].as_deref(),
            Some("Rewritten"),
            "the pass ran once"
        );
        assert_dir_empty(&fx.cache);
    }

    /// feature-62 I4 (AC28): `--force` records the policy without the notice.
    #[test]
    fn a_forced_run_records_the_docx_policy() {
        let fx = docx_kb("groove-f62-i4");
        write_bytes(&fx, "quarry.docx", &numeric_heading_docx(TITLE, &[]));
        index_stderr(&fx);
        delete_meta(&fx, POLICY_KEY);

        let out = fx.index_force();
        let stderr = stderr_of(&out);
        assert!(out.status.success(), "{stderr}");
        assert!(!stderr.contains("Re-reading"), "{stderr}");
        assert_eq!(meta(&fx, POLICY_KEY).as_deref(), Some(POLICY));
        let awaiting: i64 = db(&fx)
            .query_row(
                "SELECT count(*) FROM documents WHERE content_hash = ?1",
                [AWAITING],
                |r| r.get(0),
            )
            .expect("count");
        assert_eq!(awaiting, 0);
    }

    /// feature-62 I5 (AC29): a knowledge base whose parsers leave docx out records the
    /// policy on its first run.
    #[test]
    fn an_index_without_a_docx_parser_records_the_docx_policy() {
        let fx = docx_kb("groove-f62-i5");
        configure(&fx, "", r#"["md"]"#, "");
        write_bytes(&fx, "quarry.docx", &numeric_heading_docx(TITLE, &[]));
        write_meadow(&fx);
        let stderr = index_stderr(&fx);
        assert!(!stderr.contains("Re-reading"), "{stderr}");
        assert_eq!(meta(&fx, POLICY_KEY).as_deref(), Some(POLICY));
        assert!(!has_row(&fx, "quarry.docx"));
    }

    /// feature-62 I8 (AC32, J7): the frontmatter pass and the docx pass are separate.
    #[test]
    fn the_frontmatter_pass_does_not_reopen_docx() {
        let fx = docx_kb("groove-f62-i8");
        write_bytes(&fx, "quarry.docx", &numeric_heading_docx(TITLE, &[]));
        write_meadow(&fx);
        index_stderr(&fx);
        set_headings(&fx, "quarry.docx", "Rewritten");
        delete_meta(&fx, "frontmatter_policy");

        let second = index_stderr(&fx);
        assert!(!second.contains("Re-reading"), "{second}");
        assert_eq!(
            headings(&fx, "quarry.docx")[1].as_deref(),
            Some("Rewritten")
        );
    }

    /// feature-62 I10 (AC24, H1): a docx whose bytes changed takes the ordinary path, which
    /// writes its new hash, and is not counted in the notice.
    #[test]
    fn a_changed_docx_during_the_pass_takes_the_ordinary_path() {
        let fx = docx_kb("groove-f62-i10");
        write_bytes(&fx, "quarry.docx", &numeric_heading_docx(TITLE, &[]));
        index_stderr(&fx);
        delete_meta(&fx, POLICY_KEY);

        let changed =
            numeric_heading_docx(TITLE, &[("docProps/app.xml", b"<Properties/>".as_slice())]);
        write_bytes(&fx, "quarry.docx", &changed);
        let second = index_stderr(&fx);
        assert!(!second.contains("Re-reading"), "{second}");
        assert!(second.contains("(1 updated, "), "{second}");
        assert_eq!(content_hash(&fx, "quarry.docx"), Some(sha256_hex(&changed)));
        assert_eq!(meta(&fx, POLICY_KEY).as_deref(), Some(POLICY));
    }

    /// The AC1 document without its preface: every paragraph sits under a heading.
    fn headings_only_docx() -> Vec<u8> {
        let doc = document_xml(&[
            (Some("1"), H_FIRST),
            (None, B_FIRST),
            (Some("2"), H_NESTED),
            (None, B_NESTED),
            (Some("1"), H_SECOND),
            (None, B_SECOND),
        ]);
        let styles = styles_xml(&word2010_ja_styles());
        let core = core_xml(TITLE);
        docx(&[
            ("word/document.xml", doc.as_bytes()),
            ("word/styles.xml", styles.as_bytes()),
            ("docProps/core.xml", core.as_bytes()),
        ])
    }

    /// feature-62 I11 (AC25, J17, J21): a re-read that leaves nothing to index removes the
    /// row and counts a skip, not a deletion, and the next run finds it absent too.
    #[test]
    fn a_docx_the_new_rule_leaves_without_chunks_is_removed() {
        let fx = docx_kb("groove-f62-i11");
        write_bytes(&fx, "quarry.docx", &headings_only_docx());
        write_meadow(&fx);
        index_stderr(&fx);
        assert!(has_row(&fx, "quarry.docx"));

        configure(&fx, EXCLUDE_ALL, MD_AND_DOCX, "");
        delete_meta(&fx, POLICY_KEY);
        let second = index_stderr(&fx);
        assert!(!has_row(&fx, "quarry.docx"), "{second}");
        assert!(
            second.contains("(0 updated, 0 renamed, 0 deleted, 1 skipped,"),
            "{second}"
        );
        assert_eq!(meta(&fx, POLICY_KEY).as_deref(), Some(POLICY));
        let hits = fx.search_json("kettle");
        assert!(
            !hit_paths(&hits).iter().any(|p| p == "quarry.docx"),
            "an excluded section's body must not stay searchable: {hits}"
        );

        let third = index_stderr(&fx);
        assert!(!has_row(&fx, "quarry.docx"), "{third}");
        assert!(
            third.contains("(0 updated, 0 renamed, 0 deleted, 1 skipped,"),
            "{third}"
        );
    }

    /// feature-62 I11's control (AC25): with the policy recorded, the same configuration
    /// change leaves the row on the fast path, as before.
    #[test]
    fn an_excluded_docx_keeps_its_row_when_no_pass_runs() {
        let fx = docx_kb("groove-f62-i11c");
        write_bytes(&fx, "quarry.docx", &headings_only_docx());
        write_meadow(&fx);
        index_stderr(&fx);
        configure(&fx, EXCLUDE_ALL, MD_AND_DOCX, "");
        let second = index_stderr(&fx);
        assert!(has_row(&fx, "quarry.docx"), "{second}");
    }

    /// I3 / I3b: a `.docx` the pass cannot read under `capped` is settled with the awaiting
    /// hash in one run (the policy is recorded), the next run under the same cap names it again
    /// without re-running the pass, and once the cap is lifted it is read on the ordinary path
    /// and its hash is the bytes' again.
    fn unreadable_docx_settles_with_the_awaiting_hash(prefix: &str, capped: &str, says: &str) {
        let fx = docx_kb(prefix);
        let bytes = numeric_heading_docx(TITLE, &[]);
        write_bytes(&fx, "quarry.docx", &bytes);
        index_stderr(&fx);
        delete_meta(&fx, POLICY_KEY);
        let skipped = |stderr: &str| {
            stderr
                .lines()
                .any(|l| l.contains("Skipping quarry.docx:") && l.contains(says))
        };

        configure(&fx, "", MD_AND_DOCX, capped);
        let second = index_stderr(&fx);
        assert!(skipped(&second), "{second}");
        assert_eq!(
            meta(&fx, POLICY_KEY).as_deref(),
            Some(POLICY),
            "an unreadable document does not hold the pass open:\n{second}"
        );
        assert_eq!(content_hash(&fx, "quarry.docx").as_deref(), Some(AWAITING));

        let third = index_stderr(&fx);
        assert!(!third.contains("Re-reading"), "{third}");
        assert!(
            skipped(&third),
            "tried again, as a changed file would be:\n{third}"
        );
        assert_eq!(content_hash(&fx, "quarry.docx").as_deref(), Some(AWAITING));

        configure(&fx, "", MD_AND_DOCX, "");
        let fourth = index_stderr(&fx);
        assert!(!fourth.contains("Re-reading"), "{fourth}");
        assert!(fourth.contains("(1 updated, "), "{fourth}");
        assert_eq!(content_hash(&fx, "quarry.docx"), Some(sha256_hex(&bytes)));
    }

    /// feature-62 I3 (AC26): a `.docx` the scan skips for its size.
    #[test]
    fn a_docx_the_scan_skipped_is_settled_with_a_hash_awaiting_reparse() {
        unreadable_docx_settles_with_the_awaiting_hash(
            "groove-f62-i3",
            "[index]\nmax_binary_file_size = 100\n",
            "file too large",
        );
    }

    /// feature-62 I3b (AC27): a `.docx` that fails to parse.
    #[test]
    fn a_docx_that_fails_to_parse_is_settled_with_a_hash_awaiting_reparse() {
        unreadable_docx_settles_with_the_awaiting_hash(
            "groove-f62-i3b",
            "[index]\nmax_decompressed_size = 100\n",
            "parse failed",
        );
    }
}
