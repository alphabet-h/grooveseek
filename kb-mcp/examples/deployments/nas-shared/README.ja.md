# デプロイメントレシピ — NAS 上のナレッジベース

> **English version**: [README.md](./README.md)

ナレッジベース本体は NAS (NFS / SMB / CIFS) に置いて全員が同じファイルを
編集する。**インデックスは置かない。** 各マシンが自分のローカルディスクに
`.kb-mcp.db` を持ち、共有ファイルを自分で index する。

> **⚠️ なぜインデックスをローカルに置くのか。** SQLite の WAL モードは
> この点を明言している ([SQLite docs, WAL](https://www.sqlite.org/wal.html)):
>
> > All processes using a database must be on the same host computer; WAL
> > does not work over a network filesystem. This is because WAL requires
> > all processes to share a small amount of memory and processes on
> > separate host machines obviously cannot share memory with each other.
>
> kb-mcp は全接続を WAL モードで開くため、`.kb-mcp.db` を共有に置いて
> 複数マシンから開く構成は、マウントオプションでも「書き手は 1 台だけ」
> という運用規約でも安全にできない — **reader も共有メモリのプロトコルに
> 参加する**から。本レシピは以前まさにそれを勧めていた。ここで訂正する。
>
> マシンごとではなく **1 つの** インデックスが欲しいなら、それが
> [`intranet-http/`](../intranet-http/) の役割: 1 台が自分のディスク上で
> DB を所有し、検索を HTTP で提供する。

## 想定環境

- KB は NAS 上のディレクトリで、複数ワークステーションに export されている
- 各ワークステーションはそのファイル群の **自分専用ローカルインデックス**
  を検索する。embedding のコストとディスクはマシンごとに払う (典型的な KB
  で数百 MB + ONNX モデルキャッシュ)
- 全機が同一 LAN 内。WAN 越しマウントは index が遅くなるだけで、**正しさが
  ネットワーク FS のロックに依存しなくなる**点は変わらない

## このディレクトリの中身

| ファイル | 用途 |
| --- | --- |
| [`kb-mcp.toml.client`](./kb-mcp.toml.client) | 各マシン用の設定。watcher off (ネットワーク FS は inotify / ReadDirectoryChangesW を配送しない)、index はタイマー駆動 |
| [`kb-mcp.toml.indexer`](./kb-mcp.toml.indexer) | KB をローカルでも編集するマシン向けに watcher の判断材料を書き足した版 |
| [`.mcp.json`](./.mcp.json) | クライアント側: stdio。`--config` でこのマシンの設定に固定 |

## セットアップ (全マシンで実施)

1. **共有の「親ディレクトリがローカルディスクになる位置」にマウントする。**
   これが肝: `.kb-mcp.db` は `kb_path` の **親** に作られるので、
   `/var/lib/kb-mcp/knowledge-base` にマウントすれば DB は
   `/var/lib/kb-mcp/.kb-mcp.db` = ローカル・専有・他ホストが触らない場所になる。

   ```bash
   # .kb-mcp.db と WAL の sidecar が作られるのは親ディレクトリなので、
   # kb-mcp を実行するアカウントが書ける必要がある。ONNX モデルキャッシュも
   # その隣に置く。マウントポイント自体も事前に作る — `mount` は作らない。
   sudo install -d -o "$(id -un)" -g "$(id -gn)" /var/lib/kb-mcp
   sudo install -d -o "$(id -un)" -g "$(id -gn)" /var/lib/kb-mcp/fastembed
   sudo install -d /var/lib/kb-mcp/knowledge-base

   # Linux NFSv4 の例。ここでは read-only で構わない — NAS 上にあるのは
   # KB ファイルだけで、kb-mcp はそれらを書き換えない。
   sudo mount -t nfs4 -o ro nas:/exports/kb /var/lib/kb-mcp/knowledge-base
   ```

   **永続化すること**。しないと、いつかタイマーが空ディレクトリに対して走り、
   indexer から見れば「ファイルが全部消えた」ので **ローカル DB の全 document が
   prune される**:

   ```
   # /etc/fstab
   nas:/exports/kb  /var/lib/kb-mcp/knowledge-base  nfs4  ro,_netdev  0  0
   ```

   read-only マウントは任意だが害はなく、「このマシンは共有 KB を編集
   しない」を強制できる。KB を編集するマシンは通常どおり read-write で。

2. `kb-mcp.toml.client` を `/var/lib/kb-mcp/kb-mcp.toml` に置き、
   `kb_path = "/var/lib/kb-mcp/knowledge-base"` にする。DB の隣に置いておくと
   下のタイマーから `--config` で直接指せる — model は index 時と一致している
   必要があり、systemd unit はシェルの作業ディレクトリを引き継がない

3. インデックス構築 (初回は数分 — NFS 読みはローカルより遅く、ONNX モデルの
   初回 DL もある):

   ```bash
   kb-mcp index --config /var/lib/kb-mcp/kb-mcp.toml
   ```

4. タイマーで最新に保つ。watcher は使えない: inotify も
   ReadDirectoryChangesW もネットワーク FS 越しには伝播せず、リモート編集を
   **無言で取りこぼす**。

   **user unit** にするので `User=` の置換も root も要らず、手順 1 で作った
   ディレクトリの所有者と自然に一致する:

   ```ini
   # ~/.config/systemd/user/kb-mcp-index.service
   [Unit]
   # 同じ事故への二重の備え: 共有が mount されていなければ (NAS 停止 /
   # ネットワーク未起動) この run を skip する。空ディレクトリを index して
   # DB を prune させない。
   ConditionPathIsMountPoint=/var/lib/kb-mcp/knowledge-base

   [Service]
   Type=oneshot
   # --config は必須: unit は作業ディレクトリを引き継がないため、config 探索が
   # 既定値に落ちて別 model で index しようとし、既存 index に弾かれる。
   ExecStart=/usr/local/bin/kb-mcp index --config /var/lib/kb-mcp/kb-mcp.toml

   # ~/.config/systemd/user/kb-mcp-index.timer
   [Timer]
   OnBootSec=2min
   # 編集頻度に合わせる。systemd に行末コメントは無く、値の後ろの `#` 以降も
   # 値の一部として解釈されて設定ごと捨てられる。
   OnUnitActiveSec=5min

   [Install]
   WantedBy=timers.target
   ```

   ```bash
   systemctl --user daemon-reload
   systemctl --user enable --now kb-mcp-index.timer
   # ログアウト中もタイマーを動かしたい場合のみ:
   sudo loginctl enable-linger "$(id -un)"
   ```

   再 index は増分 (SHA-256 の content diff) なので、変更が無ければ
   ディレクトリ走査 + ファイルごとの hash だけで済む。

5. `.mcp.json` をプロジェクトルート (or MCP クライアントが読む場所) に置く。
   タイマーと同じ理由で `--config /var/lib/kb-mcp/kb-mcp.toml` を渡している:
   クライアントは kb-mcp を **自分のプロジェクト** ディレクトリから起動するので
   探索ではそのファイルに辿り着けず、`bge-m3` の index に対して既定 model で
   起動してしまう

6. 確認:

   ```bash
   kb-mcp status --config /var/lib/kb-mcp/kb-mcp.toml
   ```

   document 数が出れば OK。`unable to open database file` が出るなら
   `kb_path` の **親** が書き込み不可 — DB と WAL の sidecar はそこに作られる。

## 運用上の注意

- **全マシンが同じ内容を embedding する**。ネットワーク越しに DB を共有
  しないことの代償。数千ドキュメントの KB で初回は各マシン数分、以降の
  タイマー実行は数秒。これが許容できないなら
  [`intranet-http/`](../intranet-http/) にして 1 回だけ払う
- **インデックスは最大でタイマー間隔分だけ古い**。マシンごとにその窓の
  どこにいるかも違う。壊れはせず、検索結果が少し古くなるだけ
- **モデルキャッシュもマシンごと**。`FASTEMBED_CACHE_DIR` は **ローカル**
  パスにする。NAS を指すとモデルロードが遅くなり、ホスト間で直列化する
- **設定の食い違いは黙って許されない**。あるマシンが
  `bge-small-en-v1.5` で index した DB を別マシンが `bge-m3` 前提で開くと
  `index_meta` チェックが起動時に弾く。DB がマシン専有になった今、これが
  効くのは DB をコピーして回した場合だけ
- **`alwaysLoad: true`** はサンプル `.mcp.json` の Claude Code v2.1.121+
  オプションで、initial load で kb-mcp のツールを必ず含める。RAG 用途で
  有用。ここでは初回起動コストが personal レシピより大きい (NFS 読み +
  モデル初回 DL) ので、起動レイテンシを優先するなら外す。他 MCP
  クライアントは無視する

## 別のレシピへ移るとき

- 全マシンで embedding コストを払いたくない / 全員が同じ 1 つの
  インデックスを見たい → [`intranet-http/`](../intranet-http/)
- `.kb-mcp.db` を共有に戻したくなった → 冒頭の SQLite の引用を読み直して、
  [`intranet-http/`](../intranet-http/) へ
- 結局 1 台しか使わない → [`personal/`](../personal/)
