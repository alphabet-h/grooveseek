//! One writer per index: an exclusive operating-system lock on `<db>.writer-lock`
//! (never deleted) plus a display-only `<db>.writer.json` naming the holder.
//! Why two files, and why the lock file stays: ADR-0031.

use std::fs::{File, OpenOptions, TryLockError};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime};

use serde::{Deserialize, Serialize};

/// What the process holding the lease is doing with the index.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    /// `groove index`.
    Index,
    /// `groove serve` (it may also watch the knowledge base).
    Serve,
    /// A rebuild of the index.
    Rebuild,
    /// A one-time write by a reader: a schema migration or recording the embedding meta.
    Migration,
}

impl Role {
    /// The name written to `<db>.writer.json` and shown in messages.
    pub fn as_str(self) -> &'static str {
        match self {
            Role::Index => "index",
            Role::Serve => "serve",
            Role::Rebuild => "rebuild",
            Role::Migration => "migration",
        }
    }
}

/// The contents of `<db>.writer.json`. Display only: nothing decides whether
/// the lease is held from it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct HolderInfo {
    pub pid: u32,
    pub role: Role,
    pub started_at: String,
    pub version: String,
    /// The holder's knowledge base, as an absolute path where the holder could make one.
    pub kb_path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub heartbeat_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub watching: Option<bool>,
}

/// How far `heartbeat_at` may be from now, either way, for the holder to
/// count as alive and reporting.
pub const HEARTBEAT_FRESH: Duration = Duration::from_secs(15);

impl HolderInfo {
    /// Whether `heartbeat_at` parses as RFC3339 and lies within
    /// [`HEARTBEAT_FRESH`] of `now`, before or after it. A missing or
    /// unparsable heartbeat, or one further off (the clock jumped), is not fresh.
    pub fn heartbeat_fresh(&self, now: SystemTime) -> bool {
        let Some(at) = self.heartbeat_at.as_deref() else {
            return false;
        };
        let Ok(at) = chrono::DateTime::parse_from_rfc3339(at) else {
            return false;
        };
        let at = SystemTime::from(at.with_timezone(&chrono::Utc));
        let gap = match now.duration_since(at) {
            Ok(past) => past,
            Err(e) => e.duration(),
        };
        gap <= HEARTBEAT_FRESH
    }

    /// Whether the holder keeps the index current: a fresh heartbeat
    /// ([`HolderInfo::heartbeat_fresh`]) and a watcher it reported running.
    pub fn keeps_current(&self, now: SystemTime) -> bool {
        self.heartbeat_fresh(now) && self.watching == Some(true)
    }
}

/// Why the lease was not taken.
#[derive(Debug)]
#[non_exhaustive]
pub enum LeaseError {
    /// Another handle holds the lock. `holder` is what `<db>.writer.json` says, if readable.
    Busy {
        lock_path: PathBuf,
        holder: Option<HolderInfo>,
    },
    /// The lock file could not be opened, or the OS refused the lock call itself.
    Io {
        lock_path: PathBuf,
        source: std::io::Error,
    },
    /// The database path, or the file it resolves to, is not valid UTF-8.
    /// The database is opened by a `&str` path, so its writer would open
    /// another spelling than the one locked; refused before anything is
    /// created. The resolved path is left out when the path asked for is
    /// itself the one that is not UTF-8.
    NotUtf8 {
        db_path: PathBuf,
        resolved_path: Option<PathBuf>,
    },
    /// The server is shutting down and takes the lease no more.
    Closed,
    /// The server started as a reader: it never takes the writer lock.
    Reader,
}

impl std::fmt::Display for LeaseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LeaseError::Busy { lock_path, .. } => write!(
                f,
                "another groove process holds the writer lock at {}",
                lock_path.display()
            ),
            // The OS words `source` in the local language, so only its kind
            // and code are shown: stderr stays ASCII (AGENTS.md).
            LeaseError::Io { lock_path, source } => write!(
                f,
                "could not take the writer lock at {}: {}",
                lock_path.display(),
                crate::embedder::ascii_io_error(source)
            ),
            // Paths by `Debug`, escaped: the bytes that are not UTF-8 show as
            // `\xFF` (unix) or `\u{d800}` (Windows), and stderr stays ASCII.
            LeaseError::NotUtf8 {
                db_path,
                resolved_path,
            } => {
                let shown = crate::watcher::ascii_diag(&format!("{db_path:?}"));
                match resolved_path {
                    None => write!(f, "the database path {shown} is not valid UTF-8")?,
                    Some(r) => write!(
                        f,
                        "the database path {shown} resolves to {}, which is not valid UTF-8",
                        crate::watcher::ascii_diag(&format!("{r:?}"))
                    )?,
                }
                write!(
                    f,
                    ", so groove cannot take its writer lock and wrote nothing; \
                     put the knowledge base under a directory whose path is valid UTF-8."
                )
            }
            LeaseError::Closed => write!(f, "this server is shutting down"),
            LeaseError::Reader => write!(
                f,
                "this server started as a reader and does not take the writer lock"
            ),
        }
    }
}

/// `source()` is left as `None` on purpose: a chain would print the OS's own,
/// possibly non-ASCII, wording of the error that `Display` names by kind and code.
impl std::error::Error for LeaseError {}

/// The database path as the `&str` [`crate::db::Database`] opens, or
/// [`LeaseError::NotUtf8`] when it is not valid UTF-8. A command that only
/// reads the index checks this before it opens anything: a lossy spelling
/// would name another file, which a reader's one-time write could create or
/// migrate. Same refusal, and same wording, as [`WriterLease::try_acquire`].
// The same `LeaseError` as the lease's, for the reason given on `impl WriterLease`.
#[allow(clippy::result_large_err)]
pub fn db_path_utf8(db_path: &Path) -> Result<&str, LeaseError> {
    db_path.to_str().ok_or_else(|| LeaseError::NotUtf8 {
        db_path: db_path.to_path_buf(),
        resolved_path: None,
    })
}

/// The held writer lease. Dropping it removes `<db>.writer.json` and releases
/// the lock; the lock file itself stays.
#[derive(Debug)]
pub struct WriterLease {
    file: File,
    /// The path the lease was asked for, spelled as the caller gave it.
    requested_path: PathBuf,
    /// The database as it is shown: the requested path after [`sidecar_base`]
    /// followed a symlink at its last component. Display only.
    db_path: PathBuf,
    /// The database file this lease locks, resolved whole ([`resolve_whole`])
    /// once, when the lease was taken. The lock, the holder json and the
    /// writer's open are all made from it, never from a spelling resolved again.
    resolved_path: PathBuf,
    lock_path: PathBuf,
    info_path: PathBuf,
    acquired_at: SystemTime,
    role: Role,
    /// What `<db>.writer.json` was last asked to say; [`WriterLease::set_role`]
    /// and [`WriterLease::touch_heartbeat`] rewrite it and publish it again.
    info: HolderInfo,
}

/// Whether a failed heartbeat write has been warned about: the heartbeat is
/// rewritten every few seconds, so its failure is logged once per process.
static HEARTBEAT_WARNED: AtomicBool = AtomicBool::new(false);

/// How often [`WriterLease::acquire_with_wait`] retries while the lock is busy.
const RETRY_INTERVAL: Duration = Duration::from_millis(250);

// `LeaseError::Busy` carries the holder inline so callers can match on it
// directly; the error path runs once per command, so its size costs nothing.
#[allow(clippy::result_large_err)]
impl WriterLease {
    /// `<db>.writer-lock` as messages show it: `<db>` is the database file
    /// after [`sidecar_base`] follows it if it is a symlink. It names the file
    /// the OS lock is taken on, which the lease itself opens through the
    /// whole-path resolution ([`WriterLease::resolved_db_path`]).
    pub fn lock_path_for(db_path: &Path) -> PathBuf {
        suffixed(&sidecar_base(db_path), ".writer-lock")
    }

    /// The database file a lease taken now on the given path would lock: the
    /// path with every symlink in it resolved, the directories above the file
    /// included (see [`resolve_whole`]). A reader that may later write under
    /// the lease opens this path, so the file it holds is the file the lease
    /// is named after.
    pub fn resolved_db_path(db_path: &Path) -> PathBuf {
        resolve_whole(db_path)
    }

    /// `<db>.writer.json` as messages show it, beside the same `<db>` as
    /// [`WriterLease::lock_path_for`].
    pub fn info_path_for(db_path: &Path) -> PathBuf {
        suffixed(&sidecar_base(db_path), ".writer.json")
    }

    /// Take the lease now, or report who holds it. A database path that is
    /// not valid UTF-8, or that resolves to one, is [`LeaseError::NotUtf8`],
    /// before any file is created.
    pub fn try_acquire(db_path: &Path, role: Role, kb_path: &Path) -> Result<Self, LeaseError> {
        Self::try_acquire_with(db_path, role, kb_path, &write_info_file)
    }

