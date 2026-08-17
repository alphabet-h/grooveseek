//! Transport layer abstraction for the MCP server.
//!
//! The MCP server can listen on either stdio (one client at a time) or
//! Streamable HTTP (many clients simultaneously). Transport selection is
//! driven by CLI flags / `groove.toml`, resolved into a [`Transport`] enum
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

    /// ブラウザが付ける `Origin` ヘッダの allow-list。
    /// `None` (省略) = **bind した port の loopback origin**
    /// (`http://localhost:<port>` / `http://127.0.0.1:<port>` /
    /// `http://[::1]:<port>`) を許可する。reverse proxy や別アプリ越しに
    /// 公開するなら、**ブラウザが実際に送る公開 origin**
    /// (`https://kb.example.com` 等) をここに明示する。
    ///
    /// MCP 仕様 2025-06-18 (Streamable HTTP / Security Warning) は
    /// *"Servers **MUST** validate the `Origin` header on all incoming
    /// connections to prevent DNS rebinding attacks"* と定めている。
    /// rmcp の既定は空リスト = **検証しない** なので、既定値を空にはしない。
    /// 空 `Vec` を明示すると検証は無効になる (起動時に warn を出す)。
    ///
    /// **認証ではない。** `Origin` を送らない要求 (MCP クライアント / tray /
    /// curl) は RFC 6454 と rmcp の仕様どおり素通りする。これで防げるのは
    /// 「利用者のブラウザに載った別サイトの JS」だけである。
    #[serde(default)]
    pub allowed_origins: Option<Vec<String>>,

    /// `/healthz` を `allowed_hosts` allow-list 配下に置くか (F-64)。
    /// `None` (省略) or `Some(true)` = 現行挙動 (`/healthz` は public、
    /// Host check なし)。`Some(false)` = `/healthz` も `allowed_hosts` で
    /// gate、non-allowlisted host から 403。default = true で backward
    /// compat 維持。
    ///
    /// **認証ではない**。ここで検査するのは呼び出し元が自由に付けられる
    /// Host header なので、ポートに到達できて許可値を送れば 200 が返る。
    /// 偶発的な探索と DNS rebinding の敷居を上げるだけで、「未知の相手に
    /// 存在を知られない」とは言えない (かつてそう書いていた)。
    #[serde(default)]
    pub healthz_public: Option<bool>,

    /// 同時に生きていられる MCP session の上限 (BU-32)。
    /// `None` (省略) = 既定の
    /// [`DEFAULT_MAX_SESSIONS`](crate::transport::http::DEFAULT_MAX_SESSIONS)、
    /// `0` = 無制限。上限に達している間、新規 session の要求は 429 で断られる
    /// (既存 session は影響を受けない)。
    #[serde(default)]
    pub max_sessions: Option<u32>,
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
        /// 許可になる)。F-33 で `groove.toml` から surface した。
        allowed_hosts: Option<Vec<String>>,
        /// `None` = bind した port の loopback origin を既定として組み立てる
        /// ([`http::default_allowed_origins`](crate::transport::http::default_allowed_origins))。
        /// `Some(vec)` = 明示 list。空 `Vec` は rmcp 側で **Origin 検証無効**
        /// になるので、既定にはしない。
        allowed_origins: Option<Vec<String>>,
        /// F-64: `/healthz` を `allowed_hosts` 検証配下に置くか。
        /// `true` (default) = 現行挙動 (Host check なし、public)。
        /// `false` = `/healthz` も Host check (= non-allowlisted から 403)。
        healthz_public: bool,
        /// BU-32: 同時に生きていられる MCP session の上限。`0` = 無制限。
        max_sessions: u32,
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
                let allowed_origins = cfg
                    .and_then(|c| c.http.as_ref())
                    .and_then(|h| h.allowed_origins.clone());
                let healthz_public = cfg
                    .and_then(|c| c.http.as_ref())
                    .and_then(|h| h.healthz_public)
                    .unwrap_or(true);
                let max_sessions = cfg
                    .and_then(|c| c.http.as_ref())
                    .and_then(|h| h.max_sessions)
                    .unwrap_or(crate::transport::http::DEFAULT_MAX_SESSIONS);
                Ok(Transport::Http {
                    addr,
                    allowed_hosts,
                    allowed_origins,
                    healthz_public,
                    max_sessions,
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

/// (BU-01) `--bind` に非 loopback を明示した `serve` を `--i-know` で追認させる。
///
/// `groove service install` は同じ形の gate を持つ (`service/install.rs`) のに
/// `serve` 側は起動時 warning 1 行だけで、CLI から誤って LAN 公開できてしまう
/// 非対称があった。groove は認証を持たないので、bind アドレスが実質唯一の
/// アクセス制御になる。
///
/// **gate するのは CLI の `--bind` 由来の非 loopback bind だけ**。
/// `groove.toml` の `[transport.http].bind` は既存デプロイ
/// (`examples/deployments/intranet-http` の systemd unit は引数なしの
/// `groove serve` で、bind は toml から来る) が依存しているため拒否しない。
/// なお toml 由来の bind が必ず警告されるわけでもない — [`http::run_http`] の
/// warning は allow-list が未設定 / 空のときだけ出るので、`allowed_hosts` を
/// 明示した intranet 構成は無言で起動する (明示自体を意図表明とみなす)。
///
/// stdio に解決された `--bind` をここで拒否しないのは、**`main.rs` が既に
/// 「`--bind` / `--port` があるのに実効 transport が stdio なら reject」を
/// 実装しているから** (silent ignore は footgun という別の判断)。そちらの
/// 方が原因を的確に言えるので、同じ入力に 2 つのエラー経路を作らない。
/// つまり Stdio 分岐は防御的なもので、CLI 経由では到達しない。
pub fn check_cli_bind_ack(
    transport: &Transport,
    cli_bind: Option<SocketAddr>,
    i_know_non_loopback: bool,
) -> Result<()> {
    if i_know_non_loopback || !matches!(transport, Transport::Http { .. }) {
        return Ok(());
    }
    let Some(bind) = cli_bind else {
        return Ok(());
    };
    if bind.ip().is_loopback() {
        return Ok(());
    }
    anyhow::bail!(
        "--bind {bind} は non-loopback です。groove は認証を持ちません。\
         このポートに到達できる相手は、ナレッジベース全文を無資格で読めます \
         (/mcp に掛かっているのは Host 検証と session 数の上限だけで、認証ではありません)。\
         ネットワーク境界を別のもの \
         (コンテナのネットワーク分離 / reverse proxy / ファイアウォール) が\
         担っている場合にだけ使ってください。\
         承知の上で進めるなら --i-know を付けて再実行してください。"
    )
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
                allowed_origins: None,
                healthz_public: true,
                max_sessions: crate::transport::http::DEFAULT_MAX_SESSIONS,
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
                allowed_origins: None,
                healthz_public: true,
                max_sessions: crate::transport::http::DEFAULT_MAX_SESSIONS,
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
                allowed_origins: None,
                healthz_public: true,
                max_sessions: crate::transport::http::DEFAULT_MAX_SESSIONS,
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
                allowed_origins: None,
                healthz_public: true,
                max_sessions: crate::transport::http::DEFAULT_MAX_SESSIONS,
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
                allowed_origins: _,
                healthz_public: _,
                max_sessions: _,
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

    /// (1.0 blocker 4) `[transport.http].allowed_origins` が toml で明示されたら
    /// それがそのまま `Transport::Http` に渡る。proxy 越しに公開する構成では、
    /// ブラウザが送るのは loopback ではなく公開 origin になるため、既定値では
    /// 届かず、ここを通す経路が必要になる。
    #[test]
    fn test_resolve_config_allowed_origins_passes_through() {
        let cfg = TransportConfig {
            kind: Some(TransportKindConfig::Http),
            http: Some(HttpTransportConfig {
                bind: Some("127.0.0.1:3100".into()),
                allowed_origins: Some(vec!["https://kb.example.com".to_string()]),
                ..HttpTransportConfig::default()
            }),
        };
        let t = Transport::resolve(None, None, None, Some(&cfg)).unwrap();
        match t {
            Transport::Http {
                allowed_origins, ..
            } => assert_eq!(
                allowed_origins,
                Some(vec!["https://kb.example.com".to_string()])
            ),
            _ => panic!("expected Transport::Http"),
        }
    }

    /// 省略時は `None`。`run_http` がそこで **bind した port の loopback origin**
    /// を組み立てる。`Some(vec![])` との違いが要点で、空 `Vec` は rmcp では
    /// 「検証しない」を意味するため、省略の既定にしてはならない。
    #[test]
    fn test_resolve_config_omitted_allowed_origins_is_none_not_empty() {
        let cfg = TransportConfig {
            kind: Some(TransportKindConfig::Http),
            http: Some(HttpTransportConfig {
                bind: Some("127.0.0.1:3100".into()),
                ..HttpTransportConfig::default()
            }),
        };
        let t = Transport::resolve(None, None, None, Some(&cfg)).unwrap();
        match t {
            Transport::Http {
                allowed_origins, ..
            } => assert_eq!(
                allowed_origins, None,
                "omission must stay None; Some(vec![]) would disable validation"
            ),
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

    // -----------------------------------------------------------------------
    // BU-01: `serve --bind <non-loopback>` requires `--i-know`.
    // -----------------------------------------------------------------------

    /// Build an HTTP transport whose address came from the CLI `--bind` flag.
    fn http_at(addr: &str) -> Transport {
        Transport::Http {
            addr: addr.parse().unwrap(),
            allowed_hosts: None,
            allowed_origins: None,
            healthz_public: true,
            max_sessions: crate::transport::http::DEFAULT_MAX_SESSIONS,
        }
    }

    #[test]
    fn test_cli_bind_to_all_interfaces_is_rejected_without_ack() {
        let bind: SocketAddr = "0.0.0.0:3100".parse().unwrap();
        let err = check_cli_bind_ack(&http_at("0.0.0.0:3100"), Some(bind), false)
            .expect_err("non-loopback --bind must be refused without --i-know");
        let msg = err.to_string();
        assert!(
            msg.contains("--i-know"),
            "must name the escape hatch: {msg}"
        );
        assert!(msg.contains("0.0.0.0:3100"), "must name the bind: {msg}");
    }

    #[test]
    fn test_cli_bind_to_lan_ip_is_rejected_without_ack() {
        let bind: SocketAddr = "192.168.1.10:3100".parse().unwrap();
        check_cli_bind_ack(&http_at("192.168.1.10:3100"), Some(bind), false)
            .expect_err("a LAN address is just as exposed as 0.0.0.0");
    }

    #[test]
    fn test_cli_bind_to_non_loopback_is_allowed_once_acknowledged() {
        let bind: SocketAddr = "0.0.0.0:3100".parse().unwrap();
        check_cli_bind_ack(&http_at("0.0.0.0:3100"), Some(bind), true)
            .expect("--i-know is the documented escape hatch");
    }

    #[test]
    fn test_cli_bind_to_loopback_needs_no_ack() {
        let bind: SocketAddr = "127.0.0.1:3100".parse().unwrap();
        check_cli_bind_ack(&http_at("127.0.0.1:3100"), Some(bind), false)
            .expect("the default deployment must not need a flag");
    }

    #[test]
    fn test_cli_bind_to_ipv6_loopback_needs_no_ack() {
        let bind: SocketAddr = "[::1]:3100".parse().unwrap();
        check_cli_bind_ack(&http_at("[::1]:3100"), Some(bind), false).expect("::1 is loopback too");
    }

    /// The gate is deliberately scoped to the CLI flag: a non-loopback bind
    /// that came from `[transport.http].bind` keeps starting, because the
    /// published `intranet-http` recipe runs `groove serve` with no arguments
    /// and would otherwise break. It is not silently equivalent to a gate:
    /// `transport::http` warns about such a bind only when the Host allow-list
    /// is missing or empty, so a config with an explicit `allowed_hosts` gets
    /// neither the gate nor the warning.
    #[test]
    fn test_config_derived_non_loopback_bind_is_not_gated() {
        check_cli_bind_ack(&http_at("0.0.0.0:3100"), None, false)
            .expect("toml-derived binds must keep working");
    }

    /// Defensive branch, not a reachable CLI state: `main.rs` already rejects
    /// `--bind` / `--port` when the effective transport is stdio, with a
    /// message that names the real problem. This test pins that we leave that
    /// case to it rather than reporting a security error for an input that
    /// exposes nothing.
    #[test]
    fn test_stdio_is_left_to_the_transport_mismatch_error() {
        let bind: SocketAddr = "0.0.0.0:3100".parse().unwrap();
        check_cli_bind_ack(&Transport::Stdio, Some(bind), false)
            .expect("stdio listens on no port, so there is nothing to acknowledge");
    }
}
