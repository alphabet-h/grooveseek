#![cfg_attr(all(not(debug_assertions), windows), windows_subsystem = "windows")]

#[cfg(target_os = "windows")]
mod cli;
#[cfg(target_os = "windows")]
mod config;
#[cfg(target_os = "windows")]
mod logger;
#[cfg(target_os = "windows")]
mod tray;

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
    use tao::event::Event;
    use tao::event_loop::{ControlFlow, EventLoopBuilder};

    logger::install_panic_hook();
    logger::init_file_logger()?;
    let args = cli::parse();

    // (codex P3 round 4 on PR #61): wire up --debug to attach the parent
    // process's console if possible (= cmd.exe with `--debug`), otherwise
    // alloc a fresh console. Without this, the flag is parsed but ignored,
    // which is what GUI subsystem release builds discard stdout/stderr
    // make confusing. Plan Task 19 (PR-2) originally scheduled this; pulled
    // forward to PR-1 to avoid shipping a dead flag in the skeleton release.
    if args.debug {
        unsafe {
            #[link(name = "kernel32")]
            unsafe extern "system" {
                fn AttachConsole(dwProcessId: u32) -> i32;
                fn AllocConsole() -> i32;
            }
            const ATTACH_PARENT_PROCESS: u32 = u32::MAX;
            if AttachConsole(ATTACH_PARENT_PROCESS) == 0 {
                let _ = AllocConsole();
            }
        }
        tracing::info!("--debug: console attached");
    }

    // PR-1 skeleton: PR-1 では polling/menu なしなので config 不在でも tray
    // icon が出ることを確認するため fallback で進む = debug aid 専用。
    // Task 19 (PR-2) で fail-fast 化 (= `config::resolve(...)?` 直書き、spec
    // section 6 末尾の「kb-mcp.toml 不在 → fail-fast」と一致)。
    let cfg = config::resolve(&args.service_name, args.kb_path.as_ref()).or_else(|e| {
        tracing::warn!(
            "config resolve failed: {e}, falling back to default bind (PR-1 skeleton only)"
        );
        Ok::<_, anyhow::Error>(config::Config {
            service_name: args.service_name.clone(),
            bind: "127.0.0.1:3100".into(),
            base_url: "http://127.0.0.1:3100".into(),
            status_url: "http://127.0.0.1:3100/api/admin/status".into(),
            ui_url: "http://127.0.0.1:3100/ui".into(),
        })
    })?;
    tracing::info!("config resolved: bind={}", cfg.bind);

    let event_loop = EventLoopBuilder::<()>::with_user_event().build();
    let _tray = tray::build(&format!("kb-mcp ({})", cfg.service_name))?;
    tracing::info!("tray icon started");

    event_loop.run(move |event, _, control_flow| {
        if let Event::LoopDestroyed = event {
            tracing::info!("tray quitting");
        }
        *control_flow = ControlFlow::Wait;
    });
}
