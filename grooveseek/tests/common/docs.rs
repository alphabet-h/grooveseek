//! The corpus of Markdown this repository publishes, and how to read commands
//! out of it.
//!
//! Two guards ask the same question of the same files -- "which pages are this
//! repository's documentation?" -- and `AGENTS.md` says two places answering one
//! question must call one implementation. The walk started in
//! `tests/docs_links_resolve.rs`; it lives here now so the command-copy guards
//! can use it without writing a second copy. Moving it was part of adding them,
//! not a follow-up.
//!
//! # Why the `.ja.md` classifier is here and not shared with the binary
//!
//! `grooveseek/src/main.rs` already has one, in `mod documented_flags`, and its
//! documentation says it is the only place that reads the suffix. That module is
//! `#[cfg(test)]` inside the **binary**, and an integration test links the
//! library, so it cannot be reached from here. `src/test_support.rs` is
//! `#[cfg(test)]` too. Widening either to share ~40 lines would put test-only
//! code in the shipping library, which this project has deliberately avoided.
//! So there are two classifiers, and the claim in `main.rs` is scoped to its own
//! module rather than the tree.
//!
//! # Nothing checks the links in this file
//!
//! `AGENTS.md` asks for `` [`name`] `` over a plain backtick because `cargo doc`
//! then fails when the name leaves the tree. That guard does not reach here:
//! every target under `tests/` is `doc = false` (`cargo metadata`), so rustdoc
//! never reads these comments and a `` [`link`] `` in this file is a convention
//! rather than a checked reference. Written down because the brackets look like
//! the guarded form, and a reference that looks guarded and is not is worse than
//! one that never claimed to be.

#![allow(dead_code)] // each guard uses a different part of this module

use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};
use std::collections::BTreeSet;
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
pub fn github_flavour() -> Options {
    Options::ENABLE_TABLES
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_FOOTNOTES
        | Options::ENABLE_YAML_STYLE_METADATA_BLOCKS
}

/// This crate lives at `<repo>/grooveseek`, and the documentation it checks is
/// published from `<repo>`.
pub fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("CARGO_MANIFEST_DIR is <repo root>/grooveseek, which has a parent")
        .to_path_buf()
}

/// A page's text with Windows line endings folded, so a byte offset and a
/// line number mean the same thing on every checkout.
///
/// Three guards carried this as a private copy each before the fourth one
/// arrived; `AGENTS.md` says the copy is collapsed by the change that would add
/// another, so it lives here now.
pub fn read(path: &Path) -> String {
    std::fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("could not read {}: {e}", path.display()))
        .replace("\r\n", "\n")
}

/// The path as a failure message names it: relative to the repository root,
/// with `/` on every platform, so a message reads the same on Windows and in CI.
pub fn shown(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .display()
        .to_string()
        .replace('\\', "/")
}

/// Directories the walk refuses to enter, by their path from the repository
/// root, each with the reason -- so that removing one is an argument rather than
/// a deletion.
pub const SKIPPED_PATHS: &[(&str, &str)] = &[
    ("target", "build output, including vendored sources"),
    (
        "grooveseek/tests/fixtures",
        "knowledge bases under test and pages frozen from this repository's \
         history, not documentation",
    ),
];

/// Dot-directories this repository actually publishes. Everything else that
/// starts with a dot is some tool's state.
pub const PUBLISHED_DOT_DIRS: &[&str] = &[".claude", ".github"];

