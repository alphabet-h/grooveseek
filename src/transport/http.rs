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

/// `Host` header から host portion を取り出す (port を除外)。
/// RFC 7230 の Host header 文法に従う:
/// - IPv6 literal: `[::1]:3100` → `::1` (= 角括弧と port を剥がす)
/// - IPv6 literal w/o port: `[::1]` → `::1`
/// - IPv4 / hostname w/ port: `localhost:3100` → `localhost`、`192.168.1.10:3100` → `192.168.1.10`
/// - IPv4 / hostname w/o port: そのまま
///
/// codex P2 (#50): 単純な `split(':').next()` だと IPv6 が `[` だけになり、
/// またユーザが `allowed_hosts = ["192.168.1.10:3100"]` のように port 込みで
/// 列挙したとき (kb-mcp.toml.example の document 例) match しなくなる。
/// 本関数は port を剥がした「host のみ」を返し、呼び出し側は full Host
/// header と host-only の **両方** を allow-list と比較することで両 form の
/// allow-list entry を許容する。
fn extract_host_part(host_raw: &str) -> &str {
    // IPv6 literal: `[ipv6]:port` または `[ipv6]`
    if let Some(rest) = host_raw.strip_prefix('[')
        && let Some(end) = rest.find(']')
    {
        return &rest[..end];
    }
    // IPv4 / hostname: 最後の `:` で port を剥がす (port は all-digit のはず)
    if let Some(colon) = host_raw.rfind(':') {
        let port_part = &host_raw[colon + 1..];
        if !port_part.is_empty() && port_part.chars().all(|c| c.is_ascii_digit()) {
            return &host_raw[..colon];
        }
    }
    host_raw
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
    let host_full = headers
        .get("host")
        .and_then(|h| h.to_str().ok())
        .unwrap_or("");
    let host_part = extract_host_part(host_full);

    // allow-list entry を full Host header / host-only の両方と比較。
    let matches = |allow: &str| -> bool {
        allow.eq_ignore_ascii_case(host_full) || allow.eq_ignore_ascii_case(host_part)
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
}
