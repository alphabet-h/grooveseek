//! (#660) [`grooveseek::indexer::IndexResult::skipped_files`] end to end: a full run lists every
//! file it counted in [`grooveseek::indexer::IndexResult::skipped`], once each, in path order,
//! with the reason it was not indexed -- and leaves out what the walk excludes on purpose.
//!
//! These call [`grooveseek::indexer::rebuild_index`] in-process against the mock endpoint of
//! [`crate::common::embed_cli`], each test body in a hermetic child of this test binary
//! ([`crate::common::in_process::run_in_hermetic_child`]), so nothing downloads a model. The
//! registry comes from the `groove.toml` the fixture writes: `[parsers].enabled` names every
//! extension a test uses, since the default registry enables Markdown only and the walk drops
//! any other extension without a reason.

mod common;

use common::embed_cli::{Fixture, REJECT_MARKER, fixture, rejects_marker};
use common::embed_mock::assert_dir_empty;
use common::in_process::{open_in_process, run_in_hermetic_child};

use grooveseek::config::Config;
use grooveseek::db::ContextMode;
use grooveseek::indexer::progress::{
    CancelToken, ProgressCallback, ProgressEvent, ProgressMode, ProgressReporter,
};
use grooveseek::indexer::{IndexResult, LimitKey, SkipReason, load_declared_schema, rebuild_index};

use std::collections::HashSet;
use std::io::Write;
use std::path::{Path, PathBuf};
use zip::write::SimpleFileOptions;

/// The text cap of T-L1: every text file is held to it, so the fixture's sizes are fixed below.
const TEXT_CAP: &str = "\n[index]\nmax_text_file_size = 16\n";

/// 32 bytes: over [`TEXT_CAP`].
const BIG_TXT: &[u8] = b"0123456789abcdef0123456789abcdef";
/// 9 bytes: under [`TEXT_CAP`], and enough for a chunk.
const OK_MD: &[u8] = b"# A\n\nhi\n";
/// "あい" in Shift_JIS: not UTF-8.
const SJIS: &[u8] = b"\x82\xa0\x82\xa2";

/// `[parsers].enabled` naming `ids`, appended after the `[embedding]` keys.
fn parsers(ids: &[&str]) -> String {
    let quoted: Vec<String> = ids.iter().map(|id| format!("\"{id}\"")).collect();
    format!("\n[parsers]\nenabled = [{}]\n", quoted.join(", "))
}

/// The knowledge base the way [`rebuild_index`] sees it: canonical.
fn canonical_kb(fx: &Fixture) -> PathBuf {
    fx.kb().canonicalize().expect("canonical kb")
}

/// Write `bytes` to `rel` under the knowledge base, creating parents.
fn write_bytes(fx: &Fixture, rel: &str, bytes: &[u8]) {
    let full = fx.kb().join(rel);
    if let Some(parent) = full.parent() {
        std::fs::create_dir_all(parent).expect("create parents");
    }
    std::fs::write(full, bytes).expect("write a fixture file");
}

/// One full run wired the way `groove index` wires it, with the reporter passed in.
fn rebuild_with(fx: &Fixture, force: bool, progress: ProgressReporter) -> IndexResult {
    let cfg = Config::load_from(&fx.config).expect("load groove.toml");
    let kb = canonical_kb(fx);
    let registry = cfg.build_parser_registry(&kb).expect("parser registry");
    let schema = load_declared_schema(&kb).expect("groove-schema.toml");
    let (db, mut embedder) = open_in_process(fx, &cfg);
    rebuild_index(
        &db,
        &mut embedder,
        &kb,
        schema,
        force,
        cfg.exclude_headings.as_deref(),
        &cfg.resolve_exclude_dirs(),
        &registry,
        progress,
        ContextMode::Off,
    )
    .expect("rebuild_index")
}

/// One full run with a quiet reporter.
fn rebuild(fx: &Fixture, force: bool) -> IndexResult {
    rebuild_with(fx, force, ProgressReporter::new(ProgressMode::Quiet))
}

