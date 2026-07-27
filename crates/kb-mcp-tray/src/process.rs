//! Native process control for stopping the kb-mcp daemon.
//!
//! ## Why not PowerShell
//!
//! This started as a generated `Stop-Process` script. Five review rounds on
//! PR #89 found five separate defects in it, and none of them were in the
//! logic — they were all in PowerShell's error and exit-code semantics:
//! `-ErrorAction SilentlyContinue` still exits 1, `try`/`catch` does not
//! change that, `Stop-Process -Id` re-resolves the number, `-InputObject`
//! re-resolves it too because `Process.Kill()` reopens by pid, and a denied
//! handle was indistinguishable from a missing process. Each fix opened the
//! next hole, which is the signal to change the approach rather than keep
//! patching.
//!
//! Doing it through the Win32 API removes the entire class. There is no script
//! string to get right, no exit code to launder, and the outcomes are an enum
//! the caller matches on instead of a number.
//!
//! ## Why a handle pins the identity
//!
//! The pid is resolved exactly once, by `OpenProcess`. Everything after that —
//! reading the image name, terminating — goes through the returned handle, and
//! [the handles are valid until closed, even after the process they represent
//! has been terminated][handles]. So the handle refers to one specific process
//! object for its whole lifetime; a pid recycled after we opened it cannot be
//! reached through it. The only remaining window is a pid recycled *before*
//! `OpenProcess`, and the image-name check rejects that.
//!
//! Note this does not rely on an open handle preventing pid reuse. Microsoft
//! documents that an identifier is valid "until the process has been
//! terminated" and says nothing about handles holding it reserved, so the
//! design deliberately does not depend on that.
//!
//! [handles]: https://learn.microsoft.com/en-us/windows/win32/procthread/process-handles-and-identifiers

use anyhow::{Context, Result, bail};
use std::time::Duration;
use windows::Win32::Foundation::{
    CloseHandle, ERROR_ACCESS_DENIED, ERROR_INVALID_PARAMETER, HANDLE, WAIT_OBJECT_0,
};
use windows::Win32::System::Threading::{
    GetExitCodeProcess, OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
    PROCESS_SYNCHRONIZE, PROCESS_TERMINATE, QueryFullProcessImageNameW, TerminateProcess,
    WaitForSingleObject,
};

/// `GetExitCodeProcess` reports this while the process is still running.
/// Defined here rather than imported so the meaning is visible at the use site.
const STILL_RUNNING_EXIT_CODE: u32 = 259;

/// How long to wait for a terminated process to actually go away. Termination
/// is asynchronous and cannot complete until pending I/O is cancelled, so this
/// is generous; exceeding it means something is genuinely wrong.
const EXIT_WAIT: Duration = Duration::from_secs(10);

/// What a stop attempt actually did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopOutcome {
    /// The verified process was terminated **and observed to exit**.
    Terminated,
    /// Nothing to stop: the pid does not resolve, or the process it referred
    /// to had already exited.
    NotRunning,
    /// The pid resolves to some other program, so it was left alone. Almost
    /// always a recycled pid from a stale status response.
    NotOurProcess,
}

/// RAII wrapper so every early return closes the handle.
struct OwnedHandle(HANDLE);

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        // Failure here is not actionable — the process exits shortly after
        // anyway, and there is nothing to retry.
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

/// True when `image_path`'s file name is `expected_file_name`, compared the way
/// Windows compares paths (case-insensitively).
pub fn image_file_name_matches(image_path: &str, expected_file_name: &str) -> bool {
    std::path::Path::new(image_path)
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.eq_ignore_ascii_case(expected_file_name))
}

