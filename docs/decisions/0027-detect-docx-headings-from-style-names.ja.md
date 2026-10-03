# 27. docx の見出しは style の名前と basedOn の連鎖で決める

- Status: accepted
- Date: 2026-10-03
- Deciders: プロジェクトオーナー
- Applies to: v1.15.0 の次の release

## 背景と課題

この記録までは、`.docx` parser は段落が名指す style ID の綴りで見出しを決めていた:
`<w:pStyle w:val="Heading1">` を小文字化し、`heading` で始まり数字で終わることが条件だった。
style ID は書いたアプリケーションが文書ごとに選ぶ識別子で、名前ではない。日本語 UI の Word は
組み込みの見出しを ID `1` .. `6` で書き、`word/styles.xml` で `heading 1` .. `heading 6` と
名付けるので、そう書かれた文書はどの見出しも見出しと認められず、1 chunk で索引されていた。
test の文書は `word/styles.xml` を持たず `Heading1` 形の ID を使っていたので、見えなかった。

問いは、段落が見出しだと何が決めるのか、そして使える style を持たない文書をどう読むのか。

## 判断の軸

- 今正しく分かれている文書 (`Heading1` 形の ID、styles パートの有無を問わず) は同じように分かれる。
- どの UI 言語の Word が書いた文書も見出しで分かれる。
- 規則は 1 つ。同じ段落について食い違いうる規則を 2 つ持たない。
- 壊れた / 大きすぎる styles パートのせいで文書が索引から落ちてはならない。

## 検討した選択肢

1. **何が決めるか。**
   - 名前だけ (Apache Tika、unstructured): `heading N` という名前の style が見出し。却下:
     見出し style を基にした独自 style が見出しにならないが、書いた人にとっては見出しである。
   - **名前と `w:basedOn` の連鎖。派生側の style が outline level を 9 にしていれば見出しを
     打ち消す** (pandoc の規則 + 打ち消し)。採用。
   - 実効 outline level も使う (docling): 名前によらず outline level 0〜8 の style は見出し。
     却下: outline level を持つ本文 style は実在し、docling はその issue (#4106) に答える必要があった。
2. **表と綴りが食い違う時。**
   - どちらかが見出しと言えば見出し。却下: 打ち消しと「名前が見出しでない style」が、ID がたまたま
     `Heading...` と綴られている限り覆る — 規則が 2 つになる。
   - **表が持つ ID は表が決め、持たない ID だけ綴りが決める。** 採用。
3. **壊れた styles パート。**
   - 壊れる前に読めた style を使う。却下: 親を失った style は連鎖がそこで終わり、表全体とも
     綴りとも違う「本文」になり、どの段落がそうなったかが見えない。
   - **パートを丸ごと使わず、全段落を綴りで読み、stderr でそう言う。** 採用。
4. **既に読んだパートと合わせると展開 budget に収まらない styles パート。**
   - `docProps/core.xml` と同じく文書ごと失敗させる。却下: 今索引されている文書が、無くても
     困らないパートのせいで索引から落ちる。
   - **warning 付きでパートを読み飛ばし、綴りに戻る。** 採用。

## 決定

- `word/styles.xml` は固定 path で `docProps/core.xml` の後に、同じ展開 budget の下で
  `ooxml::read_optional_zip_part` を通して読む。読むと文書が budget を超えるパートは
  読み飛ばし (文書を失敗させない)、展開も残りの budget までに抑える。
- 表が持つのは、根要素 `w:styles` の直下にある段落 style (`w:type="paragraph"`、または
  `w:type` 無し)。key は entity を解決した style ID で、バイト列の完全一致で比べる。ID の
  重複は型を問わず最初の定義が勝つ。使える `w:name` を持たない style は ID を名前の代わりにする。
- 表が持つ style ID は、その style から `w:basedOn` を最大 16 style 辿る。最初に `heading N`
  と名付けられた style (綴りと同じ関数で読む: 小文字化、`heading`、trim、1〜255 の数) が
  レベルを与える。ただしそれより手前で最初に outline level を設定している style が 9 なら本文。
  16 style 以内に見出しが無ければ (循環を含む) 本文。
- 表が持たない style ID と、styles パートが無い / 壊れている / 単体で budget を超える /
  合計で読み飛ばされた文書の全段落は、従来どおり綴りで決める。壊れている各状態と budget の
  各場合は、それぞれ stderr に 1 行で名指す。zip 層が開けない・展開できない part は、
  `read_zip_part` と同じく stderr に行を出さずに綴りの規則へ戻る。
- outline level だけでは見出しにしない (style のものも、段落に直接書かれたものも)。
- 索引と `get_document` は同じ関数で parse し、それぞれ自分の budget で読む。新しい設定は無い。
- この記録より前に書かれた索引がどう追いつくかは、別の決定。

## 結果と代償

- **日本語 (および見出し ID を番号にする他の UI 言語) の Word の文書が見出しで分かれる。**
- **英語版 Word の文書の一部は分かれ方が変わる。** 見出し style を基にした独自 style が見出しに
  なる。組み込みの見出し (`heading 1` と名付けられた `Heading1`) は今までどおり分かれる。
- **見出しのテキストは、変わった文書では `get_document` の content から消える**
  (`Heading1` 形の文書では既にそうだった)。全文検索は chunk の見出しで引き続き当たる。
- **見出しを打ち消すように設定された題は本文として読む** (Word の `TOC Heading` がそう)。
- **拾わないもの**: LibreOffice の現地語の style 名、outline level だけで定義された見出し、
  見出しの番号 (テキストに無い)。
- **壊れた styles パートは表を丸ごと失う** ので、`word/styles.xml` が壊れた日本語版 Word の
  文書は 1 chunk のまま。warning がその文書を名指す。
- **`[index].max_decompressed_size` を変えた KB** では、上限に近い `.docx` を索引は style で、
  `get_document` は綴りで読む (あるいはその逆) ことがある — 読み出しは既定の budget のまま
  だから。違いは見出しのテキストが `content` に入るかどうかだけ。
- **見直す時**: 1 chunk で索引された実物の文書に、名前でも `w:basedOn` でも届かない見出し
  (outline level だけ、現地語の名前) があると分かった時。
- **test が守る場所**: `grooveseek/src/parser/docx.rs` (表、連鎖、fallback、budget)、
  `grooveseek/src/parser/ooxml.rs` (省略可能なパートの読み出し)、`grooveseek/src/server.rs`
  (読み出し側)、`grooveseek/tests/index_docx_heading_policy.rs` (binary と stderr)。

## 参考

- ECMA-376 Part 1: §17.7.4.17 `style`、§17.7.4.9 `name`、§17.7.4.3 `basedOn`、
  §17.3.1.20 `outlineLvl`。
- pandoc の docx reader (style 名と `basedOn`)、docling の issue #4106 (本文 style の outline level)。
- 運用者から見える挙動は [behavior.ja.md](../behavior.ja.md)。
- English version:
  [0027-detect-docx-headings-from-style-names.md](0027-detect-docx-headings-from-style-names.md)
