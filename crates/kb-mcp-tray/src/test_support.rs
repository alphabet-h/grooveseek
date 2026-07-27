//! Shared pieces for this crate's unit tests.
//!
//! Declared by **both** `lib.rs` and `main.rs`: `install.rs` belongs to the
//! library target and `daemon.rs` / `process.rs` to the binary, so a helper
//! either lives in one source file that both targets compile, or gets written
//! twice. This is the former.
//!
//! Mirrors `kb-mcp/src/test_support.rs`. The two cannot be shared — `kb-mcp`
//! depends on this crate, so a dependency the other way would be a cycle.

/// A suffix that is unique across processes *and* within one process.
///
/// PID + nanos, which these tests grew independently, is not enough: `cargo
/// test` runs a binary's tests on parallel threads of a **single** process, so
/// two can read the same nanosecond, derive the same path, and delete each
/// other's files on `Drop`. `kb-mcp/tests/common/temp.rs` carries the same
/// counter, added after that collision was observed on a Windows CI runner.
pub fn unique_suffix() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    suffix_for(std::process::id(), nanos)
}

static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// The part of [`unique_suffix`] that does not read the clock, split out so the
/// counter can be tested for what it is for. Asserting that two `unique_suffix`
/// calls differ proves nothing — the real clock advances between them, so that
/// passes with the counter removed.
fn suffix_for(pid: u32, nanos: u128) -> String {
    let seq = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    format!("{pid}-{nanos}-{seq}")
}

/// [`unique_suffix`] applied to a scratch path directly under the temp dir.
pub fn unique_temp_path(prefix: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!("{prefix}-{}", unique_suffix()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The property the counter exists for, with the clock pinned so the case
    /// actually arises. Without the counter this fails.
    #[test]
    fn same_pid_and_same_nanosecond_still_differ() {
        assert_ne!(suffix_for(4242, 99), suffix_for(4242, 99));
    }

    #[test]
    fn successive_suffixes_differ() {
        assert_ne!(unique_suffix(), unique_suffix());
    }

    #[test]
    fn concurrent_suffixes_are_all_distinct() {
        let handles: Vec<_> = (0..8)
            .map(|_| std::thread::spawn(|| (0..64).map(|_| unique_suffix()).collect::<Vec<_>>()))
            .collect();
        let all: Vec<String> = handles
            .into_iter()
            .flat_map(|h| h.join().unwrap())
            .collect();
        let unique: std::collections::HashSet<&String> = all.iter().collect();
        assert_eq!(
            unique.len(),
            all.len(),
            "collision among {} suffixes",
            all.len()
        );
    }

    /// Kept in lockstep with `kb-mcp/src/test_support.rs`, which cannot be
    /// imported here.
    #[test]
    fn temp_path_carries_the_prefix_and_is_unique() {
        let p = unique_temp_path("kb-mcp-tray-probe");
        assert!(p.starts_with(std::env::temp_dir()));
        let name = p.file_name().unwrap().to_string_lossy().to_string();
        assert!(
            name.starts_with("kb-mcp-tray-probe-"),
            "unexpected name: {name}"
        );
        assert_ne!(unique_temp_path("kb-mcp-tray-probe"), p);
    }
}
