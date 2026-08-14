//! (feature-50 PR 1) What the server tells a client about itself.
//!
//! Before this file there was no test anywhere in the repo asserting the
//! advertised capability set, the tool list as it appears on the wire, or the
//! server's own name and version — the three things every MCP client reads
//! first. That gap is why `initialize` could answer
//! `serverInfo {"name":"rmcp","version":"3.1.2"}` through fifteen releases
//! without anything failing: the whole `ServerHandler` impl was macro-generated
//! and nothing looked at what it generated.
//!
//! Assertions are made against **both** discovery surfaces. `initialize` is the
//! older one; protocol revision 2026-07-28 introduced `server/discover`, and a
//! test that only reads `initialize` is testing the dialect the spec moved on
//! from — the shape of trap `.dev/knowledge/rmcp-major-upgrade-1-to-3.md`
//! records for the rmcp 1 → 3 upgrade, where "compiles and every test passes"
//! was the weakest possible evidence because what changed was the protocol.
//!
//! No embedding model is downloaded (`serve` loads the default one from cache
//! and never indexes here), so this stays off `#[ignore]`.

mod common;
use common::mcp::spawn_mcp_server;
use common::temp::TempKbLayout;

use std::process::Command;

/// The revision kb-mcp negotiates. Requests at this revision carry three
/// headers **and** a `params._meta`; either one missing is rejected before the
/// handler runs (`-32020` / `-32602`).
const PROTOCOL_VERSION: &str = "2026-07-28";

/// Post one JSON-RPC request and return the parsed body.
///
/// rmcp answers some requests as SSE (`data: {...}`) and others — errors, and
/// anything the HTTP layer rejects before dispatch — as a plain JSON body.
/// A reader that only strips `data: ` silently discards the second kind, which
/// is exactly how `resources/read` looked like it returned nothing while it was
/// really answering. Accept both framings.
fn rpc(base: &str, method: &str, params: serde_json::Value) -> serde_json::Value {
    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": method,
        "params": params,
    })
    .to_string();

    let out = Command::new("curl")
        .args([
            "-s",
            "-X",
            "POST",
            &format!("{base}/mcp"),
            "-H",
            "Content-Type: application/json",
            "-H",
            "Accept: application/json, text/event-stream",
            "-H",
            &format!("MCP-Protocol-Version: {PROTOCOL_VERSION}"),
            "-H",
            &format!("Mcp-Method: {method}"),
            "-d",
            &body,
        ])
        .output()
        .expect("curl spawn");
    let text = String::from_utf8_lossy(&out.stdout);

    for line in text.lines() {
        let candidate = line.strip_prefix("data: ").unwrap_or(line);
        let trimmed = candidate.trim();
        if trimmed.starts_with('{')
            && let Ok(v) = serde_json::from_str::<serde_json::Value>(trimmed)
        {
            return v;
        }
    }
    panic!("no JSON object in the response to {method}: {text}");
}

fn meta() -> serde_json::Value {
    serde_json::json!({
        "io.modelcontextprotocol/protocolVersion": PROTOCOL_VERSION,
        "io.modelcontextprotocol/clientCapabilities": {},
    })
}

/// The layout is returned, not dropped: it owns the temp directory the server
/// is still reading from, so the caller has to hold it for the length of the
/// test. Leaking it instead would leave a directory behind on every run.
///
/// **The layout comes first in the tuple, and that ordering is load-bearing.**
/// Destructuring `let (a, b, c) = …` declares three locals left to right, and
/// locals drop in reverse declaration order — so whatever is last in the tuple
/// is destroyed first. The layout's `Drop` calls `remove_dir_all` on a
/// directory the server still has a SQLite database open under; if it runs
/// before the guard kills the child, the removal fails, the error is discarded,
/// and the test leaves a temp directory behind on every Windows run. Returning
/// the layout first makes it drop last (codex P2, round 1 on PR #160).
fn start() -> (TempKbLayout, common::mcp::ServerGuard, String) {
    let layout = TempKbLayout::new("kb-mcp-protocol-surface");
    layout.write(
        "note.md",
        "---\ntitle: Note\n---\n\n## body\n\nOne document so the knowledge base is not empty.\n",
    );
    let cfg = layout.root().join("kb-mcp.toml");
    std::fs::write(&cfg, "[watch]\nenabled = false\n").expect("write kb-mcp.toml");
    let (guard, base) = spawn_mcp_server(layout.kb(), &cfg);
    (layout, guard, base)
}

