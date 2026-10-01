# 26. Make the index size caps configurable, spell "no limit" out, and keep the read caps

- Status: accepted
- Date: 2026-10-01
- Deciders: project owner
- Applies to: v1.15.0

## Context and problem

Until this record, one constant, `MAX_RAW_BINARY_BYTES` (50 MiB,
`grooveseek/src/parser/mod.rs`), decided six things at once: the file size at
which the indexer skips a binary file before reading it, the same check on the
handle it reads from, the decompression budget of the `.xlsx` preflight, the
cumulative budget of `.docx` / `.pptx` part reads, the text budget of one PDF,
and the binary cap of `get_document` / `resources/read`. `MAX_RAW_TEXT_BYTES`
did the first two for Markdown and plain text.

Workbooks of 300 MB exist in ordinary office use, and they were skipped with a
warning. The cap cannot simply be raised for everyone: `rebuild_index` is an
MCP tool a client can call at any time, the file is held in memory whole while
it is parsed, calamine also loads the whole shared-strings table of an
`.xlsx` into memory, and an allocation failure aborts the process. Neither
`catch_unwind` nor running the parse on another thread stops an abort.

The question is how to let an operator admit such a file knowingly, without
changing anything for a configuration that does not ask for it.

## Decision drivers

- A configuration that names none of the new keys indexes exactly what it did
  before.
- Raising one limit must not silently remove another defence.
- "No limit" has to be chosen knowingly, in a spelling nobody reads two ways.
- One MCP request's memory stays bounded whatever the index settings are.
- A configuration the operator did not choose must not be able to raise how
  much of a file a run holds in memory.

## Options considered

1. **How "no limit" is spelled.**
   - `0`. Rejected: products disagree on what it means. Recoll reads `0` as no
     limit for `filtermaxseconds` and as "reject everything" for
     `compressedfilemaxkbs`; Zoekt reads it as "use the default"; Open WebUI
     treats an unset or `0` `RAG_FILE_MAX_SIZE` as no limit for uploads and as
     100 MB for archive extraction, within one product. This repository
     already refuses `[embedding].max_input_chars = 0`.
   - `-1`. The most common choice (Elasticsearch `indexed_chars`, Apache Tika,
     Solr). Rejected: a negative byte count in TOML does not read as an intent,
     and it invites "and `-2`?".
   - Leaving the key out means no limit. Rejected: every server-type product
     surveyed (Onyx, Dify, Meilisearch, Qdrant, Weaviate, Nextcloud) treats an
     unset limit as a finite default.
   - **The string `"unlimited"`.** Taken. `0` and negative values are errors
     whose message points at it.

2. **The decompression budget.**
   - Tie it to the raw-byte cap. Rejected: the XML inside a workbook inflates
     to several times the file, so `max_binary_file_size = "300 MiB"` would
     still skip a 300 MB workbook at decompression, and the only way through
     would be `"unlimited"`, which removes the zip-bomb check from every file.
   - Tie it only when the raw cap is `"unlimited"`. Rejected: "no limit" would
     silently remove a second defence.
   - Bound it by ratio, as Apache POI (minimum inflate ratio 1%) and Tika
     (output to input 100) do. Rejected for now: the existing preflight counts
     absolute bytes in two layers (declared, then actually inflated), and
     changing the axis is a separate decision.
   - **A key of its own, `max_decompressed_size`, 50 MiB by default and
     independent of the raw cap.** Taken.

3. **What a read returns.**
   - Let `get_document` / `resources/read` follow the index caps. Rejected:
     one MCP request would then hold a 300 MB file in memory.
   - **Keep the read caps where they are: 50 MiB for binary formats, 1 MiB for
     text.** Taken. A document indexed past them stays searchable and carries
     no `uri`, through the size recorded in the index
     ([ADR-0005](0005-record-document-size-in-the-index.md)).

4. **Where the keys live.** A new `[limits]` section was rejected; `[index]`
   already holds the settings of `groove index` and `rebuild_index`, and the
   caps are a property of indexing.

5. **One key for binary and text.** Rejected: text has no chunk-size bound
   (Markdown is cut only at headings), so raising a shared cap would let one
   heading-less `.txt` become one enormous chunk.

## Decision

- `[index]` takes `max_binary_file_size`, `max_text_file_size` and
  `max_decompressed_size`. Each is a byte count of at least 1, a size with a
  unit (`B`, `KB`, `MB`, `GB`, `TB` in 1000s; `KiB`, `MiB`, `GiB`, `TiB` in
  1024s; case-insensitive; no fractions), or `"unlimited"`
  (case-insensitive). Each defaults to 50 MiB. `0`, negative values,
  fractions, unknown units and other words are errors that name the key.
