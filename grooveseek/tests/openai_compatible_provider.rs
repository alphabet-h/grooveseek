//! AW-06: the OpenAI-compatible embedding provider, driven end to end.
//!
//! `[embedding] provider = "openai-compatible"` reaches the network from four
//! places -- `groove index`, `groove search`, the MCP `search` tool and the
//! file watcher -- and none of them had a test on the pull-request gate. These
//! run a stand-in endpoint inside the test process
//! ([`crate::common::embed_mock`]), so nothing is downloaded and nothing is
//! `#[ignore]`d.
//!
//! What each test pins, and the change that turns it red, is written next to
//! the test.

mod common;

use common::ansi::strip_ansi;
use common::embed_mock::{
    DOC_MODEL, EmbedMock, MockResponse, QUERY_MODEL, Recorded, assert_dir_empty, default_response,
    embed_text, hermetic, openai_config_toml, wait_until,
};
use common::mcp::{grooveseek_bin, mcp_initialize, mcp_search_call, spawn_serve_with};
use common::temp::TempKbLayout;

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

/// POST `body` to the mock with a raw socket and return `(status, body)`.
///
/// Raw on purpose: these self tests check the mock, and going through reqwest
/// would put the client under test as well.
fn raw_post(addr: SocketAddr, body: &str, extra_headers: &[&str]) -> (u16, String) {
    let mut stream = TcpStream::connect(addr).expect("connect to mock");
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .expect("read timeout");
    let mut req = format!(
        "POST /v1/embeddings HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n",
        body.len()
    );
    for h in extra_headers {
        req.push_str(h);
        req.push_str("\r\n");
    }
    req.push_str("\r\n");
    req.push_str(body);
    stream.write_all(req.as_bytes()).expect("write request");
    let mut raw = String::new();
    stream.read_to_string(&mut raw).expect("read response");
    let status: u16 = raw
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| panic!("no status line in {raw:?}"));
    let body = raw
        .split_once("\r\n\r\n")
        .map(|(_, b)| b.to_string())
        .unwrap_or_default();
    (status, body)
}

#[test]
fn embed_mock_answers_in_the_openai_shape_and_records_what_it_received() {
    let mock = EmbedMock::start(16);
    let (status, body) = raw_post(
        mock.addr(),
        r#"{"model":"m","input":["alpha beta","gamma"]}"#,
        &["Authorization: Bearer secret", "X-Mixed-Case: v"],
    );
    assert_eq!(status, 200, "body: {body}");
    let v: serde_json::Value = serde_json::from_str(&body).expect("JSON body");
    let data = v["data"].as_array().expect("data array");
    assert_eq!(data.len(), 2);
    for (i, item) in data.iter().enumerate() {
        assert_eq!(item["index"].as_u64(), Some(i as u64));
        assert_eq!(item["embedding"].as_array().map(Vec::len), Some(16));
    }

    let reqs: Vec<Recorded> = mock.requests();
    assert_eq!(reqs.len(), 1);
    assert_eq!(reqs[0].method, "POST");
    assert_eq!(reqs[0].path, "/v1/embeddings");
    assert_eq!(reqs[0].model(), Some("m"));
    assert_eq!(
        reqs[0].inputs(),
        vec!["alpha beta".to_string(), "gamma".to_string()]
    );
    assert_eq!(reqs[0].header("authorization"), Some("Bearer secret"));
    assert_eq!(
        reqs[0].header("x-mixed-case"),
        Some("v"),
        "names are lowercased"
    );
    assert_eq!(mock.connection_count(), 1);
}

#[test]
fn embed_mock_survives_a_client_that_hangs_up_without_writing() {
    let mock = EmbedMock::start(8);
    // AW-18: the unit-test mock in `embedder.rs` asserts `read > 0` and panics
    // here. This one must drop the connection and keep serving.
    drop(TcpStream::connect(mock.addr()).expect("connect"));
    let (status, _) = raw_post(mock.addr(), r#"{"model":"m","input":["x"]}"#, &[]);
    assert_eq!(status, 200);
    assert!(
        mock.connection_count() >= 2,
        "both connections were accepted"
    );
    assert_eq!(
        mock.requests().len(),
        1,
        "the empty connection records nothing"
    );
}

#[test]
fn embed_mock_vectors_are_unit_length_and_close_for_shared_words() {
    for text in ["", "!!!", "zebracornium", "a b c d e f"] {
        let v = embed_text(text, 32);
        let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-5, "{text:?} has norm {norm}");
    }
    assert_eq!(embed_text("same words", 32), embed_text("Same, words!", 32));
    let dot = |a: &[f32], b: &[f32]| a.iter().zip(b).map(|(x, y)| x * y).sum::<f32>();
    let q = embed_text("zebracornium", 64);
    let near = embed_text("the zebracornium grazes", 64);
    let far = embed_text("tokio runtime worker", 64);
    assert!(
        dot(&q, &near) > dot(&q, &far),
        "a shared word must bring vectors closer"
    );
}

