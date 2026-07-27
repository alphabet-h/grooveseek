# `kb-mcp eval` — リトリーバル品質評価

> **English**: [eval.md](./eval.md)

## この機能は誰向けか

以下のどちらかをしたい時だけ使うサブコマンド:

- モデルや設定を変えたときに **retrieval の質がどう変わったか** を定量比較したい
- チューニング中に「前より悪化していないか」を**回帰防止**として確認したい

`kb-mcp index` + `kb-mcp serve` で普通に使う一般ユーザは **触る必要なし**。
`eval` は独立した opt-in サブコマンドで、golden ファイルが無ければ hint 付きエラー
を返すだけで他の挙動には一切影響しない。

## 何をするのか

「想定される正解が分かっている質問」を並べた小さなファイル (*golden queries*)
を用意すると、`kb-mcp eval` は MCP の `search` ツールと同じハイブリッド検索を
それぞれのクエリに対して実行し、上位結果が期待通りかを数値化する。2 回目以降は
前回実行との diff を自動表示するため、設定変更の影響が可視化できる。

## クイックスタート

### 1. Golden ファイルを書く

`<kb>/.kb-mcp-eval.yml` に配置:

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
kb-mcp eval --kb-path ./knowledge-base
```

出力:

```
kb-mcp eval — 2026-04-24T14:32:01+09:00
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

## 設定

`kb-mcp.toml` の `[eval]` セクション (すべて省略可能):

```toml
[eval]
golden = ".kb-mcp-eval.yml"    # 既定: <kb_path>/.kb-mcp-eval.yml
history_size = 10              # 既定: 10
k_values = [1, 5, 10]          # 既定: [1, 5, 10]
regression_threshold = 0.05    # 既定: 0.05
```

CLI フラグが config より優先。受理されるフラグ: `--golden`, `--k 1,5,10`,
`--model`, `--reranker`, `--limit`, `--format text|json`, `--no-history`,
`--no-diff`, `--no-color`, `--fail-on-regression`。pipeline 系 (v0.7.0+):
`--mmr <bool>` / `--mmr-lambda <0..1>` / `--mmr-same-doc-penalty <0..1>` /
`--parent-retriever <bool>` — `kb-mcp search` と完全に同じ意味。各 knob の
解説は [retrieval-pipeline.ja.md](./retrieval-pipeline.ja.md) 参照。

### `--fail-on-regression` (CI gate)

集計指標 (`recall@k` の各 k / `MRR` / `ndcg@k` の各 k) のうち少なくとも 1 つが
**直前の compatible run** から `regression_threshold` (既定 0.05、`kb-mcp.toml`
の `[eval].regression_threshold` で調整) を超えて退化していた場合、exit code 1
で終了する。"compatible" = 直前 run の fingerprint (`model` / `reranker` /
`limit` / `k_values` / golden YAML の content hash / metric 実装 version、
および v0.7.0+ では実効 `[search.mmr]` / `[search.parent_retriever]` 設定、
v0.13.0 以降はさらに既定値と異なる `[search.fusion]`、v0.14.0 以降は
`[contextual].enabled = true` で構築された index の context mode) が一致
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

なお **v0.13.0 より前に記録した history は fusion 設定に関わらず非互換**:
metric 実装の修正で `metric_version` が 1 → 2 になっており、fingerprint は
構造体全体で比較されるため。それらの run は比較されず skip されるが、これは
意図した挙動 (古い数値は別の式で計算されている)。golden YAML を更新した直後の
run も同じ理由で比較対象外となる。

履歴は exit より前に書き出されるので、今回の run は次回比較用に保存される。

CI 例:

```yaml
- name: kb-mcp eval gate
  run: kb-mcp eval --kb-path knowledge-base --fail-on-regression
```

このフラグは「直前 run が無い」「`--no-history` を渡している」「`--no-diff`
を渡している (比較自体抑止)」「fingerprint 不一致」のいずれでも no-op になる。

## トラブルシューティング

| 症状 | 原因 | 対処 |
|---|---|---|
| `no golden file at ...` | golden YAML が無い | `.kb-mcp-eval.yml` を作成するか `--golden <path>` を渡す |
| `No index found at ...` | 未 index | `kb-mcp index --kb-path <kb>` を先に走らせる |
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
  `kb-mcp tune`)
- **必須化**: `eval` は `index` / `serve` / `search` の挙動を 1 バイトも変えない

## `kb-mcp tune` — fusion パラメータを測る (v0.13.0+)

`kb-mcp eval` は「検索品質がどれくらい良いか」を教えてくれる。`kb-mcp tune` は
「2 つの fusion つまみ (`rrf_k` と bm25 列重み) が **そもそもその数値を動かせるのか**」
を教えてくれる。tune は何も適用しない — 出力は貼り付け可能な `[search.fusion]`
スニペットか、「既定値のままにすべき」という結論のどちらかである。

