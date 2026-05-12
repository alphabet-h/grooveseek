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
//!        │  classify events, lookup Mutex<Database> / Mutex<Embedder>
//!        ▼
//!   indexer::{reindex,deindex,rename}_single_file
//! ```
//!
//! The bridge thread is necessary because `notify-debouncer-full` ships with
//! `std::sync::mpsc` and must run synchronously. Keeping the dispatch side on
//! the tokio runtime lets us `select!` it against `service.waiting()` so the
//! MCP server and the watcher run concurrently.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use anyhow::Result;
use notify::RecursiveMode;
use notify_debouncer_full::notify::event::{EventKind, ModifyKind, RenameMode};
use notify_debouncer_full::{DebouncedEvent, new_debouncer};
use serde::Deserialize;
use tokio::sync::mpsc;

use crate::db::Database;
use crate::embedder::Embedder;
use crate::indexer;
use crate::parser::Registry;

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

/// `[watch]` セクション (`kb-mcp.toml`)。
///
/// - `enabled` 省略時: `true` (kb-mcp の値提案 = "常に fresh" を守るため)
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
    pub exclude_dirs: Vec<String>,
    pub config: WatchConfig,
    /// Set to `true` for the duration of the watch loop (feature-43 PR-2).
    /// Shared with `KbServerShared` so `/api/admin/status` can report it.
    pub watcher_active: Arc<std::sync::atomic::AtomicBool>,
}

/// `rel` (forward-slash 相対パス) が `exclude_dirs` のいずれかの配下に
/// あるかを判定する。basename 完全一致を `/` 境界で判定するため、
/// 例えば `["node_modules"]` に対して `"node_modules/"` 開始や
/// `"sub/node_modules/"` 含みはヒットするが、`"node_modules-bak/"` は
/// ヒットしない。
fn is_under_excluded_dir(rel: &str, exclude_dirs: &[String]) -> bool {
    exclude_dirs
        .iter()
        .any(|d| rel == d || rel.starts_with(&format!("{d}/")) || rel.contains(&format!("/{d}/")))
}

