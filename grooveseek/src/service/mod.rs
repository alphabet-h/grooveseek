//! Cross-platform OS service installer for groove daemon.
//!
//! Phase 1 (user-level only) per feature-43 spec. Phase 4+ で `--system` flag
//! を追加して system-level (= Linux systemd-system / macOS LaunchDaemon /
//! Windows SCM via windows-service crate) に対応予定。
//!
//! Backend abstraction (`ServiceBackend` trait) で OS 差分を吸収:
//! - Linux: systemd-user (`~/.config/systemd/user/<name>.service`)
//! - macOS: LaunchAgent (`~/Library/LaunchAgents/com.groove.<name>.plist`)
//! - Windows: Task Scheduler AT_LOGON trigger (admin 不要、H-8 personal-http と一致)
//!
//! 3rd-party tool (NSSM / WiX) は使わず、Rust crate のみで完結 (= "1 binary value prop")。

use anyhow::Result;
use std::path::PathBuf;

pub mod install;
pub mod powershell;
pub mod render;
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
    pub config_home: PathBuf, // <dirs::config_dir()>/groove/<name>/
    pub binary_path: PathBuf, // std::env::current_exe() を install 時 freeze (spec § 8.2 a)
    pub auto_start: bool,
    pub force: bool,
}

/// service の現在状態。2-tier resolve (= spec § 2 status info source):
/// 1. OS native (= systemctl / launchctl / schtasks) で running / stopped / not-found 判定
/// 2. running 時のみ `/api/admin/status` で dynamic info (uptime / model) を取得
///
/// `Debug` / `PartialEq` は `status::enrich_from_toml_str` のテストが結果を
/// そのまま突き合わせるために derive してある (どのフィールドが toml から
/// 埋まったかを、表示文字列を経由せずに検査したい)。
#[derive(Debug, PartialEq)]
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
        compile_error!("groove service install is only supported on Linux / macOS / Windows")
    }
}

/// `<config_dir>/groove/<service-name>/` を返す。
/// 優先順: (1) `GROOVE_CONFIG_HOME` env var、(2) `dirs::config_dir()` (= XDG_CONFIG_HOME / OS 標準)。
pub(crate) fn resolve_config_home(service_name: &str) -> Result<PathBuf> {
    resolve_config_home_in(crate::config::env_dir("GROOVE_CONFIG_HOME"), service_name)
}