/// Whether the walk stops here, given the directory's path from the root.
///
/// The dot rule is a shape rather than a list on purpose. `.git` is one case,
/// but so are `.dev` (a nested private repository, present on the maintainer's
/// machine and absent from a fresh clone) and `.remember` (one machine's
/// session notes), and the next tool to leave Markdown in a dot-directory has
/// not been installed yet. Naming them one at a time means the guard checks a
/// different corpus on every machine until someone notices.
pub fn is_skipped_dir(relative: &Path) -> bool {
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
/// one failure this test has no way to report. The named pages each guard asserts
/// on catch a skip rule that grew too wide; they cannot catch a directory the
/// filesystem refused halfway down.
pub fn markdown_files(root: &Path) -> Vec<PathBuf> {
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

/// The suffix that puts a page in the Japanese half of a bilingual pair.
pub const JAPANESE_SUFFIX: &str = ".ja.md";

/// The English page a Japanese one translates, or `None` when this is not a
/// Japanese page.
///
/// `Path::extension` answers `"md"` for both halves, because it reads from the
/// last dot. The file name's suffix is the only thing that separates them, and
/// this is the only place in these guards that reads it.
///
/// The name is read with `to_string_lossy`, which is what
/// `main.rs`'s `documented_flags::Corpus::of` does, and the match matters. An
/// earlier version here used `to_str()` and returned `None` for a name that is
/// not valid UTF-8. The binary would have called such a name Japanese and the
/// twin guard would have skipped it -- **silently**, which is the one way a
/// guard fails without saying so. The two classifiers cannot be collapsed into
/// one (see this module's header), so the next best thing is that they read a
/// name the same way, and that this is written where either is edited.
pub fn english_counterpart(path: &Path) -> Option<PathBuf> {
    let name = path.file_name()?.to_string_lossy().into_owned();
    let stem = name.strip_suffix(JAPANESE_SUFFIX)?;
    Some(path.with_file_name(format!("{stem}.md")))
}

/// Fence info strings under which this project writes commands.
///
/// A tag rather than a guess at the contents: an untagged fence in this corpus
/// is a transcript of output, and reading those as commands makes the answer
/// depend on whether a word in the output happens to look like a program name.
pub const SHELL_TAGS: &[&str] = &[
    "bash",
    "sh",
    "shell",
    "console",
    "zsh",
    "powershell",
    "pwsh",
    "bat",
    "cmd",
];

/// A fence's language, lowercased. Empty when the fence has no info string.
///
/// One function because more than one caller asks it -- [`fenced_blocks`] to
/// label every block it collects, [`pin_sites`] to label the one a marker names,
/// [`is_shell_tag`] to decide whether it is a shell. How many callers there are
/// is not written down: a number beside a list goes stale on its own, which is
/// what this file exists to catch.
pub fn fence_tag(info: &str) -> String {
    info.split_whitespace()
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase()
}

/// Whether a fence's info string names one of [`SHELL_TAGS`].
pub fn is_shell_tag(info: &str) -> bool {
    SHELL_TAGS.contains(&fence_tag(info).as_str())
}

/// One fenced code block, located well enough to name in a failure.
#[derive(Clone, Debug)]
pub struct FencedBlock {
    /// 1-based line of the opening fence.
    pub line: usize,
    /// The info string's first word, lowercased. Empty when the fence has none.
    pub tag: String,
    /// The text between the fences, verbatim.
    pub body: String,
}

/// One inline `` `code` `` span that chains commands with `&&` or `||`.
#[derive(Clone, Debug)]
pub struct InlineChain {
    /// 1-based line the span starts on.
    pub line: usize,
    /// The span as the page writes it, for quoting in a failure. Rebuilding it
    /// from [`commands`](Self::commands) would print a separator the page does
    /// not use and send a reader searching for text that is not there.
    pub text: String,
    /// The chain's parts, normalised the way [`command_lines`] normalises a
    /// block's lines.
    pub commands: Vec<String>,
}

/// A `<!-- groove-pin: id -->` marker and the block it names, if it names one.
#[derive(Clone, Debug)]
pub struct PinSite {
    pub id: String,
    /// 1-based line of the marker.
    pub line: usize,
    /// The fenced block immediately after the marker. `None` when the next
    /// thing in the document is anything else -- see [`pin_sites`].
    pub block: Option<FencedBlock>,
}

/// The 1-based line an offset falls on.
pub fn line_of(markdown: &str, offset: usize) -> usize {
    markdown[..offset].matches('\n').count() + 1
}

/// The document as a list of events, which both readers below walk.
type Events<'a> = Vec<(Event<'a>, std::ops::Range<usize>)>;

fn events_of(markdown: &str) -> Events<'_> {
    Parser::new_ext(markdown, github_flavour())
        .into_offset_iter()
        .collect()
}

