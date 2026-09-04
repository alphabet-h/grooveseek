//! (C-12) End-to-end coverage for the PHP grammar groove publishes.
//!
//! `grammar_plugin_cli.rs` next door covers the loader itself — every refusal, in the wording a
//! user sees, through fixtures that hand out the Rust parse table under a `py` id. Those cases
//! are about the contract and not about any one language, so they are not repeated here. What
//! is left is the chain only a real shipped grammar can close: the crate name decides the
//! library name, the library name is what the loader looks up from the `php` id, the grammar
//! parses PHP rather than something that merely loads, and the language name it declares is
//! what reaches a `lang:` filter. A rename or a mispackaging that broke any link would leave
//! every test in the neighbouring file passing.
//!
//! # Why this is its own file
//!
//! The helpers in `grammar_plugin_cli.rs` are written around one id: `PARSERS_MD_PY` is a
//! constant, and `plugin_file_name` there spells `groove_grammar_python` into a `format!`. They
//! are reached from every test in a file of some 1,300 lines. Parameterising them by language
//! would touch all of that to give one new test a home, so the few helpers this needs are
//! copied down here instead, small enough to read in one screen.
//!
//! # What is deliberately not mirrored from the Python tests
//!
//! - **The loader-acceptance test** (`a_plugin_the_loader_accepts_indexes_the_files_it_claims`).
//!   It drives the loader through a fixture that declares `py` and hands over the Rust table;
//!   the language is incidental. The loader reads its table as data, so a second copy under a
//!   second id would exercise the same code with the same inputs.
//! - **The short-definition test** (`a_short_python_constant_comes_back_without_asking_for_low_quality_hits`).
//!   That is a regression test for the quality profile in [`grooveseek::quality`]
//!   ([ADR-0015](../../docs/decisions/0015-let-a-definition-be-short.md)), which reads a chunk's
//!   `symbol_kind` and never asks which grammar produced it. A PHP twin would re-run one code
//!   path with a different grammar in front of it.
//!
//! Both are here so that a later reader adding PHP coverage "for parity" finds the reasoning
//! rather than having to re-derive it.
//!
//! # Running it
//!
//! `#[ignore]` for two reasons, the same two the Python test carries: indexing for real
//! downloads the embedding model, and the library it needs is one `cargo test` never builds,
//! because nothing in the workspace links a `cdylib` (rust-lang/cargo#8311).
//!
//! ```text
//! cargo build -p groove-grammar-php
//! cargo test --test grammar_plugin_php_cli -- --ignored
//! ```

use std::path::{Path, PathBuf};
use std::process::Command;

mod common;
use common::ansi::strip_ansi;
use common::mcp::grooveseek_bin;
use common::temp::TempKbLayout;

/// `[parsers].enabled` with PHP turned on beside Markdown.
const PARSERS_MD_PHP: &str =
    "model = \"bge-small-en-v1.5\"\n[parsers]\nenabled = [\"md\", \"php\"]\n";

/// One file carrying a definition of every kind PHP's tags query captures, plus a `const`.
///
/// The `const` is here to be absent from the results, not present in them: it is the construct a
/// reader is most likely to assume is captured — it reads like other languages' constants, and
/// Python's grammar does capture its module-level assignments — and PHP's upstream `tags.scm`
/// has no `@definition.constant`. A file that omitted it would let that gap close upstream
/// without anything noticing.
///
/// The prose in the doc comments is what the search below matches on, so it is written the way
/// a person would ask rather than as a restatement of the identifiers.
const SAMPLE_PHP_REAL: &str = r#"<?php

namespace ShardTable;

interface Rebalancer
{
    public function rebalance(int $nodes): int;
}

/** Return the shard count once a node has left the pool. */
function rebalance_shard_table(int $nodes): int
{
    return $nodes * 7;
}

class ShardTable implements Rebalancer
{
    /** How many nodes a single table will spread itself over. */
    private int $nodes;

    const MAX_NODES = 64;

    public function __construct(int $nodes)
    {
        $this->nodes = $nodes;
    }

    public function rebalance(int $nodes): int
    {
        return $nodes * 7;
    }
}
"#;

/// The file name the loader looks for, whatever this platform calls a dynamic library.
fn plugin_file_name() -> String {
    format!(
        "{}groove_grammar_php{}",
        std::env::consts::DLL_PREFIX,
        std::env::consts::DLL_SUFFIX
    )
}

/// The cargo flag that puts a build in the same profile directory this test is reading.
///
/// A repair instruction that names a directory the caller is not using is worse than none: a
/// `--release` run reads `target/release`, and a build command without the flag writes
/// `target/debug`. Derived from the path rather than from `cfg!(debug_assertions)`, which
/// describes how the *test* was compiled and not where cargo put the artifacts.
fn profile_flag(profile_dir: &Path) -> &'static str {
    match profile_dir.file_name().and_then(|n| n.to_str()) {
        Some("debug") => "",
        Some("release") => " --release",
        _ => " --profile <the profile you are running>",
    }
}

/// Where `cargo build -p groove-grammar-php` left the shipped library.
fn shipped_cdylib() -> PathBuf {
    let profile_dir = grooveseek_bin()
        .parent()
        .expect("the test binary knows where groove is")
        .to_path_buf();
    let path = profile_dir.join(plugin_file_name());
    assert!(
        path.exists(),
        "the shipped PHP grammar was not built. `cargo test` does not build a cdylib that \
         nothing depends on (rust-lang/cargo#8311). Run `cargo build -p groove-grammar-php{}` \
         first to produce {}",
        profile_flag(&profile_dir),
        path.display()
    );
    path
}

