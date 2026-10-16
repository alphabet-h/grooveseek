//! feature-66: where each `.docx` / `.pptx` table row sits in its chunk (`chunks.table_rows`)
//! through `groove index`, and the one-time re-read that writes the column into an index
//! written before it, under the keys `docx_table_rows_policy` and `pptx_table_rows_policy`
//! beside the earlier ones (ADR-0030). The chunk text never changes, so nothing is re-embedded.
//!
//! Each test drives a knowledge base of generated files against the mock endpoint of
//! [`crate::common::embed_cli`], so nothing downloads a model. The helpers of
//! `tests/index_pptx_table_policy.rs` are private to that file, so the ones needed here are
//! repeated rather than opened up, which would edit it. The file names, the slide text and the
//! cells share no word.

mod common;

use common::embed_cli::{DIM, Fixture, fixture, stderr_of};
use common::embed_mock::{DOC_MODEL, assert_dir_empty, openai_config_toml};
use rusqlite::Connection;
use std::io::Write;
use zip::write::SimpleFileOptions;

const P_NS: &str = "http://schemas.openxmlformats.org/presentationml/2006/main";
const A_NS: &str = "http://schemas.openxmlformats.org/drawingml/2006/main";

/// `[parsers].enabled` for every test here.
const ENABLED: &str = r#"["md", "docx", "pptx"]"#;

/// A zip of `parts`, in the order given.
fn zip_of(parts: &[(&str, &[u8])]) -> Vec<u8> {
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

/// A slide whose `<p:spTree>` holds `tree`.
fn slide_xml(tree: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><p:sld xmlns:p="{P_NS}" xmlns:a="{A_NS}"><p:cSld><p:spTree>{tree}</p:spTree></p:cSld></p:sld>"#
    )
}

/// A shape holding one paragraph of `text`.
fn shape(text: &str) -> String {
    format!("<p:sp><p:txBody><a:bodyPr/><a:p><a:r><a:t>{text}</a:t></a:r></a:p></p:txBody></p:sp>")
}

/// A `<p:graphicFrame>` holding a table of `rows`, each cell one paragraph.
fn pptx_table(rows: &[&[&str]]) -> String {
    let mut body = String::new();
    for row in rows {
        body.push_str("<a:tr>");
        for text in *row {
            body.push_str(&format!(
                "<a:tc><a:txBody><a:bodyPr/><a:p><a:r><a:t>{text}</a:t></a:r></a:p></a:txBody><a:tcPr/></a:tc>"
            ));
        }
        body.push_str("</a:tr>");
    }
    format!(
        "<p:graphicFrame><a:graphic><a:graphicData><a:tbl>{body}</a:tbl></a:graphicData></a:graphic></p:graphicFrame>"
    )
}

/// Point `fx`'s `groove.toml` at its mock with [`ENABLED`], `tables` after `[parsers]`.
fn configure(fx: &Fixture, tables: &str) {
    let toml = format!(
        "{}[parsers]\nenabled = {ENABLED}\n{tables}",
        openai_config_toml(&fx.mock.endpoint(), None, DIM)
    );
    std::fs::write(&fx.config, toml).expect("write groove.toml");
}

/// An empty knowledge base, a mock, and a `groove.toml` enabling Markdown, docx and pptx.
fn kb(prefix: &str) -> Fixture {
    let fx = fixture(prefix, &[], "");
    configure(&fx, "");
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

fn db(fx: &Fixture) -> Connection {
    Connection::open(fx.layout.root().join(".groove.db")).expect("open the index")
}

/// The `name` column of `rel`'s chunks, in chunk order.
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

/// The texts the mock was asked to embed as documents since request `before`.
fn embedded_since(fx: &Fixture, before: usize) -> Vec<String> {
    fx.requests_since(before)
        .iter()
        .filter(|r| r.model() == Some(DOC_MODEL))
        .flat_map(|r| r.inputs())
        .collect()
}

/// feature-66 I6 (AC23): a deck replaced by bytes whose chunk text is the same but whose tab
/// now comes from a table rather than a paragraph is not re-embedded, and its table rows are
/// rewritten; a Markdown file changed the same way keeps NULL.
#[test]
fn a_deck_whose_rows_move_under_the_same_text_gets_new_table_rows() {
    let fx = kb("groove-f66-i6");
    let paragraph = zip_of(&[(
        "ppt/slides/slide1.xml",
        slide_xml(&shape("larch\tmaple")).as_bytes(),
    )]);
    let tabled = zip_of(&[(
        "ppt/slides/slide1.xml",
        slide_xml(&pptx_table(&[&["larch", "maple"]])).as_bytes(),
    )]);
    write_bytes(&fx, "tabline.pptx", &paragraph);
    write_bytes(
        &fx,
        "memo.md",
        b"---\ntags: [ochre]\n---\n# Pigments\n\nochre umber sienna notes\n",
    );
    index_stderr(&fx);
    assert_eq!(
        column(&fx, "tabline.pptx", "table_rows"),
        vec![Some("[]".to_string())],
        "premise: the paragraph's tab is no row"
    );

    write_bytes(&fx, "tabline.pptx", &tabled);
    write_bytes(
        &fx,
        "memo.md",
        b"---\ntags: [umber]\n---\n# Pigments\n\nochre umber sienna notes\n",
    );
    let before = fx.mock.requests().len();
    let stderr = index_stderr(&fx);
    assert!(stderr.contains("(2 updated, "), "{stderr}");
    assert!(
        embedded_since(&fx, before).is_empty(),
        "matching chunk text is not re-embedded"
    );
    assert_eq!(
        column(&fx, "tabline.pptx", "content"),
        vec![Some("larch\tmaple".to_string())]
    );
    assert_eq!(
        column(&fx, "tabline.pptx", "table_rows"),
        vec![Some(r#"[{"table":0,"start":0,"end":11}]"#.to_string())]
    );
    assert!(
        column(&fx, "memo.md", "table_rows")
            .iter()
            .all(Option::is_none),
        "a Markdown row is never written"
    );
    assert_dir_empty(&fx.cache);
}
