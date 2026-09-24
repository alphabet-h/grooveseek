//! A stand-in for an OpenAI-compatible `/v1/embeddings` endpoint, run inside
//! the test process, plus what a test needs to point `groove` at it.
//!
//! Hand-written HTTP/1.1 on `std::net`: no crate is added for this. It is built
//! so the flaws AW-18 found in the unit-test mock in [`grooveseek::embedder`]
//! never arrive here:
//! - `accept` is non-blocking and polled against a stop flag and a lifetime
//!   deadline, so a regression that never connects cannot hang the test;
//! - reads on an accepted connection time out after a short poll and look at
//!   the same stop flag, so a client that connects and then says nothing
//!   cannot hold up `Drop` (it still has the whole per-request budget to send
//!   its request while the mock runs);
//! - a connection closed before it wrote anything (`read == 0`) is dropped,
//!   not asserted on;
//! - every response carries `Connection: close`, so one request is one
//!   connection and [`crate::common::embed_mock::connection_count`] means
//!   something.
//!
//! The default answer is a deterministic bag-of-words vector
//! ([`crate::common::embed_mock::embed_text`]): a query and a document
//! sharing a word end up close, so a test can assert the ranking and not
//! only that a request arrived.
//! [`crate::common::embed_mock::with_responder`] replaces the answer (AW-03's
//! 401, AW-04's 413 / 429).

use std::collections::BTreeMap;
use std::io::{ErrorKind, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::Path;
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

/// The `document_model` [`openai_config_toml`] writes.
pub const DOC_MODEL: &str = "doc-model";
/// The `query_model` [`openai_config_toml`] writes.
pub const QUERY_MODEL: &str = "query-model";

/// How long a mock may live. A leaked one stops on its own after this.
const MAX_LIFETIME: Duration = Duration::from_secs(300);
/// How long one connection may take to send its request.
const READ_TIMEOUT: Duration = Duration::from_secs(10);
/// How long one read may block before the stop flag is looked at again.
const READ_POLL: Duration = Duration::from_millis(50);
/// Pause between `accept` polls.
const ACCEPT_POLL: Duration = Duration::from_millis(5);
/// Pause between [`wait_until`] polls.
const WAIT_POLL: Duration = Duration::from_millis(50);

/// One request the mock received.
#[derive(Clone, Debug)]
pub struct Recorded {
    pub method: String,
    pub path: String,
    /// Header names lowercased; values trimmed.
    pub headers: BTreeMap<String, String>,
    /// The body parsed as JSON, or `Null` when it was not JSON.
    pub body: serde_json::Value,
}

impl Recorded {
    /// The value of header `name` (pass it lowercased).
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).map(String::as_str)
    }

    /// The `model` field of the body.
    pub fn model(&self) -> Option<&str> {
        self.body.get("model")?.as_str()
    }

    /// The `input` array of the body, as strings.
    pub fn inputs(&self) -> Vec<String> {
        self.body
            .get("input")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|s| s.as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default()
    }
}

/// What the mock answers with.
pub struct MockResponse {
    pub status: u16,
    pub body: Vec<u8>,
}

impl MockResponse {
    pub fn json(status: u16, value: &serde_json::Value) -> Self {
        Self {
            status,
            body: serde_json::to_vec(value).expect("serialize mock response"),
        }
    }
}

/// The answer an OpenAI-compatible endpoint gives: one vector per input, in
/// order, as long as the dimension parameter says. A body without an
/// `input` array gets a 400.
pub fn default_response(req: &Recorded, dimension: usize) -> MockResponse {
    let Some(inputs) = req.body.get("input").and_then(|v| v.as_array()) else {
        return MockResponse::json(
            400,
            &serde_json::json!({"error": {"message": "mock: body has no `input` array"}}),
        );
    };
    let data: Vec<serde_json::Value> = inputs
        .iter()
        .enumerate()
        .map(|(i, text)| {
            serde_json::json!({
                "object": "embedding",
                "index": i,
                "embedding": embed_text(text.as_str().unwrap_or(""), dimension),
            })
        })
        .collect();
    MockResponse::json(
        200,
        &serde_json::json!({"object": "list", "data": data, "model": req.model()}),
    )
}

