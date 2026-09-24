//! AW-06: the OpenAI-compatible embedding provider, driven end to end.
//!
//! `[embedding] provider = "openai-compatible"` reaches the network from four
//! places -- `groove index`, `groove search`, the MCP `search` tool and the
//! file watcher -- and none of them had a test on the pull-request gate. These
//! run a stand-in endpoint inside the test process (`common::embed_mock`), so
//! nothing is downloaded and nothing is `#[ignore]`d.
//!
//! What each test pins, and the change that turns it red, is written next to
//! the test.

mod common;

use common::ansi::strip_ansi;
use common::embed_mock::{
    DOC_MODEL, EmbedMock, QUERY_MODEL, Recorded, assert_dir_empty, embed_text, hermetic,
    openai_config_toml, wait_until,
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
/// A word only `alpha.md` contains, so a query for it has one right answer
/// on both the vector and the keyword side.
const ALPHA_MARKER: &str = "zebracornium";

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
        "---\ntitle: Beta\n---\n\n## Runtime\n\nA tokio runtime worker thread must not \
         block on network input or output.\n",
    );
    let mock = EmbedMock::start(DIM);
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
/// Red if the query side sends `document_model` (the two sides are not
/// interchangeable; see [`grooveseek::embedder`]), or if either command
/// stops reaching the endpoint.
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

    let resp = fx.search_cli(ALPHA_MARKER);
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
    assert_eq!(new[0].inputs(), vec![ALPHA_MARKER.to_string()]);
    assert!(
        top_path(&resp).ends_with("alpha.md"),
        "the document sharing the query's word must rank first: {resp}"
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
/// side, from inside the server's runtime.
///
/// Red if the server's search path sends `document_model`, or loses the
/// endpoint (the config reaches `serve` only through `--config`).
#[test]
fn the_mcp_search_tool_embeds_the_query_through_the_http_provider() {
    let fx = fixture("groove-aw06-mcp", None, "");
    fx.index();
    let before = fx.mock.requests().len();

    let (guard, base) = spawn_serve_with(fx.kb(), &fx.config, false, |c| {
        hermetic(c, &fx.cache);
    });
    let session = mcp_initialize(&base);
    let resp = mcp_search_call(&base, &session, mcp_search_args(ALPHA_MARKER));

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
    assert_eq!(new[0].inputs(), vec![ALPHA_MARKER.to_string()]);
    assert!(top_path(&resp).ends_with("alpha.md"), "{resp}");
    drop(guard);
    assert_dir_empty(&fx.cache);
}

/// A word only the file written while the server runs contains.
const FRESH_MARKER: &str = "quillfeatherstone";

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
        &format!(
            "---\ntitle: Fresh\n---\n\n## Fresh\n\nThe {FRESH_MARKER} arrived after the server started.\n"
        ),
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

    let session = mcp_initialize(&base);
    let resp = mcp_search_call(&base, &session, mcp_search_args(FRESH_MARKER));
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
/// [`grooveseek::config`], and `OpenAiCompatibleConfig::new` in
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
