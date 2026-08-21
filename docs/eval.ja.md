# `groove eval` — リトリーバル品質評価

> **English**: [eval.md](./eval.md)

## この機能は誰向けか

以下のどちらかをしたい時だけ使うサブコマンド:

- モデルや設定を変えたときに **retrieval の質がどう変わったか** を定量比較したい
- チューニング中に「前より悪化していないか」を**回帰防止**として確認したい

`groove index` + `groove serve` で普通に使う一般ユーザは **触る必要なし**。
`eval` は独立した opt-in サブコマンドで、golden ファイルが無ければ hint 付きエラー
を返すだけで他の挙動には一切影響しない。

## 何をするのか

「想定される正解が分かっている質問」を並べた小さなファイル (*golden queries*)
を用意すると、`groove eval` は MCP の `search` ツールと同じハイブリッド検索を
それぞれのクエリに対して実行し、上位結果が期待通りかを数値化する。2 回目以降は
前回実行との diff を自動表示するため、設定変更の影響が可視化できる。

## クイックスタート

### 1. Golden ファイルを書く

`<kb>/.groove-eval.yml` に配置:

```yaml
queries:
  - id: rrf-basics
    query: "RRF の k パラメータの意味は？"
    expected:
      - path: "docs/ARCHITECTURE.md"
        heading: "Data flow"   # 任意。省略するとファイル一致で OK
      - path: "src/db.rs"      # heading 省略 = ファイル内の任意ヒットで正解

  - query: "チャンクの重複排除はどうしている？"
    expected:
      - path: "src/indexer.rs"
```

### 2. 実行

```bash
groove eval --kb-path ./knowledge-base
```

出力:

```
groove eval — 2026-04-24T14:32:01+09:00
  model: bge-m3    reranker: none    limit: 10    queries: 2

Aggregate
  recall@1   0.500
  recall@5   1.000
  recall@10  1.000
  MRR        0.750
  nDCG@10    0.821
```

2 回目以降は自動で前回との差分が表示される。

## Golden YAML リファレンス

| フィールド | 型 | 必須 | 意味 |
|---|---|---|---|
| `queries` | list | yes | 評価するクエリ一覧 |
| `queries[].query` | string | yes | 検索クエリ文字列 |
| `queries[].expected` | list | yes | 正解ヒット (1 件以上) |
| `queries[].expected[].path` | string | yes | KB 基準の相対パス (例: `docs/foo.md`) |
| `queries[].expected[].heading` | string | no | 指定するとチャンクの heading も一致が必要 (大小文字・前後空白無視) |
| `queries[].id` | string | no | diff の行 key 用の安定 ID (省略時は query 先頭 32 文字) |
| `queries[].tags` | list | no | 将来的な drill-down 集計のため予約 |
| `defaults.limit` | int | no | 予約フィールド。現状は CLI `--limit` を使う |
| `defaults.rerank` | bool | no | 予約フィールド。現状は CLI `--reranker` を使う |

**ヒット判定**: `path` が search 結果と完全一致、かつ (heading 指定があれば)
`trim` + 小文字化した heading が一致したら正解。

## 指標の意味

各クエリには「正解ヒット集合」が定義されている。検索の top-*k* と照合して指標化する。

### recall@k

> 「正解のうち top-*k* に何割が入ったか」

数式: `|expected ∩ top_k| / |expected|`。範囲 0.0–1.0。

= *網羅率*。`recall@10 = 0.8` なら期待していた正解の 80 % が top 10 に入った。
top 内の並び順は関係ない。

### MRR (Mean Reciprocal Rank)

> 「最初に当たった正解の rank の逆数 = "どれだけ早く当たるか"」

クエリごとに `1 / rank_of_first_hit` (無ければ 0) を計算し、全クエリ平均。
1.0 なら「1 位が正解」、0.5 なら「2 位が最初の正解」。

top の 1 件だけが本命で良いユースケースで特に重要。

### nDCG@k

> 「正解が上の方に集中しているか」

上位ほど重みを付けて正解ヒットを加算し、"理想順"の合計で割った正規化スコア
(0.0–1.0、1.0 = 正解が全部 top に固まっている状態)。

recall@k が変わらないが nDCG@k が改善 → *順位が良くなった* というシグナル。
再ランカーや MMR のような「並び替え系」改善の効果測定に効く。

