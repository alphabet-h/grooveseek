# 27. Detect docx headings from style names and their basedOn chain

- Status: accepted
- Date: 2026-10-03
- Deciders: project owner
- Applies to: the release after v1.15.0

## Context and problem

Until this record the `.docx` parser decided a heading from the spelling of the
style ID a paragraph names: `<w:pStyle w:val="Heading1">`, lowercased, had to
start with `heading` and end in a number. A style ID is an identifier the
writing application picks for each document, not a name. Word with a Japanese
interface writes its built-in headings under the IDs `1` .. `6` and names them
`heading 1` .. `heading 6` in `word/styles.xml`, so a document written that way
had none of its headings recognised and was indexed as a single chunk. The
test documents had no `word/styles.xml` and used `Heading1`-style IDs, which is
why nothing showed it.

The question is what decides that a paragraph is a heading, and what a
document without usable styles gets.

## Decision drivers

- A document split correctly today (style IDs such as `Heading1`, with or
  without a styles part) splits the same way.
- A document Word writes in any interface language splits at its headings.
- One rule, not two that can disagree about the same paragraph.
- A damaged or oversized styles part must not cost a document its place in
  the index.

## Options considered

1. **What decides.**
   - Names only, as Apache Tika and unstructured do: a style named
     `heading N` is a heading. Rejected: a custom style based on a heading
     style is not, and to its author it is a heading.
   - **Names and the `w:basedOn` chain, with an outline level of 9 on a
     derived style cancelling the heading** (pandoc's rule, plus the
     cancellation). Taken.
   - The effective outline level as well, as docling does: any style with an
     outline level from 0 to 8 is a heading whatever its name. Rejected: body
     styles that carry an outline level exist, and docling had to answer an
     issue (#4106) about them.
2. **When the table and the spelling disagree.**
   - A heading if either says so. Rejected: the cancellation, and a style
     whose name is not a heading's, would be overruled whenever the ID
     happens to be spelled `Heading...` — two rules.
   - **The table decides an ID it holds; the spelling decides only an ID it
     does not.** Taken.
3. **A malformed styles part.**
   - Use the styles that parsed before the break. Rejected: a style whose
     parent was lost ends its walk early and becomes body text that neither
     the whole table nor the spelling would give, and nothing shows which.
   - **Ignore the whole part, read every paragraph by spelling, and say so on
     stderr.** Taken.
4. **A styles part that the decompression budget cannot hold together with
   the parts already read.**
   - Fail the document, as for `docProps/core.xml`. Rejected: a document
     indexed today would drop out of the index for a part it can do without.
   - **Skip the part with a warning and fall back.** Taken.

## Decision

- `word/styles.xml` is read at a fixed path after `docProps/core.xml`, under
  the same decompression budget, through `ooxml::read_optional_zip_part`: a
  part that would take the document past the budget is skipped, never fatal,
  and is inflated only up to what is left of it.
- The table holds the paragraph styles (`w:type="paragraph"`, or no
  `w:type`) that are direct children of the root `w:styles`, keyed by style ID
  with entities resolved and compared byte for byte. The first definition of
  an ID wins, whatever its type. A style without a usable `w:name` is named by
  its ID.
- For a style ID the table holds, the walk follows `w:basedOn` from that style
  for at most 16 styles. The first style named `heading N` — read by the same
  function that reads the spelling: lowercased, `heading`, trimmed, a number
  from 1 to 255 — gives the level, unless the first style before it that sets
  an outline level sets 9. No heading within 16 styles, a cycle included, is
  body text.
- A style ID the table does not hold, and every paragraph of a document whose
  styles part is missing, malformed, over the budget on its own or skipped for
  the total, is decided by the spelling, as before. Each malformed state of the
  part and each budget case is named on stderr in one line. A part the zip
  layer cannot open or inflate falls back the same way without a line, as
  `read_zip_part` always has.
- An outline level never makes a heading by itself, neither a style's nor one
  written on the paragraph.
- The index and `get_document` parse through the same function, each under its
  own budget. Nothing new is configured.
- How an index written before this record catches up with it is a separate
  decision.

## Consequences

- **Word documents written in Japanese, and in other interface languages that
  number their heading IDs, split at their headings.**
- **Some documents written in English Word split differently.** A custom
  style based on a heading style is now a heading. Built-in headings
  (`Heading1` named `heading 1`) split exactly as before.
- **A heading's text leaves `get_document`'s content** for the documents this
  changes, as it already did for `Heading1`-style documents; full-text search
  still finds it in the chunk's heading.
- **A title styled to cancel the heading reads as body text**, as Word's
  `TOC Heading` does.
- **Not detected**: LibreOffice's localised style names, headings defined by
  an outline level alone, and a heading's numbering, which is not in its text.
- **A malformed styles part costs the whole table**, so a document written in
  Japanese Word whose `word/styles.xml` is broken stays one chunk; the warning
  names it.
- **A knowledge base that changed `[index].max_decompressed_size`** can hold a
  `.docx` near the limit that the index splits by styles while `get_document`
  reads it by spelling, or the other way round, because reads keep the
  default budget. Only whether heading text appears in `content` differs.
- **Revisit when** a real document indexed as one chunk turns out to have
  headings that neither a name nor `w:basedOn` reaches: an outline level
  alone, or a localised name.
- **Tests hold it**: `grooveseek/src/parser/docx.rs` (the table, the walk, the
  fallback, the budget), `grooveseek/src/parser/ooxml.rs` (the optional read),
  `grooveseek/src/server.rs` (the read side) and
  `grooveseek/tests/index_docx_heading_policy.rs` (the binary and its stderr).

## References

- ECMA-376 Part 1: §17.7.4.17 `style`, §17.7.4.9 `name`, §17.7.4.3
  `basedOn`, §17.3.1.20 `outlineLvl`.
- pandoc's docx reader (style names and `basedOn`); docling issue #4106
  (outline levels on body styles).
- [behavior.md](../behavior.md) for what an operator sees.
- Japanese version:
  [0027-detect-docx-headings-from-style-names.ja.md](0027-detect-docx-headings-from-style-names.ja.md)
