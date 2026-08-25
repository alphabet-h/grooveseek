//! How much does the lock inside `search` cost concurrent HTTP clients?
//!
//! `search_blocking` takes three `std::sync::Mutex`es in order: `embedder`
//! (released as soon as the query is embedded), then `reranker` (taken even
//! when no reranker is configured) and `db`, both held to the end of the
//! pipeline. Every HTTP session shares the same three. Feature idea D-13
//! asks whether that serialisation is a real problem for N concurrent
//! clients, and whether splitting the locks would buy anything.
//!
//! This is a **measurement**, not a guard. It starts a real `groove serve
//! --transport http`, fires N concurrent stateless `tools/call` requests at
//! `/mcp` from N OS threads released by one `Barrier`, and prints a table.
//! The verdict it prints is not asserted: what it asserts is that the
//! harness measured something (non-empty hits, no failed requests, the
//! client really ran in parallel). The nightly `--include-ignored` run
//! exercises the harness on the three-file fixture; the decision numbers
//! come from a release build against a real corpus, see "Running" below.
//!
//! # What it measures
//!
//! Three workloads that differ only in which locks they take:
//!
//! | workload   | tool                   | locks                       |
//! |------------|------------------------|-----------------------------|
//! | `search`   | `search`               | embedder, reranker, db      |
//! | `graph`    | `get_connection_graph` | db only                     |
//! | `document` | `get_document`         | none (filesystem)           |
//!
//! `document` is the control arm: it goes through the same HTTP, rmcp and
//! `spawn_blocking` path and takes nothing, so what it shows at N=8 is
//! transport cost, not lock waiting.
//!
//! Beyond the table, three discriminators decide the D-13 question, because
//! the table alone cannot tell "serialised and CPU-bound" from "serialised
//! with idle cores" -- both show p50 growing with N and a flat qps:
//!
//! 1. **E and D**, measured in-process: the cost of one query embedding and
//!    the cost of one hybrid candidate fetch. The current pipeline overlaps
//!    the two (the embedder lock is released before the db lock is taken),
//!    so its throughput ceiling is `1 / max(E, D_full)`; a read-only
//!    connection pool of k would move it to `1 / max(E, D_full / k)`. If
//!    `E >= D_full`, no lock refactor can raise throughput on this corpus.
//! 2. **Twin daemons**: the same corpus served by two processes, N=8 split
//!    4+4. If the combined qps is well above one daemon's N=8 qps, there
//!    was idle capacity a pool could use; if it is about the same, the
//!    machine was already saturated (fastembed runs one inference across
//!    every core).
//! 3. **CPU per request** at N=1 versus N=8, sampled from the daemon's
//!    process CPU time around each cell.
//!
//! # Running
//!
//! ```text
//! cargo test -p grooveseek --test http_lock_contention -- --ignored --nocapture
//! ```
//!
//! With no environment variables that copies `tests/fixtures/kb-bench` to a
//! temp directory, indexes it with BGE-small and measures that. For decision
//! numbers point it at a real, already-indexed corpus (the test copies the
//! tree and its `.groove.db` to a temp root first and never touches the
//! original) and build in release, because the dev profile compiles the
//! bundled sqlite-vec at `-O0` and inflates D alone:
//!
//! ```text
//! GROOVE_BENCH_KB=<kb dir>            # its parent must hold .groove.db
//! GROOVE_BENCH_CONFIG=<groove.toml>   # default: <kb>/groove.toml if present
//! GROOVE_BENCH_QUERY="..."            # default: "tokio runtime"
//! GROOVE_BENCH_OUT=<file.json>        # default: <temp>/groove-http-lock-contention-<secs>.json
//! GROOVE_BENCH_RERANK=1               # also start a --reranker daemon for two reference rows
//! GROOVE_BENCH_ALLOW_DEBUG=1          # accept a debug build for a real corpus (numbers are not decision-grade)
//! cargo test -p grooveseek --release --test http_lock_contention -- --ignored --nocapture
//! ```
//!
//! # What this cannot tell you
//!
//! - It measures one machine, one corpus at a time. D grows with the chunk
//!   count (the KNN is a brute-force scan), so the verdict is per corpus.
//! - The reranked rows are a reference, not a series: one reranked query is
//!   tens of seconds on the reranker lock, and the serialisation there is
//!   visible from the source without a benchmark.
//! - N=16 on an eight-core box oversubscribes the client threads, the tokio
//!   workers and the ONNX intra-op threads; it is printed, not interpreted.

mod common;

use common::mcp::{build_index, spawn_mcp_server, spawn_mcp_server_with_args};
use common::temp::{TempKbLayout, TempRoot};
use grooveseek::db::{Database, FusionParams, SearchFilters};
use grooveseek::embedder::{Embedder, ModelChoice};
use serde_json::{Value, json};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

// ---------------------------------------------------------------------------
// Corpus staging
// ---------------------------------------------------------------------------

/// What the test serves: a temp copy it owns, plus the config to pass.
///
/// The original corpus is never opened by the daemon. `build_index` is
/// pinned to BGE-small, and pointing it at a BGE-M3 index would wipe that
/// index (the model switch resets the database), so a corpus that comes
/// from the environment is copied with its database and **never indexed**.
struct Staged {
    _hold: Hold,
    kb: PathBuf,
    db: PathBuf,
    config: PathBuf,
    label: String,
}

/// Owns the temp tree so it is removed when the `Staged` drops. Never read,
/// only dropped.
#[allow(dead_code)]
enum Hold {
    Layout(TempKbLayout),
    Root(TempRoot),
}

impl Staged {
    /// A second, independent copy of this staged corpus (for the twin
    /// daemon). Copies the already-indexed tree, so the fixture is not
    /// embedded twice.
    fn twin(&self) -> Staged {
        let root = TempRoot::new("groove-lock-contention-twin");
        let kb = root.path().join("kb");
        copy_tree(&self.kb, &kb);
        let db = root.path().join(".groove.db");
        copy_db_files(&self.db, &db);
        Staged {
            kb,
            db,
            config: self.config.clone(),
            label: format!("{} (twin copy)", self.label),
            _hold: Hold::Root(root),
        }
    }
}

fn stage_corpus() -> Staged {
    match std::env::var("GROOVE_BENCH_KB") {
        Ok(src) if !src.trim().is_empty() => stage_from_env(Path::new(src.trim())),
        _ => stage_fixture(),
    }
}

