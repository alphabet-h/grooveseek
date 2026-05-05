//! Streamable HTTP transport runner.
//!
//! rmcp 1.x の `StreamableHttpService` を axum でマウントし、複数クライアント
//! 同時接続可能な MCP サーバを提供する。mount path は `/mcp` 固定 (MVP)。
//! `/healthz` は 200 "ok" を返すだけの health check。
//!
//! rmcp の service factory は session 毎に新しい Handler を要求するが、
//! 重いリソース (embedder / reranker / DB) は `KbServerShared` を Arc で
//! 共有するので重複ロードは起きない。

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::{Context, Result};
use axum::{
    Router,
    body::Body,
    extract::{Request, State},
    http::{HeaderMap, StatusCode},
    middleware::{self, Next},
    response::Response,
    routing::get,
};
use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
};

use crate::server::{KbServer, KbServerShared};

/// rmcp's default loopback-only allow-list, mirrored locally so the F-64
/// `/healthz` middleware can apply identical semantics when
/// `allowed_hosts = None`. Keep in sync with rmcp upstream.
const DEFAULT_LOOPBACK_HOSTS: &[&str] = &["localhost", "127.0.0.1", "::1"];

/// Start an axum-based HTTP server that exposes the MCP service at `/mcp`.
/// Blocks until SIGINT or a bind error. On bind failure, returns with a
/// helpful context message.
///
/// `allowed_hosts`:
/// - `None` → rmcp の default (`["localhost", "127.0.0.1", "::1"]`、loopback
///   only) を使う。DNS rebinding 攻撃に対する標準的な防御。
/// - `Some(vec)` → `[transport.http].allowed_hosts` で operator が明示した
///   list を使う。LAN / イントラ公開時はここに公開ホスト名 / IP を入れる。
///   空 `Vec` を渡すと rmcp は **全 Host ヘッダを許可** する (
///   `disable_allowed_hosts` と同等)。public 公開時は推奨されない。
///
/// 加えて、bind が **非 loopback** (`0.0.0.0`、特定 LAN IP 等) の状態で
/// `allowed_hosts` が `None` (= loopback only な default) のままなら、
/// 起動時に `tracing::warn` を発してオペレータの注意を促す。loopback only
/// の allow-list で外部 bind するのは「公開する気はあるが host 検証で
/// reject される」というほぼ確実に意図しない構成なので。
pub async fn run_http(
    addr: SocketAddr,
    allowed_hosts: Option<Vec<String>>,
    healthz_public: bool,
    shared: KbServerShared,
) -> Result<()> {
    // bind 範囲と allow-list の組合せが噛み合っていない時に warn を出す。
    if should_warn_non_loopback_bind(&addr, allowed_hosts.as_deref()) {
        tracing::warn!(
            bind = %addr,
            "non-loopback bind with default allowed_hosts (loopback-only). \
             Inbound requests with a non-loopback Host header will be rejected. \
             Set [transport.http].allowed_hosts explicitly in kb-mcp.toml \
             (e.g. allowed_hosts = [\"kb.example.lan\", \"192.168.1.10\"])."
        );
    }

    // Session manager: LocalSessionManager keeps per-session state in memory.
    // Suitable for a single-process server (our deployment model).
    let session_manager = Arc::new(LocalSessionManager::default());

    // Service factory: invoked per new MCP session. Builds a fresh `KbServer`
    // handle that clones the Arc-shared heavy resources. The factory must
    // return `Result<_, std::io::Error>` per rmcp's trait. `shared` は以降
    // 使わないので clone せず move する (evaluator Med #4)。
    let factory_shared = shared;
    let factory =
        move || -> Result<KbServer, std::io::Error> { Ok(KbServer::from_shared(&factory_shared)) };

    let mcp_config = match allowed_hosts.clone() {
        Some(hosts) => StreamableHttpServerConfig::default().with_allowed_hosts(hosts),
        None => StreamableHttpServerConfig::default(),
    };
    let mcp_service = StreamableHttpService::new(factory, session_manager, mcp_config);

    // F-64: `/healthz` を `allowed_hosts` 検証配下に置く opt-in。
    // healthz_public = true (default) の場合は従来通り Host check なしで public。
    // false の場合は `allowed_hosts` を `Arc` で middleware state に渡し、
    // Host header を検証して non-allowlisted は 403。
    let healthz_router = if healthz_public {
        Router::new().route("/healthz", get(healthz))
    } else {
        let allowed_state = Arc::new(allowed_hosts.clone());
        Router::new()
            .route("/healthz", get(healthz))
            .layer(middleware::from_fn_with_state(
                allowed_state,
                healthz_host_check,
            ))
    };
    let app = healthz_router.nest_service("/mcp", mcp_service);

    let listener = tokio::net::TcpListener::bind(addr).await.with_context(|| {
        format!(
            "failed to bind {addr}: is another kb-mcp instance running, or the \
                 port occupied?"
        )
    })?;
    eprintln!(
        "kb-mcp server ready (http transport, listening on {})",
        listener.local_addr().unwrap_or(addr)
    );

    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            // Ctrl-C でグレースフルシャットダウン。Windows / Linux 両対応。
            let _ = tokio::signal::ctrl_c().await;
            eprintln!("kb-mcp: shutdown signal received");
        })
        .await
        .context("axum::serve failed")?;
    Ok(())
}

