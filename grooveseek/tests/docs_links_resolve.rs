//! Every relative link in this repository's Markdown resolves, `#anchor` included.
//!
//! The sibling guard `cargo doc` covers doc comments. Nothing covered the
//! Markdown, and the two rot the same way: a page is renamed, a heading is
//! reworded, and the link that pointed at it keeps rendering as a link. On
//! GitHub a dead relative path is a 404 and a dead anchor is worse -- it opens
//! the right page at the wrong place, silently, so a reader who follows it
//! believes they read the answer.
//!
//! A throwaway script checked this once, by hand, during the README split
//! (PR 3). It lived in a scratch directory and was never run again. This is
//! that check, in the tree, on every push.
//!
//! # What is walked
//!
//! Every `.md` file under the repository root except:
//!
//! - `target/`, which is build output, and `grooveseek/tests/fixtures/`, which
//!   is knowledge bases fed to the search engine -- inputs under test rather
//!   than documentation, and a fixture is entitled to contain a link that goes
//!   nowhere.
//! - every dot-directory except `.claude/` and `.github/`. `.git/` is one case;
//!   the ones that matter are `.dev/` (a nested private repository, here and not
//!   in a fresh clone) and `.remember/` (one machine's session notes). Walking
//!   either would make the local run and the CI run check different file sets,
//!   which is the one property a guard must not have, and the next tool to leave
//!   Markdown in a dot-directory has not been installed yet.
//! - `*.local.md`, gitignored for the same reason and linking into `.dev/`.
//!
//! Everything else is in, including `.claude/` and `grooveseek/examples/`: a
//! link that no longer resolves is wrong wherever it was written.
//!
//! # What is checked
//!
//! Only relative destinations. `http://`, `https://` and `mailto:` are skipped
//! outright -- reaching them needs a network, and a guard that can fail because
//! someone else's server is down stops being read.
//!
//! Existence is asked of the filesystem, so **the ubuntu leg of CI is the only
//! one that sees a link whose case is wrong**: Windows and macOS both answer
//! yes to `Readme.md`, and GitHub does not. That is a reason this test is in the
//! ordinary suite, which runs on three operating systems, rather than anywhere
//! that runs on one.
//!
//! # What this cannot catch
//!
//! **A link that resolves while the sentence around it lies.** The README split
//! had all of its links machine-verified as sound while seven statements were
//! broken: "see the section below" pointing at a section that had moved to
//! another file, "(see [Optional config file](configuration.md) above)" where
//! the link is fine and the word "above" is not. Passing this test says nothing
//! about that class; the complement is to grep for `above|below|earlier|後述|
//! 前述|上記|下記` after moving text between files.

use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// The extensions GitHub renders these pages with, and only those.
///
/// `Options::all()` is the shorthand, and it is wrong in two ways that would
/// never announce themselves. `ENABLE_SMART_PUNCTUATION` rewrites `--` as an en
/// dash before the slug is taken, and an en dash is stripped where a hyphen is
/// kept -- so a heading containing `--` would slug one way here and another on
/// GitHub. `ENABLE_HEADING_ATTRIBUTES` reads `## Foo {#bar}` as an id, which
/// GitHub does not honour: there the braces are part of the text the slug comes
/// from. Neither shape is in this corpus today, which is exactly why the choice
/// has to be written down rather than discovered later.
///
/// The metadata-block extension stays on. `.claude/skills/*/SKILL.md` carries
/// YAML frontmatter, and without it the closing `---` turns the block above into
/// a setext heading and invents an anchor for it.
fn github_flavour() -> Options {
    Options::ENABLE_TABLES
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_FOOTNOTES
        | Options::ENABLE_YAML_STYLE_METADATA_BLOCKS
}

/// This crate lives at `<repo>/grooveseek`, and the documentation it checks is
/// published from `<repo>`.
fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("CARGO_MANIFEST_DIR is <repo root>/grooveseek, which has a parent")
        .to_path_buf()
}