fn stage_fixture() -> Staged {
    let src = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("kb-bench");
    let layout = TempKbLayout::new("groove-lock-contention");
    let mut copied = 0usize;
    for entry in std::fs::read_dir(&src).expect("read tests/fixtures/kb-bench") {
        let entry = entry.expect("fixture dir entry");
        if entry.path().extension().and_then(|e| e.to_str()) == Some("md") {
            std::fs::copy(entry.path(), layout.kb().join(entry.file_name()))
                .expect("copy fixture file");
            copied += 1;
        }
    }
    assert!(copied > 0, "tests/fixtures/kb-bench holds no .md files");
    build_index(layout.kb());
    let config = layout.root().join("groove.toml");
    std::fs::write(&config, "[watch]\nenabled = false\n").expect("write groove.toml");
    let db = grooveseek::resolve_db_path(layout.kb());
    assert!(db.exists(), "build_index left no database at {}", db.display());
    Staged {
        kb: layout.kb().to_path_buf(),
        db,
        config,
        label: format!("tests/fixtures/kb-bench ({copied} files, BGE-small)"),
        _hold: Hold::Layout(layout),
    }
}

fn stage_from_env(src_kb: &Path) -> Staged {
    assert!(
        src_kb.is_dir(),
        "GROOVE_BENCH_KB={} is not a directory",
        src_kb.display()
    );
    let src_db = grooveseek::resolve_db_path(src_kb);
    assert!(
        src_db.exists(),
        "GROOVE_BENCH_KB is set but {} does not exist. This test never indexes a real corpus \
         (the helper's model would replace the index); run \
         `groove index --kb-path {} --config <groove.toml>` first",
        src_db.display(),
        src_kb.display()
    );
    let src_config = match std::env::var("GROOVE_BENCH_CONFIG") {
        Ok(c) if !c.trim().is_empty() => PathBuf::from(c.trim()),
        _ => src_kb.join("groove.toml"),
    };
    let root = TempRoot::new("groove-lock-contention");
    let kb = root.path().join("kb");
    copy_tree(src_kb, &kb);
    let db = root.path().join(".groove.db");
    copy_db_files(&src_db, &db);
    let config = if src_config.is_file() {
        src_config
    } else {
        let c = root.path().join("groove.toml");
        std::fs::write(&c, "[watch]\nenabled = false\n").expect("write groove.toml");
        c
    };
    Staged {
        kb,
        db,
        config,
        label: format!("copy of {}", src_kb.display()),
        _hold: Hold::Root(root),
    }
}

fn copy_tree(src: &Path, dst: &Path) {
    std::fs::create_dir_all(dst).expect("create copy root");
    for entry in std::fs::read_dir(src).unwrap_or_else(|e| panic!("read {}: {e}", src.display())) {
        let entry = entry.expect("dir entry");
        let from = entry.path();
        let to = dst.join(entry.file_name());
        let ty = entry.file_type().expect("file type");
        if ty.is_dir() {
            copy_tree(&from, &to);
        } else if ty.is_file() {
            std::fs::copy(&from, &to)
                .unwrap_or_else(|e| panic!("copy {} -> {}: {e}", from.display(), to.display()));
        }
    }
}

/// Copy `.groove.db` and, if present, its `-wal` sidecar. The `-shm` file
/// is per-process state and is rebuilt by whoever opens the copy.
fn copy_db_files(src_db: &Path, dst_db: &Path) {
    std::fs::copy(src_db, dst_db)
        .unwrap_or_else(|e| panic!("copy {} -> {}: {e}", src_db.display(), dst_db.display()));
    let wal = PathBuf::from(format!("{}-wal", src_db.display()));
    if wal.exists() {
        std::fs::copy(&wal, PathBuf::from(format!("{}-wal", dst_db.display())))
            .unwrap_or_else(|e| panic!("copy {}: {e}", wal.display()));
    }
}

// ---------------------------------------------------------------------------
// Workloads
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Workload {
    Search,
    Graph,
    Document,
    SearchRerank,
}

