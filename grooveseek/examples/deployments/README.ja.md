# デプロイメントレシピ集

groove の代表的な 3 パターンの運用例。各サブディレクトリにそのまま流用可能な
`groove.toml` / `.mcp.json` と短い README が入っている。状況に近いものを選んで、
コピー → パス調整、で動かせる。

> **English version**: [README.md](./README.md)

| シナリオ | 想定 | トランスポート | indexer マシン数 |
| --- | --- | --- | --- |
| [`personal/`](./personal/) | 単一ユーザ / 1 セッション / ローカル KB | stdio | 1 (このマシン) |
| [`nas-shared/`](./nas-shared/) | KB ファイルは NAS、index は各マシンがローカルに持つ | stdio (各マシン) | 全マシン |
| [`intranet-http/`](./intranet-http/) | 社内サーバ、複数ユーザ同時利用 | Streamable HTTP | 1 (サーバ機) |

**単一ユーザ / 複数 Claude Code セッション並行** (= 1 マシンで複数プロジェクト並行で開きたい場合)、旧 `personal-http/` レシピは v0.8.0 で廃止。代わりに同梱の service installer を使う:

```bash
groove service install --kb-path /path/to/your/kb
```

OS のネイティブ service registry (Linux systemd-user / macOS LaunchAgent / Windows Task Scheduler AT_LOGON) に手動テンプレ編集なしで登録できる。詳細は `groove service --help`。

## 選び方ガイド

```
KB の利用者は自分だけ？
├── はい → personal 系
│   ├── 同時に開く Claude Code は 1 セッションのみ？ → personal/  (stdio、daemon 不要)
│   └── 複数プロジェクト並行で同じマシンに Claude Code を立ち上げる？
│       → groove service install  (v0.8.0+ 同梱の OS service 登録機能)
│
└── いいえ
    ├── 各ユーザが自分のコピーを持つ？ → 各マシンで personal/
    │
    └── 単一の正本 (NAS or 共有ホスト) を共有
        ├── すべてのクライアントが groove serve を動かせるホストと同じ LAN？
        │   └── はい → intranet-http/  (1 サーバ : 多クライアント)
        │
        └── クライアントは stdio で済ませたい (各自で groove serve を持つのが面倒)?
            └── nas-shared/  (KB ファイルを共有し、index は各マシンが
                             ローカルに持つ — SQLite WAL はホストを跨げない)
```

## 共通の注意点

- **Embedding モデルキャッシュ**: 初回実行時に ONNX モデル (BGE-small ~130 MB / BGE-M3 ~2.3 GB) をマシンごとに DL する。`groove.toml` の `fastembed_cache_dir` キーを設定するとそのマシン上の全 groove 呼び出しでキャッシュ共有できる。キー名は小文字で、未知のキーは起動時に拒否されるため、環境変数の綴り `FASTEMBED_CACHE_DIR` をファイルに書いても効かない (環境変数**として**は正しく、ファイルの値を上書きする)。各シナリオの設定は `personal/groove.toml` / `intranet-http/groove.toml`、nas-shared は `groove.toml.client` と `groove.toml.indexer` の 2 種類を参照。
- **インデックス配置**: `.groove.db` は **`kb_path` の親ディレクトリ** に必ず作られる (例: `kb_path = /srv/kb/notes` → DB は `/srv/kb/.groove.db`)。CLI で配置先を変更するフラグは無い。ディスクレイアウトはこれを織り込む必要がある。
- **バックアップ方針**: DB は `groove index --force --kb-path <kb_path>` でいつでも再構築可能。ソースファイルが authoritative、DB は派生物として扱うこと。

## ここで扱わないこと

- **公開インターネット運用** — groove は認証機構を持たない。社内 LAN を超える場合は前段に認証 + TLS の reverse proxy が必須。
- **コンテナ / Kubernetes manifest** — 可能だが現時点で同梱していない。`intranet-http/` レシピを container 内で再利用する形で十分。サイズ見積もりは**ダウンロードサイズではなくリリース資材から**行うこと: 配布 tarball は圧縮後 ~9–11 MB だが、image layer が実際に運ぶ展開後のバイナリはその数倍ある (ONNX runtime を静的リンクしているため)。ONNX モデルキャッシュは実行時 DL でさらに大きい (BGE-small ~130 MB / BGE-M3 ~2.3 GB) ので、image に焼かず volume でマウントすること。
- **HA (高可用構成)** — groove はシングルプロセス。インデックス更新は 1 つの `Mutex<Database>` でシリアライズされるので、1 index につき 1 インスタンスで運用する。
