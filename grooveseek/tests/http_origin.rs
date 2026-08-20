//! Origin validation, exercised through a running server.
//!
//! v0.27.0 shipped the fix for a defect every release before it carried: the
//! MCP specification requires a Streamable HTTP server to validate the `Origin`
//! header against DNS rebinding, rmcp implements the check but defaults it to
//! an empty list meaning *do not validate*, and groove never set it. The fix
//! arrived with twenty-five tests — every one of them against a function that
//! assembles a `Vec<String>`. Not one asked whether that vector reaches rmcp,
//! or what a request carrying a foreign `Origin` actually gets back. Deleting
//! the `.with_allowed_origins(origins)` call in `transport/http.rs` left the
//! entire suite green.
//!
//! So these go through the wire. They are also **not** `#[ignore]`d, which is
//! the other half of the point: PR CI runs `cargo test` without features, so a
//! check that lives behind `--features test-helpers` or behind `#[ignore]` is a
//! check that only nightly performs. A security regression should not wait a
//! day to be noticed. Nothing here builds an index, so no model is downloaded —
//! the same reason `mcp_protocol_surface.rs` stays off `#[ignore]`.
//!
//! The server is bound with `--bind 127.0.0.1:0` rather than a port picked in
//! advance. The allow-list is derived from the address the listener actually
//! got, and only a port this test did not choose can tell the difference
//! between deriving it and echoing it back.
//!
//! **Two surfaces enforce this list, not one.** rmcp checks `Origin` for
//! `/mcp`; `/ui` and `/api/admin/status` are served by axum directly and had
//! no check at all until L-5. rmcp keeps its matcher private, so the admin
//! router cannot call it — what it can do is compare against the identical
//! list, and the second half of this file asks both surfaces the same question
//! and requires the same answer.

mod common;
use common::temp::TempKbLayout;

use std::process::Command;

/// A valid `initialize`, the same shape `http_transport.rs` uses. Origin is
/// checked at the top of rmcp's `handle()`, before anything looks at the body,
/// so what the body says only matters for the requests that are *supposed* to
/// get through: those have to be answerable, or a 4xx from the handler would be
/// indistinguishable from a 4xx from the check.
const INIT_BODY: &str = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"origin-test","version":"0.1"}}}"#;

