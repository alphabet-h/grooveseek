//! Encoding of what `powershell.exe` writes back to us.
//!
//! A redirected `powershell.exe` stdout/stderr is encoded in the **active code
//! page**, not UTF-8. Measured on Japanese Windows 11 / Windows PowerShell 5.1,
//! writing `日` (U+65E5): `93 fa` (CP932) by default, `e6 97 a5` (UTF-8) once
//! [`UTF8_OUTPUT_PRELUDE`] runs first. CP932 is not valid UTF-8, so
//! `String::from_utf8_lossy` used to flatten the whole run into U+FFFD.
//!
//! For the tray that was never only a display problem. [`install`] returns the
//! created `.lnk` path *through this pipe* and the caller turns it into a
//! `PathBuf`, so on an account whose profile directory contains non-ASCII
//! characters the stored path was silently wrong.
//!
//! Two properties were measured rather than assumed, because either one would
//! have broken the fix: `[Text.Encoding]::UTF8` is the BOM-carrying encoding,
//! but .NET strips the preamble when it rebuilds the console writer, so no
//! `EF BB BF` reaches the pipe and prefixes the path; and setting
//! `[Console]::OutputEncoding` still succeeds when the child is spawned from a
//! GUI-subsystem parent with `CREATE_NO_WINDOW`, which is exactly the shape a
//! release `groove-tray.exe` has.
//!
//! **Kept in lockstep with `grooveseek/src/service/powershell.rs`**, which carries
//! the same three functions and the full measurement table. The duplication is
//! deliberate: `groove` depends on this crate, so this crate cannot depend back
//! on it without a cycle.
//!
//! [`install`]: crate::install

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
/// not of what the script says.
pub fn with_utf8_output(script: &str) -> String {
    format!("{UTF8_OUTPUT_PRELUDE}\n{script}")
}

/// Creation flag every `powershell.exe` spawn in this crate must carry, so that
/// Windows does not give the child a console of its own.
///
/// The tray is built as a GUI-subsystem binary and therefore owns no console.
/// `powershell.exe` is a console-subsystem program, so when it is started
/// without this flag Windows **allocates a fresh console for it** — a window
/// that flashes on screen for the lifetime of the call. Measured from a
/// `windows_subsystem = "windows"` parent with every stdio handle piped, the
/// child reporting its own `GetConsoleWindow()`:
///
/// | Creation flags | `GetConsoleWindow()` in the child |
/// |---|---|
/// | (none) | non-zero — it has a window |
/// | `CREATE_NO_WINDOW` | `0` |
///
/// Redirecting stdout / stderr does not prevent the allocation; only the flag
/// does. The same fix already sits on the logon path, where v0.9.1 introduced
/// the GUI-subsystem `groove-svc.exe` to detach-spawn the daemon without a
/// console — the tray's own PowerShell calls were simply never given the same
/// treatment.
///
/// Taken from the Win32 constant rather than written as a literal so it cannot
/// drift; pinned by a test below.
pub const CREATE_NO_WINDOW: u32 = windows::Win32::System::Threading::CREATE_NO_WINDOW.0;

/// Decode output that the caller turns into a **value** — here, the `.lnk` path
/// and the duplicate-check JSON.
///
/// Strict on purpose. Lossy decoding substitutes U+FFFD and returns `Ok`, so a
/// path built from code-page bytes would be wrong in a way nothing downstream
/// could detect. `what` names the stream for the error message.
pub fn decode_value(bytes: &[u8], what: &str) -> Result<String> {
    match std::str::from_utf8(bytes) {
        Ok(s) => Ok(s.to_string()),
        Err(e) => Err(anyhow!(
            "{what} is not valid UTF-8 (first bad byte at offset {}), so it cannot be used as a \
             value. PowerShell was asked to emit UTF-8 via `{UTF8_OUTPUT_PRELUDE}` — that prelude \
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
    /// way), so the literal itself is pinned on both sides. `groove`'s copy
    /// asserts the same text.
    #[test]
    fn prelude_text_matches_the_grooveseek_copy() {
        assert_eq!(
            UTF8_OUTPUT_PRELUDE,
            "[Console]::OutputEncoding=[Text.Encoding]::UTF8;"
        );
    }

    /// `CREATE_NO_WINDOW` is documented as `0x0800_0000`. Taking it from the
    /// `windows` crate keeps it honest, but a wrong-but-plausible value would
    /// still compile and would silently restore the flashing window, so the
    /// number is pinned here as well.
    #[test]
    fn create_no_window_is_the_documented_flag() {
        assert_eq!(CREATE_NO_WINDOW, 0x0800_0000);
    }

    /// The flag changes how the child is created, so it could plausibly break
    /// the pipes this module reads back — which would turn a cosmetic fix into
    /// a functional regression. Spawning for real is the only way to know.
    ///
    /// Windows-only by construction (the whole crate is), and cheap: one
    /// `powershell.exe` that echoes a non-ASCII string, so it also re-checks
    /// that the flag and the UTF-8 prelude coexist.
    #[test]
    fn output_is_still_captured_with_the_flag_applied() {
        use std::os::windows::process::CommandExt as _;

        let out = std::process::Command::new("powershell.exe")
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                &with_utf8_output("Write-Output '日本'"),
            ])
            .creation_flags(CREATE_NO_WINDOW)
            .output()
            .expect("spawn powershell");

        assert!(
            out.status.success(),
            "powershell failed: {}",
            decode_diagnostic(&out.stderr)
        );
        let stdout = decode_value(&out.stdout, "stdout").expect("stdout must still be UTF-8");
        assert_eq!(
            stdout.trim(),
            "日本",
            "CREATE_NO_WINDOW must not disturb the captured pipe"
        );
    }

    #[test]
    fn prelude_precedes_the_script() {
        let s = with_utf8_output("Write-Output $lnk");
        assert!(
            s.starts_with(UTF8_OUTPUT_PRELUDE),
            "prelude must come first: {s}"
        );
        assert!(s.contains("Write-Output $lnk"), "script must survive: {s}");
    }

    /// The install scripts are multi-line; appending must not disturb them.
    #[test]
    fn multi_line_script_is_preserved_verbatim() {
        let script = "$ErrorActionPreference='Stop'\n$startup = 1\nWrite-Output $startup\n";
        let s = with_utf8_output(script);
        assert!(
            s.ends_with(script),
            "script must be appended unchanged: {s}"
        );
        assert_eq!(s.lines().next(), Some(UTF8_OUTPUT_PRELUDE));
    }

    #[test]
    fn utf8_value_round_trips() {
        let out = decode_value("C:\\Users\\山田\\x.lnk".as_bytes(), "stdout").expect("valid UTF-8");
        assert_eq!(out, "C:\\Users\\山田\\x.lnk");
    }

    /// The regression this module exists for: these bytes used to become four
    /// U+FFFD and were then handed to `PathBuf::from`.
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

    #[test]
    fn empty_output_is_not_an_encoding_error() {
        assert_eq!(decode_value(b"", "stdout").expect("empty is valid"), "");
        assert_eq!(decode_diagnostic(b""), "");
    }
}
