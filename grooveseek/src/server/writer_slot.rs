//! The writer slot: who inside one `groove serve` holds the writer lease, and
//! the `query_only` switch that follows it (ADR-0031).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use crate::db::Database;
use crate::poison::recover;
use crate::writer_lease::{HolderInfo, HolderRelation, LeaseError, Role, WriterLease};

/// The label a poisoned inner mutex is reported under.
const INNER: &str = "writer slot";

/// How a managed slot takes the lease: [`WriterLease::try_acquire`] in
/// production; a test injects a failure.
type AcquireFn = Box<dyn Fn(&Path, Role, &Path) -> Result<WriterLease, LeaseError> + Send + Sync>;

/// What a slot reports a watch error through: the watcher's bridge thread
/// calls it with the cause, or `None` once the watch is registered.
pub type WatchStatusFn = Arc<dyn Fn(Option<WatchErrorCause>) + Send + Sync>;

/// The writer lease inside one server: the claims that hold it (the watcher,
/// a rebuild), and the `query_only` switch of the shared connection. ADR-0031.
pub struct WriterSlot {
    mode: SlotMode,
    inner: Mutex<SlotInner>,
    /// Whether the lease is held, readable without the inner mutex.
    held: AtomicBool,
    /// Whether the refusal for an I/O failure on the lease was warned about.
    // Read by the rebuild refusal (a later task of #259 PR2).
    #[allow(dead_code)]
    io_warned: AtomicBool,
}

enum SlotMode {
    Managed {
        db_path: PathBuf,
        kb_path: PathBuf,
        model_id: String,
        dim: u32,
        acquire: AcquireFn,
    },
    /// For tests: always counts as holding, and never touches a pragma or a lease.
    Unmanaged,
}

/// The state behind the inner mutex.
#[derive(Default)]
struct SlotInner {
    lease: Option<WriterLease>,
    /// The watcher's mark: the generation of its live claim.
    watcher: Option<u64>,
    /// The rebuild's mark.
    rebuild: Option<u64>,
    next_gen: u64,
    /// The lease was taken anew and the embedding meta not yet checked.
    meta_unchecked: bool,
    /// Claims whose guard was dropped without `release` (a panic).
    pending_release: Vec<(Claimant, u64)>,
    closed: bool,
    reader: Option<ReaderReason>,
    watch_error: Option<WatchErrorCause>,
    /// Whether the current watcher has reported its watch registered.
    watch_reported: bool,
    /// The error the watcher's task ended with, by generation.
    watcher_exit: Option<(u64, String)>,
    watcher_stopped: Option<WatcherStopped>,
}

impl SlotInner {
    fn mark(&self, who: Claimant) -> Option<u64> {
        match who {
            Claimant::Watcher => self.watcher,
            Claimant::Rebuild => self.rebuild,
        }
    }

    fn mark_mut(&mut self, who: Claimant) -> &mut Option<u64> {
        match who {
            Claimant::Watcher => &mut self.watcher,
            Claimant::Rebuild => &mut self.rebuild,
        }
    }

    /// The role the marks call for: the watcher's makes it a server. ADR-0031.
    fn role(&self) -> Option<Role> {
        if self.watcher.is_some() {
            Some(Role::Serve)
        } else if self.rebuild.is_some() {
            Some(Role::Rebuild)
        } else {
            None
        }
    }

    fn no_marks(&self) -> bool {
        self.watcher.is_none() && self.rebuild.is_none()
    }

    /// Rewrite writer.json's role if the marks now call for another.
    fn sync_role(&mut self) {
        let role = self.role();
        if let (Some(lease), Some(role)) = (self.lease.as_mut(), role)
            && lease.role() != role
        {
            lease.set_role(role);
        }
    }

    /// Lower `who`'s mark if it is still `generation`; a lowered
    /// watcher's mark outside shutdown records the stop. ADR-0031.
    fn lower(&mut self, who: Claimant, generation: u64) -> bool {
        if self.mark(who) != Some(generation) {
            return false;
        }
        *self.mark_mut(who) = None;
        if who == Claimant::Watcher && !self.closed {
            let error = match self.watcher_exit.take() {
                Some((g, e)) if g == generation => Some(e),
                _ => None,
            };
            self.watcher_stopped = Some(WatcherStopped { error });
        }
        true
    }

    /// What writer.json's `watching` says now. ADR-0031.
    fn watching(&self) -> Option<bool> {
        let Some(generation) = self.watcher else {
            return Some(false);
        };
        if self
            .pending_release
            .contains(&(Claimant::Watcher, generation))
            || self.watcher_stopped.is_some()
            || self.watch_error.is_some()
        {
            Some(false)
        } else if !self.watch_reported {
            // Not reported yet: say nothing rather than claim a watch.
            None
        } else {
            Some(true)
        }
    }

