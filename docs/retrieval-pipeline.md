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

`vec_chunks` (sqlite-vec, L2 distance — its default metric) and `fts_chunks` (FTS5 trigram + bm25 with a 2× heading weight by default) each return their own top-N. Reciprocal Rank Fusion combines them on the Rust side with `k = 60` by default (the standard RRF constant). The score returned to clients is the RRF score (higher = better), not a distance.

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
