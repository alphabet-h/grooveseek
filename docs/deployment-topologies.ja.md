# デプロイ構成

GrooveSeek をどの形で走らせるか、常駐が実際に何を買っているか、そして
同一ホスト制約がどこから来るのか — それは 1 箇所ではなく 3 箇所ある。

> **English version**: [deployment-topologies.md](./deployment-topologies.md)

以下はすべて **v1.0.0** に対して 2026-08-22 に、1 台の Windows マシンで実測した。
数値を挙げた箇所には、それを出したコマンドも併記してある (自分のハードウェアで
反証できるように)。コードは**行番号ではなくファイル名と関数名**で引く —
このページの前身は行番号で引いており、**5 日で全部が嘘になった**。

## プロセスの形は 2 つ、そして 1 プロセスは必ずどちらか一方

同じ `groove` バイナリが両方の形を担うが、1 プロセスは stdio か HTTP のどちらか
であって両方にはならない。`run_server` は transport で 1 度だけ分岐し、
fallthrough は無く、その分岐のコメント自身が「2 つの arm は排他」と明記している。

### クライアントが起動する — stdio

```
  同一マシン・同一ユーザ
  ┌───────────────────────────────┐
  │  MCP クライアント             │
  │  (Claude Code / Cursor / …)   │
  │            │                  │
  │            │ spawn + stdin/stdout
  │            ▼                  │
  │  groove serve                 │
  └───────────────────────────────┘
     ソケットを 1 つも開かない
```

1 プロセス = 1 クライアントで、クライアントが生きている間だけ生きる。
同一ホストであることが**設定ではなく構造**である — 子プロセスを他のマシンに
置きようがないので、`Host` 検証も `Origin` 検証も認証も、そもそも問いとして
発生しない。

### 上げっぱなしにする — HTTP

```
  ┌──────────────────┐  ┌──────────────┐  ┌──────────────┐
  │ MCP クライアント×N│  │ ブラウザ /ui │  │ groove-tray  │
  └────────┬─────────┘  └──────┬───────┘  └──────┬───────┘
           └───────────────────┼─────────────────┘
                               ▼  TCP (既定 127.0.0.1:3100)
                      groove serve --bind …
                  モデル 1 つ・索引 1 つを共有
```

複数クライアントが 1 つのロード済みモデルと 1 つの索引を共有する。この形にだけ
上限が 2 つ掛かる: 同時 session が最大 `DEFAULT_MAX_SESSIONS` (**256**)、
リクエスト body が最大 `REQUEST_BODY_MAX_BYTES` (**1 MiB**)。body 上限に
axum の `DefaultBodyLimit` ではなく `tower_http` の層を使うのは、前者が
**自分で body を読むサービス**に効かないため — `rmcp` がまさにそれ。

### どちらの形でも成り立つ 3 つ

**ファイル監視は両方の形で動く。** watcher は transport 分岐より前、同じ関数の中で
spawn される。つまり stdio の子プロセスも常駐 daemon と同じように監視している。
**「更新監視のために daemon を上げておく」は理由にならない** — 既に動いている。
(監視は既定で on であって無条件ではない。`--no-watch` と `[watch].enabled` で切れる)

**1 プロセス = 1 モデル。** embedder は分岐より前に 1 度だけ構築され、
`Arc<Mutex<_>>` で共有される。2 つ目のプロセスは自分のぶんを持つ。したがって
常駐 daemon を上げたままエディタが子プロセスを起こすと、メモリに 2 つ載る —
**`bge-m3` ならおよそ 2 GB ずつ、既定の `bge-small-en-v1.5` なら 500 MB ずつ**
([usage.ja.md](usage.ja.md))。`bge-m3` についてよく引かれる 2.3 GB は
**ダウンロードサイズであって常駐サイズではない**。reranker を設定していれば
それが同一プロセス内のもう 1 つのモデルになる (既定では設定されていない)。

**多重起動ガードは無い。** lock file も PID file も排他 DB モードも無い。
SQLite は WAL で開かれ、`busy_timeout` が 30 秒に設定されている — これは
**別プロセスの `search` / `status` が即失敗せずに待つため**で、同時アクセスは
想定され支援されている。防がれていないのは「**同じ索引作業が 2 プロセスで
二重に走ること**」の方で、書き先は同じ `.groove.db` になる。

ガードに見えて違うものが 2 つある:

- **`RebuildSlot`** は `rebuild_index` の同時実行を断るが、**1 プロセスの中**に居る。
  daemon と子プロセスはスロットを 1 つずつ持つので、同じコーパスの再 embed が
  両方で同時に走り得る
- **bind 失敗の文言** (*"is another groove instance running, or the port occupied?"*)
  が見えるのは同一ポートの別リスナだけで、**stdio の子プロセスは見えない** —
  ここで問題になるのはまさにその形

### 常駐が何を買うのか、実測

