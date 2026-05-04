//! End-to-end integration tests for the Parent retriever pipeline (PR-3).
//!
//! These tests exercise the **MCP `search` tool path** through a real
//! `kb-mcp serve --transport http` subprocess, mirroring the pattern in
//! `tests/search_mmr_integration.rs` (PR-2). The Parent retriever is wired
//! at the `apply_parent_retriever` call-site in
//! `src/server.rs::run_search_pipeline`, plus the post-expansion
//! `compute_match_spans` recomputation a few lines below; both paths are
//! covered here end-to-end.
//!
//! All tests are `#[ignore]` because they need:
//! - a built `kb-mcp` binary (`cargo build` first)
//! - the BGE-small model on disk (~130 MB DL on first run, cache hit
//!   afterwards because PR-2 already paid the download)
//! - network access for the initial model fetch
//! - a free TCP port + `curl` on `PATH`
//!
//! Run with:
//! ```text
//! cargo test --test search_parent_integration -- --ignored
//! ```
//!
//! ## What the 2 scenarios cover
//!
//! 1. `test_parent_expanded_from_set_when_enabled` —
//!    With `[search.parent_retriever] enabled = true`, a search hit on the
//!    middle chunk of a 3-chunk document returns `expanded_from = Some(...)`
//!    in the JSON response, and the `content` of the top hit contains text
//!    that lives in *adjacent* chunks (= the merge wire actually fired).
//!    This is the "wire is connected" smoke test.
//!
//! 2. `test_parent_match_spans_recomputed_on_expanded_content` —
//!    `expand_parent` defensively clears `match_spans = None` (`src/parent.rs`
//!    line 139 / 183), and the caller (`run_search_pipeline`) recomputes via
//!    `compute_match_spans` against the post-expansion `content`. We assert
//!    that `match_spans` comes back as `Some([...])` with at least one span
//!    whose offsets land **inside the expanded content** and slice to the
//!    query word — i.e. offsets are valid against the merged content, not
//!    leaked from the pre-expansion chunk.
//!
//! Helpers are imported from `tests/common/mcp.rs` (extracted in feature-34 / F-55).

mod common;
use common::mcp::{build_index, mcp_initialize, mcp_search_call, spawn_mcp_server};
use common::temp::TempKbLayout;

// ---------------------------------------------------------------------------
// Fixture: a single document with three sections each large enough to
// exceed `whole_doc_threshold_tokens = 100` (~400 byte content) so the
// adjacent-merge path fires (small-chunk path = whole-doc fallback would
// still satisfy assertion #1, but assertion #2 specifically wants
// adjacent merge — so we size sections accordingly).
//
// Marker words "alpha" / "beta" / "gamma" let assertion #1 verify that
// neighbors are present in the expanded content. Search query targets
// "beta" so the middle chunk wins and the merge spans both flanks.
//
// We add a second short doc to widen the candidate pool slightly so the
// search engine has at least 2 docs to choose between (FTS5 likes that).
// ---------------------------------------------------------------------------

