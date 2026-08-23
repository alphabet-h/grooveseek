# Contributing to GrooveSeek

Thanks for considering a contribution! This document covers the essentials of working on GrooveSeek. The product is GrooveSeek; the command it installs is `groove` ([ADR-0007](docs/decisions/0007-rename-the-project-to-grooveseek.md)).

> **日本語版**: [CONTRIBUTING.ja.md](./CONTRIBUTING.ja.md)

## Prerequisites

- Rust stable (edition 2024)
- Git
- ~4.7 GB of disk space for ONNX model caches when running ignored tests (BGE-small ~130 MB + BGE-M3 ~2.3 GB + BGE-reranker-v2-m3 ~2.3 GB)

## First-time setup

After cloning, opt in to the repository's git hooks once:

```bash
git config core.hooksPath .githooks
```

This activates `.githooks/pre-push`, which runs `cargo fmt --all -- --check` before every push so a missed `cargo fmt` cannot reach CI. The hook is shared with the rest of the team — see [`.githooks/pre-push`](./.githooks/pre-push). To bypass it in an emergency, append `--no-verify` to the push.

## Build and test

```bash
cargo build --release      # Release binary at target/release/groove(.exe)
cargo check --all-targets  # Quick type check
cargo test                 # Unit + integration tests (no model download)
cargo test -p grooveseek --lib <name>  # One test by name (the workspace has several crates,
                                  # and `--lib` skips the integration-test binaries)
```

**`--lib` also skips `main.rs`.** The binary carries three `#[cfg(test)]`
modules of its own — the CLI surface tests among them — and none of them run
under `--lib`. Reach those with `--bin groove`, and when you are not sure which
target a test lives in, drop the filter and let `cargo test <name>` search all
of them.

To reproduce what CI runs, all of these have to pass — `cargo clippy --all-targets` alone is **not** what CI checks, so it can be clean locally while CI fails:

```bash
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo clippy --all-targets --features test-helpers,heavy-bench -- -D warnings
cargo check --all-targets
cargo test --test index_progress_cli -- --test-threads=1   # first, and single-threaded
cargo test
cargo doc --no-deps --workspace --all-features --document-private-items
```

`cargo doc` is there because a doc comment that names something the tree no
longer has is a defect nothing else catches: a backticked name is invisible to
rustc, and only an intra-doc link — `` [`name`] `` — is checked at all. The lint
levels come from `[workspace.lints.rustdoc]` in the root `Cargo.toml`, so this
command says the same thing locally as it does in CI. Neither flag is optional:
most of this crate is private modules, and without `--document-private-items`
their doc comments are never read; `test-helpers` is default-off and gates
documented items, which rustdoc removes along with the items unless
`--all-features` is set.

The order of the two `cargo test` lines matters on a cold model cache, which is why CI runs them that way too. `index_progress_cli` spawns `groove` subprocesses that each need BGE-small; run in parallel they race on the HuggingFace download lock and fail with "Lock acquisition failed". Running that target single-threaded first lets exactly one process do the download, and the full suite then runs against a warm cache.

> **`cargo test -- --ignored` changes your machine.** Read the next section before running it.

- `cargo fmt --all` before committing (also enforced by the pre-push hook and in CI)
- Japanese comments are welcome for Japanese-KB-specific logic (CJK tokenization, date formats, etc.); English otherwise

## Repository layout

- `grooveseek/src/parser/` — `Parser` trait + `Registry` (one impl per file format)
- `grooveseek/src/indexer.rs` — `walkdir` → parse → embed → store pipeline
- `grooveseek/src/db.rs` + `grooveseek/src/db/` — SQLite + sqlite-vec + FTS5 storage. Split in v0.15.0 into `schema.rs` (creation + forward migrations), `storage.rs` (CRUD), `search.rs` (vector KNN, FTS candidates, RRF fusion — `search_hybrid`, k=60 by default), `meta.rs` (`index_meta` key/value), and `fts_query.rs` (compiling a query into per-token FTS phrases, v0.16.0+)
- `grooveseek/src/embedder.rs` — `fastembed-rs` wrapper (embeddings + cross-encoder rerankers)
- `grooveseek/src/mmr.rs` — MMR diversity re-rank (`mmr_select`, v0.7.0+)
- `grooveseek/src/parent.rs` — Parent retriever content expansion (`apply_parent_retriever`, v0.7.0+)
- `grooveseek/src/server.rs` — `rmcp::ServerHandler` with six MCP tools
- `grooveseek/src/transport/` — stdio and Streamable HTTP transports
- `grooveseek/src/watcher.rs` — `notify-debouncer-full`-based incremental reindex
- `grooveseek/src/schema.rs` — frontmatter schema validation
- `grooveseek/src/quality.rs` / `grooveseek/src/graph.rs` — quality filter + BFS connection graph
- `grooveseek/src/eval.rs` — optional retrieval-quality evaluation for `groove eval`
- `grooveseek/src/config.rs` — `groove.toml` 4-tier discovery / merge with CLI overrides
- `grooveseek/src/markdown.rs` — backward-compatible shim re-exporting `parser::markdown`
- `grooveseek/src/indexer/progress.rs` — per-file progress output for `groove index` (`--quiet` / `--progress`)
- `grooveseek/src/service/` — `groove service install/uninstall/status` (systemd-user / LaunchAgent / Task Scheduler)
- `grooveseek/src/tune.rs` + `grooveseek/src/tune/` — `groove tune` fusion-parameter sweep: `grid.rs` (sweep grid), `stats.rs` (aggregation), `report.rs` (rendering)
- `grooveseek/src/links.rs` — hard-link detection shared by the index / watcher / `get_document` guards (v0.19.0+)
- `grooveseek/src/poison.rs` — recovering from a poisoned mutex instead of inheriting the panic (v0.19.0+)
- `grooveseek/src/test_support.rs` — shared test helpers, notably `unique_temp_path` (this repo deliberately does not use the `tempfile` crate; see the comment there)
- `crates/groove-tray/` — Windows system-tray monitor (`groove-tray.exe`, v0.9.0+)
- `crates/groove-svc/` — Windows hidden-console launcher started by the scheduled task (v0.9.1+)
- `grooveseek/tests/` — integration tests; `grooveseek/benches/` — criterion benchmarks

