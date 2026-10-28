//! `groove index` under the writer lease (ADR-0031): it takes the lease on
//! `<db>.writer-lock` before it touches the database or the model, exits 3
//! naming the holder when another process holds it, waits with `--wait`, and
//! lets the operating system release the lease when the process ends.
//!
//! Every child `groove` runs under [`common::embed_mock::hermetic`] with an
//! empty model cache that is checked after it exits: nothing here may
//! download a model. Where a test needs to see that `groove index` got past
//! the lease, the database already records another model, so the run stops
//! at `embedding model mismatch` (exit 1) before any model is loaded.

use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use grooveseek::db::Database;
use grooveseek::writer_lease::{LeaseError, Role, WriterLease};

mod common;
use common::ansi::strip_ansi;
use common::embed_mock::{assert_dir_empty, hermetic};
use common::mcp::grooveseek_bin;
use common::production::{fn_ranges, production_lines};
use common::temp::{TempKbLayout, TempRoot};

/// The env var naming the database [`lease_holder_child`] takes the lease on.
const HOLDER_DB: &str = "GROOVE_TEST_LEASE_HOLDER_DB";
/// The env var naming the knowledge base [`lease_holder_child`] records.
const HOLDER_KB: &str = "GROOVE_TEST_LEASE_HOLDER_KB";
/// What [`lease_holder_child`] prints once it holds the lease.
const HELD: &str = "LEASE_HELD";

fn layout(prefix: &str) -> TempKbLayout {
    let kb = TempKbLayout::new(prefix);
    kb.write(
        "note.md",
        "---\ntitle: Note\n---\n\n# Note\n\nEnough content here to stand as a chunk.\n",
    );
    kb
}

fn db_of(kb: &TempKbLayout) -> std::path::PathBuf {
    kb.root().join(".groove.db")
}

fn wal_of(kb: &TempKbLayout) -> std::path::PathBuf {
    kb.root().join(".groove.db-wal")
}

/// A database that records a model no run here uses, so `groove index`
/// without `--force` stops at the meta check right after the lease.
fn place_other_model_db(kb: &TempKbLayout) {
    let db = Database::open(&db_of(kb).to_string_lossy()).expect("open the db");
    db.verify_embedding_meta("some-other-model", 384)
        .expect("record the other model");
}

/// The `groove index` command for `kb`, hermetic, with `cache` as the model cache.
fn index_cmd(kb: &TempKbLayout, cache: &Path, extra: &[&str]) -> Command {
    let mut cmd = Command::new(grooveseek_bin());
    cmd.arg("index").arg("--kb-path").arg(kb.kb()).args(extra);
    hermetic(&mut cmd, cache);
    cmd
}

/// Run `groove index` and return its exit code and stderr (ANSI stripped).
fn run_index(kb: &TempKbLayout, cache: &Path, extra: &[&str]) -> (i32, String) {
    let out = index_cmd(kb, cache, extra).output().expect("spawn groove");
    assert_dir_empty(cache);
    (
        out.status.code().unwrap_or(-1),
        strip_ansi(&String::from_utf8_lossy(&out.stderr)),
    )
}

fn hold(kb: &TempKbLayout, role: Role, holder_kb: &Path) -> WriterLease {
    WriterLease::try_acquire(&db_of(kb), role, holder_kb).expect("take the lease in the test")
}

/// Poll `try_acquire` every 250 ms until it succeeds or `limit` passes.
fn acquire_within(db: &Path, kb: &Path, limit: Duration) -> Option<WriterLease> {
    let deadline = Instant::now() + limit;
    loop {
        match WriterLease::try_acquire(db, Role::Index, kb) {
            Ok(lease) => return Some(lease),
            Err(LeaseError::Busy { .. }) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(250));
            }
            Err(LeaseError::Busy { .. }) => return None,
            Err(e) => panic!("try_acquire: {e}"),
        }
    }
}

