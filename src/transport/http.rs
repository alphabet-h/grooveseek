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

/// `/healthz` 用 Host validation の reject 理由。
/// HTTP status code への mapping は middleware 側で決定:
/// - `MissingHost` / `MalformedHost` → 400 Bad Request (= rmcp parity)
/// - `NotAllowed` → 403 Forbidden (= DNS rebinding 試行想定)
///
/// Encoding error (= `HeaderValue::to_str()` 失敗) は middleware 内で helper を
/// 経由せず直接返すため、本 enum には対応 variant を持たせない (= dead variant 回避)。
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(dead_code)] // Task 7 で `healthz_host_check` middleware が helper を呼ぶようになったら外す
pub(crate) enum HostRejection {
    /// Host header と URI authority の双方が不在。
    MissingHost,
    /// Host header の文字列が `Authority::try_from` で parse 失敗、または
    /// kb-mcp 拡張の defensive reject (= userinfo / port out-of-range)。
    MalformedHost,
    /// parse 成功したが allow-list と一致しなかった。
    NotAllowed,
}

/// Allow-list entry / incoming Host header の比較用 normalized form。
/// rmcp 1.4 `tower.rs::NormalizedAuthority` (line 169-180) の mirror。
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(dead_code)] // Task 7 で middleware が helper を呼ぶようになったら外す
struct NormalizedAuthority {
    /// host: bracket 剥がし + ASCII lowercase。`Authority::host()` は IPv6 で
    /// brackets を含む文字列を返すため、`trim_matches('[' / ']')` + lowercase 化。
    host: String,
    /// port: `Authority::port_u16()` を u16 として保持、port なしは `None`。
    port: Option<u16>,
}

#[allow(dead_code)] // Task 7 で middleware が helper を呼ぶようになったら外す
impl NormalizedAuthority {
    /// 既に parse 済の `Authority` から作る (= incoming Host header 用、infallible)。
    fn from_authority(authority: &http::uri::Authority) -> Self {
        Self {
            host: authority
                .host()
                .trim_matches('[')
                .trim_matches(']')
                .to_ascii_lowercase(),
            port: authority.port_u16(),
        }
    }

    /// allow-list entry の raw 文字列から作る (= rmcp `parse_allowed_authority`
    /// line 182-193 mirror、infallible で fallback semantics)。
    fn from_allowed_entry(raw: &str) -> Self {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Self {
                host: String::new(),
                port: None,
            };
        }
        if let Ok(authority) = http::uri::Authority::try_from(trimmed) {
            return Self::from_authority(&authority);
        }
        // try_from 失敗 = fallback: raw を host-only として保存
        // (= unbracketed IPv6 `"::1"` のような config 形式を救済)
        Self {
            host: trimmed
                .trim_matches('[')
                .trim_matches(']')
                .to_ascii_lowercase(),
            port: None,
        }
    }

    /// host eq + port-strict / port-agnostic match。
    /// rmcp `host_is_allowed` line 200-209 mirror。
    fn matches(&self, incoming: &Self) -> bool {
        if self.host != incoming.host {
            return false;
        }
        match self.port {
            Some(p) => incoming.port == Some(p), // strict
            None => true,                        // port-agnostic
        }
    }
}

/// `host:port` form の port 部分が空でない explicit port suffix を持つか判定。
/// `port_u16().is_none() && has_explicit_port_suffix(raw)` の組み合わせで
/// port out-of-range silent degrade (`"localhost:99999"` 等) を検知する。
///
/// 入力前提: `Authority::try_from` 成功後に呼ばれる post-check のため、
/// malformed bracketed (= 二重 `]`、不一致 `[`) は到達しない。
#[allow(dead_code)] // Task 7 で middleware が helper を呼ぶようになったら外す
fn has_explicit_port_suffix(raw: &str) -> bool {
    // bracketed: `]:` の後ろを見る
    if let Some(end) = raw.find(']') {
        let after = &raw[end + 1..];
        return after.starts_with(':') && after.len() > 1;
    }
    // unbracketed IPv6 (= 3 つ以上の `:`): port なし扱い
    if raw.split(':').count() >= 3 {
        return false;
    }
    // unbracketed `host:port`: 末尾 `:` の後ろが non-empty
    if let Some((_, port)) = raw.rsplit_once(':') {
        return !port.is_empty();
    }
    false
}

