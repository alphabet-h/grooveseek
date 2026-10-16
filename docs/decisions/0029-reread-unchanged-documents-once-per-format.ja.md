# 29. 変わっていない文書は、形式ごとの policy key で一度だけ読み直す

- Status: accepted
- Date: 2026-10-14
- Deciders: プロジェクトオーナー
- Applies to: v1.19.0 (PR #359)

## 背景と課題

[ADR-0028](0028-reread-unchanged-docx-once-per-index.ja.md) は、`.docx` の読み方の変更に
既存の索引が普通の run 1 回で追いつく仕組みを決めた: `index_meta` の policy key が現行値で
ない間、変わっていない `.docx` を読み直し、行ごとに決着させる。この pass は `.docx` 専用で、
key、読み直す行を選ぶ述語 (`rereads_unchanged_docx`)、`awaiting-reparse` の印、最後の
transaction がどれも `"docx"` を名指していた。

v1.19.0 は `.pptx` の読み方を変える: 表の行を 1 行に書き (セルはタブ区切り)、`<a:br>` を改行
として読む。変わっていない `.pptx` は、索引がある限り前の版が切った chunk のまま残る。
ADR-0028 が `.docx` で解いたのと同じ問題で、ADR-0028 の見直し条件「別の形式でも同じ追いつきが
要る変更が来た時」に当たる。本 ADR は、ADR-0028 の「行ごとの列 (選択肢 1) が一般形」という
一文を置き換える。

## 判断の軸

- ADR-0028 の軸はそのまま: 普通の run 1 回で足りる、正しく読めている文書は再 embedding しない、
  追いつきの記録後に旧規則の行が fast path の陰に残らない、途中で止まった run は何も記録しない、
  schema を変えない。
- ある形式の key が古くても、別の形式の行を読み直さない・再 embedding しない・印を付けない。
- `.docx` の key・値・告知と、それを固定している test を変えない。

## 検討した選択肢

1. **文書行ごとに「どの規則で書いたか」の列を持つ** (ADR-0028 の選択肢 1)。正確で一般的だが、
   migration 付きの schema 変更と全書き込み経路が埋める列が要り、`.docx` の test が key の値と
   「key を消すと pass が開く」を固定している。この 2 点で却下。
2. **`.docx` の pass を `.pptx` 用に写す。** `.docx` の経路は変わらないが、同じ問い (「この run は
   この行を読み直すか」) に実装が 2 つになり、AGENTS.md が禁じる形になる。
3. **形式ごとの policy key と、(拡張子, 古いか) の組で回す 1 つの述語。** 採用。

## 決定

- 追いつきが要る形式ごとに `index_meta` の key を持つ: `.docx` は今の 3 本 (ADR-0028)、`.pptx` は
  `pptx_table_break_policy` (`tab-joined-rows-kept-breaks`)。
- 行を読み直すかは 1 つの述語 `rereads_unchanged_as(extension, stale, …)` が決める: その形式の
  pass が走っている、その形式の parser が書いた行である、hash が走査の hash と一致する。
  `rereads_unchanged_docx` はその `.docx` の場合。
- run で決着しなかった行に `awaiting-reparse` を付けるかは 1 つの述語 `stale_row` が決める: pass が
  走っている形式の行であること。
- key は run の最後の transaction で印と一緒に書く: `.docx` の key を今の順に、その後に `.pptx` の
  key。cancel された run とエラーを返した run はどれも書かない。
- `groove index` は走る pass ごとに loop の前に告知し、`.docx` の行を先に出す。file watcher は pass
  を走らせない。
- 後の形式は、自分の key と pass を同じ述語に足す。

## 結果と代償

- `.pptx` の key が古くても `.docx` は読み直さず、その逆もない。v1.18.0 からの索引は `.pptx` を
  1 回読み直し、`.docx` は fast path のまま。
- key の数は追いつきが要った形式の数だけ増える。
- **daemon は先に再起動する** (ADR-0028 と同じ): 前の版のままの daemon は、key の記録後も編集
  された `.pptx` を旧規則で書き続け、それは `--force` でしか戻らない。
- **見直す時**: 1 つの形式に同じ種類の key が 4 本目に要る時 (ADR-0028 の `.docx` の条件を引き継ぐ)、
  または 3 つ目の形式に追いつきが要る時。
  *(2026-10-17, v1.20.0)* `.docx` に 4 本目の key が要り、この条件に当たった。形式ごとの key を
  維持した理由は [ADR-0030](0030-mark-table-rows-per-chunk.ja.md)。
- **test が守る場所**: `grooveseek/src/indexer.rs` (述語と告知)、`grooveseek/src/db.rs` (key)、
  `grooveseek/tests/index_pptx_table_policy.rs` (run、形式をまたぐ場合、書き込みの失敗、callback、
  watcher)。

## 参考

- 一般化した元の pass は [ADR-0028](0028-reread-unchanged-docx-once-per-index.ja.md)。
- 告知と daemon の順序は [usage.ja.md](../usage.ja.md)、表と改行の読み方は [behavior.ja.md](../behavior.ja.md)。
- English version:
  [0029-reread-unchanged-documents-once-per-format.md](0029-reread-unchanged-documents-once-per-format.md)
