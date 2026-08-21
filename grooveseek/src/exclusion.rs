//! One place that decides whether a knowledge-base path is out of scope.
//!
//! # Why this is a single type
//!
//! Three call sites walk or watch the knowledge base — the full index walk
//! ([`crate::indexer::collect_source_files`]), the `validate` walk
//! (`validate_collect_md_files` in `src/main.rs`) and the live watcher
//! ([`crate::watcher`]) — and they have drifted apart twice. AU-03 landed with
//! the watcher missing the hardcoded denylist the other two applied, so a
//! narrowed `exclude_dirs` let `npm install` pour `node_modules/` into the
//! index. BU-19 landed with two of the three switched to case-insensitive
//! matching, so the full index skipped a `Build/` that the watcher went on
//! indexing — a state worse than before that fix.
//!
//! Adding `.grooveignore` on top of two existing layers is exactly how a third
//! one happens, so the layers are not three checks a caller remembers to make
//! in the right order. They are one method.
//!
//! # What it is not
//!
//! **The boundary here is the index, not access.** A file this type excludes is
//! never indexed, so it can never surface through `search` or
//! `get_connection_graph` — those read chunks out of the database and never
//! touch the filesystem. It is still readable through `get_document` by a
//! caller who knows its path, exactly as a file under `exclude_dirs` has always
//! been (see `validate_get_document_path` in `src/server.rs`, and the
//! `document_in_excluded_dir_is_still_readable` test that pins it). Anything
//! that must not be readable by groove belongs outside `kb_path`.
//!
//! That is a deliberate limit rather than an oversight: whoever can write into
//! the knowledge base can also delete `.grooveignore`, so a rule that lives
//! inside the tree it protects cannot be the thing that protects it.
//!
//! # Why the ancestor walk is written out here
//!
//! [`ignore`] offers `Gitignore::matched_path_or_any_parents`, which looks like
//! it does this job. It does not do the same job. Measured on 0.4.33:
//!
//! ```text
//! patterns: ["logs/", "!logs/important.md"]
//!   matched("logs", is_dir = true)                     -> Ignore
//!   matched_path_or_any_parents("logs/important.md")   -> Whitelist
//! ```
//!
//! A walk never sees `logs/important.md`, because it stopped at `logs/` and did
//! not descend — that is git's own rule, that a file under an ignored directory
//! cannot be brought back with `!`. So the walk's answer is "excluded" while
//! `matched_path_or_any_parents` says "kept", and a watcher built on the latter
//! would index a file the full index drops. Using it would have introduced the
//! very drift this module exists to prevent, at the level of which API was
//! called. [`ExclusionRules::is_excluded`] stops at the first ignored ancestor
//! instead, which is what the walk does.
//!
//! It also panics on an absolute path outside its root, which is a second
//! reason not to reach for it.

use std::path::Path;

use ignore::gitignore::{Gitignore, GitignoreBuilder};

use crate::indexer::{is_hardcoded_excluded, is_user_excluded_dir};

/// The file, in the root of the knowledge base and nowhere else.
///
/// Not searched for in subdirectories and not searched for above `kb_path`.
/// The hierarchical form is what makes the walk and the watcher hard to keep in
/// agreement: a walk applies each directory's file as it descends, while the
/// watcher would have to reconstruct that stack for one path in isolation.
pub const IGNORE_FILE_NAME: &str = ".grooveignore";

/// Refuse to read more than this much of it.
///
/// The file lives inside the knowledge base, so it is written by whoever writes
/// the notes; a cap keeps a stray binary from being parsed as 40 MB of globs.
pub const MAX_IGNORE_FILE_BYTES: u64 = 64 * 1024;

/// And no more than this many patterns out of what was read.
pub const MAX_IGNORE_PATTERNS: usize = 1000;

/// Everything that decides a path is out of scope, in one object.
#[derive(Debug, Default)]
pub struct ExclusionRules {
    exclude_dirs: Vec<String>,
    /// `None` when there is no `.grooveignore`, which is the common case, and
    /// also when there is one that could not be read — see [`Self::load`].
    ignore: Option<Gitignore>,
    patterns: usize,
}

