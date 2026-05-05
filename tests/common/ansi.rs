//! ANSI escape sequence stripping for subprocess stderr assertions.
//!
//! Windows tracing-subscriber emits CSI color codes on stderr by default, so
//! integration tests that grep stderr need to strip them first. The matcher
//! recognises CSI sequences (`ESC [ ... <ASCII alphabetic>`); other ESC
//! sequences (e.g. `ESC c`) pass through untouched. Test-only helper, so
//! allocation efficiency is secondary to clarity.

/// Strip ANSI CSI sequences from `s`. Non-CSI ESC sequences pass through.
pub fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' && chars.peek() == Some(&'[') {
            chars.next(); // consume '['
            for ch in chars.by_ref() {
                if ch.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}
