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
use common::embed_mock::{DOC_MODEL, assert_dir_empty, hermetic, openai_config_toml};
use common::temp::TempRoot;
use grooveseek::config::Config;
use grooveseek::db::{ContextMode, Database};
use grooveseek::embedder::Embedder;
use grooveseek::indexer::progress::{CancelToken, ProgressEvent, ProgressMode, ProgressReporter};
use grooveseek::indexer::{IndexResult, load_declared_schema, rebuild_index};
use rusqlite::{Connection, OptionalExtension};
use sha2::{Digest, Sha256};
use std::io::Write;
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use zip::write::SimpleFileOptions;

const P_NS: &str = "http://schemas.openxmlformats.org/presentationml/2006/main";
const A_NS: &str = "http://schemas.openxmlformats.org/drawingml/2006/main";

/// `[parsers].enabled` for every test here.
const ENABLED: &str = r#"["md", "docx", "pptx"]"#;

const W_NS: &str = "http://schemas.openxmlformats.org/wordprocessingml/2006/main";

const DOCX_ROWS_KEY: &str = "docx_table_rows_policy";
const PPTX_ROWS_KEY: &str = "pptx_table_rows_policy";
const ROWS_POLICY: &str = "row-spans";
/// The keys before this feature and their values, which it leaves as they were.
const EARLIER_KEYS: [(&str, &str); 4] = [
    ("docx_heading_policy", "styles-name-basedon"),
    ("docx_section_policy", "fold-empty-headings"),
    ("docx_table_policy", "tab-joined-rows"),
    ("pptx_table_break_policy", "tab-joined-rows-kept-breaks"),
];
const AWAITING: &str = "awaiting-reparse";
/// The `.docx` notice when the table-rows key alone is stale, after its count.
const DOCX_ROWS_NOTICE: &str = "unchanged .docx document(s) once: table rows are now marked in each chunk; only documents whose sections change are re-embedded";
/// The `.pptx` notice when the table-rows key alone is stale, after its count.
const PPTX_ROWS_NOTICE: &str = "unchanged .pptx document(s) once: table rows are now marked in each chunk; only documents whose slides change are re-embedded";

/// The paragraph over the table of [`grid_pptx`].
const DECK_LEAD: &str = "tariff schedule overview";
/// The cells of [`grid_pptx`], row by row.
const DECK_ROWS: [&[&str]; 2] = [&["Rate", "Approver"], &["five percent", "section chief"]];
/// The heading of [`sheet_docx`], styled `Heading1` with no styles part.
const SHEET_HEADING: &str = "Quota";
/// The cells of [`sheet_docx`], row by row.
const SHEET_ROWS: [[&str; 2]; 2] = [["Signer", "Limit"], ["plant manager", "twelve units"]];

/// `chunks.table_rows` of each document of [`full_kb`], as this version writes it.
const GRID_ROWS: &str = r#"[{"table":0,"start":25,"end":38},{"table":0,"start":39,"end":65}]"#;
const PLAIN_ROWS: &str = "[]";
const SHEET_ROWS_JSON: &str = r#"[{"table":0,"start":0,"end":12},{"table":0,"start":13,"end":39}]"#;
const TABBED_DOCX_ROWS: &str = r#"[{"table":0,"start":0,"end":18}]"#;
const TABBED_PPTX_ROWS: &str = r#"[{"table":0,"start":0,"end":19}]"#;

/// The decompression budget a run that cannot read [`bloat_docx`] runs under.
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

/// [`DECK_LEAD`] over a table of [`DECK_ROWS`] on one slide: `tariff schedule overview`, then
/// `Rate\tApprover` at 25..38 and `five percent\tsection chief` at 39..65.
fn grid_pptx() -> Vec<u8> {
    let slide = slide_xml(&format!("{}{}", shape(DECK_LEAD), pptx_table(&DECK_ROWS)));
    zip_of(&[("ppt/slides/slide1.xml", slide.as_bytes())])
}

