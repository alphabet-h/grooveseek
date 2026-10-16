# 30. 表の行の位置は chunk ごとの NULL 可の列に持ち、追いつきは形式ごとの key で行う

- Status: accepted
- Date: 2026-10-17
- Deciders: プロジェクトオーナー
- Applies to: v1.20.0 (PR #362)

## 背景と課題

v1.18.0 (`.docx`) と v1.19.0 (`.pptx`) から、表の行は chunk の 1 行で、セルはタブで区切る。
抜粋を見せる側 (grooveseek-desktop など) は、どの行が表の行かを本文の文字だけから推測するしか
なく、その推測は 2 方向に外れる: 表の外の段落もタブを持ち得るし、最初か最後のセルが空の行は
タブで始まるか終わり、trim でそのタブが消える。どの行を表の行として書いたかを知っているのは
parser だけで、それは chunk ごとの情報である。

ADR-0028 と ADR-0029 は schema を変えずに、policy key で既存の索引を追いつかせた。`.docx` は
4 本目の key が要り、ADR-0029 の見直し条件「1 つの形式に同じ種類の key が 4 本目に要る時」に
当たる。

## 判断の軸

- chunk の本文を変えない: 再 embedding も全文検索の変化も無く、本文を固定する test はそのまま通る。
- 読む側が「表の行が無い」と「計算していない」を見分けられ、後者では自分の推測に戻れる。
- ADR-0028 / 0029 と同じく、普通の run 1 回で既存の索引が追いつく。
- MCP の応答は変えない。

## 検討した選択肢

1. **本文に印の文字を入れる。** `.docx` / `.pptx` の全 chunk の embedding と全文検索の入力が
   変わり、全部を再 embedding する。却下。
2. **chunk id → 範囲の別 table。** 全ての書き込み・削除の経路で `chunks` と揃え続ける必要がある。
   却下。
3. **`chunks` の NULL 可の列。** 採用: NULL がそのまま「計算していない」を表し、値は行と一緒に
   動く (`upsert_document` は行をまとめて消して書き直す)。ADR-0028 / 0029 が schema を変えなかった
   のは「どの規則で書いたか」を行に持たせないためで、この列は規則の世代ではなく chunk の中身
   (行の位置) なので、その軸に反しない。

追いつきについて:

1. **今、形式ごとの key を一般化する** (ADR-0029 の見直し条件)。既存の key の値と「key を消すと
   pass が開く」を test が固定していて、一般化すればそれを編集することになる。却下。
2. **列の NULL を追いつきの印に使う。** chunk の本文がどの規則で切られたかは表さないので key は
   どのみち残り、1 つの問いに仕組みが 2 つになる。却下。
3. **形式ごとに key を 1 本足し、同じ述語に乗せる。** 採用: 定数、read / write、告知で済む。

## 決定

- `Chunk::table_rows: Option<Vec<TableRowSpan>>`。`TableRowSpan { table, start, end }` は `content`
  の中の byte 範囲 (行末の改行を含まない) と、chunk の中で最初に現れた順の表の番号。`.docx` /
  `.pptx` の parser は `Some` (行が無ければ空の列) を、他の形式と抽出済みテキストの fallback は
  `None` を返す。
- `chunks.table_rows TEXT` にその JSON を持ち、NULL は「計算していない」。`SearchResult::table_rows`
  は本文に収まるかを確かめてから返し、収まらない値は `None` として読む。`SearchHit` は運ばない。
- `docx_table_rows_policy` と `pptx_table_rows_policy` (`row-spans`) を ADR-0029 の述語に足し、run の
  最後の transaction でそれぞれの形式の key の最後に書く。
- 本文が parse と一致し表の行の位置だけ違う chunk は、その列だけを再 embedding せずに書く。判定は
  保存済みの hash を読み、書き込みは `BEGIN IMMEDIATE` の中で hash と本文を読み直してから書くので、
  持っていない本文から計算した位置が行に残ることは無い。読み直しがこの列を書くのは、行の hash が
  parse したバイトの hash のままの時だけで、その間に別の書き手が hash を書き換えた行はそのまま
  残して未決着とし、後の run がもう一度読む。本文が一致する変更ファイルで、その間に行が動いて
  いたものは、その書き込みが hash を置き換えるので列を NULL にする。

## 結果と代償

- v1.20.0 の最初の run は、変わっていない `.docx` / `.pptx` を 1 回読み直して列を書く。再 embedding
  は無く、`updated` にも数えない。
- **daemon は先に再起動する**: 前の版の daemon は、key の記録後に編集されたファイルを NULL の列で
  書く。読む側はその行で推測に戻るだけで誤りにはならないが、次の編集か `--force` まで NULL のまま。
- `SearchResult` に欄が増えたので、struct literal で組む Rust のコードは `table_rows: None` が要る。
- セルの中のタブはタブのままなので、範囲が保証するのは「この範囲は表の行」までで、セルの境界では
  ない。
- **見直す時**: `.docx` に同じ種類の 5 本目の key が要る時、3 つ目の形式に追いつきが要る時、または
  chunk ごとの列が 2 つ目に要る時。
- **test が守る場所**: `grooveseek/src/parser/docx.rs`、`grooveseek/src/parser/pptx.rs`、
  `grooveseek/src/parser/ooxml.rs` (範囲)、`grooveseek/src/parser/mod.rs`
  (`table_rows_fit`、範囲が通る検査)、`grooveseek/src/db.rs` (列、読む時の検査、key)、
  `grooveseek/src/indexer.rs` (列だけの書き込みと読み直し、告知)、
  `grooveseek/tests/index_table_rows_policy.rs` (run)。

## 参考

- 新しい key が乗る述語は [ADR-0029](0029-reread-unchanged-documents-once-per-format.ja.md)、
  元の pass は [ADR-0028](0028-reread-unchanged-docx-once-per-index.ja.md)。
- 告知と daemon の順序は [usage.ja.md](../usage.ja.md)、表の読み方は [behavior.ja.md](../behavior.ja.md)。
- English version: [0030-mark-table-rows-per-chunk.md](0030-mark-table-rows-per-chunk.md)
