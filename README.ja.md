# kb-mcp

Markdown / プレーンテキストのナレッジベースに対するセマンティック検索を提供する MCP サーバ。

YAML frontmatter 付きの Markdown (および任意で `.txt` / `.pdf` / `.docx` / `.xlsx` / `.pptx`) をパースし、見出し単位でチャンク化、選択可能な埋め込みモデル (既定は BGE-small-en-v1.5、多言語 / 日本語向けには BGE-M3) でベクトルを生成して、sqlite-vec 搭載の SQLite に格納する。stdio (既定、1 クライアント) または Streamable HTTP (複数クライアント) トランスポート経由で Claude Code / Cursor など MCP 互換クライアントに接続する。

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

> **Windows で service として動かすなら、archive がもう 2 つ要る。** どちらも別ダウンロード (v0.14.0 以降) で、`kb-mcp.exe` と**同じディレクトリ**に展開する:
>
> | Archive | 理由 |
> | --- | --- |
> | `kb-mcp-svc-x86_64-pc-windows-msvc.zip` | `kb-mcp service install` は `kb-mcp-svc.exe` が `kb-mcp.exe` の隣にあればそれを logon task の起動対象にし、**無ければ console 可視の launcher に fallback する** — 毎回のログオンでコンソール窓が一瞬出る。fallback したことは warning で報告されるが、`service install` の**前に**展開しておけば入れ直さずに済む。 |
> | `kb-mcp-tray-x86_64-pc-windows-msvc.zip` | 任意。system tray 監視 binary で、`service install --with-tray` を使う場合のみ必要。 |

各アーカイブにはバイナリの他に `CHANGELOG.md` / `LICENSE-MIT` / `LICENSE-APACHE` / `README.md` が同梱される。実行前にリリースに添付された `sha256.sum` または各アーカイブ用 `*.sha256` で SHA-256 チェックサムを照合すること。

