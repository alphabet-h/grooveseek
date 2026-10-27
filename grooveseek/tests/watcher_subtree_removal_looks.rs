//! (kuriya 557) A directory's remove event looks at the directory before it drops the rows
//! under it.
//!
//! The live watcher used to delete every row under a removed directory without looking at it
//! (feature-67's prefix sweep). When the directory was back on disk by the time the removal was
//! handled -- removed and created again within one debounce window -- the files there had their
//! rows dropped all the same: they were then embedded again from scratch, and when the new
//! directory could not be scanned they stayed out of the index until the next `groove index`.
//!
//! Each test drives `groove serve` with the watcher on against the mock endpoint of
//! [`crate::common::embed_cli`], so nothing downloads a model.
//!
//! Not on macOS: there FSEvents folds the removal and the creation into one change (kuriya trap
//! 525), so the watcher never sees the directory's removal and these paths are not taken
//! (the remove-then-write end-to-end tests say the same for their removal tests).
#![cfg(not(target_os = "macos"))]

mod common;

use common::ansi::strip_ansi;
use common::embed_cli::{Fixture, fixture};
use common::embed_mock::{hermetic, wait_until};
use common::mcp::{ServerGuard, spawn_serve_with};
use std::time::Duration;

/// Turns the watcher on with a short debounce. Appended after the `[embedding]` keys.
const WATCH_TOML: &str = "\n[watch]\nenabled = true\ndebounce_ms = 300\n";

/// A word only `sub/a.md` holds.
const A_MARKER: &str = "zephyrquill";
/// A word only `sub/b.md` holds.
const B_MARKER: &str = "marmotlantern";

/// A Markdown document titled `title` with four `##` sections, each holding `marker`.
fn note_with(title: &str, marker: &str) -> String {
    let mut md = format!("---\ntitle: {title}\n---\n\n");
    for (heading, body) in [
        (
            "Install",
            "Download the archive and unpack it into the tools folder before anything else.",
        ),
        (
            "Configure",
            "Point the config file at the shared cache directory on the build machine.",
        ),
        (
            "Verify",
            "Run the smoke check against the staging endpoint and compare the counts.",
        ),
        (
            "Rollback",
            "Restore the previous release from the archive when the smoke check fails.",
        ),
    ] {
        md.push_str(&format!("## {heading}\n\n{body} The {marker} step.\n\n"));
    }
    md
}

/// `groove index` once, then `groove serve` with the watcher on.
fn serve(fx: &Fixture) -> ServerGuard {
    fx.index();
    // `spawn_serve_with(.., true, ..)` returns only after `watcher: watching`, so the edits a
    // test makes next cannot fall before the debouncer is armed.
    let (guard, _base) = spawn_serve_with(fx.kb(), &fx.config, true, |c| {
        hermetic(c, &fx.cache);
    });
    guard
}

fn stderr_lines(guard: &ServerGuard) -> Vec<String> {
    guard
        .stderr()
        .lines()
        .iter()
        .map(|l| strip_ansi(l))
        .collect()
}

/// Wait up to 30 s for a stderr line holding `needle`; every line seen so far comes back too.
fn wait_for_line(guard: &ServerGuard, needle: &str) -> (bool, Vec<String>) {
    let seen = wait_until(Duration::from_secs(30), || {
        stderr_lines(guard).iter().any(|l| l.contains(needle))
    });
    (seen, stderr_lines(guard))
}

/// The `path` of every hit `groove search` returns for `query`.
fn hit_paths(fx: &Fixture, query: &str) -> Vec<String> {
    let hits = fx.search_json(query);
    hits.get("results")
        .and_then(|r| r.as_array())
        .unwrap_or_else(|| panic!("search JSON has no results array: {hits}"))
        .iter()
        .filter_map(|h| h.get("path").and_then(|c| c.as_str()).map(str::to_owned))
        .collect()
}

