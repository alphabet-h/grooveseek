//! The writer role a `groove serve` takes at start (ADR-0031): writer, reader
//! or no watch, the janitor that keeps the holder's heartbeat and settles
//! panicked claims, and the watcher a writer runs.

// `run_server` calls these once the start is wired to them; until then only
// the tests below do.
#![cfg_attr(not(test), allow(dead_code))]

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::task::JoinHandle;

use super::KbServerShared;
use super::writer_slot::{ClaimGuard, Claimant, ReaderReason, WriterSlot};
use crate::db::{ContextMode, Database};
use crate::poison::recover_db;
use crate::watcher::{WatcherState, run_watch_loop_until};
use crate::writer_lease::{HolderInfo, LeaseError, WriterLease, holder_relation};

/// How often the janitor rewrites the heartbeat and looks for panicked claims.
pub(crate) const JANITOR_TICK: Duration = Duration::from_secs(5);

/// What the server's first claim made it.
#[derive(Debug)]
pub(crate) enum StartRole {
    /// It holds the lease through the watcher's claim, connection writable.
    Writer(ClaimGuard),
    /// Another process held the lease, or it could not be taken: searches only.
    Reader,
    /// `--no-watch`: no lease at start; a `rebuild_index` takes it.
    NoWatch,
}

/// The tasks the writer role runs. The janitor starts right after the
/// claim, before the models download; the watcher starts after.
#[derive(Default)]
pub(crate) struct WriterRoleHandles {
    pub janitor: Option<JoinHandle<()>>,
    pub watcher: Option<JoinHandle<()>>,
    /// Turned `true` to stop the watcher at its next batch boundary.
    pub watcher_stop: Option<tokio::sync::watch::Sender<bool>>,
}

/// Decide at start whether this server writes the index (ADR-0031). With
/// the watch on, the watcher's claim takes the lease, the connection is
/// made writable and the context mode resolved under the db guard. A held
/// or failing lease leaves a reader, warned about once on stderr.
pub(crate) fn claim_writer_role(
    writer: &Arc<WriterSlot>,
    db: &Mutex<Database>,
    db_path: &Path,
    kb_path: &Path,
    watch_enabled: bool,
    context_mode_desired: ContextMode,
) -> anyhow::Result<StartRole> {
    if !watch_enabled {
        return Ok(StartRole::NoWatch);
    }
    match writer.claim(Claimant::Watcher) {
        Ok(mut guard) => {
            let db = recover_db(db.lock());
            // An Err here has already released the claim.
            writer.make_writable(&mut guard, &db)?;
            if let Err(e) = crate::indexer::resolve_context_mode(&db, context_mode_desired, false) {
                writer.release(guard, &db);
                return Err(e);
            }
            drop(db);
            Ok(StartRole::Writer(guard))
        }
        Err(LeaseError::Busy { holder, .. }) => {
            let warning = busy_warning(db_path, kb_path, holder.as_ref());
            writer.become_reader(ReaderReason::Busy {
                relation: holder_relation(holder.as_ref(), kb_path),
                holder_at_start: holder,
            });
            eprintln!("{warning}");
            Ok(StartRole::Reader)
        }
        Err(LeaseError::Io { lock_path, source }) => {
            let error = crate::embedder::ascii_io_error(&source);
            Ok(io_reader(writer, kb_path, lock_path, error))
        }
        Err(e @ LeaseError::NotUtf8 { .. }) => {
            let error = e.to_string().trim_end_matches('.').to_string();
            Ok(io_reader(
                writer,
                kb_path,
                WriterLease::lock_path_for(db_path),
                error,
            ))
        }
        Err(e) => Err(anyhow::anyhow!(
            "could not claim the writer lease at start: {e}"
        )),
    }
}