/// Terminate `pid`, but only if its executable is named `expected_file_name`.
///
/// Returns what happened rather than a bare success, so the caller can tell
/// "stopped it" from "there was nothing to stop" from "that pid is somebody
/// else's" — the distinction the old script kept collapsing into exit 0.
///
/// Errors are reserved for genuine failures: a handle that cannot be opened
/// while the process exists, or a termination that fails on a process that is
/// still running. Those must not be reported as a successful stop.
pub fn stop_process_if_image_matches(pid: u32, expected_file_name: &str) -> Result<StopOutcome> {
    // PROCESS_QUERY_LIMITED_INFORMATION is the least privilege that satisfies
    // QueryFullProcessImageNameW; PROCESS_TERMINATE is what TerminateProcess
    // requires. Asking for both up front means the whole operation runs on one
    // handle.
    let handle = unsafe {
        OpenProcess(
            PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_TERMINATE | PROCESS_SYNCHRONIZE,
            false,
            pid,
        )
    };
    let handle = match handle {
        Ok(h) => OwnedHandle(h),
        Err(e) if e.code() == ERROR_INVALID_PARAMETER.to_hresult() => {
            // Documented result for a pid that does not identify a process.
            return Ok(StopOutcome::NotRunning);
        }
        Err(e) => {
            return Err(e).with_context(|| format!("OpenProcess failed for pid {pid}"));
        }
    };

    let image = match query_image_path(&handle) {
        Ok(path) => path,
        // The process can exit between the open and the query. Its handle stays
        // valid, so ask the handle whether it exited rather than guessing from
        // the error.
        Err(e) => {
            if has_exited(&handle)? {
                return Ok(StopOutcome::NotRunning);
            }
            return Err(e);
        }
    };
    if !image_file_name_matches(&image, expected_file_name) {
        return Ok(StopOutcome::NotOurProcess);
    }

    // SAFETY: `handle` was opened with PROCESS_TERMINATE above and is still
    // owned by this scope.
    match unsafe { TerminateProcess(handle.0, 1) } {
        Ok(()) => {
            // Termination is asynchronous: it "initiates termination and
            // returns immediately". Returning here would mean `Terminated`
            // only ever promised that a request was made, and the caller would
            // have to guess when it took effect. Waiting on the handle turns it
            // into an observation.
            wait_for_exit(&handle)?;
            Ok(StopOutcome::Terminated)
        }
        Err(e) => {
            // Microsoft documents that terminating an already-exited process
            // fails with ERROR_ACCESS_DENIED through a still-open handle — the
            // same code a genuine permission failure produces. The error alone
            // cannot tell them apart, so consult the exit status.
            if e.code() == ERROR_ACCESS_DENIED.to_hresult() && has_exited(&handle)? {
                return Ok(StopOutcome::NotRunning);
            }
            Err(e).with_context(|| format!("TerminateProcess failed for pid {pid}"))
        }
    }
}

/// Block until the process behind `handle` exits.
///
/// This is what lets the caller trust `Terminated` without polling an HTTP
/// endpoint and guessing (codex P1 round 6 on PR #89: a single failed status
/// request is not proof a daemon stopped, but a signalled process handle is).
fn wait_for_exit(handle: &OwnedHandle) -> Result<()> {
    // SAFETY: `handle` carries PROCESS_SYNCHRONIZE, which is what the wait
    // functions require.
    let waited = unsafe { WaitForSingleObject(handle.0, EXIT_WAIT.as_millis() as u32) };
    if waited != WAIT_OBJECT_0 {
        bail!(
            "process did not exit within {:?} after termination (wait returned {:?})",
            EXIT_WAIT,
            waited
        );
    }
    Ok(())
}

/// Full path of the executable backing `handle`.
fn query_image_path(handle: &OwnedHandle) -> Result<String> {
    // MAX_PATH is not a real bound for this API; long paths are common enough
    // that a generous buffer avoids a retry loop.
    let mut buf = vec![0u16; 32_768];
    let mut len = buf.len() as u32;
    // SAFETY: `buf` outlives the call and `len` describes it accurately.
    unsafe {
        QueryFullProcessImageNameW(
            handle.0,
            PROCESS_NAME_WIN32,
            windows::core::PWSTR(buf.as_mut_ptr()),
            &mut len,
        )
    }
    .context("QueryFullProcessImageNameW failed")?;
    Ok(String::from_utf16_lossy(&buf[..len as usize]))
}

