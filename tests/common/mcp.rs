//! Shared MCP / kb-mcp binary helpers extracted from
//! `tests/search_mmr_integration.rs` and `tests/search_parent_integration.rs`
//! as part of feature-34 / F-55. Used by integration tests that spawn the
//! kb-mcp binary, perform an MCP HTTP handshake, and issue `tools/call`
//! requests for the `search` tool.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

/// Locate the kb-mcp binary under test. Cargo sets `CARGO_BIN_EXE_<name>`
/// for integration tests automatically.
pub fn kb_mcp_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_kb-mcp"))
}

/// Pick a free ephemeral TCP port via `bind 127.0.0.1:0` then drop the
/// listener. TOCTOU between drop and the spawned server's bind exists in
/// theory but is fine for an integration test (same approach
/// `tests/http_transport.rs` uses).
pub fn pick_free_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    port
}

/// Poll `<url>` until 200 or `deadline` expires.
///
/// **TODO (feature-34 / F-55, Windows compatibility)**: `curl -o /dev/null`
/// uses the POSIX null-device path. Windows `curl` (Win10+) treats unknown
/// device paths as regular files, which **may** still work because curl
/// opens it with O_WRONLY and writes the body away — but the formal cross-
/// platform spelling is `-o nul`. The existing mmr/parent integration tests
/// are all `#[ignore]` and run on Linux CI only, so the issue is latent.
/// Cross-platform fix is deferred to F-58 (CI 3-OS matrix) when this code
/// path will actually be exercised on Windows / macOS runners.
pub fn wait_http_200(url: &str, deadline: Duration) -> bool {
    let start = Instant::now();
    while start.elapsed() < deadline {
        let out = Command::new("curl")
            .args(["-s", "-o", "/dev/null", "-w", "%{http_code}", url])
            .output();
        if let Ok(out) = out
            && let Ok(code) = String::from_utf8(out.stdout)
            && code.trim() == "200"
        {
            return true;
        }
        thread::sleep(Duration::from_millis(300));
    }
    false
}

/// Spawn `kb-mcp serve --transport http` against the given KB + config and
/// wait for `/healthz` to come up. Returns `(ServerGuard, base_url)`.
pub fn spawn_mcp_server(kb_path: &Path, config_path: &Path) -> (ServerGuard, String) {
    let port = pick_free_port();
    let bin = kb_mcp_bin();
    assert!(
        bin.exists(),
        "binary not found at {} — run `cargo build` first",
        bin.display()
    );

    let child = Command::new(&bin)
        .args([
            "--config",
            config_path.to_str().unwrap(),
            "serve",
            "--kb-path",
            kb_path.to_str().unwrap(),
            "--transport",
            "http",
            "--port",
            &port.to_string(),
            "--no-watch",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn kb-mcp serve");

    let base = format!("http://127.0.0.1:{port}");
    let guard = ServerGuard { child: Some(child) };

    // 60 s upper bound: covers BGE-small first-time DL on cold cache.
    if !wait_http_200(&format!("{base}/healthz"), Duration::from_secs(60)) {
        // guard's Drop will reap the child; surface a useful error.
        panic!("/healthz did not return 200 within 60s — server failed to start");
    }
    (guard, base)
}

/// RAII handle for the spawned MCP server child. Kills + reaps on Drop so
/// a panicking test does not orphan the server process.
pub struct ServerGuard {
    child: Option<Child>,
}

impl Drop for ServerGuard {
    fn drop(&mut self) {
        if let Some(mut c) = self.child.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

/// Issue a JSON-RPC `initialize` against `<base>/mcp` and return the
/// `Mcp-Session-Id` header value. Subsequent `tools/call` requests must
/// echo this header back per the Streamable HTTP spec.
pub fn mcp_initialize(base: &str) -> String {
    let init_body = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"it","version":"0.1"}}}"#;
    let out = Command::new("curl")
        .args([
            "-s",
            "-i",
            "-X",
            "POST",
            "-H",
            "content-type: application/json",
            "-H",
            "accept: application/json, text/event-stream",
            "-d",
            init_body,
            &format!("{base}/mcp"),
        ])
        .output()
        .expect("curl initialize");
    assert!(
        out.status.success(),
        "curl initialize failed: stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let lower = stdout.to_ascii_lowercase();
    let h = "mcp-session-id:";
    let idx = lower
        .find(h)
        .unwrap_or_else(|| panic!("no mcp-session-id header in response:\n{stdout}"));
    let after = &stdout[idx + h.len()..];
    let end = after.find('\n').unwrap_or(after.len());
    after[..end].trim().trim_end_matches('\r').to_string()
}

/// POST a `tools/call` request for the `search` tool with `arguments` =
/// the given JSON value. Returns the deserialized JSON value of the
/// `result.content[0].text` (= the inner SearchResponse JSON our server
/// produces).
pub fn mcp_search_call(
    base: &str,
    session_id: &str,
    arguments: serde_json::Value,
) -> serde_json::Value {
    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/call",
        "params": {
            "name": "search",
            "arguments": arguments,
        }
    });
    let body_str = serde_json::to_string(&body).unwrap();
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
            "MCP-Protocol-Version: 2025-06-18",
            "-H",
            &format!("Mcp-Session-Id: {session_id}"),
            "-d",
            &body_str,
            &format!("{base}/mcp"),
        ])
        .output()
        .expect("curl tools/call");
    assert!(
        out.status.success(),
        "curl tools/call failed: stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let payload = stdout
        .lines()
        .filter_map(|line| {
            line.strip_prefix("data:")
                .or_else(|| line.strip_prefix("data: "))
                .map(|s| s.trim())
        })
        .find(|s| !s.is_empty())
        .unwrap_or_else(|| panic!("no non-empty `data:` line in SSE body:\n{stdout}"));
    let envelope: serde_json::Value = serde_json::from_str(payload)
        .unwrap_or_else(|e| panic!("invalid JSON-RPC envelope ({e}): {payload}"));
    let text = envelope
        .pointer("/result/content/0/text")
        .and_then(|v| v.as_str())
        .unwrap_or_else(|| panic!("missing result.content[0].text in envelope:\n{envelope}"));
    serde_json::from_str(text)
        .unwrap_or_else(|e| panic!("inner content text is not JSON ({e}): {text}"))
}

/// Run `kb-mcp index` against the given KB so the SQLite + vec index is
/// populated before we spawn the server. Uses BGE-small for speed.
pub fn build_index(kb_path: &Path) {
    let bin = kb_mcp_bin();
    let st = Command::new(&bin)
        .args([
            "index",
            "--kb-path",
            kb_path.to_str().unwrap(),
            "--model",
            "bge-small-en-v1.5",
        ])
        .status()
        .expect("kb-mcp index");
    assert!(st.success(), "kb-mcp index failed");
}

/// Extract `(path, heading)` order from a SearchResponse-shaped JSON.
/// Used as a stable cross-OS proxy for the chunk-id sequence (raw f32
/// score is not bit-exact across architectures).
pub fn extract_path_heading_order(resp: &serde_json::Value) -> Vec<(String, String)> {
    resp["results"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .map(|hit| {
                    let p = hit["path"].as_str().unwrap_or("").to_string();
                    let h = hit["heading"].as_str().unwrap_or("").to_string();
                    (p, h)
                })
                .collect()
        })
        .unwrap_or_default()
}
