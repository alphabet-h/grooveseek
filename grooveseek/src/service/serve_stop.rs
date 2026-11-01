//! Stopping a service's `groove serve` on Windows (kuriya #650).
//!
//! The scheduled task's Action is `groove-svc.exe`, which detach-spawns
//! `groove.exe serve` and exits at once (`crates/groove-svc/src/main.rs`).
//! Task Scheduler therefore sees a task that has finished, in state `Ready`,
//! and `schtasks /End` has nothing to end: the server keeps running after
//! `groove service uninstall`, and `--purge` used to wait out its whole wait
//! for a writer lock that server never let go of.
//!
//! So the server is found and stopped by its process, the way the tray stops
//! it (`crates/groove-tray/src/daemon.rs`, `process.rs`), with the pid taken
//! from the index's writer lock rather than from the status endpoint:
//!
//! - The pid is the one `<db>.writer.json` records, believed only while the
//!   writer lock beside it is actually held ([`BareWriterLock::try_hold`]
//!   answers [`LeaseError::Busy`]) and the record names a server
//!   ([`Role::Serve`] or [`Role::Rebuild`], the two roles a server's lease
//!   takes). A free lock means no server writes this index, whatever an old
//!   record says.
//! - Before anything is terminated, the process behind the pid must be
//!   `groove.exe` **and** must have been created no later than the record's
//!   `started_at` ([`RECORD_CLOCK_SLACK`] aside). Pids are unique among live
//!   processes, so a process that already existed when the record was
//!   written under its pid is the one that wrote it; a process created after
//!   that is a reused pid, and is left alone even when it is another groove.
//! - Everything after `OpenProcess` goes through the one handle, so a pid
//!   reused after the checks cannot be reached.
//!
//! The status endpoint (`/api/admin/status`, `daemon.pid`) is not used: it
//! no longer names the knowledge base (`KbInfo` in `server.rs`), so the pid
//! it reports could be any groove server listening on the configured port,
//! not necessarily the one writing this service's index; and this crate has
//! no HTTP client on the service path to ask it with.

use crate::writer_lease::{BareWriterLock, HolderInfo, LeaseError, Role};
use std::path::Path;
use std::time::{Duration, Instant, SystemTime};

/// File name of the server's executable. A pid is terminated only when its
/// image is this program.
pub(crate) const SERVE_IMAGE: &str = "groove.exe";

/// How long, after the server has exited, to wait for the OS to release its
/// writer lock. Windows releases a terminated process's file locks
/// asynchronously, so the lock can be busy for a moment after the process
/// handle is signalled.
pub(crate) const SERVE_LOCK_RELEASE_WAIT: Duration = Duration::from_secs(10);

/// How long to wait for a terminated process to exit. Termination is
/// asynchronous and waits for pending I/O to be cancelled.
const EXIT_WAIT: Duration = Duration::from_secs(10);

/// How much later than the record's `started_at` a process may have been
/// created and still be taken as the record's writer: room for the wall
/// clock being stepped between process start and lease, not for pid reuse
/// (a holder would have to die within this long of taking the lease, leave
/// its record behind, and have its pid reused).
const RECORD_CLOCK_SLACK: Duration = Duration::from_secs(2);

/// How often the writer lock is tried while waiting for it to be released.
const RELEASE_POLL: Duration = Duration::from_millis(250);

/// What [`stop_serve_of`] did.
#[derive(Debug)]
pub(crate) enum ServeStop {
    /// No server holds the index's writer lock: the lock is free, cannot be
    /// taken at all, or is held by a process whose record does not name a
    /// server (a `groove index` run, or no record).
    NoServe,
    /// The server named by the record was terminated, has exited, and the
    /// writer lock is free.
    Stopped { pid: u32 },
    /// A server holds the lock and was not stopped (or was, and the lock is
    /// still held); `why` says which, in ASCII.
    NotStopped { holder: HolderInfo, why: String },
}

