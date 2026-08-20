# デプロイメントレシピ — 社内 HTTP サーバ

> **English version**: [README.md](./README.md)

1 台のサーバが `groove serve --transport http` を動かし、index への唯一の
writer 接続を保持。同じ社内 LAN の複数クライアントマシンから Streamable HTTP
経由で MCP リクエストを受ける。

> **⚠️ 信頼境界.** groove はクライアント認証を持たない。信頼できる
> 社内 LAN からしか到達できないインタフェースのみに bind し、ポートに
> 到達できる人物 = KB 全体を読める人物、と想定すること。下記
> 「セキュリティモデル」参照。

## 想定環境

- 単一の共有 KB を持つチーム / 家庭 / 研究室
- 1 台の Linux サーバ (物理 / VM / シェルの効く NAS アプライアンス) に
  まともな disk と CPU。KB と SQLite DB はここに置く
- 複数のクライアントマシンが同じ LAN から Claude Code / Cursor で HTTP に接続
- 任意 (推奨): 前段に reverse proxy (nginx / Caddy) で TLS + アクセス制御
  (信頼できないユーザがネットワーク内にいる場合)

## このディレクトリの中身

| ファイル | 用途 |
| --- | --- |
| [`groove.toml`](./groove.toml) | サーバ側: HTTP transport / watcher on / kb_path / model |
| [`groove.service`](./groove.service) | サーバ用 systemd unit。`User=groove`、失敗時 restart |
| [`.mcp.json`](./.mcp.json) | **クライアント側**: HTTP transport でサーバ URL を指す |

## セットアップ

### サーバ側 (1 台)

1. 専用 unix ユーザ作成 (推奨): `sudo useradd -r -s /usr/sbin/nologin groove`
2. バイナリを `/usr/local/bin/groove` に (chmod 755)、KB を例えば
   `/srv/groove/knowledge-base/` に配置。`.groove.db` を書けるよう親を
   `groove` 所有に:

   ```bash
   sudo install -d -o groove -g groove /srv/groove
   sudo cp -r ./knowledge-base /srv/groove/
   sudo chown -R groove:groove /srv/groove/
   ```
3. このディレクトリの `groove.toml` を `/srv/groove/groove.toml` に置く
   (CWD 探索 — systemd unit が `WorkingDirectory=/srv/groove` を設定する)。
   `kb_path` / `model` / `[transport.http].bind` を環境に合わせる

   **他マシンから接続させるなら `[transport.http].allowed_hosts` も設定する。**
   既定は DNS rebinding 対策として loopback のみ (`localhost` / `127.0.0.1` /
   `::1`) なので、`http://kb-server.lan:3100/mcp` を叩く LAN クライアントは
   **`bind` を何にしても 403 になる**。クライアントが URL に書くホスト名 /
   アドレスをすべて列挙する:

   ```toml
   [transport.http]
   bind = "0.0.0.0:3100"
   allowed_hosts = ["kb-server.lan", "192.168.1.10"]
   ```

   このキーが無いまま非 loopback に bind すると groove は起動時に warn を出す。
   reverse proxy 経由ならクライアントが proxy に使う名前を列挙し、proxy 側で
   その Host を転送する (`proxy_set_header Host $host;`)。
4. ONNX キャッシュディレクトリを作成 (systemd unit は `ReadWritePaths=` を
   宣言するだけで作成 / chown はしない):

   ```bash
   sudo install -d -o groove -g groove /var/cache/fastembed
   ```
5. 初回インデックス (root から sudo で groove として):

   ```bash
   sudo -u groove /usr/local/bin/groove index \
       --kb-path /srv/groove/knowledge-base
   ```

   初回はモデル DL + embedding 生成で数分かかる
6. systemd unit インストール:

   ```bash
   sudo cp groove.service /etc/systemd/system/groove.service
   sudo systemctl daemon-reload
   sudo systemctl enable --now groove.service
   ```

   > 本レシピは `groove service install` (v0.8.0+) を **使わない**。あちらが
   > 登録するのは *user-level* unit (`~/.config/systemd/user/`) で、ログイン
   > セッションと共に起動し実行ユーザは自分自身になる。共有サーバに欲しいのは
   > 逆で、誰もログインしていなくても boot 時に起動し、専用 `groove` アカウント
   > で動き、`groove.service` の sandbox 指定を持つ system unit。
   > `groove service install` は個人ワークステーションの常駐 daemon 向け。
7. ヘルスチェック:

   ```bash
   curl http://127.0.0.1:3100/healthz   # → 200 OK
   ```
8. ファイアウォールで社内のみ許可。UFW 例:

   ```bash
   sudo ufw allow from 192.168.1.0/24 to any port 3100 proto tcp
   ```

### クライアント側 (各ワークステーション)

1. サーバ URL の到達性確認: `curl http://kb-server.lan:3100/healthz`
2. このディレクトリの `.mcp.json` をプロジェクトルートか
   `~/.config/claude/.mcp.json` に置く。URL をサーバアドレスに合わせて編集
3. それで終わり — クライアントには groove バイナリ不要、HTTP 対応の
   MCP クライアントだけあれば動く

## 運用上の注意

- **単一 writer**。`serve` が index への唯一の `Mutex<Database>` を保持。
  サーバ側 watcher が `kb_path` 配下の編集を拾って増分再インデックス。
  クライアントは決して書き込まない
- **並行性**。rmcp の Streamable HTTP は接続レベルでは並列だが、`search`
  呼び出しは embedder + DB の mutex でシリアライズされる。reranker off で
  CPU 次第 5-15 qps / instance 程度。スループット必要時はサーバを縦に
  スケール (CPU / 速い disk) — groove は設計上 single-process