/// rmcp 1.4 default loopback list の mirror。本 helper では IPv6 を **bracketed**
/// (`"[::1]"`) で保持。allow-list 側は `NormalizedAuthority::from_allowed_entry`
/// の fallback で unbracketed (`"::1"`) も同等扱いされるため、`Authority::try_from`
/// が parse できる bracketed 形式を一次形にすると helper 内 normalize が単純化される。
const DEFAULT_LOOPBACK_HOSTS: &[&str] = &["localhost", "127.0.0.1", "[::1]"];

/// `/healthz` 用 Host validation の pure helper (no I/O、test 容易)。
///
/// 引数:
/// - `host_raw`: HTTP Host header 文字列、または URI authority の文字列
///   (HTTP/2 / proxy-forwarded fallback)。両方不在なら `None` で `MissingHost`
/// - `allowed`:
///   - `None` → `DEFAULT_LOOPBACK_HOSTS` ("localhost" / "127.0.0.1" / "[::1]")
///   - `Some(&[])` → 全許可 (= rmcp `disable_allowed_hosts` 相当)
///   - `Some(&[..])` → 厳密 match
///
/// 比較 semantics:
/// - host parse は `http::uri::Authority::try_from` 委譲、失敗 → `MalformedHost`
/// - allow-list entry は rmcp `parse_allowed_authority` mirror で fallback
///   (= unbracketed IPv6 config 救済)
/// - host comparison: `Authority::host()` の bracket を `trim_matches('[' / ']')`
///   + ASCII lowercase で正規化 (= rmcp `normalize_host` mirror)
/// - port: allow に port 指定あり → strict 一致、なし → port-agnostic
/// - kb-mcp 拡張の defensive reject: userinfo (`user@`) / port out-of-range
#[allow(dead_code)] // Task 7 で `healthz_host_check` middleware が helper を呼ぶようになったら外す
pub(crate) fn validate_host_header(
    host_raw: Option<&str>,
    allowed: Option<&[String]>,
) -> Result<(), HostRejection> {
    let raw = host_raw.ok_or(HostRejection::MissingHost)?;

    // userinfo pre-check: Authority::try_from("user@host") は Ok を返し
    // userinfo を strip するが、kb-mcp は defensive に reject する
    // (= authentication bypass の予兆を operator log に残す)
    if raw.contains('@') {
        return Err(HostRejection::MalformedHost);
    }

    // bracketed IPv6 pre-check: `Authority::try_from` は `[::1]evil.example` を
    // Ok で返す (host=`[::1]`、as_str に trailing garbage 保持) pitfall がある。
    // また `[]:80` (= empty host) も Ok で通る。本前段で **input が `[` で始まる
    // なら必ず単一 `]` を含み、`]` の直後は空 or `:<port>` のみ、bracket 内 host
    // は non-empty** という constraint を defensive に check する。
    if raw.starts_with('[') {
        match raw.find(']') {
            None => return Err(HostRejection::MalformedHost),
            Some(end) => {
                // bracket 内 host が空 (`[]:80` 等) は reject
                if end == 1 {
                    return Err(HostRejection::MalformedHost);
                }
                let after = &raw[end + 1..];
                // `]` の後ろは空 or `:port` のみ valid
                // (= `[::1]evil.example` の trailing garbage を reject)
                if !after.is_empty() && !after.starts_with(':') {
                    return Err(HostRejection::MalformedHost);
                }
            }
        }
    }

    let authority =
        http::uri::Authority::try_from(raw).map_err(|_| HostRejection::MalformedHost)?;

    // port out-of-range post-check: Authority::try_from("localhost:99999") は
    // Ok を返し port_u16() が None に degrade する pitfall。明示 reject。
    if authority.port_u16().is_none() && has_explicit_port_suffix(raw) {
        return Err(HostRejection::MalformedHost);
    }

    let incoming = NormalizedAuthority::from_authority(&authority);

    // allow-list 解決: None → loopback default、Some(empty) → 全許可、
    // Some(non_empty) → 厳密 match。`DEFAULT_LOOPBACK_HOSTS` は `&[&str]` 型のため
    // Iterator pattern で各 entry を順次 normalize して any 検査する形に展開
    // (= 不要な Vec allocation 回避)。
    let any_match = match allowed {
        None => DEFAULT_LOOPBACK_HOSTS
            .iter()
            .any(|e| NormalizedAuthority::from_allowed_entry(e).matches(&incoming)),
        Some([]) => return Ok(()),
        Some(v) => v
            .iter()
            .any(|e| NormalizedAuthority::from_allowed_entry(e.as_str()).matches(&incoming)),
    };

    if any_match {
        Ok(())
    } else {
        Err(HostRejection::NotAllowed)
    }
}

