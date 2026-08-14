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
    Json, Router,
    body::Body,
    extract::{Request, State},
    http::{HeaderMap, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
};

use crate::server::{KbServer, KbServerShared};

/// HTTP transport が受理するリクエスト body の上限 (1 MiB)。
///
/// AU-17 の codex P1 指摘: `search` の filter 件数・要素長の検査は、rmcp が
/// body を全部 buffer して `SearchParams` に deserialize した **後** にしか
/// 走らない。100 万件の tags を載せた body は、はじかれる時点で既に
/// メモリと parse 時間を使い切っている。上限は body 自体にかける必要がある。
///
/// `axum::extract::DefaultBodyLimit` ではなく
/// `tower_http::limit::RequestBodyLimitLayer` を使うのは、前者が
/// 「extractor が上限 extension を読む」前提の仕組みで、`nest_service` した
/// rmcp の `StreamableHttpService` (自前で body を読む) には効かないため。
/// 後者は body を包むので、誰が読むかに依存しない。
///
/// 値の根拠: 正当な MCP リクエストで最大のものは `search` で、
/// filter 3 本が上限いっぱいでも 64 × 1 KiB × 3 = 192 KiB、query が 1 KiB。
/// 1 MiB はその 5 倍で、JSON-RPC の枠や session header を足しても十分余裕がある。
///
/// **stdio transport には同じ上限が無い**。こちらの client は同じユーザ権限で
/// 動くローカルプロセスであり、そもそもユーザにできることは何でもできるので、
/// body を絞っても守る対象が無い。守るべきは「ネットワーク越しに到達しうる」
/// HTTP 側だけ。
pub(crate) const REQUEST_BODY_MAX_BYTES: usize = 1024 * 1024;

/// 同時に生きていられる MCP session の既定上限 (BU-32)。
///
/// rmcp 1.4 は session 数の knob を持たない (`StreamableHttpServerConfig` にも
/// `LocalSessionManager` にも無く、あるのは per-session の `channel_capacity`
/// と、1 session 内の shadow stream 数を縛る `MAX_SHADOW_STREAMS = 32` だけ)。
///
/// 値の根拠は実測 (2026-08-14、release binary、1 本の keep-alive 接続):
/// **生きた session 1 つあたり約 100 KB**。256 なら約 25 MB で、
/// エディタ複数 + tray + CI から同時に繋いでも届かない余裕がある。
/// `[transport.http].max_sessions` で変更でき、`0` は無制限。
pub(crate) const DEFAULT_MAX_SESSIONS: u32 = 256;

// 既定値の意図を定義の隣で固定する。下限を割ると「複数クライアントの通常運用が
// 断られる」、上限を超えると「暴走したクライアントが数百 MB を確保できる」。
// どちらも既定値として選んだ理由に反するので、動かすなら根拠を測り直すこと。
const _: () = assert!(
    DEFAULT_MAX_SESSIONS >= 64,
    "a default this small would refuse ordinary multi-client use"
);
const _: () = assert!(
    DEFAULT_MAX_SESSIONS <= 1024,
    "at ~100 KB per live session, a default this large stops bounding anything"
);

/// 上限に達した時に `Retry-After` で返す秒数。
///
/// **見込みであって保証ではない**。空きが出るのは他のクライアントが切れた時か、
/// rmcp の per-session idle timeout (`SessionConfig::keep_alive`、既定 300 秒)
/// が発火した時で、どちらもこちらからは分からない。30 秒は「待たずに叩き続けるな」
/// を伝えるための値。
const SESSION_RETRY_AFTER_SECS: u32 = 30;

// ---------------------------------------------------------------------------
// (feature-43 PR-2) Admin endpoint response types + small ISO timestamp helper.
// ---------------------------------------------------------------------------

#[derive(serde::Serialize)]
pub struct AdminStatus {
    pub daemon: DaemonInfo,
    pub indexing: IndexingInfo,
    pub watcher: WatcherInfo,
    pub kb: crate::server::KbInfo,
    pub config_source: String,
}

#[derive(serde::Serialize)]
pub struct DaemonInfo {
    pub version: String,
    /// OS process id of the daemon answering this request.
    ///
    /// The tray needs it to stop the daemon at all. `Stop-ScheduledTask`
    /// terminates only the task's *own* process, and since v0.9.1 the Windows
    /// Action points at `kb-mcp-svc.exe`, which detach-spawns this process and
    /// exits immediately — so the scheduler considers the task finished and has
    /// nothing left to stop. Measured 2026-07-26: with a task whose action was
    /// still running, stopping it killed the parent and the child survived, so
    /// the task's reach does not extend to descendants either way.
    ///
    /// Only ever served from `/api/admin/status`, which `admin_host_check`
    /// restricts to loopback unless the operator explicitly allows their bind
    /// address. It is an identifier, not an action — stopping still requires
    /// local privileges to signal the process.
    pub pid: u32,
    pub uptime_secs: u64,
    pub started_at: String,
}

#[derive(serde::Serialize)]
pub struct IndexingInfo {
    pub active: bool,
    pub started_at: Option<String>,
    pub progress: Option<IndexingProgressView>,
}

#[derive(serde::Serialize)]
pub struct IndexingProgressView {
    pub current: u64,
    pub total: u64,
}

#[derive(serde::Serialize)]
pub struct WatcherInfo {
    pub active: bool,
    pub debounce_ms: u64,
}

/// Format a `SystemTime` as a minimal RFC3339 string (`YYYY-MM-DDTHH:MM:SSZ`,
/// seconds precision, UTC). Avoids pulling chrono just for this; uses Howard
/// Hinnant's civil-from-days algorithm so the conversion stays a pure fn.
fn format_iso(t: std::time::SystemTime) -> String {
    let dur = t.duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
    let secs = dur.as_secs() as i64;
    let days = secs.div_euclid(86400);
    let rem = secs.rem_euclid(86400);
    let h = rem / 3600;
    let m = (rem % 3600) / 60;
    let s = rem % 60;
    // Civil-from-days (Howard Hinnant 2013): days since 1970-01-01 → (y, m, d).
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { y + 1 } else { y };
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        year, month, d, h, m, s
    )
}