#[test]
fn index_exits_3_naming_the_server_that_holds_the_lease() {
    let kb = layout("groove-lease-serve");
    let cache = TempRoot::new("groove-lease-serve-cache");
    let _lease = hold(&kb, Role::Serve, kb.kb());
    let (code, stderr) = run_index(&kb, cache.path(), &[]);
    assert_eq!(code, 3, "{stderr}");
    assert!(
        stderr.contains(&format!("pid {}", std::process::id()))
            && stderr.contains("rebuild_index")
            && stderr.contains("--wait"),
        "the holder, the server's tool and the wait flag are named: {stderr}"
    );
    assert!(
        !db_of(&kb).exists() && !wal_of(&kb).exists(),
        "a refused run creates neither the database nor its WAL: {stderr}"
    );
}

#[test]
fn index_exits_3_without_naming_rebuild_index_when_a_one_off_run_holds_it() {
    let kb = layout("groove-lease-oneoff");
    let cache = TempRoot::new("groove-lease-oneoff-cache");
    let _lease = hold(&kb, Role::Index, kb.kb());
    let (code, stderr) = run_index(&kb, cache.path(), &[]);
    assert_eq!(code, 3, "{stderr}");
    assert!(
        stderr.contains("one-off")
            && stderr.contains("--wait")
            && stderr.contains("can run once that run exits"),
        "a one-off holder is waited for, not rebuilt through: {stderr}"
    );
    assert!(
        !stderr.contains("rebuild_index"),
        "a one-off run has no rebuild_index to offer: {stderr}"
    );
    assert!(!db_of(&kb).exists() && !wal_of(&kb).exists());
}

#[test]
fn index_exits_3_warning_against_another_kbs_rebuild_index() {
    let kb = layout("groove-lease-otherkb");
    let cache = TempRoot::new("groove-lease-otherkb-cache");
    let other = kb.root().join("other");
    std::fs::create_dir_all(&other).expect("create the other kb");
    let _lease = hold(&kb, Role::Serve, &other);
    let (code, stderr) = run_index(&kb, cache.path(), &[]);
    assert_eq!(code, 3, "{stderr}");
    assert!(
        stderr.contains("different knowledge base")
            && stderr.contains("would remove this knowledge base's rows"),
        "another kb's server is not offered as the rebuild: {stderr}"
    );
    assert!(!db_of(&kb).exists() && !wal_of(&kb).exists());
}

#[test]
fn index_exits_3_leaves_an_existing_index_untouched() {
    let kb = layout("groove-lease-existing");
    let cache = TempRoot::new("groove-lease-existing-cache");
    place_other_model_db(&kb);
    let before = std::fs::read(db_of(&kb)).expect("read the db");
    let wal_before = wal_of(&kb).exists();
    let _lease = hold(&kb, Role::Serve, kb.kb());
    let (code, stderr) = run_index(&kb, cache.path(), &[]);
    assert_eq!(code, 3, "{stderr}");
    assert_eq!(
        std::fs::read(db_of(&kb)).expect("read the db again"),
        before,
        "a refused run leaves the database bytes as they were"
    );
    assert_eq!(
        wal_of(&kb).exists(),
        wal_before,
        "a refused run neither creates nor removes the WAL"
    );
}

#[test]
fn index_releases_the_lease_when_it_exits() {
    let kb = layout("groove-lease-release");
    let cache = TempRoot::new("groove-lease-release-cache");
    place_other_model_db(&kb);
    let (code, stderr) = run_index(&kb, cache.path(), &[]);
    assert_eq!(
        code, 1,
        "the run got past the lease to the meta check: {stderr}"
    );
    assert!(stderr.contains("embedding model mismatch"), "{stderr}");
    assert!(
        acquire_within(&db_of(&kb), kb.kb(), Duration::from_secs(10)).is_some(),
        "the lease is free once groove index has exited"
    );
}

