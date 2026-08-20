# 使い方

`groove` コマンドのリファレンス — `index` / `status` / `serve` / `search` /
`graph` / `validate` / `doctor` / `eval` / `tune` / `service`。

> **English version**: [usage.md](./usage.md)

## 検索インデックスの構築 / 再構築

```bash
groove index --kb-path /path/to/knowledge-base
groove index --kb-path /path/to/knowledge-base --force   # 完全再インデックス
groove index --kb-path /path/to/knowledge-base --model bge-m3 --force  # BGE-M3 (1024 dim、多言語) に切替
```

指定ディレクトリ配下のソースファイルを走査し、既定の `exclude_dirs` セット (`.obsidian` / `.git` / `node_modules` / `target` / `.vscode` / `.idea` — [docs/behavior.ja.md](behavior.ja.md) の「ディレクトリ除外」参照) をスキップする。既定では `.md` のみ取り込み。`groove.toml` に `[parsers].enabled = ["md", "txt"]` を追加すると `.txt` もインデックス対象になる (タイトルはファイル名から派生: `deep-dive-2026.txt` → `"deep dive 2026"`、本文全体が 1 チャンク)。前回実行時と content hash が変わっていないファイルは `--force` を渡さない限りスキップされる。

`--model` が受け付ける値:
- `bge-small-en-v1.5` (既定) — 384 次元、英語特化、初回 DL 約 130 MB
- `bge-m3` — 1024 次元、多言語 (100+ 言語、日本語含む)、初回 DL 約 2.3 GB。日本語主体の KB ではこちら推奨

既存インデックスでのモデル切替には `--force` が必須 (DB の `index_meta` テーブルにモデル / 次元が記録されており、不一致時は起動が拒否される)。

### 進捗出力フラグ (v0.7.8+)

`groove index` の進捗表示を切り替える 2 フラグ。**相互排他** + フラグなしの既定動作は不変 (= 既存の per-file `  indexed: foo.md (N chunks)` 出力をそのまま維持)。

- `--quiet`: 各ファイルごとの出力を抑止し、開始 / `Found N source files` / `Done in ...` のサマリ 3 行のみ。harness (Claude Code Bash tool 等) では子 process の streaming 出力が exit まで集約 buffer されるため、`--quiet` で「無音 = 進行中」と認識可能。ハングと進行中の混同を防ぐ。
- `--progress`: 進捗 UI を表示。stderr の `IsTerminal` で自動分岐 — TTY なら `indicatif` バー (経過時間 / 件数 / % / ETA)、非 TTY (pipe / redirect) なら `Progress: N/M (P%)` 行を約 20 回 + 100% アンカー 1 回で flush。`tail -f indexing.log` で監視可能。

```bash
groove index --kb-path ./big-kb --quiet         # 完了まで silence
groove index --kb-path ./big-kb --progress      # TTY ではバー、pipe では定期行
```

### モデル選択のトレードオフ

| 観点 | BGE-small-en-v1.5 | BGE-M3 |
|---|---|---|
| 初回 DL | 約 130 MB | 約 2.3 GB |
| 埋め込み次元 | 384 | 1024 (index ファイルが約 2.6 倍) |
| 実行時 RAM | 約 500 MB | 約 2 GB |
| index ビルド時間 | baseline | CPU 推論で約 3–10 倍遅い |
| 日本語精度 | 低い (英語中心語彙) | 強い (多言語 tokenizer + 訓練) |
| 英語精度 | 強い | 同等 |

モデル切替コスト (既存 index → 新モデル):

1. `groove index --kb-path ... --model <new> --force` で完全再 embedding (増分更新不可: `documents`/`chunks`/`vec_chunks` を全削除してやり直す)
2. 以降の `serve` / `index` はすべて同じ `--model` を渡す (または `groove.toml` に書く)。不一致は `index_meta` チェックで起動拒否

実務的な推奨: 最初に KB の**主要言語**に合うモデルを選び、具体的な精度問題が無い限りモデル間でブレない — 完全再 embedding が最も重いステップだから。

## MCP サーバの起動

```bash
groove serve --kb-path /path/to/knowledge-base
groove serve --kb-path /path/to/knowledge-base --model bge-m3   # index 時と一致必須
groove serve --kb-path ... --model bge-m3 --reranker bge-v2-m3  # + cross-encoder 再ランク
groove serve --kb-path ... --transport http --port 3100         # HTTP、複数クライアント
groove serve --kb-path ... --no-watch                           # ライブ同期無効
```