/// `/healthz` 用 Host validation の reject 理由。
/// HTTP status code への mapping は middleware 側で決定:
/// - `MissingHost` / `MalformedHost` → 400 Bad Request (= rmcp parity)
/// - `NotAllowed` → 403 Forbidden (= DNS rebinding 試行想定)
///
/// Encoding error (= `HeaderValue::to_str()` 失敗) は middleware 内で helper を
/// 経由せず直接返すため、本 enum には対応 variant を持たせない (= dead variant 回避)。
#[derive(Debug, Clone, PartialEq, Eq)]
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
struct NormalizedAuthority {
    /// host: bracket 剥がし + ASCII lowercase。`Authority::host()` は IPv6 で
    /// brackets を含む文字列を返すため、`trim_matches('[' / ']')` + lowercase 化。
    host: String,
    /// port: `Authority::port_u16()` を u16 として保持、port なしは `None`。
    port: Option<u16>,
}

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
    max_sessions: u32,
    shared: KbServerShared,
) -> Result<()> {
    // bind 範囲と allow-list の組合せが噛み合っていない時に warn を出す。
    if should_warn_non_loopback_bind(&addr, allowed_hosts.as_deref()) {
        tracing::warn!(
            bind = %addr,
            "{} kb-mcp has no authentication, and Host header validation is not a \
             substitute for it (any peer can send `Host: localhost`), so the bind \
             address is the only access control. Restrict reachability at the \
             network layer.",
            non_loopback_bind_symptom(allowed_hosts.as_deref()),
        );
    }

    // Session manager: LocalSessionManager keeps per-session state in memory.
    // Suitable for a single-process server (our deployment model).
    let session_manager = Arc::new(LocalSessionManager::default());

    // Service factory: invoked per new MCP session. Builds a fresh `KbServer`
    // handle that clones the Arc-shared heavy resources. The factory must
    // return `Result<_, std::io::Error>` per rmcp's trait.
    //
    // (feature-43 PR-2) The admin sub-router also needs `Arc<KbServerShared>`
    // in its state, so wrap `shared` in Arc upfront and clone for both the
    // factory closure and the admin router.
    let factory_shared = Arc::new(shared);
    let factory = {
        let f = Arc::clone(&factory_shared);
        move || -> Result<KbServer, std::io::Error> { Ok(KbServer::from_shared(&f)) }
    };

    let mcp_config = match allowed_hosts.clone() {
        Some(hosts) => StreamableHttpServerConfig::default().with_allowed_hosts(hosts),
        None => StreamableHttpServerConfig::default(),
    };
    let mcp_service = StreamableHttpService::new(factory, Arc::clone(&session_manager), mcp_config);

    // (BU-32) `/mcp` だけを門番の配下に置く。`.layer()` を merge 後の app に
    // 掛けると `/healthz` も `/ui` も `/api/*` も巻き込むので、nest する側で
    // 閉じておく。
    let mcp_router: Router =
        Router::new()
            .fallback_service(mcp_service)
            .layer(middleware::from_fn_with_state(
                McpSessionGate {
                    live: LiveSessionCount::Rmcp(session_manager),
                    max_sessions,
                    admissions: Arc::new(Admissions::default()),
                    refusals: Arc::new(RefusalLog::new()),
                },
                mcp_session_gate,
            ));

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

    // (feature-43 PR-2) Admin sub-router — loopback only via Host check
    // middleware. `/api/admin/*` lives here; the public sub-router (`/mcp`,
    // `/healthz`) is untouched, so admin gating cannot affect the MCP path.
    let admin_router = Router::new()
        .route("/api/admin/status", get(api_admin_status))
        .route("/api/search", post(api_search))
        .route("/ui", get(ui_index))
        .with_state(Arc::clone(&factory_shared))
        .layer(middleware::from_fn_with_state(
            Arc::clone(&factory_shared),
            admin_host_check,
        ));

    let app = healthz_router
        .merge(admin_router)
        .nest_service("/mcp", mcp_router)
        .layer(tower_http::limit::RequestBodyLimitLayer::new(
            REQUEST_BODY_MAX_BYTES,
        ));

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

    // codex P1 round 6 on PR #57: `into_make_service_with_connect_info::<SocketAddr>()`
    // populates the `ConnectInfo<SocketAddr>` request extension so the
    // admin Host check can verify peer.is_loopback() (= remote attackers
    // cannot bypass via spoofed Host: 127.0.0.1).
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(async {
        let _ = tokio::signal::ctrl_c().await;
        eprintln!("kb-mcp: shutdown signal received");
    })
    .await
    .context("axum::serve failed")?;
    Ok(())
}

/// 生きている MCP session を数える手段。
///
/// production では rmcp の `LocalSessionManager` を直接見る (`sessions` は
/// `pub` な `RwLock<HashMap<..>>` なので、独自 `SessionManager` を実装しなくても
/// 現在数が読める)。テストからは固定値を差し込む。
#[derive(Clone)]
enum LiveSessionCount {
    Rmcp(Arc<LocalSessionManager>),
    #[cfg(test)]
    Fixed(Arc<std::sync::atomic::AtomicUsize>),
}

impl LiveSessionCount {
    async fn get(&self) -> usize {
        match self {
            Self::Rmcp(manager) => manager.sessions.read().await.len(),
            #[cfg(test)]
            Self::Fixed(n) => {
                // **読んだ後に**譲る。狙いは「生きている数を読み終えた直後に
                // 世界が動く」最悪のスケジューリングを毎回起こすこと。
                // production は `RwLock::read().await` の 1 回だけが yield 点で、
                // その先の in_flight 読みとの間に譲る保証は無い — つまり実機で
                // この隙間が開くのは別コアと競った時だけで、極めて狭い。
                // 即値を返す double だと隙間が **一度も** 開かず、
                // 「予約と解放の直列化」を外しても全テストが緑のままだった。
                let value = n.load(std::sync::atomic::Ordering::SeqCst);
                tokio::task::yield_now().await;
                value
            }
        }
    }
}

/// 上限に達している間の拒否ログを間引く (BU-32)。
///
/// 満杯の間は**リクエストごとに**拒否が起きる。実測では 1 秒で 1744 件断り、
/// そのまま書くと 1744 行出た。daemon は stderr をログファイルに落とすので、
/// **ログ自体が第 2 の資源枯渇**になる。最初の 1 件は必ず出し、以後は
/// [`REFUSAL_LOG_EVERY_SECS`] に 1 行へ落として「その間に何件断ったか」を添える。
struct RefusalLog {
    started: std::time::Instant,
    /// 最後に出力した時刻 (started からの秒)。`u64::MAX` = まだ 1 度も出していない。
    last_logged_secs: std::sync::atomic::AtomicU64,
    /// 出力を見送った件数。
    suppressed: std::sync::atomic::AtomicU64,
}

const REFUSAL_LOG_EVERY_SECS: u64 = 60;

