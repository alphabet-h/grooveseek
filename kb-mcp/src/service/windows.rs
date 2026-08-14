//! Windows Task Scheduler backend for kb-mcp service.
//!
//! Module-level `#[cfg(target_os = "windows")]` lives on the `pub mod windows;`
//! declaration in `src/service/mod.rs`; no inner `#![cfg]` needed.
//!
//! ## Why PowerShell cmdlets, not `schtasks` / `-Xml`
//!
//! The Phase 1 install needs to register a task at the root path (`\<name>`)
//! under the user's normal (non-elevated) shell — spec § Q4 promised "no admin
//! required". The three rejected approaches:
//!
//! 1. **`schtasks /Create /XML`** (v0.8.0 / v0.8.1 attempts) — even with a
//!    correctly UTF-16 LE BOM-encoded XML, returns "Access is denied" on
//!    root-path registration from a non-elevated shell. The legacy CLI
//!    apparently doesn't go through the COM API path used by the PowerShell
//!    module.
//! 2. **`Register-ScheduledTask -Xml`** (v0.8.2 attempt) — XML parameter set
//!    doesn't auto-populate a `<UserId>` in the Principal, so Task Scheduler
//!    falls back to a user-ambiguous principal that needs admin. Returns
//!    HRESULT 0x80070005 from the same non-elevated shell.
//! 3. **`Register-ScheduledTask -Action -Trigger -Settings`** (v0.8.3, current)
//!    — cmdlet auto-builds the Principal from the current logon identity, so
//!    user-level registration just works.

use super::powershell::{decode_diagnostic, decode_value, with_utf8_output};
use super::{InstallContext, ServiceBackend, ServiceState};
use anyhow::{Context, Result, anyhow};
use std::path::{Path, PathBuf};
use std::process::Command;

pub(crate) struct TaskScheduler;

/// The scheduled-task name for a service.
///
/// (AU-33) `kb-mcp-tray` carries an identical copy in `daemon.rs`, and it
/// cannot be shared: that module belongs to the tray's **bin** target, while
/// this crate can only import its **lib**. The same one-way dependency forced
/// the duplicated PowerShell helpers in AU-04.
///
/// A divergence would not fail to compile — the tray would simply control a
/// task nobody registered, silently, which is the shape AU-65 already had once.
/// So both sides pin the literal in a test instead, as AU-04 did for the UTF-8
/// prelude.
fn task_name(service_name: &str) -> String {
    format!("kb-mcp-{}", service_name)
}

fn run_schtasks(args: &[&str]) -> Result<()> {
    let status = Command::new("schtasks")
        .args(args)
        .status()
        .with_context(|| format!("schtasks {} 実行失敗", args.join(" ")))?;
    if !status.success() {
        return Err(anyhow!(
            "schtasks {} 失敗 (status: {})",
            args.join(" "),
            status
        ));
    }
    Ok(())
}

/// The subcommand word a console-binary Action has to supply itself.
///
/// **Invariant, enforced jointly with `crates/kb-mcp-svc/src/main.rs`:** exactly
/// one side supplies `serve`. `kb-mcp-svc.exe` prepends it unconditionally, so
/// an Action aimed at the svc launcher must leave this empty, while an Action
/// aimed at `kb-mcp.exe` must supply it. Breaking either half produces
/// `kb-mcp.exe serve serve` (or a bare `kb-mcp.exe` with no subcommand) — and
/// only at the *next logon*, long after the install command reported success.
/// Both halves are unit-tested for that reason.
const SERVE_SUBCOMMAND: &str = "serve";

/// Which binary the AtLogOn Action executes, plus the subcommand word that must
/// accompany it.
///
/// **This is not the whole `-Argument` clause** (it was, until BU-07). Task
/// Scheduler accepts exactly one `-Argument`, and the Action now also carries
/// `--config`, so the clause has to be assembled in one place —
/// [`build_register_script`], which is the only function that knows the config
/// home. Holding a bare word here keeps that assembly possible without string
/// surgery on a clause that was already quoted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActionTarget {
    pub execute_path: PathBuf,
    /// `"serve"` for the console binary, empty for the svc launcher (which adds
    /// it itself).
    pub serve_argument: String,
    /// `false` when the svc launcher was missing and the console-visible
    /// fallback was taken.
    ///
    /// Carried in the return value rather than reported from inside
    /// [`resolve_action_target`] so that function stays pure — which is what
    /// makes it testable without a real install layout (AU-07 / AU-08). The
    /// caller is responsible for telling the user; leaving the fallback silent
    /// is what AU-67 was about.
    pub used_svc_launcher: bool,
}

