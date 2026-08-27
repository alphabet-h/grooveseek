//! (feature-56 PR-3a) End-to-end tests for grammars that arrive as separate libraries.
//!
//! What these cover that the unit tests cannot: the wording of each refusal reaching a user
//! through the binary, the guarantee that a refusal happens **before** a database exists, and
//! a real `dlopen` of a real `cdylib` on whatever platform this is running on.
//!
//! # Why most of these are not `#[ignore]`
//!
//! Every failing path is decided while the parser registry is built, which the `Commands::Index`
//! arm in `src/main.rs` (a binary, so not linkable from here) does before
//! [`grooveseek::db::Database::open`] and before
//! [`grooveseek::embedder::Embedder::with_model`] — deliberately, so that a
//! run known to fail cannot first create a database or download a model. So the refusals cost
//! a process spawn and nothing else. Only the accepting path indexes for real, and that one is
//! `#[ignore]` like the rest of the model-loading tests.
//!
//! # The fixtures
//!
//! `tests/fixtures/grammar_plugins/*.rs`, built as `cdylib`s by `cargo test` through the
//! `[[example]]` entries in `Cargo.toml`. They hand over the Rust parse table under whatever
//! extension the case needs; the loader checks a contract and does not know one language from
//! another.

use std::path::{Path, PathBuf};
use std::process::Command;

mod common;
use common::mcp::grooveseek_bin;
use common::temp::TempKbLayout;

/// `[parsers].enabled` with the plugin language turned on beside Markdown.
const PARSERS_MD_PY: &str =
    "model = \"bge-small-en-v1.5\"\n[parsers]\nenabled = [\"md\", \"py\"]\n";

/// A file that is valid Rust, named `.py` so the fixture grammar can parse it.
///
/// The fixture declares the `py` extension and hands over the Rust parse table, so feeding it
/// Rust is what makes the definitions in the assertions real rather than a tree of errors.
/// What is under test is the loader, not a Python grammar — that arrives with PR-3b.
const SAMPLE_PY: &str = r#"/// Rebalances the shard table after a node leaves.
pub fn rebalance_shard_table(nodes: usize) -> usize {
    let quorum_drift_budget = 7;
    nodes * quorum_drift_budget
}
"#;

const SAMPLE_MD: &str = "---\ntitle: Shard notes\n---\n\n## Rebalancing\n\nThe prose page also discusses the quorum drift budget at length, so a query for it has\nsomething to match in both halves of the knowledge base.\n";

/// Where `cargo` left a `cdylib` example, derived from the test binary's own knowledge of
/// where the `groove` binary is.
///
/// `CARGO_BIN_EXE_groove` points at `target/<profile>/groove`, so its parent is the profile
/// directory and `examples/` sits beside it. Deriving it keeps `--release` working without a
/// second rule to remember.
fn example_cdylib(name: &str) -> PathBuf {
    let profile_dir = grooveseek_bin()
        .parent()
        .expect("the test binary knows where groove is")
        .to_path_buf();
    let file = format!(
        "{}{name}{}",
        std::env::consts::DLL_PREFIX,
        std::env::consts::DLL_SUFFIX
    );
    let path = profile_dir.join("examples").join(&file);
    assert!(
        path.exists(),
        "the {name} fixture was not built; `cargo test` should have produced {}",
        path.display()
    );
    path
}

/// The file name the loader looks for, whatever this platform calls a dynamic library.
fn plugin_file_name() -> String {
    format!(
        "{}groove_grammar_python{}",
        std::env::consts::DLL_PREFIX,
        std::env::consts::DLL_SUFFIX
    )
}

/// Make `<root>/grammars/` and return it. Nothing is put in it.
fn empty_grammar_dir(layout: &TempKbLayout) -> PathBuf {
    let dir = layout.root().join("grammars");
    std::fs::create_dir_all(&dir).expect("create grammar dir");
    dir
}

/// Put `fixture` in the grammar directory under the name the loader will look for.
fn place_plugin(dir: &Path, fixture: &str) {
    std::fs::copy(example_cdylib(fixture), dir.join(plugin_file_name())).expect("place plugin");
}

/// Write a config naming the grammar directory, and return its path.
///
/// `grammar_dir` is written with forward slashes because a TOML basic string treats a
/// backslash as an escape, and a Windows path is full of them.
fn write_config(layout: &TempKbLayout, grammar_dir: Option<&Path>) -> PathBuf {
    let mut body = String::new();
    if let Some(dir) = grammar_dir {
        body.push_str(&format!(
            "grammar_dir = \"{}\"\n",
            dir.display().to_string().replace('\\', "/")
        ));
    }
    body.push_str(PARSERS_MD_PY);
    let cfg = layout.root().join("groove.toml");
    std::fs::write(&cfg, body).expect("write config");
    cfg
}

