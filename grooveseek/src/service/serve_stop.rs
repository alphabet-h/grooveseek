//! Stopping a service's `groove serve` on Windows (kuriya #650).
//!
//! The scheduled task's Action is `groove-svc.exe`, which detach-spawns
//! `groove.exe serve` and exits at once (in the `groove-svc` crate).
//! Task Scheduler therefore sees a task that has finished, in state `Ready`,
//! and `schtasks /End` has nothing to end: the server kept running after
//! `groove service uninstall`, and `--purge` waited out its whole wait for a
//! writer lock that server never let go of.
//!
//! So `groove-svc` records the server it starts — the child's pid and the
//! creation time the OS gives that process, read from the child's own handle
//! ([`groove_winproc::ServeRecord`], `serve-pid.json` in the config home) —
//! and uninstall stops that process, and only that process: the pid must
//! still be `groove.exe` **and** have exactly the recorded creation time
//! ([`groove_winproc::stop_process_if_matches`]). A pid reused since, even by
//! another groove, is left alone.
//!
//! The index's writer lock is deliberately not how the server is found. A
//! service's server that started as a reader (another `groove serve` on the
//! same knowledge base, such as a client's stdio server, took the lease
//! first) or with its watcher off holds no lock, and the process that does
//! hold it is then somebody else's. The status endpoint is not used either:
//! [`crate::server::KbInfo`] no longer names the knowledge base, so the pid it
//! reports could be any groove listening on the configured port.

use groove_winproc::{ServeRecord, StopOutcome};
use std::path::{Path, PathBuf};

/// File name of the server's executable.
pub(crate) const SERVE_IMAGE: &str = "groove.exe";

