//! One gate, in front of every route — measured through a running server.
//!
//! [ADR-0009](../../docs/decisions/0009-one-dns-rebinding-gate.md) collapsed
//! four implementations of two questions into one. Before it, rmcp answered
//! "is this Host allowed?" for `/mcp` while `validate_host_header` answered it
//! for `/healthz` and the admin routes, both handed the same list and expected
//! to agree.
//!
//! **They did not.** Measured on the commit before this file existed, with the
//! identical `effective_hosts` list:
//!
//! ```text
//! Host                        /mcp (rmcp)   /healthz (groove)
//! user:pw@127.0.0.1:PORT      200           400
//! @127.0.0.1:PORT             200           400
//! 127.0.0.1@localhost         200           400
//! 127.0.0.1:65536             200           400
//! localhost:abc               200           400
//! ```
//!
//! Five spellings, every one of them groove refusing what rmcp accepted, and
//! no spelling where rmcp was the stricter of the two. That asymmetry is what
//! made the change safe: adopting groove's check for `/mcp` could only tighten.
//!
//! So this file asks the surfaces the same questions and requires the same
//! answers — and pins the five, because a refusal only groove produces is the
//! fingerprint of *which* implementation is enforcing. Revert the gate and
//! rmcp's permissiveness comes back with it.
//!
//! Origin parity lives in `http_origin.rs`, which was written for it and has
//! the two tables (request spellings and configured entries).

mod common;

use std::process::Command;

/// `/healthz` is public by default, so its Host check has to be switched on
/// before it can be compared with anything.
const HEALTHZ_GUARDED: &str =
    "[watch]\nenabled = false\n\n[transport.http]\nhealthz_public = false\n";

const INIT_BODY: &str = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"gate-test","version":"0.1"}}}"#;

