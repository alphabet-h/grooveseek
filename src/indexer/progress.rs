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

use std::io::IsTerminal;
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
    pub fn start_indexing(&mut self, total: usize) {
        if total == 0 {
            return;
        }
        if matches!(self.inner, ProgressInner::AutoPending) {
            let total_u64 = total as u64;
            let is_tty = std::io::stderr().is_terminal();
            self.inner = if is_tty {
                use indicatif::{ProgressBar, ProgressStyle};
                let bar = ProgressBar::new(total_u64);
                bar.set_style(
                    ProgressStyle::with_template(
                        "[{elapsed_precise}] [{bar:24.cyan/blue}] {pos}/{len} ({percent}%, ETA {eta}) {msg}",
                    )
                    .expect("static template")
                    .progress_chars("█▉▊▋▌▍▎▏ "),
                );
                bar.enable_steady_tick(std::time::Duration::from_millis(100));
                ProgressInner::Tty(bar)
            } else {
                let step = std::cmp::max(1u64, total_u64 / 20);
                ProgressInner::NonTty {
                    total: total_u64,
                    step,
                    count: AtomicU64::new(0),
                }
            };
        }
    }

    pub fn report_indexed(&self, rel: &str, chunks: u32) {
        match &self.inner {
            ProgressInner::Verbose => {
                eprintln!("  indexed: {rel} ({chunks} chunks)");
            }
            ProgressInner::Quiet | ProgressInner::AutoPending => {}
            ProgressInner::Tty(bar) => {
                bar.inc(1);
                bar.set_message(format!("{rel} ({chunks} chunks)"));
            }
            ProgressInner::NonTty { total, step, count } => {
                let new_count = count.fetch_add(1, Ordering::Relaxed) + 1;
                if should_emit(new_count, *total, *step) {
                    let pct = (new_count * 100) / total;
                    eprintln!("Progress: {new_count}/{total} ({pct}%)");
                }
                let _ = (rel, chunks);
            }
        }
    }

    /// Tick progress for an `Unchanged` / `Skipped` file (= incremental run
    /// で hash 一致した case)。Verbose mode は何も出さない (= 既存挙動を保つ、
    /// per-file `  indexed:` は更新時のみ)。Tty / NonTty は **必ず tick** して、
    /// 進捗 100% / bar full を保証する (= codex P1 round 1 on PR #55、
    /// incremental run で `force=false` + 多数 unchanged の場合に bar が
    /// 100% に到達せず stale 値で終わる罠)。
    pub fn report_unchanged(&self, rel: &str) {
        match &self.inner {
            ProgressInner::Verbose | ProgressInner::Quiet | ProgressInner::AutoPending => {}
            ProgressInner::Tty(bar) => {
                bar.inc(1);
                // message は updated 時のものを上書きしないよう、unchanged では設定しない。
                let _ = rel;
            }
            ProgressInner::NonTty { total, step, count } => {
                let new_count = count.fetch_add(1, Ordering::Relaxed) + 1;
                if should_emit(new_count, *total, *step) {
                    let pct = (new_count * 100) / total;
                    eprintln!("Progress: {new_count}/{total} ({pct}%)");
                }
                let _ = rel;
            }
        }
    }

    pub fn report_renamed(&self, old: &str, new: &str) {
        match &self.inner {
            ProgressInner::Verbose => {
                eprintln!("  renamed: {old} -> {new}");
            }
            ProgressInner::Tty(bar) => {
                bar.println(format!("  renamed: {old} -> {new}"));
            }
            ProgressInner::Quiet | ProgressInner::AutoPending | ProgressInner::NonTty { .. } => {
                // NonTty は per-file 進捗 = indexed のみカウント、
                // renamed / deleted は補助情報として silence。
            }
        }
    }

    pub fn report_deleted(&self, rel: &str) {
        match &self.inner {
            ProgressInner::Verbose => {
                eprintln!("  deleted: {rel}");
            }
            ProgressInner::Tty(bar) => {
                bar.println(format!("  deleted: {rel}"));
            }
            ProgressInner::Quiet | ProgressInner::AutoPending | ProgressInner::NonTty { .. } => {}
        }
    }

    /// Tear down (clear bar, etc.). Owned consume so the caller can rely on
    /// "the reporter is done at this point".
    pub fn finish(self) {
        if let ProgressInner::Tty(bar) = &self.inner {
            bar.finish_and_clear();
        }
        // Verbose / Quiet / NonTty / AutoPending は何もしない
    }
}

