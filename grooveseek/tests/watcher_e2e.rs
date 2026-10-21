//! F-57: Watcher real-disk end-to-end smoke.
//!
//! Spawns `groove serve --transport http` *with the watcher enabled*
//! (= no `--no-watch`), creates a brand-new `.md` file in the watched
//! KB directory, waits for the debouncer + reindex tick, and verifies
//! that an MCP `search` call now finds the new content.
//!
//! Exercises the full real-disk pipeline:
//!   notify-debouncer-full -> bridge thread (mpsc) ->
//!   tokio task (run_watch_loop) -> classify -> indexer::reindex_single_file.
//!
//! `#[ignore]` because the test:
//! - spawns a groove subprocess (~5-7 sec wall clock)
//! - downloads BGE-small (~130 MB) on first run
//! - depends on inotify (Linux) / FSEvents (macOS) / ReadDirectoryChangesW
//!   (Windows). Linux runners are most stable; macOS / Windows are
//!   best-effort opt-in.
//!
//! Run:
//!   cargo test --release --test watcher_e2e -- --ignored
//!
//! See spec `.dev/specs/feature-37-watcher-e2e-index-bench.md` for the
//! design rationale (Cycle C). Re-uses `tests/common/mcp.rs` (= F-55)
//! via the new `spawn_mcp_server_with_watch` helper appended in F-57.

mod common;
use common::mcp::{
    build_index, extract_path_heading_order, mcp_initialize, mcp_search_call,
    spawn_mcp_server_with_watch,
};
use common::temp::TempKbLayout;

use std::thread::sleep;
use std::time::{Duration, Instant};

/// Place a single `.md` file under `layout.kb()` so the initial
/// `build_index` run has something to chunk + embed.
fn setup_initial_kb(layout: &TempKbLayout) {
    layout.write(
        "initial.md",
        concat!(
            "---\ntitle: Initial Doc\ntags: [rust, async]\n---\n",
            "\n",
            "## tokio\n",
            "\n",
            "Initial baseline content. This file exists before the watcher\n",
            "starts and acts as a sanity guard that the index is non-empty.\n",
        ),
    );
}

/// Distinct token that we will search for after creating `freshly_added.md`.
/// Chosen to *not* appear in the initial KB so a hit unambiguously proves
/// the watcher picked up the new file.
const FRESH_MARKER: &str = "watchersurfaceuniquemarker";

#[test]
#[ignore = "spawns groove serve with watcher; needs inotify (Linux primary; opt-in on macOS/Windows)"]
fn test_watcher_picks_up_new_file() {
    let layout = TempKbLayout::new("groove-watcher-e2e");
    setup_initial_kb(&layout);
    build_index(layout.kb());

    // Minimal config + watch enabled (= debounce 500ms default, but
    // pin it explicitly so this test does not depend on future default
    // changes).
    let cfg_path = layout.root().join("groove.toml");
    std::fs::write(&cfg_path, "[watch]\nenabled = true\ndebounce_ms = 500\n")
        .expect("write groove.toml");

    let (_guard, base) = spawn_mcp_server_with_watch(layout.kb(), &cfg_path);
    let session = mcp_initialize(&base);

    // (AU-55) No sleep here any more. `spawn_mcp_server_with_watch` now returns
    // only once the watcher has printed that it is armed, which is the one
    // moment this test actually depends on. The old fixed 2 s wait was the only
    // timing construct in the suite that could not recover: if the debouncer
    // was not ready when the file landed, the event was gone and the 8 s poll
    // loop below had nothing left to find.

    // *** Drop a brand-new file into the watched directory ***.
    layout.write(
        "freshly_added.md",
        &format!(
            concat!(
                "---\ntitle: Freshly Added\ntags: [test]\n---\n",
                "\n",
                "## fresh\n",
                "\n",
                "Distinct content with the marker `{}` so search assertions can\n",
                "prove this file got indexed by the watcher (and not by\n",
                "`build_index` above).\n",
            ),
            FRESH_MARKER,
        ),
    );

    // Poll `mcp_search` until the watcher has indexed `freshly_added.md`
    // or `deadline` expires. Replaces a previous fixed `sleep(3000)`
    // (codex review P2): on slower CI hosts the watcher can index just
    // past a fixed deadline and produce a false failure. Polling is
    // bounded by `deadline` so we still surface a real hang.
    //
    // Deadline budget = debounce window (500ms) + handle_events (db
    // lock + embed + commit) + flush. Empirically ~1-1.5s on Linux;
    // 8s gives plenty of headroom for slower CI hosts (mirrors the
    // wait_http_200 pattern).
    let deadline = Duration::from_millis(8000);
    let poll_interval = Duration::from_millis(250);
    let start = Instant::now();
    let order_at_deadline = loop {
        let resp = mcp_search_call(
            &base,
            &session,
            serde_json::json!({
                "query": FRESH_MARKER,
                "limit": 5,
                "mmr": false,
            }),
        );
        let order = extract_path_heading_order(&resp);
        if order
            .iter()
            .any(|(path, _heading)| path.ends_with("freshly_added.md"))
        {
            return;
        }
        if start.elapsed() >= deadline {
            break order;
        }
        sleep(poll_interval);
    };
    panic!(
        "watcher did not surface `freshly_added.md` within {deadline:?}; \
         last search result {order_at_deadline:?}.\n\
         If this is intermittent on macOS/Windows, the test is best-effort \
         opt-in there (Linux is primary). On Linux, increase the deadline \
         or investigate handle_events latency.",
    );
}