/// Whether a writer-lock record names a process to stop.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Candidate {
    /// The lock is held, but `<db>.writer.json` could not be read.
    NoRecord,
    /// The record names a process that is not a server.
    NotAServe(Role),
    /// The record names this very process.
    ThisProcess,
    /// The record names a server, to be checked against its process before
    /// it is stopped.
    Serve,
}

/// Classify the record of a held writer lock.
pub(crate) fn candidate(holder: Option<&HolderInfo>, own_pid: u32) -> Candidate {
    let Some(h) = holder else {
        return Candidate::NoRecord;
    };
    if !matches!(h.role, Role::Serve | Role::Rebuild) {
        return Candidate::NotAServe(h.role);
    }
    if h.pid == own_pid {
        return Candidate::ThisProcess;
    }
    Candidate::Serve
}

/// What [`terminate_if_recorded`] did with a pid.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Termination {
    /// The process was terminated and observed to exit.
    Terminated,
    /// No process has the pid, or it had already exited.
    NotRunning,
    /// The pid belongs to another program (its image file name), left alone.
    OtherImage(String),
    /// The pid belongs to a process created after the record was written: a
    /// reused pid, left alone.
    CreatedAfterRecord,
}

/// Parse a record's `started_at` (RFC 3339).
pub(crate) fn record_time(started_at: &str) -> Option<SystemTime> {
    let at = chrono::DateTime::parse_from_rfc3339(started_at).ok()?;
    Some(SystemTime::from(at.with_timezone(&chrono::Utc)))
}

/// Whether a process created at `created` can be the one that wrote a record
/// at `recorded` ([`RECORD_CLOCK_SLACK`]).
pub(crate) fn created_in_time(created: SystemTime, recorded: SystemTime) -> bool {
    match created.duration_since(recorded) {
        Ok(later_by) => later_by <= RECORD_CLOCK_SLACK,
        Err(_) => true,
    }
}

/// Find the server holding `resolved`'s writer lock, stop it if its process
/// checks out, and wait up to `release_wait` for the lock to be let go.
/// `resolved` is the database path resolved whole
/// ([`crate::writer_lease::WriterLease::resolved_db_path`]).
pub(crate) fn stop_serve_of(resolved: &Path, release_wait: Duration) -> ServeStop {
    let holder = match BareWriterLock::try_hold(resolved) {
        Ok(_free) => return ServeStop::NoServe,
        Err(LeaseError::Busy { holder, .. }) => holder,
        // The lock file cannot be opened or locked: no holder is known.
        Err(_) => return ServeStop::NoServe,
    };
    let holder = match (candidate(holder.as_ref(), std::process::id()), holder) {
        (Candidate::Serve, Some(h)) => h,
        (Candidate::ThisProcess, Some(h)) => {
            let why = format!("pid {} is this process", h.pid);
            return ServeStop::NotStopped { holder: h, why };
        }
        _ => return ServeStop::NoServe,
    };
    let pid = holder.pid;
    let Some(recorded) = record_time(&holder.started_at) else {
        let why = format!(
            "the record's start time {:?} cannot be read, so pid {pid} cannot be confirmed \
             as the process that wrote it",
            holder.started_at
        );
        return ServeStop::NotStopped { holder, why };
    };
    let terminated = match terminate_if_recorded(pid, recorded, SERVE_IMAGE) {
        Ok(t) => t,
        Err(why) => return ServeStop::NotStopped { holder, why },
    };
    match terminated {
        Termination::Terminated | Termination::NotRunning => {}
        Termination::OtherImage(image) => {
            let why = format!(
                "pid {pid} is {image}, not {SERVE_IMAGE}: the record is stale, so it was left alone"
            );
            return ServeStop::NotStopped { holder, why };
        }
        Termination::CreatedAfterRecord => {
            let why = format!(
                "pid {pid} was started after the record was written: the pid has been \
                 reused, so it was left alone"
            );
            return ServeStop::NotStopped { holder, why };
        }
    }
    if wait_for_release(resolved, release_wait) {
        return ServeStop::Stopped { pid };
    }
    let why = match terminated {
        Termination::Terminated => format!(
            "pid {pid} was stopped, but the writer lock was still held {}s later",
            release_wait.as_secs()
        ),
        _ => format!(
            "pid {pid} is not running, but the writer lock was still held {}s later",
            release_wait.as_secs()
        ),
    };
    ServeStop::NotStopped { holder, why }
}