## Diff 出力の読み方

前回実行からの変化が矢印で注記される:

- **↑ 0.056** (緑): `regression_threshold` (既定 0.05) を超える改善
- **↓ 0.056** (赤): `regression_threshold` を超える劣化
- **↑ / ↓ 0.010** (灰): 動いたがノイズ範囲内
- **—**: 変化なし

per-query セクションには **劣化 (↓)** と **ミス (現在の recall@max_k が 0)** の
クエリだけが並ぶ。全量は `--format json` で取得する。

### Golden が変わった場合

実行間に golden ファイルを編集すると fingerprint が変わり、diff は無効化される:

```
⚠️ golden changed since last run, diff disabled
```

今回の数値は出力される。次回以降は新しい golden に対して diff される。

### 実行間に corpus が変わった場合 (v0.15.0+)

各 run は測定対象の index を記録し、ヘッダに出す:

```
  corpus: 646 docs / 11215 chunks
```

比較対象の run と違っていれば、変化の内容を名指しし、下に並ぶ数値に注釈を付ける:

```
  corpus: 646 docs / 11215 chunks
    ⚠️ corpus changed since last run (642 -> 646 documents, 11090 -> 11215 chunks)
       a delta below may reflect that, not retrieval
```

digest が対象にするのは**索引された chunk** であってソースファイルではない。
検索が読んでいるのは chunk なので、ソースが同一のまま取り込まれ方だけが変わった
再構築 (`exclude_headings` の変更など) も、ファイル hash が全件不変でも検知できる。
既存ファイルの書き換えは件数を動かさないので、件数だけを見ると
「変わっていない」と判定してしまう:

```
    ⚠️ corpus changed since last run (same document and chunk counts, different contents)
```

**golden の変更と違い、これは diff を無効化しない。** これは意図的である。
ナレッジベースは普通は増え続けるので、文書が 1 つ増えるたびに比較を止めていては、
`--fail-on-regression` が最も必要な場面で働かなくなる。したがって corpus は
**報告はするが互換性判定には入れない** — run は比較可能なまま保たれ、低下を
「競合が変わったせいかもしれない」と読める状態になる。

はっきり書いておくと、**報告された regression の原因が retrieval ではなく corpus
である可能性がある**。どちらを疑うべきかを教えるのはこの行だけである。
`--format json` には `corpus` と `corpus_changed` (bool) が載る。比較対象が無い
ときは `null` で、`false` (= 変わっていない) と区別される。

この記録が入る前の run は corpus を持たず、「変わった」とは決して報告されない。
最初の 1 回で corpus が書かれ、次の run から通常どおり比較される。

### コーパスが golden セットを引用している場合 (v0.24.0+)

評価対象のナレッジベース**そのものの中**に評価についてのノートを置くと、その
ノートが検索結果になる。golden query の文面を逐語で引用したノートは、その
クエリに対する最強の一致になるので上位を占め、本来の正解を押し下げる。
**golden について書くほど golden が通りにくくなる。**

各 run はこれをコーパス全体から探し、**stderr** に報告する。exit code は動かさない:

```
groove eval: 1 document(s) quote 2 or more golden queries verbatim (golden-queries-quoted).
  engineering/deep-dive/rag/evaluation.md
    torch-compile (not in top_k)
    cross-encoder-reranker (rank 8)
  Either these notes leaked into the corpus, or the queries came from them
  and the documents belong in `expected`. groove eval changes neither.
```

同じ内容は `--format json` の `findings` にも載る。**何も無くても空配列として
必ず出す** — 「検査して 0 件」と「検査していない古い版」を消費側が区別できるようにする:

```json
"findings": [
  {
    "check": "golden-queries-quoted",
    "path": "engineering/deep-dive/rag/evaluation.md",
    "quoted": [
      { "query_id": "torch-compile", "rank_in_top_k": null },
      { "query_id": "cross-encoder-reranker", "rank_in_top_k": 8 }
    ]
  }
]
```

`rank_in_top_k` が `null` なのは、引用しているがその query の `top_k` には
出ていない場合 = コーパスには居るが、まだ枠を奪ってはいない、という意味。