- The caps travel on the parser registry (`Registry::limits`). The indexer
  reads the raw caps from there on every read path (the full run, the watcher's
  create / modify and its rename), and the binary parsers are built with the
  decompression budget (`with_budget`) and pass it to their deepest read. The
  fixed-cap helpers remain for tests only, so no production path checks a
  second time against 50 MiB.
- A refusal from the decompression budget names `max_decompressed_size`, and
  so does the warning for a single `.docx` / `.pptx` part skipped for being
  over it, which used to be dropped without a word; a raw-cap skip still says
  `file too large`. The two keys are told apart on stderr.
- The read side keeps 50 MiB under a name of its own,
  `GET_DOCUMENT_BINARY_MAX_BYTES`, so the same number answering a different
  question is not mistaken for the index default. Reads also keep the built-in
  decompression budget whatever `max_decompressed_size` says: `get_document`
  and `resources/read` parse through `parse_bytes_for_read`, which the four
  binary parsers answer with the default budget rather than the one they were
  built with, so one MCP request stays as bounded as it was.
- A value above the default is announced once per process with a warning:
  the file is held in memory whole, an allocation failure aborts the process,
  PDF extraction still stops after 120 s, and with `"unlimited"` memory use is
  bounded only by the files in the knowledge base.
- A configuration found beside a knowledge base cannot set the three keys, in
  either direction; they are dropped with a warning and the built-in caps
  apply. `--config` accepts them.
- Lowering a cap does not remove documents already indexed, as before.
- [ADR-0004](0004-resource-reads-are-bounded-by-the-index.md) and
  [ADR-0005](0005-record-document-size-in-the-index.md) speak of indexing up
  to 50 MiB. That figure is now the default. They are not edited; this record
  is the note.

## Consequences

- **A 300 MB workbook takes two keys.** `max_binary_file_size` admits the
  file and `max_decompressed_size` admits what it inflates to. The value to
  write for the second is measured on the operator's workbook, not known here.
- **Memory follows the configuration.** With the caps raised, peak memory
  grows with the largest file, and a file larger than the machine can hold
  ends the process instead of being skipped. The warning says so; nothing
  prevents it.
- **`groove doctor` stays yellow for a knowledge base that raised the caps on
  purpose.** A document indexed past the read cap is reported as
  `larger-than-a-read-returns` (Warning, exit 1), the same treatment text over
  1 MiB already gets.
- **Raising `max_decompressed_size` takes every binary document off offer.**
  A read parses with the built-in budget, and the index records only raw size,
  which cannot say which binary documents inflate past it. So while the key is
  above the default, no binary hit carries a `uri` and `resources/list` offers
  none; they stay searchable, and `get_document` still refuses one that
  inflates past the budget with the decompression message, or returns it
  without the parts that are over the budget on their own. Text is unaffected.
  `groove doctor` names the withheld binary documents as `binary-uris-withheld`
  (Warning, exit 1), so this too keeps a raised configuration yellow.
  Recording the inflated size at index time would let the server offer the
  binary documents that fit; that is not done yet.
- **Lowering a cap keeps what was indexed under the higher one** until
  `groove index --force` runs or the file shrinks under the cap, because the
  scan's size skip protects existing rows rather than deleting them. An edit
  that leaves the file over the cap keeps the old text searchable: the watcher
  records only the new size, and the next scan skips the file again.
- **The per-sheet and per-page text caps (1 MiB) are unchanged**, so a large
  workbook is indexed one truncated chunk per sheet. Raising them needs chunks
  smaller than a sheet first.
- **The PDF extraction timeout (120 s) does not move** with the raw cap; its
  arithmetic assumed a 50 MB input and is reviewed separately.
- **Revisit when** sheets are split into row-block chunks (the text caps can
  then join `[index]`), when files are parsed from a handle instead of a
  buffer (the warning's "held in memory whole" stops being true), or when the
  decompression budget moves to a ratio.
- **Tests hold it**: `grooveseek/src/parser/mod.rs` (the value grammar and the
  warning), the four parser modules and `registry.rs` (the budget reaches each
  read), `grooveseek/src/config.rs` (the keys, both registry arms, the
  untrusted rule), `grooveseek/src/indexer.rs` and
  `grooveseek/tests/index_size_caps.rs` (the three read paths), and
  `grooveseek/src/server.rs` (the read side does not move, its raw cap and its
  decompression budget alike).

## References

- [ADR-0005](0005-record-document-size-in-the-index.md), whose recorded size
  is what keeps a document indexed past the read cap off offer.
- [ADR-0004](0004-resource-reads-are-bounded-by-the-index.md), whose bounded
  read this record leaves unchanged.
- [configuration.md](../configuration.md) for the keys and the untrusted-config
  table; [behavior.md](../behavior.md) for what an operator sees.
- Japanese version:
  [0026-configurable-index-size-caps.ja.md](0026-configurable-index-size-caps.ja.md)
