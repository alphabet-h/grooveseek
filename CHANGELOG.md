# Changelog

All notable changes to GrooveSeek are documented here. The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

**This project was named `kb-mcp` through v0.25.0.** Entries for those versions are left as they were written, and their release assets are still published under that name, so `kb-mcp`, `kb-mcp.toml` and `.kb-mcp.db` below all refer to this project before the rename. See [ADR-0007](docs/decisions/0007-rename-the-project-to-grooveseek.md).

Each heading's date is the date its `vX.Y.Z` tag was created, **in the timezone of whoever created it** — that offset is stored in the tag object, so no conversion is involved. Verify with:

```
git for-each-ref --format='%(taggerdate:short)' refs/tags/vX.Y.Z
```

Do not reach for `format-local` here: it renders in the *reader's* timezone, so it answers a different question and gives a different day for tags made near midnight. Writing the date before tagging is the other way the two drift apart.

## [Unreleased]

### Added

- **A page about which shape to deploy in.** [docs/deployment-topologies.md](docs/deployment-topologies.md)
  answers three questions the reference pages each answer a piece of: whether to
  let a client spawn `groove` or to leave one running, what residency actually
  buys, and where the same-host boundary comes from.

  It began as an internal note measured against v0.26.0 and was re-measured
  against v1.0.0 before publishing, because the two releases in between changed
  exactly what it was about. `/api/search` — which its benchmark named and its
  fourth open question argued about — no longer exists. Origin validation, which
  it listed as unresolved, ships on by default. All four of its open questions
  were settled before 1.0.0, so they appear as outcomes with the ADR that
  records each one, rather than as questions.

  The measurements are new: a resident daemon answers a search in about 200 ms
  where the CLI takes about 3.1 seconds, and a control — `groove status`, which
  does everything except load a model, in about 35 ms — puts roughly 99% of that
  gap in the model load rather than in I/O. It also says the thing the internal
  note did not: residency is not the same as speed. A first query after hours
  idle took 4.6 seconds, and a CLI search running alongside the daemon dragged it
  from 200 ms to 2 seconds, which is the page's own "one process, one model"
  warning arriving as latency.

  Nothing in it cites a line number. The version it replaced cited eleven, and
  all eleven were wrong within five days.

## [1.0.0] - 2026-08-22

### Added

- **`groove doctor` says what an old `.kb-mcpignore` left in the index.**
  [ADR-0007](docs/decisions/0007-rename-the-project-to-grooveseek.md) renamed the
  project with no aliases and no automatic migration, so an ignore file under the
  old name is not read and stops excluding anything at all. What comes back into
  the index is whatever the two gates that still apply admit — the current
  exclusion rules, and whether `[parsers].enabled` opens that extension. Nothing
  said so.

  Two findings, both warnings. `indexed-despite-legacy-ignore` names the indexed
  documents the old file matches that the current rules do not exclude — real
  paths, up to five of them, not a count. `legacy-ignore-not-examined` is what
  comes out when the check could not be completed — the file is there and cannot
  be read, or the filesystem will not say whether it is there at all: **a check
  that could not run is not a check that found nothing**, and reporting the two
  the same way is how a clean bill of health stops meaning anything. Its wording
  claims only that, with what was actually observed carried alongside, because
  one of those two cases never established that the file exists.

  The remedy has three branches, because the destination has three states. With
  the name `.grooveignore` **free**, the fix is a rename. With a working ignore
  file there, it is copying over the lines you still want — never an overwrite.
  And when the name is not free but nothing is being applied from it — a
  directory, a refused link, a file over the cap, or a name the filesystem would
  not answer for — the remedy says that and sends you to the destination first,
  because neither renaming nor copying into it produces an ignore file and the
  documents just reported would stay indexed either way. No branch asks for a
  deletion: a knowledge base whose new file is broken looks identical from here,
  and the old file may be the only copy of the patterns.

  "Free" throughout means the filesystem said so, not that it failed to say
  otherwise. Whether a name is free, taken, or unanswerable is one three-valued
  question with one implementation, asked by both the check and the remedy.

  The old file goes through the same `ExclusionRules` the index walk asks, so
  this is not a second implementation of the exclusion rule; and it is asked
  about documents that are **in the database**, which is what lets the finding
  carry paths instead of the observation that a filename exists. **The check is
  for the migration and is due to be removed in 1.1.0**, in one file.

- **All four front pages open with a banner** — `README.md`, `README.ja.md`,
  and the two `docs/index` pages. It draws what separates this from a plain
  vector store: a semantic path and a lexical path converging on one node, and
  ranked results leaving it for an MCP client — the RRF fusion of the
  sqlite-vec and FTS5 legs.

  **It carries no words.** A wordmark would repeat the `# GrooveSeek` heading
  directly beneath it, a caption strip at this width is illegible on a phone,
  and text drawn into an image is text no screen reader and no translation can
  reach. The `alt` attribute carries the meaning instead, and it is written per
  language.

  Light and dark, chosen by `prefers-color-scheme`, like the logo and the
  screenshot before it. WebP rather than PNG — 33 KB against roughly 1 MB, for
  an image every visitor loads. The PNGs are committed as a fallback, and
  `assets/README.md` says what to swap.

  **That file also stops repeating a claim it could never check.** It recorded
  a report that `raw.githubusercontent.com` serves `.svg` as `text/plain`,
  which is why nothing here references an SVG; the report could not be measured
  when it was written because the host answered 429 all session. Measured now
  against this repository's own files, the host answers `image/svg+xml` — and
  `image/webp` for the banner. Whether GitHub's Markdown renderer would then
  display an SVG is a second question, about its `camo` proxy, and is still
  untested; the PNGs stay for that reason rather than the old one.

  The 56-pixel logo those four pages used to open with is gone, since the
  banner carries the mark at its centre. **No page embeds `assets/logo-*` any
  more** — `/ui` never did, drawing the diamond as a character and its favicon
  as an inline `data:` URI. The files stay: the SVGs are where the mark is
  defined, which is what the banner's colours were measured against.

### Changed

- **`groove status`, `groove service status` and `groove service list` print
  their results on stdout.** All three used to write everything to stderr, so
  `groove status | grep Documents` received nothing — the pipe looked like it
  would work and silently did not.
  [ADR-0008](docs/decisions/0008-declare-what-1-0-freezes.md) declared the
  stdout/stderr split frozen but left these three explicitly unsettled;
  [ADR-0010](docs/decisions/0010-settle-what-the-1-0-command-line-freezes.md)
  settles them, because after 1.0.0 moving them would be a major release.

  A caller redirecting with `2>&1` is unaffected. A caller capturing stderr
  alone now reads nothing where it used to read the counts.

  What stays on stderr is everything that is not an answer: `index`'s progress,
  the confirmations from `service install` / `uninstall` / `tray-install` /
  `tray-uninstall`, and `status`'s "No index found" — which reports an
  inability to answer and leaves stdout empty. The **wording** of these lines is
  still not stable; only the channel is. `groove doctor --format json` remains
  the machine-readable route to `documents` and `chunks`.

- **One implementation now answers `Host` and `Origin` wherever they are
  asked, `/mcp` included.** The two questions had four implementations —
  rmcp's for `/mcp`, and GrooveSeek's own for `/healthz` and for the admin
  routes — fed one list and expected to agree. Measured, they did not: with
  the identical allow-list,
  `Host: user:pw@127.0.0.1:PORT`, `@127.0.0.1:PORT`, `127.0.0.1@localhost`,
  `127.0.0.1:65536` and `localhost:abc` were accepted on `/mcp` and refused
  next door, and the admin refusal bodies were missing the `Forbidden: `
  prefix the others carried.

  **`/mcp` therefore refuses those five spellings now**, where it used to
  answer 200. All are malformed Hosts that no browser or MCP client
  constructs, and the change can only refuse more, never less: no spelling was
  found that rmcp refused and GrooveSeek accepted. Its refusal wording is
  unchanged. See [ADR-0009](docs/decisions/0009-one-dns-rebinding-gate.md).

  **What each route is compared against has not changed**: `allowed_origins`
  still reaches `/mcp` and the admin routes only, the admin routes still match
  `Host` against a loopback-only list of their own, and `/healthz` still
  validates `Host` alone and only when `healthz_public = false`.

  Two consequences worth having on their own: a refused request no longer
  reserves a session seat before being turned away (measured with
  `max_sessions = 1`, a foreign `Host` used to get `429` and now gets `403`),
  and refusal logging on `/mcp` is bounded — rmcp wrote one line per refusal
  with no limit, and the gate carries the same one-line-a-minute budget the
  session limit has used since v0.27.0.

- **An admin refusal no longer repeats the header it refused.** The 403 body
  read `Host 'kb.example.lan' not in admin allow-list`, echoing caller-supplied
  bytes back; `/healthz` next door and rmcp on `/mcp` both say only that the
  header was not allowed. This surface now says the same, and the rejected
  value goes to the log instead, where the operator who can act on it will see
  it.

- **`rebuild_index` refuses a second call while one is running.** A rebuild
  re-embeds the whole corpus while holding the embedder and the database, so a
  second call never ran beside the first — it queued behind it, with `search`,
  `get_document` and `/ui` unavailable for the sum of the two. Nothing bounded
  how many could queue: the session gate lets every non-`initialize` request
  past without taking a seat, so `max_sessions` did not apply, and
  `spawn_blocking` cannot be aborted, so closing the connection did not stop one
  either. A few dozen bytes of request bought a full re-vectorisation, as many
  times over as the caller liked.

  The second caller now gets an error naming how long the running rebuild has
  been going, instead of a wait with no upper bound. **The bound is on the MCP
  tool**: `groove index` runs in its own process and still overlaps a served
  rebuild.

### Security

- **`/ui` and `/api/admin/status` validate `Origin`.** The admin routes are
  served by GrooveSeek rather than by rmcp, so the check that guards `/mcp`
  never reached them: any page open in the operator's browser could call them
  cross-origin. Nothing leaked — they are `GET`s, and a foreign page cannot read
  a response that carries no CORS headers — but nothing kept that true either,
  and the first admin route with a side effect would have inherited the gap.
  They now compare against the same effective `[transport.http].allowed_origins`
  rmcp gets, with a test that asks both surfaces about one origin and requires
  the same answer. As on `/mcp`, a request carrying no `Origin` still passes,
  so the tray, `curl` and the page's own status poll are unaffected.

- **Admin refusals no longer write a log line each.** Bound to a non-loopback
  address, the peer check refuses before anything else looks at the request, so
  a stream of cheap requests wrote an unbounded stream of lines to the daemon's
  log file. The session gate on `/mcp` already thinned its refusals to one line
  a minute, carrying the count of what it stood for; the admin gates now share
  one such budget between them.

- **`/ui` is served with a Content Security Policy and `nosniff`.** The policy
  is `default-src 'none'` plus exactly what the page uses — its inline
  `<script>` and `<style>`, its `data:` favicon, same-origin `fetch`,
  `frame-ancestors 'none'` — so an external script or stylesheet added to the
  page fails there rather than loading. Both headers are attached outside the
  two gates, so refusals carry them too.

- **`h2` was updated 0.4.13 → 0.4.18 to clear RUSTSEC-2026-0258.** It is a
  transitive dependency, reached through `hyper` — and so through `axum`, which
  the HTTP transport is built on — and through `reqwest`, which the model
  downloader uses. No GrooveSeek code calls it directly.

### Removed

- **`groove validate --strict` is gone.** It was accepted and discarded — the
  documentation said so, which made it a promise the binary did not keep: a CI
  job that passed `--strict` believed it had asked for stricter checking and
  had not. Giving it meaning after 1.0.0 would change what an accepted flag
  does, which is a major release; removing it now costs nothing and adding it
  back when `[options].allow_unknown_fields` exists is a minor one. Scripts
  passing it will now fail to parse, which is the visible version of what was
  already happening silently. See
  [ADR-0010](docs/decisions/0010-settle-what-the-1-0-command-line-freezes.md).

- **`kb.path` is gone from `/api/admin/status`, and from `/ui`'s status band.**
  It held the knowledge base's absolute path, which on Windows reads
  `C:\Users\<name>\...` — the operator's account name, in a JSON body and in
  every screenshot of the page most likely to end up in a bug report. Nothing
  consumed it: the Windows tray reads `daemon.pid` and `indexing.active`, and
  the page's own comment claiming the tray needed the field was wrong. What
  identifies a knowledge base to the person looking at it — `kb.documents`,
  `kb.chunks`, `kb.model` — is unchanged. [ADR-0008](docs/decisions/0008-declare-what-1-0-freezes.md)
  puts this surface outside the 1.0 freeze, which is why the removal happens
  now rather than during 1.x.

### Fixed

- **`groove service install` on macOS reports an error instead of panicking.**
  Two `unwrap()` calls in the LaunchAgent backend. Neither is reachable on a
  normal system — `plist_path` always returns a path under
  `~/Library/LaunchAgents`, and a home directory is normally UTF-8 — but
  `install()` returns a `Result` and its caller already handles one, so a panic
  was the wrong way to say either had gone wrong.

- **Every word GrooveSeek writes to stderr is ASCII, and a test now says so.**
  A Japanese Windows console is CP932, where an em dash or a kana arrives as
  mojibake, so [AGENTS.md](AGENTS.md) has required diagnostics to be ASCII since
  v0.25.0. Nothing checked it, and review kept finding the same defect one
  instance at a time.

  **43 messages** contributed characters that console cannot render: the whole
  of `groove service install` / `uninstall` / `status` on all three platforms,
  which spoke Japanese and now speaks English; em dashes in `groove tune`'s two
  notes and two warnings, in three `groove index` PDF refusals, in the two
  `[parsers].enabled` refusals, in a watcher diagnostic, in the poisoned-mutex
  warning and in both PowerShell decoding errors; and the `groove eval` note
  this began with. Only the wording changed — an error that named a file still
  names the same file.

  The rule is about **the words a message chooses, not the data it names**, so a
  note called `日本語のノート.md` still comes out of `groove index` as itself.

  **`groove index --progress` draws its bar with `=>-`** instead of eighth-block
  characters. The bar is drawn to stderr by indicatif, which made it the one
  place the rule was broken by a library's rendering rather than by a message —
  and the one that would have looked worst on the console the rule is about.

  `tests/diagnostics_stay_ascii.rs` walks the workspace source at run time
  instead of naming files, so a new file is covered by existing rather than by
  being remembered.

- **The golden query file and the eval history are read with a bound.** Both
  default to living inside the knowledge base and both were read whole with no
  cap — a stray binary on one of those names was parsed as-is. They now go
  through the same route `.grooveignore` takes, so a hard link, a FIFO, or
  something that is not a regular file is a refusal rather than an unbounded
  read. Symlinks are refused **on Unix only**, which is that route's existing
  and deliberate scope: making one on Windows needs a privilege this threat
  model's attacker does not have, and refusing reparse points there would
  refuse every OneDrive and Dropbox placeholder.

  The **size** caps differ, because the two files differ. The golden is written
  by a person, and 1 MiB is far past what it is for. The history is written by
  `groove eval` and carries every golden query with its hits, once per retained
  run: measured, this repository's own 25-query golden produces **0.598 MiB**
  after ten runs, and the same golden at `limit = 20` produces **1.049 MiB**. A
  megabyte is inside its ordinary range, so the history's cap is 64 MiB.

  **A history that cannot be read now stops `groove eval` instead of reading as
  empty.** An empty history is not inert — the new run is pushed onto it and
  saved back over the same path — so answering "empty" for a file that is
  intact and merely unread would replace every baseline with one run, and
  `--fail-on-regression` would then pass without having compared anything.
  Content that was read and does not parse still starts fresh, unchanged: those
  bytes held no baseline to lose. `--no-history` skips the file entirely.

  A **dangling symlink** counts as a file that is there, for the same reason.
  `Path::exists` follows the link and answers about the target, so a history
  kept on a volume that is not mounted read as absent — and absent is the
  answer that leads to the new file being renamed over the link.

  **And what `eval` writes is now bounded by what it will read.** `history_size`
  bounds the number of runs kept and bounded nothing about the size, so a large
  golden or a high `--limit` could write a history the next run refused —
  `groove eval` producing a file only it could no longer read. Saving now drops
  the **oldest** runs until the result fits, warning when it does; the run the
  next diff compares against is the last to go. If a single run does not fit,
  the save reports that instead of writing it, and says to reduce the golden or
  the limit, or to pass `--no-history`.

  `groove tune` reached the golden through a different function than `groove
  eval` did, so the two are now one: `eval` no longer keeps its own copy of the
  read-and-parse, and a bound added to either is a bound on both.

- **A refusal printed to stderr carried an em dash.** `AGENTS.md` keeps stderr
  ASCII so a CP932 console does not render it as mojibake, and
  `Refused::log_line`'s "not a regular file" message had two — printed from
  `groove index`, from the `.grooveignore` reader, and from `get_document`,
  since each was written. A fourth caller in `eval` is what got it noticed.

  The path in that line is still interpolated as-is: a note named in Japanese
  makes the message non-ASCII whatever the wording is, and escaping it would
  hand the reader `\u{65e5}\u{672c}` where they expected a filename. That
  trade-off is left where it is, and the test that pins the wording says so.

- **The documentation told you to type a prompt command no shipped recipe
  produces.** `docs/mcp-tools.md`, its Japanese counterpart and `prompts.rs`
  all rendered the prompt path as `/mcp__groove__<name>`, but a client builds
  that path from the key **you** wrote in your `.mcp.json`, and all four
  bundled recipes call the server `ai-knowledge`. Anyone who copied a recipe
  was given the wrong command; anyone who chose their own name was given a
  different wrong one. The path is now written `/mcp__<server>__<name>` with
  the note that `<server>` is yours, and a test rejects any concrete name
  spelled into that position.

- **`docs/index.md` had no Japanese counterpart**, though the language policy
  and `Corpus`'s own documentation both said every page under `docs/` has one.
  The landing page's navigation text — the row describing each page — was
  English-only. [docs/index.ja.md](docs/index.ja.md) now exists and the two
  link to each other, and a test walks `docs/` in both directions so a page
  published in one language cannot go missing from the other again.

- **An `allowed_origins` entry without a scheme refused every browser, in
  silence.** `[transport.http].allowed_hosts` takes a bare `host:port` — its
  parser falls back to reading the whole string as a host — and the key beside
  it looks identical but requires a scheme. `allowed_origins =
  ["127.0.0.1:3100"]` was therefore dropped by rmcp before any comparison,
  which left Origin validation switched **on** with nothing to match: every
  request carrying an `Origin` header got 403, including `/ui`'s own search.
  Nothing warned. The "this list names no loopback origin" check strips the
  scheme optionally, so it read the host as `127.0.0.1` and concluded the list
  was fine.

  Such a config is now refused at startup, with a message quoting the entry and
  naming the key that does accept that spelling. **A config that used to start
  will now stop** — with an error you can act on, rather than a server that
  answers 403 to everything. An empty list is still accepted: that one is the
  documented off switch.

  The check runs where the list is consumed — resolving an HTTP transport —
  and nowhere earlier, so it cannot refuse a value that was never going to be
  read. Two earlier placements could. Checking during config loading meant a
  `groove.toml` in a cloned repository could stop every command, because a
  discovered config's `allowed_origins` is discarded as untrusted before it is
  ever used. Checking after that, but still during loading, meant a typo in an
  HTTP-only setting stopped `index`, `search`, `validate` and a stdio server —
  none of which read the key.

  The same list carries a second cost, which cannot be removed. rmcp matches an
  entry with no port against *every* port on that host — wider than RFC 6454,
  where an omitted port means the scheme's default. Writing the port in does
  not fix it: the browser omits the port too, so `http://127.0.0.1:80` would be
  compared against a request that carries none and would refuse the very page
  it exists for. At port 80 the derived default therefore has to include the
  port-less spelling, and a page served from any other local port can reach
  `/mcp`; the server now says so at startup. An entry you write yourself is
  left alone, because `https://kb.example.com` is the shipped proxy recipe and
  means 443.

- **`--path-glob` split its value on commas, which no glob survives.** A glob's
  own syntax uses commas — `docs/{a,b}/**` is one pattern — and the flag cut it
  in half, leaving `docs/{a` to be rejected as an unclosed alternate group. The
  MCP `path_globs` parameter takes an array and never had the problem, so the
  same value worked over one surface and failed over the other: exactly what
  aligning the two was meant to prevent.

  `docs/usage.md` had always described this flag as **(repeatable)** and never
  as comma-separated, so the code was the side that disagreed. `--tag-any`,
  `--tag-all` and `--exclude-paths` keep their commas, which is their documented
  contract — none of those values can contain one meaningfully.

  If you were passing several patterns in one `--path-glob` separated by commas
  — undocumented, and it would have broken on any pattern containing braces —
  pass the flag once per pattern instead.

- **`groove-schema.toml.example` offered two field types the schema refuses.**
  The comment listed `"integer"` and `"date"` among its `type` values;
  `schema.rs` rejects both at compile time, because frontmatter is held as
  strings throughout and neither is implemented. Anyone following the template's
  own documentation met a schema that would not load. The body was always
  right — it already expresses a date as a string with a pattern — so it was the
  comment above it that was wrong. The file is also now in English, matching
  `groove.toml.example`; it was the last shipped example still in Japanese.

### Internal

- **`server.rs` became four files.** The search half is `server/search.rs`,
  document reading is `server/documents.rs`, and the corpus side of the `kb://`
  resource surface is `server/kb_uri.rs`. What stayed behind is the tool surface
  itself — the `#[tool_router]` / `#[tool_handler]` impls, the parameter and
  response types, and `mod tests`.

  No behaviour change: the bodies moved byte-identical and in the order they
  were already in. The only thing that changed was visibility, and only where
  the parent still calls or names something — each `pub(super)` was named by
  `cargo check` after a move that widened nothing, rather than chosen in
  advance.

- **The eval golden has five questions with two right answers, and the quality
  gate stopped averaging them together with the other twenty-five.** No
  behaviour change; the last of the 2026-08-18 audit's test-coverage rows.

  Every golden query named exactly one document, so nothing measured whether a
  search returns several relevant documents when several are relevant. The five
  new ones are built on pairs the fixture corpus already had — the two release
  documents, the two database ones, the two authentication ones, the pair that
  both say when to act before you know why, and the pair an incident write-up is
  assembled from. No documents were added: the golden is never copied into the
  corpus, so a new query cannot change the candidate set the other queries are
  scored against, and the twenty-five re-measure identically.

  **The two groups are averaged separately**, because a query with two right
  answers caps recall@1 at 0.5 and blending them moves the headline number by an
  amount that depends on how many such queries the golden holds rather than on
  whether retrieval got worse — measured, adding five drags the blend from 0.92
  to 0.85 on BGE-small with nothing changed. The four existing floors keep their
  values and are now compared against the group they were measured over.

  On the new group recall@1 is 0.50 on both models — its ceiling — and MRR is
  1.000 on both, because every one of the five puts one of its two documents at
  rank 1. That is exactly the blindness the queries were added for. recall@5 is
  the one that separates the models, 0.80 against 1.00, where on the
  single-answer group the same metric separates them 0.96 against 1.00 and was
  rejected as a gate for that reason.

- **A hybrid search is now held to a fixed number of SQL statements, and the
  two bounds a graph walk is built on are property-tested.** No behaviour
  change; two more of the gaps the 2026-08-18 audit named.

  The performance guard this project had compared wall-clock as a ratio. That
  is right for what it guards and wrong as the only one: timing on a shared
  runner is noise, so it is `#[ignore]`d, runs once a night, and its threshold
  has to be loose enough to survive that runner. Counting statements instead
  costs milliseconds, gives the same answer on every machine, and runs on every
  pull request — and it catches the regression a stopwatch notices last, a
  query issued per candidate, per result or per document.

  `search_hybrid` issues **two** statements: one for the vector leg, one for
  the full-text leg, with the fusion done in Rust over what they returned. Two
  at 50 chunks and at 500, asking for one result and for ten. Counting every
  statement SQLite traces instead gives 175 and 769 — FTS5 reading
  `fts_chunks_docsize` once per row it scores for bm25, which this project
  neither wrote nor wants to change, and a gate over that number would have
  been red the day it landed.

  The graph walk's node budget and seed cap take whatever an MCP client sends,
  including `0` and `u32::MAX`. Both are now generated rather than sampled, and
  the asymmetry at zero is stated as the rule rather than as two examples:
  `max_nodes = 0` is a coherent request and is honoured, while
  `max_seed_chunks = 0` would make an answer indistinguishable from "no such
  document" and becomes 1.

  `.grooveignore`'s `!` is generated too, across nine spellings, every
  hardcoded name, and three depths. Breaking the rule three ways showed what
  that adds: two of the three breakages are caught by the example tests as
  well, and the third — giving `!` gitignore's own precedence, so it wins
  where an earlier line ignored something — passes every example and fails
  both properties, because each example spells its negation `!name` in a file
  with no ignore line at all. The shrunk counterexample is `*` followed by
  `!.git`.

- **The two Origin startup warnings are now checked for what they say and when
  they fire.** They are the only thing an operator gets in two configurations
  that otherwise look like they are working — one where Origin validation is
  off, and one where `/ui` is served but its search is refused with nothing on
  screen to say why. Both are inline conditions rather than predicates, so
  nothing could reach them: neither the wording nor the trigger was tested.

  A third test asserts they stay **quiet** on an ordinary configuration. A
  warning that fires always would satisfy the other two and teach an operator
  to ignore the line.

  The shared stderr drain in `tests/common` keeps its lines now rather than
  discarding them. It has to be that one: a second reader on the same pipe
  would take lines away from the first, and the first is where the bound
  address comes from.

- **Nightly gained a macOS leg, and the launchd backend gained end-to-end
  tests.** `install`, `status` and `uninstall` are reached only from `#[ignore]`
  territory, and nightly ran Linux and Windows — so on macOS none of them had
  ever been executed by anything. That is the same gap AU-09 closed for
  Windows, left open for the third platform.

  Measured on a GitHub-hosted runner before writing any of it: `launchctl
  managername` answers `Aqua`, so the `gui/<uid>` domain the backend bootstraps
  into exists there — which is not a given, since `launchctl(1)` says a GUI
  domain is created at GUI login and other CI fleets report
  `Bootstrap failed: 125` for exactly this. `bootstrap` exits 0, `RunAtLoad`
  really starts the program, and `bootout` cleans up.

  The skip list for the big-model tests moved out of the matrix and into the
  step that uses it, now that two legs share it: a long string written once per
  leg is a string that gets updated once.

- **Four gaps the 2026-08-18 audit named now have tests.** No behaviour change.

  `groove service uninstall` and `status` take `--service-name`; the instance
  name used to be positional on both, and nothing checked that the old spelling
  was **refused** rather than quietly ignored — which would have left
  `groove service uninstall work` removing the instance called `groove`.

  `groove search` reads `rerank_by_default` from `groove.toml`. The decision
  was a pure function with its own tests, none of which could see whether the
  command line handed it the key at all. The new test tells the two apart by
  the shape of `score`: an RRF sum is bounded by `2 / (rrf_k + 1)`, and a
  cross-encoder logit is not on that scale.

  Three boundary inputs: an empty knowledge base, a query far past the 1 KiB
  the MCP surface refuses, and a query made only of characters outside the BMP.
  The long-query test also records where the real ceiling is on that surface —
  Windows caps a whole command line at 32,767 characters, so a 64 KiB query
  fails before the process starts.

- **`docs/stability.md` writes out what a search answers with.** It had
  promised since v0.27.0 that "every field documented today keeps its name,
  type, and meaning" while documenting none of them — a promise with no subject,
  in the document that says what 1.0 freezes. All 28 fields of the `search`
  response are now listed with their type and presence rule, for the MCP tool
  and for `groove search --format json`, taken from the types rather than the
  prose. Three tests hold the table to the response in both directions and hold
  the Japanese table to the English one.

  The tests found the hole on their first run: the table stopped at
  `match_spans` and `expanded_from` without describing what is inside them, so
  five fields sat outside the freeze while looking covered.

  **`low_confidence` is frozen as a field, not as a judgement.** The key is
  present and boolean; the formula, the default threshold, and which queries
  trip it are explicitly outside the freeze. Measured, it tracks how much the
  fused scores are distributed rather than whether the answer is right — on a
  corpus where all 25 golden queries were answered correctly at rank 1 it still
  fired on 14 of them, and reranking can switch it off outright — cross-encoder
  logits often make the mean negative, and the sign check then answers `false`
  whatever the spread was (measured: `false` for all 25 with `bge-v2-m3`). A
  `false` therefore tells a caller nothing when a reranker ran.
  `docs/filters.md` records both limits. No behaviour changed: no corpus-independent threshold exists, so
  moving the default would have swapped one arbitrary number for another.

- **Two `chunks_exact(2)` calls in the PDF parser became `as_chunks::<2>()`.**
  Rust 1.98.0 stabilised `clippy::chunks_exact_to_as_chunks`, and CI installs
  `stable` unpinned, so the lint arrived on its own and turned `-D warnings`
  into a failure on code nobody had touched. `as_chunks::<2>().0` splits
  identically — the trailing odd element goes to `.1` the way `chunks_exact`
  left it in `.remainder()` — and states the pair width in the type, which is
  what both call sites meant.

- **`docs/usage.md` now documents `RUST_LOG`.** Raising the log level is the
  first step in diagnosing a wrong `groove.toml`, a `get_best_practice` that
  reports "not found", or a query that matches less than expected — and the
  variable appeared nowhere a user would look. The new section says which
  target to raise, what each level adds, and what is *not* behind it: the
  chosen config file is logged at `info` already, and `index`'s progress does
  not go through the logger at all.

- **The flag-coverage check no longer reads `docs/decisions/`.** An ADR
  explaining why a flag was removed has to name it, which failed the reverse
  direction of the check — and ADRs are immutable once merged, so the failure
  could not have been repaired, only worked around. `CHANGELOG.md` was already
  excluded for the same reason. This tightens the forward direction rather than
  loosening it: a flag named only in a decision record no longer counts as
  documented, and `every_long_flag_the_binary_accepts_is_documented` still
  passes, so none was relying on one.

