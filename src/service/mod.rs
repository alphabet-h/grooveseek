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

use anyhow::Result;
use std::path::PathBuf;

pub mod install;
pub mod status;
pub mod uninstall;

#[cfg(target_os = "linux")]
pub mod linux;
#[cfg(target_os = "macos")]
pub mod macos;
#[cfg(target_os = "windows")]
pub mod windows;

/// install command が backend に渡す context。
pub struct InstallContext {
    pub service_name: String,
    pub kb_path: PathBuf,     // resolved (= flag or toml)
    pub bind: String,         // e.g. "127.0.0.1:3100"
    pub config_home: PathBuf, // <dirs::config_dir()>/kb-mcp/<name>/
    pub binary_path: PathBuf, // std::env::current_exe() を install 時 freeze (spec § 8.2 a)
    pub auto_start: bool,
    pub force: bool,
}

/// service の現在状態。2-tier resolve (= spec § 2 status info source):
/// 1. OS native (= systemctl / launchctl / schtasks) で running / stopped / not-found 判定
/// 2. running 時のみ `/api/admin/status` で dynamic info (uptime / model) を取得
pub enum ServiceState {
    Running {
        uptime_secs: u64,
        bind: Option<String>,
        kb_path: Option<PathBuf>,
        model: Option<String>,
    },
    Stopped {
        bind: Option<String>,
        kb_path: Option<PathBuf>,
    },
    NotFound,
}

/// platform-specific backend abstraction。Phase 4+ で --system 切替時は別 struct を増やす想定。
pub(crate) trait ServiceBackend {
    fn install(&self, ctx: &InstallContext) -> Result<()>;
    fn uninstall(&self, service_name: &str) -> Result<()>;
    fn status(&self, service_name: &str) -> Result<ServiceState>;
    fn list(&self) -> Result<Vec<(String, ServiceState)>>;
    /// uninstall で daemon 起動中を stop してから unit を消すための内部 helper。
    /// 現状は per-OS の `uninstall` impl が自前で stop しており unused だが、
    /// Phase 4+ の `--system` 切替 / 明示的 stop subcommand 追加時に使う想定。
    #[allow(dead_code)]
    fn stop(&self, service_name: &str) -> Result<()>;
}

/// Host-OS の `ServiceBackend` を構築する factory。
/// cfg(target_os = ...) で一つだけ branch が compile される。
pub(crate) fn backend() -> Box<dyn ServiceBackend> {
    #[cfg(target_os = "linux")]
    {
        Box::new(linux::SystemdUser)
    }
    #[cfg(target_os = "macos")]
    {
        Box::new(macos::LaunchAgent)
    }
    #[cfg(target_os = "windows")]
    {
        Box::new(windows::TaskScheduler)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    {
        compile_error!("kb-mcp service install is only supported on Linux / macOS / Windows")
    }
}

/// `<config_dir>/kb-mcp/<service-name>/` を返す。
/// 優先順: (1) `KB_MCP_CONFIG_HOME` env var、(2) `dirs::config_dir()` (= XDG_CONFIG_HOME / OS 標準)。
pub(crate) fn resolve_config_home(service_name: &str) -> Result<PathBuf> {
    let base = std::env::var("KB_MCP_CONFIG_HOME")
        .ok()
        .map(PathBuf::from)
        .or_else(dirs::config_dir)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "config dir 解決失敗 (KB_MCP_CONFIG_HOME / XDG_CONFIG_HOME / HOME いずれも未設定)"
            )
        })?;
    Ok(base.join("kb-mcp").join(service_name))
}

/// service-name は path-safe / unit-naming-safe にするため `[a-zA-Z0-9_-]+` のみ受け付ける。
/// 空文字 / slash / dot / 空白 / 非 ASCII は reject。spec § 1 / 8.1 (= 確定済) 参照。
pub fn validate_service_name(s: &str) -> Result<String, String> {
    if s.is_empty() {
        return Err("service-name must not be empty".into());
    }
    if !s
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
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

    #[test]
    fn resolve_config_home_uses_env_var_when_set() {
        let original = std::env::var("KB_MCP_CONFIG_HOME").ok();
        // SAFETY: edition 2024 made env mutation unsafe due to thread-safety
        // 懸念。本 test は service::tests 内に閉じており他 env mutation と並走しない。
        unsafe {
            std::env::set_var("KB_MCP_CONFIG_HOME", "/tmp/kb-mcp-test-override");
        }
        let result = resolve_config_home("svc").unwrap();
        assert_eq!(
            result,
            PathBuf::from("/tmp/kb-mcp-test-override/kb-mcp/svc")
        );
        unsafe {
            match original {
                Some(v) => std::env::set_var("KB_MCP_CONFIG_HOME", v),
                None => std::env::remove_var("KB_MCP_CONFIG_HOME"),
            }
        }
    }
}