/// The text inside the code block whose `Start` event is at `index`.
///
/// One function because two readers want it: [`fenced_blocks`], which collects
/// every block, and [`pin_sites`], which wants the one block a marker names.
/// Written twice, the two would answer differently the first time pulldown-cmark
/// put an event inside a code block that neither expected -- and the shape that
/// disagreement takes is a body silently missing part of itself, which is the
/// failure this whole file exists to refuse.
fn code_block_body(events: &Events<'_>, index: usize) -> String {
    let mut body = String::new();
    for (event, _) in &events[index + 1..] {
        match event {
            Event::Text(text) => body.push_str(text),
            Event::End(TagEnd::CodeBlock) => break,
            _ => {}
        }
    }
    body
}

/// Every fenced code block in the document, in source order.
///
/// Fenced only. An indented block carries no info string, so it cannot say
/// whether it holds commands, and these guards say outright that they do not
/// read them.
pub fn fenced_blocks(markdown: &str) -> Vec<FencedBlock> {
    let events = events_of(markdown);
    let mut blocks = Vec::new();
    for (index, (event, range)) in events.iter().enumerate() {
        if let Event::Start(Tag::CodeBlock(CodeBlockKind::Fenced(info))) = event {
            blocks.push(FencedBlock {
                line: line_of(markdown, range.start),
                tag: fence_tag(info),
                body: code_block_body(&events, index),
            });
        }
    }
    blocks
}

/// The fenced blocks whose tag is one of [`SHELL_TAGS`].
pub fn shell_blocks(markdown: &str) -> Vec<FencedBlock> {
    fenced_blocks(markdown)
        .into_iter()
        .filter(|b| is_shell_tag(&b.tag))
        .collect()
}

/// The first occurrence of `needle` that is not inside single or double quotes.
///
/// The scan is over bytes and the comparison is between bytes, because these
/// pages are bilingual: slicing the `str` at a byte offset to compare there
/// panics the moment the offset lands inside a Japanese character. Every needle
/// and every quote here is ASCII, and an ASCII byte never occurs inside a
/// multi-byte UTF-8 sequence, so a byte match is always at a character boundary
/// and the offset returned is safe to slice at.
fn find_unquoted(line: &str, needle: &str) -> Option<usize> {
    let bytes = line.as_bytes();
    let needle = needle.as_bytes();
    let mut quote: Option<u8> = None;
    for i in 0..bytes.len() {
        match quote {
            Some(q) => {
                if bytes[i] == q {
                    quote = None;
                }
            }
            None => {
                if bytes[i] == b'\'' || bytes[i] == b'"' {
                    quote = Some(bytes[i]);
                } else if bytes[i..].starts_with(needle) {
                    return Some(i);
                }
            }
        }
    }
    None
}

/// The line with any trailing `# comment` removed.
///
/// A `#` only starts a comment at the start of a word and outside quotes, so
/// `groove search "a #b"` keeps its argument. Backslash escapes inside quotes
/// are not handled; no line in this corpus needs them.
fn without_comment(line: &str) -> &str {
    let bytes = line.as_bytes();
    let mut quote: Option<u8> = None;
    for i in 0..bytes.len() {
        match quote {
            Some(q) => {
                if bytes[i] == q {
                    quote = None;
                }
            }
            None => {
                if bytes[i] == b'\'' || bytes[i] == b'"' {
                    quote = Some(bytes[i]);
                } else if bytes[i] == b'#'
                    && (i == 0 || bytes[i - 1] == b' ' || bytes[i - 1] == b'\t')
                {
                    return &line[..i];
                }
            }
        }
    }
    line
}

