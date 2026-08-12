# 2. Compile queries into per-token `OR` phrases for full-text search

- Status: accepted
- Date: 2026-08-12
- Deciders: project owner
- Applies to: v0.16.0 (the previous behaviour shipped in v0.1.0 through v0.15.2)

## Context and Problem Statement

kb-mcp's hybrid search fuses two retrievers with Reciprocal Rank Fusion: a
sqlite-vec KNN over embeddings, and SQLite FTS5 over a trigram tokenizer. Until
v0.16.0 the FTS half received the user's query wrapped in a single quoted
phrase. Over a trigram tokenizer a quoted phrase is a contiguous-substring
match, so that construction searched for the entire query verbatim.

For a keyword this behaves acceptably. For a sentence it matches nothing at
all, because no document contains the question as written. Measured on the
dogfood knowledge base (650 documents, 9,419 chunks), **all ten
natural-language golden queries returned zero FTS candidates**, and of the 26
main golden queries only 16 had anything to fuse. On those queries the hybrid
was not hybrid: RRF had one input, and the system had been running as
vector-only retrieval while reporting itself as hybrid.

The defect is invisible from the outside. Nothing errors, no candidate count is
surfaced to a user, and results still come back — worse ones, from one
retriever. It survived fifteen releases, and the tests were structurally unable
to catch it: every fusion test placed the FTS-matching chunk at the query
vector, so the vector half alone satisfied the assertion.

The question this decision answers is how a query should be turned into an FTS5
`MATCH` expression, given a tokenizer that only matches substrings and a corpus
that is mostly Japanese, where words are not separated by spaces.

## Decision Drivers

- Both retrievers must contribute on the queries users actually type, which for
  this knowledge base are Japanese sentences.
- The change must not require re-indexing. The index, schema, and tokenizer
  represent hours of embedding computation for every user.
- Whatever splits the query must be inspectable and testable in isolation; the
  previous behaviour was a single expression built inline, and nothing pinned
  it.
- The compiler runs on untrusted input. An expression that FTS5 rejects is not
  a degraded search but an error from the whole query.

## Considered Options

1. **Status quo** — one verbatim phrase per query.
2. **Morphological analysis** — segment Japanese with a dictionary-backed
   tokenizer (lindera, vibrato) and emit one phrase per morpheme.
3. **Split at script boundaries** — cut the query at separators, then at
   transitions between kanji, hiragana, katakana, and other word characters.
4. **Pass the query to FTS5 unquoted** and let its own expression parser
   tokenize it.

## Decision Outcome

Chosen option: **3 — split at script boundaries**, because it recovers most of
the benefit of segmentation at none of its cost, and because it is a pure
function of the query string that can be tested exhaustively.

`再ランキングの評価について` compiles to
`"再ランキング" OR "ランキング" OR "の評価" OR "について"`. Script transitions are a
coarse but real proxy for word boundaries in Japanese: compounds are typically
kanji runs, loanwords katakana runs, and grammatical particles hiragana.

Measured on the dogfood corpus with the same scratch copy before and after
(bge-m3, no reranker):

| | before | after |
|---|---|---|
| Golden queries where fusion has two inputs | 16/26 | **26/26** |
| MRR (main / binary golden) | 0.955 / 0.939 | **0.962 / 0.955** |
| recall@10 | 0.954 | **0.965** |
| recall@5 | 0.926 | 0.906 |

Why the others were not chosen:

- **Option 1** is what produced the defect. It is retained as a *fallback*, not
  as the default: when tokenizing yields no usable phrase — every fragment
  shorter than the trigram floor, as in `AI と ML` — the whole trimmed query is
  searched verbatim, so no query class is made worse than it was in v0.15.x.
- **Option 2** is the accurate answer and was deliberately deferred. A
  dictionary tokenizer adds a multi-megabyte dictionary to a binary that is
  distributed as a self-contained executable, plus a load cost on a path that
  currently touches no model. The benefit over script boundaries is real but
  unmeasured, and this decision does not foreclose it: the seam is one function
  returning `Vec<String>`, so a morphological splitter can replace it without
  touching the callers. Tracked privately as a candidate.
