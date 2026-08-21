// tests/service_install_integration.rs
// Integration tests for `groove service install/uninstall/status/list`.
// Unit-level tests (= no OS service register) run on every `cargo test`.
// Dangerous tests (= OS service register, marked #[ignore]) run only on
// `cargo test -- --ignored`.

mod common;
use common::temp::TempRoot;

#[allow(unused_imports)]
use std::path::PathBuf;

#[test]
fn install_resolves_kb_path_from_flag() {
    let tmp = TempRoot::new("install_flag");
    let kb = tmp.path().join("my-kb");
    std::fs::create_dir_all(&kb).unwrap();
    let result = grooveseek::service::install::resolve_kb_path(Some(kb.clone()), None).unwrap();
    assert_eq!(result, kb);
}

#[test]
fn install_resolves_kb_path_from_toml_when_no_flag() {
    let tmp = TempRoot::new("install_toml");
    let kb = tmp.path().join("toml-kb");
    std::fs::create_dir_all(&kb).unwrap();
    let toml_path = tmp.path().join("groove.toml");
    // TOML literal strings ('...') do not interpret backslash escapes, which
    // matters on Windows where path separators are `\` (a double-quoted TOML
    // string would treat `\U` as a unicode escape and fail to parse).
    // Match the Config schema (top-level `kb_path`, no `[index]` section)
    // — `Config` uses `deny_unknown_fields` so unrecognised tables would crash
    // `groove serve` at startup. TOML literal strings ('...') do not
    // interpret backslash escapes (Windows `\U` issue).
    std::fs::write(&toml_path, format!("kb_path = '{}'\n", kb.display())).unwrap();
    let result = grooveseek::service::install::resolve_kb_path(None, Some(toml_path)).unwrap();
    assert_eq!(result, kb);
}

#[test]
fn install_resolve_kb_path_errors_when_neither_provided() {
    assert!(grooveseek::service::install::resolve_kb_path(None, None).is_err());
}

#[cfg(target_os = "linux")]
#[test]
fn linux_unit_template_renders_correctly() {
    use grooveseek::service::*;
    let ctx = InstallContext {
        service_name: "groove".into(),
        kb_path: PathBuf::from("/home/u/kb"),
        bind: "127.0.0.1:3100".into(),
        config_home: PathBuf::from("/home/u/.config/groove/groove"),
        binary_path: PathBuf::from("/home/u/.cargo/bin/groove"),
        auto_start: true,
        force: false,
    };
    let unit = grooveseek::service::linux::render_unit(&ctx).unwrap();
    assert!(unit.contains("[Unit]"));
    assert!(unit.contains("ExecStart=/home/u/.cargo/bin/groove serve"));
    assert!(unit.contains("WorkingDirectory=/home/u/.config/groove/groove"));
    assert!(unit.contains("Description=groove loopback HTTP MCP server (groove)"));
    assert!(unit.contains("Restart=on-failure"));
}

#[cfg(target_os = "macos")]
#[test]
fn macos_plist_template_renders_correctly() {
    use grooveseek::service::*;
    let ctx = InstallContext {
        service_name: "groove".into(),
        kb_path: PathBuf::from("/Users/me/kb"),
        bind: "127.0.0.1:3100".into(),
        config_home: PathBuf::from("/Users/me/Library/Application Support/groove/groove"),
        binary_path: PathBuf::from("/Users/me/.cargo/bin/groove"),
        auto_start: true,
        force: false,
    };
    let plist = grooveseek::service::macos::render_plist(&ctx);
    assert!(plist.contains("<key>Label</key>"));
    assert!(plist.contains("<string>com.groove.groove</string>"));
    assert!(plist.contains("<string>/Users/me/.cargo/bin/groove</string>"));
    assert!(plist.contains("<string>serve</string>"));
    assert!(plist.contains("<key>WorkingDirectory</key>"));
}

// ---------------------------------------------------------------------------
// Windows Task Scheduler registration (AU-07 / AU-08).
//
// `register_via_powershell` used to build the script and spawn PowerShell in
// one function, so the v0.8.3 (Action/Trigger/Settings parameter set) and
// v0.9.1 (svc-preferred Action with no `serve` argument) hot-fixes had no
// regression coverage at all — both are one-line edits away from silently
// reverting, and both fail only at the *next logon*. Splitting the pure
// `build_register_script` from the filesystem-probing `resolve_action_target`
// makes each half assertable without touching the real Task Scheduler.
// ---------------------------------------------------------------------------