- **Three holes in the tests that guard the frozen surface.** The flag-coverage
  check pooled the English and Japanese documentation into one buffer, so a flag
  described in only one language satisfied it — while `docs/stability.md`, the
  page that gives "documented" its meaning, is the English one. Each language is
  now checked separately; both pass, which is the cheapest moment to make sure
  they keep doing so.

  The pairing tables covered two of the six MCP tools. `rebuild_index` now pairs
  with `groove index`, and the tools that have no command behind them —
  `get_document`, `get_best_practice` — declare their parameters and the reason
  they have no second surface, so a parameter cannot appear on any of the six
  unnoticed. `list_topics` is recorded as the one that takes none at all.

  And the pairing rule now checks values, not only names: a flag documented as
  repeatable must not carry a delimiter, and one documented as a comma list
  must.

- **Origin validation is now tested through a running server.** The check
  shipped in 0.27.0 with twenty-five tests, every one of them against the
  function that assembles the allow-list rather than against the server that
  applies it — deleting the call that hands that list to rmcp left the whole
  suite passing. Four tests now bind a server, send real requests, and assert
  what comes back: a foreign origin is refused, a request with no `Origin` is
  not, the server accepts its own bound address, and an empty list really does
  turn the check off.

  They bind with `--bind 127.0.0.1:0` and read the port back, because the
  allow-list is derived from the address the listener *received*; a test that
  supplies the port cannot tell that apart from one that echoes it. And they run
  in ordinary `cargo test`, without `--ignore` and without a feature flag, so a
  regression fails the pull request that causes it rather than the next nightly.

- **`/ui` and the request it sends are now exercised through a running
  server.** The page started searching through `/mcp` in v0.27.0 and nothing
  asserted since that the request it sends is one the server accepts — the
  existing web UI tests are feature-gated and ignored, which is right for the
  ones that build an index and wrong for a check PR CI therefore never runs.
  Five tests, none ignored or gated: the page is served, the handshake-free
  `tools/call` it sends is accepted, dropping the protocol header is refused
  (which is what gives the previous one meaning), `/api/search` is absent from a
  shipped server, and `/ui` refuses a foreign `Host`.

  The request is **read out of the page**, not transcribed: the `fetch` target,
  the method, the headers, and the stringified body with its envelope and
  nesting all come from `callTool`. Anything the reader cannot model stops the
  test rather than being skipped — a computed target, an unrecognised value, an
  option the replay does not implement, a body that is no longer
  JSON-stringified — because a shape it cannot read must never be reported as a
  shape that matches. A transcribed request passes happily while the page it
  claims to describe has changed.

  A separate assertion pins the page to rmcp's `STANDARD_HEADERS`. Both are
  needed: measured, a page pinned to `LATEST` still gets a result, because rmcp
  accepts a handshake-free call on known older versions — so the live test
  alone would not have caught the mistake most likely to be made.

- **The tests no longer choose the port they tell the server to bind.** They
  bound `127.0.0.1:0`, read the number, dropped the listener, and passed that
  number on the command line — leaving a window in which anything else starting
  a server could take it, with a dozen of them running in parallel inside one
  test binary. The helper's own comment called the window theoretical.
  `tests/mcp_protocol_surface.rs` flaked twice in three days.

  They also captured the server's stderr and never read it, and a pipe nobody
  empties eventually blocks the process writing to it — which fits the symptom
  seen: a server that answered `/healthz` and then returned an empty body. The
  watcher spawner already drained, with a comment saying why, but only after
  `/healthz` answered, so the startup window went unread. Reading the assigned
  address requires draining from the moment the child starts, so one change
  closes both.

  Three spawners did it, not one: the shared helper, its watcher variant, and
  `tests/http_transport.rs`, which carried its own copy. `--port` and
  `pick_free_port` now appear nowhere under `tests/`, and the reader that finds
  the address is shared rather than copied — one parser, so a change to the
  server's wording cannot be fixed in one spawner and left in another. The
  flake is intermittent, so this is not shown to have fixed it; what is shown
  is that two known ways for these tests to interfere with each other are gone.

- **No test mutates the process environment any more.** One was left: it set
  `GROOVE_CONFIG_HOME`, asserted, and put it back, with a note saying nothing
  else mutated the environment beside it. True, and beside the point — the
  hazard is not another writer, it is every concurrent reader.
  `TrustRoots::from_env` reads that same variable to decide which directories
  are trusted, so a test calling `Config::discover()` while this one held it
  would have seen `/tmp/groove-test-override` as a trust root, and failed
  somewhere else for a reason invisible from where it failed.

  The judgement now takes its input as an argument — `resolve_config_home_in`,
  matching `Config::discover_in`, which already had this shape. The same move
  applies to `env_dir`'s rule that an empty value counts as *unset*: that rule
  had no test at all, because reaching it meant setting a variable. It is
  `dir_from_env_value` now, and deleting the filter fails a test instead of
  none.

## [0.27.0] - 2026-08-18

### Added

- **The documentation is published as a site.** `docs/` is the GitHub Pages
  publishing source, so the twenty-two reference pages and nine ADRs are
  readable at <https://alphabet-h.github.io/grooveseek/> without cloning
  anything. Both languages are published; every page already linked to its
  counterpart, and `jekyll-relative-links` — on by default — resolves those
  links, so the language switch is the one that was already in the text.

  The repository root was the other possible source and was not chosen: it
  would have published ninety-four Markdown files, thirty-two of them synthetic
  test fixtures, plus the source tree as static files, and would have needed an
  exclusion list maintained against a repository that is mostly not
  documentation.

  Six links inside `docs/` pointed outside it — at
  `grooveseek/examples/` and `groove.toml.example` — and would have resolved to
  nothing on a site whose root is `docs/`. They are absolute now, for the same
  reason the README's images are.

- **The README has a face: a mark, a screenshot of `/ui`, and three badges.**
  [ADR-0007](docs/decisions/0007-rename-the-project-to-grooveseek.md) accepted,
  knowingly, that "GrooveSeek" says nothing about what the product does and that
  searching for "groove" lands in music software — and concluded that this "makes
  the first line of the README load-bearing". The mark is lines of a document
  with the shipped `◆` marking the passage a search found, in the same accent
  the web interface uses; light and dark variants are selected with `<picture>`.

  The badges are CI, latest release, and the licence. There is deliberately no
  crates.io or downloads badge: every crate here is `publish = false`, so both
  would be false.

  Images are referenced by absolute URL rather than repository-relative path,
  because a release archive ships this README without `assets/` — the same
  reason the documentation links were made absolute in the previous change.
  They point at PNG renders rather than the SVG sources: an absolute URL
  resolves to `raw.githubusercontent.com`, which is reported to serve `.svg` as
  `text/plain` so an `<img>` will not render it, and the screenshots are PNG in
  any case. `assets/README.md` records the reasoning and how to regenerate.

- **`[transport.http].allowed_origins`.** Names the browser origins the server
  accepts. Needed when a browser reaches groove through a reverse proxy, because
  the browser then sends the *public* origin and the loopback default will not
  match it. Entries carry a scheme and bracket IPv6, since they are compared as
  RFC 6454 `(scheme, host, port)` triples.

  Setting it **replaces** the default list rather than extending it, matching
  `allowed_hosts`. Keep the loopback entries alongside your public origin if
  browser-based clients also reach you over loopback.

  An empty list disables validation entirely and now warns at startup. Like
  `allowed_hosts`, `healthz_public` and `max_sessions`, the key is **ignored when
  it comes from a config file groove discovered rather than one you passed with
  `--config`** — otherwise whoever can write a `groove.toml` beside the binary
  could name their own origin, or blank the list, and turn the check off.

- **A stability policy: [docs/stability.md](docs/stability.md).** It states what
  1.0.0 will freeze and — more usefully — what it deliberately will not. Without
  it, tagging 1.0.0 would promise that everything observable stays fixed until
  2.0.0: 408 public Rust items across 24 modules, 138 command-line flags, 6 MCP
  tools, 11 configuration sections, and a SQLite schema.

  Stable from 1.0.0: subcommand names and documented flags, exit codes, the
  stdout/stderr split, the JSON from `search` and `graph` (fields may be added, so
  ignore ones you do not recognise), MCP tool and prompt names with their schemas,
  the `kb://` resource scheme, `/mcp` and `/healthz`, configuration keys and
  defaults, the default embedding model, and the names written into your
  filesystem.

  Explicitly **not** stable: `/ui` and `/api/*` (loopback-only admin surface, due
  to be rebuilt), all human-readable text output, the internal database schema, log
  wording, and the Rust API. Reasoning: [ADR-0008](docs/decisions/0008-declare-what-1-0-freezes.md).

### Changed

- **`groove service uninstall` and `service status` take `--service-name`
  instead of a positional.** `install`, `tray-install` and `tray-uninstall`
  already named the instance with a flag, so the same thing had two spellings —
  `install --service-name work` against `uninstall work`.
  [docs/stability.md](docs/stability.md) freezes subcommand positionals as well
  as long flags, which would have kept both forever, and a positional cannot be
  taken away afterwards at all.

  The two also gained the name validation the other three already had. A name
  `install` refuses can never have been installed, so nothing that used to work
  stops working.

- **`docs/stability.md` now says what it freezes, rather than leaving it to be
  inferred.**

  *Which flags.* The promise is scoped to the `groove` binary and to flags this
  documentation describes — and "documented" is now checked by a test rather
  than assumed. Two flags were undocumented and would have been left unfrozen by
  accident: `groove validate --schema`, the only way to point validation at a
  schema that does not sit beside the knowledge base, and `--fail-fast`. Both
  are written up in [docs/usage.md](docs/usage.md) now.

  *Which output.* Every subcommand that takes `--format` is listed in one of two
  groups, because nine of them were in neither and silence reads as a promise.
  The JSON of `search`, `graph`, `doctor` and `validate` is stable, as is
  `validate --format github`. Text output is not, from any subcommand; neither
  are `graph --format dot` and `--format svg`, which are drawings; neither is
  the JSON of `eval` and `tune`, whose numbers are expected to improve — `eval`
  already stamps its history with a `metric_version` for that reason.

  *Which channel.* The stdout/stderr split is stated as it actually is. Six
  subcommands produce a result on stdout; `index`, `status` and `service` write
  everything to stderr, so `groove status | …` receives nothing. That was true
  before and the document said otherwise.

- **The command line and the MCP tools now use the same noun for the same
  thing, and [docs/stability.md](docs/stability.md) says which parts of the two
  surfaces correspond.** Both are frozen at 1.0.0, so this is the last release
  that can move either one.

  Two names were one concept called two things. `groove graph --exclude` is now
  `--exclude-paths`, matching the tool's `exclude_paths`; and the tool's `path`
  is now `start`, matching `groove graph --start`. The tool took the flag's word
  rather than the other way round, because `--path` beside `--kb-path` reads as
  the corpus, and `get_document` keeps `path` for the document it fetches.

  What remains different is deliberate, and is now written down instead of being
  inferred: a repeatable flag is singular where the array it fills is plural
  (`--path-glob` / `path_globs`, `--tag-any` / `tags_any`), tool names and
  subcommand names do not correspond at all (`get_connection_graph` is
  `groove graph`), and `rerank` is a per-call boolean while `--reranker` picks a
  model. Neither shape is unusual — `gh --label` fills the REST API's `labels`,
  and `docker --publish` fills Compose's `ports` — so the rule is that the
  mapping is predictable, not that the strings are equal.

  *Values* are held to a stricter rule, because a name that differs costs a
  lookup while a value that differs fails the call outright: `seed_strategy` now
  takes `all_chunks` and `all-chunks` on both sides. Copying either spelling
  from one surface to the other used to be rejected — by clap on one side and by
  `unknown seed_strategy` on the other. There is one table of accepted
  spellings and both parsers read it, so a strategy cannot become reachable on
  one surface only; `--help` still advertises the one spelling the command
  line's own conventions produce.

  A test pins the pairing itself. Adding a parameter to either surface fails
  until the table names its counterpart or records why it has none, which puts
  the question in front of whoever adds it while the answer is still free.

- **`docs/ARCHITECTURE.md` stopped calling `/ui` a disposable placeholder.** It
  still described the file as "a disposable placeholder — a proper redesign is
  expected in Phase 3+" after that redesign had shipped.

- **`/ui` shows the knowledge-base path the way it was typed.** Windows
  canonicalisation returns an extended-length path, so the status band read
  `\\?\C:\notes` where the operator had passed `C:\notes`. The prefix is now
  stripped for display only; `/api/admin/status` still returns what it returned,
  because the tray reads that field too.

- **The README is an entry point again, and the reference it used to carry now
  lives under `docs/`.** It had grown to 1,057 lines, of which 1,004 — 95% —
  were configuration, CLI and client reference that a first-time reader has to
  scroll past to reach "what is this and how do I install it". Those five
  sections moved verbatim into `docs/configuration.md`, `docs/usage.md`,
  `docs/clients.md`, `docs/mcp-tools.md` and `docs/behavior.md` (each with its
  `.ja.md` pair), and the README is now 112 lines: what it is, how to install
  it, a quick start, and an index of the rest.

  **Links into the old sections change.** Anchors that pointed at, say,
  `README.md#config-file-discovery` now live at
  `docs/configuration.md#config-file-discovery`; the section names and their
  anchors are unchanged, only the file is. Everything inside the repository
  that referenced them was updated in the same commit.

  Two things are read outside the repository and were handled separately: a
  release archive ships the binary and this README but no `docs/`, so the
  Documentation section says so and gives an absolute URL, and `groove --help`
  now names that URL rather than a path the reader may not have.

- **`/ui` is the operator's view of their own server, and it searches through
  `/mcp`.** It shows a status band — version, documents, chunks, model, watcher,
  uptime, pid, indexing progress — over a search box, replacing a placeholder
  that said "MVP" and "to be redesigned" in its own markup while the project was
  preparing to call itself stable. Still one file, no external requests, and
  every string out of the knowledge base placed with `textContent`.

  Routing its search through `/mcp` rather than a private endpoint means the
  page exercises the same surface an external client would, and **puts `/ui`
  under `Origin` validation for the first time**. With the default list it
  works; an `allowed_origins` that names only a public origin leaves the page
  served but unable to query, and the server now warns about that at startup
  rather than leaving a silent 403 on screen.

- **`grooveseek` is marked `publish = false`.** The Rust API is not part of the 1.0
  promise, and `cargo package` cannot succeed anyway while the workspace uses
  unversioned path dependencies. `cargo publish` now refuses rather than relying on
  a documentation note. `[package.metadata.dist] dist = true` was added in the same
  change, without which cargo-dist would silently stop shipping the main binary.
- **Configuration files are declared not forward compatible.** Unknown keys stay an
  error, so a 1.0.x binary will refuse a configuration written for 1.1. The
  alternative would let `modle = "bge-m3"` index a knowledge base with the wrong
  model behind a single warning on a daemon's stderr.
- The README titles now name the product (**GrooveSeek**) rather than the command
  (`groove`).
- **[docs/stability.md](docs/stability.md) now says where GrooveSeek is meant to
  run.** Having no authentication is a design position, not a gap awaiting work,
  and saying so is what makes the rest of the policy coherent: the HTTP transport
  expects to be reached from the same host, with the network boundary owned by a
  container, a reverse proxy, or the application that puts a face on the knowledge
  base. Non-loopback binds stay allowed — a container has to bind one or published
  ports never reach it — but they mean you have taken that boundary on yourself.
- **"Is this address loopback?" now has one answer instead of three.** The admin
  router unwraps IPv4-mapped IPv6 (`::ffff:127.0.0.1`) and treats it as local;
  `groove serve` asked `IpAddr::is_loopback`, which says no; and
  `groove service install` matched on string prefixes. So binding to a mapped
  loopback address was refused as "network exposure" without `--i-know`, while
  a peer arriving from that same address was being let into `/ui`. All three
  now call one predicate, and **`--bind [::ffff:127.0.0.1]:PORT` no longer
  demands `--i-know`** — it is a loopback address, and the rest of the server
  already behaved as though it were. Nothing else changes: every other address
  the old predicates already agreed on.
- **The refusal printed for a non-loopback `--bind` now states the consequence.**
  It used to say groove "has no auth" and that exposure "is dangerous", which
  leaves the reader to work out what is actually at stake. It now says that
  anything able to reach the port can read the entire knowledge base, and that
  `Host` validation and the session cap are not authentication. Same text in
  `groove serve` and `groove service install`.
- **The admin web surface is now documented as scheduled to go away.**
  `docs/stability.md` records the intent to retire `/ui` during 1.x, once a
  client that speaks `/mcp` exists — browsing belongs there, where every tool
  and every search parameter is reachable and the surface is already stable.
  `/api/admin/status` stays: it reports operational state (version, pid,
  indexing progress) that does not belong in a tool surface built for language
  models. Both remain unstable, so this is notice rather than a promise.

### Fixed

- **The intranet-HTTP recipe never mentioned `allowed_origins`.** That release
  adds Origin validation and turns it on by default, and the recipe it matters
  most for — a reverse proxy terminating TLS in front of the server — explained
  only the `Host` half. Following it as written left every browser-based client
  refused with no indication why. The config template, the threat table and the
  nginx step now name the key and say that a browser behind a proxy sends the
  *public* origin.

- **`docs/behavior.md` said groove has no authentication "yet".** That reads as a
  promise; [docs/stability.md](docs/stability.md) states the opposite — no
  authentication, by design, with the boundary belonging to whatever runs in
  front. A page describing behaviour and a page defining the 1.0 surface must not
  disagree about a security posture.

- **`docs/stability.md` froze an environment variable the binary does not read.**
  `GROOVE_BIN` is a variable of the shipped example hook. The three the binary
  actually reads are `GROOVE_CONFIG_HOME`, `GROOVE_TRAY_LOG`, and fastembed's own
  `FASTEMBED_CACHE_DIR`, which is not ours to freeze. Same shape as the
  `--verbose` entry below, found the same way — by checking the list against the
  code rather than reading it.

- **`groove search` ignored `rerank_by_default`.** The key decided whether
  `serve` reranked every call; the command line did not read it at all. One
  `groove.toml` carrying `reranker = "bge-v2-m3"` beside
  `rerank_by_default = false` therefore reranked from the CLI and did not rerank
  from the server — and three of the shipped deployment recipes are that exact
  pair. The difference is not subtle: measured here on a warm cache, the same
  query took 7 seconds without the cross-encoder and 72 with it.

  **This changes behaviour.** With `rerank_by_default = false` next to a
  `reranker`, `groove search` no longer reranks. Naming a model on the command
  line opts a single query back in — `--reranker bge-v2-m3` — and
  `--reranker none` opts a single query out, which is how a CLI argument has
  always related to the file. No `--rerank` flag was added for it:
  [docs/stability.md](docs/stability.md) freezes the MCP `rerank` parameter as
  the per-call boolean and `--reranker` as the model picker, and a `--rerank`
  one letter away from it, taking a different type, would be frozen beside it
  at 1.0.0.

  The decision now lives in one function both surfaces call. Each still spells
  its own per-call override — a parameter on one side, naming a model on the
  other — but what an override *means*, and what happens without one, is a
  single expression. Writing that twice is how the two came apart to begin with.

  `groove eval` keeps reading only `--reranker`, deliberately: its run
  fingerprint records the model and not this key, so honouring it would let two
  runs carry the same fingerprint while measuring different pipelines — and
  `--fail-on-regression` picks its baseline by fingerprint equality.

- **`--min-confidence-ratio` accepted `nan` and `inf`.** A non-finite ratio
  compares false against every score, so a value passed in order to *tighten*
  the low-confidence check switched it off instead. The JSON echo could not
  report that either: serde writes a non-finite float as `null`, and the
  null-stripping pass then drops the key, leaving output with no trace of the
  override. The flag now requires a finite value `>= 0.0` — `0.0` is still how
  the check is disabled — and rejects before any model is loaded.
  `[search].min_confidence_ratio` in `groove.toml` is held to the same rule by
  the same predicate, which matters because that is the path `serve` reads. The
  MCP parameter is unchanged: it cannot refuse a value mid-conversation, so it
  substitutes — a non-finite ratio is logged and replaced by the server's own,
  and a negative one is clamped to `0.0`.

- **`docs/usage.md` said the CLI and the MCP tool answer with the same JSON.**
  The wrapper is the same — `results`, `low_confidence`, `filter_applied` — but
  the hits are not: an MCP hit also carries a `uri` when the document is one the
  server will hand over, and a CLI hit never does. The sentence now says which
  part is shared and links to where the `uri` rule is written down.

- **`docs/stability.md` described a flag that does not exist.** It offered
  `--verbose` as the way to get more detail; `groove` has never had one.
  Verbosity comes from `RUST_LOG`, which appeared nowhere in the documentation.
  The paragraph names the real mechanism now.

### Removed

- **`/api/search`.** It accepted `query` and `limit` — 2 of the 17 parameters
  the MCP `search` tool takes — so `/mcp` was already the better endpoint for
  anything outside the process, and `/ui` uses `/mcp` now. The endpoint was
  declared unstable in [docs/stability.md](docs/stability.md), and this removes
  it before 1.0.0 rather than during it.

  If you were calling it directly, `/mcp` answers the same query with the whole
  parameter set and no session handshake — see the request shape in the
  `/ui` source (`grooveseek/src/transport/webui_index.html`), which is now the
  smallest working example of an MCP client over Streamable HTTP.

### Security

- **The HTTP transport now validates the `Origin` header, which it never did.**
  The MCP specification's Streamable HTTP section states that a server *"**MUST**
  validate the `Origin` header on all incoming connections to prevent DNS
  rebinding attacks"*. rmcp implements the check but defaults it to an empty
  list, which means *do not validate*, and groove never set it — so every release
  up to and including v0.26.0 accepted any `Origin`. Measured against a running
  v0.26.0 daemon: `Origin: http://evil.example` was answered normally.

  The default is now the loopback origins for whichever port is bound
  (`http://localhost:PORT`, `http://127.0.0.1:PORT`, `http://[::1]:PORT`).

  **This does not break existing clients.** Per RFC 6454 a request that carries
  no `Origin` header passes, and ordinary MCP clients, the tray and `curl` send
  none. What it stops is a web page open in the operator's own browser reaching
  `/mcp` cross-origin. It is not authentication, and groove still has none.

  **It covers `/mcp`, and `/ui` searches through `/mcp`** (see below), so this
  list decides whether the built-in page can query. `/api/admin/status` has no
  `Origin` check of its own; it is restricted by requiring a loopback peer,
  which is not configurable.


## [0.26.0] - 2026-08-17

### Changed

- **BREAKING — the project is now GrooveSeek, and the command is `groove`.**
  The old name collided inside its own category (`github.com/moikas-code/kb-mcp`
  is also a knowledge-base MCP server) and bound the product to one of the two
  ways it is read — a browser opening `/ui` is the other. The rename lands now
  because the name is written into your filesystem, and after 1.0.0 changing it
  would mean carrying a "look for the old name too" layer for all of 1.x.
  Reasoning and the candidates that were measured and rejected:
  [ADR-0007](docs/decisions/0007-rename-the-project-to-grooveseek.md).

  **There is no automatic migration, and no aliases.** A 0.26.0 binary does not
  see anything left by 0.25.0. To carry an install over:

  | Old | New |
  |---|---|
  | `kb-mcp` (command) | `groove` |
  | `kb-mcp.toml` | `groove.toml` |
  | `.kb-mcp.db` | `.groove.db` |
  | `.kb-mcpignore` | `.grooveignore` |
  | `.kb-mcp-eval-history.json` | `.groove-eval-history.json` |
  | `.kb-mcp-eval.yml` | `.groove-eval.yml` |
  | `KB_MCP_CONFIG_HOME` | `GROOVE_CONFIG_HOME` |
  | `KB_MCP_TRAY_LOG` | `GROOVE_TRAY_LOG` |
  | `KB_MCP_BIN` | `GROOVE_BIN` |
  | `KBMCP_BENCH_KB` | `GROOVE_BENCH_KB` |
  | `kb-mcp-svc` / `kb-mcp-tray` | `groove-svc` / `groove-tray` |
  | `<config_dir>/kb-mcp/<service>/` | `<config_dir>/groove/<service>/` |

  Renaming the files is enough — the formats did not change, so the index does
  not need rebuilding. A service registered by `kb-mcp service install` must be
  uninstalled with the **old** binary before `groove service install` is run;
  the new binary does not know the old registration exists.

  `.mcp.json` entries need their `"command"` updated to `groove`. The MCP server
  now identifies itself as `grooveseek` in `serverInfo.name`.

### Fixed

- **The watcher missed every file inside a directory that was newly created
  under the knowledge base — on Linux.** Copy a folder of notes into a watched
  KB and its contents stayed unindexed until the next full `groove index`; the
  directory event arrived, the files' did not.

  This is not a debounce or a deadline: the events are **unobservable**. inotify
  watches are per-directory, so a file written into a directory that was created
  microseconds earlier is reported by no watch at all — not the parent's, which
  only names the directory, and not the new directory's, which is registered too
  late. Measured on Ubuntu 22.04 with raw inotify: the file was on disk 0.79 ms
  after `mkdir`, and the earliest a watcher could register the new watch was
  2.41 ms. Nothing inside `notify` recovers it, so the watcher now looks inside
  a directory once when it appears.

  Windows was never affected — `ReadDirectoryChangesW` watches the subtree from
  a single handle — which is why this survived unnoticed until a Linux-only CI
  failure.

  What gets indexed is decided by the **full index walk's** filter, now reachable
  for a subtree, so a directory drop and a later `groove index` agree. The count
  is logged: a directory drop is never a silent bulk index.

## [0.25.0] - 2026-08-16

### Added

- **`kb-mcp graph --format dot` and `--format svg`** (E-3). The walk's value is
  its shape, and neither existing format showed it: `json` and `text` list the
  same nodes without saying where the search branched.

  `dot` emits a Graphviz program to pipe at `dot -Tsvg`, open in a DOT viewer,
  or paste into a web one. `svg` is a finished drawing that needs **nothing
  installed** — which is the point of having it. Nodes are coloured by BFS
  depth, edges carry the similarity score, and both formats **state when a limit
  cut the walk short**, so a picture is never read as the whole neighbourhood.

  **No drawing dependency was added.** A general graph would need a layout
  engine, but this one is a tree — each node carries a single `parent_id` and
  the walk never reaches a node twice — so depth becomes the column and sibling
  order the row, in one pass. The candidate crate was also last published 16
  months ago; not needing it at all is the better answer.

  The formats live on a `graph`-only enum. Sharing `search`'s would have grown a
  `search --format dot` with no graph to draw.

  Escaping is worth a note for anyone extending this: the DOT grammar says the
  only escape inside a quoted string is `\"`, so a backslash is **not** an escape
  character to the lexer — while the label renderer does read `\n` and `\l` as
  directives. On the reference corpus 74 of 8773 headings contain a double quote
  and 4 contain a backslash, so both paths run on the first real graph rather
  than in theory.

## [0.24.0] - 2026-08-15

### Added

- **`kb-mcp eval` reports a corpus that quotes its own golden set** (D-12).

  If you keep notes about the evaluation inside the knowledge base being
  evaluated, a note that quotes a golden query verbatim becomes the strongest
  match for that query — it takes the top slot and pushes the labelled answer
  down. **The more you write about the evaluation, the harder it is to pass**,
  and until now nothing said so; the one case found on the reference corpus was
  noticed only because someone happened to read the per-query rows.

  Each run now scans the indexed corpus once and reports documents that quote
  **two or more distinct golden queries** verbatim, on stderr and in
  `--format json` under `findings`. **The exit code is unchanged** — a quote is
  either a note that leaked in or the source the query was written from, in
  which case the document belongs in that query's `expected`, and only the
  author of the golden set can tell which.

  Requiring two quotes rather than one is the whole design, and it is measured:
  golden queries are often topic names (`cross-encoder`, `torch.compile`), which
  appear verbatim in the documents explaining them, so reporting single matches
  produced **8 findings, all false positives**, on a healthy 662-document
  corpus — where the rule as shipped produced exactly one, and it was the note
  that was in fact documenting the golden set. Reasoning:
  [ADR-0006](docs/decisions/0006-report-a-corpus-that-quotes-the-golden-set.md).

## [0.23.0] - 2026-08-15

### Added

- **`kb-mcp doctor`** (D-8). Asks the index whether it is in the state it should
  be, and reports; it never repairs.

  Search reads three tables that have to agree about a chunk — its text, its
  embedding, its full-text row. **When they stop agreeing nothing errors.** A
  chunk with no embedding is simply never a vector hit; one with no full-text
  row is never a keyword hit. That this happens is not hypothetical —
  `backfill_fts` exists precisely to repair it — but until now the only way to
  discover it was to run a full index and watch the repair go by.

  It also explains what the MCP resource surface is holding back: an extension
  no longer in `[parsers].enabled`, a document larger than a resource read
  returns, or a size not recorded yet because the document was indexed by an
  earlier version. Those answers come from calling the server's own
  `paths_with_unregistered_extension` and `ServableRules` rather than
  recomputing something equivalent — a doctor that answers a slightly different
  question than the server is worse than no doctor.

  Report on stdout, `--format text|json`, exit `0` (nothing to report), `1`
  (findings), `2` (could not run — usually no index). Each finding carries the
  command that fixes it. **Not implemented on purpose**: a `--fix` flag. The
  narrower version of this contract already exists and already says report,
  suggest `kb-mcp index`, never delete.

  Like `search` and `eval`, it opens the database, and opening one applies any
  pending schema migration — read-only about its findings, not about the file.

### Fixed

