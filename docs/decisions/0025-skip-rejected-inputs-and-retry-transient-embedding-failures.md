# 25. Skip inputs the embedding endpoint rejects, and retry the failures that pass

- Status: accepted
- Date: 2026-09-25
- Deciders: project owner
- Applies to: v1.14.0

## Context and problem

With `provider = "openai-compatible"`
([ADR-0022](0022-embedding-provider-boundary.md)), every file that
`rebuild_index` (`grooveseek/src/indexer.rs`) re-embeds is one or more HTTP
requests. Until this record, any failed request stopped the whole run with
`failed to embed chunks for <file>`. The walk visits files in the same order
every time, so the next run stopped at the same file, and the files after it
were never indexed. The steps that follow the loop never ran either: the sweep
that removes rows for deleted files, and the recording of the declared-field
set.

Two kinds of failure did this, and they are different in kind:

- **The endpoint refuses the input.** Servers with a token limit answer a long
  chunk with HTTP 400, 413 or 422. Nothing sent again will change that answer,
  and the rest of the knowledge base is not affected by it.
- **The endpoint fails for a moment.** A rate limit (429), a 5xx from a server
  that is restarting, a timeout, a refused connection. The same request may
  succeed a few seconds later.

The question is how `groove index`, MCP `rebuild_index` and the watcher should
treat each, without hiding a configuration error behind a run that looks
successful.

## Decision drivers

- A problem with one file must not stop the rest of the knowledge base from
  being indexed.
- A configuration error (a wrong model alias, a wrong endpoint) must not be
  skipped silently.
- The index identity must not move: a setting that changes only how requests
  are sent must not force a rebuild.
- The endpoint's response body must not reach an MCP caller.
- The configuration surface that 1.x freezes (key names, types, defaults) stays
  small.

## Options considered

1. **Which failures skip a file.**
   - Skip the file on any failure. Rejected: a 401 or a wrong dimension would
     turn every file into a skip, and the run would report success.
   - **Skip only on a failure caused by the input: HTTP 400, 413 and 422.**
     Taken.
   - Stop on every failure, as before. Rejected: it is the problem above.

2. **How to bound a long input before it is sent.**
   - By tokens. Rejected: groove holds no tokenizer for the model behind an
     alias, and cannot know which one the endpoint uses.
   - By bytes. Rejected: a byte limit cuts inside a multi-byte character and
     means a different amount of text per script.
   - **By characters (Unicode scalar values).** Taken. The unit does not match
     the server's token limit, and an input that is still too long falls to the
     skip above.

3. **What the retry exposes as configuration.**
   - The wait times as keys as well. Rejected: more frozen keys, and nobody has
     asked to tune them.
   - **`max_retries` alone; the waits are constants.** Taken.
   - No key at all. Rejected: a latency-sensitive daemon needs a way to turn
     the retries off (see Consequences).

4. **What a run with rejections reports.**
   - Always exit 0. Rejected: a configuration error that the endpoint answers
     with 400 would look like a clean run with many skips.
   - Exit non-zero on any rejection. Rejected: one oversized chunk in a large,
     healthy knowledge base would fail every run.
   - **Exit non-zero only when the run had a rejection and embedded no file.**
     Taken. That pattern points at `model` / `document_model` or `endpoint`
     rather than at the files.

## Decision

**A rejected input skips its file; a transient failure is retried; a run that
only saw rejections fails after it has finished.**

- **Classification**, per request (`grooveseek/src/embedder.rs`):

  | What happened | Treatment |
  |---|---|
  | HTTP 400, 413, 422 | `EmbedInputRejected`: not retried. The indexer skips the file |
  | HTTP 429, 500-599 | Retried |
  | A timeout, or a failed connection | Retried |
  | Any other status (401, 403, 404, 408, ...) | The run stops at once |
  | A 2xx whose body is not valid JSON, or whose vectors have the wrong count, index or dimension | The run stops at once |
  | Any other transport error (for example a connection the server drops after accepting it) | The run stops at once |

- **Skipping**: the indexer prints
  `warning: <file>: embedding endpoint rejected the input (HTTP <status>); skipped, the index keeps what it had for this file`
  (the status only, never the response body), counts the file under
  `skipped`, keeps the row it already had, and goes on. When one file needs
  several batches and a later one is rejected, the vectors of the earlier ones
  are discarded and nothing of the file is written.
- **`[embedding] max_input_chars`**, openai-compatible only, default 8000,
  `0` refused. Each input is cut to that many characters before it is sent,
  documents and queries alike. The chunk text in the database and the
  full-text index is not cut.
- **`[embedding] max_retries`**, openai-compatible only, default 3, accepted
  from 0 to 10 (`MAX_EMBEDDING_RETRIES`). One batch is the unit that is sent
  again; a later batch failing does not resend an earlier one. `0` sends once
  and returns the first error unchanged. After the last attempt the error names
  how many attempts were made.
