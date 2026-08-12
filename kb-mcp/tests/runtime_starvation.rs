//! (BU-06) The production router must keep serving cheap endpoints while a
//! tool body is busy.
//!
//! `server.rs` already pins the two halves of this in unit tests that CI runs:
//! `run_blocking_leaves_the_async_workers_free` proves the offload mechanism
//! and `tool_handlers_do_not_block_the_runtime` proves every `#[tool]` handler
//! uses it. What neither can reach is the wiring — that a request arriving at
//! the real axum router really does travel through those handlers. That is
//! what this file covers, at the cost of an embedder model, hence `#[ignore]`.
//!
//! Run with `cargo test --features test-helpers --test runtime_starvation --
//! --ignored`.

// Declared at the file top level so rustfmt can resolve the `#[path]` relative
// to `tests/` (see the same note in `webui_integration.rs`).
#[cfg(feature = "test-helpers")]
#[path = "common/mod.rs"]
mod common;

#[cfg(feature = "test-helpers")]
mod with_helpers {
    use super::common;

    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use tower::ServiceExt;

    use kb_mcp::server::KbServerShared;
    use kb_mcp::transport::http::build_router_for_test;

    /// How long the foreign thread parks the embedder mutex. Long enough that
    /// "the worker was free" and "the worker was blocked" cannot be confused
    /// for one another on a loaded CI box.
    const STALL: Duration = Duration::from_millis(1500);
    /// How long the main thread waits before measuring, to be sure the worker
    /// has actually entered the search handler.
    const SETTLE: Duration = Duration::from_millis(300);

    fn build_shared(prefix: &str) -> Arc<KbServerShared> {
        use common::temp::TempRoot;
        let tmp = TempRoot::new(prefix);
        let kb = tmp.path().to_path_buf();
        std::fs::write(kb.join("a.md"), "# A\ntokio worker threads").unwrap();
        let db_path = tmp.path().join(".kb-mcp.db");
        let db = kb_mcp::db::Database::open(&db_path.to_string_lossy()).unwrap();
        let embedder =
            kb_mcp::embedder::Embedder::with_model(kb_mcp::embedder::ModelChoice::default())
                .expect("embedder init (BGE-small may need model DL on first run)");
        // The db handle holds a file lock for as long as the shared state
        // lives, so let the OS reap the temp tree instead of the Drop guard.
        std::mem::forget(tmp);
        Arc::new(KbServerShared::for_test(db, embedder, kb))
    }

    /// A search stuck behind the embedder mutex must not take `/healthz` down
    /// with it.
    ///
    /// The runtime gets exactly **one** worker thread, so this is a statement
    /// about scheduling rather than about timing: whoever owns that worker owns
    /// the entire server. Before BU-06 the search body ran on it directly and
    /// `/healthz` could not be polled until the stall was over.
    #[test]
    #[ignore = "requires embedder model download (BGE-small ~130 MB)"]
    fn healthz_answers_while_a_search_is_stuck_on_the_embedder_lock() {
        let shared = build_shared("bu06-starvation");
        // `build_router_for_test` carries the admin routes only; production
        // composes them with `/healthz`, whose handler is this exact body.
        let app = axum::Router::new()
            .route("/healthz", axum::routing::get(|| async { "ok" }))
            .merge(build_router_for_test(Arc::clone(&shared)));

        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();

        // Park the embedder mutex from a thread the runtime knows nothing
        // about. `search` takes this lock first, so its body has something
        // real — and deterministically timed — to block on.
        let embedder = Arc::clone(&shared.embedder);
        let (locked_tx, locked_rx) = std::sync::mpsc::channel();
        let holder = std::thread::spawn(move || {
            let _guard = embedder.lock().unwrap();
            locked_tx.send(()).unwrap();
            std::thread::sleep(STALL);
        });
        locked_rx.recv().expect("lock holder thread died");

        let started = Instant::now();
        let search_app = app.clone();
        let search = rt.spawn(async move {
            let resp = search_app
                .oneshot(
                    axum::http::Request::builder()
                        .method("POST")
                        .uri("/api/search")
                        .header("Host", "127.0.0.1")
                        .header("content-type", "application/json")
                        .body(axum::body::Body::from(
                            r#"{"query":"tokio worker threads","limit":5}"#,
                        ))
                        .unwrap(),
                )
                .await
                .unwrap();
            (resp.status(), started.elapsed())
        });

        // Sleep on *this* thread — it is not a runtime worker, so the search
        // task is guaranteed to have been picked up by the time we measure.
        std::thread::sleep(SETTLE);

        // Start the clock *before* spawning. Taking it inside the task would
        // measure only how long `/healthz` took once it was already running
        // and silently exclude the queueing delay — which is the entire
        // quantity under test.
        let hz_started = Instant::now();
        let healthz_elapsed = rt.block_on(async {
            let hz_app = app.clone();
            rt.spawn(async move {
                let resp = hz_app
                    .oneshot(
                        axum::http::Request::builder()
                            .uri("/healthz")
                            .body(axum::body::Body::empty())
                            .unwrap(),
                    )
                    .await
                    .unwrap();
                assert_eq!(resp.status(), 200, "/healthz did not answer 200");
            })
            .await
            .expect("healthz task panicked");
            hz_started.elapsed()
        });

        let (search_status, search_elapsed) = rt.block_on(search).expect("search task panicked");
        holder.join().unwrap();

        // The stall has to have been real, or the assertion below proves
        // nothing: a search that returned before the lock was released never
        // blocked on anything.
        assert!(
            search_elapsed >= STALL - SETTLE,
            "the search did not actually block on the embedder mutex — it \
             returned after {search_elapsed:?} (status {search_status}) while \
             the lock was held for {STALL:?}. The /healthz assertion below \
             would pass for the wrong reason."
        );
        assert!(
            healthz_elapsed < Duration::from_millis(400),
            "/healthz waited {healthz_elapsed:?} behind a search that was \
             blocked for {search_elapsed:?}. Tool bodies are running on the \
             async workers again and starving the runtime (BU-06)."
        );
    }
}
