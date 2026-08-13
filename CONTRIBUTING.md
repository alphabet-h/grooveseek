# Contributing to kb-mcp

Thanks for considering a contribution! This document covers the essentials of working on kb-mcp.

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
cargo build --release      # Release binary at target/release/kb-mcp(.exe)
cargo check --all-targets  # Quick type check
cargo test                 # Unit + integration tests (no model download)
cargo test -p kb-mcp --lib <name>  # One test by name (the workspace has several crates,
                                  # and `--lib` skips the integration-test binaries)
```

To reproduce what CI runs, all of these have to pass — `cargo clippy --all-targets` alone is **not** what CI checks, so it can be clean locally while CI fails:

```bash
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo clippy --all-targets --features test-helpers,heavy-bench -- -D warnings
cargo test --test index_progress_cli -- --test-threads=1   # first, and single-threaded
cargo test
```

The order of the last two matters on a cold model cache, which is why CI runs them that way too. `index_progress_cli` spawns `kb-mcp` subprocesses that each need BGE-small; run in parallel they race on the HuggingFace download lock and fail with "Lock acquisition failed". Running that target single-threaded first lets exactly one process do the download, and the full suite then runs against a warm cache.

> **`cargo test -- --ignored` changes your machine.** Read the next section before running it.

- `cargo fmt --all` before committing (also enforced by the pre-push hook and in CI)
- Japanese comments are welcome for Japanese-KB-specific logic (CJK tokenization, date formats, etc.); English otherwise

## Repository layout

- `kb-mcp/src/parser/` — `Parser` trait + `Registry` (one impl per file format)
- `kb-mcp/src/indexer.rs` — `walkdir` → parse → embed → store pipeline
- `kb-mcp/src/db.rs` + `kb-mcp/src/db/` — SQLite + sqlite-vec + FTS5 storage. Split in v0.15.0 into `schema.rs` (creation + forward migrations), `storage.rs` (CRUD), `search.rs` (vector KNN, FTS candidates, RRF fusion — `search_hybrid`, k=60 by default), `meta.rs` (`index_meta` key/value), and `fts_query.rs` (compiling a query into per-token FTS phrases, v0.16.0+)
- `kb-mcp/src/embedder.rs` — `fastembed-rs` wrapper (embeddings + cross-encoder rerankers)
- `kb-mcp/src/mmr.rs` — MMR diversity re-rank (`mmr_select`, v0.7.0+)
- `kb-mcp/src/parent.rs` — Parent retriever content expansion (`apply_parent_retriever`, v0.7.0+)
- `kb-mcp/src/server.rs` — `rmcp::ServerHandler` with six MCP tools
- `kb-mcp/src/transport/` — stdio and Streamable HTTP transports
- `kb-mcp/src/watcher.rs` — `notify-debouncer-full`-based incremental reindex
- `kb-mcp/src/schema.rs` — frontmatter schema validation
- `kb-mcp/src/quality.rs` / `kb-mcp/src/graph.rs` — quality filter + BFS connection graph
- `kb-mcp/src/eval.rs` — optional retrieval-quality evaluation for `kb-mcp eval`
- `kb-mcp/src/config.rs` — `kb-mcp.toml` 4-tier discovery / merge with CLI overrides
- `kb-mcp/src/markdown.rs` — backward-compatible shim re-exporting `parser::markdown`
- `kb-mcp/src/indexer/progress.rs` — per-file progress output for `kb-mcp index` (`--quiet` / `--progress`)
- `kb-mcp/src/service/` — `kb-mcp service install/uninstall/status` (systemd-user / LaunchAgent / Task Scheduler)
- `kb-mcp/src/tune.rs` + `kb-mcp/src/tune/` — `kb-mcp tune` fusion-parameter sweep: `grid.rs` (sweep grid), `stats.rs` (aggregation), `report.rs` (rendering)
- `kb-mcp/src/test_support.rs` — shared test helpers, notably `unique_temp_path` (this repo deliberately does not use the `tempfile` crate; see the comment there)
- `crates/kb-mcp-tray/` — Windows system-tray monitor (`kb-mcp-tray.exe`, v0.9.0+)
- `crates/kb-mcp-svc/` — Windows hidden-console launcher started by the scheduled task (v0.9.1+)
- `kb-mcp/tests/` — integration tests; `kb-mcp/benches/` — criterion benchmarks

See [docs/ARCHITECTURE.md](./docs/ARCHITECTURE.md) for a detailed walkthrough.

## Test layering

- **Light tests**: default `cargo test`. No network, no model download, runs in seconds. This is the only layer CI runs.
- **Ignored tests** (`#[ignore]`): opt in via `cargo test -- --ignored`. Two different kinds of cost hide behind that one flag:
  - **Model downloads** — ONNX models on first run (BGE-small ~130 MB, BGE-M3 ~2.3 GB, BGE-reranker-v2-m3 ~2.3 GB), cached per OS convention afterwards. See the README's "Working around HuggingFace TLS failures" section if your network blocks the download.
  - **Real changes to your machine** — a few tests register and unregister actual OS services. `kb-mcp/tests/service_install_integration.rs` calls `Register-ScheduledTask` on Windows, and `crates/kb-mcp-tray/tests/install_integration.rs` writes a shortcut into `%APPDATA%\…\Start Menu\Programs\Startup\`. They use a per-PID service name and clean up after themselves, but a killed run can leave a scheduled task or a startup shortcut behind. Check with `Get-ScheduledTask -TaskName 'kb-mcp*'` if a run dies partway.

  Run `cargo test -- --ignored` deliberately, not as a habit. To take only the download cost, target the suite you actually want: `cargo test --test <name> -- --ignored`.

When adding behavior that needs the embedder or reranker, mark the test `#[ignore]` and add a comment explaining what it exercises. When a test touches the OS (services, autostart, the registry), say so in the `#[ignore = "…"]` reason itself so the cost is visible at the call site.

## Submitting changes

1. Fork the repo and branch from `main`
2. Add tests for new behavior (unit tests inline, integration tests under `tests/`)
3. `cargo fmt --all && cargo clippy --all-targets -- -D warnings && cargo test`
4. Open a PR describing the problem and the change; link any related issues

## Reporting bugs

Include:
- A minimal reproduction (commands, small sample KB if relevant)
- `kb-mcp --version`
- Operating system and Rust toolchain version (`rustc --version`)
- Expected vs observed behavior

## License

By contributing, you agree that your contributions are dual-licensed under **MIT OR Apache-2.0**, matching the project. See [LICENSE-MIT](./LICENSE-MIT) and [LICENSE-APACHE](./LICENSE-APACHE).