/// (v0.9.1 hot-fix) Decide what the AtLogOn Action should execute.
///
/// Prefer the windows-subsystem `kb-mcp-svc.exe` sibling when present so Task
/// Scheduler spawns a console-less launcher that detach-spawns
/// `kb-mcp.exe serve`. Without this the bare `kb-mcp.exe` Action surfaces a
/// visible console window on every login — Windows allocates conhost before the
/// process starts, so even `-WindowStyle Hidden` / `FreeConsole()` only hide it
/// *after* a ~1 sec flash. The svc binary has
/// `#![windows_subsystem = "windows"]`, so the kernel never allocates a console
/// for the parent and the child kb-mcp inherits an empty console handle = a true
/// 0-flash launch. When the sibling is absent (e.g. `cargo install --path
/// kb-mcp` installs only the main binary) fall back to the legacy
/// console-visible Action so a dev install still yields a working daemon.
///
/// This is the only part of the registration path that touches the filesystem;
/// keeping it separate from [`build_register_script`] is what makes the script
/// itself testable without a real install layout.
pub fn resolve_action_target(binary_path: &Path) -> ActionTarget {
    let legacy = || ActionTarget {
        execute_path: binary_path.to_path_buf(),
        serve_argument: SERVE_SUBCOMMAND.to_string(),
        used_svc_launcher: false,
    };
    match binary_path.parent() {
        Some(dir) => {
            let svc = dir.join("kb-mcp-svc.exe");
            if svc.exists() {
                ActionTarget {
                    execute_path: svc,
                    serve_argument: String::new(),
                    used_svc_launcher: true,
                }
            } else {
                legacy()
            }
        }
        None => legacy(),
    }
}

/// Wrap a path so the **child's** command-line parser reads it as one argument.
///
/// The `-Argument` value is not re-parsed by PowerShell: Task Scheduler stores
/// it and hands it to the child as its raw command line, where
/// `CommandLineToArgvW` splits on whitespace. So `C:\Users\John Doe\...` becomes
/// two arguments and `--config` silently loses its value — at the next logon,
/// with the daemon's stdio nulled by `kb-mcp-svc`, which is the least visible
/// failure this codebase has.
///
/// Double quotes are safe to add unconditionally: `"` is not a legal character
/// in a Windows path, so there is nothing inside to escape, and a file path
/// cannot end in `\`, so the trailing-backslash rule of `CommandLineToArgvW`
/// cannot bite either. The separate PowerShell-level quoting (`'` doubling)
/// happens once over the assembled clause in [`build_register_script`].
fn quote_child_argument(path: &Path) -> String {
    format!("\"{}\"", path.display())
}

