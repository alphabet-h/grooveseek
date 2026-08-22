//! Every word `groove` writes to stderr is ASCII.
//!
//! `AGENTS.md` ("Results go to stdout, diagnostics to stderr, and stderr stays
//! ASCII") has said so since PR #166, and until this file nothing checked it.
//! Two rounds of review had already caught the same defect one instance at a
//! time -- an em dash in `links.rs` (PR #203), another in a test assertion
//! (PR #208) -- which is what an unenforced invariant looks like from the
//! inside.
//!
//! The reason is CP932: a Japanese Windows console renders an em dash, an
//! arrow or a kana as mojibake. `AGENTS.md` is precise about what that covers
//! -- "the words a diagnostic chooses, not the data it names" -- and the line
//! that draws is **written here or came from somewhere else**. What is spelled
//! out in the call is the author's choice and is checked, whatever slot it sits
//! in: `eprintln!("{}", '-')` chose that character as surely as the words
//! around it. What arrives at runtime is data and is not: a note called
//! `<Japanese>.md` coming out of `groove index` is normal operation.
//!
//! What a literal *contains* is not what it *emits*, either.
//! `eprintln!("\u{2014}")` is ASCII in the file and an em dash on the console,
//! so escapes are resolved before a literal is accepted.
//!
//! # Why this shape
//!
//! Three shapes were measured against the tree before one was picked.
//!
//! *Enumerating files with `include_str!`* is what the other structural tests
//! in this repo do, and it is wrong here: the path has to be a literal, so a
//! new `.rs` is silently outside the check. `main.rs` already carries a note
//! saying to check a shape rather than a list of files, written after an audit
//! named two pages and missed a third.
//!
//! *Cutting each file at its test module* looked like a way to scan only what
//! the binary ships, and it hides violations: `main.rs` names its test module
//! `documented_flags` rather than `tests`, and `tune.rs` has production code
//! *after* a `#[cfg(test)]` item. Both mistakes are silent. So nothing is cut,
//! and a diagnostic inside a `#[cfg(test)]` module in `src/` is held to the
//! same rule -- measured, that costs one message.
//!
//! *Requiring the whole call to be ASCII*, so no string parsing is needed at
//! all, over-reports: five calls in this tree carry a trailing comment in
//! Japanese on a code line. Literals have to be identified, so they are.
//!
//! # What is not scanned
//!
//! `tests/` is not walked. `AGENTS.md` puts a failing assertion's message
//! outside the rule -- it is printed by `cargo test` to a developer -- and
//! scanning this directory would make the guard match the opener list below in
//! its own source, a bug this repo has already shipped once (feature-51, cited
//! at `watcher.rs`'s sibling guard).
//!
//! **`assert!` is the one family that cannot be guarded this way.** Of 2921
//! call sites here, 255 carry non-ASCII -- and nearly all of it is the *data
//! under test*: Japanese headings, half-width kana, a Korean phrase, the CJK
//! strings the tokenizer exists to split. A scan cannot tell those from an em
//! dash in the message beside them, which is the distinction the whole rule
//! rests on. `AGENTS.md` puts a failing assertion outside the rule anyway,
//! since `cargo test` prints it to a developer.
//!
//! The panic family is not like that, which is why it *is* an opener: across
//! 614 call sites, two carried a non-ASCII character and both were messages
//! rather than data.
//!
//! # Local wrappers
//!
//! A `macro_rules!` that expands into a diagnostic carries its words at the
//! call sites rather than in its body, so listing only the standard macros
//! checks the wrapper's one-line body and none of the messages. `watcher.rs`'s
//! `wdiag!` is one, with some two dozen call sites. They are found by shape --
//! a macro whose body reaches a known opener -- so the next wrapper is covered
//! without anyone remembering. Adding that pass found an em dash the enumerated
//! openers had missed.
//!
//! # Every way a literal reaches stderr, counted
//!
//! Four review rounds each named another surface, so the surfaces were
//! enumerated and measured instead of waiting for a fifth:
//!
//! | surface | call sites | non-ASCII |
//! |---|---|---|
//! | the macro and panic openers below | see the list | 43, all fixed |
//! | indicatif's progress style | 2 | 1, fixed and now an opener |
//! | `write!` / `writeln!` **aimed at stderr** | **0** | — |
//! | `write!` / `writeln!` anywhere | 162 | 10, every one a stdout report |
//! | clap's `help` / `about` / `value_name` attributes | 111 | 0 |
//!
//! clap's error path was measured end to end as well, by running the binary
//! with bad flags: its output is ASCII. `--help` is not -- some flag
//! descriptions are in Japanese -- but that goes to stdout, where the rule
//! does not reach, and it is a language-policy question rather than this one.
//!
//! # Known limits
//!
//! A message **assembled from pieces** -- a `const` named in a format string, a
//! `&str` returned by a helper -- has its literal somewhere outside the call,
//! where nothing here looks. `AGENTS.md` says so where the rule is stated.
//!
//! A **raw byte string** (`br"..."`) would be read as an ordinary string, whose
//! escapes it does not have. Plain `b"..."` is fine -- its escapes are the same
//! ones, and Rust does not allow a non-ASCII character in it at all. There is
//! no `br"` in this tree; `b"` appears and is handled.
//!
//! Two limits that used to be listed here were wrong rather than known. Macros
//! written with brackets were said to fail loudly, and they failed silently
//! (see [`call_end`]). A trailing non-ASCII comment inside a call was said to
//! be reported, and it is not: comments are masked before anything is read,
//! which is what the third measured shape was rejected for not doing.

