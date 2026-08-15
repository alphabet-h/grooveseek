# 4. Resource reads are bounded by the index, not by the filesystem

- Status: accepted
- Date: 2026-08-15
- Deciders: project owner
- Applies to: v0.22.0

## Context and Problem Statement

kb-mcp gained the MCP `resources` capability in v0.22.0. A client can now ask
for `kb://doc/<path>` and receive a document's text.

That raises a question the project has already answered once, for a different
mechanism, and answered deliberately: **what bounds a read?**

[ADR-0003](0003-kb-mcpignore-bounds-indexing-not-access.md), accepted days
earlier, decided that `.kb-mcpignore` bounds *indexing* and not *access* —
`get_document` returns any file under `kb_path` whose extension is registered,
whether or not the index contains it, and
`document_in_excluded_dir_is_still_readable` pins that. The stated reason: a
rule that lives inside the tree cannot be the thing that guards the tree,
because whoever can write into the knowledge base can delete it.

`resources/read` could inherit that contract unchanged, or it could be
narrower. The question is not academic: a resource is something the **server
offered**, so a client asking for one is doing something different from a caller
who already knows a path and asks `get_document` for it.

## Decision Drivers

- Whatever is chosen must not *widen* what a client can reach. The previous
  release closed a hard-link hole; adding a second read path that skips a guard
  would give it back.
- ADR-0003 is recent and its reasoning is sound. Re-litigating it a week later
  on the same grounds would be churn.
- `resources/list` is a promise. A URI a client was handed and then cannot read
  is worse than one that was never offered.
- There must be exactly one sequence of guards. Two call sites with two copies
  is how a guard comes to apply to one of them — the failure mode `max_bytes_for`
  already exists to prevent one level down.

## Considered Options

1. **The same contract as `get_document`** — any file under `kb_path` with a
   registered extension, whether indexed or not.
2. **Index membership first, then exactly `get_document`'s guards.**
3. **`get_document`'s guards plus a live `.kb-mcpignore` check** on every read.

## Decision Outcome

**Option 2: a document is served as a resource only if it is in the index, and
then only through the same guards `get_document` applies.**

Option 3 is rejected outright. It rebuilds the boundary ADR-0003 declined,
on reasoning that has not changed in the days since, and it would make the
answer to "can I read this?" depend on a file any writer of the knowledge base
can delete.

Option 2 is narrower than option 1, so it cannot widen what is reachable. Its
justification is also *materially different* from the one ADR-0003 rejected: it
does not trust a file inside the knowledge base to police the knowledge base. It
trusts kb-mcp's own database — state the server built and owns.

The distinction that makes it right rather than merely safe is what a resource
*is*. `get_document` answers a caller who already knows the path; the contract
there is "anything under `kb_path` is readable, so keep secrets outside it".
`resources/read` answers a caller holding a URI **this server handed out**.
Serving a URI that was never on offer is not the same operation, and bounding it
by the offer is the natural contract rather than a restriction bolted on.

It also makes `resources/list` honest — but only if the listing is built from
what a read will actually accept, which is not the raw index. Narrowing
`[parsers].enabled` without reindexing deliberately *keeps* the rows for the
dropped extensions, and the extension check inside the shared guard then refuses
them. A listing built on index membership alone would therefore hand out
`kb://doc/…` links that the very next call rejects. So both the listing and the
read go through one query, `servable_document_paths()` — the indexed paths minus
those the active registry cannot open. "The listing offers what a read accepts"
is a property of a single list or of neither.

### Consequences

- A file that is on disk and would be returned by `get_document` is **not**
  readable as a resource until it is indexed. That is intended, and a test
  writes such a file and asserts the refusal.
- `.kb-mcpignore`d and `exclude_dirs`-excluded documents are absent from
  `resources/list` and unreadable through `resources/read`, because they are
  absent from the index. This happens as a consequence of ADR-0003's own
  contract, not as a new boundary: they remain readable through `get_document`
  exactly as before, and ADR-0003 is unchanged.
- The guard sequence lives in one function, `KbCore::load_document_blocking`,
  which both `get_document` and `resources/read` call. Symlink and hard-link
  refusal, path traversal, extension membership, the size cap and the
  handle-bound read all apply once, to both.
- A resource read returns the document's **text**, not the JSON envelope
  `get_document` produces, with the media type of what is served — `text/markdown`
  for Markdown, `text/plain` for anything delivered as extracted text. A PDF
  comes back as the text kb-mcp extracted from it, so calling it
  `application/pdf` would misdescribe the bytes the client is holding.
- Because the envelope is gone, anything it used to carry has to be said in the
  text or not at all. Extraction above 1 MiB is truncated, and `get_document`
  reports that in a `truncated` field; a resource read appends a marked notice
  instead. Serving the prefix bare would present part of a document as all of
  it — the same silent-loss shape BU-31 closed for a query cut at the phrase
  cap, and the same answer: hand over what there is, and say that is what it is.
- What this does **not** promise: that every offered URI reads successfully.
  Some refusals are conditions of the moment — the file was deleted since it was
  indexed, a hard link was renamed over it — and a listing cannot answer those
  at all. One is not: a document above `GET_DOCUMENT_MAX_BYTES` (1 MiB of text)
  is indexed, because indexing accepts 50 MiB, and then refused on read. That
  one is *knowable*, but only by stat-ing every indexed file on every listing,
  which would make the offer a live filesystem probe rather than a property of
  the index — and still would not close the gap, since a file can cross the cap
  between the listing and the read. The durable fix is to record the size at
  index time or to reconcile the two caps; until then this is a known
  limitation, not a claim. Measured on the reference corpus: 0 of 666 documents
  are within reach of the cap, the largest being 231 KiB.
- A row whose extension the parser registry no longer covers stays indexed —
  narrowing `[parsers].enabled` does not delete it — but is **not offered**: it
  is absent from `resources/list`, unreadable through `resources/read`, and its
  `search` hit carries no `uri` key. The hit itself stays, so the document
  remains findable; it simply carries no link to a read that would refuse it.
- Topic-group resources (`kb://topic/<prefix>`) list documents rather than
  serving them, so they expose nothing a `resources/list` did not already.