/// Watcher タスク本体。notify の裏スレッドから tokio channel 越しにイベントを
/// 受け取り、indexer 増分 API にディスパッチする。
///
/// `enabled = false` なら即座に `Ok(())` を返す (watcher は起動しない)。
/// タスク内部での処理エラーはログに流して次のイベントへ進む (silent drop 禁止)。
/// tokio task が panic しないよう各イベント処理は `catch_unwind` 相当の防衛線を
/// 張らない代わりに、error 経路は `eprintln!` で可視化する。
pub async fn run_watch_loop(state: WatcherState) -> Result<()> {
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

    // bridge thread: std::sync::mpsc → tokio::sync::mpsc
    // watch 初期化や watch() が失敗 (ディレクトリ削除等) した
    // 場合は指数バックオフで再試行する。30 秒以内に復帰できなければ次周で延期。
    let _bridge = std::thread::Builder::new()
        .name("kb-mcp-watcher".to_string())
        .spawn(move || {
            // 外側ループ = self-heal。debouncer ハンドルが生きている間は
            // inner parking で停止、壊れたら backoff して再構築。
            let mut backoff = Duration::from_secs(1);
            let max_backoff = Duration::from_secs(30);
            loop {
                let tx_clone = tx_async.clone();
                let debouncer_result = new_debouncer(
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
                            match tx_clone.try_send(events) {
                                Ok(()) => {}
                                Err(mpsc::error::TrySendError::Full(_)) => {
                                    eprintln!(
                                        "watcher: event channel full (capacity {WATCHER_CHANNEL_CAPACITY}); \
                                         dropping batch — handle_events is too slow or blocked. \
                                         Consider increasing kb-mcp resources or running rebuild_index manually."
                                    );
                                }
                                Err(mpsc::error::TrySendError::Closed(_)) => {
                                    // receiver (tokio task) drop → 静かに終了
                                }
                            }
                        }
                        Err(errs) => {
                            for e in errs {
                                eprintln!("watcher: debouncer error: {e:?}");
                            }
                        }
                    },
                );
                let mut debouncer = match debouncer_result {
                    Ok(d) => d,
                    Err(e) => {
                        eprintln!(
                            "watcher: failed to create debouncer: {e} (retry in {}s)",
                            backoff.as_secs()
                        );
                        std::thread::sleep(backoff);
                        backoff = (backoff * 2).min(max_backoff);
                        continue;
                    }
                };
                if let Err(e) = debouncer.watch(&kb_watch_path, RecursiveMode::Recursive) {
                    eprintln!(
                        "watcher: failed to watch {}: {e} (retry in {}s)",
                        kb_watch_path.display(),
                        backoff.as_secs()
                    );
                    drop(debouncer);
                    std::thread::sleep(backoff);
                    backoff = (backoff * 2).min(max_backoff);
                    continue;
                }
                // 成功: backoff をリセット
                backoff = Duration::from_secs(1);
                // periodic liveness probe: 30 秒ごとに kb_path の
                // 存在確認をして、ディレクトリが消えていたら debouncer を
                // drop して再構築する。inotify は親ディレクトリ削除時に
                // 無音で死ぬため明示的な polling が必要。
                let probe_interval = Duration::from_secs(30);
                loop {
                    std::thread::park_timeout(probe_interval);
                    if !kb_watch_path.exists() {
                        eprintln!(
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

    eprintln!(
        "watcher started ({:?} debounce, {:?})",
        debounce,
        state.registry.extensions()
    );

    while let Some(events) = rx_async.recv().await {
        handle_events(&state, &events);
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Internal
// ---------------------------------------------------------------------------

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

/// debounced event batch を分類して indexer に流す。
fn handle_events(state: &WatcherState, events: &[DebouncedEvent]) {
    for evt in events {
        match classify(evt) {
            Classified::Rename { from, to } => {
                let (Some(old_rel), Some(new_rel)) =
                    (to_rel(&state.kb_path, from), to_rel(&state.kb_path, to))
                else {
                    continue;
                };
                if !should_process(&new_rel, to, state) && !should_process(&old_rel, from, state) {
                    continue;
                }
                dispatch_rename(state, &old_rel, &new_rel);
            }
            Classified::Reindex(paths) => {
                for p in paths {
                    if let Some(rel) = to_rel(&state.kb_path, p)
                        && should_process(&rel, p, state)
                    {
                        dispatch_reindex(state, &rel);
                    }
                }
            }
            Classified::Deindex(paths) => {
                for p in paths {
                    if let Some(rel) = to_rel(&state.kb_path, p)
                        && should_process(&rel, p, state)
                    {
                        dispatch_deindex(state, &rel);
                    }
                }
            }
            Classified::Ignore => {}
        }
    }
}

/// 対象ファイルの拡張子が `registry` にあり、`exclude_dirs` 配下でないこと。
fn should_process(rel: &str, full: &Path, state: &WatcherState) -> bool {
    // 除外ディレクトリ配下は無視 (rebuild_index と同じ扱い)
    if is_under_excluded_dir(rel, &state.exclude_dirs) {
        return false;
    }
    // `.kb-mcp.db*` は kb_path の外にあるので通常ヒットしないが念のため
    if rel.ends_with(".kb-mcp.db") || rel.ends_with(".kb-mcp.db-journal") {
        return false;
    }
    let ext = full.extension().and_then(|e| e.to_str()).unwrap_or("");
    state
        .registry
        .extensions()
        .iter()
        .any(|e| e.eq_ignore_ascii_case(ext))
}

/// 絶対パスを kb_path 相対 (forward-slash) に変換。kb_path 外ならエラーを
/// ログに出して `None`。
fn to_rel(kb_path: &Path, full: &Path) -> Option<String> {
    match full.strip_prefix(kb_path) {
        Ok(rel) => Some(rel.to_string_lossy().replace('\\', "/")),
        Err(_) => {
            // canonicalize ズレで失敗することがある — 再度 canonicalize して再試行
            full.canonicalize().ok().and_then(|c| {
                c.strip_prefix(kb_path)
                    .ok()
                    .map(|r| r.to_string_lossy().replace('\\', "/"))
            })
        }
    }
}

fn dispatch_reindex(state: &WatcherState, rel: &str) {
    let Ok(mut embedder) = state.embedder.lock() else {
        eprintln!("watcher: embedder mutex poisoned");
        return;
    };
    let Ok(db) = state.db.lock() else {
        eprintln!("watcher: db mutex poisoned");
        return;
    };
    match indexer::reindex_single_file(
        &db,
        &mut embedder,
        &state.kb_path,
        rel,
        state.exclude_headings.as_deref(),
        &state.registry,
    ) {
        Ok(indexer::SingleResult::Updated { chunks }) => {
            eprintln!("watcher: reindexed {rel} ({chunks} chunks)");
        }
        Ok(indexer::SingleResult::Unchanged) => { /* no-op */ }
        Ok(indexer::SingleResult::Skipped { reason }) => {
            eprintln!("watcher: skipped {rel} ({reason})");
        }
        Err(e) => {
            eprintln!("watcher: reindex {rel} failed: {e}");
        }
    }
}

fn dispatch_deindex(state: &WatcherState, rel: &str) {
    let Ok(db) = state.db.lock() else {
        eprintln!("watcher: db mutex poisoned");
        return;
    };
    match indexer::deindex_single_file(&db, rel) {
        Ok(true) => eprintln!("watcher: deindexed {rel}"),
        Ok(false) => { /* no-op: not in DB */ }
        Err(e) => eprintln!("watcher: deindex {rel} failed: {e}"),
    }
}

fn dispatch_rename(state: &WatcherState, old_rel: &str, new_rel: &str) {
    let Ok(mut embedder) = state.embedder.lock() else {
        eprintln!("watcher: embedder mutex poisoned");
        return;
    };
    let Ok(db) = state.db.lock() else {
        eprintln!("watcher: db mutex poisoned");
        return;
    };
    match indexer::rename_single_file(
        &db,
        &mut embedder,
        &state.kb_path,
        old_rel,
        new_rel,
        state.exclude_headings.as_deref(),
        &state.registry,
    ) {
        Ok(indexer::RenameOutcome::Renamed) => {
            eprintln!("watcher: renamed {old_rel} -> {new_rel}");
        }
        Ok(indexer::RenameOutcome::RenamedAndReindexed { chunks }) => {
            eprintln!("watcher: renamed+reindexed {old_rel} -> {new_rel} ({chunks} chunks)");
        }
        Ok(indexer::RenameOutcome::OldPathMissing) => {
            eprintln!("watcher: rename target {old_rel} not in DB, indexed {new_rel}");
        }
        Err(e) => eprintln!("watcher: rename {old_rel} -> {new_rel} failed: {e}"),
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

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
        let kb = std::env::temp_dir().join("kb-mcp-watcher-torel");
        std::fs::create_dir_all(&kb).unwrap();
        let kb = kb.canonicalize().unwrap();
        let full = kb.join("notes").join("a.md");
        std::fs::create_dir_all(full.parent().unwrap()).unwrap();
        std::fs::write(&full, "").unwrap();
        let full = full.canonicalize().unwrap();
        assert_eq!(to_rel(&kb, &full), Some("notes/a.md".to_string()));
        let _ = std::fs::remove_dir_all(&kb);
    }

    /// `should_process` は WatcherState のうち `kb_path` / `registry` /
    /// `exclude_dirs` しか見ないので、test 用にその 3 つだけ差し込んだ
    /// 軽量判定ヘルパを用意する (`Database` / `Embedder` のダミー構築を
    /// 避けるため)。
    fn should_process_lite(
        rel: &str,
        full: &Path,
        registry: &Registry,
        exclude_dirs: &[String],
    ) -> bool {
        if is_under_excluded_dir(rel, exclude_dirs) {
            return false;
        }
        if rel.ends_with(".kb-mcp.db") || rel.ends_with(".kb-mcp.db-journal") {
            return false;
        }
        let ext = full.extension().and_then(|e| e.to_str()).unwrap_or("");
        registry
            .extensions()
            .iter()
            .any(|e| e.eq_ignore_ascii_case(ext))
    }

    fn default_exclude_dirs() -> Vec<String> {
        vec![".obsidian".to_string()]
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
        let full = Path::new("/tmp/a/.kb-mcp.db");
        assert!(!should_process_lite(
            ".kb-mcp.db",
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
}
