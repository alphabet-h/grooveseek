# 11. ハイブリッド検索の両脚から語を除外する

- Status: accepted
- Date: 2026-08-26
- Deciders: project owner
- 対象: v1.1.0

## 背景と問題

[ADR-0002](0002-compile-queries-into-per-token-fts-phrases.md) は、ハイブリッド
の全文検索側がクエリを token 単位の `OR` phrase にコンパイルするようにし、
recall を広げた。しかしこれには、呼び出し側が特定の語を除外して絞り込む手段が
残っていない。`path_globs` や `tags_any` / `tags_all` はドキュメントのメタ
データにしか効かず、クエリ構文からは chunk の本文に一切届かない。SQLite FTS5
には `NOT` 演算子があるが、ハイブリッドのベクトル側は語について何も知らない —
全文検索側だけで除外しても、Reciprocal Rank Fusion がベクトル側から同じ
chunk を再び拾い上げてしまい、「除外した」と報告しながら実際には除外できて
いない検索になる。

本決定が答えるのは、「片方しか語を理解しない 2 つの検索器のハイブリッド」で
語の除外をどう両方に届けるか、そして除外だけで探すものが何も残らないクエリを
どう扱うか、である。

## 決定要因

- ハイブリッドの両脚が「どの chunk を除外するか」で一致していなければならない
  — FTS 側だけの除外は融合で静かに取り消される
- 「この chunk が語を含むか」の判定は 1 つの実装が答える。ベクトル側がテキスト
  照合を再実装して判定してはならない
- 除外の無いクエリは v1.1.0 以前と byte 単位で同じに埋め込み・コンパイル・評価
  される — 使わなければコストは 0 でなければならない
- 「探すものが何も無い」に帰着するクエリ (すべての group が除外) は、ベクトル
  側の都合で黙って何かを返すのではなく、はっきり失敗しなければならない

## 検討した選択肢

1. **全文検索側だけで `NOT`。** 負の phrase を FTS5 式に組み込み、ベクトル側は
   触らない
2. **両脚への hard filter。** FTS5 式は `(positives) NOT (negatives)` を持ち、
   ベクトル側は「負の式単独で FTS5 がマッチする chunk id 集合」に入る候補を
   落とす
3. **soft demotion。** 融合後、除外語を含む chunk を落とさず順位だけ下げる

## 決定

採用: **2 — 両脚への hard filter**。判定は両脚が共有する 1 回の FTS5 評価に
委ねる。

選択肢 1 は ADR-0002 が生んだ欠陥を鏡写しに再生産する — 「除外した」と報告する
ハイブリッド検索が、融合が汲み上げてくる先では実際には除外していない。選択肢
3 は呼び出し側に説明できる契約にならない — 「除外の効果を確認したい」に対して
「たいてい消えている」は答えにならない。選択肢 2 のコストは、検索 1 回あたり
負の式の rowid だけの走査 1 回で、ranking も `LIMIT` も無い: 5,000 chunk の
全行にマッチする負の式 (= 除外語が全行にある最悪ケース) に対し 934.5µs
(best of 5) と実測した。同じ検索に伴う ranking 付き FTS query の 3.5855ms に
対する比であり (measured:
`cargo test -p grooveseek --release --lib the_exclusion_id_scan_stays_cheaper_than_the_ranked_fts_query -- --ignored --nocapture`)、
併走する query のコストの 4 分の 1 強にとどまる。

### インタフェースの変更

- whitespace 区切りの group の先頭に付く `-` は除外になり、判定は正側の
  マッチが見るのと同じ FTS row — `heading` / contextual prefix / `content`
  の 3 列 (`schema.rs:109-115`) — に対して行われ、本文だけではない。
  `"-foo"` で v1.1.0 以前どおり先頭ハイフンを逐語検索できる
- `filter_applied.excluded_terms` が、トークン化と trigram 下限を経て実際に
  除外された phrase を、除外があったときだけ echo する — 除外だけでも他に
  filter が無ければ `filter_applied` は空にならない
- `ConfigFingerprint.fts_query_version` が 3 になり、
  `groove eval --fail-on-regression` はこの変更をまたいだ比較をしない
- 除外だけの query は 3 面すべてで拒否される: MCP は `{"error": …}`、CLI は
  stderr + 非ゼロ終了、golden file は読み込み時のエラー

### 帰結

- quote されない `-word` は正の phrase と同じ規則でトークン化されるため、
  正側の recall を広げる「独立 emit」規則が除外側にも同じだけ効く:
  `-再ランキング` は `ランキング` も除外する。`-"..."` が複合語だけを除外する
  逃げ道になる
- trigram 下限は除外にも効く: `-ab` は何も除外しない。3 文字未満の phrase は
  どちらの極性でも検索対象にならないため
- parent retriever がヒットを展開した先に除外語が再登場し得る — 除外の判定は
  検索時点の hit chunk に対して行われ、後から展開が足す content は見ないため
- `query_phrases(positive_text)` は `parse_query(raw).include` と、1 つの
  例外を除いて等しい: `foo -"bar"-baz` では、raw は quote された除外の後の
  逐語 `-baz` を保つが、`positive_text` はそれを 2 つ目の除外として読み直す。
  検索結果への影響は無い (両脚とも raw を使う) が、その `-baz` の highlight は
  失われる。`docs/citations.md` と `docs/retrieval-pipeline.md` は、span が
  positive text 上で計算されることを書き、2 つの phrase 列が常に一致するとは
  書かない
- phrase 上限に達したときの FTS 最悪コストはおよそ倍になる: 負の式が、同じ
  statement の中でもう 1 本の 32-phrase `OR` として評価され、これにベクトル
  側の rowid 走査 1 回が加わる

### 確認方法

- `a_chunk_holding_an_excluded_term_never_reaches_the_fts_leg` と
  `an_excluded_term_drops_the_vector_nearest_chunk_too` が、実際の FTS5
  テーブルに対して両脚を固定する — それぞれ `match_expr` の括弧付けとベクトル
  側の `contains` 判定に対する mutation 検出器である
- `exclusion_is_judged_by_the_trigram_tokenizer_case_and_diacritics_included`
  が、判定が FTS5 自身のものであり Rust 側の 2 つ目のテキスト照合ではない
  ことを固定する
- proptest `positive_text_equals_the_raw_query_when_no_group_is_excluded` が、
  除外の無いクエリは本決定より前と同じに埋め込み・コンパイル・評価される
  ことを固定する
- **型では担保していない**: embedder と reranker と span 計算が実際に raw では
  なく `positive_text()` を受け取っていること。ハイブリッドの両脚は既に除外行を
  落としているので、将来の変更が静かに raw へ戻しても integration test では
  見分けが付かない。`match_spans_never_cover_an_excluded_term` は span について
  もこれを塞がない: このテスト自身が `compute_match_spans(parsed.positive_text(),
  …)` を呼ぶので、固定できるのは「positive text を**渡されたとき**の当該関数の
  振る舞い」であり、呼び出し側が raw へ戻っても緑のままである。3 つの呼び出し点は
  いずれも review 項目であり、guard ではない

## 参考

- feature-55 PR-2、branch `feature/search-exclusion-syntax`
- [ADR-0002](0002-compile-queries-into-per-token-fts-phrases.md) — 本決定が
  拡張するクエリコンパイラ
- `docs/retrieval-pipeline.ja.md` — 結果として得られた機構とそのコストモデル
- `CHANGELOG.md` の v1.1.0 → Added / Changed
- English version: [0011-exclude-a-term-from-both-halves-of-the-search.md](./0011-exclude-a-term-from-both-halves-of-the-search.md)
