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
//! A fragment is read in the language of the file it lands on, because GitHub
//! reads it that way: on a page a heading slug or an anchor the page names
//! outright with `<a name>`, on anything rendered as source a line range such as
//! `#L10`, where what is checked is that the line is in the file, and on a
//! directory the README shown under its file list.
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
use std::path::{Component, Path, PathBuf};

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
///
/// A walk error is fatal rather than skipped. `.flatten()` would drop the
/// unreadable directory and carry on, and the corpus would quietly lose whatever
/// was under it -- a guard that checks less than it says it checks, which is the
/// one failure this test has no way to report. The three named pages below catch
/// a skip rule that grew too wide; they cannot catch a directory the filesystem
/// refused halfway down.
fn markdown_files(root: &Path) -> Vec<PathBuf> {
    let mut found: Vec<PathBuf> = walkdir::WalkDir::new(root)
        .into_iter()
        .filter_entry(|e| {
            e.depth() == 0
                || !e.file_type().is_dir()
                || !is_skipped_dir(e.path().strip_prefix(root).unwrap_or(e.path()))
        })
        .map(|entry| {
            entry.unwrap_or_else(|e| {
                panic!(
                    "the walk from {} failed partway through, so the corpus it \
                     collected is smaller than this test claims to check: {e}",
                    root.display()
                )
            })
        })
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
/// The algorithm is `github-slugger`'s -- the implementation the remark and MDX
/// toolchains use to reproduce GitHub's anchors, and the oracle the cases below
/// are taken from: downcase, delete everything outside `[\p{Word}\- ]`, then
/// turn spaces into hyphens. **The order is the part implementations get wrong.**
/// Punctuation
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
/// Repeats get `-1`, `-2`, which is what GitHub does when a page says "## Notes"
/// twice. The suffix is retried until it lands on an anchor nothing has taken:
/// `# Foo`, `# Foo`, `# Foo-1` ends `foo`, `foo-1`, `foo-1-1`, because the third
/// heading's own slug is already spoken for. Counting occurrences per base slug
/// instead -- the shape the old `html-pipeline` filter had -- hands the third
/// heading `foo-1` a second time, and a link to `#foo-1-1` is then reported
/// broken.
///
/// **Which of the two GitHub itself does for that collision is not written down
/// anywhere, and `POST /markdown` renders headings without ids, so it cannot be
/// measured from here.** The retry is chosen because of how the two fail: it can
/// only accept an anchor that does not exist, on a shape no page in this
/// repository has, while the counter can reject a link that works and stop CI.
/// A guard that cries wolf is a guard people learn to override.
fn anchors_of(markdown: &str) -> BTreeSet<String> {
    // Doubles as the set of anchors handed out: a key exists only because some
    // heading was given exactly that anchor, and the value counts the suffixes
    // tried against that key as a base.
    let mut taken: BTreeMap<String, usize> = BTreeMap::new();
    // Anchors the page names outright, which are verbatim rather than slugged.
    let mut named: BTreeSet<String> = BTreeSet::new();
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
                let mut anchor = base.clone();
                while taken.contains_key(&anchor) {
                    let tried = taken
                        .get_mut(&base)
                        .expect("the first collision was on `base`, so it is a key");
                    *tried += 1;
                    anchor = format!("{base}-{tried}");
                }
                taken.insert(anchor, 0);
            }
            Event::Text(t) | Event::Code(t) if in_heading => text.push_str(&t),
            Event::SoftBreak | Event::HardBreak if in_heading => text.push(' '),
            Event::Html(h) | Event::InlineHtml(h) => named.extend(explicit_anchors(&h)),
            _ => {}
        }
    }
    named.extend(taken.into_keys());
    named
}

