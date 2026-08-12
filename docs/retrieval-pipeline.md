# Retrieval pipeline (RRF → reranker → MMR → parent retriever)

> **日本語版**: [retrieval-pipeline.ja.md](./retrieval-pipeline.ja.md)

This document narrates the full pipeline that `kb-mcp` runs at query time, with tuning advice for the v0.7.0+ stages (MMR diversity re-rank, parent retriever content expansion).

## At a glance

```
query
  │
  ▼
┌─────────────────────────────────────────────────────────────────┐
│  1. Hybrid candidate generation                                 │
│       vec_chunks MATCH (top-N)  +  fts_chunks MATCH + bm25      │
│       └─→ Reciprocal Rank Fusion (k=60, configurable)           │
└─────────────────────────────────────────────────────────────────┘
  │
  ▼
┌─────────────────────────────────────────────────────────────────┐
│  2. (optional) Cross-encoder reranker                           │
│       Re-score the candidate pool with a transformer            │
│       (BGE-reranker-v2-m3 / jina-v2-ml / bge-base)              │
└─────────────────────────────────────────────────────────────────┘
  │
  ▼
┌─────────────────────────────────────────────────────────────────┐
│  3. (optional, v0.7.0+) MMR diversity re-rank                   │
│       Greedy: max  λ·rel(c) − (1−λ)·max_sim(c, picked)          │
│             − same_doc_penalty · 1[doc(c) ∈ picked]             │
│       Picks `limit` chunks from the larger candidate pool.      │
└─────────────────────────────────────────────────────────────────┘
  │
  ▼
┌─────────────────────────────────────────────────────────────────┐
│  4. (optional, v0.7.0+) Parent retriever content expansion     │
│       For each hit chunk:                                       │
│         tokens < whole_doc_threshold_tokens → whole document    │
│                                              (capped at         │
│                                               max_expanded)     │
│         else                                  → adjacent merge  │
│                                              (level-aware)      │
│       Score / rank / path / match_spans untouched.              │
│       `expanded_from` carries the source range.                 │
└─────────────────────────────────────────────────────────────────┘
  │
  ▼
match_spans  → top-`limit` SearchHit, wrapped in
{results, low_confidence, filter_applied}
```

Every optional stage is a no-op when its config is off, so a v0.6.x configuration produces v0.6.x output bit-for-bit.

## Stage 1 — Hybrid candidate generation (always on)

`vec_chunks` (sqlite-vec, L2 distance — its default metric) and `fts_chunks` (FTS5 trigram over three columns since v0.12.0 — `heading`, `context`, `content` — scored with bm25 at a 2× heading weight by default) each return their own top-N. Reciprocal Rank Fusion combines them on the Rust side with `k = 60` by default (the standard RRF constant). The score returned to clients is the RRF score (higher = better), not a distance.

**How the query reaches FTS5** (v0.16.0+): the query string is not sent as-is. `build_fts_query` compiles it into a set of quoted phrases joined with ` OR `:

- A region you wrap in `"..."` is kept as a **verbatim phrase**, under FTS5's own doubled-quote convention (`""` inside a phrase is a literal `"`). A quoted phrase holding fewer than 3 characters is dropped.
- Outside quotes, the query is cut into groups at separators (whitespace, punctuation, symbols), and each group is cut further at **script boundaries** — kanji / hiragana / katakana / other word characters. `再ランキングの評価について` yields the runs `再` / `ランキング` / `の` / `評価` / `について`.
- A run shorter than 3 characters (the trigram floor, below which a phrase matches nothing at all) is joined to its neighbours **within the same group** until it reaches 3. When that join extends a span that already stood on its own, the pre-extension span is emitted as a phrase too: `再ランキング` also emits `ランキング`, and `システム化` also emits `システム`.
- A group that never reaches 3 characters has nothing to join to — a join never crosses a separator — so it is **dropped** and never enters the expression: `AI について` searches only for `"について"`, and `ML pipelines` only for `"pipelines"`. Quoting the short word on its own does not rescue it, since a quoted phrase under 3 characters is dropped by the same floor; quoting a region wide enough to clear the floor does, as in `"AI について"`, at the cost of searching that region verbatim. The floor itself is unavoidable — under a trigram tokenizer a phrase shorter than 3 characters matches nothing at all.
- The phrases are deduplicated, capped at 32, and joined with ` OR `.

