//! Deciding whether a path may be read, and reading it through the same handle
//! the decision was made about (BU-20).
//!
//! groove already refuses symlinks in every place a file can enter the index or
//! leave it as content: the full index (`follow_links(false)` plus an
//! `is_file()` check), the watcher, `get_document`, and `validate`. The reason
//! is that whoever can write into the KB should not be able to make groove read
//! — and then hand back over `search` — a file they cannot read themselves.
//!
//! A **hard link** does the same thing and passed all of them:
//! `symlink_metadata().is_symlink()` is false, `is_file()` is true, and
//! `canonicalize()` returns the path *inside* the KB, because a hard link has no
//! target to follow — it is simply a second name for the same inode. Creating
//! one does **not** require read access to the file, and on Windows it needs no
//! privilege at all (measured on Windows 11 as a non-administrator: the link was
//! created, indexed, and its content came back in a `SearchHit`).
//!
//! What we can observe is the link *count*: more than one name means the file is
//! reachable from somewhere else, and nothing portable can tell us whether that
//! somewhere is inside the KB. So the guard is deliberately blunt — a file with
//! two names is refused, wherever the second name lives — and it is logged, so a
//! legitimately hard-linked KB (deduplicated notes, one file shared between two
//! knowledge bases) does not look like it vanished for no reason.
//!
//! ## Two questions, two entry points
//!
//! [`crate::links::is_multiply_linked`] answers *which files belong in the
//! index at all*, and it answers it about a **path**: the walk uses it to
//! decide what to collect, and the watcher to decide whether an event is worth
//! acting on. Both are bookkeeping decisions, and both need the path form — the
//! watcher in particular gates deindexing, where there is no file left to open.
//! Not collecting a file is also what *evicts* an already-indexed document that
//! gained a second name, since the deletion pass treats "not collected" as gone.
//!
//! [`crate::links::read_checked`] answers the other question — *may these bytes
//! be used* — and it answers it about a **handle**. It opens the file once and
//! takes every decision from that open file description: link count, file type
//! and size all come from one `fstat`, and the content is read from the same
//! descriptor. Nothing can be substituted in between, because after the open
//! there is no name left in the loop to substitute.
//!
//! The second entry point exists because the first is not enough on its own. The
//! walk collects paths and the bytes are read later, so a KB writer who could
//! time a rebuild used to be able to show the check an ordinary file and rename
//! a hard link over that path before it was opened — needing no power over the
//! original at all (codex P1 round 4 on PR #155).
//!
//! ## What the open refuses, and why it differs by platform
//!
//! On Unix the open carries `O_NOFOLLOW`, so a **symlink** renamed over the path
//! after the walk checked it fails the open instead of being followed. That
//! window is the same window as the hard-link one and is free to exploit there:
//! `symlink(2)` needs no privilege. It also carries `O_NONBLOCK`, because a FIFO
//! left in place of a note would otherwise block the open until a writer
//! appeared and hang the whole index run; for regular files both flags are
//! no-ops.
//!
//! On Windows the open adds nothing, deliberately. Creating a symlink there
//! requires `SeCreateSymbolicLinkPrivilege` or Developer Mode — measured on
//! Windows 11: a non-elevated `New-Item -ItemType SymbolicLink` is refused with
//! "Administrator privilege required" while the hard link in the same script
//! succeeds — so the attacker in this threat model cannot make one. And the
//! obvious symmetric guard is actively wrong: refusing reparse points would
//! refuse **every OneDrive / Dropbox placeholder**, since Files On-Demand
//! implements them as reparse points. This asymmetry is the mirror image of the
//! one BU-20 rejected, and for the same reason — each platform is guarded where
//! the primitive is actually free.
//!
//! Neither platform closes an **intermediate directory** swapped for a symlink;
//! that needs `openat2(RESOLVE_NO_SYMLINKS)`, which is Linux-only.
//!
//! ## What this still does not do (codex P1 round 2 on PR #155)
//!
//! **Link, then unlink.** Someone who can link the file in *and* remove its
//! original name — write access to the directory holding it, not read access to
//! the file — leaves the KB path as the only name, count 1, indistinguishable
//! from a file that was always there. Log rotation reaches the same state by
//! accident. A link count is the state of a file now, not where it came from.
//!
//! Remembering inodes seen at count > 1 would not close it: link and unlink can
//! both happen between two index runs, so there may be no moment at which
//! groove observes the second name at all. The only check that would close it is
//! ownership — refuse files not owned by the user running groove — and that
//! breaks a knowledge base shared between accounts, which is a product decision
//! well outside this guard.
//!
//! **The count is whatever the filesystem says.** FAT32 and exFAT have no hard
//! links and answer 1; most SMB and other network redirectors answer 1 as well,
//! whatever the truth is. A knowledge base on a USB stick or a network share
//! therefore gets no protection from either entry point, and there is no
//! portable signal that would let us say so at run time.
//!
//! So: this raises the bar for the common case (the attacker cannot delete the
//! original — `/etc/shadow`, another account's files), and **the knowledge base
//! directory is still not a security boundary**. Anything that must not be
//! readable by groove belongs outside `kb_path`, on a path groove's user cannot
//! read.
//!
//! **Fail-open on error.** A path that cannot be examined is allowed through by
//! [`crate::links::is_multiply_linked`]. Deletion events arrive after the file
//! is gone, and the gate in [`crate::watcher`] covers deindexing as well as
//! indexing -- `should_process_parts`, private to that module and so nameable
//! from here only in prose. Refusing what we cannot stat would leave deleted
//! documents in the index forever. The rule does not extend to
//! [`crate::links::read_checked`], where a file that cannot be opened yields no
//! bytes either way.

