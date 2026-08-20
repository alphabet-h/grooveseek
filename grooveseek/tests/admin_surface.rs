//! What the admin router puts on the wire: its response headers, what its
//! refusals say, and what its status payload carries.
//!
//! Three audit findings, one surface. `/ui` shipped with no `Content-Security-
//! Policy` and no `X-Content-Type-Options` (L-6), its Host refusal quoted the
//! caller's own header back into the body (L-7), and `/api/admin/status`
//! returned the knowledge base's absolute path — an operator's home directory,
//! printed in the status band of the page most likely to end up in a
//! screenshot (L-8).
//!
//! Through a running server, and not `#[ignore]`d, for the reason
//! `http_origin.rs` gives at length: PR CI runs `cargo test` with no features,
//! so a check behind `--features test-helpers` is a check only nightly
//! performs. Nothing here builds an index, so no model is downloaded.
//!
//! `build_router_for_test` would have been cheaper and would have proved less.
//! It composes its own layers, so it can agree with these assertions while the
//! server disagrees — which is the shape of the defect this project keeps
//! finding (PR #189: deleting `.with_allowed_origins()` left 1,427 tests
//! green).

mod common;

use std::process::Command;

/// The default configuration: watcher off, nothing said about the transport.
const PLAIN: &str = "[watch]\nenabled = false\n";

/// Fetch `<base><path>` and return `(status, headers, body)` as one lowercased
/// header blob plus the untouched body.
fn fetch(base: &str, path: &str, extra: &[&str]) -> (String, String, String) {
    let mut cmd = Command::new("curl");
    cmd.args(["-s", "-i"]);
    for arg in extra {
        cmd.arg(arg);
    }
    cmd.arg(format!("{base}{path}"));
    let out = cmd.output().expect("curl spawn failed");
    let raw = String::from_utf8_lossy(&out.stdout).replace("\r\n", "\n");
    // A `-i` response is headers, a blank line, then the body. Split on the
    // first blank line only: the body may contain them, and `/ui` does.
    let (head, body) = raw.split_once("\n\n").unwrap_or((raw.as_str(), ""));
    let status = head
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .unwrap_or("")
        .to_string();
    (status.clone(), head.to_ascii_lowercase(), body.to_string())
}

/// Read one header value out of the lowercased blob `fetch` returns.
fn header(headers: &str, name: &str) -> String {
    headers
        .lines()
        .find_map(|line| line.strip_prefix(&format!("{name}:")))
        .unwrap_or("")
        .trim()
        .to_string()
}

// ---------------------------------------------------------------------------
// (L-6) The headers.
// ---------------------------------------------------------------------------

/// Every directive is asserted by name, because the policy is written as a
/// multi-line Rust string literal joined with `\`-continuations — a form that
/// silently loses a space, or a whole directive, without failing to compile.
#[test]
fn the_page_is_served_with_a_policy_that_names_what_it_uses() {
    let (_kb, _guard, base) = common::mcp::spawn_with_config("groove-admin-csp", PLAIN);

    let (status, headers, _) = fetch(&base, "/ui", &[]);
    assert_eq!(status, "200", "/ui did not answer 200");

    let csp = header(&headers, "content-security-policy");
    assert!(!csp.is_empty(), "/ui carries no Content-Security-Policy");

    for directive in [
        // Nothing loads unless a later directive says otherwise.
        "default-src 'none'",
        // The page's own inline <script> and <style>, and nothing external.
        "script-src 'unsafe-inline'",
        "style-src 'unsafe-inline'",
        // The `data:` favicon in <link rel="icon"> — CSP counts favicons under
        // img-src, so `default-src 'none'` alone would drop it.
        "img-src data:",
        // fetch() to /mcp and /api/admin/status, both same-origin.
        "connect-src 'self'",
        "base-uri 'none'",
        "form-action 'none'",
        // Clickjacking. This one is why the policy is a header: CSP3 ignores
        // frame-ancestors when the policy arrives in a <meta> element.
        "frame-ancestors 'none'",
    ] {
        assert!(
            csp.contains(directive),
            "the policy is missing {directive:?}, got {csp:?}"
        );
    }

    assert_eq!(
        header(&headers, "x-content-type-options"),
        "nosniff",
        "/ui carries no nosniff"
    );
}

/// The policy has to describe the page that is actually served. Reading both
/// and comparing them is what keeps this from becoming a comment: adding a
/// `<img src>` or a CDN `<script>` to the page fails here rather than in
/// somebody's browser console.
#[test]
fn the_policy_covers_everything_the_page_asks_for() {
    let (_kb, _guard, base) = common::mcp::spawn_with_config("groove-admin-csp-page", PLAIN);

    let (_, headers, page) = fetch(&base, "/ui", &[]);
    let csp = header(&headers, "content-security-policy");
    // Measured: without this line the test passes with the header layer
    // deleted, because every assertion below is about the page. A comparison
    // is only worth what its weaker side is worth.
    assert!(
        !csp.is_empty(),
        "there is no policy to compare the page against"
    );

    // What the page must not have grown without the policy growing with it.
    // Each pair is (what to look for, which directive would have to allow it).
    for (marker, needed) in [
        ("<script src=", "script-src with a source"),
        ("<link rel=\"stylesheet\"", "style-src with a source"),
        ("<img ", "img-src beyond data:"),
        ("<iframe", "frame-src"),
    ] {
        assert!(
            !page.contains(marker),
            "the page now contains {marker:?}, which needs {needed}; the \
             policy served with it is {csp:?}"
        );
    }
    // The two fetches the page does make are same-origin, which is what
    // `connect-src 'self'` permits. Absolute URLs would not be.
    assert!(
        page.contains("fetch(\"/mcp\"") && page.contains("fetch(\"/api/admin/status\")"),
        "the page no longer fetches /mcp and /api/admin/status by relative \
         path; connect-src 'self' does not cover an absolute URL"
    );
}