    /// [`WriterLease::try_acquire`] with the `<db>.writer.json` write injected, so a
    /// test can make it fail.
    fn try_acquire_with(
        db_path: &Path,
        role: Role,
        kb_path: &Path,
        write_info: &dyn Fn(&Path, &[u8]) -> std::io::Result<()>,
    ) -> Result<Self, LeaseError> {
        // The database is opened by a `&str` path: a path that is not UTF-8
        // would be opened by a lossy spelling, another file than the one
        // locked here. Refused before anything is created (ADR-0031).
        if db_path.to_str().is_none() {
            return Err(LeaseError::NotUtf8 {
                db_path: db_path.to_path_buf(),
                resolved_path: None,
            });
        }
        // Resolved once, whole: the lock, the holder json and the database
        // the holder opens ([`WriterLease::resolved_path`]) all name this one
        // file, even if a symlink at `db_path` -- or a directory link above
        // it -- is repointed while the lease is held. `shown` is for messages.
        let resolved = resolve_whole(db_path);
        if resolved.to_str().is_none() {
            return Err(LeaseError::NotUtf8 {
                db_path: db_path.to_path_buf(),
                resolved_path: Some(resolved),
            });
        }
        let shown = sidecar_base(db_path);
        let lock_path = suffixed(&resolved, ".writer-lock");
        let info_path = suffixed(&resolved, ".writer.json");
        let shown_lock_path = suffixed(&shown, ".writer-lock");
        let file = match OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)
        {
            Ok(file) => file,
            Err(source) => {
                return Err(LeaseError::Io {
                    lock_path: shown_lock_path,
                    source,
                });
            }
        };
        match file.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => {
                return Err(LeaseError::Busy {
                    lock_path: shown_lock_path,
                    holder: read_holder_at(&info_path),
                });
            }
            Err(TryLockError::Error(source)) => {
                return Err(LeaseError::Io {
                    lock_path: shown_lock_path,
                    source,
                });
            }
        }

        let acquired_at = SystemTime::now();
        let now = chrono::Utc::now().to_rfc3339();
        let info = HolderInfo {
            pid: std::process::id(),
            role,
            started_at: now.clone(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            kb_path: normalized_kb_path(kb_path),
            heartbeat_at: Some(now),
            watching: None,
        };
        publish_info(&info_path, &info, write_info, true);

        Ok(WriterLease {
            file,
            requested_path: db_path.to_path_buf(),
            db_path: shown,
            resolved_path: resolved,
            lock_path,
            info_path,
            acquired_at,
            role,
            info,
        })
    }

    /// Change the role this lease is held for, and rewrite `<db>.writer.json`
    /// to say so; `started_at` stays. A failed rewrite is handled as on
    /// acquire: the previous json is removed, so no stale role is shown.
    pub fn set_role(&mut self, role: Role) {
        self.set_role_with(role, &write_info_file);
    }

    /// [`WriterLease::set_role`] with the `<db>.writer.json` write injected.
    fn set_role_with(
        &mut self,
        role: Role,
        write_info: &dyn Fn(&Path, &[u8]) -> std::io::Result<()>,
    ) {
        self.role = role;
        self.info.role = role;
        publish_info(&self.info_path, &self.info, write_info, true);
    }

    /// Rewrite `<db>.writer.json` with `heartbeat_at` set to now and
    /// `watching` as given; `None` leaves the field out (the holder has not
    /// reported a watcher yet). A failed write is warned about once per
    /// process and keeps the previous json: its older heartbeat then goes
    /// stale by itself.
    pub fn touch_heartbeat(&mut self, watching: Option<bool>) {
        self.touch_heartbeat_with(watching, &write_info_file);
    }

    /// [`WriterLease::touch_heartbeat`] with the `<db>.writer.json` write injected.
    fn touch_heartbeat_with(
        &mut self,
        watching: Option<bool>,
        write_info: &dyn Fn(&Path, &[u8]) -> std::io::Result<()>,
    ) {
        self.info.heartbeat_at = Some(chrono::Utc::now().to_rfc3339());
        self.info.watching = watching;
        publish_info(&self.info_path, &self.info, write_info, false);
    }

    /// Retry [`WriterLease::try_acquire`] every 250 ms until it succeeds or
    /// `wait` has passed; then return the last `Busy`. An `Io` error returns at
    /// once. A zero `wait` is a single try. A `wait` too large to add to the
    /// clock means no deadline.
    pub fn acquire_with_wait(
        db_path: &Path,
        role: Role,
        kb_path: &Path,
        wait: Duration,
    ) -> Result<Self, LeaseError> {
        if wait.is_zero() {
            return Self::try_acquire(db_path, role, kb_path);
        }
        let deadline = Instant::now().checked_add(wait);
        loop {
            match Self::try_acquire(db_path, role, kb_path) {
                Err(busy @ LeaseError::Busy { .. }) => {
                    let pause = match deadline {
                        None => RETRY_INTERVAL,
                        Some(deadline) => {
                            let left = deadline.saturating_duration_since(Instant::now());
                            if left.is_zero() {
                                return Err(busy);
                            }
                            left.min(RETRY_INTERVAL)
                        }
                    };
                    std::thread::sleep(pause);
                }
                other => return other,
            }
        }
    }

    /// What `<db>.writer.json` says, or `None` when it is missing, unreadable or
    /// broken. Read beside the database resolved now ([`resolve_whole`]); display only.
    pub fn read_holder(db_path: &Path) -> Option<HolderInfo> {
        read_holder_at(&suffixed(&resolve_whole(db_path), ".writer.json"))
    }

    /// The database as messages show it: the path the lease was asked for,
    /// or the target of a symlink at its last component (see [`sidecar_base`]).
    /// Display only: a writer opens [`WriterLease::resolved_path`].
    pub fn db_path(&self) -> &Path {
        &self.db_path
    }

    /// The database file this lease locks, every symlink in its path resolved
    /// once when the lease was taken ([`resolve_whole`]). A writer under this
    /// lease opens this path, not the spelling it was asked for: a symlink
    /// repointed since then, at the file or at a directory above it, names
    /// another file.
    pub fn resolved_path(&self) -> &Path {
        &self.resolved_path
    }

    /// The path the lease was asked for, as the caller spelled it.
    pub(crate) fn requested_path(&self) -> &Path {
        &self.requested_path
    }

    /// The role this lease was taken for.
    pub fn role(&self) -> Role {
        self.role
    }

    /// When this lease was taken.
    pub fn acquired_at(&self) -> SystemTime {
        self.acquired_at
    }
}

impl Drop for WriterLease {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.info_path);
        if let Err(e) = self.file.unlock() {
            tracing::warn!(
                "could not release the writer lock at {}: {}",
                self.lock_path.display(),
                crate::embedder::ascii_io_error(&e)
            );
        }
    }
}

/// How the holder named in `<db>.writer.json` relates to this process's knowledge base.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HolderRelation {
    /// A `groove serve` on the same knowledge base: its watcher keeps the index current.
    WatchingSameKb,
    /// A one-off run on the same knowledge base, with this role.
    OneOffRun(Role),
    /// A process on a different knowledge base sharing the same database; holds its path.
    OtherKb(String),
    /// No readable holder, or a path that cannot be compared.
    Unknown,
}

/// Classify `holder` against `own_kb`. Both sides are canonicalized, so two
/// spellings of the same directory match; anything that cannot be resolved is `Unknown`.
pub fn holder_relation(holder: Option<&HolderInfo>, own_kb: &Path) -> HolderRelation {
    let Some(h) = holder else {
        return HolderRelation::Unknown;
    };
    let theirs = Path::new(&h.kb_path);
    if !theirs.is_absolute() {
        return HolderRelation::Unknown;
    }
    let (Ok(theirs), Ok(ours)) = (std::fs::canonicalize(theirs), std::fs::canonicalize(own_kb))
    else {
        return HolderRelation::Unknown;
    };
    if theirs != ours {
        HolderRelation::OtherKb(h.kb_path.clone())
    } else if h.role == Role::Serve {
        HolderRelation::WatchingSameKb
    } else {
        HolderRelation::OneOffRun(h.role)
    }
}