#[test]
fn index_wait_takes_the_lease_once_the_holder_lets_go() {
    let kb = layout("groove-lease-wait");
    let cache = TempRoot::new("groove-lease-wait-cache");
    place_other_model_db(&kb);
    let lease = hold(&kb, Role::Serve, kb.kb());
    let mut child = index_cmd(&kb, cache.path(), &["--wait", "10"])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn groove");
    let stderr = child.stderr.take().expect("piped stderr");
    let (tx, rx) = mpsc::channel::<String>();
    let reader = std::thread::spawn(move || {
        let mut all = Vec::new();
        for line in BufReader::new(stderr).lines() {
            let Ok(line) = line else { break };
            let line = strip_ansi(&line);
            let _ = tx.send(line.clone());
            all.push(line);
        }
        all.join("\n")
    });
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(left) {
            Ok(line) if line.starts_with("waiting for the writer of") => break,
            Ok(_) => {}
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                panic!("no waiting line from groove index --wait: {e}");
            }
        }
    }
    drop(lease);
    let status = child.wait().expect("wait for groove");
    let stderr = reader.join().expect("join the stderr reader");
    assert_dir_empty(cache.path());
    assert_ne!(
        status.code(),
        Some(3),
        "the wait ended with the lease: {stderr}"
    );
    assert_eq!(status.code(), Some(1), "{stderr}");
    assert!(
        stderr.contains("embedding model mismatch"),
        "the run got past the lease to the meta check: {stderr}"
    );
}

#[test]
fn index_wait_gives_up_with_exit_3_after_the_deadline() {
    let kb = layout("groove-lease-deadline");
    let cache = TempRoot::new("groove-lease-deadline-cache");
    let _lease = hold(&kb, Role::Serve, kb.kb());
    let (code, stderr) = run_index(&kb, cache.path(), &["--wait", "1"]);
    assert_eq!(code, 3, "{stderr}");
    let lines: Vec<&str> = stderr.lines().collect();
    let wait_at = lines
        .iter()
        .position(|l| l.starts_with("waiting for the writer of"))
        .expect("waiting line");
    let busy_at = lines
        .iter()
        .position(|l| l.starts_with("error: waited 1s; another groove process"))
        .expect("busy line");
    assert!(
        wait_at < busy_at,
        "the waiting line comes before the refusal: {stderr}"
    );
    assert!(!db_of(&kb).exists() && !wal_of(&kb).exists());
}

/// The holder for [`the_os_releases_the_lease_of_a_killed_process`]. Reads
/// `GROOVE_TEST_LEASE_HOLDER_DB` (the database) and
/// `GROOVE_TEST_LEASE_HOLDER_KB` (the knowledge base it records); without
/// both it returns at once, so `cargo test -- --ignored` passes over it.
#[test]
#[ignore]
fn lease_holder_child() {
    let (Some(db), Some(kb)) = (std::env::var_os(HOLDER_DB), std::env::var_os(HOLDER_KB)) else {
        return;
    };
    let _lease = WriterLease::try_acquire(Path::new(&db), Role::Index, Path::new(&kb))
        .expect("the holder child takes the lease");
    let mut out = std::io::stdout();
    out.write_all(format!("\n{HELD}\n").as_bytes())
        .expect("write the held marker");
    out.flush().expect("flush the held marker");
    std::thread::sleep(Duration::from_secs(120));
}