/// (v0.8.3 hot-fix) Render the PowerShell script that registers the task at the
/// root path via the `Register-ScheduledTask` cmdlet's **Action / Trigger /
/// Settings** parameter set. That parameter set is the only proven path that
/// works under a user-level (non-elevated) shell — see the module-level
/// doc-comment for the history of the `schtasks /Create` and
/// `Register-ScheduledTask -Xml` failures.
///
/// `service_name` is upstream-validated by `validate_service_name`
/// (= `[a-zA-Z0-9_-]+`) so it cannot contain `'`. Paths from `InstallContext`
/// may include the user's profile directory — accounts like `O'Brien` would
/// produce paths with `'`, which we double-escape per PowerShell single-quote
/// string rules (= `'` → `''`).
///
/// Pure: no filesystem access, no process spawn. The sibling probe lives in
/// [`resolve_action_target`].
///
/// The Action carries `--config <config_home>\kb-mcp.toml` (BU-07). Two levels
/// of quoting meet here and they are not the same quoting: the inner one is for
/// the **child's** `CommandLineToArgvW` ([`quote_child_argument`]), the outer
/// one is for the PowerShell single-quoted string this function emits (`'` →
/// `''`). The outer one is applied once, over the assembled clause, so a path
/// containing an apostrophe cannot terminate the string early no matter which
/// half it landed in.
pub fn build_register_script(
    service_name: &str,
    target: &ActionTarget,
    config_home: &Path,
    auto_start: bool,
    force: bool,
) -> String {
    let task = task_name(service_name);
    let bin_escaped = target
        .execute_path
        .display()
        .to_string()
        .replace('\'', "''");
    let home_escaped = config_home.display().to_string().replace('\'', "''");
    let auto_start_val = if auto_start { "$true" } else { "$false" };
    let force_clause = if force { " -Force" } else { "" };

    // Task Scheduler takes exactly one `-Argument`, so the words are joined
    // here rather than concatenated from two places.
    let mut words: Vec<String> = Vec::with_capacity(3);
    if !target.serve_argument.is_empty() {
        words.push(target.serve_argument.clone());
    }
    words.push("--config".to_string());
    words.push(quote_child_argument(
        &config_home.join(super::render::SERVICE_CONFIG_FILE),
    ));
    let argument = format!(" -Argument '{}'", words.join(" ").replace('\'', "''"));

    // `$ErrorActionPreference='Stop'` ensures cmdlet failures propagate as
    // non-zero exit codes. `$trigger.Enabled = $false` honors --no-auto-start
    // at the OS layer (= the LogonTrigger is registered but inert). The
    // `-User "$env:USERDOMAIN\$env:USERNAME"` on the trigger pins the
    // logon target to the current user (= matches the principal the cmdlet
    // auto-constructs for registration, no admin needed).
    format!(
        "$ErrorActionPreference='Stop'; \
         $action = New-ScheduledTaskAction -Execute '{bin}'{argument} -WorkingDirectory '{home}'; \
         $trigger = New-ScheduledTaskTrigger -AtLogOn -User \"$env:USERDOMAIN\\$env:USERNAME\"; \
         $trigger.Enabled = {auto_start}; \
         $settings = New-ScheduledTaskSettingsSet -RestartCount 3 -RestartInterval (New-TimeSpan -Minutes 1) -Priority 7; \
         Register-ScheduledTask -TaskName '{name}' -Action $action -Trigger $trigger -Settings $settings -RunLevel Limited -Description 'kb-mcp loopback HTTP MCP server ({name})'{force} | Out-Null",
        bin = bin_escaped,
        argument = argument,
        home = home_escaped,
        auto_start = auto_start_val,
        name = task,
        force = force_clause,
    )
}

/// The one place this backend starts `powershell.exe`.
///
/// Concentrating the spawn here is what keeps the UTF-8 output prelude from
/// being forgotten: a new script reaches PowerShell only through this function,
/// and this function always applies [`with_utf8_output`]. Without it every
/// message below is decoded from the active code page as if it were UTF-8 — see
/// [`super::powershell`] for the measurements.
/// (AU-19) Ask Task Scheduler for one task's state, as a bare word.
///
/// Replaces `schtasks /Query /FO CSV /NH`, whose output was grepped for the
/// substring `Running` across the **whole row** — and the first column of that
/// row is the task name. A service installed as `--service-name Running`
/// therefore reported Running forever, whatever the task was doing.
///
/// `TaskState` is a typed value rather than a column of localized CSV, so the
/// name can no longer reach the state parse. `service_name` is upstream-
/// validated to `[a-zA-Z0-9_-]+`, so it cannot close the quote.
///
/// `-TaskPath '\'` is not decoration. `-TaskName` on its own matches across the
/// **whole** Task Scheduler namespace, so a same-named task in any subfolder
/// would come back too and `.State` would print one word per match — several
/// lines, which [`parse_task_state`] cannot read, turning a running root task
/// into `Stopped`. The `schtasks /Query /TN <name>` call this replaces was an
/// exact root lookup, and pinning the path is what preserves that.
///
/// A missing task makes `Get-ScheduledTask` write to stderr and exit non-zero,
/// which the caller reads as `NotFound`.
fn build_status_script(task: &str) -> String {
    format!(
        "$ErrorActionPreference='Stop'; \
         (Get-ScheduledTask -TaskPath '\\' -TaskName '{task}').State"
    )
}