/// POST `initialize` to `<base>/mcp` and return the HTTP status code.
///
/// `origin = None` sends no `Origin` header at all, which is what an ordinary
/// MCP client, the tray and `curl` do — and per RFC 6454 that request has no
/// origin to check rather than an origin that fails the check.
fn post_initialize(base: &str, origin: Option<&str>) -> String {
    let url = format!("{base}/mcp");
    let mut cmd = Command::new("curl");
    cmd.args([
        "-s",
        "-X",
        "POST",
        "-H",
        "content-type: application/json",
        "-H",
        "accept: application/json, text/event-stream",
    ]);
    if let Some(origin) = origin {
        cmd.args(["-H", &format!("origin: {origin}")]);
    }
    cmd.args([
        "-o",
        "/dev/null",
        "-w",
        "%{http_code}",
        "-d",
        INIT_BODY,
        &url,
    ]);

    let out = cmd.output().expect("curl spawn failed");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// Stand up a server for one of these tests. The fixture itself lives in
/// `common::mcp` — `tests/admin_surface.rs` needs the identical thing, and the
/// notes about drop order and about `--config` versus discovery belong with it.
fn start(prefix: &str, config_body: &str) -> (TempKbLayout, common::mcp::ServerGuard, String) {
    common::mcp::spawn_with_config(prefix, config_body)
}

/// The default list names loopback origins for the bound port and nothing else.
const NO_ORIGIN_KEYS: &str = "[watch]\nenabled = false\n";

/// An empty list is rmcp's "do not validate" — groove warns about it at
/// startup but honours it, and this pins that it really is off rather than
/// merely unset.
const ORIGINS_DISABLED: &str =
    "[watch]\nenabled = false\n\n[transport.http]\nallowed_origins = []\n";

#[test]
fn a_cross_origin_request_is_refused_by_the_default_list() {
    let (_kb, _guard, base) = start("groove-origin-refused", NO_ORIGIN_KEYS);

    let code = post_initialize(&base, Some("https://evil.example"));

    assert_eq!(
        code, "403",
        "a page on another origin reached /mcp and got {code}; \
         the Origin allow-list is not being applied"
    );
}

#[test]
fn a_request_with_no_origin_header_is_allowed() {
    let (_kb, _guard, base) = start("groove-origin-absent", NO_ORIGIN_KEYS);

    let code = post_initialize(&base, None);

    assert_eq!(
        code, "200",
        "a request carrying no Origin got {code}; every ordinary MCP client \
         sends none, so this is what breaks first if the check is too eager"
    );
}

/// The regression this file exists for as much as the refusal itself.
///
/// The allow-list has to be built from the address the listener received, not
/// the one that was requested. Built from the request, `--bind 127.0.0.1:0`
/// yields `http://127.0.0.1:0` — an origin no browser will ever send — and the
/// real port is refused. The base URL here came back from the OS, so asserting
/// against it asserts the derivation.
#[test]
fn the_default_list_names_the_port_the_os_actually_assigned() {
    let (_kb, _guard, base) = start("groove-origin-bound-port", NO_ORIGIN_KEYS);

    let code = post_initialize(&base, Some(&base));

    assert_eq!(
        code, "200",
        "the server refused its own bound address as an Origin ({base}) and \
         answered {code}; the allow-list was derived before the bind"
    );
}

/// Setting the key replaces the default rather than extending it, and this is
/// the end of that sentence: the entry an operator writes is the one that is
/// matched, and the loopback origin that used to work no longer does. It also
/// answers the question the refusal test cannot — that a *correctly* spelled
/// entry reaches rmcp's comparison at all, so the check added in front of it
/// is not simply refusing everything.
#[test]
fn an_explicit_entry_is_the_one_that_gets_matched() {
    const PUBLIC_ORIGIN: &str = "https://kb.example.com";
    let (_kb, _guard, base) = start(
        "groove-origin-explicit",
        "[watch]\nenabled = false\n\n[transport.http]\nallowed_origins = [\"https://kb.example.com\"]\n",
    );

    assert_eq!(
        post_initialize(&base, Some(PUBLIC_ORIGIN)),
        "200",
        "the origin named in the config was refused"
    );
    assert_eq!(
        post_initialize(&base, Some(&base)),
        "403",
        "the loopback origin still passed ({base}); setting the key is \
         documented as replacing the default list, not adding to it"
    );
}

/// Whether a padded entry survives is not a question to settle by reading.
///
/// `check_origin_entry` trims before deciding, on the grounds that rmcp's
/// `parse_origin_value` opens with `value.trim()` and is the only consumer of
/// the allow-list. If that were wrong, the entry would be dropped exactly where
/// we promised it would pass, and the server would answer 403 to every browser
/// — the failure the check exists to prevent, reintroduced by the check itself.
///
/// So it is measured. The origin sent here matches the entry only after the
/// padding is gone.
#[test]
fn an_entry_with_padding_is_still_honoured_by_the_server() {
    const PUBLIC_ORIGIN: &str = "https://kb.example.com";
    let (_kb, _guard, base) = start(
        "groove-origin-padded",
        "[watch]\nenabled = false\n\n[transport.http]\nallowed_origins = [\"  https://kb.example.com  \"]\n",
    );

    assert_eq!(
        post_initialize(&base, Some(PUBLIC_ORIGIN)),
        "200",
        "a padded entry was dropped before matching; check_origin_entry must \
         stop trimming, because the parser it mirrors no longer does"
    );
}

/// A command that opens no socket must not care what this key says.
///
/// The check sat in config loading for one revision, which meant a typo in an
/// HTTP-only setting stopped `index`, `search` and `validate` as well —
/// measured, not theorised: `groove validate`, which reads Markdown and exits,
/// refused to run. It now lives in `Transport::resolve`'s HTTP arm, so this
/// asserts the blast radius rather than the fix.
///
/// `validate` is the cheapest witness available: no model, no database, and
/// with no schema file it exits 0 after a single line.
#[test]
fn a_command_with_no_http_transport_ignores_a_broken_origin_list() {
    let layout = TempKbLayout::new("groove-origin-unrelated-cmd");
    layout.write("note.md", "---\ntitle: Note\n---\n\n## body\n\nOne.\n");
    let cfg = layout.root().join("groove.toml");
    std::fs::write(
        &cfg,
        "[watch]\nenabled = false\n\n[transport.http]\nallowed_origins = [\"127.0.0.1:3100\"]\n",
    )
    .expect("write groove.toml");

    let out = Command::new(common::mcp::grooveseek_bin())
        .args([
            "--config",
            cfg.to_str().unwrap(),
            "validate",
            "--kb-path",
            layout.kb().to_str().unwrap(),
        ])
        .output()
        .expect("spawn groove validate");

    assert!(
        out.status.success(),
        "an HTTP-only setting stopped a command that never reads it; stderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// The defect this pair of changes exists for.
///
/// `allowed_hosts` takes a bare `host:port` — its parser falls back to reading
/// the whole string as a host — so the spelling travels one key over, where
/// rmcp's origin parser requires a scheme, cannot find one, and drops the entry
/// with `filter_map`. The list stays non-empty, so validation stays *on* with
/// nothing left to match, and every request carrying an `Origin` is refused.
/// Nothing warns: `names_a_loopback_host` strips the scheme optionally, reads
/// the host as `127.0.0.1`, and concludes the list covers loopback.
///
/// So the server has to refuse to start. Reaching a listening state at all
/// would mean shipping the 403.
#[test]
fn an_entry_rmcp_would_drop_stops_the_server_from_starting() {
    let layout = TempKbLayout::new("groove-origin-scheme-less");
    layout.write("note.md", "---\ntitle: Note\n---\n\n## body\n\nOne.\n");
    let cfg = layout.root().join("groove.toml");
    std::fs::write(
        &cfg,
        "[watch]\nenabled = false\n\n[transport.http]\nallowed_origins = [\"127.0.0.1:3100\"]\n",
    )
    .expect("write groove.toml");

    let mut child = Command::new(common::mcp::grooveseek_bin())
        .args([
            "--config",
            cfg.to_str().unwrap(),
            "serve",
            "--kb-path",
            layout.kb().to_str().unwrap(),
            "--transport",
            "http",
            "--bind",
            "127.0.0.1:0",
            "--no-watch",
        ])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn groove serve");

    // Polled rather than `output()`: if the guard ever regresses the server
    // starts and serves, and `output()` would hang this test forever instead of
    // failing it. A regression has to be reported, not waited on.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    let status = loop {
        match child.try_wait().expect("try_wait") {
            Some(status) => break status,
            None if std::time::Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                panic!(
                    "the server was still running after 30s with an entry rmcp \
                     drops; it would answer 403 to every browser, including /ui"
                );
            }
            None => std::thread::sleep(std::time::Duration::from_millis(100)),
        }
    };
    assert!(
        !status.success(),
        "groove serve exited successfully with an unusable Origin allow-list"
    );

    let mut stderr = String::new();
    use std::io::Read as _;
    child
        .stderr
        .take()
        .expect("stderr was piped")
        .read_to_string(&mut stderr)
        .expect("read stderr");
    let stderr = common::ansi::strip_ansi(&stderr);
    for needle in [
        "[transport.http].allowed_origins",
        "127.0.0.1:3100",
        "scheme",
        "allowed_hosts",
    ] {
        assert!(
            stderr.contains(needle),
            "the refusal must contain {needle:?} for the operator to act on it, got:\n{stderr}"
        );
    }
}

#[test]
fn an_empty_allowed_origins_really_disables_the_check() {
    let (_kb, _guard, base) = start("groove-origin-disabled", ORIGINS_DISABLED);

    let code = post_initialize(&base, Some("https://evil.example"));

    assert_eq!(
        code, "200",
        "with allowed_origins = [] the foreign origin got {code}; the off \
         switch is documented as off, and a check that cannot be turned off \
         cannot be reasoned about either"
    );
}

// ---------------------------------------------------------------------------
// (L-5) The admin router, which had no Origin check of its own until now.
//
// rmcp guards `/mcp`; these routes are served by axum directly, so nothing
// upstream was ever going to guard them. The list is the same one, and these
// ask what a browser actually gets back.
// ---------------------------------------------------------------------------

/// GET `<base><path>` and return the HTTP status code.
fn get_status(base: &str, path: &str, origin: Option<&str>) -> String {
    let url = format!("{base}{path}");
    let mut cmd = Command::new("curl");
    cmd.args(["-s"]);
    if let Some(origin) = origin {
        cmd.args(["-H", &format!("origin: {origin}")]);
    }
    cmd.args(["-o", "/dev/null", "-w", "%{http_code}", &url]);
    let out = cmd.output().expect("curl spawn failed");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

#[test]
fn a_cross_origin_request_to_the_admin_router_is_refused() {
    let (_kb, _guard, base) = start("groove-admin-origin", NO_ORIGIN_KEYS);

    for path in ["/ui", "/api/admin/status"] {
        let code = get_status(&base, path, Some("https://evil.example"));
        assert_eq!(
            code, "403",
            "{path} answered {code} to a foreign origin. Nothing leaks through \
             a GET a browser cannot read the response of — but the next admin \
             route with a side effect inherits whatever this answers today"
        );
    }
}

/// The failure this check breaks first if it is written too eagerly.
///
/// The page's own poll of `/api/admin/status` is a same-origin `GET`, and a
/// browser sends no `Origin` on one of those. Neither does the tray, nor
/// `curl`. A gate that demanded the header would refuse every real caller
/// while still admitting the attacker, who sends one because they have to.
#[test]
fn the_admin_router_still_answers_a_request_with_no_origin() {
    let (_kb, _guard, base) = start("groove-admin-no-origin", NO_ORIGIN_KEYS);

    for path in ["/ui", "/api/admin/status"] {
        let code = get_status(&base, path, None);
        assert_eq!(
            code, "200",
            "{path} answered {code} to a request carrying no Origin header; \
             that is what /ui's own status poll and the Windows tray send"
        );
    }
}

/// The loopback origin for the port the OS assigned is allowed here too — the
/// same derivation `/mcp` gets, not a second list built from the requested
/// address.
#[test]
fn the_admin_router_allows_the_origin_the_page_is_served_from() {
    let (_kb, _guard, base) = start("groove-admin-own-origin", NO_ORIGIN_KEYS);

    let code = get_status(&base, "/api/admin/status", Some(&base));

    assert_eq!(
        code, "200",
        "the admin router answered {code} to {base}, which is the origin its \
         own page is served from"
    );
}

/// **The reason the admin gate compares against the same list rather than a
/// list of its own.** Two surfaces, one setting: they have to reach the same
/// verdict, or an operator debugging a 403 learns nothing from which URL they
/// tried.
///
/// (codex P1 round 1 on PR #193) It is a *table*, not a pair. rmcp's matcher is
/// private, so the admin gate necessarily holds a second implementation of one
/// verdict — the same arrangement `validate_host_header` has had since #57 —
/// and the failure mode is an rmcp upgrade changing semantics under one of
/// them. Two origins would not notice: they agree on the ordinary allowed one
/// and the ordinary foreign one no matter what either side does with case,
/// brackets, userinfo or a port out of range. **These are the cases where an
/// upgrade would show up first**, and this test is what turns "they might
/// drift" into a CI failure on the day they do.
///
/// The expected verdicts are deliberately *not* written down. Pinning them
/// would make this a test of rmcp, which is not ours to fix; requiring only
/// that the two answers match leaves rmcp free to change and requires us to
/// follow. The final assertion keeps that from degenerating into "both refuse
/// everything, so both agree".
#[test]
fn an_origin_gets_the_same_verdict_from_both_surfaces() {
    let (_kb, _guard, base) = start("groove-origin-agree", NO_ORIGIN_KEYS);
    let port = base
        .rsplit(':')
        .next()
        .expect("base names a port")
        .to_string();

    let cases = vec![
        // The ordinary two.
        ("the origin this page is served from", base.clone()),
        ("a foreign origin", "https://evil.example".to_string()),
        // Spellings of the same allowed origin. Every one of these depends on
        // a normalization step that lives in both implementations.
        ("a loopback alias", format!("http://localhost:{port}")),
        ("bracketed IPv6", format!("http://[::1]:{port}")),
        ("an upper-case scheme", format!("HTTP://127.0.0.1:{port}")),
        ("an upper-case host", format!("http://LOCALHOST:{port}")),
        (
            "userinfo before the host",
            format!("http://u@127.0.0.1:{port}"),
        ),
        // Near misses: right host, wrong one component.
        ("the wrong scheme", format!("https://127.0.0.1:{port}")),
        ("no port at all", "http://127.0.0.1".to_string()),
        ("a different port", "http://127.0.0.1:1".to_string()),
        // Not tuple origins.
        ("the opaque origin", "null".to_string()),
        // Unparseable, where the two could differ on 400 versus 403.
        ("no scheme", format!("127.0.0.1:{port}")),
        ("a scheme and nothing else", "http://".to_string()),
        ("a port out of range", "http://127.0.0.1:99999".to_string()),
        ("not a URI at all", "banana".to_string()),
    ];

    let mut verdicts = Vec::new();
    for (what, origin) in &cases {
        let mcp = post_initialize(&base, Some(origin));
        let admin = get_status(&base, "/api/admin/status", Some(origin));
        assert_eq!(
            mcp, admin,
            "{what} ({origin}) got {mcp} from /mcp and {admin} from \
             /api/admin/status. rmcp guards one and this repository guards the \
             other; they read one list and must reach one verdict"
        );
        verdicts.push(mcp);
    }

    // Without this, a gate that refused every request would pass the loop.
    assert!(
        verdicts.iter().any(|v| v == "200"),
        "no case was allowed, so the agreement above is the agreement of two \
         closed doors: {verdicts:?}"
    );
    assert!(
        verdicts.iter().any(|v| v != "200"),
        "no case was refused, so the agreement above is the agreement of two \
         open doors: {verdicts:?}"
    );
}

/// Host is checked before Origin, as it is in rmcp
/// (`validate_dns_rebinding_headers`). A request failing both therefore hears
/// about the Host — from either surface, which is the part worth pinning: the
/// admin router assembles its own layers, and layer order there is the reverse
/// of the order they are written in.
///
/// **Its blind spot, measured:** it also passes with the Origin gate deleted
/// outright, since the Host gate then answers by default. Order is all this one
/// checks; the three tests above are what require the gate to be there at all.
#[test]
fn the_host_check_answers_before_the_origin_check() {
    let (_kb, _guard, base) = start("groove-admin-gate-order", NO_ORIGIN_KEYS);

    let out = Command::new("curl")
        .args([
            "-s",
            "-H",
            "host: kb.example.lan",
            "-H",
            "origin: https://evil.example",
            &format!("{base}/api/admin/status"),
        ])
        .output()
        .expect("curl spawn failed");
    let body = String::from_utf8_lossy(&out.stdout);

    assert!(
        body.to_ascii_lowercase().contains("host"),
        "a request failing both gates was answered with {body:?}; the Host \
         check runs first for /mcp, and the two surfaces should not differ on \
         which refusal an operator sees"
    );
}

#[test]
fn an_empty_allowed_origins_disables_the_admin_check_too() {
    let (_kb, _guard, base) = start("groove-admin-disabled", ORIGINS_DISABLED);

    let code = get_status(&base, "/api/admin/status", Some("https://evil.example"));

    assert_eq!(
        code, "200",
        "with allowed_origins = [] the admin router answered {code}; the off \
         switch turns off Origin validation, and an operator who read the \
         warning and set it anyway did not ask for one surface to keep it on"
    );
}