/// The anchors a page names outright instead of growing from a heading.
///
/// `<a name="install"></a>` is how a paragraph is given a stable link target,
/// and GitHub honours it, so `#install` is a link that works and a page that
/// only knew about headings called it a missing one. `id` is the same thing
/// spelled the modern way.
///
/// Taken verbatim: an explicit anchor is not slugged, and it takes no part in
/// the numbering headings do -- GitHub's own numbering counts headings.
///
/// Lowercased for comparison only, and with `to_ascii_lowercase`, which is the
/// part worth saying: `to_lowercase` can change a string's *length* (`İ` is two
/// bytes and lowercases to three), and every index below is an index into both
/// strings at once.
fn explicit_anchors(html: &str) -> Vec<String> {
    let lowered = html.to_ascii_lowercase();
    let mut found = Vec::new();
    let mut from = 0;

    while let Some(open) = lowered[from..].find("<a").map(|at| from + at) {
        let after_name = open + "<a".len();
        let Some(close) = lowered[after_name..].find('>').map(|at| after_name + at) else {
            break;
        };
        from = close + 1;

        // `<abbr` and `<article` also start with `<a`.
        let (tag, lowered_tag) = (&html[after_name..close], &lowered[after_name..close]);
        if lowered_tag.starts_with(|c: char| c.is_ascii_alphanumeric() || c == '-') {
            continue;
        }

        for attribute in ["name", "id"] {
            if let Some(value) = attribute_value(tag, lowered_tag, attribute) {
                found.push(value.to_string());
            }
        }
    }
    found
}

/// One attribute's value out of a tag's text, or nothing if it has no such
/// attribute with a quoted value.
///
/// `lowered` is `tag` ASCII-lowercased, so the two share every index; the name
/// is matched against it and the value is cut out of `tag`, because the name is
/// case-insensitive and the value is not.
///
/// The name has to be a whole one -- `data-id="x"` ends in `id=` and names
/// nothing -- and HTML allows spaces around the `=`, which is why this is a scan
/// and not a search for `id="`.
fn attribute_value<'a>(tag: &'a str, lowered: &str, name: &str) -> Option<&'a str> {
    let mut from = 0;
    while let Some(at) = lowered[from..].find(name).map(|found| from + found) {
        from = at + name.len();
        if !tag[..at]
            .chars()
            .next_back()
            .is_none_or(char::is_whitespace)
        {
            continue;
        }
        let after_equals = tag[from..].trim_start().strip_prefix('=')?.trim_start();
        let Some(quote @ ('"' | '\'')) = after_equals.chars().next() else {
            continue;
        };
        if let Some(end) = after_equals[1..].find(quote) {
            return Some(&after_equals[1..1 + end]);
        }
    }
    None
}

/// Whether a destination points outside the repository.
///
/// A leading `//` is a URL that borrows the page's scheme, not a path. Joining
/// one to the source directory yields `\\example.com\guide` on Windows, which is
/// a UNC share, and a missing file on the other two -- so a link a reader can
/// follow would fail the build, differently on each operating system.
fn is_external(dest: &str) -> bool {
    dest.starts_with("//") || has_scheme(dest)
}

/// Whether the destination opens with a URI scheme.
///
/// The rule is the grammar rather than a list of the schemes seen so far:
/// RFC 3986 says `ALPHA *( ALPHA / DIGIT / "+" / "-" / "." ) ":"`, and it is
/// case-insensitive, so `MAILTO:` is one and a list of lowercase prefixes says
/// it is a filename. So do `tel:`, `sms:` and `urn:`, which have no `://` for a
/// substring search to find. Every one of those would be looked up on disk and
/// reported missing.
///
/// A relative path keeps its colon, because a scheme cannot contain `/`:
/// `docs/a:b.md` splits into `docs/a`, which the grammar rejects. What this does
/// take is `C:\x`, and correctly -- a browser reads that as a scheme too, so it
/// is not a link to anything in this repository either way.
fn has_scheme(dest: &str) -> bool {
    let Some((scheme, _)) = dest.split_once(':') else {
        return false;
    };
    let mut characters = scheme.chars();
    characters.next().is_some_and(|c| c.is_ascii_alphabetic())
        && characters.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
}