impl RefusalLog {
    fn new() -> Self {
        Self {
            started: std::time::Instant::now(),
            last_logged_secs: std::sync::atomic::AtomicU64::new(u64::MAX),
            suppressed: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// 拒否を 1 件記録し、**今ログを出すべきなら**「前回出力してから見送った
    /// 件数」を返す。ログ出力そのものは呼び出し側が行う (= ここは純粋に
    /// 判定なので、時刻を注入すればテストできる)。
    fn record(&self, now_secs: u64) -> Option<u64> {
        let last = self
            .last_logged_secs
            .load(std::sync::atomic::Ordering::Relaxed);
        let due = last == u64::MAX || now_secs.saturating_sub(last) >= REFUSAL_LOG_EVERY_SECS;
        if due
            && self
                .last_logged_secs
                .compare_exchange(
                    last,
                    now_secs,
                    std::sync::atomic::Ordering::Relaxed,
                    std::sync::atomic::Ordering::Relaxed,
                )
                .is_ok()
        {
            // 勝った 1 本だけが出力する。負けた側は下で件数に積む。
            return Some(
                self.suppressed
                    .swap(0, std::sync::atomic::Ordering::Relaxed),
            );
        }
        self.suppressed
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        None
    }

    fn elapsed_secs(&self) -> u64 {
        self.started.elapsed().as_secs()
    }
}

/// 上限内に席を 1 つ予約する (BU-32、codex review round 1-2 の P1)。
///
/// # 数える対象が 2 つに割れている
///
/// 通した要求は、**rmcp が map に入れ終えるまで** `live` に現れない。だから
/// 上限は「生きている数」だけでは測れず、「通したがまだ確定していない数」
/// (= `in_flight`) を足す必要がある。`live` だけを読んで通していた初版は、
/// 同時に来た要求が全部同じ「まだ空いている」を見て通ってしまい、
/// **`max_sessions = 1` でも 16 本同時なら 16 本とも通った** (round 1 の P1)。
///
/// # 2 つの数を足すだけでも足りない
///
/// `live` と `in_flight` を別々に読むと、その 2 つの読みの**間**に
/// 「A が insert して席を返す」引き渡しが挟まり得る。B は A を含まない `live`
/// と、A が抜けた後の `in_flight` を見て、どちらにも A が数えられていない状態で
/// 通ってしまう (round 2 の P1)。
///
/// そこで **予約と解放を同じ [`tokio::sync::Mutex`] で直列化**する。解放側も
/// lock を取るので、B が 2 つの数を読んでいる最中に A が抜けることはない。
/// A の insert は A の解放より**前**に起きる (rmcp は `initialize_session` まで
/// 済ませてから応答を返し、席はその後で返る) ので、B が見るのは必ず
/// 「`in_flight` に A がいる」か「`live` に A がいる」のどちらかになる。
///
/// # 打ち切られた要求
///
/// 応答を待たずに接続が切れた場合、明示的な解放には到達しない。その時は
/// [`AdmissionSeat`] の `Drop` が lock 無しで席を返す。この経路だけは上の
/// 直列化から外れるが、上限を超える方向には効かない — 打ち切られた要求は
/// 「session を作れずに終わった」(`live` は増えない) か
/// 「作った後に切れた」(`live` が数える) かのどちらかで、
/// どちらも二重に数えないだけだから。
#[derive(Default)]
struct Admissions {
    /// 臨界区間の token。中身は持たない (数は `in_flight` 側にある) —
    /// `Drop` から lock 無しで減らせる必要があるため。
    gate: tokio::sync::Mutex<()>,
    in_flight: std::sync::atomic::AtomicUsize,
}

impl Admissions {
    async fn try_reserve(
        self: &Arc<Self>,
        live: &LiveSessionCount,
        max_sessions: u32,
    ) -> Option<AdmissionSeat> {
        use std::sync::atomic::Ordering;
        let _guard = self.gate.lock().await;
        let now_live = live.get().await;
        let in_flight = self.in_flight.load(Ordering::Acquire);
        if now_live + in_flight >= max_sessions as usize {
            return None;
        }
        self.in_flight.fetch_add(1, Ordering::AcqRel);
        Some(AdmissionSeat {
            admissions: Arc::clone(self),
            released: false,
        })
    }
}

/// 予約した 1 席。通常は [`AdmissionSeat::release`] で返し、そこに到達
/// できなかった場合 (= 要求が打ち切られた) だけ `Drop` が肩代わりする。
struct AdmissionSeat {
    admissions: Arc<Admissions>,
    released: bool,
}

impl AdmissionSeat {
    /// 席を返す。**予約と同じ lock を取る**ことが要点で、これが無いと
    /// 「A が抜ける」瞬間を B が 2 つの数の間で観測できてしまう。
    async fn release(mut self) {
        let _guard = self.admissions.gate.lock().await;
        self.admissions
            .in_flight
            .fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
        self.released = true;
    }
}

impl Drop for AdmissionSeat {
    fn drop(&mut self) {
        if !self.released {
            // 打ち切られた要求の後始末。await できないので lock は取らない。
            self.admissions
                .in_flight
                .fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
        }
    }
}

/// `/mcp` の前段に入る門番の state (BU-32)。
#[derive(Clone)]
struct McpSessionGate {
    live: LiveSessionCount,
    /// `0` = 無制限。
    max_sessions: u32,
    admissions: Arc<Admissions>,
    refusals: Arc<RefusalLog>,
}

/// `/mcp` の前段 middleware。**session を作りに来たリクエストだけ**を見て、
/// (1) initialize でないものを rmcp に渡さず、(2) 上限に達していれば 429 を返す。
///
/// # なぜ rmcp より手前で initialize を弾くのか
///
/// rmcp 1.4.0 の `handle_post` は **`create_session()` を「initialize 要求か」の
/// 検証より先に**呼ぶ。session は既に map へ入っており、続く
/// `return Err(unexpected_message_response("initialize request"))` (422) は
/// `close_session` を呼ばない — 後始末を持つ `tokio::spawn` はその後ろにある。
/// しかも取り残された worker は initialize 前の `recv()` で park し、そこには
/// keep-alive も cancellation token の arm も無いので**永久に残る**
/// (rmcp-1.4.0 `streamable_http_server/tower.rs:632-646` と
/// `session/local.rs:922-928`)。
///
/// 実測 (2026-08-14、release binary、**1 本の** keep-alive 接続): セッション無しの
/// 非 initialize POST を 2000 回で private bytes が 157 → 274 MiB、**1 件あたり
/// 約 58 KB が解放されない**。所要 1 秒 = 約 117 MiB/秒。認証もセッションも
/// initialize も要らない。
///
/// **上限だけを足すと、これは「無制限のメモリ増加」から「永久に新規 session を
/// 拒否する DoS」に変わる** — 取り残されたエントリは期限切れにならないので、
/// 上限を埋められた時点で正規クライアントは二度と繋がらない。だから順序が
/// 逆にできない: **先に漏れを止め、その上で数える**。
///
/// # 何を複製し、何を複製しないか
///
/// 判定は「セッション無しの POST の body が、単一の JSON-RPC initialize 要求か」
/// だけ。これは **MCP 仕様**であって rmcp の実装詳細ではない。
/// Host / Accept / Content-Type の検証は**複製しない** (F-64 で host 検証を
/// mirror せず委譲に倒した判断と同じ)。その結果、Accept や Content-Type も
/// 同時に誤っている非 initialize 要求は、rmcp の 406 / 415 ではなく本 middleware の
/// 422 を受け取る。どちらも 4xx で、状態は動かず、情報も増えない。
/// **Host が許可されないリクエストはそもそも rmcp が session を作らない**ので、
/// 素通しにしても漏れない。
///
/// body が JSON として壊れている場合も素通しする。rmcp は
/// `expect_json` で session 分岐**より前**に落とすので漏れない (実測で確認済み)。
async fn mcp_session_gate(
    State(gate): State<McpSessionGate>,
    req: Request,
    next: Next,
) -> Response {
    // session を作りに来ないリクエストは一切触らない。
    // 新しい session ができるのは「POST かつ Mcp-Session-Id ヘッダ無し」の時だけ
    // (rmcp tower.rs:562-568 で header 有りは既存 session 分岐へ行く)。
    if req.method() != axum::http::Method::POST || req.headers().contains_key("mcp-session-id") {
        return next.run(req).await;
    }

    let (parts, body) = req.into_parts();
    // 外側の RequestBodyLimitLayer が既に長さを縛っているので、ここで詰むのは
    // 壊れた stream の時だけ。
    let bytes = match axum::body::to_bytes(body, REQUEST_BODY_MAX_BYTES).await {
        Ok(bytes) => bytes,
        Err(_) => return bad_request_typed("could not read request body"),
    };

    match serde_json::from_slice::<serde_json::Value>(&bytes) {
        // JSON として読める。単一の initialize 要求でなければ、rmcp に渡さず
        // ここで終わらせる (= session が作られないので漏れない)。
        Ok(value) => {
            if value.get("method").and_then(|m| m.as_str()) != Some("initialize") {
                // 文言は rmcp の `unexpected_message_response("initialize request")`
                // と同一にしてある。クライアントから見て応答が変わらないように。
                return (
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "Unexpected message, expect initialize request",
                )
                    .into_response();
            }
        }
        // JSON にならない body は rmcp が session 分岐の手前で落とす。
        Err(_) => {
            return next
                .run(Request::from_parts(parts, Body::from(bytes)))
                .await;
        }
    }

    // 予約を取ってから通す。席は応答を返した時点 (= rmcp が session を map に
    // 入れ終えた後) で返す。
    let seat = if gate.max_sessions > 0 {
        match gate
            .admissions
            .try_reserve(&gate.live, gate.max_sessions)
            .await
        {
            Some(seat) => Some(seat),
            None => {
                let live = gate.live.get().await;
                if let Some(suppressed) = gate.refusals.record(gate.refusals.elapsed_secs()) {
                    tracing::warn!(
                        live,
                        max_sessions = gate.max_sessions,
                        also_refused_since_the_last_line = suppressed,
                        "refusing a new MCP session: the concurrent-session limit is full \
                         (raise [transport.http].max_sessions, or set it to 0 for no limit)"
                    );
                }
                return (
                    StatusCode::TOO_MANY_REQUESTS,
                    [("retry-after", SESSION_RETRY_AFTER_SECS.to_string())],
                    "Too Many Requests: the concurrent MCP session limit is full",
                )
                    .into_response();
            }
        }
    } else {
        None
    };

    let response = next
        .run(Request::from_parts(parts, Body::from(bytes)))
        .await;
    if let Some(seat) = seat {
        seat.release().await;
    }
    response
}

/// F-64: `/healthz` 用 axum middleware。Host header を allowed_hosts と照合し
/// 不一致なら 400 / 403 を返す。実際の比較は pure helper `validate_host_header`
/// に委譲、本 fn は HTTP-specific layer (= header / authority / response builder) のみ。
///
/// rmcp 1.4 `tower.rs::validate_dns_rebinding_headers` と semantic parity:
/// - missing Host → 400 "Bad Request: missing Host header"
/// - non-UTF8 Host → 400 "Bad Request: Invalid Host header encoding"
/// - parse 失敗 → 400 "Bad Request: Invalid Host header"
/// - allow-list 不一致 → 403 "Forbidden: Host header is not allowed"
///
/// kb-mcp 拡張: HTTP/2 `:authority` fallback (= Q4=C2 で意図的に維持、
/// rmcp の superset)。Host header 不在時に URI authority を fallback として読む。
async fn healthz_host_check(
    State(allowed): State<Arc<Option<Vec<String>>>>,
    headers: HeaderMap,
    req: Request,
    next: Next,
) -> Response {
    // non-UTF8 Host header value は helper を経由せず middleware で直接 catch
    // (= rmcp tower.rs:227-229 と同じ責務分担、helper には str しか渡さない)
    let host_str: Option<Result<&str, _>> = headers.get("host").map(|h| h.to_str());
    if let Some(Err(_)) = host_str {
        return bad_request_typed("Invalid Host header encoding");
    }
    let host_from_header: Option<&str> = host_str.and_then(|r| r.ok());

    // Host 不在時の URI authority fallback (= HTTP/2 / proxy-forwarded 互換)
    let authority_owned: Option<String> = req.uri().authority().map(|a| a.to_string());
    let host_raw: Option<&str> = host_from_header.or(authority_owned.as_deref());

    // Arc<Option<Vec<String>>> → Option<&[String]> 変換
    // (= `Option<Vec<String>>::as_deref()` は `Option<&[String]>` を返す。Vec の Deref<Target=[T]> による)
    let allowed_slice: Option<&[String]> = allowed.as_ref().as_deref();

    match validate_host_header(host_raw, allowed_slice) {
        Ok(()) => next.run(req).await,
        // 呼び出し側は prefix を含めない文字列を渡す (二重付与防止)
        Err(HostRejection::MissingHost) => bad_request_typed("missing Host header"),
        Err(HostRejection::MalformedHost) => bad_request_typed("Invalid Host header"),
        Err(HostRejection::NotAllowed) => forbidden_plain("Host header is not allowed"),
    }
}

/// `addr` が非 loopback (0.0.0.0、unspecified、または LAN IP 等) で、かつ
/// `allowed_hosts` が「意味のある allow-list」になっていない場合に true。
///
/// 警告対象は 2 種類あり、どちらも operator の意図とずれている:
///
/// - `None` (= loopback only の default allow-list) — 外部クライアントは
///   Host header validation で必ず 403 になる。公開したいのに届かない。
/// - `Some([])` — rmcp の `host_is_allowed` は**空リストを「全 Host 許可」
///   として扱う** (rmcp 1.4.0 `streamable_http_server/tower.rs`: 空なら
///   即 `true`)。つまり Host 検証が丸ごと無効で、非 loopback bind と
///   組み合わせると LAN 全体に無認証で開く。
///
/// (BU-01) 後者はもともと「operator が明示的に無効化した自己責任」として
/// 警告対象外だった (F-33)。だが **いちばん危険な構成がいちばん静か**に
/// なるので反転させた。そもそも Host header は攻撃者が自由に付けられるので、
/// allow-list は DNS rebinding 対策であって認証ではない。
/// Is this peer address the local machine?
///
/// (BU-21) `IpAddr::is_loopback` answers "no" for `::ffff:127.0.0.1`, because
/// `Ipv6Addr::is_loopback` recognises only `::1`. That form is exactly what a
/// dual-stack listener (`bind = "[::]:3100"`) reports for a client connecting
/// over IPv4 — including the tray, which polls `/api/admin/status`. The admin
/// router would answer 403 to a process on the same machine.
///
/// So unwrap the IPv4-mapped form before asking. This does not widen what
/// counts as local: `to_ipv4_mapped` returns `Some` only for the
/// `::ffff:0:0/96` block, whose low 32 bits are a real IPv4 address, and the
/// answer for it is whatever `Ipv4Addr::is_loopback` says (`127.0.0.0/8`).
/// Deprecated IPv4-compatible addresses (`::a.b.c.d`) are deliberately not
/// unwrapped.
fn is_loopback_peer(ip: std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v4) => v4.is_loopback(),
        std::net::IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => v4.is_loopback(),
            None => v6.is_loopback(),
        },
    }
}

