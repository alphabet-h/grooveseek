# 32. Report why each file was not indexed; the count is derived from the list

- Status: accepted
- Date: 2026-11-01
- Deciders: project owner
- Applies to: v1.24.0, for the library's `rebuild_index` and
  `reindex_single_file_explained`, and for the MCP tool `rebuild_index`

## Context and problem

A desktop front end for groove wants to show its user which files the index
could not read and why, so the user can decide whether to ignore a file or act
on it (remove a password, raise a limit, save it again under another name).
The user is not a developer and does not see stderr.

Until now the library could not answer that:

- `IndexResult::skipped` was a count. The reason for each file went to stderr
  and was lost.
- Every parser failure became the same `parse failed`. An encrypted file, a
  file over the decompression budget and a corrupt file were told apart only
  in the text of the stderr line.
- The scan collected the paths it skipped into a set and dropped the reasons.
- A hard link and a name the index cannot hold
  ([ADR-0023](0023-index-only-names-the-server-can-open.md)) produced a warning
  only and were not counted in `skipped`, although both are "present on disk
  but not indexed", the definition [usage.md](../usage.md) gives for
  `skipped`.
- The MCP `rebuild_index` reply carried the count only, and its caller cannot
  read the server's stderr.

## Decision drivers

- One list per run, and a count that cannot disagree with it.
- Tell apart what the user can act on: encrypted, over a configurable limit
  (and which key), over a time limit, over a fixed limit of a parser, corrupt
  or unsupported, not UTF-8, too little text, text that decodes to mojibake.
- Do not change a single parser error message: stderr lines and existing tests
  hold them.
- Classify each failure in one place, shared by the full index and the
  watcher's single-file reindex.
- Files left out on purpose (ignore rules, unsupported extensions, Office lock
  files, symlinks) are not "could not read" and must not look like it.
- Keep the MCP reply bounded however many files are skipped.

## Options considered

1. **Where the list lives.** (a) `IndexResult` only. (b) A new
   `ProgressEvent::Skipped` only: the desktop would rebuild the list from
   events itself, and MCP has no events, so the library would need the list
   anyway. (c) Both: (b) means rewriting the one-file-one-event order of
   `ProgressEvent`, including what a file read twice in one run (a crossed
   rename read again) reports, with nothing needing it now. **(a) taken.**
2. **How the count is kept.** Keep adding to and subtracting from `skipped`
   at each write point and push to and pop from the list beside it, or write
   only the list during the run and derive `skipped` from it once at the end.
   The first is the shape that lets the two drift apart silently. **The second
   taken.**
3. **How a parser reports why.** Replace the `anyhow` errors with a new error
   type (changes every message path), carry an English detail string in the
   reason (no longer `Copy`, its size follows the parser's message, and the
   plain-language text belongs to the front end), or carry a typed mark inside
   the `anyhow::Error` whose `Display` is the old message. **The mark taken.**
   A nested reason (`ParseFailed { why }`) was rejected: a front end maps each
   variant to a sentence, and nesting only adds a level to map.
4. **Hard links and names the index cannot hold.** Leave `skipped` as it was
   and add a second count and list (`left_out`) beside it, or count them in
   `skipped`. The first makes the invariant "list == `skipped` + `left_out`"
   and has every consumer add the two. **Counting them in `skipped` taken**
   (decided by the project owner): they were already inside the published
   definition and had been missed, so this corrects the count rather than
   changing what the field means.
5. **A directory left out for its name.** List it only when it holds a file
   the index would have read, or list it once whatever it holds. The first
   means walking the subtree the walk never enters, at a cost that grows with
   its contents, and deciding again how links and names inside it are
   treated. **Once, marked as a directory, taken.**
6. **The MCP reply.** No cap (unbounded), counts per reason (the desktop uses
   the library and a model reading the reply is served by the first entries),
   a nested `reason` object (one level deeper, with no key collision to
   avoid). **The first N entries, flat keys and the number left out, taken.**
7. **`#[non_exhaustive]` on each struct variant.** Code outside the crate could
   then not build those variants, so a front end could not unit-test its
   mapping without running an index. Rejected; the enum as a whole is
   `#[non_exhaustive]`.
8. **Returning the list outside `IndexResult`.** It would change the type
   every caller of `rebuild_index` receives and split the invariant across two
   values. Rejected; `IndexResult` derives `Clone` instead, which the
   `..struct` updates in three existing tests needed once it held a `Vec`.

## Decision

- **The types.** `indexer::SkippedFile { path, reason }` and
  `indexer::SkipReason`, `#[non_exhaustive]`, with 16 variants: `TooLarge`
  (size, limit and the `LimitKey` that sets it), `OverExtractionLimit`,
  `OverParserLimit` (a fixed limit no key raises), `ExtractionTimedOut`,
  `Encrypted`, `CorruptOrUnsupported` (a parser panic included), `NotUtf8`,
  `TextUndecodable`, `TooLittleText`, `NoText`, `Unreadable` (with a
  `ReadFailure`: `InUse`, `PermissionDenied`, `Gone`, `Other`),
  `EmbedRejected` (the HTTP status), `ChangedDuringRead`, `NotPlainFile`,
  `HardLink` and `UnspellableName { directory }`. `SkipReason::kind()` is the
  only place the stable snake_case names live (`too_large`, `encrypted`, ...);
  `LimitKey::key()` and `SkipReason::limit_key()` are the only places the
  `[index]` key names live. The path is kb-relative with `/`, the spelling
  `documents.path` uses.
