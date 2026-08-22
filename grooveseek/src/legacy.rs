//! The name this project stopped reading, and what it still costs.
//!
//! **This module exists to be deleted. Remove it in 1.1.0**, together with the
//! two findings it feeds in [`crate::doctor`] and
//! [`crate::exclusion::ExclusionRules::ignore_only_from_bytes`], which has no
//! other caller. Nothing else imports it.
//!
//! [ADR-0007] renamed the project with no aliases and no automatic migration.
//! A `.kb-mcpignore` left in a knowledge base is therefore not read, and
//! whatever it used to keep out comes back into the index on the next run.
//! Nothing said so.
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

/// Whether the name `<kb_path>/.grooveignore` already holds something.
///
/// **Not** "is there an ignore file in effect".
/// [`ExclusionRules::ignore_file_patterns`] answers `None` both when there is no
/// file and when there is one that could not be read — a directory, a hard link,
/// a symlink, something over the cap; [`ExclusionRules::load`] says so. A remedy
/// that branched on that would tell an operator to rename onto an occupied name,
/// which overwrites their file on Unix and fails on Windows, and either way
/// leaves the documents this module reported still indexed (codex P2 round 1).
///
/// This is the question `symlink_metadata` is actually good for. It claims
/// nothing about what is there — only that a `mv` onto it would not be free —
/// and nothing built on it here says otherwise.
pub(crate) fn live_ignore_name_is_taken(kb_path: &Path) -> bool {
    kb_path
        .join(crate::exclusion::IGNORE_FILE_NAME)
        .symlink_metadata()
        .is_ok()
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
    let bytes = match crate::links::read_checked(&path, MAX_IGNORE_FILE_BYTES) {
        Ok(crate::links::Content::Bytes(b)) => b,
        // The same guard the live file goes through, so a hard link or a
        // symlink is refused here for the reason it is refused there.
        Ok(crate::links::Content::Refused(r)) => return LegacyIgnore::CannotSay(r.log_line(&path)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return LegacyIgnore::Absent,
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