impl ExclusionRules {
    /// Read `<kb_path>/.grooveignore`, if it is there, and combine it with
    /// `exclude_dirs`.
    ///
    /// **Never fails.** A missing file is the ordinary case and says nothing. A
    /// file that exists but cannot be read — a hard link, a symlink, a
    /// directory, something over the cap — leaves the rules without it and
    /// warns. That direction is deliberate: this bounds what gets indexed, so
    /// failing open costs some noise in the index, while failing closed would
    /// stop a daemon from starting over a file the user may not even know is
    /// there. The warning is the part that must not be silent.
    pub fn load(kb_path: &Path, exclude_dirs: Vec<String>) -> Self {
        let path = kb_path.join(IGNORE_FILE_NAME);
        let bytes = match crate::links::read_checked(&path, MAX_IGNORE_FILE_BYTES) {
            Ok(crate::links::Content::Bytes(b)) => b,
            Ok(crate::links::Content::Refused(r)) => {
                tracing::warn!(
                    "{} exists but was not read, so nothing in it is in effect: {}",
                    path.display(),
                    r.log_line(&path)
                );
                return Self::from_exclude_dirs(exclude_dirs);
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Self::from_exclude_dirs(exclude_dirs);
            }
            Err(e) => {
                tracing::warn!(
                    "{} exists but could not be read, so nothing in it is in effect: {e}",
                    path.display()
                );
                return Self::from_exclude_dirs(exclude_dirs);
            }
        };
        let (ignore, patterns) = compile(kb_path, &path, &bytes);
        Self {
            exclude_dirs,
            ignore,
            patterns,
        }
    }

    /// The rules with no ignore file, which is what every knowledge base had
    /// before this feature.
    pub fn from_exclude_dirs(exclude_dirs: Vec<String>) -> Self {
        Self {
            exclude_dirs,
            ignore: None,
            patterns: 0,
        }
    }

    /// **The** decision. `rel` is relative to `kb_path`, forward-slash
    /// separated; `is_dir` says what the last component is.
    ///
    /// Each ancestor is tested as a directory first, and the first one that is
    /// excluded ends it — a file under an ignored directory is out, whatever a
    /// later `!` line says about the file itself. Then the last component is
    /// tested with the `is_dir` it was given.
    ///
    /// `is_dir` has to be right. A trailing-slash pattern is a
    /// directory-only pattern, so `build/` tested against `build` with
    /// `is_dir = false` matches nothing at all — measured, and silent.
    pub fn is_excluded(&self, rel: &str, is_dir: bool) -> bool {
        let rel = rel.trim_matches('/');
        if rel.is_empty() {
            return false;
        }
        for (idx, _) in rel.match_indices('/') {
            let ancestor = &rel[..idx];
            if !ancestor.is_empty() && self.matches(ancestor, true) {
                return true;
            }
        }
        self.matches(rel, is_dir)
    }

    /// One component path, against all three layers. Union: any of them saying
    /// "excluded" is enough, and `.grooveignore`'s `!` can only undo an earlier
    /// line of `.grooveignore` — never `exclude_dirs`, and never the hardcoded
    /// denylist, which is a fail-safe and stops being one the moment a file in
    /// the tree can switch it off.
    fn matches(&self, rel: &str, is_dir: bool) -> bool {
        // Directories only. Both of these compare a whole basename against a
        // list *of directory names* — that is what `exclude_dirs` is documented
        // to be, and the index walk has always tested `file_type().is_dir()`
        // before consulting them.
        //
        // The two implementations this replaces disagreed about the last
        // component: the walk tested directories only, while the watcher also
        // tested it when the path had no other component. Following the watcher
        // there looked like the safe side for a denylist, and it is not — a
        // configured name that can also be a filename, `exclude_dirs =
        // ["archive.md"]`, would start dropping every `notes/archive.md` from a
        // knowledge base that has no `.grooveignore` at all (codex P2, round 1
        // on PR #159). Ancestors are passed `is_dir = true` by
        // [`Self::is_excluded`], so the `.git/` and `node_modules/` fail-safe
        // still reaches everything underneath them.
        if is_dir {
            let basename = rel.rsplit('/').next().unwrap_or(rel);
            if is_hardcoded_excluded(basename) || is_user_excluded_dir(basename, &self.exclude_dirs)
            {
                return true;
            }
        }
        self.ignore
            .as_ref()
            .is_some_and(|gi| gi.matched(rel, is_dir).is_ignore())
    }