#[cfg(target_os = "windows")]
fn svc_action_target() -> grooveseek::service::windows::ActionTarget {
    grooveseek::service::windows::ActionTarget {
        execute_path: PathBuf::from("C:\\bin\\groove-svc.exe"),
        serve_argument: String::new(),
        used_svc_launcher: true,
    }
}

#[cfg(target_os = "windows")]
fn console_action_target() -> grooveseek::service::windows::ActionTarget {
    grooveseek::service::windows::ActionTarget {
        execute_path: PathBuf::from("C:\\bin\\groove.exe"),
        serve_argument: "serve".to_string(),
        used_svc_launcher: false,
    }
}

/// v0.8.3 regression: the script must use the **Action / Trigger / Settings**
/// parameter set. `-Xml` (v0.8.2) does not auto-populate a `<UserId>` in the
/// Principal, so registration returns HRESULT 0x80070005 from a non-elevated
/// shell. `-RunLevel Limited` is what keeps the task user-level (spec § Q4
/// promised "no admin required").
#[cfg(target_os = "windows")]
#[test]
fn windows_register_script_uses_action_trigger_settings_parameter_set() {
    use grooveseek::service::windows::build_register_script;
    let script = build_register_script(
        "groove",
        &svc_action_target(),
        &PathBuf::from("C:\\Users\\me\\AppData\\Roaming\\groove\\groove"),
        true,
        false,
    );
    assert!(script.contains("New-ScheduledTaskAction -Execute"));
    assert!(script.contains("New-ScheduledTaskTrigger -AtLogOn"));
    assert!(script.contains("New-ScheduledTaskSettingsSet"));
    assert!(script.contains("-Action $action"));
    assert!(script.contains("-Trigger $trigger"));
    assert!(script.contains("-Settings $settings"));
    assert!(script.contains("-RunLevel Limited"));
    assert!(
        !script.contains("-Xml"),
        "the -Xml parameter set was abandoned in v0.8.3 (Access is denied without elevation)"
    );
    assert!(script.contains("-TaskName 'groove-groove'"));
    assert!(script.contains("-WorkingDirectory 'C:\\Users\\me\\AppData\\Roaming\\groove\\groove'"));
}

/// v0.9.1 regression, installer half of the "exactly one `serve`" invariant:
/// when the Action points at `groove-svc.exe` it must pass **no** argument,
/// because the svc launcher prepends `serve` itself. The svc half is asserted
/// in `crates/groove-svc/src/main.rs::tests`.
#[cfg(target_os = "windows")]
#[test]
fn windows_register_script_omits_serve_argument_for_the_svc_launcher() {
    use grooveseek::service::windows::build_register_script;
    let script = build_register_script(
        "groove",
        &svc_action_target(),
        &PathBuf::from("C:\\cfg"),
        true,
        false,
    );
    assert!(script.contains("-Execute 'C:\\bin\\groove-svc.exe'"));
    // (BU-07) The Action now also carries `--config`, and Task Scheduler takes
    // exactly one `-Argument`, so the predicate narrows from "no -Argument at
    // all" to "the -Argument carries no `serve`" — which is what the message
    // below has always been about. `contains("serve")` alone would not do:
    // the `-Description` clause says "MCP server".
    assert!(script.contains("-Argument '--config"));
    assert!(
        !script.contains("-Argument 'serve"),
        "groove-svc adds `serve` itself; passing it here yields `groove.exe serve serve`\n{script}"
    );
}

/// The dev-install fallback (`cargo install --path grooveseek` ships no svc
/// binary) still has to produce a working daemon, so the console Action keeps
/// carrying `serve`.
#[cfg(target_os = "windows")]
#[test]
fn windows_register_script_passes_serve_to_the_console_binary() {
    use grooveseek::service::windows::build_register_script;
    let script = build_register_script(
        "groove",
        &console_action_target(),
        &PathBuf::from("C:\\cfg"),
        true,
        false,
    );
    // (BU-07) `--config` joins the same clause; the fixture's config_home is
    // `C:\cfg`, so the child's argv is fully determined here.
    assert!(script.contains(
        "-Execute 'C:\\bin\\groove.exe' -Argument 'serve --config \"C:\\cfg\\groove.toml\"'"
    ));
}

