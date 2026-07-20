//! Integration tests for feature-46 PR-2 (contextual retrieval).
//!
//! Covers the plan §7 scenarios plus two controller follow-ups from the
//! Task 2.5 / Task 2.7 per-task reviews:
//!
//! 1. `[contextual].enabled = true` (default): index -> search hits via the
//!    injected ancestry context, `status` reports `Context mode: static`.
//! 2. An `off`-mode DB, flipping the config to `enabled = true` without
//!    `--force`, must **not** silently switch modes mid-index (would build
//!    a mixed embedding space) -- it stays in the DB-stored mode and warns
//!    on stderr. `--force` is required to actually migrate.
//! 3. A genuine pre-feature-46 (v0.11.0) DB -- 2-column FTS, `chunks`
//!    without `context_text` -- migrates cleanly on open, and a real
//!    search against the migrated schema does not hit the "wrong number of
//!    arguments to function bm25()" trap a stale 2-column schema would
//!    produce.
//! 4. (controller follow-up, E-8) Static-mode context is title-derived: a
//!    frontmatter-only change to `title` must NOT take the frontmatter-only
//!    skip fast path (would leave a stale title baked into `context_text`);
//!    a frontmatter-only change to unrelated fields (e.g. `tags`, title
//!    unchanged) must still take the fast path (no re-embed).
//! 5. (controller follow-up) `kb-mcp status` displays `Context mode: ...`
//!    and the config/DB mismatch warning lands on stderr. The `status`
//!    display half needs no embedding model at all (`Commands::Status`
//!    never touches the embedder) and runs as a normal, non-`#[ignore]`
//!    test against a hand-built DB file; the warning-text half is only
//!    reachable via `kb-mcp index`, so it piggybacks on scenario 2's
//!    `#[ignore]` test instead of adding a second model-requiring test.
//!
//! All tests that call `kb-mcp index` or `kb-mcp search` are `#[ignore]`
//! because they load the BGE-small embedding model (~130 MB on a cold
//! cache) -- same policy as `tests/binary_formats_cli.rs`. Run with:
//! ```text
//! cargo test --test contextual_retrieval_cli -- --ignored
//! ```
//! `test_status_shows_context_mode_static_without_model_download` and
//! `test_legacy_v0_11_0_db_status_migrates_without_model_download` are the
//! two exceptions and run under plain `cargo test`.

use std::path::Path;
use std::process::Command;

mod common;
use common::ansi::strip_ansi;
use common::mcp::kb_mcp_bin;
use common::temp::TempKbLayout;

// ---------------------------------------------------------------------------
// Subprocess helpers (pattern from tests/binary_formats_cli.rs)
// ---------------------------------------------------------------------------

