//! groove-tray library: Windows-only API exposed to the main `groove` crate
//! for shell:startup shortcut install/uninstall.

#![cfg(target_os = "windows")]

pub mod install;
pub mod powershell;
/// Test-only. Declared by `main.rs` as well — see the module doc.
#[cfg(test)]
pub mod test_support;
