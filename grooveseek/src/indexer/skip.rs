//! (#660) Why a file present on disk was not indexed: the public
//! [`crate::indexer::skip::SkippedFile`] / [`crate::indexer::skip::SkipReason`] types, the one
//! classification of a parse failure ([`crate::indexer::skip::reason_of_parse_error`]) and of a
//! read failure ([`crate::indexer::skip::read_failure_of`]), and the run-local
//! [`crate::indexer::skip::SkipLedger`] the rebuild loop keeps.
//!
//! Every name lives here once: the reason names of the MCP reply only in
//! [`crate::indexer::skip::SkipReason::kind`], the `[index]` keys
//! ([`crate::config::IndexConfig`]) only in [`crate::indexer::skip::LimitKey::key`] and
//! [`crate::indexer::skip::SkipReason::limit_key`].

use std::collections::BTreeMap;

use crate::parser::{ParseCause, Unindexable};

/// One file this run did not index, and why. (#660)
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkippedFile {
    /// Relative to the knowledge base root, forward slashes -- the spelling `documents.path` uses. For a name the
    /// index cannot hold ([`SkipReason::UnspellableName`]) it is the walk's lossy spelling.
    pub path: String,
    pub reason: SkipReason,
}

/// Why a file present on disk was not indexed. Grows by addition: match with a `_` arm.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum SkipReason {
    /// Over the file size cap, measured by a stat or on the read handle.
    TooLarge {
        size: u64,
        limit: u64,
        key: LimitKey,
    },
    /// Decompressed bytes or extracted text passed `[index].max_decompressed_size`
    /// ([`crate::config::IndexConfig::max_decompressed_size`]).
    OverExtractionLimit { limit: u64 },
    /// Over a fixed size limit of the parser itself that no key raises (source code over the
    /// code parser's raw byte cap).
    OverParserLimit { size: u64, limit: u64 },
    /// Text extraction ran past its time limit (no key sets it).
    ExtractionTimedOut { seconds: u64 },
    /// Password-protected / encrypted (PDF, or an OOXML file wrapped in an encrypted container).
    Encrypted,
    /// The bytes are not a readable document of this format: corrupt, truncated, a legacy binary
    /// format under an OOXML extension, a crafted archive, or a parser panic on them.
    CorruptOrUnsupported,
    /// A text-format file (.md / .txt / code) whose bytes are not UTF-8 (e.g. Shift_JIS).
    NotUtf8,
    /// A PDF whose text layer decoded to mojibake.
    TextUndecodable,
    /// A PDF that yields too little text per page (a scan with no text layer, slides, figures).
    TooLittleText,
    /// Read and parsed, and holds nothing to embed (empty, frontmatter-only).
    NoText,
    /// The file could not be stat'ed or read.
    Unreadable { cause: ReadFailure },
    /// The embedding endpoint refused the input (HTTP 400 / 413 / 422).
    EmbedRejected { http_status: u16 },
    /// The bytes changed between the scan and the read; the next run reads it again.
    ChangedDuringRead,
    /// Not a regular file at read time (a symlink, device or pipe swapped in after the walk). A file
    /// the walk already saw as a symlink, or a path through a link, is left out on purpose and has
    /// no reason.
    NotPlainFile,
    /// The file has more than one name (a hard link).
    HardLink,
    /// The name is not one the `get_document` tool ([`crate::server`]) or a `kb://` URI can take
    /// (ADR-0023). `directory`: a whole
    /// directory was left out, counted once, whether or not it held a file the index would have
    /// read (its subtree is never walked). Show it as a folder, not as an unreadable file.
    UnspellableName { directory: bool },
}

/// The `[index]` key ([`crate::config::IndexConfig`]) that sets a file size cap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum LimitKey {
    /// `[index].max_binary_file_size` ([`crate::config::IndexConfig::max_binary_file_size`]):
    /// pdf / docx / xlsx / pptx.
    MaxBinaryFileSize,
    /// `[index].max_text_file_size` ([`crate::config::IndexConfig::max_text_file_size`]):
    /// md / txt / source code.
    MaxTextFileSize,
}

