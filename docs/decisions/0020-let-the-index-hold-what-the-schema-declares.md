# 20. Let the index hold what the schema declares

- Status: accepted
- Date: 2026-09-09
- Deciders: project owner
- Applies to: v1.9.0

## Context and problem

[ADR-0019](0019-hold-every-frontmatter-key-and-let-the-schema-name-it.md) made the
parser keep every frontmatter key and let `groove-schema.toml` name any of them,
and stopped there: retained keys reached `groove validate` and nothing else. The
same user who asked for that (#289) showed where the rest of the cost sits. In a
74-document corpus with four metadata axes, 434 of 1,036 tag occurrences exist
only to mirror `status`, `environment`, `team` and `source` into `tags`, because
`tags` is the only frontmatter the search filters can see. The mirror needs an
external validator to stay honest, and `topic` ends up spent on document type.
They asked for one thing: filter on a retained key, exact string match, no
operators, only on keys the schema declares.

The question this answers: **who brings a declared key into the index, and when
— the indexer reading the schema, or the query reading it?**

## Decision drivers

- "Only keys the schema declares" has to be true of what a caller can filter on,
  not only of what a schema file says today. A filter surface that accepts any
  key and answers by luck is not the feature.
- The schema is a file the operator edits between runs. Nothing in `search` or
  `serve` reads it, and neither should have to: a query is answered from the
  index.
- An unchanged document is not read again. The indexer skips a file whose hash
  matches, so a schema that declares a new key would never reach documents that
  did not change — the same trap #251 met with `frontmatter_policy`.
- `frontmatter_unparsed` is a number CI keys on. A pass that reads healthy
  documents for a new reason must not count them as broken.
- The result and filter surfaces are frozen (`docs/stability.md`): a new field is
  a minor release, a changed meaning is not. The database's internal schema is
  not a contract.
- The reporter's benchmark records a class, *deprecated-trap*, that only an
  exclusion closes; and they asked for no operators.

## Options considered

1. **The indexer reads the schema and stores the declared keys' values in their
   own table; `search` filters on the table and never reads the schema.** Taken.
2. **Store every scalar the parser retained; validate the key against the schema
   at query time.** Rejected: `serve` and `groove search` would both need to load
   the schema, a knowledge base without one would need a separate rule, and the
   index would carry values nobody declared.
3. **A JSON column on `documents`, filtered in Rust after the query, like `tags`.**
   Rejected: a separate table lets the query itself narrow, and a list value is a
   second row rather than a second encoding.
4. **Exclusion as an operator (`status!=deprecated`).** Rejected: the reporter
   asked for exact match and no operators. A second flag, `--field-not` /
   `fields_not`, gives the exclusion the same exact-match shape and leaves the
   value grammar alone.

## Decision

- **`groove index` reads `<kb_path>/groove-schema.toml` — the file `groove
  validate` reads — on every run.** The keys it declares, minus the five the
  parser stores in their own fields, are the declared set. A schema that does
  not load stops the index with its error, as a configuration that does not
  load stops the binary: indexing without it would make every field filter
  answer empty and say nothing.
- **A declared key's value is stored per document in `document_fields`**, one
  row per scalar and one per list element, as the string the parser held. An
  opaque shape and a null hold no value and write nothing; an undeclared key is
  not the index's business. Only Markdown carries these rows.
- **The declared set is a generation key.** `index_meta.declared_fields` records
  the set a completed run used. When the set the schema declares differs, the
  run reads every unchanged Markdown document once more and rewrites its rows
  without re-embedding; it does not count a healthy document as a broken
  frontmatter, so `[index].fail_on_frontmatter_error` is untouched by a schema
  change. An index that predates the key behaves as if the set had changed.
- **`search` filters on the table.** `fields` keeps a document that holds one of
  the values for every key given; `fields_not` drops a document that holds one
  of the values for any key given, and keeps a document without the key.
  Exact, byte-wise comparison. A key not in the index matches nothing and is
  not an error, because the gate is the index. The command line spells it
  `--field key=value` / `--field-not key=value`, repeatable, split at the first
  `=`; the tool takes `fields` / `fields_not` as `key → list of strings` — one
  spelling, because a `string | list` union in the tool schema is the shape
  `schema_compat` exists to avoid (#75) — and `filter_applied` echoes it back.
- **`graph` is unchanged.** Extending the filter to `get_connection_graph` is a
  later decision.

## Consequences

- `groove-schema.toml` is now an input of the index, not only of validation. A
  schema edit is followed by the next `groove index`, with no flag.
- Two new rows can appear in `filter_applied`, two new tool parameters, two new
  flags: a minor release under `docs/stability.md`.
- `document_fields` is a table an index built by 1.8.0 does not have; opening it
  with 1.9.0 creates the table, and the first `groove index` fills it.
- The vector leg of the search appends the predicate to a sqlite-vec KNN query;
  a test pins that this is accepted and correct. Should a later sqlite-vec
  refuse it, the leg falls back to filtering in Rust after the query, the way
  the other filters already work.
- `doctor` does not yet look at the table. `status` does not count it.

## References

- Issue #289 (design input, the corpus and the ask).
- ADR-0019 for what the parser keeps; this record decides what the index keeps.
- `grooveseek/src/indexer.rs` (`declared_field_names`, `declared_field_rows`),
  `grooveseek/src/db/search.rs` (`field_predicates`), `docs/filters.md`.
