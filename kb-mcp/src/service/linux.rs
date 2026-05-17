//! Linux systemd-user backend for kb-mcp service.
//!
//! Module-level `#[cfg(target_os = "linux")]` lives on the `pub mod linux;`
//! declaration in `src/service/mod.rs`; no inner `#![cfg]` needed.

use super::{InstallContext, ServiceBackend, ServiceState};
use anyhow::{Context, Result, anyhow};
use std::path::PathBuf;
use std::process::Command;

pub(crate) struct SystemdUser;

pub fn render_unit(ctx: &InstallContext) -> String {
    format!(
        "[Unit]\n\
         Description=kb-mcp loopback HTTP MCP server ({name})\n\
         After=default.target\n\
         \n\
         [Service]\n\
         Type=simple\n\
         WorkingDirectory={home}\n\
         ExecStart={bin} serve\n\
         Restart=on-failure\n\
         RestartSec=5s\n\
         Environment=RUST_LOG=info\n\
         StandardOutput=journal\n\
         StandardError=journal\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n",
        name = ctx.service_name,
        home = ctx.config_home.display(),
        bin = ctx.binary_path.display(),
    )
}

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
        std::fs::write(&path, render_unit(ctx))?;
        run_systemctl(&["daemon-reload"])?;
        if ctx.auto_start {
            let name = format!("kb-mcp-{}.service", ctx.service_name);
            run_systemctl(&["enable", &name])?;
            run_systemctl(&["start", &name])?;
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
