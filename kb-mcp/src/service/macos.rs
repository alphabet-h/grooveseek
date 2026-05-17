//! macOS LaunchAgent backend for kb-mcp service.
//!
//! Module-level `#[cfg(target_os = "macos")]` lives on the `pub mod macos;`
//! declaration in `src/service/mod.rs`; no inner `#![cfg]` needed.

use super::{InstallContext, ServiceBackend, ServiceState};
use anyhow::{Context, Result, anyhow};
use std::path::PathBuf;
use std::process::Command;

pub(crate) struct LaunchAgent;

pub fn render_plist(ctx: &InstallContext) -> String {
    // codex P2 round 5 on PR #56: honor `--no-auto-start` by emitting
    // `<false/>` for `RunAtLoad` and `KeepAlive` when auto_start is false.
    // Otherwise launchd would still start (and keep alive) the agent at the
    // next login as soon as it's loaded — `--no-auto-start` becomes a no-op
    // for the LaunchAgent backend.
    let bool_val = if ctx.auto_start {
        "<true/>"
    } else {
        "<false/>"
    };
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>com.kb-mcp.{name}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{bin}</string>
        <string>serve</string>
    </array>
    <key>WorkingDirectory</key>
    <string>{home}</string>
    <key>RunAtLoad</key>
    {bool_val}
    <key>KeepAlive</key>
    {bool_val}
    <key>EnvironmentVariables</key>
    <dict>
        <key>RUST_LOG</key>
        <string>info</string>
    </dict>
    <key>StandardOutPath</key>
    <string>{home}/kb-mcp.out</string>
    <key>StandardErrorPath</key>
    <string>{home}/kb-mcp.err</string>
</dict>
</plist>
"#,
        name = ctx.service_name,
        bin = ctx.binary_path.display(),
        home = ctx.config_home.display(),
    )
}

fn plist_path(service_name: &str) -> Result<PathBuf> {
    let home = dirs::home_dir().ok_or_else(|| anyhow!("HOME 解決失敗"))?;
    Ok(home.join(format!(
        "Library/LaunchAgents/com.kb-mcp.{}.plist",
        service_name
    )))
}

fn current_uid() -> Result<String> {
    let out = Command::new("id")
        .arg("-u")
        .output()
        .context("id -u 実行失敗")?;
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn run_launchctl(args: &[&str]) -> Result<()> {
    let status = Command::new("launchctl")
        .args(args)
        .status()
        .with_context(|| format!("launchctl {} 実行失敗", args.join(" ")))?;
    if !status.success() {
        return Err(anyhow!(
            "launchctl {} が失敗 (status: {})",
            args.join(" "),
            status
        ));
    }
    Ok(())
}

impl ServiceBackend for LaunchAgent {
    fn install(&self, ctx: &InstallContext) -> Result<()> {
        let path = plist_path(&ctx.service_name)?;
        std::fs::create_dir_all(path.parent().unwrap())?;
        if path.exists() && !ctx.force {
            return Err(anyhow!(
                "plist が既存: {} (--force で上書き)",
                path.display()
            ));
        }
        std::fs::write(&path, render_plist(ctx))?;
        if ctx.auto_start {
            let uid = current_uid()?;
            run_launchctl(&["bootstrap", &format!("gui/{}", uid), path.to_str().unwrap()])?;
        }
        Ok(())
    }
    fn uninstall(&self, service_name: &str) -> Result<()> {
        let path = plist_path(service_name)?;
        if path.exists() {
            let uid = current_uid().unwrap_or_default();
            let _ = run_launchctl(&[
                "bootout",
                &format!("gui/{}/com.kb-mcp.{}", uid, service_name),
            ]);
            std::fs::remove_file(&path)?;
        }
        Ok(())
    }
    fn status(&self, service_name: &str) -> Result<ServiceState> {
        let out = Command::new("launchctl")
            .args(["list", &format!("com.kb-mcp.{}", service_name)])
            .output()
            .context("launchctl list 実行失敗")?;
        if !out.status.success() {
            // exit 1 from launchctl list <label> = label not loaded
            return Ok(ServiceState::NotFound);
        }
        // codex P2 round 4 on PR #56: `launchctl list <label>` exits 0 for
        // loaded-but-not-running agents (= LastExitStatus != 0, no PID).
        // Distinguish Running vs Stopped by checking for a numeric PID line
        // in the plist-style output (`"PID" = NNN;`).
        let stdout = String::from_utf8_lossy(&out.stdout);
        let has_pid = stdout
            .lines()
            .map(str::trim)
            .any(|l| l.starts_with("\"PID\" =") || l.starts_with("PID ="));
        Ok(if has_pid {
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
        })
    }
    fn list(&self) -> Result<Vec<(String, ServiceState)>> {
        let dir = dirs::home_dir()
            .ok_or_else(|| anyhow!("HOME 解決失敗"))?
            .join("Library/LaunchAgents");
        let mut out = Vec::new();
        if !dir.exists() {
            return Ok(out);
        }
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().to_string();
            if let Some(rest) = name
                .strip_prefix("com.kb-mcp.")
                .and_then(|s| s.strip_suffix(".plist"))
            {
                let state = self.status(rest)?;
                out.push((rest.to_string(), state));
            }
        }
        Ok(out)
    }
    fn stop(&self, service_name: &str) -> Result<()> {
        let uid = current_uid()?;
        run_launchctl(&[
            "bootout",
            &format!("gui/{}/com.kb-mcp.{}", uid, service_name),
        ])
    }
}