/// Why a file could not be stat'ed or read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ReadFailure {
    /// Another process holds the file open without sharing it (Windows).
    InUse,
    /// The OS refused access.
    PermissionDenied,
    /// The file disappeared between the walk and the read.
    Gone,
    /// Any other I/O failure.
    Other,
}

impl SkipReason {
    /// The stable snake_case name the MCP reply uses (`"too_large"`, `"encrypted"`, ...). ASCII.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::TooLarge { .. } => "too_large",
            Self::OverExtractionLimit { .. } => "over_extraction_limit",
            Self::OverParserLimit { .. } => "over_parser_limit",
            Self::ExtractionTimedOut { .. } => "extraction_timed_out",
            Self::Encrypted => "encrypted",
            Self::CorruptOrUnsupported => "corrupt_or_unsupported",
            Self::NotUtf8 => "not_utf8",
            Self::TextUndecodable => "text_undecodable",
            Self::TooLittleText => "too_little_text",
            Self::NoText => "no_text",
            Self::Unreadable { .. } => "unreadable",
            Self::EmbedRejected { .. } => "embed_rejected",
            Self::ChangedDuringRead => "changed_during_read",
            Self::NotPlainFile => "not_plain_file",
            Self::HardLink => "hard_link",
            Self::UnspellableName { .. } => "unspellable_name",
        }
    }

    /// The key under `[index]` ([`crate::config::IndexConfig`]) that would admit this file:
    /// [`Self::TooLarge`] → its [`LimitKey`]'s [`LimitKey::key`], [`Self::OverExtractionLimit`] →
    /// `Some("max_decompressed_size")`, every other variant (including [`Self::OverParserLimit`],
    /// a fixed limit) → `None`. The MCP reply takes its limit key from here only.
    pub fn limit_key(&self) -> Option<&'static str> {
        match self {
            Self::TooLarge { key, .. } => Some(key.key()),
            Self::OverExtractionLimit { .. } => Some("max_decompressed_size"),
            Self::OverParserLimit { .. }
            | Self::ExtractionTimedOut { .. }
            | Self::Encrypted
            | Self::CorruptOrUnsupported
            | Self::NotUtf8
            | Self::TextUndecodable
            | Self::TooLittleText
            | Self::NoText
            | Self::Unreadable { .. }
            | Self::EmbedRejected { .. }
            | Self::ChangedDuringRead
            | Self::NotPlainFile
            | Self::HardLink
            | Self::UnspellableName { .. } => None,
        }
    }
}

impl LimitKey {
    /// The key under `[index]` ([`crate::config::IndexConfig`]): `"max_binary_file_size"` / `"max_text_file_size"`.
    pub fn key(&self) -> &'static str {
        match self {
            Self::MaxBinaryFileSize => "max_binary_file_size",
            Self::MaxTextFileSize => "max_text_file_size",
        }
    }
}

impl ReadFailure {
    /// `"in_use"` / `"permission_denied"` / `"gone"` / `"other"`.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::InUse => "in_use",
            Self::PermissionDenied => "permission_denied",
            Self::Gone => "gone",
            Self::Other => "other",
        }
    }
}

impl From<ParseCause> for SkipReason {
    fn from(cause: ParseCause) -> Self {
        match cause {
            ParseCause::Encrypted => Self::Encrypted,
            ParseCause::OverExtractionLimit { limit } => Self::OverExtractionLimit { limit },
            ParseCause::OverParserLimit { size, limit } => Self::OverParserLimit { size, limit },
            ParseCause::ExtractionTimedOut { seconds } => Self::ExtractionTimedOut { seconds },
            ParseCause::CorruptOrUnsupported => Self::CorruptOrUnsupported,
            ParseCause::TextUndecodable => Self::TextUndecodable,
            ParseCause::TooLittleText => Self::TooLittleText,
        }
    }
}

