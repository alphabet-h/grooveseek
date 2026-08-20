//! Integration tests for feature-45 PR-2 binary-format (PDF) support:
//! index-time no-abort on a binary-mixed KB, opt-in gating of the `pdf`
//! parser, `get_document` extraction over MCP, and prune-retention of a
//! binary file that outgrew `MAX_RAW_BINARY_BYTES` (§7 / §6 #1c).
//!
//! All tests are `#[ignore]` because they spawn `groove index` / `groove
//! serve`, which load the BGE-small embedding model (~130 MB). Same policy
//! as `tests/kb_small_smoke.rs`: run on demand with
//! `cargo test --test binary_formats_cli -- --ignored`.
//!
//! Read-failure prune retention (the other half of the §4.2 unified prune
//! principle) is **not** covered here — Windows file-lock reproduction is
//! unstable in CI/dev-machine subprocess tests. That path is covered by the
//! unit test `indexer::tests::test_documents_to_delete_retains_skipped_paths`.
//! This file only exercises the size-skip half end-to-end.

use std::process::Command;

mod common;
use common::mcp::{grooveseek_bin, mcp_initialize, spawn_mcp_server};
use common::temp::TempKbLayout;

/// `[parsers].enabled = ["md", "pdf"]` — opts the `pdf` parser in, alongside
/// the always-on default `md`.
const PARSERS_MD_PDF: &str =
    "model = \"bge-small-en-v1.5\"\n[parsers]\nenabled = [\"md\", \"pdf\"]\n";

/// Default config: no `[parsers]` section, so `build_parser_registry()`
/// falls back to `Registry::defaults()` = `["md"]` only (see
/// `src/config.rs` around `build_parser_registry`).
const PARSERS_DEFAULT: &str = "model = \"bge-small-en-v1.5\"\n";

/// `[parsers].enabled = ["md", "docx", "xlsx", "pptx"]` — feature-45 PR-3
/// Office formats, opted in alongside the always-on default `md`.
const PARSERS_MD_OFFICE: &str =
    "model = \"bge-small-en-v1.5\"\n[parsers]\nenabled = [\"md\", \"docx\", \"xlsx\", \"pptx\"]\n";

/// Absolute path to `tests/fixtures/binary/<name>`.
fn fixture(name: &str) -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("binary")
        .join(name)
}

