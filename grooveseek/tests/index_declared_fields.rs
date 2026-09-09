//! feature-58: the keys `groove-schema.toml` declares reach `document_fields`
//! at index time and `groove search --field` / `--field-not` and the MCP
//! `search` tool's `fields` / `fields_not` filter on them. Subprocess tests
//! against the built binary, in the shape of `index_frontmatter_unparsed.rs`.

mod common;

use common::ansi::strip_ansi;
use common::mcp::{mcp_initialize, mcp_search_call, spawn_mcp_server};
use common::temp::TempKbLayout;
use std::path::Path;
use std::process::{Command, ExitStatus};

fn groove_bin() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_BIN_EXE_groove"))
}

fn run(kb: &Path, args: &[&str]) -> (String, String, ExitStatus) {
    run_with_config(kb, None, args)
}

/// Like [`run`], but threads `--config <path>` ahead of the subcommand -- the
/// same shape `index_frontmatter_unparsed.rs::run_index` uses.
///
/// `groove.toml` discovery walks up from the process's CWD (`.git` ancestor,
/// then the binary's own directory -- `Config::discover_located`,
/// `grooveseek/src/config.rs:554-600`), never from `--kb-path`. A `groove.toml`
/// written under the temp KB directory itself is therefore never found by a
/// plain `run`; a test that needs one active (e.g. `fail_on_frontmatter_error`)
/// has to write it beside the KB ([`TempKbLayout::root`], a sibling of
/// [`TempKbLayout::kb`] that is not walked as content) and hand it over explicitly, exactly as
/// `index_frontmatter_unparsed.rs::strict_config` does.
fn run_with_config(
    kb: &Path,
    config: Option<&Path>,
    args: &[&str],
) -> (String, String, ExitStatus) {
    let mut cmd = Command::new(groove_bin());
    if let Some(cfg) = config {
        cmd.arg("--config").arg(cfg);
    }
    cmd.args(args).arg("--kb-path").arg(kb);
    let out = cmd.output().expect("groove runs");
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        strip_ansi(&String::from_utf8_lossy(&out.stderr)),
        out.status,
    )
}

