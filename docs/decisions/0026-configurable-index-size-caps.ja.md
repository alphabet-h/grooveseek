# 26. 索引のサイズ上限を設定可能にし、無制限は綴らせ、読み出し上限は据え置く

- Status: accepted
- Date: 2026-10-01
- Deciders: プロジェクトオーナー
- Applies to: v1.14.0 の次のリリース

## 背景と課題

この記録までは、1 つの定数 `MAX_RAW_BINARY_BYTES` (50 MiB、`grooveseek/src/parser/mod.rs`)
が 6 つのことを同時に決めていた: indexer がバイナリファイルを読む前に skip するサイズ、
読む handle での同じ判定、`.xlsx` の展開 pre-flight の budget、`.docx` / `.pptx` のパート
読み出しの累積 budget、PDF 1 本のテキスト budget、そして `get_document` / `resources/read`
のバイナリ上限である。`MAX_RAW_TEXT_BYTES` は最初の 2 つを Markdown とプレーンテキストに
ついて持っていた。

300 MB の workbook は普通の業務に存在し、warning 付きで skip されていた。上限を全員に
対して上げるだけでは済まない: `rebuild_index` はクライアントがいつでも呼べる MCP ツールで、
ファイルは parse の間まるごとメモリに持たれ、calamine は `.xlsx` の共有文字列表も丸ごと
メモリに載せ、しかも確保に失敗すると process が abort する。abort は `catch_unwind` でも
別スレッドでの parse でも止まらない。

問いは、設定で求めていない構成には何も変えずに、運用者がそうしたファイルを分かって
受け入れられるようにする方法である。

## 判断の軸

- 新しいキーを 1 つも書いていない設定は、以前とまったく同じものを索引する。
- 1 つの上限を上げることが、別の防御を黙って外してはならない。
- 「無制限」は分かって選ぶもので、2 通りに読まれない綴りにする。
- MCP の 1 リクエストが握るメモリは、索引の設定に関わらず有界のまま。
- 運用者が選んでいない設定が、1 回の run がファイルをどれだけメモリに持つかを上げられ
  てはならない。

## 検討した選択肢

1. **「無制限」の綴り。**
   - `0`。却下: 製品ごとに意味が違う。Recoll は `filtermaxseconds` では無制限、
     `compressedfilemaxkbs` では「全拒否」と読み、Zoekt は「既定を使う」と読み、Open WebUI は
     未設定または `0` の `RAG_FILE_MAX_SIZE` を、同じ製品の中でアップロードでは無制限、
     アーカイブ展開では 100 MB として扱う。この repository は既に
     `[embedding].max_input_chars = 0` を拒否している。
   - `-1`。最も多い選択 (Elasticsearch `indexed_chars`、Apache Tika、Solr)。却下: TOML 上で
     負のバイト数は意図として読めず、「では `-2` は?」を招く。
   - キーを書かなければ無制限。却下: 調べたサーバ型の製品 (Onyx、Dify、Meilisearch、
     Qdrant、Weaviate、Nextcloud) はすべて未設定を有限の既定値として扱う。
   - **文字列 `"unlimited"`。** 採用。`0` と負値はエラーにし、メッセージでこれを案内する。

2. **展開 budget。**
   - raw の上限に連動させる。却下: workbook の中の XML はファイルの数倍に展開されるので、
     `max_binary_file_size = "300 MiB"` でも 300 MB の workbook は展開段で skip され、
     通す方法が `"unlimited"` しか残らず、それは全ファイルから zip-bomb の検査を外す。
   - raw が `"unlimited"` の時だけ連動させる。却下: 「無制限」が 2 つ目の防御を黙って外す。
   - Apache POI (最小展開比 1%) や Tika (出力:入力比 100) のように比率で縛る。今回は却下:
     既存の pre-flight は絶対バイト数を 2 層 (申告値、実際の展開量) で数えており、軸を
     変えるのは別の判断である。
   - **独立したキー `max_decompressed_size`、既定 50 MiB、raw の上限と非連動。** 採用。

3. **読み出しが返すもの。**
   - `get_document` / `resources/read` を索引の上限に連動させる。却下: MCP の 1 リクエストが
     300 MB のファイルをメモリに持つことになる。
   - **読み出し上限は据え置く: バイナリ形式 50 MiB、テキスト 1 MiB。** 採用。それを超えて
     索引された文書は、索引に記録したサイズ
     ([ADR-0005](0005-record-document-size-in-the-index.ja.md)) によって、検索には出るが
     `uri` を持たない。

4. **キーの置き場所。** `[limits]` の新設は却下した。`[index]` が既に `groove index` と
   `rebuild_index` の設定を持ち、上限は索引の性質そのものである。

5. **バイナリとテキストを 1 キーにする。** 却下: テキストには chunk の大きさの上限が無い
   (Markdown は見出しでしか切らない) ので、共有の上限を上げると見出しの無い `.txt` 1 本が
   巨大な 1 chunk になり得る。

## 決定

- `[index]` に `max_binary_file_size` / `max_text_file_size` / `max_decompressed_size` を
  置く。値は 1 以上のバイト数、単位付きの大きさ (`B` / `KB` / `MB` / `GB` / `TB` は 1000 進、
  `KiB` / `MiB` / `GiB` / `TiB` は 1024 進、大文字小文字を区別しない、小数は不可)、または
  `"unlimited"` (大文字小文字を区別しない)。既定はどれも 50 MiB。`0`、負値、小数、未知の
  単位、それ以外の語は、キー名を含むエラーになる。
