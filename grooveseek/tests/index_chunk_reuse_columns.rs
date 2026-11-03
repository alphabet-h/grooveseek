//! What a re-index that reuses a stored vector (feature-69) writes beside it, and what it does
//! not yet notice.
//!
//! - audit AX-17 (kuriya 613): a chunk whose vector is reused is still written from the chunk
//!   the new parse made, so its line range, symbol kind and table rows are the new ones, never
//!   the old row's.
//! - audit AX-18 (kuriya 614): `[embedding] max_input_chars` is not part of the match key, so
//!   lowering it leaves the vectors computed from the longer input in place, on an edit and on a
//!   rename. That test pins the current behaviour, which is a known limitation and not a
//!   specification: it is meant to fail the day the key takes the cap into account.
//!
//! Each test drives `groove index` over a generated knowledge base against the mock endpoint of
//! [`crate::common::embed_cli`], which answers each input with a vector derived from its text
//! alone, so equal bytes mean a kept vector and a different input gives different bytes.

mod common;

use common::embed_cli::{DIM, Fixture, fixture, stderr_of};
use common::embed_mock::{DOC_MODEL, assert_dir_empty, openai_config_toml};
use grooveseek::db::{Database, StoredChunkVector};
use rusqlite::Connection;
use std::io::Write;
use zip::write::SimpleFileOptions;

/// The texts the mock was asked to embed as documents since request `before`, the endpoint
/// probe left out.
fn chunk_inputs_since(fx: &Fixture, before: usize) -> Vec<String> {
    fx.requests_since(before)
        .iter()
        .filter(|r| r.model() == Some(DOC_MODEL))
        .flat_map(|r| r.inputs())
        .filter(|t| t != grooveseek::embedder::ENDPOINT_PROBE_TEXT)
        .collect()
}

/// `groove index`, which must succeed; its `Done in` line.
fn index_ok(fx: &Fixture) -> String {
    let out = fx.run_index();
    let stderr = stderr_of(&out);
    assert!(out.status.success(), "groove index failed:\n{stderr}");
    stderr
        .lines()
        .find(|l| l.contains("Done in"))
        .unwrap_or_else(|| panic!("no Done in line:\n{stderr}"))
        .to_string()
}

/// `rel`'s chunks as stored, in chunk order.
fn vectors(fx: &Fixture, rel: &str) -> Vec<StoredChunkVector> {
    let path = grooveseek::resolve_db_path(fx.kb());
    Database::open(path.to_str().expect("a UTF-8 index path"))
        .expect("open the index")
        .stored_chunk_vectors_for_path(rel)
        .expect("read the stored vectors")
}

/// The stored vector of the chunk whose content is `content`.
fn blob_of(rows: &[StoredChunkVector], content: &str) -> Vec<u8> {
    rows.iter()
        .find(|r| r.content == content)
        .unwrap_or_else(|| {
            let have: Vec<&str> = rows.iter().map(|r| r.content.as_str()).collect();
            panic!("no stored chunk {content:?}; stored: {have:?}")
        })
        .embedding
        .clone()
}

/// One stored chunk's columns besides its text and vector.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ChunkColumns {
    heading: Option<String>,
    content: String,
    start_line: Option<i64>,
    end_line: Option<i64>,
    symbol_kind: Option<String>,
    table_rows: Option<String>,
}

/// The columns of `rel`'s chunks, in chunk order, read straight from the index file.
fn columns(fx: &Fixture, rel: &str) -> Vec<ChunkColumns> {
    let conn = Connection::open(grooveseek::resolve_db_path(fx.kb())).expect("open the index");
    let mut stmt = conn
        .prepare(
            "SELECT c.heading, c.content, c.start_line, c.end_line, c.symbol_kind, c.table_rows
               FROM chunks c JOIN documents d ON d.id = c.document_id
              WHERE d.path = ?1 ORDER BY c.chunk_index",
        )
        .expect("prepare");
    stmt.query_map([rel], |r| {
        Ok(ChunkColumns {
            heading: r.get(0)?,
            content: r.get(1)?,
            start_line: r.get(2)?,
            end_line: r.get(3)?,
            symbol_kind: r.get(4)?,
            table_rows: r.get(5)?,
        })
    })
    .expect("query")
    .collect::<Result<_, _>>()
    .expect("rows")
}

/// The stored chunk whose content is `content`.
fn chunk_with(rows: &[ChunkColumns], content: &str) -> ChunkColumns {
    rows.iter()
        .find(|r| r.content == content)
        .unwrap_or_else(|| panic!("no chunk {content:?} in {rows:#?}"))
        .clone()
}