/// The one classification of a parse failure (#660). Read by the rebuild loop and, through it,
/// by the watcher's entry. A parser's typed mark wins; a UTF-8 decode error anywhere under the
/// context layers is [`SkipReason::NotUtf8`]; anything else is
/// [`SkipReason::CorruptOrUnsupported`].
pub(crate) fn reason_of_parse_error(e: &anyhow::Error) -> SkipReason {
    if let Some(u) = e.downcast_ref::<Unindexable>() {
        return u.cause.into();
    }
    if e.downcast_ref::<std::str::Utf8Error>().is_some() {
        return SkipReason::NotUtf8;
    }
    SkipReason::CorruptOrUnsupported
}

/// The one classification of a stat / read failure (#660), shared by the scan and the read.
/// The sharing / lock violations (`ERROR_SHARING_VIOLATION` 32, `ERROR_LOCK_VIOLATION` 33) are
/// read by raw code and on Windows only: on Unix 32 is `EPIPE` and 33 is `EDOM`.
pub(crate) fn read_failure_of(e: &std::io::Error) -> ReadFailure {
    match e.kind() {
        std::io::ErrorKind::PermissionDenied => return ReadFailure::PermissionDenied,
        std::io::ErrorKind::NotFound => return ReadFailure::Gone,
        _ => {}
    }
    if cfg!(windows) && matches!(e.raw_os_error(), Some(32 | 33)) {
        return ReadFailure::InUse;
    }
    ReadFailure::Other
}

/// The run-local list of skipped files, one entry per kb-relative path. Only [`Self::upsert`] and
/// [`Self::remove`] change it; [`Self::finish`] turns it into the path-ordered list a run returns.
#[derive(Debug, Default)]
pub(crate) struct SkipLedger(BTreeMap<String, SkipReason>);

impl SkipLedger {
    /// Record the given reason for `rel`, replacing any earlier reason for the same path.
    pub(crate) fn upsert(&mut self, rel: &str, reason: SkipReason) {
        self.0.insert(rel.to_owned(), reason);
    }

    /// Forget `rel` (it was indexed after all, or left the disk). A path not present is a no-op.
    pub(crate) fn remove(&mut self, rel: &str) {
        self.0.remove(rel);
    }

    /// Whether `rel` currently has a reason.
    pub(crate) fn contains(&self, rel: &str) -> bool {
        self.0.contains_key(rel)
    }

