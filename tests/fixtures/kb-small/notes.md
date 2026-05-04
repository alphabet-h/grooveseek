---
---

## Quick notes

These are scratch notes that should still index correctly even though
the YAML frontmatter is empty. The frontmatter parser must accept the
`---\n---\n` form and treat all metadata fields as `None`.

## Edge cases

Empty frontmatter is a real-world pattern, often produced by tooling
that scaffolds a frontmatter block before any metadata is filled in.