| | モデルのロード | 1 クエリ | コーパス |
|---|---|---|---|
| `groove search` (CLI) | 毎回 | **~3,000〜3,500 ms** (中央値 ~3,150) | 135 文書 / 1,801 chunk |
| 常駐 daemon の `/mcp` `tools/call` | 起動時に 1 度 | **~140〜290 ms** (typical ~200) | 686 文書 / 9,813 chunk |

**およそ 15 倍、実測レンジのどこを取っても 10 倍を下回らない。** 両側とも
`bge-m3` で reranker 無しなので、意図的な差はコーパスサイズだけ。そしてその差は
**結論に不利な向き**に効いている — 遅い側の方が小さいコーパスで、文書数で 5 倍、
chunk 数で 5.4 倍の開きがある。

内訳は推論ではなく**対照実験**で分かる。`groove status` は同じ DB・同じ config に
対して、検索経路がやることを全部やる (プロセス起動、config 探索、DB オープン) —
**モデルのロードだけをしない**。これが **~35 ms**。つまり CLI の 3 秒のうち
**約 99% がモデルロード**であり、同じクエリの 2 回目が速くならない
(3,158 / 3,014 / 3,015 / 3,108 / 3,039 ms) のもこれで説明が付く。

**したがって PHP や Node からリクエストごとに CLI を叩く形は成立しない。**
外部アプリは必ず「既にモデルを握っているプロセス」と話すことになる。

> **「常駐」は「常に速い」ではない。** 10〜20 倍のコストになる条件を 2 つ実測した。
> どちらも上の「1 プロセス = 1 モデル」が別の顔で出てきたもの。
> **2.3 時間 idle した後の初回クエリが 4,616 ms。** CLI 検索を並走させると
> `/mcp` が ~200 ms から **~2,000 ms** に落ち、CLI を止めた途端 ~180 ms に戻った —
> CLI を走らせるたびにモデルがもう 1 つメモリに載り、idle な daemon の working set が
> トリムされ、次のクエリでフォールトして戻すため。**上の表を再現するなら 2 行を
> 別バッチで測ること。** 交互に測ると比が ~15 倍から無意味な ~1.6 倍に潰れる。

## 誰が呼ぶかが、認証が自分の問題になるかを決める

GrooveSeek は API を提供し、人が見る画面は前段のアプリが持つ。そのアプリが
**何で書かれているか**が形を決める。

### MCP クライアント

Claude Code / Cursor / VS Code は MCP を直接話す。`groove` を stdio で spawn するか、
daemon の `/mcp` に POST するかのどちらか。**daemon が既に居るなら `/mcp` を使う** —
モデルの二重ロードを避けられる。

### Node アプリ

公式 TypeScript SDK の `StdioClientTransport` が `groove` を子プロセスとして起こす。
Claude Code がやっているのと同じ形。ポートを開かないので、`Host` 検証も `Origin`
検証も認証も発生しない。**6 つの tool と `search` の 17 パラメータすべてに到達できる**
([mcp-tools.ja.md](mcp-tools.ja.md))。画面・ログイン・ユーザ単位のスコープ・監査ログは
アプリ側が持つ。

### PHP アプリ

PHP-FPM のワーカは短命なので子プロセスを抱えられない — 抱えたら**ワーカの数だけ
モデルが常駐する**。PHP アプリは常駐 daemon と話す:

```
  PHP-FPM アプリ  ──cURL──▶  http://127.0.0.1:3100/mcp
```

`/mcp` は **stateless な POST を受ける** — `tools/call` 1 本で、`initialize`
ハンドシェイクも session id も要らない (MCP 2026-07-28 / SEP-2567)。これが
「次の瞬間には居ないワーカ」から使える理由。`/ui` 自身がまさにこの形で呼んでおり、
Streamable HTTP 上の MCP クライアントとして**最小の動く実例**になっている。

アプリを別コンテナに分けること自体は**できる** — `/mcp` に loopback peer の要求は
無い。ただしそこからが本当の判断で、**判断の中身は到達性ではない**。
GrooveSeek が誰も認証しないこと、そのものである。次節と
[stability.ja.md](stability.ja.md) を参照。

## 同一ホスト制約の出どころは 1 つではなく 3 つ

[ADR-0009](decisions/0009-one-dns-rebinding-gate.ja.md) 以降、検証される経路は
すべて 1 つの middleware `dns_rebinding_gate` を通り、そこで **peer → Host → Origin**
の順に 3 つの問いが立つ (経路グループごとに別の state を渡す)。**「誰が答えるか」を
統一したことは「何を問うか」を意図的に変えていない**ので、ここでも 3 つは分けて扱う。
この 3 つを「同一ホスト制約」1 列に畳んだことが、このページの前身が `/mcp` について
間違えた原因だった。

