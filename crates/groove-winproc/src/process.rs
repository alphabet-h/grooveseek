//! The Windows half: open a pid once, check it, terminate it through the same
//! handle.
//!
//! ## Why a handle pins the identity
//!
//! The pid is resolved exactly once, by `OpenProcess`. Everything after that —
//! reading the image name and the creation time, terminating, waiting — goes
//! through the returned handle, and a process handle stays valid, and refers
//! to the same process object, until it is closed, even after that process
//! has exited. A pid reused after the open cannot be reached through it. A pid
//! reused *before* the open is what the image-name check, and given a
//! [`crate::ServeRecord`] the creation-time check, reject.
//!
//! Errors are worded in ASCII with the Win32 code, not the OS's localized
//! message: `groove` keeps stderr ASCII (AGENTS.md).

use crate::{StopOutcome, image_file_name_matches};
use std::io;
use std::os::windows::io::AsRawHandle;
use std::time::Duration;
use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_ACCESS_DENIED, ERROR_INVALID_PARAMETER, FILETIME, GetLastError, HANDLE,
    WAIT_FAILED, WAIT_OBJECT_0,
};
use windows_sys::Win32::System::Threading::{
    GetExitCodeProcess, GetProcessTimes, OpenProcess, PROCESS_ACCESS_RIGHTS, PROCESS_NAME_WIN32,
    PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE, PROCESS_TERMINATE,
    QueryFullProcessImageNameW, TerminateProcess, WaitForSingleObject,
};

/// How long to wait for a terminated process to exit. Termination is
/// asynchronous and cannot complete until pending I/O is cancelled, so this is
/// generous; exceeding it means something is genuinely wrong.
pub const EXIT_WAIT: Duration = Duration::from_secs(10);

/// `GetExitCodeProcess` reports this while the process runs.
const STILL_ACTIVE: u32 = 259;

/// Closes the handle on every return.
struct Owned(HANDLE);

impl Drop for Owned {
    fn drop(&mut self) {
        // SAFETY: the handle came from OpenProcess and is closed once here.
        unsafe {
            CloseHandle(self.0);
        }
    }
}

fn os_error(what: &str, pid: u32, code: u32) -> io::Error {
    let kind = io::Error::from_raw_os_error(code as i32).kind();
    let detail = if code == ERROR_ACCESS_DENIED {
        "access denied, Win32 error 5".to_string()
    } else {
        format!("Win32 error {code}")
    };
    io::Error::new(kind, format!("{what} failed for pid {pid} ({detail})"))
}

/// The creation time of `process`, in `FILETIME` units
/// (100 ns intervals since 1601-01-01 UTC). `groove-svc` reads it from the
/// handle of the child it just spawned (`std::process::Child`'s handle has
/// every access right), for the [`crate::ServeRecord`].
pub fn creation_time_of(process: &impl AsRawHandle) -> io::Result<u64> {
    creation_time(process.as_raw_handle() as HANDLE)
        .ok_or_else(|| io::Error::other("GetProcessTimes failed for the child's handle"))
}

fn creation_time(handle: HANDLE) -> Option<u64> {
    let zero = || FILETIME {
        dwLowDateTime: 0,
        dwHighDateTime: 0,
    };
    let (mut created, mut exited, mut kernel, mut user) = (zero(), zero(), zero(), zero());
    // SAFETY: the four out-pointers are live locals; the handle carries at
    // least PROCESS_QUERY_LIMITED_INFORMATION, which GetProcessTimes accepts.
    let ok = unsafe { GetProcessTimes(handle, &mut created, &mut exited, &mut kernel, &mut user) };
    (ok != 0).then(|| (u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime))
}

fn image_path(handle: &Owned) -> Option<String> {
    // MAX_PATH is not a real bound for this API; a generous buffer avoids a
    // retry loop.
    let mut buf = vec![0u16; 32_768];
    let mut len = buf.len() as u32;
    // SAFETY: `buf` outlives the call and `len` is its length.
    let ok = unsafe {
        QueryFullProcessImageNameW(handle.0, PROCESS_NAME_WIN32, buf.as_mut_ptr(), &mut len)
    };
    (ok != 0).then(|| String::from_utf16_lossy(&buf[..len as usize]))
}

fn has_exited(handle: &Owned) -> bool {
    let mut code = 0u32;
    // SAFETY: the out-pointer is a live local.
    let ok = unsafe { GetExitCodeProcess(handle.0, &mut code) };
    ok != 0 && code != STILL_ACTIVE
}