    /// With no mark left, turn `query_only` on and drop the lease; if it
    /// cannot be turned on, keep the lease (a writable connection must not
    /// outlive it) and warn. Otherwise follow the marks' role.
    fn release_if_unmarked(&mut self, db: &Database) {
        if !self.no_marks() {
            self.sync_role();
            return;
        }
        if self.lease.is_none() {
            return;
        }
        match db.set_query_only(true) {
            Ok(()) => self.lease = None,
            Err(e) => tracing::warn!(
                error = %e,
                "failed to turn query_only back on; keeping the writer lease until this process exits"
            ),
        }
    }
}

/// Who claims the writer lease inside a server.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Claimant {
    Watcher,
    Rebuild,
}

impl Claimant {
    /// The name warnings use: `watcher` or `rebuild`.
    pub fn as_str(self) -> &'static str {
        match self {
            Claimant::Watcher => "watcher",
            Claimant::Rebuild => "rebuild",
        }
    }
}

/// Why the server's first claim did not take the lease. ADR-0031.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum ReaderReason {
    /// Another process held it. `holder_at_start` is writer.json then (display only).
    Busy {
        holder_at_start: Option<HolderInfo>,
        relation: HolderRelation,
    },
    /// The lock file could not be opened or locked.
    Io { lock_path: PathBuf, error: String },
}

/// Why the watcher is not watching: the cause only, worded by the reader of it.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct WatchErrorCause {
    pub kb: PathBuf,
    pub error: String,
    pub retry_secs: u64,
}

/// The watcher's claim went down outside shutdown; `error` is its
/// task's error, `None` after a panic.
pub struct WatcherStopped {
    // Read by the status of a later task of #259 PR2, and by the tests.
    #[cfg_attr(not(test), allow(dead_code))]
    error: Option<String>,
}

/// A live claim. `release` it with the database guard held; dropped without
/// that (a panic), it is queued for `settle_pending`.
pub struct ClaimGuard {
    slot: Arc<WriterSlot>,
    who: Claimant,
    generation: u64,
    released: bool,
}

impl ClaimGuard {
    /// The generation of this claim's mark.
    pub(crate) fn generation(&self) -> u64 {
        self.generation
    }
}

impl std::fmt::Debug for ClaimGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClaimGuard")
            .field("who", &self.who)
            .field("generation", &self.generation)
            .field("released", &self.released)
            .finish()
    }
}

impl Drop for ClaimGuard {
    fn drop(&mut self) {
        if self.released || matches!(self.slot.mode, SlotMode::Unmanaged) {
            return;
        }
        // No database here (I18): the janitor settles it under the db guard.
        // After shutdown every mark is gone and nothing settles the queue.
        let mut inner = recover(self.slot.inner.lock(), INNER);
        if !inner.closed {
            inner.pending_release.push((self.who, self.generation));
        }
    }
}

impl WriterSlot {
    /// A slot that takes the lease on `db_path` for the knowledge base at `kb_path`.
    pub fn managed(db_path: PathBuf, kb_path: PathBuf, model_id: String, dim: u32) -> Arc<Self> {
        Self::with_mode(SlotMode::Managed {
            db_path,
            kb_path,
            model_id,
            dim,
            acquire: Box::new(WriterLease::try_acquire),
        })
    }

    /// [`WriterSlot::managed`] with the lease's acquisition injected.
    #[cfg(test)]
    pub(crate) fn managed_with_acquire(
        db_path: PathBuf,
        kb_path: PathBuf,
        model_id: String,
        dim: u32,
        acquire: AcquireFn,
    ) -> Arc<Self> {
        Self::with_mode(SlotMode::Managed {
            db_path,
            kb_path,
            model_id,
            dim,
            acquire,
        })
    }

    /// A slot for tests that always counts as holding the lease.
    pub fn unmanaged() -> Arc<Self> {
        Self::with_mode(SlotMode::Unmanaged)
    }

    fn with_mode(mode: SlotMode) -> Arc<Self> {
        let held = matches!(mode, SlotMode::Unmanaged);
        Arc::new(Self {
            mode,
            inner: Mutex::new(SlotInner::default()),
            held: AtomicBool::new(held),
            io_warned: AtomicBool::new(false),
        })
    }

