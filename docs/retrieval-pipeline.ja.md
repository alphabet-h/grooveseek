# Retrieval パイプライン (RRF → reranker → MMR → parent retriever)

> **English**: [retrieval-pipeline.md](./retrieval-pipeline.md)

`kb-mcp` がクエリ実行時に走らせる完全なパイプラインを解説する。v0.7.0+ で追加された MMR 多様性再ランクと parent retriever 展開のチューニング指針も含む。

## 全景

```
query
  │
  ▼
┌─────────────────────────────────────────────────────────────────┐
│  1. Hybrid 候補生成                                             │
│       vec_chunks MATCH (top-N)  +  fts_chunks MATCH + bm25      │
│       └─→ Reciprocal Rank Fusion (k=60, configurable)           │
└─────────────────────────────────────────────────────────────────┘
  │
  ▼
┌─────────────────────────────────────────────────────────────────┐
│  2. (任意) Cross-encoder reranker                               │
│       Transformer で候補プールを再スコア                        │
│       (BGE-reranker-v2-m3 / jina-v2-ml / bge-base)              │
└─────────────────────────────────────────────────────────────────┘
  │
  ▼
┌─────────────────────────────────────────────────────────────────┐
│  3. (任意, v0.7.0+) MMR 多様性再ランク                          │
│       貪欲: max  λ·rel(c) − (1−λ)·max_sim(c, picked)            │
│             − same_doc_penalty · 1[doc(c) ∈ picked]             │
│       拡大した候補プールから `limit` 個を選択                   │
└─────────────────────────────────────────────────────────────────┘
  │
  ▼
┌─────────────────────────────────────────────────────────────────┐
│  4. (任意, v0.7.0+) Parent retriever 展開                      │
│       各ヒットチャンクについて:                                 │
│         tokens < whole_doc_threshold_tokens → ドキュメント全体  │
│                                              (max_expanded で   │
│                                               cap)              │
│         else                                  → 隣接 sibling    │
│                                              マージ (level 整合)│
│       score / rank / path / match_spans は不変                  │
│       `expanded_from` に展開元の range を載せる                 │
└─────────────────────────────────────────────────────────────────┘
  │
  ▼
match_spans  → top-`limit` SearchHit を
{results, low_confidence, filter_applied} ラッパに格納
```

各任意段は対応する設定が off なら no-op となるため、v0.6.x の設定では v0.6.x と bit-identical な出力を返す。

## Stage 1 — Hybrid 候補生成 (常時 on)

`vec_chunks` (sqlite-vec、L2 距離 — sqlite-vec 既定のメトリック) と `fts_chunks` (FTS5 trigram + bm25、既定では見出しに 2 倍重み) からそれぞれ top-N を取り、Rust 側で Reciprocal Rank Fusion (既定 `k = 60`、RRF の標準定数) でマージする。クライアントに返す `score` は RRF スコア (大きいほど良い) で距離ではない。

RRF の定数と bm25 の 3 つの列重みは、v0.13.0 以降 `kb-mcp.toml` の `[search.fusion]` で設定できる (ビルトイン既定値は `rrf_k = 60.0`、`heading / context / content = 2.0 / 1.0 / 1.0`)。実測の裏付けが無い限り触らないこと — この 2 つのつまみが自分の KB で検索品質をどれだけ (あるいは全く) 動かさないかは `kb-mcp tune` が報告する。詳細は [eval.ja.md](./eval.ja.md) を参照。

`kb-mcp eval` が既定で測定するのはこの段。ここを底上げするとパイプライン全体の floor が上がる。

## Stage 2 — Reranker (任意, v0.1.0+)

`--reranker` (または `kb-mcp.toml` の `[reranker]`) を設定すると、上位 RRF 候補を cross-encoder で再スコアして返す。`score` 列は RRF から reranker raw score に切り替わる。