/// Terminate `pid`, but only if its executable is named `expected_file_name`.
/// The tray's stop when it has no record (the pid comes from the status
/// endpoint): [`stop_process_if_matches`] without the creation-time check.
pub fn stop_process_if_image_matches(
    pid: u32,
    expected_file_name: &str,
) -> io::Result<StopOutcome> {
    stop_process_if_matches(pid, expected_file_name, None)
}

/// Open `pid` with `access` and check that it is the process meant: its
/// executable is named `expected_file_name`, it has not exited, and, given
/// `created` (a [`crate::ServeRecord`]'s creation time), it was created at
/// exactly that time. `Ok(Err(outcome))` says why it is not (never
/// [`StopOutcome::Terminated`]). The one identity check, shared by
/// [`stop_process_if_matches`] and [`is_recorded_process_running`].
fn open_checked(
    pid: u32,
    expected_file_name: &str,
    created: Option<u64>,
    access: PROCESS_ACCESS_RIGHTS,
) -> io::Result<Result<Owned, StopOutcome>> {
    // SAFETY: plain call; the result is checked for null.
    let raw = unsafe { OpenProcess(access, 0, pid) };
    if raw.is_null() {
        // SAFETY: plain call, right after the failing one.
        let code = unsafe { GetLastError() };
        if code == ERROR_INVALID_PARAMETER {
            // The documented answer for a pid that names no process.
            return Ok(Err(StopOutcome::NotRunning));
        }
        return Err(os_error("OpenProcess", pid, code));
    }
    let handle = Owned(raw);

    let Some(image) = image_path(&handle) else {
        // The process can exit between the open and the query; the handle
        // still answers whether it did.
        if has_exited(&handle) {
            return Ok(Err(StopOutcome::NotRunning));
        }
        return Err(io::Error::other(format!(
            "QueryFullProcessImageNameW failed for pid {pid}"
        )));
    };
    if !image_file_name_matches(&image, expected_file_name) {
        return Ok(Err(StopOutcome::NotOurProcess));
    }
    if let Some(expected) = created {
        match creation_time(handle.0) {
            Some(actual) if actual == expected => {}
            Some(_) => return Ok(Err(StopOutcome::Reused)),
            None if has_exited(&handle) => return Ok(Err(StopOutcome::NotRunning)),
            None => {
                return Err(io::Error::other(format!(
                    "GetProcessTimes failed for pid {pid}"
                )));
            }
        }
    }
    if has_exited(&handle) {
        return Ok(Err(StopOutcome::NotRunning));
    }
    Ok(Ok(handle))
}

/// Whether the process `groove-svc` recorded (`pid`, created at `created`) is
/// still running as `expected_file_name`: the check
/// [`stop_process_if_matches`] makes before it terminates, without the
/// terminate. An `Err` means it could not be found out (a process with that
/// pid exists but cannot be examined).
pub fn is_recorded_process_running(
    pid: u32,
    expected_file_name: &str,
    created: u64,
) -> io::Result<bool> {
    let checked = open_checked(
        pid,
        expected_file_name,
        Some(created),
        PROCESS_QUERY_LIMITED_INFORMATION,
    )?;
    Ok(checked.is_ok())
}

