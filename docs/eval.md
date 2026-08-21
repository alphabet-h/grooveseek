# `groove eval` — Retrieval quality evaluation

> **日本語版**: [eval.ja.md](./eval.ja.md)

## Who this is for

You only need this subcommand if you want to **compare retrieval quality across
model/config changes** or **guard against regressions when tuning**.

Regular users running `groove index` + `groove serve` **never need to touch this**.
`eval` is an independent, opt-in subcommand. Without a golden file, it does
nothing but print an error with a hint.

## What it does

Given a small file of "questions with known answers" (*golden queries*),
`groove eval` runs each question through the same hybrid search used by the
MCP `search` tool, then computes how well the returned chunks match what you
expected. On the second run onwards it diffs against the previous run, so you
can see whether a config change improved or regressed quality.

## Quick start

### 1. Write a golden file

Place it at `<kb>/.groove-eval.yml`:

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
groove eval --kb-path ./knowledge-base
```

Output:

```
groove eval — 2026-04-24T14:32:01+09:00
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

### Corpus changed between runs (v0.15.0+)

Every run records the index it measured, and the header repeats it:

```
  corpus: 646 docs / 11215 chunks
```

When that differs from the compared run, the change is named and the numbers
below are qualified:

```
  corpus: 646 docs / 11215 chunks
    ⚠️ corpus changed since last run (642 -> 646 documents, 11090 -> 11215 chunks)
       a delta below may reflect that, not retrieval
```

The digest covers the **indexed chunks**, not the source files — chunks are
what the search reads, so a rebuild that parses unchanged files differently is
caught even though every file hash held. A document rewritten in place moves
neither count, so counts alone would call that "unchanged":

```
    ⚠️ corpus changed since last run (same document and chunk counts, different contents)
```

**Unlike a golden change, this does not disable the diff.** That is deliberate.
A knowledge base is normally growing, so treating every added document as a
reason to stop comparing would make `--fail-on-regression` inert exactly when it
is wanted. The corpus is therefore reported but kept out of the compatibility
test: runs stay comparable, and a drop can be read with the knowledge that the
competition changed.

The consequence is worth stating plainly: **a reported regression may be caused
by the corpus rather than by retrieval**, and only this line tells you which to
suspect. `--format json` carries `corpus` and a `corpus_changed` boolean, which
is `null` when there is nothing to compare against — distinct from `false`.

Runs recorded before this existed carry no corpus, and are never reported as
changed; the first run after that writes one, and the next run compares normally.

### When the corpus quotes the golden set (v0.24.0+)

If you keep notes about your evaluation *inside the knowledge base you are
evaluating*, those notes become search results. A note that quotes a golden
query verbatim is the strongest possible match for that query, so it takes the
top slot and pushes the real answer down — the golden set gets harder to pass
the more you write about it.

Every run scans the indexed corpus for this and reports what it finds on
**stderr**, leaving the exit code alone:

```
groove eval: 1 document(s) quote 2 or more golden queries verbatim (golden-queries-quoted).
  engineering/deep-dive/rag/evaluation.md
    torch-compile (not in top_k)
    cross-encoder-reranker (rank 8)
  Either these notes leaked into the corpus, or the queries came from them
  and the documents belong in `expected`. groove eval changes neither.
```

The same findings are in `--format json` under `findings`, as an array that is
present (and empty) even when nothing was found, so a consumer can tell "checked,
nothing" from "an older version that never checked":

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

`rank_in_top_k` is `null` when the document quotes the query but did not reach
that query's `top_k` — it is in the corpus but has not taken a slot yet.

**The report does not say which of the two causes it is**, because it cannot: a
document that quotes a query is either a note *about* the test, or the source
the query was written from — in which case it belongs in that query's `expected`
and the golden file is what needs fixing. Only the person who wrote the golden
set knows. `eval` reports and changes nothing.

**Why "two or more" queries, and not one.** A single verbatim match is not
evidence of anything: golden queries are often topic names (`cross-encoder`,
`torch.compile`), which naturally appear in the documents that explain them.
Measured on a healthy 662-document corpus with 26 golden queries, reporting
every single match produced **8 findings, all false positives**, while requiring
two distinct queries in one document produced **exactly one — the note that was
in fact documenting the golden set**. A document quoting several golden queries
is what "a note about the test" looks like; one quoting a single query is what
"a document about that topic" looks like.

Two consequences worth knowing:

- Queries shorter than **12 characters** after whitespace normalization are not
  matched at all. Short queries occur in too many documents by chance. The same
  measurement brackets this value: 8 and 12 give identical results, and 16 loses
  the true finding.