/// A run whose token is set from inside the callback right after the first event `when`
/// answers `true` for.
fn rebuild_cancelling(
    fx: &Fixture,
    when: impl Fn(&ProgressEvent<'_>) -> bool + Send + 'static,
) -> IndexResult {
    let token = CancelToken::new();
    let setter = token.clone();
    let f: ProgressCallback = Box::new(move |ev| {
        if when(&ev) {
            setter.cancel();
        }
    });
    rebuild_with(
        fx,
        false,
        ProgressReporter::with_callback(f).with_cancel(token),
    )
}

/// Every `documents.path`, sorted.
fn indexed_paths(fx: &Fixture) -> Vec<String> {
    let db_path = grooveseek::resolve_db_path(fx.kb());
    grooveseek::db::Database::open(&db_path.to_string_lossy())
        .expect("open the index")
        .all_document_paths()
        .expect("document paths")
}

/// The list as `(path, reason)` pairs, for a readable assertion.
fn listed(result: &IndexResult) -> Vec<(String, SkipReason)> {
    result
        .skipped_files
        .iter()
        .map(|s| (s.path.clone(), s.reason))
        .collect()
}

fn entry(path: &str, reason: SkipReason) -> (String, SkipReason) {
    (path.to_string(), reason)
}

/// Spec §6 I1, I2, I6 and I7 on one value [`rebuild_index`] returned: the list has one entry per
/// file counted in `skipped`, no path twice, in ascending byte order, and `unspellable` is the
/// number of [`SkipReason::UnspellableName`] entries, so it never exceeds `skipped`.
fn assert_skip_ledger(result: &IndexResult) {
    assert_eq!(
        result.skipped as usize,
        result.skipped_files.len(),
        "I1: skipped == skipped_files.len(): {result:?}"
    );
    let paths: HashSet<&str> = result
        .skipped_files
        .iter()
        .map(|s| s.path.as_str())
        .collect();
    assert_eq!(
        paths.len(),
        result.skipped_files.len(),
        "I2: no path twice: {result:?}"
    );
    assert!(
        result
            .skipped_files
            .windows(2)
            .all(|w| w[0].path.as_bytes() < w[1].path.as_bytes()),
        "I6: path order: {result:?}"
    );
    let unspellable = result
        .skipped_files
        .iter()
        .filter(|s| matches!(s.reason, SkipReason::UnspellableName { .. }))
        .count();
    assert_eq!(
        result.unspellable as usize, unspellable,
        "unspellable counts the UnspellableName entries: {result:?}"
    );
    assert!(
        result.unspellable <= result.skipped,
        "I7: unspellable <= skipped: {result:?}"
    );
}

/// The T-L1 knowledge base: one file for each of five reasons, and one that indexes, under a
/// 16-byte text cap and with md / txt / pdf / xlsx enabled.
fn every_kind_kb(prefix: &str) -> Fixture {
    let extra = format!("{}{TEXT_CAP}", parsers(&["md", "txt", "pdf", "xlsx"]));
    let fx = fixture(prefix, &[], &extra);
    write_bytes(&fx, "big.txt", BIG_TXT);
    write_bytes(&fx, "empty.md", b"");
    write_bytes(&fx, "sjis.txt", SJIS);
    let pdf = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/binary/encrypted.pdf");
    std::fs::copy(pdf, fx.kb().join("encrypted.pdf")).expect("copy encrypted.pdf");
    write_bytes(&fx, "locked.xlsx", &encrypted_cfb());
    write_bytes(&fx, "ok.md", OK_MD);
    fx
}

/// What T-L1's knowledge base lists, in path order.
fn every_kind_expected() -> Vec<(String, SkipReason)> {
    vec![
        entry(
            "big.txt",
            SkipReason::TooLarge {
                size: BIG_TXT.len() as u64,
                limit: 16,
                key: LimitKey::MaxTextFileSize,
            },
        ),
        entry("empty.md", SkipReason::NoText),
        entry("encrypted.pdf", SkipReason::Encrypted),
        entry("locked.xlsx", SkipReason::Encrypted),
        entry("sjis.txt", SkipReason::NotUtf8),
    ]
}

/// A compound file the OOXML parsers read as encrypted: the CFB magic, zero padding, then the
/// UTF-16LE name "EncryptedPackage". Not a valid compound file; built here because the crate's
/// own copy is `cfg(test)`.
fn encrypted_cfb() -> Vec<u8> {
    let mut bytes = vec![0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1];
    bytes.resize(512, 0);
    bytes.extend("EncryptedPackage".encode_utf16().flat_map(u16::to_le_bytes));
    bytes.resize(1024, 0);
    bytes
}

/// A `.docx` holding one paragraph of `text`: a zip with `word/document.xml` only.
fn docx(text: &str) -> Vec<u8> {
    let xml = format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:p><w:r><w:t>{text}</w:t></w:r></w:p></w:body></w:document>"#
    );
    let mut buf = Vec::new();
    {
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
        zip.start_file("word/document.xml", SimpleFileOptions::default())
            .expect("start word/document.xml");
        zip.write_all(xml.as_bytes())
            .expect("write word/document.xml");
        zip.finish().expect("finish the zip");
    }
    buf
}

