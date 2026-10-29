//! The database side of the writer lease: writer opens that take a held
//! [`crate::writer_lease::WriterLease`] as proof, and the `PRAGMA query_only`
//! refusal that keeps a reader from writing. Why: ADR-0031.

use super::*;
use crate::writer_lease::{HolderInfo, LeaseError, Role, WriterLease};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// How long a reader waits for the lease before a one-time write gives up.
const ONE_TIME_WRITE_WAIT: Duration = Duration::from_secs(2);

/// The one-time writes a reader makes under a short lease.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OneTimeWrite {
    /// A schema migration run by [`Database::init`].
    SchemaUpgrade,
    /// Recording the embedding model and dimension in `index_meta`.
    EmbeddingMeta,
    /// Recording the context mode in `index_meta`.
    ContextMode,
}

/// A reader's one-time write stopped because another process holds the
/// writer lease. The root of the `Err` so a caller can `downcast_ref` it.
#[derive(Debug)]
pub struct OneTimeWriteBusy {
    pub purpose: OneTimeWrite,
    pub db_path: PathBuf,
    pub holder: Option<HolderInfo>,
}

// No leading `error:`: `main` prints `Error: ` in front of it.
impl std::fmt::Display for OneTimeWriteBusy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let db = self.db_path.display();
        let info_path = WriterLease::info_path_for(&self.db_path);
        let who = match &self.holder {
            Some(h) => format!(
                "pid {} per {} ({})",
                h.pid,
                info_path.display(),
                h.role.as_str()
            ),
            None => format!("pid unknown (no {})", info_path.display()),
        };
        match self.purpose {
            OneTimeWrite::SchemaUpgrade => write!(
                f,
                "the index at {db} needs a schema upgrade that only a writer can run, \
                 and {who} is writing it; retry after it exits."
            ),
            OneTimeWrite::EmbeddingMeta => write!(
                f,
                "the index at {db} has no embedding model recorded yet, and only a writer \
                 can record it; {who} is writing it (a first or --force index run records \
                 it partway through); retry after it exits."
            ),
            OneTimeWrite::ContextMode => write!(
                f,
                "the index at {db} has no context mode recorded yet, and only a writer can \
                 record it; {who} is writing it; retry after it exits."
            ),
        }
    }
}

impl std::error::Error for OneTimeWriteBusy {}

/// How long a reader's connection waits on a SQLite lock, as [`Database::init`]
/// sets it; set before [`Database::init`] so that its first statement waits too.
const READER_BUSY_TIMEOUT: Duration = Duration::from_millis(30_000);

/// How one reader open attempt failed.
enum AttemptFailure {
    /// [`Database::init`] wanted to write and `query_only` refused it, or
    /// there is no database yet, which only a lease holder may create.
    Refusal,
    /// Anything else, worded as [`Database::open`] words it.
    Fail(anyhow::Error),
}

/// Sort a failed reader open attempt. The database handle is `None` when the
/// connection itself could not be opened; it is dropped here either way, so the
/// connection is closed before anyone waits for the lease.
fn classify_attempt_error(path: &str, db: Option<Database>, e: anyhow::Error) -> AttemptFailure {
    let Some(db) = db else {
        return AttemptFailure::Fail(e);
    };
    // Corruption first, as in `Database::open`: a file that is not a
    // database is never a migration to wait for.
    if let Some(corrupt) = CorruptDatabase::from_init_error(path, &e) {
        return AttemptFailure::Fail(anyhow::Error::new(corrupt));
    }
    if is_query_only_refusal(&db.conn, &e) {
        return AttemptFailure::Refusal;
    }
    AttemptFailure::Fail(e.context(format!("failed to initialise database at {path}")))
}

/// Holds the lease of a [`Database::write_once_under_lease`] and, when it goes
/// (also while a panic in the write unwinds), turns `query_only` back on
/// before it releases the lease. If `query_only` cannot be turned back on, the
/// lease is kept until the process ends, when the OS releases it: a writable
/// connection must not outlive the lease (ADR-0031).
struct QueryOnlyRestore<'a> {
    db: &'a Database,
    /// `None` once [`Self::finish`] or `Drop` has dealt with it.
    lease: Option<WriterLease>,
}

impl QueryOnlyRestore<'_> {
    /// Turn `query_only` back on, then release the lease; on failure keep
    /// the lease (see the type) and return the error.
    fn finish(mut self) -> Result<()> {
        self.restore()
    }

    fn restore(&mut self) -> Result<()> {
        let Some(lease) = self.lease.take() else {
            return Ok(());
        };
        match self.db.set_query_only(true) {
            Ok(()) => {
                drop(lease);
                Ok(())
            }
            Err(e) => {
                std::mem::forget(lease);
                Err(e)
            }
        }
    }
}

impl Drop for QueryOnlyRestore<'_> {
    fn drop(&mut self) {
        // Only reached with the lease still here when `finish` was not
        // called: a `?` before it, or a panic. No panic here (abort risk).
        if let Err(e) = self.restore() {
            tracing::warn!(
                error = %e,
                "failed to turn query_only back on after a one-time write; \
                 keeping the writer lease until this process exits"
            );
        }
    }
}

impl Database {
    /// Open (or create) the database at the given path as a reader: the
    /// connection runs with `PRAGMA query_only` on. A schema upgrade
    /// [`Self::init`] needs, or a missing or empty file, is written once under
    /// a short writer lease (2 s wait); if a writer holds it, the error's root
    /// is [`OneTimeWriteBusy`] and the reader has created nothing.
    /// Why: ADR-0031.
    pub fn open_read_only(path: &str, kb_path: &Path) -> Result<Self> {
        Self::open_read_only_with(path, kb_path, &|| {})
    }

    /// [`Database::open_read_only`] with a hook run after the refusal is read
    /// and before the lease is waited for, so a test can change the world there.
    fn open_read_only_with(path: &str, kb_path: &Path, before_lease: &dyn Fn()) -> Result<Self> {
        // Resolved once, whole, before the first attempt: the reader then
        // holds the file a lease on this same path locks, so a later one-time
        // write under that lease ([`Self::write_once_under_lease`]) writes the
        // file it locked, even if a symlink at `path` or a directory link
        // above it is repointed meanwhile. Messages keep the caller's spelling.
        let shown = path;
        let resolved = WriterLease::resolved_db_path(Path::new(path));
        let resolved = match resolved.to_str() {
            Some(p) => p.to_string(),
            None => anyhow::bail!(
                "the database at {path} resolves to a path that is not valid UTF-8, which groove cannot open"
            ),
        };
        let path = resolved.as_str();
        // Every attempt opens without CREATE: the file is created only by
        // `open_writer` below, under the lease, so whether it exists is
        // decided again there and not on what an attempt saw.
        let attempt = || -> std::result::Result<Self, AttemptFailure> {
            let (db, e) = match Self::open_query_only(path) {
                Ok(Some(db)) => return Ok(db),
                Ok(None) => return Err(AttemptFailure::Refusal),
                Err(failed) => failed,
            };
            Err(classify_attempt_error(shown, db, e))
        };
        match attempt() {
            Ok(db) => return Ok(db),
            Err(AttemptFailure::Fail(e)) => return Err(e),
            Err(AttemptFailure::Refusal) => {}
        }
        before_lease();
        let db_path = Path::new(path);
        match WriterLease::acquire_with_wait(db_path, Role::Migration, kb_path, ONE_TIME_WRITE_WAIT)
        {
            Ok(lease) => {
                {
                    // The writer is closed before the lease is released.
                    let _writer = Self::open_writer(path, &lease)?;
                }
                drop(lease);
                match attempt() {
                    Ok(db) => Ok(db),
                    Err(AttemptFailure::Fail(e)) => Err(e),
                    Err(AttemptFailure::Refusal) => anyhow::bail!(
                        "the index at {shown} still needs a schema upgrade after one was run"
                    ),
                }
            }
            // I42: the holder may have run the upgrade while we waited.
            Err(LeaseError::Busy { holder, .. }) => match attempt() {
                Ok(db) => Ok(db),
                Err(AttemptFailure::Fail(e)) => Err(e),
                Err(AttemptFailure::Refusal) => Err(anyhow::Error::new(OneTimeWriteBusy {
                    purpose: OneTimeWrite::SchemaUpgrade,
                    db_path: PathBuf::from(shown),
                    holder,
                })),
            },
            Err(e) => Err(anyhow::Error::new(e)),
        }
    }

