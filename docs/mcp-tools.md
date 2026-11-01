# MCP tools, prompts, and resources

The MCP surface GrooveSeek exposes to a connected client.

> **日本語版**: [mcp-tools.ja.md](./mcp-tools.ja.md)

## Tools

| Tool | Description | Key parameters |
|---|---|---|
| `search` | Hybrid search (vector + FTS5 full-text) merged via Reciprocal Rank Fusion, optionally followed by cross-encoder reranking, optional MMR diversity re-rank, and optional parent retriever content expansion. Returns a wrapper `{ results, low_confidence, filter_applied }` with chunks ranked by relevance; each result may carry `expanded_from` if parent retriever fired, and — when the chunk came from a source file (v1.2.0+) — `start_line` / `end_line` (1-based, describing the chunk) and `symbol_kind` (the grammar's word for the definition). Those three keys are absent, not `null`, on anything else. A successful reply also carries `writer_notice` (v1.22.0+) while the server is not keeping the index current; see "The writer notice" below. See [docs/citations.md](citations.md), [docs/filters.md](filters.md), [docs/retrieval-pipeline.md](retrieval-pipeline.md). | `query` (required; a whitespace-delimited group led by `-`, e.g. `-async`, excludes it from both halves of the search instead of searching for it — v1.1.0+, see [retrieval-pipeline.md](retrieval-pipeline.md) — and the same three-character trigram floor applies, so `-ab` excludes nothing), `limit`, `category`, `topic`, `rerank` (override server default), `min_quality`, `include_low_quality`, `path_globs` (glob list, `!`-prefix excludes), `tags_any` / `tags_all`, `date_from` / `date_to` (`YYYY-MM-DD`), `fields` / `fields_not` (object: key → list of strings, only keys the schema declared, v1.9.0+), `min_confidence_ratio`, `mmr` / `mmr_lambda` / `mmr_same_doc_penalty` (v0.7.0+), `parent_retriever` (v0.7.0+) |
| `list_topics` | List all indexed topics and categories with document counts. Call it to learn which `category` and `topic` values the `search` filters accept. (v1.1.0+) Each entry also carries `children`: the directory tree beneath that category/topic, one node per path segment below the topic, each with `segment`, `file_count` (documents under that prefix, so a parent counts everything beneath it) and its own `children`; root / category-only entries, and topics whose documents sit directly in the topic directory, return `[]`. The tree comes from the stored path, so a document whose frontmatter `topic:` overrides the path-derived topic still contributes the directories after its second path segment — and because those segments come from that document's own path, they need not exist as directories under `<category>/<topic>/`, so a client must not build a path prefix out of them. See [MCP surface stability](stability.md#mcp-surface). | (none) |
| `get_document` | Get the full content and metadata of a document by its relative path. The path has to be the document's **one spelling** — relative to the knowledge base, `/`-separated, exactly as `search` returns it. Any other route to the same file (`./a.md`, `a//b.md`, `a/../a.md`, an absolute path, a different case on Windows or macOS, a backslash or an 8.3 short name on Windows, a symlinked directory) is answered "not found", so a gateway that filters on the requested string cannot be walked around. A path that is not a name the index can hold — absolute, empty, holding a `..`, `.` or empty segment (`./a.md`, `a//b.md`), and on Windows a drive, a UNC share, a backslash, a colon anywhere (an alternate data stream such as `note.md:secret`), a reserved device name such as `NUL` or `COM1`, a segment ending in a dot or a space, or one of `< > " \| ? *` or a control character — is refused before anything on disk is looked at: the answer is the same whether or not something is there, and on Windows no share is contacted. Another spelling that is a name the index could hold (a different case, an 8.3 short name, a symlinked directory) is looked at first, inside the knowledge base only, and then answered "not found" the same way. | `path` (e.g. `"deep-dive/mcp/overview.md"`) |
| `get_best_practice` | Opt-in: when `[best_practice].path_templates` is configured in `groove.toml`, fetch a best-practices document for the given target and optionally extract an h2 section. Without configuration the tool returns a "not configured" error. | `target` (e.g. `"claude-code"`), `category` (optional) |
| `rebuild_index` | Rebuild the search index by scanning all source files (Markdown plus any other extensions enabled via `[parsers].enabled`). **One at a time** (v1.0.0+): a call arriving while a rebuild is running is refused with an error naming how long the running one has been going, because a rebuild holds the embedder and the database and search is unavailable until it finishes. That bound is inside this server. Between processes the writer lock applies (v1.22.0+): a call is **refused before the embedder or the database is touched, and without waiting for them or for a running rebuild**, when another groove process holds the index's lock (a `groove index` run, or another server that is writing the same index), naming that process; and on a server that started as a reader, every call is refused until that server is restarted. A server started with `--no-watch` takes the lock only for the length of each call, so a `groove index` beside it exits 3 while a rebuild runs. Every reply, refusals and errors included, carries `writer_notice` when there is one (see "The writer notice" below). Returns the same counters as the CLI summary, including `frontmatter_unparsed` (v1.7.0+) and `chunks_embedded` / `chunks_reused` (v1.21.0+; the chunks sent to the embedding model, and those that kept their stored vector because their embedding input was unchanged; present in the replies that carry an `error` too) and `kept_changed` / `renames_skipped` (v1.22.0+; the rows the prune kept, and the detected renames it skipped, because a writer that does not take the lock changed them during the run, 0 when none; they are in the reply of a run that finished and then got an `error` from strict frontmatter (`fail_on_frontmatter_error`) or from refused embeddings, but not in a `Rebuild failed` reply, where the run itself failed) and `skipped_files` / `skipped_files_omitted` (v1.24.0+; the files counted in `skipped`, each with the reason it was not indexed, in the same replies as `kept_changed`; see "Skipped files" below); under `[index].fail_on_frontmatter_error = true` the reply carries an `error` key beside those counters when it is non-zero. With an OpenAI-compatible embedding provider, a file whose input the endpoint refuses (HTTP 400 / 413 / 422) is skipped and counted in `skipped`; a run that had such a refusal before any batch of the file was accepted, and in which the endpoint accepted no batch at all, carries an `error` key beside the counters, and so does a `force: true` run with any such refusal, since the rebuild emptied the index first and the file is not in it ([ADR-0025](decisions/0025-skip-rejected-inputs-and-retry-transient-embedding-failures.md)). With `force: true` and an OpenAI-compatible embedding provider, one fixed probe text is embedded before the index is emptied; if the endpoint refuses it the reply is an error saying nothing was removed from the index ([ADR-0024](decisions/0024-probe-the-endpoint-before-a-forced-rebuild.md)). | `force` (optional, default false) |
| `get_connection_graph` | BFS-expand semantically related chunks starting from a document path. Use it after `search` when the answer may sit in documents the query's wording did not reach: the walk follows embedding similarity from the start document, not the query. Returns a flat list of nodes with `parent_id` / `depth` / `score` / `snippet` so the caller can chain context discovery, plus `truncated` / `truncation[]` when a bound cut the walk short. A `snippet` is an excerpt; call `get_document` for a node's full text. | `start` (required, the path to walk from — `groove graph --start`), `depth` (default 2, max 3), `fan_out` (default 5, max 20), `min_similarity` (default 0.3), `seed_strategy` (`all_chunks` / `centroid`; `all-chunks` is accepted too), `dedup_by_path`, `category`, `topic`, `exclude_paths`, `max_nodes` (default 100, max 2000), `max_seed_chunks` (default 32, max 1000) |

### The writer notice

(v1.22.0+) Only one groove process writes an index at a time ([ADR-0031](decisions/0031-one-writer-per-index-by-an-os-lock.md)). When this server is not keeping the index current, the `search` and `rebuild_index` replies carry an optional string field, `writer_notice`, that says why. It is one of three texts:

- **This server is a reader.** It started while another groove process held the index's lock (or while the lock could not be taken), so it answers searches but does not update the index, and it refuses `rebuild_index`. It stays a reader until it is restarted, even after the other process exits; files added, edited or deleted meanwhile are not reflected, so a deleted file keeps showing up in search. The text is there on every reply until the restart.
- **The watcher cannot register a watch** on the knowledge base (its directory is missing, or on Linux `fs.inotify.max_user_watches` is exhausted). The text is there until the watch succeeds.
- **The watcher has stopped** outside shutdown. The text is there until the server is restarted.

The tool descriptions name `writer_notice` and ask the client to pass it on to the user. On `search` it rides on a successful reply only, not on an `error` reply, and `groove search --format json` never carries it; it is advisory and outside the frozen search fields ([stability.md](stability.md#what-a-search-answers)). No other tool carries it.

A reader's text names the process that held the lock when it started, and what `.groove.db.writer.json` says now. It says that process keeps this knowledge base current only when it is a server watching the same knowledge base, its heartbeat in that file is no older than 15 seconds, and it reports that its watcher is watching. With an older or missing heartbeat the text says that process has probably exited (a killed process leaves the file behind) and changes are not reflected until this server is restarted; when it reports that it is not watching, the text says changes are not indexed while that lasts.

### Skipped files

(v1.24.0+) The `rebuild_index` reply lists the files the run counted in `skipped` — present on disk but not indexed — each with the reason ([ADR-0032](decisions/0032-report-why-each-file-was-not-indexed.md)). Two top-level keys carry it:

- **`skipped_files`**: the first 50 entries of the library's `IndexResult::skipped_files`, in path order (the server does not re-sort or regroup them), one entry per path. <!-- via: grooveseek/src/server.rs `const SKIPPED_FILES_IN_REPLY: usize = 50` --> Each entry is an object with `path` (relative to the knowledge base, forward slashes — for a name the index cannot hold, its lossy spelling), `reason`, and the detail keys of that reason from the table below. A key the reason does not have is absent, not `null`.
- **`skipped_files_omitted`**: how many more entries there were beyond those listed. `skipped_files` plus `skipped_files_omitted` is the number of entries in the library's list, which is `skipped`.

Both keys are present even when nothing was skipped (an empty array and 0). They are in the reply of a run that finished, including one that carries an `error` (strict frontmatter, refused embeddings), and not in a `Rebuild failed` reply or a refusal, where there is no run.

```json
{
  "skipped": 3,
  "skipped_files": [
    { "path": "docs/catalog.pdf", "reason": "too_large", "size": 73400320, "limit": 52428800, "limit_key": "max_binary_file_size" },
    { "path": "notes/old.txt", "reason": "not_utf8" }
  ],
  "skipped_files_omitted": 1
}
```

(The values above show the shape; the reply carries the other counters too.)

| `reason` | The file | Detail keys |
|---|---|---|
| `too_large` | is over the file size cap | `size`, `limit` (bytes), `limit_key`: `max_binary_file_size` (pdf / docx / xlsx / pptx) or `max_text_file_size` (md / txt / source code) |
| `over_extraction_limit` | passed the decompression / extracted-text budget | `limit` (bytes), `limit_key`: always `max_decompressed_size` |
| `over_parser_limit` | is over a fixed limit of the parser itself (source code over the code parser's byte cap) | `size`, `limit` (bytes); no `limit_key`, because no setting raises it |
| `extraction_timed_out` | took longer to extract than the time limit | `seconds` |
| `encrypted` | is password-protected (PDF, or an Office file in an encrypted container) | — |
| `corrupt_or_unsupported` | is not a readable document of its format (corrupt, truncated, a legacy format under an OOXML extension, a parser failure) | — |
| `not_utf8` | is a text-format file whose bytes are not UTF-8 (e.g. Shift_JIS) | — |
| `text_undecodable` | is a PDF whose text layer decoded to mojibake | — |
| `too_little_text` | is a PDF with too little text per page (a scan with no text layer, a sparse deck) | — |
| `no_text` | was read and parsed and holds nothing to embed (empty, frontmatter only) | — |
| `unreadable` | could not be stat'ed or read | `cause`: `in_use` (another process holds it open, Windows), `permission_denied`, `gone` (removed after the walk) or `other` |
| `embed_rejected` | was refused by the embedding endpoint | `http_status` (400 / 413 / 422) |
| `changed_during_read` | changed between the scan and the read; the next run reads it again | — |
| `not_plain_file` | was not a regular file when read (a symlink, device or pipe swapped in after the walk) | — |
| `hard_link` | has more than one name (a hard link) | — |
| `unspellable_name` | has a name `get_document` and a `kb://` URI cannot take ([ADR-0023](decisions/0023-index-only-names-the-server-can-open.md)) | `directory`: `true` when a whole directory was left out, `false` for a file (counted once, whether or not it held a file the index would have read) |

`limit_key` names a key under `[index]` in `groove.toml` that would admit the file if raised. A file that is on disk but never considered (`.grooveignore`, `exclude_dirs`, unsupported extensions, Office lock files `~$*`, symlinks seen by the walk) has no entry.

**The `reason` vocabulary.** New values may be added. When that happens, a file that was reported with a generic value (`corrupt_or_unsupported` / `no_text`) may move to a more specific value (a newly added one, or one that already exists). Treat a value you do not know as a generic failure. Moving between two specific values, moving from a specific value to a generic one, or removing or renaming a value, would be a breaking change.

## Prompts

(v0.22.0+) Four prompts ship with the server. A client surfaces them as commands the **user** picks — Claude Code renders them as `/mcp__<server>__<name>`, where `<server>` is the key you gave this server in your `.mcp.json`, not a name GrooveSeek chooses. Each one exists because the tools alone do not say how to combine them: `get_connection_graph` says it can follow a `search`, but no tool says how far to expand, when to search again with the vocabulary that turns up, or that a `low_confidence` flag means the answer should say so.

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

`resources/list` returns the topic groups, **not one entry per document**. A knowledge base has hundreds of documents but tens of groups, and a listing is something the client fetches on every connect. Individual documents stay reachable through the template and through the `uri` field that now appears on `search` hits — the specification permits handing back links to documents a listing never enumerated. Both the listing and that field come from one predicate, so they always agree about a document. Three things take a document off offer while leaving it indexed and findable (on Windows a fourth, a name Windows cannot open as written; see [ADR-0023](decisions/0023-index-only-names-the-server-can-open.md)). The first is the active parser registry: if you narrow `[parsers].enabled` without reindexing, the rows for the dropped extensions stay in the index and stay in search results, but they are no longer offered, because a read would refuse them. The second is size (v0.23.0+): a Markdown or text document over 1 MiB is more than `resources/read` will return, so it is not offered either — its `search` hit stays, without a `uri`. A PDF or spreadsheet over that size is still offered, because a read truncates its extracted text rather than refusing it. A binary document indexed past 50 MiB — possible only when `[index].max_binary_file_size` raises the index cap — is not offered, because a read refuses a binary file over 50 MiB whatever the index settings ([ADR-0026](decisions/0026-configurable-index-size-caps.md)). The third is the decompression cap: a binary document is offered only while `[index].max_decompressed_size` is at or below the built-in 50 MiB decompression budget: above it, binary hits carry no `uri` and `resources/list` and `resources/read` do not offer them, because a read parses with the built-in budget and the recorded raw size cannot say which documents it would accept. Text documents are not affected. Sizes are recorded during indexing; documents indexed by an earlier version have no size recorded and stay on offer until the next `groove index`, which fills them in without re-embedding. `groove doctor` names the documents held back for each of the three reasons (`extension-not-registered`, `larger-than-a-read-returns`, `binary-uris-withheld`) and counts the ones whose size is not recorded yet. Reasoning: [ADR-0005](decisions/0005-record-document-size-in-the-index.md).

Separators stay as forward slashes; everything else is percent-encoded, so a path with spaces or non-ASCII characters produces a valid ASCII URI.

**A read is bounded by the index.** A document is served only if it is indexed, and then only through the same checks `get_document` applies — symlink and hard-link refusal, path traversal, extension membership, size cap, and a handle-bound read. This is *narrower* than `get_document`, which serves anything under `kb_path` with a registered extension: a resource is something the server offered, so serving a URI that was never on offer is a different operation. `.grooveignore`d documents are therefore absent from resources while remaining readable through `get_document`, which is [ADR-0003](decisions/0003-kb-mcpignore-bounds-indexing-not-access.md)'s contract unchanged. The reasoning is in [ADR-0004](decisions/0004-resource-reads-are-bounded-by-the-index.md).

Content comes back as text with the media type of what is served: `text/markdown` for Markdown, `text/plain` for anything delivered as extracted text. A PDF or a spreadsheet is served as the text groove extracted from it, not as the original bytes.

Not implemented: `resources/subscribe` and `notifications/resources/list_changed`. `"resources": {}` is a conforming declaration without them, and a fixed set of topic groups rarely changes.

## Related

- `docs/citations.md` — `match_spans` and byte offsets
- `docs/filters.md` — narrowing search results
- `docs/clients.md` — connecting a client in the first place