fn note(title: &str, body: &str) -> String {
    format!("---\ntitle: {title}\n---\n\n## {title}\n\n{body}\n")
}

/// T-L1: one file for each reason the run can give without a special endpoint, in path order;
/// the file that indexes is not in the list.
#[test]
fn every_kind_of_skip_is_listed_in_path_order() {
    if run_in_hermetic_child("every_kind_of_skip_is_listed_in_path_order") {
        return;
    }
    let fx = every_kind_kb("groove-660-every-kind");

    let result = rebuild(&fx, false);
    assert_eq!(listed(&result), every_kind_expected(), "{result:?}");
    assert_eq!(result.updated, 1, "only ok.md is indexed: {result:?}");
    assert_eq!(indexed_paths(&fx), vec!["ok.md".to_string()]);
    assert_skip_ledger(&result);
    assert_dir_empty(&fx.cache);
}

/// T-L2 (D1): a skipped file is read again on every run, so a second, incremental run over the
/// same knowledge base lists the same files with the same reasons.
#[test]
fn a_second_run_lists_the_same_files() {
    if run_in_hermetic_child("a_second_run_lists_the_same_files") {
        return;
    }
    let fx = every_kind_kb("groove-660-second-run");

    let first = rebuild(&fx, false);
    assert_skip_ledger(&first);
    let second = rebuild(&fx, false);
    assert_eq!(second.skipped_files, first.skipped_files, "{second:?}");
    assert_eq!(listed(&second), every_kind_expected(), "{second:?}");
    assert_skip_ledger(&second);
}

/// T-L3 (C2): what the walk leaves out on purpose -- a file `.grooveignore` names, an extension
/// no parser takes, an Office lock file, a symlink (unix) -- is neither listed nor counted. The
/// ordinary `a.docx` beside the lock file is indexed, so `~$a.docx` was left out by its `~$`
/// name and not by its extension.
#[test]
fn intended_exclusions_are_not_listed() {
    if run_in_hermetic_child("intended_exclusions_are_not_listed") {
        return;
    }
    let fx = fixture("groove-660-intended", &[], &parsers(&["md", "docx"]));
    write_bytes(&fx, ".grooveignore", b"ignored.md\n");
    write_bytes(
        &fx,
        "ignored.md",
        note("Ignored", "ignored body text").as_bytes(),
    );
    write_bytes(&fx, "n.md", note("Note", "plain body text").as_bytes());
    write_bytes(&fx, "x.png", b"\x89PNG\r\n\x1a\n");
    write_bytes(&fx, "a.docx", &docx("ordinary docx paragraph text"));
    write_bytes(&fx, "~$a.docx", b"\x0bkoshian   not a zip at all");
    #[cfg(unix)]
    std::os::unix::fs::symlink(fx.kb().join("n.md"), fx.kb().join("link.md"))
        .expect("create a symlink");

    let result = rebuild(&fx, false);
    assert!(result.skipped_files.is_empty(), "{result:?}");
    assert_eq!(result.skipped, 0, "{result:?}");
    assert_eq!(
        indexed_paths(&fx),
        vec!["a.docx".to_string(), "n.md".to_string()],
        "a.docx is indexed: the docx parser is on"
    );
    assert_skip_ledger(&result);
}