See [docs/ARCHITECTURE.md](./docs/ARCHITECTURE.md) for a detailed walkthrough.

## Test layering

- **Light tests**: default `cargo test`. No network, no model download, runs in seconds. This is the only *test* layer that gates pull requests — `ci.yml` runs no other tests, only the checks listed above (fmt, clippy, check, and `cargo doc`).
- **Ignored tests** (`#[ignore]`): opt in via `cargo test -- --ignored`. Not PR-gating, but not manual-only either: `nightly.yml` runs `cargo test --features test-helpers -- --include-ignored` daily on both ubuntu-latest and windows-latest, so these do get exercised — a day later, and the Windows leg skips the three tests that need the ~2.3 GB models. Two different kinds of cost hide behind that one flag:
  - **Model downloads** — ONNX models on first run (BGE-small ~130 MB, BGE-M3 ~2.3 GB, BGE-reranker-v2-m3 ~2.3 GB), cached per OS convention afterwards. See "Working around HuggingFace TLS failures" in [docs/clients.md](docs/clients.md) if your network blocks the download.
  - **Real changes to your machine** — a few tests register and unregister actual OS services. `grooveseek/tests/service_install_integration.rs` calls `Register-ScheduledTask` on Windows, and `crates/groove-tray/tests/install_integration.rs` writes a shortcut into `%APPDATA%\…\Start Menu\Programs\Startup\`. They use a per-PID service name and clean up after themselves, but a killed run can leave a scheduled task or a startup shortcut behind. Check with `Get-ScheduledTask -TaskName 'groove*'` if a run dies partway.

  Run `cargo test -- --ignored` deliberately, not as a habit. To take only the download cost, target the suite you actually want: `cargo test --test <name> -- --ignored`.

When adding behavior that needs the embedder or reranker, mark the test `#[ignore]` and add a comment explaining what it exercises. When a test touches the OS (services, autostart, the registry), say so in the `#[ignore = "…"]` reason itself so the cost is visible at the call site.

### Retrieval quality gate

`grooveseek/tests/eval_corpus_quality.rs` is the one test that measures whether a change made search *worse* rather than broken. It indexes `tests/fixtures/kb-eval/` — 20 committed Japanese/English documents — runs the committed golden set in `tests/fixtures/kb-eval-golden.yml` through `groove eval`, and fails when aggregate recall@1 or MRR drops below a measured floor. The BGE-small run is the sensitive one and executes on both nightly legs; the BGE-M3 run guards the Japanese semantic path and is Linux-only.

If you change retrieval — query compilation, fusion, chunking, the parser, MMR — expect this gate to move, and read the failure output before adjusting a threshold: it names every query that lost rank 1, what it expected, and what won instead. Lowering a floor is a decision to accept worse search, so it belongs in the pull request description together with the new measurement. The module docs record the current baseline and how it was taken.

### Coverage floor

`nightly.yml` also measures line coverage with `cargo-llvm-cov`, and fails if **any single file** falls below 35%. That is a tripwire for "arrived with no tests", not a target — it sits far below the ~86% total on purpose, because the total is not a number you can threshold honestly: in-file `#[cfg(test)]` modules push it up, code reachable only from `#[ignore]` tests reads as 0% and pushes it down, and Windows/macOS-only code is not in the Linux leg's denominator at all. Three files are excluded for that middle reason and each is named in the workflow.

So: if you add a module, add tests with it. When the floor trips, the offending file is emitted as a GitHub error annotation with its percentage — cargo-llvm-cov itself only exits 1 — and the full per-file table is in the job summary either way.

## Submitting changes

1. Fork the repo and branch from `main`
2. Add tests for new behavior (unit tests inline, integration tests under `tests/`)
3. Run the checks above — both clippy legs, not just the first:
   ```bash
   cargo fmt --all
   cargo clippy --all-targets -- -D warnings
   cargo clippy --all-targets --features test-helpers,heavy-bench -- -D warnings
   cargo test
   cargo doc --no-deps --workspace --all-features --document-private-items
   ```
4. Open a PR describing the problem and the change; link any related issues

## Reporting bugs

Include:
- A minimal reproduction (commands, small sample KB if relevant)
- `groove --version`
- Operating system and Rust toolchain version (`rustc --version`)
- Expected vs observed behavior

## License

By contributing, you agree that your contributions are dual-licensed under **MIT OR Apache-2.0**, matching the project. See [LICENSE-MIT](./LICENSE-MIT) and [LICENSE-APACHE](./LICENSE-APACHE).