/// `Host` header / allow-list entry を `(host, Option<port>)` に分解する。
/// RFC 7230 の Host header 文法に従う:
/// - IPv6 literal w/ port: `[::1]:3100` → (`"::1"`, `Some("3100")`)
/// - IPv6 literal w/o port: `[::1]` → (`"::1"`, `None`)
/// - IPv6 unbracketed (config 形式): `"::1"` → (`"::1"`, `None`)
/// - IPv4 / hostname w/ port: `192.168.1.10:3100` → (`"192.168.1.10"`, `Some("3100")`)
/// - IPv4 / hostname w/o port: `192.168.1.10` → (`"192.168.1.10"`, `None`)
///
/// codex P2 (#50 round 1-4): port-aware にすることで以下を一貫させる:
/// - IPv6 literal の bracket 剥がし
/// - allow-list の host-only entry は port-agnostic match
/// - allow-list の host:port entry は **port 厳密一致**
///   (= `["example.com:8080"]` が `Host: example.com:9999` を accept しない)
///
/// codex P2 (#50 round 6): port は `u16` 範囲 (0-65535) のみ valid。
/// `99999` のような u16 範囲外は **invalid port** として全体を raw 扱い、
/// allow-list match から外す (= rmcp の `Authority::try_from` が同条件で
/// reject するのに合わせる)。
fn split_host_port(s: &str) -> (&str, Option<&str>) {
    // Bracketed IPv6: `[ipv6]:port` or `[ipv6]`
    // codex P1 (#50 round 5): `]` の後ろに任意の文字列 (例: `[::1]evil.example`)
    // を許すと、malformed Host が allow-list bypass になる (= host="::1" に
    // 正規化されてしまい loopback default に通る)。`after` は **空** または
    // `:<valid u16 port>` のみ許容、それ以外は raw 文字列を返して match 不能化する。
    if let Some(rest) = s.strip_prefix('[')
        && let Some(end) = rest.find(']')
    {
        let host = &rest[..end];
        let after = &rest[end + 1..];
        if after.is_empty() {
            return (host, None);
        }
        if let Some(port) = after.strip_prefix(':')
            && !port.is_empty()
            && port.chars().all(|c| c.is_ascii_digit())
            && port.parse::<u16>().is_ok()
        {
            return (host, Some(port));
        }
        // Malformed bracketed Host (`]` 後に予期しない文字列、または
        // u16 範囲外 port) → raw を返す。比較側は host_full == raw or
        // host_part == raw のみで判定するので、通常の allow-list entry とは
        // 一致しない = 403 (= bypass を防ぐ)。
        return (s, None);
    }
    // No brackets. Count colons to disambiguate IPv4/hostname:port vs IPv6 unbracketed.
    let colon_count = s.bytes().filter(|&b| b == b':').count();
    if colon_count >= 2 {
        // IPv6 unbracketed (config 形式 like "::1") — no port form.
        return (s, None);
    }
    if colon_count == 1
        && let Some(colon) = s.rfind(':')
    {
        let port_part = &s[colon + 1..];
        if !port_part.is_empty()
            && port_part.chars().all(|c| c.is_ascii_digit())
            && port_part.parse::<u16>().is_ok()
        {
            return (&s[..colon], Some(port_part));
        }
        // u16 範囲外 port (`localhost:99999` 等) → raw を返す。
        return (s, None);
    }
    (s, None)
}