/// Try the writer lock until it is free (true) or `wait` has passed (false).
fn wait_for_release(resolved: &Path, wait: Duration) -> bool {
    let deadline = Instant::now() + wait;
    loop {
        match BareWriterLock::try_hold(resolved) {
            Ok(_free) => return true,
            Err(LeaseError::Busy { .. }) => {}
            // The lock file went away or cannot be opened: nobody holds it.
            Err(_) => return true,
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return false;
        }
        std::thread::sleep(left.min(RELEASE_POLL));
    }
}

pub(crate) use process::terminate_if_recorded;

mod process {
    use super::{EXIT_WAIT, Termination, created_in_time};
    use std::time::{Duration, SystemTime, UNIX_EPOCH};
    use windows_sys::Win32::Foundation::{
        CloseHandle, ERROR_ACCESS_DENIED, ERROR_INVALID_PARAMETER, FILETIME, GetLastError, HANDLE,
        WAIT_OBJECT_0,
    };
    use windows_sys::Win32::System::Threading::{
        GetExitCodeProcess, GetProcessTimes, OpenProcess, PROCESS_NAME_WIN32,
        PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE, PROCESS_TERMINATE,
        QueryFullProcessImageNameW, TerminateProcess, WaitForSingleObject,
    };

    /// `GetExitCodeProcess` reports this while the process runs.
    const STILL_ACTIVE: u32 = 259;

    /// 100 ns intervals between 1601-01-01 (FILETIME's epoch) and 1970-01-01.
    const FILETIME_UNIX_EPOCH: u64 = 116_444_736_000_000_000;

    /// Closes the handle on every return.
    struct Owned(HANDLE);

    impl Drop for Owned {
        fn drop(&mut self) {
            // SAFETY: the handle came from OpenProcess and is closed once.
            unsafe {
                CloseHandle(self.0);
            }
        }
    }

    /// Terminate `pid` when its image file name is `image` and it was
    /// created by the time `recorded` (the record's `started_at`), and wait
    /// for it to exit. An `Err` is a failure to find out or to stop it,
    /// worded in ASCII.
    pub(crate) fn terminate_if_recorded(
        pid: u32,
        recorded: SystemTime,
        image: &str,
    ) -> Result<Termination, String> {
        // SAFETY: plain call; the result is checked for null.
        let raw = unsafe {
            OpenProcess(
                PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_TERMINATE | PROCESS_SYNCHRONIZE,
                0,
                pid,
            )
        };
        if raw.is_null() {
            // SAFETY: plain call, right after the failing one.
            let code = unsafe { GetLastError() };
            if code == ERROR_INVALID_PARAMETER {
                // The documented answer for a pid that names no process.
                return Ok(Termination::NotRunning);
            }
            return Err(format!(
                "could not open pid {pid} to stop it ({})",
                win32_error(code)
            ));
        }
        let handle = Owned(raw);

        let path = match image_path(&handle) {
            Some(p) => p,
            None if has_exited(&handle) => return Ok(Termination::NotRunning),
            None => return Err(format!("could not read the image of pid {pid}")),
        };
        let name = std::path::Path::new(&path)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_string();
        if !name.eq_ignore_ascii_case(image) {
            // ASCII only on stderr (AGENTS.md); a non-ASCII name is escaped.
            return Ok(Termination::OtherImage(name.escape_default().to_string()));
        }
        match creation_time(&handle) {
            Some(created) if created_in_time(created, recorded) => {}
            Some(_) => return Ok(Termination::CreatedAfterRecord),
            None if has_exited(&handle) => return Ok(Termination::NotRunning),
            None => return Err(format!("could not read when pid {pid} was started")),
        }

        // SAFETY: the handle carries PROCESS_TERMINATE.
        if unsafe { TerminateProcess(handle.0, 1) } == 0 {
            // SAFETY: plain call, right after the failing one.
            let code = unsafe { GetLastError() };
            // Terminating a process that has already exited fails with
            // ERROR_ACCESS_DENIED through a still-open handle, the same code
            // as a real permission failure: the exit status tells them apart.
            if code == ERROR_ACCESS_DENIED && has_exited(&handle) {
                return Ok(Termination::NotRunning);
            }
            return Err(format!("could not stop pid {pid} ({})", win32_error(code)));
        }
        // SAFETY: the handle carries PROCESS_SYNCHRONIZE.
        let waited = unsafe { WaitForSingleObject(handle.0, EXIT_WAIT.as_millis() as u32) };
        if waited != WAIT_OBJECT_0 {
            return Err(format!(
                "pid {pid} did not exit within {}s of being stopped",
                EXIT_WAIT.as_secs()
            ));
        }
        Ok(Termination::Terminated)
    }

