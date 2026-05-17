//! kb-mcp-tray library: Windows-only API exposed to the main `kb-mcp` crate
//! for shell:startup shortcut install/uninstall.

#![cfg(target_os = "windows")]

pub mod install;