    fn lock(&self) -> MutexGuard<'_, SlotInner> {
        recover(self.inner.lock(), INNER)
    }

    fn sync_held(&self, inner: &SlotInner) {
        if matches!(self.mode, SlotMode::Managed { .. }) {
            self.held.store(inner.lease.is_some(), Ordering::Relaxed);
        }
    }

    /// Claim the lease for `who`, taking it if no claim holds it. Touches no
    /// database: [`WriterSlot::make_writable`] does, under the db guard.
    // `LeaseError::Busy` carries the holder inline so callers can match on it
    // directly; the error path runs once per command, so its size costs nothing.
    #[allow(clippy::result_large_err)]
    pub fn claim(self: &Arc<Self>, who: Claimant) -> Result<ClaimGuard, LeaseError> {
        // Declared before the inner guard so that it drops after it: if a
        // later step panics, its Drop queues the claim without deadlocking
        // on the inner mutex this function still holds.
        let made: ClaimGuard;
        let mut inner = self.lock();
        if inner.closed {
            return Err(LeaseError::Closed);
        }
        if inner.reader.is_some() {
            return Err(LeaseError::Reader);
        }
        let SlotMode::Managed { .. } = &self.mode else {
            inner.next_gen += 1;
            let generation = inner.next_gen;
            drop(inner);
            return Ok(self.guard(who, generation));
        };
        debug_assert!(
            inner
                .mark(who)
                .is_none_or(|g| inner.pending_release.contains(&(who, g))),
            "a second live {} claim",
            who.as_str()
        );
        if inner.lease.is_none() {
            let role = if who == Claimant::Watcher || inner.watcher.is_some() {
                Role::Serve
            } else {
                Role::Rebuild
            };
            inner.lease = Some(self.acquire(role)?);
            inner.meta_unchecked = true;
        }
        inner.next_gen += 1;
        let generation = inner.next_gen;
        *inner.mark_mut(who) = Some(generation);
        made = self.guard(who, generation);
        if who == Claimant::Watcher {
            inner.watch_reported = false;
        }
        inner.sync_role();
        self.sync_held(&inner);
        drop(inner);
        Ok(made)
    }

    fn guard(self: &Arc<Self>, who: Claimant, generation: u64) -> ClaimGuard {
        ClaimGuard {
            slot: Arc::clone(self),
            who,
            generation,
            released: false,
        }
    }

    // The same `LeaseError` as the lease's, for the reason given on `claim`.
    #[allow(clippy::result_large_err)]
    fn acquire(&self, role: Role) -> Result<WriterLease, LeaseError> {
        match &self.mode {
            SlotMode::Managed {
                db_path,
                kb_path,
                acquire,
                ..
            } => acquire(db_path, role, kb_path),
            SlotMode::Unmanaged => unreachable!("an unmanaged slot takes no lease"),
        }
    }

    /// The first claim found the lease held or failing: from now on every
    /// claim is [`LeaseError::Reader`].
    pub fn become_reader(&self, reason: ReaderReason) {
        self.lock().reader = Some(reason);
    }

    /// The watcher's bridge reports a watch error, or `None` once registered.
    /// Only the live watcher's generation is taken; a change of
    /// `watching` rewrites writer.json at once.
    pub fn set_watch_error(&self, watcher_gen: u64, cause: Option<WatchErrorCause>) {
        let mut inner = self.lock();
        if inner.watcher != Some(watcher_gen)
            || inner
                .pending_release
                .contains(&(Claimant::Watcher, watcher_gen))
        {
            return;
        }
        let before = inner.watching();
        if cause.is_none() {
            inner.watch_reported = true;
        }
        inner.watch_error = cause;
        let after = inner.watching();
        if before != after
            && !inner.closed
            && let Some(lease) = inner.lease.as_mut()
        {
            lease.touch_heartbeat(after);
        }
    }

    /// The watcher's status callback for `watcher_gen`, or `None` for an
    /// unmanaged slot.
    pub fn watch_status_callback(self: &Arc<Self>, watcher_gen: u64) -> Option<WatchStatusFn> {
        match self.mode {
            SlotMode::Managed { .. } => {
                let slot = Arc::clone(self);
                Some(Arc::new(move |cause| {
                    slot.set_watch_error(watcher_gen, cause)
                }))
            }
            SlotMode::Unmanaged => None,
        }
    }

    /// The watcher's task ended with `error`; called before its `release`.
    pub fn set_watcher_exit(&self, watcher_gen: u64, error: String) {
        let mut inner = self.lock();
        if inner.watcher == Some(watcher_gen) {
            inner.watcher_exit = Some((watcher_gen, error));
        }
    }

    /// Turn `query_only` off for `guard`'s claim, with the db guard held;
    /// a lease taken anew has its embedding meta checked first. Any
    /// `Err` has already released the claim: the caller does not `release` it.
    pub fn make_writable(&self, guard: &mut ClaimGuard, db: &Database) -> anyhow::Result<()> {
        let SlotMode::Managed {
            db_path,
            model_id,
            dim,
            ..
        } = &self.mode
        else {
            return Ok(());
        };
        // The inner mutex is not held across SQL (I45); the caller's db
        // guard keeps the lease from going meanwhile.
        let (resolved, meta_unchecked) = {
            let inner = self.lock();
            // A guard that outlived shutdown, or whose mark is gone, must not
            // turn the connection writable again. Its mark is not there to
            // lower, and `query_only` is left as it is for any live claim.
            if inner.closed {
                guard.released = true;
                return Err(LeaseError::Closed.into());
            }
            if inner.mark(guard.who) != Some(guard.generation()) {
                guard.released = true;
                return Err(anyhow::anyhow!(
                    "this {} claim on the writer lease is no longer live",
                    guard.who.as_str()
                ));
            }
            match inner.lease.as_ref() {
                Some(lease) => (lease.resolved_path().to_path_buf(), inner.meta_unchecked),
                None => {
                    drop(inner);
                    return Err(self.abandon(
                        guard,
                        db,
                        anyhow::anyhow!("writer lease is not held"),
                    ));
                }
            }
        };
        if !db.is_on(&resolved) {
            let e = anyhow::anyhow!(
                "the index at {} now resolves to {}, not the file this server opened; restart the server",
                db_path.display(),
                resolved.display()
            );
            return Err(self.abandon(guard, db, e));
        }
        if let Err(e) = db.set_query_only(false) {
            return Err(self.abandon(guard, db, e));
        }
        if meta_unchecked && let Err(e) = db.verify_embedding_meta(model_id, *dim) {
            return Err(self.abandon(guard, db, e));
        }
        if meta_unchecked {
            self.lock().meta_unchecked = false;
        }
        Ok(())
    }

    /// The cleanup of a failed [`WriterSlot::make_writable`]: `query_only`
    /// on, the claim's mark lowered, the lease dropped if no claim is left,
    /// and the guard marked released. Returns `e`.
    fn abandon(&self, guard: &mut ClaimGuard, db: &Database, e: anyhow::Error) -> anyhow::Error {
        let on = db.set_query_only(true);
        let mut inner = self.lock();
        let _ = inner.lower(guard.who, guard.generation());
        guard.released = true;
        if inner.no_marks() && inner.lease.is_some() {
            match on {
                Ok(()) => inner.lease = None,
                Err(on_err) => tracing::warn!(
                    error = %on_err,
                    "failed to turn query_only back on; keeping the writer lease until this process exits"
                ),
            }
        } else {
            inner.sync_role();
        }
        self.sync_held(&inner);
        e
    }

    /// Release `guard`'s claim, with the db guard held: lower its mark if it
    /// is still this claim's; with no claim left, `query_only` on, then
    /// the lease dropped (ADR-0031).
    pub fn release(&self, mut guard: ClaimGuard, db: &Database) {
        guard.released = true;
        if matches!(self.mode, SlotMode::Unmanaged) {
            return;
        }
        let mut inner = self.lock();
        if inner.lower(guard.who, guard.generation()) {
            inner.release_if_unmarked(db);
        }
        self.sync_held(&inner);
    }

    /// Settle the claims a panic left queued, as `release` would, with the
    /// db guard held. Returns who was released; a queued claim whose mark has
    /// a newer generation is only discarded.
    pub fn settle_pending(&self, db: &Database) -> Vec<Claimant> {
        let mut inner = self.lock();
        let pending = std::mem::take(&mut inner.pending_release);
        let mut settled = Vec::new();
        for (who, generation) in pending {
            if inner.lower(who, generation) {
                settled.push(who);
            }
        }
        if !settled.is_empty() {
            inner.release_if_unmarked(db);
        }
        self.sync_held(&inner);
        settled
    }

    /// Whether a panicked claim waits for [`WriterSlot::settle_pending`].
    pub fn has_pending(&self) -> bool {
        !self.lock().pending_release.is_empty()
    }

    /// The first step of shutdown: later claims are `Closed`, and the
    /// watcher's release no longer records a stop.
    pub fn begin_shutdown(&self) {
        self.lock().closed = true;
    }

    /// Shut down with the db guard held: `query_only` on, every mark
    /// and queued claim discarded, the lease dropped. If `query_only` cannot
    /// be turned on, the lease is kept until the process exits.
    pub fn shutdown(&self, db: &Database) {
        let mut inner = self.lock();
        inner.closed = true;
        inner.watcher = None;
        inner.rebuild = None;
        inner.pending_release.clear();
        if matches!(self.mode, SlotMode::Managed { .. }) {
            inner.release_if_unmarked(db);
        }
        self.sync_held(&inner);
    }

    /// Rewrite writer.json's heartbeat and `watching`, only while the lease
    /// is held and the slot is not shutting down. Takes the inner mutex only.
    pub fn heartbeat(&self) {
        let mut inner = self.lock();
        if inner.closed {
            return;
        }
        let watching = inner.watching();
        if let Some(lease) = inner.lease.as_mut() {
            lease.touch_heartbeat(watching);
        }
    }

    /// Whether the lease is held (an unmanaged slot always counts as holding).
    pub fn is_held(&self) -> bool {
        self.held.load(Ordering::Relaxed)
    }

    /// Whether this server started as a reader.
    pub fn is_reader(&self) -> bool {
        self.lock().reader.is_some()
    }

    /// Whether the watcher's stop is recorded, and its error: `None` = not
    /// recorded, `Some(None)` = recorded without an error (a panic).
    #[cfg(test)]
    pub(crate) fn watcher_stopped_error(&self) -> Option<Option<String>> {
        self.lock()
            .watcher_stopped
            .as_ref()
            .map(|w| w.error.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::writer_lease::{HolderRelation, LeaseError, Role, WriterLease};
    use std::path::{Path, PathBuf};
    use std::time::SystemTime;

    const MODEL: &str = "test-model";
    const DIM: u32 = 4;

    /// A scratch directory under the temp dir, removed on drop (no `tempfile`).
    struct TempDir(PathBuf);
    impl TempDir {
        fn new(prefix: &str) -> Self {
            let p = crate::test_support::unique_temp_path(&format!("groove-slot-{prefix}"));
            std::fs::create_dir_all(&p).unwrap();
            Self(p)
        }
        fn path(&self) -> &Path {
            &self.0
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// A temp dir with `kb/` and `.groove.db` (meta recorded as `model`), and a
    /// reader connection on it (`open_read_only`).
    struct Fixture {
        _dir: TempDir,
        db_path: PathBuf,
        kb: PathBuf,
        db: Database,
    }

    fn fixture_with_model(prefix: &str, model: &str) -> Fixture {
        let dir = TempDir::new(prefix);
        let kb = dir.path().join("kb");
        std::fs::create_dir_all(&kb).unwrap();
        let db_path = dir.path().join(".groove.db");
        {
            let w = Database::open(db_path.to_str().unwrap()).unwrap();
            w.verify_embedding_meta(model, DIM).unwrap();
        }
        let db = Database::open_read_only(db_path.to_str().unwrap(), &kb).unwrap();
        Fixture {
            _dir: dir,
            db_path,
            kb,
            db,
        }
    }

    fn fixture(prefix: &str) -> Fixture {
        fixture_with_model(prefix, MODEL)
    }

    impl Fixture {
        fn slot(&self) -> Arc<WriterSlot> {
            WriterSlot::managed(
                self.db_path.clone(),
                self.kb.clone(),
                MODEL.to_string(),
                DIM,
            )
        }
        fn query_only(&self) -> i64 {
            self.db.query_only_for_test()
        }
        /// Whether another handle can take the lease now (it is dropped at once).
        fn lease_is_free(&self) -> bool {
            match WriterLease::try_acquire(&self.db_path, Role::Index, &self.kb) {
                Ok(_) => true,
                Err(LeaseError::Busy { .. }) => false,
                Err(e) => panic!("unexpected lease error: {e}"),
            }
        }
        /// The invariant T-L7 checks after every step: a writable connection
        /// implies the lease is held.
        fn assert_off_implies_held(&self, step: &str) {
            if self.query_only() == 0 {
                assert!(
                    !self.lease_is_free(),
                    "{step}: query_only is off without the lease"
                );
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
        fn info_bytes(&self) -> Vec<u8> {
            std::fs::read(WriterLease::info_path_for(&self.db_path)).unwrap()
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

    #[test]
    fn query_only_off_implies_lease_in_every_claim_release_order() {
        use Claimant::{Rebuild as R, Watcher as W};
        // Each order: claim first, claim second, release the third, release the fourth.
        let orders = [(W, R, W, R), (W, R, R, W), (R, W, W, R), (R, W, R, W)];
        for (i, (a, b, first_out, second_out)) in orders.into_iter().enumerate() {
            let f = fixture(&format!("order{i}"));
            let slot = f.slot();
            let step = |s: &str| format!("order {i} ({a:?},{b:?}): {s}");

            let mut ga = slot.claim(a).unwrap();
            f.assert_off_implies_held(&step("claim a"));
            slot.make_writable(&mut ga, &f.db).unwrap();
            assert_eq!(f.query_only(), 0);
            f.assert_off_implies_held(&step("make_writable a"));
            let mut gb = slot.claim(b).unwrap();
            f.assert_off_implies_held(&step("claim b"));
            slot.make_writable(&mut gb, &f.db).unwrap();
            f.assert_off_implies_held(&step("make_writable b"));

            let (g1, g2) = if first_out == a { (ga, gb) } else { (gb, ga) };
            assert_eq!(g1.who, first_out);
            assert_eq!(g2.who, second_out);
            slot.release(g1, &f.db);
            f.assert_off_implies_held(&step("release first"));
            assert_eq!(f.query_only(), 0, "{}", step("one claim is left"));
            assert!(!f.lease_is_free(), "{}", step("one claim is left"));
            slot.release(g2, &f.db);
            f.assert_off_implies_held(&step("release second"));

            assert_eq!(f.query_only(), 1, "{}", step("both released"));
            assert!(f.lease_is_free(), "{}", step("both released"));
            assert!(!slot.is_held());
        }
    }

    #[test]
    fn a_panicked_claim_is_settled_by_settle_pending() {
        // Rebuild alone.
        let f = fixture("panic-alone");
        let slot = f.slot();
        let mut g = slot.claim(Claimant::Rebuild).unwrap();
        slot.make_writable(&mut g, &f.db).unwrap();
        panic_with(g);
        assert!(!f.lease_is_free(), "the lease stays until settled");
        assert!(slot.has_pending());
        assert_eq!(slot.settle_pending(&f.db), vec![Claimant::Rebuild]);
        assert!(!slot.has_pending());
        assert_eq!(f.query_only(), 1);
        assert!(f.lease_is_free());
        drop(f);

        // With the watcher's claim still up, the lease stays.
        let f = fixture("panic-with-watcher");
        let slot = f.slot();
        let mut w = slot.claim(Claimant::Watcher).unwrap();
        slot.make_writable(&mut w, &f.db).unwrap();
        let mut r = slot.claim(Claimant::Rebuild).unwrap();
        slot.make_writable(&mut r, &f.db).unwrap();
        panic_with(r);
        assert_eq!(slot.settle_pending(&f.db), vec![Claimant::Rebuild]);
        assert!(!f.lease_is_free());
        f.assert_off_implies_held("after settling the rebuild");
        slot.release(w, &f.db);
        assert_eq!(f.query_only(), 1);
        assert!(f.lease_is_free());
    }

    #[test]
    fn settle_pending_skips_a_mark_of_a_newer_generation() {
        let f = fixture("generation");
        let slot = f.slot();
        let mut r1 = slot.claim(Claimant::Rebuild).unwrap();
        slot.make_writable(&mut r1, &f.db).unwrap();
        panic_with(r1);
        let mut r2 = slot.claim(Claimant::Rebuild).unwrap();
        slot.make_writable(&mut r2, &f.db).unwrap();
        assert_eq!(slot.settle_pending(&f.db), Vec::<Claimant>::new());
        assert!(!slot.has_pending());
        assert_eq!(f.query_only(), 0);
        assert!(!f.lease_is_free());
        slot.release(r2, &f.db);
        assert_eq!(f.query_only(), 1);
        assert!(f.lease_is_free());
    }

    #[test]
    fn the_role_in_writer_json_follows_the_marks() {
        let f = fixture("role-rw");
        let slot = f.slot();
        let r = slot.claim(Claimant::Rebuild).unwrap();
        assert_eq!(f.holder().map(|h| h.role), Some(Role::Rebuild));
        let w = slot.claim(Claimant::Watcher).unwrap();
        assert_eq!(f.holder().map(|h| h.role), Some(Role::Serve));
        slot.release(r, &f.db);
        assert_eq!(f.holder().map(|h| h.role), Some(Role::Serve));
        slot.release(w, &f.db);
        assert_eq!(f.holder(), None);
        drop(f);

        let f = fixture("role-wr");
        let slot = f.slot();
        let w = slot.claim(Claimant::Watcher).unwrap();
        assert_eq!(f.holder().map(|h| h.role), Some(Role::Serve));
        let r = slot.claim(Claimant::Rebuild).unwrap();
        assert_eq!(f.holder().map(|h| h.role), Some(Role::Serve));
        slot.release(w, &f.db);
        assert_eq!(f.holder().map(|h| h.role), Some(Role::Rebuild));
        slot.release(r, &f.db);
    }

    #[test]
    fn shutdown_turns_query_only_on_and_drops_the_lease_and_refuses_later_claims() {
        let f = fixture("shutdown");
        let slot = f.slot();
        let mut w = slot.claim(Claimant::Watcher).unwrap();
        slot.make_writable(&mut w, &f.db).unwrap();
        let mut r = slot.claim(Claimant::Rebuild).unwrap();
        slot.make_writable(&mut r, &f.db).unwrap();
        slot.shutdown(&f.db);
        assert_eq!(f.query_only(), 1);
        assert!(f.lease_is_free());
        assert!(matches!(
            slot.claim(Claimant::Rebuild),
            Err(LeaseError::Closed)
        ));
        assert!(!slot.is_held());
        // The other handle is gone again; the slot did not take the lease back.
        assert!(f.lease_is_free());
        drop((w, r));
        assert!(f.lease_is_free());
        assert!(!slot.has_pending());
    }

    #[test]
    fn a_guard_that_outlived_shutdown_cannot_make_the_connection_writable() {
        let f = fixture("after-shutdown");
        let slot = f.slot();
        let mut w = slot.claim(Claimant::Watcher).unwrap();
        slot.shutdown(&f.db);
        let err = slot.make_writable(&mut w, &f.db).unwrap_err();
        assert!(
            matches!(err.downcast_ref::<LeaseError>(), Some(LeaseError::Closed)),
            "{err:#}"
        );
        assert_eq!(f.query_only(), 1);
        assert!(f.lease_is_free());
        drop(w);
        assert!(!slot.has_pending());
    }

    #[test]
    fn a_failed_make_writable_of_the_watcher_records_a_stopped_watcher() {
        let f = fixture_with_model("watcher-meta", "other-model");
        let slot = f.slot();
        let mut w = slot.claim(Claimant::Watcher).unwrap();
        slot.make_writable(&mut w, &f.db).unwrap_err();
        assert_eq!(slot.watcher_stopped_error(), Some(None));
        assert_eq!(f.query_only(), 1);
        assert!(f.lease_is_free());
    }

    #[test]
    fn a_meta_mismatch_on_a_fresh_claim_releases_and_does_not_queue_a_pending() {
        let f = fixture_with_model("meta", "other-model");
        let slot = f.slot();
        let mut g = slot.claim(Claimant::Rebuild).unwrap();
        let err = slot.make_writable(&mut g, &f.db).unwrap_err();
        assert!(
            format!("{err:#}").contains("embedding model mismatch"),
            "{err:#}"
        );
        assert_eq!(f.query_only(), 1);
        assert!(f.lease_is_free());
        drop(g);
        assert!(!slot.has_pending());
        assert!(!slot.is_held());
    }

    #[test]
    fn make_writable_refuses_when_the_connection_is_not_on_the_leased_file() {
        let f = fixture("not-on");
        let other_dir = f._dir.path().join("other");
        std::fs::create_dir_all(&other_dir).unwrap();
        let other_db = other_dir.join(".groove.db");
        let slot = WriterSlot::managed(other_db.clone(), f.kb.clone(), MODEL.to_string(), DIM);
        let mut g = slot.claim(Claimant::Rebuild).unwrap();
        let err = slot.make_writable(&mut g, &f.db).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("not the file this server opened"), "{msg}");
        assert!(msg.is_ascii(), "{msg}");
        assert_eq!(f.query_only(), 1);
        assert!(
            WriterLease::try_acquire(&other_db, Role::Index, &f.kb).is_ok(),
            "the lease on the other file is released"
        );
        drop(g);
        assert!(!slot.has_pending());
    }

    #[test]
    fn a_stopped_watcher_is_recorded_unless_it_was_shutdown() {
        // The watcher's task ended with an error.
        let f = fixture("stopped");
        let slot = f.slot();
        let w = slot.claim(Claimant::Watcher).unwrap();
        slot.set_watcher_exit(w.generation(), "boom".to_string());
        slot.release(w, &f.db);
        assert_eq!(slot.watcher_stopped_error(), Some(Some("boom".to_string())));
        assert!(f.lease_is_free());

        // A panic: the guard is settled by the janitor.
        let slot = f.slot();
        let w = slot.claim(Claimant::Watcher).unwrap();
        panic_with(w);
        assert_eq!(slot.watcher_stopped_error(), None);
        assert_eq!(slot.settle_pending(&f.db), vec![Claimant::Watcher]);
        assert_eq!(slot.watcher_stopped_error(), Some(None));
        assert!(f.lease_is_free());

        // Shutdown is not a stop.
        let slot = f.slot();
        let w = slot.claim(Claimant::Watcher).unwrap();
        slot.shutdown(&f.db);
        slot.release(w, &f.db);
        assert_eq!(slot.watcher_stopped_error(), None);

        // begin_shutdown, then the task's release: not a stop either.
        let slot = f.slot();
        let w = slot.claim(Claimant::Watcher).unwrap();
        slot.begin_shutdown();
        slot.release(w, &f.db);
        assert_eq!(slot.watcher_stopped_error(), None);
        slot.shutdown(&f.db);
        assert!(f.lease_is_free());

        // An exit of another generation leaves nothing.
        let slot = f.slot();
        let w = slot.claim(Claimant::Watcher).unwrap();
        slot.set_watcher_exit(w.generation() + 1, "stale".to_string());
        slot.release(w, &f.db);
        assert_eq!(slot.watcher_stopped_error(), Some(None));
    }

    #[test]
    fn heartbeat_rewrites_only_while_the_lease_is_held() {
        // 1: held, then the watcher reports its registration.
        let f = fixture("hb");
        let slot = f.slot();
        let w = slot.claim(Claimant::Watcher).unwrap();
        f.age_the_heartbeat();
        assert!(!f.holder().unwrap().heartbeat_fresh(SystemTime::now()));
        slot.heartbeat();
        let h = f.holder().unwrap();
        assert!(h.heartbeat_fresh(SystemTime::now()));
        assert_eq!(h.watching, None, "not reported yet");
        slot.set_watch_error(w.generation(), None);
        f.age_the_heartbeat();
        slot.heartbeat();
        let h = f.holder().unwrap();
        assert!(h.heartbeat_fresh(SystemTime::now()));
        assert_eq!(h.watching, Some(true));
        slot.release(w, &f.db);

        // 2: released.
        let slot = f.slot();
        let g = slot.claim(Claimant::Rebuild).unwrap();
        slot.release(g, &f.db);
        slot.heartbeat();
        assert_eq!(f.holder(), None);

        // 3: shut down.
        let slot = f.slot();
        let _g = slot.claim(Claimant::Rebuild).unwrap();
        slot.shutdown(&f.db);
        slot.heartbeat();
        assert_eq!(f.holder(), None);

        // 4: shutting down, the lease still held.
        let slot = f.slot();
        let g = slot.claim(Claimant::Rebuild).unwrap();
        f.age_the_heartbeat();
        let before = f.info_bytes();
        slot.begin_shutdown();
        slot.heartbeat();
        assert_eq!(f.info_bytes(), before);
        slot.shutdown(&f.db);
        drop(g);
    }

    #[test]
    fn watching_follows_the_watch_error_without_waiting_for_a_heartbeat() {
        let f = fixture("watching");
        let slot = f.slot();
        let w = slot.claim(Claimant::Watcher).unwrap();
        let cause = WatchErrorCause {
            kb: f.kb.clone(),
            error: "no such directory".to_string(),
            retry_secs: 1,
        };
        slot.set_watch_error(w.generation(), Some(cause.clone()));
        assert_eq!(f.holder().unwrap().watching, Some(false));
        slot.set_watch_error(w.generation(), None);
        assert_eq!(f.holder().unwrap().watching, Some(true));

        // Another generation's report changes nothing.
        let before = f.info_bytes();
        slot.set_watch_error(w.generation() + 1, Some(cause));
        assert_eq!(f.info_bytes(), before);
        slot.release(w, &f.db);

        // A rebuild alone is not watching.
        let slot = f.slot();
        let r = slot.claim(Claimant::Rebuild).unwrap();
        slot.heartbeat();
        assert_eq!(f.holder().unwrap().watching, Some(false));
        slot.release(r, &f.db);
    }

    #[test]
    fn a_reader_slot_refuses_every_claim_without_touching_the_lease() {
        let f = fixture("reader");
        let slot = f.slot();
        slot.become_reader(ReaderReason::Busy {
            holder_at_start: None,
            relation: HolderRelation::Unknown,
        });
        assert!(slot.is_reader());
        for who in [Claimant::Rebuild, Claimant::Watcher] {
            assert!(matches!(slot.claim(who), Err(LeaseError::Reader)));
        }
        assert!(f.lease_is_free());
        assert!(!slot.is_held());
    }

    #[test]
    fn shutdown_discards_pending_marks() {
        let f = fixture("shutdown-pending");
        let slot = f.slot();
        let mut w = slot.claim(Claimant::Watcher).unwrap();
        slot.make_writable(&mut w, &f.db).unwrap();
        let mut r = slot.claim(Claimant::Rebuild).unwrap();
        slot.make_writable(&mut r, &f.db).unwrap();
        panic_with(r);
        assert!(slot.has_pending());
        slot.shutdown(&f.db);
        assert!(!slot.has_pending());
        assert_eq!(f.query_only(), 1);
        assert!(f.lease_is_free());
        slot.release(w, &f.db);
        assert!(f.lease_is_free());
        assert_eq!(f.query_only(), 1);
    }

    #[test]
    // `LeaseError::Busy` carries the holder inline so callers can match on it
    // directly; the error path runs once per command, so its size costs nothing.
    #[allow(clippy::result_large_err)]
    fn a_failed_acquire_is_returned_as_it_is_and_raises_no_mark() {
        let f = fixture("acquire-io");
        let slot = WriterSlot::managed_with_acquire(
            f.db_path.clone(),
            f.kb.clone(),
            MODEL.to_string(),
            DIM,
            Box::new(|db, _, _| {
                Err(LeaseError::Io {
                    lock_path: WriterLease::lock_path_for(db),
                    source: std::io::Error::other("injected"),
                })
            }),
        );
        assert!(matches!(
            slot.claim(Claimant::Rebuild),
            Err(LeaseError::Io { .. })
        ));
        assert!(!slot.has_pending());
        assert!(!slot.is_held());
        assert_eq!(slot.settle_pending(&f.db), Vec::<Claimant>::new());
        assert!(f.lease_is_free());
    }

    #[test]
    fn an_unmanaged_slot_claims_without_a_lease_or_pragma() {
        let dir = TempDir::new("unmanaged");
        let kb = dir.path().join("kb");
        std::fs::create_dir_all(&kb).unwrap();
        let db_path = dir.path().join(".groove.db");
        let db = Database::open(db_path.to_str().unwrap()).unwrap();
        let slot = WriterSlot::unmanaged();
        let mut g = slot.claim(Claimant::Rebuild).unwrap();
        slot.make_writable(&mut g, &db).unwrap();
        assert_eq!(db.query_only_for_test(), 0);
        assert!(WriterLease::try_acquire(&db_path, Role::Index, &kb).is_ok());
        slot.release(g, &db);
        assert_eq!(db.query_only_for_test(), 0);
        assert!(slot.watch_status_callback(1).is_none());
    }
}