/// One slide of a paragraph and no table.
fn plain_pptx() -> Vec<u8> {
    let slide = slide_xml(&shape("lighthouse keeper logbook entries"));
    zip_of(&[("ppt/slides/slide1.xml", slide.as_bytes())])
}

/// A table whose first cell holds a raw tab: `yew\tzelkova\tjuniper`, one row at 0..19.
fn tabbed_pptx() -> Vec<u8> {
    let slide = slide_xml(&pptx_table(&[&["yew\tzelkova", "juniper"]]));
    zip_of(&[("ppt/slides/slide1.xml", slide.as_bytes())])
}

/// A `.docx` whose `<w:body>` is `body`.
fn docx_of(body: &str) -> Vec<u8> {
    let doc = format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><w:document xmlns:w="{W_NS}"><w:body>{body}</w:body></w:document>"#
    );
    zip_of(&[("word/document.xml", doc.as_bytes())])
}

/// [`SHEET_HEADING`] over a table of [`SHEET_ROWS`]: `Signer\tLimit` at 0..12 and
/// `plant manager\ttwelve units` at 13..39.
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
    docx_of(&body)
}

/// A table whose first cell holds a `<w:tab/>`: `quince\trowan\tsumac`, one row at 0..18.
fn tabbed_docx() -> Vec<u8> {
    docx_of(concat!(
        "<w:tbl><w:tr>",
        "<w:tc><w:p><w:r><w:t>quince</w:t><w:tab/><w:t>rowan</w:t></w:r></w:p></w:tc>",
        "<w:tc><w:p><w:r><w:t>sumac</w:t></w:r></w:p></w:tc>",
        "</w:tr></w:tbl>",
    ))
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

/// A `.docx` whose `word/document.xml` alone is over [`DEC_CAP`].
fn bloat_docx() -> Vec<u8> {
    let doc = format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><w:document xmlns:w="{W_NS}"><w:body><w:p><w:r><w:t>ballast memo</w:t></w:r></w:p></w:body></w:document>"#
    );
    let doc = pad_to(&doc, "</w:body>", DEC_CAP + 1);
    zip_of(&[("word/document.xml", doc.as_bytes())])
}

/// Every document of the knowledge base the catch-up tests use, with the `chunks.table_rows`
/// this version writes for its one chunk: three decks and two documents, two of them with a tab
/// inside a cell (spec AC18).
fn full_kb() -> [(&'static str, Vec<u8>, &'static str); 5] {
    [
        ("a-grid.pptx", grid_pptx(), GRID_ROWS),
        ("b-plain.pptx", plain_pptx(), PLAIN_ROWS),
        ("c-sheet.docx", sheet_docx(), SHEET_ROWS_JSON),
        ("d-tabbed.docx", tabbed_docx(), TABBED_DOCX_ROWS),
        ("e-tabbed.pptx", tabbed_pptx(), TABBED_PPTX_ROWS),
    ]
}

/// Write [`full_kb`] into `fx`.
fn write_full_kb(fx: &Fixture) {
    for (rel, bytes, _) in full_kb() {
        write_bytes(fx, rel, &bytes);
    }
}

