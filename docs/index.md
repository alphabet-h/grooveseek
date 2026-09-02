<picture>
  <source media="(prefers-color-scheme: dark)" srcset="https://github.com/alphabet-h/grooveseek/raw/main/assets/grooveseek-readme-hero-dark-v2.webp">
  <img src="https://github.com/alphabet-h/grooveseek/raw/main/assets/grooveseek-readme-hero-light-v2.webp" alt="Markdown files flow into a chunker, a semantic path and a lexical path converge on one node, and ranked results leave it for an MCP client." width="100%">
</picture>

# GrooveSeek documentation

MCP server for semantic search over a Markdown / plain-text knowledge base. The
command is `groove`.

> **日本語版**: [index.ja.md](./index.ja.md)

**Installing it, and getting a first search running, are on the front page:**
[github.com/alphabet-h/grooveseek](https://github.com/alphabet-h/grooveseek).
What follows is the reference.

Every page exists in English and Japanese, and each links to its counterpart at
the top.

## Reference

| | English | 日本語 |
| --- | --- | --- |
| Every command — `index`, `status`, `serve`, `search`, `graph`, `validate`, `doctor`, `eval`, `tune`, `service` | [usage.md](usage.md) | [usage.ja.md](usage.ja.md) |
| Every `groove.toml` key, the discovery order, and which locations are trusted | [configuration.md](configuration.md) | [configuration.ja.md](configuration.ja.md) |
| `.mcp.json` recipes, the HTTP transport, the PostToolUse hook, the file watcher | [clients.md](clients.md) | [clients.ja.md](clients.ja.md) |
| The MCP surface: tools, prompts, and `kb://` resources | [mcp-tools.md](mcp-tools.md) | [mcp-tools.ja.md](mcp-tools.ja.md) |
| What gets indexed, where it is stored, and which files are refused | [behavior.md](behavior.md) | [behavior.ja.md](behavior.ja.md) |
| Which process shape to deploy, what residency costs, and where the same-host boundary comes from | [deployment-topologies.md](deployment-topologies.md) | [deployment-topologies.ja.md](deployment-topologies.ja.md) |

## Retrieval

| | English | 日本語 |
| --- | --- | --- |
| RRF, reranking, MMR and parent retriever, in the order they run | [retrieval-pipeline.md](retrieval-pipeline.md) | [retrieval-pipeline.ja.md](retrieval-pipeline.ja.md) |
| Narrowing search results | [filters.md](filters.md) | [filters.ja.md](filters.ja.md) |
| `match_spans` and byte offsets, for quoting sources accurately | [citations.md](citations.md) | [citations.ja.md](citations.ja.md) |
| Measuring retrieval quality against a golden query set | [eval.md](eval.md) | [eval.ja.md](eval.ja.md) |

## Project

| | English | 日本語 |
| --- | --- | --- |
| Source layout, and how a query flows through it | [ARCHITECTURE.md](ARCHITECTURE.md) | [ARCHITECTURE.ja.md](ARCHITECTURE.ja.md) |
| What 1.0.0 freezes, and what it deliberately does not | [stability.md](stability.md) | [stability.ja.md](stability.ja.md) |

## Decisions

Architecture Decision Records — what was chosen, which alternatives were
rejected, and what it cost. [ADR-0000](decisions/0000-record-decisions-as-adrs.md)
describes when a decision is recorded and when a changelog entry is enough.

| | English | 日本語 |
| --- | --- | --- |
| 0. Record architecturally significant decisions as ADRs | [en](decisions/0000-record-decisions-as-adrs.md) | [ja](decisions/0000-record-decisions-as-adrs.ja.md) |
| 1. Withdraw `.xls` (legacy BIFF) support | [en](decisions/0001-withdraw-xls-legacy-biff-support.md) | [ja](decisions/0001-withdraw-xls-legacy-biff-support.ja.md) |
| 2. Compile queries into per-token `OR` phrases for full-text search | [en](decisions/0002-compile-queries-into-per-token-fts-phrases.md) | [ja](decisions/0002-compile-queries-into-per-token-fts-phrases.ja.md) |
| 3. `.kb-mcpignore` bounds indexing, not access, and uses `ignore` only as a matcher | [en](decisions/0003-kb-mcpignore-bounds-indexing-not-access.md) | [ja](decisions/0003-kb-mcpignore-bounds-indexing-not-access.ja.md) |
| 4. Resource reads are bounded by the index, not by the filesystem | [en](decisions/0004-resource-reads-are-bounded-by-the-index.md) | [ja](decisions/0004-resource-reads-are-bounded-by-the-index.ja.md) |
| 5. Record each document's size in the index | [en](decisions/0005-record-document-size-in-the-index.md) | [ja](decisions/0005-record-document-size-in-the-index.ja.md) |
| 6. Report a corpus that quotes the golden set, and require more than one quote | [en](decisions/0006-report-a-corpus-that-quotes-the-golden-set.md) | [ja](decisions/0006-report-a-corpus-that-quotes-the-golden-set.ja.md) |
| 7. Rename the project to GrooveSeek, and let the command be `groove` | [en](decisions/0007-rename-the-project-to-grooveseek.md) | [ja](decisions/0007-rename-the-project-to-grooveseek.ja.md) |
| 8. Declare what 1.0.0 freezes, and leave the Rust API out of it | [en](decisions/0008-declare-what-1-0-freezes.md) | [ja](decisions/0008-declare-what-1-0-freezes.ja.md) |
| 9. One DNS-rebinding gate, owned here | [en](decisions/0009-one-dns-rebinding-gate.md) | [ja](decisions/0009-one-dns-rebinding-gate.ja.md) |
| 10. Settle the three command-line questions ADR-0008 left open | [en](decisions/0010-settle-what-the-1-0-command-line-freezes.md) | [ja](decisions/0010-settle-what-the-1-0-command-line-freezes.ja.md) |
| 11. Exclude a term from both halves of the hybrid search | [en](decisions/0011-exclude-a-term-from-both-halves-of-the-search.md) | [ja](decisions/0011-exclude-a-term-from-both-halves-of-the-search.ja.md) |
| 12. Chunk code at its definitions and fill the gaps by line | [en](decisions/0012-chunk-code-at-its-definitions-and-fill-the-gaps-by-line.md) | [ja](decisions/0012-chunk-code-at-its-definitions-and-fill-the-gaps-by-line.ja.md) |
| 13. Compile in one grammar and load the rest | [en](decisions/0013-compile-in-one-grammar-and-load-the-rest.md) | [ja](decisions/0013-compile-in-one-grammar-and-load-the-rest.ja.md) |

ADR-0003's filename still says `kb-mcpignore`. The file it describes is now
`.grooveignore`; an ADR is not edited after it is merged, and
[ADR-0007](decisions/0007-rename-the-project-to-grooveseek.md) explains why
`kb-mcp` in anything dated before 2026-08-17 means this project.

ADR-0000 rules `.dev/` out as a home for decision records partly on the grounds
that it "has no nested repository, and is not backed up". That stopped being true
on 2026-08-10, when `.dev/` gained a private mirror. The decision stands on the
two grounds that did not change: a private mirror still does not arrive with a
clone, and public documentation still cannot link into it. The record is left as
written, for the same reason ADR-0003's filename is.