fn should_warn_non_loopback_bind(addr: &SocketAddr, allowed_hosts: Option<&[String]>) -> bool {
    if addr.ip().is_loopback() {
        return false;
    }
    match allowed_hosts {
        None => true,
        Some(hosts) => hosts.is_empty(),
    }
}

/// 警告の前半 = その構成で実際に起きること。後半 (「Host 検証は認証の
/// 代わりにならない」) は両方に共通なので呼び出し側が付ける。
fn non_loopback_bind_symptom(allowed_hosts: Option<&[String]>) -> &'static str {
    match allowed_hosts {
        Some([]) => {
            "non-loopback bind with `allowed_hosts = []`: Host validation is disabled \
             entirely, so every peer that can reach this port can call /mcp (search, \
             get_document, rebuild_index)."
        }
        _ => {
            "non-loopback bind with the default allowed_hosts (loopback-only): inbound \
             requests carrying a non-loopback Host header are rejected. Set \
             [transport.http].allowed_hosts explicitly in kb-mcp.toml (e.g. \
             allowed_hosts = [\"kb.example.lan\", \"192.168.1.10\"])."
        }
    }
}

/// Health check endpoint. Always returns 200 with body "ok".
async fn healthz() -> &'static str {
    "ok"
}

// ---------------------------------------------------------------------------
// (feature-43 PR-2) Admin sub-router: `/api/admin/status` + Host check.
// ---------------------------------------------------------------------------

/// `/api/admin/status` endpoint — returns daemon / indexing / watcher / kb
/// state. Gated by `admin_host_check` middleware (loopback only by default,
/// callers add their bind addr to `KbServerShared.allowed_admin_hosts`).
async fn api_admin_status(
    State(shared): State<Arc<KbServerShared>>,
) -> Result<axum::Json<AdminStatus>, (StatusCode, String)> {
    // codex P2 round 2 on PR #57: read the cheap mutexes first (indexing_state,
    // watcher_active, started_*) so the response can be assembled even when
    // `rebuild_index` is holding the db / embedder locks. `kb_info()` itself
    // uses `try_lock` and yields `None` counts on contention.
    let indexing_info = {
        let guard = shared.indexing_state.lock().map_err(|_| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "indexing_state mutex poisoned".to_string(),
            )
        })?;
        match guard.as_ref() {
            Some(s) => IndexingInfo {
                active: true,
                started_at: Some(format_iso(s.started_at)),
                progress: s.progress.as_ref().map(|p| IndexingProgressView {
                    current: p.current,
                    total: p.total,
                }),
            },
            None => IndexingInfo {
                active: false,
                started_at: None,
                progress: None,
            },
        }
    };
    let kb = shared.kb_info().map_err(|e| {
        tracing::warn!("admin_status kb_info failure: {e:?}");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "kb info unavailable".to_string(),
        )
    })?;
    Ok(axum::Json(AdminStatus {
        daemon: DaemonInfo {
            version: env!("CARGO_PKG_VERSION").into(),
            pid: std::process::id(),
            uptime_secs: shared.started_instant.elapsed().as_secs(),
            started_at: format_iso(shared.started_at),
        },
        indexing: indexing_info,
        watcher: WatcherInfo {
            active: shared
                .watcher_active
                .load(std::sync::atomic::Ordering::Relaxed),
            debounce_ms: shared.watcher_debounce_ms,
        },
        kb,
        config_source: shared.config_source_label.clone(),
    }))
}