/// A deterministic, L2-normalised bag-of-words vector.
///
/// Tokens are the lowercased alphanumeric runs of `text`; each adds 1 to the
/// bucket its FNV-1a hash picks. FNV rather than `DefaultHasher` because the
/// latter's output is not promised to stay the same across Rust releases. A
/// text with no tokens gets `e0`, so no vector is all zeros (which would
/// normalise to NaN).
pub fn embed_text(text: &str, dimension: usize) -> Vec<f32> {
    let mut v = vec![0.0f32; dimension];
    for token in text
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
    {
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for b in token.to_lowercase().bytes() {
            h ^= u64::from(b);
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
        v[(h % dimension as u64) as usize] += 1.0;
    }
    if v.iter().all(|x| *x == 0.0) {
        v[0] = 1.0;
    }
    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    v.iter_mut().for_each(|x| *x /= norm);
    v
}

/// Poll `cond` until it holds or `deadline` passes; `true` when it held.
///
/// The one polling loop these tests use: [`EmbedMock::wait_for`] is this over
/// the recorded requests, and a test waiting on something else (a server's
/// stderr) calls it directly.
pub fn wait_until(deadline: Duration, mut cond: impl FnMut() -> bool) -> bool {
    let end = Instant::now() + deadline;
    loop {
        if cond() {
            return true;
        }
        if Instant::now() >= end {
            return false;
        }
        thread::sleep(WAIT_POLL);
    }
}

type Responder = dyn Fn(&Recorded) -> MockResponse + Send + Sync;

/// The running mock. Dropping it stops the accept loop and joins the thread.
pub struct EmbedMock {
    addr: SocketAddr,
    requests: Arc<Mutex<Vec<Recorded>>>,
    connections: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl EmbedMock {
    /// A mock that answers every request with [`default_response`].
    pub fn start(dimension: usize) -> Self {
        Self::with_responder(move |req| default_response(req, dimension))
    }

    /// A mock that answers with `responder`. Requests are recorded before the
    /// responder runs, whatever it answers.
    pub fn with_responder(
        responder: impl Fn(&Recorded) -> MockResponse + Send + Sync + 'static,
    ) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock listener");
        listener
            .set_nonblocking(true)
            .expect("non-blocking mock listener");
        let addr = listener.local_addr().expect("mock local addr");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let connections = Arc::new(AtomicUsize::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let responder: Arc<Responder> = Arc::new(responder);
        let handle = {
            let (requests, connections, stop) =
                (requests.clone(), connections.clone(), stop.clone());
            thread::spawn(move || {
                accept_loop(listener, &requests, &connections, &stop, responder.as_ref())
            })
        };
        Self {
            addr,
            requests,
            connections,
            stop,
            handle: Some(handle),
        }
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// The full endpoint URL, `/v1/embeddings` included, as `[embedding].endpoint` wants it.
    pub fn endpoint(&self) -> String {
        format!("http://{}/v1/embeddings", self.addr)
    }

    /// A copy of every request recorded so far, in arrival order.
    pub fn requests(&self) -> Vec<Recorded> {
        self.requests.lock().expect("mock requests lock").clone()
    }

    /// How many connections were accepted, including ones that sent nothing.
    pub fn connection_count(&self) -> usize {
        self.connections.load(Ordering::SeqCst)
    }

    /// Poll until `pred` holds over the recorded requests, or `deadline` passes.
    pub fn wait_for(&self, deadline: Duration, pred: impl Fn(&[Recorded]) -> bool) -> bool {
        wait_until(deadline, || pred(&self.requests()))
    }
}

impl Drop for EmbedMock {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

fn accept_loop(
    listener: TcpListener,
    requests: &Mutex<Vec<Recorded>>,
    connections: &AtomicUsize,
    stop: &AtomicBool,
    responder: &Responder,
) {
    let end = Instant::now() + MAX_LIFETIME;
    while !stop.load(Ordering::SeqCst) && Instant::now() < end {
        match listener.accept() {
            Ok((stream, _)) => {
                connections.fetch_add(1, Ordering::SeqCst);
                serve_one(stream, requests, responder, stop);
            }
            // `WouldBlock` is the usual case (nobody is connecting). Any other
            // error is transient for a loopback listener; the loop tries again
            // either way, bounded by the stop flag and the lifetime deadline.
            Err(_) => thread::sleep(ACCEPT_POLL),
        }
    }
}

/// Read one request, record it, answer it, close. Anything malformed or cut
/// short is dropped without a panic: a panic here would kill the accept loop
/// and turn every later request into a hang.
///
/// The socket option calls are the exception: a failure there panics, because a
/// read without its short timeout would block past the stop flag, and `Drop`
/// would then wait on it for good.
fn serve_one(
    mut stream: TcpStream,
    requests: &Mutex<Vec<Recorded>>,
    responder: &Responder,
    stop: &AtomicBool,
) {
    stream
        .set_nonblocking(false)
        .expect("blocking mock connection");
    stream
        .set_read_timeout(Some(READ_POLL))
        .expect("read timeout on mock connection");
    let deadline = Instant::now() + READ_TIMEOUT;
    let Some(recorded) = read_request(&mut stream, stop, deadline) else {
        return;
    };
    requests
        .lock()
        .expect("mock requests lock")
        .push(recorded.clone());
    let resp = responder(&recorded);
    let head = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        resp.status,
        reason(resp.status),
        resp.body.len()
    );
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(&resp.body);
    let _ = stream.flush();
}

/// One read that gives up when the mock is stopping or the request is out of
/// time. `None` means abandon the connection: closed, failed, stopped, or past
/// the deadline.
///
/// The socket's read timeout is [`READ_POLL`], so a client that connects and
/// then says nothing costs the serving thread at most that long before it
/// looks at the stop flag again. A timed-out read is `WouldBlock` on Unix and
/// `TimedOut` on Windows.
fn read_some(
    stream: &mut TcpStream,
    chunk: &mut [u8],
    stop: &AtomicBool,
    deadline: Instant,
) -> Option<usize> {
    loop {
        match stream.read(chunk) {
            Ok(0) => return None,
            Ok(n) => return Some(n),
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                if stop.load(Ordering::SeqCst) || Instant::now() >= deadline {
                    return None;
                }
            }
            Err(_) => return None,
        }
    }
}

fn read_request(stream: &mut TcpStream, stop: &AtomicBool, deadline: Instant) -> Option<Recorded> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    let header_end = loop {
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos;
        }
        let n = read_some(stream, &mut chunk, stop, deadline)?;
        buf.extend_from_slice(&chunk[..n]);
    };
    let head = String::from_utf8_lossy(&buf[..header_end]).into_owned();
    let mut lines = head.split("\r\n");
    let mut request_line = lines.next()?.split_whitespace();
    let method = request_line.next()?.to_string();
    let path = request_line.next()?.to_string();
    let headers: BTreeMap<String, String> = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string()))
        .collect();
    let len: usize = headers
        .get("content-length")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let mut body = buf[header_end + 4..].to_vec();
    while body.len() < len {
        let n = read_some(stream, &mut chunk, stop, deadline)?;
        body.extend_from_slice(&chunk[..n]);
    }
    body.truncate(len);
    let body = serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null);
    Some(Recorded {
        method,
        path,
        headers,
        body,
    })
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        413 => "Payload Too Large",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        _ => "Status",
    }
}