/// (feature-49) A `.grooveignore` written while the server is running takes
/// effect for subsequent events.
///
/// The reload is reachable only because `handle_events` looks for the file
/// **before** classifying anything: `.grooveignore` has no registered
/// extension, so the ordinary path would drop the event on the same filter the
/// file is meant to change, and the watcher would go on indexing what the next
/// `groove index` will drop.
///
/// Two files are written afterwards, not one. "The ignored file never appears"
/// passes just as well when the watcher is dead, when the debouncer never
/// armed, or when the deadline was too short — so a second, *not* ignored file
/// has to appear in the same window for the negative half to mean anything.
#[test]
#[ignore = "spawns groove serve with watcher; needs inotify (Linux primary; opt-in on macOS/Windows)"]
fn watcher_reloads_grooveseekignore_while_running() {
    const IGNORED_MARKER: &str = "zqxwignoredmarker49";
    const VISIBLE_MARKER: &str = "zqxwvisiblemarker49";

    let layout = TempKbLayout::new("groove-watcher-ignore-e2e");
    setup_initial_kb(&layout);
    build_index(layout.kb());

    let cfg_path = layout.root().join("groove.toml");
    std::fs::write(&cfg_path, "[watch]\nenabled = true\ndebounce_ms = 500\n")
        .expect("write groove.toml");

    let (guard, base) = spawn_mcp_server_with_watch(layout.kb(), &cfg_path);
    let session = mcp_initialize(&base);

    // The server started without one, so this write is the reload.
    layout.write(".grooveignore", "secret/\n");

    // Let that land in its own debounce window. Sharing one with the writes
    // below would still work — the reload runs before the batch is classified —
    // but a note landing in an *earlier* batch than the ignore file would be
    // indexed, and the failure would look like a broken reload rather than a
    // racy fixture.
    //
    // (AV-32) Waited for, not slept through. `.grooveignore` has no parser, so
    // `search` cannot show the reload — but the watcher says it on stderr the
    // moment `reload_rules` has run, and that line is written from inside the
    // batch that carried the ignore file, so seeing it means that batch is
    // closed and the writes below cannot fall into it. A fixed 2 s was four
    // debounce windows on a fast host and no guarantee on a slow one; this
    // takes the same 8 s budget as the search polls and fails loudly instead of
    // quietly landing the fixture in the wrong batch.
    let reload_line = "watcher: reloaded .grooveignore";
    let deadline = Duration::from_millis(8000);
    let poll_interval = Duration::from_millis(250);
    let start = Instant::now();
    while !guard.stderr().contains(reload_line) {
        assert!(
            start.elapsed() < deadline,
            "the watcher never reported reloading .grooveignore within {deadline:?}; \
             stderr so far: {:?}",
            guard.stderr().lines()
        );
        sleep(poll_interval);
    }

    // Both bodies have to be substantial. The per-chunk quality filter is on by
    // default at threshold 0.3, and a one-line section under 30 characters
    // scores below it — so a terse fixture is hidden from `search` whether or
    // not it was indexed, which would make the negative assertion below pass
    // for the wrong reason and the positive one fail for it.
    let body = |marker: &str| {
        format!(
            concat!(
                "---\ntitle: Watcher ignore fixture\ntags: [test]\n---\n",
                "\n",
                "## marker section\n",
                "\n",
                "Distinct content carrying the marker `{}` so the search assertions\n",
                "can tell whether the live watcher indexed this file. Written long\n",
                "enough to clear the default per-chunk quality filter, which drops\n",
                "short single-line sections from search results regardless of\n",
                "whether they reached the index.\n",
            ),
            marker,
        )
    };
    layout.write("secret/hidden.md", &body(IGNORED_MARKER));
    layout.write("public/shown.md", &body(VISIBLE_MARKER));

    // Same budget as the test above.
    let deadline = Duration::from_millis(8000);
    let poll_interval = Duration::from_millis(250);
    let start = Instant::now();
    let mut saw_visible = false;
    loop {
        let hits = |marker: &str| {
            let resp = mcp_search_call(
                &base,
                &session,
                serde_json::json!({ "query": marker, "limit": 5, "mmr": false }),
            );
            extract_path_heading_order(&resp)
        };

        let ignored = hits(IGNORED_MARKER);
        assert!(
            !ignored
                .iter()
                .any(|(path, _)| path.ends_with("hidden.md") || path.contains("secret/")),
            "the watcher indexed a file under a directory the reloaded \
             .grooveignore excludes: {ignored:?}"
        );

        if !saw_visible
            && hits(VISIBLE_MARKER)
                .iter()
                .any(|(path, _)| path.ends_with("shown.md"))
        {
            saw_visible = true;
        }

        if start.elapsed() >= deadline {
            break;
        }
        sleep(poll_interval);
    }

    assert!(
        saw_visible,
        "`public/shown.md` never appeared within {deadline:?}, so the watcher \
         was not doing anything and the assertion about the ignored file proved \
         nothing. Investigate the watcher before trusting this test."
    );
}

