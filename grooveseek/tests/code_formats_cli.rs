//! (feature-56) End-to-end tests for source code indexed one definition at a time.
//!
//! What these cover that the parser's own unit tests cannot: the `rs` id actually reaching the
//! registry from a config file, the definition metadata surviving the round trip through
//! SQLite and back out of `groove search`, and the existing filters being enough to separate
//! code from prose without a new search parameter.
//!
//! All tests are `#[ignore]` because they spawn `groove index`, which loads the BGE-small
//! embedding model (~130 MB). Same policy as `tests/binary_formats_cli.rs`: run on demand with
//! `cargo test --test code_formats_cli -- --ignored`.

use std::path::Path;
use std::process::Command;

mod common;
use common::mcp::grooveseek_bin;
use common::temp::TempKbLayout;

/// Opts the `rs` parser in alongside the always-on default `md`.
const PARSERS_MD_RS: &str =
    "model = \"bge-small-en-v1.5\"\n[parsers]\nenabled = [\"md\", \"rs\"]\n";

/// No `[parsers]` section, so the registry falls back to `["md"]` only.
const PARSERS_DEFAULT: &str = "model = \"bge-small-en-v1.5\"\n";

/// A file with one documented function, one struct and one `impl` block.
///
/// `reciprocal_fusion_weight` is a term that appears only inside a function body, so a hit on
/// it can only have come from a code chunk.
const SAMPLE_RS: &str = r#"use std::collections::BTreeMap;
use std::fmt::Debug;

/// Combines two ranked lists into one.
///
/// The doc comment is part of the definition's chunk, not of the imports above it.
pub fn fuse_ranked_lists(a: &[usize], b: &[usize]) -> Vec<usize> {
    let reciprocal_fusion_weight = 60;
    let mut out = Vec::new();
    for (rank, id) in a.iter().enumerate() {
        out.push(id + rank + reciprocal_fusion_weight);
    }
    for (rank, id) in b.iter().enumerate() {
        out.push(id + rank + reciprocal_fusion_weight);
    }
    out
}

pub struct RankTable {
    rows: BTreeMap<usize, usize>,
}

impl RankTable {
    pub fn insert_row(&mut self, key: usize, value: usize) {
        self.rows.insert(key, value);
    }
}
"#;

/// Definitions that are one line, shorter than the quality filter's short-content threshold,
/// and carry no doc comment — the exact shape AV-07 is about. `mod shard;` names nothing but
/// itself; `type ShardId = u64;` carries a width that is written down nowhere else.
///
/// Deliberately no `const`: `tree-sitter-rust`'s tags query emits class / method / function /
/// interface / module / macro and **no constant**
/// (via: `grep -n 'definition\.' <registry>/tree-sitter-rust-0.24.2/queries/tags.scm`), so a
/// `const` item in Rust is never a definition chunk to begin with. It reaches the gap-fill
/// path instead and, under 30 characters, is dropped there. Python is the language where
/// short constants are definitions — see `grammar_plugin_cli.rs`.
const SHORT_DEFS_RS: &str = "pub mod shard;\n\ntype ShardId = u64;\n";

const SAMPLE_MD: &str = "---\ntitle: Fusion notes\n---\n\n## Reciprocal rank fusion\n\nThe prose page also talks about reciprocal fusion weight, at length, so that a search for it\nhas something to find in both halves of the knowledge base and the two can be told apart by\nwhat the response carries rather than by which one happened to win.\n";

fn write_config(layout: &TempKbLayout, body: &str) -> std::path::PathBuf {
    let cfg = layout.root().join("groove.toml");
    std::fs::write(&cfg, body).expect("write config");
    cfg
}

