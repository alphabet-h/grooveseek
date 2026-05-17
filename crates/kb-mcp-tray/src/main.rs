#![cfg_attr(all(not(debug_assertions), windows), windows_subsystem = "windows")]

#[cfg(target_os = "windows")]
mod cli;
#[cfg(target_os = "windows")]
mod config;
#[cfg(target_os = "windows")]
mod daemon;
#[cfg(target_os = "windows")]
mod logger;
#[cfg(target_os = "windows")]
mod poll;
#[cfg(target_os = "windows")]
mod state;
#[cfg(target_os = "windows")]
mod tray;
#[cfg(target_os = "windows")]
mod ui;

/// User-defined events sent through the tao EventLoopProxy from the tokio
/// polling task (StatusUpdate) and the muda menu thread (MenuClicked /
/// Quit) into the main-thread event loop, where the tray UI is updated
/// and daemon control actions are dispatched back to the tokio runtime.
#[cfg(target_os = "windows")]
#[derive(Debug, Clone)]
pub enum UserEvent {
    StatusUpdate { dot: state::StatusDot, text: String },
    MenuClicked(String),
    Quit,
}

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
    use tray_icon::TrayIconEvent;
    use tray_icon::menu::MenuEvent;

    logger::install_panic_hook();
    logger::init_file_logger()?;
    let args = cli::parse();

    // (codex P3 round 4 on PR #61): wire up --debug to attach the parent
    // process's console if possible (= cmd.exe with `--debug`), otherwise
    // alloc a fresh console. Without this, the flag is parsed but ignored,
    // which is what GUI subsystem release builds discard stdout/stderr
    // make confusing.
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

    // PR-2: fail-fast config resolve (= spec section 6 末尾、PR-1 fallback drop).
    let cfg = config::resolve(&args.service_name, args.kb_path.as_ref())?;
    tracing::info!(
        "config resolved: service={} bind={}",
        cfg.service_name,
        cfg.bind
    );

    // tao EventLoop occupies the main thread (= tray-icon罠 1)。
    let event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build();
    let proxy = event_loop.create_proxy();

    // tokio runtime on a dedicated thread (= tray-icon罠 3 dual event loop pattern).
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;
    // The `_poll_handle` binding MUST be retained — `let _ = ...` would drop
    // the JoinHandle immediately. The leading underscore silences the unused
    // warning while preserving the binding scope.
    let _poll_handle = runtime.spawn(poll::run(cfg.status_url.clone(), proxy.clone()));

    // Tray must be constructed on the main thread (= tray-icon罠 2).
    let tray = tray::build(&format!("kb-mcp ({})", cfg.service_name))?;
    tracing::info!("tray started");

    // muda emits MenuEvent on its own thread via a crossbeam channel — we
    // bridge those into UserEvent::MenuClicked so the main event loop is
    // the single dispatcher.
    let menu_channel = MenuEvent::receiver().clone();
    let menu_proxy = proxy.clone();
    std::thread::spawn(move || {
        while let Ok(ev) = menu_channel.recv() {
            let _ = menu_proxy.send_event(UserEvent::MenuClicked(ev.id.0));
        }
    });
    // tray-icon also exposes TrayIconEvent (left/right click on the icon
    // itself). MVP ignores these — context menu is reached via right-click
    // and dispatched as MenuEvent above. Capture so the receiver isn't dropped.
    let _tray_channel = TrayIconEvent::receiver();

    event_loop.run(move |event, _, control_flow| {
        // (codex P2 round 1 on PR #62): tao 0.35 defaults to Poll which
        // spins the GUI thread continuously when there are no OS events.
        // Always-running tray app must use Wait to idle until something
        // actually happens (UserEvent or Win32 message).
        *control_flow = ControlFlow::Wait;
        match event {
            Event::UserEvent(UserEvent::StatusUpdate { dot, text }) => {
                if let Err(e) = tray::apply_dot(&tray, dot, &text) {
                    tracing::warn!("apply_dot failed: {e}");
                }
            }
            Event::UserEvent(UserEvent::MenuClicked(id)) => {
                handle_menu(&id, &runtime, &cfg, &proxy);
            }
            Event::UserEvent(UserEvent::Quit) => {
                tracing::info!("quit requested");
                *control_flow = ControlFlow::Exit;
            }
            _ => {}
        }
    });
}

/// Dispatch a menu click. Daemon control actions are spawned on the tokio
/// runtime (= main thread is NOT blocked, spec section 6 review round 2 M-1).
/// `&Config` cannot be captured by `async move`, so we clone the fields we
/// need (= `service_name`) before moving into the future.
#[cfg(target_os = "windows")]
fn handle_menu(
    id: &str,
    runtime: &tokio::runtime::Runtime,
    cfg: &config::Config,
    proxy: &tao::event_loop::EventLoopProxy<UserEvent>,
) {
    let service = cfg.service_name.clone();
    match id {
        "start" => {
            runtime.spawn(async move {
                if let Err(e) = daemon::start(&service).await {
                    tracing::warn!("daemon start failed: {e}");
                }
            });
        }
        "stop" => {
            runtime.spawn(async move {
                if let Err(e) = daemon::stop(&service).await {
                    tracing::warn!("daemon stop failed: {e}");
                }
            });
        }
        "restart" => {
            runtime.spawn(async move {
                if let Err(e) = daemon::restart(&service).await {
                    tracing::warn!("daemon restart failed: {e}");
                }
            });
        }
        "open" => {
            if let Err(e) = ui::open_web_ui(&cfg.ui_url) {
                tracing::warn!("open_web_ui failed: {e}");
            }
        }
        "quit" => {
            let _ = proxy.send_event(UserEvent::Quit);
        }
        other => {
            tracing::debug!("unknown menu id: {other}");
        }
    }
}
