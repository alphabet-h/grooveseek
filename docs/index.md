<picture>
  <source media="(prefers-color-scheme: dark)" srcset="https://github.com/alphabet-h/grooveseek/raw/main/assets/logo-dark.png">
  <img src="https://github.com/alphabet-h/grooveseek/raw/main/assets/logo-light.png" alt="" width="56" height="56">
</picture>

# GrooveSeek documentation

MCP server for semantic search over a Markdown / plain-text knowledge base. The
command is `groove`.

**Installing it, and getting a first search running, are on the front page:**
[github.com/alphabet-h/grooveseek](https://github.com/alphabet-h/grooveseek).
What follows is the reference.

Every page exists in English and Japanese, and each links to its counterpart at
the top. 各ページに日本語版があり、冒頭で相互にリンクしています。

## Reference

| | English | 日本語 |
| --- | --- | --- |
| Every command — `index`, `status`, `serve`, `search`, `graph`, `validate`, `doctor`, `eval`, `tune`, `service` | [usage.md](usage.md) | [usage.ja.md](usage.ja.md) |
| Every `groove.toml` key, the discovery order, and which locations are trusted | [configuration.md](configuration.md) | [configuration.ja.md](configuration.ja.md) |
| `.mcp.json` recipes, the HTTP transport, the PostToolUse hook, the file watcher | [clients.md](clients.md) | [clients.ja.md](clients.ja.md) |
| The MCP surface: tools, prompts, and `kb://` resources | [mcp-tools.md](mcp-tools.md) | [mcp-tools.ja.md](mcp-tools.ja.md) |
| What gets indexed, where it is stored, and which files are refused | [behavior.md](behavior.md) | [behavior.ja.md](behavior.ja.md) |

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

ADR-0003's filename still says `kb-mcpignore`. The file it describes is now
`.grooveignore`; an ADR is not edited after it is merged, and
[ADR-0007](decisions/0007-rename-the-project-to-grooveseek.md) explains why
`kb-mcp` in anything dated before 2026-08-17 means this project.
