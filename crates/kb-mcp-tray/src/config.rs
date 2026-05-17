#![cfg(target_os = "windows")]

use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct Config {
    pub service_name: String,
    pub bind: String,
    /// Reserved for future endpoint additions (= status_url / ui_url are
    /// pre-built from this so we don't re-derive them per call site).
    #[allow(dead_code)]
    pub base_url: String,
    pub status_url: String,
    pub ui_url: String,
}

#[derive(Deserialize, Debug, Default)]
struct RawConfig {
    #[serde(default)]
    transport: RawTransport,
}

#[derive(Deserialize, Debug, Default)]
struct RawTransport {
    #[serde(default)]
    http: RawHttp,
}

#[derive(Deserialize, Debug)]
struct RawHttp {
    #[serde(default = "default_bind")]
    bind: String,
}

impl Default for RawHttp {
    fn default() -> Self {
        Self {
            bind: default_bind(),
        }
    }
}

fn default_bind() -> String {
    "127.0.0.1:3100".to_string()
}

/// Resolve the tray's Config by reading `kb-mcp.toml` from either:
/// - `<kb_path_override>/kb-mcp.toml` (= `--kb-path` flag, rare opt-in), or
/// - `<dirs::config_dir()>/kb-mcp/<service_name>/kb-mcp.toml` (= default,
///   matches what `kb-mcp service install` wrote).
///
/// Returns Err if the toml is missing or unparseable. The tray's main.rs
/// (PR-1 skeleton) catches this and falls back to a placeholder Config for
/// debug purposes; PR-2 will switch to fail-fast (spec section 6 末尾).
pub fn resolve(service_name: &str, kb_path_override: Option<&PathBuf>) -> Result<Config> {
    let toml_path = if let Some(p) = kb_path_override {
        p.join("kb-mcp.toml")
    } else {
        dirs::config_dir()
            .ok_or_else(|| anyhow::anyhow!("config_dir not found"))?
            .join("kb-mcp")
            .join(service_name)
            .join("kb-mcp.toml")
    };

    let body = std::fs::read_to_string(&toml_path)
        .with_context(|| format!("read {}", toml_path.display()))?;
    let raw: RawConfig =
        toml::from_str(&body).with_context(|| format!("parse {}", toml_path.display()))?;
    let bind = raw.transport.http.bind;

    // (codex P2 round 2 on PR #62): admin endpoints (/ui, /api/admin/*,
    // /api/search) are loopback-only by spec (= server.rs allow-list +
    // service install warning). When the toml bind is a wildcard
    // (0.0.0.0, ::, [::]:...) or non-loopback host, deriving tray URLs
    // straight from `bind` makes polling target a URL the server rejects.
    // Rewrite the host to 127.0.0.1 while preserving the port.
    let admin_host_port = normalize_to_loopback(&bind);
    let base_url = format!("http://{admin_host_port}");
    let status_url = format!("{base_url}/api/admin/status");
    let ui_url = format!("{base_url}/ui");
    Ok(Config {
        service_name: service_name.to_string(),
        bind,
        base_url,
        status_url,
        ui_url,
    })
}