/// POST `initialize` to `<base>/mcp` with the given Host, returning the status.
fn mcp_status(base: &str, host: &str) -> String {
    let out = Command::new("curl")
        .args([
            "-s",
            "-X",
            "POST",
            "-H",
            "content-type: application/json",
            "-H",
            "accept: application/json, text/event-stream",
            "-H",
            &format!("host: {host}"),
            "-o",
            "/dev/null",
            "-w",
            "%{http_code}",
            "-d",
            INIT_BODY,
            &format!("{base}/mcp"),
        ])
        .output()
        .expect("curl spawn failed");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// GET `<base><path>` with the given Host, returning the status.
fn get_status(base: &str, path: &str, host: &str) -> String {
    let out = Command::new("curl")
        .args([
            "-s",
            "-H",
            &format!("host: {host}"),
            "-o",
            "/dev/null",
            "-w",
            "%{http_code}",
            &format!("{base}{path}"),
        ])
        .output()
        .expect("curl spawn failed");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// Every Host spelling that told the two implementations apart, plus enough
/// ordinary and refused ones that the comparison is not two closed doors.
///
/// `{port}` is replaced with the port the OS assigned.
const HOSTS: &[&str] = &[
    // Ordinary, and accepted.
    "127.0.0.1:{port}",
    "127.0.0.1",
    "localhost:{port}",
    "LOCALHOST:{port}",
    "[::1]:{port}",
    "127.0.0.1:03177",
    // The five that used to disagree.
    "user:pw@127.0.0.1:{port}",
    "@127.0.0.1:{port}",
    "127.0.0.1@localhost",
    "127.0.0.1:65536",
    "localhost:abc",
    // Refused by both, before and after.
    "kb.example.lan",
    "0.0.0.0:{port}",
    "localhost.",
    "127.1",
    "2130706433",
    "[::ffff:127.0.0.1]:{port}",
    "::ffff:127.0.0.1",
    "localhost:{port}:{port}",
    "localhost#frag",
    "localhost/",
    "127.0.0.1%00",
];

/// The five spellings `/mcp` used to accept and its neighbours refused. They
/// are asserted by *value*, not by parity, because parity alone is satisfied by
/// reverting both surfaces to rmcp — and rmcp is what accepted these.
const ONLY_GROOVE_REFUSES: &[&str] = &[
    "user:pw@127.0.0.1:{port}",
    "@127.0.0.1:{port}",
    "127.0.0.1@localhost",
    "127.0.0.1:65536",
    "localhost:abc",
];

fn port_of(base: &str) -> String {
    base.rsplit(':')
        .next()
        .expect("base names a port")
        .to_string()
}

#[test]
fn every_surface_reaches_the_same_verdict_on_a_host() {
    let (_kb, _guard, base) = common::mcp::spawn_with_config("groove-gate-parity", HEALTHZ_GUARDED);
    let port = port_of(&base);

    let mut verdicts = Vec::new();
    for spelling in HOSTS {
        let host = spelling.replace("{port}", &port);
        let mcp = mcp_status(&base, &host);
        let healthz = get_status(&base, "/healthz", &host);
        assert_eq!(
            mcp, healthz,
            "Host {host:?} got {mcp} from /mcp and {healthz} from /healthz. \
             Both are handed the same list by run_http, and since ADR-0009 the \
             same gate reads it"
        );
        verdicts.push(mcp);
    }

    assert!(
        verdicts.iter().any(|v| v == "200"),
        "no Host was accepted, so the agreement above is two closed doors: \
         {verdicts:?}"
    );
    assert!(
        verdicts.iter().any(|v| v != "200"),
        "no Host was refused: {verdicts:?}"
    );
}

/// The admin routes carry their own Host list (`allowed_admin_hosts`, loopback
/// plus the bind address, not configurable), so they cannot be compared on the
/// *allowed* set. What they must share is the reading of a spelling: the five
/// below are malformed, and malformed does not depend on which list you hold.
#[test]
fn the_admin_routes_read_a_host_the_same_way() {
    let (_kb, _guard, base) = common::mcp::spawn_with_config("groove-gate-admin", HEALTHZ_GUARDED);
    let port = port_of(&base);

    for spelling in ONLY_GROOVE_REFUSES {
        let host = spelling.replace("{port}", &port);
        let mcp = mcp_status(&base, &host);
        let admin = get_status(&base, "/api/admin/status", &host);
        assert_eq!(
            mcp, admin,
            "Host {host:?} got {mcp} from /mcp and {admin} from \
             /api/admin/status; a malformed Host is malformed on both"
        );
    }
}

/// **The fingerprint.** These five are refused by `validate_host_header` and
/// were accepted by rmcp, so a green run here says groove's gate is the thing
/// answering — which parity alone cannot say.
#[test]
fn the_gate_in_front_is_the_one_answering() {
    let (_kb, _guard, base) = common::mcp::spawn_with_config("groove-gate-finger", HEALTHZ_GUARDED);
    let port = port_of(&base);

    for spelling in ONLY_GROOVE_REFUSES {
        let host = spelling.replace("{port}", &port);
        let code = mcp_status(&base, &host);
        assert_eq!(
            code, "400",
            "/mcp answered {code} to Host {host:?}. rmcp accepts this spelling \
             and groove refuses it, so a 200 here means rmcp's own check is \
             back in front — the arrangement ADR-0009 ended"
        );
    }
}

/// Bodies too, not only statuses. `/mcp`'s wording is rmcp's, kept verbatim so
/// that taking the check over changed nothing a client can see; the admin
/// routes used to answer without the prefix, and now do not.
///
/// **Its blind spot, measured:** it is the one test here that still passes when
/// `/mcp` is handed back to rmcp, because the wording it asserts is rmcp's own.
/// What it guards is the admin prefix, not which implementation answered — the
/// four tests around it cover that.
#[test]
fn a_refusal_reads_the_same_whichever_route_produced_it() {
    let (_kb, _guard, base) = common::mcp::spawn_with_config("groove-gate-body", HEALTHZ_GUARDED);

    let body = |path: &str, args: &[&str]| -> String {
        let mut cmd = Command::new("curl");
        cmd.args(["-s", "-H", "host: kb.example.lan"]);
        cmd.args(args);
        cmd.arg(format!("{base}{path}"));
        let out = cmd.output().expect("curl spawn failed");
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    };

    let mcp = body(
        "/mcp",
        &[
            "-X",
            "POST",
            "-H",
            "content-type: application/json",
            "-H",
            "accept: application/json, text/event-stream",
            "-d",
            INIT_BODY,
        ],
    );
    let healthz = body("/healthz", &[]);
    let admin = body("/api/admin/status", &[]);

    assert_eq!(
        mcp, "Forbidden: Host header is not allowed",
        "/mcp's wording is rmcp's and must survive the handover verbatim"
    );
    assert_eq!(healthz, mcp, "/healthz disagrees with /mcp: {healthz:?}");
    assert_eq!(admin, mcp, "the admin routes disagree with /mcp: {admin:?}");
}

/// Validation happens **before** admission, which it did not when rmcp owned
/// it: `validate_dns_rebinding_headers` runs inside `handle()`, and the session
/// gate is a layer in front of that. So a refused `initialize` used to reserve
/// a seat and release it on the way out, letting a stream of requests with a
/// foreign Host push legitimate clients into 429.
///
/// Measured both ways — moving the gate inside the session gate turns the last
/// assertion here from 403 into 429.
#[test]
fn a_refused_request_never_reaches_the_session_limit() {
    let (_kb, _guard, base) = common::mcp::spawn_with_config(
        "groove-gate-seat",
        "[watch]\nenabled = false\n\n[transport.http]\nmax_sessions = 1\n",
    );
    let port = port_of(&base);
    let good = format!("127.0.0.1:{port}");

    assert_eq!(
        mcp_status(&base, &good),
        "200",
        "the first initialize did not get the only seat"
    );
    assert_eq!(
        mcp_status(&base, &good),
        "429",
        "the seat is not actually held, so the rest of this test proves nothing"
    );
    assert_eq!(
        mcp_status(&base, "kb.example.lan"),
        "403",
        "a foreign Host was answered by the session limit rather than by the \
         gate, which means the gate is no longer the outermost layer"
    );
}
