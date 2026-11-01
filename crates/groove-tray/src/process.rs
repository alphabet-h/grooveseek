//! Native process control for stopping the groove daemon.
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
//! ## Where the Win32 half lives now
//!
//! In `groove-winproc` (kuriya #650), shared with `groove service uninstall`,
//! which needed the same open-check-terminate on one handle plus a check of
//! the process's creation time against what `groove-svc` recorded. This module
//! keeps the names the tray and its tests use and adds the tray's error
//! wording on top. Why a handle pins the identity is written there.

use anyhow::Result;

pub use groove_winproc::StopOutcome;

/// True when `image_path`'s file name is `expected_file_name`, compared the way
/// Windows compares paths (case-insensitively).
#[cfg_attr(not(test), allow(dead_code))] // the tray calls it only through its tests
pub fn image_file_name_matches(image_path: &str, expected_file_name: &str) -> bool {
    groove_winproc::image_file_name_matches(image_path, expected_file_name)
}

/// Terminate `pid`, but only if its executable is named `expected_file_name`,
/// and wait for it to exit. See [`groove_winproc::stop_process_if_matches`].
pub fn stop_process_if_image_matches(pid: u32, expected_file_name: &str) -> Result<StopOutcome> {
    Ok(groove_winproc::stop_process_if_image_matches(
        pid,
        expected_file_name,
    )?)
}

/// [`stop_process_if_image_matches`] for the server `groove-svc` recorded:
/// also only when the process was created at `created`, so a reused pid,
/// even one now running another `groove.exe`, is left alone
/// ([`StopOutcome::Reused`]).
pub fn stop_recorded_process(
    pid: u32,
    expected_file_name: &str,
    created: u64,
) -> Result<StopOutcome> {
    Ok(groove_winproc::stop_process_if_matches(
        pid,
        expected_file_name,
        Some(created),
    )?)
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
            "C:\\Users\\me\\.cargo\\bin\\groove.exe",
            "groove.exe"
        ));
        assert!(image_file_name_matches("C:\\a\\GROOVE.EXE", "groove.exe"));
        assert!(!image_file_name_matches(
            "C:\\a\\groove-svc.exe",
            "groove.exe"
        ));
        assert!(!image_file_name_matches(
            "C:\\a\\groove.exe.bak",
            "groove.exe"
        ));
        assert!(!image_file_name_matches("", "groove.exe"));
    }

    /// A pid belonging to another program must be left strictly alone. This is
    /// the guard that stops a stale status response from killing whatever
    /// recycled the number.
    #[test]
    fn refuses_to_stop_a_process_with_a_different_image() {
        let mut victim = spawn_victim();
        let pid = victim.id();

        let outcome = stop_process_if_image_matches(pid, "groove.exe").expect("no error");
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

    /// The production call passes `groove.exe`, so exercise that exact name
    /// rather than only the stand-in. Copying a harmless executable under the
    /// daemon's file name gives a real process the real predicate accepts,
    /// without needing the daemon itself (no model download, no port).
    #[test]
    fn stops_a_process_running_under_the_daemon_file_name() {
        let dir = crate::test_support::unique_temp_path("groove-tray-stop");
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let system_root = std::env::var("SystemRoot").unwrap_or_else(|_| "C:\\Windows".to_string());
        let stand_in = std::path::Path::new(&system_root).join("System32\\ping.exe");
        let disguised = dir.join("groove.exe");
        std::fs::copy(&stand_in, &disguised).expect("copy stand-in executable");

        let mut victim = Command::new(&disguised)
            .args(["-t", "127.0.0.1"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn disguised process");

        let outcome = stop_process_if_image_matches(victim.id(), "groove.exe").expect("no error");
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
        let outcome = stop_process_if_image_matches(0x7FFF_FFF0, "groove.exe").expect("no error");
        assert_eq!(outcome, StopOutcome::NotRunning);
    }
}