/// (feature-67, kuriya issue #479) A directory whose files are removed one by
/// one and then the directory itself, all inside one debounce window (what
/// `git rm -r` or a branch switch does), leaves nothing of it in the index.
///
/// notify-debouncer-full drops a child's Remove when its parent's Remove lands
/// in the same window, so only the directory's event may reach groove. The
/// watcher has to expand that one event from its own store.
///
/// The `sub/` documents disappearing is what proves the watcher was alive.
/// `sub-other/` shares the name prefix without the `/` and must survive: that is
/// the anchoring check only, since its survival alone is indistinguishable from
/// nothing having happened. Only the end
/// state is asserted, so a platform whose backend does not fold the events
/// (single-file deletes then do the work) stays green.
#[test]
#[ignore = "spawns groove serve with watcher; needs inotify (Linux primary; opt-in on macOS/Windows)"]
fn watcher_deindexes_a_subdirectory_removed_file_by_file() {
    const A_MARKER: &str = "zqxwsubdiramarker67";
    const B_MARKER: &str = "zqxwsubdirbmarker67";
    const C_MARKER: &str = "zqxwsubdircmarker67";

    // Long enough to clear the default per-chunk quality filter (see the test
    // above): a terse body would be hidden from `search` whether or not it was
    // indexed.
    let body = |marker: &str| {
        format!(
            concat!(
                "---\ntitle: Watcher subtree fixture\ntags: [test]\n---\n",
                "\n",
                "## marker section\n",
                "\n",
                "Distinct content carrying the marker `{}` so the search assertions\n",
                "can tell whether this file is still in the index. Written long\n",
                "enough to clear the default per-chunk quality filter, which drops\n",
                "short single-line sections from search results regardless of\n",
                "whether they reached the index.\n",
            ),
            marker,
        )
    };

    let layout = TempKbLayout::new("groove-watcher-subtree-e2e");
    layout.write("sub/a.md", &body(A_MARKER));
    layout.write("sub/b.md", &body(B_MARKER));
    layout.write("sub-other/c.md", &body(C_MARKER));
    build_index(layout.kb());

    let cfg_path = layout.root().join("groove.toml");
    std::fs::write(&cfg_path, "[watch]\nenabled = true\ndebounce_ms = 500\n")
        .expect("write groove.toml");

    let (guard, base) = spawn_mcp_server_with_watch(layout.kb(), &cfg_path);
    let session = mcp_initialize(&base);

    let hits = |marker: &str| {
        let resp = mcp_search_call(
            &base,
            &session,
            serde_json::json!({ "query": marker, "limit": 5, "mmr": false }),
        );
        extract_path_heading_order(&resp)
    };
    let has = |marker: &str, path: &str| hits(marker).iter().any(|(p, _)| p.ends_with(path));

    // Sanity: the fixture is searchable before anything is removed, so a
    // missing hit below means "deindexed", not "never indexed".
    assert!(
        has(A_MARKER, "sub/a.md"),
        "sub/a.md not indexed to begin with"
    );
    assert!(
        has(B_MARKER, "sub/b.md"),
        "sub/b.md not indexed to begin with"
    );
    assert!(
        has(C_MARKER, "sub-other/c.md"),
        "sub-other/c.md not indexed to begin with"
    );

    // As git does it: the files first, then the directory, back to back.
    let sub = layout.kb().join("sub");
    std::fs::remove_file(sub.join("a.md")).expect("remove sub/a.md");
    std::fs::remove_file(sub.join("b.md")).expect("remove sub/b.md");
    std::fs::remove_dir(&sub).expect("remove sub/");

    // Same budget as the tests above.
    let deadline = Duration::from_millis(8000);
    let poll_interval = Duration::from_millis(250);
    let start = Instant::now();
    loop {
        let a_left = has(A_MARKER, "sub/a.md");
        let b_left = has(B_MARKER, "sub/b.md");
        if !a_left && !b_left {
            break;
        }
        assert!(
            start.elapsed() < deadline,
            "documents under the removed `sub/` were still searchable after \
             {deadline:?} (sub/a.md: {a_left}, sub/b.md: {b_left}); stderr: {:?}",
            guard.stderr().lines()
        );
        sleep(poll_interval);
    }
    assert!(
        has(C_MARKER, "sub-other/c.md"),
        "`sub-other/c.md` shares the name prefix without the `/` and must survive \
         the removal of `sub/`; stderr: {:?}",
        guard.stderr().lines()
    );
}