/// `--no-auto-start` is honored at the OS layer (the LogonTrigger is
/// registered but inert), and `--force` maps to `-Force`.
#[cfg(target_os = "windows")]
#[test]
fn windows_register_script_reflects_auto_start_and_force_flags() {
    use grooveseek::service::windows::build_register_script;
    let cfg = PathBuf::from("C:\\cfg");
    let enabled = build_register_script("groove", &svc_action_target(), &cfg, true, false);
    assert!(enabled.contains("$trigger.Enabled = $true"));
    assert!(!enabled.contains("-Force"));

    let inert = build_register_script("groove", &svc_action_target(), &cfg, false, true);
    assert!(inert.contains("$trigger.Enabled = $false"));
    assert!(inert.contains("-Force"));
}

/// Paths are interpolated into PowerShell single-quoted strings, so a profile
/// directory belonging to an account like `O'Brien` must be doubled (`'` →
/// `''`) or the script terminates the string early and the cmdlet misparses.
#[cfg(target_os = "windows")]
#[test]
fn windows_register_script_escapes_single_quotes_in_paths() {
    use grooveseek::service::windows::{ActionTarget, build_register_script};
    let target = ActionTarget {
        execute_path: PathBuf::from("C:\\Users\\O'Brien\\bin\\groove-svc.exe"),
        serve_argument: String::new(),
        used_svc_launcher: true,
    };
    let script = build_register_script(
        "groove",
        &target,
        &PathBuf::from("C:\\Users\\O'Brien\\cfg"),
        true,
        false,
    );
    assert!(script.contains("-Execute 'C:\\Users\\O''Brien\\bin\\groove-svc.exe'"));
    assert!(script.contains("-WorkingDirectory 'C:\\Users\\O''Brien\\cfg'"));
    // (BU-07) The apostrophe now also has to survive inside `-Argument`, which
    // carries the config path. `windows.rs` reconstructs the child's argv from
    // this to prove the quoting is not merely present but correct.
    assert!(script.contains("-Argument '--config \"C:\\Users\\O''Brien\\cfg\\groove.toml\"'"));
}

/// v0.9.1: prefer the windows-subsystem sibling so the AtLogOn trigger causes
/// no console flash.
#[cfg(target_os = "windows")]
#[test]
fn windows_resolve_action_target_prefers_the_svc_sibling_when_present() {
    use grooveseek::service::windows::resolve_action_target;
    let tmp = TempRoot::new("resolve_action_svc");
    tmp.write("groove-svc.exe", "stub");
    let target = resolve_action_target(&tmp.path().join("groove.exe"));
    assert_eq!(target.execute_path, tmp.path().join("groove-svc.exe"));
    assert_eq!(
        target.serve_argument, "",
        "the svc launcher supplies `serve` itself"
    );
}

/// Fallback when only the main binary is installed.
#[cfg(target_os = "windows")]
#[test]
fn windows_resolve_action_target_falls_back_when_sibling_is_absent() {
    use grooveseek::service::windows::resolve_action_target;
    let tmp = TempRoot::new("resolve_action_fallback");
    let bin = tmp.path().join("groove.exe");
    let target = resolve_action_target(&bin);
    assert_eq!(target.execute_path, bin);
    assert_eq!(target.serve_argument, "serve");
}

/// A parent-less path (a drive root) cannot host a sibling probe; it must take
/// the same fallback rather than panic.
#[cfg(target_os = "windows")]
#[test]
fn windows_resolve_action_target_falls_back_for_a_parentless_path() {
    use grooveseek::service::windows::resolve_action_target;
    let target = resolve_action_target(std::path::Path::new("C:\\"));
    assert_eq!(target.execute_path, PathBuf::from("C:\\"));
    assert_eq!(target.serve_argument, "serve");
}

