//! The commands that only read the index -- `groove status`, `groove search`,
//! `groove graph`, `groove doctor`, `groove eval` and `groove tune` -- open it
//! as a reader (ADR-0031): in the production lines of the binary's crate root
//! and of [`grooveseek::tune`] and [`grooveseek::eval`] there is no writer
//! open, only [`grooveseek::db::Database::open_read_only`]. `groove index`
//! opens its writer through [`grooveseek::db::Database::open_writer`] /
//! [`grooveseek::db::Database::open_or_replace_corrupt_with`], whose needles
//! below do not match.

use std::path::{Path, PathBuf};

mod common;
use common::production::{fn_ranges, production_lines};

fn src(file: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("src").join(file);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

#[test]
fn readers_in_main_tune_and_eval_never_open_the_index_for_writing() {
    for (file, marker, min_reader_opens) in [
        ("main.rs", "fn main(", 4),
        ("tune.rs", "pub fn run(opts: &TuneOpts)", 1),
        ("eval.rs", "pub fn compute_query_metrics(", 1),
    ] {
        let text = src(file);
        let prod = production_lines(&text);
        assert!(
            prod.iter().any(|(_, l)| l.contains(marker)),
            "{file}: marker {marker} not in production lines"
        );
        assert!(
            prod.iter()
                .filter(|(_, l)| l.contains("Database::open_read_only("))
                .count()
                >= min_reader_opens,
            "{file}: fewer than {min_reader_opens} reader opens"
        );
        for needle in ["Database::open(", "Database::open_or_replace_corrupt("] {
            let hits: Vec<_> = prod.iter().filter(|(_, l)| l.contains(needle)).collect();
            assert!(hits.is_empty(), "{file}: {needle} in production: {hits:?}");
        }
    }
}

/// The `.rs` files under `dir`, recursively, sorted.
fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let mut entries: Vec<_> = std::fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("read {}: {e}", dir.display()))
        .map(|e| e.expect("a directory entry").path())
        .collect();
    entries.sort();
    for path in entries {
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|x| x == "rs") {
            out.push(path);
        }
    }
}

/// The name of `mod <name>;` (also `pub mod` / `pub(crate) mod`), a module
/// declared in a file of its own.
fn out_of_line_mod(line: &str) -> Option<&str> {
    let t = line.trim();
    let t = t
        .strip_prefix("pub(crate) ")
        .or_else(|| t.strip_prefix("pub "))
        .unwrap_or(t);
    let name = t.strip_prefix("mod ")?.strip_suffix(';')?.trim();
    (!name.is_empty() && name.chars().all(|c| c.is_alphanumeric() || c == '_')).then_some(name)
}

/// The files and directories a `#[cfg(test)]` `mod <name>;` in `file`
/// points at: `<dir>/<name>.rs` and `<dir>/<name>/`, where `<dir>` is the
/// directory of `file` for `lib.rs` / `main.rs` / `mod.rs`, and the
/// directory named after `file` otherwise.
fn test_only_module_paths(file: &Path, text: &str) -> Vec<PathBuf> {
    let parent = file.parent().expect("a file has a parent");
    let stem = file.file_stem().and_then(|s| s.to_str()).unwrap_or("");
    let dir = if matches!(stem, "lib" | "main" | "mod") {
        parent.to_path_buf()
    } else {
        parent.join(stem)
    };
    let lines: Vec<&str> = text.lines().collect();
    let mut out = Vec::new();
    for pair in lines.windows(2) {
        if pair[0].trim() == "#[cfg(test)]"
            && let Some(name) = out_of_line_mod(pair[1])
        {
            out.push(dir.join(format!("{name}.rs")));
            out.push(dir.join(name));
        }
    }
    out
}

