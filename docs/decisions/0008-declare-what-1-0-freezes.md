# 8. Declare what 1.0.0 freezes, and leave the Rust API out of it

- Status: accepted
- Date: 2026-08-17
- Deciders: project owner
- Applies to: v1.0.0

## Context and Problem Statement

Semantic versioning says a major version is required to break the public API. It
does not say what the public API *is*. For a library that question answers itself —
the API is what the crate exports. For a program with a command line, an MCP
surface, an HTTP transport, a configuration format, and files it writes into the
user's home directory, the answer is whatever the maintainer says it is, and if the
maintainer says nothing, users will reasonably assume it is **everything they can
observe**.

Measured on the day of this decision:

| | |
|---|---|
| public Rust items | **408**, across 24 `pub mod` |
| command-line flags | **138**, across 10 subcommands |
| MCP tools / prompts | 6 / 4, plus the `kb://` resource scheme |
| HTTP routes | 5 |
| configuration sections | 11, with `deny_unknown_fields` in 25 places |
| on-disk artifacts | index database, config, exclusion file, eval set and history, service registrations |

Freezing all of that until 2.0.0 leads to one of two failures: the project never
improves the parts nobody depends on, or it ships major versions for changes nobody
would have noticed. Both are worse than saying, in advance and in writing, which
parts are the promise.

One item makes this concrete rather than theoretical. The web interface at `/ui` is
a placeholder that says so in its own HTML, and it is scheduled to be rebuilt. If
1.0.0 ships without a declaration, **rebuilding it becomes a 2.0.0**.

## Decision Drivers

- The promise has to be one that can be kept for the whole 1.x series, not one that
  sounds generous at the moment it is made.
- Users cannot tell a contract from an implementation detail by looking. Anything
  not written down will be inferred, and the inference will be broad.
- **The Rust API is not a contract today, and this is measurable rather than
  asserted**: `cargo package -p grooveseek` fails with *"dependency `groove-tray`
  does not specify a version"*, because the workspace uses unversioned path
  dependencies. Nothing on crates.io can depend on these 408 items, so tagging
  1.0.0 would not freeze them even if we wanted it to.
- A promise about configuration has to pick a failure direction. Silently ignoring a
  key you do not understand and loudly refusing to start are both defensible; they
  are not both safe.

## Considered Options

- **Say nothing and tag 1.0.0.** The default reading is "everything is frozen".
  Rejected: unkeepable, and it would make the planned `/ui` rework a major release.
- **Freeze everything and mean it.** Honest, but it converts every internal
  refactor into a compatibility question and blocks the work 1.0.0 exists to enable.
- **Declare a narrow surface in writing.** Chosen.
- **Delay 1.0.0 until the surface is small enough to freeze wholesale.** This is the
  option that looks careful and is not: the surface does not shrink on its own, and
  staying at 0.x indefinitely is its own way of never making a promise.

## Decision Outcome

**The stable surface is written down in [docs/stability.md](../stability.md)** (and
its Japanese counterpart) rather than left to inference. In summary: the command
line, the machine-readable JSON, the MCP surface, `/mcp` and `/healthz`,
configuration keys and defaults, the default embedding model, and the names written
into the user's filesystem. Explicitly outside: the admin web surface, all
human-readable text output, the database's internal schema, log wording, and the
Rust API.

Three parts of that deserve their reasoning here rather than in the policy.

**The Rust API is excluded, and `grooveseek` is marked `publish = false`.** Marking
it rather than only documenting it makes the intent enforceable: `cargo publish`
refuses, so the exclusion cannot be undone by accident. This is not a way of dodging
the question — the crate genuinely cannot be packaged today, so the exclusion
describes reality. Publishing later is a separate decision that would come with its
own stability statement.

**The admin web surface is unstable.** `/ui`, `/api/search` and `/api/admin/status`
are loopback-only by design because GrooveSeek has no authentication
([ADR-0007](0007-rename-the-project-to-grooveseek.md) covers the naming; the
loopback decision predates it). They serve the operator looking at their own data,
not integrators. Declaring them unstable is what keeps the planned rework a minor
release.

**Configuration is not forward compatible, and unknown keys stay an error.** The
alternative — warn and continue — trades a loud failure for a quiet one. A
misspelled `model` key would leave the knowledge base indexed by the default model
while a single warning scrolls past on a daemon's stderr that nobody reads. A
configuration file belongs to the binary version that reads it; this policy says so
instead of pretending otherwise.

### Consequences

- **Three other 1.0 blockers are resolved by this document rather than by code**:
  the default embedding model is declared part of the contract, the unknown-key
  policy is settled, and the service artifact names are named as frozen. What
  remains is aligning CLI and MCP names, deciding the HTTP `Origin` default, and
  writing out the full `search` response.
- **Rebuilding the web interface stays a minor release.** That was the point.
- **Downgrading across a configuration change fails loudly.** A 1.0.x binary will
  refuse a 1.1 configuration. This is the accepted cost of catching typos.
- **`cargo package` still fails, and that is now deliberate rather than an
  oversight.** If the failure is ever fixed, it should be because publishing was
  decided on, not as a drive-by cleanup.
- **What this does not do**: it does not make the surface smaller. The 408 public
  items are still there, and a reader of the source cannot tell which are load
  bearing. Narrowing the code to match the promise is separate work, and this
  document is the prerequisite for it rather than a substitute.
