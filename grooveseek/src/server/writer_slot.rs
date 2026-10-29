//! The writer slot: who inside one `groove serve` holds the writer lease, and
//! the `query_only` switch that follows it (ADR-0031).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::SystemTime;

use crate::db::Database;
use crate::poison::recover;
use crate::writer_lease::{
    HEARTBEAT_FRESH, HolderInfo, HolderRelation, LeaseError, Role, WriterLease, holder_relation,
};

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
    error: Option<String>,
}

/// What admin status reports about the writer lease (ADR-0031). An
/// unmanaged slot reports `held: false` and nothing else.
#[derive(Debug, Clone, serde::Serialize)]
#[non_exhaustive]
pub struct WriterStatus {
    pub held: bool,
    /// The role writer.json names, while the lease is held.
    pub role: Option<String>,
    /// When the lease was taken (RFC3339), while it is held.
    pub held_since: Option<String>,
    pub reader: bool,
    pub reader_notice: Option<String>,
    /// Kept after the watcher stopped, as it was when it stopped.
    pub watch_error: Option<String>,
    pub watcher_stopped: Option<String>,
}

/// The state the notices are worded from, copied out of the inner mutex.
struct NoticeState {
    reader: Option<ReaderReason>,
    /// `Some(error)` once the watcher stopped; see [`WatcherStopped`].
    stopped: Option<Option<String>>,
    watch_error: Option<WatchErrorCause>,
    lease: Option<(Role, SystemTime)>,
}

/// The three notices a slot can carry, worded once for both
/// [`WriterSlot::notice`] and [`WriterSlot::status`].
struct Notices {
    reader: Option<String>,
    watcher_stopped: Option<String>,
    watch_error: Option<String>,
}

/// `<db>.writer.json` as the notices name it.
fn info_display(db: &Path) -> String {
    WriterLease::info_path_for(db).display().to_string()
}

/// The notice of a reader server: why it does not update the index, who
/// kept it then, and what writer.json says now (ADR-0031). It never names
/// `rebuild_index`: a reader refuses it.
fn reader_notice_text(
    db: &Path,
    own_kb: &Path,
    reason: &ReaderReason,
    holder_now: Option<&HolderInfo>,
    now: SystemTime,
) -> String {
    match reason {
        ReaderReason::Busy {
            holder_at_start,
            relation,
        } => {
            let info = info_display(db);
            let at_start = match holder_at_start {
                Some(h) => format!(
                    "pid {} per {info}, which can be stale; {}, kb {}",
                    h.pid,
                    h.role.as_str(),
                    h.kb_path
                ),
                None => format!("no {info} was readable"),
            };
            let keeper = match relation {
                HolderRelation::WatchingSameKb => "That process is a server for this same knowledge base: while it runs and its watcher is watching files, it keeps the index current; after it exits, new, edited and deleted files are not reflected until this server is restarted.".to_string(),
                HolderRelation::OneOffRun(role) => format!(
                    "That process was a one-off {} run that does not watch files, so new, edited and deleted files are not reflected until this server is restarted, even while it runs.",
                    role.as_str()
                ),
                HolderRelation::OtherKb(kb) => format!(
                    "That process works on a different knowledge base (kb {kb}) that shares this index file, so nothing keeps this server's knowledge base (kb {}) current: new, edited and deleted files in it are not reflected until that process exits and this server is restarted.",
                    own_kb.display()
                ),
                HolderRelation::Unknown => "Whether that process keeps this knowledge base current cannot be told, so treat new, edited and deleted files as not reflected until this server is restarted.".to_string(),
            };
            let now_text = match holder_now {
                None => format!("Now there is no {info}, so that process has probably exited."),
                Some(h) => match holder_relation(Some(h), own_kb) {
                    // Staleness first: a killed server leaves a `serve` json behind.
                    HolderRelation::WatchingSameKb if !h.heartbeat_fresh(now) => format!(
                        "Now {info} names pid {}, a server watching this same knowledge base (since {}), but its heartbeat is older than {}s, so that process has probably exited without removing the file; treat new, edited and deleted files as not reflected until this server is restarted.",
                        h.pid,
                        h.started_at,
                        HEARTBEAT_FRESH.as_secs()
                    ),
                    HolderRelation::WatchingSameKb if h.keeps_current(now) => format!(
                        "Now {info} names pid {}, a server watching this same knowledge base (since {}): while it runs, it keeps the index current, so searches here see its changes and this server does not need a restart for that. The file can be stale: a process that was killed leaves it behind.",
                        h.pid, h.started_at
                    ),
                    HolderRelation::WatchingSameKb => format!(
                        "Now {info} names pid {}, a server for this same knowledge base (since {}), but it does not report that its watcher is watching files right now (for example, it could not register a watch and is retrying), so new, edited and deleted files are not being indexed while that lasts. The file can be stale: a process that was killed leaves it behind.",
                        h.pid, h.started_at
                    ),
                    _ => format!(
                        "Now {info} names pid {} ({}, since {}), which can be stale: a process that was killed leaves the file behind.",
                        h.pid,
                        h.role.as_str(),
                        h.started_at
                    ),
                },
            };
            format!(
                "this server is not updating the index at {}: when it started, another groove process held the writer lock ({at_start}). Searches still work and return what has been indexed. {keeper} This server cannot rebuild the index itself. After a restart while no other process holds the lock, it writes the index and watches files, but changes made before the restart need one rebuild after the restart. {now_text}",
                db.display()
            )
        }
        ReaderReason::Io { lock_path, error } => format!(
            "this server is not updating the index at {}: it could not take the writer lock at {} ({error}) when it started. Searches still work and return what has been indexed. New, edited and deleted files are not reflected until the lock works and this server is restarted. This server cannot rebuild the index itself.",
            db.display(),
            lock_path.display()
        ),
    }
}