/// A destination as the filesystem and the heading list spell it.
///
/// GitHub percent-decodes before it resolves, so `docs/my%20guide.md#caf%C3%A9`
/// reaches `docs/my guide.md` and the heading `café`. Comparing the encoded form
/// instead fails a link that works -- and the encoded form is what a writer
/// produces by pasting a URL out of the address bar, which is the ordinary way
/// to write a link to a page whose name has a space in it.
///
/// Split first, decode after: an encoded `%23` is a literal `#` in a filename,
/// not the start of a fragment, and decoding first would cut the path there.
fn decoded(part: &str) -> String {
    percent_encoding::percent_decode_str(part)
        .decode_utf8_lossy()
        .into_owned()
}

/// The path a destination names and the heading it asks for, still encoded.
///
/// A relative URL is `path [ "?" query ] [ "#" fragment ]`, and only the path is
/// a filename. `docs/usage.md?plain=1#search` is a link GitHub hands out from
/// its own interface; leaving the query on the path looks for a file called
/// `usage.md?plain=1`, which no filesystem here can even hold.
///
/// The fragment is cut first, so a `?` on the far side of the `#` stays in the
/// fragment where it belongs. Both cuts happen before [`decoded`] runs, for the
/// reason written there: a path may not carry a bare `?` or `#`, so an encoded
/// one is part of the name and has to survive the split to be decoded into it.
///
/// An empty fragment is dropped. `page.md#` and `#` are the top of the page,
/// which every page has and no heading is named.
fn split_destination(dest: &str) -> (&str, Option<&str>) {
    let (before_fragment, fragment) = match dest.split_once('#') {
        Some((path, fragment)) => (path, Some(fragment)),
        None => (dest, None),
    };
    let path = before_fragment
        .split_once('?')
        .map_or(before_fragment, |(path, _query)| path);
    (path, fragment.filter(|f| !f.is_empty()))
}

/// Whether a destination carries a root, so it names somewhere other than this
/// repository.
///
/// GitHub resolves `/docs/usage.md` against the site root -- it means
/// `github.com/docs/usage.md`, not this file -- so a rooted destination is
/// broken wherever it is written. Saying so from the string matters more than
/// the verdict: `Path::join` **throws the document's directory away** when what
/// it is given has a root, so the alternative is asking the filesystem about the
/// machine's own root, and CI and a laptop do not have the same things there.
///
/// Both separators, because a link is not typed by the platform that reads it,
/// and `has_root` to catch what a bare `starts_with` misses -- `C:\x` on
/// Windows. This has to be asked **after** decoding, since `%2F` and `%5C`
/// decode into exactly these.
fn is_rooted(path: &str) -> bool {
    path.starts_with('/') || path.starts_with('\\') || Path::new(path).has_root()
}

/// The destination resolved against the page's own directory, or `None` if it
/// climbs out of the repository.
///
/// Resolved by the text, because `exists()` cannot answer this: a link that
/// leaves the checkout lands wherever the machine happens to keep things, and
/// whether that is a file is not a fact about this repository.
///
/// **Counted from the root rather than from `/`, which is the whole point.**
/// `../grooveseek/README.md` on a root-level page is out of the repository, and
/// GitHub serves it as a 404 -- there is nothing above a repository to address.
/// Fold it absolutely and it merely walks up one directory and back down into
/// whatever sits beside the checkout; on GitHub Actions that is the checkout
/// itself, because the path is `work/<repo>/<repo>`, so the link would resolve
/// to a real tracked file and pass. The name of the directory the repository
/// happens to be cloned into then decides the verdict. Counting depth from the
/// root asks the question GitHub asks, and gives the same answer everywhere.
///
/// Lexical for the same reason: resolving symlinks would answer a different
/// question than GitHub's, and would need the file to exist in order to ask.
fn resolved_within(root: &Path, directory: &Path, destination: &str) -> Option<PathBuf> {
    let mut from_root: Vec<Component> = directory
        .strip_prefix(root)
        .expect("every page walked was found under the repository root")
        .components()
        .collect();

    for component in Path::new(destination).components() {
        match component {
            Component::CurDir => {}
            // Nothing is above the repository, so this is where a link leaves.
            Component::ParentDir => {
                from_root.pop()?;
            }
            named => from_root.push(named),
        }
    }

    let mut resolved = root.to_path_buf();
    resolved.extend(from_root.iter());
    Some(resolved)
}