    /// The entries in path order.
    pub(crate) fn finish(self) -> Vec<SkippedFile> {
        self.0
            .into_iter()
            .map(|(path, reason)| SkippedFile { path, reason })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds_are_unique_snake_case_ascii() {
        let all = [
            SkipReason::TooLarge {
                size: 2,
                limit: 1,
                key: LimitKey::MaxTextFileSize,
            },
            SkipReason::OverExtractionLimit { limit: 1 },
            SkipReason::OverParserLimit { size: 2, limit: 1 },
            SkipReason::ExtractionTimedOut { seconds: 1 },
            SkipReason::Encrypted,
            SkipReason::CorruptOrUnsupported,
            SkipReason::NotUtf8,
            SkipReason::TextUndecodable,
            SkipReason::TooLittleText,
            SkipReason::NoText,
            SkipReason::Unreadable {
                cause: ReadFailure::Other,
            },
            SkipReason::EmbedRejected { http_status: 413 },
            SkipReason::ChangedDuringRead,
            SkipReason::NotPlainFile,
            SkipReason::HardLink,
            SkipReason::UnspellableName { directory: false },
        ];
        let kinds: Vec<&str> = all.iter().map(SkipReason::kind).collect();
        let expected = [
            "too_large",
            "over_extraction_limit",
            "over_parser_limit",
            "extraction_timed_out",
            "encrypted",
            "corrupt_or_unsupported",
            "not_utf8",
            "text_undecodable",
            "too_little_text",
            "no_text",
            "unreadable",
            "embed_rejected",
            "changed_during_read",
            "not_plain_file",
            "hard_link",
            "unspellable_name",
        ];
        assert_eq!(kinds, expected, "the names spec section 5.1 lists");
        let unique: std::collections::BTreeSet<&str> = kinds.iter().copied().collect();
        assert_eq!(unique.len(), kinds.len(), "kind() names collide: {kinds:?}");
        let read_kinds: Vec<&str> = [
            ReadFailure::InUse,
            ReadFailure::PermissionDenied,
            ReadFailure::Gone,
            ReadFailure::Other,
        ]
        .iter()
        .map(ReadFailure::kind)
        .collect();
        assert_eq!(read_kinds, ["in_use", "permission_denied", "gone", "other"]);
        // snake_case ASCII: a lowercase letter first, then lowercase letters, digits (`not_utf8`)
        // and underscores.
        for k in kinds.iter().chain(read_kinds.iter()) {
            assert!(
                k.bytes().next().is_some_and(|b| b.is_ascii_lowercase())
                    && k.bytes()
                        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'),
                "{k:?} is not [a-z][a-z0-9_]*"
            );
        }
    }

    #[test]
    fn limit_keys_match_the_config_field_names() {
        assert_eq!(LimitKey::MaxBinaryFileSize.key(), "max_binary_file_size");
        assert_eq!(LimitKey::MaxTextFileSize.key(), "max_text_file_size");
        assert_eq!(
            SkipReason::OverExtractionLimit { limit: 1 }.limit_key(),
            Some("max_decompressed_size")
        );
        assert_eq!(
            SkipReason::OverParserLimit { size: 2, limit: 1 }.limit_key(),
            None
        );
        assert_eq!(SkipReason::Encrypted.limit_key(), None);
        assert_eq!(
            SkipReason::TooLarge {
                size: 2,
                limit: 1,
                key: LimitKey::MaxBinaryFileSize,
            }
            .limit_key(),
            Some("max_binary_file_size")
        );

        // The keys are the `[index]` fields the config reads: `IndexConfig` denies unknown
        // fields, so a key that drifted from the field name fails to parse here.
        let decompressed = SkipReason::OverExtractionLimit { limit: 1 }
            .limit_key()
            .unwrap();
        let toml = format!(
            "[index]\n{} = \"1 MiB\"\n{} = \"2 MiB\"\n{} = \"3 MiB\"\n",
            LimitKey::MaxBinaryFileSize.key(),
            LimitKey::MaxTextFileSize.key(),
            decompressed,
        );
        let cfg: crate::config::Config =
            toml::from_str(&toml).unwrap_or_else(|e| panic!("{toml}\n{e}"));
        let index = cfg.index.expect("[index] parsed");
        assert!(index.max_binary_file_size.is_some(), "{toml}");
        assert!(index.max_text_file_size.is_some(), "{toml}");
        assert!(index.max_decompressed_size.is_some(), "{toml}");
    }

    #[test]
    fn reason_of_parse_error_reads_the_mark_utf8_and_falls_back() {
        use crate::parser::{ParseCause, unindexable};
        use anyhow::Context;

        let marked = unindexable(ParseCause::Encrypted, "cannot open PDF".into())
            .context("a.pdf")
            .context("outer");
        assert_eq!(reason_of_parse_error(&marked), SkipReason::Encrypted);

        let limited = unindexable(
            ParseCause::OverParserLimit { size: 9, limit: 4 },
            "too big".into(),
        );
        assert_eq!(
            reason_of_parse_error(&limited),
            SkipReason::OverParserLimit { size: 9, limit: 4 }
        );

        // Shift_JIS hiragana "a", not UTF-8; built at run time so the decode is not a constant.
        let bytes: Vec<u8> = vec![0x82, 0xa0];
        let not_utf8 = std::str::from_utf8(&bytes)
            .map(|_| ())
            .with_context(|| "a.md: not valid UTF-8")
            .unwrap_err();
        assert_eq!(reason_of_parse_error(&not_utf8), SkipReason::NotUtf8);

        assert_eq!(
            reason_of_parse_error(&anyhow::anyhow!("x")),
            SkipReason::CorruptOrUnsupported
        );
    }

    #[test]
    fn every_parse_cause_maps_to_the_variant_of_the_same_name() {
        use crate::parser::ParseCause;
        let pairs = [
            (ParseCause::Encrypted, SkipReason::Encrypted),
            (
                ParseCause::OverExtractionLimit { limit: 7 },
                SkipReason::OverExtractionLimit { limit: 7 },
            ),
            (
                ParseCause::OverParserLimit { size: 8, limit: 7 },
                SkipReason::OverParserLimit { size: 8, limit: 7 },
            ),
            (
                ParseCause::ExtractionTimedOut { seconds: 3 },
                SkipReason::ExtractionTimedOut { seconds: 3 },
            ),
            (
                ParseCause::CorruptOrUnsupported,
                SkipReason::CorruptOrUnsupported,
            ),
            (ParseCause::TextUndecodable, SkipReason::TextUndecodable),
            (ParseCause::TooLittleText, SkipReason::TooLittleText),
        ];
        for (cause, reason) in pairs {
            assert_eq!(SkipReason::from(cause), reason);
        }
    }

    #[test]
    fn the_ledger_keeps_one_entry_per_path_in_path_order() {
        let mut ledger = SkipLedger::default();
        ledger.upsert("b.md", SkipReason::NotUtf8);
        ledger.upsert("a.md", SkipReason::NoText);
        ledger.upsert("b.md", SkipReason::HardLink);
        assert!(ledger.contains("a.md"));
        assert!(ledger.contains("b.md"));
        assert!(!ledger.contains("c.md"));

        let mut again = SkipLedger::default();
        again.upsert("b.md", SkipReason::NotUtf8);
        again.upsert("a.md", SkipReason::NoText);
        again.upsert("b.md", SkipReason::HardLink);
        assert_eq!(
            again.finish(),
            vec![
                SkippedFile {
                    path: "a.md".into(),
                    reason: SkipReason::NoText,
                },
                SkippedFile {
                    path: "b.md".into(),
                    reason: SkipReason::HardLink,
                },
            ]
        );

        ledger.remove("a.md");
        ledger.remove("not-there.md");
        assert!(!ledger.contains("a.md"));
        assert_eq!(
            ledger.finish(),
            vec![SkippedFile {
                path: "b.md".into(),
                reason: SkipReason::HardLink,
            }]
        );
    }

    #[test]
    fn read_failures_classify_by_kind() {
        use std::io::{Error, ErrorKind};
        assert_eq!(
            read_failure_of(&Error::from(ErrorKind::PermissionDenied)),
            ReadFailure::PermissionDenied
        );
        assert_eq!(
            read_failure_of(&Error::from(ErrorKind::NotFound)),
            ReadFailure::Gone
        );
        assert_eq!(read_failure_of(&Error::other("boom")), ReadFailure::Other);
    }

    #[cfg(windows)]
    #[test]
    fn sharing_violation_codes_are_in_use_on_windows() {
        use std::io::Error;
        assert_eq!(
            read_failure_of(&Error::from_raw_os_error(32)),
            ReadFailure::InUse
        );
        assert_eq!(
            read_failure_of(&Error::from_raw_os_error(33)),
            ReadFailure::InUse
        );
    }

    #[cfg(unix)]
    #[test]
    fn epipe_and_edom_are_not_in_use_on_unix() {
        use std::io::Error;
        assert_ne!(
            read_failure_of(&Error::from_raw_os_error(32)),
            ReadFailure::InUse
        );
        assert_ne!(
            read_failure_of(&Error::from_raw_os_error(33)),
            ReadFailure::InUse
        );
    }
}
