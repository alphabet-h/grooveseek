# `kb-mcp eval` — Retrieval quality evaluation

> **日本語版**: [eval.ja.md](./eval.ja.md)

## Who this is for

You only need this subcommand if you want to **compare retrieval quality across
model/config changes** or **guard against regressions when tuning**.

Regular users running `kb-mcp index` + `kb-mcp serve` **never need to touch this**.
`eval` is an independent, opt-in subcommand. Without a golden file, it does
nothing but print an error with a hint.

## What it does

Given a small file of "questions with known answers" (*golden queries*),
`kb-mcp eval` runs each question through the same hybrid search used by the
MCP `search` tool, then computes how well the returned chunks match what you
expected. On the second run onwards it diffs against the previous run, so you
can see whether a config change improved or regressed quality.

## Quick start

### 1. Write a golden file

Place it at `<kb>/.kb-mcp-eval.yml`:

```yaml
queries:
  - id: rrf-basics            # optional, used as the diff row key
    query: "What does the k parameter in RRF do?"
    expected:
      - path: "docs/ARCHITECTURE.md"
        heading: "Data flow"   # optional; omit for file-level hit
      - path: "src/db.rs"      # heading omitted → any hit in this file counts

  - query: "How are chunks deduplicated?"
    expected:
      - path: "src/indexer.rs"
```

### 2. Run

```bash
kb-mcp eval --kb-path ./knowledge-base
```

Output:

```
kb-mcp eval — 2026-04-24T14:32:01+09:00
  model: bge-m3    reranker: none    limit: 10    queries: 2

Aggregate
  recall@1   0.500
  recall@5   1.000
  recall@10  1.000
  MRR        0.750
  nDCG@10    0.821

Per-query (regressions and misses, 1 of 2)
  ✗ 32-char-truncated-query  recall@10: 0.00    expected src/indexer.rs missing
```

On the next run it will automatically show a diff against this one.

## Golden YAML reference

| Field | Type | Required | Meaning |
|---|---|---|---|
| `queries` | list | yes | Queries to evaluate |
| `queries[].query` | string | yes | The search query text |
| `queries[].expected` | list | yes | Ground-truth hits (at least one entry) |
| `queries[].expected[].path` | string | yes | KB-relative path, e.g. `docs/foo.md` |
| `queries[].expected[].heading` | string | no | If given, the returned chunk must match this heading (case- and whitespace-insensitive) |
| `queries[].id` | string | no | Stable identifier for diff row keys (default: first 32 chars of `query`) |
| `queries[].tags` | list | no | Reserved for future drill-down filtering |
| `defaults.limit` | int | no | Reserved; currently ignored — use CLI `--limit` |
| `defaults.rerank` | bool | no | Reserved; currently ignored — use CLI `--reranker` |

**Hit rule**: an expected entry counts as a hit if a returned chunk has the
same `path`, and (if `heading` is given) the same normalized heading
(`.trim().to_lowercase()`). No `heading` = any chunk in that file counts.

## Metrics explained

Each query has some number of *expected* hits. After running the query, we
look at the top-*k* returned chunks and compare.

### recall@k

> "Of all expected hits, what fraction appeared in the top *k*?"

Formula: `|expected ∩ top_k| / |expected|`. Range: 0.0 – 1.0.

Read this as *coverage*. `recall@10 = 0.8` means 80 % of what you expected
was in the top 10. It doesn't care about the order within top-*k*.

### MRR (Mean Reciprocal Rank)

> "How quickly did we find the first correct answer?"

For each query, MRR = `1 / rank` of the first expected hit (0 if none).
A value of 1.0 means "first result was correct", 0.5 means "second result
was first correct", etc. The report shows the mean across all queries.

Use this when you care more about the *top* result than the whole set.

### nDCG@k (Normalized Discounted Cumulative Gain)

> "Are the expected hits concentrated at the top?"

Rewards expected hits that appear early in the ranking more than those near
the bottom. Normalized so 1.0 means "all expected hits at the very top"
(ideal ordering). Range: 0.0 – 1.0.

Use this to detect improvements in *ordering*, not just presence. If
`recall@10` is unchanged but `nDCG@10` improved, you moved correct answers
higher.

## Understanding the diff output

Arrows annotate the change since the previous run:

- **↑ 0.056** (green): improved by more than `regression_threshold` (default 0.05)
- **↓ 0.056** (red): regressed by more than `regression_threshold`
- **↑ / ↓ 0.010** (gray): moved, but within noise
- **—**: unchanged

The per-query section only lists queries that **regressed** or **missed**
(current `recall@max_k = 0`). For the full list, use `--format json`.

### Golden changed between runs

If you edit the golden file between runs, the fingerprints differ and the
diff is disabled:

```
⚠️ golden changed since last run, diff disabled
```

The current numbers still print. The next run will diff against this one.

## Configuration

All knobs are optional in `kb-mcp.toml`:

```toml
[eval]
golden = ".kb-mcp-eval.yml"    # default: <kb_path>/.kb-mcp-eval.yml
history_size = 10              # default: 10
k_values = [1, 5, 10]          # default: [1, 5, 10]
regression_threshold = 0.05    # default: 0.05
```

CLI flags override config values. Recognized flags: `--golden`, `--k 1,5,10`,
`--model`, `--reranker`, `--limit`, `--format text|json`, `--no-history`,
`--no-diff`, `--no-color`, `--fail-on-regression`. Pipeline flags (v0.7.0+):
`--mmr <bool>` / `--mmr-lambda <0..1>` / `--mmr-same-doc-penalty <0..1>` /
`--parent-retriever <bool>` — exact same semantics as on `kb-mcp search`,
see [retrieval-pipeline.md](./retrieval-pipeline.md) for what each knob does.

