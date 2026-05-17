#![cfg(target_os = "windows")]

use anyhow::{Context, Result};

/// Launch the user's default browser at `ui_url` (= `Config::ui_url`,
/// pre-built in config.rs so we never re-derive it from `base_url`).
///
/// `cmd /c start "" <url>` is the canonical Windows browser-launch pattern:
/// the empty `""` argument is the window title (= required because `start`
/// treats the first quoted arg as the title, not the URL).
pub fn open_web_ui(ui_url: &str) -> Result<()> {
    std::process::Command::new("cmd")
        .args(["/c", "start", "", ui_url])
        .spawn()
        .context("spawn cmd /c start")?;
    Ok(())
}