fn fields_of(kb: &Path, rel: &str) -> Vec<(String, String)> {
    let conn = rusqlite::Connection::open(kb.parent().unwrap().join(".groove.db")).unwrap();
    let mut stmt = conn
        .prepare(
            "SELECT f.key, f.value FROM document_fields f JOIN documents d ON d.id = f.document_id \
             WHERE d.path = ?1 ORDER BY f.key, f.value",
        )
        .unwrap();
    stmt.query_map([rel], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

fn declared_meta(kb: &Path) -> Option<String> {
    let conn = rusqlite::Connection::open(kb.parent().unwrap().join(".groove.db")).unwrap();
    conn.query_row(
        "SELECT value FROM index_meta WHERE key = 'declared_fields'",
        [],
        |r| r.get(0),
    )
    .ok()
}

const SCHEMA: &str = "[fields.status]\nenum = [\"active\", \"deprecated\"]\n[fields.environment]\n";

/// [`TempKbLayout`] (tests/common/temp.rs): [`TempKbLayout::kb`] is the
/// `--kb-path`, [`TempKbLayout::root`] its parent (where `.groove.db` lands),
/// [`TempKbLayout::write`] writes under [`TempKbLayout::kb`]. Dropping it
/// removes the root.
fn corpus() -> TempKbLayout {
    let kb = TempKbLayout::new("groove-declared-fields");
    kb.write(
        "active.md",
        "---\ntitle: Active runbook\nstatus: active\nenvironment: [dev, prod]\nteam: core\n---\n\n# Restart the gateway\n\nRun the restart script for the gateway service.\n",
    );
    kb.write(
        "old.md",
        "---\ntitle: Deprecated runbook\nstatus: deprecated\nteam: core\n---\n\n# Restart the gateway (old)\n\nRun the restart script for the gateway service the old way.\n",
    );
    kb.write(
        "plain.md",
        "---\ntitle: No status\n---\n\n# Restart the gateway notes\n\nNotes about the restart script for the gateway service.\n",
    );
    kb
}

fn pairs(v: &[(&str, &str)]) -> Vec<(String, String)> {
    v.iter()
        .map(|(k, val)| (k.to_string(), val.to_string()))
        .collect()
}

#[test]
fn declared_keys_reach_document_fields_and_undeclared_ones_do_not() {
    let kb = corpus();
    kb.write("groove-schema.toml", SCHEMA);
    let (_, err, status) = run(kb.kb(), &["index"]);
    assert!(status.success(), "{err}");
    assert_eq!(
        fields_of(kb.kb(), "active.md"),
        pairs(&[
            ("environment", "dev"),
            ("environment", "prod"),
            ("status", "active")
        ])
    );
    assert_eq!(
        fields_of(kb.kb(), "old.md"),
        pairs(&[("status", "deprecated")])
    );
    assert!(fields_of(kb.kb(), "plain.md").is_empty());
    assert_eq!(
        declared_meta(kb.kb()).as_deref(),
        Some("[\"environment\",\"status\"]")
    );
}

#[test]
fn without_a_schema_nothing_is_recorded_and_the_generation_key_is_the_empty_list() {
    let kb = corpus();
    let (_, err, status) = run(kb.kb(), &["index"]);
    assert!(status.success(), "{err}");
    assert!(fields_of(kb.kb(), "active.md").is_empty());
    assert_eq!(declared_meta(kb.kb()).as_deref(), Some("[]"));
    assert!(
        !err.contains("Recorded the declared frontmatter fields"),
        "no refresh pass for an empty set: {err}"
    );
}

#[test]
fn a_schema_that_does_not_load_stops_the_index_and_names_the_file() {
    let kb = corpus();
    kb.write(
        "groove-schema.toml",
        "[fields.status]\ntype = \"integer\"\n",
    );
    let (_, err, status) = run(kb.kb(), &["index"]);
    assert!(!status.success());
    assert!(err.contains("groove-schema.toml"), "{err}");
}

#[test]
fn a_schema_that_does_not_load_leaves_a_forced_rebuild_untouched() {
    // (codex P1 round 1 on PR #291) `--force` calls `reset_for_model` before
    // `rebuild_index` is even entered (`main.rs`'s `Commands::Index` arm), so
    // a schema load failure has to be checked before that reset runs, not
    // only inside `rebuild_index`. This pins that a malformed schema stops a
    // forced run before it empties the index.
    let kb = corpus();
    kb.write("groove-schema.toml", SCHEMA);
    let (_, err, status) = run(kb.kb(), &["index"]);
    assert!(status.success(), "{err}");
    let declared_before = fields_of(kb.kb(), "active.md");
    assert_eq!(
        declared_before,
        pairs(&[
            ("environment", "dev"),
            ("environment", "prod"),
            ("status", "active")
        ])
    );

    kb.write(
        "groove-schema.toml",
        "[fields.status]\ntype = \"integer\"\n",
    );
    let (_, err, status) = run(kb.kb(), &["index", "--force"]);
    assert!(
        !status.success(),
        "a malformed schema must fail --force too"
    );
    assert!(err.contains("groove-schema.toml"), "{err}");

    let conn = rusqlite::Connection::open(kb.root().join(".groove.db")).unwrap();
    let doc_count: i64 = conn
        .query_row("SELECT count(*) FROM documents", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        doc_count, 3,
        "a --force that fails on the schema must not have emptied the index"
    );
    assert_eq!(
        fields_of(kb.kb(), "active.md"),
        declared_before,
        "document_fields must survive the failed forced rebuild too"
    );
}

// No MCP-tool variant: `tests/common/mcp.rs` has no helper for the `rebuild_index`
// tool (only `mcp_search_call`), so this scenario is covered on the CLI path only.

#[test]
fn an_index_built_before_the_schema_gains_its_rows_without_re_embedding_or_a_frontmatter_failure() {
    let kb = corpus();
    // `groove.toml` config discovery never looks under `--kb-path` (see
    // `run_with_config`'s doc comment), so the strict flag has to be written
    // beside the KB and passed with `--config`, not written into the KB.
    let cfg = kb.root().join("groove.toml");
    std::fs::write(&cfg, "[index]\nfail_on_frontmatter_error = true\n").unwrap();
    let (_, err, status) = run_with_config(kb.kb(), Some(&cfg), &["index"]);
    assert!(status.success(), "{err}");
    // Simulate a 1.8.0 index: no generation key at all.
    {
        let conn = rusqlite::Connection::open(kb.root().join(".groove.db")).unwrap();
        conn.execute("DELETE FROM index_meta WHERE key = 'declared_fields'", [])
            .unwrap();
        conn.execute("DELETE FROM document_fields", []).unwrap();
    }
    kb.write("groove-schema.toml", SCHEMA);
    let (_, err, status) = run_with_config(kb.kb(), Some(&cfg), &["index"]);
    assert!(
        status.success(),
        "a healthy KB gaining declared keys must not trip fail_on_frontmatter_error: {err}"
    );
    assert!(err.contains("(0 updated, "), "nothing re-embedded: {err}");
    assert!(err.contains(", 0 frontmatter unparsed), "), "{err}");
    assert!(
        err.contains(
            "Recorded the declared frontmatter fields of 3 unchanged Markdown document(s)"
        ),
        "{err}"
    );
    assert!(!err.contains("had failed to parse"), "{err}");
    assert_eq!(
        fields_of(kb.kb(), "active.md"),
        pairs(&[
            ("environment", "dev"),
            ("environment", "prod"),
            ("status", "active")
        ])
    );
    // Second run: fast path, no refresh sentence.
    let (_, err, status) = run_with_config(kb.kb(), Some(&cfg), &["index"]);
    assert!(status.success(), "{err}");
    assert!(
        !err.contains("Recorded the declared frontmatter fields"),
        "{err}"
    );
}

#[test]
fn a_broken_frontmatter_in_the_same_refresh_run_is_the_only_document_counted() {
    let kb = corpus();
    kb.write(
        "broken.md",
        "---\ntitle: [unclosed\nstatus: active\n---\n\n# Broken\n\nThe restart script for the gateway service, with a YAML block that does not parse.\n",
    );
    let (_, err, status) = run(kb.kb(), &["index"]);
    assert!(status.success(), "{err}");
    assert!(
        err.contains(", 1 frontmatter unparsed), "),
        "first run names the broken file: {err}"
    );

    // Both generation keys absent: the #251 tag pass and the declared-fields
    // pass run together. Only the broken document moves the counter.
    {
        let conn = rusqlite::Connection::open(kb.root().join(".groove.db")).unwrap();
        conn.execute(
            "DELETE FROM index_meta WHERE key IN ('declared_fields', 'frontmatter_policy')",
            [],
        )
        .unwrap();
    }
    kb.write("groove-schema.toml", SCHEMA);
    let (_, err, status) = run(kb.kb(), &["index"]);
    assert!(status.success(), "{err}");
    assert!(err.contains("(0 updated, "), "{err}");
    assert!(
        err.contains(", 1 frontmatter unparsed), "),
        "the broken document, and only it, is counted: {err}"
    );
    assert!(
        err.contains("Tagged 1 unchanged Markdown document(s)"),
        "{err}"
    );
    assert!(
        err.contains(
            "Recorded the declared frontmatter fields of 4 unchanged Markdown document(s)"
        ),
        "{err}"
    );
    assert!(
        fields_of(kb.kb(), "broken.md").is_empty(),
        "a refused block holds no values"
    );
    assert_eq!(
        fields_of(kb.kb(), "active.md"),
        pairs(&[
            ("environment", "dev"),
            ("environment", "prod"),
            ("status", "active")
        ])
    );

    // Declared set changes again, the tag pass already done: the broken document
    // is read for the fields reason only and is not counted, as on any second run.
    kb.write("groove-schema.toml", &format!("{SCHEMA}[fields.team]\n"));
    let (_, err, status) = run(kb.kb(), &["index"]);
    assert!(status.success(), "{err}");
    assert!(err.contains(", 0 frontmatter unparsed), "), "{err}");
    assert!(!err.contains("Tagged "), "{err}");
    assert!(
        err.contains(
            "Recorded the declared frontmatter fields of 4 unchanged Markdown document(s)"
        ),
        "{err}"
    );
}

#[test]
fn declaring_another_key_adds_its_rows_and_undeclaring_removes_them() {
    let kb = corpus();
    kb.write("groove-schema.toml", SCHEMA);
    let (_, err, status) = run(kb.kb(), &["index"]);
    assert!(status.success(), "{err}");
    kb.write("groove-schema.toml", &format!("{SCHEMA}[fields.team]\n"));
    let (_, err, status) = run(kb.kb(), &["index"]);
    assert!(status.success(), "{err}");
    assert_eq!(
        fields_of(kb.kb(), "old.md"),
        pairs(&[("status", "deprecated"), ("team", "core")])
    );
    kb.write("groove-schema.toml", "[fields.team]\n");
    let (_, err, status) = run(kb.kb(), &["index"]);
    assert!(status.success(), "{err}");
    assert_eq!(fields_of(kb.kb(), "old.md"), pairs(&[("team", "core")]));
}

#[test]
fn an_interrupted_run_s_leftover_rows_are_cleared_when_the_schema_goes_away() {
    // (codex P2 round 2 on PR #291) `refresh_fields`'s shortcut treated "no
    // generation key" as "no rows" -- true for a fresh 1.8.0-shaped index, but
    // not for a run that declared keys, wrote some documents' rows, and was
    // interrupted before recording the generation key. Delete only the
    // generation key here (not `document_fields`) to simulate that, then
    // remove the schema so the target set becomes `[]`: the refresh pass must
    // still run and clear the leftover rows, not just record `[]` over them.
    let kb = corpus();
    kb.write("groove-schema.toml", SCHEMA);
    let (_, err, status) = run(kb.kb(), &["index"]);
    assert!(status.success(), "{err}");
    assert_eq!(
        fields_of(kb.kb(), "active.md"),
        pairs(&[
            ("environment", "dev"),
            ("environment", "prod"),
            ("status", "active")
        ])
    );

    {
        let conn = rusqlite::Connection::open(kb.root().join(".groove.db")).unwrap();
        conn.execute("DELETE FROM index_meta WHERE key = 'declared_fields'", [])
            .unwrap();
    }
    std::fs::remove_file(kb.kb().join("groove-schema.toml")).unwrap();

    let (_, err, status) = run(kb.kb(), &["index"]);
    assert!(status.success(), "{err}");
    assert!(
        err.contains(
            "Recorded the declared frontmatter fields of 3 unchanged Markdown document(s)"
        ),
        "the refresh pass must run rather than short-circuit: {err}"
    );
    assert!(
        fields_of(kb.kb(), "active.md").is_empty(),
        "leftover rows from before the interruption must be cleared"
    );
    assert_eq!(declared_meta(kb.kb()).as_deref(), Some("[]"));
}

fn search_paths(kb: &Path, extra: &[&str]) -> (Vec<String>, serde_json::Value) {
    let mut args = vec![
        "search",
        "restart script for the gateway service",
        "--format",
        "json",
        "--limit",
        "10",
    ];
    args.extend_from_slice(extra);
    let (out, err, status) = run(kb, &args);
    assert!(status.success(), "{err}");
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    let mut paths: Vec<String> = v["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| h["path"].as_str().unwrap().to_string())
        .collect();
    paths.sort();
    paths.dedup();
    (paths, v["filter_applied"].clone())
}

#[test]
fn the_command_line_filters_on_declared_fields_and_echoes_them() {
    let kb = corpus();
    kb.write("groove-schema.toml", SCHEMA);
    let (_, err, status) = run(kb.kb(), &["index"]);
    assert!(status.success(), "{err}");

    let (paths, echo) = search_paths(kb.kb(), &["--field", "status=active"]);
    assert_eq!(paths, vec!["active.md"]);
    assert_eq!(echo["fields"], serde_json::json!({"status": ["active"]}));

    let (paths, _) = search_paths(
        kb.kb(),
        &["--field", "status=active", "--field", "status=deprecated"],
    );
    assert_eq!(paths, vec!["active.md", "old.md"]);

    let (paths, _) = search_paths(
        kb.kb(),
        &["--field", "status=active", "--field", "environment=prod"],
    );
    assert_eq!(paths, vec!["active.md"]);

    let (paths, echo) = search_paths(kb.kb(), &["--field-not", "status=deprecated"]);
    assert_eq!(
        paths,
        vec!["active.md", "plain.md"],
        "a document without the key survives"
    );
    assert_eq!(
        echo["fields_not"],
        serde_json::json!({"status": ["deprecated"]})
    );
    assert!(echo.get("fields").is_none());

    let (paths, _) = search_paths(
        kb.kb(),
        &[
            "--field-not",
            "status=deprecated",
            "--field-not",
            "environment=prod",
        ],
    );
    assert_eq!(
        paths,
        vec!["plain.md"],
        "matching either exclusion drops the document"
    );

    let (paths, echo) = search_paths(kb.kb(), &["--field", "nope=x"]);
    assert!(
        paths.is_empty(),
        "an undeclared key is an empty answer, not an error"
    );
    assert_eq!(echo["fields"], serde_json::json!({"nope": ["x"]}));

    for bad in ["status", "=x", "status="] {
        let (_, _, status) = run(kb.kb(), &["search", "q", "--field", bad]);
        assert_eq!(status.code(), Some(2), "{bad:?} is a usage error");
    }
}

#[test]
fn the_mcp_search_tool_filters_on_declared_fields_and_echoes_them() {
    let kb = corpus();
    kb.write("groove-schema.toml", SCHEMA);
    let (_, err, status) = run(kb.kb(), &["index"]);
    assert!(status.success(), "{err}");
    // spawn_mcp_server(kb_path, config_path) -> (ServerGuard, base_url); the guard
    // kills and reaps the server on Drop (tests/common/mcp.rs:248). The config is
    // passed as `--config`, so write one (the exclusion tests do the same).
    let cfg_path = kb.root().join("groove.toml");
    std::fs::write(&cfg_path, "[watch]\nenabled = false\n").unwrap();
    let (_guard, base) = spawn_mcp_server(kb.kb(), &cfg_path);
    let session = mcp_initialize(&base);
    let query = "restart script for the gateway service";

    let v = mcp_search_call(
        &base,
        &session,
        serde_json::json!({"query": query, "limit": 10, "fields": {"status": ["active"]}}),
    );
    let paths: std::collections::BTreeSet<String> = v["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| h["path"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(paths.into_iter().collect::<Vec<_>>(), vec!["active.md"]);
    assert_eq!(
        v["filter_applied"]["fields"],
        serde_json::json!({"status": ["active"]})
    );

    let v = mcp_search_call(
        &base,
        &session,
        serde_json::json!({"query": query, "limit": 10, "fields_not": {"status": ["deprecated"]}, "fields": {}}),
    );
    let paths: std::collections::BTreeSet<String> = v["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| h["path"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(
        paths.into_iter().collect::<Vec<_>>(),
        vec!["active.md", "plain.md"]
    );
    assert!(
        v["filter_applied"].get("fields").is_none(),
        "an empty object has no effect and is not echoed"
    );

    let v = mcp_search_call(
        &base,
        &session,
        serde_json::json!({"query": query, "fields": {"status": [""]}}),
    );
    assert_eq!(
        v["error"].as_str().unwrap(),
        "fields.status has an empty value"
    );
}
