# デプロイメントレシピ集

kb-mcp の代表的な 3 パターンの運用例。各サブディレクトリにそのまま流用可能な
`kb-mcp.toml` / `.mcp.json` と短い README が入っている。状況に近いものを選んで、
コピー → パス調整、で動かせる。

> **English version**: [README.md](./README.md)

| シナリオ | 想定 | トランスポート | indexer マシン数 |
| --- | --- | --- | --- |
| [`personal/`](./personal/) | 単一ユーザ / 1 セッション / ローカル KB | stdio | 1 (このマシン) |
| [`nas-shared/`](./nas-shared/) | KB は NAS、複数マシンから読む | stdio (クライアント側) | 1 (専属 indexer) |
| [`intranet-http/`](./intranet-http/) | 社内サーバ、複数ユーザ同時利用 | Streamable HTTP | 1 (サーバ機) |

**単一ユーザ / 複数 Claude Code セッション並行** (= 1 マシンで複数プロジェクト並行で開きたい場合)、旧 `personal-http/` レシピは v0.8.0 で廃止。代わりに同梱の service installer を使う:

```bash
kb-mcp service install --kb-path /path/to/your/kb
```

OS のネイティブ service registry (Linux systemd-user / macOS LaunchAgent / Windows Task Scheduler AT_LOGON) に手動テンプレ編集なしで登録できる。詳細は `kb-mcp service --help`。

## 選び方ガイド

```
KB の利用者は自分だけ？
├── はい → personal 系
│   ├── 同時に開く Claude Code は 1 セッションのみ？ → personal/  (stdio、daemon 不要)
│   └── 複数プロジェクト並行で同じマシンに Claude Code を立ち上げる？
│       → kb-mcp service install  (v0.8.0+ 同梱の OS service 登録機能)
│
└── いいえ
    ├── 各ユーザが自分のコピーを持つ？ → 各マシンで personal/
    │
    └── 単一の正本 (NAS or 共有ホスト) を共有
        ├── すべてのクライアントが kb-mcp serve を動かせるホストと同じ LAN？
        │   └── はい → intranet-http/  (1 サーバ : 多クライアント)
        │
        └── クライアントは stdio で済ませたい (各自で kb-mcp serve を持つのが面倒)?
            └── nas-shared/  (KB をマウント、SQLite 制約に注意)
```

## 共通の注意点

- **Embedding モデルキャッシュ**: 初回実行時に ONNX モデル (BGE-small ~130 MB / BGE-M3 ~2.3 GB) をマシンごとに DL する。`kb-mcp.toml` の `FASTEMBED_CACHE_DIR` を設定するとそのマシン上の全 kb-mcp 呼び出しでキャッシュ共有できる — 各シナリオの `kb-mcp.toml` を参照。
- **インデックス配置**: `.kb-mcp.db` は **`kb_path` の親ディレクトリ** に必ず作られる (例: `kb_path = /srv/kb/notes` → DB は `/srv/kb/.kb-mcp.db`)。CLI で配置先を変更するフラグは無い。ディスクレイアウトはこれを織り込む必要がある。
- **バックアップ方針**: DB は `kb-mcp index --force --kb-path <kb_path>` でいつでも再構築可能。ソースファイルが authoritative、DB は派生物として扱うこと。

## ここで扱わないこと

- **公開インターネット運用** — kb-mcp は認証機構を持たない。社内 LAN を超える場合は前段に認証 + TLS の reverse proxy が必須。
- **コンテナ / Kubernetes manifest** — 静的リンクバイナリで容易に container 化可能 (~10 MB) だが、現時点で同梱していない。`intranet-http/` レシピを container 内で再利用する形で十分。
- **HA (高可用構成)** — kb-mcp はシングルプロセス。インデックス更新は 1 つの `Mutex<Database>` でシリアライズされるので、1 index につき 1 インスタンスで運用する。