/// Point `fx`'s `groove.toml` at its mock, with `embedding_extra` inside `[embedding]` and
/// `rest` after it.
fn configure(fx: &Fixture, embedding_extra: &str, rest: &str) {
    let toml = format!(
        "{}{embedding_extra}{rest}",
        openai_config_toml(&fx.mock.endpoint(), None, DIM)
    );
    std::fs::write(&fx.config, toml).expect("write groove.toml");
}

// ---------------------------------------------------------------------------
// AX-17: a reused chunk carries the new chunk's columns
// ---------------------------------------------------------------------------

/// The body of `alpha` in [`code_v1`] / [`code_v2`]: the same text in both, so its vector is
/// reused.
const ALPHA: &str = "fn alpha() -> u32 {\n    let total = 40 + 2;\n    total\n}";

/// `alpha` on lines 1-4, `beta` on lines 6-9.
fn code_v1() -> String {
    format!("{ALPHA}\n\nfn beta() -> u32 {{\n    let seven = 7;\n    seven\n}}\n")
}

/// [`code_v1`] with three lines inserted above `alpha` (now on lines 4-7) and `beta`'s body
/// changed, so the document is re-embedded through the reuse plan rather than the meta-only
/// write.
fn code_v2() -> String {
    format!(
        "// three lines are inserted above alpha so every definition moves down\n\n\n{ALPHA}\n\nfn beta() -> u32 {{\n    let eight = 8;\n    eight\n}}\n"
    )
}

/// kuriya 613 (AX-17): a code chunk whose vector is reused is written with the line range and
/// symbol kind the new parse gave it -- here moved three lines down -- and keeps its stored
/// vector bytes.
#[test]
fn a_reused_code_chunk_carries_its_new_line_range() {
    let fx = fixture("ax17-code", &[("lib.rs", &code_v1())], "");
    configure(&fx, "", "[parsers]\nenabled = [\"md\", \"rs\"]\n");
    index_ok(&fx);
    let before_rows = columns(&fx, "lib.rs");
    let before = chunk_with(&before_rows, ALPHA);
    assert_eq!(
        (before.start_line, before.end_line),
        (Some(1), Some(4)),
        "premise: alpha starts on line 1: {before_rows:#?}"
    );
    assert!(
        before.symbol_kind.is_some(),
        "premise: alpha is a definition chunk: {before:?}"
    );
    let alpha_blob = blob_of(&vectors(&fx, "lib.rs"), ALPHA);

    fx.layout.write("lib.rs", &code_v2());
    let sent_from = fx.mock.requests().len();
    let line = index_ok(&fx);

    let sent = chunk_inputs_since(&fx, sent_from);
    assert!(
        !sent.iter().any(|t| t == ALPHA),
        "alpha's text is unchanged and must not be sent: {sent:?}"
    );
    assert!(
        !sent.is_empty(),
        "premise: beta changed, so the document went through the reuse plan: {line}"
    );
    assert_eq!(
        blob_of(&vectors(&fx, "lib.rs"), ALPHA),
        alpha_blob,
        "premise: alpha's vector was reused"
    );
    let after = chunk_with(&columns(&fx, "lib.rs"), ALPHA);
    assert_eq!(
        (after.start_line, after.end_line),
        (Some(4), Some(7)),
        "the reused chunk kept the old row's line range: {after:?}"
    );
    assert_eq!(
        after.symbol_kind, before.symbol_kind,
        "the reused chunk's symbol kind is not the new parse's: {after:?}"
    );
    assert_eq!(after.heading, before.heading);
    assert_dir_empty(&fx.cache);
}

const W_NS: &str = "http://schemas.openxmlformats.org/wordprocessingml/2006/main";

/// The four cells of the first section, which [`docx_v1`] writes as one paragraph joined by
/// tabs and [`docx_v2`] as one table row: the same chunk text either way.
const CELLS: [&str; 4] = [
    "larch plantation",
    "maple syrup",
    "birch bark",
    "rowan berries",
];

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

/// A `.docx` whose `<w:body>` is `body`.
fn docx_of(body: &str) -> Vec<u8> {
    let doc = format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><w:document xmlns:w="{W_NS}"><w:body>{body}</w:body></w:document>"#
    );
    zip_of(&[("word/document.xml", doc.as_bytes())])
}