use std::fs::File;
use std::path::Path;

/// What `get_document` and `get_best_practice` tell a caller when a hard link is
/// refused.
///
/// One literal for the two moments it can happen — the pre-read validation and
/// the read itself — so the same condition cannot produce two different MCP
/// errors. It names nothing about the server's filesystem, unlike the operator
/// line in [`Refused::log_line`]: echoing an absolute path back to an
/// unauthenticated caller is what BU-23 was about.
pub(crate) const HARD_LINK_DENIED: &str =
    "Access denied: files with more than one name (hard links) are not allowed.";

/// The number of names the **already-open** file has, or `None` when that
/// cannot be determined.
///
/// Taking the count from metadata that came from the descriptor — rather than a
/// second lookup by name — is what makes it un-substitutable.
#[cfg(unix)]
fn handle_link_count(meta: &std::fs::Metadata, _file: &File) -> Option<u64> {
    use std::os::unix::fs::MetadataExt;
    Some(meta.nlink())
}

/// The number of names the **already-open** file has, or `None` when that
/// cannot be determined.
///
/// `std::fs::Metadata` does carry the count on Windows — `File::metadata` calls
/// `GetFileInformationByHandle` — but the `MetadataExt::number_of_links` that
/// would expose it is still unstable, so the same call is made again here. See
/// [`hard_link_count`] for why the path form cannot use `fs::metadata` either.
#[cfg(windows)]
fn handle_link_count(_meta: &std::fs::Metadata, file: &File) -> Option<u64> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle,
    };

    // SAFETY: `info` is a plain POD struct that the call fills in, and the
    // handle is owned by `file`, which outlives the call.
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    let ok = unsafe { GetFileInformationByHandle(file.as_raw_handle() as _, &mut info) };
    if ok == 0 {
        return None;
    }
    Some(u64::from(info.nNumberOfLinks))
}

/// The number of directory entries pointing at `path`, or `None` when that
/// cannot be determined.
#[cfg(unix)]
pub(crate) fn hard_link_count(path: &Path) -> Option<u64> {
    use std::os::unix::fs::MetadataExt;
    // `symlink_metadata`, not `metadata`: a symlink's own link count is its
    // own business, and symlinks are refused by the callers anyway.
    std::fs::symlink_metadata(path)
        .ok()
        .map(|meta| meta.nlink())
}