/// Directories the walk refuses to enter, by their path from the repository
/// root, each with the reason -- so that removing one is an argument rather than
/// a deletion.
const SKIPPED_PATHS: &[(&str, &str)] = &[
    ("target", "build output, including vendored sources"),
    (
        "grooveseek/tests/fixtures",
        "knowledge bases under test, not documentation",
    ),
];

/// Dot-directories this repository actually publishes. Everything else that
/// starts with a dot is some tool's state.
const PUBLISHED_DOT_DIRS: &[&str] = &[".claude", ".github"];

/// Whether the walk stops here, given the directory's path from the root.
///
/// The dot rule is a shape rather than a list on purpose. `.git` is one case,
/// but so are `.dev` (a nested private repository, present on the maintainer's
/// machine and absent from a fresh clone) and `.remember` (one machine's
/// session notes), and the next tool to leave Markdown in a dot-directory has
/// not been installed yet. Naming them one at a time means the guard checks a
/// different corpus on every machine until someone notices.
fn is_skipped_dir(relative: &Path) -> bool {
    if SKIPPED_PATHS
        .iter()
        .any(|(path, _)| relative == Path::new(path))
    {
        return true;
    }
    relative
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|name| name.starts_with('.') && !PUBLISHED_DOT_DIRS.contains(&name))
}

/// Every Markdown page this repository publishes, in a stable order.
fn markdown_files(root: &Path) -> Vec<PathBuf> {
    let mut found: Vec<PathBuf> = walkdir::WalkDir::new(root)
        .into_iter()
        .filter_entry(|e| {
            e.depth() == 0
                || !e.file_type().is_dir()
                || !is_skipped_dir(e.path().strip_prefix(root).unwrap_or(e.path()))
        })
        .flatten()
        .map(|e| e.into_path())
        .filter(|p| p.extension().is_some_and(|e| e == "md"))
        .filter(|p| {
            // `*.local.md` is gitignored, so it is on one machine only.
            !p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.ends_with(".local.md"))
        })
        .collect();
    found.sort();
    found
}

/// One heading's anchor, the way GitHub generates it.
///
/// GitHub's own implementation is `html-pipeline`'s table-of-contents filter:
/// downcase, delete everything outside `[\p{Word}\- ]`, then turn spaces into
/// hyphens. **The order is the part implementations get wrong.** Punctuation
/// goes first, so `信頼する置き場所 / しない置き場所` loses the slash and keeps
/// *two* spaces, which become two hyphens -- and a slugger that mapped spaces
/// first would produce one, and would then reject a link this repository
/// actually contains.
///
/// `is_alphanumeric` stands in for `\p{Word}` and is not the same set. It keeps
/// `Nl` and `No` (Roman numerals, fractions) where `\p{Word}` drops them, and
/// drops standalone combining marks where `\p{Word}` keeps them. Neither shape
/// occurs in any heading here, measured; a heading that introduced one would be
/// the first thing to re-check.
fn slug(heading_text: &str) -> String {
    heading_text
        .to_lowercase()
        .chars()
        .filter(|c| *c == ' ' || *c == '-' || *c == '_' || c.is_alphanumeric())
        .map(|c| if c == ' ' { '-' } else { c })
        .collect()
}

/// Every anchor GitHub would generate for this page.
///
/// Parsed rather than matched line by line. A `#` inside a fenced block is a
/// shell comment, not a heading, and this repository has one: the configuration
/// page shows `# enabled = false` inside TOML. A regex over line starts reads it
/// as a heading and invents an anchor nothing links to -- harmless -- but the
/// same regex misses a heading carrying a link or emphasis, which is not.
///
/// Repeats get `-1`, `-2`, counted against the original slug, which is what
/// GitHub does when a page says "## Notes" twice.
fn anchors_of(markdown: &str) -> BTreeSet<String> {
    let mut occurrences: BTreeMap<String, usize> = BTreeMap::new();
    let mut anchors = BTreeSet::new();
    let mut in_heading = false;
    let mut text = String::new();

    for event in Parser::new_ext(markdown, github_flavour()) {
        match event {
            Event::Start(Tag::Heading { .. }) => {
                in_heading = true;
                text.clear();
            }
            Event::End(TagEnd::Heading(_)) if in_heading => {
                in_heading = false;
                let base = slug(&text);
                let seen = occurrences.entry(base.clone()).or_insert(0);
                let anchor = if *seen == 0 {
                    base.clone()
                } else {
                    format!("{base}-{seen}")
                };
                *seen += 1;
                anchors.insert(anchor);
            }
            Event::Text(t) | Event::Code(t) if in_heading => text.push_str(&t),
            Event::SoftBreak | Event::HardBreak if in_heading => text.push(' '),
            _ => {}
        }
    }
    anchors
}

