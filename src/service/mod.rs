//! Cross-platform OS service installer for kb-mcp daemon.
//!
//! Phase 1 (user-level only) per feature-43 spec. Phase 4+ で `--system` flag
//! を追加して system-level (= Linux systemd-system / macOS LaunchDaemon /
//! Windows SCM via windows-service crate) に対応予定。
//!
//! Backend abstraction (`ServiceBackend` trait) で OS 差分を吸収:
//! - Linux: systemd-user (`~/.config/systemd/user/<name>.service`)
//! - macOS: LaunchAgent (`~/Library/LaunchAgents/com.kb-mcp.<name>.plist`)
//! - Windows: Task Scheduler AT_LOGON trigger (admin 不要、H-8 personal-http と一致)
//!
//! 3rd-party tool (NSSM / WiX) は使わず、Rust crate のみで完結 (= "1 binary value prop")。