/// A `groove.toml` that selects the OpenAI-compatible provider at `endpoint`.
///
/// Pass it with `--config` **before** the subcommand: a config found by
/// discovery has its `[embedding]` section dropped (R7, [`grooveseek::config`]). No
/// top-level `model` (refused with this provider) and no reranker (it would
/// download one). `timeout_seconds` is short so a mock that stops answering
/// fails the test inside its deadline rather than after the 60 s default.
pub fn openai_config_toml(endpoint: &str, api_key: Option<&str>, dimension: usize) -> String {
    let mut s = format!(
        "[embedding]\n\
         provider = \"openai-compatible\"\n\
         endpoint = {}\n\
         document_model = {}\n\
         query_model = {}\n\
         dimension = {dimension}\n\
         timeout_seconds = 15\n",
        toml_str(endpoint),
        toml_str(DOC_MODEL),
        toml_str(QUERY_MODEL),
    );
    if let Some(key) = api_key {
        s.push_str(&format!("api_key = {}\n", toml_str(key)));
    }
    s
}

/// A TOML basic string. Only ASCII without quotes or backslashes reaches this.
fn toml_str(s: &str) -> String {
    assert!(
        s.chars()
            .all(|c| c.is_ascii() && c != '"' && c != '\\' && !c.is_control()),
        "toml_str only handles plain ASCII: {s:?}"
    );
    format!("\"{s}\"")
}

/// Pin the environment a child `groove` sees, so it reaches the mock and
/// nothing else.
///
/// - `GROOVE_EMBEDDING_API_KEY` is removed: it overrides `api_key`, and a
///   developer's real key would both leak to the mock and break the
///   authorization test.
/// - Proxy variables are removed (both cases; reqwest reads either) and
///   `NO_PROXY` covers loopback, so a runner's proxy never sees the request.
/// - `FASTEMBED_CACHE_DIR` points at `fastembed_dir`, which the caller keeps
///   empty and checks with [`assert_dir_empty`]: nothing on these paths may
///   download a model, and a file appearing there says one did.
pub fn hermetic<'a>(cmd: &'a mut Command, fastembed_dir: &Path) -> &'a mut Command {
    for var in [
        "GROOVE_EMBEDDING_API_KEY",
        "HTTP_PROXY",
        "http_proxy",
        "HTTPS_PROXY",
        "https_proxy",
        "ALL_PROXY",
        "all_proxy",
    ] {
        cmd.env_remove(var);
    }
    cmd.env("NO_PROXY", "127.0.0.1,localhost")
        .env("no_proxy", "127.0.0.1,localhost")
        .env("FASTEMBED_CACHE_DIR", fastembed_dir)
}

/// Panic unless `dir` exists and is empty. The model-download tripwire.
pub fn assert_dir_empty(dir: &Path) {
    let entries: Vec<_> = std::fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("read {}: {e}", dir.display()))
        .filter_map(Result::ok)
        .map(|e| e.file_name())
        .collect();
    assert!(
        entries.is_empty(),
        "{} must stay empty (a model was downloaded?): {entries:?}",
        dir.display()
    );
}
