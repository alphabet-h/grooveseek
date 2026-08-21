//! HTTP Streamable transport integration test.
//!
//! `#[ignore]` 前提: embedder のモデル DL (BGE-small ~130 MB 以上) が必要で
//! 通常の `cargo test` には載せない。明示的に
//! `cargo test --test http_transport -- --ignored` で実行する。
//!
//! 検証内容:
//! - `groove serve --transport http` を ephemeral port で spawn
//! - `/healthz` が 200 "ok" を返す
//! - `/mcp` に JSON-RPC initialize を POST して 200 を返す

mod common;

use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

/// 手動 smoke test の自動化版。実バイナリ (`target/release/groove.exe` or
/// `target/debug/groove`) が存在することを前提にし、ephemeral port で起動して
/// `GET /healthz` と `POST /mcp` を叩く。
///
/// `FASTEMBED_CACHE_DIR` が事前に設定されていない環境 (CI 等) では
/// embedding モデルの初回 DL が走るため、必要に応じて skip する。
#[test]
#[ignore]
fn test_http_serve_healthz_and_initialize() {
    let bin = grooveseek_bin();
    assert!(
        bin.exists(),
        "binary not found at {}. Run `cargo build` first.",
        bin.display()
    );

    // Temporary KB directory with 1 markdown file + index it first.
    let kb_dir = tempdir("groove-http-it");
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
        .expect("groove index failed to spawn");
    assert!(
        out.status.success(),
        "groove index failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // Spawn the HTTP server. The OS assigns the port and the server reports
    // it; this test used to bind `127.0.0.1:0` itself, read the number, drop
    // the listener and pass that number on the command line, which leaves a
    // window for anything else starting a server to take it first.
    let mut child = Command::new(&bin)
        .args([
            "serve",
            "--kb-path",
            kb_dir.join("knowledge-base").to_str().unwrap(),
            "--transport",
            "http",
            "--bind",
            "127.0.0.1:0",
            "--no-watch",
        ])
        // The same deterministic filter `common::mcp`'s spawner sets, for the
        // same reason: `main` builds it from `RUST_LOG` and a runner carrying
        // one decides what this server logs. Nothing here reads a warning
        // today — `listening on` is `eprintln!` and unaffected — so this is
        // consistency rather than a fix, and the note is here so the next
        // person does not have to measure it again.
        .env("RUST_LOG", "info")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("groove serve failed to spawn");

    // `common::mcp::drain_stderr` is how the address arrives, and it also keeps
    // the pipe from filling: both pipes are captured, and one nobody empties
    // eventually blocks the process writing to it. Shared with the other
    // spawners rather than copied, so a change to the server's wording cannot
    // leave this test waiting out its timeout for a server that started fine.
    let stderr = child.stderr.take().expect("stderr was piped");
    let rx = common::mcp::drain_stderr(stderr);
    let addr = match rx.recv_timeout(Duration::from_secs(60)) {
        Ok(common::mcp::Ready::Addr(addr)) => addr,
        // This server runs `--no-watch`, so nothing else is ever reported.
        Ok(common::mcp::Ready::Armed) | Err(_) => {
            let _ = child.kill();
            panic!(
                "server did not report a bound address within 60s (looking for 'listening on ')"
            );
        }
    };
    let base = format!("http://{addr}");
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

fn grooveseek_bin() -> std::path::PathBuf {
    // Workspace 化 (feature-44 PR-1) 以降、CARGO_MANIFEST_DIR は groove/ で
    // workspace target dir と一致しない。CARGO_BIN_EXE_groove は cargo が
    // test build 時に absolute path を set する built-in env var で workspace
    // 構成に追従する (Cargo 1.39+)。
    if let Ok(custom_target) = std::env::var("CARGO_TARGET_DIR") {
        let target = std::path::PathBuf::from(custom_target);
        let profile = if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        };
        #[cfg(windows)]
        let bin = target.join(profile).join("groove.exe");
        #[cfg(not(windows))]
        let bin = target.join(profile).join("groove");
        bin
    } else {
        std::path::PathBuf::from(env!("CARGO_BIN_EXE_groove"))
    }
}

fn tempdir(prefix: &str) -> std::path::PathBuf {
    // PID + nanos alone is not unique within one test binary: its tests run on
    // parallel threads of a single process.
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let pid = std::process::id();
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let d = std::env::temp_dir().join(format!("{prefix}-{pid}-{nonce}-{seq}"));
    std::fs::create_dir_all(&d).unwrap();
    d
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
