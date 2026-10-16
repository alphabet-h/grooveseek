# 30. Mark table rows per chunk in a nullable column, and catch up through per-format keys

- Status: accepted
- Date: 2026-10-17
- Deciders: project owner
- Applies to: v1.20.0 (PR #NNN)

## Context and problem

Since v1.18.0 (`.docx`) and v1.19.0 (`.pptx`) a table row is one line of a
chunk, its cells separated by tabs. A reader that shows an excerpt, such as
grooveseek-desktop, has to guess which lines are table rows from the text alone,
and the guess fails both ways: a paragraph outside a table can hold a tab, and a
row whose first or last cell is empty starts or ends with a tab that a trim
removes. Only the parser knows which lines it wrote as rows, and it knows it per
chunk.

ADR-0028 and ADR-0029 kept the schema unchanged and caught existing indexes up
through policy keys. `.docx` now needs a fourth key, which meets ADR-0029's
revisit condition ("one format needs a fourth key of this kind").

## Decision drivers

- The chunk text does not change: no re-embedding, no change to full-text search,
  and the tests that pin the text keep passing.
- A reader must be able to tell "no table row here" from "not computed", and fall
  back to its own reading for the second.
- One ordinary run catches an existing index up, as ADR-0028 and ADR-0029 ask.
- The MCP responses do not change.

## Options considered

1. **Put a marker in the chunk text.** Changes the embedding and full-text input
   of every `.docx` and `.pptx` chunk, so every one of them is re-embedded.
   Rejected.
2. **A separate table from chunk id to spans.** Has to be kept in step with
   `chunks` on every write and delete path. Rejected.
3. **A nullable column on `chunks`.** Taken: NULL says "not computed" as it is,
   and the value goes with its row, which `upsert_document` already deletes and
   rewrites as a whole. ADR-0028 and ADR-0029 kept the schema unchanged so that
   no row had to say which rules wrote it; this column holds what a chunk
   contains (where its rows are), not a generation of rules, so it does not run
   against that.

For the catch-up:

1. **Generalise the per-format keys now** (ADR-0029's revisit condition). The
   tests pin the existing key values and that dropping a key reopens the pass,
   so a general mechanism would edit them. Rejected.
2. **Use the column's NULL as the catch-up mark.** It says nothing about the
   rules the chunk text was cut by, so the keys would stay anyway, and two
   mechanisms would answer one question. Rejected.
3. **One more key per format, on the same predicate.** Taken: a constant, a read
   and a write, and a notice.

## Decision

- `Chunk::table_rows: Option<Vec<TableRowSpan>>`, where `TableRowSpan { table,
  start, end }` is a byte range into `content` without the row's newline and the
  table's number in the chunk by first appearance. The `.docx` and `.pptx`
  parsers return `Some` (an empty list when there is no row); every other format
  and the text fallback return `None`.
- `chunks.table_rows TEXT` holds its JSON, NULL meaning not computed.
  `SearchResult::table_rows` returns it after checking it fits the content; a
  value that does not reads as `None`. `SearchHit` does not carry it.
- `docx_table_rows_policy` and `pptx_table_rows_policy` (`row-spans`) join the
  predicate of ADR-0029, each written last among its format's keys in the run's
  final transaction.
- A chunk whose text matches the parse and whose table rows do not gets that
  column alone, without re-embedding. The decision reads the stored hash; the
  write opens `BEGIN IMMEDIATE` and reads the hash and the text again before it
  writes, so no row keeps table rows computed from a text it does not hold. A
  re-read writes the column only while the row's hash is the hash of the bytes
  it parsed: a row another writer rehashed in between is left as it is and stays
  unsettled, so a later run reads it again. A changed file whose chunk text
  matches and whose row moved in between gets the column NULL, since that write
  replaces the hash.

## Consequences

- The first run of v1.20.0 re-reads every unchanged `.docx` and `.pptx` once and
  writes the column; nothing is re-embedded and nothing is counted as updated.
- **A daemon must be restarted first**: a daemon on the previous version writes
  edited files with a NULL column after the keys are recorded. A reader falls
  back to its guess for those rows, which is not wrong, but they stay NULL until
  the next edit or `--force`.
- `SearchResult` has a new field, so Rust code that builds one with a struct
  literal needs `table_rows: None`.
- A tab inside a cell stays a tab, so a span says "this range is a table row"
  and not where its cells split.
- **Revisit when** `.docx` needs a fifth key of this kind, a third format needs a
  catch-up, or a second per-chunk column is needed.
- **Tests hold it**: `grooveseek/src/parser/docx.rs`, `grooveseek/src/parser/pptx.rs`
  and `grooveseek/src/parser/ooxml.rs` (the spans), `grooveseek/src/db.rs` (the
  column, the read check, the keys), `grooveseek/src/indexer.rs` (the
  column-only write and its recheck, the notices) and
  `grooveseek/tests/index_table_rows_policy.rs` (the runs).

## References

- [ADR-0029](0029-reread-unchanged-documents-once-per-format.md), the predicate
  the new keys join; [ADR-0028](0028-reread-unchanged-docx-once-per-index.md).
- [usage.md](../usage.md) for the notices and the daemon order;
  [behavior.md](../behavior.md) for how tables are read.
- Japanese version:
  [0030-mark-table-rows-per-chunk.ja.md](0030-mark-table-rows-per-chunk.ja.md)