impl Workload {
    fn name(self) -> &'static str {
        match self {
            Workload::Search => "search",
            Workload::Graph => "graph",
            Workload::Document => "document",
            Workload::SearchRerank => "search_rerank",
        }
    }

    fn tool(self) -> &'static str {
        match self {
            Workload::Search | Workload::SearchRerank => "search",
            Workload::Graph => "get_connection_graph",
            Workload::Document => "get_document",
        }
    }

    fn arguments(self, query: &str, doc: &str) -> Value {
        match self {
            Workload::Search => json!({ "query": query, "limit": 5, "rerank": false }),
            Workload::SearchRerank => json!({ "query": query, "limit": 5, "rerank": true }),
            // Small enough to sit in the same order of magnitude as one
            // search: one centroid seed, at most a handful of KNN calls.
            Workload::Graph => json!({
                "start": doc,
                "depth": 1,
                "fan_out": 2,
                "max_nodes": 8,
                "max_seed_chunks": 4,
                "seed_strategy": "centroid",
                "min_similarity": 0.0
            }),
            Workload::Document => json!({ "path": doc }),
        }
    }

    /// Check the tool's inner JSON is the non-empty kind of success. Returns
    /// the server-side duration when the tool reports one.
    fn check(self, inner: &Value) -> Result<Option<u64>, String> {
        if inner.get("error").is_some() {
            return Err(format!("tool error: {}", inner["error"]));
        }
        match self {
            Workload::Search | Workload::SearchRerank => {
                let n = inner["results"].as_array().map(Vec::len).unwrap_or(0);
                if n == 0 {
                    Err("search returned 0 results".to_string())
                } else {
                    Ok(None)
                }
            }
            Workload::Graph => {
                let nodes = inner["nodes"].as_array().map(Vec::len).unwrap_or(0);
                let knn = inner["stats"]["knn_queries"].as_u64().unwrap_or(0);
                if nodes == 0 || knn == 0 {
                    Err(format!("graph returned {nodes} nodes after {knn} knn queries"))
                } else {
                    Ok(inner["stats"]["duration_ms"].as_u64())
                }
            }
            Workload::Document => {
                let len = inner["content"].as_str().map(str::len).unwrap_or(0);
                if len == 0 {
                    Err("document content is empty".to_string())
                } else {
                    Ok(None)
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// A hand-rolled HTTP/1.1 client
// ---------------------------------------------------------------------------
//
// No HTTP client crate is a dependency of this crate and the tests keep it
// that way; `curl` per request would spawn a process per sample, and on
// Windows process start (50-100 ms) is longer than the request being timed,
// so N parallel curls serialise themselves. One thread per client and a
// blocking socket is the smallest thing that measures the server and not
// the client.

/// One request's timings and outcome. Milliseconds from the moment the
/// thread was released by the barrier.
#[derive(Clone, Debug)]
struct Sample {
    first_data_ms: f64,
    eof_ms: f64,
    ok: bool,
    why: Option<String>,
    srv_ms: Option<u64>,
}

fn stateless_tools_call(tool: &str, arguments: &Value) -> Vec<u8> {
    serde_json::to_vec(&json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": {
            "name": tool,
            "arguments": arguments,
            "_meta": {
                "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                "io.modelcontextprotocol/clientCapabilities": {}
            }
        }
    }))
    .expect("serialise request")
}

fn request_bytes(authority: &str, tool: &str, body: &[u8]) -> Vec<u8> {
    let mut req = format!(
        "POST /mcp HTTP/1.1\r\n\
         Host: {authority}\r\n\
         Content-Type: application/json\r\n\
         Accept: application/json, text/event-stream\r\n\
         MCP-Protocol-Version: 2026-07-28\r\n\
         Mcp-Method: tools/call\r\n\
         Mcp-Name: {tool}\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\
         \r\n",
        body.len()
    )
    .into_bytes();
    req.extend_from_slice(body);
    req
}

fn connect(authority: &str, read_timeout: Duration) -> TcpStream {
    let stream = TcpStream::connect(authority)
        .unwrap_or_else(|e| panic!("connect {authority}: {e}"));
    stream.set_nodelay(true).expect("set_nodelay");
    stream
        .set_read_timeout(Some(read_timeout))
        .expect("set_read_timeout");
    stream
}

/// A parsed HTTP response: status, lower-cased headers, decoded body.
struct Response {
    status: u16,
    content_type: String,
    body: Vec<u8>,
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || hay.len() < needle.len() {
        return None;
    }
    hay.windows(needle.len()).position(|w| w == needle)
}

fn parse_response(raw: &[u8]) -> Result<Response, String> {
    let head_end = find(raw, b"\r\n\r\n").ok_or("no end of headers in response")?;
    let head = String::from_utf8_lossy(&raw[..head_end]).to_string();
    let mut lines = head.split("\r\n");
    let status_line = lines.next().unwrap_or_default();
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| format!("bad status line: {status_line}"))?;
    let mut chunked = false;
    let mut content_length: Option<usize> = None;
    let mut content_type = String::new();
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let name = name.trim().to_ascii_lowercase();
        let value = value.trim();
        match name.as_str() {
            "transfer-encoding" => chunked = value.to_ascii_lowercase().contains("chunked"),
            "content-length" => content_length = value.parse().ok(),
            "content-type" => content_type = value.to_ascii_lowercase(),
            _ => {}
        }
    }
    let raw_body = &raw[head_end + 4..];
    let body = if chunked {
        dechunk(raw_body)?
    } else if let Some(n) = content_length {
        if raw_body.len() < n {
            return Err(format!(
                "body shorter than content-length ({} < {n})",
                raw_body.len()
            ));
        }
        raw_body[..n].to_vec()
    } else {
        raw_body.to_vec()
    };
    Ok(Response {
        status,
        content_type,
        body,
    })
}

/// Undo HTTP/1.1 chunked transfer encoding. A truncated body is an error,
/// not a shorter body.
fn dechunk(raw: &[u8]) -> Result<Vec<u8>, String> {
    let mut out = Vec::with_capacity(raw.len());
    let mut pos = 0usize;
    loop {
        let line_end = find(&raw[pos..], b"\r\n").ok_or("chunk size line not terminated")? + pos;
        let size_text = String::from_utf8_lossy(&raw[pos..line_end]);
        let size_hex = size_text.split(';').next().unwrap_or("").trim();
        let size = usize::from_str_radix(size_hex, 16)
            .map_err(|e| format!("bad chunk size {size_hex:?}: {e}"))?;
        pos = line_end + 2;
        if size == 0 {
            return Ok(out);
        }
        if raw.len() < pos + size + 2 {
            return Err(format!(
                "chunk of {size} bytes truncated at {} of {}",
                raw.len(),
                pos + size + 2
            ));
        }
        out.extend_from_slice(&raw[pos..pos + size]);
        pos += size + 2;
    }
}

/// The JSON-RPC envelope of a response: the first `data:` event carrying a
/// `result` or `error` for SSE, the whole body otherwise.
fn envelope(resp: &Response) -> Result<Value, String> {
    let text = String::from_utf8_lossy(&resp.body);
    if resp.content_type.contains("text/event-stream") {
        for line in text.lines() {
            let Some(payload) = line.strip_prefix("data:") else {
                continue;
            };
            let payload = payload.trim();
            if payload.is_empty() {
                continue;
            }
            if let Ok(v) = serde_json::from_str::<Value>(payload)
                && (v.get("result").is_some() || v.get("error").is_some())
            {
                return Ok(v);
            }
        }
        Err(format!(
            "no data: event with a result or error in SSE body ({} bytes)",
            resp.body.len()
        ))
    } else {
        serde_json::from_str::<Value>(text.trim())
            .map_err(|e| format!("body is not JSON ({e}): {}", text.chars().take(200).collect::<String>()))
    }
}

