//! groove-svc: hidden-console launcher for `groove.exe serve` on Windows.
//!
//! ## Why a separate binary
//!
//! `groove.exe` is a console-subsystem binary (Cargo default, no
//! `#![windows_subsystem = "windows"]`). When Windows Task Scheduler launches
//! a console binary under an AtLogOn trigger with `RunLevel Limited`, the
//! kernel allocates a conhost.exe and a visible console window **before** the
//! process starts — so even `-WindowStyle Hidden`, `FreeConsole()`, and
//! `ShowWindow(SW_HIDE)` only hide the window *after* it has flashed for ~1
//! second. This is a known, unfixed Windows behaviour (Microsoft tracks it
//! under microsoft/terminal#249 and PowerShell/PowerShell#3028 since 2018).
//!
//! The only way to get a true 0-flash hidden launch is to start a
//! windows-subsystem binary (= no console allocation at all) which then
//! spawns the actual console binary as a detached child with stdio nulled
//! out. The windows-subsystem parent has no console for the child to
//! inherit, so the kernel skips conhost allocation entirely.
//!
//! Task Scheduler's Action is rewritten in v0.9.1 to point at
//! `groove-svc.exe` instead of `groove.exe` directly. The svc binary lives
//! next to `groove.exe` (= same `<install dir>/`), so its `current_exe()`
//! parent doubles as the lookup root for the real groove binary.

#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

/// Build the argv handed to the child `groove.exe`.
///
/// `serve` is prepended **unconditionally** — which is precisely why the Task
/// Scheduler Action must not pass `serve` itself. The other half of that
/// invariant lives in `grooveseek/src/service/windows.rs::resolve_action_target`,
/// which leaves `serve_argument` empty whenever the Action points at this
/// binary. If both sides supplied `serve` the child would launch as
/// `groove.exe serve serve` and die with "unknown subcommand"; if neither did,
/// `groove.exe` would start with no subcommand at all. Either way the breakage
/// surfaces only at the *next logon*, long after `groove service install`
/// reported success — so both halves are unit-tested.
///
/// Extra args are forwarded after `serve`. Since BU-07 the Action does use
/// that: it passes `--config <config_home>\groove.toml`, which reaches
/// `groove.exe serve --config <path>` — valid because `--config` is a global
/// clap argument and so may follow the subcommand.
///
/// Compiled on every platform (not just Windows) so the invariant test runs on
/// the Linux and macOS CI legs too; only the Windows `main` actually calls it.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
fn child_args(extra: &[String]) -> Vec<String> {
    let mut args = Vec::with_capacity(extra.len() + 1);
    args.push("serve".to_string());
    args.extend_from_slice(extra);
    args
}

#[cfg(target_os = "windows")]
fn main() -> std::io::Result<()> {
    use std::os::windows::process::CommandExt;
    use std::process::{Command, Stdio};

    // CREATE_NO_WINDOW = 0x0800_0000 (winbase.h). Without this flag the child
    // `groove.exe` (console subsystem) auto-invokes `AllocConsole()` on
    // start-up because the parent svc binary (windows subsystem) has no
    // inheritable console handle. AllocConsole creates a fresh visible
    // conhost window — defeating the entire purpose of the svc wrapper.
    // CREATE_NO_WINDOW tells CreateProcess to skip console allocation
    // entirely for the child, yielding a true 0-flash hidden launch.
    //
    // Reference: docs.microsoft.com/.../process-creation-flags
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    let svc_exe = std::env::current_exe()?;
    let grooveseek_exe = svc_exe.with_file_name("groove.exe");

    // Descriptive diagnostic for the Task Scheduler history when the install
    // layout is broken (= someone deleted groove.exe but left groove-svc.exe).
    // Without this check the bare `spawn()` error surfaces as "The system
    // cannot find the file specified" which does not point at the missing
    // file. svc is a GUI-subsystem binary so the task history is the only
    // diagnostic surface — make it actionable.
    if !grooveseek_exe.exists() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!(
                "groove.exe not found at {} (groove-svc expects it as a sibling)",
                grooveseek_exe.display()
            ),
        ));
    }

    // Forward any args we received to the child (= Task Scheduler may pass
    // extra flags in future revisions). The first arg of std::env::args()
    // is our own exe path; skip it. `child_args` supplies the `serve`
    // subcommand — see its doc-comment for the invariant it shares with the
    // installer.
    let extra_args: Vec<String> = std::env::args().skip(1).collect();

    let spawn = || {
        Command::new(&grooveseek_exe)
            .args(child_args(&extra_args))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .creation_flags(CREATE_NO_WINDOW)
            .spawn()
    };
    // `AlreadyRunning` is a success: the service's server is up, which is
    // what starting the task asks for.
    launch(record_dir(&extra_args).as_deref(), spawn)?;
    Ok(())
}

