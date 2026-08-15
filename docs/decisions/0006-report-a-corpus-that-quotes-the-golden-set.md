# 6. Report a corpus that quotes the golden set, and require more than one quote

- Status: accepted
- Date: 2026-08-15
- Deciders: project owner
- Applies to: v0.24.0

## Context and Problem Statement

`kb-mcp eval` measures retrieval against a golden query set. When the knowledge
base being measured is also where its owner writes notes — the normal case for a
personal or team KB — a note that quotes a golden query verbatim becomes the
strongest match for that query. It takes the top slot and pushes the labelled
answer down.

This was observed on the reference corpus: a note written the same day took rank
1 for a query whose wording it quoted, demoting the expected document to rank 4.
Aggregate recall survived, so nothing failed; the cause was found only because a
person happened to read the per-query rows. **The more that is written about the
evaluation, the harder the evaluation is to pass, and nothing says so.**

The awkward part is that the observation has two causes and they look identical.
A document that contains a query verbatim is either a note *about* the test, or
the source the query was written from — and in the second case it belongs in
that query's `expected`, so the golden file is what is wrong. Only the person who
wrote the golden set can tell them apart.

## Decision Drivers

- A warning that fires on a healthy corpus is worse than no warning: it trains
  the reader to skip it, and it will still be there when a real one appears.
- `eval`'s exit code already means something (`--fail-on-regression`). Whatever
  is reported must not compete for it.
- The report has to be honest about not knowing which of the two causes applies.
- Cost has to stay under the cost of what `eval` already does per run.

## Considered Options

Each option was measured against the reference corpus — 662 documents, 26 golden
queries, no known leak other than the one note — by counting **how many findings
it produces on a corpus that is healthy**.

1. **Embedding similarity**: report hits that are highly similar to the query
   text and not in `expected`.
2. **Verbatim quote, one is enough, top_k only**: report a `top_k` hit whose body
   contains the query text verbatim.
3. **Verbatim quote, one is enough, whole corpus**: the same, scanned over every
   indexed document rather than the retrieved ones.
4. **Verbatim quote, two or more distinct queries, whole corpus.**

## Decision Outcome

**Option 4.**

Option 1 has no threshold to find. Every top-ranked hit is highly similar to the
query — that is what retrieval is — so the condition is close to "report the
results".

Option 3 produced **8 findings, all false positives**. The reason is structural
rather than a matter of tuning: golden queries are frequently topic names
(`cross-encoder`, `torch.compile`, `Qwen3.5-Omni`), and a topic name appears
verbatim in the documents that explain that topic. One verbatim match is what a
*document about a topic* looks like.

Option 2 produced **0**. Restricting the scan to `top_k` does not reduce the
false positives (they are top hits by construction); it only makes the rule
weaker. Measured on the same corpus, the one genuinely leaking note appeared in
`top_k` for one of the queries it quoted and not for another, whose ten slots
were all chunks of a single document.

Option 4 produced **exactly one finding, and it was the note that was in fact
documenting the golden set** — quoting query strings inside backticks while
explaining how the golden queries were designed. Quoting several golden queries
is what a *note about the test* looks like, and it separates cleanly from the
population that made option 3 useless.

The finding is named for what was measured (`golden-queries-quoted`), not for
the cause it suggests, and the message states both possible causes. This follows
the same rule as `kb-mcp doctor`: report the observation, name the remedies,
change nothing.

### Consequences

- Every `eval` run scans the indexed chunk bodies once, inside the same read
  snapshot as the searches, so the report describes the same index the metrics
  came from. One pass over ~9.4k chunks is negligible beside the embedding and
  search work the run already does.
- Findings go to **stderr** as a warning and to `--format json` as `findings`.
  **The exit code does not change.** Whether a quote is a leak or a labelling gap
  is not something kb-mcp can decide, so it is not grounds to fail a build.
- `findings` is always present in the JSON, empty when nothing was found, so a
  consumer can distinguish "checked, nothing" from output that predates the
  check. `rank_in_top_k` is `null` rather than absent for the same reason: not
  reaching `top_k` is a measured fact, not missing data.
- Findings live on `EvalRun`, so they are written to the run history — and
  **deliberately outside `ConfigFingerprint`**, for the same reason the recorded
  corpus is: anything inside the fingerprint disables the diff when it changes,
  which would cost a run its baseline exactly when it reported something.
- Queries shorter than 12 characters after whitespace normalization are not
  matched. Short queries appear in many documents by chance and only add noise to
  the count the rule depends on. The measurement brackets the value from both
  sides: 8 and 12 give identical results on the reference corpus, and 16 loses
  the true finding, because golden queries skew short.
- Matching is **within one indexed text field**: each chunk's body and each
  chunk's heading, separately. Headings are included because the Markdown parser
  removes the heading line from the body while full-text search indexes headings
  at a *higher* weight than body text — so a note that lists golden queries as
  `##` headings, which is the most natural way to document a test, would be
  invisible to a body-only scan while actively competing with the labelled
  answers. Nothing is concatenated: joining fields or chunks would let unrelated
  text on either side of a seam read as a single quote, and the cost of not
  joining — a quote split across a seam is not seen — is the cheaper mistake.
  `context_text` is not scanned; it is built from the same document's ancestor
  headings, so scanning headings already finds that document.
- A document listed in a query's `expected` is not counted for that query. It
  contains the wording because it is the answer.
- There is no way to silence an intentional quote. With the report costing
  nothing but a line on stderr, an allow-list would mostly be a way to make a
  real leak permanent. It can be added later without breaking anything if the
  noise turns out to be real.
- **What this cannot see**: a corpus that quotes exactly one golden query. That
  is the price of the precision, and it is paid knowingly — the alternative was
  measured at eight false alarms per run.
