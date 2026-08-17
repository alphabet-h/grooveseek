# Search filters

The `search` MCP tool accepts several filters to narrow results. Filters compose
with **AND** semantics — all conditions must match for a chunk to appear in
`results`.

> **日本語版**: [filters.ja.md](./filters.ja.md)

## Quick reference

| Param | Type | Example | Effect |
|---|---|---|---|
| `category` | string | `"deep-dive"` | Match `documents.category` exactly |
| `topic` | string | `"mcp"` | Match `documents.topic` exactly |
| `path_globs` | string[] | `["docs/**", "!docs/draft/**"]` | Glob include / exclude |
| `tags_any` | string[] | `["rust", "wasm"]` | OR — any tag must match |
| `tags_all` | string[] | `["draft"]` | AND — all tags must match |
| `date_from` | string | `"2026-01-01"` | hit.date >= from (lex compare) |
| `date_to` | string | `"2026-12-31"` | hit.date <= to (lex compare) |
| `min_quality` | number | `0.5` | Per-call override of the quality-filter threshold (`[quality_filter].threshold`) |
| `include_low_quality` | bool | `true` | Disable the quality filter for this call (equivalent to `min_quality: 0.0`, but explicit) |
| `min_confidence_ratio` | number | `1.5` | Threshold for `low_confidence` flag |

## `path_globs`

- Patterns prefixed with `!` are exclusion patterns.
- Without `!`, patterns are inclusion patterns.
- A path passes if it matches **any include** AND **no exclude**.
- All-`!` arrays are valid: missing include is interpreted as "include all".
- An **empty array `[]` is rejected** with an error. Use `null` (omit the key)
  to disable the filter, or `["**", "!a/**"]` for exclude-only.

```jsonc
{
  "path_globs": ["docs/**", "!docs/draft/**"]
  // matches "docs/a.md", excludes "docs/draft/b.md", excludes "notes/c.md"
}
```

## `tags_any` and `tags_all`

These match against `documents.tags` (the YAML frontmatter `tags:` array).

- **`tags_any`** = OR: hit must contain at least one of the listed tags
- **`tags_all`** = AND: hit must contain every listed tag
- When both are set: `(all of tags_all) AND (any of tags_any)` must match

```jsonc
{
  "tags_all": ["rust"],
  "tags_any": ["async", "concurrency"]
  // matches docs tagged with "rust" AND (one of "async" or "concurrency")
}
```

## `date_from` / `date_to`

- Use **`YYYY-MM-DD`** (recommended) or RFC 3339 timestamps.
- Compared lexicographically (string `<` / `<=`), so consistent format is required.
- **Strict semantics**: chunks whose `documents.date` is `NULL` are excluded
  whenever `date_from` or `date_to` is set.

```jsonc
{
  "date_from": "2026-01-01",
  "date_to":   "2026-04-30"
}
```

> **Mixing date formats** (e.g., `"2026-04-26 12:00:00 +0900"` vs
> `"2026-04-26T12:00:00+09:00"`) breaks lex ordering. Choose one format per KB.

## `low_confidence` and `min_confidence_ratio`

The response wrapper includes a top-level `low_confidence` boolean. It's `true`
when the top hit's score is **not noticeably better** than the rest of the
result set:

```
low_confidence ⇔ (results.len() >= 2)
                 AND (mean(scores) > 0.0)
                 AND (max(scores) / mean(scores) < min_confidence_ratio)
```

- The numerator is `max(scores)`, **not** `results[0].score`. They differ whenever the returned order is not score-descending — which is exactly what MMR does, since it re-orders for diversity.
- Default `min_confidence_ratio = 1.5` (the best score must be at least 1.5× the mean)
- Set to `0.0` to disable the judgment entirely
- Override per-call via the `min_confidence_ratio` param, or globally via
  `groove.toml`:

  ```toml
  [search]
  min_confidence_ratio = 1.5
  ```

`low_confidence: true` means "the matches are flat — Claude should be cautious
about citing them as authoritative." The actual `results` are still returned;
the flag is purely advisory.

## `category` vs `tags_any`: different filter axes

These are **different fields** in the index:

- **`category`** filters `documents.category`, a single string column populated
  from the `category:` frontmatter field (or auto-derived path segment).
- **`tags_any` / `tags_all`** filter `documents.tags`, a JSON array of tag
  strings populated from the `tags:` frontmatter list.

A document with `category: "deep-dive"` and `tags: ["mcp", "rust"]` matches
`category: "deep-dive"` but **does not** match `tags_any: ["deep-dive"]` —
they're separate axes.

## Combining filters

Filters compose with **AND**:

```jsonc
{
  "path_globs": ["docs/**"],
  "tags_all":   ["rust"],
  "date_from":  "2026-01-01"
  // = under docs/, tagged "rust", from 2026 onward
}
```

## Related

- `docs/citations.md` — match_spans / byte offsets
- `docs/mcp-tools.md` — full search tool reference