/// The number of directory entries pointing at `path`, or `None` when that
/// cannot be determined.
///
/// `std::os::windows::fs::MetadataExt::number_of_links` would be the obvious
/// answer, but it is still unstable (E0658 on rustc 1.93.1), and it is only
/// populated for metadata obtained from a **handle** — `walkdir`'s entries carry
/// `WIN32_FIND_DATAW`, which has no link count in it. So we open the file
/// ourselves and ask.
///
/// The open requests `FILE_READ_ATTRIBUTES` only, and shares read, write and
/// delete: a file another process is writing (an Office document being edited,
/// say) still answers. `fs::metadata` would not do instead — it falls back to
/// `FindFirstFileExW` on a sharing violation, and that fallback carries no link
/// count at all, so it would report "no count" for exactly the locked files this
/// sharing mode exists to handle.
#[cfg(windows)]
pub(crate) fn hard_link_count(path: &Path) -> Option<u64> {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    };

    let file = std::fs::OpenOptions::new()
        .access_mode(FILE_READ_ATTRIBUTES)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .open(path)
        .ok()?;
    let meta = file.metadata().ok()?;
    handle_link_count(&meta, &file)
}

/// Whether `path` is reachable under more than one name.
///
/// `false` when the count cannot be determined — see the module docs for why
/// this direction is the safe one.
pub(crate) fn is_multiply_linked(path: &Path) -> bool {
    hard_link_count(path).is_some_and(|count| count > 1)
}

/// Why a path that exists was not read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Refused {
    /// The handle reports more than one name.
    MultiplyLinked,
    /// The object opened is not a regular file, or the final path component was
    /// a symlink at the moment of the open (Unix).
    NotAPlainFile,
    /// The handle's own length is over the cap that applies here.
    TooLarge { len: u64, cap: u64 },
}

impl Refused {
    /// The operator-facing line, for stderr and `tracing`. Names the file.
    pub(crate) fn log_line(&self, path: &Path) -> String {
        match self {
            Refused::MultiplyLinked => refusal_reason(path),
            // ASCII, deliberately: this goes to stderr, and AGENTS.md keeps
            // stderr ASCII so a CP932 console does not render it as mojibake.
            // It used to carry two em dashes and reached stderr from three
            // places (`exclusion.rs`, `indexer.rs`, `server/documents.rs`)
            // before a fourth in `eval.rs` made someone look
            // (codex P1 on PR #203).
            Refused::NotAPlainFile => format!(
                "{} is not a regular file and was skipped: something other than the \
                 note that was collected (a symlink, a device, a named pipe) was in \
                 its place when it came to be read (BU-20).",
                path.display()
            ),
            Refused::TooLarge { len, cap } => format!(
                "{} is {len} bytes, over the {cap} byte limit, measured on the handle \
                 it would have been read from",
                path.display()
            ),
        }
    }

    /// The caller-facing line for MCP responses. Says nothing about the server's
    /// filesystem (BU-23).
    pub(crate) fn client_message(&self) -> &'static str {
        match self {
            Refused::MultiplyLinked => HARD_LINK_DENIED,
            Refused::NotAPlainFile => "Access denied: only regular files can be read.",
            Refused::TooLarge { .. } => "File too large.",
        }
    }
}

/// The outcome of [`read_checked`] short of an I/O failure.
#[derive(Debug)]
pub(crate) enum Content {
    Bytes(Vec<u8>),
    Refused(Refused),
}

