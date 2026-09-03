# 設定ファイル

`groove.toml` のリファレンス — 受け付ける全キー、ファイルの探索順、
そのうちどの置き場所を信頼するか。

> **English version**: [configuration.md](./configuration.md)

[docs/usage.ja.md](usage.ja.md) の CLI オプションはすべて `groove.toml` で既定値を与えられる。CLI 引数は常に優先され、設定ファイルは単に同じデプロイでの記述の繰り返しを減らすためのもの。配置場所の探索順は [設定ファイルの探索順](#設定ファイルの探索順) を参照 — 最も一般的なのはプロジェクトルート (CWD) かバイナリの隣。**この 2 つは同等ではない**: groove が**見つけた**ファイルは一部しか信頼されないので、プロジェクトルートに置くなら `--config` で名指しするのがよい。[信頼する置き場所 / しない置き場所](#信頼する置き場所--しない置き場所) を参照。[`grooveseek/groove.toml.example`](https://github.com/alphabet-h/grooveseek/blob/main/grooveseek/groove.toml.example) を `groove.toml` にコピーして編集する。

**このテンプレートはコピーしても何も変わらない。** 空ではなく、ファイルの形が見えるように 12 個のセクション (`[quality_filter]` / `[best_practice]` / `[parsers]` / `[parsers.code]` / `[watch]` / `[transport]` / `[transport.http]` / `[eval]` / `[search]` / `[search.mmr]` / `[search.fusion]` / `[search.parent_retriever]`) は有効なまま残してあるが、**有効な値はすべて既定値そのもの**なので、コピーは「既定値を明示的に固定する」だけで挙動を変えない。挙動が変わる項目はすべてコメントアウトしてある。一方、下のブロックは別物で、各キーが何をするかを示すために値を入れた**説明用の例** — 既定値でない値もあれば、既定値をそのまま書いているものもある。**丸ごと貼るためのものではなく、メニューとして**読むこと:

```toml
# groove.toml (プロジェクトルート / .git 祖先 / groove の隣 のいずれかに置く)
kb_path = "/path/to/knowledge-base"
model = "bge-m3"
reranker = "bge-v2-m3"
# `groove serve` と `groove search` の両方が読む (v0.27.0+)。1 クエリだけ変えたい
# なら MCP ツールの `rerank` パラメータ、あるいはコマンドラインで `--reranker` を
# 明示する (`--reranker none` でそのクエリだけ再ランクを切れる)。
rerank_by_default = true
fastembed_cache_dir = "/home/you/.cache/huggingface/hub"

# grammar plugin の置き場 (v1.3.0+)。焼き込まれていない言語 — Rust 以外すべて —
# は自分で DL して置くライブラリで、このキーがその置き場を指す。
# `GROOVE_GRAMMAR_DIR` が優先し、そちらは絶対パス必須。
# **`[parsers].enabled` に plugin が要る言語がある時だけ読む**ので、Markdown だけの
# ナレッジベースがこの値を参照することは無い。
# 既定は `<ローカルデータディレクトリ>/groove/grammars` で、Windows なら
# `%LOCALAPPDATA%\groove\grammars`、Linux なら `~/.local/share/groove/grammars`、
# macOS なら `~/Library/Application Support/groove/grammars`。どれも決められない
# 環境では既定を持たず、plugin を必要とするコマンドがその旨を告げて止まる
# (CWD 相対には推測で落とさない)。置き方は docs/clients.ja.md を参照。
grammar_dir = "/home/you/.local/share/groove/grammars"

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
# "xlsx" / "pptx" (v0.11.0+) / "rs" (v1.2.0+)。
# ("xls" は v0.14.0 で取り下げ、behavior.ja.md 参照)
# grammar が焼き込まれていない言語 — "py" (v1.3.0+) — は plugin を先に
# 置く必要がある (clients.ja.md 参照)。
# 全部入り例:
[parsers]
enabled = ["md", "txt", "pdf", "docx", "xlsx", "pptx", "rs"]

# ソースコード専用。max_chunk_chars は 1 chunk の予算 (非空白文字数)。
# 収まる定義は 1 chunk になり、超えた定義は入れ子の定義へ、入れ子が無ければ
# 行で割る (長い関数では後者が通常)。細かい粒度が欲しければ下げ、長い本体を
# 割りたくなければ上げる。既定 3500。
[parsers.code]
max_chunk_chars = 3500

# ライブ同期ファイルウォッチャ。`groove serve` 実行中、
# kb_path 配下の変更が `debounce_ms` 窓内に検出され、該当ファイルのみ
# 増分再インデックスされる。PostToolUse hook を補完する位置付け:
# 手動編集 / `git pull` / 外部スクリプトをカバーする。CLI の
# `--no-watch` / `--debounce-ms` で上書き可能。セクション省略時は
# 既定 (enabled, 500ms debounce)。
[watch]
enabled = true
debounce_ms = 500

# `groove serve` のトランスポート。`kind = "stdio"` (既定)
# は 1 クライアント / サーバプロセス。`kind = "http"` (Streamable HTTP)
# なら `/mcp` で複数クライアント同時接続が可能。`/healthz` は 200 OK を
# 返しヘルスチェックに使える。CLI `--transport http --port 3100` で
# 上書き可能。
[transport]
kind = "http"

[transport.http]
bind = "127.0.0.1:3100"
# allowed_hosts = ["kb.example.lan", "192.168.1.10"]  # LAN 公開時に明示 (v0.5.0+)
# /mcp に対するブラウザ Origin allow-list (admin 経路には Origin 検査は無い)。
# allowed_hosts と違い **既定で有効**: MCP 仕様が Origin 検証を要求しているので、
# 省略すると bind した port の loopback origin を許可する。proxy 越しに
# ブラウザ上のクライアントから使う場合は、送られてくる**公開 origin** を
# ここに明示する。Origin を持たない要求 (通常の MCP クライアント / tray / curl)
# はどちらでも素通り。空リストは検証を無効化し、起動時に警告が出る。
# 綴りの規則が 2 つあり、どちらも見た目より厳しい。各 entry は **scheme が必須**
# (allowed_hosts は host / host:port を受けるが、こちらは受けない)。scheme の
# 無い entry は照合時に捨てられ、検証が有効なまま比較対象 0 件になって全ブラウザを
# 拒む状態になるため、groove は**起動を拒否**する。もう 1 つ、**port の無い entry は
# そのホストの全 port に一致する**ので、scheme の既定 port を意図する場合
# (https://kb.example.com = 443) を除いて port を書く。
# allowed_origins = ["https://kb.example.com"]
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

# 任意: `groove eval` (retrieval 品質評価、パワーユーザ機能)。
# モデル比較や回帰追跡のために `groove eval` を使うときだけ必要。
# セクション全体を省略するとすべて既定値で動作する。
# [eval]
# golden = ".groove-eval.yml"             # 既定: <kb_path>/.groove-eval.yml
# history_size = 10                       # 既定: 10
# k_values = [1, 5, 10]                   # 既定: [1, 5, 10]
# regression_threshold = 0.05             # 既定: 0.05

# 任意: `search` ツールのチューニング (v0.3.0+)。省略時は既定値で動作する。
# [search]
# # rank-based low_confidence 判定: top1.score / mean(top-N.score) <
# # min_confidence_ratio で flag が立つ。0.0 で判定無効。CLI
# # `--min-confidence-ratio` / MCP param `min_confidence_ratio` で per-query 上書き可。
# # 値は有限かつ >= 0.0。非有限値はどのスコアとの比較も false になり、
# # 判定をきつくするどころか無効化するため、起動時に拒否する。
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
# 自分の KB で `groove tune` が推奨しない限り触らないこと。
# [search.fusion]
# rrf_k = 60.0                # >= 1.0。小さいほど片方の検索器の 1 位を重視
# bm25_heading_weight = 2.0   # >= 0.0
# bm25_context_weight = 1.0   # >= 0.0
# bm25_content_weight = 1.0   # >= 0.0

# 任意: 静的 Contextual Retrieval (v0.12.0+)。既定 off。reranker を
# 併用しない限り悪化するため、reranker 設定時のみ有効化を推奨
# (詳細は usage.ja.md の「Contextual Retrieval」節を参照)。
# [contextual]
# enabled = true
```

この設定ファイルを置けば `groove serve` / `index` / `status` / `graph` / `search` のどれも対応フラグを省略して動かせる — **ただし groove がその置き場所を信頼する場合**。プロジェクトルートや `.git` 祖先に置く = groove が**見つけただけ**なので、見せ方に関するキーはそのまま効くが、`kb_path` / `[parsers]` / `grammar_dir` / `fastembed_cache_dir` / `[transport.http]` のゲートは安全な既定へ戻される。丸ごと効かせたいなら名指しすること — `groove --config ./groove.toml index`。どのキーがなぜ制限されるかは [信頼する置き場所 / しない置き場所](#信頼する置き場所--しない置き場所) を参照。**まず直すべきは `index`** — parser 集合が対象外にした拡張子の document を削除するため。未知のキーはタイポ対策のため拒否される。`FASTEMBED_CACHE_DIR` の実環境変数は設定ファイルの同項目より優先される。

## 設定ファイルの探索順

`groove` は起動のたびに以下の順序で `groove.toml` を探し、最初に見つかった
ものだけを使う:

| 優先 | 場所                                       | 備考                                                     |
| ---- | ------------------------------------------ | -------------------------------------------------------- |
| 1    | `--config <PATH>` (全 subcommand 共通)     | 指定したファイルが無ければエラー終了 (フォールバック禁止) |
| 2    | `./groove.toml` (CWD 直下)                 | プロジェクトローカル KB に最適 — ただし**名指しではなく発見**なので一部しか信頼されない (下記) |
| 3    | `<git-root>/groove.toml` (祖先方向に探索)  | CWD + 最大 19 祖先 (合計 20 ディレクトリ) を確認。発見であることは上と同じ |
| 4    | `<binary-dir>/groove.toml`                 | 後方互換 / グローバル install 用フォールバック。丸ごと信頼される |
| 5    | (なし — 組み込み既定値)                    | この場合 `--kb-path` を CLI で必ず指定する必要あり        |

`--config` に渡した `~` は全プラットフォームで home に展開する (`~` を展開
しない Windows `cmd.exe` でも動く)。

起動時に stderr へ `grooveseek::config: loaded config source=... path=... trust=...`
が出るので、**どの toml が効いているか**と**どこまで信用したか**をログで確認できる。

#### 信頼する置き場所 / しない置き場所

優先度 2 と 3 は**ユーザが名前を挙げていないファイル**を拾う。他人が書いた
リポジトリに `cd` した場合や、MCP クライアントがそこを cwd にしてサーバを起動した
場合、そのファイルがそのまま効いてしまう。そこで groove は**置き場所だけから**
(ファイルの中身は一切見ずに) 運用者のものかどうかを判定する:

- **信頼する**: `--config` (自分で名指しした)、`<binary-dir>` (書き込みには
  インストール先への権限が要る)、`groove service install` が使う config home、
  そしてファイルが無い場合
- **信頼しない**: それ以外の、CWD / `.git` 祖先で見つかったもの

信頼しない config も**読み込みはする**。KB の見せ方を決めるだけのもの
(`[search]` / `[quality_filter]` / `exclude_dirs` / `[watch]` /
`[contextual]`) はそのまま効く。制限するのは 5 つだけで、これらは「どのコードを
実行するか」「何が外に出るか」「誰から届くか」を決めるため:

| フィールド | 信頼しない config の場合 |
| --- | --- |
| `fastembed_cache_dir` | 警告して無視し、標準のキャッシュディレクトリを使う。どの `.onnx` を読むかを決める値であり、キャッシュに既にあるモデルは検証されないため (関連: `FASTEMBED_CACHE_DIR` は絶対パス必須で、モデルディレクトリが CWD 相対に解決されることは無い) |
| `[transport.http].bind` | 非 loopback ならポートを保ったまま `127.0.0.1` に降格 (警告つき)。`allowed_hosts` / `allowed_origins` / `healthz_public` / `max_sessions` は破棄する — 前 3 つは loopback 限定の既定に戻し、4 つ目は組み込みの既定に戻す (植えられた `max_sessions = 1` で「2 人目が繋げないサーバ」を他人に作らせないため)。`allowed_origins` の破棄は両方向に効く — 植えられたリストは攻撃者の origin を名指しできるし、**空リストは「Origin を検証しない」の意味**になるため。`kind` は尊重する |
| `kb_path` | ファイルシステムのルート / ホームディレクトリ / その祖先 / config ファイルのあるディレクトリの祖先 を指していれば**警告して無視**。`--kb-path` は従来どおり効くので上書きでき、どちらも無ければ通常どおり「`--kb-path` is required」で停止する |
| `grammar_dir` | 警告して無視し、標準の置き場を使う。プロセスへ `dlopen` されるネイティブライブラリを選ぶ値であり、grammar plugin はデータではなくコードであるため。**キーの有無に関わらず必ず設定する** — 書かないことで選択に影響できてしまうため。標準の置き場が決められない場合は代わりにキーを落とし、plugin を必要とするコマンドが `GROOVE_GRAMMAR_DIR` を案内して停止する |
| `[parsers]` | 警告して無視し、既定の集合 (Markdown のみ) を使う。`enabled` は**そもそもどの parser を走らせるか**を決めるので、KB の隣で見つかった config が、運用者が外していた最も入力面の広い形式 (`pdf` / `xlsx` / `pptx` / `docx`) を再有効化したり、grammar plugin が `dlopen` される言語を名指ししたりできてしまう。`grammar_dir` が向きだけを決めているスイッチがこちら — 有効な言語が plugin を必要としなければ、plugin は探されない。上 2 つと違い**キーが無い場合の差し替えは不要** — `[parsers]` を省略した時点で Markdown のみに落ちており、この規則が行き着く先と同じだから。`[parsers.code]` も一緒に落ちる (設定する対象の parser が残らないため) |

`kb_path` の規則は「閉じ込め」ではなく「境界弾き」で、`kb_path = "./docs"` も
`kb_path = "/srv/kb/knowledge-base"` も通る (project-local な `groove.toml` に
絶対パスを書く使い方はそのまま)。塞ぐのは**環境を知らなくても書ける**指定 —
`../..` / `/` / `C:\Users` と、それらを指す symlink。

全部効かせたいなら名指しする: `groove serve --config ./groove.toml`。

インストール済みサービスは影響を受けない。v0.20.0 以降、`groove service install` は
登録する unit / plist / scheduled task に `--config <config home>/groove.toml` を
書き込む。daemon は config を「探す」のではなく「名指しされる」ので、起動時の環境が
どうであっても信頼される。これで唯一の例外だったケース —
**`GROOVE_CONFIG_HOME` を `service install` の時だけ設定した場合**、その値は daemon
実行時の環境には無いため信頼されなかった — が塞がる。

旧版で登録したサービスは launch line が古いままになる。更新するには**自分が使った**
`groove service install` コマンドに `--force` を足して再実行すること (素の
`service install` は service 名 / auto-start / bind を既定値に戻してしまう)。

**`GROOVE_CONFIG_HOME` を最初に設定したなら、再実行時にも設定すること。** この値は
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
config ファイルではなくコマンドライン全体を握っている。groove 側の規則では
どうにもならず、そこは MCP クライアントの承認プロンプトの領分。

### 例: プロジェクトに同梱する per-project KB

```jsonc
// repo-root/.mcp.json
{
  "mcpServers": {
    "kb": { "command": "groove", "args": ["serve", "--config", "./groove.toml"] }
  }
}
```

`groove.toml` を `.mcp.json` の隣にコミットしておけば、Claude Code が
プロジェクトを開いた時点で `groove serve` がリポジトリルートから起動し、
`--config` がすぐ隣にあるそのファイルを名指しする。

**名指しすることが「効かせる」ということ。** 省いても CWD 探索はそのファイルを
見つけるが、**信頼しない config として扱う** — まさにこの節が扱っている形
(自分で指した config ではなく、groove が見つけた config) だからだ。
Markdown 以外のプロジェクト KB は Markdown だけとして提供されることになる。
この config を読む他の `groove` コマンドにも同じ引数が要り、**特に `index`**:
訪れなかった document を削除するので、既定の parser 集合で再構築すると
Markdown 以外が索引から消える。

### 例: 1 セッションで複数 KB を併用

```jsonc
{
  "mcpServers": {
    "kb-personal": { "command": "groove", "args": ["serve", "--config", "~/kb/personal/groove.toml"] },
    "kb-project":  { "command": "groove", "args": ["serve", "--config", "./groove.toml"] },
    "kb-rust-docs":{ "command": "groove", "args": ["serve", "--config", "~/kb/rust-docs/groove.toml"] }
  }
}
```

各エントリは独立した MCP サーバとして動き、それぞれ自分の `groove.toml` と
`.groove.db` を持つ。Claude からは MCP サーバ名で source を識別できる。

## Related

- `docs/usage.ja.md` — ここで既定値を与える CLI フラグ
- `docs/behavior.ja.md` — 索引まわりのキーが実際に何をするか
- `README.ja.md` — インストールとクイックスタート
