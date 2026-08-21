# Usage

Reference for the `groove` command line: `index`, `status`, `serve`, `search`,
`graph`, `validate`, `doctor`, `eval`, `tune`, and `service`.

> **日本語版**: [usage.ja.md](./usage.ja.md)

## Build / rebuild the search index

```bash
groove index --kb-path /path/to/knowledge-base
groove index --kb-path /path/to/knowledge-base --force   # full re-index
groove index --kb-path /path/to/knowledge-base --model bge-m3 --force  # switch to BGE-M3 (1024 dim, multilingual)
```

Scans source files under the given directory, skipping the default `exclude_dirs` set (`.obsidian`, `.git`, `node_modules`, `target`, `.vscode`, `.idea` — see "Directory exclusion" in [docs/behavior.md](behavior.md)). By default only `.md` is picked up. Add `[parsers].enabled = ["md", "txt"]` to `groove.toml` to also index `.txt` files — their title is derived from the filename (`deep-dive-2026.txt` → `"deep dive 2026"`) and the whole body becomes a single chunk. Files whose content hash has not changed since the last run are skipped unless `--force` is passed.

`--model` accepts:
- `bge-small-en-v1.5` (default) — 384 dim, English-focused, ~130 MB first download.
- `bge-m3` — 1024 dim, multilingual (100+ languages incl. Japanese), ~2.3 GB first download. Recommended for Japanese-heavy knowledge bases.

Switching models on an existing index requires `--force` (the DB records the model/dim in `index_meta` and rejects mismatched runtimes).

### Progress reporting flags (v0.7.8+)

Two flags control how `groove index` reports progress; they are mutually exclusive and default-off (the existing per-file `  indexed: foo.md (N chunks)` output is unchanged when neither flag is given).

- `--quiet`: suppress per-file output; only print start / `Found N source files` / `Done in ...` summary lines. Useful when running from harnesses (e.g. Claude Code Bash tool) that buffer streaming output until exit, so you can recognise "silence = still working" instead of confusing it with a hang.
- `--progress`: show progress UI. Auto-detects via `IsTerminal` on stderr — TTY gets an `indicatif` bar with elapsed / position / percent / ETA, non-TTY gets periodic `Progress: N/M (P%)` lines (~20 emits per run plus a 100 % anchor) so `tail -f indexing.log` works.

```bash
groove index --kb-path ./big-kb --quiet         # silent except for start / done
groove index --kb-path ./big-kb --progress      # bar in TTY, periodic lines in pipe
```

### Model selection trade-offs

| Aspect | BGE-small-en-v1.5 | BGE-M3 |
|---|---|---|
| First-time download | ~130 MB | ~2.3 GB |
| Embedding dim | 384 | 1024 (index file ~2.6× larger) |
| RAM when loaded | ~500 MB | ~2 GB |
| Index build time | baseline | ~3–10× slower (CPU inference) |
| Japanese precision | poor (English-centric vocab) | strong (multilingual tokenizer + training) |
| English precision | strong | comparable |

Switching cost (existing index → new model):

1. `groove index --kb-path ... --model <new> --force` runs a full re-embedding (no incremental update possible; `DELETE FROM documents/chunks/vec_chunks` and start over).
2. Every `serve` / `index` call afterwards must pass the same `--model` (or have it set in `groove.toml`). A mismatch is rejected at startup by the `index_meta` check.

Practical recommendation: pick the model that matches your knowledge base's **primary language** up front. Don't oscillate between models unless you have a concrete precision problem — the full re-embedding is the expensive step.

## Start the MCP server

```bash
groove serve --kb-path /path/to/knowledge-base
groove serve --kb-path /path/to/knowledge-base --model bge-m3   # must match the indexed model
groove serve --kb-path ... --model bge-m3 --reranker bge-v2-m3  # + cross-encoder reranking
groove serve --kb-path ... --transport http --port 3100         # HTTP, multi-client
groove serve --kb-path ... --no-watch                           # disable live-sync
```

