//! Retrieval-quality eval over a knowledge base that mixes Markdown with every
//! supported binary format (AU-24).
//!
//! # Why this exists
//!
//! `.pdf` (v0.10.0) and `.docx` / `.xlsx` / `.pptx` (v0.11.0) are covered by
//! parser unit tests and by `tests/binary_formats_cli.rs`, which check that a
//! file is *parsed* and *indexed*. Neither answers the question `groove eval`
//! exists to answer: **once a binary document is in the index, does a query
//! about its contents actually retrieve it?** The production golden set is
//! 26 queries over 49 expected documents, all of them `.md`, so every metric
//! the project tracks is blind to the five formats added in v0.10-0.11.
//!
//! This test closes that gap end to end — index, chunk, embed, fuse, rank —
//! with one query per format.
//!
//! # Design notes (read before changing the fixtures)
//!
//! Two properties keep this from passing for the wrong reason:
//!
//! 1. **Topical words live in the body, never in the filename or the
//!    heading.** Filenames are deliberately meaningless (`b2.pdf`), and the
//!    generated documents use generic headings (`Section`, `Sheet1`). A
//!    chunk's heading is weighted at 2.0 in the FTS side of the fusion and the
//!    title falls back to the filename for these formats, so topical filenames
//!    or headings would let a document rank first even if body extraction were
//!    broken. AU-13 was exactly that failure — a swallowed XML error returning
//!    truncated `.docx` text as success.
//! 2. **Eight Markdown distractors.** With only the five targets in the
//!    corpus, `recall@5` is trivially 1.0 for every query no matter how badly
//!    the parsers behave; the assertion would be vacuous. The distractors make
//!    both `recall@1` and rank position meaningful.
//!
//! Bodies are kept above 80 characters on purpose: the per-chunk quality
//! filter (enabled by default, threshold 0.3) penalises short content and a
//! filtered chunk would fail this test for a reason unrelated to parsing.
//!
//! `#[ignore]`: downloads BGE-small (~130 MB) and builds an index. Runs in
//! nightly via `--include-ignored`, or manually with
//! `cargo test --test eval_binary_formats -- --ignored`.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;

// ---------------------------------------------------------------------------
// Temp KB (tempfile crate is not used in this project; see CLAUDE.local.md)
// ---------------------------------------------------------------------------

/// Root holds the KB plus the sibling `.groove.db` and `groove.toml`, so the
/// `Drop` guard removes the database too (it lands at `kb_path.parent()`).
struct TempKb {
    root: PathBuf,
    kb: PathBuf,
}

impl TempKb {
    fn new(prefix: &str) -> Self {
        // `src/test_support.rs` carries this same naming but is `#[cfg(test)]`
        // gated, so it is reachable from lib unit tests only — integration
        // tests each keep a copy, as `tests/eval_cli.rs` does.
        //
        // The counter is not redundant with the clock: a test binary runs its
        // tests on parallel threads of one process, so PID and nanos alone
        // collide (AU-54).
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let pid = std::process::id();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let root = std::env::temp_dir().join(format!("{prefix}-{pid}-{nanos}-{seq}"));
        let kb = root.join("kb");
        std::fs::create_dir_all(&kb).unwrap();
        Self { root, kb }
    }

    fn kb(&self) -> &Path {
        &self.kb
    }

    fn write_text(&self, rel: &str, content: &str) {
        std::fs::write(self.kb.join(rel), content).unwrap();
    }

    fn write_bytes(&self, rel: &str, content: &[u8]) {
        std::fs::write(self.kb.join(rel), content).unwrap();
    }

    /// `groove.toml` next to the KB, enabling every parser under test.
    fn write_config(&self) -> PathBuf {
        let cfg = self.root.join("groove.toml");
        std::fs::write(
            &cfg,
            "[parsers]\nenabled = [\"md\", \"pdf\", \"docx\", \"xlsx\", \"pptx\"]\n",
        )
        .unwrap();
        cfg
    }
}