- **Option 3 with `AND`** was rejected on the same evidence as option 1: a
  sentence's tokens do not co-occur in one chunk, so conjunction reproduces the
  empty result set. `OR` widens the candidate pool and lets bm25 and RRF do the
  ranking, which is what a fusion architecture is for.
- **Option 4** is unsafe. FTS5's expression parser reads a C string and treats
  `"`, `*`, `:`, `^`, `(`, `)`, `NEAR`, `AND`, `OR`, `NOT` as syntax, so an
  arbitrary user query is either a syntax error — failing the entire search,
  not just the FTS half — or an unintended operator. Every phrase kb-mcp emits
  is quoted and escaped for exactly this reason.

### Interface changes

Two things became part of the contract with users and with stored data:

- **`"..."` in a query is now meaningful.** A quoted region is kept as a
  verbatim phrase instead of being escaped into the text being searched for.
  This is how the pre-v0.16.0 behaviour is requested on demand, and it is how a
  multi-word English name is kept together. The reachable document set is
  therefore *not* a superset of the old one: `"a""b"` used to search for the
  literal text `"a""b"` and now searches for `a"b`.
- **`ConfigFingerprint` carries `fts_query_version`.** Evaluation history from
  before this change is not comparable — the same golden queries against the
  same index produce different candidates — so `kb-mcp eval --fail-on-regression`
  refuses to compare across the boundary rather than reporting a regression or
  an improvement that is really a change of method. Deserialising a fingerprint
  without the field yields version 1.

### Consequences

- The FTS half contributes on every golden query. recall@10 and MRR improve;
  recall@5 drops by 0.020, from two queries out of 26 where a second expected
  document moved from rank 5 to rank 8. Accepted deliberately: the rank-1
  result is equal or better in both cases, and the vacated slots had been
  occupied by duplicate chunks of the same document.
- **A fragment with no neighbour inside its group is dropped, not merged.** A
  join never crosses a separator, so in `AI について` the `AI` has a space on
  one side and nothing on the other and never reaches the three-character
  trigram floor; the full-text half searches only for `について`. Quoting it
  does not rescue it — a quoted phrase under three characters is dropped by the
  same floor — but quoting a wider region does, at the cost of searching that
  region verbatim.
- Worst-case FTS cost rose by roughly **10×**. A query of 32 common fragments,
  each matching every row, costs 171 ms against 16 ms for a single such phrase
  at 20,000 rows. The cost is linear in both the matching population and the
  phrase count, and the population ceiling — every row in the index — was
  always one common substring away. `LIMIT` does not bound this, because
  `ORDER BY bm25(...)` scores every match first. The phrase cap of 32 is the
  only effective lever and is left where the retrieval evaluation above
  measured it.
- Query compilation became a testable unit. The compiler is four pure stages,
  chosen over a single-pass state machine specifically because the rule for an
  unterminated quote produces identical end-to-end output under either
  implementation — a mutation of that rule is unkillable without the seams.
- No re-index. The change is query-side only.

### Confirmation

- `db/fts_query.rs` holds 38 unit tests across the four stages plus a property
  test. Separately, a 50-input test runs every generated expression through a
  real `MATCH`, because comparing strings never establishes that FTS5 *accepts*
  what was built — and a rejected expression fails the whole search, not just
  the full-text half.
- `fts_or_expansion_is_one_statement_over_the_union_of_its_phrases` pins that
  the expansion is a union executed as one statement, counted from statements
  traced out of SQLite rather than calls into the Rust wrapper.
- `bu03_or_expansion_stays_within_a_small_multiple_of_a_single_phrase` bounds
  the cost multiple rather than an absolute duration.
- `fts_decides_the_top_rank_when_the_vector_leg_prefers_another_chunk` fails if
  the full-text half stops contributing to the fused ranking — the property no
  test held before.

## More Information

- feature-48, PR #134 (v0.16.0); cost measurement in PR #136
- `docs/retrieval-pipeline.md` — the resulting mechanism, and its cost model
- `CHANGELOG.md`, v0.16.0 → Changed
- Japanese version: [0002-compile-queries-into-per-token-fts-phrases.ja.md](./0002-compile-queries-into-per-token-fts-phrases.ja.md)
