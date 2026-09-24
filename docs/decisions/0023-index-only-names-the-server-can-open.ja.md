# 23. サーバが開ける名前だけを索引し、その判定を 1 つの述語に集める

- Status: accepted
- Date: 2026-09-24
- Deciders: プロジェクトオーナー
- Applies to: v1.14.0

## 背景と課題

索引に入る名前と、読み出せる名前を、別々のコードが決めていた。索引の walk
(`grooveseek/src/indexer.rs` の `collect_source_files_under`) と常駐 watcher
(`grooveseek/src/watcher.rs` の `should_process_parts`) は除外規則・拡張子・リンクを
見るだけで、名前は見ない。読み出し側 — `kb://doc/` URI と、`get_document` がディスクに
触る前に走らせる字句検査 — は `grooveseek/src/resources.rs` の `is_safe_relative` に訊き、
これは Windows でコロンと予約デバイス名を拒否していた。

Windows では、KB の canonical なパスが持つ verbatim prefix `\\?\` のおかげで、
Win32 が書いたとおりには開かない名前のファイルが存在できる: `CON.md` や、
`dir.` という名前のディレクトリの配下。walk はそれを索引し、`search` は見つけ、
URI は付かず `get_document` は拒否した (AW-43)。逆向きの穴もあった: Win32 が
書き換える綴り `b.md.` や `dir./x.md` は字句検査を通り、ディスクを見た後で
やっと拒否されていた (AW-42)。`a/b?.md` は "not found" ではなく "unavailable" で
返っていた (AW-40)。

#319 の review は 3 round 続けて、この同じ軸で別の事例を見つけた。本 ADR が答えるのは
**「索引が持ちうる名前」の規則をどこに置けば、次の事例が 4 本目のパッチではなく
1 関数の変更で済むか**。

## 判断の軸

- search hit は、それが持つ名前で開けなければならない —
  [ADR-0004](./0004-resource-reads-are-bounded-by-the-index.ja.md) が resource の read に
  置いた性質を、そもそも何を索引するかにまで広げる。
- 1 つの問いに実装は 1 つ (AGENTS.md): walk・watcher・URI 側・`get_document` が
  違う答えを返せてはならない。
- 今開けるものは失わない。Unix の名前はそのまま。

## 検討した選択肢

1. **索引に入れたまま読めるようにする** — verbatim prefix 経由で開き、URI も付ける。
   棄却: パスを受け取る側すべて (要求文字列で絞る gateway、クライアント、URI parser) が
   「このサーバでは `CON.md` や `b.md.` が名前である」ことを知る必要があり、
   `get_document` の「綴りは 1 つ」の規則に、Win32 自身が書き換える名前の例外が要る。
2. **現状の分担のまま docs に書く**。棄却: 3 連続の指摘を生んだ状態そのもので、
   次の指摘も同じパッチになる。
3. **述語を 1 つにしてどこでもそれに訊き、そういう名前は索引しない**。

## 決定

選択肢 3 を採る。

- `resources::doc_is_addressable` を「索引が持ちうる名前」の唯一の述語にする。
  中身は `is_safe_relative` に、文書名だけが満たすべき条件 — 空でない、`.` の区間も
  空の区間も無い (`./a.md`・`a//b.md`・末尾 `/`) — を足したもので、全 OS 共通。
- `is_safe_relative` は Windows で、コロンと予約デバイス名に加えて、末尾がドットか
  空白の区間と、`< > " | ? *`・制御文字を含むものを拒否する。Unix ではどれも普通の名前。
- 索引の walk は、述語が拒否するディレクトリを prune し、ファイルは拡張子の判定の後で
  skip する。どちらも 1 件 1 行の warning を出し、`groove index` の `Done in` 行に件数を
  出す (MCP の `rebuild_index` の返答には件数は載らない)。walk が名前を判定するのは、
  `kb_path` の綴りのままの KB の配下にあるパスだけ。
- watcher は拡張子の判定の後で、イベントから取り戻した相対パスについて同じ述語に訊く。
  だから KB の別の綴りで届いたパスも判定される。訊くのは行を書きうるところ —
  reindex・rename の新しい側・新しく現れたディレクトリが持ち込んだファイル — で、
  そういう名前への rename は deindex になる。行を消すか移すだけのところ — 削除と、
  rename の旧い側 (その時点で旧い名前のファイルがまた存在していても同じ) — では
  訊かないので、以前の版がそういう名前で入れた行はファイルと一緒に出ていく。
- `get_document` は最初の stat の前にこの述語に訊く。綴りの検査 (2b) に残るのは、
  索引が持ちうる名前だが文書の正規の綴りではないもの — 大文字小文字の揺れ・8.3 短縮名・
  symlink のディレクトリ経由 — だけ。
- 以前の版が入れた行は、次の `groove index` (または `rebuild_index`) が消す (sweep は
  walk が集めなかったものを消す)。それまでは `groove doctor` が Windows で
  `name-not-spellable-on-windows` として、DB だけを見て報告する。

## 結果と代償

- 良い: search hit は必ず `get_document` と `kb://doc/` URI が受け付ける名前を持つ。
  この軸の次の指摘は 1 関数の 1 行になる。
- 良い: Windows で `a?b.md` や `a.md.` は、ディスクを見ずに綴り違いの答えを返す。
- 悪い: Windows で `CON.md` という名前のファイル、`dir./` の配下のファイルは、
  名前を変えるまで検索に出ない。warning と `Done in` の件数がそれを伝える。
- 悪い: Unix で、名前の中にバックスラッシュで挟まれた `..` を持つファイル
  (`a\..\b.md`) — `get_document` は既に拒否していた — も索引から外れる。
  `groove doctor` の検出は Windows だけなので、以前の版がそれについて入れた行は
  報告されない。行は次の full index が消す。
- 中立: この変更の前に作った索引で起動した daemon は、full index が走るまでその行を
  持ち続ける。その間 Windows では `doctor` が終了コード `1` を返す。ファイルが
  無くなれば watcher がそれより早くその行を消す: ファイルを消せば deindex し、
  索引が持ちうる名前へ rename すれば行を付け替える。
- 中立: `grooveseek/src/server.rs` のテスト
  `the_spelling_refusal_names_nothing_for_spellings_the_lexical_check_lets_through`
  は、このプロジェクトではテストを編集しないので、そのままにした。その入力
  (`./HiddenVault/a.md` など) は今は字句検査で拒否されるので、綴りの検査までは届かず、
  doc comment は以前の順序を説明したままになっている。2 つの検査は同じ返答を返すので、
  テストが確かめていること — 拒否がパスを名指ししない — は変わらず成り立つ。
- テストが押さえていること: `grooveseek/src/resources.rs`・`indexer.rs`・`watcher.rs`・
  `doctor.rs`・`server.rs` の unit test が各規則を OS ごとに固定し、
  `grooveseek/tests/index_unspellable_names.rs` が Windows で `CON.md` を含む KB に
  `groove index` を実行する。

## 参考

- [ADR-0004](./0004-resource-reads-are-bounded-by-the-index.ja.md) — resource の read は索引で縛る
- "Naming Files, Paths, and Namespaces":
  <https://learn.microsoft.com/en-us/windows/win32/fileio/naming-a-file>
- English version: [0023-index-only-names-the-server-can-open.md](./0023-index-only-names-the-server-can-open.md)