/// Run `kb-mcp --config <cfg> index --kb-path <kb> [--force]`, asserting
/// exit 0, and return stripped stderr (index progress/warnings are a
/// stderr-only concern per CLAUDE.md's CLI output convention).
fn run_index(bin: &Path, cfg: &Path, kb: &Path, force: bool) -> String {
    let mut args = vec![
        "--config".to_string(),
        cfg.to_string_lossy().into_owned(),
        "index".to_string(),
        "--kb-path".to_string(),
        kb.display().to_string(),
    ];
    if force {
        args.push("--force".to_string());
    }
    let out = Command::new(bin)
        .args(&args)
        .output()
        .expect("kb-mcp index");
    assert!(
        out.status.success(),
        "index failed:\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    strip_ansi(&String::from_utf8_lossy(&out.stderr))
}

/// Run `kb-mcp [--config cfg] status --kb-path <kb>`, asserting exit 0, and
/// return stripped stderr.
fn run_status(bin: &Path, cfg: Option<&Path>, kb: &Path) -> String {
    let mut args = Vec::new();
    if let Some(c) = cfg {
        args.push("--config".to_string());
        args.push(c.to_string_lossy().into_owned());
    }
    args.push("status".to_string());
    args.push("--kb-path".to_string());
    args.push(kb.display().to_string());
    let out = Command::new(bin)
        .args(&args)
        .output()
        .expect("kb-mcp status");
    assert!(
        out.status.success(),
        "status failed:\nstderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    strip_ansi(&String::from_utf8_lossy(&out.stderr))
}

/// Run `kb-mcp [--config cfg] search <query> --kb-path <kb> --format json`,
/// asserting exit 0, and return the parsed `results` array.
fn run_search(bin: &Path, cfg: Option<&Path>, kb: &Path, query: &str) -> Vec<serde_json::Value> {
    let mut args = Vec::new();
    if let Some(c) = cfg {
        args.push("--config".to_string());
        args.push(c.to_string_lossy().into_owned());
    }
    args.push("search".to_string());
    args.push(query.to_string());
    args.push("--kb-path".to_string());
    args.push(kb.display().to_string());
    args.push("--format".to_string());
    args.push("json".to_string());
    let out = Command::new(bin)
        .args(&args)
        .output()
        .expect("kb-mcp search");
    assert!(
        out.status.success(),
        "search failed:\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let v: serde_json::Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("search stdout is not JSON ({e}): {stdout}"));
    v["results"].as_array().cloned().unwrap_or_default()
}

fn contains_heading(results: &[serde_json::Value], heading: &str) -> bool {
    results
        .iter()
        .any(|h| h["heading"].as_str() == Some(heading))
}

// ---------------------------------------------------------------------------
// DB inspection helper (E-8: chunk identity + context_text)
// ---------------------------------------------------------------------------

/// Read `(chunks.id, chunks.context_text)` for the sole chunk of `path`.
/// Used to detect whether a re-index took the full re-embed path (new
/// autoincrement `id` via `upsert_document`'s DELETE+INSERT branch) or the
/// frontmatter-only meta-update fast path (`chunks` untouched, `id` stable).
fn chunk_row_for_path(db_path: &Path, path: &str) -> (i64, Option<String>) {
    let conn = rusqlite::Connection::open(db_path).expect("open db for inspection");
    conn.query_row(
        "SELECT c.id, c.context_text FROM chunks c \
         JOIN documents d ON d.id = c.document_id WHERE d.path = ?1",
        [path],
        |row| Ok((row.get::<_, i64>(0)?, row.get::<_, Option<String>>(1)?)),
    )
    .expect("chunk row must exist")
}

// ---------------------------------------------------------------------------
// Config / content fixtures
// ---------------------------------------------------------------------------

const CONTEXTUAL_ON: &str = "model = \"bge-small-en-v1.5\"\n[contextual]\nenabled = true\n";
const CONTEXTUAL_OFF: &str = "model = \"bge-small-en-v1.5\"\n[contextual]\nenabled = false\n";
const MODEL_ONLY: &str = "model = \"bge-small-en-v1.5\"\n";

/// `## 検索パイプライン` (parent) / `### RRF` (child). The RRF chunk's own
/// heading/body never mention "検索パイプライン" -- only the injected
/// ancestry context ("<title> > 検索パイプライン > RRF") does. Querying for
/// that parent-heading vocabulary and still finding the RRF chunk is the
/// signal that context injection is actually wired into embedding + FTS,
/// not just a meta flag.
const GUIDE_MD: &str = concat!(
    "---\n",
    "title: 検索基盤ガイド\n",
    "---\n",
    "\n",
    "## 検索パイプライン\n",
    "\n",
    "複数の検索手法を組み合わせてスコアを算出する仕組みについて説明します。",
    "まずベクトル検索と全文検索を並列に実行し、それぞれの候補を集めます。\n",
    "\n",
    "### RRF\n",
    "\n",
    "reciprocal rank fusion と呼ばれる手法でベクトル検索と全文検索のスコアを統合し、",
    "最終的な順位を決定します。\n",
);

const SOLO_TITLE_A: &str = concat!(
    "---\n",
    "title: Title Alpha\n",
    "tags: [x]\n",
    "---\n",
    "\n",
    "## Section\n",
    "\n",
    "Body content that is long enough to pass the quality filter comfortably, ",
    "mentioning search infrastructure details for the E-8 regression test.\n",
);

const SOLO_TITLE_B: &str = concat!(
    "---\n",
    "title: Title Beta\n",
    "tags: [x]\n",
    "---\n",
    "\n",
    "## Section\n",
    "\n",
    "Body content that is long enough to pass the quality filter comfortably, ",
    "mentioning search infrastructure details for the E-8 regression test.\n",
);

const SOLO_TITLE_B_TAGS_ONLY: &str = concat!(
    "---\n",
    "title: Title Beta\n",
    "tags: [y]\n",
    "---\n",
    "\n",
    "## Section\n",
    "\n",
    "Body content that is long enough to pass the quality filter comfortably, ",
    "mentioning search infrastructure details for the E-8 regression test.\n",
);

// ---------------------------------------------------------------------------
// Scenario 1: contextual on -> search hits via context, status = static
// ---------------------------------------------------------------------------

#[test]
#[ignore = "requires embedding model download"]
fn test_contextual_on_search_hits_via_context_and_status_shows_static() {
    let layout = TempKbLayout::new("kb-mcp-ctx-on");
    layout.write("guide.md", GUIDE_MD);
    let cfg = layout.root().join("kb-mcp.toml");
    std::fs::write(&cfg, CONTEXTUAL_ON).unwrap();

    let bin = kb_mcp_bin();
    run_index(&bin, &cfg, layout.kb(), false);

    // Brief step 2, literal ask: heading's own vocabulary hits it (trivial,
    // but pins the baseline behavior).
    let rrf_hits = run_search(&bin, Some(&cfg), layout.kb(), "RRF");
    assert!(
        contains_heading(&rrf_hits, "RRF"),
        "expected a hit with heading 'RRF', got: {rrf_hits:?}"
    );

    // Controller follow-up: parent-heading vocabulary (absent from the RRF
    // chunk's own heading/content) still surfaces it, because Static mode
    // prefixes the ancestry breadcrumb into both the embedding input
    // (`indexer::embed_input_for`) and the FTS `context` column.
    let parent_vocab_hits = run_search(&bin, Some(&cfg), layout.kb(), "検索パイプライン");
    assert!(
        contains_heading(&parent_vocab_hits, "RRF"),
        "expected parent-heading vocabulary query to surface the RRF chunk via \
         injected context, got: {parent_vocab_hits:?}"
    );

    let status = run_status(&bin, Some(&cfg), layout.kb());
    assert!(
        status.contains("Context mode: static"),
        "expected 'Context mode: static' in status stderr, got:\n{status}"
    );
}

// ---------------------------------------------------------------------------
// Scenario 2: off DB -> config flip warns (stays off) -> --force migrates
// ---------------------------------------------------------------------------

#[test]
#[ignore = "requires embedding model download"]
fn test_off_db_config_flip_warns_then_force_migrates_to_static() {
    let layout = TempKbLayout::new("kb-mcp-ctx-force");
    layout.write("guide.md", GUIDE_MD);
    let cfg = layout.root().join("kb-mcp.toml");
    std::fs::write(&cfg, CONTEXTUAL_OFF).unwrap();

    let bin = kb_mcp_bin();
    run_index(&bin, &cfg, layout.kb(), false);
    let status_off = run_status(&bin, Some(&cfg), layout.kb());
    assert!(
        status_off.contains("Context mode: off"),
        "expected 'Context mode: off' after enabled=false index, got:\n{status_off}"
    );

    // Flip config to enabled=true, re-index WITHOUT --force: must warn and
    // must NOT switch modes mid-index (would mix embedding spaces).
    std::fs::write(&cfg, CONTEXTUAL_ON).unwrap();
    let reindex_stderr = run_index(&bin, &cfg, layout.kb(), false);
    assert!(
        reindex_stderr.to_lowercase().contains("migrate"),
        "expected a 'migrate' warning in stderr on config/DB mismatch, got:\n{reindex_stderr}"
    );
    let status_still_off = run_status(&bin, Some(&cfg), layout.kb());
    assert!(
        status_still_off.contains("Context mode: off"),
        "expected DB to remain in 'off' mode without --force (grandfather continues, \
         E-11/D-13), got:\n{status_still_off}"
    );

    // --force actually migrates.
    run_index(&bin, &cfg, layout.kb(), true);
    let status_static = run_status(&bin, Some(&cfg), layout.kb());
    assert!(
        status_static.contains("Context mode: static"),
        "expected 'Context mode: static' after --force migration, got:\n{status_static}"
    );

    // And it is reflected in FTS/embedding, not just the meta flag (same
    // parent-heading-vocabulary probe as scenario 1).
    let parent_vocab_hits = run_search(&bin, Some(&cfg), layout.kb(), "検索パイプライン");
    assert!(
        contains_heading(&parent_vocab_hits, "RRF"),
        "expected context to be reflected in FTS/embedding after --force, got: \
         {parent_vocab_hits:?}"
    );
}

// ---------------------------------------------------------------------------
// Scenario 3: legacy (pre-feature-46 / v0.11.0) DB migration
// ---------------------------------------------------------------------------

/// v0.11.0 (pre-feature-46) DB schema: `chunks` has no `context_text`
/// column, `fts_chunks` is the 2-column (heading, content) schema. Modeled
/// on `Database`'s own `create_legacy_2col_fts_db` unit-test fixture
/// (`src/db.rs`), minus the `context_text` column/data -- that fixture
/// exists to test the FTS-only migration in isolation and already carries
/// `context_text`; a genuine v0.11.0 DB predates feature-46 entirely, so it
/// has neither the column nor a 3-column FTS index.
fn create_legacy_v0_11_0_db(path: &Path) {
    let conn = rusqlite::Connection::open(path).expect("open legacy db file");
    conn.execute_batch(
        "PRAGMA journal_mode = WAL;
         CREATE TABLE index_meta (key TEXT PRIMARY KEY, value TEXT);
         CREATE TABLE documents (id INTEGER PRIMARY KEY AUTOINCREMENT, path TEXT UNIQUE NOT NULL,
            title TEXT, topic TEXT, category TEXT, depth TEXT, tags TEXT, date TEXT,
            content_hash TEXT NOT NULL, last_indexed TEXT NOT NULL);
         CREATE TABLE chunks (id INTEGER PRIMARY KEY AUTOINCREMENT, document_id INTEGER NOT NULL,
            chunk_index INTEGER NOT NULL, heading TEXT, level INTEGER, content TEXT NOT NULL,
            token_count INTEGER, quality_score REAL NOT NULL DEFAULT 1.0);
         CREATE VIRTUAL TABLE fts_chunks USING fts5(heading, content, content='',
            contentless_delete=1, tokenize=\"trigram remove_diacritics 1 case_sensitive 0\");
         INSERT INTO documents (path, title, content_hash, last_indexed)
            VALUES ('legacy.md', 'Legacy Doc', 'h', '2026-01-01T00:00:00Z');
         INSERT INTO chunks (document_id, chunk_index, heading, level, content)
            VALUES (1, 0, 'Legacy Heading', 2,
                    'legacy body text about widgets and gadgets, written before feature-46 shipped');
         INSERT INTO fts_chunks (rowid, heading, content)
            VALUES (1, 'Legacy Heading',
                    'legacy body text about widgets and gadgets, written before feature-46 shipped');",
    )
    .expect("create legacy v0.11.0 schema");
}

/// `status` never touches the embedder (`src/main.rs` `Commands::Status`),
/// so the "does `Database::open` migrate a genuine legacy DB without
/// blowing up" half of scenario 3 needs no model download at all.
#[test]
fn test_legacy_v0_11_0_db_status_migrates_without_model_download() {
    let layout = TempKbLayout::new("kb-mcp-ctx-legacy-status");
    let db_path = layout.root().join(".kb-mcp.db");
    create_legacy_v0_11_0_db(&db_path);

    let bin = kb_mcp_bin();
    let status = run_status(&bin, None, layout.kb());
    assert!(
        status.contains("Documents: 1") && status.contains("Chunks: 1"),
        "status must succeed against a migrated legacy DB, got:\n{status}"
    );
}

/// The other half: a real `search` (needs the embedder, hence `#[ignore]`)
/// proves the migrated 3-column `bm25(fts_chunks, ...)` query actually
/// works end-to-end -- a stale 2-column schema would make that query fail
/// outright with "wrong number of arguments to function bm25()", not just
/// return empty/wrong results.
#[test]
#[ignore = "requires embedding model download"]
fn test_legacy_v0_11_0_db_search_works_after_fts_migration() {
    let layout = TempKbLayout::new("kb-mcp-ctx-legacy-search");
    let db_path = layout.root().join(".kb-mcp.db");
    create_legacy_v0_11_0_db(&db_path);
    let cfg = layout.root().join("kb-mcp.toml");
    std::fs::write(&cfg, MODEL_ONLY).unwrap();

    let bin = kb_mcp_bin();
    let hits = run_search(&bin, Some(&cfg), layout.kb(), "widgets");
    assert!(
        hits.iter().any(|h| h["path"].as_str() == Some("legacy.md")),
        "expected legacy.md to be found via FTS after migration, got: {hits:?}"
    );
}

// ---------------------------------------------------------------------------
// Controller follow-up 4 (E-8): title-only change forces re-embed,
// tags-only change does not.
// ---------------------------------------------------------------------------

/// Static mode's context is title-derived, so a frontmatter-only change to
/// `title` must NOT take the frontmatter-only skip fast path (would leave a
/// stale title baked into `context_text` -- `indexer::index_single_disk_entry`'s
/// `title_unchanged` gate). A frontmatter-only change to an unrelated field
/// (title held constant, only `tags` changes) must still take the fast
/// path.
///
/// We use the SQLite rowid of the chunk (`chunks.id`) as the re-embed
/// signal: the full re-embed path goes through `upsert_document`'s UPDATE
/// branch, which DELETEs the document's existing chunks before re-inserting
/// -- so the autoincrement `id` changes. The frontmatter-only fast path
/// (`update_document_meta`) never touches the `chunks` table, so `id` (and
/// therefore the embedding/context_text it carries) stays identical.
#[test]
#[ignore = "requires embedding model download"]
fn test_frontmatter_title_change_forces_reembed_but_tags_only_change_skips_it() {
    let layout = TempKbLayout::new("kb-mcp-ctx-e8");
    layout.write("solo.md", SOLO_TITLE_A);
    let cfg = layout.root().join("kb-mcp.toml");
    std::fs::write(&cfg, CONTEXTUAL_ON).unwrap();
    let db_path = layout.root().join(".kb-mcp.db");

    let bin = kb_mcp_bin();
    run_index(&bin, &cfg, layout.kb(), false);
    let (id_a, ctx_a) = chunk_row_for_path(&db_path, "solo.md");
    assert!(
        ctx_a.as_deref().is_some_and(|c| c.contains("Title Alpha")),
        "expected context_text to carry the initial title, got: {ctx_a:?}"
    );

    // (1) title-only change (heading/content unchanged): must re-embed.
    layout.write("solo.md", SOLO_TITLE_B);
    run_index(&bin, &cfg, layout.kb(), false);
    let (id_b, ctx_b) = chunk_row_for_path(&db_path, "solo.md");
    assert_ne!(
        id_a, id_b,
        "title change must re-embed the chunk (new chunks.id via DELETE+INSERT), \
         but rowid stayed {id_a}"
    );
    assert!(
        ctx_b.as_deref().is_some_and(|c| c.contains("Title Beta")),
        "expected context_text to carry the updated title, got: {ctx_b:?}"
    );
    assert!(
        ctx_b.as_deref().is_some_and(|c| !c.contains("Title Alpha")),
        "expected the stale title to be gone from context_text, got: {ctx_b:?}"
    );

    // (2) title unchanged, tags-only change: must NOT re-embed.
    layout.write("solo.md", SOLO_TITLE_B_TAGS_ONLY);
    run_index(&bin, &cfg, layout.kb(), false);
    let (id_c, ctx_c) = chunk_row_for_path(&db_path, "solo.md");
    assert_eq!(
        id_b, id_c,
        "tags-only change must NOT re-embed the chunk (chunks.id should be stable)"
    );
    assert_eq!(
        ctx_b, ctx_c,
        "tags-only change must leave context_text untouched"
    );
}

// ---------------------------------------------------------------------------
// Controller follow-up 5: status display, no model download required.
// ---------------------------------------------------------------------------

/// Hand-built **modern** (post-feature-46) schema DB: `chunks.context_text`
/// present, `fts_chunks` already 3-column, `index_meta.context_mode` set to
/// `mode`. `Database::open`'s migration steps are all column/table
/// presence checks, so opening this file is a pure no-op read path -- no
/// embedder involved, which is what lets the test below skip `#[ignore]`.
fn create_modern_context_db(path: &Path, mode: &str) {
    let conn = rusqlite::Connection::open(path).expect("open modern db file");
    conn.execute_batch(&format!(
        "PRAGMA journal_mode = WAL;
         CREATE TABLE index_meta (key TEXT PRIMARY KEY, value TEXT);
         CREATE TABLE documents (id INTEGER PRIMARY KEY AUTOINCREMENT, path TEXT UNIQUE NOT NULL,
            title TEXT, topic TEXT, category TEXT, depth TEXT, tags TEXT, date TEXT,
            content_hash TEXT NOT NULL, last_indexed TEXT NOT NULL);
         CREATE TABLE chunks (id INTEGER PRIMARY KEY AUTOINCREMENT, document_id INTEGER NOT NULL
            REFERENCES documents(id) ON DELETE CASCADE, chunk_index INTEGER NOT NULL,
            heading TEXT, level INTEGER, content TEXT NOT NULL, token_count INTEGER,
            quality_score REAL NOT NULL DEFAULT 1.0, context_text TEXT);
         CREATE VIRTUAL TABLE fts_chunks USING fts5(heading, context, content, content='',
            contentless_delete=1, tokenize=\"trigram remove_diacritics 1 case_sensitive 0\");
         INSERT INTO index_meta (key, value) VALUES ('context_mode', '{mode}');
         INSERT INTO documents (path, title, content_hash, last_indexed)
            VALUES ('a.md', 'A', 'h', '2026-01-01T00:00:00Z');
         INSERT INTO chunks (document_id, chunk_index, heading, level, content, context_text)
            VALUES (1, 0, 'H', 2,
                    'body text here, long enough to pass the quality filter comfortably',
                    'A > H');
         INSERT INTO fts_chunks (rowid, heading, context, content)
            VALUES (1, 'H', 'A > H',
                    'body text here, long enough to pass the quality filter comfortably');"
    ))
    .expect("create modern schema db");
}

/// `kb-mcp status` displays `Context mode: static` on stderr. `status`
/// never constructs an `Embedder` (see `Commands::Status` in
/// `src/main.rs`), so this needs neither `kb-mcp index` nor a model
/// download -- a hand-built DB file with `index_meta.context_mode` set
/// directly is enough. (The other half of controller follow-up 5, the
/// config/DB mismatch *warning* text, is only reachable through
/// `kb-mcp index` and is already asserted by the `#[ignore]`d
/// `test_off_db_config_flip_warns_then_force_migrates_to_static` above --
/// duplicating a second model-requiring test for it would just add DL cost
/// for no new coverage.)
#[test]
fn test_status_shows_context_mode_static_without_model_download() {
    let layout = TempKbLayout::new("kb-mcp-ctx-status-nomodel");
    let db_path = layout.root().join(".kb-mcp.db");
    create_modern_context_db(&db_path, "static");

    let bin = kb_mcp_bin();
    let status = run_status(&bin, None, layout.kb());
    assert!(
        status.contains("Context mode: static"),
        "expected 'Context mode: static' in status stderr, got:\n{status}"
    );
    assert!(status.contains("Documents: 1"), "got:\n{status}");
    assert!(status.contains("Chunks: 1"), "got:\n{status}");
}

// ---------------------------------------------------------------------------
// codex P2 on PR #73 (F3): Static-mode rename must not take the same-hash
// fast path when the document's context-title is filename-derived.
// ---------------------------------------------------------------------------

/// No frontmatter at all -- `ctx_title` falls back to the filename stem
/// (`parser::txt::derive_title_pub`, E-1: `-`/`_` become spaces). Renaming
/// this file changes the filename-derived title even though the file's
/// *content* (and hence its SHA-256 hash) does not change.
const NO_TITLE_MD: &str = concat!(
    "## Section\n",
    "\n",
    "Body content that is long enough to pass the quality filter comfortably, ",
    "mentioning rename regression testing details for the F3 codex fix.\n",
);

/// codex P2 on PR #73 (F3): renaming a frontmatter-title-less file in Static
/// mode must re-embed (not take the same-hash rename fast path in
/// `index_single_disk_entry`), because the chunk's `context_text` breadcrumb
/// is derived from the *filename* (E-1) and would otherwise go stale after
/// the rename. Before the fix, `rebuild_index` applied `db.rename_document`
/// (path UPDATE only) and then called `index_single_disk_entry` on the
/// renamed entry with `force=false`; since the disk hash always matches the
/// just-renamed `documents.content_hash` row, the hash-equality fast path
/// fired unconditionally and skipped re-parsing -- so `context_text` kept
/// the *old* filename-derived title forever. This test pins the fix that
/// forces renamed entries through a full reparse/re-embed in Static mode.
#[test]
#[ignore = "requires embedding model download"]
fn test_static_mode_rename_no_title_reembeds_and_updates_context() {
    let layout = TempKbLayout::new("kb-mcp-ctx-f3-static");
    layout.write("old-widget-doc.md", NO_TITLE_MD);
    let cfg = layout.root().join("kb-mcp.toml");
    std::fs::write(&cfg, CONTEXTUAL_ON).unwrap();
    let db_path = layout.root().join(".kb-mcp.db");

    let bin = kb_mcp_bin();
    run_index(&bin, &cfg, layout.kb(), false);
    let (id_before, ctx_before) = chunk_row_for_path(&db_path, "old-widget-doc.md");
    assert!(
        ctx_before
            .as_deref()
            .is_some_and(|c| c.contains("old widget doc")),
        "expected context_text to carry the filename-derived title, got: {ctx_before:?}"
    );

    // Rename on disk. Content is unchanged, so the disk hash after rename is
    // identical to the pre-rename `documents.content_hash` row.
    std::fs::rename(
        layout.kb().join("old-widget-doc.md"),
        layout.kb().join("new-gadget-doc.md"),
    )
    .expect("rename on disk");
    run_index(&bin, &cfg, layout.kb(), false);

    let (id_after, ctx_after) = chunk_row_for_path(&db_path, "new-gadget-doc.md");
    assert_ne!(
        id_before, id_after,
        "Static-mode rename must re-embed (new chunks.id via full reparse), \
         but rowid stayed {id_before} -- the rename fast path was not disabled"
    );
    assert!(
        ctx_after
            .as_deref()
            .is_some_and(|c| c.contains("new gadget doc")),
        "expected context_text to carry the new filename-derived title, got: {ctx_after:?}"
    );
    assert!(
        ctx_after
            .as_deref()
            .is_some_and(|c| !c.contains("old widget doc")),
        "expected the stale filename-derived title to be gone, got: {ctx_after:?}"
    );
}

/// Off-mode control: the same rename scenario must still take the same-hash
/// fast path (no re-embed) -- Off mode never embeds context, so a
/// filename-derived title change is harmless and the perf optimization must
/// be preserved (F3 fix must be Static-only).
#[test]
#[ignore = "requires embedding model download"]
fn test_off_mode_rename_no_title_keeps_fast_path() {
    let layout = TempKbLayout::new("kb-mcp-ctx-f3-off");
    layout.write("old-widget-doc.md", NO_TITLE_MD);
    let cfg = layout.root().join("kb-mcp.toml");
    std::fs::write(&cfg, CONTEXTUAL_OFF).unwrap();
    let db_path = layout.root().join(".kb-mcp.db");

    let bin = kb_mcp_bin();
    run_index(&bin, &cfg, layout.kb(), false);
    let (id_before, ctx_before) = chunk_row_for_path(&db_path, "old-widget-doc.md");
    assert!(
        ctx_before.is_none(),
        "Off mode never stores context_text, got: {ctx_before:?}"
    );

    std::fs::rename(
        layout.kb().join("old-widget-doc.md"),
        layout.kb().join("new-gadget-doc.md"),
    )
    .expect("rename on disk");
    run_index(&bin, &cfg, layout.kb(), false);

    let (id_after, ctx_after) = chunk_row_for_path(&db_path, "new-gadget-doc.md");
    assert_eq!(
        id_before, id_after,
        "Off-mode rename must keep the same-hash fast path (chunks.id stable)"
    );
    assert!(
        ctx_after.is_none(),
        "Off mode never stores context_text, got: {ctx_after:?}"
    );
}