/// Kills and reaps the child when dropped, so a failed assert does not
/// leave a process holding the lock file open.
struct KillOnDrop(Child);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn the_os_releases_the_lease_of_a_killed_process() {
    let kb = layout("groove-lease-killed");
    let db = db_of(&kb);
    let mut child = Command::new(std::env::current_exe().expect("this test binary"))
        .args([
            "lease_holder_child",
            "--exact",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(HOLDER_DB, &db)
        .env(HOLDER_KB, kb.kb())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn the holder child");
    let stdout = child.stdout.take().expect("piped stdout");
    // Declared after `kb`, so it is dropped first: the child is gone before
    // the temp directory is removed.
    let mut guard = KillOnDrop(child);

    let (tx, rx) = mpsc::channel::<String>();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else { break };
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(left) {
            Ok(line) if line.trim() == HELD => break,
            Ok(_) => {} // libtest's own lines
            Err(e) => panic!("the holder child never reported {HELD}: {e}"),
        }
    }
    assert!(
        matches!(
            WriterLease::try_acquire(&db, Role::Index, kb.kb()),
            Err(LeaseError::Busy { .. })
        ),
        "the child holds the lease"
    );

    let killed_at = Instant::now();
    guard.0.kill().expect("kill the holder child");
    guard.0.wait().expect("reap the holder child");
    let lease = acquire_within(&db, kb.kb(), Duration::from_secs(10));
    eprintln!(
        "lease free {:?} after the holder was killed",
        killed_at.elapsed()
    );
    assert!(
        lease.is_some(),
        "the operating system released the lease of the killed process"
    );
}

#[test]
fn writer_opens_in_main_are_only_inside_index_with_lease() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/main.rs");
    let src = std::fs::read_to_string(&path).expect("read main.rs");
    let prod = production_lines(&src);
    let ranges: Vec<_> = fn_ranges(&src, "fn index_with_lease(")
        .into_iter()
        .filter(|(s, _)| prod.iter().any(|(n, _)| n == s))
        .collect();
    assert_eq!(
        ranges.len(),
        1,
        "exactly one production fn index_with_lease"
    );
    let (s, e) = ranges[0];
    let opens: Vec<usize> = prod
        .iter()
        .filter(|(_, l)| {
            l.contains("Database::open_writer(")
                || l.contains("Database::open_or_replace_corrupt_with(")
        })
        .map(|(n, _)| *n)
        .collect();
    assert!(
        opens.iter().any(|n| (s..=e).contains(n)),
        "index_with_lease opens the writer itself ({s}..{e})"
    );
    assert!(
        opens.iter().all(|n| (s..=e).contains(n)),
        "{opens:?} outside {s}..{e}"
    );
    let from_fn: String = src.lines().skip(s - 1).collect::<Vec<_>>().join("\n");
    let sig = &from_fn[..from_fn.find('{').unwrap()];
    assert!(
        !sig.contains("Database"),
        "index_with_lease must not hand a Database out: {sig}"
    );
}

#[test]
fn production_lines_drops_cfg_test_modules_and_comments() {
    let src = concat!(
        "fn keep() {}\n",
        "#[cfg(test)]\n",
        "fn also_keep() {}\n",
        "// x\n",
        "    // Database::open(x)\n",
        "let s = \"// x\";\n",
        "#[cfg(test)]\n",
        "mod t {\n",
        "    fn gone() {}\n",
        "    mod inner {\n",
        "    }\n",
        "}\n",
        "#[cfg(test)]\n",
        "pub(crate) mod u {\n",
        "    fn gone_too() {}\n",
        "}\n",
        "fn after() {}\n",
    );
    let got: Vec<(usize, &str)> = production_lines(src);
    assert_eq!(
        got,
        vec![
            (1, "fn keep() {}"),
            (2, "#[cfg(test)]"),
            (3, "fn also_keep() {}"),
            (6, "let s = \"// x\";"),
            (17, "fn after() {}"),
        ]
    );
}

#[test]
fn fn_ranges_ends_at_the_matching_indent() {
    let src = concat!(
        "fn outer() {\n",
        "    fn target(a: u8) {\n",
        "        if a > 0 {\n",
        "        }\n",
        "    }\n",
        "}\n",
        "fn target(b: u8) {\n",
        "    let _ = b;\n",
        "}\n",
        "fn target(unclosed: u8) {\n",
    );
    assert_eq!(fn_ranges(src, "fn target("), vec![(2, 5), (7, 9)]);
}

