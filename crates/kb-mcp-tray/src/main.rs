#![cfg_attr(all(not(debug_assertions), windows), windows_subsystem = "windows")]

#[cfg(target_os = "windows")]
mod logger;
#[cfg(target_os = "windows")]
mod cli;

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
    logger::install_panic_hook();
    logger::init_file_logger()?;
    let args = cli::parse();
    tracing::info!("kb-mcp-tray starting for service='{}'", args.service_name);
    Ok(())
}
