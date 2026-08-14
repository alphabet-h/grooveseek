//! Linux systemd-user backend for kb-mcp service.
//!
//! Module-level `#[cfg(target_os = "linux")]` lives on the `pub mod linux;`
//! declaration in `src/service/mod.rs`; no inner `#![cfg]` needed.

use super::{InstallContext, ServiceBackend, ServiceState};
use anyhow::{Context, Result, anyhow};
use std::path::PathBuf;
use std::process::Command;

pub(crate) struct SystemdUser;

/// テンプレート本体は `service::render` にある (全 OS で compile + テストする
/// ため)。ここは既存の呼び出し経路 `service::linux::render_unit` を保つための
/// re-export。
pub use super::render::render_unit;

fn unit_path(service_name: &str) -> Result<PathBuf> {
    let dir = dirs::config_dir()
        .ok_or_else(|| anyhow!("XDG_CONFIG_HOME / HOME 解決失敗"))?
        .join("systemd/user");
    Ok(dir.join(format!("kb-mcp-{}.service", service_name)))
}

fn run_systemctl(args: &[&str]) -> Result<()> {
    let mut cmd = Command::new("systemctl");
    cmd.arg("--user").args(args);
    let status = cmd
        .status()
        .with_context(|| format!("systemctl --user {} の実行失敗", args.join(" ")))?;
    if !status.success() {
        return Err(anyhow!(
            "systemctl --user {} が失敗 (status: {})",
            args.join(" "),
            status
        ));
    }
    Ok(())
}

impl ServiceBackend for SystemdUser {
    fn install(&self, ctx: &InstallContext) -> Result<()> {
        let path = unit_path(&ctx.service_name)?;
        std::fs::create_dir_all(path.parent().unwrap())?;
        if path.exists() && !ctx.force {
            return Err(anyhow!(
                "service unit が既存: {} (--force で上書き)",
                path.display()
            ));
        }
        std::fs::write(&path, render_unit(ctx)?)?;
        run_systemctl(&["daemon-reload"])?;
        let name = format!("kb-mcp-{}.service", ctx.service_name);
        if ctx.auto_start {
            run_systemctl(&["enable", &name])?;
            // `restart`, not `start`: on a fresh install the two are the same,
            // but over an already-running unit `start` is a no-op and the daemon
            // keeps running with the previous `ExecStart`. That matters now that
            // the launch line carries `--config` — `install --force` has to be
            // the way an existing service picks it up (same reason as the
            // `bootout` in the macOS backend, codex P2 round 2 on PR #156).
            run_systemctl(&["restart", &name])?;
        } else {
            // (codex P2 round 3 on PR #156) `--no-auto-start` must not start
            // anything — but a unit someone started by hand is still holding the
            // previous `ExecStart`, and `restart` here would *start* a service
            // the user asked not to run. `try-restart` is exactly the missing
            // verb; systemctl(1): "Stop and then start one or more units ... if
            // the units are running. This does nothing if units are not
            // running." So the ordinary case already succeeds, and the error is
            // propagated (round 4): swallowing it would report a successful
            // install while the old daemon kept serving without `--config`.
            run_systemctl(&["try-restart", &name])?;
        }
        eprintln!(
            "Note: run 'sudo loginctl enable-linger $USER' to keep the service running after logout."
        );
        Ok(())
    }
    fn uninstall(&self, service_name: &str) -> Result<()> {
        let unit_name = format!("kb-mcp-{}.service", service_name);
        let _ = run_systemctl(&["stop", &unit_name]);
        let _ = run_systemctl(&["disable", &unit_name]);
        let path = unit_path(service_name)?;
        if path.exists() {
            std::fs::remove_file(&path)?;
        }
        let _ = run_systemctl(&["daemon-reload"]);
        Ok(())
    }
    fn status(&self, service_name: &str) -> Result<ServiceState> {
        let unit_name = format!("kb-mcp-{}.service", service_name);
        let out = Command::new("systemctl")
            .args(["--user", "is-active", &unit_name])
            .output()
            .context("systemctl --user is-active 実行失敗")?;
        let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
        Ok(match stdout.as_str() {
            "active" => ServiceState::Running {
                uptime_secs: 0,
                bind: None,
                kb_path: None,
                model: None,
            },
            "inactive" | "failed" => ServiceState::Stopped {
                bind: None,
                kb_path: None,
            },
            _ => ServiceState::NotFound,
        })
    }
    fn list(&self) -> Result<Vec<(String, ServiceState)>> {
        let dir = dirs::config_dir()
            .ok_or_else(|| anyhow!("config dir 解決失敗"))?
            .join("systemd/user");
        let mut out = Vec::new();
        if !dir.exists() {
            return Ok(out);
        }
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().to_string();
            if let Some(rest) = name
                .strip_prefix("kb-mcp-")
                .and_then(|s| s.strip_suffix(".service"))
            {
                let state = self.status(rest)?;
                out.push((rest.to_string(), state));
            }
        }
        Ok(out)
    }
    fn stop(&self, service_name: &str) -> Result<()> {
        run_systemctl(&["stop", &format!("kb-mcp-{}.service", service_name)])
    }
}
