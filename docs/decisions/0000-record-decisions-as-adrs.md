# 0. Record architecturally significant decisions as ADRs

- Status: accepted
- Date: 2026-07-28
- Deciders: project owner

## Context and Problem Statement

When `.xls` support was withdrawn in v0.14.0, the reasoning survived in four
places — `CHANGELOG.md`, both READMEs, a 32-line doc comment in
`parser/xlsx.rs`, and a private note under `.dev/knowledge/` — while the
*intent to revisit it* survived in none. The private backlog had no entry, and
`.dev/feature-ideas.md` still listed `.xls` as `done`, actively claiming the
opposite of what shipped. Reconstructing the decision a week later meant
reading four documents and noticing that a fifth was wrong.

That failure is not specific to `.xls`. A withdrawal PR treats the withdrawal
itself as the deliverable, so effort goes into explaining the change; recording
the option that was *not* taken looks out of scope and gets dropped. The same
applies to any decision where alternatives were weighed: the chosen path is
visible in the code, and the discarded ones are visible nowhere.

What is missing is a single canonical answer to "why is it this way, what else
was considered, and what did it cost" — one that lives in version control,
travels with a clone, and is reviewable in the pull request that makes the
decision.

## Considered Options

1. **Status quo.** Keep recording rationale wherever it fits — changelog,
   README, source comments, private notes.
2. **A private decision log under `.dev/decisions/`.** Cheap and informal,
   consistent with the existing private notes.
3. **`docs/decisions/`, following MADR.** Tracked in git alongside the code.

## Decision Outcome

Chosen option: **`docs/decisions/`, following MADR**, because it is the only
option that puts the record under version control.

Option 1 is what produced the `.xls` situation. Option 2 fails on a property
that matters more than convenience: `.dev/` is excluded via
`.git/info/exclude`, has no nested repository, and is not backed up — a
decision log kept there is not versioned, does not survive a clone, cannot be
referenced from public documentation, and is lost with the machine. The single
most valuable property of an ADR is durability, and option 2 removes it.

### Format

[MADR](https://adr.github.io/madr/) with the optional sections dropped unless
they earn their place. Files are `NNNN-title-with-dashes.md`, numbered
consecutively from 0000, in `docs/decisions/`.

### Language

English and Japanese pairs — `NNNN-slug.md` and `NNNN-slug.ja.md` — matching
the convention already used for `README` and everything under `docs/`.

English-only was considered and rejected: the primary reader today is the
project owner, who reads Japanese. The usual objection to bilingual long-form
prose is drift, but it does not apply here — an ADR is superseded rather than
edited, so the text is written once and then left alone. Only the `Status`
line changes over time.

### When to write one

Write an ADR only when **all three** hold:

1. Real alternatives were compared — not just "we picked a library".
2. Reversing it would be expensive.
3. It affects structure, dependencies, interfaces, or non-functional
   characteristics (memory, startup cost, binary size, security posture).

The dominant failure mode reported in practice is not too few ADRs but too
many: without a threshold, the log fills with routine choices and the
significant decisions become unfindable. When in doubt, a `CHANGELOG` entry is
enough.

### What an ADR does not replace

An ADR is the canonical *why*. It absorbs rationale that would otherwise be
duplicated, and it leaves the following alone:

| Location | Keeps |
|---|---|
| `CHANGELOG.md` | What changed in a release, and the upgrade impact |
| `README` | What a user can and cannot do today |
| `docs/ARCHITECTURE.md` | How the current system is put together |
| Source comments | Facts a reader needs at that line — measurements, invariants |
| `.dev/knowledge/` | How an investigation went wrong, and traps to avoid repeating |

When an ADR is added, prose that merely restates its reasoning elsewhere should
be cut down to a summary and a link.

### Immutability

An ADR is never edited to reflect a reversal and never deleted. To change a
decision, add a new ADR and set the old one's status to
`superseded by ADR-NNNN`. The discarded reasoning is the point of the record.

Statuses used: `proposed`, `accepted`, `rejected`, `deprecated`,
`superseded by ADR-NNNN`.

## Consequences

- A decision now has one address, so `CHANGELOG` / `README` / source comments
  can shrink to a summary and a link instead of each carrying the full
  argument.
- The reasoning becomes public. Anything that must stay private belongs in
  `.dev/knowledge/`, not here.
- Every ADR costs two files. The three-part test above is what keeps that
  bounded.
- Existing decisions are not backfilled wholesale. Records are written for new
  decisions, and retroactively only where the rationale is at risk of being
  lost.

## Template

```markdown
# N. Short noun phrase

- Status: proposed | accepted | rejected | deprecated | superseded by ADR-NNNN
- Date: YYYY-MM-DD
- Deciders: who

## Context and Problem Statement
What forces are at play, stated neutrally. What breaks if nothing is done.

## Considered Options
1. ...
2. ...

## Decision Outcome
Chosen option: "...", because ...

### Consequences
What the resulting context looks like — including what got worse.

### Confirmation
How compliance is checked (a test, a CI step, a measurement).

## More Information
Links to the PR, the issue, measurements, related ADRs.
```

## More Information

- [MADR](https://adr.github.io/madr/) — the template this follows
- [Michael Nygard, *Documenting Architecture Decisions* (2011)](https://cognitect.com/blog/2011/11/15/documenting-architecture-decisions)
  — the origin of the format and of the "never delete, mark superseded" rule
- Japanese version: [0000-record-decisions-as-adrs.ja.md](./0000-record-decisions-as-adrs.ja.md)