/// Whether a destination points outside the repository.
fn is_external(dest: &str) -> bool {
    dest.contains("://") || dest.starts_with("mailto:") || dest.starts_with("data:")
}

/// Every relative destination on this page, with the line it sits on.
///
/// Images count: a missing screenshot is the same defect as a missing page.
/// Reference-style links (`[text][id]`) arrive here already resolved, which is
/// the other thing a line-by-line matcher gets wrong.
fn links_of(markdown: &str) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    for (event, range) in Parser::new_ext(markdown, github_flavour()).into_offset_iter() {
        let dest = match event {
            Event::Start(Tag::Link { dest_url, .. })
            | Event::Start(Tag::Image { dest_url, .. }) => dest_url,
            _ => continue,
        };
        if dest.is_empty() || is_external(&dest) {
            continue;
        }
        let line = markdown[..range.start].matches('\n').count() + 1;
        out.push((line, dest.to_string()));
    }
    out
}

#[test]
fn every_relative_link_in_the_documentation_resolves() {
    let root = repo_root();
    let files = markdown_files(&root);

    // A walk that finds nothing passes, so say what was walked. Named pages
    // rather than a count: a number here would be one more thing to keep
    // current, and these three are the corners of the corpus -- the English
    // entry point, a page under `docs/`, and a page under a dot-directory that
    // the skip rule has to let through.
    for required in [
        "README.md",
        "docs/stability.md",
        ".claude/commands/feature-flow.md",
    ] {
        let wanted = root.join(required);
        assert!(
            files.contains(&wanted),
            "the walk from {} did not reach {required}, so whatever it did reach \
             is not this repository's documentation. A skip rule grew too wide, \
             or the layout moved and this test moves with it rather than being \
             relaxed. Walked {} files",
            root.display(),
            files.len()
        );
    }

    let mut anchor_cache: BTreeMap<PathBuf, BTreeSet<String>> = BTreeMap::new();
    let mut broken: Vec<String> = Vec::new();
    let (mut checked, mut anchored) = (0usize, 0usize);

    for file in &files {
        let text = std::fs::read_to_string(file)
            .unwrap_or_else(|e| panic!("{} must be readable: {e}", file.display()));
        let shown = file
            .strip_prefix(&root)
            .unwrap_or(file)
            .display()
            .to_string();

        for (line, dest) in links_of(&text) {
            checked += 1;
            let (path_part, anchor) = match dest.split_once('#') {
                Some((p, a)) => (p, Some(a)),
                None => (dest.as_str(), None),
            };
            let where_ = format!("{shown}:{line}");

            let target = if path_part.is_empty() {
                file.clone()
            } else {
                let joined = file
                    .parent()
                    .expect("a file always has a parent directory")
                    .join(path_part);
                if !joined.exists() {
                    broken.push(format!("{where_}  no such file          -> {dest}"));
                    continue;
                }
                joined
            };

            let Some(anchor) = anchor else { continue };
            anchored += 1;
            if target.extension().is_none_or(|e| e != "md") {
                broken.push(format!("{where_}  anchor on a non-page  -> {dest}"));
                continue;
            }
            let anchors = anchor_cache.entry(target.clone()).or_insert_with(|| {
                let body = std::fs::read_to_string(&target)
                    .unwrap_or_else(|e| panic!("{} must be readable: {e}", target.display()));
                anchors_of(&body)
            });
            if !anchors.contains(anchor) {
                broken.push(format!("{where_}  no such heading       -> {dest}"));
            }
        }
    }

    assert!(
        checked > 0,
        "no relative link was found across {} files, so the extraction stopped \
         working before the corpus did",
        files.len()
    );
    assert!(
        anchored > 0,
        "no link carried an #anchor, and the anchor half is the half that needs \
         the slug algorithm. Passing with none found is passing without doing \
         anything"
    );

    broken.sort();
    assert!(
        broken.is_empty(),
        "these Markdown links do not resolve, so they are 404s and wrong-place \
         jumps for anyone reading these pages on GitHub:\n  {}\n\
         Fix the link or the heading it names. Relaxing this check restores the \
         state where a renamed page took its inbound links with it and nothing \
         said so.",
        broken.join("\n  ")
    );
}

