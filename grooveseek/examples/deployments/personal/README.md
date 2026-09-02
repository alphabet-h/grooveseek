# Deployment recipe — personal local

> **日本語版**: [README.ja.md](./README.ja.md)

Single user, single machine, local knowledge base. The most common setup
and the simplest. Everything stays on your laptop / desktop, the file
watcher keeps the index in sync, and Claude Code launches groove via
stdio.

## Target environment

- One developer / writer using one machine.
- Knowledge base is a local directory (Obsidian vault, project notes,
  research dump — whatever).
- Claude Code, Cursor, or any other MCP client running on the same
  machine connects to groove over stdio.

## What's in this directory

| File | Purpose |
| --- | --- |
| [`groove.toml`](./groove.toml) | Server-side defaults: model, watcher, parsers, quality filter |
| [`.mcp.json`](./.mcp.json) | Client-side stub: `groove serve --config ./groove.toml` |

## Setup

1. **Install groove**. Either grab a [prebuilt binary](https://github.com/alphabet-h/grooveseek/releases/latest) and place it on `PATH`, or `cargo install --path grooveseek` from a clone (the repository root is a workspace manifest, so `--path .` fails).
2. **Decide where the KB lives**. For example `~/notes/` (personal notes) or `~/projects/<repo>/docs/` (project-scoped).
3. **Pick a config location**. Two natural options — see [Config file discovery](../../../../docs/configuration.md#config-file-discovery):
   - **Project-scoped**: drop both `groove.toml` and `.mcp.json` next to your project (commit them — `groove.toml` is meant to be shared). **The `.mcp.json` here names the file with `--config`, and that is not a style choice**: a `groove.toml` groove only *found* is honoured in part, and `[parsers]` is one of the keys reset to its default, so a project-scoped config that opts into `.txt`, PDFs or source code would silently index Markdown alone. See [Trusted and untrusted config locations](../../../../docs/configuration.md#trusted-and-untrusted-config-locations).
   - **Global**: place `groove.toml` next to the binary (`~/.local/bin/groove.toml` or `%USERPROFILE%\bin\groove.toml`) so every project sees the same defaults. The binary's own directory is trusted, so `--config` is not needed for this one.
4. **Edit `groove.toml`**: set `kb_path` to the absolute path of your KB. Adjust the model and reranker if the defaults don't match your language.
5. **Build the initial index**:

   ```bash
   groove index --kb-path /absolute/path/to/kb
   ```

   First run downloads the ONNX model. Subsequent runs are incremental (SHA-256 diff).
6. **Connect from Claude Code**: copy `.mcp.json` into your project root (or `~/.config/claude/.mcp.json` for global usage).

## Operational notes

- **Watcher** is on by default. Edits to your `.md` files (manual save / `git pull` / external scripts) are detected and re-indexed automatically within ~500 ms.
- **PostToolUse hook** is optional and complementary — see [`examples/hooks/`](../../hooks/). The watcher already covers manual edits; the hook is mainly useful when you want zero-latency rebuild after Claude itself writes files.
- **Reranker** is not configured in this recipe — the `reranker` key in `groove.toml` is commented out so the first run does not pull a ~2.3 GB model. Until you uncomment it, `rerank: true` on a `search` call is a **silent no-op** (the server only reranks when a reranker was loaded at startup). Once it is enabled, keep `rerank_by_default = false` and opt in per query: on CPU a reranked search takes tens of seconds where an ordinary one takes a fraction of one, which is not a cost to pay on every search. [usage.md](../../../../docs/usage.md#when-to-enable-reranking) carries the measurement and the conditions it was taken under.
- **Single client per server**. stdio only supports one MCP client at a time — fine for solo use; for multiple clients see [`intranet-http/`](../intranet-http/).
- **`alwaysLoad: true`** in the example `.mcp.json` is a Claude Code v2.1.121+ option that forces groove's tools to be present at initial load instead of going through the tool-search shortlist. Recommended for RAG use ("I want to search anytime"). Drop it if first-startup latency (model download / index open) outweighs the win, or if your client predates v2.1.121. Other MCP clients ignore the field.

## When to step up to another recipe

- You want to share the KB with a teammate → [`nas-shared/`](../nas-shared/) or [`intranet-http/`](../intranet-http/).
- You run multiple Claude Code sessions in parallel against the same KB → [`intranet-http/`](../intranet-http/).
- Your KB is on a network share → [`nas-shared/`](../nas-shared/).