/// [`resolve_config_home`] の、環境変数を**引数で受ける**版。
///
/// **env を読むのは呼び出し側 1 箇所だけ**にするためにある。テストが
/// `GROOVE_CONFIG_HOME` を `set_var` で立てると、同じプロセスで並走している
/// テストにも見える — そして `TrustRoots::from_env` が**同じ変数を trust root
/// として読む** (`config.rs`)。つまり env を立てたテストの隣で `discover()` を
/// 呼んでいるテストの**信頼判定が変わる**。落ちるのは env を触った方ではない。
///
/// `config.rs` は既にこの形で、`discover` が `discover_in` に
/// `TrustRoots::from_env()` を渡すだけになっている。ここが最後の 1 箇所だった。
pub(crate) fn resolve_config_home_in(
    config_home: Option<PathBuf>,
    service_name: &str,
) -> Result<PathBuf> {
    // 空文字は未設定として扱う (BU-07) — `env_dir` の責務。通してしまうと base が
    // 空になり、`base.join("groove").join(name)` が **CWD 相対**のパスになる =
    // service の config をカレントディレクトリ配下に書き、起動のたびに違う場所を見る。
    let base = config_home.or_else(dirs::config_dir).ok_or_else(|| {
        anyhow::anyhow!(
            "config dir 解決失敗 (GROOVE_CONFIG_HOME / XDG_CONFIG_HOME / HOME いずれも未設定)"
        )
    })?;
    Ok(base.join("groove").join(service_name))
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
        assert!(validate_service_name("groove").is_ok());
        assert!(validate_service_name("work_kb").is_ok());
        assert!(validate_service_name("kb-2024").is_ok());
        assert!(validate_service_name("A").is_ok());
    }

    #[test]
    fn validate_service_name_rejects_invalid() {
        assert!(validate_service_name("").is_err());
        assert!(validate_service_name("my/kb").is_err());
        assert!(validate_service_name("groove seek").is_err());
        assert!(validate_service_name("groove.seek").is_err());
        assert!(validate_service_name("日本語").is_err());
    }

    /// The same assertion this test always made, reached without mutating the
    /// process environment.
    ///
    /// It used to `set_var("GROOVE_CONFIG_HOME", …)` and put it back afterwards,
    /// with a SAFETY note claiming nothing else mutated env beside it. True as
    /// far as it went, and beside the point: the hazard is not another *writer*,
    /// it is every concurrent *reader*. `TrustRoots::from_env` reads this same
    /// variable to decide which directories are trusted, so any test calling
    /// `Config::discover()` while this one held the variable would have seen
    /// `/tmp/groove-test-override` as a trust root — and failed somewhere else,
    /// intermittently, for a reason not visible from where it failed.
    #[test]
    fn resolve_config_home_uses_env_var_when_set() {
        let result =
            resolve_config_home_in(Some(PathBuf::from("/tmp/groove-test-override")), "svc")
                .unwrap();
        assert_eq!(
            result,
            PathBuf::from("/tmp/groove-test-override/groove/svc")
        );
    }

    /// With nothing supplied it falls back to the OS config directory, and the
    /// service name is still the last component. The base is whatever this
    /// machine reports, so the shape is what can be asserted.
    #[test]
    fn resolve_config_home_falls_back_to_the_os_config_dir() {
        let Some(expected_base) = dirs::config_dir() else {
            // No HOME / XDG_CONFIG_HOME: the function is documented to fail.
            assert!(resolve_config_home_in(None, "svc").is_err());
            return;
        };
        assert_eq!(
            resolve_config_home_in(None, "svc").unwrap(),
            expected_base.join("groove").join("svc")
        );
    }

    /// The wrapper still reads the variable.
    ///
    /// Every other test here calls `resolve_config_home_in` directly, so none
    /// of them notices if `resolve_config_home` stops passing `env_dir(…)` —
    /// the wiring could be cut and the suite would stay green. This compares
    /// the two, which catches that.
    ///
    /// **It only bites where the variable is set.** With it unset both sides
    /// take the `dirs::config_dir()` fallback and agree no matter what the
    /// wrapper passes, so on an ordinary CI run this asserts nothing. Run it as
    /// `GROOVE_CONFIG_HOME=/tmp/probe cargo test --lib service::tests` to
    /// exercise it — reading the variable the harness was started with is not
    /// mutation, and does not reach the tests running alongside.
    #[test]
    fn the_wrapper_passes_the_environment_through() {
        assert_eq!(
            resolve_config_home("svc").unwrap(),
            resolve_config_home_in(crate::config::env_dir("GROOVE_CONFIG_HOME"), "svc").unwrap(),
        );
    }

    /// (BU-07) Why `env_dir` filters an empty value: an empty base makes the
    /// join relative, so a service would write its config under whatever
    /// directory it happened to start in and read a different one next time.
    ///
    /// **This pins the consequence, not the filter.** The filter only runs on a
    /// real environment variable, and reaching it from here would mean setting
    /// one — which is the thing this whole change removes. What is asserted is
    /// that an empty base really does produce a relative path, so the filter
    /// has something to prevent. `GROOVE_CONFIG_HOME= cargo test --lib
    /// service::tests` reaches the filter itself, through
    /// `the_wrapper_passes_the_environment_through`.
    #[test]
    fn an_empty_base_would_be_a_relative_path() {
        let relative = resolve_config_home_in(Some(PathBuf::new()), "svc").unwrap();
        assert!(
            relative.is_relative(),
            "an empty base produced {relative:?}; this is what env_dir exists to stop"
        );
        assert_eq!(
            crate::config::env_dir("GROOVE_CONFIG_HOME_DEFINITELY_UNSET_a9f3"),
            None,
            "an unset variable is None, so the fallback is what runs"
        );
    }
}
