//! (kuriya issue #520, investigation only) Dump what FSEvents and
//! notify-debouncer-full hand over on macOS for the shapes the watcher e2e
//! tests drive: a directory removed file by file, and a directory renamed.
//!
//! Never fails: the dump goes straight to the stderr handle, which the test
//! harness does not capture, so it shows in the CI log whether or not the
//! test passes, and a later test binary still runs.

#![cfg(target_os = "macos")]

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread::sleep;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use notify_debouncer_full::notify::{self, RecursiveMode, Watcher};
use notify_debouncer_full::{DebounceEventResult, new_debouncer};

fn out(s: &str) {
    let mut e = std::io::stderr().lock();
    let _ = writeln!(e, "[probe-520] {s}");
}

fn scratch(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("probe520-{tag}-{}-{nanos}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    // Canonical, as `groove serve` passes kb_path: the file-id cache keys on
    // the watched spelling, and FSEvents reports /private/var.
    dir.canonicalize().unwrap()
}

fn write(root: &Path, rel: &str) {
    let p = root.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(&p, "x").unwrap();
}

/// Raw notify events (no debouncer) for `act`, after `gap` since setup.
fn raw(tag: &str, gap: Duration, setup: &dyn Fn(&Path), act: &dyn Fn(&Path)) {
    let root = scratch(tag);
    setup(&root);
    sleep(gap);
    let (tx, rx) = mpsc::channel();
    let mut w = notify::recommended_watcher(tx).unwrap();
    w.watch(&root, RecursiveMode::Recursive).unwrap();
    sleep(Duration::from_millis(300));
    out(&format!(
        "== raw {tag} gap={gap:?} root={} canonical={}",
        root.display(),
        root.canonicalize().unwrap().display()
    ));
    act(&root);
    let end = std::time::Instant::now() + Duration::from_millis(2500);
    while let Some(left) = end.checked_duration_since(std::time::Instant::now()) {
        match rx.recv_timeout(left) {
            Ok(ev) => out(&format!("raw {tag}: {ev:?}")),
            Err(_) => break,
        }
    }
    drop(w);
    let _ = std::fs::remove_dir_all(&root);
}

/// The same through notify-debouncer-full with the watcher's 500 ms window.
fn debounced(tag: &str, gap: Duration, setup: &dyn Fn(&Path), act: &dyn Fn(&Path)) {
    let root = scratch(tag);
    setup(&root);
    sleep(gap);
    let (tx, rx) = mpsc::channel::<DebounceEventResult>();
    let mut d = new_debouncer(Duration::from_millis(500), None, tx).unwrap();
    d.watch(&root, RecursiveMode::Recursive).unwrap();
    sleep(Duration::from_millis(300));
    out(&format!("== debounced {tag} gap={gap:?}"));
    act(&root);
    let end = std::time::Instant::now() + Duration::from_millis(3000);
    let mut any = false;
    while let Some(left) = end.checked_duration_since(std::time::Instant::now()) {
        match rx.recv_timeout(left) {
            Ok(Ok(evs)) => {
                for ev in evs {
                    any = true;
                    out(&format!(
                        "debounced {tag}: {:?} {:?}",
                        ev.event.kind, ev.event.paths
                    ));
                }
            }
            Ok(Err(errs)) => out(&format!("debounced {tag}: errors {errs:?}")),
            Err(_) => break,
        }
    }
    if !any {
        out(&format!("debounced {tag}: (nothing)"));
    }
    drop(d);
    let _ = std::fs::remove_dir_all(&root);
}

fn setup_sub(root: &Path) {
    write(root, "sub/a.md");
    write(root, "sub/b.md");
    write(root, "sub-other/c.md");
}

fn remove_sub(root: &Path) {
    let sub = root.join("sub");
    std::fs::remove_file(sub.join("a.md")).unwrap();
    std::fs::remove_file(sub.join("b.md")).unwrap();
    std::fs::remove_dir(&sub).unwrap();
}

fn setup_docs(root: &Path) {
    write(root, "docs/a.md");
    write(root, "docs/b.md");
}

fn rename_docs(root: &Path) {
    std::fs::rename(root.join("docs"), root.join("notes")).unwrap();
}

fn setup_one(root: &Path) {
    write(root, "one.md");
}

fn remove_one(root: &Path) {
    std::fs::remove_file(root.join("one.md")).unwrap();
}

#[test]
fn probe_520_dump_fsevents_shapes() {
    for gap in [Duration::from_millis(1500), Duration::from_secs(8)] {
        raw("remove-one", gap, &setup_one, &remove_one);
        debounced("remove-one", gap, &setup_one, &remove_one);
        raw("remove-sub", gap, &setup_sub, &remove_sub);
        debounced("remove-sub", gap, &setup_sub, &remove_sub);
        raw("rename-dir", gap, &setup_docs, &rename_docs);
        debounced("rename-dir", gap, &setup_docs, &rename_docs);
    }
}
