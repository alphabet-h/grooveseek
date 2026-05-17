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

    // (codex P2 rounds 2-4 on PR #62): admin endpoints (/ui,
    // /api/admin/*, /api/search) are loopback-only by spec, so the tray
    // always targets 127.0.0.1:<port> regardless of the daemon's bind.
    // - Wildcard binds (0.0.0.0 / ::): daemon listens on loopback too,
    //   so loopback polling succeeds. No warning.
    // - Specific non-loopback binds (e.g. 192.168.1.5): daemon does NOT
    //   listen on loopback, so loopback polling will fail with
    //   "connection refused". The user is expected to use `--with-tray`
    //   together with a loopback-capable bind (loopback or wildcard);
    //   we emit a warning so the misconfiguration is discoverable from
    //   the tray log.
    let admin_host_port = normalize_to_loopback_with_warning(&bind);
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

/// Build the host:port the tray should talk to for admin polling.
/// Always returns a loopback host (127.0.0.1 / [::1]) since admin routes
/// are loopback-only. Logs a warning for specific non-loopback binds —
/// in that case the daemon is NOT listening on loopback and polling
/// will fail with "connection refused", which is a `--with-tray` user
/// misconfiguration (= daemon should be bound to loopback or wildcard).
fn normalize_to_loopback_with_warning(bind: &str) -> String {
    let host_port = normalize_to_loopback(bind);
    let host = host_of(bind);
    let is_loopback = host == "127.0.0.1" || host == "localhost" || host == "::1";
    let is_wildcard = host == "0.0.0.0" || host == "::";
    if !is_loopback && !is_wildcard {
        tracing::warn!(
            "daemon bind '{bind}' is specific non-loopback; tray polls 127.0.0.1 \
             but the daemon does not listen there. Either change the daemon bind \
             to loopback (127.0.0.1) or wildcard (0.0.0.0), or remove --with-tray.",
        );
    }
    host_port
}

/// Pure split + loopback rewrite. Always returns a loopback host:port.
fn normalize_to_loopback(bind: &str) -> String {
    let (host, port) = split_host_port(bind);
    let is_loopback = host == "127.0.0.1" || host == "localhost" || host == "::1";
    if is_loopback {
        if host == "::1" {
            format!("[::1]:{port}")
        } else {
            format!("{host}:{port}")
        }
    } else {
        // Wildcard AND specific non-loopback both fall here: tray must
        // target a loopback URL because admin routes enforce loopback.
        // Wildcard daemon listens on loopback, so polling succeeds.
        // Specific non-loopback daemon does NOT — caller logs a warning.
        format!("127.0.0.1:{port}")
    }
}

fn split_host_port(bind: &str) -> (String, String) {
    if let Some(rest) = bind.strip_prefix('[') {
        if let Some(close) = rest.find(']') {
            let host = rest[..close].to_string();
            let port = rest[close + 1..].trim_start_matches(':').to_string();
            return (host, port);
        }
        return (bind.to_string(), String::new());
    }
    if let Some(idx) = bind.rfind(':') {
        return (bind[..idx].to_string(), bind[idx + 1..].to_string());
    }
    (bind.to_string(), String::new())
}

fn host_of(bind: &str) -> String {
    split_host_port(bind).0
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
    fn specific_non_loopback_bind_rewrites_to_loopback() {
        // codex P2 round 4 on PR #62: tray admin routes are loopback-only,
        // so the URL always targets 127.0.0.1. A daemon bound to a
        // specific NIC (not loopback, not wildcard) makes polling fail
        // with "connection refused" — that's a misconfiguration the
        // caller surfaces via tracing::warn! in
        // normalize_to_loopback_with_warning.
        assert_eq!(normalize_to_loopback("192.168.1.5:8080"), "127.0.0.1:8080");
        assert_eq!(normalize_to_loopback("10.0.0.42:3100"), "127.0.0.1:3100");
    }
}