#[test]
fn embed_mock_stops_promptly_when_dropped() {
    let mock = EmbedMock::start(8);
    let started = Instant::now();
    drop(mock);
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "Drop must stop the accept loop, took {:?}",
        started.elapsed()
    );
}

#[test]
fn embed_mock_stops_promptly_while_a_client_holds_a_connection_open() {
    let mock = EmbedMock::start(8);
    // Connected, nothing written, not closed: the serving thread is inside
    // the read for this connection when the mock is dropped.
    let held = TcpStream::connect(mock.addr()).expect("connect");
    assert!(
        common::embed_mock::wait_until(Duration::from_secs(5), || mock.connection_count() >= 1),
        "the mock never accepted the connection"
    );
    // Dropped on another thread: a mock stuck in its read would otherwise
    // hang this test instead of failing it.
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let started = Instant::now();
        drop(mock);
        let _ = tx.send(started.elapsed());
    });
    let took = rx.recv_timeout(Duration::from_secs(5));
    drop(held);
    let took = took.expect("Drop did not return within 5 s while a client held a connection open");
    assert!(
        took < Duration::from_secs(2),
        "Drop must not wait out a half-open connection, took {took:?}"
    );
}

/// Linux and macOS only, on purpose. Backpressure cannot be induced on a
/// Windows loopback connection: Windows takes a blocking write like this whole
/// (measured: 512 MiB returned `Ok` in 66 ms to a client that never read), so
/// there the test would pass with or without the bounded write. The bounded
/// write itself ships on every OS.
#[test]
#[cfg(not(windows))]
fn embed_mock_stops_promptly_while_a_client_stops_reading_the_response() {
    // Far more than Linux or macOS buffers on a loopback connection, so the
    // write meets backpressure from a client that never reads.
    const BODY: usize = 64 * 1024 * 1024;
    let mock = EmbedMock::with_responder(|_| common::embed_mock::MockResponse {
        status: 200,
        body: vec![b' '; BODY],
    });
    let mut held = TcpStream::connect(mock.addr()).expect("connect");
    let body = r#"{"model":"m","input":["x"]}"#;
    let req = format!(
        "POST /v1/embeddings HTTP/1.1\r\nHost: {}\r\nContent-Length: {}\r\n\r\n{body}",
        mock.addr(),
        body.len()
    );
    held.write_all(req.as_bytes()).expect("write request");
    assert!(
        wait_until(Duration::from_secs(5), || mock.requests().len() == 1),
        "the mock never recorded the request"
    );
    // Dropped on another thread: a mock stuck in its write would otherwise
    // hang this test instead of failing it.
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let started = Instant::now();
        drop(mock);
        let _ = tx.send(started.elapsed());
    });
    let took = rx.recv_timeout(Duration::from_secs(5));
    drop(held);
    let took = took.expect("Drop did not return within 5 s while the client stopped reading");
    assert!(
        took < Duration::from_secs(2),
        "Drop must not wait on a client that stopped reading, took {took:?}"
    );
}

#[test]
fn the_config_helper_writes_an_openai_compatible_section_and_no_top_level_model() {
    let with_key = openai_config_toml("http://127.0.0.1:9/v1/embeddings", Some("   "), 32);
    let parsed: toml::Table = with_key.parse().expect("helper writes valid TOML");
    let emb = parsed["embedding"].as_table().expect("[embedding] table");
    assert_eq!(emb["provider"].as_str(), Some("openai-compatible"));
    assert_eq!(
        emb["endpoint"].as_str(),
        Some("http://127.0.0.1:9/v1/embeddings")
    );
    assert_eq!(emb["document_model"].as_str(), Some(DOC_MODEL));
    assert_eq!(emb["query_model"].as_str(), Some(QUERY_MODEL));
    assert_eq!(emb["dimension"].as_integer(), Some(32));
    assert_eq!(
        emb["api_key"].as_str(),
        Some("   "),
        "a blank key is written as given"
    );
    assert!(
        !parsed.contains_key("model"),
        "top-level `model` is refused with openai-compatible"
    );
    let without = openai_config_toml("http://127.0.0.1:9/v1/embeddings", None, 32);
    assert!(!without.contains("api_key"));
}