use std::path::{Path, PathBuf};

/// Calls whose string literals become the words groove writes to stderr.
///
/// A list is only as good as the reason it is complete, so here is that reason.
/// **The standard library writes to stderr from `eprint!`, `eprintln!`, `dbg!`
/// and the panic family, and from nothing else**; `print!` / `println!` /
/// `write!` to any other target are elsewhere by definition. Of the crates this
/// program depends on, `tracing` renders through a subscriber pointed at stderr
/// and `indicatif` draws the progress bar there. `log` is not a dependency, so
/// its macros cannot appear. A macro this repo defines itself is found by
/// shape rather than listed -- see [`wrapper_macros`].
///
/// Naming the crate is half the argument: **every entry point of it that takes
/// text has to be here too**. Listing indicatif's styling methods and not its
/// message-bearing ones left `bar.println` unguarded directly below the
/// `eprintln!` it mirrors, which is the asymmetry that makes this easy to get
/// wrong -- one branch of a `match` checked and the next one not.
///
/// `anyhow!` / `bail!` / `ensure!` / `context` are here because `main` prints
/// an error's body to stderr, so those messages are read on the same console
/// as the rest. The panic family is here for the same reason one level down:
/// Rust's default hook writes the payload of a `panic!` or an `.expect(...)`
/// to stderr, and a panic in a shipped binary is read by an operator.
///
/// **`write!` / `writeln!` are the second family that cannot be guarded this
/// way**, and adding them was tried rather than assumed: ten call sites carry
/// non-ASCII, and every one builds a `groove eval` or `groove tune` report --
/// which goes to stdout, where `AGENTS.md` says the formatters "may use them
/// freely". The same macro serves both destinations and the call does not say
/// which, so flagging them would flag exactly what the rule permits. Nothing
/// is lost today: no call in this tree aims either macro at stderr at all.
///
/// The `tracing` levels are listed bare so that `tracing::warn!` and a
/// `use tracing::warn;` shorthand are both caught by one entry.
const DIAGNOSTIC_OPENERS: &[&str] = &[
    "eprintln!",
    "eprint!",
    "dbg!",
    "warn!",
    "error!",
    "info!",
    "debug!",
    "trace!",
    "anyhow!",
    "bail!",
    "ensure!",
    "panic!",
    "unreachable!",
    "todo!",
    "unimplemented!",
    ".context(",
    ".with_context(",
    ".expect(",
    ".expect_err(",
    // indicatif draws the `groove index` progress bar to stderr on a TTY, so
    // every string it is handed is a word this program puts on that console
    // even though no message macro is involved. Naming the crate is not enough:
    // its styling entry points were listed and its message-bearing ones were
    // not, which left `bar.println` and `bar.set_message` unguarded directly
    // below the `eprintln!` they mirror (codex P2 on PR #213).
    ".progress_chars(",
    ".tick_chars(",
    "with_template(",
    ".template(",
    ".println(",
    ".set_message(",
    ".set_prefix(",
    ".finish_with_message(",
    ".abandon_with_message(",
];

