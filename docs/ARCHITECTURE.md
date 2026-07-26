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
| `src/service/` | (v0.8.0+) Cross-platform OS user service installer. `mod.rs` (= `ServiceBackend` trait + `InstallContext` + `ServiceState`), `install.rs` / `uninstall.rs` / `status.rs` (= orchestration), `linux.rs` / `macos.rs` / `windows.rs` (= per-OS backends, cfg-gated). Phase 1 = user-level only (= no admin/sudo, Linux systemd-user / macOS LaunchAgent / Windows Task Scheduler AT_LOGON). `kb-mcp service install` self-registers using Rust crates only (= no NSSM / WiX / 3rd-party tooling). The Windows backend (v0.8.3+) invokes PowerShell's `Register-ScheduledTask -Action -Trigger -Settings` cmdlet via `Command::new("powershell")` — `schtasks /Create /XML` was abandoned across v0.8.0 → v0.8.3 due to layered locale / elevation / Principal issues documented in `.dev/knowledge/windows-task-scheduler-pitfalls.md`. |
| `src/indexer.rs` | `walkdir`-based file scan using `Registry::extensions()`. All read paths (initial scan, `reindex_single_file`, `rename_single_file`) go through `fs::read` (bytes, not `read_to_string`) and hash the raw bytes with SHA-256 for content-hash diff detection — for existing UTF-8 KBs this is a no-op vs. the old string hash. Parses via the Parser trait (`parse_bytes`), embeds, stores. Per-file skip isolation: a `read` failure, a size-cap breach, or a `parse_bytes` error skips just that file (logged as a warning) instead of aborting the whole run, and the skipped path is retained in the index rather than pruned as deleted. Incremental APIs (`reindex_single_file` / `deindex_single_file` / `rename_single_file`) shared with the file watcher. |
| `src/indexer/progress.rs` | (v0.7.8+) `ProgressReporter` + `ProgressMode` enum. Drives per-file output for `kb-mcp index`: `Verbose` (default) / `Quiet` (`--quiet`) / `Auto` (`--progress`, TTY = `indicatif::ProgressBar`, non-TTY = periodic `Progress: N/M (P%)` lines). MCP server `rebuild_index` tool wires `Quiet` directly. Bar lifetime is closed inside `rebuild_index` (lazy init via `start_indexing(total)`) so `Backfilled` / `Found` lines stay plain `eprintln!`. |
| `src/parser/` | Parser trait + Registry. `mod.rs` (Frontmatter / Chunk / ParsedDocument, plus the `parse_bytes(bytes, path_hint, exclude_headings) -> Result<ParsedDocument>` entry point that every call site — indexer and server — now goes through. `parse_bytes` is **not** the override point: it delegates to `parse_bytes_inner` (the same signature) inside a `catch_unwind`, so a panic anywhere in a parser or its dependencies becomes a per-file `Err` instead of aborting the whole `index` run. `parse_bytes_inner`'s default impl validates UTF-8 then delegates to `parse`, so `md`/`txt` need no override, while binary-format parsers override it directly. `is_binary()` (default `false`) flags binary parsers for `get_document`'s size-cap classification and quality-filter exemption. `MAX_RAW_BINARY_BYTES` = 50 MiB, the shared raw-byte cap for binary formats used by both the indexer's size-skip guard and `get_document`), `markdown.rs`, `txt.rs`, `pdf.rs` (v0.10.0+, see below), `ooxml.rs` / `xlsx.rs` / `docx.rs` / `pptx.rs` (v0.11.0+, see below), `panic_guard.rs` (see below), `registry.rs` (extension lookup, `binary_extensions()`). |
| `src/parser/panic_guard.rs` | The panic-isolation machinery behind `Parser::parse_bytes`: `catch_parser_panic` runs the parser under `catch_unwind` and turns a panic into `Err("<path>: <id> parser panicked: <payload>")`, keeping the payload so the indexer's skip line still says what happened. A wrapper panic hook is installed **once** (never swapped) and consults a thread-local flag set by an RAII guard, so only the parsing thread's own backtrace is suppressed while panics on other threads keep reporting — a hook that is swapped per call races when two threads parse at once. This started as PDF-only (v0.10.0) and moved here so `docx`/`xlsx`/`pptx` are covered too; without it a single crafted spreadsheet aborts `kb-mcp index` outright (calamine's `get_dimension` subtracts unchecked, so `ref="B2:A1"` panics in any build with debug assertions). |
| `src/parser/pdf.rs` | (v0.10.0+) `PdfParser`, `is_binary() == true`, opt-in via `[parsers].enabled = ["md", "pdf"]`. Extracts text page-by-page with [oxidize-pdf](https://crates.io/crates/oxidize-pdf) (`PdfReader` + `PdfDocument::extract_text`), one non-empty page per chunk (heading `p.N`, `level: None`). `Title` / `CreationDate` PDF metadata become frontmatter, falling back to a filename-derived title when `Title` is absent. A malformed PDF that panics inside the crate's parser internals degrades to a per-file `Err` (indexer skip-and-warn) instead of aborting the whole `index` run — that `catch_unwind` used to live here, and now sits in `Parser::parse_bytes` / `parser/panic_guard.rs` where every parser gets it. Scanned/image-only PDFs (no text layer) are detected via an average-chars-per-page heuristic (< 50 chars/page) and rejected; encrypted PDFs surface as an `Err` from `PdfReader::new` / `extract_text` (oxidize-pdf's `ParseResult`-based error design, no password support). Post-processing joins conservative line-end hyphenation (`-\n` only when both neighbors are ASCII lowercase) and normalizes common ligatures (ﬁ/ﬂ/ﬀ/ﬃ/ﬄ). |
| `src/parser/ooxml.rs` | (v0.11.0+) Shared OOXML zip/XML helper consumed by `xlsx.rs` / `docx.rs` / `pptx.rs` (no parser struct of its own). `read_zip_entry` reads one zip part as raw bytes; `core_xml_frontmatter` / `parse_core_xml` map `docProps/core.xml` (Dublin Core: `dc:title` / `dcterms:created` or `modified` / `cp:keywords`) to `Frontmatter`, falling back to a filename-derived title when the part is missing or `title` is empty; `local_name_pub` strips a namespace prefix from a QName (`cp:title` → `title`) so element matching is prefix-agnostic; `resolve_general_ref` resolves quick-xml 0.38+'s `Event::GeneralRef` (entity references such as `&amp;` arrive as a separate event, not folded into `Event::Text`) — factored out here because docx.rs and pptx.rs both need the same char-ref/named-entity handling. |
| `src/parser/xlsx.rs` | (v0.11.0+) `XlsxParser` (`.xlsx`) and `XlsParser` (`.xls`), both `is_binary() == true`, sharing one `parse_workbook_bytes` implementation. Opens the workbook via `calamine::open_workbook_auto_from_rs` (auto-detects OOXML vs. legacy BIFF), emitting one chunk per non-empty sheet (heading `Sheet: <name>`, tab-joined cell text per row), truncated at `SHEET_MAX_BYTES` (1 MiB) with row-aligned truncation semantics (the row that pushes the running total past the cap is still emitted whole, then extraction for that sheet stops — never cuts mid-row). Frontmatter comes from `docProps/core.xml` via `ooxml::core_xml_frontmatter` when the bytes open as a zip (true for `.xlsx`); `.xls` is pre-OOXML BIFF and never has that part, so it always falls back to a filename-derived title. |
| `src/parser/docx.rs` | (v0.11.0+) `DocxParser`, `is_binary() == true`. Reads `word/document.xml` paragraph-by-paragraph (`<w:p>`), treating a `<w:pStyle w:val="HeadingN">` as a section boundary — the same heading-hierarchy chunking rule `markdown.rs` uses for Markdown headings, including `exclude_headings` support (body under an excluded heading is dropped until the next non-excluded heading). Table (`<w:tbl>`) text needs no special-casing: the OOXML nesting `w:tbl > w:tr > w:tc > w:p > w:r > w:t` already funnels table cell text through the ordinary `<w:p>` boundary handling into the current section's body. Frontmatter via `ooxml::core_xml_frontmatter`. |
| `src/parser/pptx.rs` | (v0.11.0+) `PptxParser`, `is_binary() == true`. Collects `ppt/slides/slideN.xml` entries and sorts by the numeric slide index (not zip iteration order), emitting one chunk per slide (heading `Slide N: <title>` when a `ctrTitle`/`title` placeholder shape has text, else bare `Slide N`), with in-slide table text included in the body. Speaker notes are appended as a trailing `[notes]` section, resolved by reading the slide's `ppt/slides/_rels/slideN.xml.rels` for a `notesSlide` relationship `Target` — deliberately not a same-numbered-file heuristic (`slideN.xml` ↔ `notesSlideN.xml`), which a dry-run (plan Task 3.7) showed can misattribute notes to the wrong slide when slide/notes numbering diverges after edits. Frontmatter via `ooxml::core_xml_frontmatter`. |
| `src/markdown.rs` | Thin shim over `crate::parser::markdown::MarkdownParser`, retained for legacy `parse()` / `parse_with_excludes()` callers. |
| `src/watcher.rs` | `notify-debouncer-full` bridged to a tokio channel. Filters by extension and path, then dispatches to `indexer::{reindex,deindex,rename}_single_file`. Runs alongside the MCP server via `tokio::spawn`. |
| `src/transport/` | MCP transport abstraction. `mod.rs` (Transport enum + CLI/config resolution), `stdio.rs` (stdio), `http.rs` (rmcp `StreamableHttpService` + axum, mounts `/mcp` and `/healthz`; v0.8.0+ also mounts an admin sub-router with `/ui` + `/api/admin/status` + `/api/search` gated by `admin_host_check` middleware = exact-match Host header against loopback aliases + bind addr). `KbServerShared` is `Arc`-shared through a session factory so each connection gets a lightweight handle. |
| `src/transport/webui_index.html` | (v0.8.0+) WebUI MVP placeholder HTML, embedded via `include_str!` in `transport/http.rs::ui_index`. Raw HTML + JS, no CSS framework, XSS-safe via `textContent` / `createElement` only (= no `innerHTML`). Phase 3+ で本格 redesign 前提の disposable placeholder。 |
| `crates/kb-mcp-tray/` | (v0.9.0+) Windows-only system tray binary (`kb-mcp-tray.exe`, GUI subsystem) for daemon monitoring + lifecycle control. Polls `/api/admin/status` every 5s and renders a 4-state status dot (green / yellow indexing / red 1min+ down / gray polling-pending), right-click menu with 6 items (Status / Open Web UI / Start / Stop / Restart / Quit Tray). Daemon control via async PowerShell `Start/Stop-ScheduledTask` cmdlets (= same path as `src/service/windows.rs`). Dual event loop: `tao` on the main thread, `tokio` runtime on a dedicated thread, bridged via `EventLoopProxy::send_event`. Panic hook + `tracing-appender::rolling::daily` write logs to `%LOCALAPPDATA%\kb-mcp\logs\tray.YYYY-MM-DD`. Library API (`install::install_autostart` / `uninstall_autostart`) is invoked by `kb-mcp service install --with-tray` / `service uninstall` / `service tray-install` / `service tray-uninstall` to manage the shell:startup `.lnk` shortcut via PowerShell `WScript.Shell` COM. cargo-dist publishes `kb-mcp-tray.exe` only for `x86_64-pc-windows-msvc`. |
| `src/schema.rs` | Frontmatter schema validation. Reads `kb-mcp-schema.toml` under `kb_path`, enforces `required` / `type` / `pattern` / `enum` / `min_length` / `max_length` / `allow_empty`. Invoked by the `kb-mcp validate` CLI which reports in text / JSON / GitHub-annotation formats. |
| `src/embedder.rs` | Thin wrapper over `fastembed-rs`. `ModelChoice` selects the embedding model (BGE-small-en-v1.5 / BGE-M3). `RerankerChoice` + `Reranker` provide optional cross-encoder reranking. |
| `src/db.rs` | `rusqlite` + `sqlite-vec` + FTS5 (trigram). Manages the `chunks` / `vec_chunks` / `fts_chunks` schemas and CRUD. Exposes `search_hybrid` (Reciprocal Rank Fusion; the constant and the bm25 column weights are configurable via `[search.fusion]` since v0.13.0, defaults `k = 60` and `2.0 / 1.0 / 1.0`) and the v0.7.0 unbounded variants for the MMR / parent retriever pipeline. `SearchFilters` struct unifies filter args (path globs / tags / date range / min_quality); `MatchSpan` carries byte-offset citations (added in v0.3.0). `chunks.level` (added v0.7.0) distinguishes h2 / h3 headings. |
| `src/mmr.rs` | (v0.7.0+) Maximal Marginal Relevance greedy re-rank with a similarity cache. `mmr_select` operates on the post-rerank candidate pool and is gated by `[search.mmr]` config or the `mmr` per-call param. |
| `src/parent.rs` | (v0.7.0+) Display-time parent retriever. `apply_parent_retriever` expands hit chunks via `expand_adjacent` (level-aware sibling merge) or `expand_whole_document` (full-doc fallback for chunks under `whole_doc_threshold_tokens`). Score / rank / `match_spans` stay on the original hit; only `content` and the new `expanded_from` field change. |
| `src/quality.rs` | Per-chunk quality scoring (length / boilerplate / structure signals). |
| `src/graph.rs` | Connection graph BFS over the vector index, for the `get_connection_graph` MCP tool and the `kb-mcp graph` CLI. |
| `src/eval.rs` | Optional retrieval-quality evaluation for the `kb-mcp eval` CLI. Parses a golden YAML, runs each query through `db.search_hybrid`, and computes recall@k / MRR / nDCG@k. Loads / saves `<kb_path>/.kb-mcp-eval-history.json` for diff display. `ConfigFingerprint` (v0.7.0+) carries optional `mmr` / `parent_retriever` / `fusion` (v0.13.0+) so eval runs with different settings produce distinguishable history entries; each is recorded only when it differs from the built-in default, keeping older baselines comparable. Opt-in; does not affect `serve` / `search` / `index`. |
| `src/tune.rs` | (v0.13.0+) Optional measurement tool for the `kb-mcp tune` CLI. Sweeps a fixed grid of RRF constants and FTS5 bm25 column weights over the golden query set, guards the result with nested leave-one-query-out CV (paired SE, sign test, selection stability, secondary-metric non-degradation), and prints either a paste-ready `[search.fusion]` snippet or a "keep the defaults" conclusion. Applies nothing automatically and never runs a reranker. Reuses `eval`'s `GoldenSet` / `compute_query_metrics` and `db::fuse_rrf_ids`. |

## Data flow

```
.md / .txt / .pdf files (filtered by Registry::extensions())
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

## Contextual Retrieval (v0.12.0+)

Static Contextual Retrieval (feature-46) prepends a document-structure breadcrumb to each chunk before it reaches the embedder / FTS index / reranker, entirely at index time and with no LLM call. Gated by `[contextual].enabled` (default **off** as of v0.12.0 — a `false`-by-default judgment gate result, see the README's "Contextual Retrieval" section for the A/B numbers that drove it).

- **`Chunk.context: Option<String>`** (`src/parser/mod.rs`) is a search-only field, never returned by `search` / `get_document`. `build_context(parts: &[&str]) -> Option<String>` joins non-empty, non-consecutive-duplicate parts with `" > "`, capped at 200 chars (char-boundary safe) to bound BGE-small's 512-token input.
- **Two ancestry generation families**, matching the two parser shapes in the codebase:
  1. **Markdown** (`src/parser/markdown.rs`): a level-keyed ancestry `stack: Vec<(u8, String)>` is popped down to the current heading's depth on every heading transition (so an h2→h4 level jump inherits the nearest shallower ancestor, not a synthetic h3), then pushed with the new heading — excluded headings (`exclude_headings`) are still pushed onto the stack so descendants keep correct ancestry even though the excluded section itself produces no chunk. `context = build_context(&[title, ...ancestry, heading])`.
  2. **Binary / flat formats** (PDF page chunks, Office single-section chunks, plain `.txt`, via `parser::single_text_chunk` and the per-format chunkers): a single-level context of `[title]` only — these formats have no nested heading hierarchy to walk.
- **`chunks.context_text TEXT`** (`src/db.rs`): nullable column, added via the idempotent `ensure_context_text_column` `ALTER TABLE` for pre-feature-46 DBs. Populated from `Chunk.context` only when the active `ContextMode` is `Static`; stays `NULL` in `Off` mode.
- **FTS5 third column** `context` on `fts_chunks` (alongside `heading` / `content`): a legacy 2-column index is migrated via `ensure_fts_context_column` — drop + recreate the virtual table, then repopulate with `INSERT ... SELECT id, heading, COALESCE(context_text, ''), content FROM chunks`, wrapped in a `BEGIN IMMEDIATE` transaction (`begin_immediate_tx`) to serialize against concurrent openers. The migration holds the write lock for the full repopulate (measured 9.7–12.3s under concurrent embedding/reranker load on a 574-doc / 10,002-chunk KB), so `Database::init` sets `busy_timeout = 30_000ms` (raised from 10s in v0.12.0) so a `serve`-resident process's `search` / `status` waits out an in-flight migration instead of failing with `SQLITE_BUSY`. Contextual BM25 scoring weights the `context` column via `FTS_BM25_CONTEXT_WEIGHT = 1.0` in the `bm25(fts_chunks, heading_weight, context_weight, content_weight)` call.
- **Embedding input composition** (`indexer::embed_input_for`): in `Static` mode with a non-empty context, the embedder receives `"{context}\n\n{content}"`; otherwise (Off mode, or no context available) it receives `content` unchanged — this is the only place the embedding input differs from pre-v0.12.0 behavior.
- **Reranker input composition** (`embedder::contextualize_for_rerank`): `SearchResult` carries `context_text` end-to-end from `db.rs`'s search queries; the reranker composes the same `"{context}\n\n{content}"` string per candidate before scoring. `context_text` itself is stripped from the response before it reaches the MCP/CLI caller — it is purely an internal ranking signal.
- **`index_meta.context_mode` versioning** (`ContextMode::{Off, Static}`, `db::read_context_mode` / `write_context_mode`, `indexer::resolve_context_mode`): the DB's *actual* built mode is authoritative over the config's *desired* mode to avoid silently mixing embedding spaces mid-index.
  - `--force`: adopt the desired mode unconditionally (the DB was just reset by `reset_for_model`) and record it.
  - No `--force`, DB already has a recorded mode that differs from desired: **stay in the DB's mode**, print a stderr warning pointing at `kb-mcp index --force`.
  - No `--force`, no recorded mode (a genuine pre-feature-46 DB, `index_meta` has no `context_mode` key): grandfather to `Off` if the DB already has chunks (an existing index that predates this feature), or adopt the desired mode if the DB is empty (a brand-new index).
  - `kb-mcp status` prints `Context mode: static` / `Context mode: off` on stderr, sourced directly from `read_context_mode`.
- `main.rs` computes the desired mode identically for both `serve` and `index` from `cfg.contextual.as_ref().map(|c| c.enabled).unwrap_or(false)` — the `unwrap_or(false)` mirrors `ContextualConfig::default()` so an absent `[contextual]` section and an explicit `enabled = false` behave identically.

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
- **`tray-icon`** 0.24 + **`tao`** 0.35 + **`image`** 0.25 + **`tracing-appender`** 0.2 + **`winresource`** 0.1 (build-dep) — Windows-only deps of the `kb-mcp-tray` crate (added v0.9.0 / feature-44). `tray-icon` provides the muda-based context menu + icon swap, `tao` the Win32 event loop, `image` decodes embedded PNG status icons to RGBA, `tracing-appender` writes the daily rotating tray log, `winresource` embeds `assets/app.ico` as the exe icon. All gated to `target_os = "windows"` so non-Windows workspace builds skip them.