/// The delimiter a heredoc on this line opens, if it opens one.
fn heredoc_tag(line: &str) -> Option<String> {
    let at = find_unquoted(line, "<<")?;
    let rest = &line[at + 2..];
    let rest = rest.strip_prefix('-').unwrap_or(rest);
    let rest = rest.trim_start();
    let (quote, rest) = match rest.chars().next() {
        Some(c @ ('\'' | '"')) => (Some(c), &rest[c.len_utf8()..]),
        _ => (None, rest),
    };
    let end = rest
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .unwrap_or(rest.len());
    if end == 0 {
        return None;
    }
    if let Some(q) = quote
        && !rest[end..].starts_with(q)
    {
        return None;
    }
    Some(rest[..end].to_string())
}

/// Whether a bare word is a name a program could have.
fn is_program_name(word: &str) -> bool {
    let mut chars = word.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !(first.is_ascii_alphabetic() || first == '_') {
        return false;
    }
    chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '+' | '-'))
}

/// The program a token names, if it names one.
///
/// A command is often invoked by path -- `./groove`, `/usr/local/bin/groove`,
/// `.\groove.exe` -- and the deployment recipes here do exactly that. Reading
/// only bare words dropped those lines entirely, which is worse than reading
/// them wrong: both halves of a translated pair dropped the same line, so they
/// agreed by both being invisible, and a change to one of them would not have
/// been reported.
///
/// The identity is the last path component, because that is the program:
/// `/usr/local/bin/groove index` and `groove index` are the same instruction
/// written for different installations. A path is not evidence of a different
/// command, so it should not read as one.
///
/// This is deliberately loose in one direction: anything shaped like a path
/// reads as a program, so `text/plain` answers `plain`. Nothing here can tell
/// an install path from a MIME type, and narrowing it to `./`, `../` and `/`
/// would drop `scripts/build.sh`, which is how a relative invocation is
/// ordinarily written. What keeps the looseness harmless is that a caller has
/// to have other reasons to be reading the text as a command: a shell fence, or
/// a span held together by `&&`. `text/plain; charset=utf-8` reaches neither.
fn command_token_name(token: &str) -> Option<&str> {
    let name = token.rsplit(['/', '\\']).next()?;
    if is_program_name(name) {
        Some(name)
    } else {
        None
    }
}

/// Whether a token can start a command.
fn is_command_token(token: &str) -> bool {
    command_token_name(token).is_some()
}

/// `sudo` options that take a value, so the value is not read as the program.
///
/// `sudo -u groove /usr/local/bin/groove index` is in this repository's
/// deployment recipes, and `groove` there is the user to become, not the thing
/// being run. Enumerated rather than guessed: a rule like "skip the token after
/// any flag" would swallow the program itself after `sudo -n`.
const SUDO_FLAGS_WITH_VALUES: &[&str] = &[
    "-u", "-g", "-h", "-p", "-C", "-D", "-R", "-T", "-U", "-c", "-r", "-t",
];

/// Drop a leading `sudo` and the options that belong to it.
fn strip_sudo(tokens: &mut Vec<String>) {
    if tokens.first().map(String::as_str) != Some("sudo") {
        return;
    }
    tokens.remove(0);
    while let Some(first) = tokens.first() {
        if !first.starts_with('-') {
            break;
        }
        let takes_value = SUDO_FLAGS_WITH_VALUES.contains(&first.as_str());
        tokens.remove(0);
        if takes_value && !tokens.is_empty() {
            tokens.remove(0);
        }
    }
}

/// Where a token's `NAME=` assignment ends, if it opens one.
fn assignment_value(token: &str) -> Option<&str> {
    let at = token.find('=')?;
    let name = &token[..at];
    if name.is_empty() || !is_program_name(name) {
        return None;
    }
    Some(&token[at + 1..])
}