/// Send one already-built request on a connected socket and read to EOF.
/// `t0` is the caller's clock: it starts before the first byte is written.
fn exchange(stream: &mut TcpStream, req: &[u8], t0: Instant, workload: Workload) -> Sample {
    if let Err(e) = stream.write_all(req) {
        return Sample {
            first_data_ms: 0.0,
            eof_ms: t0.elapsed().as_secs_f64() * 1000.0,
            ok: false,
            why: Some(format!("write: {e}")),
            srv_ms: None,
        };
    }
    let mut raw: Vec<u8> = Vec::with_capacity(16 * 1024);
    let mut chunk = [0u8; 16 * 1024];
    let mut first_data: Option<f64> = None;
    let mut timed_out_after_data = false;
    loop {
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                let scan_from = raw.len().saturating_sub(8);
                raw.extend_from_slice(&chunk[..n]);
                if first_data.is_none() {
                    if find(&raw[scan_from..], b"data:").is_some() {
                        first_data = Some(t0.elapsed().as_secs_f64() * 1000.0);
                    } else if find(&raw[scan_from..], b"\r\n\r\n").is_some()
                        && !String::from_utf8_lossy(&raw).to_ascii_lowercase().contains("text/event-stream")
                        && find(&raw, b"\r\n\r\n").is_some()
                    {
                        // Not a stream: the body arrives whole, count it when
                        // the connection closes.
                    }
                }
            }
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                if first_data.is_some() {
                    timed_out_after_data = true;
                    break;
                }
                return Sample {
                    first_data_ms: 0.0,
                    eof_ms: t0.elapsed().as_secs_f64() * 1000.0,
                    ok: false,
                    why: Some("read timed out before any response data".to_string()),
                    srv_ms: None,
                };
            }
            Err(e) => {
                return Sample {
                    first_data_ms: 0.0,
                    eof_ms: t0.elapsed().as_secs_f64() * 1000.0,
                    ok: false,
                    why: Some(format!("read: {e}")),
                    srv_ms: None,
                };
            }
        }
    }
    let eof_ms = t0.elapsed().as_secs_f64() * 1000.0;
    let first_data_ms = first_data.unwrap_or(eof_ms);
    let outcome = parse_response(&raw).and_then(|resp| {
        if resp.status != 200 {
            return Err(format!(
                "HTTP {}: {}",
                resp.status,
                String::from_utf8_lossy(&resp.body).chars().take(200).collect::<String>()
            ));
        }
        let env = envelope(&resp)?;
        if let Some(err) = env.get("error") {
            return Err(format!("JSON-RPC error: {err}"));
        }
        let result = env.get("result").ok_or("envelope has no result")?;
        if result.get("isError").and_then(Value::as_bool) == Some(true) {
            return Err(format!("isError: {result}"));
        }
        let text = result
            .pointer("/content/0/text")
            .and_then(Value::as_str)
            .ok_or("result.content[0].text missing")?;
        let inner: Value =
            serde_json::from_str(text).map_err(|e| format!("inner text is not JSON: {e}"))?;
        workload.check(&inner)
    });
    match outcome {
        Ok(srv_ms) => Sample {
            first_data_ms,
            eof_ms,
            ok: true,
            why: if timed_out_after_data {
                Some("stream stayed open after the result; eof is the read timeout".to_string())
            } else {
                None
            },
            srv_ms,
        },
        Err(why) => Sample {
            first_data_ms,
            eof_ms,
            ok: false,
            why: Some(why),
            srv_ms: None,
        },
    }
}

/// One request, sequentially, from the calling thread. Used for warm-up and
/// for the admin status read.
fn one_call(authority: &str, workload: Workload, query: &str, doc: &str, timeout: Duration) -> Sample {
    let body = stateless_tools_call(workload.tool(), &workload.arguments(query, doc));
    let req = request_bytes(authority, workload.tool(), &body);
    let mut stream = connect(authority, timeout);
    let t0 = Instant::now();
    exchange(&mut stream, &req, t0, workload)
}

/// The inner JSON of one call, panicking on failure. Used to pick the
/// document the graph and document workloads will use.
fn one_call_inner(authority: &str, workload: Workload, query: &str, doc: &str) -> Value {
    let body = stateless_tools_call(workload.tool(), &workload.arguments(query, doc));
    let req = request_bytes(authority, workload.tool(), &body);
    let mut stream = connect(authority, Duration::from_secs(120));
    if let Err(e) = stream.write_all(&req) {
        panic!("write: {e}");
    }
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).expect("read response");
    let resp = parse_response(&raw).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(
        resp.status,
        200,
        "HTTP {} from {}: {}",
        resp.status,
        workload.tool(),
        String::from_utf8_lossy(&resp.body)
    );
    let env = envelope(&resp).unwrap_or_else(|e| panic!("{e}"));
    let text = env
        .pointer("/result/content/0/text")
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("no result.content[0].text in {env}"));
    serde_json::from_str(text).unwrap_or_else(|e| panic!("inner text is not JSON ({e}): {text}"))
}

fn admin_status(authority: &str) -> Option<Value> {
    let req = format!(
        "GET /api/admin/status HTTP/1.1\r\nHost: {authority}\r\nAccept: application/json\r\nConnection: close\r\n\r\n"
    );
    let mut stream = connect(authority, Duration::from_secs(10));
    stream.write_all(req.as_bytes()).ok()?;
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).ok()?;
    let resp = parse_response(&raw).ok()?;
    if resp.status != 200 {
        return None;
    }
    serde_json::from_slice(&resp.body).ok()
}

// ---------------------------------------------------------------------------
// Load generation
// ---------------------------------------------------------------------------

struct Cell {
    workload: Workload,
    n: usize,
    pass: &'static str,
    samples: Vec<Sample>,
    /// Sum of per-round wall clock, barrier release to last EOF.
    wall_ms: f64,
    cpu_ms: Option<f64>,
}

/// Where the load goes and what every request carries.
struct Site<'a> {
    /// Thread `i` talks to `authorities[i % len]`, so a twin cell is the
    /// same function with two addresses.
    authorities: Vec<String>,
    query: &'a str,
    doc: &'a str,
    timeout: Duration,
    /// Daemon to sample CPU time from around each cell; `None` skips it.
    pid: Option<u32>,
}

impl Site<'_> {
    fn cpu_delta(&self, before: Option<f64>) -> Option<f64> {
        match (before, self.pid.and_then(cpu_ms)) {
            (Some(a), Some(b)) => Some((b - a).max(0.0)),
            _ => None,
        }
    }
}

/// `rounds` rounds of `n` concurrent requests, each round released by one
/// barrier.
///
/// The clock: every thread connects **before** the barrier and takes `t0`
/// the moment `wait` returns, before writing a byte. A clock started inside
/// the server's queue would exclude the queueing it exists to measure.
fn run_cell(site: &Site<'_>, workload: Workload, n: usize, rounds: usize, pass: &'static str) -> Cell {
    let cpu_before = site.pid.and_then(cpu_ms);
    let mut samples = Vec::with_capacity(n * rounds);
    let mut wall_ms = 0.0;
    let body = stateless_tools_call(workload.tool(), &workload.arguments(site.query, site.doc));
    for _ in 0..rounds {
        let barrier = Arc::new(Barrier::new(n + 1));
        let mut handles = Vec::with_capacity(n);
        for i in 0..n {
            let authority = site.authorities[i % site.authorities.len()].clone();
            let req = request_bytes(&authority, workload.tool(), &body);
            let barrier = Arc::clone(&barrier);
            let timeout = site.timeout;
            handles.push(thread::spawn(move || {
                let mut stream = connect(&authority, timeout);
                barrier.wait();
                let t0 = Instant::now();
                exchange(&mut stream, &req, t0, workload)
            }));
        }
        barrier.wait();
        let round_t0 = Instant::now();
        for h in handles {
            samples.push(h.join().expect("client thread panicked"));
        }
        wall_ms += round_t0.elapsed().as_secs_f64() * 1000.0;
    }
    Cell {
        workload,
        n,
        pass,
        samples,
        wall_ms,
        cpu_ms: site.cpu_delta(cpu_before),
    }
}