/// (feature-68) `std::fs::rename` that retries a few times on
/// `PermissionDenied` only, then panics with the last error.
///
/// A directory written into a moment ago can be held open briefly by another
/// process (Windows Defender and similar scanners are the suspects; this is
/// unconfirmed), and Windows refuses to rename a directory something holds.
/// The same shape of failure, a process holding a directory and stopping its
/// rename, is trap 23 of the skill `windows-quirks`. Any other error kind
/// fails at once: retrying would only hide a real bug in the fixture.
///
/// 20 attempts 100 ms apart (about 2 s in all) is a guess at "a scanner's
/// brief look", well under the 8 s poll budget the tests spend afterwards.
fn rename_retrying(from: &std::path::Path, to: &std::path::Path) {
    const ATTEMPTS: u32 = 20;
    const INTERVAL: Duration = Duration::from_millis(100);
    let mut attempt = 1;
    loop {
        match std::fs::rename(from, to) {
            Ok(()) => return,
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied && attempt < ATTEMPTS => {
                attempt += 1;
                sleep(INTERVAL);
            }
            Err(e) => panic!(
                "rename {} -> {} failed after {attempt} attempt(s): {e}",
                from.display(),
                to.display()
            ),
        }
    }
}

/// (feature-68) An indexed directory renamed while the watcher runs is
/// searchable under its new path and no longer under its old one.
///
/// Why this is red without the fix (Windows): `docs/` exists before the watch
/// starts, so notify-debouncer-full's file-id cache holds it from the initial
/// walk and stitches the rename into one event carrying both paths. Such a
/// paired rename of a directory is skipped today, and the debouncer emits no
/// events for the children, so the `docs/` rows stay and nothing appears under
/// `notes/`. Linux stitches through the inotify cookie and is expected to be
/// red the same way (unverified). After the end state, the test also asserts
/// the watcher's line for a paired directory rename, so a platform that does
/// not stitch and handles the two halves through the existing paths is red
/// rather than green: the test proves the paired path ran.
///
/// `docs-other/` shares the name prefix without the `/` and must survive.
///
/// On macOS this and the other directory-rename tests failed in nightly run
/// 39471586474 (2026-10-21): FSEvents hands back flags a path collected
/// earlier, so the rename arrived behind a stale `Create` and the debouncer
/// dropped it. The watcher drops such stale events before the debouncer now
/// (kuriya issue #520), and the tests run there again.
#[test]
#[ignore = "spawns groove serve with watcher; needs inotify (Linux primary; opt-in on macOS/Windows)"]
fn watcher_moves_an_indexed_directory_on_rename() {
    const A_MARKER: &str = "zqxwdirrenameamarker68";
    const B_MARKER: &str = "zqxwdirrenamebmarker68";
    const C_MARKER: &str = "zqxwdirrenamecmarker68";

    // Long enough to clear the default per-chunk quality filter (see
    // `watcher_reloads_grooveseekignore_while_running`): a terse body would be
    // hidden from `search` whether or not it was indexed.
    let body = |marker: &str| {
        format!(
            concat!(
                "---\ntitle: Watcher dir rename fixture\ntags: [test]\n---\n",
                "\n",
                "## marker section\n",
                "\n",
                "Distinct content carrying the marker `{}` so the search assertions\n",
                "can tell under which path this file is in the index. Written long\n",
                "enough to clear the default per-chunk quality filter, which drops\n",
                "short single-line sections from search results regardless of\n",
                "whether they reached the index.\n",
            ),
            marker,
        )
    };

    let layout = TempKbLayout::new("groove-watcher-dirrename-e2e");
    layout.write("docs/a.md", &body(A_MARKER));
    layout.write("docs/b.md", &body(B_MARKER));
    layout.write("docs-other/c.md", &body(C_MARKER));
    build_index(layout.kb());

    let cfg_path = layout.root().join("groove.toml");
    std::fs::write(&cfg_path, "[watch]\nenabled = true\ndebounce_ms = 500\n")
        .expect("write groove.toml");

    let (guard, base) = spawn_mcp_server_with_watch(layout.kb(), &cfg_path);
    let session = mcp_initialize(&base);

    let hits = |marker: &str| {
        let resp = mcp_search_call(
            &base,
            &session,
            serde_json::json!({ "query": marker, "limit": 5, "mmr": false }),
        );
        extract_path_heading_order(&resp)
    };
    let has = |marker: &str, path: &str| hits(marker).iter().any(|(p, _)| p.ends_with(path));

    // Sanity: the fixture is searchable under the old paths before the rename,
    // so a missing hit below means "moved", not "never indexed".
    assert!(
        has(A_MARKER, "docs/a.md"),
        "docs/a.md not indexed to begin with"
    );
    assert!(
        has(B_MARKER, "docs/b.md"),
        "docs/b.md not indexed to begin with"
    );
    assert!(
        has(C_MARKER, "docs-other/c.md"),
        "docs-other/c.md not indexed to begin with"
    );

    rename_retrying(&layout.kb().join("docs"), &layout.kb().join("notes"));

    // Same budget as the tests above.
    let deadline = Duration::from_millis(8000);
    let poll_interval = Duration::from_millis(250);
    let start = Instant::now();
    loop {
        let new_a = has(A_MARKER, "notes/a.md");
        let new_b = has(B_MARKER, "notes/b.md");
        let old_a = has(A_MARKER, "docs/a.md");
        let old_b = has(B_MARKER, "docs/b.md");
        if new_a && new_b && !old_a && !old_b {
            break;
        }
        assert!(
            start.elapsed() < deadline,
            "the renamed `docs/` did not move to `notes/` in the index within {deadline:?} (notes/a.md: {new_a}, notes/b.md: {new_b}, docs/a.md: {old_a}, docs/b.md: {old_b}); stderr: {:?}",
            guard.stderr().lines()
        );
        sleep(poll_interval);
    }
    assert!(
        has(C_MARKER, "docs-other/c.md"),
        "`docs-other/c.md` shares the name prefix without the `/` and must survive the rename of `docs/`; stderr: {:?}",
        guard.stderr().lines()
    );
    // The end state alone could be reached by an unpaired rename too; this line
    // is printed only by the paired directory-rename path.
    assert!(
        guard
            .stderr()
            .contains("watcher: a renamed directory held 2 indexed documents"),
        "the end state was reached without the paired directory-rename path; stderr: {:?}",
        guard.stderr().lines()
    );
}