/// `groove --config <cfg> index --kb-path <kb>`, returning (success, stderr).
fn run_index(cfg: &Path, kb: &Path) -> (bool, String) {
    let out = Command::new(grooveseek_bin())
        .args([
            "--config",
            cfg.to_str().unwrap(),
            "index",
            "--kb-path",
            &kb.display().to_string(),
        ])
        .output()
        .expect("groove index");
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// The database `groove index` would create, so a test can assert it did not.
fn db_path(layout: &TempKbLayout) -> PathBuf {
    layout.root().join(".groove.db")
}

// ---------------------------------------------------------------------------
// (i) The directory is known and the file is not in it
// ---------------------------------------------------------------------------

/// The whole point of failing at registry construction: nothing is created first.
///
/// Placed before [`grooveseek::db::Database::open`] on purpose (see the comment on the
/// `Commands::Index` arm in `src/main.rs`), and this
/// is the test that says so from outside. Without it, a later refactor could move the check
/// down and only a careful reader would notice.
#[test]
fn a_missing_plugin_stops_the_run_before_a_database_exists() {
    let layout = TempKbLayout::new("groove-plugin-missing");
    layout.write("notes.md", SAMPLE_MD);
    layout.write("sample.py", SAMPLE_PY);
    let grammars = empty_grammar_dir(&layout);
    let cfg = write_config(&layout, Some(&grammars));

    let (ok, stderr) = run_index(&cfg, layout.kb());
    assert!(!ok, "a missing plugin must fail the run:\n{stderr}");
    assert!(
        stderr.contains(&plugin_file_name()),
        "the message should name the file to place:\n{stderr}"
    );
    assert!(
        stderr.contains("groove-grammar-py-<target>"),
        "the message should name the release asset:\n{stderr}"
    );
    assert!(
        !db_path(&layout).exists(),
        "a run that cannot succeed must not create {}",
        db_path(&layout).display()
    );
}

// ---------------------------------------------------------------------------
// (ii) The file is there and was refused
// ---------------------------------------------------------------------------

/// Bytes that are not a library at all fail at the platform call, before any symbol lookup.
#[test]
fn a_file_that_is_not_a_library_is_refused_with_its_path() {
    let layout = TempKbLayout::new("groove-plugin-garbage");
    layout.write("notes.md", SAMPLE_MD);
    let grammars = empty_grammar_dir(&layout);
    std::fs::write(grammars.join(plugin_file_name()), b"not a library at all").expect("write");
    let cfg = write_config(&layout, Some(&grammars));

    let (ok, stderr) = run_index(&cfg, layout.kb());
    assert!(!ok, "a corrupt plugin must fail the run:\n{stderr}");
    assert!(
        stderr.contains("was refused because"),
        "expected the refusal wording:\n{stderr}"
    );
    assert!(
        stderr.contains(&plugin_file_name()),
        "the message should name the path it refused:\n{stderr}"
    );
    assert!(
        !db_path(&layout).exists(),
        "a refused plugin must not leave a database behind"
    );
}

/// A perfectly good library that is not a grammar fails at the first symbol, which is a
/// different branch from the one above and reads differently on purpose.
#[test]
fn a_library_without_the_contract_is_refused_by_the_symbol_it_lacks() {
    let layout = TempKbLayout::new("groove-plugin-nosym");
    layout.write("notes.md", SAMPLE_MD);
    let grammars = empty_grammar_dir(&layout);
    place_plugin(&grammars, "groove_grammar_no_symbols");
    let cfg = write_config(&layout, Some(&grammars));

    let (ok, stderr) = run_index(&cfg, layout.kb());
    assert!(!ok, "a library with no contract must fail:\n{stderr}");
    assert!(
        stderr.contains("does not export groove_grammar_abi_version"),
        "expected the missing symbol to be named:\n{stderr}"
    );
    assert!(!db_path(&layout).exists());
}

// ---------------------------------------------------------------------------
// The declared extension has to be the one the id stands for
// ---------------------------------------------------------------------------

/// A plugin cannot move the language it was loaded for.
///
/// groove found this file by building its name from the enabled id, so the two already claim
/// to be the same thing. A library declaring something else is a mispackaged plugin, and
/// registering what it says would take `.py` out of the index and put `.rs` in — with nothing
/// refused and nothing logged. Both orders are run because the answer must not depend on
/// whether the compiled-in grammar happened to be registered first.
#[test]
fn a_plugin_declaring_another_languages_extension_is_refused_in_either_order() {
    for enabled in ["[\"rs\", \"py\"]", "[\"py\", \"rs\"]"] {
        let layout = TempKbLayout::new("groove-plugin-mismatch");
        layout.write("notes.md", SAMPLE_MD);
        let grammars = empty_grammar_dir(&layout);
        place_plugin(&grammars, "groove_grammar_claims_rs");
        let body = format!(
            "grammar_dir = \"{}\"\nmodel = \"bge-small-en-v1.5\"\n[parsers]\nenabled = {enabled}\n",
            grammars.display().to_string().replace('\\', "/")
        );
        let cfg = layout.root().join("groove.toml");
        std::fs::write(&cfg, body).expect("write config");

        let (ok, stderr) = run_index(&cfg, layout.kb());
        assert!(
            !ok,
            "{enabled}: a mismatched extension must fail:\n{stderr}"
        );
        assert!(
            stderr.contains("but the id it was loaded for stands for"),
            "{enabled}: expected the mismatch wording:\n{stderr}"
        );
        assert!(!db_path(&layout).exists(), "{enabled}");
    }
}

// ---------------------------------------------------------------------------
// The untrusted-location rule, from outside
// ---------------------------------------------------------------------------

/// A config found rather than named cannot choose which native library is loaded.
///
/// The plugin is placed exactly where the planted config points, and the run still fails —
/// because the value was replaced with the machine's own default before the loader saw it.
/// The failure is (i), the same wording a trusted config gets when its directory is empty:
/// **trust decides which directory, never whether a missing grammar is fatal.**
#[test]
fn a_config_found_in_the_working_directory_cannot_choose_the_grammar_directory() {
    let layout = TempKbLayout::new("groove-plugin-untrusted");
    layout.write("notes.md", SAMPLE_MD);
    let grammars = empty_grammar_dir(&layout);
    place_plugin(&grammars, "groove_grammar_python");
    // Written where discovery finds it, and *not* passed with `--config`.
    write_config(&layout, Some(&grammars));

    let out = Command::new(grooveseek_bin())
        .args(["index", "--kb-path", &layout.kb().display().to_string()])
        .current_dir(layout.root())
        .output()
        .expect("groove index");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !out.status.success(),
        "the planted grammar directory must not be honoured:\n{stderr}"
    );
    assert!(
        stderr.contains("untrusted location"),
        "the substitution should be announced:\n{stderr}"
    );
    assert!(
        !stderr.contains("loaded a grammar plugin"),
        "the planted plugin must never be opened:\n{stderr}"
    );
}