- **The list and the count.** `IndexResult::skipped_files` holds every file
  counted in `skipped`, one entry per path, sorted by path. During a run only a
  run-local ledger is written, and only through `upsert` (the later reason for
  a path replaces the earlier) and `remove`. `skipped_files`, `skipped` (its
  length) and `unspellable` (its `UnspellableName` entries) are written once,
  where every run ends, completed or cancelled. So
  `skipped_files.len() == skipped` holds for every value `rebuild_index`
  returns.
- **Parsers mark the cause.** A parser that refuses a document returns an
  `anyhow::Error` carrying an `Unindexable { cause: ParseCause }`; its
  `Display` is the message it printed before, unchanged. One function,
  `reason_of_parse_error`, turns an error into a `SkipReason` (the mark, else a
  UTF-8 decode error is `NotUtf8`, else `CorruptOrUnsupported`), and one,
  `read_failure_of`, classifies an I/O error.
- **Not on progress events.** `ProgressEvent` is unchanged.
- **The watcher's entry.** `reindex_single_file_explained` returns
  `Reindexed { result, reason }`. `reason` is `Some` exactly when
  `rebuild_index` would count that result in `skipped`, except for a path
  reached through a link, which the full index leaves out on purpose and lists
  nowhere (`None`). Both paths go through the same body, so they cannot
  classify differently. The existing functions stay as wrappers, and
  `watcher.rs` is not changed.
- **Hard links and names are counted.** The walk returns what it left out, and
  those entries are in the list and in `skipped`. A directory left out for its
  name is one entry with `directory: true`. These paths are not added to the
  set that protects a skipped file's row from the prune: an indexed document
  that became a hard link loses its row as before. `unspellable` becomes a part
  of `skipped`, like `embed_rejected`.
- **Not listed** (unchanged): files left out by `.grooveignore` /
  `exclude_dirs`, unsupported extensions, Office lock files (`~$*`) and
  symlinks.
- **The MCP reply.** `rebuild_index` adds `skipped_files` (the first 50
  entries of the library's list, in its order, each as `path`, `reason` and
  flat detail keys) and `skipped_files_omitted` (how many more). This is a
  separate change shipped in the same release as the rest; until it lands, the
  MCP `skipped` count already includes hard links and names, but the reply has
  no list.
- **`groove index` is unchanged**: no new output, and the `Done in` line keeps
  its wording.
- **This amends [ADR-0023](0023-index-only-names-the-server-can-open.md)** on
  two points of fact, without changing its decision: the MCP `rebuild_index`
  reply now carries the count of names the index cannot hold (inside
  `skipped`), and the `not indexed` count of the `Done in` line is no longer
  outside `skipped`. ADR-0023 carries a dated note saying so.

## Consequences

- **`skipped` grows by the hard links and names** (`groove index`'s `Done in`
  line, MCP `rebuild_index`, `IndexResult`). Minor version, since the
  published definition already included them.
- **The `not indexed` count of the `Done in` line is part of `skipped`
  (v1.24.0+).** It was outside before, and the two were added. The wording
  is unchanged (tests pin it), so a reader or script that still adds them
  counts names twice; only the docs and the changelog say so.
- **A run in which an indexed document became a hard link counts it under both
  `deleted` and `skipped`**: the row was removed, and the file could not be
  read. Two facts, not a double count.
- **A directory entry may hold no file the index would have read** (images
  only, or empty). A front end should show it as a folder whose name cannot be
  used, not as an unreadable file.
- **Separate files whose non-UTF-8 names have the same lossy spelling count
  once**, in `skipped` and in `unspellable`, where the loop used to count each
  file. `embed_rejected` stays per file, so it can exceed the matching list
  entries in that case.
- **The library's list has no cap**: one small entry per skipped file, next to
  the per-file state a run already holds. The cap is applied where the list
  leaves the process (MCP).
- **The watcher's entry still returns `Err`** when the first stat, resolve or
  read of a file fails; a failure of the second read is a skip with
  `Unreadable`. `rename_single_file` still drops the reason.
- **The vocabulary of `reason` grows by addition**, and a file reported with a
  generic value (`corrupt_or_unsupported`, `no_text`) may later move to a more
  specific one; a consumer treats an unknown value as a generic failure.
  Moving between specific values, or removing or renaming one, is a major
  change. [mcp-tools.md](../mcp-tools.md) states this with the MCP change.
- **Revisit when** the desktop wants the files as they are skipped (add
  `ProgressEvent::Skipped`, which `SkipReason` being `Copy` allows), or the
  watcher or the admin status must show what it could not read (and with it
  the reason `rename_single_file` drops).
- **Tests hold it**: `grooveseek/src/indexer/skip.rs` (the names, the keys,
  the classification, the ledger), `grooveseek/tests/index_skipped_files.rs`
  (every reason listed in path order, the count equal to the list on completed,
  cancelled and forced runs, hard links, intended exclusions left out) and
  `grooveseek/tests/index_skipped_unspellable_names.rs` (Windows: a name and a
  directory counted once).

## References

- [ADR-0023](0023-index-only-names-the-server-can-open.md), which names the
  files the walk leaves out for their name;
  [ADR-0025](0025-skip-rejected-inputs-and-retry-transient-embedding-failures.md),
  which counts endpoint refusals in `skipped`.
- [usage.md](../usage.md) for the `Done in` line, and
  [behavior.md](../behavior.md) for what is skipped and why.
- Japanese version:
  [0032-report-why-each-file-was-not-indexed.ja.md](0032-report-why-each-file-was-not-indexed.ja.md)