既定では stdio トランスポート (1 クライアント / サーバ) で MCP サーバを起動する。複数クライアントを同時接続するには `--transport http --port <PORT>` (または `--bind <SOCKETADDR>`) を渡し Streamable HTTP に切り替える — 詳細は [HTTP トランスポート (複数クライアント同時接続)](clients.ja.md#http-トランスポート-複数クライアント同時接続) 参照。loopback 外の `--bind` は、groove が認証を持たないため追加で `--i-know` が必要。

サーバは 6 つの MCP ツール ([docs/mcp-tools.ja.md](mcp-tools.ja.md)) を公開し、インデックスをプロセス内に保持して低レイテンシでクエリに答える。`--model` が現在の index を作ったモデルと一致しない場合、実行可能なエラーメッセージで起動を拒否する。ファイルウォッチャ (既定有効) が `--kb-path` 配下のコンテンツ変更を検知して再インデックスする — [ライブ同期 (file watcher)](clients.ja.md#ライブ同期-file-watcher) 参照。

`--reranker` (任意、既定 `none`) はハイブリッド検索の上位候補に cross-encoder 再ランクをかける:

- `none` — 無効 (既定)
- `bge-v2-m3` — BAAI/bge-reranker-v2-m3 (多言語 100+、初回 DL 約 2.3 GB)。日本語 KB では推奨
- `jina-v2-ml` — jinaai/jina-reranker-v2-base-multilingual (多言語、約 1.2 GB)。軽量版
- `bge-base` — BAAI/bge-reranker-base (英語 / 中国語のみ、約 280 MB)。日本語では非推奨

再ランクのレイテンシコストは、CPU で `bge-v2-m3` を 50 候補に適用した場合 1 クエリあたり約 300–700 ms。`--rerank-by-default <BOOL>` (`--reranker` 指定時は既定 on) はすべての `search` 呼び出しで再ランクするかを制御する。**値を取るフラグ**なので、無効化は `--rerank-by-default=false` と書く。MCP ツール側は `rerank: Option<bool>` で per-query 上書き可能。reranker の切替に**再インデックスは不要** (index 非依存)。

v0.27.0 以降、`rerank_by_default` は `groove search` も読む。同じ `groove.toml` で「CLI は再ランクするがサーバはしない」という食い違いは起きない。コマンドライン側の per-query 上書きは `--reranker` そのもので、モデルを明示すればファイルが `false` でもそのクエリだけ再ランクし、`--reranker none` ならそのクエリだけ切れる (`groove eval` は意図的にこのキーを読まない — 測るのは `--reranker` が選んだパイプラインで、run fingerprint もそのモデルを記録するため、黙って別物を測ってはいけない)。

### 再ランクを有効にすべきケース

再ランクは精度とレイテンシのトレードオフ。使用パターン次第:

| シナリオ | 推奨 |
|---|---|
| 対話的エージェントフロー (LLM が 1 ターンで 2–5 回 `search` を呼ぶ) | **切っておく**。+500 ms × N が積もって重くなる。BGE-M3 + 見出し加重 bm25 の検索品質で大抵十分 |
| 精度重視の単発クエリ (調査・定義的回答) | **有効化**。レイテンシ税は 1 ターンに 1 回、cross-encoder が意味的に関連する候補を明確に前に出す |
| 混在 | `rerank_by_default = false` で始め、呼び出し側が個別に選べるようにする — MCP ツールの `rerank: true`、コマンドラインなら `--reranker <model>` |

再ランクを入れるべきサイン:

- トップ 5 が明白な正解チャンクを外すことが多い (クエリ言い換えをしても)
- インデックス側の表現と同義語 / 言い換え関係にあるクエリが失敗する (例: 日本語「バグ」 vs 英語 "error")
- エージェントが 1 ターンで何度も再クエリし、間違ったヒットを読むためにコンテキストを浪費している

再ランクは index 非依存なので、1 週間試して品質差を測り、見えなければ無効化してよい — 再インデックス不要。

## groove を OS サービスとして登録 (v0.8.0+)

`groove service install` で daemon を OS のユーザレベルサービスとして登録し、ログイン時の auto-start を設定できる (admin / sudo 不要)。

```bash
# デフォルト: service name 'groove'、bind 127.0.0.1:3100、auto-start ON
groove service install --kb-path /path/to/your-kb

# Multi-instance (= 複数 KB を別サービスとして実行)
groove service install --service-name work --kb-path /path/to/work-kb --bind 127.0.0.1:3100
groove service install --service-name personal --kb-path /path/to/personal-kb --bind 127.0.0.1:3101

# 確認 / 管理
groove service status                              # default 'groove'
groove service status --service-name personal      # 名前付き instance
groove service list                                # 全 instance
groove service uninstall --service-name personal               # unit のみ削除、config + DB 残す
groove service uninstall --service-name personal --purge --yes # config + DB も削除
```

OS 別バックエンド:
- **Linux**: systemd-user (`~/.config/systemd/user/groove-<name>.service`)。ログアウト後も daemon を生かしたい場合は `sudo loginctl enable-linger $USER` を実行。
- **macOS**: launchd LaunchAgent (`~/Library/LaunchAgents/com.groove.<name>.plist`)。daemon の出力は launchd が config home の `groove.out` / `groove.err` に書く。plist は `Umask` に `0077` を指定するので、agent が作るもの (ログ、インデックス DB) はすべて自分のアカウントからしか読めない。
- **Windows**: Task Scheduler AT_LOGON (= admin 不要、`\groove-<name>` task)。

Installer は config home を `<dirs::config_dir()>/groove/<service-name>/` に作成し、`groove.toml` (`kb_path` / `bind` 含む) を配置。base directory は `GROOVE_CONFIG_HOME` env var で override 可能。登録される launch line はこのファイルを `--config` で名指しする (v0.20.0+) ので、daemon は working directory から探し当てたものではなく installer が書いた config を読む。詳細は [信頼する置き場所 / しない置き場所](configuration.ja.md#信頼する置き場所--しない置き場所) を参照。

非 loopback の bind (例: `0.0.0.0:3100`) は groove が認証機構を持たないため `--i-know` 明示が必要。

> **v0.7.x personal-http レシピからの移行**: `grooveseek/examples/deployments/personal-http/` のテンプレートは v0.8.0 で削除。手動 install 済の unit を `groove service install` 実行前に削除すること:
> - Linux: `systemctl --user disable groove.service && rm ~/.config/systemd/user/groove.service`
> - macOS: `launchctl bootout gui/<uid>/com.groove.groove && rm ~/Library/LaunchAgents/com.groove.groove.plist`
> - Windows: `schtasks /End /TN '\groove' ; schtasks /Delete /TN '\groove' /F` (= `\groove` は旧 task 名に置換)
>
> 旧 `groove.toml` の設定 (`model = "bge-m3"` / `exclude_dirs` / `best_practice` / `fastembed_cache_dir` 等) を持ち越したい場合は、install 後に **新 config** (`<dirs::config_dir()>/groove/<service-name>/groove.toml`) を編集。**`kb_path` は必ず絶対パスで記述すること** — 新 daemon の `WorkingDirectory` は `config_home` なので、相対パス `kb_path = "./knowledge-base"` は `<config_home>/knowledge-base` に解決され実 KB を見失う。Windows path の backslash escape を避けるには TOML literal 文字列 (single quote) が便利: `kb_path = 'C:\Users\you\your-kb'`。

## Tray monitor (Windows only、v0.9.0+)

`groove-tray.exe` は Windows system tray に常駐する daemon 監視 binary。v0.14.0 以降は専用 archive `groove-tray-x86_64-pc-windows-msvc.zip` として配布される (`groove` の archive の中ではない)。`groove.exe` と同じディレクトリに展開すること — `groove service install --with-tray` はそこを探す。(v0.14.0 より前のリリースには**そもそも含まれていなかった**: Windows 用の companion binary 2 本はビルドされていたが release に添付されていなかった。v0.14.0 以降を使うこと)

daemon と一緒に install:

```bash
groove service install --kb-path C:\path\to\kb --with-tray
```

次回 logon で tray icon が表示され、color dot で daemon 状態を示す:

- **緑** — daemon healthy (= 直近の `/api/admin/status` polling 成功)
- **黄** — daemon が indexing 中
- **赤** — daemon 1 分以上応答なし (= 5sec interval で 12 連続失敗)
- **灰** — 初回 polling 待ち (= 起動直後 5 秒)

right-click で 6 menu items: **Status** (read-only) / **Open Web UI** / **Start** / **Stop** / **Restart** / **Quit Tray**。**Start** は scheduled task を実行、**Stop** は `/api/admin/status` が報告する pid のプロセスを終了させ (v0.14.0+)、daemon の bind アドレスを bind できることで停止を確認する — `Stop-ScheduledTask` が止めていたのは即座に終了する launcher だけで、実質何もしていなかった。

Tray log は `%LOCALAPPDATA%\groove\logs\tray.YYYY-MM-DD` (= 日次 rotation)。verbose 出力には `GROOVE_TRAY_LOG=debug` を設定、`--debug` flag で console attach して stdout/stderr を直接見る。

daemon を uninstall すると tray shortcut も一緒に削除:

```bash
groove service uninstall --service-name groove
```

daemon と独立に tray shortcut だけ管理する subcommand:

```bash
groove service tray-install --service-name groove     # shortcut のみ追加
groove service tray-install --service-name groove --force   # 既存 shortcut を上書き
groove service tray-uninstall --service-name groove   # shortcut のみ削除
```

tray は `127.0.0.1:<port>/api/admin/status` を polling するので、daemon は loopback (`127.0.0.1`) または wildcard (`0.0.0.0`) で listen している必要あり。`192.168.1.5:3100` のような特定 NIC bind は loopback で listen しないため tray polling が fail (= 起動時に warning log)。

## インデックスの状態確認

```bash
groove status --kb-path /path/to/knowledge-base
```

既存 index の状態を **stderr に** 表示する (`status` は stdout に何も書かないのでパイプで受けないこと): document / chunk 数、`tags` frontmatter の parse に失敗した件数、index が構築された context mode (`static` / `off`)。品質フィルタを通過するチャンク数はもう 1 行で出るが、**実効閾値が 0 より大きいときだけ**なので、`[quality_filter] enabled = false` や `threshold = 0.0` では出力されない。

## コマンドラインからの一発検索

シェルスクリプトや skill bin が「KB をこの文字列で検索したい」だけの目的で使う用途 — MCP 接続を立ち上げずに:

```bash
groove search "RAG server comparison" --limit 3 --format text
groove search "E0382" --category deep-dive --format json | jq '.results[] | .path'
groove search "クエリ最適化" --reranker bge-v2-m3        # 呼び出し単位の再ランクも可
```

`--format` は `json` (既定、後述「検索フィルタと引用」の通り `{ results, low_confidence, filter_applied }` ラッパ) か `text` (`---` 区切りの LLM フレンドリなブロック)。他のフラグは `serve` と同じ: `--kb-path` / `--model` / `--reranker` / `--category` / `--topic` / `--limit`。品質フィルタは既定有効 — 単発クエリで フィルタ無効状態に戻すには `--include-low-quality` または `--min-quality 0` を渡す。`groove.toml` の既定値は `serve` / `index` と同じく適用される。

**クエリがどうマッチするか** (v0.16.0+): ハイブリッドの FTS 側はクエリを逐語で探すわけではない。クエリを Separator と文字種境界 (漢字 / ひらがな / カタカナ / それ以外の語構成文字) で割り、trigram 下限の 3 文字に満たない断片は隣接断片と連結し、そうしてできた phrase 群を `OR` で結んで検索する — つまり `再ランキングの評価について` は `再ランキング` / `ランキング` / `の評価` / `について` を探すので、自然文の質問がそのままの形で出現していなくてもマッチする。1 個の逐語 phrase として固めたい部分は `"..."` で囲む (`groove search '"Foundry Local" の設定'`)。クエリ全体を囲めば v0.16.0 以前の部分文字列検索がそのまま再現される。`search` MCP ツールも同じコードパスを通るので挙動は変わらない。この変更に再 index は不要。詳細は [docs/retrieval-pipeline.ja.md](retrieval-pipeline.ja.md) を参照。

典型的な skill-bin 用途: Claude Code の skill が `bin/` に `groove.exe` + `groove.toml` を同梱し、`groove search "<user_query>" --format text --limit 3` のようなコマンドで LLM が引用するための参照抜粋を返す。

## 検索フィルタと引用 (v0.3.0+)

v0.3.0 から `search` MCP ツールの戻り値が単なるヒット配列ではなくラッパオブジェクトになる。**これは破壊的変更**で、`Vec<SearchHit>` を直接 parse しているクライアントは更新が必要:

```jsonc
{
  "results":        [{ "score": 0.83, "path": "...", "match_spans": [...], "tags": [...], ... }],
  "low_confidence": false,
  "filter_applied": { /* デフォルトと異なるフィルタだけ echo back、フィルタ無しなら空 object */ }
}
```

`results[].match_spans` はクエリを分割した term がすべて ASCII の場合に `content` 内のバイトオフセットを返すため、MCP クライアント側で原文の正確な引用を作れる。span は昇順かつ**互いに重ならない**。100 span の予算は検索した term 間で分け合うので、ある term が数百回一致しても 1 回しか出ない term はハイライトされる。32 phrase 上限に当たらない限り、クエリの語順を入れ替えても同じ配列が返る (v0.18.0+、完全な契約とこの但し書きは [docs/citations.ja.md](citations.ja.md))。`low_confidence` は順位ベースの flag (`top1.score / mean(top-N.score) < min_confidence_ratio`) で、閾値の既定は `1.5`。`groove.toml` の `[search].min_confidence_ratio` で全体調整、`--min-confidence-ratio` で per-query 上書き可能。

入力境界 (防御的、v0.6.0+): `query` は 1 KiB 上限、超過時は `ErrorResponse` で reject。`match_spans` は 256 KiB 以下の chunk にのみ計算、上限 100 span/chunk。乱用防止が目的で正常用途には影響しない — 通常 chunk は十分上限以下。

v0.3.0 で `search` ツール / CLI に追加されたフィルタ:

```bash
groove search "tokio spawn" \
  --path-glob "docs/**" --path-glob "!docs/draft/**" \
  --tag-any rust,async \
  --date-from 2026-01-01 \
  --min-confidence-ratio 1.5
```

- `--path-glob <PATTERN>` (繰り返し可) — パス glob によるフィルタ。`!` 始まりは exclude。**1 パターンにつき 1 回**渡す: glob の構文自体がカンマを使う (`docs/{a,b}/**` は 2 つではなく 1 つのパターン) ので、**カンマでは区切らない**。MCP param: `path_globs`
- `--tag-any <a,b,c>` — チャンクが**いずれか**のタグを持つときのみ通過。MCP param: `tags_any`
- `--tag-all <a,b,c>` — チャンクが**すべての**タグを持つときのみ通過。MCP param: `tags_all`
- `--date-from <YYYY-MM-DD>` / `--date-to <YYYY-MM-DD>` — 辞書順比較。どちらかが指定された場合、`date` 未設定のチャンクは厳密に除外される。MCP params: `date_from` / `date_to`
- `--min-confidence-ratio <N>` — `low_confidence` 閾値の per-query 上書き。**有限かつ `>= 0.0`** であること。判定を切るのは `0.0`。それ以外の値は CLI がモデル読み込みの前に弾く — 非有限値はどのスコアと比較しても false になり、**閾値をきつくしたつもりが判定そのものを黙って無効化する**ため。MCP の同名パラメータは会話の途中で値を拒めないので、**弾かずに置き換える**: 非有限値は warn してサーバ既定値に戻し、負値は `0.0` に clamp する (= 呼び出しを失敗させる代わりに判定を切る)

CLI `groove search --format json` のラッパ (`results` / `low_confidence` / `filter_applied`) は MCP と同じで、hit のフィールドも 1 点を除いて同じ: **MCP の hit はサーバが引き渡せる文書のとき `uri` を持つ**が、CLI の hit は持たない。`uri` が付く条件は [docs/mcp-tools.ja.md](mcp-tools.ja.md) 参照。`match_spans` / byte offset の詳細は [docs/citations.ja.md](citations.ja.md)、フィルタの完全リファレンスは [docs/filters.ja.md](filters.ja.md) 参照。

## 多様性 (MMR) と parent retriever (v0.7.0+)

retrieval 品質を上げるための任意の knob を 2 つ追加。両者は独立しており、片方だけ on / 両方 on / 両方 off いずれでも動く。**既定は両方 off** なので既存パイプラインの挙動は変わらない。

```bash
# MMR (多様性再ランク)
groove search "tokio runtime" --mmr true --mmr-lambda 0.7

# Parent retriever (短い chunk を隣接 sibling や全文に展開)
groove search "k=60 in RRF" --parent-retriever true

# 両方同時
groove search "context management" --mmr true --parent-retriever true
```

CLI フラグ (`groove eval` も同じものを受け付ける):

- `--mmr <bool>` — MMR 多様性再ランクを有効化。既定 `false`
- `--mmr-lambda <0..1>` — MMR の関連度と多様性のバランス。`1.0` で「多様性なし」(= MMR off と等価)、低くすると探索寄り (重複の少ない候補を優先)。既定 `0.7`
- `--mmr-same-doc-penalty <0..1>` — 既選択チャンクと同一 document に属する候補へ追加コストを乗せる係数。`0.0` で純 MMR、上げると同 doc chunk を積極的に除外。既定 `0.0`
- `--parent-retriever <bool>` — ヒットチャンクの token_count が `whole_doc_threshold_tokens` 未満のとき、`content` を隣接 sibling (level 一致を優先) もしくはドキュメント全体 (極端に短いチャンクの fallback) に拡張する。score / rank / path / `match_spans` は変えず、`content` と新しい optional `expanded_from` のみ変化。既定 `false`

MCP `search` ツールも同名の per-call params (`mmr` / `mmr_lambda` / `mmr_same_doc_penalty` / `parent_retriever`) を受ける。toml 既定値は `[search.mmr]` / `[search.parent_retriever]` ([docs/configuration.ja.md](configuration.ja.md))。優先順位は per-call > toml > built-in defaults。

パイプライン順序は **`RRF → reranker → MMR → parent retriever → match_spans`**。MMR は reranker score を保ったまま並べ替え、parent retriever は最後に走るので展開 content が relevance signal を汚さない。完全な解説とチューニング指針は [docs/retrieval-pipeline.ja.md](retrieval-pipeline.ja.md) 参照。

## Contextual Retrieval (v0.12.0+、opt-in)

各チャンクの先頭に短い context breadcrumb ―― ドキュメントタイトルと見出しの祖先パンくず (`ドキュメントタイトル > セクション > サブセクション`、` > ` 区切り) ―― を**静的に**生成して付与し、それを embedding の入力、FTS5 index (専用の第 3 列、Contextual BM25 の重み付きでスコアリング)、reranker の入力に注入する機能。Anthropic 原典の Contextual Retrieval 手法と異なり、この context は index 時にドキュメント構造だけから決定論的に生成される ―― LLM 呼び出しも追加の実行時依存も無く、通常の再 index で対応できる範囲を超える staleness も生じない。

有効化するには:

```toml
[contextual]
enabled = true
```

**既定は off** で、これは慎重さのためではなく実測された悪化が根拠になっている: 574 doc の dogfood knowledge base (bge-m3 embedding) で A/B 評価したところ、groove の実際の default パイプライン (reranker なし) では static context 注入によって retrieval が**むしろ悪化**した ―― recall@5 は 0.707 から 0.627 に低下し (-0.080)、MRR も -0.041 悪化した。短いチャンク本文のベクトル信号が、前置された breadcrumb テキストによって希釈され、かつそれを補正する後段の再スコアリングが無いためと見られる。

**reranker を併用する場合** (`--reranker bge-v2-m3`) は様相が反転する: context 注入により recall@10 のわずかな低下を除く全指標が改善した ―― recall@5 は 0.760 から 0.807、MRR は 0.848 から 0.950、nDCG@10 は 0.814 から 0.858 へ向上。cross-encoder reranker は、生の embedding/BM25 段だけでは活かしきれない追加の構造的シグナルを利用できる。

**推奨**: reranker (`--reranker bge-v2-m3` 等 / `groove.toml` の `reranker = "bge-v2-m3"`) を併用する場合に限り `[contextual] enabled = true` を有効化すること。素の default パイプラインでは off のままにする。

補足:

- 返却される検索結果の schema は**不変**。context はランキング内部のシグナルに過ぎず、`search` / `get_document` の出力には一切現れない。
- **既存**の DB でこの機能を有効化するには `groove index --force` が必要 (embedding と FTS index を context 注入込みで再構築する)。`--force` なしで config と DB の mode が食い違うと stderr に警告が出るだけで DB は現在の mode を維持する (embedding 空間が意図せず混在した index を作らないための安全策)。
- `groove status` は DB の現在の mode を `Context mode: static` / `Context mode: off` として表示する。
- context breadcrumb の生成・格納の詳細は [docs/ARCHITECTURE.ja.md](ARCHITECTURE.ja.md) を参照。

## 起点ドキュメントからの Connection Graph

単一ドキュメントではなく「その近傍 (さらにその近傍)」を意味的に探索したいときは `graph` サブコマンド:

```bash
groove graph --start deep-dive/mcp/overview.md --depth 2 --fan-out 5
groove graph --start notes/rag.md --dedup-by-path --format text
groove graph --start a.md --exclude-paths junk1.md,junk2.md --min-similarity 0.5
```

フラグ:

- `--start PATH` — 必須、index 済みドキュメントの相対パス。MCP param: `start`
- `--depth` (既定 2、最大 3 にクランプ) — BFS のホップ数
- `--fan-out` (既定 5、最大 20 にクランプ) — ホップあたりのノード隣接数。`0` なら seed のみ返却
- `--min-similarity` (既定 0.3) — コサイン類似度カットオフ。`0.0..=1.0`
- `--seed-strategy` — `all-chunks` (既定) はシードになった各チャンクから展開、`centroid` は平均 (L2 再正規化) した 1 個の seed ノードにまとめ、`--max-nodes` のうちその 1 個を除く全部を connection に回す。**どちらも見えるのは `--max-seed-chunks` 個までの前半だけ** (MCP ツール側の綴りは `all_chunks`。**どちらの綴りも両側で通る** — [stability.ja.md](stability.ja.md#同じものを二つの面がどう名付けるか) 参照)
- `--max-nodes` (既定 100、最大 2000 にクランプ) — 総ノード数。KNN 実行回数もこれで縛られる
- `--max-seed-chunks` (既定 32、`1..=1000` にクランプ) — シードに使う起点文書のチャンク数
- `--exclude-paths` — 結果から除外するカンマ区切りパス。起点パス自身は常に除外される。MCP param: `exclude_paths`
- `--dedup-by-path` — 同一パスのヒットをまとめて各ドキュメント最大 1 回に
- `--category` / `--topic` — 各ホップにカテゴリ / トピックフィルタを適用
- `--format json|text|dot|svg` — `json` (既定) と `text` は機械 / 人間向けの一覧、`dot` と `svg` は探索を図にする (v0.25.0+、下記)

### 形を見る (`--format dot` / `--format svg`)

`json` も `text` も同じノードを並べるだけで、**どこで枝分かれしたか**を見せない。見る価値があるのはそこなので、図として出す形式を 2 つ用意した:

```bash
groove graph --start notes/rag.md --format dot > graph.dot   # dot -Tsvg graph.dot に渡す
groove graph --start notes/rag.md --format svg > graph.svg   # ブラウザでそのまま開ける
```

`dot` は [Graphviz](https://graphviz.org/) のプログラム。`dot -Tsvg` / `-Tpng` / `-Tpdf` に渡すも、DOT ビューアで開くも、web のビューアに貼るも自由。`svg` は**何も入れていない環境でそのまま開ける**完成品で、groove が自前でレイアウトしている — **描画用の依存を取っていない**のは、このグラフが木であり、木なら 1 パスで配置できるから。

どちらも BFS の深さで配色し、edge に類似度を書き、**上限で探索が打ち切られた場合はその旨を図に載せる** (図を全体像と誤解させないため)。横に広いグラフでは Graphviz 経由の方が紙面が締まる。組み込み SVG は葉 1 つにつき 1 行積むので、読める大きさに保つ調整は `--max-nodes` で行う。

出力は `parent_id` / `depth` / `score` 付きのノードのフラット配列で、消費側で木を再構築できる。典型ユース: 「この note の周りの関連コンテキストを 30 チャンク LLM に読ませたい」「この overview から 2 ホップ辿ってどのトピックに触れているか見たい」。

## TOML スキーマによる frontmatter 検証

ナレッジベースで frontmatter の規約を運用しているなら、`groove validate` がすべての `.md` を TOML スキーマに対して検証し違反を報告する。スキーマ書式は [Frontmatter スキーマ検証](clients.ja.md#frontmatter-スキーマ検証) 節参照。コマンド自体は:

```bash
groove validate --kb-path /path/to/knowledge-base
groove validate --kb-path ... --format json | jq '.files[]'
groove validate --kb-path ... --format github         # CI 用 ::error annotation
```

フラグ:

- `--schema <PATH>` — `<kb-path>/groove-schema.toml` 以外からスキーマを読む。
  **ナレッジベースの隣に置かないスキーマを使う唯一の手段** — 複数のベースで
  1 つのスキーマを共有する、CI 用に厳しめのものを別に置く、といった場合
- `--fail-fast` — 最初の違反で exit 1 して残りを走査しない。
  「何が悪いか」ではなく「きれいかどうか」だけ知りたいときに使う
- `--no-color` — `--format text` の ANSI 色を落とす。stdout が TTY でなければ
  元から色は付かないので、TTY のときに落としたい場合のフラグ

終了コード: `0` (違反なし) / `1` (違反あり) / `2` (スキーマロードエラー)。`--kb-path` 直下に `groove-schema.toml` が無いときは短い "no schema found" メッセージと共に exit 0 となるため、既存ワークフローへの `groove validate` 追加は実際にスキーマを書くまで非破壊。

## 索引そのものを検査する (v0.23.0+)

`groove validate` が検査するのは**文書**。`groove doctor` が検査するのは**索引**:

```bash
groove doctor --kb-path /path/to/knowledge-base
groove doctor --kb-path ... --format json | jq '.findings[]'
```

検索は 1 つの chunk について 3 つのテーブルが一致していることを前提にしている — 本文・embedding・全文検索行。**ずれてもエラーにはならない**: embedding の無い chunk は単にベクトル検索に出ず、全文検索行の無い chunk はキーワード検索に出ないだけ。これまでは full index を回して修復されるのを見るまで気付けなかった。`doctor` は直接それを問う。あわせて、MCP の resource 面が**どの索引済み文書を提示していないか、なぜか**も報告する — 現在の `[parsers].enabled` に無い拡張子 / resource read が返せるサイズを超える文書 / 以前のバージョンで索引されたため size が未記録の文書。

終了コード: `0` (報告なし) / `1` (検出あり) / `2` (実行できない — 大抵は索引が無い)。**報告するだけで修復はしない**。各検出には直し方が併記される (構造的なものはすべて `groove index` か `groove index --force`)。

> `search` / `eval` と同様、本コマンドは DB を開くので、**未適用の schema migration があれば走る**。検出結果については read-only だが、ファイルについてはそうではない。

## Golden query セットに対する retrieval 品質評価

**任意のパワーユーザ機能**。`groove eval` は「想定される正解がわかっている質問」の小さなファイルを、`search` ツールと同じハイブリッド検索にかけ、**recall@k / MRR / nDCG@k** + 前回実行との差分を出す。モデル比較や `[quality_filter]` / RRF パラメータのチューニング時に便利。

`groove index` + `groove serve` で普通に使う一般ユーザは触る必要なし — golden ファイルが無ければ `eval` は hint 付きエラーで終了するだけで他の挙動には影響しない。

```bash
# 1) Golden YAML を <kb_path>/.groove-eval.yml に配置
cat > knowledge-base/.groove-eval.yml <<'EOF'
queries:
  - query: "RRF の k パラメータの意味は？"
    expected:
      - { path: "docs/ARCHITECTURE.md", heading: "Data flow" }
      - { path: "src/db.rs" }   # heading 省略 = ファイル一致で正解
EOF

# 2) index 済み DB に対して実行
groove eval --kb-path knowledge-base

# 3) 設定やモデルを変えて再実行、diff で変化を見る
groove eval --kb-path knowledge-base --reranker bge-v2-m3
```

出力: 集計指標 + 劣化 / ミスのあるクエリ行のみ。`--format json` で全クエリの詳細を取得可能。履歴は `<kb_path>/.groove-eval-history.json` に保存され、直近 10 件を diff 表示用に保持する。

評価対象のナレッジベースの中に評価についてのノートを置いていると、そのノートが本来の正解と競合する。v0.24.0 からは各 run がコーパスを走査し、**2 件以上**の golden query を逐語で含む文書 (= golden セット *について* 書いたノートの形) を stderr に警告する (`--format json` では `findings`)。**報告のみで exit code は不変**。1 件では報告しない理由を含めた詳細は [docs/eval.ja.md](eval.ja.md)。

CI 用途には `--fail-on-regression` (v0.6.0+) を渡す。直前の **fingerprint-compatible** run から `recall@k` / `MRR` / `ndcg@k` のいずれかが `regression_threshold` (既定 0.05) を超えて退化していたら exit code 1 を返す。golden YAML を更新すると hash が変わるので次回 run は比較対象外 = false positive にならない。

Golden YAML のリファレンス、指標の詳細説明、diff 出力の読み方、トラブルシューティングは [docs/eval.ja.md](eval.ja.md) 参照。

## fusion パラメータを測る (`groove tune`、v0.13.0+)

`[search.fusion]` で RRF 定数と bm25 列重みを公開しているが、既定値は業界慣例値
であり、RRF は公式に「チューニング不要」とされている。自分の KB について当て推量
ではなく根拠が欲しい場合は:

```bash
groove tune --kb-path knowledge-base
```

golden query セットに対して固定グリッドを掃引し、leave-one-query-out 交差検証で
結果をガードした上で、貼り付け可能なスニペットか「既定値を維持すべき」という結論
を出力する。tune 自身は何も適用しない。なお、このパラメータが動かせるのは
**そもそも bm25 段に到達する** クエリだけなので、tune はまず pre-flight で実効 N
を報告し、0 なら exit 2 で終わる。詳細は [docs/eval.ja.md](eval.ja.md) を参照。

## Related

- `docs/configuration.ja.md` — 同じフラグに `groove.toml` で既定値を与える
- `docs/clients.ja.md` — 起動したサーバに MCP クライアントを繋ぐ
- `README.ja.md` — インストールとクイックスタート
