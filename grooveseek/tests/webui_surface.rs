//! `/ui` and the request it makes, exercised through a running server.
//!
//! `tests/webui_integration.rs` covers this area with `#[cfg(feature =
//! "test-helpers")]` and `#[ignore]`, so PR CI compiles it and nightly runs it.
//! That is the right shape for what lives there — those tests build an index,
//! which downloads a model. It is the wrong shape for the parts that do not,
//! and the parts that do not are where `/ui` actually broke: the page began
//! searching through `/mcp` in #174, and nothing has asserted since that the
//! request it sends is one the server accepts.
//!
//! So these are neither ignored nor gated, for the same reason
//! `tests/http_origin.rs` is neither. Nothing here builds an index.
//!
//! They also go through a real server rather than `build_router_for_test`.
//! That router is not the shipped one — it carries `/api/search`, whose handler
//! is `#[cfg(any(test, feature = "test-helpers"))]` because
//! `tests/runtime_starvation.rs` needs a route that blocks on the embedder
//! lock. The divergence is deliberate and documented; what was missing is
//! anything asserting it, which the last test here supplies.

mod common;
use common::mcp::spawn_mcp_server_ephemeral;
use common::temp::TempKbLayout;

use std::process::Command;

/// Only `[watch]` — the Origin allow-list stays at its default.
const PLAIN: &str = "[watch]\nenabled = false\n";

/// The layout is returned first so it drops **last**: locals drop in reverse
/// declaration order, and its `Drop` removes a directory the server still has
/// a database open under.
fn start(prefix: &str) -> (TempKbLayout, common::mcp::ServerGuard, String) {
    let layout = TempKbLayout::new(prefix);
    layout.write(
        "note.md",
        "---\ntitle: Note\n---\n\n## body\n\nOne document so the knowledge base is not empty.\n",
    );
    let cfg = layout.root().join("groove.toml");
    std::fs::write(&cfg, PLAIN).expect("write groove.toml");
    let (guard, base) = spawn_mcp_server_ephemeral(layout.kb(), &cfg);
    (layout, guard, base)
}

/// `curl`, as everywhere else in `tests/` — `reqwest` is deliberately not a
/// dev-dependency of this crate.
fn curl(args: &[&str]) -> String {
    let out = Command::new("curl")
        .args(args)
        .output()
        .expect("curl spawn");
    String::from_utf8_lossy(&out.stdout).to_string()
}

/// The version string the page actually pins, read out of the page.
///
/// Written out rather than hard-coded here: a copy would let the page be
/// changed to a version the server refuses while this file kept sending the
/// one that works, and the test would pass for a `/ui` that cannot search.
/// `src/transport/http.rs` separately pins the page against rmcp's
/// `STANDARD_HEADERS`; this asserts the softer, more important half — whatever
/// the page pins is accepted by the server it talks to.
fn version_the_page_pins() -> &'static str {
    const PAGE: &str = include_str!("../src/transport/webui_index.html");
    const MARKER: &str = "const MCP_VERSION = \"";
    let after = PAGE
        .split_once(MARKER)
        .expect("webui_index.html declares MCP_VERSION")
        .1;
    after
        .split_once('"')
        .expect("the MCP_VERSION literal is closed")
        .0
}

/// The `tools/call` the page sends: no handshake, three headers plus the
/// `_meta` block. `include_protocol_header` exists so a test can drop one and
/// see the difference — without that, "the server accepted it" is equally
/// consistent with a server that reads no headers at all.
fn page_shaped_call(base: &str, tool: &str, include_protocol_header: bool) -> String {
    let url = format!("{base}/mcp");
    let version = version_the_page_pins();
    let body = format!(
        r#"{{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{{"name":"{tool}","arguments":{{}},"_meta":{{"io.modelcontextprotocol/protocolVersion":"{version}","io.modelcontextprotocol/clientCapabilities":{{}}}}}}}}"#
    );
    let mut args: Vec<String> = vec![
        "-s".into(),
        "-X".into(),
        "POST".into(),
        "-H".into(),
        "Content-Type: application/json".into(),
        "-H".into(),
        "Accept: application/json, text/event-stream".into(),
        "-H".into(),
        "Mcp-Method: tools/call".into(),
        "-H".into(),
        format!("Mcp-Name: {tool}"),
    ];
    if include_protocol_header {
        args.push("-H".into());
        args.push(format!("MCP-Protocol-Version: {version}"));
    }
    args.extend(["-d".into(), body, url]);
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    curl(&refs)
}

