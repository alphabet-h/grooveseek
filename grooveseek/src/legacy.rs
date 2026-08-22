//! The name this project stopped reading, and what it still costs.
//!
//! **This module exists to be deleted. Remove it in 1.1.0**, together with the
//! two findings it feeds in [`crate::doctor`] and
//! [`crate::exclusion::ExclusionRules::ignore_only_from_bytes`], which has no
//! other caller. Nothing else imports it.
//!
//! [ADR-0007] renamed the project with no aliases and no automatic migration.
//! A `.kb-mcpignore` left in a knowledge base is therefore not read, and what
//! it used to keep out comes back into the index on the next run — **except
//! what the current rules exclude anyway**, which is most of a well-configured
//! knowledge base and none of the interesting part. Nothing said so.
//!
//! That qualification is not decoration. It is the difference between a finding
//! that names documents and one that names a filename, and [`inspect`] applies
//! it with `!current.is_excluded(...)` before naming anything.
//!
//! # Why it opens the file
//!
//! The withdrawn first attempt (PR #203) did not. It asked `symlink_metadata`
//! whether the name was taken, and everything it could say from that got weaker
//! each review round until what was left — "something with that name is there" —
//! was what `ls` says. Seven review findings shared that one root.
//!
//! This one starts from the index instead. The old file is compiled into the
//! same matcher a live `.grooveignore` gets, and asked about documents that are
//! **actually in the database**. What comes out is a list of real paths, which
//! is the thing `ls` cannot produce.
//!
//! Opening it is not a second implementation of the exclusion rule.
//! `ExclusionRules` is *called*; no part of what it decides is restated here.
//!
//! [ADR-0007]: ../../docs/decisions/0007-rename-the-project-to-grooveseek.md

use std::path::Path;

use crate::exclusion::{ExclusionRules, MAX_IGNORE_FILE_BYTES};

/// What the ignore file was called before ADR-0007.
///
/// Deliberately not next to [`crate::exclusion::IGNORE_FILE_NAME`]: that one is
/// the name the product reads, this one is a name only this module looks for,
/// and keeping them apart is what makes the removal a single-file edit.
pub(crate) const LEGACY_IGNORE_FILE_NAME: &str = ".kb-mcpignore";

/// What a knowledge base's `.kb-mcpignore` still amounts to.
#[derive(Debug)]
pub(crate) enum LegacyIgnore {
    /// There is none. The ordinary case, and there is nothing to say.
    Absent,
    /// There is one, and it could not be turned into a matcher. Carries the
    /// operator-facing reason.
    ///
    /// Distinct from `Read { still_indexed: [] }` on purpose. Reporting a check
    /// that could not run as a check that found nothing is what made a clean
    /// bill of health mean two different things (codex round 7 on PR #203).
    CannotSay(String),
    /// It was read. `still_indexed` holds the indexed documents it matches that
    /// the current rules do **not** exclude — empty when it costs nothing today.
    Read { still_indexed: Vec<String> },
}

/// Whether a name is free, holds something, or cannot be answered for.
///
/// **Three values, because the question has three answers.** A `bool` here
/// collapses "nothing is there" with "could not look", which is the distinction
/// this whole module exists to keep — and it was collapsed twice before this
/// type existed: once deciding [`LegacyIgnore::Absent`] and once choosing a
/// remedy (codex P2 rounds 4 and 5).
#[derive(Debug)]
pub(crate) enum Occupancy {
    /// Nothing is at the name.
    Free,
    /// Something is, whatever it turns out to be.
    Taken,
    /// The filesystem would not say — an ACL denial, a volume that went away.
    /// Carries the operator-facing reason.
    Unknown(String),
}

/// Ask a single name which of the three it is.
///
/// `symlink_metadata` rather than `metadata`, because it must not follow a
/// symlink: a dangling one occupies its name exactly as much as a file does.
/// Only `NotFound` means free; every other error means the answer is not known.
///
/// **One implementation, two callers** ([`inspect`] and the remedy branch in
/// [`crate::doctor`]), as `AGENTS.md` requires of a question asked twice.
pub(crate) fn occupancy(path: &Path) -> Occupancy {
    match path.symlink_metadata() {
        Ok(_) => Occupancy::Taken,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Occupancy::Free,
        Err(e) => Occupancy::Unknown(format!("{} could not be looked at: {e}", path.display())),
    }
}

