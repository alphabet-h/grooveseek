//! Subprocess tests for #253: when `.groove.db` is not a SQLite database,
//! every command that opens it names the file and the remedy, and
//! `groove index --force` is that remedy -- it replaces the file instead of
//! propagating the open error.
//!
//! The corruption is the issue's own: a file whose first bytes are not the
//! SQLite header. `search`, plain `index` and `doctor` fail at the open,
//! before any embedding model is loaded, so those three need no model. The
//! `--force` case rebuilds for real and runs under the same conditions as
//! `index_frontmatter_unparsed.rs`.

use std::path::Path;
use std::process::Command;

mod common;
use common::ansi::strip_ansi;
use common::mcp::grooveseek_bin;
use common::temp::TempKbLayout;

const GARBAGE: &[u8] = b"not a sqlite database";

fn corrupt_kb(prefix: &str) -> TempKbLayout {
    let kb = TempKbLayout::new(prefix);
    kb.write(
        "note.md",
        "---\ntitle: Note\n---\n\n# Note\n\nEnough content here to stand as a chunk.\n",
    );
    std::fs::write(kb.root().join(".groove.db"), GARBAGE).expect("write garbage db");
    kb
}

fn run(kb: &Path, args: &[&str]) -> (i32, String, String) {
    let out = Command::new(grooveseek_bin())
        .args(args)
        .arg("--kb-path")
        .arg(kb)
        .output()
        .expect("spawn groove");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        strip_ansi(&String::from_utf8_lossy(&out.stderr)),
    )
}

fn db_path_text(kb: &TempKbLayout) -> String {
    kb.root().join(".groove.db").display().to_string()
}

#[test]
fn search_on_a_corrupt_index_names_the_file_and_the_remedy() {
    let kb = corrupt_kb("groove-corrupt-search");
    let (code, stdout, stderr) = run(kb.kb(), &["search", "anything"]);
    assert_ne!(
        code, 0,
        "an index that cannot be opened is a failure; stdout: {stdout}"
    );
    assert!(stdout.is_empty(), "no result is written: {stdout}");
    assert!(
        stderr.contains("Error:"),
        "the top-level error is on stderr: {stderr}"
    );
    assert!(
        stderr.contains(&db_path_text(&kb)),
        "the file lives in the parent of --kb-path, so the message has to say where: {stderr}"
    );
    assert!(
        stderr.contains("groove index") && stderr.contains("--force"),
        "the remedy is named: {stderr}"
    );
}

#[test]
fn index_without_force_refuses_and_leaves_the_file_alone() {
    let kb = corrupt_kb("groove-corrupt-index-plain");
    let (code, _, stderr) = run(kb.kb(), &["index"]);
    assert_eq!(code, 1, "without --force nothing is deleted: {stderr}");
    assert!(
        stderr.contains("--force"),
        "and the flag that would is named: {stderr}"
    );
    assert_eq!(
        std::fs::read(kb.root().join(".groove.db")).expect("read back"),
        GARBAGE,
        "the file is untouched by a run that did not ask to replace it"
    );
}

#[test]
fn index_with_force_replaces_the_file_and_rebuilds() {
    let kb = corrupt_kb("groove-corrupt-index-force");
    let (code, _, stderr) = run(kb.kb(), &["index", "--force", "--quiet"]);
    assert_eq!(
        code, 0,
        "--force is the repair, so it has to succeed: {stderr}"
    );
    assert!(
        stderr.contains("warning:") && stderr.contains(&db_path_text(&kb)),
        "replacing a file is said out loud, with its path: {stderr}"
    );
    assert!(
        stderr.contains("Done in"),
        "and the run completes: {stderr}"
    );

    let (code, stdout, stderr) = run(kb.kb(), &["status"]);
    assert_eq!(code, 0, "the rebuilt index opens: {stderr}");
    assert!(
        stdout.contains("Documents: 1"),
        "and holds the corpus again: {stdout}"
    );
}

#[test]
fn doctor_on_a_corrupt_index_exits_two_and_names_the_file() {
    let kb = corrupt_kb("groove-corrupt-doctor");
    let (code, stdout, stderr) = run(kb.kb(), &["doctor"]);
    assert_eq!(
        code, 2,
        "could not look, as opposed to found something: {stderr}"
    );
    assert!(stdout.is_empty(), "no report to print: {stdout}");
    assert!(
        stderr.contains(&db_path_text(&kb)) && stderr.contains("--force"),
        "doctor's job is to name what fixes it: {stderr}"
    );
}
