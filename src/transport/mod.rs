//! Transport layer abstraction for the MCP server.
//!
//! The MCP server can listen on either stdio (one client at a time) or
//! Streamable HTTP (many clients simultaneously). Transport selection is
//! driven by CLI flags / `kb-mcp.toml`, resolved into a [`Transport`] enum
//! and then dispatched to the corresponding runner in [`stdio`] / [`http`].

use std::net::SocketAddr;

use anyhow::Result;
use serde::Deserialize;

pub mod http;
pub mod stdio;

// ---------------------------------------------------------------------------
// CLI / config enums
// ---------------------------------------------------------------------------

/// CLI-level transport selector.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum TransportKind {
    Stdio,
    Http,
}

/// `[transport].kind` の config 表現。`clap::ValueEnum` と独立の型に
/// しておくと config 側で deny_unknown_fields が素直に効く。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TransportKindConfig {
    Stdio,
    Http,
}

/// `[transport.http]` config.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct HttpTransportConfig {
    /// `127.0.0.1:3100` 等の SocketAddr 文字列 (bind address)。
    #[serde(default)]
    pub bind: Option<String>,

    /// 受理する `Host` ヘッダの allow-list。`None` (省略) なら rmcp の
    /// default = `["localhost", "127.0.0.1", "::1"]` (loopback only、DNS
    /// rebinding 防御) を使う。LAN / イントラ公開時は
    /// `["192.168.1.10", "kb.example.lan", ...]` のように明示する。
    /// 空 `Vec` を渡すと rmcp は **全 Host を許可** する (
    /// `disable_allowed_hosts` と同等)。public 公開時は推奨されない。
    #[serde(default)]
    pub allowed_hosts: Option<Vec<String>>,

    /// `/healthz` を `allowed_hosts` allow-list 配下に置くか (= F-64
    /// fingerprinting hardening)。`None` (省略) or `Some(true)` =
    /// 現行挙動 (`/healthz` は public、Host check なし)。`Some(false)`
    /// = `/healthz` も `allowed_hosts` で gate、non-allowlisted host
    /// から 403。default = true で backward compat 維持。
    #[serde(default)]
    pub healthz_public: Option<bool>,
}

/// `[transport]` config section.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct TransportConfig {
    #[serde(default)]
    pub kind: Option<TransportKindConfig>,
    #[serde(default)]
    pub http: Option<HttpTransportConfig>,
}

// ---------------------------------------------------------------------------
// Runtime transport choice
// ---------------------------------------------------------------------------

/// Resolved transport to use at runtime.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Transport {
    Stdio,
    Http {
        addr: SocketAddr,
        /// `None` = rmcp の default loopback-only allow-list を使う。
        /// `Some(vec)` = 明示 list (空 `Vec` を渡すと rmcp 側で全 Host
        /// 許可になる)。F-33 で `kb-mcp.toml` から surface した。
        allowed_hosts: Option<Vec<String>>,
        /// F-64: `/healthz` を `allowed_hosts` 検証配下に置くか。
        /// `true` (default) = 現行挙動 (Host check なし、public)。
        /// `false` = `/healthz` も Host check (= non-allowlisted から 403)。
        healthz_public: bool,
    },
}

const DEFAULT_HTTP_PORT: u16 = 3100;

