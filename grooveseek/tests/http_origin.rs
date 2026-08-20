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

mod common;
use common::mcp::spawn_mcp_server_ephemeral;
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

/// The layout is returned first so it drops **last**: locals drop in reverse
/// declaration order, and its `Drop` removes a directory the server still has a
/// database open under (codex P2 round 1 on PR #160).
///
/// `config_body` is written to a file passed with `--config`. That matters for
/// the empty-list case: a `groove.toml` groove *discovered* has its
/// `allowed_origins` stripped precisely so nobody can turn the check off by
/// leaving a file beside the binary, so a test of the off switch has to hand
/// the file over explicitly.
fn start(prefix: &str, config_body: &str) -> (TempKbLayout, common::mcp::ServerGuard, String) {
    let layout = TempKbLayout::new(prefix);
    layout.write(
        "note.md",
        "---\ntitle: Note\n---\n\n## body\n\nOne document so the knowledge base is not empty.\n",
    );
    let cfg = layout.root().join("groove.toml");
    std::fs::write(&cfg, config_body).expect("write groove.toml");
    let (guard, base) = spawn_mcp_server_ephemeral(layout.kb(), &cfg);
    (layout, guard, base)
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