/// Assert `chunks.table_rows` of each document of [`full_kb`] whose name ends in `suffix`
/// (`".docx"` / `".pptx"`): this version's value when `filled`, NULL otherwise.
fn assert_rows(fx: &Fixture, suffix: &str, filled: bool) {
    for (rel, _, rows) in full_kb() {
        if rel.ends_with(suffix) {
            let expected = filled.then(|| rows.to_string());
            assert_eq!(column(fx, rel, "table_rows"), vec![expected], "{rel}");
        }
    }
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

/// Set `chunks.table_rows` NULL on every chunk of a document with extension `ext`
/// (`"docx"` / `"pptx"`): the rows a version before this one wrote.
fn clear_rows(fx: &Fixture, ext: &str) {
    db(fx)
        .execute(
            "UPDATE chunks SET table_rows = NULL WHERE document_id IN (SELECT id FROM documents WHERE path LIKE ?1)",
            [format!("%.{ext}")],
        )
        .expect("clear table_rows");
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

/// feature-66 I1 (AC17): a new index records both table-rows keys beside the earlier ones,
/// announces nothing, and writes each docx / pptx chunk's table rows.
#[test]
fn a_new_index_records_the_table_rows_generation() {
    let fx = kb("groove-f66-i1");
    write_full_kb(&fx);
    let stderr = index_stderr(&fx);
    assert!(!stderr.contains("Re-reading"), "{stderr}");
    assert_eq!(meta(&fx, DOCX_ROWS_KEY).as_deref(), Some(ROWS_POLICY));
    assert_eq!(meta(&fx, PPTX_ROWS_KEY).as_deref(), Some(ROWS_POLICY));
    for (key, value) in EARLIER_KEYS {
        assert_eq!(meta(&fx, key).as_deref(), Some(value), "{key}");
    }
    assert_rows(&fx, ".docx", true);
    assert_rows(&fx, ".pptx", true);
    assert_dir_empty(&fx.cache);
}

/// feature-66 I2 (AC18): an index whose docx / pptx rows hold no table rows and whose two
/// table-rows keys are gone re-reads its unchanged documents once, docx notice first, writes the
/// column, embeds nothing, counts nothing as updated or skipped, and records both keys. The next
/// run takes the fast path.
#[test]
fn an_index_without_table_rows_catches_up_once_without_re_embedding() {
    let fx = kb("groove-f66-i2");
    write_full_kb(&fx);
    index_stderr(&fx);
    clear_rows(&fx, "docx");
    clear_rows(&fx, "pptx");
    delete_meta(&fx, DOCX_ROWS_KEY);
    delete_meta(&fx, PPTX_ROWS_KEY);

    let before = fx.mock.requests().len();
    let second = index_stderr(&fx);
    let lines: Vec<&str> = second.lines().collect();
    let docx_at = lines
        .iter()
        .position(|l| l.contains(&format!("Re-reading 2 {DOCX_ROWS_NOTICE}")))
        .unwrap_or_else(|| panic!("no docx notice:\n{second}"));
    let pptx_at = lines
        .iter()
        .position(|l| l.contains(&format!("Re-reading 3 {PPTX_ROWS_NOTICE}")))
        .unwrap_or_else(|| panic!("no pptx notice:\n{second}"));
    assert!(docx_at < pptx_at, "{second}");
    assert_eq!(
        lines.iter().filter(|l| l.contains("Re-reading")).count(),
        2,
        "{second}"
    );
    assert_rows(&fx, ".docx", true);
    assert_rows(&fx, ".pptx", true);
    assert!(
        embedded_since(&fx, before).is_empty(),
        "nothing is re-embedded"
    );
    assert!(
        second.contains("(0 updated, 0 renamed, 0 deleted, 0 skipped, "),
        "the re-read documents are unchanged: {second}"
    );
    assert_eq!(meta(&fx, DOCX_ROWS_KEY).as_deref(), Some(ROWS_POLICY));
    assert_eq!(meta(&fx, PPTX_ROWS_KEY).as_deref(), Some(ROWS_POLICY));
    for (key, value) in EARLIER_KEYS {
        assert_eq!(meta(&fx, key).as_deref(), Some(value), "{key}");
    }

    let before = fx.mock.requests().len();
    let third = index_stderr(&fx);
    assert!(!third.contains("Re-reading"), "{third}");
    assert!(embedded_since(&fx, before).is_empty(), "{third}");
    assert_dir_empty(&fx.cache);
}

/// feature-66 I3 (AC19): each format's key re-reads its own rows only. With the docx key gone
/// and every row cleared, the docx rows fill and the pptx rows stay NULL with no pptx notice;
/// then the other way round.
#[test]
fn a_stale_rows_key_of_one_format_leaves_the_other_alone() {
    let fx = kb("groove-f66-i3");
    write_full_kb(&fx);
    index_stderr(&fx);

    clear_rows(&fx, "docx");
    clear_rows(&fx, "pptx");
    delete_meta(&fx, DOCX_ROWS_KEY);
    let docx_run = index_stderr(&fx);
    assert!(
        docx_run.contains(&format!("Re-reading 2 {DOCX_ROWS_NOTICE}")),
        "{docx_run}"
    );
    assert!(!docx_run.contains(".pptx document(s) once"), "{docx_run}");
    assert_rows(&fx, ".docx", true);
    assert_rows(&fx, ".pptx", false);

    clear_rows(&fx, "docx");
    delete_meta(&fx, PPTX_ROWS_KEY);
    let pptx_run = index_stderr(&fx);
    assert!(
        pptx_run.contains(&format!("Re-reading 3 {PPTX_ROWS_NOTICE}")),
        "{pptx_run}"
    );
    assert!(!pptx_run.contains(".docx document(s) once"), "{pptx_run}");
    assert_rows(&fx, ".pptx", true);
    assert_rows(&fx, ".docx", false);
}

/// feature-66 I5 (AC21): the write of the docx table-rows key fails, so the run returns an
/// error and records nothing -- neither table-rows key, no row marked. With the trigger gone the
/// next run marks the document it cannot read and records both keys.
#[test]
fn a_run_whose_rows_key_fails_records_nothing() {
    let fx = kb("groove-f66-i5");
    write_full_kb(&fx);
    let bloat = bloat_docx();
    write_bytes(&fx, "f-bloat.docx", &bloat);
    index_stderr(&fx);
    delete_meta(&fx, DOCX_ROWS_KEY);
    delete_meta(&fx, PPTX_ROWS_KEY);
    configure(
        &fx,
        &format!("[index]\nmax_decompressed_size = {DEC_CAP}\n"),
    );
    {
        let conn = db(&fx);
        conn.execute_batch("CREATE TRIGGER inject_rows_policy_failure BEFORE INSERT ON index_meta WHEN NEW.key = 'docx_table_rows_policy' BEGIN SELECT RAISE(ABORT, 'injected rows policy failure'); END;")
            .expect("create the trigger");
        let probe = conn
            .execute(
                "INSERT OR REPLACE INTO index_meta (key, value) VALUES ('docx_table_rows_policy', 'probe')",
                [],
            )
            .expect_err("premise: the trigger stops the docx table-rows key");
        assert!(
            probe.to_string().contains("injected rows policy failure"),
            "{probe}"
        );
    }

    let out = fx.run_index();
    let stderr = stderr_of(&out);
    assert!(!out.status.success(), "{stderr}");
    assert!(stderr.contains("injected rows policy failure"), "{stderr}");
    assert_eq!(meta(&fx, DOCX_ROWS_KEY), None);
    assert_eq!(meta(&fx, PPTX_ROWS_KEY), None, "the same transaction");
    let marked: i64 = db(&fx)
        .query_row(
            "SELECT count(*) FROM documents WHERE content_hash = ?1",
            [AWAITING],
            |r| r.get(0),
        )
        .expect("count marks");
    assert_eq!(marked, 0, "the failed run marks nothing");
    assert_eq!(content_hash(&fx, "f-bloat.docx"), Some(sha256_hex(&bloat)));

    db(&fx)
        .execute_batch("DROP TRIGGER inject_rows_policy_failure;")
        .expect("drop the trigger");
    index_stderr(&fx);
    assert_eq!(content_hash(&fx, "f-bloat.docx").as_deref(), Some(AWAITING));
    assert_eq!(meta(&fx, DOCX_ROWS_KEY).as_deref(), Some(ROWS_POLICY));
    assert_eq!(meta(&fx, PPTX_ROWS_KEY).as_deref(), Some(ROWS_POLICY));
}

/// Set on the child [`run_in_hermetic_child`] starts, so the child runs the test body.
const HERMETIC_CHILD: &str = "GROOVE_F66_HERMETIC_CHILD";

/// Run the test `name` again in a child of this test binary under the environment
/// [`common::embed_mock::hermetic`] pins. `true` in the parent, after the child passed exactly
/// one test; `false` in the child.
///
/// Copy of the helper of the same name in `tests/index_pptx_table_policy.rs`, whose copy is
/// private to that file.
fn run_in_hermetic_child(name: &str) -> bool {
    if std::env::var_os(HERMETIC_CHILD).is_some() {
        return false;
    }
    let cache = TempRoot::new("groove-f66-fastembed");
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

/// [`grooveseek::indexer::rebuild_index`] over `fx` under its `groove.toml`, wired the way
/// `groove index` wires it, reporting to `progress`.
fn rebuild_in_process(fx: &Fixture, progress: ProgressReporter) -> anyhow::Result<IndexResult> {
    let kb = fx.kb();
    let cfg = Config::load_from(&fx.config).expect("load groove.toml");
    let registry = cfg.build_parser_registry(kb).expect("parser registry");
    let schema = load_declared_schema(kb).expect("groove-schema.toml");
    let embedding = cfg.resolve_embedding(None).expect("resolve [embedding]");
    let db_path = grooveseek::resolve_db_path(kb);
    let db = Database::open(&db_path.to_string_lossy()).expect("open the index");
    db.verify_embedding_meta(embedding.model_id(), embedding.dimension() as u32)
        .expect("embedding meta");
    let mut embedder = Embedder::with_settings(embedding).expect("build the embedder");
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

/// feature-66 I4 (AC20): a run cancelled after its first document records neither table-rows
/// key; the next run announces both passes, fills every row and records both keys.
#[test]
fn a_cancelled_run_records_no_rows_key() {
    if run_in_hermetic_child("a_cancelled_run_records_no_rows_key") {
        return;
    }
    let fx = kb("groove-f66-i4");
    write_full_kb(&fx);
    rebuild_in_process(&fx, ProgressReporter::new(ProgressMode::Quiet)).expect("first run");
    clear_rows(&fx, "docx");
    clear_rows(&fx, "pptx");
    delete_meta(&fx, DOCX_ROWS_KEY);
    delete_meta(&fx, PPTX_ROWS_KEY);

    let token = CancelToken::new();
    let seen = Arc::new(AtomicUsize::new(0));
    let (stop, count) = (token.clone(), Arc::clone(&seen));
    let reporter = ProgressReporter::with_callback(Box::new(move |ev| {
        let document = matches!(
            ev,
            ProgressEvent::Indexed { .. } | ProgressEvent::Unchanged { .. }
        );
        if document && count.fetch_add(1, Ordering::SeqCst) == 0 {
            stop.cancel();
        }
    }))
    .with_cancel(token);
    let result = rebuild_in_process(&fx, reporter).expect("a cancelled run returns Ok");
    assert!(result.cancelled, "{result:?}");
    assert_eq!(
        meta(&fx, DOCX_ROWS_KEY),
        None,
        "a cancelled run records no key"
    );
    assert_eq!(
        meta(&fx, PPTX_ROWS_KEY),
        None,
        "a cancelled run records no key"
    );

    let next = index_stderr(&fx);
    assert!(next.contains(DOCX_ROWS_NOTICE), "{next}");
    assert!(next.contains(PPTX_ROWS_NOTICE), "{next}");
    assert_rows(&fx, ".docx", true);
    assert_rows(&fx, ".pptx", true);
    assert_eq!(meta(&fx, DOCX_ROWS_KEY).as_deref(), Some(ROWS_POLICY));
    assert_eq!(meta(&fx, PPTX_ROWS_KEY).as_deref(), Some(ROWS_POLICY));
}