impl Drop for TempKb {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

// ---------------------------------------------------------------------------
// Fixture generators
// ---------------------------------------------------------------------------

/// A one-page PDF carrying `body` as its only text run.
///
/// Offsets are recorded while the objects are emitted and the xref table is
/// built from them, so the file stays valid when the body length changes —
/// unlike the hand-written fixtures under `tests/fixtures/binary/`, whose
/// offsets are literals.
///
/// No `/Info /Title` entry: the title then falls back to the filename, which
/// is deliberately meaningless (see the module docs).
fn make_pdf(body: &str) -> Vec<u8> {
    assert!(
        !body.contains('(') && !body.contains(')') && !body.contains('\\'),
        "PDF literal strings would need escaping for these characters"
    );
    // The PDF parser treats a page averaging under 50 extracted characters as
    // image-only and skips the document, so a short body would be dropped
    // rather than indexed.
    assert!(
        body.len() >= 50,
        "body must clear the scanned-PDF heuristic (>= 50 chars/page), got {}",
        body.len()
    );

    let stream = format!("BT /F1 12 Tf 72 720 Td ({body}) Tj ET\n");
    let objects = [
        "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
        "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_string(),
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources << /Font << /F1 4 0 R >> >> /Contents 5 0 R >>".to_string(),
        "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_string(),
        format!(
            "<< /Length {} >>\nstream\n{stream}endstream",
            stream.len()
        ),
    ];

    let mut out = Vec::new();
    out.extend_from_slice(b"%PDF-1.4\n");
    let mut offsets = Vec::with_capacity(objects.len());
    for (i, obj) in objects.iter().enumerate() {
        offsets.push(out.len());
        out.extend_from_slice(format!("{} 0 obj\n{obj}\nendobj\n", i + 1).as_bytes());
    }

    let xref_at = out.len();
    out.extend_from_slice(format!("xref\n0 {}\n", objects.len() + 1).as_bytes());
    // Every xref entry is exactly 20 bytes, including the trailing space.
    out.extend_from_slice(b"0000000000 65535 f \n");
    for off in &offsets {
        out.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
    }
    out.extend_from_slice(
        format!(
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref_at}\n%%EOF\n",
            objects.len() + 1
        )
        .as_bytes(),
    );
    out
}

/// A `.docx` whose only Heading1 is the generic word "Section" and whose body
/// paragraph carries `body`. No `docProps/core.xml`, so the title falls back
/// to the filename.
fn make_docx(body: &str) -> Vec<u8> {
    let doc_xml = format!(
        r#"<?xml version="1.0"?><w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:p><w:pPr><w:pStyle w:val="Heading1"/></w:pPr><w:r><w:t>Section</w:t></w:r></w:p><w:p><w:r><w:t>{body}</w:t></w:r></w:p></w:body></w:document>"#
    );
    zip_parts(&[
        (
            "[Content_Types].xml",
            r#"<?xml version="1.0"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/></Types>"#,
        ),
        ("word/document.xml", &doc_xml),
    ])
}

/// An `.xlsx` with a single generically-named sheet holding `body` in A1.
fn make_xlsx(body: &str) -> Vec<u8> {
    let sheet_xml = format!(
        r#"<?xml version="1.0"?><worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData><row r="1"><c r="A1" t="inlineStr"><is><t>{body}</t></is></c></row></sheetData></worksheet>"#
    );
    zip_parts(&[
        (
            "[Content_Types].xml",
            r#"<?xml version="1.0"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/><Override PartName="/xl/worksheets/sheet1.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/></Types>"#,
        ),
        (
            "_rels/.rels",
            r#"<?xml version="1.0"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/></Relationships>"#,
        ),
        (
            "xl/workbook.xml",
            r#"<?xml version="1.0"?><workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><sheets><sheet name="Sheet1" sheetId="1" r:id="rId1"/></sheets></workbook>"#,
        ),
        (
            "xl/_rels/workbook.xml.rels",
            r#"<?xml version="1.0"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/></Relationships>"#,
        ),
        ("xl/worksheets/sheet1.xml", &sheet_xml),
    ])
}

/// A `.pptx` with one slide titled "Slide" and `body` in a second shape.
fn make_pptx(body: &str) -> Vec<u8> {
    let slide_xml = format!(
        r#"<?xml version="1.0"?><p:sld xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main" xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"><p:cSld><p:spTree><p:sp><p:nvSpPr><p:nvPr><p:ph type="title"/></p:nvPr></p:nvSpPr><p:txBody><a:p><a:r><a:t>Slide</a:t></a:r></a:p></p:txBody></p:sp><p:sp><p:txBody><a:p><a:r><a:t>{body}</a:t></a:r></a:p></p:txBody></p:sp></p:spTree></p:cSld></p:sld>"#
    );
    zip_parts(&[
        (
            "[Content_Types].xml",
            r#"<?xml version="1.0"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"/>"#,
        ),
        ("ppt/slides/slide1.xml", &slide_xml),
    ])
}

