//! AW-13: a loopback OpenAI-compatible endpoint is contacted directly, even
//! when the environment names a proxy; any other endpoint still goes through
//! that proxy.
//!
//! The document text and the API key are meant for a server on this machine;
//! a proxy set for the outside world must not see them. The test drives the
//! real `groove` binary against the in-process mock
//! ([`crate::common::embed_mock`]) through the shared fixture
//! ([`crate::common::embed_cli`]), then undoes the part of
//! [`crate::common::embed_mock::hermetic`] that would hide the bug: the child
//! gets every proxy variable pointed at a listener that counts connections,
//! and no `NO_PROXY`.

mod common;

use common::embed_cli::{Fixture, fixture, note, stderr_of};

use std::net::TcpListener;
use std::process::Output;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

/// A listener standing in for a proxy: it accepts, counts and drops every
/// connection. Dropping it stops the accept loop.
struct CountingProxy {
    url: String,
    accepted: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl CountingProxy {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind proxy listener");
        listener
            .set_nonblocking(true)
            .expect("non-blocking proxy listener");
        let url = format!("http://{}", listener.local_addr().expect("proxy addr"));
        let accepted = Arc::new(AtomicUsize::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let handle = {
            let (accepted, stop) = (accepted.clone(), stop.clone());
            thread::spawn(move || {
                // Bounded like the mock's own loop, so a leaked proxy stops.
                let end = Instant::now() + Duration::from_secs(300);
                while !stop.load(Ordering::SeqCst) && Instant::now() < end {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            accepted.fetch_add(1, Ordering::SeqCst);
                            drop(stream);
                        }
                        Err(_) => thread::sleep(Duration::from_millis(5)),
                    }
                }
            })
        };
        Self {
            url,
            accepted,
            stop,
            handle: Some(handle),
        }
    }

    fn accepted(&self) -> usize {
        self.accepted.load(Ordering::SeqCst)
    }
}

impl Drop for CountingProxy {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

/// `groove index` against a loopback endpoint, with every proxy variable
/// set and no `NO_PROXY`, reaches the endpoint and never the proxy.
///
/// Red if the embedding client stops calling `no_proxy()` for a loopback
/// endpoint: the request then goes to the proxy, which drops it.
#[test]
fn loopback_endpoint_bypasses_proxy_environment() {
    let fx = fixture(
        "groove-aw13-proxy",
        &[(
            "alpha.md",
            &note("Alpha", "The lighthouse keeper logs ships."),
        )],
        "max_retries = 0\n",
    );
    let proxy = CountingProxy::start();

    let out = index_through(&fx, &proxy);
    let stderr = stderr_of(&out);

    assert_eq!(
        proxy.accepted(),
        0,
        "the proxy saw a connection meant for a loopback endpoint:\n{stderr}"
    );
    assert!(out.status.success(), "groove index failed:\n{stderr}");
    assert!(
        !fx.mock.requests().is_empty(),
        "the loopback endpoint was never reached:\n{stderr}"
    );
}

/// The other half of the rule: an endpoint that is not loopback still goes
/// through the proxy the environment names.
///
/// `.invalid` never resolves (RFC 6761), so the only way a connection reaches
/// the listener is a request the client routed through it. `groove index` is
/// expected to fail, since the stand-in proxy drops what it accepts.
///
/// Red if every endpoint gets `no_proxy()`: the client then looks the name up
/// itself, fails, and the proxy sees nothing.
#[test]
fn remote_endpoint_still_uses_proxy_environment() {
    let fx = fixture(
        "groove-aw13-remote",
        &[(
            "alpha.md",
            &note("Alpha", "The lighthouse keeper logs ships."),
        )],
        "max_retries = 0\n",
    );
    let remote = format!(
        "http://embed.invalid:{}/v1/embeddings",
        fx.mock.addr().port()
    );
    let toml = std::fs::read_to_string(&fx.config).expect("read groove.toml");
    assert!(
        toml.contains(&fx.mock.endpoint()),
        "groove.toml does not name the mock endpoint:\n{toml}"
    );
    std::fs::write(&fx.config, toml.replace(&fx.mock.endpoint(), &remote))
        .expect("rewrite groove.toml");
    let proxy = CountingProxy::start();

    let out = index_through(&fx, &proxy);
    let stderr = stderr_of(&out);

    assert!(
        proxy.accepted() >= 1,
        "a non-loopback endpoint bypassed the proxy:\n{stderr}"
    );
    assert!(
        fx.mock.requests().is_empty(),
        "the mock was reached directly:\n{stderr}"
    );
}

/// `groove index` on `fx`'s knowledge base with every proxy variable (both
/// cases) pointed at `proxy` and no `NO_PROXY`, whatever the exit code.
fn index_through(fx: &Fixture, proxy: &CountingProxy) -> Output {
    let mut cmd = fx.cmd();
    for var in ["NO_PROXY", "no_proxy"] {
        cmd.env_remove(var);
    }
    for var in [
        "HTTP_PROXY",
        "http_proxy",
        "HTTPS_PROXY",
        "https_proxy",
        "ALL_PROXY",
        "all_proxy",
    ] {
        cmd.env(var, &proxy.url);
    }
    cmd.args(["index", "--kb-path"])
        .arg(fx.kb())
        .output()
        .expect("spawn groove index")
}
