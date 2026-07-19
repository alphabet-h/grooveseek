//! Integration tests for feature-45 PR-2 binary-format (PDF) support:
//! index-time no-abort on a binary-mixed KB, opt-in gating of the `pdf`
//! parser, `get_document` extraction over MCP, and prune-retention of a
//! binary file that outgrew `MAX_RAW_BINARY_BYTES` (§7 / §6 #1c).
//!
//! All tests are `#[ignore]` because they spawn `kb-mcp index` / `kb-mcp
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
use common::mcp::{kb_mcp_bin, mcp_initialize, spawn_mcp_server};
use common::temp::TempKbLayout;

/// `[parsers].enabled = ["md", "pdf"]` — opts the `pdf` parser in, alongside
/// the always-on default `md`.
const PARSERS_MD_PDF: &str =
    "model = \"bge-small-en-v1.5\"\n[parsers]\nenabled = [\"md\", \"pdf\"]\n";

/// Default config: no `[parsers]` section, so `build_parser_registry()`
/// falls back to `Registry::defaults()` = `["md"]` only (see
/// `src/config.rs` around `build_parser_registry`).
const PARSERS_DEFAULT: &str = "model = \"bge-small-en-v1.5\"\n";

/// Absolute path to `tests/fixtures/binary/<name>`.
fn fixture(name: &str) -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("binary")
        .join(name)
}

/// Run `kb-mcp --config <cfg> index --kb-path <kb>` and return stderr
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
        .expect("kb-mcp index");
    assert!(
        out.status.success(),
        "index failed:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// Run `kb-mcp status --kb-path <kb>` and return stderr (contains
/// `Documents: N`). Same read convention as `tests/kb_small_smoke.rs`.
fn status_stderr(bin: &std::path::Path, kb: &std::path::Path) -> String {
    let out = Command::new(bin)
        .args(["status", "--kb-path", &kb.display().to_string()])
        .output()
        .expect("kb-mcp status");
    assert!(
        out.status.success(),
        "status failed:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stderr).into_owned()
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
/// `kb-mcp index` — each unreadable/unparseable file is per-file skipped
/// (indexer.rs `Skipping <rel>: ...`) while the two valid documents are
/// indexed normally.
#[test]
#[ignore = "requires embedding model download"]
fn test_index_does_not_abort_on_binary_mixed_kb() {
    let layout = TempKbLayout::new("kb-mcp-bin-mixed");
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
    let cfg = layout.root().join("kb-mcp.toml");
    std::fs::write(&cfg, PARSERS_MD_PDF).unwrap();

    let bin = kb_mcp_bin();
    let stderr = run_index(&bin, &cfg, layout.kb());
    assert!(
        stderr.contains("skipped") || stderr.to_lowercase().contains("skipping"),
        "expected skip warning for broken/encrypted files, got:\n{stderr}"
    );
    // valid.md + minimal.pdf are indexed; broken.md (invalid UTF-8) and
    // locked.pdf (encrypted) are skipped (never upserted into the DB).
    assert!(
        status_stderr(&bin, layout.kb()).contains("Documents: 2"),
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
    let layout = TempKbLayout::new("kb-mcp-bin-noopt");
    std::fs::write(
        layout.kb().join("valid.md"),
        "# V\n\nbody enough body enough body enough",
    )
    .unwrap();
    std::fs::copy(fixture("minimal.pdf"), layout.kb().join("doc.pdf")).unwrap();
    let cfg = layout.root().join("kb-mcp.toml");
    std::fs::write(&cfg, PARSERS_DEFAULT).unwrap();

    let bin = kb_mcp_bin();
    run_index(&bin, &cfg, layout.kb());
    assert!(
        status_stderr(&bin, layout.kb()).contains("Documents: 1"),
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
    let layout = TempKbLayout::new("kb-mcp-bin-getdoc");
    std::fs::copy(fixture("minimal.pdf"), layout.kb().join("doc.pdf")).unwrap();
    let cfg = layout.root().join("kb-mcp.toml");
    std::fs::write(&cfg, PARSERS_MD_PDF).unwrap();

    let bin = kb_mcp_bin();
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
    let layout = TempKbLayout::new("kb-mcp-bin-sizeskip");
    std::fs::write(
        layout.kb().join("note.md"),
        "# N\n\nbody enough body enough body enough",
    )
    .unwrap();
    let pdf = layout.kb().join("doc.pdf");
    std::fs::copy(fixture("minimal.pdf"), &pdf).unwrap();
    let cfg = layout.root().join("kb-mcp.toml");
    std::fs::write(&cfg, PARSERS_MD_PDF).unwrap();
    let bin = kb_mcp_bin();

    // ① Initial index: note.md + doc.pdf = 2 documents.
    run_index(&bin, &cfg, layout.kb());
    assert!(
        status_stderr(&bin, layout.kb()).contains("Documents: 2"),
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
        status_stderr(&bin, layout.kb()).contains("Documents: 2"),
        "size-skipped binary must be retained, not pruned"
    );
}
