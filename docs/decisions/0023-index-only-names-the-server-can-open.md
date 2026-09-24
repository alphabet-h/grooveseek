# 23. Index only the names the server can open, and ask one predicate for all of them

- Status: accepted
- Date: 2026-09-24
- Deciders: project owner
- Applies to: v1.14.0

## Context and problem

Which names reach the index and which names can be read back were decided by
different code. The index walk (`collect_source_files_under` in
`grooveseek/src/indexer.rs`) and the live watcher (`should_process_parts` in
`grooveseek/src/watcher.rs`) looked at exclusion rules, extensions and links,
never at the name. The read side — `kb://doc/` URIs and the lexical check
`get_document` runs before it touches the disk — asked `is_safe_relative` in
`grooveseek/src/resources.rs`, which on Windows refused a colon and a reserved
device name.

On Windows the verbatim `\\?\` prefix, which a canonical knowledge-base path
carries, lets files exist under names Win32 will not open as written: `CON.md`,
or anything under a directory called `dir.`. The walk indexed them; `search`
found them; no URI was offered for them and `get_document` refused them
(AW-43). The same gap ran the other way for spellings that Win32 rewrites:
`b.md.` and `dir./x.md` passed the lexical check and were only refused after a
look at the disk (AW-42), and `a/b?.md` came back as "unavailable" rather than
"not found" (AW-40).

Three review rounds on #319 each found another instance on this one axis. The
question this record answers is **where the rule for "a name the index can
hold" lives, so the next instance is a change to one function rather than a
fourth patch.**

## Decision drivers

- A search hit must be openable by the name it carries — the property
  [ADR-0004](./0004-resource-reads-are-bounded-by-the-index.md) set for resource
  reads, extended to what gets indexed at all.
- One question, one implementation (AGENTS.md): the walk, the watcher, the URI
  side and `get_document` must not be able to answer differently.
- Nothing is lost that can be opened today; Unix keeps its names.

## Considered options

1. **Index such names and make them readable** — open them through the
   verbatim prefix, and give them URIs. Rejected: every consumer of the path
   (gateways filtering on the requested string, clients, the URI parser) would
   have to learn that `CON.md` and `b.md.` are names on this server and not
   elsewhere, and `get_document`'s one-spelling rule would need an exception
   for names Win32 itself rewrites.
2. **Keep the current split and document it.** Rejected: it is the state that
   produced three findings in a row, and the next one would be the same patch.
3. **One predicate, asked everywhere; leave such names out of the index.**

## Decision outcome

Chosen option: 3.

- `resources::doc_is_addressable` is the one predicate for a name the index
  can hold. It is `is_safe_relative` plus what only a document name must
  satisfy: not empty, no `.` segment and no empty one (`./a.md`, `a//b.md`, a
  trailing `/`) — on every platform.
- `is_safe_relative` refuses, on Windows, besides the colon and device names:
  a segment ending in a dot or a space, and `< > " | ? *` or a control
  character. On Unix all of these stay ordinary names.
- The index walk prunes a directory it refuses and skips a file it refuses
  after the extension filter, with one warning each and a count on the
  `groove index` `Done in` line (the MCP `rebuild_index` reply does not carry
  the count). The walk judges only paths under the knowledge base as `kb_path`
  spells it.
- The watcher asks it after the extension filter, of the relative path it
  recovered from the event, so a path reached under another spelling of the
  knowledge base is judged too. It asks for every event it lets through —
  reindex, deindex and both ends of a rename — and for the files a newly
  arrived directory brought in, so a rename onto such a name deindexes.
- `get_document` asks it before the first stat. What its spelling check still
  answers is a name the index could hold that is not the document's one
  spelling: a case variant, an 8.3 short name, a symlinked directory.
- Rows an earlier version stored are removed by the next `groove index` (or
  `rebuild_index`), whose sweep deletes what the walk no longer collects.
  Until then `groove doctor` reports them on Windows as
  `name-not-spellable-on-windows`, from the database alone.

### Consequences

- Good: a search hit always carries a name `get_document` and a `kb://doc/`
  URI accept; the next finding on this axis is a line in one function.
- Good: `a?b.md` and `a.md.` get the misspelling answer without a look.
- Bad: a Windows file named `CON.md` or kept under `dir./` is not searchable
  until it is renamed. The warning and the `Done in` count say so.
- Bad: on Unix a file whose name holds `..` between backslashes (`a\..\b.md`),
  which `get_document` already refused, is now left out of the index as well.
  `groove doctor` does not report a row an earlier version stored for it,
  since its finding is Windows only; the next full index removes the row.
- Neutral: a daemon started on an index built before this keeps such rows
  until a full index runs; on Windows `doctor` exits `1` over them meanwhile.
  The watcher refuses such a name on a delete as well, so deleting the file,
  or renaming it to a name the index can hold, leaves the old row in place
  until that full index.
- Neutral: the test
  `the_spelling_refusal_names_nothing_for_spellings_the_lexical_check_lets_through`
  in `grooveseek/src/server.rs` was left unedited, as tests are in this
  project. Its inputs (`./HiddenVault/a.md` and the like) are now refused at
  the lexical check, so it no longer reaches the spelling check and its doc
  comment describes the older order. Both checks give the same reply, so what
  it asserts — the refusal names no path — still holds.

### Confirmation

Unit tests in `grooveseek/src/resources.rs`, `indexer.rs`, `watcher.rs`,
`doctor.rs` and `server.rs` pin each clause per platform, and
`grooveseek/tests/index_unspellable_names.rs` runs `groove index` on Windows
over a knowledge base holding `CON.md`.

## More information

- [ADR-0004](./0004-resource-reads-are-bounded-by-the-index.md) — resource reads
  are bounded by the index.
- "Naming Files, Paths, and Namespaces":
  <https://learn.microsoft.com/en-us/windows/win32/fileio/naming-a-file>
- Japanese version: [0023-index-only-names-the-server-can-open.ja.md](./0023-index-only-names-the-server-can-open.ja.md)