    /// One reader open attempt: connect to an existing database
    /// ([`open_existing`]), switch `query_only` on, then [`Self::init`].
    /// `Ok(None)` when there is no database yet. On failure the connection is
    /// handed back (when there is one) so the caller can tell a `query_only`
    /// refusal from anything else.
    #[allow(clippy::result_large_err)]
    fn open_query_only(
        path: &str,
    ) -> std::result::Result<Option<Self>, (Option<Self>, anyhow::Error)> {
        ensure_vec_extension();
        let conn = match open_existing(path) {
            Ok(Some(conn)) => conn,
            Ok(None) => return Ok(None),
            Err(e) => return Err((None, e)),
        };
        let db = Self {
            conn,
            tags_parse_failures: AtomicU64::new(0),
        };
        // After `Self` is built so that its error, too, goes through the
        // corruption check.
        if let Err(e) = db.set_query_only(true) {
            return Err((Some(db), e));
        }
        // `init`'s first statement reads the header, which a writer that has
        // just created the file may hold locked for a moment.
        if let Err(e) = db.conn.busy_timeout(READER_BUSY_TIMEOUT) {
            return Err((Some(db), e.into()));
        }
        if let Err(e) = db.init() {
            return Err((Some(db), e));
        }
        Ok(Some(db))
    }

    /// Run `f` as this reader's one-time write: take the writer lease as
    /// [`Role::Migration`] (2 s wait; a busy lease is [`OneTimeWriteBusy`]),
    /// lift `query_only` for `f`, and put it back on before the lease goes,
    /// whether `f` failed, succeeded or panicked (see [`QueryOnlyRestore`]).
    pub fn write_once_under_lease<T>(
        &self,
        db_path: &Path,
        kb_path: &Path,
        purpose: OneTimeWrite,
        f: impl FnOnce(&Self) -> Result<T>,
    ) -> Result<T> {
        let lease = match WriterLease::acquire_with_wait(
            db_path,
            Role::Migration,
            kb_path,
            ONE_TIME_WRITE_WAIT,
        ) {
            Ok(lease) => lease,
            Err(LeaseError::Busy { holder, .. }) => {
                return Err(anyhow::Error::new(OneTimeWriteBusy {
                    purpose,
                    db_path: db_path.to_path_buf(),
                    holder,
                }));
            }
            Err(e) => return Err(anyhow::Error::new(e)),
        };
        // The lease covers the file `db_path` resolved to just now; this
        // connection was opened earlier. If a symlink in `db_path` (at the
        // file or a directory above it) was repointed in between, they are
        // different files, and writing here would write a file another
        // process's lease may cover (ADR-0031).
        if !connection_is_on(&self.conn, lease.resolved_path()) {
            anyhow::bail!(
                "the index at {} now resolves to {}, not the file this connection opened; \
                 nothing was written, run the command again",
                db_path.display(),
                lease.resolved_path().display()
            );
        }
        // From here on the guard puts query_only back on before the lease
        // goes, on every exit: `?`, `f`'s Err, and a panic in `f` unwinding.
        let guard = QueryOnlyRestore {
            db: self,
            lease: Some(lease),
        };
        self.set_query_only(false)?;
        let r = f(self);
        guard
            .finish()
            .context("failed to turn query_only back on after a one-time write")?;
        r
    }

    /// [`Database::verify_embedding_meta`] for a reader: a meta that is
    /// missing is recorded under a short writer lease; a meta that matches or
    /// mismatches is answered without the lease.
    pub fn verify_embedding_meta_or_record(
        &self,
        db_path: &Path,
        kb_path: &Path,
        model: &str,
        dim: u32,
    ) -> Result<()> {
        self.verify_embedding_meta_or_record_with(db_path, kb_path, model, dim, &|| {})
    }

    /// [`Database::verify_embedding_meta_or_record`] with a hook run after the
    /// refusal is read and before the lease is waited for.
    fn verify_embedding_meta_or_record_with(
        &self,
        db_path: &Path,
        kb_path: &Path,
        model: &str,
        dim: u32,
        before_lease: &dyn Fn(),
    ) -> Result<()> {
        let e = match self.verify_embedding_meta(model, dim) {
            Ok(()) => return Ok(()),
            Err(e) => e,
        };
        if !is_query_only_refusal(&self.conn, &e) {
            return Err(e);
        }
        before_lease();
        let busy =
            match self.write_once_under_lease(db_path, kb_path, OneTimeWrite::EmbeddingMeta, |db| {
                db.verify_embedding_meta(model, dim)
            }) {
                Ok(()) => return Ok(()),
                Err(e) if e.downcast_ref::<OneTimeWriteBusy>().is_some() => e,
                Err(e) => return Err(e),
            };
        // I42: the holder may have recorded the meta while we waited -- or
        // recorded another model, which is a mismatch, not a reason to retry.
        match self.verify_embedding_meta(model, dim) {
            Ok(()) => Ok(()),
            Err(e) if is_query_only_refusal(&self.conn, &e) => Err(busy),
            Err(e) => Err(e),
        }
    }
    /// [`Database::open`] for a process that holds the writer lease; asking
    /// for the lease is how the caller shows it holds one. When the given path
    /// is the one the lease was asked for, the file opened is the one the lease
    /// locks ([`WriterLease::resolved_path`], resolved when it was taken), not
    /// what that path names now: a symlink repointed in between, at the file or
    /// at a directory above it, would otherwise put this writer on a file
    /// another process's lease covers (ADR-0031). A corrupt file is named as
    /// the caller spelled it.
    pub fn open_writer(path: &str, lease: &WriterLease) -> Result<Self> {
        Self::open(&leased_path(path, lease)?).map_err(|e| shown_as(e, path))
    }

    /// [`Database::open_or_replace_corrupt`] for a process that holds the
    /// writer lease, on the same file as [`Database::open_writer`] picks.
    pub fn open_or_replace_corrupt_with(path: &str, lease: &WriterLease) -> Result<(Self, bool)> {
        Self::open_or_replace_corrupt(&leased_path(path, lease)?).map_err(|e| shown_as(e, path))
    }

    /// Set `PRAGMA query_only` on this connection: while it is on, every
    /// write fails with `SQLITE_READONLY`.
    pub(crate) fn set_query_only(&self, on: bool) -> Result<()> {
        self.conn.pragma_update(None, "query_only", on)?;
        Ok(())
    }
}

