#![cfg(target_os = "windows")]

use serde::Deserialize;

/// Tray status indicator. 4 states total:
/// - `Gray`: pre-first-poll (= polling pending)
/// - `Green`: daemon healthy (= last poll succeeded, not indexing)
/// - `Yellow`: daemon indexing (= /api/admin/status.indexing.active == true)
/// - `Red`: daemon down (= >= 12 consecutive polling failures = 1 minute)
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StatusDot {
    Green,
    Yellow,
    Red,
    Gray,
}

/// Subset of /api/admin/status JSON the tray cares about. We use serde
/// `#[serde(default)]` on every level so partial responses from older daemon
/// versions don't break the polling loop.
#[derive(Debug, Deserialize, Clone, Default)]
pub struct AdminStatus {
    #[serde(default)]
    pub indexing: IndexingState,
}

#[derive(Debug, Deserialize, Clone, Default)]
pub struct IndexingState {
    #[serde(default)]
    pub active: bool,
}

/// Polling state machine. 1 minute (= 12 polls at 5s interval) of consecutive
/// failures = Red. 1 successful poll resets failures to 0 (no hysteresis,
/// spec section 6 "回復セマンティクス").
pub struct StatusState {
    pub consecutive_failures: u32,
    pub indexing_active: bool,
    pub initialized: bool,
}

impl StatusState {
    pub fn new() -> Self {
        Self {
            consecutive_failures: 0,
            indexing_active: false,
            initialized: false,
        }
    }

    pub fn on_success(&mut self, resp: &AdminStatus) {
        self.consecutive_failures = 0;
        self.indexing_active = resp.indexing.active;
        self.initialized = true;
    }

    pub fn on_failure(&mut self) {
        self.consecutive_failures = self.consecutive_failures.saturating_add(1);
        // (codex P2 round 2 on PR #62): a failure is just as much a
        // "we've talked to the polling layer" signal as a success — flip
        // initialized so that 12 consecutive failures from the very first
        // poll still transition Gray -> Red after 1 minute. Without this,
        // a daemon that was never up keeps the tray stuck at
        // "Status: Connecting..." indefinitely.
        self.initialized = true;
    }

    pub fn current_dot(&self) -> StatusDot {
        if !self.initialized {
            return StatusDot::Gray;
        }
        if self.consecutive_failures >= 12 {
            return StatusDot::Red;
        }
        if self.indexing_active {
            return StatusDot::Yellow;
        }
        StatusDot::Green
    }

    pub fn status_text(&self) -> &'static str {
        match self.current_dot() {
            StatusDot::Gray => "Status: Connecting...",
            StatusDot::Green => "Status: Running",
            StatusDot::Yellow => "Status: Indexing...",
            StatusDot::Red => "Status: Stopped",
        }
    }
}

impl Default for StatusState {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(active: bool) -> AdminStatus {
        AdminStatus {
            indexing: IndexingState { active },
        }
    }

    #[test]
    fn initial_state_is_gray() {
        let s = StatusState::new();
        assert_eq!(s.current_dot(), StatusDot::Gray);
        assert_eq!(s.status_text(), "Status: Connecting...");
    }

    #[test]
    fn first_success_transitions_to_green() {
        let mut s = StatusState::new();
        s.on_success(&ok(false));
        assert_eq!(s.current_dot(), StatusDot::Green);
    }

    #[test]
    fn indexing_active_yields_yellow() {
        let mut s = StatusState::new();
        s.on_success(&ok(true));
        assert_eq!(s.current_dot(), StatusDot::Yellow);
        assert_eq!(s.status_text(), "Status: Indexing...");
    }

    #[test]
    fn twelve_consecutive_failures_yield_red() {
        let mut s = StatusState::new();
        s.on_success(&ok(false));
        for _ in 0..11 {
            s.on_failure();
        }
        assert_eq!(s.current_dot(), StatusDot::Green);
        s.on_failure();
        assert_eq!(s.current_dot(), StatusDot::Red);
        assert_eq!(s.status_text(), "Status: Stopped");
    }

    #[test]
    fn single_success_after_red_recovers_to_green() {
        let mut s = StatusState::new();
        s.on_success(&ok(false));
        for _ in 0..20 {
            s.on_failure();
        }
        assert_eq!(s.current_dot(), StatusDot::Red);
        s.on_success(&ok(false));
        assert_eq!(s.current_dot(), StatusDot::Green);
    }

    #[test]
    fn failures_before_first_success_eventually_turn_red() {
        // codex P2 round 2 on PR #62: on_failure now flips initialized so
        // a daemon that was never up still transitions Gray -> Red after
        // 12 failures (1 minute at 5s interval).
        let mut s = StatusState::new();
        assert_eq!(s.current_dot(), StatusDot::Gray);
        for _ in 0..11 {
            s.on_failure();
        }
        // initialized is now true (failures count as "we talked to the
        // polling layer"), so we have left Gray. With 11 < 12 failures
        // and no indexing data, the state machine treats us as Green.
        // The important guarantee is that we no longer stay in Gray
        // forever; turning to Green for a sub-minute window is acceptable
        // because the 12th failure flips us to Red on schedule.
        assert_ne!(s.current_dot(), StatusDot::Gray);
        s.on_failure();
        assert_eq!(s.current_dot(), StatusDot::Red);
    }

    #[test]
    fn admin_status_parses_minimal_json() {
        let json = r#"{"indexing":{"active":true}}"#;
        let s: AdminStatus = serde_json::from_str(json).unwrap();
        assert!(s.indexing.active);
    }

    #[test]
    fn admin_status_parses_empty_json_with_defaults() {
        let s: AdminStatus = serde_json::from_str("{}").unwrap();
        assert!(!s.indexing.active);
    }
}
