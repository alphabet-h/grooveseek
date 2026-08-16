//! End-to-end coverage for `groove graph --format`.
//!
//! `#[ignore]`: building an index downloads the embedding model (BGE-small,
//! ~130 MB). Run with `cargo test --test graph_cli -- --ignored`.
//!
//! The rendering itself is unit-tested in `src/graph_render.rs`; what these
//! tests pin is the part only a real process shows — that each format reaches
//! stdout, that the default did not move, and that the drawings did not leak
//! onto `search`.

mod common;

use common::temp::TempKbLayout;
use std::path::PathBuf;
use std::process::Command;

fn grooveseek_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_groove"))
}

/// A knowledge base with enough overlap that a walk from one document reaches
/// the others.
fn indexed_kb() -> TempKbLayout {
    let kb = TempKbLayout::new("groove-graph-cli");
    kb.write(
        "rrf.md",
        "# RRF\n\nReciprocal Rank Fusion merges the vector and keyword rankings \
         with a constant k of 60, which is the fusion step of hybrid search.\n",
    );
    kb.write(
        "hybrid.md",
        "# Hybrid search\n\nHybrid search runs a vector ranking and a keyword \
         ranking and fuses them, which is where Reciprocal Rank Fusion applies.\n",
    );
    kb.write(
        "chunks.md",
        "# Chunks\n\nChunks are split by heading and deduplicated by hash before \
         the vector ranking ever sees them.\n",
    );

    let status = Command::new(grooveseek_bin())
        .arg("index")
        .arg("--kb-path")
        .arg(kb.kb())
        .arg("--model")
        .arg("bge-small-en-v1.5")
        .status()
        .expect("spawn groove index");
    assert!(status.success(), "index failed");
    kb
}

fn graph(kb: &TempKbLayout, extra: &[&str]) -> std::process::Output {
    let mut cmd = Command::new(grooveseek_bin());
    cmd.arg("graph")
        .arg("--start")
        .arg("rrf.md")
        .arg("--kb-path")
        .arg(kb.kb())
        .arg("--model")
        .arg("bge-small-en-v1.5");
    for a in extra {
        cmd.arg(a);
    }
    cmd.output().expect("spawn groove graph")
}

#[test]
#[ignore]
fn graph_renders_every_format_to_stdout() {
    let kb = indexed_kb();

    // Default: unchanged by this feature. A caller parsing `groove graph`
    // output must keep getting JSON without asking for it.
    let out = graph(&kb, &[]);
    assert!(out.status.success(), "default format failed");
    let v: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("the default output is still JSON");
    assert!(
        v["nodes"]
            .as_array()
            .map(|a| !a.is_empty())
            .unwrap_or(false),
        "the walk found nothing to draw: {v}"
    );

    let out = graph(&kb, &["--format", "dot"]);
    assert!(out.status.success(), "dot format failed");
    let dot = String::from_utf8_lossy(&out.stdout);
    assert!(dot.starts_with("digraph grooveseek_graph {"), "{dot}");
    assert!(dot.trim_end().ends_with('}'), "{dot}");
    assert!(dot.contains("rrf.md"), "the start document is in it: {dot}");

    let out = graph(&kb, &["--format", "svg"]);
    assert!(out.status.success(), "svg format failed");
    let svg = String::from_utf8_lossy(&out.stdout);
    assert!(svg.starts_with("<svg xmlns="), "{svg}");
    assert!(svg.trim_end().ends_with("</svg>"), "{svg}");
    // Standalone is the point: it has to open with nothing else installed.
    assert!(!svg.contains("<image"), "{svg}");
    assert!(!svg.contains("@import"), "{svg}");

    let out = graph(&kb, &["--format", "text"]);
    assert!(out.status.success(), "text format failed");
    assert!(String::from_utf8_lossy(&out.stdout).contains("# Connection graph from:"));
}

/// The drawings belong to `graph` alone. They were kept off the shared format
/// enum so that `search`, which has no graph to draw, cannot be asked for one.
#[test]
#[ignore]
fn search_does_not_accept_the_graph_only_formats() {
    let kb = indexed_kb();
    for fmt in ["dot", "svg"] {
        let out = Command::new(grooveseek_bin())
            .arg("search")
            .arg("hybrid")
            .arg("--kb-path")
            .arg(kb.kb())
            .arg("--model")
            .arg("bge-small-en-v1.5")
            .arg("--format")
            .arg(fmt)
            .output()
            .expect("spawn groove search");
        assert!(
            !out.status.success(),
            "search --format {fmt} must be rejected"
        );
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            stderr.contains("invalid value") || stderr.contains("possible values"),
            "clap should explain the rejection: {stderr}"
        );
    }
}

/// A drawing that silently shows part of the neighbourhood is worse than a
/// small one, so both formats have to say when a limit cut the walk short.
#[test]
#[ignore]
fn truncation_is_visible_in_both_drawings() {
    let kb = indexed_kb();
    for (fmt, needle) in [("dot", "truncated:"), ("svg", "truncated:")] {
        let out = graph(&kb, &["--format", fmt, "--max-nodes", "2"]);
        assert!(out.status.success(), "{fmt} with a low node budget failed");
        let text = String::from_utf8_lossy(&out.stdout);
        assert!(
            text.contains(needle),
            "{fmt} output must report the truncation: {text}"
        );
    }
}