- Matching happens **within one indexed text field**, and the fields scanned are
  exactly the ones full-text search indexes: each chunk's heading, its
  breadcrumb, and its body. That match is the whole selection rule — the scan is
  looking for text that can displace the labelled answer, so it has to cover
  precisely the text that has the power to. Headings matter because the Markdown
  parser strips the heading line out of the body and search weights headings
  *above* body text; the breadcrumb matters because it begins with the document
  title, which is frontmatter or the filename and appears in neither of the
  other two. (The breadcrumb is empty unless contextual indexing was enabled at
  index time, in which case scanning it costs nothing.) Nothing is concatenated
  first, so a quote split across a seam is not seen — deliberately, since joining
  would let unrelated text on either side of the seam read as one quote.

The scan is one pass over the indexed chunk bodies, inside the same read
snapshot as the searches, so it reports on exactly the index the metrics came
from.

## Configuration

All knobs are optional in `groove.toml`:

```toml
[eval]
golden = ".groove-eval.yml"    # default: <kb_path>/.groove-eval.yml
history_size = 10              # default: 10
k_values = [1, 5, 10]          # default: [1, 5, 10]
regression_threshold = 0.05    # default: 0.05
```

CLI flags override config values. Recognized flags: `--golden`, `--k 1,5,10`,
`--model`, `--reranker`, `--limit`, `--format text|json`, `--no-history`,
`--no-diff`, `--no-color`, `--fail-on-regression`. Pipeline flags (v0.7.0+):
`--mmr <bool>` / `--mmr-lambda <0..1>` / `--mmr-same-doc-penalty <0..1>` /
`--parent-retriever <bool>` — exact same semantics as on `groove search`,
see [retrieval-pipeline.md](./retrieval-pipeline.md) for what each knob does.

### `--fail-on-regression` (CI gate)

Exit with code 1 if any aggregate metric (`recall@k` for any k, `MRR`, or
`ndcg@k` for any k) regressed from the previous **compatible** run by more
than `regression_threshold` (default 0.05; tune via `[eval].regression_threshold`
in `groove.toml`). "Compatible" means the previous run had the same
fingerprint — `model`, `reranker`, `limit`, `k_values`, the golden YAML's
content hash, the metric implementation version, and (v0.7.0+) the effective
`[search.mmr]` / `[search.parent_retriever]` settings, plus (v0.13.0+) a
non-default `[search.fusion]`, (v0.14.0+) the index's context mode when it
was built with `[contextual].enabled = true`, and (v0.16.0+) the FTS query
compilation version (`fts_query_version`). Toggling MMR or parent retriever, or moving
the fusion parameters off their built-in defaults, therefore breaks
fingerprint compatibility (intentionally — comparing `recall@k` with the
diversity stage on vs off is apples-to-oranges). Switching `[contextual]` therefore breaks compatibility too, as it should:
that setting changes every chunk's embedding and FTS text and requires a
`--force` re-index, so the runs on either side measure different indexes even
though the model and golden file are identical. The mode recorded is the one
the **index** carries (`index_meta.context_mode`), not what the config asked
for. Context-off runs record nothing, so they stay comparable with every
baseline taken before this existed.

**A history file that cannot be read stops the run.** Both files `eval` keeps
default to living inside the knowledge base, so both are read through the same
checks `.grooveignore` gets: a hard link, something that is not a regular file,
or a size past the cap (1 MiB for the golden, 64 MiB for the history) is
refused — and, on Unix, a symlink. That last one is deliberately Unix-scoped:
creating a symlink on Windows needs a privilege this threat model's attacker
does not have, and refusing reparse points there would refuse every OneDrive
and Dropbox placeholder. For the history that refusal is an **error**, not an empty
history — the new run would otherwise be saved over the file, replacing every
baseline with one run, and `--fail-on-regression` would pass without having
compared anything. Content that *was* read and does not parse still starts
fresh, because those bytes held no baseline. `--no-history` skips the file.

Saving is bounded by the same number, so `eval` cannot write a history it will
refuse: the **oldest** runs are dropped until the file fits, with a warning
naming how many were kept. `history_size` remains what you asked for; this is
the floor under it. A single run that does not fit is reported instead of
written — that means a very large golden set or a very high `--limit`.

Note that history written **before v0.13.0 is incompatible regardless of
fusion settings**: `metric_version` went 1 → 2 when the metric implementation
was corrected, and the fingerprint is compared as a whole. Those runs are
skipped rather than compared, which is the intended behavior — the older
numbers were computed by a different formula.
The same holds for history written **before v0.16.0**: `fts_query_version`
went 1 → 2 when the query-to-`MATCH` compilation changed (see
[retrieval-pipeline.md](./retrieval-pipeline.md)), so those runs — including a
frozen baseline — drop out of the comparison. That is intentional as well: the
two versions send different expressions to FTS5, so they measure different
retrieval even with the same model, index, and golden file.
Updating the golden file likewise does **not** trigger a false regression
on the next run; it just means the comparison is skipped.

