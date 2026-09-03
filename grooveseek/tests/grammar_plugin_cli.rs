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
//! One test reaches that same point through `serve --port` rather than `index`:
//! [`crate::a_config_found_in_the_working_directory_cannot_choose_the_grammar_directory`]. Its input
//! is an untrusted config, so since R5 (AV-05) no plugin language is enabled at all and
//! `index` would have nothing left to refuse — it would go on to open a database and load a
//! model. `serve --port` on the default stdio transport is refused after the registry is built
//! and before a runtime exists, which keeps that test in the same cost class as its neighbours.
//!
//! # The fixtures
//!
//! `tests/fixtures/grammar_plugins/*.rs`, built as `cdylib`s by `cargo test` through the
//! `[[example]]` entries in `Cargo.toml`. They come in two families, and which family a case
//! belongs to is decided by the loader rather than by taste:
//!
//! - **Built by [`groove_grammar_abi::groove_grammar_plugin`]**, and so needing `grammar-rust`
//!   to have a parse table to hand over. These are the cases decided *after* the table has been
//!   accepted -- the extension, the name, the tags query -- which a fixture with no real table
//!   cannot reach. They hand over the Rust parse table under whatever extension the case needs;
//!   the loader checks a contract and does not know one language from another.
//! - **Hand-written, export by export**, and needing no grammar crate at all. These are the
//!   cases decided *before* the table is asked for: a version, a missing export, a NULL, a
//!   length that does not describe its pointer, bytes that are not UTF-8. Every one of them is
//!   a shape the macro cannot express, which is why they are written out by hand rather than
//!   out of preference.
//!
//! [`crate::every_grammar_fixture_the_manifest_declares_is_placed_by_a_test_in_this_file`] is
//! what keeps the manifest and this file from drifting apart.

use std::path::{Path, PathBuf};
use std::process::Command;

mod common;
use common::ansi::strip_ansi;
use common::mcp::grooveseek_bin;
use common::temp::TempKbLayout;

/// `[parsers].enabled` with the plugin language turned on beside Markdown.
const PARSERS_MD_PY: &str =
    "model = \"bge-small-en-v1.5\"\n[parsers]\nenabled = [\"md\", \"py\"]\n";

/// A file that is valid Rust, named `.py` so the fixture grammar can parse it.
///
/// The fixture declares the `py` extension and hands over the Rust parse table, so feeding it
/// Rust is what makes the definitions in the assertions real rather than a tree of errors.
/// What is under test here is the loader, not a Python grammar. The grammar groove actually
/// publishes is exercised separately, at the bottom of this file, against real Python.
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
        "the {name} fixture was not built. Run `cargo build --examples -p grooveseek{}` first, \
         or drop the `--test` filter: plain `cargo test` builds the examples as libraries, a \
         `--test`-filtered run does not, and `--examples` does not fix that -- under `cargo \
         test` it builds each example as a test executable instead. Expected {}",
        profile_flag(&profile_dir),
        path.display()
    );
    path
}

/// The cargo flag that puts a build in the same profile directory this test is reading.
///
/// A repair instruction that names a directory the caller is not using is worse than none: a
/// `--release` run reads `target/release`, and a build command without the flag writes
/// `target/debug`, so following the message leaves the file exactly as absent as before.
/// Derived from the path rather than from `cfg!(debug_assertions)`, which describes how the
/// *test* was compiled and not where cargo put the artifacts.
fn profile_flag(profile_dir: &Path) -> &'static str {
    match profile_dir.file_name().and_then(|n| n.to_str()) {
        Some("debug") => "",
        // Any other profile is reached by name; `release` has its own shorthand.
        Some("release") => " --release",
        _ => " --profile <the profile you are running>",
    }
}

