# 22. Put embedding behind a provider boundary, and keep FastEmbed the default

- Status: accepted
- Date: 2026-09-23
- Deciders: project owner
- Applies to: v1.13.0

## Context and problem

Until v1.12.0 embedding inference was the concrete FastEmbed wrapper. Commands,
configuration and the index compatibility check all spoke in terms of
`ModelChoice`, the enum of the two local BGE models GrooveSeek can download and
run in-process. Nothing else could produce a vector, and every place that
decided whether an index was usable decided it by asking that enum.

Operators asked to point GrooveSeek at an embedding service they already run —
a local inference server, or a hosted one — without GrooveSeek learning each
vendor (issue #312). Most such services speak the same
`POST /v1/embeddings` request shape that OpenAI published, so one client covers
many of them.

Two things make this more than adding a variant. An external endpoint receives
**every indexed chunk and every query**: the KB's text leaves the process, and
possibly the machine. And the index records which model produced its vectors
and refuses to open under a different one, so whatever identifies "the model"
now has to cover a service GrooveSeek does not control.

The question this record answers is **where the seam between GrooveSeek and an
embedding implementation sits, what identifies an index built through it, and
who may turn outbound embedding on.**

## Decision drivers

- The default must not move. A FastEmbed index built by v1.12.0 has to open
  unchanged, and an installation that says nothing about providers must send
  nothing anywhere.
- Retrieval models are often asymmetric: a query and a document are embedded
  differently, sometimes by different model aliases. The seam has to keep the
  two apart so the indexer and search pipeline never learn which provider does
  what.
- Whether an index can be opened must be decidable without the network.
  Startup and config validation that depend on a remote answer are not
  deterministic, and they send traffic before the operator's command has done
  anything.
- Sending the knowledge base's text out of the machine is an operator decision.
  A file that merely happens to sit in a directory must not be able to make it.
- Vendor-specific behaviour — task prefixes, sidecar lifecycles, remote
  reranking — belongs to the vendors. One generic protocol is enough to carry,
  and the surface frozen by [docs/stability.md](../stability.md) grows with
  every key added.

## Options considered

1. **A small private provider trait with separate document and query calls;
   FastEmbed as its default implementation; a vendor-neutral OpenAI-compatible
   HTTP client as the second.** Taken.

2. **Keep the concrete FastEmbed type and add external models as variants of
   `ModelChoice`.** Rejected. Command and config plumbing and the compatibility
   check already depended on `ModelChoice` (PR #314's reason for the refactor),
   and a remote model is not something that enum can describe: it has no fixed
   dimension, no cache directory and no download. Every new variant would widen
   one vendor's model list into the place that decides whether an index opens.

3. **Bundle or manage a sidecar, or depend on a vendor SDK** (a Python server,
   MLX, Jina's client). Rejected; these were issue #312's non-goals. GrooveSeek
   would own another process's lifecycle — start, health, restart, upgrade —
   and would grow vendor-specific request fields and task conventions. The
   operator already runs the service; GrooveSeek only needs to reach it.

4. **Probe the endpoint to discover the model and its dimension.** Rejected.
   Opening an index would then depend on a network round trip: startup and
   `Config::validate` would stop being deterministic, a service that is down
   would look like a configuration error, and document text or a probe request
   would leave the machine before the index had even been opened. The operator
   declares `dimension` instead, and GrooveSeek checks every response against
   it.

   The condition that would reopen it: a protocol that returns the model's
   identity and dimension from a metadata call carrying no user text, supported
   widely enough to be relied on.

5. **Honour `[embedding]` from any config GrooveSeek finds.** Rejected. A
   config discovered in the current directory or its git root may have been put
   there by whoever wrote that repository, archive or shared drive. Honouring it
   would let a file found on disk send indexed document text and every query to
   an address it names. This is the reason R5 exists for `[parsers]`
   ([ADR-0016](0016-keep-the-plugin-directory-outside-the-knowledge-base.md)),
   and the same boundary is applied here as R7.

6. **Fold the endpoint into the identity.** Rejected. A change of hostname,
   port or scheme, or a load-balanced pair of servers, would then force a full
   reindex although the vectors are unchanged. And the endpoint string still
   would not identify the model actually deployed behind it: a redeploy at the
   same URL is invisible either way. It buys a rebuild on every move and no
   guarantee in return.

   The condition that would reopen it: a server-side identifier of the
   embedding space, such as a model fingerprint in the response, that
   GrooveSeek could record instead of the URL.

## Decision

**Embedding inference sits behind a private `EmbeddingProvider` trait. FastEmbed
stays the default implementation; the second is a vendor-neutral client for
`POST /v1/embeddings`.**

- **The trait keeps documents and queries apart.** It has two methods,
  `embed_documents` and `embed_query` (`EmbeddingProvider` in
  `grooveseek/src/embedder.rs`). `Embedder` is the public entry point, holds a
  `Box<dyn EmbeddingProvider>`, and delegates `embed_texts` and `embed_single`
  to them. The trait is not public; nothing outside the crate implements it.
- **FastEmbed is one implementation, not a special case**
  (`FastEmbedProvider` in `grooveseek/src/embedder.rs`). Its identity is still
  the bare model id and dimension (`bge-small-en-v1.5` / 384, `bge-m3` / 1024),
  so a v1.12.0 index opens unchanged; the test
  `resolved_fastembed_settings_accept_an_existing_index` pins that.
- **The HTTP implementation is one protocol, not one vendor**
  (`OpenAiCompatibleProvider` in `grooveseek/src/embedder.rs`). Documents are
  sent under `document_model` and queries under `query_model`, in batches of at
  most 64 (`OPENAI_COMPATIBLE_BATCH_SIZE`). `dimensions` is sent only when
  `request_dimensions = true`, because not every server accepts it. Redirects
  are not followed.
- **An index is identified by what GrooveSeek can know about its vectors.** The
  identity is the provider kind, the document alias, the query alias and the
  declared dimension, spelled
  `openai-compatible:{document_model}|{query_model}:{12 hex}` where the hex is a
  SHA-256 over the two aliases, each length-prefixed, and the dimension
  (`OpenAiCompatibleConfig::new` in `grooveseek/src/embedder.rs`).
  `endpoint`, `api_key` and `timeout_seconds` are deliberately left out, so an
  operator can move the service — host, port, TLS, key rotation — without a
  rebuild. The cost is stated plainly: GrooveSeek cannot detect that the same
  aliases at a different endpoint, or at the same endpoint after a redeploy,
  now produce a different embedding space. When that happens the operator has
  to run `groove index --force` themselves, and nothing warns them.
- **GrooveSeek never probes the endpoint.** `dimension` is mandatory for the
  external provider. The identity is resolved into `EmbeddingSettings` before
  any provider exists, and `verify_embedding_meta`
  (`grooveseek/src/db/meta.rs`) runs on those settings before
  `Embedder::with_settings` constructs one. Every response is then checked:
  the vector count must match the inputs, each `index` must be in range and
  unique, none may be missing, and each vector must have the declared
  dimension.
- **Outbound embedding needs a trusted config (R7).** `[embedding]` is honoured
  from a config the operator named with `--config`, one installed beside the
  binary, or one under a trusted root (`classify_trust` in
  `grooveseek/src/config.rs`). A config GrooveSeek discovered in the current
  directory or its git root has the whole section dropped, with a warning that
  says what the section can send and how to accept it (`restrict_untrusted` in
  `grooveseek/src/config.rs`). That is the boundary `[parsers]` already has.
- **`--model` keeps its historical meaning.** It selects FastEmbed for that
  invocation and overrides `[embedding]` (`Config::resolve_embedding` in
  `grooveseek/src/config.rs`).
- **Two pull requests, boundary first.** PR #314 introduced the trait, the
  settings type and the compatibility plumbing with no change in behaviour, so
  existing indexes were proved compatible before anything new could be
  configured. PR #316 added the HTTP provider, its contract tests and the
  English docs.

## Consequences

- **The Linux and macOS default build now always links reqwest 0.13's blocking
  client and a second rustls crypto backend.** `reqwest` 0.13 with the
  `blocking`, `json` and `rustls` features is an unconditional dependency
  (`grooveseek/Cargo.toml`), and its `rustls` feature brings in the aws-lc-rs /
  aws-lc-sys backend. No cargo feature removes it, although only an opt-in
  provider uses it. A blocking HTTP client and two TLS stacks are not new:
  v1.12.0 already carried ureq, reqwest 0.12 and native-tls through hf-hub, and
  rustls with the ring backend through fastembed, hf-hub and ureq. What changes
  is that two rustls crypto providers, ring and aws-lc-rs, now coexist in one
  binary. The picture of the earlier build, including that aws-lc-sys was absent
  from it, is read off the old lockfile, where only the Windows-only tray used
  reqwest 0.13; **this was not measured**. The current state is measured before
  each release with
  `cargo tree -p grooveseek -i aws-lc-sys --target aarch64-unknown-linux-gnu`.
- **Nine new keys are frozen from the release that ships them.** `[embedding]`
  carries `provider`, `endpoint`, `model`, `query_model`, `document_model`,
  `dimension`, `request_dimensions`, `api_key` and `timeout_seconds`, with the
  defaults `provider = "fastembed"`, `request_dimensions = false` and
  `timeout_seconds = 60`. The
  [Configuration](../stability.md#configuration) promise freezes key names,
  types and defaults, so these cannot be renamed or re-defaulted in a minor
  release. [The default embedding model](../stability.md#the-default-embedding-model)
  promise covers the provider too: making anything other than FastEmbed the
  default is a major change.
- **A version floor.** v1.12.0 and earlier reject an unknown key, so a
  `groove.toml` carrying `[embedding]` stops those releases from starting at
  all. This is the same floor
  [ADR-0021](0021-take-the-socket-you-were-given.md) put in front of
  `systemd_socket`: upgrade GrooveSeek first, then add the section.
- **The identity string is internal but not free to change.** It lives in
  `index_meta`, whose schema is not a contract, yet changing its shape — the
  prefix, the separator, the hash input or its length — makes every index built
  through an external provider refuse to open until it is rebuilt. The mismatch
  message is chosen by the current runtime identity, not by the index: it
  points at `groove --config <cfg> index --kb-path <path> --force` when the
  current configuration selects an external provider, and at
  `--force --model <id>` when it selects FastEmbed
  (`verify_embedding_meta` in `grooveseek/src/db/meta.rs`).
- **The blocking client cannot live on a tokio worker.** Building it there
  panics with `Cannot drop a runtime in a context where blocking is not
  allowed`, and the client is built lazily on the first embed call. The search
  handlers already ran on the blocking pool. The watcher did not, so it now
  moves each event batch to `spawn_blocking` — per batch, not around the whole
  receive loop, so shutdown still responds (`run_watch_loop` in
  `grooveseek/src/watcher.rs`). **What the tests hold is narrower than this.**
  `openai_compatible_embeds_on_first_call_inside_a_tokio_runtime` pins that the
  provider works when called through `spawn_blocking` inside a runtime; **that
  the watcher's path actually goes through `spawn_blocking` is held by review
  alone.**
- **The new Rust types are outside the compatibility promise.**
  `EmbeddingSettings`, `Embedder::with_settings` and `OpenAiCompatibleConfig`
  are public because the binary is built from them, and
  [The Rust library API](../stability.md#the-rust-library-api) section, backed
  by [ADR-0008](0008-declare-what-1-0-freezes.md), leaves them free to change.
- **The operator now carries a data-flow decision.** Whatever answers at
  `endpoint` receives every chunk indexed and every query searched, in plain
  text over whatever transport the URL names. GrooveSeek does not check where
  that is or who runs it. It refuses URLs that embed credentials, keeps
  `api_key` out of its `Debug` output, strips the URL from transport errors, and
  caps and escapes an error body to 512 bytes, so the refusal and warning text
  it prints stays ASCII and does not echo a secret back.
- **Which model answers at the endpoint is the operator's to keep stable.**
  GrooveSeek records the alias, not the model behind it. Swapping the model
  behind an alias leaves the index opening as before, and from then on search
  compares the old document vectors with query vectors from a different
  embedding space. Nothing reports it; the two spaces stay mixed until the
  operator rebuilds the index.

## References

- Issue #312 (<https://github.com/alphabet-h/grooveseek/issues/312>) for the
  request, the owner's scope and the non-goals; PR #314
  (<https://github.com/alphabet-h/grooveseek/pull/314>) for the boundary; PR
  #316 (<https://github.com/alphabet-h/grooveseek/pull/316>) for the HTTP
  provider.
- [ADR-0008](0008-declare-what-1-0-freezes.md) for why the Rust API is outside
  the stability promise.
- [ADR-0013](0013-compile-in-one-grammar-and-load-the-rest.md) for the earlier
  weighing of a dependency against binary size.
- [ADR-0016](0016-keep-the-plugin-directory-outside-the-knowledge-base.md) for
  the untrusted-config boundary R7 reuses.
- [ADR-0021](0021-take-the-socket-you-were-given.md) for the same version floor
  on a new configuration key.
- [usage.md](../usage.md#external-openai-compatible-embeddings) for how to
  configure an external provider.
- `grooveseek/src/embedder.rs` (`EmbeddingProvider`, `EmbeddingSettings`,
  `FastEmbedProvider`, `OpenAiCompatibleProvider`, `OpenAiCompatibleConfig`),
  `grooveseek/src/config.rs` (`EmbeddingConfig`, `restrict_untrusted`,
  `classify_trust`, `resolve_embedding`), `grooveseek/src/db/meta.rs`
  (`verify_embedding_meta`), `grooveseek/src/watcher.rs` (`run_watch_loop`).
- Japanese version:
  [0022-embedding-provider-boundary.ja.md](0022-embedding-provider-boundary.ja.md)
