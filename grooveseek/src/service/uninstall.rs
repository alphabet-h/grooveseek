//! Uninstall orchestration for groove service backends.
use crate::service::{ServiceBackend, backend, resolve_config_home, validate_service_name};
use anyhow::{Result, anyhow};
use std::path::{Path, PathBuf};

pub struct UninstallParams {
    pub service_name: String,
    pub purge: bool,
    pub yes: bool,
}

pub fn run(params: UninstallParams) -> Result<()> {
    let name = validate_service_name(&params.service_name).map_err(|e| anyhow!(e))?;
    // `Option`, not `?`. Removing a scheduled task or a unit file needs only
    // the service name, and the original code reflected that: it reached for
    // the config home lazily, and on the non-purge path with `if let Ok(..)`,
    // so a machine where neither GROOVE_CONFIG_HOME nor `dirs::config_dir()`
    // resolves could still uninstall. Making resolution unconditional here
    // would have turned that into a hard failure.
    let config_home = resolve_config_home(&name).ok();
    run_with_backend(backend().as_ref(), config_home.as_deref(), params)
}

/// `run` with its two ambient dependencies passed in (AU-28).
///
/// Both are needed, not just the backend. `--purge --yes` deletes the config
/// home and the index database it names, so a test that supplied only a fake
/// backend would delete the *developer's* real ones. The other route — pointing
/// `GROOVE_CONFIG_HOME` somewhere safe — mutates process-wide state, and
/// `cargo test` runs tests on parallel threads of one process, which is the
/// unsoundness AU-63 was about.
///
/// `config_home` is optional because it is only *required* by `--purge`, which
/// cannot decide what to delete without it. Everything else treats it as
/// informational.
pub(crate) fn run_with_backend(
    be: &dyn ServiceBackend,
    config_home: Option<&Path>,
    params: UninstallParams,
) -> Result<()> {
    let name = validate_service_name(&params.service_name).map_err(|e| anyhow!(e))?;

    if params.purge && !params.yes {
        return Err(anyhow!(
            "--purge will delete the index database (.groove.db with its -wal / -shm) and groove.toml.\n\
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
        if let Err(e) = groove_tray::install::uninstall_autostart(&name) {
            eprintln!("Warning: tray autostart cleanup failed: {e}");
        }
    }

    if params.purge {
        let home = config_home
            .ok_or_else(|| {
                anyhow!(
                    "--purge needs the config home, but neither GROOVE_CONFIG_HOME nor the OS \
                     config directory could be resolved. The service unit has been removed; \
                     delete groove.toml and .groove.db (with its -wal / -shm) by hand."
                )
            })?
            .to_path_buf();
        // `.groove.db` lives next to the user's KB (= `resolve_db_path(kb_path)`),
        // NOT inside config_home. Read the configured `kb_path` from the
        // install-generated toml before deleting config_home so that the
        // advertised `--purge` cleanup actually removes the index database.
        // When it cannot be read, say so: the config home about to be removed
        // is the last place that names the database.
        match database_named_by(&home) {
            Ok(db) => purge_database(&db),
            Err(why) => eprintln!(
                "Warning: {why}; the index database (.groove.db with its -wal / -shm) \
                 was not removed"
            ),
        }

        if home.exists() {
            std::fs::remove_dir_all(&home)?;
            eprintln!(
                "Removed config home: {} (groove.toml + service files)",
                home.display()
            );
        }
    } else if let Some(home) = config_home
        && home.exists()
    {
        eprintln!(
            "Kept config home: {} (use --purge --yes to remove)",
            home.display()
        );
    }
    Ok(())
}

/// The `--purge` half that deletes the index database: `.groove.db` and its
/// `-wal` / `-shm` sidecars (kuriya #645). The database runs in WAL mode, so
/// committed pages that were never checkpointed live only in `-wal`; removing
/// the main file alone left them on disk.
///
/// Best-effort, like the rest of `--purge` after the service unit is gone:
/// the unit cannot be put back, so a file that will not go (on Windows, a
/// server `schtasks /End` has not finished stopping still holds it) is a
/// warning, and the config home is still removed. Every file is accounted
/// for on stderr, removed or left in place, so a partial purge is never
/// silent. The removal order is [`crate::db::remove_database_files`]'s: least
/// valuable first, so a failure leaves the database whole rather than a
/// database without its WAL.
///
/// Sidecars that are not index content are left alone and named when present:
/// `<db>.writer-lock` is never deleted (ADR-0031: deleting it while another
/// process waits on it lets two writers in), and `<db>.writer.json` /
/// `<db>.replace-lock` belong to whichever process holds them, which removes
/// them itself when it finishes. So is `<db>.writer.json.tmp-<pid>`, which a
/// failed replacement of `writer.json` can leave behind.
///
/// When `.groove.db` is a symlink, the files removed are the ones it points
/// to: SQLite opens the database through the link and puts `-wal` / `-shm`
/// beside the target (the writer lease resolves it the same way), so removing
/// the link alone would leave the whole index on disk. The target is resolved
/// once, before anything is removed, and every path printed -- removed, left
/// in place or kept -- is named from it. The link itself goes last, and only
/// once its target's files are gone, so a partial purge keeps the way to them.
fn purge_database(db: &Path) {
    let base = crate::writer_lease::sidecar_base(db);
    let kept = kept_sidecars(db, &base);
    let files: Vec<PathBuf> = crate::db::DATABASE_FILE_SUFFIXES
        .iter()
        .map(|suffix| with_suffix(&base, suffix))
        .collect();
    let present: Vec<&PathBuf> = files.iter().filter(|p| exists_no_follow(p)).collect();

    let mut removed_all = true;
    if !present.is_empty() {
        if let Err(e) = crate::db::remove_database_files(&base) {
            removed_all = false;
            eprintln!("Warning: failed to remove the index database: {e:#}");
        }
        for p in present {
            if exists_no_follow(p) {
                eprintln!("Warning: left in place: {}", p.display());
            } else {
                eprintln!("Removed index database file: {}", p.display());
            }
        }
    }

    if base != db && is_symlink(db) {
        if !removed_all {
            eprintln!(
                "Warning: left in place: {} (symlink to {}, kept with its target)",
                db.display(),
                base.display()
            );
        } else {
            match std::fs::remove_file(db) {
                Ok(()) => eprintln!(
                    "Removed symlink to the index database: {} -> {}",
                    db.display(),
                    base.display()
                ),
                Err(e) => eprintln!("Warning: left in place: {} ({e})", db.display()),
            }
        }
    }

    for p in kept {
        eprintln!(
            "Kept: {} (lock / holder file, not index content)",
            p.display()
        );
    }
}

/// The lock and holder files `--purge` leaves beside the database, as they
/// are on disk now: `<db>.writer-lock`, `<db>.writer.json`, any
/// `<db>.writer.json.tmp-<pid>`, and `<db>.replace-lock`. Looked up beside
/// `base` (the database file, symlink followed) and, when the caller's
/// spelling differs, beside `db` as well, where a caller that passed the
/// link's path would have put its `replace-lock`.
fn kept_sidecars(db: &Path, base: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    for root in [base, db] {
        let mut candidates = vec![
            with_suffix(root, ".writer-lock"),
            with_suffix(root, ".writer.json"),
            with_suffix(root, ".replace-lock"),
        ];
        if let (Some(dir), Some(name)) = (root.parent(), root.file_name()) {
            let dir = if dir.as_os_str().is_empty() {
                Path::new(".")
            } else {
                dir
            };
            let prefix = format!("{}.writer.json.tmp-", name.to_string_lossy());
            let mut tmps: Vec<PathBuf> = std::fs::read_dir(dir)
                .into_iter()
                .flatten()
                .flatten()
                .filter(|e| e.file_name().to_string_lossy().starts_with(&prefix))
                .map(|e| root.with_file_name(e.file_name()))
                .collect();
            tmps.sort();
            candidates.extend(tmps);
        }
        for p in candidates {
            if exists_no_follow(&p) && !found.contains(&p) {
                found.push(p);
            }
        }
    }
    found
}

/// The database a service's `groove.toml` names, or why it cannot be told.
///
/// `kb_path` is resolved the way the server resolves it when it loads that
/// file: a relative one against the directory holding `groove.toml` (the
/// config home), not against wherever `groove service uninstall` was run. A `kb_path` with
/// no parent (a filesystem root) is refused: [`crate::resolve_db_path`] would
/// fall back to `./.groove.db`, a path relative to the current directory that
/// need not be this service's database at all.
fn database_named_by(home: &Path) -> std::result::Result<PathBuf, String> {
    let toml_path = home.join("groove.toml");
    let text = std::fs::read_to_string(&toml_path)
        .map_err(|e| format!("could not read {}: {e}", toml_path.display()))?;
    let value = toml::from_str::<toml::Value>(&text)
        .map_err(|e| format!("could not parse {}: {e}", toml_path.display()))?;
    let kb = value
        .get("kb_path")
        .and_then(|p| p.as_str())
        .map(PathBuf::from)
        .ok_or_else(|| format!("{} names no kb_path", toml_path.display()))?;
    let kb = if kb.is_absolute() { kb } else { home.join(kb) };
    if kb.parent().is_none() {
        return Err(format!(
            "kb_path {} in {} has no parent directory to hold .groove.db",
            kb.display(),
            toml_path.display()
        ));
    }
    Ok(crate::resolve_db_path(&kb))
}

fn exists_no_follow(p: &Path) -> bool {
    std::fs::symlink_metadata(p).is_ok()
}

fn is_symlink(p: &Path) -> bool {
    std::fs::symlink_metadata(p).is_ok_and(|m| m.file_type().is_symlink())
}

/// `path` with `suffix` appended to its last component, the way the database
/// sidecars are named (`.groove.db` -> `.groove.db-wal`).
fn with_suffix(path: &Path, suffix: &str) -> std::path::PathBuf {
    let mut s = path.as_os_str().to_os_string();
    s.push(suffix);
    s.into()
}

#[cfg(target_os = "windows")]
pub fn run_tray_uninstall(service_name: &str) -> Result<()> {
    let name = validate_service_name(service_name).map_err(|e| anyhow!(e))?;
    groove_tray::install::uninstall_autostart(&name)?;
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
                .join(format!("groove-au28-{}-{nanos}-{seq}", std::process::id()));
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
    /// config home and the index database that `groove.toml` points at.
    ///
    /// Before AU-28 this could not be tested at all — `run` reached for the
    /// real backend and the real config home, so running it would have
    /// unregistered a service and deleted the developer's own files.
    #[test]
    fn purge_removes_the_config_home_and_the_database_it_names() {
        let home = TempHome::new();
        // The KB gets its own root, *outside* the config home, because that is
        // where a real one lives: `.groove.db` sits next to the user's KB, not
        // inside `~/.config/groove/<name>/`. Nesting it under `home` would make
        // the `remove_dir_all(home)` at the end of purge delete the database as
        // a side effect, and the assertion below would hold even with the
        // explicit database removal deleted — the test would pass for the
        // wrong reason (codex P2).
        let kb_root = TempHome::new();
        let kb = kb_root.path().join("kb");
        std::fs::create_dir_all(&kb).unwrap();
        let db = crate::resolve_db_path(&kb);
        std::fs::write(&db, b"not really a database").unwrap();
        assert!(
            !db.starts_with(home.path()),
            "the database must sit outside the config home for this test to mean anything"
        );
        std::fs::write(
            home.path().join("groove.toml"),
            format!("kb_path = '{}'\n", kb.display()),
        )
        .unwrap();

        let be = FakeBackend::ok();
        run_with_backend(&be, Some(home.path()), params(true, true)).expect("purge should succeed");

        assert_eq!(be.uninstalled.take().as_deref(), Some("au28"));
        assert!(!db.exists(), "the index database must be deleted");
        assert!(
            !home.path().exists(),
            "the config home must be deleted: {}",
            home.path().display()
        );
    }

    /// A KB root outside `home` with a `groove.toml` in `home` naming its KB;
    /// returns the KB root guard and the database path. Outside, for the
    /// reason the test above gives: nested, `remove_dir_all(home)` would
    /// delete the files and the assertions would hold for the wrong reason.
    fn kb_outside(home: &TempHome) -> (TempHome, PathBuf) {
        let kb_root = TempHome::new();
        let kb = kb_root.path().join("kb");
        std::fs::create_dir_all(&kb).unwrap();
        let db = crate::resolve_db_path(&kb);
        assert!(!db.starts_with(home.path()));
        std::fs::write(
            home.path().join("groove.toml"),
            format!("kb_path = '{}'\n", kb.display()),
        )
        .unwrap();
        (kb_root, db)
    }

    /// kuriya #645. The database runs in WAL mode, so committed pages that
    /// were never checkpointed live only in `-wal`. `--purge` used to remove
    /// the main file alone and leave both sidecars, and with them part of the
    /// index it promised to delete.
    #[test]
    fn purge_removes_the_wal_and_shm_beside_the_database() {
        let home = TempHome::new();
        let (_kb_root, db) = kb_outside(&home);
        let wal = with_suffix(&db, "-wal");
        let shm = with_suffix(&db, "-shm");
        std::fs::write(&db, b"main").unwrap();
        std::fs::write(&wal, b"uncheckpointed pages").unwrap();
        std::fs::write(&shm, b"wal index").unwrap();

        run_with_backend(&FakeBackend::ok(), Some(home.path()), params(true, true))
            .expect("purge should succeed");

        assert!(!db.exists(), "the database must be deleted");
        assert!(!wal.exists(), "the WAL must be deleted: {}", wal.display());
        assert!(!shm.exists(), "the shm must be deleted: {}", shm.display());
    }

    /// kuriya #645. The guard used to test the main file alone, so sidecars
    /// whose database was already gone were never looked at.
    #[test]
    fn purge_removes_sidecars_whose_database_is_already_gone() {
        let home = TempHome::new();
        let (_kb_root, db) = kb_outside(&home);
        let wal = with_suffix(&db, "-wal");
        let shm = with_suffix(&db, "-shm");
        std::fs::write(&wal, b"uncheckpointed pages").unwrap();
        std::fs::write(&shm, b"wal index").unwrap();
        assert!(!db.exists());

        run_with_backend(&FakeBackend::ok(), Some(home.path()), params(true, true))
            .expect("purge should succeed");

        assert!(!wal.exists(), "the WAL must be deleted: {}", wal.display());
        assert!(!shm.exists(), "the shm must be deleted: {}", shm.display());
    }

    /// kuriya #645. A file that will not go is a warning, not an abort: the
    /// service unit is already gone, so the config home is still removed --
    /// and the order leaves the database whole rather than without its WAL.
    #[test]
    fn a_wal_that_will_not_go_keeps_the_database_and_still_removes_the_config_home() {
        let home = TempHome::new();
        let (_kb_root, db) = kb_outside(&home);
        let wal = with_suffix(&db, "-wal");
        std::fs::write(&db, b"main").unwrap();
        // A directory where `-wal` goes is the portable way to make
        // `remove_file` fail (see the same trick in `db.rs`).
        std::fs::create_dir_all(&wal).unwrap();

        run_with_backend(&FakeBackend::ok(), Some(home.path()), params(true, true))
            .expect("purge is best-effort past the service unit");

        assert_eq!(
            std::fs::read(&db).unwrap(),
            b"main",
            "the database is kept whole"
        );
        assert!(!home.path().exists(), "the config home is still removed");
    }

    /// ADR-0031: `<db>.writer-lock` is never deleted, and the holder files
    /// belong to whichever process holds them. `--purge` removes the index
    /// and leaves all of them, and lists every one of them -- the leftover
    /// `writer.json.tmp-<pid>` included -- so none stays behind unnamed.
    #[test]
    fn purge_keeps_the_lock_and_holder_files_and_names_them() {
        let home = TempHome::new();
        let (_kb_root, db) = kb_outside(&home);
        std::fs::write(&db, b"main").unwrap();
        let lock = with_suffix(&db, ".writer-lock");
        let info = with_suffix(&db, ".writer.json");
        let tmp = with_suffix(&db, ".writer.json.tmp-4242");
        let replace = with_suffix(&db, ".replace-lock");
        for p in [&lock, &info, &tmp, &replace] {
            std::fs::write(p, b"").unwrap();
        }

        let named = kept_sidecars(&db, &db);
        for p in [&lock, &info, &tmp, &replace] {
            assert!(
                named.contains(p),
                "{} must be named: {named:?}",
                p.display()
            );
        }

        run_with_backend(&FakeBackend::ok(), Some(home.path()), params(true, true))
            .expect("purge should succeed");

        assert!(!db.exists(), "the database must be deleted");
        for p in [&lock, &info, &tmp, &replace] {
            assert!(p.exists(), "{} must be kept", p.display());
        }
    }

    /// The server opens `.groove.db` through a symlink, so its pages and its
    /// `-wal` / `-shm` sit beside the target. Removing the link alone used to
    /// leave the whole index on disk while stderr said it was removed.
    #[test]
    fn purge_removes_what_a_symlinked_database_points_to() {
        let home = TempHome::new();
        let (_kb_root, db) = kb_outside(&home);
        let elsewhere = TempHome::new();
        let target = elsewhere.path().join("real.db");
        let wal = with_suffix(&target, "-wal");
        let shm = with_suffix(&target, "-shm");
        let lock = with_suffix(&target, ".writer-lock");
        std::fs::write(&target, b"main").unwrap();
        std::fs::write(&wal, b"uncheckpointed pages").unwrap();
        std::fs::write(&shm, b"wal index").unwrap();
        std::fs::write(&lock, b"").unwrap();
        if !crate::writer_lease::file_symlink_or_skip(&target, &db) {
            return;
        }

        assert!(
            kept_sidecars(&db, &crate::writer_lease::sidecar_base(&db)).contains(&lock),
            "the lock beside the target must be named"
        );

        run_with_backend(&FakeBackend::ok(), Some(home.path()), params(true, true))
            .expect("purge should succeed");

        assert!(!target.exists(), "the database the link points to must go");
        assert!(!wal.exists(), "its WAL must go: {}", wal.display());
        assert!(!shm.exists(), "its shm must go: {}", shm.display());
        assert!(
            std::fs::symlink_metadata(&db).is_err(),
            "the link itself must go once its target is gone"
        );
        assert!(lock.exists(), "the writer-lock beside the target is kept");
    }

    /// The server resolves a relative `kb_path` against the directory holding
    /// `groove.toml`; `--purge` used to resolve it against its own working
    /// directory and could name another instance's database.
    #[test]
    fn a_relative_kb_path_is_resolved_against_the_config_home() {
        let home = TempHome::new();
        std::fs::write(home.path().join("groove.toml"), "kb_path = 'kb'\n").unwrap();

        let db = database_named_by(home.path()).expect("a relative kb_path names a database");

        assert_eq!(db, crate::resolve_db_path(&home.path().join("kb")));
    }

    /// A `kb_path` that is a filesystem root has no parent to hold
    /// `.groove.db`; [`crate::resolve_db_path`] would fall back to `./.groove.db` in
    /// the current directory, which `--purge` must not delete.
    #[test]
    fn a_root_kb_path_names_no_database() {
        let home = TempHome::new();
        let root = home.path().ancestors().last().unwrap().to_path_buf();
        std::fs::write(
            home.path().join("groove.toml"),
            format!("kb_path = '{}'\n", root.display()),
        )
        .unwrap();

        let why = database_named_by(home.path()).expect_err("a root has no parent");

        assert!(why.contains("no parent directory"), "{why}");
    }

    /// A `groove.toml` that cannot be read used to skip the database without
    /// a word and then delete the only file that named it. It is now a
    /// reason (printed as a warning), and the rest of the purge goes on.
    #[test]
    fn an_unparsable_config_names_no_database_and_the_home_still_goes() {
        let home = TempHome::new();
        std::fs::write(home.path().join("groove.toml"), "kb_path = [unclosed\n").unwrap();

        let why = database_named_by(home.path()).expect_err("broken toml");
        assert!(why.contains("could not parse"), "{why}");

        run_with_backend(&FakeBackend::ok(), Some(home.path()), params(true, true))
            .expect("purge is best-effort past the service unit");
        assert!(!home.path().exists(), "the config home is still removed");
    }

    /// A backend that refuses must abort before anything is deleted. Deleting
    /// the index of a service that is still registered would leave an
    /// installation that starts and finds nothing.
    #[test]
    fn a_failing_backend_deletes_nothing() {
        let home = TempHome::new();
        let toml = home.path().join("groove.toml");
        std::fs::write(&toml, "kb_path = '/nowhere'\n").unwrap();

        let be = FakeBackend::failing();
        let err = run_with_backend(&be, Some(home.path()), params(true, true))
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
        let toml = home.path().join("groove.toml");
        std::fs::write(&toml, "kb_path = '/nowhere'\n").unwrap();

        let be = FakeBackend::ok();
        run_with_backend(&be, Some(home.path()), params(false, false))
            .expect("uninstall should succeed");

        assert_eq!(be.uninstalled.take().as_deref(), Some("au28"));
        assert!(toml.exists(), "groove.toml must be kept without --purge");
    }

    /// `--purge` without `--yes` must refuse **before** touching the backend.
    /// The existing integration test covers the message; this one covers the
    /// ordering, which is what makes the guard worth having.
    #[test]
    fn purge_without_yes_does_not_reach_the_backend() {
        let home = TempHome::new();
        let toml = home.path().join("groove.toml");
        std::fs::write(&toml, "kb_path = '/nowhere'\n").unwrap();

        let be = FakeBackend::ok();
        let err =
            run_with_backend(&be, Some(home.path()), params(true, false)).expect_err("must refuse");

        assert!(err.to_string().contains("--yes"), "{err}");
        assert!(
            be.uninstalled.take().is_none(),
            "the backend must not be consulted when the confirmation is missing"
        );
        assert!(toml.exists());
    }
}