    fn win32_error(code: u32) -> String {
        if code == ERROR_ACCESS_DENIED {
            "access denied, Win32 error 5".to_string()
        } else {
            format!("Win32 error {code}")
        }
    }

    fn image_path(handle: &Owned) -> Option<String> {
        let mut buf = vec![0u16; 32_768];
        let mut len = buf.len() as u32;
        // SAFETY: `buf` outlives the call and `len` is its length.
        let ok = unsafe {
            QueryFullProcessImageNameW(handle.0, PROCESS_NAME_WIN32, buf.as_mut_ptr(), &mut len)
        };
        (ok != 0).then(|| String::from_utf16_lossy(&buf[..len as usize]))
    }

    fn creation_time(handle: &Owned) -> Option<SystemTime> {
        let zero = || FILETIME {
            dwLowDateTime: 0,
            dwHighDateTime: 0,
        };
        let (mut created, mut exited, mut kernel, mut user) = (zero(), zero(), zero(), zero());
        // SAFETY: the four out-pointers are live locals; the handle carries
        // PROCESS_QUERY_LIMITED_INFORMATION, which GetProcessTimes accepts.
        let ok =
            unsafe { GetProcessTimes(handle.0, &mut created, &mut exited, &mut kernel, &mut user) };
        if ok == 0 {
            return None;
        }
        let ticks = (u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime);
        let since_unix = ticks.checked_sub(FILETIME_UNIX_EPOCH)?;
        UNIX_EPOCH.checked_add(Duration::from_nanos(since_unix.saturating_mul(100)))
    }

