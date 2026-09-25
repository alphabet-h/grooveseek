//! AW-04: what `groove index`, `groove search`, MCP `rebuild_index` and the
//! watcher do when an OpenAI-compatible endpoint refuses or fails.
//!
//! Every test drives the real `groove` binary against the in-process mock
//! ([`crate::common::embed_mock`]) through the shared fixture
//! ([`crate::common::embed_cli`]), under
//! [`crate::common::embed_mock::hermetic`], so a runner's proxy never sees a
//! request and nothing is downloaded. Waits are kept short by answering
//! `Retry-After: 0`, or 1 where the wait itself is what is checked.

mod common;

use common::ansi::strip_ansi;
use common::embed_cli::{
    BODY_SENTINEL, DIM, REJECT_MARKER, files, fixture, note, rejects_marker, reply, stderr_of,
};
use common::embed_mock::{
    DOC_MODEL, EmbedMock, MockReply, QUERY_MODEL, assert_dir_empty, default_response, hermetic,
    wait_until,
};
use common::mcp::{mcp_initialize, mcp_tool_call, spawn_serve_with};

use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// The mock writes the headers a reply responder returns.
///
/// Red if `serve_one` drops [`MockReply::headers`].
#[test]
fn embed_mock_sends_the_headers_a_reply_responder_returns() {
    let mock = EmbedMock::with_reply_responder(|_| reply(429, &[("Retry-After", "7")]));
    let mut stream = TcpStream::connect(mock.addr()).expect("connect");
    let body = r#"{"model":"m","input":["x"]}"#;
    write!(
        stream,
        "POST /v1/embeddings HTTP/1.1\r\nHost: {}\r\nContent-Length: {}\r\n\r\n{body}",
        mock.addr(),
        body.len()
    )
    .expect("write request");
    let mut answer = String::new();
    stream.read_to_string(&mut answer).expect("read answer");
    assert!(answer.starts_with("HTTP/1.1 429 "), "{answer}");
    assert!(answer.contains("\r\nRetry-After: 7\r\n"), "{answer}");
    assert!(answer.contains(BODY_SENTINEL), "{answer}");
}