**MMR が enabled なとき**は reranker に **より大きい候補プール** (`limit × 5`、最小 50) を流して多様性再ランクの操作余地を確保する。MMR off のときは reranker への入力 limit が `limit` (または reranker のみ on の場合は `limit × 5`、これは v0.7.0 以前の reranker overfetch を保つ) になる。Parent retriever は **プールを拡大しない** — 既に選択されたヒットに対する content-only 段なので、`--parent-retriever` 単独 on のとき reranker 負荷は変わらない。

**enable する場面**: 多言語 / 言語跨ぎ クエリ、上位 RRF が文脈は近いが topic 違いのケース、複数の expected doc を持つ クエリ (rank-1 → rank-2 の入れ替えが顕著に良くなる)

## Stage 3 — MMR 多様性再ランク (任意, v0.7.0+)

**MMR が何をするか**: 上位 `limit` を score 順で返すのではなく、1 個ずつ貪欲に選択する。各ステップで以下を最大化する候補を選ぶ:

```
λ · rel(候補) − (1 − λ) · max_similarity(候補, 既選択)
              − same_doc_penalty · 1[doc(候補) ∈ 既選択 docs]
```

- `rel(候補)` は relevance score (RRF または reranker のいずれか stage 2 が出したもの) を **min-max で `[0, 1]` に正規化** したもの。これにより lambda のバランスは score スケール (RRF ≈ 0.01、reranker ≈ [-10, 10]) に依存しない
- `max_similarity(c, picked)` は `c` の embedding と既選択チャンクの embedding 間の cosine 類似度の最大値
- `same_doc_penalty` は `c` が既選択チャンクと同一 document に属するときに追加で減点される項

**チューニングノブ** (すべて `[search.mmr]`):

| ノブ | 既定 | 上げる場面 | 下げる場面 |
|---|---|---|---|
| `enabled` | `false` | 同一 doc の chunk が 3 つ以上返る、上位 k に冗長性が見える | — |
| `lambda` | `0.7` | off-topic な結果が混じると言われたとき (関連度寄り) | 範囲を広く取りたい (top-1 関連度を犠牲にしてでも) とき |
| `same_doc_penalty` | `0.0` | 長い章持ちの 1 doc が top-k を支配する KB | 0 のままで OK (similarity 項が大半の重複削減を担当する) |

**Eval signal**: MMR を on にして `kb-mcp eval` を再走させる。期待される動き:
- `recall@1` 軽く ↓ (MMR は厳密な top-1 を多様性のために手放しうる)
- 1 query に複数 expected doc がある golden set では `recall@5` / `recall@10` が ↑ (多様性項によって異なる doc が top-k に入りやすくなる)
- `nDCG@10` は混合 — golden ファイルが多様性を重視するか集中した関連度を重視するかに依存

**アンチパターン**: MMR enabled + `lambda = 1.0` は MMR off と等価だが少しだけ遅い (類似度キャッシュは動く)。その場合は MMR を off にすべき — kb-mcp はこの footgun を検知すると warn を出す (実効 MMR off だが lambda override が指定されている)

## Stage 4 — Parent retriever (任意, v0.7.0+)

**Parent retriever が何をするか**: ヒットチャンクが小さい (見出し下の 1 行 bullet など) と LLM が周辺コンテキスト不足で上手く回答できないことがある。Parent retriever は以下のように小さなヒットの `content` を書き換える:

- **ドキュメント全体 fallback** — `whole_doc_threshold_tokens` (既定 100) 未満のチャンクには文書全体を返す (`max_expanded_tokens` で cap)
- **隣接 sibling マージ** — それ以外は同じ heading level で前後に隣接するチャンクを `max_expanded_tokens` まで連結する

元のヒットの score / rank / path / `match_spans` は **保持される**。新しい `expanded_from` フィールドが「どの range が merge されたか」を伝える。relevance ランキングは変わらない — parent retriever は表示内容を入れ替えるだけで順序には触れない。

**チューニングノブ** (すべて `[search.parent_retriever]`):

