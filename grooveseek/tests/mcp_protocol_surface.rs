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
//! Everything here runs without building an index, so it stays off
//! `#[ignore]` — with one exception at the end. `resources/list` is built from
//! what is indexed and `resources/read` refuses what is not, so the half of the
//! resource surface that has documents behind it needs a real index, and that
//! one test is `#[ignore]`d for the model load it implies.

mod common;
use common::mcp::spawn_mcp_server;
use common::temp::TempKbLayout;

use std::process::Command;

/// The revision groove negotiates. Requests at this revision carry three
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
    let layout = TempKbLayout::new("groove-protocol-surface");
    layout.write(
        "note.md",
        "---\ntitle: Note\n---\n\n## body\n\nOne document so the knowledge base is not empty.\n",
    );
    let cfg = layout.root().join("groove.toml");
    std::fs::write(&cfg, "[watch]\nenabled = false\n").expect("write groove.toml");
    let (guard, base) = spawn_mcp_server(layout.kb(), &cfg);
    (layout, guard, base)
}

/// The one every client reads. Both halves have been wrong: the name and
/// version came from rmcp's build environment rather than this crate's.
#[test]
fn the_server_identifies_itself_as_grooveseek() {
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
        info["name"], "grooveseek",
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
/// round-trips that return nothing. That is why this list grew one entry at a
/// time — `prompts`, then `resources` — each in the commit that gave it
/// content, and why `completions` and `logging` are still absent.
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

/// (feature-50 PR 3) The resource surface's shape, without needing an index.
///
/// `resources/list` is empty here — nothing has been indexed — and that is
/// still worth asserting: the result must be well-formed and carry the caching
/// hints, which is the part a hand-written handler has to supply itself.
#[test]
fn the_resource_lists_are_well_formed_and_carry_the_caching_hints() {
    let (_kb, _guard, base) = start();

    for method in ["resources/list", "resources/templates/list"] {
        let resp = rpc(&base, method, serde_json::json!({"_meta": meta()}));
        let result = &resp["result"];
        assert_eq!(result["resultType"], "complete", "{method}: {resp}");
        assert!(
            result.get("ttlMs").is_some() && result.get("cacheScope").is_some(),
            "{method} must carry caching hints on a complete result: {resp}"
        );
    }

    let templates = rpc(
        &base,
        "resources/templates/list",
        serde_json::json!({"_meta": meta()}),
    );
    let list = templates["result"]["resourceTemplates"]
        .as_array()
        .unwrap_or_else(|| panic!("no template array: {templates}"));
    assert_eq!(list.len(), 1, "{templates}");
    assert_eq!(
        list[0]["uriTemplate"], "kb://doc/{path}",
        "the per-document template is how a client reaches what the listing \
         deliberately does not enumerate: {templates}"
    );
    assert!(
        list[0].get("mimeType").is_none(),
        "the template covers documents of several media types, so it must not \
         claim one: {templates}"
    );
}

/// A URI that never came from this server is refused, however it is spelled.
///
/// The traversal check runs after percent-decoding, so the encoded form has to
/// be refused too — a check that ran first would not see `%2e%2e%2f` as `../`.
#[test]
fn resource_reads_refuse_anything_that_was_not_offered() {
    let (_kb, _guard, base) = start();

    for uri in [
        "kb://doc/../secret.md",
        "kb://doc/%2e%2e%2fsecret.md",
        "kb://doc/note.md", // real file, but nothing is indexed here
        "file:///etc/passwd",
        "kb://nonsense/x",
    ] {
        let resp = rpc_named(
            &base,
            "resources/read",
            Some(uri),
            serde_json::json!({"uri": uri, "_meta": meta()}),
        );
        assert!(
            resp["error"].is_object(),
            "must be refused: {uri} -> {resp}"
        );
    }
}

/// The transport requirement, pinned for the same reason as `prompts/get`:
/// nothing in the Rust API mentions it, so the first test written without it
/// fails in a way that looks like a broken handler.
#[test]
fn resources_read_is_rejected_without_the_mcp_name_header() {
    let (_kb, _guard, base) = start();

    let resp = rpc(
        &base,
        "resources/read",
        serde_json::json!({"uri": "kb://doc/note.md", "_meta": meta()}),
    );
    assert_eq!(
        resp["error"]["code"], -32020,
        "expected the transport to reject a nameless resources/read: {resp}"
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

/// (feature-50 PR 3) The resource surface with an index behind it — the half
/// the tests above cannot reach, because `resources/list` is built from what is
/// indexed and `resources/read` refuses anything that is not.
///
/// `#[ignore]` because it builds an index, which loads the embedding model.
#[test]
#[ignore = "builds an index; downloads BGE-small on first run"]
fn resources_list_and_read_agree_with_the_index() {
    let layout = TempKbLayout::new("groove-resources-e2e");
    let body = |marker: &str| {
        format!(
            "---\ntitle: Resource fixture\n---\n\n## body\n\nContent carrying {marker}, long \
             enough to be a real chunk rather than something the quality filter hides.\n"
        )
    };
    layout.write("root-note.md", &body("zqxwroot"));
    layout.write("deep-dive/mcp/overview.md", &body("zqxwmcp"));
    layout.write("ai-news/today.md", &body("zqxwnews"));
    common::mcp::build_index(layout.kb());

    let cfg = layout.root().join("groove.toml");
    std::fs::write(&cfg, "[watch]\nenabled = false\n").expect("write groove.toml");
    let (_guard, base) = spawn_mcp_server(layout.kb(), &cfg);

    // Groups, not documents: three files fall into three groups here, but the
    // rule is the first one or two path segments, not one entry per file.
    let listed = rpc(
        &base,
        "resources/list",
        serde_json::json!({"_meta": meta()}),
    );
    let mut uris: Vec<String> = listed["result"]["resources"]
        .as_array()
        .unwrap_or_else(|| panic!("no resources: {listed}"))
        .iter()
        .map(|r| r["uri"].as_str().unwrap_or_default().to_string())
        .collect();
    uris.sort();
    assert_eq!(
        uris,
        vec![
            "kb://topic/",
            "kb://topic/ai-news",
            "kb://topic/deep-dive/mcp"
        ],
        "listing must group by path prefix: {listed}"
    );

    // Everything offered must be readable — that is what makes a listing a
    // promise rather than a guess.
    for uri in &uris {
        let resp = rpc_named(
            &base,
            "resources/read",
            Some(uri),
            serde_json::json!({"uri": uri, "_meta": meta()}),
        );
        assert!(
            resp["result"]["contents"][0]["text"].is_string(),
            "a listed resource must be readable: {uri} -> {resp}"
        );
        // A read is a complete result like the two listings, and owes the same
        // hints. rmcp's constructor leaves them unset, so only the handlers
        // that add them conform — and this one is hand-written.
        assert!(
            resp["result"].get("ttlMs").is_some() && resp["result"].get("cacheScope").is_some(),
            "resources/read must carry caching hints on a complete result: {resp}"
        );
    }

    // A document, by the template's URI, with the media type of what is served.
    let doc = "kb://doc/deep-dive/mcp/overview.md";
    let resp = rpc_named(
        &base,
        "resources/read",
        Some(doc),
        serde_json::json!({"uri": doc, "_meta": meta()}),
    );
    let content = &resp["result"]["contents"][0];
    assert_eq!(content["mimeType"], "text/markdown", "{resp}");
    assert!(
        content["text"]
            .as_str()
            .unwrap_or_default()
            .contains("zqxwmcp"),
        "the document's own text must come back, not an envelope: {resp}"
    );

    // A file that exists on disk but is not in the index was never offered.
    layout.write("unindexed.md", &body("zqxwunindexed"));
    let sneaky = "kb://doc/unindexed.md";
    let refused = rpc_named(
        &base,
        "resources/read",
        Some(sneaky),
        serde_json::json!({"uri": sneaky, "_meta": meta()}),
    );
    assert!(
        refused["error"].is_object(),
        "membership in the index is the gate, not presence on disk: {refused}"
    );

    // And `search` hands back the URI for every hit, so a document nobody
    // enumerated is still addressable.
    let session = common::mcp::mcp_initialize(&base);
    let hits = common::mcp::mcp_search_call(
        &base,
        &session,
        serde_json::json!({"query": "zqxwmcp", "limit": 3, "mmr": false}),
    );
    let first = &hits["results"][0];
    assert_eq!(
        first["uri"], "kb://doc/deep-dive/mcp/overview.md",
        "every search hit must carry the resource URI for its document: {hits}"
    );
}

/// The tool descriptions a client reads, against the behaviour the server has.
///
/// A client never opens `docs/mcp-tools.md`. The `description=` string is the
/// whole of what it is told, and both of these had drifted from the page:
/// `rebuild_index` enforced a one-at-a-time bound it did not mention, and
/// `get_connection_graph` returned two fields it did not name.
///
/// Worth saying what substring assertions cannot do. This catches a fact being
/// deleted from a description. It does not catch a description disagreeing with
/// the page, because nothing here reads the page — that is a different guard,
/// and it does not exist.
///
/// The full-phrase assertions are load-bearing for a second reason. Those
/// literals are joined with `\` continuations, which eat the newline *and the
/// leading whitespace* of the next line, so one missing trailing space welds
/// two words together and nothing else in the tree would notice.
#[test]
fn the_tool_descriptions_name_the_behaviour_the_server_has() {
    let (_kb, _guard, base) = start();

    let listed = rpc(&base, "tools/list", serde_json::json!({"_meta": meta()}));
    let tools = listed["result"]["tools"]
        .as_array()
        .unwrap_or_else(|| panic!("tools/list must return an array: {listed}"));

    let describe = |name: &str| -> String {
        let tool = tools
            .iter()
            .find(|t| t["name"] == name)
            .unwrap_or_else(|| panic!("{name} must be advertised: {listed}"));
        tool["description"]
            .as_str()
            .unwrap_or_else(|| panic!("{name} must carry a description: {listed}"))
            .to_string()
    };

    // `claim_rebuild_slot` refuses a call that arrives during a rebuild, and
    // `rebuild_already_running` names the elapsed time in the error.
    let rebuild = describe("rebuild_index");
    for wanted in [
        "all source files in the knowledge base",
        "One at a time",
        "refused",
        "how long the running one has been going",
    ] {
        assert!(
            rebuild.contains(wanted),
            "rebuild_index refuses a concurrent call and its description must \
             say so: {wanted:?} missing from {rebuild:?}"
        );
    }

    // Fields `GraphNode` and the response envelope actually carry.
    let graph = describe("get_connection_graph");
    for wanted in [
        "parent_id / depth / score / snippet",
        "truncated",
        "truncation[]",
    ] {
        assert!(
            graph.contains(wanted),
            "get_connection_graph returns {wanted:?} and must name it: {graph:?}"
        );
    }

    for (name, text) in [
        ("rebuild_index", &rebuild),
        ("get_connection_graph", &graph),
    ] {
        assert!(
            !text.contains("  ") && !text.contains('\n'),
            "{name}'s description must read as one paragraph, so a continuation \
             that kept its indentation is a defect: {text:?}"
        );
    }
}
