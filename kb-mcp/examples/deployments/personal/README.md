# Deployment recipe — personal local

> **日本語版**: [README.ja.md](./README.ja.md)

Single user, single machine, local knowledge base. The most common setup
and the simplest. Everything stays on your laptop / desktop, the file
watcher keeps the index in sync, and Claude Code launches kb-mcp via
stdio.

## Target environment

- One developer / writer using one machine.
- Knowledge base is a local directory (Obsidian vault, project notes,
  research dump — whatever).
- Claude Code, Cursor, or any other MCP client running on the same
  machine connects to kb-mcp over stdio.

## What's in this directory

| File | Purpose |
| --- | --- |
| [`kb-mcp.toml`](./kb-mcp.toml) | Server-side defaults: model, watcher, parsers, quality filter |
| [`.mcp.json`](./.mcp.json) | Client-side stub: `kb-mcp serve` (no args — discovery picks up the toml) |

## Setup

1. **Install kb-mcp**. Either grab a [prebuilt binary](https://github.com/alphabet-h/kb-mcp/releases/latest) and place it on `PATH`, or `cargo install --path .` from a clone.
2. **Decide where the KB lives**. For example `~/notes/` (personal notes) or `~/projects/<repo>/docs/` (project-scoped).
3. **Pick a config location**. Two natural options — see [Config file discovery](../../../README.md#config-file-discovery):
   - **Project-scoped**: drop both `kb-mcp.toml` and `.mcp.json` next to your project (commit them — `kb-mcp.toml` is meant to be shared).
   - **Global**: place `kb-mcp.toml` next to the binary (`~/.local/bin/kb-mcp.toml` or `%USERPROFILE%\bin\kb-mcp.toml`) so every project sees the same defaults.
4. **Edit `kb-mcp.toml`**: set `kb_path` to the absolute path of your KB. Adjust the model and reranker if the defaults don't match your language.
5. **Build the initial index**:

   ```bash
   kb-mcp index --kb-path /absolute/path/to/kb
   ```

   First run downloads the ONNX model. Subsequent runs are incremental (SHA-256 diff).
6. **Connect from Claude Code**: copy `.mcp.json` into your project root (or `~/.config/claude/.mcp.json` for global usage).

## Operational notes

- **Watcher** is on by default. Edits to your `.md` files (manual save / `git pull` / external scripts) are detected and re-indexed automatically within ~500 ms.
- **PostToolUse hook** is optional and complementary — see [`examples/hooks/`](../../hooks/). The watcher already covers manual edits; the hook is mainly useful when you want zero-latency rebuild after Claude itself writes files.
- **Reranker** is loaded but off by default. Enable per-query with `rerank: true` in the MCP `search` call when you need it; the latency cost (~300-700 ms on CPU) is not worth paying for every search.
- **Single client per server**. stdio only supports one MCP client at a time — fine for solo use; for multiple clients see [`intranet-http/`](../intranet-http/).
- **`alwaysLoad: true`** in the example `.mcp.json` is a Claude Code v2.1.121+ option that forces kb-mcp's tools to be present at initial load instead of going through the tool-search shortlist. Recommended for RAG use ("I want to search anytime"). Drop it if first-startup latency (model download / index open) outweighs the win, or if your client predates v2.1.121. Other MCP clients ignore the field.

## When to step up to another recipe

- You want to share the KB with a teammate → [`nas-shared/`](../nas-shared/) or [`intranet-http/`](../intranet-http/).
- You run multiple Claude Code sessions in parallel against the same KB → [`intranet-http/`](../intranet-http/).
- Your KB is on a network share → [`nas-shared/`](../nas-shared/).