/// `n` threads each issuing requests back to back for `duration`. Steady
/// state throughput, as opposed to the barrier's first-arrival story.
fn run_closed_loop(site: &Site<'_>, workload: Workload, n: usize, duration: Duration) -> Cell {
    let cpu_before = site.pid.and_then(cpu_ms);
    let body = stateless_tools_call(workload.tool(), &workload.arguments(site.query, site.doc));
    let barrier = Arc::new(Barrier::new(n + 1));
    let mut handles = Vec::with_capacity(n);
    for i in 0..n {
        let authority = site.authorities[i % site.authorities.len()].clone();
        let req = request_bytes(&authority, workload.tool(), &body);
        let barrier = Arc::clone(&barrier);
        let timeout = site.timeout;
        handles.push(thread::spawn(move || {
            let mut out = Vec::new();
            barrier.wait();
            let deadline = Instant::now() + duration;
            while Instant::now() < deadline {
                let mut stream = connect(&authority, timeout);
                let t0 = Instant::now();
                out.push(exchange(&mut stream, &req, t0, workload));
            }
            out
        }));
    }
    barrier.wait();
    let t0 = Instant::now();
    let mut samples = Vec::new();
    for h in handles {
        samples.extend(h.join().expect("client thread panicked"));
    }
    let wall_ms = t0.elapsed().as_secs_f64() * 1000.0;
    Cell {
        workload,
        n,
        pass: "loop",
        samples,
        wall_ms,
        cpu_ms: site.cpu_delta(cpu_before),
    }
}

// ---------------------------------------------------------------------------
// CPU time of the daemon
// ---------------------------------------------------------------------------

/// Total CPU time (user + system) of process `pid` in milliseconds, read
/// with what the platform ships. `None` when it cannot be read; the table
/// then says `n/a` rather than the test failing over an instrument.
#[cfg(windows)]
fn cpu_ms(pid: u32) -> Option<f64> {
    let out = Command::new("powershell")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            &format!("(Get-Process -Id {pid}).TotalProcessorTime.TotalMilliseconds"),
        ])
        .output()
        .ok()?;
    String::from_utf8_lossy(&out.stdout).trim().parse::<f64>().ok()
}

#[cfg(target_os = "linux")]
fn cpu_ms(pid: u32) -> Option<f64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let after = &stat[stat.rfind(')')? + 1..];
    let fields: Vec<&str> = after.split_whitespace().collect();
    // utime and stime are fields 14 and 15 of the whole line; after the
    // `)` the state is field 3, so they sit at indexes 11 and 12.
    let utime: f64 = fields.get(11)?.parse().ok()?;
    let stime: f64 = fields.get(12)?.parse().ok()?;
    // CLK_TCK is 100 on every Linux this test is expected to run on.
    Some((utime + stime) * 10.0)
}

#[cfg(target_os = "macos")]
fn cpu_ms(pid: u32) -> Option<f64> {
    let out = Command::new("ps")
        .args(["-o", "cputime=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
    // [hh:]mm:ss.cc
    let mut total = 0.0f64;
    for part in text.split(':') {
        total = total * 60.0 + part.trim().parse::<f64>().ok()?;
    }
    Some(total * 1000.0)
}

#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
fn cpu_ms(_pid: u32) -> Option<f64> {
    None
}

// ---------------------------------------------------------------------------
// Statistics and reporting
// ---------------------------------------------------------------------------

fn sorted_latencies(samples: &[Sample]) -> Vec<f64> {
    let mut v: Vec<f64> = samples.iter().map(|s| s.first_data_ms).collect();
    v.sort_by(|a, b| a.partial_cmp(b).expect("finite latency"));
    v
}

/// Nearest-rank percentile of an ascending slice.
fn pct(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return f64::NAN;
    }
    let rank = ((p / 100.0) * sorted.len() as f64).ceil() as usize;
    sorted[rank.clamp(1, sorted.len()) - 1]
}

fn median(values: &mut [f64]) -> f64 {
    values.sort_by(|a, b| a.partial_cmp(b).expect("finite"));
    pct(values, 50.0)
}

impl Cell {
    fn p50(&self) -> f64 {
        pct(&sorted_latencies(&self.samples), 50.0)
    }

    fn qps(&self) -> f64 {
        if self.wall_ms <= 0.0 {
            return f64::NAN;
        }
        self.samples.len() as f64 / (self.wall_ms / 1000.0)
    }

    fn errors(&self) -> usize {
        self.samples.iter().filter(|s| !s.ok).count()
    }

    fn cpu_per_req_ms(&self) -> Option<f64> {
        self.cpu_ms.map(|c| c / self.samples.len().max(1) as f64)
    }

    /// Sum of the samples' latencies over the wall clock they took: 1.0 for
    /// requests issued one after another, up to N for N fully overlapping.
    fn overlap(&self) -> f64 {
        if self.wall_ms <= 0.0 {
            return f64::NAN;
        }
        self.samples.iter().map(|s| s.first_data_ms).sum::<f64>() / self.wall_ms
    }

    fn row(&self, cores: usize) -> String {
        let lat = sorted_latencies(&self.samples);
        let mut tails: Vec<f64> = self
            .samples
            .iter()
            .map(|s| s.eof_ms - s.first_data_ms)
            .collect();
        let tail = median(&mut tails);
        let mut srv: Vec<f64> = self.samples.iter().filter_map(|s| s.srv_ms.map(|v| v as f64)).collect();
        let srv_p50 = if srv.is_empty() {
            "-".to_string()
        } else {
            format!("{:.1}", median(&mut srv))
        };
        let short = self.wall_ms < 500.0;
        let approx = if short { "~" } else { "" };
        let (cpu, cpu_req, util) = match self.cpu_ms {
            Some(c) => {
                let cores_busy = c / self.wall_ms.max(1.0);
                (
                    format!("{approx}{c:.0}"),
                    format!("{approx}{:.1}", self.cpu_per_req_ms().unwrap_or(0.0)),
                    format!("{approx}{:.0}%", 100.0 * cores_busy / cores.max(1) as f64),
                )
            }
            None => ("n/a".to_string(), "n/a".to_string(), "n/a".to_string()),
        };
        format!(
            "{:<13} {:>3} {:<5} {:>7} {:>9.1} {:>9.1} {:>9.1} {:>8.2} {:>4} {:>4} {:>8.1} {:>8} {:>8} {:>8} {:>6}",
            self.workload.name(),
            self.n,
            self.pass,
            self.samples.len(),
            pct(&lat, 50.0),
            pct(&lat, 95.0),
            pct(&lat, 100.0),
            self.qps(),
            self.samples.len() - self.errors(),
            self.errors(),
            tail,
            srv_p50,
            cpu,
            cpu_req,
            util
        )
    }

    fn to_json(&self) -> Value {
        json!({
            "workload": self.workload.name(),
            "n": self.n,
            "pass": self.pass,
            "samples": self.samples.len(),
            "errors": self.errors(),
            "p50_ms": self.p50(),
            "qps": self.qps(),
            "wall_ms": self.wall_ms,
            "cpu_ms": self.cpu_ms,
            "first_data_ms": self.samples.iter().map(|s| s.first_data_ms).collect::<Vec<_>>(),
            "eof_ms": self.samples.iter().map(|s| s.eof_ms).collect::<Vec<_>>(),
            "srv_ms": self.samples.iter().map(|s| s.srv_ms).collect::<Vec<_>>(),
            "why": self.samples.iter().filter_map(|s| s.why.clone()).collect::<Vec<_>>(),
        })
    }
}

fn header_line() -> String {
    format!(
        "{:<13} {:>3} {:<5} {:>7} {:>9} {:>9} {:>9} {:>8} {:>4} {:>4} {:>8} {:>8} {:>8} {:>8} {:>6}",
        "workload", "N", "pass", "samples", "p50_ms", "p95_ms", "max_ms", "qps", "ok", "err",
        "tail_ms", "srv_p50", "cpu_ms", "cpu/req", "util"
    )
}

/// Merge the samples of every cell matching `(workload, n)` with a barrier
/// pass into one, so the verdict reads both mirror passes at once.
fn merged<'a>(cells: &'a [Cell], workload: Workload, n: usize) -> Option<Cell> {
    let parts: Vec<&'a Cell> = cells
        .iter()
        .filter(|c| c.workload == workload && c.n == n && c.pass != "loop")
        .collect();
    if parts.is_empty() {
        return None;
    }
    let mut samples = Vec::new();
    let mut wall_ms = 0.0;
    let mut cpu_ms = Some(0.0);
    for c in &parts {
        samples.extend(c.samples.iter().cloned());
        wall_ms += c.wall_ms;
        cpu_ms = match (cpu_ms, c.cpu_ms) {
            (Some(a), Some(b)) => Some(a + b),
            _ => None,
        };
    }
    Some(Cell {
        workload,
        n,
        pass: "both",
        samples,
        wall_ms,
        cpu_ms,
    })
}