So `再ランキングの評価について` becomes `"再ランキング" OR "ランキング" OR "の評価" OR "について"`, and `"Foundry Local" の設定` becomes `"Foundry Local" OR "の設定"`. Before v0.16.0 the whole query was wrapped in one phrase, which over a trigram tokenizer is a verbatim substring search — a natural-language Japanese query produced zero FTS candidates, so the FTS half of the hybrid was effectively dead and only the vector half ran. Why script boundaries rather than a morphological analyser, and what the change cost in retrieval quality, is in [ADR-0002](./decisions/0002-compile-queries-into-per-token-fts-phrases.md).

If tokenizing produces no phrase at all — every fragment is under the floor, as in `AI と ML` — the whole trimmed query falls back to that single-phrase form, so this shape of query does not regress. Only a query that is itself under 3 characters after trimming skips FTS entirely and searches vector-only. Quoting the entire query reproduces the old verbatim behaviour on demand. This is a query-side change only: the index, the schema, and the tokenizer are untouched, so **no re-index is needed**.

**What the FTS half costs.** `ORDER BY bm25(...)` scores *every* matching row before `LIMIT` is applied, so the cost of the full-text half tracks the number of rows the expression matches — not the number you asked for. Measured on this pipeline (release build, synthetic corpora where every phrase matches every row, i.e. the worst case): a single-phrase query costs 4.3 ms at 5,000 rows, 16.0 ms at 20,000 and 32.8 ms at 40,000, while the 32-phrase `OR` costs 46.9 ms / 171 ms / 329 ms. Both are linear in the matching population, and the multiple between them (**~10×**) is flat across corpus sizes. Cost also grows roughly linearly with the number of phrases: at 20,000 rows, 1 / 2 / 4 / 8 / 16 / 32 phrases cost 17.6 / 22.9 / 34.4 / 44.0 / 81.5 / 172 ms.

Three consequences worth knowing before tuning anything. First, **lowering a limit does not reduce this cost**: at 40,000 rows the same query takes 339 ms at `LIMIT 1` and 329 ms at `LIMIT 100`; only a limit large enough to *return* thousands of rows adds anything (+42 ms at 10,000), and that is materialisation, not matching. Second, matching every row in the index has always been one common substring away — `"について"` alone does it with a single phrase — so per-token compilation did not raise the ceiling on *how many rows* a query can touch, though it did raise the ceiling on cost by roughly 10×. Third, the knob that would actually bound the worst case is the phrase cap (32), because cost is near-linear in phrase count — but it is deliberately left alone. Across 37 golden queries the largest produced 9 phrases, so the cap never binds on real queries; halving it would halve the worst case and equally halve the length at which a genuine query starts losing its trailing phrases. Since a query over the cap still succeeds and only returns less, that loss is silent, which is the failure mode this project would rather not add. Truncation is logged at `warn` instead, and a test pins that realistic queries keep at least 2× headroom.

A regression guard pins the multiple rather than any absolute timing (`bu03_or_expansion_stays_within_a_small_multiple_of_a_single_phrase`), so it stays meaningful across machines and SQLite versions.

Both the RRF constant and the three bm25 column weights are configurable via `[search.fusion]` in `kb-mcp.toml` (v0.13.0+); the built-in defaults are `rrf_k = 60.0` and `heading / context / content = 2.0 / 1.0 / 1.0`. They are deliberately left alone unless you have measured otherwise — see [eval.md](./eval.md) for `kb-mcp tune`, which reports how much (if at all) these knobs move retrieval quality on *your* KB.

