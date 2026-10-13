//! feature-65: `.pptx` tables and line breaks through `groove index`, and the one-time re-read
//! that catches up an index written before them, under a key of its own,
//! `pptx_table_break_policy`, beside the three `.docx` keys (ADR-0029). Each format's pass
//! re-reads its own rows only: a stale `.pptx` key never re-reads a `.docx`, nor the other way.
//!
//! Each test drives a knowledge base of generated `.pptx` and `.docx` files against the mock
//! endpoint of [`common::embed_cli`], so nothing downloads a model. The helpers of
//! `tests/index_docx_heading_policy.rs` are private to its modules, so the ones needed here are
//! repeated rather than opened up, which would edit those modules. The file names, the slide
//! text and the cells share no word.

mod common;

use common::embed_cli::{DIM, Fixture, fixture, stderr_of};
use common::embed_mock::{DOC_MODEL, assert_dir_empty, hermetic, openai_config_toml};
use common::temp::TempRoot;
use grooveseek::config::Config;
use grooveseek::db::{ContextMode, Database};
use grooveseek::embedder::Embedder;
use grooveseek::indexer::progress::{ProgressEvent, ProgressMode, ProgressReporter};
use grooveseek::indexer::{
    IndexResult, SingleResult, load_declared_schema, rebuild_index, reindex_single_file,
};
use rusqlite::{Connection, OptionalExtension};
use sha2::{Digest, Sha256};
use std::io::Write;
use std::process::Command;
use std::sync::{Arc, Mutex};
use zip::write::SimpleFileOptions;

const P_NS: &str = "http://schemas.openxmlformats.org/presentationml/2006/main";
const A_NS: &str = "http://schemas.openxmlformats.org/drawingml/2006/main";
const W_NS: &str = "http://schemas.openxmlformats.org/wordprocessingml/2006/main";

/// `[parsers].enabled` for every test here.
const ENABLED: &str = r#"["md", "docx", "pptx"]"#;

/// The three `.docx` keys and the values this feature leaves as they were.
const DOCX_KEYS: [(&str, &str); 3] = [
    ("docx_heading_policy", "styles-name-basedon"),
    ("docx_section_policy", "fold-empty-headings"),
    ("docx_table_policy", "tab-joined-rows"),
];
const DOCX_TABLE_KEY: &str = "docx_table_policy";
const PPTX_KEY: &str = "pptx_table_break_policy";
const PPTX_POLICY: &str = "tab-joined-rows-kept-breaks";
const AWAITING: &str = "awaiting-reparse";
/// The `.pptx` notice after its count.
const PPTX_NOTICE: &str = "unchanged .pptx document(s) once: table rows are now one line each, cells separated by tabs, and line breaks are kept";
/// The `.docx` notice after its count, when the table key alone is stale.
const DOCX_TABLE_NOTICE: &str =
    "unchanged .docx document(s) once: table rows are now one line each, cells separated by tabs";

/// The paragraph over the table of [`grid_pptx`].
const DECK_LEAD: &str = "tariff schedule overview";
/// The cells of [`grid_pptx`], row by row.
const DECK_ROWS: [[&str; 2]; 2] = [["Rate", "Approver"], ["five percent", "section chief"]];
/// The one paragraph of [`plain_pptx`].
const PLAIN_BODY: &str = "lighthouse keeper logbook entries";
/// The heading of [`sheet_docx`], styled `Heading1` with no styles part.
const SHEET_HEADING: &str = "Quota";
/// The cells of [`sheet_docx`], row by row.
const SHEET_ROWS: [[&str; 2]; 2] = [["Signer", "Limit"], ["plant manager", "twelve units"]];

