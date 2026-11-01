# 32. 索引に入らなかったファイルごとに理由を返し、件数は一覧から導く

- Status: accepted
- Date: 2026-11-01
- Deciders: プロジェクトオーナー
- Applies to: v1.24.0。library の `rebuild_index` と `reindex_single_file_explained`、
  および MCP の tool `rebuild_index`

## 背景と課題

groove の desktop の画面は、索引が読めなかったファイルが**どれ**で**なぜ**かを利用者に見せ、
無視するか手を打つか (パスワードを外す、上限を上げる、別名で保存し直す) を利用者が自分で
決められるようにしたい。利用者は開発者ではなく、stderr を見ない。

これまでの library はそれに答えられなかった:

- `IndexResult::skipped` は件数だけだった。ファイルごとの理由は stderr に流れて消えた。
- parser の失敗はどれも同じ `parse failed` になった。暗号化されたファイル、展開量の上限を
  超えたファイル、壊れたファイルの区別は stderr の行の文面にしか無かった。
- scan は skip した path を集合に集め、理由を捨てていた。
- hard link と、索引が持てない名前
  ([ADR-0023](0023-index-only-names-the-server-can-open.ja.md)) は warning を出すだけで
  `skipped` に数えられなかった。どちらも [usage.ja.md](../usage.ja.md) が `skipped` に与える
  定義「disk にはあるが索引されなかった」に入っていたのに。
- MCP の `rebuild_index` の返答も件数だけで、その呼び手は server の stderr を読めない。

## 判断の軸

- run ごとに一覧は 1 つ。件数はそれと食い違えない。
- 利用者が手を打てる違いを分ける: 暗号化、設定できる上限の超過 (どの key か)、時間の上限、
  parser の固定の上限、壊れている / 未対応、UTF-8 でない、文字が少なすぎる、文字化けに
  復号される。
- parser のエラー文は 1 文字も変えない: stderr の行と既存の test がそれを固定している。
- 失敗の分類は 1 か所で行い、全件の index と watcher の単一ファイルの reindex が共有する。
- 意図して外したファイル (ignore 規則、未対応の拡張子、Office の lock file、symlink) は
  「読めなかった」ではなく、そう見せてはならない。
- skip がいくつあっても MCP の返答の大きさは有界に保つ。

## 検討した選択肢

1. **一覧の置き場。** (a) `IndexResult` だけ。(b) 新しい `ProgressEvent::Skipped` だけ:
   desktop が event から一覧を自前で組み直すことになり、MCP は event を持たないので結局
   library にも一覧が要る。(c) 両方: (b) は `ProgressEvent` の「1 file 1 event」の順序を
   書き直すことになり、1 つの run で 2 回読まれるファイル (交差した rename の読み直し) が
   何を報告するかも決め直す。今それを要るものが無い。**(a) を採用。**
2. **件数の持ち方。** 書き込み点ごとに `skipped` を足し引きし、その隣で一覧に push / pop
   し続けるか、run の間は一覧だけを書き、最後に一度だけ `skipped` を一覧から導くか。前者は
   2 つが黙ってずれていく形である。**後者を採用。**
3. **parser が理由を伝える方法。** `anyhow` のエラーを新しい error 型に置き換える (エラー文の
   通り道がすべて変わる)、英語の詳細文を理由に持たせる (`Copy` でなくなり、大きさが parser の
   文で決まり、平易な文は画面側の持ち物)、`Display` が元の文のままの型の印を
   `anyhow::Error` の中に載せる。**印を採用。** 入れ子の理由 (`ParseFailed { why }`) は退けた:
   画面側は variant ごとに文へ写すだけで、入れ子は写す段を増やすだけ。
4. **hard link と、索引が持てない名前。** `skipped` は今のままにして、隣に 2 つ目の件数と
   一覧 (`left_out`) を足すか、`skipped` に数えるか。前者は不変条件を
   「一覧 == `skipped` + `left_out`」にし、どの消費者にも 2 つを足させる。
   **`skipped` に数える方を採用** (プロジェクトオーナーの決定): どちらも公開の定義に
   既に入っていて数え漏れていたので、field の意味を変えるのではなく件数を直すことになる。
5. **名前のせいで外したディレクトリ。** 索引が読むはずのファイルを含む時だけ載せるか、
   中身によらず 1 つ載せるか。前者は walk が入らない subtree を歩くことになり、費用が中身の
   量に比例し、その中のリンクと名前の扱いも決め直す。**ディレクトリと印を付けて 1 つ載せる方を
   採用。**