| 経路 | peer が loopback か | `Host` | `Origin` | 設定で開けるか |
|---|---|---|---|---|
| stdio | — (ソケット無し) | — | — | 不可能 — 子プロセスだから |
| `/ui`, `/api/admin/status` | **要求する** | `allowed_admin_hosts` = loopback 別名 + bind アドレスが loopback ならそれ。config キー無し | 共有の `allowed_origins` | **不可**。peer 検査はそもそも設定できない |
| `/mcp` | 要求しない | `effective_allowed_hosts` (既定は上と同じ)。`[transport.http].allowed_hosts` で置換可 (config 専用、CLI フラグ無し) | `effective_allowed_origins` (既定は bind した port の loopback origin)。`allowed_origins` で置換可 | **可** — 下記 |
| `/healthz` | 要求しない | 既定では **gate を付けずに mount**。`healthz_public = false` の時だけ `/mcp` と同じリスト | **決して検証しない** | `healthz_public` のみ |

`/api/search` はこの表に**無い** — v0.27.0 で削除された。同名のハンドラは
テスト専用の feature gate の裏にだけ残っており、「出荷されたサーバがこれに応答
しないこと」を確かめるテストがある。

**同一ホスト制約と言えるのは peer の列だけ。** `Host` も `Origin` も呼び出し側が
名乗る値なので、これらは**ブラウザが DNS rebinding に騙されることへの防御**であって、
アクセス制御ではないし、呼び出し元がどこに居るかの証明でもない。

**Origin 検証は既定で有効。** `allowed_origins` 未設定なら、実際に bind した port の
loopback origin が既定になる。`Origin` ヘッダを持たないリクエストは RFC 6454 に従って
通るので、通常の MCP クライアントや `curl` は影響を受けない。キーを設定すると既定を
**拡張ではなく置換**する。

### これが何を意味するか

**外に出せる唯一の口が、誰も認証しない口である。**

`/ui` と `/api/admin/status` は peer 検査で閉じている — これは呼び出し側が偽造できない。
`/mcp` は**検証されている** (`Host` と `Origin` を、GrooveSeek 自身の gate が、
session gate より外側で。しかも置き換え前のライブラリより **5 つの不正な `Host` 綴りに
対して厳しい**)。ただし peer を見ないので、**ポートに到達できて `Host: localhost` を
送る相手は通る**。非 loopback への `--bind` と `--i-know` が「ポートに到達できる」を
作るのであって、**どちらのフラグも allow-list を広げてはいない**。

これは**欠陥ではなく宣言された立場**である。GrooveSeek に認証は無く、入れる予定も無い。
loopback 以外への bind を許すのは、コンテナがそうせざるを得ないから。そしてそうした
時点で、ネットワーク境界の責任はこちら側に移る。`serve` が非 loopback bind に対して
出す拒否文が、同じことを同じ言葉で述べている。
[stability.ja.md](stability.ja.md) の「GrooveSeek はどこで動く想定か」を参照。

## 決まったこと

このページは内部の草稿では、未決の問い 4 つで終わっていた。**4 つとも 1.0.0 の前に
決着している。** ここに残すのは、上に書いた形がその帰結だからである。

| 問い | 結論 |
|---|---|
| `/mcp` を同一ホストのみに閉じるか | **閉じない** — 線はコードではなく宣言で引いた。`--i-know` は残し、それが承認する拒否文の方を「何が起きるか」を述べる形に書き換えた。[stability.ja.md](stability.ja.md) |
| `Origin` を検証するか | **する。既定で有効** (v0.27.0)。さらに v1.0.0 で `Host` と `Origin` の両方をライブラリから取り上げ、全経路について GrooveSeek 自身が答えるようにした。[ADR-0009](decisions/0009-one-dns-rebinding-gate.ja.md) |
| `/api/*` を公開 API に格上げするか | **しない — `/api/search` は凍結ではなく削除**した (v0.27.0)。`search` の 17 パラメータのうち 2 つ、6 tool のうち 1 つしか通しておらず、`/mcp` の方が既に優れていた。`/api/admin/status` は tray が polling するので**意図的に unstable のまま**残す。[ADR-0008](decisions/0008-declare-what-1-0-freezes.ja.md) |
| `/ui` の位置づけ | **運用者が自分のサーバを覗く窓**。そして **1.x のうちに引退させる**予定 — `/mcp` を十分に話すクライアントが出てきた時点で。1.0 の凍結対象から外してあることが、それを minor リリースで済ませられる理由。[stability.ja.md](stability.ja.md) |

## 関連

- [clients.ja.md](clients.ja.md) — `.mcp.json` のレシピ、HTTP transport、watcher
- [stability.ja.md](stability.ja.md) — 1.0.0 が凍結するもの、および想定する動作環境
- [ARCHITECTURE.ja.md](ARCHITECTURE.ja.md) — 以上すべての背後にあるソース構成
- [ADR-0008](decisions/0008-declare-what-1-0-freezes.ja.md) — 1.0.0 が凍結するもの
- [ADR-0009](decisions/0009-one-dns-rebinding-gate.ja.md) — DNS rebinding gate を 1 つに
- [ADR-0010](decisions/0010-settle-what-the-1-0-command-line-freezes.ja.md) — ADR-0008 が残した 3 つの問い
