# AGENTS.md

kb-mcp is a Rust MCP server for semantic search over a knowledge base of
Markdown, plain text, PDF and Office documents. Documents are chunked by heading
(sheet / slide / page), embedded, and retrieved by fusing sqlite-vec KNN with
FTS5 full-text search through Reciprocal Rank Fusion, optionally reranked by a
cross-encoder. It serves MCP clients over stdio or Streamable HTTP.

The workspace holds the `kb-mcp` binary plus two Windows satellites: a tray app
and a service launcher.

- Development guidance in depth (Japanese): `CLAUDE.md`
- Source layout: `docs/ARCHITECTURE.md`
- Why things are the way they are: `docs/decisions/`

## Build and test

```bash
cargo check                                             # type check only
cargo test                                              # skips model downloads
cargo test -- --ignored                                 # runs them (~130 MB / ~2.3 GB)
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all --check
```

Windows needs no extra DLLs: ONNX Runtime and SQLite are linked in statically.

## Code Review Rules

These are the four things a reviewer here has had to explain more than once.
Each states what must hold and the way to satisfy it.

### A field added to a persisted struct needs `#[serde(default)]`

Evaluation runs are written to a history file and read back by later runs,
including runs of a *newer* binary reading a file written by an older one. The
loader treats a deserialization failure as "no history" and warns — so a field
that older files cannot supply does not fail loudly, it **silently discards
every saved baseline**, which is the thing the history exists to protect.

Add `#[serde(default)]` to the field, and cover it with a test that loads a JSON
document written without it and asserts the previous run survived. The same
applies to anything else read back from a file a previous release wrote.

### Results go to stdout, diagnostics to stderr, and stderr stays ASCII

A subcommand's output is its result; progress, warnings and errors are
diagnostics. Only the result belongs on stdout, because that is what a caller
redirects and parses — `serve` over stdio goes further, since stdout carries the
MCP protocol itself and anything else written there corrupts the stream.

Diagnostics additionally have to be ASCII. They are read on a console, and on a
Japanese Windows install that console is CP932, where `→` and emoji arrive as
mojibake. The formatters that build stdout output may use them freely; the
message on its way to stderr may not.

### One question gets one implementation

When two places answer the same question — is this document servable, does this
hit count, what does this text normalize to — they must call one shared
implementation rather than each carry a copy. Two copies agree right up until
someone adds a condition to one of them, and the failure is silent: the surfaces
simply start disagreeing about the same document.

Put the condition in the shared predicate and let both callers reach it. If a
copy already exists, collapsing it is part of the change that adds the condition,
not a follow-up.

### A new column must be written by the paths that skip unchanged work

Indexing answers "unchanged" for a file whose content hash still matches, and
that path deliberately writes no row at all. So a new column populated only from
the row writers is never populated for a knowledge base where nothing changed —
which is exactly the knowledge base a migration is written for. The change looks
correct, passes its tests, and does nothing on the corpus it was meant to fix.

Write the value from the scan that measured it, unconditionally, so the paths
that skip work still record it. Prove it with a test that indexes twice and
asserts the value is present after the second, no-op run.
