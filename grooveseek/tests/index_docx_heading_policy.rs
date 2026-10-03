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
    use crate::common::embed_mock::hermetic;
    use crate::common::temp::TempRoot;
    use grooveseek::config::Config;
    use grooveseek::db::{ContextMode, Database};
    use grooveseek::embedder::Embedder;
    use grooveseek::indexer::progress::{
        CancelToken, ProgressEvent, ProgressMode, ProgressReporter,
    };
    use grooveseek::indexer::{
        IndexResult, SingleResult, load_declared_schema, rebuild_index, reindex_single_file,
    };
    use rusqlite::{Connection, OptionalExtension};
    use sha2::{Digest, Sha256};
    use std::process::Command;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

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

    /// Overwrite the stored content hash of `rel` with `hash`.
    fn set_content_hash(fx: &Fixture, rel: &str, hash: &str) {
        db(fx)
            .execute(
                "UPDATE documents SET content_hash = ?1 WHERE path = ?2",
                [hash, rel],
            )
            .expect("rewrite content_hash");
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

    /// The "path" field of every hit.
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

    /// Set on the child [`run_in_hermetic_child`] starts, so the child runs the test body.
    const HERMETIC_CHILD: &str = "GROOVE_F62_HERMETIC_CHILD";

    /// Run the test `name` (with its module path) again in a child of this test binary under
    /// the environment [`crate::common::embed_mock::hermetic`] pins, the way the cancellation
    /// tests do; a copy of their helper, since moving it would edit those tests (D7). `true` in
    /// the parent, after the child passed exactly one test; `false` in the child.
    fn run_in_hermetic_child(name: &str) -> bool {
        if std::env::var_os(HERMETIC_CHILD).is_some() {
            return false;
        }
        let cache = TempRoot::new("groove-f62-fastembed");
        let mut cmd = Command::new(std::env::current_exe().expect("this test binary"));
        cmd.args([name, "--exact", "--nocapture", "--test-threads=1"])
            .env(HERMETIC_CHILD, "1");
        hermetic(&mut cmd, cache.path());
        let out = cmd.output().expect("run the test in a child");
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            out.status.success(),
            "{name} failed in the child:\n{stdout}\n{stderr}"
        );
        assert!(
            stdout.contains("test result: ok. 1 passed"),
            "the child ran no test named {name}:\n{stdout}\n{stderr}"
        );
        assert_dir_empty(cache.path());
        true
    }

    /// The index and the embedder under `cfg`, opened the way `groove index` opens them.
    fn open_in_process(fx: &Fixture, cfg: &Config) -> (Database, Embedder) {
        let embedding = cfg.resolve_embedding(None).expect("resolve [embedding]");
        let db_path = grooveseek::resolve_db_path(fx.kb());
        let db = Database::open(&db_path.to_string_lossy()).expect("open the index");
        db.verify_embedding_meta(embedding.model_id(), embedding.dimension() as u32)
            .expect("embedding meta");
        let embedder = Embedder::with_settings(embedding).expect("build the embedder");
        (db, embedder)
    }

    /// [`grooveseek::indexer::rebuild_index`] over `fx` under its `groove.toml`, wired the way
    /// `groove index` wires it, reporting to the reporter it is handed.
    fn rebuild_in_process(fx: &Fixture, progress: ProgressReporter) -> anyhow::Result<IndexResult> {
        let kb = fx.kb();
        let cfg = Config::load_from(&fx.config).expect("load groove.toml");
        let registry = cfg.build_parser_registry(kb).expect("parser registry");
        let schema = load_declared_schema(kb).expect("groove-schema.toml");
        let (db, mut embedder) = open_in_process(fx, &cfg);
        rebuild_index(
            &db,
            &mut embedder,
            kb,
            schema,
            false,
            cfg.exclude_headings.as_deref(),
            &cfg.resolve_exclude_dirs(),
            &registry,
            progress,
            ContextMode::Off,
        )
    }

    /// `len` bytes deflate cannot shrink, so a part made of them keeps a docx's file size up.
    fn incompressible(len: usize) -> Vec<u8> {
        let mut state: u32 = 0x9E37_79B9;
        (0..len)
            .map(|_| {
                state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                (state >> 24) as u8
            })
            .collect()
    }

    /// feature-62 I6 (AC30, I-4): a cancelled run records no policy and marks no row, though a
    /// document it could not parse and one the scan skipped were both met before the stop;
    /// the next run marks both and records the policy. A run that wrote the placeholder inside
    /// the loop or right after the scan would fail the first half. Stopped after the second
    /// document event, by the check before the third document (C3).
    #[test]
    fn a_cancelled_run_leaves_the_docx_policy_and_hashes_untouched() {
        if run_in_hermetic_child(
            "reread_pass::a_cancelled_run_leaves_the_docx_policy_and_hashes_untouched",
        ) {
            return;
        }
        a_cancelled_run_records_nothing("groove-f62-i6", 2);
    }

    /// feature-62 I6 at C4 -- a pin stronger than spec AC30, which stops the run inside the
    /// loop only: stopped after the third and last document event, the run passes no check
    /// inside the loop and stops at the one after it (C4), before the deletion sweep. A run that
    /// recorded the policy or the placeholders after the loop but before that check would fail.
    #[test]
    fn a_run_cancelled_after_its_last_document_records_nothing_either() {
        if run_in_hermetic_child(
            "reread_pass::a_run_cancelled_after_its_last_document_records_nothing_either",
        ) {
            return;
        }
        a_cancelled_run_records_nothing("groove-f62-i6c4", 3);
    }

    /// I6: a knowledge base of (i) two readable `.docx`, (ii) one that walks first and fails to
    /// parse under the run's budget and (iii) one the scan skips under the run's file cap,
    /// first indexed under the default caps; then a run under the lower caps whose callback
    /// sets the cancel token after its `cancel_at`-th document event (the documents report, in
    /// walk order, a-bloat as unchanged and b-one and c-two as indexed); then an ordinary run.
    fn a_cancelled_run_records_nothing(prefix: &str, cancel_at: usize) {
        const BIN_CAP: usize = 65_536;
        const DEC_CAP: usize = 16_384;
        let fx = docx_kb(prefix);

        // (ii) Walked first, and over the run's budget on word/document.xml alone.
        let bloat_doc = pad_to(
            &document_xml(&[(Some("1"), H_FIRST), (None, B_FIRST)]),
            "</w:body>",
            DEC_CAP + 1,
        );
        let bloat = docx(&[("word/document.xml", bloat_doc.as_bytes())]);
        // (i) Two documents that fit both of the run's caps.
        let one = numeric_heading_docx("Ledger", &[]);
        let two = numeric_heading_docx("Vellum", &[]);
        // (iii) Over the run's file cap: a picture no part reads, too random to compress.
        let picture = incompressible(BIN_CAP + 1);
        let huge = numeric_heading_docx("Quarto", &[("word/media/image1.bin", picture.as_slice())]);
        let whole = numeric_heading_document_xml().len()
            + styles_xml(&word2010_ja_styles()).len()
            + core_xml("Vellum").len();
        assert!(
            whole <= DEC_CAP && one.len() < BIN_CAP && two.len() < BIN_CAP,
            "fixture: (i) fits both caps"
        );
        assert!(
            bloat.len() < BIN_CAP && huge.len() > BIN_CAP,
            "fixture: (ii) and (iii)"
        );
        write_bytes(&fx, "a-bloat.docx", &bloat);
        write_bytes(&fx, "b-one.docx", &one);
        write_bytes(&fx, "c-two.docx", &two);
        write_bytes(&fx, "z-huge.docx", &huge);

        rebuild_in_process(&fx, ProgressReporter::new(ProgressMode::Quiet))
            .expect("the first run, under the default caps");
        let (bloat_hash, huge_hash) = (sha256_hex(&bloat), sha256_hex(&huge));
        assert_eq!(content_hash(&fx, "a-bloat.docx"), Some(bloat_hash.clone()));
        assert_eq!(content_hash(&fx, "z-huge.docx"), Some(huge_hash.clone()));

        delete_meta(&fx, POLICY_KEY);
        set_headings(&fx, "b-one.docx", "Rewritten");
        set_headings(&fx, "c-two.docx", "Rewritten");
        let caps = format!(
            "[index]\nmax_binary_file_size = {BIN_CAP}\nmax_decompressed_size = {DEC_CAP}\n"
        );
        configure(&fx, "", MD_AND_DOCX, &caps);

        // Stop after the `cancel_at`-th document event: 2 stops at the check before c-two.docx
        // (C3), 3 at the check after the loop (C4).
        let token = CancelToken::new();
        let seen = Arc::new(AtomicUsize::new(0));
        let (stop, count) = (token.clone(), Arc::clone(&seen));
        let reporter = ProgressReporter::with_callback(Box::new(move |ev| {
            let document = matches!(
                ev,
                ProgressEvent::Indexed { .. } | ProgressEvent::Unchanged { .. }
            );
            if document && count.fetch_add(1, Ordering::SeqCst) + 1 == cancel_at {
                stop.cancel();
            }
        }))
        .with_cancel(token);
        let result = rebuild_in_process(&fx, reporter).expect("a cancelled run returns Ok");
        assert!(result.cancelled, "{result:?}");
        assert_eq!(
            meta(&fx, POLICY_KEY),
            None,
            "a cancelled run records no policy"
        );
        assert_eq!(
            content_hash(&fx, "a-bloat.docx"),
            Some(bloat_hash),
            "nothing is marked before the sweep"
        );
        assert_eq!(content_hash(&fx, "z-huge.docx"), Some(huge_hash));
        if cancel_at < 3 {
            assert_eq!(
                headings(&fx, "c-two.docx")[1].as_deref(),
                Some("Rewritten"),
                "C3 stops inside the loop, before c-two.docx is re-read"
            );
        }

        let result = rebuild_in_process(&fx, ProgressReporter::new(ProgressMode::Quiet))
            .expect("the next run");
        assert!(!result.cancelled, "{result:?}");
        assert_eq!(meta(&fx, POLICY_KEY).as_deref(), Some(POLICY));
        assert_eq!(content_hash(&fx, "a-bloat.docx").as_deref(), Some(AWAITING));
        assert_eq!(content_hash(&fx, "z-huge.docx").as_deref(), Some(AWAITING));
        assert_eq!(
            headings(&fx, "c-two.docx"),
            expected_headings(),
            "the document the stop came before is settled now"
        );
    }

    /// feature-62 I6 at C1 (spec I-4, AC30): a run stopped by a token set from the first scan
    /// report returns through the early branch after the scan, before the list of unsettled
    /// documents is built or the final transaction runs, and so records neither the policy
    /// nor a placeholder. A run that wrote either ahead of that branch would fail here.
    #[test]
    fn a_run_cancelled_during_the_scan_records_no_docx_settlement() {
        if run_in_hermetic_child(
            "reread_pass::a_run_cancelled_during_the_scan_records_no_docx_settlement",
        ) {
            return;
        }
        a_run_cancelled_at_the_scan_records_nothing("groove-f62-c1", false);
    }

    /// feature-62 I6 at C2 (spec I-4, AC30): a run stopped by a token set from the last scan
    /// report, after every file was scanned and before the first document. From a callback
    /// this reaches the C2 check through the observer's stop at the last file -- the same early
    /// branch C2 returns through, as the cancellation tests of feature-60 note -- so the whole
    /// scan, the skipped file included, is behind it. A run that settled the docx rows between
    /// the scan and the first document would fail here.
    #[test]
    fn a_run_cancelled_after_the_scan_records_no_docx_settlement() {
        if run_in_hermetic_child(
            "reread_pass::a_run_cancelled_after_the_scan_records_no_docx_settlement",
        ) {
            return;
        }
        a_run_cancelled_at_the_scan_records_nothing("groove-f62-c2", true);
    }

    /// The knowledge base of the C3 test (one docx that fails to parse under the run's
    /// budget, one the scan skips under the run's file cap, two readable ones), first indexed
    /// under the default caps, the policy then removed; then a run under the lower caps whose
    /// callback sets the cancel token from a scan report -- the last one when at_last_file is
    /// set, the first otherwise; then an ordinary run, which shows the stopped run had rows
    /// to mark.
    fn a_run_cancelled_at_the_scan_records_nothing(prefix: &str, at_last_file: bool) {
        const BIN_CAP: usize = 65_536;
        const DEC_CAP: usize = 16_384;
        const DOCS: [&str; 4] = ["a-bloat.docx", "b-one.docx", "c-two.docx", "z-huge.docx"];
        let fx = docx_kb(prefix);

        let bloat_doc = pad_to(
            &document_xml(&[(Some("1"), H_FIRST), (None, B_FIRST)]),
            "</w:body>",
            DEC_CAP + 1,
        );
        let bloat = docx(&[("word/document.xml", bloat_doc.as_bytes())]);
        let one = numeric_heading_docx("Ledger", &[]);
        let two = numeric_heading_docx("Vellum", &[]);
        let picture = incompressible(BIN_CAP + 1);
        let huge = numeric_heading_docx("Quarto", &[("word/media/image1.bin", picture.as_slice())]);
        assert!(
            bloat.len() < BIN_CAP && huge.len() > BIN_CAP,
            "fixture: one docx under the file cap, one over it"
        );
        write_bytes(&fx, "a-bloat.docx", &bloat);
        write_bytes(&fx, "b-one.docx", &one);
        write_bytes(&fx, "c-two.docx", &two);
        write_bytes(&fx, "z-huge.docx", &huge);

        rebuild_in_process(&fx, ProgressReporter::new(ProgressMode::Quiet))
            .expect("the first run, under the default caps");
        let before: Vec<Option<String>> = DOCS.iter().map(|rel| content_hash(&fx, rel)).collect();
        assert_eq!(before[0], Some(sha256_hex(&bloat)));
        assert_eq!(before[3], Some(sha256_hex(&huge)));
        for (rel, hash) in DOCS.iter().zip(&before) {
            assert!(
                hash.is_some() && hash.as_deref() != Some(AWAITING),
                "fixture: {rel} holds its sha256 before the stopped run: {hash:?}"
            );
        }

        delete_meta(&fx, POLICY_KEY);
        let caps = format!(
            "[index]\nmax_binary_file_size = {BIN_CAP}\nmax_decompressed_size = {DEC_CAP}\n"
        );
        configure(&fx, "", MD_AND_DOCX, &caps);

        let token = CancelToken::new();
        let scans = Arc::new(AtomicUsize::new(0));
        let scan_total = Arc::new(AtomicUsize::new(0));
        let documents = Arc::new(AtomicUsize::new(0));
        let stop = token.clone();
        let (seen_scans, seen_total, seen_documents) = (
            Arc::clone(&scans),
            Arc::clone(&scan_total),
            Arc::clone(&documents),
        );
        let reporter = ProgressReporter::with_callback(Box::new(move |ev| match ev {
            ProgressEvent::Scanning { done, total } => {
                seen_scans.fetch_add(1, Ordering::SeqCst);
                seen_total.store(total, Ordering::SeqCst);
                let last = done == total;
                if (at_last_file && last) || (!at_last_file && done == 1) {
                    stop.cancel();
                }
            }
            ProgressEvent::Indexed { .. } | ProgressEvent::Unchanged { .. } => {
                seen_documents.fetch_add(1, Ordering::SeqCst);
            }
            _ => {}
        }))
        .with_cancel(token);
        let result = rebuild_in_process(&fx, reporter).expect("a cancelled run returns Ok");
        assert!(result.cancelled, "{result:?}");

        let (scanned, total) = (
            scans.load(Ordering::SeqCst),
            scan_total.load(Ordering::SeqCst),
        );
        assert!(total >= DOCS.len(), "the scan reports every docx: {total}");
        if at_last_file {
            assert_eq!(
                scanned, total,
                "the token is set after the last scan report"
            );
        } else {
            assert_eq!(scanned, 1, "the scan stops at the first file");
        }
        assert_eq!(
            documents.load(Ordering::SeqCst),
            0,
            "the run stops before its first document"
        );
        assert_eq!(
            meta(&fx, POLICY_KEY),
            None,
            "a run stopped at the scan records no policy"
        );
        for (rel, hash) in DOCS.iter().zip(&before) {
            assert_eq!(
                &content_hash(&fx, rel),
                hash,
                "a run stopped at the scan leaves {rel}'s hash as it was"
            );
        }
        let awaiting: i64 = db(&fx)
            .query_row(
                "SELECT COUNT(*) FROM documents WHERE content_hash = ?1",
                [AWAITING],
                |r| r.get(0),
            )
            .expect("count placeholder rows");
        assert_eq!(awaiting, 0, "no row carries the placeholder");

        let result = rebuild_in_process(&fx, ProgressReporter::new(ProgressMode::Quiet))
            .expect("the next run");
        assert!(!result.cancelled, "{result:?}");
        assert_eq!(meta(&fx, POLICY_KEY).as_deref(), Some(POLICY));
        assert_eq!(content_hash(&fx, "a-bloat.docx").as_deref(), Some(AWAITING));
        assert_eq!(content_hash(&fx, "z-huge.docx").as_deref(), Some(AWAITING));
    }

    /// feature-62 I7 (AC31): the shape desktop sees -- a callback reporter, `force=false` --
    /// hears `Indexed` for a re-split document and `Unchanged` for one that matched.
    #[test]
    fn a_resplit_docx_reports_indexed_and_a_matching_one_reports_unchanged() {
        if run_in_hermetic_child(
            "reread_pass::a_resplit_docx_reports_indexed_and_a_matching_one_reports_unchanged",
        ) {
            return;
        }
        let fx = docx_kb("groove-f62-i7");
        write_bytes(&fx, "quarry.docx", &numeric_heading_docx(TITLE, &[]));
        write_bytes(&fx, "vellum.docx", &numeric_heading_docx("Vellum", &[]));
        rebuild_in_process(&fx, ProgressReporter::new(ProgressMode::Quiet)).expect("first run");
        set_headings(&fx, "quarry.docx", "Rewritten");
        delete_meta(&fx, POLICY_KEY);

        let log: Arc<Mutex<Vec<(String, &'static str)>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&log);
        let reporter = ProgressReporter::with_callback(Box::new(move |ev| {
            let seen = match ev {
                ProgressEvent::Indexed { rel, .. } => Some((rel.to_string(), "indexed")),
                ProgressEvent::Unchanged { rel, .. } => Some((rel.to_string(), "unchanged")),
                _ => None,
            };
            if let Some(seen) = seen {
                sink.lock().expect("log").push(seen);
            }
        }));
        rebuild_in_process(&fx, reporter).expect("second run");
        assert_eq!(
            *log.lock().expect("log"),
            vec![
                ("quarry.docx".to_string(), "indexed"),
                ("vellum.docx".to_string(), "unchanged"),
            ]
        );
    }

    /// feature-62 I9 (AC33, J13): the watcher reads a changed docx by the new rule and runs no
    /// pass over an unchanged one, even with the policy absent.
    #[test]
    fn the_watcher_splits_a_changed_docx_and_leaves_an_unchanged_one_alone() {
        if run_in_hermetic_child(
            "reread_pass::the_watcher_splits_a_changed_docx_and_leaves_an_unchanged_one_alone",
        ) {
            return;
        }
        let fx = docx_kb("groove-f62-i9");
        write_bytes(&fx, "quarry.docx", &numeric_heading_docx(TITLE, &[]));
        let draft = document_xml(&[(None, PREFACE)]);
        write_bytes(
            &fx,
            "draft.docx",
            &docx(&[("word/document.xml", draft.as_bytes())]),
        );
        rebuild_in_process(&fx, ProgressReporter::new(ProgressMode::Quiet)).expect("first run");

        let cfg = Config::load_from(&fx.config).expect("load groove.toml");
        let registry = cfg.build_parser_registry(fx.kb()).expect("parser registry");
        let kb = fx.kb().canonicalize().expect("canonical kb");
        let (db, mut embedder) = open_in_process(&fx, &cfg);

        write_bytes(&fx, "draft.docx", &numeric_heading_docx("Draft", &[]));
        let changed = reindex_single_file(&db, &mut embedder, &kb, "draft.docx", None, &registry)
            .expect("reindex the changed document");
        assert_eq!(
            changed,
            SingleResult::Updated {
                chunks: 4,
                frontmatter_unparsed: false
            }
        );

        set_headings(&fx, "quarry.docx", "Rewritten");
        delete_meta(&fx, POLICY_KEY);
        let unchanged =
            reindex_single_file(&db, &mut embedder, &kb, "quarry.docx", None, &registry)
                .expect("reindex the unchanged document");
        assert_eq!(unchanged, SingleResult::Unchanged);
        assert_eq!(meta(&fx, POLICY_KEY), None, "the watcher records no policy");
        assert_eq!(
            headings(&fx, "quarry.docx")[1].as_deref(),
            Some("Rewritten")
        );
    }

    /// feature-62 spec R4.4 / I-2 on the daemon path: a row the pass could not settle carries
    /// the placeholder instead of its hash, so the watcher's
    /// [`grooveseek::indexer::reindex_single_file`] cannot take the unchanged fast path for it.
    /// The watcher parses the file, re-embeds it and writes its real SHA-256 back, and records
    /// no policy key. A fast path that trusted any stored hash would return unchanged here and
    /// leave the placeholder and the rewritten headings behind.
    #[test]
    fn the_watcher_settles_a_docx_row_that_awaits_its_reparse() {
        if run_in_hermetic_child(
            "reread_pass::the_watcher_settles_a_docx_row_that_awaits_its_reparse",
        ) {
            return;
        }
        let fx = docx_kb("groove-f62-i9r");
        let bytes = numeric_heading_docx(TITLE, &[]);
        write_bytes(&fx, "quarry.docx", &bytes);
        rebuild_in_process(&fx, ProgressReporter::new(ProgressMode::Quiet)).expect("first run");
        assert_eq!(content_hash(&fx, "quarry.docx"), Some(sha256_hex(&bytes)));
        delete_meta(&fx, POLICY_KEY);

        set_content_hash(&fx, "quarry.docx", AWAITING);
        set_headings(&fx, "quarry.docx", "Rewritten");
        assert_eq!(
            content_hash(&fx, "quarry.docx"),
            Some(AWAITING.to_string()),
            "fixture: the row awaits its reparse"
        );

        let cfg = Config::load_from(&fx.config).expect("load groove.toml");
        let registry = cfg.build_parser_registry(fx.kb()).expect("parser registry");
        let kb = fx.kb().canonicalize().expect("canonical kb");
        let (db, mut embedder) = open_in_process(&fx, &cfg);
        let settled = reindex_single_file(&db, &mut embedder, &kb, "quarry.docx", None, &registry)
            .expect("reindex the marked document");
        assert_eq!(
            settled,
            SingleResult::Updated {
                chunks: 4,
                frontmatter_unparsed: false
            },
            "a marked row bypasses the unchanged fast path"
        );
        assert_eq!(
            content_hash(&fx, "quarry.docx"),
            Some(sha256_hex(&bytes)),
            "the watcher writes the real hash back"
        );
        assert_eq!(headings(&fx, "quarry.docx"), expected_headings());
        assert_eq!(meta(&fx, POLICY_KEY), None, "the watcher records no policy");
    }

    /// feature-62 spec I-1 / the single transaction of R4.4: the placeholder over the rows the
    /// pass could not settle and the policy key land together or not at all. A trigger on the
    /// index makes the write of the key fail after the placeholder was staged, so
    /// [`grooveseek::indexer::rebuild_index`] returns an error; the key is absent and every row
    /// keeps its own hash and chunks. Once the trigger is dropped, the next run records the
    /// key and the placeholder together. A run that committed between the two writes would
    /// leave the placeholder behind and fail the first half.
    #[test]
    fn a_failed_final_settlement_leaves_neither_the_marks_nor_the_key() {
        if run_in_hermetic_child(
            "reread_pass::a_failed_final_settlement_leaves_neither_the_marks_nor_the_key",
        ) {
            return;
        }
        const DEC_CAP: usize = 16_384;
        const TRIGGER: &str = "CREATE TRIGGER inject_policy_failure BEFORE INSERT ON index_meta WHEN NEW.key = 'docx_heading_policy' BEGIN SELECT RAISE(ABORT, 'injected policy failure'); END;";
        let fx = docx_kb("groove-f62-i1tx");

        // Over the second run's budget on word/document.xml alone, so that run cannot settle it.
        let bloat_doc = pad_to(
            &document_xml(&[(Some("1"), H_FIRST), (None, B_FIRST)]),
            "</w:body>",
            DEC_CAP + 1,
        );
        let bloat = docx(&[("word/document.xml", bloat_doc.as_bytes())]);
        // Fits the second run's budget, so that run reads it and finds its sections unchanged.
        let tidy = numeric_heading_docx("Ledger", &[]);
        write_bytes(&fx, "a-bloat.docx", &bloat);
        write_bytes(&fx, "b-tidy.docx", &tidy);

        rebuild_in_process(&fx, ProgressReporter::new(ProgressMode::Quiet))
            .expect("the first run, under the default caps");
        let (bloat_hash, tidy_hash) = (sha256_hex(&bloat), sha256_hex(&tidy));
        assert_eq!(content_hash(&fx, "a-bloat.docx"), Some(bloat_hash.clone()));
        assert_eq!(content_hash(&fx, "b-tidy.docx"), Some(tidy_hash.clone()));
        let (bloat_chunks, tidy_chunks) = (
            column(&fx, "a-bloat.docx", "content"),
            column(&fx, "b-tidy.docx", "content"),
        );
        assert!(!bloat_chunks.is_empty() && !tidy_chunks.is_empty());
        delete_meta(&fx, POLICY_KEY);
        configure(
            &fx,
            "",
            MD_AND_DOCX,
            &format!("[index]\nmax_decompressed_size = {DEC_CAP}\n"),
        );

        // The key is written with INSERT OR REPLACE; a BEFORE INSERT trigger fires for it.
        {
            let conn = db(&fx);
            conn.execute_batch(TRIGGER).expect("create the trigger");
            let probe = conn
                .execute(
                    "INSERT OR REPLACE INTO index_meta (key, value) VALUES ('docx_heading_policy', 'probe')",
                    [],
                )
                .expect_err("the trigger stops INSERT OR REPLACE");
            assert!(
                probe.to_string().contains("injected policy failure"),
                "{probe}"
            );
        }
        assert_eq!(meta(&fx, POLICY_KEY), None, "the probe left no key");

        let err = rebuild_in_process(&fx, ProgressReporter::new(ProgressMode::Quiet))
            .expect_err("the run whose key cannot be written fails");
        let chain = format!("{err:#}");
        assert!(chain.contains("injected policy failure"), "{chain}");
        assert_eq!(meta(&fx, POLICY_KEY), None, "the failed run records no key");
        let after = content_hash(&fx, "a-bloat.docx");
        assert_ne!(after.as_deref(), Some(AWAITING), "the mark was rolled back");
        assert_eq!(
            after,
            Some(bloat_hash),
            "the unsettled row keeps its own hash"
        );
        assert_eq!(column(&fx, "a-bloat.docx", "content"), bloat_chunks);
        assert_eq!(content_hash(&fx, "b-tidy.docx"), Some(tidy_hash.clone()));
        assert_eq!(column(&fx, "b-tidy.docx", "content"), tidy_chunks);
        assert!(has_row(&fx, "a-bloat.docx") && has_row(&fx, "b-tidy.docx"));

        db(&fx)
            .execute_batch("DROP TRIGGER inject_policy_failure;")
            .expect("drop the trigger");
        rebuild_in_process(&fx, ProgressReporter::new(ProgressMode::Quiet))
            .expect("the run after the trigger is gone");
        assert_eq!(meta(&fx, POLICY_KEY).as_deref(), Some(POLICY));
        assert_eq!(content_hash(&fx, "a-bloat.docx").as_deref(), Some(AWAITING));
        assert_eq!(content_hash(&fx, "b-tidy.docx"), Some(tidy_hash));
    }
}
