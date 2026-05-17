# デプロイメントレシピ — NAS 共有 KB

> **English version**: [README.md](./README.md)

KB と SQLite インデックスを NAS (NFS / SMB / CIFS) 上に置く構成。1 台が
**専属 indexer (書き込み権限あり)**、他のマシンは同じ共有を read-only で
マウントしてローカル kb-mcp serve をマウント済パスに向ける。

> **⚠️ 最初に読むこと.** ネットワークファイルシステム上の SQLite は
> 細心の注意が必要。SQLite WAL が依存する `fcntl` byte-range lock は SMB
> では信頼できず、多くの NFS 設定では実装すらされていない。本レシピは
> 1 台のみを書き手と定めることでこれを回避する。**複数ホストから同じ
> `.kb-mcp.db` への並行書き込みは index を破壊する**。

## 想定環境

- KB が複数ワークステーションに export されている NAS 上に存在
- 1 台のワークステーション (= "indexer host") が書き込みを独占: `kb-mcp index`
  (または watcher) を動かし、`.kb-mcp.db` への書き込みは必ずこのマシンから
- 他のワークステーションは同じ共有をマウントして、ローカル `kb-mcp serve` で
  **read-only 検索**
- 全機が同一 LAN 内 (レイテンシが効く。WAN 越し NFS で SQLite は地獄)

## このディレクトリの中身

| ファイル | 用途 |
| --- | --- |
| [`kb-mcp.toml.indexer`](./kb-mcp.toml.indexer) | indexer host **専用**。watcher off (NFS/SMB の inotify は信頼できない)、cron / systemd timer で index 走行 |
| [`kb-mcp.toml.client`](./kb-mcp.toml.client) | 他の全ワークステーション用。watcher off (書かない)、reranker / quality filter は好み |
| [`.mcp.json`](./.mcp.json) | クライアント側: stdio + 共有 kb_path |

## セットアップ

### indexer host (1 台)

1. NAS 共有を **read+write** でマウント (kb-mcp 実行ユーザに書き込み権限)。
   信頼できる順:
   - NFSv4 + `noac` が SQLite には比較的安全
   - Linux クライアントの SMB は `cache=strict,actimeo=0` で動くが並行負荷下で脆い
2. `kb-mcp.toml.indexer` を `kb-mcp.toml` として discovery path のいずれかに
   コピー。`kb_path` をマウント先に書き換え
3. 初回フルインデックス (NFS 越しの read で時間がかかる):

   ```bash
   kb-mcp index --kb-path /mnt/nas/knowledge-base --force
   ```

4. cron / systemd timer で増分再構築をスケジュール。systemd 例:

   ```ini
   # /etc/systemd/system/kb-mcp-index.service
   [Service]
   Type=oneshot
   ExecStart=/usr/local/bin/kb-mcp index --kb-path /mnt/nas/knowledge-base
   User=kbmcp

   # /etc/systemd/system/kb-mcp-index.timer
   [Timer]
   OnBootSec=2min
   OnUnitActiveSec=5min  # 編集頻度に合わせて
   Unit=kb-mcp-index.service

   [Install]
   WantedBy=timers.target
   ```

5. indexer host 上で **watcher 有効の `kb-mcp serve` を動かさない** こと。
   他マシンの reader 接続が WAL を開いている最中に watcher の増分書き込みが
   走るとレースする。選択肢:
   - timer の `kb-mcp index` のみ、`serve` 自体動かさない
   - `serve` を `--no-watch` で動かし、timer に再構築を任せる

### read-only クライアント (他のマシン全部)

1. 同じ NAS 共有を **read-only** でマウント:

   ```bash
   # Linux NFSv4 の例
   sudo mount -t nfs4 -o ro,noac nas:/exports/kb /mnt/nas/knowledge-base
   ```

   **read-only マウントが重要** — クライアントマシンで誤って `kb-mcp index`
   を打っても indexer の DB を壊さない
2. `kb-mcp.toml.client` を `kb-mcp.toml` として discovery path に置き、`kb_path`
   をマウント先に
3. `.mcp.json` をプロジェクトルート (or Claude Code が読む場所) に配置
4. read-only 動作確認:

   ```bash
   kb-mcp status --kb-path /mnt/nas/knowledge-base
   ```

   document 数が表示されれば OK (何も書かずに済む)

## 運用上の注意

- **DB も NAS 上**。`.kb-mcp.db` は `/mnt/nas/.kb-mcp.db` (kb_path の親) に
  作られる。全 reader が同じファイルを見る。1 ホストからの書き込みが無い間
  SQLite は read-only で開けるので並行検索は安全だが、複数ホストからの
  並行書き込みは安全ではない
- **クライアントの watcher は OFF**。Linux の inotify / Windows の
  ReadDirectoryChangesW はネットワーク FS では伝播しない。watcher は静かに
  イベントを取りこぼす。indexer 側の timer に任せる
- **indexer 側の watcher も OFF** (本レシピ) — リモート reader が検索中に
  watcher の増分書き込みが走るのは SQLite WAL がネットワーク FS で想定して
  いない負荷。編集量が少なく NAS が速ければ indexer のみ
  `[watch].enabled = true` を試す価値あり (再構築中はリモート reader を
  止めてから)
- **モデル DL** は依然として各マシン独立。`FASTEMBED_CACHE_DIR` を共有 path に
  向けるなら **そのマシンのローカル disk** に限る。NAS 上に向けると初回 load が
  極端に遅くなり、ホスト間で embedder ロードがシリアライズされる
- **`alwaysLoad: true`** はサンプル `.mcp.json` に入れている Claude Code v2.1.121+
  のオプション。initial load で kb-mcp のツールを必ず含める。RAG 用途では便利だが、
  NAS マウントの KB は初回起動コスト (slow disk + 初回モデル DL) が大きくなりがち
  なので、起動レイテンシを優先したい場合は外す選択肢もある。他 MCP クライアントは
  未知フィールドとして無視

## 次のレシピへの移行サイン

- 「1 writer, 多 reader」前提が崩れる (誰でも編集 + 再インデックスしたい)
  → [`intranet-http/`](../intranet-http/) — 1 HTTP サーバが唯一の writer
  接続を持ち、クライアントはネットワーク越しに query する形に移行
- `database is locked` / `database disk image is malformed` が出始めた
  → ネットワーク FS の lock 意味論が破綻している。データを失う前に
  [`intranet-http/`](../intranet-http/) へ移行