/// The start of a server another groove process keeps out of the index.
fn busy_warning(db_path: &Path, kb_path: &Path, holder: Option<&HolderInfo>) -> String {
    let info = WriterLease::info_path_for(db_path);
    let who = match holder {
        Some(h) => format!(
            "pid {} per {}, {}, kb {}",
            h.pid,
            info.display(),
            h.role.as_str(),
            h.kb_path
        ),
        None => format!("pid unknown (no {})", info.display()),
    };
    format!(
        "warning: another groove process ({who}) is writing the index at {}; this server (kb {}) answers searches but does not update the index, and it keeps not updating it after that process exits, until this server is restarted.",
        db_path.display(),
        kb_path.display()
    )
}

/// Become a reader over a lock that could not be taken, and warn once.
fn io_reader(
    writer: &WriterSlot,
    kb_path: &Path,
    lock_path: std::path::PathBuf,
    error: String,
) -> StartRole {
    let warning = format!(
        "warning: could not take the writer lock at {} ({error}); this server (kb {}) answers searches but does not update the index until the lock works and it is restarted.",
        lock_path.display(),
        kb_path.display()
    );
    writer.become_reader(ReaderReason::Io { lock_path, error });
    eprintln!("{warning}");
    StartRole::Reader
}

/// Start the janitor: on every [`JANITOR_TICK`] it rewrites the heartbeat
/// first, then starts a settle of panicked claims without awaiting it, so
/// a settle waiting for the db never holds the heartbeat back. A settle is
/// not started while the previous one still runs. Started for every role,
/// right after [`claim_writer_role`].
pub(crate) fn spawn_janitor(writer: Arc<WriterSlot>, db: Arc<Mutex<Database>>) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(JANITOR_TICK);
        let mut settling: Option<JoinHandle<()>> = None;
        loop {
            tick.tick().await;
            let beat = Arc::clone(&writer);
            // The heartbeat takes the slot's inner mutex only, never the db.
            let _ = tokio::task::spawn_blocking(move || beat.heartbeat()).await;
            if writer.has_pending() && settling.as_ref().is_none_or(JoinHandle::is_finished) {
                let writer = Arc::clone(&writer);
                let db = Arc::clone(&db);
                settling = Some(tokio::task::spawn_blocking(move || {
                    let db = recover_db(db.lock());
                    for who in writer.settle_pending(&db) {
                        tracing::warn!("writer lease released after a panic in {}", who.as_str());
                    }
                }));
            }
        }
    })
}

