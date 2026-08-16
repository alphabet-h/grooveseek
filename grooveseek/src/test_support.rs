//! Shared pieces for the unit tests in `src/**`.
//!
//! `#[cfg(test)]`, so none of this is compiled into the binary or the library
//! as consumers see it.

/// A suffix that is unique across processes *and* within one process.
///
/// Every scratch path under `std::env::temp_dir()` needs one. The project does
/// not use `tempfile`, so uniqueness is ours to produce, and PID + nanos —
/// which is what most of these tests grew independently — is not enough:
/// `cargo test` runs a binary's tests on parallel threads of a **single**
/// process, so two of them can read the same nanosecond and derive the same
/// path. They then share a directory and each one's `Drop` deletes the other's
/// files, which surfaces as an unrelated test failing on a missing file.
///
/// This is not hypothetical. `tests/common/temp.rs` carries the same counter,
/// added because the collision was observed on a Windows CI runner.
///
/// The counter is process-wide, so it disambiguates the same-nanosecond case;
/// PID and nanos still separate concurrent runs and reruns.
pub fn unique_suffix() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    suffix_for(std::process::id(), nanos)
}

static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// The part of [`unique_suffix`] that does not read the clock.
///
/// Split out so the counter can be tested for what it is *for*. Calling
/// `unique_suffix` twice and asserting the results differ proves nothing: the
/// clock does advance between two adjacent calls on a normal machine, so that
/// assertion passes with the counter removed. Pinning `nanos` here reproduces
/// the same-nanosecond case directly, and fails without it.
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
    /// actually arises. Without the counter this fails; with `unique_suffix`
    /// instead of `suffix_for` it would pass either way, since the real clock
    /// advances between two adjacent calls.
    #[test]
    fn same_pid_and_same_nanosecond_still_differ() {
        let a = suffix_for(4242, 99);
        let b = suffix_for(4242, 99);
        assert_ne!(a, b, "a repeated clock reading collided");
    }

    #[test]
    fn successive_suffixes_differ() {
        assert_ne!(unique_suffix(), unique_suffix());
    }

    /// Same across threads, which is how `cargo test` actually runs them.
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

    #[test]
    fn temp_path_carries_the_prefix_and_lands_in_the_temp_dir() {
        let p = unique_temp_path("groove-probe");
        assert!(p.starts_with(std::env::temp_dir()));
        let name = p.file_name().unwrap().to_string_lossy().to_string();
        assert!(name.starts_with("groove-probe-"), "unexpected name: {name}");
        assert_ne!(unique_temp_path("groove-probe"), p);
    }
}