- **A document the resource surface offered could be one a read refuses.**
  Indexing accepts 50 MiB of text; `resources/read` returns at most 1 MiB. A
  Markdown or plain-text document in between was indexed, listed under its topic
  group, given a `uri` on its `search` hits — and refused when a client followed
  that link. v0.22.0 recorded this as a known limitation because the only way to
  know a file's size was to stat every indexed file on every listing, which
  would have made an offer a live filesystem probe rather than a property of the
  index.

  The index now knows the size. `documents` gains a nullable `size_bytes`,
  written wherever a document row is written, and the predicate that decides
  what is offered applies the **same** per-extension cap a read applies —
  `max_bytes_for`, the chooser `load_document_blocking` already passes to
  `read_checked` — so the listing and the read cannot enforce different limits.
  A binary document over the text cap is still offered, because a read truncates
  its extracted text rather than refusing it. Reasoning:
  [ADR-0005](docs/decisions/0005-record-document-size-in-the-index.md); the
  principle it preserves is
  [ADR-0004](docs/decisions/0004-resource-reads-are-bounded-by-the-index.md)'s.

  `resources/list` and the `uri` on a `search` hit are now one predicate rather
  than two calls that happened to agree. They each tested the parser registry
  separately, which was harmless only while that was the whole rule; adding the
  size condition to one of them is exactly what would have produced the defect
  being fixed. The hit itself is unaffected either way — an unservable document
  stays findable and simply carries no link.

  A file that **grows past the index cap** after being indexed is refused and
  its new size recorded, by the full run and the watcher alike. A refusal
  preserves the row, so otherwise the recorded size would stay the last one
  small enough to index while the file became one no read can return. This is
  knowable — kb-mcp stat'd the file in order to refuse it — unlike a file
  deleted or replaced after indexing, which a listing still cannot answer for.

  **Existing indexes**: the column is added on open with every row NULL, which
  means "not recorded" and is treated as servable, so nothing disappears from a
  listing after an upgrade. One `kb-mcp index` fills it in **without
  re-embedding**: the sizes come from the disk scan, so documents whose content
  hash is unchanged — which is all of them, on a knowledge base that was just
  upgraded — are backfilled even though that path writes no document row.

## [0.22.0] - 2026-08-15

### Added

- **MCP resources** (B-2), under the `kb://` scheme.

  `resources/list` returns one entry per **topic group** — the first one or two
  path segments, the same derivation the indexer uses for `category` and
  `topic` — not one per document. A knowledge base has hundreds of documents but
  tens of groups, and a listing is what a client fetches on every connect.
  Individual documents stay reachable through the `kb://doc/{path}` template and
  through the **`uri` field now on every `search` hit**; the specification
  permits handing back links to documents a listing never enumerated.

  **A read is bounded by the index.** A document is served only if it is
  indexed, and then only through the same checks `get_document` applies. That is
  *narrower* than `get_document`, which serves anything under `kb_path` with a
  registered extension — a resource is something the server offered, so serving
  a URI that was never on offer is a different operation. `.kb-mcpignore`d
  documents are therefore absent from resources while remaining readable through
  `get_document`, which leaves ADR-0003's contract exactly as it was. Reasoning
  in [ADR-0004](docs/decisions/0004-resource-reads-are-bounded-by-the-index.md).

  The guard sequence now lives in one function that both `get_document` and
  `resources/read` call, rather than being written twice.

  Content comes back as text with the media type of what is served —
  `text/markdown` for Markdown, `text/plain` for anything delivered as extracted
  text. A PDF is served as the text kb-mcp extracted from it, so it is not
  described as a PDF. Extraction above 1 MiB is truncated, as it already was for
  `get_document`; since a resource read has no envelope to put a `truncated`
  field in, it appends a marked notice rather than presenting a prefix as the
  whole document.

  `search`'s result gains one field and changes shape in no other way: the MCP
  result is still a single text content block carrying the same JSON, so
  existing clients are unaffected. The field is omitted for a hit whose
  extension the active parser registry no longer covers — such a row stays
  indexed and stays in the results, but no read would open it, so the honest
  answer is no link rather than a broken one. The same filter decides what
  `resources/list` offers.

  Not implemented: `resources/subscribe` and
  `notifications/resources/list_changed`.

- **MCP prompts** (B-3). Four of them, surfaced by clients as commands the user
  picks — Claude Code renders them as `/mcp__kb-mcp__<name>`:

  | Prompt | Arguments |
  |---|---|
  | `summarize_topic` | `topic` (required) |
  | `deep_dive` | `question` (required) |
  | `whats_new` | `since` (optional ISO date) |
  | `find_gaps` | `topic` (optional) |

  Each exists because the tools alone do not say how to combine them: `search`
  never tells a caller to expand its best hits with `get_connection_graph`, or
  that a `low_confidence` flag means the answer should say so. All four are
  plain text and share one set of citation rules — cite the `path` of every
  document used, surface `low_confidence` rather than answering through it, and
  say when the knowledge base is silent instead of filling the gap.

  `whats_new` states its own limitation rather than implying otherwise:
  `date_from` filters the frontmatter `date`, which is what an author typed, not
  when a file was modified or indexed. kb-mcp has no query for "recently
  changed", so the prompt describes itself as an approximation.

  Fixed at compile time rather than configurable. Prompt text reaches the model
  and `kb-mcp.toml` is *discovered* — from the working directory or a `.git`
  ancestor — so a `[prompts]` section would sit in the same privileged category
  as `kb_path` under `restrict_untrusted`. The specification offers no cover
  here: unlike tool annotations, it gives clients no guidance to distrust prompt
  content.

- **The server now identifies itself.** `initialize` answered
  `serverInfo {"name":"rmcp","version":"3.1.2"}` — the SDK's build environment,
  because the whole `ServerHandler` impl was macro-generated and nothing
  supplied a `get_info`. Clients display that name. It is now `kb-mcp` and this
  crate's version. Splitting the generated impl was also the prerequisite for
  declaring any new capability, since a generated impl cannot be extended;
  measured before and after, `tools/list` is byte-identical.

- **The first protocol-level assertions in the repo**
  (`tests/mcp_protocol_surface.rs`): the advertised capability set, the tool and
  prompt lists as they appear on the wire, the server's own name, and the
  caching hints the specification requires on a complete result. They read
  **both** discovery surfaces — `initialize` and `server/discover` — because a
  test that reads only the former is checking the dialect 2026-07-28 moved on
  from. Nothing asserted any of this before, which is how the wrong server name
  survived fifteen releases.

### Fixed