/// (kuriya 557) A directory removed and created again within one debounce window, holding one
/// of its files again with the same text, keeps that file's rows: nothing of it is embedded
/// again, and the file not written back is dropped.
///
/// The end state (one document, `sub/a.md`) is the same whether the rows were kept or dropped
/// and written again from scratch, so the test reads the requests the embedding endpoint saw:
/// a row kept in place reads the file again and finds its hash unchanged, which embeds nothing.
#[test]
fn a_directory_removed_and_recreated_in_one_window_keeps_the_rows_of_files_still_there() {
    let a_body = note_with("Alpha", A_MARKER);
    let fx = fixture(
        "i557-recreated",
        &[
            ("sub/a.md", &a_body),
            ("sub/b.md", &note_with("Beta", B_MARKER)),
        ],
        WATCH_TOML,
    );
    let guard = serve(&fx);
    assert_eq!(fx.documents(), 2);
    let before = fx.mock.requests().len();

    let sub = fx.kb().join("sub");
    std::fs::remove_dir_all(&sub).expect("remove sub");
    std::fs::create_dir(&sub).expect("create sub again");
    fx.layout.write("sub/a.md", &a_body);

    // The arrival's scan of the new directory is the last thing the watcher does for it, on
    // every platform (`Create(sub)` reaches it on Windows too).
    let arrived = "a new directory arrived";
    let (seen, lines) = wait_for_line(&guard, arrived);
    assert!(seen, "no line holding `{arrived}`:\n{}", lines.join("\n"));
    let settled = wait_until(Duration::from_secs(30), || fx.documents() == 1);
    // Leave the trailing events of the window (`Create(sub/a.md)`) time to land.
    std::thread::sleep(Duration::from_millis(1500));
    let lines = stderr_lines(&guard);
    drop(guard);
    // Counted before any search below: a query for the marker is a request holding it too.
    let embedded_a = fx
        .requests_since(before)
        .iter()
        .filter(|r| r.inputs().iter().any(|i| i.contains(A_MARKER)))
        .count();
    assert!(
        settled,
        "the index did not settle on one document:\n{}",
        lines.join("\n")
    );
    assert_eq!(fx.documents(), 1, "{}", lines.join("\n"));
    assert_eq!(
        hit_paths(&fx, A_MARKER).first().map(String::as_str),
        Some("sub/a.md")
    );
    assert!(
        !hit_paths(&fx, B_MARKER).iter().any(|p| p == "sub/b.md"),
        "sub/b.md is not on disk any more but is still indexed"
    );
    assert_eq!(
        embedded_a,
        0,
        "sub/a.md is on disk with the same text, yet its text was embedded again \
         ({embedded_a} requests): its rows were dropped by the directory's removal:\n{}",
        lines.join("\n")
    );
}

/// (kuriya 557) A directory removed and created again within one debounce window whose parent
/// has lost the search permission by the time the removal is handled keeps the rows under it:
/// the watcher cannot tell the directory is gone.
///
/// Linux only: the stat fails with `EACCES` there. On Windows a stat that meets access denied
/// is retried through the directory listing, and macOS folds the removal away (see the top of
/// this file). When the process can stat through the mode (root), the test says so on stderr
/// and checks nothing.
#[test]
#[cfg(target_os = "linux")]
fn a_removed_directory_that_cannot_be_looked_at_keeps_the_rows_under_it() {
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;

    /// Gives the directory its search permission back, so the fixture can remove it.
    struct RestoreMode(PathBuf);
    impl Drop for RestoreMode {
        fn drop(&mut self) {
            let _ = std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(0o755));
        }
    }

    let a_body = note_with("Alpha", A_MARKER);
    let fx = fixture("i557-unknown", &[("outer/sub/a.md", &a_body)], WATCH_TOML);
    let guard = serve(&fx);
    assert_eq!(fx.documents(), 1);

    let outer = fx.kb().join("outer");
    let sub = outer.join("sub");
    // Declared after the fixture and the server, so it drops first.
    let restore = RestoreMode(outer.clone());
    std::fs::remove_dir_all(&sub).expect("remove outer/sub");
    std::fs::create_dir(&sub).expect("create outer/sub again");
    fx.layout.write("outer/sub/a.md", &a_body);
    std::fs::set_permissions(&outer, std::fs::Permissions::from_mode(0o000))
        .expect("take the search permission away");
    if std::fs::metadata(&sub).is_ok() {
        drop(restore);
        eprintln!("skipped: this process can stat through a directory of mode 000 (root?)");
        return;
    }

    // The line the watcher prints for the directory's removal: before the fix
    // `deindexed 1 documents under outer/sub/`, after it the one saying it could not tell.
    let (reported, lines) = wait_for_line(&guard, "outer/sub");
    drop(restore);
    drop(guard);
    assert!(
        reported,
        "no watcher line naming outer/sub:\n{}",
        lines.join("\n")
    );
    assert_eq!(
        fx.documents(),
        1,
        "the removal dropped the rows under a directory it could not look at:\n{}",
        lines.join("\n")
    );
}

/// (kuriya 557, regression) A directory removed for good still drops every row under it, its
/// subdirectories' included, and nothing under a sibling that shares its name as a prefix.
#[test]
fn a_directory_removed_for_good_still_drops_every_row_under_it() {
    let fx = fixture(
        "i557-gone",
        &[
            ("sub/a.md", &note_with("Alpha", A_MARKER)),
            ("sub/deep/b.md", &note_with("Beta", B_MARKER)),
            ("sub-other/c.md", &note_with("Gamma", "quokkaharbor")),
        ],
        WATCH_TOML,
    );
    let guard = serve(&fx);
    assert_eq!(fx.documents(), 3);

    std::fs::remove_dir_all(fx.kb().join("sub")).expect("remove sub");
    let settled = wait_until(Duration::from_secs(30), || fx.documents() == 1);
    let lines = stderr_lines(&guard);
    drop(guard);
    assert!(
        settled,
        "the rows under the removed directory stayed:\n{}",
        lines.join("\n")
    );
    assert_eq!(
        hit_paths(&fx, "quokkaharbor").first().map(String::as_str),
        Some("sub-other/c.md")
    );
}