/// 400 Bad Request response builder。
/// rmcp `tower.rs::bad_request_response` (line 212-220) と byte-identical body:
/// - status: 400
/// - body: `format!("Bad Request: {msg}")`
/// - Content-Type: `text/plain; charset=utf-8`
///
/// 呼び出し側は prefix を **含めない** 文字列を渡すこと
/// (= 内部で `"Bad Request: "` を付加するため二重付与防止)。
#[allow(dead_code)] // Task 7 で middleware が builder を呼ぶようになったら外す
fn bad_request_typed(msg: &str) -> Response {
    Response::builder()
        .status(StatusCode::BAD_REQUEST)
        .header(http::header::CONTENT_TYPE, "text/plain; charset=utf-8")
        .body(Body::from(format!("Bad Request: {msg}")))
        .expect("static response build")
}

/// 403 Forbidden response builder。
/// rmcp `tower.rs::forbidden_response` (line 156-161) と byte-identical:
/// - status: 403
/// - body: `format!("Forbidden: {msg}")`
/// - Content-Type: (なし、rmcp と同じく非設定)
#[allow(dead_code)] // Task 7 で middleware が builder を呼ぶようになったら外す
fn forbidden_plain(msg: &str) -> Response {
    Response::builder()
        .status(StatusCode::FORBIDDEN)
        .body(Body::from(format!("Forbidden: {msg}")))
        .expect("static response build")
}

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

    use axum::body::to_bytes;
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
        let body = to_bytes(resp.into_body(), 1024).await.unwrap();
        assert!(
            body.starts_with(b"Forbidden: Host header is not allowed"),
            "body should match rmcp forbidden_response, got: {:?}",
            String::from_utf8_lossy(&body)
        );
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
        let body = to_bytes(resp_evil.into_body(), 1024).await.unwrap();
        assert!(
            body.starts_with(b"Forbidden: Host header is not allowed"),
            "body should match rmcp forbidden_response, got: {:?}",
            String::from_utf8_lossy(&body)
        );

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
    /// regression test。rmcp parity で 400 Bad Request を返すこと。
    #[tokio::test]
    async fn test_healthz_public_false_rejects_malformed_bracketed_host() {
        let app = build_test_router(false, None);
        let req = HttpRequest::builder()
            .uri("/healthz")
            .header("host", "[::1]evil.example")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let body = to_bytes(resp.into_body(), 1024).await.unwrap();
        assert!(
            body.starts_with(b"Bad Request: Invalid Host header"),
            "body should match rmcp bad_request, got: {:?}",
            String::from_utf8_lossy(&body)
        );
    }

    /// codex P2 (#50 round 6): u16 範囲外 port (`99999`) が parse できないことを
    /// 確認 = `Host: localhost:99999` は rmcp parity で 400 Bad Request。
    #[tokio::test]
    async fn test_healthz_public_false_rejects_invalid_port() {
        let app = build_test_router(false, None);
        let req = HttpRequest::builder()
            .uri("/healthz")
            .header("host", "localhost:99999")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let body = to_bytes(resp.into_body(), 1024).await.unwrap();
        assert!(
            body.starts_with(b"Bad Request: Invalid Host header"),
            "body should match rmcp bad_request, got: {:?}",
            String::from_utf8_lossy(&body)
        );
    }

    /// codex P2 (#50 round 6): IPv6 literal でも u16 範囲外 port は reject (400)。
    #[tokio::test]
    async fn test_healthz_public_false_rejects_invalid_port_ipv6() {
        let app = build_test_router(false, None);
        let req = HttpRequest::builder()
            .uri("/healthz")
            .header("host", "[::1]:99999")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let body = to_bytes(resp.into_body(), 1024).await.unwrap();
        assert!(
            body.starts_with(b"Bad Request: Invalid Host header"),
            "body should match rmcp bad_request, got: {:?}",
            String::from_utf8_lossy(&body)
        );
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
        let body2 = to_bytes(resp2.into_body(), 1024).await.unwrap();
        assert!(
            body2.starts_with(b"Forbidden: Host header is not allowed"),
            "body should match rmcp forbidden_response, got: {:?}",
            String::from_utf8_lossy(&body2)
        );

        // port 抜きの incoming Host も 403 (allow が port 指定なので strict)
        let app3 = build_test_router(false, Some(vec!["example.com:8080".into()]));
        let req3 = HttpRequest::builder()
            .uri("/healthz")
            .header("host", "example.com")
            .body(Body::empty())
            .unwrap();
        let resp3 = app3.oneshot(req3).await.unwrap();
        assert_eq!(resp3.status(), StatusCode::FORBIDDEN);
        let body3 = to_bytes(resp3.into_body(), 1024).await.unwrap();
        assert!(
            body3.starts_with(b"Forbidden: Host header is not allowed"),
            "body should match rmcp forbidden_response, got: {:?}",
            String::from_utf8_lossy(&body3)
        );
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

    // ===========================================================================
    // feature-39 / D-11: NormalizedAuthority unit tests
    // ===========================================================================

    #[test]
    fn test_normalized_authority_from_authority_strips_brackets_and_lowercases() {
        let auth = http::uri::Authority::try_from("[::1]:80").unwrap();
        let normalized = NormalizedAuthority::from_authority(&auth);
        assert_eq!(normalized.host, "::1");
        assert_eq!(normalized.port, Some(80));
    }

    #[test]
    fn test_normalized_authority_from_authority_uppercase_hostname() {
        let auth = http::uri::Authority::try_from("EXAMPLE.COM:8080").unwrap();
        let normalized = NormalizedAuthority::from_authority(&auth);
        assert_eq!(normalized.host, "example.com");
        assert_eq!(normalized.port, Some(8080));
    }

    #[test]
    fn test_normalized_authority_from_allowed_entry_unbracketed_ipv6_fallback() {
        // rmcp `parse_allowed_authority` mirror: try_from 失敗で raw fallback
        let normalized = NormalizedAuthority::from_allowed_entry("::1");
        assert_eq!(normalized.host, "::1");
        assert_eq!(normalized.port, None);
    }

    #[test]
    fn test_normalized_authority_matches_port_strict() {
        let allow = NormalizedAuthority::from_allowed_entry("example.com:8080");
        let incoming_match = NormalizedAuthority::from_authority(
            &http::uri::Authority::try_from("example.com:8080").unwrap(),
        );
        let incoming_diff = NormalizedAuthority::from_authority(
            &http::uri::Authority::try_from("example.com:9999").unwrap(),
        );
        assert!(allow.matches(&incoming_match));
        assert!(!allow.matches(&incoming_diff));
    }

    #[test]
    fn test_normalized_authority_matches_port_agnostic_when_allow_has_no_port() {
        let allow = NormalizedAuthority::from_allowed_entry("example.com");
        let incoming = NormalizedAuthority::from_authority(
            &http::uri::Authority::try_from("example.com:8080").unwrap(),
        );
        assert!(allow.matches(&incoming)); // allow に port なし = port-agnostic
    }

    // ===========================================================================
    // feature-39 / D-11: has_explicit_port_suffix unit tests (#30-#37)
    // ===========================================================================

    #[test]
    fn test_has_explicit_port_suffix_hostname_no_colon() {
        // #30: localhost (= hostname、colon なし) → false
        assert!(!has_explicit_port_suffix("localhost"));
    }

    #[test]
    fn test_has_explicit_port_suffix_hostname_with_port() {
        // #31: localhost:80 → true
        assert!(has_explicit_port_suffix("localhost:80"));
    }

    #[test]
    fn test_has_explicit_port_suffix_hostname_empty_port() {
        // #32: localhost: (= 末尾 colon、port 部空) → false
        assert!(!has_explicit_port_suffix("localhost:"));
    }

    #[test]
    fn test_has_explicit_port_suffix_bracketed_ipv6_no_port() {
        // #33: [::1] (= bracketed IPv6 without port) → false
        assert!(!has_explicit_port_suffix("[::1]"));
    }

    #[test]
    fn test_has_explicit_port_suffix_bracketed_ipv6_with_port() {
        // #34: [::1]:80 → true
        assert!(has_explicit_port_suffix("[::1]:80"));
    }

    #[test]
    fn test_has_explicit_port_suffix_bracketed_ipv6_empty_port() {
        // #35: [::1]: (= bracketed IPv6 with empty port) → false
        assert!(!has_explicit_port_suffix("[::1]:"));
    }

    #[test]
    fn test_has_explicit_port_suffix_unbracketed_ipv6() {
        // #36: ::1 (= unbracketed IPv6、3 つ以上の colon) → false
        // 注: production code では Authority::try_from("::1") が Err を返すため
        // post-check に到達しないが、単体 fn の境界 case として検証
        assert!(!has_explicit_port_suffix("::1"));
    }

    #[test]
    fn test_has_explicit_port_suffix_ipv4_no_colon() {
        // #37: 0.0.0.0 (= IPv4、colon なし) → false
        assert!(!has_explicit_port_suffix("0.0.0.0"));
    }

    // ===========================================================================
    // feature-39 / D-11: validate_host_header unit tests (#1-#28)
    // ===========================================================================

    fn allow(entries: &[&str]) -> Vec<String> {
        entries.iter().map(|s| s.to_string()).collect()
    }

    // ---- 正常系 (Ok) — 9 件 ----

    #[test]
    fn test_validate_host_header_hostname_only_ok() {
        // #1: hostname-only allow + hostname-only Host
        assert_eq!(
            validate_host_header(Some("localhost"), Some(&allow(&["localhost"]))),
            Ok(())
        );
    }

    #[test]
    fn test_validate_host_header_ipv4_with_and_without_port() {
        // #2: IPv4 allow + IPv4 Host (port なし、port あり)
        assert_eq!(
            validate_host_header(Some("192.168.1.10"), Some(&allow(&["192.168.1.10"]))),
            Ok(())
        );
        assert_eq!(
            validate_host_header(
                Some("192.168.1.10:3100"),
                Some(&allow(&["192.168.1.10:3100"]))
            ),
            Ok(())
        );
    }

    #[test]
    fn test_validate_host_header_bracketed_ipv6_ok() {
        // #3: bracketed IPv6 allow + bracketed IPv6 Host (port なし / あり)
        assert_eq!(
            validate_host_header(Some("[::1]"), Some(&allow(&["[::1]"]))),
            Ok(())
        );
        assert_eq!(
            validate_host_header(Some("[::1]:3100"), Some(&allow(&["[::1]"]))),
            Ok(())
        );
    }

    #[test]
    fn test_validate_host_header_unbracketed_ipv6_config_with_bracketed_host() {
        // #4: unbracketed IPv6 config allow ["::1"] + bracketed Host [::1]:3100
        // (rmcp parse_allowed_authority mirror の救済)
        assert_eq!(
            validate_host_header(Some("[::1]:3100"), Some(&allow(&["::1"]))),
            Ok(())
        );
    }

    #[test]
    fn test_validate_host_header_case_insensitive() {
        // #5: 大文字 hostname EXAMPLE.COM + 小文字 allow
        assert_eq!(
            validate_host_header(Some("EXAMPLE.COM"), Some(&allow(&["example.com"]))),
            Ok(())
        );
    }

    #[test]
    fn test_validate_host_header_port_numeric_normalize() {
        // #6: Host: example.com:080 + allow: example.com:80 → Ok
        // (port_u16() が "080" を 80 に正規化)
        assert_eq!(
            validate_host_header(Some("example.com:080"), Some(&allow(&["example.com:80"]))),
            Ok(())
        );
    }

    #[test]
    fn test_validate_host_header_port_agnostic_allow() {
        // #7: allow: example.com (port なし) → Host: example.com:8080 も Ok
        assert_eq!(
            validate_host_header(Some("example.com:8080"), Some(&allow(&["example.com"]))),
            Ok(())
        );
    }

    #[test]
    fn test_validate_host_header_some_empty_allows_any() {
        // #8: Some(empty) allow → 任意 Host が Ok
        assert_eq!(
            validate_host_header(Some("evil.example"), Some(&allow(&[]))),
            Ok(())
        );
    }

    #[test]
    fn test_validate_host_header_none_uses_loopback_default() {
        // #9: None allow → loopback 3 entry のみ Ok
        assert_eq!(validate_host_header(Some("localhost"), None), Ok(()));
        assert_eq!(validate_host_header(Some("127.0.0.1"), None), Ok(()));
        assert_eq!(validate_host_header(Some("[::1]"), None), Ok(()));
        // non-loopback は NotAllowed
        assert_eq!(
            validate_host_header(Some("evil.example"), None),
            Err(HostRejection::NotAllowed)
        );
    }

    // ---- MalformedHost (400) — 8 件 ----

    #[test]
    fn test_validate_host_header_unbracketed_ipv6_in_host_rejected() {
        // #10: unbracketed IPv6 in Host header (rmcp parity で reject)
        assert_eq!(
            validate_host_header(Some("::1"), None),
            Err(HostRejection::MalformedHost)
        );
    }

    #[test]
    fn test_validate_host_header_malformed_bracketed() {
        // #11: malformed bracketed [::1]evil.example
        assert_eq!(
            validate_host_header(Some("[::1]evil.example"), None),
            Err(HostRejection::MalformedHost)
        );
    }

    #[test]
    fn test_validate_host_header_unclosed_bracket() {
        // #12: unclosed bracket [::1
        assert_eq!(
            validate_host_header(Some("[::1"), None),
            Err(HostRejection::MalformedHost)
        );
    }

    #[test]
    fn test_validate_host_header_empty_bracket() {
        // #13: empty bracket []:80
        assert_eq!(
            validate_host_header(Some("[]:80"), None),
            Err(HostRejection::MalformedHost)
        );
    }

    #[test]
    fn test_validate_host_header_control_chars_rejected() {
        // #14: control char in host
        for ctrl in ["host\rname", "host\nname", "host\tname"] {
            assert_eq!(
                validate_host_header(Some(ctrl), None),
                Err(HostRejection::MalformedHost),
                "control char {ctrl:?} should be MalformedHost"
            );
        }
    }

    #[test]
    fn test_validate_host_header_userinfo_rejected() {
        // #15: userinfo (user@host) → defensive reject
        assert_eq!(
            validate_host_header(Some("user@host:80"), None),
            Err(HostRejection::MalformedHost)
        );
    }

    #[test]
    fn test_validate_host_header_null_byte_rejected() {
        // #16: control byte \x00
        assert_eq!(
            validate_host_header(Some("host\x00"), None),
            Err(HostRejection::MalformedHost)
        );
    }

    #[test]
    fn test_validate_host_header_port_out_of_range_rejected() {
        // #17: port out-of-range (localhost:99999) → MalformedHost
        // port_u16() が None に degrade するが、has_explicit_port_suffix で reject
        assert_eq!(
            validate_host_header(Some("localhost:99999"), None),
            Err(HostRejection::MalformedHost)
        );
        assert_eq!(
            validate_host_header(Some("[::1]:99999"), None),
            Err(HostRejection::MalformedHost)
        );
    }

    // ---- NotAllowed (403) — 4 件 ----

    #[test]
    fn test_validate_host_header_not_in_allowlist() {
        // #18: allow に無い hostname
        assert_eq!(
            validate_host_header(Some("evil.example"), Some(&allow(&["example.com"]))),
            Err(HostRejection::NotAllowed)
        );
    }

    #[test]
    fn test_validate_host_header_port_strict_mismatch() {
        // #19: port-strict 不一致
        assert_eq!(
            validate_host_header(
                Some("example.com:9999"),
                Some(&allow(&["example.com:8080"]))
            ),
            Err(HostRejection::NotAllowed)
        );
    }

    #[test]
    fn test_validate_host_header_port_strict_no_port_in_host() {
        // #20: port-strict + port なし Host
        assert_eq!(
            validate_host_header(Some("example.com"), Some(&allow(&["example.com:8080"]))),
            Err(HostRejection::NotAllowed)
        );
    }

    #[test]
    fn test_validate_host_header_ipv6_unauthorized() {
        // #21: IPv6 bracketed unauthorized
        assert_eq!(
            validate_host_header(Some("[::1]:3100"), Some(&allow(&["192.168.1.10"]))),
            Err(HostRejection::NotAllowed)
        );
    }

    // ---- MissingHost (400) — 1 件 ----

    #[test]
    fn test_validate_host_header_missing_when_none() {
        // #22: host_raw = None
        assert_eq!(
            validate_host_header(None, Some(&allow(&["localhost"]))),
            Err(HostRejection::MissingHost)
        );
    }

    // ---- rmcp parse_allowed_authority mirror — 3 件 ----

    #[test]
    fn test_validate_host_header_bracketed_allow_unbracketed_host_rejected() {
        // #23: allowed = ["[::1]"] + Host: ::1 → Host 側は MalformedHost で reject
        assert_eq!(
            validate_host_header(Some("::1"), Some(&allow(&["[::1]"]))),
            Err(HostRejection::MalformedHost)
        );
    }

    #[test]
    fn test_validate_host_header_bracketed_allow_bracketed_host() {
        // #24: allowed = ["[::1]"] + Host: [::1]:3100 → Ok
        assert_eq!(
            validate_host_header(Some("[::1]:3100"), Some(&allow(&["[::1]"]))),
            Ok(())
        );
    }

    #[test]
    fn test_validate_host_header_unbracketed_allow_bracketed_host() {
        // #25: allowed = ["::1"] + Host: [::1]:3100 → Ok (= unbracketed config 救済)
        assert_eq!(
            validate_host_header(Some("[::1]:3100"), Some(&allow(&["::1"]))),
            Ok(())
        );
    }

    // ---- non-ASCII / 高位 byte — 1 件 ----

    #[test]
    fn test_validate_host_header_non_ascii_high_byte_rejected() {
        // #26: non-ASCII 高位 byte (BOM 等) → Authority::try_from が Err → MalformedHost
        assert_eq!(
            validate_host_header(Some("\u{FEFF}example.com"), None),
            Err(HostRejection::MalformedHost)
        );
    }

    // ---- trailing dot — 2 件 ----

    #[test]
    fn test_validate_host_header_trailing_dot_not_in_allowlist() {
        // #27: allow ["example.com"] + Host: example.com. → NotAllowed
        // (Authority::try_from は Ok だが host() が trailing dot 保持で mismatch)
        assert_eq!(
            validate_host_header(Some("example.com."), Some(&allow(&["example.com"]))),
            Err(HostRejection::NotAllowed)
        );
    }

    #[test]
    fn test_validate_host_header_trailing_dot_explicitly_allowed() {
        // #28: allow ["example.com", "example.com."] + Host: example.com. → Ok
        assert_eq!(
            validate_host_header(
                Some("example.com."),
                Some(&allow(&["example.com", "example.com."]))
            ),
            Ok(())
        );
    }
}
