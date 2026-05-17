//! Uninstall orchestration for kb-mcp service backends.
use crate::service::{backend, resolve_config_home, validate_service_name};
use anyhow::{Result, anyhow};

pub struct UninstallParams {
    pub service_name: String,
    pub purge: bool,
    pub yes: bool,
}

pub fn run(params: UninstallParams) -> Result<()> {
    let name = validate_service_name(&params.service_name).map_err(|e| anyhow!(e))?;

    if params.purge && !params.yes {
        return Err(anyhow!(
            "--purge will delete the index database (.kb-mcp.db) and kb-mcp.toml.\n\
             Re-installing will require a full re-index (~minutes to hours for large KBs).\n\
             This is destructive and irreversible. Re-run with --yes to confirm."
        ));
    }

    backend().uninstall(&name)?;
    eprintln!("Removed service unit for '{}'.", name);

    // (feature-44 PR-3) Best-effort tray autostart cleanup. uninstall_autostart
    // is idempotent — a missing shortcut is a no-op. Failure is logged as a
    // warning so the rest of the uninstall (config_home cleanup) still runs.
    #[cfg(target_os = "windows")]
    {
        if let Err(e) = kb_mcp_tray::install::uninstall_autostart(&name) {
            eprintln!("Warning: tray autostart cleanup failed: {e}");
        }
    }

    if params.purge {
        let home = resolve_config_home(&name)?;
        // `.kb-mcp.db` lives next to the user's KB (= `resolve_db_path(kb_path)`),
        // NOT inside config_home. Read the configured `kb_path` from the
        // install-generated toml before deleting config_home so that the
        // advertised `--purge` cleanup actually removes the index database.
        let db_path = std::fs::read_to_string(home.join("kb-mcp.toml"))
            .ok()
            .and_then(|c| toml::from_str::<toml::Value>(&c).ok())
            .and_then(|v| {
                v.get("kb_path")
                    .and_then(|p| p.as_str())
                    .map(std::path::PathBuf::from)
            })
            .map(|kb| crate::resolve_db_path(&kb));

        if let Some(db) = db_path.as_ref()
            && db.exists()
        {
            if let Err(e) = std::fs::remove_file(db) {
                eprintln!(
                    "Warning: failed to remove .kb-mcp.db at {}: {}",
                    db.display(),
                    e
                );
            } else {
                eprintln!("Removed index database: {}", db.display());
            }
        }

        if home.exists() {
            std::fs::remove_dir_all(&home)?;
            eprintln!(
                "Removed config home: {} (kb-mcp.toml + service files)",
                home.display()
            );
        }
    } else if let Ok(h) = resolve_config_home(&name)
        && h.exists()
    {
        eprintln!(
            "Kept config home: {} (use --purge --yes to remove)",
            h.display()
        );
    }
    Ok(())
}

#[cfg(target_os = "windows")]
pub fn run_tray_uninstall(service_name: &str) -> Result<()> {
    let name = validate_service_name(service_name).map_err(|e| anyhow!(e))?;
    kb_mcp_tray::install::uninstall_autostart(&name)?;
    eprintln!("Tray autostart shortcut removed for service '{}'", name);
    Ok(())
}

#[cfg(not(target_os = "windows"))]
pub fn run_tray_uninstall(_service_name: &str) -> Result<()> {
    Err(anyhow!("tray-uninstall is only supported on Windows"))
}