/// Drop a leading `VAR=value` environment prefix, however many there are.
///
/// An assignment whose value *runs* something keeps the program instead of
/// taking it away. `NEXT_ID=$(jq '...' file)` splits on whitespace into
/// `NEXT_ID=$(jq` and the rest, and dropping that first token would drop `jq`
/// with it -- leaving fragments that name no program, so the whole line fell out
/// of the corpus without a word. `.claude/commands/full-audit.md` writes exactly
/// that.
///
/// The substitution has to be closed as well as opened. `PWD=$(pwd)` takes no
/// arguments, so the whole thing is **one** token and the `)` is still attached
/// to the program -- and `pwd)` names no program, which put the line back in the
/// silence this was meant to end. A no-argument substitution is the commoner
/// spelling of the two.
fn strip_env_prefix(tokens: &mut Vec<String>) {
    while let Some(first) = tokens.first() {
        if first.starts_with('-') {
            break;
        }
        let Some(value) = assignment_value(first) else {
            break;
        };
        let value = value.trim_start_matches(['"', '\'']);
        let opened = value
            .strip_prefix("$(")
            .map(|rest| (rest, ')'))
            .or_else(|| value.strip_prefix('`').map(|rest| (rest, '`')));
        match opened {
            Some((rest, closer)) => {
                let program = rest.trim_end_matches(['"', '\'']).trim_end_matches(closer);
                if program.is_empty() {
                    tokens.remove(0);
                } else {
                    tokens[0] = program.to_string();
                    break;
                }
            }
            None => {
                tokens.remove(0);
            }
        }
    }
}

/// Step past a subshell's opening parenthesis.
///
/// `(gh run list ...) | Where-Object { ... }` runs `gh`; the bracket is
/// grouping, not a program.
fn strip_subshell(tokens: &mut [String]) {
    if let Some(first) = tokens.first() {
        let opened = first.trim_start_matches('(');
        if opened.len() != first.len() && !opened.is_empty() {
            tokens[0] = opened.to_string();
        }
    }
}

/// Step past everything that wraps a command without being one, until what is
/// left starts with the program.
///
/// Both orders occur. `RUST_LOG=debug groove serve` sets the environment and
/// then runs; `sudo -u groove RUST_LOG=debug groove serve` runs sudo, which
/// takes its own options and *then* accepts assignments -- the usage line puts
/// `[VAR=value]` after the options. Stripping each once in a fixed order leaves
/// the probe pointing at whichever wrapper came second, and a line whose program
/// is never found is a line dropped in silence.
///
/// One function because both callers ask the same question. [`head_of`] needs
/// the program to name the command and [`command_lines`] needs it to decide
/// whether the line is a command at all; two copies of this sequence would
/// answer differently the first time either grew a case.
fn strip_wrappers(tokens: &mut Vec<String>) {
    loop {
        let before = (tokens.len(), tokens.first().cloned());
        strip_subshell(tokens);
        strip_env_prefix(tokens);
        strip_sudo(tokens);
        // Length alone is not progress any more: unwrapping an assignment that
        // runs something rewrites the first token instead of removing it.
        if (tokens.len(), tokens.first().cloned()) == before {
            return;
        }
    }
}

/// One command's identity: its program, plus the bare word after it.
///
/// Flags are deliberately dropped. `cargo fmt --all` and
/// `cargo fmt --all -- --check` are the same command run differently, and a
/// copy that names the same commands with different flags is the copy this
/// guard exists to find -- comparing whole lines would rank that as a
/// difference and miss it.
///
/// The word after the program is a subcommand for `cargo` and `groove` and an
/// argument for `cp`, and nothing here tells them apart: `sudo cp
/// groove.service /etc/systemd/system/` reads as `cp groove.service`. That is
/// tolerated rather than fixed, because getting it wrong can only ever **split**
/// one identity into two, never merge two into one -- and the direction that
/// matters is the second. A merge would let an unrelated command satisfy a
/// subset; a split at worst leaves a copy unreported, which is the failure this
/// guard already accepts elsewhere. Telling them apart would need a list of
/// which programs take subcommands, and a list is a thing that goes stale.
pub fn head_of(fragment: &str) -> Option<String> {
    let text = without_comment(fragment.trim()).trim();
    if text.is_empty() {
        return None;
    }
    let mut tokens: Vec<String> = text.split_whitespace().map(str::to_string).collect();
    strip_wrappers(&mut tokens);
    let mut head = command_token_name(tokens.first()?)?.to_string();
    // A subcommand is a bare word. `groove ./kb` names one program, not two.
    if let Some(second) = tokens.get(1)
        && is_program_name(second)
    {
        head.push(' ');
        head.push_str(second);
    }
    Some(head)
}