/// `split_host_port` の host portion のみを返す薄い wrapper。
/// 既存 unit test との後方互換性のため残す (= production code は
/// `split_host_port` を直接使用、本関数は test 範囲のみ参照)。
#[cfg(test)]
fn extract_host_part(s: &str) -> &str {
    split_host_port(s).0
}

/// F-64: `/healthz` 用 axum middleware。`Host` header を `allowed_hosts`
/// (state) と照合し、不一致なら 403 を返す。`allowed_hosts` の semantics は
/// rmcp の `with_allowed_hosts` と同等:
/// - `None` → `DEFAULT_LOOPBACK_HOSTS` (`localhost` / `127.0.0.1` / `::1`) のみ pass
/// - `Some(empty)` → 全 Host 許可 (= `disable_allowed_hosts` 相当)
/// - `Some(non_empty)` → list と case-insensitive 一致のみ pass
///
/// 比較は **full Host header と host-only の両方**で行うので、allow-list
/// entry が `"192.168.1.10"` でも `"192.168.1.10:3100"` でも match する
/// (= kb-mcp.toml.example の document 例と整合)。
async fn healthz_host_check(
    State(allowed): State<Arc<Option<Vec<String>>>>,
    headers: HeaderMap,
    req: Request,
    next: Next,
) -> Response {
    // codex P2 (#50 round 2): HTTP/2 では `Host` header の代わりに
    // `:authority` pseudo-header が使われ、`headers.get("host")` が `None`
    // を返す。rmcp の `/mcp` 経路はこれを `uri.authority()` で fallback して
    // accept するので、本 middleware も同じ semantics に揃える (= HTTP/2 や
    // proxy-forwarded health check で false reject を出さない)。
    let authority_owned: Option<String> = req.uri().authority().map(|a| a.to_string());
    let host_full = headers
        .get("host")
        .and_then(|h| h.to_str().ok())
        .or(authority_owned.as_deref())
        .unwrap_or("");
    let (incoming_host, incoming_port) = split_host_port(host_full);

    // codex P2 (#50 round 1-4): allow-list entry を `(host, port)` に分解、
    // host は normalize (= bracket / port 剥がし) で比較。port は:
    // - allow に port 指定あり → incoming も同じ port のみ pass (strict)
    // - allow に port 指定なし → incoming の port は無視 (port-agnostic)
    // これで以下が満たされる:
    //   - `["192.168.1.10"]` (= host-only) は `Host: 192.168.1.10` も
    //     `Host: 192.168.1.10:3100` も match
    //   - `["192.168.1.10:3100"]` (= port 込み) は `Host: 192.168.1.10:3100` のみ match、
    //     `Host: 192.168.1.10:9999` は **403** (codex round 4 の P2 fix)
    //   - `["[::1]"]` も `["::1"]` も IPv6 loopback の任意 form と match
    //
    // codex P2 (#50 round 7): port は raw string ではなく **`u16` numeric** で
    // 比較する (= `"080"` と `"80"` を semantically 等価扱い)。`split_host_port`
    // は port を u16 範囲内のみ許容 = parse 失敗はあり得ないが、念のため
    // `parse::<u16>().ok()` で defensive 比較。
    let matches = |allow: &str| -> bool {
        let (allow_host, allow_port) = split_host_port(allow);
        if !allow_host.eq_ignore_ascii_case(incoming_host) {
            return false;
        }
        match (allow_port, incoming_port) {
            (None, _) => true,        // allow に port なし = port-agnostic
            (Some(_), None) => false, // allow has port, incoming doesn't
            (Some(ap), Some(ip)) => ap.parse::<u16>().ok() == ip.parse::<u16>().ok(),
        }
    };

    let allowed_match = match allowed.as_ref() {
        // None → rmcp default loopback list 互換
        None => DEFAULT_LOOPBACK_HOSTS.iter().any(|a| matches(a)),
        // Some(empty) → 全許可 (= disable_allowed_hosts 相当)
        Some(v) if v.is_empty() => true,
        // Some(non_empty) → 一致のみ pass
        Some(v) => v.iter().any(|a| matches(a.as_str())),
    };

    if allowed_match {
        next.run(req).await
    } else {
        Response::builder()
            .status(StatusCode::FORBIDDEN)
            .body(Body::from("forbidden"))
            .expect("static response build")
    }
}