#[test]
fn hermetic_strips_proxies_and_the_env_api_key_from_the_child() {
    let cache = common::temp::TempRoot::new("groove-aw06-hermetic");
    let mut cmd = std::process::Command::new("unused");
    hermetic(&mut cmd, cache.path());
    // Keyed uppercase: Windows environment names are case-insensitive, and
    // `get_envs` there reports `http_proxy` and `HTTP_PROXY` as one entry under
    // whichever spelling came first. On Unix the two spellings are two entries,
    // and the fold would hide a lowercase one going missing -- the block below
    // checks those by their raw names.
    let envs: std::collections::HashMap<String, Option<String>> = cmd
        .get_envs()
        .map(|(k, v)| {
            (
                k.to_string_lossy().to_ascii_uppercase(),
                v.map(|v| v.to_string_lossy().into_owned()),
            )
        })
        .collect();
    for removed in [
        "GROOVE_EMBEDDING_API_KEY",
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "ALL_PROXY",
    ] {
        assert_eq!(envs.get(removed), Some(&None), "{removed} must be removed");
    }
    assert_eq!(envs["NO_PROXY"].as_deref(), Some("127.0.0.1,localhost"));
    assert_eq!(
        envs["FASTEMBED_CACHE_DIR"].as_deref(),
        Some(cache.path().to_string_lossy().as_ref())
    );

    // reqwest reads the lowercase spellings too, and on Unix they are separate
    // variables: each must be handled under its own name.
    #[cfg(not(windows))]
    {
        let raw: std::collections::HashMap<String, Option<String>> = cmd
            .get_envs()
            .map(|(k, v)| {
                (
                    k.to_string_lossy().into_owned(),
                    v.map(|v| v.to_string_lossy().into_owned()),
                )
            })
            .collect();
        for removed in ["http_proxy", "https_proxy", "all_proxy"] {
            assert_eq!(raw.get(removed), Some(&None), "{removed} must be removed");
        }
        assert_eq!(raw["no_proxy"].as_deref(), Some("127.0.0.1,localhost"));
    }
}

#[test]
#[should_panic(expected = "must stay empty")]
fn assert_dir_empty_fires_when_a_file_lands_in_the_cache() {
    // The tripwire the provider tests lean on: without this, an
    // `assert_dir_empty` that never fires would pass them all.
    let cache = common::temp::TempRoot::new("groove-aw06-tripwire");
    std::fs::write(cache.path().join("model.onnx"), b"x").expect("write into cache");
    assert_dir_empty(cache.path());
}

/// Vector length the mock answers with and the config declares.
const DIM: usize = 256;
/// A word only `alpha.md` contains, to tell its text apart in what
/// `groove index` sends.
const ALPHA_MARKER: &str = "zebracornium";
/// The body of `beta.md`.
const BETA_BODY: &str = "A tokio runtime worker thread must not block on network input or output.";
/// A query that is a substring of no document, path or title, so the keyword
/// side finds nothing for it. [`steered_response`] answers it with
/// [`BETA_BODY`]'s vector, so the only way `beta.md` ranks first for it is
/// through the vector the endpoint returned.
const BETA_PROBE: &str = "qxjvwk";

/// Where a probe query's vector points: at one document's text.
fn probe_target(query: &str) -> Option<&'static str> {
    match query {
        BETA_PROBE => Some(BETA_BODY),
        FRESH_PROBE => Some(FRESH_BODY),
        _ => None,
    }
}

/// The mock's answer: [`default_response`], except that a probe query (see
/// [`probe_target`]) sent with the query model gets the vector of the text it
/// points at.
///
/// Without this a search result proves nothing about the vectors: a query
/// word that is also in the document ranks it first through the keyword side
/// alone, whatever vector came back.
fn steered_response(req: &Recorded) -> MockResponse {
    if req.model() == Some(QUERY_MODEL)
        && let [query] = req.inputs().as_slice()
        && let Some(target) = probe_target(query)
    {
        let mut steered = req.clone();
        steered.body["input"] = serde_json::json!([target]);
        return default_response(&steered, DIM);
    }
    default_response(req, DIM)
}

