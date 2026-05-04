# Changelog

All notable changes to kb-mcp are documented here. The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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

## [0.7.3] - 2026-05-04

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

## [0.7.2] - 2026-05-04

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

## [0.6.1] - 2026-05-01

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

[Unreleased]: https://github.com/alphabet-h/kb-mcp/compare/v0.7.4...HEAD
[0.7.4]: https://github.com/alphabet-h/kb-mcp/compare/v0.7.3...v0.7.4
[0.7.3]: https://github.com/alphabet-h/kb-mcp/compare/v0.7.2...v0.7.3
[0.7.2]: https://github.com/alphabet-h/kb-mcp/compare/v0.7.1...v0.7.2
[0.7.1]: https://github.com/alphabet-h/kb-mcp/compare/v0.7.0...v0.7.1
[0.7.0]: https://github.com/alphabet-h/kb-mcp/compare/v0.6.1...v0.7.0
[0.6.1]: https://github.com/alphabet-h/kb-mcp/compare/v0.6.0...v0.6.1
[0.6.0]: https://github.com/alphabet-h/kb-mcp/compare/v0.5.0...v0.6.0
[0.5.0]: https://github.com/alphabet-h/kb-mcp/compare/v0.4.3...v0.5.0
[0.4.3]: https://github.com/alphabet-h/kb-mcp/compare/v0.4.2...v0.4.3
[0.4.2]: https://github.com/alphabet-h/kb-mcp/compare/v0.4.1...v0.4.2
[0.4.1]: https://github.com/alphabet-h/kb-mcp/compare/v0.4.0...v0.4.1
[0.4.0]: https://github.com/alphabet-h/kb-mcp/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/alphabet-h/kb-mcp/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/alphabet-h/kb-mcp/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/alphabet-h/kb-mcp/releases/tag/v0.1.0
