//! Status / list for kb-mcp service backends.
//! Phase 1: OS native + toml fallback. HTTP enrichment (`uptime_secs`, `model`) is deferred to Phase 2.
use crate::service::{ServiceState, backend, resolve_config_home, validate_service_name};
use anyhow::{Result, anyhow};
use std::path::PathBuf;

pub fn run_status(service_name: &str) -> Result<String> {
    let name = validate_service_name(service_name).map_err(|e| anyhow!(e))?;
    let state = backend().status(&name)?;
    let state = enrich_with_toml(&name, state);
    Ok(format_state(&name, &state))
}

pub fn run_list() -> Result<String> {
    let entries = backend().list()?;
    let mut s =
        String::from("NAME       STATUS     BIND               KB_PATH                UPTIME\n");
    for (name, state) in entries {
        let state = enrich_with_toml(&name, state);
        s.push_str(&format_row(&name, &state));
        s.push('\n');
    }
    Ok(s)
}

fn enrich_with_toml(name: &str, state: ServiceState) -> ServiceState {
    let toml_path = resolve_config_home(name)
        .ok()
        .map(|h| h.join("kb-mcp.toml"));
    let (bind_toml, kb_toml) = toml_path
        .and_then(|p| std::fs::read_to_string(&p).ok())
        .and_then(|c| toml::from_str::<toml::Value>(&c).ok())
        .map(|v| {
            let bind = v
                .get("transport")
                .and_then(|t| t.get("http"))
                .and_then(|h| h.get("bind"))
                .and_then(|b| b.as_str())
                .map(String::from);
            let kb = v.get("kb_path").and_then(|p| p.as_str()).map(PathBuf::from);
            (bind, kb)
        })
        .unwrap_or((None, None));
    match state {
        ServiceState::Running {
            uptime_secs,
            model,
            bind,
            kb_path,
        } => ServiceState::Running {
            uptime_secs,
            bind: bind.or(bind_toml),
            kb_path: kb_path.or(kb_toml),
            model,
        },
        ServiceState::Stopped { bind, kb_path } => ServiceState::Stopped {
            bind: bind.or(bind_toml),
            kb_path: kb_path.or(kb_toml),
        },
        s => s,
    }
}

fn format_state(name: &str, state: &ServiceState) -> String {
    match state {
        ServiceState::Running {
            uptime_secs,
            bind,
            kb_path,
            model,
        } => format!(
            "{}: running (uptime {}s, bind {}, kb_path {}, model {})",
            name,
            uptime_secs,
            bind.as_deref().unwrap_or("(unknown)"),
            kb_path
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "(unknown)".into()),
            model.as_deref().unwrap_or("(unknown)"),
        ),
        ServiceState::Stopped { bind, kb_path } => format!(
            "{}: stopped (bind {}, kb_path {})",
            name,
            bind.as_deref().unwrap_or("(unknown)"),
            kb_path
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "(unknown)".into()),
        ),
        ServiceState::NotFound => format!("{}: not found", name),
    }
}

fn format_row(name: &str, state: &ServiceState) -> String {
    match state {
        ServiceState::Running {
            uptime_secs,
            bind,
            kb_path,
            ..
        } => format!(
            "{:<10} running    {:<18} {:<22} {}s",
            name,
            bind.as_deref().unwrap_or("(unknown)"),
            kb_path
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "(unknown)".into()),
            uptime_secs,
        ),
        ServiceState::Stopped { bind, kb_path } => format!(
            "{:<10} stopped    {:<18} {:<22} -",
            name,
            bind.as_deref().unwrap_or("(unknown)"),
            kb_path
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "(unknown)".into()),
        ),
        ServiceState::NotFound => format!("{:<10} not-found", name),
    }
}
