//! Renaming a path in a fixture while the watcher runs.

use std::time::Duration;

/// (feature-68) `std::fs::rename` that retries a few times on
/// `PermissionDenied` only, then panics with the last error.
///
/// A directory written into a moment ago can be held open briefly by another
/// process (Windows Defender and similar scanners are the suspects; this is
/// unconfirmed), and Windows refuses to rename a directory something holds.
/// The same shape of failure, a process holding a directory and stopping its
/// rename, is trap 23 of the skill `windows-quirks`. Any other error kind
/// fails at once: retrying would only hide a real bug in the fixture.
///
/// 20 attempts 100 ms apart (about 2 s in all) is a guess at "a scanner's
/// brief look", well under the poll budgets the callers spend afterwards.
pub fn rename_retrying(from: &std::path::Path, to: &std::path::Path) {
    const ATTEMPTS: u32 = 20;
    const INTERVAL: Duration = Duration::from_millis(100);
    let mut attempt = 1;
    loop {
        match std::fs::rename(from, to) {
            Ok(()) => return,
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied && attempt < ATTEMPTS => {
                attempt += 1;
                std::thread::sleep(INTERVAL);
            }
            Err(e) => panic!(
                "rename {} -> {} failed after {attempt} attempt(s): {e}",
                from.display(),
                to.display()
            ),
        }
    }
}
