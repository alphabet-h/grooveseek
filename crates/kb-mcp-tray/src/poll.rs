#![cfg(target_os = "windows")]

use crate::UserEvent;
use crate::state::{AdminStatus, StatusState};
use std::time::Duration;
use tao::event_loop::EventLoopProxy;

/// Long-running tokio task that polls `/api/admin/status` every 5 seconds and
/// publishes the resulting StatusDot/text via the event loop proxy so the
/// tray (running on the main thread) can update its icon and menu label.
///
/// Failures increment a counter; 12 consecutive failures (= 1 minute at 5s
/// interval) flip the dot to Red. A single success resets the counter to 0
/// (= no hysteresis, spec section 6 "回復セマンティクス").
pub async fn run(status_url: String, proxy: EventLoopProxy<UserEvent>) {
    let mut state = StatusState::new();
    let mut interval = tokio::time::interval(Duration::from_secs(5));
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(3))
        .build()
    {
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
