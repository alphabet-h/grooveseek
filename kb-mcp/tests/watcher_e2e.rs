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

    // Give the watcher's bridge thread + tokio recv setup ample time
    // to settle. Empirically the debouncer is ready well within ~500ms
    // after `wait_http_200` returns, but we have no deterministic sync
    // signal exposed today (a stderr-scan for "watcher started" is a
    // future refinement).
    sleep(Duration::from_millis(2000));

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