/// Map the `TaskState` word to a [`ServiceState`].
///
/// **This reports the state of the scheduled task, not of the daemon.** With
/// the `kb-mcp-svc.exe` launcher the task detach-spawns the daemon and exits
/// immediately, so a perfectly healthy daemon leaves the task `Ready`. That was
/// equally true of the `schtasks` version this replaces — the fix here is the
/// name collision, not the meaning. Knowing whether the daemon is up is an HTTP
/// question (`/api/admin/status`), which is the route the tray takes (AU-65).
fn parse_task_state(state: &str) -> ServiceState {
    if state.trim().eq_ignore_ascii_case("Running") {
        ServiceState::Running {
            uptime_secs: 0,
            bind: None,
            kb_path: None,
            model: None,
        }
    } else {
        ServiceState::Stopped {
            bind: None,
            kb_path: None,
        }
    }
}

fn run_powershell_capture(script: &str, what: &str) -> Result<std::process::Output> {
    Command::new("powershell")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            &with_utf8_output(script),
        ])
        .output()
        .with_context(|| format!("powershell {what} invocation failed"))
}

/// (AU-67) What to print when the svc launcher is missing and the
/// console-visible Action was registered instead.
///
/// The fallback produces a working install, so this is a warning and not an
/// error — but it must not stay silent. `kb-mcp-svc.exe` was not attached to a
/// release at all until v0.14.0, so **every** installation from a release
/// archive between v0.9.0 and v0.13.1 took this branch: users saw a console
/// window at each logon while the v0.9.1 fix meant to prevent it appeared to be
/// in place. Nothing reported the substitution, and the distribution bug stayed
/// invisible for eight releases. Silence about an "optional optimisation" not
/// applying is what made a detectable defect undetectable.
///
/// Kept separate from the printing so the wording — specifically the archive
/// name a user has to go find — is pinned by a test.
///
/// `auto_start` decides when the console actually appears. `--no-auto-start`
/// registers the task with `$trigger.Enabled = $false`, so it does not run at
/// logon at all; promising a window "at every logon" there would describe a
/// symptom the user will never see and make the rest of the advice look wrong.
fn svc_fallback_warning(binary_path: &Path, auto_start: bool) -> String {
    let when = if auto_start {
        "at every logon"
    } else {
        "each time the task is started"
    };
    format!(
        "warning: kb-mcp-svc.exe was not found next to {}.\n         \
         Registered the task to run kb-mcp.exe directly, which shows a\n         \
         console window {when}. To avoid it, extract\n         \
         kb-mcp-svc-x86_64-pc-windows-msvc.zip from the release next to\n         \
         kb-mcp.exe, then re-run the same `kb-mcp service install` command\n         \
         you used before with `--force` added. A bare `service install`\n         \
         would reset the service name, auto-start and bind to their defaults.",
        binary_path.display()
    )
}

/// Register the scheduled task: resolve the Action target, render the script,
/// hand it to PowerShell.
fn register_via_powershell(
    service_name: &str,
    binary_path: &Path,
    config_home: &Path,
    auto_start: bool,
    force: bool,
) -> Result<()> {
    let target = resolve_action_target(binary_path);
    let script = build_register_script(service_name, &target, config_home, auto_start, force);
    let out = run_powershell_capture(&script, "Register-ScheduledTask")?;
    if !out.status.success() {
        // Diagnostic decode: a failed registration must still report why it
        // failed, so an undecodable byte must not turn into a second error that
        // replaces the first.
        let stderr = decode_diagnostic(&out.stderr);
        let stdout = decode_diagnostic(&out.stdout);
        return Err(anyhow!(
            "PowerShell Register-ScheduledTask failed (status: {})\nstderr: {}\nstdout: {}",
            out.status,
            stderr.trim(),
            stdout.trim(),
        ));
    }
    // AU-67, after the success check: the warning is worded in the past tense
    // ("Registered the logon task ..."), which is only true once the cmdlet has
    // actually run. Printed before it, a registration that then fails — the
    // reproducible case being an existing task without `--force` — would emit
    // "Registered ..." immediately followed by "Register-ScheduledTask failed",
    // and nothing would have been registered at all. A failed install has one
    // actionable message, and it is not this one.
    if !target.used_svc_launcher {
        eprintln!("{}", svc_fallback_warning(binary_path, auto_start));
    }
    Ok(())
}