/// `addr` が非 loopback (0.0.0.0、unspecified、または LAN IP 等) で、かつ
/// operator が `allowed_hosts` を toml で明示していない場合に true。
///
/// loopback only の default allow-list で外部 bind すると、外部クライアント
/// からは Host header validation で必ず弾かれて 403 になるが、エラー文言
/// だけでは原因が分かりにくい。起動時に警告してオペレータの設定漏れを
/// 早期に気付かせる。
fn should_warn_non_loopback_bind(addr: &SocketAddr, allowed_hosts: Option<&[String]>) -> bool {
    let ip = addr.ip();
    let is_external = !ip.is_loopback();
    let no_explicit_hosts = allowed_hosts.is_none();
    is_external && no_explicit_hosts
}

/// Health check endpoint. Always returns 200 with body "ok".
async fn healthz() -> &'static str {
    "ok"
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// F-33: 0.0.0.0 + default allowed_hosts → warn が立つ
    /// (loopback-only allow-list で外部 bind は即 403 確定なので確実に
    /// 設定漏れ)。
    #[test]
    fn test_warn_on_unspecified_bind_with_default_allowed_hosts() {
        let addr: SocketAddr = "0.0.0.0:3100".parse().unwrap();
        assert!(should_warn_non_loopback_bind(&addr, None));
    }

    /// F-33: 127.0.0.1 + default allowed_hosts → warn 不要
    /// (default 構成、これが想定運用)。
    #[test]
    fn test_no_warn_on_loopback_bind_with_default_allowed_hosts() {
        let addr: SocketAddr = "127.0.0.1:3100".parse().unwrap();
        assert!(!should_warn_non_loopback_bind(&addr, None));
    }

    /// F-33: ::1 (IPv6 loopback) + default → warn 不要。
    #[test]
    fn test_no_warn_on_ipv6_loopback() {
        let addr: SocketAddr = "[::1]:3100".parse().unwrap();
        assert!(!should_warn_non_loopback_bind(&addr, None));
    }

    /// F-33: 0.0.0.0 + 明示 allowed_hosts → warn 不要
    /// (operator が意図して LAN 公開 + Host 許可を設定している)。
    #[test]
    fn test_no_warn_on_unspecified_bind_with_explicit_allowed_hosts() {
        let addr: SocketAddr = "0.0.0.0:3100".parse().unwrap();
        let hosts = ["kb.example.lan".to_string(), "192.168.1.10".to_string()];
        assert!(!should_warn_non_loopback_bind(&addr, Some(&hosts)));
    }

    /// F-33: 0.0.0.0 + 空 allowed_hosts → warn 不要
    /// (operator が `allowed_hosts = []` で明示的に Host 検証を無効化
    /// した = 警告対象外。disable_allowed_hosts() 相当の自己責任設定)。
    #[test]
    fn test_no_warn_on_unspecified_bind_with_empty_allowed_hosts() {
        let addr: SocketAddr = "0.0.0.0:3100".parse().unwrap();
        let hosts: [String; 0] = [];
        assert!(!should_warn_non_loopback_bind(&addr, Some(&hosts)));
    }

    /// F-33: LAN IP (192.168.x.x) + default → warn が立つ。
    #[test]
    fn test_warn_on_lan_ip_bind_with_default_allowed_hosts() {
        let addr: SocketAddr = "192.168.1.10:3100".parse().unwrap();
        assert!(should_warn_non_loopback_bind(&addr, None));
    }

    // -----------------------------------------------------------------------
    // F-64: /healthz Host check middleware (healthz_public opt-in).
    // -----------------------------------------------------------------------

    use axum::http::Request as HttpRequest;
    use tower::ServiceExt;

    /// Build a minimal Router with only the `/healthz` route, mirroring the
    /// `run_http` pattern but without spawning an actual TCP server.
    fn build_test_router(healthz_public: bool, allowed_hosts: Option<Vec<String>>) -> Router {
        if healthz_public {
            Router::new().route("/healthz", get(healthz))
        } else {
            let allowed_state = Arc::new(allowed_hosts);
            Router::new()
                .route("/healthz", get(healthz))
                .layer(middleware::from_fn_with_state(
                    allowed_state,
                    healthz_host_check,
                ))
        }
    }

    /// `healthz_public = true` (default) なら任意 Host から 200。
    #[tokio::test]
    async fn test_healthz_public_true_allows_any_host() {
        let app = build_test_router(true, None);
        let req = HttpRequest::builder()
            .uri("/healthz")
            .header("host", "evil.example")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    /// `healthz_public = false` + 明示 allow-list で allowlisted Host から 200。
    #[tokio::test]
    async fn test_healthz_public_false_with_explicit_allowed_hosts_allows_allowlisted() {
        let app = build_test_router(false, Some(vec!["custom.example".into()]));
        let req = HttpRequest::builder()
            .uri("/healthz")
            .header("host", "custom.example")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    /// `healthz_public = false` + 明示 allow-list で non-allowlisted Host から 403。
    #[tokio::test]
    async fn test_healthz_public_false_with_explicit_allowed_hosts_rejects_non_allowlisted() {
        let app = build_test_router(false, Some(vec!["custom.example".into()]));
        let req = HttpRequest::builder()
            .uri("/healthz")
            .header("host", "evil.example")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }

    /// `healthz_public = false` + `allowed_hosts = None` → rmcp default
    /// loopback list 互換 (= localhost / 127.0.0.1 / ::1 のみ pass)。
    #[tokio::test]
    async fn test_healthz_public_false_with_none_allowed_hosts_uses_loopback_default() {
        // non-loopback Host → 403
        let app1 = build_test_router(false, None);
        let req1 = HttpRequest::builder()
            .uri("/healthz")
            .header("host", "evil.example")
            .body(Body::empty())
            .unwrap();
        let resp_evil = app1.oneshot(req1).await.unwrap();
        assert_eq!(resp_evil.status(), StatusCode::FORBIDDEN);

        // loopback Host → 200
        let app2 = build_test_router(false, None);
        let req2 = HttpRequest::builder()
            .uri("/healthz")
            .header("host", "localhost")
            .body(Body::empty())
            .unwrap();
        let resp_loopback = app2.oneshot(req2).await.unwrap();
        assert_eq!(resp_loopback.status(), StatusCode::OK);
    }

    /// `healthz_public = false` + `allowed_hosts = Some(empty)` → 全許可
    /// (= rmcp の `disable_allowed_hosts` 相当、operator 自己責任 opt-out)。
    #[tokio::test]
    async fn test_healthz_public_false_with_empty_allowed_hosts_allows_any() {
        let app = build_test_router(false, Some(vec![]));
        let req = HttpRequest::builder()
            .uri("/healthz")
            .header("host", "anything.example")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    /// codex P2 (#50): IPv6 literal `[::1]:3100` Host header が
    /// default loopback list の `::1` と match。`split(':').next()` 罠 fix の
    /// regression check。
    #[tokio::test]
    async fn test_healthz_public_false_with_ipv6_loopback_host_header() {
        // `Host: [::1]:3100` (IPv6 loopback w/ port) は default loopback と一致
        let app = build_test_router(false, None);
        let req = HttpRequest::builder()
            .uri("/healthz")
            .header("host", "[::1]:3100")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    /// codex P2 (#50): allow-list が port 込みで `"192.168.1.10:3100"` でも
    /// 同 Host header から match (kb-mcp.toml.example の document 例と整合)。
    #[tokio::test]
    async fn test_healthz_public_false_with_port_included_allowlist_entry() {
        // 同じ port 込み entry → full Host header の比較で match
        let app1 = build_test_router(false, Some(vec!["192.168.1.10:3100".into()]));
        let req1 = HttpRequest::builder()
            .uri("/healthz")
            .header("host", "192.168.1.10:3100")
            .body(Body::empty())
            .unwrap();
        let resp = app1.oneshot(req1).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        // host-only entry でも port 付き Host header が match (host-only の比較)
        let app2 = build_test_router(false, Some(vec!["192.168.1.10".into()]));
        let req2 = HttpRequest::builder()
            .uri("/healthz")
            .header("host", "192.168.1.10:3100")
            .body(Body::empty())
            .unwrap();
        let resp = app2.oneshot(req2).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    // --- extract_host_part unit tests ---

    #[test]
    fn test_extract_host_part_ipv4_with_port() {
        assert_eq!(extract_host_part("192.168.1.10:3100"), "192.168.1.10");
    }

    #[test]
    fn test_extract_host_part_ipv4_without_port() {
        assert_eq!(extract_host_part("192.168.1.10"), "192.168.1.10");
    }

    #[test]
    fn test_extract_host_part_hostname_with_port() {
        assert_eq!(extract_host_part("localhost:3100"), "localhost");
    }

    #[test]
    fn test_extract_host_part_ipv6_with_port() {
        assert_eq!(extract_host_part("[::1]:3100"), "::1");
    }

    #[test]
    fn test_extract_host_part_ipv6_without_port() {
        assert_eq!(extract_host_part("[::1]"), "::1");
    }

    #[test]
    fn test_extract_host_part_empty() {
        assert_eq!(extract_host_part(""), "");
    }

    /// codex P2 (#50 round 2): Host header 不在時に URI authority を
    /// fallback として読む (= HTTP/2 / proxy-forwarded request の `:authority`
    /// pseudo-header 互換)。Host header を **付けず**、URI に
    /// `http://localhost/healthz` を渡して authority 経由で match。
    #[tokio::test]
    async fn test_healthz_public_false_falls_back_to_uri_authority_when_host_missing() {
        let app = build_test_router(false, None);
        // No `Host` header. URI carries the authority (= `localhost`).
        let req = HttpRequest::builder()
            .uri("http://localhost/healthz")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    /// codex P1 (#50 round 5): malformed bracketed Host (`[::1]evil.example`)
    /// が host-only に正規化されて allow-list bypass になる security 罠の
    /// regression test。loopback default で 403 を返すこと。
    #[tokio::test]
    async fn test_healthz_public_false_rejects_malformed_bracketed_host() {
        let app = build_test_router(false, None);
        let req = HttpRequest::builder()
            .uri("/healthz")
            .header("host", "[::1]evil.example")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }

    /// codex P2 (#50 round 6): u16 範囲外 port (`99999`) が parse できないことを
    /// 確認 = `Host: localhost:99999` が loopback default で 403。
    #[tokio::test]
    async fn test_healthz_public_false_rejects_invalid_port() {
        let app = build_test_router(false, None);
        let req = HttpRequest::builder()
            .uri("/healthz")
            .header("host", "localhost:99999")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }

    /// codex P2 (#50 round 6): IPv6 literal でも u16 範囲外 port は reject。
    #[tokio::test]
    async fn test_healthz_public_false_rejects_invalid_port_ipv6() {
        let app = build_test_router(false, None);
        let req = HttpRequest::builder()
            .uri("/healthz")
            .header("host", "[::1]:99999")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }

    /// codex P2 (#50 round 7): port は u16 numeric 比較 (= `"080"` == `"80"`)。
    /// rmcp の Authority::try_from と同 semantics。
    #[tokio::test]
    async fn test_healthz_public_false_normalizes_port_numerically() {
        // allow `"example.com:80"` + incoming `Host: example.com:080` → 200
        // (= zero-padded port を numeric 比較で同値扱い)
        let app = build_test_router(false, Some(vec!["example.com:80".into()]));
        let req = HttpRequest::builder()
            .uri("/healthz")
            .header("host", "example.com:080")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    /// codex P2 (#50 round 4): allow-list entry が **port 込み** の場合は
    /// incoming Host header の port も **strict 一致** (= port-aware)。
    /// `["example.com:8080"]` は `Host: example.com:9999` を accept しない。
    #[tokio::test]
    async fn test_healthz_public_false_with_port_qualified_allowlist_strict() {
        // 同じ port → 200
        let app1 = build_test_router(false, Some(vec!["example.com:8080".into()]));
        let req1 = HttpRequest::builder()
            .uri("/healthz")
            .header("host", "example.com:8080")
            .body(Body::empty())
            .unwrap();
        let resp1 = app1.oneshot(req1).await.unwrap();
        assert_eq!(resp1.status(), StatusCode::OK);

        // 異なる port → 403 (codex round 4 fix の核心)
        let app2 = build_test_router(false, Some(vec!["example.com:8080".into()]));
        let req2 = HttpRequest::builder()
            .uri("/healthz")
            .header("host", "example.com:9999")
            .body(Body::empty())
            .unwrap();
        let resp2 = app2.oneshot(req2).await.unwrap();
        assert_eq!(resp2.status(), StatusCode::FORBIDDEN);

        // port 抜きの incoming Host も 403 (allow が port 指定なので strict)
        let app3 = build_test_router(false, Some(vec!["example.com:8080".into()]));
        let req3 = HttpRequest::builder()
            .uri("/healthz")
            .header("host", "example.com")
            .body(Body::empty())
            .unwrap();
        let resp3 = app3.oneshot(req3).await.unwrap();
        assert_eq!(resp3.status(), StatusCode::FORBIDDEN);
    }

    /// codex P2 (#50 round 3): allow-list entry も normalize して比較。
    /// `["[::1]"]` (= bracketed IPv6 entry) は incoming `Host: [::1]:3100`
    /// (or `Host: ::1`) と match (= rmcp の `with_allowed_hosts` 互換)。
    #[tokio::test]
    async fn test_healthz_public_false_with_bracketed_ipv6_allowlist_entry() {
        // allow-list 側も extract_host_part で normalize されるので、
        // bracketed entry が bracketed Host と match
        let app1 = build_test_router(false, Some(vec!["[::1]".into()]));
        let req1 = HttpRequest::builder()
            .uri("/healthz")
            .header("host", "[::1]:3100")
            .body(Body::empty())
            .unwrap();
        let resp = app1.oneshot(req1).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        // allow-list `["[::1]"]` + incoming Host `[::1]` (no port) も match
        let app2 = build_test_router(false, Some(vec!["[::1]".into()]));
        let req2 = HttpRequest::builder()
            .uri("/healthz")
            .header("host", "[::1]")
            .body(Body::empty())
            .unwrap();
        let resp = app2.oneshot(req2).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        // allow-list `["::1"]` (= host-only) + incoming bracketed Host も match
        let app3 = build_test_router(false, Some(vec!["::1".into()]));
        let req3 = HttpRequest::builder()
            .uri("/healthz")
            .header("host", "[::1]:3100")
            .body(Body::empty())
            .unwrap();
        let resp = app3.oneshot(req3).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }
}
