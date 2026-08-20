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
    response::{IntoResponse, Response},
    routing::get,
};
// (Phase 4 PR-2) Only the test router posts anything now: a running server
// serves `/api/admin/status` and `/ui` with GET, and search moved to `/mcp`.
#[cfg(any(test, feature = "test-helpers"))]
use axum::{Json, routing::post};
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

/// `Origin` allow-list の既定値 — 指定 port の loopback origin 3 種。
///
/// MCP 仕様 2025-06-18 (Streamable HTTP / Security Warning) は
/// *"Servers **MUST** validate the `Origin` header on all incoming connections
/// to prevent DNS rebinding attacks"* と定めているが、rmcp の既定は
/// `allowed_origins: vec![]` = **検証しない**。既定でこれを埋めることで
/// 仕様を満たす。
///
/// **bind IP ではなく port だけを見る。** `--bind 0.0.0.0:3100` で起動しても、
/// 運用者がブラウザで開くのは `http://127.0.0.1:3100/ui` だからで、bind IP を
/// origin に混ぜても誰も送ってこない値が増えるだけになる。
///
/// IPv6 に角括弧を付けるのは RFC 6454 の origin シリアライズに合わせるため
/// (`http://[::1]:3100`)。rmcp は entry に scheme を要求する。
///
/// `https://` を混ぜていないのは、TLS を終端するのは常に前段の proxy であり、
/// その時ブラウザが送る origin は loopback ではなく公開ホスト名になるから。
/// その構成では `[transport.http].allowed_origins` に公開 origin を明示する。
pub fn default_allowed_origins(port: u16) -> Vec<String> {
    // (codex P1 round 6 on PR #173) Built from `DEFAULT_LOOPBACK_HOSTS`, not a
    // second literal with the same three entries. Both are allow-lists deciding
    // which local browser addresses count, so if the set ever changes -- another
    // alias, a different normalized spelling -- a copy would let Host validation
    // and Origin validation quietly disagree about the same browser.
    DEFAULT_LOOPBACK_HOSTS
        .iter()
        .flat_map(|host| origins_for_host(host, port))
        .collect()
}

/// Whether one `[transport.http].allowed_origins` entry survives as far as the
/// comparison, mirroring rmcp's `parse_origin_value`.
///
/// rmcp runs the allow-list through that parser at match time and drops what it
/// cannot read with a `filter_map` (`rmcp-3.1.2`
/// `transport/streamable_http_server/tower.rs:781-806`). Dropping is silent and
/// the list stays non-empty, so validation remains *switched on* with nothing
/// left to compare against — and a non-empty list that matches nothing refuses
/// every request carrying an `Origin`. The operator sees a server answering 403
/// to their own browser with no warning anywhere.
///
/// **The check exists because this key is spelled unlike its neighbour.**
/// `allowed_hosts` ends in a fallback that reads the whole string as a host
/// (`parse_allowed_authority`, `tower.rs:741-752`), so no non-empty entry is
/// ever dropped there. Origin entries must carry a scheme. `"127.0.0.1:3100"`
/// is the trap: `http::Uri` accepts it as authority-form — a scheme has to
/// begin with a letter, so `127.0.0.1` cannot be one — and it fails only at
/// `scheme_str()`, one line further on.
///
/// `"null"` is a real origin (RFC 6454 §6.1; sandboxed frames, `file://`), and
/// rmcp maps it to its own variant, so it is accepted here too.
pub(crate) fn check_origin_entry(entry: &str) -> Result<(), &'static str> {
    match parse_origin(entry) {
        Ok(_) => Ok(()),
        Err(OriginRejection::Empty) => Err("an entry must not be empty"),
        Err(OriginRejection::Unreadable) => Err("an entry must be a serialized origin"),
        Err(OriginRejection::NoScheme) => {
            Err("an entry must carry a scheme (\"http://\" or \"https://\")")
        }
        Err(OriginRejection::NoHost) => Err("an entry must name a host"),
    }
}

/// An origin in the form both sides compare: rmcp's `NormalizedOrigin`
/// (`tower.rs:771-779`), expressed with the [`NormalizedAuthority`] this file
/// already mirrors for `Host`.
///
/// The two halves line up exactly. rmcp's origin path normalizes its host with
/// the same `normalize_host` its Host path uses, and its port rule
/// (`a_port.is_none() || a_port == o_port`, `tower.rs:819`) is the rule
/// [`NormalizedAuthority::matches`] already implements.
#[derive(Debug, Clone, PartialEq, Eq)]
enum NormalizedOrigin {
    /// RFC 6454 §6.1's opaque origin, spelled `null`: sandboxed frames and
    /// `file://` documents send it.
    Null,
    Tuple {
        scheme: String,
        authority: NormalizedAuthority,
    },
}

/// Why an origin string did not survive parsing. rmcp keeps no such distinction
/// — it returns `Option` and drops the entry — but [`check_origin_list`] has to
/// tell the operator *which* rule their entry broke, so the reason is carried
/// here and turned into words there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OriginRejection {
    Empty,
    Unreadable,
    NoScheme,
    NoHost,
}

/// Read an origin the way rmcp reads one, and no more strictly.
///
/// **Three callers ask this same question** — [`check_origin_entry`] (is this
/// config entry usable?), [`origin_matches_any_port`] (does this entry leave the
/// port open?) and [`origin_is_allowed`] (does this request's `Origin` match?)
/// — so it is written once. Three copies of a parser whose whole purpose is to
/// agree with an upstream one is how the surfaces start disagreeing about the
/// same browser.
///
/// It deliberately omits the defensive rejects `validate_host_header` adds for
/// `Host` (userinfo, port out of range). Those make groove *stricter* than rmcp,
/// which is safe when groove is the only gate. Here it is not: `/mcp` is guarded
/// by rmcp's copy and the admin router by this one, and a rule present in only
/// one of them is a surface answering 403 where its neighbour answers 200.
fn parse_origin(value: &str) -> Result<NormalizedOrigin, OriginRejection> {
    // rmcp trims first (`tower.rs:782`), so a padded entry in `groove.toml`
    // is honoured rather than dropped. Measured, not assumed — see
    // `an_entry_with_padding_is_still_honoured_by_the_server`.
    let value = value.trim();
    if value.is_empty() {
        return Err(OriginRejection::Empty);
    }
    if value.eq_ignore_ascii_case("null") {
        return Ok(NormalizedOrigin::Null);
    }
    let uri = http::Uri::try_from(value).map_err(|_| OriginRejection::Unreadable)?;
    // The trap this ordering exists for: `http::Uri` accepts `127.0.0.1:3100`
    // as authority-form — a scheme has to begin with a letter, so `127.0.0.1`
    // cannot be one — and it fails only here, one line further on.
    let scheme = uri
        .scheme_str()
        .ok_or(OriginRejection::NoScheme)?
        .to_ascii_lowercase();
    let authority = uri.authority().ok_or(OriginRejection::NoHost)?;
    Ok(NormalizedOrigin::Tuple {
        scheme,
        authority: NormalizedAuthority::from_authority(authority),
    })
}

/// Does this request's origin match the allow-list? `rmcp`'s `origin_is_allowed`
/// (`tower.rs:799-822`), for the routes rmcp does not serve.
///
/// An empty list means "do not validate", exactly as it does upstream — that is
/// what `allowed_origins = []` buys, and both surfaces have to read it the same
/// way. Unparseable entries are dropped rather than refused, also as upstream;
/// [`check_origin_list`] is what stops such an entry from reaching a running
/// server in the first place.
fn origin_is_allowed(origin: &NormalizedOrigin, allowed_origins: &[String]) -> bool {
    if allowed_origins.is_empty() {
        return true;
    }
    allowed_origins
        .iter()
        .filter_map(|raw| parse_origin(raw).ok())
        .any(|allowed| match (&allowed, origin) {
            (NormalizedOrigin::Null, NormalizedOrigin::Null) => true,
            (
                NormalizedOrigin::Tuple {
                    scheme: a_scheme,
                    authority: a_authority,
                },
                NormalizedOrigin::Tuple {
                    scheme: o_scheme,
                    authority: o_authority,
                },
            ) => a_scheme == o_scheme && a_authority.matches(o_authority),
            _ => false,
        })
}

/// Refuse a whole `[transport.http].allowed_origins` list before it becomes
/// part of a running configuration.
///
/// **Where this is called is part of the design**, and two earlier placements
/// were wrong for the same reason:
///
/// - in `Config::load_from`, it checked an `allowed_origins` that
///   `restrict_untrusted` was about to discard, so a `groove.toml` in a cloned
///   repository could have stopped every command;
/// - in `Config::discover_in`, it checked a key that `index`, `search` and
///   `serve --transport stdio` never read — measured: `groove validate`, which
///   opens no socket at all, refused to run.
///
/// The rule both times is the same one: **check a value where it is
/// consumed.** That place is `Transport::resolve`'s HTTP arm, which is where
/// the list stops being text in a file and starts being what rmcp will match
/// against.
pub(crate) fn check_origin_list(origins: &[String]) -> anyhow::Result<()> {
    for entry in origins {
        if let Err(why) = check_origin_entry(entry) {
            // この文面は stderr に出るので AGENTS.md の ASCII 規約が掛かる。
            // `{entry:?}` (= `Debug`) は**印字可能な非 ASCII をそのまま通す**
            // ので、IDN を含む entry が CP932 コンソールで mojibake になる。
            // `escape_default` は非 ASCII を `\u{...}` に落とす。
            let shown = entry.escape_default();
            anyhow::bail!(
                "[transport.http].allowed_origins: {why}, got \"{shown}\". An entry \
                 rmcp cannot parse is dropped before matching, which leaves Origin \
                 validation switched on with nothing to match: every request \
                 carrying an Origin header is then refused with 403, and nothing \
                 says why. Note this key is stricter than allowed_hosts, which \
                 accepts a bare host or host:port."
            );
        }
    }
    Ok(())
}

