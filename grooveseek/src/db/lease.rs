//! The database side of the writer lease: writer opens that take a held
//! [`crate::writer_lease::WriterLease`] as proof, and the `PRAGMA query_only`
//! refusal that keeps a reader from writing. Why: ADR-0031.

use super::*;
use crate::writer_lease::WriterLease;

impl Database {
    /// [`Database::open`] for a process that holds the writer lease. The
    /// lease is not read; asking for it is how the caller shows it holds one.
    pub fn open_writer(path: &str, lease: &WriterLease) -> Result<Self> {
        let _ = lease;
        Self::open(path)
    }

    /// [`Database::open_or_replace_corrupt`] for a process that holds the
    /// writer lease. The lease is not read, as in [`Database::open_writer`].
    pub fn open_or_replace_corrupt_with(path: &str, lease: &WriterLease) -> Result<(Self, bool)> {
        let _ = lease;
        Self::open_or_replace_corrupt(path)
    }

    /// Set `PRAGMA query_only` on this connection: while it is on, every
    /// write fails with `SQLITE_READONLY`.
    pub(crate) fn set_query_only(&self, on: bool) -> Result<()> {
        self.conn.pragma_update(None, "query_only", on)?;
        Ok(())
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

    /// A reader as Task 5's `open_read_only` will make it: opened, then
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
}