/// T-L4 (C3): both names of a hard-linked file are listed as [`SkipReason::HardLink`] and
/// counted in `skipped`. A document indexed before it became a hard link loses its row --
/// counted in `deleted` -- and is listed as well.
#[test]
fn hard_links_are_listed_and_counted() {
    if run_in_hermetic_child("hard_links_are_listed_and_counted") {
        return;
    }
    let fx = fixture("groove-660-hardlink", &[], "");
    write_bytes(&fx, "a.md", note("Alpha", "alpha body text").as_bytes());
    std::fs::hard_link(fx.kb().join("a.md"), fx.kb().join("b.md"))
        .expect("hard links need no privilege");
    write_bytes(&fx, "c.md", note("Gamma", "gamma body text").as_bytes());

    let first = rebuild(&fx, false);
    assert_eq!(
        listed(&first),
        vec![
            entry("a.md", SkipReason::HardLink),
            entry("b.md", SkipReason::HardLink),
        ],
        "{first:?}"
    );
    assert_eq!(first.skipped, 2, "{first:?}");
    assert_eq!(indexed_paths(&fx), vec!["c.md".to_string()]);
    assert_skip_ledger(&first);

    std::fs::hard_link(fx.kb().join("c.md"), fx.kb().join("d.md"))
        .expect("hard links need no privilege");
    let second = rebuild(&fx, false);
    assert!(second.deleted >= 1, "c.md's row is pruned: {second:?}");
    assert_eq!(
        listed(&second),
        vec![
            entry("a.md", SkipReason::HardLink),
            entry("b.md", SkipReason::HardLink),
            entry("c.md", SkipReason::HardLink),
            entry("d.md", SkipReason::HardLink),
        ],
        "{second:?}"
    );
    assert!(indexed_paths(&fx).is_empty());
    assert_skip_ledger(&second);
}

/// The T-L8 knowledge base: a hard link pair the walk leaves out, a file the scan declines for
/// its size, an empty note the loop skips, and three notes that index.
fn cancel_kb(prefix: &str) -> Fixture {
    let fx = fixture(prefix, &[], "\n[index]\nmax_text_file_size = 200\n");
    write_bytes(&fx, "a.md", note("Alpha", "alpha body text").as_bytes());
    write_bytes(&fx, "b.md", b"");
    write_bytes(&fx, "c.md", note("Gamma", "gamma body text").as_bytes());
    write_bytes(&fx, "d.md", note("Delta", "delta body text").as_bytes());
    write_bytes(&fx, "huge.md", &[b'x'; 300]);
    write_bytes(&fx, "x.md", note("Linked", "linked body text").as_bytes());
    std::fs::hard_link(fx.kb().join("x.md"), fx.kb().join("y.md"))
        .expect("hard links need no privilege");
    fx
}

/// T-L8: a run stopped at the end of the scan (C1 on the last file, the return it shares with
/// C2) and one stopped in the loop (C3) both return `cancelled` with a list that matches
/// `skipped`.
#[test]
fn cancelled_runs_still_match_their_count() {
    if run_in_hermetic_child("cancelled_runs_still_match_their_count") {
        return;
    }
    // The token is set by the last file's `Scanning`, so no document is read. The observer
    // sees it right after that file, which is C1 on the last file; C2 is the same return
    // reached by a token set from another thread after the observer's last look, and no
    // callback can set one in that gap, so C2 cannot be told apart from C1 here (as in
    // `tests/index_cancel.rs`). What this pins is the property both share.
    let fx = cancel_kb("groove-660-cancel-c2");
    let after_scan = rebuild_cancelling(
        &fx,
        |ev| matches!(ev, ProgressEvent::Scanning { done, total } if done == total),
    );
    assert!(after_scan.cancelled, "{after_scan:?}");
    assert_eq!(after_scan.updated, 0, "{after_scan:?}");
    let huge = SkipReason::TooLarge {
        size: 300,
        limit: 200,
        key: LimitKey::MaxTextFileSize,
    };
    assert_eq!(
        listed(&after_scan),
        vec![
            entry("huge.md", huge),
            entry("x.md", SkipReason::HardLink),
            entry("y.md", SkipReason::HardLink),
        ],
        "the walk's and the scan's entries, nothing from the loop: {after_scan:?}"
    );
    assert_skip_ledger(&after_scan);

    // C3: the token is set by the first document's event.
    let fx = cancel_kb("groove-660-cancel-c3");
    let mid_loop = rebuild_cancelling(&fx, |ev| matches!(ev, ProgressEvent::Indexed { .. }));
    assert!(mid_loop.cancelled, "{mid_loop:?}");
    assert_eq!(mid_loop.updated, 1, "{mid_loop:?}");
    let got = listed(&mid_loop);
    for want in [
        entry("huge.md", huge),
        entry("x.md", SkipReason::HardLink),
        entry("y.md", SkipReason::HardLink),
    ] {
        assert!(got.contains(&want), "{want:?} missing: {mid_loop:?}");
    }
    assert_skip_ledger(&mid_loop);
}

