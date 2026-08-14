//! kb-mcp-svc: hidden-console launcher for `kb-mcp.exe serve` on Windows.
//!
//! ## Why a separate binary
//!
//! `kb-mcp.exe` is a console-subsystem binary (Cargo default, no
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
//! `kb-mcp-svc.exe` instead of `kb-mcp.exe` directly. The svc binary lives
//! next to `kb-mcp.exe` (= same `<install dir>/`), so its `current_exe()`
//! parent doubles as the lookup root for the real kb-mcp binary.

#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

/// Build the argv handed to the child `kb-mcp.exe`.
///
/// `serve` is prepended **unconditionally** — which is precisely why the Task
/// Scheduler Action must not pass `serve` itself. The other half of that
/// invariant lives in `kb-mcp/src/service/windows.rs::resolve_action_target`,
/// which leaves `serve_argument` empty whenever the Action points at this
/// binary. If both sides supplied `serve` the child would launch as
/// `kb-mcp.exe serve serve` and die with "unknown subcommand"; if neither did,
/// `kb-mcp.exe` would start with no subcommand at all. Either way the breakage
/// surfaces only at the *next logon*, long after `kb-mcp service install`
/// reported success — so both halves are unit-tested.
///
/// Extra args are forwarded after `serve`. Since BU-07 the Action does use
/// that: it passes `--config <config_home>\kb-mcp.toml`, which reaches
/// `kb-mcp.exe serve --config <path>` — valid because `--config` is a global
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
    // `kb-mcp.exe` (console subsystem) auto-invokes `AllocConsole()` on
    // start-up because the parent svc binary (windows subsystem) has no
    // inheritable console handle. AllocConsole creates a fresh visible
    // conhost window — defeating the entire purpose of the svc wrapper.
    // CREATE_NO_WINDOW tells CreateProcess to skip console allocation
    // entirely for the child, yielding a true 0-flash hidden launch.
    //
    // Reference: docs.microsoft.com/.../process-creation-flags
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    let svc_exe = std::env::current_exe()?;
    let kb_mcp_exe = svc_exe.with_file_name("kb-mcp.exe");

    // Descriptive diagnostic for the Task Scheduler history when the install
    // layout is broken (= someone deleted kb-mcp.exe but left kb-mcp-svc.exe).
    // Without this check the bare `spawn()` error surfaces as "The system
    // cannot find the file specified" which does not point at the missing
    // file. svc is a GUI-subsystem binary so the task history is the only
    // diagnostic surface — make it actionable.
    if !kb_mcp_exe.exists() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!(
                "kb-mcp.exe not found at {} (kb-mcp-svc expects it as a sibling)",
                kb_mcp_exe.display()
            ),
        ));
    }

    // Forward any args we received to the child (= Task Scheduler may pass
    // extra flags in future revisions). The first arg of std::env::args()
    // is our own exe path; skip it. `child_args` supplies the `serve`
    // subcommand — see its doc-comment for the invariant it shares with the
    // installer.
    let extra_args: Vec<String> = std::env::args().skip(1).collect();

    Command::new(&kb_mcp_exe)
        .args(child_args(&extra_args))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()?;

    Ok(())
}

#[cfg(not(target_os = "windows"))]
fn main() {
    eprintln!(
        "kb-mcp-svc: Windows-only helper binary (no-op on non-Windows hosts). \
         The Linux / macOS service backends invoke kb-mcp directly via systemd-user \
         or LaunchAgent, where this hidden-console workaround is unnecessary."
    );
    std::process::exit(1);
}

/// This crate's half of the "exactly one `serve`" invariant. The installer's
/// half is asserted in
/// `kb-mcp/tests/service_install_integration.rs::windows_register_script_*`.
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

    /// (BU-07) The production Action carries `--config <config_home>\kb-mcp.toml`
    /// and still no subcommand, so this is the argv that reaches `kb-mcp.exe`
    /// at logon. `--config` after `serve` parses because it is a global clap
    /// argument; the ordering is the contract `child_args` exists to keep.
    #[test]
    fn child_args_supplies_exactly_one_serve_for_the_production_action() {
        let extra = vec![
            "--config".to_string(),
            "C:\\Users\\me\\AppData\\Roaming\\kb-mcp\\kb-mcp\\kb-mcp.toml".to_string(),
        ];
        let args = child_args(&extra);
        assert_eq!(
            args,
            [
                "serve",
                "--config",
                "C:\\Users\\me\\AppData\\Roaming\\kb-mcp\\kb-mcp\\kb-mcp.toml"
            ]
        );
        assert_eq!(
            args.iter().filter(|a| a.as_str() == "serve").count(),
            1,
            "svc must add serve exactly once; the Task Scheduler Action supplies no subcommand"
        );
    }

    /// Extras land *after* the subcommand — `kb-mcp --bind x serve` would be
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
