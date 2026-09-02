//! AV-02: a `cdylib` no target depends on must be *built* by the job whose
//! tests open it, or those tests fail on an artefact that was never produced.
//!
//! `cargo test` builds the `[[example]]` fixtures beside it — it carries no
//! target filter, so example targets are in scope — but it does not build a
//! `cdylib` that nothing in the workspace links against
//! (rust-lang/cargo#8311). A published grammar is exactly that shape: the
//! loader opens it by path at run time, so no target names it as a dependency
//! and cargo has no reason to produce it.
//!
//! That is not a hypothetical. `groove-grammar-python` shipped in #250 with a
//! test that opens the very library the release publishes, and nothing in
//! `.github/workflows/` built it: every nightly from 2026-08-28 failed on all
//! three runners, on an assert whose own message names the command that was
//! missing. The test was right, the workflow had simply never been told.
//!
//! So this guard pins the two lists to each other, the way
//! `bench_targets_run_in_ci.rs` pins `[[bench]]` targets to the `--bench`
//! flags: every workspace member that produces a `cdylib` must be named by a
//! `cargo build -p <name>` step in the job that runs `--include-ignored`, and
//! a `-p` naming a package that is not one must be reported too, because a
//! misspelled package name builds nothing and says so only in that job's log.
//!
//! Deriving the list from the manifest rather than writing it here is what
//! makes a *future* grammar crate covered on the day it is added: a new
//! `crates/groove-grammar-<lang>` with `crate-type = ["cdylib"]` fails this
//! test until the workflow builds it.
//!
//! The workflow is read through [`crate::common::workflow`], so only what a
//! `run:` step runs is looked at; a package named in a comment does not count.

mod common;

use common::docs::repo_root;
use common::workflow::run_steps;

/// The `run:` text that marks the job whose tests need these artefacts.
///
/// The ignored tests are the ones that open a published grammar, and this flag
/// is what makes them run. Matching on the flag rather than on the job's name
/// keeps the guard pointed at the job that actually needs the build, even if
/// the job is renamed.
const IGNORED_TESTS_MARKER: &str = "--include-ignored";

/// Every workspace member whose manifest declares a `cdylib`.
///
/// Read from `[workspace] members` and each member's `[lib] crate-type`, so a
/// crate added later is covered without editing this file. `[[example]]`
/// cdylibs are deliberately not here: they live in `grooveseek`'s own manifest
/// and a plain `cargo test` builds them.
fn cdylib_packages() -> Vec<String> {
    let root = repo_root().join("Cargo.toml");
    let text =
        std::fs::read_to_string(&root).unwrap_or_else(|e| panic!("read {}: {e}", root.display()));
    let manifest: toml::Value =
        toml::from_str(&text).unwrap_or_else(|e| panic!("parse {}: {e}", root.display()));

    let members = manifest
        .get("workspace")
        .and_then(|w| w.get("members"))
        .and_then(|m| m.as_array())
        .unwrap_or_else(|| panic!("{} has no [workspace] members array", root.display()));

    let mut out = Vec::new();
    for member in members {
        let dir = member
            .as_str()
            .unwrap_or_else(|| panic!("a non-string member in {}: {member}", root.display()));
        let path = repo_root().join(dir).join("Cargo.toml");
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
        let member_manifest: toml::Value =
            toml::from_str(&text).unwrap_or_else(|e| panic!("parse {}: {e}", path.display()));

        let is_cdylib = member_manifest
            .get("lib")
            .and_then(|lib| lib.get("crate-type"))
            .and_then(|kinds| kinds.as_array())
            .is_some_and(|kinds| kinds.iter().any(|k| k.as_str() == Some("cdylib")));
        if !is_cdylib {
            continue;
        }

        let name = member_manifest
            .get("package")
            .and_then(|p| p.get("name"))
            .and_then(|n| n.as_str())
            .unwrap_or_else(|| panic!("{} declares a cdylib but no package name", path.display()));
        out.push(name.to_string());
    }
    out
}

/// The packages a `cargo build -p <name>` in `commands` names.
///
/// Whole tokens on both sides: a `contains("-p groove-grammar")` scan would
/// report a misspelled `groove-grammar-py` as covered. Only `cargo build` is
/// read — `cargo test -p x` runs tests, it does not leave a `cdylib` behind.
fn packages_built_by(commands: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    for command in commands {
        let tokens: Vec<&str> = command.split_whitespace().collect();
        let is_cargo_build = tokens
            .windows(2)
            .any(|pair| pair[0].ends_with("cargo") && pair[1] == "build");
        if !is_cargo_build {
            continue;
        }
        for pair in tokens.windows(2) {
            if pair[0] == "-p" || pair[0] == "--package" {
                out.push(pair[1].trim_matches(['"', '\'']).to_string());
            }
        }
    }
    out
}

#[test]
fn every_cdylib_the_ignored_tests_open_is_built_by_that_job() {
    let declared = cdylib_packages();
    // Anti-vacuity: if the manifest walk above ever stops finding them, the
    // comparison below would pass by examining nothing at all.
    assert!(
        !declared.is_empty(),
        "no workspace member declaring `crate-type = [\"cdylib\"]` was found — either \
         they were all removed (then this guard has nothing left to protect) or the \
         manifest walk stopped seeing them (then this guard silently stopped guarding)"
    );

    let path = repo_root()
        .join(".github")
        .join("workflows")
        .join("nightly.yml");
    let workflow = std::fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!(
            "read {}: {e} — the ignored tests run there; if the workflow moved, \
             point this guard at its new home",
            path.display()
        )
    });
    let steps = run_steps(&workflow).unwrap_or_else(|why| panic!("{} {why}", path.display()));

    // The job is found by what it runs, not by its name. A line the reader
    // could not place is kept as written, for the same reason the bench guard
    // keeps it: a line dropped in silence shrinks the set of commands, and a
    // smaller set agrees more easily.
    let job = steps
        .iter()
        .find(|step| {
            step.commands
                .iter()
                .any(|command| command.contains(IGNORED_TESTS_MARKER))
        })
        .map(|step| step.job.clone())
        .unwrap_or_else(|| {
            panic!(
                "no step in {} runs `{IGNORED_TESTS_MARKER}` — the ignored tests are what \
                 open a published grammar, so if they moved, this guard has to move with them",
                path.display()
            )
        });

    let commands: Vec<String> = steps
        .iter()
        .filter(|step| step.job == job)
        .flat_map(|step| {
            let unread = step.unread.iter().map(|line| line.raw.clone());
            step.commands.iter().cloned().chain(unread)
        })
        .collect();
    let built = packages_built_by(&commands);

    let missing: Vec<&String> = declared.iter().filter(|p| !built.contains(p)).collect();
    assert!(
        missing.is_empty(),
        "these workspace members produce a `cdylib` but are never built by the `{job}` job \
         of {}: {missing:?}. `cargo test` does not build a cdylib nothing depends on \
         (rust-lang/cargo#8311), so a test that opens one fails on a file that was never \
         produced. Add `cargo build -p <name>` to that job.",
        path.display()
    );

    let unknown: Vec<&String> = built
        .iter()
        .filter(|p| !declared.contains(p))
        .filter(|p| *p != "grooveseek")
        .collect();
    assert!(
        unknown.is_empty(),
        "the `{job}` job of {} runs `cargo build -p` for {unknown:?}, which no workspace \
         member declaring a cdylib is named. A package name cargo cannot resolve fails the \
         step; one it can resolve but that produces no cdylib builds nothing this job needs.",
        path.display()
    );
}
