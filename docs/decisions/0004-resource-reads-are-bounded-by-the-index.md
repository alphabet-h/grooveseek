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

It also makes `resources/list` honest. The listing is built from
`all_document_paths()` and a read is checked against the same query, so a URI
the client was given cannot fail membership when it reads it back — the property
a listing has to have to be a promise.

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
- Topic-group resources (`kb://topic/<prefix>`) list documents rather than
  serving them, so they expose nothing a `resources/list` did not already.
