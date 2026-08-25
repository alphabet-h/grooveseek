# AGENTS.md

GrooveSeek is a Rust MCP server for semantic search over a knowledge base of
Markdown, plain text, PDF and Office documents. Documents are chunked by heading
(sheet / slide / page), embedded, and retrieved by fusing sqlite-vec KNN with
FTS5 full-text search through Reciprocal Rank Fusion, optionally reranked by a
cross-encoder. It serves MCP clients over stdio or Streamable HTTP.

The workspace holds the `groove` binary plus two Windows satellites: a tray app
and a service launcher.

- Development guidance in depth (Japanese): `CLAUDE.md`
- Source layout: `docs/ARCHITECTURE.md`
- Why things are the way they are: `docs/decisions/`

## Build and test

```bash
cargo check                                             # type check only
cargo test                                              # skips model downloads
cargo test -- --ignored                                 # runs them (~130 MB / ~2.3 GB)
```

**To reproduce what CI checks, every one of these has to pass** — `cargo clippy
--all-targets` alone is not the whole of it, so it can be clean here while CI
fails. No count is written down on purpose: the list is the list, and a number
beside it is one more thing that can go stale.

`grooveseek/tests/docs_commands_pinned.rs` compares this block with the `run:`
steps of `.github/workflows/ci.yml` -- the same commands, in each job's order,
with the order between jobs free -- so a command added to the block and not
the workflow, or the other way round, fails the suite.

<!-- groove-pin: ci-command-block -->
```bash
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo clippy --all-targets --features test-helpers,heavy-bench -- -D warnings
cargo check --all-targets
cargo test --test index_progress_cli -- --test-threads=1   # first, and single-threaded
cargo test
cargo doc --no-deps --workspace --all-features --document-private-items
```

Clippy runs **twice** because the second pass is the only one that compiles the
code behind `test-helpers` and `heavy-bench` (`.github/workflows/ci.yml`). The
order of the two `cargo test` lines matters on a cold model cache:
`index_progress_cli` spawns `groove` subprocesses that each need BGE-small, and
in parallel they race on the HuggingFace download lock. `cargo doc` is a check
rather than a build step here — `--document-private-items` is what makes it read
this crate's private modules, which is most of it, and `--all-features` is what
makes it read the items `test-helpers` gates. `CONTRIBUTING.md` explains all
three at length.

Windows needs no extra DLLs: ONNX Runtime and SQLite are linked in statically.

## Code Review Rules

These are the things a reviewer here has had to explain more than once. Each
states what must hold and the way to satisfy it.

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

**This is about the words a diagnostic chooses, not the data it names.** A
path, a heading or a query echoed into a message carries whatever characters
the knowledge base uses, and `groove index` prints the relative path of every
file it indexes — measured, a note called `日本語のノート.md` comes out of
`eprintln!` as itself. Escaping those would hand a reader `\u{65e5}\u{672c}`
where they expected a filename, in a project whose recommended model is chosen
for Japanese knowledge bases. What has to stay ASCII is everything the message
contributes itself: no arrows, no em dashes, no emoji, no box drawing.

Where echoing the value is itself the problem — a caller's `Host` header, a
path that may not be valid UTF-8 — the reason is not the encoding, and the fix
is named where that decision was taken (`transport/http.rs`'s refusals,
`service/macos.rs`'s `escape_default`).

The rule is about **what the binary writes**. A failing assertion's message is
printed by `cargo test` to a developer, not by `groove` to an operator, so it
is not covered — but it is read on the same console, so keep it ASCII anyway.
That costs nothing and removes the question.

`grooveseek/tests/diagnostics_stay_ascii.rs` enforces this over every `.rs`
under `grooveseek/src` and `crates/*/src`, so a review does not have to. What
it cannot see is a message assembled from pieces rather than written as a
literal; that still needs reading.

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

### A doc comment that names something in this tree links to it

`cargo doc` runs in CI and `[workspace.lints.rustdoc]` denies a link that no
longer resolves, so a renamed function cannot leave its documentation behind.
That guard sees links and nothing else. A name in plain backticks — `` `foo` ``
— is invisible to rustc and to rustdoc alike, so writing one opts the sentence
out of the check without saying so, and it reads exactly like the checked form.
`transport/http.rs` named a function ADR-0009 had deleted, in backticks, for two
days.

Write `` [`foo`] `` for anything that exists in this workspace: a function, a
type, a constant, a module. In a module's own `//!` header the name has to be
absolute — `` [`crate::links::read_checked`] `` — because a bare name there does
not resolve even to an item in that same file, which is measured and is the
opposite of how `///` on an item behaves. Prose about something *outside* the
tree — an upstream symbol, a TOML key like `` `[eval].golden` ``, an interval
like `` `k in [30,100]` `` — stays in backticks, which is also what keeps
rustdoc from reading the brackets as a link.

One case cannot be written as a link at all: an item **private to another
module**, which Rust does not let you name from here and rustdoc reports as "no
item named". Link the module and leave the item in prose — `` the gate in
[`crate::watcher`] ... `should_process_parts` `` — rather than widening the
item's visibility so a sentence can point at it.
