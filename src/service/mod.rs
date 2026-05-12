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

/// service-name は path-safe / unit-naming-safe にするため `[a-zA-Z0-9_-]+` のみ受け付ける。
/// 空文字 / slash / dot / 空白 / 非 ASCII は reject。spec § 1 / 8.1 (= 確定済) 参照。
pub(crate) fn validate_service_name(s: &str) -> Result<String, String> {
    if s.is_empty() {
        return Err("service-name must not be empty".into());
    }
    if !s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
        return Err(format!(
            "invalid service-name {s:?}: must match [a-zA-Z0-9_-]+"
        ));
    }
    Ok(s.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_service_name_accepts_valid() {
        assert!(validate_service_name("kb-mcp").is_ok());
        assert!(validate_service_name("work_kb").is_ok());
        assert!(validate_service_name("kb-2024").is_ok());
        assert!(validate_service_name("A").is_ok());
    }

    #[test]
    fn validate_service_name_rejects_invalid() {
        assert!(validate_service_name("").is_err());
        assert!(validate_service_name("my/kb").is_err());
        assert!(validate_service_name("kb mcp").is_err());
        assert!(validate_service_name("kb.mcp").is_err());
        assert!(validate_service_name("日本語").is_err());
    }
}