/// (feature-43 PR-2) `/ui` — serves the WebUI MVP placeholder HTML (XSS-safe
/// via `textContent` + `createElement`, no CSS framework). Phase 3+ で本格
/// redesign 前提の disposable placeholder。
async fn ui_index() -> axum::response::Html<&'static str> {
    axum::response::Html(include_str!("webui_index.html"))
}

#[derive(serde::Deserialize)]
struct WebSearchRequest {
    query: String,
    #[serde(default)]
    limit: Option<u32>,
}

/// (feature-43 PR-2) `/api/search` POST — JSON-in / JSON-out wrapper around
/// `KbServer::search` for the WebUI. Gated by the same admin Host check
/// middleware as `/api/admin/status`.
///
/// `web_search` returns an already pretty-printed JSON string
/// (`SearchResponse` or `ErrorResponse`); pass it through verbatim with an
/// explicit `Content-Type: application/json` so we do not re-serialize.
async fn api_search(
    State(shared): State<Arc<KbServerShared>>,
    Json(req): Json<WebSearchRequest>,
) -> Result<Response, (StatusCode, String)> {
    let body = crate::server::web_search(&shared, req.query, req.limit).await;
    Response::builder()
        .status(StatusCode::OK)
        .header(http::header::CONTENT_TYPE, "application/json")
        .body(Body::from(body))
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
}

/// (feature-43 PR-2) `admin_host_check` middleware — exact-match Host header
/// against `shared.allowed_admin_hosts` (= loopback aliases + bind addr).
/// Substring match is rejected since `10.0.127.0.1.evil.com` would otherwise
/// match `127.0.0.1`. Port suffix is stripped before comparison so
/// `127.0.0.1:3100` matches the bare `127.0.0.1` entry.
async fn admin_host_check(
    State(shared): State<Arc<KbServerShared>>,
    req: axum::http::Request<axum::body::Body>,
    next: axum::middleware::Next,
) -> Result<axum::response::Response, (StatusCode, String)> {
    // codex P1 round 6 on PR #57: enforce loopback by **peer address** for
    // admin routes. Host header alone is client-controlled — a remote
    // attacker on the same LAN as a `--bind 0.0.0.0` daemon can send
    // `Host: 127.0.0.1` and bypass the allow-list. Production code path
    // (`run_http` -> `into_make_service_with_connect_info::<SocketAddr>()`)
    // populates the `ConnectInfo<SocketAddr>` extension; tests via
    // `oneshot` may leave it unset, in which case we fall through to the
    // Host-only check (= test convenience, the production listener always
    // wraps with connect_info so production is fail-closed).
    if let Some(axum::extract::ConnectInfo(peer)) = req
        .extensions()
        .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
        && !is_loopback_peer(peer.ip())
    {
        return Err((
            StatusCode::FORBIDDEN,
            format!(
                "admin endpoints are loopback-only; peer {} is not loopback",
                peer
            ),
        ));
    }

    // codex P2 round 5+6 on PR #57: reuse `validate_host_header` so admin
    // Host validation shares /healthz's hardened defenses (= userinfo /
    // trailing garbage / port out-of-range rejected, NormalizedAuthority
    // normalization for IPv6 and case).
    let host_header = req
        .headers()
        .get(axum::http::header::HOST)
        .and_then(|h| h.to_str().ok());
    let host_for_err = host_header.unwrap_or("").to_string();
    match validate_host_header(host_header, Some(shared.allowed_admin_hosts.as_slice())) {
        Ok(()) => {}
        Err(HostRejection::MissingHost) => {
            return Err((StatusCode::BAD_REQUEST, "missing Host header".to_string()));
        }
        Err(HostRejection::MalformedHost) => {
            return Err((
                StatusCode::BAD_REQUEST,
                format!("malformed Host header '{host_for_err}'"),
            ));
        }
        Err(HostRejection::NotAllowed) => {
            return Err((
                StatusCode::FORBIDDEN,
                format!("Host '{host_for_err}' not in admin allow-list"),
            ));
        }
    }
    Ok(next.run(req).await)
}

