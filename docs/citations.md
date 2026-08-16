# Citations

The `search` MCP tool returns `match_spans` for each hit, indicating where the
query terms matched within the chunk's `content`. This helps Claude / clients
quote source text accurately and reduces hallucination.

> **日本語版**: [citations.ja.md](./citations.ja.md)

## Output shape

```jsonc
{
  "results": [
    {
      "score": 0.0327,
      "path": "docs/foo.md",
      "content": "Use tokio::spawn for async tasks.",
      "match_spans": [
        {"start": 4,  "end": 9 },   // "tokio"
        {"start": 11, "end": 16}    // "spawn"
      ],
      // ... other fields
    }
  ]
}
```

## `match_spans` semantics

| Value | Meaning |
|---|---|
| `null` (key omitted) | Match spans not computed. Three cases: any of the terms the query splits into contains a non-ASCII character, the query is empty / whitespace-only, or the chunk's `content` exceeds 256 KiB (`MATCH_SPAN_CONTENT_MAX_BYTES`, a guard against O(N×M) scanning on abnormal input) |
| `[]` (empty array) | Computed, but no match was found |
| `[{...}, ...]` | Computed; one or more matches |

## Byte offsets

`start` / `end` are **byte offsets** into the chunk's `content` string. `groove`
guarantees that both indices fall on UTF-8 codepoint boundaries, so clients can
safely slice:

> **Note (v0.7.0+):** When the parent retriever (`[search.parent_retriever]`)
> fires, the returned `content` is the **expanded** text (adjacent siblings or
> the whole document), and `match_spans` are byte offsets into that expanded
> content — not the original chunk. Clients can keep slicing
> `content.get(start..end)` safely; the new `expanded_from` field on the same
> hit indicates which chunk range was merged in. See
> [retrieval-pipeline.md](./retrieval-pipeline.md) for the full pipeline order
> (`match_spans` is recomputed *after* parent expansion).

```typescript
const snippet = content.slice(span.start, span.end);
```

In Rust:

```rust
let snippet = content.get(span.start..span.end).unwrap_or("");
```

If you ever observe a span that breaks codepoint boundaries, please file a bug.

## What gets matched

`match_spans` are computed by:

1. Splitting the query into terms with `query_phrases` (v0.16.0+) — **the same split that produces the FTS5 phrases** (see [retrieval-pipeline.md](./retrieval-pipeline.md)). Before v0.16.0 the split here was an independent whitespace split, so a quoted query such as `"Foundry Local"` was looked up as `"Foundry` and `Local"`: FTS matched the phrase while the spans came back empty.
2. Falling back to a whitespace split of the trimmed query when — and only when — that split yields no phrase at all, as in `ab cd` where every fragment is under the trigram floor. Such a query does not reach FTS through phrases either.
3. Lower-casing both the terms and the content (ASCII fold only).
4. Searching for each term as a substring (case-insensitive) in `content`.
5. Giving each term a share of the 100-span budget — `floor(100 / number of terms)`, at least one each — and taking that many of its occurrences in document order.
6. Folding the collected positions into **sorted, non-overlapping** spans: overlapping matches become one span covering their union, while merely adjacent ones stay separate.

The result satisfies, for every response (v0.18.0+):

| Guarantee | Meaning |
| --- | --- |
| Sorted and disjoint | `spans[i].end <= spans[i+1].start`. No span overlaps another. |
| Non-empty | Every span has `start < end`. |
| Bounded | At most 100 spans (`MATCH_SPAN_MAX_COUNT`). |
| Order-independent | Reordering the words of your query returns the identical array — **provided the query stays under the 32-phrase cap** (see below). |
| Covering | If your query has *k* terms (k ≤ 100) and each occurs at least once, **every one of them** is covered by some span. |
| Idempotent | Folding the same rule over the returned array changes nothing. |

Two of these are new in v0.18.0 and worth knowing if you wrote a client against an earlier version. Before it, a query containing both a quoted phrase and a word inside that phrase — `"Foundry Local" Foundry` — returned overlapping spans `(0,7)` and `(0,13)` for the same text, and a highlighter had to decide what that meant. And the 100-span budget was spent in phrase order, so a term matching hundreds of times consumed all of it and the rare term you also asked about was highlighted nowhere. Sharing the budget costs a few spans when the query is wide: with 32 terms the budget is `floor(100/32) = 3` each, so 96 rather than 100. The leftover is deliberately **not** redistributed, because handing it out in term order would make the answer depend on word order again.

Sharing the split with FTS has three visible consequences:

- A region you wrap in `"..."` is **one term**, so it yields **one span** per occurrence: `"Foundry Local"` highlights `Foundry Local` whole and never breaks into a span per word.
- A fragment shorter than 3 characters is not a phrase on its own, so it is not highlighted on its own: in `ML pipelines` only `pipelines` is highlighted. (The whitespace fallback of step 2 is the exception — there no phrase existed to begin with.)
- The phrase list is deduplicated and capped at 32, so a very long query highlights only its first 32 distinct fragments — on top of the 100-span cap of step 5. **This is the one place term order still matters**: the cap keeps the first 32 *in query order*, so reordering a query that exceeds it changes which fragments survive. That is a property of what the full-text search looks for, not of highlighting, so the order-independence guarantee above is scoped to queries below the cap. (The 100-term limit on the whitespace-fallback path does not have this problem — that list is sorted before it is truncated.)

## Non-ASCII queries

`match_spans` is omitted from the JSON output entirely (key not present) when
**any term is non-ASCII**. For a Japanese query that is the normal case, since
the terms are substrings of the query itself.

The test applies to the terms, not to the raw query (v0.16.0+). A non-ASCII
character that acts purely as a **separator** is dropped while splitting, so it
no longer suppresses the spans: `rust、tokio` splits into `rust` and `tokio`,
both ASCII, and both get highlighted. Before v0.16.0 the same query returned
`null`.

This is a deliberate MVP limitation. Substring matching on non-ASCII text would
miss the granularity that the FTS5 trigram tokenizer provides on the search
side, leading to confusing results. A future feature will use FTS5's `snippet()`
function for precise span extraction across all languages.

## Empty results

When `results: []` is returned, `match_spans` simply isn't relevant (there's no
chunk to point into). The `low_confidence` flag should be checked for the
"no relevant content" signal.

## Related

- `docs/filters.md` — narrowing search results
- `README.md` — full search tool reference