/// Run `groove <args> --kb-path <kb>` hermetically and return its exit code,
/// stdout and stderr (ANSI stripped).
fn run_groove(kb: &TempKbLayout, cache: &Path, args: &[&str]) -> (i32, String, String) {
    let mut cmd = Command::new(grooveseek_bin());
    cmd.args(args).arg("--kb-path").arg(kb.kb());
    hermetic(&mut cmd, cache);
    let out = cmd.output().expect("spawn groove");
    assert_dir_empty(cache);
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        strip_ansi(&String::from_utf8_lossy(&out.stderr)),
    )
}

#[test]
fn search_on_a_corrupt_index_still_reports_it_as_corrupt() {
    let kb = layout("groove-lease-corrupt-search");
    let cache = TempRoot::new("groove-lease-corrupt-search-cache");
    std::fs::write(db_of(&kb), b"not a sqlite database").expect("write garbage db");
    let (code, stdout, stderr) = run_groove(&kb, cache.path(), &["search", "anything"]);
    assert_ne!(code, 0, "stdout: {stdout}\nstderr: {stderr}");
    assert!(
        stderr.contains("is not a usable SQLite database") && !stderr.contains("schema upgrade"),
        "a corrupt file is named as corrupt, not as one waiting for an upgrade: {stderr}"
    );
}

#[test]
fn status_and_doctor_answer_while_another_process_holds_the_lease() {
    let kb = layout("groove-lease-readers");
    let cache = TempRoot::new("groove-lease-readers-cache");
    {
        let db = Database::open(&db_of(&kb).to_string_lossy()).expect("open the db");
        db.verify_embedding_meta("bge-small-en-v1.5", 384)
            .expect("record the model");
        db.upsert_document("note.md", Some("Note"), None, None, None, &[], None, "h", 0)
            .expect("insert a document");
    }
    let _lease = hold(&kb, Role::Serve, kb.kb());

    let (code, stdout, stderr) = run_groove(&kb, cache.path(), &["status"]);
    assert_eq!(code, 0, "status reads past the lease: {stderr}");
    assert!(
        stdout.contains("Documents: 1"),
        "stdout: {stdout}\nstderr: {stderr}"
    );

    let (code, stdout, stderr) = run_groove(&kb, cache.path(), &["doctor"]);
    assert_ne!(
        code, 2,
        "doctor could look while the lease was held: stdout: {stdout}\nstderr: {stderr}"
    );
}

#[test]
fn status_on_a_legacy_index_fails_naming_the_writer_that_holds_the_lease() {
    let kb = layout("groove-lease-legacy");
    let cache = TempRoot::new("groove-lease-legacy-cache");
    let db = db_of(&kb);
    {
        let opened = Database::open(&db.to_string_lossy()).expect("open the db");
        opened
            .verify_embedding_meta("bge-small-en-v1.5", 384)
            .expect("record the model");
    }
    {
        // `ALTER TABLE documents DROP COLUMN size_bytes` fails on this schema
        // (the comment inside its CREATE statement), so rebuild the table
        // without the column, as `make_legacy_db` in src/db/lease.rs does.
        let conn = rusqlite::Connection::open(&db).expect("open the db raw");
        conn.execute_batch(
            "DROP TABLE documents;
             CREATE TABLE documents (
                id INTEGER PRIMARY KEY AUTOINCREMENT, path TEXT UNIQUE NOT NULL,
                title TEXT, topic TEXT, category TEXT, depth TEXT, tags TEXT,
                date TEXT, content_hash TEXT NOT NULL, last_indexed TEXT NOT NULL
             );",
        )
        .expect("rebuild documents without size_bytes");
    }
    let _lease = hold(&kb, Role::Index, kb.kb());

    let (code, stdout, stderr) = run_groove(&kb, cache.path(), &["status"]);
    assert_eq!(code, 1, "stdout: {stdout}\nstderr: {stderr}");
    assert!(
        stderr.contains("needs a schema upgrade")
            && stderr.contains(&format!("pid {} per", std::process::id())),
        "the upgrade and the writer holding it are named: {stderr}"
    );
}
