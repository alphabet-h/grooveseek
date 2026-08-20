# 9. DNS rebinding の門番を 1 つにし、こちらが持つ

日付: 2026-08-20

> **English**: [0009-one-dns-rebinding-gate.md](0009-one-dns-rebinding-gate.md)

## Status

Accepted

## Context

GrooveSeek は HTTP リクエストごとに 2 つを問う — この `Host` は許可されているか、
この `Origin` は許可されているか。後者は MCP 仕様が Streamable HTTP サーバに
要求するもので、2 つ合わせて loopback 常駐デーモンの DNS rebinding 対策の定番である。

これまで、その 2 つの問いに **4 つの実装**が答えていた:

| | `/mcp` | `/healthz` | `/ui` / `/api/admin/status` |
| --- | --- | --- | --- |
| Host | rmcp `host_is_allowed` | `healthz_host_check` | `admin_host_check` |
| Origin | rmcp `origin_is_allowed` | — | `admin_origin_check` |

この配置は意図的で、2 度擁護もしている — **リストを共有し、ロジックは mirror し、
一致はレビューで保つ**。[#173](https://github.com/alphabet-h/grooveseek/pull/173)
で `DEFAULT_LOOPBACK_HOSTS` を唯一の定義にしたのは、まさに「どの面にも違う名前集合が
渡らない」ためだったし、[#193](https://github.com/alphabet-h/grooveseek/pull/193)
では実サーバ越しに 2 面へ同じ問いを投げる一致テーブルを足した。

**しかし実測すると、mirror は一致していなかった。** 同一の `effective_hosts` を
渡した状態で、26 通りの `Host` を `/mcp` と `/healthz` に送った結果:

```text
Host                        /mcp (rmcp)   /healthz (groove)
user:pw@127.0.0.1:PORT      200           400
@127.0.0.1:PORT             200           400
127.0.0.1@localhost         200           400
127.0.0.1:65536             200           400
localhost:abc               200           400
```

食い違い 5 件、**すべて同じ向き** — `validate_host_header` が userinfo と
`u16` 範囲外 port に defensive reject を足しているので、groove が厳しい。
**rmcp が拒否して groove が通す綴りは 1 件も無かった。**

拒否本文も違った: `/mcp` と `/healthz` は `Forbidden: Host header is not allowed`、
admin は同じ文から接頭辞を落としたもの。さらに **groove 自身の 2 つの Host 検査も
同一ではなかった** — `healthz_host_check` は HTTP/2 の `:authority` fallback を
読み、`admin_host_check` は読まない。

#193 のコードレビューはこの一般形を 2 度指摘し、`Origin` matcher の統合を求めた。
調査して分かったのはより有用な事実で、**実在する食い違いは `Host` 側**だった —
`Origin` だけ統合すれば、**一致している方を直して、していない方を残す**ことになる。

## Decision

**GrooveSeek が配信する全 route (`/mcp` を含む) の `Host` と `Origin` を、
前段に置いた 1 つの門番で検証する。**

rmcp 側の検査は**明示的に**止める — `with_allowed_hosts(vec![])` /
`with_allowed_origins(vec![])` は上流で「全 Host 許可」「Origin を検証しない」を
意味する。**呼び出しを省くのではなく空リストを渡す**のが決定の一部で、
`StreamableHttpServerConfig::default()` は loopback 限定なので、省くと
**綴りの違う 2 つ目の検査が武装したまま残る**。

門番は middleware 1 枚で、route 群ごとに違うリストを与える:

- `/mcp` — effective な `allowed_hosts` と `allowed_origins`
- `/healthz` — 同じ `Host` リスト、`Origin` リストは無し。`/healthz` は元々
  `Origin` を検証していないし、本決定は**問いの答え手を変えるもの**であって
  新しい問いを増やすものではない
- `/ui` / `/api/admin/status` — admin 用 `Host` リスト (loopback + bind
  アドレス、設定不可)、同じ `Origin` リスト、加えて **peer が loopback であること**

門番内の順序は peer → `Host` → `Origin`。rmcp の順序をそのまま保つので、
複数の検査に落ちるリクエストは**どの面でも同じ理由で拒否される**。

拒否の文面は rmcp のものを逐語で維持し、`/mcp` のクライアントから見て変化を出さない。
admin は欠けていた `Forbidden: ` / `Bad Request: ` 接頭辞を得る。

## Consequences

**`/mcp` が 5 つの不正な `Host` 綴りに対して厳しくなる** (200 → 400)。
`docs/stability.md` が凍結しているのは「`/mcp` が存在する」「健康なら `/healthz` が
`200` を返す」だけで、**どの不正綴りを許容するかは凍結していない**。5 件はいずれも
ブラウザや MCP クライアントが組み立てない `Host` である。向きは安全側 —
**拒否が増えることはあっても減ることはない**。これが採用可能にした理由そのもの。

**拒否されたリクエストが session 上限に触れなくなる。** rmcp は自身の `handle()`
内で検証しており、そこは session gate の後ろなので、拒否される `initialize` が
**席を予約してから**返っていた。`max_sessions = 1` で実測: 外部 `Host` の応答は
`429` から `403` に変わった。

**拒否ログが `/mcp` でも有界になる。** rmcp は拒否ごとに `warn!` を無制限に書いて
いた。門番は #190 以来 session gate が使っている「1 分 1 行 + 見送り件数」の予算を
**面ごとに**持つ。

**`/mcp` の DNS rebinding 防御を GrooveSeek が持つことになる。** rmcp が将来
検査を強化しても**継承しない** — 問い直す場所は `docs/decisions` であり、
面がずれ始めたら気付くのは一致テストである。逆に、rmcp の parse が変わっても
**`/mcp` だけを動かすことはできなくなる**。独立に動かせるものが残っていないため。

**門番の配線を切れば `/mcp` は無防備になる。** ここで取ったのはこのリスクで、
だからテストがこの形をしている — `tests/dns_rebinding.rs` は 5 綴りを**面の一致
ではなく値で**assert する。一致だけなら「全 route を rmcp に戻す」でも満たせるので、
**`validate_host_header` だけが出す拒否**を「どの実装が答えているか」の指紋に使う。
実測: `/mcp` を rmcp に戻すと、同ファイルの 5 本中 4 本が落ちる。

**2 つの問いに 2 実装が残る (1 つずつではない)。** rmcp は自前の実装を依存の中に
持ち続ける。得たのは「**到達できるのは片方だけ**」であり、生きたリクエストについて
2 つが食い違うことはもう起きない。

## Alternatives considered

**現状維持し、5 件の食い違いを文書化する。** 最も安く、仕様の実装本体を経路に残せる。
却下した理由は、**その食い違いは誰も選んでいない**から — 2 つの parser がそうなって
いただけで、最初の 1 つが書かれてから何リリースも後に、読んでではなく測って見つかった。
書き留めることは**事故を固定すること**になる。

**`Origin` だけ統合する** (レビューの要求)。実測で却下 — `Origin` matcher は
検査した 21 綴りすべてで一致し、`Host` は一致していなかった。観測された食い違いを
残したまま、しかも `/mcp` は Origin→Host、admin は Host→Origin という**逆順**が
生まれる。

**門番を前に置いた上で rmcp も武装させたまま**にする。防御は厳密に増え、検査を 1 つも
止めない。却下した理由は、**答えうる実装の数が減らない**こと、そして「外側が実測で
どこでも厳しい」は上流の変更を跨いで保証にならないこと — **置き換えたかった構図と
同じ弱さを、部品を増やして抱える**ことになる。

**matcher を上流に提案する**。両者が本当に 1 実装を共有できる、長期的には正しい答えで、
本決定と矛盾もしない。ただし他プロジェクトのレビューとリリース周期に依存し、
**1.0 はそれを待たない**。