- **KB の編集**。インデックスを最新に保つ方法 2 つ:
  - サーバ上で直接編集 (SSH / サーバ上のエディタ)。watcher が ~500 ms 内に検出
  - クライアントからサーバ上の bare repo に `git push`、post-receive hook で
    `/srv/groove/knowledge-base` 配下に `git pull`。watcher が結果のファイル
    変更を検出
- **再起動安全性**。SQLite WAL + `synchronous = NORMAL` の既定で動く。
  index 中に kill してもロストするのは現在チャンクの commit 1 件のみ。
  次の `groove index` がソースファイルから再構築する

## セキュリティモデル

groove は **クライアント認証を持たない**。Streamable HTTP は既定で
`127.0.0.1:3100` に bind するのは事故防止のため。`0.0.0.0` への bind は
opt-in、そして運用責任は利用者にある。

| 脅威 | 緩和策 |
| --- | --- |
| LAN 上での平文盗聴 (HTTP 暗号化なし) | nginx / Caddy で TLS termination、groove は loopback bind のみ |
| LAN 内の不正クライアント | reverse proxy で HTTP basic auth or mTLS、またはアクセス制御済 subnet 内に隔離 |
| 悪意ある大量リクエスト (DoS) | proxy 側のレート制限。groove 本体にレート制限機能なし |
| ブラウザからの DNS rebinding | 検証は 2 段。まず Host ヘッダを `[transport.http].allowed_hosts` と照合する (v0.5.0+、自分のホスト名を列挙するまでは loopback のみ許可)。次に Origin ヘッダを `[transport.http].allowed_origins` と照合する (v0.27.0+、既定は bind port の loopback origin)。Origin を送らない要求 (通常の MCP クライアント / `curl`) は通過する。**proxy 越しのブラウザは公開 origin を送る**ので、そこに列挙しない限りブラウザ由来のクライアントは全て 403 になる。`/healthz` は既定で Host 検証の対象外だが、`healthz_public = false` (v0.8.0+) で同じ検証下に置ける |

Web UI (`/ui`) と admin API (`/api/admin/*`) は **他マシンから到達できない**。
これらは別の check の背後にあり、Host ヘッダが allow-list に載っていても
**peer アドレスが loopback でなければ 403** になる。使う時は公開するのではなく
SSH port forward (`ssh -L 3100:127.0.0.1:3100 kb-server.lan`) 経由にする。

> **同一ホスト上の reverse proxy はこの check を無効化する。** proxy 経由だと
> groove から見た peer は proxy の loopback アドレスになり、素の
> `proxy_pass http://127.0.0.1:3100` は `Host: 127.0.0.1:3100` を送る — admin
> の allow-list は「loopback の別名 + (bind 自体が loopback の場合のみ) bind
> アドレス」なのでこれも通る。結果、両方の gate を抜けて proxy に到達できる
> 相手にこれらの route が出る。**allow-list 方式にすること**: map するのは
> `/mcp` と `/healthz` だけ。block-list は形が悪い — `/ui` と `/api/admin/*` は
> 同じ router にあり KB と daemon の状態を返すので、**塞ぎ忘れたものが
> そのまま露出する**。意図的に公開するなら、KB 本体・index 再構築・daemon
> status の前に立つのは proxy 自身の認証だけになる。

現時点で認証が必要なら標準レシピは:

```
[インターネット / VPN] → nginx (TLS + basic auth) → 127.0.0.1:3100 → groove
```

`groove.toml` で `127.0.0.1:3100` に bind し、nginx では **`/mcp` と
`/healthz` だけ** を allow-list として proxy する — 他の route (`/ui` /
`/api/admin/*`) はすべて loopback gate 付きで、同一ホストの
proxy はその gate を無効化する (上の警告を参照)。クライアントの Host を転送し (`proxy_set_header Host $host;`)、
その名前を `[transport.http].allowed_hosts` に列挙する。ブラウザ由来の
クライアントが繋ぐなら、**公開 origin を `[transport.http].allowed_origins` にも
列挙する** — ブラウザが送るのはサーバではなく proxy の origin である。

Origin の entry は host の entry と**綴りが違う**。書く前に知っておく点が 2 つある。
まず **scheme が必須**で、無い場合は無視ではなく**起動拒否**になる — 解釈できない
entry は照合前に捨てられ、全部捨てられたリストは「検証は有効・比較対象 0 件」に
なって、ログに何も出さないまま全ブラウザを拒むからである。もう 1 つ、**port の
無い entry はそのホストの全 port に一致する**。scheme の既定 port を意図する場合を
除いて port を書くこと — `https://kb.example.com` は 443 を意味し、上の TLS 終端の
構成ではそれが正しい。

### `alwaysLoad: true` (クライアント側)

サンプルの client `.mcp.json` には `"alwaysLoad": true` を入れている。これは
Claude Code v2.1.121+ のオプションで、tool-search ショートリストを介さず initial
load で groove のツールを必ず含める。RAG 用途 (常時検索可能) では推奨。重い処理は
サーバ側で行われるため、HTTP transport ではクライアント側起動コストは無視できる
レベル — 有効のままで問題ない。他 MCP クライアント (Cursor 等) は未知フィールドと
して無視する。

## 次のレシピへの移行サイン

- 認証が必須になった → 本レシピを既に超えている。手前に認証付き reverse
  proxy を立てる
- 複数地理拠点 → LAN 限定前提が崩れる。groove 現状の運用面を超える。
  rsync 系で KB をリージョンごとに複製するか、ホスティング版を待つか
