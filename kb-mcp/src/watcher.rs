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

use crate::poison::{recover, recover_db};

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
/// あるかを判定する。basename を `/` 境界で判定するため、
/// 例えば `["node_modules"]` に対して `"node_modules/"` 開始や
/// `"sub/node_modules/"` 含みはヒットするが、`"node_modules-bak/"` は
/// ヒットしない。
///
/// (BU-19) 判定そのものは [`indexer::is_user_excluded_dir`] に委譲する。
/// index walk 側だけ大文字小文字を無視するようにすると、`exclude_dirs =
/// ["build"]` + ディスク上 `Build/` で **full index は skip するのに watcher が
/// 増分 index する**という食い違いが生まれる (= AU-03 で watcher だけ
/// hardcoded denylist を持っていなかったのと同型)。
fn is_under_excluded_dir(rel: &str, exclude_dirs: &[String]) -> bool {
    // 最後の要素はイベント対象そのもの。`rel` が除外ディレクトリ名と完全一致
    // する場合 (= ディレクトリ自体のイベント) だけそこも見る、という旧実装
    // (`rel == d || starts_with("{d}/") || contains("/{d}/")`) と同じ範囲。
    let comps: Vec<&str> = rel.split('/').collect();
    let dirs = if comps.len() == 1 {
        &comps[..]
    } else {
        &comps[..comps.len() - 1]
    };
    dirs.iter()
        .any(|c| indexer::is_user_excluded_dir(c, exclude_dirs))
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
                eprintln!(
                    "watcher: watching {} (debounce {}ms)",
                    kb_watch_path.display(),
                    debounce.as_millis()
                );
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
                // 両端の可否で 3 分岐する (codex P2 on PR #81)。以前は
                // 「どちらかが通れば rename」だったため、index 済みファイルを
                // `.git/` や `node_modules/` へ rename すると
                // `rename_single_file` が **denylist 配下の新パスへ DB を
                // 書き換えて残す**。除外したはずのファイルが index に居座る。
                let old_ok = should_process(&old_rel, from, state);
                let new_ok = should_process(&new_rel, to, state);
                match rename_action(old_ok, new_ok) {
                    RenameAction::Rename => dispatch_rename(state, &old_rel, &new_rel),
                    RenameAction::Deindex => dispatch_deindex(state, &old_rel),
                    RenameAction::Reindex => dispatch_reindex(state, &new_rel),
                    RenameAction::Skip => continue,
                }
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

/// 対象ファイルの拡張子が `registry` にあり、除外対象でないこと。
///
/// `WatcherState` のうち `registry` / `exclude_dirs` しか見ないので、判定本体は
/// [`should_process_parts`] に切り出してある (test から `Database` / `Embedder`
/// のダミー構築なしに **本番と同一のロジック** を叩けるようにするため)。
fn should_process(rel: &str, full: &Path, state: &WatcherState) -> bool {
    should_process_parts(rel, full, &state.registry, &state.exclude_dirs)
}

/// [`should_process`] の判定本体。
fn should_process_parts(
    rel: &str,
    full: &Path,
    registry: &Registry,
    exclude_dirs: &[String],
) -> bool {
    // 除外ディレクトリ配下は無視 (rebuild_index と同じ扱い)
    if is_under_excluded_dir(rel, exclude_dirs) {
        return false;
    }
    // ユーザ設定に関わらず常に skip する denylist (`.git` / `node_modules` 等)。
    // full-audit 2026-07-26 AU-03: `collect_source_files` (indexer) と
    // `validate_collect_md_files` (main) は適用済みだったが watcher だけ
    // 抜けており、`exclude_dirs` を絞った設定では **live watcher だけが**
    // `.git/` や `node_modules/` を index していた (`npm install` で KB 汚染)。
    if rel.split('/').any(indexer::is_hardcoded_excluded) {
        return false;
    }
    // `.kb-mcp.db*` は kb_path の外にあるので通常ヒットしないが念のため
    if rel.ends_with(".kb-mcp.db") || rel.ends_with(".kb-mcp.db-journal") {
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
    // Reindex / Deindex の両方を gate している)。
    if let Ok(meta) = std::fs::symlink_metadata(full)
        && meta.file_type().is_symlink()
    {
        return false;
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
    // (BU-20) A hard link is the same attack with no symlink to see: a second
    // name for a file that may live outside the KB, creatable without read
    // access to it and without any privilege on Windows.
    //
    // Last, for two reasons. On Windows the link count needs the file opened,
    // and there is no point opening what the extension filter already rejected.
    // On Linux every *directory* has a link count of at least two (`.` and
    // `..`), so checking earlier would refuse directory events on one platform
    // and not the other.
    //
    // Same fail-open rule as the symlink check above: a vanished file has no
    // link count, and this function gates deindexing as well as indexing.
    if crate::links::is_multiply_linked(full) {
        eprintln!("watcher: {}", crate::links::refusal_reason(full));
        return false;
    }
    true
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
    let mut embedder = recover(state.embedder.lock(), "embedder");
    let db = recover_db(state.db.lock());
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
    let db = recover_db(state.db.lock());
    match indexer::deindex_single_file(&db, rel) {
        Ok(true) => eprintln!("watcher: deindexed {rel}"),
        Ok(false) => { /* no-op: not in DB */ }
        Err(e) => eprintln!("watcher: deindex {rel} failed: {e}"),
    }
}

fn dispatch_rename(state: &WatcherState, old_rel: &str, new_rel: &str) {
    let mut embedder = recover(state.embedder.lock(), "embedder");
    let db = recover_db(state.db.lock());
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
        Ok(indexer::RenameOutcome::RenamedSizeCapped) => {
            eprintln!(
                "watcher: renamed {old_rel} -> {new_rel} (binary too large, hash check skipped)"
            );
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
        // Was a fixed name, so every concurrent run and every rerun shared one
        // directory — and the `remove_dir_all` below deleted it out from under
        // whoever else was mid-test.
        let kb = crate::test_support::unique_temp_path("kb-mcp-watcher-torel");
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
    ///
    /// full-audit 2026-07-26 (H-1): 以前はここに本番 `should_process` の
    /// 逐語コピーが置かれており、**テストがコピーだけを検証していて本番の
    /// skip ルールを変えても全部 green のまま**だった。判定本体を
    /// `should_process_parts` に切り出し、ここは委譲だけにすることで、
    /// 既存 assert を 1 文字も変えずにテストが本番経路を踏むようにした。
    fn should_process_lite(
        rel: &str,
        full: &Path,
        registry: &Registry,
        exclude_dirs: &[String],
    ) -> bool {
        should_process_parts(rel, full, registry, exclude_dirs)
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
        let kb = crate::test_support::unique_temp_path("kb-mcp-watcher-symlink");
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
                "test_symlink_is_not_indexed_by_the_watcher: symlink 作成不可のため \
                 symlink 分岐を skip (Windows の特権不足)。以下の分岐だけ検証する。"
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
        // 脅威 (KB に書ける者が、読めないファイルを kb-mcp の権限で読ませる) を
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
