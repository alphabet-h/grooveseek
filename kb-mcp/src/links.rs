//! Counting the names a file has, so a hard link cannot smuggle content in
//! (BU-20).
//!
//! kb-mcp already refuses symlinks in every place a file can enter the index or
//! leave it as content: the full index (`follow_links(false)` plus an
//! `is_file()` check), the watcher, and `get_document`. The reason is that
//! whoever can write into the KB should not be able to make kb-mcp read — and
//! then hand back over `search` — a file they cannot read themselves.
//!
//! A **hard link** does the same thing and passed all three:
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
//! ## What this does not do (codex P1 round 2 on PR #155)
//!
//! A link count is a property of *now*, not of provenance. Someone who can both
//! link the file in **and remove its original name** — which needs write access
//! to the directory holding it, not read access to the file — leaves the KB path
//! as the only name, count 1, indistinguishable from a file that was always
//! there. Log rotation does the same thing by accident.
//!
//! Remembering inodes seen at count > 1 would not close it either: link and
//! unlink can both happen between two index runs, so there may be no moment at
//! which kb-mcp observes the second name at all. The only check that would close
//! it is ownership — refuse files not owned by the user running kb-mcp — and
//! that breaks a knowledge base shared between accounts, which is a product
//! decision well outside this guard.
//!
//! So: this raises the bar for the common case (the attacker cannot delete the
//! original — `/etc/shadow`, another account's files), and **the knowledge base
//! directory is still not a security boundary**. Anything that must not be
//! readable by kb-mcp belongs outside `kb_path`, on a path kb-mcp's user cannot
//! read.
//!
//! **Fail-open on error.** A file that cannot be examined is allowed through.
//! Deletion events arrive after the file is gone, and `should_process_parts`
//! gates deindexing as well as indexing: refusing what we cannot stat would
//! leave deleted documents in the index forever.

use std::path::Path;

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
/// say) still answers, where `File::open` would fail with a sharing violation.
#[cfg(windows)]
pub(crate) fn hard_link_count(path: &Path) -> Option<u64> {
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ,
        FILE_SHARE_WRITE, GetFileInformationByHandle,
    };

    let file = std::fs::OpenOptions::new()
        .access_mode(FILE_READ_ATTRIBUTES)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .open(path)
        .ok()?;

    // SAFETY: `info` is a plain POD struct that the call fills in, and the
    // handle is owned by `file`, which outlives the call.
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    let ok = unsafe { GetFileInformationByHandle(file.as_raw_handle() as _, &mut info) };
    if ok == 0 {
        return None;
    }
    Some(u64::from(info.nNumberOfLinks))
}

/// Whether `path` is reachable under more than one name.
///
/// `false` when the count cannot be determined — see the module docs for why
/// this direction is the safe one.
pub(crate) fn is_multiply_linked(path: &Path) -> bool {
    hard_link_count(path).is_some_and(|count| count > 1)
}

/// The message every call site logs, so the three of them stay in step and a
/// skipped file is never a mystery.
pub(crate) fn refusal_reason(path: &Path) -> String {
    format!(
        "{} has more than one name (hard link) and was skipped: kb-mcp cannot tell \
         whether the other name is outside the knowledge base, and a hard link is \
         how a file you cannot read gets indexed on your behalf (BU-20). Replace it \
         with a copy if it belongs in the index.",
        path.display()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A plain file has exactly one name; a hard link to it gives both two.
    /// Hard links need no privilege on any of the three platforms, so unlike
    /// the symlink tests this one never has to skip.
    #[test]
    fn a_second_name_is_visible_in_the_link_count() {
        let dir = crate::test_support::unique_temp_path("kb-mcp-links");
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
        let missing = crate::test_support::unique_temp_path("kb-mcp-links-missing").join("gone.md");
        assert_eq!(hard_link_count(&missing), None);
        assert!(
            !is_multiply_linked(&missing),
            "fail-open: a deletion event must still reach deindexing"
        );
    }
}
