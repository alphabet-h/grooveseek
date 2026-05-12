//! Integration tests for `/api/admin/status` + admin Host check + WebUI MVP
//! (= feature-43 PR-2, Task 18 + Task 19). The router is exercised via
//! `tower::ServiceExt::oneshot` so no real TCP listener is spawned.
//!
//! The admin / WebUI integration tests are `#[ignore]`-marked because
//! `Embedder::with_model` will attempt to download BGE-small (~130 MB) on
//! first run. The middleware / handler logic is covered statically by
//! `cargo check --tests`; the ignored tests are CI / manual
//! `cargo test -- --ignored` coverage.
//!
//! The plain regression test `webui_index_html_does_not_use_innerhtml`
//! lives outside the `with_helpers` module so it runs in default
//! `cargo test` without the `test-helpers` feature flag.

/// Regression: webui_index.html must not contain `innerHTML` (XSS vector).
/// Plain compile-time string scan — no `test-helpers` feature required.
#[test]
fn webui_index_html_does_not_use_innerhtml() {
    let html = include_str!("../src/transport/webui_index.html");
    assert!(
        !html.contains(".innerHTML"),
        "innerHTML found in webui_index.html — XSS regression risk. \
         Use textContent + createElement + appendChild instead."
    );
}

#[cfg(feature = "test-helpers")]
mod with_helpers {
    #[path = "../common/mod.rs"]
    mod common;

    use std::sync::Arc;

    use tower::ServiceExt;

    use kb_mcp::server::KbServerShared;
    use kb_mcp::transport::http::build_router_for_test;

    /// Build a `KbServerShared` wrapped in `Arc` with two tiny md files indexed
    /// only by virtue of being on disk (no actual indexing run; the admin status
    /// endpoint reports raw db counts which start at zero — that's fine, we only
    /// assert structure of the JSON response, not the count values).
    fn build_test_shared(prefix: &str) -> Arc<KbServerShared> {
        use common::temp::TempRoot;
        let tmp = TempRoot::new(prefix);
        let kb = tmp.path().to_path_buf();
        // Drop two small md files so the kb dir is non-empty (purely cosmetic;
        // the endpoint reports SQLite counts which are 0 until index runs).
        std::fs::write(kb.join("a.md"), "# A\nfoo bar baz").unwrap();
        std::fs::write(kb.join("b.md"), "# B\nlorem ipsum").unwrap();
        let db_path = tmp.path().join(".kb-mcp.db");
        let db = kb_mcp::db::Database::open(&db_path.to_string_lossy()).unwrap();
        let embedder =
            kb_mcp::embedder::Embedder::with_model(kb_mcp::embedder::ModelChoice::default())
                .expect("embedder init (BGE-small may need model DL on first run)");
        // Leak the temp dir so it outlives the test scope (the OS will reap the
        // temp directory tree on shutdown; the db handle keeps a file lock on
        // the .kb-mcp.db until the KbServerShared is dropped).
        std::mem::forget(tmp);
        Arc::new(KbServerShared::for_test(db, embedder, kb))
    }

    #[tokio::test]
    #[ignore = "requires embedder model download (BGE-small ~130 MB)"]
    async fn api_admin_status_returns_state() {
        let shared = build_test_shared("admin_status");
        let app = build_router_for_test(shared);
        let resp = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/api/admin/status")
                    .header("Host", "127.0.0.1")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(
            json.get("daemon").and_then(|d| d.get("version")).is_some(),
            "daemon.version missing"
        );
        assert!(json.get("indexing").is_some());
        assert!(json.get("watcher").is_some());
        assert!(json.get("kb").is_some());
        assert!(json.get("config_source").is_some());
    }

    #[tokio::test]
    #[ignore = "requires embedder model download (BGE-small ~130 MB)"]
    async fn api_admin_status_rejects_non_loopback_host() {
        let shared = build_test_shared("admin_non_loopback");
        let app = build_router_for_test(shared);
        let resp = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/api/admin/status")
                    .header("Host", "192.168.0.42:3100")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), 403);
    }
}