/// A two-document knowledge base, a mock endpoint, and a `groove.toml` that
/// points at the mock.
///
/// Fields drop in declaration order: the mock first, the directories last,
/// so nothing is removed from under a thread still using it. A `ServerGuard`
/// a test holds is a separate local declared after its `Fixture` and so
/// drops before it.
struct Fixture {
    mock: EmbedMock,
    config: PathBuf,
    cache: PathBuf,
    layout: TempKbLayout,
}

fn fixture(prefix: &str, api_key: Option<&str>, extra_toml: &str) -> Fixture {
    let layout = TempKbLayout::new(prefix);
    layout.write(
        "alpha.md",
        &format!(
            "---\ntitle: Alpha\n---\n\n## Grazing\n\nThe {ALPHA_MARKER} grazes on the \
             northern slope and is seen only at dawn.\n"
        ),
    );
    layout.write(
        "beta.md",
        &format!("---\ntitle: Beta\n---\n\n## Runtime\n\n{BETA_BODY}\n"),
    );
    let mock = EmbedMock::with_responder(steered_response);
    let config = layout.root().join("groove.toml");
    std::fs::write(
        &config,
        openai_config_toml(&mock.endpoint(), api_key, DIM) + extra_toml,
    )
    .expect("write groove.toml");
    let cache = layout.root().join("fastembed-tripwire");
    std::fs::create_dir_all(&cache).expect("create tripwire dir");
    Fixture {
        mock,
        config,
        cache,
        layout,
    }
}

impl Fixture {
    fn kb(&self) -> &Path {
        self.layout.kb()
    }

    /// `groove --config <cfg>` with the pinned environment; the caller adds
    /// the subcommand.
    fn cmd(&self) -> Command {
        let mut cmd = Command::new(grooveseek_bin());
        cmd.arg("--config").arg(&self.config);
        hermetic(&mut cmd, &self.cache);
        cmd
    }

    fn index(&self) {
        let out = self
            .cmd()
            .arg("index")
            .arg("--kb-path")
            .arg(self.kb())
            .output()
            .expect("spawn groove index");
        assert!(
            out.status.success(),
            "groove index failed: {}\nrequests: {}",
            String::from_utf8_lossy(&out.stderr),
            describe(&self.mock.requests())
        );
    }

    fn search_cli(&self, query: &str) -> serde_json::Value {
        let out = self
            .cmd()
            .args(["search", query, "--kb-path"])
            .arg(self.kb())
            .args([
                "--format",
                "json",
                "--limit",
                "5",
                "--include-low-quality",
                "--min-confidence-ratio",
                "0",
            ])
            .output()
            .expect("spawn groove search");
        assert!(
            out.status.success(),
            "groove search failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
            panic!(
                "search stdout is not JSON ({e}): {}",
                String::from_utf8_lossy(&out.stdout)
            )
        })
    }
}

/// The path of the first hit, or `""` when there is none.
fn top_path(resp: &serde_json::Value) -> String {
    resp.pointer("/results/0/path")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string()
}

/// One line per request, for failure messages.
fn describe(reqs: &[Recorded]) -> String {
    reqs.iter()
        .map(|r| format!("{:?} {:?}", r.model(), r.inputs()))
        .collect::<Vec<_>>()
        .join("\n")
}

/// `groove index` embeds with `document_model`, `groove search` with
/// `query_model`, and the vectors that come back are the ones the ranking
/// uses.
///
/// The query is [`BETA_PROBE`], which the keyword side cannot find, so
/// `beta.md` ranks first only through the vectors: the one [`steered_response`]
/// returned for the query and the ones `groove index` stored for the
/// documents.
///
/// Red if the query side sends `document_model` (the two sides are not
/// interchangeable; see [`grooveseek::embedder`]), if either command stops
/// reaching the endpoint, or if the returned vectors stop deciding the
/// ranking.
#[test]
fn index_then_search_round_trips_through_an_openai_compatible_endpoint() {
    let fx = fixture("groove-aw06-cli", None, "");
    fx.index();

    let indexed = fx.mock.requests();
    assert!(!indexed.is_empty(), "index sent nothing to the endpoint");
    assert!(
        indexed.iter().all(|r| r.model() == Some(DOC_MODEL)),
        "every index request must name the document model:\n{}",
        describe(&indexed)
    );
    assert!(
        indexed
            .iter()
            .flat_map(Recorded::inputs)
            .any(|i| i.contains(ALPHA_MARKER)),
        "alpha.md's text never reached the endpoint:\n{}",
        describe(&indexed)
    );

    let resp = fx.search_cli(BETA_PROBE);
    let all = fx.mock.requests();
    let new = &all[indexed.len()..];
    assert_eq!(
        new.len(),
        1,
        "search must send exactly one request:\n{}",
        describe(new)
    );
    assert_eq!(
        new[0].model(),
        Some(QUERY_MODEL),
        "search must name the query model"
    );
    assert_eq!(new[0].inputs(), vec![BETA_PROBE.to_string()]);
    assert!(
        top_path(&resp).ends_with("beta.md"),
        "the document the query's vector points at must rank first: {resp}"
    );
    assert_dir_empty(&fx.cache);
}