6. **MCP の返答。** 上限なし (有界でない)、理由ごとの件数 (desktop は library を使い、
   返答を読む model には先頭の entry で足りる)、入れ子の `reason` object (1 段深くなるだけで、
   避ける key の衝突も無い)。**先頭 N 件・flat な key・外した件数を採用。**
7. **struct variant ごとの `#[non_exhaustive]`。** crate の外からその variant を組めなくなり、
   画面側が写しの関数を index を回さずに unit test できない。退けた。enum 全体には
   `#[non_exhaustive]` を付ける。
8. **一覧を `IndexResult` の外で返す。** `rebuild_index` の呼び手すべてが受け取る型が変わり、
   不変条件が 2 つの値にまたがる。退けた。代わりに `IndexResult` に `Clone` を derive した。
   `Vec` を持つと、既存の 3 つの test の `..struct` の更新がそれを要したため。

## 決定

- **型。** `indexer::SkippedFile { path, reason }` と、`#[non_exhaustive]` の
  `indexer::SkipReason`。variant は 16: `TooLarge` (大きさ、上限、それを決める `LimitKey`)、
  `OverExtractionLimit`、`OverParserLimit` (key で上げられない固定の上限)、
  `ExtractionTimedOut`、`Encrypted`、`CorruptOrUnsupported` (parser の panic を含む)、
  `NotUtf8`、`TextUndecodable`、`TooLittleText`、`NoText`、`Unreadable` (`ReadFailure`:
  `InUse`・`PermissionDenied`・`Gone`・`Other` を持つ)、`EmbedRejected` (HTTP の status)、
  `ChangedDuringRead`、`NotPlainFile`、`HardLink`、`UnspellableName { directory }`。
  安定した snake_case の名前 (`too_large`、`encrypted`、...) を持つのは `SkipReason::kind()`
  だけ、`[index]` の key の名前を持つのは `LimitKey::key()` と `SkipReason::limit_key()`
  だけ。path は kb 相対で `/` 区切り、`documents.path` と同じ綴り。
- **一覧と件数。** `IndexResult::skipped_files` は `skipped` に数えたファイルをすべて、
  path ごとに 1 件、path 順に持つ。run の間に書くのは run ローカルの台帳だけで、書くのは
  `upsert` (同じ path の後の理由が前の理由を置き換える) と `remove` だけ。
  `skipped_files`、`skipped` (その長さ)、`unspellable` (その `UnspellableName` の件数) は、
  完走でも中断でもすべての run が通る終わりの 1 点で一度だけ書く。だから
  `rebuild_index` が返すどの値でも `skipped_files.len() == skipped` が成り立つ。
- **parser は原因に印を付ける。** 文書を断る parser は `Unindexable { cause: ParseCause }` を
  載せた `anyhow::Error` を返す。その `Display` は前に出していた文のまま。エラーを
  `SkipReason` にするのは `reason_of_parse_error` の 1 つ (印、無ければ UTF-8 の decode
  エラーは `NotUtf8`、それ以外は `CorruptOrUnsupported`)、I/O のエラーを分けるのは
  `read_failure_of` の 1 つ。
- **進捗の event には載せない。** `ProgressEvent` は変えない。
- **watcher の入口。** `reindex_single_file_explained` は `Reindexed { result, reason }` を
  返す。`reason` が `Some` になるのは、`rebuild_index` がその結果を `skipped` に数える時と
  ちょうど同じ。例外はリンクを通る path で、全件の index が意図して外し、どこにも載せない
  ので `None`。どちらの経路も同じ本体を通るので、分類が食い違わない。既存の関数は wrapper
  として残し、`watcher.rs` は変えない。
- **hard link と名前を数える。** walk は外したものを返し、その entry は一覧と `skipped` に
  入る。名前のせいで外したディレクトリは `directory: true` の 1 件。これらの path は、skip した
  ファイルの row を prune から守る集合には入れない: hard link になった索引済みの文書は、
  今までどおり row を失う。`unspellable` は `embed_rejected` と同じく `skipped` の内訳になる。
- **載せないもの** (今と同じ): `.grooveignore` / `exclude_dirs` で外したファイル、未対応の
  拡張子、Office の lock file (`~$*`)、symlink。