/// The workspace root. `CARGO_MANIFEST_DIR` is `<root>/grooveseek` since the
/// workspace split (feature-44 PR-1).
fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("CARGO_MANIFEST_DIR is <workspace root>/grooveseek, which has a parent")
        .to_path_buf()
}

/// The source directory of every workspace member, read from the manifest.
///
/// The members list is the workspace's own answer to what this program is, so
/// a crate added later joins the check by being added to the workspace. An
/// earlier version walked `grooveseek/src` and `crates/*/src`: the same set
/// today, and one that would quietly skip a member placed anywhere else --
/// the shape of mistake this whole file exists to stop.
///
/// A member whose `src` is missing -- a glob in `members`, a moved crate --
/// fails here rather than contributing nothing.
fn source_dirs() -> Vec<PathBuf> {
    let root = workspace_root();
    let manifest_path = root.join("Cargo.toml");
    let text = std::fs::read_to_string(&manifest_path)
        .unwrap_or_else(|e| panic!("could not read {}: {e}", manifest_path.display()));
    let manifest: toml::Value = toml::from_str(&text)
        .unwrap_or_else(|e| panic!("could not parse {}: {e}", manifest_path.display()));
    let members = manifest
        .get("workspace")
        .and_then(|workspace| workspace.get("members"))
        .and_then(|members| members.as_array())
        .unwrap_or_else(|| panic!("{} declares no workspace members", manifest_path.display()));

    let mut dirs: Vec<PathBuf> = members
        .iter()
        .map(|member| {
            let name = member
                .as_str()
                .unwrap_or_else(|| panic!("a workspace member that is not a path: {member}"));
            root.join(name).join("src")
        })
        .collect();
    for dir in &dirs {
        assert!(
            dir.is_dir(),
            "a workspace member has no src directory, so nothing of it would be checked: {}",
            dir.display()
        );
    }
    dirs.sort();
    dirs
}

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut here: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
    here.sort();
    for path in here {
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

fn blank(masked: &mut [u8], from: usize, to: usize) {
    for byte in &mut masked[from..to] {
        *byte = b' ';
    }
}

fn preceded_by_ident(bytes: &[u8], at: usize) -> bool {
    at > 0 && (bytes[at - 1].is_ascii_alphanumeric() || bytes[at - 1] == b'_')
}

/// `Some(end)` when a char literal starts at `at`, `None` when it is a lifetime.
///
/// This matters for the mask below rather than for the rule: `'"'` and `'('`
/// both appear in this tree, and mistaking either for a string delimiter or a
/// paren would silently mis-read everything after it.
fn char_literal_end(src: &str, at: usize) -> Option<usize> {
    let bytes = src.as_bytes();
    let mut i = at + 1;
    if bytes.get(i) == Some(&b'\\') {
        i += 1;
        match bytes.get(i) {
            // `\u{...}` runs to its brace, `\xNN` takes exactly two more, and
            // every other escape stands for one character. Reading `'\x41'` as
            // a lifetime -- which is what happened before `\x` was handled --
            // leaves its closing quote to open a literal of its own and
            // mis-reads the rest of the file.
            Some(&b'u') => {
                while i < bytes.len() && bytes[i] != b'}' {
                    i += 1;
                }
            }
            Some(&b'x') => i += 2,
            _ => {}
        }
        i += 1;
    } else {
        let c = src.get(i..)?.chars().next()?;
        i += c.len_utf8();
    }
    (bytes.get(i) == Some(&b'\'')).then_some(i + 1)
}

/// One pass over a file: a copy with every comment, string and char literal
/// blanked out, plus the byte range of each string and char literal.
///
/// Blanking is what keeps an opener named inside a doc comment, or a paren
/// inside a message, from being read as code. Every range blanked starts and
/// ends on an ASCII delimiter, so a multi-byte character is never cut in half.
fn mask_comments_and_strings(src: &str) -> (Vec<u8>, Vec<(usize, usize)>) {
    let bytes = src.as_bytes();
    let mut masked = bytes.to_vec();
    let mut literals = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'/' && bytes.get(i + 1) == Some(&b'/') {
            let start = i;
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            blank(&mut masked, start, i);
            continue;
        }
        if bytes[i] == b'/' && bytes.get(i + 1) == Some(&b'*') {
            let start = i;
            let mut depth = 1usize;
            i += 2;
            while i < bytes.len() && depth > 0 {
                if bytes[i] == b'/' && bytes.get(i + 1) == Some(&b'*') {
                    depth += 1;
                    i += 2;
                } else if bytes[i] == b'*' && bytes.get(i + 1) == Some(&b'/') {
                    depth -= 1;
                    i += 2;
                } else {
                    i += 1;
                }
            }
            let end = i.min(bytes.len());
            blank(&mut masked, start, end);
            i = end;
            continue;
        }
        if bytes[i] == b'r' && !preceded_by_ident(bytes, i) {
            let mut j = i + 1;
            let mut hashes = 0usize;
            while bytes.get(j) == Some(&b'#') {
                hashes += 1;
                j += 1;
            }
            if bytes.get(j) == Some(&b'"') {
                let start = i;
                j += 1;
                while j < bytes.len() {
                    if bytes[j] == b'"' {
                        let mut k = j + 1;
                        let mut seen = 0usize;
                        while seen < hashes && bytes.get(k) == Some(&b'#') {
                            seen += 1;
                            k += 1;
                        }
                        if seen == hashes {
                            j = k;
                            break;
                        }
                    }
                    j += 1;
                }
                let end = j.min(bytes.len());
                literals.push((start, end));
                blank(&mut masked, start, end);
                i = end;
                continue;
            }
        }
        if bytes[i] == b'"' {
            let start = i;
            i += 1;
            while i < bytes.len() {
                match bytes[i] {
                    // An escape consumes whatever follows, a newline included:
                    // that is how a Rust literal continues onto the next line,
                    // and reading it any other way ends the literal early. The
                    // scan that first sized this job did exactly that and
                    // reported 2 violations where there were 42.
                    b'\\' => i += 2,
                    b'"' => {
                        i += 1;
                        break;
                    }
                    _ => i += 1,
                }
            }
            let end = i.min(bytes.len());
            literals.push((start, end));
            blank(&mut masked, start, end);
            continue;
        }
        if bytes[i] == b'\'' {
            if let Some(end) = char_literal_end(src, i) {
                // A char literal inside a diagnostic is a character the author
                // chose, so it is checked like a string: `eprintln!("{}", '-')`
                // with an em dash there emits one. It is only ever looked at
                // when it falls inside a diagnostic call, so the `'.'` and
                // `'\u{3000}'` that live in this tree's tokenizer tables are
                // not affected.
                literals.push((i, end));
                blank(&mut masked, i, end);
                i = end;
            } else {
                i += 1; // a lifetime
            }
            continue;
        }
        i += 1;
    }
    (masked, literals)
}