/// MCP `search` arguments that switch off everything able to drop or reorder
/// a hit for reasons other than the vectors: quality filter, confidence
/// trimming, MMR.
fn mcp_search_args(query: &str) -> serde_json::Value {
    serde_json::json!({
        "query": query,
        "limit": 5,
        "include_low_quality": true,
        "min_confidence_ratio": 0.0,
        "mmr": false,
    })
}

/// The MCP `search` tool embeds its query through the provider, on the query
/// side, from inside the server's runtime, and ranks by the vector it got
/// back ([`BETA_PROBE`], as in the CLI round trip).
///
/// Red if the server's search path sends `document_model`, loses the
/// endpoint (the config reaches `serve` only through `--config`), or stops
/// ranking by the returned vector.
#[test]
fn the_mcp_search_tool_embeds_the_query_through_the_http_provider() {
    let fx = fixture("groove-aw06-mcp", None, "");
    fx.index();
    let before = fx.mock.requests().len();

    let (guard, base) = spawn_serve_with(fx.kb(), &fx.config, false, |c| {
        hermetic(c, &fx.cache);
    });
    let session = mcp_initialize(&base);
    let resp = mcp_search_call(&base, &session, mcp_search_args(BETA_PROBE));

    let all = fx.mock.requests();
    let new = &all[before..];
    assert_eq!(
        new.len(),
        1,
        "one MCP search must send one request:\n{}\nserver stderr:\n{}",
        describe(new),
        guard.stderr().lines().join("\n")
    );
    assert_eq!(new[0].model(), Some(QUERY_MODEL));
    assert_eq!(new[0].inputs(), vec![BETA_PROBE.to_string()]);
    assert!(top_path(&resp).ends_with("beta.md"), "{resp}");
    drop(guard);
    assert_dir_empty(&fx.cache);
}

/// A word only the file written while the server runs contains.
const FRESH_MARKER: &str = "quillfeatherstone";
/// The body of that file.
const FRESH_BODY: &str = "The quillfeatherstone arrived after the server started.";
/// [`BETA_PROBE`]'s counterpart for `fresh.md`: found by no keyword, answered
/// with [`FRESH_BODY`]'s vector.
const FRESH_PROBE: &str = "vqzxjk";

