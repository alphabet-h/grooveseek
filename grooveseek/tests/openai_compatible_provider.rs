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

use common::embed_mock::{
    DOC_MODEL, EmbedMock, QUERY_MODEL, Recorded, assert_dir_empty, embed_text, hermetic,
    openai_config_toml,
};

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
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
    // whichever spelling came first. On Unix the two spellings are two entries
    // with the same value, so folding them loses nothing.
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
