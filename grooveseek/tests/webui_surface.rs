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

/// The `{ ... }` object literal opened by `marker`, as `(key, raw value)` pairs.
///
/// Deliberately small: the page is ours, its two relevant literals are one key
/// per line, and a real JavaScript parse would be a dependency for no gain. It
/// is written to **fail loudly** on anything it does not recognise, which is
/// the property that matters — a shape it cannot read must not be reported as
/// a shape that matches.
fn object_literal(marker: &str) -> Vec<(String, String)> {
    let body = PAGE
        .split_once(marker)
        .unwrap_or_else(|| panic!("webui_index.html no longer contains {marker:?}"))
        .1;
    let body = body
        .split_once("\n    },")
        .or_else(|| body.split_once("\n        },"))
        .unwrap_or_else(|| panic!("the object opened by {marker:?} is not closed as expected"))
        .0;

    body.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with("//"))
        .map(|line| {
            let (key, value) = line
                .split_once(':')
                .unwrap_or_else(|| panic!("cannot read {line:?} as a key/value pair"));
            (
                key.trim().trim_matches('"').to_string(),
                value.trim().trim_end_matches(',').to_string(),
            )
        })
        .collect()
}

/// Resolve one JavaScript value from the page into what it will be on the wire.
///
/// Only the expressions the page actually uses are known. **An unfamiliar one
/// panics rather than being skipped**: silently ignoring it is how a copied
/// request stops matching the page it claims to describe.
fn resolve(raw: &str, tool: &str) -> String {
    match raw {
        "MCP_VERSION" => version_the_page_pins().to_string(),
        "name" => tool.to_string(),
        "{}" => "{}".to_string(),
        lit if lit.starts_with('"') && lit.ends_with('"') => lit.trim_matches('"').to_string(),
        other => panic!(
            "webui_index.html now sends {other:?}, which this test cannot resolve. \
             Teach `resolve` what it means — do not drop it, or the request built \
             here stops being the request the page sends."
        ),
    }
}

/// The `tools/call` the page sends, **built from the page**: the header names
/// and values, and the `_meta` keys, are read out of `callTool` rather than
/// transcribed. Drop `_meta` or rename a header in the page and this changes
/// with it; introduce an expression it cannot read and it panics.
///
/// What is still written here is the JSON-RPC envelope around `params`
/// (`jsonrpc`, `id`, `method`), which `the_page_still_sends_this_envelope`
/// checks separately.
///
/// `include_protocol_header` exists so a test can drop one header and see the
/// difference — without that, "the server accepted it" is equally consistent
/// with a server that reads no headers at all.
fn page_shaped_call(base: &str, tool: &str, include_protocol_header: bool) -> String {
    let meta: Vec<String> = object_literal("        _meta: {")
        .into_iter()
        .map(|(k, v)| {
            let resolved = resolve(&v, tool);
            if resolved == "{}" {
                format!("\"{k}\":{{}}")
            } else {
                format!("\"{k}\":\"{resolved}\"")
            }
        })
        .collect();
    let body = format!(
        r#"{{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{{"name":"{tool}","arguments":{{}},"_meta":{{{}}}}}}}"#,
        meta.join(",")
    );

    let mut args: Vec<String> = vec!["-s".into(), "-X".into(), "POST".into()];
    for (name, raw) in object_literal("    headers: {") {
        if !include_protocol_header && name.eq_ignore_ascii_case("MCP-Protocol-Version") {
            continue;
        }
        args.push("-H".into());
        args.push(format!("{name}: {}", resolve(&raw, tool)));
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

/// The part of the request `page_shaped_call` still writes out rather than
/// reading: the JSON-RPC envelope around `params`. Nothing derives it from the
/// page, so this is what stops it drifting — the same reason the headers and
/// `_meta` are read instead of copied.
#[test]
fn the_page_still_sends_this_envelope() {
    for fragment in [
        "jsonrpc: \"2.0\"",
        "method: \"tools/call\"",
        "name: name,",
        "arguments: args,",
    ] {
        assert!(
            PAGE.contains(fragment),
            "callTool no longer sends {fragment:?}; the request built in this \
             file is no longer the request the page sends"
        );
    }
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
