//! HTTP Streamable transport integration test.
//!
//! `#[ignore]` 前提: embedder のモデル DL (BGE-small ~130 MB 以上) が必要で
//! 通常の `cargo test` には載せない。明示的に
//! `cargo test --test http_transport -- --ignored` で実行する。
//!
//! 検証内容:
//! - `kb-mcp serve --transport http` を ephemeral port で spawn
//! - `/healthz` が 200 "ok" を返す
//! - `/mcp` に JSON-RPC initialize を POST して 200 を返す

use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

/// 手動 smoke test の自動化版。実バイナリ (`target/release/kb-mcp.exe` or
/// `target/debug/kb-mcp`) が存在することを前提にし、ephemeral port で起動して
/// `GET /healthz` と `POST /mcp` を叩く。
///
/// `FASTEMBED_CACHE_DIR` が事前に設定されていない環境 (CI 等) では
/// embedding モデルの初回 DL が走るため、必要に応じて skip する。
#[test]
#[ignore]
fn test_http_serve_healthz_and_initialize() {
    let bin = kb_mcp_bin();
    assert!(
        bin.exists(),
        "binary not found at {}. Run `cargo build` first.",
        bin.display()
    );

    // Temporary KB directory with 1 markdown file + index it first.
    let kb_dir = tempdir("kb-mcp-http-it");
    std::fs::create_dir_all(kb_dir.join("knowledge-base")).unwrap();
    std::fs::write(
        kb_dir.join("knowledge-base").join("a.md"),
        "---\ntitle: Hello\n---\n\n# Body\n\nplain text.\n",
    )
    .unwrap();

    // Pre-index so the HTTP server has something to serve.
    let out = Command::new(&bin)
        .args([
            "index",
            "--kb-path",
            kb_dir.join("knowledge-base").to_str().unwrap(),
        ])
        .output()
        .expect("kb-mcp index failed to spawn");
    assert!(
        out.status.success(),
        "kb-mcp index failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // Spawn the HTTP server.
    let port = pick_free_port();
    let mut child = Command::new(&bin)
        .args([
            "serve",
            "--kb-path",
            kb_dir.join("knowledge-base").to_str().unwrap(),
            "--transport",
            "http",
            "--port",
            &port.to_string(),
            "--no-watch",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("kb-mcp serve failed to spawn");

    let base = format!("http://127.0.0.1:{port}");
    let healthz_ok = wait_http_200(&format!("{base}/healthz"), Duration::from_secs(60));
    if !healthz_ok {
        let _ = child.kill();
        panic!("/healthz did not return 200 within 60s");
    }

    // POST initialize to /mcp.
    let init_body = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"it","version":"0.1"}}}"#;
    let out = Command::new("curl")
        .args([
            "-s",
            "-X",
            "POST",
            "-H",
            "content-type: application/json",
            "-H",
            "accept: application/json, text/event-stream",
            "-o",
            "/dev/null",
            "-w",
            "%{http_code}",
            "-d",
            init_body,
            &format!("{base}/mcp"),
        ])
        .output()
        .expect("curl spawn failed");
    let code = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let _ = child.kill();
    let _ = child.wait();
    assert_eq!(code, "200", "initialize returned {code}, expected 200");
}

// ---------------------------------------------------------------------------
// Helpers (no tempfile / reqwest dep — keep integration test lightweight)
// ---------------------------------------------------------------------------

fn kb_mcp_bin() -> std::path::PathBuf {
    let target = std::env::var("CARGO_TARGET_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target"));
    let profile = if cfg!(debug_assertions) {
        "debug"
    } else {
        "release"
    };
    #[cfg(windows)]
    let bin = target.join(profile).join("kb-mcp.exe");
    #[cfg(not(windows))]
    let bin = target.join(profile).join("kb-mcp");
    bin
}

fn tempdir(prefix: &str) -> std::path::PathBuf {
    let pid = std::process::id();
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let d = std::env::temp_dir().join(format!("{prefix}-{pid}-{nonce}"));
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// tokio なしで ephemeral port を拾う: bind して local_addr を取り、すぐ drop。
/// 取った直後に別プロセスに取られる TOCTOU は理論上あるが、integration test の
/// 範囲では十分。
fn pick_free_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    port
}

fn wait_http_200(url: &str, deadline: Duration) -> bool {
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
