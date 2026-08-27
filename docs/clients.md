# Connecting to Claude Code / Cursor

How to point an MCP client at GrooveSeek — `.mcp.json` over stdio, the
HTTP transport for several clients at once, and the pieces around them.

> **日本語版**: [clients.ja.md](./clients.ja.md)

> **Looking for full deployment recipes?** See [`grooveseek/examples/deployments/`](https://github.com/alphabet-h/grooveseek/tree/main/grooveseek/examples/deployments) for ready-to-adapt configs covering three patterns: personal stdio, NAS-shared (one writer + many read-only clients), and intranet HTTP server (one server + many clients). For a single-machine loopback daemon shared by several parallel Claude Code sessions, use `groove service install` — it replaced the former `personal-http` recipe in v0.8.0. The snippets below are the canonical stdio entry point you'll find in those recipes.

Add the following to `.mcp.json` in your project root (or the equivalent MCP config for your client):

<!-- groove-pin: mcp-stdio-snippet -->
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

With a multilingual model and reranker enabled:

```json
{
  "mcpServers": {
    "ai-knowledge": {
      "command": "/path/to/groove",
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
      "command": "/path/to/groove",
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

Or, if you placed a `groove.toml` somewhere on the [discovery path](configuration.md#config-file-discovery) with those options set, the `.mcp.json` can shrink to:

```json
{
  "mcpServers": {
    "ai-knowledge": {
      "command": "/path/to/groove",
      "args": ["serve"],
      "type": "stdio"
    }
  }
}
```

The server will be started automatically when the client connects.

## Keeping the index fresh via PostToolUse hook
If you edit the knowledge base from inside a Claude Code session (or run a skill that writes Markdown files), the running MCP server will keep returning stale results until the index is rebuilt. A `PostToolUse` hook in `.claude/settings.json` can re-index automatically after every write. Minimal form:

<!-- groove-pin: posttooluse-hook-snippet -->
```json
{
  "hooks": {
    "PostToolUse": [
      {
        "matcher": "Write|Edit|MultiEdit|Skill",
        "hooks": [
          { "type": "command", "command": "groove index" }
        ]
      }
    ]
  }
}
```

SHA-256 diffing in `groove index` makes the second-and-later invocations fast (usually sub-second on small KBs). A richer shell script that inspects the tool payload and only rebuilds when the edited file is under `$KB_PATH` ships with the repo: see [`grooveseek/examples/hooks/`](https://github.com/alphabet-h/grooveseek/blob/main/grooveseek/examples/hooks/README.md). SQLite runs in WAL mode so the hook can safely run while the MCP server is still up.

## Frontmatter schema validation
If your knowledge base follows a frontmatter convention (e.g. `title` required, `date` is YYYY-MM-DD, `topic` limited to an enum), you can check every `.md` file for violations with:

```bash
groove validate --kb-path /path/to/knowledge-base
```

Put a `groove-schema.toml` at the root of `--kb-path` (template: `groove-schema.toml.example`):

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

## HTTP transport for multiple simultaneous clients
By default `groove serve` speaks MCP over stdio — one client per server process. To serve multiple clients simultaneously (e.g. several Claude Code sessions or an external script hitting the same index), switch to Streamable HTTP:

```bash
groove serve --kb-path /path/to/knowledge-base --transport http --port 3100
# or, to accept connections from outside this machine: --bind 0.0.0.0:3100 --i-know
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
- Default bind is `127.0.0.1:3100` (loopback). **groove has no built-in authentication**, so the bind address is the only access control — use `--bind 0.0.0.0:3100` on trusted networks only. Since v0.17.0 a non-loopback `--bind` is refused unless you add `--i-know`, matching `groove service install`. A non-loopback address coming from `[transport.http].bind` in `groove.toml` is **not** gated — existing service deployments keep working — and it warns at startup only when the Host allow-list is missing or empty (see the next two bullets). Writing an explicit `allowed_hosts` list is taken as a statement of intent, so that combination is silent by design.
- **GrooveSeek validates the `Host` header itself**, loopback-only by default, to prevent DNS rebinding attacks; rmcp's own check is switched off so that one gate answers wherever the check runs ([ADR-0009](decisions/0009-one-dns-rebinding-gate.md)). **Host validation is not authentication** — any peer that can reach the port may send `Host: localhost`. Treat it as a browser-side defence, and restrict reachability at the network layer.
- For LAN / intranet exposure, set `[transport.http].allowed_hosts` in `groove.toml` to your public hostnames / IPs (e.g. `["kb.example.lan", "192.168.1.10"]`). Binding to a non-loopback address with the default loopback-only allow-list means external requests are 403'd by Host validation; groove emits a `tracing::warn` at startup when this misconfiguration is detected. An empty `allowed_hosts = []` disables the check entirely, which combined with a non-loopback bind leaves `/mcp` open to every peer that can reach the port — that combination now warns at startup too.

- **`Origin` validation is on by default**, unlike `allowed_hosts`. The MCP specification states that a Streamable HTTP server *"MUST validate the `Origin` header on all incoming connections to prevent DNS rebinding attacks"*, so omitting `[transport.http].allowed_origins` accepts the loopback origins for the bind port (`http://localhost:PORT`, `http://127.0.0.1:PORT`, `http://[::1]:PORT`) rather than accepting everything. Requests that carry no `Origin` header — every ordinary MCP client, the tray, `curl` — pass regardless, per RFC 6454; the check exists to stop a web page open in your own browser from reaching the port, and it is **not** a second access control. Behind a reverse proxy a browser-based client sends your public origin, so name it explicitly. **Setting the key replaces the default list rather than extending it**, so keep the loopback entries too if browser clients also reach you over loopback: `allowed_origins = ["https://kb.example.com", "http://127.0.0.1:3100", "http://localhost:3100"]`. An empty list disables validation and warns at startup.

- **`Origin` validation covers `/mcp`, and `/ui` searches through `/mcp`.** So this list decides whether the built-in page can search: replace the default with only a public origin and `/ui` is still served but every query it makes is refused. `allowed_hosts` does the same one step earlier — Host validation runs first, so the documented LAN recipe (`allowed_hosts = ["kb.example.lan"]`) refuses the `Host: localhost` a locally opened `/ui` sends. Either key **replaces** its default rather than extending it, so list the exact names and origins you browse with — `allowed_hosts = ["127.0.0.1"]` still refuses a page opened through `localhost`. The server warns at startup when a list has no loopback entry at all, but not when it has one that does not match the address you use; `/ui` reports the host and origin it needs when a search is refused. **One check answers this wherever it runs** — GrooveSeek performs it itself rather than leaving `/mcp` to rmcp, so two surfaces can no longer read the same value two different ways; [ADR-0009](decisions/0009-one-dns-rebinding-gate.md) records the five `Host` spellings on which they used to, and why `/mcp` now refuses them. **What each route compares against still differs**: this key reaches `/mcp` and the admin routes, the admin routes match `Host` against a loopback-only list of their own, and `/healthz` validates `Host` only, and only when `healthz_public = false`. Requests carrying no `Origin` pass wherever the check runs, which is what the page's own status poll and the tray send.
- Concurrent requests queue on the server's locks: every `search` takes the embedder, reranker and database mutexes, and holds the last two for the rest of the pipeline. Measured with eight clients at once (`cargo test -p grooveseek --release --test http_lock_contention -- --ignored`), `search` throughput goes from ~7 to ~9 qps on a 9,800-chunk corpus and from ~12–16 to ~13–20 qps on an 800-chunk one, while latency grows about linearly with the number of clients. The table, and why a lock refactor would buy little while one embedding already uses every core, are in [deployment-topologies.md](deployment-topologies.md#concurrent-clients-measured).

## Web UI and admin API (HTTP transport only)

Running `serve` with `--transport http` mounts two more routes beside `/mcp`
and `/healthz`. Nothing enables them — they exist whenever the HTTP transport
does — and both are **loopback-only**: the middleware rejects any request
whose peer address is not loopback, then checks the `Host` header against the
loopback aliases (`127.0.0.1`, `::1`, `localhost`) — plus the bind address, but
only when that is itself loopback. Bind to `0.0.0.0` and `Host: 0.0.0.0` is
rejected too, deliberately: a LAN browser must not reach these routes through
the bind address. A machine elsewhere on the network gets 403 even if you
allow-listed its Host for `/mcp`. `Origin` is checked after the `Host`, against
`[transport.http].allowed_origins`.

Both routes answer with `X-Content-Type-Options: nosniff` and a
`Content-Security-Policy` of `default-src 'none'` plus exactly what the page
uses: its own inline `<script>` and `<style>`, its `data:` favicon, and
same-origin `fetch`. Nothing external loads from `/ui`, and the policy is what
keeps that true.

| Route | What it is |
| --- | --- |
| `/ui` | The operator's view: a status band (version, documents, chunks, model, watcher, uptime, pid) over a search box. It searches by calling **`/mcp`**, which makes the page the smallest working example of an MCP client over Streamable HTTP. |
| `/api/admin/status` | Daemon / indexing / watcher / KB status as JSON. This is what the Windows tray polls every 5 seconds, and what the band above reads. |

> **`/api/search` was removed in v0.27.0.** It accepted 2 of the 17 parameters the `search` tool takes, so `/mcp` was already the better endpoint for anything outside the process; `/ui` uses `/mcp` now. See [docs/stability.md](stability.md).

```bash
curl http://127.0.0.1:3100/api/admin/status
```

```json
{
  "daemon":   { "version": "0.13.1", "pid": 36400, "uptime_secs": 4210, "started_at": "2026-07-26T09:12:03Z" },
  "indexing": { "active": false, "started_at": null, "progress": null },
  "watcher":  { "active": true, "debounce_ms": 500 },
  "kb":       { "documents": 596, "chunks": 8878, "model": "bge-m3" },
  "config_source": "Cwd"
}
```

> **`kb.path` was removed.** It carried the knowledge base's absolute path, so
> on Windows the payload — and the status band that printed it — read
> `C:\Users\<name>\...`. Nothing consumed it: the tray reads `daemon.pid` and
> `indexing.active`. See [docs/stability.md](stability.md).

`/ui` is what the Windows tray's **Open Web UI** menu item opens, but it is not
Windows-specific. On Linux or macOS, browse to it on the machine running the
daemon, or forward the port:

```bash
ssh -L 3100:127.0.0.1:3100 kb-server.lan   # then open http://127.0.0.1:3100/ui
```

Do **not** map these routes in a reverse proxy: the proxy is itself a loopback
peer and its default `Host` is allow-listed, so proxying `/ui` hands the page to
whoever can reach the proxy. Forward `/mcp` and `/healthz` only.

## Live-sync via file watcher
`groove serve` runs a `notify`-based file watcher by default. Any change under `--kb-path` (create / modify / delete / rename) is detected, debounced, and only the affected file is re-indexed. This covers manual editor saves, `git pull`, and external scripts — cases the PostToolUse hook cannot intercept.

- **Default on**. `[watch].enabled = false` in `groove.toml` or `--no-watch` on the command line disables it.
- **Debounce** is 500 ms by default. Tune with `[watch].debounce_ms` or `--debounce-ms`.
- **Coexists with the PostToolUse hook**. Both paths lock the same `Mutex<Database>` / `Mutex<Embedder>`, so concurrent triggers are serialized at the Rust layer and are idempotent.
- **Extension-aware**. The watcher shares the Parser registry with `rebuild_index`, so only files whose extension is enabled in `[parsers].enabled` are re-indexed; other events are dropped.
- **Resilience**. Errors inside the watcher task are logged to stderr (not silently dropped) and the MCP server keeps running. Local disk is assumed — inotify on WSL / SMB / network shares is not guaranteed.
- **Backpressure (v0.6.0+)**. The bridge from the debouncer to the indexer task uses a bounded 64-batch channel; if the consumer cannot keep up (e.g. embedder is paused), excess batches are dropped with a warn log instead of growing the queue indefinitely. Run `rebuild_index` manually after the burst to recover any missed events.

## Placing a grammar plugin (v1.3.0+)

groove parses source code one definition at a time, but only Rust is compiled into the binary. Every other language is a small library you download and place — that asymmetry, and why it is not a feature flag, is [ADR-0013](decisions/0013-compile-in-one-grammar-and-load-the-rest.md).

1. Find the archive named `groove-grammar-<language>-<target>` for **your groove version** on the [releases page](https://github.com/alphabet-h/grooveseek/releases). The plugin and the binary share an ABI version, so a plugin from a different release may be refused.
2. Unpack it and put the library in the grammar directory. The default is `%LOCALAPPDATA%\groove\grammars` on Windows, `~/.local/share/groove/grammars` on Linux, and `~/Library/Application Support/groove/grammars` on macOS. To use a different one, set `grammar_dir` in `groove.toml`, or the `GROOVE_GRAMMAR_DIR` environment variable — which must be an absolute path, because a relative one would resolve against whatever directory the client happened to launch groove from.
3. Add the language to `[parsers].enabled`, e.g. `enabled = ["md", "py"]`.
4. **Run `groove doctor` once by hand before letting a service do it.** A registered Windows service discards stdio, so if the plugin is missing or refused, the message saying so goes nowhere and the daemon simply does not work. Running the command yourself puts that message on your screen.

Nothing is downloaded automatically and nothing but the enabled languages is opened — a file in that directory belonging to a language you did not enable is never touched. If an enabled language has no usable plugin, the command stops and says which file it wanted and where; it does not fall back to indexing the source as plain text.

> **A grammar plugin is native code that groove loads into its own process.** Treat one like any other binary you install: take it from the release page for the version you are running, and not from anywhere else. This is also why a `groove.toml` that groove merely *found* — rather than one you named with `--config` — cannot choose the directory; see [Trusted and untrusted config locations](configuration.md#trusted-and-untrusted-config-locations).

## Working around HuggingFace TLS failures on first download

Some environments (corporate proxies, firewalls with TLS inspection) reject fastembed's native TLS connection to `huggingface.co` with `os error 10054` / "Connection was reset". In that case, pre-download the model via the Python HuggingFace CLI and point `FASTEMBED_CACHE_DIR` at the HF Hub cache:

```bash
# Install once
pip install --user huggingface_hub

# Pre-download BGE-M3 (required ONNX files only)
hf download BAAI/bge-m3 \
    --include 'onnx/*' 'tokenizer*' 'config.json' 'special_tokens_map.json'

# Pre-download BGE-reranker-v2-m3 (for `--reranker bge-v2-m3`)
hf download BAAI/bge-reranker-v2-m3

# Run groove pointing at the HF cache (HF Hub cache layout is compatible with fastembed)
FASTEMBED_CACHE_DIR=~/.cache/huggingface/hub \
    groove index --kb-path ./knowledge-base --model bge-m3 --force
```

## Related

- `docs/mcp-tools.md` — what a connected client can call
- `docs/configuration.md` — the same options as `groove.toml` keys
- `README.md` — install and quick start
