# 17. Bound the chunk count without dropping bytes

- Status: accepted
- Date: 2026-09-06
- Deciders: project owner
- Applies to: v1.6.0

## Context and problem

[ADR-0012](0012-chunk-code-at-its-definitions-and-fill-the-gaps-by-line.md) makes one
promise about every source file groove indexes: it contributes every byte it has, whether
or not it parses. Nothing may be dropped for being outside a definition, and nothing may be
dropped for being in a region the grammar could not read.
[ADR-0014](0014-bound-the-chunker-by-the-shape-of-its-input.md) added a bound on how deeply
a definition may sit and kept that promise on purpose: a file past the bound is chunked by
lines rather than refused, and tagged `parse:too-deep` so the choice is visible.

A third bound predates both and kept no such promise. `MAX_CHUNKS_PER_FILE` says one file
may contribute at most 512 chunks, and it was applied by keeping the first 512 pieces and
dropping the rest. The pieces are sorted by position, so what it dropped was always the end
of the file. A file of six hundred definitions was indexed from its first line to somewhere
in its middle; the rest was in `documents` (the code parser keeps the source verbatim, so
`get_document` returned the whole file) and in no chunk, so no search could reach it. One
`tracing::warn!` line said so at index time and nothing else did — no tag on the document,
nothing for `groove doctor` to read back, nothing in the documentation.

It was also applied outside the branch that chose the fallback, so it truncated the scope
fallback's output as well: a file degraded specifically to keep it whole then had its tail
cut for the other reason.

The question this answers: **what does a bound on chunk count mean, when the file has to
survive it?**

## Decision drivers

- ADR-0012's promise is not a thing to trade against. A bound that keeps it costs
  granularity; a bound that breaks it costs content, and silently.
- A bound has to be a bound. Announcing "at most 512" and then producing six hundred is the
  same class of untruth as announcing it and then cutting the file.
- Chunks are the index, so the same file has to chunk the same way on any machine. This is
  why ADR-0014 rejected a wall-clock budget, and it applies to anything derived here too.
- Whatever the bound does has to be visible. ADR-0012 rejected "definitions only" partly
  because a third of a file could go missing with nothing in the output to say so.
- The knowledge base sets `[parsers.code].max_chunk_chars`, and there is no floor on it. A
  rule that is only safe at the default is not safe.

## Options considered

1. **Keep truncating, and say so.** Add the tag and the `doctor` finding the audit asked
   for, leave the tail dropped.
2. **Chunk by lines instead**, and accept that the count can exceed the bound when the
   configured budget is narrow.
3. **Chunk by lines instead, widening the line budget so the result fits the bound.**
4. **Drop the bound**, on the grounds that the 1 MiB byte cap already bounds the file.

## Decision

**Option 3.** A file whose definitions want more chunks than the bound allows is chunked by
lines — the same fallback the scope bound already used — and tagged `parse:too-many-chunks`.
The line budget is widened where it has to be so that what comes back fits the bound.

**Why not option 1.** The tail of a file being unsearchable while `get_document` hands it
back is not a limitation to write down; it is the failure ADR-0012 exists to prevent, and
writing it down would not give anyone a way out. `MAX_CHUNKS_PER_FILE` is a constant rather
than a setting, so the only remedy a finding could name would be "make the file smaller".

**Why not option 2.** The line fallback is bounded by the budget, and the budget is a
configuration key with no floor. A knowledge base that set `max_chunk_chars` to a small
number would make one 1 MiB file produce hundreds of thousands of chunks, every one of them
embedded while `rebuild_index` holds the embedder and the database — the availability shape
ADR-0014 exists to prevent, reached from the other direction. The bound would have become
advice.

**Why not option 4.** The byte cap bounds bytes, not chunks, and the two are not the same
question. `split_by_lines` starts a new piece only when the next line would overrun the
budget, so any two neighbouring pieces weigh more than the budget together, and a text
weighing `W` non-whitespace characters yields fewer than `2W / budget + 1` pieces. Putting
the shipped budget of 3500 and the 1 MiB cap into that gives roughly six hundred for a file
with no whitespace in it — over the bound. (An arithmetic bound, not a measurement; the
`the_widened_budget_keeps_the_split_under_the_bound` test checks the arithmetic against
`split_by_lines` itself rather than restating it.) Dropping the bound would leave no answer
at all to "how many chunks may one file contribute".