/// What [`launch`] did.
#[cfg(target_os = "windows")]
#[derive(Debug, PartialEq, Eq)]
enum Launch {
    /// The server recorded in the config home is running (this pid): nothing
    /// was spawned and the record was left as it was.
    AlreadyRunning(u32),
    /// A server was spawned (this pid), and recorded when there is a config
    /// home to record it in.
    Started(u32),
}

/// File name of the server's executable, as the record's check expects it.
#[cfg(target_os = "windows")]
const SERVE_IMAGE: &str = "groove.exe";

/// Start the service's server unless the one recorded in `config_home` is
/// still running (kuriya #650).
///
/// The task can be started again while its server runs (Task Scheduler sees
/// a finished task once this launcher exits: a manual `/Run`, the next logon,
/// the tray's Start). A second server would fail to bind the port and exit,
/// and recording it would drop the running one from the record, out of
/// uninstall's reach. So the check of the recorded server, the removal of the
/// record, the spawn and the new record happen in one section, serialized per
/// config home by [`groove_winproc::RecordLock`]; a running recorded server
/// (its image and exact creation time, the check the stop makes) means no
/// spawn at all.
///
/// The old record is removed before the spawn, so a record that then cannot
/// be written leaves none (uninstall warns) rather than an old one read as
/// "that server has exited". A server whose record cannot be written (or
/// whose creation time cannot be read) is stopped again and the launch fails:
/// a server out of the record is out of reach of uninstall and of the next
/// launch's check, so it is not left running. An error before the spawn (the lock not taken in time, a
/// recorded process that exists but cannot be examined) spawns nothing, and
/// lands in the task history. Without a config home (an Action from before
/// BU-07) there is nothing to record or check, and the server is started as
/// before.
#[cfg(target_os = "windows")]
fn launch(
    config_home: Option<&std::path::Path>,
    spawn: impl FnOnce() -> std::io::Result<std::process::Child>,
) -> std::io::Result<Launch> {
    launch_waiting(
        config_home,
        groove_winproc::RECORD_LOCK_WAIT,
        spawn,
        |record, path| record.write(path),
    )
}

