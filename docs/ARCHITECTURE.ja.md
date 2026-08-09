# アーキテクチャ

kb-mcp のソース構造とデータフロー。コードを拡張・修正するコントリビュータ向け。

> **English version**: [ARCHITECTURE.md](./ARCHITECTURE.md)

## ソース別の責務

| ファイル | 役割 |
|---|---|
| `kb-mcp/src/lib.rs` | (v0.7.1+) ライブラリクレートのルート。下記モジュールを `kb_mcp::*` として再公開し、`benches/` や `tests/` から内部 API をサブプロセス経由なしで呼び出せるようにする。ライブラリの公開面は意図的に unstable であり、外部利用者向けではない |
| `kb-mcp/src/main.rs` | バイナリエントリ。clap CLI が `index` / `status` / `serve` / `search` / `graph` / `validate` / `eval` / `tune` / `service` サブコマンドをディスパッチ (`service` は `install` / `uninstall` / `status` / `list` / `tray-install` / `tray-uninstall` を内包)。`use kb_mcp::*;` 経由で lib を呼ぶ。`kb-mcp.toml` 読み込みと CLI 引数へのマージ。JSON / text 出力フォーマッタ |
| `kb-mcp/src/config.rs` | 4 階層の `kb-mcp.toml` 探索 (`--config` フラグ → CWD → `.git` 祖先 (CWD + 最大 19 祖先) → バイナリ隣 legacy)。`Config::discover()` が `ConfigSource` enum を返し、`main.rs` が起動時に tracing で出す。`CLI > 設定ファイル > 既定値` の優先順位を解決。config が設定していて env 未設定の場合のみ `FASTEMBED_CACHE_DIR` を env に注入 |
| `kb-mcp/src/server.rs` | rmcp `ServerHandler` 実装。6 つの MCP ツールをディスパッチ。`search` は `db.search_hybrid` 経由で結果を `SearchResponse` ラッパ (`low_confidence` / `match_spans` / `filter_applied`) に包んで返す (v0.3.0 で BREAKING、CHANGELOG 参照) |
| `kb-mcp/src/schema_compat.rs` | (v0.14.0+) 上記ツール用に公開する JSON Schema を、サーバから出る前に正規化する。`schemars` は `Option<T>` を union type (`{"type": ["string", "null"]}`) として導出し、Rust の整数幅を `format: uint32` として刻む。どちらも JSON Schema 2020-12 として正当だが、strict な tool-calling ランタイムは union を拒否し、その format を知らない。frontmatter を検証する `schema.rs` とは無関係 — 名前は近いが対象が違う |
| `kb-mcp/src/service/` | (v0.8.0+) クロスプラットフォーム OS ユーザサービスインストーラ。`mod.rs` (= `ServiceBackend` trait + `InstallContext` + `ServiceState`)、`install.rs` / `uninstall.rs` / `status.rs` (= orchestration)、`linux.rs` / `macos.rs` / `windows.rs` (= OS 別 backend、cfg-gated)。加えて **cfg gate の外**に意図的に置いた 2 module があり、全 OS leg で compile + テストされる: `render.rs` (v0.14.0+、unit / plist のテンプレートと escape 処理 — plist の誤りは以前は macOS runner でしか検出できなかった)、`powershell.rs` (v0.14.0+、`powershell.exe` の出力に対する UTF-8 前置と strict / diagnostic の 2 種類の decoder)。Phase 1 = user-level のみ (= admin / sudo 不要、Linux systemd-user / macOS LaunchAgent / Windows Task Scheduler AT_LOGON)。`kb-mcp service install` は Rust crate のみで自己登録 (= NSSM / WiX / 3rd-party tooling 不使用)。Windows backend (v0.8.3+) は `Command::new("powershell")` 経由で `Register-ScheduledTask -Action -Trigger -Settings` cmdlet を呼ぶ — `schtasks /Create /XML` は v0.8.0 → v0.8.3 で locale / elevation / Principal の 3 段階問題により放棄、詳細は `.dev/knowledge/windows-task-scheduler-pitfalls.md` 参照。 |
| `kb-mcp/src/indexer.rs` | walkdir で `Registry::extensions()` の拡張子を走査。全 read 経路 (初回スキャン / `reindex_single_file` / `rename_single_file`) は `fs::read` でバイト読み (`read_to_string` ではない) し、生バイトを SHA-256 で hash して content-hash 差分検出に使う — 既存の UTF-8 KB では旧文字列 hash と一致するため no-op。Parser trait (`parse_bytes`) でパース → embedder で embedding → db に格納。per-file skip 隔離: `read` 失敗 / size cap 超過 / `parse_bytes` エラーはそのファイルだけ skip (warning ログ) して全体を abort せず、skip したパスは削除扱いにせずインデックスに保持する。watcher と共有する増分 API (`reindex_single_file` / `deindex_single_file` / `rename_single_file`) |
| `kb-mcp/src/indexer/progress.rs` | (v0.7.8+) `ProgressReporter` + `ProgressMode` enum。`kb-mcp index` の per-file 出力を制御: `Verbose` (既定) / `Quiet` (`--quiet`) / `Auto` (`--progress`、TTY = `indicatif::ProgressBar`、非 TTY = 定期 `Progress: N/M (P%)` 行)。MCP server `rebuild_index` ツールは `Quiet` 固定。bar lifetime は `rebuild_index` 内に閉じる lazy init (`start_indexing(total)` 経由) で `Backfilled` / `Found` 行は plain `eprintln!` のままにし、bar との衝突を構造的に回避する |
| `kb-mcp/src/parser/` | Parser trait + Registry。`mod.rs` (Frontmatter / Chunk / ParsedDocument に加え、indexer / server の全 call site が経由するエントリポイント `parse_bytes(bytes, path_hint, exclude_headings) -> Result<ParsedDocument>` を定義。`parse_bytes` は extension trait `ParserExt` 側に blanket impl (`impl<T: Parser + ?Sized>`) として置いてあり、**parser 側が定義を持てない**。常に `Parser::parse_bytes_inner` (同じシグネチャ) を `catch_unwind` 越しに呼ぶため、parser や依存 crate のどこで panic しても per-file の `Err` になり `index` 実行全体が落ちない (default method だと override で隔離を素通りできてしまうので採らない)。`parse_bytes_inner` の default impl は UTF-8 検証 → `parse` へ委譲するため `md`/`txt` は override 不要、バイナリ形式 parser はこれを直接 override する。`is_binary()` (既定 `false`) はバイナリ parser を示し、`get_document` の size cap 分類と quality filter 免除判定に使う。`MAX_RAW_BINARY_BYTES` = 50 MiB はバイナリ形式共通の生バイト上限で、indexer の size-skip guard と `get_document` の両方で共有)、`markdown.rs`、`txt.rs`、`pdf.rs` (v0.10.0+、詳細下記)、`ooxml.rs` / `xlsx.rs` / `docx.rs` / `pptx.rs` (v0.11.0+、詳細下記)、`panic_guard.rs` (詳細下記)、`registry.rs` (拡張子ルックアップ、`binary_extensions()`) |
| `kb-mcp/src/parser/panic_guard.rs` | `ParserExt::parse_bytes` の panic 隔離機構。`catch_parser_panic` が parser を `catch_unwind` 下で実行し、panic を `Err("<path>: <id> parser panicked: <payload>")` に変換する (payload を残すので indexer の skip 行に原因が出る)。wrapper panic hook は **一度だけ** install して以後入れ替えず、RAII guard が立てる thread-local flag を見て **parse 中のそのスレッド自身** の backtrace だけを抑制する (呼び出しごとに hook を差し替える方式は 2 スレッド同時 parse で race する)。元は PDF 専用 (v0.10.0) だったものを `docx`/`xlsx`/`pptx` にも効かせるためここへ移設した。これが無いと crafted な spreadsheet 1 個で `kb-mcp index` が丸ごと落ちる (calamine の `get_dimension` は減算を checked していないため、`ref="B2:A1"` は debug assertion 有効ビルドで panic する) |
| `kb-mcp/src/parser/pdf.rs` | (v0.10.0+) `PdfParser`、`is_binary() == true`、`[parsers].enabled = ["md", "pdf"]` でオプトイン。[oxidize-pdf](https://crates.io/crates/oxidize-pdf) (`PdfReader` + `PdfDocument::extract_text`) でページ単位にテキストを抽出し、空でない各ページが 1 チャンク (見出し `p.N`、`level: None`) になる。PDF の `Title` / `CreationDate` メタデータを frontmatter に反映し、`Title` が無ければファイル名派生タイトルに fallback する。oxidize-pdf 内部で発生し得る未知の panic は per-file `Err` (indexer の skip + warn) に正規化されて `index` 実行全体の abort を防ぐ — この `catch_unwind` は元々ここにあったが、現在は全 parser に効く `Parser::parse_bytes` / `parser/panic_guard.rs` 側にある。抽出したページは 2 つの門を**この順序で**通る (`reject_unindexable_pages`)。**第 1 に**、C1 制御コード (U+0080–U+009F) が全文字の 1% に達したテキストは文字化けとして reject する (v0.15.1+)。UTF-16BE を 1 バイトずつ読むと CJK の上位バイトが C1 に落ちるが、正しく復号できたテキストにこの領域は現れない — 正しく抽出できた 6 サンプルで 0.00%、誤デコードした 4 サンプルで 3.61〜15.59% と実測で完全に分離した。この門を先に置くのは、誤デコードが 1 文字を 2 文字に増やすため、文字化けの方が下の密度の門を**通過してしまう**から (実測 1052 chars/page)。正しく抽出できた薄いテキスト (29 chars/page) は通過しない。**第 2 に**、平均抽出文字数が 50 chars/page 未満の PDF を reject する。このヒューリスティックはスキャン / 画像のみの PDF (text layer なし、OCR 非対応) のために書かれたが、それ専用ではない — 正しくデコードできていて本当に 1 ページあたりの文字が少ない PDF (表紙、ラベル、図版主体の資料) もここに落ちる。閾値を下げないのは、電子的に載せたページ番号と「CONFIDENTIAL」スタンプだけを持つスキャン PDF が 39 chars/page を出すため。**文字数だけでは価値のない定型文と密度の低い本文を分離できない**。そのため診断メッセージは「スキャン」と断定せず、また原因を閉じた形で列挙もせず、測った値と代表的な原因を開いた列挙として出す。`/ToUnicode` 付き TrueType サブセットを埋め込む日本語 PDF (Word / LibreOffice / Google ドキュメントの出力) は正しく抽出できる。文字化けするのは予約 CMap を使い `/ToUnicode` を持たない CID-keyed フォントの場合で、根本原因は upstream 側 — oxidize-pdf が `/DescendantFonts` を CIDFont が間接参照のときしか読まず、直接辞書の場合は読まない。暗号化 PDF は `PdfReader::new` / `extract_text` から `Err` として現れる (oxidize-pdf の `ParseResult` ベースのエラー設計、パスワード対応なし)。後処理として保守的な行末ハイフン結合 (`-\n` は両隣が ASCII 小文字の場合のみ) とよく使われるリガチャ (ﬁ/ﬂ/ﬀ/ﬃ/ﬄ) の正規化を行う |
| `kb-mcp/src/parser/ooxml.rs` | (v0.11.0+) `xlsx.rs` / `docx.rs` / `pptx.rs` が共有する OOXML zip/XML helper (parser struct 自体は持たない)。`read_zip_entry` は zip パート 1 個を生バイトで読む。`core_xml_frontmatter` / `parse_core_xml` は `docProps/core.xml` (Dublin Core: `dc:title` / `dcterms:created` または `modified` / `cp:keywords`) を `Frontmatter` にマップし、パート不在または `title` が空なら filename 派生タイトルに fallback する。`local_name_pub` は QName から namespace prefix を除いた local part を取る (`cp:title` → `title`)、要素名判定を prefix 非依存にする。`resolve_general_ref` は quick-xml 0.38+ の `Event::GeneralRef` (entity 参照 `&amp;` 等が `Event::Text` に畳み込まれず別 event として届く挙動) を解決する — docx.rs と pptx.rs の両方が同じ char-ref / named-entity 処理を必要とするためここに共通化した |
| `kb-mcp/src/parser/xlsx.rs` | (v0.11.0+) `XlsxParser` (`.xlsx`)、`is_binary() == true`。`XlsParser` (`.xls`) は残っているが **v0.14.0 以降 registry から到達しない** (AU-06): calamine は `Xls::new` の中でシートごとに密なセル格子を作り、BIFF が縛るのはシート 1 枚 (65,536 × 256 = `Data` 512 MB) であって workbook ではないため、最大矩形のシートを多数宣言した小さなファイルが kb-mcp に制御が戻る前にメモリを使い切る。しかも割り当て失敗は skip ではなくプロセス異常終了になる。塞ぐには `Xls::new` の前に CFB コンテナから BOUNDSHEET / DIMENSIONS を自前で読む必要があり、`.xls` の要望が出るまで見送っている。両者は `parse_workbook_bytes` を共通の入口とし、そこから形式別に dispatch する。ワークブックは登録拡張子に一致する reader (`calamine::Xlsx` / `calamine::Xls`) で開く (形式 probe はしない = 拡張子と実体が食い違う payload は parse せず reject)。`.xlsx` は開く前に解凍 pre-flight を通す: **全** entry を、**申告 uncompressed size** と **実際に展開したバイト数** (出力は捨て、残り budget + 1 バイトで打ち切る) の両方で `MAX_RAW_BINARY_BYTES` と照合する (= 「archive 全体の展開量が cap 以内」という、名前に言及しない不変条件)。zip 仕様は申告値を強制せず zip 8.6 も deflate 出力を申告値で bound しないため、zip-bomb を止めるのは後者 (実測: 申告 10 バイトの 101 KB crafted workbook が 100 MB に展開された)。拡張子で対象を絞らないのは意図的で、calamine はパートを rels の `Target` で解決しファイル名を見ない (`xl/worksheets/payload` も worksheet として読む) ため、名前ベースの選別は 3 回破られている (固定パス → `.rels` 漏れ → 拡張子前提そのもの)。代償として `xl/media/` の画像も budget に乗るので、raw cap 付近かつ中身の大半が画像の workbook は skip され得る。その後、空でないシートごとに 1 チャンク (見出し `Sheet: <name>`、行ごとにセルをタブ結合) を生成し、`SHEET_MAX_BYTES` (1 MiB) で truncate する — 行単位の境界セマンティクス (合計を cap 超過させた行はそのまま丸ごと emit してから、そのシートの抽出を打ち切る。行途中では絶対に切らない)。frontmatter は、バイト列が zip として開ける場合 (`.xlsx` は該当) は `ooxml::core_xml_frontmatter` 経由で `docProps/core.xml` から取得する |
| `kb-mcp/src/parser/docx.rs` | (v0.11.0+) `DocxParser`、`is_binary() == true`。`word/document.xml` を段落 (`<w:p>`) 単位で読み、`<w:pStyle w:val="HeadingN">` をセクション境界として扱う — `markdown.rs` が Markdown 見出しに使うのと同じ見出し階層チャンク化規則で、`exclude_headings` 対応も含む (除外見出し配下の本文は次の非除外見出しまで捨てる)。表 (`<w:tbl>`) のテキストは特別扱い不要: OOXML 上の入れ子構造 `w:tbl > w:tr > w:tc > w:p > w:r > w:t` により、表セルのテキストは通常の `<w:p>` 境界処理を通って自然に現在のセクション本文に取り込まれる。frontmatter は `ooxml::core_xml_frontmatter` 経由 |
| `kb-mcp/src/parser/pptx.rs` | (v0.11.0+) `PptxParser`、`is_binary() == true`。`ppt/slides/slideN.xml` エントリを集めて数値のスライド番号順にソートする (zip 内の格納順ではない)。スライドごとに 1 チャンク (見出しは `ctrTitle`/`title` placeholder shape にテキストがあれば `Slide N: <title>`、無ければ素の `Slide N`) を生成し、スライド内の表テキストも本文に含める。発表者ノートは末尾 `[notes]` セクションとして付加し、スライドの `ppt/slides/_rels/slideN.xml.rels` を読んで `notesSlide` relationship の `Target` を解決する — 意図的に同番号ファイル (`slideN.xml` ↔ `notesSlideN.xml`) の推測 heuristic にはしていない。編集後にスライド番号とノート番号がずれたケースで dry-run (plan Task 3.7) が誤帰属を実証したため。frontmatter は `ooxml::core_xml_frontmatter` 経由 |
| `kb-mcp/src/markdown.rs` | `crate::parser::markdown::MarkdownParser` への薄い shim。legacy `parse()` / `parse_with_excludes()` 公開 API を維持 |
| `kb-mcp/src/watcher.rs` | `notify-debouncer-full` を tokio channel 越しに受信。拡張子 + path でフィルタして `indexer::{reindex,deindex,rename}_single_file` にディスパッチ。MCP サーバと並走 (`tokio::spawn`) |
| `kb-mcp/src/transport/` | MCP transport 抽象。`mod.rs` (Transport enum + CLI/config 解決)、`stdio.rs` (stdio)、`http.rs` (rmcp `StreamableHttpService` + axum、`/mcp` と `/healthz` をマウント。v0.8.0+ で admin sub-router を追加: `/ui` + `/api/admin/status` + `/api/search` を `admin_host_check` middleware (= Host header の **exact match** で loopback alias + bind addr 限定) で gate)。`KbServerShared` を Arc 共有し session factory で接続ごとに軽量ハンドルを生成 |
| `kb-mcp/src/transport/webui_index.html` | (v0.8.0+) WebUI MVP placeholder HTML、`transport/http.rs::ui_index` で `include_str!` 経由 embed。Raw HTML + JS、CSS framework 不使用、`textContent` / `createElement` のみで XSS 安全 (= `innerHTML` 不使用)。Phase 3+ で本格 redesign する disposable placeholder。 |
| `crates/kb-mcp-tray/` | (v0.9.0+) Windows 限定 system tray binary (`kb-mcp-tray.exe`、GUI subsystem) で daemon の監視 + lifecycle 制御。5 秒間隔で `/api/admin/status` を polling し、4 状態 status dot (緑 = healthy / 黄 = indexing / 赤 = 1 分以上 down / 灰 = polling 待ち) を描画、right-click menu 6 項目 (Status / Open Web UI / Start / Stop / Restart / Quit Tray)。start は PowerShell `Start-ScheduledTask` (= `kb-mcp/src/service/windows.rs` と同 path) だが、**stop は違う** — v0.14.0 以降は `/api/admin/status` から daemon の `pid` を読み、Win32 API でそのプロセスを終了する (`OpenProcess` 1 回で handle を取り、image 名検証と `TerminateProcess` を同じ handle 上で行うので pid 再利用に当たらない)。`Stop-ScheduledTask` は両分岐で走るが、成否を決める役ではない: pid で止めた後は v0.9.1 以前の install が持つ task instance を掃除する best-effort (失敗は log して無視)、probe が使える pid を返さなかった場合 (status endpoint 不達を含む) は fallback として実行し、そのエラーは即 return せず後段の確認に委ねる。いずれの経路でも成否は **daemon の設定 bind アドレスを bind できるか** で判定し、機構自身の戻り値は信用しない。dual event loop: `tao` を main thread、`tokio` runtime を別 thread で spawn、`EventLoopProxy::send_event` で bridge。panic hook + `tracing-appender::rolling::daily` で `%LOCALAPPDATA%\kb-mcp\logs\tray.YYYY-MM-DD` に log 出力。library API (`install::install_autostart` / `uninstall_autostart`) は `kb-mcp service install --with-tray` / `service uninstall` / `service tray-install` / `service tray-uninstall` から呼ばれ、shell:startup `.lnk` shortcut を PowerShell `WScript.Shell` COM 経由で管理。cargo-dist は `kb-mcp-tray.exe` を `x86_64-pc-windows-msvc` のみ artifact 化。 |
| `crates/kb-mcp-svc/` | (v0.9.1+) Windows 限定の launcher binary (`kb-mcp-svc.exe`)。目的は `kb-mcp.exe serve` を **コンソールウィンドウ無し**で起動することだけ。`kb-mcp.exe` は console-subsystem binary で、Windows はプロセス開始の **前** に conhost を割り当てるため、プロセス内で隠しても ~1 秒間フラッシュしてから消える (プラットフォーム側の未修正挙動、microsoft/terminal#249)。本 crate は `windows_subsystem = "windows"` なので子に渡す console を持たない — が、**それだけでは足りない**: 継承できる console が無い console-subsystem の子は自分で `AllocConsole()` を呼び、結局新しい可視ウィンドウを作る。したがって spawn 時に `CREATE_NO_WINDOW` (0x0800_0000) を **必ず** 渡し、`CreateProcess` に子の console 割り当てを skip させる。この flag と windows-subsystem の親の組み合わせで初めて 0-flash になる。stdio を null にして `kb-mcp.exe` を detach spawn し、自身は即 exit する。v0.9.1 以降、Task Scheduler の Action は `kb-mcp.exe` ではなくこちらを指す。`child_args` は `serve` を無条件に前置するため、`kb-mcp/src/service/windows.rs::resolve_action_target` は Action の `-Argument` 節を空にする — この不変条件は **次回ログオンまで破綻が表面化しない**ので両側とも unit test 済。非 Windows ビルドは fail-fast stub にコンパイルされ workspace は全 OS でビルドできる。cargo-dist は `x86_64-pc-windows-msvc` のみ artifact 化。tray への影響: scheduled task 自身のプロセスは即終了するため、daemon の停止に `Stop-ScheduledTask` は使えない (tray 行を参照)。 |
| `kb-mcp/src/schema.rs` | Frontmatter スキーマ検証。`kb_path` 直下の `kb-mcp-schema.toml` を読み、`required` / `type` / `pattern` / `enum` / `min_length` / `max_length` / `allow_empty` を検証。`kb-mcp validate` CLI から呼ばれ、text / JSON / GitHub annotation 形式で報告 |
| `kb-mcp/src/embedder.rs` | `fastembed-rs` の薄いラッパ。`ModelChoice` で embedding モデル (BGE-small-en-v1.5 / BGE-M3) を選択。`RerankerChoice` + `Reranker` で optional な cross-encoder 再ランク |
| `kb-mcp/src/db.rs` | `rusqlite` + `sqlite-vec` + FTS5 (trigram)。`chunks` / `vec_chunks` / `fts_chunks` スキーマと CRUD を管理。`search_hybrid` (Reciprocal Rank Fusion。定数 k と bm25 列重みは v0.13.0 以降 `[search.fusion]` で設定可能、既定は `k = 60` と `2.0 / 1.0 / 1.0`) と v0.7.0 で追加した unbounded variant (MMR / parent retriever 用) を提供。`SearchFilters` 構造体でフィルタ引数 (path glob / tags / date range / min_quality) を集約、`MatchSpan` でバイトオフセット引用を表現 (v0.3.0 追加)。`chunks.level` (v0.7.0 追加) で h2 / h3 を区別 |
| `kb-mcp/src/db/schema.rs` | (v0.15.0+) スキーマ作成と前方マイグレーション。全コンストラクタが呼ぶ `Database::init` から実行されるので、**DB を開くことが更新すること**にあたる。 |
| `kb-mcp/src/db/search.rs` | (v0.15.0+) 検索: ベクトル KNN、FTS5 候補、両者を融合する RRF。**挙動が数値として観測できる**側の半分。 |
| `kb-mcp/src/db/storage.rs` | (v0.15.0+) ドキュメントとチャンク。1 文書の書き込みは `documents` / `chunks` / `fts_chunks` / `vec_chunks` を整合させる複数テーブル操作なので、呼び出し側が tx を持っていない時だけ自分で開く (`is_autocommit()`)。 |
| `kb-mcp/src/db/meta.rs` | (v0.15.0+) index 単位のメタデータ・統計・全体メンテナンス: `index_meta` (埋め込みモデル / 次元 / context mode)、document / chunk 件数、rename 検出用の path→hash 表、および (AU-71) `corpus_snapshot` — 件数と索引済み chunk の digest を **1 トランザクション内で**読む。 |
| `kb-mcp/src/mmr.rs` | (v0.7.0+) Maximal Marginal Relevance の貪欲再ランク + 類似度キャッシュ。`mmr_select` は post-rerank の候補プールに対して動き、`[search.mmr]` 設定または per-call `mmr` パラメータで gating される |
| `kb-mcp/src/parent.rs` | (v0.7.0+) 表示時 parent retriever。`apply_parent_retriever` がヒットチャンクを `expand_adjacent` (level 整合な隣接 sibling マージ) または `expand_whole_document` (`whole_doc_threshold_tokens` 未満チャンクの全文 fallback) で拡張する。score / rank / `match_spans` は元のヒットを保ち、`content` と新フィールド `expanded_from` のみが変わる |
| `kb-mcp/src/quality.rs` | チャンク単位の品質スコアリング (長さ / 定型語 / 構造シグナル) |
| `kb-mcp/src/graph.rs` | ベクトルインデックス上での Connection Graph BFS。`get_connection_graph` MCP ツールと `kb-mcp graph` CLI から利用 |
| `kb-mcp/src/eval.rs` | `kb-mcp eval` CLI 用のリトリーバル品質評価 (opt-in)。Golden YAML を parse し、各クエリを `db.search_hybrid` で実行、recall@k / MRR / nDCG@k を計算。`<kb_path>/.kb-mcp-eval-history.json` を読み書きして前回との差分を表示。`ConfigFingerprint` (v0.7.0+) は `mmr` / `parent_retriever` / `fusion` (v0.13.0+) を optional に保持し、設定違いの eval 実行を別 history entry として区別する。いずれもビルトイン既定値と異なるときだけ記録するため、旧 baseline との比較は維持される。`serve` / `search` / `index` の挙動は一切変えない |
| `kb-mcp/src/tune.rs` | (v0.13.0+) `kb-mcp tune` CLI 用の測定ツール (opt-in)。RRF 定数と FTS5 bm25 列重みの固定グリッドを golden query セット上で掃引し、nested leave-one-query-out CV (paired SE / selection stability / 副指標の非悪化。sign test も算出して report に載せるが `decide` は参照しない) で結果をガードした上で、貼り付け可能な `[search.fusion]` スニペットか「既定値維持」の結論のどちらかを出力する。自動では何も適用せず、reranker も一切使わない。`eval` の `GoldenSet` / `compute_query_metrics` と `db::fuse_rrf_ids` を再利用する |
| `kb-mcp/src/tune/grid.rs` | (v0.15.0+) `kb-mcp tune` が掃引するパラメータ空間と、掃引中に持ち回る per-query 状態。 |
| `kb-mcp/src/tune/stats.rs` | (v0.15.0+) 採否判定の統計 — 平均・標本 SD・paired SE・sign test と、採用閾値 `ADOPT_MIN_MEAN_DELTA` / `ADOPT_SE_MULTIPLIER` / `STABILITY_MIN`。 |
| `kb-mcp/src/tune/report.rs` | (v0.15.0+) 掃引結果の stdout 向け整形 (text は `print!`、JSON 形式も)。 |

## データフロー

```
.md / .txt / .pdf / .docx / .xlsx / .pptx ファイル
(Registry::extensions() でフィルタ。既定で有効なのは .md のみ)
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
- query → sanitize → `fts_chunks MATCH` + bm25 (top-N) — 既定では見出しに 2 倍の重み (`[search.fusion].bm25_heading_weight`、v0.13.0+)
- Rust 側で Reciprocal Rank Fusion (既定 `k = 60`、`[search.fusion].rrf_k`) → top-`limit` を返却
- (任意) cross-encoder reranker が上位候補を再スコアリングして返却
- (任意, v0.7.0+) MMR 多様性再ランクが大きめの候補プールから貪欲に `limit` 個を選択し、関連度と新規性のバランス (`lambda`)、同一 doc の penalty (`same_doc_penalty`) を効かせる
- (任意, v0.7.0+) parent retriever が短いヒットチャンクの `content` を隣接 sibling またはドキュメント全体に展開する。score / rank / path / `match_spans` は変えないため relevance signal は維持される

v0.7.0 のフルパイプラインは **`RRF → reranker → MMR → parent retriever → match_spans`**。各段は対応する設定が off なら no-op となるため、既定では v0.7.0 以前の挙動に等しい。narrative は [retrieval-pipeline.ja.md](./retrieval-pipeline.ja.md) を参照。

## Contextual Retrieval (v0.12.0+)

静的 Contextual Retrieval (feature-46) は、各チャンクが embedder / FTS index / reranker に渡る前に、ドキュメント構造由来の breadcrumb を前置する機能。すべて index 時に LLM 呼び出しなしで決定論的に生成される。`[contextual].enabled` で on/off する (v0.12.0 時点の既定は **off** ―― judgment gate の結果として false-by-default に転換した経緯は README の「Contextual Retrieval」節の A/B 数値を参照)。

- **`Chunk.context: Option<String>`** (`kb-mcp/src/parser/mod.rs`) は検索用の内部フィールドで、`search` / `get_document` の返却には一切現れない。`build_context(parts: &[&str]) -> Option<String>` が空要素・連続重複要素を skip しつつ `" > "` で結合し、BGE-small の 512 token 入力制限を守るため 200 文字 (char boundary 安全) で cap する。
- **2 系統の ancestry 生成**。コードベース内の parser 形状にそのまま対応する:
  1. **Markdown** (`kb-mcp/src/parser/markdown.rs`): level をキーにした ancestry `stack: Vec<(u8, String)>` を、見出し遷移のたびに現在の見出しの深さまで pop してから push する (h2→h4 のような level 飛びでも、架空の h3 を補わず最も近い浅い祖先を継承する)。`exclude_headings` で除外された見出しも stack には積まれる (除外セクション自体は chunk を生成しないが、その子孫の ancestry は正しく保たれる)。`context = build_context(&[title, ...ancestry, heading])`。
  2. **バイナリ / フラット形式** (PDF のページ chunk、Office の単一セクション chunk、プレーン `.txt`。`parser::single_text_chunk` および各形式の chunker 経由): これらは辿るべき見出し階層を持たないため、`[title]` のみの単層 context になる。
- **`chunks.context_text TEXT`** (`kb-mcp/src/db.rs`): nullable 列。feature-46 以前の DB には idempotent な `ensure_context_text_column` の `ALTER TABLE` で追加される。現在の `ContextMode` が `Static` のときのみ `Chunk.context` から埋まり、`Off` モードでは `NULL` のまま。
- **`fts_chunks` の第 3 列** `context` (`heading` / `content` に加えて): 2 列の legacy index は `ensure_fts_context_column` で migration される ―― virtual table を drop + 再作成し、`INSERT ... SELECT id, heading, COALESCE(context_text, ''), content FROM chunks` で repopulate する。並行 opener との競合を避けるため `BEGIN IMMEDIATE` トランザクション (`begin_immediate_tx`) でラップする。この repopulate は write lock を保持し続ける処理で、574 doc / 10,002 chunks の KB を embedding + reranker モデル同時ロード中の並行負荷下で計測したところ 9.7〜12.3s かかったため、`Database::init` は `busy_timeout` を 30,000ms に設定している (v0.12.0 で 10s から引き上げ)。これにより `serve` 常駐プロセスの `search` / `status` が、進行中の migration を `SQLITE_BUSY` で即失敗せず待機できる。Contextual BM25 のスコアリングは `bm25(fts_chunks, heading_weight, context_weight, content_weight)` 呼び出しで重み付けする。3 つの重みは v0.13.0 以降 `[search.fusion]` (`bm25_heading_weight` / `bm25_context_weight` / `bm25_content_weight`、既定 `2.0` / `1.0` / `1.0`) から来る — feature-47 以前はコンパイル時定数 `FTS_BM25_*_WEIGHT` だった。
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

- **stdout** = そのコマンドの *結果* を、指定された形式で出す。**既定形式はコマンドごとに違う**ので parse 前に確認すること: `search` / `graph` は `--format json` が既定、`eval` / `tune` / `validate` は human-readable な text が既定 (`validate --format github` も CI annotation を出す machine-consumed 形式):
  - `kb-mcp search` の結果 (`print_search_results`)
  - `kb-mcp eval` の golden query 評価結果
  - `kb-mcp tune` の sweep 結果
  - `kb-mcp validate` のレポート (`print_validate_report`。`--format github` が
    CI annotation 用に出す `::error file=…` 行も含む)
  - `kb-mcp graph` の connection graph (`print_graph`)
- **stderr** は人間向けの進捗 / 統計 / warning / error:
  - `kb-mcp index` の進捗行 (`Indexing ...`, `Done in ...`、各ファイル毎の `  indexed:` / `  renamed:` / `  deleted:`)。`--quiet` で per-file 出力を抑止 (start / found / done のサマリだけ残す)、`--progress` で `indicatif` バー (TTY) または定期 `Progress: N/M (P%)` 行 (非 TTY) に切替。両 flag は相互排他 + 既定 off (v0.7.8 追加)
  - `kb-mcp status` の統計 (`Documents: N`, `Chunks: N`)
  - `kb-mcp service install/uninstall/status/list` の全 message は stderr (= status / progress / 診断、規約準拠)。stdout は空。
  - すべての `tracing` / `eprintln!` 系診断メッセージ

新規 subprocess test を書く場合は、`kb-mcp/src/main.rs` の対応する `Commands::*` block を grep して、その subcommand が stdout / stderr のどちらに書くかを必ず先に確認する。**arm 自身ではなく helper (`print_search_results` / `print_graph` / `print_validate_report`) が出力している場合がある**点に注意。stdout に *CLI の結果* を書くのは上記 5 つだけで、`index` / `status` / `service` は stderr のみ。`serve` は別枠: CLI 出力は無いが、既定の stdio transport では **MCP プロトコル自体** が stdout を占有する (subprocess harness が drain し続けねばならないのはこのため)。grep は `println!` だけでなく **`print!` も** 対象にすること — `eval` / `tune` の text 分岐はそちらを使っている。

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