/// The refusal `groove index` prints when another process holds the writer
/// lease (ADR-0031); the last line is dropped when the holder is unknown.
pub fn index_busy_message(
    db_path: &Path,
    holder: Option<&HolderInfo>,
    own_kb: &Path,
    waited_secs: Option<u64>,
) -> String {
    let info_path = WriterLease::info_path_for(db_path);
    let waited = waited_secs.map_or(String::new(), |n| format!("waited {n}s; "));
    let who = match holder {
        Some(h) => format!(
            "pid {} per {}, which can be stale ({}, since {}, kb {})",
            h.pid,
            info_path.display(),
            h.role.as_str(),
            h.started_at,
            h.kb_path
        ),
        None => format!("pid unknown (no {})", info_path.display()),
    };
    let relation = holder_relation(holder, own_kb);
    let what_now = match &relation {
        HolderRelation::WatchingSameKb => "  A `groove serve` with its watcher on keeps this index current; to rebuild now, call its MCP tool `rebuild_index`.".to_string(),
        HolderRelation::OneOffRun(role) => format!(
            "  That writer is a one-off {} run, not a server watching files; wait for it with --wait (below), or run this again after it exits.",
            role.as_str()
        ),
        HolderRelation::OtherKb(kb) => format!(
            "  That writer works on a different knowledge base (kb {kb}) that shares this index file; its rebuild_index would remove this knowledge base's rows, so do not use it to rebuild this one."
        ),
        HolderRelation::Unknown => "  If the writer is a groove serve watching this same knowledge base, it keeps this index current and its MCP tool rebuild_index rebuilds it now.".to_string(),
    };
    let other_model = match &relation {
        HolderRelation::WatchingSameKb | HolderRelation::Unknown => {
            "  A rebuild with a different model (`--force --model ...`) needs that writer stopped first: a server's `rebuild_index` keeps the model it started with."
        }
        HolderRelation::OneOffRun(_) => {
            "  A rebuild with a different model (`--force --model ...`) can run once that run exits; --wait (below) waits for it."
        }
        HolderRelation::OtherKb(_) => {
            "  A rebuild with a different model (`--force --model ...`) needs that writer stopped first."
        }
    };
    let mut lines = vec![
        format!(
            "error: {waited}another groove process is writing the index at {}: {who}.",
            db_path.display()
        ),
        what_now,
        "  To wait for the writer instead, run `groove index --wait <SECONDS>`.".to_string(),
        other_model.to_string(),
        "  If the writer is no longer making progress, stopping it releases the lock (the operating system drops it with the process).".to_string(),
    ];
    if let Some(h) = holder {
        lines.push(format!(
            "  Before stopping pid {}, confirm it is groove: check the process name, or `writer.held_since` in the server's /api/admin/status.",
            h.pid
        ));
    }
    lines.join("\n")
}

/// The line `groove index --wait` prints before it waits for the holder, e.g.
/// `waiting for the writer of <db> (pid <pid> per writer.json) up to <N>s`.
pub fn index_waiting_line(db_path: &Path, holder: Option<&HolderInfo>, wait_secs: u64) -> String {
    let pid = match holder {
        Some(h) => format!("pid {} per writer.json", h.pid),
        None => "pid unknown".to_string(),
    };
    format!(
        "waiting for the writer of {} ({pid}) up to {wait_secs}s",
        db_path.display()
    )
}

/// How many symlinks [`sidecar_base`] follows before it gives up (Linux's `MAXSYMLINKS`).
const MAX_SYMLINK_HOPS: usize = 40;

/// The database path as messages show the writer-lease sidecars: the database
/// path, or, while that path itself is a symlink, its target (a relative target
/// is taken from the link's directory). Display only -- the lease locks and
/// opens [`resolve_whole`]'s path.
///
/// Only the last component is followed, and nothing is canonicalized, so a
/// path without a symlink keeps the spelling it was given (no `\\?\`, 8.3
/// names or `/private/var`). A database that does not exist yet is not a
/// symlink, and keeps its path; a dangling symlink is followed to where
/// SQLite would create the file. A link that cannot be read, or a chain
/// longer than [`MAX_SYMLINK_HOPS`], leaves the last path reached.
fn sidecar_base(db_path: &Path) -> PathBuf {
    let mut path = db_path.to_path_buf();
    for _ in 0..MAX_SYMLINK_HOPS {
        let is_link = std::fs::symlink_metadata(&path).is_ok_and(|m| m.file_type().is_symlink());
        if !is_link {
            break;
        }
        let Ok(target) = std::fs::read_link(&path) else {
            break;
        };
        path = match path.parent() {
            Some(dir) if target.is_relative() => dir.join(target),
            _ => target,
        };
    }
    path
}

/// The database file a lease on the given path locks and its writer opens:
/// [`sidecar_base`]'s path with every symlink (and Windows junction) in it
/// resolved, the directories above the file included, so two aliases of one
/// database share one lock, and a directory link repointed after the lease is
/// taken does not move the holder to another file. SQLite would open the same
/// file through the alias at that moment: the unix VFS (`unixFullPathname` in
/// the bundled `sqlite3.c`) resolves every symlink in the path, since groove
/// never passes `SQLITE_OPEN_NOFOLLOW`; on Windows `CreateFileW` follows them.
///
/// A path that exists is `std::fs::canonicalize`d. One that does not (a
/// database not created yet, or a dangling symlink's target) has its nearest
/// existing ancestor canonicalized and the rest appended as spelled; with no
/// such ancestor the absolute path is kept. On Windows the verbatim prefix
/// `canonicalize` adds is dropped where the plain spelling names the same path
/// (the Windows-only helper in [`crate::config`] that a found config's
/// relative base also uses); where it cannot be, the `\\?\C:\...` form is
/// kept, which SQLite opens with its `-wal` and `-shm` beside it (Windows 11,
/// the test `a_verbatim_drive_path_opens_with_its_wal_and_shm_beside_it` in
/// the lease tests of [`crate::db`]).
fn resolve_whole(db_path: &Path) -> PathBuf {
    let base = sidecar_base(db_path);
    let resolved = std::fs::canonicalize(&base).unwrap_or_else(|_| resolve_by_ancestor(&base));
    #[cfg(windows)]
    {
        if let Some(plain) = crate::config::without_verbatim_prefix(&resolved) {
            return plain;
        }
    }
    resolved
}

/// [`resolve_whole`] for a path that does not exist: canonicalize the nearest
/// ancestor that does and append the remaining names as given.
fn resolve_by_ancestor(path: &Path) -> PathBuf {
    let absolute = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    let mut tail = Vec::new();
    let mut cur = absolute.as_path();
    loop {
        if let Ok(mut resolved) = std::fs::canonicalize(cur) {
            for name in tail.iter().rev() {
                resolved.push(name);
            }
            return resolved;
        }
        match (cur.parent(), cur.file_name()) {
            (Some(parent), Some(name)) => {
                tail.push(name.to_os_string());
                cur = parent;
            }
            _ => return absolute,
        }
    }
}

