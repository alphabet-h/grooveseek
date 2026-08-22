//! Encoding of what `powershell.exe` writes back to us (OS-independent half).
//!
//! `windows.rs` only compiles on the Windows leg, so a mistake in here would be
//! invisible to the Linux / macOS legs. The logic is pure string handling, so it
//! lives here and is compiled + tested everywhere — same reasoning as
//! [`super::render`].
//!
//! ## Why the prelude is needed
//!
//! A redirected `powershell.exe` stdout/stderr is encoded in the **active code
//! page**, not UTF-8. Measured on Japanese Windows 11 / Windows PowerShell 5.1,
//! writing `日` (U+65E5):
//!
//! | script | bytes on stdout |
//! |---|---|
//! | `Write-Output …` | `93 fa` (CP932) |
//! | `[Console]::OutputEncoding=[Text.Encoding]::UTF8; Write-Output …` | `e6 97 a5` (UTF-8) |
//!
//! CP932 is not valid UTF-8 (`0x93` is a continuation byte, `0xfa` is not a
//! legal lead byte), so `String::from_utf8_lossy` collapses the whole run into
//! U+FFFD. That is how a localized cmdlet error became unreadable, and — worse —
//! how the tray's `.lnk` path came back corrupted for a user whose profile
//! directory contains non-ASCII characters.
//!
//! Three further properties were measured rather than assumed, because each one
//! would have broken the fix:
//!
//! - **It covers stderr too.** A localized cmdlet error (`Get-Item` on a missing
//!   drive) arrives as CP932 without the prelude and as UTF-8 with it.
//! - **No BOM is emitted.** `[Text.Encoding]::UTF8` is the BOM-carrying
//!   encoding, but .NET strips the preamble when it rebuilds the console writer,
//!   so no `EF BB BF` reaches the pipe. This matters because a leading BOM would
//!   have corrupted the `.lnk` path just as thoroughly as the mojibake did.
//! - **It does not need a console.** Setting `[Console]::OutputEncoding` still
//!   succeeds when the child is spawned from a GUI-subsystem parent with
//!   `CREATE_NO_WINDOW`, i.e. exactly the shape a release `groove-tray.exe` has.
//!
//! `crates/groove-tray/src/powershell.rs` carries the same three functions. The
//! duplication is deliberate: `groove` already depends on `groove-tray`, so the
//! tray cannot depend back on this crate without a cycle.

use anyhow::{Result, anyhow};

/// The statement that makes PowerShell write UTF-8 to a redirected
/// stdout / stderr.
///
/// Keeps its trailing `;` so it is also correct when joined to a script with a
/// space rather than a newline.
pub const UTF8_OUTPUT_PRELUDE: &str = "[Console]::OutputEncoding=[Text.Encoding]::UTF8;";

/// Prefix `script` with [`UTF8_OUTPUT_PRELUDE`].
///
/// Applied by whoever spawns the process, not by the individual script
/// builders: the encoding of the pipe is a property of how we read the child,
/// not of what the script says. Keeping it at the single spawn site is also what
/// stops a future script from being added without it.
pub fn with_utf8_output(script: &str) -> String {
    format!("{UTF8_OUTPUT_PRELUDE}\n{script}")
}

/// Decode output that the caller turns into a **value** (a path, a JSON
/// document).
///
/// Strict on purpose. Lossy decoding substitutes U+FFFD and returns `Ok`, so a
/// `.lnk` path built from CP932 bytes would be silently wrong and nothing
/// downstream could tell. `what` names the stream for the error message.
pub fn decode_value(bytes: &[u8], what: &str) -> Result<String> {
    match std::str::from_utf8(bytes) {
        Ok(s) => Ok(s.to_string()),
        Err(e) => Err(anyhow!(
            "{what} is not valid UTF-8 (first bad byte at offset {}), so it cannot be used as a \
             value. PowerShell was asked to emit UTF-8 via `{UTF8_OUTPUT_PRELUDE}`, and that \
             prelude \
             did not take effect.",
            e.valid_up_to(),
        )),
    }
}