    fn has_exited(handle: &Owned) -> bool {
        let mut code = 0u32;
        // SAFETY: the out-pointer is a live local.
        let ok = unsafe { GetExitCodeProcess(handle.0, &mut code) };
        ok != 0 && code != STILL_ACTIVE
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::{Child, Command, Stdio};

    fn record(pid: u32, role: Role, started_at: &str) -> HolderInfo {
        HolderInfo {
            pid,
            role,
            started_at: started_at.into(),
            version: "0.0.0".into(),
            kb_path: "C:\\kb".into(),
            heartbeat_at: None,
            watching: None,
        }
    }

    /// A long-lived, harmless process: `ping -t` loops until terminated.
    fn spawn(exe: &Path) -> Child {
        Command::new(exe)
            .args(["-t", "127.0.0.1"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn the stand-in")
    }

    fn system_ping() -> std::path::PathBuf {
        let root = std::env::var("SystemRoot").unwrap_or_else(|_| "C:\\Windows".into());
        Path::new(&root).join("System32").join("ping.exe")
    }

    /// `ping.exe` copied under the server's file name, in its own temp dir
    /// that is removed on drop.
    struct Disguised(std::path::PathBuf);

    impl Disguised {
        fn new() -> Self {
            let dir = crate::test_support::unique_temp_path("groove-650-stop");
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::copy(system_ping(), dir.join(SERVE_IMAGE)).unwrap();
            Self(dir)
        }
        fn exe(&self) -> std::path::PathBuf {
            self.0.join(SERVE_IMAGE)
        }
    }

    impl Drop for Disguised {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn reap(mut child: Child) {
        let _ = child.kill();
        let _ = child.wait();
    }

    #[test]
    fn only_a_servers_record_is_a_candidate() {
        let own = 4242;
        assert_eq!(candidate(None, own), Candidate::NoRecord);
        assert_eq!(
            candidate(Some(&record(8, Role::Index, "x")), own),
            Candidate::NotAServe(Role::Index)
        );
        assert_eq!(
            candidate(Some(&record(8, Role::Migration, "x")), own),
            Candidate::NotAServe(Role::Migration)
        );
        assert_eq!(
            candidate(Some(&record(own, Role::Serve, "x")), own),
            Candidate::ThisProcess
        );
        assert_eq!(
            candidate(Some(&record(8, Role::Serve, "x")), own),
            Candidate::Serve
        );
        assert_eq!(
            candidate(Some(&record(8, Role::Rebuild, "x")), own),
            Candidate::Serve
        );
    }

    #[test]
    fn a_process_created_after_the_record_did_not_write_it() {
        let recorded = record_time("2026-11-01T00:00:00Z").unwrap();
        assert!(created_in_time(
            recorded - Duration::from_secs(60),
            recorded
        ));
        assert!(created_in_time(recorded + RECORD_CLOCK_SLACK, recorded));
        assert!(!created_in_time(
            recorded + RECORD_CLOCK_SLACK + Duration::from_millis(1),
            recorded
        ));
        assert!(record_time("not a time").is_none());
    }

    /// The image guard: a pid that is some other program is left running.
    #[test]
    fn a_process_with_another_image_is_not_stopped() {
        let child = spawn(&system_ping());
        let outcome =
            terminate_if_recorded(child.id(), SystemTime::now(), SERVE_IMAGE).expect("no error");
        assert!(matches!(outcome, Termination::OtherImage(_)), "{outcome:?}");
        let mut child = child;
        assert!(child.try_wait().unwrap().is_none(), "it must still run");
        reap(child);
    }

    /// The reuse guard: a `groove.exe` started after the record was written
    /// is not the record's writer, and is left running.
    #[test]
    fn a_groove_started_after_the_record_is_not_stopped() {
        let d = Disguised::new();
        let mut child = spawn(&d.exe());
        let long_ago = SystemTime::now() - Duration::from_secs(3600);
        let outcome = terminate_if_recorded(child.id(), long_ago, SERVE_IMAGE).expect("no error");
        assert_eq!(outcome, Termination::CreatedAfterRecord);
        assert!(child.try_wait().unwrap().is_none(), "it must still run");
        reap(child);
    }

    /// A `groove.exe` that existed when the record was written is stopped,
    /// and has exited by the time the call returns.
    #[test]
    fn the_recorded_groove_is_stopped_and_has_exited() {
        let d = Disguised::new();
        let child = spawn(&d.exe());
        let pid = child.id();
        let outcome = terminate_if_recorded(pid, SystemTime::now(), SERVE_IMAGE).expect("no error");
        assert_eq!(outcome, Termination::Terminated);
        assert_eq!(
            terminate_if_recorded(pid, SystemTime::now(), SERVE_IMAGE).expect("no error"),
            Termination::NotRunning,
            "gone the moment Terminated is returned"
        );
        reap(child);
    }

    #[test]
    fn an_unassigned_pid_is_not_running() {
        assert_eq!(
            terminate_if_recorded(0x7FFF_FFF0, SystemTime::now(), SERVE_IMAGE).expect("no error"),
            Termination::NotRunning
        );
    }

    fn temp_db() -> (std::path::PathBuf, std::path::PathBuf) {
        let dir = crate::test_support::unique_temp_path("groove-650-db");
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join(".groove.db");
        std::fs::write(&db, b"main").unwrap();
        (dir.clone(), db)
    }

    fn write_record(db: &Path, h: &HolderInfo) {
        let mut info = db.as_os_str().to_os_string();
        info.push(".writer.json");
        std::fs::write(info, serde_json::to_vec(h).unwrap()).unwrap();
    }

    /// A record whose lock is free is not believed: nothing is stopped,
    /// even though the record names a running `groove.exe`.
    #[test]
    fn a_record_without_a_held_lock_stops_nothing() {
        let (dir, db) = temp_db();
        let d = Disguised::new();
        let mut child = spawn(&d.exe());
        write_record(
            &db,
            &record(child.id(), Role::Serve, &chrono::Utc::now().to_rfc3339()),
        );
        let outcome = stop_serve_of(&db, Duration::ZERO);
        assert!(matches!(outcome, ServeStop::NoServe), "{outcome:?}");
        assert!(child.try_wait().unwrap().is_none(), "it must still run");
        reap(child);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A held lock whose record names a process that is not groove: not
    /// stopped, and the reason says the record is stale.
    #[test]
    fn a_held_lock_with_a_stale_record_stops_nothing() {
        let (dir, db) = temp_db();
        let _held = BareWriterLock::try_hold(&db).expect("free");
        let mut child = spawn(&system_ping());
        write_record(
            &db,
            &record(child.id(), Role::Serve, &chrono::Utc::now().to_rfc3339()),
        );
        let outcome = stop_serve_of(&db, Duration::ZERO);
        match &outcome {
            ServeStop::NotStopped { holder, why } => {
                assert_eq!(holder.pid, child.id());
                assert!(why.contains("stale"), "{why}");
            }
            other => panic!("{other:?}"),
        }
        assert!(child.try_wait().unwrap().is_none(), "it must still run");
        reap(child);
        drop(_held);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// The recorded server is stopped, but the lock stays held (here by this
    /// test, standing in for a holder that is not the recorded process): the
    /// wait for its release is bounded and the outcome says so.
    #[test]
    fn a_lock_still_held_after_the_stop_is_reported_after_a_bounded_wait() {
        let (dir, db) = temp_db();
        let held = BareWriterLock::try_hold(&db).expect("free");
        let d = Disguised::new();
        let mut child = spawn(&d.exe());
        write_record(
            &db,
            &record(child.id(), Role::Serve, &chrono::Utc::now().to_rfc3339()),
        );
        let started = Instant::now();
        let outcome = stop_serve_of(&db, Duration::from_millis(600));
        assert!(started.elapsed() < Duration::from_secs(5), "bounded wait");
        match &outcome {
            ServeStop::NotStopped { why, .. } => {
                assert!(why.contains("was stopped"), "{why}");
                assert!(why.contains("still held"), "{why}");
            }
            other => panic!("{other:?}"),
        }
        assert!(
            child.try_wait().unwrap().is_some(),
            "the recorded groove was stopped"
        );
        drop(held);
        let _ = child.wait();
        let _ = std::fs::remove_dir_all(dir);
    }

    /// This process is never stopped, whatever the record says.
    #[test]
    fn a_record_naming_this_process_is_not_stopped() {
        let (dir, db) = temp_db();
        let held = BareWriterLock::try_hold(&db).expect("free");
        write_record(
            &db,
            &record(
                std::process::id(),
                Role::Serve,
                &chrono::Utc::now().to_rfc3339(),
            ),
        );
        let outcome = stop_serve_of(&db, Duration::ZERO);
        assert!(
            matches!(&outcome, ServeStop::NotStopped { why, .. } if why.contains("this process")),
            "{outcome:?}"
        );
        drop(held);
        let _ = std::fs::remove_dir_all(dir);
    }
}
