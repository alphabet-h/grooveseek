//! (feature-50) What the server tells a client about itself.
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
    rpc_named(base, method, None, params)
}

/// As [`rpc`], with the `Mcp-Name` header some methods require.
///
/// Measured: the Streamable HTTP transport rejects `prompts/get` and
/// `resources/read` outright with `-32020 missing required Mcp-Name header`
/// before the handler runs, while `tools/list`, `prompts/list` and
/// `resources/list` do not need it. The pattern is that operations naming a
/// single primitive must name it in a header too — and the rejection arrives as
/// a plain JSON body rather than SSE, so a reader that only strips `data: `
/// turns it into silence.
fn rpc_named(
    base: &str,
    method: &str,
    name: Option<&str>,
    params: serde_json::Value,
) -> serde_json::Value {
    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": method,
        "params": params,
    })
    .to_string();

    let mut args: Vec<String> = vec![
        "-s".into(),
        "-X".into(),
        "POST".into(),
        format!("{base}/mcp"),
        "-H".into(),
        "Content-Type: application/json".into(),
        "-H".into(),
        "Accept: application/json, text/event-stream".into(),
        "-H".into(),
        format!("MCP-Protocol-Version: {PROTOCOL_VERSION}"),
        "-H".into(),
        format!("Mcp-Method: {method}"),
    ];
    if let Some(name) = name {
        args.push("-H".into());
        args.push(format!("Mcp-Name: {name}"));
    }
    args.push("-d".into());
    args.push(body.clone());

    let out = Command::new("curl")
        .args(&args)
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
/// Each entry here is a promise with something behind it. `prompts/list` and
/// `resources/list` answer with empty arrays whether or not anything is
/// declared, so declaring a capability with nothing behind it only invites
/// round-trips that return nothing — which is why `resources` is still absent
/// while `prompts` has joined `tools`. Changing this list is how a capability
/// gets added: on purpose, in the commit that gives it content.
#[test]
fn the_advertised_capabilities_are_exactly_what_is_implemented() {
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
    for present in ["tools", "prompts", "resources"] {
        assert!(
            from_initialize.get(present).is_some(),
            "{present} is implemented and must be advertised: {init}"
        );
    }
    for absent in ["completions", "logging"] {
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

/// (feature-50 PR 2) The prompt list as a client sees it — the names it renders
/// as commands, and the caching hints the spec requires.
///
/// Measured rather than assumed: the `#[prompt_handler]` macro does set `ttlMs`
/// and `cacheScope`, the way the tool macro does and the trait defaults do not.
/// Asserting it here is what keeps that true if the macro changes.
#[test]
fn the_prompt_list_is_the_four_prompts_with_the_caching_hints_the_spec_requires() {
    let (_kb, _guard, base) = start();

    let resp = rpc(&base, "prompts/list", serde_json::json!({"_meta": meta()}));
    let result = &resp["result"];

    let mut names: Vec<String> = result["prompts"]
        .as_array()
        .unwrap_or_else(|| panic!("no prompts array: {resp}"))
        .iter()
        .map(|p| p["name"].as_str().unwrap_or_default().to_string())
        .collect();
    names.sort();
    assert_eq!(
        names,
        vec!["deep_dive", "find_gaps", "summarize_topic", "whats_new"],
        "the prompt surface changed; if that was deliberate, update this list"
    );

    assert_eq!(result["resultType"], "complete", "{resp}");
    assert!(
        result.get("ttlMs").is_some() && result.get("cacheScope").is_some(),
        "the spec requires caching hints on a complete result: {resp}"
    );
}

/// A prompt has to come back with a message when asked for, and say something
/// useful when asked for one that does not exist.
#[test]
fn a_prompt_returns_a_user_message_and_an_unknown_one_names_the_alternatives() {
    let (_kb, _guard, base) = start();

    let resp = rpc_named(
        &base,
        "prompts/get",
        Some("summarize_topic"),
        serde_json::json!({
            "name": "summarize_topic",
            "arguments": {"topic": "zqxw-distinct-topic"},
            "_meta": meta(),
        }),
    );
    let messages = resp["result"]["messages"]
        .as_array()
        .unwrap_or_else(|| panic!("no messages: {resp}"));
    assert_eq!(messages.len(), 1, "{resp}");
    assert_eq!(messages[0]["role"], "user", "{resp}");
    let text = messages[0]["content"]["text"]
        .as_str()
        .unwrap_or_else(|| panic!("no text content: {resp}"));
    assert!(
        text.contains("zqxw-distinct-topic"),
        "the argument never reached the message: {text}"
    );

    let missing = rpc_named(
        &base,
        "prompts/get",
        Some("no_such_prompt"),
        serde_json::json!({"name": "no_such_prompt", "_meta": meta()}),
    );
    assert!(
        missing["error"].is_object(),
        "an unknown prompt must be an error, not an empty success: {missing}"
    );
    let detail = missing["error"].to_string();
    assert!(
        detail.contains("summarize_topic"),
        "the error should tell the caller what does exist: {detail}"
    );
}

/// The header requirement itself, pinned because it is transport-level and
/// invisible from the Rust API: nothing in `ServerHandler` mentions it, so the
/// first integration test written without it fails in a way that looks like a
/// broken handler.
#[test]
fn prompts_get_is_rejected_without_the_mcp_name_header() {
    let (_kb, _guard, base) = start();

    let resp = rpc(
        &base,
        "prompts/get",
        serde_json::json!({
            "name": "summarize_topic",
            "arguments": {"topic": "mcp"},
            "_meta": meta(),
        }),
    );
    assert_eq!(
        resp["error"]["code"], -32020,
        "expected the transport to reject a nameless prompts/get: {resp}"
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