/// What a fragment on a directory link resolves against.
///
/// Browsing to `examples/deployments/personal/` renders that directory's README
/// underneath the file list, headings and anchors included, so
/// `examples/deployments/personal/#設置` is a link that works. A directory has no
/// `.md` extension, so without this it would be read as a file whose fragment
/// cannot be a heading.
enum DirectoryPage {
    /// A README whose anchors this test generates the way GitHub does.
    Markdown(PathBuf),
    /// A README in a language this test cannot slug. GitHub also renders
    /// `README.rst` and `README.adoc`, and their anchors follow their own
    /// languages' rules -- reproducing those would be a check with nothing to
    /// verify it against, so the fragment goes unchecked. That is the direction
    /// taken elsewhere here whenever the answer is not knowable: accept, rather
    /// than fail a link that works. This repository is Markdown throughout, so
    /// nothing takes this path today.
    Unreadable,
    /// No README at all, so the directory's page has no anchors to hit.
    Missing,
}

/// The README GitHub would render under this directory's file list.
fn directory_page(directory: &Path) -> DirectoryPage {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return DirectoryPage::Missing;
    };
    let mut found = DirectoryPage::Missing;
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_stem().and_then(|n| n.to_str()) else {
            continue;
        };
        if !name.eq_ignore_ascii_case("readme") || !path.is_file() {
            continue;
        }
        if path
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("md"))
        {
            return DirectoryPage::Markdown(path);
        }
        found = DirectoryPage::Unreadable;
    }
    found
}