/// Make `<root>/grammars/` and return it. Nothing is put in it.
fn empty_grammar_dir(layout: &TempKbLayout) -> PathBuf {
    let dir = layout.root().join("grammars");
    std::fs::create_dir_all(&dir).expect("create grammar dir");
    dir
}

/// Write a config naming the grammar directory, and return its path.
///
/// `grammar_dir` is written with forward slashes because a TOML basic string treats a backslash
/// as an escape, and a Windows path is full of them.
fn write_config(layout: &TempKbLayout, grammar_dir: &Path) -> PathBuf {
    let body = format!(
        "grammar_dir = \"{}\"\n{PARSERS_MD_PHP}",
        grammar_dir.display().to_string().replace('\\', "/")
    );
    let cfg = layout.root().join("groove.toml");
    std::fs::write(&cfg, body).expect("write config");
    cfg
}

/// `groove --config <cfg> search <query> [--include-low-quality]`, returning the parsed body.
fn search(cfg: &Path, kb: &Path, query: &str, include_low_quality: bool) -> serde_json::Value {
    let kb = kb.display().to_string();
    let mut args = vec![
        "--config",
        cfg.to_str().unwrap(),
        "search",
        query,
        "--kb-path",
        &kb,
        "--limit",
        "10",
    ];
    if include_low_quality {
        args.push("--include-low-quality");
    }
    let out = Command::new(grooveseek_bin())
        .args(&args)
        .output()
        .expect("groove search");
    assert!(
        out.status.success(),
        "search failed:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).expect("search returns json by default")
}

/// The kinds of definition a search answered with, in the order it answered.
fn kinds_of(body: &serde_json::Value) -> Vec<&str> {
    body["results"]
        .as_array()
        .expect("results array")
        .iter()
        .filter_map(|r| r["symbol_kind"].as_str())
        .collect()
}

/// The grammar groove publishes is one its own loader accepts, and it parses PHP.
#[test]
#[ignore]
fn the_php_grammar_groove_publishes_is_one_its_loader_accepts() {
    let layout = TempKbLayout::new("groove-plugin-php");
    layout.write("shards.php", SAMPLE_PHP_REAL);
    let grammars = empty_grammar_dir(&layout);
    std::fs::copy(shipped_cdylib(), grammars.join(plugin_file_name())).expect("place plugin");
    let cfg = write_config(&layout, &grammars);

    let out = Command::new(grooveseek_bin())
        .args([
            "--config",
            cfg.to_str().unwrap(),
            "index",
            "--kb-path",
            &layout.kb().display().to_string(),
        ])
        .output()
        .expect("groove index");
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(
        out.status.success(),
        "indexing with the shipped grammar must succeed:\n{stderr}"
    );
    // The field name and its value are separated by colour codes on Windows, so the pair only
    // reads as one string once those are gone.
    let plain = strip_ansi(&stderr);
    assert!(
        plain.contains("grammar=\"php\""),
        "the load should name the language a `lang:` filter is written against:\n{plain}"
    );

    let body = search(
        &cfg,
        layout.kb(),
        "shard table rebalancing after a node leaves the pool",
        false,
    );
    let kinds = kinds_of(&body);
    assert!(
        kinds.contains(&"function") && kinds.contains(&"class") && kinds.contains(&"interface"),
        "PHP's tags query captures functions and methods, classes, interfaces and traits, \
         namespaces and properties, so a file holding one of each should reach the index with \
         them: {body}"
    );
    let tagged = body["results"]
        .as_array()
        .expect("results array")
        .iter()
        .any(|r| {
            r["tags"]
                .as_array()
                .is_some_and(|t| t.iter().any(|x| x == "lang:php"))
        });
    assert!(
        tagged,
        "the grammar's own name is what a `lang:` filter matches on: {body}"
    );
}

/// A PHP `const` is not a definition, and the docs that say so are checked here.
///
/// Asked separately from the test above, and asked the way that can actually answer it. Absence
/// from a ten-hit search for prose proves nothing: a `const` chunk could be sitting just below
/// the cut, or under the quality cutoff. So this searches for the constant's own name **with**
/// `--include-low-quality`, which leaves only one reason for `constant` not to appear — that
/// the tags query never captured it.
///
/// If this fails, `tree-sitter-php` has started capturing constants. That is a change worth
/// having, not a break to paper over: take it, and correct the same claim where it is written
/// down for users — `CHANGELOG.md`, `docs/behavior.md` and its Japanese twin, and the crate
/// docs in `crates/groove-grammar-php/src/lib.rs`.
#[test]
#[ignore]
fn a_php_const_is_not_indexed_as_a_definition() {
    let layout = TempKbLayout::new("groove-plugin-php-const");
    layout.write("shards.php", SAMPLE_PHP_REAL);
    let grammars = empty_grammar_dir(&layout);
    std::fs::copy(shipped_cdylib(), grammars.join(plugin_file_name())).expect("place plugin");
    let cfg = write_config(&layout, &grammars);

    let out = Command::new(grooveseek_bin())
        .args([
            "--config",
            cfg.to_str().unwrap(),
            "index",
            "--kb-path",
            &layout.kb().display().to_string(),
        ])
        .output()
        .expect("groove index");
    assert!(
        out.status.success(),
        "indexing with the shipped grammar must succeed:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let body = search(&cfg, layout.kb(), "MAX_NODES", true);
    assert!(
        !body["results"]
            .as_array()
            .expect("results array")
            .is_empty(),
        "the file itself must come back, or this test proves nothing about the const: {body}"
    );
    assert!(
        !kinds_of(&body).contains(&"constant"),
        "PHP's tags query has no @definition.constant, so `const MAX_NODES = 64;` should reach \
         the index as part of a line-filled chunk rather than as a definition of its own: {body}"
    );
}
