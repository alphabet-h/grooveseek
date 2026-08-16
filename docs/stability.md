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

**`/ui`, `/api/search`, and `/api/admin/status` are not stable.** They exist to let
the person running the server look at their own knowledge base, they are
loopback-only by design (GrooveSeek has no authentication), and the web interface is
expected to be rebuilt. Treat the HTML and both JSON endpoints as internal.

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