impl Transport {
    /// Resolve `Transport` from CLI + config + defaults, in that priority order.
    ///
    /// - CLI `--transport` wins over config
    /// - `[transport.http]` 単独指定 (kind 省略) は HTTP 扱い (糖衣)
    /// - HTTP bind 解決: `--bind` (完全形) > `(127.0.0.1, --port)` > config bind > `127.0.0.1:3100`
    /// - `allowed_hosts`: `[transport.http].allowed_hosts` が指定されていれば
    ///   それ、無ければ rmcp default (loopback only) を保つ。CLI からは設定
    ///   不可 (config 専用、誤設定を防ぐ意図 — ここを CLI で渡せると public
    ///   bind 時に「うっかり全 Host 許可」が起きやすい)。
    pub fn resolve(
        cli_transport: Option<TransportKind>,
        cli_bind: Option<SocketAddr>,
        cli_port: Option<u16>,
        cfg: Option<&TransportConfig>,
    ) -> Result<Self> {
        let kind = cli_transport
            .map(|t| match t {
                TransportKind::Stdio => TransportKindConfig::Stdio,
                TransportKind::Http => TransportKindConfig::Http,
            })
            .or_else(|| cfg.and_then(|c| c.kind))
            .or_else(|| {
                // [transport.http] があれば kind 未指定でも Http と解釈
                if cfg.is_some_and(|c| c.http.is_some()) {
                    Some(TransportKindConfig::Http)
                } else {
                    None
                }
            })
            .unwrap_or(TransportKindConfig::Stdio);

        match kind {
            TransportKindConfig::Stdio => Ok(Transport::Stdio),
            TransportKindConfig::Http => {
                let addr = resolve_http_addr(cli_bind, cli_port, cfg)?;
                let allowed_hosts = cfg
                    .and_then(|c| c.http.as_ref())
                    .and_then(|h| h.allowed_hosts.clone());
                let healthz_public = cfg
                    .and_then(|c| c.http.as_ref())
                    .and_then(|h| h.healthz_public)
                    .unwrap_or(true);
                Ok(Transport::Http {
                    addr,
                    allowed_hosts,
                    healthz_public,
                })
            }
        }
    }
}