/// No production line of the crate opens the index for writing
/// outside the lease. `Database::open(` and `Database::open_or_replace_corrupt(`
/// appear only in tests; production opens a reader
/// (`Database::open_read_only(`) or a writer under a held lease
/// (`Database::open_writer(` / `Database::open_or_replace_corrupt_with(`,
/// which these needles do not match). The needles are the calls to
/// [`grooveseek::db::Database::open`],
/// [`grooveseek::db::Database::open_or_replace_corrupt`],
/// [`grooveseek::db::Database::open_read_only`],
/// [`grooveseek::db::Database::open_writer`] and
/// [`grooveseek::db::Database::open_or_replace_corrupt_with`].
#[test]
fn no_source_file_opens_the_index_for_writing_outside_the_lease() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    rust_files(&root, &mut files);
    let mut test_only = Vec::new();
    for file in &files {
        let text = std::fs::read_to_string(file).unwrap();
        test_only.extend(test_only_module_paths(file, &text));
    }
    assert!(
        test_only.contains(&root.join("test_support.rs")),
        "the test-only test_support module must be found: {test_only:?}"
    );
    let scanned: Vec<&PathBuf> = files
        .iter()
        .filter(|f| !test_only.iter().any(|t| f.starts_with(t)))
        .collect();
    for must in ["server.rs", "writer_lease.rs", "server/writer_slot.rs"] {
        let path = root.join(must);
        assert!(
            scanned.contains(&&path),
            "{} must be scanned",
            path.display()
        );
    }
    assert!(
        !scanned.contains(&&root.join("test_support.rs")),
        "test_support.rs is test-only"
    );

    let mut hits = Vec::new();
    for file in &scanned {
        let text = std::fs::read_to_string(file).unwrap();
        for (n, line) in production_lines(&text) {
            for needle in ["Database::open(", "Database::open_or_replace_corrupt("] {
                if line.contains(needle) {
                    hits.push(format!("{}:{n}: {}", file.display(), line.trim()));
                }
            }
        }
    }
    assert!(hits.is_empty(), "writer opens outside the lease: {hits:#?}");

    let server = src("server.rs");
    let prod = production_lines(&server);
    assert!(
        prod.iter()
            .any(|(_, l)| l.contains("pub async fn run_server(")),
        "server.rs: run_server not in production lines"
    );
    assert!(
        prod.iter()
            .any(|(_, l)| l.contains("Database::open_read_only(")),
        "server.rs: no reader open in production lines"
    );
}

/// The only line range in the source of [`grooveseek::server`] that starts
/// with `needle`.
fn the_one_range(text: &str, needle: &str) -> (usize, usize) {
    let ranges = fn_ranges(text, needle);
    assert_eq!(
        ranges.len(),
        1,
        "exactly one {needle} in server.rs: {ranges:?}"
    );
    ranges[0]
}

/// The production line numbers within `range` that contain `needle`.
fn lines_with(prod: &[(usize, &str)], range: (usize, usize), needle: &str) -> Vec<usize> {
    prod.iter()
        .filter(|(n, l)| *n >= range.0 && *n <= range.1 && l.contains(needle))
        .map(|(n, _)| *n)
        .collect()
}

/// `groove serve` claims the writer role and starts the janitor before it
/// loads the models, so the heartbeat runs through the download; the model
/// loading itself is the closure [`grooveseek::server::run_server`] passes in.
#[test]
fn run_server_with_spawns_the_janitor_before_the_model_download() {
    let text = src("server.rs");
    let prod = production_lines(&text);
    let with = the_one_range(&text, "async fn run_server_with(");
    let first = |needle: &str| -> usize {
        let at = lines_with(&prod, with, needle);
        assert!(!at.is_empty(), "{needle} not in run_server_with");
        at[0]
    };
    let claim = first("claim_writer_role(");
    let janitor = first("spawn_janitor(");
    let serve = first("serve_claimed(");
    assert!(
        claim < janitor && janitor < serve,
        "claim {claim} < janitor {janitor} < serve_claimed {serve}"
    );
    assert!(
        lines_with(&prod, with, "Embedder::with_settings(").is_empty(),
        "the model download is not in run_server_with"
    );
    let run = the_one_range(&text, "pub async fn run_server(");
    assert!(
        !lines_with(&prod, run, "Embedder::with_settings(").is_empty(),
        "run_server passes the model download in"
    );
}