/// The notice of a server whose watcher stopped outside shutdown, kept
/// until restart (ADR-0031). It names another server that keeps the index
/// current only while this slot holds no lease and that server reports a
/// running watcher.
fn watcher_stopped_text(
    db: &Path,
    own_kb: &Path,
    error: Option<&str>,
    lease_held: bool,
    holder_now: Option<&HolderInfo>,
    now: SystemTime,
) -> String {
    let why = error.unwrap_or("the watcher task ended without finishing");
    let mut text = format!(
        "this server stopped watching files for {} ({why}); new, edited and deleted files are not reflected until this server is restarted.",
        own_kb.display()
    );
    if let Some(h) = holder_now
        && !lease_held
        && h.keeps_current(now)
        && holder_relation(Some(h), own_kb) == HolderRelation::WatchingSameKb
    {
        text.push_str(&format!(
            " Now {} names pid {}, another server watching this same knowledge base (since {}): while it runs, it keeps the index current, so searches here see its changes. The file can be stale: a process that was killed leaves it behind.",
            info_display(db),
            h.pid,
            h.started_at
        ));
    }
    text
}

/// The notice of a writer whose watcher cannot register its watch (ADR-0031).
fn watch_error_text(cause: &WatchErrorCause) -> String {
    format!(
        "the watcher could not watch {}: {} (next retry in {}s); new, edited and deleted files are not being indexed until it succeeds. Fix the cause (on Linux, for example, raise fs.inotify.max_user_watches), or call rebuild_index to index what changed meanwhile.",
        cause.kb.display(),
        cause.error,
        cause.retry_secs
    )
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

    /// What admin status shows about the lease and its notices (ADR-0031).
    pub fn status(&self) -> WriterStatus {
        let Some((state, notices)) = self.notices() else {
            return WriterStatus {
                held: false,
                role: None,
                held_since: None,
                reader: false,
                reader_notice: None,
                watch_error: None,
                watcher_stopped: None,
            };
        };
        WriterStatus {
            held: state.lease.is_some(),
            role: state.lease.map(|(role, _)| role.as_str().to_string()),
            held_since: state
                .lease
                .map(|(_, at)| chrono::DateTime::<chrono::Utc>::from(at).to_rfc3339()),
            reader: state.reader.is_some(),
            reader_notice: notices.reader,
            watch_error: notices.watch_error,
            watcher_stopped: notices.watcher_stopped,
        }
    }

    /// The one notice a `search` or `rebuild_index` response carries: the
    /// reader's, else the stopped watcher's, else the watch error's
    /// (ADR-0031). A stopped watcher comes before its last watch error,
    /// which admin status still keeps.
    pub fn notice(&self) -> Option<String> {
        let (_, n) = self.notices()?;
        n.reader.or(n.watcher_stopped).or(n.watch_error)
    }

    /// Copy the state out of the inner mutex, then word every notice with it
    /// released: writer.json is read outside the mutex, and only when a
    /// notice names its holder. `None` for an unmanaged slot.
    fn notices(&self) -> Option<(NoticeState, Notices)> {
        let SlotMode::Managed {
            db_path, kb_path, ..
        } = &self.mode
        else {
            return None;
        };
        let state = {
            let inner = self.lock();
            NoticeState {
                reader: inner.reader.clone(),
                stopped: inner.watcher_stopped.as_ref().map(|w| w.error.clone()),
                watch_error: inner.watch_error.clone(),
                lease: inner.lease.as_ref().map(|l| (l.role(), l.acquired_at())),
            }
        };
        let needs_holder =
            matches!(state.reader, Some(ReaderReason::Busy { .. })) || state.stopped.is_some();
        let holder_now = if needs_holder {
            WriterLease::read_holder(db_path)
        } else {
            None
        };
        let now = SystemTime::now();
        let notices = Notices {
            reader: state
                .reader
                .as_ref()
                .map(|r| reader_notice_text(db_path, kb_path, r, holder_now.as_ref(), now)),
            watcher_stopped: state.stopped.as_ref().map(|error| {
                watcher_stopped_text(
                    db_path,
                    kb_path,
                    error.as_deref(),
                    state.lease.is_some(),
                    holder_now.as_ref(),
                    now,
                )
            }),
            watch_error: state.watch_error.as_ref().map(watch_error_text),
        };
        Some((state, notices))
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

    // --- The notice texts (ADR-0031) ---

    /// A temp dir with `kb/`; the db path beside it need not exist for the texts.
    struct TextLayout {
        dir: TempDir,
        kb: PathBuf,
        db: PathBuf,
    }

    impl TextLayout {
        fn new(prefix: &str) -> Self {
            let dir = TempDir::new(prefix);
            let kb = dir.path().join("kb");
            std::fs::create_dir_all(&kb).unwrap();
            let db = dir.path().join(".groove.db");
            Self { dir, kb, db }
        }
        fn canonical_kb(&self) -> String {
            std::fs::canonicalize(&self.kb)
                .unwrap()
                .display()
                .to_string()
        }
        /// A sibling of `kb/` that exists, canonicalized.
        fn other_kb(&self) -> String {
            let other = self.dir.path().join("other");
            std::fs::create_dir_all(&other).unwrap();
            std::fs::canonicalize(&other).unwrap().display().to_string()
        }
    }

    /// A writer.json as a holder writes it; `age` is how old its heartbeat is
    /// (`None` = no heartbeat field).
    fn holder_info(
        role: Role,
        kb: &str,
        age: Option<i64>,
        watching: Option<bool>,
    ) -> crate::writer_lease::HolderInfo {
        crate::writer_lease::HolderInfo {
            pid: 4242,
            role,
            started_at: "2026-01-01T00:00:00+00:00".to_string(),
            version: "0.0.1".to_string(),
            kb_path: kb.to_string(),
            heartbeat_at: age
                .map(|s| (chrono::Utc::now() - chrono::Duration::seconds(s)).to_rfc3339()),
            watching,
        }
    }

    fn busy(
        at_start: Option<&crate::writer_lease::HolderInfo>,
        relation: HolderRelation,
    ) -> ReaderReason {
        ReaderReason::Busy {
            holder_at_start: at_start.cloned(),
            relation,
        }
    }

    #[test]
    fn reader_notice_for_a_server_on_this_kb_says_it_keeps_the_index_current() {
        let l = TextLayout::new("txt-same");
        let h = holder_info(Role::Serve, &l.canonical_kb(), Some(0), Some(true));
        let reason = busy(Some(&h), HolderRelation::WatchingSameKb);
        let t = reader_notice_text(&l.db, &l.kb, &reason, Some(&h), SystemTime::now());
        assert!(
            t.contains("not updating the index")
                && t.contains("keeps the index current")
                && t.contains("a process that was killed"),
            "{t}"
        );
        assert!(
            t.contains("cannot rebuild")
                && t.contains("after the restart")
                && !t.contains("rebuild_index"),
            "{t}"
        );
        assert!(t.contains("does not need a restart for that"), "{t}");
        assert!(
            !t.contains("which can be stale: a process that was killed"),
            "{t}"
        );
        assert!(t.contains("pid 4242 per"), "{t}");
    }

    #[test]
    fn reader_notice_for_a_one_off_holder_never_says_it_keeps_the_index_current() {
        let l = TextLayout::new("txt-oneoff");
        let h = holder_info(Role::Index, &l.canonical_kb(), Some(0), Some(false));
        let reason = busy(Some(&h), HolderRelation::OneOffRun(Role::Index));
        let t = reader_notice_text(&l.db, &l.kb, &reason, Some(&h), SystemTime::now());
        assert!(
            t.contains("one-off index run")
                && t.contains("even while it runs")
                && t.contains("which can be stale: a process that was killed"),
            "{t}"
        );
        assert!(!t.contains("keeps the index current"), "{t}");
    }

    #[test]
    fn reader_notice_for_another_kb_names_both_kbs() {
        let l = TextLayout::new("txt-other");
        let other = l.other_kb();
        let h = holder_info(Role::Serve, &other, Some(0), Some(true));
        let reason = busy(Some(&h), HolderRelation::OtherKb(other.clone()));
        let t = reader_notice_text(&l.db, &l.kb, &reason, Some(&h), SystemTime::now());
        assert!(t.contains("different knowledge base"), "{t}");
        assert!(t.contains(&other), "{t}");
        assert!(t.contains(&l.kb.display().to_string()), "{t}");
        assert!(!t.contains("keeps the index current"), "{t}");
    }

    #[test]
    fn reader_notice_without_a_holder_at_start_cannot_tell() {
        let l = TextLayout::new("txt-unknown");
        let reason = busy(None, HolderRelation::Unknown);
        let t = reader_notice_text(&l.db, &l.kb, &reason, None, SystemTime::now());
        assert!(t.contains("cannot be told"), "{t}");
        assert!(t.contains("was readable"), "{t}");
    }

    #[test]
    fn reader_notice_now_says_probably_exited_without_writer_json() {
        let l = TextLayout::new("txt-gone");
        let h = holder_info(Role::Serve, &l.canonical_kb(), Some(0), Some(true));
        let reason = busy(Some(&h), HolderRelation::WatchingSameKb);
        let t = reader_notice_text(&l.db, &l.kb, &reason, None, SystemTime::now());
        assert!(t.contains("Now there is no"), "{t}");
        assert!(t.contains("probably exited"), "{t}");
        assert!(t.contains("restarted"), "{t}");
        assert!(!t.contains("does not need a restart for that"), "{t}");
    }

    #[test]
    fn reader_notice_now_turns_stale_when_the_heartbeat_is_older_than_15s() {
        let l = TextLayout::new("txt-stale");
        let kb = l.canonical_kb();
        let reason = busy(
            Some(&holder_info(Role::Serve, &kb, Some(0), Some(true))),
            HolderRelation::WatchingSameKb,
        );
        for age in [Some(60), None] {
            let h = holder_info(Role::Serve, &kb, age, Some(true));
            let t = reader_notice_text(&l.db, &l.kb, &reason, Some(&h), SystemTime::now());
            assert!(t.contains("heartbeat is older than"), "{age:?}: {t}");
            assert!(t.contains("probably exited"), "{age:?}: {t}");
            assert!(
                !t.contains("does not need a restart for that"),
                "{age:?}: {t}"
            );
        }
    }

    #[test]
    fn reader_notice_now_says_not_indexed_while_the_holder_does_not_report_watching() {
        let l = TextLayout::new("txt-notwatching");
        let kb = l.canonical_kb();
        let reason = busy(
            Some(&holder_info(Role::Serve, &kb, Some(0), Some(true))),
            HolderRelation::WatchingSameKb,
        );
        for watching in [Some(false), None] {
            let h = holder_info(Role::Serve, &kb, Some(0), watching);
            let t = reader_notice_text(&l.db, &l.kb, &reason, Some(&h), SystemTime::now());
            assert!(
                t.contains("does not report that its watcher is watching"),
                "{watching:?}: {t}"
            );
            assert!(
                t.contains("not being indexed while that lasts"),
                "{watching:?}: {t}"
            );
            assert!(
                !t.contains("does not need a restart for that"),
                "{watching:?}: {t}"
            );
            assert!(!t.contains("heartbeat is older than"), "{watching:?}: {t}");
        }
    }

    #[test]
    fn reader_notice_matches_a_non_canonical_spelling_of_the_same_kb() {
        let l = TextLayout::new("txt-spelling");
        std::fs::create_dir_all(l.kb.join("sub")).unwrap();
        let spelled = l.kb.join("sub").join("..");
        let h = holder_info(Role::Serve, &l.canonical_kb(), Some(0), Some(true));
        let reason = busy(Some(&h), HolderRelation::WatchingSameKb);
        let t = reader_notice_text(&l.db, &spelled, &reason, Some(&h), SystemTime::now());
        assert!(t.contains("does not need a restart for that"), "{t}");
    }

    #[test]
    fn reader_notice_treats_a_broken_writer_json_as_absent() {
        let f = fixture("txt-broken");
        let slot = f.slot();
        slot.become_reader(ReaderReason::Busy {
            holder_at_start: None,
            relation: HolderRelation::Unknown,
        });
        std::fs::write(WriterLease::info_path_for(&f.db_path), b"{not json").unwrap();
        let t = slot.notice().expect("a reader always has a notice");
        assert!(t.contains("Now there is no"), "{t}");
        assert!(t.contains("probably exited"), "{t}");
        let s = slot.status();
        assert!(s.reader);
        assert_eq!(s.reader_notice.as_deref(), Some(t.as_str()));
    }

    #[test]
    fn io_reader_notice_names_the_lock_path() {
        let l = TextLayout::new("txt-io");
        let lock_path = WriterLease::lock_path_for(&l.db);
        let reason = ReaderReason::Io {
            lock_path: lock_path.clone(),
            error: "access denied".to_string(),
        };
        let t = reader_notice_text(&l.db, &l.kb, &reason, None, SystemTime::now());
        assert!(t.contains(&lock_path.display().to_string()), "{t}");
        assert!(t.contains("could not take the writer lock"), "{t}");
        assert!(t.contains("access denied"), "{t}");
        assert!(t.contains("cannot rebuild"), "{t}");
        assert!(!t.contains("rebuild_index"), "{t}");
    }

    #[test]
    fn watcher_stopped_text_names_the_error_or_says_it_ended() {
        let l = TextLayout::new("txt-stopped");
        let now = SystemTime::now();
        let t = watcher_stopped_text(&l.db, &l.kb, Some("boom"), false, None, now);
        assert!(t.contains("stopped watching files"), "{t}");
        assert!(t.contains("boom"), "{t}");
        assert!(!t.contains("ended without finishing"), "{t}");
        let t = watcher_stopped_text(&l.db, &l.kb, None, false, None, now);
        assert!(t.contains("ended without finishing"), "{t}");

        let kb = l.canonical_kb();
        let keeper = holder_info(Role::Serve, &kb, Some(0), Some(true));
        let t = watcher_stopped_text(&l.db, &l.kb, None, false, Some(&keeper), now);
        assert!(
            t.contains("another server watching this same knowledge base"),
            "{t}"
        );
        let t = watcher_stopped_text(&l.db, &l.kb, None, true, Some(&keeper), now);
        assert!(!t.contains("another server watching"), "{t}");
        let not_watching = holder_info(Role::Serve, &kb, Some(0), Some(false));
        let t = watcher_stopped_text(&l.db, &l.kb, None, false, Some(&not_watching), now);
        assert!(!t.contains("another server watching"), "{t}");
        let stale = holder_info(Role::Serve, &kb, Some(60), Some(true));
        let t = watcher_stopped_text(&l.db, &l.kb, None, false, Some(&stale), now);
        assert!(!t.contains("another server watching"), "{t}");
        let one_off = holder_info(Role::Index, &kb, Some(0), Some(true));
        let t = watcher_stopped_text(&l.db, &l.kb, None, false, Some(&one_off), now);
        assert!(!t.contains("another server watching"), "{t}");
    }

    #[test]
    fn watch_error_text_says_could_not_watch_and_has_no_catch_up() {
        let cause = WatchErrorCause {
            kb: PathBuf::from("/srv/kb"),
            error: "no such directory".to_string(),
            retry_secs: 3,
        };
        let t = watch_error_text(&cause);
        assert!(t.contains("could not watch"), "{t}");
        assert!(t.contains("no such directory"), "{t}");
        assert!(t.contains("next retry in 3s"), "{t}");
        assert!(t.contains("rebuild_index"), "{t}");
        assert!(!t.contains("catch-up"), "{t}");
    }

    #[test]
    fn notice_prefers_reader_then_stopped_then_watch_error() {
        let f = fixture("txt-priority");
        let slot = f.slot();
        assert_eq!(slot.notice(), None);
        let w = slot.claim(Claimant::Watcher).unwrap();
        let cause = WatchErrorCause {
            kb: f.kb.clone(),
            error: "no such directory".to_string(),
            retry_secs: 1,
        };
        slot.set_watch_error(w.generation(), Some(cause));
        let n = slot.notice().unwrap();
        assert!(n.contains("could not watch"), "{n}");
        assert_eq!(slot.status().watch_error.as_deref(), Some(n.as_str()));
        slot.set_watcher_exit(w.generation(), "boom".to_string());
        slot.release(w, &f.db);
        let n = slot.notice().unwrap();
        assert!(
            n.contains("stopped watching files") && n.contains("boom"),
            "{n}"
        );
        let s = slot.status();
        assert_eq!(s.watcher_stopped.as_deref(), Some(n.as_str()));
        assert!(
            s.watch_error
                .as_deref()
                .is_some_and(|e| e.contains("could not watch")),
            "{s:?}"
        );
        assert!(!s.held);

        let reader = f.slot();
        reader.become_reader(busy(None, HolderRelation::Unknown));
        let n = reader.notice().unwrap();
        assert!(
            n.starts_with("this server is not updating the index"),
            "{n}"
        );
    }

    #[test]
    fn status_of_a_writer_reports_held_role_and_since() {
        let f = fixture("txt-status");
        let slot = f.slot();
        let s = slot.status();
        assert!(
            !s.held && s.role.is_none() && s.held_since.is_none(),
            "{s:?}"
        );
        let w = slot.claim(Claimant::Watcher).unwrap();
        let s = slot.status();
        assert!(s.held, "{s:?}");
        assert_eq!(s.role.as_deref(), Some("serve"));
        let since = s.held_since.clone().expect("held_since");
        assert!(
            chrono::DateTime::parse_from_rfc3339(&since).is_ok(),
            "{since}"
        );
        assert!(!s.reader);
        assert_eq!(s.reader_notice, None);
        assert_eq!(s.watch_error, None);
        assert_eq!(s.watcher_stopped, None);
        assert_eq!(slot.notice(), None);
        slot.release(w, &f.db);

        let s = WriterSlot::unmanaged().status();
        assert!(!s.held && s.role.is_none() && s.held_since.is_none() && !s.reader);
        assert_eq!(WriterSlot::unmanaged().notice(), None);
    }

    #[test]
    fn every_notice_text_is_ascii_for_ascii_paths() {
        let l = TextLayout::new("txt-ascii");
        let kb = l.canonical_kb();
        let other = l.other_kb();
        let now = SystemTime::now();
        let holders = [
            None,
            Some(holder_info(Role::Serve, &kb, Some(0), Some(true))),
            Some(holder_info(Role::Serve, &kb, Some(60), Some(true))),
            Some(holder_info(Role::Serve, &kb, Some(0), None)),
            Some(holder_info(Role::Index, &kb, Some(0), None)),
            Some(holder_info(Role::Serve, &other, Some(0), Some(true))),
        ];
        let relations = [
            HolderRelation::WatchingSameKb,
            HolderRelation::OneOffRun(Role::Rebuild),
            HolderRelation::OtherKb(other.clone()),
            HolderRelation::Unknown,
        ];
        let mut texts = Vec::new();
        for h in &holders {
            for r in &relations {
                let reason = busy(h.as_ref(), r.clone());
                texts.push(reader_notice_text(&l.db, &l.kb, &reason, h.as_ref(), now));
            }
            for error in [Some("boom"), None] {
                texts.push(watcher_stopped_text(
                    &l.db,
                    &l.kb,
                    error,
                    false,
                    h.as_ref(),
                    now,
                ));
            }
        }
        texts.push(reader_notice_text(
            &l.db,
            &l.kb,
            &ReaderReason::Io {
                lock_path: WriterLease::lock_path_for(&l.db),
                error: "denied".to_string(),
            },
            None,
            now,
        ));
        texts.push(watch_error_text(&WatchErrorCause {
            kb: l.kb.clone(),
            error: "gone".to_string(),
            retry_secs: 2,
        }));
        // The words are checked, not the paths the temp dir happens to have.
        let paths = [
            WriterLease::info_path_for(&l.db).display().to_string(),
            WriterLease::lock_path_for(&l.db).display().to_string(),
            l.db.display().to_string(),
            l.kb.display().to_string(),
            kb.clone(),
            other.clone(),
        ];
        for t in texts {
            let mut words = t.clone();
            for p in &paths {
                words = words.replace(p.as_str(), "");
            }
            assert!(words.is_ascii(), "{t}");
        }
    }

    // --- Whole texts, pinned word for word (ADR-0031) ---

    /// The Busy reader's body with its three placeholders filled in.
    fn busy_body(db: &Path, at_start: &str, keeper: &str, now: &str) -> String {
        format!(
            "this server is not updating the index at {}: when it started, another groove process held the writer lock ({at_start}). Searches still work and return what has been indexed. {keeper} This server cannot rebuild the index itself. After a restart while no other process holds the lock, it writes the index and watches files, but changes made before the restart need one rebuild after the restart. {now}",
            db.display()
        )
    }

    #[test]
    fn busy_reader_text_is_pinned_for_each_at_start_and_keeper() {
        let l = TextLayout::new("full-keeper");
        let kb = l.canonical_kb();
        let other = l.other_kb();
        let info = WriterLease::info_path_for(&l.db).display().to_string();
        let now = SystemTime::now();
        let gone = format!("Now there is no {info}, so that process has probably exited.");
        let h = holder_info(Role::Serve, &kb, Some(0), Some(true));
        let at_start_some = format!("pid 4242 per {info}, which can be stale; serve, kb {kb}");
        let at_start_none = format!("no {info} was readable");
        let cases = [
            (
                HolderRelation::WatchingSameKb,
                "That process is a server for this same knowledge base: while it runs and its watcher is watching files, it keeps the index current; after it exits, new, edited and deleted files are not reflected until this server is restarted.".to_string(),
            ),
            (
                HolderRelation::OneOffRun(Role::Index),
                "That process was a one-off index run that does not watch files, so new, edited and deleted files are not reflected until this server is restarted, even while it runs.".to_string(),
            ),
            (
                HolderRelation::OtherKb(other.clone()),
                format!(
                    "That process works on a different knowledge base (kb {other}) that shares this index file, so nothing keeps this server's knowledge base (kb {}) current: new, edited and deleted files in it are not reflected until that process exits and this server is restarted.",
                    l.kb.display()
                ),
            ),
            (
                HolderRelation::Unknown,
                "Whether that process keeps this knowledge base current cannot be told, so treat new, edited and deleted files as not reflected until this server is restarted.".to_string(),
            ),
        ];
        for (relation, keeper) in cases {
            for (at_start, expected_at_start) in
                [(Some(&h), &at_start_some), (None, &at_start_none)]
            {
                let reason = busy(at_start, relation.clone());
                assert_eq!(
                    reader_notice_text(&l.db, &l.kb, &reason, None, now),
                    busy_body(&l.db, expected_at_start, &keeper, &gone),
                    "{relation:?}"
                );
            }
        }
    }

    #[test]
    fn busy_reader_now_text_is_pinned_for_each_form() {
        let l = TextLayout::new("full-now");
        let kb = l.canonical_kb();
        let other = l.other_kb();
        let info = WriterLease::info_path_for(&l.db).display().to_string();
        let now = SystemTime::now();
        let since = "2026-01-01T00:00:00+00:00";
        let reason = busy(None, HolderRelation::Unknown);
        let at_start = format!("no {info} was readable");
        let keeper = "Whether that process keeps this knowledge base current cannot be told, so treat new, edited and deleted files as not reflected until this server is restarted.";
        let cases = [
            (
                None,
                format!("Now there is no {info}, so that process has probably exited."),
            ),
            (
                Some(holder_info(Role::Index, &kb, Some(0), Some(true))),
                format!(
                    "Now {info} names pid 4242 (index, since {since}), which can be stale: a process that was killed leaves the file behind."
                ),
            ),
            (
                Some(holder_info(Role::Serve, &other, Some(0), Some(true))),
                format!(
                    "Now {info} names pid 4242 (serve, since {since}), which can be stale: a process that was killed leaves the file behind."
                ),
            ),
            (
                Some(holder_info(Role::Serve, &kb, Some(0), Some(true))),
                format!(
                    "Now {info} names pid 4242, a server watching this same knowledge base (since {since}): while it runs, it keeps the index current, so searches here see its changes and this server does not need a restart for that. The file can be stale: a process that was killed leaves it behind."
                ),
            ),
            (
                Some(holder_info(Role::Serve, &kb, Some(60), Some(true))),
                format!(
                    "Now {info} names pid 4242, a server watching this same knowledge base (since {since}), but its heartbeat is older than 15s, so that process has probably exited without removing the file; treat new, edited and deleted files as not reflected until this server is restarted."
                ),
            ),
            (
                Some(holder_info(Role::Serve, &kb, Some(0), Some(false))),
                format!(
                    "Now {info} names pid 4242, a server for this same knowledge base (since {since}), but it does not report that its watcher is watching files right now (for example, it could not register a watch and is retrying), so new, edited and deleted files are not being indexed while that lasts. The file can be stale: a process that was killed leaves it behind."
                ),
            ),
        ];
        for (holder_now, now_text) in cases {
            assert_eq!(
                reader_notice_text(&l.db, &l.kb, &reason, holder_now.as_ref(), now),
                busy_body(&l.db, &at_start, keeper, &now_text),
                "{holder_now:?}"
            );
        }
    }

    #[test]
    fn io_reader_watcher_stopped_and_watch_error_texts_are_pinned() {
        let l = TextLayout::new("full-other");
        let kb = l.canonical_kb();
        let info = WriterLease::info_path_for(&l.db).display().to_string();
        let lock_path = WriterLease::lock_path_for(&l.db);
        let now = SystemTime::now();

        let io = ReaderReason::Io {
            lock_path: lock_path.clone(),
            error: "access denied".to_string(),
        };
        assert_eq!(
            reader_notice_text(&l.db, &l.kb, &io, None, now),
            format!(
                "this server is not updating the index at {}: it could not take the writer lock at {} (access denied) when it started. Searches still work and return what has been indexed. New, edited and deleted files are not reflected until the lock works and this server is restarted. This server cannot rebuild the index itself.",
                l.db.display(),
                lock_path.display()
            )
        );

        let stopped = |why: &str| {
            format!(
                "this server stopped watching files for {} ({why}); new, edited and deleted files are not reflected until this server is restarted.",
                l.kb.display()
            )
        };
        assert_eq!(
            watcher_stopped_text(&l.db, &l.kb, Some("boom"), false, None, now),
            stopped("boom")
        );
        assert_eq!(
            watcher_stopped_text(&l.db, &l.kb, None, false, None, now),
            stopped("the watcher task ended without finishing")
        );
        let keeper = holder_info(Role::Serve, &kb, Some(0), Some(true));
        assert_eq!(
            watcher_stopped_text(&l.db, &l.kb, None, false, Some(&keeper), now),
            format!(
                "{} Now {info} names pid 4242, another server watching this same knowledge base (since 2026-01-01T00:00:00+00:00): while it runs, it keeps the index current, so searches here see its changes. The file can be stale: a process that was killed leaves it behind.",
                stopped("the watcher task ended without finishing")
            )
        );

        let cause = WatchErrorCause {
            kb: l.kb.clone(),
            error: "no such directory".to_string(),
            retry_secs: 3,
        };
        assert_eq!(
            watch_error_text(&cause),
            format!(
                "the watcher could not watch {}: no such directory (next retry in 3s); new, edited and deleted files are not being indexed until it succeeds. Fix the cause (on Linux, for example, raise fs.inotify.max_user_watches), or call rebuild_index to index what changed meanwhile.",
                l.kb.display()
            )
        );
    }
}