```bash
kb-mcp tune --kb-path knowledge-base
kb-mcp tune --kb-path knowledge-base --format json > tune.json
```

### golden セットに求められる条件

kb-mcp の FTS はクエリ全体を 1 つのクォート済みフレーズとして trigram
tokenizer に投げるため、**クエリが本文に逐語で出現するときにしか bm25 段に
到達しない**。自然文の質問だけで構成された golden セットでは全 query の FTS
候補が 0 件になり、grid のどの点でも同じ順位が返るので測るものが無い。
そこで tune は pre-flight を先に走らせる:

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
kb-mcp tune: WARNING — every chunk has an empty context column, so the
bm25_context_weight axis is a no-op on this KB (contextual retrieval is off).
Its rows below mean "not measured", not "has no effect".
```

測定可能な golden にするには逐語クエリ (固有名詞・API 名・コマンド名・エラーコード等)
を含めること。3 文字未満のクエリ (trigram 下限) と `heading:foo` のような column
filter 構文は避ける (後者はクエリのサニタイズで無効化されるため filter として働かない)。

### 推奨がどうガードされるか

小さな golden セットでの argmax はほぼ確実に過学習するので、候補が推奨されるのは
以下を **すべて** 満たすときだけ:

1. refit された条件がビルトイン既定値と異なる
2. held-out の平均 ΔnDCG@5 > 0.02
3. held-out の平均 ΔnDCG@5 > 2 × paired SE (`SD({d_j}) / sqrt(N)`)
4. selection stability > 0.5 — leave-one-query-out の fold の過半数が同じ条件を
   選んだこと (fold 間の不一致は過学習の最も直接的な兆候)
5. 副指標 (各 k の recall@k、MRR) が既定値より悪化していないこと

> **条件 3 は「2 sigma」という語感より大幅に緩くなり得る。** `SD({d_j}) / sqrt(N)`
> は fold ごとの差分が独立であることを仮定する。N 個の leave-one-out 選択は互いに
> N−2 個の query を共有するので **fold ごとに違う条件を選び得る**が、共有は相関を
> 可能にするだけで生み出すわけではない — 全 fold が同じ条件を選べば各 `d_j` は自分の
> held-out 行だけに依存し、仮定は成り立つ。
>
> 既知の生成過程に対するシミュレーションでは、fold の選択が割れた 3 設定で報告される
> SE は真値の **0.53〜0.60 倍**、割れなかった 1 設定では **1.03 倍**だった。
>
> **その代償は sigma 換算せず棄却率で直接測る** — 報告される SE は run ごとに変動し
> 観測された mean delta と相関し得るので、平均の比から gate の発火確率は決まらない。
> **真の優位差をゼロにした**設定での実測:
>
> | | null での発火率 |
> |---|---|
> | `平均 ΔnDCG@5 > 2 × paired SE` | **12.7%** |
> | 5 条件すべてを通って「採用」 | **12.7%** |
> | 較正された片側 2 sigma なら | 約 2.3% |
>
> つまり**見つけるものが何も無い golden set でも、8 回に 1 回ほど採用推奨が出る**。
> 条件 4 を通った replication だけで SE 比を取り直すと 0.62〜0.73 に上がり、
> stability gate は差を縮めるが埋めない。300 rep 中 192〜300 が通過するので稀な隅でも
> ない。
>
> このシミュレーションでは 5 条件全体の採用率が SE gate の発火率とほぼ一致しており、
> 他の条件がほとんど追加で効いていない。ただしこれは合成データの性質でもある
> (fixture が nDCG / recall / MRR に同じ値を書くため条件 5 が通りやすい) ので、実際の
> golden set で他の条件がどれだけ効くかを示すものではない。
>
> 係数は**意図的に上げていない** — 上げるとツールの推奨内容そのものが変わるため。
> シミュレーションは `tune.rs` の `au16_paired_se_versus_the_true_standard_error`。

満たさなければ結論は「ビルトイン既定値を維持」であり、これは正常かつ想定内の結果で
ある: RRF 原論文は k ∈ [30, 100] で相対 MAP が約 0.4% しか動かないことを実測して
おり、Elasticsearch は RRF を「チューニング不要」と明記している。

レポートには per-query の内訳 (何件の query が悪化し、どれだけ悪化したか) も出る。
rank fusion は平均の改善の裏に per-query の劣化を隠すことが常だからである。

### 推奨を採用する前の確認

tune は常に **reranker なし** の素の RRF 段を測る。したがって tune が見つけた改善が
本番パイプラインでも残る保証は無い。`adopt` の判定が出たら、スニペットを
`kb-mcp.toml` に貼った上で実構成の `eval` を回してから採用を決めること:

```bash
kb-mcp eval --kb-path knowledge-base --reranker bge-v2-m3 --no-history
```

`[search.fusion]` を外した状態の同じコマンドと比較する。rerank 後の数値が改善しない
なら変更は破棄する — reranker は上流の順位差を吸収 (あるいは反転) することが多い。
