//! Shared MCP / groove binary helpers extracted from
//! `tests/search_mmr_integration.rs` and `tests/search_parent_integration.rs`
//! as part of feature-34 / F-55. Used by integration tests that spawn the
//! groove binary, perform an MCP HTTP handshake, and issue `tools/call`
//! requests for the `search` tool.

use std::path::{Path, PathBuf};
use std::process::{Child, ChildStderr, Command, Stdio};
use std::sync::mpsc::Receiver;
use std::thread;
use std::time::{Duration, Instant};

/// Locate the groove binary under test. Cargo sets `CARGO_BIN_EXE_<name>`
/// for integration tests automatically.
pub fn grooveseek_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_groove"))
}

/// Poll `<url>` until 200 or `deadline` expires.
///
/// **TODO (feature-34 / F-55, Windows compatibility)**: `curl -o /dev/null`
/// uses the POSIX null-device path; the formal cross-platform spelling is
/// `-o nul`. Windows `curl` (Win10+) treats unknown device paths as regular
/// files, so it still works — it opens the path with O_WRONLY and writes the
/// body away. Since AU-09 these `#[ignore]` tests do run on the nightly
/// `windows-latest` leg and pass, which confirms the behaviour empirically
/// rather than leaving it latent. The cosmetic `-o nul` fix is still open.
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

/// What the reader thread on the server's stderr reports back.
pub enum Ready {
    /// The address the OS actually assigned, from `listening on <addr>`.
    Addr(String),
    /// The file watcher finished arming, from `watcher: watching <path>`.
    Armed,
}

/// Drain a spawned server's stderr on its own thread and report what a caller
/// can wait for.
///
/// The two jobs are one job. The assigned address exists only here — the
/// server prints it after binding — and a captured pipe nobody empties
/// eventually blocks the process writing to it, so anything that wants the
/// address gets the draining for free and anything that wants the draining
/// needs to read anyway.
///
/// Shared rather than copied because a second parser answers a different
/// question the moment the server's wording changes: the copy's caller would
/// then wait out its timeout and report a failure for a server that started
/// correctly.
pub fn drain_stderr(stderr: ChildStderr) -> Receiver<Ready> {
    drain_stderr_keeping(stderr, StderrLog::default()).0
}

/// Every line the server wrote to stderr, in order, readable while it runs.
///
/// The drain already reads all of them and threw them away; a test that wants
/// to assert on a **warning** needs them kept. Cheap to add here and impossible
/// to add later — a second reader on the same pipe would take lines away from
/// the first, and the first is what supplies the bound address.
#[derive(Clone, Default)]
pub struct StderrLog(std::sync::Arc<std::sync::Mutex<Vec<String>>>);

impl StderrLog {
    /// A copy of what has been read so far.
    pub fn lines(&self) -> Vec<String> {
        self.0.lock().expect("stderr log is not poisoned").clone()
    }

    /// Whether any line so far contains `needle`.
    ///
    /// Startup warnings are written **before** the server binds, so anything a
    /// caller reaches through `spawn_mcp_server` has already been read by the
    /// time `/healthz` answers. No polling, no window.
    pub fn contains(&self, needle: &str) -> bool {
        self.lines().iter().any(|l| l.contains(needle))
    }
}

/// [`drain_stderr`], keeping the lines in `log` as they go past.
pub fn drain_stderr_keeping(stderr: ChildStderr, log: StderrLog) -> (Receiver<Ready>, StderrLog) {
    let (tx, rx) = std::sync::mpsc::channel::<Ready>();
    let sink = log.clone();
    thread::spawn(move || {
        use std::io::BufRead;
        let (mut said_addr, mut said_armed) = (false, false);
        // The loop outlives both sends on purpose: it keeps the pipe empty for
        // the life of the process, which is the other half of why it exists.
        for line in std::io::BufReader::new(stderr)
            .lines()
            .map_while(Result::ok)
        {
            if !said_addr && let Some((_, rest)) = line.split_once("listening on ") {
                said_addr = true;
                let _ = tx.send(Ready::Addr(rest.trim().trim_end_matches(')').to_string()));
            }
            if !said_armed && line.contains("watcher: watching ") {
                said_armed = true;
                let _ = tx.send(Ready::Armed);
            }
            if let Ok(mut held) = sink.0.lock() {
                held.push(line);
            }
        }
    });
    (rx, log)
}