/// A source file read once: how to name it in a failure, its text, the masked
/// copy, and the byte range of every string and char literal in it.
struct Scanned {
    shown: String,
    src: String,
    masked: Vec<u8>,
    literals: Vec<(usize, usize)>,
}

/// Local macros that expand into a diagnostic.
///
/// Such a macro carries its words at the **call sites**, not in the body, so
/// listing only the standard macros checks the wrapper's own one-line body and
/// none of the messages. `watcher.rs`'s `wdiag!` is the one that exists today
/// and it has some two dozen call sites; the first version of this test looked
/// at none of them.
///
/// Finding them by shape — a `macro_rules!` whose body reaches a known opener —
/// means the next wrapper is covered by existing rather than by being added
/// here. One level deep is enough for this tree; a wrapper of a wrapper is not
/// recognised.
fn wrapper_macros(masked: &[u8]) -> Vec<String> {
    const DECL: &[u8] = b"macro_rules!";
    let mut found = Vec::new();
    for at in occurrences(masked, DECL) {
        let mut i = at + DECL.len();
        while i < masked.len() && masked[i].is_ascii_whitespace() {
            i += 1;
        }
        let start = i;
        while i < masked.len() && (masked[i].is_ascii_alphanumeric() || masked[i] == b'_') {
            i += 1;
        }
        let Ok(name) = std::str::from_utf8(&masked[start..i]) else {
            continue;
        };
        if name.is_empty() {
            continue;
        }
        // `macro_rules! foo ( .. );` is as legal as the brace form, so where a
        // definition's body ends is the same question a call asks, and gets the
        // same implementation (AGENTS.md, "One question gets one
        // implementation"). It had its own brace-only answer, which is the
        // defect codex found one function over.
        let body = &masked[i..call_end(masked, i)];
        if DIAGNOSTIC_OPENERS
            .iter()
            .any(|opener| !occurrences(body, opener.as_bytes()).is_empty())
        {
            found.push(name.to_string());
        }
    }
    found
}