/// What the holder json at `info_path` says, or `None` when it is missing,
/// unreadable or broken.
fn read_holder_at(info_path: &Path) -> Option<HolderInfo> {
    let bytes = std::fs::read(info_path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// The database path with `suffix` appended to its file name.
fn suffixed(db_path: &Path, suffix: &str) -> PathBuf {
    let mut s = db_path.as_os_str().to_os_string();
    s.push(suffix);
    PathBuf::from(s)
}

/// A kb path recorded in writer.json, as a server's notice or refusal shows it.
/// writer.json keeps the canonical form; on Windows that carries the verbatim
/// prefix `canonicalize` adds, which is dropped here where the plain spelling
/// names the same path (`\\?\C:\a` -> `C:\a`, `\\?\UNC\srv\share` ->
/// `\\srv\share`). Anything else is shown as recorded.
pub(crate) fn kb_shown(kb: &str) -> String {
    #[cfg(windows)]
    {
        if let Some(plain) = crate::config::without_verbatim_prefix(Path::new(kb)) {
            return plain.display().to_string();
        }
    }
    kb.to_string()
}

/// The kb path as the holder records it: canonical, else absolute, else as given.
fn normalized_kb_path(kb_path: &Path) -> String {
    std::fs::canonicalize(kb_path)
        .or_else(|_| std::path::absolute(kb_path))
        .unwrap_or_else(|_| kb_path.to_path_buf())
        .display()
        .to_string()
}

fn write_info_file(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    std::fs::write(path, bytes)
}

/// Replace `<db>.writer.json` through `<db>.writer.json.tmp-<pid>`. On failure
/// the temp file is removed; the lease itself is unaffected.
///
/// With `remove_on_failure`, a failure also removes the previous json and is
/// warned about every time: the json would name another holder or role, so
/// none is shown instead. Without it (a heartbeat), the previous json stays,
/// since its heartbeat goes stale by itself, and the failure is warned about
/// once per process ([`HEARTBEAT_WARNED`]).
fn publish_info(
    info_path: &Path,
    info: &HolderInfo,
    write_info: &dyn Fn(&Path, &[u8]) -> std::io::Result<()>,
    remove_on_failure: bool,
) {
    let tmp = suffixed(info_path, &format!(".tmp-{}", std::process::id()));
    let result = serde_json::to_vec_pretty(info)
        .map_err(std::io::Error::other)
        .and_then(|bytes| write_info(&tmp, &bytes))
        .and_then(|()| std::fs::rename(&tmp, info_path));
    if let Err(e) = result {
        let _ = std::fs::remove_file(&tmp);
        if remove_on_failure {
            tracing::warn!(
                "could not write {} (display only, the lease is held): {}",
                info_path.display(),
                crate::embedder::ascii_io_error(&e)
            );
            let _ = std::fs::remove_file(info_path);
        } else if !HEARTBEAT_WARNED.swap(true, Ordering::Relaxed) {
            tracing::warn!(
                "could not refresh the heartbeat in {} (display only, the lease is held; \
                 warned once): {}",
                info_path.display(),
                crate::embedder::ascii_io_error(&e)
            );
        }
    }
}

/// Test helper: a file name that is not valid UTF-8 but that the file system
/// takes: `kb-` and a lone `0xFF` byte on unix, `kb-` and an unpaired
/// surrogate on Windows.
#[cfg(test)]
pub(crate) fn non_utf8_name() -> std::ffi::OsString {
    #[cfg(unix)]
    let name = {
        use std::os::unix::ffi::OsStringExt;
        std::ffi::OsString::from_vec(b"kb-\xff".to_vec())
    };
    #[cfg(windows)]
    let name = {
        use std::os::windows::ffi::OsStringExt;
        let mut wide: Vec<u16> = "kb-".encode_utf16().collect();
        wide.push(0xD800);
        std::ffi::OsString::from_wide(&wide)
    };
    assert!(name.to_str().is_none(), "the name must not be UTF-8");
    name
}

/// Test helper: create the directory at the given path, which holds a name
/// that is not valid UTF-8 ([`non_utf8_name`]), with its parents. Returns false, and the test named `test`
/// skips, only when the file system refuses such a name with `EILSEQ` (macOS's
/// APFS does). This skip is allowed on CI too: there such a path cannot exist,
/// so there is nothing to check. Any other error panics.
#[cfg(test)]
pub(crate) fn create_non_utf8_dir_or_skip(path: &Path, test: &str) -> bool {
    match std::fs::create_dir_all(path) {
        Ok(()) => true,
        Err(e) if refuses_non_utf8_names(&e) => {
            eprintln!("SKIPPED: {test}: the file system refuses names that are not valid UTF-8");
            false
        }
        Err(e) => panic!("create {}: {e}", path.display()),
    }
}

/// Whether `e` is the file system refusing a name for its bytes (`EILSEQ`,
/// "Illegal byte sequence"). Windows takes an unpaired surrogate, so never.
#[cfg(test)]
fn refuses_non_utf8_names(e: &std::io::Error) -> bool {
    #[cfg(unix)]
    {
        e.raw_os_error() == Some(libc::EILSEQ)
    }
    #[cfg(not(unix))]
    {
        let _ = e;
        false
    }
}

/// Test helper: make `link` a file symlink to `target`. Returns false, and the
/// test skips, only for Windows' "a required privilege is not held" (os error
/// 1314: no Developer Mode, not elevated) outside CI. On CI (`CI` set, as
/// GitHub Actions sets it) every failure panics: its Windows runners run as
/// administrators with UAC off, so a symlink there can always be made, and a
/// skip would turn the test green without checking Windows. Any other error
/// panics everywhere.
#[cfg(test)]
pub(crate) fn file_symlink_or_skip(target: &Path, link: &Path) -> bool {
    #[cfg(unix)]
    let made = std::os::unix::fs::symlink(target, link);
    #[cfg(windows)]
    let made = std::os::windows::fs::symlink_file(target, link);
    symlink_made_or_skip(made, "file", target, link)
}

/// Test helper: [`file_symlink_or_skip`] for a directory symlink, under the same rule.
#[cfg(test)]
pub(crate) fn dir_symlink_or_skip(target: &Path, link: &Path) -> bool {
    #[cfg(unix)]
    let made = std::os::unix::fs::symlink(target, link);
    #[cfg(windows)]
    let made = std::os::windows::fs::symlink_dir(target, link);
    symlink_made_or_skip(made, "directory", target, link)
}

/// A directory link a test points the way a person might alias a knowledge base.
#[cfg(test)]
#[derive(Clone, Copy, Debug)]
pub(crate) enum DirLink {
    /// A directory symlink ([`dir_symlink_or_skip`]: skipped where it cannot be made).
    Symlink,
    /// A Windows junction (`mklink /J`), which needs no privilege, so the test
    /// also runs on a Windows machine without Developer Mode.
    #[cfg(windows)]
    Junction,
}

#[cfg(test)]
impl DirLink {
    /// Every kind a test runs through on this platform.
    pub(crate) fn all() -> Vec<DirLink> {
        #[allow(unused_mut)]
        let mut kinds = vec![DirLink::Symlink];
        #[cfg(windows)]
        kinds.push(DirLink::Junction);
        kinds
    }

    /// Make `link` point at the directory `target`, replacing a link already
    /// there. False when the kind cannot be made here and the test skips it.
    pub(crate) fn point(self, link: &Path, target: &Path) -> bool {
        if std::fs::symlink_metadata(link).is_ok() {
            // A directory symlink is removed as a file on unix and as a
            // directory on Windows, as is a junction.
            #[cfg(unix)]
            std::fs::remove_file(link).unwrap();
            #[cfg(windows)]
            std::fs::remove_dir(link).unwrap();
        }
        match self {
            DirLink::Symlink => dir_symlink_or_skip(target, link),
            #[cfg(windows)]
            DirLink::Junction => {
                let made = std::process::Command::new("cmd")
                    .args([
                        "/c",
                        "mklink",
                        "/J",
                        &link.display().to_string(),
                        &target.display().to_string(),
                    ])
                    .stdout(std::process::Stdio::null())
                    .status()
                    .expect("mklink must be runnable");
                assert!(made.success(), "mklink /J needs no elevation");
                true
            }
        }
    }
}

/// The rule [`file_symlink_or_skip`] states, for the result of making a file or directory symlink.
#[cfg(test)]
fn symlink_made_or_skip(made: std::io::Result<()>, kind: &str, target: &Path, link: &Path) -> bool {
    let err = match made {
        Ok(()) => return true,
        Err(e) => e,
    };
    let on_ci = std::env::var_os("CI").is_some_and(|v| !v.is_empty());
    const ERROR_PRIVILEGE_NOT_HELD: i32 = 1314;
    if cfg!(windows) && err.raw_os_error() == Some(ERROR_PRIVILEGE_NOT_HELD) && !on_ci {
        eprintln!(
            "SKIPPED: could not create a {kind} symlink here (os error 1314: Windows needs Developer Mode or admin); CI runs this test"
        );
        return false;
    }
    panic!(
        "could not create the {kind} symlink {} -> {}: {}{}",
        link.display(),
        target.display(),
        crate::embedder::ascii_io_error(&err),
        if on_ci {
            " (CI is set, so this test may not skip)"
        } else {
            ""
        }
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scratch directory under the temp dir, removed on drop (no `tempfile`).
    struct TempDir(PathBuf);
    impl TempDir {
        fn new(prefix: &str) -> Self {
            let p = crate::test_support::unique_temp_path(&format!("groove-lease-{prefix}"));
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

    /// `<tmp>/kb` (created) and the `<tmp>/.groove.db` path beside it (not created).
    fn layout(prefix: &str) -> (TempDir, PathBuf, PathBuf) {
        let dir = TempDir::new(prefix);
        let kb = dir.path().join("kb");
        std::fs::create_dir_all(&kb).unwrap();
        let db = crate::resolve_db_path(&kb);
        (dir, db, kb)
    }

    fn stale_json(pid: u32) -> Vec<u8> {
        serde_json::to_vec_pretty(&HolderInfo {
            pid,
            role: Role::Serve,
            started_at: "2020-01-01T00:00:00+00:00".to_string(),
            version: "0.0.1".to_string(),
            kb_path: "/somewhere/else/kb".to_string(),
            heartbeat_at: None,
            watching: Some(true),
        })
        .unwrap()
    }

    fn no_tmp_files_left(db: &Path) -> bool {
        let info = WriterLease::info_path_for(db);
        let prefix = format!("{}.tmp-", info.file_name().unwrap().to_string_lossy());
        let parent = db.parent().unwrap();
        !std::fs::read_dir(parent)
            .unwrap()
            .filter_map(|e| e.ok())
            .any(|e| e.file_name().to_string_lossy().starts_with(&prefix))
    }

    fn holder(role: Role, kb_path: &str) -> HolderInfo {
        HolderInfo {
            pid: 4242,
            role,
            started_at: "2026-01-01T00:00:00+00:00".to_string(),
            version: "0.0.1".to_string(),
            kb_path: kb_path.to_string(),
            heartbeat_at: None,
            watching: None,
        }
    }

    /// `<tmp>/a` and `<tmp>/kb` both exist, so `<tmp>/a/../kb` canonicalizes on
    /// every OS (unix needs `a` to exist to resolve the `..`).
    fn non_canonical_layout(prefix: &str) -> (TempDir, PathBuf, PathBuf) {
        let dir = TempDir::new(prefix);
        std::fs::create_dir_all(dir.path().join("a")).unwrap();
        let kb = dir.path().join("kb");
        std::fs::create_dir_all(&kb).unwrap();
        let spelled = dir.path().join("a").join("..").join("kb");
        (dir, kb, spelled)
    }

    #[test]
    fn second_try_acquire_on_the_same_db_is_busy_until_the_first_is_dropped() {
        let (_dir, db, kb) = layout("busy");
        let a = WriterLease::try_acquire(&db, Role::Index, &kb).unwrap();
        assert!(matches!(
            WriterLease::try_acquire(&db, Role::Index, &kb),
            Err(LeaseError::Busy { .. })
        ));
        drop(a);
        WriterLease::try_acquire(&db, Role::Index, &kb).expect("free again after drop");
    }

    #[test]
    fn the_lock_file_stays_after_the_lease_is_dropped() {
        let (_dir, db, kb) = layout("stays");
        drop(WriterLease::try_acquire(&db, Role::Index, &kb).unwrap());
        assert!(WriterLease::lock_path_for(&db).exists());
        assert!(!WriterLease::info_path_for(&db).exists());
    }

    #[test]
    fn busy_names_the_holder_from_writer_json() {
        let (_dir, db, kb) = layout("names");
        let _a = WriterLease::try_acquire(&db, Role::Serve, &kb).unwrap();
        let Err(LeaseError::Busy {
            holder: Some(h),
            lock_path,
        }) = WriterLease::try_acquire(&db, Role::Index, &kb)
        else {
            panic!("expected Busy with a holder")
        };
        assert_eq!(h.pid, std::process::id());
        assert_eq!(h.role, Role::Serve);
        assert_eq!(h.heartbeat_at.as_deref(), Some(h.started_at.as_str()));
        assert_eq!(h.watching, None);
        assert_eq!(lock_path, WriterLease::lock_path_for(&db));
    }

    #[test]
    fn a_leftover_lock_file_without_a_holder_does_not_block() {
        let (_dir, db, kb) = layout("leftover");
        std::fs::write(WriterLease::lock_path_for(&db), b"").unwrap();
        WriterLease::try_acquire(&db, Role::Index, &kb).expect("an unlocked leftover file is free");
    }

    #[test]
    fn busy_reports_no_holder_when_writer_json_is_missing_or_broken() {
        let (_dir, db, kb) = layout("broken");
        let _a = WriterLease::try_acquire(&db, Role::Index, &kb).unwrap();
        std::fs::write(WriterLease::info_path_for(&db), b"{not json").unwrap();
        assert!(matches!(
            WriterLease::try_acquire(&db, Role::Index, &kb),
            Err(LeaseError::Busy { holder: None, .. })
        ));
        std::fs::remove_file(WriterLease::info_path_for(&db)).unwrap();
        assert!(matches!(
            WriterLease::try_acquire(&db, Role::Index, &kb),
            Err(LeaseError::Busy { holder: None, .. })
        ));
    }

    #[test]
    fn a_stale_writer_json_is_reported_as_it_is() {
        let (_dir, db, kb) = layout("stale");
        let _a = WriterLease::try_acquire(&db, Role::Index, &kb).unwrap();
        std::fs::write(WriterLease::info_path_for(&db), stale_json(999_999)).unwrap();
        let Err(LeaseError::Busy {
            holder: Some(h), ..
        }) = WriterLease::try_acquire(&db, Role::Index, &kb)
        else {
            panic!("expected Busy with the stale holder")
        };
        assert_eq!(h.pid, 999_999);
        assert_eq!(h.role, Role::Serve);
        assert_eq!(h.kb_path, "/somewhere/else/kb");
    }

    #[test]
    fn a_failed_info_write_removes_the_previous_holders_json() {
        let (_dir, db, kb) = layout("failwrite");
        std::fs::write(WriterLease::info_path_for(&db), stale_json(999_999)).unwrap();
        let lease = WriterLease::try_acquire_with(&db, Role::Index, &kb, &|_, _| {
            Err(std::io::Error::other("injected"))
        })
        .unwrap();
        assert_eq!(WriterLease::read_holder(&db), None);
        assert!(no_tmp_files_left(&db));
        drop(lease);
    }

    #[test]
    fn acquire_succeeds_while_another_handle_is_reading_writer_json() {
        let (_dir, db, kb) = layout("reader");
        drop(WriterLease::try_acquire(&db, Role::Index, &kb).unwrap());
        std::fs::write(WriterLease::info_path_for(&db), stale_json(1)).unwrap();
        let _reader = std::fs::File::open(WriterLease::info_path_for(&db)).unwrap();
        let _lease = WriterLease::try_acquire(&db, Role::Index, &kb).unwrap();
        assert_eq!(
            WriterLease::read_holder(&db).map(|h| h.pid),
            Some(std::process::id())
        );
    }

    #[test]
    fn a_lock_file_that_cannot_be_created_is_an_io_error_naming_it() {
        let (dir, _db, kb) = layout("nodir");
        let db = dir.path().join("no-such-dir").join(".groove.db");
        let err = WriterLease::try_acquire(&db, Role::Index, &kb).unwrap_err();
        let LeaseError::Io { lock_path, .. } = &err else {
            panic!("{err:?}")
        };
        assert_eq!(*lock_path, WriterLease::lock_path_for(&db));
        assert!(err.to_string().contains(&lock_path.display().to_string()));
    }

    /// See [`file_symlink_or_skip`].
    fn file_symlink(target: &Path, link: &Path) -> bool {
        file_symlink_or_skip(target, link)
    }

    /// (GitHub Codex round 1, P2) A `.groove.db` that is a symlink to another
    /// knowledge base's database is the same database to SQLite, so taking the
    /// lease through the alias must be `Busy` while the real path holds it.
    #[test]
    fn a_symlinked_database_shares_the_lock_of_its_target() {
        let (dir, real_db, kb) = layout("alias");
        std::fs::write(&real_db, b"").unwrap();
        let other = dir.path().join("other");
        std::fs::create_dir_all(&other).unwrap();
        let alias_db = other.join(".groove.db");
        if !file_symlink(&real_db, &alias_db) {
            return;
        }
        assert_eq!(
            WriterLease::lock_path_for(&alias_db),
            WriterLease::lock_path_for(&real_db)
        );
        assert_eq!(
            WriterLease::info_path_for(&alias_db),
            WriterLease::info_path_for(&real_db)
        );
        let held = WriterLease::try_acquire(&real_db, Role::Serve, &kb).unwrap();
        let Err(LeaseError::Busy { lock_path, holder }) =
            WriterLease::try_acquire(&alias_db, Role::Index, &other)
        else {
            panic!("taking the lease through the alias must be Busy")
        };
        assert_eq!(lock_path, WriterLease::lock_path_for(&real_db));
        assert_eq!(holder.map(|h| h.role), Some(Role::Serve));
        drop(held);
        let through_alias = WriterLease::try_acquire(&alias_db, Role::Index, &other).unwrap();
        assert!(matches!(
            WriterLease::try_acquire(&real_db, Role::Index, &kb),
            Err(LeaseError::Busy { .. })
        ));
        drop(through_alias);
        assert!(!other.join(".groove.db.writer-lock").exists());
    }

    /// A relative symlink target is taken from the link's directory, not the
    /// process's current directory.
    #[test]
    fn a_relative_database_symlink_resolves_from_the_links_directory() {
        let (dir, real_db, kb) = layout("relalias");
        std::fs::write(&real_db, b"").unwrap();
        let other = dir.path().join("other");
        std::fs::create_dir_all(&other).unwrap();
        let alias_db = other.join(".groove.db");
        if !file_symlink(&Path::new("..").join(".groove.db"), &alias_db) {
            return;
        }
        assert_eq!(
            WriterLease::lock_path_for(&alias_db),
            other.join("..").join(".groove.db.writer-lock")
        );
        let _held = WriterLease::try_acquire(&real_db, Role::Index, &kb).unwrap();
        assert!(matches!(
            WriterLease::try_acquire(&alias_db, Role::Index, &other),
            Err(LeaseError::Busy { .. })
        ));
    }

    /// (Local Codex pre-push round 4) The lease resolves the symlink once: the
    /// lock, the holder json and [`WriterLease::db_path`] stay on the old
    /// target after the link is repointed, and the new target stays free.
    #[test]
    fn a_held_lease_keeps_the_file_it_resolved_when_the_symlink_is_repointed() {
        let (dir, real_db, kb) = layout("repoint");
        std::fs::write(&real_db, b"").unwrap();
        let other_db = dir.path().join("other.db");
        std::fs::write(&other_db, b"").unwrap();
        let alias_db = dir.path().join("alias.db");
        if !file_symlink(&real_db, &alias_db) {
            return;
        }
        let lease = WriterLease::try_acquire(&alias_db, Role::Index, &kb).unwrap();
        assert_eq!(lease.db_path(), real_db.as_path());
        assert_eq!(lease.requested_path(), alias_db.as_path());
        std::fs::remove_file(&alias_db).unwrap();
        assert!(file_symlink(&other_db, &alias_db));

        assert_eq!(lease.db_path(), real_db.as_path());
        assert!(WriterLease::info_path_for(&real_db).exists());
        assert!(matches!(
            WriterLease::try_acquire(&real_db, Role::Index, &kb),
            Err(LeaseError::Busy { .. })
        ));
        drop(WriterLease::try_acquire(&other_db, Role::Index, &kb).expect("other.db is free"));
        drop(lease);
        assert!(!WriterLease::info_path_for(&real_db).exists());
    }

    /// Whether two paths name one file, compared by canonical path.
    fn same_file(a: &Path, b: &Path) -> bool {
        std::fs::canonicalize(a).unwrap() == std::fs::canonicalize(b).unwrap()
    }

    /// (Local Codex pre-push, second round of this push, [high]) The lease
    /// resolves a directory link above the database too: after the link is
    /// repointed, the lock, the holder json and [`WriterLease::resolved_path`]
    /// stay on the first directory's database, and the second's stays free.
    #[test]
    fn a_held_lease_keeps_its_database_when_a_directory_link_above_it_is_repointed() {
        for kind in DirLink::all() {
            let (dir, _db, kb) = layout("dirrepoint");
            let a = dir.path().join("a");
            let b = dir.path().join("b");
            for d in [&a, &b] {
                std::fs::create_dir_all(d).unwrap();
                std::fs::write(d.join(".groove.db"), b"").unwrap();
            }
            let alias = dir.path().join("alias");
            if !kind.point(&alias, &a) {
                continue;
            }
            let alias_db = alias.join(".groove.db");
            let (a_db, b_db) = (a.join(".groove.db"), b.join(".groove.db"));
            let lease = WriterLease::try_acquire(&alias_db, Role::Index, &kb).unwrap();
            assert!(kind.point(&alias, &b));

            assert!(same_file(lease.resolved_path(), &a_db), "{kind:?}");
            assert!(suffixed(&a_db, ".writer.json").exists(), "{kind:?}");
            assert!(!suffixed(&b_db, ".writer.json").exists(), "{kind:?}");
            assert!(
                matches!(
                    WriterLease::try_acquire(&a_db, Role::Index, &kb),
                    Err(LeaseError::Busy { .. })
                ),
                "{kind:?}: a's lease must stay held"
            );
            drop(WriterLease::try_acquire(&alias_db, Role::Index, &kb).expect("b is free"));
            drop(lease);
            assert!(!suffixed(&a_db, ".writer.json").exists(), "{kind:?}");
            drop(WriterLease::try_acquire(&a_db, Role::Index, &kb).expect("a is free again"));
        }
    }

    /// A database that does not exist yet under a directory link resolves
    /// through its nearest existing ancestor, so it too stays put.
    #[test]
    fn a_missing_database_under_a_directory_link_resolves_through_its_parent() {
        for kind in DirLink::all() {
            let (dir, _db, kb) = layout("dirmissing");
            let a = dir.path().join("a");
            let b = dir.path().join("b");
            std::fs::create_dir_all(&a).unwrap();
            std::fs::create_dir_all(&b).unwrap();
            let alias = dir.path().join("alias");
            if !kind.point(&alias, &a) {
                continue;
            }
            let alias_db = alias.join(".groove.db");
            let lease = WriterLease::try_acquire(&alias_db, Role::Index, &kb).unwrap();
            assert!(kind.point(&alias, &b));
            let resolved = lease.resolved_path();
            assert_eq!(
                resolved.file_name(),
                Some(std::ffi::OsStr::new(".groove.db"))
            );
            assert!(same_file(resolved.parent().unwrap(), &a), "{kind:?}");
            assert!(
                !resolved.to_string_lossy().starts_with(r"\\?\"),
                "{kind:?}: {}",
                resolved.display()
            );
            assert!(suffixed(&a.join(".groove.db"), ".writer-lock").exists());
            assert!(!suffixed(&b.join(".groove.db"), ".writer-lock").exists());
            drop(lease);
        }
    }

    #[test]
    fn a_database_that_does_not_exist_keeps_its_own_lock_path() {
        let (_dir, db, _kb) = layout("missingdb");
        assert!(!db.exists());
        assert_eq!(
            WriterLease::lock_path_for(&db),
            suffixed(&db, ".writer-lock")
        );
        assert_eq!(
            WriterLease::info_path_for(&db),
            suffixed(&db, ".writer.json")
        );
    }

    #[test]
    fn lease_error_display_is_ascii() {
        let (_dir, db, kb) = layout("ascii");
        assert!(
            db.display().to_string().is_ascii(),
            "the temp path itself must be ASCII for this test to mean anything: {}",
            db.display()
        );
        let lock_path = WriterLease::lock_path_for(&db);
        let canonical_kb = std::fs::canonicalize(&kb).unwrap();
        let errors = [
            LeaseError::Busy {
                lock_path: lock_path.clone(),
                holder: Some(holder(Role::Serve, &canonical_kb.display().to_string())),
            },
            LeaseError::Busy {
                lock_path: lock_path.clone(),
                holder: None,
            },
            LeaseError::Io {
                lock_path,
                source: std::io::Error::other("x"),
            },
        ];
        for e in errors {
            let s = e.to_string();
            assert!(s.is_ascii(), "non-ASCII display: {s}");
        }
    }

    /// (local Codex round 2) The OS words a lock-file error in the local
    /// language; the `Display` names it by kind and code instead, and keeps the path.
    #[test]
    fn lease_error_io_display_is_ascii_for_a_non_ascii_os_message() {
        let (_dir, db, _kb) = layout("ascii-io");
        let lock_path = WriterLease::lock_path_for(&db);
        let shown = lock_path.display().to_string();
        assert!(
            shown.is_ascii(),
            "the temp path itself must be ASCII for this test to mean anything: {shown}"
        );
        let cases = [
            (
                std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "アクセスが拒否されました。",
                ),
                "PermissionDenied",
            ),
            (std::io::Error::other("ファイルを開けません"), "Other"),
            // A real OS error: its text is CP932 Japanese on a Japanese Windows.
            (std::io::Error::from_raw_os_error(5), "(os error 5)"),
        ];
        for (source, expected) in cases {
            let e = LeaseError::Io {
                lock_path: lock_path.clone(),
                source,
            };
            let s = e.to_string();
            assert!(s.is_ascii(), "non-ASCII display: {s}");
            assert!(s.contains(&shown), "path dropped: {s}");
            assert!(s.contains(expected), "kind or code dropped: {s}");
        }
    }

    #[test]
    fn acquire_with_wait_with_a_huge_wait_while_held_does_not_panic() {
        let (_dir, db, kb) = layout("hugewait");
        let a = WriterLease::try_acquire(&db, Role::Serve, &kb).unwrap();
        let t = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(400));
            drop(a);
        });
        WriterLease::acquire_with_wait(&db, Role::Index, &kb, Duration::from_secs(u64::MAX))
            .expect("no panic, and Ok once dropped");
        t.join().unwrap();
    }

    #[test]
    fn acquire_with_wait_returns_busy_after_the_deadline() {
        let (_dir, db, kb) = layout("deadline");
        let _a = WriterLease::try_acquire(&db, Role::Serve, &kb).unwrap();
        let t0 = Instant::now();
        assert!(matches!(
            WriterLease::acquire_with_wait(&db, Role::Index, &kb, Duration::from_millis(600)),
            Err(LeaseError::Busy { .. })
        ));
        assert!(t0.elapsed() >= Duration::from_millis(600));
    }

    #[test]
    fn acquire_with_wait_gets_the_lease_once_the_holder_drops_it() {
        let (_dir, db, kb) = layout("waitok");
        let a = WriterLease::try_acquire(&db, Role::Serve, &kb).unwrap();
        let t = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(400));
            drop(a);
        });
        let lease = WriterLease::acquire_with_wait(&db, Role::Index, &kb, Duration::from_secs(10))
            .expect("Ok once the holder drops it");
        assert_eq!(lease.role(), Role::Index);
        assert!(lease.acquired_at() <= std::time::SystemTime::now());
        t.join().unwrap();
    }

    #[test]
    fn writer_json_records_the_canonical_kb_path() {
        let (dir, kb, spelled) = non_canonical_layout("canon");
        let db = crate::resolve_db_path(&kb);
        assert_eq!(db.parent(), Some(dir.path()));
        let _lease = WriterLease::try_acquire(&db, Role::Index, &spelled).unwrap();
        let h = WriterLease::read_holder(&db).expect("writer.json is readable");
        let expected = std::fs::canonicalize(&kb).unwrap().display().to_string();
        assert_eq!(h.kb_path, expected);
        assert!(Path::new(&h.kb_path).is_absolute());
    }

    /// writer.json keeps the canonical kb, which `holder_relation` still matches
    /// through either spelling; only the shown form drops the verbatim prefix.
    #[test]
    fn the_shown_kb_drops_the_verbatim_prefix_while_writer_json_keeps_the_canonical_form() {
        let (_dir, kb, spelled) = non_canonical_layout("shown");
        let db = crate::resolve_db_path(&kb);
        let _lease = WriterLease::try_acquire(&db, Role::Serve, &spelled).unwrap();
        let h = WriterLease::read_holder(&db).expect("writer.json is readable");
        let canonical = std::fs::canonicalize(&kb).unwrap().display().to_string();
        assert_eq!(h.kb_path, canonical);
        assert_eq!(
            holder_relation(Some(&h), &kb),
            HolderRelation::WatchingSameKb
        );
        assert_eq!(
            holder_relation(Some(&h), &spelled),
            HolderRelation::WatchingSameKb
        );
        let shown = kb_shown(&h.kb_path);
        assert!(!shown.starts_with(r"\\?\"), "{shown}");
        assert!(Path::new(&shown).is_absolute(), "{shown}");
        assert_eq!(
            std::fs::canonicalize(&shown).unwrap().display().to_string(),
            canonical
        );
    }

    /// The shown form of drive, UNC and unstrippable verbatim paths.
    #[cfg(windows)]
    #[test]
    fn kb_shown_maps_drive_and_unc_verbatim_paths_and_keeps_the_rest() {
        assert_eq!(kb_shown(r"\\?\C:\Users\me\kb"), r"C:\Users\me\kb");
        assert_eq!(kb_shown(r"\\?\UNC\server\share\kb"), r"\\server\share\kb");
        assert_eq!(kb_shown(r"C:\Users\me\kb"), r"C:\Users\me\kb");
        // A trailing dot names another path without the prefix, so it is kept.
        assert_eq!(kb_shown(r"\\?\C:\kb."), r"\\?\C:\kb.");
    }

    #[cfg(not(windows))]
    #[test]
    fn kb_shown_keeps_a_unix_path_as_recorded() {
        assert_eq!(kb_shown("/srv/kb"), "/srv/kb");
    }

    #[test]
    fn holder_relation_is_watching_same_kb_for_a_serve_on_this_kb() {
        let (_dir, kb, _spelled) = non_canonical_layout("same");
        let canonical = std::fs::canonicalize(&kb).unwrap().display().to_string();
        let h = holder(Role::Serve, &canonical);
        assert_eq!(
            holder_relation(Some(&h), &kb),
            HolderRelation::WatchingSameKb
        );
    }

    #[test]
    fn holder_relation_matches_a_non_canonical_spelling_of_the_same_kb() {
        let (_dir, kb, spelled) = non_canonical_layout("spelling");
        let canonical = std::fs::canonicalize(&kb).unwrap().display().to_string();
        let h = holder(Role::Serve, &canonical);
        assert_eq!(
            holder_relation(Some(&h), &spelled),
            HolderRelation::WatchingSameKb
        );
    }

    #[test]
    fn holder_relation_is_one_off_for_index_rebuild_and_migration() {
        let (_dir, kb, _spelled) = non_canonical_layout("oneoff");
        let canonical = std::fs::canonicalize(&kb).unwrap().display().to_string();
        for role in [Role::Index, Role::Rebuild, Role::Migration] {
            let h = holder(role, &canonical);
            assert_eq!(
                holder_relation(Some(&h), &kb),
                HolderRelation::OneOffRun(role)
            );
        }
    }

    #[test]
    fn holder_relation_is_other_kb_for_a_sibling_directory() {
        let (dir, kb, _spelled) = non_canonical_layout("sibling");
        let other = dir.path().join("other");
        std::fs::create_dir_all(&other).unwrap();
        let other_str = std::fs::canonicalize(&other).unwrap().display().to_string();
        let h = holder(Role::Serve, &other_str);
        assert_eq!(
            holder_relation(Some(&h), &kb),
            HolderRelation::OtherKb(other_str.clone())
        );
    }

    #[test]
    fn holder_relation_is_unknown_without_a_holder_or_with_a_relative_kb() {
        let (dir, kb, _spelled) = non_canonical_layout("unknown");
        assert_eq!(holder_relation(None, &kb), HolderRelation::Unknown);
        let relative = holder(Role::Serve, "kb");
        assert_eq!(
            holder_relation(Some(&relative), &kb),
            HolderRelation::Unknown
        );
        let missing = dir.path().join("does-not-exist");
        let gone = holder(Role::Serve, &missing.display().to_string());
        assert!(missing.is_absolute());
        assert_eq!(holder_relation(Some(&gone), &kb), HolderRelation::Unknown);
    }

    /// `<tmp>/kb` and `<tmp>/other` (both created) and the `<tmp>/.groove.db` path (not created).
    fn message_layout(prefix: &str) -> (TempDir, PathBuf, PathBuf, PathBuf) {
        let dir = TempDir::new(prefix);
        let kb = dir.path().join("kb");
        let sibling_kb = dir.path().join("other");
        std::fs::create_dir_all(&kb).unwrap();
        std::fs::create_dir_all(&sibling_kb).unwrap();
        let db = dir.path().join(".groove.db");
        (dir, db, kb, sibling_kb)
    }

    /// A holder for this process whose `kb_path` is the canonical form of an existing `kb`.
    fn holder_on(role: Role, kb: &Path) -> HolderInfo {
        let canonical = std::fs::canonicalize(kb).unwrap().display().to_string();
        HolderInfo {
            pid: std::process::id(),
            ..holder(role, &canonical)
        }
    }

    #[test]
    fn busy_message_for_a_server_on_this_kb_points_to_its_rebuild_index() {
        let (_dir, db, kb, _sibling) = message_layout("msg-serve");
        let h = holder_on(Role::Serve, &kb);
        let m = index_busy_message(&db, Some(&h), &kb, None);
        assert!(m.starts_with("error: another groove process is writing the index at "));
        assert!(m.contains(&format!("pid {} per ", h.pid)) && m.contains("which can be stale"));
        assert!(m.contains("call its MCP tool `rebuild_index`"));
        assert!(m.contains("a server's `rebuild_index` keeps the model it started with"));
        assert!(m.contains("groove index --wait <SECONDS>"));
        assert!(m.contains(&format!("Before stopping pid {}", h.pid)));
        assert!(m.contains(&WriterLease::info_path_for(&db).display().to_string()));
    }

    #[test]
    fn busy_message_for_a_one_off_run_never_names_rebuild_index() {
        let (_dir, db, kb, _sibling) = message_layout("msg-oneoff");
        let m = index_busy_message(&db, Some(&holder_on(Role::Index, &kb)), &kb, None);
        assert!(m.contains("That writer is a one-off index run, not a server watching files"));
        assert!(m.contains("can run once that run exits; --wait (below) waits for it."));
        assert!(!m.contains("rebuild_index"));
    }

    #[test]
    fn busy_message_for_another_kb_warns_against_its_rebuild_index() {
        let (_dir, db, kb, sibling_kb) = message_layout("msg-other");
        let m = index_busy_message(&db, Some(&holder_on(Role::Serve, &sibling_kb)), &kb, None);
        assert!(
            m.contains("works on a different knowledge base")
                && m.contains("would remove this knowledge base's rows")
        );
        assert!(m.contains(
            "A rebuild with a different model (`--force --model ...`) needs that writer stopped first."
        ));
        assert!(!m.contains("a server's `rebuild_index` keeps the model"));
    }

    #[test]
    fn busy_message_without_a_holder_says_pid_unknown_and_drops_the_last_line() {
        let (_dir, db, kb, _sibling) = message_layout("msg-none");
        let m = index_busy_message(&db, None, &kb, None);
        assert!(
            m.contains("pid unknown (no ")
                && m.contains("If the writer is a groove serve watching this same knowledge base")
        );
        assert!(!m.contains("Before stopping pid"));
    }

    #[test]
    fn busy_message_after_waiting_says_how_long() {
        let (_dir, db, kb, _sibling) = message_layout("msg-waited");
        assert!(
            index_busy_message(&db, None, &kb, Some(10))
                .starts_with("error: waited 10s; another groove process")
        );
    }

    #[test]
    fn busy_message_and_waiting_line_are_ascii_for_ascii_paths() {
        let (dir, db, kb, sibling_kb) = message_layout("msg-ascii");
        let tmp = dir.path();
        assert!(
            tmp.to_string_lossy().is_ascii(),
            "the temp path itself must be ASCII for this test to mean anything: {}",
            tmp.display()
        );
        let missing = tmp.join("missing");
        let holders = [
            Some(holder_on(Role::Serve, &kb)),
            Some(holder_on(Role::Index, &kb)),
            Some(holder_on(Role::Serve, &sibling_kb)),
            Some(holder(Role::Serve, &missing.display().to_string())),
            None,
        ];
        for h in &holders {
            for waited in [None, Some(5)] {
                let m = index_busy_message(&db, h.as_ref(), &kb, waited);
                assert!(m.is_ascii(), "non-ASCII busy message: {m}");
            }
            let w = index_waiting_line(&db, h.as_ref(), 30);
            assert!(w.is_ascii(), "non-ASCII waiting line: {w}");
        }
    }

    #[test]
    fn busy_message_renders_a_non_ascii_kb_path() {
        let (dir, db, _kb, _sibling) = message_layout("msg-nonascii");
        let kb = dir.path().join("ノート");
        std::fs::create_dir_all(&kb).unwrap();
        let h = holder_on(Role::Index, &kb);
        let m = index_busy_message(&db, Some(&h), &kb, None);
        assert!(m.contains(&h.kb_path), "{m}");
        assert!(m.contains("one-off index run"), "{m}");
    }

    #[test]
    fn waiting_line_names_the_pid_or_says_unknown() {
        let (_dir, db, kb, _sibling) = message_layout("msg-waitline");
        let h = holder_on(Role::Index, &kb);
        assert_eq!(
            index_waiting_line(&db, Some(&h), 30),
            format!(
                "waiting for the writer of {} (pid {} per writer.json) up to 30s",
                db.display(),
                h.pid
            )
        );
        assert!(index_waiting_line(&db, None, 30).contains("(pid unknown)"));
    }

    #[test]
    fn role_as_str_matches_the_serialized_name() {
        for (role, name) in [
            (Role::Index, "index"),
            (Role::Serve, "serve"),
            (Role::Rebuild, "rebuild"),
            (Role::Migration, "migration"),
        ] {
            assert_eq!(role.as_str(), name);
            assert_eq!(serde_json::to_string(&role).unwrap(), format!("\"{name}\""));
        }
    }

    /// The names in `dir`, as lossy strings, for an assert message.
    fn names_in(dir: &Path) -> Vec<String> {
        std::fs::read_dir(dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect()
    }

    /// `NotUtf8`'s display: ASCII, says why, and says what to do.
    fn assert_not_utf8_display(e: &LeaseError) {
        let s = e.to_string();
        assert!(s.is_ascii(), "non-ASCII display: {s}");
        assert!(
            s.contains("not valid UTF-8") && s.contains("put the knowledge base under"),
            "the reason and the way out are named: {s}"
        );
    }

    /// (local Codex round 3) A database path that is not UTF-8 is refused
    /// before the lock file or anything else is created: the writer opens the
    /// database by a `&str`, whose lossy spelling would be another file.
    #[test]
    fn a_database_path_that_is_not_utf8_is_refused_before_anything_is_created() {
        let dir = TempDir::new("not-utf8");
        let bad = dir.path().join(non_utf8_name());
        let kb = bad.join("kb");
        if !create_non_utf8_dir_or_skip(
            &kb,
            "a_database_path_that_is_not_utf8_is_refused_before_anything_is_created",
        ) {
            return;
        }
        let db = crate::resolve_db_path(&kb);
        let tries = [
            WriterLease::try_acquire(&db, Role::Index, &kb),
            WriterLease::acquire_with_wait(&db, Role::Migration, &kb, Duration::from_secs(2)),
        ];
        for r in tries {
            let e = r.expect_err("a lease on a path that is not UTF-8");
            let LeaseError::NotUtf8 {
                ref db_path,
                resolved_path: None,
            } = e
            else {
                panic!("expected NotUtf8 naming the path itself: {e}");
            };
            assert_eq!(db_path, &db);
            assert_not_utf8_display(&e);
        }
        assert_eq!(
            names_in(&bad),
            vec!["kb".to_string()],
            "neither the lock file, the holder json nor the database is created"
        );
    }

    /// The same when the path is UTF-8 but resolves, through a directory
    /// link, to one that is not.
    #[test]
    fn a_database_path_that_resolves_to_one_that_is_not_utf8_is_refused() {
        let dir = TempDir::new("not-utf8-link");
        let bad = dir.path().join(non_utf8_name());
        if !create_non_utf8_dir_or_skip(
            &bad.join("kb"),
            "a_database_path_that_resolves_to_one_that_is_not_utf8_is_refused",
        ) {
            return;
        }
        let alias = dir.path().join("alias");
        if !dir_symlink_or_skip(&bad, &alias) {
            return;
        }
        let kb = alias.join("kb");
        let db = crate::resolve_db_path(&kb);
        assert!(db.to_str().is_some());
        let e = WriterLease::try_acquire(&db, Role::Index, &kb).expect_err("a lease");
        let LeaseError::NotUtf8 {
            resolved_path: Some(ref resolved),
            ..
        } = e
        else {
            panic!("expected NotUtf8 naming the resolved path: {e}");
        };
        assert!(resolved.to_str().is_none());
        assert_not_utf8_display(&e);
        assert_eq!(names_in(&bad), vec!["kb".to_string()]);
    }

    /// [`db_path_utf8`], the check the reading commands run before they open
    /// the index, hands a UTF-8 path back as it is and refuses one that is
    /// not with the lease's own `NotUtf8`.
    #[test]
    fn db_path_utf8_returns_a_utf8_path_and_refuses_one_that_is_not() {
        let ok = Path::new("kb").join(".groove.db");
        assert_eq!(db_path_utf8(&ok).unwrap(), ok.to_str().unwrap());

        let bad = Path::new("parent").join(non_utf8_name()).join(".groove.db");
        let e = db_path_utf8(&bad).expect_err("a path that is not UTF-8");
        let LeaseError::NotUtf8 {
            ref db_path,
            resolved_path: None,
        } = e
        else {
            panic!("expected NotUtf8 naming the path itself: {e}");
        };
        assert_eq!(db_path, &bad);
        assert_not_utf8_display(&e);
    }

    /// `now` shifted by `secs` (negative is the past), as the RFC3339 the holder json carries.
    fn rfc3339_at(now: SystemTime, secs: i64) -> String {
        let shifted = if secs >= 0 {
            now + Duration::from_secs(secs as u64)
        } else {
            now - Duration::from_secs(secs.unsigned_abs())
        };
        chrono::DateTime::<chrono::Utc>::from(shifted).to_rfc3339()
    }

    fn with_heartbeat(heartbeat_at: Option<String>, watching: Option<bool>) -> HolderInfo {
        HolderInfo {
            heartbeat_at,
            watching,
            ..holder(Role::Serve, "/kb")
        }
    }

    #[test]
    fn heartbeat_fresh_within_15s_and_stale_after() {
        let now = SystemTime::now();
        for (secs, fresh) in [(-14, true), (-16, false), (14, true), (60, false)] {
            let h = with_heartbeat(Some(rfc3339_at(now, secs)), None);
            assert_eq!(h.heartbeat_fresh(now), fresh, "heartbeat {secs}s from now");
        }
        assert!(!with_heartbeat(None, None).heartbeat_fresh(now));
        assert!(!with_heartbeat(Some("garbage".to_string()), None).heartbeat_fresh(now));
    }

    #[test]
    fn keeps_current_needs_a_fresh_heartbeat_and_watching_true() {
        let now = SystemTime::now();
        let fresh = || Some(rfc3339_at(now, -1));
        assert!(with_heartbeat(fresh(), Some(true)).keeps_current(now));
        assert!(!with_heartbeat(fresh(), Some(false)).keeps_current(now));
        assert!(!with_heartbeat(fresh(), None).keeps_current(now));
        assert!(!with_heartbeat(Some(rfc3339_at(now, -60)), Some(true)).keeps_current(now));
    }

    #[test]
    fn set_role_rewrites_writer_json_and_keeps_started_at() {
        let (_dir, db, kb) = layout("setrole");
        let mut lease = WriterLease::try_acquire(&db, Role::Serve, &kb).unwrap();
        let before = WriterLease::read_holder(&db).expect("writer.json after acquire");
        lease.set_role(Role::Rebuild);
        let after = WriterLease::read_holder(&db).expect("writer.json after set_role");
        assert_eq!(after.role, Role::Rebuild);
        assert_eq!(after.started_at, before.started_at);
        assert_eq!(lease.role(), Role::Rebuild);
    }

    #[test]
    fn touch_heartbeat_writes_heartbeat_and_watching() {
        let (_dir, db, kb) = layout("touch");
        let mut lease = WriterLease::try_acquire(&db, Role::Serve, &kb).unwrap();
        let mut old = WriterLease::read_holder(&db).unwrap();
        old.heartbeat_at = Some(rfc3339_at(SystemTime::now(), -60));
        std::fs::write(
            WriterLease::info_path_for(&db),
            serde_json::to_vec_pretty(&old).unwrap(),
        )
        .unwrap();
        assert!(
            !WriterLease::read_holder(&db)
                .unwrap()
                .heartbeat_fresh(SystemTime::now())
        );

        lease.touch_heartbeat(Some(true));
        let h = WriterLease::read_holder(&db).unwrap();
        assert!(h.heartbeat_fresh(SystemTime::now()) && h.watching == Some(true));

        lease.touch_heartbeat(Some(false));
        assert_eq!(WriterLease::read_holder(&db).unwrap().watching, Some(false));

        lease.touch_heartbeat(None);
        let h = WriterLease::read_holder(&db).unwrap();
        assert_eq!(h.watching, None);
        assert!(h.heartbeat_fresh(SystemTime::now()));
        assert!(no_tmp_files_left(&db));
    }

    #[test]
    fn touch_heartbeat_failure_keeps_the_previous_json() {
        let (_dir, db, kb) = layout("touchfail");
        let mut lease = WriterLease::try_acquire(&db, Role::Serve, &kb).unwrap();
        let fail =
            |_: &Path, _: &[u8]| -> std::io::Result<()> { Err(std::io::Error::other("injected")) };
        lease.touch_heartbeat_with(Some(true), &fail);
        assert_eq!(
            WriterLease::read_holder(&db).map(|h| h.pid),
            Some(std::process::id()),
            "a failed heartbeat leaves the previous json in place"
        );
        assert!(no_tmp_files_left(&db));

        lease.set_role_with(Role::Rebuild, &fail);
        assert_eq!(
            WriterLease::read_holder(&db),
            None,
            "a failed role rewrite removes the previous json"
        );
        assert!(no_tmp_files_left(&db));
    }

    #[test]
    fn closed_and_reader_display_is_ascii() {
        for (e, expected) in [
            (LeaseError::Closed, "this server is shutting down"),
            (
                LeaseError::Reader,
                "this server started as a reader and does not take the writer lock",
            ),
        ] {
            let s = e.to_string();
            assert!(s.is_ascii(), "non-ASCII display: {s}");
            assert_eq!(s, expected);
        }
    }
}
