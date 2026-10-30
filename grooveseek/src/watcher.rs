//! File watcher that debounces OS events and dispatches them to
//! the incremental index API (`indexer::reindex_single_file` /
//! `deindex_single_file` / `rename_single_file`).
//!
//! Architecture:
//!
//! ```text
//! notify-debouncer-full (std::sync::mpsc::Sender)
//!        │  DebouncedEvent batches
//!        ▼
//!   bridge thread
//!        │  tokio::mpsc::UnboundedSender
//!        ▼
//!   tokio task (run_watch_loop)
//!        │  one spawn_blocking per event batch
//!        ▼
//!   handle_events (blocking pool)
//!        │  classify events, lookup Mutex<Database> / Mutex<Embedder>
//!        ▼
//!   indexer::{reindex,deindex,rename}_single_file
//! ```
//!
//! [`crate::watcher::run_watch_loop`] hands each event batch to `tokio::task::spawn_blocking`
//! because [`crate::watcher::handle_events`] locks the embedder and, with the OpenAI-compatible
//! provider, may build a blocking HTTP client on its first embed call; doing
//! either on a tokio worker thread would block it (the client build panics).
//!
//! The bridge thread is necessary because `notify-debouncer-full` ships with
//! `std::sync::mpsc` and must run synchronously. Keeping the dispatch side on
//! the tokio runtime lets us `select!` it against `service.waiting()` so the
//! MCP server and the watcher run concurrently.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64};
use std::sync::{LockResult, Mutex, MutexGuard, TryLockError};
use std::time::Duration;

use crate::poison::{recover, recover_db};

use anyhow::Result;
use notify::RecursiveMode;
use notify_debouncer_full::notify::event::{EventKind, ModifyKind, RenameMode};
use notify_debouncer_full::{DebouncedEvent, RecommendedCache, new_debouncer_opt};
use serde::Deserialize;
use tokio::sync::mpsc;

use crate::db::Database;
use crate::embedder::Embedder;
use crate::indexer;
use crate::parser::Registry;
use crate::server::{WatchErrorCause, WatchStatusFn};

/// Print a watcher diagnostic, ASCII-escaped.
///
/// **Every** `watcher:` line in this file goes through here. A macro rather than
/// a function so the call sites keep `format!` syntax, and defined this early
/// because `macro_rules!` is textually scoped — a diagnostic added above this
/// point would not compile, which is the cheapest possible enforcement.
///
/// One printer is the only way "diagnostics stay ASCII" (AGENTS.md) holds as an
/// invariant rather than a habit. Escaping at the call sites was tried for two
/// review rounds and kept losing: the subtree scan feeds already-existing
/// printers, so "escape the lines this branch adds" has no stable boundary —
/// every path that newly reaches an old `eprintln!` is a new stderr path.
/// Asking one question in two places is also exactly how this file drifted
/// twice before (AU-03, BU-19).
macro_rules! wdiag {
    ($($arg:tt)*) => {
        eprintln!("{}", ascii_diag(&format!($($arg)*)))
    };
}

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

/// `[watch]` セクション (`groove.toml`)。
///
/// - `enabled` 省略時: `true` (groove の値提案 = "常に fresh" を守るため)
/// - `debounce_ms` 省略時: 500ms。エディタの save が複数イベントを生む
///   ケースを吸収するのに十分な長さ
///
/// セクション自体が無ければ `WatchConfig::default()` (= enabled=true,
/// debounce=500ms) が適用される。`--no-watch` CLI flag で opt-out 可能。
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WatchConfig {
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    #[serde(default = "default_debounce_ms")]
    pub debounce_ms: u64,
}

fn default_enabled() -> bool {
    true
}
fn default_debounce_ms() -> u64 {
    500
}

impl Default for WatchConfig {
    fn default() -> Self {
        Self {
            enabled: default_enabled(),
            debounce_ms: default_debounce_ms(),
        }
    }
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// 共有状態。`run_watch_loop` が `tokio::select!` の一方として起動される。
/// 各イベントは `Mutex<Database>` / `Mutex<Embedder>` を順にロックして直列化する
/// (fastembed は同時呼び出し不可、rusqlite も writer 1 本想定)。
#[allow(dead_code)]
pub struct WatcherState {
    pub kb_path: PathBuf,
    pub db: Arc<Mutex<Database>>,
    pub embedder: Arc<Mutex<Embedder>>,
    pub registry: Arc<Registry>,
    pub exclude_headings: Option<Vec<String>>,
    /// (feature-49) 除外の 3 層をまとめた判定器。`exclude_dirs` の生リストを
    /// 持っていた頃と違い、`.grooveignore` の中身も含む = **可変**。
    /// ignore ファイルが書き換わったら [`handle_events`] が差し替える。
    pub rules: crate::exclusion::ExclusionRules,
    pub config: WatchConfig,
    /// Set to `true` for the duration of the watch loop (feature-43 PR-2).
    /// Shared with `KbServerShared` so `/api/admin/status` can report it.
    pub watcher_active: Arc<std::sync::atomic::AtomicBool>,
    /// Told why the watch failed, from the bridge thread, before each retry
    /// sleep; told `None` once the watch is registered. `None` reports nothing.
    pub watch_status: Option<WatchStatusFn>,
    /// The first and the longest retry wait of the bridge thread after a
    /// failed watch. [`WatcherState::DEFAULT_WATCH_BACKOFF`] in production.
    pub watch_backoff: (Duration, Duration),
    /// Counts batches as they pass the channel and the event loop; `None`
    /// counts nothing.
    pub batch_probe: Option<Arc<WatchBatchProbe>>,
}

impl WatcherState {
    /// One second first, doubling up to thirty.
    pub const DEFAULT_WATCH_BACKOFF: (Duration, Duration) =
        (Duration::from_secs(1), Duration::from_secs(30));

    /// Lock `m` (the embedder or the database) for batch handling. With a
    /// [`WatcherState::batch_probe`] set, a lock held elsewhere is counted in
    /// [`WatchBatchProbe::lock_waits`] before the wait; without one this is
    /// `m.lock()`.
    fn lock_for_batch<'a, T>(&self, m: &'a Mutex<T>) -> LockResult<MutexGuard<'a, T>> {
        if let Some(p) = &self.batch_probe {
            match m.try_lock() {
                Ok(guard) => return Ok(guard),
                Err(TryLockError::Poisoned(poisoned)) => return Err(poisoned),
                Err(TryLockError::WouldBlock) => {
                    p.lock_waits
                        .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                }
            }
        }
        m.lock()
    }
}

/// Batch counters of one watch loop, for a caller that has to know whether a
/// batch was cut short by the stop signal.
#[derive(Debug, Default)]
pub struct WatchBatchProbe {
    /// Batches the debouncer handed over, counted before the channel send
    /// whatever it returned (a batch sent after the loop stopped finds the
    /// channel closed and is still counted).
    pub emitted: AtomicU64,
    /// Batches the event loop took off the channel.
    pub started: AtomicU64,
    /// Batches whose handling returned.
    pub finished: AtomicU64,
    /// The stop signal found a batch still waiting in the channel.
    pub stopped_with_queued: AtomicBool,
    /// Times batch handling found the embedder or database lock held
    /// elsewhere, counted before it waits for that lock. A batch blocked on a
    /// lock its caller holds has counted here once.
    pub lock_waits: AtomicU64,
}

/// Watcher タスク本体。notify の裏スレッドから tokio channel 越しにイベントを
/// 受け取り、indexer 増分 API にディスパッチする。
///
/// `enabled = false` なら即座に `Ok(())` を返す (watcher は起動しない)。
/// タスク内部での処理エラーはログに流して次のイベントへ進む (silent drop 禁止)。
/// tokio task が panic しないよう各イベント処理は `catch_unwind` 相当の防衛線を
/// 張らない代わりに、error 経路は `eprintln!` で可視化する。
///
/// Runs until the channel closes; it has no stop signal. A caller that has to
/// stop it uses [`run_watch_loop_until`].
pub async fn run_watch_loop(state: WatcherState) -> Result<()> {
    // The sender is held for the whole loop, so the signal never fires.
    let (_tx, rx) = tokio::sync::watch::channel(false);
    run_watch_loop_until(state, rx).await
}

/// [`run_watch_loop`] with a stop signal: once the stop receiver turns `true` the
/// loop
/// returns `Ok(())` at the next batch boundary. A batch already being handled
/// is finished first; one still waiting in the channel is not handled.
/// Dropping the stop signal's sender is not a stop.
pub async fn run_watch_loop_until(
    mut state: WatcherState,
    mut stop: tokio::sync::watch::Receiver<bool>,
) -> Result<()> {
    if !state.config.enabled {
        return Ok(());
    }

    // (feature-43 PR-2) Mark the watcher as active for the duration of this
    // function, and clear the flag on any exit path via a Drop guard so that
    // `/api/admin/status` always reflects the true state — including early
    // return / panic / `?` propagation.
    use std::sync::atomic::Ordering;
    state.watcher_active.store(true, Ordering::Relaxed);
    struct ActiveGuard(Arc<std::sync::atomic::AtomicBool>);
    impl Drop for ActiveGuard {
        fn drop(&mut self) {
            self.0.store(false, Ordering::Relaxed);
        }
    }
    let _active_guard = ActiveGuard(Arc::clone(&state.watcher_active));

    // F-36: bounded channel で event flood 時のメモリ無制限増を防ぐ。
    // 1 element = debounce 窓内の events 塊 (DebouncedEvent ベクトル) なので、
    // 64 batch ぶん buffer すれば通常 1 秒未満の handle_events 処理待ちは
    // 吸収できる。それを超える backlog は handle_events 側 (embedder/db lock
    // を取って同期処理) が遅延の原因なので、新 batch を drop + warn して
    // 「何か詰まっている」が visible に出るようにする。
    const WATCHER_CHANNEL_CAPACITY: usize = 64;
    let (tx_async, mut rx_async) = mpsc::channel::<Vec<DebouncedEvent>>(WATCHER_CHANNEL_CAPACITY);
    let debounce = Duration::from_millis(state.config.debounce_ms);
    let kb_watch_path = state.kb_path.clone();
    let watch_status = state.watch_status.clone();
    let (backoff0, max_backoff) = state.watch_backoff;
    let probe_tx = state.batch_probe.clone();

    // bridge thread: std::sync::mpsc → tokio::sync::mpsc
    // watch 初期化や watch() が失敗 (ディレクトリ削除等) した
    // 場合は指数バックオフで再試行する。30 秒以内に復帰できなければ次周で延期。
    let _bridge = std::thread::Builder::new()
        .name("groove-watcher".to_string())
        .spawn(move || {
            // 外側ループ = self-heal。debouncer ハンドルが生きている間は
            // inner parking で停止、壊れたら backoff して再構築。
            let mut backoff = backoff0;
            let report = |cause: Option<WatchErrorCause>| {
                if let Some(f) = &watch_status {
                    f(cause);
                }
            };
            let failed = |error: String, backoff: Duration| {
                report(Some(WatchErrorCause {
                    kb: kb_watch_path.clone(),
                    error,
                    retry_secs: backoff.as_secs(),
                }))
            };
            loop {
                let tx_clone = tx_async.clone();
                let probe_cb = probe_tx.clone();
                let debouncer_result = new_debouncer_opt::<_, Backend, RecommendedCache>(
                    debounce,
                    None,
                    move |res: notify_debouncer_full::DebounceEventResult| match res {
                        Ok(events) => {
                            // F-36: bounded channel なので送信は try_send
                            // (debouncer callback は std thread = blocking_send
                            // が呼べないため)。Full は handle_events 側の
                            // 詰まりを意味するので、drop + warn で可視化する。
                            // 同 batch で次回 callback 時に events は再生成され
                            // ない (debouncer の固定 windowing) ので、ここで
                            // drop した変更は観測できないが、tail-drop は
                            // memory の観点で確定の上限が出る。
                            if let Some(p) = &probe_cb {
                                p.emitted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                            }
                            match tx_clone.try_send(events) {
                                Ok(()) => {}
                                Err(mpsc::error::TrySendError::Full(_)) => {
                                    wdiag!(
                                        "watcher: event channel full (capacity {WATCHER_CHANNEL_CAPACITY}); \
                                         dropping batch. handle_events is too slow or blocked. \
                                         Consider increasing groove resources or running rebuild_index manually."
                                    );
                                }
                                Err(mpsc::error::TrySendError::Closed(_)) => {
                                    // receiver (tokio task) drop → 静かに終了
                                }
                            }
                        }
                        Err(errs) => {
                            for e in errs {
                                wdiag!("watcher: debouncer error: {e:?}");
                            }
                        }
                    },
                    RecommendedCache::new(),
                    notify_config(),
                );
                let mut debouncer = match debouncer_result {
                    Ok(d) => d,
                    Err(e) => {
                        wdiag!(
                            "watcher: failed to create debouncer: {e} (retry in {}s)",
                            backoff.as_secs()
                        );
                        failed(e.to_string(), backoff);
                        std::thread::sleep(backoff);
                        backoff = (backoff * 2).min(max_backoff);
                        continue;
                    }
                };
                if let Err(e) = debouncer.watch(&kb_watch_path, RecursiveMode::Recursive) {
                    wdiag!(
                        "watcher: failed to watch {}: {e} (retry in {}s)",
                        kb_watch_path.display(),
                        backoff.as_secs()
                    );
                    drop(debouncer);
                    failed(e.to_string(), backoff);
                    std::thread::sleep(backoff);
                    backoff = (backoff * 2).min(max_backoff);
                    continue;
                }
                // 成功: backoff をリセット
                backoff = backoff0;
                // (AU-55) The only point at which the watcher is actually
                // armed. `wait_http_200` returning says the *server* is up,
                // which happens before this thread finishes building the
                // debouncer, so a test that starts editing files at that
                // moment can lose the event with no way to notice — the
                // debouncer either saw it or it did not.
                //
                // Emitting it here rather than before `watch()` matters: a
                // failed `watch()` retries in the branch above, and a signal
                // printed ahead of that would claim readiness the retry
                // contradicts.
                wdiag!(
                    "watcher: watching {} (debounce {}ms)",
                    kb_watch_path.display(),
                    debounce.as_millis()
                );
                report(None);
                // periodic liveness probe: 30 秒ごとに kb_path の
                // 存在確認をして、ディレクトリが消えていたら debouncer を
                // drop して再構築する。inotify は親ディレクトリ削除時に
                // 無音で死ぬため明示的な polling が必要。
                let probe_interval = Duration::from_secs(30);
                loop {
                    std::thread::park_timeout(probe_interval);
                    if !kb_watch_path.exists() {
                        wdiag!(
                            "watcher: kb_path {} vanished, will retry",
                            kb_watch_path.display()
                        );
                        break;
                    }
                }
                drop(debouncer);
            }
        })
        .map_err(|e| anyhow::anyhow!("failed to spawn watcher thread: {e}"))?;

    wdiag!(
        "watcher started ({:?} debounce, {:?})",
        debounce,
        state.registry.extensions()
    );

    use std::sync::atomic::Ordering::SeqCst;
    let probe = state.batch_probe.clone();
    // Cleared when the stop sender is dropped: that is not a stop, and a
    // closed `wait_for` would otherwise return at once on every turn.
    let mut stop_open = true;
    loop {
        tokio::select! {
            // The stop is looked at first, so a batch waiting in the channel
            // is not started once the signal is up.
            biased;
            // The `Ref` that `wait_for` returns is not `Send`; it is read and
            // let go inside this future so it never lives across an await.
            stopped = async { stop.wait_for(|v| *v).await.is_ok() }, if stop_open => {
                if stopped {
                    if let Some(p) = &probe
                        && rx_async.try_recv().is_ok()
                    {
                        p.stopped_with_queued.store(true, SeqCst);
                    }
                    break;
                }
                stop_open = false;
            }
            ev = rx_async.recv() => {
                let Some(events) = ev else { break };
                if let Some(p) = &probe {
                    p.started.fetch_add(1, SeqCst);
                }
                // `handle_events` locks `embedder` and, on the first embed call, builds
                // the blocking reqwest client — both happen synchronously here, so
                // running it inline on this tokio worker panics exactly like the
                // pre-fix search path did (`Cannot drop a runtime in a context where
                // blocking is not allowed`). Move to the blocking pool per batch
                // (not around the whole receive loop) so shutdown still responds.
                state = tokio::task::spawn_blocking(move || {
                    handle_events(&mut state, &events);
                    state
                })
                .await
                .map_err(|e| anyhow::anyhow!("watcher: handle_events task panicked: {e}"))?;
                if let Some(p) = &probe {
                    p.finished.fetch_add(1, SeqCst);
                }
            }
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Internal
// ---------------------------------------------------------------------------

/// The backend the debouncer drives. On macOS the FSEvents watcher sits behind
/// [`StaleCreateFilter`] (kuriya issues #520 and #525); elsewhere the
/// recommended watcher is used as it is.
#[cfg(target_os = "macos")]
type Backend = StaleCreateFilter<notify::RecommendedWatcher>;
#[cfg(not(target_os = "macos"))]
type Backend = notify::RecommendedWatcher;

/// (kuriya issue #520) A `notify::Watcher` that drops a `Create` or a
/// content / metadata `Modify` for a path that is no longer there, before the
/// debouncer sees it.
///
/// FSEvents reports one record per path carrying every flag that path has
/// collected, not only what just happened, and notify turns each flag into
/// its own event in a fixed order, `Create` first (how notify 8.2.0's FSEvents
/// backend, its `fsevent` module, translates the flags). Measured on the macOS runner (nightly
/// run 39578259680): deleting a file written 8 s before the watch started
/// arrives as `Create(File)`, `Remove(File)`, `Modify(Data(Content))`.
/// notify-debouncer-full reads that as "created and gone within the window":
/// on a `Remove` it drops the whole queue of a path whose first event is a
/// `Create` (`push_remove_event`), so only the trailing `Modify` is handed on,
/// and for a directory nothing at all. A rename whose source queue starts with
/// a `Create` likewise loses its rename event (`push_rename_event`). The
/// watcher then never hears that the old path went away and its rows stay.
///
/// Such an event for a path that does not exist when it is delivered
/// announces nothing the watcher could index, so dropping it loses no
/// arrival; what is left is the `Remove` or rename half, which the debouncer
/// then hands on. `Modify(Name(_))` (the rename halves), `Remove`, every other
/// kind and every error pass through untouched.
///
/// Known limitation (kuriya issue #525): FSEvents does not say which end of a
/// rename a path is, so the debouncer guesses from whether the path exists
/// (`Modify(Name(Any))`). When the source directory is created again before
/// the event is delivered, both ends exist, the rename is not paired, and the
/// rows under the old directory stay until the next full index. Dropping
/// events cannot recover that; it is the same family as the unpaired macOS
/// renames the watcher tests already allow for.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
struct StaleCreateFilter<W>(W);

/// The `notify::EventHandler` [`StaleCreateFilter`] hands the inner watcher:
/// asks [`is_stale_create`] and forwards everything it keeps.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
struct StaleCreateHandler<H>(H);

/// Whether [`StaleCreateFilter`] drops `event`: a `Create` or a non-rename
/// `Modify` whose first path is gone ("not found", not any other stat failure,
/// which may hide a path that is there).
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn is_stale_create(event: &notify::Event) -> bool {
    let arrival_or_change = match event.kind {
        EventKind::Create(_) => true,
        EventKind::Modify(ModifyKind::Name(_)) => false,
        EventKind::Modify(_) => true,
        _ => false,
    };
    arrival_or_change
        && event.paths.first().is_some_and(|p| {
            std::fs::symlink_metadata(p).is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound)
        })
}

impl<H: notify::EventHandler> notify::EventHandler for StaleCreateHandler<H> {
    fn handle_event(&mut self, event: notify::Result<notify::Event>) {
        if let Ok(ev) = &event
            && is_stale_create(ev)
        {
            return;
        }
        self.0.handle_event(event);
    }
}

impl<W: notify::Watcher> notify::Watcher for StaleCreateFilter<W> {
    fn new<F: notify::EventHandler>(
        event_handler: F,
        config: notify::Config,
    ) -> notify::Result<Self> {
        W::new(StaleCreateHandler(event_handler), config).map(Self)
    }

    fn watch(&mut self, path: &Path, recursive_mode: RecursiveMode) -> notify::Result<()> {
        self.0.watch(path, recursive_mode)
    }

    fn unwatch(&mut self, path: &Path) -> notify::Result<()> {
        self.0.unwatch(path)
    }

    fn configure(&mut self, option: notify::Config) -> notify::Result<bool> {
        self.0.configure(option)
    }

    fn kind() -> notify::WatcherKind {
        W::kind()
    }
}

/// 単一 event を以下のどれかに分類する (evaluator High #1 対応):
/// - Rename (from, to): notify-debouncer-full が paths.len()==2 で渡してきたペア
/// - Reindex: Create / Data/Metadata/Any/Other Modify (Name は除外)
/// - Deindex: Remove / Name(From) のみの 1-path 版
/// - Ignore: Access / Other 種別
///
/// 1 パスを同じ batch 内で「reindex も rename も」両方ディスパッチすると、
/// rename-to のパスに対して upsert + その後の path UPDATE で UNIQUE 制約違反が
/// 起きるため、この関数で排他的に分類する。
#[derive(Debug, PartialEq)]
enum Classified<'a> {
    Rename {
        from: &'a std::path::PathBuf,
        to: &'a std::path::PathBuf,
    },
    Reindex(&'a [std::path::PathBuf]),
    Deindex(&'a [std::path::PathBuf]),
    Ignore,
}

fn classify(evt: &DebouncedEvent) -> Classified<'_> {
    match &evt.event.kind {
        // rename ペアが debouncer で stitch 済みのケース (macOS / Windows で頻出)
        EventKind::Modify(ModifyKind::Name(RenameMode::Both)) if evt.paths.len() == 2 => {
            Classified::Rename {
                from: &evt.paths[0],
                to: &evt.paths[1],
            }
        }
        // 一般的な Modify(Name(Any)) 等で paths.len()==2 のケース
        EventKind::Modify(ModifyKind::Name(_)) if evt.paths.len() == 2 => Classified::Rename {
            from: &evt.paths[0],
            to: &evt.paths[1],
        },
        // Name(From) 単独 → 旧 path の削除として扱う
        EventKind::Modify(ModifyKind::Name(RenameMode::From)) => Classified::Deindex(&evt.paths),
        // Name(To) / Name(Any) で 1 path → 新 path の reindex として扱う
        EventKind::Modify(ModifyKind::Name(_)) => Classified::Reindex(&evt.paths),
        // その他の Modify (Data / Metadata / Any / Other) は reindex
        EventKind::Modify(_) | EventKind::Create(_) => Classified::Reindex(&evt.paths),
        EventKind::Remove(_) => Classified::Deindex(&evt.paths),
        _ => Classified::Ignore,
    }
}

/// (feature-49) `.grooveignore` が書き換わったら判定器を組み直す。
///
/// `&mut` で済むのは `WatcherState` を所有しているのが [`run_watch_loop`] 1 本
/// だけだから。ロックを足さずに済むこの形が一番強い排他で、BU-32 の
/// 「並行の不変条件は論証でなく排他で閉じる」をコストゼロで満たす。
///
/// **効くのはこれ以降のイベントだけ**。すでに index に入っている文書は次の
/// full index (`groove index` / MCP `rebuild_index`) まで残る — walk から消えた
/// path を DB から落とすのは削除 pass の仕事で、watcher は個々のイベントしか
/// 見ないため。ログでそう言う。
/// Escape a diagnostic so it survives a non-UTF-8 console.
///
/// AGENTS.md asks stderr to stay ASCII: it is read on a console, and on a
/// Japanese Windows install that console is CP932, where anything outside the
/// code page arrives as mojibake. A knowledge base path is routinely outside it,
/// and so is the text of an error that embeds one — so the **whole formatted
/// message** goes through here, not just the path (codex P1 on PR #168).
///
/// ASCII graphics and spaces pass through unchanged, which keeps ordinary
/// messages readable; everything else becomes `\u{...}` or `\n`, which is
/// lossless — two different paths never collapse into the same diagnostic.
///
/// Reached through [`wdiag!`], which every watcher diagnostic uses. Escaping at
/// the call sites instead was tried for two review rounds and kept losing: the
/// scan feeds already-existing printers ([`dispatch_reindex`] and friends), so
/// "escape the lines this branch adds" has no stable boundary — every path that
/// newly reaches an old `eprintln!` is a new stderr path. One printer, one rule.
/// The embedder's transport errors escape their causes with it too (AW-16).
pub(crate) fn ascii_diag(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c == ' ' || c.is_ascii_graphic() {
            out.push(c);
        } else {
            out.extend(c.escape_default());
        }
    }
    out
}

/// Does this event kind mean the path **just arrived**, as opposed to changing
/// in place?
///
/// Only an arrival can be hiding contents nobody has seen. `Classified::Reindex`
/// deliberately lumps arrivals together with `Modify(Data/Metadata/Any)` because
/// a file is re-read the same way either way — but a *directory* is not: walking
/// one costs its whole subtree, and doing that on every metadata change would
/// re-embed a large directory whenever its mtime moved (codex P2 on PR #168).
///
/// `Modify(Name(_))` counts: a rename into the knowledge base is an arrival, and
/// its contents are as unseen as a fresh `mkdir`'s.
fn is_arrival(kind: &EventKind) -> bool {
    matches!(
        kind,
        EventKind::Create(_) | EventKind::Modify(ModifyKind::Name(_))
    )
}

/// Is this event path the knowledge base's own `.grooveignore`?
///
/// The root's, and only the root's: a `sub/.grooveignore` is not read, so a
/// write to one must not silently reload anything either. Routed through
/// [`to_rel`] so it answers the same way the rest of the watcher relativizes.
fn is_ignore_file(kb_path: &Path, p: &Path) -> bool {
    to_rel(kb_path, p).as_deref() == Some(crate::exclusion::IGNORE_FILE_NAME)
}

fn reload_rules(state: &mut WatcherState) {
    let exclude_dirs = state.rules.exclude_dirs().to_vec();
    state.rules = crate::exclusion::ExclusionRules::load(&state.kb_path, exclude_dirs);
    let name = crate::exclusion::IGNORE_FILE_NAME;
    match state.rules.ignore_file_patterns() {
        Some(n) => wdiag!(
            "watcher: reloaded {name} ({n} pattern{}). New events follow the new rules; \
             documents already indexed stay until the next full index run.",
            if n == 1 { "" } else { "s" }
        ),
        None => {
            wdiag!("watcher: {name} is gone or could not be read; only exclude_dirs applies now.")
        }
    }
}

/// (audit AX-02) The notify configuration the watcher is built with: the default, with
/// `follow_symlinks` off.
///
/// notify 8.2.0 defaults it to `true` (the `Default` of notify's own configuration type), and
/// notify's inotify backend hands it to the `WalkDir` that sets up a recursive watch, both
/// for the root and for a directory moved in later. With it on, a
/// directory symlink inside the KB gets watches on its target, so on Linux the target's
/// files arrive as events under in-KB names -- the walk never follows such a link, and
/// `max_user_watches` runs out on `-> /`. The kqueue and poll backends read the same flag;
/// the FSEvents (macOS) and ReadDirectoryChangesW (Windows) backends do not read it, so it
/// changes nothing there. The single-file paths check where a file resolves as well
/// ([`indexer::reindex_single_file`]), which covers an event that reaches them anyway.
fn notify_config() -> notify::Config {
    notify::Config::default().with_follow_symlinks(false)
}

/// debounced event batch を分類して indexer に流す。
fn handle_events(state: &mut WatcherState, events: &[DebouncedEvent]) {
    // (feature-49) ignore ファイルの変更を分類より **先** に処理する。
    // `.grooveignore` は registry にある拡張子を持たないので、通常の経路に
    // 流すと `should_process` に弾かれ、変更に気づく機会ごと消える。
    if events
        .iter()
        .any(|evt| evt.paths.iter().any(|p| is_ignore_file(&state.kb_path, p)))
    {
        reload_rules(state);
    }

    let state = &*state;
    for evt in events {
        match classify(evt) {
            Classified::Rename { from, to } => {
                let (Some(old_rel), Some(new_rel)) =
                    (to_rel(&state.kb_path, from), to_rel(&state.kb_path, to))
                else {
                    continue;
                };
                // (feature-68) A directory rename: the file system reports only the directory.
                // Returns false when this is not one, and the file rename below runs as before.
                //
                // `dispatch_directory_rename` expands it into the file rename of each
                // document under the old prefix, sweeps what is left there once the
                // old directory is gone, and scans the destination for files not
                // indexed yet (which covers a directory created under a temporary
                // name before the debounce window, written to, and renamed inside it,
                // codex P2 on PR #168; one created inside the same window already
                // arrives as a Create). It used to be skipped here because indexing
                // the destination alone would have left the source subtree's rows
                // behind as stale hits that cannot be opened: there was no delete by
                // prefix. feature-67 added one and `dispatch_directory_rename` uses it.
                if dispatch_directory_rename(state, from, to, &old_rel, &new_rel) {
                    continue;
                }
                // 両端の可否で 3 分岐する (codex P2 on PR #81)。以前は
                // 「どちらかが通れば rename」だったため、index 済みファイルを
                // `.git/` や `node_modules/` へ rename すると
                // `rename_single_file` が **denylist 配下の新パスへ DB を
                // 書き換えて残す**。除外したはずのファイルが index に居座る。
                //
                // (kuriya 259) The old end is looked at before its row is dropped
                // ([`dispatch_removal`] with [`RemovalCause::RenamedAway`]): a file the write gate
                // takes back there is reindexed in place; with no row left there, nothing is
                // looked at.
                match rename_decision(&old_rel, from, &new_rel, to, &state.registry, &state.rules) {
                    RenameAction::Rename => {
                        dispatch_rename(state, &old_rel, &new_rel, WriteLook::Spelled)
                    }
                    RenameAction::Deindex => {
                        dispatch_removal(state, &old_rel, from, RemovalCause::RenamedAway);
                        hand_on_a_link_destination(state, &new_rel, to);
                    }
                    RenameAction::Reindex => dispatch_reindex(state, &new_rel, to),
                    RenameAction::Skip => hand_on_a_link_destination(state, &new_rel, to),
                }
            }
            Classified::Reindex(paths) => {
                // Only an event that means "this path just arrived" can be
                // hiding contents (codex P2). `Classified::Reindex` also covers
                // ordinary `Modify(Data/Metadata/Any)`, so scanning on all of
                // them would walk and re-embed a large existing directory every
                // time its mtime or permissions changed — enough work to block
                // event handling and overflow the bounded channel, for nothing.
                let arrived = is_arrival(&evt.event.kind);
                for p in paths {
                    let Some(rel) = to_rel(&state.kb_path, p) else {
                        continue;
                    };
                    // (local Codex r3 on AX-02) A path that is now a link: if it was a
                    // directory, whatever the index holds under it is reached through the
                    // link now, and the full index (which does not follow links) drops it.
                    // The removal that should have done so can be folded away by the
                    // debouncer when the link is made in the same window. Anything else about a
                    // link is decided below: `should_process` refuses it, and (kuriya 566) the
                    // removal path decides its own row. The db is locked only for
                    // a link, so an ordinary event costs one `symlink_metadata` and no lock.
                    if std::fs::symlink_metadata(p).is_ok_and(|m| m.file_type().is_symlink()) {
                        let handled = {
                            let db = recover_db(state.lock_for_batch(&state.db));
                            match deindex_under_a_link_parts(&db, &state.kb_path, p) {
                                Some((under, outcome)) => {
                                    match outcome {
                                        Ok(deleted) if deleted.is_empty() => {}
                                        Ok(deleted) => wdiag!(
                                            "watcher: {under} is now a link; deindexed {} documents at or under it",
                                            deleted.len()
                                        ),
                                        Err(e) => wdiag!(
                                            "watcher: deindexing under the link {under}/ failed: {e}"
                                        ),
                                    }
                                    true
                                }
                                None => false,
                            }
                        };
                        if handled {
                            continue;
                        }
                    }
                    // A directory that just appeared brings its contents with
                    // it, and on Linux those files produce no event of their
                    // own. Look inside once. `symlink_metadata` rather than
                    // `is_dir` so a symlink pointing at a directory is not
                    // followed — the walk refuses symlinks and so does
                    // `should_process`, and this must not be the one place that
                    // does otherwise.
                    if std::fs::symlink_metadata(p).is_ok_and(|m| m.file_type().is_dir()) {
                        // (#544 A2/A5) The look above is outside the lock; the one that decides
                        // the rows is taken again inside it, and only for an arrival, so an
                        // ordinary directory Modify still costs no lock.
                        if arrived {
                            let arrival = {
                                let db = recover_db(state.lock_for_batch(&state.db));
                                act_on_arrived_directory(&db, &state.kb_path, &rel, p)
                                // `db` is dropped here, before the scan locks it again.
                            };
                            if log_arrived_directory(&rel, arrival) {
                                dispatch_new_directory(state, p, &rel);
                            }
                        }
                        // A directory never had anything else to do here:
                        // `should_process` rejects it on the extension filter.
                        continue;
                    }
                    let facts = PathFacts::of(p);
                    if should_process(&rel, &facts, state) {
                        dispatch_reindex(state, &rel, p);
                    } else if facts.is_symlink() || facts.is_multiply_linked() {
                        // (kuriya 566 #22) A link the write gate refused: its row, if any, is
                        // decided by the removal path, as if the path's removal had been
                        // reported. The removal gate passes a link, so a file replaced by a
                        // hard link loses its row here, as the full index drops it. A file that
                        // only gains a second name elsewhere sends no event for its own path
                        // (kuriya 569), so its row waits for the full index. A symlink is mostly
                        // taken by the check above.
                        handle_removed_path(state, &rel, p);
                    }
                }
            }
            Classified::Deindex(paths) => {
                for p in paths {
                    if let Some(rel) = to_rel(&state.kb_path, p) {
                        handle_removed_path(state, &rel, p);
                    }
                    // (feature-67) Always, whatever the gate above said: a
                    // removed directory carries no extension, and its files'
                    // own events may have been folded into this one.
                    // (kuriya 557) It looks at the directory before it drops anything.
                    dispatch_subtree_removal(state, p);
                }
            }
            Classified::Ignore => {}
        }
    }
}

/// (#543) What a remove event does for the path `rel` (the event's path `full`): what is at the
/// path now decides, not the event kind alone. The event loop calls it for each removed path,
/// and (kuriya 557) [`dispatch_subtree_removal`] for each row under a removed directory that is
/// there again, as if that row's own removal had been reported.
fn handle_removed_path(state: &WatcherState, rel: &str, full: &Path) {
    match removal_dispatch(
        rel,
        removal_outcome(rel, full, &state.registry, &state.rules),
    ) {
        RemovalDispatch::DecideInLocks => {
            dispatch_removal(state, rel, full, RemovalCause::RemoveEvent)
        }
        RemovalDispatch::Deindex => dispatch_deindex(state, rel),
        RemovalDispatch::Skip => {}
    }
}

/// rename event の両端が処理対象かどうかから、取るべき動作を決める。
#[derive(Debug, PartialEq, Eq)]
enum RenameAction {
    /// 両端とも対象 = DB のパスを付け替える。
    Rename,
    /// 対象領域から除外領域へ出て行った = 追跡をやめる。
    Deindex,
    /// 除外領域から対象領域へ入ってきた = 新規として取り込む。
    Reindex,
    /// 両端とも対象外 = 何もしない。
    Skip,
}

/// [`RenameAction`] の決定。純関数として切り出してあるのは、以前ここが
/// 「どちらか一方が対象なら `dispatch_rename`」だったため、index 済み
/// ファイルを `.git/` や `node_modules/` へ rename すると **denylist 配下の
/// 新パスへ DB を書き換えて残していた** から (codex P2 on PR #81)。
fn rename_action(old_ok: bool, new_ok: bool) -> RenameAction {
    match (old_ok, new_ok) {
        (true, true) => RenameAction::Rename,
        (true, false) => RenameAction::Deindex,
        (false, true) => RenameAction::Reindex,
        (false, false) => RenameAction::Skip,
    }
}

/// The [`RenameAction`] for a rename event, both ends judged by [`gate_parts`].
///
/// (ADR-0023) The old end is judged as a removal: all it decides is whether
/// the existing row is moved or dropped, and nothing is written under the old
/// name. So a row an older version stored under a name the index no longer
/// holds (say `CON.md`) still follows a rename to `console.md`, the remedy
/// the refusal itself suggests -- even when a file under the old name exists
/// again by the time the debounced rename is handled (codex P2 on PR #322).
/// The new end is a write and keeps the name check, so a bad name still
/// cannot enter the index.
fn rename_decision(
    old_rel: &str,
    from: &Path,
    new_rel: &str,
    to: &Path,
    registry: &Registry,
    rules: &crate::exclusion::ExclusionRules,
) -> RenameAction {
    rename_action(
        deindex_passes(old_rel, from, registry, rules),
        should_process_parts(new_rel, to, registry, rules),
    )
}

/// Whether a remove event's path is deindexed: [`gate_parts`] as a removal,
/// which only ever deletes a row, so a name the index cannot hold is no reason
/// to keep one (ADR-0023).
fn deindex_passes(
    rel: &str,
    full: &Path,
    registry: &Registry,
    rules: &crate::exclusion::ExclusionRules,
) -> bool {
    gate_parts(rel, full, registry, rules, Gate::Removal)
}

/// What a remove event (`Remove`, or a `Modify(Name(From))` that arrived unpaired) does to
/// its path. See [`removal_outcome`].
#[derive(Debug, PartialEq, Eq)]
enum RemovalAction {
    /// An indexable file is at the path again: reindex it in place.
    Reindex,
    /// Drop the row.
    Deindex,
    /// Neither: the path is not one the index tracks.
    Skip,
}

/// (#543) A remove event whose path holds an indexable regular file again by the time the
/// event is handled is a reindex, not a deindex.
///
/// An editor that saves by removing the file and writing a new one (Claude Code's Edit tool
/// arrives as a `Remove` and a `Create` of the same path) used to have its row deleted first,
/// so the reindex that followed had no stored vector to reuse (feature-69) and embedded every
/// chunk again. Reindexing in place keeps the row, so the unchanged chunks keep their vectors;
/// the `Create` that follows then finds the hash unchanged and does nothing. The disk is read
/// when the event is handled rather than trusting the batch to also carry the `Create`, so it
/// holds when the debouncer emits the removal on its own (the write landed after the removal
/// queued, but before it was handled).
///
/// The write is judged by the same gate as any reindex ([`should_process_parts`]), on a regular
/// file only: a symlink, a hard link, an excluded path or a directory is not reindexed here. A
/// path with a link on the way passes this gate as it does for any reindex, and the single-file
/// read refuses it and drops its row (AX-02). Only a key the index can hold is
/// considered for it, so a key written by an older version under a name this one refuses
/// ([`Gate::Removal`], ADR-0023) still goes, without a refusal warning on every delete.
/// Anything not reindexed takes the removal gate exactly as before ([`deindex_passes`]).
///
/// (local Codex r1, high) The file must be there under the very spelling of `rel`
/// ([`path_on_disk`]): on a case-insensitive volume a case-only rename leaves the
/// old name opening the new file, and reindexing it would keep a row under the old spelling
/// beside the one the new spelling's event writes. (local Codex r6, medium) That holds for
/// every directory of `rel` on the way as well as for the leaf: a case-only directory rename
/// leaves the old directory name opening the new directory. This is a first look, outside the locks;
/// [`dispatch_reindex_after_removal`] looks again inside them.
///
/// (#543, local Codex r3, medium) Only a file found under the exact spelling
/// ([`OnDisk::Present`]) is reindexed. A look that cannot tell ([`OnDisk::Unknown`]: the stat
/// or the listing failed with anything but "not there") takes the removal gate.
///
/// (kuriya 548) What the event loop does with that answer is [`removal_dispatch`]'s: an
/// unknown that ends in [`RemovalAction::Deindex`] no longer drops the row here. It is looked
/// at again with both locks held ([`dispatch_reindex_after_removal`]), and a look there that
/// still cannot tell leaves the row as it was and says why, as a write does (D3) and as
/// `groove index` does (a stat that fails is a skip, a directory that cannot be listed stops
/// the run before its sweep). kuriya 543 r3 dropped it, since no later event may come to drop the
/// row of a file that is in fact gone; such a row now stays until the path's next event or a
/// `groove index` run that can see the file is gone.
///
/// The same holds for a look outside that found the path gone: that deindex too is looked at
/// again inside the locks ([`removal_dispatch`]), so a file written there again in between
/// keeps its row. Only a name the index cannot hold is dropped without that second look. A path
/// still gone on the second look is dropped under the database lock alone, with no embedder
/// lock ([`dispatch_removal`]).
///
/// (local Codex r7, medium) The look is taken once ([`removal_look`]) and
/// [`removal_outcome_for`] returns its answer with the action: the kind of an unknown that
/// ends in a deindex. (kuriya 548) That kind no longer reaches any line: [`removal_dispatch`]
/// reads only whether one is there (`Some(_)`) as the cue to look again inside the locks, and
/// the kind a line names is the one of the look taken there ([`decide_removal_in_locks`],
/// [`log_removed_path_left_indexed`]).
fn removal_outcome(
    rel: &str,
    full: &Path,
    registry: &Registry,
    rules: &crate::exclusion::ExclusionRules,
) -> (RemovalAction, Option<std::io::ErrorKind>) {
    removal_outcome_for(rel, full, registry, rules, removal_look(rel, full))
}

/// The action alone of [`removal_outcome`], which is how the tests written before the kind
/// was kept ask for it.
#[cfg(test)]
fn removal_decision(
    rel: &str,
    full: &Path,
    registry: &Registry,
    rules: &crate::exclusion::ExclusionRules,
) -> RemovalAction {
    removal_outcome(rel, full, registry, rules).0
}

/// The look [`removal_outcome`] takes, outside the locks: [`path_on_disk`] from the directory
/// that holds `full`, or [`OnDisk::Gone`] when there is none.
fn removal_look(rel: &str, full: &Path) -> OnDisk {
    match full.parent() {
        Some(dir) => path_on_disk(dir, rel),
        None => OnDisk::Gone,
    }
}

/// (#543, local Codex r7, medium) [`removal_decision_for`], plus the kind of the failed look
/// when an [`OnDisk::Unknown`] ends in [`RemovalAction::Deindex`]. `None` for every other
/// answer. (kuriya 548) The event loop reads a kind as "look again inside the locks"
/// ([`removal_dispatch`]) rather than as a deindex; the line names the kind of the look taken
/// there.
fn removal_outcome_for(
    rel: &str,
    full: &Path,
    registry: &Registry,
    rules: &crate::exclusion::ExclusionRules,
    on_disk: OnDisk,
) -> (RemovalAction, Option<std::io::ErrorKind>) {
    let action = removal_decision_for(rel, full, registry, rules, on_disk);
    let kind = match (&action, on_disk) {
        (RemovalAction::Deindex, OnDisk::Unknown(kind)) => Some(kind),
        _ => None,
    };
    (action, kind)
}

/// The action of [`removal_outcome`] once the disk has been looked at, split out so a test can hand it an
/// [`OnDisk::Unknown`] that no file system makes on demand.
///
/// The reindex asks [`crate::resources::doc_is_addressable`], not
/// [`crate::resources::rel_is_indexable_name`]: whether `rel` may be written as a row is a
/// different question, and the empty key (the knowledge base itself) never can be.
///
/// (kuriya 566 R7) The path is looked at once ([`PathFacts::of`]) and both gates are asked of
/// that one look ([`removal_decision_given`]).
fn removal_decision_for(
    rel: &str,
    full: &Path,
    registry: &Registry,
    rules: &crate::exclusion::ExclusionRules,
    on_disk: OnDisk,
) -> RemovalAction {
    removal_decision_given(rel, &PathFacts::of(full), registry, rules, on_disk)
}

/// [`removal_decision_for`] on facts already taken: the write gate and the removal gate read
/// the same look, so they cannot disagree about what is at the path (R7).
fn removal_decision_given(
    rel: &str,
    facts: &PathFacts<'_>,
    registry: &Registry,
    rules: &crate::exclusion::ExclusionRules,
    on_disk: OnDisk,
) -> RemovalAction {
    if on_disk == OnDisk::Present
        && crate::resources::doc_is_addressable(rel)
        && gate_given(rel, facts, registry, rules, Gate::Write)
    {
        return RemovalAction::Reindex;
    }
    if gate_given(rel, facts, registry, rules, Gate::Removal) {
        RemovalAction::Deindex
    } else {
        RemovalAction::Skip
    }
}

/// What the event loop does with a remove event's path, given [`removal_outcome`]'s answer.
#[derive(Debug, PartialEq, Eq)]
enum RemovalDispatch {
    /// Look at the path again and decide on that look ([`dispatch_removal`]): under the
    /// database lock alone while it is still gone, with both locks once a file is there or the
    /// look cannot tell ([`dispatch_reindex_after_removal`]).
    DecideInLocks,
    /// A name the index cannot hold (for example `a//doc.md`), which the removal gate passes
    /// early whatever is on disk (ADR-0023): drop the row ([`dispatch_deindex`]) without
    /// looking again. A path found gone outside the locks is not this: it goes inside them.
    Deindex,
    /// The path is not one the index tracks.
    Skip,
}

/// (kuriya 548) The event loop's reading of [`removal_outcome`] for `rel`. A reindex and a
/// deindex both go inside the locks ([`dispatch_removal`]), where the path is looked at again
/// and [`removal_decision_for`] is asked again on that look ([`decide_removal_in_locks`]): a look
/// outside that could not tell (the kind is there), and as much a look outside that found the
/// path gone, since a file written there again before the locks are taken (its create folded
/// away by the debouncer) would otherwise lose its row until the next full index. Only a
/// deindex of a name the index cannot hold is settled here ([`removal_gate_passes_by_name`]):
/// the removal gate passes it early whatever is on disk (ADR-0023), so a second look could not
/// change the answer. A path the removal gate turned away is a skip. Split out so a test can
/// pin that no look outside the locks reaches [`dispatch_deindex`].
fn removal_dispatch(
    rel: &str,
    outcome: (RemovalAction, Option<std::io::ErrorKind>),
) -> RemovalDispatch {
    match outcome {
        (RemovalAction::Deindex, None) if removal_gate_passes_by_name(rel) => {
            RemovalDispatch::Deindex
        }
        (RemovalAction::Reindex | RemovalAction::Deindex, _) => RemovalDispatch::DecideInLocks,
        (RemovalAction::Skip, _) => RemovalDispatch::Skip,
    }
}

/// (ADR-0023) Whether the removal gate passes `rel` on its name alone, before it looks at
/// the disk: a name the index cannot hold. [`gate_parts`] calls this first for
/// [`Gate::Removal`], and [`removal_dispatch`] calls it to skip the look inside the locks, so
/// the two answer from one test; the empty key (the knowledge base itself) is not one.
fn removal_gate_passes_by_name(rel: &str) -> bool {
    !crate::resources::rel_is_indexable_name(rel)
}

/// Why the row of a path is being decided with the locks held.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RemovalCause {
    /// A remove event named the path: the removal gate decides whether the index tracks it.
    RemoveEvent,
    /// (kuriya 259) A rename took the row away from this path: the old end of a file rename the
    /// gates sent to a deindex, or a row listed under a renamed directory. The caller has decided
    /// the row leaves, so a removal gate that turns the path away does not keep it; only a file
    /// back at the path that the write gate takes keeps it (reindexed in place), and only while
    /// the row is still there: a path whose row is already gone (moved by the rename) is not
    /// looked at, and a file back there is indexed by its own event.
    RenamedAway,
}

/// What [`decide_removal_in_locks`] did.
#[derive(Debug)]
enum RemovalInLocks {
    /// An indexable file is at the path and the write gate takes it: the reindex goes ahead.
    Reindex,
    /// The path is gone, or holds a file under a name the index cannot hold: the row under
    /// `rel` and its folded legacy row were deleted, and the result says whether either was
    /// there.
    Deindexed(Result<bool>),
    /// The look could not tell: nothing was touched, not even the folded row (nothing is known
    /// about the file). `held_row` says whether a row was under `rel` to keep.
    LeftIndexed {
        kind: std::io::ErrorKind,
        held_row: Result<bool>,
    },
    /// The removal gate turns the path away: nothing to do.
    Skip,
}

/// (kuriya 548) What a remove event does with its path once both locks are held and the disk
/// has been looked at there (`on_disk`): [`removal_decision_for`] asked again on that look, so
/// a file found inside the locks is reindexed only when the write gate takes it, whatever the
/// look outside found. Then:
///
/// - a reindex is [`RemovalInLocks::Reindex`]
/// - a deindex on [`OnDisk::Gone`], or on [`OnDisk::Present`] (a file under a name the index
///   cannot hold, which the removal gate passes early, ADR-0023; or, kuriya 566, a hard link,
///   which the write gate refuses and the removal gate passes), drops the rows
///   ([`deindex_removed_path`])
/// - a deindex on [`OnDisk::Unknown`] leaves the rows as they were, as a write keeps them (D3)
///   and as `groove index` keeps them; before kuriya 548 they went
/// - a skip (the removal gate turns the path away: an excluded path, a wrong extension) does
///   nothing
///
/// (local Codex r2, medium) [`remove_if_still_gone`] calls this too, under the database lock
/// alone, for a look that found the path gone: then only the deindex or the skip can come
/// back, and neither needs the embedder.
///
/// (kuriya 259) `cause` says why the row is being decided: for [`RemovalCause::RenamedAway`] a
/// skip from the removal gate is read as a deindex (the rename has decided the row leaves), on
/// the same one look; every other answer is taken as it is.
///
/// Split out of the dispatch so a test can run it with no [`crate::embedder::Embedder`] and an
/// [`OnDisk::Unknown`] that no file system makes on demand.
#[allow(clippy::too_many_arguments)]
fn decide_removal_in_locks(
    db: &Database,
    kb_path: &Path,
    rel: &str,
    full: &Path,
    registry: &Registry,
    rules: &crate::exclusion::ExclusionRules,
    on_disk: OnDisk,
    cause: RemovalCause,
) -> RemovalInLocks {
    let action = match removal_decision_for(rel, full, registry, rules, on_disk) {
        RemovalAction::Skip if cause == RemovalCause::RenamedAway => RemovalAction::Deindex,
        action => action,
    };
    match action {
        RemovalAction::Reindex => RemovalInLocks::Reindex,
        RemovalAction::Skip => RemovalInLocks::Skip,
        RemovalAction::Deindex => match on_disk {
            OnDisk::Unknown(kind) => RemovalInLocks::LeftIndexed {
                kind,
                held_row: db.get_document_hash(rel).map(|hash| hash.is_some()),
            },
            OnDisk::Gone | OnDisk::Present => {
                RemovalInLocks::Deindexed(deindex_removed_path(db, kb_path, rel))
            }
        },
    }
}

/// What [`remove_if_still_gone`] did under the database lock alone.
#[derive(Debug)]
enum RemovalUnderDatabaseLock {
    /// The path is still gone: the row under `rel` and its folded legacy row were deleted, and
    /// the result says whether either was there.
    Deindexed(Result<bool>),
    /// The path is still gone and the removal gate turns it away: nothing to do.
    Skip,
    /// A file is at the path or the look could not tell: nothing was touched, and the removal
    /// is decided again with both locks held ([`dispatch_reindex_after_removal`]).
    NeedsBothLocks,
    /// (kuriya 259) A rename took the row away and no row is under `rel` any more: nothing was
    /// looked at; only the folded legacy row was swept (the result is that sweep's).
    NoRowLeft(Result<bool>),
}

/// (local Codex r2, medium) The part of a removal that needs no embedder, for a caller holding
/// the database lock and nothing else. `on_disk` is the look taken under that lock, right before
/// the delete. A path still gone is settled here by [`decide_removal_in_locks`] (its rows go, or
/// the removal gate turns it away); a file there or a look that cannot tell may need a reindex,
/// so it is handed on untouched.
///
/// The look that decides a delete is the one taken under the database lock: every writer of a
/// document's rows (a reindex, a rename, another deindex) writes under that lock, so nothing
/// writes the row between the look and the delete. The embedder lock adds nothing to that, and
/// taking it first made a deletion wait for a search's embedding. [`sweep_source_if_gone`] takes
/// the same shape.
///
/// Split out of the dispatch so a test can run it with no [`crate::embedder::Embedder`].
#[allow(clippy::too_many_arguments)]
fn remove_if_still_gone(
    db: &Database,
    kb_path: &Path,
    rel: &str,
    full: &Path,
    registry: &Registry,
    rules: &crate::exclusion::ExclusionRules,
    on_disk: OnDisk,
    cause: RemovalCause,
) -> RemovalUnderDatabaseLock {
    if on_disk != OnDisk::Gone {
        return RemovalUnderDatabaseLock::NeedsBothLocks;
    }
    match decide_removal_in_locks(db, kb_path, rel, full, registry, rules, on_disk, cause) {
        RemovalInLocks::Deindexed(outcome) => RemovalUnderDatabaseLock::Deindexed(outcome),
        RemovalInLocks::Skip => RemovalUnderDatabaseLock::Skip,
        // A gone path is never reindexed nor left on an unknown look; named rather than
        // caught so a new variant stays a compile error.
        RemovalInLocks::Reindex | RemovalInLocks::LeftIndexed { .. } => {
            RemovalUnderDatabaseLock::NeedsBothLocks
        }
    }
}

/// (local Codex r2, medium) What the event loop does for [`RemovalDispatch::DecideInLocks`].
/// First the database lock alone: the path is looked at again under it
/// ([`removed_path_on_disk`]) and, while it is still gone, its rows go there
/// ([`remove_if_still_gone`]), so an ordinary deletion never waits on the embedder lock that a
/// search holds through a slow embedding. Only when the look finds a file or cannot tell is
/// that lock released and both locks taken, embedder first and database second as everywhere
/// else ([`dispatch_reindex_after_removal`]), where the path is looked at once more and the
/// decision asked again. Never the embedder lock while holding the database lock: that order
/// would deadlock against every other reindex.
///
/// (kuriya 259) `cause` goes with the decision both under the database lock and with both
/// locks; for [`RemovalCause::RenamedAway`] a path with no row under `rel` any more is not
/// looked at, and only its folded legacy row is swept.
fn dispatch_removal(state: &WatcherState, rel: &str, full: &Path, cause: RemovalCause) {
    let under_db = {
        let db = recover_db(state.lock_for_batch(&state.db));
        if cause == RemovalCause::RenamedAway && matches!(db.get_document_hash(rel), Ok(None)) {
            RemovalUnderDatabaseLock::NoRowLeft(indexer::sweep_legacy_folded_row(
                &db,
                &state.kb_path,
                rel,
                false,
            ))
        } else {
            let on_disk = removed_path_on_disk(&state.kb_path, rel);
            remove_if_still_gone(
                &db,
                &state.kb_path,
                rel,
                full,
                &state.registry,
                &state.rules,
                on_disk,
                cause,
            )
        }
        // `db` is dropped here, before the embedder lock can be taken.
    };
    match under_db {
        RemovalUnderDatabaseLock::Deindexed(outcome) => log_removed_path_deindexed(rel, outcome),
        RemovalUnderDatabaseLock::Skip => {}
        RemovalUnderDatabaseLock::NeedsBothLocks => {
            dispatch_reindex_after_removal(state, rel, full, cause)
        }
        RemovalUnderDatabaseLock::NoRowLeft(outcome) => log_folded_sweep(rel, outcome),
    }
}

/// (#543, local Codex r2, medium) What a look at the disk found for a path a remove event
/// named: three answers, because a failed look is not a missing file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OnDisk {
    /// A regular file under the exact spelling asked for, every directory on the way included.
    Present,
    /// Nothing there, something other than a regular file there, or the file there only
    /// under another case of its name or of a directory on the way.
    Gone,
    /// The look failed in a way that does not say the file is gone (access denied, an I/O
    /// error). (kuriya 548) A remove event leaves the row as it was on this answer, as a write
    /// does ([`decide_removal_in_locks`]); the kind is kept for the line that says why.
    Unknown(std::io::ErrorKind),
}

/// (#543, local Codex r2, medium) What a failed stat or listing says about the path: only
/// "not found" and "not a directory" (a file where a directory on the way was) say it is gone.
/// Anything else, permission denied or an I/O error among them, says nothing about whether
/// the file is still there. (#544) The question is [`indexer::stat_error_means_gone`]'s, the
/// one the single-file paths ask too.
fn probe_error_on_disk(kind: std::io::ErrorKind) -> OnDisk {
    if indexer::stat_error_means_gone(kind) {
        OnDisk::Gone
    } else {
        OnDisk::Unknown(kind)
    }
}

/// Whether `leaf` under the directory given is a regular file under exactly that spelling
/// ([`spelled_on_disk`]), with a failed stat read through [`probe_error_on_disk`].
fn file_on_disk(dir: &Path, leaf: &str) -> OnDisk {
    match std::fs::symlink_metadata(dir.join(leaf)) {
        Ok(meta) if meta.file_type().is_file() => spelled_on_disk(dir, leaf),
        Ok(_) => OnDisk::Gone,
        Err(e) => probe_error_on_disk(e.kind()),
    }
}

/// (#543, local Codex r6, medium) What is at `rel`, given the directory its leaf sits in:
/// [`file_on_disk`] for the leaf, then, only when that found the file under its exact name,
/// [`spelled_on_disk`] for each directory segment of `rel` from the innermost out, each
/// against the directory above it (reached by `Path::parent`, so a verbatim path keeps its
/// prefix). The first answer other than [`OnDisk::Present`] is the answer: a directory held on
/// disk only under another case is [`OnDisk::Gone`], a listing that failed is
/// [`OnDisk::Unknown`]. The directory `rel` is relative to is not checked: it is configured,
/// and no row key holds it.
fn path_on_disk(leaf_dir: &Path, rel: &str) -> OnDisk {
    let mut segments = rel.rsplit('/');
    let leaf = segments.next().unwrap_or(rel);
    let found = file_on_disk(leaf_dir, leaf);
    if found != OnDisk::Present {
        return found;
    }
    let mut dir = leaf_dir;
    for segment in segments {
        // `dir` ends in `segment`, so it has a parent; none would mean the path cannot hold
        // `rel` at all.
        let Some(parent) = dir.parent() else {
            return OnDisk::Gone;
        };
        let spelled = spelled_on_disk(parent, segment);
        if spelled != OnDisk::Present {
            return spelled;
        }
        dir = parent;
    }
    OnDisk::Present
}

/// (#543, local Codex r1, high) Whether the directory given holds an entry named exactly
/// `name`, given that `name` under it was just found (a file, or for [`path_on_disk`] a
/// directory on the way to one). On a case-insensitive volume that lookup also succeeds for an
/// entry spelled in another case, which is not the name the row is keyed by.
///
/// No directory listing where the answer is plain: a `name` with no cased letter cannot be
/// matched by another case, and when the case-swapped spelling is not found the directory
/// tells the two apart (a case-sensitive file system: Linux, as a rule), so the exact name is
/// the one that was found. Otherwise the parent is listed and its names compared byte for
/// byte: [`OnDisk::Present`] when the exact name is listed, [`OnDisk::Gone`] when the listing
/// went through without it. (local Codex r2, medium) [`OnDisk::Unknown`] when the listing,
/// or an entry of it, failed and the exact name was not among what was read.
fn spelled_on_disk(dir: &Path, name: &str) -> OnDisk {
    let lower = name.to_lowercase();
    let swapped = if lower != name {
        lower
    } else {
        name.to_uppercase()
    };
    if swapped == name {
        return OnDisk::Present;
    }
    if std::fs::symlink_metadata(dir.join(&swapped))
        .is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound)
    {
        return OnDisk::Present;
    }
    #[cfg(test)]
    LISTINGS.with(|n| n.set(n.get() + 1));
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) => return OnDisk::Unknown(e.kind()),
    };
    let mut failed = None;
    for entry in entries {
        match entry {
            Ok(entry) if entry.file_name() == std::ffi::OsStr::new(name) => {
                return OnDisk::Present;
            }
            Ok(_) => {}
            Err(e) => failed = Some(e.kind()),
        }
    }
    failed.map_or(OnDisk::Gone, OnDisk::Unknown)
}

// (#544) How many directories [`spelled_on_disk`] listed on this thread, for the tests that pin
// what a look costs. Per thread, so tests running side by side do not count each other's.
#[cfg(test)]
thread_local! {
    static LISTINGS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// [`spelled_on_disk`] as a yes or no, for the tests written before it had three answers:
/// only [`OnDisk::Present`] is a yes.
#[cfg(test)]
fn leaf_is_spelled_on_disk(dir: &Path, leaf: &str) -> bool {
    spelled_on_disk(dir, leaf) == OnDisk::Present
}

/// (#543, local Codex r1) What is at the path a remove event was about to reindex
/// ([`path_on_disk`], every segment of `rel` under its exact spelling). Built from `kb_path`
/// one segment of `rel` at a time, so the directory and the leaf split the way `rel` does
/// whatever `kb_path` looks like.
fn removed_path_on_disk(kb_path: &Path, rel: &str) -> OnDisk {
    let mut dir = kb_path.to_path_buf();
    let mut segments = rel.split('/').peekable();
    while let Some(segment) = segments.next() {
        if segments.peek().is_some() {
            dir.push(segment);
        }
    }
    path_on_disk(&dir, rel)
}

/// What [`check_removed_path`] did.
#[cfg(test)]
#[derive(Debug)]
enum RemovedPathCheck {
    /// The path is gone: the rows were deleted, and the result says whether either was there.
    Deindexed(Result<bool>),
    /// The file is still at `rel`: nothing touched, the reindex goes ahead.
    StillThere,
    /// The look could not tell: the rows were deleted as for a path that is gone, no reindex
    /// runs, and the kind says why the look failed.
    Unknown(std::io::ErrorKind, Result<bool>),
}

/// (#543, local Codex r1) The removal [`dispatch_reindex_after_removal`] falls back to, for a
/// caller already holding the database lock: unless the file is still there
/// ([`OnDisk::Present`], [`removed_path_on_disk`]), the row under `rel` and its folded legacy
/// row ([`indexer::sweep_legacy_folded_row`] as a removal) are deleted. Split out so a test
/// can run it with no [`crate::embedder::Embedder`].
///
/// (kuriya 548) Test only now: the dispatch asks [`decide_removal_in_locks`] instead.
#[cfg(test)]
fn check_removed_path(db: &Database, kb_path: &Path, rel: &str) -> RemovedPathCheck {
    act_on_removed_path(db, kb_path, rel, removed_path_on_disk(kb_path, rel))
}

/// [`check_removed_path`] once the disk has been looked at, split out so a test can hand it an
/// [`OnDisk::Unknown`]. (local Codex r3, medium) Anything but [`OnDisk::Present`] deletes the
/// rows, an unknown look included: the remove event is the only signal the file is going, and
/// the removal it carried before this look existed is what it falls back to.
///
/// (kuriya 548) The event loop no longer hands this an unknown look, nor calls it at all:
/// [`decide_removal_in_locks`] leaves the row on an unknown. The unknown arm is what a removal
/// did before kuriya 548, kept with its contract for the tests that pin it.
#[cfg(test)]
fn act_on_removed_path(
    db: &Database,
    kb_path: &Path,
    rel: &str,
    on_disk: OnDisk,
) -> RemovedPathCheck {
    match on_disk {
        OnDisk::Present => RemovedPathCheck::StillThere,
        OnDisk::Unknown(kind) => {
            RemovedPathCheck::Unknown(kind, deindex_removed_path(db, kb_path, rel))
        }
        OnDisk::Gone => RemovedPathCheck::Deindexed(deindex_removed_path(db, kb_path, rel)),
    }
}

/// The deletion a remove event runs for a path not found there ([`decide_removal_in_locks`]):
/// the row under `rel` and its folded legacy row. `Ok(true)` when either was there.
fn deindex_removed_path(db: &Database, kb_path: &Path, rel: &str) -> Result<bool> {
    indexer::deindex_single_file(db, rel).and_then(|exact| {
        let folded = indexer::sweep_legacy_folded_row(db, kb_path, rel, false)?;
        Ok(exact || folded)
    })
}

/// [`check_removed_path`] in the shape the tests written before it had three answers use:
/// `Some` when the rows were deleted, `None` when nothing was touched.
#[cfg(test)]
fn deindex_if_removed_path_is_gone(
    db: &Database,
    kb_path: &Path,
    rel: &str,
) -> Option<Result<bool>> {
    match check_removed_path(db, kb_path, rel) {
        RemovedPathCheck::Deindexed(outcome) | RemovedPathCheck::Unknown(_, outcome) => {
            Some(outcome)
        }
        RemovedPathCheck::StillThere => None,
    }
}

/// (#544) How a create, modify or rename-to looks at its path inside the locks
/// ([`written_path_on_disk`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WriteLook {
    /// Every segment of `rel` under its exact spelling, the look a remove event takes
    /// ([`removed_path_on_disk`]): for a path an event named, whose spelling nothing else has
    /// checked.
    Spelled,
    /// Only whether a regular file is at `rel` (`symlink_metadata`, links not followed), for a
    /// path whose directories were spelled by a walk or by a look taken once for all of them.
    /// No directory is listed.
    ExistsOnly,
}

/// (#544 A1/A3) What is at a path a write is about to (re)index, looked at as `look` says.
/// [`OnDisk::Gone`] for anything but a regular file, and for a failed stat that
/// [`indexer::stat_error_means_gone`] reads as "not there"; [`OnDisk::Unknown`] for any other
/// failed stat or listing.
fn written_path_on_disk(kb_path: &Path, rel: &str, look: WriteLook) -> OnDisk {
    match look {
        WriteLook::Spelled => removed_path_on_disk(kb_path, rel),
        WriteLook::ExistsOnly => match std::fs::symlink_metadata(kb_path.join(rel)) {
            Ok(meta) if meta.file_type().is_file() => OnDisk::Present,
            Ok(_) => OnDisk::Gone,
            Err(e) => probe_error_on_disk(e.kind()),
        },
    }
}

/// (#544 C4) Whether `rel` is a lossy spelling of the path the event named: that path's name
/// below `kb_path` is not UTF-8, so `rel` holds U+FFFD where the name has other bytes
/// ([`indexer::index_rel_path`]) and no look at `rel` can find the file. Asked of the event's
/// own path ([`indexer::index_rel_path_exact`] is `None` for it), never of `rel`: a name may
/// hold a real U+FFFD.
///
/// `None` (no event path at hand) is not lossy. A path not under `kb_path` as spelled is
/// canonicalized once, as [`to_rel`] does; one that still cannot be placed is read as lossy, so
/// a write keeps the row as it did before the look existed.
fn rel_is_lossy(kb_path: &Path, raw: Option<&Path>) -> bool {
    let Some(raw) = raw else {
        return false;
    };
    let lossy_under = |p: &Path| {
        indexer::index_rel_path(kb_path, p)
            .map(|_| indexer::index_rel_path_exact(kb_path, p).is_none())
    };
    lossy_under(raw)
        .or_else(|| raw.canonicalize().ok().and_then(|c| lossy_under(&c)))
        .unwrap_or(true)
}

/// (kuriya 557, local Codex r1 F2 on PR #401) Whether a stored key may be the lossy spelling of
/// a name that is not valid Unicode: it holds U+FFFD, which [`indexer::index_rel_path`] puts in
/// place of such a name's other bytes. Asked where only the key is at hand, with no event path
/// to ask [`rel_is_lossy`] of; a name holding a real U+FFFD gets the same yes, since the key
/// alone cannot tell the two apart.
fn key_may_be_lossy(key: &str) -> bool {
    key.contains(char::REPLACEMENT_CHARACTER)
}

/// What [`act_on_written_path`] did before a write's reindex.
#[derive(Debug)]
enum WrittenPathCheck {
    /// The file is there (or `rel` is lossy and cannot be looked at): the reindex goes ahead.
    Proceed,
    /// The look could not tell: nothing touched, the reindex goes ahead, and the kind says why
    /// the look failed.
    ProceedUnknown(std::io::ErrorKind),
    /// The path is gone under that spelling: the rows under `rel` were deleted (the result says
    /// whether either was there) and no reindex runs.
    Deindexed(Result<bool>),
}

/// (#544 A1/A3, D3) What a write does with the look inside the locks, before its reindex:
///
/// - [`OnDisk::Present`]: go ahead
/// - [`OnDisk::Unknown`]: go ahead and keep the rows. A write is not a signal that the file is
///   going, and an editor holding the file while it saves must not cost the document its row.
///   (kuriya 548) A remove event, which is such a signal, now keeps the rows on this answer
///   too ([`decide_removal_in_locks`])
/// - [`OnDisk::Gone`]: the row under `rel` and its folded legacy row go
///   ([`deindex_removed_path`]), since the event's path is no longer one the index can hold
///   (deleted since, a directory now, or there only under another case). Unless `lossy`
///   ([`rel_is_lossy`]): a lossy `rel` always looks gone, and the row stays as before (C4)
fn act_on_written_path(
    db: &Database,
    kb_path: &Path,
    rel: &str,
    on_disk: OnDisk,
    lossy: bool,
) -> WrittenPathCheck {
    match on_disk {
        OnDisk::Present => WrittenPathCheck::Proceed,
        OnDisk::Unknown(kind) => WrittenPathCheck::ProceedUnknown(kind),
        OnDisk::Gone if lossy => WrittenPathCheck::Proceed,
        OnDisk::Gone => WrittenPathCheck::Deindexed(deindex_removed_path(db, kb_path, rel)),
    }
}

/// (#544 A2/A5) Whether every segment of `rel` -- the directory itself and each one above it,
/// from the innermost out -- is on disk under its exact spelling ([`spelled_on_disk`], each
/// against the directory above it, built from `kb_path` one segment at a time). The first
/// answer other than [`OnDisk::Present`] is the answer. Nothing is asked of what is at `rel`;
/// [`arrived_directory_look`] has already found a directory there.
fn directory_spelled_on_disk(kb_path: &Path, rel: &str) -> OnDisk {
    let mut full = kb_path.to_path_buf();
    for segment in rel.split('/') {
        full.push(segment);
    }
    let mut dir = full.as_path();
    for segment in rel.rsplit('/') {
        let Some(parent) = dir.parent() else {
            return OnDisk::Gone;
        };
        let spelled = spelled_on_disk(parent, segment);
        if spelled != OnDisk::Present {
            return spelled;
        }
        dir = parent;
    }
    OnDisk::Present
}

/// (#544) What is at a directory an event said arrived, `rel` with the event's path `raw`:
/// whether it is a directory (`symlink_metadata` of `raw`, links not followed; `None` for a
/// failed stat) and the look that goes with it. For a directory, the look is
/// [`directory_spelled_on_disk`]; for something else [`OnDisk::Gone`]; for a failed stat, the
/// stat's kind read through [`probe_error_on_disk`]. One stat and one spelling look for the
/// whole directory, however many files it holds.
fn arrived_directory_look(kb_path: &Path, rel: &str, raw: &Path) -> (Option<bool>, OnDisk) {
    match std::fs::symlink_metadata(raw) {
        Ok(meta) if meta.file_type().is_dir() => {
            (Some(true), directory_spelled_on_disk(kb_path, rel))
        }
        Ok(_) => (Some(false), OnDisk::Gone),
        Err(e) => (None, probe_error_on_disk(e.kind())),
    }
}

/// (#544 A12) [`arrived_directory_look`] as one answer: [`OnDisk::Present`] only for a
/// directory at `raw` whose `rel` is on disk under its exact spelling.
fn arrived_directory_on_disk(kb_path: &Path, rel: &str, raw: &Path) -> OnDisk {
    match arrived_directory_look(kb_path, rel, raw) {
        (Some(true), spelled) => spelled,
        (Some(false), _) => OnDisk::Gone,
        (None, look) => look,
    }
}

/// What [`act_on_arrived_directory`] did with a directory that arrived.
#[derive(Debug)]
enum ArrivedDirectory {
    /// The directory is there under its spelling (or its name is lossy and cannot be looked
    /// at): scan it. The result is the deletion of the row a file under the directory's name
    /// left, `Ok(true)` when there was one.
    Scan(Result<bool>),
    /// The path is no longer a directory, or no longer there: nothing done, its own event
    /// comes on its own.
    NoLongerADirectory,
    /// The directory is on disk only under another spelling: no scan, and the rows under the
    /// spelling the event named went, the one at the name itself (`exact`) and every one under
    /// it (`under`).
    Dropped {
        exact: Result<bool>,
        under: Result<Vec<String>>,
    },
    /// The look could not tell: no row touched, and the directory is scanned all the same.
    Unknown(std::io::ErrorKind),
}

/// (#544 A2/A5/A13) What an arrived directory does with the look inside the db lock, before
/// its scan ([`act_on_arrived_directory_for`] once the disk has been looked at,
/// [`arrived_directory_look`]). `raw` is the event's path, which also says whether `rel` is
/// lossy ([`rel_is_lossy`]).
fn act_on_arrived_directory(
    db: &Database,
    kb_path: &Path,
    rel: &str,
    raw: &Path,
) -> ArrivedDirectory {
    let (is_dir, spelled) = arrived_directory_look(kb_path, rel, raw);
    act_on_arrived_directory_for(
        db,
        kb_path,
        rel,
        is_dir,
        spelled,
        rel_is_lossy(kb_path, Some(raw)),
    )
}

/// [`act_on_arrived_directory`] once the disk has been looked at, split out so a test can hand
/// it a look no file system makes on demand. `is_dir` is the stat's answer (`None` when it
/// failed, its kind then in `spelled`):
///
/// - not a directory, or a stat that says the path is gone, gives
///   [`ArrivedDirectory::NoLongerADirectory`]
/// - a stat that could not tell, or a directory whose spelling could not be looked at, gives
///   [`ArrivedDirectory::Unknown`], touching no row (D3)
/// - a lossy `rel` gives [`ArrivedDirectory::Scan`] and touches no row: its look never finds
///   it, and a sibling whose name is lost the same way shares its prefix (C4)
/// - a directory under its exact spelling: the row under its name goes (a file it replaced,
///   with its folded legacy row), then [`ArrivedDirectory::Scan`] (A2)
/// - a directory on disk only under another spelling: the row under its name and every row
///   under `rel/` go, and no scan runs (spec r4 M-d). `documents.path` compares bytes, so any
///   key under that spelling holds a segment the disk does not, and the full index drops it too
fn act_on_arrived_directory_for(
    db: &Database,
    kb_path: &Path,
    rel: &str,
    is_dir: Option<bool>,
    spelled: OnDisk,
    lossy: bool,
) -> ArrivedDirectory {
    match (is_dir, spelled) {
        (Some(false), _) | (None, OnDisk::Gone | OnDisk::Present) => {
            ArrivedDirectory::NoLongerADirectory
        }
        (None | Some(true), OnDisk::Unknown(kind)) => ArrivedDirectory::Unknown(kind),
        // The knowledge base itself names no row.
        (Some(true), _) if lossy || rel.is_empty() => ArrivedDirectory::Scan(Ok(false)),
        (Some(true), OnDisk::Present) => {
            ArrivedDirectory::Scan(deindex_removed_path(db, kb_path, rel))
        }
        (Some(true), OnDisk::Gone) => ArrivedDirectory::Dropped {
            exact: deindex_removed_path(db, kb_path, rel),
            under: indexer::deindex_subtree(db, rel),
        },
    }
}

/// The lines for what [`act_on_arrived_directory`] did; `true` when the directory is to be
/// scanned.
fn log_arrived_directory(rel: &str, arrival: ArrivedDirectory) -> bool {
    match arrival {
        ArrivedDirectory::Scan(exact) => {
            match exact {
                Ok(true) => {
                    wdiag!(
                        "watcher: {rel} is a directory now; deindexed the file indexed under its name"
                    )
                }
                Ok(false) => {}
                Err(e) => wdiag!("watcher: deindex {rel} failed: {e}"),
            }
            true
        }
        ArrivedDirectory::NoLongerADirectory => false,
        ArrivedDirectory::Dropped { exact, under } => {
            let exact = match exact {
                Ok(exact) => usize::from(exact),
                Err(e) => {
                    wdiag!("watcher: deindex {rel} failed: {e}");
                    0
                }
            };
            let under = match under {
                Ok(deleted) => deleted.len(),
                Err(e) => {
                    wdiag!("watcher: deindexing under {rel}/ failed: {e}");
                    0
                }
            };
            if exact + under > 0 {
                wdiag!(
                    "watcher: the directory {rel} is on disk only under another spelling; deindexed {} documents at or under it",
                    exact + under
                );
            }
            false
        }
        ArrivedDirectory::Unknown(kind) => {
            wdiag!("watcher: could not look at the directory {rel} ({kind:?}); scanning it anyway");
            true
        }
    }
}

/// What a path is being judged for, which decides whether the name check
/// ([`name_is_addressable`]) applies. Every other check is the same for both.
///
/// Keyed on what the event does to the row, never on whether the file is
/// there: on Windows a non-verbatim event path through a directory ending in
/// a dot has the dot trimmed by Win32 and can come back with no metadata
/// while naming a file that would be written, and a bad old name can exist
/// again when its rename is handled.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Gate {
    /// The path may be written as a row: reindex, the new end of a rename.
    Write,
    /// The row, if any, is only deleted or moved away: a remove event, the
    /// old end of a rename.
    ///
    /// A key [`crate::resources::doc_is_addressable`] refuses passes at once,
    /// before the exclusion, extension, lock-file, symlink and hard-link
    /// checks: no such row can be one this version wrote, so moving or
    /// dropping it is always right, whatever sits at the path now (a file
    /// recreated there as a link, say). Every other key goes through all of
    /// those checks as before, except that a link at the path passes: a
    /// symlink (a junction on Windows) since kuriya 557, a hard link since
    /// kuriya 566 (U1). No document is held at a link, so this answer is the
    /// one place that decides whether the row of a path now a link goes.
    Removal,
}

/// 対象ファイルの拡張子が `registry` にあり、除外対象でないこと。
///
/// `WatcherState` のうち `registry` / `rules` しか見ないので、判定本体は
/// [`should_process_parts`] に切り出してある (test から `Database` / `Embedder`
/// のダミー構築なしに **本番と同一のロジック** を叩けるようにするため)。
///
/// (kuriya 566) Asked of facts already taken, so the event loop can ask the same look whether
/// a refused path is a link ([`PathFacts`]).
fn should_process(rel: &str, facts: &PathFacts<'_>, state: &WatcherState) -> bool {
    gate_given(rel, facts, &state.registry, &state.rules, Gate::Write)
}

/// [`should_process`] の判定本体 ([`Gate::Write`] としての [`gate_parts`])。
fn should_process_parts(
    rel: &str,
    full: &Path,
    registry: &Registry,
    rules: &crate::exclusion::ExclusionRules,
) -> bool {
    gate_parts(rel, full, registry, rules, Gate::Write)
}

/// What kind of entry one look at a path found ([`PathFacts`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PathKind {
    /// `symlink_metadata` failed: nothing there, as a rule a removal.
    Missing,
    Dir,
    /// A symlink, or on Windows any name surrogate (a junction among them).
    Symlink,
    /// A regular file, or anything else that is neither of the above.
    Other,
}

/// (kuriya 566 R7 / #550) What one look at `full` found, for every gate asked of it in one
/// decision: one `symlink_metadata`, and the link count read at most once, only when a gate
/// asks for it. [`removal_decision_for`] asks the write gate and the removal gate of the same
/// facts, so the two cannot see two different files at the path (R7: one saw a hard link, the
/// other a regular file, and a file there kept by neither lost its row).
struct PathFacts<'a> {
    full: &'a Path,
    kind: PathKind,
    meta: Option<std::fs::Metadata>,
    links: std::cell::OnceCell<Option<u64>>,
}

impl<'a> PathFacts<'a> {
    /// One `symlink_metadata` of `full`, links not followed.
    fn of(full: &'a Path) -> Self {
        #[cfg(test)]
        FACTS_TAKEN.with(|n| n.set(n.get() + 1));
        let meta = std::fs::symlink_metadata(full).ok();
        let kind = match &meta {
            None => PathKind::Missing,
            Some(m) if m.file_type().is_dir() => PathKind::Dir,
            Some(m) if m.file_type().is_symlink() => PathKind::Symlink,
            Some(_) => PathKind::Other,
        };
        Self {
            full,
            kind,
            meta,
            links: std::cell::OnceCell::new(),
        }
    }

    /// Facts handed in rather than read from the disk, for a test that needs what no file
    /// system here makes on demand (a directory with a link count of 3 on Windows, say).
    #[cfg(test)]
    fn given(full: &'a Path, kind: PathKind, links: Option<u64>) -> Self {
        let cell = std::cell::OnceCell::new();
        let _ = cell.set(links);
        Self {
            full,
            kind,
            meta: None,
            links: cell,
        }
    }

    fn is_dir(&self) -> bool {
        self.kind == PathKind::Dir
    }

    fn is_symlink(&self) -> bool {
        self.kind == PathKind::Symlink
    }

    /// The link count, read once ([`crate::links::link_count_given`]: from the metadata on
    /// Unix, from one open on Windows). `None` when it cannot be read, nothing there included.
    fn link_count(&self) -> Option<u64> {
        *self.links.get_or_init(|| {
            self.meta
                .as_ref()
                .and_then(|m| crate::links::link_count_given(m, self.full))
        })
    }

    /// (#550) Whether the file at the path has more than one name. Never for a directory or a
    /// symlink: every directory on Linux and macOS counts `.` and its subdirectories' `..` as
    /// links, which says nothing about a second name, and a symlink is judged as a link on its
    /// own. Fail-open as [`crate::links::is_multiply_linked`] is: no count is no.
    fn is_multiply_linked(&self) -> bool {
        match self.kind {
            PathKind::Dir | PathKind::Symlink | PathKind::Missing => false,
            PathKind::Other => self.link_count().is_some_and(|count| count > 1),
        }
    }
}

// (kuriya 566 R7) How many times [`PathFacts::of`] looked at a path on this thread, for the
// test that pins one look per removal decision. Per thread, as `LISTINGS` is.
#[cfg(test)]
thread_local! {
    static FACTS_TAKEN: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// [`should_process_parts`] / [`deindex_passes`] / [`rename_decision`] の判定本体:
/// path を 1 度見て ([`PathFacts::of`]) [`gate_given`] に聞く。
fn gate_parts(
    rel: &str,
    full: &Path,
    registry: &Registry,
    rules: &crate::exclusion::ExclusionRules,
    gate: Gate,
) -> bool {
    gate_given(rel, &PathFacts::of(full), registry, rules, gate)
}

/// [`gate_parts`] on facts already taken, so one decision that asks both gates
/// ([`removal_decision_for`]) reads the path once (kuriya 566 R7).
fn gate_given(
    rel: &str,
    facts: &PathFacts<'_>,
    registry: &Registry,
    rules: &crate::exclusion::ExclusionRules,
    gate: Gate,
) -> bool {
    let full = facts.full;
    // (ADR-0023) See [`Gate::Removal`]. The empty key is the knowledge base
    // itself, which names no row and takes the ordinary path. The name test is
    // the one [`removal_dispatch`] asks too ([`removal_gate_passes_by_name`]),
    // so the gate and the dispatcher cannot drift apart.
    if gate == Gate::Removal && removal_gate_passes_by_name(rel) {
        return true;
    }
    // 1 回の look ([`PathFacts`]) を全部の判定で使い回す: ディレクトリかどうか (末尾
    // スラッシュの `.grooveignore` パターンの効き方が変わる)、symlink かどうか、link 数。
    // `symlink_metadata` が失敗するのは対象が既に無い時 = 削除イベントで、
    // 下の判定はどれもその場合 **通す** (この関数は deindex も gate する)。
    let is_dir = facts.is_dir();

    // (feature-49) 除外の 3 層 — hardcoded denylist (`.git` / `node_modules`) /
    // user `exclude_dirs` / `.grooveignore` — をここで 1 回だけ判定する。
    //
    // AU-03 は watcher だけ denylist を持っておらず、`exclude_dirs` を絞った
    // 設定で **live watcher だけが** `.git/` や `node_modules/` を index して
    // いた件 (`npm install` で KB 汚染)。BU-19 は index walk 側だけ大文字小文字を
    // 無視するようにして、`exclude_dirs = ["build"]` + ディスク上 `Build/` で
    // **full index は skip するのに watcher が増分 index する**食い違いを作った件。
    // どちらも同じ判断が 2 実装に分かれていたのが原因なので、3 層目を足すに
    // あたって判定そのものを [`crate::exclusion::ExclusionRules`] の 1 メソッドに
    // 集約した。index walk と `validate` walk も同じものを呼ぶ。
    if rules.is_excluded(rel, is_dir) {
        return false;
    }
    // `.groove.db*` は kb_path の外にあるので通常ヒットしないが念のため
    if rel.ends_with(".groove.db") || rel.ends_with(".groove.db-journal") {
        return false;
    }
    // symlink は index しない。full re-index 側は `collect_source_files` が
    // `follow_links(false)` + `is_file()` で落としており (indexer.rs)、
    // `get_document` も `validate_get_document_path` で明示的に拒否している。
    // **live watcher だけが同じ制御を持っていなかった** (AU-03 と同型の再発、
    // full-audit 2026-08-12 セキュリティ軸 H-1)。KB に書ける者が
    // `notes.md -> ~/.ssh/id_rsa` を張ると、`fs::read` が link を辿るので
    // 中身が chunk 化・embed され `search` から平文で読めてしまう。
    //
    // `symlink_metadata` が失敗した場合は**通す**。削除イベントではファイルが
    // 既に無く、ここで弾くと deindex が動かなくなるため (この関数は
    // Reindex / Deindex の両方を gate している)。上で取った 1 回ぶんを使う。
    //
    // (kuriya 557, GitHub codex r1 P2 on PR #401) A link is refused as a write
    // only. As a removal it passes: the index holds no document at a link (the
    // full walk skips it, and a write at one drops the row,
    // [`deindex_under_a_link_parts`]), so a file replaced by a link loses its
    // row rather than keeping its old text. The caller decides the delete on a
    // look taken under the database lock ([`decide_removal_in_locks`]), and the
    // decision there takes its own look ([`removal_decision_for`]).
    if facts.is_symlink() {
        return gate == Gate::Removal;
    }
    // Office lock/owner file (~$*.docx / .~lock.*#) は collect_source_files
    // (フル re-index) と同じ判定で skip する。放置すると Office ファイルを
    // 開くたびに create イベント → parse 失敗 warn がスパムする。
    let name = full.file_name().unwrap_or_default().to_string_lossy();
    if indexer::is_office_lock_file(name.as_ref()) {
        return false;
    }
    let ext = full.extension().and_then(|e| e.to_str()).unwrap_or("");
    if !registry
        .extensions()
        .iter()
        .any(|e| e.eq_ignore_ascii_case(ext))
    {
        return false;
    }
    // (ADR-0023) Below the extension filter, as in the walk, so only a file
    // that would otherwise have been indexed is named in a warning. Only a
    // path that may be written is asked (see [`Gate`]): a rename onto such a
    // name deindexes the old row, while deleting one or renaming it away still
    // reaches a row an older version stored under it. A removal skips no file,
    // so there is nothing to warn of either.
    if gate == Gate::Write && !name_is_addressable(rel, full, is_dir) {
        return false;
    }
    // (BU-20) A hard link is the same attack with no symlink to see: a second
    // name for a file that may live outside the KB, creatable without read
    // access to it and without any privilege on Windows.
    //
    // Last: on Windows the link count needs the file opened, and there is no
    // point opening what the extension filter already rejected.
    //
    // (#550) A directory or a symlink is never asked its link count
    // ([`PathFacts::is_multiply_linked`]): every directory on Linux and macOS
    // has a count of at least two (`.` and `..`), and a directory whose name
    // passes the extension filter (`x.md`) was read here as a hard link, so a
    // removal of `x.md` that found a directory there kept its row on Linux and
    // macOS and dropped it on Windows (which cannot open a directory for the
    // count). The removal gate now asks no count at all (kuriya 566, below).
    //
    // The write gate keeps the answer it gave a directory before #550, from the
    // raw count: a refusal on Linux and macOS, a pass on Windows. Whether the
    // write gate should refuse a directory outright is not this change's call
    // (kuriya 566 plan, S3).
    //
    // Same fail-open rule as the symlink check above: a vanished file has no
    // link count, and this function gates deindexing as well as indexing.
    //
    // (kuriya 566 U1, 2026-10-28) A hard link is refused as a write only, as a
    // symlink is. As a removal it passes without its count being read: the
    // index holds no document at a hard link either (the full walk does not
    // collect one and its deletion pass drops the row), so a file replaced by
    // a hard link loses its row rather than keeping text the index would no
    // longer read. This answer runs only on an event for the path: a file that
    // only gains a second name elsewhere sends none (kuriya 569), so its row
    // waits for the full index. Every watcher path that meets a
    // link hands it to this answer ([`handle_removed_path`],
    // [`decide_removal_in_locks`]) rather than judging the link itself.
    if gate == Gate::Removal {
        return true;
    }
    let linked = facts.is_multiply_linked()
        || (facts.is_dir() && facts.link_count().is_some_and(|count| count > 1));
    if linked {
        wdiag!("watcher: {}", crate::links::refusal_reason(full));
        return false;
    }
    true
}

/// (ADR-0023) Whether the index can hold `rel`, logging the refusal when it
/// cannot: the same predicate the full walk, `get_document` of
/// [`crate::server`] and the `kb://doc/` URI side ask
/// ([`crate::resources::doc_is_addressable`]).
///
/// Judged on `rel` as [`to_rel`] recovered it, never on the raw event path.
/// The walk judges no name under a spelling of the knowledge base other than
/// `kb_path`'s (there is no relative path to judge there), and leaves those to
/// this relativizing. The empty string is the knowledge base itself, which is
/// no document name and is turned away elsewhere.
fn name_is_addressable(rel: &str, full: &Path, is_dir: bool) -> bool {
    if crate::resources::rel_is_indexable_name(rel) {
        return true;
    }
    wdiag!(
        "watcher: {}",
        crate::resources::unspellable_reason(full, is_dir)
    );
    false
}

/// 絶対パスを kb_path 相対 (forward-slash) に変換。kb_path 外ならエラーを
/// ログに出して `None`。
///
/// 綴りそのものは [`indexer::index_rel_path`] が決める (full index の走査と
/// 同じ `documents.path` を書くため)。ここが持つのは canonicalize の再試行だけ。
fn to_rel(kb_path: &Path, full: &Path) -> Option<String> {
    indexer::index_rel_path(kb_path, full).or_else(|| {
        // canonicalize ズレで失敗することがある — 再度 canonicalize して再試行
        full.canonicalize()
            .ok()
            .and_then(|c| indexer::index_rel_path(kb_path, &c))
    })
}

/// Index what a newly appeared directory brought in with it.
///
/// Measured on Ubuntu 22.04 with raw inotify: a file written into a directory
/// created 0.79 ms earlier is already on disk when the earliest possible watch
/// on that directory is registered (2.41 ms), and it is reported by **no**
/// watch — not the parent's, which only names the directory, and not the new
/// one's, which did not exist yet. The event is unobservable rather than late,
/// so no debounce window and no retry recovers it; the only fix is to look.
///
/// Windows is unaffected (`ReadDirectoryChangesW` covers the subtree from one
/// handle), which is why the gap survived until a Linux-only CI failure.
///
/// Filtering is [`crate::indexer::collect_source_files_under`]'s, i.e. the full
/// index walk's, so a directory drop and a later `groove index` agree about
/// what belongs in the index -- plus the one name check the walk leaves to the
/// watcher ([`new_directory_entries`]). Re-checking with [`should_process`] here would be
/// a second implementation of the same question, which is exactly how the
/// watcher and the walk drifted apart twice before (AU-03, BU-19).
fn dispatch_new_directory(state: &WatcherState, dir: &Path, rel: &str) {
    let found = match new_directory_entries(&state.kb_path, dir, &state.registry, &state.rules) {
        Ok(found) => found,
        Err(e) => {
            // Not retried, and the consequence is stated rather than implied
            // (codex P2 on PR #168 asked for a retry). `collect_source_files_under`
            // fails the whole walk on one unreadable entry, and it is shared with
            // the full index, where failing loudly is the right answer — returning
            // partial results would change *that* contract to hide a broken tree.
            // Retrying inside the event loop would block it for an unbounded time
            // on a directory that may still be being written. The recoverable
            // truth is that a full index fixes it, so the message says so.
            wdiag!(
                "watcher: could not scan the new directory {rel}: {e}. \
                 Its files stay unindexed until the next full `groove index` (or this server's MCP `rebuild_index`)."
            );
            return;
        }
    };
    hand_on_rows_the_walk_left_behind(state, rel, &found);
    if found.is_empty() {
        return;
    }
    // Say how many, so a directory drop is never a silent bulk index.
    //
    // The directory is deliberately **not** named here (codex P1 on PR #168):
    // stderr is meant to stay ASCII (AGENTS.md), a knowledge base path is
    // routinely not, and `dispatch_reindex` names every file on the very next
    // lines — so interpolating it would buy nothing and cost mojibake on a
    // CP932 console. Where a path is the only thing the message has to say, it
    // is still printed, the way the other watcher diagnostics do; making this
    // one call site escape differently is the "one question, two
    // implementations" shape the same file already got burned by twice.
    wdiag!(
        "watcher: a new directory arrived with {} indexable file{}; indexing \
         them (their own events are not delivered on Linux).",
        found.len(),
        if found.len() == 1 { "" } else { "s" }
    );
    // (#544) Each file only asks whether a regular file is at its `rel`: the walk spelled
    // the directories on the way.
    for (frel, f) in &found {
        dispatch_reindex_from(
            state,
            frel,
            ReindexOrigin::Write {
                raw: Some(f),
                look: WriteLook::ExistsOnly,
            },
        );
    }
}

/// The relative paths [`dispatch_new_directory`] reindexes: the walk's files
/// under `dir`, relativized by [`to_rel`], minus a name the index cannot hold.
///
/// (ADR-0023) The walk already leaves such names out under `kb_path` as
/// spelled, but an event can arrive under another spelling of the same
/// directory, where the walk judges no name ([`name_is_addressable`]). The
/// name is judged here on the recovered `rel`, the one that would be written,
/// which is the one question the walk left to the watcher -- not a second
/// filter over the rest.
///
/// (#544) Each `rel` comes with the path the walk found it at, which the look inside the
/// locks asks whether the name is lossy ([`rel_is_lossy`]).
fn new_directory_entries(
    kb_path: &Path,
    dir: &Path,
    registry: &Registry,
    rules: &crate::exclusion::ExclusionRules,
) -> Result<Vec<(String, PathBuf)>> {
    let found = crate::indexer::collect_source_files_under(kb_path, dir, registry, rules)?;
    Ok(found
        .into_iter()
        .filter_map(|f| {
            to_rel(kb_path, &f)
                .filter(|r| name_is_addressable(r, &f, false))
                .map(|r| (r, f))
        })
        .collect())
}

/// (kuriya 566 #16 / #17) The rows under an arrived directory that its walk did not collect:
/// `held` (the keys under `rel/`) minus every `rel` the walk found, split into the keys to hand
/// to the removal path and the number of keys left alone because they may be lossy
/// ([`key_may_be_lossy`]: no look can reach such a key, so handing it on would read a file
/// still there as gone).
fn rows_the_walk_left_behind(
    held: Vec<String>,
    found: &[(String, PathBuf)],
) -> (Vec<String>, usize) {
    let found: std::collections::HashSet<&str> = found.iter().map(|(r, _)| r.as_str()).collect();
    let (lossy, rest): (Vec<String>, Vec<String>) = held
        .into_iter()
        .filter(|key| !found.contains(key.as_str()))
        .partition(|key| key_may_be_lossy(key));
    (rest, lossy.len())
}

/// (kuriya 566 #16 / #17) What [`dispatch_new_directory`] does with the rows under `rel/` its
/// walk did not collect: each goes through [`handle_removed_path`], as if its own removal had
/// been reported, so the removal path decides it (a link where the file was: the row goes; a
/// file there the write gate takes: it is reindexed; an excluded path: it stays). The walk
/// collects no link (a symlink, a junction, a hard link), so a row whose file is now a link is
/// one of these, and with no removal of the old directory delivered (folded away, or made while
/// no watcher ran) nothing else would reach it. A directory that just arrived usually holds no
/// row, so the cost is the listing.
///
/// The db lock is held for the listing only, then released before any row is handed on: each
/// takes the locks it needs, the embedder lock never while the db lock is held (as
/// [`dispatch_subtree_removal`] does).
fn hand_on_rows_the_walk_left_behind(state: &WatcherState, rel: &str, found: &[(String, PathBuf)]) {
    if rel.is_empty() {
        return;
    }
    let held = {
        let db = recover_db(state.lock_for_batch(&state.db));
        db.document_paths_under(rel)
        // `db` is dropped here, before any row is handed on.
    };
    let held = match held {
        Ok(held) => held,
        Err(e) => {
            wdiag!("watcher: listing the documents under {rel}/ failed: {e}");
            return;
        }
    };
    let (left, lossy) = rows_the_walk_left_behind(held, found);
    if lossy > 0 {
        wdiag!(
            "watcher: left {lossy} documents under {rel}/ indexed: their names hold U+FFFD, \
             so they cannot be looked at one by one"
        );
    }
    for child in left {
        let mut full = state.kb_path.clone();
        for segment in child.split('/') {
            full.push(segment);
        }
        handle_removed_path(state, &child, &full);
    }
}

/// The `rel`s alone of [`new_directory_entries`], which is how the tests written before the
/// paths were kept ask for them. Only the Windows-only test asks for it.
#[cfg(all(test, windows))]
fn new_directory_rels(
    kb_path: &Path,
    dir: &Path,
    registry: &Registry,
    rules: &crate::exclusion::ExclusionRules,
) -> Result<Vec<String>> {
    Ok(new_directory_entries(kb_path, dir, registry, rules)?
        .into_iter()
        .map(|(rel, _)| rel)
        .collect())
}

/// Drop the row a version up to 1.12.0 stored for `rel` under its folded
/// spelling ([`indexer::sweep_legacy_folded_row`]). A no-op on Windows and for
/// any path without a `\` in it, which is every path but a handful.
///
/// `require_real_key` is passed straight through: `true` from the paths that
/// *write* `rel`, `false` from a removal. Whether `rel` is in the index is read
/// over there, inside the transaction that also decides and deletes, not here.
///
/// Not one transaction with the write of `rel` before it, and it does not need
/// to be: the order is "real key first", so stopping in between leaves a
/// duplicate row, which the next event for the file or the next full index
/// removes.
fn sweep_legacy_row(db: &Database, kb_path: &Path, rel: &str, require_real_key: bool) {
    log_folded_sweep(
        rel,
        indexer::sweep_legacy_folded_row(db, kb_path, rel, require_real_key),
    );
}

/// (feature-69) The line [`dispatch_reindex`] prints for a reindexed document. Takes the
/// counts as one value so the embedded and reused halves cannot be swapped on the way in;
/// the total is their sum. The `frontmatter unparsed` note is #251's.
fn reindexed_message(
    rel: &str,
    counts: indexer::ChunkVectorCounts,
    frontmatter_unparsed: bool,
) -> String {
    let chunks = counts.embedded + counts.reused;
    let reused = counts.reused;
    // (#251) The per-file warning naming the file is already on stderr
    // from the indexer; this only says the reindex kept the tag.
    let note = if frontmatter_unparsed {
        ", frontmatter unparsed"
    } else {
        ""
    };
    format!("watcher: reindexed {rel} ({chunks} chunks, {reused} reused{note})")
}

/// A create, modify or rename-to of `rel` that an event named at `raw`:
/// [`dispatch_reindex_from`] as a write, looked at under its exact spelling, with `raw`
/// deciding whether `rel` is lossy ([`rel_is_lossy`]).
fn dispatch_reindex(state: &WatcherState, rel: &str, raw: &Path) {
    dispatch_reindex_from(
        state,
        rel,
        ReindexOrigin::Write {
            raw: Some(raw),
            look: WriteLook::Spelled,
        },
    );
}

/// (#544) What started a reindex, which decides what the look inside the locks does with a
/// path that is not there.
#[derive(Debug, Clone, Copy)]
enum ReindexOrigin<'a> {
    /// A remove event found a file at the path again ([`RemovalAction::Reindex`]), or could not
    /// tell whether it is there (kuriya 548): [`decide_removal_in_locks`] and
    /// [`settle_removal_reindex_looked`]. `full` is the path the event named, which the gates
    /// judge when asked again inside the locks; `cause` is why its row is being decided
    /// ([`RemovalCause`]).
    Removal { full: &'a Path, cause: RemovalCause },
    /// A create, modify or rename-to: [`act_on_written_path`] and [`settle_write_reindex`].
    /// `raw` is the path the event named, when the caller has it ([`rel_is_lossy`]); `look` is
    /// how `rel` is looked at ([`written_path_on_disk`]).
    Write {
        raw: Option<&'a Path>,
        look: WriteLook,
    },
}

/// (#544) Reindex `rel` with both locks held once, the disk looked at inside them before the
/// reindex and, for a write the reindex did not settle, again after it. The look decides
/// whether a row is written, kept or dropped, not the event kind alone: a path that is gone by
/// the time the locks are held, is a directory, or is there only under another case of its
/// name keeps no row under that spelling.
fn dispatch_reindex_from(state: &WatcherState, rel: &str, origin: ReindexOrigin<'_>) {
    let mut embedder = recover(state.lock_for_batch(&state.embedder), "embedder");
    let db = recover_db(state.lock_for_batch(&state.db));
    match origin {
        ReindexOrigin::Removal { full, cause } => {
            reindex_after_removal_locked(state, &db, &mut embedder, rel, full, cause)
        }
        ReindexOrigin::Write { raw, look } => {
            reindex_after_write_locked(state, &db, &mut embedder, rel, raw, look)
        }
    }
}

/// The write half of [`dispatch_reindex_from`], for a caller holding both locks.
fn reindex_after_write_locked(
    state: &WatcherState,
    db: &Database,
    embedder: &mut Embedder,
    rel: &str,
    raw: Option<&Path>,
    look: WriteLook,
) {
    let kb_path = &state.kb_path;
    let lossy = rel_is_lossy(kb_path, raw);
    match act_on_written_path(
        db,
        kb_path,
        rel,
        written_path_on_disk(kb_path, rel, look),
        lossy,
    ) {
        WrittenPathCheck::Deindexed(outcome) => {
            log_written_path_gone(rel, outcome);
            return;
        }
        WrittenPathCheck::ProceedUnknown(kind) => {
            wdiag!("watcher: could not look at {rel} ({kind:?}); reindexing it anyway");
        }
        WrittenPathCheck::Proceed => {}
    }
    let texts_before = embedder.texts_embedded();
    let result = indexer::reindex_single_file(
        db,
        embedder,
        kb_path,
        rel,
        state.exclude_headings.as_deref(),
        &state.registry,
    );
    let settled = settle_write_reindex(db, kb_path, rel, &result, lossy, || {
        written_path_on_disk(kb_path, rel, look)
    });
    // (kuriya 566 #19 / #23) The indexer refused a file still there: when it is a link now (a
    // hard link made since the gate outside the locks passed it), the removal decision on a look
    // taken here drops the row, as a remove event would.
    if matches!(settled, WriteReindexSettled::LeftAsIs)
        && matches!(result, Ok(indexer::SingleResult::Refused))
    {
        let full = raw.map_or_else(|| kb_path.join(rel), Path::to_path_buf);
        if let Some(outcome) = refused_row_in_locks(
            db,
            kb_path,
            rel,
            &full,
            &state.registry,
            &state.rules,
            written_path_on_disk(kb_path, rel, look),
        ) {
            log_reindex_result(rel, result, texts_before, embedder);
            // `deindex_removed_path` already swept the folded row as a removal.
            log_refused_link_deindexed(rel, outcome);
            return;
        }
    }
    match settled {
        WriteReindexSettled::Kept | WriteReindexSettled::LeftAsIs => {
            let require_real_key = sweep_requires_real_key(&result);
            log_reindex_result(rel, result, texts_before, embedder);
            sweep_legacy_row(db, kb_path, rel, require_real_key);
        }
        // `deindex_removed_path` already swept the folded row as a removal.
        WriteReindexSettled::Deindexed(outcome) => {
            log_reindex_result(rel, result, texts_before, embedder);
            log_written_path_gone(rel, outcome);
        }
    }
}

/// What [`settle_write_reindex`] did after a write's reindex.
#[derive(Debug)]
enum WriteReindexSettled {
    /// The reindex settled the row (written, unchanged, or dropped by the indexer): no second
    /// look was taken.
    Kept,
    /// The reindex left the row as it was and the path is gone now: the rows under `rel` were
    /// deleted, and the result says whether either was there.
    Deindexed(Result<bool>),
    /// The reindex left the row as it was, and the second look did not find the path gone (or
    /// `rel` is lossy): nothing more was done.
    LeftAsIs,
}

/// (#544 A1) After a write's reindex: a result that settled the row is left alone without a
/// second look. One that left the row as it was -- a skip whatever its reason, a refusal, an
/// error -- is looked at again (`relook`), since the file may have gone between the first look
/// and the read; a path gone by then (and not lossy, [`rel_is_lossy`]) drops its rows as
/// [`act_on_written_path`] does. Unknown or present leaves them. Split out of the dispatch so a
/// test can run it with no [`crate::embedder::Embedder`].
fn settle_write_reindex(
    db: &Database,
    kb_path: &Path,
    rel: &str,
    result: &Result<indexer::SingleResult>,
    lossy: bool,
    relook: impl FnOnce() -> OnDisk,
) -> WriteReindexSettled {
    if write_reindex_settled_the_row(result) {
        return WriteReindexSettled::Kept;
    }
    match relook() {
        OnDisk::Gone if !lossy => {
            WriteReindexSettled::Deindexed(deindex_removed_path(db, kb_path, rel))
        }
        OnDisk::Gone | OnDisk::Present | OnDisk::Unknown(_) => WriteReindexSettled::LeftAsIs,
    }
}

/// (kuriya 566 #19 / #23 / #28) What happens to the row of `rel` when the indexer refused the
/// file at it ([`indexer::SingleResult::Refused`], or a rename's
/// [`indexer::RenameOutcome::RenamedButRefused`]) and a look taken with both locks held
/// (`on_disk`) still finds it there: [`decide_removal_in_locks`] on that look, so the removal
/// gate alone decides whether the row of a path now a link goes. `Some` with the deindex's
/// outcome when the rows went; `None` when nothing was touched: the look did not find the
/// file (the caller's own relook decides that), or the write gate takes the file (refused for
/// another reason, such as its size measured on the handle, and the row stays as it was), or
/// the removal gate turns the path away.
///
/// Split out of the dispatch so a test can run it with no [`crate::embedder::Embedder`].
fn refused_row_in_locks(
    db: &Database,
    kb_path: &Path,
    rel: &str,
    full: &Path,
    registry: &Registry,
    rules: &crate::exclusion::ExclusionRules,
    on_disk: OnDisk,
) -> Option<Result<bool>> {
    if on_disk != OnDisk::Present {
        return None;
    }
    match decide_removal_in_locks(
        db,
        kb_path,
        rel,
        full,
        registry,
        rules,
        on_disk,
        RemovalCause::RemoveEvent,
    ) {
        RemovalInLocks::Deindexed(outcome) => Some(outcome),
        RemovalInLocks::Reindex | RemovalInLocks::Skip | RemovalInLocks::LeftIndexed { .. } => None,
    }
}

/// What a write or a rename says when the file the indexer refused turned out to be one the
/// index holds no row for (a link, as a rule) and its rows went ([`refused_row_in_locks`]).
/// Silent when no row was there.
fn log_refused_link_deindexed(rel: &str, outcome: Result<bool>) {
    match outcome {
        Ok(true) => wdiag!("watcher: {rel} is not a file the index holds now; deindexed it"),
        Ok(false) => { /* no-op: not in DB */ }
        Err(e) => wdiag!("watcher: deindex {rel} failed: {e}"),
    }
}

/// (#544) Whether a write's reindex settled the row of its path: it wrote a row there, found
/// the row already right, or found nothing there to keep a row for
/// ([`indexer::SingleResult::NothingToIndex`],
/// [`indexer::SingleResult::OutsideKnowledgeBaseDeindexed`]). False when it skipped or refused
/// the file or failed: the row is then as it was before the reindex. The one answer to that
/// question for both [`settle_write_reindex`] (whether to look at the disk again) and
/// [`log_cleared_rename`] (whether the destination of a failed rename is left without a row).
/// Each variant named, not a catch-all, so a new variant stays a compile error (#251's way).
fn write_reindex_settled_the_row(result: &Result<indexer::SingleResult>) -> bool {
    match result {
        Ok(
            indexer::SingleResult::Updated { .. }
            | indexer::SingleResult::Unchanged
            | indexer::SingleResult::MetadataRefreshed { .. }
            | indexer::SingleResult::NothingToIndex { .. }
            | indexer::SingleResult::OutsideKnowledgeBaseDeindexed,
        ) => true,
        Ok(indexer::SingleResult::Skipped { .. } | indexer::SingleResult::Refused) | Err(_) => {
            false
        }
    }
}

/// What a write says when its path turned out not to be there under that spelling and its rows
/// went. Silent when no row was there.
fn log_written_path_gone(rel: &str, outcome: Result<bool>) {
    match outcome {
        Ok(true) => {
            wdiag!("watcher: {rel} is no longer on disk under that spelling; deindexed it")
        }
        Ok(false) => { /* no-op: not in DB */ }
        Err(e) => wdiag!("watcher: deindex {rel} failed: {e}"),
    }
}

/// (#543, local Codex, medium) The `require_real_key` that [`dispatch_reindex`] passes to
/// [`sweep_legacy_row`] after a reindex. A file left with nothing to embed
/// ([`indexer::SingleResult::NothingToIndex`]) is gone from the index like a removal, so its
/// folded legacy row goes too, whether or not the real key held a row; the write form (`true`)
/// would keep a folded row whose real key holds none. Every other result keeps the write form.
fn sweep_requires_real_key(result: &Result<indexer::SingleResult>) -> bool {
    !matches!(result, Ok(indexer::SingleResult::NothingToIndex { .. }))
}

/// (#543, local Codex r1) [`dispatch_reindex`] for a path a remove event found holding a file
/// again ([`RemovalAction::Reindex`]). The look that decided it ran outside the locks, and the
/// remove event is the only signal that the file is going: if it is gone by the time the locks
/// are held, no later event will drop the row. So the path is looked at again inside the same
/// embedder + database region as the write, before the reindex, and the row goes in that
/// region when the file is no longer there under the exact spelling of `rel`, directories on
/// the way included ([`removed_path_on_disk`]).
///
/// (kuriya 548) A remove event whose look outside the locks could not tell, or found the path
/// gone, comes here too ([`removal_dispatch`]), and `full`, the path the event named, comes
/// with both. (local Codex r2, medium) It comes only once a look under the database lock alone
/// found a file there or could not tell; a path still gone on that look has its rows dropped
/// there, with no embedder lock ([`dispatch_removal`]). Inside the
/// locks [`decide_removal_in_locks`] asks [`removal_decision_for`] again on the look taken
/// there, so a file is reindexed only when the write gate takes it, the rows go when the path
/// is gone or holds what the index cannot keep, and a look that still cannot tell leaves the
/// rows as they were and names why. Before kuriya 548 (local Codex r3, medium) such a look,
/// outside the locks or inside them, dropped the rows as a removal.
///
/// (kuriya 549) After the reindex, [`settle_removal_reindex_looked`] decides. A result that
/// settles the row (written, unchanged, or dropped by the indexer, as for an empty file or a
/// frontmatter-only stub) is taken as it is. Any other answer -- a skip whatever its reason,
/// a refusal, an error -- is looked at again: the rows go as a removal drops them only when the
/// path is gone by then, and stay as they were while the file is there or the look cannot
/// tell, as a write and `groove index` keep them. Before kuriya 549 (kuriya 543, GitHub codex
/// r1 P2) every such answer dropped the rows whatever the disk showed afterwards.
///
/// Only the remove path calls this. (#544) A create, modify or rename-to looks at the disk
/// inside the locks too, with its own answers ([`ReindexOrigin::Write`]).
fn dispatch_reindex_after_removal(
    state: &WatcherState,
    rel: &str,
    full: &Path,
    cause: RemovalCause,
) {
    dispatch_reindex_from(state, rel, ReindexOrigin::Removal { full, cause });
}

/// The removal half of [`dispatch_reindex_from`] ([`dispatch_reindex_after_removal`]'s body),
/// for a caller holding both locks.
fn reindex_after_removal_locked(
    state: &WatcherState,
    db: &Database,
    embedder: &mut Embedder,
    rel: &str,
    full: &Path,
    cause: RemovalCause,
) {
    let look = removed_path_on_disk(&state.kb_path, rel);
    match decide_removal_in_locks(
        db,
        &state.kb_path,
        rel,
        full,
        &state.registry,
        &state.rules,
        look,
        cause,
    ) {
        RemovalInLocks::Reindex => {}
        RemovalInLocks::Deindexed(outcome) => {
            log_removed_path_deindexed(rel, outcome);
            return;
        }
        RemovalInLocks::LeftIndexed { kind, held_row } => {
            log_removed_path_left_indexed(rel, kind, held_row);
            return;
        }
        RemovalInLocks::Skip => return,
    }
    let texts_before = embedder.texts_embedded();
    let result = indexer::reindex_single_file(
        db,
        embedder,
        &state.kb_path,
        rel,
        state.exclude_headings.as_deref(),
        &state.registry,
    );
    let settled = settle_removal_reindex_looked(db, &state.kb_path, rel, &result, || {
        removed_path_on_disk(&state.kb_path, rel)
    });
    match settled {
        RemovalSettled::Settled(RemovalReindexSettled::Kept) => {
            log_reindex_result(rel, result, texts_before, embedder);
            sweep_legacy_row(db, &state.kb_path, rel, true);
        }
        RemovalSettled::Settled(RemovalReindexSettled::IndexerDeindexed(folded)) => {
            log_reindex_result(rel, result, texts_before, embedder);
            log_folded_sweep(rel, folded);
        }
        RemovalSettled::Settled(RemovalReindexSettled::Deindexed { reason, outcome }) => {
            log_replacement_not_indexed(rel, &reason, outcome);
        }
        RemovalSettled::LeftIndexed {
            reason,
            unknown,
            held_row,
            folded,
        } => {
            log_replacement_left_indexed(rel, &reason, unknown, held_row);
            log_folded_sweep(rel, folded);
        }
    }
}

/// What [`settle_removal_reindex`] did after the reindex a remove event ran. (kuriya 549) The
/// dispatch gets it inside [`RemovalSettled::Settled`].
#[derive(Debug)]
enum RemovalReindexSettled {
    /// The rows now hold what is on disk: nothing more was done.
    Kept,
    /// The indexer already dropped the row under `rel`; only its folded legacy row was swept,
    /// as a removal (the result is that sweep's).
    IndexerDeindexed(Result<bool>),
    /// The replacement was not indexed: the row under `rel` and its folded legacy row were
    /// deleted, as for a plain removal. `outcome` is `Ok(true)` when either was there;
    /// The reason field says why the replacement was not indexed, with no document text in it.
    Deindexed {
        reason: String,
        outcome: Result<bool>,
    },
}

/// (#543, GitHub codex r1 P2) After [`dispatch_reindex_after_removal`]'s reindex: the rows are
/// left only when the reindex says they now hold the file on disk (`Updated`, `Unchanged`,
/// `MetadataRefreshed`). Any other answer -- a skip whatever its reason, a refusal, an error --
/// drops them, as the remove event dropped them before #543. A result whose row the indexer
/// already dropped only has its folded row swept, so no second line names it. The reason for a
/// skip, refusal or error comes from [`unsettled_reindex_reason`]; an error's goes through
/// [`crate::embedder::body_free_message`] there, so no document text reaches the log.
///
/// (kuriya 549) The dispatch calls this through [`settle_removal_reindex_looked`], only for a
/// result that settled the row, and for one that did not once a second look found the path
/// gone. A skip, refusal or error with the file still there (or a look that cannot tell) no
/// longer reaches it: the row stays, as `groove index` keeps it. The examples the r1 P2 named,
/// an empty file and a frontmatter-only stub, come back as
/// [`indexer::SingleResult::NothingToIndex`], a settled result, and are still dropped here.
///
/// Split out of the dispatch so a test can run it with no [`crate::embedder::Embedder`].
fn settle_removal_reindex(
    db: &Database,
    kb_path: &Path,
    rel: &str,
    result: &Result<indexer::SingleResult>,
) -> RemovalReindexSettled {
    match result {
        Ok(
            indexer::SingleResult::Updated { .. }
            | indexer::SingleResult::Unchanged
            | indexer::SingleResult::MetadataRefreshed { .. },
        ) => RemovalReindexSettled::Kept,
        Ok(indexer::SingleResult::OutsideKnowledgeBaseDeindexed) => {
            RemovalReindexSettled::IndexerDeindexed(indexer::sweep_legacy_folded_row(
                db, kb_path, rel, false,
            ))
        }
        // Each named, not a catch-all, so a new variant stays a compile error (#251's way);
        // the reason is only for the line, never branched on. Worded by the one mapping the
        // left-indexed line uses too ([`unsettled_reindex_reason`]), so a row dropped and a
        // row kept name the same result alike.
        Ok(indexer::SingleResult::Skipped { .. } | indexer::SingleResult::Refused) | Err(_) => {
            RemovalReindexSettled::Deindexed {
                reason: unsettled_reindex_reason(result),
                outcome: deindex_removed_path(db, kb_path, rel),
            }
        }
        // (#543) The skip of a file with nothing to embed, whose row the indexer has already
        // dropped: settled as that skip is, and the line counts that row as one the removal
        // dropped, so it reads as it did before the indexer took the row itself.
        Ok(indexer::SingleResult::NothingToIndex { deindexed, .. }) => {
            RemovalReindexSettled::Deindexed {
                reason: indexer::SKIPPED_NO_CHUNKS.to_string(),
                outcome: deindex_removed_path(db, kb_path, rel).map(|swept| swept || *deindexed),
            }
        }
    }
}

/// What [`settle_removal_reindex_looked`] did after the reindex a remove event ran.
#[derive(Debug)]
enum RemovalSettled {
    /// The reindex settled the row, or it did not and the second look found the path gone:
    /// what [`settle_removal_reindex`] did.
    Settled(RemovalReindexSettled),
    /// The reindex did not settle the row and the second look did not find the path gone:
    /// the row was left as it was. The reason field says why the file was not indexed, with no
    /// document text in it; `unknown` holds the kind of the failed look when the look could not
    /// tell, and is `None` when it found the file there. `held_row` says whether a row was
    /// under `rel` to keep. `folded` is the folded-row sweep's result, run in the write form
    /// (only when the real key holds a row), as after a write that left the row
    /// ([`WriteReindexSettled::LeftAsIs`]): never the removal form, which would drop a folded
    /// row that is all the index holds for a file still there.
    LeftIndexed {
        reason: String,
        unknown: Option<std::io::ErrorKind>,
        held_row: Result<bool>,
        folded: Result<bool>,
    },
}

/// (kuriya 549) After [`dispatch_reindex_after_removal`]'s reindex: a result that settled the
/// row ([`write_reindex_settled_the_row`]) goes to [`settle_removal_reindex`] with no second
/// look. One that did not -- a skip whatever its reason, a refusal, an error -- is looked at
/// again (`relook`), as [`settle_write_reindex`] does for a write: a path gone by then drops
/// its rows through [`settle_removal_reindex`], as before; a file still there, or a look that
/// cannot tell, leaves the row as it was, as `groove index` does when it skips a file (a size
/// cap, a read or parse failure, an embedding refusal or error); only the folded row is swept
/// then, in the write form. Split out of the dispatch so a test can run it with no
/// [`crate::embedder::Embedder`].
fn settle_removal_reindex_looked(
    db: &Database,
    kb_path: &Path,
    rel: &str,
    result: &Result<indexer::SingleResult>,
    relook: impl FnOnce() -> OnDisk,
) -> RemovalSettled {
    if write_reindex_settled_the_row(result) {
        return RemovalSettled::Settled(settle_removal_reindex(db, kb_path, rel, result));
    }
    let unknown = match relook() {
        OnDisk::Gone => {
            return RemovalSettled::Settled(settle_removal_reindex(db, kb_path, rel, result));
        }
        OnDisk::Present => None,
        OnDisk::Unknown(kind) => Some(kind),
    };
    let held_row = db.get_document_hash(rel).map(|hash| hash.is_some());
    RemovalSettled::LeftIndexed {
        reason: unsettled_reindex_reason(result),
        unknown,
        held_row,
        folded: indexer::sweep_legacy_folded_row(db, kb_path, rel, true),
    }
}

/// Why a reindex left the row as it was: the skip's reason, `refused`, or the error with no
/// document text in it ([`crate::embedder::body_free_message`]). The one mapping for both
/// lines that name such a result after a remove event, the one that says the row went
/// ([`settle_removal_reindex`]) and the one that says it was kept
/// ([`settle_removal_reindex_looked`]), so the two cannot word the same result apart. Only
/// called for a result [`write_reindex_settled_the_row`] answered false for; a settled one has
/// no such reason and gets an empty string.
fn unsettled_reindex_reason(result: &Result<indexer::SingleResult>) -> String {
    match result {
        Ok(indexer::SingleResult::Skipped { reason, .. }) => (*reason).to_string(),
        Ok(indexer::SingleResult::Refused) => "refused".to_string(),
        Err(e) => format!("reindex failed: {}", crate::embedder::body_free_message(e)),
        // Each named, not a catch-all, so a new variant stays a compile error (#251's way).
        Ok(
            indexer::SingleResult::Updated { .. }
            | indexer::SingleResult::Unchanged
            | indexer::SingleResult::MetadataRefreshed { .. }
            | indexer::SingleResult::NothingToIndex { .. }
            | indexer::SingleResult::OutsideKnowledgeBaseDeindexed,
        ) => String::new(),
    }
}

/// (kuriya 549) The line [`dispatch_reindex_after_removal`] prints when the replacement was not
/// indexed and the row was left as it was ([`RemovalSettled::LeftIndexed`]). Worded apart from
/// [`log_replacement_not_indexed`]'s, which says the row went.
fn left_indexed_message(rel: &str, reason: &str, unknown: Option<std::io::ErrorKind>) -> String {
    match unknown {
        None => format!(
            "watcher: {rel} was removed and written again, but the new file was not indexed ({reason}); the index keeps its previous text"
        ),
        Some(kind) => format!(
            "watcher: {rel} was removed and written again, but the new file was not indexed ({reason}); could not tell whether {rel} is still there ({kind}), so the index keeps its previous text"
        ),
    }
}

/// What [`dispatch_reindex_after_removal`] says for [`RemovalSettled::LeftIndexed`]. When no
/// row was there, only the skip is reported, as [`log_replacement_not_indexed`] does; a row
/// count that failed reads as a row kept, since nothing was deleted either way.
fn log_replacement_left_indexed(
    rel: &str,
    reason: &str,
    unknown: Option<std::io::ErrorKind>,
    held_row: Result<bool>,
) {
    match held_row {
        Ok(false) => wdiag!("watcher: skipped {rel} ({reason})"),
        Ok(true) | Err(_) => wdiag!("{}", left_indexed_message(rel, reason, unknown)),
    }
}

/// The line [`dispatch_reindex_after_removal`] prints when the replacement was not indexed and
/// the rows went as a removal. When no row was there, only the skip is reported.
fn log_replacement_not_indexed(rel: &str, reason: &str, outcome: Result<bool>) {
    match outcome {
        Ok(true) => wdiag!(
            "watcher: {rel} was removed and its replacement was not indexed ({reason}); deindexed it"
        ),
        Ok(false) => wdiag!("watcher: skipped {rel} ({reason})"),
        Err(e) => wdiag!(
            "watcher: {rel} was removed and its replacement was not indexed ({reason}); deindex failed: {e}"
        ),
    }
}

/// The lines for a folded-row sweep ([`indexer::sweep_legacy_folded_row`]) already run.
fn log_folded_sweep(rel: &str, outcome: Result<bool>) {
    match outcome {
        Ok(true) => wdiag!(
            "watcher: removed the row an older version stored for {rel} under a folded spelling"
        ),
        Ok(false) => {}
        Err(e) => wdiag!("watcher: removing the folded-spelling row for {rel} failed: {e}"),
    }
}

/// (kuriya 548) The line [`dispatch_reindex_after_removal`] prints when the look inside the
/// locks could not tell whether the path is still there and the row was left as it was
/// ([`RemovalInLocks::LeftIndexed`]). Before kuriya 548 the line said the row was deindexed.
fn unknown_left_indexed_message(rel: &str, kind: std::io::ErrorKind) -> String {
    format!("watcher: could not tell whether {rel} is still there ({kind}); left it indexed")
}

/// What [`dispatch_reindex_after_removal`] says for [`RemovalInLocks::LeftIndexed`]. Silent,
/// like [`log_removed_path_deindexed`], when no row was there; a row count that failed reads as
/// a row kept, since nothing was deleted either way.
fn log_removed_path_left_indexed(rel: &str, kind: std::io::ErrorKind, held_row: Result<bool>) {
    match held_row {
        Ok(false) => { /* no-op: not in DB */ }
        Ok(true) | Err(_) => wdiag!("{}", unknown_left_indexed_message(rel, kind)),
    }
}

/// What [`dispatch_reindex_after_removal`] says when the path turned out to be gone.
fn log_removed_path_deindexed(rel: &str, outcome: Result<bool>) {
    match outcome {
        Ok(true) => wdiag!("watcher: deindexed {rel}"),
        Ok(false) => { /* no-op: not in DB */ }
        Err(e) => wdiag!("watcher: deindex {rel} failed: {e}"),
    }
}

/// (#543) The line [`dispatch_reindex`] prints when a file it had indexed now holds nothing to
/// embed and the indexer dropped its rows ([`indexer::SingleResult::NothingToIndex`]).
fn emptied_message(rel: &str) -> String {
    format!("watcher: {rel} has nothing to embed now; deindexed it")
}

/// The line [`dispatch_reindex`] prints for what [`indexer::reindex_single_file`] returned.
fn log_reindex_result(
    rel: &str,
    result: Result<indexer::SingleResult>,
    texts_before: u64,
    embedder: &Embedder,
) {
    match result {
        Ok(indexer::SingleResult::Updated {
            chunks,
            frontmatter_unparsed,
        }) => {
            let counts = indexer::ChunkVectorCounts::of_updated(chunks, texts_before, embedder);
            wdiag!("{}", reindexed_message(rel, counts, frontmatter_unparsed));
        }
        // (#251) `MetadataRefreshed` cannot come back here: the one-time check
        // is off on this path. Named so a new variant stays a compile error.
        Ok(indexer::SingleResult::Unchanged | indexer::SingleResult::MetadataRefreshed { .. }) => {
            /* no-op */
        }
        // (BU-20) The reason is already on stderr from the read; this says what
        // happened to the document, which the reason does not.
        Ok(indexer::SingleResult::Refused) => {
            wdiag!("watcher: {rel} refused, index left as it was");
        }
        // (AX-02, review P2-1) The reason is on stderr from the indexer; this says the row went.
        Ok(indexer::SingleResult::OutsideKnowledgeBaseDeindexed) => {
            wdiag!("watcher: {rel} is reached through a link; deindexed it");
        }
        Ok(indexer::SingleResult::Skipped { reason, .. }) => {
            wdiag!("watcher: skipped {rel} ({reason})");
        }
        // (#543) The indexer dropped the row the file's earlier text left; say so, since the
        // document no longer answers searches. With no row there it reads as the skip it was.
        Ok(indexer::SingleResult::NothingToIndex {
            deindexed: true, ..
        }) => {
            wdiag!("{}", emptied_message(rel));
        }
        Ok(indexer::SingleResult::NothingToIndex {
            deindexed: false, ..
        }) => {
            wdiag!("watcher: skipped {rel} ({})", indexer::SKIPPED_NO_CHUNKS);
        }
        Err(e) => {
            wdiag!(
                "watcher: reindex {rel} failed: {}",
                crate::embedder::body_free_message(&e)
            );
        }
    }
}

/// (kuriya 259) The deindex of a name the index cannot hold, which
/// [`handle_removed_path`] alone calls: nothing at such a name is ever indexed, so the row
/// leaves without a look at the disk. Every other event-driven deletion looks at the disk inside
/// the database lock first ([`dispatch_removal`]).
fn dispatch_deindex(state: &WatcherState, rel: &str) {
    let db = recover_db(state.lock_for_batch(&state.db));
    match indexer::deindex_single_file(&db, rel) {
        Ok(true) => wdiag!("watcher: deindexed {rel}"),
        Ok(false) => { /* no-op: not in DB */ }
        Err(e) => wdiag!("watcher: deindex {rel} failed: {e}"),
    }
    sweep_legacy_row(&db, &state.kb_path, rel, false);
}

/// (feature-67) The range delete under a directory, for a caller holding the db
/// lock that has already decided the directory is gone from disk: the source of
/// a directory rename ([`sweep_source_if_gone`]), a path that is now a link
/// ([`deindex_under_a_link_parts`]). (kuriya 557) A removed directory runs the
/// same delete in [`subtree_removal_for`], once its look says it is gone.
/// Split out so a test can run it with no [`crate::embedder::Embedder`] (as
/// [`should_process_parts`] is).
///
/// `None` when `p` names no subtree ([`subtree_rel`]). Otherwise the rows under
/// `rel/` are deleted, and `rel` is returned with the outcome so the caller
/// logs the same spelling it deleted by. It stats nothing of its own and asks
/// no exclusion / extension / link gate: whatever the store still holds under
/// a directory that is not there is gone from disk. (kuriya 557) The look that
/// says the directory is not there is the caller's, taken under the same db
/// lock.
fn deindex_subtree_parts(
    db: &Database,
    kb_path: &Path,
    p: &Path,
) -> Option<(String, Result<Vec<String>>)> {
    let rel = subtree_rel(kb_path, p)?;
    let outcome = indexer::deindex_subtree(db, &rel);
    Some((rel, outcome))
}

/// (feature-67, kuriya 557) The prefix whose rows an event path `p` names as a subtree: its
/// `rel`, or `None` when `p` is not under the KB or is the KB root itself (an empty prefix
/// would name the whole KB, and [`indexer::deindex_subtree`] refuses it too). Every subtree
/// delete and listing asks this one question. Relativisation is the existing [`to_rel`],
/// shared with the single-file path, whose canonicalize fallback returns `None` for a path
/// that no longer exists.
///
/// Known limitation: on Unix a non-UTF-8 directory name is spelled lossily
/// (U+FFFD) by [`indexer::index_rel_path`], so two sibling directories whose
/// names collapse to the same spelling share a prefix and both fall in the range.
fn subtree_rel(kb_path: &Path, p: &Path) -> Option<String> {
    to_rel(kb_path, p).filter(|rel| !rel.is_empty())
}

/// (local Codex r3 on AX-02) [`deindex_subtree_parts`] for an event path that is now a link (a
/// symlink, or on Windows a junction, by `symlink_metadata`); `None`, touching nothing, for
/// anything else, including a path that cannot be stat'ed. Split out so a test can run it with
/// no [`crate::embedder::Embedder`].
///
/// (local Codex after PR #391 r1) The row at the link's own name goes too -- a file replaced
/// by a link -- with its folded legacy row ([`indexer::sweep_legacy_folded_row`] as a removal,
/// `require_real_key = false`): the full walk takes no link as a document. Its name is added to
/// the deleted list when it was there.
fn deindex_under_a_link_parts(
    db: &Database,
    kb_path: &Path,
    p: &Path,
) -> Option<(String, Result<Vec<String>>)> {
    let is_link = std::fs::symlink_metadata(p).is_ok_and(|m| m.file_type().is_symlink());
    if !is_link {
        return None;
    }
    let (rel, under) = deindex_subtree_parts(db, kb_path, p)?;
    let outcome = under.and_then(|mut deleted| {
        let exact = indexer::deindex_single_file(db, &rel)?;
        let folded = indexer::sweep_legacy_folded_row(db, kb_path, &rel, false)?;
        if exact || folded {
            deleted.push(rel.clone());
        }
        Ok(deleted)
    });
    Some((rel, outcome))
}

/// (kuriya 557) What [`subtree_removal_for`] did with the rows under a removed path.
#[derive(Debug)]
enum SubtreeRemoval {
    /// The directory is gone (or is now a file, a link, or there only under another case): the
    /// rows under it were deleted, and the result lists them.
    Swept(Result<Vec<String>>),
    /// The directory is there under its exact spelling: nothing deleted. The rows under it are
    /// listed, each to be handled as its own removal once the db lock is released.
    StillThere(Result<Vec<String>>),
    /// The look could not tell: nothing deleted, and the rows under it are listed for the line
    /// that says why.
    LeftIndexed {
        kind: std::io::ErrorKind,
        held: Result<Vec<String>>,
    },
}

/// (kuriya 557) [`subtree_removal_looked`] with a look a test hands in, in place of the one
/// the watcher takes ([`subtree_removal_under_lock`]).
#[cfg(test)]
fn decide_subtree_removal(
    db: &Database,
    kb_path: &Path,
    p: &Path,
    look: OnDisk,
) -> Option<(String, SubtreeRemoval)> {
    subtree_removal_looked(db, kb_path, p, |_| look)
}

/// (kuriya 557) What a remove event does with the rows under its path `p`, for a caller
/// holding the db lock: `None` when `p` names no subtree ([`subtree_rel`]), otherwise `rel`
/// with [`subtree_removal_for`] given `look(rel)`. `rel` is taken once, so the look, the delete
/// and the line use the same spelling.
fn subtree_removal_looked(
    db: &Database,
    kb_path: &Path,
    p: &Path,
    look: impl FnOnce(&str) -> OnDisk,
) -> Option<(String, SubtreeRemoval)> {
    let rel = subtree_rel(kb_path, p)?;
    let removal = subtree_removal_for(db, &rel, look(&rel));
    Some((rel, removal))
}

/// (kuriya 557) What a remove event does with the rows under `rel/` (not empty), for a caller
/// holding the db lock, given the look at the directory taken under that lock
/// ([`arrived_directory_on_disk`]).
///
/// [`OnDisk::Gone`] deletes the rows under `rel/` as feature-67 always did: no document can be
/// under a directory that is not there. [`OnDisk::Present`] and [`OnDisk::Unknown`] delete
/// nothing and list the rows under it: a removed directory there again (removed and created
/// within one debounce window) may hold the same files, and one that cannot be looked at may
/// still hold them.
fn subtree_removal_for(db: &Database, rel: &str, look: OnDisk) -> SubtreeRemoval {
    match look {
        OnDisk::Gone => SubtreeRemoval::Swept(indexer::deindex_subtree(db, rel)),
        OnDisk::Present => SubtreeRemoval::StillThere(db.document_paths_under(rel)),
        OnDisk::Unknown(kind) => SubtreeRemoval::LeftIndexed {
            kind,
            held: db.document_paths_under(rel),
        },
    }
}

/// (kuriya 557) [`subtree_removal_looked`] with the look at the directory taken here
/// ([`arrived_directory_on_disk`]), for a caller holding the db lock: the look and the delete
/// it decides sit in the same lock region, so no write of this process lands between them.
fn subtree_removal_under_lock(
    db: &Database,
    kb_path: &Path,
    p: &Path,
) -> Option<(String, SubtreeRemoval)> {
    subtree_removal_looked(db, kb_path, p, |rel| {
        arrived_directory_on_disk(kb_path, rel, p)
    })
}

/// (feature-67, kuriya issue #479, kuriya 557) What the store holds under a removed path. The
/// debouncer drops a child's Remove when its directory's Remove lands in the same window, so
/// the directory event is the only one that arrives.
///
/// The directory is looked at under the db lock first ([`subtree_removal_under_lock`]). Gone:
/// the rows under it go, as before. There again: the lock is released, then each row under it
/// goes through [`handle_removed_path`], as if its own removal had been reported; each one
/// looks at its own file and takes the locks it needs (the embedder lock is never taken while
/// the db lock is held), except a row whose key may be lossy ([`key_may_be_lossy`]), which
/// stays as it is with one line saying how many. Unknown: nothing is touched, and one line says
/// why.
fn dispatch_subtree_removal(state: &WatcherState, p: &Path) {
    let decided = {
        let db = recover_db(state.lock_for_batch(&state.db));
        subtree_removal_under_lock(&db, &state.kb_path, p)
        // `db` is dropped here, before any row under the directory is handled.
    };
    let Some((rel, removal)) = decided else {
        return;
    };
    match removal {
        SubtreeRemoval::Swept(outcome) => log_subtree_deindex(&rel, outcome),
        SubtreeRemoval::StillThere(Ok(held)) => {
            // (local Codex r1 F2 on PR #401) A key that may be a lossy spelling names no path
            // the look can reach, so handing it on would read a file still there as gone.
            let (kept, held): (Vec<String>, Vec<String>) =
                held.into_iter().partition(|key| key_may_be_lossy(key));
            if !kept.is_empty() {
                wdiag!(
                    "watcher: left {} documents under {rel}/ indexed: their names hold U+FFFD, \
                     so they cannot be looked at one by one",
                    kept.len()
                );
            }
            for child in held {
                let mut full = state.kb_path.clone();
                for segment in child.split('/') {
                    full.push(segment);
                }
                handle_removed_path(state, &child, &full);
            }
        }
        SubtreeRemoval::StillThere(Err(e)) => {
            wdiag!("watcher: listing the documents under {rel}/ failed: {e}")
        }
        SubtreeRemoval::LeftIndexed { kind, held } => match held {
            Ok(held) if held.is_empty() => {}
            Ok(held) => wdiag!(
                "watcher: could not tell whether the directory {rel} is still there ({kind:?}); \
                 left the {} documents under it indexed",
                held.len()
            ),
            Err(e) => wdiag!(
                "watcher: could not tell whether the directory {rel} is still there ({kind:?}); \
                 listing the documents under it failed: {e}"
            ),
        },
    }
}

/// The line for a range delete under `rel/` already run ([`deindex_subtree_parts`]).
fn log_subtree_deindex(rel: &str, outcome: Result<Vec<String>>) {
    match outcome {
        Ok(deleted) if deleted.is_empty() => {}
        Ok(deleted) => wdiag!(
            "watcher: deindexed {} documents under {rel}/",
            deleted.len()
        ),
        Err(e) => wdiag!("watcher: deindexing under {rel}/ failed: {e}"),
    }
}

/// What [`sweep_source_if_gone`] found at the source of a directory rename.
#[derive(Debug)]
enum SourceSweep {
    /// The source is gone: what the store held under it was deleted ([`deindex_subtree_parts`],
    /// `None` when the source names no prefix).
    Swept(Option<(String, Result<Vec<String>>)>),
    /// Something is at the source again: nothing deleted.
    Present,
    /// The stat failed in a way that does not say the source is gone: nothing deleted.
    Unknown(std::io::Error),
}

/// (#544 A14) The sweep of a renamed directory's source, for a caller holding the db lock: the
/// stat and the range delete in the same lock region, so no write of the same process lands
/// between them. Gone is [`indexer::stat_error_means_gone`]'s (not found, or a file where a
/// directory on the way was, under which nothing can be), not [`stat_unless_gone`]'s (not found
/// only), so a source whose parent became a file is swept too. `symlink_metadata`, so a link
/// left at the source is something there.
fn sweep_source_if_gone(db: &Database, kb_path: &Path, from: &Path) -> SourceSweep {
    match std::fs::symlink_metadata(from) {
        Ok(_) => SourceSweep::Present,
        Err(e) if indexer::stat_error_means_gone(e.kind()) => {
            SourceSweep::Swept(deindex_subtree_parts(db, kb_path, from))
        }
        Err(e) => SourceSweep::Unknown(e),
    }
}

/// (#544 P1-S) Whether a directory rename moves its rows to the destination, given the look at
/// the destination's spelling taken once ([`arrived_directory_on_disk`]). Not when the
/// destination is on disk only under another spelling: the moved rows would carry a spelling
/// the disk does not hold, so the rows only leave the old path and the destination's own event
/// indexes it. A look that cannot tell moves them as before (D3), and so does a lossy
/// destination, whose look never finds it (C4).
fn dir_rename_moves_its_steps(to_spelled: OnDisk, lossy: bool) -> bool {
    match to_spelled {
        OnDisk::Gone => lossy,
        OnDisk::Present | OnDisk::Unknown(_) => true,
    }
}

/// (#544) What a rename does with the look at its destination inside the locks, before the
/// indexer's rename ([`rename_plan`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RenamePlan {
    /// The destination is not on disk under that spelling: both ends' rows go, nothing moves.
    DropBoth,
    /// The destination could not be looked at, and moving the row is not possible (no row at
    /// the old end) or would run into the destination's row: every row stays as it is.
    LeaveUnknown(std::io::ErrorKind),
    /// The destination could not be looked at and only the old end holds a row: the row moves
    /// to the new end with its content left as it was, nothing read.
    MoveRowUnknown(std::io::ErrorKind),
    /// The destination is there and both ends hold a row (an overwriting rename): the
    /// destination's row goes, then the indexer's rename moves the source's onto it.
    ClearDestinationThenRename,
    /// The destination is there: the indexer's rename, as before.
    Rename,
}

/// (#544 C1/A9/A10, D3) How a rename of `old_rel` to `new_rel` goes, given the look at the
/// destination taken inside the locks:
///
/// - [`OnDisk::Gone`] gives [`RenamePlan::DropBoth`]
/// - [`OnDisk::Unknown`] gives [`RenamePlan::MoveRowUnknown`] when only `old_rel` holds a row,
///   [`RenamePlan::LeaveUnknown`] otherwise. The indexer's rename is not run: its own look
///   would read a failed stat as a missing file and drop the row (C2)
/// - [`OnDisk::Present`] gives [`RenamePlan::ClearDestinationThenRename`] when the two ends
///   differ and both hold a row, [`RenamePlan::Rename`] otherwise. With no row at `old_rel` the
///   destination's row is kept: the indexer reindexes it in place and reuses its vectors
///
/// The old end is not looked at: its row is only moved or dropped, and nothing is written there.
/// (kuriya 560) A file at the old end by the time the rename is handled is another file. Its own
/// create is queued apart from the rename (the debouncer moves the source's queue to the
/// destination) and comes after it in time order, so its event writes its row. A look here could
/// not keep the old row either: that row holds the text of the file that left. The watcher's
/// integration tests pin this for a file rename and for a directory rename, each followed by a
/// new file written at the old end before the rename is handled. A third test moves
/// a file into an excluded directory, which drops the old row without reaching this plan. Only on
/// Linux does it see the old row dropped without a look (by the paired rename or an unpaired
/// rename-from, which print the same line); on Windows the move arrives as a removal and an add,
/// so there it checks only the end state, reached through the removal that looks.
fn rename_plan(db: &Database, old_rel: &str, new_rel: &str, dest: OnDisk) -> Result<RenamePlan> {
    let has_row = |rel: &str| -> Result<bool> { Ok(db.get_document_hash(rel)?.is_some()) };
    Ok(match dest {
        OnDisk::Gone => RenamePlan::DropBoth,
        OnDisk::Unknown(kind) => {
            if has_row(old_rel)? && !has_row(new_rel)? {
                RenamePlan::MoveRowUnknown(kind)
            } else {
                RenamePlan::LeaveUnknown(kind)
            }
        }
        OnDisk::Present => {
            if old_rel != new_rel && has_row(old_rel)? && has_row(new_rel)? {
                RenamePlan::ClearDestinationThenRename
            } else {
                RenamePlan::Rename
            }
        }
    })
}

/// (#544) The part of a [`RenamePlan`] that runs before the indexer's rename and needs no
/// [`crate::embedder::Embedder`], with its line. `Ok(true)` when the indexer's rename is to run
/// next ([`RenamePlan::ClearDestinationThenRename`] and [`RenamePlan::Rename`]). No transaction
/// spans this and the rename: the rename opens its own. A failure in between, or of the rename
/// before it moved the source's row, is settled inside the same locks by
/// [`settle_cleared_rename`].
fn apply_rename_plan_before(
    db: &Database,
    kb_path: &Path,
    old_rel: &str,
    new_rel: &str,
    plan: &RenamePlan,
) -> Result<bool> {
    match plan {
        RenamePlan::DropBoth => {
            let old = deindex_removed_path(db, kb_path, old_rel)?;
            let new = deindex_removed_path(db, kb_path, new_rel)?;
            if old || new {
                wdiag!(
                    "watcher: {new_rel} is not on disk under that spelling; deindexed {old_rel} and {new_rel}"
                );
            }
            Ok(false)
        }
        RenamePlan::LeaveUnknown(kind) => {
            wdiag!("watcher: could not look at {new_rel} ({kind:?}); left the rows as they were");
            Ok(false)
        }
        RenamePlan::MoveRowUnknown(kind) => {
            db.rename_document(old_rel, new_rel, crate::db::FrontmatterTopic::Unread)?;
            wdiag!(
                "watcher: could not look at {new_rel} ({kind:?}); moved the row of {old_rel} without reading it"
            );
            Ok(false)
        }
        RenamePlan::ClearDestinationThenRename => {
            deindex_removed_path(db, kb_path, new_rel)?;
            wdiag!("watcher: {new_rel} was overwritten by {old_rel}; replaced its row");
            Ok(true)
        }
        RenamePlan::Rename => Ok(true),
    }
}

/// (#544 J4) A row [`RenamePlan::MoveRowUnknown`] moved to `new_rel` without reading it
/// (`before` is `Ok(false)`), across a parser ([`indexer::rename_crosses_a_parser`]): its chunks
/// were written by the parser of `old_rel`, so its hash is overwritten with
/// [`indexer::HASH_AWAITING_REPARSE`] and no read of `new_rel` takes the unchanged bytes as
/// indexed: (local Codex r1) [`finish_rename_locked`] reads it once in the same dispatch, and
/// when that read cannot settle it, the next event does. `Ok(true)` when the hash was written;
/// `Ok(false)` for any other plan, a `before` that did not move the row, or a rename within one
/// parser, all of which write nothing. Kept apart from [`apply_rename_plan_before`] so that its signature
/// needs no [`Registry`].
fn await_reparse_after_unread_move(
    db: &Database,
    registry: &Registry,
    old_rel: &str,
    new_rel: &str,
    plan: &RenamePlan,
    before: &Result<bool>,
) -> Result<bool> {
    if !unread_move_crosses_a_parser(registry, old_rel, new_rel, plan, before) {
        return Ok(false);
    }
    let written = db.overwrite_content_hash(&[new_rel], indexer::HASH_AWAITING_REPARSE)?;
    Ok(written > 0)
}

/// (#544, local Codex r2) Whether the plan moved the row of `old_rel` to `new_rel` unread
/// ([`RenamePlan::MoveRowUnknown`] with a `before` of `Ok(false)`) across a parser
/// ([`indexer::rename_crosses_a_parser`]): the moved row holds the chunks of the parser of
/// `old_rel`. Decided from what the plan did to the row, never from whether
/// [`await_reparse_after_unread_move`] then managed to write the hash.
fn unread_move_crosses_a_parser(
    registry: &Registry,
    old_rel: &str,
    new_rel: &str,
    plan: &RenamePlan,
    before: &Result<bool>,
) -> bool {
    matches!(plan, RenamePlan::MoveRowUnknown(_))
        && matches!(before, Ok(false))
        && indexer::rename_crosses_a_parser(registry, old_rel, new_rel)
}

/// (#544, local Codex r2) The [`RenameStart`] of [`dispatch_rename`]: the plan, what
/// [`apply_rename_plan_before`] gave for it, and, for a row moved unread across a parser
/// ([`unread_move_crosses_a_parser`]), the hash written by [`await_reparse_after_unread_move`]
/// and its line. Whether [`finish_rename_locked`] reads the destination again follows the
/// move, not the write: a row whose hash could not be written is read again all the same.
/// (#552) Wrapped in a [`DispatchStart`] that says whether the rename crosses a parser.
fn rename_start(
    db: &Database,
    registry: &Registry,
    (old_rel, new_rel): (&str, &str),
    plan: RenamePlan,
    before: Result<bool>,
) -> DispatchStart {
    let awaits = await_reparse_after_unread_move(db, registry, old_rel, new_rel, &plan, &before);
    log_await_reparse_after_unread_move(old_rel, new_rel, &awaits);
    let unread_move_awaits =
        unread_move_crosses_a_parser(registry, old_rel, new_rel, &plan, &before);
    DispatchStart {
        start: RenameStart {
            plan,
            before,
            unread_move_awaits,
        },
        crosses_a_parser: indexer::rename_crosses_a_parser(registry, old_rel, new_rel),
    }
}

/// (#544 J4) The line [`dispatch_rename`] prints after [`await_reparse_after_unread_move`]
/// (local Codex r2: and [`read_unread_move_again`] after it writes the hash again):
/// one when the hash was written, one when writing it failed (the row stays moved, with the
/// old row's hash), and none when there was nothing to write.
fn log_await_reparse_after_unread_move(old_rel: &str, new_rel: &str, result: &Result<bool>) {
    match result {
        Ok(true) => {
            wdiag!("watcher: {new_rel} crosses a parser from {old_rel}; marked to be read again")
        }
        Ok(false) => {}
        Err(e) => wdiag!(
            "watcher: {new_rel} crosses a parser from {old_rel}; could not mark it for a reparse: {}",
            crate::embedder::body_free_message(e)
        ),
    }
}

/// (#544 A7/A8) Whether the indexer's rename settled the row at `new_rel`: true when the row
/// was moved, reindexed or dropped and the outcome says so. False for an outcome that left the
/// row as it was (a refusal, a skipped reparse) and for an error, after which the destination
/// is looked at again. Each named, not a catch-all, so a new variant is a compile error here.
fn rename_outcome_is_settled(outcome: &Result<indexer::RenameOutcome>) -> bool {
    match outcome {
        Ok(
            indexer::RenameOutcome::Renamed
            | indexer::RenameOutcome::RenamedAndReindexed { .. }
            | indexer::RenameOutcome::OldPathMissing
            | indexer::RenameOutcome::RenamedSizeCapped
            | indexer::RenameOutcome::RenamedSizeCappedAndDropped
            | indexer::RenameOutcome::RenamedButRefusedAndDropped
            | indexer::RenameOutcome::MovedOutOfTheKnowledgeBase,
        ) => true,
        Ok(
            indexer::RenameOutcome::OldPathMissingAndRefused
            | indexer::RenameOutcome::RenamedButRefused
            | indexer::RenameOutcome::RenamedButNotReindexed
            // (#544 PR-2) The row was kept because the second read changed under the rename:
            // the destination is looked at again, and a gone one drops the row.
            | indexer::RenameOutcome::RenamedAcrossParsersAwaitingReparse,
        )
        | Err(_) => false,
    }
}

/// (#544) A paired rename of `old_rel` to `new_rel`, with both locks held once: the destination
/// is looked at as `look` says ([`written_path_on_disk`]) before anything is moved
/// ([`rename_plan`], [`apply_rename_plan_before`]), and again after an indexer's rename that
/// did not settle the row ([`rename_outcome_is_settled`]), whose row goes when the destination
/// is gone by then ([`finish_rename_locked`]).
fn dispatch_rename(state: &WatcherState, old_rel: &str, new_rel: &str, look: WriteLook) {
    let mut embedder = recover(state.lock_for_batch(&state.embedder), "embedder");
    let db = recover_db(state.lock_for_batch(&state.db));
    let kb_path = &state.kb_path;
    let dest = written_path_on_disk(kb_path, new_rel, look);
    let plan = match rename_plan(&db, old_rel, new_rel, dest) {
        Ok(plan) => plan,
        Err(e) => {
            log_rename_failed(old_rel, new_rel, &e);
            return;
        }
    };
    let before = apply_rename_plan_before(&db, kb_path, old_rel, new_rel, &plan);
    let start = rename_start(&db, &state.registry, (old_rel, new_rel), plan, before);
    let exclude_headings = state.exclude_headings.as_deref();
    finish_rename_locked_checked(
        &db,
        kb_path,
        (old_rel, new_rel),
        look,
        start,
        &mut *embedder,
        |embedder| {
            indexer::rename_single_file(
                &db,
                embedder,
                kb_path,
                old_rel,
                new_rel,
                exclude_headings,
                &state.registry,
            )
        },
        |embedder, checked| {
            indexer::reindex_single_file_checked(
                &db,
                embedder,
                kb_path,
                new_rel,
                exclude_headings,
                &state.registry,
                checked,
            )
        },
    );
    // (kuriya 566 #28) Still under both locks: a destination that became a hard link before
    // they were taken left the moved row there (the indexer refused the file and kept it).
    if let Some(outcome) =
        link_destination_in_locks(&db, kb_path, new_rel, &state.registry, &state.rules, look)
    {
        log_refused_link_deindexed(new_rel, outcome);
    }
}

/// (kuriya 566 #28) After a rename with both locks held: when the destination `new_rel` is a
/// link and a look taken here finds a file there, the removal decision on that look decides its
/// row ([`refused_row_in_locks`]), as for a reindex the indexer refused. A rename onto a hard
/// link moves the row and then has the indexer refuse the file
/// ([`indexer::RenameOutcome::RenamedButRefused`]); without this the row kept the moved text
/// at a path the index holds no document at. A destination that is not a link costs one
/// `symlink_metadata`, and its row is not touched. Split out so a test can run it with no
/// [`crate::embedder::Embedder`].
fn link_destination_in_locks(
    db: &Database,
    kb_path: &Path,
    new_rel: &str,
    registry: &Registry,
    rules: &crate::exclusion::ExclusionRules,
    look: WriteLook,
) -> Option<Result<bool>> {
    let mut full = kb_path.to_path_buf();
    for segment in new_rel.split('/') {
        full.push(segment);
    }
    let facts = PathFacts::of(&full);
    if !(facts.is_symlink() || facts.is_multiply_linked()) {
        return None;
    }
    refused_row_in_locks(
        db,
        kb_path,
        new_rel,
        &full,
        registry,
        rules,
        written_path_on_disk(kb_path, new_rel, look),
    )
}

/// (kuriya 566 #25 / #27) A rename whose new end the write gate refused: when that end is a
/// link now, its own row (the text of the file the rename overwrote) is decided by the removal
/// path, as if its removal had been reported. Called after the old end was dealt with.
fn hand_on_a_link_destination(state: &WatcherState, new_rel: &str, to: &Path) {
    let facts = PathFacts::of(to);
    if facts.is_symlink() || facts.is_multiply_linked() {
        handle_removed_path(state, new_rel, to);
    }
}

/// The line a rename prints when it failed before the indexer's rename returned.
fn log_rename_failed(old_rel: &str, new_rel: &str, e: &anyhow::Error) {
    wdiag!(
        "watcher: rename {old_rel} -> {new_rel} failed: {}",
        crate::embedder::body_free_message(e)
    );
}

/// (#544, local Codex r1) What [`finish_rename_locked`] starts from: the [`RenamePlan`], what
/// [`apply_rename_plan_before`] gave for it, and whether the plan moved a row unread across a
/// parser ([`unread_move_crosses_a_parser`]; local Codex r2: whether or not
/// [`await_reparse_after_unread_move`] then wrote its hash). A `(plan, before)` pair converts to
/// one that did not.
#[derive(Debug)]
struct RenameStart {
    plan: RenamePlan,
    before: Result<bool>,
    unread_move_awaits: bool,
}

impl From<(RenamePlan, Result<bool>)> for RenameStart {
    fn from((plan, before): (RenamePlan, Result<bool>)) -> Self {
        Self {
            plan,
            before,
            unread_move_awaits: false,
        }
    }
}

/// (#552) What [`finish_rename_locked`] starts from: a [`RenameStart`], and whether the rename
/// crosses a parser ([`indexer::rename_crosses_a_parser`]), which decides whether an indexer's
/// rename that failed after it moved the row is read again in the same dispatch. A
/// [`RenameStart`] or a `(plan, before)` pair converts to one that crosses no parser.
#[derive(Debug)]
struct DispatchStart {
    start: RenameStart,
    crosses_a_parser: bool,
}

impl From<RenameStart> for DispatchStart {
    fn from(start: RenameStart) -> Self {
        Self {
            start,
            crosses_a_parser: false,
        }
    }
}

impl From<(RenamePlan, Result<bool>)> for DispatchStart {
    fn from(pair: (RenamePlan, Result<bool>)) -> Self {
        RenameStart::from(pair).into()
    }
}

/// (#544, local Codex r1) The row an unsettled indexer's rename left at the destination with
/// chunks it did not build from the bytes there: the reparse wrote nothing
/// ([`indexer::RenameOutcome::RenamedButNotReindexed`]), or the rename crossed a parser and its
/// reparse found other bytes than its first read hashed
/// ([`indexer::RenameOutcome::RenamedAcrossParsersAwaitingReparse`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KeptRow {
    NotReindexed,
    AwaitingReparse,
}

/// The [`KeptRow`] an outcome of the indexer's rename left, if any.
fn kept_row(outcome: &Result<indexer::RenameOutcome>) -> Option<KeptRow> {
    match outcome {
        Ok(indexer::RenameOutcome::RenamedButNotReindexed) => Some(KeptRow::NotReindexed),
        Ok(indexer::RenameOutcome::RenamedAcrossParsersAwaitingReparse) => {
            Some(KeptRow::AwaitingReparse)
        }
        _ => None,
    }
}

/// (#544, local Codex r1) Whether [`finish_rename_locked`]'s second look at the destination,
/// `on_disk`, reads it again for the row `kept`: a destination that is there, for either kept
/// row; and one the look could not tell, for a row kept awaiting a reparse, whose chunks are
/// another parser's -- one attempt, whose failure leaves the mark (D5). A gone destination is
/// never read: its row goes.
fn reads_destination_again(kept: Option<KeptRow>, on_disk: OnDisk) -> bool {
    matches!(
        (kept, on_disk),
        (Some(_), OnDisk::Present) | (Some(KeptRow::AwaitingReparse), OnDisk::Unknown(_))
    )
}

/// (#544) The destination's second read in [`finish_rename_locked`]: the caller's reindex of
/// `new_rel`, taken from `reindex` so it runs at most once per dispatch, and its line. Returns
/// the `require_real_key` of the sweep at `new_rel` that read's own result asks for
/// ([`sweep_requires_real_key`]); the write form when the reindex was already taken. The read
/// is not asked to check its bytes before parsing (the reindex's flag is false): a row within
/// one parser is kept on a failed parse, as for a file edited in place (J2).
fn read_destination_again<C, R>(
    ctx: &mut C,
    reindex: &mut Option<R>,
    (old_rel, new_rel): (&str, &str),
) -> bool
where
    R: FnOnce(&mut C, bool) -> Result<indexer::SingleResult>,
{
    let Some(reindex) = reindex.take() else {
        return true;
    };
    let result = reindex(ctx, false);
    let requires_real_key = sweep_requires_real_key(&result);
    log_rename_read_again(old_rel, new_rel, result);
    requires_real_key
}

/// (#544, local Codex r3, J3) [`read_destination_again`] for a row a rename across a parser kept
/// awaiting a reparse ([`KeptRow::AwaitingReparse`]): the read's result is settled as the
/// rename's first read is ([`settle_crossed_read_again`]), so bytes the read verified and the
/// new parser cannot index drop the row, and only bytes that changed again keep it awaiting. A
/// dropped row's folded rows are swept as a removal. The read checks its bytes against its own
/// hash before parsing (the reindex's flag is true,
/// [`indexer::reindex_single_file_checked`]), so a parse failure that drops the row is always
/// one of the bytes checked (D5).
fn read_awaiting_destination_again<C, R>(
    db: &Database,
    ctx: &mut C,
    reindex: &mut Option<R>,
    (old_rel, new_rel): (&str, &str),
) -> bool
where
    R: FnOnce(&mut C, bool) -> Result<indexer::SingleResult>,
{
    let Some(reindex) = reindex.take() else {
        return true;
    };
    let result = reindex(ctx, true);
    let dropped = settle_crossed_read_again(db, new_rel, &result);
    let requires_real_key = !dropped && sweep_requires_real_key(&result);
    let settled = read_again_settles_the_row(&result);
    log_rename_read_again(old_rel, new_rel, result);
    if dropped && !settled {
        log_crossed_row_dropped(old_rel, new_rel);
    }
    requires_real_key
}

/// (#544, local Codex r3) The line printed after [`settle_crossed_read_again`] dropped a row
/// whose further read had kept it ([`log_rename_read_again`] said "kept the row" just before).
fn log_crossed_row_dropped(old_rel: &str, new_rel: &str) {
    wdiag!(
        "watcher: {new_rel} crosses a parser from {old_rel}; dropped its row, whose bytes the new parser cannot index"
    );
}

/// (#544, local Codex r3, J3) The settlement of a further read of a row a rename across a parser
/// left with the old parser's chunks: (local Codex r5) every result goes to
/// [`indexer::settle_further_crossed_read`], which names the outcomes that may drop the row and
/// keeps it under [`indexer::HASH_AWAITING_REPARSE`] on every other one. An error of the read
/// checked no bytes and touches nothing (D5). `true` when the row was dropped. A failure of the
/// settlement is logged and the row is left as it was.
fn settle_crossed_read_again(
    db: &Database,
    new_rel: &str,
    result: &Result<indexer::SingleResult>,
) -> bool {
    let Ok(outcome) = result else {
        return false;
    };
    match indexer::settle_further_crossed_read(db, new_rel, outcome) {
        Ok(dropped) => dropped,
        Err(e) => {
            wdiag!(
                "watcher: could not settle the row of {new_rel} after reading it again: {}",
                crate::embedder::body_free_message(&e)
            );
            false
        }
    }
}

/// (#544, local Codex r2) [`read_destination_again`] for a row [`RenamePlan::MoveRowUnknown`]
/// moved unread across a parser ([`unread_move_crosses_a_parser`]), read whether or not its
/// hash was written. (local Codex r3, J3) The read is settled as a crossed rename's first read
/// is ([`settle_crossed_read_again`]): bytes it verified and the new parser cannot index drop
/// the row. A read that neither rewrites nor drops it ([`read_again_settles_the_row`]) -- bytes
/// that changed again, or a read that verified nothing -- leaves the old parser's chunks, so
/// the hash is written again as [`indexer::HASH_AWAITING_REPARSE`] (D5: an unverified read
/// never drops the row). A failure of that write is logged and the dispatch goes on. The read
/// checks its bytes before parsing (the reindex's flag is true), as
/// [`read_awaiting_destination_again`]'s does.
fn read_unread_move_again<C, R>(
    db: &Database,
    ctx: &mut C,
    reindex: &mut Option<R>,
    (old_rel, new_rel): (&str, &str),
) -> bool
where
    R: FnOnce(&mut C, bool) -> Result<indexer::SingleResult>,
{
    let Some(reindex) = reindex.take() else {
        return true;
    };
    let result = reindex(ctx, true);
    let dropped = settle_crossed_read_again(db, new_rel, &result);
    let requires_real_key = !dropped && sweep_requires_real_key(&result);
    let settled = read_again_settles_the_row(&result);
    log_rename_read_again(old_rel, new_rel, result);
    if dropped && !settled {
        log_crossed_row_dropped(old_rel, new_rel);
    } else if !settled {
        let marked = db
            .overwrite_content_hash(&[new_rel], indexer::HASH_AWAITING_REPARSE)
            .map(|written| written > 0);
        log_await_reparse_after_unread_move(old_rel, new_rel, &marked);
    }
    requires_real_key
}

/// (#544, local Codex r2) Whether a second read of a destination rewrote its row from the bytes
/// there or dropped it. A read that kept the row -- unchanged, a metadata-only refresh, a skip,
/// a refusal, a failure -- leaves whatever chunks it had. Each named, so a new variant is a
/// compile error here.
fn read_again_settles_the_row(result: &Result<indexer::SingleResult>) -> bool {
    match result {
        Ok(
            indexer::SingleResult::Updated { .. }
            | indexer::SingleResult::OutsideKnowledgeBaseDeindexed
            | indexer::SingleResult::NothingToIndex { .. },
        ) => true,
        Ok(
            indexer::SingleResult::Unchanged
            | indexer::SingleResult::MetadataRefreshed { .. }
            | indexer::SingleResult::Skipped { .. }
            | indexer::SingleResult::Refused,
        )
        | Err(_) => false,
    }
}

/// (#544) The rest of [`dispatch_rename`] once the plan has run ([`apply_rename_plan_before`]
/// gave `before`), with both locks still held. `rename` is the indexer's rename and `reindex`
/// a reindex of `new_rel`, each handed `ctx` (the embedder), so a test can run this with no
/// [`crate::embedder::Embedder`] and inject their failures.
///
/// (#544 C1) A plan that cleared the destination's row
/// ([`RenamePlan::ClearDestinationThenRename`]) and then failed -- clearing it, or the
/// indexer's rename -- goes to [`settle_cleared_rename`] before anything else, so a file on
/// disk at `new_rel` gets a row from its own reindex when that reindex writes one. The
/// source's row is not moved onto it there.
///
/// (#544 Review Focus 4) A rename whose indexer's rename moved the source's row but whose
/// reparse wrote nothing ([`indexer::RenameOutcome::RenamedButNotReindexed`]), with the file
/// still at `new_rel` on the second look: `new_rel` is read again, once, by the caller's reindex closure, and its
/// result alone decides the row. This holds for a plain [`RenamePlan::Rename`] as well as for
/// [`RenamePlan::ClearDestinationThenRename`]: Windows delivers an overwriting rename as a
/// removal of the destination followed by a plain rename, and a plain rename whose bytes left
/// nothing to index would otherwise keep the source's old content under the new path.
///
/// (#544, local Codex r1) The same second read, once, for a rename across a parser whose row
/// was kept under [`indexer::HASH_AWAITING_REPARSE`]
/// ([`indexer::RenameOutcome::RenamedAcrossParsersAwaitingReparse`]) -- on a destination that
/// is there, or that the second look could not tell ([`reads_destination_again`]) -- and for a
/// row [`RenamePlan::MoveRowUnknown`] moved unread across a parser
/// ([`RenameStart::unread_move_awaits`]; local Codex r2: whether or not
/// [`await_reparse_after_unread_move`] wrote its hash). That read's own result decides the row:
/// a read that cannot settle the bytes leaves the mark (D5), written again after an unread move
/// ([`read_unread_move_again`]). (local Codex r3, J3) For the row kept awaiting, a read that
/// verified bytes the new parser cannot index drops it, as the rename's first read would
/// ([`read_awaiting_destination_again`]); only bytes that changed again keep the mark.
///
/// (#552) The same read, once, when an indexer's rename across a parser
/// ([`DispatchStart::crosses_a_parser`]) returned an error and the row at the destination holds
/// [`indexer::HASH_AWAITING_REPARSE`]: such a rename marks the row before it moves it, so the
/// error came after the move, and the row holds the old parser's chunks. It is read as a row
/// kept awaiting is, and for an overwriting rename the settlement of a failed overwrite
/// ([`settle_cleared_rename`]), which would find the moved row in place and do nothing, is
/// skipped so that the read can run. Whichever event marked it -- this rename, or an earlier
/// one that left a sentinel row there before this rename failed without moving anything (its
/// source row already gone) -- a destination row still under the sentinel after a failed
/// crossed rename is read once, checked. A failed crossed rename that leaves no sentinel row at
/// the destination, and one within a parser, is not read again.
///
/// `reindex` here takes no flag: it is [`finish_rename_locked_checked`] with a reindex that
/// ignores whether it was asked to check its bytes before parsing. Only the tests call this
/// form; [`dispatch_rename`] calls the checked one.
#[allow(clippy::too_many_arguments)]
#[cfg_attr(not(test), allow(dead_code))]
fn finish_rename_locked<C, R>(
    db: &Database,
    kb_path: &Path,
    pair: (&str, &str),
    look: WriteLook,
    start: impl Into<DispatchStart>,
    ctx: &mut C,
    rename: impl FnOnce(&mut C) -> Result<indexer::RenameOutcome>,
    reindex: R,
) where
    R: FnOnce(&mut C) -> Result<indexer::SingleResult>,
{
    finish_rename_locked_checked(
        db,
        kb_path,
        pair,
        look,
        start,
        ctx,
        rename,
        move |ctx: &mut C, _checked: bool| reindex(ctx),
    );
}

/// (#544, local Codex r3, D5) [`finish_rename_locked`], whose `reindex` is told whether the read
/// must check its bytes against its own hash before parsing
/// ([`indexer::reindex_single_file_checked`]): true for the further read of a row a rename
/// across a parser left with the old parser's chunks ([`read_awaiting_destination_again`],
/// [`read_unread_move_again`]), whose result can drop the row; false for every other read.
/// The reindex still runs at most once per dispatch.
#[allow(clippy::too_many_arguments)]
fn finish_rename_locked_checked<C, R>(
    db: &Database,
    kb_path: &Path,
    (old_rel, new_rel): (&str, &str),
    look: WriteLook,
    start: impl Into<DispatchStart>,
    ctx: &mut C,
    rename: impl FnOnce(&mut C) -> Result<indexer::RenameOutcome>,
    reindex: R,
) where
    R: FnOnce(&mut C, bool) -> Result<indexer::SingleResult>,
{
    let DispatchStart {
        start:
            RenameStart {
                plan,
                before,
                unread_move_awaits,
            },
        crosses_a_parser,
    } = start.into();
    let cleared = plan == RenamePlan::ClearDestinationThenRename;
    // Taken by the settlement of a failed overwrite or by the destination's second read
    // ([`read_destination_again`]); a rename reaches at most one of them.
    let mut reindex = Some(reindex);
    let settle_cleared = |ctx: &mut C, reindex: R| {
        let recovery = settle_cleared_rename(
            db,
            kb_path,
            (old_rel, new_rel),
            || written_path_on_disk(kb_path, new_rel, look),
            || written_path_on_disk(kb_path, old_rel, look),
            || reindex(ctx, false),
        );
        log_cleared_rename(old_rel, new_rel, recovery);
    };
    match before {
        Ok(true) => {}
        // The rows are already where the plan put them; a moved row's folded rows follow it.
        // (#544, local Codex r1) A row moved unread across a parser, marked awaiting a reparse,
        // is read once here, and that read's own result decides the row and the sweep.
        Ok(false) if matches!(plan, RenamePlan::MoveRowUnknown(_)) => {
            let new_requires_real_key = if unread_move_awaits {
                read_unread_move_again(db, ctx, &mut reindex, (old_rel, new_rel))
            } else {
                true
            };
            sweep_legacy_row(db, kb_path, old_rel, false);
            sweep_legacy_row(db, kb_path, new_rel, new_requires_real_key);
            return;
        }
        // Dropping swept the folded rows as a removal; leaving touches nothing.
        Ok(false) => return,
        Err(e) => {
            log_rename_failed(old_rel, new_rel, &e);
            if cleared && let Some(reindex) = reindex.take() {
                settle_cleared(ctx, reindex);
                sweep_legacy_row(db, kb_path, old_rel, false);
                sweep_legacy_row(db, kb_path, new_rel, true);
            }
            return;
        }
    }
    let outcome = rename(ctx);
    let failed = outcome.is_err();
    // (#552) A rename across a parser marks the row awaiting a reparse before it moves it, so a
    // failure that leaves such a row at the destination came after the move.
    let crossed_err = crosses_a_parser && failed && awaits_reparse_at(db, new_rel);
    let settled = rename_outcome_is_settled(&outcome);
    let kept = if crossed_err {
        Some(KeptRow::AwaitingReparse)
    } else {
        kept_row(&outcome)
    };
    log_rename_outcome(old_rel, new_rel, outcome);
    if crossed_err {
        log_crossed_rename_failed_after_the_move(old_rel, new_rel);
    }
    if cleared
        && failed
        && !crossed_err
        && let Some(reindex) = reindex.take()
    {
        settle_cleared(ctx, reindex);
    }
    // The write form of the sweep at `new_rel`, unless the destination's second read below
    // decides otherwise.
    let mut new_requires_real_key = true;
    if !settled {
        match written_path_on_disk(kb_path, new_rel, look) {
            OnDisk::Gone => {
                log_written_path_gone(new_rel, deindex_removed_path(db, kb_path, new_rel));
            }
            on_disk if reads_destination_again(kept, on_disk) => {
                new_requires_real_key = if kept == Some(KeptRow::AwaitingReparse) {
                    read_awaiting_destination_again(db, ctx, &mut reindex, (old_rel, new_rel))
                } else {
                    read_destination_again(ctx, &mut reindex, (old_rel, new_rel))
                };
            }
            OnDisk::Present | OnDisk::Unknown(_) => {}
        }
    }
    // The file has left `old_rel` under every spelling; at `new_rel` the sweep
    // follows the real key in, as it does after a reindex, and a second read that found
    // nothing to embed sweeps it as a removal ([`sweep_requires_real_key`]).
    sweep_legacy_row(db, kb_path, old_rel, false);
    sweep_legacy_row(db, kb_path, new_rel, new_requires_real_key);
}

/// (#552) Whether the row at `rel` holds [`indexer::HASH_AWAITING_REPARSE`]. A failed read of
/// the row is logged and answers false: the dispatch then goes on as it did before #552.
fn awaits_reparse_at(db: &Database, rel: &str) -> bool {
    match db.get_document_hash(rel) {
        Ok(hash) => hash.as_deref() == Some(indexer::HASH_AWAITING_REPARSE),
        Err(e) => {
            wdiag!(
                "watcher: could not read the stored hash of {rel}: {}",
                crate::embedder::body_free_message(&e)
            );
            false
        }
    }
}

/// (#552) The line [`finish_rename_locked`] prints when an indexer's rename across a parser
/// failed after it moved the row, before it looks at the destination again: the look reads it,
/// or drops the row unread when the file is gone.
fn log_crossed_rename_failed_after_the_move(old_rel: &str, new_rel: &str) {
    wdiag!(
        "watcher: rename {old_rel} -> {new_rel} failed after the row moved; look at {new_rel} again"
    );
}

/// (#544 Review Focus 4) The line [`finish_rename_locked`] prints after it read the destination
/// of a rename again because the rename did not index its content: the indexer's reparse wrote
/// nothing, or (local Codex r1) the row was kept awaiting a reparse or moved unread across a
/// parser.
fn log_rename_read_again(old_rel: &str, new_rel: &str, result: Result<indexer::SingleResult>) {
    let what = read_again_outcome(&result);
    wdiag!(
        "watcher: renamed {old_rel} -> {new_rel}, whose content the rename did not index; read {new_rel} again: {what}"
    );
}

/// What [`log_rename_read_again`] says the destination's second read did to the row there, in
/// words. Every variant is named so a new one is a compile error rather than a silent line.
fn read_again_outcome(result: &Result<indexer::SingleResult>) -> String {
    match result {
        Ok(indexer::SingleResult::Updated { chunks, .. }) => {
            format!("rewrote the row ({chunks} chunks)")
        }
        Ok(indexer::SingleResult::Unchanged) => "kept the row (content unchanged)".to_string(),
        Ok(indexer::SingleResult::MetadataRefreshed { .. }) => {
            "rewrote the row's metadata".to_string()
        }
        Ok(indexer::SingleResult::Skipped { reason, .. }) => format!("kept the row ({reason})"),
        Ok(indexer::SingleResult::Refused) => "kept the row (the file was refused)".to_string(),
        Ok(indexer::SingleResult::OutsideKnowledgeBaseDeindexed) => {
            "dropped the row (the file is reached through a link)".to_string()
        }
        Ok(indexer::SingleResult::NothingToIndex {
            deindexed: true, ..
        }) => "dropped the row (nothing to embed)".to_string(),
        Ok(indexer::SingleResult::NothingToIndex {
            deindexed: false, ..
        }) => "wrote no row (nothing to embed)".to_string(),
        Err(e) => format!("failed ({})", crate::embedder::body_free_message(e)),
    }
}

/// What [`settle_cleared_rename`] did at the destination.
#[derive(Debug)]
enum ClearedDestination {
    /// A row is at `new_rel` (the failure came after the source's row moved): nothing touched.
    RowInPlace,
    /// Whether `new_rel` holds a row could not be read: nothing touched.
    RowUnread(anyhow::Error),
    /// The file is there: it was reindexed, with this result.
    Reindexed(Result<indexer::SingleResult>),
    /// The file is not there, or could not be looked at: nothing was written.
    NotReindexed(OnDisk),
}

/// What [`settle_cleared_rename`] did with the source's row.
#[derive(Debug)]
enum ClearedSource {
    /// Not looked at: the destination holds a row, or the source's row could not be read.
    Untouched,
    /// No row at `old_rel`.
    NoRow,
    /// The file is at `old_rel` again, or could not be looked at: the row stays, for that
    /// file's own event.
    Kept(OnDisk),
    /// The file is gone from `old_rel`: the source's rows were deleted, and the result says
    /// whether either was there. They are not moved onto the destination.
    Dropped(Result<bool>),
}

/// (#544 C1) An overwriting rename cleared the destination's row
/// ([`RenamePlan::ClearDestinationThenRename`]) and then failed, maybe before the source's row
/// moved onto it. Inside the same locks, nothing looped:
///
/// - a row at `new_rel` means the move happened: nothing more is done
/// - otherwise the destination is looked at again (`look_new`). A file there is reindexed
///   (`reindex`), and that is the only way it gets a row here. When the reindex writes nothing
///   (it fails, or skips the file), or the destination cannot be looked at, it is left with no
///   row until its next event or a full index, and stderr says why ([`log_cleared_rename`])
/// - the source's row is never moved onto the destination: the bytes there may be the
///   source's and still not be what the destination's parser reads (local Codex r3: a
///   Markdown row on a `.docx` path), and only the indexer's rename settles that. The row
///   goes when its file is gone from `old_rel` (`look_old`); a file at `old_rel` again, or one
///   that cannot be looked at, keeps its row
///
/// A failure here leaves the rows as they are and says so on stderr.
fn settle_cleared_rename(
    db: &Database,
    kb_path: &Path,
    (old_rel, new_rel): (&str, &str),
    look_new: impl FnOnce() -> OnDisk,
    look_old: impl FnOnce() -> OnDisk,
    reindex: impl FnOnce() -> Result<indexer::SingleResult>,
) -> (ClearedDestination, ClearedSource) {
    match db.get_document_hash(new_rel) {
        Ok(Some(_)) => return (ClearedDestination::RowInPlace, ClearedSource::Untouched),
        Ok(None) => {}
        Err(e) => return (ClearedDestination::RowUnread(e), ClearedSource::Untouched),
    }
    let dest = match look_new() {
        OnDisk::Present => ClearedDestination::Reindexed(reindex()),
        not_there => ClearedDestination::NotReindexed(not_there),
    };
    let source = match db.get_document_hash(old_rel) {
        Ok(None) => ClearedSource::NoRow,
        Err(_) => ClearedSource::Untouched,
        Ok(Some(_)) => match look_old() {
            OnDisk::Gone => ClearedSource::Dropped(deindex_removed_path(db, kb_path, old_rel)),
            back @ (OnDisk::Present | OnDisk::Unknown(_)) => ClearedSource::Kept(back),
        },
    };
    (dest, source)
}

/// The lines [`finish_rename_locked`] prints for what [`settle_cleared_rename`] did.
fn log_cleared_rename(
    old_rel: &str,
    new_rel: &str,
    (dest, source): (ClearedDestination, ClearedSource),
) {
    match dest {
        ClearedDestination::RowInPlace => {}
        ClearedDestination::RowUnread(e) => wdiag!(
            "watcher: after the failed rename, could not read the row of {new_rel}; left the rows as they were: {}",
            crate::embedder::body_free_message(&e)
        ),
        ClearedDestination::Reindexed(result) => {
            // A destination the reindex did not settle has no row: it was cleared before.
            let settled = write_reindex_settled_the_row(&result);
            let what = match result {
                Ok(result) => format!("reindexed {new_rel}: {result:?}"),
                Err(e) => format!(
                    "reindex {new_rel} failed: {}",
                    crate::embedder::body_free_message(&e)
                ),
            };
            if settled {
                wdiag!("watcher: after the failed rename, {what}");
            } else {
                wdiag!(
                    "watcher: after the failed rename, {what}; {new_rel} has no row until its next event or a full index"
                );
            }
        }
        ClearedDestination::NotReindexed(OnDisk::Gone) => {
            wdiag!("watcher: after the failed rename, {new_rel} was not reindexed (Gone)")
        }
        ClearedDestination::NotReindexed(on_disk) => wdiag!(
            "watcher: after the failed rename, {new_rel} was not reindexed ({on_disk:?}); it has no row until its next event or a full index"
        ),
    }
    match source {
        ClearedSource::Untouched | ClearedSource::NoRow => {}
        ClearedSource::Kept(on_disk) => wdiag!(
            "watcher: after the failed rename, left the row of {old_rel} as it was ({on_disk:?})"
        ),
        ClearedSource::Dropped(outcome) => log_written_path_gone(old_rel, outcome),
    }
}

/// (#544 PR-2) The line [`log_rename_outcome`] prints for
/// [`indexer::RenameOutcome::RenamedAcrossParsersAwaitingReparse`]: the rename crossed a
/// parser, the file changed while it was read, and the old parser's row waits under the new
/// path for the next read. A function of its own so a test can read the line.
fn awaiting_reparse_rename_message(old_rel: &str, new_rel: &str) -> String {
    format!(
        "watcher: renamed {old_rel} -> {new_rel} (the file changed while it was read; \
         previous row kept under the new path until it is read again)"
    )
}

/// The line [`dispatch_rename`] prints for what [`indexer::rename_single_file`] returned.
fn log_rename_outcome(old_rel: &str, new_rel: &str, outcome: Result<indexer::RenameOutcome>) {
    match outcome {
        Ok(indexer::RenameOutcome::Renamed) => {
            wdiag!("watcher: renamed {old_rel} -> {new_rel}");
        }
        Ok(indexer::RenameOutcome::RenamedAndReindexed { chunks }) => {
            wdiag!("watcher: renamed+reindexed {old_rel} -> {new_rel} ({chunks} chunks)");
        }
        Ok(indexer::RenameOutcome::OldPathMissing) => {
            wdiag!("watcher: rename target {old_rel} not in DB, indexed {new_rel}");
        }
        // (BU-20) Not "indexed": no row was created. Saying otherwise sends
        // whoever reads this log looking for a document that is not there.
        Ok(indexer::RenameOutcome::OldPathMissingAndRefused) => {
            wdiag!("watcher: rename target {old_rel} not in DB, and {new_rel} was refused");
        }
        Ok(indexer::RenameOutcome::RenamedSizeCapped) => {
            wdiag!(
                "watcher: renamed {old_rel} -> {new_rel} (binary too large, hash check skipped)"
            );
        }
        // (codex P2 round 12 on PR #291) The crossed-parser twins say the document is gone,
        // because it is: "content left as it was" would send a reader looking for a row
        // that was dropped.
        Ok(indexer::RenameOutcome::RenamedSizeCappedAndDropped) => {
            wdiag!(
                "watcher: renamed {old_rel} -> {new_rel} (too large for the new parser, \
                 document dropped from the index)"
            );
        }
        // (local Codex on PR #291 after round 13) Not a success: the reparse wrote nothing,
        // so the previous content (and, in Static mode, the previous path's breadcrumb)
        // is what the new path serves until the file is indexed again.
        Ok(indexer::RenameOutcome::RenamedButNotReindexed) => {
            wdiag!(
                "watcher: renamed {old_rel} -> {new_rel} (reparse skipped, previous content \
                 kept under the new path; see the reason above)"
            );
        }
        Ok(indexer::RenameOutcome::RenamedButRefusedAndDropped) => {
            wdiag!(
                "watcher: renamed {old_rel} -> {new_rel} (the new parser could not index it, \
                 document dropped from the index)"
            );
        }
        Ok(indexer::RenameOutcome::RenamedAcrossParsersAwaitingReparse) => {
            wdiag!("{}", awaiting_reparse_rename_message(old_rel, new_rel));
        }
        // (BU-20) The reason is already on stderr from `read_for_index_under`; this
        // line says what happened to the document, which the reason does not.
        Ok(indexer::RenameOutcome::RenamedButRefused) => {
            wdiag!(
                "watcher: renamed {old_rel} -> {new_rel} (new path refused, content left as it was)"
            );
        }
        // (audit AX-02) The reason is on stderr from the indexer; this says the row went.
        Ok(indexer::RenameOutcome::MovedOutOfTheKnowledgeBase) => {
            wdiag!("watcher: {new_rel} is reached through a link; deindexed {old_rel}");
        }
        // (AW-16) The helper the reindex arm uses, which the integration tests cover; a
        // rename is not driven there, since macOS (FSEvents) does not pair renames.
        Err(e) => wdiag!(
            "watcher: rename {old_rel} -> {new_rel} failed: {}",
            crate::embedder::body_free_message(&e)
        ),
    }
}

/// (feature-68) One stored document under a renamed directory, mapped to the
/// file rename it stands for.
#[derive(Debug, PartialEq, Eq)]
struct DirRenameStep {
    /// The stored path under the old directory.
    old_rel: String,
    /// The same path under the new directory.
    new_rel: String,
    /// What a rename of this one file would do, from [`rename_decision`].
    action: RenameAction,
}

/// (feature-68) Map each stored path under `old_dir` to its file rename under
/// `new_dir`, judged by the existing [`rename_decision`]: the old end as a
/// removal, the new end as a write. A move into an excluded or unaddressable
/// place therefore deindexes and a move out of one reindexes, exactly as a
/// single file rename would. Both full paths are `kb_path.join(rel)`.
///
/// Split out so a test can run the same decision as production with no
/// [`crate::embedder::Embedder`] ([`deindex_subtree_parts`] is the precedent).
///
/// The steps keep the order of `paths`. A path not under `old_dir/` is
/// skipped (the listing helper never returns one).
fn dir_rename_steps(
    paths: &[String],
    kb_path: &Path,
    old_dir: &str,
    new_dir: &str,
    registry: &Registry,
    rules: &crate::exclusion::ExclusionRules,
) -> Vec<DirRenameStep> {
    let old_prefix = format!("{old_dir}/");
    paths
        .iter()
        .filter_map(|p| {
            let rest = p.strip_prefix(&old_prefix)?;
            let new_rel = format!("{new_dir}/{rest}");
            let action = rename_decision(
                p,
                &kb_path.join(p),
                &new_rel,
                &kb_path.join(&new_rel),
                registry,
                rules,
            );
            Some(DirRenameStep {
                old_rel: p.clone(),
                new_rel,
                action,
            })
        })
        .collect()
}

/// (feature-68) What the caller saw at a rename's destination, from one
/// `symlink_metadata` taken outside the db lock. A symlink is not followed,
/// so a symlink to a directory is [`ToState::NotDir`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ToState {
    /// A directory is there.
    Dir,
    /// Something is there that is not a directory (a file, a symlink).
    NotDir,
    /// Nothing is there.
    Missing,
    /// The stat failed for a reason other than "not found" (permission, a
    /// sharing violation, an I/O error): something may be there.
    Unknown,
}

/// (feature-68) How a paired rename is handled once it is known to be a
/// directory rename.
#[derive(Debug, PartialEq, Eq)]
enum DirRenameEntry {
    /// The destination is a directory: move the rows under the old prefix,
    /// sweep what is left there, and scan the destination.
    ScanDestination,
    /// The destination is already gone but rows sit under the old prefix:
    /// drop the rows listed here, sweep the old prefix if the old directory
    /// is gone too, and neither move nor scan (local Codex r5: the listing is
    /// kept so a recreated old directory does not keep the listed rows).
    SweepOnly(Vec<String>),
}

/// (feature-68) Decide whether a paired rename is a directory rename.
/// `None` leaves it to the file rename as before.
///
/// - [`ToState::Dir`]: [`DirRenameEntry::ScanDestination`], without listing
///   or looking at the store argument.
/// - [`ToState::NotDir`] and [`ToState::Unknown`]: `None`, without listing.
/// - [`ToState::Missing`]: list the rows under `old_rel` in the store
///   argument; any row means [`DirRenameEntry::SweepOnly`] carrying them
///   (sorted, as listed), none (or no store given) means `None`.
///
/// The stat and the lock belong to the caller
/// ([`dispatch_directory_rename`]), the decision lives here, and the caller keeps the db
/// guard inside the scope of this call. This function stats, locks and logs
/// nothing. A listing error is returned as `Err` rather than folded into
/// `None`, so the caller can log it before falling back to the file rename.
fn dir_rename_entry(
    to: ToState,
    db: Option<&Database>,
    old_rel: &str,
) -> Result<Option<DirRenameEntry>> {
    match (to, db) {
        (ToState::Dir, _) => Ok(Some(DirRenameEntry::ScanDestination)),
        // (local Codex r4) An unknown destination is never read as gone: the
        // rows it would sweep may belong to a directory that is still there.
        (ToState::NotDir | ToState::Unknown, _) => Ok(None),
        (ToState::Missing, None) => Ok(None),
        (ToState::Missing, Some(db)) => {
            let paths = db.document_paths_under(old_rel)?;
            if paths.is_empty() {
                Ok(None)
            } else {
                Ok(Some(DirRenameEntry::SweepOnly(paths)))
            }
        }
    }
}

/// (feature-68) What is at a rename's destination, from one `symlink_metadata`.
/// A symlink is not followed. Only "not found" reads as [`ToState::Missing`];
/// any other failure is [`ToState::Unknown`] and comes back with its error so
/// the caller can log it (local Codex r4). Unlike [`gate_parts`], which only
/// declines to read a path it cannot stat, "missing" here can lead to a
/// sweep that deletes rows, so a failure that does not prove absence must not
/// count as one.
fn to_state(p: &Path) -> (ToState, Option<std::io::Error>) {
    match stat_unless_gone(p) {
        Ok(Some(m)) if m.file_type().is_dir() => (ToState::Dir, None),
        Ok(Some(_)) => (ToState::NotDir, None),
        Ok(None) => (ToState::Missing, None),
        Err(e) => (ToState::Unknown, Some(e)),
    }
}

/// (feature-68) The one answer to "is this end of a directory rename gone?",
/// for both ends: `symlink_metadata` (a symlink is not followed), with
/// `Ok(None)` only for `std::io::ErrorKind::NotFound`. Any other failure
/// (permission, a sharing violation, I/O) is returned as `Err`, because on
/// this path "gone" leads to deleting rows and such a failure does not prove
/// absence (local Codex r4, and the source end after the re-review of it).
/// (#544 A14) Only the destination end asks it now; the source end is
/// [`sweep_source_if_gone`], which also reads not-a-directory as gone.
fn stat_unless_gone(p: &Path) -> std::io::Result<Option<std::fs::Metadata>> {
    match std::fs::symlink_metadata(p) {
        Ok(m) => Ok(Some(m)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// (feature-68) Handle a paired rename as a directory rename when it is one.
/// Returns `true` when it was handled here, `false` to fall through to the
/// file rename, which is left exactly as it was.
///
/// It is a directory rename when `to` is a real directory (a symlink is not
/// followed), or when `to` is already gone and the store holds rows under
/// `old_rel/` (the handler fell behind a chain of renames). The decision is
/// [`dir_rename_entry`]; the KB root (an empty rel at either end) never enters,
/// and neither does a paired event whose two ends are the same path, which
/// notify-debouncer-full emits for a rename away and back inside one debounce
/// window: it falls through and the file rename skips it.
/// A destination that cannot be stat'ed for a reason other than "not found"
/// is logged and left to the file rename. If the event was a directory
/// rename, that is a skip for two directory names: its rows stay under the
/// old path rather than being swept while the directory may still be there,
/// and a full index recovers them. (#544 L-h) The destination's gone is
/// [`stat_unless_gone`]'s (only not-found), the source's is
/// [`sweep_source_if_gone`]'s under the db lock (not-found or not-a-directory,
/// [`indexer::stat_error_means_gone`]); the two never both act in one call.
/// (#544 P1-S) The destination's spelling is looked at once, in the listing's
/// lock region ([`arrived_directory_on_disk`]): a destination on disk only
/// under another spelling moves no row and is not scanned
/// ([`dir_rename_moves_its_steps`]); the listed rows still leave the old path.
///
/// - [`DirRenameEntry::ScanDestination`]: every row under `old_rel/` goes
///   through its file rename ([`dir_rename_steps`], judged by the existing
///   [`rename_decision`] and run by the existing [`dispatch_rename`] /
///   [`dispatch_removal`] / [`dispatch_reindex`]); then every listed row is
///   dropped from its old path through [`dispatch_removal`] (not looked at for
///   one already moved), so no listed row stays there even when the old
///   directory was recreated; rows added under the old prefix after the listing are
///   swept by [`sweep_source_if_gone`] once `from` is gone; last,
///   [`dispatch_new_directory`] scans `to` for files not indexed yet.
/// - [`DirRenameEntry::SweepOnly`]: no move and no scan. When `from` is gone,
///   one range sweep by [`sweep_source_if_gone`] drops the listed rows and
///   any added since. Otherwise (`from` recreated, or its stat failed) only
///   the rows the entry decision listed are dropped, one by one through
///   [`dispatch_removal`], so a recreated directory's own rows are kept.
///   Renaming the rows onto a destination that no longer exists would drop
///   them while logging a rename; the later event for the new name scans it.
///
/// On both paths, the row stored under `old_rel` itself (a file, not under
/// `old_rel/`) is dropped through [`dispatch_removal`], once, before the
/// destination scan: the file rename the event may stand for would have
/// dropped it, and for a real directory rename it is a no-op.
///
/// (kuriya 259) Each of these drops is a rename away
/// ([`RemovalCause::RenamedAway`]): a file back at the old path that the write
/// gate takes keeps its row (reindexed in place), looked at inside the database
/// lock of the delete; a path whose row is already gone is not looked at.
///
/// The db is locked only while listing, never across the loop, so the MCP search
/// tool is not held up for the size of the directory. The listing guard is
/// dropped before any `dispatch_*` call, because each of those locks the db
/// again and a std `Mutex` is not reentrant. The embedder is not locked here.
/// When `to` exists and is not a directory, the db is not locked at all.
fn dispatch_directory_rename(
    state: &WatcherState,
    from: &Path,
    to: &Path,
    old_rel: &str,
    new_rel: &str,
) -> bool {
    if old_rel.is_empty() || new_rel.is_empty() {
        return false;
    }
    // (re-review) Both ends the same path: the debouncer folds a rename away
    // and back inside one window into one paired event on the same path.
    // Taken here it would rename each row onto itself, drop them all and
    // re-embed the directory. Left to the file rename it is a skip, so changes
    // made under the temporary name inside the window are not picked up until
    // the file changes again or a full index runs: the debouncer does not move
    // the children's queues, so such an edit arrives under the temporary path
    // and is skipped as a file that no longer exists.
    if old_rel == new_rel {
        return false;
    }
    let (to_kind, stat_err) = to_state(to);
    if let Some(e) = &stat_err {
        wdiag!(
            "watcher: could not stat a rename destination: {e}. Handled as a file rename; if it was a directory, the rows under its old path stay until the next full `groove index` (or this server's MCP `rebuild_index`)."
        );
    }
    let entry = match to_kind {
        ToState::Missing => {
            let db = recover_db(state.lock_for_batch(&state.db));
            dir_rename_entry(to_kind, Some(&db), old_rel)
            // `db` is dropped here, before the outcome is acted on.
        }
        ToState::Dir | ToState::NotDir | ToState::Unknown => {
            dir_rename_entry(to_kind, None, old_rel)
        }
    };
    let entry = match entry {
        Ok(Some(entry)) => entry,
        Ok(None) => return false,
        Err(e) => {
            wdiag!(
                "watcher: listing the documents of a renamed directory failed: {e}. They stay under the old path until the next full `groove index` (or this server's MCP `rebuild_index`)."
            );
            return false;
        }
    };
    // (#544 P1-S) Whether the rows move to `to`, from one look at its spelling taken in the
    // listing's lock region. A sweep-only entry moves nothing whatever this says.
    let mut moves = true;
    if entry == DirRenameEntry::ScanDestination {
        let (listed, to_spelled) = {
            let db = recover_db(state.lock_for_batch(&state.db));
            (
                db.document_paths_under(old_rel),
                arrived_directory_on_disk(&state.kb_path, new_rel, to),
            )
        };
        moves = dir_rename_moves_its_steps(to_spelled, rel_is_lossy(&state.kb_path, Some(to)));
        if !moves {
            wdiag!(
                "watcher: the renamed directory {new_rel} is not on disk under that spelling; its rows leave the old path and are not moved there"
            );
        }
        let paths = match listed {
            Ok(paths) => paths,
            Err(e) => {
                wdiag!(
                    "watcher: listing the documents of a renamed directory failed: {e}. Their rows are not moved: the old directory is deindexed if it is gone, and the destination is scanned as new."
                );
                Vec::new()
            }
        };
        if !paths.is_empty() {
            let n = paths.len();
            wdiag!(
                "watcher: a renamed directory held {n} indexed document{}; following the rename for them (the file system reports only the directory).",
                if n == 1 { "" } else { "s" }
            );
            let steps = if moves {
                dir_rename_steps(
                    &paths,
                    &state.kb_path,
                    old_rel,
                    new_rel,
                    &state.registry,
                    &state.rules,
                )
            } else {
                Vec::new()
            };
            for step in &steps {
                match step.action {
                    // (#544) Only whether a file is there: the destination directory was
                    // spelled once above, and below it a step's `new_rel` is the row's own
                    // spelling.
                    RenameAction::Rename => {
                        dispatch_rename(state, &step.old_rel, &step.new_rel, WriteLook::ExistsOnly)
                    }
                    RenameAction::Deindex => dispatch_removal(
                        state,
                        &step.old_rel,
                        &state.kb_path.join(&step.old_rel),
                        RemovalCause::RenamedAway,
                    ),
                    // (#544) No event path for a step (its `new_rel` is built from the
                    // row's), so it is read as not lossy and only whether a file is there
                    // is asked: a lossy row's step looks gone, the answer
                    // `rename_single_file` already gives such a destination.
                    RenameAction::Reindex => dispatch_reindex_from(
                        state,
                        &step.new_rel,
                        ReindexOrigin::Write {
                            raw: None,
                            look: WriteLook::ExistsOnly,
                        },
                    ),
                    RenameAction::Skip => {}
                }
            }
            // (local Codex r1) Every listed row leaves its old path, whether or
            // not `from` is gone: a row the pass above did not move (a skip, a
            // reindex of the new path, a rename that failed) would otherwise stay
            // as a hit for a path the old directory may hold again under other
            // contents. (kuriya 259) Unless a file the write gate takes is back
            // there, which is reindexed in place. A row already moved is not
            // looked at.
            for p in &paths {
                dispatch_removal(state, p, &state.kb_path.join(p), RemovalCause::RenamedAway);
            }
        }
    }
    // The old prefix is swept only when `from` is known to be gone. A stat that
    // fails for another reason does not prove that, and sweeping a directory
    // that is still there would delete its live rows. (#544 A14) The stat and the
    // range delete share one db lock region; on the sweep-only path the delete
    // takes the listed rows too.
    let swept = {
        let db = recover_db(state.lock_for_batch(&state.db));
        sweep_source_if_gone(&db, &state.kb_path, from)
        // `db` is dropped here, before any `dispatch_*` locks it again.
    };
    let from_gone = match swept {
        SourceSweep::Swept(Some((rel, outcome))) => {
            log_subtree_deindex(&rel, outcome);
            true
        }
        SourceSweep::Swept(None) => true,
        SourceSweep::Present => false,
        SourceSweep::Unknown(e) => {
            wdiag!(
                "watcher: could not stat the source of a renamed directory: {e}. Rows added under its old path after the listing stay until the next full `groove index` (or this server's MCP `rebuild_index`)."
            );
            false
        }
    };
    if !from_gone && let DirRenameEntry::SweepOnly(paths) = &entry {
        // (local Codex r5, r6) `from` was recreated, or cannot be stat'ed: the
        // listed rows predate the rename and leave their old paths, but one by
        // one, so a recreated directory's own rows are not taken by a prefix
        // sweep. (kuriya 259) A listed row whose path holds again a file the
        // write gate takes stays, reindexed in place. Rare, so the per-row cost
        // (a lock and a look each) is paid only here.
        for p in paths {
            dispatch_removal(state, p, &state.kb_path.join(p), RemovalCause::RenamedAway);
        }
    }
    // (local Codex r6, r7) On both paths, the row stored under the old path
    // itself, which is not under `old_rel/` and so neither listed above nor in
    // the sweep's range: a file renamed onto a name that is a directory by the
    // time the event is handled, or a file once stored at the name the renamed
    // directory later took. The file rename would have dropped it. For a real
    // directory rename there is no such row and this is a no-op (one deletes
    // the exact row, the sweep what is strictly under it, as in feature-67).
    // (kuriya 259) Unless a file the write gate takes is back at the old path,
    // which is reindexed in place.
    dispatch_removal(state, old_rel, from, RemovalCause::RenamedAway);
    if entry == DirRenameEntry::ScanDestination && moves {
        dispatch_new_directory(state, to, new_rel);
    }
    true
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// (audit AX-02) The watcher is built with notify's `follow_symlinks` off. Runs on every
    /// platform, so turning it back on is red in every PR leg, not only on Linux where it bites.
    #[test]
    fn the_watcher_does_not_ask_notify_to_follow_symlinks() {
        assert!(!notify_config().follow_symlinks());
        // and nothing else of the default changed
        assert_eq!(
            notify_config().with_follow_symlinks(true),
            notify::Config::default()
        );
    }

    /// (audit AX-02) On Linux, a recursive inotify watch built from [`notify_config`] puts no
    /// watch on a directory symlink's target: a file written there produces no event under the
    /// link's in-KB name, while a file written in the KB itself does. With notify's default
    /// (`follow_symlinks: true`) the target's write arrives as `kb/notes/secret.md`, before the
    /// in-KB one (one inotify descriptor, events in order).
    #[cfg(target_os = "linux")]
    #[test]
    fn an_inotify_watch_does_not_reach_through_a_directory_symlink() {
        use notify::Watcher as _;
        use std::time::Instant;

        struct Dir(PathBuf);
        impl Drop for Dir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let root = Dir(crate::test_support::unique_temp_path("groove-ax02-inotify"));
        let kb = root.0.join("kb");
        let outside = root.0.join("outside");
        std::fs::create_dir_all(&kb).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, kb.join("notes")).unwrap();

        let (tx, rx) = std::sync::mpsc::channel::<notify::Result<notify::Event>>();
        let mut watcher = notify::RecommendedWatcher::new(tx, notify_config()).unwrap();
        watcher.watch(&kb, RecursiveMode::Recursive).unwrap();

        std::fs::write(outside.join("secret.md"), "outside the knowledge base").unwrap();
        std::fs::write(kb.join("in.md"), "inside the knowledge base").unwrap();

        let in_kb = kb.join("in.md");
        // Receive until the in-KB write has been seen and then nothing has come for `QUIET`,
        // under one overall cap. The secret is written first, and one inotify descriptor
        // delivers in order, so a watch through the link would report it before `in.md`; the
        // quiet drain after `in.md` also takes anything that comes later, so the assertion
        // below never passes for having stopped listening early. A notify error fails the test.
        const QUIET: Duration = Duration::from_millis(500);
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut seen: Vec<PathBuf> = Vec::new();
        loop {
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            let step = if seen.contains(&in_kb) {
                QUIET
            } else {
                Duration::from_millis(200)
            };
            let wait = step.min(deadline - now);
            match rx.recv_timeout(wait) {
                Ok(Ok(event)) => seen.extend(event.paths),
                Ok(Err(e)) => panic!("notify reported an error: {e:?}"),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) if seen.contains(&in_kb) => break,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    panic!("the watcher's channel closed: {seen:?}")
                }
            }
        }
        assert!(
            seen.contains(&in_kb),
            "the in-KB write was not seen: {seen:?}"
        );
        let through_link: Vec<&PathBuf> = seen
            .iter()
            .filter(|p| p.starts_with(kb.join("notes")))
            .collect();
        assert!(
            through_link.is_empty(),
            "a write outside the KB arrived under the link: {through_link:?}"
        );
    }

    #[test]
    fn test_watch_config_default() {
        let c = WatchConfig::default();
        assert!(c.enabled);
        assert_eq!(c.debounce_ms, 500);
    }

    #[test]
    fn test_watch_config_from_toml_full() {
        let toml = "enabled = false\ndebounce_ms = 1000\n";
        let c: WatchConfig = toml::from_str(toml).unwrap();
        assert!(!c.enabled);
        assert_eq!(c.debounce_ms, 1000);
    }

    #[test]
    fn test_watch_config_from_toml_partial_uses_defaults() {
        let c: WatchConfig = toml::from_str("debounce_ms = 250\n").unwrap();
        assert!(c.enabled, "missing enabled must default to true");
        assert_eq!(c.debounce_ms, 250);
    }

    #[test]
    fn test_watch_config_rejects_unknown_fields() {
        let err: Result<WatchConfig, _> = toml::from_str("enabled = true\nbogus = 1\n");
        assert!(err.is_err());
    }

    #[test]
    fn test_to_rel_basic() {
        // Was a fixed name, so every concurrent run and every rerun shared one
        // directory — and the `remove_dir_all` below deleted it out from under
        // whoever else was mid-test.
        let kb = crate::test_support::unique_temp_path("groove-watcher-torel");
        std::fs::create_dir_all(&kb).unwrap();
        let kb = kb.canonicalize().unwrap();
        let full = kb.join("notes").join("a.md");
        std::fs::create_dir_all(full.parent().unwrap()).unwrap();
        std::fs::write(&full, "").unwrap();
        let full = full.canonicalize().unwrap();
        assert_eq!(to_rel(&kb, &full), Some("notes/a.md".to_string()));
        let _ = std::fs::remove_dir_all(&kb);
    }

    /// What the three dispatchers call after they have written or removed the
    /// real key, against an index a version up to 1.12.0 built: the folded row
    /// goes once the real key is in (modify / create / rename-to) or
    /// unconditionally (delete / rename-from), and stays while the real key is
    /// not in -- a refused or skipped reindex leaves the index as it was, and
    /// for this file the folded row is the index as it was.
    #[cfg(unix)]
    #[test]
    fn test_sweep_legacy_row_follows_the_real_key_and_never_runs_ahead_of_it() {
        let kb = crate::test_support::unique_temp_path("groove-watcher-legacy-row");
        std::fs::create_dir_all(&kb).unwrap();
        std::fs::write(kb.join("secret\\pay.md"), "# pay").unwrap();
        let db = Database::open_in_memory().unwrap();
        db.verify_embedding_meta("bge-small-en-v1.5", 384).unwrap();
        let seed = |path: &str| {
            db.upsert_document(path, None, None, None, None, &[], None, "h", 0)
                .unwrap();
        };
        let has = |path: &str| db.get_document_hash(path).unwrap().is_some();

        // A reindex that wrote nothing: the folded row is all there is.
        seed("secret/pay.md");
        sweep_legacy_row(&db, &kb, "secret\\pay.md", true);
        assert!(has("secret/pay.md"));

        // The reindex wrote the real key: the duplicate goes.
        seed("secret\\pay.md");
        sweep_legacy_row(&db, &kb, "secret\\pay.md", true);
        assert!(has("secret\\pay.md"));
        assert!(!has("secret/pay.md"));

        // A delete: the folded row goes whether or not the real key ever existed.
        seed("secret/pay.md");
        sweep_legacy_row(&db, &kb, "secret\\pay.md", false);
        assert!(!has("secret/pay.md"));

        // A real file at the folded path owns that row.
        std::fs::create_dir_all(kb.join("secret")).unwrap();
        std::fs::write(kb.join("secret").join("pay.md"), "# another").unwrap();
        seed("secret/pay.md");
        sweep_legacy_row(&db, &kb, "secret\\pay.md", false);
        assert!(has("secret/pay.md"));

        let _ = std::fs::remove_dir_all(&kb);
    }

    /// The watcher writes the same `documents.path` the full index walk does,
    /// so it has to spell a literal `\` the same way: left alone on Unix. A
    /// folded key here would also send the reindex to `kb/secret/pay.md`, a
    /// path that does not exist.
    #[cfg(unix)]
    #[test]
    fn test_to_rel_keeps_a_literal_backslash_on_unix() {
        let kb = Path::new("/kb");
        assert_eq!(
            to_rel(kb, Path::new("/kb/secret\\pay.md")),
            Some("secret\\pay.md".to_string())
        );
    }

    /// `should_process` は WatcherState のうち `kb_path` / `registry` /
    /// `exclude_dirs` しか見ないので、test 用にその 3 つだけ差し込んだ
    /// 軽量判定ヘルパを用意する (`Database` / `Embedder` のダミー構築を
    /// 避けるため)。
    ///
    /// full-audit 2026-07-26 (H-1): 以前はここに本番 `should_process` の
    /// 逐語コピーが置かれており、**テストがコピーだけを検証していて本番の
    /// skip ルールを変えても全部 green のまま**だった。判定本体を
    /// `should_process_parts` に切り出し、ここは委譲だけにすることで、
    /// 既存 assert を 1 文字も変えずにテストが本番経路を踏むようにした。
    ///
    /// (feature-49) `exclude_dirs` を受け取る形はそのまま残してある。
    /// `ExclusionRules` を組み立てるのはここの仕事で、既存の呼び出し側 20 箇所は
    /// 1 文字も変わらない。`.grooveignore` を絡める新しいテストは
    /// [`rules_with_ignore_file`] を使う。
    fn should_process_lite(
        rel: &str,
        full: &Path,
        registry: &Registry,
        exclude_dirs: &[String],
    ) -> bool {
        let rules = crate::exclusion::ExclusionRules::from_exclude_dirs(exclude_dirs.to_vec());
        should_process_parts(rel, full, registry, &rules)
    }

    /// Regression (full-audit 2026-08-12、セキュリティ軸 H-1): full re-index は
    /// `collect_source_files` が `follow_links(false)` + `is_file()` で symlink を
    /// 落とし、`get_document` も `validate_get_document_path` で明示的に拒否する。
    /// **live watcher だけが同じ制御を持っていなかった**ので、KB に書ける者が
    /// `notes.md -> <KB 外の秘密>` を張ると watcher がリンク先を読んで index し、
    /// `search` から中身が平文で返っていた (full rebuild すると消えるので痕跡も残らない)。
    ///
    /// Windows では symlink 作成に開発者モードか管理者権限が要るので、作れなかった
    /// 場合は skip する (Linux CI では必ず実行される)。
    #[test]
    fn test_symlink_is_not_indexed_by_the_watcher() {
        let kb = crate::test_support::unique_temp_path("groove-watcher-symlink");
        std::fs::create_dir_all(&kb).unwrap();
        let secret = kb.join("outside-secret.txt");
        std::fs::write(&secret, "ssh-rsa AAAA...").unwrap();
        let link = kb.join("notes.md");

        #[cfg(unix)]
        let made = std::os::unix::fs::symlink(&secret, &link).is_ok();
        #[cfg(windows)]
        let made = std::os::windows::fs::symlink_file(&secret, &link).is_ok();

        let registry = Registry::defaults();

        if made {
            assert!(
                !should_process_lite("notes.md", &link, &registry, &default_exclude_dirs()),
                "a symlink must not be indexed by the watcher"
            );
        } else {
            // Windows は symlink 作成に開発者モードか管理者権限が要る (WinError 1314)。
            // **黙って return すると「通ったのに何も検証していない」テストになる**ので、
            // skip したことを出力に残す。Linux CI では必ず上の assert が走る。
            eprintln!(
                "test_symlink_is_not_indexed_by_the_watcher: could not create a \
                 symlink, so the symlink branch is skipped (Windows without the \
                 privilege). Only the branch below is verified."
            );
        }

        // 素のファイルは従来どおり通る (guard が広すぎないことの確認)。
        let plain = kb.join("real.md");
        std::fs::write(&plain, "# Real\n").unwrap();
        assert!(should_process_lite(
            "real.md",
            &plain,
            &registry,
            &default_exclude_dirs()
        ));

        // 削除イベントではファイルが既に無い。ここで弾くと deindex が
        // 動かなくなるので、metadata が取れないパスは通す。
        let gone = kb.join("deleted.md");
        assert!(should_process_lite(
            "deleted.md",
            &gone,
            &registry,
            &default_exclude_dirs()
        ));

        // hardlink は **Windows でも特権不要**なので、symlink を作れない環境でも
        // 「リンクの向こう側を読ませない」意図をここで踏める。
        //
        // **契約変更 (BU-20、2026-08-14、user 承認済み)**: 以前ここは
        // 「hardlink は通る」= 既知の残存リスクを pin していた。symlink と同じ
        // 脅威 (KB に書ける者が、読めないファイルを groove の権限で読ませる) を
        // 同じ扱いに揃えたので、assert を反転させた。代償は「hardlink で dedup /
        // 共有している KB のファイルが index されなくなる」ことで、これは
        // `links::refusal_reason` のログで説明される。
        //
        // なお、この hardlink の相手 `outside-secret.txt` は名前に反して **KB 内**に
        // ある。guard は「リンクが 2 本以上ある」ことしか見ない (相手がどこに
        // いるかは portable に分からない) ので、それでも弾かれるのが正しい。
        let hard = kb.join("hardlink.md");
        if std::fs::hard_link(&secret, &hard).is_ok() {
            assert!(
                !should_process_lite("hardlink.md", &hard, &registry, &default_exclude_dirs()),
                "hardlink も watcher が index してはいけない (BU-20)"
            );
            // リンクは**対称**なので先にあった方も弾かれる。両方 `.md` にして
            // 確かめる: 拡張子フィルタ (この guard の直前) で落ちる組み合わせだと、
            // assert は「hardlink を弾いた」ことを何も検証しなくなる。
            let note_a = kb.join("note-a.md");
            std::fs::write(&note_a, "# A\n").unwrap();
            assert!(
                should_process_lite("note-a.md", &note_a, &registry, &default_exclude_dirs()),
                "名前が 1 つのうちは通る"
            );
            let note_b = kb.join("note-b.md");
            std::fs::hard_link(&note_a, &note_b).unwrap();
            assert!(
                !should_process_lite("note-a.md", &note_a, &registry, &default_exclude_dirs()),
                "後から 2 つ目の名前が付いた既存ノートも弾かれる (承認済みの代償)"
            );
            assert!(!should_process_lite(
                "note-b.md",
                &note_b,
                &registry,
                &default_exclude_dirs()
            ));

            // リンクを外せば戻る = 効くのは「名前が 2 つある間」だけ。
            std::fs::remove_file(&note_b).unwrap();
            assert!(
                should_process_lite("note-a.md", &note_a, &registry, &default_exclude_dirs()),
                "2 つ目の名前が消えたら通る"
            );
        }

        let _ = std::fs::remove_dir_all(&kb);
    }

    fn default_exclude_dirs() -> Vec<String> {
        vec![".obsidian".to_string()]
    }

    /// Regression (codex P2 on PR #81): rename の両端で可否が食い違うとき、
    /// 以前は「どちらか一方が対象なら rename」だったため、index 済み
    /// ファイルを `.git/` や `node_modules/` へ rename すると
    /// `rename_single_file` が denylist 配下の新パスへ DB を書き換えて残した。
    /// 除外したはずのファイルが index に居座る。
    #[test]
    fn test_rename_action_covers_both_endpoints() {
        assert_eq!(rename_action(true, true), RenameAction::Rename);
        // 対象 → 除外領域: 追跡をやめる (以前はここが Rename だった)
        assert_eq!(rename_action(true, false), RenameAction::Deindex);
        // 除外領域 → 対象: 新規として取り込む (以前はここも Rename)
        assert_eq!(rename_action(false, true), RenameAction::Reindex);
        assert_eq!(rename_action(false, false), RenameAction::Skip);
    }

    /// Regression (full-audit 2026-07-26 AU-03): `HARDCODED_EXCLUDE_DIRS` は
    /// ユーザ設定に関わらず常に効く denylist だが、`collect_source_files`
    /// (indexer) と `validate_collect_md_files` (main) だけが適用しており
    /// **watcher は素通し**だった。`exclude_dirs` を空にした設定では、フル
    /// index が skip する `.git/` `node_modules/` を live watcher だけが
    /// 拾ってしまう (`npm install` 一発で KB が汚染される)。
    #[test]
    fn test_should_process_applies_hardcoded_excludes_even_with_empty_config() {
        let reg = Registry::defaults();
        let empty: Vec<String> = Vec::new();
        // HARDCODED_EXCLUDE_DIRS = [".git", ".svn", "node_modules"]。
        for rel in [
            "node_modules/pkg/README.md",
            ".git/COMMIT_EDITMSG.md",
            ".svn/entries.md",
            "sub/node_modules/deep/x.md",
        ] {
            let full = Path::new("/tmp/kb").join(rel);
            assert!(
                !should_process_lite(rel, &full, &reg, &empty),
                "{rel} must be skipped by the hardcoded denylist regardless of exclude_dirs"
            );
        }
        // denylist に無い通常ファイルは従来どおり通す。
        let ok = Path::new("/tmp/kb/notes/a.md");
        assert!(should_process_lite("notes/a.md", ok, &reg, &empty));
    }

    /// (BU-19, codex P1 on PR #141) The watcher applies `exclude_dirs` with
    /// the same case rules as the index walk.
    ///
    /// This is AU-03's shape a second time. BU-19 made
    /// `collect_source_files` and `validate` match case-insensitively so a
    /// directory on disk called `Build` could not walk past
    /// `exclude_dirs = ["build"]`. The watcher kept its own exact comparison,
    /// which is worse than before the fix: the full index skips the directory
    /// while the live watcher incrementally indexes everything written into
    /// it. Both now go through `indexer::is_user_excluded_dir`.
    #[test]
    fn watcher_applies_exclude_dirs_with_the_same_case_rules_as_the_index_walk() {
        let reg = Registry::defaults();
        let excludes = vec!["build".to_string(), "Cache".to_string()];

        for rel in [
            "build/out.md",
            "Build/out.md",
            "BUILD/out.md",
            "sub/Build/deep/out.md",
            "cache/x.md",
            "CACHE/x.md",
        ] {
            let full = Path::new("/tmp/kb").join(rel);
            assert!(
                !should_process_lite(rel, &full, &reg, &excludes),
                "{rel} is under a configured exclusion however it is capitalised, \
                 so the watcher must ignore it — otherwise it indexes what the \
                 full index walk skips"
            );
        }

        // The bound is still a bound: a different directory is not excluded.
        for rel in ["build-output/x.md", "notes/rebuild/x.md", "cached/x.md"] {
            let full = Path::new("/tmp/kb").join(rel);
            assert!(
                should_process_lite(rel, &full, &reg, &excludes),
                "{rel} names a different directory and must still be watched"
            );
        }
    }

    /// (feature-49) The same shape as the test above, for the third exclusion
    /// layer — and this time both surfaces are run, rather than one being
    /// trusted to follow the other.
    ///
    /// AU-03 and BU-19 were both "the walk and the watcher answered
    /// differently", and both were found after they shipped, because each side
    /// had a test that only ever asked its own side. Here one `.grooveignore`
    /// is written to one directory, and every path is put through
    /// `collect_source_files` (the full index walk) **and**
    /// `should_process_parts` (the live watcher); the assertion is that they
    /// agree, whatever the answer is.
    #[test]
    fn the_index_walk_and_the_watcher_agree_about_a_grooveseekignore() {
        let kb = crate::test_support::unique_temp_path("kb-watch-ignore-agree");
        std::fs::create_dir_all(&kb).unwrap();
        struct Cleanup(std::path::PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let _cleanup = Cleanup(kb.clone());

        // Every case the two implementations could disagree about: a plain
        // pattern, a directory pattern, an anchored one, a `**` one, a
        // negation, and a negation under an ignored parent.
        std::fs::write(
            kb.join(crate::exclusion::IGNORE_FILE_NAME),
            "*.tmp.md\ndrafts/\n/root-only.md\narchive/**\nnotes/*.md\n!notes/keep.md\n\
             logs/\n!logs/important.md\n",
        )
        .unwrap();

        let cases = [
            "keep.md",
            "a.tmp.md",
            "deep/b.tmp.md",
            "drafts/x.md",
            "drafts/deep/x.md",
            "root-only.md",
            "sub/root-only.md",
            "archive/x.md",
            "archive/deep/x.md",
            "notes/drop.md",
            "notes/keep.md",
            "logs/important.md",
            // The other side of each boundary.
            "draftsy/x.md",
            "notes/deep/x.md",
            "a.tmp",
        ];
        for rel in cases {
            let full = kb.join(rel.replace('/', std::path::MAIN_SEPARATOR_STR));
            std::fs::create_dir_all(full.parent().unwrap()).unwrap();
            std::fs::write(&full, "# x\n").unwrap();
        }

        let reg = Registry::defaults();
        let rules = crate::exclusion::ExclusionRules::load(&kb, default_exclude_dirs());
        assert_eq!(
            rules.ignore_file_patterns(),
            Some(8),
            "the fixture's patterns must all have compiled, or this proves nothing"
        );

        let collected = crate::indexer::collect_source_files(&kb, &reg, &rules).unwrap();
        let walked: std::collections::HashSet<String> = collected
            .iter()
            .map(|p| crate::exclusion::rel_key(&kb, p))
            .collect();

        for rel in cases {
            if !rel.ends_with(".md") {
                continue; // the extension filter, not the exclusion, decides these
            }
            let full = kb.join(rel.replace('/', std::path::MAIN_SEPARATOR_STR));
            let watcher_takes_it = should_process_parts(rel, &full, &reg, &rules);
            let walk_takes_it = walked.contains(rel);
            assert_eq!(
                walk_takes_it, watcher_takes_it,
                "the index walk and the watcher disagree about {rel}: walk={walk_takes_it}, \
                 watcher={watcher_takes_it}. One of them is indexing what the other skips, \
                 which is exactly AU-03 and BU-19"
            );
        }

        // And the answers themselves, so "they agree" cannot become "they are
        // both wrong in the same way".
        assert!(walked.contains("keep.md"));
        assert!(
            walked.contains("notes/keep.md"),
            "a negation must re-include"
        );
        assert!(walked.contains("sub/root-only.md"), "a leading / anchors");
        assert!(!walked.contains("a.tmp.md"));
        assert!(!walked.contains("drafts/deep/x.md"));
        assert!(!walked.contains("root-only.md"));
        assert!(!walked.contains("archive/deep/x.md"));
        assert!(!walked.contains("notes/drop.md"));
        assert!(
            !walked.contains("logs/important.md"),
            "an ignored parent cannot be undone by a ! line for a file inside it"
        );
        assert!(walked.contains("draftsy/x.md"));
        assert!(
            walked.contains("notes/deep/x.md"),
            "`notes/*.md` must not reach a deeper level"
        );
    }

    /// (ADR-0023) The walk and the watcher agree about names Windows cannot
    /// open as written, and the answer is "not indexed": a device name at file
    /// level, a directory ending in a dot or a space above the file. A rename
    /// onto such a name is then a deindex (`rename_action(true, false)`).
    #[cfg(windows)]
    #[test]
    fn the_index_walk_and_the_watcher_agree_about_names_win32_cannot_spell() {
        let scratch = crate::test_support::unique_temp_path("kb-watch-unspellable");
        std::fs::create_dir_all(&scratch).unwrap();
        // The verbatim form, the only one under which these names are entries
        // on disk; removed through it too.
        let kb = scratch.canonicalize().unwrap();
        struct Cleanup(std::path::PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let _cleanup = Cleanup(kb.clone());

        // Backslashes: under `\\?\` a `/` is not a separator.
        let cases = ["ok.md", "CON.md", "nul.md", r"dir.\x.md", r"sp \y.md"];
        for rel in cases {
            let full = kb.join(rel);
            std::fs::create_dir_all(full.parent().unwrap()).unwrap();
            std::fs::write(&full, "# x\n").unwrap();
            assert!(full.is_file(), "the fixture must hold {rel:?}");
        }

        let reg = Registry::defaults();
        let rules = crate::exclusion::ExclusionRules::load(&kb, default_exclude_dirs());
        let walked: std::collections::HashSet<String> =
            crate::indexer::collect_source_files(&kb, &reg, &rules)
                .unwrap()
                .iter()
                .map(|p| crate::exclusion::rel_key(&kb, p))
                .collect();

        for native in cases {
            let rel = native.replace('\\', "/");
            let full = kb.join(native);
            let watcher_takes_it = should_process_parts(&rel, &full, &reg, &rules);
            assert_eq!(
                walked.contains(&rel),
                watcher_takes_it,
                "the index walk and the watcher disagree about {rel}"
            );
            assert_eq!(watcher_takes_it, rel == "ok.md", "{rel}");
        }
    }

    /// (ADR-0023) An event can name a file under a spelling of the knowledge
    /// base other than `kb_path`'s -- here the directory's own name in another
    /// case. The walk judges no name there, so the watcher judges it on the
    /// `rel` that [`to_rel`] recovers: a device name or a trailing-dot directory
    /// is still refused, an ordinary file there is still processed, on the
    /// single-file path and in a directory that just arrived.
    #[cfg(windows)]
    #[test]
    fn the_watcher_refuses_names_win32_cannot_spell_under_another_spelling_of_the_kb() {
        let scratch = crate::test_support::unique_temp_path("kb-watch-unspellable-alt");
        std::fs::create_dir_all(&scratch).unwrap();
        let kb = scratch.canonicalize().unwrap();
        struct Cleanup(std::path::PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let _cleanup = Cleanup(kb.clone());

        let leaf = kb.file_name().unwrap().to_str().unwrap().to_uppercase();
        let alt = kb.parent().unwrap().join(&leaf);
        assert_ne!(alt, kb, "the other spelling must differ as a path");

        // Backslashes: under `\\?\` a `/` is not a separator.
        let cases = [r"sub\ok.md", r"sub\CON.md", r"sub\dir.\x.md"];
        for native in cases {
            let full = kb.join(native);
            std::fs::create_dir_all(full.parent().unwrap()).unwrap();
            std::fs::write(&full, "# x\n").unwrap();
        }

        let reg = Registry::defaults();
        let rules = crate::exclusion::ExclusionRules::load(&kb, default_exclude_dirs());

        for native in cases {
            let rel = native.replace('\\', "/");
            let raw = alt.join(native);
            assert_eq!(
                indexer::index_rel_path(&kb, &raw),
                None,
                "the event path must not be under kb_path as spelled, or this proves nothing"
            );
            let recovered = to_rel(&kb, &raw);
            assert_eq!(recovered.as_deref(), Some(rel.as_str()));
            assert_eq!(
                should_process_parts(&rel, &raw, &reg, &rules),
                rel == "sub/ok.md",
                "{rel}"
            );
        }

        // The walk under the other spelling keeps all three, so what drops
        // two of them below is the watcher's own check.
        let walked =
            crate::indexer::collect_source_files_under(&kb, &alt.join("sub"), &reg, &rules)
                .unwrap();
        assert_eq!(walked.len(), 3, "{walked:?}");
        assert_eq!(
            new_directory_rels(&kb, &alt.join("sub"), &reg, &rules).unwrap(),
            vec!["sub/ok.md".to_string()]
        );
    }

    /// A verbatim scratch knowledge base, so `CON.md` is a file name there
    /// and not the console, removed when the guard drops.
    #[cfg(windows)]
    fn verbatim_scratch_kb(tag: &str) -> (std::path::PathBuf, impl Drop) {
        let scratch = crate::test_support::unique_temp_path(tag);
        std::fs::create_dir_all(&scratch).unwrap();
        let kb = scratch.canonicalize().unwrap();
        struct Cleanup(std::path::PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        (kb.clone(), Cleanup(kb))
    }

    /// (ADR-0023) Good -> bad: the new end is a write and is refused, so the
    /// old row goes.
    #[cfg(windows)]
    #[test]
    fn a_rename_onto_a_name_win32_cannot_spell_deindexes_the_old_row() {
        let (kb, _cleanup) = verbatim_scratch_kb("kb-watch-unspellable-rn1");
        let reg = Registry::defaults();
        let rules = crate::exclusion::ExclusionRules::load(&kb, default_exclude_dirs());
        std::fs::write(kb.join("CON.md"), "# x\n").unwrap();
        assert_eq!(
            rename_decision(
                "ok.md",
                &kb.join("ok.md"),
                "CON.md",
                &kb.join("CON.md"),
                &reg,
                &rules
            ),
            RenameAction::Deindex
        );
    }

    /// (ADR-0023) Bad (gone) -> good, the remedy the refusal suggests: the row
    /// an older version stored under the bad name moves, rather than staying
    /// behind as an unopenable hit next to a reindexed copy.
    #[cfg(windows)]
    #[test]
    fn renaming_a_name_win32_cannot_spell_away_moves_its_row() {
        let (kb, _cleanup) = verbatim_scratch_kb("kb-watch-unspellable-rn2");
        let reg = Registry::defaults();
        let rules = crate::exclusion::ExclusionRules::load(&kb, default_exclude_dirs());
        std::fs::write(kb.join("console.md"), "# x\n").unwrap();
        std::fs::create_dir_all(kb.join("dir")).unwrap();
        std::fs::write(kb.join(r"dir\x.md"), "# x\n").unwrap();
        for (old_rel, old_native, new_rel, new_native) in [
            ("CON.md", "CON.md", "console.md", "console.md"),
            ("dir./x.md", r"dir.\x.md", "dir/x.md", r"dir\x.md"),
        ] {
            assert_eq!(
                rename_decision(
                    old_rel,
                    &kb.join(old_native),
                    new_rel,
                    &kb.join(new_native),
                    &reg,
                    &rules
                ),
                RenameAction::Rename,
                "{old_rel} -> {new_rel}"
            );
        }
    }

    /// (ADR-0023) Deleting a file under a name the index no longer holds still
    /// deindexes the row an older version stored for it.
    #[cfg(windows)]
    #[test]
    fn deleting_a_name_win32_cannot_spell_deindexes_its_row() {
        let (kb, _cleanup) = verbatim_scratch_kb("kb-watch-unspellable-rm");
        let reg = Registry::defaults();
        let rules = crate::exclusion::ExclusionRules::load(&kb, default_exclude_dirs());
        for (rel, native) in [("CON.md", "CON.md"), ("dir./x.md", r"dir.\x.md")] {
            let full = kb.join(native);
            assert!(
                std::fs::symlink_metadata(&full).is_err(),
                "{rel} must be gone"
            );
            assert!(deindex_passes(rel, &full, &reg, &rules), "{rel}");
        }
    }

    /// (ADR-0023) The removal exemption does not reach a write: a bad-named
    /// file that exists is refused for a reindex.
    #[cfg(windows)]
    #[test]
    fn a_name_win32_cannot_spell_that_exists_is_still_not_written() {
        let (kb, _cleanup) = verbatim_scratch_kb("kb-watch-unspellable-wr");
        let reg = Registry::defaults();
        let rules = crate::exclusion::ExclusionRules::load(&kb, default_exclude_dirs());
        std::fs::write(kb.join("CON.md"), "# x\n").unwrap();
        assert!(!should_process_parts(
            "CON.md",
            &kb.join("CON.md"),
            &reg,
            &rules
        ));
    }

    /// (ADR-0023, codex P2 on PR #322) The old end of a rename is a removal
    /// even while a file exists under the old name again -- recreated before
    /// the debounced rename is handled. The row still moves to a good new
    /// name, or is dropped when the new name is bad too; otherwise the row an
    /// older version stored would stay, and the recreated file's own event is
    /// refused, so nothing else would remove it before a full index.
    #[cfg(windows)]
    #[test]
    fn the_old_end_of_a_rename_is_a_removal_even_when_the_old_name_exists_again() {
        let (kb, _cleanup) = verbatim_scratch_kb("kb-watch-unspellable-rc");
        let reg = Registry::defaults();
        let rules = crate::exclusion::ExclusionRules::load(&kb, default_exclude_dirs());
        std::fs::write(kb.join("CON.md"), "# recreated\n").unwrap();
        std::fs::write(kb.join("console.md"), "# x\n").unwrap();
        std::fs::write(kb.join("nul.md"), "# x\n").unwrap();
        assert_eq!(
            rename_decision(
                "CON.md",
                &kb.join("CON.md"),
                "console.md",
                &kb.join("console.md"),
                &reg,
                &rules
            ),
            RenameAction::Rename
        );
        assert_eq!(
            rename_decision(
                "CON.md",
                &kb.join("CON.md"),
                "nul.md",
                &kb.join("nul.md"),
                &reg,
                &rules
            ),
            RenameAction::Deindex
        );
    }

    /// (ADR-0023, local Codex round 2 on PR #322) A key the index cannot hold
    /// passes the removal gate before any check that reads what is on disk
    /// now: recreated as a hard link or a symlink, or sitting under an
    /// excluded directory, its row still moves with a rename or goes with a
    /// delete. No such row can be one this version wrote.
    #[cfg(windows)]
    #[test]
    fn a_key_win32_cannot_spell_passes_the_removal_gate_whatever_is_on_disk() {
        let (kb, _cleanup) = verbatim_scratch_kb("kb-watch-unspellable-rg");
        let reg = Registry::defaults();
        let rules = crate::exclusion::ExclusionRules::load(&kb, default_exclude_dirs());
        std::fs::write(kb.join("good.md"), "# x\n").unwrap();
        let other = kb.join("other.md");
        std::fs::write(&other, "# other\n").unwrap();
        let rename_to_good = |old_rel: &str, old: &Path| {
            rename_decision(old_rel, old, "good.md", &kb.join("good.md"), &reg, &rules)
        };

        // Hard link: the check that would refuse it reads the link count.
        std::fs::hard_link(&other, kb.join("CON.md")).unwrap();
        assert!(crate::links::is_multiply_linked(&kb.join("CON.md")));
        assert_eq!(
            rename_to_good("CON.md", &kb.join("CON.md")),
            RenameAction::Rename
        );
        assert!(deindex_passes("CON.md", &kb.join("CON.md"), &reg, &rules));

        // Under an excluded directory (`.obsidian` in `default_exclude_dirs`).
        std::fs::create_dir_all(kb.join(".obsidian")).unwrap();
        std::fs::write(kb.join(r".obsidian\nul.md"), "# x\n").unwrap();
        assert_eq!(
            rename_to_good(".obsidian/nul.md", &kb.join(r".obsidian\nul.md")),
            RenameAction::Rename
        );

        // Symlink: needs Developer Mode or elevation on Windows.
        if std::os::windows::fs::symlink_file(&other, kb.join("aux.md")).is_ok() {
            assert_eq!(
                rename_to_good("aux.md", &kb.join("aux.md")),
                RenameAction::Rename
            );
        } else {
            eprintln!(
                "a_key_win32_cannot_spell_passes_the_removal_gate_whatever_is_on_disk: \
                 could not create a symlink, so the symlink case is skipped."
            );
        }
    }

    /// (ADR-0023) The early pass is for keys the index cannot hold only: an
    /// ordinary old key goes through every other removal check.
    ///
    /// **契約変更 (kuriya #566 U1、2026-10-28 user 承認)**: 以前ここは「hard link に
    /// 作り直された普通の旧 key は removal gate を通らない」を固定していた。
    /// removal gate が symlink と同じく hard link も通すことにした (index は link に
    /// 文書を持たない。full index の walk も hard link を集めず削除 pass が行を消す)
    /// ので、[`deindex_passes`] は true、rename は旧行を新端へ移す
    /// [`RenameAction::Rename`] に反転した。
    #[cfg(windows)]
    #[test]
    fn an_addressable_key_keeps_every_removal_gate() {
        let (kb, _cleanup) = verbatim_scratch_kb("kb-watch-unspellable-rk");
        let reg = Registry::defaults();
        let rules = crate::exclusion::ExclusionRules::load(&kb, default_exclude_dirs());
        std::fs::write(kb.join("good.md"), "# x\n").unwrap();
        let other = kb.join("other.md");
        std::fs::write(&other, "# other\n").unwrap();
        std::fs::hard_link(&other, kb.join("old.md")).unwrap();
        assert!(deindex_passes("old.md", &kb.join("old.md"), &reg, &rules));
        assert_eq!(
            rename_decision(
                "old.md",
                &kb.join("old.md"),
                "good.md",
                &kb.join("good.md"),
                &reg,
                &rules
            ),
            RenameAction::Rename
        );
    }

    /// (#543) A remove event is a reindex when an indexable regular file is at the path again
    /// by the time it is handled, and a deindex when nothing is there, as before.
    #[test]
    fn a_removal_whose_file_is_back_is_a_reindex_and_one_whose_file_is_gone_a_deindex() {
        let kb = crate::test_support::unique_temp_path("groove-watcher-i543");
        std::fs::create_dir_all(&kb).unwrap();
        struct Cleanup(std::path::PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let _cleanup = Cleanup(kb.clone());
        let reg = Registry::defaults();
        let rules = crate::exclusion::ExclusionRules::load(&kb, default_exclude_dirs());

        std::fs::write(kb.join("back.md"), "# Back\n").unwrap();
        assert_eq!(
            removal_decision("back.md", &kb.join("back.md"), &reg, &rules),
            RemovalAction::Reindex
        );

        let gone = kb.join("gone.md");
        assert!(std::fs::symlink_metadata(&gone).is_err());
        assert_eq!(
            removal_decision("gone.md", &gone, &reg, &rules),
            RemovalAction::Deindex
        );

        // Excluded (`.obsidian` in `default_exclude_dirs`): neither, as before.
        std::fs::create_dir_all(kb.join(".obsidian")).unwrap();
        std::fs::write(kb.join(".obsidian").join("x.md"), "# x\n").unwrap();
        assert_eq!(
            removal_decision(
                ".obsidian/x.md",
                &kb.join(".obsidian").join("x.md"),
                &reg,
                &rules
            ),
            RemovalAction::Skip
        );

        // A wrong extension is not reindexed either.
        std::fs::write(kb.join("back.tmp"), "x").unwrap();
        assert_ne!(
            removal_decision("back.tmp", &kb.join("back.tmp"), &reg, &rules),
            RemovalAction::Reindex
        );
    }

    /// (#543) Only a regular file the reindex gate takes is reindexed from a remove event: a
    /// directory or a hard link put at the path is not.
    ///
    /// **契約変更 (kuriya #566 U1、2026-10-28 user 承認)**: hard link の期待値は以前
    /// `Skip` (#543 以前の removal gate と同じ答え) だった。removal gate が hard link を
    /// 通すことにしたので `Deindex` に反転した (link に置き換わった path の行は消す)。
    #[test]
    fn a_removal_is_not_reindexed_for_a_directory_or_a_hard_link_at_the_path() {
        let kb = crate::test_support::unique_temp_path("groove-watcher-i543-nf");
        std::fs::create_dir_all(&kb).unwrap();
        struct Cleanup(std::path::PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let _cleanup = Cleanup(kb.clone());
        let reg = Registry::defaults();
        let rules = crate::exclusion::ExclusionRules::load(&kb, default_exclude_dirs());

        // A directory with a registered extension in its name.
        std::fs::create_dir_all(kb.join("dir.md")).unwrap();
        assert_ne!(
            removal_decision("dir.md", &kb.join("dir.md"), &reg, &rules),
            RemovalAction::Reindex
        );

        // A hard link (no privilege needed on Windows either).
        let other = kb.join("other.md");
        std::fs::write(&other, "# other\n").unwrap();
        std::fs::hard_link(&other, kb.join("linked.md")).unwrap();
        assert_eq!(
            removal_decision("linked.md", &kb.join("linked.md"), &reg, &rules),
            RemovalAction::Deindex,
            "a hard link at the path drops the row (contract change, kuriya #566 U1, user-approved 2026-10-28)"
        );
    }

    /// (#543, local Codex r1, high) A remove event whose name is held on disk only under
    /// another case (`Old.md` renamed to `old.md`, the `Name(From)` arriving unpaired) is not
    /// a reindex: reindexing would rewrite the row under the old spelling, and the new
    /// spelling's own event adds a second row (`documents.path` compares bytes). On a
    /// case-insensitive volume (Windows, macOS) the old name still opens the file; on a
    /// case-sensitive one it is simply gone. Either way, not a reindex.
    #[test]
    fn a_removal_whose_name_is_on_disk_only_in_another_case_is_not_a_reindex() {
        let kb = crate::test_support::unique_temp_path("groove-watcher-i543-case");
        std::fs::create_dir_all(kb.join("sub")).unwrap();
        struct Cleanup(std::path::PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let _cleanup = Cleanup(kb.clone());
        let reg = Registry::defaults();
        let rules = crate::exclusion::ExclusionRules::load(&kb, default_exclude_dirs());

        std::fs::write(kb.join("A.md"), "# A\n").unwrap();
        assert_eq!(
            removal_decision("a.md", &kb.join("a.md"), &reg, &rules),
            RemovalAction::Deindex
        );
        std::fs::write(kb.join("sub").join("Old.md"), "# Old\n").unwrap();
        assert_eq!(
            removal_decision("sub/old.md", &kb.join("sub").join("old.md"), &reg, &rules),
            RemovalAction::Deindex
        );
        // The exact spelling is still a reindex.
        assert_eq!(
            removal_decision("sub/Old.md", &kb.join("sub").join("Old.md"), &reg, &rules),
            RemovalAction::Reindex
        );
    }

    /// (#543, local Codex r1, medium) The look [`dispatch_reindex_after_removal`] takes inside
    /// the locks: a file gone after the remove event was judged a reindex, a directory put at
    /// the path, or the file there only in another case drops the row (and nothing else); the
    /// file still there under its exact spelling touches nothing, so the reindex goes ahead.
    #[test]
    fn a_removed_path_that_is_gone_by_the_time_the_locks_are_held_drops_its_row() {
        let kb = crate::test_support::unique_temp_path("groove-watcher-i543-gone");
        std::fs::create_dir_all(kb.join("sub")).unwrap();
        struct Cleanup(std::path::PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let _cleanup = Cleanup(kb.clone());
        let db = dir_rename_db(&[
            "gone.md",
            "sub/now-a-dir.md",
            "sub/case.md",
            "sub/here.md",
            "other.md",
        ]);
        std::fs::create_dir_all(kb.join("sub").join("now-a-dir.md")).unwrap();
        std::fs::write(kb.join("sub").join("Case.md"), "# Case\n").unwrap();
        std::fs::write(kb.join("sub").join("here.md"), "# here\n").unwrap();

        assert!(
            deindex_if_removed_path_is_gone(&db, &kb, "sub/here.md").is_none(),
            "a file still at its exact spelling is left to the reindex"
        );
        for rel in ["gone.md", "sub/now-a-dir.md", "sub/case.md"] {
            assert!(
                deindex_if_removed_path_is_gone(&db, &kb, rel)
                    .expect("gone")
                    .unwrap(),
                "{rel} had a row and it went"
            );
        }
        assert_eq!(
            db.all_document_paths().unwrap(),
            vec!["other.md".to_string(), "sub/here.md".to_string()]
        );
        // Nothing left to drop: still handled as gone, reporting no row.
        assert!(
            !deindex_if_removed_path_is_gone(&db, &kb, "gone.md")
                .unwrap()
                .unwrap()
        );
    }

    /// (#543, local Codex r1, high) The spelling check itself: the exact name passes, another
    /// case of it does not (where the volume finds that other case at all), and a name with no
    /// cased letter needs no look.
    #[test]
    fn leaf_is_spelled_on_disk_takes_only_the_exact_name() {
        let kb = crate::test_support::unique_temp_path("groove-watcher-i543-spell");
        std::fs::create_dir_all(&kb).unwrap();
        struct Cleanup(std::path::PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let _cleanup = Cleanup(kb.clone());
        std::fs::write(kb.join("Note.md"), "# n\n").unwrap();
        std::fs::write(kb.join("123.md"), "# n\n").unwrap();
        assert!(leaf_is_spelled_on_disk(&kb, "Note.md"));
        assert!(leaf_is_spelled_on_disk(&kb, "123.md"));
        // On a case-sensitive file system `note.md` is not found either way; on a
        // case-insensitive one it is found, and the listing says it is not the name on disk.
        if std::fs::symlink_metadata(kb.join("note.md")).is_ok() {
            assert!(!leaf_is_spelled_on_disk(&kb, "note.md"));
            assert!(!leaf_is_spelled_on_disk(&kb, "NOTE.MD"));
        }
    }

    /// (#543, local Codex r6, medium) The spelling check covers the directories on the way,
    /// not the leaf alone. After an unpaired case-only directory rename (`Dir/note.md` to
    /// `dir/note.md`) a case-insensitive volume still opens `Dir/note.md`, and the leaf there
    /// is spelled exactly `note.md`; reindexing it would keep the row under `Dir/` beside the
    /// one the new spelling's event writes. Neither the first look nor the one inside the
    /// locks may keep it. On a case-sensitive volume `Dir` is simply not there.
    #[test]
    fn a_removal_whose_directory_is_on_disk_only_in_another_case_drops_its_row() {
        let kb = crate::test_support::unique_temp_path("groove-watcher-i543-dircase");
        std::fs::create_dir_all(kb.join("dir")).unwrap();
        struct Cleanup(std::path::PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let _cleanup = Cleanup(kb.clone());
        std::fs::write(kb.join("dir").join("note.md"), "# note\n").unwrap();
        let reg = Registry::defaults();
        let rules = crate::exclusion::ExclusionRules::load(&kb, default_exclude_dirs());

        assert_ne!(
            removal_decision("Dir/note.md", &kb.join("Dir").join("note.md"), &reg, &rules),
            RemovalAction::Reindex
        );
        // The exact spelling is still a reindex.
        assert_eq!(
            removal_decision("dir/note.md", &kb.join("dir").join("note.md"), &reg, &rules),
            RemovalAction::Reindex
        );

        let db = dir_rename_db(&["Dir/note.md", "other.md"]);
        assert!(
            deindex_if_removed_path_is_gone(&db, &kb, "Dir/note.md")
                .expect("gone under that spelling")
                .unwrap(),
            "the row under Dir/ was there and it went"
        );
        assert_eq!(
            db.all_document_paths().unwrap(),
            vec!["other.md".to_string()]
        );
    }

    /// (#543, local Codex r2, medium) Only "not found" and "not a directory" say a path is
    /// gone. Permission denied, an I/O error or anything else says nothing either way.
    #[test]
    fn only_not_found_and_not_a_directory_say_a_removed_path_is_gone() {
        use std::io::ErrorKind;
        assert_eq!(probe_error_on_disk(ErrorKind::NotFound), OnDisk::Gone);
        assert_eq!(probe_error_on_disk(ErrorKind::NotADirectory), OnDisk::Gone);
        for kind in [
            ErrorKind::PermissionDenied,
            ErrorKind::Other,
            ErrorKind::Interrupted,
            ErrorKind::TimedOut,
            ErrorKind::ResourceBusy,
        ] {
            assert_eq!(probe_error_on_disk(kind), OnDisk::Unknown(kind), "{kind:?}");
        }
    }

    /// (#543, local Codex r3, medium) Inside the locks, a look that could not tell drops the
    /// row as a path found gone does, and leaves every other row where it is.
    #[test]
    fn a_removed_path_that_could_not_be_looked_at_is_deindexed() {
        let kb = crate::test_support::unique_temp_path("groove-watcher-i543-unknown");
        std::fs::create_dir_all(&kb).unwrap();
        struct Cleanup(std::path::PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let _cleanup = Cleanup(kb.clone());
        let db = dir_rename_db(&["locked.md", "other.md"]);

        let kept = act_on_removed_path(
            &db,
            &kb,
            "locked.md",
            OnDisk::Unknown(std::io::ErrorKind::PermissionDenied),
        );
        assert!(
            matches!(
                kept,
                RemovedPathCheck::Unknown(std::io::ErrorKind::PermissionDenied, Ok(true))
            ),
            "{kept:?}"
        );
        assert_eq!(
            db.all_document_paths().unwrap(),
            vec!["other.md".to_string()]
        );

        let gone = act_on_removed_path(&db, &kb, "locked.md", OnDisk::Gone);
        assert!(
            matches!(gone, RemovedPathCheck::Deindexed(Ok(false))),
            "{gone:?}"
        );
        assert_eq!(
            db.all_document_paths().unwrap(),
            vec!["other.md".to_string()]
        );
    }

    /// (#543, local Codex r3, medium) Outside the locks, a look that could not tell is never a
    /// reindex: it takes the removal gate's answer, as a path found gone does.
    #[test]
    fn a_removal_that_could_not_be_looked_at_takes_the_removal_gate() {
        let kb = crate::test_support::unique_temp_path("groove-watcher-i543-unknown-out");
        std::fs::create_dir_all(&kb).unwrap();
        struct Cleanup(std::path::PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let _cleanup = Cleanup(kb.clone());
        let reg = Registry::defaults();
        let rules = crate::exclusion::ExclusionRules::load(&kb, default_exclude_dirs());
        let unknown = OnDisk::Unknown(std::io::ErrorKind::PermissionDenied);

        assert_eq!(
            removal_decision_for("note.md", &kb.join("note.md"), &reg, &rules, unknown),
            RemovalAction::Deindex
        );
        assert_eq!(
            removal_decision_for("note.md", &kb.join("note.md"), &reg, &rules, OnDisk::Gone),
            RemovalAction::Deindex
        );
        assert_eq!(
            removal_decision_for(
                ".obsidian/x.md",
                &kb.join(".obsidian").join("x.md"),
                &reg,
                &rules,
                unknown
            ),
            RemovalAction::Skip
        );
    }

    /// (#543, local Codex r7, medium) Outside the locks, a look that could not tell and ends
    /// in a deindex keeps the kind of the failure, so the line it prints can say why; a path
    /// found gone, found present, or skipped carries none.
    #[test]
    fn a_removal_that_could_not_be_looked_at_keeps_the_kind_for_its_line() {
        let kb = crate::test_support::unique_temp_path("groove-watcher-i543-unknown-kind");
        std::fs::create_dir_all(&kb).unwrap();
        struct Cleanup(std::path::PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let _cleanup = Cleanup(kb.clone());
        let reg = Registry::defaults();
        let rules = crate::exclusion::ExclusionRules::load(&kb, default_exclude_dirs());
        let denied = std::io::ErrorKind::PermissionDenied;
        let note = kb.join("note.md");

        assert_eq!(
            removal_outcome_for("note.md", &note, &reg, &rules, OnDisk::Unknown(denied)),
            (RemovalAction::Deindex, Some(denied))
        );
        assert_eq!(
            removal_outcome_for(
                "note.md",
                &note,
                &reg,
                &rules,
                OnDisk::Unknown(std::io::ErrorKind::Other)
            ),
            (RemovalAction::Deindex, Some(std::io::ErrorKind::Other))
        );
        assert_eq!(
            removal_outcome_for("note.md", &note, &reg, &rules, OnDisk::Gone),
            (RemovalAction::Deindex, None)
        );
        assert_eq!(
            removal_outcome_for(
                ".obsidian/x.md",
                &kb.join(".obsidian").join("x.md"),
                &reg,
                &rules,
                OnDisk::Unknown(denied)
            ),
            (RemovalAction::Skip, None)
        );
        std::fs::write(&note, "# note\n").unwrap();
        assert_eq!(
            removal_outcome_for("note.md", &note, &reg, &rules, OnDisk::Present),
            (RemovalAction::Reindex, None)
        );
    }

    /// (#543, GitHub codex r1 P2) After the reindex a remove event ran on a file still there,
    /// the rows stay only when the reindex says they hold the file now. A skip (whatever its
    /// reason), a refusal or an error drops the row as a removal does, and leaves every other
    /// row where it is.
    #[test]
    fn a_removal_reindex_that_did_not_index_the_replacement_drops_the_row() {
        let kb = crate::test_support::unique_temp_path("groove-watcher-i543-settle");
        std::fs::create_dir_all(&kb).unwrap();
        struct Cleanup(std::path::PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let _cleanup = Cleanup(kb.clone());
        // The file is there, as it is when this runs: the disk is not what decides.
        std::fs::write(kb.join("doc.md"), "").unwrap();

        let kept: [Result<indexer::SingleResult>; 3] = [
            Ok(indexer::SingleResult::Updated {
                chunks: 1,
                frontmatter_unparsed: false,
            }),
            Ok(indexer::SingleResult::Unchanged),
            Ok(indexer::SingleResult::MetadataRefreshed {
                frontmatter_unparsed: false,
            }),
        ];
        for result in &kept {
            let db = dir_rename_db(&["doc.md", "other.md"]);
            let settled = settle_removal_reindex(&db, &kb, "doc.md", result);
            assert!(
                matches!(settled, RemovalReindexSettled::Kept),
                "{result:?}: {settled:?}"
            );
            assert_eq!(
                db.all_document_paths().unwrap(),
                vec!["doc.md".to_string(), "other.md".to_string()],
                "{result:?}"
            );
        }

        let dropped: [(Result<indexer::SingleResult>, &str); 4] = [
            (
                Ok(indexer::SingleResult::Skipped {
                    reason: "no embeddable chunks",
                    frontmatter_unparsed: false,
                }),
                "no embeddable chunks",
            ),
            (
                Ok(indexer::SingleResult::Skipped {
                    reason: "a reason added later",
                    frontmatter_unparsed: true,
                }),
                "a reason added later",
            ),
            (Ok(indexer::SingleResult::Refused), "refused"),
            (Err(anyhow::anyhow!("endpoint down")), "reindex failed"),
        ];
        for (result, reason_part) in &dropped {
            let db = dir_rename_db(&["doc.md", "other.md"]);
            let settled = settle_removal_reindex(&db, &kb, "doc.md", result);
            match &settled {
                RemovalReindexSettled::Deindexed {
                    reason,
                    outcome: Ok(true),
                } => assert!(reason.contains(reason_part), "{reason}"),
                other => panic!("{result:?}: {other:?}"),
            }
            assert_eq!(
                db.all_document_paths().unwrap(),
                vec!["other.md".to_string()],
                "{result:?}"
            );
        }

        // The indexer already dropped the row: nothing else goes, and no deindex is reported.
        let db = dir_rename_db(&["other.md"]);
        let settled = settle_removal_reindex(
            &db,
            &kb,
            "doc.md",
            &Ok(indexer::SingleResult::OutsideKnowledgeBaseDeindexed),
        );
        assert!(
            matches!(settled, RemovalReindexSettled::IndexerDeindexed(Ok(false))),
            "{settled:?}"
        );
        assert_eq!(
            db.all_document_paths().unwrap(),
            vec!["other.md".to_string()]
        );
    }

    /// (#543) A replacement with nothing to embed, whose row the indexer already dropped
    /// ([`indexer::SingleResult::NothingToIndex`]), is settled as the skip it is: the line
    /// still says the removal deindexed the document when a row was there, and stays a plain
    /// skip when none was. Every other row stays.
    #[test]
    fn a_removal_reindex_that_found_nothing_to_index_reports_the_row_the_indexer_dropped() {
        let kb = crate::test_support::unique_temp_path("groove-watcher-i543-nothing");
        std::fs::create_dir_all(&kb).unwrap();
        struct Cleanup(std::path::PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let _cleanup = Cleanup(kb.clone());
        std::fs::write(kb.join("doc.md"), "").unwrap();

        for had_row in [true, false] {
            // The indexer took the row already, so only `other.md` is left either way.
            let db = dir_rename_db(&["other.md"]);
            let settled = settle_removal_reindex(
                &db,
                &kb,
                "doc.md",
                &Ok(indexer::SingleResult::NothingToIndex {
                    deindexed: had_row,
                    frontmatter_unparsed: false,
                }),
            );
            match &settled {
                RemovalReindexSettled::Deindexed {
                    reason,
                    outcome: Ok(reported),
                } => {
                    assert_eq!(reason, indexer::SKIPPED_NO_CHUNKS);
                    assert_eq!(*reported, had_row, "had_row = {had_row}");
                }
                other => panic!("had_row = {had_row}: {other:?}"),
            }
            assert_eq!(
                db.all_document_paths().unwrap(),
                vec!["other.md".to_string()]
            );
        }
    }

    /// A temp KB holding `doc.md`, removed on drop. The disk is not what the settle tests
    /// decide on (the relook closure is), but the file is there as it is when they run.
    struct I549Kb(std::path::PathBuf);
    impl Drop for I549Kb {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    fn i549_kb(prefix: &str) -> I549Kb {
        let kb = crate::test_support::unique_temp_path(prefix);
        std::fs::create_dir_all(&kb).unwrap();
        std::fs::write(kb.join("doc.md"), "# doc\n\nbody\n").unwrap();
        I549Kb(kb)
    }

    /// The results of a reindex that did not settle the row (S4-S12 of the plan's table S),
    /// each with a part of the reason the line names.
    fn i549_unsettled_results() -> Vec<(Result<indexer::SingleResult>, &'static str)> {
        let skipped = |reason: &'static str| {
            (
                Ok(indexer::SingleResult::Skipped {
                    reason,
                    frontmatter_unparsed: false,
                }),
                reason,
            )
        };
        vec![
            skipped("file no longer exists"),
            skipped("file too large"),
            skipped("read failed"),
            skipped("parse failed"),
            skipped("changed between scan and read"),
            skipped("embedding endpoint rejected the input"),
            skipped("a reason added later"),
            (Ok(indexer::SingleResult::Refused), "refused"),
            (
                Err(anyhow::anyhow!("embedding endpoint returned HTTP 503")),
                "reindex failed: embedding endpoint returned HTTP 503",
            ),
        ]
    }

    /// (kuriya 549) A removal reindex that did not settle the row, with the file still there
    /// at the second look, leaves the row (and every other row) as it was and says why, in the
    /// words the deindexing settle uses for the same result.
    #[test]
    fn i549_an_unsettled_removal_reindex_keeps_the_row_while_the_file_is_there() {
        let kb = i549_kb("groove-watcher-i549-present");
        for (result, reason_part) in &i549_unsettled_results() {
            let db = dir_rename_db(&["doc.md", "other.md"]);
            let mut looked = 0;
            let settled = settle_removal_reindex_looked(&db, &kb.0, "doc.md", result, || {
                looked += 1;
                OnDisk::Present
            });
            assert_eq!(looked, 1, "{result:?}: looked at again once");
            match &settled {
                RemovalSettled::LeftIndexed {
                    reason,
                    unknown: None,
                    held_row: Ok(true),
                    folded: Ok(false),
                } => assert!(reason.contains(reason_part), "{result:?}: {reason}"),
                other => panic!("{result:?}: {other:?}"),
            }
            assert_eq!(
                db.all_document_paths().unwrap(),
                vec!["doc.md".to_string(), "other.md".to_string()],
                "{result:?}"
            );
        }

        // No row under `rel`: still left (nothing to delete), and the outcome says so.
        let db = dir_rename_db(&["other.md"]);
        let settled = settle_removal_reindex_looked(
            &db,
            &kb.0,
            "doc.md",
            &Ok(indexer::SingleResult::Refused),
            || OnDisk::Present,
        );
        assert!(
            matches!(
                settled,
                RemovalSettled::LeftIndexed {
                    held_row: Ok(false),
                    ..
                }
            ),
            "{settled:?}"
        );
    }

    /// (kuriya 549) The same results, once the second look finds the path gone, go to
    /// [`settle_removal_reindex`] as before: the row under `rel` is dropped, the others stay, and
    /// the reason is the one the kept branch would have named.
    #[test]
    fn i549_an_unsettled_removal_reindex_drops_the_row_once_the_path_is_gone() {
        let kb = i549_kb("groove-watcher-i549-gone");
        for (result, reason_part) in &i549_unsettled_results() {
            let db = dir_rename_db(&["doc.md", "other.md"]);
            let settled =
                settle_removal_reindex_looked(&db, &kb.0, "doc.md", result, || OnDisk::Gone);
            match &settled {
                RemovalSettled::Settled(RemovalReindexSettled::Deindexed {
                    reason,
                    outcome: Ok(true),
                }) => {
                    assert!(reason.contains(reason_part), "{result:?}: {reason}");
                    assert_eq!(reason, &unsettled_reindex_reason(result), "{result:?}");
                }
                other => panic!("{result:?}: {other:?}"),
            }
            assert_eq!(
                db.all_document_paths().unwrap(),
                vec!["other.md".to_string()],
                "{result:?}"
            );
        }
    }

    /// (kuriya 549) A second look that cannot tell keeps the row as a write's does (U1 of the
    /// plan), and the outcome carries the kind for the line.
    #[test]
    fn i549_an_unsettled_removal_reindex_keeps_the_row_when_the_relook_cannot_tell() {
        let kb = i549_kb("groove-watcher-i549-unknown");
        for (result, reason_part) in &i549_unsettled_results() {
            let db = dir_rename_db(&["doc.md", "other.md"]);
            let settled = settle_removal_reindex_looked(&db, &kb.0, "doc.md", result, || {
                OnDisk::Unknown(std::io::ErrorKind::PermissionDenied)
            });
            match &settled {
                RemovalSettled::LeftIndexed {
                    reason,
                    unknown: Some(std::io::ErrorKind::PermissionDenied),
                    held_row: Ok(true),
                    folded: Ok(false),
                } => assert!(reason.contains(reason_part), "{result:?}: {reason}"),
                other => panic!("{result:?}: {other:?}"),
            }
            assert_eq!(
                db.all_document_paths().unwrap(),
                vec!["doc.md".to_string(), "other.md".to_string()],
                "{result:?}"
            );
        }
    }

    /// (kuriya 549, GitHub codex r3 P1) The same unsettled result gets the same reason
    /// whether the second look drops the row ([`settle_removal_reindex`]) or keeps it: both
    /// take it from [`unsettled_reindex_reason`], so the skip's words, `refused`, the error's
    /// format and the cut of a response body cannot drift apart between the two lines.
    #[test]
    fn i549_a_dropped_and_a_kept_row_name_an_unsettled_reindex_alike() {
        let kb = i549_kb("groove-watcher-i549-alike");
        let reason_of = |settled: &RemovalSettled| match settled {
            RemovalSettled::Settled(RemovalReindexSettled::Deindexed { reason, .. })
            | RemovalSettled::LeftIndexed { reason, .. } => reason.clone(),
            other => panic!("no reason: {other:?}"),
        };
        // An error whose chain quotes a response body: the cut must be the same on both sides.
        let body = serde_json::from_str::<serde_json::Value>("{\"body\": secret text")
            .expect_err("not JSON");
        let mut results = i549_unsettled_results();
        results.push((
            Err(anyhow::Error::new(body).context("failed to embed chunks for doc.md")),
            "reindex failed: failed to embed chunks for doc.md: malformed JSON (syntax)",
        ));
        for (result, expected) in &results {
            let dropped_db = dir_rename_db(&["doc.md", "other.md"]);
            let dropped =
                settle_removal_reindex_looked(&dropped_db, &kb.0, "doc.md", result, || {
                    OnDisk::Gone
                });
            let kept_db = dir_rename_db(&["doc.md", "other.md"]);
            let kept = settle_removal_reindex_looked(&kept_db, &kb.0, "doc.md", result, || {
                OnDisk::Present
            });
            assert!(
                matches!(
                    dropped,
                    RemovalSettled::Settled(RemovalReindexSettled::Deindexed { .. })
                ),
                "{result:?}: {dropped:?}"
            );
            assert!(
                matches!(kept, RemovalSettled::LeftIndexed { .. }),
                "{result:?}: {kept:?}"
            );
            let (dropped, kept) = (reason_of(&dropped), reason_of(&kept));
            assert_eq!(dropped, kept, "{result:?}");
            assert_eq!(&dropped, expected, "{result:?}");
            assert!(!dropped.contains("secret"), "{dropped}");
        }
    }

    /// (kuriya 549) A result that settled the row is not looked at again: it gets what
    /// [`settle_removal_reindex`] gives it, the empty-file skip (`NothingToIndex`) still dropped.
    #[test]
    fn i549_a_settled_removal_reindex_is_not_looked_at_again() {
        let kb = i549_kb("groove-watcher-i549-settled");
        let no_look = || -> OnDisk { panic!("a settled result must not be looked at again") };

        let kept: [Result<indexer::SingleResult>; 3] = [
            Ok(indexer::SingleResult::Updated {
                chunks: 1,
                frontmatter_unparsed: false,
            }),
            Ok(indexer::SingleResult::Unchanged),
            Ok(indexer::SingleResult::MetadataRefreshed {
                frontmatter_unparsed: false,
            }),
        ];
        for result in &kept {
            let db = dir_rename_db(&["doc.md", "other.md"]);
            let settled = settle_removal_reindex_looked(&db, &kb.0, "doc.md", result, no_look);
            assert!(
                matches!(
                    settled,
                    RemovalSettled::Settled(RemovalReindexSettled::Kept)
                ),
                "{result:?}: {settled:?}"
            );
            assert_eq!(
                db.all_document_paths().unwrap(),
                vec!["doc.md".to_string(), "other.md".to_string()],
                "{result:?}"
            );
        }

        // The indexer dropped the row of an emptied file: settled, still a deindex.
        let db = dir_rename_db(&["other.md"]);
        let settled = settle_removal_reindex_looked(
            &db,
            &kb.0,
            "doc.md",
            &Ok(indexer::SingleResult::NothingToIndex {
                deindexed: true,
                frontmatter_unparsed: false,
            }),
            no_look,
        );
        match &settled {
            RemovalSettled::Settled(RemovalReindexSettled::Deindexed {
                reason,
                outcome: Ok(true),
            }) => assert_eq!(reason, indexer::SKIPPED_NO_CHUNKS),
            other => panic!("{other:?}"),
        }

        let db = dir_rename_db(&["other.md"]);
        let settled = settle_removal_reindex_looked(
            &db,
            &kb.0,
            "doc.md",
            &Ok(indexer::SingleResult::OutsideKnowledgeBaseDeindexed),
            no_look,
        );
        assert!(
            matches!(
                settled,
                RemovalSettled::Settled(RemovalReindexSettled::IndexerDeindexed(Ok(false)))
            ),
            "{settled:?}"
        );
        assert_eq!(
            db.all_document_paths().unwrap(),
            vec!["other.md".to_string()]
        );
    }

    /// (kuriya 549) A row left as it was keeps the folded row an older version stored while the
    /// real key holds none (the folded row is then all the index holds for the file), and drops
    /// it once the real key holds a row: the write form of the sweep, never the removal form.
    /// Unix only: a folded spelling exists only where `\` is not a separator.
    #[cfg(unix)]
    #[test]
    fn i549_a_row_left_indexed_keeps_a_folded_row_whose_real_key_holds_none() {
        let kb = i549_kb("groove-watcher-i549-folded");
        std::fs::write(kb.0.join("secret\\pay.md"), "# pay").unwrap();
        let has = |db: &Database, path: &str| db.get_document_hash(path).unwrap().is_some();

        // Only the folded row: it stays.
        let db = dir_rename_db(&["secret/pay.md"]);
        let settled = settle_removal_reindex_looked(
            &db,
            &kb.0,
            "secret\\pay.md",
            &Ok(indexer::SingleResult::Refused),
            || OnDisk::Present,
        );
        assert!(
            matches!(
                settled,
                RemovalSettled::LeftIndexed {
                    held_row: Ok(false),
                    folded: Ok(false),
                    ..
                }
            ),
            "{settled:?}"
        );
        assert!(has(&db, "secret/pay.md"), "the folded row is all there is");

        // The real key holds a row too: the duplicate goes, the real key stays.
        let db = dir_rename_db(&["secret/pay.md", "secret\\pay.md"]);
        let settled = settle_removal_reindex_looked(
            &db,
            &kb.0,
            "secret\\pay.md",
            &Ok(indexer::SingleResult::Refused),
            || OnDisk::Present,
        );
        assert!(
            matches!(
                settled,
                RemovalSettled::LeftIndexed {
                    held_row: Ok(true),
                    folded: Ok(true),
                    ..
                }
            ),
            "{settled:?}"
        );
        assert!(has(&db, "secret\\pay.md"));
        assert!(!has(&db, "secret/pay.md"));
    }

    /// (kuriya 549) The line for a row left as it was must not read as the line for a row
    /// dropped: the remove-then-write end-to-end tests wait for "was removed and its
    /// replacement was not indexed" as the drop. Both forms still start with "was removed and",
    /// which the end-to-end tests that pin this fix wait for.
    #[test]
    fn i549_the_left_indexed_line_is_not_the_deindexed_line() {
        for unknown in [None, Some(std::io::ErrorKind::PermissionDenied)] {
            let line = left_indexed_message("doc.md", "file too large", unknown);
            assert!(
                !line.contains("doc.md was removed and its replacement was not indexed"),
                "{line}"
            );
            assert!(line.contains("doc.md was removed and"), "{line}");
            assert!(line.contains("(file too large)"), "{line}");
            assert!(line.is_ascii(), "{line}");
        }
        assert!(
            left_indexed_message(
                "doc.md",
                "read failed",
                Some(std::io::ErrorKind::PermissionDenied)
            )
            .contains("could not tell whether doc.md is still there (permission denied)")
        );
    }

    /// (kuriya 548) Inside the locks, a look that could not tell leaves the row under `rel`,
    /// its folded legacy row and every other row as they were, and keeps the kind for the
    /// line; a missing row is reported as missing. The line says the row was left, not
    /// deindexed, and is the one the end-to-end tests that pin this fix wait for.
    #[test]
    fn i548_an_unknown_look_inside_the_locks_leaves_the_row() {
        let kb = i549_kb("groove-watcher-i548-unknown");
        let reg = Registry::defaults();
        let rules = crate::exclusion::ExclusionRules::load(&kb.0, default_exclude_dirs());
        let unknown = OnDisk::Unknown(std::io::ErrorKind::PermissionDenied);

        let db = dir_rename_db(&["doc.md", "other.md"]);
        let decided = decide_removal_in_locks(
            &db,
            &kb.0,
            "doc.md",
            &kb.0.join("doc.md"),
            &reg,
            &rules,
            unknown,
            RemovalCause::RemoveEvent,
        );
        assert!(
            matches!(
                decided,
                RemovalInLocks::LeftIndexed {
                    kind: std::io::ErrorKind::PermissionDenied,
                    held_row: Ok(true),
                }
            ),
            "{decided:?}"
        );
        assert_eq!(
            db.all_document_paths().unwrap(),
            vec!["doc.md".to_string(), "other.md".to_string()]
        );

        let db = dir_rename_db(&["other.md"]);
        let decided = decide_removal_in_locks(
            &db,
            &kb.0,
            "doc.md",
            &kb.0.join("doc.md"),
            &reg,
            &rules,
            unknown,
            RemovalCause::RemoveEvent,
        );
        assert!(
            matches!(
                decided,
                RemovalInLocks::LeftIndexed {
                    held_row: Ok(false),
                    ..
                }
            ),
            "{decided:?}"
        );

        // The folded legacy row stays too: no sweep runs on an unknown, in either form. Unix
        // only: a folded spelling exists only where `\` is not a separator.
        #[cfg(unix)]
        {
            std::fs::write(kb.0.join("secret\\pay.md"), "# pay").unwrap();
            let db = dir_rename_db(&["secret/pay.md", "secret\\pay.md"]);
            let decided = decide_removal_in_locks(
                &db,
                &kb.0,
                "secret\\pay.md",
                &kb.0.join("secret\\pay.md"),
                &reg,
                &rules,
                unknown,
                RemovalCause::RemoveEvent,
            );
            assert!(
                matches!(
                    decided,
                    RemovalInLocks::LeftIndexed {
                        held_row: Ok(true),
                        ..
                    }
                ),
                "{decided:?}"
            );
            assert_eq!(
                db.all_document_paths().unwrap(),
                vec!["secret/pay.md".to_string(), "secret\\pay.md".to_string()]
            );
        }

        let line = unknown_left_indexed_message("sub/a.md", std::io::ErrorKind::PermissionDenied);
        assert_eq!(
            line,
            "watcher: could not tell whether sub/a.md is still there (permission denied); left it indexed"
        );
        assert!(!line.contains("deindexed"), "{line}");
        assert!(line.is_ascii(), "{line}");
    }

    /// (kuriya 548) Inside the locks, the look taken there decides through the removal
    /// decision asked again: a path gone drops its row (and only its row), a file the write
    /// gate takes is reindexed, an excluded path is skipped, and a look that cannot tell keeps
    /// the row. The answer the look outside gave plays no part.
    #[test]
    fn i548_a_removal_looked_at_again_inside_the_locks_decides_on_that_look() {
        let kb = i549_kb("groove-watcher-i548-decide");
        let reg = Registry::defaults();
        let rules = crate::exclusion::ExclusionRules::load(&kb.0, default_exclude_dirs());
        let full = kb.0.join("doc.md");

        let db = dir_rename_db(&["doc.md", "other.md"]);
        let gone = decide_removal_in_locks(
            &db,
            &kb.0,
            "doc.md",
            &full,
            &reg,
            &rules,
            OnDisk::Gone,
            RemovalCause::RemoveEvent,
        );
        assert!(
            matches!(gone, RemovalInLocks::Deindexed(Ok(true))),
            "{gone:?}"
        );
        assert_eq!(
            db.all_document_paths().unwrap(),
            vec!["other.md".to_string()]
        );

        let db = dir_rename_db(&["doc.md", "other.md"]);
        let present = decide_removal_in_locks(
            &db,
            &kb.0,
            "doc.md",
            &full,
            &reg,
            &rules,
            OnDisk::Present,
            RemovalCause::RemoveEvent,
        );
        assert!(matches!(present, RemovalInLocks::Reindex), "{present:?}");
        assert_eq!(
            db.all_document_paths().unwrap(),
            vec!["doc.md".to_string(), "other.md".to_string()]
        );

        let excluded_full = kb.0.join(".obsidian").join("x.md");
        std::fs::create_dir_all(excluded_full.parent().unwrap()).unwrap();
        std::fs::write(&excluded_full, "# x").unwrap();
        for look in [
            OnDisk::Present,
            OnDisk::Gone,
            OnDisk::Unknown(std::io::ErrorKind::PermissionDenied),
        ] {
            let db = dir_rename_db(&[".obsidian/x.md"]);
            let skipped = decide_removal_in_locks(
                &db,
                &kb.0,
                ".obsidian/x.md",
                &excluded_full,
                &reg,
                &rules,
                look,
                RemovalCause::RemoveEvent,
            );
            assert!(
                matches!(skipped, RemovalInLocks::Skip),
                "{look:?}: {skipped:?}"
            );
            assert_eq!(
                db.all_document_paths().unwrap(),
                vec![".obsidian/x.md".to_string()],
                "{look:?}"
            );
        }

        let db = dir_rename_db(&["doc.md", "other.md"]);
        let unknown = decide_removal_in_locks(
            &db,
            &kb.0,
            "doc.md",
            &full,
            &reg,
            &rules,
            OnDisk::Unknown(std::io::ErrorKind::Other),
            RemovalCause::RemoveEvent,
        );
        assert!(
            matches!(
                unknown,
                RemovalInLocks::LeftIndexed {
                    kind: std::io::ErrorKind::Other,
                    held_row: Ok(true),
                }
            ),
            "{unknown:?}"
        );
        assert_eq!(
            db.all_document_paths().unwrap(),
            vec!["doc.md".to_string(), "other.md".to_string()]
        );
    }

    /// (kuriya 548) A file found inside the locks that the write gate refuses is never
    /// reindexed there, whatever the look outside found. A hard link passes the removal gate,
    /// so its row is dropped
    /// ([`a_removal_is_not_reindexed_for_a_directory_or_a_hard_link_at_the_path`] pins the same
    /// answer outside the locks). A key the index cannot hold passes the removal gate early
    /// (ADR-0023), so a file there has its row dropped rather than reindexed.
    ///
    /// **契約変更 (kuriya #566 U1、2026-10-28 user 承認)**: hard link は以前 removal gate も
    /// 通らず `Skip` で行が残った (kuriya 543 以前の removal と同じ答え)。removal gate が
    /// hard link を通すことにしたので `Deindexed(Ok(true))`、残る行は `other.md` だけに反転した。
    #[test]
    fn i548_a_file_the_write_gate_refuses_inside_the_locks_is_not_reindexed() {
        let kb = i549_kb("groove-watcher-i548-gate");
        std::fs::hard_link(kb.0.join("doc.md"), kb.0.join("twin.md")).unwrap();
        let reg = Registry::defaults();
        let rules = crate::exclusion::ExclusionRules::load(&kb.0, default_exclude_dirs());

        let db = dir_rename_db(&["doc.md", "other.md"]);
        let linked = decide_removal_in_locks(
            &db,
            &kb.0,
            "doc.md",
            &kb.0.join("doc.md"),
            &reg,
            &rules,
            OnDisk::Present,
            RemovalCause::RemoveEvent,
        );
        assert!(
            matches!(linked, RemovalInLocks::Deindexed(Ok(true))),
            "{linked:?}"
        );
        assert_eq!(
            db.all_document_paths().unwrap(),
            vec!["other.md".to_string()]
        );

        // An empty segment: a spelling no document name may take, on every platform.
        let db = dir_rename_db(&["a//doc.md", "other.md"]);
        let unaddressable = decide_removal_in_locks(
            &db,
            &kb.0,
            "a//doc.md",
            &kb.0.join("doc.md"),
            &reg,
            &rules,
            OnDisk::Present,
            RemovalCause::RemoveEvent,
        );
        assert!(
            matches!(unaddressable, RemovalInLocks::Deindexed(Ok(true))),
            "{unaddressable:?}"
        );
        assert_eq!(
            db.all_document_paths().unwrap(),
            vec!["other.md".to_string()]
        );
    }

    /// (kuriya 548) The event loop's reading of the outside look: an unknown that ends in a
    /// deindex goes inside the locks to be looked at again, never straight to the deindex; so
    /// does a path found gone (see
    /// [`i548_a_removal_found_gone_outside_the_locks_is_looked_at_again_inside_them`]), and the
    /// rest is as before.
    #[test]
    fn i548_the_event_loop_does_not_drop_a_row_on_an_unknown_look() {
        assert_eq!(
            removal_dispatch(
                "doc.md",
                (
                    RemovalAction::Deindex,
                    Some(std::io::ErrorKind::PermissionDenied)
                )
            ),
            RemovalDispatch::DecideInLocks
        );
        assert_eq!(
            removal_dispatch("doc.md", (RemovalAction::Reindex, None)),
            RemovalDispatch::DecideInLocks
        );
        assert_eq!(
            removal_dispatch("doc.md", (RemovalAction::Deindex, None)),
            RemovalDispatch::DecideInLocks
        );
        assert_eq!(
            removal_dispatch("doc.md", (RemovalAction::Skip, None)),
            RemovalDispatch::Skip
        );
        assert_eq!(
            removal_dispatch(
                "doc.md",
                (
                    RemovalAction::Skip,
                    Some(std::io::ErrorKind::PermissionDenied)
                )
            ),
            RemovalDispatch::Skip
        );
    }

    /// (kuriya 548, local Codex r1, high) A deindex the look outside the locks reached on a
    /// path it found gone is not settled there: it goes inside the locks to be looked at
    /// again, as the empty key does. Only a name the index cannot hold, which the removal gate
    /// passes early whatever is on disk (ADR-0023), is dropped without a second look.
    #[test]
    fn i548_a_removal_found_gone_outside_the_locks_is_looked_at_again_inside_them() {
        let kb = i549_kb("groove-watcher-i548-gone-outside");
        let reg = Registry::defaults();
        let rules = crate::exclusion::ExclusionRules::load(&kb.0, default_exclude_dirs());
        let full = kb.0.join("gone.md");

        let outside = removal_outcome("gone.md", &full, &reg, &rules);
        assert_eq!(outside, (RemovalAction::Deindex, None));
        assert_eq!(
            removal_dispatch("gone.md", outside),
            RemovalDispatch::DecideInLocks
        );
        for rel in ["doc.md", "sub/doc.md", ""] {
            assert_eq!(
                removal_dispatch(rel, (RemovalAction::Deindex, None)),
                RemovalDispatch::DecideInLocks,
                "{rel:?}"
            );
        }

        // An empty segment: a spelling no document name may take, on every platform.
        assert!(removal_gate_passes_by_name("a//doc.md"));
        assert_eq!(
            removal_dispatch("a//doc.md", (RemovalAction::Deindex, None)),
            RemovalDispatch::Deindex
        );
        assert!(!removal_gate_passes_by_name("doc.md"));
        assert!(!removal_gate_passes_by_name(""));
    }

    /// (kuriya 548, GitHub Codex r2) The removal gate's early pass and the name test
    /// [`removal_dispatch`] reads are one test: for every name, [`gate_parts`] with
    /// [`Gate::Removal`] passes on the name alone exactly when [`removal_gate_passes_by_name`]
    /// says so. The full path does not exist and has an extension no parser takes, so a name
    /// that misses the early pass is turned away by the extension filter below it, and only
    /// the early pass can answer yes.
    #[test]
    fn i548_the_removal_gate_and_the_dispatcher_share_one_name_test() {
        let kb = i549_kb("groove-watcher-i548-one-name-test");
        let reg = Registry::defaults();
        let rules = crate::exclusion::ExclusionRules::load(&kb.0, default_exclude_dirs());
        let full = kb.0.join("nowhere.unparsed-ext");
        assert!(!full.exists());

        let mut names = vec![
            "doc.md",
            "sub/doc.md",
            "",
            "a//doc.md",
            "./a.md",
            "a/./b.md",
            "a/",
            ".",
            "../a.md",
            "/abs.md",
            "a\\..\\b.md",
            "C:/note.md",
        ];
        if cfg!(windows) {
            names.extend(["b.md.", "dir./x.md", "a<b.md", "a/b?.md", "a.b.md"]);
        }
        let mut passed = 0;
        for rel in names {
            let by_name = removal_gate_passes_by_name(rel);
            assert_eq!(
                gate_parts(rel, &full, &reg, &rules, Gate::Removal),
                by_name,
                "{rel:?}"
            );
            passed += usize::from(by_name);
        }
        // Both answers are exercised: some names pass early and some do not.
        assert!(passed > 0);
        assert!(!removal_gate_passes_by_name("doc.md"));
    }

    /// (kuriya 548, local Codex r1, high) The case the second look is for: the look outside
    /// the locks found the path gone, and the file was written again before the locks were
    /// taken (its create folded away by the debouncer). The look inside finds it, and the
    /// row is kept and handed to the reindex rather than dropped until the next full index.
    #[test]
    fn i548_a_file_written_again_before_the_locks_keeps_its_row() {
        let kb = i549_kb("groove-watcher-i548-written-again");
        let reg = Registry::defaults();
        let rules = crate::exclusion::ExclusionRules::load(&kb.0, default_exclude_dirs());
        let full = kb.0.join("doc.md");
        std::fs::remove_file(&full).unwrap();

        let outside = removal_outcome("doc.md", &full, &reg, &rules);
        assert_eq!(outside, (RemovalAction::Deindex, None));
        assert_eq!(
            removal_dispatch("doc.md", outside),
            RemovalDispatch::DecideInLocks
        );

        std::fs::write(&full, "# doc\n\nwritten again\n").unwrap();
        let look = removed_path_on_disk(&kb.0, "doc.md");
        assert_eq!(look, OnDisk::Present);
        let db = dir_rename_db(&["doc.md", "other.md"]);
        let decided = decide_removal_in_locks(
            &db,
            &kb.0,
            "doc.md",
            &full,
            &reg,
            &rules,
            look,
            RemovalCause::RemoveEvent,
        );
        assert!(matches!(decided, RemovalInLocks::Reindex), "{decided:?}");
        assert_eq!(
            db.all_document_paths().unwrap(),
            vec!["doc.md".to_string(), "other.md".to_string()]
        );
    }

    /// (kuriya 548, local Codex r2, medium) Under the database lock alone, a path still gone
    /// has its row (and only its row) dropped, or is skipped when the removal gate turns it
    /// away; a file there or a look that cannot tell touches nothing and is handed on to the
    /// path that takes both locks.
    #[test]
    fn i548_a_removal_still_gone_is_settled_under_the_database_lock_alone() {
        let kb = i549_kb("groove-watcher-i548-db-lock-only");
        let reg = Registry::defaults();
        let rules = crate::exclusion::ExclusionRules::load(&kb.0, default_exclude_dirs());
        let full = kb.0.join("doc.md");

        let db = dir_rename_db(&["doc.md", "other.md"]);
        let gone = remove_if_still_gone(
            &db,
            &kb.0,
            "doc.md",
            &full,
            &reg,
            &rules,
            OnDisk::Gone,
            RemovalCause::RemoveEvent,
        );
        assert!(
            matches!(gone, RemovalUnderDatabaseLock::Deindexed(Ok(true))),
            "{gone:?}"
        );
        assert_eq!(
            db.all_document_paths().unwrap(),
            vec!["other.md".to_string()]
        );

        let excluded_full = kb.0.join(".obsidian").join("x.md");
        let db = dir_rename_db(&[".obsidian/x.md"]);
        let skipped = remove_if_still_gone(
            &db,
            &kb.0,
            ".obsidian/x.md",
            &excluded_full,
            &reg,
            &rules,
            OnDisk::Gone,
            RemovalCause::RemoveEvent,
        );
        assert!(
            matches!(skipped, RemovalUnderDatabaseLock::Skip),
            "{skipped:?}"
        );
        assert_eq!(
            db.all_document_paths().unwrap(),
            vec![".obsidian/x.md".to_string()]
        );

        for look in [
            OnDisk::Present,
            OnDisk::Unknown(std::io::ErrorKind::PermissionDenied),
        ] {
            let db = dir_rename_db(&["doc.md", "other.md"]);
            let handed_on = remove_if_still_gone(
                &db,
                &kb.0,
                "doc.md",
                &full,
                &reg,
                &rules,
                look,
                RemovalCause::RemoveEvent,
            );
            assert!(
                matches!(handed_on, RemovalUnderDatabaseLock::NeedsBothLocks),
                "{look:?}: {handed_on:?}"
            );
            assert_eq!(
                db.all_document_paths().unwrap(),
                vec!["doc.md".to_string(), "other.md".to_string()],
                "{look:?}"
            );
        }
    }

    /// (kuriya 548, local Codex r2, medium) A remove event whose path is still gone is handled
    /// through the event loop while the embedder lock is held elsewhere (a search embedding a
    /// query on a slow endpoint): the row goes and the batch finishes without waiting for that
    /// lock. Taking both locks for it, as the event loop did for a moment, would block here
    /// until the lock is let go.
    #[test]
    fn i548_a_removal_still_gone_does_not_wait_for_the_embedder_lock() {
        let kb = i549_kb("groove-watcher-i548-no-embedder-wait");
        let full = kb.0.join("doc.md");
        std::fs::remove_file(&full).unwrap();
        let db = Arc::new(Mutex::new(dir_rename_db(&["doc.md", "other.md"])));
        let embedder = Arc::new(Mutex::new(Embedder::stub_for_tests(384)));
        let mut state = WatcherState {
            kb_path: kb.0.clone(),
            db: Arc::clone(&db),
            embedder: Arc::clone(&embedder),
            registry: Arc::new(Registry::defaults()),
            exclude_headings: None,
            rules: crate::exclusion::ExclusionRules::load(&kb.0, default_exclude_dirs()),
            config: WatchConfig::default(),
            watcher_active: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            watch_status: None,
            watch_backoff: WatcherState::DEFAULT_WATCH_BACKOFF,
            batch_probe: None,
        };
        let events = [mk_evt(
            EventKind::Remove(notify_debouncer_full::notify::event::RemoveKind::File),
            vec![full.clone()],
        )];

        let held = embedder.lock().unwrap();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let finished = std::thread::scope(|scope| {
            scope.spawn(|| {
                handle_events(&mut state, &events);
                let _ = done_tx.send(());
            });
            // Generous: the deletion takes milliseconds; the wait only runs out when the
            // removal is blocked on the held lock.
            let finished = done_rx.recv_timeout(Duration::from_secs(20)).is_ok();
            // Let a blocked removal go so the scope can join it either way.
            drop(held);
            finished
        });
        assert!(
            finished,
            "a removal of a path still gone waited for the embedder lock"
        );
        assert_eq!(
            db.lock().unwrap().all_document_paths().unwrap(),
            vec!["other.md".to_string()]
        );
    }

    /// (kuriya 259) A document long enough to give the reindex a chunk to embed (a short body
    /// would be dropped as nothing to index, which also leaves no row).
    const I6_DOC: &str = concat!(
        "---\ntitle: Rename away fixture\ntags: [test]\n---\n",
        "\n",
        "## back at the old path\n",
        "\n",
        "This file was written at the path a rename had just taken a document\n",
        "away from. Written long enough to clear the default per-chunk quality\n",
        "filter, so the reindex that keeps the row has a chunk to embed and\n",
        "writes the row with the hash of these bytes.\n",
    );

    /// (kuriya 259) A knowledge base with `node_modules/a.md` (the file a rename moved into an
    /// excluded directory) and, when `back` is given, `a.md` holding it (a file back at the old
    /// end of the rename).
    fn i6_rename_kb(prefix: &str, back: Option<&str>) -> I549Kb {
        let kb = crate::test_support::unique_temp_path(prefix);
        std::fs::create_dir_all(kb.join("node_modules")).unwrap();
        let kb = I549Kb(kb);
        std::fs::write(kb.0.join("node_modules").join("a.md"), I6_DOC).unwrap();
        if let Some(body) = back {
            std::fs::write(kb.0.join("a.md"), body).unwrap();
        }
        kb
    }

    /// The watcher state the kuriya 259 event-loop tests run [`handle_events`] on.
    fn i6_state(
        kb: &Path,
        db: &Arc<Mutex<Database>>,
        embedder: &Arc<Mutex<Embedder>>,
    ) -> WatcherState {
        WatcherState {
            kb_path: kb.to_path_buf(),
            db: Arc::clone(db),
            embedder: Arc::clone(embedder),
            registry: Arc::new(Registry::defaults()),
            exclude_headings: None,
            rules: crate::exclusion::ExclusionRules::load(kb, default_exclude_dirs()),
            config: WatchConfig::default(),
            watcher_active: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            watch_status: None,
            watch_backoff: WatcherState::DEFAULT_WATCH_BACKOFF,
            batch_probe: None,
        }
    }

    /// The rename of `a.md` into `node_modules/` (an excluded directory), as the debouncer
    /// stitches it.
    fn i6_rename_into_excluded(kb: &Path) -> [DebouncedEvent; 1] {
        [mk_evt(
            EventKind::Modify(ModifyKind::Name(RenameMode::Both)),
            vec![kb.join("a.md"), kb.join("node_modules").join("a.md")],
        )]
    }

    /// (kuriya 259) A file written back at the old end of a rename into
    /// an excluded directory before the batch is handled keeps its row: the old end is looked at
    /// inside the database lock of the delete and reindexed, so the row holds the new bytes
    /// (its hash is no longer the one the rename moved away).
    #[test]
    fn i6_t_i2_a_file_back_at_the_old_end_of_a_rename_into_an_excluded_dir_keeps_its_row() {
        let kb = i6_rename_kb("groove-watcher-i6-t-i2", Some(I6_DOC));
        let db = Arc::new(Mutex::new(dir_rename_db(&["a.md"])));
        let embedder = Arc::new(Mutex::new(Embedder::stub_for_tests(384)));
        let mut state = i6_state(&kb.0, &db, &embedder);

        handle_events(&mut state, &i6_rename_into_excluded(&kb.0));

        let db = db.lock().unwrap();
        assert_eq!(db.all_document_paths().unwrap(), vec!["a.md".to_string()]);
        let hash = db.get_document_hash("a.md").unwrap();
        assert!(
            hash.is_some() && hash.as_deref() != Some("h"),
            "the row was not rewritten by a reindex: {hash:?}"
        );
    }

    /// (kuriya 259) The same rename with nothing back at the old path drops the row, as it did
    /// before the old end was looked at.
    #[test]
    fn i6_a_rename_into_an_excluded_dir_with_nothing_back_drops_the_row() {
        let kb = i6_rename_kb("groove-watcher-i6-nothing-back", None);
        let db = Arc::new(Mutex::new(dir_rename_db(&["a.md"])));
        let embedder = Arc::new(Mutex::new(Embedder::stub_for_tests(384)));
        let mut state = i6_state(&kb.0, &db, &embedder);

        handle_events(&mut state, &i6_rename_into_excluded(&kb.0));

        assert!(db.lock().unwrap().all_document_paths().unwrap().is_empty());
    }

    /// (kuriya 259) The old end of a rename that is gone is settled under the database lock
    /// alone, so the batch finishes while the embedder lock is held elsewhere (a search embedding
    /// a query), as a remove event does.
    #[test]
    fn i6_a_rename_away_from_a_gone_path_does_not_wait_for_the_embedder_lock() {
        let kb = i6_rename_kb("groove-watcher-i6-no-embedder-wait", None);
        let db = Arc::new(Mutex::new(dir_rename_db(&["a.md", "other.md"])));
        let embedder = Arc::new(Mutex::new(Embedder::stub_for_tests(384)));
        let mut state = i6_state(&kb.0, &db, &embedder);
        let events = i6_rename_into_excluded(&kb.0);

        let held = embedder.lock().unwrap();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let finished = std::thread::scope(|scope| {
            scope.spawn(|| {
                handle_events(&mut state, &events);
                let _ = done_tx.send(());
            });
            // Generous: the deletion takes milliseconds; the wait only runs out when the
            // removal is blocked on the held lock.
            let finished = done_rx.recv_timeout(Duration::from_secs(20)).is_ok();
            drop(held);
            finished
        });
        assert!(
            finished,
            "the old end of a rename, still gone, waited for the embedder lock"
        );
        assert_eq!(
            db.lock().unwrap().all_document_paths().unwrap(),
            vec!["other.md".to_string()]
        );
    }

    /// (kuriya 259) A knowledge base holding the empty directories `dirs` and each `(rel, body)`
    /// of `files` (directories on the way are created).
    fn i6_dir_kb(prefix: &str, dirs: &[&str], files: &[(&str, &str)]) -> I549Kb {
        let kb = I549Kb(crate::test_support::unique_temp_path(prefix));
        std::fs::create_dir_all(&kb.0).unwrap();
        for dir in dirs {
            std::fs::create_dir_all(kb.0.join(dir)).unwrap();
        }
        for (rel, body) in files {
            let path = kb.0.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, body).unwrap();
        }
        kb
    }

    /// The rename of the path `old` to another one, both relative to the knowledge base, as the
    /// debouncer stitches it.
    fn i6_rename(kb: &Path, old: &str, new: &str) -> [DebouncedEvent; 1] {
        [mk_evt(
            EventKind::Modify(ModifyKind::Name(RenameMode::Both)),
            vec![kb.join(old), kb.join(new)],
        )]
    }

    /// Runs `events` on a store holding `rows` over `kb`, and returns the paths left in it.
    fn i6_paths_after(kb: &Path, rows: &[&str], events: &[DebouncedEvent]) -> Vec<String> {
        let db = Arc::new(Mutex::new(dir_rename_db(rows)));
        let embedder = Arc::new(Mutex::new(Embedder::stub_for_tests(384)));
        let mut state = i6_state(kb, &db, &embedder);
        handle_events(&mut state, events);
        db.lock().unwrap().all_document_paths().unwrap()
    }

    /// (kuriya 259) A directory rename onto a directory: a listed row its step skips
    /// (`.txt`, which the registry does not take at either end) leaves its old path through the
    /// listed loop, as a rename away does, even though the removal gate would turn the path away.
    /// The old directory is recreated empty so the range sweep of a gone source does not take
    /// the row instead: only the listed loop can drop it.
    #[test]
    fn i6_t_i1_a_scan_destination_rename_drops_the_listed_rows_the_gate_would_turn_away() {
        let kb = i6_dir_kb(
            "groove-watcher-i6-scan-gate",
            &["docs"],
            &[("notes/a.md", I6_DOC)],
        );
        let paths = i6_paths_after(
            &kb.0,
            &["docs/a.md", "docs/c.txt"],
            &i6_rename(&kb.0, "docs", "notes"),
        );
        assert_eq!(paths, vec!["notes/a.md".to_string()]);
    }

    /// (kuriya 259) A directory renamed to a destination already gone,
    /// with the old directory recreated and one file written back into it before the batch is
    /// handled: that file's row stays (reindexed in place), the other listed row leaves.
    #[test]
    fn i6_t_i1_a_sweep_only_rename_keeps_the_row_of_a_file_back() {
        let kb = i6_dir_kb(
            "groove-watcher-i6-sweep-keeps",
            &[],
            &[("docs/a.md", I6_DOC)],
        );
        let paths = i6_paths_after(
            &kb.0,
            &["docs/a.md", "docs/b.md"],
            &i6_rename(&kb.0, "docs", "notes"),
        );
        assert_eq!(paths, vec!["docs/a.md".to_string()]);
    }

    /// (kuriya 259) A directory renamed into an excluded directory, with
    /// a file written back at the old path of one of its rows: the step's deindex looks first,
    /// so that row stays (reindexed in place).
    #[test]
    fn i6_t_i1_a_step_into_an_excluded_dir_keeps_the_row_of_a_file_back() {
        let kb = i6_dir_kb(
            "groove-watcher-i6-step-excluded",
            &[],
            &[("node_modules/docs/a.md", I6_DOC), ("docs/a.md", I6_DOC)],
        );
        let paths = i6_paths_after(
            &kb.0,
            &["docs/a.md"],
            &i6_rename(&kb.0, "docs", "node_modules/docs"),
        );
        assert_eq!(paths, vec!["docs/a.md".to_string()]);
    }

    /// (kuriya 259) A file renamed onto a name that is a directory by
    /// the time the event is handled, with a file written back at the old name: the row of the
    /// old name itself stays (reindexed in place).
    #[test]
    fn i6_t_i1_the_old_name_itself_keeps_the_row_of_a_file_back() {
        let kb = i6_dir_kb("groove-watcher-i6-old-name", &["y"], &[("x.md", I6_DOC)]);
        let paths = i6_paths_after(&kb.0, &["x.md"], &i6_rename(&kb.0, "x.md", "y"));
        assert_eq!(paths, vec!["x.md".to_string()]);
    }

    /// (kuriya 259) A sweep-only directory rename with the old directory recreated: a listed row
    /// the removal gate would turn away (`.txt`) leaves too, as a rename away does.
    #[test]
    fn i6_a_sweep_only_rename_drops_the_listed_rows_the_gate_would_turn_away() {
        let kb = i6_dir_kb(
            "groove-watcher-i6-sweep-gate",
            &[],
            &[("docs/a.md", I6_DOC)],
        );
        let paths = i6_paths_after(
            &kb.0,
            &["docs/a.md", "docs/b.md", "docs/c.txt"],
            &i6_rename(&kb.0, "docs", "notes"),
        );
        assert_eq!(paths, vec!["docs/a.md".to_string()]);
    }

    /// (kuriya 259) A file back under the recreated old directory only under another
    /// case of its name does not keep the old row, and no row is written for the other spelling
    /// (no event named it): the same answer on a file system that tells cases apart and on one
    /// that does not.
    #[test]
    fn i6_a_file_back_only_under_another_case_does_not_keep_the_old_row() {
        let kb = i6_dir_kb(
            "groove-watcher-i6-other-case",
            &[],
            &[("docs/A.md", I6_DOC)],
        );
        let paths = i6_paths_after(&kb.0, &["docs/a.md"], &i6_rename(&kb.0, "docs", "notes"));
        assert!(paths.is_empty(), "{paths:?}");
    }

    /// (kuriya 259, kuriya 566) A hard link written back at the old path of a listed row does not
    /// keep it: the write gate refuses a hard link, so the row leaves.
    #[test]
    fn i6_a_hard_link_back_under_the_old_directory_still_loses_its_row() {
        let kb = i6_dir_kb("groove-watcher-i6-hard-link", &["docs"], &[]);
        let outside = I549Kb(crate::test_support::unique_temp_path(
            "groove-watcher-i6-hard-link-outside",
        ));
        std::fs::create_dir_all(&outside.0).unwrap();
        std::fs::write(outside.0.join("a.md"), I6_DOC).unwrap();
        std::fs::hard_link(outside.0.join("a.md"), kb.0.join("docs").join("a.md"))
            .expect("hard links need no privilege");
        let paths = i6_paths_after(&kb.0, &["docs/a.md"], &i6_rename(&kb.0, "docs", "notes"));
        assert!(paths.is_empty(), "{paths:?}");
    }

    /// (kuriya 259) The control: a plain directory rename with nothing written back moves every
    /// row to the new directory, as before.
    #[test]
    fn i6_t_i1_a_plain_directory_rename_moves_every_row() {
        let kb = i6_dir_kb(
            "groove-watcher-i6-plain-dir",
            &[],
            &[("notes/a.md", I6_DOC), ("notes/b.md", I6_DOC)],
        );
        let paths = i6_paths_after(
            &kb.0,
            &["docs/a.md", "docs/b.md"],
            &i6_rename(&kb.0, "docs", "notes"),
        );
        assert_eq!(
            paths,
            vec!["notes/a.md".to_string(), "notes/b.md".to_string()]
        );
    }

    /// (#543) The line a create or modify prints when the file it had indexed now holds
    /// nothing to embed. `tests/watcher_remove_then_write.rs` waits for this text.
    #[test]
    fn the_emptied_line_names_the_file_and_the_deindex() {
        assert_eq!(
            emptied_message("runbook.md"),
            "watcher: runbook.md has nothing to embed now; deindexed it"
        );
    }

    /// (#543, local Codex, medium) After a reindex that left the file with nothing to embed,
    /// the folded legacy row is swept as for a removal (`false`), whether or not the real key
    /// held a row; every other result keeps the write form (`true`).
    #[test]
    fn a_file_left_with_nothing_to_embed_sweeps_its_folded_row_as_a_removal() {
        for deindexed in [true, false] {
            assert!(!sweep_requires_real_key(&Ok(
                indexer::SingleResult::NothingToIndex {
                    deindexed,
                    frontmatter_unparsed: false,
                }
            )));
        }
        assert!(sweep_requires_real_key(&Ok(
            indexer::SingleResult::Unchanged
        )));
        assert!(sweep_requires_real_key(&Ok(
            indexer::SingleResult::Skipped {
                reason: "file no longer exists",
                frontmatter_unparsed: false,
            }
        )));
        assert!(sweep_requires_real_key(&Err(anyhow::anyhow!(
            "endpoint down"
        ))));
    }

    /// (#543, ADR-0023) A key the index cannot hold is still dropped on a remove event even
    /// when a file is back under that name: it is never reindexed from here.
    #[cfg(windows)]
    #[test]
    fn a_removal_of_a_name_win32_cannot_spell_still_deindexes_when_the_file_is_back() {
        let (kb, _cleanup) = verbatim_scratch_kb("kb-watch-unspellable-i543");
        let reg = Registry::defaults();
        let rules = crate::exclusion::ExclusionRules::load(&kb, default_exclude_dirs());
        std::fs::write(kb.join("CON.md"), "# back\n").unwrap();
        assert_eq!(
            removal_decision("CON.md", &kb.join("CON.md"), &reg, &rules),
            RemovalAction::Deindex
        );
    }

    /// Only the knowledge base's own ignore file triggers a reload.
    #[test]
    fn only_the_root_ignore_file_is_a_reload_trigger() {
        let kb = Path::new("/tmp/kb");
        assert!(is_ignore_file(
            kb,
            &kb.join(crate::exclusion::IGNORE_FILE_NAME)
        ));
        assert!(!is_ignore_file(
            kb,
            &kb.join("sub").join(crate::exclusion::IGNORE_FILE_NAME)
        ));
        assert!(!is_ignore_file(kb, &kb.join("notes.md")));
        assert!(!is_ignore_file(
            kb,
            Path::new("/tmp/other")
                .join(crate::exclusion::IGNORE_FILE_NAME)
                .as_path()
        ));
    }

    #[test]
    fn test_should_process_lite_md_ok() {
        let reg = Registry::defaults();
        let full = Path::new("/tmp/a/notes/a.md");
        assert!(should_process_lite(
            "notes/a.md",
            full,
            &reg,
            &default_exclude_dirs()
        ));
    }

    #[test]
    fn test_should_process_lite_obsidian_rejected() {
        let reg = Registry::defaults();
        let full = Path::new("/tmp/a/.obsidian/workspace.md");
        assert!(!should_process_lite(
            ".obsidian/workspace.md",
            full,
            &reg,
            &default_exclude_dirs()
        ));
        let full2 = Path::new("/tmp/a/sub/.obsidian/x.md");
        assert!(!should_process_lite(
            "sub/.obsidian/x.md",
            full2,
            &reg,
            &default_exclude_dirs()
        ));
    }

    #[test]
    fn test_should_process_lite_wrong_extension() {
        let reg = Registry::defaults();
        let full = Path::new("/tmp/a/notes/a.txt");
        assert!(!should_process_lite(
            "notes/a.txt",
            full,
            &reg,
            &default_exclude_dirs()
        ));
    }

    #[test]
    fn test_should_process_lite_txt_accepted_when_opted_in() {
        let reg = Registry::from_enabled(&["md".into(), "txt".into()]).unwrap();
        let full = Path::new("/tmp/a/notes/a.txt");
        assert!(should_process_lite(
            "notes/a.txt",
            full,
            &reg,
            &default_exclude_dirs()
        ));
    }

    #[test]
    fn test_should_process_lite_db_file_rejected() {
        let reg = Registry::defaults();
        let full = Path::new("/tmp/a/.groove.db");
        assert!(!should_process_lite(
            ".groove.db",
            full,
            &reg,
            &default_exclude_dirs()
        ));
    }

    #[test]
    fn test_should_process_lite_office_owner_file_rejected() {
        let reg = Registry::defaults();
        let full = Path::new("/tmp/a/notes/~$report.docx");
        assert!(!should_process_lite(
            "notes/~$report.docx",
            full,
            &reg,
            &default_exclude_dirs()
        ));
    }

    #[test]
    fn test_should_process_lite_libreoffice_lock_file_rejected() {
        let reg = Registry::defaults();
        let full = Path::new("/tmp/a/notes/.~lock.report.docx#");
        assert!(!should_process_lite(
            "notes/.~lock.report.docx#",
            full,
            &reg,
            &default_exclude_dirs()
        ));
    }

    // docx はこのブランチではまだ未登録拡張子 (Registry::defaults() は md のみ)
    // なので上記 2 test は「lock 判定」と「拡張子未登録」のどちらが理由で
    // false になったか区別できない。lock 判定自体が効いていることを保証する
    // ため、registered 拡張子 (md) でも office lock file なら false になる
    // ことを追加で確認する (indexer::tests::test_collect_source_files_skips_office_lock
    // の watcher 版に相当する regression guard)。
    #[test]
    fn test_should_process_lite_office_owner_file_with_registered_extension_rejected() {
        let reg = Registry::defaults(); // md registered
        let full = Path::new("/tmp/a/notes/~$report.md");
        assert!(!should_process_lite(
            "notes/~$report.md",
            full,
            &reg,
            &default_exclude_dirs()
        ));
    }

    // -----------------------------------------------------------------------
    // classify() のイベント分類テスト (evaluator High #1 / #2 回帰ガード)
    // -----------------------------------------------------------------------

    use notify_debouncer_full::notify::Event;
    use std::time::Instant;

    fn mk_evt(kind: EventKind, paths: Vec<PathBuf>) -> DebouncedEvent {
        let mut event = Event::new(kind);
        event.paths = paths;
        DebouncedEvent {
            event,
            time: Instant::now(),
        }
    }

    #[test]
    fn test_classify_create_is_reindex() {
        let evt = mk_evt(
            EventKind::Create(notify_debouncer_full::notify::event::CreateKind::File),
            vec![PathBuf::from("/tmp/a.md")],
        );
        match classify(&evt) {
            Classified::Reindex(paths) => assert_eq!(paths.len(), 1),
            other => panic!("expected Reindex, got {other:?}"),
        }
    }

    #[test]
    fn test_classify_modify_data_is_reindex() {
        let evt = mk_evt(
            EventKind::Modify(ModifyKind::Data(
                notify_debouncer_full::notify::event::DataChange::Content,
            )),
            vec![PathBuf::from("/tmp/a.md")],
        );
        assert!(matches!(classify(&evt), Classified::Reindex(_)));
    }

    #[test]
    fn test_classify_remove_is_deindex() {
        let evt = mk_evt(
            EventKind::Remove(notify_debouncer_full::notify::event::RemoveKind::File),
            vec![PathBuf::from("/tmp/a.md")],
        );
        assert!(matches!(classify(&evt), Classified::Deindex(_)));
    }

    /// feature-67: Windows reports a removed directory as `RemoveKind::Any`
    /// with no extension; it must still reach the Deindex arm, where the
    /// subtree delete runs.
    #[test]
    fn test_classify_remove_any_extensionless_is_deindex() {
        let evt = mk_evt(
            EventKind::Remove(notify_debouncer_full::notify::event::RemoveKind::Any),
            vec![PathBuf::from("/tmp/sub")],
        );
        match classify(&evt) {
            Classified::Deindex(paths) => assert_eq!(paths, [PathBuf::from("/tmp/sub")]),
            other => panic!("expected Deindex, got {other:?}"),
        }
    }

    /// (local Codex r3 on AX-02) An event path that is now a link -- a directory replaced by
    /// a link, the removal folded away -- drops the rows under it; a real directory, or a
    /// path that is not there, touches nothing. A junction on Windows (`mklink /J`, no
    /// privilege), a symlink on Unix; the link dangles, as the target is never created.
    #[test]
    fn deindex_under_a_link_parts_drops_the_rows_under_a_path_that_is_now_a_link() {
        struct Dir(PathBuf);
        impl Drop for Dir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let root = Dir(crate::test_support::unique_temp_path(
            "groove-watcher-link-subtree",
        ));
        let kb = root.0.join("kb");
        std::fs::create_dir_all(kb.join("real")).unwrap();
        let target = root.0.join("gone");
        std::fs::create_dir_all(&target).unwrap();
        #[cfg(windows)]
        {
            let made = std::process::Command::new("cmd")
                .args([
                    "/c",
                    "mklink",
                    "/J",
                    &kb.join("sub").display().to_string(),
                    &target.display().to_string(),
                ])
                .status()
                .expect("mklink must be runnable");
            assert!(made.success(), "mklink /J needs no elevation");
        }
        #[cfg(unix)]
        std::os::unix::fs::symlink(&target, kb.join("sub")).unwrap();
        std::fs::remove_dir_all(&target).unwrap();

        let db = Database::open_in_memory().unwrap();
        db.verify_embedding_meta("bge-small-en-v1.5", 384).unwrap();
        for path in ["sub/a.md", "sub/deep/b.md", "real/c.md", "missing/d.md"] {
            db.upsert_document(path, None, None, None, None, &[], None, "h", 0)
                .unwrap();
        }

        assert!(deindex_under_a_link_parts(&db, &kb, &kb.join("real")).is_none());
        assert!(deindex_under_a_link_parts(&db, &kb, &kb.join("missing")).is_none());
        assert_eq!(db.all_document_paths().unwrap().len(), 4);

        let (rel, outcome) = deindex_under_a_link_parts(&db, &kb, &kb.join("sub"))
            .expect("a link under the KB is handled");
        assert_eq!(rel, "sub");
        assert_eq!(
            outcome.unwrap(),
            vec!["sub/a.md".to_string(), "sub/deep/b.md".to_string()]
        );
        assert_eq!(
            db.all_document_paths().unwrap(),
            vec!["missing/d.md".to_string(), "real/c.md".to_string()]
        );
    }

    /// (local Codex after PR #391 r1) A file replaced by a link: the row at the link's own
    /// name goes too, not only the rows under it. A junction on Windows, a symlink on Unix,
    /// both dangling.
    #[test]
    fn deindex_under_a_link_parts_drops_the_row_at_the_links_own_name() {
        struct Dir(PathBuf);
        impl Drop for Dir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let root = Dir(crate::test_support::unique_temp_path(
            "groove-watcher-link-leaf",
        ));
        let kb = root.0.join("kb");
        std::fs::create_dir_all(&kb).unwrap();
        let target = root.0.join("gone");
        std::fs::create_dir_all(&target).unwrap();
        let link = kb.join("note.md");
        #[cfg(windows)]
        {
            let made = std::process::Command::new("cmd")
                .args([
                    "/c",
                    "mklink",
                    "/J",
                    &link.display().to_string(),
                    &target.display().to_string(),
                ])
                .status()
                .expect("mklink must be runnable");
            assert!(made.success(), "mklink /J needs no elevation");
        }
        #[cfg(unix)]
        std::os::unix::fs::symlink(&target, &link).unwrap();
        std::fs::remove_dir_all(&target).unwrap();

        let db = Database::open_in_memory().unwrap();
        db.verify_embedding_meta("bge-small-en-v1.5", 384).unwrap();
        for path in ["note.md", "other.md"] {
            db.upsert_document(path, None, None, None, None, &[], None, "h", 0)
                .unwrap();
        }
        let (rel, outcome) =
            deindex_under_a_link_parts(&db, &kb, &link).expect("a link under the KB");
        assert_eq!(rel, "note.md");
        assert_eq!(outcome.unwrap(), vec!["note.md".to_string()]);
        assert_eq!(
            db.all_document_paths().unwrap(),
            vec!["other.md".to_string()]
        );
    }

    /// feature-67: the subtree delete decides from the store alone. The KB is
    /// never created on disk, so nothing here can have been stat'ed.
    #[test]
    fn test_deindex_subtree_parts_drops_only_rows_under_the_removed_dir() {
        let kb = crate::test_support::unique_temp_path("groove-watcher-subtree");
        assert!(!kb.exists(), "the KB must not exist on disk");
        let db = Database::open_in_memory().unwrap();
        db.verify_embedding_meta("bge-small-en-v1.5", 384).unwrap();
        for path in ["sub/a.md", "sub/b.md", "sub-other/c.md"] {
            db.upsert_document(path, None, None, None, None, &[], None, "h", 0)
                .unwrap();
        }

        // The KB root itself: no subtree delete at all.
        assert!(deindex_subtree_parts(&db, &kb, &kb).is_none());
        assert_eq!(db.all_document_paths().unwrap().len(), 3);

        let (rel, outcome) = deindex_subtree_parts(&db, &kb, &kb.join("sub"))
            .expect("a path under the KB is handled");
        assert_eq!(rel, "sub");
        let deleted = outcome.unwrap();
        assert_eq!(
            deleted,
            vec!["sub/a.md".to_string(), "sub/b.md".to_string()]
        );
        assert_eq!(
            db.all_document_paths().unwrap(),
            vec!["sub-other/c.md".to_string()]
        );
        assert!(!kb.exists(), "still nothing on disk");
    }

    /// feature-68: an in-memory store holding `rows`, for the directory-rename
    /// decision. Nothing is created on disk.
    fn dir_rename_db(rows: &[&str]) -> Database {
        let db = Database::open_in_memory().unwrap();
        db.verify_embedding_meta("bge-small-en-v1.5", 384).unwrap();
        for path in rows {
            db.upsert_document(path, None, None, None, None, &[], None, "h", 0)
                .unwrap();
        }
        db
    }

    fn dir_rename_step(old_rel: &str, new_rel: &str, action: RenameAction) -> DirRenameStep {
        DirRenameStep {
            old_rel: old_rel.to_string(),
            new_rel: new_rel.to_string(),
            action,
        }
    }

    /// feature-68: every row under the old directory maps to its file rename,
    /// in the listing's order; a sibling sharing the name as a prefix is not
    /// under it. `.txt` is off in the default registry, so that row skips.
    #[test]
    fn test_dir_rename_steps_maps_each_row_under_the_old_dir() {
        let kb = crate::test_support::unique_temp_path("groove-watcher-dir-rename");
        assert!(!kb.exists(), "the KB must not exist on disk");
        let db = dir_rename_db(&[
            "docs/a.md",
            "docs/deep/b.md",
            "docs/c.txt",
            "docs-other/d.md",
        ]);
        let registry = Registry::defaults();
        let rules = crate::exclusion::ExclusionRules::from_exclude_dirs(default_exclude_dirs());
        let paths = db.document_paths_under("docs").unwrap();

        let steps = dir_rename_steps(&paths, &kb, "docs", "notes", &registry, &rules);

        assert_eq!(
            steps,
            vec![
                dir_rename_step("docs/a.md", "notes/a.md", RenameAction::Rename),
                dir_rename_step("docs/c.txt", "notes/c.txt", RenameAction::Skip),
                dir_rename_step("docs/deep/b.md", "notes/deep/b.md", RenameAction::Rename),
            ]
        );
        assert!(!kb.exists(), "still nothing on disk");
    }

    /// feature-68: a directory moved under a denylisted one leaves the index,
    /// as a single file moved there does.
    #[test]
    fn test_dir_rename_steps_into_an_excluded_dir_deindexes() {
        let kb = crate::test_support::unique_temp_path("groove-watcher-dir-rename");
        assert!(!kb.exists(), "the KB must not exist on disk");
        let db = dir_rename_db(&["docs/a.md", "docs/deep/b.md"]);
        let registry = Registry::defaults();
        let rules = crate::exclusion::ExclusionRules::from_exclude_dirs(default_exclude_dirs());
        let paths = db.document_paths_under("docs").unwrap();

        let steps = dir_rename_steps(&paths, &kb, "docs", "node_modules/docs", &registry, &rules);

        let olds: Vec<&str> = steps.iter().map(|s| s.old_rel.as_str()).collect();
        assert_eq!(olds, vec!["docs/a.md", "docs/deep/b.md"]);
        for step in &steps {
            assert_eq!(step.action, RenameAction::Deindex, "{step:?}");
        }
        assert!(!kb.exists(), "still nothing on disk");
    }

    /// feature-68: a new directory name the index cannot hold (a `.` segment,
    /// refused by [`crate::resources::doc_is_addressable`] on every platform) is a write the name
    /// check turns away, so each row is deindexed.
    #[test]
    fn test_dir_rename_steps_onto_an_unaddressable_name_deindexes() {
        let kb = crate::test_support::unique_temp_path("groove-watcher-dir-rename");
        assert!(!kb.exists(), "the KB must not exist on disk");
        let db = dir_rename_db(&["docs/a.md", "docs/deep/b.md"]);
        let registry = Registry::defaults();
        let rules = crate::exclusion::ExclusionRules::from_exclude_dirs(default_exclude_dirs());
        let paths = db.document_paths_under("docs").unwrap();

        let steps = dir_rename_steps(&paths, &kb, "docs", "notes/.", &registry, &rules);

        assert_eq!(steps.len(), 2, "{steps:?}");
        for step in &steps {
            assert!(
                !crate::resources::doc_is_addressable(&step.new_rel),
                "{step:?}"
            );
            assert_eq!(step.action, RenameAction::Deindex, "{step:?}");
        }
        assert!(!kb.exists(), "still nothing on disk");
    }

    /// feature-68: a nested directory moves only its own rows; a file beside
    /// it in the parent is not touched.
    #[test]
    fn test_dir_rename_steps_nested_keeps_the_rest() {
        let kb = crate::test_support::unique_temp_path("groove-watcher-dir-rename");
        assert!(!kb.exists(), "the KB must not exist on disk");
        let db = dir_rename_db(&["a/b/x.md", "a/b/d/y.md", "a/z.md"]);
        let registry = Registry::defaults();
        let rules = crate::exclusion::ExclusionRules::from_exclude_dirs(default_exclude_dirs());
        let paths = db.document_paths_under("a/b").unwrap();

        let steps = dir_rename_steps(&paths, &kb, "a/b", "c/b", &registry, &rules);

        assert_eq!(
            steps,
            vec![
                dir_rename_step("a/b/d/y.md", "c/b/d/y.md", RenameAction::Rename),
                dir_rename_step("a/b/x.md", "c/b/x.md", RenameAction::Rename),
            ]
        );
        assert!(!kb.exists(), "still nothing on disk");
    }

    /// feature-68: rows left under a directory excluded after they were
    /// indexed follow it out as a reindex of the new path.
    #[test]
    fn test_dir_rename_steps_out_of_an_excluded_dir_reindexes() {
        let kb = crate::test_support::unique_temp_path("groove-watcher-dir-rename");
        assert!(!kb.exists(), "the KB must not exist on disk");
        let db = dir_rename_db(&["node_modules/docs/a.md"]);
        let registry = Registry::defaults();
        let rules = crate::exclusion::ExclusionRules::from_exclude_dirs(default_exclude_dirs());
        let paths = db.document_paths_under("node_modules/docs").unwrap();

        let steps = dir_rename_steps(&paths, &kb, "node_modules/docs", "docs", &registry, &rules);

        assert_eq!(
            steps,
            vec![dir_rename_step(
                "node_modules/docs/a.md",
                "docs/a.md",
                RenameAction::Reindex
            )]
        );
        assert!(!kb.exists(), "still nothing on disk");
    }

    /// feature-68: a path handed in that is not under `old_dir/` is skipped
    /// rather than spliced onto the new directory.
    #[test]
    fn test_dir_rename_steps_skips_a_path_outside_the_old_dir() {
        let kb = crate::test_support::unique_temp_path("groove-watcher-dir-rename");
        assert!(!kb.exists(), "the KB must not exist on disk");
        let registry = Registry::defaults();
        let rules = crate::exclusion::ExclusionRules::from_exclude_dirs(default_exclude_dirs());
        let paths = vec!["docs/a.md".to_string(), "docs-other/d.md".to_string()];

        let steps = dir_rename_steps(&paths, &kb, "docs", "notes", &registry, &rules);

        assert_eq!(
            steps,
            vec![dir_rename_step(
                "docs/a.md",
                "notes/a.md",
                RenameAction::Rename
            )]
        );
        assert!(!kb.exists(), "still nothing on disk");
    }

    /// feature-68: a directory at the destination is a directory rename whose
    /// destination is to be scanned, decided before any listing.
    #[test]
    fn test_dir_rename_entry_dir_scans_the_destination() {
        let entry = dir_rename_entry(ToState::Dir, None, "docs").unwrap();
        assert_eq!(entry, Some(DirRenameEntry::ScanDestination));
    }

    /// feature-68: something other than a directory at the destination is left
    /// to the file rename, even while rows sit under the old name as a prefix.
    #[test]
    fn test_dir_rename_entry_not_a_dir_is_left_to_the_file_rename() {
        let db = dir_rename_db(&["docs/a.md", "docs/deep/b.md"]);
        assert_eq!(db.document_paths_under("docs").unwrap().len(), 2);

        let entry = dir_rename_entry(ToState::NotDir, None, "docs").unwrap();
        assert_eq!(entry, None);
    }

    /// feature-68: nothing at the destination but rows under the old prefix is
    /// a directory rename whose destination is already gone: sweep only.
    #[test]
    fn test_dir_rename_entry_missing_with_rows_sweeps_only() {
        let db = dir_rename_db(&["docs/a.md", "docs/deep/b.md"]);

        let entry = dir_rename_entry(ToState::Missing, Some(&db), "docs").unwrap();
        assert!(matches!(entry, Some(DirRenameEntry::SweepOnly(_))));
    }

    /// feature-68: nothing at the destination and no rows under the old name as
    /// a prefix (a chained file rename) is left to the file rename.
    #[test]
    fn test_dir_rename_entry_missing_without_rows_is_left_to_the_file_rename() {
        let db = dir_rename_db(&["docs/a.md", "docs/deep/b.md"]);

        let entry = dir_rename_entry(ToState::Missing, Some(&db), "a.md").unwrap();
        assert_eq!(entry, None);
    }

    /// feature-68: a listing error comes back as an error, not as "no rows".
    #[test]
    fn test_dir_rename_entry_surfaces_a_listing_error() {
        let db = dir_rename_db(&["docs/a.md", "docs/deep/b.md"]);

        let entry = dir_rename_entry(ToState::Missing, Some(&db), "docs/");
        assert!(entry.is_err(), "{entry:?}");
    }

    /// feature-68: a sibling sharing the old name as a string prefix is not
    /// under the old directory.
    #[test]
    fn test_dir_rename_entry_missing_ignores_a_sibling_prefix() {
        let db = dir_rename_db(&["docs-other/c.md"]);

        let entry = dir_rename_entry(ToState::Missing, Some(&db), "docs").unwrap();
        assert_eq!(entry, None);
    }

    /// feature-68: with nothing at the destination and no store to list, the
    /// rename is left to the file rename.
    #[test]
    fn test_dir_rename_entry_missing_without_a_db_is_none() {
        let entry = dir_rename_entry(ToState::Missing, None, "docs").unwrap();
        assert_eq!(entry, None);
    }

    /// feature-68: something other than a directory at the destination is left
    /// to the file rename even when a store with rows under the old prefix is
    /// handed in. This test and the next carry the proof that `NotDir` and
    /// `Dir` do not list; the earlier `NotDir` / `Dir` tests pass no store.
    #[test]
    fn test_dir_rename_entry_not_dir_ignores_rows_even_with_a_db() {
        let db = dir_rename_db(&["docs/a.md", "docs/deep/b.md"]);

        let entry = dir_rename_entry(ToState::NotDir, Some(&db), "docs").unwrap();
        assert_eq!(entry, None);
    }

    /// feature-68: a directory at the destination is scanned even when the
    /// store handed in has no rows under the old prefix (a directory written
    /// under a temporary name before the watch window).
    #[test]
    fn test_dir_rename_entry_dir_scans_even_with_an_empty_db() {
        let db = dir_rename_db(&[]);

        let entry = dir_rename_entry(ToState::Dir, Some(&db), "docs").unwrap();
        assert_eq!(entry, Some(DirRenameEntry::ScanDestination));
    }

    /// feature-68 (local Codex r4): a destination whose stat failed for a
    /// reason other than "not found" is left to the file rename without
    /// listing, even when a store with rows under the old prefix is handed in.
    /// Read as "gone", those rows would be swept while the directory may well
    /// be there.
    #[test]
    fn test_dir_rename_entry_unknown_does_not_list() {
        let db = dir_rename_db(&["docs/a.md", "docs/deep/b.md"]);

        let entry = dir_rename_entry(ToState::Unknown, Some(&db), "docs").unwrap();
        assert_eq!(entry, None);
    }

    /// feature-68 (local Codex r5): the sweep-only entry carries exactly the
    /// rows it listed under the old directory, sorted, so the caller can drop
    /// them even when the old directory has been recreated. A sibling sharing
    /// the name as a prefix is not among them.
    #[test]
    fn test_dir_rename_entry_missing_carries_the_listed_rows() {
        let db = dir_rename_db(&["docs/deep/b.md", "docs-other/c.md", "docs/a.md"]);

        let entry = dir_rename_entry(ToState::Missing, Some(&db), "docs").unwrap();
        assert_eq!(
            entry,
            Some(DirRenameEntry::SweepOnly(vec![
                "docs/a.md".to_string(),
                "docs/deep/b.md".to_string(),
            ]))
        );
    }

    #[test]
    fn test_classify_rename_both_two_paths_is_rename() {
        let evt = mk_evt(
            EventKind::Modify(ModifyKind::Name(RenameMode::Both)),
            vec![PathBuf::from("/tmp/from.md"), PathBuf::from("/tmp/to.md")],
        );
        match classify(&evt) {
            Classified::Rename { from, to } => {
                assert!(from.ends_with("from.md"));
                assert!(to.ends_with("to.md"));
            }
            other => panic!("expected Rename, got {other:?}"),
        }
    }

    #[test]
    fn test_classify_rename_from_only_is_deindex() {
        // Linux inotify で ペア化されなかった From 単独 → 旧 path 削除
        let evt = mk_evt(
            EventKind::Modify(ModifyKind::Name(RenameMode::From)),
            vec![PathBuf::from("/tmp/from.md")],
        );
        assert!(matches!(classify(&evt), Classified::Deindex(_)));
    }

    #[test]
    fn test_classify_rename_to_only_is_reindex() {
        let evt = mk_evt(
            EventKind::Modify(ModifyKind::Name(RenameMode::To)),
            vec![PathBuf::from("/tmp/to.md")],
        );
        assert!(matches!(classify(&evt), Classified::Reindex(_)));
    }

    #[test]
    fn test_classify_rename_name_any_two_paths_is_rename() {
        // 古い notify / 別プラットフォームで Any + 2 paths が来ても rename 扱い
        let evt = mk_evt(
            EventKind::Modify(ModifyKind::Name(RenameMode::Any)),
            vec![PathBuf::from("/tmp/from.md"), PathBuf::from("/tmp/to.md")],
        );
        assert!(matches!(classify(&evt), Classified::Rename { .. }));
    }

    #[test]
    fn test_classify_rename_both_event_does_not_also_trigger_reindex() {
        // evaluator High #1 の回帰ガード: Modify(Name(Both)) は絶対に
        // Reindex 経路に落ちないこと (二重ディスパッチ防止)。
        let evt = mk_evt(
            EventKind::Modify(ModifyKind::Name(RenameMode::Both)),
            vec![PathBuf::from("/tmp/from.md"), PathBuf::from("/tmp/to.md")],
        );
        let c = classify(&evt);
        assert!(
            !matches!(c, Classified::Reindex(_)),
            "Modify(Name(Both)) must never be Reindex: {c:?}"
        );
    }

    #[test]
    fn test_classify_access_is_ignore() {
        let evt = mk_evt(
            EventKind::Access(notify_debouncer_full::notify::event::AccessKind::Any),
            vec![PathBuf::from("/tmp/a.md")],
        );
        assert!(matches!(classify(&evt), Classified::Ignore));
    }

    /// (codex P1 on PR #168) The escaping only holds if **every** diagnostic
    /// goes through `wdiag!`. Nothing in the compiler notices a fresh
    /// `eprintln!` with a `watcher:` message, and three review rounds were spent
    /// on paths reaching stderr unescaped, so the invariant gets a guard.
    #[test]
    fn every_watcher_diagnostic_goes_through_the_escaping_printer() {
        let src = include_str!("watcher.rs");
        // Assembled at runtime: written as a literal, this needle would match
        // its own source and the test would fail on itself (feature-51 shipped
        // exactly that bug in a source-scanning test).
        let needle = format!("{}{}!(\"watcher", "eprint", "ln");
        assert!(
            !src.contains(&needle),
            "a watcher diagnostic bypasses wdiag!; stderr would carry unescaped \
             text on a CP932 console"
        );
    }

    /// (codex P1 on PR #168) Diagnostics stay ASCII so a CP932 console can read
    /// them, and the escaping is lossless so two different paths never render as
    /// the same line.
    #[test]
    fn ascii_diag_keeps_plain_text_and_escapes_everything_else() {
        assert_eq!(
            ascii_diag("watcher: could not scan notes/a.md: denied"),
            "watcher: could not scan notes/a.md: denied",
            "ASCII graphics and spaces must survive unchanged"
        );

        let escaped = ascii_diag("設計/メモ.md");
        assert!(
            escaped.is_ascii(),
            "a non-ASCII path must not reach stderr: {escaped}"
        );
        assert_ne!(
            escaped,
            ascii_diag("設計/ノート.md"),
            "escaping must stay lossless, or two paths report as one line"
        );

        assert_eq!(
            ascii_diag("a\nb\tc"),
            "a\\nb\\tc",
            "control characters must not break the line into two diagnostics"
        );
    }

    /// (codex P2 on PR #168) A directory subtree is only walked when the
    /// directory *arrived*. `Classified::Reindex` alone is not that signal — it
    /// also carries `Modify(Data/Metadata/Any)`, so keying the scan off it would
    /// re-walk and re-embed a large existing directory whenever its mtime or
    /// permissions changed, with nothing new inside.
    #[test]
    fn only_arrival_events_can_be_hiding_directory_contents() {
        for kind in [
            EventKind::Create(notify_debouncer_full::notify::event::CreateKind::Folder),
            EventKind::Create(notify_debouncer_full::notify::event::CreateKind::Any),
            EventKind::Modify(ModifyKind::Name(RenameMode::To)),
            EventKind::Modify(ModifyKind::Name(RenameMode::Any)),
        ] {
            assert!(is_arrival(&kind), "{kind:?} means the path just arrived");
        }

        for kind in [
            EventKind::Modify(ModifyKind::Data(
                notify_debouncer_full::notify::event::DataChange::Any,
            )),
            EventKind::Modify(ModifyKind::Metadata(
                notify_debouncer_full::notify::event::MetadataKind::Any,
            )),
            EventKind::Modify(ModifyKind::Any),
        ] {
            assert!(
                !is_arrival(&kind),
                "{kind:?} changes a path in place; walking a subtree for it is \
                 unbounded work for nothing"
            );
        }
    }

    /// The two halves have to stay related: every kind that is an arrival must
    /// still classify as `Reindex`, or the scan is keyed off a branch that the
    /// dispatcher never reaches and the Linux fix silently stops working.
    #[test]
    fn every_single_path_arrival_still_classifies_as_reindex() {
        for kind in [
            EventKind::Create(notify_debouncer_full::notify::event::CreateKind::Folder),
            EventKind::Modify(ModifyKind::Name(RenameMode::To)),
        ] {
            let evt = mk_evt(kind, vec![PathBuf::from("/tmp/fresh")]);
            assert!(
                matches!(classify(&evt), Classified::Reindex(_)),
                "{kind:?} must reach the Reindex arm"
            );
        }
    }

    // ---------------------------------------------------------------------
    // F-36: bounded channel backpressure semantics
    // ---------------------------------------------------------------------

    /// `tokio::sync::mpsc::channel(N)` の挙動を直接 test する。
    /// `unbounded_channel` 時代に依存していた「無限に send できる」前提が
    /// もう成立しないことの確認。capacity 1 の channel に 2 連続 try_send
    /// を打つと 2 件目は `Full` になる。
    #[tokio::test]
    async fn test_bounded_channel_try_send_returns_full_at_capacity() {
        use tokio::sync::mpsc;
        let (tx, _rx) = mpsc::channel::<u8>(1);
        assert!(tx.try_send(1).is_ok());
        match tx.try_send(2) {
            Err(mpsc::error::TrySendError::Full(_)) => {}
            other => panic!("expected Full, got {other:?}"),
        }
    }

    /// recv で 1 件抜けば後続の try_send が再度通る。F-36 では「flood
    /// 直後に hot 期間が終わって receiver が追いつけば降伏しない」ことを
    /// 担保する性質。
    #[tokio::test]
    async fn test_bounded_channel_recovers_after_drain() {
        use tokio::sync::mpsc;
        let (tx, mut rx) = mpsc::channel::<u8>(1);
        tx.try_send(1).unwrap();
        // ここでは Full
        assert!(matches!(
            tx.try_send(2),
            Err(mpsc::error::TrySendError::Full(_))
        ));
        // 1 件 drain
        assert_eq!(rx.recv().await, Some(1));
        // 容量回復、2 件目が通る
        assert!(tx.try_send(2).is_ok());
    }

    /// receiver が drop されたら try_send は `Closed` を返す (debouncer
    /// callback がアプリ shutdown 後に静かに死ぬための signal)。
    #[tokio::test]
    async fn test_bounded_channel_closed_when_receiver_dropped() {
        use tokio::sync::mpsc;
        let (tx, rx) = mpsc::channel::<u8>(4);
        drop(rx);
        match tx.try_send(1) {
            Err(mpsc::error::TrySendError::Closed(_)) => {}
            other => panic!("expected Closed, got {other:?}"),
        }
    }

    // -----------------------------------------------------------------------
    // (kuriya issue #520) StaleCreateFilter in front of the real debouncer
    // -----------------------------------------------------------------------

    use notify_debouncer_full::notify::event::{CreateKind, RemoveKind};

    std::thread_local! {
        /// The handler the debouncer gave [`InjectWatcher::new`] on this thread.
        static INJECTED: std::cell::RefCell<Option<Box<dyn notify::EventHandler>>> =
            const { std::cell::RefCell::new(None) };
    }

    /// A backend that watches nothing: the test hands the debouncer the raw
    /// events FSEvents would, through the handler the debouncer passes to
    /// `Watcher::new`.
    struct InjectWatcher;

    impl notify::Watcher for InjectWatcher {
        fn new<F: notify::EventHandler>(h: F, _: notify::Config) -> notify::Result<Self> {
            INJECTED.with(|slot| *slot.borrow_mut() = Some(Box::new(h)));
            Ok(Self)
        }
        fn watch(&mut self, _: &Path, _: RecursiveMode) -> notify::Result<()> {
            Ok(())
        }
        fn unwatch(&mut self, _: &Path) -> notify::Result<()> {
            Ok(())
        }
        fn kind() -> notify::WatcherKind {
            notify::WatcherKind::NullWatcher
        }
    }

    fn inject(kind: EventKind, path: &Path) {
        INJECTED.with(|slot| {
            slot.borrow_mut()
                .as_mut()
                .expect("the debouncer built its backend on this thread")
                .handle_event(Ok(notify::Event::new(kind).add_path(path.to_path_buf())));
        });
    }

    /// Every debounced event (kind, paths) that arrives within `wait`.
    fn drain_debounced(
        rx: &std::sync::mpsc::Receiver<notify_debouncer_full::DebounceEventResult>,
        wait: Duration,
    ) -> Vec<(EventKind, Vec<PathBuf>)> {
        let end = std::time::Instant::now() + wait;
        let mut got = Vec::new();
        while let Some(left) = end.checked_duration_since(std::time::Instant::now()) {
            match rx.recv_timeout(left) {
                Ok(Ok(evs)) => got.extend(evs.into_iter().map(|e| (e.event.kind, e.event.paths))),
                Ok(Err(errs)) => panic!("debouncer errors: {errs:?}"),
                Err(_) => break,
            }
        }
        got
    }

    struct RemoveDirOnDrop(PathBuf);
    impl Drop for RemoveDirOnDrop {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// What FSEvents hands over for a removed file that was created before the
    /// watch: one record, `Create`, `Remove`, `Modify(Data)`, for a path that is
    /// gone. Unfiltered, notify-debouncer-full drops the `Remove`; through the
    /// filter only the `Remove` reaches the watcher.
    #[test]
    fn stale_create_filter_lets_the_remove_of_a_recently_created_file_through() {
        let root = crate::test_support::unique_temp_path("kb-watch-stale-create-rm");
        std::fs::create_dir_all(&root).unwrap();
        let _cleanup = RemoveDirOnDrop(root.clone());
        let gone = root.join("a.md");

        let run = |filtered: bool| {
            let (tx, rx) = std::sync::mpsc::channel();
            let window = Duration::from_millis(100);
            let cache = notify_debouncer_full::NoCache;
            let cfg = notify::Config::default();
            let _d: Box<dyn std::any::Any> = if filtered {
                Box::new(
                    new_debouncer_opt::<_, StaleCreateFilter<InjectWatcher>, _>(
                        window, None, tx, cache, cfg,
                    )
                    .unwrap(),
                )
            } else {
                Box::new(
                    new_debouncer_opt::<_, InjectWatcher, _>(window, None, tx, cache, cfg).unwrap(),
                )
            };
            // The order notify gives one FSEvents record (nightly run 39578259680).
            inject(EventKind::Create(CreateKind::File), &gone);
            inject(EventKind::Remove(RemoveKind::File), &gone);
            inject(
                EventKind::Modify(ModifyKind::Data(
                    notify_debouncer_full::notify::event::DataChange::Content,
                )),
                &gone,
            );
            drain_debounced(&rx, Duration::from_millis(800))
        };

        let unfiltered = run(false);
        assert!(
            !unfiltered
                .iter()
                .any(|(k, _)| matches!(k, EventKind::Remove(_))),
            "control: the unfiltered debouncer is expected to drop a create+remove pair; \
             if it now passes the Remove on, the filter may no longer be needed; got {unfiltered:?}"
        );
        assert_eq!(
            run(true),
            vec![(EventKind::Remove(RemoveKind::File), vec![gone.clone()])]
        );
    }

    /// What FSEvents hands over for a directory renamed shortly after it was
    /// created: for each end a `Create` and a `Modify(Name(Any))`. Unfiltered,
    /// the debouncer turns this into a bare `Create` of the new name and the
    /// old rows stay; through the filter it stitches the rename the watcher's
    /// directory-rename path handles.
    #[test]
    fn stale_create_filter_keeps_a_directory_rename_stitched() {
        let root = crate::test_support::unique_temp_path("kb-watch-stale-create-mv");
        let _cleanup = RemoveDirOnDrop(root.clone());

        let run = |filtered: bool| {
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(root.join("docs")).unwrap();
            std::fs::write(root.join("docs").join("a.md"), "# a\n").unwrap();
            let (tx, rx) = std::sync::mpsc::channel();
            let window = Duration::from_millis(100);
            let cache = notify_debouncer_full::FileIdMap::new();
            let cfg = notify::Config::default();
            let mut kept: Vec<Box<dyn std::any::Any>> = Vec::new();
            // `watch` seeds the file-id cache from disk while `docs/` is
            // still there, as the watcher's initial watch does.
            if filtered {
                let mut d = new_debouncer_opt::<_, StaleCreateFilter<InjectWatcher>, _>(
                    window, None, tx, cache, cfg,
                )
                .unwrap();
                d.watch(&root, RecursiveMode::Recursive).unwrap();
                kept.push(Box::new(d));
            } else {
                let mut d =
                    new_debouncer_opt::<_, InjectWatcher, _>(window, None, tx, cache, cfg).unwrap();
                d.watch(&root, RecursiveMode::Recursive).unwrap();
                kept.push(Box::new(d));
            }
            let (from, to) = (root.join("docs"), root.join("notes"));
            std::fs::rename(&from, &to).unwrap();
            inject(EventKind::Create(CreateKind::Folder), &from);
            inject(EventKind::Modify(ModifyKind::Name(RenameMode::Any)), &from);
            inject(EventKind::Create(CreateKind::Folder), &to);
            inject(EventKind::Modify(ModifyKind::Name(RenameMode::Any)), &to);
            (drain_debounced(&rx, Duration::from_millis(800)), from, to)
        };

        let both = EventKind::Modify(ModifyKind::Name(RenameMode::Both));
        let (got, ..) = run(false);
        assert!(
            !got.iter().any(|(k, _)| *k == both),
            "control: the unfiltered debouncer is expected to lose the rename; got {got:?}"
        );
        let (got, from, to) = run(true);
        assert!(
            got.contains(&(both, vec![from, to])),
            "the filtered debouncer must stitch the directory rename; got {got:?}"
        );
    }

    /// (feature-69) The reindex line names the chunks whose stored vector was kept, beside
    /// the total, and stays ASCII for a CP932 console.
    #[test]
    fn test_reindexed_message_names_the_reused_chunks() {
        let counts = indexer::ChunkVectorCounts {
            embedded: 2,
            reused: 3,
        };
        let plain = reindexed_message("a.md", counts, false);
        assert_eq!(plain, "watcher: reindexed a.md (5 chunks, 3 reused)");
        let unparsed = reindexed_message("a.md", counts, true);
        assert_eq!(
            unparsed,
            "watcher: reindexed a.md (5 chunks, 3 reused, frontmatter unparsed)"
        );
        assert!(plain.is_ascii() && unparsed.is_ascii());
    }

    /// A scratch knowledge base for the #544 write-look tests, removed when the guard drops.
    fn i544_scratch_kb(tag: &str) -> (std::path::PathBuf, impl Drop) {
        let kb = crate::test_support::unique_temp_path(tag);
        std::fs::create_dir_all(&kb).unwrap();
        struct Cleanup(std::path::PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        (kb.clone(), Cleanup(kb))
    }

    /// (#544 A1) A write whose file is gone by the time the locks are held drops the row under
    /// its path, and only that row; no reindex runs.
    #[test]
    fn a_write_whose_file_is_gone_drops_the_row() {
        let (kb, _cleanup) = i544_scratch_kb("groove-watcher-i544-gone");
        let db = dir_rename_db(&["a.md", "other.md"]);
        let look = written_path_on_disk(&kb, "a.md", WriteLook::Spelled);
        assert_eq!(look, OnDisk::Gone);
        let check = act_on_written_path(&db, &kb, "a.md", look, false);
        assert!(
            matches!(check, WrittenPathCheck::Deindexed(Ok(true))),
            "{check:?}"
        );
        assert_eq!(
            db.all_document_paths().unwrap(),
            vec!["other.md".to_string()]
        );
    }

    /// (#544 A3) A write whose name is on disk only in another case (a case-only rename whose
    /// new spelling's event comes on its own) drops the row under the spelling the event
    /// named, and leaves the row under the other spelling. The look reads such a name as gone
    /// on a case-insensitive volume (the old name opens the file) and on a case-sensitive one
    /// (it is simply not there), for the leaf and for a directory on the way.
    #[test]
    fn a_write_whose_name_is_on_disk_only_in_another_case_drops_that_spelling() {
        let (kb, _cleanup) = i544_scratch_kb("groove-watcher-i544-case");
        let db = dir_rename_db(&["Old.md", "old.md"]);
        let check = act_on_written_path(&db, &kb, "Old.md", OnDisk::Gone, false);
        assert!(
            matches!(check, WrittenPathCheck::Deindexed(Ok(true))),
            "{check:?}"
        );
        assert_eq!(db.all_document_paths().unwrap(), vec!["old.md".to_string()]);

        std::fs::write(kb.join("A.md"), "# A\n").unwrap();
        assert_eq!(
            written_path_on_disk(&kb, "a.md", WriteLook::Spelled),
            OnDisk::Gone
        );
        assert_eq!(
            written_path_on_disk(&kb, "A.md", WriteLook::Spelled),
            OnDisk::Present
        );
        std::fs::create_dir_all(kb.join("Dir")).unwrap();
        std::fs::write(kb.join("Dir").join("note.md"), "# n\n").unwrap();
        assert_eq!(
            written_path_on_disk(&kb, "dir/note.md", WriteLook::Spelled),
            OnDisk::Gone
        );
        assert_eq!(
            written_path_on_disk(&kb, "Dir/note.md", WriteLook::Spelled),
            OnDisk::Present
        );
    }

    /// (#544 D3, Review Focus 1) A write whose look cannot tell -- on Windows, an editor holding
    /// the file while it saves -- keeps the row and goes on to the reindex.
    #[test]
    fn a_write_whose_look_cannot_tell_still_reindexes() {
        let (kb, _cleanup) = i544_scratch_kb("groove-watcher-i544-unknown");
        let db = dir_rename_db(&["locked.md", "other.md"]);
        let denied = std::io::ErrorKind::PermissionDenied;
        let check = act_on_written_path(&db, &kb, "locked.md", OnDisk::Unknown(denied), false);
        assert!(
            matches!(check, WrittenPathCheck::ProceedUnknown(k) if k == denied),
            "{check:?}"
        );
        assert_eq!(
            db.all_document_paths().unwrap(),
            vec!["locked.md".to_string(), "other.md".to_string()]
        );
    }

    /// (#544) A write of a file that is there under its exact spelling touches no row before
    /// the reindex.
    #[test]
    fn a_write_of_a_present_file_proceeds() {
        let (kb, _cleanup) = i544_scratch_kb("groove-watcher-i544-present");
        std::fs::write(kb.join("a.md"), "# a\n").unwrap();
        let db = dir_rename_db(&["a.md", "other.md"]);
        let look = written_path_on_disk(&kb, "a.md", WriteLook::Spelled);
        assert_eq!(look, OnDisk::Present);
        let check = act_on_written_path(&db, &kb, "a.md", look, false);
        assert!(matches!(check, WrittenPathCheck::Proceed), "{check:?}");
        assert_eq!(
            db.all_document_paths().unwrap(),
            vec!["a.md".to_string(), "other.md".to_string()]
        );
    }

    /// (#544 C4, P1-L file branch) A file whose name is not UTF-8 is keyed by a lossy spelling
    /// (U+FFFD), and a look at that spelling never finds it. Its write is not read as gone:
    /// the row stays, as before the look existed. Asked of the event's path, so with none at
    /// hand nothing is lossy.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_write_under_a_lossy_name_is_never_read_as_gone() {
        use std::os::unix::ffi::OsStrExt;
        let (kb, _cleanup) = i544_scratch_kb("groove-watcher-i544-lossy");
        let raw = kb.join(std::ffi::OsStr::from_bytes(b"a\xff.md"));
        std::fs::write(&raw, "# a\n").unwrap();
        let rel = indexer::index_rel_path(&kb, &raw).unwrap();
        let db = dir_rename_db(&[rel.as_str()]);
        assert!(rel_is_lossy(&kb, Some(&raw)));
        assert!(!rel_is_lossy(&kb, None));
        assert_eq!(
            written_path_on_disk(&kb, &rel, WriteLook::Spelled),
            OnDisk::Gone,
            "the lossy spelling names no file"
        );
        let check = act_on_written_path(&db, &kb, &rel, OnDisk::Gone, true);
        assert!(matches!(check, WrittenPathCheck::Proceed), "{check:?}");
        assert_eq!(db.all_document_paths().unwrap(), vec![rel]);
    }

    /// (#544 C4) A name holding a real U+FFFD is spelled exactly, so it is not lossy and its
    /// look is the ordinary one.
    #[test]
    fn a_real_replacement_character_in_a_name_is_not_lossy() {
        let (kb, _cleanup) = i544_scratch_kb("groove-watcher-i544-fffd");
        let raw = kb.join("a\u{FFFD}.md");
        std::fs::write(&raw, "# a\n").unwrap();
        assert!(!rel_is_lossy(&kb, Some(&raw)));
        assert_eq!(
            written_path_on_disk(&kb, "a\u{FFFD}.md", WriteLook::Spelled),
            OnDisk::Present
        );
    }

    /// (#544 C4) An event path that cannot be placed under the knowledge base at all, even
    /// canonicalized, is read as lossy: no look at a `rel` made for it can be trusted, so the
    /// write keeps the row as it did before the look existed.
    #[test]
    fn an_event_path_that_cannot_be_placed_under_the_kb_reads_as_lossy() {
        let (kb, _cleanup) = i544_scratch_kb("groove-watcher-i544-unplaced");
        let (elsewhere, _cleanup2) = i544_scratch_kb("groove-watcher-i544-elsewhere");
        assert!(rel_is_lossy(&kb, Some(&elsewhere.join("x.md"))));
        std::fs::write(kb.join("x.md"), "# x\n").unwrap();
        assert!(!rel_is_lossy(&kb, Some(&kb.join("x.md"))));
    }

    /// (#544) A reindex that settled the row -- written, unchanged, refreshed, or dropped by
    /// the indexer -- is not looked at again, and no row is touched.
    #[test]
    fn a_settled_write_reindex_does_not_look_again() {
        let (kb, _cleanup) = i544_scratch_kb("groove-watcher-i544-settled");
        let settled: [Result<indexer::SingleResult>; 5] = [
            Ok(indexer::SingleResult::Updated {
                chunks: 1,
                frontmatter_unparsed: false,
            }),
            Ok(indexer::SingleResult::Unchanged),
            Ok(indexer::SingleResult::MetadataRefreshed {
                frontmatter_unparsed: false,
            }),
            Ok(indexer::SingleResult::NothingToIndex {
                deindexed: true,
                frontmatter_unparsed: false,
            }),
            Ok(indexer::SingleResult::OutsideKnowledgeBaseDeindexed),
        ];
        for result in &settled {
            let db = dir_rename_db(&["a.md", "other.md"]);
            let got = settle_write_reindex(&db, &kb, "a.md", result, false, || {
                panic!("a settled reindex must not look again: {result:?}")
            });
            assert!(
                matches!(got, WriteReindexSettled::Kept),
                "{result:?}: {got:?}"
            );
            assert_eq!(
                db.all_document_paths().unwrap(),
                vec!["a.md".to_string(), "other.md".to_string()],
                "{result:?}"
            );
        }
    }

    /// (#544 A1) A reindex that left the row as it was -- a skip whatever its reason, a
    /// refusal, an error -- looks again: a path gone by then drops its row; one present or
    /// that cannot be looked at keeps it, and so does a lossy `rel` whatever the look says.
    #[test]
    fn an_unsettled_write_reindex_drops_the_row_when_the_file_is_gone() {
        let (kb, _cleanup) = i544_scratch_kb("groove-watcher-i544-unsettled");
        let unsettled = || -> [Result<indexer::SingleResult>; 3] {
            [
                Ok(indexer::SingleResult::Skipped {
                    reason: "file no longer exists",
                    frontmatter_unparsed: false,
                }),
                Ok(indexer::SingleResult::Refused),
                Err(anyhow::anyhow!("x")),
            ]
        };
        for result in &unsettled() {
            let db = dir_rename_db(&["a.md", "other.md"]);
            let got = settle_write_reindex(&db, &kb, "a.md", result, false, || OnDisk::Gone);
            assert!(
                matches!(got, WriteReindexSettled::Deindexed(Ok(true))),
                "{result:?}: {got:?}"
            );
            assert_eq!(
                db.all_document_paths().unwrap(),
                vec!["other.md".to_string()],
                "{result:?}"
            );
        }
        for look in [
            OnDisk::Present,
            OnDisk::Unknown(std::io::ErrorKind::PermissionDenied),
        ] {
            for result in &unsettled() {
                let db = dir_rename_db(&["a.md", "other.md"]);
                let got = settle_write_reindex(&db, &kb, "a.md", result, false, || look);
                assert!(
                    matches!(got, WriteReindexSettled::LeftAsIs),
                    "{look:?} {result:?}: {got:?}"
                );
                assert_eq!(
                    db.all_document_paths().unwrap(),
                    vec!["a.md".to_string(), "other.md".to_string()],
                    "{look:?} {result:?}"
                );
            }
        }
        for result in &unsettled() {
            let db = dir_rename_db(&["a.md", "other.md"]);
            let got = settle_write_reindex(&db, &kb, "a.md", result, true, || OnDisk::Gone);
            assert!(
                matches!(got, WriteReindexSettled::LeftAsIs),
                "lossy {result:?}: {got:?}"
            );
            assert_eq!(
                db.all_document_paths().unwrap(),
                vec!["a.md".to_string(), "other.md".to_string()],
                "lossy {result:?}"
            );
        }
    }

    /// (#544) The look that only asks whether a regular file is there reads a directory, and a
    /// missing path, as gone, and a file as present.
    #[test]
    fn exists_only_look_reads_a_directory_as_gone() {
        let (kb, _cleanup) = i544_scratch_kb("groove-watcher-i544-existsonly");
        std::fs::create_dir_all(kb.join("d.md")).unwrap();
        std::fs::write(kb.join("f.md"), "# f\n").unwrap();
        assert_eq!(
            written_path_on_disk(&kb, "d.md", WriteLook::ExistsOnly),
            OnDisk::Gone
        );
        assert_eq!(
            written_path_on_disk(&kb, "f.md", WriteLook::ExistsOnly),
            OnDisk::Present
        );
        assert_eq!(
            written_path_on_disk(&kb, "missing.md", WriteLook::ExistsOnly),
            OnDisk::Gone
        );
    }

    /// (#544 P1-I) On macOS a rename can arrive as an unpaired `Modify(Name(Any))` for the old
    /// name, a path that is gone. The debouncer hands it on as one event of one path, the
    /// watcher classifies it as a reindex of that path, and the look inside the locks then
    /// finds the path gone and drops its row, with the event's own path deciding lossiness.
    #[test]
    fn an_unpaired_rename_reaches_the_watcher_as_a_write_of_one_path() {
        let (kb, _cleanup) = i544_scratch_kb("groove-watcher-i544-unpaired");
        let gone = kb.join("a.md");
        let name_any = EventKind::Modify(ModifyKind::Name(RenameMode::Any));

        let (tx, rx) = std::sync::mpsc::channel();
        let _d = new_debouncer_opt::<_, InjectWatcher, _>(
            Duration::from_millis(100),
            None,
            tx,
            notify_debouncer_full::NoCache,
            notify::Config::default(),
        )
        .unwrap();
        inject(name_any, &gone);
        let got = drain_debounced(&rx, Duration::from_millis(800));
        assert_eq!(got, vec![(name_any, vec![gone.clone()])]);

        let evt = mk_evt(got[0].0, got[0].1.clone());
        let Classified::Reindex(paths) = classify(&evt) else {
            panic!("an unpaired Name(Any) must be a reindex");
        };
        assert_eq!(paths, std::slice::from_ref(&gone));

        let rel = to_rel(&kb, &paths[0]).unwrap();
        assert_eq!(rel, "a.md");
        let db = dir_rename_db(&["a.md", "b.md"]);
        let check = act_on_written_path(
            &db,
            &kb,
            &rel,
            written_path_on_disk(&kb, &rel, WriteLook::Spelled),
            rel_is_lossy(&kb, Some(&paths[0])),
        );
        assert!(
            matches!(check, WrittenPathCheck::Deindexed(Ok(true))),
            "{check:?}"
        );
        assert_eq!(db.all_document_paths().unwrap(), vec!["b.md".to_string()]);
    }

    /// (#544) The scan of a directory that just arrived hands over, with each `rel`, the path
    /// the walk found it at, so the look inside the locks can ask that path whether its name is
    /// lossy.
    #[test]
    fn the_scan_of_a_new_directory_hands_over_the_paths_it_found() {
        let (kb, _cleanup) = i544_scratch_kb("groove-watcher-i544-scan");
        std::fs::create_dir_all(kb.join("new").join("sub")).unwrap();
        std::fs::write(kb.join("new").join("x.md"), "# x\n").unwrap();
        std::fs::write(kb.join("new").join("sub").join("y.md"), "# y\n").unwrap();
        let reg = Registry::defaults();
        let rules = crate::exclusion::ExclusionRules::load(&kb, default_exclude_dirs());

        let mut found = new_directory_entries(&kb, &kb.join("new"), &reg, &rules).unwrap();
        found.sort();
        let rels: Vec<&str> = found.iter().map(|(rel, _)| rel.as_str()).collect();
        assert_eq!(rels, vec!["new/sub/y.md", "new/x.md"]);
        for (rel, path) in &found {
            assert_eq!(
                path.canonicalize().unwrap(),
                kb.join(rel).canonicalize().unwrap(),
                "{rel}"
            );
            assert!(!rel_is_lossy(&kb, Some(path)), "{rel}");
        }
    }

    /// (#544 A9/A10) A rename whose destination is not on disk under that spelling (gone, a
    /// directory, or there only in another case) moves no row: both ends' rows go, and the
    /// indexer's rename is not run.
    #[test]
    fn a_rename_whose_destination_is_gone_drops_both_rows() {
        let (kb, _cleanup) = i544_scratch_kb("groove-watcher-i544-rename-gone");
        let db = dir_rename_db(&["a.md", "B.md", "other.md"]);
        let plan = rename_plan(&db, "a.md", "B.md", OnDisk::Gone).unwrap();
        assert_eq!(plan, RenamePlan::DropBoth);
        let run_rename = apply_rename_plan_before(&db, &kb, "a.md", "B.md", &plan).unwrap();
        assert!(!run_rename);
        assert_eq!(
            db.all_document_paths().unwrap(),
            vec!["other.md".to_string()]
        );
    }

    /// (#544 D3, C2) A rename whose destination cannot be looked at deletes nothing: with no
    /// row at the old end, or rows at both ends, the rows stay as they are; with a row at the
    /// old end only, the row moves to the new end, its content left as it was.
    #[test]
    fn a_rename_whose_destination_cannot_be_looked_at_keeps_or_moves_rows() {
        let (kb, _cleanup) = i544_scratch_kb("groove-watcher-i544-rename-unknown");
        let denied = std::io::ErrorKind::PermissionDenied;
        let cases: [(&[&str], RenamePlan, &[&str]); 3] = [
            (
                &["b.md", "other.md"],
                RenamePlan::LeaveUnknown(denied),
                &["b.md", "other.md"],
            ),
            (
                &["a.md", "b.md", "other.md"],
                RenamePlan::LeaveUnknown(denied),
                &["a.md", "b.md", "other.md"],
            ),
            (
                &["a.md", "other.md"],
                RenamePlan::MoveRowUnknown(denied),
                &["b.md", "other.md"],
            ),
        ];
        for (rows, want_plan, want_rows) in cases {
            let db = dir_rename_db(rows);
            let plan = rename_plan(&db, "a.md", "b.md", OnDisk::Unknown(denied)).unwrap();
            assert_eq!(plan, want_plan, "{rows:?}");
            let run_rename = apply_rename_plan_before(&db, &kb, "a.md", "b.md", &plan).unwrap();
            assert!(!run_rename, "{rows:?}");
            assert_eq!(db.all_document_paths().unwrap(), want_rows, "{rows:?}");
        }
    }

    /// (#544 J4) A row moved unread to a path read by another parser (`a.md` to `a.txt`) gets
    /// the awaiting hash, so the next event for `a.txt` reads it again. Within one parser
    /// (`a.md` to `b.md`) the hash stays, and the plans that do not move a row unread write
    /// nothing.
    #[test]
    fn a_row_moved_unread_across_a_parser_awaits_a_reparse() {
        let (kb, _cleanup) = i544_scratch_kb("groove-watcher-i544-unread-cross-parser");
        let reg = Registry::from_enabled(&["md".into(), "txt".into()]).unwrap();
        let denied = std::io::ErrorKind::PermissionDenied;

        let moved = |new_rel: &str| {
            let db = dir_rename_db(&["a.md", "other.md"]);
            let plan = rename_plan(&db, "a.md", new_rel, OnDisk::Unknown(denied)).unwrap();
            assert_eq!(plan, RenamePlan::MoveRowUnknown(denied), "{new_rel}");
            let before = apply_rename_plan_before(&db, &kb, "a.md", new_rel, &plan);
            assert!(matches!(before, Ok(false)), "{new_rel}: {before:?}");
            let wrote = await_reparse_after_unread_move(&db, &reg, "a.md", new_rel, &plan, &before)
                .unwrap();
            (db, wrote)
        };

        let (db, wrote) = moved("a.txt");
        assert!(wrote);
        assert_eq!(db.get_document_hash("a.md").unwrap(), None);
        assert_eq!(
            db.get_document_hash("a.txt").unwrap().as_deref(),
            Some(indexer::HASH_AWAITING_REPARSE)
        );
        assert_eq!(
            db.get_document_hash("other.md").unwrap().as_deref(),
            Some("h")
        );

        let (db, wrote) = moved("b.md");
        assert!(!wrote);
        assert_eq!(db.get_document_hash("b.md").unwrap().as_deref(), Some("h"));

        // A plain rename across a parser: the indexer's rename reads the file, nothing here.
        let db = dir_rename_db(&["a.md"]);
        let before = Ok(true);
        assert!(
            !await_reparse_after_unread_move(
                &db,
                &reg,
                "a.md",
                "a.txt",
                &RenamePlan::Rename,
                &before
            )
            .unwrap()
        );
        assert_eq!(db.get_document_hash("a.md").unwrap().as_deref(), Some("h"));

        // Left as it was: the row at `a.md` keeps its hash and none appears at `a.txt`.
        let plan = RenamePlan::LeaveUnknown(denied);
        let before = apply_rename_plan_before(&db, &kb, "a.md", "a.txt", &plan);
        assert!(
            !await_reparse_after_unread_move(&db, &reg, "a.md", "a.txt", &plan, &before).unwrap()
        );
        assert_eq!(db.get_document_hash("a.md").unwrap().as_deref(), Some("h"));
        assert_eq!(db.get_document_hash("a.txt").unwrap(), None);

        // A move that failed wrote no row at `a.txt`, so there is no hash to mark.
        let plan = RenamePlan::MoveRowUnknown(denied);
        let before = Err(anyhow::anyhow!("injected failure of the move"));
        assert!(
            !await_reparse_after_unread_move(&db, &reg, "a.md", "a.txt", &plan, &before).unwrap()
        );
        assert_eq!(db.get_document_hash("a.md").unwrap().as_deref(), Some("h"));
    }

    /// (#544 C1, D6) A rename over a path that holds a row, from one that holds a row too:
    /// the destination's row goes first, so the indexer's move of the source row does not run
    /// into it on the unique key.
    #[test]
    fn an_overwriting_rename_onto_an_indexed_path_clears_the_destination_first() {
        let (kb, _cleanup) = i544_scratch_kb("groove-watcher-i544-overwrite");
        let db = dir_rename_db(&["a.md", "b.md", "other.md"]);
        let plan = rename_plan(&db, "b.md", "a.md", OnDisk::Present).unwrap();
        assert_eq!(plan, RenamePlan::ClearDestinationThenRename);
        let run_rename = apply_rename_plan_before(&db, &kb, "b.md", "a.md", &plan).unwrap();
        assert!(run_rename);
        assert_eq!(
            db.all_document_paths().unwrap(),
            vec!["b.md".to_string(), "other.md".to_string()]
        );
    }

    /// (#544, spec r1 M1) A rename onto an indexed path from one with no row clears nothing:
    /// the indexer reindexes the destination in place (`OldPathMissing`), reusing its vectors.
    #[test]
    fn a_rename_with_no_source_row_does_not_clear_the_destination() {
        let (kb, _cleanup) = i544_scratch_kb("groove-watcher-i544-no-source");
        let db = dir_rename_db(&["a.md", "other.md"]);
        let plan = rename_plan(&db, "b.md", "a.md", OnDisk::Present).unwrap();
        assert_eq!(plan, RenamePlan::Rename);
        let run_rename = apply_rename_plan_before(&db, &kb, "b.md", "a.md", &plan).unwrap();
        assert!(run_rename);
        assert_eq!(
            db.all_document_paths().unwrap(),
            vec!["a.md".to_string(), "other.md".to_string()]
        );
    }

    /// (#544, feature-68 r11) A rename whose two ends are the same path is not an overwrite:
    /// its own row is not cleared.
    #[test]
    fn a_rename_onto_itself_is_not_an_overwrite() {
        let (kb, _cleanup) = i544_scratch_kb("groove-watcher-i544-self");
        let db = dir_rename_db(&["docs"]);
        let plan = rename_plan(&db, "docs", "docs", OnDisk::Present).unwrap();
        assert_eq!(plan, RenamePlan::Rename);
        let run_rename = apply_rename_plan_before(&db, &kb, "docs", "docs", &plan).unwrap();
        assert!(run_rename);
        assert_eq!(db.all_document_paths().unwrap(), vec!["docs".to_string()]);
    }

    /// (#544 A7/A8) Which outcomes of the indexer's rename settle the row, and which leave it
    /// for a second look at the destination. Named one by one, as the function does.
    #[test]
    fn the_settled_rename_outcomes_are_named_one_by_one() {
        use indexer::RenameOutcome as R;
        let settled = [
            R::Renamed,
            R::RenamedAndReindexed { chunks: 1 },
            R::OldPathMissing,
            R::RenamedSizeCapped,
            R::RenamedSizeCappedAndDropped,
            R::RenamedButRefusedAndDropped,
            R::MovedOutOfTheKnowledgeBase,
        ];
        for outcome in settled {
            let label = format!("{outcome:?}");
            assert!(rename_outcome_is_settled(&Ok(outcome)), "{label}");
        }
        let unsettled = [
            R::OldPathMissingAndRefused,
            R::RenamedButRefused,
            R::RenamedButNotReindexed,
        ];
        for outcome in unsettled {
            let label = format!("{outcome:?}");
            assert!(!rename_outcome_is_settled(&Ok(outcome)), "{label}");
        }
        assert!(!rename_outcome_is_settled(&Err(anyhow::anyhow!("x"))));
    }

    /// (#544 PR-2) A cross-parser rename that kept its row because the second read changed is
    /// not settled: the destination is looked at again, as for the other kept-row outcomes.
    #[test]
    fn the_awaiting_rename_outcome_is_looked_at_again() {
        assert!(!rename_outcome_is_settled(&Ok(
            indexer::RenameOutcome::RenamedAcrossParsersAwaitingReparse
        )));
    }

    /// (#544 PR-2) The line for a cross-parser rename kept awaiting a reparse names both paths
    /// and stays ASCII for a CP932 console.
    #[test]
    fn the_awaiting_rename_line_is_ascii_and_names_both_paths() {
        let line = awaiting_reparse_rename_message("dir/note.txt", "dir/note.md");
        assert!(line.is_ascii(), "{line}");
        assert!(
            line.contains("dir/note.txt -> dir/note.md"),
            "the line does not name both paths: {line}"
        );
        assert!(
            !line.contains("dropped"),
            "the row was kept, yet the line says it went: {line}"
        );
    }

    /// The rows of an overwriting rename of `b.md` onto `a.md`, for the tests of a failure after
    /// the destination's row went: the source's row has its own hash, so a test can tell which
    /// content ended up at `a.md`.
    fn overwrite_db() -> Database {
        let db = dir_rename_db(&["a.md", "other.md"]);
        db.upsert_document("b.md", None, None, None, None, &[], None, "hb", 0)
            .unwrap();
        db
    }

    /// The reindex a test hands [`finish_rename_locked`]: writes a row at `rel` with hash
    /// `fresh`, as a reindex of the file on disk would.
    fn write_fresh_row(db: &Database, rel: &str) -> Result<indexer::SingleResult> {
        db.upsert_document(rel, None, None, None, None, &[], None, "fresh", 0)?;
        Ok(indexer::SingleResult::Updated {
            chunks: 1,
            frontmatter_unparsed: false,
        })
    }

    /// (#544 C1) An overwriting rename whose indexer's rename fails before it moves the source's
    /// row (a stat or a read racing the first look, a DB error): the destination's row is gone,
    /// so the file at the destination is reindexed inside the same locks, and the source's row,
    /// whose file is gone, goes. Both looks a rename takes: a file rename's and a directory
    /// rename's step.
    #[test]
    fn an_overwrite_whose_rename_fails_before_the_move_reindexes_the_destination() {
        for look in [WriteLook::Spelled, WriteLook::ExistsOnly] {
            let (kb, _cleanup) = i544_scratch_kb("groove-watcher-i544-overwrite-fail");
            std::fs::write(kb.join("a.md"), "# b\n").unwrap();
            let db = overwrite_db();
            let plan = rename_plan(&db, "b.md", "a.md", OnDisk::Present).unwrap();
            assert_eq!(plan, RenamePlan::ClearDestinationThenRename);
            let before = apply_rename_plan_before(&db, &kb, "b.md", "a.md", &plan);
            let mut reindexed = 0;
            finish_rename_locked(
                &db,
                &kb,
                ("b.md", "a.md"),
                look,
                (plan, before),
                &mut reindexed,
                |_| Err(anyhow::anyhow!("injected failure before the move")),
                |n| {
                    *n += 1;
                    write_fresh_row(&db, "a.md")
                },
            );
            assert_eq!(reindexed, 1, "{look:?}");
            assert_eq!(
                db.all_document_paths().unwrap(),
                vec!["a.md".to_string(), "other.md".to_string()],
                "{look:?}"
            );
            assert_eq!(
                db.get_document_hash("a.md").unwrap().as_deref(),
                Some("fresh"),
                "{look:?}"
            );
        }
    }

    /// (#544 C1, local Codex r3) The same failure, and the reindex of the destination fails
    /// too: the source's row does not move onto the destination, even though the file there
    /// holds the source's bytes. The source is gone, so its row goes, and the destination is
    /// left with no row for its next event or a full index. A source back on disk by then keeps
    /// its row, and the destination still gets none.
    #[test]
    fn an_overwrite_whose_reindex_also_fails_leaves_the_destination_with_no_row() {
        use sha2::Digest;
        let source_hash = format!("{:x}", sha2::Sha256::digest(b"# b\n"));
        for source_back in [false, true] {
            let (kb, _cleanup) = i544_scratch_kb("groove-watcher-i544-overwrite-fail-twice");
            std::fs::write(kb.join("a.md"), "# b\n").unwrap();
            if source_back {
                std::fs::write(kb.join("b.md"), "# b again\n").unwrap();
            }
            let db = overwrite_db();
            db.upsert_document("b.md", None, None, None, None, &[], None, &source_hash, 0)
                .unwrap();
            let plan = rename_plan(&db, "b.md", "a.md", OnDisk::Present).unwrap();
            let before = apply_rename_plan_before(&db, &kb, "b.md", "a.md", &plan);
            finish_rename_locked(
                &db,
                &kb,
                ("b.md", "a.md"),
                WriteLook::Spelled,
                (plan, before),
                &mut (),
                |_| Err(anyhow::anyhow!("injected failure before the move")),
                |_| Err(anyhow::anyhow!("injected reindex failure")),
            );
            assert_eq!(
                db.get_document_hash("a.md").unwrap(),
                None,
                "source back: {source_back}"
            );
            let expected: Vec<String> = if source_back {
                vec!["b.md".to_string(), "other.md".to_string()]
            } else {
                vec!["other.md".to_string()]
            };
            assert_eq!(
                db.all_document_paths().unwrap(),
                expected,
                "source back: {source_back}"
            );
        }
    }

    /// (#544 C1, local Codex r2) The same two failures, and the file at the destination is no
    /// longer the one the source's row was read from (an editor or another process replaced it
    /// after the overwrite): the source's row does not move onto it, so the replaced file is
    /// not served with the source's old content. The source is gone, so its row goes, and the
    /// destination is left with no row for its next event or a full index.
    #[test]
    fn an_overwrite_whose_destination_was_replaced_does_not_move_the_source_row() {
        use sha2::Digest;
        let (kb, _cleanup) = i544_scratch_kb("groove-watcher-i544-overwrite-replaced");
        std::fs::write(kb.join("a.md"), "# replaced\n").unwrap();
        let db = overwrite_db();
        let source_hash = format!("{:x}", sha2::Sha256::digest(b"# b\n"));
        db.upsert_document("b.md", None, None, None, None, &[], None, &source_hash, 0)
            .unwrap();
        let plan = rename_plan(&db, "b.md", "a.md", OnDisk::Present).unwrap();
        let before = apply_rename_plan_before(&db, &kb, "b.md", "a.md", &plan);
        finish_rename_locked(
            &db,
            &kb,
            ("b.md", "a.md"),
            WriteLook::Spelled,
            (plan, before),
            &mut (),
            |_| Err(anyhow::anyhow!("injected failure before the move")),
            |_| Err(anyhow::anyhow!("injected reindex failure")),
        );
        assert_eq!(
            db.all_document_paths().unwrap(),
            vec!["other.md".to_string()]
        );
    }

    /// (#544 C1, local Codex r3) An overwriting rename across parsers (`b.md` onto `a.docx`, the
    /// bytes still Markdown) fails before the move, and the reindex of `a.docx` writes nothing
    /// (the docx parser skips Markdown bytes). The bytes hash to the source's row, but that row
    /// holds Markdown chunks, which the docx parser would not have written: it does not move onto
    /// `a.docx`. The source is gone, so its row goes, and `a.docx` is left with no row.
    #[test]
    fn an_overwrite_across_parsers_whose_reindex_writes_nothing_does_not_move_the_source_row() {
        use sha2::Digest;
        let (kb, _cleanup) = i544_scratch_kb("groove-watcher-i544-overwrite-cross-parser");
        std::fs::write(kb.join("a.docx"), "# b\n").unwrap();
        let db = dir_rename_db(&["a.docx", "other.md"]);
        let source_hash = format!("{:x}", sha2::Sha256::digest(b"# b\n"));
        db.upsert_document("b.md", None, None, None, None, &[], None, &source_hash, 0)
            .unwrap();
        let plan = rename_plan(&db, "b.md", "a.docx", OnDisk::Present).unwrap();
        assert_eq!(plan, RenamePlan::ClearDestinationThenRename);
        let before = apply_rename_plan_before(&db, &kb, "b.md", "a.docx", &plan);
        finish_rename_locked(
            &db,
            &kb,
            ("b.md", "a.docx"),
            WriteLook::Spelled,
            (plan, before),
            &mut (),
            |_| Err(anyhow::anyhow!("injected failure before the move")),
            |_| {
                Ok(indexer::SingleResult::Skipped {
                    reason: "not a docx",
                    frontmatter_unparsed: false,
                })
            },
        );
        assert_eq!(db.get_document_hash("a.docx").unwrap(), None);
        assert_eq!(
            db.all_document_paths().unwrap(),
            vec!["other.md".to_string()]
        );
    }

    /// (#544 C1) Clearing the destination fails after its exact row went (the folded-row sweep
    /// returning an error): the indexer's rename is not run, and the destination is reindexed
    /// inside the same locks as when the rename fails.
    #[test]
    fn an_overwrite_whose_clearing_fails_reindexes_the_destination() {
        let (kb, _cleanup) = i544_scratch_kb("groove-watcher-i544-overwrite-clear-fail");
        std::fs::write(kb.join("a.md"), "# b\n").unwrap();
        let db = overwrite_db();
        let plan = rename_plan(&db, "b.md", "a.md", OnDisk::Present).unwrap();
        assert!(indexer::deindex_single_file(&db, "a.md").unwrap());
        let before = Err(anyhow::anyhow!("injected failure of the folded-row sweep"));
        finish_rename_locked(
            &db,
            &kb,
            ("b.md", "a.md"),
            WriteLook::Spelled,
            (plan, before),
            &mut (),
            |_| panic!("the indexer's rename must not run after clearing failed"),
            |_| write_fresh_row(&db, "a.md"),
        );
        assert_eq!(
            db.all_document_paths().unwrap(),
            vec!["a.md".to_string(), "other.md".to_string()]
        );
        assert_eq!(
            db.get_document_hash("a.md").unwrap().as_deref(),
            Some("fresh")
        );
    }

    /// (#544 C1) A failure after the source's row was moved leaves the moved row where it is: no
    /// reindex runs, since the destination has a row.
    #[test]
    fn an_overwrite_that_fails_after_the_move_keeps_the_moved_row() {
        let (kb, _cleanup) = i544_scratch_kb("groove-watcher-i544-overwrite-fail-after");
        std::fs::write(kb.join("a.md"), "# b\n").unwrap();
        let db = overwrite_db();
        let plan = rename_plan(&db, "b.md", "a.md", OnDisk::Present).unwrap();
        let before = apply_rename_plan_before(&db, &kb, "b.md", "a.md", &plan);
        finish_rename_locked(
            &db,
            &kb,
            ("b.md", "a.md"),
            WriteLook::Spelled,
            (plan, before),
            &mut (),
            |_| {
                db.rename_document("b.md", "a.md", crate::db::FrontmatterTopic::Unread)?;
                Err(anyhow::anyhow!("injected failure after the move"))
            },
            |_| panic!("a destination with a row must not be reindexed"),
        );
        assert_eq!(
            db.all_document_paths().unwrap(),
            vec!["a.md".to_string(), "other.md".to_string()]
        );
        assert_eq!(db.get_document_hash("a.md").unwrap().as_deref(), Some("hb"));
    }

    /// (#544 C1) After the same failure, a source that is on disk again keeps its row (its own
    /// event follows), and a destination gone by then is not reindexed.
    #[test]
    fn an_overwrite_failure_keeps_a_source_that_is_back_and_skips_a_gone_destination() {
        let (kb, _cleanup) = i544_scratch_kb("groove-watcher-i544-overwrite-fail-back");
        std::fs::write(kb.join("a.md"), "# b\n").unwrap();
        std::fs::write(kb.join("b.md"), "# b again\n").unwrap();
        let db = overwrite_db();
        let plan = rename_plan(&db, "b.md", "a.md", OnDisk::Present).unwrap();
        let before = apply_rename_plan_before(&db, &kb, "b.md", "a.md", &plan);
        finish_rename_locked(
            &db,
            &kb,
            ("b.md", "a.md"),
            WriteLook::Spelled,
            (plan, before),
            &mut (),
            |_| Err(anyhow::anyhow!("injected failure before the move")),
            |_| write_fresh_row(&db, "a.md"),
        );
        assert_eq!(
            db.all_document_paths().unwrap(),
            vec![
                "a.md".to_string(),
                "b.md".to_string(),
                "other.md".to_string()
            ]
        );

        let (kb, _cleanup) = i544_scratch_kb("groove-watcher-i544-overwrite-fail-gone");
        let db = overwrite_db();
        let plan = rename_plan(&db, "b.md", "a.md", OnDisk::Present).unwrap();
        let before = apply_rename_plan_before(&db, &kb, "b.md", "a.md", &plan);
        finish_rename_locked(
            &db,
            &kb,
            ("b.md", "a.md"),
            WriteLook::Spelled,
            (plan, before),
            &mut (),
            |_| Err(anyhow::anyhow!("injected failure before the move")),
            |_| panic!("a destination gone by then must not be reindexed"),
        );
        assert_eq!(
            db.all_document_paths().unwrap(),
            vec!["other.md".to_string()]
        );
    }

    /// The rename a test hands [`finish_rename_locked`] for an indexer's rename whose reparse
    /// wrote nothing: the source's row moves to `new_rel` with its old content, as
    /// [`crate::indexer::rename_single_file`] leaves it.
    fn move_row_unreindexed(
        db: &Database,
        old_rel: &str,
        new_rel: &str,
    ) -> Result<indexer::RenameOutcome> {
        db.rename_document(old_rel, new_rel, crate::db::FrontmatterTopic::Unread)?;
        Ok(indexer::RenameOutcome::RenamedButNotReindexed)
    }

    /// The reindex a test hands [`finish_rename_locked`] for a file that holds nothing to
    /// embed: the row at `rel` goes, as an event's reindex of an empty file drops it.
    fn drop_row_nothing_to_index(db: &Database, rel: &str) -> Result<indexer::SingleResult> {
        let deindexed = indexer::deindex_single_file(db, rel)?;
        Ok(indexer::SingleResult::NothingToIndex {
            deindexed,
            frontmatter_unparsed: false,
        })
    }

    /// (#544 Review Focus 4, J2) An overwriting rename whose indexer's rename moved the source's
    /// row but whose reparse wrote nothing (the source was emptied): the destination is on
    /// disk, so it is read again inside the same locks, and that reindex decides the row. Here
    /// it finds nothing to embed, so no row is left at `a.md`.
    #[test]
    fn an_overwriting_rename_whose_reparse_wrote_nothing_reads_the_destination_again() {
        let (kb, _cleanup) = i544_scratch_kb("groove-watcher-i544-overwrite-unreindexed");
        std::fs::write(kb.join("a.md"), "").unwrap();
        let db = overwrite_db();
        let plan = rename_plan(&db, "b.md", "a.md", OnDisk::Present).unwrap();
        assert_eq!(plan, RenamePlan::ClearDestinationThenRename);
        let before = apply_rename_plan_before(&db, &kb, "b.md", "a.md", &plan);
        assert!(matches!(before, Ok(true)), "{before:?}");
        let mut reindexed = 0;
        finish_rename_locked(
            &db,
            &kb,
            ("b.md", "a.md"),
            WriteLook::Spelled,
            (plan, before),
            &mut reindexed,
            |_| move_row_unreindexed(&db, "b.md", "a.md"),
            |n| {
                *n += 1;
                drop_row_nothing_to_index(&db, "a.md")
            },
        );
        assert_eq!(reindexed, 1);
        assert_eq!(db.get_document_hash("a.md").unwrap(), None);
        assert_eq!(
            db.all_document_paths().unwrap(),
            vec!["other.md".to_string()]
        );
    }

    /// (#544 Review Focus 4) A rename that overwrote nothing, whose reparse wrote nothing (the
    /// shape an overwrite takes on Windows, after the destination's removal dropped its row):
    /// the destination is read again, and that reindex decides the row. Here it finds nothing
    /// to embed, so the source's old content is not left at `a.md`.
    #[test]
    fn a_plain_rename_whose_reparse_wrote_nothing_reads_the_destination_again() {
        let (kb, _cleanup) = i544_scratch_kb("groove-watcher-i544-plain-unreindexed");
        std::fs::write(kb.join("a.md"), "").unwrap();
        let db = dir_rename_db(&["other.md"]);
        db.upsert_document("b.md", None, None, None, None, &[], None, "hb", 0)
            .unwrap();
        let plan = rename_plan(&db, "b.md", "a.md", OnDisk::Present).unwrap();
        assert_eq!(plan, RenamePlan::Rename);
        let before = apply_rename_plan_before(&db, &kb, "b.md", "a.md", &plan);
        assert!(matches!(before, Ok(true)), "{before:?}");
        let mut reindexed = 0;
        finish_rename_locked(
            &db,
            &kb,
            ("b.md", "a.md"),
            WriteLook::Spelled,
            (plan, before),
            &mut reindexed,
            |_| move_row_unreindexed(&db, "b.md", "a.md"),
            |n| {
                *n += 1;
                drop_row_nothing_to_index(&db, "a.md")
            },
        );
        assert_eq!(reindexed, 1);
        assert_eq!(db.get_document_hash("a.md").unwrap(), None);
        assert_eq!(
            db.all_document_paths().unwrap(),
            vec!["other.md".to_string()]
        );
    }

    /// (#544, Task 5 review) A rename onto `a\b.md`, a name a version up to 1.12.0 stored folded
    /// as `a/b.md`, whose reparse wrote nothing: the second read of `a\b.md` finds nothing to
    /// embed and drops the real key, and the folded row goes with it, swept as a removal by that
    /// read's own result. The write form of the sweep would keep it, since the real key holds
    /// no row by then. Not on Windows, where `\` separates components and there is no folded row.
    #[cfg(not(windows))]
    #[test]
    fn a_rename_whose_second_read_found_nothing_sweeps_the_folded_row_as_a_removal() {
        let (kb, _cleanup) = i544_scratch_kb("groove-watcher-i544-unreindexed-folded");
        std::fs::write(kb.join("a\\b.md"), "").unwrap();
        let db = dir_rename_db(&["a/b.md", "other.md"]);
        db.upsert_document("c.md", None, None, None, None, &[], None, "hc", 0)
            .unwrap();
        let plan = rename_plan(&db, "c.md", "a\\b.md", OnDisk::Present).unwrap();
        assert_eq!(plan, RenamePlan::Rename);
        let before = apply_rename_plan_before(&db, &kb, "c.md", "a\\b.md", &plan);
        assert!(matches!(before, Ok(true)), "{before:?}");
        let mut reindexed = 0;
        finish_rename_locked(
            &db,
            &kb,
            ("c.md", "a\\b.md"),
            WriteLook::ExistsOnly,
            (plan, before),
            &mut reindexed,
            |_| move_row_unreindexed(&db, "c.md", "a\\b.md"),
            |n| {
                *n += 1;
                drop_row_nothing_to_index(&db, "a\\b.md")
            },
        );
        assert_eq!(reindexed, 1);
        assert_eq!(db.get_document_hash("a\\b.md").unwrap(), None);
        assert_eq!(db.get_document_hash("a/b.md").unwrap(), None);
        assert_eq!(
            db.all_document_paths().unwrap(),
            vec!["other.md".to_string()]
        );
    }

    /// (#544 J2, D3) The same overwriting rename, with the destination gone by the second
    /// look: it is not read again, and the moved row goes as for any unsettled rename whose
    /// destination is gone.
    #[test]
    fn an_overwriting_rename_whose_destination_is_gone_is_not_read_again() {
        let (kb, _cleanup) = i544_scratch_kb("groove-watcher-i544-overwrite-unreindexed-gone");
        let db = overwrite_db();
        let plan = rename_plan(&db, "b.md", "a.md", OnDisk::Present).unwrap();
        assert_eq!(plan, RenamePlan::ClearDestinationThenRename);
        let before = apply_rename_plan_before(&db, &kb, "b.md", "a.md", &plan);
        assert!(matches!(before, Ok(true)), "{before:?}");
        let mut reindexed = 0;
        finish_rename_locked(
            &db,
            &kb,
            ("b.md", "a.md"),
            WriteLook::Spelled,
            (plan, before),
            &mut reindexed,
            |_| move_row_unreindexed(&db, "b.md", "a.md"),
            |n| {
                *n += 1;
                write_fresh_row(&db, "a.md")
            },
        );
        assert_eq!(reindexed, 0);
        assert_eq!(
            db.all_document_paths().unwrap(),
            vec!["other.md".to_string()]
        );
    }

    /// (#544 J2) An overwriting rename that the indexer's rename settled (the content was the
    /// same, or the reparse wrote it): the destination is not read again.
    #[test]
    fn a_settled_overwriting_rename_is_not_read_again() {
        use indexer::RenameOutcome as R;
        for settled in [R::Renamed, R::RenamedAndReindexed { chunks: 1 }] {
            let label = format!("{settled:?}");
            let (kb, _cleanup) = i544_scratch_kb("groove-watcher-i544-overwrite-settled");
            std::fs::write(kb.join("a.md"), "# b\n").unwrap();
            let db = overwrite_db();
            let plan = rename_plan(&db, "b.md", "a.md", OnDisk::Present).unwrap();
            assert_eq!(plan, RenamePlan::ClearDestinationThenRename, "{label}");
            let before = apply_rename_plan_before(&db, &kb, "b.md", "a.md", &plan);
            let mut reindexed = 0;
            finish_rename_locked(
                &db,
                &kb,
                ("b.md", "a.md"),
                WriteLook::Spelled,
                (plan, before),
                &mut reindexed,
                |_| {
                    db.rename_document("b.md", "a.md", crate::db::FrontmatterTopic::Unread)?;
                    Ok(settled)
                },
                |n| {
                    *n += 1;
                    drop_row_nothing_to_index(&db, "a.md")
                },
            );
            assert_eq!(reindexed, 0, "{label}");
            assert_eq!(
                db.get_document_hash("a.md").unwrap().as_deref(),
                Some("hb"),
                "{label}"
            );
        }
    }

    /// The rename a test hands [`finish_rename_locked`] for a rename across a parser whose
    /// reparse found bytes other than the ones its first read hashed: the source's row moves to
    /// `new_rel` under [`indexer::HASH_AWAITING_REPARSE`], as
    /// [`crate::indexer::rename_single_file`] leaves it.
    fn move_row_awaiting(
        db: &Database,
        old_rel: &str,
        new_rel: &str,
    ) -> Result<indexer::RenameOutcome> {
        db.rename_document(old_rel, new_rel, crate::db::FrontmatterTopic::Unread)?;
        db.overwrite_content_hash(&[new_rel], indexer::HASH_AWAITING_REPARSE)?;
        Ok(indexer::RenameOutcome::RenamedAcrossParsersAwaitingReparse)
    }

    /// (#544, local Codex r1) A rename across a parser (`b.txt` to `a.md`) that kept its row
    /// awaiting a reparse, with the file still at `a.md` on the second look: `a.md` is read
    /// again inside the same locks, once, and that read alone decides the row. Here it writes
    /// the row; when it too cannot settle the bytes, the row stays awaiting.
    #[test]
    fn an_awaiting_rename_whose_destination_is_present_reads_it_again() {
        for writes in [true, false] {
            let (kb, _cleanup) = i544_scratch_kb("groove-watcher-i544-awaiting-present");
            std::fs::write(kb.join("a.md"), "# a\n").unwrap();
            let db = dir_rename_db(&["other.md"]);
            db.upsert_document("b.txt", None, None, None, None, &[], None, "hb", 0)
                .unwrap();
            let plan = rename_plan(&db, "b.txt", "a.md", OnDisk::Present).unwrap();
            assert_eq!(plan, RenamePlan::Rename, "writes: {writes}");
            let before = apply_rename_plan_before(&db, &kb, "b.txt", "a.md", &plan);
            assert!(matches!(before, Ok(true)), "{before:?}");
            let mut reindexed = 0;
            finish_rename_locked(
                &db,
                &kb,
                ("b.txt", "a.md"),
                WriteLook::Spelled,
                (plan, before),
                &mut reindexed,
                |_| move_row_awaiting(&db, "b.txt", "a.md"),
                |n| {
                    *n += 1;
                    if writes {
                        write_fresh_row(&db, "a.md")
                    } else {
                        Ok(indexer::SingleResult::Skipped {
                            reason: "changed between scan and read",
                            frontmatter_unparsed: false,
                        })
                    }
                },
            );
            assert_eq!(reindexed, 1, "writes: {writes}");
            let want = if writes {
                "fresh"
            } else {
                indexer::HASH_AWAITING_REPARSE
            };
            assert_eq!(
                db.get_document_hash("a.md").unwrap().as_deref(),
                Some(want),
                "writes: {writes}"
            );
            assert_eq!(db.get_document_hash("b.txt").unwrap(), None);
        }
    }

    /// (#544, local Codex r3, J3) The same rename across a parser, whose further read verified
    /// the bytes on disk and could not index them under the new parser: the row goes, as the
    /// rename's first read would have dropped it. Only a read whose bytes changed again keeps
    /// the row awaiting a reparse.
    #[test]
    fn an_awaiting_rename_whose_further_read_cannot_index_the_bytes_drops_the_row() {
        let skipped = |reason| indexer::SingleResult::Skipped {
            reason,
            frontmatter_unparsed: false,
        };
        let cases = [
            (skipped("parse failed"), None),
            (skipped("no embeddable chunks"), None),
            (skipped("embedding endpoint rejected the input"), None),
            (indexer::SingleResult::Refused, None),
            (
                skipped("changed between scan and read"),
                Some(indexer::HASH_AWAITING_REPARSE),
            ),
        ];
        for (read, want) in cases {
            let label = format!("{read:?}");
            let (kb, _cleanup) = i544_scratch_kb("groove-watcher-i544-awaiting-unindexable");
            std::fs::write(kb.join("a.md"), "# a\n").unwrap();
            let db = dir_rename_db(&["other.md"]);
            db.upsert_document("b.txt", None, None, None, None, &[], None, "hb", 0)
                .unwrap();
            let plan = rename_plan(&db, "b.txt", "a.md", OnDisk::Present).unwrap();
            let before = apply_rename_plan_before(&db, &kb, "b.txt", "a.md", &plan);
            let mut reindexed = 0;
            finish_rename_locked(
                &db,
                &kb,
                ("b.txt", "a.md"),
                WriteLook::Spelled,
                (plan, before),
                &mut reindexed,
                |_| move_row_awaiting(&db, "b.txt", "a.md"),
                |n| {
                    *n += 1;
                    Ok(read)
                },
            );
            assert_eq!(reindexed, 1, "{label}");
            assert_eq!(
                db.get_document_hash("a.md").unwrap().as_deref(),
                want,
                "{label}"
            );
            assert_eq!(db.get_document_hash("b.txt").unwrap(), None, "{label}");
        }
    }

    /// The reindex a test hands [`finish_rename_locked_checked`] for bytes that change between
    /// the read's two looks at them: a read asked to check its bytes before parsing sees the
    /// change, and one not asked parses the changed bytes and fails. Counts the reads in `n` and
    /// records the flag it was given.
    fn read_bytes_changing_under_the_parse(
        (n, flag): &mut (usize, Option<bool>),
        checked: bool,
    ) -> Result<indexer::SingleResult> {
        *n += 1;
        *flag = Some(checked);
        Ok(indexer::SingleResult::Skipped {
            reason: if checked {
                "changed between scan and read"
            } else {
                "parse failed"
            },
            frontmatter_unparsed: false,
        })
    }

    /// (#544, local Codex r3, D5) The further read of a row a rename across a parser kept
    /// awaiting (`b.txt` to `a.md`) is asked to check its bytes before parsing, so bytes that
    /// change under it keep the row awaiting rather than drop it on a parse of bytes nobody
    /// checked.
    #[test]
    fn an_awaiting_rename_checks_its_further_read_before_parsing() {
        let (kb, _cleanup) = i544_scratch_kb("groove-watcher-i544-awaiting-checked");
        std::fs::write(kb.join("a.md"), "# a\n").unwrap();
        let db = dir_rename_db(&["other.md"]);
        db.upsert_document("b.txt", None, None, None, None, &[], None, "hb", 0)
            .unwrap();
        let plan = rename_plan(&db, "b.txt", "a.md", OnDisk::Present).unwrap();
        let before = apply_rename_plan_before(&db, &kb, "b.txt", "a.md", &plan);
        let mut seen = (0, None);
        finish_rename_locked_checked(
            &db,
            &kb,
            ("b.txt", "a.md"),
            WriteLook::Spelled,
            (plan, before),
            &mut seen,
            |_| move_row_awaiting(&db, "b.txt", "a.md"),
            read_bytes_changing_under_the_parse,
        );
        assert_eq!(seen, (1, Some(true)));
        assert_eq!(
            db.get_document_hash("a.md").unwrap().as_deref(),
            Some(indexer::HASH_AWAITING_REPARSE),
            "the row went on bytes its read did not check"
        );
    }

    /// (#544, local Codex r3, D5) The same for a row moved unread across a parser (`a.md` to
    /// `a.txt`, the look at `a.txt` failed).
    #[test]
    fn an_unread_move_across_a_parser_checks_its_read_before_parsing() {
        let (kb, _cleanup) = i544_scratch_kb("groove-watcher-i544-unread-move-checked");
        let reg = Registry::from_enabled(&["md".into(), "txt".into()]).unwrap();
        let denied = std::io::ErrorKind::PermissionDenied;
        let db = dir_rename_db(&["a.md", "other.md"]);
        let plan = rename_plan(&db, "a.md", "a.txt", OnDisk::Unknown(denied)).unwrap();
        let before = apply_rename_plan_before(&db, &kb, "a.md", "a.txt", &plan);
        let start = rename_start(&db, &reg, ("a.md", "a.txt"), plan, before);
        let mut seen = (0, None);
        finish_rename_locked_checked(
            &db,
            &kb,
            ("a.md", "a.txt"),
            WriteLook::Spelled,
            start,
            &mut seen,
            |_| panic!("an unread move does not run the indexer's rename"),
            read_bytes_changing_under_the_parse,
        );
        assert_eq!(seen, (1, Some(true)));
        assert_eq!(
            db.get_document_hash("a.txt").unwrap().as_deref(),
            Some(indexer::HASH_AWAITING_REPARSE),
            "the row went on bytes its read did not check"
        );
    }

    /// (#544, local Codex r5) Which results of the further read of a row a rename across a
    /// parser kept awaiting (`b.txt` to `a.md`) drop it: a destination the read found gone (D1)
    /// and one it measured over the size cap (J3) do; a skip whose reason the settlement does
    /// not name, and an unchanged answer, keep it awaiting.
    #[test]
    fn a_crossed_further_read_drops_the_row_only_on_a_named_outcome() {
        let skipped = |reason| indexer::SingleResult::Skipped {
            reason,
            frontmatter_unparsed: false,
        };
        let awaiting = Some(indexer::HASH_AWAITING_REPARSE);
        let cases = [
            (skipped("file no longer exists"), None),
            (skipped("file too large"), None),
            (skipped("a reason nobody named"), awaiting),
            (indexer::SingleResult::Unchanged, awaiting),
        ];
        for (read, want) in cases {
            let label = format!("{read:?}");
            let (kb, _cleanup) = i544_scratch_kb("groove-watcher-i544-awaiting-named");
            std::fs::write(kb.join("a.md"), "# a\n").unwrap();
            let db = dir_rename_db(&["other.md"]);
            db.upsert_document("b.txt", None, None, None, None, &[], None, "hb", 0)
                .unwrap();
            let plan = rename_plan(&db, "b.txt", "a.md", OnDisk::Present).unwrap();
            let before = apply_rename_plan_before(&db, &kb, "b.txt", "a.md", &plan);
            let mut reindexed = 0;
            finish_rename_locked_checked(
                &db,
                &kb,
                ("b.txt", "a.md"),
                WriteLook::Spelled,
                (plan, before),
                &mut reindexed,
                |_| move_row_awaiting(&db, "b.txt", "a.md"),
                |n, _checked| {
                    *n += 1;
                    Ok(read)
                },
            );
            assert_eq!(reindexed, 1, "{label}");
            assert_eq!(
                db.get_document_hash("a.md").unwrap().as_deref(),
                want,
                "{label}"
            );
        }
    }

    /// (#544, local Codex r4, D5) The further read of a row a rename across a parser left with
    /// the old parser's chunks fails inside the indexer (a "read failed" skip): no bytes were
    /// checked, so the row stays awaiting, for the row kept awaiting and for one moved unread.
    #[test]
    fn a_crossed_further_read_that_fails_keeps_the_row_awaiting() {
        let read_failed = |n: &mut usize, _checked: bool| {
            *n += 1;
            Ok(indexer::SingleResult::Skipped {
                reason: "read failed",
                frontmatter_unparsed: false,
            })
        };

        let (kb, _cleanup) = i544_scratch_kb("groove-watcher-i544-awaiting-read-failed");
        std::fs::write(kb.join("a.md"), "# a\n").unwrap();
        let db = dir_rename_db(&["other.md"]);
        db.upsert_document("b.txt", None, None, None, None, &[], None, "hb", 0)
            .unwrap();
        let plan = rename_plan(&db, "b.txt", "a.md", OnDisk::Present).unwrap();
        let before = apply_rename_plan_before(&db, &kb, "b.txt", "a.md", &plan);
        let mut reindexed = 0;
        finish_rename_locked_checked(
            &db,
            &kb,
            ("b.txt", "a.md"),
            WriteLook::Spelled,
            (plan, before),
            &mut reindexed,
            |_| move_row_awaiting(&db, "b.txt", "a.md"),
            read_failed,
        );
        assert_eq!(reindexed, 1);
        assert_eq!(
            db.get_document_hash("a.md").unwrap().as_deref(),
            Some(indexer::HASH_AWAITING_REPARSE),
            "kept awaiting: a read that checked no bytes dropped the row"
        );

        let (kb, _cleanup) = i544_scratch_kb("groove-watcher-i544-unread-move-read-failed");
        let reg = Registry::from_enabled(&["md".into(), "txt".into()]).unwrap();
        let denied = std::io::ErrorKind::PermissionDenied;
        let db = dir_rename_db(&["a.md", "other.md"]);
        let plan = rename_plan(&db, "a.md", "a.txt", OnDisk::Unknown(denied)).unwrap();
        let before = apply_rename_plan_before(&db, &kb, "a.md", "a.txt", &plan);
        let start = rename_start(&db, &reg, ("a.md", "a.txt"), plan, before);
        let mut reindexed = 0;
        finish_rename_locked_checked(
            &db,
            &kb,
            ("a.md", "a.txt"),
            WriteLook::Spelled,
            start,
            &mut reindexed,
            |_| panic!("an unread move does not run the indexer's rename"),
            read_failed,
        );
        assert_eq!(reindexed, 1);
        assert_eq!(
            db.get_document_hash("a.txt").unwrap().as_deref(),
            Some(indexer::HASH_AWAITING_REPARSE),
            "moved unread: a read that checked no bytes dropped the row"
        );
    }

    /// (#544, local Codex r3, J2) A rename within one parser whose reparse wrote nothing reads
    /// the destination again without the check, and its parse failure keeps the row.
    #[test]
    fn a_same_parser_rename_reads_the_destination_again_unchecked() {
        let (kb, _cleanup) = i544_scratch_kb("groove-watcher-i544-same-parser-unchecked");
        std::fs::write(kb.join("a.md"), "# a\n").unwrap();
        let db = dir_rename_db(&["other.md"]);
        db.upsert_document("b.md", None, None, None, None, &[], None, "hb", 0)
            .unwrap();
        let plan = rename_plan(&db, "b.md", "a.md", OnDisk::Present).unwrap();
        let before = apply_rename_plan_before(&db, &kb, "b.md", "a.md", &plan);
        let mut seen = (0, None);
        finish_rename_locked_checked(
            &db,
            &kb,
            ("b.md", "a.md"),
            WriteLook::Spelled,
            (plan, before),
            &mut seen,
            |_| move_row_unreindexed(&db, "b.md", "a.md"),
            read_bytes_changing_under_the_parse,
        );
        assert_eq!(seen, (1, Some(false)));
        assert_eq!(db.get_document_hash("a.md").unwrap().as_deref(), Some("hb"));
    }

    /// (#544, local Codex r3, J2) A rename within one parser whose reparse wrote nothing, and
    /// whose further read fails to parse the bytes: the row stays, as for a file edited in
    /// place. Only a rename across a parser drops it.
    #[test]
    fn a_same_parser_rename_whose_further_read_fails_to_parse_keeps_the_row() {
        let (kb, _cleanup) = i544_scratch_kb("groove-watcher-i544-same-parser-unparsed");
        std::fs::write(kb.join("a.md"), "# a\n").unwrap();
        let db = dir_rename_db(&["other.md"]);
        db.upsert_document("b.md", None, None, None, None, &[], None, "hb", 0)
            .unwrap();
        let plan = rename_plan(&db, "b.md", "a.md", OnDisk::Present).unwrap();
        let before = apply_rename_plan_before(&db, &kb, "b.md", "a.md", &plan);
        let mut reindexed = 0;
        finish_rename_locked(
            &db,
            &kb,
            ("b.md", "a.md"),
            WriteLook::Spelled,
            (plan, before),
            &mut reindexed,
            |_| move_row_unreindexed(&db, "b.md", "a.md"),
            |n| {
                *n += 1;
                Ok(indexer::SingleResult::Skipped {
                    reason: "parse failed",
                    frontmatter_unparsed: false,
                })
            },
        );
        assert_eq!(reindexed, 1);
        assert_eq!(db.get_document_hash("a.md").unwrap().as_deref(), Some("hb"));
    }

    /// (#544, local Codex r1) The same rename with the destination gone by the second look: the
    /// row goes, and the destination is not read.
    #[test]
    fn an_awaiting_rename_whose_destination_is_gone_drops_the_row_unread() {
        let (kb, _cleanup) = i544_scratch_kb("groove-watcher-i544-awaiting-gone");
        let db = dir_rename_db(&["other.md"]);
        db.upsert_document("b.txt", None, None, None, None, &[], None, "hb", 0)
            .unwrap();
        let plan = rename_plan(&db, "b.txt", "a.md", OnDisk::Present).unwrap();
        let before = apply_rename_plan_before(&db, &kb, "b.txt", "a.md", &plan);
        let mut reindexed = 0;
        finish_rename_locked(
            &db,
            &kb,
            ("b.txt", "a.md"),
            WriteLook::Spelled,
            (plan, before),
            &mut reindexed,
            |_| move_row_awaiting(&db, "b.txt", "a.md"),
            |n| {
                *n += 1;
                write_fresh_row(&db, "a.md")
            },
        );
        assert_eq!(reindexed, 0);
        assert_eq!(
            db.all_document_paths().unwrap(),
            vec!["other.md".to_string()]
        );
    }

    /// (#552) The rename a test hands [`finish_rename_locked_checked`] for an indexer's rename
    /// across a parser that failed after it moved the row: the row moves to `new_rel` under
    /// [`indexer::HASH_AWAITING_REPARSE`], as [`crate::indexer::rename_single_file`] marks it
    /// before the move, and the rename returns an error.
    fn move_row_awaiting_then_fail(
        db: &Database,
        old_rel: &str,
        new_rel: &str,
    ) -> Result<indexer::RenameOutcome> {
        db.overwrite_content_hash(&[old_rel], indexer::HASH_AWAITING_REPARSE)?;
        db.rename_document(old_rel, new_rel, crate::db::FrontmatterTopic::Unread)?;
        Err(anyhow::anyhow!("injected failure after the move"))
    }

    /// (#552) The reindex a test hands [`finish_rename_locked_checked`]: counts the reads in
    /// `n`, records the flag it was given, and writes a row at `a.md` with hash `fresh`.
    fn count_and_write_fresh(
        db: &Database,
        (n, flag): &mut (usize, Option<bool>),
        checked: bool,
    ) -> Result<indexer::SingleResult> {
        *n += 1;
        *flag = Some(checked);
        write_fresh_row(db, "a.md")
    }

    /// (#552 D1) A rename across a parser (`b.txt` to `a.md`) whose indexer's rename failed after
    /// it moved the row, marked awaiting a reparse: `a.md` is read again inside the same locks,
    /// once, checked before parsing, and that read rewrites the row.
    #[test]
    fn i552_a_crossed_rename_that_failed_after_moving_is_read_again_checked() {
        let (kb, _cleanup) = i544_scratch_kb("groove-watcher-i552-crossed-err");
        std::fs::write(kb.join("a.md"), "# a\n").unwrap();
        let reg = Registry::from_enabled(&["md".into(), "txt".into()]).unwrap();
        let db = dir_rename_db(&["other.md"]);
        db.upsert_document("b.txt", None, None, None, None, &[], None, "hb", 0)
            .unwrap();
        let plan = rename_plan(&db, "b.txt", "a.md", OnDisk::Present).unwrap();
        assert_eq!(plan, RenamePlan::Rename);
        let before = apply_rename_plan_before(&db, &kb, "b.txt", "a.md", &plan);
        assert!(matches!(before, Ok(true)), "{before:?}");
        let start = rename_start(&db, &reg, ("b.txt", "a.md"), plan, before);
        assert!(start.crosses_a_parser, "b.txt to a.md crosses a parser");
        let mut seen = (0, None);
        finish_rename_locked_checked(
            &db,
            &kb,
            ("b.txt", "a.md"),
            WriteLook::Spelled,
            start,
            &mut seen,
            |_| move_row_awaiting_then_fail(&db, "b.txt", "a.md"),
            |seen, checked| count_and_write_fresh(&db, seen, checked),
        );
        assert_eq!(seen, (1, Some(true)));
        assert_eq!(
            db.get_document_hash("a.md").unwrap().as_deref(),
            Some("fresh")
        );
        assert_eq!(db.get_document_hash("b.txt").unwrap(), None);
    }

    /// (#552 D2) The same failure for an overwriting rename across a parser
    /// ([`RenamePlan::ClearDestinationThenRename`]): the moved row at `a.md` is not left in place
    /// by the settlement of a failed overwrite ([`settle_cleared_rename`]), which would take the
    /// reindex and do nothing; it is read again, once, checked before parsing.
    #[test]
    fn i552_an_overwriting_crossed_rename_that_failed_after_moving_is_read_again() {
        let (kb, _cleanup) = i544_scratch_kb("groove-watcher-i552-crossed-overwrite-err");
        std::fs::write(kb.join("a.md"), "# a\n").unwrap();
        let reg = Registry::from_enabled(&["md".into(), "txt".into()]).unwrap();
        let db = dir_rename_db(&["a.md", "other.md"]);
        db.upsert_document("b.txt", None, None, None, None, &[], None, "hb", 0)
            .unwrap();
        let plan = rename_plan(&db, "b.txt", "a.md", OnDisk::Present).unwrap();
        assert_eq!(plan, RenamePlan::ClearDestinationThenRename);
        let before = apply_rename_plan_before(&db, &kb, "b.txt", "a.md", &plan);
        assert!(matches!(before, Ok(true)), "{before:?}");
        let start = rename_start(&db, &reg, ("b.txt", "a.md"), plan, before);
        assert!(start.crosses_a_parser, "b.txt to a.md crosses a parser");
        let mut seen = (0, None);
        finish_rename_locked_checked(
            &db,
            &kb,
            ("b.txt", "a.md"),
            WriteLook::Spelled,
            start,
            &mut seen,
            |_| move_row_awaiting_then_fail(&db, "b.txt", "a.md"),
            |seen, checked| count_and_write_fresh(&db, seen, checked),
        );
        assert_eq!(seen, (1, Some(true)));
        assert_eq!(
            db.get_document_hash("a.md").unwrap().as_deref(),
            Some("fresh")
        );
        assert_eq!(db.get_document_hash("b.txt").unwrap(), None);
    }

    /// (#552) A rename across a parser whose indexer's rename failed before it moved the row:
    /// no row is at `a.md`, so nothing there is read again, and the source's row stays as it was.
    #[test]
    fn i552_a_rename_that_failed_before_moving_is_not_read_again() {
        let (kb, _cleanup) = i544_scratch_kb("groove-watcher-i552-crossed-err-before");
        std::fs::write(kb.join("a.md"), "# a\n").unwrap();
        let reg = Registry::from_enabled(&["md".into(), "txt".into()]).unwrap();
        let db = dir_rename_db(&["other.md"]);
        db.upsert_document("b.txt", None, None, None, None, &[], None, "hb", 0)
            .unwrap();
        let plan = rename_plan(&db, "b.txt", "a.md", OnDisk::Present).unwrap();
        let before = apply_rename_plan_before(&db, &kb, "b.txt", "a.md", &plan);
        let start = rename_start(&db, &reg, ("b.txt", "a.md"), plan, before);
        assert!(start.crosses_a_parser, "b.txt to a.md crosses a parser");
        let mut seen = (0, None);
        finish_rename_locked_checked(
            &db,
            &kb,
            ("b.txt", "a.md"),
            WriteLook::Spelled,
            start,
            &mut seen,
            |_| Err(anyhow::anyhow!("injected failure before the move")),
            |seen, checked| count_and_write_fresh(&db, seen, checked),
        );
        assert_eq!(seen, (0, None));
        assert_eq!(db.get_document_hash("a.md").unwrap(), None);
        assert_eq!(
            db.get_document_hash("b.txt").unwrap().as_deref(),
            Some("hb")
        );
    }

    /// (#552) The same failure after the move, for a start that does not cross a parser (a
    /// `(plan, before)` pair): the failed rename is not read again, and the row stays awaiting
    /// for the next event or run.
    #[test]
    fn i552_a_failed_rename_not_crossing_a_parser_is_not_read_again() {
        let (kb, _cleanup) = i544_scratch_kb("groove-watcher-i552-uncrossed-err");
        std::fs::write(kb.join("a.md"), "# a\n").unwrap();
        let db = dir_rename_db(&["other.md"]);
        db.upsert_document("b.txt", None, None, None, None, &[], None, "hb", 0)
            .unwrap();
        let plan = rename_plan(&db, "b.txt", "a.md", OnDisk::Present).unwrap();
        let before = apply_rename_plan_before(&db, &kb, "b.txt", "a.md", &plan);
        let start = DispatchStart::from((plan, before));
        assert!(!start.crosses_a_parser);
        let mut seen = (0, None);
        finish_rename_locked_checked(
            &db,
            &kb,
            ("b.txt", "a.md"),
            WriteLook::Spelled,
            start,
            &mut seen,
            |_| move_row_awaiting_then_fail(&db, "b.txt", "a.md"),
            |seen, checked| count_and_write_fresh(&db, seen, checked),
        );
        assert_eq!(seen, (0, None));
        assert_eq!(
            db.get_document_hash("a.md").unwrap().as_deref(),
            Some(indexer::HASH_AWAITING_REPARSE)
        );
    }

    /// (#552) The same failure after the move across a parser, with `a.md` gone by the second
    /// look: the moved row goes, and `a.md` is not read.
    #[test]
    fn i552_a_crossed_rename_that_failed_with_the_destination_gone_drops_the_row_unread() {
        let (kb, _cleanup) = i544_scratch_kb("groove-watcher-i552-crossed-err-gone");
        let reg = Registry::from_enabled(&["md".into(), "txt".into()]).unwrap();
        let db = dir_rename_db(&["other.md"]);
        db.upsert_document("b.txt", None, None, None, None, &[], None, "hb", 0)
            .unwrap();
        let plan = rename_plan(&db, "b.txt", "a.md", OnDisk::Present).unwrap();
        let before = apply_rename_plan_before(&db, &kb, "b.txt", "a.md", &plan);
        let start = rename_start(&db, &reg, ("b.txt", "a.md"), plan, before);
        assert!(start.crosses_a_parser, "b.txt to a.md crosses a parser");
        let mut seen = (0, None);
        finish_rename_locked_checked(
            &db,
            &kb,
            ("b.txt", "a.md"),
            WriteLook::Spelled,
            start,
            &mut seen,
            |_| move_row_awaiting_then_fail(&db, "b.txt", "a.md"),
            |seen, checked| count_and_write_fresh(&db, seen, checked),
        );
        assert_eq!(seen, (0, None));
        assert_eq!(
            db.all_document_paths().unwrap(),
            vec!["other.md".to_string()]
        );
    }

    /// (#544, local Codex r1) Which second looks read the destination of an unsettled rename
    /// again: a reparse that wrote nothing, on a destination that is there; a row kept awaiting
    /// a reparse, on a destination that is there or that the look could not tell (one attempt,
    /// whose failure leaves the mark). A gone destination is never read, and an outcome that
    /// kept no row reads nothing.
    #[test]
    fn the_second_look_reads_the_destination_again_for_the_kept_rows_only() {
        use indexer::RenameOutcome as R;
        let denied = OnDisk::Unknown(std::io::ErrorKind::PermissionDenied);
        let cases: [(Result<R>, OnDisk, bool); 9] = [
            (Ok(R::RenamedButNotReindexed), OnDisk::Present, true),
            (Ok(R::RenamedButNotReindexed), denied, false),
            (Ok(R::RenamedButNotReindexed), OnDisk::Gone, false),
            (
                Ok(R::RenamedAcrossParsersAwaitingReparse),
                OnDisk::Present,
                true,
            ),
            (Ok(R::RenamedAcrossParsersAwaitingReparse), denied, true),
            (
                Ok(R::RenamedAcrossParsersAwaitingReparse),
                OnDisk::Gone,
                false,
            ),
            (Ok(R::RenamedButRefused), OnDisk::Present, false),
            (Ok(R::OldPathMissingAndRefused), denied, false),
            (Err(anyhow::anyhow!("x")), OnDisk::Present, false),
        ];
        for (outcome, on_disk, want) in cases {
            let label = format!("{outcome:?} {on_disk:?}");
            assert_eq!(
                reads_destination_again(kept_row(&outcome), on_disk),
                want,
                "{label}"
            );
        }
    }

    /// (#544, local Codex r1) A row moved unread across a parser (`a.md` to `a.txt`, the look at
    /// `a.txt` failed), whose hash [`await_reparse_after_unread_move`] overwrote: `a.txt` is read
    /// once inside the same locks, and that read decides the row.
    #[test]
    fn an_unread_move_across_a_parser_reads_the_destination_once() {
        let (kb, _cleanup) = i544_scratch_kb("groove-watcher-i544-unread-move-read");
        let reg = Registry::from_enabled(&["md".into(), "txt".into()]).unwrap();
        let denied = std::io::ErrorKind::PermissionDenied;
        let db = dir_rename_db(&["a.md", "other.md"]);
        let plan = rename_plan(&db, "a.md", "a.txt", OnDisk::Unknown(denied)).unwrap();
        assert_eq!(plan, RenamePlan::MoveRowUnknown(denied));
        let before = apply_rename_plan_before(&db, &kb, "a.md", "a.txt", &plan);
        let awaits =
            await_reparse_after_unread_move(&db, &reg, "a.md", "a.txt", &plan, &before).unwrap();
        assert!(awaits);
        let mut reindexed = 0;
        finish_rename_locked(
            &db,
            &kb,
            ("a.md", "a.txt"),
            WriteLook::Spelled,
            RenameStart {
                plan,
                before,
                unread_move_awaits: awaits,
            },
            &mut reindexed,
            |_| panic!("an unread move does not run the indexer's rename"),
            |n| {
                *n += 1;
                write_fresh_row(&db, "a.txt")
            },
        );
        assert_eq!(reindexed, 1);
        assert_eq!(
            db.get_document_hash("a.txt").unwrap().as_deref(),
            Some("fresh")
        );
        assert_eq!(db.get_document_hash("a.md").unwrap(), None);
    }

    /// (#544, local Codex r1) A row moved unread within one parser (`a.md` to `b.md`) keeps its
    /// own parser's chunks under its own hash: nothing marked it, so nothing reads it again.
    #[test]
    fn an_unread_move_within_one_parser_is_not_read_again() {
        let (kb, _cleanup) = i544_scratch_kb("groove-watcher-i544-unread-move-same");
        let reg = Registry::from_enabled(&["md".into(), "txt".into()]).unwrap();
        let denied = std::io::ErrorKind::PermissionDenied;
        let db = dir_rename_db(&["a.md", "other.md"]);
        let plan = rename_plan(&db, "a.md", "b.md", OnDisk::Unknown(denied)).unwrap();
        let before = apply_rename_plan_before(&db, &kb, "a.md", "b.md", &plan);
        let awaits =
            await_reparse_after_unread_move(&db, &reg, "a.md", "b.md", &plan, &before).unwrap();
        assert!(!awaits);
        finish_rename_locked(
            &db,
            &kb,
            ("a.md", "b.md"),
            WriteLook::Spelled,
            RenameStart {
                plan,
                before,
                unread_move_awaits: awaits,
            },
            &mut (),
            |_| panic!("an unread move does not run the indexer's rename"),
            |_| panic!("a row moved within one parser must not be read again"),
        );
        assert_eq!(db.get_document_hash("b.md").unwrap().as_deref(), Some("h"));
    }

    /// (#544, local Codex r2) A row moved unread across a parser (`a.md` to `a.txt`) whose
    /// awaiting hash could not be written (a trigger refuses it while `fault.armed` is 1): the
    /// destination is read once all the same. (local Codex r3, J3) A read that verified bytes
    /// the new parser cannot index drops the row, fault or not; one whose bytes changed again,
    /// or that verified nothing, is followed by a second attempt at the hash. Each case says
    /// whether the read lifts the fault, what the read returns, and the hash `a.txt` ends with.
    #[test]
    fn an_unread_move_across_a_parser_is_read_again_when_its_mark_fails_and_dropped_when_unindexable()
     {
        type Read = fn(&Database) -> Result<indexer::SingleResult>;
        let changed: Read = |_| {
            Ok(indexer::SingleResult::Skipped {
                reason: "changed between scan and read",
                frontmatter_unparsed: false,
            })
        };
        let unparsed: Read = |_| {
            Ok(indexer::SingleResult::Skipped {
                reason: "parse failed",
                frontmatter_unparsed: false,
            })
        };
        let unchanged: Read = |_| Ok(indexer::SingleResult::Unchanged);
        let failed: Read = |_| Err(anyhow::anyhow!("injected failure of the read"));
        let fresh: Read = |db| write_fresh_row(db, "a.txt");
        let awaiting = Some(indexer::HASH_AWAITING_REPARSE);
        let cases: [(&str, bool, Read, Option<&str>); 8] = [
            ("changed again, mark retried", true, changed, awaiting),
            ("unchanged, mark retried", true, unchanged, awaiting),
            ("read failed, mark retried", true, failed, awaiting),
            ("changed again, mark fails again", false, changed, Some("h")),
            ("cannot parse, dropped", true, unparsed, None),
            ("cannot parse, fault, dropped", false, unparsed, None),
            ("rewritten, no mark over it", true, fresh, Some("fresh")),
            ("rewritten under the fault", false, fresh, Some("fresh")),
        ];
        let reg = Registry::from_enabled(&["md".into(), "txt".into()]).unwrap();
        let denied = std::io::ErrorKind::PermissionDenied;
        for (label, lift, read, want) in cases {
            let (kb, _cleanup) = i544_scratch_kb("groove-watcher-i544-unread-move-mark-fails");
            let db = dir_rename_db(&["a.md", "other.md"]);
            db.execute_for_test(concat!(
                "CREATE TABLE fault (armed INTEGER NOT NULL); INSERT INTO fault VALUES (1); ",
                "CREATE TRIGGER inject_mark_failure BEFORE UPDATE OF content_hash ON documents ",
                "WHEN NEW.content_hash = 'awaiting-reparse' AND (SELECT armed FROM fault) = 1 ",
                "BEGIN SELECT RAISE(ABORT, 'injected mark failure'); END;"
            ))
            .unwrap();
            let plan = rename_plan(&db, "a.md", "a.txt", OnDisk::Unknown(denied)).unwrap();
            assert_eq!(plan, RenamePlan::MoveRowUnknown(denied), "{label}");
            let before = apply_rename_plan_before(&db, &kb, "a.md", "a.txt", &plan);
            let start = rename_start(&db, &reg, ("a.md", "a.txt"), plan, before);
            assert_eq!(
                db.get_document_hash("a.txt").unwrap().as_deref(),
                Some("h"),
                "{label}: the injected fault did not stop the first mark"
            );
            let mut reindexed = 0;
            finish_rename_locked(
                &db,
                &kb,
                ("a.md", "a.txt"),
                WriteLook::Spelled,
                start,
                &mut reindexed,
                |_| panic!("an unread move does not run the indexer's rename"),
                |n| {
                    *n += 1;
                    if lift {
                        db.execute_for_test("UPDATE fault SET armed = 0").unwrap();
                    }
                    read(&db)
                },
            );
            assert_eq!(reindexed, 1, "{label}: the destination was not read once");
            assert_eq!(
                db.get_document_hash("a.txt").unwrap().as_deref(),
                want,
                "{label}"
            );
            assert_eq!(db.get_document_hash("a.md").unwrap(), None, "{label}");
        }
    }

    /// The directory listings [`spelled_on_disk`] has made on this thread so far.
    fn listings() -> usize {
        LISTINGS.with(std::cell::Cell::get)
    }

    /// (#544 A2) A directory that arrived where an indexed file was: the file's row under the
    /// directory's name goes, and the directory is scanned.
    #[test]
    fn an_arrived_directory_drops_the_row_of_the_file_it_replaced() {
        let (kb, _cleanup) = i544_scratch_kb("groove-watcher-i544-arrived");
        std::fs::create_dir_all(kb.join("a.md")).unwrap();
        let db = dir_rename_db(&["a.md", "other.md"]);
        let got = act_on_arrived_directory(&db, &kb, "a.md", &kb.join("a.md"));
        assert!(matches!(got, ArrivedDirectory::Scan(Ok(true))), "{got:?}");
        assert_eq!(
            db.all_document_paths().unwrap(),
            vec!["other.md".to_string()]
        );
    }

    /// (#544 P1-S2) A directory that arrived under a spelling the disk holds only in another
    /// case: it is not scanned, and the rows under that spelling go, the directory's own and
    /// every one under it, while the rows under the spelling on disk stay.
    ///
    /// The look is handed in: on a case-sensitive OS `Docs` is not found when only `docs` is
    /// there, so the event never reaches the directory branch of the reindex arm (the extension
    /// filter of [`should_process`] drops it). This branch comes from the file system only on a
    /// case-insensitive volume.
    #[test]
    fn an_arrived_directory_held_only_in_another_case_drops_its_prefix() {
        let (kb, _cleanup) = i544_scratch_kb("groove-watcher-i544-arrived-case");
        let db = dir_rename_db(&["Docs", "Docs/a.md", "docs/b.md"]);
        let got = act_on_arrived_directory_for(&db, &kb, "Docs", Some(true), OnDisk::Gone, false);
        let ArrivedDirectory::Dropped { exact, under } = got else {
            panic!("expected Dropped, got {got:?}");
        };
        assert!(exact.unwrap());
        assert_eq!(under.unwrap(), vec!["Docs/a.md".to_string()]);
        assert_eq!(
            db.all_document_paths().unwrap(),
            vec!["docs/b.md".to_string()]
        );
    }

    /// (#544 D3) A directory whose spelling could not be looked at keeps every row, and is
    /// scanned as before.
    #[test]
    fn an_arrived_directory_that_cannot_be_looked_at_keeps_its_prefix() {
        let (kb, _cleanup) = i544_scratch_kb("groove-watcher-i544-arrived-unknown");
        let db = dir_rename_db(&["Docs", "Docs/a.md", "docs/b.md"]);
        let denied = std::io::ErrorKind::PermissionDenied;
        let got = act_on_arrived_directory_for(
            &db,
            &kb,
            "Docs",
            Some(true),
            OnDisk::Unknown(denied),
            false,
        );
        assert!(
            matches!(got, ArrivedDirectory::Unknown(k) if k == denied),
            "{got:?}"
        );
        let got =
            act_on_arrived_directory_for(&db, &kb, "Docs", None, OnDisk::Unknown(denied), false);
        assert!(
            matches!(got, ArrivedDirectory::Unknown(k) if k == denied),
            "{got:?}"
        );
        assert_eq!(
            db.all_document_paths().unwrap(),
            vec![
                "Docs".to_string(),
                "Docs/a.md".to_string(),
                "docs/b.md".to_string()
            ]
        );
    }

    /// (#544) A path that is no longer a directory by the time the lock is held, or is not
    /// there at all, is left alone: its own event comes on its own.
    #[test]
    fn an_arrived_directory_that_is_no_longer_one_is_left_alone() {
        let (kb, _cleanup) = i544_scratch_kb("groove-watcher-i544-arrived-not-dir");
        let db = dir_rename_db(&["a.md", "a.md/x.md"]);
        let got = act_on_arrived_directory_for(&db, &kb, "a.md", Some(false), OnDisk::Gone, false);
        assert!(
            matches!(got, ArrivedDirectory::NoLongerADirectory),
            "{got:?}"
        );
        let got = act_on_arrived_directory_for(&db, &kb, "a.md", None, OnDisk::Gone, false);
        assert!(
            matches!(got, ArrivedDirectory::NoLongerADirectory),
            "{got:?}"
        );
        std::fs::write(kb.join("a.md"), "# a\n").unwrap();
        let got = act_on_arrived_directory(&db, &kb, "a.md", &kb.join("a.md"));
        assert!(
            matches!(got, ArrivedDirectory::NoLongerADirectory),
            "{got:?}"
        );
        assert_eq!(
            db.all_document_paths().unwrap(),
            vec!["a.md".to_string(), "a.md/x.md".to_string()]
        );
    }

    /// (#544 P1-L directory branch) A directory whose name is not UTF-8 is keyed by a lossy
    /// spelling, which a sibling whose name differs only in the bytes that are not UTF-8 shares.
    /// Its arrival is never read as a spelling gone from disk: it is scanned, and no row goes,
    /// neither its own nor the sibling's.
    #[cfg(target_os = "linux")]
    #[test]
    fn an_arrived_directory_under_a_lossy_name_keeps_its_rows() {
        use std::os::unix::ffi::OsStrExt;
        let (kb, _cleanup) = i544_scratch_kb("groove-watcher-i544-arrived-lossy");
        let raw = kb.join(std::ffi::OsStr::from_bytes(b"d\xff"));
        let sibling = kb.join(std::ffi::OsStr::from_bytes(b"d\xfe"));
        std::fs::create_dir_all(&raw).unwrap();
        std::fs::create_dir_all(&sibling).unwrap();
        let rel = indexer::index_rel_path(&kb, &raw).unwrap();
        assert_eq!(indexer::index_rel_path(&kb, &sibling).unwrap(), rel);
        let under = format!("{rel}/a.md");
        let db = dir_rename_db(&[under.as_str()]);
        assert!(rel_is_lossy(&kb, Some(&raw)));
        let got = act_on_arrived_directory(&db, &kb, &rel, &raw);
        assert!(matches!(got, ArrivedDirectory::Scan(Ok(false))), "{got:?}");
        let got = act_on_arrived_directory_for(&db, &kb, &rel, Some(true), OnDisk::Gone, true);
        assert!(matches!(got, ArrivedDirectory::Scan(Ok(false))), "{got:?}");
        assert_eq!(db.all_document_paths().unwrap(), vec![under]);
    }

    /// (#544 P1-S2) The scan of an arrived directory spells the directory once; each file it
    /// found only asks whether a regular file is there, which lists no directory.
    #[test]
    fn the_scan_of_an_arrived_directory_lists_no_directory_per_file() {
        let (kb, _cleanup) = i544_scratch_kb("groove-watcher-i544-arrived-cost");
        std::fs::create_dir_all(kb.join("docs")).unwrap();
        let names = ["a.md", "b.md", "c.md", "d.md"];
        for name in names {
            std::fs::write(kb.join("docs").join(name), "# x\n").unwrap();
        }
        let before = listings();
        for name in names {
            assert_eq!(
                written_path_on_disk(&kb, &format!("docs/{name}"), WriteLook::ExistsOnly),
                OnDisk::Present,
                "{name}"
            );
        }
        assert_eq!(listings(), before, "a file of the scan listed a directory");
    }

    /// (#544 P1-S) A directory renamed onto a name the disk holds only in another case moves
    /// none of its rows there (the listed rows still leave the old path); one whose destination
    /// is there, or cannot be looked at, moves them as before. A lossy destination is never read
    /// as gone.
    #[test]
    fn a_directory_renamed_onto_a_name_held_only_in_another_case_moves_nothing() {
        assert!(!dir_rename_moves_its_steps(OnDisk::Gone, false));
        assert!(dir_rename_moves_its_steps(OnDisk::Present, false));
        assert!(dir_rename_moves_its_steps(
            OnDisk::Unknown(std::io::ErrorKind::PermissionDenied),
            false
        ));
        assert!(dir_rename_moves_its_steps(OnDisk::Gone, true));
    }

    /// (#544 P1-S) A directory rename spells its destination once: after that look, each
    /// step's destination only asks whether a regular file is there, which lists nothing more.
    #[test]
    fn a_directory_rename_spells_its_destination_once() {
        let (kb, _cleanup) = i544_scratch_kb("groove-watcher-i544-dir-rename-cost");
        std::fs::create_dir_all(kb.join("Dest")).unwrap();
        let names = ["a.md", "b.md", "c.md", "d.md"];
        for name in names {
            std::fs::write(kb.join("Dest").join(name), "# x\n").unwrap();
        }
        assert_eq!(
            arrived_directory_on_disk(&kb, "Dest", &kb.join("Dest")),
            OnDisk::Present
        );
        let after_the_destination = listings();
        for name in names {
            assert_eq!(
                written_path_on_disk(&kb, &format!("Dest/{name}"), WriteLook::ExistsOnly),
                OnDisk::Present,
                "{name}"
            );
        }
        assert_eq!(
            listings(),
            after_the_destination,
            "a step listed a directory"
        );
        assert_eq!(
            arrived_directory_on_disk(&kb, "dest", &kb.join("dest")),
            OnDisk::Gone,
            "the destination under another case is not there under that spelling"
        );
    }

    /// (#544 A14) The source of a directory rename is swept, under the db lock, only when it
    /// is gone: not found, or a file where a directory on the way was. A source that is there
    /// again keeps its rows.
    #[test]
    fn the_source_of_a_directory_rename_is_swept_only_when_gone() {
        let (kb, _cleanup) = i544_scratch_kb("groove-watcher-i544-source-sweep");
        let db = dir_rename_db(&["old/a.md", "other.md"]);
        match sweep_source_if_gone(&db, &kb, &kb.join("old")) {
            SourceSweep::Swept(Some((rel, Ok(deleted)))) => {
                assert_eq!(rel, "old");
                assert_eq!(deleted, vec!["old/a.md".to_string()]);
            }
            got => panic!("a missing source must be swept: {got:?}"),
        }
        assert_eq!(
            db.all_document_paths().unwrap(),
            vec!["other.md".to_string()]
        );

        let db = dir_rename_db(&["old/a.md", "other.md"]);
        std::fs::create_dir_all(kb.join("old")).unwrap();
        let got = sweep_source_if_gone(&db, &kb, &kb.join("old"));
        assert!(matches!(got, SourceSweep::Present), "{got:?}");
        assert_eq!(
            db.all_document_paths().unwrap(),
            vec!["old/a.md".to_string(), "other.md".to_string()]
        );

        // A file where the directory above the source was: not a directory on Unix, not found
        // on Windows (estimated), and gone either way.
        std::fs::write(kb.join("f"), "x").unwrap();
        let db = dir_rename_db(&["f/old/a.md", "other.md"]);
        match sweep_source_if_gone(&db, &kb, &kb.join("f").join("old")) {
            SourceSweep::Swept(Some((rel, Ok(deleted)))) => {
                assert_eq!(rel, "f/old");
                assert_eq!(deleted, vec!["f/old/a.md".to_string()]);
            }
            got => panic!("a source under a file must be swept: {got:?}"),
        }
        assert_eq!(
            db.all_document_paths().unwrap(),
            vec!["other.md".to_string()]
        );
    }

    /// (#544 M4) The rename follow-up's line words the second read's outcome as prose in ASCII,
    /// never as the Rust value it came from.
    #[test]
    fn read_again_outcome_is_ascii_prose() {
        let dropped = read_again_outcome(&Ok(indexer::SingleResult::NothingToIndex {
            deindexed: true,
            frontmatter_unparsed: false,
        }));
        assert_eq!(dropped, "dropped the row (nothing to embed)");
        let rewrote = read_again_outcome(&Ok(indexer::SingleResult::Updated {
            chunks: 3,
            frontmatter_unparsed: false,
        }));
        assert_eq!(rewrote, "rewrote the row (3 chunks)");
        let every: [Result<indexer::SingleResult>; 9] = [
            Ok(indexer::SingleResult::Updated {
                chunks: 1,
                frontmatter_unparsed: true,
            }),
            Ok(indexer::SingleResult::Unchanged),
            Ok(indexer::SingleResult::MetadataRefreshed {
                frontmatter_unparsed: false,
            }),
            Ok(indexer::SingleResult::Skipped {
                reason: indexer::SKIPPED_NO_CHUNKS,
                frontmatter_unparsed: false,
            }),
            Ok(indexer::SingleResult::Refused),
            Ok(indexer::SingleResult::OutsideKnowledgeBaseDeindexed),
            Ok(indexer::SingleResult::NothingToIndex {
                deindexed: true,
                frontmatter_unparsed: true,
            }),
            Ok(indexer::SingleResult::NothingToIndex {
                deindexed: false,
                frontmatter_unparsed: false,
            }),
            Err(anyhow::anyhow!("disk went away")),
        ];
        for result in &every {
            let line = read_again_outcome(result);
            assert!(line.is_ascii(), "{line}");
            assert!(
                !line.contains('{') && !line.contains("SingleResult"),
                "a Debug value leaked: {line}"
            );
        }
    }

    /// (kuriya 557) The rows the store holds for the 557 decisions: two under `sub/`, one under
    /// a sibling sharing `sub` as a prefix, and one at a file named after it.
    const I557_ROWS: [&str; 4] = ["sub/a.md", "sub/deep/b.md", "sub-other/c.md", "sub.md"];

    fn i557_under_sub() -> Vec<String> {
        vec!["sub/a.md".to_string(), "sub/deep/b.md".to_string()]
    }

    fn i557_all_rows() -> Vec<String> {
        // `all_document_paths` lists in byte order.
        let mut rows: Vec<String> = I557_ROWS.iter().map(|s| s.to_string()).collect();
        rows.sort();
        rows
    }

    /// (kuriya 557) A removed directory that is still gone drops every row under it, and
    /// nothing under a sibling sharing its name as a prefix or at a file named after it.
    #[test]
    fn i557_a_directory_still_gone_drops_every_row_under_it() {
        let kb = crate::test_support::unique_temp_path("groove-watcher-i557-gone");
        let db = dir_rename_db(&I557_ROWS);
        match decide_subtree_removal(&db, &kb, &kb.join("sub"), OnDisk::Gone) {
            Some((rel, SubtreeRemoval::Swept(Ok(deleted)))) => {
                assert_eq!(rel, "sub");
                assert_eq!(deleted, i557_under_sub());
            }
            got => panic!("a gone directory must be swept: {got:?}"),
        }
        assert_eq!(
            db.all_document_paths().unwrap(),
            vec!["sub-other/c.md".to_string(), "sub.md".to_string()]
        );
    }

    /// (kuriya 557) A removed directory that is there again (deleted and created within one
    /// window) drops no row: the rows under it are handed on, each to be decided as its own
    /// removal.
    #[test]
    fn i557_a_directory_still_there_hands_its_rows_on_and_drops_none() {
        let kb = crate::test_support::unique_temp_path("groove-watcher-i557-there");
        let db = dir_rename_db(&I557_ROWS);
        match decide_subtree_removal(&db, &kb, &kb.join("sub"), OnDisk::Present) {
            Some((rel, SubtreeRemoval::StillThere(Ok(held)))) => {
                assert_eq!(rel, "sub");
                assert_eq!(held, i557_under_sub());
            }
            got => panic!("a directory still there must hand its rows on: {got:?}"),
        }
        assert_eq!(db.all_document_paths().unwrap(), i557_all_rows());
    }

    /// (kuriya 557) A removed directory the watcher cannot look at keeps every row under it,
    /// and says how many and why.
    #[test]
    fn i557_a_directory_that_cannot_be_looked_at_keeps_its_rows() {
        let kb = crate::test_support::unique_temp_path("groove-watcher-i557-unknown");
        let db = dir_rename_db(&I557_ROWS);
        let look = OnDisk::Unknown(std::io::ErrorKind::PermissionDenied);
        match decide_subtree_removal(&db, &kb, &kb.join("sub"), look) {
            Some((
                rel,
                SubtreeRemoval::LeftIndexed {
                    kind,
                    held: Ok(held),
                },
            )) => {
                assert_eq!(rel, "sub");
                assert_eq!(kind, std::io::ErrorKind::PermissionDenied);
                assert_eq!(held, i557_under_sub());
            }
            got => panic!("an unknown look must leave the rows: {got:?}"),
        }
        assert_eq!(db.all_document_paths().unwrap(), i557_all_rows());
    }

    /// (kuriya 557) The knowledge base itself and a path outside it name no subtree, whatever
    /// the look says.
    #[test]
    fn i557_the_kb_root_and_a_path_outside_name_no_subtree() {
        let kb = crate::test_support::unique_temp_path("groove-watcher-i557-root");
        let outside = crate::test_support::unique_temp_path("groove-watcher-i557-outside");
        let db = dir_rename_db(&I557_ROWS);
        for look in [OnDisk::Gone, OnDisk::Present] {
            assert!(decide_subtree_removal(&db, &kb, &kb, look).is_none());
            assert!(decide_subtree_removal(&db, &kb, &outside.join("sub"), look).is_none());
        }
        // The watcher's own entry, which takes the look itself, asks the same question.
        assert!(subtree_removal_under_lock(&db, &kb, &kb).is_none());
        assert!(subtree_removal_under_lock(&db, &kb, &outside.join("sub")).is_none());
        assert_eq!(subtree_rel(&kb, &kb), None);
        assert_eq!(subtree_rel(&kb, &kb.join("sub")).as_deref(), Some("sub"));
        assert_eq!(db.all_document_paths().unwrap(), i557_all_rows());
    }

    /// (kuriya 557, GitHub codex r1 P2 on PR #401) A removed file replaced by a link (a symlink
    /// on Unix, a junction on Windows, where neither needs a privilege): the index holds no
    /// document at a link (the full walk skips it, a write at it drops the row), so the
    /// removal drops the row. The look outside the locks hands it inside them, and the look
    /// taken there, under the database lock, is the one that deletes.
    #[test]
    fn i557_a_removed_file_replaced_by_a_link_drops_its_row() {
        let (kb, _cleanup) = i544_scratch_kb("groove-watcher-i557-link");
        let target = kb.join("target");
        std::fs::create_dir(&target).unwrap();
        let link = kb.join("a.md");
        #[cfg(windows)]
        {
            let made = std::process::Command::new("cmd")
                .args([
                    "/c",
                    "mklink",
                    "/J",
                    &link.display().to_string(),
                    &target.display().to_string(),
                ])
                .status()
                .expect("mklink must be runnable");
            assert!(made.success(), "mklink /J needs no elevation");
        }
        #[cfg(unix)]
        {
            std::fs::write(target.join("t.md"), "# t\n").unwrap();
            std::os::unix::fs::symlink(target.join("t.md"), &link).unwrap();
        }
        let reg = Registry::defaults();
        let rules = crate::exclusion::ExclusionRules::load(&kb, default_exclude_dirs());

        let outcome = removal_outcome("a.md", &link, &reg, &rules);
        assert_eq!(outcome, (RemovalAction::Deindex, None));
        assert_eq!(
            removal_dispatch("a.md", outcome),
            RemovalDispatch::DecideInLocks
        );

        let db = dir_rename_db(&["a.md", "other.md"]);
        let look = removed_path_on_disk(&kb, "a.md");
        assert_eq!(look, OnDisk::Gone);
        let removed = remove_if_still_gone(
            &db,
            &kb,
            "a.md",
            &link,
            &reg,
            &rules,
            look,
            RemovalCause::RemoveEvent,
        );
        assert!(
            matches!(removed, RemovalUnderDatabaseLock::Deindexed(Ok(true))),
            "{removed:?}"
        );
        assert_eq!(
            db.all_document_paths().unwrap(),
            vec!["other.md".to_string()]
        );
    }

    /// (kuriya 557, local Codex r1 F2) Only a key holding U+FFFD may be a lossy spelling.
    #[test]
    fn i557_only_a_key_holding_the_replacement_character_may_be_lossy() {
        assert!(key_may_be_lossy("sub/caf\u{FFFD}.md"));
        assert!(!key_may_be_lossy("sub/café.md"));
        assert!(!key_may_be_lossy("sub/a.md"));
    }

    /// (kuriya 557) What replaced a removed directory decides the look the subtree removal
    /// takes ([`arrived_directory_on_disk`]): nothing, a file, a link, or the directory under
    /// another case is gone; the directory under its exact spelling is there. The existing
    /// function is pinned here because the subtree removal relies on these answers.
    #[test]
    fn i557_what_replaced_a_removed_directory_decides_its_look() {
        let (kb, _cleanup) = i544_scratch_kb("groove-watcher-i557-look");
        let sub = kb.join("sub");
        assert_eq!(arrived_directory_on_disk(&kb, "sub", &sub), OnDisk::Gone);

        std::fs::write(&sub, "x").unwrap();
        assert_eq!(arrived_directory_on_disk(&kb, "sub", &sub), OnDisk::Gone);
        std::fs::remove_file(&sub).unwrap();

        std::fs::create_dir(&sub).unwrap();
        assert_eq!(arrived_directory_on_disk(&kb, "sub", &sub), OnDisk::Present);
        #[cfg(any(windows, target_os = "macos"))]
        assert_eq!(
            arrived_directory_on_disk(&kb, "Sub", &kb.join("Sub")),
            OnDisk::Gone,
            "the directory under another case is not there under that spelling"
        );
        std::fs::remove_dir(&sub).unwrap();

        let target = kb.join("target");
        std::fs::create_dir(&target).unwrap();
        #[cfg(windows)]
        {
            let made = std::process::Command::new("cmd")
                .args([
                    "/c",
                    "mklink",
                    "/J",
                    &sub.display().to_string(),
                    &target.display().to_string(),
                ])
                .status()
                .expect("mklink must be runnable");
            assert!(made.success(), "mklink /J needs no elevation");
        }
        #[cfg(unix)]
        std::os::unix::fs::symlink(&target, &sub).unwrap();
        assert_eq!(
            arrived_directory_on_disk(&kb, "sub", &sub),
            OnDisk::Gone,
            "a link where the directory was is not the directory"
        );
    }

    /// (kuriya 557) The subtree removal takes its look from [`arrived_directory_on_disk`] under
    /// the caller's database lock: a directory replaced by a file of the same name is swept,
    /// though something is at the path; the directory there again keeps its rows.
    #[test]
    fn i557_the_subtree_removal_looks_at_the_directory_itself() {
        let (kb, _cleanup) = i544_scratch_kb("groove-watcher-i557-caller");
        let sub = kb.join("sub");

        std::fs::write(&sub, "x").unwrap();
        let db = dir_rename_db(&I557_ROWS);
        match subtree_removal_under_lock(&db, &kb, &sub) {
            Some((_, SubtreeRemoval::Swept(Ok(deleted)))) => {
                assert_eq!(deleted, i557_under_sub())
            }
            got => panic!("a file where the directory was must sweep: {got:?}"),
        }
        std::fs::remove_file(&sub).unwrap();

        std::fs::create_dir(&sub).unwrap();
        let db = dir_rename_db(&I557_ROWS);
        match subtree_removal_under_lock(&db, &kb, &sub) {
            Some((_, SubtreeRemoval::StillThere(Ok(held)))) => assert_eq!(held, i557_under_sub()),
            got => panic!("the directory there again must keep its rows: {got:?}"),
        }
        assert_eq!(db.all_document_paths().unwrap(), i557_all_rows());
    }

    /// (#550) A directory is never read as a hard link: every directory on Linux and macOS
    /// has a link count of two or more, and a directory whose name passes the extension filter
    /// (`dir.md`) used to fail the removal gate there, so its row was kept on Linux and macOS
    /// and dropped on Windows. The facts are handed in so Windows, where a directory's count
    /// cannot be read, runs the same case.
    ///
    /// The write gate keeps its answer from before #550 (the raw count refuses), recorded here
    /// so a change to it is a decision rather than a side effect (kuriya 566 plan, S3).
    #[test]
    fn i550_a_directory_is_never_read_as_a_hard_link() {
        let kb = i549_kb("groove-watcher-i550");
        let reg = Registry::defaults();
        let rules = crate::exclusion::ExclusionRules::load(&kb.0, default_exclude_dirs());
        let full = kb.0.join("dir.md");

        let dir = PathFacts::given(&full, PathKind::Dir, Some(3));
        assert!(!dir.is_multiply_linked());
        assert!(gate_given("dir.md", &dir, &reg, &rules, Gate::Removal));
        assert!(!gate_given("dir.md", &dir, &reg, &rules, Gate::Write));

        let symlink = PathFacts::given(&full, PathKind::Symlink, Some(2));
        assert!(!symlink.is_multiply_linked());

        // A file with the same count is a hard link.
        let file = PathFacts::given(&full, PathKind::Other, Some(3));
        assert!(file.is_multiply_linked());
        let single = PathFacts::given(&full, PathKind::Other, Some(1));
        assert!(!single.is_multiply_linked());
    }

    /// (#550) The same on disk: a directory named `dir.md` with a subdirectory in it (a count
    /// of three on ext4 and APFS) passes the removal gate. Unix only: Windows never read a
    /// directory's count, so it passed there before the fix too.
    #[cfg(unix)]
    #[test]
    fn i550_a_directory_on_disk_passes_the_removal_gate() {
        let kb = i549_kb("groove-watcher-i550-disk");
        let reg = Registry::defaults();
        let rules = crate::exclusion::ExclusionRules::load(&kb.0, default_exclude_dirs());
        std::fs::create_dir_all(kb.0.join("dir.md").join("sub")).unwrap();
        assert!(deindex_passes("dir.md", &kb.0.join("dir.md"), &reg, &rules));
    }

    /// (kuriya 566 R7) One removal decision looks at the path once, and both gates read that
    /// look: a path read as a link by one gate and as a plain file by the other was a row
    /// dropped for a file there that neither would keep.
    #[test]
    fn r7_one_removal_decision_reads_the_path_once() {
        let kb = i549_kb("groove-watcher-r7");
        let reg = Registry::defaults();
        let rules = crate::exclusion::ExclusionRules::load(&kb.0, default_exclude_dirs());
        // A hard link, so the write gate refuses it and the removal gate is asked too.
        std::fs::hard_link(kb.0.join("doc.md"), kb.0.join("twin.md")).unwrap();

        FACTS_TAKEN.with(|n| n.set(0));
        let _ = removal_decision_for(
            "twin.md",
            &kb.0.join("twin.md"),
            &reg,
            &rules,
            OnDisk::Present,
        );
        assert_eq!(FACTS_TAKEN.with(|n| n.get()), 1);

        let full = kb.0.join("doc.md");
        let link = PathFacts::given(&full, PathKind::Symlink, None);
        assert_eq!(
            removal_decision_given("doc.md", &link, &reg, &rules, OnDisk::Present),
            RemovalAction::Deindex
        );
        let plain = PathFacts::given(&full, PathKind::Other, Some(1));
        assert_eq!(
            removal_decision_given("doc.md", &plain, &reg, &rules, OnDisk::Present),
            RemovalAction::Reindex
        );
    }

    /// (kuriya 566) A removal whose path is now a hard link drops the row, as the full index
    /// does (its walk does not collect a hard link, and its deletion pass drops the row): the
    /// index holds no document at a link, a symlink's or a hard link's alike. The other row stays.
    #[test]
    fn i566_a_removal_drops_the_row_of_a_path_now_a_hard_link() {
        let kb = i549_kb("groove-watcher-i566-removal");
        std::fs::hard_link(kb.0.join("doc.md"), kb.0.join("twin.md")).unwrap();
        let reg = Registry::defaults();
        let rules = crate::exclusion::ExclusionRules::load(&kb.0, default_exclude_dirs());

        let db = dir_rename_db(&["twin.md", "other.md"]);
        let outcome = decide_removal_in_locks(
            &db,
            &kb.0,
            "twin.md",
            &kb.0.join("twin.md"),
            &reg,
            &rules,
            OnDisk::Present,
            RemovalCause::RemoveEvent,
        );
        assert!(
            matches!(outcome, RemovalInLocks::Deindexed(Ok(true))),
            "{outcome:?}"
        );
        assert_eq!(
            db.all_document_paths().unwrap(),
            vec!["other.md".to_string()]
        );
    }

    /// (kuriya 566 #16 / #17) The rows an arrived directory's walk did not collect are the
    /// held keys minus the walk's, and a key that may be lossy is counted and kept, never
    /// handed on.
    #[test]
    fn i566_rows_the_walk_left_behind_are_the_held_keys_it_did_not_find() {
        let found = vec![
            ("d/b.md".to_string(), PathBuf::from("d/b.md")),
            ("d/new.md".to_string(), PathBuf::from("d/new.md")),
        ];
        let held = vec![
            "d/a.md".to_string(),
            "d/b.md".to_string(),
            format!("d/{}.md", char::REPLACEMENT_CHARACTER),
            "d/sub/c.md".to_string(),
        ];
        let (left, lossy) = rows_the_walk_left_behind(held, &found);
        assert_eq!(left, vec!["d/a.md".to_string(), "d/sub/c.md".to_string()]);
        assert_eq!(lossy, 1);

        let (left, lossy) = rows_the_walk_left_behind(Vec::new(), &found);
        assert!(left.is_empty());
        assert_eq!(lossy, 0);
    }

    /// (kuriya 566 #19 / #23 / #28) A file the indexer refused inside the locks loses its row
    /// when it is a hard link there; a plain file refused for another reason (its size measured
    /// on the handle, say) keeps it, and so does a path the look did not find, which the
    /// caller's own relook decides.
    #[test]
    fn i566_a_refused_hard_link_loses_its_row_and_a_refused_plain_file_keeps_it() {
        let kb = i549_kb("groove-watcher-i566-refused");
        std::fs::write(kb.0.join("big.md"), "# big\n").unwrap();
        std::fs::hard_link(kb.0.join("doc.md"), kb.0.join("twin.md")).unwrap();
        let reg = Registry::defaults();
        let rules = crate::exclusion::ExclusionRules::load(&kb.0, default_exclude_dirs());
        let db = dir_rename_db(&["twin.md", "big.md", "other.md"]);
        let refused = |rel: &str, on_disk: OnDisk| {
            refused_row_in_locks(&db, &kb.0, rel, &kb.0.join(rel), &reg, &rules, on_disk)
        };

        assert!(refused("big.md", OnDisk::Present).is_none());
        assert!(refused("twin.md", OnDisk::Gone).is_none());
        assert!(refused("twin.md", OnDisk::Unknown(std::io::ErrorKind::Other)).is_none());
        assert_eq!(
            db.all_document_paths().unwrap(),
            vec![
                "big.md".to_string(),
                "other.md".to_string(),
                "twin.md".to_string()
            ]
        );

        assert!(matches!(
            refused("twin.md", OnDisk::Present),
            Some(Ok(true))
        ));
        assert_eq!(
            db.all_document_paths().unwrap(),
            vec!["big.md".to_string(), "other.md".to_string()]
        );
    }

    /// (kuriya 566 #28) After a rename under both locks, a destination that is a hard link
    /// loses the row the rename moved there; a plain file at the destination (one the indexer
    /// refused for its size, say) keeps it.
    #[test]
    fn i566_a_rename_destination_now_a_hard_link_loses_its_row() {
        let kb = i549_kb("groove-watcher-i566-dest");
        std::fs::write(kb.0.join("big.md"), "# big\n").unwrap();
        std::fs::hard_link(kb.0.join("doc.md"), kb.0.join("twin.md")).unwrap();
        let reg = Registry::defaults();
        let rules = crate::exclusion::ExclusionRules::load(&kb.0, default_exclude_dirs());
        let db = dir_rename_db(&["twin.md", "big.md", "other.md"]);

        assert!(
            link_destination_in_locks(&db, &kb.0, "big.md", &reg, &rules, WriteLook::Spelled)
                .is_none()
        );
        assert!(matches!(
            link_destination_in_locks(&db, &kb.0, "twin.md", &reg, &rules, WriteLook::Spelled),
            Some(Ok(true))
        ));
        assert_eq!(
            db.all_document_paths().unwrap(),
            vec!["big.md".to_string(), "other.md".to_string()]
        );
    }

    /// (kuriya 566) A rename away from a path that is now a hard link moves the row: the old
    /// end passes the removal gate whatever is at it, and only the new end is judged as a write.
    #[test]
    fn i566_a_rename_away_from_a_hard_link_moves_the_row() {
        let kb = i549_kb("groove-watcher-i566-rename");
        std::fs::hard_link(kb.0.join("doc.md"), kb.0.join("old.md")).unwrap();
        std::fs::write(kb.0.join("good.md"), "# good\n").unwrap();
        let reg = Registry::defaults();
        let rules = crate::exclusion::ExclusionRules::load(&kb.0, default_exclude_dirs());
        assert_eq!(
            rename_decision(
                "old.md",
                &kb.0.join("old.md"),
                "good.md",
                &kb.0.join("good.md"),
                &reg,
                &rules
            ),
            RenameAction::Rename
        );
    }

    // -----------------------------------------------------------------------
    // (#259 PR2) run_watch_loop_until, watch_status, watch_backoff, batch_probe
    // -----------------------------------------------------------------------

    /// A watcher state over `kb` with a stub embedder and an in-memory store
    /// holding `doc.md`, debouncing for 100 ms.
    fn until_state(kb: &Path) -> WatcherState {
        WatcherState {
            kb_path: kb.to_path_buf(),
            db: Arc::new(Mutex::new(dir_rename_db(&["doc.md"]))),
            embedder: Arc::new(Mutex::new(Embedder::stub_for_tests(384))),
            registry: Arc::new(Registry::defaults()),
            exclude_headings: None,
            rules: crate::exclusion::ExclusionRules::load(kb, default_exclude_dirs()),
            config: WatchConfig {
                enabled: true,
                debounce_ms: 100,
            },
            watcher_active: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            watch_status: None,
            watch_backoff: WatcherState::DEFAULT_WATCH_BACKOFF,
            batch_probe: None,
        }
    }

    /// Poll `cond` every 20 ms until it holds or `limit` runs out.
    async fn poll_until(limit: Duration, mut cond: impl FnMut() -> bool) -> bool {
        let deadline = tokio::time::Instant::now() + limit;
        loop {
            if cond() {
                return true;
            }
            if tokio::time::Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// A `true` on the stop signal ends the loop with `Ok(())`.
    #[tokio::test]
    async fn the_stop_signal_ends_the_loop_with_ok() {
        let kb = i549_kb("groove-watcher-until-stop");
        let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
        let task = tokio::spawn(run_watch_loop_until(until_state(&kb.0), stop_rx));
        stop_tx.send(true).unwrap();
        let ended = tokio::time::timeout(Duration::from_secs(5), task).await;
        let joined = ended.expect("the loop did not end within 5 s of the stop signal");
        joined
            .expect("the loop task panicked")
            .expect("the loop ended with an error");
    }

    /// Dropping the stop sender is not a stop: the loop keeps running.
    #[tokio::test]
    async fn dropping_the_stop_sender_does_not_end_the_loop() {
        let kb = i549_kb("groove-watcher-until-drop");
        let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
        let task = tokio::spawn(run_watch_loop_until(until_state(&kb.0), stop_rx));
        drop(stop_tx);
        tokio::time::sleep(Duration::from_secs(1)).await;
        let finished = task.is_finished();
        task.abort();
        assert!(!finished, "dropping the stop sender ended the loop");
    }

    /// A kb directory that is not there is reported through `watch_status`
    /// with the kb and the error; once it appears, `None` clears it.
    #[tokio::test]
    async fn watch_status_reports_a_missing_dir_then_clears() {
        let kb_path = crate::test_support::unique_temp_path("groove-watcher-until-missing");
        assert!(!kb_path.exists());
        let _cleanup = I549Kb(kb_path.clone());
        let seen: Arc<Mutex<Vec<Option<crate::server::WatchErrorCause>>>> =
            Arc::new(Mutex::new(Vec::new()));
        let mut state = until_state(&kb_path);
        // The bridge thread outlives the test and retries once the dir is gone;
        // a 2 s cap keeps that slow while the first retries stay short.
        state.watch_backoff = (Duration::from_millis(50), Duration::from_secs(2));
        // Weak: the leaked bridge thread must not keep pushing into `seen`.
        let sink = Arc::downgrade(&seen);
        state.watch_status = Some(Arc::new(move |cause| {
            if let Some(seen) = sink.upgrade() {
                seen.lock().unwrap().push(cause);
            }
        }));
        let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
        let task = tokio::spawn(run_watch_loop_until(state, stop_rx));

        let reported = poll_until(Duration::from_secs(10), || {
            seen.lock().unwrap().iter().any(|c| c.is_some())
        })
        .await;
        assert!(
            reported,
            "no watch error within 10 s: {:?}",
            seen.lock().unwrap()
        );
        let cause = seen
            .lock()
            .unwrap()
            .iter()
            .flatten()
            .next()
            .cloned()
            .unwrap();
        assert!(!cause.error.is_empty(), "{cause:?}");
        assert_eq!(cause.kb, kb_path);

        std::fs::create_dir_all(&kb_path).unwrap();
        let cleared = poll_until(Duration::from_secs(10), || {
            let seen = seen.lock().unwrap();
            let first_error = seen.iter().position(|c| c.is_some()).unwrap();
            seen[first_error..].iter().any(|c| c.is_none())
        })
        .await;
        assert!(
            cleared,
            "no clear within 10 s of the dir appearing: {:?}",
            seen.lock().unwrap()
        );

        stop_tx.send(true).unwrap();
        let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
    }

    /// Rewriting an indexed file is one batch the probe sees emitted, started
    /// and finished.
    #[tokio::test]
    async fn the_batch_probe_counts_started_and_finished_batches() {
        use std::sync::atomic::Ordering::SeqCst;
        let kb = i549_kb("groove-watcher-until-probe");
        let armed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let probe = Arc::new(WatchBatchProbe::default());
        let mut state = until_state(&kb.0);
        let flag = Arc::clone(&armed);
        state.watch_status = Some(Arc::new(move |cause| {
            if cause.is_none() {
                flag.store(true, SeqCst);
            }
        }));
        state.batch_probe = Some(Arc::clone(&probe));
        let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
        let task = tokio::spawn(run_watch_loop_until(state, stop_rx));

        assert!(
            poll_until(Duration::from_secs(10), || armed.load(SeqCst)).await,
            "the watch was not registered within 10 s"
        );
        std::fs::write(kb.0.join("doc.md"), "# doc\n\nrewritten body\n").unwrap();
        let counted = poll_until(Duration::from_secs(10), || {
            let started = probe.started.load(SeqCst);
            started >= 1 && probe.finished.load(SeqCst) == started
        })
        .await;
        let (emitted, started, finished) = (
            probe.emitted.load(SeqCst),
            probe.started.load(SeqCst),
            probe.finished.load(SeqCst),
        );
        assert!(
            counted,
            "emitted {emitted}, started {started}, finished {finished}"
        );
        assert!(
            emitted >= started,
            "emitted {emitted}, started {started}, finished {finished}"
        );

        stop_tx.send(true).unwrap();
        let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
    }

    /// A watch state over `kb` with a fresh probe and a flag raised when the
    /// watch is registered.
    fn probed_state(
        kb: &Path,
    ) -> (
        WatcherState,
        Arc<WatchBatchProbe>,
        Arc<std::sync::atomic::AtomicBool>,
    ) {
        let armed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let probe = Arc::new(WatchBatchProbe::default());
        let mut state = until_state(kb);
        let flag = Arc::clone(&armed);
        state.watch_status = Some(Arc::new(move |cause| {
            if cause.is_none() {
                flag.store(true, std::sync::atomic::Ordering::SeqCst);
            }
        }));
        state.batch_probe = Some(Arc::clone(&probe));
        (state, probe, armed)
    }

    /// A stop that arrives while a batch is being handled lets that batch
    /// finish, then the loop returns `Ok(())`.
    /// A loop that dropped the in-flight handling on the stop would return
    /// with [`WatchBatchProbe::finished`] still 0.
    #[tokio::test]
    async fn a_stop_during_a_batch_waits_for_that_batch_to_finish() {
        use std::sync::atomic::Ordering::SeqCst;
        let kb = i549_kb("groove-watcher-until-mid-batch");
        // The canonical kb, as `groove serve` hands the watcher one. On macOS
        // the temp dir is under `/var`, a link to `/private/var`, and FSEvents
        // names every path under the latter: against the spelling as written
        // the write is not under the kb, and its batch finishes without
        // taking the embedder.
        let (state, probe, armed) = probed_state(&kb.0.canonicalize().unwrap());
        let embedder = Arc::clone(&state.embedder);
        let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
        let task = tokio::spawn(run_watch_loop_until(state, stop_rx));
        assert!(
            poll_until(Duration::from_secs(10), || armed.load(SeqCst)).await,
            "the watch was not registered within 10 s"
        );

        // Hold the embedder lock on a plain thread (as the i548 test holds it)
        // so the reindex of the batch blocks until it is let go.
        let (locked_tx, locked_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let holder = std::thread::spawn(move || {
            let _held = embedder.lock().unwrap();
            locked_tx.send(()).unwrap();
            let _ = release_rx.recv();
        });
        locked_rx.recv().unwrap();
        let waits_before = probe.lock_waits.load(SeqCst);

        std::fs::write(kb.0.join("doc.md"), "# doc\n\nrewritten mid batch\n").unwrap();
        // Not "a batch is in flight": on Linux the write's `Access(Open)` can
        // come as a batch of its own that takes no lock and finishes first.
        // A rise in `lock_waits` says a batch has reached the held lock.
        let reached = poll_until(Duration::from_secs(10), || {
            probe.lock_waits.load(SeqCst) > waits_before
        })
        .await;
        assert!(
            reached,
            "no batch reached the held lock within 10 s: {probe:?}"
        );
        // The loop handles one batch at a time, and this one is blocked.
        let (started, finished) = (probe.started.load(SeqCst), probe.finished.load(SeqCst));
        stop_tx.send(true).unwrap();
        // The batch is blocked on the lock, so the loop must still be running.
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(probe.finished.load(SeqCst), finished, "{probe:?}");
        release_tx.send(()).unwrap();
        holder.join().unwrap();

        let ended = tokio::time::timeout(Duration::from_secs(10), task).await;
        ended
            .expect("the loop did not end within 10 s of the lock being let go")
            .expect("the loop task panicked")
            .expect("the loop ended with an error");
        // The stop let the blocked batch finish and started no other.
        assert_eq!(probe.started.load(SeqCst), started, "{probe:?}");
        assert_eq!(probe.finished.load(SeqCst), started, "{probe:?}");
    }

    /// After the stop, a batch the debouncer hands over finds the channel
    /// closed and is still counted in `emitted`.
    /// Counting only a successful send would leave `emitted` where it was.
    #[tokio::test]
    async fn a_batch_after_the_stop_is_still_counted_as_emitted() {
        use std::sync::atomic::Ordering::SeqCst;
        let kb = i549_kb("groove-watcher-until-after-stop");
        let (state, probe, armed) = probed_state(&kb.0);
        let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
        let task = tokio::spawn(run_watch_loop_until(state, stop_rx));
        assert!(
            poll_until(Duration::from_secs(10), || armed.load(SeqCst)).await,
            "the watch was not registered within 10 s"
        );
        stop_tx.send(true).unwrap();
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .expect("the loop did not end within 5 s of the stop signal")
            .expect("the loop task panicked")
            .expect("the loop ended with an error");

        let before = probe.emitted.load(SeqCst);
        std::fs::write(kb.0.join("doc.md"), "# doc\n\nwritten after the stop\n").unwrap();
        assert!(
            poll_until(Duration::from_secs(10), || probe.emitted.load(SeqCst)
                > before)
            .await,
            "no batch counted within 10 s of a write after the stop (emitted {before})"
        );
        assert_eq!(probe.started.load(SeqCst), 0);
    }

    // -----------------------------------------------------------------------
    // (#259 PR4) RemovalCause: a rename's removal decided with the locks held
    // -----------------------------------------------------------------------

    /// (kuriya 259) A row a rename took away goes even where the removal gate turns the path
    /// away (an excluded path, an extension the registry does not hold); a remove event keeps
    /// such a row, as before.
    #[test]
    fn i6_decide_renamed_away_reads_a_gate_skip_as_a_deindex() {
        let kb = i549_kb("groove-watcher-i6-gate-skip");
        let reg = Registry::defaults();
        let rules = crate::exclusion::ExclusionRules::load(&kb.0, default_exclude_dirs());
        let excluded_full = kb.0.join(".obsidian").join("x.md");

        let db = dir_rename_db(&[".obsidian/x.md"]);
        let kept = decide_removal_in_locks(
            &db,
            &kb.0,
            ".obsidian/x.md",
            &excluded_full,
            &reg,
            &rules,
            OnDisk::Gone,
            RemovalCause::RemoveEvent,
        );
        assert!(matches!(kept, RemovalInLocks::Skip), "{kept:?}");
        assert_eq!(
            db.all_document_paths().unwrap(),
            vec![".obsidian/x.md".to_string()]
        );

        let db = dir_rename_db(&[".obsidian/x.md"]);
        let dropped = decide_removal_in_locks(
            &db,
            &kb.0,
            ".obsidian/x.md",
            &excluded_full,
            &reg,
            &rules,
            OnDisk::Gone,
            RemovalCause::RenamedAway,
        );
        assert!(
            matches!(dropped, RemovalInLocks::Deindexed(Ok(true))),
            "{dropped:?}"
        );
        assert!(db.all_document_paths().unwrap().is_empty());

        let db = dir_rename_db(&["c.txt"]);
        let dropped = decide_removal_in_locks(
            &db,
            &kb.0,
            "c.txt",
            &kb.0.join("c.txt"),
            &reg,
            &rules,
            OnDisk::Gone,
            RemovalCause::RenamedAway,
        );
        assert!(
            matches!(dropped, RemovalInLocks::Deindexed(Ok(true))),
            "{dropped:?}"
        );
        assert!(db.all_document_paths().unwrap().is_empty());
    }

    /// (kuriya 259) A file back at the path a rename left, which the write gate takes, keeps
    /// its row: it is reindexed in place.
    #[test]
    fn i6_decide_renamed_away_reindexes_a_file_the_write_gate_takes() {
        let kb = i549_kb("groove-watcher-i6-reindex");
        let reg = Registry::defaults();
        let rules = crate::exclusion::ExclusionRules::load(&kb.0, default_exclude_dirs());

        let db = dir_rename_db(&["doc.md"]);
        let decided = decide_removal_in_locks(
            &db,
            &kb.0,
            "doc.md",
            &kb.0.join("doc.md"),
            &reg,
            &rules,
            OnDisk::Present,
            RemovalCause::RenamedAway,
        );
        assert!(matches!(decided, RemovalInLocks::Reindex), "{decided:?}");
        assert_eq!(db.all_document_paths().unwrap(), vec!["doc.md".to_string()]);
    }

    /// (kuriya 259) A file back at the path a rename left, which the index would not hold (an
    /// excluded path), does not keep the row.
    #[test]
    fn i6_decide_renamed_away_drops_a_present_file_the_index_would_not_hold() {
        let kb = i549_kb("groove-watcher-i6-present-excluded");
        let reg = Registry::defaults();
        let rules = crate::exclusion::ExclusionRules::load(&kb.0, default_exclude_dirs());
        let excluded_full = kb.0.join(".obsidian").join("x.md");
        std::fs::create_dir_all(excluded_full.parent().unwrap()).unwrap();
        std::fs::write(&excluded_full, "# x").unwrap();

        let db = dir_rename_db(&[".obsidian/x.md"]);
        let decided = decide_removal_in_locks(
            &db,
            &kb.0,
            ".obsidian/x.md",
            &excluded_full,
            &reg,
            &rules,
            OnDisk::Present,
            RemovalCause::RenamedAway,
        );
        assert!(
            matches!(decided, RemovalInLocks::Deindexed(Ok(true))),
            "{decided:?}"
        );
        assert!(db.all_document_paths().unwrap().is_empty());
    }

    /// (kuriya 259) A look at the path a rename left that cannot tell keeps the row, as a
    /// remove event does (kuriya 548).
    #[test]
    fn i6_decide_renamed_away_leaves_the_row_on_an_unknown_look() {
        let kb = i549_kb("groove-watcher-i6-unknown");
        let reg = Registry::defaults();
        let rules = crate::exclusion::ExclusionRules::load(&kb.0, default_exclude_dirs());

        let db = dir_rename_db(&["doc.md"]);
        let decided = decide_removal_in_locks(
            &db,
            &kb.0,
            "doc.md",
            &kb.0.join("doc.md"),
            &reg,
            &rules,
            OnDisk::Unknown(std::io::ErrorKind::PermissionDenied),
            RemovalCause::RenamedAway,
        );
        assert!(
            matches!(
                decided,
                RemovalInLocks::LeftIndexed {
                    kind: std::io::ErrorKind::PermissionDenied,
                    held_row: Ok(true),
                }
            ),
            "{decided:?}"
        );
        assert_eq!(db.all_document_paths().unwrap(), vec!["doc.md".to_string()]);
    }

    /// (kuriya 259, kuriya 566 R7) Reading a gate skip as a deindex takes no second look.
    #[test]
    fn i6_decide_renamed_away_takes_one_look() {
        let kb = i549_kb("groove-watcher-i6-one-look");
        let reg = Registry::defaults();
        let rules = crate::exclusion::ExclusionRules::load(&kb.0, default_exclude_dirs());
        let excluded_full = kb.0.join(".obsidian").join("x.md");
        std::fs::create_dir_all(excluded_full.parent().unwrap()).unwrap();
        std::fs::write(&excluded_full, "# x").unwrap();

        let db = dir_rename_db(&[".obsidian/x.md"]);
        FACTS_TAKEN.with(|n| n.set(0));
        let _ = decide_removal_in_locks(
            &db,
            &kb.0,
            ".obsidian/x.md",
            &excluded_full,
            &reg,
            &rules,
            OnDisk::Present,
            RemovalCause::RenamedAway,
        );
        assert_eq!(FACTS_TAKEN.with(|n| n.get()), 1);
    }

    /// (kuriya 259) A path a rename has already taken the row away from is not looked at: no
    /// row is there to drop, and nothing is written for it. A file there under another case of
    /// its name (`Docs/a.md` for `docs/a.md`) changes neither, on any file system.
    #[test]
    fn i6_a_renamed_away_path_with_no_row_takes_no_look() {
        let kb = i549_kb("groove-watcher-i6-no-row");
        std::fs::create_dir_all(kb.0.join("Docs")).unwrap();
        std::fs::write(kb.0.join("Docs").join("a.md"), "# a\n\nbody\n").unwrap();
        let mut state = until_state(&kb.0);
        state.db = Arc::new(Mutex::new(dir_rename_db(&["other.md"])));

        LISTINGS.with(|n| n.set(0));
        FACTS_TAKEN.with(|n| n.set(0));
        dispatch_removal(
            &state,
            "docs/a.md",
            &kb.0.join("docs/a.md"),
            RemovalCause::RenamedAway,
        );
        assert_eq!(listings(), 0);
        assert_eq!(FACTS_TAKEN.with(|n| n.get()), 0);
        assert_eq!(
            state.db.lock().unwrap().all_document_paths().unwrap(),
            vec!["other.md".to_string()]
        );
    }
}