/// Open `path`, decide from that handle whether its bytes may be used, and read
/// them from the same handle.
///
/// `Refused` is a decision and `Err` is a failure; callers must not merge them.
/// A vanished file has to keep reaching the deindexing path, while a hard link
/// must not — that is the whole distinction.
///
/// `cap` is enforced on the handle's own length, not on a `stat` of the path
/// taken earlier. The path-based caps upstream (`size_cap_exceeded`,
/// `validate_get_document_path`) still run and still produce their own messages;
/// they avoid opening a file that is obviously too big. This one is the limit
/// that cannot be swapped past — which matters precisely because the premise of
/// this whole guard is that a path can be swapped between check and use. The
/// read is bounded a second time as it runs, so a file that grows under us
/// cannot outrun the check either.
pub(crate) fn read_checked(path: &Path, cap: u64) -> std::io::Result<Content> {
    use std::io::Read;

    let file = match open_for_read(path) {
        Ok(f) => f,
        // O_NOFOLLOW reports a symlink as ELOOP. That is a refusal, not a
        // failure: the file we were told about is not the file that is there.
        Err(e) if is_symlink_open_error(&e) => {
            return Ok(Content::Refused(Refused::NotAPlainFile));
        }
        Err(e) => return Err(e),
    };

    // One `fstat`; every decision below comes out of it.
    let meta = file.metadata()?;
    if !meta.file_type().is_file() {
        return Ok(Content::Refused(Refused::NotAPlainFile));
    }
    if handle_link_count(&meta, &file).is_some_and(|count| count > 1) {
        return Ok(Content::Refused(Refused::MultiplyLinked));
    }
    let len = meta.len();
    if len > cap {
        return Ok(Content::Refused(Refused::TooLarge { len, cap }));
    }

    // Sized from the handle, like `std::fs::read` does from its own — without
    // it every read walks the doubling ladder and copies as it goes.
    let mut bytes = Vec::with_capacity(usize::try_from(len).unwrap_or(0));
    file.take(cap.saturating_add(1)).read_to_end(&mut bytes)?;
    let read = bytes.len() as u64;
    if read > cap {
        return Ok(Content::Refused(Refused::TooLarge { len: read, cap }));
    }
    Ok(Content::Bytes(bytes))
}

/// Open a path for reading, refusing to follow a symlink in its final component
/// and refusing to block on a FIFO. See the module docs for why the two
/// platforms differ.
#[cfg(unix)]
fn open_for_read(path: &Path) -> std::io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
}

/// Open a path for reading.
///
/// Plain `File::open`, which is exactly what `std::fs::read` did here before —
/// the same access mode (`GENERIC_READ`) and the same default sharing
/// (read | write | delete). So no file that could be indexed before stops being
/// readable now; only where the count comes from has changed.
#[cfg(not(unix))]
fn open_for_read(path: &Path) -> std::io::Result<File> {
    File::open(path)
}

#[cfg(unix)]
fn is_symlink_open_error(e: &std::io::Error) -> bool {
    e.raw_os_error() == Some(libc::ELOOP)
}

#[cfg(not(unix))]
fn is_symlink_open_error(_e: &std::io::Error) -> bool {
    false
}

