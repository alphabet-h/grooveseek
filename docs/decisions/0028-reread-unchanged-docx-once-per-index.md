# 28. Re-read unchanged docx once and settle each row

- Status: accepted
- Date: 2026-10-03
- Deciders: project owner
- Applies to: the release after v1.15.0

## Context and problem

[ADR-0027](0027-detect-docx-headings-from-style-names.md) changed how a `.docx`
is split, but an index answers "unchanged" for a file whose content hash still
matches before the parser runs, so a `.docx` nobody edits keeps the chunks the
previous rule cut for as long as the index exists. `--force` repairs that, but
the desktop application never passes it, and its users never see stderr.

The question is how an existing index catches up once — every unchanged
`.docx`, on the first ordinary run — without re-embedding the documents that
already split correctly, and without a document that cannot be read holding
the catch-up open for every other one.

## Decision drivers

- One ordinary run (`groove index`, MCP `rebuild_index`, the desktop's call)
  is enough; nothing has to pass `--force`.
- A document that already splits correctly is not re-embedded.
- No row the old rule wrote stays behind the fast path once the catch-up is
  recorded as done.
- A run that stops early, by a cancel or an error, records nothing.
- The schema of `.groove.db` does not change.

## Options considered

1. **When is the catch-up done.**
   - When every `.docx` was read, as the frontmatter check of #251 is.
     Rejected: one `.docx` that cannot be read (over a size cap, or failing to
     parse) keeps it open, and then every run re-reads every `.docx`; the
     desktop has neither `--force` nor stderr to get out of that.
   - A column on each document row saying which rule split it. Exact, but a
     schema change with a migration, a column every write path has to fill —
     including the ones that skip unchanged work — and a record of its own.
   - **Settle each row in the run: rewritten, re-read and found to match,
     dropped, or marked with a content hash no file hashes to, so the fast path
     cannot keep it.** Taken. It needs no schema change, and its cost is one
     read per run for each `.docx` that cannot be read yet.
2. **The marker.** A key of its own, `index_meta.docx_heading_policy`, beside
   `frontmatter_policy` (#251) rather than shared with it: the two passes cover
   different files, do different work and finish under different conditions,
   and sharing would make either one's unfinished state hold the other open.

## Decision

- While `docx_heading_policy` is not `styles-name-basedon`, every index run
  that is not `--force` (`groove index`, the MCP `rebuild_index` tool, the
  desktop's call) re-reads every `.docx` whose row's hash is the scan's
  (`Reindex::Reparse`): matching chunks are left as they are and nothing is
  re-embedded; chunks that differ are written the ordinary way; no chunks at all
  remove the row, which is counted as skipped, the way `--force` would leave
  none. A `.docx` whose content changed takes the ordinary path, and a rename
  that is already forced stays forced.
- After the deletion sweep, in one transaction, every `.docx` row this run did
  not settle — the scan skipped it, it failed to parse, the endpoint refused
  it, or its bytes changed under the read — gets the content hash
  `awaiting-reparse`, and the key is written. The marks are written whatever
  the rows hold by then. A cancelled run, or one that returns an error, writes
  neither.
- `groove index` says before the loop how many documents it re-reads, unless
  `--quiet`; MCP `rebuild_index` and a callback reporter get no new output or
  event. The file watcher runs no pass.

## Consequences

- **The catch-up runs once.** Later runs take the fast path, except for each
  marked row, which every run reads the way it reads a changed file — its
  `Skipping ...` line repeats — until it can be indexed.
- A marked `.docx` that parses to no chunks on a later run keeps its old chunks and is read again on every run, the way any changed file that yields no chunks does today. Only the pass's own re-read removes a row that yields no chunks; aligning the changed-file path is a behaviour change for every format and is out of scope here.
- **A marked document that is moved** is a new file and a deletion, not a
  rename, because rename detection matches hashes.
- **The evaluation corpus digest does not move on the run that only writes marks.**
  It covers chunk rows (path, index, heading, content, context), not content
  hashes, so it changes only when the pass rewrites or removes chunks.
- **A daemon must be restarted first.** `groove serve` does not index at
  start-up and its watcher reads only files that change, so a daemon on the
  new version waits for one `rebuild_index`; a daemon still on the previous
  version keeps writing edited `.docx` files the old way after the key is
  recorded, which only `--force` undoes. A downgrade does the same.
- **Revisit when** a later change needs the same catch-up for another format:
  the per-row column (option 1) is the general form.
- **Tests hold it**: `grooveseek/src/indexer.rs` (the re-read, the predicate,
  the marker), `grooveseek/src/db.rs` (the key and the overwrite),
  `grooveseek/src/indexer/progress.rs` (the notice) and
  `grooveseek/tests/index_docx_heading_policy.rs` (the runs, the cancel, the
  callback, the watcher).

## References

- [ADR-0027](0027-detect-docx-headings-from-style-names.md), the rule this
  catches up with.
- [usage.md](../usage.md) for the run summary and the daemon order;
  [behavior.md](../behavior.md) for renames.
- Japanese version:
  [0028-reread-unchanged-docx-once-per-index.ja.md](0028-reread-unchanged-docx-once-per-index.ja.md)
