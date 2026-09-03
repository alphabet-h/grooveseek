# 補足 (挙動リファレンス)

起動後の GrooveSeek の振る舞い — 何が索引され、どこに保存され、
どのファイルが拒否され、検索が 2 つの索引をどう合成するか。

> **English version**: [behavior.md](./behavior.md)

- **埋め込みモデル**: 初回実行時、選択した ONNX モデルが OS 標準のキャッシュディレクトリに DL される。2 回目以降は再利用。解決順:
  1. `FASTEMBED_CACHE_DIR` 環境変数 (設定されていれば)
  2. OS キャッシュ + `fastembed` (Linux: `~/.cache/fastembed`、macOS: `~/Library/Caches/fastembed`、Windows: `%LOCALAPPDATA%\fastembed`)
  3. CWD 直下の `.fastembed_cache` (最終フォールバック)
- **インデックス保存先**: SQLite DB は `--kb-path` の**親ディレクトリ**に `.groove.db` として保存される (例: `--kb-path ./knowledge-base` ならリポジトリルート)
- **Parser registry**: `[parsers].enabled` に列挙された拡張子のみインデックス対象。既定は `["md"]` (従来デフォルト)、`["md", "txt"]` で `.txt` にオプトイン (タイトルはファイル名派生)、`["md", "pdf"]` (v0.10.0+) で `.pdf` にオプトイン (詳細は下記 PDF インデックスの補足)、`["md", "docx", "xlsx", "pptx"]` (v0.11.0+) で Office ドキュメントにオプトイン (詳細は下記 Office ドキュメントインデックスの補足)、`["md", "rs"]` (v1.2.0+) でソースコードにオプトイン (詳細は下記ソースコードインデックスの補足)。`"py"` (v1.3.0+) も同じくソースコードだが、先に grammar plugin を置く必要がある。未知 id (例: `"rst"` / `"adoc"`) は起動時に拒否、空配列も「何もインデックスされない」事故防止のため拒否。このセクションが効くのは groove が**信頼する** config に書いた場合だけで、KB の隣で見つけただけの config は `[parsers]` を警告つきで既定へ戻す — このキーはどの parser を走らせるか、そして grammar plugin を読み込むかどうかを決めるため。[信頼する置き場所 / しない置き場所](configuration.ja.md#信頼する置き場所--しない置き場所) を参照
- **PDF インデックス (v0.10.0+)**: `[parsers].enabled = ["md", "pdf"]` でオプトイン。[oxidize-pdf](https://crates.io/crates/oxidize-pdf) (純 Rust) でページ単位にテキストを抽出し、空でない各ページが見出し `p.N` の 1 チャンクになる。PDF の `Title` / `CreationDate` メタデータがあれば frontmatter に反映、`Title` が無ければファイル名派生タイトルに fallback する。暗号化 PDF は warning 付きで skip (パスワード対応なし)。他のバイナリ形式と同様、`.pdf` にも 50 MiB の生バイト上限が適用され、超過分は実行全体を abort せず warning 付き skip になる。既知の制限:
  - **テキストが薄すぎる PDF は落とす**: 抽出文字数の平均が 50 chars/page 未満の PDF は warning 付きで skip され、一切インデックスされない。スキャン / 画像のみの PDF (**OCR 非対応**) はここに含まれるが、それだけではない — 表紙・ラベル・図版中心のスライドは、テキストが完璧に抽出できていてもここに落ちる。閾値を下げないのは、**この判定が本来狙う相手**であるスキャン画像 + 電子的に載せたページ番号 /「CONFIDENTIAL」ヘッダだけの PDF が **39 chars/page** を出すため (2026-08-10 実測)
  - **CJK PDF は v0.15.2 以降正しく抽出できる**。予約 CMap + `/ToUnicode` 無しの CID-keyed フォント (ReportLab の出力形式) を含む。旧版でこの形が文字化けした原因は oxidize-pdf 側 — `/DescendantFonts` を CIDFont が間接参照で書かれている場合しか読まなかった — で、本プロジェクトが報告・修正した ([bzsanti/oxidizePdf#469](https://github.com/bzsanti/oxidizePdf/issues/469)、修正は oxidize-pdf 4.3.0 に収録、v0.15.2 で取り込み)。**`/ToUnicode` 付きの TrueType サブセットを埋め込む日本語 PDF — Word / LibreOffice / Google ドキュメントの出力形式 — は従来から正しく抽出できていた** (2026-08-10 実測: 密な日本語レポートで 569 chars/page)
  - **text layer が文字化けに復号された PDF は索引せず skip する** (v0.15.1+)。上記 CID 修正後は、他の原因による復号失敗への防衛層として維持している。groove は 2 つのシグナルで検出し — 抽出文字の 1% 以上が C1 制御コード U+0080–U+009F (正しく復号できたテキストには決して現れない)、および C1 を出さない清音かな主体の形を捕まえる「UTF-16BE を 1 バイトずつ読んだ交互パターン」(`あ` が `0B` になる) — どのクエリにも一致しないテキストを索引に入れることを拒否する。診断もページ密度のせいにせず復号失敗を名指しする
  - **多段組レイアウトの reading order 乱れ**: 抽出順は PDF 内部のテキスト描画順に従うため、複雑な多段組レイアウト (スライド資料等) では列が入り交じることがある。単一段組の文書は影響を受けない
  - **`Title` メタデータのゴミは filename fallback しない**: filename fallback は PDF の `Title` フィールドが空の場合のみ発火する。空ではないが無意味な自動生成タイトル (エクスポートパイプライン由来の残骸等) はそのまま使われる
  - **ハイフン結合は保守的なヒューリスティック**: 行末の `-\n` は、`-` の直前と `\n` の直後がともに ASCII 小文字の場合のみ結合する (型番・日付・CJK に隣接するハイフンを誤って壊さないため)。この結果、本来結合すべき単語分断が結合されない、あるいは偶然の小文字-小文字の並びを誤って結合してしまうケースが稀にある

  実際の日本語 PDF での dogfood (2026-07-19) で発見した `oxidize-pdf` の癖には対処済み: `/Title` が PDF 仕様の UTF-16BE 文字列形式 (非 ASCII タイトルで一般的) の場合、この依存クレートは byte-order-mark を検出できず 1 byte ずつ mis-decode して文字化けを生む。groove はこの mis-decode パターンを検知して元のタイトルに復元する。復元できない (あるいは復元結果もなお不自然な) 場合は文字化けをそのまま出さず filename fallback に倒す。抽出されたページ本文 (`content`) はそもそもこの問題の影響を受けていない — 化けるのは `title` フィールドのみだった
- **Office ドキュメントインデックス (v0.11.0+)**: `[parsers].enabled = [..., "docx", "xlsx", "pptx"]` でオプトイン。各形式とも自前実装 (LibreOffice / MS Office への依存なし):

  | 拡張子 | ライブラリ | チャンク粒度 | frontmatter 由来 |
  |---|---|---|---|
  | `.docx` | zip + [quick-xml](https://crates.io/crates/quick-xml) | Markdown と同じ規則の見出し階層セクション (`Heading1`〜`Heading6` 段落スタイルがセクション境界) | `docProps/core.xml` (Dublin Core: title / created / keywords) |
  | `.xlsx` | [calamine](https://crates.io/crates/calamine) | 空でないシートごとに 1 チャンク (見出し `Sheet: <name>`)、シートあたり 1 MiB で truncate (行単位境界 — cap 超過を招いた行はそのまま保持してから打ち切り) | `docProps/core.xml` |
  | `.pptx` | zip + quick-xml | スライドごとに 1 チャンク (見出し `Slide N: <title>`、title placeholder が無ければ `Slide N`)、発表者ノートは末尾 `[notes]` セクションとしてスライドの `.rels` 関係を解決して付加 (同番号ファイルの推測はしない = notes の誤帰属を避ける) | `docProps/core.xml` |

  既知の制限:
  - **legacy バイナリ形式は非対応**: 2007 年以前の `.doc` (Word) / `.ppt` (PowerPoint) / `.xls` (Excel) は非対応 — 対応するのは上記の OOXML 形式 (`.docx` / `.pptx` / `.xlsx`) のみ

    `.xls` は v0.11.0〜v0.13.1 でインデックス対象だったが v0.14.0 で取り下げた: calamine は workbook を開く時点で全体を密に確保し、BIFF が縛るのは**シート 1 枚**であって **workbook ではない**ため、小さな細工ファイルでメモリを使い切れる。しかも割り当て失敗はファイルの skip ではなくプロセスの異常終了になる。`[parsers].enabled` に `"xls"` を書くと起動時にこの理由付きで拒否される — `.xlsx` に変換すれば streaming で読める。**原本は残すこと**: 変換でセルのテキストは引き継がれるが一般に無損失ではない (VBA マクロは `.xlsm` が必要、その他のレガシー固有機能も失われうる)。詳しい理由: [ADR-0001](decisions/0001-withdraw-xls-legacy-biff-support.ja.md)
  - **OpenDocument 形式は非対応**: `.odt` / `.ods` / `.odp` は非対応
  - **パスワード保護ファイルは復号ではなく skip**: 暗号化された Office ファイルは (zip / BIFF コンテナが開けないことで) 検出され、実行全体を失敗させず warning 付きで skip される — パスワード対応なし
  - **表構造は plain text 化される**: `.docx` / `.pptx` の表セルは通常のテキストとして読み取られる (行/列構造はチャンク内に保持されない)。`.xlsx` の行は 1 行ごとにタブ区切りで連結される。下流の検索が見るのはグリッドではなく地の文

  `.pdf` と同様、この 4 形式も 50 MiB の生バイト上限 (`MAX_RAW_BINARY_BYTES`) を indexer の size-skip guard と `get_document` の両方で共有する。
- **ソースコードインデックス (v1.2.0+)**: `[parsers].enabled = [..., "rs"]` でオプトイン。単位は見出しではなく**定義** — grammar 自身の tags query から取る。関数・struct・method がそれぞれ 1 チャンクになる。定義のチャンクは直上に書かれた doc comment から始まり (間に空行があればそこで切る。空行で隔てられた comment はその定義ではなくファイルへの注釈)、囲んでいるスコープを context として持つ。同じファイルにある同名の 2 つの method が区別できるのはこれによる。どの定義も覆わない範囲 — import、トップレベルの文、`impl` ブロックを囲む波括弧、parser が解釈できなかった領域 — は行で埋めるので、構文エラーのあるファイルは 1 チャンクに潰れるのではなく、壊れた箇所の周囲の定義を差し出す。quality filter の短さ閾値に満たない断片は捨てる (捨てるとファイルが何も産まなくなる場合は残す)。`[parsers.code].max_chunk_chars` (既定 3500 非空白文字) を超えた定義は入れ子の定義へ、入れ子が無ければ行で割る — method は入れ子を持たないので後者が通常。割れた各片は元の定義の見出しと種別を保つ。末尾の片が単独で立つには薄すぎる場合 (同じ短さ閾値未満) は 1 つ前の片に畳み込む — 本文が閉じ括弧だけのチャンクを作らないため。hit は `start_line` / `end_line` / `symbol_kind` を持ち、ソース由来でないものには**キーごと現れない**。行範囲は定義ではなくチャンクを指すので、その行でファイルを開けば返ってきたものがそこにある。`parent_retriever` が hit を広げたときは行範囲も一緒に広がり、merge した全チャンクを覆う範囲になる — merge したチャンクが揃って行範囲を持たない場合はキーごと落ちる。`symbol_kind` は、答えに 2 つ以上のチャンクが入った時点で落ちる — 本文が単一の定義を指さなくなるため。`symbol_kind` は言語のキーワードではなく grammar の語 — Rust の tags query は struct も enum も union も `class` と呼ぶ。結果は既定でコードと散文が混ざる。分けるには `tags_any: ["code"]` か、先頭 `!` の `path_globs` を使う。v1.4.0 から**定義は quality filter の長さ由来の 2 減点を免除される**ので、1 行の定義も隠されずに返る: `MAXYEAR = 9999` が短いのは定数という形式のせいであって中身が薄いからではなく、その値は index の他のどこにも書かれていない。免除はこれ以上細かくできない — `pub mod x;` や unit struct は `type ShardId = u64;` とまったく同じ 2 減点を取るので、名前だけの宣言も一緒に返る。**しきい値でも戻せない** — 免除された定義のスコアはちょうど `1.0` で、`min_quality` は `1.0` で clamp され、チャンクが落ちるのは閾値を**下回った**時だけなので、`pub mod x;` だけを外せる値は存在しない。要らなければパスで切る (先頭 `!` の `path_globs`)、`tags_any: ["code"]` で反対側を取る、あるいはその木では `[parsers].enabled` にその言語を入れない。そもそもどの 1 行が定義になるかは grammar の判断で、言語ごとに違う: Python の tags query は module 直下の代入を拾うが、Rust の tags query は定数を 1 つも拾わないので、Rust の `const` は定義ではなく行単位の gap として扱われる。v1.4.0 より前に作った index は次の `groove index` で追随する — 書き換わるのは `quality_score` 列だけで、再 embedding は起きない。既知の制限: 1 MiB を超えるソースファイルは warning 付きで skip する — tree-sitter の allocator は OOM で unwind せず abort するため、parser が見る前に拒む必要がある。構文木の祖先を 64 個より深く辿る位置に定義があるファイルは、定義単位ではなく行単位で chunk 化し `parse:too-deep` を付ける — 定義ごとのスコープ解決は深いほど高くつき、この上限が無かった頃は 10 KB・1000 段のファイル 1 本の index に 64 秒かかった <!-- via: target/release/groove.exe --config <cfg> index --force -->。`target/` は既定の `exclude_dirs` に入っているので、リポジトリのルートで `rs` を有効にしてもビルド成果物は index されない — ただしこのキーを指定するとリスト全体が**置き換わる** (追加ではない) ので、独自の `exclude_dirs` には `target` を書き直す必要がある。Rust は焼き込み済みで、他の言語は利用者が置くライブラリとして届く。最初の 1 つが Python (v1.3.0+) — [grammar plugin の置き方](clients.ja.md#grammar-plugin-の置き方-v130) 参照。[ADR-0012](decisions/0012-chunk-code-at-its-definitions-and-fill-the-gaps-by-line.ja.md)、入れ子の上限については [ADR-0014](decisions/0014-bound-the-chunker-by-the-shape-of-its-input.ja.md)、短い定義が filter されずに返る理由については [ADR-0015](decisions/0015-let-a-definition-be-short.ja.md) 参照
- **ライブ同期ウォッチャ**: `groove serve` は `notify` ベースの watcher を既定 spawn (`[watch].enabled = true`、500ms debounce)。手動 save / `git pull` / 外部スクリプトを MCP ツールと同じ Mutex 付きリソース上で増分再インデックスするため、同時トリガは直列化される。`--no-watch` / `[watch].enabled = false` で無効化
- **HTTP トランスポート**: `--transport http --port 3100` で rmcp の Streamable HTTP を `/mcp` に提供し、`/healthz` をプローブ用、内部は Mutex 直列化。既定 bind は `127.0.0.1:3100`、`0.0.0.0` は明示 opt-in。**GrooveSeek は設計として認証を持たない** ([stability.ja.md](stability.ja.md)) ので、境界はコンテナ / リバースプロキシ / 前段アプリが持つ。ポートに到達できるものはナレッジベース全文を読める
- **埋め込み次元**: `--model` で決まる。BGE-small-en-v1.5 = 384、BGE-M3 = 1024。選択した次元は `vec_chunks` 仮想テーブルに宣言され `index_meta` に記録される。実行時の不一致は検出して拒否
- **増分インデックス**: ファイルは SHA-256 content hash で追跡。以降の `index` 実行では変更されたファイルのみ再 embedding される (`--force` を渡さない限り)。内容を変えずに移動 / リネームすると hash 一致で検知され `documents.path` の UPDATE として処理 — 既存の chunk / embedding / FTS 行は再利用される。再構築サマリでは `updated` / `deleted` の隣に `renamed` としてカウントされる
- **read 不能 / 非 UTF-8 ファイルへの耐性**: read 失敗・size cap 超過・parse 失敗のファイルは warning を出して skip されるだけで `index` 実行全体は abort しない — `--kb-path` にバイナリファイルが混ざっていても、それ以外の knowledge base のインデックスは壊れない
- **サイズ上限**: ファイル 1 本あたり生バイト 50 MiB を、read する前に `stat` で判定する。バイナリ形式 (`MAX_RAW_BINARY_BYTES`) だけでなく **テキスト形式 (`MAX_RAW_TEXT_BYTES`、v0.17.0 以降)** にも適用される。以前テキストは無制限で、巨大な `.md` 1 本で内容が丸ごとメモリに載った — `rebuild_index` は MCP ツールなのでクライアントから誘発できた。上限超過のファイルは、どちらの上限に当たったかを明示した warning とともに skip される
- **ハイブリッド検索 (FTS5 + ベクトル)**: `search` ツールは SQLite FTS5 全文検索 (trigram tokenizer、日本語 / CJK も動く。v0.12.0 以降は `heading` / `context` / `content` の 3 列で、bm25 では既定で `heading` を 2 倍重み) をベクトル検索と Reciprocal Rank Fusion (既定 `k = 60`) でマージする。重みと `k` は v0.13.0 以降 `[search.fusion]` で設定でき、自分の KB で動かす価値があるかは `groove tune` が測る。返される `score` は RRF スコア (大きいほど良い) で距離ではない。v0.16.0 以降、クエリは逐語で検索されるのではなく token 単位の phrase にコンパイルされて `OR` で結合される ([docs/usage.ja.md](usage.ja.md#コマンドラインからの一発検索) の「コマンドラインからの一発検索」を参照)。有効な phrase が 1 つも作れないクエリはベクトルのみにフォールバックするが、断片がすべて短いクエリはその前にクエリ全体の逐語 phrase へ fallback するので、ベクトルのみになるのは trim 後 3 文字未満 (trigram の最小値を下回る) のときだけ。v1.1.0 以降、whitespace 区切りの group の先頭にある `-` は検索するのではなく両脚から除外する (`"-foo"` は先頭ハイフンを逐語検索する) — 詳細は [ADR-0011](decisions/0011-exclude-a-term-from-both-halves-of-the-search.ja.md)
- **任意の再ランク**: `--reranker <model>` を付けると上位候補が cross-encoder で再スコアされてから返る。再ランク適用時は `score` が RRF 値ではなく cross-encoder の生スコアになる。再ランクは index 非依存 — サーバ起動時に再インデックスなしでトグル可能
- **Connection graph**: `get_connection_graph` / `groove graph` はドキュメント起点でベクトルインデックス上を BFS する。追加インデックスは作らず、**展開されたノードごとに** sqlite-vec KNN を新規発行する。ANN 索引は無いので、KNN 1 回で KB の全ベクトルを走査する。

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
- **見出し除外**: 見出しテキストが `exclude_headings` のいずれかを含むセクションは、チャンキング時に落とされる。既定は空リスト (全セクション残す)。`groove.toml` の `exclude_headings` に substring を列挙するとオプトインになる。マッチは部分文字列 (`heading.contains(pattern)`) で、短いパターンは `"参考リンク"` → `"## 参考リンク (旧)"` のような変種も拾う
- **ディレクトリ除外**: `walkdir` は basename が `exclude_dirs` のいずれかと一致するディレクトリ (とその subtree) をスキップする。照合は名前全体、かつ**大文字小文字を区別しない**: Unicode の小文字マッピング + ギリシャ語 final sigma の畳み込みで比較するので、`["résumé"]` は `RÉSUMÉ` に、`["οσ"]` は `ΟΣ` に一致する。full Unicode case folding ではない (`straße` と `STRASSE` は別物のまま)、また正規化もしない (結合文字で書かれた名前と合成済みの名前は別物のまま)。したがって `exclude_dirs = ["build"]` は `Build` というディレクトリも除外する — Windows / macOS ではこの 2 つは同一ディレクトリなので、完全一致にするとディスク上の綴り次第で除外設定を素通りできてしまうため。この規則は full index walk・`groove validate`・live watcher の 3 つすべてに等しく適用される。既定は `[".obsidian", ".git", "node_modules", "target", ".vscode", ".idea"]`。ユーザ指定リストは既定を完全に置き換える (merge ではない)。`exclude_dirs = []` を明示しても `.git` / `.svn` / `node_modules` は fail-safe として除外され続ける
- **`.grooveignore`** (v0.21.0+): KB の**ルート**に `.grooveignore` を置くと、[gitignore 構文](https://git-scm.com/docs/gitignore)でパスを除外できる — `*` / `?` / `[a-z]`、3 種類の位置の `**`、ディレクトリ限定の末尾 `/`、ルートに固定する先頭 `/`、前の行の除外を打ち消す `!`。`exclude_dirs` がディレクトリ名しか書けないのに対し、こちらはファイル単位・glob 単位で書ける: `drafts/`、`*.tmp.md`、`archive/**`、`notes/*.md` + `!notes/keep.md` など。

  読むのはこの 1 枚だけ。サブディレクトリの ignore ファイルは見ないし、`kb_path` より上へも遡らないし、`.gitignore` も見ない — git 管理の KB は「大きすぎて git に入れない、しかし索引はしたい」ファイルをちょうど gitignore していることが多く、リポジトリ側の都合で索引内容が変わらないようにするため。同じパターンが欲しければコピーすること。

  3 層は **union**: 組み込みの `.git` / `.svn` / `node_modules` fail-safe → `exclude_dirs` → このファイル。どれか 1 つでも除外と言えば除外なので、`!` が打ち消せるのは **`.grooveignore` 内の前の行だけ**で、`exclude_dirs` や fail-safe が外したものは戻せない。照合は `exclude_dirs` と同じく**大文字小文字を区別しない**ので、同じファイルが Linux でも Windows / macOS でも同じ挙動になる。git と同様、除外されたディレクトリ配下のファイルは後続の `!` 行では復活できない (walk がそのディレクトリに降りないため)。

  この規則は full index walk・`groove validate`・live watcher の 3 つすべてに等しく適用される。サーバ稼働中に編集した場合、効くのは**それ以降のファイルイベント**で、すでに index に入っている文書は次の `groove index` (または MCP `rebuild_index`) まで残る — その実行時にファイルが読み直され、除外対象になった文書が落ちる。ファイルが存在するのに読めない場合 (hardlink / symlink / ディレクトリ / 64 KiB 超 / 1000 パターン超) は warning を出し、そのファイル (または超過分) 無しで続行する。起動を止めることはしない。

  **これは索引の境界であってアクセスの境界ではない**。除外されたファイルは索引されないので `search` にも `get_connection_graph` にも絶対に出ない (どちらも DB から読むだけでファイルシステムに触らない)。一方、パスを知っている呼び出し元は `get_document` で読める — `exclude_dirs` 配下のファイルが従来そうであったのと同じ。これは見落としではなく意図的な線引きで、KB に書ける者は `.grooveignore` を消すこともできる以上、木の中に置いたルールがその木を守る境界にはなり得ないため。読ませたくないものは `kb_path` の外に置くこと
- **リンクは辿らない (hardlink も同じ)**: symlink は index されず、watcher も拾わず、`get_document` も返さない。KB に書ける者が「**自分では読めないファイル**を groove の権限で読ませ、`search` から回収する」ことを防ぐため。hardlink は見た目が普通のファイルのまま同じことができる — 1 つのファイルに 2 つ目の名前を付けるだけで、作成に読み取り権限は要らず、Windows では特権すら要らない — ので、**名前が 2 つ以上あるファイル**は同じ 3 箇所で同じように拒否する。拒否時はファイル名と理由をログに出す。判定は意図的に粗い: 「もう一方の名前が KB の内か外か」を portable に知る方法が無いため、**正当な hardlink (dedup したノート、2 つの KB で共有しているファイル) も同様に skip される**。index に入れたいならコピーに置き換えること。リンク数がそもそも読めない場合 (削除直後など) は通すので、削除は従来どおり index に反映される。v0.20.0 以降、**判定とバイト列は同じ handle から取る**: リンク数・ファイル種別・サイズ上限はすべて「実際に中身を読む handle」から読むので、検査を通した後にそのパスへ hardlink を rename で被せても、読まれずに拒否される。Unix ではその open が symlink を辿ることも拒否し、名前付きパイプで待たされることも防ぐ。Windows ではどちらも入れていない — symlink の作成に管理者権限が要る (実測済) 一方、reparse point を拒否すると **OneDrive の placeholder が全滅する**ため。**それでもこれは「敷居を上げる」であって境界ではない**: リンクを張った上で**元の名前を消せる**者 (= そのディレクトリへの書き込み権限。ファイルの読み取り権限は不要) は KB 側の名前だけを残せて、最初からそこにあったファイルと区別できない — リンク数は「今の状態」であって出自ではない。**途中のディレクトリ**を symlink に差し替える経路はどのプラットフォームでも塞げていない。そしてリンク数は結局ファイルシステムが答える値でしかなく、FAT32 / exFAT や大半のネットワーク共有は真偽に関わらず 1 を返すので、**USB メモリや共有ドライブ上の KB はこのガードの保護を一切受けない**。**groove に読ませたくないものは `kb_path` の外**、groove の実行ユーザが読めない場所に置くこと
- **`get_best_practice` path templates**: opt-in 機能で、使うには `groove.toml` の `[best_practice].path_templates` を設定する必要がある。各テンプレートは `{target}` をプレースホルダとして使える (例: `"best-practices/{target}/PERFECT.md"`、`"docs/{target}.md"`)。サーバはリスト順に試して `kb_path` 配下に最初に存在したファイルを返す (path traversal は拒否)。セクション省略 or `path_templates = []` の場合はツール自体は登録されるが "not configured" エラーを返すため、意図しない呼び出しは明示的に失敗する
- **チャンク単位品質フィルタ** (**既定有効** 閾値 `0.3`): インデックス時に各チャンクに対し 3 つのシグナル — 長さ (30 文字未満 → -0.6)、定型語のみ (TBD / TODO / 詳細は後述 等 → -0.5)、弱い構造 (80 文字未満の 1 行 → -0.3) — から `quality_score` を計算。**長さ由来の 2 シグナルは 2 種類のチャンクで免除される** (定型語のみの減点は常に適用): バイナリ形式 (`.pdf` / `.docx` / `.xlsx` / `.pptx`) 由来のチャンク — ページ / シート / スライドが短いのは形式の構造であって薄い節ではない — と、v1.4.0 以降のソースコードの定義チャンク (同じ理由)。閾値未満のチャンクは `search` / `groove search` / `get_connection_graph` で非表示。`get_connection_graph` の seed チャンクは免除。フィルタ無効化は `groove.toml` の `[quality_filter] enabled = false`、per-query は CLI `--include-low-quality` / MCP `include_low_quality: true`。閾値上書きは `--min-quality 0.5` / `min_quality: 0.5`。既存 index のアップグレード: 次の `groove index` 実行時に `quality_score` 列が透過的に追加され (ALTER TABLE)、1 度だけ backfill される (冪等)。同じ pass が v1.4.0 より前の規則で採点された定義チャンクも採点し直す。書き換えるのはその列だけなので再 embedding は起きず、`--force` も要らない

## Related

- `docs/configuration.ja.md` — この挙動を操るキー
- `docs/retrieval-pipeline.ja.md` — 検索パイプラインの全体像
- `README.ja.md` — インストールとクイックスタート