/// A paragraph styled `Heading1`.
fn heading1(text: &str) -> String {
    format!(r#"<w:p><w:pPr><w:pStyle w:val="Heading1"/></w:pPr><w:r><w:t>{text}</w:t></w:r></w:p>"#)
}

/// A plain paragraph.
fn para(text: &str) -> String {
    format!("<w:p><w:r><w:t>{text}</w:t></w:r></w:p>")
}

/// The second section's body in [`docx_v1`].
const SECOND_V1: &str =
    "The orchard ledger lists every tree planted along the north fence last spring.";
/// The second section's body in [`docx_v2`]: changed, so the document is re-embedded through
/// the reuse plan.
const SECOND_V2: &str =
    "The orchard ledger lists every tree planted along the south fence last autumn.";

/// `Grove` over [`CELLS`] as one paragraph with tabs, then `Ledger` over [`SECOND_V1`].
fn docx_v1() -> Vec<u8> {
    let mut cells = String::from("<w:p><w:r>");
    for (i, c) in CELLS.iter().enumerate() {
        if i > 0 {
            cells.push_str("<w:tab/>");
        }
        cells.push_str(&format!("<w:t>{c}</w:t>"));
    }
    cells.push_str("</w:r></w:p>");
    docx_of(&format!(
        "{}{cells}{}{}",
        heading1("Grove"),
        heading1("Ledger"),
        para(SECOND_V1)
    ))
}

/// `Grove` over [`CELLS`] as one table row, then `Ledger` over [`SECOND_V2`].
fn docx_v2() -> Vec<u8> {
    let mut row = String::from("<w:tbl><w:tr>");
    for c in CELLS {
        row.push_str(&format!(
            "<w:tc><w:p><w:r><w:t>{c}</w:t></w:r></w:p></w:tc>"
        ));
    }
    row.push_str("</w:tr></w:tbl>");
    docx_of(&format!(
        "{}{row}{}{}",
        heading1("Grove"),
        heading1("Ledger"),
        para(SECOND_V2)
    ))
}

/// kuriya 613 (AX-17): a `.docx` chunk whose vector is reused is written with the table rows the
/// new parse marked: its text stays the same while the tab-joined paragraph becomes a table row,
/// and another section changes so the document goes through the reuse plan.
#[test]
fn a_reused_docx_chunk_carries_its_new_table_rows() {
    let fx = fixture("ax17-docx", &[], "");
    configure(&fx, "", "[parsers]\nenabled = [\"md\", \"docx\"]\n");
    std::fs::write(fx.kb().join("grove.docx"), docx_v1()).expect("write the docx");
    index_ok(&fx);
    let joined = CELLS.join("\t");
    let before_rows = columns(&fx, "grove.docx");
    let before = before_rows
        .iter()
        .find(|r| r.content.contains(&joined))
        .unwrap_or_else(|| panic!("premise: a chunk holds the joined cells: {before_rows:#?}"))
        .clone();
    let before_blob = blob_of(&vectors(&fx, "grove.docx"), &before.content);

    std::fs::write(fx.kb().join("grove.docx"), docx_v2()).expect("rewrite the docx");
    let sent_from = fx.mock.requests().len();
    let line = index_ok(&fx);

    let sent = chunk_inputs_since(&fx, sent_from);
    assert!(
        !sent.contains(&before.content),
        "the cells' chunk text is unchanged and must not be sent: {sent:?}"
    );
    assert!(
        !sent.is_empty(),
        "premise: the second section changed, so the document went through the reuse plan: {line}"
    );
    let after = chunk_with(&columns(&fx, "grove.docx"), &before.content);
    assert_eq!(
        blob_of(&vectors(&fx, "grove.docx"), &before.content),
        before_blob,
        "premise: the cells' vector was reused"
    );
    let rows = after
        .table_rows
        .as_deref()
        .unwrap_or_else(|| panic!("the reused chunk has no table rows: {after:?}"));
    let spans: Vec<serde_json::Value> = serde_json::from_str(rows).expect("table rows are JSON");
    assert_eq!(
        spans.len(),
        1,
        "the reused chunk kept the old row's table rows ({:?} before): {after:?}",
        before.table_rows
    );
    assert_ne!(after.table_rows, before.table_rows, "{after:?}");
    assert_dir_empty(&fx.cache);
}

// ---------------------------------------------------------------------------
// AX-18: max_input_chars is not in the match key (current behaviour, not a spec)
// ---------------------------------------------------------------------------

/// The sections of [`runbook`]: `(heading, body)`.
const S1: (&str, &str) = (
    "Install",
    "Download the archive and unpack it into the tools folder before anything else.",
);
const S2: (&str, &str) = (
    "Configure",
    "Point the config file at the shared cache directory on the build machine.",
);
const S3: (&str, &str) = (
    "Verify",
    "Run the smoke check against the staging endpoint and compare the counts.",
);

/// A Markdown document titled `Runbook` with one `##` section per `(heading, body)`.
fn runbook(sections: &[(&str, &str)]) -> String {
    let mut md = String::from("---\ntitle: Runbook\n---\n\n");
    for (heading, body) in sections {
        md.push_str(&format!("## {heading}\n\n{body}\n\n"));
    }
    md
}

/// The embedding input of `(heading, body)` under the static context in a document titled
/// `Runbook`.
fn static_input((heading, body): (&str, &str)) -> String {
    format!("Runbook > {heading}\n\n{body}")
}

/// The static context, which makes a rename re-read the moved document past the hash fast path.
const STATIC: &str = "[contextual]\nenabled = true\n";

/// The `[embedding] max_input_chars` the test lowers to: shorter than every embedding input.
const CAP: usize = 16;

/// The first `max` chars of `text`, as the OpenAI-compatible provider cuts an input.
fn first_chars(text: &str, max: usize) -> String {
    text.chars().take(max).collect()
}

/// kuriya 614 (AX-18) -- **pins current behaviour, not a specification**. Lowering
/// `[embedding] max_input_chars` changes what the endpoint is sent for a new input, but not the
/// key a stored vector is matched by, so every chunk whose text is unchanged keeps the vector
/// computed from its longer input: on an edit of another section, and (kuriya 608) on a rename
/// re-read. Only `--force` brings them under the cap. This is the documented limitation of the
/// feature-69 spec (a non-goal); if the key ever includes the cap, this test fails and should be
/// replaced by one that pins the new rule.
#[test]
fn changing_max_input_chars_reuses_stored_vectors_documented_limitation() {
    let fx = fixture("ax18-cap", &[("runbook.md", &runbook(&[S1, S2, S3]))], "");
    configure(&fx, "", STATIC);
    index_ok(&fx);
    let before = vectors(&fx, "runbook.md");
    let (s1_before, s2_before) = (blob_of(&before, S1.1), blob_of(&before, S2.1));

    configure(&fx, &format!("max_input_chars = {CAP}\n"), STATIC);

    // An edit of one section: the new input goes out cut to the cap, the others are reused as
    // they were computed before the cap.
    let s3 = S3.1.replace("staging", "production");
    fx.layout
        .write("runbook.md", &runbook(&[S1, S2, ("Verify", &s3)]));
    let sent_from = fx.mock.requests().len();
    let line = index_ok(&fx);
    assert_eq!(
        chunk_inputs_since(&fx, sent_from),
        vec![first_chars(&static_input(("Verify", &s3)), CAP)],
        "premise: the cap is in effect for what is embedded"
    );
    assert!(line.contains("(this run: 1 embedded, 2 reused)"), "{line}");
    let edited = vectors(&fx, "runbook.md");
    assert_eq!(blob_of(&edited, S1.1), s1_before, "S1 was embedded again");
    assert_eq!(blob_of(&edited, S2.1), s2_before, "S2 was embedded again");

    // A rename: re-read past the fast path under the static context, every vector reused.
    std::fs::rename(fx.kb().join("runbook.md"), fx.kb().join("renamed.md")).expect("rename");
    let sent_from = fx.mock.requests().len();
    let line = index_ok(&fx);
    assert_eq!(
        chunk_inputs_since(&fx, sent_from),
        Vec::<String>::new(),
        "{line}"
    );
    let renamed = vectors(&fx, "renamed.md");
    assert_eq!(blob_of(&renamed, S1.1), s1_before, "S1 was embedded again");
    assert_eq!(blob_of(&renamed, S2.1), s2_before, "S2 was embedded again");

    // What the kept vectors are not: under `--force` the same chunk is embedded from its input
    // cut to the cap, and the mock answers that shorter input with other bytes.
    let out = fx.index_force();
    assert!(
        out.status.success(),
        "groove index --force failed:\n{}",
        stderr_of(&out)
    );
    let forced = vectors(&fx, "renamed.md");
    assert_ne!(
        blob_of(&forced, S1.1),
        s1_before,
        "premise: an input cut to the cap gives a different vector"
    );
    assert_dir_empty(&fx.cache);
}
