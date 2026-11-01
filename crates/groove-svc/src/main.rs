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

    // (kuriya #650) The previous record goes before the new server starts, so
    // a record that then cannot be written leaves none rather than an old one:
    // uninstall warns about a missing record, but would read an old one as
    // "that server has exited, nothing to stop".
    let record_path = clear_old_record(&extra_args);

    let child = Command::new(&grooveseek_exe)
        .args(child_args(&extra_args))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()?;

    // (kuriya #650) Record which process is the service's server, so
    // `groove service uninstall` can stop it: this process exits right away,
    // so Task Scheduler sees a finished task and `schtasks /End` stops nothing.
    // Best-effort: the server is already running, and failing here would only
    // put an error in the task history while it runs anyway.
    if let Some(path) = record_path
        && let Ok(created) = groove_winproc::creation_time_of(&child)
    {
        let record = groove_winproc::ServeRecord {
            pid: child.id(),
            created,
        };
        let _ = record.write(&path);
    }
    // Dropping `child` closes its handle and leaves the server running.
    Ok(())
}

/// Remove the record of the server an earlier run started, and return where
/// this run's record goes; `None` when the Action names no config home.
/// Best-effort like the write: a record that cannot be removed is left, and
/// one that is not there is what is wanted.
#[cfg(target_os = "windows")]
fn clear_old_record(extra: &[String]) -> Option<std::path::PathBuf> {
    let path = groove_winproc::ServeRecord::path_in(&record_dir(extra)?);
    let _ = std::fs::remove_file(&path);
    Some(path)
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

    /// The old record is gone before the new server is started, so a failed
    /// write leaves no record rather than a stale one.
    #[cfg(windows)]
    #[test]
    fn the_old_record_is_cleared_before_the_server_starts() {
        let dir = std::env::temp_dir().join(format!(
            "groove-svc-clear-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let old = groove_winproc::ServeRecord::path_in(&dir);
        groove_winproc::ServeRecord { pid: 8, created: 1 }
            .write(&old)
            .unwrap();
        let extra = vec![
            "--config".to_string(),
            dir.join("groove.toml").display().to_string(),
        ];
        assert_eq!(super::clear_old_record(&extra), Some(old.clone()));
        assert!(!old.exists(), "the old record is removed");
        assert_eq!(
            super::clear_old_record(&extra),
            Some(old.clone()),
            "and none is fine"
        );
        assert_eq!(super::clear_old_record(&[]), None);
        let _ = std::fs::remove_dir_all(dir);
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
