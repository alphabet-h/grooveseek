# 5. Record each document's size in the index

- Status: accepted
- Date: 2026-08-15
- Deciders: project owner
- Applies to: v0.23.0

## Context and Problem Statement

[ADR-0004](0004-resource-reads-are-bounded-by-the-index.md) established that a
resource read is bounded by the index, and that `resources/list` must therefore
offer only what a read will accept. It also recorded one place where that
property did not hold:

> a document above `GET_DOCUMENT_MAX_BYTES` (1 MiB of text) is indexed, because
> indexing accepts 50 MiB, and then refused on read. That one is *knowable*, but
> only by stat-ing every indexed file on every listing […] The durable fix is to
> record the size at index time or to reconcile the two caps; until then this is
> a known limitation, not a claim.

So a Markdown or plain-text document between 1 MiB and 50 MiB is indexed,
appears in a topic listing, carries a `uri` on its `search` hits — and is
refused when a client follows that link. Binary formats are unaffected: they are
read under the 50 MiB cap and their extracted text is truncated with a notice
rather than refused, which ADR-0004 already covers.

Nothing forced the question now. What made it the moment to answer is that
`kb-mcp doctor` was being added, and both changes are about what the index
knows about itself; deciding later would mean a second migration of the same
table.

## Decision Drivers

- ADR-0004's principle — an offer is a property of the index — is not in
  question. Whatever closes the gap has to keep it, not trade it away.
- The gap is currently unmeasurable in practice (0 of 666 documents on the
  reference corpus are within reach of the cap), so cost matters: this must not
  make the common path slower.
- Existing indexes must keep working. A knowledge base indexed by an earlier
  release cannot be required to re-embed to stay usable.
- The listing and the `search` `uri` have to change together. They are two
  surfaces answering one question, and only one of them was going through the
  shared predicate.

## Considered Options

1. **Stat every indexed file on every listing.** The option ADR-0004 named and
   declined.
2. **Reconcile the two caps** — refuse to index text above what a read can
   return.
3. **Record the byte size at index time**, in the `documents` table, and use it
   in the predicate that decides what is offered.

## Decision Outcome

**Option 3.**

Option 1 turns an offer from a property of the index into a live filesystem
probe, which is precisely the boundary ADR-0004 drew, and it does not even
close the gap: a file can cross the cap between the listing and the read.

Option 2 is backwards-incompatible in the worst direction. `GET_DOCUMENT_MAX_BYTES`
is 1 MiB for a reason that has nothing to do with indexing — it is how much text
kb-mcp is willing to hand an MCP client in one response — so reconciling the two
means lowering the *index* limit to it, and documents that are indexed and
searchable today would silently stop being indexed. A retrieval system that
drops a document because it is long is worse than one that finds it and declines
to inline the whole thing.

Option 3 keeps ADR-0004 intact: size becomes part of what the index knows, so
the offer stays a statement about the index. It costs one nullable column, and
the query that consults it asks only for rows past the *smallest* read cap — on
a corpus with no oversized document, that returns nothing.

`ServableRules` is now the single predicate behind both `resources/list` and the
`uri` on a `search` hit. It applies the per-extension cap through
`max_bytes_for`, the same chooser `load_document_blocking` passes to
`read_checked`, so the listing and the read cannot come to enforce different
limits. Before this change the two surfaces called the registry check
separately; that was harmless only while the predicate was a single call, and
adding a second condition to one of them is exactly what would have made a
`search` hand out a link `resources/read` refuses.

### Consequences

- `documents` gains `size_bytes INTEGER`, nullable. It is added to existing
  databases on open, like every other forward migration here.
- **NULL means "never recorded", not zero, and a NULL is offered.** Every row in
  an index written before this release starts that way, and reading unknown as
  "too large" would empty `resources/list` for anyone who upgraded without
  reindexing. The safe-looking reading is the destructive one here.
- The size is written wherever a `documents` row is written, including the
  frontmatter-only update path, where the bytes changed but the chunks did not.
- **An index run backfills what it skips.** `rebuild_index` answers
  `Unchanged` for a file whose content hash matches, and that path writes no
  document row at all — so a migration that only wrote from the two document
  writers would never run on the knowledge base it was written for, which is one
  where nothing changed. The backfill reads from the disk scan instead, beside
  `backfill_fts`, and `WHERE size_bytes IS NULL` keeps it from overwriting a
  size that a real read recorded.
- A text document past the read cap is now absent from `resources/list`, absent
  from topic-group bodies, and its `search` hit carries no `uri`. **The hit
  itself remains**, exactly as for an unregistered extension: the document stays
  findable, it simply carries no link to a read that would refuse it.
- A binary document past the *text* cap is still offered, because a read
  truncates it rather than refusing. The per-extension cap is what distinguishes
  the two, and it comes from the same function the read uses.
- **A file that grows past the index cap is refused *and* its new size is
  recorded.** Such a file is skipped, and a skip deliberately preserves the row
  — so without this the recorded size would stay the last one small enough to
  index while the file on disk became one no read can return. This belongs on
  the knowable side of the line rather than the unknowable one for a precise
  reason: kb-mcp stat'd the file a moment earlier in order to refuse it. It had
  the answer and was throwing it away. The full index run and the watcher both
  write it, because leaving one of them to be corrected by the other is the
  drift this whole feature is about.
- What this still does not promise is unchanged from ADR-0004: refusals that are
  conditions of the moment — the file was deleted since it was indexed, a hard
  link was renamed over it, it grew between a listing and the read that followed
  — remain unknowable to a listing. The distinction that decides which side a
  case falls on is whether kb-mcp measured it.
- `kb-mcp doctor` reports both the documents past the cap and the ones whose
  size is not recorded yet, so the state this migration creates is visible
  rather than silent.