/// What the name a live `.grooveignore` would occupy currently holds.
///
/// Asked by the remedy branch, which must not send an operator to `mv` onto a
/// name that is taken — that overwrites their file on Unix and fails on Windows
/// — nor onto one the filesystem would not answer for (codex P2 rounds 1 and 5).
///
/// **Not** "is there an ignore file in effect".
/// [`ExclusionRules::ignore_file_patterns`] answers `None` both when there is no
/// file and when there is one that could not be read; [`ExclusionRules::load`]
/// says so. The two questions are separate and the remedy reads both.
pub(crate) fn live_ignore_name(kb_path: &Path) -> Occupancy {
    occupancy(&kb_path.join(crate::exclusion::IGNORE_FILE_NAME))
}

/// Ask what `<kb_path>/.kb-mcpignore` would still keep out.
///
/// `indexed` is `documents.path` as stored: `kb_path`-relative and
/// forward-slashed, which is exactly the shape
/// [`ExclusionRules::is_excluded`] takes, so nothing is converted on the way in.
///
/// A path is named only when the old file matches it **and** `current` does not
/// exclude it. The second half is what makes the finding true after the remedy
/// is applied rather than before it: without it, a path that today's rules
/// already drop — one the next `groove index` would remove on its own — would
/// be blamed on a file that is no longer read.
pub(crate) fn inspect(
    kb_path: &Path,
    current: &ExclusionRules,
    indexed: &[String],
) -> LegacyIgnore {
    let path = kb_path.join(LEGACY_IGNORE_FILE_NAME);

    // Occupancy decides `Absent`, and it is asked **first**, so that every
    // other outcome below is either `Read` or `CannotSay`.
    //
    // Deriving `Absent` from a `NotFound` out of the read instead is wrong on
    // Windows: `File::open` follows a symlink, so a `.kb-mcpignore` pointing at
    // a target that has been deleted answers `NotFound` for a name that is very
    // much occupied — and this module would then report nothing found for a
    // check it never got to run (codex P2 round 4). Unix refuses the same file
    // as a symlink and reaches `CannotSay` anyway, which is how one platform
    // could have carried the hole alone. Asking here removes the difference
    // rather than special-casing it.
    //
    // A file deleted between this call and the read lands in `CannotSay` rather
    // than `Absent`. That is the safe direction: "could not say" about a file
    // that is gone costs a line, "found nothing" about a file that is there is
    // the failure this whole module exists to avoid.
    match occupancy(&path) {
        Occupancy::Free => return LegacyIgnore::Absent,
        Occupancy::Unknown(why) => return LegacyIgnore::CannotSay(why),
        Occupancy::Taken => {}
    }

    let bytes = match crate::links::read_checked(&path, MAX_IGNORE_FILE_BYTES) {
        Ok(crate::links::Content::Bytes(b)) => b,
        // The same guard the live file goes through, so a hard link or a
        // symlink is refused here for the reason it is refused there.
        Ok(crate::links::Content::Refused(r)) => return LegacyIgnore::CannotSay(r.log_line(&path)),
        Err(e) => {
            return LegacyIgnore::CannotSay(format!("{} could not be read: {e}", path.display()));
        }
    };
    let Some(legacy) = ExclusionRules::ignore_only_from_bytes(kb_path, &path, &bytes) else {
        return LegacyIgnore::CannotSay(format!(
            "{} could not be compiled into a matcher",
            path.display()
        ));
    };

    // `is_dir = false` for every one of them: `documents` holds files.
    let still_indexed = indexed
        .iter()
        .filter(|rel| legacy.is_excluded(rel, false) && !current.is_excluded(rel, false))
        .cloned()
        .collect();
    LegacyIgnore::Read { still_indexed }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempKb(std::path::PathBuf);

    impl TempKb {
        fn new(prefix: &str) -> Self {
            let p = crate::test_support::unique_temp_path(&format!("groove-legacy-{prefix}"));
            std::fs::create_dir_all(&p).expect("create temp kb");
            Self(p)
        }
        fn write_legacy(&self, body: &str) {
            std::fs::write(self.0.join(LEGACY_IGNORE_FILE_NAME), body).expect("write legacy");
        }
    }

    impl Drop for TempKb {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// The live rules a knowledge base with no `.grooveignore` has.
    fn live(kb: &TempKb, exclude_dirs: &[&str]) -> ExclusionRules {
        ExclusionRules::load(&kb.0, exclude_dirs.iter().map(|s| s.to_string()).collect())
    }

    fn named(kb: &TempKb, current: &ExclusionRules, indexed: &[&str]) -> Vec<String> {
        let indexed: Vec<String> = indexed.iter().map(|s| s.to_string()).collect();
        match inspect(&kb.0, current, &indexed) {
            LegacyIgnore::Read { still_indexed } => still_indexed,
            other => panic!("expected the old file to be read, got {other:?}"),
        }
    }

    /// The known-answer case. If this is wrong nothing below it means anything.
    #[test]
    fn a_pattern_in_the_old_file_names_the_documents_it_still_matches() {
        let kb = TempKb::new("basic");
        kb.write_legacy("drafts/\n*.log.md\n");
        let rules = live(&kb, &[]);
        assert_eq!(
            named(
                &kb,
                &rules,
                &["notes/keep.md", "drafts/wip.md", "build.log.md"]
            ),
            vec!["drafts/wip.md".to_string(), "build.log.md".to_string()],
            "both spellings match, and the document neither one names does not"
        );
    }

    /// codex round 3 on PR #203: "everything it excluded is in the index now" is
    /// false when `exclude_dirs` already covers it. Counting from the index
    /// answers that by construction — a covered path is not indexed, and one
    /// left over from an older run is dropped by the `current` half of the test.
    #[test]
    fn an_exclude_dir_that_already_covers_it_leaves_nothing_to_report() {
        let kb = TempKb::new("covered");
        kb.write_legacy("drafts/\n");
        let rules = live(&kb, &["drafts"]);
        assert!(
            named(&kb, &rules, &["drafts/wip.md", "notes/keep.md"]).is_empty(),
            "the operator's own exclude_dirs is already keeping it out"
        );
    }

    #[test]
    fn no_old_file_is_the_ordinary_case_and_says_nothing() {
        let kb = TempKb::new("absent");
        let rules = live(&kb, &[]);
        let indexed = vec!["notes/keep.md".to_string()];
        assert!(matches!(
            inspect(&kb.0, &rules, &indexed),
            LegacyIgnore::Absent
        ));
    }

    /// codex round 7 on PR #203: the check failed and the report said clean.
    ///
    /// A directory is the cheapest way to occupy the name with something that
    /// cannot be read; `exclusion.rs` uses the same technique on the live file.
    /// Which refusal the platform produces differs — Unix opens it and sees a
    /// non-file, Windows refuses the open — so this asserts the variant and not
    /// the wording.
    #[test]
    fn an_old_file_that_cannot_be_read_is_not_reported_as_nothing_found() {
        let kb = TempKb::new("unreadable");
        std::fs::create_dir_all(kb.0.join(LEGACY_IGNORE_FILE_NAME)).expect("mkdir");
        let rules = live(&kb, &[]);
        let indexed = vec!["drafts/wip.md".to_string()];
        match inspect(&kb.0, &rules, &indexed) {
            LegacyIgnore::CannotSay(why) => assert!(
                why.contains(LEGACY_IGNORE_FILE_NAME),
                "the reason has to name the file it is about, got: {why}"
            ),
            other => panic!("a check that could not run must not answer clean, got {other:?}"),
        }
    }

    /// The other way for the read to be refused, and the reason there are two
    /// tests here rather than one.
    ///
    /// The directory above takes a different route on each platform — Unix opens
    /// it and finds a non-file, Windows refuses the open outright — so it only
    /// ever exercises one of the two arms that answer `CannotSay`, and which one
    /// depends on where the suite is running. **Measured**, by making each arm
    /// answer `Absent` in turn: silencing the refusal left the directory test
    /// green on Windows. A hard link is refused as a refusal on both, which is
    /// what closes that half.
    #[test]
    fn a_hard_linked_old_file_is_refused_rather_than_read() {
        let kb = TempKb::new("hardlink");
        let elsewhere = kb.0.join("source.txt");
        std::fs::write(&elsewhere, "drafts/\n").expect("write source");
        std::fs::hard_link(&elsewhere, kb.0.join(LEGACY_IGNORE_FILE_NAME))
            .expect("hard links need no privilege");

        let rules = live(&kb, &[]);
        let indexed = vec!["drafts/wip.md".to_string()];
        match inspect(&kb.0, &rules, &indexed) {
            LegacyIgnore::CannotSay(why) => assert!(
                why.contains(LEGACY_IGNORE_FILE_NAME),
                "the reason has to name the file it is about, got: {why}"
            ),
            other => {
                panic!("a file the read guard refuses must not be reported as read, got {other:?}")
            }
        }
    }

    /// The two answers a test can construct, from the one function both callers
    /// now ask (`AGENTS.md`: one question, one implementation).
    ///
    /// **`Unknown` is not here.** Producing it needs the filesystem to refuse a
    /// `symlink_metadata` for a reason other than absence — an ACL denial, a
    /// volume going away — which no fixture here arranges. What it protects is
    /// stated in the type's own docs: neither caller may read "could not look"
    /// as "free".
    #[test]
    fn a_name_is_free_until_something_is_at_it() {
        let kb = TempKb::new("occupancy");
        let path = kb.0.join(LEGACY_IGNORE_FILE_NAME);
        assert!(
            matches!(occupancy(&path), Occupancy::Free),
            "nothing has been written yet"
        );

        kb.write_legacy("drafts/\n");
        assert!(matches!(occupancy(&path), Occupancy::Taken));

        std::fs::remove_file(&path).expect("rm");
        std::fs::create_dir_all(&path).expect("mkdir");
        assert!(
            matches!(occupancy(&path), Occupancy::Taken),
            "a directory holds the name as firmly as a file does"
        );
    }

    /// `Absent` means the name is free. Nothing that occupies it may answer
    /// `Absent`, whatever the read of it does.
    ///
    /// codex P2 round 4 found the way that broke: on Windows `File::open`
    /// follows a symlink, so a `.kb-mcpignore` whose target has been deleted
    /// answers `NotFound` — and deriving `Absent` from that read error reported
    /// nothing found for a check that never ran. `inspect` now asks occupancy
    /// first, so the read's error kind cannot decide `Absent` at all.
    ///
    /// **That specific file is not in this table**, and saying so is the point.
    /// Measured on the machine this was written on: `New-Item -ItemType
    /// SymbolicLink` answers "Administrator privilege required". Whether a CI
    /// runner could is not something anyone here found out, so the case is
    /// closed by construction rather than by a fixture, and this pins the
    /// invariant that closing it was for.
    #[test]
    fn nothing_that_occupies_the_name_is_ever_reported_absent() {
        let plain = TempKb::new("occupied-plain");
        plain.write_legacy("drafts/\n");

        let dir = TempKb::new("occupied-dir");
        std::fs::create_dir_all(dir.0.join(LEGACY_IGNORE_FILE_NAME)).expect("mkdir");

        let linked = TempKb::new("occupied-link");
        let elsewhere = linked.0.join("source.txt");
        std::fs::write(&elsewhere, "drafts/\n").expect("write source");
        std::fs::hard_link(&elsewhere, linked.0.join(LEGACY_IGNORE_FILE_NAME))
            .expect("hard links need no privilege");

        let indexed = vec!["notes/keep.md".to_string()];
        for kb in [&plain, &dir, &linked] {
            let rules = live(kb, &[]);
            assert!(
                !matches!(inspect(&kb.0, &rules, &indexed), LegacyIgnore::Absent),
                "{} holds the name, so the answer cannot be that there is none",
                kb.0.display()
            );
        }
    }

    /// The old file names plenty this knowledge base has never held. Only what
    /// is in the index is reported, because only that was measured.
    #[test]
    fn a_path_the_index_does_not_hold_is_never_named() {
        let kb = TempKb::new("notindexed");
        kb.write_legacy("secrets/\ndrafts/\n");
        let rules = live(&kb, &[]);
        assert_eq!(
            named(&kb, &rules, &["drafts/wip.md"]),
            vec!["drafts/wip.md".to_string()],
            "nothing under secrets/ is indexed, so nothing under it is named"
        );
    }

    /// `ignore_only_from_bytes` carries the hardcoded denylist, so rules built
    /// from an empty old file still answer "excluded" for `node_modules/`. That
    /// must not become a finding: the denylist is in the live rules too, so the
    /// second half of the test cancels it. This pins the argument.
    #[test]
    fn a_denylisted_directory_is_not_blamed_on_the_legacy_file() {
        let kb = TempKb::new("denylist");
        kb.write_legacy("# nothing but a comment\n");
        let rules = live(&kb, &[]);
        assert!(
            named(
                &kb,
                &rules,
                &["node_modules/pkg/readme.md", "notes/keep.md"]
            )
            .is_empty(),
            "an empty old file excludes nothing, whatever the denylist says"
        );
    }
}