// ---------------------------------------------------------------------------
// The accepting path
// ---------------------------------------------------------------------------

/// A plugin that passes every check parses its files like a compiled-in grammar would.
///
/// `#[ignore]` because this one indexes for real, which loads the BGE-small model (~130 MB).
/// Run with `cargo test --test grammar_plugin_cli -- --ignored`.
#[test]
#[ignore]
fn a_plugin_the_loader_accepts_indexes_the_files_it_claims() {
    let layout = TempKbLayout::new("groove-plugin-accepted");
    layout.write("notes.md", SAMPLE_MD);
    layout.write("sample.py", SAMPLE_PY);
    let grammars = empty_grammar_dir(&layout);
    place_plugin(&grammars, "groove_grammar_python");
    let cfg = write_config(&layout, Some(&grammars));

    let (ok, stderr) = run_index(&cfg, layout.kb());
    assert!(ok, "indexing with a valid plugin must succeed:\n{stderr}");
    assert!(
        stderr.contains("loaded a grammar plugin") || stderr.contains("fakepy"),
        "the load should be announced:\n{stderr}"
    );

    // A term that exists only inside a function body, so a hit on it can only have come from a
    // chunk the plugin's grammar produced.
    let out = Command::new(grooveseek_bin())
        .args([
            "--config",
            cfg.to_str().unwrap(),
            "search",
            "quorum drift budget",
            "--kb-path",
            &layout.kb().display().to_string(),
            "--format",
            "json",
            "--path-glob",
            "**/*.py",
        ])
        .output()
        .expect("groove search");
    assert!(
        out.status.success(),
        "search failed:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let body: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("search returns json by default");
    let results = body["results"].as_array().expect("results array");
    assert!(
        !results.is_empty(),
        "the plugin's chunks should be searchable: {body}"
    );
    let hit = &results[0];
    assert_eq!(
        hit["symbol_kind"], "function",
        "a definition chunk carries the tags vocabulary: {hit}"
    );
    assert!(
        hit["start_line"].is_number() && hit["end_line"].is_number(),
        "a definition chunk carries its line range: {hit}"
    );
}