/// (feature-68) Files written into a directory that is then renamed are
/// indexed under the new path: the "write under a temporary name, then rename
/// into place" pattern.
///
/// The directory has to be created in an earlier debounce window than the
/// rename. Inside one window the debouncer rewrites the directory's Create to
/// the new path and emits no rename at all, which the watcher already handles.
/// So the test waits for the seed file to be searchable under `tmp/` before
/// it writes the second file and renames.
///
/// Why this is red without the fix (Windows): `tmp/` entered the debouncer's
/// file-id cache on its Create, so the rename is stitched into one event with
/// both paths, and that paired directory rename is skipped today. The
/// `tmp/seed.md` row stays, the child Create for `tmp/a.md` comes out under the
/// old path and is skipped because that file no longer exists, so nothing is
/// searchable under `final/`. Linux stitches through the inotify cookie and is
/// expected to be red the same way (unverified). A platform that does not
/// stitch handles the two halves through the existing paths and stays green
/// without the fix: only the end state is asserted here (the moves test above
/// asserts that the paired path ran).
///
/// The seed row also exercises the rewrite of an already indexed row, so this
/// one test covers the rewrite, the sweep of the old prefix and the scan of
/// the arrival.
#[test]
#[ignore = "spawns groove serve with watcher; needs inotify (Linux primary; opt-in on macOS/Windows)"]
fn watcher_indexes_files_written_into_a_directory_renamed_within_the_window() {
    const SEED_MARKER: &str = "zqxwdirwindowseedmarker68";
    const A_MARKER: &str = "zqxwdirwindowamarker68";

    // Long enough to clear the default per-chunk quality filter, as above.
    let body = |marker: &str| {
        format!(
            concat!(
                "---\ntitle: Watcher dir window fixture\ntags: [test]\n---\n",
                "\n",
                "## marker section\n",
                "\n",
                "Distinct content carrying the marker `{}` so the search assertions\n",
                "can tell under which path this file is in the index. Written long\n",
                "enough to clear the default per-chunk quality filter, which drops\n",
                "short single-line sections from search results regardless of\n",
                "whether they reached the index.\n",
            ),
            marker,
        )
    };

    let layout = TempKbLayout::new("groove-watcher-dirwindow-e2e");
    setup_initial_kb(&layout);
    build_index(layout.kb());

    let cfg_path = layout.root().join("groove.toml");
    std::fs::write(&cfg_path, "[watch]\nenabled = true\ndebounce_ms = 500\n")
        .expect("write groove.toml");

    let (guard, base) = spawn_mcp_server_with_watch(layout.kb(), &cfg_path);
    let session = mcp_initialize(&base);

    let hits = |marker: &str| {
        let resp = mcp_search_call(
            &base,
            &session,
            serde_json::json!({ "query": marker, "limit": 5, "mmr": false }),
        );
        extract_path_heading_order(&resp)
    };
    let has = |marker: &str, path: &str| hits(marker).iter().any(|(p, _)| p.ends_with(path));

    let tmp = layout.kb().join("tmp");
    std::fs::create_dir(&tmp).expect("create tmp/");
    layout.write("tmp/seed.md", &body(SEED_MARKER));

    // Waited for on the index state, not slept through (the AV-32 shape
    // above). The debouncer decides per event whether
    // `now - event.time >= timeout` before emitting it (notify-debouncer-full
    // 0.6.0 `lib.rs:229`), so on the tick the later Create of `tmp/seed.md`
    // has expired, the older Create of `tmp/` has expired too: once the seed is
    // searchable under `tmp/`, the event that indexed it is out of the
    // debouncer and so is the Create of `tmp/`, even when mkdir and the write
    // fell into separate windows. A fixed sleep could leave that Create still
    // queued on a slow host; the rename would then come out as a rewritten
    // Create instead of a paired rename. This is also the sanity check that the
    // seed was indexed before the rename.
    let deadline = Duration::from_millis(8000);
    let poll_interval = Duration::from_millis(250);
    let start = Instant::now();
    while !has(SEED_MARKER, "tmp/seed.md") {
        assert!(
            start.elapsed() < deadline,
            "tmp/seed.md never became searchable within {deadline:?}; stderr so far: {:?}",
            guard.stderr().lines()
        );
        sleep(poll_interval);
    }

    // Back to back: the second file and the rename land in one window.
    layout.write("tmp/a.md", &body(A_MARKER));
    rename_retrying(&tmp, &layout.kb().join("final"));

    // Same budget as the tests above.
    let deadline = Duration::from_millis(8000);
    let poll_interval = Duration::from_millis(250);
    let start = Instant::now();
    loop {
        let new_seed = has(SEED_MARKER, "final/seed.md");
        let new_a = has(A_MARKER, "final/a.md");
        let old_seed = has(SEED_MARKER, "tmp/seed.md");
        if new_seed && new_a && !old_seed {
            break;
        }
        assert!(
            start.elapsed() < deadline,
            "files in the renamed `tmp/` were not indexed under `final/` within {deadline:?} (final/seed.md: {new_seed}, final/a.md: {new_a}, tmp/seed.md: {old_seed}); stderr: {:?}",
            guard.stderr().lines()
        );
        sleep(poll_interval);
    }
}

