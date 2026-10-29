//! The writer role a `groove serve` takes at start (ADR-0031): writer, reader
//! or no watch, the janitor that keeps the holder's heartbeat and settles
//! panicked claims, and the watcher a writer runs.

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
    /// `--no-watch`: no lease at start; a [`crate::server::KbServer::rebuild_index`] takes it.
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
            crate::writer_lease::kb_shown(&h.kb_path)
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
/// The watcher reports its watch failures and successes to the slot under
/// its claim's generation, so a report that outlives the claim is dropped;
/// an error its loop ends with is recorded before the release, for the
/// stopped watcher's notice. A reader and a `--no-watch` server start
/// nothing here.
pub(crate) fn start_writer_role(
    shared: &KbServerShared,
    role: StartRole,
    watcher_state: impl FnOnce() -> WatcherState + Send + 'static,
    handles: &mut WriterRoleHandles,
) {
    let StartRole::Writer(guard) = role else {
        return;
    };
    let generation = guard.generation();
    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    let mut state = watcher_state();
    state.watch_status = shared.writer.watch_status_callback(generation);
    let writer = Arc::clone(&shared.writer);
    let db = Arc::clone(&shared.db);
    handles.watcher = Some(tokio::spawn(async move {
        let exit = run_watch_loop_until(state, stop_rx).await;
        if let Err(e) = &exit {
            eprintln!("watcher exited with error: {e}");
        }
        let _ = tokio::task::spawn_blocking(move || {
            end_watcher(&writer, &db, guard, generation, &exit);
        })
        .await;
    }));
    handles.watcher_stop = Some(stop_tx);
}

/// The server's shutdown of the writer role (ADR-0031). Later claims are
/// refused first, so the watcher's release below records no stop. The
/// janitor is aborted. The watcher is told to stop at its next batch
/// boundary and awaited, however long the batch it is handling takes, so a
/// batch never loses the writable connection halfway. Last, under the db
/// guard, `query_only` goes on and the lease is dropped, with any claim
/// still marked or queued. A start that failed before the watcher started
/// passes the janitor only.
pub(crate) async fn shutdown_writer_role(
    writer: &Arc<WriterSlot>,
    db: &Arc<Mutex<Database>>,
    handles: WriterRoleHandles,
) {
    let WriterRoleHandles {
        janitor,
        watcher,
        watcher_stop,
    } = handles;
    writer.begin_shutdown();
    if let Some(janitor) = janitor {
        janitor.abort();
        let _ = janitor.await;
    }
    if let Some(stop) = &watcher_stop {
        // An error means the loop has already ended.
        let _ = stop.send(true);
    }
    if let Some(watcher) = watcher {
        let _ = watcher.await;
    }
    drop(watcher_stop);
    let writer = Arc::clone(writer);
    let db = Arc::clone(db);
    let shut = tokio::task::spawn_blocking(move || {
        let db = recover_db(db.lock());
        writer.shutdown(&db);
    })
    .await;
    if let Err(e) = shut {
        tracing::warn!(error = %e, "the writer lease shutdown did not finish");
    }
}

