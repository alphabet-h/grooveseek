# Architecture

Source-level structure and data flow of kb-mcp, for contributors extending or modifying the codebase.

> **日本語版**: [ARCHITECTURE.ja.md](./ARCHITECTURE.ja.md)

## Source layout

| File | Responsibility |
|---|---|
| `src/lib.rs` | (v0.7.1+) Library crate root. Re-exports the modules below as `kb_mcp::*` so benches under `benches/` and integration tests under `tests/` can drive internal APIs without subprocess. The library surface is intentionally unstable and not intended for external consumers. |
| `src/main.rs` | Binary entry point. clap CLI dispatches `index` / `status` / `serve` / `search` / `graph` / `validate` / `eval` subcommands. Consumes the lib via `use kb_mcp::*;`. Loads `kb-mcp.toml` and merges with CLI args. JSON / text output formatting. |
| `src/config.rs` | 4-tier `kb-mcp.toml` discovery (`--config` flag → CWD → `.git` ancestor (CWD + up to 19 ancestors) → binary-side legacy). `Config::discover()` returns a `ConfigSource` enum that `main.rs` logs at startup. Resolves `CLI > config > default` precedence. Injects `FASTEMBED_CACHE_DIR` env when the config sets it and the env is unset. |
| `src/server.rs` | `rmcp::ServerHandler` impl. Dispatches six MCP tools. `search` routes to `db.search_hybrid` and wraps the result in a `SearchResponse` with `low_confidence` / `match_spans` / `filter_applied` (BREAKING in v0.3.0; see CHANGELOG). |
| `src/service/` | (v0.8.0+) Cross-platform OS user service installer. `mod.rs` (= `ServiceBackend` trait + `InstallContext` + `ServiceState`), `install.rs` / `uninstall.rs` / `status.rs` (= orchestration), `linux.rs` / `macos.rs` / `windows.rs` (= per-OS backends, cfg-gated). Phase 1 = user-level only (= no admin/sudo, Linux systemd-user / macOS LaunchAgent / Windows Task Scheduler AT_LOGON). `kb-mcp service install` self-registers using Rust crates only (= no NSSM / WiX / 3rd-party tooling). |
| `src/indexer.rs` | `walkdir`-based file scan using `Registry::extensions()`. Parses via the Parser trait, embeds, stores. SHA-256 content-hash diff detection. Incremental APIs (`reindex_single_file` / `deindex_single_file` / `rename_single_file`) shared with the file watcher. |
| `src/indexer/progress.rs` | (v0.7.8+) `ProgressReporter` + `ProgressMode` enum. Drives per-file output for `kb-mcp index`: `Verbose` (default) / `Quiet` (`--quiet`) / `Auto` (`--progress`, TTY = `indicatif::ProgressBar`, non-TTY = periodic `Progress: N/M (P%)` lines). MCP server `rebuild_index` tool wires `Quiet` directly. Bar lifetime is closed inside `rebuild_index` (lazy init via `start_indexing(total)`) so `Backfilled` / `Found` lines stay plain `eprintln!`. |
| `src/parser/` | Parser trait + Registry. `mod.rs` (Frontmatter / Chunk / ParsedDocument), `markdown.rs`, `txt.rs`, `registry.rs` (extension lookup). |
| `src/markdown.rs` | Thin shim over `crate::parser::markdown::MarkdownParser`, retained for legacy `parse()` / `parse_with_excludes()` callers. |
| `src/watcher.rs` | `notify-debouncer-full` bridged to a tokio channel. Filters by extension and path, then dispatches to `indexer::{reindex,deindex,rename}_single_file`. Runs alongside the MCP server via `tokio::spawn`. |
| `src/transport/` | MCP transport abstraction. `mod.rs` (Transport enum + CLI/config resolution), `stdio.rs` (stdio), `http.rs` (rmcp `StreamableHttpService` + axum, mounts `/mcp` and `/healthz`). `KbServerShared` is `Arc`-shared through a session factory so each connection gets a lightweight handle. |
| `src/schema.rs` | Frontmatter schema validation. Reads `kb-mcp-schema.toml` under `kb_path`, enforces `required` / `type` / `pattern` / `enum` / `min_length` / `max_length` / `allow_empty`. Invoked by the `kb-mcp validate` CLI which reports in text / JSON / GitHub-annotation formats. |
| `src/embedder.rs` | Thin wrapper over `fastembed-rs`. `ModelChoice` selects the embedding model (BGE-small-en-v1.5 / BGE-M3). `RerankerChoice` + `Reranker` provide optional cross-encoder reranking. |
| `src/db.rs` | `rusqlite` + `sqlite-vec` + FTS5 (trigram). Manages the `chunks` / `vec_chunks` / `fts_chunks` schemas and CRUD. Exposes `search_hybrid` (Reciprocal Rank Fusion, `k = 60`) and the v0.7.0 unbounded variants for the MMR / parent retriever pipeline. `SearchFilters` struct unifies filter args (path globs / tags / date range / min_quality); `MatchSpan` carries byte-offset citations (added in v0.3.0). `chunks.level` (added v0.7.0) distinguishes h2 / h3 headings. |
| `src/mmr.rs` | (v0.7.0+) Maximal Marginal Relevance greedy re-rank with a similarity cache. `mmr_select` operates on the post-rerank candidate pool and is gated by `[search.mmr]` config or the `mmr` per-call param. |
| `src/parent.rs` | (v0.7.0+) Display-time parent retriever. `apply_parent_retriever` expands hit chunks via `expand_adjacent` (level-aware sibling merge) or `expand_whole_document` (full-doc fallback for chunks under `whole_doc_threshold_tokens`). Score / rank / `match_spans` stay on the original hit; only `content` and the new `expanded_from` field change. |
| `src/quality.rs` | Per-chunk quality scoring (length / boilerplate / structure signals). |
| `src/graph.rs` | Connection graph BFS over the vector index, for the `get_connection_graph` MCP tool and the `kb-mcp graph` CLI. |
| `src/eval.rs` | Optional retrieval-quality evaluation for the `kb-mcp eval` CLI. Parses a golden YAML, runs each query through `db.search_hybrid`, and computes recall@k / MRR / nDCG@k. Loads / saves `<kb_path>/.kb-mcp-eval-history.json` for diff display. `ConfigFingerprint` (v0.7.0+) carries optional `mmr` / `parent_retriever` so eval runs with different settings produce distinguishable history entries. Opt-in; does not affect `serve` / `search` / `index`. |

