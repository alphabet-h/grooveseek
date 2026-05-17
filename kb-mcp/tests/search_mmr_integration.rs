//! End-to-end integration tests for the MMR re-rank pipeline (PR-2).
//!
//! These tests exercise the **MCP `search` tool path** through a real
//! `kb-mcp serve --transport http` subprocess, because that is the only
//! call-site where MMR is actually plumbed (CLI `kb-mcp search` parses
//! `--mmr` flags but currently discards them — see `src/main.rs` Task 2.9
//! comment "MMR / parent-retriever flags are parsed-but-not-yet-pipeline-active").
//!
//! All tests are `#[ignore]` because they need:
//! - a built `kb-mcp` binary (`cargo build` first)
//! - the BGE-small model on disk (~130 MB DL on first run)
//! - network access for the initial model fetch
//! - a free TCP port + `curl` on `PATH`
//!
//! Run with:
//! ```text
//! cargo test --test search_mmr_integration -- --ignored
//! ```
//!
//! The same trade-offs as `tests/http_transport.rs`: we use `curl` instead
//! of pulling in `reqwest` to keep the dev-dep surface small. The MCP
//! Streamable HTTP transport returns Server-Sent Events by default
//! (`text/event-stream`), so the helper below grabs the first `data:` line
//! out of the body and parses it as a JSON-RPC envelope.
//!
//! ## What the 3 scenarios cover
//!
//! 1. `test_mmr_off_matches_legacy_search_chunk_id_order` —
//!    Two `mmr: false` requests against the same KB+query produce the
//!    *same* (path, heading) sequence (= MMR-off path is deterministic
//!    and does not perturb the legacy bit-exact ordering invariant #3).
//!    We compare `(path, heading)` tuples rather than the f32 score
//!    itself: BGE/ONNX/SIMD scores are not bit-exact across OS/CPU and
//!    the integration harness must run on Windows + Linux + macOS.
//!
//! 2. `test_mmr_per_call_override_beats_toml` —
//!    `kb-mcp.toml` says `[search.mmr] enabled = false`, the request
//!    passes `mmr: true` with a low `mmr_lambda` (= diversity-leaning).
//!    We assert the result *differs* from the MMR-off baseline when
//!    the candidate pool has enough material for MMR to reorder. (We
//!    use a deliberately redundant fixture so that a low-lambda MMR
//!    pass has something to do; otherwise MMR-on and MMR-off can
//!    coincide and the assertion would be meaningless.)
//!
//! 3. `test_mmr_lambda_warn_when_mmr_off` —
//!    A request with `mmr: false` + `mmr_lambda: 0.3` emits a
//!    `tracing::warn!` per `SearchOverrides::resolve` (see
//!    `src/config.rs` "footgun guard"). Asserting the actual log line
//!    requires `tracing-test` (not a current dep), so this test only
//!    smoke-checks that the request *succeeds* — i.e. an out-of-band
//!    `lambda` is silently ignored, not turned into an error. The warn
//!    emission itself is verified by code review of `config.rs`.

mod common;
use common::mcp::{
    build_index, extract_path_heading_order, mcp_initialize, mcp_search_call, spawn_mcp_server,
};
use common::temp::TempKbLayout;

