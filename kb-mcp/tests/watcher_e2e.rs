//! F-57: Watcher real-disk end-to-end smoke.
//!
//! Spawns `kb-mcp serve --transport http` *with the watcher enabled*
//! (= no `--no-watch`), creates a brand-new `.md` file in the watched
//! KB directory, waits for the debouncer + reindex tick, and verifies
//! that an MCP `search` call now finds the new content.
//!
//! Exercises the full real-disk pipeline:
//!   notify-debouncer-full -> bridge thread (mpsc) ->
//!   tokio task (run_watch_loop) -> classify -> indexer::reindex_single_file.
//!
//! `#[ignore]` because the test:
//! - spawns a kb-mcp subprocess (~5-7 sec wall clock)
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
#[ignore = "spawns kb-mcp serve with watcher; needs inotify (Linux primary; opt-in on macOS/Windows)"]
fn test_watcher_picks_up_new_file() {
    let layout = TempKbLayout::new("kb-mcp-watcher-e2e");
    setup_initial_kb(&layout);
    build_index(layout.kb());

    // Minimal config + watch enabled (= debounce 500ms default, but
    // pin it explicitly so this test does not depend on future default
    // changes).
    let cfg_path = layout.root().join("kb-mcp.toml");
    std::fs::write(&cfg_path, "[watch]\nenabled = true\ndebounce_ms = 500\n")
        .expect("write kb-mcp.toml");

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

/// (feature-49) A `.kb-mcpignore` written while the server is running takes
/// effect for subsequent events.
///
/// The reload is reachable only because `handle_events` looks for the file
/// **before** classifying anything: `.kb-mcpignore` has no registered
/// extension, so the ordinary path would drop the event on the same filter the
/// file is meant to change, and the watcher would go on indexing what the next
/// `kb-mcp index` will drop.
///
/// Two files are written afterwards, not one. "The ignored file never appears"
/// passes just as well when the watcher is dead, when the debouncer never
/// armed, or when the deadline was too short — so a second, *not* ignored file
/// has to appear in the same window for the negative half to mean anything.
#[test]
#[ignore = "spawns kb-mcp serve with watcher; needs inotify (Linux primary; opt-in on macOS/Windows)"]
fn watcher_reloads_kb_mcpignore_while_running() {
    const IGNORED_MARKER: &str = "zqxwignoredmarker49";
    const VISIBLE_MARKER: &str = "zqxwvisiblemarker49";

    let layout = TempKbLayout::new("kb-mcp-watcher-ignore-e2e");
    setup_initial_kb(&layout);
    build_index(layout.kb());

    let cfg_path = layout.root().join("kb-mcp.toml");
    std::fs::write(&cfg_path, "[watch]\nenabled = true\ndebounce_ms = 500\n")
        .expect("write kb-mcp.toml");

    let (_guard, base) = spawn_mcp_server_with_watch(layout.kb(), &cfg_path);
    let session = mcp_initialize(&base);

    // The server started without one, so this write is the reload.
    layout.write(".kb-mcpignore", "secret/\n");

    // Let that land in its own debounce window. Sharing one with the writes
    // below would still work — the reload runs before the batch is classified —
    // but a note landing in an *earlier* batch than the ignore file would be
    // indexed, and the failure would look like a broken reload rather than a
    // racy fixture.
    sleep(Duration::from_millis(2000));

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
             .kb-mcpignore excludes: {ignored:?}"
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