/// The command lines of a block body, normalised.
///
/// Heredoc bodies are dropped: they are data, and reading them as commands both
/// invents head names out of the payload and makes a translated payload look
/// like a drifted command. Continuations are joined, comments removed,
/// whitespace squeezed, and anything whose first word cannot start a command --
/// JSON braces, prose, transcript output -- is left out.
///
/// The heredoc and the continuation are tracked in **one** pass, and the order
/// matters. Joining continuations first lets a heredoc body line ending in `\`
/// swallow the line after it -- and if that line was the terminator, the skip
/// never ends and every command below it disappears from a block that still
/// looks like it was read. A payload is text, so a backslash at the end of one
/// is a character rather than a continuation.
pub fn command_lines(body: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut pending = String::new();
    let mut skip_until: Option<String> = None;

    for raw in body.lines() {
        if let Some(tag) = skip_until.as_deref() {
            if raw.trim() == tag {
                skip_until = None;
            }
            continue;
        }

        let line = raw.trim_end();
        let line = if pending.is_empty() {
            line.to_string()
        } else {
            format!("{} {}", pending.trim_end(), line.trim_start())
        };
        pending.clear();
        if let Some(stripped) = line.strip_suffix('\\') {
            pending = stripped.to_string();
            continue;
        }

        if let Some(tag) = heredoc_tag(&line) {
            skip_until = Some(tag);
        }
        let text = without_comment(&line).trim().to_string();
        if text.is_empty() {
            continue;
        }
        let text = text
            .strip_prefix("$ ")
            .or_else(|| text.strip_prefix("> "))
            .unwrap_or(&text)
            .to_string();
        let tokens: Vec<String> = text.split_whitespace().map(str::to_string).collect();
        // Two different questions, and they must not share an answer. *Whether*
        // this line is a command is decided on the program, which
        // [`strip_wrappers`] finds. *What is kept* is the line as written,
        // because everything stepped over is still part of the instruction:
        // `RUST_LOG=grooveseek=debug groove serve` and `RUST_LOG=trace groove
        // serve` tell a reader to do different things, and `docs/usage.md` sets
        // RUST_LOG on three lines whose Japanese twin would otherwise be free to
        // disagree about it.
        let joined = tokens.join(" ");
        if is_instruction(&joined) {
            out.push(joined);
        }
    }
    out
}

/// Whether a fragment tells the reader to do something.
///
/// Wider than [`head_of`], which answers "which program is this". A line can be
/// an instruction and name no program: `S=<scratchpad>` in the codex-review
/// skill, `FILES=(a b)`, `FOO=1 && BAR=2`. Setting a variable is a step a reader
/// performs and a value a translation can get wrong, so it has to survive into
/// what gets compared -- and before this it did not, which put a whole class of
/// line back in the silence these guards exist to end.
///
/// Assignments have no program, so they contribute nothing to [`heads_of`] and
/// cannot make one block look like a subset of another. They matter to the twin
/// comparison, which reads whole lines.
pub fn is_instruction(fragment: &str) -> bool {
    if head_of(fragment).is_some() {
        return true;
    }
    let text = without_comment(fragment.trim()).trim();
    text.split_whitespace()
        .next()
        .and_then(assignment_value)
        .is_some()
}

/// The set of command identities a group of command lines names.
pub fn heads_of(lines: &[String]) -> BTreeSet<String> {
    let mut heads = BTreeSet::new();
    for line in lines {
        for part in split_chain(line) {
            if let Some(head) = head_of(&part) {
                heads.insert(head);
            }
        }
    }
    heads
}

