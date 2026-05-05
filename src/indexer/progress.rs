//! Progress reporting for `kb-mcp index`.
//!
//! Wraps the existing per-file `eprintln!` output behind a small structured
//! API so that we can suppress it (`--quiet`), turn it into an `indicatif`
//! progress bar (`--progress` on TTY) or emit periodic `Progress: N/M (P%)`
//! lines (`--progress` off-TTY). MCP server `rebuild_index` tool wires
//! `ProgressMode::Quiet` directly.
//!
//! Lifetime: `rebuild_index` constructs a `ProgressReporter` from caller
//! intent, then calls `start_indexing(total)` once `total` is known (after
//! source-file discovery), then `report_*` per file, then `finish` at the
//! end. The bar is constructed lazily inside `start_indexing` so that the
//! pre-loop `Backfilled ...` / `Found N source files` lines are emitted
//! through plain `eprintln!` without colliding with an active bar.

use std::sync::atomic::{AtomicU64, Ordering};

/// Caller-facing intent for progress output.
#[derive(Debug, Clone, Copy)]
pub enum ProgressMode {
    /// Existing per-file `eprintln!` (CLI default for backward compat).
    Verbose,
    /// Suppress per-file output (CLI `--quiet`, MCP server fixed).
    Quiet,
    /// `--progress` flag — TTY / non-TTY auto-detected at `start_indexing`.
    Auto,
}

/// Output reporter, owned by `rebuild_index`.
pub struct ProgressReporter {
    inner: ProgressInner,
}

enum ProgressInner {
    /// `Verbose` mode: existing per-file `eprintln!`.
    Verbose,
    /// `Quiet` mode: every `report_*` is a no-op.
    Quiet,
    /// `Auto` mode pre-`start_indexing`: not yet decided.
    AutoPending,
    /// `Auto` + TTY (decided at `start_indexing`).
    Tty(indicatif::ProgressBar),
    /// `Auto` + non-TTY (decided at `start_indexing`).
    NonTty {
        total: u64,
        step: u64,
        count: AtomicU64,
    },
}

impl ProgressReporter {
    /// Build a reporter from explicit mode (used by MCP server with `Quiet`).
    pub fn new(mode: ProgressMode) -> Self {
        let inner = match mode {
            ProgressMode::Verbose => ProgressInner::Verbose,
            ProgressMode::Quiet => ProgressInner::Quiet,
            ProgressMode::Auto => ProgressInner::AutoPending,
        };
        Self { inner }
    }

    /// CLI flag adapter. clap's `conflicts_with` ensures `(true, true)` is
    /// rejected at parse time, so this match never reaches that combination
    /// at runtime.
    pub fn from_cli_flags(quiet: bool, progress: bool) -> Self {
        match (quiet, progress) {
            (true, _) => Self::new(ProgressMode::Quiet),
            (_, true) => Self::new(ProgressMode::Auto),
            _ => Self::new(ProgressMode::Verbose),
        }
    }

    /// Initialise bar / counter once `total` is known (= after source-file
    /// discovery). `total == 0` keeps the reporter no-op for the rest of
    /// the run (= 罠 H1: empty KB の早期 no-op、bar 不構築)。
    ///
    /// Task 1 では `Auto` 経路を `Verbose` に落とす placeholder 実装。
    /// Task 2 で `NonTty`、Task 3 で `Tty` を実装して `_total` を消費する。
    pub fn start_indexing(&mut self, total: usize) {
        if total == 0 {
            return;
        }
        if matches!(self.inner, ProgressInner::AutoPending) {
            let _total = total;
            self.inner = ProgressInner::Verbose;
        }
    }

    pub fn report_indexed(&self, rel: &str, chunks: u32) {
        match &self.inner {
            ProgressInner::Verbose => {
                eprintln!("  indexed: {rel} ({chunks} chunks)");
            }
            ProgressInner::Quiet | ProgressInner::AutoPending => {}
            // Tty / NonTty は Task 2-3 で実装
            ProgressInner::Tty(_) | ProgressInner::NonTty { .. } => {}
        }
    }

    pub fn report_renamed(&self, old: &str, new: &str) {
        match &self.inner {
            ProgressInner::Verbose => {
                eprintln!("  renamed: {old} -> {new}");
            }
            ProgressInner::Quiet | ProgressInner::AutoPending => {}
            ProgressInner::Tty(_) | ProgressInner::NonTty { .. } => {}
        }
    }

    pub fn report_deleted(&self, rel: &str) {
        match &self.inner {
            ProgressInner::Verbose => {
                eprintln!("  deleted: {rel}");
            }
            ProgressInner::Quiet | ProgressInner::AutoPending => {}
            ProgressInner::Tty(_) | ProgressInner::NonTty { .. } => {}
        }
    }

    /// Tear down (clear bar, etc.). Owned consume so the caller can rely on
    /// "the reporter is done at this point".
    pub fn finish(self) {
        // Tty / NonTty cleanup は Task 2-3 で追加
        let _ = self.inner;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_from_cli_flags_default() {
        let r = ProgressReporter::from_cli_flags(false, false);
        assert!(matches!(r.inner, ProgressInner::Verbose));
    }

    #[test]
    fn test_from_cli_flags_quiet() {
        let r = ProgressReporter::from_cli_flags(true, false);
        assert!(matches!(r.inner, ProgressInner::Quiet));
    }

    #[test]
    fn test_from_cli_flags_progress() {
        let r = ProgressReporter::from_cli_flags(false, true);
        assert!(matches!(r.inner, ProgressInner::AutoPending));
    }

    #[test]
    fn test_new_quiet_explicit() {
        // MCP server 経路 (= server.rs::rebuild_index で固定)
        let r = ProgressReporter::new(ProgressMode::Quiet);
        assert!(matches!(r.inner, ProgressInner::Quiet));
    }

    #[test]
    fn test_start_indexing_zero_is_noop() {
        let mut r = ProgressReporter::new(ProgressMode::Auto);
        r.start_indexing(0);
        // total=0 なら AutoPending のまま (= Auto 解決されない)
        assert!(matches!(r.inner, ProgressInner::AutoPending));
    }

    #[test]
    fn test_quiet_report_does_not_panic() {
        // 出力 capture は subprocess test (Task 7) で行う。ここでは関数が
        // panic しないことだけ確認。
        let r = ProgressReporter::new(ProgressMode::Quiet);
        r.report_indexed("foo.md", 3);
        r.report_renamed("a.md", "b.md");
        r.report_deleted("c.md");
        r.finish();
    }
}