/// Decode output that is only going into a **diagnostic message**.
///
/// Never fails: an error path must not lose the error it was reporting. When the
/// bytes were not valid UTF-8 the replacement is called out, so a mojibake
/// message says why it is mojibake instead of looking like corrupted data.
pub fn decode_diagnostic(bytes: &[u8]) -> String {
    match std::str::from_utf8(bytes) {
        Ok(s) => s.to_string(),
        Err(_) => format!(
            "{} [not valid UTF-8; some characters replaced]",
            String::from_utf8_lossy(bytes)
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// CP932 for `日本`, as measured from a default `powershell.exe` stdout.
    const CP932_NIHON: &[u8] = &[0x93, 0xfa, 0x96, 0x7b];

    /// The two crates cannot share this constant (the dependency only runs one
    /// way), so the literal itself is pinned on both sides.
    /// `crates/groove-tray/src/powershell.rs` asserts the same text.
    #[test]
    fn prelude_text_matches_the_tray_copy() {
        assert_eq!(
            UTF8_OUTPUT_PRELUDE,
            "[Console]::OutputEncoding=[Text.Encoding]::UTF8;"
        );
    }

    #[test]
    fn prelude_precedes_the_script() {
        let s = with_utf8_output("Write-Output 'ok'");
        assert!(
            s.starts_with(UTF8_OUTPUT_PRELUDE),
            "prelude must come first: {s}"
        );
        assert!(s.contains("Write-Output 'ok'"), "script must survive: {s}");
    }

    /// The prelude has to be a complete statement on its own. Without the
    /// trailing separator it would fuse with the first line of the script.
    #[test]
    fn prelude_is_a_terminated_statement() {
        assert!(UTF8_OUTPUT_PRELUDE.ends_with(';'));
        let s = with_utf8_output("$ErrorActionPreference='Stop'");
        let first = s.lines().next().expect("first line");
        assert_eq!(first, UTF8_OUTPUT_PRELUDE);
    }

    /// A multi-line script keeps every one of its lines.
    #[test]
    fn multi_line_script_is_preserved_verbatim() {
        let script = "$a = 1\n$b = 2\nWrite-Output ($a + $b)\n";
        let s = with_utf8_output(script);
        assert!(
            s.ends_with(script),
            "script must be appended unchanged: {s}"
        );
    }

    #[test]
    fn utf8_value_round_trips() {
        let out = decode_value("C:\\Users\\山田\\x.lnk".as_bytes(), "stdout").expect("valid UTF-8");
        assert_eq!(out, "C:\\Users\\山田\\x.lnk");
    }

    /// The regression this whole module exists for: CP932 must not be accepted
    /// as a value. Before the fix these bytes became four U+FFFD and were used
    /// as a filesystem path.
    #[test]
    fn cp932_value_is_rejected_rather_than_mangled() {
        let err = decode_value(CP932_NIHON, "stdout").expect_err("CP932 must not decode");
        let msg = err.to_string();
        assert!(msg.contains("not valid UTF-8"), "unexpected message: {msg}");
        assert!(
            msg.contains(UTF8_OUTPUT_PRELUDE),
            "the message must name the prelude that failed to apply: {msg}"
        );
    }

    #[test]
    fn diagnostic_decode_is_transparent_for_utf8() {
        assert_eq!(
            decode_diagnostic("ドライブが見つかりません".as_bytes()),
            "ドライブが見つかりません"
        );
    }

    #[test]
    fn diagnostic_decode_flags_replacement() {
        let s = decode_diagnostic(CP932_NIHON);
        assert!(s.contains('\u{fffd}'), "lossy replacement expected: {s}");
        assert!(
            s.contains("not valid UTF-8"),
            "mojibake must explain itself: {s}"
        );
    }

    /// An empty stream is normal (a script that only has side effects) and must
    /// not be reported as an encoding failure.
    #[test]
    fn empty_output_is_not_an_encoding_error() {
        assert_eq!(decode_value(b"", "stdout").expect("empty is valid"), "");
        assert_eq!(decode_diagnostic(b""), "");
    }
}