This stage is what `kb-mcp eval` measures by default: any improvement here lifts the floor for the entire pipeline.

## Stage 2 — Reranker (optional, v0.1.0+)

When `--reranker` is set (or `[reranker]` in `kb-mcp.toml`), the top RRF candidates are re-scored by a cross-encoder before being returned. The score column switches from RRF to the reranker raw score.

When **MMR is enabled**, kb-mcp pulls a *larger candidate pool* (`limit × 5`, min 50) through the reranker so that diversity re-rank has room to operate. When MMR is off, the reranker input limit matches `limit` (or `limit × 5` when only reranking, preserving pre-v0.7.0 reranker overfetch behavior). Parent retriever does **not** enlarge the pool — it is a content-only stage that runs on the already-selected hits, so reranker workload is unchanged when only `--parent-retriever` is set.

**When to enable**: cross-language queries, queries where the top RRF hit is contextually close but topically wrong, or queries with multiple expected docs (the reranker re-orders rank-1 → rank-2 transitions noticeably).

## Stage 3 — MMR diversity re-rank (optional, v0.7.0+)

**What MMR does**: instead of returning the top `limit` candidates by score, MMR picks them one at a time, at each step choosing the candidate that maximizes:

```
λ · rel(candidate) − (1 − λ) · max_similarity(candidate, already_picked)
                  − same_doc_penalty · 1[doc(candidate) ∈ already_picked_docs]
```

- `rel(candidate)` is the relevance score (RRF or reranker output, whichever stage 2 produced) **min-max normalized to `[0, 1]`** so the lambda balance is invariant to score scale (RRF ≈ 0.01, reranker ≈ [-10, 10]).
- `max_similarity(c, picked)` is the cosine similarity between `c`'s embedding and the most similar already-picked chunk's embedding.
- `same_doc_penalty` is an extra subtracted term when `c` lives in the same document as any already-picked chunk.

**Tuning knobs** (all in `[search.mmr]`):

| Knob | Default | When to raise | When to lower |
|---|---|---|---|
| `enabled` | `false` | Searches that often return 3+ chunks of one doc, or visibly redundant top-k | — |
| `lambda` | `0.7` | When users complain about off-topic results (lean toward relevance) | When users want broader coverage at cost of top-1 relevance |
| `same_doc_penalty` | `0.0` | When the corpus has long single-doc chapters and one doc dominates top-k | Keep at 0 unless you have a concrete dedup goal — the similarity term already does most of the work |

**Eval signal**: turn MMR on and re-run `kb-mcp eval`. Expect:
- `recall@1` slight ↓ (MMR can drop the strict-top-1 expectation in favor of diversity)
- `recall@5` / `recall@10` typically ↑ on golden sets with multiple expected docs per query (the diversity term lets more distinct docs into top-k)
- `nDCG@10` mixed — depends on how the golden file weights diversity vs. concentrated relevance

**Anti-pattern**: setting `lambda = 1.0` with MMR enabled is equivalent to MMR off but slightly slower (the similarity cache still runs). Just turn MMR off in that case — kb-mcp emits a warn when it detects this footgun (effective MMR off but lambda override provided).

## Stage 4 — Parent retriever (optional, v0.7.0+)

**What parent retriever does**: when a hit chunk is small (e.g. a single-line bullet under a heading), the LLM may not have enough surrounding context to answer well. Parent retriever rewrites the `content` field of small hits so that:

- **Whole-document fallback** for chunks below `whole_doc_threshold_tokens` (default 100): the entire document is returned, capped at `max_expanded_tokens`.
- **Adjacent-sibling merge** for everything else: chunks immediately before and after the hit at the same heading level are merged into the hit's content, until the merged block hits `max_expanded_tokens`.