    /// Whether a `.grooveignore` is in effect, and with how many patterns.
    /// For the one line each entry point logs at startup.
    pub fn ignore_file_patterns(&self) -> Option<usize> {
        self.ignore.as_ref().map(|_| self.patterns)
    }

    /// The `exclude_dirs` these rules were built with, for callers that still
    /// have to pass them on.
    pub fn exclude_dirs(&self) -> &[String] {
        &self.exclude_dirs
    }
}

/// Turn the bytes of a `.grooveignore` into a matcher.
///
/// Split out from [`ExclusionRules::load`] so the parsing rules can be tested
/// on bytes rather than on a file that has to be created first — and so the
/// caps and the BOM are tested through the same code the real read uses.
fn compile(kb_path: &Path, source: &Path, bytes: &[u8]) -> (Option<Gitignore>, usize) {
    let text = String::from_utf8_lossy(bytes);
    // A leading BOM is otherwise part of the first pattern. Measured: the line
    // `\u{feff}bom.md` compiles into a glob that matches nothing a file is ever
    // called, so the first line of a file saved by Notepad would silently do
    // nothing. `str::lines` already drops the `\r` of a CRLF ending.
    let text = text.strip_prefix('\u{feff}').unwrap_or(&text);

    let mut builder = GitignoreBuilder::new(kb_path);
    // Before any pattern is added, because it does not apply retroactively:
    // measured, `Build/` added first and then `case_insensitive(true)` leaves
    // `build` unmatched while a pattern added afterwards folds correctly.
    //
    // Case-insensitive at all because `exclude_dirs` and the hardcoded denylist
    // already are (BU-19), and one config file whose two exclusion mechanisms
    // disagree about `Build` versus `build` is worse than either rule. On
    // Windows and macOS they are one directory regardless.
    if let Err(e) = builder.case_insensitive(true) {
        tracing::warn!(
            "{}: could not be made case-insensitive: {e}",
            source.display()
        );
    }

    let mut used = 0usize;
    let mut dropped = 0usize;
    for (n, line) in text.lines().enumerate() {
        // Blank and comment lines carry no pattern, so they must not count
        // toward the cap. Only the trailing side is trimmed for the emptiness
        // test: leading whitespace is part of a gitignore pattern.
        if line.trim().is_empty() || line.starts_with('#') {
            continue;
        }
        if used >= MAX_IGNORE_PATTERNS {
            dropped += 1;
            continue;
        }
        match builder.add_line(None, line) {
            Ok(_) => used += 1,
            Err(e) => tracing::warn!(
                "{}:{}: pattern ignored, the rest of the file still applies: {e}",
                source.display(),
                n + 1
            ),
        }
    }
    if dropped > 0 {
        tracing::warn!(
            "{}: only the first {MAX_IGNORE_PATTERNS} patterns are in effect; \
             {dropped} further lines were dropped",
            source.display()
        );
    }

    match builder.build() {
        Ok(gi) => (Some(gi), used),
        Err(e) => {
            tracing::warn!(
                "{}: could not be compiled, so nothing in it is in effect: {e}",
                source.display()
            );
            (None, 0)
        }
    }
}