/// The file a writer under the given lease opens for the given path: the
/// lease's resolved database ([`WriterLease::resolved_path`]) when the path is
/// spelled as the lease was asked for, else the path as given. No production
/// caller passes another path: the binary opens its writer only in its
/// index command, under the lease it took on that same path. A lease is only
/// ever taken on a path that is UTF-8 and resolves to one
/// ([`LeaseError::NotUtf8`]), so a caller's `to_string_lossy` of that path is
/// the path itself and compares equal.
fn leased_path(path: &str, lease: &WriterLease) -> Result<String> {
    if Path::new(path) != lease.requested_path() {
        return Ok(path.to_string());
    }
    match lease.resolved_path().to_str() {
        Some(p) => Ok(p.to_string()),
        None => anyhow::bail!(
            "the database at {path} resolves to a path that is not valid UTF-8, which groove cannot open"
        ),
    }
}

/// A [`CorruptDatabase`] from opening the resolved path, renamed to the path
/// the caller spelled (the same file when the lease was taken): messages show
/// the operator's spelling, not a canonical one. Any other error is kept.
fn shown_as(e: anyhow::Error, shown: &str) -> anyhow::Error {
    match e.downcast::<CorruptDatabase>() {
        Ok(mut corrupt) => {
            corrupt.path = shown.to_string();
            anyhow::Error::new(corrupt)
        }
        Err(e) => e,
    }
}