/// [`launch`] with the wait for the section and the record's write passed
/// in, so a test can refuse the section without sitting out
/// [`groove_winproc::RECORD_LOCK_WAIT`], and make the write fail.
#[cfg(target_os = "windows")]
fn launch_waiting(
    config_home: Option<&std::path::Path>,
    wait: std::time::Duration,
    spawn: impl FnOnce() -> std::io::Result<std::process::Child>,
    write: impl FnOnce(&groove_winproc::ServeRecord, &std::path::Path) -> std::io::Result<()>,
) -> std::io::Result<Launch> {
    use groove_winproc::{RecordLock, ServeRecord};
    let Some(home) = config_home else {
        return Ok(Launch::Started(spawn()?.id()));
    };
    let _section = match RecordLock::acquire(home, wait) {
        Ok(section) => section,
        // No config home: no groove.toml either, so the server will report
        // that itself; there is no record to keep consistent.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Launch::Started(spawn()?.id()));
        }
        Err(e) => return Err(e),
    };
    let path = ServeRecord::path_in(home);
    if let Ok(Some(record)) = ServeRecord::read(&path)
        && groove_winproc::is_recorded_process_running(record.pid, SERVE_IMAGE, record.created)?
    {
        return Ok(Launch::AlreadyRunning(record.pid));
    }
    let _ = std::fs::remove_file(&path);
    let mut child = spawn()?;
    let recorded = groove_winproc::creation_time_of(&child).and_then(|created| {
        write(
            &ServeRecord {
                pid: child.id(),
                created,
            },
            &path,
        )
    });
    if let Err(e) = recorded {
        // A server that is not recorded is out of reach of uninstall and of
        // the next launch's check: stop it, through the handle it was spawned
        // with, and wait for it, rather than leave it running unmanaged. The
        // error reaches the task history.
        let _ = child.kill();
        let _ = child.wait();
        return Err(std::io::Error::new(
            e.kind(),
            format!(
                "could not record the server in {} ({e}); it was stopped",
                path.display()
            ),
        ));
    }
    // Dropping `child` closes its handle and leaves the server running.
    Ok(Launch::Started(child.id()))
}

/// The directory the [`groove_winproc::ServeRecord`] goes in: the one holding
/// the `groove.toml` the Action passes as `--config <config_home>\groove.toml`
/// (BU-07), which is the service's config home. `None` when the Action passes
/// no `--config` (an install from before BU-07), so nothing is recorded.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
fn record_dir(extra: &[String]) -> Option<std::path::PathBuf> {
    let at = extra.iter().position(|a| a == "--config")?;
    let config = std::path::Path::new(extra.get(at + 1)?);
    config
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(|p| p.to_path_buf())
}

#[cfg(not(target_os = "windows"))]
fn main() {
    eprintln!(
        "groove-svc: Windows-only helper binary (no-op on non-Windows hosts). \
         The Linux / macOS service backends invoke groove directly via systemd-user \
         or LaunchAgent, where this hidden-console workaround is unnecessary."
    );
    std::process::exit(1);
}

/// (kuriya #650) Where groove-svc records the server it starts.
#[cfg(test)]
mod record_dir_tests {
    use super::record_dir;

    /// A backslash is a separator only on Windows, where this runs for real.
    #[cfg(windows)]
    #[test]
    fn the_record_goes_beside_the_groove_toml_the_action_names() {
        let extra = vec![
            "--config".to_string(),
            "C:\\Users\\me\\AppData\\Roaming\\groove\\work\\groove.toml".to_string(),
        ];
        assert_eq!(
            record_dir(&extra).unwrap(),
            std::path::Path::new("C:\\Users\\me\\AppData\\Roaming\\groove\\work")
        );
    }

    #[test]
    fn without_a_config_argument_nothing_is_recorded() {
        assert_eq!(record_dir(&[]), None);
        assert_eq!(record_dir(&["--config".to_string()]), None);
        assert_eq!(
            record_dir(&["--config".to_string(), "groove.toml".to_string()]),
            None
        );
    }

    #[test]
    fn a_relative_config_records_beside_it() {
        assert_eq!(
            record_dir(&["--config".to_string(), "home/groove.toml".to_string()]),
            Some(std::path::PathBuf::from("home"))
        );
    }
}

/// (kuriya #650) The launcher's check and record: a second start of the task
/// while the recorded server runs spawns nothing and keeps the record.
#[cfg(all(test, target_os = "windows"))]
mod launch_tests {
    use super::{Launch, launch};
    use groove_winproc::ServeRecord;
    use std::path::{Path, PathBuf};
    use std::process::{Child, Command, Stdio};