/// Start what the role runs after the models are loaded: a writer's
/// watcher, whose claim is released under the db guard when its loop ends.
/// A reader and a `--no-watch` server start nothing here.
pub(crate) fn start_writer_role(
    shared: &KbServerShared,
    role: StartRole,
    watcher_state: impl FnOnce() -> WatcherState + Send + 'static,
    handles: &mut WriterRoleHandles,
) {
    let StartRole::Writer(guard) = role else {
        return;
    };
    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    let state = watcher_state();
    let writer = Arc::clone(&shared.writer);
    let db = Arc::clone(&shared.db);
    handles.watcher = Some(tokio::spawn(async move {
        if let Err(e) = run_watch_loop_until(state, stop_rx).await {
            eprintln!("watcher exited with error: {e}");
        }
        let _ = tokio::task::spawn_blocking(move || {
            let db = recover_db(db.lock());
            writer.release(guard, &db);
        })
        .await;
    }));
    handles.watcher_stop = Some(stop_tx);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::{KbServer, KbServerShared, RebuildIndexParams, SearchParams};
    use crate::watcher::{WatchBatchProbe, WatchConfig};
    use crate::writer_lease::{LeaseError, Role, WriterLease};
    use rmcp::handler::server::wrapper::Parameters;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::time::{Duration, Instant, SystemTime};

    const MODEL: &str = "test-model";
    const DIM: u32 = 4;

    /// How the lease is taken, injected into a managed slot.
    type AcquireForTest =
        Box<dyn Fn(&Path, Role, &Path) -> Result<WriterLease, LeaseError> + Send + Sync>;

    /// A knowledge base (`kb/note.md`) and its index (meta `model`, 4) in a
    /// scratch directory removed on drop (no `tempfile`).
    struct TestKb {
        dir: PathBuf,
        kb: PathBuf,
        db_path: PathBuf,
    }

    impl TestKb {
        fn new(prefix: &str) -> Self {
            Self::with_model(prefix, MODEL)
        }

        fn with_model(prefix: &str, model: &str) -> Self {
            let dir = crate::test_support::unique_temp_path(&format!("groove-role-{prefix}"));
            let kb = dir.join("kb");
            std::fs::create_dir_all(&kb).unwrap();
            std::fs::write(kb.join("note.md"), "# Note\n\nSome text to index.\n").unwrap();
            let db_path = dir.join(".groove.db");
            {
                let w = Database::open(db_path.to_str().unwrap()).unwrap();
                w.verify_embedding_meta(model, DIM).unwrap();
            }
            Self { dir, kb, db_path }
        }

        /// Index `kb/` once, as an earlier `groove index` would have.
        fn index_once(&self) {
            let db = Database::open(self.db_path.to_str().unwrap()).unwrap();
            let mut embedder = crate::embedder::Embedder::stub_for_tests(DIM as usize);
            crate::indexer::rebuild_index(
                &db,
                &mut embedder,
                &self.kb,
                None,
                false,
                None,
                &[],
                &crate::parser::Registry::defaults(),
                crate::indexer::progress::ProgressReporter::new(
                    crate::indexer::progress::ProgressMode::Quiet,
                ),
                ContextMode::Off,
            )
            .unwrap();
        }

        /// A sibling of `kb/` that exists.
        fn other_kb(&self) -> PathBuf {
            let other = self.dir.join("other");
            std::fs::create_dir_all(&other).unwrap();
            other
        }

        /// Another handle's lease on this index, held until dropped.
        fn hold_lease(&self, role: Role, kb: &Path) -> WriterLease {
            WriterLease::try_acquire(&self.db_path, role, kb)
                .unwrap_or_else(|e| panic!("another handle must take the lease: {e}"))
        }

        /// Whether another handle can take the lease now (dropped at once).
        fn lease_is_free(&self) -> bool {
            match WriterLease::try_acquire(&self.db_path, Role::Index, &self.kb) {
                Ok(_) => true,
                Err(LeaseError::Busy { .. }) => false,
                Err(e) => panic!("unexpected lease error: {e}"),
            }
        }

        fn holder(&self) -> Option<crate::writer_lease::HolderInfo> {
            WriterLease::read_holder(&self.db_path)
        }

        /// Rewrite writer.json with a heartbeat 60 s old.
        fn age_the_heartbeat(&self) {
            let mut h = self.holder().expect("writer.json must be there");
            let old = chrono::Utc::now() - chrono::Duration::seconds(60);
            h.heartbeat_at = Some(old.to_rfc3339());
            std::fs::write(
                WriterLease::info_path_for(&self.db_path),
                serde_json::to_vec_pretty(&h).unwrap(),
            )
            .unwrap();
        }

        fn heartbeat_is_fresh(&self) -> bool {
            self.holder()
                .is_some_and(|h| h.heartbeat_fresh(SystemTime::now()))
        }

        /// Every row of `documents`, `chunks` and `index_meta`, read through
        /// a connection of its own.
        fn rows(&self) -> Vec<(String, Vec<String>)> {
            let conn = rusqlite::Connection::open_with_flags(
                &self.db_path,
                rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
            )
            .unwrap();
            let mut out = Vec::new();
            for table in ["documents", "chunks", "index_meta"] {
                let mut stmt = conn
                    .prepare(&format!("SELECT * FROM {table} ORDER BY rowid"))
                    .unwrap();
                let n = stmt.column_count();
                let rows = stmt
                    .query_map([], |r| {
                        (0..n)
                            .map(|i| {
                                r.get::<_, rusqlite::types::Value>(i)
                                    .map(|v| format!("{v:?}"))
                            })
                            .collect::<Result<Vec<_>, _>>()
                    })
                    .unwrap();
                for row in rows {
                    out.push((table.to_string(), row.unwrap()));
                }
            }
            out
        }
    }

    impl Drop for TestKb {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    /// The process holding the lease when the server starts.
    enum Holder {
        /// A server on the same knowledge base, reporting a running watcher.
        Serve,
        /// A one-off `groove index` on the same knowledge base.
        Index,
        /// A server on a sibling knowledge base sharing the index.
        OtherKbServe,
        /// A server on the same knowledge base whose writer.json is gone.
        ServeWithoutInfo,
    }

    /// What [`start_writer_for_test`] varies.
    #[derive(Default)]
    struct StartOpts {
        /// Start as `--no-watch`.
        no_watch: bool,
        holder: Option<Holder>,
        acquire: Option<AcquireForTest>,
        /// Claim and start the janitor only, as during the model download.
        skip_watcher: bool,
        kb_path: Option<PathBuf>,
        watch_backoff: Option<(Duration, Duration)>,
        batch_probe: Option<Arc<WatchBatchProbe>>,
        debounce_ms: Option<u64>,
    }

    struct TestServer {
        kb: TestKb,
        shared: KbServerShared,
        writer: Arc<WriterSlot>,
        holder: Option<WriterLease>,
        /// The role of a start that skipped the watcher, kept so its claim
        /// is not dropped (a dropped claim reads as a panic).
        unstarted: Option<StartRole>,
    }

    impl TestServer {
        async fn search(&self) -> serde_json::Value {
            let out = KbServer::from_shared(&self.shared)
                .search(Parameters(SearchParams {
                    query: "note".into(),
                    ..Default::default()
                }))
                .await;
            serde_json::from_str(&out).unwrap_or_else(|e| panic!("not JSON ({e}): {out}"))
        }

        async fn rebuild(&self) -> serde_json::Value {
            let out = KbServer::from_shared(&self.shared)
                .rebuild_index(Parameters(RebuildIndexParams::default()))
                .await;
            serde_json::from_str(&out).unwrap_or_else(|e| panic!("not JSON ({e}): {out}"))
        }

        fn watcher_active(&self) -> bool {
            self.shared
                .watcher_active
                .load(std::sync::atomic::Ordering::Relaxed)
        }

        fn query_only(&self) -> i64 {
            recover_db(self.shared.db.lock()).query_only_for_test()
        }
    }

    /// The slot and shared state of a server on `kb`, with the holder of
    /// `opts` in place; nothing claimed yet.
    fn serve_for_test(kb: TestKb, opts: &mut StartOpts) -> TestServer {
        let holder = opts.holder.take().map(|h| match h {
            Holder::Serve => {
                let mut l = kb.hold_lease(Role::Serve, &kb.kb);
                l.touch_heartbeat(Some(true));
                l
            }
            Holder::Index => kb.hold_lease(Role::Index, &kb.kb),
            Holder::OtherKbServe => {
                let other = kb.other_kb();
                let mut l = kb.hold_lease(Role::Serve, &other);
                l.touch_heartbeat(Some(true));
                l
            }
            Holder::ServeWithoutInfo => {
                let l = kb.hold_lease(Role::Serve, &kb.kb);
                std::fs::remove_file(WriterLease::info_path_for(&kb.db_path)).unwrap();
                l
            }
        });
        let writer = match opts.acquire.take() {
            Some(acquire) => WriterSlot::managed_with_acquire(
                kb.db_path.clone(),
                kb.kb.clone(),
                MODEL.to_string(),
                DIM,
                acquire,
            ),
            None => WriterSlot::managed(kb.db_path.clone(), kb.kb.clone(), MODEL.to_string(), DIM),
        };
        let db = Database::open_read_only(kb.db_path.to_str().unwrap(), &kb.kb).unwrap();
        let shared = KbServerShared::for_test_with_writer(
            db,
            crate::embedder::Embedder::stub_for_tests(DIM as usize),
            kb.kb.clone(),
            Arc::clone(&writer),
        );
        TestServer {
            kb,
            shared,
            writer,
            holder,
            unstarted: None,
        }
    }

    /// The start in the order `groove serve` runs it: claim, then the
    /// janitor, then (after the model download) the watcher.
    fn start_writer_for_test(kb: TestKb, mut opts: StartOpts) -> (TestServer, WriterRoleHandles) {
        let mut ts = serve_for_test(kb, &mut opts);
        let role = claim_writer_role(
            &ts.writer,
            &ts.shared.db,
            &ts.kb.db_path,
            &ts.kb.kb,
            !opts.no_watch,
            ContextMode::Off,
        )
        .expect("the start must not fail");
        let mut handles = WriterRoleHandles {
            janitor: Some(spawn_janitor(
                Arc::clone(&ts.writer),
                Arc::clone(&ts.shared.db),
            )),
            watcher: None,
            watcher_stop: None,
        };
        if opts.skip_watcher {
            ts.unstarted = Some(role);
        } else {
            let shared = &ts.shared;
            let kb_path = opts.kb_path.unwrap_or_else(|| shared.kb_path.clone());
            let db = Arc::clone(&shared.db);
            let embedder = Arc::clone(&shared.embedder);
            let registry = Arc::clone(&shared.parser_registry);
            let exclude_headings = shared.exclude_headings.clone();
            let exclude_dirs = shared.exclude_dirs.clone();
            let watcher_active = Arc::clone(&shared.watcher_active);
            let debounce_ms = opts.debounce_ms.unwrap_or(100);
            let watch_backoff = opts
                .watch_backoff
                .unwrap_or(crate::watcher::WatcherState::DEFAULT_WATCH_BACKOFF);
            let batch_probe = opts.batch_probe;
            start_writer_role(
                shared,
                role,
                move || WatcherState {
                    rules: crate::exclusion::ExclusionRules::load(&kb_path, exclude_dirs),
                    kb_path,
                    db,
                    embedder,
                    registry,
                    exclude_headings,
                    config: WatchConfig {
                        enabled: true,
                        debounce_ms,
                    },
                    watcher_active,
                    watch_status: None,
                    watch_backoff,
                    batch_probe,
                },
                &mut handles,
            );
        }
        (ts, handles)
    }

    /// The end of every test here: no janitor, watcher or lease is left
    /// behind. Stands in for the server's shutdown until it exists.
    async fn finish(ts: &TestServer, handles: WriterRoleHandles) {
        if let Some(j) = handles.janitor {
            j.abort();
            let _ = j.await;
        }
        if let Some(w) = handles.watcher {
            w.abort();
            let _ = w.await;
        }
        drop(handles.watcher_stop);
        let db = recover_db(ts.shared.db.lock());
        ts.writer.shutdown(&db);
    }

    /// Poll `cond` every 250 ms until it holds or `within` passes.
    async fn eventually(within: Duration, mut cond: impl FnMut() -> bool) -> bool {
        let deadline = Instant::now() + within;
        loop {
            if cond() {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }

    /// Drop `guard` inside a caught panic, as a panicking handler would.
    fn panic_with(guard: ClaimGuard) {
        let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            let _held = guard;
            panic!("panicked on purpose while holding a claim");
        }));
        assert!(caught.is_err());
    }

    fn own_pid_per() -> String {
        format!("pid {} per", std::process::id())
    }

    /// T-L9 1: the first server on an index writes it and watches files.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_server_that_takes_the_lease_writes_and_watches() {
        let (ts, handles) = start_writer_for_test(TestKb::new("writer"), StartOpts::default());
        let watching = eventually(Duration::from_secs(10), || ts.watcher_active()).await;
        assert!(watching, "the watcher must start within 10 s");
        let status = ts.writer.status();
        assert!(status.held, "{status:?}");
        assert!(!status.reader, "{status:?}");
        assert_eq!(ts.query_only(), 0);
        let json = ts.search().await;
        assert!(json.get("results").is_some(), "{json}");
        assert!(json.get("writer_notice").is_none(), "{json}");
        finish(&ts, handles).await;
        assert!(ts.kb.lease_is_free());
    }

    /// T-L9 2: a server started while another holds the lease reads only,
    /// and says why on every reply.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_server_started_while_another_holds_the_lease_is_a_reader() {
        let (ts, handles) = start_writer_for_test(
            TestKb::new("reader"),
            StartOpts {
                holder: Some(Holder::Serve),
                ..Default::default()
            },
        );
        tokio::time::sleep(Duration::from_secs(1)).await;
        assert!(!ts.watcher_active());
        assert!(handles.watcher.is_none());
        let status = ts.writer.status();
        assert!(status.reader, "{status:?}");
        assert!(!status.held, "{status:?}");
        assert_eq!(ts.query_only(), 1);

        let search = ts.search().await;
        let rebuild = ts.rebuild().await;
        assert!(rebuild.get("error").is_some(), "{rebuild}");
        let reader_notice = status.reader_notice.clone().expect("a reader notice");
        for notice in [
            search["writer_notice"]
                .as_str()
                .expect("search writer_notice"),
            rebuild["writer_notice"]
                .as_str()
                .expect("rebuild writer_notice"),
            reader_notice.as_str(),
        ] {
            assert!(notice.contains("not updating the index"), "{notice}");
            assert!(notice.contains(&own_pid_per()), "{notice}");
            assert!(notice.contains("keeps the index current"), "{notice}");
            assert!(notice.contains("a process that was killed"), "{notice}");
            assert!(!notice.contains("rebuild_index"), "{notice}");
            assert!(notice.contains("cannot rebuild"), "{notice}");
            assert!(notice.contains("after the restart"), "{notice}");
        }
        finish(&ts, handles).await;
    }

    /// T-L9 3: a reader does not take the lease when the holder leaves.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_reader_stays_a_reader_after_the_holder_leaves() {
        let (mut ts, handles) = start_writer_for_test(
            TestKb::new("stays"),
            StartOpts {
                holder: Some(Holder::Serve),
                ..Default::default()
            },
        );
        assert!(ts.writer.status().reader);
        drop(ts.holder.take());
        tokio::time::sleep(Duration::from_secs(6)).await;
        assert!(!ts.watcher_active());
        assert!(!ts.writer.status().held);
        assert!(ts.kb.lease_is_free());
        let rebuild = ts.rebuild().await;
        assert!(rebuild.get("error").is_some(), "{rebuild}");
        assert!(ts.kb.lease_is_free());
        let notice = rebuild["writer_notice"].as_str().expect("writer_notice");
        assert!(notice.contains("probably exited"), "{notice}");
        assert!(notice.contains("restarted"), "{notice}");
        finish(&ts, handles).await;
    }

    /// T-L9 4: a reader writes nothing, whatever happens to the files.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_reader_writes_nothing() {
        let kb = TestKb::new("nothing");
        kb.index_once();
        let before = kb.rows();
        assert!(
            before.iter().any(|(t, _)| t == "documents"),
            "the index must hold rows to compare: {before:?}"
        );
        let (mut ts, handles) = start_writer_for_test(
            kb,
            StartOpts {
                holder: Some(Holder::Serve),
                ..Default::default()
            },
        );
        assert!(ts.writer.status().reader);
        std::fs::write(ts.kb.kb.join("added.md"), "# Added\n\nNew text.\n").unwrap();
        std::fs::write(ts.kb.kb.join("note.md"), "# Note\n\nEdited text.\n").unwrap();
        std::fs::write(ts.kb.kb.join("gone.md"), "# Gone\n\nSoon gone.\n").unwrap();
        tokio::time::sleep(Duration::from_millis(500)).await;
        std::fs::remove_file(ts.kb.kb.join("gone.md")).unwrap();
        drop(ts.holder.take());
        tokio::time::sleep(Duration::from_secs(6)).await;
        let _ = ts.rebuild().await;
        assert_eq!(ts.kb.rows(), before);
        finish(&ts, handles).await;
        assert_eq!(ts.kb.rows(), before);
    }

    /// T-L9 5: the reader's notice names what kind of holder kept it out.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_reader_started_under_a_one_off_or_another_kb_or_unknown_holder() {
        // A one-off index run on this knowledge base.
        let (ts, handles) = start_writer_for_test(
            TestKb::new("one-off"),
            StartOpts {
                holder: Some(Holder::Index),
                ..Default::default()
            },
        );
        let notice = ts.writer.status().reader_notice.expect("a reader notice");
        assert!(notice.contains("one-off index run"), "{notice}");
        assert!(notice.contains("even while it runs"), "{notice}");
        assert!(
            notice.contains("which can be stale: a process that was killed"),
            "{notice}"
        );
        assert!(!notice.contains("keeps the index current"), "{notice}");
        finish(&ts, handles).await;
        drop(ts);

        // A server on a sibling knowledge base sharing the index.
        let (ts, handles) = start_writer_for_test(
            TestKb::new("other-kb"),
            StartOpts {
                holder: Some(Holder::OtherKbServe),
                ..Default::default()
            },
        );
        let notice = ts.writer.status().reader_notice.expect("a reader notice");
        assert!(notice.contains("different knowledge base"), "{notice}");
        assert!(notice.contains(&ts.kb.kb.display().to_string()), "{notice}");
        assert!(!notice.contains("keeps the index current"), "{notice}");
        finish(&ts, handles).await;
        drop(ts);

        // A holder whose writer.json is gone.
        let (ts, handles) = start_writer_for_test(
            TestKb::new("unknown"),
            StartOpts {
                holder: Some(Holder::ServeWithoutInfo),
                ..Default::default()
            },
        );
        let notice = ts.writer.status().reader_notice.expect("a reader notice");
        assert!(notice.contains("cannot be told"), "{notice}");
        finish(&ts, handles).await;
    }

    /// T-L9 6: a lock that cannot be taken at all leaves a reader too.
    // The injected acquisition returns the lease's own `LeaseError`.
    #[allow(clippy::result_large_err)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_server_whose_lock_fails_is_an_io_reader() {
        let (ts, handles) = start_writer_for_test(
            TestKb::new("io"),
            StartOpts {
                acquire: Some(Box::new(|db: &Path, _role: Role, _kb: &Path| {
                    Err(LeaseError::Io {
                        lock_path: WriterLease::lock_path_for(db),
                        source: std::io::Error::new(
                            std::io::ErrorKind::PermissionDenied,
                            "refused for a test",
                        ),
                    })
                })),
                ..Default::default()
            },
        );
        let status = ts.writer.status();
        assert!(status.reader, "{status:?}");
        assert!(!status.held, "{status:?}");
        let notice = status.reader_notice.expect("a reader notice");
        let lock = WriterLease::lock_path_for(&ts.kb.db_path);
        assert!(notice.contains(&lock.display().to_string()), "{notice}");
        assert!(handles.watcher.is_none());
        finish(&ts, handles).await;
    }

    /// T-L9 7: the janitor settles a rebuild that panicked, and the
    /// watcher's claim keeps the lease.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_janitor_settles_a_panicked_rebuild_claim_and_keeps_the_watchers_lease() {
        let (ts, handles) = start_writer_for_test(TestKb::new("settle"), StartOpts::default());
        let guard = ts.writer.claim(Claimant::Rebuild).unwrap();
        panic_with(guard);
        assert!(ts.writer.has_pending());
        let settled = eventually(Duration::from_secs(10), || !ts.writer.has_pending()).await;
        assert!(settled, "the janitor must settle within 10 s");
        assert!(
            !ts.kb.lease_is_free(),
            "the watcher's claim keeps the lease"
        );
        assert_eq!(ts.query_only(), 0);
        finish(&ts, handles).await;
    }

    /// A lease taken anew has its embedding meta checked: an index whose
    /// meta changed stops the start and leaves it read-only and unleased.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_fresh_claim_with_a_changed_meta_stops_the_start() {
        let mut opts = StartOpts::default();
        let ts = serve_for_test(TestKb::with_model("meta", "other-model"), &mut opts);
        let role = claim_writer_role(
            &ts.writer,
            &ts.shared.db,
            &ts.kb.db_path,
            &ts.kb.kb,
            true,
            ContextMode::Off,
        );
        assert!(role.is_err(), "a changed meta must stop the start");
        assert_eq!(ts.query_only(), 1);
        assert!(ts.kb.lease_is_free());
        assert!(!ts.writer.status().held);
        finish(&ts, WriterRoleHandles::default()).await;
    }

    /// `--no-watch` takes no lease at start, but the janitor still runs.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_no_watch_server_takes_no_lease_at_start() {
        let (ts, handles) = start_writer_for_test(
            TestKb::new("no-watch"),
            StartOpts {
                no_watch: true,
                ..Default::default()
            },
        );
        assert!(ts.kb.lease_is_free());
        assert!(!ts.writer.status().held);
        assert!(!ts.writer.status().reader);
        assert_eq!(ts.query_only(), 1);
        assert!(handles.watcher.is_none());
        let janitor = handles.janitor.as_ref().expect("the janitor runs");
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(!janitor.is_finished());
        finish(&ts, handles).await;
    }

    /// The janitor rewrites a heartbeat that went stale.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_janitor_refreshes_a_stale_heartbeat() {
        let (ts, handles) = start_writer_for_test(TestKb::new("stale"), StartOpts::default());
        // No check that it reads stale right after the overwrite: a tick
        // landing between the write and the read would fail it falsely.
        ts.kb.age_the_heartbeat();
        let fresh = eventually(Duration::from_secs(10), || ts.kb.heartbeat_is_fresh()).await;
        assert!(fresh, "the heartbeat must be fresh again within two ticks");
        finish(&ts, handles).await;
    }

    /// The janitor runs from the claim on, so the heartbeat stays fresh
    /// while the models download, before any watcher starts. `watching` is
    /// left unsaid until the watcher reports.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_janitor_keeps_the_heartbeat_fresh_before_the_watcher_starts() {
        let (ts, handles) = start_writer_for_test(
            TestKb::new("before-watcher"),
            StartOpts {
                skip_watcher: true,
                ..Default::default()
            },
        );
        assert!(ts.writer.status().held);
        assert!(handles.watcher.is_none());
        ts.kb.age_the_heartbeat();
        let fresh = eventually(Duration::from_secs(10), || ts.kb.heartbeat_is_fresh()).await;
        assert!(fresh, "the heartbeat must be fresh again within two ticks");
        let holder = ts.kb.holder().expect("writer.json");
        assert_ne!(holder.watching, Some(true), "{holder:?}");
        assert_eq!(holder.watching, None, "{holder:?}");
        // The watcher was never started: its claim is released by shutdown.
        finish(&ts, handles).await;
        assert!(ts.kb.lease_is_free());
    }

    /// The heartbeat does not wait for a settle that waits for the db.
    ///
    /// A janitor that beats first and then awaits the settle goes red here:
    /// the first tick after the panic starts a settle that blocks on the db
    /// guard held below, the loop waits on it, and no later tick comes to
    /// refresh the heartbeat overwritten after that.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_heartbeat_does_not_wait_for_a_settle_that_waits_for_the_db() {
        let (ts, handles) = start_writer_for_test(TestKb::new("no-wait"), StartOpts::default());
        let db = Arc::clone(&ts.shared.db);
        let (held_tx, held_rx) = std::sync::mpsc::channel();
        let (go_tx, go_rx) = std::sync::mpsc::channel::<()>();
        let db_holder = std::thread::spawn(move || {
            let _g = recover_db(db.lock());
            held_tx.send(()).unwrap();
            let _ = go_rx.recv();
        });
        held_rx.recv().unwrap();
        let guard = ts.writer.claim(Claimant::Rebuild).unwrap();
        panic_with(guard);
        assert!(ts.writer.has_pending());
        // Longer than a tick, so one tick starts a settle that blocks on the db.
        tokio::time::sleep(JANITOR_TICK + Duration::from_secs(1)).await;
        ts.kb.age_the_heartbeat();
        let fresh = eventually(Duration::from_secs(10), || ts.kb.heartbeat_is_fresh()).await;
        assert!(fresh, "the heartbeat must not wait for the settle");
        assert!(
            ts.writer.has_pending(),
            "the settle is still waiting for the db"
        );
        go_tx.send(()).unwrap();
        db_holder.join().unwrap();
        let settled = eventually(Duration::from_secs(10), || !ts.writer.has_pending()).await;
        assert!(settled, "the settle must finish once the db is free");
        finish(&ts, handles).await;
    }
}