### `--fail-on-regression` (CI gate)

Exit with code 1 if any aggregate metric (`recall@k` for any k, `MRR`, or
`ndcg@k` for any k) regressed from the previous **compatible** run by more
than `regression_threshold` (default 0.05; tune via `[eval].regression_threshold`
in `kb-mcp.toml`). "Compatible" means the previous run had the same
fingerprint — `model`, `reranker`, `limit`, `k_values`, the golden YAML's
content hash, the metric implementation version, and (v0.7.0+) the effective
`[search.mmr]` / `[search.parent_retriever]` settings plus (v0.13.0+) a
non-default `[search.fusion]`. Toggling MMR or parent retriever, or moving
the fusion parameters off their built-in defaults, therefore breaks
fingerprint compatibility (intentionally — comparing `recall@k` with the
diversity stage on vs off is apples-to-oranges). Runs at the built-in fusion
defaults keep comparing cleanly against baselines recorded before v0.13.0.
Updating the golden file likewise does **not** trigger a false regression
on the next run; it just means the comparison is skipped.

History is still written before the process exits, so the new run is
recorded for the next comparison.

Typical CI shape:

```yaml
- name: kb-mcp eval gate
  run: kb-mcp eval --kb-path knowledge-base --fail-on-regression
```

The flag is a no-op when there is no previous run yet, when `--no-history`
is set, when `--no-diff` is set (since the comparison is suppressed), or
when the previous run's fingerprint differs.

## Troubleshooting

| Symptom | Cause | Fix |
|---|---|---|
| `no golden file at ...` | Missing golden YAML | Create `.kb-mcp-eval.yml` or pass `--golden <path>` |
| `No index found at ...` | KB not indexed | Run `kb-mcp index --kb-path <kb>` first |
| `expected path not in index` (per-query) | The path in `expected` does not exist in the index | Check spelling / re-index |
| `golden changed since last run, diff disabled` | Golden file edited | Expected; the next run will diff normally |
| Model mismatch error | `--model` does not match the indexed model | Pass the model used for indexing, or re-index |

## Non-goals (intentional)

- **Graded relevance (0 / 1 / 2)**: parsed tolerantly but ignored today
- **Sweeps / matrices**: to compare models, run `eval` twice against two
  different indexed databases
- **Mandatory adoption**: running `eval` does not change anything about
  `index` / `serve` / `search`. It is a purely auxiliary tool

## `kb-mcp tune` — measuring the fusion parameters (v0.13.0+)

`kb-mcp eval` tells you how good retrieval is. `kb-mcp tune` tells you whether
the two fusion knobs (`rrf_k` and the three bm25 column weights) can move that
number **on your KB at all**. It applies nothing — the output is either a
paste-ready `[search.fusion]` snippet or the conclusion that the built-in
defaults should stay.

```bash
kb-mcp tune --kb-path knowledge-base
kb-mcp tune --kb-path knowledge-base --format json > tune.json
```

### What it needs from your golden set

kb-mcp's FTS wraps the entire query in a single quoted phrase over a trigram
tokenizer, so **a query only reaches the bm25 stage when it occurs verbatim in
the text**. A golden set made only of natural-language questions produces zero
FTS candidates for every query, every grid point returns the same ranking, and
there is nothing to measure. `tune` therefore starts with a pre-flight pass:

- it counts FTS candidates per query and reports the **effective N** (queries
  with at least 2 candidates — 0 candidates falls back to vector-only, 1
  candidate has a fixed rank and so is insensitive to the weights)
- if the effective N is 0 it prints the diagnosis to stderr and **exits 2**
  without running the grid
- if the effective N is below 50 it warns that this is under the IR convention
  and the numbers are suggestive rather than conclusive

To get a measurable golden set, include verbatim queries: proper nouns, API
names, command names, error codes. Avoid queries under 3 characters (the
trigram floor) and column-filter syntax such as `heading:foo` (it is neutralized
by query sanitization, so it never acts as a filter).

### How the recommendation is guarded

Small golden sets almost always overfit an argmax, so a candidate is only
recommended when **all** of the following hold:

1. the refit condition differs from the built-in defaults
2. held-out mean ΔnDCG@5 > 0.02
3. held-out mean ΔnDCG@5 > 2 × paired SE (`SD({d_j}) / sqrt(N)`)
4. selection stability > 0.5 — more than half of the leave-one-query-out folds
   picked the same condition (folds disagreeing is the most direct overfitting
   signal there is)
5. no secondary metric (recall@k for any k, MRR) regressed against the defaults

Otherwise the verdict is "keep the built-in defaults", which is a normal and
expected outcome: the RRF paper measured only ~0.4% relative MAP movement
across k ∈ [30, 100], and Elasticsearch documents RRF as requiring no tuning.

The report also prints the per-query breakdown (how many queries got worse and
by how much), because rank fusion routinely hides per-query losses behind an
average gain.

### Confirming a recommendation before you keep it

`tune` always measures the plain RRF stage with **no reranker**, so a gain it
finds has not been shown to survive the full pipeline. If you get an `adopt`
verdict, paste the snippet into `kb-mcp.toml` and re-run `eval` with your real
configuration before keeping it:

```bash
kb-mcp eval --kb-path knowledge-base --reranker bge-v2-m3 --no-history
```

Compare against the same command run with `[search.fusion]` removed. If the
reranked numbers do not improve, drop the change — the reranker frequently
absorbs (or reverses) upstream ranking differences.
