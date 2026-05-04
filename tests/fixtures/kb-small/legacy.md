## Legacy document

This document has no YAML frontmatter at all. The parser should treat
it as a body-only Markdown file with metadata fields all `None`. This
exercises the no-frontmatter branch of the indexer that was added in
the parser registry refactor.

## Why we keep these around

Legacy notes from an older note-taking tool that did not emit
frontmatter still need to be searchable; the indexer must accept them
without complaint.
