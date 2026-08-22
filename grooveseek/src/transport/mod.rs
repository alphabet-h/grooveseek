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

    /// 受理する `Host` ヘッダの allow-list。`None` (省略) なら
    /// [`http::DEFAULT_LOOPBACK_HOSTS`](crate::transport::http::DEFAULT_LOOPBACK_HOSTS)
    /// = `["localhost", "127.0.0.1", "[::1]"]` (loopback only、DNS rebinding 防御)。
    /// LAN / イントラ公開時は `["192.168.1.10", "kb.example.lan", ...]` のように
    /// 明示する。空 `Vec` を渡すと **全 Host を許可**する (rmcp の
    /// `disable_allowed_hosts` 相当)。public 公開時は推奨されない。
    ///
    /// **検証するのは rmcp ではなく groove 自身**。ADR-0009 以降、rmcp の Host /
    /// Origin 検査は空リストを渡して無効化してあり、`/mcp` も `/healthz` も admin
    /// 経路も [`http::dns_rebinding_gate`](crate::transport::http) が答える。
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
    /// rmcp の既定は空リスト = **検証しない** だったので、既定値を空にはしない
    /// (ADR-0009 以降、検証しているのは rmcp ではなく groove 自身)。空 `Vec` を
    /// 明示すると検証は無効になる (起動時に warn を出す)。
    ///
    /// **認証ではない。** `Origin` を送らない要求 (MCP クライアント / tray /
    /// curl) は RFC 6454 どおり素通りする。これで防げるのは
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
        /// `None` = [`http::DEFAULT_LOOPBACK_HOSTS`](crate::transport::http::DEFAULT_LOOPBACK_HOSTS)
        /// の loopback-only allow-list を使う。`Some(vec)` = 明示 list
        /// (空 `Vec` を渡すと全 Host 許可になる)。F-33 で `groove.toml` から
        /// surface した。判定するのは ADR-0009 以降 groove 自身。
        allowed_hosts: Option<Vec<String>>,
        /// `None` = bind した port の loopback origin を既定として組み立てる
        /// ([`http::default_allowed_origins`](crate::transport::http::default_allowed_origins))。
        /// `Some(vec)` = 明示 list。空 `Vec` は **Origin 検証無効**になるので、
        /// 既定にはしない。
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
                // ここが `allowed_origins` の消費点 — この arm を通らない限り
                // rmcp には渡らない。だから検査もここに置く。`Stdio` arm も、
                // `index` / `search` も、このキーを読まない
                // (`check_origin_list` の doc が 2 度の置き場所の誤りを持っている)。
                if let Some(list) = allowed_origins.as_deref() {
                    crate::transport::http::check_origin_list(list)?;
                }
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
/// The refusal shown when a non-loopback bind is requested without `--i-know`.
///
/// **One implementation, two callers** (`groove serve` here and
/// `groove service install`). They answer the same question, and the changelog
/// promises the same text on both surfaces — a second copy would let the next
/// wording or policy correction update only one of them, silently.
///
/// **ASCII only.** This is a diagnostic, so it goes to stderr, and on a Japanese
/// Windows install that console is CP932 where non-ASCII arrives as mojibake.
/// Writing it in Japanese would garble precisely the sentence that explains what
/// is being exposed. (AGENTS.md, "Results go to stdout, diagnostics to stderr,
/// and stderr stays ASCII".)
///
/// `bind` is `Display` rather than `SocketAddr` because `service install` holds
/// the value as the string the user typed, and validates it separately.
pub fn non_loopback_bind_refusal(bind: impl std::fmt::Display) -> String {
    format!(
        "bind {bind} is not a loopback address. groove has no authentication, so \
         anything that can reach this port can read the entire knowledge base. \
         (/mcp is covered by Host validation and a session cap; neither one \
         authenticates the caller.) Use a non-loopback bind only when something \
         else owns the network boundary -- a container's network isolation, a \
         reverse proxy, or a firewall. Add --i-know to proceed anyway."
    )
}

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
    // (codex P2 round 3 on PR #173) The crate's one loopback predicate, not
    // `IpAddr::is_loopback`. The difference is `::ffff:127.0.0.1`, which the
    // admin router already treats as local (BU-21) — refusing to bind there
    // without `--i-know` told the operator it was network exposure while the
    // same address was being let into `/ui`.
    if crate::transport::http::is_loopback_peer(bind.ip()) {
        return Ok(());
    }
    anyhow::bail!("{}", non_loopback_bind_refusal(bind))
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

    /// The spelling `allowed_hosts` accepts, written into the key next door.
    /// rmcp drops it at match time and says nothing, leaving Origin validation
    /// on with nothing to match — so the server 403s every browser, including
    /// its own `/ui`, and the log is silent. Refusing here, before the list
    /// becomes part of a running transport, is the last place it is visible.
    #[test]
    fn an_origin_entry_that_never_reaches_the_comparison_stops_the_http_transport() {
        let cfg = TransportConfig {
            kind: Some(TransportKindConfig::Http),
            http: Some(HttpTransportConfig {
                bind: Some("127.0.0.1:3100".into()),
                allowed_origins: Some(vec!["127.0.0.1:3100".to_string()]),
                ..HttpTransportConfig::default()
            }),
        };
        let msg = format!(
            "{:#}",
            Transport::resolve(None, None, None, Some(&cfg))
                .expect_err("an entry rmcp would drop must not reach a running server")
        );
        for needle in [
            "[transport.http].allowed_origins",
            "127.0.0.1:3100",
            "scheme",
        ] {
            assert!(
                msg.contains(needle),
                "the refusal must contain {needle:?} to be actionable, got {msg:?}"
            );
        }
    }

    /// The other half, and the reason the check sits here rather than in
    /// `Config`: `index`, `search` and a stdio server never read this key.
    /// Measured while it was in `discover_in` — `groove validate`, which opens
    /// no socket at all, refused to run because of an HTTP-only typo.
    ///
    /// `[transport.http]` alone implies HTTP, so `kind` is set explicitly here;
    /// that sugar is what makes the stdio case easy to lose.
    #[test]
    fn a_stdio_transport_does_not_read_the_origin_list() {
        let cfg = TransportConfig {
            kind: Some(TransportKindConfig::Stdio),
            http: Some(HttpTransportConfig {
                allowed_origins: Some(vec!["127.0.0.1:3100".to_string()]),
                ..HttpTransportConfig::default()
            }),
        };
        assert!(
            matches!(
                Transport::resolve(None, None, None, Some(&cfg)),
                Ok(Transport::Stdio)
            ),
            "an HTTP-only setting must not stop a transport that never reads it"
        );
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

    /// (codex P1 round 1 on PR #173) The refusal is a diagnostic, so it goes to
    /// stderr, and on a Japanese Windows console that is CP932 — non-ASCII would
    /// arrive as mojibake and garble exactly the sentence explaining what is
    /// exposed. AGENTS.md requires ASCII there; this pins it, because the text
    /// is prose and prose is what drifts.
    #[test]
    fn the_non_loopback_refusal_is_ascii_only() {
        let msg = non_loopback_bind_refusal("0.0.0.0:3100");
        assert!(
            msg.is_ascii(),
            "stderr diagnostics must be ASCII (CP932 consoles); got: {msg}"
        );
        assert!(msg.contains("--i-know"), "the escape hatch must be named");
        assert!(
            msg.contains("read the entire knowledge base"),
            "the refusal has to state the consequence, not just call it dangerous"
        );
    }

    /// (codex P1 round 1 on PR #173) `groove serve` and `groove service install`
    /// answer the same question, and the changelog promises the same text on
    /// both. One implementation, so a later wording change cannot land on only
    /// one surface (AGENTS.md, "One question gets one implementation").
    #[test]
    fn serve_and_service_install_refuse_with_the_same_text() {
        let bind = "0.0.0.0:3100";
        let from_serve = check_cli_bind_ack(&http_at(bind), Some(bind.parse().unwrap()), false)
            .expect_err("non-loopback --bind must be refused without --i-know")
            .to_string();
        assert_eq!(
            from_serve,
            non_loopback_bind_refusal(bind),
            "serve must render the shared refusal verbatim"
        );
    }

    /// (codex P2 round 3 on PR #173) "Is this address loopback" now has one
    /// answer. It had three, and they disagreed on `::ffff:127.0.0.1`: the admin
    /// router let such a peer in (BU-21) while `serve` and `service install`
    /// both called the same address network exposure and demanded `--i-know`.
    ///
    /// The mapped form is the case worth pinning — every other address the two
    /// old predicates already agreed on, so a regression would show up here
    /// first.
    #[test]
    fn the_bind_gate_uses_the_same_loopback_predicate_as_the_admin_router() {
        use crate::transport::http::is_loopback_peer;
        let mapped: SocketAddr = "[::ffff:127.0.0.1]:3100".parse().unwrap();
        assert!(
            is_loopback_peer(mapped.ip()),
            "IPv4-mapped loopback is loopback; the admin router already says so"
        );
        assert!(
            !mapped.ip().is_loopback(),
            "std disagrees, which is exactly why the shared predicate exists"
        );
        check_cli_bind_ack(&http_at("[::ffff:127.0.0.1]:3100"), Some(mapped), false)
            .expect("a mapped loopback bind must not demand --i-know");

        // The answers that were never in dispute stay put.
        for addr in ["127.0.0.1:3100", "127.0.0.2:3100", "[::1]:3100"] {
            check_cli_bind_ack(&http_at(addr), Some(addr.parse().unwrap()), false)
                .unwrap_or_else(|e| panic!("{addr} must be accepted as loopback: {e}"));
        }
        for addr in ["0.0.0.0:3100", "192.168.1.10:3100"] {
            check_cli_bind_ack(&http_at(addr), Some(addr.parse().unwrap()), false)
                .expect_err("a non-loopback bind must still require --i-know");
        }
    }

    /// (codex P1 round 7 on PR #173) The loopback **alias set** gets the same
    /// treatment as the loopback **predicate**: one definition, no copies.
    /// Round 6 replaced the Origin literal and left the two in `server.rs`,
    /// which is the half-state that keeps producing findings — an alias added
    /// to the shared set would then be honoured by `/mcp` and refused by `/ui`.
    ///
    /// The scan is on the literal rather than the behaviour because the copy is
    /// what drifts; a behavioural test would still pass on the day the copy is
    /// made, and only fail later when someone edits one of them.
    #[test]
    fn the_loopback_alias_set_has_no_second_definition() {
        use crate::transport::http::DEFAULT_LOOPBACK_HOSTS;
        assert_eq!(
            DEFAULT_LOOPBACK_HOSTS,
            &["localhost", "127.0.0.1", "[::1]"],
            "if this set changes on purpose, every list below follows it"
        );
        let server = include_str!("../server.rs");
        assert!(
            server.contains("DEFAULT_LOOPBACK_HOSTS"),
            "server.rs must build its admin allow-list from the shared set"
        );
        for (n, line) in server.lines().enumerate() {
            if line.trim_start().starts_with("//") {
                continue;
            }
            assert!(
                !line.contains("\"localhost\""),
                "server.rs:{} spells an alias out again; build it from \
                 DEFAULT_LOOPBACK_HOSTS instead -- line was: {}",
                n + 1,
                line.trim()
            );
        }
    }

    /// (codex P2 round 4 on PR #173) Round 3 converted three call sites and
    /// left three behind, which was worse than before: the gate said loopback
    /// while the startup warning, the admin allow-list and the untrusted-config
    /// downgrade said exposure. This scan is the thing that would have caught
    /// it, so it exists now rather than after the next one.
    ///
    /// `is_loopback_peer`'s own body is the one legitimate `is_loopback()` —
    /// it is the implementation — and this test's sibling asserts `std` still
    /// disagrees about the mapped form, so both are excluded by line.
    #[test]
    fn no_call_site_answers_the_loopback_question_on_its_own() {
        let sources = [
            ("config.rs", include_str!("../config.rs")),
            ("server.rs", include_str!("../server.rs")),
            ("service/install.rs", include_str!("../service/install.rs")),
            ("transport/http.rs", include_str!("http.rs")),
        ];
        for (name, src) in sources {
            for (n, line) in src.lines().enumerate() {
                let code = line.trim_start();
                // Prose may name the std method while explaining why we do not
                // call it; the scan is about call sites.
                if code.starts_with("//") || !line.contains(".is_loopback()") {
                    continue;
                }
                // Inside `is_loopback_peer` the calls ARE the implementation.
                let inside_the_predicate = line.contains("v4.is_loopback()")
                    || line.contains("v6.is_loopback()")
                    || line.contains("Some(v4) =>")
                    || line.contains("None =>");
                assert!(
                    inside_the_predicate,
                    "{name}:{} calls is_loopback() directly; use \
                     transport::http::is_loopback_peer so every surface agrees \
                     about ::ffff:127.0.0.1 -- line was: {}",
                    n + 1,
                    line.trim()
                );
            }
        }
    }

    /// The other half of the same invariant, and the half a behavioural test
    /// cannot reach cheaply: `service install` needs a registry / launchd write
    /// to exercise, so scan its source instead. What must not exist is a second
    /// copy of the prose — that is the thing that drifts.
    #[test]
    fn service_install_does_not_carry_its_own_copy_of_the_refusal() {
        let install = include_str!("../service/install.rs");
        assert!(
            install.contains("non_loopback_bind_refusal"),
            "service install must call the shared refusal"
        );
        assert!(
            !install.contains("has no authentication, so"),
            "service install must not inline the wording; call the shared fn"
        );
        // (codex P2 round 3 on PR #173) Same reasoning for the loopback test:
        // install used a string-prefix predicate of its own, which is why it
        // disagreed about `::ffff:127.0.0.1`.
        assert!(
            install.contains("is_loopback_peer"),
            "service install must ask the shared loopback predicate"
        );
        assert!(
            !install.contains("starts_with(\"127.\")"),
            "no private loopback predicate in install.rs; it drifts from the router"
        );
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
