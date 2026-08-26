# 11. Exclude a term from both halves of the hybrid search

- Status: accepted
- Date: 2026-08-26
- Deciders: project owner
- Applies to: v1.1.0

## Context and Problem Statement

[ADR-0002](0002-compile-queries-into-per-token-fts-phrases.md) made the
full-text half of the hybrid compile a query into per-token `OR` phrases,
which widened recall but left no way to narrow a search by a word the caller
wants excluded. `path_globs` and `tags_any` / `tags_all` filter on document
metadata only; nothing in the query language reaches the text of a chunk.
SQLite FTS5 has a `NOT` operator, but the vector half of the hybrid knows
nothing about words — a full-text-only exclusion would be undone by
Reciprocal Rank Fusion re-admitting the same chunk from the vector leg, so a
search that reported an exclusion would not actually have enforced it.

The question this decision answers: given a hybrid of two retrievers where
only one understands text, how does excluding a term reach both, and what
happens to a query that excludes everything and leaves nothing to search
for?

## Decision Drivers

- Both halves of the hybrid must agree on which chunks are excluded — an
  FTS-only exclusion is silently undone by fusion.
- One implementation answers "does this chunk contain the term"; the vector
  half must not reimplement text matching to decide it.
- A query with no exclusion must be embedded, compiled and evaluated
  byte-for-byte as before v1.1.0 — the feature must cost nothing when
  unused.
- A query that reduces to "nothing to search for" (every group excluded)
  must fail loudly rather than silently return whatever the vector half
  prefers.

## Considered Options

1. **`NOT` on the full-text half only.** Compile the negative phrases into
   the FTS5 expression and leave the vector half untouched.
2. **A hard filter on both halves.** The FTS5 expression carries
   `(positives) NOT (negatives)`, and the vector half drops any candidate
   whose chunk id is in the set FTS5 returns for the negative expression
   alone.
3. **Soft demotion.** Lower the rank of a chunk that contains an excluded
   term after fusion, rather than dropping it.

## Decision Outcome

Chosen option: **2 — a hard filter on both halves**, judged by one FTS5
evaluation shared by both legs.

Option 1 rebuilds the ADR-0002 defect in mirror image: a hybrid search that
reports an exclusion and does not enforce it on the leg fusion also draws
from. Option 3 is a contract nobody can state to a caller — "usually gone"
is not an answer to "did my exclusion work". Option 2 costs one rowid-only
scan of the negative expression per search, with no ranking and no `LIMIT`:
measured at 934.5µs (best of 5) for a negative expression matching every one
of 5,000 chunks — the worst case, an excluded term present in every row —
against 3.5855ms for the ranked FTS query it accompanies in the same search
(measured:
`cargo test -p grooveseek --release --lib the_exclusion_id_scan_stays_cheaper_than_the_ranked_fts_query -- --ignored --nocapture`).
The scan stays at just over a quarter of the cost of the query it rides
alongside.

### Interface changes

- A `-` that begins a whitespace-delimited group is now an exclusion, judged
  against the same FTS row a positive match sees — `heading`, the
  contextual prefix, and `content` together (`schema.rs:109-115`), not the
  body alone. `"-foo"` restores the literal, pre-v1.1.0 search for a leading
  hyphen.
- `filter_applied.excluded_terms` echoes the phrases actually excluded
  (after tokenizing and the trigram floor) whenever the query excluded
  something — an exclusion alone leaves `filter_applied` non-empty even with
  no other filter given.
- `ConfigFingerprint.fts_query_version` becomes 3, so
  `groove eval --fail-on-regression` does not compare history across the
  change.
- A query made only of exclusions is refused on all three surfaces:
  `{"error": …}` over MCP, stderr and a non-zero exit on the command line,
  and a load-time error for a golden file.

### Consequences

- An unquoted `-word` is tokenized with the same rules as a positive phrase,
  so the independent-emit rule that widens recall for positives also widens
  an exclusion: `-再ランキング` also excludes `ランキング`. `-"..."` is the
  escape for excluding only the compound.
- The trigram floor applies to exclusions too: `-ab` excludes nothing,
  because a phrase under three characters cannot be searched for in either
  polarity.
- The parent retriever may expand a hit into text that contains the
  excluded term, since exclusion is judged on the hit chunk at search time,
  not on the content a later expansion adds.
- `query_phrases(positive_text)` equals `parse_query(raw).include` for
  every query with one exception: `foo -"bar"-baz`, where the raw text keeps
  a literal `-baz` after a quoted exclusion but `positive_text` re-reads it
  as a second exclusion. Search results are unaffected — both legs already
  use the raw query — but a highlight for that `-baz` is lost.
  `docs/citations.md` and `docs/retrieval-pipeline.md` describe spans as
  computed on the positive text, not as always agreeing with the raw
  query's phrase list.
- Worst-case FTS cost roughly doubles at the phrase cap: the negative
  expression is a second 32-phrase `OR` evaluated in the same statement, on
  top of the one rowid scan the vector leg adds.

### Confirmation

- `a_chunk_holding_an_excluded_term_never_reaches_the_fts_leg` and
  `an_excluded_term_drops_the_vector_nearest_chunk_too` pin the two halves
  against a real FTS5 table — the mutation detectors for the
  parenthesisation of `match_expr` and for the vector leg's `contains`
  check, respectively.
- `exclusion_is_judged_by_the_trigram_tokenizer_case_and_diacritics_included`
  pins that the judgment is FTS5's own, not a second Rust-side text match.
- proptest `positive_text_equals_the_raw_query_when_no_group_is_excluded`
  pins that a query without an exclusion embeds, compiles and evaluates
  exactly as it did before this decision.
- **Not held by a type**: that the embedder, the reranker and the spans call
  actually receive `positive_text()` rather than the raw query. Both legs of
  the hybrid already drop the excluded rows, so an integration test cannot
  see the difference if a future change quietly reverts to raw input.
  `match_spans_never_cover_an_excluded_term` does not close this for spans
  either: it calls `compute_match_spans(parsed.positive_text(), …)` itself,
  so what it pins is that function's behaviour *given* the positive text,
  and a call site reverted to the raw query leaves it green. All three call
  sites are a review item, not a guard.

## More Information

- feature-55 PR-2, branch `feature/search-exclusion-syntax`
- [ADR-0002](0002-compile-queries-into-per-token-fts-phrases.md) — the query
  compiler this extends
- `docs/retrieval-pipeline.md` — the resulting mechanism and its cost model
- `CHANGELOG.md`, v1.1.0 → Added / Changed
- Japanese version: [0011-exclude-a-term-from-both-halves-of-the-search.ja.md](./0011-exclude-a-term-from-both-halves-of-the-search.ja.md)