fn zip_parts(parts: &[(&str, &str)]) -> Vec<u8> {
    let mut buf = Vec::new();
    {
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
        let opts: zip::write::FileOptions<()> =
            zip::write::FileOptions::default().compression_method(zip::CompressionMethod::Deflated);
        for (name, body) in parts {
            zip.start_file(*name, opts).unwrap();
            zip.write_all(body.as_bytes()).unwrap();
        }
        zip.finish().unwrap();
    }
    buf
}

// ---------------------------------------------------------------------------
// Corpus
// ---------------------------------------------------------------------------

/// `(relative path, query, body text)`. One entry per supported format, each
/// on a subject that shares no vocabulary with the others or with the
/// distractors, so a first-place hit is attributable to that document's own
/// extracted text.
struct Target {
    path: &'static str,
    query: &'static str,
    body: &'static str,
}

const TARGETS: &[Target] = &[
    Target {
        path: "a1.md",
        query: "avalanche forecasting from snowpack stability tests",
        body: "Avalanche forecasters dig a snowpit and run compression and extended column tests on the snowpack, grading each layer by hardness and grain form before issuing a slab release rating.",
    },
    Target {
        path: "b2.pdf",
        query: "varroa mite treatment for rooftop beehives",
        body: "Rooftop apiaries in dense cities need a queen excluder above the brood box and an autumn varroa mite treatment, because drifting drones from neighbouring hives spread the parasite quickly.",
    },
    Target {
        path: "c3.docx",
        query: "steam locomotive boiler safety valve pressure",
        body: "A steam locomotive boiler is protected by twin spring loaded safety valves that lift at the working pressure stamped on the firebox plate, venting steam before the crown sheet is uncovered.",
    },
    Target {
        path: "d4.xlsx",
        query: "citrus harvest tonnage from Valencia orange groves",
        body: "Quarterly citrus harvest tonnage recorded across the Valencia orange groves, split by navel and blood orange cultivars and adjusted for the fruit lost to the autumn hailstorm.",
    },
    Target {
        path: "e5.pptx",
        query: "Mercator projection distorting landmass area near the poles",
        body: "The Mercator projection preserves compass bearings for marine navigation but inflates landmass area with increasing latitude, which is why Greenland appears comparable in size to Africa.",
    },
];

/// Markdown documents on unrelated subjects. Without them the corpus is five
/// documents and `recall@5` is 1.0 whatever the parsers do.
const DISTRACTORS: &[(&str, &str)] = &[
    (
        "d01.md",
        "# Notes\n\nSourdough starters ferment on a schedule of wild yeast feedings, and the hydration ratio of the levain decides how open the final crumb structure becomes.\n",
    ),
    (
        "d02.md",
        "# Notes\n\nA harpsichord plucks its strings with quills mounted on jacks, so unlike a fortepiano it cannot vary loudness by how firmly the keyboard is struck.\n",
    ),
    (
        "d03.md",
        "# Notes\n\nMangrove forests trap estuarine sediment among their stilt roots, buffering the shoreline against storm surge and nursing juvenile fish through their first season.\n",
    ),
    (
        "d04.md",
        "# Notes\n\nCuneiform tablets were impressed with a reed stylus into damp clay, and the archives that survive were baked hard by the fires that destroyed the palaces holding them.\n",
    ),
    (
        "d05.md",
        "# Notes\n\nA sailing hull reaches hull speed when its own bow wave length matches the waterline, after which further driving force mostly steepens the wave rather than adding knots.\n",
    ),
    (
        "d06.md",
        "# Notes\n\nLichens pair a fungus with an alga or a cyanobacterium, and because they draw minerals from rainfall alone they are sensitive enough to serve as air quality indicators.\n",
    ),
    (
        "d07.md",
        "# Notes\n\nPhotographic dodging and burning selectively withhold or add enlarger light during a print exposure, lifting shadow detail without flattening the highlights of the negative.\n",
    ),
    (
        "d08.md",
        "# Notes\n\nThe hydraulic ram pump uses the pressure surge of a closing check valve to lift a fraction of the flow far above the source, running without any external power.\n",
    ),
];

// ---------------------------------------------------------------------------
// Test
// ---------------------------------------------------------------------------

fn grooveseek_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_groove"))
}

