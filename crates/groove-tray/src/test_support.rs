//! Shared pieces for this crate's unit tests.
//!
//! Declared by **both** `lib.rs` and `main.rs`: `install.rs` belongs to the
//! library target and `daemon.rs` / `process.rs` to the binary, so a helper
//! either lives in one source file that both targets compile, or gets written
//! twice. This is the former.
//!
//! Mirrors `grooveseek/src/test_support.rs`. The two cannot be shared — `groove`
//! depends on this crate, so a dependency the other way would be a cycle.

/// A suffix that is unique across processes *and* within one process.
///
/// PID + nanos, which these tests grew independently, is not enough: `cargo
/// test` runs a binary's tests on parallel threads of a **single** process, so
/// two can read the same nanosecond, derive the same path, and delete each
/// other's files on `Drop`. `grooveseek/tests/common/temp.rs` carries the same
/// counter, added after that collision was observed on a Windows CI runner.
pub fn unique_suffix() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    suffix_for(std::process::id(), nanos)
}

static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// The part of [`unique_suffix`] that does not read the clock, split out so the
/// counter can be tested for what it is for. Asserting that two `unique_suffix`
/// calls differ proves nothing — the real clock advances between them, so that
/// passes with the counter removed.
fn suffix_for(pid: u32, nanos: u128) -> String {
    let seq = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    format!("{pid}-{nanos}-{seq}")
}

/// [`unique_suffix`] applied to a scratch path directly under the temp dir.
pub fn unique_temp_path(prefix: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!("{prefix}-{}", unique_suffix()))
}

/// A stand-in daemon for one request: bind a loopback port, and on a background thread accept
/// one connection, read the request up to the end of its headers and write `response`, the
/// whole HTTP response as bytes on the wire, then close. Returns the bound address.
///
/// The accept has no timeout: if no connection comes, the thread blocks until the test process
/// exits. Once a connection is accepted, each read of the request gives up after 3 seconds
/// without data, and the thread then ends without answering; the test that started it fails on
/// its own terms.
pub fn serve_one_response(response: String) -> std::net::SocketAddr {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind stand-in daemon");
    let addr = listener.local_addr().expect("stand-in daemon addr");
    std::thread::spawn(move || {
        let Ok((mut stream, _)) = listener.accept() else {
            return;
        };
        let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(3)));
        let mut buf = Vec::new();
        let mut chunk = [0u8; 1024];
        while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
            match stream.read(&mut chunk) {
                Ok(0) | Err(_) => return,
                Ok(n) => buf.extend_from_slice(&chunk[..n]),
            }
        }
        let _ = stream.write_all(response.as_bytes());
    });
    addr
}

/// The response [`serve_one_response`] takes for `status` (the status line after
/// `HTTP/1.1 `) with a JSON `body`, the connection closed after it.
pub fn json_response(status: &str, body: &str) -> String {
    format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

/// A port that is bound and never answers: a background thread accepts every connection and
/// holds it open without reading or writing, until the test process exits. Returns the bound
/// loopback address. This is the shape of a busy daemon.
///
/// `daemon::tests::confirm_stopped_refuses_a_port_that_is_still_bound` has the same listener
/// inline in its body, which stays as it is because test bodies are not edited.
pub fn hold_connections() -> std::net::SocketAddr {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind holding listener");
    let addr = listener.local_addr().expect("holding listener addr");
    std::thread::spawn(move || {
        let mut held = Vec::new();
        for stream in listener.incoming() {
            match stream {
                Ok(s) => held.push(s),
                Err(_) => break,
            }
        }
    });
    addr
}

/// A port that is bound and answers nothing: a background thread accepts every connection and
/// closes it at once, without reading or writing, until the test process exits. Returns the
/// bound loopback address. This is the shape of a daemon that drops a request.
///
/// The listener stays bound for the life of the process, so unlike a port bound and released
/// to stand for "nothing listening", no test running beside the caller can be given it.
pub fn close_on_accept() -> std::net::SocketAddr {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind closing listener");
    let addr = listener.local_addr().expect("closing listener addr");
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            match stream {
                Ok(s) => drop(s),
                Err(_) => break,
            }
        }
    });
    addr
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The property the counter exists for, with the clock pinned so the case
    /// actually arises. Without the counter this fails.
    #[test]
    fn same_pid_and_same_nanosecond_still_differ() {
        assert_ne!(suffix_for(4242, 99), suffix_for(4242, 99));
    }

    #[test]
    fn successive_suffixes_differ() {
        assert_ne!(unique_suffix(), unique_suffix());
    }

    #[test]
    fn concurrent_suffixes_are_all_distinct() {
        let handles: Vec<_> = (0..8)
            .map(|_| std::thread::spawn(|| (0..64).map(|_| unique_suffix()).collect::<Vec<_>>()))
            .collect();
        let all: Vec<String> = handles
            .into_iter()
            .flat_map(|h| h.join().unwrap())
            .collect();
        let unique: std::collections::HashSet<&String> = all.iter().collect();
        assert_eq!(
            unique.len(),
            all.len(),
            "collision among {} suffixes",
            all.len()
        );
    }

    /// Kept in lockstep with `grooveseek/src/test_support.rs`, which cannot be
    /// imported here.
    #[test]
    fn temp_path_carries_the_prefix_and_is_unique() {
        let p = unique_temp_path("groove-tray-probe");
        assert!(p.starts_with(std::env::temp_dir()));
        let name = p.file_name().unwrap().to_string_lossy().to_string();
        assert!(
            name.starts_with("groove-tray-probe-"),
            "unexpected name: {name}"
        );
        assert_ne!(unique_temp_path("groove-tray-probe"), p);
    }
}
