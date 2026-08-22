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
//! -- "the words a diagnostic chooses, not the data it names" -- so this scan
//! reads string literals and never the values interpolated into them. A note
//! called `<Japanese>.md` coming out of `groove index` is normal operation.
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
//! # Known limits, all of which fail loudly
//!
//! A macro written with brackets (`eprintln![..]`) is not recognised as a call
//! and the paren scan runs past it. A trailing comment carrying non-ASCII on a
//! code line inside a diagnostic call is reported. Neither can hide a
//! violation; both produce a failure naming the file.

use std::path::{Path, PathBuf};

/// Calls whose string literals become the words groove writes to stderr.
///
/// `anyhow!` / `bail!` / `ensure!` / `context` are here because `main` prints
/// an error's body to stderr, so those messages are read on the same console
/// as the rest. `println!` is absent on purpose: stdout is a result.
///
/// The `tracing` levels are listed bare so that `tracing::warn!` and a
/// `use tracing::warn;` shorthand are both caught by one entry.
const DIAGNOSTIC_OPENERS: &[&str] = &[
    "eprintln!",
    "eprint!",
    "warn!",
    "error!",
    "info!",
    "debug!",
    "trace!",
    "anyhow!",
    "bail!",
    "ensure!",
    ".context(",
    ".with_context(",
];

/// The workspace root. `CARGO_MANIFEST_DIR` is `<root>/grooveseek` since the
/// workspace split (feature-44 PR-1).
fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("CARGO_MANIFEST_DIR is <workspace root>/grooveseek, which has a parent")
        .to_path_buf()
}

/// The directories holding the program's own source, discovered rather than
/// listed so a new crate joins the check by existing.
fn source_dirs() -> Vec<PathBuf> {
    let root = workspace_root();
    let mut dirs = vec![root.join("grooveseek").join("src")];
    if let Ok(entries) = std::fs::read_dir(root.join("crates")) {
        let mut found: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path().join("src"))
            .filter(|p| p.is_dir())
            .collect();
        found.sort();
        dirs.extend(found);
    }
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
        if bytes.get(i) == Some(&b'u') {
            while i < bytes.len() && bytes[i] != b'}' {
                i += 1;
            }
        }
        i += 1;
    } else {
        let c = src.get(i..)?.chars().next()?;
        i += c.len_utf8();
    }
    (bytes.get(i) == Some(&b'\'')).then_some(i + 1)
}

/// One pass over a file: a copy with every comment, string and char literal
/// blanked out, plus the byte range of each string literal.
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

/// Byte just past the paren that closes the call starting at `from`.
fn call_end(masked: &[u8], from: usize) -> usize {
    let mut depth = 0usize;
    let mut i = from;
    while i < masked.len() {
        match masked[i] {
            b'(' => depth += 1,
            b')' => {
                if depth == 0 {
                    return i; // the opener was not a call after all
                }
                depth -= 1;
                if depth == 0 {
                    return i + 1;
                }
            }
            _ => {}
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

/// The characters a message contributes that a CP932 console cannot render,
/// rendered so this failure is itself readable there.
fn offending_characters(literal: &str) -> String {
    let mut seen: Vec<char> = literal.chars().filter(|c| !c.is_ascii()).collect();
    seen.sort_unstable();
    seen.dedup();
    seen.iter().flat_map(|c| c.escape_default()).collect()
}

#[test]
fn every_word_groove_writes_to_stderr_is_ascii() {
    let root = workspace_root();
    let mut offenders: Vec<String> = Vec::new();
    let mut files = 0usize;
    let mut calls = 0usize;

    for dir in source_dirs() {
        let mut paths = Vec::new();
        rust_files(&dir, &mut paths);
        for path in paths {
            files += 1;
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

            for opener in DIAGNOSTIC_OPENERS {
                let macro_call = !opener.ends_with('(');
                for at in occurrences(&masked, opener.as_bytes()) {
                    if macro_call && preceded_by_ident(&masked, at) {
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
                    let end = call_end(&masked, scan_from);
                    for (start, stop) in literals.iter().copied() {
                        if start < at || stop > end {
                            continue;
                        }
                        let text = &src[start..stop];
                        if text.is_ascii() {
                            continue;
                        }
                        let line = src[..start].matches('\n').count() + 1;
                        offenders.push(format!(
                            "{shown}:{line}: {} carries {}",
                            opener,
                            offending_characters(text)
                        ));
                    }
                }
            }
        }
    }

    // A walk that finds nothing passes, so say what was walked. Moving the
    // source tree must break this test rather than quietly satisfy it.
    assert!(
        files > 0,
        "no source files were walked under {}",
        root.display()
    );
    assert!(
        calls > 0,
        "no diagnostic calls were found in {files} source file(s)"
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