impl Drop for ProgressReporter {
    fn drop(&mut self) {
        // 罠 M5 (Ctrl-C / panic): finish() が呼ばれずに drop された case で
        // bar 描画を必ず clear する。Tty 以外は no-op。
        // 既に finish() で finish_and_clear 済の bar を再度 clear するのは
        // indicatif 仕様上 idempotent (= 二重 finish も safe)。
        if let ProgressInner::Tty(bar) = &self.inner {
            bar.finish_and_clear();
        }
    }
}

/// 非 TTY mode で emit を判定するヘルパ。
/// `count` は 1-based (= report_indexed が呼ばれた回数)。
/// `total == 0` のとき `start_indexing` で early return するので呼ばれない
/// 想定だが、defensive に false を返す。
fn should_emit(count: u64, total: u64, step: u64) -> bool {
    if total == 0 {
        return false;
    }
    count > 0 && (count.is_multiple_of(step) || count == total)
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

    #[test]
    fn test_should_emit_basic() {
        // total=320, step=16 (= 320/20)
        assert!(!should_emit(0, 320, 16), "count=0 must not emit");
        assert!(should_emit(16, 320, 16), "first step boundary");
        assert!(should_emit(32, 320, 16));
        assert!(!should_emit(15, 320, 16));
        assert!(!should_emit(17, 320, 16));
        assert!(should_emit(320, 320, 16), "100% always emits");
    }

    #[test]
    fn test_should_emit_small_total() {
        // total=5, step=max(1, 5/20)=1 (= 全件 emit)
        assert!(!should_emit(0, 5, 1));
        assert!(should_emit(1, 5, 1));
        assert!(should_emit(5, 5, 1));
    }

    #[test]
    fn test_should_emit_total_zero_never_called() {
        // start_indexing(0) で no-op になるため should_emit は呼ばれない前提だが、
        // defensive に呼ばれた場合の挙動も「emit しない」であることを確認
        assert!(!should_emit(0, 0, 1));
        assert!(!should_emit(1, 0, 1)); // count > total ありえないが defensive
    }

    #[test]
    fn test_nontty_report_indexed_emits_at_boundary() {
        // 内部 count を直接 inspect。emit 検証は subprocess test で行うが、
        // count increment が正しく走ることだけ確認。
        let r = ProgressReporter {
            inner: ProgressInner::NonTty {
                total: 5,
                step: 1,
                count: AtomicU64::new(0),
            },
        };
        r.report_indexed("foo.md", 3);
        if let ProgressInner::NonTty { count, .. } = &r.inner {
            assert_eq!(count.load(Ordering::Relaxed), 1);
        } else {
            panic!("expected NonTty variant");
        }
    }

    #[test]
    fn test_nontty_report_unchanged_also_ticks_count() {
        // 罠 codex P1 round 1: incremental run の unchanged file も tick されないと
        // 100% アンカーに届かない。report_unchanged が NonTty で counter を tick する
        // ことを直接検証。
        let r = ProgressReporter {
            inner: ProgressInner::NonTty {
                total: 3,
                step: 1,
                count: AtomicU64::new(0),
            },
        };
        r.report_indexed("a.md", 1);
        r.report_unchanged("b.md");
        r.report_unchanged("c.md");
        if let ProgressInner::NonTty { count, .. } = &r.inner {
            assert_eq!(
                count.load(Ordering::Relaxed),
                3,
                "report_indexed + 2x report_unchanged should advance count to 3 (= 100%)"
            );
        } else {
            panic!("expected NonTty variant");
        }
    }

    #[test]
    fn test_verbose_report_unchanged_does_not_emit() {
        // Verbose mode で report_unchanged が「  indexed: ...」を出すと regression。
        // 既存挙動 (= unchanged は何も出さない) を保つことを panic 不発で確認。
        let r = ProgressReporter::new(ProgressMode::Verbose);
        r.report_unchanged("foo.md");
        // 出力 capture は subprocess test (Task 7) で間接的に保証されている
        // (= test_index_default_emits_per_file は unchanged 行を期待しない)。
    }
}