impl ServiceBackend for TaskScheduler {
    fn install(&self, ctx: &InstallContext) -> Result<()> {
        // v0.8.3: skip XML entirely — pass Action/Trigger/Settings directly
        // to Register-ScheduledTask (= the parameter set that auto-populates
        // the Principal from the current logon identity, the only path that
        // works without admin elevation on a non-elevated shell).
        register_via_powershell(
            &ctx.service_name,
            &ctx.binary_path,
            &ctx.config_home,
            ctx.auto_start,
            ctx.force,
        )?;
        if ctx.auto_start {
            run_schtasks(&["/Run", "/TN", &task_name(&ctx.service_name)])?;
        }
        Ok(())
    }
    fn uninstall(&self, service_name: &str) -> Result<()> {
        let task = task_name(service_name);
        let _ = run_schtasks(&["/End", "/TN", &task]);
        let _ = run_schtasks(&["/Delete", "/TN", &task, "/F"]);
        Ok(())
    }
    fn status(&self, service_name: &str) -> Result<ServiceState> {
        let task = task_name(service_name);
        let out = run_powershell_capture(&build_status_script(&task), "Get-ScheduledTask")?;
        if !out.status.success() {
            // The script writes nothing and exits non-zero when the task does
            // not exist, which is the same answer `schtasks /Query` gave.
            return Ok(ServiceState::NotFound);
        }
        Ok(parse_task_state(&decode_value(
            &out.stdout,
            "Get-ScheduledTask stdout",
        )?))
    }
    fn list(&self) -> Result<Vec<(String, ServiceState)>> {
        let out = Command::new("schtasks")
            .args(["/Query", "/FO", "CSV", "/NH"])
            .output()
            .context("schtasks /Query 全体 実行失敗")?;
        // Same as `status`: `schtasks` output is code-page encoded and out of
        // reach of the PowerShell prelude, but only the ASCII `kb-mcp-` prefix
        // is matched here. A separate defect does live on this path — a service
        // literally named `Running` would report Running forever, because the
        // status check greps the whole CSV row rather than the state column.
        // Moving both calls to `Get-ScheduledTask`, whose `TaskState` is a typed
        // value rather than a localized CSV, retires the encoding question and
        // that defect together.
        let stdout = String::from_utf8_lossy(&out.stdout);
        let mut result = Vec::new();
        for line in stdout.lines() {
            if let Some(name_field) = line.split(',').next() {
                let cleaned = name_field.trim_matches('"').trim_start_matches('\\');
                if let Some(rest) = cleaned.strip_prefix("kb-mcp-") {
                    let state = self.status(rest)?;
                    result.push((rest.to_string(), state));
                }
            }
        }
        Ok(result)
    }
    fn stop(&self, service_name: &str) -> Result<()> {
        run_schtasks(&["/End", "/TN", &task_name(service_name)])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::service::powershell::decode_value;

    /// AU-19. The bug was that the state check grepped the whole `schtasks`
    /// CSV row, whose first column is the task name — so a service installed
    /// as `--service-name Running` reported Running forever.
    ///
    /// The name now cannot reach the state parse at all: the script asks for
    /// `.State` and the parser sees only that word. Pinning it here means the
    /// two halves cannot drift back together.
    #[test]
    fn a_service_named_running_is_not_reported_running() {
        // What Task Scheduler returns for a task that is not running, for a
        // task that happens to be *called* Running.
        assert!(matches!(
            parse_task_state("Ready"),
            ServiceState::Stopped { .. }
        ));

        let script = build_status_script(&task_name("Running"));
        assert!(
            script.contains("-TaskName 'kb-mcp-Running').State"),
            "the script must read the State property, not the whole row: {script}"
        );
        assert!(
            !script.contains("schtasks"),
            "the CSV path is what carried the defect: {script}"
        );
    }

    /// `-TaskName` alone searches the entire Task Scheduler namespace, so a
    /// same-named task in a subfolder would add a second line to `.State` and
    /// [`parse_task_state`] would read the pair as "not Running". The
    /// `schtasks /TN` call this replaced was an exact root lookup; the path
    /// restriction is what keeps that property.
    /// (AU-33) The tray computes the same name from its own copy, in a module
    /// this crate cannot import. A divergence compiles fine and shows up only
    /// as a tray that controls a task nobody registered — so both sides pin
    /// the literal. The counterpart is `task_name_uses_kb_mcp_prefix` in
    /// `crates/kb-mcp-tray/src/daemon.rs`; the two must agree.
    #[test]
    fn task_name_matches_the_tray_copy() {
        assert_eq!(task_name("personal"), "kb-mcp-personal");
        assert!(
            !task_name("personal").starts_with('\\'),
            "a leading backslash makes the cmdlet look for a path that does not exist"
        );
    }

    #[test]
    fn status_lookup_is_pinned_to_the_root_task_folder() {
        let script = build_status_script(&task_name("svc"));
        assert!(
            script.contains(r"-TaskPath '\'"),
            "the lookup must be restricted to the root folder: {script}"
        );

        // The shape the restriction protects against: two matches, two words.
        assert!(
            matches!(
                parse_task_state("Running\r\nReady"),
                ServiceState::Stopped { .. }
            ),
            "multiple matches cannot be parsed, which is why they must not happen"
        );
    }

    #[test]
    fn task_state_words_map_to_service_state() {
        assert!(matches!(
            parse_task_state("Running"),
            ServiceState::Running { .. }
        ));
        // Task Scheduler's other states are all "not running" for our purposes.
        for word in ["Ready", "Disabled", "Queued", "Unknown", ""] {
            assert!(
                matches!(parse_task_state(word), ServiceState::Stopped { .. }),
                "{word:?} must not read as Running"
            );
        }
        // PowerShell prints the enum name; tolerate whitespace and casing
        // rather than depending on how it formats.
        assert!(matches!(
            parse_task_state(" running \r\n"),
            ServiceState::Running { .. }
        ));
    }

    /// AU-67. The point of the warning is that a user can act on it, which
    /// means naming the archive to download — a message saying only "svc not
    /// found" would leave them exactly where the silent fallback did.
    #[test]
    fn svc_fallback_warning_tells_the_user_what_to_do() {
        let msg = svc_fallback_warning(Path::new("C:\\bin\\kb-mcp.exe"), true);

        assert!(
            msg.contains("kb-mcp-svc-x86_64-pc-windows-msvc.zip"),
            "must name the archive to extract: {msg}"
        );
        assert!(
            msg.contains("console window"),
            "must name the symptom the user will otherwise see: {msg}"
        );
        assert!(
            msg.contains("--force"),
            "must say how to redo the install: {msg}"
        );
        // A bare `kb-mcp service install --force` restores the *default*
        // service name, auto-start and bind. Handing that to someone who
        // installed with `--service-name` / `--no-auto-start` / `--bind` would
        // point them at the wrong service, or silently reset the one they
        // meant. The recovery step has to be *their* command plus `--force`.
        assert!(
            msg.contains("the same `kb-mcp service install` command"),
            "must tell the user to repeat their own invocation: {msg}"
        );
        assert!(
            msg.contains("reset the service name, auto-start and bind"),
            "must say what a bare re-install would clobber: {msg}"
        );
        assert!(
            msg.contains("C:\\bin\\kb-mcp.exe"),
            "must say where it looked: {msg}"
        );
    }

    /// The message is one literal split across source lines with `\` joins,
    /// which silently eat the newline *and* the following indentation — a
    /// miscount still compiles and produces ragged output. Pin the shape.
    /// `--no-auto-start` registers the task with `$trigger.Enabled = $false`,
    /// so it never fires at logon. Promising a window "at every logon" there
    /// describes a symptom the user will never see, which makes the rest of the
    /// advice look wrong too.
    #[test]
    fn svc_fallback_warning_matches_when_the_task_actually_runs() {
        let path = Path::new("C:\\bin\\kb-mcp.exe");

        let at_logon = svc_fallback_warning(path, true);
        assert!(
            at_logon.contains("at every logon"),
            "an auto-start install does flash on each logon: {at_logon}"
        );

        let on_demand = svc_fallback_warning(path, false);
        assert!(
            !on_demand.contains("logon"),
            "--no-auto-start never fires at logon, so the message must not say so: {on_demand}"
        );
        assert!(
            on_demand.contains("each time the task is started"),
            "it must still say when the window appears: {on_demand}"
        );

        // The actionable half is identical either way.
        for msg in [&at_logon, &on_demand] {
            assert!(msg.contains("kb-mcp-svc-x86_64-pc-windows-msvc.zip"));
            assert!(msg.contains("--force"));
        }
    }

    #[test]
    fn svc_fallback_warning_lines_are_aligned() {
        let msg = svc_fallback_warning(Path::new("C:\\bin\\kb-mcp.exe"), true);
        let mut lines = msg.lines();

        assert!(
            lines.next().is_some_and(|l| l.starts_with("warning: ")),
            "first line carries the prefix: {msg}"
        );
        for line in lines {
            assert!(
                line.starts_with("         ") && !line.starts_with("          "),
                "continuation lines are indented to exactly 9 columns: {line:?}"
            );
        }
    }

    #[test]
    fn resolve_action_target_reports_which_branch_it_took() {
        // No `kb-mcp-svc.exe` can exist next to a path under a directory that
        // does not exist, so this exercises the fallback without a fixture.
        let missing = Path::new("C:\\kb-mcp-au67-no-such-dir\\kb-mcp.exe");
        let target = resolve_action_target(missing);
        assert!(
            !target.used_svc_launcher,
            "fallback must be reported, not silent"
        );
        assert_eq!(target.execute_path, missing);
        assert_eq!(target.serve_argument, SERVE_SUBCOMMAND);
    }

    /// BU-07. The Action names the config file explicitly, so the daemon's own
    /// `kb-mcp.toml` is `ConfigSource::Explicit` = Trusted regardless of what
    /// `KB_MCP_CONFIG_HOME` happened to be set to at install time.
    #[test]
    fn the_action_names_the_config_file_on_both_branches() {
        let cfg = Path::new("C:\\cfg");

        let svc = ActionTarget {
            execute_path: PathBuf::from("C:\\bin\\kb-mcp-svc.exe"),
            serve_argument: String::new(),
            used_svc_launcher: true,
        };
        let script = build_register_script("kb-mcp", &svc, cfg, true, false);
        assert!(
            script.contains("-Argument '--config \"C:\\cfg\\kb-mcp.toml\"'"),
            "the svc launcher gets the flag and no subcommand: {script}"
        );

        let console = ActionTarget {
            execute_path: PathBuf::from("C:\\bin\\kb-mcp.exe"),
            serve_argument: SERVE_SUBCOMMAND.to_string(),
            used_svc_launcher: false,
        };
        let script = build_register_script("kb-mcp", &console, cfg, true, false);
        assert!(
            script.contains("-Argument 'serve --config \"C:\\cfg\\kb-mcp.toml\"'"),
            "the console binary gets the subcommand and the flag, in that order: {script}"
        );
    }

    /// The `-Argument` value becomes the child's raw command line, so a space in
    /// the config home has to survive as **one** argument. Reconstructing the
    /// child's argv is the only assertion that actually proves it — a
    /// `contains` on the script would pass for an unquoted path too.
    #[test]
    fn a_space_in_the_config_home_stays_one_child_argument() {
        let target = ActionTarget {
            execute_path: PathBuf::from("C:\\bin\\kb-mcp.exe"),
            serve_argument: SERVE_SUBCOMMAND.to_string(),
            used_svc_launcher: false,
        };
        let script = build_register_script(
            "kb-mcp",
            &target,
            Path::new("C:\\Users\\John Doe\\AppData\\Roaming\\kb-mcp\\kb-mcp"),
            true,
            false,
        );

        let argv = child_argv_of(&script);
        assert_eq!(
            argv,
            vec![
                "serve".to_string(),
                "--config".to_string(),
                "C:\\Users\\John Doe\\AppData\\Roaming\\kb-mcp\\kb-mcp\\kb-mcp.toml".to_string(),
            ],
            "the path must arrive as one argument, or `--config` silently loses \
             its value at the next logon: {script}"
        );
    }

    /// An apostrophe in the profile directory has to survive the *PowerShell*
    /// quoting as well, and it is inside the `-Argument` string now, not only
    /// inside `-Execute` / `-WorkingDirectory`.
    #[test]
    fn an_apostrophe_in_the_config_home_is_doubled_inside_the_argument_clause() {
        let target = ActionTarget {
            execute_path: PathBuf::from("C:\\bin\\kb-mcp-svc.exe"),
            serve_argument: String::new(),
            used_svc_launcher: true,
        };
        let script = build_register_script(
            "kb-mcp",
            &target,
            Path::new("C:\\Users\\O'Brien\\cfg"),
            true,
            false,
        );
        assert!(
            script.contains("-Argument '--config \"C:\\Users\\O''Brien\\cfg\\kb-mcp.toml\"'"),
            "an unescaped apostrophe would close the PowerShell string early: {script}"
        );
        assert_eq!(
            child_argv_of(&script),
            vec![
                "--config".to_string(),
                "C:\\Users\\O'Brien\\cfg\\kb-mcp.toml".to_string(),
            ]
        );
    }

    /// Undo both quoting layers in the same order the real chain does —
    /// PowerShell's single-quoted string first, then `CommandLineToArgvW` — so
    /// the tests above assert what the child actually receives.
    fn child_argv_of(script: &str) -> Vec<String> {
        let after = script
            .split_once("-Argument '")
            .expect("the Action must carry an -Argument clause")
            .1;
        // The clause ends at the first apostrophe that is not doubled.
        let mut raw = String::new();
        let mut chars = after.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '\'' {
                if chars.peek() == Some(&'\'') {
                    chars.next();
                    raw.push('\'');
                    continue;
                }
                break;
            }
            raw.push(c);
        }
        split_command_line(&raw)
    }

