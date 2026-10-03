# 28. 既存索引の docx は一度だけ読み直し、行ごとに決着させる

- Status: accepted
- Date: 2026-10-03
- Deciders: プロジェクトオーナー
- Applies to: v1.15.0 の次の release

## 背景と課題

[ADR-0027](0027-detect-docx-headings-from-style-names.ja.md) は `.docx` の分け方を変えたが、
索引は parser を走らせる前に、content hash が一致するファイルを「変わっていない」と答える。
だから誰も編集しない `.docx` は、索引がある限り前の規則が切った chunk のまま残る。`--force` で
直るが、desktop アプリは `--force` を渡さず、その利用者は stderr を見ない。

問いは、既存の索引がどうやって 1 回で追いつくか — 変わっていない `.docx` を、最初の普通の
run で — 正しく分かれている文書を再 embedding せず、読めない文書 1 本のせいで他の文書の追いつきが
いつまでも終わらない、ということも無しに。

## 判断の軸

- 普通の run (`groove index`、MCP `rebuild_index`、desktop の呼び出し) 1 回で足りる。`--force` を
  渡す必要が無い。
- 既に正しく分かれている文書は再 embedding しない。
- 追いつきが済んだと記録された後に、旧規則が書いた行が fast path の陰に残らない。
- cancel やエラーで途中で止まった run は何も記録しない。
- `.groove.db` の schema は変えない。

## 検討した選択肢

1. **追いつきはいつ済むか。**
   - #251 の frontmatter の点検と同じく、どの `.docx` も読めた時。却下: 読めない `.docx`
     が 1 本 (サイズ上限超過、parse 失敗) あると済まず、毎 run どの `.docx` も読み直す。
     desktop には抜け出すための `--force` も stderr も無い。
   - 文書行ごとに「どの規則で分けたか」の列を持つ。正確だが、migration 付きの schema 変更、
     変わっていない仕事を飛ばす経路も含めて全書き込み経路が埋める列、それ自体の記録が要る。
   - **run の中で行ごとに決着させる: 書き直す / 読み直して一致を確かめる / 消す / どのファイルの
     hash とも一致しない content hash を書いて fast path に乗せない。** 採用。schema 変更は要らず、
     コストは、まだ読めない `.docx` 1 本につき run ごとの読み込み 1 回。
2. **印。** `index_meta.docx_heading_policy` という独自の key を、#251 の `frontmatter_policy` と
   共有せずに隣に置く: 2 つの pass は対象のファイルも仕事も済む条件も違い、共有すると片方の未完が
   もう片方を止める。

## 決定

- `docx_heading_policy` が `styles-name-basedon` でない間、`rebuild_index` (`--force` でない) は、
  行の hash が走査の hash と一致する `.docx` を読み直す (`Reindex::Reparse`): chunk が
  一致すればそのままで再 embedding しない、違えば普通の経路で書く、chunk が 1 つも無ければ行を
  消して skip に数える (`--force` なら行が残らないのと同じ)。内容の変わった `.docx` は普通の経路、
  既に強制されている rename は強制のまま。
- 削除の掃き出しの後、1 つの transaction で、この run で決着しなかった `.docx` の行 — 走査で
  skip された、parse に失敗した、endpoint に拒まれた、読む間に bytes が変わった — の content hash
  を `awaiting-reparse` にし、key を書く。印はその時点で行が何を持っていても書く。cancel された
  run とエラーを返した run はどちらも書かない。
- `groove index` は loop の前に読み直す文書の数を知らせる (`--quiet` では出さない)。MCP
  `rebuild_index` と callback の reporter には新しい出力も event も無い。file watcher は pass を
  走らせない。

## 結果と代償

- **追いつきは 1 回で済む。** 以降の run は fast path に戻る。例外は印を付けた行で、読めるように
  なるまで、変更されたファイルと同じく毎 run 読まれ、`Skipping ...` 行が繰り返し出る。
- **印を付けた文書を移動すると**、rename ではなく新規 + 削除になる (rename の検出は hash の一致で
  見るため)。
- **評価の corpus digest は 1 回変わる** (印を書いた run)。digest が content hash を含むため。
- **daemon は先に再起動する。** `groove serve` は起動時に索引せず、watcher は変わったファイルしか
  読まないので、新しい版の daemon は `rebuild_index` を 1 回待つ。前の版のままの daemon は、key の
  記録後も編集された `.docx` を旧規則で書き続け、それは `--force` でしか戻らない。downgrade も同じ。
- **見直す時**: 別の形式でも同じ追いつきが要る変更が来た時。行ごとの列 (選択肢 1) が一般形。
- **test が守る場所**: `grooveseek/src/indexer.rs` (読み直し、述語、印)、`grooveseek/src/db.rs`
  (key と上書き)、`grooveseek/src/indexer/progress.rs` (告知行)、
  `grooveseek/tests/index_docx_heading_policy.rs` (run、cancel、callback、watcher)。

## 参考

- 追いつく先の規則は [ADR-0027](0027-detect-docx-headings-from-style-names.ja.md)。
- run の集計と daemon の順序は [usage.ja.md](../usage.ja.md)、rename は [behavior.ja.md](../behavior.ja.md)。
- English version:
  [0028-reread-unchanged-docx-once-per-index.md](0028-reread-unchanged-docx-once-per-index.md)
