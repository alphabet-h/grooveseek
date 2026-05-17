#![cfg(target_os = "windows")]

use std::sync::OnceLock;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::EnvFilter;

static LOG_GUARD: OnceLock<WorkerGuard> = OnceLock::new();

/// Install a panic hook that logs `info` to `tracing::error!` before delegating
/// to the default hook. This is required for GUI subsystem binaries — without
/// it, panics in `--release` builds are silent (no stdout/stderr attached).
pub fn install_panic_hook() {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        tracing::error!("PANIC: {info}");
        if let Some(loc) = info.location() {
            tracing::error!("  at {}:{}:{}", loc.file(), loc.line(), loc.column());
        }
        default_hook(info);
    }));
}

/// Initialize a daily-rotating file logger at
/// `%LOCALAPPDATA%\kb-mcp\logs\tray.YYYY-MM-DD`. Override the log level with
/// the `KB_MCP_TRAY_LOG` env var (e.g. `KB_MCP_TRAY_LOG=debug`).
///
/// The non-blocking writer requires a `WorkerGuard` to stay alive for the
/// lifetime of the process; we stash it in a `OnceLock` so it lives until
/// the binary exits.
pub fn init_file_logger() -> anyhow::Result<()> {
    let log_dir = dirs::data_local_dir()
        .ok_or_else(|| anyhow::anyhow!("LOCALAPPDATA not found"))?
        .join("kb-mcp")
        .join("logs");
    std::fs::create_dir_all(&log_dir)?;

    let appender = tracing_appender::rolling::daily(&log_dir, "tray");
    let (writer, guard) = tracing_appender::non_blocking(appender);
    LOG_GUARD
        .set(guard)
        .map_err(|_| anyhow::anyhow!("logger already initialized"))?;

    let filter =
        EnvFilter::try_from_env("KB_MCP_TRAY_LOG").unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_writer(writer)
        .with_ansi(false)
        .with_env_filter(filter)
        .init();
    Ok(())
}
