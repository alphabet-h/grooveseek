//! Progress reporting for `groove index`.
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
//!
//! (v1.14.0+) Between
//! [`crate::indexer::progress::ProgressReporter::start_indexing`] and the
//! first per-document call,
//! [`crate::indexer::progress::ProgressReporter::report_scanned`] is called once
//! for every file the scan visits. A run stopped through a
//! [`crate::indexer::progress::CancelToken`] ends with
//! [`crate::indexer::progress::ProgressReporter::finish_cancelled`] instead of
//! [`crate::indexer::progress::ProgressReporter::finish`].
//!
//! (v1.13.0+) One more destination, which `groove` itself never selects: a
//! reporter built by
//! [`crate::indexer::progress::ProgressReporter::with_callback`] hands every
//! step to a closure as a [`crate::indexer::progress::ProgressEvent`] rather
//! than writing it, so an application that embeds this crate draws its own
//! progress from values instead of scraping lines. The command line has no
//! flag for it.
//!
//! What that silences is the **reporter**, and only the reporter.
//! [`crate::indexer::rebuild_index`] writes its own diagnostics with
//! `eprintln!` whatever reporter it was handed — the scan-time `Skipping ...`
//! warnings, the `Found N source files` line, the backfill line and the
//! summary lines — so an embedding application whose stderr must stay quiet
//! has to capture or redirect it.

use std::io::IsTerminal;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

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

/// (v1.14.0+) A flag an application sets to stop an indexing run.
///
/// Hand a clone to [`ProgressReporter::with_cancel`] and keep one; calling
/// [`CancelToken::cancel`] on either, from any thread, is seen by
/// [`crate::indexer::rebuild_index`] the next time it looks. It looks after
/// each scanned file, before it looks for renames to apply, before each
/// document, and once after the last document, before the deletion sweep.
/// When it sees the flag it stops there, a callback reporter receives
/// [`ProgressEvent::Cancelled`] in place of [`ProgressEvent::Finished`], and
/// the call returns `Ok` with [`crate::indexer::IndexResult::cancelled`] set.
/// What the index looks like afterwards is described on that field.
///
/// Setting the flag does not stop the run at once:
///
/// - The segment before the first of those points is not interruptible. It
///   holds the steps [`crate::indexer::rebuild_index`] runs before the scan:
///   on a `force` run the provider probe (with its retries), otherwise the
///   full-text and quality backfills; then the context-mode reset, the
///   declared-field and chunk-policy bookkeeping, and the directory walk that
///   collects the source files. The stop delay includes whatever those take.
///   The bound below covers the embedding retry only, not these steps.
/// - The longest stretch between two of those points is one document's
///   embedding. With `provider = "openai-compatible"` that includes the
///   retries of every batch. A rough bound for one batch is
///   `(max_retries + 1) * timeout_seconds + max_retries * 75` seconds, taking
///   every wait at its largest possible value. Adding up the bound of each
///   retry's own wait instead (60 seconds while `Retry-After` can set it, 75
///   seconds from the seventh retry on) gives 420 seconds with the defaults
///   and `11 * timeout_seconds + 660` seconds at `max_retries = 10`. Both are
///   estimates from the constants, per batch; multiply by the number of
///   batches the document needs. FastEmbed has no retry loop.
/// - A document whose move requires it to be parsed again (a rename across
///   parsers, or any rename under the static context mode) is re-parsed
///   before the run looks at the flag, so the moved document does not keep
///   what its old path's parser wrote. Such documents are processed first,
///   so the wait is at most that many documents.
/// - Once the deletion sweep has begun the flag is too late: the run
///   completes, reports [`ProgressEvent::Finished`], and returns with
///   `false` in [`crate::indexer::IndexResult::cancelled`].
///
/// A set flag stays set; there is no `reset`. Resume by starting a new run
/// with a new token: documents the stopped run committed are skipped by
/// their hash.
///
/// The flag is stored and loaded with `Ordering::Relaxed`. It carries no data
/// of its own -- the run reads nothing that the thread setting the flag wrote
/// before setting it -- so all the run needs is to see the store eventually,
/// which any ordering provides.
#[derive(Debug, Clone, Default)]
pub struct CancelToken(Arc<AtomicBool>);

impl CancelToken {
    /// A token that is not set.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the flag. Calling it again, or from another thread, is harmless.
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Relaxed);
    }

    /// Whether [`CancelToken::cancel`] has been called on this token or on a
    /// clone of it.
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }
}

