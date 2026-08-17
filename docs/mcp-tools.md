# MCP tools, prompts, and resources

The MCP surface GrooveSeek exposes to a connected client.

> **日本語版**: [mcp-tools.ja.md](./mcp-tools.ja.md)

## Tools

| Tool | Description | Key parameters |
|---|---|---|
| `search` | Hybrid search (vector + FTS5 full-text) merged via Reciprocal Rank Fusion, optionally followed by cross-encoder reranking, optional MMR diversity re-rank, and optional parent retriever content expansion. Returns a wrapper `{ results, low_confidence, filter_applied }` with chunks ranked by relevance; each result may carry `expanded_from` if parent retriever fired. See [docs/citations.md](citations.md), [docs/filters.md](filters.md), [docs/retrieval-pipeline.md](retrieval-pipeline.md). | `query` (required), `limit`, `category`, `topic`, `rerank` (override server default), `min_quality`, `include_low_quality`, `path_globs` (glob list, `!`-prefix excludes), `tags_any` / `tags_all`, `date_from` / `date_to` (`YYYY-MM-DD`), `min_confidence_ratio`, `mmr` / `mmr_lambda` / `mmr_same_doc_penalty` (v0.7.0+), `parent_retriever` (v0.7.0+) |
| `list_topics` | List all indexed topics and categories with document counts. | (none) |
| `get_document` | Get the full content and metadata of a document by its relative path. | `path` (e.g. `"deep-dive/mcp/overview.md"`) |
| `get_best_practice` | Opt-in: when `[best_practice].path_templates` is configured in `groove.toml`, fetch a best-practices document for the given target and optionally extract an h2 section. Without configuration the tool returns a "not configured" error. | `target` (e.g. `"claude-code"`), `category` (optional) |
| `rebuild_index` | Rebuild the search index by scanning all source files (Markdown plus any other extensions enabled via `[parsers].enabled`). | `force` (optional, default false) |
| `get_connection_graph` | BFS-expand semantically related chunks starting from a document path. Returns a flat list of nodes with `parent_id` / `depth` / `score` / `snippet` so the caller can chain context discovery, plus `truncated` / `truncation[]` when a bound cut the walk short. | `path` (required), `depth` (default 2, max 3), `fan_out` (default 5, max 20), `min_similarity` (default 0.3), `seed_strategy` (`all_chunks` / `centroid`), `dedup_by_path`, `category`, `topic`, `exclude_paths`, `max_nodes` (default 100, max 2000), `max_seed_chunks` (default 32, max 1000) |

## Prompts

(v0.22.0+) Four prompts ship with the server. A client surfaces them as commands the **user** picks — Claude Code renders them as `/mcp__groove__<name>` — and each one exists because the tools alone do not say how to combine them: `search` does not tell a caller to follow it with `get_connection_graph`, or that a `low_confidence` flag means the answer should say so.

| Prompt | Arguments | What it asks for |
|---|---|---|
| `summarize_topic` | `topic` (required) | Confirm the topic exists with `list_topics`, gather it with `search`, read the documents that carry weight with `get_document`, then summarize — including what the knowledge base does *not* cover. |
| `deep_dive` | `question` (required) | Do not answer from the first search: expand the strongest hits with `get_connection_graph` at depth 2, read whole documents, and search again with the vocabulary that turns up. |
| `whats_new` | `since` (optional `YYYY-MM-DD`; defaults to 30 days ago) | Survey documents dated since then. The prompt says outright that `date_from` filters the frontmatter `date` — what an author typed — not when a file changed, so this is an approximation and should be presented as one. It also warns that `date_from` is compared as a plain string: a value that is not `YYYY-MM-DD` filters out every document rather than erroring. |
| `find_gaps` | `topic` (optional) | Look for what is missing: questions that come back with `low_confidence`, and stubs that only appear with `include_low_quality: true`. Reports absences, does not propose content. |

All four are plain text and share one set of citation rules: cite the `path` of every document used, surface `low_confidence` rather than answering through it, and say when the knowledge base is silent instead of filling the gap from general knowledge.

They are fixed at compile time rather than configurable. Prompt text goes to the model, and `groove.toml` is *discovered* — from the working directory or a `.git` ancestor — so a configurable prompt set would need the same restriction that already applies to `kb_path` under an untrusted config. The MCP specification offers no help here: unlike tool annotations, it gives clients no guidance to distrust prompt content.

## Resources

(v0.22.0+) The knowledge base is also exposed as MCP resources under the `kb://` scheme. In Claude Code these appear in the `@` menu.

| URI | What it is |
|---|---|
| `kb://topic/<prefix>` | A **topic group** — the first one or two path segments, the same derivation the indexer uses for `category` and `topic`. Reading one returns a Markdown list of the documents under it, with their URIs. `kb://topic/` is the root group. |
| `kb://doc/<path>` | One indexed document, by its knowledge-base-relative path. Advertised as a template rather than enumerated. |

`resources/list` returns the topic groups, **not one entry per document**. A knowledge base has hundreds of documents but tens of groups, and a listing is something the client fetches on every connect. Individual documents stay reachable through the template and through the `uri` field that now appears on `search` hits — the specification permits handing back links to documents a listing never enumerated. Both the listing and that field come from one predicate, so they always agree about a document. Two things take a document off offer while leaving it indexed and findable. The first is the active parser registry: if you narrow `[parsers].enabled` without reindexing, the rows for the dropped extensions stay in the index and stay in search results, but they are no longer offered, because a read would refuse them. The second is size (v0.23.0+): a Markdown or text document over 1 MiB is more than `resources/read` will return, so it is not offered either — its `search` hit stays, without a `uri`. A PDF or spreadsheet over that size is still offered, because a read truncates its extracted text rather than refusing it. Sizes are recorded during indexing; documents indexed by an earlier version have no size recorded and stay on offer until the next `groove index`, which fills them in without re-embedding. `groove doctor` reports both counts. Reasoning: [ADR-0005](decisions/0005-record-document-size-in-the-index.md).

Separators stay as forward slashes; everything else is percent-encoded, so a path with spaces or non-ASCII characters produces a valid ASCII URI.

**A read is bounded by the index.** A document is served only if it is indexed, and then only through the same checks `get_document` applies — symlink and hard-link refusal, path traversal, extension membership, size cap, and a handle-bound read. This is *narrower* than `get_document`, which serves anything under `kb_path` with a registered extension: a resource is something the server offered, so serving a URI that was never on offer is a different operation. `.grooveignore`d documents are therefore absent from resources while remaining readable through `get_document`, which is [ADR-0003](decisions/0003-kb-mcpignore-bounds-indexing-not-access.md)'s contract unchanged. The reasoning is in [ADR-0004](decisions/0004-resource-reads-are-bounded-by-the-index.md).

Content comes back as text with the media type of what is served: `text/markdown` for Markdown, `text/plain` for anything delivered as extracted text. A PDF or a spreadsheet is served as the text groove extracted from it, not as the original bytes.

Not implemented: `resources/subscribe` and `notifications/resources/list_changed`. `"resources": {}` is a conforming declaration without them, and a fixed set of topic groups rarely changes.

## Related

- `docs/citations.md` — `match_spans` and byte offsets
- `docs/filters.md` — narrowing search results
- `docs/clients.md` — connecting a client in the first place
