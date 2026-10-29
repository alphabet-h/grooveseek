//! Reading a Rust source file as the shipped binary sees it, for the tests
//! that scan the source for where a call may appear (ADR-0031's writer
//! opens). Line-based on purpose: the scanned files are rustfmt-formatted
//! (`cargo fmt --check` runs in CI), so a module or function ends on a line
//! holding only `}` at the indentation it started at.

/// The `(1-based line number, line)` pairs of the source that are production code:
/// every line except
/// - a `#[cfg(test)]` line followed by `mod <name> {` (also `pub mod` /
///   `pub(crate) mod`), through the line holding only `}` at that `mod`
///   line's indentation;
/// - a line that starts with `//` once leading whitespace is trimmed
///   (indented comments included; a `//` later in the line is kept).
///
/// A `#[cfg(test)]` on anything other than an inline `mod` is kept.
pub fn production_lines(src: &str) -> Vec<(usize, &str)> {
    let lines: Vec<&str> = src.lines().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        if line.trim() == "#[cfg(test)]"
            && let Some(next) = lines.get(i + 1)
            && is_inline_mod_open(next)
        {
            let close = format!("{}}}", indent_of(next));
            i = lines[i + 2..]
                .iter()
                .position(|l| l.trim_end() == close)
                .map_or(lines.len(), |p| i + 2 + p + 1);
            continue;
        }
        if !line.trim_start().starts_with("//") {
            out.push((i + 1, line));
        }
        i += 1;
    }
    out
}

/// Every `(start, end)` (1-based, inclusive) where the start line contains
/// `needle` and the end line is the first line after it holding only `}` at
/// the indentation of the start line. A match with no such line is left out.
pub fn fn_ranges(src: &str, needle: &str) -> Vec<(usize, usize)> {
    let lines: Vec<&str> = src.lines().collect();
    let mut out = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        if !line.contains(needle) {
            continue;
        }
        let close = format!("{}}}", indent_of(line));
        if let Some(p) = lines[i + 1..].iter().position(|l| l.trim_end() == close) {
            out.push((i + 1, i + 1 + p + 1));
        }
    }
    out
}

/// The leading whitespace of `line`.
fn indent_of(line: &str) -> &str {
    &line[..line.len() - line.trim_start().len()]
}

/// `mod <name> {`, optionally `pub` / `pub(crate)`, on one line.
fn is_inline_mod_open(line: &str) -> bool {
    let t = line.trim();
    let t = t
        .strip_prefix("pub(crate) ")
        .or_else(|| t.strip_prefix("pub "))
        .unwrap_or(t);
    let Some(rest) = t.strip_prefix("mod ") else {
        return false;
    };
    let Some(name) = rest.strip_suffix('{') else {
        return false;
    };
    let name = name.trim_end();
    !name.is_empty() && name.chars().all(|c| c.is_alphanumeric() || c == '_')
}
