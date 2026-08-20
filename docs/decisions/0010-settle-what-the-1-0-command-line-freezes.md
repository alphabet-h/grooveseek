# 10. Settle the three command-line questions ADR-0008 left open

Date: 2026-08-21

> **日本語**: [0010-settle-what-the-1-0-command-line-freezes.ja.md](0010-settle-what-the-1-0-command-line-freezes.ja.md)

## Status

Accepted

## Context

[ADR-0008](0008-declare-what-1-0-freezes.md) wrote down what 1.0.0 freezes. It
did not decide everything: it named three questions and deferred them in
writing, `docs/stability.md` saying of one of them "**is not settled, and this
paragraph does not freeze it** … it will be settled before 1.0.0 rather than by
it."

They are deferred, not forgotten, and the deferral has an expiry date. Tagging
1.0.0 with them open does not leave them open — it answers all three by default,
in whichever direction the code happens to sit, and makes every answer expensive
to revise.

**The three:**

1. `groove validate --strict` is accepted and discarded. `main.rs` binds it as
   `strict: _strict`, and `docs/usage.md` documented it as "currently a no-op".
2. `status`, `service status` and `service list` write their output to stderr,
   so `groove status | grep Documents` receives nothing.
3. Three configuration keys use a different word from the flag that sets the
   same value: `[quality_filter].threshold` against `--min-quality` and the MCP
   `min_quality`, `[eval].k_values` against `--k`, and `[transport].kind`
   against `--transport`.

## Decision Drivers

**Before 1.0.0 the freedom runs one way only.** Every one of these can be
changed now for nothing and changed later only in a major release. That is not a
reason to change all of them — it is a reason to ask, for each, *which
direction stays open afterwards*:

| | now | after 1.0.0 |
| --- | --- | --- |
| remove `--strict` | free | major |
| give `--strict` a meaning | free | major |
| add `--strict` back | — | **minor** |
| move a result to stdout | free | major |
| remove a configuration key name | free | major |
| add a configuration key name | — | **minor** (`stability.md`) |

Two of the three close permanently at 1.0.0. The third does not, and that
asymmetry decides it.

## Decision

### `--strict` is removed

A flag that is accepted, documented, and discarded is worse than one that does
not exist. A CI job passing `--strict` believed it had asked for stricter
checking; it had not, and nothing told it so. Implementing it is a feature —
`[options].allow_unknown_fields` plus the schema work behind it — not a
decision about what 1.0 promises, and freezing it as a no-op would make giving
it meaning later a major release.

Removing it now keeps the cheap direction open: adding the flag back when the
feature exists is a minor release. Scripts passing it will fail to parse, which
is the visible form of what was already happening.

### `status`, `service status` and `service list` print their results on stdout

What each of them prints is the answer to the question it was asked. That an
answer is meant for a person to read does not make it progress, and
`docs/stability.md` already freezes "the result goes to stdout" for every other
command that produces one.

Everything that is not an answer stays on stderr: `index`'s progress, the
confirmations from `service install` / `uninstall` / `tray-install` /
`tray-uninstall` — which report on an action performed rather than a question
asked — and `status`'s "No index found", which reports an inability to answer
and leaves stdout empty.

Only the **channel** is frozen. The wording of these lines stays unstable, and
`groove doctor --format json` remains the machine-readable route to the two
numbers `status` leads with.

### The configuration key names are left as they are

Measured, the case for renaming does not survive contact with the three names:

| key | the flag | renaming to what |
| --- | --- | --- |
| `[transport].kind` | `--transport` | nothing. `transport.transport` is not a name; `kind` is what a TOML section calls its variant |
| `[eval].k_values` | `--k` | `[eval].k` is worse. A bare `k` in a file read months later says nothing; `--k` is legible only because a command line is read while being written |
| `[quality_filter].threshold` | `--min-quality` | `quality_filter.min_quality` stutters, and section-qualified the current name already reads as what it is |

A configuration key is read section-qualified and a flag is read alone, so the
two are not spelled the same way even when they mean the same thing. Requiring
them to match would make at least two of these names worse.

This is also the one question that does not close at 1.0.0. `docs/stability.md`
makes **adding** a key a minor release, so a better name can still be introduced
during 1.x. What closes is removing the old one — and that is the direction
this decision declines in any case.

The cost of renaming now is not hypothetical: unknown keys are rejected, so a
rename stops every existing `groove.toml` from starting. The four deployment
recipes under `examples/deployments/` and `groove.toml.example` all set
`[quality_filter]`, and every user's file that has ever set it would need
editing before the binary would run at all.

## Consequences

**`groove status | …` now receives the counts**, and a caller that captured
stderr alone now reads nothing. A caller redirecting `2>&1` is unaffected. This
is the only one of the three that changes behaviour for someone who was not
already broken.

**`groove validate --strict` now fails to parse.** The
`documented_flags` tests keep the removal honest in both directions: a flag the
binary accepts must appear in the prose, and prose must not name a flag the
binary lacks.

**The three key names are frozen at 1.0.0 in their current spelling.** If
`quality_filter.threshold` turns out to be the wrong name, the fix during 1.x is
to add the better one, not to move this one.

**`docs/stability.md` no longer defers anything about the command line.** The
paragraph that said the channel question was unsettled now states the answer,
which means the next reader cannot mistake the current behaviour for an
accident that is still being weighed.

## Alternatives considered

**Implement `--strict` instead of removing it.** It is the outcome a user
reading the documentation would expect. Rejected as a scope error rather than a
wrong answer: it needs `[options].allow_unknown_fields` and the schema work
behind it, which is a feature with its own design, and holding 1.0.0 for it
trades a dated promise for an undated one.

**Freeze `--strict` as a documented no-op.** Honest about what the binary does,
and breaks nothing. Rejected because it makes the flag permanently useless: with
1.0.0 tagged, giving it the meaning its name claims becomes a major release, so
the feature that would justify it can never land in 1.x.

**Leave `status` on stderr and freeze that.** Nothing breaks, and four test
files keep reading the stream they read today. Rejected because it freezes for
the whole 1.x series a pipe that looks like it works and does not — and the
`2>&1` case, which is the common one, is unaffected by moving.

**Give `status` a `--format json` instead of moving the channel.** It would make
the counts machine-readable without touching stderr. Rejected because it adds a
new frozen surface to solve a problem `groove doctor --format json` already
solves, and it would leave the channel question unanswered underneath.

**Rename `[quality_filter].threshold` and keep the old name as an alias.** Both
spellings would work, nothing would break, and the names would match. Rejected
because it freezes two names where one is enough, and because it can be done in
a minor release at any point during 1.x if the need is ever felt. Doing it now
spends the one-way freedom on the one question that did not need it.