/// One step of an indexing run, as a value.
///
/// Handed to the closure a [`ProgressReporter::with_callback`] reporter was
/// built with. The `&str` fields borrow from the caller's loop, so a consumer
/// that keeps an event past the call has to copy them.
///
/// (v1.14.0+) One run delivers, in this order: [`ProgressEvent::Started`],
/// one [`ProgressEvent::Scanning`] per scanned file, the
/// [`ProgressEvent::Renamed`] events, one [`ProgressEvent::Indexed`] or
/// [`ProgressEvent::Unchanged`] per document, the [`ProgressEvent::Deleted`]
/// events, and at most one terminal event, [`ProgressEvent::Finished`] or
/// [`ProgressEvent::Cancelled`]. A stopped run's sequence ends in the phase it
/// stopped in, so [`ProgressEvent::Deleted`] never comes before
/// [`ProgressEvent::Cancelled`]. A run that returns `Err` or panics delivers
/// no terminal event (see [`ProgressEvent::Finished`]). Documents whose move
/// forces a re-parse come first, then the rest, each group in the order the
/// walk found them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProgressEvent<'a> {
    /// `total` source files were discovered. Emitted once, from
    /// [`ProgressReporter::start_indexing`], **including when `total` is 0** —
    /// the other modes skip their lazy init there, but a consumer still has to
    /// be told there is nothing to index.
    ///
    /// Once really means once: [`ProgressReporter::start_indexing`] is `pub`,
    /// and a second call on the same reporter is ignored outright — no further
    /// [`ProgressEvent::Started`], and the `total` every later event carries
    /// stays the one this event announced.
    Started { total: usize },
    /// (v1.14.0+) The scan visited one more file. Emitted once per file by
    /// [`ProgressReporter::report_scanned`], after [`ProgressEvent::Started`]
    /// and before every other event, and never when
    /// [`ProgressEvent::Started::total`] is 0. This event's
    /// [`ProgressEvent::Scanning::total`] is the number
    /// [`ProgressEvent::Started`] carried.
    ///
    /// `done` counts every file the scan looked at, **including** the ones it
    /// declined (over the size cap, not stat-able, unreadable, a hard link),
    /// so it reaches the total when the scan ends. That is where it differs from
    /// the `done` of [`ProgressEvent::Indexed`] and
    /// [`ProgressEvent::Unchanged`], which counts documents processed and so
    /// never counts a declined file: the scan reports files seen, the loop
    /// after it reports documents. The two counts are separate, and the
    /// document count starts again from 1.
    Scanning { done: usize, total: usize },
    /// A file was parsed and embedded. `done` counts the files reported so
    /// far, indexed and unchanged together, and `total` is the number
    /// [`ProgressEvent::Started`] carried.
    ///
    /// `done` can stop short of `total`: the scan declines a file that is over
    /// the size cap, cannot be stat'd, or is a hard link, and such a file
    /// reaches neither of the document calls,
    /// [`ProgressReporter::report_indexed`] and
    /// [`ProgressReporter::report_unchanged`], which are the two that advance
    /// `done`. (v1.14.0+) It does reach [`ProgressReporter::report_scanned`],
    /// which counts it in [`ProgressEvent::Scanning`] instead. That is the
    /// same anchor the non-TTY `Progress: N/M` lines already report, not
    /// something this path adds, so a consumer must not read a `done` short
    /// of the total at [`ProgressEvent::Finished`] or
    /// [`ProgressEvent::Cancelled`] as a failure.
    Indexed {
        rel: &'a str,
        chunks: u32,
        done: usize,
        total: usize,
    },
    /// A file was left as it was — its content hash still matched, it was
    /// skipped or refused, or only its metadata was refreshed. Advances `done`
    /// exactly like [`ProgressEvent::Indexed`] does, because
    /// [`crate::indexer::rebuild_index`] reports one or the other per file and
    /// never both.
    Unchanged {
        rel: &'a str,
        done: usize,
        total: usize,
    },
    /// A document moved. Reported outside the per-file loop, so it does not
    /// advance `done`.
    Renamed { old: &'a str, new: &'a str },
    /// A document's row was removed because the file is gone. Reported outside
    /// the per-file loop, so it does not advance `done`.
    Deleted { rel: &'a str },
    /// The run reached its end. Emitted **only** from
    /// [`ProgressReporter::finish`]: that method consumes the reporter, so
    /// `Drop` runs right behind it and emitting from both would deliver two.
    /// A reporter dropped without [`ProgressReporter::finish`] emits no
    /// [`ProgressEvent::Finished`], and how the consumer hears that the run
    /// ended depends on why: an early `?` return from
    /// [`crate::indexer::rebuild_index`] reports it through the `Err` that
    /// call returns, while a panic — including one
    /// raised by the callback itself — unwinds straight out of the call, so
    /// there is no `Err` either. See [`ProgressReporter::with_callback`].
    Finished,
    /// (v1.14.0+) The run stopped early because the reporter's
    /// [`CancelToken`] was set. Emitted **only** from
    /// [`ProgressReporter::finish_cancelled`], in place of
    /// [`ProgressEvent::Finished`], and nothing follows it.
    ///
    /// `done` counts the documents reported ([`ProgressEvent::Indexed`] plus
    /// [`ProgressEvent::Unchanged`]) wherever the run stopped -- never scanned
    /// files, so a run stopped during or right after the scan reports 0 --
    /// and [`ProgressEvent::Cancelled::total`] is the number
    /// [`ProgressEvent::Started`] carried. A `done` short of the total does
    /// not mean documents were left: a run stopped after
    /// its last document, with only the deletion sweep to go, still falls
    /// short by the files the scan declined. How to draw either number, as a
    /// count or as a share, is the consumer's choice.
    Cancelled { done: usize, total: usize },
}

