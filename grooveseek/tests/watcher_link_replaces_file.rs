//! (kuriya 566) A file whose path comes to hold a link (a symlink, a junction or a hard link)
//! loses its row, whichever watcher path meets it: the index holds no document at a link, and
//! the full index drops such a row too (its walk collects no link, and its deletion pass drops
//! what it did not collect).
//!
//! Each test drives `groove serve` with the watcher on against the mock endpoint of
//! [`crate::common::embed_cli`], so nothing downloads a model, and reads the rows the server left
//! straight from its index.
//!
//! Not on macOS: FSEvents does not say which end of a rename a path is, and the debouncer drops
//! removals and renames when flags pile up on a path (kuriya trap 525). The decisions these tests
//! reach are pinned by unit tests in `watcher.rs` on all three platforms.
#![cfg(not(target_os = "macos"))]

mod common;

use common::ansi::strip_ansi;
use common::embed_cli::{Fixture, fixture};
use common::embed_mock::{hermetic, wait_until};
use common::mcp::{ServerGuard, spawn_serve_with};
use common::rename::rename_retrying;
use grooveseek::db::Database;
use std::path::Path;
use std::time::Duration;

/// How long a test waits for the watcher to reach the rows it expects.
const SETTLE: Duration = Duration::from_secs(30);
/// How long a test keeps looking after the rows were reached, to see that they stay: ten
/// debounce windows of the `debounce_ms = 300` the fixture sets.
const HOLD: Duration = Duration::from_secs(3);

/// A Markdown note titled `title` with one section holding `marker`.
fn note(title: &str, marker: &str) -> String {
    format!(
        "---\ntitle: {title}\n---\n\n## {title}\n\nThis note carries the marker {marker} in its only section.\n"
    )
}

/// A knowledge base holding `files`, indexed once, served with the watcher on.
fn served(prefix: &str, files: &[(&str, &str)]) -> (Fixture, ServerGuard) {
    served_after(prefix, files, |_| {})
}

/// [`served`], with `between` run on the knowledge base after the index and before the server
/// starts: what it changes reaches the watcher as no event.
fn served_after(
    prefix: &str,
    files: &[(&str, &str)],
    between: impl FnOnce(&Fixture),
) -> (Fixture, ServerGuard) {
    let fx = fixture(
        prefix,
        files,
        "\n[watch]\nenabled = true\ndebounce_ms = 300\n",
    );
    fx.index();
    between(&fx);
    // `spawn_serve_with(.., true, ..)` returns only after `watcher: watching`, so the edits a
    // test makes next cannot fall before the debouncer is armed.
    let (guard, _base) = spawn_serve_with(fx.kb(), &fx.config, true, |c| {
        hermetic(c, &fx.cache);
    });
    (fx, guard)
}

/// The index the server writes, opened beside it (WAL, with a busy timeout), and closed again
/// when the caller drops it.
fn open_index(fx: &Fixture) -> Database {
    let db_path = grooveseek::resolve_db_path(fx.kb());
    Database::open(&db_path.to_string_lossy()).expect("open the index")
}

/// Every document path in the index, sorted.
fn paths(fx: &Fixture) -> Vec<String> {
    let mut all = open_index(fx).all_document_paths().expect("paths");
    all.sort();
    all
}

fn stderr_lines(guard: &ServerGuard) -> Vec<String> {
    guard
        .stderr()
        .lines()
        .iter()
        .map(|l| strip_ansi(l))
        .collect()
}

