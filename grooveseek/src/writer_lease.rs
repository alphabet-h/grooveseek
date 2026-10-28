//! One writer per index: an exclusive operating-system lock on `<db>.writer-lock`
//! (never deleted) plus a display-only `<db>.writer.json` naming the holder.
//! Why two files, and why the lock file stays: ADR-0031.

use std::fs::{File, OpenOptions, TryLockError};
use std::path::{Path, PathBuf};
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

/// Why the lease was not taken.
#[derive(Debug)]
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
}

impl std::fmt::Display for LeaseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LeaseError::Busy { lock_path, .. } => write!(
                f,
                "another groove process holds the writer lock at {}",
                lock_path.display()
            ),
            LeaseError::Io { lock_path, source } => write!(
                f,
                "could not take the writer lock at {}: {source}",
                lock_path.display()
            ),
        }
    }
}

/// `source()` is left as `None` on purpose: the OS error is already part of
/// the `Display` text, and a chain would print it a second time.
impl std::error::Error for LeaseError {}

/// The held writer lease. Dropping it removes `<db>.writer.json` and releases
/// the lock; the lock file itself stays.
#[derive(Debug)]
pub struct WriterLease {
    file: File,
    lock_path: PathBuf,
    info_path: PathBuf,
    acquired_at: SystemTime,
    role: Role,
}

/// How often [`WriterLease::acquire_with_wait`] retries while the lock is busy.
const RETRY_INTERVAL: Duration = Duration::from_millis(250);

// `LeaseError::Busy` carries the holder inline so callers can match on it
// directly; the error path runs once per command, so its size costs nothing.
#[allow(clippy::result_large_err)]
impl WriterLease {
    /// `<db>.writer-lock`: the file the OS lock is taken on.
    pub fn lock_path_for(db_path: &Path) -> PathBuf {
        suffixed(db_path, ".writer-lock")
    }

    /// `<db>.writer.json`: the display-only description of the holder.
    pub fn info_path_for(db_path: &Path) -> PathBuf {
        suffixed(db_path, ".writer.json")
    }

    /// Take the lease now, or report who holds it.
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
        let lock_path = Self::lock_path_for(db_path);
        let file = match OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)
        {
            Ok(file) => file,
            Err(source) => return Err(LeaseError::Io { lock_path, source }),
        };
        match file.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => {
                return Err(LeaseError::Busy {
                    lock_path,
                    holder: Self::read_holder(db_path),
                });
            }
            Err(TryLockError::Error(source)) => return Err(LeaseError::Io { lock_path, source }),
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
        let info_path = Self::info_path_for(db_path);
        publish_info(&info_path, &info, write_info);

        Ok(WriterLease {
            file,
            lock_path,
            info_path,
            acquired_at,
            role,
        })
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

    /// What `<db>.writer.json` says, or `None` when it is missing, unreadable or broken.
    pub fn read_holder(db_path: &Path) -> Option<HolderInfo> {
        let bytes = std::fs::read(Self::info_path_for(db_path)).ok()?;
        serde_json::from_slice(&bytes).ok()
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
                "could not release the writer lock at {}: {e}",
                self.lock_path.display()
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

/// `db_path` with `suffix` appended to its file name.
fn suffixed(db_path: &Path, suffix: &str) -> PathBuf {
    let mut s = db_path.as_os_str().to_os_string();
    s.push(suffix);
    PathBuf::from(s)
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

/// Replace `<db>.writer.json` through `<db>.writer.json.tmp-<pid>`. On failure,
/// remove the temp file and the previous holder's json, so no stale holder is
/// shown; the lease itself is unaffected.
fn publish_info(
    info_path: &Path,
    info: &HolderInfo,
    write_info: &dyn Fn(&Path, &[u8]) -> std::io::Result<()>,
) {
    let tmp = suffixed(info_path, &format!(".tmp-{}", std::process::id()));
    let result = serde_json::to_vec_pretty(info)
        .map_err(std::io::Error::other)
        .and_then(|bytes| write_info(&tmp, &bytes))
        .and_then(|()| std::fs::rename(&tmp, info_path));
    if let Err(e) = result {
        tracing::warn!(
            "could not write {} (display only, the lease is held): {e}",
            info_path.display()
        );
        let _ = std::fs::remove_file(&tmp);
        let _ = std::fs::remove_file(info_path);
    }
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
}