/// Run `groove --config <cfg> index --kb-path <kb>` and return stderr
/// (asserts exit 0 — CLAUDE.md output convention: index progress/status
/// goes to stderr, not stdout).
fn run_index(bin: &std::path::Path, cfg: &std::path::Path, kb: &std::path::Path) -> String {
    let out = Command::new(bin)
        .args([
            "--config",
            cfg.to_str().unwrap(),
            "index",
            "--kb-path",
            &kb.display().to_string(),
        ])
        .output()
        .expect("groove index");
    assert!(
        out.status.success(),
        "index failed:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// Run `groove status --kb-path <kb>` and return stdout (contains
/// `Documents: N`). Same read convention as `tests/kb_small_smoke.rs`.
fn status_stdout(bin: &std::path::Path, kb: &std::path::Path) -> String {
    let out = Command::new(bin)
        .args(["status", "--kb-path", &kb.display().to_string()])
        .output()
        .expect("groove status");
    assert!(
        out.status.success(),
        "status failed:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// Issue an MCP `tools/call` for `get_document` and return the deserialized
/// `result.content[0].text` JSON value (either a `DocumentResponse` or an
/// `ErrorResponse`, shape-compatible — same pattern as
/// `common::mcp::mcp_search_call` but for the `get_document` tool, which
/// that helper does not cover).
fn mcp_get_document_call(base: &str, session_id: &str, path: &str) -> serde_json::Value {
    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/call",
        "params": {
            "name": "get_document",
            "arguments": { "path": path },
        }
    });
    let body_str = serde_json::to_string(&body).unwrap();
    let out = Command::new("curl")
        .args([
            "-s",
            "-X",
            "POST",
            "-H",
            "content-type: application/json",
            "-H",
            "accept: application/json, text/event-stream",
            "-H",
            "MCP-Protocol-Version: 2025-06-18",
            "-H",
            &format!("Mcp-Session-Id: {session_id}"),
            "-d",
            &body_str,
            &format!("{base}/mcp"),
        ])
        .output()
        .expect("curl tools/call get_document");
    assert!(
        out.status.success(),
        "curl tools/call failed: stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let payload = stdout
        .lines()
        .filter_map(|line| {
            line.strip_prefix("data:")
                .or_else(|| line.strip_prefix("data: "))
                .map(|s| s.trim())
        })
        .find(|s| !s.is_empty())
        .unwrap_or_else(|| panic!("no non-empty `data:` line in SSE body:\n{stdout}"));
    let envelope: serde_json::Value = serde_json::from_str(payload)
        .unwrap_or_else(|e| panic!("invalid JSON-RPC envelope ({e}): {payload}"));
    let text = envelope
        .pointer("/result/content/0/text")
        .and_then(|v| v.as_str())
        .unwrap_or_else(|| panic!("missing result.content[0].text in envelope:\n{envelope}"));
    serde_json::from_str(text)
        .unwrap_or_else(|e| panic!("inner content text is not JSON ({e}): {text}"))
}

/// Scenario ①: a KB mixing a valid `.md`, a valid `.pdf`, a structurally
/// broken (encrypted) `.pdf`, and a non-UTF-8 `.md` must not abort
/// `groove index` — each unreadable/unparseable file is per-file skipped
/// (indexer.rs `Skipping <rel>: ...`) while the two valid documents are
/// indexed normally.
#[test]
#[ignore = "requires embedding model download"]
fn test_index_does_not_abort_on_binary_mixed_kb() {
    let layout = TempKbLayout::new("groove-bin-mixed");
    std::fs::write(
        layout.kb().join("valid.md"),
        "# V\n\nbody enough body enough body enough",
    )
    .unwrap();
    std::fs::copy(fixture("minimal.pdf"), layout.kb().join("doc.pdf")).unwrap();
    std::fs::copy(fixture("encrypted.pdf"), layout.kb().join("locked.pdf")).unwrap();
    // Invalid UTF-8 `.md` (0xFF 0xFE 0x00) — under the old `read_to_string`
    // era this would have aborted the whole index run via `?` propagation.
    std::fs::write(layout.kb().join("broken.md"), [0xffu8, 0xfe, 0x00]).unwrap();
    let cfg = layout.root().join("groove.toml");
    std::fs::write(&cfg, PARSERS_MD_PDF).unwrap();

    let bin = grooveseek_bin();
    let stderr = run_index(&bin, &cfg, layout.kb());
    assert!(
        stderr.contains("skipped") || stderr.to_lowercase().contains("skipping"),
        "expected skip warning for broken/encrypted files, got:\n{stderr}"
    );
    // valid.md + minimal.pdf are indexed; broken.md (invalid UTF-8) and
    // locked.pdf (encrypted) are skipped (never upserted into the DB).
    assert!(
        status_stdout(&bin, layout.kb()).contains("Documents: 2"),
        "expected 2 indexed docs (valid.md + doc.pdf)"
    );
}

/// Scenario ②: without opting the `pdf` parser in (`[parsers]` section
/// omitted → `Registry::defaults()` = `["md"]` only), a `.pdf` file in the
/// KB is not even collected as a source file — it is silently ignored, not
/// indexed and not counted as skipped.
#[test]
#[ignore = "requires embedding model download"]
fn test_pdf_not_indexed_without_opt_in() {
    let layout = TempKbLayout::new("groove-bin-noopt");
    std::fs::write(
        layout.kb().join("valid.md"),
        "# V\n\nbody enough body enough body enough",
    )
    .unwrap();
    std::fs::copy(fixture("minimal.pdf"), layout.kb().join("doc.pdf")).unwrap();
    let cfg = layout.root().join("groove.toml");
    std::fs::write(&cfg, PARSERS_DEFAULT).unwrap();

    let bin = grooveseek_bin();
    run_index(&bin, &cfg, layout.kb());
    assert!(
        status_stdout(&bin, layout.kb()).contains("Documents: 1"),
        "expected only valid.md to be indexed; doc.pdf must be ignored without \
         [parsers].enabled opt-in"
    );
}

/// Scenario ③: `get_document` over MCP on an indexed `.pdf` returns the
/// extracted text (non-empty) with `truncated=false` for a fixture well
/// under `EXTRACTED_TEXT_MAX_BYTES` (1 MiB).
#[test]
#[ignore = "requires embedding model download"]
fn test_get_document_returns_pdf_extracted_text() {
    let layout = TempKbLayout::new("groove-bin-getdoc");
    std::fs::copy(fixture("minimal.pdf"), layout.kb().join("doc.pdf")).unwrap();
    let cfg = layout.root().join("groove.toml");
    std::fs::write(&cfg, PARSERS_MD_PDF).unwrap();

    let bin = grooveseek_bin();
    run_index(&bin, &cfg, layout.kb());

    let (_guard, base) = spawn_mcp_server(layout.kb(), &cfg);
    let session_id = mcp_initialize(&base);
    let resp = mcp_get_document_call(&base, &session_id, "doc.pdf");

    let content = resp["content"].as_str().unwrap_or_else(|| {
        panic!("expected `content` string in get_document response, got: {resp}")
    });
    assert!(
        !content.trim().is_empty(),
        "expected non-empty extracted text from doc.pdf, got: {resp}"
    );
    assert_eq!(
        resp["truncated"].as_bool(),
        Some(false),
        "expected truncated=false for a small PDF, got: {resp}"
    );
}

/// Scenario ④ (prune-retention e2e, §7 / §6 #1c, size-skip half only): a
/// `.pdf` that grows past `MAX_RAW_BINARY_BYTES` (50 MiB) on re-index must
/// be size-skipped, not deleted from the DB — the §4.2 unified prune
/// principle (`visited ∪ skipped` are retained; only paths missing from
/// both are pruned).
///
/// Read-failure retention is intentionally **not** exercised here (Windows
/// file-lock reproduction is unstable in a subprocess test); it is covered
/// by `indexer::tests::test_documents_to_delete_retains_skipped_paths`.
#[test]
#[ignore = "requires embedding model download; writes a 50+ MiB sparse file"]
fn test_prune_retains_binary_grown_past_size_cap() {
    let layout = TempKbLayout::new("groove-bin-sizeskip");
    std::fs::write(
        layout.kb().join("note.md"),
        "# N\n\nbody enough body enough body enough",
    )
    .unwrap();
    let pdf = layout.kb().join("doc.pdf");
    std::fs::copy(fixture("minimal.pdf"), &pdf).unwrap();
    let cfg = layout.root().join("groove.toml");
    std::fs::write(&cfg, PARSERS_MD_PDF).unwrap();
    let bin = grooveseek_bin();

    // ① Initial index: note.md + doc.pdf = 2 documents.
    run_index(&bin, &cfg, layout.kb());
    assert!(
        status_stdout(&bin, layout.kb()).contains("Documents: 2"),
        "expected 2 docs after initial index"
    );

    // ② Grow doc.pdf past MAX_RAW_BINARY_BYTES (50 MiB) and re-index (size
    //    skip path). `set_len` produces a sparse file — logical size is
    //    what the size-cap check reads, so no real 51 MiB of I/O happens.
    {
        let f = std::fs::OpenOptions::new().write(true).open(&pdf).unwrap();
        f.set_len(51 * 1024 * 1024).unwrap(); // logical length 51 MiB (sparse) > 50 MiB cap
    }
    let stderr = run_index(&bin, &cfg, layout.kb());
    assert!(
        stderr.to_lowercase().contains("too large") || stderr.contains("skipping"),
        "expected size-skip warning in stderr, got:\n{stderr}"
    );

    // ③ The size-skipped pdf's document row is retained, not pruned
    //    (Documents count unchanged).
    assert!(
        status_stdout(&bin, layout.kb()).contains("Documents: 2"),
        "size-skipped binary must be retained, not pruned"
    );
}

// ---------------------------------------------------------------------------
// Task 3.6: Office formats (docx/xlsx/pptx) integration coverage
// ---------------------------------------------------------------------------
//
// Valid docx/xlsx/pptx fixtures are generated in-process rather than
// committed as binary assets (spec §7: minimize committed binary fixtures —
// the unit-test `parse_bytes` coverage in `src/parser/{docx,xlsx,pptx}.rs`
// already exercises real content extraction; these on-the-fly zips only
// need to be *valid enough* to round-trip through `groove index` +
// `get_document`). The builders below are trimmed re-implementations of the
// `make_minimal_*` helpers in those modules' `#[cfg(test)]` blocks —
// duplicated here because those helpers are private to their module and
// this integration test compiles as a separate crate that cannot reach them.

/// Minimal single-heading docx: `word/document.xml` with one `Heading1`
/// paragraph followed by a body paragraph. Mirrors
/// `docx::tests::make_minimal_docx`.
fn make_office_docx() -> Vec<u8> {
    use std::io::Write;
    use zip::write::SimpleFileOptions;

    let doc_xml = r#"<?xml version="1.0"?><w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:p><w:pPr><w:pStyle w:val="Heading1"/></w:pPr><w:r><w:t>Office Report</w:t></w:r></w:p><w:p><w:r><w:t>docx body content for search recall</w:t></w:r></w:p></w:body></w:document>"#;

    let mut buf = Vec::new();
    {
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
        let opt = SimpleFileOptions::default();
        zip.start_file("[Content_Types].xml", opt).unwrap();
        zip.write_all(br#"<?xml version="1.0"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/></Types>"#).unwrap();
        zip.start_file("word/document.xml", opt).unwrap();
        zip.write_all(doc_xml.as_bytes()).unwrap();
        zip.finish().unwrap();
    }
    buf
}

/// Minimal single-sheet xlsx using `inlineStr` cells (no `sharedStrings`
/// part needed). Mirrors `xlsx::tests::make_minimal_xlsx`.
fn make_office_xlsx() -> Vec<u8> {
    use std::io::Write;
    use zip::write::SimpleFileOptions;

    let mut buf = Vec::new();
    {
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
        let opt = SimpleFileOptions::default();

        zip.start_file("[Content_Types].xml", opt).unwrap();
        zip.write_all(br#"<?xml version="1.0"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/><Override PartName="/xl/worksheets/sheet1.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/></Types>"#).unwrap();

        zip.start_file("_rels/.rels", opt).unwrap();
        zip.write_all(br#"<?xml version="1.0"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/></Relationships>"#).unwrap();

        zip.start_file("xl/workbook.xml", opt).unwrap();
        zip.write_all(br#"<?xml version="1.0"?><workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><sheets><sheet name="Sheet1" sheetId="1" r:id="rId1"/></sheets></workbook>"#).unwrap();

        zip.start_file("xl/_rels/workbook.xml.rels", opt).unwrap();
        zip.write_all(br#"<?xml version="1.0"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/></Relationships>"#).unwrap();

        zip.start_file("xl/worksheets/sheet1.xml", opt).unwrap();
        zip.write_all(br#"<?xml version="1.0"?><worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData><row r="1"><c r="A1" t="inlineStr"><is><t>xlsx cell content for search recall</t></is></c></row></sheetData></worksheet>"#).unwrap();

        zip.finish().unwrap();
    }
    buf
}

/// Minimal single-slide pptx with a title placeholder + body shape. Mirrors
/// `pptx::tests::make_minimal_pptx`.
fn make_office_pptx() -> Vec<u8> {
    use std::io::Write;
    use zip::write::SimpleFileOptions;

    let slide_xml = r#"<?xml version="1.0"?><p:sld xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main" xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"><p:cSld><p:spTree><p:sp><p:nvSpPr><p:nvPr><p:ph type="title"/></p:nvPr></p:nvSpPr><p:txBody><a:p><a:r><a:t>Office Slide</a:t></a:r></a:p></p:txBody></p:sp><p:sp><p:txBody><a:p><a:r><a:t>pptx body content for search recall</a:t></a:r></a:p></p:txBody></p:sp></p:spTree></p:cSld></p:sld>"#;

    let mut buf = Vec::new();
    {
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
        let opt = SimpleFileOptions::default();
        zip.start_file("[Content_Types].xml", opt).unwrap();
        zip.write_all(br#"<?xml version="1.0"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"/>"#).unwrap();
        zip.start_file("ppt/slides/slide1.xml", opt).unwrap();
        zip.write_all(slide_xml.as_bytes()).unwrap();
        zip.finish().unwrap();
    }
    buf
}

/// Scenario ⑤: `[parsers].enabled = ["md", "docx", "xlsx", "pptx"]` — a KB
/// mixing a valid `.md` with on-the-fly-generated `.docx` / `.xlsx` /
/// `.pptx` files indexes all four documents, and a `~$`-prefixed Office
/// lock/owner file (garbage bytes, not a real zip) is silently excluded at
/// the walk stage (`indexer::is_office_lock_file`) rather than attempted
/// and skip-warned — same "opt-in gate, silent exclusion" shape as Scenario
/// ② for the pdf-without-opt-in case.
#[test]
#[ignore = "requires embedding model download"]
fn test_index_office_formats_and_ignores_lock_file() {
    let layout = TempKbLayout::new("groove-bin-office");
    std::fs::write(
        layout.kb().join("valid.md"),
        "# V\n\nbody enough body enough body enough",
    )
    .unwrap();
    std::fs::write(layout.kb().join("report.docx"), make_office_docx()).unwrap();
    std::fs::write(layout.kb().join("book.xlsx"), make_office_xlsx()).unwrap();
    std::fs::write(layout.kb().join("deck.pptx"), make_office_pptx()).unwrap();
    // MS Office owner/lock file: garbage bytes (not a real zip) under a
    // `~$` name. If it were collected and handed to a parser, `parse_bytes`
    // would error and the file would show up as a "Skipping ..." warning
    // (like scenario ①'s encrypted.pdf) — instead it must never reach the
    // parser at all, so no such warning is expected and it is not counted.
    std::fs::write(
        layout.kb().join("~$lock_dummy.docx"),
        b"not a real docx, owner-file placeholder bytes",
    )
    .unwrap();
    let cfg = layout.root().join("groove.toml");
    std::fs::write(&cfg, PARSERS_MD_OFFICE).unwrap();

    let bin = grooveseek_bin();
    let stderr = run_index(&bin, &cfg, layout.kb());
    assert!(
        !stderr.to_lowercase().contains("lock_dummy"),
        "the `~$` lock file must be excluded at the walk stage, not skip-warned:\n{stderr}"
    );
    // valid.md + report.docx + book.xlsx + deck.pptx = 4; ~$lock_dummy.docx
    // is never collected as a source file.
    assert!(
        status_stdout(&bin, layout.kb()).contains("Documents: 4"),
        "expected 4 indexed docs (md + docx + xlsx + pptx); lock file must be excluded"
    );

    let (_guard, base) = spawn_mcp_server(layout.kb(), &cfg);
    let session_id = mcp_initialize(&base);

    let docx_resp = mcp_get_document_call(&base, &session_id, "report.docx");
    let docx_content = docx_resp["content"].as_str().unwrap_or_else(|| {
        panic!("expected `content` string in get_document response, got: {docx_resp}")
    });
    assert!(
        docx_content.contains("docx body content for search recall"),
        "expected extracted docx body text, got: {docx_resp}"
    );

    let xlsx_resp = mcp_get_document_call(&base, &session_id, "book.xlsx");
    let xlsx_content = xlsx_resp["content"].as_str().unwrap_or_else(|| {
        panic!("expected `content` string in get_document response, got: {xlsx_resp}")
    });
    assert!(
        xlsx_content.contains("xlsx cell content for search recall"),
        "expected extracted xlsx cell text, got: {xlsx_resp}"
    );

    let pptx_resp = mcp_get_document_call(&base, &session_id, "deck.pptx");
    let pptx_content = pptx_resp["content"].as_str().unwrap_or_else(|| {
        panic!("expected `content` string in get_document response, got: {pptx_resp}")
    });
    assert!(
        pptx_content.contains("pptx body content for search recall"),
        "expected extracted pptx slide text, got: {pptx_resp}"
    );
}

/// AU-06 (codex P2): a config that was valid in an earlier release must not
/// cost the user their index.
///
/// `.xls` was a legal `[parsers].enabled` id up to v0.13.1. Upgrading without
/// editing `groove.toml` and then running `index --force` used to empty the
/// database first and reject the config afterwards, because
/// `build_parser_registry()` ran after `reset_for_model()`. Validating the
/// registry first costs nothing — it touches neither the filesystem nor the
/// database.
///
/// Not `#[ignore]`d: with the ordering correct the run fails before the
/// embedding model is ever loaded, so this needs no model download. If the
/// ordering regresses the test still fails, just more slowly.
#[test]
fn a_withdrawn_parser_id_does_not_cost_the_user_their_index() {
    let layout = TempKbLayout::new("groove-xls-migration");
    layout.write("a.md", "# A\n\nbody enough body enough body enough");
    let cfg_path = layout.root().join("groove.toml");
    // v0.13.1 までは妥当だった設定。
    std::fs::write(
        &cfg_path,
        "model = \"bge-small-en-v1.5\"\n[parsers]\nenabled = [\"md\", \"xls\"]\n",
    )
    .unwrap();

    // 既存 index を用意する (embedding model を使わずに lib API で直接置く)。
    let db_path = grooveseek::resolve_db_path(layout.kb());
    {
        let db = grooveseek::db::Database::open(&db_path.to_string_lossy()).unwrap();
        db.verify_embedding_meta("bge-small-en-v1.5", 384).unwrap();
        let doc_id = db
            .upsert_document("a.md", Some("A"), None, None, None, &[], None, "h", 0)
            .unwrap();
        db.insert_chunk(doc_id, 0, None, None, "body", None, &vec![0.1f32; 384], 1.0)
            .unwrap();
        assert_eq!(db.document_count().unwrap(), 1);
    }

    let out = Command::new(grooveseek_bin())
        .args([
            "--config",
            cfg_path.to_str().unwrap(),
            "index",
            "--kb-path",
            &layout.kb().display().to_string(),
            "--force",
        ])
        .output()
        .expect("groove index --force");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !out.status.success(),
        "an unusable [parsers].enabled must fail the run:\n{stderr}"
    );
    assert!(
        stderr.contains("xls"),
        "the error should name the offending id:\n{stderr}"
    );

    // 肝心なのはここ: 落ちる前に index を消していないこと。
    let db = grooveseek::db::Database::open(&db_path.to_string_lossy()).unwrap();
    assert_eq!(
        db.document_count().unwrap(),
        1,
        "the existing index must survive a rejected config:\n{stderr}"
    );
}

/// AU-06 (codex P2, round 3): a rejected config must not create a database
/// either.
///
/// The sibling test above proves an *existing* index survives, which the
/// review pointed out is not the whole claim: `Database::open` creates the
/// database and WAL files and runs schema migrations
/// (`ensure_fts_context_column` rebuilds the FTS index), so validating after
/// it still lets a doomed run mutate state. With the check first, a fresh KB
/// is left with no database at all.
#[test]
fn a_withdrawn_parser_id_does_not_even_create_a_database() {
    let layout = TempKbLayout::new("groove-xls-nodb");
    layout.write("a.md", "# A\n\nbody enough body enough body enough");
    let cfg_path = layout.root().join("groove.toml");
    std::fs::write(
        &cfg_path,
        "model = \"bge-small-en-v1.5\"\n[parsers]\nenabled = [\"md\", \"xls\"]\n",
    )
    .unwrap();

    let db_path = grooveseek::resolve_db_path(layout.kb());
    assert!(!db_path.exists(), "precondition: no database yet");

    let out = Command::new(grooveseek_bin())
        .args([
            "--config",
            cfg_path.to_str().unwrap(),
            "index",
            "--kb-path",
            &layout.kb().display().to_string(),
        ])
        .output()
        .expect("groove index");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "run must fail:\n{stderr}");
    assert!(
        !db_path.exists(),
        "a rejected config must not create the database:\n{stderr}"
    );
}