fn build_test_kb(layout: &TempKbLayout) {
    layout.write(
        "doc1.md",
        concat!(
            "---\n",
            "title: Greek Letter Doc\n",
            "tags: [letters]\n",
            "---\n",
            "\n",
            "## alpha section\n",
            "\n",
            // ~600 byte body, repeated phrase to push token_count above 100
            "alpha discusses the first letter of the greek alphabet. ",
            "It comes before beta and gamma in the standard ordering. ",
            "We use alpha to mean leading or primary in many contexts. ",
            "Alpha alpha alpha alpha alpha alpha alpha alpha alpha alpha. ",
            "alpha discusses the first letter of the greek alphabet. ",
            "It comes before beta and gamma in the standard ordering. ",
            "\n",
            "## beta section\n",
            "\n",
            "beta discusses the second letter of the greek alphabet. ",
            "It sits between alpha and gamma in the standard ordering. ",
            "Beta beta beta beta beta beta beta beta beta beta beta beta. ",
            "We sometimes call a release candidate a beta version. ",
            "beta discusses the second letter of the greek alphabet. ",
            "It sits between alpha and gamma in the standard ordering. ",
            "\n",
            "## gamma section\n",
            "\n",
            "gamma discusses the third letter of the greek alphabet. ",
            "It comes after alpha and beta in the standard ordering. ",
            "Gamma gamma gamma gamma gamma gamma gamma gamma gamma gamma. ",
            "Gamma rays are high energy electromagnetic radiation. ",
            "gamma discusses the third letter of the greek alphabet. ",
            "It comes after alpha and beta in the standard ordering. ",
            "\n",
        ),
    );
    layout.write(
        "doc2.md",
        concat!(
            "---\n",
            "title: Unrelated Side Doc\n",
            "tags: [misc]\n",
            "---\n",
            "\n",
            "## delta\n",
            "\n",
            "delta discusses the fourth letter, kept here only to ",
            "give the index a second document so the candidate pool ",
            "is not pathologically tiny.\n",
        ),
    );
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Scenario 1: with `[search.parent_retriever] enabled = true`, a search hit
/// on the "beta" middle chunk returns `expanded_from = Some(...)` and the
/// expanded `content` includes the alpha/gamma flanking sections.
#[test]
#[ignore = "spawns kb-mcp serve which loads embedding model"]
fn test_parent_expanded_from_set_when_enabled() {
    let layout = TempKbLayout::new("kb-mcp-parent-it-expanded");
    build_test_kb(&layout);
    build_index(layout.kb());

    let cfg_path = layout.root().join("kb-mcp.toml");
    std::fs::write(
        &cfg_path,
        // whole_doc_threshold/max_expanded explicit for clarity even though
        // both match the defaults in src/config.rs.
        concat!(
            "[search.parent_retriever]\n",
            "enabled = true\n",
            "whole_doc_threshold_tokens = 100\n",
            "max_expanded_tokens = 2000\n",
        ),
    )
    .unwrap();

    let (_guard, base) = spawn_mcp_server(layout.kb(), &cfg_path);
    let session = mcp_initialize(&base);

    let resp = mcp_search_call(
        &base,
        &session,
        serde_json::json!({
            "query": "beta letter greek alphabet ordering",
            "limit": 3,
        }),
    );

    let results = resp
        .get("results")
        .and_then(|v| v.as_array())
        .unwrap_or_else(|| panic!("response missing `results` array: {resp}"));
    assert!(
        !results.is_empty(),
        "expected at least one result, got empty: {resp}"
    );

    // Find the hit on doc1.md (the multi-section doc). It should not
    // strictly need to be #1 — beta is the most relevant — but doc2.md
    // is deliberately unrelated, so doc1 ought to dominate. We pick the
    // first hit whose path is doc1.md to make this robust to any
    // tie-breaker drift.
    let doc1_hit = results
        .iter()
        .find(|h| {
            h.get("path")
                .and_then(|p| p.as_str())
                .is_some_and(|p| p.ends_with("doc1.md"))
        })
        .unwrap_or_else(|| panic!("no doc1.md hit in results: {results:?}"));

    let expanded = doc1_hit
        .get("expanded_from")
        .unwrap_or_else(|| panic!("doc1 hit missing `expanded_from` key: {doc1_hit}"));
    assert!(
        !expanded.is_null(),
        "expanded_from should be Some(...) when parent_retriever=true, got null: {doc1_hit}"
    );
    // The doc has 3 sections each with token_count ~120; threshold = 100, so
    // we expect Adjacent (not WholeDocument). But assert only that a `kind`
    // field is present (snake_case-tagged enum) — fixture chunking is the
    // markdown chunker's call, and we shouldn't couple this test to its
    // exact splitting.
    assert!(
        expanded.get("kind").and_then(|v| v.as_str()).is_some(),
        "expanded_from should be a tagged enum object with `kind`: {expanded}"
    );

    // Content should have grown to include neighboring sections. We hit the
    // beta chunk; the merge should pull in at least one of alpha / gamma.
    let content = doc1_hit
        .get("content")
        .and_then(|v| v.as_str())
        .unwrap_or_else(|| panic!("doc1 hit missing `content` string: {doc1_hit}"));
    assert!(
        content.contains("beta"),
        "expanded content should still contain the original `beta` text: {content:?}"
    );
    assert!(
        content.contains("alpha") || content.contains("gamma"),
        "expanded content should contain at least one neighbor (alpha or gamma); \
         got content snippet: {}",
        &content.chars().take(400).collect::<String>()
    );
}

/// Scenario 2: after `expand_parent` clears `match_spans = None` defensively,
/// the caller (`run_search_pipeline`) recomputes spans against the
/// post-expansion `content`. This test asserts that `match_spans` comes back
/// populated and that each span's `[start, end)` slice yields the (case-
/// folded) query word — i.e. the recomputation actually ran on the merged
/// content, not on the pre-expansion chunk.
///
/// We deliberately query a word that occurs in **multiple** sections so that
/// after the merge there is more than one match position, exercising the
/// multi-span path in `compute_match_spans`.
#[test]
#[ignore = "spawns kb-mcp serve which loads embedding model"]
fn test_parent_match_spans_recomputed_on_expanded_content() {
    let layout = TempKbLayout::new("kb-mcp-parent-it-spans");
    build_test_kb(&layout);
    build_index(layout.kb());

    let cfg_path = layout.root().join("kb-mcp.toml");
    std::fs::write(
        &cfg_path,
        concat!(
            "[search.parent_retriever]\n",
            "enabled = true\n",
            "whole_doc_threshold_tokens = 100\n",
            "max_expanded_tokens = 2000\n",
        ),
    )
    .unwrap();

    let (_guard, base) = spawn_mcp_server(layout.kb(), &cfg_path);
    let session = mcp_initialize(&base);

    // "alphabet" appears once in every greek-letter section (alpha / beta /
    // gamma). After parent expansion of the beta chunk, the merged content
    // should contain "alphabet" at least twice.
    let resp = mcp_search_call(
        &base,
        &session,
        serde_json::json!({
            "query": "alphabet",
            "limit": 3,
        }),
    );

    let results = resp
        .get("results")
        .and_then(|v| v.as_array())
        .unwrap_or_else(|| panic!("response missing `results`: {resp}"));
    assert!(
        !results.is_empty(),
        "expected at least one result, got empty: {resp}"
    );

    let doc1_hit = results
        .iter()
        .find(|h| {
            h.get("path")
                .and_then(|p| p.as_str())
                .is_some_and(|p| p.ends_with("doc1.md"))
        })
        .unwrap_or_else(|| panic!("no doc1.md hit in results: {results:?}"));

    // expanded_from must be set (parent retriever ran).
    let expanded = doc1_hit
        .get("expanded_from")
        .unwrap_or_else(|| panic!("doc1 hit missing expanded_from: {doc1_hit}"));
    assert!(
        !expanded.is_null(),
        "expanded_from should be Some(...) on doc1 hit: {doc1_hit}"
    );

    // match_spans must be present (= recomputed against expanded content,
    // NOT the defensive `None` left by expand_parent).
    let content = doc1_hit
        .get("content")
        .and_then(|v| v.as_str())
        .unwrap_or_else(|| panic!("doc1 hit missing content: {doc1_hit}"));
    let spans = doc1_hit
        .get("match_spans")
        .and_then(|v| v.as_array())
        .unwrap_or_else(|| {
            panic!(
                "match_spans should be present (and an array) on the expanded hit. \
                 If this fires it usually means run_search_pipeline did not recompute \
                 spans after apply_parent_retriever — see src/server.rs around the \
                 `for h in &mut hits {{ h.match_spans = compute_match_spans(...) }}` loop. \
                 doc1_hit = {doc1_hit}"
            )
        });
    assert!(
        !spans.is_empty(),
        "match_spans should contain at least one match for `alphabet` in expanded content. \
         content len = {}, spans = {spans:?}",
        content.len()
    );

    // Each span must slice to the (case-folded) query word *within* the
    // expanded content boundary. If recomputation had been skipped we would
    // see either None / [] (defensive clear left as-is) or stale offsets
    // pointing past the original chunk boundary.
    for (i, span) in spans.iter().enumerate() {
        let start = span
            .get("start")
            .and_then(|v| v.as_u64())
            .unwrap_or_else(|| panic!("span[{i}] missing `start`: {span}"))
            as usize;
        let end =
            span.get("end")
                .and_then(|v| v.as_u64())
                .unwrap_or_else(|| panic!("span[{i}] missing `end`: {span}")) as usize;
        assert!(
            end <= content.len(),
            "span[{i}] end ({end}) must be within expanded content (len {}); \
             possible stale offset from pre-expansion chunk. spans={spans:?}",
            content.len()
        );
        assert!(
            start < end,
            "span[{i}] start ({start}) must be < end ({end}): {span}"
        );
        let slice = content
            .get(start..end)
            .unwrap_or_else(|| panic!("span[{i}] {start}..{end} not on a char boundary"));
        assert_eq!(
            slice.to_ascii_lowercase(),
            "alphabet",
            "span[{i}] should slice to the query word (case-insensitive); got {slice:?}"
        );
    }
}