/// `path` as a `kb_path`-relative, forward-slash key — the shape
/// [`ExclusionRules::is_excluded`] expects.
///
/// Passing an absolute path to the matcher instead is the quiet failure mode
/// worth naming: measured, an absolute path outside the matcher's root does not
/// error, it is matched against its own trailing components, so
/// `D:/somewhere/else/drafts` comes back ignored under a `drafts/` pattern.
/// Everything that decides exclusion goes through this function first.
pub fn rel_key(kb_path: &Path, path: &Path) -> String {
    path.strip_prefix(kb_path)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rules(lines: &str, exclude_dirs: &[&str]) -> ExclusionRules {
        let kb = Path::new("/kb");
        let (ignore, patterns) = compile(kb, Path::new("/kb/.grooveignore"), lines.as_bytes());
        ExclusionRules {
            exclude_dirs: exclude_dirs.iter().map(|s| s.to_string()).collect(),
            ignore,
            patterns,
        }
    }

    /// The known-answer case. If this one is wrong, nothing below it means
    /// anything.
    #[test]
    fn a_plain_pattern_excludes_the_file_it_names_and_nothing_else() {
        let r = rules("*.log\n", &[]);
        assert!(r.is_excluded("a.log", false));
        assert!(!r.is_excluded("a.md", false));
    }

    /// The reason the ancestor loop exists: the matcher on its own answers
    /// "no" for a file under an ignored directory, because gitignore expects
    /// the walk to have stopped at the directory.
    #[test]
    fn a_file_under_an_ignored_directory_is_excluded() {
        let r = rules("drafts/\n", &[]);
        assert!(r.is_excluded("drafts", true));
        assert!(r.is_excluded("drafts/note.md", false));
        assert!(r.is_excluded("drafts/deep/note.md", false));
        assert!(!r.is_excluded("published/note.md", false));
    }

    /// git's own rule, and the one place `matched_path_or_any_parents` would
    /// have answered differently: once the directory is out, a `!` line for a
    /// file inside it cannot bring it back, because the walk never gets there.
    #[test]
    fn a_negation_cannot_re_include_a_file_under_an_ignored_directory() {
        let r = rules("logs/\n!logs/important.md\n", &[]);
        assert!(
            r.is_excluded("logs/important.md", false),
            "an ignored parent wins; the walk never descends far enough to read the ! line"
        );
    }

    /// Negation still works where git says it does.
    #[test]
    fn a_negation_re_includes_a_file_whose_parents_are_all_kept() {
        let r = rules("*.md\n!keep.md\n", &[]);
        assert!(r.is_excluded("drop.md", false));
        assert!(!r.is_excluded("keep.md", false));
    }

    #[test]
    fn the_last_matching_line_wins() {
        assert!(rules("*.md\n!a.md\na.md\n", &[]).is_excluded("a.md", false));
        assert!(!rules("*.md\na.md\n!a.md\n", &[]).is_excluded("a.md", false));
    }

    /// A pattern with no slash matches at any depth; one with a slash is
    /// anchored to the knowledge-base root.
    #[test]
    fn a_slash_in_the_pattern_anchors_it_to_the_root() {
        let unanchored = rules("tmp\n", &[]);
        assert!(unanchored.is_excluded("tmp", true));
        assert!(unanchored.is_excluded("a/b/tmp", true));

        let anchored = rules("/tmp\n", &[]);
        assert!(anchored.is_excluded("tmp", true));
        assert!(!anchored.is_excluded("a/tmp", true));
    }

    #[test]
    fn double_star_forms() {
        assert!(rules("**/tmp\n", &[]).is_excluded("a/b/tmp", true));
        assert!(rules("a/**/b\n", &[]).is_excluded("a/x/y/b", true));
        // `docs/**` covers what is inside `docs`, not `docs` itself — so the
        // walk descends and drops each file, which the per-file check now does.
        let r = rules("docs/**\n", &[]);
        assert!(!r.is_excluded("docs", true));
        assert!(r.is_excluded("docs/x.md", false));
        assert!(r.is_excluded("docs/x/y.md", false));
    }

    /// The same rule `exclude_dirs` and the hardcoded denylist follow.
    #[test]
    fn patterns_match_regardless_of_case() {
        let r = rules("Build/\nNOTES/*.tmp.md\n", &[]);
        assert!(r.is_excluded("build", true));
        assert!(r.is_excluded("BUILD/x.md", false));
        assert!(r.is_excluded("notes/draft.tmp.md", false));
    }

    /// Union, in the direction that matters: an ignore file cannot switch off
    /// the fail-safe or the configured list.
    #[test]
    fn a_negation_cannot_undo_exclude_dirs_or_the_hardcoded_denylist() {
        let r = rules("!node_modules\n!cache\n", &["cache"]);
        assert!(
            r.is_excluded("node_modules", true),
            "the hardcoded denylist is a fail-safe and a file in the tree must not lift it"
        );
        assert!(r.is_excluded("node_modules/pkg/readme.md", false));
        assert!(r.is_excluded("cache", true));
        assert!(r.is_excluded("cache/x.md", false));
    }

    /// All three layers at once, including the boundaries on the other side of
    /// each one.
    #[test]
    fn the_three_layers_are_a_union() {
        let r = rules("*.tmp.md\nsecret/\n", &["cache"]);
        for excluded in [
            ".git/config",
            "node_modules/a/b.md",
            "cache/note.md",
            "notes/cache/note.md",
            "a.tmp.md",
            "deep/b.tmp.md",
            "secret/x.md",
        ] {
            assert!(
                r.is_excluded(excluded, false),
                "should be excluded: {excluded}"
            );
        }
        for kept in [
            "notes/a.md",
            "cache-of-notes/a.md",
            "notes/rebuild/a.md",
            "a.tmp",
            "secretive/x.md",
        ] {
            assert!(!r.is_excluded(kept, false), "should be kept: {kept}");
        }
    }

    /// (codex P2, round 1 on PR #159) `exclude_dirs` is a list of *directory*
    /// names, and nothing stops one of its entries from also being a plausible
    /// filename. Applying it to files as well would quietly drop documents from
    /// a knowledge base that never adopted `.grooveignore` — a regression
    /// introduced by the very refactor that was meant to stop the three
    /// surfaces disagreeing.
    #[test]
    fn a_directory_denylist_does_not_reach_a_file_of_the_same_name() {
        let r = ExclusionRules::from_exclude_dirs(vec!["archive.md".to_string()]);
        assert!(
            !r.is_excluded("notes/archive.md", false),
            "a file is not a directory, whatever it is called"
        );
        assert!(!r.is_excluded("archive.md", false));
        assert!(
            r.is_excluded("archive.md", true),
            "a directory of that name is still excluded"
        );
        assert!(r.is_excluded("archive.md/inside.md", false));
    }

    /// The same for the hardcoded fail-safe, which must keep reaching every
    /// path under `.git/` and `node_modules/` through the ancestor pass.
    #[test]
    fn the_hardcoded_denylist_still_covers_everything_under_it() {
        let r = ExclusionRules::from_exclude_dirs(vec![]);
        for rel in [
            "node_modules/pkg/README.md",
            ".git/COMMIT_EDITMSG.md",
            ".svn/entries.md",
            "sub/node_modules/deep/x.md",
        ] {
            assert!(r.is_excluded(rel, false), "must stay excluded: {rel}");
        }
        assert!(r.is_excluded("node_modules", true));
        assert!(
            !r.is_excluded("node_modules.md", false),
            "and it does not spill onto files that merely start with the name"
        );
    }

    #[test]
    fn the_knowledge_base_root_itself_is_never_excluded() {
        let r = rules("*\n", &[]);
        assert!(!r.is_excluded("", true));
        assert!(!r.is_excluded("/", true));
    }

    /// A trailing-slash pattern is directory-only, so the flag has to be right.
    /// Pinned because getting it wrong is silent.
    #[test]
    fn a_trailing_slash_pattern_only_matches_a_directory() {
        let r = rules("build/\n", &[]);
        assert!(r.is_excluded("build", true));
        assert!(!r.is_excluded("build", false));
    }

    #[test]
    fn no_ignore_file_leaves_exclude_dirs_behaving_exactly_as_before() {
        let r = ExclusionRules::from_exclude_dirs(vec!["target".to_string()]);
        assert!(r.ignore_file_patterns().is_none());
        assert!(r.is_excluded("target", true));
        assert!(r.is_excluded("target/doc/x.md", false));
        assert!(!r.is_excluded("targets/x.md", false));
        assert!(!r.is_excluded("notes/x.md", false));
    }

    // --- parsing -------------------------------------------------------

    #[test]
    fn blank_and_comment_lines_carry_no_pattern() {
        let (_, n) = compile(
            Path::new("/kb"),
            Path::new("/kb/.grooveignore"),
            b"\n   \n# a comment\n\r\n*.log\n",
        );
        assert_eq!(n, 1, "only the one real pattern should count");
    }

    /// Notepad writes one, and without stripping it the first pattern silently
    /// matches nothing.
    #[test]
    fn a_leading_byte_order_mark_is_not_part_of_the_first_pattern() {
        let mut bytes = "\u{feff}drafts/\n".as_bytes().to_vec();
        assert_eq!(
            &bytes[..3],
            &[0xEF, 0xBB, 0xBF],
            "test fixture lost its BOM"
        );
        bytes.extend_from_slice(b"");
        let (ignore, n) = compile(Path::new("/kb"), Path::new("/kb/.grooveignore"), &bytes);
        assert_eq!(n, 1);
        let r = ExclusionRules {
            exclude_dirs: vec![],
            ignore,
            patterns: n,
        };
        assert!(r.is_excluded("drafts/x.md", false));
    }

    #[test]
    fn crlf_line_endings_parse_the_same_as_lf() {
        let lf = rules("a/\nb/\n", &[]);
        let crlf = rules("a/\r\nb/\r\n", &[]);
        for p in ["a/x.md", "b/x.md", "c/x.md"] {
            assert_eq!(
                lf.is_excluded(p, false),
                crlf.is_excluded(p, false),
                "line endings changed the answer for {p}"
            );
        }
    }

    #[test]
    fn patterns_past_the_cap_are_dropped_and_the_rest_still_apply() {
        let mut src = String::new();
        for i in 0..MAX_IGNORE_PATTERNS + 50 {
            src.push_str(&format!("dir{i}/\n"));
        }
        let (ignore, n) = compile(
            Path::new("/kb"),
            Path::new("/kb/.grooveignore"),
            src.as_bytes(),
        );
        assert_eq!(n, MAX_IGNORE_PATTERNS);
        let r = ExclusionRules {
            exclude_dirs: vec![],
            ignore,
            patterns: n,
        };
        assert!(r.is_excluded("dir0/x.md", false));
        assert!(!r.is_excluded(&format!("dir{}/x.md", MAX_IGNORE_PATTERNS + 10), false));
    }

    /// Invalid UTF-8 must not take the file down with it.
    #[test]
    fn invalid_utf8_does_not_stop_the_valid_lines() {
        let mut bytes = b"drafts/\n".to_vec();
        bytes.extend_from_slice(&[0xFF, 0xFE, b'\n']);
        bytes.extend_from_slice(b"*.tmp.md\n");
        let (ignore, n) = compile(Path::new("/kb"), Path::new("/kb/.grooveignore"), &bytes);
        assert!(n >= 2, "the two valid patterns should survive, got {n}");
        let r = ExclusionRules {
            exclude_dirs: vec![],
            ignore,
            patterns: n,
        };
        assert!(r.is_excluded("drafts/x.md", false));
        assert!(r.is_excluded("x.tmp.md", false));
    }

    // --- loading -------------------------------------------------------

    struct TempKb {
        path: std::path::PathBuf,
    }

    impl TempKb {
        fn new(prefix: &str) -> Self {
            let path = crate::test_support::unique_temp_path(prefix);
            std::fs::create_dir_all(&path).unwrap();
            Self { path }
        }
        fn write_ignore(&self, body: &str) {
            std::fs::write(self.path.join(IGNORE_FILE_NAME), body).unwrap();
        }
    }

    impl Drop for TempKb {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    #[test]
    fn load_without_the_file_is_the_pre_feature_behaviour() {
        let kb = TempKb::new("kb-excl-none");
        let r = ExclusionRules::load(&kb.path, vec!["target".to_string()]);
        assert!(r.ignore_file_patterns().is_none());
        assert!(r.is_excluded("target/x.md", false));
        assert!(!r.is_excluded("notes/x.md", false));
    }

    #[test]
    fn load_reads_the_file_from_the_knowledge_base_root() {
        let kb = TempKb::new("kb-excl-load");
        kb.write_ignore("# notes to myself\ndrafts/\n*.tmp.md\n");
        let r = ExclusionRules::load(&kb.path, vec![]);
        assert_eq!(r.ignore_file_patterns(), Some(2));
        assert!(r.is_excluded("drafts/a.md", false));
        assert!(r.is_excluded("notes/b.tmp.md", false));
        assert!(!r.is_excluded("notes/b.md", false));
    }

    /// A hard-linked ignore file is refused by the same guard the notes go
    /// through (BU-20), and the rules carry on without it rather than failing
    /// to start. The warning is what the operator gets.
    #[test]
    fn a_hard_linked_ignore_file_is_refused_and_the_rest_still_works() {
        let kb = TempKb::new("kb-excl-hardlink");
        let elsewhere = kb.path.join("source.txt");
        std::fs::write(&elsewhere, "drafts/\n").unwrap();
        std::fs::hard_link(&elsewhere, kb.path.join(IGNORE_FILE_NAME))
            .expect("hard links need no privilege");

        let r = ExclusionRules::load(&kb.path, vec!["target".to_string()]);
        assert!(
            r.ignore_file_patterns().is_none(),
            "a multiply-linked ignore file must not be honoured"
        );
        assert!(!r.is_excluded("drafts/a.md", false));
        assert!(
            r.is_excluded("target/a.md", false),
            "exclude_dirs must keep working when the ignore file is refused"
        );
    }

    /// A directory in its place is refused the same way, on both platforms.
    #[test]
    fn an_ignore_file_that_is_a_directory_is_refused() {
        let kb = TempKb::new("kb-excl-dir");
        std::fs::create_dir_all(kb.path.join(IGNORE_FILE_NAME)).unwrap();
        let r = ExclusionRules::load(&kb.path, vec![]);
        assert!(r.ignore_file_patterns().is_none());
    }

    #[test]
    fn an_oversized_ignore_file_is_refused_whole() {
        let kb = TempKb::new("kb-excl-big");
        let mut body = String::from("drafts/\n");
        while body.len() as u64 <= MAX_IGNORE_FILE_BYTES {
            body.push_str("padding-pattern-that-goes-on/\n");
        }
        kb.write_ignore(&body);
        let r = ExclusionRules::load(&kb.path, vec![]);
        assert!(
            r.ignore_file_patterns().is_none(),
            "over the cap the file is not read at all, so no half-applied rule set"
        );
        assert!(!r.is_excluded("drafts/a.md", false));
    }

    /// What the watcher's hot reload relies on, and what the next `index` run
    /// relies on: `load` is a fresh read every time, with no caching to go
    /// stale — including the file being deleted.
    #[test]
    fn load_reflects_a_rewritten_ignore_file() {
        let kb = TempKb::new("kb-excl-reload");

        kb.write_ignore("drafts/\n");
        let before = ExclusionRules::load(&kb.path, vec![]);
        assert!(before.is_excluded("drafts/x.md", false));
        assert!(!before.is_excluded("logs/x.md", false));

        kb.write_ignore("logs/\n");
        let after = ExclusionRules::load(&kb.path, vec![]);
        assert!(
            !after.is_excluded("drafts/x.md", false),
            "the old patterns must not survive the rewrite"
        );
        assert!(after.is_excluded("logs/x.md", false));

        std::fs::remove_file(kb.path.join(IGNORE_FILE_NAME)).unwrap();
        let gone = ExclusionRules::load(&kb.path, vec![]);
        assert!(gone.ignore_file_patterns().is_none());
        assert!(!gone.is_excluded("logs/x.md", false));
    }

    #[test]
    fn rel_key_is_forward_slashed_and_relative() {
        let kb = Path::new("/kb");
        assert_eq!(rel_key(kb, &kb.join("notes").join("a.md")), "notes/a.md");
    }

    // -----------------------------------------------------------------------
    // The fail-safe, under generated input (audit L-21)
    //
    // `matches` documents the rule this file exists to hold: `.grooveignore`'s
    // `!` can undo an earlier line **of `.grooveignore`**, and never
    // `exclude_dirs` or the hardcoded denylist. The example tests above each
    // pick one negation and check it. A negation is a pattern language, and the
    // examples cover the spellings someone thought of.
    //
    // What makes this worth generating rather than listing was measured, by
    // breaking `matches` three ways and recording which tests noticed.
    //
    // **Two of the three are caught without these properties.** Consulting the
    // ignore matcher first, so a `!` wins outright, fails
    // `a_negation_cannot_undo_exclude_dirs_or_the_hardcoded_denylist`. Applying
    // the denylist only at the knowledge-base root fails
    // `the_hardcoded_denylist_still_covers_everything_under_it` and
    // `the_three_layers_are_a_union`. The examples above are not blind to a
    // reordered `matches`, and saying they were would overstate what is added
    // here.
    //
    // **The third is what these are for.** Give `!` gitignore's own rule — it
    // wins where an earlier line ignored something, and loses otherwise — and
    // every example passes, because each of them spells its negation `!name`
    // in a file that carries no ignore line at all. Both properties fail, and
    // proptest shrinks the input to `*\n!.git`: ignore everything, then take
    // one back. That spelling is in the generated set and in none of the
    // examples, which is the difference between covering a pattern language
    // and covering the patterns someone thought of.
    // -----------------------------------------------------------------------

    /// Spellings of "un-ignore this", as a `.grooveignore` would carry them.
    ///
    /// `{}` is filled with the directory name under test, so each one is a
    /// negation aimed **at that name**, which is the only kind that could
    /// plausibly reach it.
    const NEGATION_SHAPES: &[&str] = &[
        "!{}\n",
        "!{}/\n",
        "!/{}\n",
        "!{}/**\n",
        "!**/{}\n",
        "!**/{}/**\n",
        "*\n!{}\n",
        "**/*\n!{}/**\n",
        "!{}\n!{}/\n!**/{}/**\n",
    ];

    proptest::proptest! {
        /// No `.grooveignore` can re-admit a directory the denylist refuses.
        ///
        /// `.git` and `node_modules` are a fail-safe, and a fail-safe that a
        /// file inside the tree can switch off has stopped being one — a
        /// checked-out repository would start indexing its own object store,
        /// and the `.grooveignore` that did it would be a file the repository
        /// itself could carry.
        #[test]
        fn no_negation_can_re_admit_a_hardcoded_directory(
            shape in proptest::sample::select(NEGATION_SHAPES),
            name in proptest::sample::select(crate::indexer::HARDCODED_EXCLUDE_DIRS),
            depth in 0usize..3,
        ) {
            let r = rules(&shape.replace("{}", name), &[]);
            let prefix = "sub/".repeat(depth);
            let dir = format!("{prefix}{name}");
            proptest::prop_assert!(
                r.is_excluded(&dir, true),
                "`{}` re-admitted {dir}, which the hardcoded denylist refuses",
                shape.escape_debug()
            );
            proptest::prop_assert!(
                r.is_excluded(&format!("{dir}/inside.md"), false),
                "`{}` re-admitted a file under {dir}",
                shape.escape_debug()
            );
        }

        /// The same for a name the operator configured.
        ///
        /// `exclude_dirs` is the operator's list and `.grooveignore` is the
        /// knowledge base's; the second must not overrule the first, or the
        /// setting means "unless a file in the tree disagrees".
        #[test]
        fn no_negation_can_re_admit_a_configured_directory(
            shape in proptest::sample::select(NEGATION_SHAPES),
            name in "[a-z][a-z0-9_-]{0,12}",
            depth in 0usize..3,
        ) {
            let r = rules(&shape.replace("{}", &name), &[&name]);
            let prefix = "sub/".repeat(depth);
            let dir = format!("{prefix}{name}");
            proptest::prop_assert!(
                r.is_excluded(&dir, true),
                "`{}` re-admitted the configured directory {dir}",
                shape.escape_debug()
            );
        }

        /// And the boundary the two above could be satisfied by accident:
        /// **a name nothing refuses is still reachable**.
        ///
        /// A `matches` that returned `true` unconditionally would pass both
        /// properties above and exclude the whole knowledge base.
        #[test]
        fn a_directory_no_layer_names_is_not_excluded(
            name in "[a-z][a-z0-9_-]{0,12}",
        ) {
            proptest::prop_assume!(!crate::indexer::is_hardcoded_excluded(&name));
            let r = rules("", &[]);
            proptest::prop_assert!(
                !r.is_excluded(&name, true),
                "{name} was excluded with an empty .grooveignore and no exclude_dirs"
            );
        }
    }
}
