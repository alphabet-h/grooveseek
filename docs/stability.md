# Stability policy

What GrooveSeek promises not to break, and what it deliberately does not promise.
This policy takes effect at **1.0.0**. Releases before that are beta and carry no
compatibility guarantee.

GrooveSeek follows [Semantic Versioning](https://semver.org/spec/v2.0.0.html).
Within this document:

- **MAJOR** — a stable surface below was removed, renamed, or changed meaning.
- **MINOR** — something was added, or an unstable surface changed.
- **PATCH** — a defect was fixed without changing any surface.

## Why this document exists

Without it, "1.0.0" would promise that **everything observable** stays fixed until
2.0.0. That is not a promise this project can keep: at the time of writing the code
exposes 408 public Rust items across 24 modules, 138 command-line flags, 6 MCP tools,
11 configuration sections, and a SQLite schema. Freezing all of it would mean either
never improving the parts nobody depends on, or shipping major versions for changes
nobody would notice.

So the promise is narrowed on purpose, and the narrowing is written down rather than
left to be inferred.

## Where GrooveSeek is meant to run

**GrooveSeek has no authentication, and none is planned.** That is a design
position rather than a gap, so it belongs in this document instead of a roadmap.

The HTTP transport is meant to be reached from the same host. Something else owns
the network boundary — a container's network isolation, a reverse proxy, or the
application that puts a face on the knowledge base. Whatever it is, it terminates
the connection from outside, authenticates, and talks to GrooveSeek over loopback.

Binding to a non-loopback address stays allowed, because a container has to bind
one or published ports never reach it. But doing so means you have taken that
boundary on yourself. `/mcp` is covered by `Host` validation and a session cap,
and neither is authentication: **anything that can reach the port can read the
entire knowledge base.** The startup warning says so, and means it.

`/ui` and `/api/admin/status` refuse every peer that is not loopback. That is
not configurable, so a proxy has to run on the same host.

### Origin validation

The MCP specification requires a Streamable HTTP server to validate the `Origin`
header against DNS rebinding. GrooveSeek does, defaulting to the loopback origins
for whichever port it binds.

Requests carrying no `Origin` at all — ordinary MCP clients, the tray, `curl` —
pass, as RFC 6454 and the specification describe. The check exists to stop a web
page open in your own browser from reaching the port; it is not a second access
control. Publishing through a proxy means naming your public origin in
`[transport.http].allowed_origins`, because a browser-based client will send that
one and not a loopback origin. That key *replaces* the default rather than
extending it, so keep the loopback entries alongside it if browser clients also
reach you over loopback.

**It covers `/mcp`, and `/ui` searches through `/mcp`.** So this list decides
whether the built-in page can search: replacing the default with only a public
origin leaves `/ui` served but unable to query. `/api/admin/status` has no
`Origin` check of its own; what restricts it is the loopback peer requirement
above.

`allowed_hosts` does the same thing one step earlier — Host validation runs
before Origin validation, so a list naming only a public hostname refuses the
`Host: localhost` that a locally opened `/ui` sends. Either key replaces its
default rather than extending it, so **list the exact names and origins you
browse with**: `allowed_hosts = ["127.0.0.1"]` still refuses a page opened
through `localhost`.

The server warns at startup when either list has no loopback entry at all. It
does not warn when a list has one that does not match the address you use, and
the browser sees only a 403 — so `/ui` reports the host and origin it needs when
a search is refused.

## Stable

Breaking any of these requires a major version.

### Command line

- **Subcommand names** and their **positional arguments**.
- **Documented long flags** (`--kb-path`, `--config`, …). Short forms are stable
  where they are documented.
- **Exit codes**.
- **The split between stdout and stderr**: a command's *result* goes to stdout and
  its *diagnostics* go to stderr. Pipelines depend on this, so it is frozen for
  every subcommand.

### Machine-readable output

- The **JSON** emitted by `search` and `graph`: every field documented today keeps
  its name, type, and meaning.
- **New fields may be added in a minor release.** Consumers must ignore fields they
  do not recognise — that is the mechanism by which the format can grow at all.

Text output from `doctor`, `validate`, `eval`, `tune`, and `status` is written for
people to read. It is **not** stable; see [Unstable](#unstable).

### MCP surface

- **Tool names**, their **input schemas** (parameter names, types, and which are
  required), and the **fields of their results**. Fields may be added; existing
  fields do not change meaning.
- **Prompt names**: `deep_dive`, `find_gaps`, `summarize_topic`, `whats_new`. The
  prompt *text* they expand to is not stable.
- **The `kb://` resource URI scheme**, including `kb://doc/{path}` and
  `kb://topic/{prefix}`, and the rule that a URI the server offered can be read
  back — see [ADR-0004](decisions/0004-resource-reads-are-bounded-by-the-index.md).

### HTTP

- **`/mcp`** — the Streamable HTTP transport endpoint.
- **`/healthz`** — its path, and that a healthy server answers `200`.

### Configuration

- **Key names, their types, and their default values.** Adding a key is a minor
  release.
- **Configuration files are not forward compatible.** Unknown keys are rejected, so
  a 1.0.x binary will refuse a configuration written for 1.1. This is deliberate:
  the alternative is that a typo such as `modle = "bge-m3"` is silently ignored and
  the knowledge base is indexed with the wrong model. Failing to start is the safer
  direction. Configurations are matched to the binary that reads them, not shared
  across versions.

### The default embedding model

Changing which model is used by default is a **major** change. The model identifier
is recorded in the index and must match exactly at startup, so a new default would
stop every existing installation from starting until it reindexed.

### Names that land on your machine

These are the reason the project renamed itself before 1.0.0 rather than after:

| | |
|---|---|
| index database | `.groove.db`, in the parent directory of `kb_path` |
| configuration | `groove.toml` |
| exclusion file | `.grooveignore` |
| evaluation set / history | `.groove-eval.yml` / `.groove-eval-history.json` |
| service artifacts | task, unit, and launch-agent names derived from the service name, and the layout of the config home |
| environment variables | `GROOVE_CONFIG_HOME`, `GROOVE_TRAY_LOG`, `GROOVE_BIN` |

## Unstable

These are visible, and this document is the notice that they may change in any
release, including a patch.

### The admin web surface

**`/ui` and `/api/admin/status` are not stable.** They exist to let the person
running the server look at their own server, they are loopback-only by design
(GrooveSeek has no authentication), and the page is expected to keep changing.
Treat the HTML and the JSON as internal.

`/api/search` **has already been removed**, before 1.0.0 rather than during it.
It only ever accepted two of the seventeen parameters the `search` tool takes,
so `/mcp` was the better endpoint for anything outside the process — and `/ui`
now uses `/mcp` itself, which makes the page the smallest working example of an
MCP client.

`/ui` is expected to **go away** as well. Browsing a knowledge base belongs to a
client that speaks `/mcp`, where every tool and every parameter is reachable and
the surface is already stable, so the intention is to retire the built-in page
during 1.x once such a client exists. `/api/admin/status` is not on that path:
it reports operational state (version, pid, indexing progress) that has no place
in a tool surface designed for language models.

Read that as advance notice, not as a schedule. These surfaces are unstable, so
nothing above is promised in either direction.

### Human-readable output

The text printed by `doctor`, `validate`, `eval`, `tune`, and `status` may be
reworded, reordered, or extended at any time. **Do not parse it.** Where a stable
machine-readable form is needed and does not exist, that is a missing feature —
please open an issue rather than writing a screen-scraper.

### The index database

`.groove.db` is a SQLite file, and its **internal schema is not a contract**. Tables
may be added, dropped, or restructured, and an upgrade may rebuild the index. What
is promised is the *file name and location*, so that tooling knows what to back up
and what to exclude from version control. Read the index through GrooveSeek, not
through SQLite.

### The Rust library API

**Not covered by this policy at all.** The crate exposes public items because the
binary and its tests are built from them, not because they are offered for reuse.
`grooveseek` is marked `publish = false`, and `cargo package` does not even succeed
— the workspace uses unversioned path dependencies. Nothing on crates.io depends on
these types, so they are free to change.

If GrooveSeek is ever published as a library, that will be a separate decision with
its own stability statement.

### Diagnostics and logs

Log lines, their levels, and the wording of warnings are not stable. `--verbose`
output in particular is a debugging aid.

## Deprecation

A stable surface is not removed without notice:

1. In a **minor** release it is marked deprecated. Using it still works and prints a
   warning to stderr naming the replacement.
2. It is removed no earlier than the next **major** release.

The one exception is a security defect that cannot be fixed while keeping the old
behaviour. If that happens it will be called out at the top of the changelog entry.

## Depending on GrooveSeek safely

- Pin the version you tested against; read the changelog before moving.
- Parse the JSON from `search` and `graph`, not the text from anything else.
- Ignore JSON fields you do not recognise, so a minor release cannot break you.
- Keep each configuration file with the binary version it was written for.
- Do not read `.groove.db` directly.
- Do not link against the crate.

## Reasoning

The judgement behind this scope — what was measured, and why the Rust API is
excluded — is in
[ADR-0008](decisions/0008-declare-what-1-0-freezes.md).