/// Whether this entry leaves the port open, so that rmcp matches it against
/// **every** port on that host (`a_port.is_none() || a_port == o_port`,
/// `tower.rs:819`).
///
/// That is wider than RFC 6454, where an omitted port *means* the scheme's
/// default port — but it cannot be narrowed from here. The browser omits the
/// port too, so an entry respelled `http://127.0.0.1:80` would be compared
/// `Some(80)` against the request's `None` and refuse the very page it exists
/// for. The entry has to stay; what it costs is worth a word at startup.
fn origin_matches_any_port(entry: &str) -> bool {
    matches!(
        parse_origin(entry),
        Ok(NormalizedOrigin::Tuple { ref authority, .. }) if authority.port.is_none()
    )
}

/// Whether to say at startup that the Origin list we built is wider than the
/// port we bound.
///
/// The asymmetry is the point. An operator's own port-less entry is left alone:
/// `https://kb.example.com` is the shipped proxy recipe and means 443 by RFC
/// 6454, so warning on it would fire on the documented happy path. A derived
/// entry is different — we chose it knowing the port, having just bound it, and
/// still had to write one that ignores it.
fn should_warn_wide_default(configured: Option<&[String]>, origins: &[String]) -> bool {
    configured.is_none() && origins.iter().any(|o| origin_matches_any_port(o))
}

/// `http` の既定ポート。RFC 6454 の origin 直列化はこれを**省く**。
const HTTP_DEFAULT_PORT: u16 = 80;

/// 1 つの host 表記に対して、クライアントが送りうる origin をすべて返す。
///
/// **規則は 1 つ**: 「bind したアドレスに対して相手が使いうる綴りは全部載せる」。
/// 綴りが違うだけの同一 origin なので、許可範囲は広がらない — 広がるのは
/// 「一致しなくて 403、しかも原因が見えない」を避けられる範囲だけ。
///
/// port 80 で 2 つ返すのがその適用例。RFC 6454 は既定ポートを origin から省くので、
/// **port 80 のサーバにブラウザが送るのは `http://127.0.0.1` であって
/// `http://127.0.0.1:80` ではない** (codex P2 round 5 on PR #173)。
fn origins_for_host(host: &str, port: u16) -> Vec<String> {
    if port == HTTP_DEFAULT_PORT {
        vec![format!("http://{host}"), format!("http://{host}:{port}")]
    } else {
        vec![format!("http://{host}:{port}")]
    }
}

/// bind したアドレスに対して、クライアントが `Host` / `Origin` に載せうる
/// host 表記をすべて返す (IPv6 は bracket 付き、`:port` を連結できる形)。
///
/// **綴りが 1 つに決まらないため。** 実測 (2026-08-17):
///
/// ```text
/// Rust の Ipv6Addr::to_string()  ::ffff:127.0.0.1   <- dotted (IPv4-mapped 特例)
/// WHATWG URL の直列化            ::ffff:7f00:1      <- hex piece、ブラウザはこちら
/// ```
///
/// どちらも同じアドレスで、どちらも誤りではない。片方に賭けると、賭けを外した側の
/// クライアントが 403 を受け取る — しかも `Host` 側の正規化
/// ([`NormalizedAuthority`]) は bracket 剥がしと小文字化しかせず、IPv6 を
/// 再直列化しないので救われない。**だから両方載せる**
/// (codex P2 round 5 on PR #173)。
///
/// **到達性の実測 (2026-08-17、Windows)**: `--bind [::ffff:127.0.0.1]:3196` は
/// OS が `WSAEADDRNOTAVAIL` (os error 10049) で拒否する。つまり Windows では
/// 「mapped アドレスに bind したサーバ」は作れない。ここを整えているのは
/// ① 他 OS の挙動を測っていない ② `is_loopback_peer` は **peer** 側 (BU-21 =
/// dual-stack listener が IPv4 クライアントを `::ffff:a.b.c.d` として報告する、
/// 実在する経路) でも使う、の 2 点による。**この分岐が Windows で発火する
/// 経路は現時点で見つかっていない**と分かった上で残している。
///
/// 分岐が IPv4-mapped だけなのは、綴りが割れるのがそこだけだからである。
/// IPv4-compatible (`::a.b.c.d`) も Rust は dotted で出すが、
/// [`is_loopback_peer`] が意図的に unwrap しないので loopback 判定に通らず、
/// この一覧に載る経路が無い。
pub(crate) fn client_host_forms(ip: std::net::IpAddr) -> Vec<String> {
    match ip {
        std::net::IpAddr::V4(v4) => vec![v4.to_string()],
        std::net::IpAddr::V6(v6) => {
            let mut forms = vec![format!("[{v6}]")];
            if let Some(v4) = v6.to_ipv4_mapped() {
                let o = v4.octets();
                let hex = format!(
                    "[::ffff:{:x}:{:x}]",
                    u16::from_be_bytes([o[0], o[1]]),
                    u16::from_be_bytes([o[2], o[3]])
                );
                if !forms.contains(&hex) {
                    forms.push(hex);
                }
            }
            forms
        }
    }
}

/// 設定値と **実際に bind したアドレス** から、有効な `Host` allow-list を決める。
///
/// [`effective_allowed_origins`] と同じ形、同じ理由。**round 2 では「`allowed_hosts`
/// の既定は rmcp が持っているので触らない」と判断したが、round 8 でこちらが
/// 構築するようになったのでその理由は消えた** (codex P2 round 9 on PR #173)。
///
/// 効き所は `--bind 127.0.0.2:3100` のような **`127.0.0.1` 以外の loopback**。
/// origin 側と admin allow-list には `127.0.0.2` が入るのに Host 側に入らないと、
/// ブラウザが送らざるを得ない `Host: 127.0.0.2:3100` が `/mcp` で 403 になる。
///
/// **非 loopback の bind では何も足さない。** `--bind 192.168.1.10` で LAN の
/// Host を黙って許可したら、運用者が書いていない許可を配ることになる。
///
/// 空 `Vec` は「全 Host 許可」(rmcp の `disable_allowed_hosts` 相当) なので、
/// 明示されたら**そのまま通す** — 既定を混ぜて黙って狭めない。
pub(crate) fn effective_allowed_hosts(
    configured: Option<Vec<String>>,
    bound: SocketAddr,
) -> Vec<String> {
    if let Some(list) = configured {
        return list;
    }
    let mut hosts: Vec<String> = DEFAULT_LOOPBACK_HOSTS
        .iter()
        .map(|h| h.to_string())
        .collect();
    if is_loopback_peer(bound.ip()) {
        // port 付きにしないのは、allow-list 側が bare host なら **どの port でも**
        // 一致するため (`validate_host_header` の比較 semantics)。
        for host in client_host_forms(bound.ip()) {
            if !hosts.contains(&host) {
                hosts.push(host);
            }
        }
    }
    hosts
}

/// 設定値と **実際に bind したアドレス** から、有効な `Origin` allow-list を決める。
///
/// 引数が要求値 (`addr`) ではなく bind 結果 (`listener.local_addr()`) なのが要点で、
/// 理由が 2 つある:
///
/// 1. **port** — `--bind 127.0.0.1:0` は「OS に選ばせる」意味なので、要求値から
///    既定を組むと `http://127.0.0.1:0` (= ブラウザが決して送れない origin) だけを
///    許可した状態になり、実際に割り当てられた port が 403 になる
///    (codex P2 round 1 on PR #173)
/// 2. **アドレス** — loopback は `127.0.0.1` だけではない。`127.0.0.0/8` は全体が
///    loopback なので `--bind 127.0.0.2:3100` は `--i-know` 無しで通るが、そこを
///    開いたブラウザは `Origin: http://127.0.0.2:3100` を送る。既定 3 種にそれが
///    無いと、Origin 検証が理由で 403 になる (codex P2 round 2 on PR #173)
///
/// **ただし 2 は「Origin では弾かない」までしか保証しない。** 実測 (bind
/// `127.0.0.2:3197`): `Host: 127.0.0.2:3197` は **rmcp の Host allow-list**
/// (既定 = `localhost` / `127.0.0.1` / `::1`) で先に 403 になる。`Host` を
/// `127.0.0.1` にすると 200 が返り、その状態で `Origin: http://127.0.0.2:3197`
/// も通る = ここの修正は効いている。つまり 127.0.0.1 以外の loopback を
/// **ブラウザから実際に使う**には `[transport.http].allowed_hosts` の設定も要る。
/// それは別の面の話なので本 fn では触らない (`allowed_hosts` の既定は rmcp が
/// 持っており、こちらで組み直すと既定値の実装が 2 つになる)。
///
/// 判定に [`is_loopback_peer`] を使うのは、`admin_host_check` と**同じ問い**
/// (「このアドレスは loopback か」) だから。別実装を置くと、IPv4-mapped IPv6 の
/// ような端の扱いが 2 箇所で食い違う。
pub(crate) fn effective_allowed_origins(
    configured: Option<Vec<String>>,
    bound: SocketAddr,
) -> Vec<String> {
    // 明示された list はそのまま使う (空 `Vec` = 検証無効も含めて)。既定を混ぜると
    // 「運用者が書いていない値」がセキュリティ設定に入り、共有ホストで
    // 「ローカルのブラウザこそ排除したい」が表現できなくなる。
    let Some(list) = configured else {
        let mut origins = default_allowed_origins(bound.port());
        if is_loopback_peer(bound.ip()) {
            for host in client_host_forms(bound.ip()) {
                for entry in origins_for_host(&host, bound.port()) {
                    if !origins.contains(&entry) {
                        origins.push(entry);
                    }
                }
            }
        }
        return origins;
    };
    list
}

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
    /// Action points at `groove-svc.exe`, which detach-spawns this process and
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
    /// groove 拡張の defensive reject (= userinfo / port out-of-range)。
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