    fn temp_dir(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let p = std::env::temp_dir().join(format!(
            "groove-svc-{tag}-{}-{nanos}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    /// `ping.exe` copied as `groove.exe`: a harmless stand-in server.
    fn stand_in_exe(dir: &Path) -> PathBuf {
        let root = std::env::var("SystemRoot").unwrap_or_else(|_| "C:\\Windows".into());
        let exe = dir.join("groove.exe");
        std::fs::copy(Path::new(&root).join("System32").join("ping.exe"), &exe).unwrap();
        exe
    }

    fn spawn_stand_in(exe: &Path) -> std::io::Result<Child> {
        Command::new(exe)
            .args(["-t", "127.0.0.1"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
    }

    #[test]
    fn a_second_start_while_the_recorded_server_runs_spawns_nothing() {
        let home = temp_dir("home");
        let bin = temp_dir("bin");
        let exe = stand_in_exe(&bin);

        let first = launch(Some(&home), || spawn_stand_in(&exe)).unwrap();
        let Launch::Started(pid) = first else {
            panic!("the first start spawns: {first:?}")
        };
        let record = ServeRecord::read(&ServeRecord::path_in(&home))
            .unwrap()
            .expect("the first start is recorded");
        assert_eq!(record.pid, pid);

        let mut spawned_again = false;
        let second = launch(Some(&home), || {
            spawned_again = true;
            spawn_stand_in(&exe)
        })
        .unwrap();
        assert_eq!(second, Launch::AlreadyRunning(pid));
        assert!(
            !spawned_again,
            "nothing is spawned while the recorded server runs"
        );
        assert_eq!(
            ServeRecord::read(&ServeRecord::path_in(&home)).unwrap(),
            Some(record),
            "the record still names the running server"
        );

        // Once it has exited, the next start spawns and records a new one.
        assert_eq!(
            groove_winproc::stop_process_if_matches(pid, "groove.exe", Some(record.created))
                .unwrap(),
            groove_winproc::StopOutcome::Terminated
        );
        let third = launch(Some(&home), || spawn_stand_in(&exe)).unwrap();
        let Launch::Started(new_pid) = third else {
            panic!("a start after the server exited spawns: {third:?}")
        };
        let new_record = ServeRecord::read(&ServeRecord::path_in(&home))
            .unwrap()
            .expect("recorded");
        assert_eq!(new_record.pid, new_pid);
        let _ = groove_winproc::stop_process_if_matches(
            new_pid,
            "groove.exe",
            Some(new_record.created),
        );
        let _ = std::fs::remove_dir_all(home);
        let _ = std::fs::remove_dir_all(bin);
    }

    /// A start that cannot take the section in time (another launch, or a
    /// stop, holds it) spawns nothing and fails, to the task history.
    #[test]
    fn a_start_that_cannot_take_the_section_spawns_nothing() {
        let home = temp_dir("busy");
        let held = groove_winproc::RecordLock::acquire(&home, std::time::Duration::ZERO).unwrap();
        let mut spawned = false;
        let err = super::launch_waiting(
            Some(&home),
            std::time::Duration::from_millis(100),
            || {
                spawned = true;
                Err(std::io::Error::other("must not be called"))
            },
            |record, path| record.write(path),
        )
        .unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::TimedOut);
        assert!(!spawned, "nothing is spawned without the section");
        drop(held);
        let _ = std::fs::remove_dir_all(home);
    }

    /// A server whose record cannot be written is stopped again: the launch
    /// fails and no server is left running out of the record.
    #[test]
    fn a_server_that_cannot_be_recorded_is_not_left_running() {
        let home = temp_dir("unwritable");
        let bin = temp_dir("unwritable-bin");
        let exe = stand_in_exe(&bin);
        let spawned_pid = std::cell::Cell::new(None);
        let err = super::launch_waiting(
            Some(&home),
            std::time::Duration::ZERO,
            || {
                let child = spawn_stand_in(&exe)?;
                spawned_pid.set(Some(child.id()));
                Ok(child)
            },
            |_, _| {
                Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "injected write failure",
                ))
            },
        )
        .unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied);
        assert!(err.to_string().contains("it was stopped"), "{err}");
        let pid = spawned_pid.get().expect("a server was spawned");
        assert_eq!(
            groove_winproc::stop_process_if_image_matches(pid, "groove.exe").unwrap(),
            groove_winproc::StopOutcome::NotRunning,
            "the unrecorded server is gone"
        );
        assert_eq!(
            ServeRecord::read(&ServeRecord::path_in(&home)).unwrap(),
            None,
            "and there is no record"
        );
        let _ = std::fs::remove_dir_all(home);
        let _ = std::fs::remove_dir_all(bin);
    }

    /// An old record whose server has exited does not stop a start, and is
    /// replaced by the new one.
    #[test]
    fn a_stale_record_is_replaced() {
        let home = temp_dir("stale");
        let bin = temp_dir("stale-bin");
        let exe = stand_in_exe(&bin);
        ServeRecord {
            pid: u32::MAX - 2,
            created: 1,
        }
        .write(&ServeRecord::path_in(&home))
        .unwrap();
        let Launch::Started(pid) = launch(Some(&home), || spawn_stand_in(&exe)).unwrap() else {
            panic!("a stale record does not block the start")
        };
        let rec = ServeRecord::read(&ServeRecord::path_in(&home))
            .unwrap()
            .unwrap();
        assert_eq!(rec.pid, pid);
        let _ = groove_winproc::stop_process_if_matches(pid, "groove.exe", Some(rec.created));
        let _ = std::fs::remove_dir_all(home);
        let _ = std::fs::remove_dir_all(bin);
    }
}

/// This crate's half of the "exactly one `serve`" invariant. The installer's
/// half is asserted in
/// `grooveseek/tests/service_install_integration.rs::windows_register_script_*`.
#[cfg(test)]
mod tests {
    use super::child_args;