Starts the MCP server on stdio transport by default (one client at a time). Pass `--transport http --port <PORT>` (or `--bind <SOCKETADDR>`) to serve multiple clients simultaneously via Streamable HTTP — details in the [HTTP transport](clients.md#http-transport-for-multiple-simultaneous-clients) section. A `--bind` outside loopback additionally requires `--i-know`, because groove ships no authentication.

The server exposes 6 tools (see [docs/mcp-tools.md](mcp-tools.md)) and keeps the index in-process for low-latency queries. `--model` must match the model that built the current index, otherwise the server refuses to start with an actionable error message. A file watcher (enabled by default) re-indexes affected files when the contents under `--kb-path` change — see [Live-sync via file watcher](clients.md#live-sync-via-file-watcher).

`--reranker` (optional, default `none`) enables a cross-encoder re-ranking pass over the top candidates of the hybrid search:

- `none` — disabled (default).
- `bge-v2-m3` — BAAI/bge-reranker-v2-m3 (multilingual 100+, ~2.3 GB first download). Recommended for Japanese knowledge bases.
- `jina-v2-ml` — jinaai/jina-reranker-v2-base-multilingual (multilingual, ~1.2 GB). Lighter alternative.
- `bge-base` — BAAI/bge-reranker-base (English/Chinese only, ~280 MB). Not recommended for Japanese.

Latency cost of rerank is roughly 300–700 ms per query on CPU with `bge-v2-m3` over 50 candidates. `--rerank-by-default <BOOL>` (on by default when `--reranker` is set) controls whether every `search` call uses rerank. It takes a value rather than being a bare flag, so turning it off is `--rerank-by-default=false`; the MCP tool takes `rerank: Option<bool>` to override per query. Switching the reranker does **not** require re-indexing (it is index-independent).

As of v0.27.0 the `rerank_by_default` key is read by `groove search` as well, so one `groove.toml` no longer means rerank here and no rerank there. On the command line the per-query override is `--reranker` itself: naming a model opts that query in even when the file says `false`, and `--reranker none` opts it out. (`groove eval` deliberately ignores the key — it measures whatever `--reranker` selects, and its run fingerprint records that model, so a run must not silently measure something else.)

### When to enable reranking

Rerank trades latency for precision. The right choice depends on usage pattern:

| Scenario | Recommendation |
|---|---|
| Interactive agent flows (the LLM calls `search` 2–5 times per turn) | **Leave off.** +500 ms × N search calls adds up fast; retrieval quality from BGE-M3 + heading-weighted bm25 is usually sufficient. |
| One-shot, precision-critical queries (research, definitive answers) | **Enable.** The latency tax is paid once per turn, and the cross-encoder meaningfully promotes semantically relevant candidates. |
| Mixed usage | Start with `rerank_by_default = false` and let the caller opt in per query — `rerank: true` on the MCP tool, or `--reranker <model>` on the command line. |

Symptoms that suggest you should turn rerank on:

- Top-5 results often miss the obviously right chunk even after query rewording.
- Queries that use synonyms / paraphrases of the indexed wording are failing (e.g. Japanese 「バグ」 vs English "error").
- The agent re-queries multiple times per turn, wasting context by reading wrong hits.

Because rerank is index-independent, you can enable it for a week, measure the quality delta, and disable it if the benefit is not visible — no re-indexing needed.

## Registering groove as an OS service (v0.8.0+)

`groove service install` registers the daemon as an OS-level user service (no admin/sudo required) and configures auto-start at login.

```bash
# Default: service name 'groove', bind 127.0.0.1:3100, auto-start ON
groove service install --kb-path /path/to/your-kb

# Multi-instance (= run multiple KBs as separate services)
groove service install --service-name work --kb-path /path/to/work-kb --bind 127.0.0.1:3100
groove service install --service-name personal --kb-path /path/to/personal-kb --bind 127.0.0.1:3101

# Inspect / manage
groove service status                              # default 'groove'
groove service status --service-name personal      # a named instance
groove service list                                # all instances
groove service uninstall --service-name personal               # remove unit, keep config + DB
groove service uninstall --service-name personal --purge --yes # also remove config + DB
```

OS-specific backends:
- **Linux**: systemd-user (`~/.config/systemd/user/groove-<name>.service`). Run `sudo loginctl enable-linger $USER` to keep the daemon running after logout.
- **macOS**: launchd LaunchAgent (`~/Library/LaunchAgents/com.groove.<name>.plist`). launchd writes the daemon's output to `groove.out` / `groove.err` in the config home; the plist sets `Umask` to `0077`, so everything the agent creates — those logs, the index database — is readable only by your account.
- **Windows**: Task Scheduler AT_LOGON (= no admin required, `\groove-<name>` task).

The installer writes a config home at `<dirs::config_dir()>/groove/<service-name>/` containing `groove.toml` (with `kb_path` and `bind`). Override the base directory via `GROOVE_CONFIG_HOME` env var. The registered launch line names that file with `--config` (v0.20.0+), so the daemon reads the config the installer wrote rather than whatever it discovers from its working directory — see [Trusted and untrusted config locations](configuration.md#trusted-and-untrusted-config-locations).

Non-loopback bind addresses (e.g. `0.0.0.0:3100`) require `--i-know` since groove has no authentication.

> **Migration from v0.7.x personal-http recipe**: The `grooveseek/examples/deployments/personal-http/` templates were removed in v0.8.0. Disable / delete the manually installed unit before running `groove service install`:
> - Linux: `systemctl --user disable groove.service && rm ~/.config/systemd/user/groove.service`
> - macOS: `launchctl bootout gui/<uid>/com.groove.groove && rm ~/Library/LaunchAgents/com.groove.groove.plist`
> - Windows: `schtasks /End /TN '\groove' ; schtasks /Delete /TN '\groove' /F` (replace `\groove` with whatever name the old task used)
>
> If you're carrying settings over from the old `groove.toml` (e.g. `model = "bge-m3"`, `exclude_dirs`, `best_practice`, `fastembed_cache_dir`), edit the **new** config at `<dirs::config_dir()>/groove/<service-name>/groove.toml` after install. **`kb_path` must be an absolute path** — the new daemon's `WorkingDirectory` is `config_home`, so a relative `kb_path = "./knowledge-base"` will resolve to `<config_home>/knowledge-base` and miss the real KB. Use TOML literal strings (single quotes) to avoid Windows backslash escapes: `kb_path = 'C:\Users\you\your-kb'`.

## Tray monitor (Windows only, v0.9.0+)

`groove-tray.exe` is a Windows system tray binary that visualizes daemon state and provides Start / Stop / Restart controls. It ships from v0.14.0 in its own archive, `groove-tray-x86_64-pc-windows-msvc.zip` — not inside the `groove` archive. Extract it next to `groove.exe`; `groove service install --with-tray` looks for it there. (Releases before v0.14.0 did not contain it at all: the two Windows companion binaries were built but never attached, so use v0.14.0 or later.)

Install alongside the daemon:

```bash
groove service install --kb-path C:\path\to\kb --with-tray
```

On next logon the tray icon appears with a colored status dot:

- **green** — daemon healthy (last `/api/admin/status` poll succeeded)
- **yellow** — daemon is indexing
- **red** — daemon has been unreachable for >= 1 minute (= 12 consecutive failed polls at 5s interval)
- **gray** — pre-first-poll (= within the first 5 seconds of startup)

Right-click reveals six menu items: **Status** (read-only line) / **Open Web UI** / **Start** / **Stop** / **Restart** / **Quit Tray**. **Start** runs the scheduled task; **Stop** terminates the daemon process by the pid it reports at `/api/admin/status` (v0.14.0+), then confirms it is gone by binding the daemon's address — `Stop-ScheduledTask` only ever stopped the launcher, which exits immediately, so it silently did nothing.

Tray logs live at `%LOCALAPPDATA%\groove\logs\tray.YYYY-MM-DD` (daily rotation). Set `GROOVE_TRAY_LOG=debug` for verbose output. Pass `--debug` to attach a console for live stdout/stderr.

Uninstalling the daemon also removes the tray shortcut:

```bash
groove service uninstall --service-name groove
```

To manage the tray shortcut independently of the daemon registration:

```bash
groove service tray-install --service-name groove     # add shortcut only
groove service tray-install --service-name groove --force   # overwrite an existing shortcut
groove service tray-uninstall --service-name groove   # remove shortcut only
```

The tray polls `127.0.0.1:<port>/api/admin/status`, so the daemon must be bound to either loopback (`127.0.0.1`) or a wildcard (`0.0.0.0`). A daemon bound to a specific NIC such as `192.168.1.5:3100` is not listening on loopback, and the tray logs a warning at startup so the misconfiguration is discoverable.

## Show index status

```bash
groove status --kb-path /path/to/knowledge-base
```

Reports on the existing index, **on stdout**, so `groove status | …` works: document and chunk counts, how many documents had unparseable `tags` frontmatter, and the context mode the index was built in (`static` / `off`). A fourth line reports how many chunks pass the quality filter — only when the effective threshold is above zero, so it is absent under `[quality_filter] enabled = false` or `threshold = 0.0`.

When there is no index yet, the "No index found" note goes to **stderr** and stdout stays empty — the command could not answer, so it produced no result. The wording of the lines above is not frozen ([docs/stability.md](stability.md)); for the two counts in a machine-readable form use `groove doctor --format json`.

## One-shot search from the command line

For shell scripts or skill bins that just need "search this string in the KB" without standing up an MCP connection:

```bash
groove search "RAG server comparison" --limit 3 --format text
groove search "E0382" --category deep-dive --format json | jq '.results[] | .path'
groove search "クエリ最適化" --reranker bge-v2-m3        # optional per-invocation rerank
```

`--format` is `json` (default, a `{ results, low_confidence, filter_applied }` wrapper as documented under "Search filters and citations" below) or `text` (LLM-friendly blocks separated by `---`). All other flags mirror `serve`: `--kb-path`, `--model`, `--reranker`, `--category`, `--topic`, `--limit`. The quality filter is on by default — pass `--include-low-quality` or `--min-quality 0` to restore the previous (filter-off) behavior for a single query. The `groove.toml` defaults apply exactly as in `serve`/`index`.

**How the query is matched** (v0.16.0+): the FTS half of the hybrid does not look for the query verbatim. It cuts the query at separators and at script boundaries (kanji / hiragana / katakana / other word characters), joins any fragment under the 3-character trigram floor to its neighbours, and searches for the resulting phrases joined with `OR` — so `再ランキングの評価について` looks for `再ランキング` / `ランキング` / `の評価` / `について`, and a natural-language question matches without appearing word-for-word. Wrap a substring in `"..."` to keep it together as one verbatim phrase (`groove search '"Foundry Local" の設定'`); quoting the whole query restores the pre-v0.16.0 substring search exactly. The same applies to the `search` MCP tool, which runs the same code path; none of this needs a re-index. Details: [docs/retrieval-pipeline.md](retrieval-pipeline.md).

Typical skill-bin use: a Claude Code skill places `groove.exe` + `groove.toml` in its `bin/`, then a command like `groove search "<user_query>" --format text --limit 3` returns a focused reference excerpt for the LLM to cite.

## Search filters and citations (v0.3.0+)

Starting in v0.3.0 the `search` MCP tool returns a wrapper object instead of a raw array of hits. **This is a breaking change** for clients that parse the response as `Vec<SearchHit>` directly:

```jsonc
{
  "results":        [{ "score": 0.83, "path": "...", "match_spans": [...], "tags": [...], ... }],
  "low_confidence": false,
  "filter_applied": { /* the echoed filters that were given; `{}` when none of them was. `min_quality` / `include_low_quality` apply without being echoed */ }
}
```

`results[].match_spans` are byte offsets into `content`, returned when every term the query splits into is ASCII, so MCP clients can quote the source text accurately. They are sorted and **non-overlapping**, and the 100-span budget is shared across the terms you searched for, so a term matching once is still highlighted when another matches hundreds of times; reordering the words of a query returns the identical array as long as it stays under the 32-phrase cap (v0.18.0+ — see [docs/citations.md](citations.md) for the full contract and that one caveat). `low_confidence` is a rank-based flag (`top1.score / mean(top-N.score) < min_confidence_ratio`); the threshold defaults to `1.5` and can be tuned via `[search].min_confidence_ratio` in `groove.toml` or `--min-confidence-ratio` per query.

Input bounds (defensive, v0.6.0+): `query` is capped at 1 KiB; longer inputs are rejected with an `ErrorResponse`. `match_spans` is computed only for chunks under 256 KiB and capped at 100 spans per chunk. These exist to bound abuse, not legitimate use — typical chunks are well under the ceilings.

The `search` tool / CLI also gained these filters in v0.3.0:

```bash
groove search "tokio spawn" \
  --path-glob "docs/**" --path-glob "!docs/draft/**" \
  --tag-any rust,async \
  --date-from 2026-01-01 \
  --min-confidence-ratio 1.5
```

- `--path-glob <PATTERN>` (repeatable) — include / exclude by path glob; `!`-prefix is an exclude. Give it once per pattern: it does **not** split on commas, because a glob's own syntax uses them (`docs/{a,b}/**` is one pattern, not two). MCP param: `path_globs`.
- `--tag-any <a,b,c>` — pass if the chunk has **any** of these tags. MCP param: `tags_any`.
- `--tag-all <a,b,c>` — pass only if the chunk has **all** of these tags. MCP param: `tags_all`.
- `--date-from <YYYY-MM-DD>` / `--date-to <YYYY-MM-DD>` — lex comparison; chunks with no `date` are excluded strictly when either bound is set. MCP params: `date_from` / `date_to`.
- `--min-confidence-ratio <N>` — per-query override of the `low_confidence` threshold. Must be finite and `>= 0.0`; `0.0` is how the check is turned off. The CLI rejects anything else before it loads a model, because a non-finite ratio compares false against every score and would quietly disable the flag rather than tighten it. The MCP parameter of the same name cannot refuse a value mid-conversation, so it substitutes instead: a non-finite ratio is logged and replaced by the server's own value, and a negative one is clamped to `0.0` — which disables the check rather than failing the call.

CLI `groove search --format json` answers with the same wrapper — `results`, `low_confidence`, `filter_applied` — and the same hit fields, with one exception: an MCP hit also carries a `uri` when the document is one the server will hand over, and a CLI hit never does. [docs/mcp-tools.md](mcp-tools.md) describes when that field is present. See [docs/citations.md](citations.md) for `match_spans` / byte-offset details and [docs/filters.md](filters.md) for the full filter reference.

## Diversity (MMR) and parent retriever (v0.7.0+)

Two opt-in retrieval-quality knobs land in v0.7.0. They are independent — enable either, both, or neither. Both default to **off** so existing pipelines behave exactly as before.

```bash
# MMR diversity re-rank
groove search "tokio runtime" --mmr true --mmr-lambda 0.7

# Parent retriever (expand short chunks to adjacent siblings or whole doc)
groove search "k=60 in RRF" --parent-retriever true

# Both at once
groove search "context management" --mmr true --parent-retriever true
```

CLI flags (also accepted by `groove eval`):

- `--mmr <bool>` — enable MMR diversity re-rank. Default `false`.
- `--mmr-lambda <0..1>` — MMR balance: `1.0` is "no diversity" (= MMR off behavior), lower values lean toward exploration / less redundancy. Default `0.7`.
- `--mmr-same-doc-penalty <0..1>` — extra cost when an already-selected chunk lives in the same document. `0.0` is pure MMR; raise to actively deduplicate same-doc chunks. Default `0.0`.
- `--parent-retriever <bool>` — when a hit chunk's token count is below `whole_doc_threshold_tokens`, expand its `content` to adjacent siblings (level-aware) or, for very short chunks, the whole document. The score, rank, path, and `match_spans` of the original hit are preserved; only `content` (and a new optional `expanded_from`) changes. Default `false`.

MCP `search` tool gains the matching per-call params `mmr` / `mmr_lambda` / `mmr_same_doc_penalty` / `parent_retriever`. Toml defaults live in `[search.mmr]` and `[search.parent_retriever]` (see [docs/configuration.md](configuration.md)). Per-call params override toml; toml overrides built-in defaults.

The pipeline order is **`RRF → reranker → MMR → parent retriever → match_spans`**. MMR re-orders candidates while the reranker score is still on the chunks; parent retriever runs last so the expanded content does not contaminate the relevance signal. See [docs/retrieval-pipeline.md](retrieval-pipeline.md) for the full pipeline narrative and tuning advice.

## Contextual Retrieval (v0.12.0+, opt-in)

Each chunk can be prefixed with a short, **statically generated** context breadcrumb — the document title plus its heading ancestry (`Doc Title > Section > Subsection`, ` > `-joined) — and that breadcrumb is injected into the embedding input, the FTS5 index (a dedicated third column, scored via a Contextual BM25 weight), and the reranker input. Unlike Anthropic's original Contextual Retrieval technique, this context is derived purely from document structure at index time — no LLM call, no extra runtime dependency, no staleness beyond what a normal re-index already handles.

Enable it via:

```toml
[contextual]
enabled = true
```

**This defaults to off**, and the reason is a measured regression, not caution for its own sake: an A/B evaluation on a 574-document dogfood knowledge base (bge-m3 embeddings) showed that with groove's actual default pipeline (no reranker), enabling static context injection made retrieval *worse* — recall@5 dropped from 0.707 to 0.627 (-0.080) and MRR dropped by -0.041. The short chunk-local vector signal gets diluted by the prefixed breadcrumb text when nothing downstream re-scores the result.

**With a reranker enabled** (`--reranker bge-v2-m3`), the picture flips: context injection improved every metric except a small recall@10 dip — recall@5 went from 0.760 to 0.807, MRR from 0.848 to 0.950, and nDCG@10 from 0.814 to 0.858. The cross-encoder reranker is able to use the extra structural signal that the raw embedding/BM25 stage cannot fully exploit on its own.

**Recommendation**: only turn `[contextual] enabled = true` on if you also run with a reranker (`--reranker bge-v2-m3` or similar / `reranker = "bge-v2-m3"` in `groove.toml`). Leave it off for the plain default pipeline.

Notes:

- The returned search result schema is **unchanged** — context is an internal signal for ranking only, never exposed in `search` / `get_document` output.
- Turning this on for an **existing** database requires `groove index --force` to rebuild the embeddings and FTS index with context injected; without `--force`, a config/DB mode mismatch just prints a warning on stderr and the database keeps its current mode (no silent mid-migration mixing of embedding spaces).
- `groove status` reports the DB's current mode as `Context mode: static` or `Context mode: off`.
- See [docs/ARCHITECTURE.md](ARCHITECTURE.md) for how the context breadcrumb is generated and stored.

## Connection graph from a starting document

When you want to find not just a single document but the semantic neighborhood around it (and neighbors of those neighbors), use the `graph` subcommand:

```bash
groove graph --start deep-dive/mcp/overview.md --depth 2 --fan-out 5
groove graph --start notes/rag.md --dedup-by-path --format text
groove graph --start a.md --exclude-paths junk1.md,junk2.md --min-similarity 0.5
```

Flags:

- `--start PATH` — required, relative path to an indexed document. MCP param: `start`.
- `--depth` (default 2, clamped to max 3) — BFS hops.
- `--fan-out` (default 5, clamped to max 20) — neighbors per node per hop. `0` returns only the seed.
- `--min-similarity` (default 0.3) — cosine similarity cut-off. `0.0..=1.0`.
- `--seed-strategy` — `all-chunks` (default) expands from each seeded chunk of the start doc; `centroid` averages them (L2-renormalized) into a single seed node, leaving all of `--max-nodes` except that one node for connections. Both see only the first `--max-seed-chunks` chunks. (The MCP tool spells this value `all_chunks`; **both spellings are accepted on both sides** — see [stability.md](stability.md#how-the-two-surfaces-name-the-same-thing).)
- `--max-nodes` (default 100, clamped to max 2000) — total nodes; also caps the number of KNN queries.
- `--max-seed-chunks` (default 32, clamped to `1..=1000`) — chunks of the start document used as seeds.
- `--exclude-paths` — comma-separated paths to drop from results. The start path itself is always excluded. MCP param: `exclude_paths`.
- `--dedup-by-path` — collapse same-path hits so each document appears at most once.
- `--category` / `--topic` — apply category / topic filters to every hop.
- `--format json|text|dot|svg` — `json` (default) and `text` are the machine- and human-readable listings; `dot` and `svg` draw the walk (v0.25.0+, see below).

### Seeing the shape (`--format dot` / `--format svg`)

`json` and `text` list the same nodes, but neither shows where the walk branched — which is the part worth looking at. Two drawing formats do:

```bash
groove graph --start notes/rag.md --format dot > graph.dot   # then: dot -Tsvg graph.dot
groove graph --start notes/rag.md --format svg > graph.svg   # opens in any browser
```

`dot` is a [Graphviz](https://graphviz.org/) program: pipe it to `dot -Tsvg` / `-Tpng` / `-Tpdf`, open it in a DOT viewer, or paste it into one of the web ones. `svg` is a finished picture that needs nothing installed — groove lays it out itself, taking no drawing dependency, because the graph is a tree and a tree lays out in one pass.

Both color nodes by BFS depth, label edges with the similarity score, and **say so when a limit cut the walk short**, so a picture is never mistaken for the whole neighbourhood. For a wide graph the Graphviz route gives the more compact page; the built-in SVG stacks one row per leaf, so `--max-nodes` is the knob that keeps it readable.

The output is a flat array of nodes with `parent_id` / `depth` / `score` so the consumer can reconstruct the tree if it wants. Good use cases: "give me 30 chunks of related context around this note for the LLM to read", or "walk two hops from this overview to see what topics it touches".

## Validate frontmatter against a TOML schema

If your knowledge base follows a frontmatter convention, `groove validate` checks every `.md` file against a TOML schema and reports violations. See the [Frontmatter schema validation](clients.md#frontmatter-schema-validation) section for the schema format; the command itself is:

```bash
groove validate --kb-path /path/to/knowledge-base
groove validate --kb-path ... --format json | jq '.files[]'
groove validate --kb-path ... --format github         # ::error annotations for CI
```

Flags:

- `--schema <PATH>` — read the schema from somewhere other than
  `<kb-path>/groove-schema.toml`. This is the only way to point `validate` at a
  schema that does not sit beside the knowledge base — one shared schema for
  several bases, or a stricter one kept in CI.
- `--fail-fast` — exit 1 at the first violation instead of scanning the rest.
  Useful when the answer you want is "is it clean", not "what is wrong".
- `--no-color` — drop ANSI color from `--format text`. Color is already off
  when stdout is not a TTY, so this is for the case where it is one.

Exit codes: `0` (no violations), `1` (violations), `2` (schema load error). When `groove-schema.toml` is absent under `--kb-path`, the command exits 0 with a short "no schema found" note, so adding `groove validate` to an existing workflow is non-disruptive until you actually write a schema.

## Check the index itself (v0.23.0+)

`groove validate` checks your documents. `groove doctor` checks the **index**:

```bash
groove doctor --kb-path /path/to/knowledge-base
groove doctor --kb-path ... --format json | jq '.findings[]'
```

Search reads three tables that have to agree about a chunk — its text, its embedding, and its full-text row. When they stop agreeing nothing errors: a chunk with no embedding is simply never a vector hit, and one with no full-text row is never a keyword hit. Until now the only way to find out was to run a full index and watch it repair things. `doctor` asks directly, and also reports which indexed documents the MCP resource surface is holding back and why — an extension no longer in `[parsers].enabled`, a document larger than a resource read returns, or a size not recorded yet because it was indexed by an earlier version.

It also reports **files that stopped being read**. This project was named `kb-mcp` through v0.25.0, and the rename shipped no aliases and no automatic migration ([ADR-0007](decisions/0007-rename-the-project-to-grooveseek.md)) — a `groove` binary does not open anything `kb-mcp` left behind. A `.kb-mcpignore` still sitting in a knowledge base is the one that matters: it is not read, so **everything it excluded is in the index**, with nothing anywhere saying so. `doctor` names it, along with a stale `.kb-mcp.db` and the two old eval files. `groove.toml` is not among them, because its location is discovered rather than fixed and a configuration that failed to load announces itself immediately.

**When the file that replaced one of these is there too, the finding says something different and advises something different.** `doctor` reads neither file, so with a live `.grooveignore` beside the old one it cannot claim anything is leaking — and "rename it to `.grooveignore`" would be telling you to write over the configuration you are using.

Exit codes: `0` (nothing to report), `1` (findings), `2` (could not run — usually no index). Findings are reported, never repaired: each one names the command that fixes it, which is `groove index` or `groove index --force` for everything structural.

> Like `search` and `eval`, this opens the database, and opening it applies any pending schema migration. It is read-only about its findings, not about the file.

## Evaluate retrieval quality against a golden query set

**Optional power-user feature.** `groove eval` takes a small file of questions with known answers, runs them through the same hybrid search the `search` tool uses, and reports **recall@k / MRR / nDCG@k** with diffs against the previous run. Useful when comparing models or tuning `[quality_filter]` / RRF parameters.

Regular users running `groove index` + `groove serve` do not need this — without a golden file, `eval` just errors with a hint and exits.

```bash
# 1) Write a golden YAML at <kb_path>/.groove-eval.yml
cat > knowledge-base/.groove-eval.yml <<'EOF'
queries:
  - query: "What does the k parameter in RRF do?"
    expected:
      - { path: "docs/ARCHITECTURE.md", heading: "Data flow" }
      - { path: "src/db.rs" }   # heading omitted = file-level hit
EOF

# 2) Run against the indexed DB
groove eval --kb-path knowledge-base

# 3) Re-run after tweaking config / model to see the diff
groove eval --kb-path knowledge-base --reranker bge-v2-m3
```

Output: aggregate metrics + per-query rows for regressions / misses only. JSON (`--format json`) exposes the full per-query detail. History lives at `<kb_path>/.groove-eval-history.json` and keeps the last 10 runs for diff display.

If you keep notes about the evaluation inside the knowledge base being evaluated, those notes compete with the real answers. From v0.24.0 every run scans the corpus and warns on stderr when a document quotes **two or more** golden queries verbatim (`findings` in `--format json`) — the shape of a note written *about* the golden set. It reports only; the exit code is unchanged. Details, including why one match is not enough to report: [docs/eval.md](eval.md).

For CI: pass `--fail-on-regression` (v0.6.0+) to exit with code 1 when any aggregate metric (`recall@k` / `MRR` / `ndcg@k`) regressed from the previous **fingerprint-compatible** run by more than `regression_threshold` (default 0.05). Updating the golden YAML changes the hash, so the next run skips the comparison rather than triggering a false positive. Details: [docs/eval.md](eval.md).

See [docs/eval.md](eval.md) for the golden YAML reference, metric definitions, diff output guide, and troubleshooting.

## Measuring the fusion parameters (`groove tune`, v0.13.0+)

`[search.fusion]` exposes the RRF constant and the bm25 column weights, but the
defaults are the industry convention and RRF is documented as requiring no
tuning. If you want evidence rather than a guess for *your* KB, run:

```bash
groove tune --kb-path knowledge-base
```

It sweeps a fixed grid against your golden query set, guards the result with
leave-one-query-out cross-validation, and prints either a paste-ready snippet
or the conclusion that the defaults should stay. It applies nothing on its own.
Note that the parameters can only move queries that **reach the bm25 stage at
all**, so `tune` opens with a pre-flight that reports the effective N and exits
2 when it is 0. See [docs/eval.md](eval.md).

## Turning up the logging

There is no verbosity flag. Verbosity comes from the **`RUST_LOG`** environment
variable, which every subcommand reads; unset, it behaves as `info`.

```bash
RUST_LOG=grooveseek=debug groove serve --kb-path ./knowledge-base
RUST_LOG=grooveseek=debug groove search "query" --kb-path ./knowledge-base
RUST_LOG=debug groove index --kb-path ./knowledge-base   # dependencies too, very noisy
```

`grooveseek=debug` is the useful setting: it raises this project's own targets
and leaves the HTTP stack and the ONNX runtime at `info`. What it adds:

- **`get_best_practice` returning "not found"** logs the paths it actually
  probed. The response deliberately reports only how many templates were tried,
  because the paths come from `[best_practice].path_templates` and would hand an
  unauthenticated caller the server's directory layout — so the operator reads
  them here or not at all.
- **A search matching less than expected** logs how the query was compiled for
  the full-text half: which fragments were dropped below the trigram floor, and
  which quoted phrases were discarded. See
  [docs/retrieval-pipeline.md](retrieval-pipeline.md).

Two things you might go looking for are not behind `RUST_LOG` at all. **Which
`groove.toml` won** is logged at `info`, so it is already visible without
raising anything: `loaded config source=… path=… trust=…` (see
[docs/configuration.md](configuration.md)). And `index`'s progress — the
`Indexing …`, `  indexed: …` and `Done in …` lines — is written directly rather
than through the logger, so it appears at any level and is controlled by
`--quiet` / `--progress` instead.

Logs go to stderr on every subcommand, so raising the level never disturbs
output being piped from stdout — which matters most for `serve`, where over the
default stdio transport stdout **is** the MCP protocol and stderr is the only
place logs can go. Log wording is explicitly *not* part of the stable surface
([docs/stability.md](stability.md)): read it, do not parse it.

For a daemon registered with `groove service install`, the level is set where
the service is defined rather than in your shell. The systemd unit carries
`Environment=RUST_LOG=info` and the launchd plist an equivalent entry; edit and
restart to change them. The Windows scheduled task sets no `RUST_LOG` at all, so
it falls back to the same `info` default — raising it there means setting the
variable in the environment the task runs under.

## Related

- `docs/configuration.md` — giving these flags defaults in `groove.toml`
- `docs/clients.md` — pointing an MCP client at a running server
- `README.md` — install and quick start
