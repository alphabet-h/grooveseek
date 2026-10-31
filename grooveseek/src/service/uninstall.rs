//! Uninstall orchestration for groove service backends.
use crate::service::{ServiceBackend, backend, resolve_config_home, validate_service_name};
use crate::writer_lease::{BareWriterLock, HolderInfo, LeaseError, WriterLease};
use anyhow::{Result, anyhow};
use std::path::{Path, PathBuf};
use std::time::Duration;

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

/// [`run`] with its two ambient dependencies passed in (AU-28).
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
    run_with_backend_waiting(be, config_home, params, PURGE_WRITER_WAIT)
}

/// How long `--purge` waits for the index's writer lock before it gives up
/// and deletes nothing. The backends stop the server without waiting for it
/// to exit (Windows' `schtasks /End` returns at once), so a server that is
/// stopping still holds the lock for a moment; one that is still holding it
/// after this long is taken to be running.
const PURGE_WRITER_WAIT: Duration = Duration::from_secs(10);

/// [`run_with_backend`] with the wait for the writer lock passed in, so a
/// test of the refusal need not sit out [`PURGE_WRITER_WAIT`].
fn run_with_backend_waiting(
    be: &dyn ServiceBackend,
    config_home: Option<&Path>,
    params: UninstallParams,
    writer_wait: Duration,
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
        match database_named_by(&home) {
            Ok(Some(db)) => {
                // Resolved once, whole; everything removed below is named
                // from this resolution (kuriya #645 review).
                let target = PurgeTarget::new(&db);
                purge_under_writer_lock(&target, &home, &name, writer_wait, remove_config_home)?;
            }
            Ok(None) => {
                // No groove.toml at all: nothing names a database, now or on
                // a re-run, so keeping the config home would only make the
                // purge impossible to finish.
                eprintln!(
                    "Warning: {} is not there, so no index database is named; only the \
                     config home is removed",
                    home.join("groove.toml").display()
                );
                remove_config_home(&home)?;
            }
            Err(why) => {
                // The config home's groove.toml is the only record of where
                // the database is. Removing it here would orphan the database
                // and leave a re-run nothing to find it or its lock by
                // (kuriya #645, local Codex r3), so nothing is removed.
                return Err(undecidable_database(&why, &home, &name));
            }
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

/// Delete the database `target` names and then the config home `home`, with
/// the index's writer lock held from before the files are listed until after
/// the config home is gone: a writer that took the lock in between could put
/// a `-wal` back after the last check, and the config home is the last place
/// that names the database.
///
/// `remove_home` is [`remove_config_home`]; it is a parameter so that a test
/// can look at the lock from inside the last step and see it still held.
///
/// When another process holds the lock past `writer_wait`, nothing is deleted
/// and the error names that process and how to stop it. When the lock cannot
/// be taken at all ([`WriterCheck::Unchecked`]: the lock file cannot be
/// created or opened), there is no holder to wait for, so the purge goes on
/// best-effort after a warning, as it did before it took the lock.
fn purge_under_writer_lock(
    target: &PurgeTarget,
    home: &Path,
    name: &str,
    writer_wait: Duration,
    remove_home: impl FnOnce(&Path) -> Result<()>,
) -> Result<()> {
    let _lock = match hold_writer_lock(target, writer_wait) {
        Ok(check) => {
            if let WriterCheck::Unchecked(why) = &check {
                eprintln!(
                    "Warning: {why}, so groove cannot tell whether a process still writes \
                     the index; purging without the lock"
                );
            }
            check
        }
        Err(busy) => {
            return Err(anyhow!(
                "{}. Neither the index database ({}) nor the config home ({}) was removed. \
                 The service unit is already gone; stop {}. Then run \
                 `groove service uninstall --service-name {name} --purge --yes` again \
                 to finish the purge.",
                busy.describe(),
                target.resolved.display(),
                home.display(),
                busy.how_to_stop()
            ));
        }
    };
    purge_database(target);
    remove_home(home)
}

/// The `--purge` half that deletes the index database: `.groove.db` and its
/// `-wal` / `-shm` sidecars (kuriya #645). The database runs in WAL mode, so
/// committed pages that were never checkpointed live only in `-wal`; removing
/// the main file alone left them on disk.
///
/// Called from [`purge_under_writer_lock`] with the index's writer lock held,
/// so no process that writes under the lock (`groove index`, a server's watcher or
/// rebuild, a reader's one-time write) can put a file back between the
/// listing, the removal and the last check below; or, when the lock could not
/// be taken at all, after a warning that says so.
///
/// Best-effort, like the rest of `--purge` after the service unit is gone:
/// the unit cannot be put back, so a file that will not go (on Windows, a
/// process that has the database open without the writer lock, such as a
/// server that never took it, still holds it) is a warning, and the config
/// home is still removed. Every file is accounted
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
/// the link alone would leave the whole index on disk. The whole path is
/// resolved once ([`PurgeTarget::new`]), the directory links above the file
/// included, and every file removed or printed is named from that one
/// resolution: a directory link repointed during the purge does not move the
/// removal into another directory. The link itself goes last, only once its
/// target's files are gone (so a partial purge keeps the way to them), and
/// only while it still holds the target it held when it was resolved
/// ([`remove_link_if_unchanged`]).
fn purge_database(target: &PurgeTarget) {
    let PurgeTarget { link, resolved, .. } = target;
    let kept = kept_sidecars(link, resolved);
    let files: Vec<PathBuf> = crate::db::DATABASE_FILE_SUFFIXES
        .iter()
        .map(|suffix| with_suffix(resolved, suffix))
        .collect();
    let present: Vec<&PathBuf> = files.iter().filter(|p| exists_no_follow(p)).collect();

    let mut removed_all = true;
    if !present.is_empty() {
        if let Err(e) = crate::db::remove_database_files(resolved) {
            removed_all = false;
            eprintln!(
                "Warning: failed to remove the index database: {}",
                ascii_error_chain(&e)
            );
        }
        for p in present {
            if exists_no_follow(p) {
                eprintln!("Warning: left in place: {}", p.display());
            } else {
                eprintln!("Removed index database file: {}", p.display());
            }
        }
    }

    match link_step(target, removed_all) {
        LinkStep::NotALink => {}
        LinkStep::Remove => remove_link_if_unchanged(target),
        LinkStep::Keep(warning) => eprintln!("{warning}"),
    }

    for p in kept {
        eprintln!(
            "Kept: {} (lock / holder file, not index content)",
            p.display()
        );
    }
}

/// The database `--purge` deletes, resolved once before anything is locked or
/// removed (kuriya #645 review).
#[derive(Debug)]
struct PurgeTarget {
    /// The path `groove.toml` names, its directory resolved whole: where the
    /// symlink is when `.groove.db` is one, and otherwise the database file
    /// itself (then equal to `resolved`).
    link: PathBuf,
    /// Whether `link` was a symlink when it was resolved.
    is_link: bool,
    /// What the symlink at `link` held when it was resolved, as
    /// [`std::fs::read_link`] spells it; `None` when `link` was not a symlink,
    /// or was one that could not be read (then the link is left in place and
    /// named, [`link_step`]). [`remove_link_if_unchanged`] removes the link
    /// only while it still holds this.
    link_target: Option<PathBuf>,
    /// The database file, every symlink and junction in its path resolved,
    /// the directories above it included ([`WriterLease::resolved_db_path`],
    /// the file a writer lease on `db` locks and its writer opens).
    resolved: PathBuf,
}

impl PurgeTarget {
    /// Resolve `db`'s directory once and build both paths from it: `link` is
    /// that directory joined with the file name, and `resolved` is `link`
    /// itself, or, when `link` is a symlink, the file it leads to, resolved
    /// from the contents read once for `link_target` (and `link`'s
    /// directories are resolved already, so none of them is looked up again
    /// through a link that may have been repointed).
    fn new(db: &Path) -> Self {
        let (link, dir_resolved) = match (db.parent(), db.file_name()) {
            (Some(dir), Some(name)) if !dir.as_os_str().is_empty() => {
                (WriterLease::resolved_db_path(dir).join(name), true)
            }
            // No directory to resolve (`database_named_by` never returns
            // such a path): resolve the whole of it instead.
            _ => (db.to_path_buf(), false),
        };
        let is_link = is_symlink(&link);
        let link_target = if is_link {
            std::fs::read_link(&link).ok()
        } else {
            None
        };
        let resolved = match &link_target {
            // Resolved from the link's contents as read once above, not by
            // reading the link again: a link repointed in between would
            // otherwise make `resolved` and `link_target` name two files.
            // The rest is what `resolved_db_path(&link)` does after its
            // first hop: a relative target is taken from the link's
            // directory, and further links are followed from there.
            Some(target) => {
                let first_hop = match link.parent() {
                    Some(dir) if target.is_relative() => dir.join(target),
                    _ => target.clone(),
                };
                WriterLease::resolved_db_path(&first_hop)
            }
            None if dir_resolved && !is_link => link.clone(),
            None => WriterLease::resolved_db_path(&link),
        };
        Self {
            link,
            is_link,
            link_target,
            resolved,
        }
    }
}

/// What [`purge_database`] does with `target.link` once the database files
/// are dealt with.
#[derive(Debug, PartialEq, Eq)]
enum LinkStep {
    /// `link` is the database file itself, already handled.
    NotALink,
    /// Remove the link, if it is unchanged ([`remove_link_if_unchanged`]).
    Remove,
    /// Leave the link, and print this warning about it.
    Keep(String),
}

/// Decide [`LinkStep`] for `target`, given whether every database file is gone.
fn link_step(target: &PurgeTarget, removed_all: bool) -> LinkStep {
    let PurgeTarget {
        link,
        is_link,
        link_target,
        resolved,
    } = target;
    if !is_link {
        return LinkStep::NotALink;
    }
    if link_target.is_none() {
        // A symlink whose contents could not be read: nothing to compare it
        // with before removing it, so it stays, named like any file kept.
        return LinkStep::Keep(format!(
            "Warning: left in place: {} (a symlink that could not be read)",
            link.display()
        ));
    }
    if !removed_all {
        return LinkStep::Keep(format!(
            "Warning: left in place: {} (symlink to {}, kept with its target)",
            link.display(),
            resolved.display()
        ));
    }
    LinkStep::Remove
}

/// What [`hold_writer_lock`] found when it went for the index's writer lock.
#[derive(Debug)]
enum WriterCheck {
    /// The lock is held; dropping this releases it.
    Held(
        // Never read: held for its `Drop`, which releases the lock.
        #[allow(dead_code)] BareWriterLock,
    ),
    /// The database's directory does not exist: no file of the index can be
    /// there for a writer to hold, and there is nothing to lock.
    NoDirectory,
    /// The lock could not be taken at all: the lock file could not be
    /// created or opened (permissions, a read-only or network filesystem), or
    /// the OS refused the lock call. No process is known to hold it, so there
    /// is nobody to wait for or stop, and groove cannot tell whether a
    /// process writes the index. Holds the reason, in ASCII.
    Unchecked(String),
}

/// Another groove process still held the index's writer lock when
/// [`hold_writer_lock`] stopped waiting for it.
#[derive(Debug)]
struct WriterBusy {
    lock_path: PathBuf,
    /// Where `holder` was read from: `<db>.writer.json` beside the lock.
    info_path: PathBuf,
    /// What `<db>.writer.json` says, when it could be read.
    holder: Option<HolderInfo>,
    waited: Duration,
}

impl WriterBusy {
    /// Who holds the lock, where, and for how long purge waited.
    ///
    /// The pid is the one `<db>.writer.json` records, which is display only
    /// ([`HolderInfo`]): after a holder that died without cleaning up, or in
    /// the moment between a new holder taking the OS lock and writing its own
    /// record, it is a previous holder's, and the OS may have given it to an
    /// unrelated process since. The message says so, as `groove index`'s
    /// refusal does ([`crate::writer_lease::index_busy_message`]).
    fn describe(&self) -> String {
        match &self.holder {
            Some(h) => format!(
                "a groove process (pid {}, {}, as recorded in {}, which can be stale) \
                 still holds the writer lock at {} after {}s",
                h.pid,
                h.role.as_str(),
                self.info_path.display(),
                self.lock_path.display(),
                self.waited.as_secs()
            ),
            None => format!(
                "a groove process still holds the writer lock at {} after {}s",
                self.lock_path.display(),
                self.waited.as_secs()
            ),
        }
    }

    /// The process to stop before purge is run again. When a pid is recorded,
    /// the way to confirm it is groove comes before the command that stops
    /// it: the record can be stale ([`Self::describe`]), and a reused pid
    /// names an unrelated process.
    fn how_to_stop(&self) -> String {
        match &self.holder {
            Some(h) => format!(
                "that process: first confirm pid {pid} is groove, since the recorded pid \
                 can belong to an earlier holder and be reused by now \
                 (Windows: `tasklist /FI \"PID eq {pid}\"`; Linux / macOS: `ps -p {pid}`), \
                 and only if it is, stop it \
                 (Windows: `taskkill /PID {pid}`; Linux / macOS: `kill {pid}`); \
                 if it is not groove, do not stop it: the record is stale, and the lock \
                 is held by another groove process, to be found by its name",
                pid = h.pid
            ),
            None => "the groove process that holds it".to_string(),
        }
    }
}

/// Take the writer lock on `target`'s database, waiting up to `wait` for a
/// server the backend has told to stop to let go of it; or report the
/// process that would not let go ([`WriterBusy`]).
///
/// What the lock does not cover: a process that opens the database without
/// it (a server that never took the lock or gave it up and goes on as a
/// reader, or a read-only command such as `groove search`). On Windows its
/// open files will not go and are reported as left in place; on unix they
/// are unlinked under it.
#[allow(clippy::result_large_err)] // once per purge, like `LeaseError`'s callers
fn hold_writer_lock(
    target: &PurgeTarget,
    wait: Duration,
) -> std::result::Result<WriterCheck, WriterBusy> {
    let resolved = &target.resolved;
    let first = match BareWriterLock::try_hold(resolved) {
        Err(busy @ LeaseError::Busy { .. }) if !wait.is_zero() => {
            eprintln!(
                "Waiting up to {}s for the groove process holding the writer lock of {} to exit...",
                wait.as_secs(),
                resolved.display()
            );
            BareWriterLock::hold_with_wait(resolved, wait, busy)
        }
        other => other,
    };
    match first {
        Ok(lock) => Ok(WriterCheck::Held(lock)),
        Err(LeaseError::Io { source, .. })
            if source.kind() == std::io::ErrorKind::NotFound
                && !resolved.parent().is_some_and(exists_no_follow) =>
        {
            Ok(WriterCheck::NoDirectory)
        }
        Err(LeaseError::Busy { lock_path, holder }) => Err(WriterBusy {
            lock_path,
            // The file [`BareWriterLock::try_hold`] read `holder` from.
            info_path: with_suffix(resolved, ".writer.json"),
            holder,
            waited: wait,
        }),
        Err(e) => Ok(WriterCheck::Unchecked(e.to_string())),
    }
}

/// Remove the symlink `target.link` if it is still a symlink holding what it
/// held when the purge resolved it to `target.resolved` (the file whose
/// removal it was the way to; [`link_unchanged`]); otherwise leave it and say so. A link
/// repointed during the purge names another database, which this purge did
/// not remove and so must not orphan or misreport.
///
/// The link's contents are compared as they are spelled, not resolved: the
/// file they led to is gone by now, and a path resolved through a missing
/// file keeps the link's spelling, which on a case-insensitive filesystem
/// can differ from `resolved` while naming the same file.
///
/// The check narrows the window, it does not close it: `link` can still be
/// swapped between the check and `remove_file`. Closing it would take
/// removing by a handle opened on the link (`unlinkat` under an `O_NOFOLLOW`
/// directory handle on unix, `FILE_FLAG_OPEN_REPARSE_POINT` with a delete
/// disposition on Windows), which `std` does not offer.
fn remove_link_if_unchanged(target: &PurgeTarget) {
    let PurgeTarget { link, resolved, .. } = target;
    if !link_unchanged(target) {
        eprintln!(
            "Warning: left in place: {} (it no longer links to {}; it changed during the purge)",
            link.display(),
            resolved.display()
        );
        return;
    }
    match std::fs::remove_file(link) {
        Ok(()) => eprintln!(
            "Removed symlink to the index database: {} -> {}",
            link.display(),
            resolved.display()
        ),
        Err(e) => eprintln!(
            "Warning: left in place: {} ({})",
            link.display(),
            crate::embedder::ascii_io_error(&e)
        ),
    }
}

/// Whether `target.link` is still a symlink holding what it held when it was
/// resolved ([`PurgeTarget::link_target`]), compared as spelled. False when
/// it was not a symlink then.
fn link_unchanged(target: &PurgeTarget) -> bool {
    let Some(then) = &target.link_target else {
        return false;
    };
    is_symlink(&target.link) && std::fs::read_link(&target.link).is_ok_and(|now| &now == then)
}

/// `error` and its causes joined as `{:#}` joins them, with every
/// [`std::io::Error`] among them shown by kind and code only
/// ([`crate::embedder::ascii_io_error`]): the OS words its message in the
/// local language, and stderr stays ASCII (AGENTS.md).
fn ascii_error_chain(error: &anyhow::Error) -> String {
    error
        .chain()
        .map(|cause| match cause.downcast_ref::<std::io::Error>() {
            Some(io) => crate::embedder::ascii_io_error(io),
            None => cause.to_string(),
        })
        .collect::<Vec<_>>()
        .join(": ")
}

/// The error `--purge` stops with when the config home's `groove.toml` is
/// there but does not tell where the index database is (`why`, from
/// [`database_named_by`]). Nothing has been removed but the service unit,
/// and the backends' `uninstall` succeeds on a unit that is already gone, so
/// the re-run it suggests can finish the purge.
fn undecidable_database(why: &str, home: &Path, name: &str) -> anyhow::Error {
    anyhow!(
        "{why}. groove cannot tell where the index database is, so nothing was removed: \
         the config home ({}) is kept, because its groove.toml ({}) is the only record of \
         where the database is. The service unit is already gone. Either fix groove.toml and \
         run `groove service uninstall --service-name {name} --purge --yes` again to finish \
         the purge, or delete the index database by hand (.groove.db in the parent directory \
         of the KB, with its .groove.db-wal / .groove.db-shm) and then the config home.",
        home.display(),
        home.join("groove.toml").display()
    )
}

/// Remove the config home, if it is there, and say so.
fn remove_config_home(home: &Path) -> Result<()> {
    if home.exists() {
        std::fs::remove_dir_all(home)?;
        eprintln!(
            "Removed config home: {} (groove.toml + service files)",
            home.display()
        );
    }
    Ok(())
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
/// `Ok(None)` only when `groove.toml` is not there at all (the open fails
/// with "not found"): then nothing names a database. A file that is there
/// but cannot be read (permissions, a directory or a named pipe in its
/// place, over the cap, not UTF-8, not TOML, no usable `kb_path`) is an
/// error, because the database it may name cannot be told apart from none.
///
/// `kb_path` is resolved the way the server resolves it when it loads that
/// file: a relative one against the directory holding `groove.toml` (the
/// config home), not against wherever `groove service uninstall` was run. A `kb_path` with
/// no parent (a filesystem root) is refused: [`crate::resolve_db_path`] would
/// fall back to `./.groove.db`, a path relative to the current directory that
/// need not be this service's database at all.
///
/// The file is read the way the server reads it, since the service's launch
/// line names it with `--config` ([`crate::config::Config::load_from`]): by
/// [`crate::links::read_required_following`], bounded by
/// [`crate::config::MAX_CONFIG_FILE_BYTES`], following links, and refusing
/// what they lead to unless it is a regular file. A named pipe or a device in
/// its place would otherwise block the purge, and a huge file would be read
/// whole, after the service unit is already gone (kuriya #645, local Codex
/// r2). Only the read is shared: [`crate::config::Config::load_from`] itself
/// is not called, because it also checks every other key against this
/// build's schema, and a config this build would refuse for an unrelated key
/// still names the database to purge.
fn database_named_by(home: &Path) -> std::result::Result<Option<PathBuf>, String> {
    let toml_path = home.join("groove.toml");
    let Some(bytes) = crate::links::read_required_following(
        &toml_path,
        crate::config::MAX_CONFIG_FILE_BYTES,
        "config",
    )
    // The chain names the file already ("refusing to read the config: <path>:
    // <reason>", "failed to read the config: <path>: <io error>").
    .map_err(|e| ascii_error_chain(&e))?
    else {
        return Ok(None);
    };
    let text = String::from_utf8(bytes)
        .map_err(|_| format!("could not read {}: it is not UTF-8", toml_path.display()))?;
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
    Ok(Some(crate::resolve_db_path(&kb)))
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

        assert_eq!(db, Some(crate::resolve_db_path(&home.path().join("kb"))));
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

        // Nor does the purge go on without it (local Codex r3): the config
        // home that holds the kb_path to fix is kept.
        run_with_backend(&FakeBackend::ok(), Some(home.path()), params(true, true))
            .expect_err("a root kb_path stops the purge");
        assert!(
            home.path().join("groove.toml").exists(),
            "groove.toml stays"
        );
    }

    /// A `groove.toml` that cannot be read used to skip the database without
    /// a word and then delete the only file that named it. Then a warning
    /// said so and the config home still went, which orphaned the database:
    /// a re-run had nothing left to find it or its lock by (kuriya #645,
    /// local Codex r3). Now nothing is removed, and the error names the
    /// reason, the config home, its `groove.toml` and the two ways to finish.
    #[test]
    fn an_unparsable_config_keeps_the_config_home_and_says_how_to_finish() {
        let home = TempHome::new();
        let toml = home.path().join("groove.toml");
        std::fs::write(&toml, "kb_path = [unclosed\n").unwrap();

        let why = database_named_by(home.path()).expect_err("broken toml");
        assert!(why.contains("could not parse"), "{why}");

        let be = FakeBackend::ok();
        let err = run_with_backend(&be, Some(home.path()), params(true, true))
            .expect_err("a database that cannot be located stops the purge");
        let msg = format!("{err:#}");

        assert_eq!(
            be.uninstalled.take().as_deref(),
            Some("au28"),
            "the service unit is removed first"
        );
        assert!(toml.exists(), "groove.toml is kept: {}", toml.display());
        assert!(msg.contains("could not parse"), "{msg}");
        assert!(msg.contains(&home.path().display().to_string()), "{msg}");
        assert!(msg.contains(&toml.display().to_string()), "{msg}");
        assert!(msg.contains("service unit is already gone"), "{msg}");
        assert!(
            msg.contains("groove service uninstall --service-name au28 --purge --yes"),
            "{msg}"
        );
        assert!(msg.contains(".groove.db-wal / .groove.db-shm"), "{msg}");
        // The paths are the machine's; everything else stays ASCII (AGENTS.md).
        let words = msg.replace(&home.path().display().to_string(), "");
        assert!(words.is_ascii(), "stderr stays ASCII: {msg}");
    }

    /// kuriya #645, local Codex r3. The refusal above is not a dead end: the
    /// re-run it names removes the service unit again (the backends succeed
    /// on a unit that is already gone) and, with `groove.toml` fixed, purges
    /// the database and the config home as a first run would have.
    #[test]
    fn a_purge_stopped_by_a_broken_config_finishes_once_the_config_is_fixed() {
        let home = TempHome::new();
        let (_kb_root, db) = kb_outside(&home);
        let wal = with_suffix(&db, "-wal");
        std::fs::write(&db, b"main").unwrap();
        std::fs::write(&wal, b"pages").unwrap();
        let toml = home.path().join("groove.toml");
        let good = std::fs::read_to_string(&toml).unwrap();
        std::fs::write(&toml, "kb_path = [unclosed\n").unwrap();

        run_with_backend(&FakeBackend::ok(), Some(home.path()), params(true, true))
            .expect_err("a broken groove.toml stops the purge");
        assert_eq!(std::fs::read(&db).unwrap(), b"main", "the database stays");
        assert!(wal.exists(), "the WAL stays: {}", wal.display());
        assert!(home.path().exists(), "the config home stays");

        std::fs::write(&toml, good).unwrap();
        run_with_backend(&FakeBackend::ok(), Some(home.path()), params(true, true))
            .expect("the re-run finishes the purge");
        assert!(!db.exists(), "the database is deleted on the re-run");
        assert!(!wal.exists(), "the WAL is deleted on the re-run");
        assert!(
            !home.path().exists(),
            "the config home is deleted on the re-run"
        );
    }

    /// kuriya #645, local Codex r3. Only a `groove.toml` that is not there
    /// counts as absent. A directory in its place is something there that
    /// cannot be read, so the purge stops and keeps the config home.
    #[test]
    fn a_groove_toml_that_is_a_directory_is_not_taken_for_an_absent_one() {
        let home = TempHome::new();
        std::fs::create_dir_all(home.path().join("groove.toml")).unwrap();

        let why = database_named_by(home.path()).expect_err("a directory is not a config");
        assert!(why.contains("groove.toml"), "{why}");

        run_with_backend(&FakeBackend::ok(), Some(home.path()), params(true, true))
            .expect_err("a directory in place of groove.toml stops the purge");
        assert!(
            home.path().join("groove.toml").is_dir(),
            "the config home stays"
        );
    }

    /// kuriya #645, local Codex r3. With no `groove.toml` at all, nothing
    /// names a database now or on any re-run, so keeping the config home
    /// would make the purge impossible to finish: it warns and removes the
    /// config home, as it did before.
    #[test]
    fn a_config_home_without_groove_toml_is_still_removed() {
        let home = TempHome::new();
        std::fs::write(home.path().join("service.xml"), b"<task/>").unwrap();

        assert_eq!(
            database_named_by(home.path()),
            Ok(None),
            "no groove.toml names no database"
        );

        run_with_backend(&FakeBackend::ok(), Some(home.path()), params(true, true))
            .expect("a config home without groove.toml is purged");
        assert!(!home.path().exists(), "the config home is removed");
    }

    /// kuriya #645, local Codex r2. `groove.toml` is read the way the server
    /// reads its `--config`, bounded by [`crate::config::MAX_CONFIG_FILE_BYTES`].
    /// One byte over it (a valid file that names the database, padded with a
    /// comment, so an unbounded read would parse it and purge the database),
    /// it names no database: the purge stops with an error and removes
    /// nothing, the database and the config home that names it both stay, as
    /// for any config that cannot be read (local Codex r3).
    #[test]
    fn a_groove_toml_over_the_config_cap_names_no_database_and_the_database_stays() {
        let home = TempHome::new();
        let (_kb_root, db) = kb_outside(&home);
        let wal = with_suffix(&db, "-wal");
        std::fs::write(&db, b"main").unwrap();
        std::fs::write(&wal, b"pages").unwrap();
        let toml = home.path().join("groove.toml");
        let mut body = std::fs::read_to_string(&toml).unwrap();
        body.push('#');
        let cap = usize::try_from(crate::config::MAX_CONFIG_FILE_BYTES).unwrap();
        let pad = cap + 1 - body.len() - 1;
        body.push_str(&"x".repeat(pad));
        body.push('\n');
        assert_eq!(body.len(), cap + 1, "one byte over the cap");
        std::fs::write(&toml, body).unwrap();

        let why = database_named_by(home.path()).expect_err("over the cap is refused");
        assert!(
            why.contains(&format!(
                "over the {} byte limit",
                crate::config::MAX_CONFIG_FILE_BYTES
            )),
            "{why}"
        );

        let err = run_with_backend_waiting(
            &FakeBackend::ok(),
            Some(home.path()),
            params(true, true),
            Duration::ZERO,
        )
        .expect_err("a database that cannot be located stops the purge");
        assert!(
            format!("{err:#}").contains("over the"),
            "the error gives the reason: {err:#}"
        );
        assert_eq!(std::fs::read(&db).unwrap(), b"main", "the database stays");
        assert!(wal.exists(), "the WAL stays: {}", wal.display());
        assert!(toml.exists(), "the config home and its groove.toml stay");
    }

    /// kuriya #645, local Codex r2. A named pipe in place of `groove.toml`
    /// is refused before it is read, rather than blocking the purge on a
    /// writer that never comes.
    #[cfg(unix)]
    #[test]
    fn a_groove_toml_that_is_a_fifo_is_refused_promptly() {
        let home = TempHome::new();
        let toml = home.path().join("groove.toml");
        let c_path = std::ffi::CString::new(toml.to_str().unwrap()).unwrap();
        // SAFETY: `c_path` is a valid NUL-terminated path that outlives the call.
        let rc = unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) };
        assert_eq!(rc, 0, "could not create the fifo to test against");

        let why = database_named_by(home.path()).expect_err("a fifo is not a config");

        assert!(why.contains("not a regular file"), "{why}");

        // Something is there, so it is not taken for an absent groove.toml:
        // the purge stops and the config home stays (local Codex r3).
        run_with_backend(&FakeBackend::ok(), Some(home.path()), params(true, true))
            .expect_err("a fifo in place of groove.toml stops the purge");
        assert!(home.path().exists(), "the config home stays");
    }

    /// kuriya #645, local Codex r2. The pid in the refusal comes from
    /// `<db>.writer.json`, which is display only and can be stale, so the
    /// refusal says so, and puts the way to confirm the pid is groove before
    /// the commands that stop it.
    #[test]
    fn a_held_writer_lock_says_to_confirm_the_recorded_pid_before_stopping_it() {
        let home = TempHome::new();
        let (kb_root, db) = kb_outside(&home);
        std::fs::write(&db, b"main").unwrap();
        let _lease = crate::writer_lease::WriterLease::try_acquire(
            &db,
            crate::writer_lease::Role::Serve,
            &kb_root.path().join("kb"),
        )
        .expect("nobody else holds the lease");
        let pid = std::process::id();

        let msg = run_with_backend_waiting(
            &FakeBackend::ok(),
            Some(home.path()),
            params(true, true),
            Duration::ZERO,
        )
        .expect_err("a held writer lock must stop the purge")
        .to_string();

        assert_confirm_before_stop(&msg, pid);
    }

    /// kuriya #645, local Codex r2. A holder record left by a process that is
    /// gone (here a pid no process has: not a multiple of 4, so never one on
    /// Windows, and above every Linux / macOS `pid_max`) while another
    /// process holds the lock: the refusal names the recorded pid as possibly
    /// stale and says to confirm it, and not to stop it when it is not groove.
    #[test]
    fn a_stale_holder_record_is_named_as_such_and_confirmed_before_stopping() {
        let home = TempHome::new();
        let (kb_root, db) = kb_outside(&home);
        std::fs::write(&db, b"main").unwrap();
        let target = PurgeTarget::new(&db);
        let _held = BareWriterLock::try_hold(&target.resolved).expect("nobody holds the lock");
        let gone = u32::MAX - 2;
        let record = HolderInfo {
            pid: gone,
            role: crate::writer_lease::Role::Index,
            started_at: "2026-01-01T00:00:00Z".into(),
            version: "0.0.0".into(),
            kb_path: kb_root.path().join("kb").display().to_string(),
            heartbeat_at: None,
            watching: None,
        };
        let info = with_suffix(&target.resolved, ".writer.json");
        std::fs::write(&info, serde_json::to_vec(&record).unwrap()).unwrap();

        let msg = run_with_backend_waiting(
            &FakeBackend::ok(),
            Some(home.path()),
            params(true, true),
            Duration::ZERO,
        )
        .expect_err("a held writer lock must stop the purge")
        .to_string();

        assert_confirm_before_stop(&msg, gone);
        assert!(
            msg.contains(&format!("as recorded in {}", info.display())),
            "{msg}"
        );
        assert!(msg.contains("if it is not groove, do not stop it"), "{msg}");
        assert!(db.exists(), "the database is kept");
    }

    /// The refusal names `pid` as recorded and possibly stale, and gives the
    /// commands that confirm it is groove before the ones that stop it.
    fn assert_confirm_before_stop(msg: &str, pid: u32) {
        assert!(msg.contains("which can be stale"), "{msg}");
        let confirm_win = format!("`tasklist /FI \"PID eq {pid}\"`");
        let confirm_unix = format!("`ps -p {pid}`");
        let stop_win = format!("`taskkill /PID {pid}`");
        let stop_unix = format!("`kill {pid}`");
        let at = |needle: &str| {
            msg.find(needle)
                .unwrap_or_else(|| panic!("{needle} missing: {msg}"))
        };
        assert!(at(&confirm_win) < at(&stop_win), "confirm first: {msg}");
        assert!(at(&confirm_unix) < at(&stop_unix), "confirm first: {msg}");
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

    /// kuriya #645 review (I1). A process that still holds the writer lock
    /// (a server the backend told to stop but that has not exited) could put
    /// a `-wal` back after purge looked. Purge now deletes nothing -- not the
    /// database, not the config home that names it -- and says to run it
    /// again; run again once the holder is gone, it finishes.
    #[test]
    fn a_database_whose_writer_lock_is_held_is_kept_with_the_config_home() {
        let home = TempHome::new();
        let (kb_root, db) = kb_outside(&home);
        let wal = with_suffix(&db, "-wal");
        std::fs::write(&db, b"main").unwrap();
        std::fs::write(&wal, b"uncheckpointed pages").unwrap();
        let toml = home.path().join("groove.toml");
        let lease = crate::writer_lease::WriterLease::try_acquire(
            &db,
            crate::writer_lease::Role::Serve,
            &kb_root.path().join("kb"),
        )
        .expect("nobody else holds the lease");

        let be = FakeBackend::ok();
        let err =
            run_with_backend_waiting(&be, Some(home.path()), params(true, true), Duration::ZERO)
                .expect_err("a held writer lock must stop the purge");

        let msg = err.to_string();
        assert!(msg.contains("writer lock"), "{msg}");
        assert!(
            msg.contains("--purge --yes"),
            "it must say to run again: {msg}"
        );
        assert_eq!(be.uninstalled.take().as_deref(), Some("au28"));
        assert_eq!(std::fs::read(&db).unwrap(), b"main", "the database is kept");
        assert!(wal.exists(), "the WAL is kept: {}", wal.display());
        assert!(toml.exists(), "the config home naming the database is kept");

        drop(lease);
        run_with_backend_waiting(
            &FakeBackend::ok(),
            Some(home.path()),
            params(true, true),
            Duration::ZERO,
        )
        .expect("run again with the lock free, the purge finishes");
        assert!(!db.exists(), "the database must be deleted");
        assert!(!wal.exists(), "the WAL must be deleted");
        assert!(!home.path().exists(), "the config home must be deleted");
    }

    /// kuriya #645 review (I1). The backend stops a server without waiting
    /// for it to exit, so purge waits for the writer lock before it gives up.
    /// [`hold_writer_lock`] is called directly: through [`super::run_with_backend`],
    /// the Windows tray cleanup before it can take longer than the hold below,
    /// and the test would pass without waiting.
    #[test]
    fn purge_waits_for_a_stopping_writer_to_let_go() {
        let home = TempHome::new();
        let (kb_root, db) = kb_outside(&home);
        std::fs::write(&db, b"main").unwrap();
        let lease = crate::writer_lease::WriterLease::try_acquire(
            &db,
            crate::writer_lease::Role::Serve,
            &kb_root.path().join("kb"),
        )
        .expect("nobody else holds the lease");
        let target = PurgeTarget::new(&db);
        assert!(
            BareWriterLock::try_hold(&target.resolved).is_err(),
            "the lease must keep purge's lock out while it is held"
        );
        let hold = Duration::from_millis(300);
        let started = std::time::Instant::now();
        let releaser = std::thread::spawn(move || {
            std::thread::sleep(hold);
            drop(lease);
        });

        let check = hold_writer_lock(&target, Duration::from_secs(10));
        let waited = started.elapsed();
        // Joined before anything below can panic, so a failure does not
        // leave the releaser running past the test.
        releaser.join().unwrap();

        let check = check.expect("the lock comes free within the wait");
        assert!(
            matches!(check, WriterCheck::Held(_)),
            "the lock is held for the purge: {check:?}"
        );
        assert!(
            waited >= hold,
            "it must have waited for the release: {waited:?}"
        );
    }

    /// kuriya #645 review (I2). The database's directory is reached through a
    /// directory link that is repointed after purge resolved it. The files
    /// removed are the ones resolved, not the other database the link now
    /// leads to.
    #[test]
    fn a_directory_link_repointed_after_resolution_does_not_move_the_removal() {
        use crate::writer_lease::DirLink;
        for kind in DirLink::all() {
            let root = TempHome::new();
            let first = root.path().join("first");
            let second = root.path().join("second");
            for dir in [&first, &second] {
                std::fs::create_dir_all(dir).unwrap();
                std::fs::write(dir.join(".groove.db"), b"main").unwrap();
                std::fs::write(dir.join(".groove.db-wal"), b"pages").unwrap();
            }
            let alias = root.path().join("alias");
            if !kind.point(&alias, &first) {
                continue;
            }
            let db = alias.join(".groove.db");

            let target = PurgeTarget::new(&db);
            assert!(kind.point(&alias, &second), "{kind:?}: repoint");
            purge_database(&target);

            assert!(
                !first.join(".groove.db").exists() && !first.join(".groove.db-wal").exists(),
                "{kind:?}: the resolved database must go"
            );
            assert_eq!(
                std::fs::read(second.join(".groove.db")).unwrap(),
                b"main",
                "{kind:?}: the database the link now leads to is not this purge's"
            );
            assert!(second.join(".groove.db-wal").exists(), "{kind:?}");
            #[cfg(windows)]
            std::fs::remove_dir(&alias).unwrap();
        }
    }

    /// kuriya #645 review (I2). A `.groove.db` symlink repointed after purge
    /// resolved it now names another database. Purge removes the one it
    /// resolved and leaves the link, rather than removing a link it did not
    /// follow and reporting it as the way to the removed file.
    #[test]
    fn a_database_link_repointed_after_resolution_is_left_in_place() {
        let home = TempHome::new();
        let (_kb_root, db) = kb_outside(&home);
        let elsewhere = TempHome::new();
        let resolved_one = elsewhere.path().join("one.db");
        let other_one = elsewhere.path().join("other.db");
        std::fs::write(&resolved_one, b"main").unwrap();
        std::fs::write(&other_one, b"other").unwrap();
        if !crate::writer_lease::file_symlink_or_skip(&resolved_one, &db) {
            return;
        }

        let target = PurgeTarget::new(&db);
        std::fs::remove_file(&db).unwrap();
        assert!(crate::writer_lease::file_symlink_or_skip(&other_one, &db));
        purge_database(&target);

        assert!(!resolved_one.exists(), "the resolved database must go");
        assert_eq!(std::fs::read(&other_one).unwrap(), b"other");
        assert!(
            std::fs::symlink_metadata(&db).is_ok_and(|m| m.file_type().is_symlink()),
            "the repointed link must stay"
        );
    }

    /// kuriya #645 review (I1). The writer lock is taken before the files are
    /// listed and let go only after the config home is gone, the last place
    /// that names the database. Looked at from inside the last step: another
    /// handle on the lock must still be kept out there.
    #[test]
    fn the_writer_lock_is_held_until_the_config_home_is_gone() {
        let home = TempHome::new();
        let (_kb_root, db) = kb_outside(&home);
        std::fs::write(&db, b"main").unwrap();
        let target = PurgeTarget::new(&db);

        let mut seen_by_last_step = None;
        purge_under_writer_lock(&target, home.path(), "au28", Duration::ZERO, |h| {
            seen_by_last_step = Some(BareWriterLock::try_hold(&target.resolved));
            remove_config_home(h)
        })
        .expect("nobody else holds the lock");

        let seen = seen_by_last_step.expect("the config home step ran");
        assert!(
            matches!(seen, Err(LeaseError::Busy { .. })),
            "the lock must still be held while the config home is removed: {seen:?}"
        );
        assert!(!db.exists(), "the database must be deleted");
        assert!(!home.path().exists(), "the config home must be deleted");
        assert!(
            BareWriterLock::try_hold(&target.resolved).is_ok(),
            "the lock is let go once the purge is over"
        );
    }

    /// kuriya #645 review (I1). A held lock is refused with the holder named
    /// and the way to stop it, not a wait for it to exit on its own: a
    /// server the backend could not stop does not exit by itself.
    #[test]
    fn a_held_writer_lock_is_refused_with_the_holder_and_how_to_stop_it() {
        let home = TempHome::new();
        let (kb_root, db) = kb_outside(&home);
        std::fs::write(&db, b"main").unwrap();
        let _lease = crate::writer_lease::WriterLease::try_acquire(
            &db,
            crate::writer_lease::Role::Serve,
            &kb_root.path().join("kb"),
        )
        .expect("nobody else holds the lease");
        let pid = std::process::id();

        let err = run_with_backend_waiting(
            &FakeBackend::ok(),
            Some(home.path()),
            params(true, true),
            Duration::ZERO,
        )
        .expect_err("a held writer lock must stop the purge");

        let msg = err.to_string();
        assert!(msg.contains(&format!("pid {pid}, serve")), "{msg}");
        assert!(msg.contains(&format!("`taskkill /PID {pid}`")), "{msg}");
        assert!(msg.contains(&format!("`kill {pid}`")), "{msg}");
        assert!(
            msg.contains("`groove service uninstall --service-name au28 --purge --yes`"),
            "{msg}"
        );
        assert!(db.exists(), "the database is kept");
    }

    /// The database's directory is not there: there is no file to hold and
    /// none to lock, so purge takes no lock, creates nothing there, and goes
    /// on to the config home.
    #[test]
    fn a_database_directory_that_is_not_there_is_nothing_to_lock() {
        let home = TempHome::new();
        let kb_root = TempHome::new();
        let missing = kb_root.path().join("missing");
        let kb = missing.join("kb");
        std::fs::write(
            home.path().join("groove.toml"),
            format!("kb_path = '{}'\n", kb.display()),
        )
        .unwrap();
        let db = crate::resolve_db_path(&kb);
        assert!(!db.parent().unwrap().exists());

        let check = hold_writer_lock(&PurgeTarget::new(&db), Duration::ZERO);
        assert!(
            matches!(check, Ok(WriterCheck::NoDirectory)),
            "no directory, no lock: {check:?}"
        );

        run_with_backend_waiting(
            &FakeBackend::ok(),
            Some(home.path()),
            params(true, true),
            Duration::ZERO,
        )
        .expect("nothing to lock is no reason to stop");
        assert!(!missing.exists(), "purge must not create the directory");
        assert!(!home.path().exists(), "the config home must be deleted");
    }

    /// kuriya #645 review. A lock file that cannot be opened (here a
    /// directory in its place; in the field, permissions or a read-only
    /// filesystem) has no holder to wait for or stop, so it does not refuse
    /// the purge: purge warns and deletes as it did before it took the lock.
    #[test]
    fn a_writer_lock_that_cannot_be_taken_does_not_stop_the_purge() {
        let home = TempHome::new();
        let (_kb_root, db) = kb_outside(&home);
        let wal = with_suffix(&db, "-wal");
        std::fs::write(&db, b"main").unwrap();
        std::fs::write(&wal, b"pages").unwrap();
        std::fs::create_dir_all(with_suffix(&db, ".writer-lock")).unwrap();

        let check = hold_writer_lock(&PurgeTarget::new(&db), Duration::ZERO);
        assert!(
            matches!(check, Ok(WriterCheck::Unchecked(_))),
            "an unopenable lock file is not a holder: {check:?}"
        );

        run_with_backend_waiting(
            &FakeBackend::ok(),
            Some(home.path()),
            params(true, true),
            Duration::ZERO,
        )
        .expect("purge goes on best-effort without the lock");
        assert!(!db.exists(), "the database must be deleted");
        assert!(!wal.exists(), "the WAL must be deleted");
        assert!(!home.path().exists(), "the config home must be deleted");
    }

    /// kuriya #645 review. On a case-insensitive filesystem a link can spell
    /// its target in another case than the file's own name. Once the file is
    /// gone, resolving the link keeps that spelling, so comparing it with the
    /// resolved name called the link changed and left it. The link's contents
    /// are compared as read instead. Skipped where names are case-sensitive.
    #[test]
    fn a_database_link_spelled_in_another_case_is_still_removed() {
        let home = TempHome::new();
        let (_kb_root, db) = kb_outside(&home);
        let elsewhere = TempHome::new();
        let target = elsewhere.path().join("real.db");
        std::fs::write(&target, b"main").unwrap();
        let other_case = elsewhere.path().join("REAL.DB");
        if !other_case.exists() {
            return;
        }
        if !crate::writer_lease::file_symlink_or_skip(&other_case, &db) {
            return;
        }

        run_with_backend(&FakeBackend::ok(), Some(home.path()), params(true, true))
            .expect("purge should succeed");

        assert!(!target.exists(), "the database the link points to must go");
        assert!(
            std::fs::symlink_metadata(&db).is_err(),
            "the link must go with it"
        );
    }

    /// [`link_unchanged`], the check behind the test above, through directory
    /// links, so that it also runs where a file symlink cannot be made (a
    /// Windows junction needs no privilege). The link spells its target in
    /// another case; once the target is gone, the link still holds what it
    /// held and is unchanged. Skipped where names are case-sensitive.
    #[test]
    fn a_link_spelled_in_another_case_is_unchanged_once_its_target_is_gone() {
        use crate::writer_lease::DirLink;
        for kind in DirLink::all() {
            let root = TempHome::new();
            let real = root.path().join("real");
            std::fs::create_dir_all(&real).unwrap();
            let other_case = root.path().join("REAL");
            if !other_case.exists() {
                return;
            }
            let link = root.path().join("alias");
            if !kind.point(&link, &other_case) {
                continue;
            }
            let target = PurgeTarget::new(&link);
            assert!(target.link_target.is_some(), "{kind:?}: {target:?}");

            std::fs::remove_dir(&real).unwrap();

            assert!(link_unchanged(&target), "{kind:?}: {target:?}");
            #[cfg(windows)]
            std::fs::remove_dir(&link).unwrap();
        }
    }

    /// AGENTS.md: stderr stays ASCII. The OS words an io error in the local
    /// language, so the chain shows it by kind and code.
    #[test]
    fn an_error_chain_shows_io_errors_by_kind_and_code() {
        let io = std::io::Error::from_raw_os_error(2);
        let e = anyhow::Error::new(io).context("failed to remove x");

        let shown = ascii_error_chain(&e);

        assert!(shown.starts_with("failed to remove x: "), "{shown}");
        assert!(shown.contains("(os error 2)"), "{shown}");
        assert!(shown.is_ascii(), "{shown}");
    }

    /// kuriya #645 review. `resolved` is built from the link's contents as
    /// read for `link_target`, not from a second read of the link; a relative
    /// target is taken from the link's directory, as the writer lease takes it.
    #[test]
    fn a_relative_database_link_resolves_from_the_target_read_once() {
        let home = TempHome::new();
        let (kb_root, db) = kb_outside(&home);
        let real = kb_root.path().join("real.db");
        std::fs::write(&real, b"main").unwrap();
        if !crate::writer_lease::file_symlink_or_skip(Path::new("real.db"), &db) {
            return;
        }

        let target = PurgeTarget::new(&db);

        assert!(target.is_link, "{target:?}");
        assert_eq!(
            target.link_target.as_deref(),
            Some(Path::new("real.db")),
            "{target:?}"
        );
        assert_eq!(
            target.resolved,
            WriterLease::resolved_db_path(&real),
            "{target:?}"
        );
        assert_eq!(
            target.resolved,
            WriterLease::resolved_db_path(&db),
            "the same file the writer lease on the link locks: {target:?}"
        );
    }

    /// kuriya #645 review. A `.groove.db` that is a symlink whose contents
    /// could not be read used to be passed over without a word, since only a
    /// link with known contents was looked at. It is now named as left in
    /// place, as every file purge does not remove is.
    #[test]
    fn a_database_link_that_could_not_be_read_is_named_as_left_in_place() {
        let link = PathBuf::from("kb-root").join(".groove.db");
        let target = PurgeTarget {
            link: link.clone(),
            is_link: true,
            link_target: None,
            resolved: link.clone(),
        };

        for removed_all in [true, false] {
            match link_step(&target, removed_all) {
                LinkStep::Keep(warning) => {
                    assert!(warning.starts_with("Warning: left in place: "), "{warning}");
                    assert!(warning.contains(&link.display().to_string()), "{warning}");
                    assert!(warning.is_ascii(), "{warning}");
                }
                other => panic!("an unreadable link must be named as kept: {other:?}"),
            }
        }

        let not_a_link = PurgeTarget {
            is_link: false,
            ..target
        };
        assert_eq!(link_step(&not_a_link, true), LinkStep::NotALink);
        let readable = PurgeTarget {
            is_link: true,
            link_target: Some(PathBuf::from("real.db")),
            ..not_a_link
        };
        assert_eq!(link_step(&readable, true), LinkStep::Remove);
        assert!(matches!(link_step(&readable, false), LinkStep::Keep(_)));
    }
}