/// (feature-68, local Codex r1) A row the per-file pass does not move is
/// still dropped from the old path when the source directory is recreated
/// before the rename is handled.
///
/// Right after `docs/` is renamed to `notes/`, `docs/` is created again and
/// `docs/a.md` is made a hard link of `notes/a.md` (no privilege needed on
/// Windows). By the time the paired rename is handled, both ends of `a.md` are
/// multiply linked, so the watcher's gate refuses both and the file rename of
/// that row is a skip; `b.md` is moved as usual. The old directory exists
/// again, so the sweep of the old prefix does not run. Without dropping every
/// listed old row after the per-file pass, the `docs/a.md` row survives as a
/// hit for a file the index would refuse. The recreated `docs/a.md` produces
/// its own Create, which the same gate refuses, so nothing puts the row back.
///
/// Red without that step on Windows (the rename is paired as in the moves test
/// above). The paired path is asserted through its stderr line, as there.
///
/// Not run on macOS: FSEvents does not say which end of a rename a path is,
/// and the debouncer guesses from whether the path exists. With `docs/`
/// created again before the event is delivered, both ends exist, the rename
/// is never paired, and the old rows stay (nightly run 39580455945, after the
/// fix for kuriya issue #520). Same family as the unpaired macOS renames of
/// the private trap #280; tracked in the private issue tracker as #525.
#[cfg(not(target_os = "macos"))]
#[test]
#[ignore = "spawns groove serve with watcher; needs inotify (Linux primary; opt-in on macOS/Windows)"]
fn watcher_drops_the_old_rows_when_the_source_is_recreated_during_a_rename() {
    const A_MARKER: &str = "zqxwdirrecreateamarker68";
    const B_MARKER: &str = "zqxwdirrecreatebmarker68";

    // Long enough to clear the default per-chunk quality filter, as above.
    let body = |marker: &str| {
        format!(
            concat!(
                "---\ntitle: Watcher dir recreate fixture\ntags: [test]\n---\n",
                "\n",
                "## marker section\n",
                "\n",
                "Distinct content carrying the marker `{}` so the search assertions\n",
                "can tell under which path this file is in the index. Written long\n",
                "enough to clear the default per-chunk quality filter, which drops\n",
                "short single-line sections from search results regardless of\n",
                "whether they reached the index.\n",
            ),
            marker,
        )
    };

    let layout = TempKbLayout::new("groove-watcher-dirrecreate-e2e");
    layout.write("docs/a.md", &body(A_MARKER));
    layout.write("docs/b.md", &body(B_MARKER));
    build_index(layout.kb());

    let cfg_path = layout.root().join("groove.toml");
    std::fs::write(&cfg_path, "[watch]\nenabled = true\ndebounce_ms = 500\n")
        .expect("write groove.toml");

    let (guard, base) = spawn_mcp_server_with_watch(layout.kb(), &cfg_path);
    let session = mcp_initialize(&base);

    let hits = |marker: &str| {
        let resp = mcp_search_call(
            &base,
            &session,
            serde_json::json!({ "query": marker, "limit": 5, "mmr": false }),
        );
        extract_path_heading_order(&resp)
    };
    let has = |marker: &str, path: &str| hits(marker).iter().any(|(p, _)| p.ends_with(path));

    // Sanity: the fixture is searchable under the old paths before the rename.
    assert!(
        has(A_MARKER, "docs/a.md"),
        "docs/a.md not indexed to begin with"
    );
    assert!(
        has(B_MARKER, "docs/b.md"),
        "docs/b.md not indexed to begin with"
    );

    // Back to back, inside one debounce window: the rename, then the source
    // directory again with a hard link in it.
    let docs = layout.kb().join("docs");
    let notes = layout.kb().join("notes");
    rename_retrying(&docs, &notes);
    std::fs::create_dir(&docs).expect("recreate docs/");
    std::fs::hard_link(notes.join("a.md"), docs.join("a.md")).expect("hard link docs/a.md");

    // Same budget as the tests above.
    let deadline = Duration::from_millis(8000);
    let poll_interval = Duration::from_millis(250);
    let start = Instant::now();
    loop {
        let new_b = has(B_MARKER, "notes/b.md");
        let old_a = has(A_MARKER, "docs/a.md");
        let old_b = has(B_MARKER, "docs/b.md");
        if new_b && !old_a && !old_b {
            break;
        }
        assert!(
            start.elapsed() < deadline,
            "after renaming `docs/` and recreating it, the old rows were not all gone within {deadline:?} (notes/b.md: {new_b}, docs/a.md: {old_a}, docs/b.md: {old_b}); stderr: {:?}",
            guard.stderr().lines()
        );
        sleep(poll_interval);
    }
    assert!(
        guard
            .stderr()
            .contains("watcher: a renamed directory held 2 indexed documents"),
        "the end state was reached without the paired directory-rename path; stderr: {:?}",
        guard.stderr().lines()
    );
}

