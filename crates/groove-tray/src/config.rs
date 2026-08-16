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

/// Resolve the tray's Config by reading `groove.toml` from either:
/// - `<kb_path_override>/groove.toml` (= `--kb-path` flag, rare opt-in), or
/// - `<dirs::config_dir()>/groove/<service_name>/groove.toml` (= default,
///   matches what `groove service install` wrote).
///
/// Returns Err if the toml is missing or unparseable. The tray's main.rs
/// (PR-1 skeleton) catches this and falls back to a placeholder Config for
/// debug purposes; PR-2 will switch to fail-fast (spec section 6 末尾).
pub fn resolve(service_name: &str, kb_path_override: Option<&PathBuf>) -> Result<Config> {
    let toml_path = if let Some(p) = kb_path_override {
        p.join("groove.toml")
    } else {
        dirs::config_dir()
            .ok_or_else(|| anyhow::anyhow!("config_dir not found"))?
            .join("groove")
            .join(service_name)
            .join("groove.toml")
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
    let admin_host_port = normalize_to_loopback_with_warning(&bind)
        .with_context(|| format!("invalid [transport.http].bind in {}", toml_path.display()))?;
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
fn normalize_to_loopback_with_warning(bind: &str) -> Result<String> {
    let (host, port) = parse_bind(bind)?;
    if host == BindHost::Other {
        tracing::warn!(
            "daemon bind '{bind}' is specific non-loopback; tray polls 127.0.0.1 \
             but the daemon does not listen there. Either change the daemon bind \
             to loopback (127.0.0.1) or wildcard (0.0.0.0), or remove --with-tray.",
        );
    }
    Ok(authority_for(host, port))
}

/// `bind` の host 部分の分類。URL は分類 + 数値 port から**組み立て直す**
/// ので、toml の文字列がそのまま authority に入ることはない。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BindHost {
    /// `127.0.0.1`
    Loopback4,
    /// `::1`
    Loopback6,
    /// リテラル `localhost`
    LocalhostName,
    /// `0.0.0.0` / `::`
    Wildcard,
    /// 特定の非 loopback アドレス
    Other,
}

/// `bind` を厳密に解釈する (AU-12)。
///
/// 以前は最後の `:` で切って host / port を **文字列のまま**引き回して
/// いたため、`groove.toml` に書かれた任意の文字列が URL に流れ込んでいた。
/// 実害が 2 つある:
///
/// - `bind = "127.0.0.1:3100&ver&"` → `ui_url` が
///   `http://127.0.0.1:3100&ver&/ui`。これを `cmd /c start` に渡すと `&` が
///   コマンド区切りとして働く。Rust の `Command` は**空白を含む引数しか
///   quote しない**ので `&` は素通りする (実測で確認済み)
/// - `bind = "127.0.0.1:3100@evil.example"` →
///   `http://127.0.0.1:3100@evil.example/api/admin/status`。`127.0.0.1:3100`
///   の側が **userinfo** として解釈され、実際の接続先は `evil.example` に
///   なる。tray の poll がそこへ出ていく
///
/// 対策は「検証してから**数値の port で組み立て直す**」こと
/// ([`authority_for`])。返る authority に toml 由来の文字列は入らない。
///
/// 受け付ける形式は `<ipv4>:<port>` / `[<ipv6>]:<port>` / `localhost:<port>`。
/// loopback の判定は従来どおり `127.0.0.1` / `::1` / `localhost` に限る
/// (`127.0.0.2` 等の 127.0.0.0/8 は従来どおり `Other` 扱い)。
fn parse_bind(bind: &str) -> Result<(BindHost, u16)> {
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

    if let Ok(addr) = bind.parse::<SocketAddr>() {
        let host = match addr.ip() {
            IpAddr::V4(v4) if v4 == Ipv4Addr::LOCALHOST => BindHost::Loopback4,
            IpAddr::V4(v4) if v4.is_unspecified() => BindHost::Wildcard,
            IpAddr::V6(v6) if v6 == Ipv6Addr::LOCALHOST => BindHost::Loopback6,
            IpAddr::V6(v6) if v6.is_unspecified() => BindHost::Wildcard,
            _ => BindHost::Other,
        };
        return Ok((host, addr.port()));
    }

    // `localhost:<port>` は IP リテラルではないので `SocketAddr` では解釈できない。
    let localhost_port = bind
        .rsplit_once(':')
        .filter(|(host, _)| host.eq_ignore_ascii_case("localhost"))
        .and_then(|(_, port)| port.parse::<u16>().ok());
    if let Some(port) = localhost_port {
        return Ok((BindHost::LocalhostName, port));
    }

    anyhow::bail!(
        "invalid bind {bind:?}: expected <ipv4>:<port>, [<ipv6>]:<port> or localhost:<port>"
    )
}

/// tray が話しかける authority を、分類と**数値の** port から組み立てる。
fn authority_for(host: BindHost, port: u16) -> String {
    match host {
        // Wildcard の daemon は loopback でも listen するので polling は通る。
        // Other は listen しないので polling は失敗する — 呼び出し側が warn 済み。
        BindHost::Loopback4 | BindHost::Wildcard | BindHost::Other => format!("127.0.0.1:{port}"),
        BindHost::Loopback6 => format!("[::1]:{port}"),
        BindHost::LocalhostName => format!("localhost:{port}"),
    }
}

/// Pure parse + loopback rewrite. Always returns a loopback host:port.
///
/// 本番経路は [`normalize_to_loopback_with_warning`] が
/// [`parse_bind`] + [`authority_for`] を直接呼ぶので、こちらは既存テストの
/// 入口としてのみ残っている (`#[cfg(test)]` を外すと dead_code になる)。
#[cfg(test)]
fn normalize_to_loopback(bind: &str) -> Result<String> {
    let (host, port) = parse_bind(bind)?;
    Ok(authority_for(host, port))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_temp_toml(body: &str) -> PathBuf {
        let dir = crate::test_support::unique_temp_path("groove-tray-cfg");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("groove.toml");
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(body.as_bytes()).unwrap();
        dir
    }

    #[test]
    fn resolves_default_bind_when_toml_empty() {
        let dir = write_temp_toml("");
        let cfg = resolve("groove", Some(&dir)).unwrap();
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
        let cfg = resolve("groove", Some(&dir)).unwrap();
        assert_eq!(cfg.bind, "127.0.0.1:4242");
        assert_eq!(cfg.ui_url, "http://127.0.0.1:4242/ui");
        assert_eq!(cfg.status_url, "http://127.0.0.1:4242/api/admin/status");
    }

    #[test]
    fn fails_when_toml_missing() {
        let dir =
            std::env::temp_dir().join(format!("groove-tray-cfg-missing-{}", std::process::id()));
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
        let cfg = resolve("groove", Some(&dir)).unwrap();
        // raw bind is preserved for diagnostics
        assert_eq!(cfg.bind, "0.0.0.0:3100");
        // but admin URLs target loopback so server allow-list accepts them
        assert_eq!(cfg.base_url, "http://127.0.0.1:3100");
        assert_eq!(cfg.status_url, "http://127.0.0.1:3100/api/admin/status");
        assert_eq!(cfg.ui_url, "http://127.0.0.1:3100/ui");
    }

    #[test]
    fn loopback_bind_passes_through() {
        assert_eq!(
            normalize_to_loopback("127.0.0.1:3100").unwrap(),
            "127.0.0.1:3100"
        );
        assert_eq!(
            normalize_to_loopback("localhost:8080").unwrap(),
            "localhost:8080"
        );
        assert_eq!(normalize_to_loopback("[::1]:3100").unwrap(), "[::1]:3100");
    }

    #[test]
    fn wildcard_bind_rewrites_host_to_loopback() {
        assert_eq!(
            normalize_to_loopback("0.0.0.0:3100").unwrap(),
            "127.0.0.1:3100"
        );
        assert_eq!(
            normalize_to_loopback("[::]:3100").unwrap(),
            "127.0.0.1:3100"
        );
    }

    #[test]
    fn specific_non_loopback_bind_rewrites_to_loopback() {
        // codex P2 round 4 on PR #62: tray admin routes are loopback-only,
        // so the URL always targets 127.0.0.1. A daemon bound to a
        // specific NIC (not loopback, not wildcard) makes polling fail
        // with "connection refused" — that's a misconfiguration the
        // caller surfaces via tracing::warn! in
        // normalize_to_loopback_with_warning.
        assert_eq!(
            normalize_to_loopback("192.168.1.5:8080").unwrap(),
            "127.0.0.1:8080"
        );
        assert_eq!(
            normalize_to_loopback("10.0.0.42:3100").unwrap(),
            "127.0.0.1:3100"
        );
    }

    // ---- AU-12: bind の検証 ----

    /// `cmd /c start` へのコマンド注入経路。`&` は Rust の `Command` が
    /// quote しない (空白を含む引数のみ quote される) ので、そのまま
    /// `cmd.exe` のコマンド区切りとして働く。
    #[test]
    fn a_bind_carrying_shell_metacharacters_is_rejected() {
        for hostile in [
            "127.0.0.1:3100&ver&",
            "127.0.0.1:3100&calc",
            "127.0.0.1:3100|whoami",
            "127.0.0.1:3100^",
            "127.0.0.1:3100>out.txt",
        ] {
            assert!(
                parse_bind(hostile).is_err(),
                "should have refused {hostile:?}"
            );
        }
    }

    /// `@` を混ぜると authority の前半が userinfo になり、実際の接続先が
    /// 別ホストになる。tray の poll がそこへ出ていくので、ブラウザを
    /// 開く前の段階で漏れる。
    #[test]
    fn a_bind_that_would_turn_the_host_into_userinfo_is_rejected() {
        for hostile in [
            "127.0.0.1:3100@evil.example",
            "127.0.0.1:3100@127.0.0.1:80",
            "localhost:3100@evil.example",
        ] {
            assert!(
                parse_bind(hostile).is_err(),
                "should have refused {hostile:?}"
            );
        }
    }

    #[test]
    fn a_bind_without_a_usable_port_is_rejected() {
        for bad in [
            "127.0.0.1",       // port 無し
            "127.0.0.1:",      // port 空
            "127.0.0.1:99999", // u16 を超える
            "127.0.0.1:-1",
            "127.0.0.1:0x1234",
            "",
            "not a bind at all",
            "[::1]",
        ] {
            assert!(parse_bind(bad).is_err(), "should have refused {bad:?}");
        }
    }

    #[test]
    fn the_authority_is_rebuilt_from_a_numeric_port() {
        // 出力に toml 由来の文字列が入らないことの確認: 分類 + u16 だけで組む。
        assert_eq!(authority_for(BindHost::Loopback4, 3100), "127.0.0.1:3100");
        assert_eq!(authority_for(BindHost::Loopback6, 3100), "[::1]:3100");
        assert_eq!(
            authority_for(BindHost::LocalhostName, 8080),
            "localhost:8080"
        );
        assert_eq!(authority_for(BindHost::Wildcard, 3100), "127.0.0.1:3100");
        assert_eq!(authority_for(BindHost::Other, 8080), "127.0.0.1:8080");
    }

    #[test]
    fn parse_bind_classifies_each_host_form() {
        assert_eq!(
            parse_bind("127.0.0.1:3100").unwrap(),
            (BindHost::Loopback4, 3100)
        );
        assert_eq!(
            parse_bind("[::1]:3100").unwrap(),
            (BindHost::Loopback6, 3100)
        );
        assert_eq!(
            parse_bind("LocalHost:8080").unwrap(),
            (BindHost::LocalhostName, 8080)
        );
        assert_eq!(
            parse_bind("0.0.0.0:3100").unwrap(),
            (BindHost::Wildcard, 3100)
        );
        assert_eq!(parse_bind("[::]:3100").unwrap(), (BindHost::Wildcard, 3100));
        assert_eq!(
            parse_bind("192.168.1.5:8080").unwrap(),
            (BindHost::Other, 8080)
        );
        // 127.0.0.0/8 のうち 127.0.0.1 以外は従来どおり Other 扱い。
        assert_eq!(
            parse_bind("127.0.0.2:3100").unwrap(),
            (BindHost::Other, 3100)
        );
    }

    /// tray は起動時に config を解決するので、不正な bind は**起動を拒否**する。
    #[test]
    fn a_hostile_bind_stops_the_tray_from_starting() {
        let dir = write_temp_toml(
            r#"
[transport.http]
bind = "127.0.0.1:3100&ver&"
"#,
        );
        let err = resolve("groove", Some(&dir)).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("bind"),
            "the error should point at the bind setting: {msg}"
        );
    }
}