/// What separates one instruction from the next.
///
/// A pipeline is **not** here. `groove search ... | jq ...` is one command's
/// output feeding another's input, not two things a reader is told to run, and
/// splitting it would make every block that pipes look like a superset of every
/// block that does not.
///
/// `;` is here because this repository uses it: the Windows line of the service
/// migration note runs `schtasks /End ... ; schtasks /Delete ...`, beside a
/// Linux line and a macOS line that use `&&`. Leaving it out made the Windows
/// instruction the only one of the three that no guard compared.
const CHAIN_SEPARATORS: &[&str] = &["&&", "||", ";"];

/// Split a line into the instructions it chains together.
pub fn split_chain(line: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut rest = line;
    loop {
        let cut = CHAIN_SEPARATORS
            .iter()
            .filter_map(|sep| find_unquoted(rest, sep).map(|at| (at, sep.len())))
            .min_by_key(|(at, _)| *at);
        match cut {
            Some((at, width)) => {
                parts.push(rest[..at].trim().to_string());
                rest = &rest[at + width..];
            }
            None => {
                parts.push(rest.trim().to_string());
                break;
            }
        }
    }
    parts.retain(|p| !p.is_empty());
    parts
}

/// Every inline code span that chains two or more commands.
///
/// The floor of two commands is what keeps `` `groove service install` `` --
/// prose naming a command, which this repository does dozens of times -- out of
/// the corpus.
///
/// It is a floor on **commands**, not on distinct ones, and the difference is
/// the Windows line of the service migration note:
/// `schtasks /End ... ; schtasks /Delete ...` runs one program twice, and both
/// halves are instructions a reader follows. Requiring two distinct programs
/// would leave it uncompared beside a Linux line and a macOS line that are.
///
/// A caller that needs a stricter floor applies its own. The subset guard does:
/// a chain naming one program is a subset of any block that runs it, so it asks
/// for two distinct commands before treating a chain as a possible copy. That
/// belongs there, with its reason, rather than here where it would silently
/// narrow what every caller sees.
pub fn inline_chains(markdown: &str) -> Vec<InlineChain> {
    read_chains(markdown).0
}

/// The spans where the reader understood **some** of a chain and not the rest.
///
/// A span whose parts are all commands is a chain. A span where none of them are
/// is not one -- `` `a && b` `` in prose about boolean operators, a line of Rust
/// -- and ignoring it is right. Between those two is the shape that goes silent:
/// the reader recognises one half, fails on the other, and drops the span
/// whole, taking the half it understood with it. No guard is then comparing
/// anything, and none of them say so.
///
/// Returned separately rather than fixed in place because the two halves want
/// opposite things: a comparison needs whole chains, and a person needs to be
/// told the reader has met a syntax it cannot read.
pub fn half_read_chains(markdown: &str) -> Vec<InlineChain> {
    read_chains(markdown).1
}

/// Both halves of the split, from one pass: the chains, and the spans the reader
/// only partly understood.
///
/// What separates them decides how much doubt a partly-read span earns.
/// `&&` and `||` are shell and nothing else, so a span containing one is a chain
/// and failing to read half of it is worth saying out loud. `;` is not: it ends
/// a statement in Rust, separates parameters in a MIME type, and joins
/// PowerShell assignments, and this repository writes all three inside
/// backticks -- `text/plain; charset=utf-8` and `vec![Data::default(); rows *
/// cols]` are in the tree today. A span held together only by `;` therefore has
/// to be readable end to end to count as a chain at all, and is otherwise not
/// one rather than a half-read one.
fn read_chains(markdown: &str) -> (Vec<InlineChain>, Vec<InlineChain>) {
    let mut whole = Vec::new();
    let mut partial = Vec::new();
    for (event, range) in Parser::new_ext(markdown, github_flavour()).into_offset_iter() {
        let Event::Code(code) = event else {
            continue;
        };
        let commands: Vec<String> = split_chain(&code)
            .into_iter()
            .map(|p| p.split_whitespace().collect::<Vec<_>>().join(" "))
            .filter(|p| !p.is_empty())
            .collect();
        if commands.len() < 2 {
            continue;
        }
        let shell_only =
            find_unquoted(&code, "&&").is_some() || find_unquoted(&code, "||").is_some();
        // What counts as read depends on how much the separator already told us.
        // `&&` is shell, so a part that merely sets a variable is an instruction
        // like any other. `;` is not, so a span held together only by it has to
        // name a **program** in every part before it is a chain at all --
        // otherwise `text/plain; charset=utf-8` qualifies, `charset=utf-8` being
        // shaped exactly like an assignment and `text/plain` reading as a path.
        let read = commands
            .iter()
            .filter(|c| {
                if shell_only {
                    is_instruction(c)
                } else {
                    head_of(c).is_some()
                }
            })
            .count();
        let chain = InlineChain {
            line: line_of(markdown, range.start),
            text: code.split_whitespace().collect::<Vec<_>>().join(" "),
            commands,
        };
        if read == chain.commands.len() {
            whole.push(chain);
        } else if read > 0 && shell_only {
            partial.push(chain);
        }
    }
    (whole, partial)
}

