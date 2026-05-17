#![cfg(target_os = "windows")]

use anyhow::{Context, Result};
use std::time::Duration;

/// `kb-mcp-<service>` is the Task Scheduler task name registered by
/// `kb-mcp service install` (= feature-43 `kb-mcp/src/service/windows.rs`
/// `task_name` helper, line 31-32). PowerShell `Start-ScheduledTask
/// -TaskName <name>` accepts the bare name without a TaskPath prefix
/// (= codex P2 round 1 on PR #62: prefixing with `\` makes the cmdlet
/// search for a path that doesn't exist and daemon control fails).
pub fn task_name(service_name: &str) -> String {
    format!("kb-mcp-{}", service_name)
}

/// PowerShell single-quoted literal escape (= each `'` becomes `''`).
/// Reused from feature-43 (windows.rs) to keep task names with apostrophes
/// safe (= e.g. service names that contain `'` somehow, or unicode that
/// PowerShell would otherwise misinterpret).
pub fn escape_single_quote(s: &str) -> String {
    s.replace('\'', "''")
}

/// Async daemon start via PowerShell `Start-ScheduledTask`. Non-blocking
/// thanks to `tokio::process::Command`, so the event loop is not stalled
/// while PowerShell spins up.
pub async fn start(service_name: &str) -> Result<()> {
    let task = escape_single_quote(&task_name(service_name));
    run_powershell(&format!("Start-ScheduledTask -TaskName '{}'", task)).await
}

pub async fn stop(service_name: &str) -> Result<()> {
    let task = escape_single_quote(&task_name(service_name));
    run_powershell(&format!("Stop-ScheduledTask -TaskName '{}'", task)).await
}

/// Stop then start, with an 800ms grace period for the daemon process to
/// fully exit before relaunching. Polling loop will pick up the recovery
/// within the next ~5 seconds.
pub async fn restart(service_name: &str) -> Result<()> {
    stop(service_name).await?;
    tokio::time::sleep(Duration::from_millis(800)).await;
    start(service_name).await
}

async fn run_powershell(script: &str) -> Result<()> {
    let out = tokio::process::Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-Command", script])
        .output()
        .await
        .context("spawn powershell")?;
    if !out.status.success() {
        anyhow::bail!(
            "powershell failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_name_uses_kb_mcp_prefix() {
        assert_eq!(task_name("kb-mcp"), "kb-mcp-kb-mcp");
        assert_eq!(task_name("work"), "kb-mcp-work");
        assert_eq!(task_name("a-b"), "kb-mcp-a-b");
    }

    #[test]
    fn escape_doubles_each_apostrophe() {
        assert_eq!(escape_single_quote("O'Brien"), "O''Brien");
        assert_eq!(escape_single_quote("plain"), "plain");
        assert_eq!(escape_single_quote("a'b'c"), "a''b''c");
        assert_eq!(escape_single_quote("''"), "''''");
    }
}