/// Start `groove serve` on an OS-assigned port and wait until it is usable.
///
/// **The server binds; nothing here picks a port for it.** The helpers used to
/// bind `127.0.0.1:0`, read the number, drop the listener, and pass that number
/// on the command line — a window in which any other test starting a server
/// could take it. A dozen of these run in parallel inside one test binary.
///
/// Draining stderr is not incidental either. It has to be read from the moment
/// the child starts, because that is where the assigned address is announced,
/// and a pipe nobody empties eventually blocks the process writing to it. The
/// old spawners captured stderr and read none of it, or started reading only
/// after `/healthz` answered — which leaves the whole startup window unread.
///
/// `watch` decides whether the file watcher runs: `spawn_mcp_server` freezes
/// the index for deterministic search assertions, `spawn_mcp_server_with_watch`
/// exercises the real-disk event pipeline.
fn spawn_serve(kb_path: &Path, config_path: &Path, watch: bool) -> (ServerGuard, String) {
    let bin = grooveseek_bin();
    assert!(
        bin.exists(),
        "binary not found at {} — run `cargo build` first",
        bin.display()
    );

    let mut args = vec![
        "--config",
        config_path.to_str().unwrap(),
        "serve",
        "--kb-path",
        kb_path.to_str().unwrap(),
        "--transport",
        "http",
        "--bind",
        "127.0.0.1:0",
    ];
    if !watch {
        args.push("--no-watch");
    }

    let child = Command::new(&bin)
        .args(&args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn groove serve");

    let mut guard = ServerGuard {
        child: Some(child),
        stderr: StderrLog::default(),
    };
    let stderr = guard
        .child
        .as_mut()
        .expect("child is present")
        .stderr
        .take()
        .expect("stderr was piped");

    let (rx, _) = drain_stderr_keeping(stderr, guard.stderr.clone());

    // 60 s upper bound: covers a first-time model download on a cold cache.
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut addr: Option<String> = None;
    let mut armed = !watch;
    while addr.is_none() || !armed {
        match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(Ready::Addr(a)) => addr = Some(a),
            Ok(Ready::Armed) => armed = true,
            Err(_) => panic!(
                "server did not become ready within 60s (address: {}, watcher armed: {armed}); \
                 looking for 'listening on ' and 'watcher: watching ' on stderr",
                if addr.is_some() { "yes" } else { "no" }
            ),
        }
    }
    let base = format!("http://{}", addr.expect("the loop exits with an address"));

    if !wait_http_200(&format!("{base}/healthz"), Duration::from_secs(60)) {
        panic!("/healthz did not return 200 within 60s on {base} — server failed to start");
    }
    (guard, base)
}

/// Spawn `groove serve --transport http --no-watch` and wait for `/healthz`.
///
/// Returns the guard and the base URL the OS assigned. The watcher is off, so
/// the index state is frozen and search assertions stay deterministic.
pub fn spawn_mcp_server(kb_path: &Path, config_path: &Path) -> (ServerGuard, String) {
    spawn_serve(kb_path, config_path, false)
}

/// Same as [`spawn_mcp_server`] but **with** the watcher
/// (= `notify-debouncer-full` -> `run_watch_loop`) running.
///
/// Used by F-57 `watcher_e2e`, which exercises the real-disk file event
/// pipeline. Everything else should keep using [`spawn_mcp_server`].
///
/// (AU-55) `/healthz` says the *server* is up. The watcher arms later, on its
/// own thread, and a test that starts editing files in between can lose the
/// event outright — the debouncer either saw it or it did not, and no amount of
/// polling afterwards recovers it. That is why the wait here used to be a flat
/// `sleep(2000)`: there was no signal to wait for. There is one now, printed
/// the moment `debouncer.watch()` succeeds, and [`spawn_serve`] waits for it.
pub fn spawn_mcp_server_with_watch(kb_path: &Path, config_path: &Path) -> (ServerGuard, String) {
    spawn_serve(kb_path, config_path, true)
}

/// Stand up a one-document knowledge base with the given `groove.toml`, and
/// serve it.
///
/// The layout is returned **first** so it drops **last**: locals drop in
/// reverse declaration order, and its `Drop` removes a directory the server
/// still has a database open under (codex P2 round 1 on PR #160).
///
/// `config_body` is written to a file passed with `--config`, which is not the
/// same as leaving it where discovery finds it. A discovered `groove.toml` has
/// its transport keys stripped when the directory is untrusted — precisely so
/// that a file beside the binary cannot turn Origin validation off — so a test
/// of those keys has to hand the file over explicitly.
///
/// Shared because both files that need it need the same thing, and the copy
/// would be the kind that agrees until one of them is taught something.
pub fn spawn_with_config(
    prefix: &str,
    config_body: &str,
) -> (super::temp::TempKbLayout, ServerGuard, String) {
    let layout = super::temp::TempKbLayout::new(prefix);
    layout.write(
        "note.md",
        "---\ntitle: Note\n---\n\n## body\n\nOne document so the knowledge base is not empty.\n",
    );
    let cfg = layout.root().join("groove.toml");
    std::fs::write(&cfg, config_body).expect("write groove.toml");
    let (guard, base) = spawn_mcp_server(layout.kb(), &cfg);
    (layout, guard, base)
}

/// RAII handle for the spawned MCP server child. Kills + reaps on Drop so
/// a panicking test does not orphan the server process.
pub struct ServerGuard {
    child: Option<Child>,
    stderr: StderrLog,
}

impl ServerGuard {
    /// What the server has written to stderr so far.
    ///
    /// Startup warnings are all written before it binds, so by the time a
    /// caller holds this guard they are already here.
    pub fn stderr(&self) -> &StderrLog {
        &self.stderr
    }
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

/// Run `groove index` against the given KB so the SQLite + vec index is
/// populated before we spawn the server. Uses BGE-small for speed.
pub fn build_index(kb_path: &Path) {
    let bin = grooveseek_bin();
    let st = Command::new(&bin)
        .args([
            "index",
            "--kb-path",
            kb_path.to_str().unwrap(),
            "--model",
            "bge-small-en-v1.5",
        ])
        .status()
        .expect("groove index");
    assert!(st.success(), "groove index failed");
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