/// The message every hard-link call site logs, so they stay in step and a
/// skipped file is never a mystery.
pub(crate) fn refusal_reason(path: &Path) -> String {
    format!(
        "{} has more than one name (hard link) and was skipped: groove cannot tell \
         whether the other name is outside the knowledge base, and a hard link is \
         how a file you cannot read gets indexed on your behalf (BU-20). Replace it \
         with a copy if it belongs in the index.",
        path.display()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Big enough not to be the thing under test in the cases that are about
    /// links rather than size.
    const NO_CAP: u64 = u64::MAX;

    /// A plain file has exactly one name; a hard link to it gives both two.
    /// Hard links need no privilege on any of the three platforms, so unlike
    /// the symlink tests this one never has to skip.
    #[test]
    fn a_second_name_is_visible_in_the_link_count() {
        let dir = crate::test_support::unique_temp_path("groove-links");
        std::fs::create_dir_all(&dir).unwrap();
        struct DirGuard(std::path::PathBuf);
        impl Drop for DirGuard {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let _guard = DirGuard(dir.clone());

        let original = dir.join("secret.txt");
        std::fs::write(&original, b"ssh-rsa AAAA...").unwrap();
        assert_eq!(
            hard_link_count(&original),
            Some(1),
            "a freshly written file has exactly one name"
        );
        assert!(!is_multiply_linked(&original));

        let link = dir.join("notes.md");
        std::fs::hard_link(&original, &link).expect("hard links need no privilege");

        assert_eq!(
            hard_link_count(&link),
            Some(2),
            "the link and its original are the same file under two names"
        );
        assert!(is_multiply_linked(&link), "the guard must see the link");
        assert!(
            is_multiply_linked(&original),
            "and must see it from the original's side too — which is the false \
             positive we accept: an existing note that someone hard-links away \
             stops being indexed"
        );

        std::fs::remove_file(&link).unwrap();
        assert_eq!(
            hard_link_count(&original),
            Some(1),
            "removing the second name must bring the file back under the limit"
        );
    }

    #[test]
    fn a_file_that_cannot_be_examined_is_not_treated_as_linked() {
        let missing = crate::test_support::unique_temp_path("groove-links-missing").join("gone.md");
        assert_eq!(hard_link_count(&missing), None);
        assert!(
            !is_multiply_linked(&missing),
            "fail-open: a deletion event must still reach deindexing"
        );
    }

    // -----------------------------------------------------------------------
    // read_checked — every decision taken on the handle the bytes come from
    // -----------------------------------------------------------------------

    /// Own the temporary tree for the tests below. Same construction rule as
    /// everywhere else in this repo (no `tempfile` crate); named differently
    /// from the guard inside `a_second_name_is_visible_in_the_link_count` so
    /// the two cannot be confused for one another.
    struct TempTree(std::path::PathBuf);

    impl TempTree {
        fn new(tag: &str) -> Self {
            let dir = crate::test_support::unique_temp_path(tag);
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
        fn join(&self, name: &str) -> std::path::PathBuf {
            self.0.join(name)
        }
    }

    impl Drop for TempTree {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn bytes_of(c: Content) -> Vec<u8> {
        match c {
            Content::Bytes(b) => b,
            Content::Refused(r) => panic!("expected bytes, got {r:?}"),
        }
    }

    fn refusal_of(c: Content) -> Refused {
        match c {
            Content::Refused(r) => r,
            Content::Bytes(b) => panic!("expected a refusal, got {} bytes", b.len()),
        }
    }

    #[test]
    fn an_ordinary_file_reads_back_byte_for_byte() {
        let tree = TempTree::new("groove-links-read-ok");
        let note = tree.join("note.md");
        std::fs::write(&note, b"# Note\n\nbody\n").unwrap();

        assert_eq!(
            bytes_of(read_checked(&note, NO_CAP).unwrap()),
            b"# Note\n\nbody\n",
            "the guard must not change what a single-named file reads as"
        );
    }

    #[test]
    fn a_second_name_refuses_the_read_from_either_side() {
        let tree = TempTree::new("groove-links-read-linked");
        let original = tree.join("secret.txt");
        std::fs::write(&original, b"ssh-rsa AAAA...").unwrap();
        let link = tree.join("notes.md");
        std::fs::hard_link(&original, &link).expect("hard links need no privilege");

        assert_eq!(
            refusal_of(read_checked(&link, NO_CAP).unwrap()),
            Refused::MultiplyLinked,
            "a hard link must not yield bytes"
        );
        assert_eq!(
            refusal_of(read_checked(&original, NO_CAP).unwrap()),
            Refused::MultiplyLinked,
            "and neither must the original — the guard sees a count, not a direction"
        );
    }

    /// The race the handle form exists to close, played out in order: the
    /// path-based check sees an ordinary file, and only *afterwards* is a hard
    /// link renamed over that path. Whoever can write into the KB decides when
    /// step 2 happens, so "in between the check and the read" is a choice they
    /// get to make (codex P1 round 4 on PR #155).
    ///
    /// The last assertion is the point of the test: `std::fs::read`, which is
    /// what every call site used before, hands the secret over.
    #[test]
    fn a_hard_link_renamed_over_the_path_after_the_check_is_still_refused() {
        let tree = TempTree::new("groove-links-toctou");
        let note = tree.join("note.md");
        std::fs::write(&note, b"ordinary note").unwrap();
        assert!(
            !is_multiply_linked(&note),
            "step 1: the walk-time check sees an ordinary file and collects it"
        );

        let secret = tree.join("secret.txt");
        std::fs::write(&secret, b"ssh-rsa AAAA...").unwrap();
        let staged = tree.join("staged.md");
        std::fs::hard_link(&secret, &staged).expect("hard links need no privilege");
        // step 2: swap a second name for the secret into the collected path.
        std::fs::rename(&staged, &note).expect("rename must replace the existing file");

        assert_eq!(
            refusal_of(read_checked(&note, NO_CAP).unwrap()),
            Refused::MultiplyLinked,
            "step 3: the read must refuse, because its count came from the handle it \
             would have read from"
        );
        assert_eq!(
            std::fs::read(&note).unwrap(),
            b"ssh-rsa AAAA...",
            "and this is what the old two-step returned instead"
        );
    }

    #[test]
    fn a_missing_file_is_an_error_and_not_a_refusal() {
        let tree = TempTree::new("groove-links-read-missing");
        let gone = tree.join("gone.md");

        let err =
            read_checked(&gone, NO_CAP).expect_err("a missing file must not read as a refusal");
        assert_eq!(
            err.kind(),
            std::io::ErrorKind::NotFound,
            "callers distinguish `could not read` from `may not read`, and a deleted \
             file has to keep reaching the deindexing path"
        );
    }

    #[test]
    fn a_directory_is_refused_rather_than_read() {
        let tree = TempTree::new("groove-links-read-dir");
        let sub = tree.join("sub");
        std::fs::create_dir_all(&sub).unwrap();

        // Opening a directory fails outright on Windows and succeeds on Unix,
        // where the file-type check is what catches it. Either answer is fine;
        // what must never happen is bytes coming back.
        if let Ok(c) = read_checked(&sub, NO_CAP) {
            assert_eq!(refusal_of(c), Refused::NotAPlainFile);
        }
    }

    /// The cap is measured on the handle, so it cannot be swapped past the way
    /// the `fs::metadata` check upstream can.
    #[test]
    fn the_cap_is_enforced_on_the_handle() {
        let tree = TempTree::new("groove-links-read-cap");
        let big = tree.join("big.md");
        std::fs::write(&big, vec![b'x'; 4096]).unwrap();

        assert_eq!(
            refusal_of(read_checked(&big, 1024).unwrap()),
            Refused::TooLarge {
                len: 4096,
                cap: 1024
            }
        );
        assert_eq!(bytes_of(read_checked(&big, 4096).unwrap()).len(), 4096);
        assert_eq!(
            bytes_of(read_checked(&big, 8192).unwrap()).len(),
            4096,
            "a cap above the file size must not truncate it"
        );
    }

    /// The two messages a refusal produces have different audiences: the log
    /// line names the file for the operator, the client line names nothing at
    /// all (BU-23).
    #[test]
    fn the_two_refusal_messages_address_different_audiences() {
        let path = Path::new("/srv/kb/notes.md");

        let log = Refused::MultiplyLinked.log_line(path);
        assert!(log.contains("notes.md") && log.contains("hard link"));
        assert_eq!(Refused::MultiplyLinked.client_message(), HARD_LINK_DENIED);

        for refused in [
            Refused::MultiplyLinked,
            Refused::NotAPlainFile,
            Refused::TooLarge { len: 9, cap: 1 },
        ] {
            assert!(
                !refused.client_message().contains("/srv/kb"),
                "a client message must not leak the server's paths: {}",
                refused.client_message()
            );
            assert!(
                refused.log_line(path).contains("notes.md"),
                "an operator message must say which file it was about"
            );
        }
    }

    /// `O_NOFOLLOW` means a symlink put in place of a collected note fails the
    /// open rather than being followed. Unix only: creating a symlink on
    /// Windows needs a privilege the attacker in this threat model does not
    /// have, and refusing reparse points there would refuse OneDrive
    /// placeholders — see the module docs.
    #[cfg(unix)]
    #[test]
    fn a_symlink_swapped_over_the_path_is_refused_rather_than_followed() {
        let tree = TempTree::new("groove-links-symlink-swap");
        let note = tree.join("note.md");
        std::fs::write(&note, b"ordinary note").unwrap();

        let secret = tree.join("secret.txt");
        std::fs::write(&secret, b"ssh-rsa AAAA...").unwrap();
        let staged = tree.join("staged.md");
        std::os::unix::fs::symlink(&secret, &staged).expect("symlinks need no privilege on unix");
        std::fs::rename(&staged, &note).expect("rename must replace the existing file");

        assert_eq!(
            refusal_of(read_checked(&note, NO_CAP).unwrap()),
            Refused::NotAPlainFile,
            "the link count would have said 1 here — following the link is the \
             thing that had to be refused"
        );
        assert_eq!(
            std::fs::read(&note).unwrap(),
            b"ssh-rsa AAAA...",
            "and this is what following it returned"
        );
    }

    /// A FIFO in place of a note must not park the index run waiting for a
    /// writer that never comes — `O_NONBLOCK` is what makes the open return.
    #[cfg(unix)]
    #[test]
    fn a_fifo_does_not_block_the_open() {
        use std::os::unix::ffi::OsStrExt;

        let tree = TempTree::new("groove-links-fifo");
        let fifo = tree.join("note.md");
        let c_path = std::ffi::CString::new(fifo.as_os_str().as_bytes()).unwrap();
        // SAFETY: a NUL-terminated path and a mode; the call touches nothing else.
        let rc = unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) };
        assert_eq!(rc, 0, "could not create the fifo to test against");

        assert_eq!(
            refusal_of(read_checked(&fifo, NO_CAP).unwrap()),
            Refused::NotAPlainFile,
            "a named pipe is not a note, and waiting on one would hang the run"
        );
    }

    /// Every refusal reaches stderr, and AGENTS.md keeps stderr ASCII.
    ///
    /// `NotAPlainFile` carried two em dashes and had done since it was
    /// written, printed from `exclusion.rs`, `indexer.rs` and
    /// `server/documents.rs` — three call sites, none of which noticed
    /// (codex P1 on PR #203). The wording is the easy thing to change back.
    ///
    /// **The path is deliberately not covered.** `AGENTS.md` scopes the rule
    /// to the words a diagnostic chooses rather than the data it names, and
    /// the reason is measured: `groove index` already prints the relative path
    /// of every file it indexes, so a note called `日本語のノート.md` reaches
    /// stderr as itself on every ordinary run. A rule that forbade that would
    /// condemn the main command's main output for the audience the project is
    /// built for, and escaping it here would make this one line inconsistent
    /// with the twelve others that name a path (codex P1 on PR #206).
    ///
    /// The path in the fixture is ASCII so that this test is about the wording
    /// only.
    #[test]
    fn every_refusal_line_is_ascii() {
        let path = Path::new("/kb/notes.md");
        for refused in [
            Refused::MultiplyLinked,
            Refused::NotAPlainFile,
            Refused::TooLarge {
                len: 4096,
                cap: 1024,
            },
        ] {
            let line = refused.log_line(path);
            assert!(
                line.is_ascii(),
                "a refusal printed to stderr must be ASCII (CP932 consoles), got: {line}"
            );
            assert!(
                refused.client_message().is_ascii(),
                "and so must the line a client sees"
            );
        }
    }
}