#[test]
#[ignore = "requires embedding model download (BGE-small ~130 MB) and an index build"]
fn eval_retrieves_every_binary_format_from_body_text() {
    let kb = TempKb::new("groove-eval-binfmt");
    let cfg = kb.write_config();

    for t in TARGETS {
        match t.path.rsplit_once('.').map(|(_, ext)| ext) {
            Some("md") => kb.write_text(t.path, &format!("# Notes\n\n{}\n", t.body)),
            Some("pdf") => kb.write_bytes(t.path, &make_pdf(t.body)),
            Some("docx") => kb.write_bytes(t.path, &make_docx(t.body)),
            Some("xlsx") => kb.write_bytes(t.path, &make_xlsx(t.body)),
            Some("pptx") => kb.write_bytes(t.path, &make_pptx(t.body)),
            other => panic!("no generator for extension {other:?}"),
        }
    }
    for (path, content) in DISTRACTORS {
        kb.write_text(path, content);
    }

    let bin = grooveseek_bin();

    // 1) Index. `[parsers].enabled` is config-only, hence `--config`.
    let out = Command::new(&bin)
        .arg("--config")
        .arg(&cfg)
        .arg("index")
        .arg("--kb-path")
        .arg(kb.kb())
        .arg("--model")
        .arg("bge-small-en-v1.5")
        .output()
        .expect("spawn groove index");
    assert!(
        out.status.success(),
        "index failed: stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );

    // 2) Every document must have made it in. A parser that skips its file
    //    would otherwise show up only as a retrieval miss below, which is a
    //    much harder failure to read.
    let status = Command::new(&bin)
        .arg("--config")
        .arg(&cfg)
        .arg("status")
        .arg("--kb-path")
        .arg(kb.kb())
        .output()
        .expect("spawn groove status");
    let status_err = String::from_utf8_lossy(&status.stderr);
    let expected_docs = TARGETS.len() + DISTRACTORS.len();
    assert!(
        status_err.contains(&format!("Documents: {expected_docs}")),
        "expected {expected_docs} indexed documents, got: {status_err}"
    );

    // 3) Golden file: one query per format, each expecting only its own file.
    let mut golden = String::from("queries:\n");
    for t in TARGETS {
        let id = t.path.replace('.', "-");
        golden.push_str(&format!("  - id: {id}\n"));
        golden.push_str(&format!("    query: \"{}\"\n", t.query));
        golden.push_str("    expected:\n");
        golden.push_str(&format!("      - path: \"{}\"\n", t.path));
    }
    std::fs::write(kb.kb().join(".groove-eval.yml"), &golden).unwrap();

    // 4) Evaluate.
    let out = Command::new(&bin)
        .arg("--config")
        .arg(&cfg)
        .arg("eval")
        .arg("--kb-path")
        .arg(kb.kb())
        .arg("--model")
        .arg("bge-small-en-v1.5")
        .arg("--format")
        .arg("json")
        .output()
        .expect("spawn groove eval");
    assert!(
        out.status.success(),
        "eval failed: stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let report: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("valid JSON from `eval --format json`");

    let per_query = report["per_query"]
        .as_array()
        .unwrap_or_else(|| panic!("per_query must be an array: {report}"));
    assert_eq!(
        per_query.len(),
        TARGETS.len(),
        "expected one result per target format: {report}"
    );

    // 5) Each format's document must rank first for its own query.
    //
    //    Rank 1 — not merely "present in top-k" — is the assertion that
    //    carries information here. `recall@5` and `recall@10` cannot fail
    //    while the corpus is this small even if extraction returns nothing
    //    useful, so asserting them would be vacuous.
    let mut failures = Vec::new();
    for (t, result) in TARGETS.iter().zip(per_query) {
        let top = result["top_k"].as_array().and_then(|hits| hits.first());
        let top_path = top.and_then(|h| h["path"].as_str()).unwrap_or("<none>");
        if top_path != t.path {
            let all: Vec<String> = result["top_k"]
                .as_array()
                .map(|hits| {
                    hits.iter()
                        .map(|h| h["path"].as_str().unwrap_or("?").to_string())
                        .collect()
                })
                .unwrap_or_default();
            failures.push(format!(
                "  {} — query {:?} ranked {:?} first; top_k = {:?}",
                t.path, t.query, top_path, all
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "these formats did not retrieve their own document first:\n{}",
        failures.join("\n")
    );

    // 6) Aggregate recall@1 restates the above through the metric the project
    //    actually tracks, so a change in how recall is computed shows up here
    //    rather than silently diverging from the per-query check.
    let recall_at_1 = report["aggregate"]["recall_at_k"]["1"]
        .as_f64()
        .unwrap_or_else(|| panic!("aggregate.recall_at_k.1 must be a number: {report}"));
    assert!(
        (recall_at_1 - 1.0).abs() < f64::EPSILON,
        "expected aggregate recall@1 == 1.0 across the five formats, got {recall_at_1}"
    );
}