/// The text that opens a pin marker.
pub const PIN_MARKER_OPEN: &str = "<!-- groove-pin:";

/// The id in a pin marker, if the text is one.
fn marker_id(html: &str) -> Option<String> {
    let at = html.find(PIN_MARKER_OPEN)?;
    let rest = &html[at + PIN_MARKER_OPEN.len()..];
    let end = rest.find("-->")?;
    let id = rest[..end].trim();
    if id.is_empty() {
        None
    } else {
        Some(id.to_string())
    }
}

/// Every pin marker in the document, with the block it resolves to.
///
/// The rule is adjacency, measured against pulldown-cmark rather than reasoned
/// about: a block-level `<!-- ... -->` arrives as `Start(HtmlBlock)`, `Html`,
/// `End(HtmlBlock)`, so what has to be a fenced code block is **the event after
/// `End(HtmlBlock)`**. Searching forward for the next code block instead would
/// step over an intervening paragraph and bind the pin to a block further down,
/// silently -- the same shape as keying a pin on its contents, which the guard
/// refuses for the same reason.
///
/// A marker written inside a paragraph becomes `InlineHtml` and never opens an
/// HTML block, so it resolves to nothing and is reported. So does a marker whose
/// next event is an indented code block: this module does not read those, and a
/// pin that reached one would hand an untagged body to a reader expecting a
/// tagged one.
pub fn pin_sites(markdown: &str) -> Vec<PinSite> {
    let events = events_of(markdown);
    let mut sites = Vec::new();
    let mut pending: Option<(String, usize)> = None;
    for (index, (event, range)) in events.iter().enumerate() {
        match event {
            Event::Html(html) | Event::InlineHtml(html) => {
                if let Some(id) = marker_id(html) {
                    let line = line_of(markdown, range.start);
                    if matches!(event, Event::InlineHtml(_)) {
                        // Never opens an HTML block, so it can never be adjacent
                        // to one. Reported rather than silently ignored.
                        sites.push(PinSite {
                            id,
                            line,
                            block: None,
                        });
                    } else {
                        pending = Some((id, line));
                    }
                }
            }
            Event::End(TagEnd::HtmlBlock) => {
                let Some((id, line)) = pending.take() else {
                    continue;
                };
                // Read out of the events already in hand, through the same
                // function that reads every other block. Re-parsing and matching
                // by line number would be the work again per pin, and a lookup
                // that missed would hand back an empty body -- a pin comparing
                // nothing against nothing.
                let block = match events.get(index + 1) {
                    Some((Event::Start(Tag::CodeBlock(CodeBlockKind::Fenced(info))), next)) => {
                        Some(FencedBlock {
                            line: line_of(markdown, next.start),
                            tag: fence_tag(info),
                            body: code_block_body(&events, index + 1),
                        })
                    }
                    _ => None,
                };
                sites.push(PinSite { id, line, block });
            }
            _ => {}
        }
    }
    sites
}
