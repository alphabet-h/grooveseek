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

use common::embed_mock::{EmbedMock, Recorded, embed_text};

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
