# 15. Let a definition be short

- Status: accepted
- Date: 2026-09-04
- Deciders: project owner
- Applies to: v1.4.0

## Context and Problem Statement

The quality filter scores every chunk from three signals and hides anything
under `0.3` by default: length (under 30 characters, `-0.6`), boilerplate-only
text (`-0.5`), and poor structure (one line under 80 characters, `-0.3`).

Two of those three read shortness as a proxy for thinness. Chunks from a binary
format were already exempt from both, because a short page, sheet or slide is
the shape of the format rather than a thin section. A source-code definition was
not, because the exemption was carried on `Parser::is_binary()` and the code
parser is not a binary parser.

[ADR-0012](0012-chunk-code-at-its-definitions-and-fill-the-gaps-by-line.md)
recorded the result as a consequence: one-line declarations score below the
cutoff and are filtered out, "carrying no information beyond a name that is
indexed elsewhere". The published behaviour doc said the same, and the spec set
a condition for revisiting it — a report of a meaningful short definition being
lost from a real knowledge base.

That claim had evidence behind it, and it held: across the Rust sources of this
repository, the only chunks under the cutoff were `pub mod x;` and unit structs
(the population and the counts are in ADR-0012's own consequences section,
sourced from `feature-56-spike-2026-08-27.md` §5).
**What was looked at was Rust.** v1.3.0 shipped the first grammar for a
second language, and no report could arrive to test the claim, because the one
production knowledge base groove's author runs has code parsing switched off.

Re-measuring, over 11,002 definitions in three corpora rather than 435 in one
(via: `python census.py <db> <rows-out>`, recorded in
`av-07-short-definition-census-2026-09-04.md` §5):

| corpus | definitions | under the cutoff | name-only | carrying a value |
|---|---|---|---|---|
| this repository, `src` (Rust) | 3362 | 70 | 70 | **0** |
| this repository, `tests` (Rust) | 814 | 19 | 19 | **0** |
| CPython `Lib/*.py` (Python) | 6826 | 723 | 2 | **721** |

Rust reproduced the original finding at eight times the population. Python did
not. What falls out of a Python index is `MAXYEAR = 9999`, `HAVE_ARGUMENT = 44`,
`SF_NOUNLINK = 0x00100000` — pickle opcodes, token ids, stat flags, version
strings. **The value is the information, and the value is indexed nowhere else.**
The documented reason is not merely incomplete for these; it is false.

So the limitation was one language generalised to all of them, and the release
that made it reachable had just shipped.

## Decision Drivers

- The exemption has to be decided by what a chunk **is**, not by which parser
  produced it. `Parser::is_binary()` already answers a second question — which
  size cap `get_document` applies — and that answer travels far from here.
- Whatever separates a definition worth returning from one worth hiding has to
  be expressible in the scores that already exist. Anything else is a fourth
  signal, which the spike behind ADR-0012 considered and dropped.
- Whichever way this goes, the published limitation has to end up true.

## Considered Options

1. **Leave it.** Rust's own numbers say the limitation is accurate.
2. **Exempt one of the two penalties**, so that `type ShardId = u64;` returns
   while `pub mod x;` stays hidden.
3. **Add a signal that reads the definition's content** — a right-hand side, a
   body — and score name-only declarations down with it.
4. **Exempt a definition from both length-based penalties**, as a binary chunk
   already is, and rewrite the documented limitation.

## Decision Outcome

Chosen: **option 4**.

**Why not option 2 — it does not exist.** At the default cutoff, one penalty is
never enough to hide a chunk: length alone leaves `0.4`, structure alone `0.7`,
boilerplate alone `0.5`. And "under 30 characters" implies "under 80", so the
two length-based penalties always fire together or not at all. A chunk therefore
falls only when both fire, and exempting *either one* lifts every affected chunk
back over the line. `pub mod x;` is ten characters
(via: `echo -n 'pub mod x;' | wc -m`) and `type ShardId = u64;` is nineteen;
they take the identical pair. The granularity cannot tell them apart. Option 2
is option 4 with a doc that still lies — and it is the first thing anyone
reopening this question will reach for.

**Why not option 3.** Separating the two means reading what a definition
contains, which is the third quality axis already dropped once, or a per-language
table of `symbol_kind` values — the thing that keeping a grammar to data exists
to avoid. What it buys is bounded and known: it would re-hide 89 name-only
declarations across both Rust corpora (the table above) while the change recovers
721 values.

**Why not option 1.** It is a defensible answer for as long as every language
behaves like Rust, which stopped being true one release earlier.

The exemption is carried on a new `QualityProfile` (`Text` / `Binary` /
`Definition`) rather than by widening the existing boolean, so that the question
"which penalties apply to this chunk" has one answer in one place and does not
ride on a flag that also decides size caps.

### Consequences

- **Name-only declarations come back too, and no threshold takes them back.**
  That is the price of option 2 not existing, and the documentation now says so
  rather than the opposite. The second half is worth stating on its own: an
  exempt definition takes no penalty at all, so it scores exactly `1.0`;
  `min_quality` is clamped to `1.0`; and a chunk is dropped only when its score
  is *below* the threshold. There is therefore no value a caller can pass that
  removes `pub mod x;` while keeping anything else. What is left is excluding by
  path — a `path_globs` entry beginning with `!` — asking for the other half
  with `tags_any: ["code"]`, or not enabling the language for that tree.
- **The effect differs by language, because which one-liners are definitions at
  all is the grammar's decision.** Python's tags query captures module-level
  assignments. Rust's captures class, method, function, interface, module and
  macro, and **no constant**, so a Rust `const` reaches the gap-filling path
  instead and this decision does not touch it
  (via: `grep -n 'definition\.' <registry>/tree-sitter-rust-0.24.2/queries/tags.scm`).
- **The boilerplate penalty still applies to a definition.** A chunk whose whole
  text is `TODO` is thin whatever produced it. Only the two length-based signals
  are exempted.
- **The chunker had to stop producing one shape for this to be safe.** A
  definition over the chunk budget is split by lines, and each piece keeps the
  definition's heading and kind — so a split whose last cut landed before the
  closing brace produced a chunk whose text was `}` and whose heading was the
  function's name, which bm25 weights. The quality filter used to hide it; the
  exemption would have started returning it. A final piece under the
  short-content threshold is now folded back onto the piece before it, which
  makes the assumption this decision rests on true: **a chunk carrying a
  `symbol_kind` and shorter than that threshold is a whole short definition.**
  Gap and fallback pieces are untouched — they carry no `symbol_kind`, take the
  penalties as before, and ADR-0012 wants their thin tails kept rather than
  merged. An index built before this release keeps whatever chunks it has until
  the file changes or `groove index --force` rebuilds it.
- **An index built before this release catches up on its next `groove index`.**
  The backfill pass, which previously looked only at chunks still holding the
  column default, now also revisits chunks carrying a `symbol_kind`. It rewrites
  one column, so nothing is re-embedded and `--force` is not needed.
- **ADR-0012's consequence bullet no longer holds.** Its decision — chunking code
  at its definitions and filling the gaps by line — is untouched; only that one
  observation about the quality cutoff is superseded here.
- Search returns more chunks per code-bearing knowledge base than v1.3.0 did.
  Nothing is removed from a result set by this change.

## More Information

The scoring lives in `grooveseek/src/quality.rs`; the backfill that carries an
existing index across is `backfill_quality` in `grooveseek/src/db/meta.rs`. The
census that decided this — its corpora and the acceptance rule, both fixed
before the counting — is `av-07-short-definition-census-2026-09-04.md`, kept
with the project's development notes rather than in this repository.