**どちらの原因なのかは報告しない。** 判定できないからである。query を逐語で含む
文書は、テストについて書いたノートか、そうでなければ **query の出典**であり、
後者ならその文書はその query の `expected` に入るべきで、直すのは golden の方に
なる。どちらかを知っているのは golden を書いた人だけで、`eval` はどちらも変更しない。

**なぜ「2 件以上」で、1 件ではないのか。** 1 件の逐語一致は何の証拠にもならない。
golden query の多くは `cross-encoder` / `torch.compile` のような**トピック名**で、
それを解説する文書に出てくるのは当たり前だからである。662 文書 / 26 golden の
健全なコーパスで実測したところ、**1 件でも報告する規則では 8 件が挙がり全部が
偽陽性**だったのに対し、**1 文書に distinct な query が 2 件以上を要求すると
ちょうど 1 件** — 実際に golden の中身を書いていたノートだけが挙がった。
複数の golden query を引用している文書は「テストについてのノート」の形をしており、
1 件しか含まない文書は「そのトピックについての文書」の形をしている。

知っておくとよい帰結が 2 つある:

- 空白正規化後 **12 文字**未満の query は照合対象にしない。短い query は偶然
  多数の文書に含まれる。この値は同じ実測で両側から挟めている: 8 でも 12 でも
  結果は同じで、16 にすると真陽性が消える。
- 照合は**索引されたテキストフィールド 1 つの中**で行い、走査する対象は
  **全文検索が索引している列とちょうど同じ** — chunk の見出し・パンくず・本文の
  3 つである。この一致が選定規則そのもので、探しているのは「正解を押しのけ得る
  テキスト」なのだから、押しのける力を持つ列を過不足なく覆う必要がある。
  見出しが要るのは、Markdown parser が見出し行を本文から取り除き、かつ検索が
  見出しを**本文より重く**索引するため。パンくずが要るのは、その先頭が
  **frontmatter か無ければファイル名**由来の title であり、他の 2 つのどちらにも
  現れないため (索引時に contextual indexing が無効ならパンくずは空なので、
  その場合は走査しても何も増えない)。何も連結しないので継ぎ目をまたぐ引用は
  見つからないが、これは意図的な選択である — 連結すると継ぎ目の両側にある
  無関係な文が 1 つの引用に見えてしまう。

走査は索引済み chunk 本文の 1 パスで、検索と**同じ read スナップショット**の中で
走る。したがって指標を出したのとまったく同じ index について報告する。

## 設定

`groove.toml` の `[eval]` セクション (すべて省略可能):

```toml
[eval]
golden = ".groove-eval.yml"    # 既定: <kb_path>/.groove-eval.yml
history_size = 10              # 既定: 10
k_values = [1, 5, 10]          # 既定: [1, 5, 10]
regression_threshold = 0.05    # 既定: 0.05
```

CLI フラグが config より優先。受理されるフラグ: `--golden`, `--k 1,5,10`,
`--model`, `--reranker`, `--limit`, `--format text|json`, `--no-history`,
`--no-diff`, `--no-color`, `--fail-on-regression`。pipeline 系 (v0.7.0+):
`--mmr <bool>` / `--mmr-lambda <0..1>` / `--mmr-same-doc-penalty <0..1>` /
`--parent-retriever <bool>` — `groove search` と完全に同じ意味。各 knob の
解説は [retrieval-pipeline.ja.md](./retrieval-pipeline.ja.md) 参照。

### `--fail-on-regression` (CI gate)

集計指標 (`recall@k` の各 k / `MRR` / `ndcg@k` の各 k) のうち少なくとも 1 つが
**直前の compatible run** から `regression_threshold` (既定 0.05、`groove.toml`
の `[eval].regression_threshold` で調整) を超えて退化していた場合、exit code 1
で終了する。"compatible" = 直前 run の fingerprint (`model` / `reranker` /
`limit` / `k_values` / golden YAML の content hash / metric 実装 version、
および v0.7.0+ では実効 `[search.mmr]` / `[search.parent_retriever]` 設定、
v0.13.0 以降はさらに既定値と異なる `[search.fusion]`、v0.14.0 以降は
`[contextual].enabled = true` で構築された index の context mode、v0.16.0
以降は FTS クエリのコンパイル version (`fts_query_version`)) が一致
していること。
MMR / parent retriever の on/off を切り替えても、fusion パラメータを
ビルトイン既定値から動かしても fingerprint は変わるので比較対象外となり、
誤検知にはならない (MMR の有無で recall@k を比較するのは意図的に
apples-to-oranges)。`[contextual]` の切り替えも同様に互換性を壊す — それが正しい。この設定は
全 chunk の embedding と FTS テキストを変え `--force` 再 index を要求するので、
model も golden も同じでも前後の run は **別の index** を測っている。記録
されるのは config の意図ではなく **index が持つ mode** (`index_meta.context_mode`)。
context off の run は何も記録しないので、この機能が無かった頃の baseline とも
そのまま比較できる。