/// The lines a `#L10` or `#L10-L20` fragment selects, if that is what it is.
///
/// GitHub gives every file it renders a fragment language, and only Markdown's
/// comes from headings. On source it is line numbers, and
/// `[the walk](../tests/docs_links_resolve.rs#L120)` is an ordinary thing to
/// write in an architecture page. Reporting those as broken anchors would fail
/// links that work, so they get the check that does apply to them: the line has
/// to be in the file.
fn selected_lines(fragment: &str) -> Option<(usize, usize)> {
    let (first, last) = match fragment.split_once('-') {
        Some((first, last)) => (first, last),
        None => (fragment, fragment),
    };
    let number = |part: &str| part.strip_prefix('L')?.parse::<usize>().ok();
    let (first, last) = (number(first)?, number(last)?);
    (first >= 1 && last >= first).then_some((first, last))
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
            let (raw_path, raw_anchor) = split_destination(&dest);
            let where_ = format!("{shown}:{line}");

            let path_part = decoded(raw_path);
            let anchor = raw_anchor.map(decoded);

            // Asked of the decoded path, because `%2F` decodes into one.
            if is_rooted(&path_part) {
                broken.push(format!("{where_}  site-root path        -> {dest}"));
                continue;
            }

            let target = if path_part.is_empty() {
                file.clone()
            } else {
                let directory = file.parent().expect("a file always has a parent directory");
                // Asked before `exists()`, because climbing out of the checkout
                // and back in through its parent lands on a real file.
                let Some(joined) = resolved_within(&root, directory, &path_part) else {
                    broken.push(format!("{where_}  leaves the repository -> {dest}"));
                    continue;
                };
                if !joined.exists() {
                    broken.push(format!("{where_}  no such file          -> {dest}"));
                    continue;
                }
                joined
            };

            let Some(anchor) = anchor else { continue };
            anchored += 1;

            // A directory is shown as its README, so that is the page the
            // fragment belongs to.
            let target = if target.is_dir() {
                match directory_page(&target) {
                    DirectoryPage::Markdown(readme) => readme,
                    DirectoryPage::Unreadable => continue,
                    DirectoryPage::Missing => target,
                }
            } else {
                target
            };

            if target.extension().is_none_or(|e| e != "md") {
                // Not a page, so the fragment is not a heading. On anything
                // GitHub renders as source it is a line range instead.
                match selected_lines(&anchor) {
                    Some((_, last)) if target.is_file() => {
                        let body = std::fs::read_to_string(&target).unwrap_or_else(|e| {
                            panic!("{} must be readable: {e}", target.display())
                        });
                        if last > body.lines().count() {
                            broken.push(format!("{where_}  past the last line    -> {dest}"));
                        }
                    }
                    _ => broken.push(format!("{where_}  anchor on a non-page  -> {dest}")),
                }
                continue;
            }
            let anchors = anchor_cache.entry(target.clone()).or_insert_with(|| {
                let body = std::fs::read_to_string(&target)
                    .unwrap_or_else(|e| panic!("{} must be readable: {e}", target.display()));
                anchors_of(&body)
            });
            if !anchors.contains(&anchor) {
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

/// A heading whose own slug was already handed out, which is the case a
/// per-base counter answers wrongly.
///
/// `# Foo`, `# Foo` leaves `foo` and `foo-1` taken. The third heading is written
/// `Foo-1`, so it *wants* `foo-1` -- and has to keep going. The fourth is `Foo`
/// again and takes the next suffix on its own base, not on the one the third
/// heading ended up with.
#[test]
fn a_heading_whose_own_slug_is_taken_keeps_looking() {
    let doc = "# Foo\n\n# Foo\n\n# Foo-1\n\n# Foo\n";
    let anchors = anchors_of(doc);
    assert_eq!(
        anchors.iter().map(String::as_str).collect::<Vec<_>>(),
        vec!["foo", "foo-1", "foo-1-1", "foo-2"],
        "{anchors:?}"
    );
}

/// Reference-style links reach the checker resolved, so they are checked.
#[test]
fn a_reference_style_link_is_extracted_like_an_inline_one() {
    let doc = "See [the page][p] and [another](docs/x.md#h).\n\n[p]: docs/p.md\n";
    let found: Vec<String> = links_of(doc).into_iter().map(|(_, d)| d).collect();
    assert_eq!(found, vec!["docs/p.md", "docs/x.md#h"], "{found:?}");
}

/// What counts as pointing out of the repository, including the form that
/// carries no scheme at all.
#[test]
fn a_destination_that_borrows_the_page_scheme_is_still_external() {
    assert!(is_external("//example.com/guide"));
    assert!(is_external("https://example.com"));
    assert!(is_external("mailto:someone@example.com"));
    assert!(!is_external("docs/usage.md"));
    assert!(!is_external("#a-heading"));

    // Schemes are a grammar, not a list: these have no `://` to search for, and
    // the case is not the writer's to get right.
    assert!(is_external("tel:+15550100"));
    assert!(is_external("sms:+15550100"));
    assert!(is_external("urn:isbn:0451450523"));
    assert!(is_external("MAILTO:someone@example.com"));
    assert!(is_external("HTTPS://example.com"));
    assert!(is_external("view-source:https://example.com"));

    // A colon inside a path is not a scheme, because a scheme has no `/`.
    assert!(!is_external("docs/a:b.md"));
    assert!(!is_external("./a:b.md"));
    assert!(!is_external("../notes/9:30.md"));
    // Nor is a leading digit, which the grammar does not allow.
    assert!(!is_external("1password:vault"));

    // And it never reaches the resolver, which would read it as a UNC share.
    let doc = "[mirror](//example.com/g) and [page](docs/p.md)\n";
    let found: Vec<String> = links_of(doc).into_iter().map(|(_, d)| d).collect();
    assert_eq!(found, vec!["docs/p.md"], "{found:?}");
}

/// A destination is a URL, and only the first of its three parts is a filename.
#[test]
fn a_destination_is_cut_where_the_url_grammar_cuts_it() {
    assert_eq!(split_destination("docs/usage.md"), ("docs/usage.md", None));
    assert_eq!(
        split_destination("docs/usage.md#search"),
        ("docs/usage.md", Some("search"))
    );

    // `?plain=1` is a link GitHub's own interface hands out.
    assert_eq!(
        split_destination("docs/usage.md?plain=1#search"),
        ("docs/usage.md", Some("search"))
    );
    assert_eq!(
        split_destination("docs/usage.md?plain=1"),
        ("docs/usage.md", None)
    );

    // The query ends at the fragment, so a `?` past the `#` stays in it.
    assert_eq!(
        split_destination("docs/x.md#what-now?"),
        ("docs/x.md", Some("what-now?"))
    );

    // The top of a page is not a heading, and every page has one.
    assert_eq!(split_destination("docs/x.md#"), ("docs/x.md", None));
    assert_eq!(split_destination("#"), ("", None));
    assert_eq!(split_destination("#a-heading"), ("", Some("a-heading")));

    // Encoded separators are name, not grammar, and survive to be decoded.
    let encoded = "docs/c%23-and-q%3F.md";
    assert_eq!(split_destination(encoded), (encoded, None));
    assert_eq!(decoded(encoded), "docs/c#-and-q?.md");
}

/// A rooted destination, in every spelling that would make `Path::join` drop
/// the directory the document sits in.
#[test]
fn a_rooted_destination_is_recognised_from_the_string() {
    assert!(is_rooted("/docs/usage.md"));
    assert!(is_rooted("\\docs\\usage.md"));
    assert!(!is_rooted("docs/usage.md"));
    assert!(!is_rooted("../docs/usage.md"));
    assert!(!is_rooted(""));

    // The check runs on the decoded path, which is the only place these two are
    // distinguishable from an ordinary name.
    assert!(!is_rooted("%2Fetc/passwd"));
    assert!(is_rooted(&decoded("%2Fetc/passwd")));
    assert!(is_rooted(&decoded("%5CWindows%5Csystem32")));
}

/// A destination that climbs out of the repository, which `exists()` cannot
/// tell from one that stays in.
///
/// The root here is a made-up relative path, and that is deliberate: the answers
/// below must not depend on where the repository is checked out or what the
/// directory holding it is called. Written against the real root, the first case
/// passes on a laptop and fails on GitHub Actions -- measured, on all three
/// runners -- because there the checkout is `work/grooveseek/grooveseek` and
/// `../grooveseek/` climbs out and lands straight back in.
#[test]
fn a_destination_that_leaves_the_repository_is_caught_by_the_text() {
    let root = Path::new("repo");
    let pages = root.join("docs");

    // Nothing is above a repository, whatever sits beside it on this machine.
    assert_eq!(resolved_within(root, root, "../grooveseek/README.md"), None);
    assert_eq!(resolved_within(root, &pages, "../../elsewhere.md"), None);

    // `..` that stays inside is ordinary, and this repository is full of it.
    assert_eq!(
        resolved_within(root, &pages, "../README.md"),
        Some(root.join("README.md"))
    );

    // `.` is dropped, and the answer never touches the filesystem.
    assert_eq!(
        resolved_within(root, &pages, "./a/../b/c.md"),
        Some(pages.join("b").join("c.md"))
    );

    // Landing on the root itself is inside it.
    assert_eq!(
        resolved_within(root, &pages, ".."),
        Some(root.to_path_buf())
    );
}

/// A directory link, which GitHub answers with the directory's README.
#[test]
fn a_directory_is_anchored_through_the_readme_github_renders() {
    let root = repo_root();
    assert!(
        matches!(directory_page(&root), DirectoryPage::Markdown(readme) if readme == root.join("README.md"))
    );

    // A directory without one has no page for a fragment to land on.
    assert!(matches!(
        directory_page(&root.join("docs").join("decisions")),
        DirectoryPage::Missing
    ));

    // Neither has something that is not a directory at all.
    assert!(matches!(
        directory_page(&root.join("README.md")),
        DirectoryPage::Missing
    ));
}

/// Anchors a page names outright, which GitHub honours and a heading list does
/// not contain.
#[test]
fn an_explicit_html_anchor_is_an_anchor() {
    let doc = "<a name=\"install\"></a>\n\n# Install it\n\nText.\n";
    let anchors = anchors_of(doc);
    assert!(anchors.contains("install"), "{anchors:?}");
    assert!(anchors.contains("install-it"), "{anchors:?}");

    // Verbatim, not slugged: the two would differ, and GitHub keeps the name.
    let shouty = "<a id=\"Step_ONE\"></a>\n\ntext\n";
    assert!(anchors_of(shouty).contains("Step_ONE"), "not verbatim");

    // Attribute and tag names are case-insensitive; the value is not.
    assert_eq!(explicit_anchors("<A NAME='top'></A>"), vec!["top"]);
    assert_eq!(explicit_anchors("<a  id = \"x\" >"), vec!["x"]);

    // Other tags starting with `a` are not anchors.
    assert!(explicit_anchors("<abbr id=\"x\">ms</abbr>").is_empty());
    assert!(explicit_anchors("<article id=\"x\">").is_empty());

    // Nor is an attribute that merely ends in one of the two names.
    assert!(explicit_anchors("<a data-id=\"x\">").is_empty());
    assert!(explicit_anchors("<a data-name=\"x\">").is_empty());

    // And nothing to find is not something to find.
    assert!(explicit_anchors("<a href=\"docs/x.md\">page</a>").is_empty());
    assert!(explicit_anchors("plain text").is_empty());
    assert!(explicit_anchors("<a name=unquoted>").is_empty());
}

/// Line fragments, which are what a fragment means on a file GitHub renders as
/// source rather than as a page.
#[test]
fn a_line_fragment_is_told_apart_from_a_heading() {
    assert_eq!(selected_lines("L10"), Some((10, 10)));
    assert_eq!(selected_lines("L10-L20"), Some((10, 20)));

    // A heading slug is not a line range, however much it starts like one.
    assert_eq!(selected_lines("license"), None);
    assert_eq!(selected_lines("L"), None);
    assert_eq!(selected_lines("L10-20"), None);
    assert_eq!(selected_lines("Lx"), None);

    // Ranges that cannot select anything are not line ranges either.
    assert_eq!(selected_lines("L0"), None);
    assert_eq!(selected_lines("L20-L10"), None);
}

/// Percent escapes are what a writer gets by copying a URL out of the address
/// bar, and GitHub resolves them decoded.
#[test]
fn a_percent_encoded_destination_is_decoded_before_it_is_resolved() {
    assert_eq!(decoded("docs/my%20guide.md"), "docs/my guide.md");
    assert_eq!(decoded("caf%C3%A9"), "café");
    assert_eq!(decoded("%E8%A6%8B%E5%87%BA%E3%81%97"), "見出し");

    // Untouched when there is nothing to decode, including a bare `%`.
    assert_eq!(decoded("docs/usage.md"), "docs/usage.md");
    assert_eq!(decoded("100%-ok"), "100%-ok");

    // An encoded `#` is part of the name, so the split has to happen first --
    // decoding first would cut this destination into a path and a fragment.
    let dest = "docs/c%23-notes.md";
    assert!(dest.split_once('#').is_none());
    assert_eq!(decoded(dest), "docs/c#-notes.md");
}