/// (feature-43 PR-2) Build the axum app router with admin endpoints only.
/// Used by integration tests in `tests/webui_integration.rs` — the production
/// app composes the admin sub-router with `/healthz` + `/mcp` in `run_http`.
///
/// Gated by the `test-helpers` feature so production binaries do not carry
/// the helper. `#[cfg(test)]` alone would not make this visible to the
/// integration test crate (a separate compilation unit).
#[cfg(any(test, feature = "test-helpers"))]
pub fn build_router_for_test(shared: Arc<KbServerShared>) -> axum::Router {
    let admin_router = axum::Router::new()
        .route("/api/admin/status", get(api_admin_status))
        .route("/api/search", post(api_search))
        .route("/ui", get(ui_index))
        .with_state(Arc::clone(&shared))
        .layer(middleware::from_fn_with_state(
            Arc::clone(&shared),
            admin_host_check,
        ));
    axum::Router::new().merge(admin_router)
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// (BU-21) A dual-stack listener reports an IPv4 client as
    /// `::ffff:a.b.c.d`, and `Ipv6Addr::is_loopback` only knows `::1`.
    ///
    /// So with `bind = "[::]:3100"` the admin router answered 403 to the tray
    /// running on the same machine — fail-closed, but the wrong answer. The
    /// mapped form has to be unwrapped before the question is asked, and only
    /// that form: a mapped address outside `127.0.0.0/8` stays non-loopback.
    #[test]
    fn ipv4_mapped_loopback_counts_as_loopback() {
        use std::net::IpAddr;
        let cases: &[(&str, bool)] = &[
            ("127.0.0.1", true),
            ("127.1.2.3", true),
            ("::1", true),
            // What a dual-stack socket reports for an IPv4 loopback client.
            ("::ffff:127.0.0.1", true),
            ("192.168.1.10", false),
            ("::ffff:192.168.1.10", false),
            ("2001:db8::1", false),
        ];
        for (raw, expected) in cases {
            let ip: IpAddr = raw.parse().expect("test address must parse");
            assert_eq!(
                is_loopback_peer(ip),
                *expected,
                "is_loopback_peer({raw}) should be {expected}"
            );
        }
    }

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

    /// BU-01 (**F-33 の判断を差し替え**): 0.0.0.0 + 空 allowed_hosts → warn する。
    ///
    /// F-33 では「operator が `allowed_hosts = []` で明示的に Host 検証を
    /// 無効化した = 自己責任だから黙る」としていた。しかし rmcp の
    /// `host_is_allowed` は空リストを**全 Host 許可**として扱うため、これは
    /// 「非 loopback bind + 検証なし + 認証なし」= 最も開いた構成であり、
    /// **唯一警告が出ない構成でもあった**。危険なほど静かになる並びを反転する。
    #[test]
    fn test_warn_on_unspecified_bind_with_empty_allowed_hosts() {
        let addr: SocketAddr = "0.0.0.0:3100".parse().unwrap();
        let hosts: [String; 0] = [];
        assert!(should_warn_non_loopback_bind(&addr, Some(&hosts)));
    }

    /// 空 allow-list でも loopback bind なら警告しない (= 反転させたのは
    /// 「非 loopback」との組合せだけで、loopback 判定は据え置き)。
    #[test]
    fn test_no_warn_on_loopback_bind_with_empty_allowed_hosts() {
        let addr: SocketAddr = "127.0.0.1:3100".parse().unwrap();
        let hosts: [String; 0] = [];
        assert!(!should_warn_non_loopback_bind(&addr, Some(&hosts)));
    }

    /// 2 つの警告理由 (検証無効 / 届かない) は別の文面で出す。
    #[test]
    fn test_symptom_text_distinguishes_disabled_validation_from_unreachable() {
        let empty: [String; 0] = [];
        let disabled = non_loopback_bind_symptom(Some(&empty));
        let unreachable = non_loopback_bind_symptom(None);
        assert!(
            disabled.contains("allowed_hosts = []"),
            "empty-list case must name the setting: {disabled}"
        );
        assert!(
            unreachable.contains("rejected"),
            "default case must explain the 403: {unreachable}"
        );
        assert_ne!(disabled, unreachable);
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
        // allow-list 側も NormalizedAuthority::from_allowed_entry で normalize されるので、
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

    /// feature-39 / D-11: middleware 経路で `HeaderValue::to_str()` 失敗 path を直叩き
    /// (= encoding error path の regression catcher)。
    ///
    /// `HeaderValue::from_bytes(&[0xFF, 0xFE])` は valid HeaderValue (byte 32-255 範囲、
    /// `http-1.4.0/src/header/value.rs:129`) だが `to_str()` は byte > 127 で `Err` を返す
    /// = middleware が helper を経由せず `bad_request_typed("Invalid Host header encoding")`
    /// を直接返す path を踏ませる。
    #[tokio::test]
    async fn test_healthz_public_false_rejects_non_utf8_host_header_at_middleware() {
        let app = build_test_router(false, None);
        let raw_bytes = [0xFF_u8, 0xFE_u8];
        let invalid_value = http::HeaderValue::from_bytes(&raw_bytes).unwrap();

        let req = HttpRequest::builder()
            .uri("/healthz")
            .body(Body::empty())
            .unwrap();
        let mut req = req;
        req.headers_mut().insert("host", invalid_value);

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

        let body = to_bytes(resp.into_body(), 1024).await.unwrap();
        assert_eq!(
            body.as_ref(),
            b"Bad Request: Invalid Host header encoding".as_slice(),
            "body should be byte-identical to rmcp"
        );
    }

    // -----------------------------------------------------------------------
    // AU-17 (codex P1 on PR #104): request body の上限。
    //
    // `search` の filter 上限は、rmcp が body を buffer して `SearchParams` に
    // deserialize した後にしか効かない。100 万件の tags を載せた body は
    // はじかれる時点で既にメモリと parse 時間を使っている。body 自体に
    // 上限をかけないと availability の問題は残る。
    // -----------------------------------------------------------------------

    use std::sync::atomic::{AtomicBool, Ordering};

    /// `/mcp` は `nest_service` で rmcp の service を載せている。上限が
    /// **nest した service にも効く**ことが要点なので、mount の形だけ同じに
    /// して body を読む dummy service を挟む (rmcp 本体は組み立てない)。
    fn router_with_body_limit(reached: Arc<AtomicBool>) -> Router {
        let inner = axum::routing::any(move |body: axum::body::Bytes| {
            let reached = Arc::clone(&reached);
            async move {
                reached.store(true, Ordering::SeqCst);
                format!("read {} bytes", body.len())
            }
        });
        Router::new().nest_service("/mcp", inner).layer(
            tower_http::limit::RequestBodyLimitLayer::new(REQUEST_BODY_MAX_BYTES),
        )
    }

    /// Content-Length が上限を超えていれば、body を 1 バイトも読まずに 413。
    #[tokio::test]
    async fn an_oversized_declared_body_is_rejected_before_the_service_reads_it() {
        let reached = Arc::new(AtomicBool::new(false));
        let app = router_with_body_limit(Arc::clone(&reached));
        let oversized = REQUEST_BODY_MAX_BYTES + 1;
        let req = HttpRequest::builder()
            .method("POST")
            .uri("/mcp")
            .header("content-length", oversized.to_string())
            .body(Body::from(vec![b'x'; oversized]))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::PAYLOAD_TOO_LARGE);
        assert!(
            !reached.load(Ordering::SeqCst),
            "the nested service must not see an oversized body"
        );
    }

    /// Content-Length を名乗らない (= chunked 相当) 場合も、読み進めた時点で
    /// 打ち切られる。ここで大事なのは status そのものより
    /// 「service まで到達しない」こと。
    #[tokio::test]
    async fn an_oversized_undeclared_body_is_cut_off_mid_read() {
        let reached = Arc::new(AtomicBool::new(false));
        let app = router_with_body_limit(Arc::clone(&reached));
        let req = HttpRequest::builder()
            .method("POST")
            .uri("/mcp")
            .body(Body::from(vec![b'x'; REQUEST_BODY_MAX_BYTES + 1]))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_ne!(resp.status(), StatusCode::OK);
        assert!(
            !reached.load(Ordering::SeqCst),
            "the nested service must not receive the full body"
        );
    }

    /// 上限内の body は素通りする (上限が正常系を壊していないこと)。
    #[tokio::test]
    async fn a_request_body_under_the_limit_still_goes_through() {
        let reached = Arc::new(AtomicBool::new(false));
        let app = router_with_body_limit(Arc::clone(&reached));
        let size = REQUEST_BODY_MAX_BYTES / 2;
        let req = HttpRequest::builder()
            .method("POST")
            .uri("/mcp")
            .header("content-length", size.to_string())
            .body(Body::from(vec![b'x'; size]))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(reached.load(Ordering::SeqCst));
    }

    /// 上限は `search` の filter 上限より十分大きいこと。片方だけ動かすと
    /// 「上限内の正当なリクエストが transport で落ちる」状態になりうる。
    #[test]
    fn the_body_limit_leaves_room_for_a_fully_loaded_search_request() {
        let filters = crate::server::FILTER_LIST_MAX_ITEMS * crate::server::FILTER_ITEM_MAX_BYTES;
        let worst_case = filters * 3 + crate::server::SEARCH_QUERY_MAX_BYTES;
        assert!(
            worst_case * 2 < REQUEST_BODY_MAX_BYTES,
            "body limit {REQUEST_BODY_MAX_BYTES} leaves too little room for a \
             maximal search request ({worst_case} bytes of filters and query)"
        );
    }

    // -----------------------------------------------------------------------
    // BU-32: the `/mcp` session gate.
    //
    // `build_router_for_test` は admin sub-router しか組まないので、この門番は
    // そこからは踏めない。body limit のテストと同じやり方で、**mount の形だけ**
    // 同じにして rmcp の代わりに dummy service を挟む。門番が守るべき性質は
    // 2 つで、どちらも「rmcp まで届いたかどうか」で観測する:
    //   1. session を作らないリクエストには触れないこと
    //   2. session を作りに来たリクエストのうち、initialize でないものと
    //      上限超えのものは **rmcp に届かない**こと
    // -----------------------------------------------------------------------

    use std::sync::atomic::AtomicUsize;

    const INITIALIZE_BODY: &str = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#;
    const OTHER_BODY: &str = r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#;

    /// `/mcp` の mount 形状 + 門番。`live` は「今生きている session 数」の
    /// 代わり、`reached` は dummy service に到達したかどうか。
    fn gated_mcp_router(live: usize, max_sessions: u32, reached: Arc<AtomicBool>) -> Router {
        let inner = axum::routing::any(move |_body: axum::body::Bytes| {
            let reached = Arc::clone(&reached);
            async move {
                reached.store(true, Ordering::SeqCst);
                "forwarded"
            }
        });
        let gate = McpSessionGate {
            live: LiveSessionCount::Fixed(Arc::new(AtomicUsize::new(live))),
            max_sessions,
            admissions: Arc::new(Admissions::default()),
            refusals: Arc::new(RefusalLog::new()),
        };
        let mcp: Router = Router::new()
            .fallback_service(inner)
            .layer(middleware::from_fn_with_state(gate, mcp_session_gate));
        Router::new().nest_service("/mcp", mcp)
    }

    fn mcp_post(body: &str) -> HttpRequest<Body> {
        HttpRequest::builder()
            .method("POST")
            .uri("/mcp")
            .header("content-type", "application/json")
            .header("accept", "application/json, text/event-stream")
            .body(Body::from(body.to_string()))
            .unwrap()
    }

    /// 漏れを止めている本体。rmcp 1.4 は `create_session()` を「initialize か」の
    /// 検証より先に呼び、422 で返る経路が `close_session` を呼ばないので、
    /// **rmcp に届かせないこと**が唯一の防ぎ方になる。
    #[tokio::test]
    async fn a_sessionless_post_that_is_not_initialize_never_reaches_the_service() {
        let reached = Arc::new(AtomicBool::new(false));
        let app = gated_mcp_router(0, 256, Arc::clone(&reached));

        let resp = app.oneshot(mcp_post(OTHER_BODY)).await.unwrap();

        assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
        assert!(
            !reached.load(Ordering::SeqCst),
            "a non-initialize sessionless POST must not reach rmcp: reaching it \
             creates a session that is never closed"
        );
        let body = to_bytes(resp.into_body(), 1024).await.unwrap();
        assert_eq!(
            String::from_utf8_lossy(&body),
            "Unexpected message, expect initialize request",
            "the wording matches rmcp's own 422 so clients see no change"
        );
    }

    /// 正常系: 空きがあれば initialize はそのまま通る。
    #[tokio::test]
    async fn an_initialize_post_goes_through_when_there_is_room() {
        let reached = Arc::new(AtomicBool::new(false));
        let app = gated_mcp_router(0, 256, Arc::clone(&reached));

        let resp = app.oneshot(mcp_post(INITIALIZE_BODY)).await.unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        assert!(reached.load(Ordering::SeqCst));
    }

    /// 既存 session への後続リクエストは body を見ない。ここを取り違えると
    /// 「initialize 以外は全部 422」になり、session が張れても何も呼べなくなる。
    #[tokio::test]
    async fn a_post_carrying_a_session_id_is_not_inspected() {
        let reached = Arc::new(AtomicBool::new(false));
        let app = gated_mcp_router(0, 256, Arc::clone(&reached));

        let mut req = mcp_post(OTHER_BODY);
        req.headers_mut()
            .insert("mcp-session-id", "abc".parse().unwrap());
        let resp = app.oneshot(req).await.unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        assert!(reached.load(Ordering::SeqCst));
    }

    /// GET (= SSE stream) と DELETE は session を作らないので素通し。
    #[tokio::test]
    async fn a_non_post_request_is_not_inspected() {
        let reached = Arc::new(AtomicBool::new(false));
        let app = gated_mcp_router(0, 256, Arc::clone(&reached));

        let req = HttpRequest::builder()
            .method("GET")
            .uri("/mcp")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        assert!(reached.load(Ordering::SeqCst));
    }

    /// JSON にならない body は rmcp に任せる。rmcp は `expect_json` で
    /// session 分岐**より前**に落とすので、ここで先回りする必要が無い。
    #[tokio::test]
    async fn a_body_that_is_not_json_is_left_to_rmcp() {
        let reached = Arc::new(AtomicBool::new(false));
        let app = gated_mcp_router(0, 256, Arc::clone(&reached));

        let resp = app.oneshot(mcp_post("not json at all")).await.unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        assert!(
            reached.load(Ordering::SeqCst),
            "rmcp answers malformed bodies itself, and does so without creating \
             a session"
        );
    }

    /// 上限に達している間、新規 session は 429 + Retry-After で断る。
    #[tokio::test]
    async fn a_full_session_limit_refuses_a_new_session_with_429() {
        let reached = Arc::new(AtomicBool::new(false));
        let app = gated_mcp_router(4, 4, Arc::clone(&reached));

        let resp = app.oneshot(mcp_post(INITIALIZE_BODY)).await.unwrap();

        assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(
            resp.headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok()),
            Some(SESSION_RETRY_AFTER_SECS.to_string().as_str()),
            "a refusal without Retry-After invites an immediate retry loop"
        );
        assert!(!reached.load(Ordering::SeqCst));
    }

    /// **上限は既存 session を壊さない。** 満杯でも、確立済みの session への
    /// リクエストは通る。ここが逆になっていると、上限は可用性の改善ではなく
    /// 全クライアントの切断になる。
    #[tokio::test]
    async fn a_full_session_limit_still_serves_established_sessions() {
        let reached = Arc::new(AtomicBool::new(false));
        let app = gated_mcp_router(4, 4, Arc::clone(&reached));

        let mut req = mcp_post(OTHER_BODY);
        req.headers_mut()
            .insert("mcp-session-id", "abc".parse().unwrap());
        let resp = app.oneshot(req).await.unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        assert!(reached.load(Ordering::SeqCst));
    }

    /// (codex review round 1 の P1) 席の確保は「数を読む」と「1 つ増やす」を
    /// **1 手**で行うこと。分けると、同時に来た要求がどれも rmcp が map に
    /// 入れる前の同じ数を読み、全部が通ってしまう。
    #[tokio::test]
    async fn a_seat_is_reserved_in_one_step() {
        let live = LiveSessionCount::Fixed(Arc::new(AtomicUsize::new(0)));
        let admissions = Arc::new(Admissions::default());

        let first = admissions.try_reserve(&live, 1).await;
        assert!(first.is_some(), "the first request takes the only seat");

        // 生きている数はまだ 0。rmcp が map に入れるのは、通した要求が
        // 処理を終えた後だから。それでも 2 本目は通ってはいけない。
        assert!(
            admissions.try_reserve(&live, 1).await.is_none(),
            "a concurrent request must not see the seat as still free just \
             because rmcp has not inserted the session yet"
        );

        first.unwrap().release().await;
        assert!(
            admissions.try_reserve(&live, 1).await.is_some(),
            "the seat comes back when the request that held it finishes"
        );
    }

    /// 打ち切られた要求の席も返る。`release().await` に到達できない経路なので
    /// `Drop` が肩代わりする — ここが漏れると、上限がじわじわ縮んでいく。
    #[tokio::test]
    async fn an_abandoned_request_still_returns_its_seat() {
        let live = LiveSessionCount::Fixed(Arc::new(AtomicUsize::new(0)));
        let admissions = Arc::new(Admissions::default());

        let seat = admissions.try_reserve(&live, 1).await;
        assert!(seat.is_some());
        drop(seat); // = 応答を待たずに接続が切れた

        assert!(
            admissions.try_reserve(&live, 1).await.is_some(),
            "a seat whose request was dropped must not stay taken"
        );
    }

    /// (codex review round 2 の P1) 予約と解放が直列化されていること。
    ///
    /// `live` と `in_flight` を別々に読むと、その 2 つの読みの**間**に
    /// 「A が insert して席を返す」引き渡しが挟まり、A がどちらにも数えられて
    /// いない瞬間を B が見る。ここでは dummy service が rmcp の insert を
    /// 演じ (応答を返す前に `live` を 1 増やす)、`LiveSessionCount::Fixed` が
    /// 読んだ直後に必ず譲ることで、その最悪スケジューリングを毎回起こす。
    ///
    /// **検査するのは「上限を超えない」ことだけ**で、「ちょうど上限まで埋まる」
    /// ことではない。in_flight を数える以上、解放待ちの席を空きと見なさない
    /// 分だけ保守的に断ることがあり、それは安全な側への外れ。直列化を外すと
    /// この test は `live` が 5 になって落ちる (上限 4)。
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn the_cap_is_never_exceeded_across_the_handoff_to_live() {
        const CAP: u32 = 4;
        let live_count = Arc::new(AtomicUsize::new(0));
        let admitted = Arc::new(AtomicUsize::new(0));

        let inner = {
            let live_count = Arc::clone(&live_count);
            let admitted = Arc::clone(&admitted);
            axum::routing::any(move |_body: axum::body::Bytes| {
                let live_count = Arc::clone(&live_count);
                let admitted = Arc::clone(&admitted);
                async move {
                    admitted.fetch_add(1, Ordering::SeqCst);
                    // rmcp が session を map に入れるのに相当。応答を返す前に
                    // 起きる = 席が返るより前。
                    tokio::task::yield_now().await;
                    live_count.fetch_add(1, Ordering::SeqCst);
                    "forwarded"
                }
            })
        };
        let gate = McpSessionGate {
            live: LiveSessionCount::Fixed(Arc::clone(&live_count)),
            max_sessions: CAP,
            admissions: Arc::new(Admissions::default()),
            refusals: Arc::new(RefusalLog::new()),
        };
        let mcp: Router = Router::new()
            .fallback_service(inner)
            .layer(middleware::from_fn_with_state(gate, mcp_session_gate));
        let app: Router = Router::new().nest_service("/mcp", mcp);

        let mut handles = Vec::new();
        for _ in 0..32 {
            let app = app.clone();
            handles.push(tokio::spawn(async move {
                app.oneshot(mcp_post(INITIALIZE_BODY))
                    .await
                    .unwrap()
                    .status()
            }));
        }
        let mut ok = 0;
        for h in handles {
            if h.await.unwrap() == StatusCode::OK {
                ok += 1;
            }
        }

        let live = live_count.load(Ordering::SeqCst);
        assert!(
            live <= CAP as usize,
            "the cap must hold while sessions move from in-flight to live: \
             {live} sessions exist with max_sessions = {CAP}"
        );
        assert_eq!(
            ok, live,
            "every admitted request created a session, and no refused one did"
        );
        assert_eq!(admitted.load(Ordering::SeqCst), ok);
        assert!(ok > 0, "the gate must not refuse everything");
    }

    /// 上限は「生きている数 + 通したがまだ確定していない数」に掛かる。
    #[tokio::test]
    async fn live_sessions_and_in_flight_admissions_share_the_cap() {
        let live = LiveSessionCount::Fixed(Arc::new(AtomicUsize::new(3)));
        let admissions = Arc::new(Admissions::default());

        let seat = admissions.try_reserve(&live, 4).await;
        assert!(seat.is_some(), "3 live + 0 in flight is under a cap of 4");
        assert!(
            admissions.try_reserve(&live, 4).await.is_none(),
            "3 live + 1 in flight already fills a cap of 4"
        );
    }

    /// 同じことを HTTP 側から。門番を抜けた要求は dummy service で待たされる
    /// ので、**全員が門番の中にいる状態**が作れる。予約が無ければ 16 本とも
    /// 通り、`entered` が 16 になる。
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_initialize_requests_cannot_overshoot_the_cap() {
        let entered = Arc::new(AtomicUsize::new(0));
        let release = Arc::new(tokio::sync::Notify::new());

        let inner = {
            let entered = Arc::clone(&entered);
            let release = Arc::clone(&release);
            axum::routing::any(move |_body: axum::body::Bytes| {
                let entered = Arc::clone(&entered);
                let release = Arc::clone(&release);
                async move {
                    entered.fetch_add(1, Ordering::SeqCst);
                    release.notified().await;
                    "forwarded"
                }
            })
        };
        let gate = McpSessionGate {
            live: LiveSessionCount::Fixed(Arc::new(AtomicUsize::new(0))),
            max_sessions: 1,
            admissions: Arc::new(Admissions::default()),
            refusals: Arc::new(RefusalLog::new()),
        };
        let mcp: Router = Router::new()
            .fallback_service(inner)
            .layer(middleware::from_fn_with_state(gate, mcp_session_gate));
        let app: Router = Router::new().nest_service("/mcp", mcp);

        let mut handles = Vec::new();
        for _ in 0..16 {
            let app = app.clone();
            handles.push(tokio::spawn(async move {
                app.oneshot(mcp_post(INITIALIZE_BODY))
                    .await
                    .unwrap()
                    .status()
            }));
        }

        // 通った 1 本は dummy の中で止まり、断られた 15 本は即座に返る。
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        assert_eq!(
            entered.load(Ordering::SeqCst),
            1,
            "a cap of 1 must admit exactly one of 16 simultaneous requests"
        );

        release.notify_waiters();
        let mut ok = 0;
        let mut refused = 0;
        for h in handles {
            match h.await.unwrap() {
                StatusCode::OK => ok += 1,
                StatusCode::TOO_MANY_REQUESTS => refused += 1,
                other => panic!("unexpected status {other}"),
            }
        }
        assert_eq!((ok, refused), (1, 15));
    }

    /// 拒否ログの間引き。満杯の間は毎リクエスト拒否が起きるので、そのまま
    /// 書くと**ログが第 2 の資源枯渇**になる (実測: 1 秒で 1744 行)。
    /// 時刻は注入するので、テストは待たない。
    #[test]
    fn refusal_logging_is_thinned_but_never_silent() {
        let log = RefusalLog::new();

        // 最初の 1 件は必ず出す。見送りはまだ 0 件。
        assert_eq!(log.record(0), Some(0), "the first refusal must be visible");

        // 同じ窓の中は出さない。
        for t in 1..REFUSAL_LOG_EVERY_SECS {
            assert_eq!(log.record(t), None, "second line inside the same window");
        }

        // 窓を跨いだら 1 行出し、その間に見送った件数を添える。
        assert_eq!(
            log.record(REFUSAL_LOG_EVERY_SECS),
            Some(REFUSAL_LOG_EVERY_SECS - 1),
            "the next line reports how many refusals it stands for"
        );

        // カウンタは持ち越さない。
        assert_eq!(log.record(REFUSAL_LOG_EVERY_SECS + 1), None);
        assert_eq!(log.record(REFUSAL_LOG_EVERY_SECS * 2), Some(1));
    }

    /// `max_sessions = 0` は無制限。数を数えている経路そのものを止める。
    #[tokio::test]
    async fn a_zero_limit_means_no_limit() {
        let reached = Arc::new(AtomicBool::new(false));
        let app = gated_mcp_router(100_000, 0, Arc::clone(&reached));

        let resp = app.oneshot(mcp_post(INITIALIZE_BODY)).await.unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        assert!(reached.load(Ordering::SeqCst));
    }

    /// 上限**未満**なら通る = 境界の向きが `>=` であること。`>` だと
    /// `max_sessions` 個目の次に 1 つ余分に張れる。
    #[tokio::test]
    async fn the_limit_admits_up_to_but_not_including_the_cap() {
        let reached = Arc::new(AtomicBool::new(false));
        let app = gated_mcp_router(3, 4, Arc::clone(&reached));
        let resp = app.oneshot(mcp_post(INITIALIZE_BODY)).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "3 live with a cap of 4 fits");
        assert!(reached.load(Ordering::SeqCst));

        let reached = Arc::new(AtomicBool::new(false));
        let app = gated_mcp_router(4, 4, Arc::clone(&reached));
        let resp = app.oneshot(mcp_post(INITIALIZE_BODY)).await.unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::TOO_MANY_REQUESTS,
            "the 5th session with a cap of 4 is refused"
        );
        assert!(!reached.load(Ordering::SeqCst));
    }
}