/// Byte just past the delimiter closing the call whose body opens at `from`.
///
/// A macro takes any of the three delimiters, and `eprintln![..]` compiles. A
/// scan that counted only parentheses ended at the first `)` of a nested call
/// and never saw the literals after it -- `eprintln!["{}", value(), "..."]`
/// would have its last argument skipped **silently**, which is the one failure
/// mode this file cannot afford (codex P2 on PR #213).
///
/// So the body's own delimiter decides. Anything between the opener and it is
/// whitespace in code that compiles; if something else is there, the opener was
/// not a call and the span is empty.
fn call_end(masked: &[u8], from: usize) -> usize {
    let mut i = from;
    while i < masked.len() && masked[i].is_ascii_whitespace() {
        i += 1;
    }
    let (open, close) = match masked.get(i) {
        Some(b'(') => (b'(', b')'),
        Some(b'[') => (b'[', b']'),
        Some(b'{') => (b'{', b'}'),
        _ => return i,
    };
    let mut depth = 0usize;
    while i < masked.len() {
        if masked[i] == open {
            depth += 1;
        } else if masked[i] == close {
            depth -= 1;
            if depth == 0 {
                return i + 1;
            }
        }
        i += 1;
    }
    masked.len()
}

fn occurrences(haystack: &[u8], needle: &[u8]) -> Vec<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return Vec::new();
    }
    haystack
        .windows(needle.len())
        .enumerate()
        .filter(|(_, window)| *window == needle)
        .map(|(at, _)| at)
        .collect()
}

/// The non-ASCII characters a literal actually puts on the console.
///
/// Takes a string literal or a char literal; both spell their escapes the
/// same way.
///
/// Reading the source spelling is not enough: `eprintln!("\u{2014}")` is nine
/// ASCII characters in the file and an em dash on the console, so a guard that
/// only looks at the source accepts exactly the output it promises to reject
/// (codex P2 on PR #213).
///
/// `\u{...}` is the only escape that can do this. Rust caps `\x` at `0x7F`
/// inside a string literal, every other escape stands for an ASCII character,
/// and a raw string has no escapes at all -- which is why a raw literal, whose
/// span starts at its `r`, is read verbatim.
fn offending_characters(literal: &str) -> String {
    let mut seen: Vec<char> = Vec::new();
    if literal.starts_with('r') {
        seen.extend(literal.chars().filter(|c| !c.is_ascii()));
    } else {
        let mut chars = literal.chars().peekable();
        while let Some(c) = chars.next() {
            if !c.is_ascii() {
                seen.push(c);
                continue;
            }
            if c != '\\' {
                continue;
            }
            // Any escape consumes the character after the backslash. Only
            // `\u{...}` can stand for something outside ASCII.
            if chars.next() != Some('u') || chars.peek() != Some(&'{') {
                continue;
            }
            chars.next();
            let mut hex = String::new();
            for h in chars.by_ref() {
                if h == '}' {
                    break;
                }
                hex.push(h);
            }
            // Rust allows underscores anywhere among those digits, including a
            // trailing one: `\u{20_14}`, `\u{2_0_1_4}` and `\u{2014_}` all
            // compile and all mean an em dash (measured with rustc, not read).
            // Leaving them in makes `from_str_radix` fail, and a failed parse
            // reads as "nothing to report" (codex P2 on PR #213).
            hex.retain(|c| c != '_');
            // An unparseable escape would not compile, so this cannot hide one.
            if let Ok(point) = u32::from_str_radix(&hex, 16)
                && point > 127
            {
                seen.push(char::from_u32(point).unwrap_or(char::REPLACEMENT_CHARACTER));
            }
        }
    }
    seen.sort_unstable();
    seen.dedup();
    seen.iter().flat_map(|c| c.escape_default()).collect()
}

