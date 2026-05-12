//! Windows Task Scheduler backend for kb-mcp service.
//!
//! Module-level `#[cfg(target_os = "windows")]` lives on the `pub mod windows;`
//! declaration in `src/service/mod.rs`; no inner `#![cfg]` needed.

use super::{InstallContext, ServiceBackend, ServiceState};
use anyhow::{Context, Result, anyhow};
use std::process::Command;

pub(crate) struct TaskScheduler;

pub fn render_task_xml(ctx: &InstallContext) -> String {
    // (v0.8.1 hot-fix) The XML declares `encoding="UTF-16"` AND the file is
    // written as UTF-16 LE with a BOM by `write_task_xml_utf16` below. Some
    // Japanese-locale Windows builds reject the `encoding="UTF-8"` variant
    // with "エンコードを切り替えることができません" (= "cannot switch
    // encoding") even though Microsoft's docs say schtasks /XML accepts
    // both — empirically UTF-16 LE BOM is the broadest-compatible form, so
    // we always emit that.
    //
    // codex P2 round 5 on PR #56: honor `--no-auto-start` by emitting
    // `<Enabled>false</Enabled>` for the LogonTrigger. Skipping `schtasks /Run`
    // alone leaves the task armed for the next logon — `--no-auto-start`
    // would otherwise be a one-shot suppression, not a backend setting.
    let trigger_enabled = if ctx.auto_start { "true" } else { "false" };
    format!(
        r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.4" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo>
    <Description>kb-mcp loopback HTTP MCP server ({name})</Description>
    <URI>\kb-mcp-{name}</URI>
  </RegistrationInfo>
  <Triggers>
    <LogonTrigger><Enabled>{trigger_enabled}</Enabled></LogonTrigger>
  </Triggers>
  <Principals>
    <Principal id="Author">
      <LogonType>InteractiveToken</LogonType>
      <RunLevel>LeastPrivilege</RunLevel>
    </Principal>
  </Principals>
  <Settings>
    <RestartOnFailure>
      <Interval>PT1M</Interval>
      <Count>3</Count>
    </RestartOnFailure>
    <Priority>7</Priority>
  </Settings>
  <Actions Context="Author">
    <Exec>
      <Command>{bin}</Command>
      <Arguments>serve</Arguments>
      <WorkingDirectory>{home}</WorkingDirectory>
    </Exec>
  </Actions>
</Task>
"#,
        name = ctx.service_name,
        bin = ctx.binary_path.display(),
        home = ctx.config_home.display(),
    )
}

/// (v0.8.1 hot-fix) Encode `xml` as UTF-16 LE bytes with a `0xFF 0xFE` BOM
/// and write to `path`. Required because `schtasks /Create /XML` on
/// Japanese-locale Windows rejects UTF-8 XML (= "エンコードを切り替える
/// ことができません") even when the declaration says `encoding="UTF-8"`.
/// UTF-16 LE BOM is the broadest-compatible form across Windows locales.
pub fn encode_utf16_le_bom(xml: &str) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(2 + xml.len() * 2);
    bytes.extend_from_slice(&[0xFF, 0xFE]); // UTF-16 LE BOM
    for unit in xml.encode_utf16() {
        bytes.extend_from_slice(&unit.to_le_bytes());
    }
    bytes
}

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

impl ServiceBackend for TaskScheduler {
    fn install(&self, ctx: &InstallContext) -> Result<()> {
        let xml = render_task_xml(ctx);
        let tmp = std::env::temp_dir().join(format!("kb-mcp-task-{}.xml", ctx.service_name));
        // v0.8.1 hot-fix: write UTF-16 LE BOM bytes (= broadest-compatible
        // schtasks XML format). See `encode_utf16_le_bom` doc-comment.
        std::fs::write(&tmp, encode_utf16_le_bom(&xml))?;
        let task = task_name(&ctx.service_name);
        let force_flag = if ctx.force { vec!["/F"] } else { vec![] };
        let mut args: Vec<&str> = vec!["/Create", "/TN", &task, "/XML"];
        args.push(tmp.to_str().unwrap());
        args.extend(force_flag);
        run_schtasks(&args)?;
        if ctx.auto_start {
            run_schtasks(&["/Run", "/TN", &task])?;
        }
        let _ = std::fs::remove_file(&tmp);
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
        let out = Command::new("schtasks")
            .args(["/Query", "/TN", &task, "/FO", "CSV", "/NH"])
            .output()
            .context("schtasks /Query 実行失敗")?;
        if !out.status.success() {
            return Ok(ServiceState::NotFound);
        }
        let stdout = String::from_utf8_lossy(&out.stdout);
        Ok(if stdout.contains("Running") {
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
        let out = Command::new("schtasks")
            .args(["/Query", "/FO", "CSV", "/NH"])
            .output()
            .context("schtasks /Query 全体 実行失敗")?;
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
