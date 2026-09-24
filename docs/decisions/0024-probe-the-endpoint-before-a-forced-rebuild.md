# 24. Probe the embedding endpoint before a forced rebuild empties the index

- Status: accepted
- Date: 2026-09-24
- Deciders: project owner
- Applies to: v1.14.0

## Context and problem

A forced rebuild — `groove index --force`, and the MCP tool `rebuild_index`
with `force: true` — starts by emptying the index (`reset_for_model` in
`grooveseek/src/db/meta.rs`, reached through `reset_and_resolve_context_mode`
in `grooveseek/src/indexer.rs`). The reset runs in a transaction of its own and
commits. Documents are embedded afterwards, one file at a time.

With FastEmbed that order costs nothing: the model is loaded before the reset
and runs in the process. With `provider = "openai-compatible"`
([ADR-0022](0022-embedding-provider-boundary.md)) the provider is built without
contacting the endpoint, and the first request goes out only after the reset
has committed. A wrong or revoked `api_key` (401), an endpoint that is down, a
rate limit (429), or a server now answering with vectors of another length all
surface on that first request — after the index is already empty. The command
fails, and what it leaves behind is an index with no documents in it.

The MCP path makes this worse. `rebuild_index` is reachable by anything that
can reach the HTTP port, and GrooveSeek has no authentication by design, so a
caller can empty the served index again and again while the endpoint refuses.

ADR-0022 decided that **GrooveSeek never probes the endpoint**. Its reasons were
about opening an index: startup and `Config::validate` would stop being
deterministic, a service that is down would look like a configuration error,
and a request would leave the machine before the index had been opened. The
question here is narrower: **may a command whose purpose is to send the
knowledge base to the endpoint first check that the endpoint will answer,
before it destroys the index it is about to replace?**

## Decision drivers

- A command that fails must not leave the operator worse off than before they
  ran it, when the failure is detectable before anything was changed.
- Opening, validating and serving an index stay deterministic and offline, for
  ADR-0022's reasons.
- Nothing from the knowledge base leaves the machine on a check.
- One seam for both callers: the CLI and MCP must not diverge on whether they
  are protected.
- The response a check accepts must be one indexing would accept, or the check
  passes and the rebuild fails anyway.

## Options considered

1. **Before the reset, embed one fixed text through the document side, and only
   in a forced rebuild.** Taken.

2. **Build the new index beside the old one and swap it in on success.**
   Rejected for now. A rebuild commits file by file, so a run that stops partway
   keeps what it finished; a whole-run transaction would hold the write lock for
   the length of the rebuild, blocking the watcher, and a shadow database
   doubles the disk while a `vec_chunks` of a new dimension is built. It is the
   stronger guarantee — it also covers an endpoint that fails halfway through —
   and a much larger change than this problem needs.

   The condition that would reopen it: a rebuild that is moved to
   build-then-swap for another reason. The probe then has nothing left to
   protect and can be removed.

3. **Delay the reset until the first real batch has been embedded.** Rejected.
   The reset precedes several writes that assume an empty index (the context
   mode, the code chunk budget and policy, the declared-field set), and the
   first embed sits inside the per-file loop, after files have been read and
   chunked. Moving the destructive step into that loop spreads it across the
   code the reset exists to keep simple, to save one small request.

4. **Probe everywhere the provider is built** — `serve` startup, `validate`,
   every `index`. Rejected, for the reasons ADR-0022 gave; nothing here changes
   them. A non-forced `index` destroys nothing, so a failure there costs no
   more than the run itself.

5. **Leave it, and document that `--force` needs a working endpoint.** Rejected.
   The loss is silent until the next search returns nothing, and over MCP it can
   be repeated by any caller.

## Decision

**A forced rebuild sends one probe to the embedding provider before it resets
the index. Nothing else probes.**

- **Where**: at the top of `rebuild_index` (`grooveseek/src/indexer.rs`), when
  `force` is set, after the knowledge base path is resolved and before anything
  is written to the database. Both `groove index --force` and MCP
  `rebuild_index {force: true}` reach the reset through this function, so each
  sends one probe. The CLI's own reset ahead of `rebuild_index`, which predated
  the reset inside it, is removed; it would otherwise have emptied the index
  before the probe ran.
- **What**: `Embedder::probe_before_reset` (`grooveseek/src/embedder.rs`). For
  the OpenAI-compatible provider it embeds `ENDPOINT_PROBE_TEXT`, a fixed ASCII
  string, as a document — under `document_model`, the side the rebuild is about
  to use — and runs the answer through the same checks every indexing response
  gets: HTTP status, vector count, index range and uniqueness, and the declared
  `dimension`. FastEmbed does nothing; its model is already loaded.
- **On failure** the command stops with an error that says the index was not
  modified, followed by the provider's own error (for example
  `embedding endpoint returned HTTP 401: ...`). Over MCP that is the tool's
  error reply.
- **Unchanged**: opening an index, `Config::validate`, `serve` startup, an
  incremental `index`, `search` and the watcher never probe. ADR-0022's rule
  stands for all of them; this record narrows it by the one exception above.

## Consequences

- **A forced rebuild costs one more request.** One input of a few tokens,
  billed like any other by a hosted service.
- **The probe proves the endpoint answered once, not that the rebuild will
  finish.** An endpoint that starts refusing after the probe — a rate limit hit
  halfway, an outage mid-run — still stops the rebuild with part of the index
  written. Option 2 is what would close that; until then the remedy is to run
  the forced rebuild again.
- **A database `--force` could not open is replaced before the probe.**
  `groove index --force` swaps out a file that is not a usable SQLite database
  (`open_or_replace_corrupt`, in `main.rs`'s `Commands::Index` arm) before
  `rebuild_index` runs. If the probe then fails, the message still says the
  index was not modified, but there was no usable index to keep.
- **Text leaves the machine one request earlier than before.** It is the fixed
  string, not knowledge-base content, and it goes only where the operator has
  already chosen to send every chunk by running a forced rebuild.
- **The error is the one indexing would give**, because the probe goes through
  the same code. A server that accepts one input and refuses a batch of 64 is
  not caught; the probe does not try to be a load test.
- **Tests hold it**, in `grooveseek/tests/openai_compatible_provider.rs`:
  `index_force_against_a_401_endpoint_leaves_the_existing_index_intact`,
  `index_force_against_a_failing_endpoint_leaves_the_existing_index_intact`
  (a 429 and a wrong dimension),
  `mcp_rebuild_index_force_against_a_401_endpoint_leaves_the_existing_index_intact`,
  `index_force_probes_the_endpoint_exactly_once_before_resetting` and
  `incremental_index_does_not_probe`.

## References

- [ADR-0022](0022-embedding-provider-boundary.md), whose "never probes" rule
  this narrows; its status says so.
- [usage.md](../usage.md#external-openai-compatible-embeddings) for when the
  external provider is sent requests.
- `grooveseek/src/embedder.rs` (`ENDPOINT_PROBE_TEXT`,
  `Embedder::probe_before_reset`), `grooveseek/src/indexer.rs`
  (`rebuild_index`, `reset_and_resolve_context_mode`).
- Japanese version:
  [0024-probe-the-endpoint-before-a-forced-rebuild.ja.md](0024-probe-the-endpoint-before-a-forced-rebuild.ja.md)