/// 「このサーバに届くローカルな名前」の**唯一の定義**。
///
/// 元は rmcp の default loopback list の *mirror* だったが、
/// (codex P1 round 8 on PR #173) で **こちらを定義側にした** — `allowed_hosts`
/// 省略時も `with_allowed_hosts` でこの集合を rmcp に渡すようにしたので、
/// mirror ではなくなった。上流の既定が変わっても、こちらの 4 つのリストが
/// 揃ってずれる / 揃わなくなる、のどちらも起きない。
///
/// IPv6 は **bracketed** (`"[::1]"`) を一次形にする。allow-list 側は
/// `NormalizedAuthority::from_allowed_entry` の fallback で unbracketed
/// (`"::1"`) も同等扱いされるため、`Authority::try_from` が parse できる
/// bracketed 形式にすると helper 内 normalize が単純化される。
/// **rmcp 側も同じ正規化を持つ** (本ファイルの `NormalizedAuthority` がその
/// mirror) ため、bracketed のまま渡して問題ない — 実測で確認済み。
/// (codex P1 round 7 on PR #173) `pub(crate)`: this is the crate's one answer to
/// "which local names reach this server". Three allow-lists ask it — `/healthz`
/// Host validation, the `/mcp` Origin defaults, and the admin router's
/// `allowed_admin_hosts` — and a copy in any of them means a new alias would be
/// accepted by one surface and refused by another.
///
/// The bracketed IPv6 spelling is the primary form here;
/// [`NormalizedAuthority::from_allowed_entry`] strips the brackets, so an entry
/// written `"::1"` compares equal.
pub(crate) const DEFAULT_LOOPBACK_HOSTS: &[&str] = &["localhost", "127.0.0.1", "[::1]"];

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
/// - groove 拡張の defensive reject: userinfo (`user@`) / port out-of-range
pub(crate) fn validate_host_header(
    host_raw: Option<&str>,
    allowed: Option<&[String]>,
) -> Result<(), HostRejection> {
    let raw = host_raw.ok_or(HostRejection::MissingHost)?;

    // userinfo pre-check: Authority::try_from("user@host") は Ok を返し
    // userinfo を strip するが、groove は defensive に reject する
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
    allowed_origins: Option<Vec<String>>,
    healthz_public: bool,
    max_sessions: u32,
    shared: KbServerShared,
) -> Result<()> {
    // bind 範囲と allow-list の組合せが噛み合っていない時に warn を出す。
    if should_warn_non_loopback_bind(&addr, allowed_hosts.as_deref()) {
        tracing::warn!(
            bind = %addr,
            "{} groove has no authentication, and Host header validation is not a \
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

    // (1.0 blocker 4) Origin validation. rmcp's default is an empty list, which
    // means "do not validate" — so leaving this unset ships a server that does
    // not meet the MCP specification's Streamable HTTP requirement. The default
    // below is the loopback origins for the bind port; an operator publishing
    // through a proxy names their public origin in the config instead.
    //
    // This is NOT authentication, and enabling it does not break existing use:
    // per RFC 6454 and rmcp, a request that carries no `Origin` header still
    // passes. MCP clients, the tray and curl send none. What it stops is a web
    // page in the operator's own browser reaching this port cross-origin.
    // (codex P1 round 1 on PR #173) Bind before deriving the default. The port
    // that matters is the one the OS actually assigned: `--bind 127.0.0.1:0`
    // asks it to choose, so a default built from `addr` would allow `:0` — an
    // origin no browser can ever send — and answer 403 to the real one.
    let listener = tokio::net::TcpListener::bind(addr).await.with_context(|| {
        format!(
            "failed to bind {addr}: is another groove instance running, or the \
                 port occupied?"
        )
    })?;
    let bound = listener.local_addr().unwrap_or(addr);

    let origins = effective_allowed_origins(allowed_origins.clone(), bound);
    if origins.is_empty() {
        tracing::warn!(
            "[transport.http].allowed_origins is empty, which disables Origin \
             validation. The MCP specification requires servers to validate it \
             to prevent DNS rebinding, so any web page loaded in a browser on \
             this machine can now reach /mcp cross-origin. Remove the key to \
             restore the loopback-only default."
        );
    } else if allowed_origins.is_some() && !origins.iter().any(|o| names_a_loopback_host(o)) {
        // (Phase 4 PR-2) `/ui` searches through `/mcp` now, so it is subject to
        // this list for the first time. An operator who replaced the default
        // with only their public origin will find `/ui` answering 403 to its own
        // requests, with nothing on screen to say why -- the page is served, and
        // only the search fails. Say it here, where the cause is still visible.
        // Same scope as the `allowed_hosts` warning below: no loopback entry at
        // all. A list holding one loopback origin but not the scheme, name or
        // port this page is opened with is refused without a word here.
        tracing::warn!(
            "[transport.http].allowed_origins names no loopback origin, so /ui \
             opened on this machine cannot search: its requests to /mcp will be \
             refused. Add the exact origins you browse with \
             (http://127.0.0.1:{port}, http://localhost:{port}) alongside the \
             public one. Setting the key replaces the default list rather than \
             extending it, so an entry for one origin does not cover another.",
            port = bound.port(),
        );
    } else if should_warn_wide_default(allowed_origins.as_deref(), &origins) {
        // Today this is port 80 and only port 80, where `origins_for_host` adds
        // the port-less spelling RFC 6454 requires. The condition asks about the
        // list rather than the port so that it keeps describing the list if that
        // function ever changes.
        tracing::warn!(
            "bound to port {port}, where the default Origin allow-list has to \
             include the port-less spelling a browser sends (RFC 6454 omits the \
             default port). An allow-list entry with no port matches EVERY port \
             on that host, so a page served from any other local port can now \
             reach /mcp cross-origin. Name the origins you actually browse with \
             in [transport.http].allowed_origins to close this.",
            port = bound.port(),
        );
    }
    // (codex P1 round 8 on PR #173) The `None` branch used to leave rmcp's own
    // default in place, which made `/mcp`'s Host check the one list not fed by
    // `DEFAULT_LOOPBACK_HOSTS` — so the next alias added to that constant would
    // have been honoured by `/healthz`, by Origin validation and by the admin
    // router, and refused by `/mcp`. Passing it explicitly inverts the old
    // relationship on purpose: the constant stops being a mirror of an upstream
    // value and becomes the definition, which also means a change to rmcp's
    // default can no longer move one of our four lists without the others.
    //
    // `should_warn_non_loopback_bind` above still reads the operator's
    // `allowed_hosts`, not this: the warning is about whether they said
    // anything, and that question is unchanged.
    //
    // (codex P2 round 9 on PR #173) One effective Host list, shared by `/mcp`
    // and `/healthz`, and it includes the bound loopback address for the same
    // reason the Origin list does.
    let effective_hosts = effective_allowed_hosts(allowed_hosts.clone(), bound);
    // (codex P2 round 1 on PR #174) The symmetric warning, and the one that is
    // easier to hit: the documented LAN recipe is to set `allowed_hosts` to the
    // public name. `/ui` still opens through localhost, because the admin router
    // has its own allow-list — but the page's request to `/mcp` carries
    // `Host: localhost`, which Host validation refuses *before* Origin
    // validation is consulted. Measured: `/ui` 200, its search 403.
    if allowed_hosts.is_some()
        && !effective_hosts.is_empty()
        && !effective_hosts.iter().any(|h| names_a_loopback_host(h))
    {
        // (codex P2 round 2 on PR #174) This fires only when the list has NO
        // loopback entry, which is the case where every local address fails.
        // A list holding `127.0.0.1` while the operator opens `/ui` through
        // `localhost` is refused too and says nothing here -- measured -- so
        // neither this text nor the docs may promise that the log identifies
        // every failure. The page states what it needs instead.
        tracing::warn!(
            "[transport.http].allowed_hosts names no loopback alias, so /ui \
             opened on this machine cannot search: its requests to /mcp are \
             refused by Host validation. Add the exact names you browse with \
             (localhost, 127.0.0.1) alongside the public one. Setting the key \
             replaces the default list rather than extending it, so an entry \
             for one local name does not cover another."
        );
    }
    // (L-5) One list, two enforcers — the same arrangement `effective_hosts`
    // already has. rmcp keeps `origin_is_allowed` private, so the admin router
    // cannot call the check that guards `/mcp`; what it can do is compare
    // against the identical list, which is what makes the agreement testable
    // instead of hoped for.
    let admin_origins = Arc::new(origins.clone());
    let mcp_config = StreamableHttpServerConfig::default()
        .with_allowed_hosts(effective_hosts.clone())
        .with_allowed_origins(origins);
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
        // (codex P2 round 9 on PR #173) The same effective list `/mcp` got, not
        // the operator's raw value — otherwise `--bind 127.0.0.2` would answer
        // `/mcp` and refuse `/healthz` for the identical Host.
        let allowed_state = Arc::new(Some(effective_hosts.clone()));
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
    // (Phase 4 PR-2) `/api/search` is gone. It only ever passed `query` and
    // `limit` — 2 of the 17 parameters `search` takes — so `/mcp` was already
    // the better endpoint for anything outside this process, and `/ui` uses it
    // now.
    // What remains here is `/api/admin/status`, which reports operational state
    // (version, pid, indexing progress) that has no place in a tool surface
    // built for language models, and which the tray polls.
    //
    // (L-5 / L-6) Three layers, and the order is the point. The last `.layer`
    // is the outermost, so this reads bottom-up: security headers wrap
    // everything (including both gates' refusals), then the Host check, then
    // the Origin check, then the handler. Host before Origin is rmcp's order
    // for `/mcp` (`validate_dns_rebinding_headers`, `tower.rs:864-879`), and
    // `the_host_check_answers_before_the_origin_check` pins that this stays so
    // — a request failing both gets the same reply from either surface.
    let admin_router = Router::new()
        .route("/api/admin/status", get(api_admin_status))
        .route("/ui", get(ui_index))
        .with_state(Arc::clone(&factory_shared))
        .layer(middleware::from_fn_with_state(
            admin_origins,
            admin_origin_check,
        ))
        .layer(middleware::from_fn_with_state(
            Arc::clone(&factory_shared),
            admin_host_check,
        ))
        .layer(middleware::from_fn(admin_security_headers));

    let app = healthz_router
        .merge(admin_router)
        .nest_service("/mcp", mcp_router)
        .layer(tower_http::limit::RequestBodyLimitLayer::new(
            REQUEST_BODY_MAX_BYTES,
        ));

    eprintln!(
        "groove server ready (http transport, listening on {})",
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
        eprintln!("groove: shutdown signal received");
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
    /// **await せずに**現在数を読む。取れなければ `None`。
    ///
    /// 同期であることが要点。予約と解放は同じ [`std::sync::Mutex`] の中で
    /// 行うので、その中で await できてはいけない (そして await しないからこそ
    /// `Drop` からも同じ lock を取れる)。rmcp の `sessions` は
    /// `tokio::sync::RwLock` なので `try_read` が使える。write lock を握って
    /// いるのは session の挿入 / 削除の一瞬だけ。
    fn try_get(&self) -> Option<usize> {
        match self {
            Self::Rmcp(manager) => manager.sessions.try_read().ok().map(|s| s.len()),
            #[cfg(test)]
            Self::Fixed(n) => Some(n.load(std::sync::atomic::Ordering::SeqCst)),
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
/// と、A が抜けた後の `in_flight` を見て、**どちらにも A が数えられていない**
/// 状態で通ってしまう (round 2 の P1)。
///
/// 読む順序を入れ替えて「insert は解放より前に起きる」で論証する案は、
/// **実測で破れた** (上限 4 に対し 5-6 セッション)。`compare_exchange` は
/// 「変わっていない」と「変わって戻った」を区別しないので、
/// `1 → 0 → 1` の間に別の予約が入り、その分を数え落とす。
///
/// # なので排他で構造的に閉じる
///
/// 「生きている数を読む」「`in_flight` を読む」「1 増やす」を
/// **[`std::sync::Mutex`] の 1 つの臨界区間**にまとめ、席を返す側も同じ lock を
/// 取る。これで割り込める場所が存在しなくなる — 論証ではなく形で保証する。
///
/// 同期 Mutex を選べるのは、臨界区間で **await しない**から
/// ([`LiveSessionCount::try_get`] が同期)。そしてそのおかげで、
/// 要求が打ち切られたときの `Drop` からも同じ lock が取れる
/// (round 3 の P1: 打ち切り経路だけ lock の外に置くと、そこから同じ穴が開く)。
///
/// # write lock と競った場合
///
/// `try_get` は rmcp が session を挿入 / 削除している一瞬だけ `None` を返す。
/// その時は lock を手放して譲り、少し待って読み直す。
/// [`RESERVE_ATTEMPTS`] 回とも競ったら断る (= 安全側)。
#[derive(Default)]
struct Admissions {
    /// 予約と解放の両方が取る。中身は `in_flight` (= 通したが `live` にまだ
    /// 現れていない数)。
    in_flight: std::sync::Mutex<usize>,
}

/// `try_get` が rmcp の write lock と競った時に読み直す回数。
const RESERVE_ATTEMPTS: usize = 8;

impl Admissions {
    async fn try_reserve(
        self: &Arc<Self>,
        live: &LiveSessionCount,
        max_sessions: u32,
    ) -> Option<AdmissionSeat> {
        for _ in 0..RESERVE_ATTEMPTS {
            {
                // poison から復帰する: 数を 1 つ数え損ねるより、上限が
                // 恒久的に壊れる方が悪い。
                let mut in_flight = self
                    .in_flight
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                if let Some(now_live) = live.try_get() {
                    if now_live + *in_flight >= max_sessions as usize {
                        return None;
                    }
                    *in_flight += 1;
                    return Some(AdmissionSeat {
                        admissions: Arc::clone(self),
                    });
                }
                // `try_get` が取れなかった。lock を手放してから譲る。
            }
            tokio::task::yield_now().await;
        }
        None
    }
}

/// 予約した 1 席。`Drop` で返る。
///
/// 応答を返し終えた通常経路でも、要求が打ち切られた経路でも同じ `Drop` が
/// 走るので、席の返し方は 1 通りしかない。[`Admissions`] の読み順がその前提
/// (「insert は解放より前」) だけに依存しているので、これで足りる。
struct AdmissionSeat {
    admissions: Arc<Admissions>,
}

impl Drop for AdmissionSeat {
    fn drop(&mut self) {
        // **予約と同じ lock**。ここを外すと、B が 2 つの数を読んでいる最中に
        // A が抜けられるようになり、上限が漏れる (round 3 の P1)。
        let mut in_flight = self
            .admissions
            .in_flight
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *in_flight = in_flight.saturating_sub(1);
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
/// 上限に達していれば 429 を返す。それ以外のリクエストには一切触らない。
///
/// # session を作りに来た、をどう見分けるか
///
/// **`initialize` 要求であること**。3 つの条件がすべて必要:
/// POST であること、`Mcp-Session-Id` ヘッダが無いこと (有れば既存 session 宛)、
/// そして body が単一の JSON-RPC `initialize` 要求であること。
///
/// `initialize` を条件に使えるのは偶然ではない。**MCP 2026-07-28 (SEP-2567) は
/// session ごと廃止し、`initialize` / `notifications/initialized` も削除した**。
/// rmcp 3 はそのバージョンを交渉したリクエストを常に stateless で処理するので、
/// **`initialize` が来る = 旧ライフサイクル = session が作られる**が厳密に一致する。
/// 逆に言えば、2026-07-28 のクライアントは session 無しで `tools/call` を直接
/// POST してくるので、**そこを弾いてはならない**。
///
/// (BU-32 の初版は「session 無し POST が initialize でなければ 422」だった。
/// rmcp 1.4.0 が `create_session()` を initialize 検証より先に呼び、続く 422 が
/// `close_session` を呼ばずに session を取り残す漏れ — 実測 117 MiB/秒 — を
/// 塞ぐためで、rmcp 2.0.0 が上流で直すまでは必要だった
/// (modelcontextprotocol/rust-sdk#934)。3.x に上げた今それは不要で、しかも
/// **残すと 2026-07-28 のクライアントが全滅する** — 実測で 422 を返していた。)
///
/// # 何を複製し、何を複製しないか
///
/// 判定は「body が単一の JSON-RPC `initialize` 要求か」だけ。これは **MCP 仕様**
/// であって rmcp の実装詳細ではない。Host / Accept / Content-Type の検証は
/// **複製しない** (F-64 で host 検証を mirror せず委譲に倒した判断と同じ)。
/// 弾くのは「上限が満杯のとき」だけなので、余計に巻き込む余地も小さい。
///
/// body が JSON にならない場合も素通しする。rmcp が自分で答える。
async fn mcp_session_gate(
    State(gate): State<McpSessionGate>,
    req: Request,
    next: Next,
) -> Response {
    // session を作りに来ないリクエストは一切触らない。
    // 新しい session ができるのは「POST かつ Mcp-Session-Id ヘッダ無し」の時だけ
    // (rmcp 3.1.2 tower.rs:1729-1736 で header 有りは既存 session 分岐へ行く)。
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

    // `initialize` 以外は session を作らない (stateless 経路)。素通しする。
    let is_initialize = serde_json::from_slice::<serde_json::Value>(&bytes)
        .ok()
        .and_then(|v| {
            v.get("method")
                .and_then(|m| m.as_str())
                .map(|m| m == "initialize")
        })
        .unwrap_or(false);
    if !is_initialize {
        return next
            .run(Request::from_parts(parts, Body::from(bytes)))
            .await;
    }

    // 予約を取ってから通す。`_seat` は応答を返し終えるまで生き、そこで Drop
    // が席を返す (= rmcp が session を map に入れ終えた後)。
    let _seat = if gate.max_sessions > 0 {
        match gate
            .admissions
            .try_reserve(&gate.live, gate.max_sessions)
            .await
        {
            Some(seat) => Some(seat),
            None => {
                let live = gate.live.try_get().unwrap_or_default();
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

    next.run(Request::from_parts(parts, Body::from(bytes)))
        .await
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
/// groove 拡張: HTTP/2 `:authority` fallback (= Q4=C2 で意図的に維持、
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
/// (codex P2 round 3 on PR #173) `pub(crate)` because this is **the** answer to
/// "is this address loopback" for the whole crate: the admin gate, the
/// `--bind` acknowledgement in `check_cli_bind_ack`, `service install`, and the
/// default `Origin` list all ask it. They used to answer it three different
/// ways, and disagreed on `::ffff:127.0.0.1` — the admin router let such a peer
/// in while both bind gates called the same address network exposure.
pub(crate) fn is_loopback_peer(ip: std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v4) => v4.is_loopback(),
        std::net::IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => v4.is_loopback(),
            None => v6.is_loopback(),
        },
    }
}

/// (Phase 4 PR-2) Does this entry name a local address?
///
/// Takes either an origin (`http://127.0.0.1:3100`) or a bare allow-list host
/// (`localhost`, `kb.example.lan`), because the scheme is optional here and
/// both lists are asked the same question: would `/ui`, opened on this machine,
/// be able to reach `/mcp` under this configuration?
///
/// It answers with the pieces that already exist — [`DEFAULT_LOOPBACK_HOSTS`]
/// for the names, [`is_loopback_peer`] for the addresses, [`NormalizedAuthority`]
/// for the parsing — rather than adding another notion of "local" to a file that
/// spent PR #173 collapsing them.
fn names_a_loopback_host(entry: &str) -> bool {
    let authority = entry.split_once("://").map_or(entry, |(_, rest)| rest);
    let host = NormalizedAuthority::from_allowed_entry(authority).host;
    if host.is_empty() {
        return false;
    }
    DEFAULT_LOOPBACK_HOSTS
        .iter()
        .any(|alias| NormalizedAuthority::from_allowed_entry(alias).host == host)
        || host.parse::<std::net::IpAddr>().is_ok_and(is_loopback_peer)
}

fn should_warn_non_loopback_bind(addr: &SocketAddr, allowed_hosts: Option<&[String]>) -> bool {
    // (codex P2 round 4 on PR #173) Shared predicate, not `IpAddr::is_loopback`.
    // Once `check_cli_bind_ack` accepted `[::ffff:127.0.0.1]` as loopback, this
    // line still called it exposure and warned about a bind the CLI had just
    // approved — the two halves of one decision contradicting each other on
    // startup.
    if is_loopback_peer(addr.ip()) {
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
             [transport.http].allowed_hosts explicitly in groove.toml (e.g. \
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
        // (BU-18, codex P2 round 1 on PR #154) A poisoned lock used to be a 500
        // here, permanently: poisoning is sticky, so the one endpoint an
        // operator reaches for after a panic was the one that stopped
        // answering. The payload is plain data — recovering it is the whole
        // point of the change.
        let guard = crate::poison::recover(shared.indexing_state.lock(), "indexing_state");
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

/// `/ui` — the operator's view of their own server: what it is doing, and a
/// search box.
///
/// Single file, no external requests, and every string that came out of the
/// knowledge base is placed with `textContent` rather than `innerHTML`.
///
/// (Phase 4 PR-2) Its search goes through **`/mcp`**, not a private endpoint,
/// which makes this page the smallest example of an MCP client over Streamable
/// HTTP — and means the browsing story it offers is the one external clients
/// get. `docs/stability.md` records the intent to retire it during 1.x, once a
/// real client exists.
async fn ui_index() -> axum::response::Html<&'static str> {
    axum::response::Html(include_str!("webui_index.html"))
}

/// (Phase 4 PR-2) `/api/search` no longer exists in a running server. The
/// handler survives behind the test gate because
/// `tests/runtime_starvation.rs` needs a route whose body blocks on the
/// embedder lock, to prove `/healthz` keeps answering while it does. That test
/// must not be edited, and `/mcp` cannot stand in for it here without building
/// the whole rmcp service inside the test router.
#[cfg(any(test, feature = "test-helpers"))]
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
#[cfg(any(test, feature = "test-helpers"))]
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
        tracing::warn!(%peer, "admin: rejected a request from a non-loopback peer");
        return Err((
            StatusCode::FORBIDDEN,
            "admin endpoints are loopback-only".to_string(),
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
    // (L-7) The rejected value goes to the log, not into the body. It came from
    // the caller, and echoing caller-controlled bytes back is a habit worth not
    // having even where the response is `text/plain` and now carries `nosniff`
    // — the sibling gate on `/healthz` says `Host header is not allowed` and
    // nothing more, rmcp says the same for `/mcp`, and this was the one surface
    // that answered differently. Nothing is lost: `tracing::warn!` still names
    // the value, on the console of the machine that can act on it.
    //
    // Logging it stays inside the ASCII rule AGENTS.md sets for stderr, and not
    // by luck: `HeaderValue::to_str` yields `Ok` only for visible ASCII, so a
    // header that could have arrived as mojibake is `None` here and reaches the
    // `MissingHost` arm instead.
    let host_for_log = host_header.unwrap_or("");
    match validate_host_header(host_header, Some(shared.allowed_admin_hosts.as_slice())) {
        Ok(()) => {}
        Err(HostRejection::MissingHost) => {
            return Err((StatusCode::BAD_REQUEST, "missing Host header".to_string()));
        }
        Err(HostRejection::MalformedHost) => {
            tracing::warn!(
                host = host_for_log,
                "admin: rejected a malformed Host header"
            );
            return Err((StatusCode::BAD_REQUEST, "Invalid Host header".to_string()));
        }
        Err(HostRejection::NotAllowed) => {
            tracing::warn!(
                host = host_for_log,
                "admin: rejected a Host header outside the admin allow-list \
                 (possible DNS rebinding attempt)"
            );
            return Err((
                StatusCode::FORBIDDEN,
                "Host header is not allowed".to_string(),
            ));
        }
    }
    Ok(next.run(req).await)
}

/// (L-5) The admin router's `Origin` check — the same question rmcp answers for
/// `/mcp`, asked of the routes rmcp does not serve.
///
/// Until now `/ui` and `/api/admin/status` were reachable cross-origin by any
/// page open in the operator's browser. Nothing leaked: the responses are
/// GETs, and without CORS headers a foreign page cannot read what comes back.
/// **What was missing was the guarantee that this stays true.** The first admin
/// route with a side effect would have been callable from
/// `https://anything.example` on the day it was added, and nothing in this file
/// would have objected.
///
/// It reads the effective list `/mcp` got, not the operator's raw value, for the
/// reason `/healthz` shares its: two lists derived from one setting drift the
/// moment someone edits one of them. `an_origin_gets_the_same_verdict_from_both_surfaces`
/// pins that they agree, through a running server, rather than asserting it here.
///
/// **What this does not close.** A request that carries no `Origin` still
/// passes — that is RFC 6454, rmcp and the MCP specification alike, and it is
/// what lets ordinary clients, the tray and `curl` work. So a cross-origin
/// `<img src>` or a top-level navigation is unaffected: they send no `Origin`.
/// This refuses `fetch` / `XMLHttpRequest`, which do. A future side-effecting
/// admin route therefore still may not be a `GET`.
async fn admin_origin_check(
    State(allowed): State<Arc<Vec<String>>>,
    req: axum::http::Request<axum::body::Body>,
    next: axum::middleware::Next,
) -> Result<axum::response::Response, (StatusCode, String)> {
    // Empty means "do not validate", as it does upstream and as the startup
    // warning says. An operator who switched Origin validation off for `/mcp`
    // did not ask for it to stay on here.
    if allowed.is_empty() {
        return Ok(next.run(req).await);
    }
    let Some(raw) = req.headers().get(http::header::ORIGIN) else {
        return Ok(next.run(req).await);
    };
    let Ok(origin_str) = raw.to_str() else {
        // No value in the log line: it is not ASCII, which is why we are here.
        tracing::warn!("admin: rejected a request whose Origin header is not ASCII");
        return Err((
            StatusCode::BAD_REQUEST,
            "Invalid Origin header encoding".to_string(),
        ));
    };
    let Ok(origin) = parse_origin(origin_str) else {
        tracing::warn!(
            origin = origin_str,
            "admin: rejected a malformed Origin header"
        );
        return Err((StatusCode::BAD_REQUEST, "Invalid Origin header".to_string()));
    };
    if !origin_is_allowed(&origin, &allowed) {
        tracing::warn!(
            origin = origin_str,
            "admin: rejected a disallowed Origin header (possible cross-origin attempt)"
        );
        return Err((
            StatusCode::FORBIDDEN,
            "Origin header is not allowed".to_string(),
        ));
    }
    Ok(next.run(req).await)
}

/// (L-6) The response headers `/ui` had none of.
///
/// `default-src 'none'` and then back in only what this page is: its own inline
/// `<style>` and `<script>`, the `data:` favicon in its `<link rel="icon">`,
/// and same-origin `fetch` to `/mcp` and `/api/admin/status`. Nothing external
/// loads today, so nothing external is allowed to — which is what makes this a
/// guard rather than a description: the day someone reaches for a CDN, the page
/// stops working here instead of silently acquiring a second origin to trust.
///
/// `img-src data:` is here because it was measured to be needed, not because a
/// favicon sounded like an image. Serving the page without it, in Chrome:
///
/// ```text
/// Loading the image 'data:image/svg+xml,...' violates the following Content
/// Security Policy directive: "default-src 'none'". Note that 'img-src' was
/// not explicitly set, so 'default-src' is used as a fallback.
/// ```
///
/// `'unsafe-inline'` is not a concession, it is the only thing the page uses. A
/// nonce would be stricter, but the page is `include_str!`'d as a constant and
/// served unchanged; injecting a per-request nonce means rewriting it per
/// request, for a document that puts every knowledge-base string in place with
/// `textContent`.
///
/// `frame-ancestors 'none'` is why this is a header and not a `<meta>` tag —
/// CSP3 §3.3 ignores that directive (and `report-uri`, and `sandbox`) when the
/// policy arrives in a meta element.
///
/// `form-action 'none'` costs nothing while the page works: the search form
/// calls `preventDefault()`. It matters when the script has *not* run, where it
/// turns a stray navigation into nothing at all.
///
/// The policy is also sent with `/api/admin/status`, where it is inert. Scoping
/// it to one route would mean a second place that has to be remembered when a
/// route is added.
const ADMIN_CSP: &str = "default-src 'none'; script-src 'unsafe-inline'; \
     style-src 'unsafe-inline'; img-src data:; connect-src 'self'; \
     base-uri 'none'; form-action 'none'; frame-ancestors 'none'";

/// Attach [`ADMIN_CSP`] and `nosniff` to everything the admin router answers.
///
/// Outermost of the three admin layers, so the refusals from the two gates
/// carry them too — a 403 is a response a browser renders like any other.
async fn admin_security_headers(req: Request, next: Next) -> Response {
    let mut resp = next.run(req).await;
    let headers = resp.headers_mut();
    headers.insert(
        http::header::X_CONTENT_TYPE_OPTIONS,
        http::HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        http::header::CONTENT_SECURITY_POLICY,
        http::HeaderValue::from_static(ADMIN_CSP),
    );
    resp
}

/// (feature-43 PR-2) Build the axum app router with admin endpoints only.
/// Used by integration tests in `tests/webui_integration.rs` — the production
/// app composes the admin sub-router with `/healthz` + `/mcp` in `run_http`.
///
/// Gated by the `test-helpers` feature so production binaries do not carry
/// the helper. `#[cfg(test)]` alone would not make this visible to the
/// integration test crate (a separate compilation unit).
///
/// **This is not the production router, and two of the differences are
/// deliberate.** It still registers `/api/search`, which the running server
/// dropped in v0.27.0, because `tests/runtime_starvation.rs` needs a handler
/// that blocks on the embedder lock. And it carries no Origin gate: the list
/// that gate compares against is derived from the port the listener actually
/// got, and nothing here binds one. Origin validation is exercised where it
/// exists — through a running server, in `tests/http_origin.rs`.
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
        ))
        .layer(middleware::from_fn(admin_security_headers));
    axum::Router::new().merge(admin_router)
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// (1.0 blocker 4) The default `Origin` allow-list. Three properties matter
    /// and each has bitten somewhere before:
    ///
    /// - every entry carries a scheme (rmcp rejects bare hosts);
    /// - the IPv6 entry is bracketed, per RFC 6454 origin serialisation, so it
    ///   matches what a browser actually sends;
    /// - the list is never empty, because empty is rmcp's encoding for "do not
    ///   validate" and would silently undo the whole point.
    #[test]
    fn default_allowed_origins_are_loopback_for_the_given_port() {
        let origins = default_allowed_origins(3100);
        assert_eq!(
            origins,
            vec![
                "http://localhost:3100".to_string(),
                "http://127.0.0.1:3100".to_string(),
                "http://[::1]:3100".to_string(),
            ]
        );
        assert!(
            origins.iter().all(|o| o.starts_with("http://")),
            "rmcp requires a scheme on every entry"
        );
        assert!(
            !origins.is_empty(),
            "an empty list is rmcp's encoding for 'skip Origin validation'"
        );
    }

    /// The port is substituted, not hard-coded: a server on a non-default port
    /// has to accept the origin its own `/ui` will send.
    #[test]
    fn default_allowed_origins_follow_the_bind_port() {
        let origins = default_allowed_origins(4242);
        assert!(origins.iter().all(|o| o.ends_with(":4242")));
        assert!(origins.contains(&"http://[::1]:4242".to_string()));
    }

    /// The list we ship must survive the parser we validate against. Port 80 is
    /// included because it is the one port where `origins_for_host` emits a
    /// second, port-less spelling, and that spelling has to be legal too.
    #[test]
    fn every_origin_we_derive_ourselves_reaches_the_comparison() {
        for port in [80u16, 3100, 65535] {
            for entry in default_allowed_origins(port) {
                assert_eq!(
                    check_origin_entry(&entry),
                    Ok(()),
                    "we would refuse our own default entry {entry:?} on port {port}"
                );
            }
        }
    }

    /// The trap this check exists for. `allowed_hosts` takes a bare `host:port`
    /// — its parser ends in a fallback that reads the whole string as a host —
    /// so the spelling carries over to the neighbouring key, where rmcp drops it
    /// without a word and refuses every request that carries an `Origin`.
    #[test]
    fn a_bare_host_and_port_is_refused_where_allowed_hosts_would_take_it() {
        for entry in ["127.0.0.1:3100", "localhost:3100", "kb.example.com"] {
            let why = check_origin_entry(entry)
                .expect_err("a spelling with no scheme never reaches rmcp's comparison");
            assert!(
                why.contains("scheme"),
                "the message has to name what is missing, got {why:?}"
            );
        }
    }

    /// Every spelling the documentation and the shipped recipes hand an operator.
    #[test]
    fn the_spellings_we_publish_are_accepted() {
        for entry in [
            "https://kb.example.com",
            "http://127.0.0.1:3100",
            "http://localhost:3100",
            "http://[::1]:3100",
            "http://127.0.0.1",
        ] {
            assert_eq!(check_origin_entry(entry), Ok(()), "{entry:?} is documented");
        }
    }

    /// `null` is a real origin (RFC 6454 §6.1 — sandboxed frames, `file://`),
    /// and rmcp gives it a variant of its own, so refusing it here would reject
    /// a config that works.
    #[test]
    fn the_null_origin_is_an_origin() {
        for entry in ["null", "NULL", " null "] {
            assert_eq!(check_origin_entry(entry), Ok(()), "{entry:?} is an origin");
        }
    }

    /// The check answers "does rmcp still have this at match time", not "is this
    /// a well-formed origin". A trailing path is not part of a serialized origin,
    /// but rmcp keeps only the scheme and authority and never looks at it — so
    /// refusing it here would stop a config that works today from starting.
    /// Being stricter than the parser we are protecting is its own defect.
    #[test]
    fn a_trailing_path_is_not_this_checks_business() {
        assert_eq!(check_origin_entry("https://kb.example.com/mcp"), Ok(()));
    }

    /// Padding is accepted **because rmcp trims first**: `parse_origin_value`
    /// opens with `let value = value.trim();` (`tower.rs:782`) and is the only
    /// consumer of the allow-list — `origin_is_allowed` maps every entry
    /// through it (`:805`). Our own warning helper trims as well, in
    /// `NormalizedAuthority::from_allowed_entry`.
    ///
    /// So this is not leniency, it is the mirror staying accurate. Should rmcp
    /// ever stop trimming, the entry would be dropped where we said it would
    /// pass — the failure this check exists to prevent — which is why the claim
    /// is measured through a running server in `tests/http_origin.rs` rather
    /// than asserted here alone.
    #[test]
    fn padding_is_accepted_because_the_parser_we_mirror_trims() {
        for entry in [" https://kb.example.com ", "\thttp://127.0.0.1:3100\n"] {
            assert_eq!(
                check_origin_entry(entry),
                Ok(()),
                "{entry:?} reaches rmcp's comparison, so refusing it here would \
                 stop a config that works"
            );
        }
    }

    /// Empty and whitespace-only entries: rmcp drops these too.
    #[test]
    fn an_entry_with_nothing_in_it_is_refused() {
        for entry in ["", "   ", "\t"] {
            assert!(
                check_origin_entry(entry).is_err(),
                "{entry:?} would be dropped by rmcp"
            );
        }
    }

    /// A scheme with no host behind it.
    #[test]
    fn a_scheme_with_no_host_is_refused() {
        for entry in ["http://", "https://"] {
            assert!(
                check_origin_entry(entry).is_err(),
                "{entry:?} names no host"
            );
        }
    }

    /// Not every list is checked into oblivion: `[]` is the documented way to
    /// turn Origin validation off, and `run_http` warns about it at startup.
    /// The check must not be the thing that refuses it.
    #[test]
    fn an_empty_origin_list_is_still_a_legal_config() {
        check_origin_list(&[]).expect("an empty list disables validation, it is not a typo");
    }

    /// The list the shipped proxy recipe hands an operator has to load.
    #[test]
    fn the_origins_the_recipes_publish_pass_the_list_check() {
        let shipped = [
            "https://kb.example.com".to_string(),
            "http://127.0.0.1:3100".to_string(),
            "http://localhost:3100".to_string(),
        ];
        check_origin_list(&shipped)
            .expect("examples/deployments/intranet-http ships exactly this list");
    }

    /// The rejected value reaches stderr, where AGENTS.md requires ASCII: a
    /// Japanese Windows console is CP932 and renders anything else as mojibake.
    /// `Debug` keeps printable non-ASCII as-is, so an internationalized host
    /// name would have travelled through intact.
    #[test]
    fn a_rejected_entry_is_escaped_before_it_reaches_the_console() {
        let bad = ["\u{4f8b}\u{3048}.jp:3100".to_string()];
        let msg = format!(
            "{:#}",
            check_origin_list(&bad).expect_err("no scheme, so it is refused")
        );
        assert!(
            msg.is_ascii(),
            "a diagnostic has to survive a CP932 console, got {msg:?}"
        );
        assert!(
            msg.contains("\\u{4f8b}"),
            "the operator still has to recognise which entry it was, got {msg:?}"
        );
    }

    /// The startup warning fires on a property of the list, not on a port
    /// number, so this pins both halves: on an ordinary port every entry we
    /// derive names its port, and on port 80 the entries RFC 6454 obliges us to
    /// add are the ones that widen to every port on the host.
    #[test]
    fn only_the_port_80_default_leaves_the_port_open() {
        for port in [3100u16, 4242, 65535] {
            let origins = default_allowed_origins(port);
            assert!(
                !origins.iter().any(|o| origin_matches_any_port(o)),
                "port {port} needs no port-less entry, got {origins:?}"
            );
        }
        let at_80 = default_allowed_origins(HTTP_DEFAULT_PORT);
        let open: Vec<_> = at_80
            .iter()
            .filter(|o| origin_matches_any_port(o))
            .collect();
        assert_eq!(
            open,
            vec!["http://localhost", "http://127.0.0.1", "http://[::1]"],
            "at port 80 the browser sends no port, so these have to be on the list"
        );
    }

    /// `/ui` calls `/mcp` with no handshake, and `STANDARD_HEADERS` is the
    /// version that defines that mode — rmcp documents it as the first
    /// requiring SEP-2243 standard HTTP headers. That is what the page names.
    ///
    /// **Not `LATEST`.** In rmcp 3.1.2 the two are deliberately different:
    /// `LATEST` is `2025-11-25`, the newest version the SDK negotiates, and
    /// `STANDARD_HEADERS` is `2026-07-28`. `server.rs` reports `LATEST` in
    /// `initialize` and is right to; reaching for the same constant here is the
    /// obvious move, and **this is the only place that catches it**.
    ///
    /// Measured, because the reverse would have been easy to assume: a page
    /// pinned to `LATEST` still gets a result from a running server, since rmcp
    /// accepts a handshake-free call on known older versions too. The live test
    /// in `tests/webui_surface.rs` only notices a version rmcp has never heard
    /// of. Leniency today is not the contract, so the pin stays here.
    #[test]
    fn the_page_names_the_protocol_version_that_allows_a_handshake_free_call() {
        let page = include_str!("webui_index.html");
        let expected = format!(
            "const MCP_VERSION = \"{}\";",
            rmcp::model::ProtocolVersion::STANDARD_HEADERS
        );
        assert!(
            page.contains(&expected),
            "webui_index.html must declare {expected:?}; a page pinned to a \
             version rmcp does not accept without a handshake cannot search"
        );
    }

    /// An entry that names its port is not the wide kind, whatever the scheme.
    #[test]
    fn an_entry_that_names_its_port_matches_only_that_port() {
        for entry in [
            "http://127.0.0.1:3100",
            "https://kb.example.com:8443",
            "http://[::1]:3100",
        ] {
            assert!(!origin_matches_any_port(entry), "{entry:?} names a port");
        }
        for entry in ["https://kb.example.com", "http://localhost"] {
            assert!(origin_matches_any_port(entry), "{entry:?} names no port");
        }
    }

    /// The warning is about a list we built, not about a spelling. An operator
    /// who writes the same port-less origin themselves gets nothing said to
    /// them, because `https://kb.example.com` is the recipe we publish and
    /// means 443 — this is the half most likely to be "fixed" into noise later.
    #[test]
    fn the_wide_default_warning_is_only_about_a_list_we_derived() {
        let at_80 = default_allowed_origins(HTTP_DEFAULT_PORT);
        assert!(
            should_warn_wide_default(None, &at_80),
            "the default at port 80 matches every local port and has to say so"
        );
        assert!(
            !should_warn_wide_default(Some(&at_80), &at_80),
            "the same list, chosen by the operator, is their decision to make"
        );
        assert!(
            !should_warn_wide_default(None, &default_allowed_origins(3100)),
            "an ordinary port needs no port-less entry, so there is nothing to say"
        );
    }

    /// (codex P2 round 1 on PR #173) The default must be built from the port
    /// that was **bound**, not the one that was requested. `--bind 127.0.0.1:0`
    /// hands port selection to the OS, so deriving from the request would allow
    /// only `http://127.0.0.1:0` — which no browser can send — and 403 the real
    /// port. `run_http` binds first and passes `listener.local_addr().port()`,
    /// and this pins the arithmetic that would otherwise regress silently.
    #[test]
    fn effective_origins_use_the_assigned_port_not_the_requested_one() {
        let requested_port = 0u16;
        let bound: SocketAddr = "127.0.0.1:51234".parse().unwrap();
        let origins = effective_allowed_origins(None, bound);
        assert!(
            origins.iter().all(|o| o.ends_with(":51234")),
            "origins must name the assigned port, got {origins:?}"
        );
        assert!(
            !origins
                .iter()
                .any(|o| o.ends_with(&format!(":{requested_port}"))),
            "port 0 must never reach the allow-list: {origins:?}"
        );
    }

    /// (codex P2 round 2 on PR #173) `127.0.0.0/8` is loopback in its entirety,
    /// so `--bind 127.0.0.2:3100` is accepted without `--i-know` — but a browser
    /// opening that address sends `Origin: http://127.0.0.2:3100`, which the
    /// three fixed defaults do not contain. The bound address itself has to join
    /// the list, so that Origin validation is not the thing refusing it.
    ///
    /// Measured caveat, recorded so nobody reads more into this than it does:
    /// with `--bind 127.0.0.2`, `Host: 127.0.0.2:PORT` is refused a step earlier
    /// by rmcp's Host allow-list. Reaching such a bind from a browser also needs
    /// `[transport.http].allowed_hosts` — a separate surface, deliberately not
    /// changed here.
    #[test]
    fn effective_origins_include_a_bound_loopback_address_beyond_127_0_0_1() {
        let bound: SocketAddr = "127.0.0.2:3100".parse().unwrap();
        let origins = effective_allowed_origins(None, bound);
        assert!(
            origins.contains(&"http://127.0.0.2:3100".to_string()),
            "the bound loopback address must be allowed, got {origins:?}"
        );
        // The fixed three stay, because the operator may still reach the server
        // through localhost regardless of which loopback address it bound.
        assert!(origins.contains(&"http://127.0.0.1:3100".to_string()));
        assert!(origins.contains(&"http://localhost:3100".to_string()));
    }

    /// The bound address is not appended twice when it is already one of the
    /// defaults, and a non-loopback bind adds nothing: `Origin: http://0.0.0.0`
    /// is not something a browser sends, and putting the LAN address in would
    /// hand out an allowance the operator never asked for.
    #[test]
    fn effective_origins_do_not_duplicate_or_widen_beyond_loopback() {
        let loopback: SocketAddr = "127.0.0.1:3100".parse().unwrap();
        let origins = effective_allowed_origins(None, loopback);
        assert_eq!(
            origins
                .iter()
                .filter(|o| *o == "http://127.0.0.1:3100")
                .count(),
            1,
            "the bound address must not be appended twice: {origins:?}"
        );
        assert_eq!(origins, default_allowed_origins(3100));

        let wildcard: SocketAddr = "0.0.0.0:3100".parse().unwrap();
        assert_eq!(
            effective_allowed_origins(None, wildcard),
            default_allowed_origins(3100),
            "a non-loopback bind must not widen the allow-list"
        );
    }

    /// (codex P1 round 8 on PR #173) The constant is the definition of the
    /// `/healthz` default set, not a list that happens to look like it. Pinning
    /// this is what makes adding an alias a one-line change: the same constant
    /// feeds Origin defaults, the admin allow-list, and — since round 8 — the
    /// list handed to rmcp for `/mcp`.
    ///
    /// rmcp's own acceptance of the bracketed spelling is covered end to end
    /// rather than here: a server started with the shared set answers 200 to
    /// `Host: 127.0.0.1`, `localhost` and `[::1]`, and 403 to `evil.example`.
    #[test]
    fn every_shared_alias_passes_the_default_host_check() {
        for alias in DEFAULT_LOOPBACK_HOSTS {
            assert!(
                validate_host_header(Some(alias), None).is_ok(),
                "{alias} is in the shared set but the default Host check rejects it"
            );
            // The same alias with a port, which is what a client actually sends.
            let with_port = format!("{alias}:3100");
            assert!(
                validate_host_header(Some(&with_port), None).is_ok(),
                "{with_port} must be accepted; the port is stripped before comparing"
            );
        }
        assert!(
            validate_host_header(Some("evil.example"), None).is_err(),
            "the default set must not have grown open"
        );
    }

    /// (codex P1 round 6 on PR #173) Host validation and Origin validation both
    /// decide which local browser addresses count, so they read the same alias
    /// set. A second literal would drift the moment one of them gains an alias,
    /// and the failure would be a browser accepted by one check and refused by
    /// the other.
    #[test]
    fn origin_defaults_are_built_from_the_shared_loopback_alias_set() {
        let origins = default_allowed_origins(3100);
        assert_eq!(
            origins.len(),
            DEFAULT_LOOPBACK_HOSTS.len(),
            "one origin per shared alias, off the default port: {origins:?}"
        );
        for host in DEFAULT_LOOPBACK_HOSTS {
            assert!(
                origins.contains(&format!("http://{host}:3100")),
                "alias {host} is missing from the Origin defaults: {origins:?}"
            );
        }
    }

    /// (codex P2 round 9 on PR #173) The Host list gets the bound loopback
    /// address for the same reason the Origin list does. Without it,
    /// `--bind 127.0.0.2:3100` produces a server that puts `127.0.0.2` in its
    /// Origin defaults and its admin allow-list, then refuses the
    /// `Host: 127.0.0.2:3100` a browser has no choice but to send.
    ///
    /// (Phase 4 PR-2) `/ui` searches through `/mcp` now, so an
    /// `allowed_origins` that names no loopback origin leaves the page served
    /// but unable to search. This predicate decides whether to warn about that,
    /// and it recognises local by the shared alias set and the shared loopback
    /// predicate — the two things this module already uses for the question.
    #[test]
    fn loopback_origins_are_recognised_by_name_and_by_address() {
        for origin in [
            "http://localhost:3100",
            "http://127.0.0.1:3100",
            "http://[::1]:3100",
            "http://127.0.0.2:3100", // all of 127.0.0.0/8 is loopback
            "http://localhost",      // port 80, serialized without it
            "https://127.0.0.1:8443",
        ] {
            assert!(
                names_a_loopback_host(origin),
                "{origin} names a local address"
            );
        }
        for origin in [
            "https://kb.example.com",
            "http://192.168.1.10:3100",
            "http://evil.127.0.0.1:3100", // a name that merely contains one
            "",
        ] {
            assert!(!names_a_loopback_host(origin), "{origin:?} is not local");
        }
    }

    /// (codex P2 round 1 on PR #174) The predicate is asked about `Host`
    /// allow-list entries too, which carry no scheme. The documented LAN recipe
    /// — `allowed_hosts = ["kb.example.lan"]` — is exactly the configuration
    /// that leaves `/ui` open but unable to search, so it has to be recognised
    /// as *not* local while bare loopback names still are.
    #[test]
    fn bare_allow_list_entries_are_judged_too_not_just_origins() {
        for entry in ["localhost", "127.0.0.1", "[::1]", "::1", "127.0.0.2:3100"] {
            assert!(names_a_loopback_host(entry), "{entry} is a local name");
        }
        for entry in ["kb.example.lan", "192.168.1.10", "kb.example.lan:3100"] {
            assert!(!names_a_loopback_host(entry), "{entry} is not local");
        }
    }

    /// Every entry the Host defaults produce must be recognised as local, for
    /// the same reason as the origins: otherwise the warning fires against a
    /// configuration that works.
    #[test]
    fn the_default_hosts_are_all_recognised_as_loopback() {
        let bound: SocketAddr = "127.0.0.2:3100".parse().unwrap();
        for host in effective_allowed_hosts(None, bound) {
            assert!(
                names_a_loopback_host(&host),
                "{host} is a default but is not recognised as local"
            );
        }
    }

    /// Every origin the defaults produce must be recognised as local, or the
    /// warning would fire against a configuration that works. This is the pair
    /// that would drift if either side grew an entry alone.
    #[test]
    fn the_default_origins_are_all_recognised_as_loopback() {
        let bound: SocketAddr = "127.0.0.2:3100".parse().unwrap();
        for origin in effective_allowed_origins(None, bound) {
            assert!(
                names_a_loopback_host(&origin),
                "{origin} is a default but is not recognised as local"
            );
        }
    }

    /// The round-2 reasoning for leaving this alone — that rmcp owned the
    /// default — stopped being true in round 8, when we started building it.
    #[test]
    fn the_host_list_includes_the_bound_loopback_address() {
        let bound: SocketAddr = "127.0.0.2:3100".parse().unwrap();
        let hosts = effective_allowed_hosts(None, bound);
        assert!(
            hosts.contains(&"127.0.0.2".to_string()),
            "the bound loopback address must be accepted as a Host: {hosts:?}"
        );
        for alias in DEFAULT_LOOPBACK_HOSTS {
            assert!(
                hosts.contains(&alias.to_string()),
                "{alias} must survive alongside it: {hosts:?}"
            );
        }
    }

    /// A non-loopback bind adds nothing. Handing out `192.168.1.10` because the
    /// server happens to listen there would be an allowance the operator never
    /// configured — and `allowed_hosts` exists precisely so they can.
    #[test]
    fn the_host_list_does_not_widen_for_a_non_loopback_bind() {
        let expected: Vec<String> = DEFAULT_LOOPBACK_HOSTS
            .iter()
            .map(|h| h.to_string())
            .collect();
        for addr in ["192.168.1.10:3100", "0.0.0.0:3100"] {
            assert_eq!(
                effective_allowed_hosts(None, addr.parse().unwrap()),
                expected,
                "{addr} must not add itself to the Host allow-list"
            );
        }
    }

    /// An explicit list is the operator's word, including the empty list that
    /// means "accept any Host". Mixing the defaults in would quietly narrow a
    /// setting they wrote deliberately.
    #[test]
    fn a_configured_host_list_is_not_extended_by_the_defaults() {
        let bound: SocketAddr = "127.0.0.2:3100".parse().unwrap();
        let configured = vec!["kb.example.lan".to_string()];
        assert_eq!(
            effective_allowed_hosts(Some(configured.clone()), bound),
            configured
        );
        assert!(
            effective_allowed_hosts(Some(vec![]), bound).is_empty(),
            "an empty list means accept any Host; it must survive intact"
        );
    }

    /// (codex P2 round 5 on PR #173) RFC 6454 omits the default port when it
    /// serializes an origin, so a server on port 80 is reached by a browser
    /// sending `http://127.0.0.1` — with no `:80` at all. Listing only the
    /// explicit form would 403 every same-origin request the built-in page
    /// makes. Both spellings are listed; they are the same origin.
    #[test]
    fn port_80_origins_include_the_form_a_browser_actually_sends() {
        let origins = default_allowed_origins(80);
        assert!(
            origins.contains(&"http://127.0.0.1".to_string()),
            "the RFC serialization drops :80, and that is what arrives: {origins:?}"
        );
        assert!(origins.contains(&"http://localhost".to_string()));
        assert!(origins.contains(&"http://[::1]".to_string()));
        // The explicit form is kept as well, for anything that sends it.
        assert!(origins.contains(&"http://127.0.0.1:80".to_string()));

        // Every other port keeps exactly one spelling -- 8080 is not special.
        let other = default_allowed_origins(8080);
        assert_eq!(other.len(), 3, "no extra entries off the default port");
        assert!(other.iter().all(|o| o.ends_with(":8080")));
    }

    /// (codex P2 round 5 on PR #173) The two serializers disagree about
    /// IPv4-mapped IPv6, and the disagreement was measured rather than assumed:
    /// `Ipv6Addr::to_string()` gives `::ffff:127.0.0.1`, the WHATWG URL
    /// serializer gives `::ffff:7f00:1`. Both spellings name the same address,
    /// and `NormalizedAuthority` cannot bridge them because it only strips
    /// brackets and lowercases — so both have to be on the list.
    #[test]
    fn a_mapped_loopback_bind_is_listed_in_both_spellings() {
        let forms = client_host_forms("::ffff:127.0.0.1".parse().unwrap());
        assert!(
            forms.contains(&"[::ffff:127.0.0.1]".to_string()),
            "the form Rust prints: {forms:?}"
        );
        assert!(
            forms.contains(&"[::ffff:7f00:1]".to_string()),
            "the form a browser sends: {forms:?}"
        );

        let bound: SocketAddr = "[::ffff:127.0.0.1]:3100".parse().unwrap();
        let origins = effective_allowed_origins(None, bound);
        assert!(origins.contains(&"http://[::ffff:127.0.0.1]:3100".to_string()));
        assert!(origins.contains(&"http://[::ffff:7f00:1]:3100".to_string()));
    }

    /// Addresses whose spelling is not in dispute get exactly one form, so the
    /// allow-list does not fill up with variants nobody sends.
    #[test]
    fn unambiguous_addresses_get_a_single_host_form() {
        assert_eq!(
            client_host_forms("127.0.0.1".parse().unwrap()),
            vec!["127.0.0.1".to_string()]
        );
        assert_eq!(
            client_host_forms("::1".parse().unwrap()),
            vec!["[::1]".to_string()]
        );
    }

    /// A configured list is passed through untouched, including the empty list
    /// that means "do not validate" — `run_http` warns about that rather than
    /// quietly repairing it, because silently substituting a default would make
    /// the setting untrustworthy.
    #[test]
    fn effective_origins_pass_a_configured_list_through_unchanged() {
        let bound: SocketAddr = "127.0.0.1:3100".parse().unwrap();
        let configured = vec!["https://kb.example.com".to_string()];
        assert_eq!(
            effective_allowed_origins(Some(configured.clone()), bound),
            configured,
            "a configured list replaces the default; it is not extended by it"
        );
        assert!(
            effective_allowed_origins(Some(vec![]), bound).is_empty(),
            "an explicit empty list must survive so the startup warning can fire"
        );
    }

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
    /// 同 Host header から match (groove.toml.example の document 例と整合)。
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
    /// MCP 2026-07-28 のリクエスト形。session も `initialize` も無く、
    /// 交渉に必要なものは `params._meta` に載る (SEP-2567 / SEP-2575)。
    const MODERN_BODY: &str = concat!(
        r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"_meta":{"#,
        r#""io.modelcontextprotocol/protocolVersion":"2026-07-28","#,
        r#""io.modelcontextprotocol/clientCapabilities":{}}}}"#,
    );

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

    /// **MCP 2026-07-28 のクライアントは session を張らずに直接リクエストを
    /// POST する** (SEP-2567 が session と `initialize` を廃止した)。門番は
    /// それに触ってはならない。
    ///
    /// これは BU-32 初版の契約の**反転**で、rmcp 3 への更新に伴う意図的な変更。
    /// 旧契約 (session 無し POST が initialize でなければ 422) は rmcp 1.4.0 の
    /// session 漏れを塞ぐためのもので、上流が 2.0.0 で直したため不要になり、
    /// **残すと新プロトコルのクライアントが全滅する** — 実機で 3.1.2 に対して
    /// 422 を返すことを確認した上で外した。
    #[tokio::test]
    async fn a_sessionless_post_that_is_not_initialize_is_left_alone() {
        let reached = Arc::new(AtomicBool::new(false));
        let app = gated_mcp_router(0, 256, Arc::clone(&reached));

        let resp = app.oneshot(mcp_post(OTHER_BODY)).await.unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        assert!(
            reached.load(Ordering::SeqCst),
            "a sessionless non-initialize POST is how a 2026-07-28 client calls a \
             tool; refusing it here would break every modern client"
        );
    }

    /// 上限が満杯でも、新プロトコルのリクエストは断らない。断ってよいのは
    /// **session を消費するリクエスト**だけで、stateless な呼び出しは消費しない。
    #[tokio::test]
    async fn a_full_session_limit_does_not_refuse_a_stateless_request() {
        let reached = Arc::new(AtomicBool::new(false));
        let app = gated_mcp_router(4, 4, Arc::clone(&reached));

        let resp = app.oneshot(mcp_post(MODERN_BODY)).await.unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        assert!(reached.load(Ordering::SeqCst));
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

        drop(first);
        assert!(
            admissions.try_reserve(&live, 1).await.is_some(),
            "the seat comes back when the request that held it finishes"
        );
    }

    /// 打ち切られた要求の席も同じ `Drop` で返る。ここが漏れると、上限が
    /// じわじわ縮んで最後は誰も繋げなくなる。
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

    /// (codex review round 1-3) 同時に来ても上限を超えないこと。
    ///
    /// dummy service が rmcp の insert を演じる — 応答を返す**前**に `live` を
    /// 1 増やすので、「通したがまだ `live` に現れていない」状態が実際に生じる。
    ///
    /// **検査するのは「上限を超えない」ことだけ**で、「ちょうど上限まで埋まる」
    /// ことではない。`in_flight` を数える以上、解放待ちの席を空きと見なさない
    /// 分だけ保守的に断ることがあり、それは安全な側への外れ。
    ///
    /// なお round 2-3 で問題になった「`live` を読んでから `in_flight` を読む
    /// までの隙間」は、いまは [`Admissions`] が 3 つの操作を 1 つの臨界区間に
    /// まとめているので**構造的に存在しない**。存在しない隙間は注入して
    /// 観測することもできないので、その一点を狙って mutation で殺せる test は
    /// 無い — 代わりに「隙間を作らない」ことを lock で保証している。
    /// この test が殺せるのは round 1 の形 (数を読むだけで通す) の方。
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
