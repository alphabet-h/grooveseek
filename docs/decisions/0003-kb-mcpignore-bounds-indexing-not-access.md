# 3. `.kb-mcpignore` bounds indexing, not access, and uses `ignore` only as a matcher

- Status: accepted
- Date: 2026-08-15
- Deciders: project owner
- Applies to: v0.21.0

## Context and Problem Statement

Until v0.21.0 the only way to keep something out of the index was
`exclude_dirs`, a list of **whole directory basenames**. There was no way to say
"not `drafts/*.md`", "not `*.tmp.md`", or "not `archive/2024/**`". The obvious
shape for that is a gitignore-syntax file in the knowledge base, which is what
Cursor, ripgrep and most developer tools offer.

Two questions had to be answered before writing any of it, and neither has a
default that is merely a matter of taste.

**First, what does the file bound?** kb-mcp already had a deliberate,
test-pinned answer for `exclude_dirs`: it means "not indexed", not "not
readable". A file under an excluded directory never appears in `search`, but
`get_document` still returns it to a caller that knows its path
(`validate_get_document_path` takes no exclusion argument, and
`document_in_excluded_dir_is_still_readable` pins that). A new ignore file could
keep that contract or break it, and the industry is split: Cursor ships **two**
files for exactly this distinction — `.cursorindexingignore` for the index,
`.cursorignore` for access.

**Second, how much of the `ignore` crate to take.** It offers a whole directory
walker (`WalkBuilder`) as well as a matcher (`Gitignore`). kb-mcp already walks
with `walkdir`, in three separate places that have drifted apart from each other
twice.

## Decision Drivers

- Three surfaces — the full index walk, the `validate` walk (which lives in the
  binary target) and the live watcher — must not be able to answer the same
  question differently. AU-03 shipped with the watcher missing the hardcoded
  denylist; BU-19 shipped with the watcher comparing case-sensitively while the
  walks did not, so the full index skipped a `Build/` the watcher kept
  indexing. Both were found only after release.
- A guard must not claim more than it delivers. The project's own README already
  says the knowledge-base directory is not a security boundary, and BU-20 was
  told twice in review that its wording was stronger than its behaviour.
- A knowledge base without the new file must behave exactly as before.
- Gitignore is a real specification with sharp edges (anchoring depends on
  whether the pattern contains a slash, `!` blocks asymmetrically, `**` means
  three different things, `[B-a]` is a byte-ordered range). Independent
  reimplementations of it disagree with each other in practice.

## Considered Options

**Scope of the file**

1. Index only, matching the existing `exclude_dirs` contract.
2. Index and access: also refuse `get_document` / `get_best_practice`.
3. Two files, one for each, as Cursor does.

**Implementation**

1. `ignore::WalkBuilder`, replacing `walkdir`.
2. `ignore::gitignore::Gitignore` as a matcher only, keeping `walkdir`.
3. Gitignore semantics written onto the `globset` dependency already present.

## Decision Outcome

**Index only (option 1), with `ignore` used as a matcher only (option 2).**

The scope decision follows from what the file can actually guarantee. Whoever
can write into the knowledge base can also delete `.kb-mcpignore`; a rule that
lives inside the tree cannot be the thing that guards the tree. Refusing
`get_document` for ignored paths would look like an access control while resting
on a file any writer can remove — the shape BU-20 was corrected for. One
contract for both exclusion mechanisms is also simply easier to state: *nothing
excluded is ever indexed, and nothing indexed is the boundary on reading.*
Anything that must not be readable belongs outside `kb_path`, which is what the
README has always said.

Option 3 was rejected as concept count without a matching benefit: it doubles
the file, the documentation and the tests to express a distinction whose
stronger half we just declined to offer.

The implementation decision is what measurement produced. `WalkBuilder` brings
defaults that change behaviour invisibly for an existing knowledge base:
`hidden()` is true by default, and on Windows "hidden" means dot-prefixed **or**
carrying `FILE_ATTRIBUTE_HIDDEN`, so a note the user hid in Explorer would
silently leave the index. `add_ignore` resolves against the process's current
directory rather than the walk root, which for an installed service is
arbitrary. `require_git`, `parents`, `git_ignore` and `git_global` are all on by
default, making behaviour depend on whether the knowledge base happens to be a
git repository. And `filter_entry` accepts one predicate with no documented
ordering against the ignore check, while kb-mcp already filters there for the
hardcoded denylist, Office lock files, symlinks and hard links.

Taking the matcher alone leaves the existing walk untouched and, more
importantly, allows **one** function to answer the exclusion question for all
three surfaces — which is the failure mode this project has already paid for
twice.

Option 3 (hand-rolled on `globset`) was rejected on the evidence that
independent gitignore implementations diverge from each other in exactly the
edge cases nobody tests, and on a prior internal finding: when review keeps
landing edge cases on a hand-written matcher, that is the signal to delegate to
a library.

### Consequences

- The `ignore` crate is a new direct dependency. Its eleven transitive
  dependencies were already in `Cargo.lock` — `globset` and `walkdir` are
  already direct — so the addition is one crate, plus `regex-automata` moving
  0.4.14 → 0.4.18.
- `matched_path_or_any_parents` is **not** used, despite appearing to be exactly
  the needed API. Measured: with `["logs/", "!logs/important.md"]` it answers
  `Whitelist` for `logs/important.md`, while a walk stops at `logs/` and never
  reaches the file. Using it in the watcher would have reintroduced walk/watcher
  drift at the level of which API was called. The ancestor loop is written out,
  and stops at the first excluded ancestor.
- Matching is case-insensitive on every platform, unlike git's own default,
  because `exclude_dirs` and the hardcoded denylist already are (BU-19) and one
  configuration whose two exclusion mechanisms disagree about `Build` versus
  `build` is worse than either rule alone.
- Only the knowledge-base root's file is read: no subdirectory files, nothing
  above `kb_path`, and not `.gitignore`. Hierarchical files are what make a walk
  and a single-path check hard to keep in agreement, and honouring `.gitignore`
  would silently change what an existing knowledge base indexes.
- The file is not a security boundary and says so, in the module documentation,
  the README, and the doc comment on `validate_get_document_path`.
- Because the index is the boundary, a newly excluded document is removed from
  the database by the ordinary deletion pass on the next full index run — "not
  collected" already means "gone". The live watcher applies new rules to
  subsequent events only, and logs that.
