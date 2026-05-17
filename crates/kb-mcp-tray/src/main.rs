#![cfg_attr(all(not(debug_assertions), windows), windows_subsystem = "windows")]

#[cfg(not(target_os = "windows"))]
fn main() {
    eprintln!(
        "kb-mcp-tray is Windows-only. \
         On Linux/macOS use the `kb-mcp tray` subcommand (planned for Phase 2.5+)."
    );
    std::process::exit(1);
}

#[cfg(target_os = "windows")]
fn main() -> anyhow::Result<()> {
    println!("kb-mcp-tray skeleton: not yet implemented");
    Ok(())
}