#[test]
fn the_page_is_served() {
    let (_kb, _guard, base) = start("groove-ui-served");

    let body = curl(&["-s", &format!("{base}/ui")]);

    assert!(
        body.contains("const MCP_VERSION"),
        "/ui did not return the page; got {} bytes starting {:?}",
        body.len(),
        body.chars().take(80).collect::<String>()
    );
}

/// The regression this file exists for. `/ui` searches through `/mcp` now, and
/// the request it sends carries no handshake — it relies on rmcp accepting a
/// stateless call described entirely by headers and `_meta`. Nothing asserted
/// that the server on the other end agrees.
///
/// `list_topics` is the tool used because it takes no arguments and reads only
/// the database, so no model is downloaded.
#[test]
fn the_request_shape_the_page_uses_is_accepted() {
    let (_kb, _guard, base) = start("groove-ui-shape-ok");

    let body = page_shaped_call(&base, "list_topics", true);

    assert!(
        !body.contains("-32020") && !body.contains("-32602"),
        "the server refused the exact request /ui sends: {body}"
    );
    assert!(
        body.contains("\"result\""),
        "expected a result for list_topics, got {body}"
    );
}

/// The other half, and the one that gives the test above its meaning: drop the
/// protocol header and the same request is refused. Without this, "accepted"
/// would be indistinguishable from a server that never looks at the headers,
/// and the page could pin any version at all and still pass.
#[test]
fn dropping_the_protocol_header_is_refused() {
    let (_kb, _guard, base) = start("groove-ui-shape-bad");

    let body = page_shaped_call(&base, "list_topics", false);

    assert!(
        !body.contains("\"result\""),
        "a handshake-free call without the protocol header succeeded, so the \
         header the page pins is not load-bearing: {body}"
    );
}

/// `/api/search` is registered by `build_router_for_test` and not by the
/// server. That is deliberate — `tests/runtime_starvation.rs` needs a route
/// whose body blocks on the embedder lock — but it means the in-process router
/// is not the shipped one, and nothing said so. A 404 here is what stops the
/// test router from being read as evidence about production.
#[test]
fn the_production_build_does_not_carry_the_test_only_search_route() {
    let (_kb, _guard, base) = start("groove-ui-no-api-search");

    let code = curl(&[
        "-s",
        "-o",
        "/dev/null",
        "-w",
        "%{http_code}",
        "-X",
        "POST",
        "-H",
        "content-type: application/json",
        "-d",
        r#"{"query":"x"}"#,
        &format!("{base}/api/search"),
    ]);

    assert_eq!(
        code.trim(),
        "404",
        "/api/search answered {code} in a shipped server; it exists only behind \
         the test gate, and a route that ships is a route that needs a threat model"
    );
}

/// The admin surface, including the page itself, is loopback-only and also
/// validates the `Host` header. The peer here is always loopback, so this
/// exercises the second half.
#[test]
fn the_page_refuses_a_foreign_host() {
    let (_kb, _guard, base) = start("groove-ui-foreign-host");

    let code = curl(&[
        "-s",
        "-o",
        "/dev/null",
        "-w",
        "%{http_code}",
        "-H",
        "Host: evil.example",
        &format!("{base}/ui"),
    ]);

    assert_eq!(
        code.trim(),
        "403",
        "/ui answered {code} to a request naming another host"
    );
}
