# kb-mcp

MCP server for semantic search over a Markdown / plain-text knowledge base.

Parses Markdown (and optionally `.txt`) files with YAML frontmatter, splits them into heading-based chunks, generates embeddings with a selectable model (BGE-small-en-v1.5 by default, BGE-M3 for multilingual/Japanese knowledge bases), and stores everything in SQLite with sqlite-vec for vector similarity search. Connects to Claude Code, Cursor, or any MCP-compatible client via stdio (default, 1 client) or Streamable HTTP (many clients) transport.

A live-sync file watcher keeps the index fresh on manual edits, `git pull`, and external scripts; an optional TOML schema can validate frontmatter conventions via `kb-mcp validate`.

> **日本語版**: [README.ja.md](./README.ja.md)

## Install

### Pre-built binaries (recommended for non-Rust users)

Download the archive for your platform from the [latest GitHub release](https://github.com/alphabet-h/kb-mcp/releases/latest), extract it, and place `kb-mcp` (or `kb-mcp.exe` on Windows) somewhere on `PATH`. Available targets:

| Platform | Archive |
| --- | --- |
| Linux x86_64 (glibc 2.38+ / Ubuntu 24.04+ / Debian 13+ / RHEL 9.5+) | `kb-mcp-x86_64-unknown-linux-gnu.tar.xz` |
| Linux aarch64 (glibc 2.38+) | `kb-mcp-aarch64-unknown-linux-gnu.tar.xz` |
| macOS Apple Silicon | `kb-mcp-aarch64-apple-darwin.tar.xz` |
| Windows x86_64 (Windows 10+) | `kb-mcp-x86_64-pc-windows-msvc.zip` |

> **Intel Mac (`x86_64-apple-darwin`)** is not shipped as a prebuilt: the upstream ONNX Runtime crate (`ort-sys`) does not provide a binary for that target. Build from source as described below.

Each archive ships the binary plus `CHANGELOG.md`, `LICENSE-MIT`, `LICENSE-APACHE`, and `README.md`. Verify the SHA-256 checksum (each release exposes `sha256.sum` and per-archive `*.sha256` files) before running.

ONNX runtime and SQLite are statically linked into the binary, so no extra DLLs are required. Embedding models (ONNX) are downloaded from HuggingFace on first run — see [Working around HuggingFace TLS failures](#working-around-huggingface-tls-failures) if your network blocks that.

### Build from source

```bash
cargo build --release
```

The binary is produced at `target/release/kb-mcp` (or `kb-mcp.exe` on Windows).

## Optional config file

Any CLI option below can be given a default via a `kb-mcp.toml` file. CLI arguments always win; the file just removes repetition for a given deployment. The discovery order is described in [Config file discovery](#config-file-discovery) below — the most common placement is the project root (CWD) or alongside the binary. Copy `kb-mcp.toml.example` to `kb-mcp.toml` and edit:

```toml
# kb-mcp.toml (placed in the project root, the .git ancestor, or next to kb-mcp)
kb_path = "/path/to/knowledge-base"
model = "bge-m3"
reranker = "bge-v2-m3"
rerank_by_default = true
fastembed_cache_dir = "/home/you/.cache/huggingface/hub"

# Heading substrings to exclude from chunking. Omit the key for no exclusions
# (the default is an empty list). Any heading containing one of these
# substrings — and its body content — is dropped from the chunk stream.
exclude_headings = ["次の深堀り候補", "参考リンク"]

# Directory basenames skipped during indexing (whole-name match). Omit the key
# for the default [".obsidian", ".git", "node_modules", "target", ".vscode",
# ".idea"]. A user-specified list replaces the default entirely; `[]` traverses
# everything.
# exclude_dirs = [".obsidian", ".git", "node_modules", "target", ".vscode", ".idea", "dist", ".next"]

# Per-chunk quality filter. Enabled by default, threshold 0.3.
# Set `enabled = false` to restore the previous (filter-off) behavior (return every chunk).
[quality_filter]
enabled = true
threshold = 0.3

# Indexing extensions. Omit the section to keep the previous default
# behavior (.md only). Opt-in to .txt via an explicit list. An empty array is
# rejected to prevent silent "nothing is indexed" failures.
# Currently supported ids: "md", "txt".
[parsers]
enabled = ["md", "txt"]

# Live-sync file watcher. When `kb-mcp serve` is running, changes
# under kb_path are detected and the affected files are re-indexed incrementally
# within `debounce_ms`. Complementary to the PostToolUse hook: covers manual
# edits, `git pull`, external scripts, etc. CLI `--no-watch` / `--debounce-ms`
# overrides. Omitting the section keeps watcher on with a 500 ms debounce.
[watch]
enabled = true
debounce_ms = 500

# Transport for `kb-mcp serve`. `kind = "stdio"` (default)
# supports one client at a time; `kind = "http"` (Streamable HTTP) allows
# many simultaneous clients at `/mcp`. `/healthz` returns 200 for health
# checks. CLI `--transport http --port 3100` overrides.
[transport]
kind = "http"

[transport.http]
bind = "127.0.0.1:3100"
# allowed_hosts = ["kb.example.lan", "192.168.1.10"]  # opt-in for LAN exposure (v0.5.0+)

# Optional: `kb-mcp eval` (retrieval quality evaluation, power-user feature).
# You only need this section if you run `kb-mcp eval` for tuning or
# regression tracking. Omit the section entirely for built-in defaults.
# [eval]
# golden = ".kb-mcp-eval.yml"             # default: <kb_path>/.kb-mcp-eval.yml
# history_size = 10                       # default: 10
# k_values = [1, 5, 10]                   # default: [1, 5, 10]
# regression_threshold = 0.05             # default: 0.05

# Optional: `search` tool tuning (v0.3.0+). Omit the section for defaults.
# [search]
# # rank-based low-confidence flag: trips when
# # top1.score / mean(top-N.score) < min_confidence_ratio.
# # 0.0 disables the flag. CLI `--min-confidence-ratio` and the MCP
# # param `min_confidence_ratio` override per query.
# min_confidence_ratio = 1.5

# Optional: MMR diversity re-rank (v0.7.0+). Off by default.
# Applied AFTER reranker and BEFORE parent retriever.
# [search.mmr]
# enabled = false
# lambda = 0.7              # 1.0 = no diversity (MMR off equiv); < 0.5 leans exploration
# same_doc_penalty = 0.0    # > 0 deduplicates same-document chunks; 0 = pure MMR

# Optional: parent retriever content expansion (v0.7.0+). Off by default.
# When a hit chunk is short, expand its `content` to adjacent siblings or the
# whole document so the LLM gets enough context. Score / order untouched.
# [search.parent_retriever]
# enabled = false
# whole_doc_threshold_tokens = 100   # token_count below this -> whole document fallback
# max_expanded_tokens = 2000         # cap for adjacent merge / whole-doc (BGE-M3 <= 8192)
```

With the file in place `kb-mcp serve` / `index` / `status` / `graph` / `search` all work without any of those flags. Unknown keys are rejected to catch typos early. `FASTEMBED_CACHE_DIR` from the real environment overrides the file entry.

### Config file discovery

`kb-mcp` looks up `kb-mcp.toml` in the following order on every invocation
and stops at the first hit:

| Priority | Location                                  | Notes                                        |
| -------- | ----------------------------------------- | -------------------------------------------- |
| 1        | `--config <PATH>` (any subcommand)        | Errors out if the file does not exist.       |
| 2        | `./kb-mcp.toml` (current working dir)     | Most natural for project-local KBs.          |
| 3        | `<git-root>/kb-mcp.toml` (walks up)       | Checks CWD + up to 19 ancestors (20 dirs total). |
| 4        | `<binary-dir>/kb-mcp.toml`                | Legacy / global-install fallback.            |
| 5        | (no config — built-in defaults)           | `--kb-path` becomes mandatory on the CLI.    |

`~` in `--config` is expanded to the home directory on all platforms
(including Windows `cmd.exe` where the shell does not expand it).

The chosen source is logged to stderr at startup as
`kb_mcp::config: loaded config source=...` so you can confirm which file is
in effect.

#### Example: per-project KB packaged in a repository

```jsonc
// repo-root/.mcp.json
{
  "mcpServers": {
    "kb": { "command": "kb-mcp", "args": ["serve"] }
  }
}
```

Commit `kb-mcp.toml` next to `.mcp.json`. Opening the project in Claude Code
launches `kb-mcp serve` from the repo root, the CWD lookup picks up
the project's `kb-mcp.toml`, and `.mcp.json` stays minimal.

#### Example: multiple KBs in the same Claude Code session

```jsonc
{
  "mcpServers": {
    "kb-personal": { "command": "kb-mcp", "args": ["serve", "--config", "~/kb/personal/kb-mcp.toml"] },
    "kb-project":  { "command": "kb-mcp", "args": ["serve", "--config", "./kb-mcp.toml"] },
    "kb-rust-docs":{ "command": "kb-mcp", "args": ["serve", "--config", "~/kb/rust-docs/kb-mcp.toml"] }
  }
}
```

Each entry runs as an independent MCP server with its own `kb-mcp.toml` and
its own `.kb-mcp.db`, so Claude can disambiguate by server name.

## Usage

### Build / rebuild the search index

```bash
kb-mcp index --kb-path /path/to/knowledge-base
kb-mcp index --kb-path /path/to/knowledge-base --force   # full re-index
kb-mcp index --kb-path /path/to/knowledge-base --model bge-m3 --force  # switch to BGE-M3 (1024 dim, multilingual)
```

Scans source files under the given directory, skipping the default `exclude_dirs` set (`.obsidian`, `.git`, `node_modules`, `target`, `.vscode`, `.idea` — see "Directory exclusion" below). By default only `.md` is picked up. Add `[parsers].enabled = ["md", "txt"]` to `kb-mcp.toml` to also index `.txt` files — their title is derived from the filename (`deep-dive-2026.txt` → `"deep dive 2026"`) and the whole body becomes a single chunk. Files whose content hash has not changed since the last run are skipped unless `--force` is passed.

`--model` accepts:
- `bge-small-en-v1.5` (default) — 384 dim, English-focused, ~130 MB first download.
- `bge-m3` — 1024 dim, multilingual (100+ languages incl. Japanese), ~2.3 GB first download. Recommended for Japanese-heavy knowledge bases.

Switching models on an existing index requires `--force` (the DB records the model/dim in `index_meta` and rejects mismatched runtimes).

#### Progress reporting flags (v0.7.8+)

Two flags control how `kb-mcp index` reports progress; they are mutually exclusive and default-off (the existing per-file `  indexed: foo.md (N chunks)` output is unchanged when neither flag is given).

- `--quiet`: suppress per-file output; only print start / `Found N source files` / `Done in ...` summary lines. Useful when running from harnesses (e.g. Claude Code Bash tool) that buffer streaming output until exit, so you can recognise "silence = still working" instead of confusing it with a hang.
- `--progress`: show progress UI. Auto-detects via `IsTerminal` on stderr — TTY gets an `indicatif` bar with elapsed / position / percent / ETA, non-TTY gets periodic `Progress: N/M (P%)` lines (~20 emits per run plus a 100 % anchor) so `tail -f indexing.log` works.

```bash
kb-mcp index --kb-path ./big-kb --quiet         # silent except for start / done
kb-mcp index --kb-path ./big-kb --progress      # bar in TTY, periodic lines in pipe
```

#### Model selection trade-offs

| Aspect | BGE-small-en-v1.5 | BGE-M3 |
|---|---|---|
| First-time download | ~130 MB | ~2.3 GB |
| Embedding dim | 384 | 1024 (index file ~2.6× larger) |
| RAM when loaded | ~500 MB | ~2 GB |
| Index build time | baseline | ~3–10× slower (CPU inference) |
| Japanese precision | poor (English-centric vocab) | strong (multilingual tokenizer + training) |
| English precision | strong | comparable |

Switching cost (existing index → new model):

1. `kb-mcp index --kb-path ... --model <new> --force` runs a full re-embedding (no incremental update possible; `DELETE FROM documents/chunks/vec_chunks` and start over).
2. Every `serve` / `index` call afterwards must pass the same `--model` (or have it set in `kb-mcp.toml`). A mismatch is rejected at startup by the `index_meta` check.

Practical recommendation: pick the model that matches your knowledge base's **primary language** up front. Don't oscillate between models unless you have a concrete precision problem — the full re-embedding is the expensive step.

### Start the MCP server

```bash
kb-mcp serve --kb-path /path/to/knowledge-base
kb-mcp serve --kb-path /path/to/knowledge-base --model bge-m3   # must match the indexed model
kb-mcp serve --kb-path ... --model bge-m3 --reranker bge-v2-m3  # + cross-encoder reranking
kb-mcp serve --kb-path ... --transport http --port 3100         # HTTP, multi-client
kb-mcp serve --kb-path ... --no-watch                           # disable live-sync
```

Starts the MCP server on stdio transport by default (one client at a time). Pass `--transport http --port <PORT>` (or `--bind <SOCKETADDR>`) to serve multiple clients simultaneously via Streamable HTTP — details in the [HTTP transport](#http-transport-for-multiple-simultaneous-clients) section.

The server exposes 6 tools (see below) and keeps the index in-process for low-latency queries. `--model` must match the model that built the current index, otherwise the server refuses to start with an actionable error message. A file watcher (enabled by default) re-indexes affected files when the contents under `--kb-path` change — see [Live-sync via file watcher](#live-sync-via-file-watcher).

`--reranker` (optional, default `none`) enables a cross-encoder re-ranking pass over the top candidates of the hybrid search:

- `none` — disabled (default).
- `bge-v2-m3` — BAAI/bge-reranker-v2-m3 (multilingual 100+, ~2.3 GB first download). Recommended for Japanese knowledge bases.
- `jina-v2-ml` — jinaai/jina-reranker-v2-base-multilingual (multilingual, ~1.2 GB). Lighter alternative.
- `bge-base` — BAAI/bge-reranker-base (English/Chinese only, ~280 MB). Not recommended for Japanese.

Latency cost of rerank is roughly 300–700 ms per query on CPU with `bge-v2-m3` over 50 candidates. `--rerank-by-default` (on by default when `--reranker` is set) controls whether every `search` call uses rerank; the MCP tool takes `rerank: Option<bool>` to override per-query. Switching the reranker does **not** require re-indexing (it is index-independent).

#### When to enable reranking

Rerank trades latency for precision. The right choice depends on usage pattern:

| Scenario | Recommendation |
|---|---|
| Interactive agent flows (the LLM calls `search` 2–5 times per turn) | **Leave off.** +500 ms × N search calls adds up fast; retrieval quality from BGE-M3 + heading-weighted bm25 is usually sufficient. |
| One-shot, precision-critical queries (research, definitive answers) | **Enable.** The latency tax is paid once per turn, and the cross-encoder meaningfully promotes semantically relevant candidates. |
| Mixed usage | Start with `rerank_by_default = false` and let the caller opt in per query via the MCP tool's `rerank: true` parameter. |

Symptoms that suggest you should turn rerank on:

- Top-5 results often miss the obviously right chunk even after query rewording.
- Queries that use synonyms / paraphrases of the indexed wording are failing (e.g. Japanese 「バグ」 vs English "error").
- The agent re-queries multiple times per turn, wasting context by reading wrong hits.

Because rerank is index-independent, you can enable it for a week, measure the quality delta, and disable it if the benefit is not visible — no re-indexing needed.

### Registering kb-mcp as an OS service (v0.8.0+)

`kb-mcp service install` registers the daemon as an OS-level user service (no admin/sudo required) and configures auto-start at login.

```bash
# Default: service name 'kb-mcp', bind 127.0.0.1:3100, auto-start ON
kb-mcp service install --kb-path /path/to/your-kb

# Multi-instance (= run multiple KBs as separate services)
kb-mcp service install --service-name work --kb-path /path/to/work-kb --bind 127.0.0.1:3100
kb-mcp service install --service-name personal --kb-path /path/to/personal-kb --bind 127.0.0.1:3101

# Inspect / manage
kb-mcp service status                              # default 'kb-mcp'
kb-mcp service list                                # all instances
kb-mcp service uninstall personal                  # remove unit, keep config + DB
kb-mcp service uninstall personal --purge --yes    # also remove config + DB
```

OS-specific backends:
- **Linux**: systemd-user (`~/.config/systemd/user/kb-mcp-<name>.service`). Run `sudo loginctl enable-linger $USER` to keep the daemon running after logout.
- **macOS**: launchd LaunchAgent (`~/Library/LaunchAgents/com.kb-mcp.<name>.plist`).
- **Windows**: Task Scheduler AT_LOGON (= no admin required, `\kb-mcp-<name>` task).

The installer writes a config home at `<dirs::config_dir()>/kb-mcp/<service-name>/` containing `kb-mcp.toml` (with `kb_path` and `bind`). Override the base directory via `KB_MCP_CONFIG_HOME` env var.

Non-loopback bind addresses (e.g. `0.0.0.0:3100`) require `--i-know` since kb-mcp has no authentication.

> **Migration from v0.7.x personal-http recipe**: The `kb-mcp/examples/deployments/personal-http/` templates were removed in v0.8.0. Disable / delete the manually installed unit before running `kb-mcp service install`:
> - Linux: `systemctl --user disable kb-mcp.service && rm ~/.config/systemd/user/kb-mcp.service`
> - macOS: `launchctl bootout gui/<uid>/com.kb-mcp.kb-mcp && rm ~/Library/LaunchAgents/com.kb-mcp.kb-mcp.plist`
> - Windows: `schtasks /End /TN '\kb-mcp' ; schtasks /Delete /TN '\kb-mcp' /F` (replace `\kb-mcp` with whatever name the old task used)
>
> If you're carrying settings over from the old `kb-mcp.toml` (e.g. `model = "bge-m3"`, `exclude_dirs`, `best_practice`, `fastembed_cache_dir`), edit the **new** config at `<dirs::config_dir()>/kb-mcp/<service-name>/kb-mcp.toml` after install. **`kb_path` must be an absolute path** — the new daemon's `WorkingDirectory` is `config_home`, so a relative `kb_path = "./knowledge-base"` will resolve to `<config_home>/knowledge-base` and miss the real KB. Use TOML literal strings (single quotes) to avoid Windows backslash escapes: `kb_path = 'C:\Users\you\your-kb'`.

### Tray monitor (Windows only, v0.9.0+)

`kb-mcp-tray.exe` is a Windows system tray binary that visualizes daemon state and provides Start / Stop / Restart controls. Shipped in the Windows release zip alongside `kb-mcp.exe`.

Install alongside the daemon:

```bash
kb-mcp service install --kb-path C:\path\to\kb --with-tray
```

On next logon the tray icon appears with a colored status dot:

- **green** — daemon healthy (last `/api/admin/status` poll succeeded)
- **yellow** — daemon is indexing
- **red** — daemon has been unreachable for >= 1 minute (= 12 consecutive failed polls at 5s interval)
- **gray** — pre-first-poll (= within the first 5 seconds of startup)

Right-click reveals six menu items: **Status** (read-only line) / **Open Web UI** / **Start** / **Stop** / **Restart** / **Quit Tray**. Start/Stop/Restart drive the daemon through PowerShell `Start/Stop-ScheduledTask` cmdlets.

Tray logs live at `%LOCALAPPDATA%\kb-mcp\logs\tray.YYYY-MM-DD` (daily rotation). Set `KB_MCP_TRAY_LOG=debug` for verbose output. Pass `--debug` to attach a console for live stdout/stderr.

Uninstalling the daemon also removes the tray shortcut:

```bash
kb-mcp service uninstall --service-name kb-mcp
```

To manage the tray shortcut independently of the daemon registration:

```bash
kb-mcp service tray-install --service-name kb-mcp     # add shortcut only
kb-mcp service tray-uninstall --service-name kb-mcp   # remove shortcut only
```

The tray polls `127.0.0.1:<port>/api/admin/status`, so the daemon must be bound to either loopback (`127.0.0.1`) or a wildcard (`0.0.0.0`). A daemon bound to a specific NIC such as `192.168.1.5:3100` is not listening on loopback, and the tray logs a warning at startup so the misconfiguration is discoverable.

### Show index status

```bash
kb-mcp status --kb-path /path/to/knowledge-base
```

Prints document and chunk counts from the existing index.

### One-shot search from the command line

For shell scripts or skill bins that just need "search this string in the KB" without standing up an MCP connection:

```bash
kb-mcp search "RAG server comparison" --limit 3 --format text
kb-mcp search "E0382" --category deep-dive --format json | jq '.results[] | .path'
kb-mcp search "クエリ最適化" --reranker bge-v2-m3        # optional per-invocation rerank
```

`--format` is `json` (default, a `{ results, low_confidence, filter_applied }` wrapper as documented under "Search filters and citations" below) or `text` (LLM-friendly blocks separated by `---`). All other flags mirror `serve`: `--kb-path`, `--model`, `--reranker`, `--category`, `--topic`, `--limit`. The quality filter is on by default — pass `--include-low-quality` or `--min-quality 0` to restore the previous (filter-off) behavior for a single query. The `kb-mcp.toml` defaults apply exactly as in `serve`/`index`.

Typical skill-bin use: a Claude Code skill places `kb-mcp.exe` + `kb-mcp.toml` in its `bin/`, then a command like `kb-mcp search "{{user_query}}" --format text --limit 3` returns a focused reference excerpt for the LLM to cite.

### Search filters and citations (v0.3.0+)

Starting in v0.3.0 the `search` MCP tool returns a wrapper object instead of a raw array of hits. **This is a breaking change** for clients that parse the response as `Vec<SearchHit>` directly:

```jsonc
{
  "results":        [{ "score": 0.83, "path": "...", "match_spans": [...], "tags": [...], ... }],
  "low_confidence": false,
  "filter_applied": { /* non-default filters echoed back; empty object when no filters */ }
}
```

`results[].match_spans` are byte offsets into `content` for ASCII queries, so MCP clients can quote the source text accurately. `low_confidence` is a rank-based flag (`top1.score / mean(top-N.score) < min_confidence_ratio`); the threshold defaults to `1.5` and can be tuned via `[search].min_confidence_ratio` in `kb-mcp.toml` or `--min-confidence-ratio` per query.

Input bounds (defensive, v0.6.0+): `query` is capped at 1 KiB; longer inputs are rejected with an `ErrorResponse`. `match_spans` is computed only for chunks under 256 KiB and capped at 100 spans per chunk. These exist to bound abuse, not legitimate use — typical chunks are well under the ceilings.

The `search` tool / CLI also gained these filters in v0.3.0:

```bash
kb-mcp search "tokio spawn" \
  --path-glob "docs/**" --path-glob "!docs/draft/**" \
  --tag-any rust,async \
  --date-from 2026-01-01 \
  --min-confidence-ratio 1.5
```

- `--path-glob <PATTERN>` (repeatable) — include / exclude by path glob; `!`-prefix is an exclude. MCP param: `path_globs`.
- `--tag-any <a,b,c>` — pass if the chunk has **any** of these tags. MCP param: `tags_any`.
- `--tag-all <a,b,c>` — pass only if the chunk has **all** of these tags. MCP param: `tags_all`.
- `--date-from <YYYY-MM-DD>` / `--date-to <YYYY-MM-DD>` — lex comparison; chunks with no `date` are excluded strictly when either bound is set. MCP params: `date_from` / `date_to`.
- `--min-confidence-ratio <N>` — per-query override of the `low_confidence` threshold.

CLI `kb-mcp search --format json` follows the same wrapper format. See [docs/citations.md](docs/citations.md) for `match_spans` / byte-offset details and [docs/filters.md](docs/filters.md) for the full filter reference.

### Diversity (MMR) and parent retriever (v0.7.0+)

Two opt-in retrieval-quality knobs land in v0.7.0. They are independent — enable either, both, or neither. Both default to **off** so existing pipelines behave exactly as before.

```bash
# MMR diversity re-rank
kb-mcp search "tokio runtime" --mmr true --mmr-lambda 0.7

# Parent retriever (expand short chunks to adjacent siblings or whole doc)
kb-mcp search "k=60 in RRF" --parent-retriever true

# Both at once
kb-mcp search "context management" --mmr true --parent-retriever true
```

CLI flags (also accepted by `kb-mcp eval`):

- `--mmr <bool>` — enable MMR diversity re-rank. Default `false`.
- `--mmr-lambda <0..1>` — MMR balance: `1.0` is "no diversity" (= MMR off behavior), lower values lean toward exploration / less redundancy. Default `0.7`.
- `--mmr-same-doc-penalty <0..1>` — extra cost when an already-selected chunk lives in the same document. `0.0` is pure MMR; raise to actively deduplicate same-doc chunks. Default `0.0`.
- `--parent-retriever <bool>` — when a hit chunk's token count is below `whole_doc_threshold_tokens`, expand its `content` to adjacent siblings (level-aware) or, for very short chunks, the whole document. The score, rank, path, and `match_spans` of the original hit are preserved; only `content` (and a new optional `expanded_from`) changes. Default `false`.

MCP `search` tool gains the matching per-call params `mmr` / `mmr_lambda` / `mmr_same_doc_penalty` / `parent_retriever`. Toml defaults live in `[search.mmr]` and `[search.parent_retriever]` (see [Optional config file](#optional-config-file) above). Per-call params override toml; toml overrides built-in defaults.

The pipeline order is **`RRF → reranker → MMR → parent retriever → match_spans`**. MMR re-orders candidates while the reranker score is still on the chunks; parent retriever runs last so the expanded content does not contaminate the relevance signal. See [docs/retrieval-pipeline.md](docs/retrieval-pipeline.md) for the full pipeline narrative and tuning advice.

### Connection graph from a starting document

When you want to find not just a single document but the semantic neighborhood around it (and neighbors of those neighbors), use the `graph` subcommand:

```bash
kb-mcp graph --start deep-dive/mcp/overview.md --depth 2 --fan-out 5
kb-mcp graph --start notes/rag.md --dedup-by-path --format text
kb-mcp graph --start a.md --exclude junk1.md,junk2.md --min-similarity 0.5
```

Flags:

- `--start PATH` — required, relative path to an indexed document.
- `--depth` (default 2, clamped to max 3) — BFS hops.
- `--fan-out` (default 5, clamped to max 20) — neighbors per node per hop. `0` returns only the seed.
- `--min-similarity` (default 0.3) — cosine similarity cut-off. `0.0..=1.0`.
- `--seed-strategy` — `all-chunks` (default) expands from every chunk of the start doc; `centroid` averages them (L2-renormalized) into one virtual seed.
- `--exclude` — comma-separated paths to drop from results. The start path itself is always excluded.
- `--dedup-by-path` — collapse same-path hits so each document appears at most once.
- `--category` / `--topic` — apply category / topic filters to every hop.
- `--format json|text` — same as `search`.

The output is a flat array of nodes with `parent_id` / `depth` / `score` so the consumer can reconstruct the tree if it wants. Good use cases: "give me 30 chunks of related context around this note for the LLM to read", or "walk two hops from this overview to see what topics it touches".

### Validate frontmatter against a TOML schema

If your knowledge base follows a frontmatter convention, `kb-mcp validate` checks every `.md` file against a TOML schema and reports violations. See the [Frontmatter schema validation](#frontmatter-schema-validation) section below for the schema format; the command itself is:

```bash
kb-mcp validate --kb-path /path/to/knowledge-base
kb-mcp validate --kb-path ... --format json | jq '.files[]'
kb-mcp validate --kb-path ... --format github         # ::error annotations for CI
```

Exit codes: `0` (no violations), `1` (violations), `2` (schema load error). When `kb-mcp-schema.toml` is absent under `--kb-path`, the command exits 0 with a short "no schema found" note, so adding `kb-mcp validate` to an existing workflow is non-disruptive until you actually write a schema.

> The `--strict` flag is currently a no-op (accepted for forward compatibility with future stricter checking modes). Use the regular invocation for now.

### Evaluate retrieval quality against a golden query set

**Optional power-user feature.** `kb-mcp eval` takes a small file of questions with known answers, runs them through the same hybrid search the `search` tool uses, and reports **recall@k / MRR / nDCG@k** with diffs against the previous run. Useful when comparing models or tuning `[quality_filter]` / RRF parameters.

Regular users running `kb-mcp index` + `kb-mcp serve` do not need this — without a golden file, `eval` just errors with a hint and exits.

```bash
# 1) Write a golden YAML at <kb_path>/.kb-mcp-eval.yml
cat > knowledge-base/.kb-mcp-eval.yml <<'EOF'
queries:
  - query: "What does the k parameter in RRF do?"
    expected:
      - { path: "docs/ARCHITECTURE.md", heading: "Data flow" }
      - { path: "src/db.rs" }   # heading omitted = file-level hit
EOF

# 2) Run against the indexed DB
kb-mcp eval --kb-path knowledge-base

# 3) Re-run after tweaking config / model to see the diff
kb-mcp eval --kb-path knowledge-base --reranker bge-v2-m3
```

Output: aggregate metrics + per-query rows for regressions / misses only. JSON (`--format json`) exposes the full per-query detail. History lives at `<kb_path>/.kb-mcp-eval-history.json` and keeps the last 10 runs for diff display.

For CI: pass `--fail-on-regression` (v0.6.0+) to exit with code 1 when any aggregate metric (`recall@k` / `MRR` / `ndcg@k`) regressed from the previous **fingerprint-compatible** run by more than `regression_threshold` (default 0.05). Updating the golden YAML changes the hash, so the next run skips the comparison rather than triggering a false positive. Details: [docs/eval.md](./docs/eval.md).

See [docs/eval.md](docs/eval.md) for the golden YAML reference, metric definitions, diff output guide, and troubleshooting.

## Connecting to Claude Code / Cursor

> **Looking for full deployment recipes?** See [`kb-mcp/examples/deployments/`](./kb-mcp/examples/deployments/) for ready-to-adapt configs covering four patterns: personal stdio, personal-http (one local daemon for multiple parallel Claude Code sessions), NAS-shared (one writer + many read-only clients), and intranet HTTP server (one server + many clients). The snippets below are the canonical stdio entry point you'll find in those recipes.

Add the following to `.mcp.json` in your project root (or the equivalent MCP config for your client):

```json
{
  "mcpServers": {
    "ai-knowledge": {
      "command": "/path/to/kb-mcp",
      "args": ["serve", "--kb-path", "/path/to/knowledge-base"],
      "type": "stdio"
    }
  }
}
```

With a multilingual model and reranker enabled:

```json
{
  "mcpServers": {
    "ai-knowledge": {
      "command": "/path/to/kb-mcp",
      "args": [
        "serve",
        "--kb-path", "/path/to/knowledge-base",
        "--model", "bge-m3",
        "--reranker", "bge-v2-m3"
      ],
      "env": {
        "FASTEMBED_CACHE_DIR": "/path/to/.cache/huggingface/hub"
      },
      "type": "stdio"
    }
  }
}
```

For agent workflows, a more conservative alternative: load the reranker but leave it off by default, letting the caller opt in with `rerank: true` on individual `search` calls.

```json
{
  "mcpServers": {
    "ai-knowledge": {
      "command": "/path/to/kb-mcp",
      "args": [
        "serve",
        "--kb-path", "/path/to/knowledge-base",
        "--model", "bge-m3",
        "--reranker", "bge-v2-m3",
        "--rerank-by-default=false"
      ],
      "env": { "FASTEMBED_CACHE_DIR": "/path/to/.cache/huggingface/hub" },
      "type": "stdio"
    }
  }
}
```

Or, if you placed a `kb-mcp.toml` somewhere on the [discovery path](#config-file-discovery) with those options set, the `.mcp.json` can shrink to:

```json
{
  "mcpServers": {
    "ai-knowledge": {
      "command": "/path/to/kb-mcp",
      "args": ["serve"],
      "type": "stdio"
    }
  }
}
```

The server will be started automatically when the client connects.

### Keeping the index fresh via PostToolUse hook
If you edit the knowledge base from inside a Claude Code session (or run a skill that writes Markdown files), the running MCP server will keep returning stale results until the index is rebuilt. A `PostToolUse` hook in `.claude/settings.json` can re-index automatically after every write. Minimal form:

```json
{
  "hooks": {
    "PostToolUse": [
      {
        "matcher": "Write|Edit|MultiEdit|Skill",
        "hooks": [
          { "type": "command", "command": "kb-mcp index" }
        ]
      }
    ]
  }
}
```

SHA-256 diffing in `kb-mcp index` makes the second-and-later invocations fast (usually sub-second on small KBs). A richer shell script that inspects the tool payload and only rebuilds when the edited file is under `$KB_PATH` ships with the repo: see [`kb-mcp/examples/hooks/`](./kb-mcp/examples/hooks/README.md). SQLite runs in WAL mode so the hook can safely run while the MCP server is still up.

### Frontmatter schema validation
If your knowledge base follows a frontmatter convention (e.g. `title` required, `date` is YYYY-MM-DD, `topic` limited to an enum), you can check every `.md` file for violations with:

```bash
kb-mcp validate --kb-path /path/to/knowledge-base
```

Put a `kb-mcp-schema.toml` at the root of `--kb-path` (template: `kb-mcp-schema.toml.example`):

```toml
[fields.title]
required = true
type = "string"
min_length = 1

[fields.date]
required = true
type = "string"
pattern = '^\d{4}-\d{2}-\d{2}$'

[fields.topic]
required = true
type = "string"
enum = ["mcp", "rag", "ai", "tooling", "ops"]

[fields.tags]
required = true
type = "array"
min_length = 1
```

- **No schema file → exit 0** with a short "no schema found" note. Backward compatible: existing pipelines that don't yet have a schema file continue to pass.
- `--format text` (default, color when TTY) / `json` / `github` for CI annotations.
- Exit codes: `0` (no violations), `1` (violations), `2` (schema load error).
- `.txt` files are skipped (no frontmatter concept).
- The `index` and `serve` commands are not affected — validation is opt-in only.

### HTTP transport for multiple simultaneous clients
By default `kb-mcp serve` speaks MCP over stdio — one client per server process. To serve multiple clients simultaneously (e.g. several Claude Code sessions or an external script hitting the same index), switch to Streamable HTTP:

```bash
kb-mcp serve --kb-path /path/to/knowledge-base --transport http --port 3100
# or: --bind 0.0.0.0:3100
```

The server mounts the MCP endpoint at `/mcp` and exposes `/healthz` for probes. `.mcp.json` for an HTTP-capable client:

```json
{
  "mcpServers": {
    "ai-knowledge": {
      "type": "http",
      "url": "http://127.0.0.1:3100/mcp"
    }
  }
}
```

Security notes:
- Default bind is `127.0.0.1:3100` (loopback). Use `--bind 0.0.0.0:3100` only on trusted networks — **kb-mcp has no built-in authentication yet**.
- rmcp's Streamable HTTP layer enforces Host header validation (loopback only by default) to prevent DNS rebinding attacks.
- For LAN / intranet exposure, set `[transport.http].allowed_hosts` in `kb-mcp.toml` to your public hostnames / IPs (e.g. `["kb.example.lan", "192.168.1.10"]`). Binding to a non-loopback address with the default loopback-only allow-list means external requests are 403'd by Host validation; kb-mcp emits a `tracing::warn` at startup when this misconfiguration is detected. An empty `allowed_hosts = []` disables the check entirely (rmcp's `disable_allowed_hosts` semantics) — operator-acknowledged opt-out, not recommended for public deployments.
- Mutex-based serialization inside the server means HTTP concurrent requests are still processed sequentially at the embedder / DB level (~10 qps expected for `search`). Heavy parallelism is a future enhancement.

### Live-sync via file watcher
`kb-mcp serve` runs a `notify`-based file watcher by default. Any change under `--kb-path` (create / modify / delete / rename) is detected, debounced, and only the affected file is re-indexed. This covers manual editor saves, `git pull`, and external scripts — cases the PostToolUse hook cannot intercept.

- **Default on**. `[watch].enabled = false` in `kb-mcp.toml` or `--no-watch` on the command line disables it.
- **Debounce** is 500 ms by default. Tune with `[watch].debounce_ms` or `--debounce-ms`.
- **Coexists with the PostToolUse hook**. Both paths lock the same `Mutex<Database>` / `Mutex<Embedder>`, so concurrent triggers are serialized at the Rust layer and are idempotent.
- **Extension-aware**. The watcher shares the Parser registry with `rebuild_index`, so only files whose extension is enabled in `[parsers].enabled` are re-indexed; other events are dropped.
- **Resilience**. Errors inside the watcher task are logged to stderr (not silently dropped) and the MCP server keeps running. Local disk is assumed — inotify on WSL / SMB / network shares is not guaranteed.
- **Backpressure (v0.6.0+)**. The bridge from the debouncer to the indexer task uses a bounded 64-batch channel; if the consumer cannot keep up (e.g. embedder is paused), excess batches are dropped with a warn log instead of growing the queue indefinitely. Run `rebuild_index` manually after the burst to recover any missed events.

### Working around HuggingFace TLS failures on first download

Some environments (corporate proxies, firewalls with TLS inspection) reject fastembed's native TLS connection to `huggingface.co` with `os error 10054` / "Connection was reset". In that case, pre-download the model via the Python HuggingFace CLI and point `FASTEMBED_CACHE_DIR` at the HF Hub cache:

```bash
# Install once
pip install --user huggingface_hub

# Pre-download BGE-M3 (required ONNX files only)
hf download BAAI/bge-m3 \
    --include 'onnx/*' 'tokenizer*' 'config.json' 'special_tokens_map.json'

# Pre-download BGE-reranker-v2-m3 (for `--reranker bge-v2-m3`)
hf download BAAI/bge-reranker-v2-m3

# Run kb-mcp pointing at the HF cache (HF Hub cache layout is compatible with fastembed)
FASTEMBED_CACHE_DIR=~/.cache/huggingface/hub \
    kb-mcp index --kb-path ./knowledge-base --model bge-m3 --force
```

## Tools

| Tool | Description | Key parameters |
|---|---|---|
| `search` | Hybrid search (vector + FTS5 full-text) merged via Reciprocal Rank Fusion, optionally followed by cross-encoder reranking, optional MMR diversity re-rank, and optional parent retriever content expansion. Returns a wrapper `{ results, low_confidence, filter_applied }` with chunks ranked by relevance; each result may carry `expanded_from` if parent retriever fired. See [docs/citations.md](docs/citations.md), [docs/filters.md](docs/filters.md), [docs/retrieval-pipeline.md](docs/retrieval-pipeline.md). | `query` (required), `limit`, `category`, `topic`, `rerank` (override server default), `min_quality`, `include_low_quality`, `path_globs` (glob list, `!`-prefix excludes), `tags_any` / `tags_all`, `date_from` / `date_to` (`YYYY-MM-DD`), `min_confidence_ratio`, `mmr` / `mmr_lambda` / `mmr_same_doc_penalty` (v0.7.0+), `parent_retriever` (v0.7.0+) |
| `list_topics` | List all indexed topics and categories with document counts. | (none) |
| `get_document` | Get the full content and metadata of a document by its relative path. | `path` (e.g. `"deep-dive/mcp/overview.md"`) |
| `get_best_practice` | Opt-in: when `[best_practice].path_templates` is configured in `kb-mcp.toml`, fetch a best-practices document for the given target and optionally extract an h2 section. Without configuration the tool returns a "not configured" error. | `target` (e.g. `"claude-code"`), `category` (optional) |
| `rebuild_index` | Rebuild the search index by scanning all source files (Markdown plus any other extensions enabled via `[parsers].enabled`). | `force` (optional, default false) |
| `get_connection_graph` | BFS-expand semantically related chunks starting from a document path. Returns a flat list of nodes with `parent_id` / `depth` / `score` / `snippet` so the caller can chain context discovery. | `path` (required), `depth` (default 2, max 3), `fan_out` (default 5, max 20), `min_similarity` (default 0.3), `seed_strategy` (`all_chunks` / `centroid`), `dedup_by_path`, `category`, `topic`, `exclude_paths` |

## Notes

- **Embedding model**: On first run, the selected ONNX model is downloaded to an OS-standard cache directory. Subsequent runs reuse the cached model. Resolution order:
  1. `FASTEMBED_CACHE_DIR` environment variable, if set.
  2. OS cache dir joined with `fastembed` (Linux: `~/.cache/fastembed`, macOS: `~/Library/Caches/fastembed`, Windows: `%LOCALAPPDATA%\fastembed`).
  3. `.fastembed_cache` under the current working directory (final fallback).
- **Index storage**: The SQLite database is stored as `.kb-mcp.db` in the **parent** directory of the `--kb-path` (i.e. the repository root when `--kb-path` points to `knowledge-base/`).
- **Parser registry**: only file extensions listed in `[parsers].enabled` are indexed. The section defaults to `["md"]` (the default behavior); `["md", "txt"]` opts into `.txt` where the title is derived from the filename. Unknown ids (e.g. `"pdf"` / `"rst"`) are rejected at startup; an empty array is also rejected to avoid silent "nothing is indexed" failures.
- **Live-sync file watcher**: `kb-mcp serve` spawns a `notify`-based watcher by default (`[watch].enabled = true`, 500 ms debounce). Manual saves, `git pull`, and external scripts are re-indexed incrementally on the same Mutex-guarded resources used by MCP tools, so concurrent triggers are serialized. Disable with `--no-watch` or `[watch].enabled = false`.
- **HTTP transport**: `--transport http --port 3100` serves MCP over rmcp's Streamable HTTP at `/mcp`, with `/healthz` for probes and a Mutex-serialized pipeline inside. Default bind is `127.0.0.1:3100` — `0.0.0.0` is opt-in and **has no built-in authentication yet**; restrict with a reverse proxy / firewall until that arrives.
- **Embedding dimensions**: Depends on `--model`. BGE-small-en-v1.5 = 384, BGE-M3 = 1024. The chosen dim is declared on the `vec_chunks` virtual table and recorded in the `index_meta` table; a mismatch at runtime is detected and rejected.
- **Incremental indexing**: Files are tracked by SHA-256 content hash. Only changed files are re-embedded on subsequent `index` runs (unless `--force` is passed). Moving / renaming a file without modifying its content is detected via hash match and handled as a `documents.path` UPDATE — the existing chunks, embeddings, and FTS rows are reused instead of being rebuilt. The rebuild summary reports the number of renames as `renamed` next to `updated` / `deleted`.
- **Hybrid search (FTS5 + vector)**: The `search` tool combines SQLite FTS5 full-text search (trigram tokenizer, works for Japanese/CJK too; `heading` column is weighted 2× `content` in bm25) with the vector search via Reciprocal Rank Fusion (k=60). The returned `score` is the RRF score (higher = better), not a distance. Queries shorter than 3 characters fall back to vector-only (below the trigram minimum).
- **Optional reranking**: With `--reranker <model>` the top candidates are re-scored by a cross-encoder before being returned. When rerank is applied, `score` is the cross-encoder raw score instead of the RRF value. Reranking is index-independent — you can toggle it at server start without re-indexing.
- **Connection graph**: `get_connection_graph` / `kb-mcp graph` do BFS over the vector index starting from a document. No extra index is built; every hop runs a fresh sqlite-vec KNN. Bounded by `depth ≤ 3` / `fan_out ≤ 20` with client-side clamping, so worst-case is ~21 KNN queries per request. Scores are cosine similarity approximated from L2 distance (`1 - d²/2`, clamped to `[0,1]`) assuming unit-normalized embeddings (BGE-small / BGE-M3 are normalized internally).
- **Heading exclusion**: Sections whose heading text contains any of `exclude_headings` are dropped during chunking. The default is an empty list (keep every section); populate `exclude_headings` in `kb-mcp.toml` to opt in. Matching is substring-based (`heading.contains(pattern)`), so short patterns catch suffixed variants (`"参考リンク"` would also match `"## 参考リンク (旧)"`).
- **Directory exclusion**: `walkdir` skips any directory whose basename matches an entry in `exclude_dirs`. The default list is `[".obsidian", ".git", "node_modules", "target", ".vscode", ".idea"]`. A user-specified list replaces the default entirely (no merging); `exclude_dirs = []` walks everything, including `.git/`.
- **`get_best_practice` path templates**: the tool is opt-in and requires `[best_practice].path_templates` in `kb-mcp.toml`. Each template may use `{target}` as a placeholder (e.g. `"best-practices/{target}/PERFECT.md"` or `"docs/{target}.md"`). The server tries templates in order and returns the first existing file under `kb_path` (path-traversal attempts are rejected). Omitting the section — or writing `path_templates = []` — leaves the tool registered but makes it return a "not configured" error, so accidental calls fail loudly instead of silently retrieving an unrelated file.
- **Per-chunk quality filter** (**enabled by default** with threshold `0.3`): each indexed chunk gets a `quality_score` computed from three signals — length (< 30 chars → -0.6), boilerplate-only content (TBD / TODO / 詳細は後述 / etc. → -0.5), poor structure (single line < 80 chars → -0.3). Chunks scoring below the threshold are hidden from `search`, `kb-mcp search`, and `get_connection_graph`. Seed chunks of `get_connection_graph` are exempt. Disable the filter with `[quality_filter] enabled = false` in `kb-mcp.toml`, or opt out per-query with `--include-low-quality` (CLI) / `include_low_quality: true` (MCP). Override the threshold with `--min-quality 0.5` / `min_quality: 0.5`. Upgrading an existing index: the next `kb-mcp index` run transparently adds the `quality_score` column (ALTER TABLE) and backfills scores once (idempotent).