/// Whether the connection has the file at the given path open. Compared by canonical
/// path, so two spellings of one file match; `false` when either side cannot
/// be resolved (an in-memory connection has no file).
fn connection_is_on(conn: &Connection, db_path: &Path) -> bool {
    let Some(open) = conn.path().filter(|p| !p.is_empty()) else {
        return false;
    };
    match (std::fs::canonicalize(open), std::fs::canonicalize(db_path)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

/// Open the database at the given path for a reader without creating it: `None` when
/// there is no database yet -- the file is missing, or empty because a writer
/// has created it and not yet written the header. Only a lease holder creates
/// one (ADR-0031): a reader that created the file, or read an empty one with
/// `query_only` on (`PRAGMA journal_mode = WAL` still writes the header), would
/// write it outside the lease while a writer may be initialising it.
fn open_existing(path: &str) -> Result<Option<Connection>> {
    use rusqlite::OpenFlags;
    // READ_WRITE without CREATE, not READ_ONLY: a write refused on a READ_ONLY
    // connection is not a `query_only` refusal, so a needed migration would go unseen.
    let flags = OpenFlags::default() - OpenFlags::SQLITE_OPEN_CREATE;
    let failed = || format!("failed to open database at {path}");
    let mut reopened = false;
    loop {
        let e = match Connection::open_with_flags(path, flags) {
            Ok(conn) => {
                return match std::fs::metadata(path) {
                    Ok(m) if m.len() > 0 => Ok(Some(conn)),
                    Ok(_) => Ok(None),
                    Err(m) if m.kind() == std::io::ErrorKind::NotFound => Ok(None),
                    Err(m) => Err(anyhow::Error::new(m).context(failed())),
                };
            }
            Err(e) => e,
        };
        let cannot_open = matches!(
            &e,
            rusqlite::Error::SqliteFailure(f, _) if f.code == rusqlite::ErrorCode::CannotOpen
        );
        if cannot_open {
            match std::fs::metadata(path) {
                // Missing, in a directory that is there (a missing directory
                // keeps the error it always had).
                Err(m) if m.kind() == std::io::ErrorKind::NotFound && parent_is_dir(path) => {
                    return Ok(None);
                }
                // Created between the open and this look: open it once more.
                Ok(m) if m.is_file() && !reopened => {
                    reopened = true;
                    continue;
                }
                _ => {}
            }
        }
        return Err(anyhow::Error::new(e).context(failed()));
    }
}

/// Whether the directory a database at the given path would be created in exists.
fn parent_is_dir(path: &str) -> bool {
    match Path::new(path).parent() {
        Some(p) if !p.as_os_str().is_empty() => p.is_dir(),
        _ => true,
    }
}

/// Whether `e` is a write refused by `PRAGMA query_only` on `conn`: the first
/// `rusqlite` error in the chain is `SQLITE_READONLY` exactly (extended code
/// 8), and `conn` was not opened read-only. Anything else, including a failure
/// to ask `conn`, is not.
pub(crate) fn is_query_only_refusal(conn: &Connection, e: &anyhow::Error) -> bool {
    let first = e
        .chain()
        .find_map(|cause| cause.downcast_ref::<rusqlite::Error>());
    let Some(rusqlite::Error::SqliteFailure(failure, _)) = first else {
        return false;
    };
    failure.extended_code == rusqlite::ffi::SQLITE_READONLY
        && matches!(conn.is_readonly(rusqlite::MAIN_DB), Ok(false))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::writer_lease::Role;
    use rusqlite::OpenFlags;
    use std::path::{Path, PathBuf};

    const MAIN: &std::ffi::CStr = rusqlite::MAIN_DB;
    const MODEL: &str = "bge-small-en-v1.5";
    const DIM: u32 = 384;

    /// A scratch directory under the temp dir, removed on drop (no `tempfile`).
    struct TempDir(PathBuf);
    impl TempDir {
        fn new(prefix: &str) -> Self {
            let p = crate::test_support::unique_temp_path(&format!("groove-dblease-{prefix}"));
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

    fn upsert(db: &Database, path: &str) -> Result<i64> {
        db.upsert_document(path, Some("t"), None, None, None, &[], None, "hash", 1)
    }

    fn chunk(db: &Database, doc: i64, index: i32) -> Result<i64> {
        db.insert_chunk(
            doc,
            index,
            Some("h"),
            Some(1),
            "body",
            None,
            &[0.1; 384],
            1.0,
        )
    }

    /// `<dir>/.groove.db` holding one document `a.md` with one chunk; returns
    /// the path and the document id. The connection is closed on return.
    fn fixture(dir: &TempDir) -> (String, i64) {
        let path = dir.path().join(".groove.db");
        let path = path.to_str().unwrap().to_string();
        let db = Database::open(&path).unwrap();
        db.verify_embedding_meta(MODEL, DIM).unwrap();
        let doc = upsert(&db, "a.md").unwrap();
        chunk(&db, doc, 0).unwrap();
        (path, doc)
    }

    /// A reader as Task 5's [`Database::open_read_only`] will make it: opened, then
    /// switched to `query_only`.
    fn query_only(path: &str) -> Database {
        let r = Database::open(path).unwrap();
        r.set_query_only(true).unwrap();
        r
    }

    /// Row counts of every table a write touches, plus every `index_meta` row,
    /// read through a fresh read-only connection.
    fn snapshot(path: &str) -> (Vec<i64>, Vec<(String, String)>) {
        ensure_vec_extension();
        let c = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        let counts = ["documents", "chunks", "fts_chunks", "vec_chunks"]
            .iter()
            .map(|t| {
                c.query_row(&format!("SELECT count(*) FROM {t}"), [], |r| r.get(0))
                    .unwrap()
            })
            .collect();
        let mut stmt = c
            .prepare("SELECT key, value FROM index_meta ORDER BY key")
            .unwrap();
        let meta = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<std::result::Result<Vec<_>, _>>()
            .unwrap();
        (counts, meta)
    }

    fn sqlite_failure(e: &anyhow::Error) -> rusqlite::ffi::Error {
        e.chain()
            .find_map(|c| match c.downcast_ref::<rusqlite::Error>() {
                Some(rusqlite::Error::SqliteFailure(f, _)) => Some(*f),
                _ => None,
            })
            .unwrap_or_else(|| panic!("no SqliteFailure in {e:#}"))
    }

    #[test]
    fn query_only_refuses_every_write_path_and_leaves_the_db_unchanged() {
        // T-L6 / P1 (spec §5.3, §9).
        let dir = TempDir::new("t-l6");
        let (path, doc) = fixture(&dir);
        let before = snapshot(&path);
        let r = query_only(&path);

        let w1 = upsert(&r, "b.md").unwrap_err();
        let w2 = r.delete_document("a.md").unwrap_err();
        let w3 = chunk(&r, doc, 1).unwrap_err();
        let w4 = anyhow::Error::from(
            r.conn
                .execute(
                    "INSERT INTO fts_chunks (rowid, heading, context, content) \
                     VALUES (999, 'h', 'c', 'x')",
                    [],
                )
                .unwrap_err(),
        );
        let w5 = anyhow::Error::from(
            r.conn
                .execute(
                    "INSERT INTO vec_chunks (chunk_id, embedding) VALUES (999, ?1)",
                    [serde_json::to_string(&vec![0.2f32; 384]).unwrap()],
                )
                .unwrap_err(),
        );

        for e in [w1, w2, w3, w4, w5] {
            assert!(is_query_only_refusal(&r.conn, &e), "{e:#}");
            assert_eq!(sqlite_failure(&e).extended_code, 8, "{e:#}");
        }
        assert!(!r.conn.is_readonly(MAIN).unwrap());
        drop(r);
        assert_eq!(snapshot(&path), before);
    }

    #[test]
    fn a_read_only_open_is_not_a_query_only_refusal() {
        let dir = TempDir::new("ro-open");
        let (path, _) = fixture(&dir);
        ensure_vec_extension();
        let c = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        assert!(c.is_readonly(MAIN).unwrap());
        let e = anyhow::Error::from(
            c.execute("DELETE FROM documents WHERE path = 'a.md'", [])
                .unwrap_err(),
        );
        assert!(!is_query_only_refusal(&c, &e), "{e:#}");
    }

    #[test]
    fn an_unrelated_error_is_not_a_query_only_refusal() {
        let dir = TempDir::new("unrelated");
        let (path, _) = fixture(&dir);
        let r = query_only(&path);
        let e = anyhow::Error::from(
            r.conn
                .query_row("SELECT 1 FROM no_such_table", [], |row| {
                    row.get::<_, i64>(0)
                })
                .unwrap_err(),
        );
        assert!(!is_query_only_refusal(&r.conn, &e), "{e:#}");
    }

    #[test]
    fn drop_flushes_the_tags_counter_from_a_query_only_connection_and_leaves_it_on() {
        // spec §5.6.
        let dir = TempDir::new("drop-flush");
        let (path, _) = fixture(&dir);
        {
            let r = query_only(&path);
            r.tags_parse_failures.fetch_add(3, Ordering::Relaxed);
            // query_only is still on when Drop runs.
            let e = upsert(&r, "c.md").unwrap_err();
            assert!(is_query_only_refusal(&r.conn, &e), "{e:#}");
        }
        let db = Database::open(&path).unwrap();
        assert_eq!(db.tags_parse_failure_count(), 3);
    }

    #[test]
    fn open_writer_and_open_or_replace_corrupt_with_open_like_open() {
        let dir = TempDir::new("writer-open");
        let kb = dir.path().join("kb");
        std::fs::create_dir_all(&kb).unwrap();
        let path = dir.path().join(".groove.db");
        let lease = WriterLease::try_acquire(&path, Role::Index, &kb).unwrap();

        let db = Database::open_writer(path.to_str().unwrap(), &lease).unwrap();
        db.verify_embedding_meta(MODEL, DIM).unwrap();
        upsert(&db, "a.md").unwrap();
        drop(db);

        let corrupt = dir.path().join("corrupt.db");
        std::fs::write(&corrupt, b"not a sqlite database").unwrap();
        let (db, replaced) =
            Database::open_or_replace_corrupt_with(corrupt.to_str().unwrap(), &lease).unwrap();
        assert!(replaced);
        upsert(&db, "a.md").unwrap();
    }

    // -- Task 5: open_read_only / write_once_under_lease / verify_or_record --

    use crate::writer_lease::HolderInfo;
    use std::sync::{Arc, Barrier, Mutex};
    use std::time::{Duration, Instant};

    /// `<dir>/kb`, created.
    fn kb_of(dir: &TempDir) -> PathBuf {
        let kb = dir.path().join("kb");
        std::fs::create_dir_all(&kb).unwrap();
        kb
    }

    fn db_path_of(dir: &TempDir) -> String {
        dir.path().join(".groove.db").to_str().unwrap().to_string()
    }

    /// A database as an older groove left it: meta recorded, but the documents
    /// table lacks the `size_bytes` column that [`Database::init`] adds.
    fn make_legacy_db(path: &str) {
        let db = Database::open(path).unwrap();
        db.verify_embedding_meta(MODEL, DIM).unwrap();
        // `ALTER TABLE .. DROP COLUMN size_bytes` fails here ("incomplete
        // input", the comment in the CREATE statement), so rebuild the table
        // as db.rs's feature-51 test does.
        db.conn
            .execute_batch(
                "DROP TABLE documents;
                 CREATE TABLE documents (
                    id INTEGER PRIMARY KEY AUTOINCREMENT, path TEXT UNIQUE NOT NULL,
                    title TEXT, topic TEXT, category TEXT, depth TEXT, tags TEXT,
                    date TEXT, content_hash TEXT NOT NULL, last_indexed TEXT NOT NULL
                 );",
            )
            .unwrap();
        drop(db);
        assert!(!has_size_bytes(path));
    }

    /// A database with no embedding meta recorded (and no chunks).
    fn make_metaless_db(path: &str) {
        let db = Database::open(path).unwrap();
        assert!(db.read_embedding_meta().unwrap().is_none());
    }

    fn has_size_bytes(path: &str) -> bool {
        ensure_vec_extension();
        let c = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        let mut stmt = c.prepare("PRAGMA table_info(documents)").unwrap();
        stmt.query_map([], |r| r.get::<_, String>(1))
            .unwrap()
            .filter_map(std::result::Result::ok)
            .any(|n| n == "size_bytes")
    }

    fn query_only_flag(db: &Database) -> i64 {
        db.conn
            .query_row("PRAGMA query_only", [], |r| r.get(0))
            .unwrap()
    }

    /// The error of a [`Database::open_read_only`] that must fail ([`Database`]
    /// is not `Debug`).
    fn open_read_only_err(path: &str, kb: &Path) -> anyhow::Error {
        match Database::open_read_only(path, kb) {
            Ok(_) => panic!("open_read_only({path}) succeeded"),
            Err(e) => e,
        }
    }

    fn lease_is_free(db: &str, kb: &Path) -> bool {
        WriterLease::try_acquire(Path::new(db), Role::Index, kb).is_ok()
    }

    #[test]
    fn open_read_only_on_a_migrated_db_writes_nothing_and_stays_query_only() {
        // T-L10b / P2.
        let dir = TempDir::new("ro-migrated");
        let kb = kb_of(&dir);
        let (path, _) = fixture(&dir);
        let before = (std::fs::read(&path).unwrap(), snapshot(&path));
        let r = Database::open_read_only(&path, &kb).unwrap();
        assert_eq!(query_only_flag(&r), 1);
        drop(r);
        let after = (std::fs::read(&path).unwrap(), snapshot(&path));
        assert!(before.0 == after.0, "the DB file's bytes changed");
        assert_eq!(after.1, before.1);
    }

    #[test]
    fn open_read_only_creates_a_missing_db() {
        // T-L10b.
        let dir = TempDir::new("ro-missing");
        let kb = kb_of(&dir);
        let path = db_path_of(&dir);
        let r = Database::open_read_only(&path, &kb).unwrap();
        assert!(Path::new(&path).is_file());
        assert_eq!(query_only_flag(&r), 1);
        drop(r);
        assert!(lease_is_free(&path, &kb));
    }

    #[test]
    fn open_read_only_on_a_missing_db_while_a_writer_holds_the_lease_fails_and_measures_the_header()
    {
        // P2's header measurement (spec §5.5 step 4 allows either size).
        let dir = TempDir::new("ro-missing-held");
        let kb = kb_of(&dir);
        let path = db_path_of(&dir);
        let _held = WriterLease::try_acquire(Path::new(&path), Role::Index, &kb).unwrap();
        let err = open_read_only_err(&path, &kb);
        let busy = err
            .downcast_ref::<OneTimeWriteBusy>()
            .unwrap_or_else(|| panic!("not OneTimeWriteBusy: {err:#}"));
        assert_eq!(busy.purpose, OneTimeWrite::SchemaUpgrade);
        let size = std::fs::metadata(&path).map(|m| m.len());
        eprintln!("P2 header: size of the missing DB after a refused reader open = {size:?}");
    }

    /// Whether `<path>`, `<path>-wal` or `<path>-shm` exists.
    fn any_db_file(path: &str) -> bool {
        ["", "-wal", "-shm"]
            .iter()
            .any(|side| Path::new(&format!("{path}{side}")).exists())
    }

    #[test]
    fn open_read_only_on_a_missing_db_while_a_writer_holds_the_lease_creates_nothing() {
        // Local Codex review round 3: the reader must not create the file, or
        // write its header, outside the lease.
        let dir = TempDir::new("ro-missing-held-nothing");
        let kb = kb_of(&dir);
        let path = db_path_of(&dir);
        let held = WriterLease::try_acquire(Path::new(&path), Role::Index, &kb).unwrap();
        let err = open_read_only_err(&path, &kb);
        let busy = err
            .downcast_ref::<OneTimeWriteBusy>()
            .unwrap_or_else(|| panic!("not OneTimeWriteBusy: {err:#}"));
        assert_eq!(busy.purpose, OneTimeWrite::SchemaUpgrade);
        assert!(!any_db_file(&path), "the reader created the DB file");
        drop(held);

        let r = Database::open_read_only(&path, &kb).unwrap();
        assert!(Path::new(&path).is_file());
        assert_eq!(query_only_flag(&r), 1);
        drop(r);
        assert!(lease_is_free(&path, &kb));
    }

    #[test]
    fn open_read_only_leaves_an_empty_db_alone_while_its_writer_initialises_it() {
        // Local Codex review round 3, the deterministic interleaving: the index
        // holder has created the file (0 bytes, as `Connection::open` leaves
        // it) and not yet run `init` when a reader opens it.
        let dir = TempDir::new("ro-empty-held");
        let kb = kb_of(&dir);
        let path = db_path_of(&dir);
        let held = WriterLease::try_acquire(Path::new(&path), Role::Index, &kb).unwrap();
        let created = Connection::open(&path).unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 0);

        let err = open_read_only_err(&path, &kb);
        let busy = err
            .downcast_ref::<OneTimeWriteBusy>()
            .unwrap_or_else(|| panic!("not OneTimeWriteBusy: {err:#}"));
        assert_eq!(busy.purpose, OneTimeWrite::SchemaUpgrade);
        assert_eq!(
            std::fs::metadata(&path).unwrap().len(),
            0,
            "the reader wrote the header"
        );

        // The writer's own init goes through, with no SQLite error left by the reader.
        drop(created);
        let w = Database::open_writer(&path, &held).unwrap();
        w.verify_embedding_meta(MODEL, DIM).unwrap();
        drop(w);
        drop(held);

        let r = Database::open_read_only(&path, &kb).unwrap();
        assert_eq!(query_only_flag(&r), 1);
        assert_eq!(
            r.read_embedding_meta().unwrap(),
            Some((MODEL.to_string(), DIM))
        );
    }

    #[test]
    fn open_read_only_rereads_once_when_the_holder_created_the_missing_db_meanwhile() {
        // I42 for a missing DB.
        let dir = TempDir::new("ro-missing-reread");
        let kb = kb_of(&dir);
        let path = db_path_of(&dir);
        let held: Mutex<Option<WriterLease>> = Mutex::new(None);
        let before_lease = || {
            assert!(!any_db_file(&path), "the first attempt created the DB file");
            let lease = WriterLease::try_acquire(Path::new(&path), Role::Index, &kb).unwrap();
            drop(Database::open_writer(&path, &lease).unwrap());
            *held.lock().unwrap() = Some(lease);
        };
        let r = Database::open_read_only_with(&path, &kb, &before_lease).unwrap();
        assert!(
            held.lock().unwrap().is_some(),
            "before_lease was not called"
        );
        assert_eq!(query_only_flag(&r), 1);
    }

    #[test]
    fn open_read_only_on_a_directory_fails_without_waiting_for_the_lease() {
        let dir = TempDir::new("ro-dir");
        let kb = kb_of(&dir);
        let path = dir.path().join("a-directory");
        std::fs::create_dir_all(&path).unwrap();
        let path = path.to_str().unwrap().to_string();
        let err = open_read_only_err(&path, &kb);
        assert!(err.downcast_ref::<OneTimeWriteBusy>().is_none(), "{err:#}");
        assert!(
            format!("{err:#}").contains("failed to open database"),
            "{err:#}"
        );
        assert!(!WriterLease::lock_path_for(Path::new(&path)).exists());
    }

    #[test]
    fn open_read_only_migrates_a_legacy_db_under_a_short_lease() {
        // T-L10.
        let dir = TempDir::new("ro-legacy");
        let kb = kb_of(&dir);
        let path = db_path_of(&dir);
        make_legacy_db(&path);
        let r = Database::open_read_only(&path, &kb).unwrap();
        assert!(has_size_bytes(&path));
        assert_eq!(query_only_flag(&r), 1);
        assert!(lease_is_free(&path, &kb));
    }

    #[test]
    fn open_read_only_on_a_legacy_db_fails_naming_the_holder_while_a_writer_holds_the_lease() {
        // T-L10.
        let dir = TempDir::new("ro-legacy-held");
        let kb = kb_of(&dir);
        let path = db_path_of(&dir);
        make_legacy_db(&path);
        let _held = WriterLease::try_acquire(Path::new(&path), Role::Index, &kb).unwrap();
        let err = open_read_only_err(&path, &kb);
        let busy = err
            .downcast_ref::<OneTimeWriteBusy>()
            .unwrap_or_else(|| panic!("not OneTimeWriteBusy: {err:#}"));
        assert_eq!(busy.purpose, OneTimeWrite::SchemaUpgrade);
        let msg = format!("{err:#}");
        assert!(msg.contains("needs a schema upgrade"), "{msg}");
        assert!(
            msg.contains(&format!("pid {} per", std::process::id())),
            "{msg}"
        );
        assert!(msg.contains("(index)"), "{msg}");
        assert!(!has_size_bytes(&path));
    }

    #[test]
    fn open_read_only_rereads_once_when_the_holder_migrated_meanwhile() {
        // T-L10 / I42.
        let dir = TempDir::new("ro-legacy-reread");
        let kb = kb_of(&dir);
        let path = db_path_of(&dir);
        make_legacy_db(&path);
        let held: Mutex<Option<WriterLease>> = Mutex::new(None);
        let before_lease = || {
            let lease = WriterLease::try_acquire(Path::new(&path), Role::Index, &kb).unwrap();
            drop(Database::open(&path).unwrap());
            *held.lock().unwrap() = Some(lease);
        };
        let r = Database::open_read_only_with(&path, &kb, &before_lease).unwrap();
        assert!(
            held.lock().unwrap().is_some(),
            "before_lease was not called"
        );
        assert_eq!(query_only_flag(&r), 1);
        assert!(has_size_bytes(&path));
    }

    #[test]
    fn open_read_only_reports_a_corrupt_file_as_corrupt() {
        // spec §5.5 step 3.
        let dir = TempDir::new("ro-corrupt");
        let kb = kb_of(&dir);
        let path = db_path_of(&dir);
        std::fs::write(&path, b"not a sqlite database").unwrap();
        let err = open_read_only_err(&path, &kb);
        assert!(
            err.downcast_ref::<CorruptDatabase>().is_some(),
            "not CorruptDatabase: {err:#}"
        );
    }

    /// (local Codex round 3) A reader's one-time write on a database path
    /// that is not UTF-8 stops before the lease, with `NotUtf8` as the root,
    /// and creates nothing beside that path; the write itself never runs.
    #[test]
    fn a_one_time_write_on_a_path_that_is_not_utf8_stops_before_the_lease() {
        let dir = TempDir::new("not-utf8");
        let kb = kb_of(&dir);
        let path = db_path_of(&dir);
        make_metaless_db(&path);
        let bad = dir.path().join(crate::writer_lease::non_utf8_name());
        std::fs::create_dir_all(&bad).unwrap();
        let bad_db = bad.join(".groove.db");
        let r = Database::open_read_only(&path, &kb).unwrap();
        let errors = [
            r.write_once_under_lease(&bad_db, &kb, OneTimeWrite::SchemaUpgrade, |_| {
                panic!("the one-time write ran without a lease")
            }),
            r.verify_embedding_meta_or_record(&bad_db, &kb, MODEL, DIM),
        ];
        for e in errors {
            let e = e.expect_err("a one-time write on a path that is not UTF-8");
            assert!(
                matches!(
                    e.downcast_ref::<LeaseError>(),
                    Some(LeaseError::NotUtf8 { .. })
                ),
                "NotUtf8 is the root: {e:#}"
            );
            let shown = format!("{e:#}");
            assert!(shown.is_ascii(), "non-ASCII error: {shown}");
        }
        assert_eq!(
            std::fs::read_dir(&bad).unwrap().count(),
            0,
            "nothing is created beside the path that is not UTF-8"
        );
        assert!(r.read_embedding_meta().unwrap().is_none());
        assert_eq!(query_only_flag(&r), 1);
    }

    #[test]
    fn verify_or_record_records_the_meta_under_a_short_lease() {
        let dir = TempDir::new("vr-record");
        let kb = kb_of(&dir);
        let path = db_path_of(&dir);
        make_metaless_db(&path);
        let r = Database::open_read_only(&path, &kb).unwrap();
        r.verify_embedding_meta_or_record(Path::new(&path), &kb, MODEL, DIM)
            .unwrap();
        assert_eq!(
            r.read_embedding_meta().unwrap(),
            Some((MODEL.to_string(), DIM))
        );
        assert_eq!(query_only_flag(&r), 1);
        assert!(lease_is_free(&path, &kb));
    }

    #[test]
    fn verify_or_record_fails_naming_the_embedding_model_while_a_writer_holds_the_lease() {
        // T-L10, the meta case.
        let dir = TempDir::new("vr-held");
        let kb = kb_of(&dir);
        let path = db_path_of(&dir);
        make_metaless_db(&path);
        let r = Database::open_read_only(&path, &kb).unwrap();
        let _held = WriterLease::try_acquire(Path::new(&path), Role::Index, &kb).unwrap();
        let err = r
            .verify_embedding_meta_or_record(Path::new(&path), &kb, MODEL, DIM)
            .unwrap_err();
        let busy = err
            .downcast_ref::<OneTimeWriteBusy>()
            .unwrap_or_else(|| panic!("not OneTimeWriteBusy: {err:#}"));
        assert_eq!(busy.purpose, OneTimeWrite::EmbeddingMeta);
        let msg = format!("{err:#}");
        assert!(msg.contains("no embedding model recorded"), "{msg}");
        assert!(!msg.contains("schema upgrade"), "{msg}");
        assert_eq!(query_only_flag(&r), 1);
    }

    #[test]
    fn verify_or_record_rereads_once_when_the_holder_recorded_the_meta_meanwhile() {
        // I42, the meta side.
        let dir = TempDir::new("vr-reread");
        let kb = kb_of(&dir);
        let path = db_path_of(&dir);
        make_metaless_db(&path);
        let r = Database::open_read_only(&path, &kb).unwrap();
        let held: Mutex<Option<WriterLease>> = Mutex::new(None);
        let before_lease = || {
            let lease = WriterLease::try_acquire(Path::new(&path), Role::Index, &kb).unwrap();
            let w = Database::open(&path).unwrap();
            w.verify_embedding_meta(MODEL, DIM).unwrap();
            drop(w);
            *held.lock().unwrap() = Some(lease);
        };
        r.verify_embedding_meta_or_record_with(Path::new(&path), &kb, MODEL, DIM, &before_lease)
            .unwrap();
        assert!(
            held.lock().unwrap().is_some(),
            "before_lease was not called"
        );
        assert_eq!(query_only_flag(&r), 1);
    }

    #[test]
    fn verify_or_record_reports_a_model_mismatch_when_the_holder_recorded_another_model_meanwhile()
    {
        // I42: the re-read fails with something other than a refusal.
        let dir = TempDir::new("vr-reread-mismatch");
        let kb = kb_of(&dir);
        let path = db_path_of(&dir);
        make_metaless_db(&path);
        let r = Database::open_read_only(&path, &kb).unwrap();
        let held: Mutex<Option<WriterLease>> = Mutex::new(None);
        let before_lease = || {
            let lease = WriterLease::try_acquire(Path::new(&path), Role::Index, &kb).unwrap();
            let w = Database::open(&path).unwrap();
            w.verify_embedding_meta("some-other-model", 384).unwrap();
            drop(w);
            *held.lock().unwrap() = Some(lease);
        };
        let err = r
            .verify_embedding_meta_or_record_with(Path::new(&path), &kb, MODEL, DIM, &before_lease)
            .unwrap_err();
        assert!(
            format!("{err:#}").contains("embedding model mismatch"),
            "{err:#}"
        );
        assert!(err.downcast_ref::<OneTimeWriteBusy>().is_none());
    }

    #[test]
    fn verify_or_record_still_reports_a_model_mismatch() {
        let dir = TempDir::new("vr-mismatch");
        let kb = kb_of(&dir);
        let path = db_path_of(&dir);
        {
            let w = Database::open(&path).unwrap();
            w.verify_embedding_meta("some-other-model", 384).unwrap();
        }
        let _held = WriterLease::try_acquire(Path::new(&path), Role::Index, &kb).unwrap();
        let r = Database::open_read_only(&path, &kb).unwrap();
        let t0 = Instant::now();
        let err = r
            .verify_embedding_meta_or_record(Path::new(&path), &kb, MODEL, DIM)
            .unwrap_err();
        assert!(
            format!("{err:#}").contains("embedding model mismatch"),
            "{err:#}"
        );
        assert!(err.downcast_ref::<OneTimeWriteBusy>().is_none());
        assert!(t0.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn verify_or_record_on_a_recorded_db_answers_while_a_writer_holds_the_lease() {
        // Review Focus 1: search / graph / eval / tune against a running writer.
        let dir = TempDir::new("vr-recorded-held");
        let kb = kb_of(&dir);
        let (path, _) = fixture(&dir);
        let _held = WriterLease::try_acquire(Path::new(&path), Role::Serve, &kb).unwrap();
        let t0 = Instant::now();
        let r = Database::open_read_only(&path, &kb).unwrap();
        r.verify_embedding_meta_or_record(Path::new(&path), &kb, MODEL, DIM)
            .unwrap();
        assert!(t0.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn one_time_write_busy_display_is_ascii() {
        // diagnostics_stay_ascii does not scan write! / writeln!, and this
        // Display reaches stderr through main's `Error: {:?}`.
        let dir = TempDir::new("busy-ascii");
        let db = dir.path().join(".groove.db");
        assert!(
            db.display().to_string().is_ascii(),
            "the temp path itself must be ASCII for this test to mean anything: {}",
            db.display()
        );
        let holder = HolderInfo {
            pid: 4242,
            role: Role::Index,
            started_at: "2026-10-29T00:00:00+00:00".to_string(),
            version: "0.0.0".to_string(),
            kb_path: dir.path().join("kb").display().to_string(),
            heartbeat_at: None,
            watching: None,
        };
        for purpose in [
            OneTimeWrite::SchemaUpgrade,
            OneTimeWrite::EmbeddingMeta,
            OneTimeWrite::ContextMode,
        ] {
            for holder in [Some(holder.clone()), None] {
                let s = OneTimeWriteBusy {
                    purpose,
                    db_path: db.clone(),
                    holder,
                }
                .to_string();
                assert!(s.is_ascii(), "non-ASCII display: {s}");
                assert!(!s.starts_with("error:"), "{s}");
            }
        }
    }

    #[test]
    fn write_once_under_lease_turns_query_only_back_on_even_when_f_fails() {
        let dir = TempDir::new("write-once-fail");
        let kb = kb_of(&dir);
        let (path, _) = fixture(&dir);
        let r = Database::open_read_only(&path, &kb).unwrap();
        let err = r
            .write_once_under_lease(
                Path::new(&path),
                &kb,
                OneTimeWrite::EmbeddingMeta,
                |db| -> Result<()> {
                    assert_eq!(query_only_flag(db), 0);
                    anyhow::bail!("f failed on purpose")
                },
            )
            .unwrap_err();
        assert!(
            format!("{err:#}").contains("f failed on purpose"),
            "{err:#}"
        );
        assert_eq!(query_only_flag(&r), 1);
        assert!(lease_is_free(&path, &kb));
    }

    #[test]
    fn write_once_under_lease_turns_query_only_back_on_before_the_lease_goes_when_f_panics() {
        // Local Codex review: a panic in `f` that the caller catches must not
        // leave a writable connection behind a released lease.
        let dir = TempDir::new("write-once-panic");
        let kb = kb_of(&dir);
        let (path, _) = fixture(&dir);
        let before = snapshot(&path);
        let r = Database::open_read_only(&path, &kb).unwrap();
        let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            r.write_once_under_lease(
                Path::new(&path),
                &kb,
                OneTimeWrite::EmbeddingMeta,
                |db| -> Result<()> {
                    assert_eq!(query_only_flag(db), 0);
                    panic!("f panicked on purpose")
                },
            )
        }));
        assert!(caught.is_err(), "f's panic did not reach catch_unwind");
        assert_eq!(query_only_flag(&r), 1);
        let e = upsert(&r, "after-panic.md").unwrap_err();
        assert!(is_query_only_refusal(&r.conn, &e), "{e:#}");
        assert!(lease_is_free(&path, &kb));
        drop(r);
        assert_eq!(snapshot(&path), before);
    }

    #[test]
    fn two_readers_opening_a_legacy_db_at_once_both_succeed() {
        // Review Focus 2.
        let dir = TempDir::new("ro-legacy-two");
        let kb = kb_of(&dir);
        let path = db_path_of(&dir);
        make_legacy_db(&path);
        let barrier = Arc::new(Barrier::new(2));
        let handles: Vec<_> = (0..2)
            .map(|_| {
                let (barrier, path, kb) = (barrier.clone(), path.clone(), kb.clone());
                std::thread::spawn(move || {
                    barrier.wait();
                    Database::open_read_only(&path, &kb).map(|db| query_only_flag(&db))
                })
            })
            .collect();
        for h in handles {
            assert_eq!(h.join().unwrap().unwrap(), 1);
        }
        assert!(has_size_bytes(&path));
    }

    #[test]
    fn the_query_only_refusals_hold_on_an_open_read_only_connection() {
        // T-L6's writes, on an `open_read_only` connection.
        let dir = TempDir::new("t-l6-ro");
        let kb = kb_of(&dir);
        let (path, doc) = fixture(&dir);
        let before = snapshot(&path);
        let r = Database::open_read_only(&path, &kb).unwrap();

        let w1 = upsert(&r, "b.md").unwrap_err();
        let w2 = r.delete_document("a.md").unwrap_err();
        let w3 = chunk(&r, doc, 1).unwrap_err();
        let w4 = anyhow::Error::from(
            r.conn
                .execute(
                    "INSERT INTO fts_chunks (rowid, heading, context, content) \
                     VALUES (999, 'h', 'c', 'x')",
                    [],
                )
                .unwrap_err(),
        );
        let w5 = anyhow::Error::from(
            r.conn
                .execute(
                    "INSERT INTO vec_chunks (chunk_id, embedding) VALUES (999, ?1)",
                    [serde_json::to_string(&vec![0.2f32; 384]).unwrap()],
                )
                .unwrap_err(),
        );

        for e in [w1, w2, w3, w4, w5] {
            assert!(is_query_only_refusal(&r.conn, &e), "{e:#}");
            assert_eq!(sqlite_failure(&e).extended_code, 8, "{e:#}");
        }
        assert!(!r.conn.is_readonly(MAIN).unwrap());
        drop(r);
        assert_eq!(snapshot(&path), before);
    }

    // -- Local Codex pre-push round 4: a symlink repointed under the lease --

    /// Point the file symlink `link` at `target`, replacing what it pointed at.
    /// False when the test should skip (see
    /// [`crate::writer_lease::file_symlink_or_skip`]).
    fn point_link(link: &Path, target: &Path) -> bool {
        if std::fs::symlink_metadata(link).is_ok() {
            std::fs::remove_file(link).unwrap();
        }
        crate::writer_lease::file_symlink_or_skip(target, link)
    }

    fn document_count(path: &Path) -> i64 {
        let db = Database::open(path.to_str().unwrap()).unwrap();
        db.conn
            .query_row("SELECT count(*) FROM documents", [], |r| r.get(0))
            .unwrap()
    }

    /// `<dir>/a.db` and `<dir>/b.db`, both made by `make`, and the path of a
    /// symlink `<dir>/alias.db` (not yet made).
    fn two_dbs_and_an_alias(dir: &TempDir, make: fn(&str)) -> (PathBuf, PathBuf, PathBuf) {
        let a = dir.path().join("a.db");
        let b = dir.path().join("b.db");
        make(a.to_str().unwrap());
        make(b.to_str().unwrap());
        (a, b, dir.path().join("alias.db"))
    }

    #[test]
    fn the_writer_opens_the_file_its_lease_locked_after_the_symlink_is_repointed() {
        // Local Codex pre-push round 4 [medium]: the deterministic interleaving
        // is acquire -> repoint -> open.
        let dir = TempDir::new("repointed-writer");
        let kb = kb_of(&dir);
        let (a, b, alias) = two_dbs_and_an_alias(&dir, |p| drop(Database::open(p).unwrap()));
        if !point_link(&alias, &a) {
            return;
        }
        let alias_str = alias.to_str().unwrap();
        let lease = WriterLease::try_acquire(&alias, Role::Index, &kb).unwrap();
        assert!(point_link(&alias, &b));

        let w = Database::open_writer(alias_str, &lease).unwrap();
        upsert(&w, "a.md").unwrap();
        drop(w);
        let (w, replaced) = Database::open_or_replace_corrupt_with(alias_str, &lease).unwrap();
        assert!(!replaced);
        upsert(&w, "b.md").unwrap();
        drop(w);

        assert_eq!(document_count(&a), 2, "the writer did not write a.db");
        assert_eq!(
            document_count(&b),
            0,
            "the writer wrote b.db, outside its lease"
        );
        // b.db's lock is free: another process could take it and write b.db.
        assert!(lease_is_free(b.to_str().unwrap(), &kb));
        drop(lease);
    }

    #[test]
    fn a_one_time_write_refuses_when_the_symlink_was_repointed_after_the_reader_opened() {
        // The reader's sibling of the round 4 finding: the reader opened the
        // file before the lease was taken.
        let dir = TempDir::new("repointed-reader");
        let kb = kb_of(&dir);
        let (a, b, alias) = two_dbs_and_an_alias(&dir, make_metaless_db);
        if !point_link(&alias, &a) {
            return;
        }
        let r = Database::open_read_only(alias.to_str().unwrap(), &kb).unwrap();
        let before_lease = || assert!(point_link(&alias, &b));
        let err = r
            .verify_embedding_meta_or_record_with(&alias, &kb, MODEL, DIM, &before_lease)
            .unwrap_err();
        assert!(format!("{err:#}").contains("now resolves to"), "{err:#}");
        assert!(err.downcast_ref::<OneTimeWriteBusy>().is_none(), "{err:#}");
        assert_eq!(query_only_flag(&r), 1);
        drop(r);
        for db in [&a, &b] {
            let db = Database::open(db.to_str().unwrap()).unwrap();
            assert!(db.read_embedding_meta().unwrap().is_none());
        }
    }

    #[test]
    fn the_writer_stays_in_its_directory_after_a_directory_link_above_the_db_is_repointed() {
        // Local Codex pre-push, second round of this push [high]: the
        // deterministic interleaving is acquire -> repoint the directory -> open.
        // `a` has no database yet (a first index); `b` has one.
        use crate::writer_lease::DirLink;
        for kind in DirLink::all() {
            let dir = TempDir::new("dirrepointed-writer");
            let kb = kb_of(&dir);
            let (a, b) = (dir.path().join("a"), dir.path().join("b"));
            std::fs::create_dir_all(&a).unwrap();
            std::fs::create_dir_all(&b).unwrap();
            let (a_db, b_db) = (a.join(".groove.db"), b.join(".groove.db"));
            drop(Database::open(b_db.to_str().unwrap()).unwrap());
            let alias = dir.path().join("alias");
            if !kind.point(&alias, &a) {
                continue;
            }
            let alias_db = alias.join(".groove.db");
            let alias_str = alias_db.to_str().unwrap();
            let lease = WriterLease::try_acquire(&alias_db, Role::Index, &kb).unwrap();
            assert!(kind.point(&alias, &b));

            let w = Database::open_writer(alias_str, &lease).unwrap();
            upsert(&w, "a.md").unwrap();
            drop(w);
            let (w, replaced) = Database::open_or_replace_corrupt_with(alias_str, &lease).unwrap();
            assert!(!replaced);
            upsert(&w, "b.md").unwrap();
            drop(w);

            assert_eq!(
                document_count(&a_db),
                2,
                "{kind:?}: the writer did not write a"
            );
            assert_eq!(
                document_count(&b_db),
                0,
                "{kind:?}: the writer wrote b, outside its lease"
            );
            // b's lock is free: another process could take it and write b.
            assert!(lease_is_free(b_db.to_str().unwrap(), &kb), "{kind:?}");
            drop(lease);
        }
    }

    #[test]
    fn a_one_time_write_refuses_when_a_directory_link_was_repointed_after_the_reader_opened() {
        use crate::writer_lease::DirLink;
        for kind in DirLink::all() {
            let dir = TempDir::new("dirrepointed-reader");
            let kb = kb_of(&dir);
            let (a, b) = (dir.path().join("a"), dir.path().join("b"));
            for d in [&a, &b] {
                std::fs::create_dir_all(d).unwrap();
                make_metaless_db(d.join(".groove.db").to_str().unwrap());
            }
            let alias = dir.path().join("alias");
            if !kind.point(&alias, &a) {
                continue;
            }
            let alias_db = alias.join(".groove.db");
            let r = Database::open_read_only(alias_db.to_str().unwrap(), &kb).unwrap();
            let before_lease = || assert!(kind.point(&alias, &b));
            let err = r
                .verify_embedding_meta_or_record_with(&alias_db, &kb, MODEL, DIM, &before_lease)
                .unwrap_err();
            assert!(
                format!("{err:#}").contains("now resolves to"),
                "{kind:?}: {err:#}"
            );
            drop(r);
            for d in [&a, &b] {
                let db = Database::open(d.join(".groove.db").to_str().unwrap()).unwrap();
                assert!(db.read_embedding_meta().unwrap().is_none(), "{kind:?}");
            }
        }
    }

    /// Whether SQLite opens a database through a verbatim `\\?\C:\...` path, the
    /// form `std::fs::canonicalize` returns on Windows, and keeps its `-wal` and
    /// `-shm` beside it: the writer lease keeps that form when it cannot drop the
    /// prefix (see [`crate::writer_lease`]).
    #[cfg(windows)]
    #[test]
    fn a_verbatim_drive_path_opens_with_its_wal_and_shm_beside_it() {
        let dir = TempDir::new("verbatim");
        let db_path = std::fs::canonicalize(dir.path())
            .unwrap()
            .join(".groove.db");
        let path = db_path.to_str().unwrap();
        assert!(path.starts_with(r"\\?\"), "{path}");
        let db = Database::open(path).unwrap();
        upsert(&db, "a.md").unwrap();
        for suffix in ["-wal", "-shm"] {
            assert!(
                dir.path().join(format!(".groove.db{suffix}")).exists(),
                "no {suffix} beside {path}"
            );
        }
        drop(db);
        assert_eq!(document_count(&dir.path().join(".groove.db")), 1);
    }

    #[test]
    fn a_reader_records_the_meta_in_the_symlinks_target() {
        // The same path without the repoint still writes, and writes the target.
        let dir = TempDir::new("symlinked-reader");
        let kb = kb_of(&dir);
        let (a, _b, alias) = two_dbs_and_an_alias(&dir, make_metaless_db);
        if !point_link(&alias, &a) {
            return;
        }
        let r = Database::open_read_only(alias.to_str().unwrap(), &kb).unwrap();
        r.verify_embedding_meta_or_record(&alias, &kb, MODEL, DIM)
            .unwrap();
        drop(r);
        let db = Database::open(a.to_str().unwrap()).unwrap();
        assert_eq!(
            db.read_embedding_meta().unwrap(),
            Some((MODEL.to_string(), DIM))
        );
    }
}