/// What [`stop_recorded_serve`] did.
#[derive(Debug)]
pub(crate) enum ServeStop {
    /// There is no record: the server, if any, was started by a `groove-svc`
    /// from before kuriya #650, by an Action that runs `groove.exe` itself
    /// (before v0.9.1), or not by the task. Nothing was stopped.
    NoRecord { path: PathBuf },
    /// There is a file that is not a record (its reason, in ASCII). Nothing
    /// was stopped.
    BadRecord { path: PathBuf, why: String },
    /// The recorded server is not running: `detail` says whether it exited or
    /// its pid now belongs to another process. Nothing was stopped.
    NotRunning { pid: u32, detail: &'static str },
    /// The recorded server was terminated and has exited.
    Stopped { pid: u32 },
    /// The recorded server is running and could not be stopped (`why`, in
    /// ASCII: access denied, or it did not exit in time).
    NotStopped { pid: u32, why: String },
}

/// Stop the server `groove-svc` recorded in `config_home`, when the process
/// behind the recorded pid is still that server. Waits up to
/// [`groove_winproc::EXIT_WAIT`] for it to exit, and no longer.
pub(crate) fn stop_recorded_serve(config_home: &Path) -> ServeStop {
    let path = ServeRecord::path_in(config_home);
    let record = match ServeRecord::read(&path) {
        Ok(Some(r)) => r,
        Ok(None) => return ServeStop::NoRecord { path },
        Err(e) => {
            // An OS error is shown by kind and code (its message is
            // localized); a parse error is this crate's own words, escaped.
            let why = match e.raw_os_error() {
                Some(_) => crate::embedder::ascii_io_error(&e),
                None => e.to_string().escape_default().to_string(),
            };
            return ServeStop::BadRecord { path, why };
        }
    };
    let pid = record.pid;
    match groove_winproc::stop_process_if_matches(pid, SERVE_IMAGE, Some(record.created)) {
        Ok(StopOutcome::Terminated) => ServeStop::Stopped { pid },
        Ok(StopOutcome::NotRunning) => ServeStop::NotRunning {
            pid,
            detail: "it has exited",
        },
        Ok(StopOutcome::NotOurProcess) => ServeStop::NotRunning {
            pid,
            detail: "the pid now belongs to another program, left alone",
        },
        Ok(StopOutcome::Reused) => ServeStop::NotRunning {
            pid,
            detail: "the pid now belongs to a later process, left alone",
        },
        Err(e) => ServeStop::NotStopped {
            pid,
            why: e.to_string(),
        },
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    //! A stand-in server for the tests here and in [`crate::service::uninstall`]: `ping.exe`
    //! copied as `groove.exe`, recorded the way `groove-svc` records it.

    use super::SERVE_IMAGE;
    use groove_winproc::ServeRecord;
    use std::path::{Path, PathBuf};
    use std::process::{Child, Command, Stdio};

    pub(crate) fn system_ping() -> PathBuf {
        let root = std::env::var("SystemRoot").unwrap_or_else(|_| "C:\\Windows".into());
        Path::new(&root).join("System32").join("ping.exe")
    }

    /// A long-lived, harmless process: `ping -t` loops until terminated.
    pub(crate) fn spawn(exe: &Path) -> Child {
        Command::new(exe)
            .args(["-t", "127.0.0.1"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn the stand-in")
    }

    /// A running `groove.exe` (a copy of `ping.exe`) in its own temp dir;
    /// killed and removed on drop if still there.
    pub(crate) struct StandIn {
        dir: PathBuf,
        pub(crate) child: Child,
    }

    impl StandIn {
        pub(crate) fn start() -> Self {
            let dir = crate::test_support::unique_temp_path("groove-650-serve");
            std::fs::create_dir_all(&dir).unwrap();
            let exe = dir.join(SERVE_IMAGE);
            std::fs::copy(system_ping(), &exe).unwrap();
            let child = spawn(&exe);
            Self { dir, child }
        }

        /// The record `groove-svc` would write for this process.
        pub(crate) fn record(&self) -> ServeRecord {
            ServeRecord {
                pid: self.child.id(),
                created: groove_winproc::creation_time_of(&self.child).unwrap(),
            }
        }

        pub(crate) fn is_running(&mut self) -> bool {
            self.child.try_wait().unwrap().is_none()
        }
    }

    impl Drop for StandIn {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::{StandIn, spawn, system_ping};
    use super::*;

    fn home() -> PathBuf {
        let dir = crate::test_support::unique_temp_path("groove-650-home");
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn no_record_stops_nothing() {
        let h = home();
        assert!(matches!(
            stop_recorded_serve(&h),
            ServeStop::NoRecord { .. }
        ));
        let _ = std::fs::remove_dir_all(h);
    }

    #[test]
    fn a_file_that_is_not_a_record_stops_nothing() {
        let h = home();
        std::fs::write(ServeRecord::path_in(&h), b"not json").unwrap();
        assert!(matches!(
            stop_recorded_serve(&h),
            ServeStop::BadRecord { .. }
        ));
        let _ = std::fs::remove_dir_all(h);
    }

    /// The main path: the recorded server is stopped and has exited.
    #[test]
    fn the_recorded_server_is_stopped() {
        let h = home();
        let mut s = StandIn::start();
        s.record().write(&ServeRecord::path_in(&h)).unwrap();
        let outcome = stop_recorded_serve(&h);
        assert!(
            matches!(outcome, ServeStop::Stopped { pid } if pid == s.child.id()),
            "{outcome:?}"
        );
        assert!(!s.is_running(), "it has exited");
        let _ = std::fs::remove_dir_all(h);
    }

    /// A pid whose process was created at another time is a reused pid, even
    /// when it is `groove.exe`: left running.
    #[test]
    fn a_reused_pid_is_left_running() {
        let h = home();
        let mut s = StandIn::start();
        let mut rec = s.record();
        rec.created -= 1;
        rec.write(&ServeRecord::path_in(&h)).unwrap();
        let outcome = stop_recorded_serve(&h);
        assert!(
            matches!(&outcome, ServeStop::NotRunning { detail, .. } if detail.contains("later process")),
            "{outcome:?}"
        );
        assert!(s.is_running(), "it must still run");
        let _ = std::fs::remove_dir_all(h);
    }

    /// A pid that now belongs to another program is left running.
    #[test]
    fn a_pid_of_another_program_is_left_running() {
        let h = home();
        let mut other = spawn(&system_ping());
        ServeRecord {
            pid: other.id(),
            created: groove_winproc::creation_time_of(&other).unwrap(),
        }
        .write(&ServeRecord::path_in(&h))
        .unwrap();
        let outcome = stop_recorded_serve(&h);
        assert!(
            matches!(&outcome, ServeStop::NotRunning { detail, .. } if detail.contains("another program")),
            "{outcome:?}"
        );
        assert!(other.try_wait().unwrap().is_none(), "it must still run");
        let _ = other.kill();
        let _ = other.wait();
        let _ = std::fs::remove_dir_all(h);
    }

    /// A recorded pid no process has (not a multiple of 4, so never one on
    /// Windows) is a server that has exited.
    #[test]
    fn a_recorded_pid_no_process_has_is_not_running() {
        let h = home();
        ServeRecord {
            pid: u32::MAX - 2,
            created: 1,
        }
        .write(&ServeRecord::path_in(&h))
        .unwrap();
        assert!(matches!(
            stop_recorded_serve(&h),
            ServeStop::NotRunning {
                detail: "it has exited",
                ..
            }
        ));
        let _ = std::fs::remove_dir_all(h);
    }
}