fn version_line() -> String {
    let bin = common::mcp::grooveseek_bin();
    let v = Command::new(&bin)
        .arg("--version")
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|| "n/a".to_string());
    let describe = Command::new("git")
        .args(["-C", env!("CARGO_MANIFEST_DIR"), "describe", "--always", "--dirty"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|| "n/a".to_string());
    format!("{v} / git {describe}")
}

fn env_flag(name: &str) -> bool {
    matches!(std::env::var(name), Ok(v) if v == "1" || v.eq_ignore_ascii_case("true"))
}

fn authority_of(base: &str) -> String {
    base.strip_prefix("http://")
        .unwrap_or(base)
        .trim_end_matches('/')
        .to_string()
}

// ---------------------------------------------------------------------------
// In-process E and D
// ---------------------------------------------------------------------------

struct EmbedAndFetch {
    model: String,
    e_ms: f64,
    d_ms: f64,
    candidates: usize,
}

/// The two halves the pipeline overlaps, each timed alone: E = one query
/// embedding, D = one hybrid candidate fetch on the same database file.
/// D is a floor for what the db lock covers -- MMR, the parent retriever
/// and the size lookup run under the same lock and are not in it.
fn measure_embed_and_fetch(model: &str, db_path: &Path, query: &str) -> EmbedAndFetch {
    let choice = match model {
        "bge-m3" => ModelChoice::BgeM3,
        _ => ModelChoice::BgeSmallEnV15,
    };
    let mut embedder = Embedder::with_model(choice).expect("load the embedding model in-process");
    for _ in 0..3 {
        embedder.embed_single(query).expect("warm-up embed");
    }
    let mut e = Vec::with_capacity(20);
    let mut emb = Vec::new();
    for _ in 0..20 {
        let t0 = Instant::now();
        emb = embedder.embed_single(query).expect("embed");
        e.push(t0.elapsed().as_secs_f64() * 1000.0);
    }
    let db = Database::open(&db_path.to_string_lossy()).expect("open the copied database");
    let filters = SearchFilters {
        min_quality: 0.3,
        ..Default::default()
    };
    let mut candidates = 0usize;
    for _ in 0..3 {
        candidates = db
            .search_hybrid_candidates(query, &emb, 5, &filters, FusionParams::default())
            .expect("warm-up fetch")
            .len();
    }
    let mut d = Vec::with_capacity(20);
    for _ in 0..20 {
        let t0 = Instant::now();
        let got = db
            .search_hybrid_candidates(query, &emb, 5, &filters, FusionParams::default())
            .expect("fetch");
        d.push(t0.elapsed().as_secs_f64() * 1000.0);
        candidates = got.len();
    }
    EmbedAndFetch {
        model: choice.model_id().to_string(),
        e_ms: median(&mut e),
        d_ms: median(&mut d),
        candidates,
    }
}

// ---------------------------------------------------------------------------
// The measurement
// ---------------------------------------------------------------------------

