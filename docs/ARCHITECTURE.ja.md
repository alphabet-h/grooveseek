# アーキテクチャ

kb-mcp のソース構造とデータフロー。コードを拡張・修正するコントリビュータ向け。

> **English version**: [ARCHITECTURE.md](./ARCHITECTURE.md)

## ソース別の責務

| ファイル | 役割 |
|---|---|
| `src/lib.rs` | (v0.7.1+) ライブラリクレートのルート。下記モジュールを `kb_mcp::*` として再公開し、`benches/` や `tests/` から内部 API をサブプロセス経由なしで呼び出せるようにする。ライブラリの公開面は意図的に unstable であり、外部利用者向けではない |
| `src/main.rs` | バイナリエントリ。clap CLI が `index` / `status` / `serve` / `search` / `graph` / `validate` / `eval` サブコマンドをディスパッチ。`use kb_mcp::*;` 経由で lib を呼ぶ。`kb-mcp.toml` 読み込みと CLI 引数へのマージ。JSON / text 出力フォーマッタ |
| `src/config.rs` | 4 階層の `kb-mcp.toml` 探索 (`--config` フラグ → CWD → `.git` 祖先 (CWD + 最大 19 祖先) → バイナリ隣 legacy)。`Config::discover()` が `ConfigSource` enum を返し、`main.rs` が起動時に tracing で出す。`CLI > 設定ファイル > 既定値` の優先順位を解決。config が設定していて env 未設定の場合のみ `FASTEMBED_CACHE_DIR` を env に注入 |
| `src/server.rs` | rmcp `ServerHandler` 実装。6 つの MCP ツールをディスパッチ。`search` は `db.search_hybrid` 経由で結果を `SearchResponse` ラッパ (`low_confidence` / `match_spans` / `filter_applied`) に包んで返す (v0.3.0 で BREAKING、CHANGELOG 参照) |
| `src/service/` | (v0.8.0+) クロスプラットフォーム OS ユーザサービスインストーラ。`mod.rs` (= `ServiceBackend` trait + `InstallContext` + `ServiceState`)、`install.rs` / `uninstall.rs` / `status.rs` (= orchestration)、`linux.rs` / `macos.rs` / `windows.rs` (= OS 別 backend、cfg-gated)。Phase 1 = user-level のみ (= admin / sudo 不要、Linux systemd-user / macOS LaunchAgent / Windows Task Scheduler AT_LOGON)。`kb-mcp service install` は Rust crate のみで自己登録 (= NSSM / WiX / 3rd-party tooling 不使用)。Windows backend (v0.8.3+) は `Command::new("powershell")` 経由で `Register-ScheduledTask -Action -Trigger -Settings` cmdlet を呼ぶ — `schtasks /Create /XML` は v0.8.0 → v0.8.3 で locale / elevation / Principal の 3 段階問題により放棄、詳細は `.dev/knowledge/windows-task-scheduler-pitfalls.md` 参照。 |
| `src/indexer.rs` | walkdir で `Registry::extensions()` の拡張子を走査。全 read 経路 (初回スキャン / `reindex_single_file` / `rename_single_file`) は `fs::read` でバイト読み (`read_to_string` ではない) し、生バイトを SHA-256 で hash して content-hash 差分検出に使う — 既存の UTF-8 KB では旧文字列 hash と一致するため no-op。Parser trait (`parse_bytes`) でパース → embedder で embedding → db に格納。per-file skip 隔離: `read` 失敗 / size cap 超過 / `parse_bytes` エラーはそのファイルだけ skip (warning ログ) して全体を abort せず、skip したパスは削除扱いにせずインデックスに保持する。watcher と共有する増分 API (`reindex_single_file` / `deindex_single_file` / `rename_single_file`) |
| `src/indexer/progress.rs` | (v0.7.8+) `ProgressReporter` + `ProgressMode` enum。`kb-mcp index` の per-file 出力を制御: `Verbose` (既定) / `Quiet` (`--quiet`) / `Auto` (`--progress`、TTY = `indicatif::ProgressBar`、非 TTY = 定期 `Progress: N/M (P%)` 行)。MCP server `rebuild_index` ツールは `Quiet` 固定。bar lifetime は `rebuild_index` 内に閉じる lazy init (`start_indexing(total)` 経由) で `Backfilled` / `Found` 行は plain `eprintln!` のままにし、bar との衝突を構造的に回避する |
| `src/parser/` | Parser trait + Registry。`mod.rs` (Frontmatter / Chunk / ParsedDocument に加え、indexer / server の全 call site が経由するエントリポイント `parse_bytes(bytes, path_hint, exclude_headings) -> Result<ParsedDocument>` を定義。default impl は UTF-8 検証 → `parse` へ委譲するため `md`/`txt` は override 不要、バイナリ形式 parser はこれを直接 override する。`is_binary()` (既定 `false`) はバイナリ parser を示し、`get_document` の size cap 分類と quality filter 免除判定に使う。`MAX_RAW_BINARY_BYTES` = 50 MiB はバイナリ形式共通の生バイト上限で、indexer の size-skip guard と `get_document` の両方で共有)、`markdown.rs`、`txt.rs`、`pdf.rs` (v0.10.0+、詳細下記)、`ooxml.rs` / `xlsx.rs` / `docx.rs` / `pptx.rs` (v0.11.0+、詳細下記)、`registry.rs` (拡張子ルックアップ、`binary_extensions()`) |
| `src/parser/pdf.rs` | (v0.10.0+) `PdfParser`、`is_binary() == true`、`[parsers].enabled = ["md", "pdf"]` でオプトイン。[oxidize-pdf](https://crates.io/crates/oxidize-pdf) (`PdfReader` + `PdfDocument::extract_text`) でページ単位にテキストを抽出し、空でない各ページが 1 チャンク (見出し `p.N`、`level: None`) になる。PDF の `Title` / `CreationDate` メタデータを frontmatter に反映し、`Title` が無ければファイル名派生タイトルに fallback する。`PdfDocument::new` + `extract_text` + `metadata` の一連は `std::panic::catch_unwind` でラップ (panic hook を差し替えてデフォルトの backtrace ノイズを抑制) し、oxidize-pdf 内部で発生し得る未知の panic を per-file `Err` (indexer の skip + warn) に正規化して `index` 実行全体の abort を防ぐ。スキャン / 画像のみの PDF (text layer なし) は平均抽出文字数 (< 50 chars/page) のヒューリスティックで検出して reject、暗号化 PDF は `PdfReader::new` / `extract_text` から `Err` として現れる (oxidize-pdf の `ParseResult` ベースのエラー設計、パスワード対応なし)。後処理として保守的な行末ハイフン結合 (`-\n` は両隣が ASCII 小文字の場合のみ) とよく使われるリガチャ (ﬁ/ﬂ/ﬀ/ﬃ/ﬄ) の正規化を行う |
| `src/parser/ooxml.rs` | (v0.11.0+) `xlsx.rs` / `docx.rs` / `pptx.rs` が共有する OOXML zip/XML helper (parser struct 自体は持たない)。`read_zip_entry` は zip パート 1 個を生バイトで読む。`core_xml_frontmatter` / `parse_core_xml` は `docProps/core.xml` (Dublin Core: `dc:title` / `dcterms:created` または `modified` / `cp:keywords`) を `Frontmatter` にマップし、パート不在または `title` が空なら filename 派生タイトルに fallback する。`local_name_pub` は QName から namespace prefix を除いた local part を取る (`cp:title` → `title`)、要素名判定を prefix 非依存にする。`resolve_general_ref` は quick-xml 0.38+ の `Event::GeneralRef` (entity 参照 `&amp;` 等が `Event::Text` に畳み込まれず別 event として届く挙動) を解決する — docx.rs と pptx.rs の両方が同じ char-ref / named-entity 処理を必要とするためここに共通化した |
| `src/parser/xlsx.rs` | (v0.11.0+) `XlsxParser` (`.xlsx`) と `XlsParser` (`.xls`)、いずれも `is_binary() == true`、`parse_workbook_bytes` 実装を共有する。`calamine::open_workbook_auto_from_rs` でワークブックを開く (OOXML / legacy BIFF を auto-detect)。空でないシートごとに 1 チャンク (見出し `Sheet: <name>`、行ごとにセルをタブ結合) を生成し、`SHEET_MAX_BYTES` (1 MiB) で truncate する — 行単位の境界セマンティクス (合計を cap 超過させた行はそのまま丸ごと emit してから、そのシートの抽出を打ち切る。行途中では絶対に切らない)。frontmatter は、バイト列が zip として開ける場合 (`.xlsx` は該当) は `ooxml::core_xml_frontmatter` 経由で `docProps/core.xml` から取得する。`.xls` は OOXML 以前の BIFF 形式でそのパートが存在しないため、常にファイル名派生タイトルに fallback する |
| `src/parser/docx.rs` | (v0.11.0+) `DocxParser`、`is_binary() == true`。`word/document.xml` を段落 (`<w:p>`) 単位で読み、`<w:pStyle w:val="HeadingN">` をセクション境界として扱う — `markdown.rs` が Markdown 見出しに使うのと同じ見出し階層チャンク化規則で、`exclude_headings` 対応も含む (除外見出し配下の本文は次の非除外見出しまで捨てる)。表 (`<w:tbl>`) のテキストは特別扱い不要: OOXML 上の入れ子構造 `w:tbl > w:tr > w:tc > w:p > w:r > w:t` により、表セルのテキストは通常の `<w:p>` 境界処理を通って自然に現在のセクション本文に取り込まれる。frontmatter は `ooxml::core_xml_frontmatter` 経由 |
| `src/parser/pptx.rs` | (v0.11.0+) `PptxParser`、`is_binary() == true`。`ppt/slides/slideN.xml` エントリを集めて数値のスライド番号順にソートする (zip 内の格納順ではない)。スライドごとに 1 チャンク (見出しは `ctrTitle`/`title` placeholder shape にテキストがあれば `Slide N: <title>`、無ければ素の `Slide N`) を生成し、スライド内の表テキストも本文に含める。発表者ノートは末尾 `[notes]` セクションとして付加し、スライドの `ppt/slides/_rels/slideN.xml.rels` を読んで `notesSlide` relationship の `Target` を解決する — 意図的に同番号ファイル (`slideN.xml` ↔ `notesSlideN.xml`) の推測 heuristic にはしていない。編集後にスライド番号とノート番号がずれたケースで dry-run (plan Task 3.7) が誤帰属を実証したため。frontmatter は `ooxml::core_xml_frontmatter` 経由 |
| `src/markdown.rs` | `crate::parser::markdown::MarkdownParser` への薄い shim。legacy `parse()` / `parse_with_excludes()` 公開 API を維持 |
| `src/watcher.rs` | `notify-debouncer-full` を tokio channel 越しに受信。拡張子 + path でフィルタして `indexer::{reindex,deindex,rename}_single_file` にディスパッチ。MCP サーバと並走 (`tokio::spawn`) |
| `src/transport/` | MCP transport 抽象。`mod.rs` (Transport enum + CLI/config 解決)、`stdio.rs` (stdio)、`http.rs` (rmcp `StreamableHttpService` + axum、`/mcp` と `/healthz` をマウント。v0.8.0+ で admin sub-router を追加: `/ui` + `/api/admin/status` + `/api/search` を `admin_host_check` middleware (= Host header の **exact match** で loopback alias + bind addr 限定) で gate)。`KbServerShared` を Arc 共有し session factory で接続ごとに軽量ハンドルを生成 |
| `src/transport/webui_index.html` | (v0.8.0+) WebUI MVP placeholder HTML、`transport/http.rs::ui_index` で `include_str!` 経由 embed。Raw HTML + JS、CSS framework 不使用、`textContent` / `createElement` のみで XSS 安全 (= `innerHTML` 不使用)。Phase 3+ で本格 redesign する disposable placeholder。 |
| `crates/kb-mcp-tray/` | (v0.9.0+) Windows 限定 system tray binary (`kb-mcp-tray.exe`、GUI subsystem) で daemon の監視 + lifecycle 制御。5 秒間隔で `/api/admin/status` を polling し、4 状態 status dot (緑 = healthy / 黄 = indexing / 赤 = 1 分以上 down / 灰 = polling 待ち) を描画、right-click menu 6 項目 (Status / Open Web UI / Start / Stop / Restart / Quit Tray)。daemon 制御は async PowerShell `Start/Stop-ScheduledTask` cmdlet (= `src/service/windows.rs` と同 path)。dual event loop: `tao` を main thread、`tokio` runtime を別 thread で spawn、`EventLoopProxy::send_event` で bridge。panic hook + `tracing-appender::rolling::daily` で `%LOCALAPPDATA%\kb-mcp\logs\tray.YYYY-MM-DD` に log 出力。library API (`install::install_autostart` / `uninstall_autostart`) は `kb-mcp service install --with-tray` / `service uninstall` / `service tray-install` / `service tray-uninstall` から呼ばれ、shell:startup `.lnk` shortcut を PowerShell `WScript.Shell` COM 経由で管理。cargo-dist は `kb-mcp-tray.exe` を `x86_64-pc-windows-msvc` のみ artifact 化。 |
| `src/schema.rs` | Frontmatter スキーマ検証。`kb_path` 直下の `kb-mcp-schema.toml` を読み、`required` / `type` / `pattern` / `enum` / `min_length` / `max_length` / `allow_empty` を検証。`kb-mcp validate` CLI から呼ばれ、text / JSON / GitHub annotation 形式で報告 |
| `src/embedder.rs` | `fastembed-rs` の薄いラッパ。`ModelChoice` で embedding モデル (BGE-small-en-v1.5 / BGE-M3) を選択。`RerankerChoice` + `Reranker` で optional な cross-encoder 再ランク |
| `src/db.rs` | `rusqlite` + `sqlite-vec` + FTS5 (trigram)。`chunks` / `vec_chunks` / `fts_chunks` スキーマと CRUD を管理。`search_hybrid` (Reciprocal Rank Fusion、`k = 60`) と v0.7.0 で追加した unbounded variant (MMR / parent retriever 用) を提供。`SearchFilters` 構造体でフィルタ引数 (path glob / tags / date range / min_quality) を集約、`MatchSpan` でバイトオフセット引用を表現 (v0.3.0 追加)。`chunks.level` (v0.7.0 追加) で h2 / h3 を区別 |
| `src/mmr.rs` | (v0.7.0+) Maximal Marginal Relevance の貪欲再ランク + 類似度キャッシュ。`mmr_select` は post-rerank の候補プールに対して動き、`[search.mmr]` 設定または per-call `mmr` パラメータで gating される |
| `src/parent.rs` | (v0.7.0+) 表示時 parent retriever。`apply_parent_retriever` がヒットチャンクを `expand_adjacent` (level 整合な隣接 sibling マージ) または `expand_whole_document` (`whole_doc_threshold_tokens` 未満チャンクの全文 fallback) で拡張する。score / rank / `match_spans` は元のヒットを保ち、`content` と新フィールド `expanded_from` のみが変わる |
| `src/quality.rs` | チャンク単位の品質スコアリング (長さ / 定型語 / 構造シグナル) |
| `src/graph.rs` | ベクトルインデックス上での Connection Graph BFS。`get_connection_graph` MCP ツールと `kb-mcp graph` CLI から利用 |
| `src/eval.rs` | `kb-mcp eval` CLI 用のリトリーバル品質評価 (opt-in)。Golden YAML を parse し、各クエリを `db.search_hybrid` で実行、recall@k / MRR / nDCG@k を計算。`<kb_path>/.kb-mcp-eval-history.json` を読み書きして前回との差分を表示。`ConfigFingerprint` (v0.7.0+) は `mmr` / `parent_retriever` を optional に保持し、設定違いの eval 実行を別 history entry として区別する。`serve` / `search` / `index` の挙動は一切変えない |

## データフロー

```
.md / .txt / .pdf ファイル (Registry::extensions() でフィルタ)
     │
     ▼ walkdir
indexer.rs: SHA-256 content-hash を chunks.hash と比較
     │
     ▼ 変更ありのファイルのみ
parser/: 拡張子で Parser を選択 → frontmatter + title 抽出 + チャンク化
     │
     ▼
embedder.rs: fastembed で embedding 生成
              (BGE-small-en-v1.5 → 384 次元、BGE-M3 → 1024 次元)
     │
     ▼
db.rs: chunks (メタデータ) + vec_chunks (embedding)
       + fts_chunks (FTS5 trigram) に UPSERT
```

検索時、`search` ツールはハイブリッド検索を実行する:

- query → embedder → `vec_chunks MATCH` (top-N)
- query → sanitize → `fts_chunks MATCH` + bm25 (top-N) — 見出しに 2 倍の重み
- Rust 側で Reciprocal Rank Fusion (`k = 60`) → top-`limit` を返却
- (任意) cross-encoder reranker が上位候補を再スコアリングして返却
- (任意, v0.7.0+) MMR 多様性再ランクが大きめの候補プールから貪欲に `limit` 個を選択し、関連度と新規性のバランス (`lambda`)、同一 doc の penalty (`same_doc_penalty`) を効かせる
- (任意, v0.7.0+) parent retriever が短いヒットチャンクの `content` を隣接 sibling またはドキュメント全体に展開する。score / rank / path / `match_spans` は変えないため relevance signal は維持される

v0.7.0 のフルパイプラインは **`RRF → reranker → MMR → parent retriever → match_spans`**。各段は対応する設定が off なら no-op となるため、既定では v0.7.0 以前の挙動に等しい。narrative は [retrieval-pipeline.ja.md](./retrieval-pipeline.ja.md) を参照。

## Contextual Retrieval (v0.12.0+)

静的 Contextual Retrieval (feature-46) は、各チャンクが embedder / FTS index / reranker に渡る前に、ドキュメント構造由来の breadcrumb を前置する機能。すべて index 時に LLM 呼び出しなしで決定論的に生成される。`[contextual].enabled` で on/off する (v0.12.0 時点の既定は **off** ―― judgment gate の結果として false-by-default に転換した経緯は README の「Contextual Retrieval」節の A/B 数値を参照)。

- **`Chunk.context: Option<String>`** (`src/parser/mod.rs`) は検索用の内部フィールドで、`search` / `get_document` の返却には一切現れない。`build_context(parts: &[&str]) -> Option<String>` が空要素・連続重複要素を skip しつつ `" > "` で結合し、BGE-small の 512 token 入力制限を守るため 200 文字 (char boundary 安全) で cap する。
- **2 系統の ancestry 生成**。コードベース内の parser 形状にそのまま対応する:
  1. **Markdown** (`src/parser/markdown.rs`): level をキーにした ancestry `stack: Vec<(u8, String)>` を、見出し遷移のたびに現在の見出しの深さまで pop してから push する (h2→h4 のような level 飛びでも、架空の h3 を補わず最も近い浅い祖先を継承する)。`exclude_headings` で除外された見出しも stack には積まれる (除外セクション自体は chunk を生成しないが、その子孫の ancestry は正しく保たれる)。`context = build_context(&[title, ...ancestry, heading])`。
  2. **バイナリ / フラット形式** (PDF のページ chunk、Office の単一セクション chunk、プレーン `.txt`。`parser::single_text_chunk` および各形式の chunker 経由): これらは辿るべき見出し階層を持たないため、`[title]` のみの単層 context になる。
- **`chunks.context_text TEXT`** (`src/db.rs`): nullable 列。feature-46 以前の DB には idempotent な `ensure_context_text_column` の `ALTER TABLE` で追加される。現在の `ContextMode` が `Static` のときのみ `Chunk.context` から埋まり、`Off` モードでは `NULL` のまま。
- **`fts_chunks` の第 3 列** `context` (`heading` / `content` に加えて): 2 列の legacy index は `ensure_fts_context_column` で migration される ―― virtual table を drop + 再作成し、`INSERT ... SELECT id, heading, COALESCE(context_text, ''), content FROM chunks` で repopulate する。並行 opener との競合を避けるため `BEGIN IMMEDIATE` トランザクション (`begin_immediate_tx`) でラップする。この repopulate は write lock を保持し続ける処理で、574 doc / 10,002 chunks の KB を embedding + reranker モデル同時ロード中の並行負荷下で計測したところ 9.7〜12.3s かかったため、`Database::init` は `busy_timeout` を 30,000ms に設定している (v0.12.0 で 10s から引き上げ)。これにより `serve` 常駐プロセスの `search` / `status` が、進行中の migration を `SQLITE_BUSY` で即失敗せず待機できる。Contextual BM25 のスコアリングは `bm25(fts_chunks, heading_weight, context_weight, content_weight)` 呼び出しの `context_weight` を `FTS_BM25_CONTEXT_WEIGHT = 1.0` として重み付けする。
- **embedding 入力の合成** (`indexer::embed_input_for`): `Static` モードかつ context が非空なら embedder には `"{context}\n\n{content}"` を渡す。それ以外 (`Off` モード、または context 未生成) は従来通り `content` のみを渡す ―― embedding 入力が v0.12.0 以前と異なるのはここだけ。
- **reranker 入力の合成** (`embedder::contextualize_for_rerank`): `db.rs` の検索クエリから `SearchResult` が `context_text` を最後まで運び、reranker は候補ごとに同じ `"{context}\n\n{content}"` を合成してからスコアリングする。`context_text` 自体は MCP / CLI 呼び出し元に届く前に response から取り除かれる ―― あくまで内部のランキング signal に過ぎない。
- **`index_meta.context_mode` の versioning** (`ContextMode::{Off, Static}`、`db::read_context_mode` / `write_context_mode`、`indexer::resolve_context_mode`): embedding 空間が意図せず混在した index を作らないよう、config の「desired」モードより DB に記録済みの「実際に構築された」モードを優先する。
  - `--force`: desired モードを無条件に採用して記録する (`reset_for_model` 直後の DB は空)。
  - `--force` なし、DB に記録済みモードがあり desired と異なる: **DB のモードを維持**し、`kb-mcp index --force` を促す警告を stderr に出す。
  - `--force` なし、記録済みモードなし (`index_meta` に `context_mode` キーが無い、feature-46 以前の genuine な DB): DB に既に chunk があれば (= この機能より前からある既存 index) `Off` へ grandfather し、DB が空 (= 新規 index) なら desired モードを採用する。
  - `kb-mcp status` は `read_context_mode` の値をそのまま `Context mode: static` / `Context mode: off` として stderr に出力する。
- `main.rs` は `serve` / `index` の両方で `cfg.contextual.as_ref().map(|c| c.enabled).unwrap_or(false)` から同一ロジックで desired モードを算出する。この `unwrap_or(false)` は `ContextualConfig::default()` と一致させてあり、`[contextual]` セクション省略時と明示的な `enabled = false` が同じ挙動になる。

## Embedding キャッシュの解決

`embedder.rs::resolve_cache_dir()` が以下の順で解決する:

1. `FASTEMBED_CACHE_DIR` 環境変数 (最優先)
2. OS 標準キャッシュディレクトリ + `fastembed`:
   - Linux: `~/.cache/fastembed`
   - macOS: `~/Library/Caches/fastembed`
   - Windows: `%LOCALAPPDATA%\fastembed`
3. CWD 直下の `.fastembed_cache/` (最終フォールバック)

初回実行時、選択した ONNX モデルが HuggingFace hub 互換のキャッシュ構造で DL される (BGE-small: 約 130 MB、BGE-M3: 約 2.3 GB、BGE-reranker-v2-m3: 約 2.3 GB)。2 回目以降は再 DL されない。

`fastembed-rs` の native TLS が HuggingFace への接続に失敗する場合 (企業プロキシや TLS inspection の影響) は、README の「HuggingFace の TLS 失敗への対処」節を参照して `huggingface_hub` CLI で迂回する。

## CLI 出力規約

`kb-mcp` CLI は **stdout = データ出力 / stderr = 進捗** の規約に従う:

- **stdout** は machine-parseable な data 出力のみ:
  - `kb-mcp search` の JSON 結果
  - `kb-mcp eval` の golden query 評価結果
- **stderr** は人間向けの進捗 / 統計 / warning / error:
  - `kb-mcp index` の進捗行 (`Indexing ...`, `Done in ...`、各ファイル毎の `  indexed:` / `  renamed:` / `  deleted:`)。`--quiet` で per-file 出力を抑止 (start / found / done のサマリだけ残す)、`--progress` で `indicatif` バー (TTY) または定期 `Progress: N/M (P%)` 行 (非 TTY) に切替。両 flag は相互排他 + 既定 off (v0.7.8 追加)
  - `kb-mcp status` の統計 (`Documents: N`, `Chunks: N`)
  - `kb-mcp service install/uninstall/status/list` の全 message は stderr (= status / progress / 診断、規約準拠)。stdout は空。
  - すべての `tracing` / `eprintln!` 系診断メッセージ

新規 subprocess test を書く場合は、`src/main.rs` の対応する `Commands::*` block を grep して、その subcommand が stdout / stderr のどちらに書くかを必ず先に確認する。**stdout に出るのは `Commands::Search` の JSON のみ**で、それ以外はすべて stderr 中心。

## 主要な依存

- **`rmcp`** 1.x — MCP サーバフレームワーク (stdio + Streamable HTTP トランスポート)
- **`fastembed`** 5.x — ONNX ベースの embedding / reranker
- **`rusqlite`** 0.39 with `bundled` — 静的リンク SQLite 3.50+、FTS5 + trigram tokenizer + `contentless_delete = 1`
- **`sqlite-vec`** 0.1 — ベクトル類似検索拡張
- **`pulldown-cmark`** 0.13 — Markdown パーサ
- **`notify`** 8 + **`notify-debouncer-full`** 0.6 — debounce 付きファイルウォッチャ
- **`axum`** 0.8 — Streamable HTTP トランスポートの HTTP サーバ
- **`dirs`** 6 — OS 標準キャッシュディレクトリ解決
- **`indicatif`** 0.18 — `kb-mcp index --progress` の TTY プログレスバー (v0.7.8 / D-10 追加)。MSRV 1.70+、binary size 約 +150 KB。stderr の TTY 自動検出は `std::io::IsTerminal` (Rust 1.70+ stdlib) を使用
- **`wide`** 0.7 — pure-rust SIMD プリミティブ (`f32x8`)、MMR cosine kernel で使用 (v0.7.2 / feature-31 で追加)
- **`tray-icon`** 0.24 + **`tao`** 0.35 + **`image`** 0.25 + **`tracing-appender`** 0.2 + **`winresource`** 0.1 (build-dep) — `kb-mcp-tray` crate の Windows 限定 deps (v0.9.0 / feature-44 で追加)。`tray-icon` が muda ベースの context menu + icon swap、`tao` が Win32 event loop、`image` が embed PNG status icon の RGBA decode、`tracing-appender` が daily rotating tray log、`winresource` が `assets/app.ico` を exe icon として embed。すべて `target_os = "windows"` で gate され、非 Windows workspace build では skip。