/// Whether the process behind `handle` has already exited.
fn has_exited(handle: &OwnedHandle) -> Result<bool> {
    let mut code = 0u32;
    // SAFETY: `handle` carries PROCESS_QUERY_LIMITED_INFORMATION, which is
    // sufficient for GetExitCodeProcess.
    unsafe { GetExitCodeProcess(handle.0, &mut code) }.context("GetExitCodeProcess failed")?;
    Ok(code != STILL_RUNNING_EXIT_CODE)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::{Child, Command, Stdio};

    /// Spawn a long-lived process we are allowed to kill. `ping -t` loops
    /// until terminated and needs no files or ports.
    fn spawn_victim() -> Child {
        Command::new("ping.exe")
            .args(["-t", "127.0.0.1"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn ping")
    }

    #[test]
    fn image_file_name_matches_ignores_case_and_directories() {
        assert!(image_file_name_matches(
            "C:\\Users\\me\\.cargo\\bin\\kb-mcp.exe",
            "kb-mcp.exe"
        ));
        assert!(image_file_name_matches("C:\\a\\KB-MCP.EXE", "kb-mcp.exe"));
        assert!(!image_file_name_matches(
            "C:\\a\\kb-mcp-svc.exe",
            "kb-mcp.exe"
        ));
        assert!(!image_file_name_matches(
            "C:\\a\\kb-mcp.exe.bak",
            "kb-mcp.exe"
        ));
        assert!(!image_file_name_matches("", "kb-mcp.exe"));
    }

    /// A pid belonging to another program must be left strictly alone. This is
    /// the guard that stops a stale status response from killing whatever
    /// recycled the number.
    #[test]
    fn refuses_to_stop_a_process_with_a_different_image() {
        let mut victim = spawn_victim();
        let pid = victim.id();

        let outcome = stop_process_if_image_matches(pid, "kb-mcp.exe").expect("no error");
        assert_eq!(outcome, StopOutcome::NotOurProcess);
        assert!(
            victim.try_wait().expect("try_wait").is_none(),
            "the process must still be running after a refused stop"
        );

        let _ = victim.kill();
        let _ = victim.wait();
    }

    /// The matching case actually terminates.
    #[test]
    fn stops_a_process_whose_image_matches() {
        let mut victim = spawn_victim();
        let pid = victim.id();

        let outcome = stop_process_if_image_matches(pid, "ping.exe").expect("no error");
        assert_eq!(outcome, StopOutcome::Terminated);

        // TerminateProcess is asynchronous, so wait rather than poll once.
        let status = victim.wait().expect("wait");
        assert!(
            !status.success(),
            "terminated process must not exit cleanly"
        );
    }

    /// `Terminated` must mean the process has actually exited, not merely that
    /// termination was requested. The caller relies on that instead of polling
    /// an HTTP endpoint and guessing — a failed request is not proof a daemon
    /// stopped, but a signalled process handle is. Asserted with no sleep in
    /// between, so only the wait inside can make it pass.
    #[test]
    fn terminated_means_the_process_has_already_exited() {
        let mut victim = spawn_victim();
        let pid = victim.id();

        assert_eq!(
            stop_process_if_image_matches(pid, "ping.exe").expect("no error"),
            StopOutcome::Terminated
        );
        assert_eq!(
            stop_process_if_image_matches(pid, "ping.exe").expect("no error"),
            StopOutcome::NotRunning,
            "the process must be gone the moment Terminated is returned"
        );

        let _ = victim.wait();
    }

    /// Stopping again after the process is gone reports NotRunning instead of
    /// failing. `restart` depends on this: a failed stop stops it from ever
    /// calling start.
    #[test]
    fn reports_not_running_once_the_process_has_exited() {
        let mut victim = spawn_victim();
        let pid = victim.id();
        victim.kill().expect("kill");
        victim.wait().expect("wait");

        let outcome = stop_process_if_image_matches(pid, "ping.exe").expect("no error");
        assert_eq!(
            outcome,
            StopOutcome::NotRunning,
            "an exited pid is nothing to stop, not a failure"
        );
    }

    /// The production call passes `kb-mcp.exe`, so exercise that exact name
    /// rather than only the stand-in. Copying a harmless executable under the
    /// daemon's file name gives a real process the real predicate accepts,
    /// without needing the daemon itself (no model download, no port).
    #[test]
    fn stops_a_process_running_under_the_daemon_file_name() {
        let dir = crate::test_support::unique_temp_path("kb-mcp-tray-stop");
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let system_root = std::env::var("SystemRoot").unwrap_or_else(|_| "C:\\Windows".to_string());
        let stand_in = std::path::Path::new(&system_root).join("System32\\ping.exe");
        let disguised = dir.join("kb-mcp.exe");
        std::fs::copy(&stand_in, &disguised).expect("copy stand-in executable");

        let mut victim = Command::new(&disguised)
            .args(["-t", "127.0.0.1"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn disguised process");

        let outcome = stop_process_if_image_matches(victim.id(), "kb-mcp.exe").expect("no error");
        assert_eq!(outcome, StopOutcome::Terminated);
        victim.wait().expect("wait");

        // Best-effort cleanup; the file is unlocked once the process is reaped.
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A pid that never existed is also just "nothing to stop".
    #[test]
    fn reports_not_running_for_an_unassigned_pid() {
        // Pids are multiples of 4 and the space is far from exhausted, so a
        // value this high is not in use. Even if it somehow were, the image
        // guard would return NotOurProcess and the assert would say so.
        let outcome = stop_process_if_image_matches(0x7FFF_FFF0, "kb-mcp.exe").expect("no error");
        assert_eq!(outcome, StopOutcome::NotRunning);
    }
}