- 上限は parser registry が運ぶ (`Registry::limits`)。indexer はすべての読み出し経路
  (フル run、watcher の作成・変更、rename) で raw の上限をそこから読み、バイナリ parser は
  構築時に展開 budget を受け取り (`with_budget`)、最も深い読み出しまで渡す。上限固定の
  helper は test 専用として残すので、本番の経路が 50 MiB でもう一度判定することは無い。
- 展開 budget による拒否は `max_decompressed_size` を名指しする。`.docx` / `.pptx` の
  パート 1 つが budget を超えて読み飛ばされる時の警告 (以前は何も言わずに捨てていた) も
  同じく名指しする。raw の上限による skip はこれまでどおり `file too large` と言う。
  stderr でどちらのキーかが区別できる。
- 読み出し側は 50 MiB を独自の名前 `GET_DOCUMENT_BINARY_MAX_BYTES` で持ち、同じ数が別の
  問いに答えていることを、索引の既定値と取り違えさせない。読み出しは展開 budget も
  `max_decompressed_size` に関わらず組み込みの値のまま: `get_document` と `resources/read` は
  `parse_bytes_for_read` を通して parse し、4 つのバイナリ parser はそこで構築時の budget では
  なく既定の budget を使うので、MCP の 1 リクエストはこれまでどおり bound される。
- 既定を超える値は process ごとに 1 回警告する: ファイルはまるごとメモリに持たれ、確保の
  失敗は process を abort し、PDF の抽出は 120 秒で打ち切られ、`"unlimited"` ではメモリ
  使用量は KB に置かれたファイルだけで決まる。
- KB の隣で見つかった設定は、この 3 キーをどちらの向きにも設定できない。警告して落とし、
  組み込みの上限を使う。`--config` で名指しすれば受け入れる。
- 上限を下げても、索引済みの文書は消さない (これまでどおり)。
- [ADR-0004](0004-resource-reads-are-bounded-by-the-index.ja.md) と
  [ADR-0005](0005-record-document-size-in-the-index.ja.md) が言う「50 MiB まで索引する」は、
  これからは既定値の話になる。両 ADR は編集せず、この記録を注記とする。

## 結果と代償

- **300 MB の workbook には 2 つのキーが要る。** `max_binary_file_size` がファイルを、
  `max_decompressed_size` がその展開後を受け入れる。2 つ目に書く値は運用者の workbook で
  測るもので、ここでは分からない。
- **メモリは設定に従う。** 上限を上げると、ピークメモリは最大のファイルに比例して増え、
  マシンが持てないほど大きいファイルは skip されずに process を終わらせる。警告はそれを
  伝えるが、防ぎはしない。
- **意図して上限を上げた KB では `groove doctor` が黄色のまま**になる。読み出し上限を超えて
  索引された文書は `larger-than-a-read-returns` (Warning、exit 1) として報告される — 1 MiB を
  超えるテキストが既に受けているのと同じ扱い。
- **`max_decompressed_size` を上げると、バイナリ文書はすべて提示から外れる。** 読み出しは
  組み込みの budget で parse し、索引が記録するのは raw の大きさだけで、どのバイナリ文書が
  budget を超えて展開するかは分からない。そのためキーが既定を超えている間、バイナリの hit は
  `uri` を持たず、`resources/list` も提示しない。検索には出続け、`get_document` は budget を
  超えて展開する文書を展開の文言で拒否するか、単独で budget を超えるパートを欠いて返す。
  テキストには影響しない。展開後の大きさを索引時に記録すれば、収まるバイナリ文書だけを
  提示できる — これは未着手。
- **上限を下げても、高い上限で索引したものは残る**。`groove index --force` を打つか、
  ファイルが上限の下まで縮むまで。scan の size skip は既存の行を消さずに保護するため。
  上限を超えたままの編集では古いテキストが検索に出続ける: watcher は新しい大きさだけを記録し、
  次の scan もそのファイルをまた skip する。
- **シート / ページ単位のテキスト上限 (1 MiB) は変わらない**ので、大きな workbook はシート
  ごとに切り詰められた 1 chunk として索引される。これを上げるには、先に chunk をシートより
  小さくする必要がある。
- **PDF 抽出のタイムアウト (120 秒) は raw の上限と一緒には動かない**。その根拠の計算は
  入力 50 MB を前提にしており、別に見直す。
- **見直す時**: シートが行ブロック単位の chunk に分かれた時 (テキスト上限も `[index]` に
  加えられる)、ファイルを buffer ではなく handle から parse するようになった時 (警告の
  「まるごとメモリに持つ」が嘘になる)、展開 budget を比率に移す時。
- **test が守る**: `grooveseek/src/parser/mod.rs` (値の文法と警告)、4 つの parser module と
  `registry.rs` (budget がそれぞれの読み出しに届く)、`grooveseek/src/config.rs` (キー、
  registry の両分岐、信頼しない設定の規則)、`grooveseek/src/indexer.rs` と
  `grooveseek/tests/index_size_caps.rs` (3 つの読み出し経路)、`grooveseek/src/server.rs`
  (読み出し側は動かない。raw の上限も展開 budget も)。

## 参考

- [ADR-0005](0005-record-document-size-in-the-index.ja.md)。記録したサイズが、読み出し上限を
  超えて索引された文書を提示から外す。
- [ADR-0004](0004-resource-reads-are-bounded-by-the-index.ja.md)。その有界な読み出しは
  この記録でも変わらない。
- キーと信頼しない設定の表は [configuration.ja.md](../configuration.ja.md)、運用者から
  見える挙動は [behavior.ja.md](../behavior.ja.md)。
- 英語版:
  [0026-configurable-index-size-caps.md](0026-configurable-index-size-caps.md)
