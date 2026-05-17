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
    // is our own exe path; skip it.
    //
    // INVARIANT: the Task Scheduler Action MUST NOT pass `serve` as an
    // Argument. kb-mcp-svc unconditionally adds `serve` below, so doubling it
    // would produce `kb-mcp.exe serve serve` which fails with
    // "unknown subcommand: serve". `register_via_powershell` enforces this
    // by leaving `argument_clause` empty when it points at the svc binary.
    let extra_args: Vec<String> = std::env::args().skip(1).collect();

    Command::new(&kb_mcp_exe)
        .arg("serve")
        .args(&extra_args)
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