/// The one every client reads. Both halves have been wrong: the name and
/// version came from rmcp's build environment rather than this crate's.
#[test]
fn the_server_identifies_itself_as_kb_mcp() {
    let (_kb, _guard, base) = start();

    let init = rpc(
        &base,
        "initialize",
        serde_json::json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": {},
            "clientInfo": {"name": "protocol-surface", "version": "0"},
            "_meta": meta(),
        }),
    );
    let info = &init["result"]["serverInfo"];
    assert_eq!(
        info["name"], "kb-mcp",
        "the server must name itself, not the SDK it is built on: {init}"
    );
    assert_eq!(
        info["version"],
        env!("CARGO_PKG_VERSION"),
        "the advertised version must be this crate's: {init}"
    );
}

/// The capability set, on both discovery surfaces.
///
/// Pinned at "tools only" deliberately: `prompts/list` and `resources/list`
/// already answer with empty arrays whether or not anything is declared, so
/// declaring a capability with nothing behind it only invites round-trips that
/// return nothing. When B-2 / B-3 land, this assertion changes with them — and
/// the point is that it has to be changed on purpose.
#[test]
fn the_advertised_capabilities_are_tools_only_on_both_discovery_surfaces() {
    let (_kb, _guard, base) = start();

    let init = rpc(
        &base,
        "initialize",
        serde_json::json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": {},
            "clientInfo": {"name": "protocol-surface", "version": "0"},
            "_meta": meta(),
        }),
    );
    let from_initialize = &init["result"]["capabilities"];
    assert!(
        from_initialize.get("tools").is_some(),
        "tools must be advertised: {init}"
    );
    for absent in ["prompts", "resources", "completions", "logging"] {
        assert!(
            from_initialize.get(absent).is_none(),
            "{absent} is not implemented and must not be advertised: {init}"
        );
    }

    let discover = rpc(
        &base,
        "server/discover",
        serde_json::json!({"_meta": meta()}),
    );
    let from_discover = &discover["result"]["capabilities"];
    assert_eq!(
        from_discover, from_initialize,
        "the two discovery surfaces must advertise the same capabilities; \
         asserting only against `initialize` tests the dialect 2026-07-28 \
         moved on from: discover={discover}"
    );
}

/// The tool list as a client sees it, including the caching hints the spec
/// requires on any result carrying `resultType: "complete"`.
#[test]
fn the_tool_list_is_six_tools_with_the_caching_hints_the_spec_requires() {
    let (_kb, _guard, base) = start();

    let resp = rpc(&base, "tools/list", serde_json::json!({"_meta": meta()}));
    let result = &resp["result"];

    let mut names: Vec<String> = result["tools"]
        .as_array()
        .unwrap_or_else(|| panic!("no tools array: {resp}"))
        .iter()
        .map(|t| t["name"].as_str().unwrap_or_default().to_string())
        .collect();
    names.sort();
    assert_eq!(
        names,
        vec![
            "get_best_practice",
            "get_connection_graph",
            "get_document",
            "list_topics",
            "rebuild_index",
            "search",
        ],
        "the tool surface changed; if that was deliberate, update this list"
    );

    assert_eq!(result["resultType"], "complete", "{resp}");
    assert!(
        result.get("ttlMs").is_some(),
        "the spec requires a caching hint on a complete result: {resp}"
    );
    assert!(
        result.get("cacheScope").is_some(),
        "the spec requires a cache scope on a complete result: {resp}"
    );
}