/// What [`ProgressReporter::with_callback`] takes.
///
/// An alias so the embedding application can name the type in its own fields
/// and signatures, and so this crate spells the long form once.
///
/// `Send` because the reporter is moved onto a worker thread. It is not
/// `Sync` — `dyn Fn + Send` is not — and it does not need to be:
/// [`ProgressReporter`] is handed to [`crate::indexer::rebuild_index`] by
/// value, and every `report_*` call is made from that one thread.
///
/// That missing `Sync` does not stay inside this alias. A
/// [`ProgressReporter`] can hold one of these, and auto traits are decided
/// per type rather than per value, so the struct as a whole is `Send` and not
/// `Sync` — see [`ProgressReporter::with_callback`].
pub type ProgressCallback = Box<dyn Fn(ProgressEvent<'_>) + Send>;

/// Output reporter, owned by `rebuild_index`.
pub struct ProgressReporter {
    inner: ProgressInner,
    /// (v1.14.0+) Set by [`ProgressReporter::with_cancel`]; `None` means the
    /// run cannot be stopped this way. Beside `inner` rather than inside it,
    /// because it has to work in every mode.
    cancel: Option<CancelToken>,
    /// (v1.14.0+) Files [`ProgressReporter::report_scanned`] has counted. Kept
    /// apart from the document counters inside `inner` (`count`), which the
    /// scan must not advance: the non-TTY `Progress: N/M` lines and the
    /// callback's `done` count documents.
    scanned: AtomicU64,
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
    /// Built by [`ProgressReporter::with_callback`]: every `report_*` becomes
    /// a [`ProgressEvent`] handed to `f` rather than a line on stderr. Only
    /// the reporter's own output is replaced — see that constructor for what
    /// [`crate::indexer::rebuild_index`] keeps writing there.
    /// `total` stays 0 until [`ProgressReporter::start_indexing`] supplies it;
    /// `count` carries `done` for the same reason [`ProgressInner::NonTty`]'s
    /// does — `report_*` take `&self`.
    ///
    /// `started` makes that first call the only one that counts, the way
    /// [`ProgressInner::AutoPending`] is spent once it resolves. A plain
    /// `bool` rather than an `AtomicBool`, because
    /// [`ProgressReporter::start_indexing`] is the one method here that takes
    /// `&mut self`.
    Callback {
        f: ProgressCallback,
        total: usize,
        count: AtomicU64,
        started: bool,
    },
}

impl ProgressReporter {
    /// The one place a reporter is assembled from its mode, so a field added to
    /// [`ProgressReporter`] is initialised once.
    fn from_inner(inner: ProgressInner) -> Self {
        Self {
            inner,
            cancel: None,
            scanned: AtomicU64::new(0),
        }
    }

    /// Build a reporter from explicit mode (used by MCP server with `Quiet`).
    pub fn new(mode: ProgressMode) -> Self {
        let inner = match mode {
            ProgressMode::Verbose => ProgressInner::Verbose,
            ProgressMode::Quiet => ProgressInner::Quiet,
            ProgressMode::Auto => ProgressInner::AutoPending,
        };
        Self::from_inner(inner)
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

    /// Build a reporter that hands every step to `f` instead of writing the
    /// reporter's per-file and progress output to stderr.
    ///
    /// `groove` itself never takes this path — no CLI flag and no MCP tool
    /// selects it. It exists for an application that embeds this crate and
    /// draws its own progress; see [`ProgressEvent`] for what arrives and
    /// when.
    ///
    /// What the callback replaces is the **reporter's** output, and only
    /// that. [`crate::indexer::rebuild_index`] writes its own diagnostics with
    /// `eprintln!` whatever reporter it was handed, and they *interleave* with
    /// the events rather than bracketing them: the backfill and
    /// `Found N source files` lines come before [`ProgressEvent::Started`],
    /// the `Skipping ...` warnings after it while the scan runs, and the
    /// summary lines after the per-file loop but still ahead of any
    /// [`ProgressEvent::Deleted`] and of [`ProgressEvent::Finished`]. So an
    /// embedding application whose stderr must stay quiet has to capture or
    /// redirect it, and one that interleaves the two streams cannot assume a
    /// diagnostic it sees belongs to the event it saw last.
    ///
    /// (v1.14.0+) Two more things follow from that. The scan's `Skipping ...`
    /// warnings arrive between [`ProgressEvent::Scanning`] events, each one
    /// just before the [`ProgressEvent::Scanning`] of the file it names. And a
    /// run stopped through a [`CancelToken`] writes no summary lines: it ends
    /// at [`ProgressEvent::Cancelled`], sometimes after a warning about the
    /// declared-field set that stays true after the stop.
    ///
    /// # Panics
    ///
    /// Never on its own, but `f` must not panic. A panic inside the callback
    /// unwinds out through whichever `report_*` or
    /// [`ProgressReporter::finish`] called it and straight out of
    /// [`crate::indexer::rebuild_index`], like any other panic: the caller
    /// gets **neither** a [`ProgressEvent::Finished`] **nor** an `Err`, and
    /// under `panic = "abort"` the process ends there. Catch inside the
    /// closure if the consumer's own work can fail.
    ///
    /// The counter starts at zero and `total` stays zero until
    /// [`ProgressReporter::start_indexing`] supplies it, which is the same
    /// ordering the bar-building modes already rely on.
    ///
    /// # Examples
    ///
    /// ```
    /// use grooveseek::indexer::progress::{ProgressEvent, ProgressReporter};
    /// use std::sync::{Arc, Mutex};
    ///
    /// let log = Arc::new(Mutex::new(Vec::new()));
    /// let sink = Arc::clone(&log);
    /// let mut reporter = ProgressReporter::with_callback(Box::new(move |ev| {
    ///     if let ProgressEvent::Indexed { rel, done, total, .. } = ev {
    ///         sink.lock().unwrap().push(format!("{rel} {done}/{total}"));
    ///     }
    /// }));
    /// reporter.start_indexing(2);
    /// reporter.report_indexed("a.md", 1);
    /// reporter.finish();
    ///
    /// assert_eq!(*log.lock().unwrap(), ["a.md 1/2"]);
    /// ```
    ///
    /// # Threading
    ///
    /// [`ProgressReporter`] is `Send` but **not** `Sync`, and this
    /// constructor is why: the reporter can hold a [`ProgressCallback`],
    /// which is not `Sync`, and auto traits are decided per type rather than
    /// per value. So the bound is missing from *every* reporter, including
    /// one built by [`ProgressReporter::new`] that holds no closure at all.
    ///
    /// That is enough for how the reporter is used — moved whole onto the
    /// thread that runs the indexing, then handed to
    /// [`crate::indexer::rebuild_index`] by value, which reports from that
    /// one thread. What it rules out is sharing: a reporter cannot be placed
    /// behind an `Arc` and reported to from several threads at once. Put a
    /// `Mutex` around it if that is ever wanted.
    pub fn with_callback(f: ProgressCallback) -> Self {
        Self::from_inner(ProgressInner::Callback {
            f,
            total: 0,
            count: AtomicU64::new(0),
            started: false,
        })
    }

    /// Initialise bar / counter once `total` is known (= after source-file
    /// discovery). `total == 0` keeps the reporter no-op for the rest of
    /// the run (= 罠 H1: empty KB の早期 no-op、bar 不構築)。
    /// The callback mode created by [`ProgressReporter::with_callback`] is the
    /// one exception — see the comment in the body.
    pub fn start_indexing(&mut self, total: usize) {
        // Callback is decided at construction, so it has no lazy init to run
        // and no reason to honour the `total == 0` early return below: a
        // consumer drawing its own progress has to be told that the knowledge
        // base is empty, and `Started { total: 0 }` is how it hears it.
        //
        // The first call is the only one that counts. This method is `pub`, so
        // a second call is reachable from outside; letting it through would
        // emit a second `Started` against a doc that promises one, and -- worse
        // -- rewrite `total` while leaving `count` where it was, so a later
        // `done` could walk past the `total` it is reported against. `Auto`
        // already spends itself this way (it leaves `AutoPending` on the first
        // call and `matches!` fails afterwards); this is the same guard, made
        // explicit because the Callback arm has no state machine to lean on.
        if let ProgressInner::Callback {
            f,
            total: known,
            started,
            ..
        } = &mut self.inner
        {
            if *started {
                return;
            }
            *started = true;
            *known = total;
            f(ProgressEvent::Started { total });
            return;
        }
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
                    // ASCII, because indicatif draws this to stderr and
                    // AGENTS.md keeps stderr readable on a CP932 console. The
                    // bar used to be drawn with eighth-blocks, which is the one
                    // place in this program where the rule was broken by a
                    // library's rendering rather than by a message
                    // (codex P2 on PR #213).
                    .progress_chars("=>-"),
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
            ProgressInner::Callback {
                f, total, count, ..
            } => {
                let done = tick_done(count);
                f(ProgressEvent::Indexed {
                    rel,
                    chunks,
                    done,
                    total: *total,
                });
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
            ProgressInner::Callback {
                f, total, count, ..
            } => {
                let done = tick_done(count);
                f(ProgressEvent::Unchanged {
                    rel,
                    done,
                    total: *total,
                });
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
            ProgressInner::Callback { f, .. } => {
                f(ProgressEvent::Renamed { old, new });
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
            ProgressInner::Callback { f, .. } => {
                f(ProgressEvent::Deleted { rel });
            }
        }
    }

    /// (feature-62) One line saying what the run is about to do, written the way this reporter
    /// writes its progress: through the bar under Tty, so the bar is not torn; to stderr under
    /// Verbose and the non-TTY modes; not at all under Quiet, whose `report_*` are no-ops and
    /// whose `--quiet` promises the start, `Found` and `Done in` lines only, nor under Callback,
    /// which turns output into [`ProgressEvent`]s and gains no event for this.
    pub(crate) fn announce(&self, line: &str) {
        match &self.inner {
            ProgressInner::Verbose | ProgressInner::AutoPending | ProgressInner::NonTty { .. } => {
                eprintln!("{line}");
            }
            ProgressInner::Tty(bar) => bar.println(line),
            ProgressInner::Quiet | ProgressInner::Callback { .. } => {}
        }
    }

    /// (v1.14.0+) Let `token` stop the run this reporter is handed to. Works
    /// on a reporter of any mode, whether built by [`ProgressReporter::new`],
    /// [`ProgressReporter::from_cli_flags`] or
    /// [`ProgressReporter::with_callback`]; calling it again replaces the
    /// earlier token. Keep a clone of `token` and call [`CancelToken::cancel`]
    /// on it -- see [`CancelToken`] for where
    /// [`crate::indexer::rebuild_index`] looks and how long that can take.
    pub fn with_cancel(mut self, token: CancelToken) -> Self {
        self.cancel = Some(token);
        self
    }

    /// (v1.14.0+) Whether this reporter's [`CancelToken`] is set. `false` for
    /// a reporter that was given none, which is every reporter `groove`
    /// itself builds.
    pub fn is_cancelled(&self) -> bool {
        self.cancel.as_ref().is_some_and(CancelToken::is_cancelled)
    }

    /// (v1.14.0+) One file of the scan that hashes every source file before
    /// the per-document loop, whether the scan kept it or declined it.
    /// Counted apart from the documents: see [`ProgressEvent::Scanning`].
    ///
    /// Two modes show it. Callback emits [`ProgressEvent::Scanning`]. Tty
    /// writes `scanning N/M` into the bar's message and leaves the bar where
    /// it is, because the same bar then draws the documents, and clears the
    /// message once the scan has visited every file. Non-TTY `--progress` stays
    /// silent and leaves its `Progress: N/M` count alone, since that counts
    /// documents; Verbose and Quiet say nothing.
    pub fn report_scanned(&self) {
        let done = tick_done(&self.scanned);
        match &self.inner {
            ProgressInner::Tty(bar) => {
                let total = bar.length().unwrap_or(0);
                if done as u64 >= total {
                    bar.set_message("");
                } else {
                    bar.set_message(format!("scanning {done}/{total}"));
                }
            }
            ProgressInner::Callback { f, total, .. } => {
                f(ProgressEvent::Scanning {
                    done,
                    total: *total,
                });
            }
            ProgressInner::Verbose
            | ProgressInner::Quiet
            | ProgressInner::AutoPending
            | ProgressInner::NonTty { .. } => {}
        }
    }

    /// Tear down (clear bar, emit [`ProgressEvent::Finished`], etc.). Owned
    /// consume so the caller can rely on "the reporter is done at this point".
    ///
    /// This is the **only** place [`ProgressEvent::Finished`] is emitted.
    /// `self` is consumed here, so `Drop` runs the instant this returns; a
    /// [`ProgressEvent::Finished`] from both would reach the consumer twice.
    pub fn finish(self) {
        match &self.inner {
            ProgressInner::Tty(bar) => bar.finish_and_clear(),
            ProgressInner::Callback { f, .. } => f(ProgressEvent::Finished),
            ProgressInner::Verbose
            | ProgressInner::Quiet
            | ProgressInner::AutoPending
            | ProgressInner::NonTty { .. } => {}
        }
    }

    /// (v1.14.0+) Tear down a run that stopped because its [`CancelToken`]
    /// was set: the counterpart of [`ProgressReporter::finish`], and the only
    /// place [`ProgressEvent::Cancelled`] is emitted. It consumes the reporter
    /// for the same reason [`ProgressReporter::finish`] does -- `Drop` runs
    /// right behind it and emits neither terminal event -- so a run ends in
    /// [`ProgressEvent::Finished`] or [`ProgressEvent::Cancelled`], never
    /// both. Tty clears its bar; the other modes have nothing to tear down.
    pub fn finish_cancelled(self) {
        match &self.inner {
            ProgressInner::Tty(bar) => bar.finish_and_clear(),
            ProgressInner::Callback {
                f, total, count, ..
            } => {
                let done = saturating_usize(count.load(Ordering::Relaxed));
                f(ProgressEvent::Cancelled {
                    done,
                    total: *total,
                });
            }
            ProgressInner::Verbose
            | ProgressInner::Quiet
            | ProgressInner::AutoPending
            | ProgressInner::NonTty { .. } => {}
        }
    }
}

impl Drop for ProgressReporter {
    fn drop(&mut self) {
        // 罠 M5 (Ctrl-C / panic): finish() が呼ばれずに drop された case で
        // bar 描画を必ず clear する。Tty 以外は no-op。
        // 既に finish() で finish_and_clear 済の bar を再度 clear するのは
        // indicatif 仕様上 idempotent (= 二重 finish も safe)。
        //
        // `Callback` is deliberately absent, and that is not the same trade.
        // `finish(self)` consumes the reporter, so this runs immediately after
        // it -- a `Finished` emitted here would be the second one, and a
        // closure is not idempotent the way clearing a bar is. The cost is
        // that a reporter dropped without `finish` emits no `Finished` at all.
        // On an early `?` return the consumer learns the run ended from the
        // `Err` `crate::indexer::rebuild_index` returns; on an unwind -- a
        // panic anywhere in that call, the callback included -- there is no
        // `Err` either, and the panic is what reports the end.
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

/// Advance a counter by one and return the new value the way the events spell it. The
/// callback's document counter and the scan's `scanned` counter both go through it.
///
/// The counter is an `AtomicU64` to match [`ProgressInner::NonTty`]'s, while
/// the event field is `usize` because `total` arrives as one. Saturating rather
/// than `as`, so a 32-bit target cannot wrap a large count into a small number
/// and hand a consumer a progress bar that walks backwards.
fn tick_done(count: &AtomicU64) -> usize {
    saturating_usize(count.fetch_add(1, Ordering::Relaxed) + 1)
}

/// `u64` counter to the `usize` the events carry, saturating instead of
/// wrapping. The one place this conversion is decided: [`tick_done`] and
/// [`ProgressReporter::finish_cancelled`] both go through it.
fn saturating_usize(n: u64) -> usize {
    usize::try_from(n).unwrap_or(usize::MAX)
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
            cancel: None,
            scanned: AtomicU64::new(0),
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
            cancel: None,
            scanned: AtomicU64::new(0),
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

    // -----------------------------------------------------------------------
    // Callback mode (desktop-v0 PR 1)
    // -----------------------------------------------------------------------

    use std::sync::{Arc, Mutex};

    /// What [`recorder`] hands back: every event, flattened to one owned line.
    ///
    /// An alias so the tuple below reads as what it is, instead of spelling
    /// the nested type out at both ends.
    type EventLog = Arc<Mutex<Vec<String>>>;

    /// A callback that writes each event to a log, plus the log itself.
    ///
    /// The events borrow their `&str` fields from the caller's stack, so
    /// nothing can be stored as it arrives; one owned line per event keeps the
    /// assertions readable as the sequence a consumer would actually see.
    fn recorder() -> (EventLog, ProgressCallback) {
        let log: EventLog = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&log);
        let f: ProgressCallback = Box::new(move |ev| {
            let line = match ev {
                ProgressEvent::Started { total } => format!("started:{total}"),
                ProgressEvent::Indexed {
                    rel,
                    chunks,
                    done,
                    total,
                } => format!("indexed:{rel}:{chunks}:{done}/{total}"),
                ProgressEvent::Unchanged { rel, done, total } => {
                    format!("unchanged:{rel}:{done}/{total}")
                }
                ProgressEvent::Renamed { old, new } => format!("renamed:{old}->{new}"),
                ProgressEvent::Deleted { rel } => format!("deleted:{rel}"),
                ProgressEvent::Finished => "finished".to_string(),
                ProgressEvent::Scanning { done, total } => format!("scanning:{done}/{total}"),
                ProgressEvent::Cancelled { done, total } => format!("cancelled:{done}/{total}"),
            };
            sink.lock().expect("recorder mutex").push(line);
        });
        (log, f)
    }

    /// Read the log back without consuming it.
    fn recorded(log: &EventLog) -> Vec<String> {
        log.lock().expect("recorder mutex").clone()
    }

    #[test]
    fn test_callback_reports_started_indexed_unchanged_then_finished() {
        let (log, f) = recorder();
        let mut r = ProgressReporter::with_callback(f);
        r.start_indexing(3);
        r.report_indexed("a.md", 2);
        r.report_unchanged("b.md");
        r.report_indexed("c.md", 1);
        r.finish();

        let got = recorded(&log);
        assert_eq!(
            got,
            vec![
                "started:3",
                "indexed:a.md:2:1/3",
                "unchanged:b.md:2/3",
                "indexed:c.md:1:3/3",
                "finished",
            ],
            "the consumer must see the run in order, with `done` reaching `total`"
        );
        // `finish` consumes `self`, so `Drop` runs immediately after it. A
        // `Finished` emitted from both would appear twice here.
        assert_eq!(
            got.iter().filter(|line| *line == "finished").count(),
            1,
            "Finished must be emitted by finish() only, never again from Drop"
        );
    }

    #[test]
    fn test_callback_forwards_renamed_and_deleted_without_advancing_done() {
        // Both are reported outside `rebuild_index`'s per-file loop -- its
        // rename pass and its deletion pass -- so counting them would push
        // `done` past the number of files the loop actually walks.
        let (log, f) = recorder();
        let mut r = ProgressReporter::with_callback(f);
        r.start_indexing(1);
        r.report_renamed("old.md", "new.md");
        r.report_deleted("gone.md");
        r.report_indexed("only.md", 4);
        r.finish();

        assert_eq!(
            recorded(&log),
            vec![
                "started:1",
                "renamed:old.md->new.md",
                "deleted:gone.md",
                "indexed:only.md:4:1/1",
                "finished",
            ],
            "renamed/deleted are forwarded, and the indexed file is still 1/1"
        );
    }

    #[test]
    fn test_callback_start_indexing_zero_still_emits_started() {
        // The other modes return early on an empty knowledge base and build no
        // bar (`test_start_indexing_zero_is_noop` pins that). A consumer
        // drawing its own progress still has to be told there is nothing to
        // do, so Callback is the one mode that reports it.
        let (log, f) = recorder();
        let mut r = ProgressReporter::with_callback(f);
        r.start_indexing(0);
        r.finish();

        assert_eq!(recorded(&log), vec!["started:0", "finished"]);
    }

    #[test]
    fn test_callback_drop_without_finish_emits_no_finished() {
        // An interrupted run -- a `?` early return or a panic inside
        // `rebuild_index` -- drops the reporter without calling `finish`, and
        // no `Finished` is emitted, which is the price of never emitting it
        // twice. What tells the consumer the run ended is not an event: on the
        // `?` return it is the `Err` it gets back, and on a panic there is no
        // `Err` either -- the unwind carries out of the call. The panic half
        // is pinned by `test_callback_panic_unwinds_out_of_report_and_emits_no_finished`.
        let (log, f) = recorder();
        {
            let mut r = ProgressReporter::with_callback(f);
            r.start_indexing(1);
            r.report_indexed("a.md", 1);
        }

        assert_eq!(recorded(&log), vec!["started:1", "indexed:a.md:1:1/1"]);
    }

    #[test]
    fn test_callback_reporter_is_send_and_works_on_a_worker_thread() {
        // The embedding application indexes off its UI thread, so the whole
        // reporter -- not just the closure -- has to cross the boundary.
        fn assert_send<T: Send>() {}
        assert_send::<ProgressReporter>();
        assert_send::<ProgressEvent<'static>>();
        assert_send::<ProgressCallback>();

        let (log, f) = recorder();
        std::thread::spawn(move || {
            let mut r = ProgressReporter::with_callback(f);
            r.start_indexing(1);
            r.report_indexed("worker.md", 7);
            r.finish();
        })
        .join()
        .expect("worker thread");

        assert_eq!(
            recorded(&log),
            vec!["started:1", "indexed:worker.md:7:1/1", "finished"]
        );
    }

    #[test]
    fn test_callback_start_indexing_twice_emits_started_once() {
        // `start_indexing` is `pub`, so a second call is reachable from an
        // embedding application. Letting it through would emit a second
        // `Started` against a doc that promises one, and would reset `total`
        // while leaving `count` alone -- after which `done` walks past the
        // `total` it is reported against. The second call is ignored instead.
        let (log, f) = recorder();
        let mut r = ProgressReporter::with_callback(f);
        r.start_indexing(3);
        r.start_indexing(99);
        r.report_indexed("a.md", 2);
        r.finish();

        let got = recorded(&log);
        assert_eq!(
            got.iter()
                .filter(|line| line.starts_with("started:"))
                .count(),
            1,
            "the second start_indexing must emit nothing"
        );
        assert_eq!(
            got,
            vec!["started:3", "indexed:a.md:2:1/3", "finished"],
            "the first total wins, and done is counted against it"
        );
    }

    #[test]
    fn test_callback_panic_unwinds_out_of_report_and_emits_no_finished() {
        // A panicking callback is the consumer's bug, and the reporter does
        // not contain it: the unwind carries out through `report_*` and, in a
        // real run, out of `crate::indexer::rebuild_index`, so the caller gets
        // neither `Finished` nor an `Err`. The rustdoc on `with_callback`
        // promises exactly that, and this is what holds it to it.
        //
        // The default panic hook is left alone, and the output stays clean
        // anyway: libtest captures what a test writes and prints it only when
        // that test fails, so the message is swallowed on the green path and
        // still there to read on a red one. Installing a silent hook would
        // instead be process-wide while this binary runs its tests in
        // parallel -- the race `crate::parser::panic_guard` documents and
        // works around with a thread-local flag -- and it would blind the
        // other tests to boot.
        let log: EventLog = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&log);
        let f: ProgressCallback = Box::new(move |ev| match ev {
            ProgressEvent::Indexed { .. } => panic!("consumer bug"),
            ProgressEvent::Finished => sink
                .lock()
                .expect("recorder mutex")
                .push("finished".to_string()),
            _ => sink
                .lock()
                .expect("recorder mutex")
                .push("other".to_string()),
        });

        let mut r = ProgressReporter::with_callback(f);
        r.start_indexing(1);

        let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            r.report_indexed("a.md", 1);
        }));
        assert!(
            unwound.is_err(),
            "a panic in the callback must unwind out of report_indexed, not be swallowed"
        );
        assert!(
            !recorded(&log).contains(&"finished".to_string()),
            "finish() was never reached, so no Finished can have been emitted"
        );
    }

    // -----------------------------------------------------------------------
    // Cancellation and scan progress (feature-60)
    // -----------------------------------------------------------------------

    #[test]
    fn test_cancel_token_clones_share_one_flag() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<CancelToken>();

        let token = CancelToken::new();
        let clone = token.clone();
        assert!(!token.is_cancelled() && !clone.is_cancelled());
        assert!(!CancelToken::default().is_cancelled());

        clone.cancel();
        assert!(
            token.is_cancelled(),
            "cancel() on a clone must be seen through the original"
        );
        // Idempotent: a second call must not panic and leaves the flag set.
        clone.cancel();
        assert!(token.is_cancelled());

        // Any thread may set it.
        let other = CancelToken::new();
        let remote = other.clone();
        std::thread::spawn(move || remote.cancel())
            .join()
            .expect("worker thread");
        assert!(other.is_cancelled());
    }

    #[test]
    fn test_reporter_without_token_is_never_cancelled() {
        let (_log, f) = recorder();
        let reporters = vec![
            ProgressReporter::new(ProgressMode::Verbose),
            ProgressReporter::new(ProgressMode::Quiet),
            ProgressReporter::new(ProgressMode::Auto),
            ProgressReporter::from_cli_flags(false, false),
            ProgressReporter::from_cli_flags(true, false),
            ProgressReporter::from_cli_flags(false, true),
            ProgressReporter::with_callback(f),
        ];
        for r in &reporters {
            assert!(
                !r.is_cancelled(),
                "a reporter given no token is never cancelled"
            );
        }
    }

    #[test]
    fn test_with_cancel_applies_in_every_mode() {
        let token = CancelToken::new();
        let (_log, f) = recorder();
        let reporters = vec![
            ProgressReporter::new(ProgressMode::Verbose).with_cancel(token.clone()),
            ProgressReporter::new(ProgressMode::Quiet).with_cancel(token.clone()),
            ProgressReporter::new(ProgressMode::Auto).with_cancel(token.clone()),
            ProgressReporter::from_cli_flags(true, false).with_cancel(token.clone()),
            ProgressReporter::with_callback(f).with_cancel(token.clone()),
        ];
        for r in &reporters {
            assert!(!r.is_cancelled(), "the token is not set yet");
        }
        token.cancel();
        for r in &reporters {
            assert!(r.is_cancelled(), "every mode follows its token");
        }

        // A second with_cancel replaces the first token.
        let set = CancelToken::new();
        set.cancel();
        let r = ProgressReporter::new(ProgressMode::Quiet)
            .with_cancel(set)
            .with_cancel(CancelToken::new());
        assert!(!r.is_cancelled(), "the later token wins");
    }

    #[test]
    fn test_callback_scanning_counts_against_started_total() {
        let (log, f) = recorder();
        let mut r = ProgressReporter::with_callback(f);
        r.start_indexing(3);
        r.report_scanned();
        r.report_scanned();
        r.report_scanned();
        r.report_indexed("a.md", 1);
        r.finish();

        assert_eq!(
            recorded(&log),
            vec![
                "started:3",
                "scanning:1/3",
                "scanning:2/3",
                "scanning:3/3",
                "indexed:a.md:1:1/3",
                "finished",
            ],
            "the scan counts files against Started's total, and the document count starts from 1"
        );
    }

    #[test]
    fn test_callback_finish_cancelled_emits_cancelled_once_and_no_finished() {
        let (log, f) = recorder();
        let mut r = ProgressReporter::with_callback(f);
        r.start_indexing(2);
        r.report_indexed("a.md", 1);
        r.finish_cancelled();

        let got = recorded(&log);
        assert_eq!(
            got,
            vec!["started:2", "indexed:a.md:1:1/2", "cancelled:1/2"]
        );
        // `finish_cancelled` consumes `self`, so `Drop` runs right behind it;
        // it must add neither a second terminal event nor a `Finished`.
        assert_eq!(
            got.iter()
                .filter(|line| line.starts_with("cancelled:") || *line == "finished")
                .count(),
            1,
            "exactly one terminal event"
        );
    }

    #[test]
    fn test_callback_cancelled_done_ignores_scanned_files() {
        let (log, f) = recorder();
        let mut r = ProgressReporter::with_callback(f);
        r.start_indexing(3);
        r.report_scanned();
        r.report_scanned();
        r.finish_cancelled();

        assert_eq!(
            recorded(&log),
            vec!["started:3", "scanning:1/3", "scanning:2/3", "cancelled:0/3"],
            "Cancelled.done counts documents, never scanned files"
        );
    }

    #[test]
    fn test_callback_cancelled_done_counts_reported_documents() {
        let (log, f) = recorder();
        let mut r = ProgressReporter::with_callback(f);
        r.start_indexing(3);
        r.report_scanned();
        r.report_scanned();
        r.report_scanned();
        r.report_indexed("a.md", 1);
        r.report_unchanged("b.md");
        r.finish_cancelled();

        assert_eq!(
            recorded(&log).last().map(String::as_str),
            Some("cancelled:2/3"),
            "Indexed and Unchanged both count toward Cancelled.done"
        );
    }

    #[test]
    fn test_nontty_report_scanned_does_not_tick_progress_count() {
        // `Progress: N/M` counts documents; the scan must not advance it, or
        // N would run to twice M.
        let r = ProgressReporter::from_inner(ProgressInner::NonTty {
            total: 3,
            step: 1,
            count: AtomicU64::new(0),
        });
        r.report_scanned();
        r.report_scanned();
        r.report_scanned();
        let ProgressInner::NonTty { count, .. } = &r.inner else {
            panic!("expected NonTty variant");
        };
        assert_eq!(
            count.load(Ordering::Relaxed),
            0,
            "the scan must not tick count"
        );
        r.report_indexed("a.md", 1);
        assert_eq!(count.load(Ordering::Relaxed), 1, "documents still tick it");
    }

    #[test]
    fn test_tty_report_scanned_moves_the_message_not_the_bar() {
        // A hidden draw target: the bar keeps its state but renders nothing.
        use indicatif::{ProgressBar, ProgressDrawTarget};
        let bar = ProgressBar::with_draw_target(Some(2), ProgressDrawTarget::hidden());
        let r = ProgressReporter::from_inner(ProgressInner::Tty(bar));
        let ProgressInner::Tty(bar) = &r.inner else {
            panic!("expected Tty variant");
        };

        r.report_scanned();
        assert_eq!(bar.message(), "scanning 1/2");
        assert_eq!(bar.position(), 0, "the scan must not move the bar");

        r.report_scanned();
        assert_eq!(
            bar.message(),
            "",
            "the message is cleared once the scan reaches total, so an unchanged-only run does not keep it"
        );
        assert_eq!(bar.position(), 0, "the bar still belongs to the documents");
    }

    /// feature-62 (J25): the one-time notice is not an event, so a callback reporter hears
    /// nothing of it, and a quiet one writes nothing (its output is the subprocess tests').
    #[test]
    fn announce_hands_a_callback_reporter_nothing() {
        let calls = Arc::new(AtomicU64::new(0));
        let seen = Arc::clone(&calls);
        let r = ProgressReporter::with_callback(Box::new(move |_| {
            seen.fetch_add(1, Ordering::Relaxed);
        }));
        r.announce("Re-reading 1 unchanged .docx document(s) once");
        assert_eq!(calls.load(Ordering::Relaxed), 0);
        ProgressReporter::new(ProgressMode::Quiet).announce("quiet");
    }
}