/// A refusal is a document a browser renders too. Headers set inside the gates
/// would not reach it.
#[test]
fn a_refusal_carries_the_same_headers() {
    let (_kb, _guard, base) = common::mcp::spawn_with_config("groove-admin-csp-403", PLAIN);

    let (status, headers, _) = fetch(&base, "/ui", &["-H", "host: kb.example.lan"]);

    assert_eq!(status, "403", "a foreign Host was not refused");
    assert!(
        !header(&headers, "content-security-policy").is_empty(),
        "the 403 carries no Content-Security-Policy"
    );
    assert_eq!(
        header(&headers, "x-content-type-options"),
        "nosniff",
        "the 403 carries no nosniff"
    );
}

// ---------------------------------------------------------------------------
// (L-7) What a refusal says.
// ---------------------------------------------------------------------------

/// The body used to read `Host 'kb.example.lan' not in admin allow-list`.
///
/// Nothing was exploitable about it — the response is `text/plain` and now
/// carries `nosniff` besides — but it was the one gate here that echoed the
/// caller's bytes back, while `/healthz` next door and rmcp on `/mcp` both say
/// only that the header was not allowed. The value still reaches the console,
/// which is where an operator can act on it.
#[test]
fn a_refusal_does_not_repeat_the_host_it_refused() {
    let (_kb, _guard, base) = common::mcp::spawn_with_config("groove-admin-echo", PLAIN);

    let marker = "kb.rejected.example";
    let (status, _, body) = fetch(
        &base,
        "/api/admin/status",
        &["-H", &format!("host: {marker}")],
    );

    assert_eq!(status, "403", "the foreign Host was not refused");
    assert!(
        !body.contains(marker),
        "the refusal repeated the Host it was sent: {body:?}"
    );
    assert!(
        body.to_ascii_lowercase().contains("host"),
        "the refusal no longer says which header it was about: {body:?}"
    );
}

/// The same rule for the Origin gate, which is new and could have been written
/// with the same habit.
#[test]
fn an_origin_refusal_does_not_repeat_the_origin() {
    let (_kb, _guard, base) = common::mcp::spawn_with_config("groove-admin-echo-origin", PLAIN);

    let marker = "https://rejected.example";
    let (status, _, body) = fetch(
        &base,
        "/api/admin/status",
        &["-H", &format!("origin: {marker}")],
    );

    assert_eq!(status, "403", "the foreign Origin was not refused");
    assert!(
        !body.contains("rejected.example"),
        "the refusal repeated the Origin it was sent: {body:?}"
    );
}

// ---------------------------------------------------------------------------
// (L-8) What the status payload carries.
// ---------------------------------------------------------------------------

/// The removed field held `kb_path` verbatim, so on Windows it read
/// `C:\Users\<name>\...`. This asserts on the payload rather than on the struct
/// because the struct is what a reviewer reads and the payload is what leaves
/// the machine.
#[test]
fn the_status_payload_carries_no_filesystem_path() {
    let (kb, _guard, base) = common::mcp::spawn_with_config("groove-admin-path", PLAIN);

    let (status, _, body) = fetch(&base, "/api/admin/status", &[]);
    assert_eq!(status, "200", "/api/admin/status did not answer 200");

    let json: serde_json::Value =
        serde_json::from_str(&body).unwrap_or_else(|e| panic!("not JSON ({e}): {body}"));

    assert!(
        json["kb"]["path"].is_null(),
        "kb.path is back in the payload: {body}"
    );

    // The specific path this server was started with, in either spelling
    // Windows might hand back, must not appear anywhere in the response —
    // including under a different key.
    let root = kb.root().display().to_string();
    for spelling in [root.clone(), root.replace('\\', "/")] {
        assert!(
            !body.contains(&spelling),
            "the payload still discloses {spelling:?}: {body}"
        );
    }
}

/// What the field was removed *for*: the page has to stay able to say which
/// knowledge base this is. Counts and the model name do that without naming a
/// directory.
#[test]
fn the_status_payload_still_identifies_the_knowledge_base() {
    let (_kb, _guard, base) = common::mcp::spawn_with_config("groove-admin-identity", PLAIN);

    let (_, _, body) = fetch(&base, "/api/admin/status", &[]);
    let json: serde_json::Value = serde_json::from_str(&body).expect("status is JSON");

    for key in ["documents", "chunks", "model"] {
        assert!(
            json["kb"].get(key).is_some(),
            "kb.{key} is missing, and it is part of what replaced kb.path: {body}"
        );
    }
    // The tray reads exactly these two and nothing else
    // (`crates/groove-tray/src/state.rs`).
    assert!(
        json["daemon"]["pid"].is_u64(),
        "daemon.pid is missing; the tray stops the daemon with it: {body}"
    );
    assert!(
        json["indexing"]["active"].is_boolean(),
        "indexing.active is missing; it drives the tray's status dot: {body}"
    );
}

/// The page must not still be reaching for the field that is gone. A JSON
/// `undefined` renders as nothing in some places and as the string `undefined`
/// in others, and this page builds its status band by hand.
#[test]
fn the_page_no_longer_reads_the_removed_field() {
    let (_kb, _guard, base) = common::mcp::spawn_with_config("groove-admin-page-path", PLAIN);

    let (_, _, page) = fetch(&base, "/ui", &[]);

    assert!(
        !page.contains("kb.path"),
        "the served page still reads kb.path, which /api/admin/status no \
         longer returns"
    );
}