/// T-L9: a file the endpoint refuses with HTTP 413 is listed as
/// [`SkipReason::EmbedRejected`] carrying that status.
#[test]
fn an_endpoint_refusal_is_listed_with_its_status() {
    if run_in_hermetic_child("an_endpoint_refusal_is_listed_with_its_status") {
        return;
    }
    let fx = fixture("groove-660-refusal", &[], "");
    write_bytes(
        &fx,
        "bad.md",
        note("Bad", &format!("a body holding {REJECT_MARKER} here")).as_bytes(),
    );
    write_bytes(&fx, "good.md", note("Good", "plain body text").as_bytes());
    fx.answer_with(rejects_marker(413));

    let result = rebuild(&fx, false);
    assert_eq!(
        listed(&result),
        vec![entry(
            "bad.md",
            SkipReason::EmbedRejected { http_status: 413 }
        )],
        "{result:?}"
    );
    assert_eq!(indexed_paths(&fx), vec!["good.md".to_string()]);
    assert_skip_ledger(&result);
}

/// Review Focus 1: an indexed document whose bytes stop being UTF-8 is listed as
/// [`SkipReason::NotUtf8`], and its row stays (`total_documents` does not move).
#[test]
fn a_document_that_turns_unreadable_is_listed_and_keeps_its_row() {
    if run_in_hermetic_child("a_document_that_turns_unreadable_is_listed_and_keeps_its_row") {
        return;
    }
    let fx = fixture("groove-660-turns-unreadable", &[], "");
    write_bytes(&fx, "a.md", note("Alpha", "alpha body text").as_bytes());
    let first = rebuild(&fx, false);
    assert!(first.skipped_files.is_empty(), "{first:?}");
    assert_eq!(first.total_documents, 1, "{first:?}");

    write_bytes(&fx, "a.md", SJIS);
    let second = rebuild(&fx, false);
    assert_eq!(
        listed(&second),
        vec![entry("a.md", SkipReason::NotUtf8)],
        "{second:?}"
    );
    assert_eq!(
        second.total_documents, first.total_documents,
        "the row is kept: {second:?}"
    );
    assert_eq!(indexed_paths(&fx), vec!["a.md".to_string()]);
    assert_skip_ledger(&second);
}

/// Review Focus 2: once the user fixes a file the last run could not index, the next run
/// indexes it and lists nothing.
#[test]
fn a_file_fixed_between_runs_leaves_the_list() {
    if run_in_hermetic_child("a_file_fixed_between_runs_leaves_the_list") {
        return;
    }
    let fx = fixture("groove-660-fixed", &[], "");
    write_bytes(&fx, "a.md", b"");
    let first = rebuild(&fx, false);
    assert_eq!(
        listed(&first),
        vec![entry("a.md", SkipReason::NoText)],
        "{first:?}"
    );
    assert_skip_ledger(&first);

    write_bytes(&fx, "a.md", note("Alpha", "alpha body text").as_bytes());
    let second = rebuild(&fx, false);
    assert!(second.skipped_files.is_empty(), "{second:?}");
    assert_eq!(second.updated, 1, "{second:?}");
    assert_skip_ledger(&second);
}

/// Review Focus 5: a forced run empties the index before it reads, a different path, and still
/// lists the same files with the same reasons as an incremental one.
#[test]
fn a_forced_run_lists_the_same_files() {
    if run_in_hermetic_child("a_forced_run_lists_the_same_files") {
        return;
    }
    let fx = every_kind_kb("groove-660-forced");

    let incremental = rebuild(&fx, false);
    assert_skip_ledger(&incremental);
    let forced = rebuild(&fx, true);
    assert_eq!(
        forced.skipped_files, incremental.skipped_files,
        "{forced:?}"
    );
    assert_eq!(listed(&forced), every_kind_expected(), "{forced:?}");
    assert_skip_ledger(&forced);
}