/// Terminate `pid` when its executable is named `expected_file_name` and,
/// given `created` (a [`crate::ServeRecord`]'s creation time), the process
/// was created at exactly that time; then wait up to [`EXIT_WAIT`] for it to
/// exit.
///
/// Returns what happened rather than a bare success, so a caller can tell
/// "stopped it" from "nothing to stop" from "that pid is not the one meant".
/// Errors are reserved for real failures: a process that exists but cannot be
/// opened, examined or terminated (access denied, typically), or one that does
/// not exit in time.
pub fn stop_process_if_matches(
    pid: u32,
    expected_file_name: &str,
    created: Option<u64>,
) -> io::Result<StopOutcome> {
    let handle = match open_checked(
        pid,
        expected_file_name,
        created,
        PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_TERMINATE | PROCESS_SYNCHRONIZE,
    )? {
        Ok(handle) => handle,
        Err(outcome) => return Ok(outcome),
    };

    // SAFETY: the handle carries PROCESS_TERMINATE.
    if unsafe { TerminateProcess(handle.0, 1) } == 0 {
        // SAFETY: plain call, right after the failing one.
        let code = unsafe { GetLastError() };
        // Terminating a process that has already exited fails with
        // ERROR_ACCESS_DENIED through a still-open handle, the same code as a
        // genuine permission failure: the exit status tells them apart.
        if code == ERROR_ACCESS_DENIED && has_exited(&handle) {
            return Ok(StopOutcome::NotRunning);
        }
        return Err(os_error("TerminateProcess", pid, code));
    }
    // Termination is asynchronous; waiting on the handle turns `Terminated`
    // into an observation rather than a request.
    // SAFETY: the handle carries PROCESS_SYNCHRONIZE.
    let waited = unsafe { WaitForSingleObject(handle.0, EXIT_WAIT.as_millis() as u32) };
    if waited == WAIT_FAILED {
        // SAFETY: plain call, right after the failing one.
        let code = unsafe { GetLastError() };
        return Err(os_error(
            "WaitForSingleObject after TerminateProcess",
            pid,
            code,
        ));
    }
    if waited != WAIT_OBJECT_0 {
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            format!(
                "pid {pid} did not exit within {}s of being terminated",
                EXIT_WAIT.as_secs()
            ),
        ));
    }
    Ok(StopOutcome::Terminated)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};
    use std::process::{Child, Command, Stdio};

    fn system_ping() -> PathBuf {
        let root = std::env::var("SystemRoot").unwrap_or_else(|_| "C:\\Windows".into());
        Path::new(&root).join("System32").join("ping.exe")
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

    /// `ping.exe` copied under the name `groove.exe`, removed on drop.
    struct Disguised(PathBuf);

    impl Disguised {
        fn new() -> Self {
            use std::sync::atomic::{AtomicU64, Ordering};
            static SEQ: AtomicU64 = AtomicU64::new(0);
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            let dir = std::env::temp_dir().join(format!(
                "groove-winproc-stop-{}-{nanos}-{}",
                std::process::id(),
                SEQ.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::copy(system_ping(), dir.join("groove.exe")).unwrap();
            Self(dir)
        }
        fn exe(&self) -> PathBuf {
            self.0.join("groove.exe")
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
    fn another_image_is_left_running() {
        let mut child = spawn(&system_ping());
        let outcome = stop_process_if_matches(child.id(), "groove.exe", None).unwrap();
        assert_eq!(outcome, StopOutcome::NotOurProcess);
        assert!(child.try_wait().unwrap().is_none(), "it must still run");
        reap(child);
    }

    /// The reuse guard: the right image, but not the recorded creation time.
    #[test]
    fn a_different_creation_time_is_a_reused_pid_and_left_running() {
        let d = Disguised::new();
        let mut child = spawn(&d.exe());
        let created = creation_time_of(&child).unwrap();
        let outcome = stop_process_if_matches(child.id(), "groove.exe", Some(created - 1)).unwrap();
        assert_eq!(outcome, StopOutcome::Reused);
        assert!(child.try_wait().unwrap().is_none(), "it must still run");
        reap(child);
    }

    /// The recorded process, with its creation time read the way groove-svc
    /// reads it, is stopped and has exited when the call returns.
    #[test]
    fn the_recorded_process_is_stopped_and_has_exited() {
        let d = Disguised::new();
        let mut child = spawn(&d.exe());
        let pid = child.id();
        let created = creation_time_of(&child).unwrap();
        assert_eq!(
            stop_process_if_matches(pid, "groove.exe", Some(created)).unwrap(),
            StopOutcome::Terminated
        );
        assert_eq!(
            stop_process_if_matches(pid, "groove.exe", Some(created)).unwrap(),
            StopOutcome::NotRunning,
            "gone the moment Terminated is returned"
        );
        assert!(child.try_wait().unwrap().is_some());
    }

    /// Without a record, the image alone decides (the tray's status-endpoint
    /// path).
    #[test]
    fn without_a_record_the_image_alone_decides() {
        let d = Disguised::new();
        let mut child = spawn(&d.exe());
        assert_eq!(
            stop_process_if_image_matches(child.id(), "groove.exe").unwrap(),
            StopOutcome::Terminated
        );
        child.wait().unwrap();
    }

    /// A pid no process has: not a multiple of 4, so never one on Windows.
    #[test]
    fn a_pid_no_process_has_is_not_running() {
        assert_eq!(
            stop_process_if_matches(u32::MAX - 2, "groove.exe", Some(1)).unwrap(),
            StopOutcome::NotRunning
        );
    }

    /// The check without the terminate: true only for the recorded process,
    /// and it leaves it running.
    #[test]
    fn the_running_check_matches_only_the_recorded_process_and_stops_nothing() {
        let d = Disguised::new();
        let mut child = spawn(&d.exe());
        let created = creation_time_of(&child).unwrap();
        assert!(is_recorded_process_running(child.id(), "groove.exe", created).unwrap());
        assert!(
            child.try_wait().unwrap().is_none(),
            "the check stops nothing"
        );
        assert!(!is_recorded_process_running(child.id(), "groove.exe", created - 1).unwrap());
        assert!(!is_recorded_process_running(child.id(), "ping.exe", created).unwrap());
        reap(child);
        assert!(!is_recorded_process_running(u32::MAX - 2, "groove.exe", created).unwrap());
    }
}