#[test]
fn every_word_groove_writes_to_stderr_is_ascii() {
    let root = workspace_root();
    let mut offenders: Vec<String> = Vec::new();
    let mut calls = 0usize;

    // Read and mask every file once, because the wrapper pass below has to see
    // the whole tree before the checking pass can start.
    let mut scanned: Vec<Scanned> = Vec::new();
    for dir in source_dirs() {
        let mut paths = Vec::new();
        rust_files(&dir, &mut paths);
        for path in paths {
            let src = std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("could not read {}: {e}", path.display()))
                .replace("\r\n", "\n");
            let (masked, literals) = mask_comments_and_strings(&src);
            let shown = path
                .strip_prefix(&root)
                .unwrap_or(&path)
                .display()
                .to_string()
                .replace('\\', "/");
            scanned.push(Scanned {
                shown,
                src,
                masked,
                literals,
            });
        }
    }

    let mut openers: Vec<String> = DIAGNOSTIC_OPENERS
        .iter()
        .map(|s| (*s).to_string())
        .collect();
    for file in &scanned {
        for name in wrapper_macros(&file.masked) {
            let call = format!("{name}!");
            if !openers.contains(&call) {
                openers.push(call);
            }
        }
    }

    for Scanned {
        shown,
        src,
        masked,
        literals,
    } in &scanned
    {
        for opener in &openers {
            let macro_call = !opener.ends_with('(');
            for at in occurrences(masked, opener.as_bytes()) {
                if macro_call && preceded_by_ident(masked, at) {
                    continue; // `some_warn!`, not `warn!`
                }
                calls += 1;
                // A macro opener stops at `!`, so the scan looks ahead for
                // the paren; `.context(` already sits on one.
                let scan_from = if macro_call {
                    at + opener.len()
                } else {
                    at + opener.len() - 1
                };
                let end = call_end(masked, scan_from);
                for (start, stop) in literals.iter().copied() {
                    if start < at || stop > end {
                        continue;
                    }
                    // Not `text.is_ascii()`: a literal can be ASCII in the file
                    // and not on the console. `offending_characters` resolves
                    // `\u{...}` before answering.
                    let carried = offending_characters(&src[start..stop]);
                    if carried.is_empty() {
                        continue;
                    }
                    let line = src[..start].matches('\n').count() + 1;
                    offenders.push(format!("{shown}:{line}: {opener} carries {carried}"));
                }
            }
        }
    }

    // A walk that finds nothing passes, so say what was walked. Moving the
    // source tree must break this test rather than quietly satisfy it.
    assert!(
        !scanned.is_empty(),
        "no source files were walked under {}",
        root.display()
    );
    assert!(
        calls > 0,
        "no diagnostic calls were found in {} source file(s)",
        scanned.len()
    );
    // The wrapper pass is the difference between checking two dozen watcher
    // messages and checking none of them, and it fails silently if the shape it
    // looks for stops matching.
    assert!(
        openers.len() > DIAGNOSTIC_OPENERS.len(),
        "no local macro expanding into a diagnostic was found; this tree has one"
    );

    offenders.sort();
    offenders.dedup();
    assert!(
        offenders.is_empty(),
        "a diagnostic contributes characters a CP932 console renders as mojibake \
         (AGENTS.md: stderr stays ASCII; the rule is about the words, not the data \
         interpolated into them):\n{}",
        offenders.join("\n")
    );
}
