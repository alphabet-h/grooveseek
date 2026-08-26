//! End-to-end integration tests for `-term` exclusion (feature-55 / F-4) over
//! the MCP `search` tool.
//!
//! Both halves of the hybrid drop the excluded rows, and they do it by
//! different mechanisms — the full-text leg with a `NOT` inside the MATCH
//! expression, the vector leg by subtracting the ids that same expression
//! matches. A unit test can show either one working on its own; only a real
//! search through a real index shows that a hit cannot slip past *both*.
//! That is what these tests are for, and it is why they run against a spawned
//! `groove serve --transport http` rather than against the pipeline function.
//!
//! `#[ignore]`, with the same requirements as `tests/search_mmr_integration.rs`:
//! a built `groove` binary, the BGE-small model on disk (~130 MB on first
//! run), a free TCP port, and `curl` on `PATH`.
//!
//! Run with:
//! ```text
//! cargo test --test search_exclusion_integration -- --ignored
//! ```

mod common;
use common::mcp::{build_index, mcp_initialize, mcp_search_call, spawn_mcp_server};
use common::temp::TempKbLayout;

/// Three documents that a single query reaches, one of which is about the
/// term the query excludes.
///
/// The excluded word is planted in more than one place on purpose: `rayon.md`
/// carries it in its title, its heading and its body, and `tokio_two.md`
/// mentions it in passing inside a document that is otherwise on topic. The
/// second one is the interesting case — the exclusion is a statement about the
/// *chunk*, not about the document, so a chunk that merely name-drops `rayon`
/// is dropped as well, and a test that only planted the term in an off-topic
/// document could not tell "excluded" from "ranked low".
fn build_test_kb(layout: &TempKbLayout) {
    layout.write(
        "tokio_one.md",
        concat!(
            "---\ntitle: Tokio Async Runtime\ntags: [rust, tokio]\n---\n",
            "\n",
            "## tokio runtime\n",
            "\n",
            "The tokio runtime is an async executor for rust that drives ",
            "futures to completion. It uses a multi-threaded scheduler with ",
            "work-stealing for high throughput in concurrent rust programs.\n",
        ),
    );
    layout.write(
        "tokio_two.md",
        concat!(
            "---\ntitle: Comparing Runtimes\ntags: [rust, tokio]\n---\n",
            "\n",
            "## runtime comparison\n",
            "\n",
            "The tokio runtime drives async I/O, while rayon drives CPU-bound ",
            "work; a rust program that does both often carries a tokio runtime ",
            "and a rayon thread pool side by side.\n",
        ),
    );
    layout.write(
        "rayon.md",
        concat!(
            "---\ntitle: Rayon Data Parallel\ntags: [rust, parallel]\n---\n",
            "\n",
            "## rayon runtime\n",
            "\n",
            "Rayon is a data-parallel library for rust. Its runtime is a ",
            "work-stealing thread pool, which is the same idea the tokio ",
            "scheduler is built on.\n",
        ),
    );
}

/// A `-term` group removes the term from **every** hit, on both legs, and the
/// response says which phrases did it.
///
/// The control run matters as much as the excluded one: without it, an empty
/// result set would pass the "no hit mentions rayon" assertion trivially. So
/// the same query is run twice, and the test requires the plain run to return
/// a hit that the excluded run does not.
#[test]
#[ignore = "requires built binary, BGE-small model download, free TCP port"]
fn an_excluded_term_is_absent_from_every_hit_over_mcp() {
    let layout = TempKbLayout::new("groove-exclusion-it");
    build_test_kb(&layout);
    build_index(layout.kb());

    let cfg_path = layout.root().join("groove.toml");
    std::fs::write(&cfg_path, "[watch]\nenabled = false\n").unwrap();

    let (_guard, base) = spawn_mcp_server(layout.kb(), &cfg_path);
    let session = mcp_initialize(&base);

    let plain = mcp_search_call(
        &base,
        &session,
        serde_json::json!({"query": "tokio runtime", "limit": 10}),
    );
    let mentions_rayon = |resp: &serde_json::Value| -> Vec<String> {
        resp["results"]
            .as_array()
            .unwrap_or_else(|| panic!("no results array: {resp}"))
            .iter()
            .filter(|hit| {
                hit["content"]
                    .as_str()
                    .unwrap_or_default()
                    .to_lowercase()
                    .contains("rayon")
            })
            .map(|hit| hit["path"].as_str().unwrap_or_default().to_string())
            .collect()
    };
    assert!(
        !mentions_rayon(&plain).is_empty(),
        "the fixture has to put the excluded term in reach of the query, or \
         the run below proves nothing: {plain}"
    );

    let excluded = mcp_search_call(
        &base,
        &session,
        serde_json::json!({"query": "tokio runtime -rayon", "limit": 10}),
    );
    assert!(
        mentions_rayon(&excluded).is_empty(),
        "no hit may carry the excluded term, on either leg of the hybrid: {excluded}"
    );
    assert!(
        !excluded["results"]
            .as_array()
            .unwrap_or_else(|| panic!("no results array: {excluded}"))
            .is_empty(),
        "the exclusion must narrow the answer, not empty it: {excluded}"
    );
    assert_eq!(
        excluded["filter_applied"]["excluded_terms"],
        serde_json::json!(["rayon"]),
        "the response has to name what it dropped, or an over-broad exclusion \
         is invisible to the caller: {excluded}"
    );
    assert!(
        plain["filter_applied"].get("excluded_terms").is_none(),
        "a query with no exclusions leaves the key out: {plain}"
    );
}

/// The refusal, over the tool rather than in a unit test: a query made only of
/// exclusions is answered with the `error` envelope and no `results` key.
///
/// `tests/mcp_protocol_surface.rs` pins the same refusal on an empty index and
/// checks where it sits in the JSON-RPC envelope. This one runs it against a
/// real index, which is the case where the search would otherwise have had
/// something to return.
#[test]
#[ignore = "requires built binary, BGE-small model download, free TCP port"]
fn an_exclusion_only_query_is_refused_over_mcp() {
    let layout = TempKbLayout::new("groove-exclusion-only-it");
    build_test_kb(&layout);
    build_index(layout.kb());

    let cfg_path = layout.root().join("groove.toml");
    std::fs::write(&cfg_path, "[watch]\nenabled = false\n").unwrap();

    let (_guard, base) = spawn_mcp_server(layout.kb(), &cfg_path);
    let session = mcp_initialize(&base);

    let resp = mcp_search_call(&base, &session, serde_json::json!({"query": "-rayon"}));
    let message = resp["error"]
        .as_str()
        .unwrap_or_else(|| panic!("the refusal must use the `error` envelope: {resp}"));
    assert!(
        message.contains("query has no positive term"),
        "the refusal must say what is missing: {message}"
    );
    assert!(
        resp.get("results").is_none(),
        "the envelope replaces the wrapper rather than joining it: {resp}"
    );
}