/// A repair instruction has to build into the directory the test is reading.
///
/// Both "not built" messages name a `cargo build`, and both are reached from a path under
/// `target/<profile>/`. Without the flag a `--release` run is told to run a command that writes
/// `target/debug`, so following the message changes nothing and the reader concludes the build
/// is broken rather than the instruction. Cheap to get wrong again, so it is pinned here rather
/// than only in the two `assert!` strings.
#[test]
fn a_repair_instruction_names_the_profile_it_has_to_build_into() {
    assert_eq!(profile_flag(Path::new("/w/target/debug")), "");
    assert_eq!(profile_flag(Path::new("/w/target/release")), " --release");
    assert_eq!(
        profile_flag(Path::new("/w/target/dist")),
        " --profile <the profile you are running>",
        "a named profile is still reachable, and saying nothing would repeat the bug"
    );
}

/// Every grammar fixture the manifest builds is opened by a test in this file.
///
/// The failure this stops is the one `AV-09` found: a `[[example]]` declared, built by every
/// `cargo test` since, and opened by nothing -- so the check it was written for stayed
/// unverified while the build cost was paid on every run. A list with nothing to compare
/// against is how that survives, and this is the comparison.
///
/// Both halves are read at compile time, so the manifest checked is the one that built the
/// libraries this binary opens rather than whatever happens to be on disk when it runs.
///
/// One direction only, on purpose: a fixture named by a test but *not* declared in the manifest
/// already fails loudly, because [`example_cdylib`] panics naming the file it could not find.
/// The silent half is the other one.
#[test]
fn every_grammar_fixture_the_manifest_declares_is_placed_by_a_test_in_this_file() {
    const MANIFEST: &str = include_str!("../Cargo.toml");
    const THIS_FILE: &str = include_str!("grammar_plugin_cli.rs");

    let mut in_example = false;
    let mut declared = Vec::new();
    for line in MANIFEST.lines() {
        let line = line.trim();
        // A trimmed line that opens and closes with a bracket is a table header; every value in
        // this manifest that holds an array writes it on one line, after its key.
        if line.starts_with('[') && line.ends_with(']') {
            in_example = line == "[[example]]";
            continue;
        }
        if in_example
            && let Some(rest) = line.strip_prefix("name = \"")
            && let Some(name) = rest.strip_suffix('"')
        {
            declared.push(name);
        }
    }

    assert!(
        !declared.is_empty(),
        "no [[example]] name was read out of the manifest, so this guard compared nothing"
    );
    for name in declared {
        assert!(
            THIS_FILE.contains(name),
            "{name} is built by every `cargo test` and opened by no test here: either place it \
             in one, or drop its [[example]] entry"
        );
    }
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
        stderr.contains("groove-grammar-python-<target>"),
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
// (iii) Everything the loader settles before it asks for a parse table
// ---------------------------------------------------------------------------
//
// The contract version, the five remaining exports, and the bytes each of them hands back.
// None of these fixtures carries a grammar, because none of them is reached with one: the
// loader decides all of it inside `read_exports`, which runs before the table is asked for.

/// The version is settled while nothing else has been touched.
///
/// Every other export is read through the signature *this* ABI defines, so a library built at
/// another version may have dropped a symbol -- which would be reported as a missing export,
/// sending the user to look for a corrupt download rather than a mismatched version -- or kept
/// the name and changed the signature, in which case calling it is undefined behaviour.
///
/// **The second assertion is the one that pins the order.** `wrong_abi.rs` exports the version
/// and nothing else, so a loader that read the exports first could only answer "it does not
/// export groove_grammar_language". Its pair is `without_language.rs`, which lacks that same
/// export and *does* get that answer, the only difference between them being a version number
/// this groove speaks.
#[test]
fn a_contract_version_this_groove_does_not_speak_is_refused_before_any_other_export_is_read() {
    let layout = TempKbLayout::new("groove-plugin-wrongabi");
    layout.write("notes.md", SAMPLE_MD);
    let grammars = empty_grammar_dir(&layout);
    place_plugin(&grammars, "groove_grammar_wrong_abi");
    let cfg = write_config(&layout, Some(&grammars));

    let (ok, stderr) = run_index(&cfg, layout.kb());
    assert!(!ok, "a contract version mismatch must fail:\n{stderr}");
    assert!(
        stderr.contains("declares grammar ABI version"),
        "expected the version to be named as the reason:\n{stderr}"
    );
    assert!(
        !stderr.contains("does not export"),
        "the version must be settled before any export is looked up:\n{stderr}"
    );
    assert!(!db_path(&layout).exists());
}

/// Each export the contract names is looked up, and the one that is absent is the one named.
///
/// The five here are the exports read after the version, one fixture per missing symbol. The
/// realistic way to break any of these lines is not to delete it -- `get(lib, symbols::X)?` is
/// load-bearing, so a deletion does not compile -- but to look up the wrong constant, and a
/// ladder is the only thing that catches that: with `symbols::LANGUAGE` swapped for
/// `symbols::NAME`, the library missing the language export is no longer refused for it.
///
/// **What this does not pin is the order of the five among themselves.** A library missing
/// exactly one export names that one whichever order the lookups happen in, so the prose in
/// [`groove_grammar_abi::symbols`] is not observable from out here beyond its head -- and the
/// head is pinned twice already, by `no_symbols.rs` (the version is looked up first) and by
/// `wrong_abi.rs` (the language is the first read after it).
#[test]
fn every_export_the_contract_names_is_required_and_named_when_it_is_missing() {
    for (fixture, symbol) in [
        ("groove_grammar_without_language", "groove_grammar_language"),
        ("groove_grammar_without_name", "groove_grammar_name"),
        (
            "groove_grammar_without_extensions",
            "groove_grammar_extensions",
        ),
        (
            "groove_grammar_without_tags_query",
            "groove_grammar_tags_query",
        ),
        (
            "groove_grammar_without_build_info",
            "groove_grammar_build_info",
        ),
    ] {
        let layout = TempKbLayout::new("groove-plugin-ladder");
        layout.write("notes.md", SAMPLE_MD);
        let grammars = empty_grammar_dir(&layout);
        place_plugin(&grammars, fixture);
        let cfg = write_config(&layout, Some(&grammars));

        let (ok, stderr) = run_index(&cfg, layout.kb());
        assert!(!ok, "{fixture}: a missing export must fail:\n{stderr}");
        assert!(
            stderr.contains(&format!("does not export {symbol}")),
            "{fixture}: expected {symbol} to be named:\n{stderr}"
        );
        assert!(!db_path(&layout).exists(), "{fixture}");
    }
}

/// A declared length is checked before it is used to build a slice.
///
/// The tags query crosses the ABI as a pointer and a length, and nothing makes the two agree.
/// The fixture's pointer is real and two bytes long, so a loader that trusted the length would
/// hand `slice::from_raw_parts` a gigabyte starting at a two-byte static: a read off the end of
/// the mapping, with no refusal and no diagnostic. That is the same "dies without a word"
/// signature the NULL exports had.
///
/// The assertions read prose, not numbers: the cap is a refusal threshold this build happens to
/// hold, and a test naming it would have to be edited the day it moves.
#[test]
fn a_tags_query_longer_than_this_build_reads_is_refused_by_the_length_it_declares() {
    let layout = TempKbLayout::new("groove-plugin-hugetags");
    layout.write("notes.md", SAMPLE_MD);
    let grammars = empty_grammar_dir(&layout);
    place_plugin(&grammars, "groove_grammar_huge_tags_query");
    let cfg = write_config(&layout, Some(&grammars));

    let (ok, stderr) = run_index(&cfg, layout.kb());
    assert!(!ok, "an oversized tags query must fail:\n{stderr}");
    assert!(
        stderr.contains("declares a tags query of"),
        "expected the declared length to be named:\n{stderr}"
    );
    assert!(
        stderr.contains("this build reads at most"),
        "expected the message to say what this build will read:\n{stderr}"
    );
    assert!(!db_path(&layout).exists());
}

/// The query's bytes are validated rather than assumed to be text.
///
/// The query is data -- it may embed a NUL, which is why it crosses as a pointer and a length
/// rather than as a C string -- so on this side of the boundary it is bytes and nothing but the
/// loader's own check says otherwise. Reaching for `from_utf8_unchecked` to save a pass over a
/// few kilobytes would turn a plugin's own bytes into undefined behaviour.
#[test]
fn a_tags_query_that_is_not_utf8_is_refused_rather_than_read_as_text() {
    let layout = TempKbLayout::new("groove-plugin-badutf8tags");
    layout.write("notes.md", SAMPLE_MD);
    let grammars = empty_grammar_dir(&layout);
    place_plugin(&grammars, "groove_grammar_not_utf8_tags_query");
    let cfg = write_config(&layout, Some(&grammars));

    let (ok, stderr) = run_index(&cfg, layout.kb());
    assert!(!ok, "a tags query that is not UTF-8 must fail:\n{stderr}");
    assert!(
        stderr.contains("its tags query is not valid UTF-8"),
        "expected the encoding to be named as the reason:\n{stderr}"
    );
    assert!(!db_path(&layout).exists());
}

/// A string export that hands back NULL is answered as NULL, not as bad UTF-8.
///
/// The three string exports are copied out through one helper, and that helper checks the
/// pointer before `CStr::from_ptr`, which has none of its own and would dereference NULL.
/// Folding the two answers together would make the friendliest possible mistake into undefined
/// behaviour, and would send the reader looking for an encoding problem in a string that was
/// never read.
///
/// The name is the first of the three copied out, so it is the one that reaches the line at
/// all; which export was NULL comes from an argument, and the wording for all three is pinned
/// by the unit tests beside the loader rather than by three fixtures here.
#[test]
fn a_string_export_that_hands_back_null_is_named_as_null_rather_than_as_bad_utf8() {
    let layout = TempKbLayout::new("groove-plugin-nullname");
    layout.write("notes.md", SAMPLE_MD);
    let grammars = empty_grammar_dir(&layout);
    place_plugin(&grammars, "groove_grammar_null_name");
    let cfg = write_config(&layout, Some(&grammars));

    let (ok, stderr) = run_index(&cfg, layout.kb());
    assert!(!ok, "a NULL name must fail:\n{stderr}");
    assert!(
        stderr.contains("its name export returned NULL"),
        "expected the NULL to be named as the reason:\n{stderr}"
    );
    assert!(
        !stderr.contains("not valid UTF-8"),
        "a NULL is not an encoding problem, and saying so sends the reader nowhere:\n{stderr}"
    );
    assert!(!db_path(&layout).exists());
}

/// A NUL-terminated string that is not UTF-8 is refused, and the export it came from is named.
///
/// The other half of the same helper. A C string is a run of bytes ending in NUL and nothing
/// more, so this is what a plugin written in C hands over without meaning anything by it. The
/// name becomes the `lang:` tag on every chunk and is leaked as a `&'static str`, so bytes that
/// are not text would be carried the whole length of the index.
#[test]
fn a_string_export_that_is_not_utf8_is_refused_by_the_export_it_came_from() {
    let layout = TempKbLayout::new("groove-plugin-badutf8name");
    layout.write("notes.md", SAMPLE_MD);
    let grammars = empty_grammar_dir(&layout);
    place_plugin(&grammars, "groove_grammar_not_utf8_name");
    let cfg = write_config(&layout, Some(&grammars));

    let (ok, stderr) = run_index(&cfg, layout.kb());
    assert!(!ok, "a name that is not UTF-8 must fail:\n{stderr}");
    assert!(
        stderr.contains("its name is not valid UTF-8"),
        "expected the export and the encoding to be named:\n{stderr}"
    );
    assert!(
        !stderr.contains("returned NULL"),
        "a pointer that was read is not a NULL one:\n{stderr}"
    );
    assert!(!db_path(&layout).exists());
}

// ---------------------------------------------------------------------------
// An export that hands back NULL is refused, not dereferenced
// ---------------------------------------------------------------------------

/// A plugin that exports the contract and hands back no grammar is refused with a sentence.
///
/// This is the one malformed shape that used to end the process instead: `abi_version`
/// dereferences the parse-table pointer with no check of its own, so the run died where every
/// other bad plugin gets a line naming the file and the reason. Under the Windows service,
/// which discards stdio, it died silently.
///
/// The fixture is hand-written because [`groove_grammar_abi::groove_grammar_plugin`] cannot
/// express it — the macro builds this export from a real grammar's `LanguageFn`.
#[test]
fn a_plugin_that_hands_back_no_grammar_is_refused_rather_than_dereferenced() {
    let layout = TempKbLayout::new("groove-plugin-nullgrammar");
    layout.write("notes.md", SAMPLE_MD);
    let grammars = empty_grammar_dir(&layout);
    place_plugin(&grammars, "groove_grammar_null_language");
    let cfg = write_config(&layout, Some(&grammars));

    let (ok, stderr) = run_index(&cfg, layout.kb());
    assert!(!ok, "a NULL grammar must fail:\n{stderr}");
    assert!(
        stderr.contains("its grammar export returned NULL"),
        "expected the NULL grammar to be named as the reason:\n{stderr}"
    );
    assert!(!db_path(&layout).exists());
}

/// "No tags query", written the obvious way, is refused rather than read.
///
/// A NULL pointer with a length of zero is what a plugin author reaches for to say the grammar
/// has no tags query, and `slice::from_raw_parts` requires a non-NULL pointer even then. So the
/// friendliest possible mistake was undefined behaviour.
#[test]
fn a_null_tags_query_is_refused_rather_than_read_as_an_empty_one() {
    let layout = TempKbLayout::new("groove-plugin-nulltags");
    layout.write("notes.md", SAMPLE_MD);
    let grammars = empty_grammar_dir(&layout);
    place_plugin(&grammars, "groove_grammar_null_tags");
    let cfg = write_config(&layout, Some(&grammars));

    let (ok, stderr) = run_index(&cfg, layout.kb());
    assert!(!ok, "a NULL tags query must fail:\n{stderr}");
    assert!(
        stderr.contains("its tags query export returned NULL"),
        "expected the NULL tags query to be named as the reason:\n{stderr}"
    );
    assert!(!db_path(&layout).exists());
}

/// The pointer the loader checks has to be the pointer it uses.
///
/// `tree_sitter::Language` can only be built by *calling* a `LanguageFn`, so handing it the
/// plugin's own export would put the check and the use one call apart. This fixture answers
/// with a real parse table once and NULL after, which nothing in the ABI forbids — so a loader
/// that checked the first answer would dereference the second, and the run would end without a
/// word instead of being refused.
///
/// The refusal it does reach is the extension mismatch, several checks later. Reaching a later
/// check at all is the evidence.
#[test]
fn a_grammar_export_that_answers_twice_is_used_on_the_answer_that_was_checked() {
    let layout = TempKbLayout::new("groove-plugin-flaky");
    layout.write("notes.md", SAMPLE_MD);
    let grammars = empty_grammar_dir(&layout);
    place_plugin(&grammars, "groove_grammar_flaky_language");
    let cfg = write_config(&layout, Some(&grammars));

    let (ok, stderr) = run_index(&cfg, layout.kb());
    assert!(!ok, "a mismatched extension must fail:\n{stderr}");
    assert!(
        stderr.contains("but the id it was loaded for stands for"),
        "expected the run to reach the extension check, which it can only do if the table it \
         verified is the table it used:\n{stderr}"
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
// (iv) The strings a plugin declares about itself
// ---------------------------------------------------------------------------
//
// Checked after the parse table has been accepted, so every fixture here carries a real
// grammar and needs `grammar-rust` -- one without a table would be refused several checks
// earlier and never reach the line it is for. A plugin is arbitrary native code, so what it
// says about itself is checked like any other untrusted input before it reaches a filesystem
// walk or the index.

/// Two extensions in one declaration is refused as two, before either is checked for validity.
///
/// [`groove_grammar_abi::EXTENSION_SEPARATOR`] is reserved for a future grammar that claims
/// more than one, and this build does not speak it yet. The order matters to the reader rather
/// than to the machine: "you declared two" and "that is not an extension" send them to
/// different places, and the validity rule refuses both -- which is what the second assertion
/// watches for, since `"py;pyi"` would fail validity too if the separator check went away.
#[test]
fn a_plugin_claiming_more_than_one_extension_is_refused_before_the_extension_is_validated() {
    let layout = TempKbLayout::new("groove-plugin-twoext");
    layout.write("notes.md", SAMPLE_MD);
    let grammars = empty_grammar_dir(&layout);
    place_plugin(&grammars, "groove_grammar_two_extensions");
    let cfg = write_config(&layout, Some(&grammars));

    let (ok, stderr) = run_index(&cfg, layout.kb());
    assert!(!ok, "two declared extensions must fail:\n{stderr}");
    assert!(
        stderr.contains("claims more than one file extension"),
        "expected the count to be named as the reason:\n{stderr}"
    );
    assert!(
        !stderr.contains("which is not lowercase ASCII"),
        "the separator is answered before validity, or the reader is sent to the wrong \
         place:\n{stderr}"
    );
    assert!(!db_path(&layout).exists());
}

/// An extension groove could not key a parser by is refused as an extension.
///
/// groove keys parsers by a bare lowercase extension, so the leading dot is the mistake an
/// author makes on the first try. The rule is applied to what the *library* says, not only to
/// what a config says.
#[test]
fn an_extension_groove_cannot_key_a_parser_by_is_refused_as_an_extension() {
    let layout = TempKbLayout::new("groove-plugin-dottedext");
    layout.write("notes.md", SAMPLE_MD);
    let grammars = empty_grammar_dir(&layout);
    place_plugin(&grammars, "groove_grammar_bad_extension");
    let cfg = write_config(&layout, Some(&grammars));

    let (ok, stderr) = run_index(&cfg, layout.kb());
    assert!(!ok, "a dotted extension must fail:\n{stderr}");
    assert!(
        stderr.contains("claims the file extension"),
        "expected the declared extension to be quoted back:\n{stderr}"
    );
    assert!(
        stderr.contains("which is not lowercase ASCII"),
        "expected the rule it broke to be stated:\n{stderr}"
    );
    assert!(!db_path(&layout).exists());
}

/// A language name no `lang:` filter could match is refused rather than carried into the index.
///
/// The name is not decoration: it becomes `lang:<name>` on every chunk the grammar produces.
/// A name with a space in it is a grammar that loads and then cannot be searched by language --
/// a failure with no message anywhere, found by a user whose filter silently matches nothing.
///
/// The fixture declares `py`, the id it is loaded under, because the extension is matched
/// against that id first; anything else would be refused for the mismatch and never reach here.
#[test]
fn a_language_name_no_lang_filter_could_match_is_refused() {
    let layout = TempKbLayout::new("groove-plugin-badname");
    layout.write("notes.md", SAMPLE_MD);
    let grammars = empty_grammar_dir(&layout);
    place_plugin(&grammars, "groove_grammar_bad_name");
    let cfg = write_config(&layout, Some(&grammars));

    let (ok, stderr) = run_index(&cfg, layout.kb());
    assert!(!ok, "an unsearchable language name must fail:\n{stderr}");
    assert!(
        stderr.contains("it calls its language"),
        "expected the declared name to be quoted back:\n{stderr}"
    );
    assert!(!db_path(&layout).exists());
}

/// A tags query that does not compile against its own grammar is a refusal, not a panic.
///
/// The query is compiled while the plugin is being accepted, which is the last thing the loader
/// does. The alternative to refusing here is a grammar that loads and then produces no
/// definitions for any file it claims -- an empty result that looks like a knowledge base with
/// nothing in it.
///
/// `((` is unbalanced against every grammar, which keeps the fixture about the check rather
/// than about a node name a future `tree-sitter-rust` might rename.
#[test]
fn a_tags_query_that_does_not_compile_against_its_own_grammar_is_refused() {
    let layout = TempKbLayout::new("groove-plugin-brokenquery");
    layout.write("notes.md", SAMPLE_MD);
    let grammars = empty_grammar_dir(&layout);
    place_plugin(&grammars, "groove_grammar_uncompilable_tags_query");
    let cfg = write_config(&layout, Some(&grammars));

    let (ok, stderr) = run_index(&cfg, layout.kb());
    assert!(!ok, "an uncompilable tags query must fail:\n{stderr}");
    assert!(
        stderr.contains("its tags query does not compile"),
        "expected the query to be named as the reason:\n{stderr}"
    );
    assert!(!db_path(&layout).exists());
}

// ---------------------------------------------------------------------------
// The untrusted-location rule, from outside
// ---------------------------------------------------------------------------

/// A config found rather than named cannot bring a plugin into the process.
///
/// The plugin is placed exactly where the planted config points, and it is still never
/// opened — because both of the values that would reach it were replaced with the
/// machine's own defaults before the loader could be asked for anything.
///
/// **The probe is `serve --port`, not `index`.** Since R5 (AV-05) the planted
/// `[parsers].enabled` is dropped too, so `py` is no longer enabled and nothing asks
/// where plugins live; `index` would therefore go on to open a database and load a
/// model, which is a cost this file does not pay (see the header). `serve --port` on the
/// default stdio transport is refused *after* the parser registry is built and *before* a
/// runtime exists, so it reaches the same decision and stops on its own.
///
/// The last assertion is the one R5 owns: with that rule removed, `py` survives, the
/// registry construction goes looking for the library and names the file it wants.
#[test]
fn a_config_found_in_the_working_directory_cannot_choose_the_grammar_directory() {
    let layout = TempKbLayout::new("groove-plugin-untrusted");
    layout.write("notes.md", SAMPLE_MD);
    let grammars = empty_grammar_dir(&layout);
    place_plugin(&grammars, "groove_grammar_python");
    // Written where discovery finds it, and *not* passed with `--config`.
    write_config(&layout, Some(&grammars));

    let out = Command::new(grooveseek_bin())
        .args([
            "serve",
            "--kb-path",
            &layout.kb().display().to_string(),
            "--port",
            "3100",
        ])
        .current_dir(layout.root())
        .output()
        .expect("groove serve");
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
    assert!(
        !stderr.contains(&plugin_file_name()),
        "an untrusted config must not turn a plugin language on at all:\n{stderr}"
    );
}

// ---------------------------------------------------------------------------
// The accepting path
// ---------------------------------------------------------------------------

/// A plugin that passes every check parses its files like a compiled-in grammar would.
///
/// `#[ignore]` because this one indexes for real, which loads the BGE-small model (~130 MB).
/// Run with `cargo build --examples -p grooveseek` followed by
/// `cargo test --test grammar_plugin_cli -- --ignored`.
///
/// **The build step is separate because no form of `cargo test` that also filters targets will
/// produce these libraries.** A `--test`-filtered run does not build examples at all, and
/// adding `--examples` does not repair it: under `cargo test` that flag means *test the
/// examples*, so cargo builds each one as a test executable
/// (`target/<profile>/examples/<name>-<hash>.exe`) and never as the `cdylib` the loader has to
/// open. Measured by deleting the library, touching the fixture source, and running
/// `cargo test --examples --test grammar_plugin_cli --no-run`: it recompiles and lists them as
/// executables, and `ls target/debug/examples/groove_grammar_python.dll` still reports no such
/// file. Plain `cargo test` with no target filter does build them, which is why CI — which runs
/// exactly that — has always had them.
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

/// Real Python, for the grammar groove publishes rather than the fixture's Rust table.
///
/// One of each definition kind Python's `tags.scm` captures — a module-level assignment, a
/// function, a class — so the assertion below is about the vocabulary reaching the index and
/// not about one lucky node type.
///
/// The assignment is the one to keep: it is easy to assume a tags query only captures `def` and
/// `class`, and review did assume it. Indexing this exact file returns
/// `constant QUORUM_DRIFT_BUDGET` at lines 3-3 with `symbol_kind` `constant`, beside
/// `function rebalance_shard_table` 6-8 and `class ShardTable` 11-13, which is why the test
/// below asserts all three kinds rather than the two obvious ones.
const SAMPLE_PY_REAL: &str = r#"import re

QUORUM_DRIFT_BUDGET = re.compile(r"^drift-[0-9]+$")


def rebalance_shard_table(nodes):
    """Return the shard count after a node leaves."""
    return nodes * 7


class ShardTable:
    def __init__(self, nodes):
        self.nodes = nodes
"#;

/// Where `cargo build -p groove-grammar-python` left the shipped library.
///
/// The profile directory, **not** `examples/` beside it: the fixture above is named
/// `groove_grammar_python` too, on purpose, because both have to answer to the one name the
/// loader builds from the `py` id. They coexist because cargo writes examples one directory
/// down — so the only thing separating the grammar groove ships from a test fixture that
/// hands out Rust is which of these two paths a test reads.
fn shipped_cdylib() -> PathBuf {
    let profile_dir = grooveseek_bin()
        .parent()
        .expect("the test binary knows where groove is")
        .to_path_buf();
    let path = profile_dir.join(plugin_file_name());
    assert!(
        path.exists(),
        "the shipped Python grammar was not built. `cargo test` does not build a cdylib that \
         nothing depends on (rust-lang/cargo#8311), and `--examples` only reaches the fixtures \
         beside it. Run `cargo build -p groove-grammar-python{}` first to produce {}",
        profile_flag(&profile_dir),
        path.display()
    );
    path
}

/// The grammar groove publishes is one its own loader accepts, and it parses Python.
///
/// This is the end of the chain the other tests only cover in halves: the crate name decides
/// the library name, the library name is what the loader looks up from the `py` id, and the
/// archive a diagnostic tells the user to download is that same name in cargo's other
/// spelling. A rename that broke any link would leave every other test here passing.
///
/// `#[ignore]` for the same reason as the test above (it indexes for real, ~130 MB of model),
/// and additionally because the library it needs is one `cargo test` does not build at all:
/// cargo does not build a `cdylib` nothing depends on (rust-lang/cargo#8311). Run with
///
/// ```text
/// cargo build -p groove-grammar-python
/// cargo build --examples -p grooveseek
/// cargo test --test grammar_plugin_cli -- --ignored
/// ```
///
/// **The two builds produce different files and neither substitutes for the other.**
/// `-p groove-grammar-python` writes the shipped library to `target/<profile>/`, and
/// `--examples -p grooveseek` writes the fixtures one directory down. The run selects every
/// ignored test in this file, so omitting the second command fails the test above rather than
/// this one — see the note there for why no `cargo test` flag replaces it.
#[test]
#[ignore]
fn the_python_grammar_groove_publishes_is_one_its_loader_accepts() {
    let layout = TempKbLayout::new("groove-plugin-python");
    layout.write("shards.py", SAMPLE_PY_REAL);
    let grammars = empty_grammar_dir(&layout);
    std::fs::copy(shipped_cdylib(), grammars.join(plugin_file_name())).expect("place plugin");
    let cfg = write_config(&layout, Some(&grammars));

    let (ok, stderr) = run_index(&cfg, layout.kb());
    assert!(
        ok,
        "indexing with the shipped grammar must succeed:\n{stderr}"
    );
    // The field name and its value are separated by colour codes on Windows, so the pair only
    // reads as one string once those are gone.
    let plain = strip_ansi(&stderr);
    assert!(
        plain.contains("grammar=\"python\""),
        "the load should name the language a `lang:` filter is written against:\n{plain}"
    );

    let out = Command::new(grooveseek_bin())
        .args([
            "--config",
            cfg.to_str().unwrap(),
            "search",
            "shard table rebalancing after a node leaves",
            "--kb-path",
            &layout.kb().display().to_string(),
            "--limit",
            "10",
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

    let kinds: Vec<&str> = results
        .iter()
        .filter_map(|r| r["symbol_kind"].as_str())
        .collect();
    // All three kinds, not just the two obvious ones. `constant` is the one worth pinning: it
    // comes from a module-level assignment rather than a `def` or a `class`, it is the kind the
    // CHANGELOG promises by name, and it is the one a reader is most likely to assume Python's
    // tags query does not capture.
    assert!(
        kinds.contains(&"function") && kinds.contains(&"class") && kinds.contains(&"constant"),
        "Python's tags query captures functions, classes and module-level assignments, so all \
         three should reach the index: {body}"
    );
    let tagged = results.iter().any(|r| {
        r["tags"]
            .as_array()
            .is_some_and(|t| t.iter().any(|x| x == "lang:python"))
    });
    assert!(
        tagged,
        "the grammar's own name is what a `lang:` filter matches on: {body}"
    );
}