/// The writes that wrap themselves in a transaction when the caller has none
/// open start it as `BEGIN IMMEDIATE`, through
/// `Database::begin_local_immediate` (crate-private, in [`grooveseek::db`]):
/// their first statement reads, and a deferred transaction would read on a
/// snapshot another writer can commit past before the write takes the lock. In
/// the production lines of the storage submodule of [`grooveseek::db`] no
/// deferred `unchecked_transaction(` is left, and neither is one in
/// [`grooveseek::db::Database::reset_for_model`] (the rest of the meta
/// submodule keeps its read-only transactions).
#[test]
fn self_wrapping_writes_open_their_own_transaction_immediate() {
    let storage = src("db/storage.rs");
    let storage_prod = production_lines(&storage);
    let deferred: Vec<usize> = storage_prod
        .iter()
        .filter(|(_, l)| l.contains("unchecked_transaction("))
        .map(|(n, _)| *n)
        .collect();
    assert!(
        deferred.is_empty(),
        "db/storage.rs: deferred self-wrapping transaction at lines {deferred:?}"
    );
    // Self-check that the scan sees the replaced call sites at all; a lower
    // bound, so adding a call does not need this test edited.
    let immediate = storage_prod
        .iter()
        .filter(|(_, l)| l.contains("begin_local_immediate()"))
        .count();
    assert!(
        immediate >= 7,
        "db/storage.rs: {immediate} begin_local_immediate() calls, expected at least 7"
    );

    let meta = src("db/meta.rs");
    let meta_prod = production_lines(&meta);
    let ranges = fn_ranges(&meta, "pub fn reset_for_model(");
    assert_eq!(
        ranges.len(),
        1,
        "exactly one reset_for_model in db/meta.rs: {ranges:?}"
    );
    let reset = ranges[0];
    assert!(
        lines_with(&meta_prod, reset, "unchecked_transaction(").is_empty(),
        "reset_for_model opens a deferred transaction"
    );
    assert_eq!(
        lines_with(&meta_prod, reset, "begin_local_immediate()").len(),
        1,
        "reset_for_model opens its own transaction through begin_local_immediate()"
    );
}

/// (kuriya 259) In the watcher, a row leaves on an event without a look at the disk inside the
/// database lock only for a name the index cannot hold: the one production call of
/// `dispatch_deindex` is in `handle_removed_path`; every other event-driven deletion goes
/// through `dispatch_removal`.
#[test]
fn only_a_name_the_index_cannot_hold_is_deindexed_without_a_look() {
    let text = src("watcher.rs");
    let prod = production_lines(&text);
    let ranges = fn_ranges(&text, "fn handle_removed_path(");
    assert_eq!(
        ranges.len(),
        1,
        "exactly one handle_removed_path in watcher.rs: {ranges:?}"
    );
    let removed = ranges[0];
    let deindex: Vec<(usize, &str)> = prod
        .iter()
        .filter(|(_, l)| l.contains("dispatch_deindex(") && !l.contains("fn dispatch_deindex("))
        .map(|(n, l)| (*n, l.trim()))
        .collect();
    assert_eq!(
        deindex.len(),
        1,
        "watcher.rs: dispatch_deindex calls in production: {deindex:?}"
    );
    assert!(
        deindex[0].0 >= removed.0 && deindex[0].0 <= removed.1,
        "watcher.rs: the dispatch_deindex call {deindex:?} is not in handle_removed_path {removed:?}"
    );
    // Self-check that the scan sees the replaced call sites at all; a lower bound, so adding a
    // call does not need this test edited. `dispatch_removal(` and not `dispatch_removal(state`:
    // rustfmt breaks a long call after the parenthesis.
    let removal = prod
        .iter()
        .filter(|(_, l)| l.contains("dispatch_removal(") && !l.contains("fn dispatch_removal("))
        .count();
    assert!(
        removal >= 6,
        "watcher.rs: {removal} dispatch_removal( calls in production, expected at least 6"
    );
}
