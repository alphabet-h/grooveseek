//! (kuriya 627, AX-42 item 6) The documented limit of the macOS watcher, pinned from its far
//! end: whatever rows the watcher leaves after a directory is renamed and created again under
//! its old name, the next full `groove index` leaves exactly what the disk holds.
//!
//! The limit (kuriya trap 525, written down on the macOS backend in [`grooveseek::watcher`]):
//! FSEvents does not say which end of a rename a path is, so when the source directory exists
//! again by the time the event is delivered, both ends exist, the debouncer does not pair the
//! rename, and the rows under the old directory stay until the next full index. That sentence
//! promises the full index is the way out. This test holds it to that promise on the state a
//! live watcher actually produced, which no other test does: the watcher end-to-end test of the
//! same edit (the watcher test of a renamed directory whose old end is created again) is
//! compiled out on macOS because of this very limit, and the full-index test of the same edit
//! (the index test of a directory renamed and its source created again) runs no watcher, so it
//! starts from the rows of the first index rather than from what a watcher wrote.
//!
//! Of the watcher's own work, one thing is asserted: that it wrote at least one row under the
//! new name `notes/` before it was stopped. Without that the full index would start from the
//! first index's rows and the test would quietly become the watcher-less one. The arrival end
//! is indexed on macOS as well (trap 525 loses the old end, not the new one). Beyond that, the
//! rows the watcher leaves are not judged: which old rows stay depends on the platform and on
//! when its events were delivered, so an assertion on them would be a flaky gate on the macOS
//! runner. The test waits for the watcher to go quiet, so the full index runs on the watcher's
//! rows, and names those rows in its failure messages. The rest of what is asserted is the end
//! state after the full index.
//!
//! On every platform, not only macOS: the claim is that a full index after the watcher needs no
//! platform. Linux and Windows usually settle the edit in the watcher already, so there the full
//! index has little left to do, and a regression there (a row the watcher writes that a later
//! index cannot reconcile, say) fails the same assertions. The temp knowledge base sits under `/var` on macOS, which is a link to
//! `/private/var` (kuriya trap 591); `groove serve` canonicalizes the path before it watches, and
//! the rows are read back by relative path, so neither side sees the difference.

mod common;

use common::embed_cli::{Fixture, index_ok, index_ok_waiting, note, rows, served_watching};
use common::embed_mock::{assert_dir_empty, wait_until};
use common::mcp::ServerGuard;
use common::rename::rename_retrying;
use std::time::{Duration, Instant};

/// How long the test gives the watcher to write a row under the new name of the directory.
const ARRIVAL: Duration = Duration::from_secs(30);
/// How long the rows must stay unchanged for the watcher to count as quiet: ten debounce windows
/// of the `debounce_ms = 300` the fixture sets.
const QUIET: Duration = Duration::from_secs(3);
/// The longest the test waits for [`QUIET`].
const QUIET_AT_MOST: Duration = Duration::from_secs(30);

/// The paths of `rows`, in their order.
fn paths(rows: &[(String, String)]) -> Vec<&str> {
    rows.iter().map(|(p, _)| p.as_str()).collect()
}

/// Require the watcher to write a row under `notes/` within [`ARRIVAL`], then wait, without
/// asserting anything more, for it to leave the rows alone for [`QUIET`]; the rows as they stand
/// when the wait ends.
fn let_the_watcher_settle(fx: &Fixture, guard: &ServerGuard) -> Vec<(String, String)> {
    let arrived = wait_until(ARRIVAL, || {
        rows(fx).iter().any(|(p, _)| p.starts_with("notes/"))
    });
    assert!(
        arrived,
        "the watcher wrote no row under notes/ within {ARRIVAL:?}, so the full index would start \
         from the first index's rows rather than from the watcher's; rows: {:?}\nserver stderr:\n{}",
        paths(&rows(fx)),
        guard.stderr().text()
    );
    let mut last = rows(fx);
    let mut since = Instant::now();
    let end = Instant::now() + QUIET_AT_MOST;
    while Instant::now() < end && since.elapsed() < QUIET {
        std::thread::sleep(Duration::from_millis(100));
        let now = rows(fx);
        if now != last {
            last = now;
            since = Instant::now();
        }
    }
    last
}

#[test]
fn a_full_index_after_the_watcher_saw_a_dir_rename_and_recreate_leaves_no_stale_row() {
    let (fx, guard) = served_watching(
        "ax42-watch-dir-recreated",
        &[
            ("docs/a.md", &note("Alpha", "alphamarker")),
            ("docs/b.md", &note("Bravo", "bravomarker")),
        ],
    );
    assert_eq!(
        paths(&rows(&fx)),
        ["docs/a.md", "docs/b.md"],
        "premise: the first index"
    );

    // The edit of the limit: the source directory exists again before the watcher hears of the
    // rename, with a file under the old name that is another file.
    let docs = fx.kb().join("docs");
    rename_retrying(&docs, &fx.kb().join("notes"));
    std::fs::create_dir(&docs).expect("create docs again");
    fx.layout.write("docs/a.md", &note("Gamma", "gammamarker"));

    let watched = let_the_watcher_settle(&fx, &guard);
    let server_said = guard.stderr().text();
    // Stopping the server lets go of the writer lease, so nothing writes the index while the full
    // index runs. The operating system releases the lease a moment after the process is reaped,
    // so the run waits for it rather than being refused.
    drop(guard);

    let line = index_ok_waiting(&fx, 10);
    let after = rows(&fx);
    let context = format!(
        "rows the watcher left: {:?}\nindex: {line}\nserver stderr:\n{server_said}",
        paths(&watched)
    );
    assert_eq!(
        paths(&after),
        ["docs/a.md", "notes/a.md", "notes/b.md"],
        "the full index left a row the disk does not back, or lost one it does\n{context}"
    );
    let text = |rel: &str| {
        after
            .iter()
            .find(|(p, _)| p == rel)
            .map(|(_, t)| t.as_str())
            .unwrap_or_default()
    };
    assert!(
        text("docs/a.md").contains("gammamarker") && !text("docs/a.md").contains("alphamarker"),
        "docs/a.md is the recreated file, not the one that moved away\n{context}"
    );
    assert!(
        text("notes/a.md").contains("alphamarker") && !text("notes/a.md").contains("gammamarker"),
        "notes/a.md is the file that moved\n{context}"
    );
    assert!(
        text("notes/b.md").contains("bravomarker"),
        "notes/b.md is the file that moved\n{context}"
    );

    // A second run finds nothing left to do: the first one did not leave a row a later run
    // still has to remove.
    let again = index_ok(&fx);
    assert!(
        again.contains(" 0 renamed, 0 deleted,"),
        "{again}\n{context}"
    );
    assert_dir_empty(&fx.cache);
}
