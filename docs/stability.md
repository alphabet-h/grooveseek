# Stability policy

What GrooveSeek promises not to break, and what it deliberately does not promise.
This policy takes effect at **1.0.0**. Releases before that are beta and carry no
compatibility guarantee.

> **日本語版**: [stability.ja.md](./stability.ja.md)

GrooveSeek follows [Semantic Versioning](https://semver.org/spec/v2.0.0.html).
Within this document:

- **MAJOR** — a stable surface below was removed, renamed, or changed meaning.
- **MINOR** — something was added, or an unstable surface changed.
- **PATCH** — a defect was fixed without changing any surface.

## Why this document exists

Without it, "1.0.0" would promise that **everything observable** stays fixed until
2.0.0. That is not a promise this project can keep: the observable surface is the
whole public Rust API, every command-line flag, the six MCP tools, every
`groove.toml` section, and a SQLite schema. Freezing all of it would mean either
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
origin leaves `/ui` served but unable to query.

**One check answers this for every route it guards** — GrooveSeek performs it
itself rather than leaving `/mcp` to the MCP library, so two surfaces can no
longer read the same value two different ways. See
[ADR-0009](decisions/0009-one-dns-rebinding-gate.md), which records the five
`Host` spellings on which they used to.

**What each route is compared against is not the same, and this key does not
reach all of them:**

| Route | `Host` | `Origin` |
| --- | --- | --- |
| `/mcp` | `allowed_hosts` | `allowed_origins` |
| `/ui`, `/api/admin/status` | loopback plus the bind address, not configurable | `allowed_origins` |
| `/healthz` | `allowed_hosts`, and **only when `healthz_public = false`** | never validated |

A request carrying no `Origin` passes wherever the check runs, which is why the
tray and `curl` are unaffected.

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
- **The long flags of the `groove` binary that this documentation describes**
  (`--kb-path`, `--config`, …). Short forms are stable where they are
  documented. Two things this deliberately excludes: flags of *other* programs
  that appear in examples here — `systemctl --user`, `cargo --release`,
  `huggingface-cli --include` — and the flags of `groove-tray`, which is a
  companion binary rather than the command line this policy covers.

  "Documented" is checked rather than assumed: a test walks every subcommand of
  `groove` and fails if a long flag it accepts appears nowhere in `docs/` or the
  README, so the frozen set cannot be decided by which flags happened to get
  written about.
- **Exit codes**.
- **The split between stdout and stderr**: where a command produces a *result*,
  the result goes to stdout and diagnostics go to stderr. Pipelines depend on
  this, so it is frozen.

  The subcommands that produce a result in that sense are `search`, `graph`,
  `doctor`, `validate`, `eval`, `tune`, `status`, `service status` and
  `service list`, and for each of them the channel is frozen.

  `serve` is the strictest case. Over the default stdio transport its stdout
  **is** the MCP connection, so nothing else may ever be written there: a single
  stray line corrupts the session for every client. That is frozen too, as a
  prohibition rather than a format.

  Everything else is a diagnostic and stays on stderr. That includes `index`'s
  progress, the confirmations from `service install` / `uninstall` /
  `tray-install` / `tray-uninstall`, and `status`'s "No index found" — which
  reports an inability to answer rather than an answer, and leaves stdout empty.

  **`status`, `service status` and `service list` used to write their results
  to stderr**, which is the question
  [ADR-0008](decisions/0008-declare-what-1-0-freezes.md) left open and
  [ADR-0010](decisions/0010-settle-what-the-1-0-command-line-freezes.md)
  settles; the changelog entry names the release that moved them. A caller that
  redirects with `2>&1` is unaffected. A caller that captured stderr alone now
  reads nothing, and `groove status | …` now receives the counts it always
  looked like it would.

### Machine-readable output

Every subcommand that takes `--format` is listed here, in one of the two groups,
so that silence is never mistaken for a promise.

**Stable** — these exist to be read by something other than a person:

- The **JSON** emitted by `search`, `graph`, `doctor`, and `validate`: every
  field documented today keeps its name, type, and meaning.
- `validate --format github`, the GitHub Actions annotation form
  (`::error file=…::message`). Its shape is GitHub's rather than ours; what is
  promised here is that the flag keeps producing it.
- **New fields may be added in a minor release.** Consumers must ignore fields they
  do not recognise — that is the mechanism by which the format can grow at all.

**Not stable** — see [Unstable](#unstable):

- All `--format text` output, from every subcommand, plus what `status`,
  `service status` and `service list` print. It is written for people to read,
  and gets reworded when a clearer wording turns up. Their **channel** is frozen
  (above) and their **wording** is not, so a script may rely on the counts
  arriving on stdout but not on the shape of the line carrying them —
  `groove doctor --format json` is the stable machine-readable route to the two
  numbers `status` leads with, `documents` and `chunks`.
- `graph --format dot` and `--format svg`. They are drawings: the DOT is valid
  DOT and the SVG is valid SVG, but the layout, the labels, and the colors are
  presentation and will change.
- The **JSON** emitted by `eval` and `tune`. Both are power-user measurement
  tools whose numbers evolve with the metrics themselves — `eval` already
  fingerprints its history with a `metric_version` for exactly that reason, and
  freezing the shape would freeze the metrics with it.

### What a search answers

The clause above promises that every documented field keeps its name, type and
meaning. This is that set, written out, for the `search` MCP tool and for
`groove search --format json` — which return the same wrapper and differ in one
field, noted below. A field not on this list is not part of the promise.

Names are given as paths because `topic` appears twice and means something
different each time.

| field | type | present |
|---|---|---|
| `results` | array | always |
| `results[].score` | number | always |
| `results[].path` | string | always, relative to the knowledge base |
| `results[].title` | string or `null` | always, `null` when the document has no title |
| `results[].heading` | string or `null` | always, `null` for a chunk with no heading |
| `results[].topic` | string or `null` | always |
| `results[].date` | string or `null` | always |
| `results[].tags` | array of strings | always, `[]` when there are none |
| `results[].content` | string | always |
| `results[].match_spans` | array | **omitted** unless computed — see below |
| `results[].match_spans[].start` | integer | byte offset into `content`, inclusive |
| `results[].match_spans[].end` | integer | byte offset into `content`, exclusive |
| `results[].expanded_from` | object | **omitted** unless the parent retriever considered the hit — present is not proof of expansion, see below |
| `results[].expanded_from.kind` | string | `"adjacent"` or `"whole_document"`, and which one decides the keys below |
| `results[].expanded_from.from_index` | integer | `adjacent` only — first chunk index merged in, inclusive |
| `results[].expanded_from.to_index` | integer | `adjacent` only — last chunk index merged in, inclusive |
| `results[].expanded_from.total_chunks` | integer | `whole_document` only — how many chunks the document has |
| `results[].start_line` | integer | **omitted** unless the chunk came from a source file — 1-based, and describes the chunk rather than the definition it came from |
| `results[].end_line` | integer | **omitted** unless the chunk came from a source file — 1-based and inclusive |
| `results[].symbol_kind` | string | **omitted** unless the chunk is a definition — the grammar's own word (`function`, `class`, `method`, `constant`, …), and the set grows as languages are added |
| `results[].uri` | string | **omitted** unless the document is one the server will hand over, and **never present over the command line** |
| `low_confidence` | boolean | always — **advisory**, see below |
| `filter_applied` | object | always; `{}` carries a narrower meaning than it looks — see the note |
| `filter_applied.category` | string | omitted unless given |
| `filter_applied.topic` | string | omitted unless given |
| `filter_applied.path_globs` | array of strings | omitted unless given |
| `filter_applied.tags_any` | array of strings | omitted unless given |
| `filter_applied.tags_all` | array of strings | omitted unless given |
| `filter_applied.date_from` | string | omitted unless given |
| `filter_applied.date_to` | string | omitted unless given |
| `filter_applied.min_confidence_ratio` | number | omitted unless given |
| `filter_applied.excluded_terms` | array of strings | omitted unless the query excluded something |
| `error` | string | **the whole response instead of the above**, when the MCP tool refuses or fails — see below |

**A search answers with one of two shapes.** Everything above the last row is
the successful one. When the MCP tool refuses a call or the search fails — an
`mmr_lambda` out of range, a query past the 1 KiB cap, a malformed glob, a
query made only of exclusions, an embedding or database failure — it answers
`{"error": "…"}` instead, with no
`results` key at all. Callers must branch on which arrived rather than reading
`results` unconditionally. The command line has no such envelope: it reports
the failure on stderr and exits non-zero, which is [the split](#command-line)
two sections up.

**`filter_applied` echoes the rows above when they arrived with an effect,
and nothing else.** Four things follow, and none of them is what an empty
object looks like it means:

- `min_quality` and `include_low_quality` are applied and never echoed. The
  quality filter is on by default, so `{}` does not say the results are
  unfiltered.
- An explicitly empty `tags_any` or `tags_all` is accepted and dropped from the
  echo, because an empty list excludes nothing. An empty `path_globs` is the
  exception: it is refused with the `error` envelope rather than accepted, since
  a glob list that matches nothing is more likely a mistake than a request for
  everything — pass `null` to disable it.
- `min_confidence_ratio` is echoed but narrows nothing: it only sets the
  threshold `low_confidence` is compared against. So the echo is not a list of
  what filtered the results either.
- An exclusion alone leaves `filter_applied` non-empty: `excluded_terms` is
  echoed whenever the query excluded something, even when no other filter
  was given.

Read `{}` as "none of the rows above arrived with an effect to report" — not
"no filter was given", and not "no filter ran". Adding the quality inputs to
the echo later would be a minor release, by the rule that new fields may be
added.

**Omitted means absent, not `null`.** Every row above that says "omitted" leaves
the key out of the object entirely. A consumer must not distinguish a missing
key from an explicit `null` — read both as "not provided".

**`expanded_from` says the parent retriever ran, not that content grew.** An
adjacent range whose two indices are equal is the degraded case: the neighbours
would have exceeded `max_expanded_tokens`, so the hit was left at its own chunk
and the field records that this happened. A single-chunk document produces the
same shape for the same reason. Read `from_index == to_index` as "considered and
not expanded" — the content is the original chunk either way.

**`match_spans` carries three states**, and they are not the same: absent means
the offsets were not computed — the query splits into a term containing a
non-ASCII character, the query is empty or whitespace-only, or the chunk's
content exceeds 256 KiB — `[]` means they were computed and nothing matched, and
a non-empty array is the contract in [docs/citations.md](citations.md), which
lists the same three cases.

**`results[].uri` is the one difference between the two successful wrappers.**
The MCP tool adds it when the document is servable; `groove search` never emits
it. Adding it to the command line later would be a minor release, by the rule
above that new fields may be added.

**Failure is where the two surfaces genuinely part.** The MCP tool answers with
the `error` envelope above and exit status has no meaning in a tool call; the
command line writes the reason on stderr and exits non-zero, and emits no JSON
at all. So the wrappers agree up to `uri` and the failure contracts do not
correspond — code written against one surface cannot assume the other reports
trouble the same way.

**`low_confidence` is frozen as a field, not as a judgement.** What is promised
is that the key is present and boolean. **The formula behind it, its default
threshold, and which queries trip it are explicitly not frozen** and may change
in any release. It is a heuristic, and measurement puts what it responds to in the shape of the
fused score distribution rather than in whether the answer is right — so treat
it as a hint to be careful and never as a verdict.
[docs/filters.md](filters.md) records what it does and does not detect.

### MCP surface

- **Tool names**, their **input schemas** (parameter names, types, and which are
  required), and the **fields of their results**. Fields may be added; existing
  fields do not change meaning. `list_topics` gaining `children` in 1.1.0 is
  what such an addition looks like: a new key on every entry, nothing existing
  renamed or re-typed.
- **Prompt names**: `deep_dive`, `find_gaps`, `summarize_topic`, `whats_new`. The
  prompt *text* they expand to is not stable.
- **The `kb://` resource URI scheme**, including `kb://doc/{path}` and
  `kb://topic/{prefix}`, and the rule that a URI the server offered can be read
  back — see [ADR-0004](decisions/0004-resource-reads-are-bounded-by-the-index.md).

### How the two surfaces name the same thing

The command line and the MCP tools are **two namespaces, frozen separately**. A
flag does not change because a parameter did, or the reverse.

Where both expose the same concept they use **the same noun**, and each follows
its own conventions for everything else. The command line is kebab-case and
names a repeatable flag in the singular; a tool parameter is snake_case and
names an array in the plural. So `--path-glob` and `path_globs` are the same
filter, as are `--tag-any` / `tags_any` and `--tag-all` / `tags_all`. The
mapping is predictable without being identical, and [usage.md](usage.md) states
it flag by flag.

Two things deliberately do not correspond:

- **Tool names and subcommand names.** `get_connection_graph` is `groove graph`,
  and `rebuild_index` is `groove index`. Each set is consistent within itself —
  tools read as a verb on an object because that is what a model chooses
  between, subcommands are short because that is what a person types — and no
  caller ever has to translate one into the other.
- **`rerank` and `--reranker`.** The tool parameter is a per-call boolean; the
  flag picks a model — and on the command line, naming one *is* the per-call
  override, with `--reranker none` opting a single query out. The standing
  default behind both is the `rerank_by_default` key, which `groove search` and
  `groove serve` both read, and whose flag is `groove serve --rerank-by-default`.

*Values* are held to a stricter rule than names, because a name that differs
costs a lookup while a value that differs fails the call outright. Where the two
surfaces would spell an enum value differently, **both spellings are accepted on
both sides**: `seed_strategy` takes `all_chunks` and `all-chunks` either way.

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
| environment variables | `GROOVE_CONFIG_HOME`, `GROOVE_TRAY_LOG`, `GROOVE_GRAMMAR_DIR` (v1.3.0+; must be absolute). (`FASTEMBED_CACHE_DIR` is fastembed's name — honoured, but not ours to freeze. `GROOVE_BIN` belongs to the shipped example hook, not to the binary.) |
| grammar plugin layout | one library per language, named `groove_grammar_<language>` with the platform's own prefix and suffix, in the grammar directory. The C symbols it exports and the ABI version it declares are frozen too: a plugin built for one 1.x groove loads in the next. |

## Unstable

These are visible, and this document is the notice that they may change in any
release, including a patch.

### The admin web surface

**`/ui` and `/api/admin/status` are not stable.** They exist to let the person
running the server look at their own server, they are loopback-only by design
(GrooveSeek has no authentication), and the page is expected to keep changing.
Treat the HTML and the JSON as internal.

`kb.path` **has already been removed** from the status payload, and the status
band no longer prints it. It held the knowledge base's absolute path, which on
Windows is `C:\Users\<name>\...` — an operator's account name, in a JSON body
and in every screenshot of that page. Nothing needed it: the tray reads
`daemon.pid` and `indexing.active`, and what identifies a knowledge base to the
person looking at it is `kb.documents`, `kb.chunks` and `kb.model`, all still
there. This paragraph is the notice; that the surface is unstable is why the
removal happens before 1.0 rather than being deferred past it.

Both routes are served with `Content-Security-Policy` and
`X-Content-Type-Options: nosniff`. The policy is `default-src 'none'` with only
what the page uses added back, so an external script or stylesheet added to
`/ui` fails there rather than loading.

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

### Human-readable output, and the two measurement tools

The text printed by any subcommand — including `search --format text` and
`graph --format text` — may be reworded, reordered, or extended at any time.
**Do not parse it.** The same goes for `graph --format dot` and `--format svg`:
they are pictures, and how a picture is laid out is presentation.

`eval --format json` and `tune --format json` are also unstable, despite being
JSON. They report retrieval measurements, and the measurements themselves are
expected to improve; `eval` already records a `metric_version` in its history
file so that runs made under different definitions are not compared. Freezing
the JSON would freeze the metrics.

Where a stable machine-readable form is needed and does not exist, that is a
missing feature — please open an issue rather than writing a screen-scraper.

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

Log lines, their levels, and the wording of warnings are not stable. Verbosity is
set through `RUST_LOG` — `RUST_LOG=grooveseek=debug` for the detail, `info` when
it is unset — and what that detail contains is a debugging aid rather than an
interface.

## Deprecation

A stable surface is not removed without notice:

1. In a **minor** release it is marked deprecated. Using it still works and prints a
   warning to stderr naming the replacement.
2. It is removed no earlier than the next **major** release.

The one exception is a security defect that cannot be fixed while keeping the old
behavior. If that happens it will be called out at the top of the changelog entry.

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
