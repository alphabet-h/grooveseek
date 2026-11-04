//! (AX-19) `watcher_dir_rename/5000_rows`: the running watcher following a directory rename
//! that moves 5000 indexed documents.
//!
//! The file system reports only the directory; the watcher lists the rows under the old path
//! and follows each one's rename (feature-68), re-reading the file and reusing its stored
//! vectors (AX-12 / AX-33). That is per-row work in a path that otherwise sees one event, so
//! it is the place a regression scales with the directory and not with the change.
//!
//! This drives the real watch loop ([`grooveseek::watcher::run_watch_loop_until`]) over a
//! temporary KB rather than calling the dispatch functions, which are private. One iteration
//! renames the directory to the other of two names and is timed from the rename until every
//! row sits under the new name and every batch the debouncer handed over has finished
//! ([`WatchBatchProbe`]). The debounce window is part of that time; it is set short here
//! ([`DEBOUNCE_MS`]) so the per-row work dominates, and is a constant either way.
//!
//! The embedder is [`Embedder::stub_for_tests`] (the `test-helpers` feature, hence
//! `required-features` on this target). Following a rename embeds nothing, and the number of
//! inputs the embedder was handed during one rename is printed as a `bench-count:` line for
//! the nightly summary, so a regression that starts re-embedding the moved documents shows as
//! a count and not only as a slower time against the stub.
//!
//! The nightly job measures this on Linux only (inotify). The watcher's end-to-end tests cover
//! the other platforms; FSEvents on macOS folds a path's flags into one record (kuriya trap
//! 525), which makes a timed loop there a measurement of that backend's timing rather than of
//! the watcher.

#[path = "../tests/common/temp.rs"]
#[allow(dead_code, unused_imports)]
mod temp;

use criterion::{Criterion, SamplingMode, criterion_group, criterion_main};
use grooveseek::db::{ContextMode, Database};
use grooveseek::embedder::Embedder;
use grooveseek::exclusion::ExclusionRules;
use grooveseek::indexer::progress::{ProgressMode, ProgressReporter};
use grooveseek::indexer::{self};
use grooveseek::parser::Registry;
use grooveseek::server::WatchStatusFn;
use grooveseek::watcher::{WatchBatchProbe, WatchConfig, WatcherState, run_watch_loop_until};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use temp::TempKbLayout;

/// Documents in the renamed directory.
const ROWS: usize = 5000;
/// The watcher's debounce window for this bench, in milliseconds (production's default is 500).
const DEBOUNCE_MS: u64 = 100;
/// The stub's vector length: BGE-small's, so the stored vectors are the size a real index has.
const DIMENSION: usize = 384;
/// The two names the directory alternates between, under `docs/`.
const NAMES: [&str; 2] = ["docs/team-a", "docs/team-b"];
/// How long one rename may take to settle before the bench gives up.
const SETTLE_LIMIT: Duration = Duration::from_secs(300);
/// How often the rows are looked at while a rename settles.
const POLL: Duration = Duration::from_millis(10);

fn note(i: usize) -> String {
    format!(
        "---\ntitle: Meeting note {i}\n---\n\n## Summary\n\nNote {i} records what was decided \
         in meeting number {i}, who owns each follow-up, and when it is due.\n"
    )
}

/// `(rows under from, rows under to)`, read under the database lock.
fn rows(db: &Mutex<Database>, from: &str, to: &str) -> (usize, usize) {
    let db = db.lock().expect("db lock");
    (
        db.document_paths_under(from).expect("list rows").len(),
        db.document_paths_under(to).expect("list rows").len(),
    )
}

/// Rename `from` to `to` on disk and wait until the watcher has followed it. Returns the time
/// from the rename to the settled state.
fn rename_and_settle(
    kb: &TempKbLayout,
    db: &Mutex<Database>,
    probe: &WatchBatchProbe,
    from: &str,
    to: &str,
) -> Duration {
    let start = Instant::now();
    std::fs::rename(kb.kb().join(from), kb.kb().join(to)).expect("rename the directory");
    loop {
        let moved = rows(db, from, to) == (0, ROWS);
        let emitted = probe.emitted.load(Ordering::SeqCst);
        let finished = probe.finished.load(Ordering::SeqCst);
        if moved && finished >= emitted {
            return start.elapsed();
        }
        assert!(
            start.elapsed() < SETTLE_LIMIT,
            "the watcher did not follow the rename of {from} to {to} within {SETTLE_LIMIT:?}: \
             rows (old, new) = {:?}, batches emitted {emitted} finished {finished}",
            rows(db, from, to)
        );
        std::thread::sleep(POLL);
    }
}