fn resolve_http_addr(
    cli_bind: Option<SocketAddr>,
    cli_port: Option<u16>,
    cfg: Option<&TransportConfig>,
) -> Result<SocketAddr> {
    if let Some(bind) = cli_bind {
        return Ok(bind);
    }
    if let Some(port) = cli_port {
        return Ok(SocketAddr::from(([127, 0, 0, 1], port)));
    }
    if let Some(bind_str) = cfg
        .and_then(|c| c.http.as_ref())
        .and_then(|h| h.bind.as_deref())
    {
        return bind_str.parse().map_err(|e| {
            anyhow::anyhow!("[transport.http].bind is not a valid SocketAddr: {bind_str}: {e}")
        });
    }
    Ok(SocketAddr::from(([127, 0, 0, 1], DEFAULT_HTTP_PORT)))
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_resolve_default_is_stdio() {
        let t = Transport::resolve(None, None, None, None).unwrap();
        assert_eq!(t, Transport::Stdio);
    }

    #[test]
    fn test_resolve_cli_http_default_bind() {
        let t = Transport::resolve(Some(TransportKind::Http), None, None, None).unwrap();
        assert_eq!(
            t,
            Transport::Http {
                addr: "127.0.0.1:3100".parse().unwrap(),
                allowed_hosts: None,
                healthz_public: true,
            }
        );
    }

    #[test]
    fn test_resolve_cli_port_only() {
        let t = Transport::resolve(Some(TransportKind::Http), None, Some(4000), None).unwrap();
        assert_eq!(
            t,
            Transport::Http {
                addr: "127.0.0.1:4000".parse().unwrap(),
                allowed_hosts: None,
                healthz_public: true,
            }
        );
    }

    #[test]
    fn test_resolve_cli_bind_full_wins() {
        let t = Transport::resolve(
            Some(TransportKind::Http),
            Some("0.0.0.0:9000".parse().unwrap()),
            Some(4000), // should be overridden by --bind
            None,
        )
        .unwrap();
        assert_eq!(
            t,
            Transport::Http {
                addr: "0.0.0.0:9000".parse().unwrap(),
                allowed_hosts: None,
                healthz_public: true,
            }
        );
    }

    #[test]
    fn test_resolve_cli_overrides_config() {
        let cfg = TransportConfig {
            kind: Some(TransportKindConfig::Http),
            http: None,
        };
        // CLI stdio wins over config http
        let t = Transport::resolve(Some(TransportKind::Stdio), None, None, Some(&cfg)).unwrap();
        assert_eq!(t, Transport::Stdio);
    }

    #[test]
    fn test_resolve_http_section_implies_http_kind() {
        // [transport.http] だけ書かれていれば kind 省略でも Http 扱い
        let cfg = TransportConfig {
            kind: None,
            http: Some(HttpTransportConfig {
                bind: Some("127.0.0.1:5555".into()),
                allowed_hosts: None,
                ..HttpTransportConfig::default()
            }),
        };
        let t = Transport::resolve(None, None, None, Some(&cfg)).unwrap();
        assert_eq!(
            t,
            Transport::Http {
                addr: "127.0.0.1:5555".parse().unwrap(),
                allowed_hosts: None,
                healthz_public: true,
            }
        );
    }

    #[test]
    fn test_resolve_config_bind_malformed_is_error() {
        let cfg = TransportConfig {
            kind: Some(TransportKindConfig::Http),
            http: Some(HttpTransportConfig {
                bind: Some("not-an-address".into()),
                allowed_hosts: None,
                ..HttpTransportConfig::default()
            }),
        };
        let err = Transport::resolve(None, None, None, Some(&cfg)).expect_err("must reject");
        assert!(err.to_string().contains("SocketAddr"));
    }

    /// F-33: `[transport.http].allowed_hosts` が toml で明示されたら
    /// それが `Transport::Http` に渡る。
    #[test]
    fn test_resolve_config_allowed_hosts_passes_through() {
        let cfg = TransportConfig {
            kind: Some(TransportKindConfig::Http),
            http: Some(HttpTransportConfig {
                bind: Some("0.0.0.0:3100".into()),
                allowed_hosts: Some(vec![
                    "kb.example.lan".to_string(),
                    "192.168.1.10".to_string(),
                ]),
                ..HttpTransportConfig::default()
            }),
        };
        let t = Transport::resolve(None, None, None, Some(&cfg)).unwrap();
        match t {
            Transport::Http {
                addr,
                allowed_hosts,
                healthz_public: _,
            } => {
                assert_eq!(addr, "0.0.0.0:3100".parse().unwrap());
                assert_eq!(
                    allowed_hosts,
                    Some(vec![
                        "kb.example.lan".to_string(),
                        "192.168.1.10".to_string()
                    ])
                );
            }
            _ => panic!("expected Transport::Http"),
        }
    }

    /// F-33: `[transport.http].allowed_hosts` の deserialize は省略可。
    /// toml に書かなければ `None` (= rmcp default loopback-only).
    #[test]
    fn test_http_transport_config_omits_allowed_hosts() {
        let toml_str = r#"bind = "127.0.0.1:3100""#;
        let cfg: HttpTransportConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(cfg.bind.as_deref(), Some("127.0.0.1:3100"));
        assert_eq!(cfg.allowed_hosts, None);
    }

    /// F-33: 配列で書けばそれが Vec<String> に解釈される。
    #[test]
    fn test_http_transport_config_parses_allowed_hosts() {
        let toml_str = r#"
            bind = "0.0.0.0:3100"
            allowed_hosts = ["kb.example.lan", "192.168.1.10"]
        "#;
        let cfg: HttpTransportConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(
            cfg.allowed_hosts,
            Some(vec![
                "kb.example.lan".to_string(),
                "192.168.1.10".to_string(),
            ])
        );
    }

    /// F-33: 空配列も valid (rmcp 側で全 Host 許可になる、operator 自己責任)。
    #[test]
    fn test_http_transport_config_allows_empty_vec() {
        let toml_str = r#"
            bind = "0.0.0.0:3100"
            allowed_hosts = []
        "#;
        let cfg: HttpTransportConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(cfg.allowed_hosts, Some(vec![]));
    }

    #[test]
    fn test_resolve_config_stdio() {
        let cfg = TransportConfig {
            kind: Some(TransportKindConfig::Stdio),
            http: None,
        };
        let t = Transport::resolve(None, None, None, Some(&cfg)).unwrap();
        assert_eq!(t, Transport::Stdio);
    }
}