/// v0.8.3 hot-fix smoke test: invokes `Register-ScheduledTask` via PowerShell
/// using the **Action / Trigger / Settings** parameter set — matches the
/// v0.8.3 production install path. Validates user-level root-path
/// registration without admin elevation (= spec § Q4 promise).
/// Cleans up via `Unregister-ScheduledTask` regardless of outcome.
///
/// **Logon-session sensitive.** The Task Scheduler COM API needs a
/// sufficiently privileged token to register a task for the current user;
/// calls from NTLM / service-style logon sessions (SSH sessions, WSL-bridged
/// shells, some CI agents) hit "Access is denied" even though the user is
/// the same. The user's own `powershell.exe` / Windows Terminal works.
///
/// Run manually from an interactive shell:
/// ```text
/// cargo test --test service_install_integration windows_register_scheduledtask_smoke_test -- --ignored
/// ```
///
/// Since AU-09 this also runs on the nightly `windows-latest` leg, and it
/// passes there: GitHub-hosted Windows runners execute jobs as an
/// administrator with UAC disabled, so the registration succeeds. It stays
/// `#[ignore]` because it mutates the Task Scheduler and must not run in a
/// plain `cargo test`.
#[cfg(target_os = "windows")]
#[test]
#[ignore = "mutates Windows Task Scheduler — run manually for end-to-end verify"]
fn windows_register_scheduledtask_smoke_test() {
    use std::process::Command;

    let unique = std::process::id();
    let svc_name = format!("smoke{}", unique);
    let task_name = format!("groove-{}", svc_name);

    let register_script = format!(
        "$ErrorActionPreference='Stop'; \
         $action = New-ScheduledTaskAction -Execute 'C:\\nonexistent\\groove.exe' -Argument 'serve' -WorkingDirectory 'C:\\nonexistent\\cfg'; \
         $trigger = New-ScheduledTaskTrigger -AtLogOn -User \"$env:USERDOMAIN\\$env:USERNAME\"; \
         $trigger.Enabled = $false; \
         $settings = New-ScheduledTaskSettingsSet -RestartCount 3 -RestartInterval (New-TimeSpan -Minutes 1) -Priority 7; \
         Register-ScheduledTask -TaskName '{name}' -Action $action -Trigger $trigger -Settings $settings -RunLevel Limited -Force | Out-Null",
        name = task_name,
    );
    let register = Command::new("powershell")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            &register_script,
        ])
        .output()
        .expect("powershell Register-ScheduledTask invocation failed");

    // Best-effort cleanup before assert so a failed test leaves no junk.
    let unregister_script = format!(
        "$ErrorActionPreference='SilentlyContinue'; \
         Unregister-ScheduledTask -TaskName '{name}' -Confirm:$false | Out-Null",
        name = task_name,
    );
    let _ = Command::new("powershell")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            &unregister_script,
        ])
        .output();

    assert!(
        register.status.success(),
        "Register-ScheduledTask failed (status: {:?})\nstderr: {}\nstdout: {}",
        register.status.code(),
        String::from_utf8_lossy(&register.stderr),
        String::from_utf8_lossy(&register.stdout),
    );
}

/// The macOS backend, end to end, through the command line.
///
/// **L-3: there was no such test, and no leg that could have run one.** The
/// launchd backend is reached only from `#[ignore]` territory, and nightly ran
/// `ubuntu` and `windows` — so `install`, `status` and `uninstall` on macOS
/// were never executed by anything, for the same reason AU-09 gave for adding
/// the Windows leg.
///
/// It goes through the binary rather than `LaunchAgent` directly, because
/// `ServiceBackend` is `pub(crate)` and because the argument wiring is part of
/// what was unverified.
///
/// **What it asserts, and what it deliberately does not.** launchd knowing the
/// label is the whole question here: `status` telling `not found` from anything
/// else is what proves `bootstrap` reached `gui/<uid>`. Whether the daemon then
/// *runs* depends on the embedding model loading and on a free port, neither of
/// which this is about — so `running` is accepted and so is `stopped`, and only
/// `not found` fails.
///
/// Measured before this was written: on a GitHub-hosted mac `launchctl
/// managername` is `Aqua`, `bootstrap gui/501` exits 0, and `RunAtLoad` really
/// does start the program. `.dev/knowledge/macos-ci-launchd-measurements.md`.
#[cfg(target_os = "macos")]
#[test]
#[ignore = "bootstraps a real LaunchAgent — nightly's macOS leg, or run manually"]
fn macos_install_status_uninstall_round_trip() {
    use std::path::PathBuf;
    use std::process::Command;

    fn groove() -> PathBuf {
        PathBuf::from(env!("CARGO_BIN_EXE_groove"))
    }

    let root = TempRoot::new("groove-macos-svc");
    let kb = root.path().join("kb");
    std::fs::create_dir_all(&kb).expect("create kb dir");
    let config_home = root.path().join("cfg");

    // Unique per run, and `validate_service_name`-clean.
    let name = format!("smoke{}", std::process::id());
    let plist = PathBuf::from(std::env::var("HOME").expect("HOME is set on macOS"))
        .join("Library/LaunchAgents")
        .join(format!("com.groove.{name}.plist"));

    // `GROOVE_CONFIG_HOME` keeps the installer out of the real config
    // directory; the plist still lands in the real `~/Library/LaunchAgents`,
    // because that is the only place launchd reads user agents from. The
    // cleanup below is what keeps that from leaking.
    let run = |args: &[&str]| {
        Command::new(groove())
            .args(args)
            .env("GROOVE_CONFIG_HOME", &config_home)
            .output()
            .expect("groove service invocation")
    };

    let install = run(&[
        "service",
        "install",
        "--service-name",
        &name,
        "--kb-path",
        kb.to_str().unwrap(),
        // Port 0 so a busy 3100 cannot make this fail for an unrelated reason.
        "--bind",
        "127.0.0.1:0",
    ]);

    // Best-effort cleanup before the asserts, so a failure leaves no agent
    // loaded and no plist behind — the shape the Windows smoke test uses.
    let uninstall = run(&["service", "uninstall", "--service-name", &name]);
    let after = run(&["service", "status", "--service-name", &name]);
    let _ = std::fs::remove_file(&plist);

    assert!(
        install.status.success(),
        "service install failed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&install.stdout),
        String::from_utf8_lossy(&install.stderr),
    );
    assert!(
        uninstall.status.success(),
        "service uninstall failed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&uninstall.stdout),
        String::from_utf8_lossy(&uninstall.stderr),
    );
    assert!(!plist.exists(), "uninstall must remove {}", plist.display());
    let after_text = String::from_utf8_lossy(&after.stdout);
    assert!(
        after_text.contains("not found"),
        "after uninstall the label must be gone from launchd, got: {after_text}"
    );
}

