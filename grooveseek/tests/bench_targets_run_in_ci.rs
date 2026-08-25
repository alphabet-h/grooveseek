//! BU-25: every `[[bench]]` target must actually be *run* by CI, not just built.
//!
//! `cargo clippy --all-targets` (ci.yml, with and without `heavy-bench`) compiles
//! the benches on every PR, so a bench that stops *building* is caught within
//! minutes. What nothing caught until BU-25 is a bench that still builds while no
//! longer benchmarking anything — the failure AU-56 found in `search_latency`,
//! where an absent index made every iteration measure the cost of starting the
//! binary and finding zero hits, and criterion reported the numbers anyway.
//!
//! nightly.yml therefore runs the bench targets in criterion's test mode, listing
//! them by name: `--benches` would also re-run the entire unit-test suite, since
//! lib and bin targets carry `bench = true` by default. Naming them one by one is
//! what makes a bench added later easy to leave out, so this test pins the two
//! lists to each other — in both directions, because a name that is wrong in the
//! workflow would otherwise only surface in the next nightly run.
//!
//! The workflow is read through `common::workflow`, so only what a `run:` step
//! runs is looked at; the file mentions the flag in comments too.

mod common;

use common::docs::repo_root;
use common::workflow::run_steps;
use std::path::Path;

/// The `name` of every `[[bench]]` entry in groove's manifest.
fn declared_bench_targets() -> Vec<String> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
    let text =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    let manifest: toml::Value =
        toml::from_str(&text).unwrap_or_else(|e| panic!("parse {}: {e}", path.display()));

    manifest
        .get("bench")
        .and_then(|benches| benches.as_array())
        .map(|benches| {
            benches
                .iter()
                .map(|bench| {
                    bench
                        .get("name")
                        .and_then(|name| name.as_str())
                        .unwrap_or_else(|| panic!("a [[bench]] entry without a name: {bench}"))
                        .to_string()
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Every bench target named after a `--bench` flag in the commands.
///
/// The input is what the workflow's `run:` steps run, one command per line,
/// so a `--bench` in a comment or a step name is not here to be counted. The
/// shape filter stays: a `--bench` that ends a line, or is followed by another
/// flag, names no target, and a misspelled target still looks like a name and
/// is still caught.
fn bench_targets_referenced_by(workflow: &str) -> Vec<&str> {
    workflow
        .split("--bench ")
        .skip(1)
        .filter_map(|rest| rest.split_whitespace().next())
        .filter(|token| {
            !token.is_empty()
                && token
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        })
        .collect()
}

#[test]
fn every_bench_target_is_executed_by_the_nightly_workflow() {
    let declared = declared_bench_targets();
    // Anti-vacuity: if the manifest is ever restructured so that `[[bench]]`
    // entries stop being found here, the comparison below would pass by
    // examining nothing at all.
    assert!(
        !declared.is_empty(),
        "no [[bench]] target was found in grooveseek/Cargo.toml — either they were \
         all removed (then this guard has nothing left to protect) or the parse \
         above stopped seeing them (then this guard silently stopped guarding)"
    );

    let path = repo_root()
        .join(".github")
        .join("workflows")
        .join("nightly.yml");
    let workflow = std::fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!(
            "read {}: {e} — the nightly workflow is where the benches are run; \
             if it moved, point this guard at its new home",
            path.display()
        )
    });

    // What the steps run, and nothing else in the file. Before this the whole
    // text was scanned, and a target that survived only in a comment counted
    // as run.
    let commands = run_steps(&workflow)
        .unwrap_or_else(|why| panic!("{} {why}", path.display()))
        .into_iter()
        .flat_map(|step| step.commands)
        .collect::<Vec<_>>()
        .join("\n");

    // Both directions compare whole tokens. A `contains("--bench <name>")` scan
    // would report `search` as covered because `--bench search_latency` starts
    // with it.
    let referenced = bench_targets_referenced_by(&commands);

    let missing: Vec<&str> = declared
        .iter()
        .map(String::as_str)
        .filter(|name| !referenced.contains(name))
        .collect();
    assert!(
        missing.is_empty(),
        "these bench targets are declared in grooveseek/Cargo.toml but never run by \
         nightly.yml: {missing:?}. Add `--bench <name>` to the criterion test-mode \
         step, or they compile forever without anyone noticing they stopped \
         measuring anything (BU-25 / AU-56)."
    );

    let unknown: Vec<&str> = referenced
        .into_iter()
        .filter(|name| !declared.iter().any(|declared| declared == name))
        .collect();
    assert!(
        unknown.is_empty(),
        "nightly.yml runs `--bench` on targets that grooveseek/Cargo.toml does not \
         declare: {unknown:?}. cargo would fail the nightly run a day later; fix \
         the name here instead."
    );
}
