# Deployment topologies

Which shape to run GrooveSeek in, what residency actually buys, and where the
same-host boundary comes from — which is not one place, but three.

> **日本語版**: [deployment-topologies.ja.md](./deployment-topologies.ja.md)

Everything below was measured against **v1.0.0** on 2026-08-22, on one Windows
machine, and the commands are in
[How the numbers were taken](#how-the-numbers-were-taken) at the end so you can
disagree with them on your own hardware. Code is cited by file and function name
rather than by line number: an earlier version of this page cited lines, and
every one of them was wrong within five days.

## Two process shapes, and a process is only ever one of them

The same `groove` binary serves both shapes, but a single process is either
stdio or HTTP and never both. `run_server` branches on the transport exactly
once, with no fallthrough, and the comment at that branch says outright that the
two arms are mutually exclusive.

### Spawned by the client — stdio

```
  same machine, same user
  ┌───────────────────────────────┐
  │  MCP client                   │
  │  (Claude Code / Cursor / …)   │
  │            │                  │
  │            │ spawn + stdin/stdout
  │            ▼                  │
  │  groove serve                 │
  └───────────────────────────────┘
     no socket is opened
```

One process, one client, for as long as the client lives. Being on the same host
is **structural rather than configured**: a child process cannot be placed on
another machine, so `Host` validation, `Origin` validation and authentication
never become questions at all.

### Left running — HTTP

```
  ┌──────────────┐  ┌──────────────┐  ┌──────────────┐
  │ MCP client×N │  │ browser /ui  │  │ groove-tray  │
  └──────┬───────┘  └──────┬───────┘  └──────┬───────┘
         └─────────────────┼─────────────────┘
                           ▼  TCP (default 127.0.0.1:3100)
                  groove serve --bind …
              one model, one index, shared
```

Many clients share one loaded model and one index. Two bounds apply here and
nowhere else.

**A request body of at most 1 MiB** (`REQUEST_BODY_MAX_BYTES`). This is a
`tower_http` layer rather than axum's `DefaultBodyLimit`, because the latter does
not reach a service that reads its own body — which `rmcp` does.

**A session cap, defaulting to 256** (`DEFAULT_MAX_SESSIONS`), settable through
`[transport.http].max_sessions`, where `0` means unlimited. While the cap is
reached, a request that would open a *new* session is refused with 429; existing
sessions are untouched. Read it carefully before planning capacity around it:
it counts **sessions, not requests, and only sessions that actually get
created**. The gate in front of `/mcp` looks solely at requests arriving to open
one — a POST, carrying no `Mcp-Session-Id`, whose body is a single `initialize`.
MCP 2026-07-28 removed sessions altogether, so a client of that protocol calling
`tools/call` directly never creates one and never occupies a seat. The cap
therefore bounds older, stateful clients; the stateless shape recommended for a
PHP application below is not subject to it at all.

### Three things that hold in both shapes

**The file watcher runs in both.** It is spawned before the transport branch, in
the same function, so a stdio child watches its knowledge base exactly as a
resident daemon does. *Keeping a daemon up is not a way to get live indexing* —
you already have it. (Watching is on by default, not unconditionally: `--no-watch`
and `[watch].enabled` turn it off.)

**One process, one model.** The embedder is built once, before the branch, and
shared through an `Arc<Mutex<_>>`. A second process gets its own. So a resident
daemon plus an editor-spawned child means two copies in memory — roughly **2 GB
each for `bge-m3`, or ~500 MB each for the default `bge-small-en-v1.5`** (see
[usage.md](usage.md); the ~2.3 GB figure often quoted for `bge-m3` is its
*download* size, not its resident size). A configured reranker is a second model
again, in the same process; no reranker is configured by default.

**There is no cross-process guard.** No lock file, no PID file, no exclusive
database mode. SQLite is opened in WAL with a 30-second `busy_timeout`
specifically so a second process's `search` or `status` waits rather than
failing — concurrent access is anticipated and supported. What is *not* prevented
is the same indexing work running twice, in two processes, over the same
`.groove.db`.

Two things look like a guard and are not:

- **`RebuildSlot`**, which refuses a second concurrent `rebuild_index`, lives
  inside one process. A daemon and a spawned child get one slot each, so both can
  re-embed the same corpus at once.
- **The bind failure** — *"is another groove instance running, or the port
  occupied?"* — only sees another listener on the same port. It cannot see a
  stdio child, which is exactly the case described here.

### What residency buys, measured

| | model loaded | one query | corpus |
|---|---|---|---|
| `groove search` (CLI) | every invocation | **~3,000–3,500 ms** (median ~3,150) | 135 docs / 1,801 chunks |
| resident daemon, `/mcp` `tools/call` | once, at startup | **~140–290 ms** (typical ~200) | 686 docs / 9,813 chunks |

**About 15×, and never below 10×** across the measured ranges. Both sides ran
`bge-m3` with no reranker, so the only deliberate difference is corpus size — and
it runs *against* the conclusion: the slower side is the smaller corpus, by 5×
the documents and 5.4× the chunks.

Where do the CLI's three seconds go? Two of the three terms are measured; the
third is what is left over.

| Term | ms | Where the number comes from |
|---|---:|---|
| process start, config discovery, database open | ~35 | **Measured** — `groove status` against the same database and config does all of this and loads no model |
| embed the query, hybrid search, serialize | ~200 | **Measured** — it is the daemon row above, which does exactly this work with the model already loaded. Over a 5× larger corpus, so on 135 documents it is likely less |
| load the model | ~2,900 | **Derived by subtraction**, not measured directly |

The model load dominates by an order of magnitude, but it is not the whole gap,
and the `groove status` control alone cannot establish that it is: `status` skips
the model *and* the search, so the ~35 ms figure means "neither of the other two
terms", not "everything except loading".

What does not need subtraction is that a second identical query is no faster —
3,158 / 3,014 / 3,015 / 3,108 / 3,039 ms for the same query — so whatever costs
the three seconds is not something a warm cache absorbs.

**Therefore calling the CLI per request, from PHP or Node, does not work.** An
external application has to talk to a process that is already holding the model.

> **"Resident" is not the same as "always fast."** Two measured conditions cost
> 10–20×, and both are the "one process, one model" hazard above wearing a
> different hat. A first query after 2.3 hours idle took **4,616 ms**. Running a
> CLI search alongside the daemon dragged `/mcp` from ~200 ms to **~2,000 ms**,
> recovering to ~180 ms as soon as the CLI stopped — each CLI run pulls another
> copy of the model into memory, the idle daemon's working set gets trimmed, and
> the next query faults it back in. If you reproduce the table above, measure the
> two rows in **separate batches**; interleaving them collapses the ratio from
> ~15× to a meaningless ~1.6×.

### Concurrent clients, measured

One daemon holds one embedder, one reranker slot and one database connection,
each behind a mutex, and `search` takes all three — the embedder only while the
query is embedded, the other two for the rest of the pipeline. Requests are
concurrent at the HTTP layer and on tokio's blocking pool; they queue on those
locks. What that costs N clients arriving at once, on the machine of the table
above (release build, `bge-m3`, no reranker; measured with
`cargo test -p grooveseek --release --test http_lock_contention -- --ignored --nocapture`,
see below):

| corpus | tool | one client, p50 | eight clients, p50 | one client, qps | eight clients, qps |
|---|---|---:|---:|---:|---:|
| 59 docs / 794 chunks | `search` | 62–83 ms | 242–355 ms | 11.7–16.1 | 12.6–19.8 |
| | `get_connection_graph` (database lock only) | 10.6 ms | 42 ms | 92 | 98 |
| | `get_document` (no lock) | 1.0 ms | 3.2–3.7 ms | 870–1,000 | 1,870–2,130 |
| 686 docs / 9,813 chunks | `search` | 136–140 ms | 593–606 ms | 7.1–7.3 | 8.9 |
| | `get_connection_graph` | 74 ms | 297–304 ms | 13.4 | 13.4–13.5 |
| | `get_document` | 1.0 ms | 3.9–4.1 ms | 754–783 | 1,515–1,653 |

Latency grows with the number of clients — about 4.5× at eight, the median of
eight requests served one after another — and throughput barely moves. That is
not idle hardware waiting on a lock: one query embedding already runs across
every core, so a second daemon serving a copy of the same corpus raised the
combined eight-client throughput by only 12% on the small corpus and 32% on the
large one. The database side is where cores do sit idle — the graph tool keeps
one core busy and the other fifteen waiting — and its share grows with the
corpus: one hybrid candidate fetch takes 10.6 ms at 794 chunks and 79.9 ms at
9,813 (the KNN is a brute-force scan), overtaking the ~50 ms embedding at
roughly five thousand chunks. Below that, no lock refactor can raise `search`
throughput; above it, a pool of read-only connections would raise it by at most
what the second daemon showed, because the embedding's CPU is the next ceiling.
A reranked query is a different matter: it holds its lock for ~48 seconds, and a
second concurrent client waits out the whole of the first.

## Who is calling decides whether authentication is your problem

GrooveSeek provides the API; the interface a person looks at belongs to the
application in front of it. What that application is written in decides the shape.

### An MCP client

Claude Code, Cursor and VS Code speak MCP directly, either by spawning `groove`
over stdio or by posting to `/mcp` on a daemon. **If a daemon is already
running, use `/mcp`** — it avoids loading the model a second time.

### A Node application

The official TypeScript SDK's `StdioClientTransport` spawns `groove` as a child,
which is the same thing Claude Code does. No port is opened, so `Host`
validation, `Origin` validation and authentication do not arise. All six tools
and all seventeen `search` parameters are reachable ([mcp-tools.md](mcp-tools.md));
the application keeps the screen, the login, the per-user scoping and the audit
log.

### A PHP application

PHP-FPM workers are short-lived, so a worker cannot hold a child process — and
one child per worker would mean one model per worker. A PHP application talks to
a resident daemon instead:

```
  PHP-FPM app  ──cURL──▶  http://127.0.0.1:3100/mcp
```

`/mcp` accepts a **stateless POST**: one `tools/call`, with no `initialize`
handshake and no session id (MCP 2026-07-28, SEP-2567). That is what makes it
usable from a worker that will not exist a moment later.

Stateless does not mean bare, though. Three headers and a `_meta` block are what
the protocol requires, and leaving any one of them out is answered with an error
rather than a result — measured against a running v1.0.0 server: dropping
`MCP-Protocol-Version`, `Mcp-Method` or `Mcp-Name` gives **HTTP 400 with
`-32020`**, and dropping `_meta` gives **HTTP 400 with `-32602`**. The request
below returns a result:

```http
POST /mcp HTTP/1.1
Host: 127.0.0.1:3100
Content-Type: application/json
Accept: application/json, text/event-stream
MCP-Protocol-Version: 2026-07-28
Mcp-Method: tools/call
Mcp-Name: search

{
  "jsonrpc": "2.0",
  "id": 1,
  "method": "tools/call",
  "params": {
    "name": "search",
    "arguments": { "query": "semantic chunking", "limit": 5 },
    "_meta": {
      "io.modelcontextprotocol/protocolVersion": "2026-07-28",
      "io.modelcontextprotocol/clientCapabilities": {}
    }
  }
}
```

The reply is `text/event-stream` unless the server was configured for plain
JSON, so read lines until the first `data:` carrying a `result` or an `error`.

`/ui` is the working version of exactly this — `callTool` in
`grooveseek/src/transport/webui_index.html`, roughly thirty lines including the
stream reader — and is the smallest complete MCP client over Streamable HTTP in
the repository.

Splitting the application into a separate container **does** work — `/mcp` has
no loopback-peer requirement — but it is where the real decision starts, and the
decision is not about reachability. It is that GrooveSeek does not authenticate
anyone. See the next section, and
[stability.md](stability.md).

## The same-host boundary has three sources, not one

Since [ADR-0009](decisions/0009-one-dns-rebinding-gate.md), every validated
route passes through one middleware, `dns_rebinding_gate`, which asks three
questions in a fixed order — **peer, then Host, then Origin** — with each route
group handed its own state. Unifying *who answers* deliberately did not change
*what is asked*, so the three questions stay separate here. Collapsing them into
a single "same-host?" column is what made an earlier version of this page wrong
about `/mcp`.

| Route | Peer must be loopback | `Host` | `Origin` | Can configuration open it? |
|---|---|---|---|---|
| stdio | — (no socket) | — | — | No — it is a child process |
| `/ui`, `/api/admin/status` | **Yes** | `allowed_admin_hosts`: loopback aliases plus the bind address when that is loopback. No config key | shared `allowed_origins` | **No.** The peer check is not configurable at all |
| `/mcp` | No | `effective_allowed_hosts`, default as above; replaceable via `[transport.http].allowed_hosts` (config only, no CLI flag) | `effective_allowed_origins`, default the loopback origins of the bound port; replaceable via `allowed_origins` | **Yes** — see below |
| `/healthz` | No | mounted with no gate at all by default; the `/mcp` list only when `healthz_public = false` | never validated | `healthz_public` only |

`/api/search` is **not** in this table: it was removed in v0.27.0. A handler of
that name survives behind a test-only feature gate, and a test exists to assert a
shipped server does not answer it.

Only the peer column is a same-host constraint. `Host` and `Origin` are both
supplied by the caller, which makes them a defence against a browser being
tricked into DNS rebinding — **not** access control, and not a statement about
where the caller is.

Origin validation is **on by default**. With `allowed_origins` unset it defaults
to the loopback origins of the port actually bound. A request carrying no
`Origin` header still passes, per RFC 6454, so ordinary MCP clients and `curl`
are unaffected. Setting the key *replaces* the default rather than extending it.

### What this adds up to

**The one route you can expose is the one that does not authenticate anyone.**

`/ui` and `/api/admin/status` are closed by the peer check, which a caller cannot
forge — **but a reverse proxy forges it for them, by being the peer.** A proxy on
the same host is itself a loopback caller, and its default `Host` is on the
admin allow-list, so mapping `/ui` through it hands the page to anyone who can
reach the proxy. The peer check protects those two routes from the network, not
from something you put in front of them. **Forward `/mcp` and `/healthz` only**;
[clients.md](clients.md) says the same where the proxy recipes are.

`/mcp` is validated — on `Host` and on `Origin`, by GrooveSeek's own gate,
ahead of the session gate, and it is *stricter* than the library it replaced for
five malformed `Host` spellings — but it has no peer check, so a caller who
reaches the port and sends `Host: localhost` passes. `--bind` to a non-loopback
address plus `--i-know` is what makes the port reachable; neither flag widens any
allow-list.

This is a **declared position, not a defect**. GrooveSeek has no authentication
and none is planned; binding beyond loopback stays allowed because a container
must bind one, and doing so means the network boundary is yours. The refusal
`serve` prints for a non-loopback bind says so in the same words. See
[stability.md](stability.md) — *Where GrooveSeek is meant to run*.

## What was decided

This page began, in an internal draft, with four open questions. All four were
settled before 1.0.0. They are recorded here because the shapes above are the
consequence of them.

| Question | Outcome |
|---|---|
| Close `/mcp` to the same host only? | **No** — the line is drawn by declaration instead of by code. `--i-know` stays; the refusal it acknowledges was rewritten to state the consequence. [stability.md](stability.md) |
| Validate `Origin`? | **Yes, on by default** (v0.27.0), and in v1.0.0 GrooveSeek took both `Host` and `Origin` away from the library and answers them itself for every route. [ADR-0009](decisions/0009-one-dns-rebinding-gate.md) |
| Promote `/api/*` to a public API? | **No — `/api/search` was removed** in v0.27.0 rather than frozen. It passed 2 of `search`'s 17 parameters, and 1 tool of 6; `/mcp` was already better at the job. `/api/admin/status` stays, deliberately unstable, because the tray polls it. [ADR-0008](decisions/0008-declare-what-1-0-freezes.md) |
| What is `/ui` for? | **An operator's window onto their own server** — and one scheduled to go away during 1.x, once a client that speaks `/mcp` well enough exists. Keeping it outside the 1.0 freeze is what makes that a minor release. [stability.md](stability.md) |

## How the numbers were taken

One Windows machine, GrooveSeek v1.0.0, `bge-m3` on both sides, no reranker
configured on either. Substitute your own knowledge base for `<kb>`.

**The CLI row** — wall clock around the whole process, repeated:

```bash
groove search "semantic chunking" --kb-path <kb> --config <kb-config>
```

Time it from outside the process rather than with a shell builtin; on Windows
PowerShell, redirecting a native command's stderr turns a successful run into a
`NativeCommandError` and aborts a timing loop, so drive it through
`System.Diagnostics.Process`.

**The control** — the same setup work with no model:

```bash
groove status --kb-path <kb> --config <kb-config>
```

**The daemon row** — the `/mcp` request printed in full
[above](#a-php-application), against a daemon that has already answered at least
one query. Corpus sizes came from `groove status` for the CLI side and
`/api/admin/status` for the daemon side.

**The two outliers** were produced deliberately. The 4,616 ms figure is the first
query sent to a daemon left idle for 2.3 hours. The ~2,000 ms figure is `/mcp`
timed while a `groove search` runs concurrently against a different knowledge
base, with the ~180 ms recovery measured immediately after that process exits.

**Measure the CLI and daemon rows in separate batches.** Interleaving them is
what produces the second outlier, and it collapses the ratio from ~15× to ~1.6×.

**The concurrent-clients table** — `grooveseek/tests/http_lock_contention.rs`,
an ignored integration test, GrooveSeek 1.0.1 on the same machine
(8 cores / 16 threads):

```bash
GROOVE_BENCH_KB=<kb> GROOVE_BENCH_CONFIG=<kb-config> \
  cargo test -p grooveseek --release --test http_lock_contention -- --ignored --nocapture
```

It copies the corpus and its index to a temporary directory (it never indexes a
corpus it was pointed at), starts `groove serve --transport http --no-watch` on
it, and for each client count N spawns N threads that connect first, are released
by one barrier, and each time their own request from release to the first byte
of the response, with `Connection: close`. Throughput is the coordinator's clock
over the round. N runs in the order 1, 2, 4, 8, 16, 8, 4, 2, 1, so drift shows up
as a difference between the two visits to the same N; the ranges in the table are
those two visits. The "second daemon" figure is the same corpus copied twice,
served by two processes, eight clients split four and four. Use a release build:
the dev profile compiles the bundled sqlite-vec without optimisation and inflates
the database side alone.

## Related

- [clients.md](clients.md) — `.mcp.json` recipes, the HTTP transport, the watcher
- [stability.md](stability.md) — what 1.0.0 freezes, and where GrooveSeek is meant to run
- [ARCHITECTURE.md](ARCHITECTURE.md) — the source layout behind all of this
- [ADR-0008](decisions/0008-declare-what-1-0-freezes.md) — what 1.0.0 freezes
- [ADR-0009](decisions/0009-one-dns-rebinding-gate.md) — one DNS-rebinding gate
- [ADR-0010](decisions/0010-settle-what-the-1-0-command-line-freezes.md) — the three questions ADR-0008 left open