/// (feature-68, re-review) A directory renamed away and back inside one
/// debounce window stays indexed where it is, and the paired directory-rename
/// path is not taken.
///
/// The debouncer can fold the two renames into one paired event whose two
/// ends are the same path. Taken as a directory rename, that event would
/// "rename" each row onto itself, drop every listed row and re-embed the
/// whole directory on the scan. Its two ends being the same, the watcher
/// leaves it to the file-rename path, which skips two directory names.
///
/// What is checked is that nothing happens, so the test waits on a sentinel
/// rather than on the change itself: a file written right after the second
/// rename. The debouncer emits events in time order and the handler takes
/// the batches in turn, so once the sentinel is searchable the renames have
/// been handled. A fixed sleep could end before the handler ran on a slow
/// host, and the test would be green without the guard.
#[test]
#[ignore = "spawns groove serve with watcher; needs inotify (Linux primary; opt-in on macOS/Windows)"]
fn watcher_leaves_a_directory_alone_when_a_rename_returns_within_the_window() {
    const A_MARKER: &str = "zqxwdirreturnamarker68";
    const SENTINEL_MARKER: &str = "zqxwdirreturnsentinelmarker68";

    // Long enough to clear the default per-chunk quality filter, as above.
    let body = |marker: &str| {
        format!(
            concat!(
                "---\ntitle: Watcher dir return fixture\ntags: [test]\n---\n",
                "\n",
                "## marker section\n",
                "\n",
                "Distinct content carrying the marker `{}` so the search assertions\n",
                "can tell under which path this file is in the index. Written long\n",
                "enough to clear the default per-chunk quality filter, which drops\n",
                "short single-line sections from search results regardless of\n",
                "whether they reached the index.\n",
            ),
            marker,
        )
    };

    let layout = TempKbLayout::new("groove-watcher-dirreturn-e2e");
    layout.write("docs/a.md", &body(A_MARKER));
    build_index(layout.kb());

    let cfg_path = layout.root().join("groove.toml");
    std::fs::write(&cfg_path, "[watch]\nenabled = true\ndebounce_ms = 500\n")
        .expect("write groove.toml");

    let (guard, base) = spawn_mcp_server_with_watch(layout.kb(), &cfg_path);
    let session = mcp_initialize(&base);

    let hits = |marker: &str| {
        let resp = mcp_search_call(
            &base,
            &session,
            serde_json::json!({ "query": marker, "limit": 5, "mmr": false }),
        );
        extract_path_heading_order(&resp)
    };
    let has = |marker: &str, path: &str| hits(marker).iter().any(|(p, _)| p.ends_with(path));

    assert!(
        has(A_MARKER, "docs/a.md"),
        "docs/a.md not indexed to begin with"
    );

    // Back to back, inside one debounce window: away and back.
    let docs = layout.kb().join("docs");
    let notes = layout.kb().join("notes");
    rename_retrying(&docs, &notes);
    rename_retrying(&notes, &docs);
    layout.write("sentinel.md", &body(SENTINEL_MARKER));

    // Same budget as the tests above.
    let deadline = Duration::from_millis(8000);
    let poll_interval = Duration::from_millis(250);
    let start = Instant::now();
    while !has(SENTINEL_MARKER, "sentinel.md") {
        assert!(
            start.elapsed() < deadline,
            "sentinel.md never became searchable within {deadline:?}; stderr so far: {:?}",
            guard.stderr().lines()
        );
        sleep(poll_interval);
    }

    assert!(
        has(A_MARKER, "docs/a.md"),
        "docs/a.md is no longer searchable after renaming `docs/` away and back; stderr: {:?}",
        guard.stderr().lines()
    );
    let stderr = guard.stderr();
    assert!(
        !stderr.contains("renamed docs/a.md -> docs/a.md"),
        "a row was renamed onto itself; stderr: {:?}",
        stderr.lines()
    );
    assert!(
        !stderr.contains("a renamed directory held"),
        "the paired directory-rename path was taken for a rename that returned; stderr: {:?}",
        stderr.lines()
    );
}