**読めない history ファイルは実行を止める。** `eval` が扱う 2 ファイルはどちらも
既定でナレッジベースの中に置かれるので、`.grooveignore` と同じ検査を通す —
symlink / hard link / 通常ファイルでないもの / 上限超過 (golden は 1 MiB、
history は 64 MiB) は**拒否**される。history の場合、その拒否は「空の history」
ではなく**エラー**にしてある。空を返すと、新しい run がそれに積まれて**同じパスに
保存され**、baseline が全部 1 run に置き換わり、`--fail-on-regression` は何とも
比較しないまま通ってしまうため。**読めたうえでパースできなかった**場合は従来どおり
空から始める — そのバイト列に baseline は入っていない。`--no-history` を渡せば
ファイル自体を見ない。

なお **v0.13.0 より前に記録した history は fusion 設定に関わらず非互換**:
metric 実装の修正で `metric_version` が 1 → 2 になっており、fingerprint は
構造体全体で比較されるため。それらの run は比較されず skip されるが、これは
意図した挙動 (古い数値は別の式で計算されている)。
**v0.16.0 より前の history** も同様: クエリから `MATCH` 式を作る規則が変わった
ときに `fts_query_version` が 1 → 2 になっているため
([retrieval-pipeline.ja.md](./retrieval-pipeline.ja.md) 参照)、凍結した baseline
を含めて比較対象から外れる。これも意図的で、両者は FTS5 に別の式を投げている
以上、model も index も golden も同じでも測っているものが違う。
golden YAML を更新した直後の run も同じ理由で比較対象外となる。

履歴は exit より前に書き出されるので、今回の run は次回比較用に保存される。

CI 例:

```yaml
- name: groove eval gate
  run: groove eval --kb-path knowledge-base --fail-on-regression
```

このフラグは「直前 run が無い」「`--no-history` を渡している」「`--no-diff`
を渡している (比較自体抑止)」「fingerprint 不一致」のいずれでも no-op になる。

## トラブルシューティング

| 症状 | 原因 | 対処 |
|---|---|---|
| `no golden file at ...` | golden YAML が無い | `.groove-eval.yml` を作成するか `--golden <path>` を渡す |
| `No index found at ...` | 未 index | `groove index --kb-path <kb>` を先に走らせる |
| per-query の `✗ <id>  recall@N: 0.00` | そのクエリの検索結果が `expected` のどのパスにも一致しなかった (typo / 未 index / 本当に取りこぼした、のいずれか) | パスの綴りを確認し、その文書の **中身** にある語句で検索して hit の `path` を見る (パス文字列で検索しても確認にはならない: FTS が張るのは `heading` / `context` / `content` で、embedding にもパスは入らない)。本当の miss なら設定ミスではなく検索結果として扱う |
| `golden changed since last run, diff disabled` | golden を編集した | 意図通り。次回以降は新 golden で diff される |
| Model mismatch エラー | `--model` が index 作成時と違う | index 時と同じモデル or 再 index |

## 非スコープ (意図的)

- **Graded relevance (0 / 1 / 2)**: 非対応。しかも **黙って無視はしない** — golden の各構造体は `deny_unknown_fields` なので、`relevance:` を書くと評価が始まる前に落ちる:

  ```
  Error: failed to parse golden file: golden.yaml

  Caused by:
      unknown field `relevance`, expected `path` or `heading`
  ```
- **モデルの Sweep / Matrix**: embedding モデルの比較は別々に index した DB を
  2 つ作って 2 回走らせる — 1 回の `eval` が測るのは 1 つの index だけ。
  (**fusion パラメータ**の sweep は 1 つの index に対して可能で、それが次節の
  `groove tune`)