- **The wait before a retry**: what `Retry-After` asks (seconds or an HTTP
  date), exactly and without jitter, when it is 60 s or less. A `Retry-After`
  over 60 s is not waited for: the batch fails at once, saying so. Without a
  usable `Retry-After`, 1 s, 2 s, 4 s ... doubling and capped at 60 s, plus a
  jitter below a quarter of that wait.
- **Neither key is part of the index identity**, alongside `endpoint`,
  `api_key`, `request_dimensions` and `timeout_seconds`. FastEmbed refuses both,
  like the other endpoint keys.
- **The forced-rebuild probe**
  ([ADR-0024](0024-probe-the-endpoint-before-a-forced-rebuild.md)) goes through
  the same request code, so it is retried and cut in the same way. A rejection
  of the probe is **not** skipped: the probe text is fixed, short ASCII, so a
  400, 413 or 422 there points at the configuration, and the rebuild stops
  before the reset as ADR-0024 intends.
- **A run with rejections and nothing embedded fails after it has finished**
  (`IndexResult::fails_all_inputs_rejected`). The run is not cut short: the
  deletion sweep and the bookkeeping complete, and `rebuild_index` still
  returns its counts. Then `groove index` exits non-zero and MCP
  `rebuild_index` returns an `error` beside the counts, both with the message
  from `IndexResult::all_inputs_rejected_message`. The watcher reindexes one
  file at a time and does not apply this rule; it reports a rejected file as
  `watcher: skipped <file> (embedding endpoint rejected the input)`.

## Consequences

- **Exit 0 no longer means every file was indexed.** A skipped file is named
  by its warning and counted in `skipped` (the `Done in` line, the MCP stats).
- **The end of a cut chunk does not count in vector search.** The full-text
  half of the search still holds the whole chunk.
- **Changing `max_input_chars` does not re-embed anything.** An index built
  before this record keeps the vectors of chunks longer than 8000 characters
  until they change or `groove index --force` is run.
- **A daemon holds the embedder lock while it waits.** MCP `rebuild_index`,
  the watcher and a search all hold the embedder for the length of their
  requests, and a retry waits inside that hold, so searches wait with it.
  The worst case for one batch with the defaults is about 420 s (4 attempts of
  the 60 s timeout, and 3 waits of up to 60 s), and about 1260 s with
  `max_retries = 10`; both are estimates from the settings, not measurements.
  A daemon that must answer quickly should set `max_retries = 0`.
- **ADR-0024's probe is still one probe.** It sends the same request again
  only when that request failed in a way that is retried; its Consequences
  entry about a rate limit hit halfway through a rebuild is softened, not
  removed.
- **An incremental run that changed one file, which the endpoint then refused,
  exits non-zero**, because no other file was embedded in that run. This is
  the rule as decided: the run cannot tell a lone oversized file from a
  configuration error.
- **A configuration error can still pass unnoticed** if the endpoint refuses
  only some inputs and another file is embedded in the same run.
- **Japanese text can exceed a token limit within 8000 characters**, since a
  character can take more than one token. Those files are skipped with a
  warning; lowering `max_input_chars` brings them back.
- **A Markdown file refused on every run holds back a changed schema.** After
  the keys `groove-schema.toml` declares change, a refused file counts as not
  read, so the new declared-field set is not recorded and `fields` /
  `fields_not` filters are refused until the file is fixed, `max_input_chars`
  is lowered, or `groove index --force` is run.
- **Tests hold it**, in `grooveseek/tests/openai_compatible_failures.rs`
  (skipping on 400 / 413 / 422, the probe rejection, retries, `Retry-After`,
  truncation of documents and queries, the run that only saw rejections, over
  the CLI and MCP) and in the unit tests of `grooveseek/src/embedder.rs`,
  `grooveseek/src/config.rs` and `grooveseek/src/indexer.rs`.

## References

- [ADR-0022](0022-embedding-provider-boundary.md), whose list of settings kept
  out of the index identity these two keys join, for the same reason: they
  change how a request is sent, not which model answers it.
- [ADR-0024](0024-probe-the-endpoint-before-a-forced-rebuild.md), whose probe
  is now retried like any other request.
- [usage.md](../usage.md#external-openai-compatible-embeddings) for what an
  operator sees when the endpoint refuses or fails.
- `grooveseek/src/embedder.rs` (`EmbedInputRejected`,
  `OpenAiCompatibleConfig::with_limits`), `grooveseek/src/indexer.rs`
  (`IndexResult::fails_all_inputs_rejected`),
  `grooveseek/tests/openai_compatible_failures.rs`.
- Japanese version:
  [0025-skip-rejected-inputs-and-retry-transient-embedding-failures.ja.md](0025-skip-rejected-inputs-and-retry-transient-embedding-failures.ja.md)