    /// The degenerate case: whatever the Action passes, exactly one `serve`
    /// must reach the child. This was the shape that actually ran at logon
    /// until BU-07 gave the Action a `--config` argument to carry; see
    /// `child_args_supplies_exactly_one_serve_for_the_production_action` for
    /// the shape that runs now.
    #[test]
    fn child_args_supplies_exactly_one_serve_for_the_no_argument_action() {
        let args = child_args(&[]);
        assert_eq!(args, ["serve"]);
        assert_eq!(
            args.iter().filter(|a| a.as_str() == "serve").count(),
            1,
            "svc must add serve exactly once; the Task Scheduler Action supplies no subcommand"
        );
    }

    /// (BU-07) The production Action carries `--config <config_home>\groove.toml`
    /// and still no subcommand, so this is the argv that reaches `groove.exe`
    /// at logon. `--config` after `serve` parses because it is a global clap
    /// argument; the ordering is the contract `child_args` exists to keep.
    #[test]
    fn child_args_supplies_exactly_one_serve_for_the_production_action() {
        let extra = vec![
            "--config".to_string(),
            "C:\\Users\\me\\AppData\\Roaming\\groove\\groove\\groove.toml".to_string(),
        ];
        let args = child_args(&extra);
        assert_eq!(
            args,
            [
                "serve",
                "--config",
                "C:\\Users\\me\\AppData\\Roaming\\groove\\groove\\groove.toml"
            ]
        );
        assert_eq!(
            args.iter().filter(|a| a.as_str() == "serve").count(),
            1,
            "svc must add serve exactly once; the Task Scheduler Action supplies no subcommand"
        );
    }

    /// Extras land *after* the subcommand — `groove --bind x serve` would be
    /// rejected by clap, so ordering is part of the contract.
    #[test]
    fn child_args_forwards_extras_after_serve() {
        let extra = vec!["--bind".to_string(), "127.0.0.1:3100".to_string()];
        assert_eq!(child_args(&extra), ["serve", "--bind", "127.0.0.1:3100"]);
    }

    /// If a future Action revision ever did pass `serve`, this documents what
    /// the child would receive: the doubled form that fails with "unknown
    /// subcommand". The guard against it is the installer leaving
    /// `argument_clause` empty, not any filtering here.
    #[test]
    fn child_args_does_not_deduplicate_a_caller_supplied_serve() {
        assert_eq!(child_args(&["serve".to_string()]), ["serve", "serve"]);
    }
}
