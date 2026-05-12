# kb-mcp

Markdown / プレーンテキストのナレッジベースに対するセマンティック検索を提供する MCP サーバ。

YAML frontmatter 付きの Markdown (および任意で `.txt`) をパースし、見出し単位でチャンク化、選択可能な埋め込みモデル (既定は BGE-small-en-v1.5、多言語 / 日本語向けには BGE-M3) でベクトルを生成して、sqlite-vec 搭載の SQLite に格納する。stdio (既定、1 クライアント) または Streamable HTTP (複数クライアント) トランスポート経由で Claude Code / Cursor など MCP 互換クライアントに接続する。

ライブ同期ファイルウォッチャにより、手動編集・`git pull`・外部スクリプトによる変更でもインデックスが最新に保たれる。`kb-mcp validate` で任意の TOML スキーマに基づく frontmatter 検証も可能。

> **English version**: [README.md](./README.md)

## インストール

### ビルド済みバイナリ (非 Rust ユーザ向け推奨)

[最新リリース](https://github.com/alphabet-h/kb-mcp/releases/latest) から自分の OS / アーキテクチャ用のアーカイブを DL → 展開 → `kb-mcp` (Windows では `kb-mcp.exe`) を `PATH` の通った場所に配置するだけ。対応ターゲット:

| プラットフォーム | アーカイブ |
| --- | --- |
| Linux x86_64 (glibc 2.38+ / Ubuntu 24.04+ / Debian 13+ / RHEL 9.5+) | `kb-mcp-x86_64-unknown-linux-gnu.tar.xz` |
| Linux aarch64 (glibc 2.38+) | `kb-mcp-aarch64-unknown-linux-gnu.tar.xz` |
| macOS Apple Silicon | `kb-mcp-aarch64-apple-darwin.tar.xz` |
| Windows x86_64 (Windows 10+) | `kb-mcp-x86_64-pc-windows-msvc.zip` |

> **Intel Mac (`x86_64-apple-darwin`)** はビルド済バイナリを配布していない: 上流 ONNX Runtime crate (`ort-sys`) がこのターゲット用 prebuilt を提供しないため。下記「ソースからビルド」を参照。

各アーカイブにはバイナリの他に `CHANGELOG.md` / `LICENSE-MIT` / `LICENSE-APACHE` / `README.md` が同梱される。実行前にリリースに添付された `sha256.sum` または各アーカイブ用 `*.sha256` で SHA-256 チェックサムを照合すること。

ONNX runtime と SQLite はバイナリに静的リンクされているので、追加 DLL は不要。Embedding モデル (ONNX) は初回実行時に HuggingFace から DL される — ネットワークがそれをブロックする場合は [HuggingFace の TLS 失敗への対処](#huggingface-の-tls-失敗への対処) を参照。

### ソースからビルド

```bash
cargo build --release
```

バイナリは `target/release/kb-mcp` (Windows では `kb-mcp.exe`) に生成される。

## 設定ファイル (任意)

以下の CLI オプションはすべて `kb-mcp.toml` で既定値を与えられる。CLI 引数は常に優先され、設定ファイルは単に同じデプロイでの記述の繰り返しを減らすためのもの。配置場所の探索順は [設定ファイルの探索順](#設定ファイルの探索順) を参照 — 最も一般的なのはプロジェクトルート (CWD) かバイナリの隣。`kb-mcp.toml.example` を `kb-mcp.toml` にコピーして編集する:

```toml
# kb-mcp.toml (プロジェクトルート / .git 祖先 / kb-mcp の隣 のいずれかに置く)
kb_path = "/path/to/knowledge-base"
model = "bge-m3"
reranker = "bge-v2-m3"
rerank_by_default = true
fastembed_cache_dir = "/home/you/.cache/huggingface/hub"

# チャンキング時に除外する見出し部分文字列。省略すると除外なし
# (既定は空リスト)。いずれかを substring として含む見出しのセクションは
# 本文ごとチャンク化対象から外される。
exclude_headings = ["次の深堀り候補", "参考リンク"]

# インデックス走査時にスキップするディレクトリ basename (完全一致)。
# 省略時の既定は [".obsidian", ".git", "node_modules", "target",
# ".vscode", ".idea"]。ユーザ指定は既定を置き換える (merge ではない)。
# `[]` を明示すると全ディレクトリを走査する。
# exclude_dirs = [".obsidian", ".git", "node_modules", "target", ".vscode", ".idea", "dist", ".next"]

# チャンク単位の品質フィルタ。既定で有効、閾値 0.3。
# `enabled = false` で 従来挙動 (全チャンク返却) に戻せる。
[quality_filter]
enabled = true
threshold = 0.3

# index 対象拡張子。セクション省略で デフォルト挙動
# (.md のみ)。明示リストで .txt にオプトイン。空配列 [] は「何もインデッ
# クスされない」事故を防ぐため拒否される。現在サポート id: "md" / "txt"。
[parsers]
enabled = ["md", "txt"]

# ライブ同期ファイルウォッチャ。`kb-mcp serve` 実行中、
# kb_path 配下の変更が `debounce_ms` 窓内に検出され、該当ファイルのみ
# 増分再インデックスされる。PostToolUse hook を補完する位置付け:
# 手動編集 / `git pull` / 外部スクリプトをカバーする。CLI の
# `--no-watch` / `--debounce-ms` で上書き可能。セクション省略時は
# 既定 (enabled, 500ms debounce)。
[watch]
enabled = true
debounce_ms = 500

# `kb-mcp serve` のトランスポート。`kind = "stdio"` (既定)
# は 1 クライアント / サーバプロセス。`kind = "http"` (Streamable HTTP)
# なら `/mcp` で複数クライアント同時接続が可能。`/healthz` は 200 OK を
# 返しヘルスチェックに使える。CLI `--transport http --port 3100` で
# 上書き可能。
[transport]
kind = "http"

[transport.http]
bind = "127.0.0.1:3100"
# allowed_hosts = ["kb.example.lan", "192.168.1.10"]  # LAN 公開時に明示 (v0.5.0+)

# 任意: `kb-mcp eval` (retrieval 品質評価、パワーユーザ機能)。
# モデル比較や回帰追跡のために `kb-mcp eval` を使うときだけ必要。
# セクション全体を省略するとすべて既定値で動作する。
# [eval]
# golden = ".kb-mcp-eval.yml"             # 既定: <kb_path>/.kb-mcp-eval.yml
# history_size = 10                       # 既定: 10
# k_values = [1, 5, 10]                   # 既定: [1, 5, 10]
# regression_threshold = 0.05             # 既定: 0.05

# 任意: `search` ツールのチューニング (v0.3.0+)。省略時は既定値で動作する。
# [search]
# # rank-based low_confidence 判定: top1.score / mean(top-N.score) <
# # min_confidence_ratio で flag が立つ。0.0 で判定無効。CLI
# # `--min-confidence-ratio` / MCP param `min_confidence_ratio` で per-query 上書き可。
# min_confidence_ratio = 1.5

# 任意: MMR (Maximal Marginal Relevance) 多様性再ランク (v0.7.0+)。既定 off。
# 適用順序は reranker の後、parent retriever の前。
# [search.mmr]
# enabled = false
# lambda = 0.7              # 1.0 = 多様性なし (MMR off 相当); 0.5 未満で探索寄り
# same_doc_penalty = 0.0    # > 0 で同一 document chunk を更に減点; 0 = 純 MMR

# 任意: parent retriever (v0.7.0+)。既定 off。
# ヒットしたチャンクが短い場合に隣接 sibling やドキュメント全体に展開して
# LLM へ十分な context を渡す。score / 順位は変わらず content だけ拡張される。
# [search.parent_retriever]
# enabled = false
# whole_doc_threshold_tokens = 100   # token_count がこの未満なら whole-doc fallback
# max_expanded_tokens = 2000         # adjacent merge / whole-doc の上限 (BGE-M3 <= 8192)
```

この設定ファイルを置けば `kb-mcp serve` / `index` / `status` / `graph` / `search` のどれも対応フラグを省略して動かせる。未知のキーはタイポ対策のため拒否される。`FASTEMBED_CACHE_DIR` の実環境変数は設定ファイルの同項目より優先される。

### 設定ファイルの探索順

`kb-mcp` は起動のたびに以下の順序で `kb-mcp.toml` を探し、最初に見つかった
ものだけを使う:

| 優先 | 場所                                       | 備考                                                     |
| ---- | ------------------------------------------ | -------------------------------------------------------- |
| 1    | `--config <PATH>` (全 subcommand 共通)     | 指定したファイルが無ければエラー終了 (フォールバック禁止) |
| 2    | `./kb-mcp.toml` (CWD 直下)                 | プロジェクトローカル KB に最適                           |
| 3    | `<git-root>/kb-mcp.toml` (祖先方向に探索)  | CWD + 最大 19 祖先 (合計 20 ディレクトリ) を確認        |
| 4    | `<binary-dir>/kb-mcp.toml`                 | 後方互換 / グローバル install 用フォールバック            |
| 5    | (なし — 組み込み既定値)                    | この場合 `--kb-path` を CLI で必ず指定する必要あり        |

`--config` に渡した `~` は全プラットフォームで home に展開する (`~` を展開
しない Windows `cmd.exe` でも動く)。

起動時に stderr へ `kb_mcp::config: loaded config source=...` が出るので、
どの toml が実際に効いているかはログで確認できる。

#### 例: プロジェクトに同梱する per-project KB

```jsonc
// repo-root/.mcp.json
{
  "mcpServers": {
    "kb": { "command": "kb-mcp", "args": ["serve"] }
  }
}
```

`kb-mcp.toml` を `.mcp.json` の隣にコミットしておけば、Claude Code が
プロジェクトを開いた時点で `kb-mcp serve` がリポジトリルートから起動し、
CWD 探索でその `kb-mcp.toml` を拾う。`.mcp.json` 側に引数を書く必要が
無くなる。

#### 例: 1 セッションで複数 KB を併用

```jsonc
{
  "mcpServers": {
    "kb-personal": { "command": "kb-mcp", "args": ["serve", "--config", "~/kb/personal/kb-mcp.toml"] },
    "kb-project":  { "command": "kb-mcp", "args": ["serve", "--config", "./kb-mcp.toml"] },
    "kb-rust-docs":{ "command": "kb-mcp", "args": ["serve", "--config", "~/kb/rust-docs/kb-mcp.toml"] }
  }
}
```

各エントリは独立した MCP サーバとして動き、それぞれ自分の `kb-mcp.toml` と
`.kb-mcp.db` を持つ。Claude からは MCP サーバ名で source を識別できる。

## 使い方

### 検索インデックスの構築 / 再構築

```bash
kb-mcp index --kb-path /path/to/knowledge-base
kb-mcp index --kb-path /path/to/knowledge-base --force   # 完全再インデックス
kb-mcp index --kb-path /path/to/knowledge-base --model bge-m3 --force  # BGE-M3 (1024 dim、多言語) に切替
```

指定ディレクトリ配下のソースファイルを走査し、既定の `exclude_dirs` セット (`.obsidian` / `.git` / `node_modules` / `target` / `.vscode` / `.idea` — 後述「ディレクトリ除外」参照) をスキップする。既定では `.md` のみ取り込み。`kb-mcp.toml` に `[parsers].enabled = ["md", "txt"]` を追加すると `.txt` もインデックス対象になる (タイトルはファイル名から派生: `deep-dive-2026.txt` → `"deep dive 2026"`、本文全体が 1 チャンク)。前回実行時と content hash が変わっていないファイルは `--force` を渡さない限りスキップされる。

`--model` が受け付ける値:
- `bge-small-en-v1.5` (既定) — 384 次元、英語特化、初回 DL 約 130 MB
- `bge-m3` — 1024 次元、多言語 (100+ 言語、日本語含む)、初回 DL 約 2.3 GB。日本語主体の KB ではこちら推奨

既存インデックスでのモデル切替には `--force` が必須 (DB の `index_meta` テーブルにモデル / 次元が記録されており、不一致時は起動が拒否される)。

#### 進捗出力フラグ (v0.7.8+)

`kb-mcp index` の進捗表示を切り替える 2 フラグ。**相互排他** + フラグなしの既定動作は不変 (= 既存の per-file `  indexed: foo.md (N chunks)` 出力をそのまま維持)。

- `--quiet`: 各ファイルごとの出力を抑止し、開始 / `Found N source files` / `Done in ...` のサマリ 3 行のみ。harness (Claude Code Bash tool 等) では子 process の streaming 出力が exit まで集約 buffer されるため、`--quiet` で「無音 = 進行中」と認識可能。ハングと進行中の混同を防ぐ。
- `--progress`: 進捗 UI を表示。stderr の `IsTerminal` で自動分岐 — TTY なら `indicatif` バー (経過時間 / 件数 / % / ETA)、非 TTY (pipe / redirect) なら `Progress: N/M (P%)` 行を約 20 回 + 100% アンカー 1 回で flush。`tail -f indexing.log` で監視可能。

```bash
kb-mcp index --kb-path ./big-kb --quiet         # 完了まで silence
kb-mcp index --kb-path ./big-kb --progress      # TTY ではバー、pipe では定期行
```

#### モデル選択のトレードオフ

| 観点 | BGE-small-en-v1.5 | BGE-M3 |
|---|---|---|
| 初回 DL | 約 130 MB | 約 2.3 GB |
| 埋め込み次元 | 384 | 1024 (index ファイルが約 2.6 倍) |
| 実行時 RAM | 約 500 MB | 約 2 GB |
| index ビルド時間 | baseline | CPU 推論で約 3–10 倍遅い |
| 日本語精度 | 低い (英語中心語彙) | 強い (多言語 tokenizer + 訓練) |
| 英語精度 | 強い | 同等 |

モデル切替コスト (既存 index → 新モデル):

1. `kb-mcp index --kb-path ... --model <new> --force` で完全再 embedding (増分更新不可: `documents`/`chunks`/`vec_chunks` を全削除してやり直す)
2. 以降の `serve` / `index` はすべて同じ `--model` を渡す (または `kb-mcp.toml` に書く)。不一致は `index_meta` チェックで起動拒否

実務的な推奨: 最初に KB の**主要言語**に合うモデルを選び、具体的な精度問題が無い限りモデル間でブレない — 完全再 embedding が最も重いステップだから。

### MCP サーバの起動

```bash
kb-mcp serve --kb-path /path/to/knowledge-base
kb-mcp serve --kb-path /path/to/knowledge-base --model bge-m3   # index 時と一致必須
kb-mcp serve --kb-path ... --model bge-m3 --reranker bge-v2-m3  # + cross-encoder 再ランク
kb-mcp serve --kb-path ... --transport http --port 3100         # HTTP、複数クライアント
kb-mcp serve --kb-path ... --no-watch                           # ライブ同期無効
```

既定では stdio トランスポート (1 クライアント / サーバ) で MCP サーバを起動する。複数クライアントを同時接続するには `--transport http --port <PORT>` (または `--bind <SOCKETADDR>`) を渡し Streamable HTTP に切り替える — 詳細は [HTTP トランスポート (複数クライアント同時接続)](#http-トランスポート-複数クライアント同時接続) 参照。

サーバは 6 つの MCP ツール (後述) を公開し、インデックスをプロセス内に保持して低レイテンシでクエリに答える。`--model` が現在の index を作ったモデルと一致しない場合、実行可能なエラーメッセージで起動を拒否する。ファイルウォッチャ (既定有効) が `--kb-path` 配下のコンテンツ変更を検知して再インデックスする — [ライブ同期 (file watcher)](#ライブ同期-file-watcher) 参照。

`--reranker` (任意、既定 `none`) はハイブリッド検索の上位候補に cross-encoder 再ランクをかける:

- `none` — 無効 (既定)
- `bge-v2-m3` — BAAI/bge-reranker-v2-m3 (多言語 100+、初回 DL 約 2.3 GB)。日本語 KB では推奨
- `jina-v2-ml` — jinaai/jina-reranker-v2-base-multilingual (多言語、約 1.2 GB)。軽量版
- `bge-base` — BAAI/bge-reranker-base (英語 / 中国語のみ、約 280 MB)。日本語では非推奨

再ランクのレイテンシコストは、CPU で `bge-v2-m3` を 50 候補に適用した場合 1 クエリあたり約 300–700 ms。`--rerank-by-default` (`--reranker` 指定時は既定 on) はすべての `search` 呼び出しで再ランクするかを制御し、MCP ツール側は `rerank: Option<bool>` で per-query 上書き可能。reranker の切替に**再インデックスは不要** (index 非依存)。

#### 再ランクを有効にすべきケース

再ランクは精度とレイテンシのトレードオフ。使用パターン次第:

| シナリオ | 推奨 |
|---|---|
| 対話的エージェントフロー (LLM が 1 ターンで 2–5 回 `search` を呼ぶ) | **切っておく**。+500 ms × N が積もって重くなる。BGE-M3 + 見出し加重 bm25 の検索品質で大抵十分 |
| 精度重視の単発クエリ (調査・定義的回答) | **有効化**。レイテンシ税は 1 ターンに 1 回、cross-encoder が意味的に関連する候補を明確に前に出す |
| 混在 | `rerank_by_default = false` で始め、呼び出し側が MCP ツールの `rerank: true` パラメータで個別に選べるようにする |

再ランクを入れるべきサイン:

- トップ 5 が明白な正解チャンクを外すことが多い (クエリ言い換えをしても)
- インデックス側の表現と同義語 / 言い換え関係にあるクエリが失敗する (例: 日本語「バグ」 vs 英語 "error")
- エージェントが 1 ターンで何度も再クエリし、間違ったヒットを読むためにコンテキストを浪費している

再ランクは index 非依存なので、1 週間試して品質差を測り、見えなければ無効化してよい — 再インデックス不要。

### kb-mcp を OS サービスとして登録 (v0.8.0+)

`kb-mcp service install` で daemon を OS のユーザレベルサービスとして登録し、ログイン時の auto-start を設定できる (admin / sudo 不要)。

```bash
# デフォルト: service name 'kb-mcp'、bind 127.0.0.1:3100、auto-start ON
kb-mcp service install --kb-path /path/to/your-kb

# Multi-instance (= 複数 KB を別サービスとして実行)
kb-mcp service install --service-name work --kb-path /path/to/work-kb --bind 127.0.0.1:3100
kb-mcp service install --service-name personal --kb-path /path/to/personal-kb --bind 127.0.0.1:3101

# 確認 / 管理
kb-mcp service status                              # default 'kb-mcp'
kb-mcp service list                                # 全 instance
kb-mcp service uninstall personal                  # unit のみ削除、config + DB 残す
kb-mcp service uninstall personal --purge --yes    # config + DB も削除
```

OS 別バックエンド:
- **Linux**: systemd-user (`~/.config/systemd/user/kb-mcp-<name>.service`)。ログアウト後も daemon を生かしたい場合は `sudo loginctl enable-linger $USER` を実行。
- **macOS**: launchd LaunchAgent (`~/Library/LaunchAgents/com.kb-mcp.<name>.plist`)。
- **Windows**: Task Scheduler AT_LOGON (= admin 不要、`\kb-mcp-<name>` task)。

Installer は config home を `<dirs::config_dir()>/kb-mcp/<service-name>/` に作成し、`kb-mcp.toml` (`kb_path` / `bind` 含む) を配置。base directory は `KB_MCP_CONFIG_HOME` env var で override 可能。

非 loopback の bind (例: `0.0.0.0:3100`) は kb-mcp が認証機構を持たないため `--i-know` 明示が必要。

> **v0.7.x personal-http レシピからの移行**: `examples/deployments/personal-http/` のテンプレートは v0.8.0 で削除。手動 install 済の unit (`~/.config/systemd/user/kb-mcp.service` 等) がある場合は、`kb-mcp service install` 実行前に `systemctl --user disable kb-mcp.service && rm ~/.config/systemd/user/kb-mcp.service` (または各 OS 同等) で先に削除すること。

### インデックスの状態確認

```bash
kb-mcp status --kb-path /path/to/knowledge-base
```

既存 index から document / chunk 数を表示する。

### コマンドラインからの一発検索

シェルスクリプトや skill bin が「KB をこの文字列で検索したい」だけの目的で使う用途 — MCP 接続を立ち上げずに:

```bash
kb-mcp search "RAG server comparison" --limit 3 --format text
kb-mcp search "E0382" --category deep-dive --format json | jq '.results[] | .path'
kb-mcp search "クエリ最適化" --reranker bge-v2-m3        # 呼び出し単位の再ランクも可
```

`--format` は `json` (既定、後述「検索フィルタと引用」の通り `{ results, low_confidence, filter_applied }` ラッパ) か `text` (`---` 区切りの LLM フレンドリなブロック)。他のフラグは `serve` と同じ: `--kb-path` / `--model` / `--reranker` / `--category` / `--topic` / `--limit`。品質フィルタは既定有効 — 単発クエリで フィルタ無効状態に戻すには `--include-low-quality` または `--min-quality 0` を渡す。`kb-mcp.toml` の既定値は `serve` / `index` と同じく適用される。

典型的な skill-bin 用途: Claude Code の skill が `bin/` に `kb-mcp.exe` + `kb-mcp.toml` を同梱し、`kb-mcp search "{{user_query}}" --format text --limit 3` のようなコマンドで LLM が引用するための参照抜粋を返す。

### 検索フィルタと引用 (v0.3.0+)

v0.3.0 から `search` MCP ツールの戻り値が単なるヒット配列ではなくラッパオブジェクトになる。**これは破壊的変更**で、`Vec<SearchHit>` を直接 parse しているクライアントは更新が必要:

```jsonc
{
  "results":        [{ "score": 0.83, "path": "...", "match_spans": [...], "tags": [...], ... }],
  "low_confidence": false,
  "filter_applied": { /* デフォルトと異なるフィルタだけ echo back、フィルタ無しなら空 object */ }
}
```

`results[].match_spans` は ASCII クエリの場合に `content` 内のバイトオフセットを返すため、MCP クライアント側で原文の正確な引用を作れる。`low_confidence` は順位ベースの flag (`top1.score / mean(top-N.score) < min_confidence_ratio`) で、閾値の既定は `1.5`。`kb-mcp.toml` の `[search].min_confidence_ratio` で全体調整、`--min-confidence-ratio` で per-query 上書き可能。

入力境界 (防御的、v0.6.0+): `query` は 1 KiB 上限、超過時は `ErrorResponse` で reject。`match_spans` は 256 KiB 以下の chunk にのみ計算、上限 100 span/chunk。乱用防止が目的で正常用途には影響しない — 通常 chunk は十分上限以下。

v0.3.0 で `search` ツール / CLI に追加されたフィルタ:

```bash
kb-mcp search "tokio spawn" \
  --path-glob "docs/**" --path-glob "!docs/draft/**" \
  --tag-any rust,async \
  --date-from 2026-01-01 \
  --min-confidence-ratio 1.5
```

- `--path-glob <PATTERN>` (繰り返し可) — パス glob によるフィルタ。`!` 始まりは exclude。MCP param: `path_globs`
- `--tag-any <a,b,c>` — チャンクが**いずれか**のタグを持つときのみ通過。MCP param: `tags_any`
- `--tag-all <a,b,c>` — チャンクが**すべての**タグを持つときのみ通過。MCP param: `tags_all`
- `--date-from <YYYY-MM-DD>` / `--date-to <YYYY-MM-DD>` — 辞書順比較。どちらかが指定された場合、`date` 未設定のチャンクは厳密に除外される。MCP params: `date_from` / `date_to`
- `--min-confidence-ratio <N>` — `low_confidence` 閾値の per-query 上書き

CLI `kb-mcp search --format json` も同じラッパ形式で出力する。`match_spans` / byte offset の詳細は [docs/citations.ja.md](docs/citations.ja.md)、フィルタの完全リファレンスは [docs/filters.ja.md](docs/filters.ja.md) 参照。

### 多様性 (MMR) と parent retriever (v0.7.0+)

retrieval 品質を上げるための任意の knob を 2 つ追加。両者は独立しており、片方だけ on / 両方 on / 両方 off いずれでも動く。**既定は両方 off** なので既存パイプラインの挙動は変わらない。

```bash
# MMR (多様性再ランク)
kb-mcp search "tokio runtime" --mmr true --mmr-lambda 0.7

# Parent retriever (短い chunk を隣接 sibling や全文に展開)
kb-mcp search "k=60 in RRF" --parent-retriever true

# 両方同時
kb-mcp search "context management" --mmr true --parent-retriever true
```

CLI フラグ (`kb-mcp eval` も同じものを受け付ける):

- `--mmr <bool>` — MMR 多様性再ランクを有効化。既定 `false`
- `--mmr-lambda <0..1>` — MMR の関連度と多様性のバランス。`1.0` で「多様性なし」(= MMR off と等価)、低くすると探索寄り (重複の少ない候補を優先)。既定 `0.7`
- `--mmr-same-doc-penalty <0..1>` — 既選択チャンクと同一 document に属する候補へ追加コストを乗せる係数。`0.0` で純 MMR、上げると同 doc chunk を積極的に除外。既定 `0.0`
- `--parent-retriever <bool>` — ヒットチャンクの token_count が `whole_doc_threshold_tokens` 未満のとき、`content` を隣接 sibling (level 一致を優先) もしくはドキュメント全体 (極端に短いチャンクの fallback) に拡張する。score / rank / path / `match_spans` は変えず、`content` と新しい optional `expanded_from` のみ変化。既定 `false`

MCP `search` ツールも同名の per-call params (`mmr` / `mmr_lambda` / `mmr_same_doc_penalty` / `parent_retriever`) を受ける。toml 既定値は `[search.mmr]` / `[search.parent_retriever]` (上の[設定ファイル (任意)](#設定ファイル-任意) 節)。優先順位は per-call > toml > built-in defaults。

パイプライン順序は **`RRF → reranker → MMR → parent retriever → match_spans`**。MMR は reranker score を保ったまま並べ替え、parent retriever は最後に走るので展開 content が relevance signal を汚さない。完全な解説とチューニング指針は [docs/retrieval-pipeline.ja.md](docs/retrieval-pipeline.ja.md) 参照。

### 起点ドキュメントからの Connection Graph

単一ドキュメントではなく「その近傍 (さらにその近傍)」を意味的に探索したいときは `graph` サブコマンド:

```bash
kb-mcp graph --start deep-dive/mcp/overview.md --depth 2 --fan-out 5
kb-mcp graph --start notes/rag.md --dedup-by-path --format text
kb-mcp graph --start a.md --exclude junk1.md,junk2.md --min-similarity 0.5
```

フラグ:

- `--start PATH` — 必須、index 済みドキュメントの相対パス
- `--depth` (既定 2、最大 3 にクランプ) — BFS のホップ数
- `--fan-out` (既定 5、最大 20 にクランプ) — ホップあたりのノード隣接数。`0` なら seed のみ返却
- `--min-similarity` (既定 0.3) — コサイン類似度カットオフ。`0.0..=1.0`
- `--seed-strategy` — `all-chunks` (既定) は起点文書の全チャンクから展開、`centroid` は平均 (L2 再正規化) した 1 個の仮想 seed を使う
- `--exclude` — 結果から除外するカンマ区切りパス。起点パス自身は常に除外される
- `--dedup-by-path` — 同一パスのヒットをまとめて各ドキュメント最大 1 回に
- `--category` / `--topic` — 各ホップにカテゴリ / トピックフィルタを適用
- `--format json|text` — `search` と同じ

出力は `parent_id` / `depth` / `score` 付きのノードのフラット配列で、消費側で木を再構築できる。典型ユース: 「この note の周りの関連コンテキストを 30 チャンク LLM に読ませたい」「この overview から 2 ホップ辿ってどのトピックに触れているか見たい」。

### TOML スキーマによる frontmatter 検証

ナレッジベースで frontmatter の規約を運用しているなら、`kb-mcp validate` がすべての `.md` を TOML スキーマに対して検証し違反を報告する。スキーマ書式は [Frontmatter スキーマ検証](#frontmatter-スキーマ検証) 節参照。コマンド自体は:

```bash
kb-mcp validate --kb-path /path/to/knowledge-base
kb-mcp validate --kb-path ... --format json | jq '.files[]'
kb-mcp validate --kb-path ... --format github         # CI 用 ::error annotation
```

終了コード: `0` (違反なし) / `1` (違反あり) / `2` (スキーマロードエラー)。`--kb-path` 直下に `kb-mcp-schema.toml` が無いときは短い "no schema found" メッセージと共に exit 0 となるため、既存ワークフローへの `kb-mcp validate` 追加は実際にスキーマを書くまで非破壊。

> `--strict` フラグは現状 no-op (将来のより厳格な検証モードへの前方互換のため受理されるだけ)。当面は通常の呼び出しで OK。

### Golden query セットに対する retrieval 品質評価

**任意のパワーユーザ機能**。`kb-mcp eval` は「想定される正解がわかっている質問」の小さなファイルを、`search` ツールと同じハイブリッド検索にかけ、**recall@k / MRR / nDCG@k** + 前回実行との差分を出す。モデル比較や `[quality_filter]` / RRF パラメータのチューニング時に便利。

`kb-mcp index` + `kb-mcp serve` で普通に使う一般ユーザは触る必要なし — golden ファイルが無ければ `eval` は hint 付きエラーで終了するだけで他の挙動には影響しない。

```bash
# 1) Golden YAML を <kb_path>/.kb-mcp-eval.yml に配置
cat > knowledge-base/.kb-mcp-eval.yml <<'EOF'
queries:
  - query: "RRF の k パラメータの意味は？"
    expected:
      - { path: "docs/ARCHITECTURE.md", heading: "Data flow" }
      - { path: "src/db.rs" }   # heading 省略 = ファイル一致で正解
EOF

# 2) index 済み DB に対して実行
kb-mcp eval --kb-path knowledge-base

# 3) 設定やモデルを変えて再実行、diff で変化を見る
kb-mcp eval --kb-path knowledge-base --reranker bge-v2-m3
```

出力: 集計指標 + 劣化 / ミスのあるクエリ行のみ。`--format json` で全クエリの詳細を取得可能。履歴は `<kb_path>/.kb-mcp-eval-history.json` に保存され、直近 10 件を diff 表示用に保持する。

CI 用途には `--fail-on-regression` (v0.6.0+) を渡す。直前の **fingerprint-compatible** run から `recall@k` / `MRR` / `ndcg@k` のいずれかが `regression_threshold` (既定 0.05) を超えて退化していたら exit code 1 を返す。golden YAML を更新すると hash が変わるので次回 run は比較対象外 = false positive にならない。

Golden YAML のリファレンス、指標の詳細説明、diff 出力の読み方、トラブルシューティングは [docs/eval.ja.md](docs/eval.ja.md) 参照。

## Claude Code / Cursor への接続

> **デプロイ用の完全なレシピは** [`examples/deployments/`](./examples/deployments/) **を参照**。4 パターン (個人 stdio / 個人 http = 1 マシン上で複数 Claude Code 並行用の loopback daemon / NAS 共有 = 1 writer + 多 read-only / 社内 HTTP サーバ = 1 サーバ + 多クライアント) で `kb-mcp.toml` / `.mcp.json` / systemd unit までセットで揃えてある。下のスニペットはそれらのレシピの中核を成す stdio エントリポイント。

プロジェクトルート (またはクライアント対応の MCP 設定場所) の `.mcp.json` に以下を追加:

```json
{
  "mcpServers": {
    "ai-knowledge": {
      "command": "/path/to/kb-mcp",
      "args": ["serve", "--kb-path", "/path/to/knowledge-base"],
      "type": "stdio"
    }
  }
}
```

多言語モデル + 再ランクを有効化する場合:

```json
{
  "mcpServers": {
    "ai-knowledge": {
      "command": "/path/to/kb-mcp",
      "args": [
        "serve",
        "--kb-path", "/path/to/knowledge-base",
        "--model", "bge-m3",
        "--reranker", "bge-v2-m3"
      ],
      "env": {
        "FASTEMBED_CACHE_DIR": "/path/to/.cache/huggingface/hub"
      },
      "type": "stdio"
    }
  }
}
```

エージェントワークフロー向けの保守的な案: reranker はロードするが既定はオフにしておき、呼び出し側が個別 `search` で `rerank: true` を指定してオプトインする:

```json
{
  "mcpServers": {
    "ai-knowledge": {
      "command": "/path/to/kb-mcp",
      "args": [
        "serve",
        "--kb-path", "/path/to/knowledge-base",
        "--model", "bge-m3",
        "--reranker", "bge-v2-m3",
        "--rerank-by-default=false"
      ],
      "env": { "FASTEMBED_CACHE_DIR": "/path/to/.cache/huggingface/hub" },
      "type": "stdio"
    }
  }
}
```

あるいは、[探索パス](#設定ファイルの探索順) のいずれかに `kb-mcp.toml` を置いて同じ項目を設定しているなら、`.mcp.json` はここまで縮められる:

```json
{
  "mcpServers": {
    "ai-knowledge": {
      "command": "/path/to/kb-mcp",
      "args": ["serve"],
      "type": "stdio"
    }
  }
}
```

クライアント接続時にサーバが自動起動する。

### PostToolUse hook による index 鮮度保守
Claude Code セッション内部からナレッジベースを編集する (または Markdown を書く skill を実行する) 場合、MCP サーバは再構築されるまで古い結果を返し続ける。`.claude/settings.json` の `PostToolUse` hook で書込み後に自動再 index できる。最小形:

```json
{
  "hooks": {
    "PostToolUse": [
      {
        "matcher": "Write|Edit|MultiEdit|Skill",
        "hooks": [
          { "type": "command", "command": "kb-mcp index" }
        ]
      }
    ]
  }
}
```

`kb-mcp index` の SHA-256 差分検出により 2 回目以降は高速 (小さな KB なら大抵 1 秒未満)。ツールペイロードを精査して編集ファイルが `$KB_PATH` 配下のときだけ再構築する、より精密なシェルスクリプトがリポジトリ同梱 — [`examples/hooks/`](./examples/hooks/README.ja.md) 参照。SQLite は WAL モードで動作するため、MCP サーバ起動中に hook が走っても安全。

### Frontmatter スキーマ検証
ナレッジベースで frontmatter 規約を運用しているなら (例: `title` 必須、`date` は YYYY-MM-DD、`topic` は enum)、以下でファイル毎の違反をチェックできる:

```bash
kb-mcp validate --kb-path /path/to/knowledge-base
```

`--kb-path` 直下に `kb-mcp-schema.toml` を置く (テンプレート: `kb-mcp-schema.toml.example`):

```toml
[fields.title]
required = true
type = "string"
min_length = 1

[fields.date]
required = true
type = "string"
pattern = '^\d{4}-\d{2}-\d{2}$'

[fields.topic]
required = true
type = "string"
enum = ["mcp", "rag", "ai", "tooling", "ops"]

[fields.tags]
required = true
type = "array"
min_length = 1
```

- **スキーマファイル無し → exit 0** と短い "no schema found" メッセージ。従来挙動を保持
- `--format text` (既定、TTY では色付き) / `json` / `github` (CI annotation 用)
- 終了コード: `0` (違反なし) / `1` (違反あり) / `2` (スキーマロードエラー)
- `.txt` は frontmatter の概念が無いのでスキップ
- `index` / `serve` コマンドには影響しない — 検証は opt-in のみ

### HTTP トランスポート (複数クライアント同時接続)
既定の `kb-mcp serve` は stdio で MCP を話す — 1 クライアント / サーバプロセス。複数クライアント同時接続 (例: 複数の Claude Code セッション、または外部スクリプトが同じ index を叩く) には Streamable HTTP に切替:

```bash
kb-mcp serve --kb-path /path/to/knowledge-base --transport http --port 3100
# または: --bind 0.0.0.0:3100
```

サーバは `/mcp` に MCP エンドポイントをマウントし、`/healthz` をヘルスプローブ用に公開する。HTTP 対応クライアントの `.mcp.json`:

```json
{
  "mcpServers": {
    "ai-knowledge": {
      "type": "http",
      "url": "http://127.0.0.1:3100/mcp"
    }
  }
}
```

セキュリティ注意:
- 既定 bind は `127.0.0.1:3100` (loopback)。`--bind 0.0.0.0:3100` は信頼できるネットワークでのみ使用 — **kb-mcp はまだ認証機構を内蔵していない**
- rmcp の Streamable HTTP 層は Host ヘッダ検証を強制 (既定で loopback のみ) し、DNS rebinding 攻撃を防ぐ
- LAN / イントラ公開時は `kb-mcp.toml` の `[transport.http].allowed_hosts` に公開ホスト名 / IP を明示する (例: `["kb.example.lan", "192.168.1.10"]`)。loopback only の default のまま 0.0.0.0 で bind すると外部リクエストは Host 検証で 403 になる — operator のミス確定なので、kb-mcp は起動時に `tracing::warn` を出して気付かせる。`allowed_hosts = []` (空配列) を渡すと Host 検証が完全に無効化される (rmcp の `disable_allowed_hosts` 相当、operator 自己責任の opt-out。public 公開には推奨されない)
- サーバ内部の Mutex ベース直列化により、HTTP の並列リクエストでも embedder / DB 層では逐次処理される (`search` で目安 10 qps 程度)。本格的な並列化は将来の拡張

### ライブ同期 (file watcher)
`kb-mcp serve` は既定で `notify` ベースのファイルウォッチャを走らせる。`--kb-path` 配下の任意の変更 (create / modify / delete / rename) が検知され、debounce ののち該当ファイルのみが再インデックスされる。手動の editor save・`git pull`・外部スクリプトといった、PostToolUse hook では捕まえられないケースをカバーする。

- **既定 on**。`kb-mcp.toml` の `[watch].enabled = false` または CLI `--no-watch` で無効化
- **Debounce** は既定 500 ms。`[watch].debounce_ms` または `--debounce-ms` で調整
- **PostToolUse hook と共存**。両経路は同じ `Mutex<Database>` / `Mutex<Embedder>` をロックするため、同時トリガは Rust 層で直列化され冪等
- **拡張子対応**。watcher は `rebuild_index` と同じ Parser registry を共有し、`[parsers].enabled` で有効化された拡張子のファイルのみを再インデックスする。他イベントは破棄
- **耐障害性**。watcher タスク内部のエラーは stderr にログされ (黙殺しない)、MCP サーバは動作し続ける。ローカルディスクを想定 — WSL / SMB / ネットワーク共有上の inotify は保証外
- **バックプレッシャ (v0.6.0+)**。debouncer から indexer task へのブリッジは bounded な 64 batch channel。consumer が追い付けない場合 (embedder が一時停止中など) は無限に queue が伸びることはなく、超過 batch を warn ログ付きで drop する。バースト後に `rebuild_index` を手動実行で取り漏らしを補える

### HuggingFace の TLS 失敗への対処 (初回 DL 時)

環境によっては (企業プロキシ、TLS inspection を行うファイアウォール) fastembed の native TLS 接続が `huggingface.co` に対して `os error 10054` / "Connection was reset" で失敗する。その場合は Python の HuggingFace CLI で事前にモデルを DL し、`FASTEMBED_CACHE_DIR` で HF Hub キャッシュを指す:

```bash
# 一度インストール
pip install --user huggingface_hub

# BGE-M3 を事前 DL (必要な ONNX ファイルのみ)
hf download BAAI/bge-m3 \
    --include 'onnx/*' 'tokenizer*' 'config.json' 'special_tokens_map.json'

# BGE-reranker-v2-m3 を事前 DL (`--reranker bge-v2-m3` 用)
hf download BAAI/bge-reranker-v2-m3

# HF cache を指して kb-mcp を起動 (HF Hub cache は fastembed と互換)
FASTEMBED_CACHE_DIR=~/.cache/huggingface/hub \
    kb-mcp index --kb-path ./knowledge-base --model bge-m3 --force
```

## MCP ツール

| ツール | 説明 | 主なパラメータ |
|---|---|---|
| `search` | ベクトル + FTS5 全文検索を Reciprocal Rank Fusion でマージしたハイブリッド検索、任意で cross-encoder 再ランク + MMR 多様性再ランク + parent retriever 展開。`{ results, low_confidence, filter_applied }` ラッパで関連度ランク付き chunk を返す。parent retriever が発火した行には `expanded_from` も付く。詳細: [docs/citations.ja.md](docs/citations.ja.md)、[docs/filters.ja.md](docs/filters.ja.md)、[docs/retrieval-pipeline.ja.md](docs/retrieval-pipeline.ja.md) | `query` (必須)、`limit`、`category`、`topic`、`rerank` (サーバ既定を上書き)、`min_quality`、`include_low_quality`、`path_globs` (`!` 始まりは exclude)、`tags_any` / `tags_all`、`date_from` / `date_to` (`YYYY-MM-DD`)、`min_confidence_ratio`、`mmr` / `mmr_lambda` / `mmr_same_doc_penalty` (v0.7.0+)、`parent_retriever` (v0.7.0+) |
| `list_topics` | index 済みの全トピック / カテゴリと文書数を列挙 | (なし) |
| `get_document` | 相対パスから文書の全文 + メタデータを取得 | `path` (例: `"deep-dive/mcp/overview.md"`) |
| `get_best_practice` | opt-in: `kb-mcp.toml` の `[best_practice].path_templates` を設定しているときのみ機能する。対象向けの best practice 文書を取得し、任意で特定 h2 セクションを抽出。未設定時は "not configured" エラーを返す | `target` (例: `"claude-code"`)、`category` (任意) |
| `rebuild_index` | すべてのソースファイル (Markdown + `[parsers].enabled` で有効化された拡張子) を走査してインデックス再構築 | `force` (任意、既定 false) |
| `get_connection_graph` | ドキュメントパスを起点に意味的に関連するチャンクを BFS 展開。`parent_id` / `depth` / `score` / `snippet` 付きのノード配列を返し、呼び出し側でコンテキスト発見を連鎖させられる | `path` (必須)、`depth` (既定 2、最大 3)、`fan_out` (既定 5、最大 20)、`min_similarity` (既定 0.3)、`seed_strategy` (`all_chunks` / `centroid`)、`dedup_by_path`、`category`、`topic`、`exclude_paths` |

## 補足

- **埋め込みモデル**: 初回実行時、選択した ONNX モデルが OS 標準のキャッシュディレクトリに DL される。2 回目以降は再利用。解決順:
  1. `FASTEMBED_CACHE_DIR` 環境変数 (設定されていれば)
  2. OS キャッシュ + `fastembed` (Linux: `~/.cache/fastembed`、macOS: `~/Library/Caches/fastembed`、Windows: `%LOCALAPPDATA%\fastembed`)
  3. CWD 直下の `.fastembed_cache` (最終フォールバック)
- **インデックス保存先**: SQLite DB は `--kb-path` の**親ディレクトリ**に `.kb-mcp.db` として保存される (例: `--kb-path ./knowledge-base` ならリポジトリルート)
- **Parser registry**: `[parsers].enabled` に列挙された拡張子のみインデックス対象。既定は `["md"]` (従来デフォルト)、`["md", "txt"]` で `.txt` にオプトイン (タイトルはファイル名派生)。未知 id (例: `"pdf"` / `"rst"`) は起動時に拒否、空配列も「何もインデックスされない」事故防止のため拒否
- **ライブ同期ウォッチャ**: `kb-mcp serve` は `notify` ベースの watcher を既定 spawn (`[watch].enabled = true`、500ms debounce)。手動 save / `git pull` / 外部スクリプトを MCP ツールと同じ Mutex 付きリソース上で増分再インデックスするため、同時トリガは直列化される。`--no-watch` / `[watch].enabled = false` で無効化
- **HTTP トランスポート**: `--transport http --port 3100` で rmcp の Streamable HTTP を `/mcp` に提供し、`/healthz` をプローブ用、内部は Mutex 直列化。既定 bind は `127.0.0.1:3100`、`0.0.0.0` は明示 opt-in かつ**まだ認証機構無し** — リバースプロキシ / ファイアウォール側で保護すること
- **埋め込み次元**: `--model` で決まる。BGE-small-en-v1.5 = 384、BGE-M3 = 1024。選択した次元は `vec_chunks` 仮想テーブルに宣言され `index_meta` に記録される。実行時の不一致は検出して拒否
- **増分インデックス**: ファイルは SHA-256 content hash で追跡。以降の `index` 実行では変更されたファイルのみ再 embedding される (`--force` を渡さない限り)。内容を変えずに移動 / リネームすると hash 一致で検知され `documents.path` の UPDATE として処理 — 既存の chunk / embedding / FTS 行は再利用される。再構築サマリでは `updated` / `deleted` の隣に `renamed` としてカウントされる
- **ハイブリッド検索 (FTS5 + ベクトル)**: `search` ツールは SQLite FTS5 全文検索 (trigram tokenizer、日本語 / CJK も動く。bm25 では `heading` 列を `content` の 2 倍重み) をベクトル検索と Reciprocal Rank Fusion (k=60) でマージする。返される `score` は RRF スコア (大きいほど良い) で距離ではない。3 文字未満のクエリは trigram の最小値を下回るためベクトルのみにフォールバック
- **任意の再ランク**: `--reranker <model>` を付けると上位候補が cross-encoder で再スコアされてから返る。再ランク適用時は `score` が RRF 値ではなく cross-encoder の生スコアになる。再ランクは index 非依存 — サーバ起動時に再インデックスなしでトグル可能
- **Connection graph**: `get_connection_graph` / `kb-mcp graph` はドキュメント起点でベクトルインデックス上を BFS する。追加インデックスは作らず、ホップ毎に sqlite-vec KNN を新規発行する。`depth ≤ 3` / `fan_out ≤ 20` で client-side クランプされるため、最悪でも 1 リクエストあたり約 21 KNN クエリ。スコアは L2 距離からの近似コサイン類似度 (`1 - d²/2` を `[0,1]` にクランプ、unit normalized embedding を前提 — BGE-small / BGE-M3 は内部で正規化済み)
- **見出し除外**: 見出しテキストが `exclude_headings` のいずれかを含むセクションは、チャンキング時に落とされる。既定は空リスト (全セクション残す)。`kb-mcp.toml` の `exclude_headings` に substring を列挙するとオプトインになる。マッチは部分文字列 (`heading.contains(pattern)`) で、短いパターンは `"参考リンク"` → `"## 参考リンク (旧)"` のような変種も拾う
- **ディレクトリ除外**: `walkdir` は basename が `exclude_dirs` のいずれかと一致するディレクトリ (とその subtree) をスキップする。既定は `[".obsidian", ".git", "node_modules", "target", ".vscode", ".idea"]`。ユーザ指定リストは既定を完全に置き換える (merge ではない)。`exclude_dirs = []` を明示すると `.git/` 等も含めて全走査する
- **`get_best_practice` path templates**: opt-in 機能で、使うには `kb-mcp.toml` の `[best_practice].path_templates` を設定する必要がある。各テンプレートは `{target}` をプレースホルダとして使える (例: `"best-practices/{target}/PERFECT.md"`、`"docs/{target}.md"`)。サーバはリスト順に試して `kb_path` 配下に最初に存在したファイルを返す (path traversal は拒否)。セクション省略 or `path_templates = []` の場合はツール自体は登録されるが "not configured" エラーを返すため、意図しない呼び出しは明示的に失敗する
- **チャンク単位品質フィルタ** (**既定有効** 閾値 `0.3`): インデックス時に各チャンクに対し 3 つのシグナル — 長さ (30 文字未満 → -0.6)、定型語のみ (TBD / TODO / 詳細は後述 等 → -0.5)、弱い構造 (80 文字未満の 1 行 → -0.3) — から `quality_score` を計算。閾値未満のチャンクは `search` / `kb-mcp search` / `get_connection_graph` で非表示。`get_connection_graph` の seed チャンクは免除。フィルタ無効化は `kb-mcp.toml` の `[quality_filter] enabled = false`、per-query は CLI `--include-low-quality` / MCP `include_low_quality: true`。閾値上書きは `--min-quality 0.5` / `min_quality: 0.5`。既存 index のアップグレード: 次の `kb-mcp index` 実行時に `quality_score` 列が透過的に追加され (ALTER TABLE)、1 度だけ backfill される (冪等)