/// Run `groove --config <cfg> index --kb-path <kb>`, asserting exit 0.
fn run_index(bin: &Path, cfg: &Path, kb: &Path) -> String {
    let out = Command::new(bin)
        .args([
            "--config",
            cfg.to_str().unwrap(),
            "index",
            "--kb-path",
            &kb.display().to_string(),
        ])
        .output()
        .expect("groove index");
    assert!(
        out.status.success(),
        "index failed:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// Run `groove --config <cfg> search <query> --kb-path <kb> --format json [extra...]` and
/// return the `results` array.
fn run_search(
    bin: &Path,
    cfg: &Path,
    kb: &Path,
    query: &str,
    extra: &[&str],
) -> Vec<serde_json::Value> {
    let mut args = vec![
        "--config".to_string(),
        cfg.to_string_lossy().into_owned(),
        "search".to_string(),
        query.to_string(),
        "--kb-path".to_string(),
        kb.display().to_string(),
        "--format".to_string(),
        "json".to_string(),
    ];
    args.extend(extra.iter().map(|s| (*s).to_string()));
    let out = Command::new(bin)
        .args(&args)
        .output()
        .expect("groove search");
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

fn results_for(hits: &[serde_json::Value], suffix: &str) -> Vec<serde_json::Value> {
    hits.iter()
        .filter(|h| h["path"].as_str().unwrap_or_default().ends_with(suffix))
        .cloned()
        .collect()
}

#[test]
#[ignore = "spawns groove index (loads the embedding model)"]
fn a_rust_file_is_indexed_only_when_the_rs_id_is_enabled() {
    let bin = grooveseek_bin();
    let layout = TempKbLayout::new("code-optin");
    layout.write("notes.md", SAMPLE_MD);
    layout.write("fusion.rs", SAMPLE_RS);

    let default_cfg = write_config(&layout, PARSERS_DEFAULT);
    run_index(&bin, &default_cfg, layout.kb());
    let prose_only = run_search(
        &bin,
        &default_cfg,
        layout.kb(),
        "reciprocal fusion weight",
        &[],
    );
    assert!(
        results_for(&prose_only, ".rs").is_empty(),
        "the default registry must not reach a .rs file: {prose_only:#?}"
    );

    let code_cfg = write_config(&layout, PARSERS_MD_RS);
    run_index(&bin, &code_cfg, layout.kb());
    let both = run_search(
        &bin,
        &code_cfg,
        layout.kb(),
        "reciprocal fusion weight",
        &[],
    );
    assert!(
        !results_for(&both, ".rs").is_empty(),
        "enabling rs must index the source file: {both:#?}"
    );
}

#[test]
#[ignore = "spawns groove index (loads the embedding model)"]
fn a_code_hit_carries_its_line_range_and_the_grammars_word_for_the_definition() {
    let bin = grooveseek_bin();
    let layout = TempKbLayout::new("code-meta");
    layout.write("fusion.rs", SAMPLE_RS);
    let cfg = write_config(&layout, PARSERS_MD_RS);
    run_index(&bin, &cfg, layout.kb());

    let hits = run_search(&bin, &cfg, layout.kb(), "reciprocal fusion weight", &[]);
    let code = results_for(&hits, ".rs");
    assert!(!code.is_empty(), "expected a code hit: {hits:#?}");
    let hit = &code[0];

    let start = hit["start_line"]
        .as_u64()
        .unwrap_or_else(|| panic!("no start_line: {hit:#?}"));
    let end = hit["end_line"]
        .as_u64()
        .unwrap_or_else(|| panic!("no end_line: {hit:#?}"));
    assert!(
        start >= 1 && end >= start,
        "line range {start}-{end} is not a range"
    );
    // The chunk begins at the doc comment, which is above the `pub fn` line.
    let content = hit["content"].as_str().unwrap_or_default();
    if content.contains("Combines two ranked lists") {
        assert!(
            start <= 4,
            "the range must start at the doc comment, got {start}: {hit:#?}"
        );
    }
    let kind = hit["symbol_kind"].as_str().unwrap_or_default();
    assert!(
        [
            "function",
            "method",
            "class",
            "module",
            "macro",
            "interface",
            "constant"
        ]
        .contains(&kind),
        "symbol_kind {kind:?} is not a tags syntax type: {hit:#?}"
    );
}

#[test]
#[ignore = "spawns groove index (loads the embedding model)"]
fn a_prose_hit_leaves_the_three_keys_out_entirely() {
    let bin = grooveseek_bin();
    let layout = TempKbLayout::new("code-prose");
    layout.write("notes.md", SAMPLE_MD);
    let cfg = write_config(&layout, PARSERS_MD_RS);
    run_index(&bin, &cfg, layout.kb());

    let hits = run_search(&bin, &cfg, layout.kb(), "reciprocal fusion weight", &[]);
    let prose = results_for(&hits, ".md");
    assert!(!prose.is_empty(), "expected a prose hit: {hits:#?}");
    for hit in &prose {
        let obj = hit.as_object().expect("a hit is an object");
        // Absent, not null: enabling a code parser must not change the shape of a prose
        // response for a client that never asked about code.
        for key in ["start_line", "end_line", "symbol_kind"] {
            assert!(!obj.contains_key(key), "prose hit carries {key}: {hit:#?}");
        }
    }
}

#[test]
#[ignore = "spawns groove index (loads the embedding model)"]
fn code_and_prose_mix_by_default_and_the_existing_filters_separate_them() {
    let bin = grooveseek_bin();
    let layout = TempKbLayout::new("code-mix");
    layout.write("notes.md", SAMPLE_MD);
    layout.write("fusion.rs", SAMPLE_RS);
    let cfg = write_config(&layout, PARSERS_MD_RS);
    run_index(&bin, &cfg, layout.kb());

    let mixed = run_search(&bin, &cfg, layout.kb(), "reciprocal fusion weight", &[]);
    assert!(
        !results_for(&mixed, ".rs").is_empty(),
        "no code in the mix: {mixed:#?}"
    );
    assert!(
        !results_for(&mixed, ".md").is_empty(),
        "no prose in the mix: {mixed:#?}"
    );

    // No new search parameter: `tags_any` reaches the tag the code parser stamps.
    let code_only = run_search(
        &bin,
        &cfg,
        layout.kb(),
        "reciprocal fusion weight",
        &["--tag-any", "code"],
    );
    assert!(!code_only.is_empty(), "tags_any code found nothing");
    assert!(
        results_for(&code_only, ".md").is_empty(),
        "tags_any code let prose through: {code_only:#?}"
    );

    // And an exclude-only path glob is the other direction.
    let prose_only = run_search(
        &bin,
        &cfg,
        layout.kb(),
        "reciprocal fusion weight",
        &["--path-glob", "!**/*.rs"],
    );
    assert!(
        results_for(&prose_only, ".rs").is_empty(),
        "the exclusion let code through: {prose_only:#?}"
    );
}

#[test]
#[ignore = "spawns groove index (loads the embedding model)"]
fn crlf_line_endings_report_the_same_lines_as_lf() {
    let bin = grooveseek_bin();
    let layout = TempKbLayout::new("code-crlf");
    layout.write("lf.rs", SAMPLE_RS);
    // Written here rather than committed as a fixture: `.gitattributes` normalises CRLF to LF
    // on checkout, so a committed fixture would arrive as LF and prove nothing.
    layout.write("crlf.rs", &SAMPLE_RS.replace('\n', "\r\n"));
    let cfg = write_config(&layout, PARSERS_MD_RS);
    run_index(&bin, &cfg, layout.kb());

    let hits = run_search(&bin, &cfg, layout.kb(), "reciprocal fusion weight", &[]);
    let lf = results_for(&hits, "lf.rs");
    let crlf = results_for(&hits, "crlf.rs");
    assert!(
        !lf.is_empty() && !crlf.is_empty(),
        "expected both files: {hits:#?}"
    );
    assert_eq!(
        lf[0]["start_line"], crlf[0]["start_line"],
        "a carriage return moved the reported line"
    );
    assert_eq!(lf[0]["end_line"], crlf[0]["end_line"]);
}

#[test]
#[ignore = "spawns groove index (loads the embedding model)"]
fn shifting_a_definition_without_changing_its_text_moves_the_reported_lines() {
    let bin = grooveseek_bin();
    let layout = TempKbLayout::new("code-shift");
    layout.write("fusion.rs", SAMPLE_RS);
    let cfg = write_config(&layout, PARSERS_MD_RS);
    run_index(&bin, &cfg, layout.kb());

    let method_start = |hits: &[serde_json::Value]| -> u64 {
        hits.iter()
            .find(|h| h["heading"].as_str() == Some("method insert_row"))
            .and_then(|h| h["start_line"].as_u64())
            .unwrap_or_else(|| panic!("no insert_row hit with a line: {hits:#?}"))
    };

    let before = run_search(
        &bin,
        &cfg,
        layout.kb(),
        "rows insert key value",
        &["--tag-any", "code"],
    );
    let start_before = method_start(&before);

    // A blank line between the function and the struct, chosen because it changes no chunk
    // text at all: the run between those two definitions is whitespace only, so it produces
    // no chunk to differ. Every chunk body therefore matches one for one and indexing takes
    // the path that skips re-embedding -- while the definitions below have moved down a line.
    // Without the fast path also rewriting the code columns, the stored line numbers stay on
    // the previous version of the file, which is a wrong answer given confidently.
    let shifted = SAMPLE_RS.replace("\npub struct RankTable {", "\n\npub struct RankTable {");
    assert_ne!(
        shifted, SAMPLE_RS,
        "the fixture changed shape; fix the marker"
    );
    layout.write("fusion.rs", &shifted);
    run_index(&bin, &cfg, layout.kb());

    let after = run_search(
        &bin,
        &cfg,
        layout.kb(),
        "rows insert key value",
        &["--tag-any", "code"],
    );
    let start_after = method_start(&after);
    assert_eq!(
        start_after,
        start_before + 1,
        "the reported line did not follow the definition (before={start_before}, after={start_after})"
    );
}

#[test]
#[ignore = "spawns groove index (loads the embedding model)"]
fn the_command_line_prints_the_line_range_for_a_code_hit() {
    let bin = grooveseek_bin();
    let layout = TempKbLayout::new("code-text-out");
    layout.write("fusion.rs", SAMPLE_RS);
    let cfg = write_config(&layout, PARSERS_MD_RS);
    run_index(&bin, &cfg, layout.kb());

    let out = Command::new(&bin)
        .args([
            "--config",
            cfg.to_str().unwrap(),
            "search",
            "reciprocal fusion weight",
            "--kb-path",
            &layout.kb().display().to_string(),
            // `search` defaults to JSON, which already carries the fields; the point here is
            // the line a person reads.
            "--format",
            "text",
        ])
        .output()
        .expect("groove search");
    assert!(out.status.success(), "search failed");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("lines: "),
        "the text output should say where in the file the hit is:\n{stdout}"
    );
}

#[test]
#[ignore = "spawns groove index (loads the embedding model)"]
fn a_one_line_definition_comes_back_without_asking_for_low_quality_hits() {
    // AV-07. Every definition here is one line, under the quality filter's 30-character
    // short-content threshold, and has no doc comment above it -- so before v1.4.0 each
    // scored 1.0 - 0.6 - 0.3 = 0.1 and sat below the default 0.3 cutoff forever. The point
    // of the test is the **absence** of `--include-low-quality` in the search below.
    let bin = grooveseek_bin();
    let layout = TempKbLayout::new("code-short-defs");
    layout.write("shard.rs", SHORT_DEFS_RS);
    let cfg = write_config(&layout, PARSERS_MD_RS);
    run_index(&bin, &cfg, layout.kb());

    for (query, needle) in [
        ("ShardId type alias", "ShardId"),
        ("shard module declaration", "mod shard"),
    ] {
        let hits = run_search(&bin, &cfg, layout.kb(), query, &[]);
        let code = results_for(&hits, ".rs");
        let hit = code
            .iter()
            .find(|h| h["content"].as_str().unwrap_or_default().contains(needle))
            .unwrap_or_else(|| {
                panic!("{needle:?} was filtered out of a default search for {query:?}: {hits:#?}")
            });
        // Without this the test would also pass if the chunker had given up on the file and
        // returned it as line-filled gap fragments, which carry no `symbol_kind`. What has to
        // come back is the definition chunk.
        assert!(
            hit["symbol_kind"].is_string(),
            "{needle:?} came back as something other than a definition: {hit:#?}"
        );
    }

    // And no threshold takes them away again. `min_quality` is clamped to 1.0, an exempt
    // definition scores exactly 1.0, and a chunk is dropped only when its score is *below*
    // the threshold — so the highest value a caller can ask for still returns `pub mod
    // shard;`. The search path compares in `db/search.rs` rather than through
    // `passes_quality_filter`, so the unit test on that helper does not cover this.
    let ceiling = run_search(
        &bin,
        &cfg,
        layout.kb(),
        "shard module declaration",
        &["--min-quality", "1.0"],
    );
    assert!(
        results_for(&ceiling, ".rs").iter().any(|h| h["content"]
            .as_str()
            .unwrap_or_default()
            .contains("mod shard")),
        "raising min_quality to its ceiling must not be documented as a way to drop these: \
         {ceiling:#?}"
    );
}
