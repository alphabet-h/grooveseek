use crate::UserEvent;
use crate::state::{AdminStatus, StatusState};
use std::time::Duration;
use tao::event_loop::EventLoopProxy;

/// How often [`run`] polls the daemon. Longer than one request may take
/// ([`crate::daemon::STATUS_TIMEOUT`]).
const POLL_INTERVAL: Duration = Duration::from_secs(5);

/// Long-running tokio task that polls `/api/admin/status` every 5 seconds and
/// publishes the resulting [`crate::state::StatusDot`]/text via the event loop proxy so the
/// tray (running on the main thread) can update its icon and menu label.
///
/// Failures increment a counter; 12 consecutive failures (= 1 minute at 5s
/// interval) flip the dot to Red. A single success resets the counter to 0
/// (= no hysteresis, spec section 6 "回復セマンティクス").
pub async fn run(status_url: String, proxy: EventLoopProxy<UserEvent>) {
    let mut state = StatusState::new();
    let mut interval = tokio::time::interval(POLL_INTERVAL);
    let client = match crate::daemon::status_client() {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("reqwest client build failed: {e}, polling task exiting");
            return;
        }
    };

    loop {
        interval.tick().await;
        match fetch(&client, &status_url).await {
            Ok(resp) => {
                tracing::debug!("polling success: indexing.active={}", resp.indexing.active);
                state.on_success(&resp);
            }
            Err(err) => {
                tracing::debug!(
                    "polling failure #{}: {}",
                    state.consecutive_failures + 1,
                    err
                );
                state.on_failure();
            }
        }
        let _ = proxy.send_event(UserEvent::StatusUpdate {
            dot: state.current_dot(),
            text: state.status_text().to_string(),
        });
    }
}

async fn fetch(client: &reqwest::Client, url: &str) -> anyhow::Result<AdminStatus> {
    let resp = client.get(url).send().await?;
    let status = resp.status();
    if !status.is_success() {
        anyhow::bail!("HTTP {}", status);
    }
    Ok(resp.json().await?)
}

/// (AX-42) [`fetch`] against a stand-in daemon on loopback, through the client the polling
/// loop uses ([`crate::daemon::status_client`]): each way it answers, and each way it fails.
#[cfg(test)]
mod tests {
    use super::{AdminStatus, POLL_INTERVAL, fetch};
    use crate::daemon::STATUS_TIMEOUT;
    use crate::test_support::{
        close_on_accept, hold_connections, json_response, serve_one_response,
    };

    fn status_url(addr: std::net::SocketAddr) -> String {
        format!("http://{addr}/api/admin/status")
    }

    async fn fetch_from(response: String) -> anyhow::Result<AdminStatus> {
        let addr = serve_one_response(response);
        let client = crate::daemon::status_client().expect("client");
        fetch(&client, &status_url(addr)).await
    }

    /// The `reqwest::Error` under `err`, which every failure but a status carries.
    fn transport_error(err: &anyhow::Error) -> &reqwest::Error {
        err.downcast_ref::<reqwest::Error>()
            .unwrap_or_else(|| panic!("not a reqwest error: {err:#}"))
    }

    /// A success answer is read into the status, both fields the tray uses.
    #[tokio::test]
    async fn a_success_answer_is_read() {
        let status = fetch_from(json_response(
            "200 OK",
            r#"{"daemon":{"pid":4242},"indexing":{"active":true}}"#,
        ))
        .await
        .expect("a 200 with a status body is a success");
        assert!(status.indexing.active);
        assert_eq!(status.daemon.pid, Some(4242));
    }

    /// A daemon that reports less than the tray reads (an older release) is still a success,
    /// the missing fields at their defaults.
    #[tokio::test]
    async fn a_partial_answer_takes_the_defaults() {
        let status = fetch_from(json_response("200 OK", "{}"))
            .await
            .expect("an empty object is a success");
        assert!(!status.indexing.active);
        assert_eq!(status.daemon.pid, None);
    }

    /// A status that is not a success fails with the status, whatever the body: the body here
    /// would parse as a status.
    #[tokio::test]
    async fn a_failure_status_fails_whatever_the_body() {
        for status in [
            "503 Service Unavailable",
            "404 Not Found",
            "500 Internal Server Error",
        ] {
            let err = fetch_from(json_response(status, r#"{"indexing":{"active":true}}"#))
                .await
                .expect_err("a non-success status is a failure");
            assert_eq!(err.to_string(), format!("HTTP {status}"));
            assert!(err.downcast_ref::<reqwest::Error>().is_none(), "{err:#}");
        }
    }

    /// A success whose body is not a status fails as a decode error, whether it is not JSON at
    /// all, JSON of the wrong shape, or empty.
    #[tokio::test]
    async fn a_body_that_is_not_a_status_fails_to_decode() {
        for body in ["<html>busy</html>", r#"{"indexing":{"active":"yes"}}"#, ""] {
            let err = fetch_from(json_response("200 OK", body))
                .await
                .expect_err("a body that is not a status is a failure");
            assert!(transport_error(&err).is_decode(), "{body:?}: {err:#}");
        }
    }

    /// A daemon that accepts the connection and closes it without answering: a transport
    /// failure, neither a status, a decode error nor a timeout.
    ///
    /// The listener is this test's own ([`close_on_accept`]), so no test running beside it
    /// can take the port, as one could a port bound and released for "nothing listening".
    #[tokio::test]
    async fn a_daemon_that_closes_without_answering_is_a_transport_failure() {
        let addr = close_on_accept();
        let client = crate::daemon::status_client().expect("client");
        let err = fetch(&client, &status_url(addr))
            .await
            .expect_err("a connection closed without an answer is a failure");
        let source = transport_error(&err);
        assert!(
            !source.is_decode() && !source.is_status() && !source.is_timeout(),
            "{err:#}"
        );
    }

    /// How far past [`STATUS_TIMEOUT`] a timed-out request may come back: room for the
    /// scheduling of a loaded Windows CI runner, not for a longer timeout.
    const TIMEOUT_SLACK: std::time::Duration = std::time::Duration::from_secs(2);

    /// A daemon that accepts and never answers fails on the client's timeout rather than
    /// holding the polling loop: no sooner than [`STATUS_TIMEOUT`], and no later than
    /// [`TIMEOUT_SLACK`] after it.
    #[tokio::test]
    async fn a_daemon_that_never_answers_times_out() {
        let addr = hold_connections();
        let client = crate::daemon::status_client().expect("client");
        let started = std::time::Instant::now();
        let err = fetch(&client, &status_url(addr))
            .await
            .expect_err("no answer is a failure");
        let took = started.elapsed();
        assert!(transport_error(&err).is_timeout(), "{err:#}");
        assert!(
            took >= STATUS_TIMEOUT,
            "came back after {took:?}, before the {STATUS_TIMEOUT:?} timeout"
        );
        assert!(
            took <= STATUS_TIMEOUT + TIMEOUT_SLACK,
            "came back after {took:?}, past the {STATUS_TIMEOUT:?} timeout and {TIMEOUT_SLACK:?}"
        );
    }

    /// A request that times out ends before the next poll is due, so a daemon that stops
    /// answering costs one failure per poll and the polls do not pile up behind it.
    #[test]
    fn a_request_times_out_before_the_next_poll() {
        assert!(
            STATUS_TIMEOUT < POLL_INTERVAL,
            "{STATUS_TIMEOUT:?} is not shorter than {POLL_INTERVAL:?}"
        );
    }
}