| ノブ | 既定 | 上げる場面 | 下げる場面 |
|---|---|---|---|
| `enabled` | `false` | LLM が断片を引いて follow-up 質問でギャップを埋めようとする | — |
| `whole_doc_threshold_tokens` | `100` | 短いノートを atomic Zettelkasten 形式で index している、ノート全体を context にしたい | 多くは見出しサイズで sibling-merge だけで足りるとき |
| `max_expanded_tokens` | `2000` | 下流 LLM の context 予算が潤沢 (Claude 200K、GPT-4 128K) | 多数の同時 client にレスポンスを返すとき (応答サイズの上限) |

**cap の相互作用**: `max_expanded_tokens` は予測可能性のため embedder の最大シーケンス長以下に保つべき。BGE-M3 は 8192 max なので既定 2000 は十分余裕がある。embedder cap を超えて上げると、index 時には embedder が見ていない量のテキストが返される可能性がある。

**`token_count` が NULL の行**: v0.7.0 以前の index では `chunks.token_count` が NULL。Parent retriever はこれらの行に `len(content) / 4` フォールバックを使う (indexer 側の estimator と整合)。これがないと cap が silent に bypass される (元の codex が見つけたバグ。`tests/search_parent_integration.rs` で固定済み)

**Eval signal**: Parent retriever は recall/MRR/nDCG を**変えない** — これらの metric は `content` を見ない。ユーザに見える content quality だけが変わる。`kb-mcp eval` の数値ではなく、LLM answer 品質を before / after で比較する (手動 or LLM-judge ハーネス)

## 構成 & 順序の根拠

順序は **`RRF → reranker → MMR → parent retriever → match_spans`** で固定:

- **MMR は reranker の後・parent retriever の前**: MMR は得られる最も精確な relevance signal (あれば reranker score) を必要とし、また MMR は **元の** per-chunk content に対して動く必要がある (多様性項が index の chunking を反映する、merge 後の content ではなく)
- **Parent retriever が最後**: content を入れ替えるだけ。これを早く走らせると MMR の similarity 項が **merged** document を比較してしまい、多様性目的が崩れる
- **`match_spans` は parent retriever の後**: span は最終的に返される `content` への byte offset なので、merge 後のテキストに対して計算する必要がある

各段の出力が次段の有効な入力となる単調合成可能 4 段と捉えれば良い。段を off にしてもパイプラインの形は変わらず、aggression が落ちるだけ。

## 推奨構成

**既定 (チューニングなし)**: `[search.mmr].enabled` と `[search.parent_retriever].enabled` の両方を `false` のままに。これは v0.6.x の挙動と完全に一致 — baseline として有用。

**LLM-as-RAG-frontend**: parent retriever を on (`enabled = true`、既定値)。LLM が各ヒットでより豊富な context を得て、follow-up search 呼び出しが減る傾向。

**多様な content の KB**: MMR を on (`enabled = true`、`lambda = 0.7`、`same_doc_penalty = 0.0`)。1 つの document が top-k を flood する場合に推奨。

**両方**: 両方 on。パイプライン順序により MMR は展開前 content (clean な多様性 signal) を見て、ユーザは展開後 content (LLM context が良い) を見ることになる。

## Eval を踏まえたチューニング ワークフロー

1. 両方 off で baseline を取る (`kb-mcp eval`)
2. MMR を on にして再走、recall@k / nDCG@k を比較。あなたの golden set にとって多様性のトレードオフが妥当か判断
3. 独立に parent retriever を on (MMR は off) にして再走。recall/nDCG はほぼ変わらないはず。変わったら bug 報告 — parent retriever は設計上 content-only 段
4. 両方 on にして v0.7.0 のリファレンス eval を実行
5. `<kb>/.kb-mcp-eval-history.json` に記録される `ConfigFingerprint` でこれら 4 種を区別できるので、フラグを倒すだけでいつでも再走できる

具体的な eval-baseline ノートのテンプレは repo 内の `.dev/knowledge/eval-baseline-2026-04-27.md` を参照 (private notes、format は `CLAUDE.local.md` に記載)。
