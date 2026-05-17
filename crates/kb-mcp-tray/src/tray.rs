#![cfg(target_os = "windows")]

use anyhow::{Context, Result};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};

const ICON_GRAY: &[u8] = include_bytes!("../assets/status-gray-16.png");

/// PR-1 skeleton: build a tray icon (gray) with a tooltip. No menu, no event
/// handling. PR-2 will extend this with menu items, icon swap, and event
/// dispatch.
pub fn build(tooltip: &str) -> Result<TrayIcon> {
    let icon = load_icon(ICON_GRAY).context("load gray icon")?;
    let tray = TrayIconBuilder::new()
        .with_tooltip(tooltip)
        .with_icon(icon)
        .build()
        .context("build tray icon")?;
    Ok(tray)
}

fn load_icon(png_bytes: &[u8]) -> Result<Icon> {
    let img = image::load_from_memory(png_bytes)?.into_rgba8();
    let (w, h) = img.dimensions();
    Icon::from_rgba(img.into_raw(), w, h).context("Icon::from_rgba")
}