- **MCP の返答 (ここで決め、出すのは別の変更)。** `rebuild_index` は `skipped_files`
  (library の一覧の先頭 50 件をその順で。各 entry は `path`、`reason`、flat な詳細の key) と
  `skipped_files_omitted` (残りの件数) を足すことにする。この 2 つの field は同じ release
  (v1.24.0) の別の変更で入り、library の変更は serialize しない。それまで MCP の返答は件数
  だけで、`skipped` は hard link と名前を既に含む。
- **`groove index` は変えない**: 新しい出力は無く、`Done in` 行の文言もそのまま。
- **[ADR-0023](0023-index-only-names-the-server-can-open.ja.md) を 2 つの事実の点で改める**。
  決定は変えない: MCP の `rebuild_index` の返答は索引が持てない名前の件数を (`skipped` の中に)
  持つようになり、`Done in` 行の `not indexed` の件数は `skipped` の外ではなくなる。
  ADR-0023 にはそのことを日付つきの注記で書く。

## 結果と代償

- **`skipped` は hard link と名前の分だけ増える** (`groove index` の `Done in` 行、MCP の
  `rebuild_index`、`IndexResult`)。公開の定義に既に入っていたので minor の版。
- **`Done in` 行の `not indexed` の件数は `skipped` の内数になる (v1.24.0+)**。前は外にあり、
  2 つを足して読んだ。文言は変えない (test が固定している) ので、まだ足して読む人や script は
  名前を二重に数える。それを伝えるのは docs と changelog だけ。
- **索引済みの文書が hard link になった run は、それを `deleted` と `skipped` の両方に数える**:
  row を消したことと、ファイルが読めなかったことは別の事実で、二重計上ではない。
- **ディレクトリの entry は、索引が読むはずのファイルを含まないことがある** (画像だけ、空)。
  画面側は読めないファイルとしてではなく、名前が使えないフォルダとして見せること。
- **UTF-8 でない名前の lossy の綴りが同じになる別々のファイルは 1 つと数える**。`skipped` でも
  `unspellable` でも同じで、前は loop がファイルごとに数えていた。`embed_rejected` は
  ファイルごとのままなので、その場合は対応する一覧の entry より多くなり得る。
- **library の一覧に上限は無い**: skip したファイルごとに小さい entry 1 つで、run が既に持つ
  ファイルごとの状態の隣にある。上限は一覧が process の外へ出る所 (MCP の返答。上の別の変更で入る) で切る。
- **watcher の入口は、ファイルの最初の stat・解決・読みが失敗した時は今も `Err` を返す**。
  2 回目の読みの失敗は `Unreadable` の skip になる。`rename_single_file` は今も理由を捨てる。
- **`reason` の語彙は追加で育ち**、汎用の値 (`corrupt_or_unsupported`、`no_text`) で報告して
  いたファイルが、後でより具体的な値へ移ることがある。消費者は知らない値を汎用の失敗として
  扱う。具体的な値どうしの移動、値の削除や改名は major の変更。
  [mcp-tools.ja.md](../mcp-tools.ja.md) が MCP の変更と一緒にこれを書く。
- **見直す時**: desktop が skip を起きた順に見せたくなった時 (`SkipReason` が `Copy` なので
  `ProgressEvent::Skipped` を足せる)、または watcher や admin status が読めなかったものを
  見せる必要が出た時 (その時は `rename_single_file` が捨てる理由も)。
- **test が守る場所**: `grooveseek/src/indexer/skip.rs` (名前、key、分類、台帳)、
  `grooveseek/tests/index_skipped_files.rs` (すべての理由が path 順に載る、完走・中断・
  強制の run で件数が一覧と等しい、hard link、意図した除外は載らない)、
  `grooveseek/tests/index_skipped_unspellable_names.rs` (Windows: 名前とディレクトリを
  1 回ずつ数える)。

## 参考

- 名前のせいで walk が外すファイルを決めた
  [ADR-0023](0023-index-only-names-the-server-can-open.ja.md)、endpoint の拒否を `skipped` に
  数える [ADR-0025](0025-skip-rejected-inputs-and-retry-transient-embedding-failures.ja.md)。
- `Done in` 行は [usage.ja.md](../usage.ja.md)、何をなぜ skip するかは
  [behavior.ja.md](../behavior.ja.md)。
- English version:
  [0032-report-why-each-file-was-not-indexed.md](0032-report-why-each-file-was-not-indexed.md)