    /// `CommandLineToArgvW` for the shapes this code emits: bare words and
    /// fully double-quoted words. Paths cannot contain `"`, and the escaping
    /// rules for backslashes only apply immediately before one, so neither
    /// arises here — a full implementation would assert nothing extra.
    fn split_command_line(raw: &str) -> Vec<String> {
        let mut out = Vec::new();
        let mut cur = String::new();
        let mut in_quotes = false;
        let mut started = false;
        for c in raw.chars() {
            match c {
                '"' => {
                    in_quotes = !in_quotes;
                    started = true;
                }
                c if c.is_whitespace() && !in_quotes => {
                    if started {
                        out.push(std::mem::take(&mut cur));
                        started = false;
                    }
                }
                c => {
                    cur.push(c);
                    started = true;
                }
            }
        }
        if started {
            out.push(cur);
        }
        out
    }

    /// End-to-end evidence that the prelude changes what actually comes back
    /// through the pipe. Asserting that the script string contains the prelude
    /// would only restate the code; this spawns a real `powershell.exe`.
    ///
    /// The text is built from codepoints inside PowerShell so that nothing
    /// between this file and the child re-encodes it on the way in, leaving the
    /// pipe as the only thing under test. With the prelude the answer is UTF-8
    /// on any code page; without it a ja-JP host emits CP932 (rejected by
    /// `decode_value`) and a host on a Latin code page emits best-fit `??`
    /// (decodes cleanly but compares unequal), so on either the fix is what
    /// makes this pass.
    ///
    /// It does **not** guard the prelude everywhere. A host whose active code
    /// page is already UTF-8 (CP65001, which Windows 11's "Beta: Use Unicode
    /// UTF-8" option turns on) emits UTF-8 with or without it, and there this
    /// passes either way. That still covers exactly the hosts the fix exists
    /// for — but if the Windows CI runner ever moves to a UTF-8 code page, this
    /// stops being a regression guard without saying so.
    #[test]
    fn powershell_output_round_trips_non_ascii() {
        let script =
            "Write-Output ([char]::ConvertFromUtf32(0x65E5) + [char]::ConvertFromUtf32(0x672C))";
        let out = run_powershell_capture(script, "encoding self-test").expect("spawn powershell");
        assert!(
            out.status.success(),
            "powershell failed: {}",
            decode_diagnostic(&out.stderr)
        );
        let stdout = decode_value(&out.stdout, "stdout").expect("stdout must decode as UTF-8");
        assert_eq!(stdout.trim(), "日本");
    }
}