That same arithmetic is what option 3 uses. Asking for fewer than `limit` pieces gives a
budget of `2W / (limit - 1)`, and the fallback takes the wider of that and the configured
one. The widening is derived from the file, so the same file still chunks the same way on
any machine, and it is inert on any file that did not need it.

The bound itself stays at 512. Counting against this repository's own sources (62 `.rs`
files) in a scratch knowledge base, the widest file produced 297 chunks
<!-- via: target/release/groove.exe index --config groove.toml --force; SELECT d.path, COUNT(c.id) FROM documents d JOIN chunks c ON c.document_id = d.id GROUP BY d.id ORDER BY 2 DESC -->,
so ordinary code has room to grow by about three-quarters before it meets the bound. What
changed is what happens when it does.

### Consequences

- A file over the bound loses its definition metadata — no `symbol_kind`, no heading, no
  scope — and keeps every byte, exactly as a file past the scope bound does.
  `parse:too-many-chunks` is a separate tag from `parse:too-deep` rather than one shared
  "gave up" tag, because the input to change differs: split the file, or flatten the nesting.
- **The scope fallback is bounded now too**, and no longer truncated. Both fallbacks run
  through the same widening, so the bound is true of every file rather than of most of them.
- `groove doctor` reports the documents carrying either tag as
  `chunked-without-definitions`. That is the first time anything reads these tags back;
  `parse:too-deep` has been written since v1.3.0 and never surfaced.
- **A file with very long lines still degrades into few, very large chunks.**
  `split_by_lines` does not cut mid-line, on purpose, so a generated or minified file can
  come back as one enormous chunk. Every byte is in the index and reachable by keyword, but
  a chunk far larger than the embedding model's window is not well represented as a vector.
  That is the existing rule rather than something decided here — but widening the budget
  makes more files meet it.
- The pieces are still built before their number is known, so a pathological budget still
  costs the chunker one pass over the file. What it no longer costs is an embedding per
  piece.
- `[parsers.code].max_chunk_chars` no longer decides how many chunks a file may contribute,
  so it is no longer an availability question — it only decides how finely files that fit
  the bound are cut. The key had no floor when this was decided; since 2026-09-10 the config
  loader rejects values under 30, the smallest piece the chunker keeps on its own. That is a
  quality bound, not the availability bound this decision made unnecessary: the widening
  above still carries the chunk count by itself.
- An index built before this release keeps its chunks for every file whose content has not
  changed, because such a file never reaches the parser again. Those files may still be
  missing the tails the old truncation cut, and nothing on the document says so — the tag is
  written by the parser, and the parser is what that path skips. **So the index records which
  chunking policy built it** — this one when `--force` rebuilds it or when it holds no source
  file yet, and otherwise a marker saying it was built by something else. `groove doctor`
  reports an index carrying anything but the current policy rather than reporting a clean
  bill over damage it cannot see, and `groove index --force` re-chunks them.

  Emptiness is measured in source files rather than in documents, so a prose knowledge base
  switching code parsing on is not held back by notes that were never in question. The record
  is written where every insertion path meets, because `serve` needs no index to start and a
  knowledge base can get its first source file from the watcher without a full rebuild ever
  running. And the legacy state is *written down* rather than left as an absent key: the
  question costs something to answer, and a state that cannot be recorded is one every
  indexed file has to re-derive.

  The alternative was to guess: a truncated document has exactly the bound's worth of chunks.
  That was rejected because a file whose definitions produce exactly that many is left alone
  by this decision, so the two are indistinguishable — the guess would libel some documents
  permanently. A generation says only what is true: this index was written under a different
  answer, so the question is open.

## References

- The chunker is `grooveseek/src/parser/code/`; the bound and the widening live in
  `mod.rs`.
- ADR-0012 for the promise this restores, ADR-0014 for the fallback it reuses and the
  determinism requirement it inherits.
