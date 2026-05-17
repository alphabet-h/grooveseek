# デプロイメントレシピ — 社内 HTTP サーバ

> **English version**: [README.md](./README.md)

1 台のサーバが `kb-mcp serve --transport http` を動かし、index への唯一の
writer 接続を保持。同じ社内 LAN の複数クライアントマシンから Streamable HTTP
経由で MCP リクエストを受ける。

> **⚠️ 信頼境界.** kb-mcp はクライアント認証を持たない。信頼できる
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
| [`kb-mcp.toml`](./kb-mcp.toml) | サーバ側: HTTP transport / watcher on / kb_path / model |
| [`kb-mcp.service`](./kb-mcp.service) | サーバ用 systemd unit。`User=kbmcp`、失敗時 restart |
| [`.mcp.json`](./.mcp.json) | **クライアント側**: HTTP transport でサーバ URL を指す |

## セットアップ

### サーバ側 (1 台)

1. 専用 unix ユーザ作成 (推奨): `sudo useradd -r -s /usr/sbin/nologin kbmcp`
2. バイナリを `/usr/local/bin/kb-mcp` に (chmod 755)、KB を例えば
   `/srv/kb-mcp/knowledge-base/` に配置。`.kb-mcp.db` を書けるよう親を
   `kbmcp` 所有に:

   ```bash
   sudo install -d -o kbmcp -g kbmcp /srv/kb-mcp
   sudo cp -r ./knowledge-base /srv/kb-mcp/
   sudo chown -R kbmcp:kbmcp /srv/kb-mcp/
   ```
3. このディレクトリの `kb-mcp.toml` を `/srv/kb-mcp/kb-mcp.toml` に置く
   (CWD 探索 — systemd unit が `WorkingDirectory=/srv/kb-mcp` を設定する)。
   `kb_path` / `model` / `[transport.http].bind` を環境に合わせる
4. ONNX キャッシュディレクトリを作成 (systemd unit は `ReadWritePaths=` を
   宣言するだけで作成 / chown はしない):

   ```bash
   sudo install -d -o kbmcp -g kbmcp /var/cache/fastembed
   ```
5. 初回インデックス (root から sudo で kbmcp として):

   ```bash
   sudo -u kbmcp /usr/local/bin/kb-mcp index \
       --kb-path /srv/kb-mcp/knowledge-base
   ```

   初回はモデル DL + embedding 生成で数分かかる
6. systemd unit インストール:

   ```bash
   sudo cp kb-mcp.service /etc/systemd/system/kb-mcp.service
   sudo systemctl daemon-reload
   sudo systemctl enable --now kb-mcp.service
   ```
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
3. それで終わり — クライアントには kb-mcp バイナリ不要、HTTP 対応の
   MCP クライアントだけあれば動く

## 運用上の注意

- **単一 writer**。`serve` が index への唯一の `Mutex<Database>` を保持。
  サーバ側 watcher が `kb_path` 配下の編集を拾って増分再インデックス。
  クライアントは決して書き込まない
- **並行性**。rmcp の Streamable HTTP は接続レベルでは並列だが、`search`
  呼び出しは embedder + DB の mutex でシリアライズされる。reranker off で
  CPU 次第 5-15 qps / instance 程度。スループット必要時はサーバを縦に
  スケール (CPU / 速い disk) — kb-mcp は設計上 single-process
- **KB の編集**。インデックスを最新に保つ方法 2 つ:
  - サーバ上で直接編集 (SSH / サーバ上のエディタ)。watcher が ~500 ms 内に検出
  - クライアントからサーバ上の bare repo に `git push`、post-receive hook で
    `/srv/kb-mcp/knowledge-base` 配下に `git pull`。watcher が結果のファイル
    変更を検出
- **再起動安全性**。SQLite WAL + `synchronous = NORMAL` の既定で動く。
  index 中に kill してもロストするのは現在チャンクの commit 1 件のみ。
  次の `kb-mcp index` がソースファイルから再構築する

## セキュリティモデル

kb-mcp は **クライアント認証を持たない**。Streamable HTTP は既定で
`127.0.0.1:3100` に bind するのは事故防止のため。`0.0.0.0` への bind は
opt-in、そして運用責任は利用者にある。

| 脅威 | 緩和策 |
| --- | --- |
| LAN 上での平文盗聴 (HTTP 暗号化なし) | nginx / Caddy で TLS termination、kb-mcp は loopback bind のみ |
| LAN 内の不正クライアント | reverse proxy で HTTP basic auth or mTLS、またはアクセス制御済 subnet 内に隔離 |
| 悪意ある大量リクエスト (DoS) | proxy 側のレート制限。kb-mcp 本体にレート制限機能なし |
| ブラウザからの DNS rebinding | rmcp は Host ヘッダを検証 (loopback bind 限定で既定有効)。非 loopback bind での厳格化はロードマップ上 |

現時点で認証が必要なら標準レシピは:

```
[インターネット / VPN] → nginx (TLS + basic auth) → 127.0.0.1:3100 → kb-mcp
```

`kb-mcp.toml` で `127.0.0.1:3100` に bind し、nginx で `/mcp` と `/healthz`
を `proxy_set_header Host $host` 等とともに proxy。

### `alwaysLoad: true` (クライアント側)

サンプルの client `.mcp.json` には `"alwaysLoad": true` を入れている。これは
Claude Code v2.1.121+ のオプションで、tool-search ショートリストを介さず initial
load で kb-mcp のツールを必ず含める。RAG 用途 (常時検索可能) では推奨。重い処理は
サーバ側で行われるため、HTTP transport ではクライアント側起動コストは無視できる
レベル — 有効のままで問題ない。他 MCP クライアント (Cursor 等) は未知フィールドと
して無視する。

## 次のレシピへの移行サイン

- 認証が必須になった → 本レシピを既に超えている。手前に認証付き reverse
  proxy を立てる
- 複数地理拠点 → LAN 限定前提が崩れる。kb-mcp 現状の運用面を超える。
  rsync 系で KB をリージョンごとに複製するか、ホスティング版を待つか
