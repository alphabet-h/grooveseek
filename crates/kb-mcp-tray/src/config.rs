#![cfg(target_os = "windows")]

use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct Config {
    pub service_name: String,
    pub bind: String,
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
    let base_url = format!("http://{bind}");
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
}