#[test]
#[ignore = "spawns groove serve (embedding model download, TCP ports) and prints a latency table; run with --nocapture"]
fn concurrent_tools_call_latency_table() {
    let real_corpus = matches!(std::env::var("GROOVE_BENCH_KB"), Ok(v) if !v.trim().is_empty());
    if real_corpus && cfg!(debug_assertions) && !env_flag("GROOVE_BENCH_ALLOW_DEBUG") {
        panic!(
            "GROOVE_BENCH_KB is set but this is a debug build: the bundled sqlite-vec is compiled \
             at -O0 and D would be inflated on its own. Re-run with `cargo test --release ...`, or \
             set GROOVE_BENCH_ALLOW_DEBUG=1 to accept numbers that are not decision-grade"
        );
    }
    let query = std::env::var("GROOVE_BENCH_QUERY").unwrap_or_else(|_| "tokio runtime".to_string());
    let cores = thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
    let build = if cfg!(debug_assertions) { "DEBUG" } else { "release" };

    let staged = stage_corpus();
    let (guard, base) = spawn_mcp_server(&staged.kb, &staged.config);
    let authority = authority_of(&base);
    let pid = Some(guard.pid());

    let status = admin_status(&authority);
    let kb_model = status
        .as_ref()
        .and_then(|s| s["kb"]["model"].as_str())
        .unwrap_or("bge-small-en-v1.5")
        .to_string();
    let documents = status.as_ref().and_then(|s| s["kb"]["documents"].as_u64());
    let chunks = status.as_ref().and_then(|s| s["kb"]["chunks"].as_u64());

    println!();
    println!("http_lock_contention: corpus={}", staged.label);
    println!("  config={} db={}", staged.config.display(), staged.db.display());
    println!(
        "  daemon pid={} base={} build={build} cores(logical)={cores} groove={}",
        guard.pid(),
        base,
        version_line()
    );
    println!(
        "  model={kb_model} documents={} chunks={}",
        documents.map(|v| v.to_string()).unwrap_or_else(|| "n/a".to_string()),
        chunks.map(|v| v.to_string()).unwrap_or_else(|| "n/a".to_string())
    );
    if cfg!(debug_assertions) {
        println!("  DEBUG BUILD: numbers are not decision-grade");
    }

    // Warm-up, and the document every other workload will point at. A
    // search that finds nothing costs the embedding and skips the rest,
    // which is exactly the part this test is about.
    let first = one_call_inner(&authority, Workload::Search, &query, "");
    let hits = first["results"].as_array().map(Vec::len).unwrap_or(0);
    assert!(
        hits > 0,
        "the bench query returned 0 results - the numbers below would be the cost of a request \
         that finds nothing. corpus={} query={query:?}",
        staged.label
    );
    let doc = first["results"][0]["path"]
        .as_str()
        .expect("first hit has a path")
        .to_string();
    println!("  query={query:?} doc={doc:?} (first hit of the warm-up search, {hits} hits)");

    let timeout = Duration::from_secs(120);
    let mut warm = Vec::new();
    for w in [Workload::Search, Workload::Graph, Workload::Document] {
        let mut ms = Vec::new();
        for _ in 0..5 {
            let s = one_call(&authority, w, &query, &doc, timeout);
            assert!(
                s.ok,
                "warm-up {} failed: {}",
                w.name(),
                s.why.unwrap_or_default()
            );
            ms.push(format!("{:.0}", s.first_data_ms));
        }
        warm.push(format!("{} {}", w.name(), ms.join("/")));
    }
    println!("  warm-up (ms): {}", warm.join(", "));
    println!();

    // Barrier cells, in mirror order so drift shows up as a difference
    // between the two N=1 and the two N=8 cells of one workload.
    let plan: [(usize, usize, &'static str); 9] = [
        (1, 5, "a"),
        (2, 4, "a"),
        (4, 3, "a"),
        (8, 2, "a"),
        (16, 3, "a"),
        (8, 2, "b"),
        (4, 3, "b"),
        (2, 4, "b"),
        (1, 5, "b"),
    ];
    let site = Site {
        authorities: vec![authority.clone()],
        query: &query,
        doc: &doc,
        timeout,
        pid,
    };
    let mut cells: Vec<Cell> = Vec::new();
    println!("{}", header_line());
    for w in [Workload::Search, Workload::Graph, Workload::Document] {
        for (n, rounds, pass) in plan {
            let cell = run_cell(&site, w, n, rounds, pass);
            println!("{}", cell.row(cores));
            cells.push(cell);
        }
    }

    // Steady state: search, N=1 and N=8, five seconds each.
    let loop_secs = Duration::from_secs(5);
    let mut loops: Vec<Cell> = Vec::new();
    for n in [1usize, 8] {
        let cell = run_closed_loop(&site, Workload::Search, n, loop_secs);
        println!("{}", cell.row(cores));
        loops.push(cell);
    }

    // Twin: the same corpus served twice, N=8 split 4+4.
    let twin_corpus = staged.twin();
    let (twin_guard, twin_base) = spawn_mcp_server(&twin_corpus.kb, &twin_corpus.config);
    let twin_site = Site {
        authorities: vec![authority.clone(), authority_of(&twin_base)],
        query: &query,
        doc: &doc,
        timeout,
        pid: None,
    };
    let twin_cell = run_cell(&twin_site, Workload::Search, 8, 4, "twin");
    println!("{}", twin_cell.row(cores));
    let twin_single = run_cell(&site, Workload::Search, 8, 4, "solo");
    println!("{}", twin_single.row(cores));
    drop(twin_guard);
    drop(twin_corpus);

    // Reference rows with a reranker resident, only on request.
    let mut rerank_cells: Vec<Cell> = Vec::new();
    let search_n1 = merged(&cells, Workload::Search, 1).expect("search N=1 cell");
    if env_flag("GROOVE_BENCH_RERANK") {
        drop(guard);
        let (rr_guard, rr_base) =
            spawn_mcp_server_with_args(&staged.kb, &staged.config, &["--reranker", "bge-v2-m3"]);
        let rr_authority = authority_of(&rr_base);
        let rr_pid = Some(rr_guard.pid());
        let rr_timeout = Duration::from_secs(900);
        let warm = one_call(&rr_authority, Workload::Search, &query, &doc, rr_timeout);
        assert!(warm.ok, "warm-up on the reranker daemon failed: {}", warm.why.unwrap_or_default());
        let rr_site = Site {
            authorities: vec![rr_authority.clone()],
            query: &query,
            doc: &doc,
            timeout: rr_timeout,
            pid: rr_pid,
        };
        for (n, rounds) in [(1usize, 3usize), (2, 1)] {
            let cell = run_cell(&rr_site, Workload::SearchRerank, n, rounds, "ref");
            println!("{}", cell.row(cores));
            let floor = 3.0 * search_n1.p50();
            for s in &cell.samples {
                assert!(
                    s.first_data_ms >= floor,
                    "a reranked search took {:.1} ms, under 3x the unreranked p50 ({:.1} ms): \
                     the daemon did not rerank (should_rerank falls back silently when no reranker is loaded)",
                    s.first_data_ms,
                    search_n1.p50()
                );
            }
            rerank_cells.push(cell);
        }
        drop(rr_guard);
    } else {
        drop(guard);
    }

    // In-process halves, with every daemon gone.
    let ed = measure_embed_and_fetch(&kb_model, &staged.db, &query);

    // ---- assertions: the harness measured something ---------------------
    for c in cells.iter().chain(loops.iter()).chain(rerank_cells.iter()).chain([&twin_cell, &twin_single]) {
        assert_eq!(
            c.errors(),
            0,
            "{} N={} pass={} had {} failed requests; first: {}",
            c.workload.name(),
            c.n,
            c.pass,
            c.errors(),
            c.samples.iter().find_map(|s| s.why.clone()).unwrap_or_default()
        );
    }
    let search_n8 = merged(&cells, Workload::Search, 8).expect("search N=8 cell");
    let doc_n1 = merged(&cells, Workload::Document, 1).expect("document N=1 cell");
    let doc_n8 = merged(&cells, Workload::Document, 8).expect("document N=8 cell");
    let graph_n1 = merged(&cells, Workload::Graph, 1).expect("graph N=1 cell");
    let graph_n8 = merged(&cells, Workload::Graph, 8).expect("graph N=8 cell");
    assert!(
        search_n1.p50() >= 1.0,
        "search N=1 p50 is {:.3} ms; a sub-millisecond search did not embed anything",
        search_n1.p50()
    );
    if real_corpus {
        assert!(
            (30.0..=5000.0).contains(&search_n1.p50()),
            "search N=1 p50 is {:.1} ms on a real corpus; outside [30, 5000] ms means a debug build, \
             a paged-out daemon or a corpus that is not what was asked for",
            search_n1.p50()
        );
    }
    // The client really ran in parallel: the latencies of one round add up
    // to more than the round took. A sequential client scores exactly 1.0
    // here whatever the server does (probe M1: making the fan-out a loop
    // left the qps ratios within noise on 2 ms requests, so a qps ratio
    // is not the invariant; this is).
    for c in [&search_n8, &doc_n8] {
        assert!(
            c.overlap() >= 2.0,
            "{} N=8: the latencies sum to {:.2}x the wall clock; below 2.0 the eight requests \
             were not in flight together, so the load generator is not delivering what the table claims",
            c.workload.name(),
            c.overlap()
        );
    }
    let parallel_ratio = doc_n8.qps() / doc_n1.qps();

    // ---- verdict --------------------------------------------------------
    let e = ed.e_ms;
    let d_full = (search_n1.p50() - doc_n1.p50() - e).max(0.0);
    let qps_now = 1000.0 / e.max(d_full).max(0.001);
    let qps_pool8 = 1000.0 / e.max(d_full / 8.0).max(0.001);
    let twin_ratio = twin_cell.qps() / twin_single.qps();
    let cpu_ratio = match (search_n8.cpu_per_req_ms(), search_n1.cpu_per_req_ms()) {
        (Some(a), Some(b)) if b > 0.0 => Some(a / b),
        _ => None,
    };
    let d_wins = d_full > e;
    let twin_wins = twin_ratio >= 1.5;
    let verdict = if d_full <= 0.0 && e <= 0.0 {
        "inconclusive"
    } else if d_wins && twin_wins {
        "yes"
    } else {
        "no"
    };

    println!();
    println!("in-process halves ({}): E = {:.1} ms (one query embedding), D = {:.1} ms (one hybrid fetch, {} candidates; floor for the db-held span)",
        ed.model, e, ed.d_ms, ed.candidates);
    println!(
        "  D_full = search N=1 p50 {:.1} - document N=1 p50 {:.1} - E {:.1} = {:.1} ms",
        search_n1.p50(),
        doc_n1.p50(),
        e,
        d_full
    );
    println!(
        "  throughput ceiling now 1/max(E, D_full) = {qps_now:.2} qps; with a pool of 8 read connections 1/max(E, D_full/8) = {qps_pool8:.2} qps"
    );
    println!(
        "twin daemons: 4+4 {:.2} qps vs one daemon N=8 {:.2} qps -> ratio {twin_ratio:.2} (>= 1.5 means idle capacity a pool could use)",
        twin_cell.qps(),
        twin_single.qps()
    );
    println!(
        "descriptive: search N=8/N=1 p50 x{:.2}, qps x{:.2}; graph p50 x{:.2}, qps x{:.2}; document p50 x{:.2}, qps x{:.2}",
        search_n8.p50() / search_n1.p50(),
        search_n8.qps() / search_n1.qps(),
        graph_n8.p50() / graph_n1.p50(),
        graph_n8.qps() / graph_n1.qps(),
        doc_n8.p50() / doc_n1.p50(),
        parallel_ratio
    );
    let mut srv1: Vec<f64> = graph_n1.samples.iter().filter_map(|s| s.srv_ms.map(|v| v as f64)).collect();
    let mut srv8: Vec<f64> = graph_n8.samples.iter().filter_map(|s| s.srv_ms.map(|v| v as f64)).collect();
    if !srv1.is_empty() && !srv8.is_empty() {
        println!(
            "  graph client p50 - server p50 (lock wait + transport): N=1 {:.1} ms, N=8 {:.1} ms",
            graph_n1.p50() - median(&mut srv1),
            graph_n8.p50() - median(&mut srv8)
        );
    }
    println!(
        "  cpu per request, search N=8 / N=1: {}",
        cpu_ratio.map(|r| format!("x{r:.2}")).unwrap_or_else(|| "n/a".to_string())
    );
    println!(
        "verdict: refactor warranted = {verdict} (D_full > E: {d_wins}, twin >= 1.5: {twin_wins}, cpu/req N8 vs N1: {})",
        cpu_ratio.map(|r| format!("x{r:.2}")).unwrap_or_else(|| "n/a".to_string())
    );

    // ---- JSON ----------------------------------------------------------
    let out = match std::env::var("GROOVE_BENCH_OUT") {
        Ok(p) if !p.trim().is_empty() => PathBuf::from(p.trim()),
        _ => {
            let secs = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            std::env::temp_dir().join(format!("groove-http-lock-contention-{secs}.json"))
        }
    };
    let report = json!({
        "corpus": staged.label,
        "config": staged.config.display().to_string(),
        "model": kb_model,
        "documents": documents,
        "chunks": chunks,
        "build": build,
        "cores_logical": cores,
        "groove": version_line(),
        "query": query,
        "doc": doc,
        "cells": cells.iter().map(Cell::to_json).collect::<Vec<_>>(),
        "loops": loops.iter().map(Cell::to_json).collect::<Vec<_>>(),
        "twin": { "split": twin_cell.to_json(), "solo": twin_single.to_json(), "ratio": twin_ratio },
        "rerank": rerank_cells.iter().map(Cell::to_json).collect::<Vec<_>>(),
        "halves": { "model": ed.model, "e_ms": e, "d_ms": ed.d_ms, "d_full_ms": d_full, "candidates": ed.candidates,
                    "qps_now": qps_now, "qps_pool8": qps_pool8 },
        "verdict": { "refactor_warranted": verdict, "d_full_gt_e": d_wins, "twin_ge_1_5": twin_wins, "cpu_per_req_ratio": cpu_ratio },
    });
    std::fs::write(&out, serde_json::to_string_pretty(&report).expect("serialise report"))
        .unwrap_or_else(|e| panic!("write {}: {e}", out.display()));
    println!("json: {}", out.display());
}