/// Require the index to reach exactly `want` and to keep it for [`HOLD`], checked over and
/// over through it, so a row that goes for a moment and is put back by a later event fails too.
fn assert_settles_on(fx: &Fixture, guard: &ServerGuard, want: &[&str], what: &str) {
    let mut last = Vec::new();
    let reached = wait_until(SETTLE, || {
        last = paths(fx);
        last == want
    });
    assert!(
        reached,
        "{what}: the index never held exactly {want:?}; last seen {last:?}\nstderr:\n{}",
        stderr_lines(guard).join("\n")
    );
    let until = std::time::Instant::now() + HOLD;
    while std::time::Instant::now() < until {
        let later = paths(fx);
        assert!(
            later == want,
            "{what}: the index held {want:?}, then {later:?}\nstderr:\n{}",
            stderr_lines(guard).join("\n")
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// A link at `link` to the file `target`: a symlink on Unix, a hard link on Windows (where a
/// file symlink needs Developer Mode or elevation, and a hard link needs no privilege).
fn link_file(target: &Path, link: &Path) {
    #[cfg(unix)]
    std::os::unix::fs::symlink(target, link).expect("symlink");
    #[cfg(windows)]
    std::fs::hard_link(target, link).expect("hard links need no privilege");
}

/// (kuriya 566 #16 / #17) A directory that arrives with a link where an indexed file was drops
/// that file's row, and the regular file beside it keeps its own.
///
/// `d/` is indexed with `a.md` and `b.md`, then removed before the server starts, so its rows
/// stay and no removal of `d/` ever reaches the watcher. A copy of it is built outside the
/// knowledge base with `a.md` a link to a file also outside it, and moved in once the watcher
/// runs. The arrival's scan collects `b.md` and not the link; the row it did not collect is
/// handed to the removal path, which drops the row of a path now a link.
///
/// With the removal of `d/` delivered in the same window instead, each row under the directory
/// there again goes through the removal path and the row goes as well; this test leaves that
/// event out so that only the arrival can drop it.
#[test]
fn a_directory_that_arrives_with_a_link_in_place_of_a_file_drops_its_row() {
    let (fx, guard) = served_after(
        "i566-arrive",
        &[
            ("d/a.md", &note("Alpha", "alphamarker566")),
            ("d/b.md", &note("Beta", "betamarker566")),
            ("keep.md", &note("Keep", "keepmarker566")),
        ],
        |fx| std::fs::remove_dir_all(fx.kb().join("d")).expect("remove the indexed d/"),
    );
    assert_eq!(paths(&fx), ["d/a.md", "d/b.md", "keep.md"]);

    let outside = fx.layout.root().join("outside");
    let stage = outside.join("d");
    std::fs::create_dir_all(&stage).expect("create the staged directory");
    let target = outside.join("target.md");
    std::fs::write(&target, note("Target", "targetmarker566")).expect("write the link target");
    std::fs::write(stage.join("b.md"), note("Beta", "betamarker566")).expect("write b.md");
    link_file(&target, &stage.join("a.md"));

    rename_retrying(&stage, &fx.kb().join("d"));

    assert_settles_on(
        &fx,
        &guard,
        &["d/b.md", "keep.md"],
        "a directory arrived with a link in place of a.md",
    );
    let lines = stderr_lines(&guard);
    assert!(
        lines
            .iter()
            .any(|l| l.contains("a new directory arrived with")),
        "the arrival's scan did not run; stderr:\n{}",
        lines.join("\n")
    );
    drop(guard);
}

/// (kuriya 566 #3 / #22) An indexed file removed and made again under the same name as a hard
/// link to another file of the knowledge base loses its row.
///
/// The other name is `other.txt`, an extension no parser takes, so it holds no row of its own
/// and the end state does not hang on whether its own name sees an event. The row can go
/// through the removal of `a.md` (the removal gate passes a hard link) or through its create
/// (the write gate refuses a hard link and hands it to the removal path); both are right, and
/// the stderr printed at the end says which ran.
#[test]
fn a_file_replaced_by_a_hard_link_loses_its_row() {
    let (fx, guard) = served(
        "i566-replaced",
        &[
            ("a.md", &note("Alpha", "alphamarker566r")),
            ("keep.md", &note("Keep", "keepmarker566r")),
            ("other.txt", "the other name of the file"),
        ],
    );
    assert_eq!(paths(&fx), ["a.md", "keep.md"]);

    std::fs::remove_file(fx.kb().join("a.md")).expect("remove a.md");
    std::fs::hard_link(fx.kb().join("other.txt"), fx.kb().join("a.md"))
        .expect("hard links need no privilege");

    assert_settles_on(&fx, &guard, &["keep.md"], "a.md replaced by a hard link");
    eprintln!(
        "a_file_replaced_by_a_hard_link_loses_its_row stderr:\n{}",
        stderr_lines(&guard).join("\n")
    );
    drop(guard);
}

/// (kuriya 566 #22) A write to an indexed path that is now a hard link drops its row: the write
/// gate refuses the hard link, and the path is handed to the removal path rather than left.
///
/// `a.md` is replaced by a hard link to a file outside the knowledge base before the server
/// starts, so no removal of it reaches the watcher; the only event is the write through `a.md`
/// itself.
#[test]
fn a_write_to_a_path_now_a_hard_link_drops_its_row() {
    let (fx, guard) = served_after(
        "i566-written",
        &[
            ("a.md", &note("Alpha", "alphamarker566w")),
            ("keep.md", &note("Keep", "keepmarker566w")),
        ],
        |fx| {
            let target = fx.layout.root().join("target.md");
            std::fs::write(&target, note("Target", "targetmarker566w")).expect("write target");
            std::fs::remove_file(fx.kb().join("a.md")).expect("remove a.md");
            std::fs::hard_link(&target, fx.kb().join("a.md"))
                .expect("hard links need no privilege");
        },
    );
    assert_eq!(paths(&fx), ["a.md", "keep.md"]);

    {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(fx.kb().join("a.md"))
            .expect("open a.md to append");
        f.write_all(b"\nOne more line.\n").expect("append to a.md");
    }

    assert_settles_on(&fx, &guard, &["keep.md"], "a write to a.md, a hard link");
    drop(guard);
}

/// (kuriya 566 #22) An indexed file given a second name outside the knowledge base loses its
/// row, as the full index drops it (its walk collects no file with more than one name). `a.md`
/// itself is not written: the row goes on the event its own name sees for the change of its
/// link count, if one comes. On Linux inotify reports a change of link count as `IN_ATTRIB`
/// (inotify(7)), which reaches the watcher as a metadata change of `a.md` (not run on Linux
/// when this was written; CI is the first run).
///
/// Not on Windows: there `ReadDirectoryChangesW` reported nothing at all for `a.md` when a
/// second name was made for it outside the watched tree (seen 2026-10-28 on Windows 11, NTFS:
/// the server's stderr held only its `watching` line, and the row stayed for the full 30 s),
/// so no watcher path can reach the row; the next `groove index` drops it (kuriya 566 plan,
/// S4).
#[cfg(not(windows))]
#[test]
fn a_file_given_a_second_name_loses_its_row() {
    let (fx, guard) = served(
        "i566-second-name",
        &[
            ("a.md", &note("Alpha", "alphamarker566s")),
            ("keep.md", &note("Keep", "keepmarker566s")),
        ],
    );
    assert_eq!(paths(&fx), ["a.md", "keep.md"]);

    std::fs::hard_link(
        fx.kb().join("a.md"),
        fx.layout.root().join("second-name.md"),
    )
    .expect("hard links need no privilege");

    assert_settles_on(&fx, &guard, &["keep.md"], "a.md given a second name");
    drop(guard);
}

/// (kuriya 566 #25 / #27) A link renamed over an indexed file drops that file's row: the
/// destination now holds a link, so the text its row kept is no longer in the knowledge base.
///
/// `tmp.md` is a link to a file outside the knowledge base (a symlink on Unix, a hard link on
/// Windows) and is renamed onto `a.md`. The new end is refused as a write; before #566 only
/// the old end was dealt with and the row of `a.md` kept the overwritten text. The stderr
/// printed at the end shows which path dropped the row: on Windows (2026-10-28) the test was
/// already green with the write path of #22 alone, so there it pins the end state only.
#[test]
fn an_overwriting_rename_of_a_link_drops_the_destination_row() {
    let (fx, guard) = served(
        "i566-rename-over",
        &[
            ("a.md", &note("Alpha", "alphamarker566o")),
            ("keep.md", &note("Keep", "keepmarker566o")),
        ],
    );
    assert_eq!(paths(&fx), ["a.md", "keep.md"]);

    let target = fx.layout.root().join("target.md");
    std::fs::write(&target, note("Target", "targetmarker566o")).expect("write the link target");
    link_file(&target, &fx.kb().join("tmp.md"));
    rename_retrying(&fx.kb().join("tmp.md"), &fx.kb().join("a.md"));

    assert_settles_on(&fx, &guard, &["keep.md"], "a link renamed over a.md");
    eprintln!(
        "an_overwriting_rename_of_a_link_drops_the_destination_row stderr:\n{}",
        stderr_lines(&guard).join("\n")
    );
    drop(guard);
}