The score, rank, path, and `match_spans` of the original hit are **preserved**. The new `expanded_from` field tells consumers what range was merged in. Relevance ranking is unchanged — parent retriever only swaps the displayed content, not the order.

**Tuning knobs** (all in `[search.parent_retriever]`):

| Knob | Default | When to raise | When to lower |
|---|---|---|---|
| `enabled` | `false` | LLM responses cite small fragments and ask follow-up questions to fill gaps | — |
| `whole_doc_threshold_tokens` | `100` | When you index very short notes (atomic Zettelkasten style) and want full-note context | When chunks are mostly heading-sized and you only want sibling-merge behavior |
| `max_expanded_tokens` | `2000` | If your downstream LLM has a generous context budget (Claude 200K, GPT-4 128K) | If you serve many simultaneous clients and want to bound response size |

**Cap interaction**: `max_expanded_tokens` should be ≤ the embedder's max sequence length for predictability. BGE-M3's max is 8192, so the default 2000 leaves headroom. If you raise it past the embedder cap you risk returning more text than the embedder ever saw at index time.

**NULL `token_count` rows**: pre-v0.7.0 indexes have NULL in `chunks.token_count`. Parent retriever falls back to `len(content) / 4` for these rows (matches the indexer's own estimator), so the cap is enforced even on legacy databases. Without this fallback the cap could be silently bypassed (the original codex-found bug is locked in by `tests/search_parent_integration.rs`).

**Eval signal**: parent retriever does **not** change recall/MRR/nDCG — those metrics ignore `content`. It only changes the user-visible content quality. Compare LLM answer quality (manually or with an LLM-judge harness) before vs after rather than relying on `kb-mcp eval` numbers.

## Composition & order rationale

The order is fixed at **`RRF → reranker → MMR → parent retriever → match_spans`**:

- **MMR after reranker, before parent retriever**: MMR needs the most accurate relevance signal it can get (the reranker score, when present), and it operates on the *original* per-chunk content (so the diversity term reflects the index's chunking, not the post-merge content).
- **Parent retriever last**: it only swaps content — running it earlier would cause MMR's similarity term to compare *merged* documents, which collapses the diversity goal.
- **`match_spans` after parent retriever**: the spans are byte offsets into the final returned `content`, so they have to be computed against the post-merge text.

You can think of the pipeline as four monotone composable stages where each stage's output is a valid input to the next; turning a stage off only changes how aggressive the pipeline is, not its shape.

## Recommended configurations

**Default (no tuning)**: leave both `[search.mmr].enabled` and `[search.parent_retriever].enabled` at `false`. This is exactly v0.6.x behavior — useful as a baseline.

**LLM-as-RAG-frontend**: turn parent retriever on (`enabled = true`, defaults). The LLM gets richer context per hit and tends to need fewer follow-up search calls.

**Diverse-content KBs**: turn MMR on (`enabled = true`, `lambda = 0.7`, `same_doc_penalty = 0.0`). Recommended when one document tends to flood top-k.

**Both**: turn both on. The pipeline order means MMR sees pre-expansion content (cleaner diversity signal) while the user sees post-expansion content (better LLM context).

## Eval-aware tuning workflow

1. Take a baseline with both off (`kb-mcp eval`).
2. Turn MMR on, re-run; compare recall@k / nDCG@k. Decide whether the diversity tradeoff is worth it for your golden set.
3. Independently, turn parent retriever on (with MMR off), re-run; recall/nDCG should be ~unchanged. If they aren't, file a bug — parent retriever is a content-only stage by design.
4. Turn both on, run a final eval as the v0.7.0 reference.
5. The `ConfigFingerprint` recorded in `<kb>/.kb-mcp-eval-history.json` distinguishes these runs so you can re-run any of them by flipping the flags.

For a concrete eval-baseline note template see `.dev/knowledge/eval-baseline-2026-04-27.md` in the repo (private notes; the format is described in `CLAUDE.local.md`).
