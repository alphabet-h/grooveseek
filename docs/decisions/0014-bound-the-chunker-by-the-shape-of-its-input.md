# 14. Bound the chunker by the shape of its input, not by a clock

- Status: accepted
- Date: 2026-09-03
- Deciders: project owner
- Applies to: v1.3.0

## Context and Problem Statement

The code chunker had one bound: a source file over 1 MiB is refused
([ADR-0012](0012-chunk-code-at-its-definitions-and-fill-the-gaps-by-line.md)
describes what it does with the rest). That bound counts bytes, and the input
that hurts is not large.

Working out the scope a definition sits in means walking from its node to the
root of the syntax tree, and tree-sitter's `Node::parent` is a search rather
than a stored pointer. The walk is repeated for every definition, so a file
whose definitions nest deeply pays the depth twice over and the total grows
with the cube of it. One line of `mod a{` repeated a thousand times, closing
braces appended, is under 10 KB and took 64 seconds to index; at 500 levels it
took 8 seconds; at 125, a third of a second. Nothing refused it, because the
only bound was the byte ceiling and the file is a rounding error against 1 MiB.

That is worse than a slow file. `rebuild_index` holds the embedder and the
database for the whole run, so a knowledge base with one such file in it stops
answering every request the server has until the run finishes. A knowledge base
is not necessarily written by the person searching it.

The question: what bounds a chunker whose cost is not a function of size?

## Decision Drivers

- The bound has to fire before the cost is paid, not after it.
- Indexing the same file twice, on two machines, has to produce the same
  chunks. The chunks are the index; a difference is not a slower answer, it is
  a different answer.
- A file that trips the bound should still be searchable. ADR-0012 promises
  that a file contributes every byte it has whether or not it parses, and a
  file groove declines to chunk carefully is not a file it should drop.
- Whatever the bound counts has to be something real code stays far away from,
  measured rather than guessed.

## Considered Options

1. **A wall-clock budget**, cancelling the parse or the tags query when it
   overruns. tree-sitter offers the hooks: `ParseOptions` takes a progress
   callback, and `TagsContext::generate_tags` takes a cancellation flag.
2. **A depth bound on the syntax tree**, measured after parsing and before the
   tags query.
3. **A bound on the scope walk itself** — the number of ancestors a definition
   may sit under — checked as the walk happens.
4. **Refusing the file**, as the byte ceiling does.

## Decision Outcome

Chosen: **option 3, with the fallback of option 4 rejected in favour of
degrading**. A definition may sit under 64 ancestors; the walk counts its steps,
and the first definition past the bound ends definition-based chunking for that
file. The file is then chunked by lines — the same shape a region no definition
covers already gets — and tagged `parse:too-deep`.

Why not the clock (option 1). A budget in seconds makes the index a function of
the machine that built it: the same file yields definition chunks on a fast
machine and line chunks on a loaded one, and re-indexing can change an answer
that nothing in the knowledge base changed. groove has refused a wall-clock
budget once before, for the connection graph, for the same reason. The hooks
would work; what they produce is not reproducible.

Why the scope walk rather than the tree (option 2). Both are properties of the
input, so both are reproducible. The difference is what they cost when they do
not fire: measuring the tree's depth means walking every node of every file,
which added 174 ms over this repository's own 62 sources against 424 ms of
parsing and 882 ms of tags queries. Counting steps inside a walk that already
happens costs an increment. The scope walk is also the thing that actually
blows up, so bounding it needs no argument about which proxy is close enough.

Why 64. Measured, not chosen: parsing every source file in this repository and
recording the longest walk gives 8, in `main.rs`. The deepest syntax tree
across the same files is 32, which is the number option 2 would have had to
clear — evidence that the two quantities are not interchangeable. 64 leaves
real code eight times the room it uses.

Why degrade rather than refuse (option 4). Refusing is one line, and the
per-file skip that follows it already exists. But ADR-0012 committed to a file
contributing every byte it has, and a file that is merely nested deeply is
readable — its text answers queries even when its structure is not worth
resolving. Falling back to the plain-text parser was not an option either: that
one produces a single chunk per file, the shape the code chunker exists to
avoid.

### Consequences

- A file past the bound keeps its content and loses its definition metadata: no
  `symbol_kind`, no heading, no scope context. `parse:too-deep` on the document
  makes that visible to a search rather than silent.
- `parse:too-deep` is a separate tag from `parse:degraded`, which means the
  grammar could not read part of the file. One says the parse failed, the other
  says groove declined to chunk what parsed.
- The bound is a constant, not a setting. The other limits the code parser
  enforces are constants for the same reason: a knob whose only effect is to
  weaken a defence against a pathological file is not worth the surface.
- An index built before this release keeps its chunks for files whose content
  has not changed, because unchanged content is not re-chunked. `groove index
  --force` rebuilds them.
- The bound does not touch a different pathological shape: a file with tens of
  thousands of definitions at ordinary depth. That cost is dominated by the
  tags query rather than the scope walk, and is bounded by the 1 MiB ceiling.

## More Information

The chunker lives in `grooveseek/src/parser/code/`. The measurements above were
taken on a release build, running the two binaries alternately on one machine
(`groove index --force` over generated fixtures); the numbers move by a third
between builds when they are not taken that way.