/// Pick the host the tray should talk to.
///
/// - Loopback (127.0.0.1 / localhost / ::1) → keep as-is.
/// - Wildcard (0.0.0.0 / ::) → rewrite to 127.0.0.1: the daemon
///   accepts every interface including loopback, so admin allow-list
///   passes.
/// - Specific non-loopback (e.g. `192.168.1.5:3100` from
///   `--bind 192.168.1.5:3100 --i-know`) → keep as-is. Rewriting to
///   127.0.0.1 would target an interface the daemon does NOT listen
///   on, breaking polling forever (codex P2 round 3 on PR #62).
fn normalize_to_loopback(bind: &str) -> String {
    let (host, port) = if let Some(rest) = bind.strip_prefix('[') {
        // "[host]:port" form (IPv6)
        if let Some(close) = rest.find(']') {
            let host = &rest[..close];
            let port = &rest[close + 1..].trim_start_matches(':');
            (host.to_string(), (*port).to_string())
        } else {
            return bind.to_string();
        }
    } else if let Some(idx) = bind.rfind(':') {
        (bind[..idx].to_string(), bind[idx + 1..].to_string())
    } else {
        return bind.to_string();
    };

    let is_loopback = host == "127.0.0.1" || host == "localhost" || host == "::1";
    let is_wildcard = host == "0.0.0.0" || host == "::";

    if is_loopback {
        // IPv6 loopback needs bracket-wrapping when joined with :port.
        if host == "::1" {
            format!("[::1]:{port}")
        } else {
            format!("{host}:{port}")
        }
    } else if is_wildcard {
        format!("127.0.0.1:{port}")
    } else {
        // Specific non-loopback (e.g. 192.168.1.5): keep as-is. The
        // server's admin allow-list will likely reject this, but
        // rewriting to 127.0.0.1 would point us at an interface the
        // daemon is not listening on, which is strictly worse.
        format!("{host}:{port}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_temp_toml(body: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "kb-mcp-tray-cfg-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("kb-mcp.toml");
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(body.as_bytes()).unwrap();
        dir
    }

    #[test]
    fn resolves_default_bind_when_toml_empty() {
        let dir = write_temp_toml("");
        let cfg = resolve("kb-mcp", Some(&dir)).unwrap();
        assert_eq!(cfg.bind, "127.0.0.1:3100");
        assert_eq!(cfg.base_url, "http://127.0.0.1:3100");
        assert_eq!(cfg.status_url, "http://127.0.0.1:3100/api/admin/status");
        assert_eq!(cfg.ui_url, "http://127.0.0.1:3100/ui");
    }

    #[test]
    fn resolves_custom_bind() {
        let dir = write_temp_toml(
            r#"
[transport.http]
bind = "127.0.0.1:4242"
"#,
        );
        let cfg = resolve("kb-mcp", Some(&dir)).unwrap();
        assert_eq!(cfg.bind, "127.0.0.1:4242");
        assert_eq!(cfg.ui_url, "http://127.0.0.1:4242/ui");
        assert_eq!(cfg.status_url, "http://127.0.0.1:4242/api/admin/status");
    }

    #[test]
    fn fails_when_toml_missing() {
        let dir =
            std::env::temp_dir().join(format!("kb-mcp-tray-cfg-missing-{}", std::process::id()));
        // Intentionally do NOT create the dir.
        let result = resolve("nonexistent", Some(&dir));
        assert!(result.is_err());
    }

    #[test]
    fn wildcard_bind_normalizes_to_loopback() {
        let dir = write_temp_toml(
            r#"
[transport.http]
bind = "0.0.0.0:3100"
"#,
        );
        let cfg = resolve("kb-mcp", Some(&dir)).unwrap();
        // raw bind is preserved for diagnostics
        assert_eq!(cfg.bind, "0.0.0.0:3100");
        // but admin URLs target loopback so server allow-list accepts them
        assert_eq!(cfg.base_url, "http://127.0.0.1:3100");
        assert_eq!(cfg.status_url, "http://127.0.0.1:3100/api/admin/status");
        assert_eq!(cfg.ui_url, "http://127.0.0.1:3100/ui");
    }

    #[test]
    fn loopback_bind_passes_through() {
        assert_eq!(normalize_to_loopback("127.0.0.1:3100"), "127.0.0.1:3100");
        assert_eq!(normalize_to_loopback("localhost:8080"), "localhost:8080");
        assert_eq!(normalize_to_loopback("[::1]:3100"), "[::1]:3100");
    }

    #[test]
    fn wildcard_bind_rewrites_host_to_loopback() {
        assert_eq!(normalize_to_loopback("0.0.0.0:3100"), "127.0.0.1:3100");
        assert_eq!(normalize_to_loopback("[::]:3100"), "127.0.0.1:3100");
    }

    #[test]
    fn specific_non_loopback_bind_preserved() {
        // codex P2 round 3 on PR #62: a daemon bound to a specific NIC
        // (e.g. 192.168.1.5) is NOT listening on loopback, so rewriting
        // would break polling. Keep the bind as-is.
        assert_eq!(
            normalize_to_loopback("192.168.1.5:8080"),
            "192.168.1.5:8080"
        );
        assert_eq!(normalize_to_loopback("10.0.0.42:3100"), "10.0.0.42:3100");
    }
}
