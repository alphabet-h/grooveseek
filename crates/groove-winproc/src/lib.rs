//! Windows process identity and stop helpers shared by `groove`,
//! `groove-svc` and `groove-tray` (kuriya #650).
//!
//! Two halves:
//!
//! - [`crate::ServeRecord`]: what `groove-svc.exe` writes beside the service's
//!   `groove.toml` when it starts `groove.exe serve` — the child's pid and the
//!   time the OS created it. Plain file I/O, compiled on every OS.
// The process functions exist on Windows only, so the bullet that names them
// is written twice: a link where the item is there, plain words elsewhere
// (a link there would be broken, a bare name an unchecked one).
#![cfg_attr(
    windows,
    doc = "- On Windows, [`crate::stop_process_if_matches`] and friends: open a pid once,"
)]
#![cfg_attr(
    not(windows),
    doc = "- On Windows (not compiled into this build), functions that open a pid once,"
)]
//!   check its image file name (and, given a record, its creation time), and
//!   terminate it through that same handle, waiting for it to exit. The handle
//!   pins the process: a pid reused after `OpenProcess` cannot be reached
//!   through it, and the checks reject a pid reused before it.
//!
//! The tray used to carry the process half alone, in its own process module;
//! `groove service uninstall` needed the same thing plus the creation-time
//! check, so both now use this crate.

use serde::{Deserialize, Serialize};
use std::io::{self, Read};
use std::path::{Path, PathBuf};

/// File name of the [`ServeRecord`] in the service's config home.
pub const SERVE_RECORD_FILE: &str = "serve-pid.json";

/// The largest record [`ServeRecord::read`] accepts. A real one is under 64
/// bytes; anything this large is not one.
const MAX_RECORD_BYTES: u64 = 4096;

/// The server `groove-svc.exe` started: its pid and the time the OS created
/// that process, in `FILETIME` units (100 ns intervals since 1601-01-01 UTC),
/// read from the child's own handle right after the spawn.
///
/// The creation time is what makes the pid trustworthy later: pids are
/// reused, but a process with this pid **and** this creation time is the one
/// that was started. It is compared for equality, so no clock is involved.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServeRecord {
    pub pid: u32,
    pub created: u64,
}

impl ServeRecord {
    /// Where the record of the service whose config home is `config_home` lives.
    pub fn path_in(config_home: &Path) -> PathBuf {
        config_home.join(SERVE_RECORD_FILE)
    }

    /// Read the record at the given path. `Ok(None)` when there is no file; an
    /// `InvalidData` error when there is one that is not a record.
    pub fn read(path: &Path) -> io::Result<Option<Self>> {
        let file = match std::fs::File::open(path) {
            Ok(f) => f,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        };
        let mut bytes = Vec::new();
        file.take(MAX_RECORD_BYTES + 1).read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAX_RECORD_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "the serve record is too large to be one",
            ));
        }
        serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))
    }

    /// Write the record to the given path whole or not at all: to a temporary file
    /// beside it first, then renamed over it, so a reader never sees half a
    /// record.
    pub fn write(&self, path: &Path) -> io::Result<()> {
        let mut tmp = path.as_os_str().to_os_string();
        tmp.push(format!(".tmp-{}", std::process::id()));
        let tmp = PathBuf::from(tmp);
        let bytes = serde_json::to_vec(self).map_err(io::Error::other)?;
        std::fs::write(&tmp, bytes)?;
        std::fs::rename(&tmp, path).inspect_err(|_| {
            let _ = std::fs::remove_file(&tmp);
        })
    }
}

/// True when `image_path`'s file name is `expected_file_name`, compared the way
/// Windows compares paths (case-insensitively). The file name is what follows
/// the last `\` or `/`, on every OS, so this is tested the same everywhere.
pub fn image_file_name_matches(image_path: &str, expected_file_name: &str) -> bool {
    image_path
        .rsplit(['\\', '/'])
        .next()
        .is_some_and(|n| !n.is_empty() && n.eq_ignore_ascii_case(expected_file_name))
}

/// What a stop attempt did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopOutcome {
    /// The checked process was terminated **and observed to exit**.
    Terminated,
    /// Nothing to stop: the pid names no process, or one that had exited.
    NotRunning,
    /// The pid belongs to another program, so it was left alone.
    NotOurProcess,
    /// The pid belongs to the expected program, but not to the process that
    /// was recorded (its creation time differs): a reused pid, left alone.
    Reused,
}

#[cfg(windows)]
mod process;
#[cfg(windows)]
pub use process::{
    EXIT_WAIT, creation_time_of, stop_process_if_image_matches, stop_process_if_matches,
};

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let p = std::env::temp_dir().join(format!(
            "groove-winproc-{tag}-{}-{nanos}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn a_record_round_trips_and_leaves_no_temporary_file() {
        let dir = temp_dir("rt");
        let path = ServeRecord::path_in(&dir);
        let rec = ServeRecord {
            pid: 1234,
            created: 134_000_000_000_000_000,
        };
        rec.write(&path).unwrap();
        assert_eq!(ServeRecord::read(&path).unwrap(), Some(rec));
        let names: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, [SERVE_RECORD_FILE], "only the record is left");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_missing_record_is_none_and_a_broken_one_is_an_error() {
        let dir = temp_dir("bad");
        let path = ServeRecord::path_in(&dir);
        assert_eq!(ServeRecord::read(&path).unwrap(), None);
        std::fs::write(&path, b"{\"pid\": 1").unwrap();
        let err = ServeRecord::read(&path).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        std::fs::write(&path, vec![b' '; 5000]).unwrap();
        assert_eq!(
            ServeRecord::read(&path).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_rewrite_replaces_the_record() {
        let dir = temp_dir("rw");
        let path = ServeRecord::path_in(&dir);
        ServeRecord { pid: 1, created: 1 }.write(&path).unwrap();
        ServeRecord { pid: 2, created: 2 }.write(&path).unwrap();
        assert_eq!(
            ServeRecord::read(&path).unwrap(),
            Some(ServeRecord { pid: 2, created: 2 })
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn image_file_name_matches_ignores_case_and_directories() {
        assert!(image_file_name_matches(
            "C:\\Users\\me\\.cargo\\bin\\groove.exe",
            "groove.exe"
        ));
        assert!(image_file_name_matches("C:\\a\\GROOVE.EXE", "groove.exe"));
        assert!(!image_file_name_matches(
            "C:\\a\\groove-svc.exe",
            "groove.exe"
        ));
        assert!(!image_file_name_matches(
            "C:\\a\\groove.exe.bak",
            "groove.exe"
        ));
        assert!(!image_file_name_matches("", "groove.exe"));
    }
}
