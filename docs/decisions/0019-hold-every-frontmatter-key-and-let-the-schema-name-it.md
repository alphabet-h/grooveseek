# 19. Hold every frontmatter key, and let the schema name it

- Status: accepted
- Date: 2026-09-09
- Deciders: project owner
- Applies to: v1.9.0

## Context and problem

`groove validate` checks a document's frontmatter against `groove-schema.toml`. Until
this release the schema could name five keys — `title`, `date`, `topic`, `depth`,
`tags` — and no others: the parser deserialised the YAML block into a struct with
those five fields, and serde's default dropped every other key before anything
downstream could see it. A schema naming `status` was refused at load time, and a
"strict" mode that flags keys the schema does not name could not exist, because
the validator never learned which keys the block had carried.

A user running a 74-document platform-engineering corpus put it this way (#252):
a corpus with no domain metadata does not need strict mode; a corpus with domain
metadata cannot use it. Their frontmatter carries twenty-one distinct keys, of
which five were declarable. They proposed a one-line loader change —
`[fields.status] known = true` — so a schema could name a key without checking it.

[ADR-0010](0010-settle-what-the-1-0-command-line-freezes.md) removed a `--strict`
flag that was accepted and discarded, and priced its return: adding it back when
`[options].allow_unknown_fields` exists is a minor release.

The question this answers: **who decides which frontmatter keys exist — the parser
or the schema?**

## Decision drivers

- The loader change alone cannot make strict mode work. The gap is one layer down,
  in the parser, and any design has to change what `Frontmatter` is — which every
  parser constructs and every reader of the index touches.
- Frontmatter is held as strings throughout. `date` is a string the schema checks
  with a pattern; `depth` is a string; this has been the rule since the schema
  existed and the reporter did not ask for it to change.
- Declared is not required. Eleven of the reporter's twenty-one keys are
  conditional on `source`; a schema that made naming a key imply requiring it
  would flag every document they have.
- A key the parser keeps must not cost more than the YAML parser already paid.
  An alias bomb or a deep mapping under an unknown key is the same input it was
  before; retaining it must not walk it.
- The search index, its filters and the MCP tools are frozen surfaces
  (`docs/stability.md`). Validation is the one place this corpus needs the keys.

## Options considered

1. **`known = true` as a rule key** (the reporter's proposal). Rejected: on its
   own it cannot work, since the parser still drops the key; and once the parser
   retains the key it may as well retain the value, at which point `known` is a
   keyword for "an empty table".
2. **Retain unknown values as re-serialised YAML strings.** Two shapes instead of
   three. Rejected: a `pattern` would then match against YAML syntax, and a
   mapping would pass `type = "string"`.
3. **Fold all five named fields into one map with accessors.** Uniform, and the
   schema dispatch becomes one lookup. Rejected: every parser, the indexer, the
   server and their tests read the five fields by name today, and `tags` would
   lose the `Vec<String>` guarantee its type carries.
4. **Keep the five fields, add `extra: BTreeMap<String, FieldValue>` with three
   shapes.** Taken.
5. **Add `type = "bool"`.** Rejected: it breaks the strings-throughout rule for one
   shape assertion that `enum = ["true", "false"]` already expresses. The
   documentation says so rather than leaving it to be discovered.
6. **A second regex key for mixed tag lists.** Rejected: one pattern per field is
   a limit of the rule, and the reporter's axis/free tag agreement is a
   cross-field constraint that belongs to their external validator.

## Decision

- **The parser keeps every top-level key of the block.** The five it stores in
  their own fields are unchanged; every other key goes to `Frontmatter.extra`
  under its own name. The YAML merge key `<<` is the one exception: serde's
  flatten would surface it unexpanded, and it was never a field.
- **A retained value has one of three shapes.** A string, a boolean or a number
  is the string it prints as. A sequence whose every element is such a scalar is
  a list of strings. Anything else — a mapping, a sequence holding anything that
  is not such a scalar, a null, an unresolved alias — is opaque: the parser keeps
  the shape's name and never reads the value. In practice an alias under an
  unknown key arrives already resolved to the value it names, so the opaque case
  is a guard rather than a path a document takes. The classification is total
  over the YAML value type, with no default arm, so a new shape is a compile
  error rather than a silent fourth kind.
- **The schema names any key.** `[fields.<name>]` accepts any name; an empty
  table declares the key and checks nothing; `required = true` stays explicit.
  The existing rules apply to a retained value by its shape when `type` is not
  given, and against the declared `type` when it is. An opaque value satisfies
  `required` and reports one `type_mismatch` naming its shape against any rule
  that would read it.
- **`[options].allow_unknown_fields = false`, or `groove validate --strict`,
  reports each key the schema does not name as one `undeclared_field`
  violation**, in key order. The five named fields are always declared. A block
  the parser refused is still one `frontmatter_unparsed` and nothing else. The
  flag only tightens: there is no `--no-strict`.
- **Retained keys reach `groove validate` and nothing else, in this release.**
  Making a key the schema declares filterable — in `search`, `graph` and the
  MCP tools — is the intended next step, not a door this record closes (#289
  asked for that intent to be written down either way). It is a separate
  decision because it touches the index and the frozen result and filter
  surfaces, each with its own pricing; this record decides only what the
  parser keeps and what validation does with it.

## Consequences

- `groove validate --strict` parses again, removed in 1.0.0 by ADR-0010 and back
  in 1.9.0 with the meaning that ADR reserved for it.
  `[index].fail_on_frontmatter_error` keeps its own name for its own meaning.
- `Frontmatter` gained a field; every parser but Markdown leaves it empty. Code
  that constructs a `Frontmatter` by naming every field has to name this one.
- A schema that a 1.8.0 binary refused with `unsupported field` loads on 1.9.0.
  A schema that carried `[fields.tags] pattern` with no `type` started reporting
  in 1.8.0; nothing else that loaded before changes what it reports, because the
  default `allow_unknown_fields = true` is the behavior 1.8.0 had.
- `serde`'s flatten moves the whole struct onto its buffering path. Two things
  are pinned by tests: that a wrong-shaped named field (`topic: [a]`) is still
  refused, and that a number given to `title` is still coerced to the string
  `"123"` as it always was. No earlier test covered either.

## References

- Issue #252 (design input, the reporter's corpus and their conditions).
- ADR-0010 for the removal this reverses and the pricing it set.
- `grooveseek/src/parser/markdown.rs` (`RawFrontmatter`, `classify`),
  `grooveseek/src/schema.rs` (`check_extra`, `NAMED_FIELDS`).