/// Build a small KB with **deliberately redundant** content so MMR has
/// something to dedupe. Three docs, each with two sections covering very
/// similar Rust async / tokio material — enough overlap that an MMR pass
/// with `lambda < 0.5` will reorder away from a pure relevance ranking.
fn build_test_kb(layout: &TempKbLayout) {
    layout.write(
        "tokio_one.md",
        concat!(
            "---\ntitle: Tokio Async Runtime One\ntags: [rust, tokio]\n---\n",
            "\n",
            "## tokio runtime\n",
            "\n",
            "The tokio runtime is an async executor for Rust that drives ",
            "futures to completion. It uses a multi-threaded scheduler with ",
            "work-stealing for high throughput in concurrent rust programs.\n",
            "\n",
            "## tokio tasks\n",
            "\n",
            "tokio::spawn creates a task that the tokio runtime polls. Each ",
            "task in the rust async ecosystem runs cooperatively until it ",
            "yields at the next .await point.\n",
        ),
    );
    layout.write(
        "tokio_two.md",
        concat!(
            "---\ntitle: Tokio Async Runtime Two\ntags: [rust, tokio]\n---\n",
            "\n",
            "## async tokio basics\n",
            "\n",
            "Async rust with tokio uses futures, the .await operator, and ",
            "the tokio runtime to drive non-blocking I/O. The tokio runtime ",
            "scheduler is the heart of every async rust application.\n",
            "\n",
            "## tokio macros\n",
            "\n",
            "The #[tokio::main] macro wraps an async fn into a synchronous ",
            "entry point that constructs a tokio runtime under the hood.\n",
        ),
    );
    layout.write(
        "rayon.md",
        concat!(
            "---\ntitle: Rayon Data Parallel\ntags: [rust, parallel]\n---\n",
            "\n",
            "## rayon basics\n",
            "\n",
            "Rayon is a data-parallel library for rust. It is not async ",
            "but uses work-stealing similar to tokio's scheduler model. ",
            "Use rayon when CPU-bound, tokio when I/O-bound.\n",
        ),
    );
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Scenario 1: with MMR off (both via toml *and* via per-call), the
/// `(path, heading)` sequence is deterministic across two identical
/// requests. This guards invariant #3 (MMR-off path is bit-exact wrt
/// pre-MMR behavior — equivalent to "calling search twice gives the
/// same result", since the pre-MMR pipeline is the same code path).
#[test]
#[ignore = "requires built binary, BGE-small model download, free TCP port"]
fn test_mmr_off_matches_legacy_search_chunk_id_order() {
    let layout = TempKbLayout::new("kb-mcp-mmr-it-off");
    build_test_kb(&layout);
    build_index(layout.kb());

    // toml with MMR explicitly off (= legacy code path).
    let cfg_path = layout.root().join("kb-mcp.toml");
    std::fs::write(
        &cfg_path,
        "[search.mmr]\nenabled = false\nlambda = 0.7\nsame_doc_penalty = 0.0\n",
    )
    .unwrap();

    let (_guard, base) = spawn_mcp_server(layout.kb(), &cfg_path);
    let session = mcp_initialize(&base);

    let args = serde_json::json!({
        "query": "tokio runtime async rust",
        "limit": 5,
        "mmr": false,
    });
    let r1 = mcp_search_call(&base, &session, args.clone());
    let r2 = mcp_search_call(&base, &session, args);

    let order1 = extract_path_heading_order(&r1);
    let order2 = extract_path_heading_order(&r2);
    assert!(!order1.is_empty(), "first search returned no results: {r1}");
    assert_eq!(
        order1, order2,
        "MMR-off path must produce the same (path, heading) order across two identical requests \
         (= bit-exact legacy invariant #3). Got:\n  r1={order1:?}\n  r2={order2:?}"
    );
}

/// Scenario 2: per-call `mmr: true` overrides toml `enabled = false` and
/// changes the result order on a deliberately redundant KB. This is the
/// "knob actually does something" smoke. We don't compare to a hand-rolled
/// expected order — that would be brittle across model versions — only
/// that the order is *different* from the MMR-off baseline.
///
/// Note: because the candidate pool size is small (3 docs, 5 chunks total)
/// and the BGE-small model is deterministic, in pathological cases MMR-on
/// could coincidentally produce the same order as MMR-off. To make this
/// robust we crank `mmr_lambda` low (= heavy diversity bias) and
/// `mmr_same_doc_penalty` high (= force selecting from different docs).
/// If this still fails reproducibly, the fixture above needs more docs
/// with overlap.
#[test]
#[ignore = "requires built binary, BGE-small model download, free TCP port"]
fn test_mmr_per_call_override_beats_toml() {
    let layout = TempKbLayout::new("kb-mcp-mmr-it-override");
    build_test_kb(&layout);
    build_index(layout.kb());

    // toml says MMR off — per-call override flips it on.
    let cfg_path = layout.root().join("kb-mcp.toml");
    std::fs::write(
        &cfg_path,
        "[search.mmr]\nenabled = false\nlambda = 0.7\nsame_doc_penalty = 0.0\n",
    )
    .unwrap();

    let (_guard, base) = spawn_mcp_server(layout.kb(), &cfg_path);
    let session = mcp_initialize(&base);

    let baseline = mcp_search_call(
        &base,
        &session,
        serde_json::json!({
            "query": "tokio runtime async rust",
            "limit": 5,
            "mmr": false,
        }),
    );
    let mmr_on = mcp_search_call(
        &base,
        &session,
        serde_json::json!({
            "query": "tokio runtime async rust",
            "limit": 5,
            "mmr": true,
            "mmr_lambda": 0.1,            // heavy diversity bias
            "mmr_same_doc_penalty": 0.5,  // strong intra-doc penalty
        }),
    );

    let baseline_order = extract_path_heading_order(&baseline);
    let mmr_order = extract_path_heading_order(&mmr_on);
    assert!(
        !baseline_order.is_empty() && !mmr_order.is_empty(),
        "expected non-empty results. baseline={baseline_order:?}, mmr={mmr_order:?}"
    );
    assert_ne!(
        baseline_order, mmr_order,
        "per-call mmr=true with low lambda + high same_doc_penalty must differ from MMR-off \
         baseline on a redundant KB. baseline={baseline_order:?}, mmr={mmr_order:?}. \
         If this assertion fires, the fixture in build_test_kb may not have enough \
         intra-doc overlap to make MMR reorder."
    );
}

/// Scenario 3: passing `mmr_lambda` while `mmr` is explicitly false (or
/// implicitly off via toml) is a "footgun" pattern — `SearchOverrides::resolve`
/// silently ignores the lambda but emits `tracing::warn!` exactly once.
///
/// We can't assert on the warn line itself without `tracing-test` (not
/// currently a dep), so this test is a smoke: the request must complete
/// successfully and return well-formed results. The warn emission is
/// covered by `src/config.rs::test_search_overrides_resolve_warn_emitted_when_mmr_off_with_lambda`.
#[test]
#[ignore = "requires built binary, BGE-small model download, free TCP port"]
fn test_mmr_lambda_warn_when_mmr_off() {
    let layout = TempKbLayout::new("kb-mcp-mmr-it-warn");
    build_test_kb(&layout);
    build_index(layout.kb());

    let cfg_path = layout.root().join("kb-mcp.toml");
    std::fs::write(
        &cfg_path,
        "[search.mmr]\nenabled = false\nlambda = 0.7\nsame_doc_penalty = 0.0\n",
    )
    .unwrap();

    let (_guard, base) = spawn_mcp_server(layout.kb(), &cfg_path);
    let session = mcp_initialize(&base);

    // mmr explicitly false + lambda set → ghost lambda. Must not error.
    let resp = mcp_search_call(
        &base,
        &session,
        serde_json::json!({
            "query": "tokio async",
            "limit": 3,
            "mmr": false,
            "mmr_lambda": 0.3,
        }),
    );

    // The wrapper shape (results / low_confidence / filter_applied) must be
    // present — i.e. the request completed instead of returning ErrorResponse.
    assert!(
        resp.get("results").is_some(),
        "ghost-lambda request must still return a SearchResponse (got: {resp})"
    );
    assert!(
        resp.get("low_confidence").is_some(),
        "ghost-lambda request must include low_confidence flag (got: {resp})"
    );
    let order = extract_path_heading_order(&resp);
    assert!(
        !order.is_empty(),
        "ghost-lambda request must still produce hits (got empty results: {resp})"
    );
}
