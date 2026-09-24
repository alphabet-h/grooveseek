//! (ADR-0023) `groove index` on Windows leaves out a file whose name Windows
//! cannot open as written: it names the file on stderr, counts it on the
//! `Done in` line, and stores no row for it.
#![cfg(windows)]

use std::path::PathBuf;
use std::process::Command;

mod common;
use common::ansi::strip_ansi;
use common::temp::TempKbLayout;

fn grooveseek_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_groove"))
}

/// Removes the scratch root through the verbatim prefix, the only spelling
/// under which `CON.md` is a file rather than the console. Declared after the
/// layout so it runs first.
struct VerbatimCleanup(PathBuf);
impl Drop for VerbatimCleanup {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

const BODY: &str = "# Title\n\nEnough content here to stand as a chunk.\n";

#[test]
fn index_leaves_out_a_reserved_device_name_and_counts_it() {
    let layout = TempKbLayout::new("groove-unspellable");
    let root = std::fs::canonicalize(layout.root()).expect("canonicalize root");
    let _cleanup = VerbatimCleanup(root.clone());
    let kb = root.join("kb");
    std::fs::write(kb.join("ok.md"), BODY).unwrap();
    std::fs::write(kb.join("CON.md"), BODY).unwrap();
    assert!(
        std::fs::metadata(kb.join("CON.md")).is_ok_and(|m| m.is_file()),
        "the fixture must hold a real file named CON.md"
    );

    let output = Command::new(grooveseek_binary())
        .arg("index")
        .arg("--kb-path")
        .arg(layout.kb())
        .output()
        .expect("failed to spawn groove index");
    let stderr = strip_ansi(&String::from_utf8_lossy(&output.stderr));
    assert!(
        output.status.success(),
        "exit {:?}\n{stderr}",
        output.status
    );
    assert!(
        stderr.contains("CON.md was skipped"),
        "the skip must be named:\n{stderr}"
    );
    assert!(
        stderr.contains(", 1 not indexed ("),
        "the Done line must count it:\n{stderr}"
    );

    let conn = rusqlite::Connection::open(layout.root().join(".groove.db")).expect("open db");
    let paths: Vec<String> = conn
        .prepare("SELECT path FROM documents ORDER BY path")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(paths, vec!["ok.md".to_string()]);
}
