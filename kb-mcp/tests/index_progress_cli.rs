//! Subprocess tests for `kb-mcp index --quiet` / `--progress` flags.
//! Tests cover the non-TTY paths (subprocess stderr is a pipe, not a TTY).
//! TTY path (= indicatif progress bar) is verified manually.

use std::path::{Path, PathBuf};
use std::process::Command;

mod common;
use common::ansi::strip_ansi;
use common::temp::TempKbLayout;

/// 5 docs の small KB を作る (`Progress: 5/5 (100%)` anchor 用)。
/// total < 20 で step = max(1, total/20) = 1 fallback、
/// `Progress: 1/5 (20%)` から `Progress: 5/5 (100%)` まで全 5 行 emit される。
///
/// `TempKbLayout` を使う理由: `resolve_db_path` が `kb_path.parent()/.kb-mcp.db`
/// を返すため、`TempRoot` (= 1 階層) だと .kb-mcp.db が system temp_dir 直下に
/// 落ちて並列 test で SQLite lock 競合する。`TempKbLayout` は
/// `<temp>/root-unique/kb/` 構造で db が `<temp>/root-unique/.kb-mcp.db` に
/// 落ちるため test ごとに分離される。
fn build_small_kb() -> TempKbLayout {
    let kb = TempKbLayout::new("kb-mcp-progress-cli");
    for i in 1..=5 {
        kb.write(
            &format!("doc-{i:03}.md"),
            &format!("---\ntitle: Doc {i}\n---\n\n# Doc {i}\n\nSome content for doc {i}.\n"),
        );
    }
    kb
}

fn kb_mcp_binary() -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.push("target");
    if cfg!(debug_assertions) {
        p.push("debug");
    } else {
        p.push("release");
    }
    p.push(if cfg!(windows) {
        "kb-mcp.exe"
    } else {
        "kb-mcp"
    });
    p
}

fn run_index(kb: &Path, args: &[&str]) -> (String, std::process::ExitStatus) {
    let bin = kb_mcp_binary();
    let mut cmd = Command::new(&bin);
    cmd.arg("index").arg("--kb-path").arg(kb);
    for a in args {
        cmd.arg(a);
    }
    let output = cmd.output().expect("failed to spawn kb-mcp index");
    let stderr = strip_ansi(&String::from_utf8_lossy(&output.stderr));
    (stderr, output.status)
}

#[test]
fn test_index_default_emits_per_file() {
    let kb = build_small_kb();
    let (stderr, status) = run_index(kb.kb(), &[]);
    assert!(
        status.success(),
        "exit failed: {status:?}\nstderr:\n{stderr}"
    );
    assert!(
        stderr.contains("  indexed: doc-001.md"),
        "expected per-file output, got:\n{stderr}"
    );
    assert!(
        stderr.contains("Done in"),
        "expected Done in line: {stderr}"
    );
}

#[test]
fn test_index_quiet_silences_per_file() {
    let kb = build_small_kb();
    let (stderr, status) = run_index(kb.kb(), &["--quiet"]);
    assert!(
        status.success(),
        "exit failed: {status:?}\nstderr:\n{stderr}"
    );
    assert!(
        !stderr.contains("  indexed:"),
        "quiet mode must silence indexed lines, got:\n{stderr}"
    );
    assert!(
        !stderr.contains("  renamed:") && !stderr.contains("  deleted:"),
        "quiet mode must silence renamed/deleted lines"
    );
    // start / found / done は残す
    assert!(
        stderr.contains("Indexing"),
        "expected Indexing line: {stderr}"
    );
    assert!(
        stderr.contains("Found") && stderr.contains("source files"),
        "expected Found N source files: {stderr}"
    );
    assert!(stderr.contains("Done in"), "expected Done in line");
}

#[test]
fn test_index_progress_non_tty_emits_progress_lines() {
    let kb = build_small_kb();
    let (stderr, status) = run_index(kb.kb(), &["--progress"]);
    assert!(
        status.success(),
        "exit failed: {status:?}\nstderr:\n{stderr}"
    );
    // total=5, step=1 で全件 emit。100% 行を anchor として最低 1 件あること。
    assert!(
        stderr.contains("Progress: 5/5 (100%)"),
        "expected Progress: 5/5 (100%) anchor, got:\n{stderr}"
    );
    // per-file `  indexed:` は emit されない
    assert!(
        !stderr.contains("  indexed:"),
        "non-tty progress mode must not emit per-file indexed lines"
    );
}

#[test]
fn test_index_quiet_progress_conflict() {
    let kb = build_small_kb();
    let (stderr, status) = run_index(kb.kb(), &["--quiet", "--progress"]);
    assert!(!status.success(), "expected non-zero exit");
    assert_eq!(status.code(), Some(2), "clap should exit with code 2");
    assert!(
        stderr.contains("cannot be used with"),
        "expected clap mutual exclusion error, got:\n{stderr}"
    );
}