- **必須化**: `eval` は `index` / `serve` / `search` の挙動を 1 バイトも変えない

## `groove tune` — fusion パラメータを測る (v0.13.0+)

`groove eval` は「検索品質がどれくらい良いか」を教えてくれる。`groove tune` は
「2 つの fusion つまみ (`rrf_k` と bm25 列重み) が **そもそもその数値を動かせるのか**」
を教えてくれる。tune は何も適用しない — 出力は貼り付け可能な `[search.fusion]`
スニペットか、「既定値のままにすべき」という結論のどちらかである。

```bash
groove tune --kb-path knowledge-base
groove tune --kb-path knowledge-base --format json > tune.json
groove tune --kb-path knowledge-base --golden ./ci-golden.yml --limit 20
```

golden set は `groove eval` と同じものを読み、探し方のフラグも同じ:
`--golden <PATH>` で `.groove-eval.yml` 以外を使い、`--limit` で 1 クエリあたりの
取得件数を変え、`--no-color` で表の ANSI を落とし、`--model` は測る対象の index に
合わせる。**`eval` と違って `--reranker` は取らない** — 測っているのは
reranking の手前にある fusion 段だから。

### golden セットに求められる条件

v0.16.0 以降、groove はクエリを token 単位の phrase にコンパイルして `OR` で
結合するため ([retrieval-pipeline.ja.md](./retrieval-pipeline.ja.md) 参照)、
**クエリが本文に逐語で出現しなくても bm25 段に到達する** — 断片ごとに単独で
マッチできるので、自然文の golden セットもそもそも測定対象になる。それでも測る
ものが無くなるのは、phrase が 1 つも残らない query か、phrase がどこにもマッチ
しない query の場合で、grid のどの点でも同じ順位が返る。そこで tune は
pre-flight を先に走らせる:

- query ごとの FTS 候補数を数え、**実効 N** (候補 2 件以上の query 数。0 件は
  vector-only にフォールバックし、1 件は rank が固定なので重みに不感) を報告する
- 実効 N が 0 なら診断を stderr に出して grid を実行せず **exit 2** する
- 実効 N が 50 未満なら「IR 慣行の下限未満であり、結果は示唆であって結論ではない」
  と警告する

もう 1 つの警告は、KB を `[contextual]` off で index した場合に出る (上の実効 N
チェックの **後** なので、grid に到達した run に限る): 全 chunk の `context` 列が
空なので、`bm25_context_weight` を 0.5〜4.0 で振ってもスコアは 1 ミリも動かない。
`[contextual]` は既定 off なので大半の run で出るが、これはパラメータではなく
index についての説明:

```
groove tune: WARNING — every chunk has an empty context column, so the
bm25_context_weight axis is a no-op on this KB (contextual retrieval is off).
Its rows below mean "not measured", not "has no effect".
```

測定可能な golden にするには識別力のある語を含むクエリ (固有名詞・API 名・
コマンド名・エラーコード等) を入れること。これらがコンパイルされる phrase は
bm25 が文書を区別できる程度に希少だが、ありふれた断片だけのクエリはどこにでも
マッチするので重みで分けるものが無い。3 文字未満のクエリ (trigram 下限を割り、
FTS に投げるものが残らない) と `heading:foo` のような column filter 構文は避ける
(後者は `:` が Separator なので両側がただの phrase になり、filter として働かない)。

### 推奨がどうガードされるか

小さな golden セットでの argmax はほぼ確実に過学習するので、候補が推奨されるのは
以下を **すべて** 満たすときだけ:

1. refit された条件がビルトイン既定値と異なる
2. held-out の平均 ΔnDCG@5 > 0.02
3. held-out の平均 ΔnDCG@5 > 3 × paired SE (`SD({d_j}) / sqrt(N)`)
4. selection stability > 0.5 — leave-one-query-out の fold の過半数が同じ条件を
   選んだこと (fold 間の不一致は過学習の最も直接的な兆候)
5. 副指標 (各 k の recall@k、MRR) が既定値より悪化していないこと

