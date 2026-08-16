# 7. Rename the project to GrooveSeek, and let the command be `groove`

- Status: accepted
- Date: 2026-08-17
- Deciders: project owner
- Applies to: v0.26.0

## Context and Problem Statement

The project shipped twenty-five 0.x releases as `kb-mcp`. Two problems with that
name only became blocking once a 1.0 was on the table.

**The name is taken by a project that does the same thing.**
`github.com/moikas-code/kb-mcp` describes itself as a "cli tool and mcp server to
help ai manage a knowledge base of your code projects". Same category, same
name. A user searching for either lands on both.

**The name binds the product to a protocol.** MCP is one of two ways this server
is read. The other is a browser: a person opening `/ui` to search their own
notes. `-mcp` names the machine-facing half and is silent about the human-facing
half — and if MCP is displaced, the name outlives the thing it names.

Neither problem is new. What changed is that **the name is about to become
permanent**. It is not only a label; it is written into the user's filesystem:

```
.kb-mcp.db                   the index
kb-mcp.toml                  the config
.kb-mcpignore                the exclusion file
.kb-mcp-eval-history.json    the eval history
KB_MCP_CONFIG_HOME           the config-home override
<config_dir>/kb-mcp/<service>/   the service config home
```

Renaming after 1.0 means every existing install stops finding its own database,
config, and registered service. Supporting both names would mean carrying a
"look for the old name too" layer for the lifetime of the 1.x series. While the
project is 0.x and explicitly beta, that layer is not needed at all. **The window
is now or never**, and it closes at 1.0.0.

## Decision Drivers

- The identifier lands on user machines. Whatever is chosen is a contract, so it
  has to be decided before the release that freezes contracts, not after.
- A name that is not searchable is not free: the previous one collided inside its
  own category, which is the collision that costs the most.
- The command is typed constantly and the exclusion file is read at a glance.
  `.kb-mcpignore` cannot be parsed by eye; length is a real cost, not taste.
- Availability had to be measured, not assumed. A name that turns out to be taken
  after the rename lands is a second rename.

## Considered Options

Around ninety candidates were probed against crates.io, npm and GitHub search. A
probe that returned anything other than a clean 200 or 404 was recorded as
`UNKNOWN` rather than guessed, so a failed request could not read as "available".

- **Keep `kb-mcp`.** It is free on crates.io, so nothing external forced a change.
  Rejected on the two problems above; the GitHub collision is the decisive one.
- **A descriptive name** (`kbase`, `mdsearch`, `localrag`, `kbsearch`). Free, but
  `mdsearch` is already wrong (PDF and Office are supported) and `localrag` binds
  to a trend term the same way `-mcp` binds to a protocol.
- **A library metaphor** (`libris`, `athenaeum`, `slipbox`, `microgroove`).
  `slipbox` and `microgroove` are in use by other projects — `microgroove` has two
  GitHub repositories of the same name in music hardware. `athenaeum` is free but
  cannot be typed or abbreviated.
- **`AkaStylus`** — "a stylus for the Akashic Record", free everywhere. Rejected
  because the abbreviated `Aka` reads as 赤 / 垢 in Japanese and as "a.k.a." in
  English, which is the docs language.
- **`GrooveSeek`** — chosen. The groove of a record and the seek of a disk head:
  the two halves of "find the part of the recording you want".

## Decision Outcome

**The project is GrooveSeek.** The crate is `grooveseek`; the command and every
on-disk identifier is `groove`.

```
crate      grooveseek          crates.io + npm free, GitHub clear
command    groove              no standard command by that name
files      .groove.db  groove.toml  .grooveignore  .groove-eval-history.json
env        GROOVE_CONFIG_HOME  GROOVE_TRAY_LOG  GROOVE_BIN
satellites groove-svc  groove-tray   (crate name = binary name; neither is published)
```

**The product name and the identifier are deliberately different.** This follows
the same shape as the `ripgrep` crate installing a command called `rg`. It buys
two things: `.grooveignore` can be read at a glance where `.grooveseekignore`
cannot, and if the product name ever changes again, **nothing on a user's disk
has to move** — the second rename would be free in exactly the way this one is
not.

The MCP server keeps identifying itself as `grooveseek`, from `CARGO_PKG_NAME`.
`serverInfo.name` is a product identifier reported to clients, not a path or
something a user types, so it follows the product rather than the command.

### Consequences

- **v0.25.0 and earlier do not migrate.** A 0.26.0 binary will not find a
  `.kb-mcp.db`, will not read a `kb-mcp.toml`, and will not recognise a service
  registered under the old name. This is accepted rather than mitigated: the
  0.x series is beta, and a compatibility layer added now would have to be
  carried through all of 1.x. The migration steps are in the changelog.
- **Environment variables changed with everything else.** `KB_MCP_CONFIG_HOME`,
  `KB_MCP_TRAY_LOG`, `KB_MCP_BIN` and `KBMCP_BENCH_KB` have no aliases.
- **The changelog and the earlier ADRs were not rewritten.** Releases up to
  v0.25.0 shipped under the old name and their release assets are still named
  that way; rewriting the history would make this document's own account false.
  **`kb-mcp` in any document dated before 2026-08-17 is this project.**
  ADR 0003 keeps `kb-mcpignore` in its filename for the same reason — the file it
  describes is now `.grooveignore`.
- **The GitHub repository moved** to `alphabet-h/grooveseek`. Clone, fetch and
  push against the old URL keep working through GitHub's redirect, but the
  redirect dies if a repository named `kb-mcp` is ever created under the same
  account, so that name must stay unused. GitHub does not redirect Pages URLs,
  which is why the rename comes before Pages exists rather than after.
- **What this does not fix**: the name says nothing about what the product does,
  and searching for "groove" lands in music software. That cost was accepted
  knowingly, and it makes the first line of the README load-bearing.