/// The other half: while it is installed, launchd knows the label.
///
/// Separate from the round trip because it has to look **between** install and
/// uninstall, and doing that inside the round trip would mean skipping the
/// cleanup-before-assert order that keeps a failure from leaving an agent
/// loaded.
#[cfg(target_os = "macos")]
#[test]
#[ignore = "bootstraps a real LaunchAgent — nightly's macOS leg, or run manually"]
fn macos_install_makes_launchd_know_the_label() {
    use std::path::PathBuf;
    use std::process::Command;

    fn groove() -> PathBuf {
        PathBuf::from(env!("CARGO_BIN_EXE_groove"))
    }

    let root = TempRoot::new("groove-macos-svc-status");
    let kb = root.path().join("kb");
    std::fs::create_dir_all(&kb).expect("create kb dir");
    let config_home = root.path().join("cfg");
    let name = format!("live{}", std::process::id());
    let plist = PathBuf::from(std::env::var("HOME").expect("HOME is set on macOS"))
        .join("Library/LaunchAgents")
        .join(format!("com.groove.{name}.plist"));

    let run = |args: &[&str]| {
        Command::new(groove())
            .args(args)
            .env("GROOVE_CONFIG_HOME", &config_home)
            .output()
            .expect("groove service invocation")
    };

    let install = run(&[
        "service",
        "install",
        "--service-name",
        &name,
        "--kb-path",
        kb.to_str().unwrap(),
        "--bind",
        "127.0.0.1:0",
    ]);
    let during = run(&["service", "status", "--service-name", &name]);
    let list = run(&["service", "list"]);

    let _ = run(&["service", "uninstall", "--service-name", &name]);
    let _ = std::fs::remove_file(&plist);

    assert!(
        install.status.success(),
        "service install failed\nstderr: {}",
        String::from_utf8_lossy(&install.stderr),
    );
    assert!(plist.exists(), "install must write {}", plist.display());

    // `running` or `stopped` — either means `launchctl list <label>` found it,
    // which is what `bootstrap gui/<uid>` succeeding looks like from here.
    // Whether the daemon came up is a different question and not this one's.
    let during_text = String::from_utf8_lossy(&during.stdout);
    assert!(
        !during_text.contains("not found"),
        "while installed, launchd must know com.groove.{name}, got: {during_text}"
    );
    assert!(
        String::from_utf8_lossy(&list.stdout).contains(&name),
        "`service list` must name an installed instance, got: {}",
        String::from_utf8_lossy(&list.stdout)
    );
}

#[test]
fn uninstall_purge_without_yes_returns_abort_msg() {
    let result =
        grooveseek::service::uninstall::run(grooveseek::service::uninstall::UninstallParams {
            service_name: "test".into(),
            purge: true,
            yes: false,
        });
    let err = result.unwrap_err().to_string();
    assert!(err.contains("--yes") || err.contains("confirm"));
}