fn bench_watcher_dir_rename(c: &mut Criterion) {
    let kb = TempKbLayout::new("groove-bench-watcher-dir-rename");
    for i in 0..ROWS {
        kb.write(&format!("{}/note-{i:05}.md", NAMES[0]), &note(i));
    }
    let db_path = kb.root().join(".groove.db");
    let db = Database::open(&db_path.to_string_lossy()).expect("open the index");
    let mut embedder = Embedder::stub_for_tests(DIMENSION);
    db.verify_embedding_meta(embedder.model_id(), DIMENSION as u32)
        .expect("embedding meta");
    let registry = Registry::defaults();
    let first = indexer::rebuild_index(
        &db,
        &mut embedder,
        kb.kb(),
        None,
        false,
        None,
        &[],
        &registry,
        ProgressReporter::new(ProgressMode::Quiet),
        ContextMode::Off,
    )
    .expect("initial rebuild_index");
    assert_eq!(
        first.total_documents as usize, ROWS,
        "every note is indexed"
    );

    // The watcher's paths are the canonical KB path, as `groove serve` passes them.
    let kb_path = kb.kb().canonicalize().expect("canonical kb path");
    let db = Arc::new(Mutex::new(db));
    let embedder = Arc::new(Mutex::new(embedder));
    let probe = Arc::new(WatchBatchProbe::default());
    let registered = Arc::new(AtomicBool::new(false));
    let watch_status: WatchStatusFn = {
        let registered = Arc::clone(&registered);
        Arc::new(move |cause| {
            if cause.is_none() {
                registered.store(true, Ordering::SeqCst);
            }
        })
    };
    let state = WatcherState {
        kb_path: kb_path.clone(),
        db: Arc::clone(&db),
        embedder: Arc::clone(&embedder),
        registry: Arc::new(registry),
        exclude_headings: None,
        rules: ExclusionRules::load(&kb_path, Vec::new()),
        config: WatchConfig {
            enabled: true,
            debounce_ms: DEBOUNCE_MS,
        },
        watcher_active: Arc::new(AtomicBool::new(false)),
        watch_status: Some(watch_status),
        watch_backoff: WatcherState::DEFAULT_WATCH_BACKOFF,
        batch_probe: Some(Arc::clone(&probe)),
    };

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("tokio runtime");
    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    let watch = runtime.spawn(run_watch_loop_until(state, stop_rx));

    let start = Instant::now();
    while !registered.load(Ordering::SeqCst) {
        assert!(
            start.elapsed() < Duration::from_secs(60),
            "the watch was not registered within 60 s"
        );
        std::thread::sleep(POLL);
    }

    // Checked once before timing: the rows follow, and following them embeds nothing.
    let before = embedder.lock().expect("embedder lock").texts_embedded();
    let took = rename_and_settle(&kb, &db, &probe, NAMES[0], NAMES[1]);
    let sent = embedder.lock().expect("embedder lock").texts_embedded() - before;
    println!(
        "bench-count: watcher_dir_rename/5000_rows: rows={ROWS} debounce_ms={DEBOUNCE_MS} \
         first_rename_ms={} texts_sent_to_embedder={sent} batches={}",
        took.as_millis(),
        probe.finished.load(Ordering::SeqCst)
    );
    assert_eq!(sent, 0, "following a directory rename embeds nothing");
    let mut at = 1;

    let mut group = c.benchmark_group("watcher_dir_rename");
    // One iteration takes seconds: the same number of iterations per sample, and the fewest
    // samples criterion takes.
    group.sampling_mode(SamplingMode::Flat);
    group.sample_size(10);
    group.warm_up_time(Duration::from_secs(1));
    group.measurement_time(Duration::from_secs(30));
    group.bench_function(format!("{ROWS}_rows"), |b| {
        b.iter_custom(|iters| {
            let mut total = Duration::ZERO;
            for _ in 0..iters {
                let (from, to) = (NAMES[at], NAMES[1 - at]);
                total += rename_and_settle(&kb, &db, &probe, from, to);
                at = 1 - at;
            }
            total
        });
    });
    group.finish();

    stop_tx.send(true).expect("stop the watch loop");
    // The loop returns at its next batch boundary; nudge it with one more event in case it is
    // waiting on the channel, then wait for it. A loop that ended in an error fails the bench;
    // one still running after the wait is left to the runtime's shutdown, since the numbers
    // above are already taken.
    kb.write("stop.md", "stop\n");
    let stopped =
        runtime.block_on(async { tokio::time::timeout(Duration::from_secs(30), watch).await });
    if let Ok(joined) = stopped {
        joined.expect("watch task").expect("watch loop");
    }
    runtime.shutdown_timeout(Duration::from_secs(5));
}

criterion_group!(benches, bench_watcher_dir_rename);
criterion_main!(benches);