/// The slug algorithm, against answers written down elsewhere.
///
/// Every case here comes from `github-slugger`'s README or from this
/// repository, rather than from what this implementation happens to do.
#[test]
fn the_slug_algorithm_agrees_with_the_answers_github_publishes() {
    // github-slugger's own examples.
    assert_eq!(slug("foo"), "foo");
    assert_eq!(slug("Привет non-latin 你好"), "привет-non-latin-你好");
    assert_eq!(slug("😄 emoji"), "-emoji");

    // Punctuation is deleted before spaces become hyphens, which is why these
    // three differ from each other.
    assert_eq!(slug("foo (bar)"), "foo-bar");
    assert_eq!(slug("foo(bar)"), "foobar");
    assert_eq!(slug("a - b"), "a---b");

    // From this repository: the slash goes first and leaves two spaces behind.
    assert_eq!(
        slug("信頼する置き場所 / しない置き場所"),
        "信頼する置き場所--しない置き場所"
    );

    // An ideographic space is not the space that becomes a hyphen; it is
    // deleted like any other separator.
    assert_eq!(slug("見出し\u{3000}テスト"), "見出しテスト");

    // Digits, underscores and hyphens survive; a heading that is only
    // punctuation slugs to nothing.
    assert_eq!(slug("ADR-0009: one_gate 2"), "adr-0009-one_gate-2");
    assert_eq!(slug("!!!"), "");
}

/// Repeated headings, which GitHub disambiguates and this repository will
/// eventually contain.
#[test]
fn a_repeated_heading_gets_the_suffix_github_gives_it() {
    let doc = "# Notes\n\ntext\n\n# Notes\n\ntext\n\n# Notes\n";
    let anchors = anchors_of(doc);
    assert!(anchors.contains("notes"), "{anchors:?}");
    assert!(anchors.contains("notes-1"), "{anchors:?}");
    assert!(anchors.contains("notes-2"), "{anchors:?}");
}

/// The two shapes a line-by-line matcher gets wrong, in both directions.
#[test]
fn headings_come_from_the_parse_and_not_from_the_line_starts() {
    // A `#` inside a fenced block is not a heading. This repository has one.
    let fenced = "# Real\n\n```toml\n# enabled = false\n```\n";
    let anchors = anchors_of(fenced);
    assert!(anchors.contains("real"), "{anchors:?}");
    assert!(!anchors.contains("enabled--false"), "{anchors:?}");

    // A heading carrying emphasis and code slugs from its text, not its markup.
    let marked_up = "## The `--force` flag is **not** optional\n";
    assert!(
        anchors_of(marked_up).contains("the---force-flag-is-not-optional"),
        "{:?}",
        anchors_of(marked_up)
    );
}

/// Reference-style links reach the checker resolved, so they are checked.
#[test]
fn a_reference_style_link_is_extracted_like_an_inline_one() {
    let doc = "See [the page][p] and [another](docs/x.md#h).\n\n[p]: docs/p.md\n";
    let found: Vec<String> = links_of(doc).into_iter().map(|(_, d)| d).collect();
    assert_eq!(found, vec!["docs/p.md", "docs/x.md#h"], "{found:?}");
}