History is still written before the process exits, so the new run is
recorded for the next comparison.

Typical CI shape:

```yaml
- name: groove eval gate
  run: groove eval --kb-path knowledge-base --fail-on-regression
```

The flag is a no-op when there is no previous run yet, when `--no-history`
is set, when `--no-diff` is set (since the comparison is suppressed), or
when the previous run's fingerprint differs.

## Troubleshooting

| Symptom | Cause | Fix |
|---|---|---|
| `no golden file at ...` | Missing golden YAML | Create `.groove-eval.yml` or pass `--golden <path>` |
| `No index found at ...` | KB not indexed | Run `groove index --kb-path <kb>` first |
| `✗ <id>  recall@N: 0.00` (per-query) | Nothing the query retrieved matched this entry's `expected` paths — often a typo, a path that was never indexed, or a genuinely missed document | Check the path spelling, then search for a phrase you know is *inside* that document and look at the `path` of the hits (searching for the path itself proves nothing: FTS indexes `heading` / `context` / `content`, and the embeddings do not include the path either). A real miss is a retrieval result, not a config error |
| `golden changed since last run, diff disabled` | Golden file edited | Expected; the next run will diff normally |
| Model mismatch error | `--model` does not match the indexed model | Pass the model used for indexing, or re-index |

## Non-goals (intentional)

- **Graded relevance (0 / 1 / 2)**: not supported, and **not** silently ignored — every golden struct is `deny_unknown_fields`, so a `relevance:` key aborts the run before anything is evaluated:

  ```
  Error: failed to parse golden file: golden.yaml

  Caused by:
      unknown field `relevance`, expected `path` or `heading`
  ```
- **Sweeps / matrices over models**: to compare embedding models, run `eval`
  twice against two separately indexed databases — one `eval` run measures one
  index. (Sweeping the *fusion* parameters against a single index is supported:
  that is `groove tune`, described in the next section.)
- **Mandatory adoption**: running `eval` does not change anything about
  `index` / `serve` / `search`. It is a purely auxiliary tool

## `groove tune` — measuring the fusion parameters (v0.13.0+)

`groove eval` tells you how good retrieval is. `groove tune` tells you whether
the two fusion knobs (`rrf_k` and the three bm25 column weights) can move that
number **on your KB at all**. It applies nothing — the output is either a
paste-ready `[search.fusion]` snippet or the conclusion that the built-in
defaults should stay.

```bash
groove tune --kb-path knowledge-base
groove tune --kb-path knowledge-base --format json > tune.json
groove tune --kb-path knowledge-base --golden ./ci-golden.yml --limit 20
```

It reads the same golden set as `groove eval` and takes the same flags for
finding it: `--golden <PATH>` to use a file other than `.groove-eval.yml`,
`--limit` to change how many hits each query fetches, `--no-color` to drop ANSI
from the tables, and `--model` to match the index being measured. Unlike
`eval`, it takes no `--reranker`: it measures the fusion stage, which sits
before reranking.

### What it needs from your golden set

Since v0.16.0 groove compiles a query into per-token phrases joined with `OR`
(see [retrieval-pipeline.md](./retrieval-pipeline.md)), so **a query no longer
has to occur verbatim in the text to reach the bm25 stage** — each of its
fragments can match on its own, which is what makes a natural-language golden
set measurable at all. What still leaves nothing to measure is a query from
which no phrase survives, or whose phrases match nothing: every grid point then
returns the same ranking. `tune` therefore starts with a pre-flight pass:

- it counts FTS candidates per query and reports the **effective N** (queries
  with at least 2 candidates — 0 candidates falls back to vector-only, 1
  candidate has a fixed rank and so is insensitive to the weights)
- if the effective N is 0 it prints the diagnosis to stderr and **exits 2**
  without running the grid
- if the effective N is below 50 it warns that this is under the IR convention
  and the numbers are suggestive rather than conclusive

A second warning fires — **after** the effective-N check above, so only on
runs that actually reach the grid — whenever the KB was indexed with
`[contextual]` off: every chunk's `context` column is then empty, so sweeping
`bm25_context_weight` from 0.5 to 4.0 cannot change any score. Since
`[contextual]` is off by default, most runs see it, and it is a statement
about the index rather than about the parameter:

```
groove tune: WARNING — every chunk has an empty context column, so the
bm25_context_weight axis is a no-op on this KB (contextual retrieval is off).
Its rows below mean "not measured", not "has no effect".
```