/// The end of the watcher's task, under the db guard: record the error
/// its loop ended with, then release its claim, so the stop the release
/// records carries that error.
fn end_watcher(
    writer: &WriterSlot,
    db: &Mutex<Database>,
    guard: ClaimGuard,
    generation: u64,
    exit: &anyhow::Result<()>,
) {
    let db = recover_db(db.lock());
    if let Err(e) = exit {
        writer.set_watcher_exit(generation, e.to_string());
    }
    writer.release(guard, &db);
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

        /// Every row of the documents, chunks and index_meta tables, read through
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

    /// The end of every test here: the server's shutdown, so no janitor,
    /// watcher or lease is left behind.
    async fn finish(ts: &TestServer, handles: WriterRoleHandles) {
        shutdown_writer_role(&ts.writer, &ts.shared.db, handles).await;
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

    /// Drop the guard inside a caught panic, as a panicking handler would.
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

    /// The first server on an index writes it and watches files.
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

    /// A server started while another holds the lease reads only,
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

    /// A reader does not take the lease when the holder leaves.
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

    /// A reader writes nothing, whatever happens to the files.
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

    /// The reader's notice names what kind of holder kept it out.
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

    /// A lock that cannot be taken at all leaves a reader too.
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
                        source: std::io::Error::from_raw_os_error(IO_TEST_OS_ERROR),
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
        let lock = lock.display().to_string();
        assert!(notice.contains(&lock), "{notice}");
        // The error is the OS error alone, not the lease's sentence that
        // names the lock path again.
        let os_error =
            crate::embedder::ascii_io_error(&std::io::Error::from_raw_os_error(IO_TEST_OS_ERROR));
        assert!(notice.contains(&format!("{lock} ({os_error})")), "{notice}");
        assert_eq!(notice.matches(&lock).count(), 1, "{notice}");
        assert!(handles.watcher.is_none());
        finish(&ts, handles).await;
    }

    /// The OS error code the injected lock failure carries.
    const IO_TEST_OS_ERROR: i32 = 5;

    /// A claim that can only be `Reader` or `Closed` stops the start with
    /// an error rather than a role.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_claim_refused_as_reader_or_closed_is_an_error() {
        let (ts, handles) = start_writer_for_test(
            TestKb::new("refused"),
            StartOpts {
                holder: Some(Holder::Serve),
                ..Default::default()
            },
        );
        assert!(ts.writer.status().reader);
        let again = claim_writer_role(
            &ts.writer,
            &ts.shared.db,
            &ts.kb.db_path,
            &ts.kb.kb,
            true,
            ContextMode::Off,
        );
        assert!(again.is_err(), "a reader's second claim must be an error");
        finish(&ts, handles).await;
        drop(ts);

        let mut opts = StartOpts::default();
        let ts = serve_for_test(TestKb::new("closed"), &mut opts);
        ts.writer.begin_shutdown();
        let role = claim_writer_role(
            &ts.writer,
            &ts.shared.db,
            &ts.kb.db_path,
            &ts.kb.kb,
            true,
            ContextMode::Off,
        );
        assert!(
            role.is_err(),
            "a claim after shutdown began must be an error"
        );
        assert!(ts.kb.lease_is_free());
        assert_eq!(ts.query_only(), 1);
        finish(&ts, WriterRoleHandles::default()).await;
    }

    /// The janitor settles a rebuild that panicked, and the
    /// watcher's claim keeps the lease.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_janitor_settles_a_panicked_rebuild_claim_and_keeps_the_watchers_lease() {
        let (ts, handles) = start_writer_for_test(TestKb::new("settle"), StartOpts::default());
        // Hold the db so no settle can finish before the pending check.
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
        go_tx.send(()).unwrap();
        db_holder.join().unwrap();
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
    /// while the models download, before any watcher starts.
    /// [`crate::writer_lease::HolderInfo::watching`] is
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

    /// The deadline every poll below waits up to.
    const WITHIN: Duration = Duration::from_secs(10);

    /// The shortened backoff of a watcher that cannot register its watch.
    const SHORT_BACKOFF: (Duration, Duration) =
        (Duration::from_millis(50), Duration::from_millis(200));

    /// A start whose watcher watches `missing`, a directory not there yet.
    fn unwatchable(missing: &Path) -> StartOpts {
        StartOpts {
            kb_path: Some(missing.to_path_buf()),
            watch_backoff: Some(SHORT_BACKOFF),
            ..Default::default()
        }
    }

    /// `writer_notice` of [`TestServer::search`], or `None` when it carries none.
    async fn search_notice(ts: &TestServer) -> Option<String> {
        ts.search().await["writer_notice"]
            .as_str()
            .map(str::to_string)
    }

    /// Poll [`TestServer::search`] every 250 ms until its `writer_notice` meets `cond` or
    /// `within` passes. Returns whether it did, and the last notice read.
    async fn notice_within(
        ts: &TestServer,
        within: Duration,
        cond: impl Fn(Option<&str>) -> bool,
    ) -> (bool, Option<String>) {
        let deadline = Instant::now() + within;
        loop {
            let notice = search_notice(ts).await;
            if cond(notice.as_deref()) {
                return (true, notice);
            }
            if Instant::now() >= deadline {
                return (false, notice);
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }

    /// Wait for admin status to carry the watch error, and return it.
    async fn the_watch_error(ts: &TestServer) -> String {
        let seen = eventually(WITHIN, || ts.writer.status().watch_error.is_some()).await;
        assert!(seen, "the watch error must show within 10 s");
        ts.writer.status().watch_error.expect("a watch error")
    }

    /// Abort the watcher's task as a crash would end it, await the same
    /// handle, and settle the claim its dropped guard queued.
    async fn abort_the_watcher(ts: &TestServer, handles: &mut WriterRoleHandles) {
        let watcher = handles.watcher.take().expect("a writer runs a watcher");
        watcher.abort();
        match watcher.await {
            Err(e) => assert!(e.is_cancelled(), "{e}"),
            Ok(()) => panic!("the watcher task ended before the abort"),
        }
        let db = recover_db(ts.shared.db.lock());
        ts.writer.settle_pending(&db);
    }

    /// A server on this same knowledge base reporting `watching`.
    fn serve_holder(kb: &TestKb, watching: Option<bool>) -> WriterLease {
        let mut lease = kb.hold_lease(Role::Serve, &kb.kb);
        lease.touch_heartbeat(watching);
        lease
    }

    /// A reader started under a one-off index run that has since exited.
    fn reader_whose_holder_left(prefix: &str) -> (TestServer, WriterRoleHandles) {
        let (mut ts, handles) = start_writer_for_test(
            TestKb::new(prefix),
            StartOpts {
                holder: Some(Holder::Index),
                ..Default::default()
            },
        );
        assert!(ts.writer.status().reader);
        drop(ts.holder.take());
        (ts, handles)
    }

    /// A writer that cannot register its watch says so on every
    /// reply and in writer.json, until the watch succeeds.
    // Red if the watcher gets no status callback, or one for a generation
    // other than its claim's: no watch error shows and writer.json never
    // turns `watching` to `false` or back to `true`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_writer_that_cannot_watch_reports_it_until_the_watch_succeeds() {
        let kb = TestKb::new("cannot-watch");
        let missing = kb.dir.join("not-yet");
        let (ts, handles) = start_writer_for_test(kb, unwatchable(&missing));
        let error = the_watch_error(&ts).await;
        assert!(error.contains("could not watch"), "{error}");
        let (seen, notice) = notice_within(&ts, WITHIN, |n| {
            n.is_some_and(|n| n.contains("could not watch"))
        })
        .await;
        assert!(seen, "{notice:?}");
        let holder = ts.kb.holder().expect("writer.json");
        assert_eq!(holder.watching, Some(false), "{holder:?}");

        std::fs::create_dir_all(&missing).unwrap();
        let cleared = eventually(WITHIN, || ts.writer.status().watch_error.is_none()).await;
        assert!(cleared, "{:?}", ts.writer.status());
        let json = ts.search().await;
        assert!(json.get("writer_notice").is_none(), "{json}");
        let watching = eventually(WITHIN, || {
            ts.kb.holder().is_some_and(|h| h.watching == Some(true))
        })
        .await;
        assert!(watching, "{:?}", ts.kb.holder());
        finish(&ts, handles).await;
    }

    /// Once the watcher's task is gone, its bridge thread still
    /// retries the watch, but nothing it reports reaches the slot.
    // Red if a report from the aborted claim's generation is still taken
    // (the watch error clears once the dir exists), or if the stop is never
    // recorded (no watcher_stopped, the notice stays the watch error).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn after_the_watcher_task_is_aborted_its_bridge_thread_writes_nothing() {
        let kb = TestKb::new("aborted");
        let missing = kb.dir.join("not-yet");
        let (ts, mut handles) = start_writer_for_test(kb, unwatchable(&missing));
        let _ = the_watch_error(&ts).await;
        abort_the_watcher(&ts, &mut handles).await;
        assert_eq!(ts.writer.watcher_stopped_error(), Some(None));
        assert!(ts.kb.lease_is_free(), "no claim is left to hold the lease");
        let before = ts
            .writer
            .status()
            .watch_error
            .expect("the watch error is kept");
        assert!(before.contains("could not watch"), "{before}");

        std::fs::create_dir_all(&missing).unwrap();
        // A negative check: well over twice the backoff's ceiling gives the
        // bridge thread time to register the watch and report it, even on
        // a slow machine.
        tokio::time::sleep(Duration::from_secs(1)).await;
        let status = ts.writer.status();
        assert_eq!(status.watch_error.as_deref(), Some(before.as_str()));
        let stopped = status.watcher_stopped.expect("watcher_stopped");
        assert!(stopped.contains("stopped watching files"), "{stopped}");
        assert!(stopped.contains("ended without finishing"), "{stopped}");
        let notice = search_notice(&ts).await.expect("writer_notice");
        assert_eq!(notice, stopped);
        assert!(!notice.contains("catch-up"), "{notice}");
        finish(&ts, handles).await;
    }

    /// The error a watcher's loop ended with is what its stop records.
    // Red if the release comes before the error is recorded: the stop is
    // recorded without it, as after a panic.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_watcher_that_ends_with_an_error_records_it_in_its_stop() {
        let (mut ts, handles) = start_writer_for_test(
            TestKb::new("ends-with-error"),
            StartOpts {
                skip_watcher: true,
                ..Default::default()
            },
        );
        let Some(StartRole::Writer(guard)) = ts.unstarted.take() else {
            panic!("the start must make a writer");
        };
        let generation = guard.generation();
        let exit: anyhow::Result<()> = Err(anyhow::anyhow!("boom"));
        end_watcher(&ts.writer, &ts.shared.db, guard, generation, &exit);
        assert_eq!(
            ts.writer.watcher_stopped_error(),
            Some(Some("boom".to_string()))
        );
        assert!(ts.kb.lease_is_free());
        finish(&ts, handles).await;
    }

    /// A reader's notice says what writer.json names now, not
    /// only who held the lease when it started.
    // Red if the notice words the holder at start (a one-off run) instead
    // of the server that took the lease after it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_reader_notice_follows_the_current_holder() {
        let (ts, handles) = reader_whose_holder_left("follows");
        let info = WriterLease::info_path_for(&ts.kb.db_path);
        let names = format!("Now {} names pid", info.display());

        let same = serve_holder(&ts.kb, Some(true));
        let notice = search_notice(&ts).await.expect("writer_notice");
        assert!(
            notice.contains("a server watching this same knowledge base"),
            "{notice}"
        );
        assert!(
            notice.contains("does not need a restart for that"),
            "{notice}"
        );
        drop(same);

        let other = ts.kb.other_kb();
        let mut sibling = ts.kb.hold_lease(Role::Serve, &other);
        sibling.touch_heartbeat(Some(true));
        let notice = search_notice(&ts).await.expect("writer_notice");
        assert!(
            !notice.contains("a server watching this same knowledge base"),
            "{notice}"
        );
        assert!(
            !notice.contains("does not need a restart for that"),
            "{notice}"
        );
        assert!(notice.contains(&names), "{notice}");
        drop(sibling);
        finish(&ts, handles).await;
    }

    /// A writer whose watcher stopped names another server only
    /// while that server reports a running watcher on this knowledge base.
    // Red if the stop is not recorded once the aborted claim is settled, or
    // if another server is named without checking that it is watching.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_stopped_writer_names_another_server_only_while_it_keeps_the_index_current() {
        let (ts, mut handles) = start_writer_for_test(TestKb::new("stopped"), StartOpts::default());
        let started = eventually(WITHIN, || ts.watcher_active()).await;
        assert!(started, "the watcher must start within 10 s");
        abort_the_watcher(&ts, &mut handles).await;
        assert!(ts.kb.lease_is_free());

        let mut other = serve_holder(&ts.kb, Some(true));
        let notice = search_notice(&ts).await.expect("writer_notice");
        assert!(notice.contains("stopped watching files"), "{notice}");
        assert!(
            notice.contains("another server watching this same knowledge base"),
            "{notice}"
        );

        other.touch_heartbeat(Some(false));
        let notice = search_notice(&ts).await.expect("writer_notice");
        assert!(notice.contains("stopped watching files"), "{notice}");
        assert!(!notice.contains("another server watching"), "{notice}");

        other.touch_heartbeat(Some(true));
        ts.kb.age_the_heartbeat();
        let notice = search_notice(&ts).await.expect("writer_notice");
        assert!(!notice.contains("another server watching"), "{notice}");

        drop(other);
        let notice = search_notice(&ts).await.expect("writer_notice");
        assert!(notice.contains("stopped watching files"), "{notice}");
        assert!(!notice.contains("another server watching"), "{notice}");
        finish(&ts, handles).await;
    }

    /// A reader's notice does not promise that a holder
    /// keeps the index current when its heartbeat is stale or missing, or
    /// when it does not report a running watcher.
    // Red if the notice trusts writer.json's `watching` without its
    // heartbeat, or a holder that does not say it is watching.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_reader_notice_does_not_trust_a_stale_or_not_watching_holder() {
        let (ts, handles) = reader_whose_holder_left("stale-holder");
        let mut holder = serve_holder(&ts.kb, Some(true));

        ts.kb.age_the_heartbeat();
        let notice = search_notice(&ts).await.expect("writer_notice");
        assert!(
            !notice.contains("does not need a restart for that"),
            "{notice}"
        );
        assert!(notice.contains("heartbeat is older than"), "{notice}");
        assert!(notice.contains("probably exited"), "{notice}");

        let mut h = ts.kb.holder().expect("writer.json");
        h.heartbeat_at = None;
        std::fs::write(
            WriterLease::info_path_for(&ts.kb.db_path),
            serde_json::to_vec_pretty(&h).unwrap(),
        )
        .unwrap();
        let raw = std::fs::read_to_string(WriterLease::info_path_for(&ts.kb.db_path)).unwrap();
        assert!(!raw.contains("heartbeat_at"), "{raw}");
        let notice = search_notice(&ts).await.expect("writer_notice");
        assert!(
            !notice.contains("does not need a restart for that"),
            "{notice}"
        );
        assert!(notice.contains("heartbeat is older than"), "{notice}");
        assert!(notice.contains("probably exited"), "{notice}");

        holder.touch_heartbeat(Some(false));
        let notice = search_notice(&ts).await.expect("writer_notice");
        assert!(
            notice.contains("does not report that its watcher is watching"),
            "{notice}"
        );
        assert!(
            notice.contains("not being indexed while that lasts"),
            "{notice}"
        );
        assert!(
            !notice.contains("does not need a restart for that"),
            "{notice}"
        );
        drop(holder);
        finish(&ts, handles).await;
    }

    /// The path and content hash of every row of the documents table, read
    /// through a
    /// connection of its own.
    fn documents(kb: &TestKb) -> Vec<(String, String)> {
        let conn = rusqlite::Connection::open_with_flags(
            &kb.db_path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        let mut stmt = conn
            .prepare("SELECT path, content_hash FROM documents ORDER BY path")
            .unwrap();
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    }

    fn sha256(bytes: &[u8]) -> String {
        use sha2::Digest;
        format!("{:x}", sha2::Sha256::digest(bytes))
    }

    /// Wait until [`WatchBatchProbe::started`] and
    /// [`WatchBatchProbe::finished`] have been equal for 2 s, longer than the
    /// 100 ms debounce window, so no batch is left to come. Up to 10 s.
    async fn wait_until_quiet(probe: &WatchBatchProbe) {
        use std::sync::atomic::Ordering::SeqCst;
        let deadline = Instant::now() + WITHIN;
        let mut last = None;
        let mut since = Instant::now();
        loop {
            let now = (probe.started.load(SeqCst), probe.finished.load(SeqCst));
            if now.0 != now.1 || last != Some(now) {
                last = Some(now);
                since = Instant::now();
            } else if since.elapsed() >= Duration::from_secs(2) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "the watcher did not go quiet within 10 s: {probe:?}"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// The db guard, held on a thread of its own (a `std::sync::Mutex`
    /// guard cannot cross threads), until [`DbHolder::let_go`].
    struct DbHolder {
        ask: std::sync::mpsc::Sender<Option<std::sync::mpsc::Sender<i64>>>,
        thread: std::thread::JoinHandle<()>,
    }

    impl DbHolder {
        fn hold(db: Arc<Mutex<Database>>) -> Self {
            let (held_tx, held_rx) = std::sync::mpsc::channel();
            let (ask, asked) = std::sync::mpsc::channel::<Option<std::sync::mpsc::Sender<i64>>>();
            let thread = std::thread::spawn(move || {
                let guard = recover_db(db.lock());
                held_tx.send(()).unwrap();
                // `Some(reply)` asks for `PRAGMA query_only`; `None` lets go.
                while let Ok(Some(reply)) = asked.recv() {
                    let _ = reply.send(guard.query_only_for_test());
                }
            });
            held_rx.recv().unwrap();
            Self { ask, thread }
        }

        /// `PRAGMA query_only` read through the held guard.
        fn query_only(&self) -> i64 {
            let (tx, rx) = std::sync::mpsc::channel();
            self.ask.send(Some(tx)).unwrap();
            rx.recv().unwrap()
        }

        fn let_go(self) {
            self.ask.send(None).unwrap();
            self.thread.join().unwrap();
        }
    }

    /// One case of the shutdown that has to wait for a running batch.
    struct BatchCase {
        /// Named in the skip line.
        name: &'static str,
        /// Files the writer's watcher indexes before the batch.
        prepare: fn(&Path),
        /// Whether the rows of `prepare` are in the index.
        prepared: fn(&[(String, String)]) -> bool,
        /// The change whose batch the shutdown has to wait for.
        trigger: fn(&Path),
        /// The rows that batch leaves; checked only when it ran alone.
        rows_after: fn(&[(String, String)]),
    }

    /// Start a writer, let the batch of `case` ([`BatchCase::trigger`]) start
    /// while the test holds
    /// the db, and shut down: the shutdown waits for the batch, keeping the
    /// lease and the writable connection until it has finished. The rows the
    /// batch leaves are checked only when it was the one batch the change
    /// made and nothing was left queued at the stop; otherwise a
    /// `T-L9-11-ROW-SKIPPED` line says why.
    // Red if the shutdown aborts the watcher instead of stopping it: the
    // aborted batch is never counted finished, and when the shutdown takes
    // the db before it, its writes meet a read-only connection.
    async fn run_batch_probe_shutdown(case: BatchCase) {
        use std::sync::atomic::Ordering::SeqCst;
        let probe = Arc::new(WatchBatchProbe::default());
        let kb = TestKb::new(case.name);
        // The watcher gets the canonical kb, as `groove serve` hands it one.
        // On macOS the temp dir is under `/var`, a link to `/private/var`, and
        // FSEvents names every path under the latter: against the spelling as
        // written no event path is under the kb, so nothing is indexed and a
        // batch finishes without taking the db.
        let watch_kb = kb.kb.canonicalize().unwrap();
        let (ts, handles) = start_writer_for_test(
            kb,
            StartOpts {
                batch_probe: Some(Arc::clone(&probe)),
                debounce_ms: Some(100),
                kb_path: Some(watch_kb),
                ..Default::default()
            },
        );
        let armed = eventually(WITHIN, || {
            ts.kb.holder().is_some_and(|h| h.watching == Some(true))
        })
        .await;
        assert!(armed, "the watch must be registered within 10 s");
        (case.prepare)(&ts.kb.kb);
        let indexed = eventually(WITHIN, || (case.prepared)(&documents(&ts.kb))).await;
        assert!(indexed, "{:?}", documents(&ts.kb));
        wait_until_quiet(&probe).await;
        let n0 = probe.started.load(SeqCst);
        let n0_emitted = probe.emitted.load(SeqCst);

        let held = DbHolder::hold(Arc::clone(&ts.shared.db));
        let waits_before = probe.lock_waits.load(SeqCst);
        (case.trigger)(&ts.kb.kb);
        // Not "a batch after `n0` is in flight": on Linux the write's
        // `Access(Open)` can come as a batch of its own that takes no db and
        // finishes ahead of the one that blocks. A rise in `lock_waits` says a
        // batch has reached the held db.
        let reached = eventually(WITHIN, || probe.lock_waits.load(SeqCst) > waits_before).await;
        assert!(
            reached,
            "a batch must reach the held db within 10 s: {probe:?}"
        );
        // The loop handles one batch at a time, and this one is blocked.
        let (s1, f1) = (probe.started.load(SeqCst), probe.finished.load(SeqCst));

        let writer = Arc::clone(&ts.writer);
        let db = Arc::clone(&ts.shared.db);
        let shutdown =
            tokio::spawn(async move { shutdown_writer_role(&writer, &db, handles).await });
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(
            !shutdown.is_finished(),
            "the shutdown must wait for the batch"
        );
        assert_eq!(probe.started.load(SeqCst), s1, "{probe:?}");
        assert_eq!(probe.finished.load(SeqCst), f1, "{probe:?}");
        assert!(!ts.kb.lease_is_free(), "the lease is kept while it waits");
        assert_eq!(held.query_only(), 0);

        held.let_go();
        tokio::time::timeout(WITHIN, shutdown)
            .await
            .expect("the shutdown must end within 10 s of the db being let go")
            .expect("the shutdown task panicked");
        let (started, finished) = (probe.started.load(SeqCst), probe.finished.load(SeqCst));
        assert_eq!(finished, started, "{probe:?}");
        assert_eq!(ts.query_only(), 1);
        assert!(ts.kb.lease_is_free());
        assert_eq!(ts.writer.status().watcher_stopped, None);

        // A batch the debouncer hands over after the stop is still counted
        // in `emitted`; 500 ms covers the 100 ms window and its tick.
        tokio::time::sleep(Duration::from_millis(500)).await;
        let emitted = probe.emitted.load(SeqCst);
        let queued = probe.stopped_with_queued.load(SeqCst);
        let skipped = if queued {
            Some("queued")
        } else if started - n0 != 1 {
            Some("started")
        } else if emitted - n0_emitted != 1 {
            Some("emitted")
        } else {
            None
        };
        match skipped {
            None => (case.rows_after)(&documents(&ts.kb)),
            Some(reason) => eprintln!(
                "T-L9-11-ROW-SKIPPED {} reason={reason} os={} started={started} n0={n0} emitted={emitted} n0_emitted={n0_emitted} queued={queued}",
                case.name,
                std::env::consts::OS
            ),
        }
    }

    /// The shutdown waits for a running dir-rename batch.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn shutdown_waits_for_a_running_dir_rename_batch() {
        const FILES: [&str; 3] = ["a.md", "b.md", "c.md"];
        run_batch_probe_shutdown(BatchCase {
            name: "dir_rename",
            prepare: |kb| {
                let dir = kb.join("before");
                std::fs::create_dir_all(&dir).unwrap();
                for f in FILES {
                    std::fs::write(dir.join(f), format!("# {f}\n\nText of {f}.\n")).unwrap();
                }
            },
            prepared: |docs| {
                FILES
                    .iter()
                    .all(|f| docs.iter().any(|(p, _)| *p == format!("before/{f}")))
            },
            trigger: |kb| std::fs::rename(kb.join("before"), kb.join("after")).unwrap(),
            rows_after: |docs| {
                for f in FILES {
                    let to = format!("after/{f}");
                    assert!(docs.iter().any(|(p, _)| *p == to), "{to}: {docs:?}");
                }
                assert!(
                    !docs.iter().any(|(p, _)| p.starts_with("before/")),
                    "{docs:?}"
                );
            },
        })
        .await;
    }

    const SINGLE_BEFORE: &str = "# Single\n\nThe first text.\n";
    const SINGLE_AFTER: &str = "# Single\n\nThe text after the rewrite.\n";

    /// The shutdown waits for a running batch of one rewritten file.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn shutdown_waits_for_a_running_single_file_batch() {
        run_batch_probe_shutdown(BatchCase {
            name: "single_file",
            prepare: |kb| std::fs::write(kb.join("single.md"), SINGLE_BEFORE).unwrap(),
            prepared: |docs| {
                let hash = sha256(SINGLE_BEFORE.as_bytes());
                docs.iter().any(|(p, h)| p == "single.md" && *h == hash)
            },
            trigger: |kb| std::fs::write(kb.join("single.md"), SINGLE_AFTER).unwrap(),
            rows_after: |docs| {
                let hash = sha256(SINGLE_AFTER.as_bytes());
                assert!(
                    docs.iter().any(|(p, h)| p == "single.md" && *h == hash),
                    "{docs:?}"
                );
            },
        })
        .await;
    }

    /// The release of the watcher a shutdown stopped records no stop.
    // Red if the shutdown stops the watcher before it begins: the watcher's
    // release records `watcher_stopped`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_shutdown_stop_does_not_record_watcher_stopped() {
        let (ts, handles) = start_writer_for_test(TestKb::new("no-stop"), StartOpts::default());
        let started = eventually(WITHIN, || ts.watcher_active()).await;
        assert!(started, "the watcher must start within 10 s");
        shutdown_writer_role(&ts.writer, &ts.shared.db, handles).await;
        assert_eq!(ts.writer.status().watcher_stopped, None);
        assert!(matches!(
            ts.writer.claim(Claimant::Rebuild),
            Err(LeaseError::Closed)
        ));
        assert!(ts.kb.lease_is_free());
    }

    /// A reader's shutdown only closes the slot: it held no lease.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn shutdown_of_a_reader_only_closes() {
        let (mut ts, handles) = start_writer_for_test(
            TestKb::new("reader-shutdown"),
            StartOpts {
                holder: Some(Holder::Serve),
                ..Default::default()
            },
        );
        assert!(ts.writer.status().reader);
        drop(ts.holder.take());
        assert!(ts.kb.lease_is_free());
        shutdown_writer_role(&ts.writer, &ts.shared.db, handles).await;
        assert!(ts.kb.lease_is_free());
        assert_eq!(ts.query_only(), 1);
        assert!(matches!(
            ts.writer.claim(Claimant::Rebuild),
            Err(LeaseError::Closed)
        ));
    }

    /// A start that fails before the watcher starts holds the janitor
    /// only: the shutdown stops it and releases the claim the start took.
    // Red if the shutdown leaves the janitor running, or keeps the lease or
    // the queued claim of the dropped start guard.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn shutdown_before_the_watcher_starts_releases_the_start_claim_and_stops_the_janitor() {
        let (mut ts, handles) = start_writer_for_test(
            TestKb::new("before-start"),
            StartOpts {
                skip_watcher: true,
                ..Default::default()
            },
        );
        assert!(handles.watcher.is_none());
        assert!(handles.watcher_stop.is_none());
        let janitor = handles
            .janitor
            .as_ref()
            .expect("the janitor runs")
            .abort_handle();
        let Some(StartRole::Writer(guard)) = ts.unstarted.take() else {
            panic!("the start must make a writer");
        };
        assert_eq!(ts.query_only(), 0);
        // Held so a janitor tick cannot settle the claim before it is seen
        // queued.
        let held = DbHolder::hold(Arc::clone(&ts.shared.db));
        drop(guard);
        assert!(ts.writer.has_pending());
        held.let_go();
        shutdown_writer_role(&ts.writer, &ts.shared.db, handles).await;
        assert_eq!(ts.query_only(), 1);
        assert!(ts.kb.lease_is_free());
        assert!(!ts.writer.has_pending());
        assert!(janitor.is_finished());
    }
}