ONNX runtime と SQLite はバイナリに静的リンクされているので、追加 DLL は不要。Embedding モデル (ONNX) は初回実行時に HuggingFace から DL される — ネットワークがそれをブロックする場合は [HuggingFace の TLS 失敗への対処](#huggingface-の-tls-失敗への対処-初回-dl-時) を参照。

### ソースからビルド

```bash
cargo build --release
```

バイナリは `target/release/kb-mcp` (Windows では `kb-mcp.exe`) に生成される。

## 設定ファイル (任意)

以下の CLI オプションはすべて `kb-mcp.toml` で既定値を与えられる。CLI 引数は常に優先され、設定ファイルは単に同じデプロイでの記述の繰り返しを減らすためのもの。配置場所の探索順は [設定ファイルの探索順](#設定ファイルの探索順) を参照 — 最も一般的なのはプロジェクトルート (CWD) かバイナリの隣。[`kb-mcp/kb-mcp.toml.example`](kb-mcp/kb-mcp.toml.example) を `kb-mcp.toml` にコピーして編集する。

**このテンプレートはコピーしても何も変わらない。** 空ではなく、ファイルの形が見えるように一部のセクション (`[quality_filter]` / `[parsers]` / `[watch]` / `[transport]` / `[transport.http]`) は有効なまま残してあるが、**有効な値はすべて既定値そのもの**なので、コピーは「既定値を明示的に固定する」だけで挙動を変えない。挙動が変わる項目はすべてコメントアウトしてある。一方、下のブロックは別物で、各キーが何をするかを示すために値を入れた**説明用の例** — 既定値でない値もあれば、既定値をそのまま書いているものもある。**丸ごと貼るためのものではなく、メニューとして**読むこと:

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

# `get_best_practice` MCP ツールは opt-in。このセクションが無い (または空
# リスト) 場合、ツールは "not configured" エラーを返す。テンプレートは先頭から
# 順に `{target}` を置換して kb_path 相対で探し、最初に見つかったものを返す。
[best_practice]
path_templates = ["best-practices/{target}/PERFECT.md", "docs/{target}.md"]

# index 対象拡張子。セクション省略で デフォルト挙動
# (.md のみ)。明示リストで .txt / .pdf / .docx / .xlsx / .pptx に
# オプトイン。空配列 [] は「何もインデックスされない」事故を防ぐため拒否
# される。現在サポート id: "md" / "txt" / "pdf" (v0.10.0+) / "docx" /
# "xlsx" / "pptx" (v0.11.0+)。("xls" は v0.14.0 で取り下げ、下記参照)
# 全部入り例:
[parsers]
enabled = ["md", "txt", "pdf", "docx", "xlsx", "pptx"]

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
# /healthz を allowed_hosts の検査対象外に置くか。既定は true (= public、Host
# check なし)。false にすると /healthz も他のエンドポイントと同様に検証され、
# allow-list に無い Host ヘッダのリクエストは 200 ではなく 403 になる (v0.7.5+)。
# **認証ではない**: Host ヘッダは呼び出し元が自由に付けられるので、ポートに
# 到達できて許可値を送れば 200 が返る。偶発的な探索の敷居を上げるだけ。
# healthz_public = false
# 同時に生きていられる MCP session の数。既定 256 (= 約 25 MB。生きた session
# 1 つが約 100 KB)。満杯の間、**新規** session を開こうとするリクエストは
# Retry-After 付きの 429 になり、確立済みの session はそのまま使える。0 で無制限。
# **対象は MCP 2025-11-25 以前のクライアントだけ** — 2026-07-28 には session が
# 無く (SEP-2567)、その要求がこの上限で断られることはない (v0.19.0+)。
# max_sessions = 256

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

# 任意: RRF / bm25 の fusion パラメータ (v0.13.0+)。以下は既定値。
# 自分の KB で `kb-mcp tune` が推奨しない限り触らないこと。
# [search.fusion]
# rrf_k = 60.0                # >= 1.0。小さいほど片方の検索器の 1 位を重視
# bm25_heading_weight = 2.0   # >= 0.0
# bm25_context_weight = 1.0   # >= 0.0
# bm25_content_weight = 1.0   # >= 0.0

# 任意: 静的 Contextual Retrieval (v0.12.0+)。既定 off。reranker を
# 併用しない限り悪化するため、reranker 設定時のみ有効化を推奨
# (詳細は下の「Contextual Retrieval」節を参照)。
# [contextual]
# enabled = true
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

起動時に stderr へ `kb_mcp::config: loaded config source=... path=... trust=...`
が出るので、**どの toml が効いているか**と**どこまで信用したか**をログで確認できる。

##### 信頼する置き場所 / しない置き場所

優先度 2 と 3 は**ユーザが名前を挙げていないファイル**を拾う。他人が書いた
リポジトリに `cd` した場合や、MCP クライアントがそこを cwd にしてサーバを起動した
場合、そのファイルがそのまま効いてしまう。そこで kb-mcp は**置き場所だけから**
(ファイルの中身は一切見ずに) 運用者のものかどうかを判定する:

- **信頼する**: `--config` (自分で名指しした)、`<binary-dir>` (書き込みには
  インストール先への権限が要る)、`kb-mcp service install` が使う config home、
  そしてファイルが無い場合
- **信頼しない**: それ以外の、CWD / `.git` 祖先で見つかったもの

信頼しない config も**読み込みはする**。KB の見せ方を決めるだけのもの
(`[search]` / `[quality_filter]` / `exclude_dirs` / `[parsers]` / `[watch]` /
`[contextual]`) はそのまま効く。制限するのは 3 つだけで、これらは「どのバイナリを
実行するか」「何が外に出るか」「誰から届くか」を決めるため:

| フィールド | 信頼しない config の場合 |
| --- | --- |
| `fastembed_cache_dir` | 警告して無視し、標準のキャッシュディレクトリを使う。どの `.onnx` を読むかを決める値であり、キャッシュに既にあるモデルは検証されないため (関連: `FASTEMBED_CACHE_DIR` は絶対パス必須で、モデルディレクトリが CWD 相対に解決されることは無い) |
| `[transport.http].bind` | 非 loopback ならポートを保ったまま `127.0.0.1` に降格 (警告つき)。`allowed_hosts` / `healthz_public` / `max_sessions` は破棄する — 前 2 つは loopback 限定の `Host` チェックに戻し、3 つ目は組み込みの既定に戻す (植えられた `max_sessions = 1` で「2 人目が繋げないサーバ」を他人に作らせないため)。`kind` は尊重する |
| `kb_path` | ファイルシステムのルート / ホームディレクトリ / その祖先 / config ファイルのあるディレクトリの祖先 を指していれば**警告して無視**。`--kb-path` は従来どおり効くので上書きでき、どちらも無ければ通常どおり「`--kb-path` is required」で停止する |

`kb_path` の規則は「閉じ込め」ではなく「境界弾き」で、`kb_path = "./docs"` も
`kb_path = "/srv/kb/knowledge-base"` も通る (project-local な `kb-mcp.toml` に
絶対パスを書く使い方はそのまま)。塞ぐのは**環境を知らなくても書ける**指定 —
`../..` / `/` / `C:\Users` と、それらを指す symlink。

全部効かせたいなら名指しする: `kb-mcp serve --config ./kb-mcp.toml`。

インストール済みサービスは影響を受けない。v0.20.0 以降、`kb-mcp service install` は
登録する unit / plist / scheduled task に `--config <config home>/kb-mcp.toml` を
書き込む。daemon は config を「探す」のではなく「名指しされる」ので、起動時の環境が
どうであっても信頼される。これで唯一の例外だったケース —
**`KB_MCP_CONFIG_HOME` を `service install` の時だけ設定した場合**、その値は daemon
実行時の環境には無いため信頼されなかった — が塞がる。

旧版で登録したサービスは launch line が古いままになる。更新するには**自分が使った**
`kb-mcp service install` コマンドに `--force` を足して再実行すること (素の
`service install` は service 名 / auto-start / bind を既定値に戻してしまう)。

**`KB_MCP_CONFIG_HOME` を最初に設定したなら、再実行時にも設定すること。** この値は
どこにも記憶されていない — `service install` は**実行時の環境**から config home を
決めるので、変数を付けずに再実行すると**別の場所に最小構成の config を作り**、
サービスをそちらへ向けてしまい、本来の設定が使われなくなる。まさにこの修正の
対象になっている人に当てはまる注意点。

Linux / macOS では再インストール時にサービスを再起動するので、新しい launch line は
即座に効く。Linux は `--no-auto-start` で入れたサービスを手動起動している場合も対象
(起動中の時だけ再起動する)。手動での再起動が要るのは 2 ケース —
**Windows** (scheduled task は再登録するが detach 済みの daemon は止めない) と、
**macOS で `--no-auto-start` かつ既に load 済みの LaunchAgent** (installer が
意図的に触らない)。サインアウト / サインインするか、自分で停止・起動すること。

**カバーしない範囲**: リポジトリが `.mcp.json` ごと同梱している場合、相手は
config ファイルではなくコマンドライン全体を握っている。kb-mcp 側の規則では
どうにもならず、そこは MCP クライアントの承認プロンプトの領分。

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

既定では stdio トランスポート (1 クライアント / サーバ) で MCP サーバを起動する。複数クライアントを同時接続するには `--transport http --port <PORT>` (または `--bind <SOCKETADDR>`) を渡し Streamable HTTP に切り替える — 詳細は [HTTP トランスポート (複数クライアント同時接続)](#http-トランスポート-複数クライアント同時接続) 参照。loopback 外の `--bind` は、kb-mcp が認証を持たないため追加で `--i-know` が必要。

サーバは 6 つの MCP ツール (後述) を公開し、インデックスをプロセス内に保持して低レイテンシでクエリに答える。`--model` が現在の index を作ったモデルと一致しない場合、実行可能なエラーメッセージで起動を拒否する。ファイルウォッチャ (既定有効) が `--kb-path` 配下のコンテンツ変更を検知して再インデックスする — [ライブ同期 (file watcher)](#ライブ同期-file-watcher) 参照。

`--reranker` (任意、既定 `none`) はハイブリッド検索の上位候補に cross-encoder 再ランクをかける:

- `none` — 無効 (既定)
- `bge-v2-m3` — BAAI/bge-reranker-v2-m3 (多言語 100+、初回 DL 約 2.3 GB)。日本語 KB では推奨
- `jina-v2-ml` — jinaai/jina-reranker-v2-base-multilingual (多言語、約 1.2 GB)。軽量版
- `bge-base` — BAAI/bge-reranker-base (英語 / 中国語のみ、約 280 MB)。日本語では非推奨

再ランクのレイテンシコストは、CPU で `bge-v2-m3` を 50 候補に適用した場合 1 クエリあたり約 300–700 ms。`--rerank-by-default <BOOL>` (`--reranker` 指定時は既定 on) はすべての `search` 呼び出しで再ランクするかを制御する。**値を取るフラグ**なので、無効化は `--rerank-by-default=false` と書く。MCP ツール側は `rerank: Option<bool>` で per-query 上書き可能。reranker の切替に**再インデックスは不要** (index 非依存)。

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
- **macOS**: launchd LaunchAgent (`~/Library/LaunchAgents/com.kb-mcp.<name>.plist`)。daemon の出力は launchd が config home の `kb-mcp.out` / `kb-mcp.err` に書く。plist は `Umask` に `0077` を指定するので、agent が作るもの (ログ、インデックス DB) はすべて自分のアカウントからしか読めない。
- **Windows**: Task Scheduler AT_LOGON (= admin 不要、`\kb-mcp-<name>` task)。

Installer は config home を `<dirs::config_dir()>/kb-mcp/<service-name>/` に作成し、`kb-mcp.toml` (`kb_path` / `bind` 含む) を配置。base directory は `KB_MCP_CONFIG_HOME` env var で override 可能。登録される launch line はこのファイルを `--config` で名指しする (v0.20.0+) ので、daemon は working directory から探し当てたものではなく installer が書いた config を読む。詳細は [信頼する置き場所 / しない置き場所](#信頼する置き場所--しない置き場所) を参照。

非 loopback の bind (例: `0.0.0.0:3100`) は kb-mcp が認証機構を持たないため `--i-know` 明示が必要。

> **v0.7.x personal-http レシピからの移行**: `kb-mcp/examples/deployments/personal-http/` のテンプレートは v0.8.0 で削除。手動 install 済の unit を `kb-mcp service install` 実行前に削除すること:
> - Linux: `systemctl --user disable kb-mcp.service && rm ~/.config/systemd/user/kb-mcp.service`
> - macOS: `launchctl bootout gui/<uid>/com.kb-mcp.kb-mcp && rm ~/Library/LaunchAgents/com.kb-mcp.kb-mcp.plist`
> - Windows: `schtasks /End /TN '\kb-mcp' ; schtasks /Delete /TN '\kb-mcp' /F` (= `\kb-mcp` は旧 task 名に置換)
>
> 旧 `kb-mcp.toml` の設定 (`model = "bge-m3"` / `exclude_dirs` / `best_practice` / `fastembed_cache_dir` 等) を持ち越したい場合は、install 後に **新 config** (`<dirs::config_dir()>/kb-mcp/<service-name>/kb-mcp.toml`) を編集。**`kb_path` は必ず絶対パスで記述すること** — 新 daemon の `WorkingDirectory` は `config_home` なので、相対パス `kb_path = "./knowledge-base"` は `<config_home>/knowledge-base` に解決され実 KB を見失う。Windows path の backslash escape を避けるには TOML literal 文字列 (single quote) が便利: `kb_path = 'C:\Users\you\your-kb'`。

### Tray monitor (Windows only、v0.9.0+)

`kb-mcp-tray.exe` は Windows system tray に常駐する daemon 監視 binary。v0.14.0 以降は専用 archive `kb-mcp-tray-x86_64-pc-windows-msvc.zip` として配布される (`kb-mcp` の archive の中ではない)。`kb-mcp.exe` と同じディレクトリに展開すること — `kb-mcp service install --with-tray` はそこを探す。(v0.14.0 より前のリリースには**そもそも含まれていなかった**: Windows 用の companion binary 2 本はビルドされていたが release に添付されていなかった。v0.14.0 以降を使うこと)

daemon と一緒に install:

```bash
kb-mcp service install --kb-path C:\path\to\kb --with-tray
```

次回 logon で tray icon が表示され、color dot で daemon 状態を示す:

- **緑** — daemon healthy (= 直近の `/api/admin/status` polling 成功)
- **黄** — daemon が indexing 中
- **赤** — daemon 1 分以上応答なし (= 5sec interval で 12 連続失敗)
- **灰** — 初回 polling 待ち (= 起動直後 5 秒)

right-click で 6 menu items: **Status** (read-only) / **Open Web UI** / **Start** / **Stop** / **Restart** / **Quit Tray**。**Start** は scheduled task を実行、**Stop** は `/api/admin/status` が報告する pid のプロセスを終了させ (v0.14.0+)、daemon の bind アドレスを bind できることで停止を確認する — `Stop-ScheduledTask` が止めていたのは即座に終了する launcher だけで、実質何もしていなかった。

Tray log は `%LOCALAPPDATA%\kb-mcp\logs\tray.YYYY-MM-DD` (= 日次 rotation)。verbose 出力には `KB_MCP_TRAY_LOG=debug` を設定、`--debug` flag で console attach して stdout/stderr を直接見る。

daemon を uninstall すると tray shortcut も一緒に削除:

```bash
kb-mcp service uninstall kb-mcp
```

daemon と独立に tray shortcut だけ管理する subcommand:

```bash
kb-mcp service tray-install --service-name kb-mcp     # shortcut のみ追加
kb-mcp service tray-uninstall --service-name kb-mcp   # shortcut のみ削除
```

tray は `127.0.0.1:<port>/api/admin/status` を polling するので、daemon は loopback (`127.0.0.1`) または wildcard (`0.0.0.0`) で listen している必要あり。`192.168.1.5:3100` のような特定 NIC bind は loopback で listen しないため tray polling が fail (= 起動時に warning log)。

### インデックスの状態確認

```bash
kb-mcp status --kb-path /path/to/knowledge-base
```

既存 index の状態を **stderr に** 表示する (`status` は stdout に何も書かないのでパイプで受けないこと): document / chunk 数、`tags` frontmatter の parse に失敗した件数、index が構築された context mode (`static` / `off`)。品質フィルタを通過するチャンク数はもう 1 行で出るが、**実効閾値が 0 より大きいときだけ**なので、`[quality_filter] enabled = false` や `threshold = 0.0` では出力されない。

### コマンドラインからの一発検索

シェルスクリプトや skill bin が「KB をこの文字列で検索したい」だけの目的で使う用途 — MCP 接続を立ち上げずに:

```bash
kb-mcp search "RAG server comparison" --limit 3 --format text
kb-mcp search "E0382" --category deep-dive --format json | jq '.results[] | .path'
kb-mcp search "クエリ最適化" --reranker bge-v2-m3        # 呼び出し単位の再ランクも可
```

`--format` は `json` (既定、後述「検索フィルタと引用」の通り `{ results, low_confidence, filter_applied }` ラッパ) か `text` (`---` 区切りの LLM フレンドリなブロック)。他のフラグは `serve` と同じ: `--kb-path` / `--model` / `--reranker` / `--category` / `--topic` / `--limit`。品質フィルタは既定有効 — 単発クエリで フィルタ無効状態に戻すには `--include-low-quality` または `--min-quality 0` を渡す。`kb-mcp.toml` の既定値は `serve` / `index` と同じく適用される。

**クエリがどうマッチするか** (v0.16.0+): ハイブリッドの FTS 側はクエリを逐語で探すわけではない。クエリを Separator と文字種境界 (漢字 / ひらがな / カタカナ / それ以外の語構成文字) で割り、trigram 下限の 3 文字に満たない断片は隣接断片と連結し、そうしてできた phrase 群を `OR` で結んで検索する — つまり `再ランキングの評価について` は `再ランキング` / `ランキング` / `の評価` / `について` を探すので、自然文の質問がそのままの形で出現していなくてもマッチする。1 個の逐語 phrase として固めたい部分は `"..."` で囲む (`kb-mcp search '"Foundry Local" の設定'`)。クエリ全体を囲めば v0.16.0 以前の部分文字列検索がそのまま再現される。`search` MCP ツールも同じコードパスを通るので挙動は変わらない。この変更に再 index は不要。詳細は [docs/retrieval-pipeline.ja.md](./docs/retrieval-pipeline.ja.md) を参照。

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

`results[].match_spans` はクエリを分割した term がすべて ASCII の場合に `content` 内のバイトオフセットを返すため、MCP クライアント側で原文の正確な引用を作れる。span は昇順かつ**互いに重ならない**。100 span の予算は検索した term 間で分け合うので、ある term が数百回一致しても 1 回しか出ない term はハイライトされる。32 phrase 上限に当たらない限り、クエリの語順を入れ替えても同じ配列が返る (v0.18.0+、完全な契約とこの但し書きは [docs/citations.ja.md](docs/citations.ja.md))。`low_confidence` は順位ベースの flag (`top1.score / mean(top-N.score) < min_confidence_ratio`) で、閾値の既定は `1.5`。`kb-mcp.toml` の `[search].min_confidence_ratio` で全体調整、`--min-confidence-ratio` で per-query 上書き可能。

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

### Contextual Retrieval (v0.12.0+、opt-in)

各チャンクの先頭に短い context breadcrumb ―― ドキュメントタイトルと見出しの祖先パンくず (`ドキュメントタイトル > セクション > サブセクション`、` > ` 区切り) ―― を**静的に**生成して付与し、それを embedding の入力、FTS5 index (専用の第 3 列、Contextual BM25 の重み付きでスコアリング)、reranker の入力に注入する機能。Anthropic 原典の Contextual Retrieval 手法と異なり、この context は index 時にドキュメント構造だけから決定論的に生成される ―― LLM 呼び出しも追加の実行時依存も無く、通常の再 index で対応できる範囲を超える staleness も生じない。

有効化するには:

```toml
[contextual]
enabled = true
```

**既定は off** で、これは慎重さのためではなく実測された悪化が根拠になっている: 574 doc の dogfood knowledge base (bge-m3 embedding) で A/B 評価したところ、kb-mcp の実際の default パイプライン (reranker なし) では static context 注入によって retrieval が**むしろ悪化**した ―― recall@5 は 0.707 から 0.627 に低下し (-0.080)、MRR も -0.041 悪化した。短いチャンク本文のベクトル信号が、前置された breadcrumb テキストによって希釈され、かつそれを補正する後段の再スコアリングが無いためと見られる。

**reranker を併用する場合** (`--reranker bge-v2-m3`) は様相が反転する: context 注入により recall@10 のわずかな低下を除く全指標が改善した ―― recall@5 は 0.760 から 0.807、MRR は 0.848 から 0.950、nDCG@10 は 0.814 から 0.858 へ向上。cross-encoder reranker は、生の embedding/BM25 段だけでは活かしきれない追加の構造的シグナルを利用できる。

**推奨**: reranker (`--reranker bge-v2-m3` 等 / `kb-mcp.toml` の `reranker = "bge-v2-m3"`) を併用する場合に限り `[contextual] enabled = true` を有効化すること。素の default パイプラインでは off のままにする。

補足:

- 返却される検索結果の schema は**不変**。context はランキング内部のシグナルに過ぎず、`search` / `get_document` の出力には一切現れない。
- **既存**の DB でこの機能を有効化するには `kb-mcp index --force` が必要 (embedding と FTS index を context 注入込みで再構築する)。`--force` なしで config と DB の mode が食い違うと stderr に警告が出るだけで DB は現在の mode を維持する (embedding 空間が意図せず混在した index を作らないための安全策)。
- `kb-mcp status` は DB の現在の mode を `Context mode: static` / `Context mode: off` として表示する。
- context breadcrumb の生成・格納の詳細は [docs/ARCHITECTURE.ja.md](docs/ARCHITECTURE.ja.md) を参照。

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
- `--seed-strategy` — `all-chunks` (既定) はシードになった各チャンクから展開、`centroid` は平均 (L2 再正規化) した 1 個の seed ノードにまとめ、`--max-nodes` のうちその 1 個を除く全部を connection に回す。**どちらも見えるのは `--max-seed-chunks` 個までの前半だけ** (MCP ツール側の綴りは `all_chunks` / `centroid`)
- `--max-nodes` (既定 100、最大 2000 にクランプ) — 総ノード数。KNN 実行回数もこれで縛られる
- `--max-seed-chunks` (既定 32、`1..=1000` にクランプ) — シードに使う起点文書のチャンク数
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

### 索引そのものを検査する (v0.23.0+)

`kb-mcp validate` が検査するのは**文書**。`kb-mcp doctor` が検査するのは**索引**:

```bash
kb-mcp doctor --kb-path /path/to/knowledge-base
kb-mcp doctor --kb-path ... --format json | jq '.findings[]'
```

検索は 1 つの chunk について 3 つのテーブルが一致していることを前提にしている — 本文・embedding・全文検索行。**ずれてもエラーにはならない**: embedding の無い chunk は単にベクトル検索に出ず、全文検索行の無い chunk はキーワード検索に出ないだけ。これまでは full index を回して修復されるのを見るまで気付けなかった。`doctor` は直接それを問う。あわせて、MCP の resource 面が**どの索引済み文書を提示していないか、なぜか**も報告する — 現在の `[parsers].enabled` に無い拡張子 / resource read が返せるサイズを超える文書 / 以前のバージョンで索引されたため size が未記録の文書。

終了コード: `0` (報告なし) / `1` (検出あり) / `2` (実行できない — 大抵は索引が無い)。**報告するだけで修復はしない**。各検出には直し方が併記される (構造的なものはすべて `kb-mcp index` か `kb-mcp index --force`)。

> `search` / `eval` と同様、本コマンドは DB を開くので、**未適用の schema migration があれば走る**。検出結果については read-only だが、ファイルについてはそうではない。

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

### fusion パラメータを測る (`kb-mcp tune`、v0.13.0+)

`[search.fusion]` で RRF 定数と bm25 列重みを公開しているが、既定値は業界慣例値
であり、RRF は公式に「チューニング不要」とされている。自分の KB について当て推量
ではなく根拠が欲しい場合は:

```bash
kb-mcp tune --kb-path knowledge-base
```

golden query セットに対して固定グリッドを掃引し、leave-one-query-out 交差検証で
結果をガードした上で、貼り付け可能なスニペットか「既定値を維持すべき」という結論
を出力する。tune 自身は何も適用しない。なお、このパラメータが動かせるのは
**そもそも bm25 段に到達する** クエリだけなので、tune はまず pre-flight で実効 N
を報告し、0 なら exit 2 で終わる。詳細は [docs/eval.ja.md](./docs/eval.ja.md) を参照。

## Claude Code / Cursor への接続

> **デプロイ用の完全なレシピは** [`kb-mcp/examples/deployments/`](./kb-mcp/examples/deployments/) **を参照**。3 パターン (個人 stdio / NAS 共有 = 1 writer + 多 read-only / 社内 HTTP サーバ = 1 サーバ + 多クライアント) で `kb-mcp.toml` / `.mcp.json` / systemd unit までセットで揃えてある。1 マシン上で複数 Claude Code を並行させる loopback daemon が要る場合は `kb-mcp service install` を使う (v0.8.0 で旧 `personal-http` レシピを置き換えた)。下のスニペットはそれらのレシピの中核を成す stdio エントリポイント。

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

`kb-mcp index` の SHA-256 差分検出により 2 回目以降は高速 (小さな KB なら大抵 1 秒未満)。ツールペイロードを精査して編集ファイルが `$KB_PATH` 配下のときだけ再構築する、より精密なシェルスクリプトがリポジトリ同梱 — [`kb-mcp/examples/hooks/`](./kb-mcp/examples/hooks/README.ja.md) 参照。SQLite は WAL モードで動作するため、MCP サーバ起動中に hook が走っても安全。

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
# または、このマシン以外からの接続を受ける場合: --bind 0.0.0.0:3100 --i-know
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
- 既定 bind は `127.0.0.1:3100` (loopback)。**kb-mcp は認証機構を内蔵していない**ので bind アドレスが実質唯一のアクセス制御 — `--bind 0.0.0.0:3100` は信頼できるネットワークでのみ使用する。v0.17.0 以降、非 loopback の `--bind` は `--i-know` を付けないと拒否される (`kb-mcp service install` と同じ規約)。`kb-mcp.toml` の `[transport.http].bind` 由来の非 loopback bind は既存のサービス構成を壊さないよう **gate しない**。起動時の警告が出るのは Host allow-list が未設定または空のときだけで (次の 2 項目を参照)、`allowed_hosts` を明示してある構成は「意図的な公開」とみなして黙る
- rmcp の Streamable HTTP 層は Host ヘッダ検証を強制 (既定で loopback のみ) し、DNS rebinding 攻撃を防ぐ。ただし **Host 検証は認証ではない** — ポートに到達できる相手は `Host: localhost` を自由に付けられる。ブラウザ側の防御と考え、到達性はネットワーク層で絞ること
- LAN / イントラ公開時は `kb-mcp.toml` の `[transport.http].allowed_hosts` に公開ホスト名 / IP を明示する (例: `["kb.example.lan", "192.168.1.10"]`)。loopback only の default のまま 0.0.0.0 で bind すると外部リクエストは Host 検証で 403 になる — operator のミス確定なので、kb-mcp は起動時に `tracing::warn` を出して気付かせる。`allowed_hosts = []` (空配列) を渡すと Host 検証が完全に無効化され (rmcp の `disable_allowed_hosts` 相当)、非 loopback bind と組み合わせるとポートに到達できる全員に `/mcp` が開く — この組合せも起動時に警告するようにした
- サーバ内部の Mutex ベース直列化により、HTTP の並列リクエストでも embedder / DB 層では逐次処理される (`search` で目安 10 qps 程度)。本格的な並列化は将来の拡張

### Web UI と admin API (HTTP transport のみ)

`serve` を `--transport http` で動かすと、`/mcp` と `/healthz` に加えて 3 つの
route が生える。有効化の設定は無く HTTP transport があれば常に存在し、3 つとも
**loopback 限定**: middleware が peer アドレスが loopback でないリクエストを
拒否し、その後 `Host` ヘッダを loopback の別名 (`127.0.0.1` / `::1` /
`localhost`) と照合する。bind アドレスが追加されるのは **それ自体が loopback の
場合だけ** で、`0.0.0.0` に bind した時の `Host: 0.0.0.0` は意図的に拒否される
(LAN のブラウザが bind アドレス経由でこれらの route に到達しないため)。
`/mcp` 用に Host を allow-list していても、ネットワーク上の別マシンからは 403。

| Route | 中身 |
| --- | --- |
| `/ui` | 最小限の**検索ページ** (MVP placeholder、Phase 3+ で再設計予定)。クエリ入力欄が `/api/search` に post して結果を並べるだけで、daemon の状態は **表示しない** |
| `/api/search` | 上のページ用の JSON 検索。MCP の `search` ツールと同じハイブリッド検索 |
| `/api/admin/status` | daemon / indexing / watcher / KB の状態を JSON で返す。Windows tray が 5 秒間隔で polling しているのはこれ |

```bash
curl http://127.0.0.1:3100/api/admin/status
```

```json
{
  "daemon":   { "version": "0.13.1", "pid": 36400, "uptime_secs": 4210, "started_at": "2026-07-26T09:12:03Z" },
  "indexing": { "active": false, "started_at": null, "progress": null },
  "watcher":  { "active": true, "debounce_ms": 500 },
  "kb":       { "path": "/srv/kb-mcp/knowledge-base", "documents": 596, "chunks": 8878, "model": "bge-m3" },
  "config_source": "Cwd"
}
```

`/ui` は Windows tray の **Open Web UI** が開くページだが、Windows 専用ではない。
Linux / macOS では daemon が動いているマシン上でブラウザから開くか、ポートを
forward する:

```bash
ssh -L 3100:127.0.0.1:3100 kb-server.lan   # → http://127.0.0.1:3100/ui
```

これらの route を reverse proxy に **map しないこと**: proxy 自身が loopback
peer で、既定の `Host` も allow-list に載るため、`/ui` を proxy すると proxy に
到達できる相手全員にページが渡る。転送するのは `/mcp` と `/healthz` だけにする。

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
| `get_connection_graph` | ドキュメントパスを起点に意味的に関連するチャンクを BFS 展開。`parent_id` / `depth` / `score` / `snippet` 付きのノード配列を返し、呼び出し側でコンテキスト発見を連鎖させられる。上限で探索が切られた場合は `truncated` / `truncation[]` が付く | `path` (必須)、`depth` (既定 2、最大 3)、`fan_out` (既定 5、最大 20)、`min_similarity` (既定 0.3)、`seed_strategy` (`all_chunks` / `centroid`)、`dedup_by_path`、`category`、`topic`、`exclude_paths`、`max_nodes` (既定 100、最大 2000)、`max_seed_chunks` (既定 32、最大 1000) |

## MCP プロンプト

(v0.22.0+) 4 つの prompt を同梱している。クライアントはこれを**ユーザが選ぶコマンド**として出す (Claude Code では `/mcp__kb-mcp__<name>`)。存在理由は「ツールだけでは組み合わせ方が分からない」こと — `search` は「次に `get_connection_graph` を呼べ」とも「`low_confidence` が立ったらそう言え」とも言わない。

| Prompt | 引数 | 何を指示するか |
|---|---|---|
| `summarize_topic` | `topic` (必須) | `list_topics` でトピックの存在を確認 → `search` で集める → 重要な文書は `get_document` で全文を読む → 要約する。**カバーされていないこと**も書かせる |
| `deep_dive` | `question` (必須) | 最初の検索だけで答えない。上位ヒットを `get_connection_graph` の depth 2 で広げ、全文を読み、そこで得た語彙で再検索する |
| `whats_new` | `since` (任意、`YYYY-MM-DD`。省略時は 30 日前) | その日付以降の文書を概観する。**`date_from` が絞るのは frontmatter の `date` = 著者が書いた値であって、ファイルの更新時刻ではない**ことを prompt 自身に明記させ、近似であると断らせる。加えて **`date_from` は文字列として比較される**ので、`YYYY-MM-DD` 以外を渡すとエラーにならず全文書が落ちることも警告する |
| `find_gaps` | `topic` (任意) | 欠落を探す。`low_confidence` が立つ問い、`include_low_quality: true` でしか出てこない stub。**欠けているものを報告させ、内容の提案はさせない** |

4 つとも text のみで、引用規則を共有する: 使った文書の `path` を必ず引用する / `low_confidence` を握り潰さず表に出す / ナレッジベースが沈黙している時は一般知識で埋めずにそう言う。

**設定ファイルではなくコンパイル時固定にしてある。** prompt 本文はモデルに渡るテキストで、`kb-mcp.toml` は cwd や `.git` 祖先から**発見される**ため、設定で定義できるようにすると untrusted config に対して `kb_path` と同じ制限が必要になる。MCP 仕様も助けにならない — tool annotation と違い、**クライアントに「prompt の内容を信用するな」と言う指針が無い**。

## MCP リソース

(v0.22.0+) ナレッジベースを `kb://` スキームの MCP resource としても公開する。Claude Code では `@` メニューに出る。

| URI | 中身 |
|---|---|
| `kb://topic/<prefix>` | **topic group** = パスの先頭 1〜2 セグメント。indexer が `category` / `topic` を導出するのと同じ規則。read するとその配下の文書一覧 (URI 付き) が Markdown で返る。`kb://topic/` は root group |
| `kb://doc/<path>` | 索引済みの文書 1 件。列挙はせず**テンプレートとして公開**する |

`resources/list` が返すのは topic group であって、**文書 1 件ごとではない**。ナレッジベースの文書は数百でもグループは数十であり、listing は接続のたびにクライアントが取りに来るもの。個々の文書はテンプレートと、**`search` hit に付くようになった `uri`** から辿れる — spec は「listing に出ていない文書へのリンクを tool が返すこと」を明示的に許している。listing もこの `uri` も**同一の述語**から来るので、同じ文書について両者が食い違うことはない。索引に残り検索でも見つかるまま、提示だけ外れる要因が 2 つある。1 つは**現在の parser registry**: `[parsers].enabled` を狭めて再 index しないと、外した拡張子の行は索引にも検索結果にも残るが、read が拒否する以上提示しない。もう 1 つは **size** (v0.23.0+): 1 MiB を超える Markdown / テキスト文書は `resources/read` が返す量を超えるので、これも提示しない (`search` hit は残り、`uri` だけが付かない)。同じサイズでも PDF や表計算は提示され続ける — read が拒否ではなく抽出テキストを切り詰めるため。size は index 時に記録される。以前のバージョンで索引した文書は size 未記録で、**次の `kb-mcp index` まで提示されたまま**になる (その 1 回で再 embed 無しに埋まる)。件数は `kb-mcp doctor` が報告する。根拠は [ADR-0005](docs/decisions/0005-record-document-size-in-the-index.ja.md)。

区切りは forward slash のまま、それ以外は percent-encode するので、空白や非 ASCII を含むパスでも正しい ASCII URI になる。

**read は索引で縛られる。** 提供されるのは索引に入っている文書のみで、そのうえで `get_document` と同一の検査 (symlink / hardlink 拒否、path traversal、拡張子 membership、size cap、handle 束縛の read) を通す。これは `get_document` (= `kb_path` 配下で拡張子が registry にあれば返す) より**狭い**。resource は「サーバが提示したもの」なので、提示していない URI を提供するのは別の操作だから。したがって `.kb-mcpignore` された文書は resource には出ないが `get_document` からは従来どおり読める — これは [ADR-0003](docs/decisions/0003-kb-mcpignore-bounds-indexing-not-access.ja.md) の契約が不変であることの帰結。判断の正本は [ADR-0004](docs/decisions/0004-resource-reads-are-bounded-by-the-index.ja.md)。

内容はテキストとして返り、media type は**提供物の型**にする: Markdown は `text/markdown`、抽出テキストとして出すものは `text/plain`。PDF や表計算は **kb-mcp が抽出したテキスト**として返り、元のバイト列ではない。

**未実装**: `resources/subscribe` と `notifications/resources/list_changed`。これらが無くても `"resources": {}` は準拠した宣言であり、固定の topic group は滅多に変わらない。

## 補足

- **埋め込みモデル**: 初回実行時、選択した ONNX モデルが OS 標準のキャッシュディレクトリに DL される。2 回目以降は再利用。解決順:
  1. `FASTEMBED_CACHE_DIR` 環境変数 (設定されていれば)
  2. OS キャッシュ + `fastembed` (Linux: `~/.cache/fastembed`、macOS: `~/Library/Caches/fastembed`、Windows: `%LOCALAPPDATA%\fastembed`)
  3. CWD 直下の `.fastembed_cache` (最終フォールバック)
- **インデックス保存先**: SQLite DB は `--kb-path` の**親ディレクトリ**に `.kb-mcp.db` として保存される (例: `--kb-path ./knowledge-base` ならリポジトリルート)
- **Parser registry**: `[parsers].enabled` に列挙された拡張子のみインデックス対象。既定は `["md"]` (従来デフォルト)、`["md", "txt"]` で `.txt` にオプトイン (タイトルはファイル名派生)、`["md", "pdf"]` (v0.10.0+) で `.pdf` にオプトイン (詳細は下記 PDF インデックスの補足)、`["md", "docx", "xlsx", "pptx"]` (v0.11.0+) で Office ドキュメントにオプトイン (詳細は下記 Office ドキュメントインデックスの補足)。未知 id (例: `"rst"` / `"adoc"`) は起動時に拒否、空配列も「何もインデックスされない」事故防止のため拒否
- **PDF インデックス (v0.10.0+)**: `[parsers].enabled = ["md", "pdf"]` でオプトイン。[oxidize-pdf](https://crates.io/crates/oxidize-pdf) (純 Rust) でページ単位にテキストを抽出し、空でない各ページが見出し `p.N` の 1 チャンクになる。PDF の `Title` / `CreationDate` メタデータがあれば frontmatter に反映、`Title` が無ければファイル名派生タイトルに fallback する。暗号化 PDF は warning 付きで skip (パスワード対応なし)。他のバイナリ形式と同様、`.pdf` にも 50 MiB の生バイト上限が適用され、超過分は実行全体を abort せず warning 付き skip になる。既知の制限:
  - **テキストが薄すぎる PDF は落とす**: 抽出文字数の平均が 50 chars/page 未満の PDF は warning 付きで skip され、一切インデックスされない。スキャン / 画像のみの PDF (**OCR 非対応**) はここに含まれるが、それだけではない — 表紙・ラベル・図版中心のスライドは、テキストが完璧に抽出できていてもここに落ちる。閾値を下げないのは、**この判定が本来狙う相手**であるスキャン画像 + 電子的に載せたページ番号 /「CONFIDENTIAL」ヘッダだけの PDF が **39 chars/page** を出すため (2026-08-10 実測)
  - **CJK PDF は v0.15.2 以降正しく抽出できる**。予約 CMap + `/ToUnicode` 無しの CID-keyed フォント (ReportLab の出力形式) を含む。旧版でこの形が文字化けした原因は oxidize-pdf 側 — `/DescendantFonts` を CIDFont が間接参照で書かれている場合しか読まなかった — で、本プロジェクトが報告・修正した ([bzsanti/oxidizePdf#469](https://github.com/bzsanti/oxidizePdf/issues/469)、修正は oxidize-pdf 4.3.0 に収録、v0.15.2 で取り込み)。**`/ToUnicode` 付きの TrueType サブセットを埋め込む日本語 PDF — Word / LibreOffice / Google ドキュメントの出力形式 — は従来から正しく抽出できていた** (2026-08-10 実測: 密な日本語レポートで 569 chars/page)
  - **text layer が文字化けに復号された PDF は索引せず skip する** (v0.15.1+)。上記 CID 修正後は、他の原因による復号失敗への防衛層として維持している。kb-mcp は 2 つのシグナルで検出し — 抽出文字の 1% 以上が C1 制御コード U+0080–U+009F (正しく復号できたテキストには決して現れない)、および C1 を出さない清音かな主体の形を捕まえる「UTF-16BE を 1 バイトずつ読んだ交互パターン」(`あ` が `0B` になる) — どのクエリにも一致しないテキストを索引に入れることを拒否する。診断もページ密度のせいにせず復号失敗を名指しする
  - **多段組レイアウトの reading order 乱れ**: 抽出順は PDF 内部のテキスト描画順に従うため、複雑な多段組レイアウト (スライド資料等) では列が入り交じることがある。単一段組の文書は影響を受けない
  - **`Title` メタデータのゴミは filename fallback しない**: filename fallback は PDF の `Title` フィールドが空の場合のみ発火する。空ではないが無意味な自動生成タイトル (エクスポートパイプライン由来の残骸等) はそのまま使われる
  - **ハイフン結合は保守的なヒューリスティック**: 行末の `-\n` は、`-` の直前と `\n` の直後がともに ASCII 小文字の場合のみ結合する (型番・日付・CJK に隣接するハイフンを誤って壊さないため)。この結果、本来結合すべき単語分断が結合されない、あるいは偶然の小文字-小文字の並びを誤って結合してしまうケースが稀にある

  実際の日本語 PDF での dogfood (2026-07-19) で発見した `oxidize-pdf` の癖には対処済み: `/Title` が PDF 仕様の UTF-16BE 文字列形式 (非 ASCII タイトルで一般的) の場合、この依存クレートは byte-order-mark を検出できず 1 byte ずつ mis-decode して文字化けを生む。kb-mcp はこの mis-decode パターンを検知して元のタイトルに復元する。復元できない (あるいは復元結果もなお不自然な) 場合は文字化けをそのまま出さず filename fallback に倒す。抽出されたページ本文 (`content`) はそもそもこの問題の影響を受けていない — 化けるのは `title` フィールドのみだった
- **Office ドキュメントインデックス (v0.11.0+)**: `[parsers].enabled = [..., "docx", "xlsx", "pptx"]` でオプトイン。各形式とも自前実装 (LibreOffice / MS Office への依存なし):

  | 拡張子 | ライブラリ | チャンク粒度 | frontmatter 由来 |
  |---|---|---|---|
  | `.docx` | zip + [quick-xml](https://crates.io/crates/quick-xml) | Markdown と同じ規則の見出し階層セクション (`Heading1`〜`Heading6` 段落スタイルがセクション境界) | `docProps/core.xml` (Dublin Core: title / created / keywords) |
  | `.xlsx` | [calamine](https://crates.io/crates/calamine) | 空でないシートごとに 1 チャンク (見出し `Sheet: <name>`)、シートあたり 1 MiB で truncate (行単位境界 — cap 超過を招いた行はそのまま保持してから打ち切り) | `docProps/core.xml` |
  | `.pptx` | zip + quick-xml | スライドごとに 1 チャンク (見出し `Slide N: <title>`、title placeholder が無ければ `Slide N`)、発表者ノートは末尾 `[notes]` セクションとしてスライドの `.rels` 関係を解決して付加 (同番号ファイルの推測はしない = notes の誤帰属を避ける) | `docProps/core.xml` |

  既知の制限:
  - **legacy バイナリ形式は非対応**: 2007 年以前の `.doc` (Word) / `.ppt` (PowerPoint) / `.xls` (Excel) は非対応 — 対応するのは上記の OOXML 形式 (`.docx` / `.pptx` / `.xlsx`) のみ

    `.xls` は v0.11.0〜v0.13.1 でインデックス対象だったが v0.14.0 で取り下げた: calamine は workbook を開く時点で全体を密に確保し、BIFF が縛るのは**シート 1 枚**であって **workbook ではない**ため、小さな細工ファイルでメモリを使い切れる。しかも割り当て失敗はファイルの skip ではなくプロセスの異常終了になる。`[parsers].enabled` に `"xls"` を書くと起動時にこの理由付きで拒否される — `.xlsx` に変換すれば streaming で読める。**原本は残すこと**: 変換でセルのテキストは引き継がれるが一般に無損失ではない (VBA マクロは `.xlsm` が必要、その他のレガシー固有機能も失われうる)。詳しい理由: [ADR-0001](docs/decisions/0001-withdraw-xls-legacy-biff-support.ja.md)
  - **OpenDocument 形式は非対応**: `.odt` / `.ods` / `.odp` は非対応
  - **パスワード保護ファイルは復号ではなく skip**: 暗号化された Office ファイルは (zip / BIFF コンテナが開けないことで) 検出され、実行全体を失敗させず warning 付きで skip される — パスワード対応なし
  - **表構造は plain text 化される**: `.docx` / `.pptx` の表セルは通常のテキストとして読み取られる (行/列構造はチャンク内に保持されない)。`.xlsx` の行は 1 行ごとにタブ区切りで連結される。下流の検索が見るのはグリッドではなく地の文

  `.pdf` と同様、この 4 形式も 50 MiB の生バイト上限 (`MAX_RAW_BINARY_BYTES`) を indexer の size-skip guard と `get_document` の両方で共有する。
- **ライブ同期ウォッチャ**: `kb-mcp serve` は `notify` ベースの watcher を既定 spawn (`[watch].enabled = true`、500ms debounce)。手動 save / `git pull` / 外部スクリプトを MCP ツールと同じ Mutex 付きリソース上で増分再インデックスするため、同時トリガは直列化される。`--no-watch` / `[watch].enabled = false` で無効化
- **HTTP トランスポート**: `--transport http --port 3100` で rmcp の Streamable HTTP を `/mcp` に提供し、`/healthz` をプローブ用、内部は Mutex 直列化。既定 bind は `127.0.0.1:3100`、`0.0.0.0` は明示 opt-in かつ**まだ認証機構無し** — リバースプロキシ / ファイアウォール側で保護すること
- **埋め込み次元**: `--model` で決まる。BGE-small-en-v1.5 = 384、BGE-M3 = 1024。選択した次元は `vec_chunks` 仮想テーブルに宣言され `index_meta` に記録される。実行時の不一致は検出して拒否
- **増分インデックス**: ファイルは SHA-256 content hash で追跡。以降の `index` 実行では変更されたファイルのみ再 embedding される (`--force` を渡さない限り)。内容を変えずに移動 / リネームすると hash 一致で検知され `documents.path` の UPDATE として処理 — 既存の chunk / embedding / FTS 行は再利用される。再構築サマリでは `updated` / `deleted` の隣に `renamed` としてカウントされる
- **read 不能 / 非 UTF-8 ファイルへの耐性**: read 失敗・size cap 超過・parse 失敗のファイルは warning を出して skip されるだけで `index` 実行全体は abort しない — `--kb-path` にバイナリファイルが混ざっていても、それ以外の knowledge base のインデックスは壊れない
- **サイズ上限**: ファイル 1 本あたり生バイト 50 MiB を、read する前に `stat` で判定する。バイナリ形式 (`MAX_RAW_BINARY_BYTES`) だけでなく **テキスト形式 (`MAX_RAW_TEXT_BYTES`、v0.17.0 以降)** にも適用される。以前テキストは無制限で、巨大な `.md` 1 本で内容が丸ごとメモリに載った — `rebuild_index` は MCP ツールなのでクライアントから誘発できた。上限超過のファイルは、どちらの上限に当たったかを明示した warning とともに skip される
- **ハイブリッド検索 (FTS5 + ベクトル)**: `search` ツールは SQLite FTS5 全文検索 (trigram tokenizer、日本語 / CJK も動く。v0.12.0 以降は `heading` / `context` / `content` の 3 列で、bm25 では既定で `heading` を 2 倍重み) をベクトル検索と Reciprocal Rank Fusion (既定 `k = 60`) でマージする。重みと `k` は v0.13.0 以降 `[search.fusion]` で設定でき、自分の KB で動かす価値があるかは `kb-mcp tune` が測る。返される `score` は RRF スコア (大きいほど良い) で距離ではない。v0.16.0 以降、クエリは逐語で検索されるのではなく token 単位の phrase にコンパイルされて `OR` で結合される (上の「コマンドラインからの一発検索」を参照)。有効な phrase が 1 つも作れないクエリはベクトルのみにフォールバックするが、断片がすべて短いクエリはその前にクエリ全体の逐語 phrase へ fallback するので、ベクトルのみになるのは trim 後 3 文字未満 (trigram の最小値を下回る) のときだけ
- **任意の再ランク**: `--reranker <model>` を付けると上位候補が cross-encoder で再スコアされてから返る。再ランク適用時は `score` が RRF 値ではなく cross-encoder の生スコアになる。再ランクは index 非依存 — サーバ起動時に再インデックスなしでトグル可能
- **Connection graph**: `get_connection_graph` / `kb-mcp graph` はドキュメント起点でベクトルインデックス上を BFS する。追加インデックスは作らず、**展開されたノードごとに** sqlite-vec KNN を新規発行する。ANN 索引は無いので、KNN 1 回で KB の全ベクトルを走査する。

  リクエストを有限に保つ上限が 2 つあり、**どちらも発火したら自己申告する**:

  | 上限 | 既定 | 天井 | 何を縛るか |
  | --- | --- | --- | --- |
  | `max_seed_chunks` | 32 | 1000 | シードに使う起点文書のチャンク数。SQL の `LIMIT` として効くので、上限を超えた行は読まれない — ただし 1 行だけプローブとして読む (打ち切りの有無を追加クエリなしで判定するため) |
  | `max_nodes` | 100 | 2000 | 結果のノード数。各ノードは 1 度しか queue に入らず高々 1 度しか展開されないので `knn_queries <= total_nodes <= max_nodes`。この 1 つで**応答サイズと KNN 実行回数の両方**が縛られる |

  `depth` (最大 3) と `fan_out` (最大 20) は探索の**形**を決めるだけで、コストは縛らない。この上限が入る前は、BFS が起点文書の**全チャンク**を種にし、その数に上限が無かった。650 文書の KB (9,419 チャンク / BGE-M3) で最大の文書 (160 チャンク) に対し release バイナリで実測:

  | `depth` | 修正前: KNN / ノード / 実時間 | 修正後 (既定): KNN / ノード / 実時間 |
  | --- | --- | --- |
  | 1 | 160 / 767 / 約 19 s | 14 / 100 / 約 1.1 s |
  | 2 (既定) | 767 / 1997 / 約 87 s | 14 / 100 / 約 1.1 s |
  | 3 (最大) | 1997 / 3682 / 約 200 s | 14 / 100 / 約 1.1 s |

  両方を天井まで開けると (`--max-seed-chunks 1000 --max-nodes 2000`)、depth 1 と depth 2 の行は `truncated: false` で**完全に再現する** — 上限は探索を縛るだけで、探索が見つけるものを変えない。depth 3 だけは例外で、3,682 ノードは天井 2,000 を超えるため、天井での実行は 2,000 ノード / 約 59 秒 / `truncated: true` になる。`max_nodes` の天井を超える結果は誰にも取得できなくなった。

  上限が発火した時に呼び出し側が見るもの: 応答のルートに `truncated: true`、加えて `truncation` 配列に `reason` (`seed_chunks` / `node_budget`)・発火した `limit`・**その理由に対応する**対処が入る。`truncated` の意味は「**何かが失われた**」であって「カウンタが上限に達した」ではない — 予算をちょうど使い切ってフロンティアも尽きた探索は `false` を返す。`stats.seeds_used` は実際にシードになったチャンク数。CLI の text 出力も stats 行と理由ごとの `!` 行で同じ情報を出す。

  BFS は幅優先なので、予算は浅い層から先に使われる。長い文書では既定の予算が depth 1 の展開で埋まるため、**`depth` だけ上げても結果は変わらない**。予算を幅でなく深さに使いたいなら `--seed-strategy centroid` (シードノードが 1 個になり、その 1 個を除く予算が connection に回る。同じ文書で depth 2 のグラフが 24 ノード / 約 0.4 s) か、`--max-seed-chunks` / `--fan-out` を下げる。ただし `max_seed_chunks` は**読み取り**に掛かるので、`centroid` が平均するのも同じ前半だけ — 予算を空けるだけで、seed 上限が落としたチャンクは戻らない。

  実行中は DB ロックを保持するので、graph リクエストは走っている間ずっと並行検索を待たせる。上限はその時間を有限かつ予測可能にするが、単位は秒ではなくノード数である点に注意 — 上記の KB では KNN 1 回が約 72 ms で、これは KB のチャンク数と埋め込み次元に比例する。`exclude_paths` は `search` の `path_globs` / `tags_any` / `tags_all` と同じく **64 件・各 1 KiB まで**。

  スコアは L2 距離からの近似コサイン類似度 (`1 - d²/2` を `[0,1]` にクランプ、unit normalized embedding を前提 — BGE-small / BGE-M3 は内部で正規化済み)
- **見出し除外**: 見出しテキストが `exclude_headings` のいずれかを含むセクションは、チャンキング時に落とされる。既定は空リスト (全セクション残す)。`kb-mcp.toml` の `exclude_headings` に substring を列挙するとオプトインになる。マッチは部分文字列 (`heading.contains(pattern)`) で、短いパターンは `"参考リンク"` → `"## 参考リンク (旧)"` のような変種も拾う
- **ディレクトリ除外**: `walkdir` は basename が `exclude_dirs` のいずれかと一致するディレクトリ (とその subtree) をスキップする。照合は名前全体、かつ**大文字小文字を区別しない**: Unicode の小文字マッピング + ギリシャ語 final sigma の畳み込みで比較するので、`["résumé"]` は `RÉSUMÉ` に、`["οσ"]` は `ΟΣ` に一致する。full Unicode case folding ではない (`straße` と `STRASSE` は別物のまま)、また正規化もしない (結合文字で書かれた名前と合成済みの名前は別物のまま)。したがって `exclude_dirs = ["build"]` は `Build` というディレクトリも除外する — Windows / macOS ではこの 2 つは同一ディレクトリなので、完全一致にするとディスク上の綴り次第で除外設定を素通りできてしまうため。この規則は full index walk・`kb-mcp validate`・live watcher の 3 つすべてに等しく適用される。既定は `[".obsidian", ".git", "node_modules", "target", ".vscode", ".idea"]`。ユーザ指定リストは既定を完全に置き換える (merge ではない)。`exclude_dirs = []` を明示しても `.git` / `.svn` / `node_modules` は fail-safe として除外され続ける
- **`.kb-mcpignore`** (v0.21.0+): KB の**ルート**に `.kb-mcpignore` を置くと、[gitignore 構文](https://git-scm.com/docs/gitignore)でパスを除外できる — `*` / `?` / `[a-z]`、3 種類の位置の `**`、ディレクトリ限定の末尾 `/`、ルートに固定する先頭 `/`、前の行の除外を打ち消す `!`。`exclude_dirs` がディレクトリ名しか書けないのに対し、こちらはファイル単位・glob 単位で書ける: `drafts/`、`*.tmp.md`、`archive/**`、`notes/*.md` + `!notes/keep.md` など。

  読むのはこの 1 枚だけ。サブディレクトリの ignore ファイルは見ないし、`kb_path` より上へも遡らないし、`.gitignore` も見ない — git 管理の KB は「大きすぎて git に入れない、しかし索引はしたい」ファイルをちょうど gitignore していることが多く、リポジトリ側の都合で索引内容が変わらないようにするため。同じパターンが欲しければコピーすること。

  3 層は **union**: 組み込みの `.git` / `.svn` / `node_modules` fail-safe → `exclude_dirs` → このファイル。どれか 1 つでも除外と言えば除外なので、`!` が打ち消せるのは **`.kb-mcpignore` 内の前の行だけ**で、`exclude_dirs` や fail-safe が外したものは戻せない。照合は `exclude_dirs` と同じく**大文字小文字を区別しない**ので、同じファイルが Linux でも Windows / macOS でも同じ挙動になる。git と同様、除外されたディレクトリ配下のファイルは後続の `!` 行では復活できない (walk がそのディレクトリに降りないため)。

  この規則は full index walk・`kb-mcp validate`・live watcher の 3 つすべてに等しく適用される。サーバ稼働中に編集した場合、効くのは**それ以降のファイルイベント**で、すでに index に入っている文書は次の `kb-mcp index` (または MCP `rebuild_index`) まで残る — その実行時にファイルが読み直され、除外対象になった文書が落ちる。ファイルが存在するのに読めない場合 (hardlink / symlink / ディレクトリ / 64 KiB 超 / 1000 パターン超) は warning を出し、そのファイル (または超過分) 無しで続行する。起動を止めることはしない。

  **これは索引の境界であってアクセスの境界ではない**。除外されたファイルは索引されないので `search` にも `get_connection_graph` にも絶対に出ない (どちらも DB から読むだけでファイルシステムに触らない)。一方、パスを知っている呼び出し元は `get_document` で読める — `exclude_dirs` 配下のファイルが従来そうであったのと同じ。これは見落としではなく意図的な線引きで、KB に書ける者は `.kb-mcpignore` を消すこともできる以上、木の中に置いたルールがその木を守る境界にはなり得ないため。読ませたくないものは `kb_path` の外に置くこと
- **リンクは辿らない (hardlink も同じ)**: symlink は index されず、watcher も拾わず、`get_document` も返さない。KB に書ける者が「**自分では読めないファイル**を kb-mcp の権限で読ませ、`search` から回収する」ことを防ぐため。hardlink は見た目が普通のファイルのまま同じことができる — 1 つのファイルに 2 つ目の名前を付けるだけで、作成に読み取り権限は要らず、Windows では特権すら要らない — ので、**名前が 2 つ以上あるファイル**は同じ 3 箇所で同じように拒否する。拒否時はファイル名と理由をログに出す。判定は意図的に粗い: 「もう一方の名前が KB の内か外か」を portable に知る方法が無いため、**正当な hardlink (dedup したノート、2 つの KB で共有しているファイル) も同様に skip される**。index に入れたいならコピーに置き換えること。リンク数がそもそも読めない場合 (削除直後など) は通すので、削除は従来どおり index に反映される。v0.20.0 以降、**判定とバイト列は同じ handle から取る**: リンク数・ファイル種別・サイズ上限はすべて「実際に中身を読む handle」から読むので、検査を通した後にそのパスへ hardlink を rename で被せても、読まれずに拒否される。Unix ではその open が symlink を辿ることも拒否し、名前付きパイプで待たされることも防ぐ。Windows ではどちらも入れていない — symlink の作成に管理者権限が要る (実測済) 一方、reparse point を拒否すると **OneDrive の placeholder が全滅する**ため。**それでもこれは「敷居を上げる」であって境界ではない**: リンクを張った上で**元の名前を消せる**者 (= そのディレクトリへの書き込み権限。ファイルの読み取り権限は不要) は KB 側の名前だけを残せて、最初からそこにあったファイルと区別できない — リンク数は「今の状態」であって出自ではない。**途中のディレクトリ**を symlink に差し替える経路はどのプラットフォームでも塞げていない。そしてリンク数は結局ファイルシステムが答える値でしかなく、FAT32 / exFAT や大半のネットワーク共有は真偽に関わらず 1 を返すので、**USB メモリや共有ドライブ上の KB はこのガードの保護を一切受けない**。**kb-mcp に読ませたくないものは `kb_path` の外**、kb-mcp の実行ユーザが読めない場所に置くこと
- **`get_best_practice` path templates**: opt-in 機能で、使うには `kb-mcp.toml` の `[best_practice].path_templates` を設定する必要がある。各テンプレートは `{target}` をプレースホルダとして使える (例: `"best-practices/{target}/PERFECT.md"`、`"docs/{target}.md"`)。サーバはリスト順に試して `kb_path` 配下に最初に存在したファイルを返す (path traversal は拒否)。セクション省略 or `path_templates = []` の場合はツール自体は登録されるが "not configured" エラーを返すため、意図しない呼び出しは明示的に失敗する
- **チャンク単位品質フィルタ** (**既定有効** 閾値 `0.3`): インデックス時に各チャンクに対し 3 つのシグナル — 長さ (30 文字未満 → -0.6)、定型語のみ (TBD / TODO / 詳細は後述 等 → -0.5)、弱い構造 (80 文字未満の 1 行 → -0.3) — から `quality_score` を計算。閾値未満のチャンクは `search` / `kb-mcp search` / `get_connection_graph` で非表示。`get_connection_graph` の seed チャンクは免除。フィルタ無効化は `kb-mcp.toml` の `[quality_filter] enabled = false`、per-query は CLI `--include-low-quality` / MCP `include_low_quality: true`。閾値上書きは `--min-quality 0.5` / `min_quality: 0.5`。既存 index のアップグレード: 次の `kb-mcp index` 実行時に `quality_score` 列が透過的に追加され (ALTER TABLE)、1 度だけ backfill される (冪等)

## 設計判断の記録

アーキテクチャを形づくった決定 — 何を選び、どの選択肢を却下し、その代償は何だったか — は [Architecture Decision Record](docs/decisions/) として `docs/decisions/` に残している。まず [ADR-0000](docs/decisions/0000-record-decisions-as-adrs.ja.md) を読むと、何を ADR に書き、何は CHANGELOG で足りるのかが分かる。日本語版は同じディレクトリに `*.ja.md` で並べてある。
