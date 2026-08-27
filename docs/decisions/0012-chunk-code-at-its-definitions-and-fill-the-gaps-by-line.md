# 12. Chunk code at its definitions and fill the gaps by line

- Status: accepted
- Date: 2026-08-27
- Deciders: project owner
- Applies to: v1.2.0

## Context and Problem Statement

Every parser groove shipped before this one splits prose. Markdown splits at
headings; the binary formats split at the units their format already has —
a page, a sheet, a slide. Source code has no heading, and the unit a reader
actually wants back is a definition: the function, the struct, the method
that answers the question they asked.

Splitting code the way prose is split produces chunks that begin in the
middle of a function body and end in the middle of the next, which is
precisely the retrieval failure this feature exists to remove. But a
definition-only split has a hole in it: a file is not made of definitions
alone. Imports, top-level statements, the braces that frame an `impl` block,
and — the case that matters most — the regions a parser could not
understand, all sit outside every definition. A chunker that emits only
definitions silently drops them.

The question this decision answers: what is the unit of a code chunk, and
what happens to the bytes that unit does not cover?

## Decision Drivers

- A hit should be something a reader can act on. Half a function is not.
- No byte of a file should vanish because the parser did not recognise it.
  A file with a syntax error is the normal case in a knowledge base that
  indexes work in progress, not an edge case.
- The language-specific knowledge must stay in the grammar. groove supports
  languages it has never seen the syntax of (see
  [ADR-0013](0013-compile-in-one-grammar-and-load-the-rest.md)), so a rule
  that needs a per-language table on groove's side is a rule that does not
  scale past the first two languages.
- Chunk boundaries decide what the embedder sees. A boundary rule that
  cannot be explained to a user is one they cannot work with.

## Considered Options

1. **Fall back to the plain-text parser when a file does not parse cleanly.**
   The obvious safety net, and the wrong one: `TxtParser` turns a whole file
   into a single chunk. A 2,000-line file with one stray brace would become
   one chunk, which retrieves nothing usefully and hides the ninety
   definitions that parsed perfectly well around the break.
2. **Fixed-size or line-window chunks, ignoring structure.** Never loses a
   byte and needs no grammar. Also never returns a whole function: the
   thing that made code different from prose is thrown away at the first
   step.
3. **Definitions only.** Clean and easy to explain, but it drops imports,
   module-level constants in languages whose tags query does not capture
   them, and every unparseable region. What it drops is invisible: nothing
   in the output says a third of the file was not indexed.
4. **Definitions, with the uncovered bytes filled in by line** (chosen).

## Decision Outcome

Chosen: **option 4**. The unit is a `@definition.*` capture from the
grammar's own `tags.scm`. Every byte range no definition covers becomes a
line-based chunk with no heading.

What follows from that, and why each part is the way it is:

**A definition's range starts at its doc comment.** The tags query reports
the definition node, and the doc comment written above it is outside that
node. Left alone, the comment would land in the definition's chunk *and* in
the gap chunk above it — indexed twice, and counted twice by anything that
sums content. The covered range is therefore extended backwards over an
unbroken run of comment nodes, stopping at a blank line: a comment separated
from the definition is commentary on the file, not on the definition.

**A definition too large for the budget is split into its nested
definitions, and by lines when it has none.** A module or a class has
children to recurse into. A method does not — so for the shape that
dominates real code, splitting by lines is the ordinary path rather than the
exceptional one. Each piece keeps the heading and the kind of the definition
it came from, and reports its own line range, so a hit on the second half of
a long function still says which function it is.

**Line numbers describe the chunk, not the definition.** A doc comment
pulled in above a function is inside the range; a function split across
three chunks gives each piece its own. This is the only reading under which
"open the file at this line and you see this chunk" is true for every chunk
a code file produces.

**The kind is the grammar's word, not the language's keyword.** Rust's tags
query calls a struct, an enum and a union all `class`. Recovering the
keyword would mean mapping node kinds to display names per language, on
groove's side — the exact per-language table this design avoids. So
`symbol_kind` carries `class`, and the set of values grows as languages are
added rather than being fixed in an enum.

**Scope comes from walking the tree, not from the definition list.** Rust's
tags query captures `impl` blocks as references, never as definitions, so
`impl Database` is not a node in the definition tree. Yet it is exactly what
distinguishes two `open` methods in the same file. The parent chain is
walked for the first ancestor carrying a `name`, `type` or `trait` field —
three field names that hold across grammars, so this stays language-neutral.

**A gap fragment shorter than the short-content threshold is dropped**,
unless dropping it would leave the file with no chunks at all. The quality
filter alone is not enough: a closing brace on its own line scores below the
cutoff and is filtered, but a two-line fragment under the threshold scores
above it and would survive as a chunk of nothing. The threshold reused here
is the one the quality filter already uses, so there is one number rather
than two that can drift apart.

### Consequences

- A file contributes every byte it has, whether or not it parses. A syntax
  error costs the definitions inside the broken region, not the file.
- The scope of this decision stops at chunking and definition metadata.
  Linking a definition to the places that call it needs an edge table the
  schema does not have, and is tracked separately.
- Search results mix code and prose by default. Callers who want one or the
  other use the filters that already exist (`tags_any: ["code"]`,
  `path_globs` with a leading `!`); no new search parameter was added.
- One-line declarations — `pub mod x;` and unit structs — score below the
  quality cutoff and are filtered out. Measured at 12 of 435 definitions
  across six files of this repository's own source. They carry no
  information beyond their name, and the thing they name is indexed
  separately.

## More Information

The chunker lives in `grooveseek/src/parser/code/`. The companion decision
about which grammars ship and how the rest arrive is
[ADR-0013](0013-compile-in-one-grammar-and-load-the-rest.md).
