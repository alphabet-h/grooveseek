# 29. Re-read unchanged documents once per format, by a policy key per format

- Status: accepted
- Date: 2026-10-14
- Deciders: project owner
- Applies to: v1.19.0 (PR #NNN)

## Context and problem

[ADR-0028](0028-reread-unchanged-docx-once-per-index.md) lets an existing index
catch up with a change to how `.docx` files are read, once, on an ordinary run:
while a policy key in `index_meta` is not at its value, every unchanged `.docx`
is re-read and each row is settled. The pass was `.docx` only: the key, the
predicate that picks the rows to re-read (`rereads_unchanged_docx`), the
awaiting-reparse mark and the final transaction all named `"docx"`.

v1.19.0 changes how `.pptx` slides are read: each table row becomes one line,
its cells tab-separated, and `<a:br>` a line break. An unchanged `.pptx` keeps
the chunks the earlier version cut for as long as the index exists, the same
problem ADR-0028 solved for `.docx`. This is ADR-0028's revisit condition: "a
later change needs the same catch-up for another format". This ADR replaces
ADR-0028's sentence that names the per-row column (its option 1) as the general
form.

## Decision drivers

- Everything ADR-0028 asked for still holds: one ordinary run is enough, a
  document that already reads correctly is not re-embedded, no old row stays
  behind the fast path once the catch-up is recorded, a run that stops early
  records nothing, the schema does not change.
- A stale key of one format must not re-read, re-embed or mark the rows of
  another format.
- The `.docx` keys, their values, their notices and the tests that pin them do
  not change.

## Options considered

1. **A column on each document row saying which rules wrote it** (ADR-0028's
   option 1). Exact and general, but a schema change with a migration, a column
   every write path has to fill, and the `.docx` tests pin the key values and
   that dropping a key reopens the pass. Rejected for those two reasons.
2. **Copy the `.docx` pass for `.pptx`.** No change to the `.docx` path, but two
   implementations of the same question ("does this run re-read this row"),
   which AGENTS.md rules out.
3. **A policy key per format and one predicate over (extension, stale).** Taken.

## Decision

- Each format that needs a catch-up has its own key in `index_meta`:
  `.docx` keeps its three (ADR-0028), `.pptx` gets
  `pptx_table_break_policy` (`tab-joined-rows-kept-breaks`).
  <!-- via: grep -o "docx_[a-z]*_policy" grooveseek/src/db/meta.rs | sort -u -->
- One predicate, `rereads_unchanged_as(extension, stale, …)`, decides whether a
  row is re-read: that format's pass is running, the row was written by that
  format's parser, and its hash is the scan's. `rereads_unchanged_docx` is its
  `.docx` case.
- One predicate, `stale_row`, decides whether a row the run did not settle gets
  `awaiting-reparse`: it belongs to a format whose pass is running.
- The keys are written in the run's final transaction, together with the marks:
  the `.docx` keys in their order, then the `.pptx` key. A run that is cancelled
  or returns an error writes none.
- `groove index` announces each running pass before the loop, the `.docx` line
  first. The file watcher runs no pass.
- A later format adds its key and its pass to the same predicates.

## Consequences

- A stale `.pptx` key never re-reads a `.docx`, nor the other way round; an
  index upgraded from v1.18.0 re-reads its `.pptx` files once and leaves its
  `.docx` files on the fast path.
- The number of keys grows with the formats that need a catch-up.
- **A daemon must be restarted first**, as ADR-0028 says: a daemon still on the
  previous version keeps writing edited `.pptx` files the old way after the key
  is recorded, which only `--force` undoes.
- **Revisit when** one format needs a fourth key of this kind (ADR-0028's
  condition for `.docx`, carried over), or a third format needs a catch-up.
- **Tests hold it**: `grooveseek/src/indexer.rs` (the predicates and the
  notice), `grooveseek/src/db.rs` (the key) and
  `grooveseek/tests/index_pptx_table_policy.rs` (the runs, the cross-format
  cases, the failed write, the callback, the watcher).

## References

- [ADR-0028](0028-reread-unchanged-docx-once-per-index.md), the pass this
  generalises.
- [usage.md](../usage.md) for the notices and the daemon order;
  [behavior.md](../behavior.md) for how tables and line breaks are read.
- Japanese version:
  [0029-reread-unchanged-documents-once-per-format.ja.md](0029-reread-unchanged-documents-once-per-format.ja.md)
