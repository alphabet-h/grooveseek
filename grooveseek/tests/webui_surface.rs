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

use serde_json::{Map, Value};
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

const PAGE: &str = include_str!("../src/transport/webui_index.html");

/// The version string the page pins, read out of the page.
///
/// **What that does and does not catch, measured.** Setting the page to
/// `1999-01-01` fails the live test below, so a version rmcp does not know is
/// caught. Setting it to `2025-11-25` — rmcp's `LATEST`, and the wrong choice
/// here — **passes**: rmcp accepts a handshake-free call on known older
/// versions too. So this half only proves the page names something the server
/// will take. Which version it must name is pinned in
/// `src/transport/http.rs`, against `STANDARD_HEADERS`, and that assertion is
/// the one that catches the plausible mistake.
fn version_the_page_pins() -> &'static str {
    literal_after(PAGE, "const MCP_VERSION = \"")
}

/// The text between `marker` and the next `"`.
fn literal_after<'a>(haystack: &'a str, marker: &str) -> &'a str {
    haystack
        .split_once(marker)
        .unwrap_or_else(|| panic!("webui_index.html no longer contains {marker:?}"))
        .1
        .split_once('"')
        .expect("the literal is closed")
        .0
}

/// The balanced `{ ... }` that follows `marker`, braces included.
fn braced_after(marker: &str) -> &'static str {
    let rest = PAGE
        .split_once(marker)
        .unwrap_or_else(|| panic!("webui_index.html no longer contains {marker:?}"))
        .1;
    let start = rest.find('{').expect("an object follows the marker");
    let bytes = rest.as_bytes();
    let (mut depth, mut i, mut in_string) = (0usize, start, false);
    while i < bytes.len() {
        match bytes[i] {
            b'\\' if in_string => i += 1,
            b'"' => in_string = !in_string,
            b'{' if !in_string => depth += 1,
            b'}' if !in_string => {
                depth -= 1;
                if depth == 0 {
                    return &rest[start..=i];
                }
            }
            _ => {}
        }
        i += 1;
    }
    panic!("the object opened after {marker:?} is never closed");
}

/// One JavaScript value from the page, as it will appear on the wire.
///
/// Only the expressions the page actually uses are known. **An unfamiliar one
/// panics rather than being skipped**: quietly ignoring it is how a request
/// built here stops being the request the page sends.
fn resolve(raw: &str, tool: &str) -> Value {
    match raw {
        "MCP_VERSION" => Value::String(version_the_page_pins().to_string()),
        // `callTool(name, args)`; these tests call it with no arguments.
        "name" => Value::String(tool.to_string()),
        "args" => Value::Object(Map::new()),
        "{}" => Value::Object(Map::new()),
        lit if lit.starts_with('"') && lit.ends_with('"') && lit.len() >= 2 => {
            Value::String(lit[1..lit.len() - 1].to_string())
        }
        num if num.parse::<i64>().is_ok() => Value::from(num.parse::<i64>().unwrap()),
        other => panic!(
            "webui_index.html now sends {other:?}, which this test cannot resolve. \
             Teach `resolve` what it means — do not drop it, or the request built \
             here stops being the request the page sends."
        ),
    }
}

/// Read one of the page's object literals into a JSON value.
///
/// Deliberately small, and deliberately strict. The page is ours and formats
/// these literals one key per line, so a line walk is enough; anything it does
/// not recognise **panics**, because a shape this cannot read must never be
/// reported as a shape that matches.
fn object_from_page(marker: &str, tool: &str) -> Value {
    let mut stack: Vec<(Option<String>, Map<String, Value>)> = Vec::new();
    for raw in braced_after(marker).lines() {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        assert!(
            !line.starts_with("//"),
            "a comment inside {marker:?} — teach the reader about it rather than \
             skipping the line: {line:?}"
        );
        if line == "{" {
            stack.push((None, Map::new()));
            continue;
        }
        if line == "}" || line == "}," || line == "})," || line == "})" {
            let (key, map) = stack.pop().expect("a close with nothing open");
            let done = Value::Object(map);
            match (key, stack.last_mut()) {
                (Some(k), Some((_, parent))) => {
                    parent.insert(k, done);
                }
                (None, None) => return done,
                _ => panic!("unbalanced object literal in {marker:?}"),
            }
            continue;
        }
        let (k, v) = line
            .split_once(':')
            .unwrap_or_else(|| panic!("cannot read {line:?} as `key: value` in {marker:?}"));
        let key = k.trim().trim_matches('"').to_string();
        let v = v.trim().trim_end_matches(',').trim();
        if v == "{" {
            stack.push((Some(key), Map::new()));
        } else {
            let (_, top) = stack.last_mut().expect("a value outside any object");
            top.insert(key, resolve(v, tool));
        }
    }
    panic!("the object literal in {marker:?} did not close");
}

/// The `tools/call` the page sends, **read out of the page** — headers and body
/// alike, envelope included. There is no second copy of the request here to
/// drift from the first: rename `params`, move `_meta`, change a header, and
/// what this sends changes with it. An expression the reader cannot resolve
/// stops the test rather than being skipped.
///
/// Executing the page's JavaScript would be stricter still, but that means a
/// JavaScript runtime as a dependency for this one call.
///
/// `include_protocol_header` exists so a test can drop one header and see the
/// difference — without that, "the server accepted it" is equally consistent
/// with a server that reads no headers at all.
fn page_shaped_call(base: &str, tool: &str, include_protocol_header: bool) -> String {
    let body = serde_json::to_string(&object_from_page("body: JSON.stringify(", tool))
        .expect("the page's body literal serialises");

    let mut args: Vec<String> = vec!["-s".into(), "-X".into(), "POST".into()];
    for (name, value) in object_from_page("headers:", tool)
        .as_object()
        .expect("the headers literal is an object")
    {
        if !include_protocol_header && name.eq_ignore_ascii_case("MCP-Protocol-Version") {
            continue;
        }
        let value = value
            .as_str()
            .unwrap_or_else(|| panic!("header {name} is not a string: {value}"));
        args.push("-H".into());
        args.push(format!("{name}: {value}"));
    }
    args.extend(["-d".into(), body, format!("{base}/mcp")]);
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

/// The envelope, checked on the **parsed** request rather than as fragments of
/// the page's text. Substring checks were the previous version of this and
/// missed the case that matters: rename `params`, or move `_meta` out of it,
/// and every fragment is still present somewhere in the file while the request
/// the page sends has changed shape.
#[test]
fn the_request_read_from_the_page_has_the_envelope_the_protocol_needs() {
    let req = object_from_page("body: JSON.stringify(", "list_topics");

    assert_eq!(req["jsonrpc"], "2.0", "not a JSON-RPC request: {req}");
    assert_eq!(req["method"], "tools/call", "not a tools/call: {req}");
    assert!(req["id"].is_number(), "no request id: {req}");

    let params = req
        .get("params")
        .unwrap_or_else(|| panic!("the arguments are no longer under `params`: {req}"));
    assert_eq!(params["name"], "list_topics", "the tool name moved: {req}");
    assert!(
        params.get("_meta").is_some_and(Value::is_object),
        "`_meta` is no longer an object inside `params`, which is where the \
         stateless protocol reads it: {req}"
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
