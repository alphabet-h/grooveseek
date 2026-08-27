# Configuration

Reference for `groove.toml`: every key it accepts, where the file is
looked for, and which of those locations are trusted.

> **日本語版**: [configuration.ja.md](./configuration.ja.md)

Any CLI option in [docs/usage.md](usage.md) can be given a default via a `groove.toml` file. CLI arguments always win; the file just removes repetition for a given deployment. The discovery order is described in [Config file discovery](#config-file-discovery) below — the most common placement is the project root (CWD) or alongside the binary. Copy [`grooveseek/groove.toml.example`](https://github.com/alphabet-h/grooveseek/blob/main/grooveseek/groove.toml.example) to `groove.toml` and edit.

**A fresh copy of that template changes nothing.** It is not blank — eleven sections (`[quality_filter]`, `[best_practice]`, `[parsers]`, `[watch]`, `[transport]`, `[transport.http]`, `[eval]`, `[search]`, `[search.mmr]`, `[search.fusion]`, `[search.parent_retriever]`) are left active so that the shape of the file is visible — but every active value in it is already the built-in default, so copying it pins those defaults rather than altering anything. Everything that *would* alter behavior is commented out. The block below is a different thing: an illustration of what each key does, with values filled in — some of them non-default, some of them just the default spelled out. Read it as a menu, not as a file to paste wholesale:

```toml
# groove.toml (placed in the project root, the .git ancestor, or next to groove)
kb_path = "/path/to/knowledge-base"
model = "bge-m3"
reranker = "bge-v2-m3"
# Read by `groove serve` and by `groove search` alike (v0.27.0+). For a single
# query, override it with the MCP tool's `rerank` parameter, or by naming
# `--reranker` on the command line — `--reranker none` opts that query out.
rerank_by_default = true
fastembed_cache_dir = "/home/you/.cache/huggingface/hub"

# Where grammar plugins live (v1.3.0+). A language that is not compiled in —
# everything except Rust — is a library you download and place yourself; this
# names the directory it goes in. `GROOVE_GRAMMAR_DIR` overrides it and must be
# absolute. Read **only** when `[parsers].enabled` names a language that needs a
# plugin, so a Markdown knowledge base never consults it.
# Default: `<local data dir>/groove/grammars`, which is
# `%LOCALAPPDATA%\groove\grammars` on Windows, `~/.local/share/groove/grammars`
# on Linux, and `~/Library/Application Support/groove/grammars` on macOS. If none
# of those can be determined the key has no default, and a command that needs a
# plugin says so rather than guessing a working-directory-relative path.
# See docs/clients.md for how to put a plugin in place.
grammar_dir = "/home/you/.local/share/groove/grammars"

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

# The `get_best_practice` MCP tool is opt-in: without this section (or with an
# empty list) it answers with a "not configured" error. Templates are tried in
# order with `{target}` substituted, resolved relative to kb_path; the first
# existing file wins.
[best_practice]
path_templates = ["best-practices/{target}/PERFECT.md", "docs/{target}.md"]

# Indexing extensions. Omit the section to keep the previous default
# behavior (.md only). Opt-in to .txt / .pdf / .docx / .xlsx / .pptx
# via an explicit list. An empty array is rejected to prevent silent
# "nothing is indexed" failures.
# Currently supported ids: "md", "txt", "pdf" (v0.10.0+), "docx", "xlsx",
# "pptx" (v0.11.0+), "rs" (v1.2.0+). ("xls" was withdrawn in v0.14.0 — see
# behavior.md.) Other languages arrive as libraries you place (v1.3.0+).
# Example enabling everything:
[parsers]
enabled = ["md", "txt", "pdf", "docx", "xlsx", "pptx", "rs"]

# Source code only. max_chunk_chars is the budget for one chunk in
# non-whitespace characters: a definition that fits becomes one chunk, one that
# does not is split into its nested definitions, or by lines when it has none
# (the usual case for a long function). Lower it for finer-grained hits, raise
# it to keep long bodies whole. Default 3500.
[parsers.code]
max_chunk_chars = 3500

# Live-sync file watcher. When `groove serve` is running, changes
# under kb_path are detected and the affected files are re-indexed incrementally
# within `debounce_ms`. Complementary to the PostToolUse hook: covers manual
# edits, `git pull`, external scripts, etc. CLI `--no-watch` / `--debounce-ms`
# overrides. Omitting the section keeps watcher on with a 500 ms debounce.
[watch]
enabled = true
debounce_ms = 500

# Transport for `groove serve`. `kind = "stdio"` (default)
# supports one client at a time; `kind = "http"` (Streamable HTTP) allows
# many simultaneous clients at `/mcp`. `/healthz` returns 200 for health
# checks. CLI `--transport http --port 3100` overrides.
[transport]
kind = "http"

[transport.http]
bind = "127.0.0.1:3100"
# allowed_hosts = ["kb.example.lan", "192.168.1.10"]  # opt-in for LAN exposure (v0.5.0+)
# Browser Origin allow-list for /mcp (the admin routes have no Origin check).
# Unlike allowed_hosts this is ON by default: the MCP specification requires
# Origin validation, so omitting the key accepts the loopback origins for the
# bind port. Set it when a browser-based client reaches the server through a
# proxy, since it then sends your public origin. Requests with no Origin
# (ordinary MCP clients, the tray, curl) pass either way; an empty list disables
# validation and warns at startup.
# Two spelling rules, both stricter than they look. Every entry must carry a
# scheme -- unlike allowed_hosts, which takes a bare host or host:port -- and
# groove refuses to start on one that does not, because an entry it cannot
# parse would be dropped at match time and leave the check refusing every
# browser. And an entry with no port matches EVERY port on that host, so write
# the port unless you mean the scheme's default (https://kb.example.com is 443).
# allowed_origins = ["https://kb.example.com"]
# Whether /healthz sits outside the allowed_hosts check. Default true (public,
# no Host check). Set false to have /healthz validated like every other
# endpoint, so a request whose Host header is not on the allow-list gets 403
# instead of 200 (v0.7.5+). Not authentication: the Host header is chosen by
# the caller, so anyone who can reach the port and sends an allowed value still
# gets a 200. It raises the bar for incidental probes, nothing more.
# healthz_public = false
# How many MCP sessions may be alive at once. Default 256 (~25 MB; a live
# session costs about 100 KB). While it is full, a request that would open a
# NEW session gets 429 with a Retry-After header, and established sessions
# keep working. 0 disables the limit. Concerns MCP 2025-11-25 and older only:
# the 2026-07-28 protocol has no sessions, so those requests hold nothing and
# are never refused by this limit (v0.19.0+).
# max_sessions = 256

# Optional: `groove eval` (retrieval quality evaluation, power-user feature).
# You only need this section if you run `groove eval` for tuning or
# regression tracking. Omit the section entirely for built-in defaults.
# [eval]
# golden = ".groove-eval.yml"             # default: <kb_path>/.groove-eval.yml
# history_size = 10                       # default: 10
# k_values = [1, 5, 10]                   # default: [1, 5, 10]
# regression_threshold = 0.05             # default: 0.05

# Optional: `search` tool tuning (v0.3.0+). Omit the section for defaults.
# [search]
# # rank-based low-confidence flag: trips when
# # top1.score / mean(top-N.score) < min_confidence_ratio.
# # 0.0 disables the flag. CLI `--min-confidence-ratio` and the MCP
# # param `min_confidence_ratio` override per query. Must be finite and
# # >= 0.0 — a non-finite value compares false against every score and
# # would disable the flag rather than tighten it, so groove refuses to
# # start with one.
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

# Optional: RRF / bm25 fusion parameters (v0.13.0+). Defaults shown.
# Leave them alone unless `groove tune` says otherwise on your own KB.
# [search.fusion]
# rrf_k = 60.0                # >= 1.0; lower favors a single retriever's top hit
# bm25_heading_weight = 2.0   # >= 0.0
# bm25_context_weight = 1.0   # >= 0.0
# bm25_content_weight = 1.0   # >= 0.0

# Optional: static Contextual Retrieval (v0.12.0+). Off by default; strongly
# recommended only when a reranker is also configured (see "Contextual
# Retrieval" in usage.md for why).
# [contextual]
# enabled = true
```

With the file in place `groove serve` / `index` / `status` / `graph` / `search` all work without any of those flags. Unknown keys are rejected to catch typos early. `FASTEMBED_CACHE_DIR` from the real environment overrides the file entry.

## Config file discovery

`groove` looks up `groove.toml` in the following order on every invocation
and stops at the first hit:

| Priority | Location                                  | Notes                                        |
| -------- | ----------------------------------------- | -------------------------------------------- |
| 1        | `--config <PATH>` (any subcommand)        | Errors out if the file does not exist.       |
| 2        | `./groove.toml` (current working dir)     | Most natural for project-local KBs.          |
| 3        | `<git-root>/groove.toml` (walks up)       | Checks CWD + up to 19 ancestors (20 dirs total). |
| 4        | `<binary-dir>/groove.toml`                | Legacy / global-install fallback.            |
| 5        | (no config — built-in defaults)           | `--kb-path` becomes mandatory on the CLI.    |

`~` in `--config` is expanded to the home directory on all platforms
(including Windows `cmd.exe` where the shell does not expand it).

The chosen file is logged to stderr at startup as
`grooveseek::config: loaded config source=... path=... trust=...`, so you can see
which file is in effect and how far it was trusted.

#### Trusted and untrusted config locations

Priorities 2 and 3 find a file you did not name. If you `cd` into a repository
someone else wrote — or an MCP client launches the server with that directory
as its cwd — that file would otherwise be honoured in full. So groove decides
**from the location alone** (never from the file's contents) whether to treat
it as yours:

- **Trusted**: `--config` (you named it), `<binary-dir>` (writing there needs
  install-directory access), a config home used by `groove service install`,
  and "no file at all".
- **Untrusted**: anything else found under the cwd or a `.git` ancestor.

An untrusted config still loads, and everything that shapes *how* a knowledge
base is presented — `[search]`, `[quality_filter]`, `exclude_dirs`,
`[parsers]`, `[watch]`, `[contextual]` — is honoured unchanged. Four fields
are restricted, because they decide which code runs, what leaves the machine,
and who can reach it:

| Field | From an untrusted config |
| --- | --- |
| `fastembed_cache_dir` | Ignored with a warning; the standard cache directory is used. It selects which `.onnx` file is loaded, and nothing verifies a model already present in a cache directory. (Related: `FASTEMBED_CACHE_DIR` must be an absolute path, and the model directory is never resolved relative to the working directory.) |
| `[transport.http].bind` | A non-loopback address keeps its port and moves to `127.0.0.1`, with a warning. `allowed_hosts`, `allowed_origins`, `healthz_public`, and `max_sessions` are dropped — the first three restore the loopback-only defaults, and the last falls back to the built-in limit, so that a planted `max_sessions = 1` cannot leave the server unable to accept a second client. Dropping `allowed_origins` matters in both directions: a planted list could name an attacker's origin, or be empty, which is how "do not validate Origin at all" is spelled. `kind` is honoured. |
| `kb_path` | **Ignored with a warning** if it is a filesystem root, your home directory, an ancestor of it, or an ancestor of the directory holding the config file. `--kb-path` still applies, so you can override it; with neither, the command stops with the usual "`--kb-path` is required". |
| `grammar_dir` | Ignored with a warning; the standard location is used. It selects which native library is `dlopen`ed into the process, and a grammar plugin is code, not data. Set for every untrusted config, present or not — omitting the key would otherwise be a way to influence the choice by saying nothing. If no standard location can be determined the key is dropped instead, and a command that needs a plugin then stops with a message naming `GROOVE_GRAMMAR_DIR`. |

The `kb_path` rule bounds rather than confines: `kb_path = "./docs"` and
`kb_path = "/srv/kb/knowledge-base"` are fine, so a project-local
`groove.toml` naming an absolute path keeps working. What it refuses are the
paths that can be written without knowing anything about your machine —
`../..`, `/`, `C:\Users` — and symlinks pointing at them.

To accept a config in full, name it: `groove serve --config ./groove.toml`.

Installed services are unaffected. Since v0.20.0 `groove service install` puts
`--config <config home>/groove.toml` into the unit, plist or scheduled task it
registers, so the daemon names its own config rather than discovering it — and
is therefore trusted whatever the environment looks like at start-up. That
closes the one case where it was not: setting `GROOVE_CONFIG_HOME` for the
`service install` command alone, since the variable is no longer in the
environment when the service later runs.

A service registered by an earlier version keeps its old launch line. To update
it, re-run **your own** `groove service install` command with `--force` added —
a bare `service install` would reset the service name, auto-start and bind.

**Set `GROOVE_CONFIG_HOME` again if you set it the first time.** It is not
remembered anywhere: `service install` resolves the config home from the
environment it is run in, so without the variable the re-install writes a
*different*, minimal config and points the service at that one, leaving your
real settings unused. This applies to precisely the people the fix is for.

On Linux and macOS the re-install restarts the service, so the new launch line
takes effect immediately — on Linux that holds for a `--no-auto-start` service
someone started by hand too, which is restarted only if it is actually running.
Two cases still need a manual restart: **Windows**, where the scheduled task is
re-registered but the detached daemon is not stopped, and **a `--no-auto-start`
LaunchAgent that is already loaded on macOS**, which the installer deliberately
does not touch. Sign out and back in, or stop and start the service yourself.

**What this does not cover**: if a repository ships its own `.mcp.json`, it
controls the whole command line, not just the config file. No rule inside
groove can help there; that is what your MCP client's approval prompt is for.

### Example: per-project KB packaged in a repository

```jsonc
// repo-root/.mcp.json
{
  "mcpServers": {
    "kb": { "command": "groove", "args": ["serve"] }
  }
}
```

Commit `groove.toml` next to `.mcp.json`. Opening the project in Claude Code
launches `groove serve` from the repo root, the CWD lookup picks up
the project's `groove.toml`, and `.mcp.json` stays minimal.

### Example: multiple KBs in the same Claude Code session

```jsonc
{
  "mcpServers": {
    "kb-personal": { "command": "groove", "args": ["serve", "--config", "~/kb/personal/groove.toml"] },
    "kb-project":  { "command": "groove", "args": ["serve", "--config", "./groove.toml"] },
    "kb-rust-docs":{ "command": "groove", "args": ["serve", "--config", "~/kb/rust-docs/groove.toml"] }
  }
}
```

Each entry runs as an independent MCP server with its own `groove.toml` and
its own `.groove.db`, so Claude can disambiguate by server name.

## Related

- `docs/usage.md` — the CLI flags these defaults back
- `docs/behavior.md` — what the indexing keys actually do
- `README.md` — install and quick start
