#![cfg(target_os = "windows")]

use crate::state::StatusDot;
use anyhow::{Context, Result};
use tray_icon::{
    Icon, TrayIcon, TrayIconBuilder,
    menu::{Menu, MenuId, MenuItem, PredefinedMenuItem},
};

const ICON_GRAY: &[u8] = include_bytes!("../assets/status-gray-16.png");
const ICON_GREEN: &[u8] = include_bytes!("../assets/status-green-16.png");
const ICON_YELLOW: &[u8] = include_bytes!("../assets/status-yellow-16.png");
const ICON_RED: &[u8] = include_bytes!("../assets/status-red-16.png");

/// Wrapper around the muda tray + menu items. We hold MenuItem refs so the
/// event loop can update the Status text and enable/disable Start/Stop/
/// Restart per dot state.
pub struct Tray {
    pub _inner: TrayIcon,
    pub status_item: MenuItem,
    pub start_item: MenuItem,
    pub stop_item: MenuItem,
    pub restart_item: MenuItem,
}

/// Build the tray icon with the 6 actionable menu items + 3 separators.
/// Initial state: gray icon, Start enabled, Stop/Restart disabled
/// (= Status::Gray => awaiting first poll).
pub fn build(tooltip: &str) -> Result<Tray> {
    let menu = Menu::new();
    let status_item =
        MenuItem::with_id(MenuId::new("status"), "Status: Connecting...", false, None);
    let open_item = MenuItem::with_id(MenuId::new("open"), "Open Web UI", true, None);
    let start_item = MenuItem::with_id(MenuId::new("start"), "Start", true, None);
    let stop_item = MenuItem::with_id(MenuId::new("stop"), "Stop", false, None);
    let restart_item = MenuItem::with_id(MenuId::new("restart"), "Restart", false, None);
    let quit_item = MenuItem::with_id(MenuId::new("quit"), "Quit Tray", true, None);

    menu.append_items(&[
        &status_item,
        &PredefinedMenuItem::separator(),
        &open_item,
        &PredefinedMenuItem::separator(),
        &start_item,
        &stop_item,
        &restart_item,
        &PredefinedMenuItem::separator(),
        &quit_item,
    ])
    .context("append menu items")?;

    let inner = TrayIconBuilder::new()
        .with_tooltip(tooltip)
        .with_icon(load_icon(ICON_GRAY)?)
        .with_menu(Box::new(menu))
        .build()
        .context("build tray icon")?;

    Ok(Tray {
        _inner: inner,
        status_item,
        start_item,
        stop_item,
        restart_item,
    })
}

/// Map a StatusDot to the embedded PNG icon bytes.
pub fn icon_for(dot: StatusDot) -> Result<Icon> {
    match dot {
        StatusDot::Gray => load_icon(ICON_GRAY),
        StatusDot::Green => load_icon(ICON_GREEN),
        StatusDot::Yellow => load_icon(ICON_YELLOW),
        StatusDot::Red => load_icon(ICON_RED),
    }
}

/// Update the tray icon + status menu label + enable/disable Start/Stop/
/// Restart per dot state.
pub fn apply_dot(tray: &Tray, dot: StatusDot, status_text: &str) -> Result<()> {
    tray._inner.set_icon(Some(icon_for(dot)?))?;
    tray.status_item.set_text(status_text);
    match dot {
        StatusDot::Red | StatusDot::Gray => {
            tray.start_item.set_enabled(true);
            tray.stop_item.set_enabled(false);
            tray.restart_item.set_enabled(false);
        }
        StatusDot::Green | StatusDot::Yellow => {
            tray.start_item.set_enabled(false);
            tray.stop_item.set_enabled(true);
            tray.restart_item.set_enabled(true);
        }
    }
    Ok(())
}

fn load_icon(png_bytes: &[u8]) -> Result<Icon> {
    let img = image::load_from_memory(png_bytes)?.into_rgba8();
    let (w, h) = img.dimensions();
    Icon::from_rgba(img.into_raw(), w, h).context("Icon::from_rgba")
}