- **A path that could not be examined no longer reports as a path that is not
  there.** `validate_get_document_path` probes the filesystem three times, and
  all three put every I/O failure into "File not found" — so a permission error
  on an indexed document sent the caller hunting for a typo that did not exist.
  Each probe now separates the two: `NotFound` and `NotADirectory` say the path
  cannot be there; everything else says the server could not look.

  `get_document` gains a more accurate message in that case ("Failed to
  examine …" rather than "File not found: …"), and `resources/read` reports it
  as an internal error rather than `RESOURCE_NOT_FOUND` — the difference
  between a client retrying and a client giving up.

## [0.21.0] - 2026-08-15

### Added

- **`.kb-mcpignore`** (C-7). A file of that name in the **root of the knowledge
  base** excludes paths in [gitignore syntax](https://git-scm.com/docs/gitignore)
  — globs, `**`, a trailing `/` for directories only, a leading `/` to anchor,
  and `!` to re-include. `exclude_dirs` could only ever name whole directories;
  this can name files and patterns:

  ```
  drafts/
  *.tmp.md
  archive/**
  notes/*.md
  !notes/keep.md
  ```

  The layers are a union — the built-in `.git` / `.svn` / `node_modules`
  fail-safe, then `exclude_dirs`, then this file — so `!` can only undo an
  earlier line of `.kb-mcpignore` itself. Matching is case-insensitive on every
  platform, as `exclude_dirs` already was, so one file behaves the same way on
  all three. Only the root's file is read: no subdirectory files, nothing above
  `kb_path`, and **not `.gitignore`**, since a knowledge base kept in git often
  ignores exactly the large files you want indexed.

  **It bounds indexing, not access.** An excluded file is never indexed, so it
  can never appear in `search` or `get_connection_graph` — both read from the
  database and never touch the filesystem — but it stays readable through
  `get_document` by a caller that knows its path, exactly as a file under
  `exclude_dirs` always has. Whoever can write into the knowledge base can also
  delete the ignore file, so a rule living inside the tree is not what guards
  the tree; keep anything that must not be readable outside `kb_path`. The
  reasoning, and the implementation choices below, are recorded in
  [ADR-0003](docs/decisions/0003-kb-mcpignore-bounds-indexing-not-access.md).

  The full index walk, `kb-mcp validate` and the live watcher all apply it, and
  they now share one decision rather than three implementations of it — AU-03
  and BU-19 were both a surface that had quietly stopped agreeing with the
  others, and both were found after release. One test runs the index walk and
  the watcher over the same fixture and asserts they answer alike.

  Editing the file while the server runs takes effect for subsequent file
  events; documents already indexed stay until the next `kb-mcp index` or MCP
  `rebuild_index`, which re-reads it and drops what it now excludes. An ignore
  file that exists but cannot be read — a hard link, a symlink, a directory,
  over 64 KiB, or past 1000 patterns — warns and is left out (or truncated)
  rather than stopping the run. It is read through the same handle-bound guard
  as any note (v0.20.0), and a leading UTF-8 BOM is stripped, since otherwise it
  becomes part of the first pattern and that line silently matches nothing.

  For a knowledge base with no `.kb-mcpignore` the only change is internal:
  `walkdir`'s `filter_entry` now consults the exclusion rules for files as well
  as directories, because `exclude_dirs` only ever named directories.

  New dependency: `ignore` (used as a matcher only — `walkdir` still does the
  walking, for the reasons in the ADR). All eleven of its transitive
  dependencies were already present, so the addition is one crate, plus
  `regex-automata` moving 0.4.14 → 0.4.18.

### Fixed

- `HARDCODED_EXCLUDE_DIRS`'s documentation called the configuration key
  `[indexer].exclude_dirs`. It is top-level `exclude_dirs`; there is no
  `[indexer]` section.

## [0.20.0] - 2026-08-15

### Security

- **The link check and the bytes now come from the same open handle** (BU-20
  residual).

  v0.19.0 refused a file with more than one name, but it checked a *path* and
  read the bytes later. A knowledge-base writer who could time a rebuild could
  show the check an ordinary file and rename a hard link over that path before
  it was opened — needing no power over the original at all. `links::read_checked`
  now opens the file once and takes the link count, the file type and the size
  limit from that one `fstat`, then reads the content from the same descriptor.
  There is no name left in the loop to substitute.

  This needed no parser API change. The note that said otherwise was wrong:
  `trait Parser` is bytes in, text out — the PDF and Office parsers wrap
  `Cursor::new(bytes)` and never touch the filesystem — so the six `fs::read`
  call sites were the whole surface.

  Three things ride along, because the handle was already open and the metadata
  already fetched:

  - **The size cap is enforced on the handle**, not only on an earlier `stat` of
    the path. The path-based checks still run as the cheap pre-filter; this one
    is the limit that cannot be swapped past, which matters for exactly the
    reason the rest of this entry exists. The read is bounded a second time as it
    runs, so a file that grows underneath cannot outrun it either.
  - **Non-regular files are refused.** A named pipe left in place of a note used
    to hang `get_document` on Unix, which never had an `is_file()` check at all.
  - **On Unix the open carries `O_NOFOLLOW` and `O_NONBLOCK`**, so a *symlink*
    renamed over a collected path is refused rather than followed, and a FIFO
    cannot park the index run. Windows deliberately gets neither: creating a
    symlink there needs administrator privilege (measured), and refusing reparse
    points would refuse every OneDrive placeholder. An intermediate directory
    swapped for a symlink stays out of reach on both — that needs `openat2`.

  Still not closed, and now written down in the module docs, both READMEs and
  `.dev/known-issues.md`: link-then-unlink leaves a count of 1 and is
  indistinguishable from a file that was always there; and the count is only what
  the filesystem reports, so a knowledge base on FAT32, exFAT or a network share
  gets nothing from this guard at all.

  `rename_single_file` gained a `RenamedButRefused` outcome, following the
  `RenamedSizeCapped` precedent: `rename_document` has already committed by the
  time the new path is read, so a refusal there is neither a failure nor a
  successful reindex and must not be logged as either. `SingleResult` gained a
  `Refused` variant to carry it — the rename path reads the file twice (once to
  hash, once to index), and the premise of this whole guard is that a path can
  change between two reads, so a refusal on the second one has to survive rather
  than fall into a catch-all that reports a successful rename.

### Fixed

- **An installed service now names its own config file, so it is trusted
  whatever the environment looks like when it starts** (BU-07 residual).

  `kb-mcp service install` writes `<config home>/kb-mcp.toml` and registers a
  service whose working directory is that config home, so the daemon used to
  *discover* its config rather than being handed it — arriving as
  `ConfigSource::Cwd`, which is only trusted when the directory sits under a
  known root. `TrustRoots` builds those roots from `KB_MCP_CONFIG_HOME` and
  `dirs::config_dir()`, **read at start-up**. Set `KB_MCP_CONFIG_HOME` for the
  install command alone and the daemon could no longer see it, so it classified
  its own config as untrusted: warnings on every start, `allowed_hosts` /
  `healthz_public` / `max_sessions` dropped, and a non-loopback `bind` demoted
  to loopback.

  The unit, plist and scheduled task now carry
  `--config <config home>/kb-mcp.toml`, which makes the source `Explicit` and
  the trust unconditional. Two quoting layers had to be got right for that, and
  both are pinned by tests: `ExecStart` is whitespace-split and `%` is a systemd
  specifier, so the path goes through the same `systemd_exec_word` the binary
  path already used; and Task Scheduler's `-Argument` value becomes the child's
  raw command line, so the path is double-quoted for `CommandLineToArgvW`
  *inside* the PowerShell single-quoted string — a profile directory such as
  `C:\Users\John Doe` would otherwise have split `--config` from its value at
  the next logon, with the daemon's stdio nulled by `kb-mcp-svc`.

  **A service registered by an earlier version keeps its old launch line.**
  Re-run your own `kb-mcp service install` command with `--force` added — and set
  `KB_MCP_CONFIG_HOME` again if you set it the first time, since the config home
  is resolved from the environment of the install command and is not remembered
  anywhere. Without it the re-install writes a different, minimal config and
  points the service at that, which would hit exactly the people this fix is for.
  Default installs were never affected — `dirs::config_dir()` is available at
  start-up too.

  Re-installing now also restarts the service on Linux and macOS, so the new
  launch line takes effect immediately. It had to: `launchctl bootstrap` does not
  replace an already-loaded job — it fails — so `--force` over a running agent
  errored out *and* left launchd holding the old `ProgramArguments`. The macOS
  backend now boots the job out first (best-effort, as `uninstall` already did),
  and the Linux backend uses `systemctl restart` where `start` was a no-op over a
  running unit — plus `try-restart` for a `--no-auto-start` unit, which restarts
  it only if it is actually running, since `--no-auto-start` must not start
  anything that was not already up. Two cases still need a manual restart:
  Windows, where the task is re-registered but the detached daemon is not
  stopped, and an already-loaded `--no-auto-start` LaunchAgent, which the
  installer deliberately does not touch.

  Along the way, `systemd_exec_word` now escapes `$` as `$$` — systemd expands
  `${FOO}` in a command line **including inside quotes**, and a config home such
  as `/srv/${TENANT}` would otherwise have sent the daemon to a path the
  installer never wrote to. This was already true of the binary path before this
  release; the config argument only made it easier to hit.

  `ActionTarget::argument_clause` became `serve_argument` as part of this:
  Task Scheduler accepts exactly one `-Argument`, so the clause is assembled in
  `build_register_script` (which knows the config home) instead of being carried
  whole. The invariant it exists for is unchanged — exactly one side supplies
  `serve`, and `kb-mcp-svc.exe` is still the side that does when it is present.

## [0.19.0] - 2026-08-14

### Security

- **An unauthenticated client could exhaust the HTTP server's memory in
  seconds, and MCP sessions are now bounded** (BU-32).

  rmcp 1.4.0's `handle_post` calls `create_session()` — which inserts into the
  session map and spawns a worker — **before** checking that the body is an
  `initialize` request. The `422` that follows never calls `close_session`; the
  task that owns cleanup is spawned after that early return. The abandoned
  worker then parks on its pre-initialize `recv()`, which has neither the
  keep-alive timer nor the cancellation arm that the post-initialize loop has,
  so nothing ever reclaims it.

  Measured against the release binary over **one** keep-alive connection: 2000
  session-less, non-`initialize` POSTs raised private bytes from 157 MiB to
  274 MiB — about **58 KB per rejected request, none of it returned** — in one
  second, i.e. ~117 MiB/s. No session, no `initialize`, no credentials. On a
  loopback bind that is any local process; with the `intranet-http` recipe it is
  anything on the network. Every release up to and including v0.18.0 shipped
  rmcp 1.4.0 and is affected.

  Two changes:

  1. **rmcp is upgraded to 3.1.2**, which fixes the leak at the source. It was
     reported against 1.4.0 as
     [modelcontextprotocol/rust-sdk#808](https://github.com/modelcontextprotocol/rust-sdk/issues/808)
     and fixed by
     [modelcontextprotocol/rust-sdk#934](https://github.com/modelcontextprotocol/rust-sdk/pull/934)
     — "only creates a session after those checks pass" — released in 2.0.0,
     which also added a `SessionConfig::init_timeout` defaulting to 60 seconds.
     Verified against the published crates: 1.4.0 through 1.8.0 create the
     session first, 2.0.0 onward do not. The same probe now moves memory by
     0.1 MiB. See "Changed" below for what else the upgrade brings.
  2. Live sessions are capped, `[transport.http].max_sessions`, default **256**
     (~25 MB; a live session measured at ~100 KB). While the cap is full, a
     request that would open a *new* session gets `429` with `Retry-After`;
     **established sessions are untouched**. `0` disables the limit. **No rmcp
     release bounds the number of sessions**, 3.1.2 included, so this stays
     kb-mcp's own.

  The cap counts live sessions *and* admissions still in flight, and reads both
  plus the increment inside one critical section that releasing a seat also
  enters. Reading the count and then forwarding would have left the limit
  advisory: every request in a simultaneous burst reads the same below-limit
  count before rmcp inserts anything, so a cap of 1 admitted all 16 of 16
  concurrent requests in a test written to check exactly that. Reasoning about
  the read order instead of excluding the interleaving was not enough either —
  a compare-exchange cannot tell "unchanged" from "changed and changed back",
  and that version measured 5 and 6 live sessions against a cap of 4.

  On rmcp 1.4.0 a cap alone would have made things worse rather than better:
  leaked entries never expire, so an attacker could fill it and leave the server
  permanently unable to accept a legitimate client. That is why the leak had to
  go first — and why it is fixed by the upgrade rather than by a workaround in
  front of it.

  The cap looks at one thing: whether the body is a single `initialize` request.
  That is the MCP specification, not an rmcp implementation detail, and after
  SEP-2567 it is exactly the request that creates a session — the 2026-07-28
  protocol removed both sessions and `initialize`, so a modern request holds
  nothing and is never inspected further or refused. `Host`, `Accept` and
  `Content-Type` validation stay delegated to rmcp, the same call made for
  `Host` in v0.7.6.

  From an untrusted config `max_sessions` is dropped like `allowed_hosts` and
  `healthz_public`, which for a *limit* means falling back to the built-in
  default: honouring it would let a planted `max_sessions = 1` leave the server
  unable to accept a second client.

  The refusal is logged at most once a minute, with a count of what it stands
  for. Logging every refusal produced 1744 lines from that one-second probe, and
  the daemon sends stderr to a file.

- **A `kb-mcp.toml` that kb-mcp found by itself is no longer trusted in full**
  (BU-07). Discovery honours `./kb-mcp.toml` and a `kb-mcp.toml` at the `.git`
  root, walking up to 20 directories — files the user never named. Whoever
  controls that directory (a cloned repository, a shared drive, an extracted
  archive) controlled them, and the only record was one log line naming the
  `ConfigSource` variant.

  Reproduced against the release binary before fixing:

  - `fastembed_cache_dir = "evil-cache"` plus a two-file HuggingFace cache
    layout made `kb-mcp index` hand the planted bytes to ONNX Runtime
    (`Load model from ...\evil-cache\...\model.onnx failed: Protobuf parsing
    failed`) — no download and no verification, because hf-hub returns a cached
    blob whenever the file exists and neither it nor fastembed checks a hash or
    signature. A valid model would have loaded and run.
  - `kb_path` pointed at another tree made `kb-mcp validate` scan it; `index`
    and `serve` follow the same field, which is what reaches an LLM client.
  - `[transport.http].bind` could open a network listener: the non-loopback
    gate added in BU-01 covers CLI `--bind` but deliberately exempts
    config-file binds, on the reasoning that a config file states the
    operator's intent — which does not hold for a file found in someone else's
    directory.

  Trust is now decided **by location only**, never by the file's contents:
  `--config`, the binary's directory, a `kb-mcp service install` config home,
  and "no file" are trusted; anything else found under the cwd or a `.git`
  ancestor is not. An untrusted config still loads, and everything that shapes
  how a knowledge base is presented (`[search]`, `[quality_filter]`,
  `exclude_dirs`, `[parsers]`, `[watch]`, `[contextual]`) is honoured
  unchanged. Three fields are restricted:

  | Field | From an untrusted config |
  | --- | --- |
  | `fastembed_cache_dir` | ignored with a warning, standard cache used |
  | `[transport.http]` | non-loopback bind keeps its port and moves to `127.0.0.1`; `allowed_hosts` / `healthz_public` dropped |
  | `kb_path` | **ignored with a warning** for filesystem roots, the home directory, its ancestors, and ancestors of the config's own directory — `--kb-path` still overrides, and with neither the command stops as usual |

  The `kb_path` rule bounds rather than confines — `./docs` and
  `/srv/kb/knowledge-base` still work, so the shipped `personal` recipe (a
  project-root toml naming an absolute path) is untouched.

  No rule aborts start-up. A refusal would kill the Windows daemon with no
  output at all (`kb-mcp-svc` spawns it with stdio set to null), and would let
  an unused config value fail a command that never needed it — `kb-mcp validate
  --kb-path /safe` should not care what a nearby config says. Dropping the
  value keeps the dangerous input out either way.

  Separately, the model directory is now never working-directory-relative:
  `resolve_cache_dir`'s last fallback used to be `./.fastembed_cache`, so a
  checkout with a planted cache could supply model bytes even with no config
  file at all. `FASTEMBED_CACHE_DIR` must now be a non-empty **absolute** path
  for the same reason — an empty value and a relative one both resolve against
  the working directory. Where no absolute directory can be determined, embedding
  commands stop with a message naming the variable; commands that load no model
  are unaffected.

  **Compatibility.** Installed services are unaffected: all three backends set
  their working directory to a config home and start `serve` without
  `--config`, so config homes are trust roots — verified against a live
  installation, which reports `trust=Trusted` with no warnings. To accept a
  discovered config in full, name it with `--config`.

  **Not covered**: a repository that ships its own `.mcp.json` controls the
  whole command line, not just the config file. No rule inside kb-mcp can help
  there.

  The config log line now also carries the resolved path and the trust
  decision; `source=Cwd` alone could not tell you which file had won.

- **The macOS LaunchAgent's logs are no longer world-readable** (BU-24). The
  plist named `kb-mcp.out` / `kb-mcp.err` but set no `Umask`, and launchd —
  which creates those files itself, before `exec`, so the daemon's own umask
  comes too late — fell back to the user domain's 022 and made them 0644.
  `<key>Umask</key><string>0077</string>` fixes that at the source; the value is
  a string because launchd.plist(5) reads an `<integer>` as **decimal**, so
  `0077` there would mean 77.

  What was exposed was modest, which is why this is defense in depth rather
  than a fix for a leak: `kb-mcp.out` is always empty (nothing in `serve`
  writes to stdout), `kb-mcp.err` carries paths, the bind address and
  re-indexing lines but no queries, documents or results, and the install has
  chmodded the enclosing config home to 0700 since the backend shipped in
  v0.10.0, so another account could not reach the files anyway.

  `Umask` applies to the whole job, so the index database the agent creates
  becomes 0600 too. And because it only applies at creation, `service install
  --force` now also tightens `kb-mcp.out` / `kb-mcp.err` if they already exist
  — upgrading an agent installed before this release does not re-create them.

### Added

- **CI now measures retrieval quality, not just correctness** (BU-11). Nothing
  told us when a change made search *worse*: the recall drop feature-48
  introduced was found by hand, on a private knowledge base, after release.
  `tests/fixtures/kb-eval/` adds 20 committed documents (9 Japanese, 11
  English, 60 chunks) and `tests/fixtures/kb-eval-golden.yml` 25 golden
  queries, run through `kb-mcp eval` by `tests/eval_corpus_quality.rs` on the
  nightly leg.

  The queries are paraphrases that avoid each document's own headings and
  distinctive nouns, so a golden built from verbatim substrings cannot pass on
  keyword overlap alone. Five deliberately lexical queries (an error number, a
  header name, a path, a literal prefix, a clock time) sit alongside them.

  Thresholds come from measurement, including of the failure the gate exists to
  catch — `build_fts_query` forced to return `None` in a scratch build:

  | | recall@1 | recall@5 | MRR |
  | --- | --- | --- | --- |
  | BGE-small, as shipped | 0.92 | 0.96 | 0.940 |
  | BGE-small, FTS leg silent | 0.80 | 0.88 | 0.835 |
  | BGE-M3, as shipped | 1.00 | 1.00 | 1.000 |
  | BGE-M3, FTS leg silent | 1.00 | 1.00 | 1.000 |

  That last row is the reason there are two gates rather than one. BGE-M3
  answers every query with the keyword half of the hybrid search removed
  entirely — 20 semantically distinct documents are separable by the vector leg
  alone — so it cannot detect an FTS regression at this corpus size, and its
  gate guards the Japanese semantic path instead. The BGE-small gate is the
  sensitive one: with the FTS leg silent, four queries degrade, three of them
  Japanese natural-language ones, which is the feature-48 class exactly. It
  needs only the ~130 MB model and runs on both nightly legs; the BGE-M3 gate
  joins the two existing Windows skips.

  Floors allow two queries of drift and trip on the third (BGE-small recall@1
  ≥ 0.84 / MRR ≥ 0.88; BGE-M3 ≥ 0.92 / ≥ 0.95) — enough slack for `f32` fusion
  ties to resolve differently on another architecture, while still sitting
  above the broken state. recall@5 is reported but not asserted: healthy and
  FTS-dead are only two queries apart there, so no threshold separates them.
  A failure names every query that lost rank 1, what it expected, and what won
  instead, so a nightly failure is diagnosable from the log without re-running
  a 2.3 GB model.

  A third test needs no model and runs in the PR gate: it checks that the
  corpus, its manifest, and the golden still describe the same documents, that
  every document is some query's expected answer, and that both languages are
  still represented. A renamed fixture surfaces there by name instead of a day
  later as an unexplained recall drop.

- **The benches are now executed by CI, not only compiled** (BU-25).
  `cargo clippy --all-targets` builds them on every PR — twice, so the
  `heavy-bench` halves are covered too — but nothing had ever run one.
  Compiling is the weaker check: AU-56 found `search_latency` timing a fixture
  that was never indexed, so every iteration measured the cost of starting the
  binary and finding zero hits, and criterion reported that as a benchmark
  result for as long as it went unnoticed.

  The nightly job now runs each bench target once through criterion's test
  mode, on both OS legs. It is a liveness check, not a performance gate:
  wall-clock on a shared runner is too noisy to threshold at a sample size
  worth paying for, and criterion exits 0 even when `--baseline` reports a
  regression, so gating would take an external script over its
  `estimates.json`. Thresholded performance guards stay in the test suite,
  where `bu03_or_expansion_stays_within_a_small_multiple_of_a_single_phrase`
  already bounds feature-48's FTS cost at 20× a single-phrase query and has
  been running nightly on both legs since it was added.

  The targets are named one by one, since `--benches` would also re-run the
  entire unit-test suite; `tests/bench_targets_run_in_ci.rs` fails if that list
  and the `[[bench]]` entries drift apart in either direction. `heavy-bench`
  bodies remain compile-only — running them needs the ~2.3 GB reranker the
  Windows leg deliberately does not download.

- **A module can no longer arrive with no tests and nobody notice** (BU-26).
  The nightly coverage job printed a per-file table and stopped there; nothing
  ever failed, so noticing required reading a table in a job nobody opens when
  it is green.

  The gate is a **per-file** floor (`--fail-under-file-lines 35`), not a global
  one. The total is not a number that can be thresholded honestly here: 53.8%
  of the physical lines under `kb-mcp/src` are in-file `#[cfg(test)] mod tests`
  and ~100% covered by construction, which pulls it up; code reachable only
  from `#[ignore]` tests reads as 0% in this job, which pulls it down; and
  ~3,400 lines of Windows/macOS-only code are not in the ubuntu leg's
  denominator at all. A global floor is also satisfiable by adding tests to a
  module that already has plenty, which is not the failure being guarded
  against.

  35 is a tripwire for "no tests at all", not a quality target — the measured
  distribution runs `main.rs` 41.43%, `embedder.rs` 58.90%, `server.rs` 63.16%,
  `watcher.rs` 65.57%, everything else ≥ 78%, so a higher floor would mean
  either more exclusions or no headroom. Three files are excluded because they
  are structurally invisible to a non-ignored run rather than untested: the
  stdio transport and the systemd backend are driven only by `#[ignore]` tests,
  and the tray's `main.rs` is a three-line Windows entry point.

  cargo-llvm-cov exits 1 without saying which file tripped the floor, so the
  step names it: every file below the floor becomes a GitHub error annotation
  carrying its percentage. The summary table also survives a failing run now,
  and the tests are run once and reported twice (`--no-report` plus two
  `report` calls) instead of being run again for the gate.

### Changed

- **rmcp 1.4.0 → 3.1.2, which brings MCP 2026-07-28 support**. Two majors of
  upstream work, including the session-leak fix described under Security and
  OAuth hardening. kb-mcp's own surface is small — the `#[tool]` macros,
  `Parameters`, `ToolRouter`, `serve_server`, stdio, and the Streamable HTTP
  service — and none of it needed changing to compile.

  What did need changing is a consequence of the protocol, not the API.
  **MCP 2026-07-28 removes sessions entirely (SEP-2567)**: there is no
  `Mcp-Session-Id`, no `initialize`, no standalone GET stream and no
  DELETE-based termination. Each request carries the negotiated version and the
  client's capabilities in `_meta`, and rmcp serves it with a fresh handler.
  rmcp's `stateful_mode` is renamed `legacy_session_mode` and now governs only
  older protocol versions.

  For kb-mcp that means a 2026-07-28 client POSTs `tools/call` to `/mcp`
  **without a session**, which the BU-32 gate — written when a session-less POST
  could only legitimately be an `initialize` — answered with `422`. Measured
  against 3.1.2 before fixing: the same request with the gate bypassed returns a
  complete `tools/list` result, so the gate was the only thing in the way. It
  now inspects a request only to decide whether the session cap applies, and
  passes everything else through untouched. A regression test drives a
  2026-07-28-shaped request through the mount, because the whole suite is
  otherwise a legacy handshake and it stayed green through the breakage.

  Nothing changes for existing clients: the legacy lifecycle still works, and
  the shipped defaults (`legacy_session_mode` on, Origin validation off) match
  the previous behaviour.

- **`get_connection_graph` / `kb-mcp graph` are now bounded, and say so when a
  bound bites** (BU-33). The walk had no upper limit on its cost: it seeded
  from *every* chunk of the start document, so clamping `depth` to 3 and
  `fan_out` to 20 never bounded a request. On a 650-document knowledge base
  (9,419 chunks, BGE-M3) the largest document — 160 chunks — measured, with the
  release binary:

  | `depth` | before | after (defaults) |
  | --- | --- | --- |
  | 1 | 160 KNN / 767 nodes / ~19 s | 14 KNN / 100 nodes / ~1.1 s |
  | 2 (default) | 767 KNN / 1997 nodes / ~87 s | 14 KNN / 100 nodes / ~1.1 s |
  | 3 | 1997 KNN / 3682 nodes / ~200 s | 14 KNN / 100 nodes / ~1.1 s |

  The call holds the database mutex throughout, so those runs delayed every
  concurrent search; a 1997-node result was also unusable as LLM context. Nor
  was this only about outsized documents — the *median* document (13 chunks)
  returned 331 nodes in 7.3 s at the default depth.

  Two bounds, both deterministic, exposed on the MCP tool and the CLI:
  `max_seed_chunks` (default 32, ceiling 1000) applied as a SQL `LIMIT` so rows
  past the cap are not read — bar one probe row, which is how truncation is
  detected without a second query — and `max_nodes` (default 100, ceiling
  2000), which caps the response size and the query count together because
  each node is queued once and expands at most once
  (`knn_queries <= total_nodes <= max_nodes`). Over-large values are clamped,
  not rejected — the same doctrine as `depth` / `fan_out` / `limit`.

  A `LIMIT` alone would not have bounded the database's work: without an index
  on `(document_id, chunk_index)`, SQLite scanned every chunk and sorted the
  matches before returning the first `cap + 1` rows (`EXPLAIN QUERY PLAN`:
  `SCAN c` + `USE TEMP B-TREE FOR ORDER BY`). The index is now created on open,
  idempotently, for new and existing databases alike — 17 ms to build on a
  9,419-chunk index, no measurable size change, and the seed read drops from
  8.00 ms to 0.22 ms while becoming proportional to the cap rather than to the
  size of the knowledge base. A test asserts the query plan rather than the
  clock.

  Both bounds are needed. A node budget alone degenerates: BFS emits every seed
  before any neighbour, so on that 160-chunk document any budget of 160 or less
  returned a connection graph with **zero connections** (at exactly 160 the
  seeds fit and the first neighbour is the one refused).

  Truncation is reported in band — `truncated: bool` at the root of the
  response plus a `truncation` array carrying `reason` (`seed_chunks` /
  `node_budget`), the `limit` that fired, and the remedy for that specific
  reason, since MCP offers no cursor with which to ask for the rest.
  `truncated` means *something was lost*, not *a counter reached its cap*: a
  walk that exhausts the graph while exactly filling the budget reports
  `false`. `stats` gains `seeds_used`, and the CLI text output gains the same
  fields plus one `!` line per reason.

  Defaults come from measurement: ~72 ms per KNN, ~4 ms per node, ~665 B of
  JSON per node, and a chunks-per-document distribution of median 13 / p90 26 /
  p99 43 / max 160. So 32 seeds trims 4.0% of documents, and 100 nodes bounds a
  request at `100 × 72 ms + 100 × 4 ms` = ~7.6 s / ~65 KiB. The measured runs
  land well under that bound (1.1 s) because the budget fills partway through
  the seed expansion, at 14 KNN rather than 100.

  **Callers who want the old behaviour** can ask for it:
  `--max-seed-chunks 1000 --max-nodes 2000` reproduces the depth-1 and depth-2
  rows exactly, with `truncated: false`. That holds for any walk that stayed
  within both ceilings — a document of at most 1000 chunks whose graph came to
  at most 2000 nodes; larger walks are truncated, and say so. Two things are no longer reachable by anyone: exhaustive seeding of
  documents larger than 1000 chunks, and results larger than 2000 nodes — the
  depth-3 row above is 3,682 nodes, so at the ceiling it returns 2,000 nodes in
  ~59 s with `truncated: true`.

  Because BFS spends the budget breadth-first, raising `depth` alone no longer
  changes the result for a long start document; `seed_strategy: "centroid"` is
  the way to spend the budget on depth (a depth-2 graph of the same document:
  24 nodes in ~0.4 s). Note that `max_seed_chunks` bounds the *read*, so
  `centroid` averages the same capped prefix — it frees the node budget, it
  does not recover chunks the seed cap dropped.

### Fixed

- **Hard links are now refused wherever symlinks already were** (BU-20). kb-mcp
  refuses symlinks everywhere a file enters the index or leaves it as content —
  the full index, the watcher, `get_document` — because whoever can write into
  the knowledge base should not be able to make kb-mcp read a file *they* cannot
  read and hand it back through `search`. A hard link does exactly that while
  passing every one of those checks: it is not a symlink, it is a regular file,
  and it canonicalizes to a path inside the knowledge base, because a hard link
  has no target to follow. Creating one needs no read access to the file, and on
  Windows no privilege at all — measured on Windows 11 as a non-administrator,
  the link was created, indexed, and its content came back in a search hit.

  A file with **more than one name** is now refused in all three places, with a
  log line naming it and saying why. The check is deliberately blunt: nothing
  portable can say whether the other name is inside the knowledge base or
  outside it, so a legitimately hard-linked note — deduplicated, or shared
  between two knowledge bases — is skipped as well, from either of its names.
  Replace it with a copy if it belongs in the index. A file whose link count
  cannot be read (it was just deleted, say) is allowed through, so deletions
  still reach the index.

  On Windows the count comes from `GetFileInformationByHandle`, because
  `MetadataExt::number_of_links` is still unstable and `walkdir`'s metadata —
  `WIN32_FIND_DATAW` — carries no link count at all.

  **This raises the bar rather than drawing a boundary**, and the distinction
  matters: the check looks at a name, at a moment, and is not bound to the bytes
  later read from that name. Whoever can link a file in *and* remove its original
  name — write access to the directory holding it, not read access to the file —
  leaves the knowledge base path as the only name, count 1, indistinguishable
  from a file that was always there. Separately, a knowledge base writer who can
  time a rebuild can let the check see an ordinary file and put a hard link in
  its place before the parser opens it. Remembering inodes seen above 1 closes
  neither, since both steps of the first can happen between two index runs; the
  second needs the count bound to the same handle the content is read from, which
  the parser interface does not currently allow. The check that would close them
  is ownership, and refusing files not owned by the user running kb-mcp would
  break a knowledge base shared between accounts. So the rule stands: anything
  that must not be readable by kb-mcp belongs outside `kb_path`, on a path its
  user cannot read.

- **A single panic no longer disables search for the life of the process**
  (BU-18). Every mutex in the server was taken with `.lock().unwrap()`. A
  `std::sync::Mutex` is *poisoned* when a thread panics while holding it, and
  every later `lock()` returns `Err` — so one panic under any of them turned
  every subsequent `search`, `list_topics`, `rebuild_index` and
  `get_connection_graph` into a panic of its own. The daemon stays up and
  answers "internal error" indefinitely, with the panic that caused it long
  gone from the log.

  Locks now recover the state the panic left behind, and what that means
  depends on the payload:

  - **Plain data** (the indexing state) is taken as is. The guard used to
    *skip* its decrement on a poisoned lock, which pins `/api/admin/status` at
    `indexing.active=true` for good.
  - **The database** is checked first. Unwinding past an open transaction
    normally runs the `Drop` that rolls it back; what is left is the case where
    that rollback *failed*, which rusqlite swallows. Then the transaction is
    still open, every later write joins it silently, and nobody commits it. The
    recovery asks the connection and rolls back explicitly, saying so in the
    log.
  - **The embedder and reranker** are recovered as a stated bet: neither
    fastembed nor ONNX Runtime offers a way to ask a session whether it is
    still consistent, so "it is fine" cannot be proven from here. Inference
    does not mutate the session, and the alternative — dropping it and
    reloading 130 MB–2.3 GB of model — is a large cost on a path that has never
    fired. The reasoning is written down at the recovery, not implied by it.

  The watcher took the same locks and *skipped* the reindex on poison; it now
  follows the same rule. `kb_info`'s `try_lock` no longer reads a poisoned lock
  as contention, which had admin status reporting `documents: null` forever and
  looking like a busy rebuild.

  The first recovery in a process logs a warning naming the mutex; later ones
  are debug. Poisoning is sticky, so a warning per recovery would repeat on
  every request for the rest of the process's life.

  Reachability is low today — the code under those locks propagates with `?`
  and has no `unwrap` — which is the point. This is about the panic someone
  adds later.

- `kb-mcp graph --seed-strategy`'s help text advertised `all_chunks`, but clap
  derives kebab-case values, so only `all-chunks` was accepted and copying the
  help text produced `error: invalid value`. The help now matches what the flag
  takes, and notes that the MCP tool spells it `all_chunks`.

## [0.18.0] - 2026-08-13

### Added

- **Line endings are pinned to LF by `.gitattributes`** (`* text=auto eol=lf`).
  Committed content was already LF everywhere — all 96 tracked `.rs` files —
  but nothing kept a Windows checkout, or a scripted edit that rewrites a whole
  file, from handing back CRLF. This repository has paid for that twice: once
  as `chore: restore LF line endings`, and again while preparing this release,
  where a test-only change to `db/fts_query.rs` silently carried a CRLF → LF
  conversion of the entire file and turned a 134-line diff into one of over
  1900. Adding the rule changes no existing file (`git add --renormalize .`
  touches nothing), so it is purely a guard against recurrence; the binary
  fixture declarations that follow it still override it, since gitattributes
  resolves with the last matching pattern.

- **The query tokenizer's accepted roughness and its trigram floor are now
  pinned by tests** (BU-27, BU-28). No behaviour changes; both were documented
  only in prose, which meant a future change could alter either one and nothing
  would say whether the new behaviour was intended.

  `fts_query`'s module doc lists four places where the character-class split is
  knowingly coarse — CJK beyond the basic ranges, no Unicode normalization, the
  asymmetry between the full-width and half-width middle dot, and punctuation-
  only runs becoming phrases. Four `accepted_roughness_*` tests now record the
  current answers, so a change there shows up as a decision rather than a
  surprise. Each was confirmed to fail against a one-range edit to `classify`.

  Writing them corrected the documentation twice. `𠮷野家` does split into two
  runs as documented, but the short-run merge puts it back together, so the
  phrase output is unchanged — the split only becomes visible when both sides
  are long enough, as in `𠮷野家具店` → `["𠮷野家具店", "野家具店"]`. The doc now
  says which is which.

  `MIN_PHRASE_CHARS = 3` is now justified against SQLite rather than against
  itself: a test inserts into a real FTS5 trigram table and checks that a
  two-character phrase matches nothing while a three-character one matches.
  Every other test in that module assumes the floor is 3, so all of them would
  keep passing if the tokenizer were swapped for one with a different floor —
  they would agree with each other while silently disagreeing with SQLite.
  Swapping `trigram` for `unicode61` in the schema kills the new test and
  nothing else.

### Changed

- **`match_spans` now has a contract, and it changes what clients receive**
  (BU-09, BU-10). Two things feature-48 left undefined are now decided and
  pinned by tests.

  *Overlaps are folded away.* Since v0.16.0 the terms come from `query_phrases`,
  which emits nested phrases, so `"Foundry Local" Foundry` returned `(0,7)` and
  `(0,13)` — two spans over the same text, and a highlighter had to guess. They
  are now merged into their union. The merge predicate is strict, so spans that
  merely touch stay separate; making it non-strict collapses the 100 adjacent
  spans of the existing cap test into one while `len() <= 100` still passes,
  which would leave that test green and meaningless.

  *The span budget is shared across terms.* It used to be spent in
  phrase-generation order, so a term matching hundreds of times consumed all
  100 and a rare term you also searched for was highlighted nowhere — and which
  terms won depended on an internal ordering feature-48 had just changed. Each
  term now gets `floor(100 / k)` spans (at least one) taken in document order.
  Reordering the words of a query now returns the identical array. The leftover
  budget is deliberately not redistributed, because handing it out in term
  order would reintroduce the order dependence; with 32 terms that means 96
  spans rather than 100.

  Every response now satisfies: sorted, disjoint, non-empty, at most 100 spans,
  independent of term order, and covering every term that occurs. All six are
  asserted, and each was confirmed to fail against a reverted fix.

  Order-independence has one documented boundary. `query_phrases` caps the
  phrase list at 32 *in query order*, so reordering a query that exceeds the
  cap changes which fragments the full-text search looks for at all — that is
  search behaviour, not highlighting, and it is out of scope here. The
  100-term limit on the whitespace-fallback path had the same problem and is
  fixed: that list is sorted and deduplicated before it is truncated, so the
  cutoff no longer follows word order. Deduplication applies the same ASCII
  case fold that matching does, so `Rust rust` is one term rather than two
  splitting a budget between identical searches.

  The alternative — collect every occurrence, then keep the 100 best by
  occurrence rank — was measured and rejected: **100–450× slower** (157 µs →
  33.1 ms for 32 dense phrases over a 256 KiB chunk; with `limit` up to 1000
  that is 33 s per search), and its correctness rests on an early-exit
  condition that a deliberately off-by-one version survived 24,000 randomized
  cases undetected. The shipped approach measures 1.0–1.2× on realistic 4–16
  KiB chunks and ~2–3× on that pathological input.

  A term clamp (`MATCH_SPAN_MAX_TERMS = 100`) was added for the
  whitespace-fallback path. `query_phrases` caps phrases at 32 but does not
  apply the whole-query fallback — that belongs to `build_fts_query` — so a
  query whose fragments are all below the trigram floor produced an unbounded
  term list. With a per-term budget of at least one, 150 terms meant 150 spans;
  the clamp is what keeps the published cap true there.

### Fixed

- **`get_connection_graph`'s `exclude_paths` is now bounded like every other
  caller-supplied list** (BU-05). AU-17 limited `search`'s `path_globs` /
  `tags_any` / `tags_all` to 64 entries of at most 1 KiB; `exclude_paths` was
  missed and went straight into the `HashSet` the BFS consults on every visit.
  The check lives in `build_connection_graph` rather than the MCP handler, so
  `kb-mcp graph --exclude` is bounded by the same rule, and it runs before the
  seed lookup so an oversized request costs nothing.

  Measuring this turned up something the ledger had only hypothesised. The
  graph expands from **every chunk** of the start document, and on the
  650-document dogfood knowledge base the largest document (160 chunks) takes
  **59 s at the default `depth = 2`** and **148 s at `depth = 3`** — holding
  the database lock throughout. That cost is now documented in the README
  alongside the caps; bounding it is tracked separately.

- **Directory exclusion is case-insensitive, in all three places that decide
  it** (BU-19). `exclude_dirs` and the hardcoded `.git` / `.svn` /
  `node_modules` fail-safe both compared basenames exactly. On Windows and
  macOS `Build` and `build` are one directory, so the exclusion could be
  bypassed by however the directory happened to be capitalised — and the
  fail-safe would index a `.GIT` directory. On Linux the two really are
  distinct, and skipping both is the safer side to err on for a denylist.

  The decision now lives in one function, `indexer::is_user_excluded_dir`,
  used by the index walk, the `validate` walk and the live watcher. Those
  three have drifted apart before — AU-03 found the watcher missing the
  hardcoded denylist the other two applied — and they drifted again inside
  this change: the first version switched only the two walkers, which left the
  watcher incrementally indexing a `Build/` that the full index skipped, a
  state worse than before the fix.

  While documenting this, the README's claim that `exclude_dirs = []` "walks
  everything, including `.git/`" turned out to be false: the hardcoded denylist
  has applied regardless since v0.7.5 (F-62).

- **A dual-stack bind no longer locks the tray out of the admin endpoints**
  (BU-21). `Ipv6Addr::is_loopback` recognises only `::1`, but a listener on
  `[::]:3100` reports an IPv4 loopback client as `::ffff:127.0.0.1`, so the
  admin router answered 403 to a process on the same machine. The IPv4-mapped
  form is now unwrapped before the question is asked — and only that form, so
  a mapped address outside `127.0.0.0/8` still counts as remote.

- **`get_document`'s size cap follows the canonical extension** (BU-22). The
  cap class was chosen by the caller from the path as typed, while the
  registry-membership check used the canonicalized path — two decisions from
  two different strings. Windows 8.3 aliasing makes them disagree for every
  Office format: `presentation-deck.pptx` is also reachable as
  `PRESEN~1.PPT` (measured on a development machine), and since `.ppt` is not
  a registered extension the 1 MiB text cap was applied to a file the registry
  classifies as binary, rejecting Office documents over 1 MiB as "too large".
  Both caps are now passed in and the choice is made where the extension is
  already known.

- **`get_best_practice` no longer returns the configured template paths**
  (BU-23). A total miss echoed every candidate path back to the caller, which
  is the server's `[best_practice].path_templates` rendered out — directory
  names an unauthenticated MCP client has no other way to learn. The reply now
  carries the number of templates tried, which is still enough to tell "no
  template matched" from "the tool is not configured"; the paths themselves go
  to the operator's log at `RUST_LOG=kb_mcp=debug`.

- **The bundled `kb-mcp.toml.example` no longer changes behaviour just by being
  copied** (BU-13, BU-14). It shipped with `[transport] kind = "http"`,
  `[parsers].enabled`, `[best_practice].path_templates` and others *active*, so
  copying the template to `kb-mcp.toml` silently switched the server from stdio
  to a listening socket, changed which extensions get indexed, and enabled an
  opt-in MCP tool. Every value the file now leaves active is already the
  built-in default, and anything that would alter behaviour is commented out,
  so a fresh copy is inert until you opt in. `the_example_as_shipped_changes_no_behaviour`
  parses the file exactly as shipped and asserts that, because the difference
  is invisible when reading it — the file looks like documentation either way.
  The file is also in English now (it was Japanese-only,
  against the English-primary policy), no longer contains a personal path or an
  internal issue id, and describes all four config-discovery tiers rather than
  just the last one. README gained the two keys it never mentioned —
  `[best_practice].path_templates` and `[transport.http].healthz_public` — and
  now says plainly that its config block is an illustration, not a file to
  paste.

- **Deployment recipes named config keys that do not exist** (BU-15). Four
  places told you to set `FASTEMBED_CACHE_DIR` "in `kb-mcp.toml`". The key is
  `fastembed_cache_dir`; because unknown keys are rejected, following the
  documentation produced a startup error. (`FASTEMBED_CACHE_DIR` is still
  correct as a real environment variable, which overrides the file.) The same
  page also pointed at "each scenario's `kb-mcp.toml`" when nas-shared ships
  `.client` / `.indexer` variants instead, and estimated a container image at
  "~10 MB" — that is the compressed tarball; the extracted binary an image
  layer actually carries is several times larger.

- **`CONTRIBUTING` understated both what CI runs and what `--ignored` costs**
  (BU-16). CI runs clippy **twice** — the second time with
  `--features test-helpers,heavy-bench` — and runs `index_progress_cli` with
  `--test-threads=1`, so the documented local command could pass while CI
  failed. And `#[ignore]` was described as "needs a model download" when some
  ignored tests register a real Windows scheduled task and write into the
  Startup folder; that cost is now spelled out, along with what to check if a
  run is killed partway. The repository layout also still listed `db.rs` and
  `tune.rs` as single files after the v0.15.0 split, and omitted
  `test_support.rs`.

- **Documentation that described the code inaccurately** (BU-08, BU-12, BU-29,
  BU-30). `validate_get_document_path` claimed to block "bypass into
  excluded_dirs", but it never receives `exclude_dirs`: a `.md` file under an
  excluded directory is not indexed yet remains readable through
  `get_document`. That is the intended contract — anything under `kb_path` is
  readable — and `document_in_excluded_dir_is_still_readable` now pins it.
  `docs/ARCHITECTURE` prose still pointed at `db.rs` for code that moved to
  `db/schema.rs` and `db/search.rs` in v0.15.0. `README` described
  `--rerank-by-default` as a bare flag when it takes a boolean, and summarised
  `kb-mcp status` as "document and chunk counts" when it prints five things, on
  stderr.

  CHANGELOG dates had drifted from their tags in **seven** entries, not the two
  the audit found. The convention turned out to be the tag date in the
  maintainer's local timezone (31 of 38 entries), not UTC as previously
  believed — a belief that had itself introduced one of the seven. All seven
  are corrected and the rule is now stated at the top of this file.

- **A busy MCP tool call no longer takes the whole HTTP server down with it**
  (BU-06). Every tool handler was an `async fn` that then did its work
  synchronously — embedding inference, SQLite queries, a full index rebuild —
  on a tokio worker thread. Since the runtime has one worker per core, that
  many concurrent calls left nothing to serve anything else: `/healthz`,
  `/api/admin/status` and every other request simply waited. Measured on a
  16-core box, 16 concurrent blocking calls stalled `/healthz` for 602 ms; on a
  single-worker runtime one call stalled it for 651 ms, versus 0.9 ms once the
  work moved off. Handler bodies now run on tokio's blocking pool, and the
  server state they need lives in a new internal `KbCore`.

  Worth recording because the obvious remedy does not work: a request timeout
  cannot fire against a handler that owns its thread. `tower`'s `Timeout` polls
  the inner future first and the deadline only afterwards, so while the inner
  future never yields the deadline is never checked — a 200 ms deadline over an
  800 ms thread-blocking body returns success at 800 ms. The same deadline over
  an offloaded body elapses at 208 ms. Offloading is the change that makes
  timeouts, concurrency limits and load shedding possible at all; those remain
  unimplemented.

  A panic inside a tool body is now reported to the caller as the usual error
  JSON instead of unwinding through the request task.

  Session count is still unbounded. rmcp 1.4's `StreamableHttpServerConfig` and
  `LocalSessionManager` expose no cap, so bounding it needs a custom session
  manager — tracked separately.

## [0.17.0] - 2026-08-13

### Added

- **[ADR-0002](docs/decisions/0002-compile-queries-into-per-token-fts-phrases.md)
  records why queries are compiled into per-token `OR` phrases**
  ([日本語](docs/decisions/0002-compile-queries-into-per-token-fts-phrases.ja.md)).
  The v0.16.0 change met all three conditions in ADR-0000: alternatives were
  compared (a morphological analyser was weighed and deferred), reversing it is
  expensive (`fts_query_version` makes evaluation history incomparable across
  the boundary), and it altered an interface — `"..."` in a query now means
  something. The rationale it absorbs has been trimmed from the CHANGELOG entry
  and the `fts_query` module documentation, which now summarise and link.

- **The cost of the full-text half is now measured, documented and guarded**
  (BU-03). `ORDER BY bm25(...)` scores every matching row before `LIMIT`
  applies, so the cost tracks how many rows the expression matches, not how
  many you asked for. Measured in the worst case (every phrase matching every
  row): a single-phrase query costs 4.3 / 16.0 / 32.8 ms at 5k / 20k / 40k
  rows, the 32-phrase `OR` costs 46.9 / 171 / 329 ms. Both are linear in the
  matching population; the **~10×** multiple between them is flat across corpus
  sizes, and cost grows roughly linearly with phrase count.

  Three things follow, all now in `docs/retrieval-pipeline`. Lowering a limit
  does **not** reduce this cost (339 ms at `LIMIT 1` vs 329 ms at `LIMIT 100`
  on 40k rows), so the over-fetch cap is left alone. Matching every row was
  always one common substring away (`"について"` does it with a single phrase),
  so per-token compilation did not raise the ceiling on rows touched — but it
  did raise the ceiling on cost by roughly 10×. And the knob that would bound
  the worst case is the phrase cap, not any limit; it stays where feature-48's
  retrieval evaluation measured it until the recall cost of lowering it has
  been measured too.

  The regression guard pins the *multiple* rather than an absolute timing,
  alongside an always-on test that the `OR` stays a union executed as one
  statement — that one counts statements traced out of SQLite, not calls into
  the Rust method that issues them.

### Changed (breaking)

- **`kb-mcp serve --bind <non-loopback>` now requires `--i-know`** (BU-01).
  `kb-mcp service install` has always refused a non-loopback bind without that
  flag, but `serve` accepted one with a single warning line — so the same
  exposure was one typo away on the command line. kb-mcp ships no
  authentication, which makes the bind address the only access control, so the
  two commands now agree.

  The gate covers the `--bind` flag only. A non-loopback address coming from
  `[transport.http].bind` in `kb-mcp.toml` still starts, because the published
  `intranet-http` recipe runs `kb-mcp serve` with no arguments. Note that such
  a bind is not universally warned about either: the startup warning fires only
  when the Host allow-list is missing or empty, so the documented intranet
  shape — a non-loopback bind plus an explicit `allowed_hosts` — remains silent
  by design, on the grounds that writing that list states the intent.

### Fixed

- **A query that exceeds the phrase cap now says so** (BU-31). Past 32 distinct
  phrases the trailing ones are dropped, so the search still succeeds and
  simply looks for less than was asked — a silent recall loss, logged at
  `debug` where nobody would see it. It is a `warn` now, naming how many
  phrases were dropped.

  The cap itself stays at 32. Measured across 37 golden queries, the largest
  produced 9 phrases, so the cap does not bind on real queries; halving it
  would halve the worst-case full-text cost (BU-03) and equally halve the query
  length at which genuine truncation begins. Given the choice between a visible
  bounded cost and a silent quality loss, the cost was kept. A test pins that
  realistic queries retain at least 2× headroom, so a future change that makes
  ordinary queries approach the cap fails rather than quietly truncating.

- **The hybrid search now has a test that fails when the full-text half stops
  contributing** (BU-04). Every existing fusion test gave the FTS-matching
  chunk the same embedding as the query, so the vector half alone put it first
  and the assertion held whether or not FTS returned anything. Measured: with
  `build_fts_query` stubbed to return `None` for *every* query,
  `test_search_hybrid_japanese_trigram` still passes. That is why the defect
  fixed in 0.16.0 survived fifteen releases.

  The new test inverts the layout — the FTS-matching chunk is the *farther* one
  and a decoy sits exactly on the query vector — so the top rank flips the
  moment the full-text half goes quiet.

- **Text files are now size-capped at index time** (BU-02). The 50 MiB raw-byte
  guard applied only to binary formats; `binary_size_exceeded` returned "fine"
  for anything else without even calling `stat`. A single oversized `.md` under
  `--kb-path` was therefore read into memory in full — and `rebuild_index` is
  an MCP tool, so any client could trigger that read on demand. Text now has
  its own cap (`MAX_RAW_TEXT_BYTES`, same 50 MiB, since the constraint is
  identical: one whole file in memory), enforced on all three paths that used
  the binary guard (full rebuild, watcher re-index, watcher rename). The skip
  message names which limit applied.

- **The most exposed HTTP configuration no longer starts silently** (BU-01).
  The startup warning for a non-loopback bind fired only when
  `[transport.http].allowed_hosts` was absent. Setting `allowed_hosts = []`
  suppressed it — yet an empty list makes rmcp accept *every* `Host` header
  (`host_is_allowed` returns early on an empty list), so `0.0.0.0` plus an
  empty list was both the widest-open shape and the only silent one. It now
  warns, with a message naming what is actually disabled.

  The warning also no longer implies that Host validation is a form of access
  control: any peer that can reach the port can send `Host: localhost`. It is a
  DNS-rebinding defence for browsers, not authentication. `README` says the
  same in both languages.

## [0.16.0] - 2026-08-12

### Changed

- **The FTS half of the hybrid search now works on natural-language queries**
  (feature-48). The whole query used to be wrapped in one quoted phrase, which
  over a trigram tokenizer is a verbatim substring search, so a sentence-shaped
  Japanese query matched nothing and the hybrid ran on vectors alone. Queries
  are now compiled into per-token phrases joined by `OR`, cut at script
  boundaries: `再ランキングの評価について` becomes
  `"再ランキング" OR "ランキング" OR "の評価" OR "について"`. Why this design and not
  a morphological analyser is recorded in
  [ADR-0002](docs/decisions/0002-compile-queries-into-per-token-fts-phrases.md).

  **This changes search results for every user, which is why it is a minor
  release.** What that means in practice:

  - A `"quoted section"` is kept verbatim, so quoting the whole query
    reproduces the old behaviour on demand. The flip side is that quotes now
    mean what they say: `"a""b"` looks for `a"b` where it used to look for the
    literal `"a""b"`.
  - A fragment shorter than three characters (the trigram floor) is joined to a
    neighbour **within the same separator-free group**; one with no neighbour
    is dropped, so `AI について` searches only for `について`. Quoting a wide
    enough region rescues it; quoting the short word alone does not.
  - A query whose fragments are *all* too short, such as `AI と ML`, falls back
    to the old whole-query phrase, so no query class regresses.
  - **No re-indexing is required.** The index, schema and tokenizer are
    untouched.

  Measured on a 650-document / 9,419-chunk knowledge base (bge-m3, no reranker,
  same index before and after): the golden set went from 16 of 26 queries where
  fusion can act to 26 of 26. MRR 0.955 → 0.962 (main golden) and 0.939 → 0.955
  (binary); recall@10 0.954 → 0.965. recall@5 fell 0.926 → 0.906 and nDCG@5
  0.894 → 0.876, from two queries where a second expected document slid from
  rank five to rank eight; the first hit was as good or better in both.

- **`kb-mcp eval` records `fts_query_version` in its config fingerprint.**
  Query compilation decides what search returns, so a change to it makes older
  runs incomparable in the same way a model or reranker change does. Runs
  recorded before this release read as version 1 and are dropped from the
  comparison instead of being reported as a retrieval regression by
  `--fail-on-regression`. Existing history files stay readable.

- **`match_spans` follows the same splitting as the search itself.** The
  citation offsets returned with each hit used to come from splitting the raw
  query on whitespace. That disagreed with the new quoting syntax — for
  `"Foundry Local"` it looked for the literal terms `"Foundry` and `Local"`,
  found neither, and returned an empty span list while the search itself
  matched correctly. Both sides now use one splitting rule. Two consequences:
  a quoted region highlights as a single span rather than word by word, and
  fragments below the trigram floor no longer highlight on their own, because
  they are not what the full-text half searched for either.

- **`kb-mcp tune` diagnostics changed meaning, not thresholds.** The `docfreq`
  column now counts chunks matching *any* of the query's phrases, so it is an
  upper bound on the document frequency of each individual phrase rather than
  the frequency of one phrase. `CLMP` therefore flags a query worth inspecting
  rather than proving that FTS5 has clamped every phrase's IDF. The report
  legend and the `exit 2` guidance were rewritten to say so.

## [0.15.2] - 2026-08-12

### Changed

- **Japanese CID-keyed PDFs now extract correctly** (AU-70, final act). The
  `oxidize-pdf` pin moves from `=4.1.1` to `=4.3.0`, which carries the fix this
  project reported and authored upstream
  ([bzsanti/oxidizePdf#469](https://github.com/bzsanti/oxidizePdf/issues/469),
  merged as PR #470): `/DescendantFonts` is now read in all four legal
  spellings, so a CID-keyed font with a predefined CMap and no `/ToUnicode` —
  what ReportLab emits — decodes to real text instead of byte-wise mojibake.
  Verified end-to-end: the fixture that v0.15.1 could only *refuse to index*
  now indexes as correct Japanese and is found by search. No kb-mcp test
  changed — the v0.15.1 fixture tests were written as dual-regime assertions
  ("if it is rejected, the rejection must name the decode failure; if it
  indexes, it must be the real text") and moved to the second regime on their
  own. The mojibake gates stay in place as defense-in-depth against decode
  failures from other causes. 4.3.0 also brings upstream extraction
  improvements (Tc/Tw/Ts applied to extraction, a space at TJ-operator
  boundaries, opt-in reading-order reordering — all off by default or
  non-breaking for kb-mcp's extraction path).

## [0.15.1] - 2026-08-10

### Fixed

- **A PDF that decoded to mojibake was indexed silently** (AU-70). A Japanese
  PDF whose CID-keyed font uses a predefined CMap with no `/ToUnicode` came out
  of extraction as its UTF-16BE bytes read one at a time — `第1章 概要` became
  `{, 1zà i…`. Nothing warned: the document was indexed, matched no query it
  should have matched, and consumed embedding time and corpus statistics
  regardless. Worse, mis-decoding turns one character into two, so the garbage
  *cleared* the 50 chars/page density gate (measured 1052 chars/page) while a
  correctly extracted Japanese slide deck (29 chars/page) was dropped — the
  gate was admitting the unusable and rejecting the usable.

  Such text is now detected and the document is skipped with a diagnosis that
  names the decode failure instead of blaming page density. Two complementary
  signals: C1 control codes (U+0080–U+009F) reaching 1% of the extracted
  characters — correctly decoded text never contains them, measured 0.00%
  across six correctly-extracted samples against 3.61–15.59% across four
  mis-decoded ones — and, for the one shape that emits no C1 at all, the
  alternating byte-pair signature of UTF-16BE read one byte at a time.
  Unvoiced-kana-only text has 0x30 for every high byte and low bytes under
  0x80, so it mis-decodes to pure ASCII (`あいうえお…` → `0B0D0F…`, 0.00% C1
  at 407 chars/page, measured on the pinned oxidize-pdf 4.1.1) and would sail
  through the C1 gate; its runs alternate a near-constant **leading** character with
  varied ones — natural words never do, and the mirror orientation
  (alternating identifiers like `1A2A3A`) is not flagged because bytewise
  decoding cannot produce it, and ≥30% of such characters
  rejects the document. Runs too short to judge alone — a label sheet or
  word list splits into 4-char tokens (measured 148 chars/page) — are
  aggregated document-wide and judged as a pool, so fragmentation does not
  reopen the hole. Recovery is not attempted — the crate has already
  collapsed NUL bytes to spaces by then, so the original bytes cannot be
  reconstructed. The gates now live in one function with the ordering as its
  documented contract, since running them the other way round is what produced
  both failures.

  The root cause is upstream in `oxidize-pdf` (4.1.1 through 4.2.2, and `main`):
  `/DescendantFonts` is read only when the CIDFont is written as an indirect
  reference, so a producer that writes it as a direct dictionary — ReportLab
  does, and ISO 32000-1 permits it — leaves `descendant_font` empty, which skips
  the `cid_encoding` branch that already resolves `UniJIS-UCS2-H` correctly.
  Verified by A/B: two PDFs differing only in that one respect decode to
  mojibake and to correct Japanese respectively.

### Changed

- **The PDF limitation notes were corrected against measurement.** README and
  ARCHITECTURE (both languages) said Japanese and other CJK PDFs "largely do not
  work", and that a TrueType-subset Japanese PDF "extracts so little" it trips
  the density threshold. Re-measured 2026-08-10: that form — what Word,
  LibreOffice and Google Docs export — extracts **correctly**, 569 chars/page on
  a dense Japanese report. The earlier figure of 45 chars/page came from a
  two-line test page and was the correct count for it, not evidence of loss.
  The density threshold stays at 50: a scan carrying only digitally-added page
  numbers and a "CONFIDENTIAL" stamp measures 39 chars/page, so lowering it
  would admit exactly what it exists to reject.

## [0.15.0] - 2026-08-10

### Fixed

- **A growing knowledge base was reported as a retrieval regression** (AU-71).
  `kb-mcp eval` decides whether two runs may be compared by comparing
  `ConfigFingerprint`, which describes configuration and nothing else —
  `golden_hash` is a hash of the golden YAML bytes alone. Adding documents to
  the knowledge base therefore left the fingerprint identical: the runs were
  judged compatible, the diff stayed on, and `--fail-on-regression` compared
  them. Rankings shift when the competition grows, and that arrived as a
  retrieval regression with nothing in the output mentioning the corpus at all.
  AU-61 closed the same hole for `[contextual].enabled`; the corpus was the
  remaining uncovered input.

  Each run now records the index it measured — document count, chunk count, and
  a digest over the indexed chunks themselves — and the header reports it,
  naming the change when there is one. A document rewritten in place moves
  neither count, so the digest is what keeps "unchanged" honest. The digest
  covers the chunks rather than the source files deliberately: chunks are what
  the search actually reads, so a rebuild that parses unchanged files
  differently — a changed `exclude_headings`, say — is caught even though every
  file hash held. The three reads share one transaction, because in WAL mode
  separate statements see separate snapshots and a `serve` watcher indexing
  alongside could otherwise produce a record of an index that never existed.

  **This deliberately does not disable the diff.** Putting the corpus into the
  compatibility test would have been the tidier fix and the wrong one: a
  knowledge base normally grows, so every added document would stop the
  comparison, leaving `--fail-on-regression` inert exactly when it is wanted.
  The runs stay comparable and the output says what moved, so a drop can be
  read correctly. When a regression is reported and the corpus also changed,
  the failure message says so, because that is the first thing to suspect.

  `--format json` gains `corpus` and `corpus_changed`; the latter is `null`
  when there is nothing to compare against, kept distinct from `false`. History
  written before this release carries no corpus and is never reported as
  changed. The `--fail-on-regression` help text, which had listed compatibility
  as "model / reranker / k_values / golden_hash" since before `metric_version`,
  `mmr`, `parent_retriever`, `fusion` and `contextual` joined it, is corrected.

- **A PDF that could not be decoded was reported as a scanned image, sending
  users after OCR they do not need.** The under-50-chars-per-page check
  announced "PDF appears to have no text layer (scanned image PDF) — skipping
  (OCR not supported)", asserting a cause it had not established. Measured
  2026-07-28 against oxidize-pdf 4.1.1: a Japanese PDF embedding a TrueType
  subset — what Word, LibreOffice and Google Docs export — extracts about 45
  chars/page and lands in exactly that branch, while `pdfminer.six` reads the
  same file perfectly. The text layer is present and conformant; what is
  missing is the decoding. Anyone following the message would have gone
  looking for OCR when the problem is a CMap.

  The diagnostic now reports what it measured and offers common causes as an
  open list — a PDF that decodes correctly but genuinely carries little text
  per page, such as a cover sheet or a label, reaches this branch too, so any
  closed enumeration would be wrong in the same way the original assertion
  was. **The underlying CJK extraction gap is not fixed** and
  is now stated plainly in both READMEs and both architecture documents: a
  CID-keyed Japanese PDF indexes as mojibake and can never be matched, and a
  TrueType-embedding one is dropped. Japanese PDFs should be considered
  unusable for now.

- **The tray no longer flashes a console window** on every Start / Stop /
  Restart and on `--with-tray` autostart install (AU-66). `kb-mcp-tray.exe` is
  a GUI-subsystem binary and so owns no console; `powershell.exe` is a
  console-subsystem program, so Windows was **allocating a fresh console for
  it** on each call. Redirecting stdout and stderr does not prevent that —
  only the `CREATE_NO_WINDOW` creation flag does. Measured from a
  GUI-subsystem parent with every handle piped, the child's own
  `GetConsoleWindow()` returns non-zero by default and `0` with the flag.

  The same fix already existed on the logon path, where v0.9.1 introduced the
  GUI-subsystem `kb-mcp-svc.exe` to detach-spawn the daemon; the tray's own
  PowerShell calls were never given it.

- **`kb-mcp service install` now says when it could not use the svc launcher**
  (AU-67). It prefers `kb-mcp-svc.exe` for the logon task and falls back to a
  console-visible Action when the sibling is missing — previously without a
  word. `kb-mcp-svc.exe` was not attached to a release at all until v0.14.0,
  so **every** installation from a release archive between v0.9.0 and v0.13.1
  took the fallback: users saw a console window at each logon while the v0.9.1
  fix meant to prevent it appeared to be in place, and nothing pointed at the
  cause. The warning now names the archive to extract and how to redo the
  install.

### Added

- **Architecture Decision Records under [`docs/decisions/`](docs/decisions/).**
  Decisions that compared real alternatives, are expensive to reverse, and
  affect structure, dependencies, interfaces, or non-functional
  characteristics now get one canonical record —
  [MADR](https://adr.github.io/madr/) format, English and Japanese pairs,
  superseded rather than edited.
  [ADR-0000](docs/decisions/0000-record-decisions-as-adrs.md) states the
  process and the threshold; [ADR-0001](docs/decisions/0001-withdraw-xls-legacy-biff-support.md)
  covers the v0.14.0 `.xls` withdrawal.

  This is a consolidation, not an addition: the reasoning behind the `.xls`
  withdrawal had been duplicated across this changelog, both READMEs, and a
  source comment, none of which recorded the options that were rejected. Those
  three now carry a summary and a link.

### Changed

- **`kb-mcp tune` recommends a change less readily: criterion 3 now requires
  the held-out mean gain to exceed 3 x the paired SE, not 2 x** (AU-68). The
  criterion was written to be a one-sided 2 sigma test, which would fire on
  about 2.3% of golden sets that contain nothing to find. It did not: AU-16
  measured `SD({d_j}) / sqrt(N)` at 0.53-0.60 of the true standard error,
  because the leave-one-out folds share training rows, and the resulting gate
  produced an "adopt" verdict on **12.7%** of null golden sets — roughly one
  run in eight, on data with no real winner at all.

  The replacement was picked by sweeping the multiplier against that rate
  rather than by argument. At 3 the null adoption rate falls to 3.4% (N=26)
  and 3.1% (N=12), while the power to detect an edge that is genuinely there
  goes from 99.0% to 95.2% — a 3.7x cut in false adoptions for 3.8 points of
  power. Raising criterion 2 instead was measured and rejected: taking the
  mean-delta floor from 0.02 to 0.04 moves the null rate only to 12.1% while
  halving that same power to 51.9%.

  In practice a `tune` run that previously ended in "adopt" may now end in
  "keep the built-in defaults". That outcome was always the expected one — the
  RRF paper measured ~0.4% relative MAP movement across k in [30, 100] — and
  the verdict now carries closer to the confidence it claims. The sweep is
  `au68_adoption_rate_across_the_two_thresholds` in `tune.rs`; both the
  English and Japanese `docs/eval` pages carry the numbers.

### Internal

- **Retrieval quality of the binary formats is now measured** (AU-24).
  `.pdf` (v0.10.0) and `.docx` / `.xlsx` / `.pptx` (v0.11.0) had parser tests
  and an indexing end-to-end test, but nothing asked whether a query about a
  binary document's contents actually retrieves it — the golden set the
  project tracks is 26 queries over 49 documents, every one of them `.md`, so
  every recall / MRR / nDCG figure ever reported was blind to those four
  formats. `tests/eval_binary_formats.rs` runs `kb-mcp eval` over a corpus
  mixing all five, one query per format.

  The assertion is that each format's document ranks **first** for its own
  query. `recall@5` would have been vacuous: a five-document corpus returns
  everything within the first five hits no matter how badly extraction
  behaves. Eight Markdown distractors plus a rank-1 assertion make the claim
  falsifiable, which was verified by mutation — replacing the `.docx` body
  with off-topic text drops it to rank 8, still inside `top_k` and still
  scoring `recall@10 = 1.0`.

  Topical vocabulary appears only in document bodies; filenames and headings
  are deliberately generic, because a chunk heading carries an FTS weight of
  2.0 and these formats fall back to a filename-derived title, so either would
  let a document rank first with its body extraction broken — the shape AU-13
  had.

## [0.14.0] - 2026-07-27

### Added

- **`kb-mcp-tray.exe` and `kb-mcp-svc.exe` are attached to the release.** They
  never had been. Both crates set `publish = false`, and cargo-dist skips a
  `publish = false` package unless `[package.metadata.dist] dist = true` says
  otherwise — so from v0.9.0 onward the release workflow built and announced
  `kb-mcp` alone, while the READMEs told Windows users to take the tray out of
  a release archive that did not contain it. Two changes were needed, and
  either one alone changes nothing: `dist = true` on both packages, and their
  versions moved to 0.14.0, because an unqualified `vX.Y.Z` tag announces only
  the dist-able packages carrying that exact version. Verified with
  `dist plan --tag=v0.14.0` against the pinned cargo-dist 0.31.0, and by
  building both with the release `dist` profile for `x86_64-pc-windows-msvc`.

  Each is its own archive — `kb-mcp-tray-x86_64-pc-windows-msvc.zip` and
  `kb-mcp-svc-x86_64-pc-windows-msvc.zip` — not extra files inside the `kb-mcp`
  archive, which is what the READMEs had claimed. Extract the tray next to
  `kb-mcp.exe`, where `kb-mcp service install --with-tray` looks for it.

  Practical consequence beyond the tray: `kb-mcp service install` prefers
  `kb-mcp-svc.exe` for the logon Action and silently falls back to a
  console-visible one when the sibling is missing. Since the launcher was
  never shipped, every installation from a release archive took the fallback,
  and the v0.9.1 "no console flash at logon" fix has not reached anyone until
  now.

### Removed

- **`.xls` (legacy BIFF) is no longer indexed** (AU-06). Listing `"xls"` in
  `[parsers].enabled` now fails at startup with an explanation instead of
  registering the parser. calamine materialises every sheet of a workbook
  densely while opening it, before kb-mcp regains control, and BIFF bounds a
  *sheet* but places no bound on a *workbook* — so a small crafted file can
  exhaust memory, and an allocation failure aborts the process rather than
  skipping the file. Convert affected workbooks to `.xlsx`, which is read as a
  stream. The measurements, the options that were rejected, and the conditions
  under which `.xls` could return are recorded in
  [ADR-0001](docs/decisions/0001-withdraw-xls-legacy-biff-support.md)
  ([日本語](docs/decisions/0001-withdraw-xls-legacy-biff-support.ja.md)).

  `kb-mcp index` now validates `[parsers].enabled` before it touches anything
  at all. The check used to run after the database was opened, after the
  embedding model was loaded, and — with `--force` — after the reset, so a
  config carrying an id this build rejects (which `"xls"` now is, and which an
  upgraded installation may still hold) emptied the database and then exited
  with an error, leaving no index. Even without `--force` it created the
  database and ran schema migrations for a run that could not succeed.
  Deciding whether an id is valid needs only the config string, so it now
  happens first: a rejected config leaves no database behind and downloads
  no model.

  `kb-mcp serve` now says so when the index still holds documents whose
  extension `[parsers].enabled` no longer covers. Those rows are pruned by
  the next `kb-mcp index`, but `serve` does not index, so an installation
  that only ever runs the server keeps them — and they surface as hits that
  search returns and `get_document` then refuses, the same "findable but not
  openable" shape as AU-02. The warning names the count and an example and
  points at `kb-mcp index`; it does not delete anything, because a narrowed
  `enabled` list is often temporary and silently dropping rows at every
  startup would be worse than the confusion it prevents.

### Fixed

- **A Windows shortcut path or service error came back garbled on a
  non-English system** (AU-04). A redirected `powershell.exe` writes in the
  active code page, not UTF-8, and every call site decoded it with
  `String::from_utf8_lossy` — so on a Japanese host CP932 became a run of
  U+FFFD. For `kb-mcp service install` and the tray's Start / Stop / Restart
  that lost the text of the failure being reported. For the tray's autostart
  installer it was not a display problem at all: the helper returns the path
  of the `.lnk` it created and the caller turns that string into a `PathBuf`,
  so an account whose profile directory contains non-ASCII characters had the
  wrong shortcut path stored. PowerShell is now asked for UTF-8 rather than
  guessed at, at the single point where each backend spawns it, and output
  that becomes a value is decoded strictly — a lossy decode returns success
  with a corrupted path, which nothing downstream can detect. Output that
  only feeds a diagnostic message still decodes leniently, since an error
  path must not lose the error it was reporting, but now says when characters
  were replaced. The two `schtasks` calls are unaffected: `schtasks` is not
  PowerShell, and only ASCII fields are read from its output.

- **PDF text extraction had no ceiling on how much it would produce**
  (AU-05). The audit filed this as "no decompression budget", but the crate
  turned out to have several: reading its source at the pinned 4.1.1 shows a
  256 MB cap per decompressed stream, enforced incrementally, a compression
  ratio guard, and a 100,000-page limit. Two real gaps sat above those. The
  per-page text limit the crate offers — `ExtractionOptions::max_extracted_bytes`,
  which bounds accumulation rather than truncating a finished string — defaults
  to `None`, and kb-mcp never set it. And every one of the crate's guards is
  per stream or per page; nothing watches the total, so pages could be summed
  without limit. That is the same shape as the per-entry-but-not-cumulative
  hole closed for OOXML in v0.11.0. Extraction now runs page by page with the
  per-page limit set and a running total capped at the same 50 MB used for
  binary input, and a page that hits the per-page limit says so instead of
  quietly losing text. Output for well-formed PDFs is unchanged — a test
  asserts the new path returns exactly what the crate's own `extract_text`
  does, and the extractor is reused across pages so its cross-page font cache
  still applies. A text budget bounds memory but not decompression: a file
  whose streams expand into operators emitting almost no text would keep that
  counter near zero while still being fully decompressed, so extraction also
  stops after 120 seconds — the crate exposes no cumulative decompression
  accounting, and the timeout it does define is not wired into the extraction
  path. That residual was bounded to begin with, since input is capped at
  50 MB and DEFLATE tops out near 1032:1, but the ceiling was measured in
  minutes rather than seconds.

- **A damaged docx or pptx was indexed silently, with part of its text
  missing** (AU-13). Every OOXML reader ended its event loop with
  `Err(_) => break`, so a file whose XML stops partway — a truncated copy, a
  bad transfer — returned whatever had been read so far as a complete,
  successful parse. It then sat in the index with content missing and nothing
  said about it. All six XML loops (`word/document.xml`, four in the pptx
  reader, and `docProps/core.xml`) now name the file and the part they were
  reading and say the text is truncated there. The partial text is still
  kept: for a damaged file, some of it beats none of it, and per-file skipping
  on hard errors already exists from AU-21.

  In the same pass, docx now treats `<w:br/>`, `<w:cr/>` and `<w:tab/>` as the
  separators they are. They appear as siblings of `<w:t>`, so ignoring them
  ran the surrounding words together — a paragraph reading "line one" then
  "line two" was indexed as `line oneline two`, which matches neither phrase.

- **One `search` request could occupy the server for minutes** (AU-17). The
  `query` string has been capped at 1 KiB since v0.7, but the list filters
  travelling in the same request — `path_globs`, `tags_any`, `tags_all` — had
  no limit on how many entries they carried or how long each one was, and the
  HTTP transport sets no body-size limit either. `tags_any` is the sharp edge:
  it is not a SQL predicate but a linear scan run against every candidate, so
  its cost grows with entries × candidates. Measured on a debug build, a
  request carrying 1,000,000 tags against 1,000 candidates spent 85 seconds;
  100,000 spent 8.2. Patterns behave similarly through glob compilation —
  100,000 of them take 1.65 s, and a single 100,000-character glob takes 0.5 s
  (globset only rejects one on its own at around a million characters, after
  2.8 s). Each list is now limited to 64 entries of at most 1 KiB, checked at
  the MCP boundary and again inside `compile_path_globs` so the CLI is covered
  by the same rule. Because those checks can only run once the request has been
  deserialized, the HTTP transport also caps request bodies at 1 MiB, which it
  had not done at all — otherwise a body carrying a million tags would still be
  buffered and parsed in full before anything could reject it. The stdio
  transport is deliberately left unbounded: its client is a local process with
  the user's own privileges, so there is nothing there to protect.

- **A `bind` value in `kb-mcp.toml` could run a command when the tray opened
  the web UI** (AU-12). The tray split `bind` at its last colon and carried
  both halves into its URLs as strings, so anything written there ended up in
  `ui_url` — which was handed to `cmd /c start`, and `cmd.exe` parses what
  follows `/c` as a command line. Rust's `Command` only quotes arguments that
  contain whitespace, so an `&` passed straight through: measured, `cmd /c echo
  <url>&ver` ran `ver`. A `bind` of `127.0.0.1:3100&ver&` was enough. The same
  string-splicing let `127.0.0.1:3100@evil.example` through, where the part
  before the `@` becomes *userinfo* and the real host is `evil.example` — the
  tray's status polling would have gone there on its own, without anyone
  clicking anything.

  `bind` is now parsed as `<ipv4>:<port>`, `[<ipv6>]:<port>` or
  `localhost:<port>`, and the tray rebuilds its authority from the host class
  and the numeric port, so no byte of the config string reaches a URL. An
  unparseable `bind` stops the tray at startup with an error naming the
  setting. Opening the UI now goes through `ShellExecuteW`, which treats its
  argument as a shell object rather than a command line; since that API will
  also launch an executable it is given, the URL is checked to be `http://` or
  `https://` first.

- **A path containing `&` or `<` produced a plist that launchd cannot read**
  (AU-10). `render_plist` interpolated the binary path, the config directory
  and the service name straight into `<string>` elements. All three of `&`,
  `<` and `>` are legal in a macOS filename, so installing from
  `/Users/a&b/bin/kb-mcp` wrote a plist that is not well-formed XML — and the
  failure surfaces at `launchctl load`, after `kb-mcp service install` has
  already reported success. Every interpolated value is now XML-escaped.
  On the systemd side, a path containing a newline is refused with a message
  naming the offending field instead of being written into a unit file, where
  everything after the newline would be read as a further directive; and the
  binary path in `ExecStart=` is quoted when it contains spaces, which
  previously turned `/home/john doe/bin/kb-mcp` into the command
  `/home/john` with `doe/bin/kb-mcp` as its first argument. A literal `%` in
  that path is doubled, since specifiers are expanded before unquoting.
  `WorkingDirectory=` is deliberately left verbatim: systemd.syntax(7)
  describes quoting only "for settings where quoting is allowed" without
  enumerating them, and emitting quotes a setting does not interpret would
  break paths that work today.

- **A `--force` reindex that failed partway through destroyed the index it was
  replacing** (AU-11). `reset_for_model` performed five writes with no
  transaction around them: three `DELETE`s, a drop-and-recreate of the
  `vec_chunks` vector table, and the `index_meta` update recording the new
  model. Anything that stopped it in the middle left a state no later run
  repairs on its own — documents present but chunks gone, or a `vec_chunks`
  built for the new dimension while `index_meta` still named the old model.
  The worst case is not hypothetical: `recreate_vec_chunks` drops the table
  before creating its replacement, and `CREATE VIRTUAL TABLE ... USING vec0`
  rejects a dimension above 8192, so a request for a larger one left the
  database with no `vec_chunks` at all. The five writes are now one
  transaction, and it steps aside when a caller has already opened one, since
  SQLite has no nested transactions. Verified that virtual-table DDL does take
  part in a rollback — the documentation does not promise it, so it was
  measured: dropping and recreating `vec_chunks` at a different dimension
  inside a transaction, then rolling back, restores the original table and its
  rows.


- **`kb-mcp eval` compared runs from either side of a `[contextual]` switch as
  if they were the same experiment** (AU-61). Turning contextual retrieval on or
  off changes every chunk's embedding and FTS text and requires a `--force`
  re-index, but the run fingerprint recorded only the model, reranker, limit,
  k values, golden hash, metric version and the MMR / parent-retriever / fusion
  settings — so `--fail-on-regression` happily diffed a context-on run against a
  context-off baseline and could fail the build over a difference that is not a
  regression. The fingerprint now carries the index's context mode, read from
  `index_meta.context_mode` rather than from the config, since it is the index
  that determines what was measured. Context-off runs record nothing and stay
  comparable with every baseline taken before this existed; a baseline recorded
  with context on becomes incomparable once, the same way the metric-version
  bump worked.


- **A crafted `.xlsx` could make indexing decompress far more than the 50 MiB
  cap by lying about its size** (AU-20). The preflight that runs before
  calamine summed each entry's *declared* uncompressed size, and the ZIP
  format does not enforce that number — the CRC is only checked after the
  whole entry has been decompressed, and zip 8.6 does not bound deflate output
  by the declaration either. Measured: a 101 KB workbook declaring 10 bytes
  for its worksheet expanded to 100 MB, sailed through the preflight, and kept
  calamine busy for 13 seconds; the ratio scales linearly, so a file still
  under the 50 MiB input cap could demand tens of gigabytes. The preflight now
  decompresses each entry for real, discarding the output and stopping one byte
  past the remaining budget, so both the memory and the work it can be made to
  do stay bounded regardless of what the archive claims. The same file is now
  rejected in 0.7 s with a `zip-bomb guard` error, and the run continues with
  the other files. Legitimate workbooks pay one extra decompression pass:
  measured at ~5 ms for 11.8 MB of XML, against embedding costs in the hundreds
  of milliseconds.

  It also no longer picks which entries to check by filename suffix. calamine
  resolves a worksheet through its relationship `Target` and never looks at the
  suffix, so a part named `xl/worksheets/payload` is read normally while a
  suffix-based check skips it — the third bypass of the same kind, after fixed
  paths and missing `.rels` in v0.11.0. Every entry now counts, which makes the
  guarantee statable without reference to naming: **an archive may decompress
  to at most the cap, in total**. The cost is that images under `xl/media/`
  count too, so a workbook whose entire decompressed content exceeds 50 MiB is
  skipped — with the raw input already capped at 50 MiB and images inflating
  about 1:1, that means a file near the cap that is mostly pictures.

- **One malformed Office document could abort an entire `kb-mcp index` run**
  (AU-21). The indexer already skips a file whose parser returns `Err`, but a
  **panic** unwinds straight past that `match`, and indexing is sequential, so
  the run dies at the offending file — files after it are never indexed. Only
  the PDF parser was protected; `docx` / `xlsx` / `pptx` were not. This is not
  hypothetical: a spreadsheet declaring `<dimension ref="B2:A1"/>` makes
  calamine compute `end - start` on unsigned values, which panics in any build
  with debug assertions. On a two-file knowledge base the old binary exited
  101 without indexing anything; it now logs `Skipping evil.xlsx: parse
  failed: … xlsx parser panicked: attempt to subtract with overflow` and
  finishes normally.

  Rather than repeat a `catch_unwind` in three parsers, the entry point
  `parse_bytes` now wraps `parse_bytes_inner` (the new override point) for
  **every** parser, present and future — the isolation belongs to the boundary
  where untrusted files meet third-party crates (calamine, zip, quick-xml,
  oxidize-pdf), not to individual formats, since we cannot enumerate the panic
  sites inside them. It sits on a `ParserExt` extension trait with a blanket
  impl rather than being a default method on `Parser`, so no parser can
  override it and quietly opt out of the guard. The panic payload is carried
  into the error message, so suppressing the backtrace does not cost the
  diagnosis. The PDF-only guard is gone, replaced by the shared one.

- **The tray's Stop and Restart could not stop the daemon, and said they
  had** (AU-65, a v0.9.1 regression). Both called `Stop-ScheduledTask`, which
  terminates only the process the scheduler itself launched. Since v0.9.1 that
  process is `kb-mcp-svc.exe`, the console-hiding launcher, which detach-spawns
  the daemon and exits immediately — so the task reads as finished and the
  cmdlet has nothing left to stop. It still returns success, so the tray
  reported the stop as done while the daemon kept serving. Measured on a probe
  task: stopping a task whose own process was still running killed that process
  and left its child alive, so the scheduler's reach does not extend to
  descendants and keeping the launcher alive would not have helped either.
  `/api/admin/status` now reports the daemon's `pid`, and the tray terminates
  that process through the Win32 API — one `OpenProcess`, then the image-name
  check and the termination both on that handle, so the pid is resolved exactly
  once and a recycled pid cannot be hit. This also covers pre-v0.9.1 installs,
  where the daemon is the task's own process; `Stop-ScheduledTask` is kept only
  as a fallback. Most importantly the stop no longer trusts the mechanism: it
  confirms the daemon is gone by **binding its configured address**, and only
  then reports success. That is what makes `restart` safe, and it makes the
  whole family of silent failures impossible to reproduce rather than fixed
  case by case. Binding is what settles it because probing does not: an HTTP
  client never classified a refusal as one, a raw TCP connect times out instead
  of being refused wherever the firewall drops packets to closed ports, and
  probing loopback misses a daemon holding the wildcard address entirely —
  Windows lets a specific address bind alongside a wildcard listener.

  Known limitation: a daemon from v0.9.1 up to this release does not report a
  pid, so the first stop after upgrading the tray still cannot reach it and
  says so instead of claiming success. Stop that daemon once by hand; every
  later one reports its pid.

  The first implementation generated a PowerShell `Stop-Process` script, and
  five review rounds found five defects in it — none in the logic, all in
  PowerShell's error and exit-code semantics (`-ErrorAction SilentlyContinue`
  still exits 1, `try`/`catch` does not change that, both `Stop-Process -Id`
  and `-InputObject` re-resolve the pid because `Process.Kill()` reopens by
  number, and a denied handle was indistinguishable from a missing process).
  Each fix opened the next hole, so the approach was replaced rather than
  patched further. The behaviour is now covered by tests that spawn real
  processes and assert they do or do not get terminated, plus an end-to-end
  test against an actual daemon — which caught one more defect the unit tests
  could not have.

- **Tool schemas advertised constructs that break strict tool-calling
  runtimes** ([#75](https://github.com/alphabet-h/kb-mcp/issues/75)). Every
  optional parameter was published as a union type — `{"type": ["string",
  "null"]}`, 26 of them — alongside Rust-width `format` values such as
  `uint32` and `float`. All of it is valid JSON Schema 2020-12 and clients
  built on the official SDKs handle it, but OpenAI-style function calling
  rejects `null` inside a union, and runtimes that compile the schema into a
  decoding grammar (llama.cpp, Ollama, vLLM) have long-standing bugs with
  union types; the workaround published for them is exactly to strip `null`
  out of the type array. When a runtime cannot build a call, the model tends
  to emit its raw tool-call template as plain text, which never reaches the
  server. kb-mcp now advertises plain single types, and replaces each width
  `format` with the explicit `minimum` / `maximum` it stood for. Nothing the
  server accepts changes: optionality was already carried by the field's
  absence from `required`, and an explicit `null` still deserialises to
  `None`. Writing the integer bounds out matters because `schemars` emits
  `minimum: 0` for unsigned types but never a `maximum`, so removing the
  format alone would have advertised a domain *wider* than the server
  accepts — a client would be told `4294967296` is a valid `u32`, and serde
  would reject it before any handler saw it.

- **The nightly `--include-ignored` run raced itself whenever the model
  cache was cold.** Several integration-test binaries each spawn `kb-mcp`
  as a subprocess, so on a cold cache they all reach for the same
  HuggingFace blob lock at once and every one but the winner dies with
  "Lock acquisition failed". `ci.yml` gained a serial pre-warm step for
  this in #71, but `nightly.yml` never did — and the nightly model cache
  is precisely what the 10 GB per-repository cache limit evicts first, so
  the failure was waiting for the first night after an eviction. The
  nightly job now pre-warms the cache single-threaded before running the
  full suite.
- **The nightly Linux leg re-downloaded 4.6 GB of models every run.** The
  job stayed green, so the only visible symptom was its saved cache
  shrinking from 2.6 GB to 74 MB. Setting `FASTEMBED_CACHE_DIR` does not
  put every model in one place: a unit test tears down by calling
  `remove_var("FASTEMBED_CACHE_DIR")`, which clears the variable for the
  whole process — `cargo test` runs its tests as threads, not as separate
  processes — so any model initialised after that point resolves to the OS
  default directory instead. BGE-M3 and the reranker load late enough to
  land outside the cached directory. The job now caches both locations, and
  the root cause is gone too: the decision that test covers is now a plain
  function taking the environment state as an argument, so the test asserts
  on it without touching process-wide state at all.
- **A failed model download left the nightly run permanently broken.** A
  transient 503 from the HuggingFace CDN — observed while repeatedly
  exercising a cold cache — failed the run, and because a cache is only
  saved when the job succeeds, the next night started cold as well and had
  the same chance of failing. The pre-warm step now retries up to three
  times with a growing backoff. Retrying is safe here specifically because
  the step exists to populate the cache, not to report on the code: the
  suite that carries the actual signal runs afterwards, unretried.
- **The nightly coverage job failed intermittently on the same download
  race.** It had neither a model cache nor a pre-warm step, so every run
  downloaded BGE-small from cold with several test binaries competing for
  the lock. It now restores the cache the `ignored-tests` job saves — read
  only, so it cannot win the key and lock that job's much larger archive
  out of storage — and pre-warms serially for the days the cache misses.
- **The PR CI's model cache could never be replaced once it had been
  written** (AU-18, AU-53). Its key was `fastembed-bge-small-<os>`, with
  nothing in it derived from the dependency graph, and `actions/cache`
  refuses to overwrite an existing key — it logs `Cache hit occurred on the
  primary key ..., not saving cache.` The first archive ever saved for an OS
  was therefore the one every later run restored, no matter what happened to
  `fastembed` or `ort` afterwards, and the job stayed green while checking
  the code against a stale model tree. The key now carries
  `hashFiles('Cargo.lock')` plus a hand-turnable version segment, which is
  what `nightly.yml` has done since its own archives went stale. It
  deliberately has no `restore-keys`: a prefix fallback lets a partially-hit
  run re-save the old contents under the new key, which is the same freeze
  reached by a different route, and the archive here holds only BGE-small
  (~130 MB), so a genuine miss costs one download.

### Internal

- **The unit and plist templates now live in one always-compiled module**
  (`src/service/render.rs`, part of AU-10). They previously sat inside
  `service::linux` and `service::macos`, which are gated on `target_os`, so the
  plist template was compiled only on macOS runners and the unit template only
  on Linux ones — a typo in either was invisible everywhere else, including
  locally. Both are pure functions over `InstallContext`, so they and their
  escaping helpers now build and are tested on all three CI legs;
  `service::linux::render_unit` and `service::macos::render_plist` remain as
  re-exports, so nothing that called them had to change. This is the same move
  AU-07/08 made for `child_args`.

- **`kb-mcp service status` / `list` had no tests at all** (AU-14). Everything
  the two subcommands print goes through three functions in
  `src/service/status.rs`, and not one of them was covered: the toml fallback
  that fills in `bind` and `kb_path` when the OS cannot report them, and the
  two formatters. The fallback is now split so its decision — which field wins
  when both the OS and `kb-mcp.toml` have an answer — is a plain function over
  an already-read config string, following the same shape as
  `build_register_script` and the AU-63 fix. That matters here because the
  alternative, driving it through `KB_MCP_CONFIG_HOME`, would have put a second
  process-wide environment mutation into a suite that runs its tests as threads
  — exactly what AU-63 removed. Seventeen tests now cover all three
  `ServiceState` arms, each field falling back independently, an absent,
  malformed, or irrelevant config, and both output formats. One of them pins
  something no eye-check reliably catches: that the columns `format_row` emits
  line up with the header `run_list` prints above them. Behaviour is unchanged.

### Documentation

- **`docs/` subpages that described behaviour the code does not have** (AU-46,
  AU-47, AU-48, AU-49). `eval.md` listed graded relevance as "parsed tolerantly
  but ignored"; every golden struct is `deny_unknown_fields`, so a `relevance:`
  key aborts the run — `unknown field 'relevance', expected 'path' or
  'heading'`, exit 1, before anything is evaluated. Its troubleshooting table
  listed an error string (`expected path not in index`) that appears nowhere in
  the source; the real symptom is a per-query `✗ <id>  recall@N: 0.00` line. It
  also claimed runs at default fusion settings stay comparable with
  pre-v0.13.0 baselines, but `metric_version` went 1 → 2 and the fingerprint is
  compared whole, so those runs are skipped — as the same file said two
  paragraphs earlier. `filters.md` was missing `min_quality` /
  `include_low_quality`, and gave the `low_confidence` formula as
  `top1.score / mean` when the implementation uses `max(scores) / mean` — they
  differ exactly when MMR has re-ordered the results. `citations.md` gave one
  condition for a null `match_spans` (there are three: non-ASCII query, empty
  query, content over 256 KiB) and promised "all match positions" where 100 per
  chunk is the cap. `retrieval-pipeline.md` still described a 2-column FTS
  index. The `tune` section now also documents the context-axis warning, which fires
  on default-configured KBs that reach the grid (a golden set with no effective
  FTS queries exits earlier).

- **The web UI and admin API were absent from the README** (AU-60). `/ui` and
  `/api/admin/status` have shipped since v0.8.0, but the only mentions were in
  the architecture doc and the Windows tray section — so anyone not running the
  tray had no way to learn they exist. Documented with the response shape, the
  loopback-peer restriction, the SSH-forward recipe for remote hosts, and why a
  reverse proxy must not map those routes. Also fixed the dead TLS-section
  anchor in both READMEs (AU-62), refreshed the hybrid-search description
  (three FTS columns, configurable `k` and weights), corrected the tray
  Start/Stop description to match v0.14.0, and brought `CLAUDE.md`'s format and
  subcommand lists up to date.

- **Deployment recipes that could not work as written** (AU-34, AU-35,
  AU-37, AU-38, AU-39, AU-45). The NAS recipe put `.kb-mcp.db` on the share
  and had every machine open it, which SQLite documents as unsupported:
  "All processes using a database must be on the same host computer; WAL does
  not work over a network filesystem." That is not a writer-only restriction —
  readers take part in the same shared-memory protocol — so no mount flag or
  single-writer rule could make it safe. The recipe now keeps the KB files on
  the NAS and gives **each machine its own index on local disk**, which falls
  out of mounting the share at a path whose parent is local (`.kb-mcp.db` is
  created beside `kb_path`). If you want one shared index, that is what the
  intranet-http recipe is for. The old advice to mount read-only was doubly
  wrong: kb-mcp opens the database read-write, and a WAL database cannot even
  be read without creating its `-shm` / `-wal` sidecars — measured with the
  directory made non-writable, `kb-mcp status` fails with `Error code 14:
  unable to open database file`. The intranet recipe
  never mentioned `[transport.http].allowed_hosts`, whose default is loopback
  only, so every LAN client following it was answered with 403 no matter what
  `bind` said; the config and both READMEs now cover it, including behind a
  reverse proxy. The personal recipe told you to `cargo install --path .`,
  which fails on the workspace root (`--path kb-mcp`), linked one directory
  too high after the workspace split, and described the reranker as loaded
  when the key is commented out — in that state `rerank: true` is a silent
  no-op, which is now stated where the claim used to be. The hook sample only
  rebuilt for `.md`, so a KB with Office or PDF files silently went stale; it
  now takes a `KB_EXTENSIONS` list defaulting to every supported format, with
  case-insensitive matching. Also documented: the intranet recipe uses a
  system unit deliberately rather than `kb-mcp service install` (user-level),
  and `/ui` plus `/api/admin/*` refuse non-loopback peers, so they cannot be
  reached from the LAN directly — but a reverse proxy on the same host
  presents a loopback peer and an allow-listed Host, so it has to map `/mcp`
  and `/healthz` only.

### Changed

- **AU-07 / AU-08**: The Windows service installer's PowerShell script is now
  built by a pure function, so the two hot-fixes baked into it have regression
  tests. `register_via_powershell` previously assembled the script and spawned
  the process in one body, which left both untested: the v0.8.3 fix (use
  `Register-ScheduledTask`'s Action/Trigger/Settings parameter set — `-Xml`
  fails user-level registration with HRESULT 0x80070005) and the v0.9.1 fix
  (an Action pointing at `kb-mcp-svc.exe` must pass no `-Argument`, because
  that launcher prepends `serve` itself). The second was an invariant split
  across two crates, documented with a comment on each side and asserted by
  neither, and both failure modes appear only at the *next logon* — well after
  `kb-mcp service install` reports success. The filesystem probe now lives in
  `resolve_action_target` and the rendering in `build_register_script`,
  matching how the Linux and macOS backends already expose `render_unit` /
  `render_plist`. `kb-mcp-svc` gained the corresponding `child_args`, compiled
  on every platform so its half of the invariant is checked on the Linux and
  macOS CI legs too. The rendered script is byte-identical; no behaviour
  changes.

- **AU-09**: The nightly `ignored-tests` job now runs on `windows-latest`
  in addition to `ubuntu-latest`. The Windows-only `#[ignore]` tests —
  Task Scheduler registration (`tests/service_install_integration.rs`) and
  tray `.lnk` install/uninstall
  (`crates/kb-mcp-tray/tests/install_integration.rs`) — had no CI coverage
  at all, because the only job passing `--include-ignored` ran on Linux.
  The two tests that each pull a ~2.3 GB model (BGE-M3 and the
  cross-encoder reranker) are skipped on the Windows leg: both assert
  OS-independent properties that the Linux leg already covers,
  `windows-latest` ships only 14 GB of free disk, and the Actions cache is
  capped at 10 GB per repository. The job caches both directories models
  can land in — the workspace-relative one that `FASTEMBED_CACHE_DIR`
  selects, and the OS default that `resolve_cache_dir` falls back to — and
  the cache key prefix moved to `fastembed-v4-` so that none of the earlier
  archives — which carry a different directory layout — is restored in place
  of the current one. The prefix has to move whenever the layout does:
  `actions/cache` refuses to overwrite an existing key, so a stale archive
  would otherwise stay frozen until `Cargo.lock` changed. Source code
  unchanged.

- **AU-53**: Every `ci.yml` job now sets `timeout-minutes`. None of them did,
  so each fell back to the documented default of 360 minutes and a job that
  hung — on a download, a lock, a test that never returns — would hold a
  runner for six hours before anyone saw a result. The caps are 30 minutes
  for `test`, 20 for `clippy` and 10 for `rustfmt`, against measured worst
  cases across the last 40 runs of 6.5, 3.3 and 0.2 minutes, leaving room for
  a cold `rust-cache` and a fresh model download. `nightly.yml` already
  bounded all three of its jobs; `release.yml` is generated by `dist` and is
  left alone. The same step also moves `ci.yml` to `actions/cache@v5`,
  finishing the Node.js 24 migration of 0.6.1 — that release bumped
  `nightly.yml` because it was the one emitting the deprecation annotation,
  and the `@v4` step here was added afterwards, so it had quietly
  reintroduced a Node.js 20 pin past the 2026-06-02 cutover. Source code
  unchanged.

## [0.13.1] - 2026-07-26

### Fixed

- **`search` accepted an unbounded `limit`, which could abort the process.**
  The value flowed through the candidate-pool calculation into
  `Vec::with_capacity`, so a single request — `kb-mcp search --limit
  4294967295`, or the equivalent MCP call — attempted a ~927 GB allocation
  and died. Allocation failure aborts rather than panics, so it could not
  be caught; over the HTTP transport the whole daemon went down with every
  open connection. `limit` is now clamped to 1000 at both the MCP and CLI
  boundaries, and the pre-allocation is derived from the already-capped
  fetch size.
- **Filtered searches with `limit >= 82` failed outright.** The filter
  over-fetch cap (10,000) exceeded sqlite-vec's fixed KNN ceiling of 4096,
  so any search that engaged a filter — including the default
  `min_quality = 0.3` — errored with "k value in knn query too large". The
  fetch size is now clamped to the sqlite-vec limit, degrading to fewer
  candidates instead of failing.
- **`get_document` rejected files with uppercase extensions.**
  `Registry::has_extension` matched case-sensitively while the indexer's
  walker did not, so `Report.PDF` was indexed and returned by search but
  could not be opened.
- **The file watcher ignored the built-in exclude list.** `.git`,
  `.svn`, and `node_modules` are skipped regardless of configuration
  during a full index, but the watcher only consulted the user's
  `exclude_dirs`; a narrowed configuration let live edits under those
  directories reach the index.
- **`kb-mcp --version` now works.** It previously failed with
  `error: unexpected argument '--version' found`, despite CONTRIBUTING
  asking bug reporters to run it first.

### Changed

- Updated `crossbeam-epoch` (0.9.18 → 0.9.20) and `quinn-proto`
  (0.11.14 → 0.11.16) to clear RUSTSEC-2026-0204 and RUSTSEC-2026-0185.
- CHANGELOG: added the compare links for every release from 0.7.5 to
  0.13.0, which had been missing since 0.7.4.

## [0.13.0] - 2026-07-26

### Added

- **`[search.fusion]` config section** — the RRF constant (`rrf_k`, default
  `60.0`) and the three FTS5 bm25 column weights (`bm25_heading_weight` /
  `bm25_context_weight` / `bm25_content_weight`, defaults `2.0 / 1.0 / 1.0`)
  are now configurable instead of compile-time constants. **Defaults are
  unchanged and the section is optional**, so existing installs behave
  bit-for-bit identically. Values are range-checked at config load
  (`rrf_k >= 1.0`, weights finite and `>= 0.0`, not all three zero); a
  non-default section is recorded in the eval `ConfigFingerprint` so tuned
  runs are never compared against untuned baselines.
- **`kb-mcp tune` subcommand** — measures how much the fusion parameters move
  retrieval quality on your own KB and prints a statistically guarded
  recommendation. It **applies nothing**: the output is either a paste-ready
  `[search.fusion]` snippet or the conclusion that the built-in defaults should
  be kept. A pre-flight pass reports the effective query count (queries with at
  least 2 FTS candidates) and exits 2 without sweeping when none is effective,
  because kb-mcp's single-phrase trigram FTS only engages for verbatim matches.
  The recommendation is gated on nested leave-one-query-out CV: held-out mean
  ΔnDCG@5 above both 0.02 and 2× the paired standard error, selection stability
  over half the folds, and no regression in recall@k or MRR. Always runs
  without a reranker; the docs describe how to confirm a candidate through the
  full pipeline with `kb-mcp eval`.

### Fixed

- **`ndcg_at_k` could exceed 1.0 when multiple expected entries matched the
  same hit** — e.g. a golden query listing the same path twice, or a
  path-only expected alongside a heading-specific expected for the same
  path. The metric now walks hits in rank order and greedily consumes
  expected entries one-to-one (preferring heading-specific entries over
  path-only ones on the same hit), which mathematically bounds DCG ≤ IDCG
  for arbitrary input. Well-formed golden sets with distinct expected paths
  are unaffected — existing eval baselines remain valid. Also fixes the
  flaky `prop_ndcg_at_k_in_unit_range` property test, which tripped over
  this exact case when its narrow path space generated duplicates.
  - `ConfigFingerprint` now carries a `metric_version` field (current: 2;
    histories recorded before this release deserialize as 1). Runs recorded
    with the old formula are automatically excluded from
    `--fail-on-regression` comparison, so this intentional metric
    correction can never be misreported as a retrieval regression. The
    first `kb-mcp eval` after upgrading starts a fresh comparison baseline.
  - Displayed comparisons (`--format text` arrows / `--format json` `diff`)
    now also require full fingerprint compatibility instead of only a
    matching golden hash, so cross-metric-version (or cross-model) deltas
    are no longer rendered; a dedicated "config or metric version changed"
    notice is shown instead.

## [0.12.0] - 2026-07-21

### Added

- **Static Contextual Retrieval (opt-in, `[contextual].enabled = true`)**:
  each chunk can be prefixed, at index time, with a deterministic context
  breadcrumb — the document title plus its heading ancestry (` > `-joined,
  200-char cap) — that gets injected into the embedding input, a new FTS5
  third column (`context`, scored via a dedicated Contextual BM25 weight),
  and the reranker input. Generated purely from document structure (two
  ancestry families: Markdown's level-keyed heading stack, and a
  single-level `[title]` for PDF/Office/`.txt` chunks) — no LLM call, no
  extra runtime dependency, no drift beyond what a normal re-index already
  handles. The returned `search` / `get_document` schema is entirely
  unchanged; context is an internal ranking signal only.
  - `index_meta.context_mode` (`ContextMode::{Off, Static}`) versions each
    DB's actually-built mode independently of the config's desired mode:
    a config/DB mismatch without `--force` prints a stderr warning and
    keeps the DB's existing mode rather than silently mixing embedding
    spaces mid-index; `kb-mcp index --force` migrates explicitly.
    `kb-mcp status` reports `Context mode: static` / `Context mode: off`.
  - **Judgment-gate result: defaults to off.** An A/B evaluation on a
    574-document dogfood KB (bge-m3) showed that with kb-mcp's actual
    default pipeline (no reranker), enabling context injection made
    retrieval measurably worse (recall@5 -0.080, MRR -0.041). With a
    reranker configured (`bge-v2-m3`), it improved every metric except a
    small recall@10 dip (recall@5 +0.047, MRR +0.102, nDCG@10 +0.044). See
    the README's "Contextual Retrieval" section for the full numbers and
    the reranker-only recommendation.

### Changed

- **FTS5 schema: `fts_chunks` gains a third column (`heading`, `context`,
  `content`)**, migrated automatically and once on first open of a
  pre-v0.12.0 database (drop + recreate the virtual table, then
  repopulate from `chunks`, inside a `BEGIN IMMEDIATE` transaction to
  serialize against concurrent openers). `chunks.context_text` is added
  the same way via an idempotent `ALTER TABLE`. No CLI action is required
  — this runs transparently the next time any `kb-mcp` command opens the
  database.
- **`busy_timeout` raised from 10s to 30s** (`Database::init`): the FTS
  migration above holds a write lock for its full repopulate, which was
  measured at 9.7–12.3s under concurrent embedding/reranker model load on
  a 10,002-chunk KB — exceeding the previous 10s budget in some trials.
  30s keeps a comfortable margin over the worst observed case.

## [0.11.0] - 2026-07-20

### Added

- **Office document indexing (opt-in `[parsers].enabled = [..., "docx",
  "xlsx", "xls", "pptx"]`)**: four new binary-format parsers, all
  implemented in-tree (no LibreOffice / MS Office dependency):
  - **`.docx`**: [zip](https://crates.io/crates/zip) +
    [quick-xml](https://crates.io/crates/quick-xml) read `word/document.xml`
    and chunk it by heading hierarchy — a `<w:pStyle w:val="HeadingN">`
    paragraph style acts as a section boundary, the same rule Markdown
    headings use, including `exclude_headings` support. Table cell text
    flows through the same paragraph handling with no special-casing
    needed (OOXML nests `w:tbl > w:tr > w:tc > w:p > w:r > w:t`).
  - **`.xlsx` / `.xls`** (legacy BIFF): [calamine](https://crates.io/crates/calamine)
    (pure Rust, auto-detects OOXML vs. BIFF) produces one chunk per
    non-empty sheet (heading `Sheet: <name>`, tab-joined cell text per
    row), truncated at 1 MiB per sheet with row-aligned truncation — the
    row that pushes the running total past the cap is kept whole, then
    extraction for that sheet stops (never cuts mid-row).
  - **`.pptx`**: zip + quick-xml collect `ppt/slides/slideN.xml` parts in
    numeric slide order (not zip iteration order), one chunk per slide
    (heading `Slide N: <title>` picked up from a `ctrTitle`/`title`
    placeholder shape, including in-slide table text in the body). Speaker
    notes are appended as a trailing `[notes]` section, resolved through
    the slide's `.rels` `notesSlide` relationship instead of a
    same-numbered-file guess — a dry-run found the same-number heuristic
    misattributes notes to the wrong slide once slide/notes numbering
    diverges after edits.
  - **Frontmatter**: `.docx` / `.xlsx` / `.pptx` all map `docProps/core.xml`
    (Dublin Core `title` / `created`-or-`modified` date / `keywords` →
    tags) to frontmatter, falling back to a filename-derived title when
    the part is missing or `title` is empty. `.xls` predates
    `docProps/core.xml` and always uses the filename-derived title.
  - Password-protected or corrupt Office files fail to open as a zip (or
    BIFF container) and are skipped with a warning instead of failing the
    whole `index` run, matching the PDF behavior. All four formats share
    the 50 MiB raw-byte size cap (`MAX_RAW_BINARY_BYTES`) with the
    indexer's size-skip guard and `get_document`. Office lock files
    (`~$*.docx`-style and `.~lock.*#`) are excluded from the directory
    walk (landed in this cycle's PR-1, alongside the byte-based read
    layer).
  - Known limitations: no legacy `.doc`/`.ppt` (pre-2007 binary Office
    formats), no OpenDocument (`.odt`/`.ods`/`.odp`), and table structure
    is flattened to plain text — no row/column grid is preserved in the
    chunk. See the README "Office document indexing" note for details.

## [0.10.0] - 2026-07-19

### Added

- **PDF indexing (opt-in `[parsers].enabled = [..., "pdf"]`)**: text is
  extracted page-by-page via [oxidize-pdf](https://crates.io/crates/oxidize-pdf)
  (pure Rust), and each non-empty page becomes one chunk with heading `p.N`.
  `Title` / `CreationDate` PDF metadata become frontmatter when present,
  falling back to a filename-derived title when the PDF has no `Title`.
  Scanned / image-only PDFs (no text layer, detected via an average
  chars-per-page heuristic **over non-empty pages only** — averaging over
  every page, including blank/separator pages, wrongly rejected real-world
  PDFs with a dense content page and many blank pages; found by codex
  review on PR #69) and encrypted PDFs are skipped with a warning
  instead of failing the whole `index` run. Like other binary formats,
  `.pdf` files share the 50 MiB raw-byte size cap (`MAX_RAW_BINARY_BYTES`)
  with the indexer's size-skip guard and `get_document`. The
  `PdfDocument::extract_text` / `metadata` call sequence is wrapped in
  `catch_unwind` so a malformed PDF that panics inside the parser's
  dependencies degrades to a per-file skip-and-warn instead of aborting
  the run. The panic-report-suppressing hook is installed once (`Once`)
  instead of being swapped per extraction, gated by a thread-local flag
  around the `catch_unwind` call, so concurrent PDF extractions (e.g.
  multiple `get_document` HTTP requests) can't race and permanently
  disable panic reporting process-wide or hide unrelated threads' panics
  (found by codex review on PR #69). Post-processing applies a
  conservative line-end hyphenation join (only when both neighbors of
  `-\n` are ASCII lowercase, to avoid corrupting hyphenated model
  numbers, dates, or CJK-adjacent hyphens) and normalizes common
  ligatures (ﬁ/ﬂ/ﬀ/ﬃ/ﬄ). Also recovers UTF-16BE PDF Info-dict `Title`
  strings (common for non-ASCII titles) that `oxidize-pdf` mis-decodes
  one byte at a time when it doesn't detect the byte-order-mark — found
  while dogfooding a real Japanese PDF; falls back to the
  filename-derived title when recovery isn't possible instead of
  surfacing mojibake. `CreationDate` parsing no longer panics on a
  multibyte-contaminated ISO date string (found by codex review on
  PR #69) — an invalid date is now silently ignored (`date: null`)
  instead of taking down the whole document's extraction. See the
  README "PDF indexing" note for remaining known limitations (no OCR,
  multi-column reading order, unfiltered garbage `Title` metadata that
  doesn't match the UTF-16BE pattern).

### Changed

- **Index read layer is now byte-based.** All file read paths (`kb-mcp index`,
  the watcher, and `get_document`) read raw bytes and hash them with SHA-256
  instead of reading to a UTF-8 string. For existing Markdown/text knowledge
  bases this is a no-op — the byte hash of a UTF-8 file equals the previous
  string hash, so no re-index is triggered. This was the groundwork that
  landed in this release for the byte-based PDF parser above.

### Fixed

- **`kb-mcp index` no longer aborts when a file cannot be read or parsed.**
  Previously a single unreadable / non-UTF-8 file in the tree failed the whole
  run. Now such files are skipped with a warning and reported in the summary
  (`... N skipped ...`), and — critically — a transiently unreadable file (AV
  scan / editor lock) is **retained** in the index rather than silently pruned.

## [0.9.2] - 2026-05-18

### Fixed

- (v0.9.2 hot-fix) **`kb-mcp service install --force` config-preservation
  regression** (carried over since v0.8.0): the install path used to
  rewrite `kb-mcp.toml` from scratch with only `kb_path` + `[transport.http]
  .bind`, obliterating every user-customized field (`model`,
  `fastembed_cache_dir`, `exclude_dirs`, `[best_practice]`, etc.). On
  a daemon whose index DB was built with `bge-m3` (1024-dim), this made
  `kb-mcp serve` crash at startup with `embedding model mismatch`
  because the regenerated toml fell back to the default `bge-small`
  (384-dim). Discovered during the feature-44 / v0.9.0 dogfood and
  documented as 罠 10 in `.dev/knowledge/feature-44-summary.md`.

  v0.9.2 switches the install path to `toml_edit` for the merge step.
  When `kb-mcp.toml` already exists, it is parsed in place and only
  `kb_path` and `[transport.http].bind` are overwritten — every other
  key, inline comment, and the original field ordering are preserved
  verbatim. If the existing toml is unparseable, the install fails with
  a descriptive error pointing at the path so the user can fix it by
  hand rather than silently lose their config.

  Behaviour delta:
  - `install` over a fresh / absent toml: unchanged (= minimal toml).
  - `install --force` over an existing toml: now merges. The user
    custom fields survive intact.
  - Invalid pre-existing toml: now errors out instead of overwriting.

  4 new unit tests under `src/service/install.rs::tests` cover the
  fresh-write, merge, comment-preservation, and invalid-TOML paths.

## [0.9.1] - 2026-05-17

### Fixed

- (v0.9.1 hot-fix) **Windows `kb-mcp service install`**: the Task Scheduler
  Action launched `kb-mcp.exe serve` directly, which surfaced a visible
  console window on every login because Windows allocates `conhost.exe`
  before a console-subsystem process starts (`-WindowStyle Hidden` /
  `FreeConsole()` only hide it *after* a ~1-second flash; tracked upstream
  as microsoft/terminal#249 and PowerShell/PowerShell#3028 since 2018).
  v0.9.1 introduces a new tiny `kb-mcp-svc.exe` helper crate
  (`crates/kb-mcp-svc/`, ~230 KB, `#![windows_subsystem = "windows"]`) that
  the install path uses as the Action when the sibling binary is present.
  The helper spawns `kb-mcp.exe serve` with `CREATE_NO_WINDOW` so the
  child inherits no console — true 0-flash hidden launch. The bare
  `kb-mcp.exe` Action remains as a fallback for `cargo install --path
  kb-mcp` users who do not have the svc helper installed.

### Migration (existing v0.9.0 users)

Existing v0.9.0 installs continue to work but still show the console
window. To pick up the hidden-launcher Action, drop in the new
`kb-mcp-svc.exe` from the v0.9.1 zip alongside your existing
`kb-mcp.exe` / `kb-mcp-tray.exe`, then either:

- Re-run `kb-mcp service install --kb-path <path> --with-tray --force`
  (= regenerates the Action via the v0.9.1 install path), **or**
- Swap the Action manually without re-creating the rest of the task:

  ```powershell
  schtasks /End /TN '\kb-mcp-<service-name>'
  $action = New-ScheduledTaskAction -Execute 'C:\Users\<you>\.cargo\bin\kb-mcp-svc.exe' -WorkingDirectory '<config_home>'
  Set-ScheduledTask -TaskName 'kb-mcp-<service-name>' -Action $action
  schtasks /Run /TN '\kb-mcp-<service-name>'
  ```

## [0.9.0] - 2026-05-17

### Added

- (feature-44 PR-1) **Workspace split**: main `kb-mcp` crate moved to `kb-mcp/`
  subdirectory, root `Cargo.toml` becomes a workspace manifest, `[profile.dist]`
  relocated to workspace root.
- (feature-44 PR-1) New `crates/kb-mcp-tray/` member crate — Windows-only
  skeleton binary (`kb-mcp-tray.exe`, GUI subsystem in release). PR-1 ships
  just a gray tray icon; polling, menu, and daemon control land in PR-2.
- (feature-44 PR-1) Panic hook + daily-rotating file logger at
  `%LOCALAPPDATA%\kb-mcp\logs\tray.YYYY-MM-DD` (override level via
  `KB_MCP_TRAY_LOG=debug`). Required because GUI-subsystem binaries discard
  stdout/stderr in release builds.
- (feature-44 PR-1) `cargo-dist` per-crate target gating: `kb-mcp-tray.exe`
  is published only for `x86_64-pc-windows-msvc`; the main `kb-mcp` binary
  inherits the workspace-wide 4-target matrix (Linux x86_64/aarch64, macOS
  aarch64, Windows x86_64).
- (feature-44 PR-2) `kb-mcp-tray.exe` polls `/api/admin/status` every 5
  seconds (3 second timeout) and renders a 4-state status dot:
  - **green** = daemon healthy (last poll succeeded, not indexing)
  - **yellow** = daemon indexing (`indexing.active == true`)
  - **red** = daemon down for >= 1 minute (= 12 consecutive failed polls)
  - **gray** = polling pending (pre-first-poll)
- (feature-44 PR-2) Tray menu with 6 actionable items + 3 separators:
  Status (read-only) / Open Web UI / Start / Stop / Restart / Quit Tray.
  Start enabled only when Red/Gray; Stop and Restart enabled only when
  Green/Yellow.
- (feature-44 PR-2) Daemon control via async PowerShell
  `Start-ScheduledTask` / `Stop-ScheduledTask` cmdlets (= reuses the
  feature-43 PowerShell path, runs on a dedicated tokio runtime so the
  main event loop never blocks).
- (feature-44 PR-2) Open Web UI menu item launches the default browser
  at `<bind>/ui`.
- (feature-44 PR-3) `kb-mcp service install --with-tray` flag
  (Windows-only) installs a shell:startup `.lnk` shortcut launching
  `kb-mcp-tray.exe --service-name <name>` at the next logon. `--force`
  doubles as the duplicate-check override (= overwrite existing
  shortcut / HKCU Run value / Task Scheduler entry).
- (feature-44 PR-3) `kb-mcp service uninstall` now performs a
  best-effort cleanup of the tray autostart shortcut. Idempotent and
  warning-only on failure so the daemon uninstall always runs.
- (feature-44 PR-3) New `kb-mcp service tray-install` /
  `kb-mcp service tray-uninstall` standalone subcommands for managing
  the tray shortcut independently of the daemon registration.
- (feature-44 PR-3) `kb-mcp-tray` library API:
  `install::install_autostart` and `install::uninstall_autostart`
  generate PowerShell scripts (`WScript.Shell` COM) to create / remove
  the `.lnk` shortcut. 4 unit tests cover script generation + apostrophe
  escaping; 2 `#[ignore]` integration tests exercise the actual
  PowerShell round-trip (run with `cargo test -- --ignored` on Windows).

### Changed

- (feature-44 PR-3) `README.md` / `README.ja.md` updated: links to
  `examples/deployments/` and `examples/hooks/` now point at the new
  `kb-mcp/examples/` location (= workspace-split fallout). New
  "Tray monitor (Windows only)" section documents `--with-tray`, the
  4-state dot, the 6-item right-click menu, log paths, and the
  loopback-bind requirement.
- (feature-44 PR-3) `docs/ARCHITECTURE.md` / `.ja.md` source layout
  table gains a `crates/kb-mcp-tray/` row plus a dep section
  enumerating the Windows-only crates (`tray-icon` 0.24 / `tao` 0.35 /
  `image` 0.25 / `tracing-appender` 0.2 / `winresource` 0.1).

## [0.8.3] - 2026-05-13

### Fixed

- **Windows `kb-mcp service install`**: third (and final) attempt at user-
  level root-path registration. v0.8.2 switched from `schtasks /Create /XML`
  to `Register-ScheduledTask -Xml`, which fixed the elevation error but
  immediately hit a new "Access is denied" (HRESULT 0x80070005) — the
  `-Xml` parameter set doesn't auto-populate `<UserId>` in the task's
  Principal, so Task Scheduler falls back to a user-ambiguous principal
  that needs admin. v0.8.3 abandons the `-Xml` parameter set entirely and
  uses `Register-ScheduledTask -Action $a -Trigger $t -Settings $s
  -RunLevel Limited`, the parameter set that auto-builds the Principal
  from the current logon identity (= the exact pattern users had been
  using as a manual fallback). XML rendering (`render_task_xml`) and
  UTF-16 LE BOM encoding (`encode_utf16_le_bom`) helpers — historical
  workarounds from v0.8.0 → v0.8.2 — were removed along with their
  regression tests; the production install path no longer touches XML.

## [0.8.2] - 2026-05-13

### Fixed

- **Windows `kb-mcp service install`**: even after the v0.8.1 UTF-16 LE BOM
  fix, `schtasks /Create /XML` returned "Access is denied" when registering
  a task at the root path (`\<name>`) from a non-elevated shell — violating
  the spec § Q4 promise of "Phase 1 = no admin required". Switched the
  install path from `schtasks /Create /XML` to PowerShell's
  `Register-ScheduledTask -Xml` cmdlet (= scheduledtasks PowerShell module,
  COM-backed) which accepts user-level root-path registration. XML rendering
  + UTF-16 LE BOM encoding from v0.8.1 are preserved; PowerShell reads the
  file via `[System.IO.File]::ReadAllText` (= auto-detects the BOM). New
  `#[ignore]` smoke test `windows_register_scheduledtask_smoke_test` mirrors
  the production path and is opt-in for manual verification from an
  interactive logon session (= network / service logon sessions hit Access
  Denied at the Task Scheduler boundary even without elevation).

## [0.8.1] - 2026-05-13

### Fixed

- **Windows `kb-mcp service install`**: schtasks XML rejected on
  Japanese-locale Windows with "エンコードを切り替えることができません".
  v0.8.0 wrote `<?xml encoding="UTF-8"?>` + UTF-8 bytes (= valid XML
  but empirically broken on Japanese-locale schtasks). v0.8.1 emits
  `<?xml encoding="UTF-16"?>` declaration + UTF-16 LE bytes prefixed by
  a `0xFF 0xFE` BOM, which is the broadest-compatible form across
  Windows locales. New regression test `windows_task_xml_is_utf16_le_with_bom`
  pins the exact byte sequence so a future "encoding cleanup" can't
  silently revert. (= dogfood discovery during local v0.8.0 install on
  日本語 Windows)

## [0.8.0] - 2026-05-13

### Added

- **F-6 + H-9 Phase 1 (PR-1)**: `kb-mcp service install/uninstall/status/list`
  subcommand for cross-platform user-level service registration. Linux =
  systemd-user (`~/.config/systemd/user/kb-mcp-<name>.service`), macOS =
  LaunchAgent (`~/Library/LaunchAgents/com.kb-mcp.<name>.plist`), Windows =
  Task Scheduler AT_LOGON (`\kb-mcp-<name>`). No admin/sudo required, no
  NSSM / WiX / 3rd-party tooling — only Rust crates. Multi-instance via
  `--service-name` (default `"kb-mcp"`). Config home at
  `<dirs::config_dir()>/kb-mcp/<service-name>/` with `kb-mcp.toml` written
  at install time; `KB_MCP_CONFIG_HOME` env var overrides the base. Defaults:
  `--bind 127.0.0.1:3100`, auto-start ON (`--no-auto-start` to opt out);
  `--bind 0.0.0.0` and other non-loopback addresses require `--i-know` since
  kb-mcp has no authentication. `--purge --yes` deletes both config and
  index DB. `--no-auto-start` is honored at the OS layer (Linux: skip
  `systemctl enable`; macOS: `RunAtLoad=false` + `KeepAlive=false`; Windows:
  `<LogonTrigger><Enabled>false</Enabled></LogonTrigger>`).
- **F-6 + H-9 Phase 1 (PR-2)**: WebUI MVP + admin API on the HTTP transport.
  New admin sub-router with `/ui` (XSS-safe placeholder HTML — `textContent`
  + `createElement` only, no `innerHTML`), `/api/admin/status` (daemon /
  indexing / watcher / kb info JSON), and `/api/search` (POST JSON-in /
  JSON-out wrapper around the existing MCP `search` tool). All three routes
  are gated by `admin_host_check` middleware (exact-match Host header
  against loopback aliases + bind addr; substring match rejected to block
  bypass via `10.0.127.0.1.evil.com`). `/mcp` + `/healthz` remain on the
  public path with no behavior change. `KbServerShared` gained
  `started_at` / `started_instant` / `indexing_state` / `watcher_active` /
  `watcher_debounce_ms` / `config_source_label` / `allowed_admin_hosts`
  fields to drive the admin status response; watcher start/stop flips
  `watcher_active` via a Drop guard.

### Changed

- **F-6 + H-9 Phase 1 (PR-1)**: Removed `examples/deployments/personal-http/`
  recipe — superseded by `kb-mcp service install`. README migration note
  guides users on disabling any pre-existing manually installed units before
  re-installing via the new subcommand.

## [0.7.8] - 2026-05-06

### Added

- **D-10**: `kb-mcp index --quiet` flag to suppress per-file progress output
  (only `Indexing` / `Found N source files` / `Done in ...` summary lines remain).
  Useful when running from harnesses (e.g. Claude Code Bash tool) where streaming
  output is buffered until exit. Mutually exclusive with `--progress`.
- **D-10**: `kb-mcp index --progress` flag to show progress UI. On TTY: an
  `indicatif` progress bar with elapsed / position / percent / ETA. On non-TTY
  (pipe / redirect): periodic `Progress: N/M (P%)` lines (~20 emits per run +
  100% anchor). Auto-detected via `std::io::IsTerminal` on stderr.
  Incremental runs (`force=false`) tick the bar on unchanged/skipped files too,
  so the bar always reaches 100%.

### Changed

- **D-10**: MCP server `rebuild_index` tool now suppresses per-file progress
  output (= `ProgressMode::Quiet` fixed). The `IndexStats` JSON response
  returned to the client is unchanged; this only affects what the server
  process prints to its own stderr.

## [0.7.7] - 2026-05-05

### Added

- **F-63**: `parse_tags_json` の silent fail-open を可視化する `tags_parse_failures`
  counter を `Database` に追加。`index_meta` table に永続化 (= session shutdown
  時の best-effort flush + 起動時 read で前 session の値を復元)。`kb-mcp status`
  出力に新規 `Tags parse failures: N` 行を追加 (= 既存 `Documents:` / `Chunks:`
  の直後)。malformed `documents.tags` JSON の発火を operator が確認できる。

## [0.7.6] - 2026-05-05

### Changed

- **D-11 (= F-64 follow-up)**: `[transport.http].healthz_public = false`
  設定時の `/healthz` Host header validation を `http::uri::Authority::try_from`
  委譲に refactor し、rmcp 1.4 と semantic parity を達成 (詳細は
  `.dev/feature-ideas.md` D-11)。挙動変更:
  - malformed Host header → status code が **403 → 400** Bad Request
    (response body は `Bad Request: Invalid Host header` /
    `Bad Request: Invalid Host header encoding` /
    `Bad Request: missing Host header` のいずれか、Content-Type は
    `text/plain; charset=utf-8`)
  - DNS rebinding (= parse OK + allow-list 不一致) は **403 のまま**、
    response body 文言を `Forbidden: Host header is not allowed` に変更
    (= rmcp と byte-identical)
  - 既存 v0.7.5 で `healthz_public = false` を opt-in 設定した user のみ影響、
    default `true` の user は完全に無影響
- kb-mcp 拡張として **`:authority` URI fallback は維持** (= HTTP/2 /
  proxy-forwarded health check 互換性)。これは rmcp と意図的に外す superset

### Internal

- 自前 `split_host_port` / `extract_host_part` / 旧 4-way matching を全削除し、
  `http::uri::Authority::try_from` 委譲 + `NormalizedAuthority` struct
  (= rmcp `parse_allowed_authority` mirror) に置換。
- 新規 helper / struct: `validate_host_header` pure helper / `HostRejection` enum
  (3 variant) / `NormalizedAuthority` / `has_explicit_port_suffix` /
  `bad_request_typed` / `forbidden_plain`。
- test: 新規 36 件 (= 5 NormalizedAuthority + 8 has_explicit_port_suffix
  + 28 validate_host_header helper + middleware integration #29) 追加、
  既存 6 件 modify (= status/body assertion を rmcp parity 化)、
  旧 6 件 delete (= 旧 `extract_host_part_*`、新 helper test に 1-1 mapping
  で意味的統合)。最終 transport::http::tests 計 62 件。
- `http` crate を Cargo.toml に direct dependency として追加
  (= axum 0.8 transitive と同 v1.4.0、resolution 操作のみ、新規 download なし)。

## [0.7.5] - 2026-05-05

### Added

- **F-64**: `[transport.http].healthz_public` opt-in flag (default `true`,
  current behavior). Setting it to `false` places `/healthz` under the same
  `allowed_hosts` Host-header validation as `/mcp`, preventing kb-mcp
  fingerprinting from non-allowlisted hosts. `None` falls back to the rmcp
  default loopback list (`localhost` / `127.0.0.1` / `::1`); `Some([])`
  matches rmcp's `disable_allowed_hosts` (= allow any host, opt-out).

### Security

- **F-62**: `collect_source_files` (`kb-mcp index`) and `validate_collect_md_files`
  (`kb-mcp validate`) now always skip `.git`, `.svn`, and `node_modules`
  directories regardless of the user's `[indexer].exclude_dirs` config
  (union semantics). `DEFAULT_EXCLUDE_DIRS` already contains these entries
  for the section-absent case, but a user who overrides `exclude_dirs =
  ["custom"]` without re-listing VCS metadata would previously have
  `.git/HEAD` / `.git/config` indexed — leading to `.kb-mcp.db` bloat and
  retrieval noise. Watcher path (`is_under_excluded_dir`) is unaffected by
  design (extension filter rejects non-`.md` files).

### Documentation

- Document the implicit "stdout = data output, stderr = progress / status /
  diagnostics" CLI convention in `CLAUDE.md` and `docs/ARCHITECTURE.{md,ja.md}`.
  Surfaced by feature-36 / F-67 where a subprocess test failed because it
  grepped `Documents: 6` from stdout while `Commands::Status` emits to stderr.

### Internal

- **F-55**: Extracted 9 MCP / kb-mcp binary helpers (kb_mcp_bin /
  pick_free_port / wait_http_200 / spawn_mcp_server / ServerGuard /
  mcp_initialize / mcp_search_call / build_index /
  extract_path_heading_order) from `tests/search_mmr_integration.rs` and
  `tests/search_parent_integration.rs` into a shared
  `tests/common/mcp.rs` module. Each test file now imports them via
  `use common::mcp::...;`. Existing test bodies and `#[ignore]` attributes
  are byte-identical.
- **F-56**: Added `tests/fixtures/kb-small/` shared KB fixture (6 docs:
  ASCII + CJK + frontmatter rich / empty / none variants). New
  `tests/kb_small_smoke.rs` exercises the fixture end-to-end via
  `kb-mcp index` + `kb-mcp serve` (MCP HTTP transport), including a
  Japanese-CJK query smoke test.
- **F-58 / F-59**: CI infra — clippy 3-OS matrix in
  `.github/workflows/ci.yml` (replaces the single ubuntu-latest job
  with a `[ubuntu-latest, macos-latest, windows-latest]` matrix,
  `fail-fast: false`) and a nightly `cargo-llvm-cov` line-coverage
  job in `.github/workflows/nightly.yml` (uses
  `taiki-e/install-action@v2` for pre-built install,
  `--summary-only` output redirected to `$GITHUB_STEP_SUMMARY`).
  Source code unchanged.
- **F-67**: Fix `tests/kb_small_smoke.rs::test_kb_small_indexes_six_documents`
  to read from stderr instead of stdout when grepping `Documents: 6`
  in `kb-mcp status` output. The CLI uses `eprintln!` for all
  status/progress reporting (= consistent with `Commands::Index` and the
  rest of `Commands::Status`), reserving stdout for data output such
  as `kb-mcp search` JSON results. Surfaced by feature-35's first live
  nightly run; production behavior is unchanged.
- **F-57 / F-60残**: Watcher real-disk e2e test (`tests/watcher_e2e.rs`,
  `#[ignore]`-gated, Linux primary) and an index_throughput criterion
  bench (`benches/index_throughput.rs`). The test exercises
  notify-debouncer-full -> run_watch_loop -> indexer end-to-end via a
  new `spawn_mcp_server_with_watch` helper appended to
  `tests/common/mcp.rs`. The bench measures chunker throughput by
  default and chunker+embedder throughput under the `heavy-bench`
  feature gate (mirrors the existing `search_latency` reranker
  pattern). Source code unchanged.
- Add `tower 0.5` to `[dev-dependencies]` (with `util` feature) for the
  F-64 `/healthz` middleware unit tests (`ServiceExt::oneshot`). Release
  binary unaffected.

## [0.7.4] - 2026-05-04

### Fixed

- **`expand_adjacent` cap-exceeded invariant breach (F-51, #45)**:
  the cap-exceeded branch in `parent.rs::expand_adjacent` previously
  guarded `match_spans = None` clear and `expanded_from = Some(Adjacent
  {chunk_idx, chunk_idx})` set inside an `if let Some(c) = ...find(...)`
  block, so when the lookup failed (= rare DB inconsistency where the
  hit chunk's `chunk_index` is excluded from the fetched range) the
  hit was returned unchanged. Callers (`run_search_pipeline`) inspect
  `expanded_from` to decide whether to recompute `match_spans`, so the
  miss could leak stale offsets. Fix: keep `hit.content` overwrite
  inside the `if let Some` guard (defensive against undefined content),
  but apply `match_spans` clear and `expanded_from` set unconditionally
  to always notify callers of the cap-degrade event.

### Tests / Internal

- F-52: extracted `is_small_chunk(Option<i64>, u32) -> bool` helper from
  `expand_parent` and added proptest coverage for the strict-less-than
  boundary (`token == threshold` yields `is_small = false`) and the
  `None` arm.
- F-53: added `test_apply_parent_retriever_disabled_pass_through` to
  guard the `enabled=false` path's invariant that `content` /
  `expanded_from` / `match_spans` are unchanged.
- F-54: added `#[cfg(not(debug_assertions))]`-gated test
  `test_cosine_similarity_dim_mismatch_returns_zero_release_only` to
  document the release-build fail-safe (`debug_assert_eq!` is no-op,
  followed by an explicit length-mismatch / empty-input early-return to
  `0.0`). Exercised via `cargo test --release` (CI integration deferred
  to F-58 / F-59 CI infra bundle).

## [0.7.3] - 2026-05-03

### Security

- **`get_best_practice` hardening to `validate_get_document_path` parity (F-45, #44)**:
  the path resolver `resolve_best_practice_path` now applies the full
  4-stage defence (symlink reject / canonicalize+starts_with / extension
  membership / size cap) for each candidate template. Symlink hits
  return `Access denied: symlinks are not allowed.` immediately
  (security event, no template fallback); other rejections (file not
  found / outside-kb / extension denied / size exceeded) try the next
  template. `validate_get_document_path`'s return type is lifted to
  `ValidatePathOutcome { Found / NotFound(ErrorResponse) / Denied(ErrorResponse) }`
  with each fail variant carrying the original error wording verbatim,
  so existing `get_document` callers and 5 unit tests are
  byte-identical in behaviour. closes the audit-todos mid-term section.

## [0.7.2] - 2026-05-03

### Performance
- **MMR `cosine_similarity` SIMD kernel (F-42 reattempt, #43)**: replaced
  the scalar dot/norm with `wide::f32x8` (8-lane SIMD, pure-rust
  ~50 KB). On Coffee Lake (AVX2 + FMA) the criterion microbench
  shows **-53% on `pool=500/limit=50` (penalty=0.0/0.5)**, **-55%
  on `pool=100`**, **-76% on `pool=50`** vs the `pre-f42-reattempt`
  baseline. profile-first methodology revisited: partial profile
  (function symbols unresolvable in MSVC PDB) + structure analysis
  (cosine inner loop ops dominate HashMap by 50x) + bench AC gate.
  See `.dev/knowledge/bench-and-perf-investigation-pitfalls.md`
  trap 6 for the PDB-resolution fallback recipe. proptest 3 (incl.
  `prop_mmr_tie_break_stable` regression catcher) green; new unit
  tests guard NaN/Inf panic-only invariant and SIMD scalar-tail
  fallback for non-8-aligned dims.

## [0.7.1] - 2026-05-03

### Performance
- **Eliminate N+1 lookup in MMR pool builder (F-41)**: `SearchResult`
  now carries `document_id: i64` from the candidate SQLs
  (`search_vec_candidates` / `search_fts_candidates` /
  `chunks_for_path`), so the MMR pool builder no longer calls
  `lookup_document_id_by_path` per candidate. Side effect: the
  `unwrap_or(0)` rename-race collision (F-44) disappears with the
  helper. Internal API change only (`SearchResult` is not exposed
  by the MCP tool).
- **`mmr_select` API simplified (F-43)**: dropped the unused
  `_query_emb: &[f32]` argument carried for historical symmetry.
  Internal API change only; relevance source has been the hybrid
  RRF + reranker score since feature-28.
- **`token_count` saturate (F-46)**: replaced
  `(content.len() / 4) as i32` with
  `i32::try_from(...).unwrap_or(i32::MAX)`. Defense-in-depth for
  the hypothetical 8 GiB+ chunk path; behaviour unchanged in
  practice.

### Changed
- `kb-mcp search` / `kb-mcp eval`: `--mmr-lambda` and
  `--mmr-same-doc-penalty` values outside `[0.0, 1.0]` (and
  NaN / ±Inf) are now rejected at parse time (clap layer)
  instead of after embedding model load. This avoids a
  ~130MB / ~2.3GB model DL just to get an "out of range"
  error. Exit code becomes 2 (clap convention) instead of 1
  (anyhow). No effect on valid inputs. The existing
  helper-level guards (`run_search_pipeline` and the MCP
  tool boundary) continue to enforce the same range for
  non-CLI callers, so the runtime contract is unchanged.

### Internal
- **criterion bench infrastructure (F-60 partial)**: introduced
  `src/lib.rs` to expose internal modules (`kb_mcp::*`) to
  benches and integration tests. Added `benches/mmr_perf.rs`
  (MMR microbench, drives `kb_mcp::mmr::mmr_select` directly)
  and `benches/search_latency.rs` (subprocess wall-clock bench).
  Reranker-on bench is gated behind a `heavy-bench` Cargo
  feature to avoid a ~2.3 GB download on default
  `cargo bench` runs. Side effect: 4 functions in `src/server.rs`
  promoted from `pub(crate)` to `pub`
  (`compile_path_globs` / `run_search_pipeline` /
  `compute_match_spans` / `compute_low_confidence`), and
  `resolve_db_path` moved from `src/main.rs` to `src/lib.rs`
  (lib API is intentionally unstable).
- **MMR tie-break stability proptest** (`prop_mmr_tie_break_stable`):
  regression catcher for any future refactor to the greedy loop
  data structure. The Vec-bool variant of F-42 was investigated
  in this cycle but reverted (bench showed +5-8% regression on
  pool=500; cosine-similarity inner loop dominates). F-42 is
  deferred to a future cycle.
- Test coverage for the codex-review trap cluster surfaced
  during feature-28: added a proptest for
  `compute_low_confidence` order invariance (F-47), a
  boundary table + proptest for
  `Database::fetch_embeddings_by_chunk_ids` covering
  `EMBEDDING_FETCH_BATCH = 500` cycles (F-48), 4 unit tests
  for the new pure helper `compute_reranker_input_limit`
  including `usize::MAX → u32::MAX` saturate (F-49), and 3
  subprocess wire tests proving the new clap-level reject
  path (F-50). Test count: 393 → 400 unit + 3 new
  integration. No behavior change beyond the CLI early
  reject above. (Originally landed in PR #40 without a tag;
  this release ships it.)

## [0.7.0] - 2026-05-03

### Added
- MMR (Maximal Marginal Relevance) diversity re-rank stage
  (feature-28 PR-2). Greedy post-rerank picker that balances
  relevance against novelty:
  ```
  score = λ · rel(c) − (1 − λ) · max_sim(c, picked)
                     − same_doc_penalty · 1[doc(c) ∈ picked_docs]
  ```
  Configured via `[search.mmr]` in `kb-mcp.toml`
  (`enabled = false` default, `lambda = 0.7`,
  `same_doc_penalty = 0.0`) and per-call `mmr` /
  `mmr_lambda` / `mmr_same_doc_penalty` params on the `search`
  MCP tool. CLI: `kb-mcp search --mmr` /
  `--mmr-lambda` / `--mmr-same-doc-penalty`. Relevance scores
  (RRF or reranker) are min-max normalized to `[0, 1]` before
  combining with the cosine-similarity diversity term, so
  `lambda` is invariant to which prior stage produced the
  score. Kicks in only when the candidate pool is larger than
  `limit`; pulls extra candidates through stages 1–2 when
  enabled. Off by default: pre-v0.7.0 pipelines behave
  identically.
- Parent retriever display-time content expansion
  (feature-28 PR-3). For each hit chunk, optionally rewrites
  the returned `content` so the LLM gets enough surrounding
  context:
  - **Whole-document fallback** when
    `token_count < whole_doc_threshold_tokens` (default 100):
    return the entire document, capped at
    `max_expanded_tokens`.
  - **Adjacent-sibling merge** otherwise: merge the chunk
    immediately before / after the hit at the same heading
    level, until the merged block hits `max_expanded_tokens`
    (default 2000; BGE-M3 max is 8192).
  Score, rank, path, and `match_spans` of the original hit
  are preserved — only `content` and the new `expanded_from:
  Option<ExpandedRange>` field change. Configured via
  `[search.parent_retriever]` (`enabled = false` default) and
  per-call `parent_retriever` MCP param. CLI:
  `kb-mcp search --parent-retriever`. Legacy rows where
  `chunks.token_count IS NULL` use a `len(content) / 4` token
  estimate (matches the indexer's own estimator) so the cap
  is enforced even on databases predating `token_count`.
- `chunks.level` schema column (feature-28 PR-1) distinguishing
  h2 / h3 headings, with idempotent migration. Used by parent
  retriever's adjacent-sibling merge to avoid jumping across
  heading levels. Old rows have `level = NULL` (no upgrade
  required); the chunker populates the column for newly
  indexed content.
- `kb-mcp eval` accepts the same `--mmr` / `--mmr-lambda` /
  `--mmr-same-doc-penalty` / `--parent-retriever` flags as
  `kb-mcp search`, so retrieval-quality experiments can pin
  the full pipeline. `ConfigFingerprint` gains optional
  `mmr` / `parent_retriever` sub-fingerprints (additive —
  the JSON layout is forward-compatible with pre-v0.7.0
  history files; old runs deserialize without these
  fields).
- New narrative doc `docs/retrieval-pipeline.{md,ja.md}`
  describing the full
  `RRF → reranker → MMR → parent retriever → match_spans`
  pipeline with tuning advice for each stage.

### Changed (additive, MCP minor-compatible)
- `SearchHit` JSON schema gains an optional `expanded_from`
  field (`null` when parent retriever did not fire). Strict
  clients that use `deny_unknown_fields` need to know this
  field exists; default-tolerant clients are unaffected.
- `Reranker::rerank_candidates` is now a thin wrapper over
  the new chunk_id-preserving `rerank_candidates_with_ids`.
  Behavior of the public `rerank_candidates` entry-point is
  unchanged. `search_hybrid_candidates` body is refactored
  to share an `rrf_topk` helper with the unbounded variant
  used by the MMR pipeline; return shape is preserved and
  every existing caller keeps compiling without changes.

### Security
- Bounded the row count for parent retriever's whole-document
  fallback (`expand_whole_document` in `src/parent.rs`). Pre-fix,
  `Database::fetch_chunks_by_index_range` had no `LIMIT` and
  loaded every chunk of the target document into a `Vec<ChunkRow>`
  before the `max_expanded_tokens` cap was checked. A pathological
  document (e.g. a single very large `.md` file) could therefore
  spike memory before the cap engaged. Fix: `fetch_chunks_by_index_range`
  now requires a `max_rows` parameter (`LIMIT` clause), and the
  whole-doc path derives `row_cap = max_expanded_tokens × 2 + 64`
  before fetching; if the cap is reached, the call falls back to
  adjacent merge. Closes the 2026-05-03 audit Sec H-1+H-3 finding.

### Fixed
- `parent.rs::expand_adjacent` / `expand_whole_document`: the
  `max_expanded_tokens` cap accumulator is now `u64` instead of
  `u32`, eliminating a theoretical wrap-around path where
  successive very large chunks could sum past `u32::MAX` and
  silently bypass the cap. Realistic KBs do not hit this; this is
  defense-in-depth so the cap remains correct under adversarial
  content sizes. Closes the 2026-05-03 audit Code C2 finding.
- `docs/retrieval-pipeline.{md,ja.md}`: corrected Stage 2 (reranker)
  candidate-pool description. Pre-fix said the pool grows when
  "MMR or parent retriever" is enabled; in fact only MMR enlarges
  the pool. Parent retriever is a content-only stage that runs on
  already-selected hits and never changes reranker workload.
  Caught by codex review on PR #38.
- `docs/eval.{md,ja.md}`: CLI flag list now includes the v0.7.0
  pipeline flags (`--mmr` / `--mmr-lambda` /
  `--mmr-same-doc-penalty` / `--parent-retriever`) and `--limit`
  (which was always supported but undocumented). The
  `--fail-on-regression` fingerprint description now lists the
  v0.7.0 additions (`mmr` / `parent_retriever`); toggling either
  intentionally breaks fingerprint compatibility.
- `docs/citations.{md,ja.md}`: added a v0.7.0+ note that when
  parent retriever fires, `match_spans` are byte offsets into the
  expanded `content`, not the original chunk. The `expanded_from`
  field on the same hit indicates the merged range.
- `CONTRIBUTING.{md,ja.md}`: repository layout list now includes
  `src/mmr.rs`, `src/parent.rs`, `src/eval.rs`, and `src/config.rs`.
- `kb-mcp.toml.example`: `[search.mmr]` / `[search.parent_retriever]`
  section comments rewritten to make the "header present, all keys
  commented = built-in defaults" semantics explicit. The behavior
  is unchanged from the v0.6.x layout; this is a clarification only.
- `src/server.rs` MCP `search` tool docstrings for the new MMR /
  parent retriever per-call params (`mmr` / `mmr_lambda` /
  `mmr_same_doc_penalty` / `parent_retriever`) are now in English,
  matching the rest of the schema. The Japanese-only docstrings
  were leaking into MCP client schema output for non-Japanese
  consumers.
- `examples/deployments/personal-http/kb-mcp-task.xml`:
  `RestartOnFailure.Interval` was set to `PT5S` (5 seconds), but
  Windows Task Scheduler rejects anything below `PT1M` at registration
  time with "value not allowed or out of range". Bumped to `PT1M`
  with an inline comment explaining the constraint. Found while
  walking through the recipe on a real Windows install.
- `examples/deployments/personal-http/README.{md,ja.md}`:
  added a `Register-ScheduledTask` (PowerShell) flow as the
  **recommended** Windows install path. The legacy
  `schtasks /Create /XML` flow is kept as the alternative because
  it can fail with a misleading "Access denied" even on AT_LOGON
  tasks in the user's own namespace (Principal-resolution quirk
  in the legacy implementation). Same end result, no admin needed
  in either path.

### Documentation
- Doc-sync sweep (post-v0.6.1, found while auditing the doc tree
  against recent feature merges):
  - `CLAUDE.md`: the subcommand listing was missing `eval`
    (added in v0.2.0). Restored to `index / status / serve /
    search / graph / validate / eval`. ARCHITECTURE.md and
    README already had it.
  - `README.md`: input-bounds note in the search section had
    `(defensive, v0.5.1+)` (a forward-looking marker that
    pre-dated the actual landing in v0.6.0). Pinned to
    `(defensive, v0.6.0+)` to match what shipped. The Japanese
    side was correct already.
  - `README.{md,ja.md}`: the eval section now mentions
    `--fail-on-regression` (v0.6.0+) with the
    fingerprint-compatibility one-liner. Detail still lives in
    `docs/eval.{md,ja.md}` — just one extra line each in the
    README so users grepping for "fail-on-regression" land
    somewhere informative.
- New `examples/deployments/personal-http/` recipe (closes
  feature-ideas.md H-8). Targets the case where a single user
  opens multiple Claude Code / Cursor sessions in parallel on
  one machine — the stdio recipe spawns one kb-mcp child per
  session (peak RAM = N × ~2.3 GB on BGE-M3, plus N file
  watchers on the same dir, plus DB writer contention if one
  session does `index --force`). The new recipe runs **one**
  daemon as a loopback HTTP service on `127.0.0.1:3100`; every
  session connects via Streamable HTTP, so one embedder + one
  DB + one watcher regardless of session count. Ships with a
  loopback-only `kb-mcp.toml`, a client-side `.mcp.json`
  template, and OS launcher units for all three platforms
  (Linux systemd **user** unit, macOS launchd LaunchAgent,
  Windows Task Scheduler XML). Selection guide at
  `examples/deployments/README{,.ja}.md` updated 3 patterns →
  4 patterns; main README en+ja updated to match.

## [0.6.1] - 2026-05-02

### Internal
- Bumped GitHub Actions to Node.js 24-runtime versions ahead
  of the 2026-06-02 default cutover (where the runner forces
  Node.js 24 on actions still pinned to Node.js 20):
  - `actions/checkout@v5` → `@v6` in `ci.yml` and
    `nightly.yml` (`release.yml` was already on `@v6`).
  - `actions/cache@v4` → `@v5` in `nightly.yml` — this is
    the action that was actively emitting the deprecation
    annotation on every nightly run.
  - `Swatinem/rust-cache@v2` (floating) needs no change —
    upstream landed `node24` in v2.9.0 and the major-tag
    pin auto-tracks it.
  - `dtolnay/rust-toolchain@stable` is a composite action
    (no JS runtime), so the Node.js deprecation does not
    apply.
  Cuts the deprecation warn surface to zero while staying
  on standard major-tag pins for everything that still
  supports the convention.
- Added criterion benchmark infrastructure under `benches/`
  (F-39 part 2). `criterion = "0.5"` with `default-features =
  false` (skips the rayon-driven HTML report machinery to
  shave first-build compile time). The first bench file,
  `benches/string_ops.rs`, measures `to_ascii_lowercase` on
  a 4 KiB ASCII chunk and on an empty string — representative
  of `compute_match_spans`'s inner loop and a stable baseline
  for spotting hot-path regressions in the stdlib / compiler.
  Real index-throughput and search-latency benches are
  deferred to a follow-up because kb-mcp is a binary crate
  with no `[lib]` target; bridging that requires either
  promoting a sliver of the crate to `[lib]` or driving the
  released binary as a subprocess. Both are out of scope for
  this PR — the goal here is to prove the harness wires up and
  give future benches a copy-paste pattern.
- Added `tests/common/` shared module (F-39 part 1). New
  integration tests can `mod common;` and reuse
  `common::temp::TempRoot` (flat scratch dir) and
  `common::temp::TempKbLayout` (`root/kb/` two-level layout
  for tests where the kb-mcp DB sibling needs to be reaped on
  Drop). Replaces seven hand-rolled `TempKb` / `TempDir`
  structs scattered across the existing integration tests —
  per the audit note, those existing tests are intentionally
  *not* rewritten in this PR (additive only). `tests/common_helpers.rs`
  is the entry-point test crate that fires the 5 inline unit
  tests of the helpers themselves.

## [0.6.0] - 2026-04-30

### Security
- Hardened MCP `search` tool input boundaries (F-35):
  - `query` is now capped at 1 KiB. Larger queries are rejected with
    a clear `ErrorResponse` instead of being silently truncated by
    the embedder / FTS5 layer downstream. This makes response shape
    predictable and removes a `query × content` O(N×M) cost vector
    from `compute_match_spans`.
  - `compute_match_spans` skips content larger than 256 KiB
    (`None` return) — typical chunks are heading-sized (a few KiB),
    but a malformed indexer state could expose pathological chunks.
  - `compute_match_spans` caps the returned span count at 100 per
    chunk. A query like `"a"` against a long string used to return
    one span per occurrence; now the count saturates so the JSON
    response stays bounded.

  These limits are constants (`SEARCH_QUERY_MAX_BYTES`,
  `MATCH_SPAN_CONTENT_MAX_BYTES`, `MATCH_SPAN_MAX_COUNT` in
  `src/server.rs`) and are not configurable today — they exist to
  bound *abuse*, not legitimate use. The 1 KiB query cap matches
  the typical MCP client embedding budget; chunks that legitimately
  hit the 256 KiB ceiling are already over the FTS / embedding
  practical horizon.

### Added
- `kb-mcp eval --fail-on-regression` (F-40). Exit with code 1 if
  any aggregate metric (`recall@k` for any k, `MRR`, or `ndcg@k`
  for any k) regressed from the previous **compatible** run by
  more than `regression_threshold` (default 0.05, set via
  `[eval].regression_threshold` in `kb-mcp.toml`). "Compatible"
  means the previous run shares the same fingerprint (model /
  reranker / limit / k_values / golden_hash), so updating the
  golden YAML does *not* spuriously trigger a regression — the
  comparison is just skipped on the next run. History is still
  written before the process exits, so the new run is recorded
  for the *next* comparison. The flag is a no-op when there is
  no previous run, when `--no-history` / `--no-diff` is set, or
  when fingerprints differ. Closes the F-38 follow-up scope split
  out for "eval regression detection in CI".

### Internal
- Watcher backpressure (F-36): replaced
  `tokio::sync::mpsc::unbounded_channel` with
  `mpsc::channel(64)` for the bridge between
  `notify-debouncer-full` (std thread) and the tokio
  consumer task. The debouncer callback now uses
  `try_send`; on `Full` it logs a warn and drops the
  batch instead of growing the queue without bound. This
  caps watcher RAM usage at "64 batches" regardless of
  how fast the filesystem fires events, and turns "watcher
  is silently lagging" into a visible log line. Closes the
  audit-flagged "unbounded watcher channel" cross-cutting
  issue. Adaptive debounce / path-level coalescing remain
  out of scope for this PR (notify-debouncer-full does not
  expose a runtime debounce-window setter, and per-path
  coalescing is already done by the debouncer itself).
- Added `.github/workflows/nightly.yml` (F-38). Runs daily at UTC
  04:00 (and on `workflow_dispatch`) with two jobs:
  - `ignored-tests`: `cargo test -- --include-ignored` on
    `ubuntu-latest` with `~/.cache/fastembed` cached via
    `actions/cache@v4` so the BGE-small / BGE-M3 / BGE-reranker-v2-m3
    downloads are paid once. Catches regressions in the model-DL
    test path (`embedder` / `reranker` / `tests/eval_cli.rs` /
    `tests/http_transport.rs` / `tests/search_cli.rs`) that the
    fast `cargo test` lane on PRs cannot exercise.
  - `cargo-audit`: installs `cargo-audit` and runs it against the
    dep tree, so a fresh RustSec advisory becomes a job failure
    (notification surface). Distinct lane so a temporarily-flaky
    advisory does not block the ignored-tests run.
  - `eval` regression detection (`kb-mcp eval --fail-on-regression`)
    is split out — that flag does not exist yet and is tracked
    separately from F-38's CI scope.

## [0.5.0] - 2026-04-29

### Security
- HTTP transport: surfaced `[transport.http].allowed_hosts` in
  `kb-mcp.toml` so operators can extend the inbound `Host` header
  allow-list past rmcp's default loopback-only set
  (`["localhost", "127.0.0.1", "::1"]`) without dropping to
  `disable_allowed_hosts`. Use this for LAN / intranet exposure
  (`allowed_hosts = ["kb.example.lan", "192.168.1.10"]`); a `[]`
  empty array still disables the check entirely (operator-acknowledged
  opt-out). Additionally, kb-mcp now emits a `tracing::warn` at
  startup when the bind address is non-loopback **and**
  `allowed_hosts` is unset — a near-certain misconfiguration where
  external requests would otherwise be silently 403'd by Host
  validation. Closes F-33 from the 2026-04-29 audit.

### Internal
- Hardened DB transaction protection across the three write paths flagged
  by the 2026-04-29 audit (F-32):
  - `Database::upsert_document` now wraps the UPDATE branch's four
    statements (DELETE vec_chunks / DELETE fts_chunks / DELETE chunks /
    UPDATE documents) in an autocommit-aware tx via
    `Connection::unchecked_transaction()`. A failure on any of the four
    statements no longer leaves dangling vec / FTS rows whose `chunks`
    parent has already been removed.
  - `Database::insert_chunk` likewise wraps its three INSERTs (chunks +
    vec_chunks + fts_chunks) so a partial failure (e.g. embedding-dim
    mismatch on the `vec_chunks` insert) cannot leave a chunk visible to
    one search backend but invisible to the other.
  - `Database::rename_documents_atomic` replaces the manual
    `BEGIN`/`COMMIT`/`ROLLBACK` pair with `unchecked_transaction()` so
    that any `?` early-return path is rolled back by the `Transaction`
    Drop guard rather than relying on an explicit `ROLLBACK` call.
  - `indexer::index_single_disk_entry` now wraps `upsert_document`
    plus the per-chunk `insert_chunk` loop in a single tx via the new
    `Database::begin_transaction()` handle — embedding inference still
    runs *outside* the tx so a long-lived write tx does not block
    concurrent WAL readers. A partial failure mid-loop now rolls the
    whole file back instead of leaving a documents row paired with
    M < N chunks. Two regression tests
    (`test_begin_transaction_rolls_back_partial_writes_on_drop`,
    `test_begin_transaction_commits_on_explicit_commit`) lock down the
    Drop-rollback / commit symmetry.
- Added `proptest` 1 as a dev-dependency and locked the f64 value-range
  invariants of the retrieval-quality metrics: `recall_at_k`,
  `ndcg_at_k`, `reciprocal_rank`, and `chunk_quality_score` are now
  property-tested over randomized inputs to ensure each result is
  finite and in `[0.0, 1.0]`. This is a permanent guard against the
  v0.4.2 nDCG > 1.0 class of regression — any future change that lets
  one of these metrics escape the unit range will fail `cargo test`
  before it can ship.
- Migrated YAML parsing from `serde_yaml` 0.9 (deprecated and
  unmaintained — alias-bomb guards rely on the upstream limits in
  `unsafe-libyaml`) to `serde_yaml_bw` 2 ("YAML support for Serde
  with an emphasis on panic-free parsing"). Frontmatter (`Markdown`
  parser) and golden-YAML loading (`kb-mcp eval`) both move to the
  new crate. The `Value` enum gains a tag field so the only API
  delta is the pattern in the `RawFrontmatter` -> `Frontmatter`
  conversion (`Value::String(s, _)`, `Value::Number(n, _)`).
  Adds a smoke regression test that a YAML alias bomb does not
  panic the parser.

## [0.4.3] - 2026-04-29

### Security
- `get_document` MCP tool now rejects symlinks, restricts the file
  extension to the registered parser set, and caps file size at 1 MiB.
  Closes a pre-existing read primitive whereby a connected MCP client
  could call `get_document {path: ".git/config"}` (or any other
  non-indexed file under `kb_path`, including paths under
  `exclude_dirs`) and have the server return its contents — the prior
  defense was only a `kb_path`-prefix check on the canonicalized path,
  which is necessary but not sufficient because `canonicalize` resolves
  symlinks and the prefix check does not enforce the indexer's own
  scoping (extension whitelist, dir exclusions). The size cap mitigates
  a trivial RAM-OOM where one request reads a multi-GB file into a
  string buffer.

### Fixed
- `kb-mcp eval` becomes more robust against non-finite f64 values:
  - `reciprocal_rank` guards rank==0 → returns `0.0` (was `1.0/0.0
    = inf`, poisoning aggregate MRR; warn-logged when triggered).
  - `format_json` no longer panics on a previous `EvalRun` whose
    serialization fails (e.g. NaN/Inf survived from older history).
- `min_quality` and `min_confidence_ratio` MCP search params now
  reject NaN / ±Inf and fall back to the configured server defaults.
  Previously NaN flowed through `clamp(0.0, 1.0)` unchanged (NaN
  comparisons are all false), silently disabling the quality filter
  or low-confidence judgment depending on the path.
- `list_topics` MCP tool no longer fragments titles that contain the
  substring `||`. The aggregator now uses `json_group_array(title)`
  instead of `GROUP_CONCAT(title, '||') + .split("||")`.

### Documentation
- `examples/deployments/{personal,nas-shared,intranet-http}/.mcp.json`
  now set `"alwaysLoad": true` on the kb-mcp server entry. This is a
  Claude Code v2.1.121+ option that forces kb-mcp's tools to be present
  at initial load instead of going through the tool-search shortlist —
  appropriate for the "search anytime" RAG use case. Other MCP clients
  (Cursor, etc.) ignore the field. Each recipe README (en+ja) gains a
  note covering when to keep it on vs drop it (initial-startup latency
  trade-off, especially relevant for NAS-mounted KBs).
- Audit-driven docs cleanup (en+ja):
  - Fixed broken `serve` example code block in both READMEs
    (line continuation collapsed onto one line, fence didn't close).
  - `kb-mcp search --format json` examples now use `jq '.results[]'`
    against the v0.3.0+ wrapper shape instead of the obsolete `jq '.[]'`
    pattern; section description aligned with the wrapper documentation.
  - Removed six dead anchor links (`#...feature-NN`) left over from the
    v0.1.0 internal-marker stripping campaign.
  - Removed remaining internal feature markers (`F18-11`, `feature 26`,
    `Pre-feature-17`, `feature-26`) from `kb-mcp.toml.example`,
    `README.md`, `docs/ARCHITECTURE.md` (en+ja).
  - `examples/deployments/intranet-http/`: cache directory comment in
    `kb-mcp.toml` corrected (the systemd unit does not create or chown
    `/var/cache/fastembed`); README setup adds an explicit step to
    `install -d -o kbmcp -g kbmcp /var/cache/fastembed` before first run.
  - `kb-mcp index` description now lists the full default `exclude_dirs`
    set instead of just `.obsidian/`.
  - `kb-mcp validate --strict` documented as a no-op accepted for
    forward compatibility.
  - Fixed redundant "by default ... (the default behavior)" stutter in
    en+ja `index` description.

## [0.4.2] - 2026-04-27

### Fixed
- `kb-mcp eval` no longer reports `nDCG@k > 1.0`. The previous DCG loop
  iterated `top` and counted any hit that matched at least one expected
  entry, which over-counted gains when several chunks of the same doc
  (e.g. different headings under one path-only `expected`) appeared in
  top-k. The fix iterates `expected` and uses each entry's first matching
  rank exactly once, restoring the standard `[0, 1]` value range. Recall
  and MRR were not affected. Existing `.kb-mcp-eval-history.json` files
  still load, but historic `nDCG@k` values are not comparable across the
  fix boundary — re-run `kb-mcp eval` to establish a fresh baseline.

## [0.4.1] - 2026-04-26

### Internal
- Added `cargo-dist` 0.31 setup for cross-platform binary releases. From
  this release onwards, GitHub Releases include prebuilt archives for
  Linux x86_64 / aarch64, macOS aarch64 (Apple Silicon), and Windows
  x86_64, plus per-archive SHA-256 sums and a global `sha256.sum`.
  ONNX Runtime and SQLite are statically linked, so the archives ship a
  single binary with no extra DLLs. Intel Mac (`x86_64-apple-darwin`)
  is **not** shipped because `ort-sys` has no prebuilt for that target —
  build from source if needed.
- Linux binaries require **glibc 2.38+** (Ubuntu 24.04+ / Debian 13+ /
  RHEL 9.5+). The `ort-sys` prebuilt references `__isoc23_*` symbols
  introduced in that release.
- Windows binaries link against the dynamic UCRT (ucrtbase.dll /
  vcruntime140.dll, shipped with Windows 10+); cargo-dist's default
  `msvc-crt-static = true` is overridden because `libcmt` conflicts
  with `ort-sys`'s prebuilt.
- README en+ja gain an `Install` section describing the prebuilt
  archives; the existing `cargo build --release` instructions are
  demoted to a `Build from source` subsection.

## [0.4.0] - 2026-04-26

### Added
- `--config <PATH>` global CLI flag for selecting an arbitrary `kb-mcp.toml`.
  `~` is expanded on all platforms. Missing path errors fast (no fallback).
- Discovery now checks `./kb-mcp.toml` (CWD) first, then walks up to 19
  `.git` ancestor levels for a project-root `kb-mcp.toml`, before falling
  back to the legacy binary-side location.

### Changed
- `kb_mcp::config: loaded config source=...` is logged to stderr at startup
  so the active config file is observable. `tracing-subscriber` now uses
  the `env-filter` feature so `RUST_LOG` is honored (default = `info`).

### Compatibility
- Fully back-compat: the binary-side `kb-mcp.toml` (`<exe-dir>/kb-mcp.toml`)
  is still picked up when no higher-priority source is present.

### Internal
- `.githooks/pre-push` enforces `cargo fmt --check` before push so a
  forgotten `cargo fmt` cannot reach CI. Opt-in once via
  `git config core.hooksPath .githooks` (see CONTRIBUTING.md).

## [0.3.0] - 2026-04-26

### Added

- `search` tool now returns `match_spans` (byte offsets) for ASCII queries,
  helping clients quote source text accurately. See `docs/citations.md`.
- `search` tool gained new filters: `path_globs` (glob with `!`-prefixed
  excludes), `tags_any` (OR), `tags_all` (AND), `date_from` / `date_to`
  (lex comparison; date-missing chunks excluded strictly). See `docs/filters.md`.
- `search` response includes a `low_confidence` flag based on a rank-based
  ratio (`top1.score / mean(top-N.score) < min_confidence_ratio`). The threshold
  defaults to `1.5` and can be configured via `[search].min_confidence_ratio`
  in `kb-mcp.toml` or via `--min-confidence-ratio` / `min_confidence_ratio` per
  query.
- `tags` field is now included in each `SearchHit`.
- CLI `kb-mcp search` accepts `--path-glob`, `--tag-any`, `--tag-all`,
  `--date-from`, `--date-to`, `--min-confidence-ratio`.
- `[search]` section in `kb-mcp.toml`.

### Changed (BREAKING)

- The `search` MCP tool now returns a wrapper object
  `{ results, low_confidence, filter_applied }` instead of a raw array of hits.
  Clients that parse the response as `Vec<SearchHit>` directly must be updated.
  CLI `kb-mcp search --format json` follows the same wrapper format.
- Internal `db::search_hybrid` / `db::search_hybrid_candidates` /
  `db::search_vec_candidates` / `db::search_fts_candidates` /
  `db::search_similar` now take a `&SearchFilters<'_>` instead of separate
  `category` / `topic` / `min_quality` arguments. Library consumers (rare
  outside this repo) must migrate.

## [0.2.0] - 2026-04-24

### Added

- `kb-mcp eval` subcommand for retrieval quality evaluation (opt-in power-user feature).
  Runs a golden query set through `search_hybrid` and reports recall@k / MRR / nDCG@k.
  Shows diffs against the previous run. Details: `docs/eval.md` / `docs/eval.ja.md`.

### Internal

- CI (GitHub Actions) upgraded to `actions/checkout@v5` to clear Node.js 20 deprecation warnings

## [0.1.0] - 2026-04-20

First public release. An MCP server providing semantic hybrid search (sqlite-vec + FTS5 via Reciprocal Rank Fusion, with optional cross-encoder reranking) over a Markdown / plain-text knowledge base. Supports stdio and Streamable HTTP transports, includes a live-sync file watcher, and ships with optional frontmatter schema validation via the `kb-mcp validate` CLI.

### Added

- Dual-licensed under **MIT OR Apache-2.0** ([`LICENSE-MIT`](./LICENSE-MIT), [`LICENSE-APACHE`](./LICENSE-APACHE))
- `docs/ARCHITECTURE.md` / `docs/ARCHITECTURE.ja.md` describing source layout, data flow, embedding cache resolution, and key dependencies
- `CONTRIBUTING.md` / `CONTRIBUTING.ja.md` with build / test / code-style instructions
- Bilingual `README.md` (English primary) and `README.ja.md` (Japanese) with cross-links
- `.mcp.json.example` template alongside `.gitignore`'d user-local `.mcp.json`
- `exclude_dirs` config key for directory-level exclusion during indexing (defaults to `.obsidian`, `.git`, `node_modules`, `target`, `.vscode`, `.idea`)
- `Cargo.toml` metadata (description / license / repository / keywords / categories) for crates.io publishing

### Changed

- `exclude_headings` default neutralized from `["次の深堀り候補"]` to `[]` (opt-in by populating the key in `kb-mcp.toml`)
- `get_best_practice` MCP tool is now **opt-in**: requires `[best_practice].path_templates` in `kb-mcp.toml`; otherwise returns a `not configured` error
- `.obsidian/` skip is no longer hardcoded — it is now part of the configurable `exclude_dirs` default list

### Documentation

- Stripped internal feature tracking markers (`[feature N]`, `pre-feature-N`, `F12-N`, etc.) from all public docs and source comments
- Split `CLAUDE.md` into a slim public version and a private `CLAUDE.local.md` (gitignored) for harness-kit / project-history notes
- `README` feature-number references removed in favor of behavior-based descriptions

### Internal

- 207 unit / integration tests + 5 validate-CLI tests pass
- `cargo fmt` / `cargo clippy --all-targets` clean
- Personal dev artifacts moved to `.dev/` (excluded via `.git/info/exclude`)

[Unreleased]: https://github.com/alphabet-h/grooveseek/compare/v1.0.0...HEAD
[1.0.0]: https://github.com/alphabet-h/grooveseek/compare/v0.27.0...v1.0.0
[0.27.0]: https://github.com/alphabet-h/grooveseek/compare/v0.26.0...v0.27.0
[0.26.0]: https://github.com/alphabet-h/grooveseek/compare/v0.25.0...v0.26.0
[0.25.0]: https://github.com/alphabet-h/grooveseek/compare/v0.24.0...v0.25.0
[0.24.0]: https://github.com/alphabet-h/grooveseek/compare/v0.23.0...v0.24.0
[0.23.0]: https://github.com/alphabet-h/grooveseek/compare/v0.22.0...v0.23.0
[0.22.0]: https://github.com/alphabet-h/grooveseek/compare/v0.21.0...v0.22.0
[0.21.0]: https://github.com/alphabet-h/grooveseek/compare/v0.20.0...v0.21.0
[0.20.0]: https://github.com/alphabet-h/grooveseek/compare/v0.19.0...v0.20.0
[0.19.0]: https://github.com/alphabet-h/grooveseek/compare/v0.18.0...v0.19.0
[0.18.0]: https://github.com/alphabet-h/grooveseek/compare/v0.17.0...v0.18.0
[0.17.0]: https://github.com/alphabet-h/grooveseek/compare/v0.16.0...v0.17.0
[0.16.0]: https://github.com/alphabet-h/grooveseek/compare/v0.15.2...v0.16.0
[0.15.2]: https://github.com/alphabet-h/grooveseek/compare/v0.15.1...v0.15.2
[0.15.1]: https://github.com/alphabet-h/grooveseek/compare/v0.15.0...v0.15.1
[0.15.0]: https://github.com/alphabet-h/grooveseek/compare/v0.14.0...v0.15.0
[0.14.0]: https://github.com/alphabet-h/grooveseek/compare/v0.13.1...v0.14.0
[0.13.1]: https://github.com/alphabet-h/grooveseek/compare/v0.13.0...v0.13.1
[0.13.0]: https://github.com/alphabet-h/grooveseek/compare/v0.12.0...v0.13.0
[0.12.0]: https://github.com/alphabet-h/grooveseek/compare/v0.11.0...v0.12.0
[0.11.0]: https://github.com/alphabet-h/grooveseek/compare/v0.10.0...v0.11.0
[0.10.0]: https://github.com/alphabet-h/grooveseek/compare/v0.9.2...v0.10.0
[0.9.2]: https://github.com/alphabet-h/grooveseek/compare/v0.9.1...v0.9.2
[0.9.1]: https://github.com/alphabet-h/grooveseek/compare/v0.9.0...v0.9.1
[0.9.0]: https://github.com/alphabet-h/grooveseek/compare/v0.8.3...v0.9.0
[0.8.3]: https://github.com/alphabet-h/grooveseek/compare/v0.8.2...v0.8.3
[0.8.2]: https://github.com/alphabet-h/grooveseek/compare/v0.8.1...v0.8.2
[0.8.1]: https://github.com/alphabet-h/grooveseek/compare/v0.8.0...v0.8.1
[0.8.0]: https://github.com/alphabet-h/grooveseek/compare/v0.7.8...v0.8.0
[0.7.8]: https://github.com/alphabet-h/grooveseek/compare/v0.7.7...v0.7.8
[0.7.7]: https://github.com/alphabet-h/grooveseek/compare/v0.7.6...v0.7.7
[0.7.6]: https://github.com/alphabet-h/grooveseek/compare/v0.7.5...v0.7.6
[0.7.5]: https://github.com/alphabet-h/grooveseek/compare/v0.7.4...v0.7.5
[0.7.4]: https://github.com/alphabet-h/grooveseek/compare/v0.7.3...v0.7.4
[0.7.3]: https://github.com/alphabet-h/grooveseek/compare/v0.7.2...v0.7.3
[0.7.2]: https://github.com/alphabet-h/grooveseek/compare/v0.7.1...v0.7.2
[0.7.1]: https://github.com/alphabet-h/grooveseek/compare/v0.7.0...v0.7.1
[0.7.0]: https://github.com/alphabet-h/grooveseek/compare/v0.6.1...v0.7.0
[0.6.1]: https://github.com/alphabet-h/grooveseek/compare/v0.6.0...v0.6.1
[0.6.0]: https://github.com/alphabet-h/grooveseek/compare/v0.5.0...v0.6.0
[0.5.0]: https://github.com/alphabet-h/grooveseek/compare/v0.4.3...v0.5.0
[0.4.3]: https://github.com/alphabet-h/grooveseek/compare/v0.4.2...v0.4.3
[0.4.2]: https://github.com/alphabet-h/grooveseek/compare/v0.4.1...v0.4.2
[0.4.1]: https://github.com/alphabet-h/grooveseek/compare/v0.4.0...v0.4.1
[0.4.0]: https://github.com/alphabet-h/grooveseek/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/alphabet-h/grooveseek/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/alphabet-h/grooveseek/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/alphabet-h/grooveseek/releases/tag/v0.1.0