/// The watcher embeds a new file through the provider without panicking.
///
/// The provider's HTTP client is reqwest's blocking one, and every blocking
/// send enters reqwest's blocking wait, which in a debug build panics when it
/// runs on a tokio worker thread. [`grooveseek::watcher::run_watch_loop`]
/// keeps it off the workers by handing each event batch to `spawn_blocking`
/// around `handle_events` (see [`grooveseek::watcher`]); red if that
/// `spawn_blocking` is removed. The file is written before any MCP call, so
/// the watcher makes this server's first embed and nothing else has touched
/// the client before it.
#[test]
fn the_watcher_embeds_a_new_file_through_the_http_provider_without_panicking() {
    let fx = fixture(
        "groove-aw06-watch",
        None,
        "\n[watch]\nenabled = true\ndebounce_ms = 300\n",
    );
    fx.index();

    let (guard, base) = spawn_serve_with(fx.kb(), &fx.config, true, |c| {
        hermetic(c, &fx.cache);
    });
    // `spawn_serve_with(.., true, ..)` returns only after `watcher: watching`,
    // so the write below cannot fall before the debouncer is armed.
    fx.layout.write(
        "fresh.md",
        &format!("---\ntitle: Fresh\n---\n\n## Fresh\n\n{FRESH_BODY}\n"),
    );

    let stderr = || -> Vec<String> {
        guard
            .stderr()
            .lines()
            .iter()
            .map(|l| strip_ansi(l))
            .collect()
    };
    let embedded = fx.mock.wait_for(Duration::from_secs(30), |reqs| {
        reqs.iter().any(|r| {
            r.model() == Some(DOC_MODEL) && r.inputs().iter().any(|i| i.contains(FRESH_MARKER))
        })
    });
    let reindexed = wait_until(Duration::from_secs(30), || {
        stderr()
            .iter()
            .any(|l| l.contains("watcher: reindexed fresh.md"))
    });
    // Checked first so that, when the watcher panics, the panic is what the
    // failure says; the two waits above have to be over by now, or a panic
    // line not yet written would pass this.
    let lines = stderr();
    assert!(
        !lines.iter().any(|l| l.contains("panicked")),
        "the watcher panicked:\n{}",
        lines.join("\n")
    );
    assert!(
        embedded,
        "the watcher never sent fresh.md to the endpoint with the document model:\n{}\nstderr:\n{}",
        describe(&fx.mock.requests()),
        lines.join("\n")
    );
    assert!(
        reindexed,
        "no `watcher: reindexed fresh.md` line:\n{}",
        lines.join("\n")
    );

    // A keyword-free probe, so `fresh.md` ranks first only through the vector
    // the watcher stored for it.
    let session = mcp_initialize(&base);
    let resp = mcp_search_call(&base, &session, mcp_search_args(FRESH_PROBE));
    assert!(top_path(&resp).ends_with("fresh.md"), "{resp}");
    drop(guard);
    assert_dir_empty(&fx.cache);
}

/// `groove graph` and `groove doctor` answer from the index alone.
///
/// `groove graph` resolves the embedding config only to check the index was
/// built with it (see verify_embedding_meta in [`grooveseek::db`]) and never
/// builds an embedder, so it needs the same `--config` or that check refuses
/// the index; `groove doctor` does not resolve it at all and runs no such
/// check. Red if either starts embedding -- for `groove graph`, re-embedding
/// the start document instead of reading its stored vectors.
#[test]
fn graph_and_doctor_never_contact_the_endpoint() {
    let fx = fixture("groove-aw06-graph", None, "");
    fx.index();
    let connections = fx.mock.connection_count();
    let requests = fx.mock.requests().len();

    let graph = fx
        .cmd()
        .args(["graph", "--start", "alpha.md", "--kb-path"])
        .arg(fx.kb())
        .output()
        .expect("spawn groove graph");
    assert!(
        graph.status.success(),
        "groove graph failed: {}",
        String::from_utf8_lossy(&graph.stderr)
    );

    let doctor = fx
        .cmd()
        .arg("doctor")
        .arg("--kb-path")
        .arg(fx.kb())
        .output()
        .expect("spawn groove doctor");
    assert_eq!(
        doctor.status.code(),
        Some(0),
        "groove doctor: stdout={} stderr={}",
        String::from_utf8_lossy(&doctor.stdout),
        String::from_utf8_lossy(&doctor.stderr)
    );

    assert_eq!(
        fx.mock.connection_count(),
        connections,
        "graph or doctor opened a connection to the endpoint:\n{}",
        describe(&fx.mock.requests()[requests..])
    );
    assert_dir_empty(&fx.cache);
}

/// A missing or blank `api_key` sends no `Authorization` header; a real one
/// sends `Bearer <key>`.
///
/// Table-driven, one knowledge base per row. The third row is the control:
/// without it, "no header" would also pass when the mock lost headers.
/// Blank keys are filtered twice (resolve_embedding_api_key in
/// [`grooveseek::config`], and OpenAiCompatibleConfig::new in
/// [`grooveseek::embedder`]); this test goes through the config, so it is red
/// only when both filters are gone.
#[test]
fn no_authorization_header_is_sent_when_the_api_key_is_absent_or_blank() {
    let cases: [(Option<&str>, Option<&str>); 3] = [
        (None, None),
        (Some("   "), None),
        (Some("k"), Some("Bearer k")),
    ];
    for (i, (api_key, expected)) in cases.into_iter().enumerate() {
        let fx = fixture(&format!("groove-aw06-auth-{i}"), api_key, "");
        fx.index();
        let reqs = fx.mock.requests();
        assert!(!reqs.is_empty(), "api_key {api_key:?}: index sent nothing");
        for r in &reqs {
            assert_eq!(
                r.header("authorization"),
                expected,
                "api_key {api_key:?}: headers were {:?}",
                r.headers
            );
        }
    }
}
