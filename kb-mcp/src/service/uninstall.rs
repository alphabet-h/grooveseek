//! Uninstall orchestration for kb-mcp service backends.
use crate::service::{ServiceBackend, backend, resolve_config_home, validate_service_name};
use anyhow::{Result, anyhow};
use std::path::Path;

pub struct UninstallParams {
    pub service_name: String,
    pub purge: bool,
    pub yes: bool,
}

pub fn run(params: UninstallParams) -> Result<()> {
    let name = validate_service_name(&params.service_name).map_err(|e| anyhow!(e))?;
    let config_home = resolve_config_home(&name)?;
    run_with_backend(backend().as_ref(), &config_home, params)
}

/// `run` with its two ambient dependencies passed in (AU-28).
///
/// Both are needed, not just the backend. `--purge --yes` deletes the config
/// home and the index database it names, so a test that supplied only a fake
/// backend would delete the *developer's* real ones. The other route — pointing
/// `KB_MCP_CONFIG_HOME` somewhere safe — mutates process-wide state, and
/// `cargo test` runs tests on parallel threads of one process, which is the
/// unsoundness AU-63 was about.
pub(crate) fn run_with_backend(
    be: &dyn ServiceBackend,
    config_home: &Path,
    params: UninstallParams,
) -> Result<()> {
    let name = validate_service_name(&params.service_name).map_err(|e| anyhow!(e))?;

    if params.purge && !params.yes {
        return Err(anyhow!(
            "--purge will delete the index database (.kb-mcp.db) and kb-mcp.toml.\n\
             Re-installing will require a full re-index (~minutes to hours for large KBs).\n\
             This is destructive and irreversible. Re-run with --yes to confirm."
        ));
    }

    be.uninstall(&name)?;
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
        let home = config_home.to_path_buf();
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
    } else if config_home.exists() {
        eprintln!(
            "Kept config home: {} (use --purge --yes to remove)",
            config_home.display()
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::service::{InstallContext, ServiceState};
    use std::cell::Cell;
    use std::path::PathBuf;

    /// A backend that records what it was asked to do and can be told to fail.
    ///
    /// `Cell` rather than a mutex because each test builds its own and never
    /// shares it across threads; the trait takes `&self`, so interior
    /// mutability is what lets the calls be observed at all.
    struct FakeBackend {
        fail_uninstall: bool,
        uninstalled: Cell<Option<String>>,
    }

    impl FakeBackend {
        fn ok() -> Self {
            Self {
                fail_uninstall: false,
                uninstalled: Cell::new(None),
            }
        }
        fn failing() -> Self {
            Self {
                fail_uninstall: true,
                uninstalled: Cell::new(None),
            }
        }
    }

    impl ServiceBackend for FakeBackend {
        fn install(&self, _ctx: &InstallContext) -> Result<()> {
            Ok(())
        }
        fn uninstall(&self, service_name: &str) -> Result<()> {
            self.uninstalled.set(Some(service_name.to_string()));
            if self.fail_uninstall {
                return Err(anyhow!("backend refused to unregister"));
            }
            Ok(())
        }
        fn status(&self, _service_name: &str) -> Result<ServiceState> {
            Ok(ServiceState::NotFound)
        }
        fn list(&self) -> Result<Vec<(String, ServiceState)>> {
            Ok(Vec::new())
        }
        fn stop(&self, _service_name: &str) -> Result<()> {
            Ok(())
        }
    }

    /// Temp directory with a `Drop` guard. The project does not use the
    /// `tempfile` crate; PID + nanos alone collide because a test binary runs
    /// its tests on parallel threads of one process (AU-54), hence the counter.
    struct TempHome(PathBuf);

    impl TempHome {
        fn new() -> Self {
            static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            let p = std::env::temp_dir()
                .join(format!("kb-mcp-au28-{}-{nanos}-{seq}", std::process::id()));
            std::fs::create_dir_all(&p).unwrap();
            Self(p)
        }
        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempHome {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn params(purge: bool, yes: bool) -> UninstallParams {
        UninstallParams {
            service_name: "au28".into(),
            purge,
            yes,
        }
    }

    /// The destructive path, end to end: `--purge --yes` must remove both the
    /// config home and the index database that `kb-mcp.toml` points at.
    ///
    /// Before AU-28 this could not be tested at all — `run` reached for the
    /// real backend and the real config home, so running it would have
    /// unregistered a service and deleted the developer's own files.
    #[test]
    fn purge_removes_the_config_home_and_the_database_it_names() {
        let home = TempHome::new();
        let kb = home.path().join("kb");
        std::fs::create_dir_all(&kb).unwrap();
        let db = crate::resolve_db_path(&kb);
        std::fs::write(&db, b"not really a database").unwrap();
        std::fs::write(
            home.path().join("kb-mcp.toml"),
            format!("kb_path = '{}'\n", kb.display()),
        )
        .unwrap();

        let be = FakeBackend::ok();
        run_with_backend(&be, home.path(), params(true, true)).expect("purge should succeed");

        assert_eq!(be.uninstalled.take().as_deref(), Some("au28"));
        assert!(!db.exists(), "the index database must be deleted");
        assert!(
            !home.path().exists(),
            "the config home must be deleted: {}",
            home.path().display()
        );
    }

    /// A backend that refuses must abort before anything is deleted. Deleting
    /// the index of a service that is still registered would leave an
    /// installation that starts and finds nothing.
    #[test]
    fn a_failing_backend_deletes_nothing() {
        let home = TempHome::new();
        let toml = home.path().join("kb-mcp.toml");
        std::fs::write(&toml, "kb_path = '/nowhere'\n").unwrap();

        let be = FakeBackend::failing();
        let err = run_with_backend(&be, home.path(), params(true, true))
            .expect_err("a failing backend must propagate");

        assert!(
            err.to_string().contains("refused to unregister"),
            "the backend's own error must surface: {err}"
        );
        assert!(
            toml.exists(),
            "nothing may be deleted after a failed uninstall"
        );
        assert!(home.path().exists());
    }

    /// Without `--purge` the config home survives, and the backend is still
    /// asked to unregister.
    #[test]
    fn without_purge_the_config_home_survives() {
        let home = TempHome::new();
        let toml = home.path().join("kb-mcp.toml");
        std::fs::write(&toml, "kb_path = '/nowhere'\n").unwrap();

        let be = FakeBackend::ok();
        run_with_backend(&be, home.path(), params(false, false)).expect("uninstall should succeed");

        assert_eq!(be.uninstalled.take().as_deref(), Some("au28"));
        assert!(toml.exists(), "kb-mcp.toml must be kept without --purge");
    }

    /// `--purge` without `--yes` must refuse **before** touching the backend.
    /// The existing integration test covers the message; this one covers the
    /// ordering, which is what makes the guard worth having.
    #[test]
    fn purge_without_yes_does_not_reach_the_backend() {
        let home = TempHome::new();
        let toml = home.path().join("kb-mcp.toml");
        std::fs::write(&toml, "kb_path = '/nowhere'\n").unwrap();

        let be = FakeBackend::ok();
        let err = run_with_backend(&be, home.path(), params(true, false)).expect_err("must refuse");

        assert!(err.to_string().contains("--yes"), "{err}");
        assert!(
            be.uninstalled.take().is_none(),
            "the backend must not be consulted when the confirmation is missing"
        );
        assert!(toml.exists());
    }
}
