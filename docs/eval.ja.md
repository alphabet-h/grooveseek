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
v0.13.0 以降はさらに既定値と異なる `[search.fusion]`) が一致していること。
MMR / parent retriever の on/off を切り替えても、fusion パラメータを
ビルトイン既定値から動かしても fingerprint は変わるので比較対象外となり、
誤検知にはならない (MMR の有無で recall@k を比較するのは意図的に
apples-to-oranges)。fusion が既定値のままの実行は、v0.13.0 より前に記録した
baseline とそのまま比較できる。golden YAML を更新した直後の run も同じ理由で
比較対象外となる。

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
| per-query の `expected path not in index` | `expected` の path が index 内に存在しない | 綴り確認 or 再 index |
| `golden changed since last run, diff disabled` | golden を編集した | 意図通り。次回以降は新 golden で diff される |
| Model mismatch エラー | `--model` が index 作成時と違う | index 時と同じモデル or 再 index |

## 非スコープ (意図的)

- **Graded relevance (0 / 1 / 2)**: parse は寛容だが現状は無視
- **Sweep / Matrix**: モデル比較は別 DB を 2 つ作って 2 回走らせる運用
- **必須化**: `eval` は `index` / `serve` / `search` の挙動を 1 バイトも変えない