> **係数が「2 sigma」の 2 ではなく 3 である理由。** `SD({d_j}) / sqrt(N)`
> は fold ごとの差分が独立であることを仮定する。N 個の leave-one-out 選択は互いに
> N−2 個の query を共有するので **fold ごとに違う条件を選び得る**が、共有は相関を
> 可能にするだけで生み出すわけではない。かといって **fold 内の一致だけでも独立には
> ならない**: 一致していても「どの条件で一致したか」自体が共有行から選ばれた確率変数
> なので、全 `d_j` がそれに依存する。切り離せるのは、**選択が golden set の抽出を
> またいで実質固定されている**場合であって、これは別の性質である。
>
> 既知の生成過程に対するシミュレーションでは、選択がばらついた 3 設定 (300 replication
> で 114〜184 種類の条件が選ばれた) で報告される SE は真値の **0.53〜0.60 倍**、
> まったくばらつかなかった 1 設定 (7,800 回の fold 選択がすべて同一条件) では
> **1.03 倍**だった。数えているのは各差分を生む **fold の選択**であって、全 N 行から
> 選ばれる refit ではない。両者は乖離しており (最初の設定で 114 対 64)、refit で
> 数えるとばらつきを過小に見せる。
>
> **その代償は sigma 換算せず棄却率で直接測る** — 報告される SE は run ごとに変動し
> 観測された mean delta と相関し得るので、平均の比から gate の発火確率は決まらない。
> **真の優位差をゼロにした**設定で回すと、係数 2 では **12.7%** 発火し、5 条件すべてを
> 通った「採用」も同じ 12.7% 出ていた — 較正された片側 2 sigma なら約 2.3% のところで
> ある。つまり**見つけるものが何も無い golden set でも、8 回に 1 回ほど採用推奨が
> 出ていた**。
>
> そこで係数そのものを誤採用率に対して掃引した (各設定 2,000 replication):
>
> | 係数 | null での採用 (N=26 / N=12) | 見つかる優位差の検出力 |
> |---|---|---|
> | 2 (旧) | 12.7% / 9.7% | 99.0% |
> | **3 (現行)** | **3.4% / 3.1%** | **95.2%** |
> | 4 | 0.5% / 0.8% | 79.4% |
>
> 3 は誤採用を 3.7 分の 1 にする代償が検出力 3.8 ポイントで済むので、これを採用値と
> した。**条件 2 を厳しくする方は代わりにならない**: 下限を 0.02 → 0.04 にしても null
> は 12.7% → 12.1% しか動かないのに、同じ検出力が 99.0% → 51.9% まで落ちる。
> 条件 4 を通った replication だけで SE 比を取り直すと 0.62〜0.73 に上がり、
> stability gate は差を縮めるが埋めない。300 rep 中 192〜300 が通過するので稀な隅でも
> ない。
>
> この数値には注意が 2 つある。合成データの fixture が nDCG / recall / MRR に同じ値を
> 書くため条件 5 が通りやすく、実際の golden set で副指標のガードがどれだけ効くかは
> これでは分からない。また null での誤採用は誤りの一部でしかない — 真の勝者が存在する
> が騒がしい landscape では、採用のうち相当数が**間違った条件**を選んでおり、N=12 では
> およそ半分がそうだった。
>
> シミュレーションは `tune.rs` の `au16_paired_se_versus_the_true_standard_error` と
> `au68_adoption_rate_across_the_two_thresholds`。

満たさなければ結論は「ビルトイン既定値を維持」であり、これは正常かつ想定内の結果で
ある: RRF 原論文は k ∈ [30, 100] で相対 MAP が約 0.4% しか動かないことを実測して
おり、Elasticsearch は RRF を「チューニング不要」と明記している。

レポートには per-query の内訳 (何件の query が悪化し、どれだけ悪化したか) も出る。
rank fusion は平均の改善の裏に per-query の劣化を隠すことが常だからである。

### 推奨を採用する前の確認

tune は常に **reranker なし** の素の RRF 段を測る。したがって tune が見つけた改善が
本番パイプラインでも残る保証は無い。`adopt` の判定が出たら、スニペットを
`groove.toml` に貼った上で実構成の `eval` を回してから採用を決めること:

```bash
groove eval --kb-path knowledge-base --reranker bge-v2-m3 --no-history
```

`[search.fusion]` を外した状態の同じコマンドと比較する。rerank 後の数値が改善しない
なら変更は破棄する — reranker は上流の順位差を吸収 (あるいは反転) することが多い。
