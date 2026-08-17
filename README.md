# GrooveSeek

MCP server for semantic search over a Markdown / plain-text knowledge base. The
command is `groove`.

Parses Markdown (and optionally `.txt` / `.pdf` / `.docx` / `.xlsx` / `.pptx`) files with YAML frontmatter, splits them into heading-based chunks, generates embeddings with a selectable model (BGE-small-en-v1.5 by default, BGE-M3 for multilingual/Japanese knowledge bases), and stores everything in SQLite with sqlite-vec for vector similarity search. Connects to Claude Code, Cursor, or any MCP-compatible client via stdio (default, 1 client) or Streamable HTTP (many clients) transport.

A live-sync file watcher keeps the index fresh on manual edits, `git pull`, and external scripts; an optional TOML schema can validate frontmatter conventions via `groove validate`.

> **日本語版**: [README.ja.md](./README.ja.md)

**Versioning**: releases before 1.0.0 are beta and carry no compatibility
guarantee. From 1.0.0, what this project promises not to break — and what it
deliberately does not promise, including the web interface and the Rust API — is
written down in [docs/stability.md](./docs/stability.md).

## Install

### Pre-built binaries (recommended for non-Rust users)

Download the archive for your platform from the [latest GitHub release](https://github.com/alphabet-h/grooveseek/releases/latest), extract it, and place `groove` (or `groove.exe` on Windows) somewhere on `PATH`. Available targets:

| Platform | Archive |
| --- | --- |
| Linux x86_64 (glibc 2.38+ / Ubuntu 24.04+ / Debian 13+ / RHEL 9.5+) | `grooveseek-x86_64-unknown-linux-gnu.tar.xz` |
| Linux aarch64 (glibc 2.38+) | `grooveseek-aarch64-unknown-linux-gnu.tar.xz` |
| macOS Apple Silicon | `grooveseek-aarch64-apple-darwin.tar.xz` |
| Windows x86_64 (Windows 10+) | `grooveseek-x86_64-pc-windows-msvc.zip` |

> **Intel Mac (`x86_64-apple-darwin`)** is not shipped as a prebuilt: the upstream ONNX Runtime crate (`ort-sys`) does not provide a binary for that target. Build from source as described below.

> **Windows: two more archives, if you will run groove as a service.** They are separate downloads (v0.14.0+), and both belong in the same directory as `groove.exe`:
>
> | Archive | Why |
> | --- | --- |
> | `groove-svc-x86_64-pc-windows-msvc.zip` | `groove service install` points the logon task at `groove-svc.exe` when it sits next to `groove.exe`, and **falls back to a console-visible launcher when it does not** — which means a console window flashes at every login. The fallback is reported as a warning, but extracting this archive before running `service install` saves you the second install. |
> | `groove-tray-x86_64-pc-windows-msvc.zip` | Optional. The system tray monitor; needed only for `service install --with-tray`. |

Each archive ships the binary plus `CHANGELOG.md`, `LICENSE-MIT`, `LICENSE-APACHE`, and `README.md`. Verify the SHA-256 checksum (each release exposes `sha256.sum` and per-archive `*.sha256` files) before running.

ONNX runtime and SQLite are statically linked into the binary, so no extra DLLs are required. Embedding models (ONNX) are downloaded from HuggingFace on first run — see [Working around HuggingFace TLS failures](docs/clients.md#working-around-huggingface-tls-failures-on-first-download) if your network blocks that.

### Build from source

```bash
cargo build --release
```

The binary is produced at `target/release/groove` (or `groove.exe` on Windows).

## Quick start

Index a knowledge base, then point an MCP client at it:

```bash
groove index --kb-path /path/to/knowledge-base
```

```json
{
  "mcpServers": {
    "ai-knowledge": {
      "command": "/path/to/groove",
      "args": ["serve", "--kb-path", "/path/to/knowledge-base"],
      "type": "stdio"
    }
  }
}
```

That goes in `.mcp.json` in your project root, or the equivalent MCP config for
your client. You can also query without a client at all:

```bash
groove search "semantic chunking" --kb-path /path/to/knowledge-base --limit 3
```

For a Japanese or otherwise multilingual knowledge base, pass `--model bge-m3`
to `index` and `serve` alike. It is a larger download and a different index, so
it is worth choosing before you build one — the trade-off is in
[docs/usage.md](docs/usage.md).

## Documentation

| Page | What is in it |
| --- | --- |
| [docs/usage.md](docs/usage.md) | Every command: `index`, `serve`, `search`, `graph`, `validate`, `doctor`, `eval`, `tune`, `service` |
| [docs/configuration.md](docs/configuration.md) | Every `groove.toml` key, the discovery order, and which locations are trusted |
| [docs/clients.md](docs/clients.md) | `.mcp.json` recipes, the HTTP transport, the PostToolUse hook, the file watcher |
| [docs/mcp-tools.md](docs/mcp-tools.md) | The MCP surface: tools, prompts, and `kb://` resources |
| [docs/behavior.md](docs/behavior.md) | What gets indexed, where it is stored, and which files are refused |
| [docs/retrieval-pipeline.md](docs/retrieval-pipeline.md) | RRF, reranking, MMR, and parent retriever, in the order they run |
| [docs/filters.md](docs/filters.md) | Narrowing search results |
| [docs/citations.md](docs/citations.md) | `match_spans` and byte offsets, for quoting sources accurately |
| [docs/eval.md](docs/eval.md) | Measuring retrieval quality against a golden query set |
| [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) | Source layout, and how a query flows through it |
| [docs/stability.md](docs/stability.md) | What 1.0.0 freezes, and what it deliberately does not |

A Japanese version of every page above sits alongside it as `*.ja.md`.
Ready-to-adapt deployment recipes — personal stdio, NAS-shared, intranet HTTP —
are in [`grooveseek/examples/deployments/`](./grooveseek/examples/deployments/).

Those are repository paths, and a release archive contains this README but not
`docs/`. If you installed from an archive, read them at
<https://github.com/alphabet-h/grooveseek/tree/main/docs>, swapping `main` for
your release tag to get the version you installed.

## MCP surface

Six tools — `search`, `get_document`, `list_topics`, `get_connection_graph`,
`get_best_practice`, `rebuild_index` — four prompts, and the knowledge base
itself exposed as `kb://` resources. Parameters and return shapes are in
[docs/mcp-tools.md](docs/mcp-tools.md).

## Design decisions

Decisions that shaped the architecture — what was chosen, which alternatives were rejected, and what it cost — are recorded as [Architecture Decision Records](docs/decisions/) in `docs/decisions/`. Start with [ADR-0000](docs/decisions/0000-record-decisions-as-adrs.md), which describes when a decision is recorded and when a changelog entry is enough. Japanese versions are alongside as `*.ja.md`.
