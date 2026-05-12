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

    if params.purge {
        let home = resolve_config_home(&name)?;
        if home.exists() {
            std::fs::remove_dir_all(&home)?;
            eprintln!(
                "Removed config home: {} (includes kb-mcp.toml and .kb-mcp.db)",
                home.display()
            );
        }
    } else if let Ok(h) = resolve_config_home(&name) {
        if h.exists() {
            eprintln!(
                "Kept config home: {} (use --purge --yes to remove)",
                h.display()
            );
        }
    }
    Ok(())
}