/// The decompression budget a run that cannot read [`bloat_pptx`] runs under.
const DEC_CAP: usize = 16_384;

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
fn pptx_table(rows: &[[&str; 2]]) -> String {
    let mut body = String::new();
    for row in rows {
        body.push_str("<a:tr>");
        for text in row {
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

/// [`DECK_LEAD`] over a table of [`DECK_ROWS`] on one slide: one chunk, `Slide 1`.
fn grid_pptx() -> Vec<u8> {
    let slide = slide_xml(&format!("{}{}", shape(DECK_LEAD), pptx_table(&DECK_ROWS)));
    zip_of(&[("ppt/slides/slide1.xml", slide.as_bytes())])
}

/// One slide of [`PLAIN_BODY`]: no table and no line break, so this feature changes nothing
/// in it.
fn plain_pptx() -> Vec<u8> {
    let slide = slide_xml(&shape(PLAIN_BODY));
    zip_of(&[("ppt/slides/slide1.xml", slide.as_bytes())])
}

/// The content this version writes for [`grid_pptx`].
fn grid_new_content() -> String {
    let rows: Vec<String> = DECK_ROWS.iter().map(|r| r.join("\t")).collect();
    format!("{DECK_LEAD}\n{}", rows.join("\n"))
}

/// The content v1.18.0 wrote for [`grid_pptx`]: a line per cell.
fn grid_old_content() -> String {
    format!("{DECK_LEAD}\n{}", DECK_ROWS.concat().join("\n"))
}

/// [`SHEET_HEADING`] over a table of [`SHEET_ROWS`]: one chunk.
fn sheet_docx() -> Vec<u8> {
    let mut body = format!(
        r#"<w:p><w:pPr><w:pStyle w:val="Heading1"/></w:pPr><w:r><w:t>{SHEET_HEADING}</w:t></w:r></w:p><w:tbl>"#
    );
    for row in SHEET_ROWS {
        body.push_str("<w:tr>");
        for text in row {
            body.push_str(&format!(
                "<w:tc><w:p><w:r><w:t>{text}</w:t></w:r></w:p></w:tc>"
            ));
        }
        body.push_str("</w:tr>");
    }
    body.push_str("</w:tbl>");
    let doc = format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><w:document xmlns:w="{W_NS}"><w:body>{body}</w:body></w:document>"#
    );
    zip_of(&[("word/document.xml", doc.as_bytes())])
}

/// The content this version (feature-64 and later) writes for [`sheet_docx`].
fn sheet_new_content() -> String {
    SHEET_ROWS
        .iter()
        .map(|r| r.join("\t"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The content a version before feature-64 wrote for [`sheet_docx`]: a line per cell.
fn sheet_old_content() -> String {
    SHEET_ROWS.concat().join("\n")
}

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

/// A deck of two slides, each under [`DEC_CAP`] on its own and over it together: the default
/// caps read it, and a run under [`DEC_CAP`] fails to parse it. A single part over the cap would
/// not do: the pptx parser skips such a part and keeps the document, and a document left with no
/// chunk is dropped rather than left unsettled.
fn bloat_pptx() -> Vec<u8> {
    let half = DEC_CAP / 2 + 64;
    let first = pad_to(&slide_xml(&shape("ballast opening")), "</p:spTree>", half);
    let second = pad_to(&slide_xml(&shape("ballast closing")), "</p:spTree>", half);
    zip_of(&[
        ("ppt/slides/slide1.xml", first.as_bytes()),
        ("ppt/slides/slide2.xml", second.as_bytes()),
    ])
}

/// A `.docx` whose `word/document.xml` alone is over [`DEC_CAP`].
fn bloat_docx() -> Vec<u8> {
    let doc = format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><w:document xmlns:w="{W_NS}"><w:body><w:p><w:r><w:t>ballast memo</w:t></w:r></w:p></w:body></w:document>"#
    );
    let doc = pad_to(&doc, "</w:body>", DEC_CAP + 1);
    zip_of(&[("word/document.xml", doc.as_bytes())])
}

/// Point `fx`'s `groove.toml` at its mock with [`ENABLED`], `tables` after `[parsers]`. The file
/// is passed with `--config`, so its `[index]` caps count.
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

fn meta(fx: &Fixture, key: &str) -> Option<String> {
    db(fx)
        .query_row("SELECT value FROM index_meta WHERE key = ?1", [key], |r| {
            r.get(0)
        })
        .optional()
        .expect("read index_meta")
}

fn delete_meta(fx: &Fixture, key: &str) {
    db(fx)
        .execute("DELETE FROM index_meta WHERE key = ?1", [key])
        .expect("delete an index_meta key");
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

/// Replace `rel`'s rows with one chunk of `heading` and `content` under the hash of `bytes`,
/// through the public write API, so the unchanged fast path keeps them: the rows an older
/// version wrote.
fn write_old_rows(fx: &Fixture, rel: &str, bytes: &[u8], heading: &str, content: &str) {
    let path = fx.layout.root().join(".groove.db");
    let db = Database::open(&path.to_string_lossy()).expect("open the index");
    db.delete_document(rel)
        .expect("drop the rows this version wrote");
    let id = db
        .upsert_document(
            rel,
            None,
            None,
            None,
            None,
            &[],
            None,
            &sha256_hex(bytes),
            bytes.len() as u64,
        )
        .expect("an old row");
    db.insert_chunk(
        id,
        0,
        Some(heading),
        Some(2),
        content,
        None,
        &[0.125_f32; DIM],
        1.0,
    )
    .expect("an old chunk");
}

/// The texts the mock was asked to embed as documents since request `before`.
fn embedded_since(fx: &Fixture, before: usize) -> Vec<String> {
    fx.requests_since(before)
        .iter()
        .filter(|r| r.model() == Some(DOC_MODEL))
        .flat_map(|r| r.inputs())
        .collect()
}

/// feature-65 I1 (AC15): a new index records the pptx generation beside the three docx ones,
/// announces nothing, and writes a pptx table one row per line.
#[test]
fn a_new_index_records_the_pptx_generation() {
    let fx = kb("groove-f65-i1");
    write_bytes(&fx, "a-grid.pptx", &grid_pptx());
    write_bytes(&fx, "c-sheet.docx", &sheet_docx());
    let stderr = index_stderr(&fx);
    assert!(!stderr.contains("Re-reading"), "{stderr}");
    assert_eq!(meta(&fx, PPTX_KEY).as_deref(), Some(PPTX_POLICY));
    for (key, value) in DOCX_KEYS {
        assert_eq!(meta(&fx, key).as_deref(), Some(value), "{key}");
    }
    assert_eq!(
        column(&fx, "a-grid.pptx", "content"),
        vec![Some(grid_new_content())]
    );
    assert_dir_empty(&fx.cache);
}

/// feature-65 I2 (AC16, AC17, AC19): an index whose pptx rows v1.18.0 wrote, with the docx keys
/// at their values and no pptx key, re-reads its unchanged `.pptx` once, says so with the pptx
/// notice, rewrites and re-embeds only the deck with a table, and records the pptx key. The
/// `.docx` here holds rows in the form before feature-64 under current docx keys: a docx pass
/// that opened would rewrite them, so their staying put shows it did not.
#[test]
fn a_v1180_index_rejoins_its_pptx_rows_once_and_leaves_docx_alone() {
    let fx = kb("groove-f65-i2");
    let (grid, plain, sheet) = (grid_pptx(), plain_pptx(), sheet_docx());
    write_bytes(&fx, "a-grid.pptx", &grid);
    write_bytes(&fx, "b-plain.pptx", &plain);
    write_bytes(&fx, "c-sheet.docx", &sheet);
    index_stderr(&fx);
    write_old_rows(&fx, "a-grid.pptx", &grid, "Slide 1", &grid_old_content());
    write_old_rows(
        &fx,
        "c-sheet.docx",
        &sheet,
        SHEET_HEADING,
        &sheet_old_content(),
    );

    let control = index_stderr(&fx);
    assert!(!control.contains("Re-reading"), "{control}");
    assert!(control.contains("(0 updated, "), "{control}");
    assert_eq!(
        column(&fx, "a-grid.pptx", "content"),
        vec![Some(grid_old_content())],
        "with every key at its value the old rows stay on the fast path"
    );

    delete_meta(&fx, PPTX_KEY);
    let docx_before: Vec<Option<String>> = DOCX_KEYS.iter().map(|(k, _)| meta(&fx, k)).collect();
    let before = fx.mock.requests().len();
    let second = index_stderr(&fx);
    assert!(
        second.contains(&format!("Re-reading 2 {PPTX_NOTICE}")),
        "{second}"
    );
    assert!(!second.contains(".docx document(s) once"), "{second}");
    assert!(second.contains("(1 updated, "), "{second}");
    assert_eq!(
        column(&fx, "a-grid.pptx", "content"),
        vec![Some(grid_new_content())]
    );
    assert_eq!(
        column(&fx, "c-sheet.docx", "content"),
        vec![Some(sheet_old_content())],
        "the docx pass did not open"
    );
    assert_eq!(content_hash(&fx, "c-sheet.docx"), Some(sha256_hex(&sheet)));
    assert_eq!(content_hash(&fx, "b-plain.pptx"), Some(sha256_hex(&plain)));
    let docx_after: Vec<Option<String>> = DOCX_KEYS.iter().map(|(k, _)| meta(&fx, k)).collect();
    assert_eq!(docx_after, docx_before);
    assert_eq!(meta(&fx, PPTX_KEY).as_deref(), Some(PPTX_POLICY));
    let embedded = embedded_since(&fx, before);
    assert!(
        embedded.iter().any(|t| t.contains("Rate\tApprover")),
        "the deck was re-embedded: {embedded:?}"
    );
    assert!(
        embedded
            .iter()
            .all(|t| !t.contains(PLAIN_BODY) && !t.contains("Signer")),
        "the deck without a table and the docx were not: {embedded:?}"
    );

    let before = fx.mock.requests().len();
    let third = index_stderr(&fx);
    assert!(!third.contains("Re-reading"), "{third}");
    assert!(third.contains("(0 updated, "), "{third}");
    assert!(
        embedded_since(&fx, before).is_empty(),
        "nothing is embedded once the pass is recorded"
    );
    assert_dir_empty(&fx.cache);
}

/// feature-65 I3 (AC18): the other way round. With the pptx key at its value and the docx table
/// key gone, the docx pass alone runs: its notice and none of the pptx one, the old docx rows
/// rewritten, the old pptx rows kept, and a `.pptx` no run can read left with its own hash, not
/// marked.
#[test]
fn a_stale_docx_key_rereads_no_pptx() {
    let fx = kb("groove-f65-i3");
    let (grid, sheet, bloat) = (grid_pptx(), sheet_docx(), bloat_pptx());
    write_bytes(&fx, "a-grid.pptx", &grid);
    write_bytes(&fx, "c-sheet.docx", &sheet);
    write_bytes(&fx, "d-bloat.pptx", &bloat);
    index_stderr(&fx);
    write_old_rows(&fx, "a-grid.pptx", &grid, "Slide 1", &grid_old_content());
    write_old_rows(
        &fx,
        "c-sheet.docx",
        &sheet,
        SHEET_HEADING,
        &sheet_old_content(),
    );
    delete_meta(&fx, DOCX_TABLE_KEY);
    configure(
        &fx,
        &format!("[index]\nmax_decompressed_size = {DEC_CAP}\n"),
    );

    let before = fx.mock.requests().len();
    let second = index_stderr(&fx);
    assert!(
        second.contains(&format!("Re-reading 1 {DOCX_TABLE_NOTICE}")),
        "{second}"
    );
    assert!(!second.contains(".pptx document(s) once"), "{second}");
    assert_eq!(
        column(&fx, "c-sheet.docx", "content"),
        vec![Some(sheet_new_content())]
    );
    assert_eq!(
        column(&fx, "a-grid.pptx", "content"),
        vec![Some(grid_old_content())],
        "the pptx pass did not open"
    );
    assert_eq!(content_hash(&fx, "a-grid.pptx"), Some(sha256_hex(&grid)));
    assert_eq!(
        content_hash(&fx, "d-bloat.pptx"),
        Some(sha256_hex(&bloat)),
        "the docx pass marks no pptx"
    );
    assert_eq!(
        meta(&fx, DOCX_TABLE_KEY).as_deref(),
        Some("tab-joined-rows")
    );
    assert_eq!(meta(&fx, PPTX_KEY).as_deref(), Some(PPTX_POLICY));
    let embedded = embedded_since(&fx, before);
    assert!(
        embedded
            .iter()
            .all(|t| !t.contains("Rate") && !t.contains("ballast")),
        "no pptx was re-embedded: {embedded:?}"
    );
    assert_dir_empty(&fx.cache);
}

/// feature-65 I4 (AC20): `--force` records the pptx generation without the notice.
#[test]
fn a_forced_run_records_the_pptx_generation() {
    let fx = kb("groove-f65-i4");
    write_bytes(&fx, "a-grid.pptx", &grid_pptx());
    index_stderr(&fx);
    delete_meta(&fx, PPTX_KEY);

    let out = fx.index_force();
    let stderr = stderr_of(&out);
    assert!(out.status.success(), "{stderr}");
    assert!(!stderr.contains("Re-reading"), "{stderr}");
    assert_eq!(meta(&fx, PPTX_KEY).as_deref(), Some(PPTX_POLICY));
    for (key, value) in DOCX_KEYS {
        assert_eq!(meta(&fx, key).as_deref(), Some(value), "{key}");
    }
    assert_eq!(
        column(&fx, "a-grid.pptx", "content"),
        vec![Some(grid_new_content())]
    );
}

/// feature-65 I6 (AC21): the write of the pptx key fails, so the run returns an error and
/// records nothing -- no pptx key, no row marked. With the trigger gone the next run announces
/// the pptx pass, marks the deck it cannot read, and records the key.
#[test]
fn a_run_whose_pptx_key_fails_records_nothing() {
    let fx = kb("groove-f65-i6");
    let bloat = bloat_pptx();
    write_bytes(&fx, "d-bloat.pptx", &bloat);
    write_bytes(&fx, "e-tidy.pptx", &plain_pptx());
    index_stderr(&fx);
    delete_meta(&fx, PPTX_KEY);
    configure(
        &fx,
        &format!("[index]\nmax_decompressed_size = {DEC_CAP}\n"),
    );
    {
        let conn = db(&fx);
        conn.execute_batch("CREATE TRIGGER inject_pptx_policy_failure BEFORE INSERT ON index_meta WHEN NEW.key = 'pptx_table_break_policy' BEGIN SELECT RAISE(ABORT, 'injected pptx policy failure'); END;")
            .expect("create the trigger");
        let probe = conn
            .execute(
                "INSERT OR REPLACE INTO index_meta (key, value) VALUES ('pptx_table_break_policy', 'probe')",
                [],
            )
            .expect_err("premise: the trigger stops the pptx key");
        assert!(
            probe.to_string().contains("injected pptx policy failure"),
            "{probe}"
        );
    }

    let out = fx.run_index();
    let stderr = stderr_of(&out);
    assert!(!out.status.success(), "{stderr}");
    assert!(stderr.contains("injected pptx policy failure"), "{stderr}");
    assert_eq!(
        meta(&fx, PPTX_KEY),
        None,
        "the failed run records no pptx key"
    );
    let marked: i64 = db(&fx)
        .query_row(
            "SELECT count(*) FROM documents WHERE content_hash = ?1",
            [AWAITING],
            |r| r.get(0),
        )
        .expect("count marks");
    assert_eq!(marked, 0, "the failed run marks nothing");
    assert_eq!(content_hash(&fx, "d-bloat.pptx"), Some(sha256_hex(&bloat)));

    db(&fx)
        .execute_batch("DROP TRIGGER inject_pptx_policy_failure;")
        .expect("drop the trigger");
    let next = index_stderr(&fx);
    assert!(
        next.contains(&format!("Re-reading 2 {PPTX_NOTICE}")),
        "{next}"
    );
    assert_eq!(content_hash(&fx, "d-bloat.pptx").as_deref(), Some(AWAITING));
    assert_eq!(meta(&fx, PPTX_KEY).as_deref(), Some(PPTX_POLICY));
}

/// feature-65 I8 (AC29): with both formats stale, the `.docx` notice comes before the `.pptx`
/// one, each once.
#[test]
fn both_stale_formats_announce_docx_before_pptx() {
    let fx = kb("groove-f65-i8");
    write_bytes(&fx, "a-grid.pptx", &grid_pptx());
    write_bytes(&fx, "b-plain.pptx", &plain_pptx());
    write_bytes(&fx, "c-sheet.docx", &sheet_docx());
    index_stderr(&fx);
    delete_meta(&fx, DOCX_TABLE_KEY);
    delete_meta(&fx, PPTX_KEY);

    let stderr = index_stderr(&fx);
    let lines: Vec<&str> = stderr.lines().collect();
    let docx_at = lines
        .iter()
        .position(|l| l.contains(&format!("Re-reading 1 {DOCX_TABLE_NOTICE}")))
        .unwrap_or_else(|| panic!("no docx notice:\n{stderr}"));
    let pptx_at = lines
        .iter()
        .position(|l| l.contains(&format!("Re-reading 2 {PPTX_NOTICE}")))
        .unwrap_or_else(|| panic!("no pptx notice:\n{stderr}"));
    assert!(docx_at < pptx_at, "{stderr}");
    assert_eq!(
        lines.iter().filter(|l| l.contains("Re-reading")).count(),
        2,
        "{stderr}"
    );
}

/// feature-65 RF-5: with both formats stale, each pass marks the rows of its own
/// format it cannot read, and the run records all four keys.
#[test]
fn both_passes_mark_the_rows_they_cannot_read() {
    let fx = kb("groove-f65-rf5");
    write_bytes(&fx, "d-bloat.pptx", &bloat_pptx());
    write_bytes(&fx, "f-bloat.docx", &bloat_docx());
    write_bytes(&fx, "a-grid.pptx", &grid_pptx());
    index_stderr(&fx);
    delete_meta(&fx, DOCX_TABLE_KEY);
    delete_meta(&fx, PPTX_KEY);
    configure(
        &fx,
        &format!("[index]\nmax_decompressed_size = {DEC_CAP}\n"),
    );

    let stderr = index_stderr(&fx);
    assert!(stderr.contains(".docx document(s) once"), "{stderr}");
    assert!(stderr.contains(".pptx document(s) once"), "{stderr}");
    assert_eq!(content_hash(&fx, "d-bloat.pptx").as_deref(), Some(AWAITING));
    assert_eq!(content_hash(&fx, "f-bloat.docx").as_deref(), Some(AWAITING));
    assert_eq!(meta(&fx, PPTX_KEY).as_deref(), Some(PPTX_POLICY));
    for (key, value) in DOCX_KEYS {
        assert_eq!(meta(&fx, key).as_deref(), Some(value), "{key}");
    }
}

/// Set on the child [`run_in_hermetic_child`] starts, so the child runs the test body.
const HERMETIC_CHILD: &str = "GROOVE_F65_HERMETIC_CHILD";

/// Run the test `name` again in a child of this test binary under the environment
/// [`common::embed_mock::hermetic`] pins. `true` in the parent, after the child passed exactly
/// one test; `false` in the child.
///
/// Copy of the helper of the same name in `tests/index_docx_heading_policy.rs`, whose copies
/// are private to its modules.
fn run_in_hermetic_child(name: &str) -> bool {
    if std::env::var_os(HERMETIC_CHILD).is_some() {
        return false;
    }
    let cache = TempRoot::new("groove-f65-fastembed");
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
/// `groove index` wires it, reporting to the
/// [`grooveseek::indexer::progress::ProgressReporter`] it is given.
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

/// feature-65 I5 (AC17): the shape desktop sees -- a callback reporter, `force=false` -- hears
/// `Indexed` for the deck whose table rows are rejoined and `Unchanged` for the deck without a
/// table and for the `.docx`, whose rows in the form before feature-64 stay as they were.
#[test]
fn a_rejoined_pptx_reports_indexed_and_the_rest_unchanged() {
    if run_in_hermetic_child("a_rejoined_pptx_reports_indexed_and_the_rest_unchanged") {
        return;
    }
    let fx = kb("groove-f65-i5");
    let (grid, sheet) = (grid_pptx(), sheet_docx());
    write_bytes(&fx, "a-grid.pptx", &grid);
    write_bytes(&fx, "b-plain.pptx", &plain_pptx());
    write_bytes(&fx, "c-sheet.docx", &sheet);
    rebuild_in_process(&fx, ProgressReporter::new(ProgressMode::Quiet)).expect("first run");
    write_old_rows(&fx, "a-grid.pptx", &grid, "Slide 1", &grid_old_content());
    write_old_rows(
        &fx,
        "c-sheet.docx",
        &sheet,
        SHEET_HEADING,
        &sheet_old_content(),
    );
    delete_meta(&fx, PPTX_KEY);

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
    let mut seen = log.lock().expect("log").clone();
    seen.sort();
    assert_eq!(
        seen,
        vec![
            ("a-grid.pptx".to_string(), "indexed"),
            ("b-plain.pptx".to_string(), "unchanged"),
            ("c-sheet.docx".to_string(), "unchanged"),
        ]
    );
    assert_eq!(
        column(&fx, "a-grid.pptx", "content"),
        vec![Some(grid_new_content())]
    );
    assert_eq!(
        column(&fx, "c-sheet.docx", "content"),
        vec![Some(sheet_old_content())]
    );
    assert_eq!(meta(&fx, PPTX_KEY).as_deref(), Some(PPTX_POLICY));
}

/// feature-65 I7 (AC22): the watcher records no pptx key. It leaves an unchanged deck's v1.18.0
/// rows alone and writes a changed one a row per line.
#[test]
fn the_watcher_rejoins_a_changed_pptx_and_records_no_generation() {
    if run_in_hermetic_child("the_watcher_rejoins_a_changed_pptx_and_records_no_generation") {
        return;
    }
    let fx = kb("groove-f65-i7");
    let grid = grid_pptx();
    write_bytes(&fx, "kept.pptx", &grid);
    write_bytes(&fx, "draft.pptx", &plain_pptx());
    rebuild_in_process(&fx, ProgressReporter::new(ProgressMode::Quiet)).expect("first run");
    write_old_rows(&fx, "kept.pptx", &grid, "Slide 1", &grid_old_content());
    delete_meta(&fx, PPTX_KEY);

    let cfg = Config::load_from(&fx.config).expect("load groove.toml");
    let registry = cfg.build_parser_registry(fx.kb()).expect("parser registry");
    let kb = fx.kb().canonicalize().expect("canonical kb");
    let (db, mut embedder) = open_in_process(&fx, &cfg);

    let unchanged = reindex_single_file(&db, &mut embedder, &kb, "kept.pptx", None, &registry)
        .expect("reindex the unchanged deck");
    assert_eq!(unchanged, SingleResult::Unchanged);
    assert_eq!(
        column(&fx, "kept.pptx", "content"),
        vec![Some(grid_old_content())],
        "the watcher runs no pass"
    );

    write_bytes(&fx, "draft.pptx", &grid);
    let changed = reindex_single_file(&db, &mut embedder, &kb, "draft.pptx", None, &registry)
        .expect("reindex the changed deck");
    assert_eq!(
        changed,
        SingleResult::Updated {
            chunks: 1,
            frontmatter_unparsed: false
        }
    );
    assert_eq!(
        column(&fx, "draft.pptx", "content"),
        vec![Some(grid_new_content())]
    );
    assert_eq!(
        meta(&fx, PPTX_KEY),
        None,
        "the watcher records no generation"
    );
}