To get a measurable golden set, include queries carrying distinctive terms:
proper nouns, API names, command names, error codes. Those compile to phrases
rare enough that bm25 can tell documents apart, whereas a query made only of
common fragments matches everywhere and the weights have little to separate.
Avoid queries under 3 characters (the trigram floor leaves nothing to send to
FTS) and column-filter syntax such as `heading:foo` (the `:` is a separator, so
the two halves become ordinary phrases and it never acts as a filter).

### How the recommendation is guarded

Small golden sets almost always overfit an argmax, so a candidate is only
recommended when **all** of the following hold:

1. the refit condition differs from the built-in defaults
2. held-out mean ΔnDCG@5 > 0.02
3. held-out mean ΔnDCG@5 > 3 × paired SE (`SD({d_j}) / sqrt(N)`)
4. selection stability > 0.5 — more than half of the leave-one-query-out folds
   picked the same condition (folds disagreeing is the most direct overfitting
   signal there is)
5. no secondary metric (recall@k for any k, MRR) regressed against the defaults

> **Why the multiplier is 3 rather than the 2 that "2 sigma" would suggest.**
> `SD({d_j}) / sqrt(N)` assumes the per-fold differences are independent. The N leave-one-out
> selections each share N−2 queries, which lets the folds pick *different*
> conditions — though sharing training data only makes correlation possible
> rather than creating it. Nor is fold agreement alone enough to restore
> independence: even when the folds agree, *which* condition they agreed on was
> itself chosen from the shared rows, so every difference still depends on it.
> What would decouple them is the selection being effectively fixed across
> sampled golden sets, which is a different property.
>
> Simulated against a known data-generating process, the reported SE came out at
> **0.53–0.60** of the real one in the three settings where the selection varied
> (114–184 distinct conditions chosen across 300 replications), and at **1.03**
> in the one where it did not vary at all — a single condition across all 7,800
> fold selections. The count is of the *fold* selections that generate each
> difference, not of the refit chosen from all N rows; the two diverge (114 vs
> 64 in the first setting), so the refit would have understated the variation.
>
> **What that costs is measured directly rather than converted into a sigma
> level** — the reported SE varies per run and can correlate with the observed
> mean delta, so a ratio of averages does not determine how often the gate
> fires. Run with **no true winner at all**, a multiplier of 2 fired in
> **12.7%** of replications and carried the full five-criterion verdict to
> "adopt" just as often — where a calibrated one-sided 2 sigma test would be
> ~2.3%. A golden set with nothing to find yielded a recommendation about one
> run in eight.
>
> The multiplier was therefore swept against that rate directly (2,000
> replications per setting):
>
> | multiplier | adopts under the null (N=26 / N=12) | detects a findable edge |
> |---|---|---|
> | 2 (previous) | 12.7% / 9.7% | 99.0% |
> | **3 (current)** | **3.4% / 3.1%** | **95.2%** |
> | 4 | 0.5% / 0.8% | 79.4% |
>
> 3 buys a 3.7x cut in false adoptions for 3.8 points of power, which is why it
> ships. **Tightening criterion 2 instead does not work**: taking the 0.02 floor
> to 0.04 moves the null rate only 12.7% → 12.1%, while dropping that same power
> from 99.0% to 51.9%. Restricting to replications that pass criterion 4 lifts
> the SE ratio to 0.62–0.73 — the stability gate narrows the gap without closing
> it, and 192–300 of 300 replications passed it, so this is not a corner case.
>
> Two caveats on those rates. The synthetic fixture writes the same value into
> nDCG, recall and MRR, which makes criterion 5 easy to satisfy, so they do not
> establish how much the secondary-metric guard binds on real golden sets. And
> the null rate is only part of the error budget: on a landscape that does have
> a real winner but is noisy, a sizable share of adoptions pick the *wrong*
> condition — at N=12, roughly half of them.
>
> The simulations live in `tune.rs` as
> `au16_paired_se_versus_the_true_standard_error` and
> `au68_adoption_rate_across_the_two_thresholds`.

Otherwise the verdict is "keep the built-in defaults", which is a normal and
expected outcome: the RRF paper measured only ~0.4% relative MAP movement
across k ∈ [30, 100], and Elasticsearch documents RRF as requiring no tuning.

The report also prints the per-query breakdown (how many queries got worse and
by how much), because rank fusion routinely hides per-query losses behind an
average gain.

### Confirming a recommendation before you keep it

`tune` always measures the plain RRF stage with **no reranker**, so a gain it
finds has not been shown to survive the full pipeline. If you get an `adopt`
verdict, paste the snippet into `groove.toml` and re-run `eval` with your real
configuration before keeping it:

```bash
groove eval --kb-path knowledge-base --reranker bge-v2-m3 --no-history
```

Compare against the same command run with `[search.fusion]` removed. If the
reranked numbers do not improve, drop the change — the reranker frequently
absorbs (or reverses) upstream ranking differences.