## Data flow

```
.md / .txt files (filtered by Registry::extensions())
     │
     ▼ walkdir
indexer.rs: SHA-256 content-hash diff vs the chunks.hash column
     │
     ▼ changed files only
parser/: dispatch by extension → extract frontmatter + title + chunk
     │
     ▼
embedder.rs: embedding via fastembed
              (BGE-small-en-v1.5 → 384 dim, BGE-M3 → 1024 dim)
     │
     ▼
db.rs: UPSERT into chunks (metadata)
       + vec_chunks (embedding)
       + fts_chunks (FTS5 trigram)
```

At query time the `search` tool runs a hybrid:

- query → embedder → `vec_chunks MATCH` (top-N)
- query → sanitize → `fts_chunks MATCH` + bm25 (top-N) — heading weighted 2×
- Reciprocal Rank Fusion on the Rust side (`k = 60`) → top-`limit` returned
- (optional) cross-encoder reranker re-scores the top candidates before return
- (optional, v0.7.0+) MMR diversity re-rank greedily picks `limit` chunks from the larger candidate pool, balancing relevance and novelty (`lambda` controls the tradeoff; `same_doc_penalty` deduplicates same-document hits)
- (optional, v0.7.0+) parent retriever expands the `content` of short hits to adjacent siblings or the whole document; the score, rank, path, and `match_spans` are preserved so the relevance signal is unchanged

The full v0.7.0 pipeline is **`RRF → reranker → MMR → parent retriever → match_spans`**. Each stage is a no-op when its config is off, so the pipeline collapses to pre-v0.7.0 behavior by default. See [retrieval-pipeline.md](./retrieval-pipeline.md) for the narrative.

## Embedding cache resolution

`embedder.rs::resolve_cache_dir()` picks in order:

1. `FASTEMBED_CACHE_DIR` env var (highest priority)
2. OS-standard cache directory joined with `fastembed`:
   - Linux: `~/.cache/fastembed`
   - macOS: `~/Library/Caches/fastembed`
   - Windows: `%LOCALAPPDATA%\fastembed`
3. `.fastembed_cache/` under CWD (final fallback)

First run downloads the chosen ONNX model to a HuggingFace-hub-compatible cache layout (BGE-small: ~130 MB, BGE-M3: ~2.3 GB, BGE-reranker-v2-m3: ~2.3 GB). Subsequent runs reuse the cache without re-downloading.

If `fastembed-rs`'s native TLS to HuggingFace fails (corporate proxies / TLS inspection), see the README's "Working around HuggingFace TLS failures" section for a `huggingface_hub` CLI workaround.

## CLI output convention

The `kb-mcp` CLI follows a **stdout = data, stderr = progress** convention:

- **stdout** is reserved for machine-parseable data output:
  - `kb-mcp search` JSON results
  - `kb-mcp eval` golden-query evaluation results
- **stderr** carries human-readable progress, status, warnings, and errors:
  - `kb-mcp index` progress lines (`Indexing ...`, `Done in ...`, per-file `  indexed:` / `  renamed:` / `  deleted:`). Use `--quiet` to suppress per-file output (start / found / done summary only) or `--progress` to switch to an `indicatif` bar (TTY) / periodic `Progress: N/M (P%)` lines (non-TTY). The two flags are mutually exclusive and default-off (added v0.7.8).
  - `kb-mcp status` statistics (`Documents: N`, `Chunks: N`)
  - `kb-mcp service install/uninstall/status/list` write all messages to stderr (= status / progress / diagnostics, per convention). stdout is empty.
  - All `tracing` / `eprintln!` diagnostics

When writing subprocess tests, grep `src/main.rs` for the corresponding `Commands::*` block to confirm which channel each subcommand uses before asserting on the captured output. Only `Commands::Search` writes its result to stdout; everything else is stderr-centric.

## Key dependencies

- **`rmcp`** 1.x — MCP server framework (stdio + Streamable HTTP transports)
- **`fastembed`** 5.x — ONNX-based embeddings / rerankers
- **`rusqlite`** 0.39 with `bundled` — statically linked SQLite 3.50+; FTS5 with trigram tokenizer and `contentless_delete = 1` enabled
- **`sqlite-vec`** 0.1 — vector similarity search extension
- **`pulldown-cmark`** 0.13 — Markdown parser
- **`notify`** 8 + **`notify-debouncer-full`** 0.6 — file watcher with debouncing
- **`axum`** 0.8 — HTTP server for the Streamable HTTP transport
- **`dirs`** 6 — OS-standard cache directory resolution
- **`indicatif`** 0.18 — TTY progress bar for `kb-mcp index --progress` (added v0.7.8 / D-10). MSRV 1.70+, ~150 KB binary impact. Auto-detection of stderr TTY uses `std::io::IsTerminal` (Rust 1.70+ stdlib).
- **`wide`** 0.7 — pure-rust SIMD primitives (`f32x8`) used by the MMR cosine kernel (added in v0.7.2 / feature-31)
