//! RAII temp-directory helpers shared across integration tests.
//!
//! ## Why hand-rolled (no `tempfile` crate)
//!
//! kb-mcp's policy (CLAUDE.local.md, "運用上の気付き") is to **not** depend
//! on the `tempfile` / `tempdir` crates. We construct unique paths from
//! `env::temp_dir() + PID + UNIX_EPOCH nanos` and clean up via `Drop`.
//! This is the same pattern the seven legacy in-test definitions used —
//! this module just gives them one canonical implementation.
//!
//! ## Two layouts
//!
//! ### `TempRoot`
//! Single flat directory. Use when the test only needs one writable
//! location and does not care about kb-mcp's `kb_path.parent()` DB
//! placement convention. Direct replacement for the historical
//! `TempDir` (`tests/config_discovery.rs`) and the validate-cli style
//! `TempKb` whose root and kb were the same directory.
//!
//! ### `TempKbLayout`
//! Two-level layout: `root/` (cleanup target) plus `root/kb/` (the
//! `kb_path` you pass to `kb-mcp`). kb-mcp's runtime places the SQLite
//! file at `kb_path.parent()/.kb-mcp.db`, so when the test wants the
//! DB to live inside the temp tree (so `Drop` reaps it), the kb must
//! sit one level deeper than the cleanup root. Direct replacement for
//! the eval-cli style helper.
//!
//! Both types are `Drop`-cleaned best-effort; cleanup failures are
//! intentionally swallowed (a test panicking during the run should
//! not be masked by a noisy "could not unlink scratch dir" error).

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// Process-wide counter that disambiguates two `TempRoot::new` calls
/// that land in the same nanosecond (rare but observed on Windows
/// CI runners). Combined with PID + UNIX nanos this gives us a unique
/// path key per call without external crates.
static COUNTER: AtomicU64 = AtomicU64::new(0);

fn unique_suffix() -> String {
    let pid = std::process::id();
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{pid}-{nanos}-{seq}")
}

/// Single flat temp directory. Drop removes the entire tree.
///
/// Replacement target: the `TempDir` in `tests/config_discovery.rs`
/// and the flat `TempKb` in `tests/validate_cli.rs`. New tests
/// preferring this should write `mod common; use common::temp::TempRoot;`.
pub struct TempRoot {
    path: PathBuf,
}

impl TempRoot {
    /// Create a fresh temp directory with the given prefix. Panics on
    /// `create_dir_all` failure — the only sensible recovery in a test
    /// is to abort, which is what an unwrap does anyway.
    pub fn new(prefix: &str) -> Self {
        let path = std::env::temp_dir().join(format!("{prefix}-{}", unique_suffix()));
        fs::create_dir_all(&path).expect("TempRoot::new: failed to create temp directory");
        Self { path }
    }

    /// Absolute path of the temp directory.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Write `content` to `path()/rel`, creating any missing parents.
    /// Panics on I/O failure (test convention).
    pub fn write(&self, rel: &str, content: &str) {
        let full = self.path.join(rel);
        if let Some(parent) = full.parent() {
            fs::create_dir_all(parent)
                .expect("TempRoot::write: failed to create parent directories");
        }
        fs::write(&full, content).expect("TempRoot::write: failed to write file");
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        // best-effort cleanup; ignore errors so a panicking test is not
        // shadowed by a "permission denied during cleanup" diagnostic.
        let _ = fs::remove_dir_all(&self.path);
    }
}

/// Two-level layout: `root/` is the cleanup target, `root/kb/` is the
/// `kb_path` to pass to `kb-mcp`.
///
/// Use this when the test wants kb-mcp's `.kb-mcp.db` (which lands at
/// `kb_path.parent()`) to fall *inside* the cleanup root, so the
/// database file is reaped on `Drop` along with the `kb` itself.
///
/// Replacement target: the layered `TempKb` in `tests/eval_cli.rs`.
pub struct TempKbLayout {
    root: PathBuf,
    kb: PathBuf,
}

impl TempKbLayout {
    /// Create `<temp>/root-<unique>/kb/`.
    pub fn new(prefix: &str) -> Self {
        let root = std::env::temp_dir().join(format!("{prefix}-{}", unique_suffix()));
        let kb = root.join("kb");
        fs::create_dir_all(&kb).expect("TempKbLayout::new: failed to create kb directory");
        Self { root, kb }
    }

    /// Returns the path to pass as `--kb-path` to `kb-mcp`.
    pub fn kb(&self) -> &Path {
        &self.kb
    }

    /// Absolute path of the cleanup root (= `kb().parent()`).
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Write `content` to `kb()/rel`, creating parents.
    pub fn write(&self, rel: &str, content: &str) {
        let full = self.kb.join(rel);
        if let Some(parent) = full.parent() {
            fs::create_dir_all(parent)
                .expect("TempKbLayout::write: failed to create parent directories");
        }
        fs::write(&full, content).expect("TempKbLayout::write: failed to write file");
    }
}

impl Drop for TempKbLayout {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

// ---------------------------------------------------------------------------
// Tests for the helpers themselves.
// ---------------------------------------------------------------------------
//
// These run as integration tests of the helpers (not of kb-mcp), so we
// keep them in this same module rather than gating them behind
// `#[cfg(test)]` (which doesn't apply inside an integration-test crate).
// They fire whenever any consumer of `mod common;` is exercised.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_temproot_unique_paths_in_same_thread() {
        let a = TempRoot::new("kbmcp-test-temproot-unique");
        let b = TempRoot::new("kbmcp-test-temproot-unique");
        assert_ne!(a.path(), b.path(), "two TempRoots must not collide");
    }

    #[test]
    fn test_temproot_write_creates_parents() {
        let t = TempRoot::new("kbmcp-test-temproot-nested");
        t.write("a/b/c.md", "hello");
        assert_eq!(
            std::fs::read_to_string(t.path().join("a/b/c.md")).unwrap(),
            "hello"
        );
    }

    #[test]
    fn test_temproot_drop_removes_tree() {
        let captured;
        {
            let t = TempRoot::new("kbmcp-test-temproot-drop");
            t.write("x.md", "y");
            captured = t.path().to_path_buf();
            assert!(captured.exists(), "must exist while alive");
        }
        // After Drop, the tree should be gone (best-effort: assert eventually).
        assert!(!captured.exists(), "TempRoot Drop must remove the tree");
    }

    #[test]
    fn test_tempkblayout_kb_is_under_root() {
        let t = TempKbLayout::new("kbmcp-test-layout");
        assert!(t.kb().starts_with(t.root()));
        assert_eq!(t.kb().parent().unwrap(), t.root());
        assert!(t.kb().exists());
    }

    #[test]
    fn test_tempkblayout_drop_removes_root() {
        let captured;
        {
            let t = TempKbLayout::new("kbmcp-test-layout-drop");
            captured = t.root().to_path_buf();
            assert!(t.kb().exists());
        }
        assert!(
            !captured.exists(),
            "TempKbLayout Drop must remove the entire root (db sibling included)"
        );
    }
}
