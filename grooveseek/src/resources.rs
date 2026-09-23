//! The MCP `resources` surface: `kb://` URIs over the indexed corpus.
//!
//! # What is listed, and what is not
//!
//! `resources/list` returns one resource per **topic group** — the first one or
//! two path segments, which is exactly what the indexer derives `category` and
//! `topic` from. That is tens of entries on a knowledge base with hundreds of
//! documents.
//!
//! Individual documents are *not* enumerated. They are reachable two other ways:
//! the `kb://doc/{path}` template in `resources/templates/list`, and the `uri`
//! field `search` now puts on its hits. The specification allows this
//! explicitly — "Resource links returned by tools are not guaranteed to appear
//! in the results of a `resources/list` request" — and every large-corpus MCP
//! server surveyed does the same, while the one that enumerates per document
//! serves a handful of demo files.
//!
//! What is offered is decided one level up, by `KbCore::servable_document_paths`:
//! the indexed paths minus those the active parser registry can no longer open.
//! This module builds URIs and takes them apart; it does not know the corpus.
//!
//! # The URI shape
//!
//! - `kb://topic/<prefix>` — a group; `kb://topic/` is the root group, the
//!   documents that sit directly in the knowledge base.
//! - `kb://doc/<relpath>` — one document.
//!
//! `file://` was not used. It leaks the host's absolute paths to the client and
//! invites the specification's own warning about sanitising paths when serving
//! `file://` resources; a relative key is what `get_document` and the database
//! already use.
//!
//! Separators are always forward slashes and everything else is
//! percent-encoded. The specification states no normalisation rules and the
//! `ignore`-crate-free path here validates nothing on its own, so the rules are
//! written down and tested rather than assumed. The knowledge base this was
//! built against happens to have no non-ASCII path and no spaces, which is
//! precisely why the encoder is tested against paths that do — an encoder that
//! never fires is an encoder nobody has checked.

use std::collections::BTreeMap;

/// The scheme, and the two namespaces under it.
pub const SCHEME: &str = "kb";
const TOPIC_PREFIX: &str = "kb://topic/";
const DOC_PREFIX: &str = "kb://doc/";

/// What a `kb://` URI names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResourceUri {
    /// A topic group, keyed by its path prefix. Empty means the root group.
    Topic(String),
    /// One document, by its knowledge-base-relative path.
    Doc(String),
}

/// One entry in `resources/list`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TopicGroup {
    /// Path prefix, forward-slashed. Empty for the root group.
    pub prefix: String,
    /// Documents whose path falls under it, sorted.
    pub paths: Vec<String>,
}

impl TopicGroup {
    /// What a client shows in a list.
    pub fn display_name(&self) -> String {
        if self.prefix.is_empty() {
            "(root)".to_string()
        } else {
            self.prefix.clone()
        }
    }

    pub fn uri(&self) -> String {
        topic_uri(&self.prefix)
    }

    pub fn description(&self) -> String {
        let n = self.paths.len();
        format!(
            "{n} indexed document{} under {}",
            if n == 1 { "" } else { "s" },
            self.display_name()
        )
    }
}

/// The prefix a path is grouped under: its first one or two segments.
///
/// Deliberately the same derivation the indexer uses for `category` and
/// `topic`, so a group and the database agree without a second query to keep in
/// step. `notes/a.md` groups under `notes`; `deep-dive/mcp/overview.md` under
/// `deep-dive/mcp`; `readme.md` under the root.
pub fn group_prefix(rel: &str) -> String {
    let parts: Vec<&str> = rel.split('/').filter(|s| !s.is_empty()).collect();
    match parts.len() {
        0 | 1 => String::new(),
        2 => parts[0].to_string(),
        _ => format!("{}/{}", parts[0], parts[1]),
    }
}

/// Group indexed paths for `resources/list`.
///
/// Built from the same list of paths a read is checked against, so a URI that
/// appears in the listing cannot fail membership when it is read back.
pub fn topic_groups(paths: &[String]) -> Vec<TopicGroup> {
    let mut by_prefix: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for p in paths {
        by_prefix
            .entry(group_prefix(p))
            .or_default()
            .push(p.clone());
    }
    by_prefix
        .into_iter()
        .map(|(prefix, mut paths)| {
            paths.sort();
            TopicGroup { prefix, paths }
        })
        .collect()
}

pub fn topic_uri(prefix: &str) -> String {
    format!("{TOPIC_PREFIX}{}", encode_path(prefix))
}

pub fn doc_uri(rel: &str) -> String {
    format!("{DOC_PREFIX}{}", encode_path(rel))
}

/// The `kb://doc/{path}` template advertised by `resources/templates/list`.
pub fn doc_uri_template() -> String {
    format!("{DOC_PREFIX}{{path}}")
}

/// Parse a `kb://` URI back into what it names.
///
/// `None` for anything this server does not serve, and — importantly — for
/// anything that decodes into a path that could climb out of the knowledge
/// base. The traversal check happens **after** decoding: `%2e%2e%2f` is `../`,
/// and a check that ran first would not see it.
pub fn parse(uri: &str) -> Option<ResourceUri> {
    let (kind, rest) = if let Some(rest) = uri.strip_prefix(DOC_PREFIX) {
        ("doc", rest)
    } else {
        ("topic", uri.strip_prefix(TOPIC_PREFIX)?)
    };

    let decoded = decode_path(rest)?;
    match kind {
        "doc" if doc_is_addressable(&decoded) => Some(ResourceUri::Doc(decoded)),
        "doc" => None,
        _ if is_safe_relative(&decoded) => Some(ResourceUri::Topic(
            decoded.trim_end_matches('/').to_string(),
        )),
        _ => None,
    }
}

/// Whether `rel` is a name the index can hold -- and so one a `kb://doc/` URI
/// can carry and [`parse`] will read back, and one `get_document` will look up.
///
/// **The one predicate for that question** (ADR-0023). The index walk and the
/// watcher skip what it refuses, `groove doctor` reports rows that fail it,
/// the side that hands a URI out asks it, [`parse`] asks it of a `kb://doc/`
/// URI, and the document path check behind `get_document` asks it before
/// anything on disk is looked at. So a search hit can never name a document
/// that cannot be opened by the name it was given.
///
/// On top of `is_safe_relative` (plain code, not a link: this item is `pub`
/// and that one is not), which also answers for topic prefixes, a
/// document name may not be empty and may hold no `.` segment and no empty
/// one (`./a.md`, `a//b.md`, a trailing `/`). Those are other spellings of a
/// path, on every platform; the walk never produces one. Split on `/` only:
/// where `\` is an ordinary filename character (Unix), `\\host\x.md` is one
/// name, not two empty segments.
pub fn doc_is_addressable(rel: &str) -> bool {
    !rel.is_empty()
        && is_safe_relative(rel)
        && !rel.split('/').any(|seg| seg.is_empty() || seg == ".")
}

/// A decoded path may be used against the knowledge base only if it stays
/// inside it and names something on this side of the OS's path syntax.
///
/// Two surfaces ask it, and for a document name both ask it through
/// [`doc_is_addressable`], the predicate for that question. The `kb://` side
/// does so in [`parse`], for a URI being read, and before a URI is handed out;
/// [`parse`] asks this function directly only for a topic prefix. The path
/// check in [`crate::server`] behind `get_document` and `get_best_practice`
/// asks it of the requested string before anything on disk
/// is looked at (AW-01): `Path::join` replaces the knowledge base with an
/// absolute right-hand side, so an absolute path, a drive or a UNC share would
/// otherwise be stat'ed wherever it points -- outside the knowledge base, or
/// across the network on Windows. Document names go through
/// [`doc_is_addressable`], which adds what only a document name has to satisfy
/// (not empty, no `.` or empty segment); topic prefixes come here directly.
/// Neither surface keeps a copy of the rule
/// (AGENTS.md, "One question gets one implementation"). The empty string
/// passes here: [`parse`] reads it as the root topic group.
///
/// **`\` is refused only where it separates components**
/// ([`crate::indexer::backslash_separates_components`], the same answer the
/// index uses when it spells a path). On Unix it is an ordinary filename
/// character: the index holds a file named `secret\pay.md` under exactly that
/// name, and refusing it here left the server unable to read a URI it had
/// handed out. Letting it through there is not a way out of the knowledge
/// base -- `\` does not separate anything on that platform, so `\\host\x.md`
/// is one filename in the knowledge base root, and the leading-slash check
/// still refuses absolute paths. The URI caller then resolves what survives
/// against the index rather than the filesystem, and both callers go on
/// through the checks `get_document` applies.
///
/// The `..` check splits on `\` as well as `/`, on every platform. That is
/// deliberately the cautious side: it gives up a Unix file literally named
/// `a\..\b.md` -- through `get_document` too, since that asks this function
/// as well -- and in return nothing downstream that reads `\` as a separator
/// can ever be handed a `..`.
///
/// On Windows four more things are refused: a colon anywhere
/// ([`holds_a_windows_colon`] -- a drive or an alternate data stream, Codex
/// round 2 on #319); a segment that names a device
/// ([`names_a_windows_device`]), so no request reaches the null device or a
/// port by being spelled `NUL` or `COM1` (Codex round 1 on #319); a segment
/// that ends in a dot or a space ([`ends_where_win32_trims`]); and a character
/// Win32 refuses in a name ([`holds_a_character_win32_refuses`]). Every one of
/// them is a name Win32 either reads as something else or will not open, so a
/// file the index holds under it could be found and never opened (ADR-0023).
pub(crate) fn is_safe_relative(p: &str) -> bool {
    if p.contains('\0') {
        return false;
    }
    if p.contains('\\') && crate::indexer::backslash_separates_components() {
        return false;
    }
    if p.starts_with('/') || holds_a_windows_colon(p) || holds_a_character_win32_refuses(p) {
        return false;
    }
    !p.split(['/', '\\'])
        .any(|seg| seg == ".." || names_a_windows_device(seg) || ends_where_win32_trims(seg))
}

/// The names Windows reserves for devices, as [`names_a_windows_device`]
/// compares them (ASCII case ignored; the superscript digits have no case).
const WINDOWS_DEVICE_NAMES: &[&str] = &[
    "CON",
    "PRN",
    "AUX",
    "NUL",
    "CONIN$",
    "CONOUT$",
    "COM0",
    "COM1",
    "COM2",
    "COM3",
    "COM4",
    "COM5",
    "COM6",
    "COM7",
    "COM8",
    "COM9",
    "COM\u{b9}",
    "COM\u{b2}",
    "COM\u{b3}",
    "LPT0",
    "LPT1",
    "LPT2",
    "LPT3",
    "LPT4",
    "LPT5",
    "LPT6",
    "LPT7",
    "LPT8",
    "LPT9",
    "LPT\u{b9}",
    "LPT\u{b2}",
    "LPT\u{b3}",
];

/// Whether `segment` is a name Windows can read as a device rather than a
/// file -- `NUL`, `CON`, `COM1` and the rest -- in any case, followed by an
/// extension or by trailing spaces or dots. The colon form, `NUL:`, never
/// reaches here: [`holds_a_windows_colon`] refuses every colon first.
///
/// **Windows only**, like [`holds_a_windows_colon`] and for the same reason:
/// elsewhere these are ordinary names.
///
/// The list is the reserved names of "Naming Files, Paths, and Namespaces"
/// (<https://learn.microsoft.com/en-us/windows/win32/fileio/naming-a-file>),
/// which also says `NUL.txt` and `NUL.tar.gz` are both `NUL`. Two kinds are
/// added to it: `CONIN$` and `CONOUT$`, which Windows 11 (build 26200)
/// reports as device names when they stand alone though the page does not
/// list them; and `COM0` and `LPT0`, which neither the page lists nor that
/// build reads as devices, refused on the cautious side since the page's own
/// namespace section shows a `COM0` device (via `GetFullPathNameW` and
/// `RtlIsDosDeviceName_U`, called from PowerShell on that build).
///
/// How much of this a given Windows still does varies, which is why the rule
/// follows the page rather than one build: on build 26200 only `NUL` (with
/// trailing dots, spaces or a colon) still turns into the device behind a
/// directory, and under the verbatim `\\?\` prefix a canonical knowledge base
/// carries, nothing does. A file some Windows lets exist under such a name,
/// `CON.md` for one, is not indexed either: the walk and the watcher ask
/// [`doc_is_addressable`] too (ADR-0023), so no search hit names it.
fn names_a_windows_device(segment: &str) -> bool {
    if !cfg!(windows) {
        return false;
    }
    let stem = segment
        .split('.')
        .next()
        .unwrap_or("")
        .trim_end_matches(' ');
    WINDOWS_DEVICE_NAMES
        .iter()
        .any(|name| stem.eq_ignore_ascii_case(name))
}

/// Whether `p` holds a colon, on Windows, where a colon in a request can only
/// be path syntax and never part of a name:
///
/// - a drive designator, which escapes the knowledge base in every form --
///   `C:\x`, `C:/x`, and the drive-*relative* `C:notes.md`, which resolves
///   against that drive's own current directory;
/// - an alternate data stream of a file, `note.md:secret` or the default
///   stream `note.md::$DATA` (Codex round 2 on #319), which the stat would
///   answer about before the spelling check refused it;
/// - the colon form of a device, `NUL:`, which becomes `\\.\NUL` behind a
///   directory on Windows 11 build 26200 (via `GetFullPathNameW`, called from
///   PowerShell on that build).
///
/// Refusing it anywhere loses no document: "Naming Files, Paths, and
/// Namespaces" (<https://learn.microsoft.com/en-us/windows/win32/fileio/naming-a-file>)
/// lists the colon among the characters a Windows file or directory name
/// cannot contain, so no file the index walked has one. This replaces the
/// drive-designator check that stood here, which it covers completely.
///
/// **Windows only.** Elsewhere a colon is an ordinary filename character:
/// `a:b.md` and `C:/note.md` (a directory literally named `C:`) are both
/// ordinary relative paths on Unix, and this module hands out URIs for them.
/// Nothing is lost by the narrowing -- the colon reaches here percent-decoded,
/// the leading-slash check already refuses Unix absolute paths, and a name
/// with a colon in it is inside the knowledge base on Unix, wherever it is
/// then looked up.
fn holds_a_windows_colon(p: &str) -> bool {
    cfg!(windows) && p.contains(':')
}

/// Whether `segment` ends in a dot or a space, on Windows, where Win32 strips
/// both from the end of a path component before it looks anything up: `b.md.`
/// and `b.md ` open `b.md`, and `dir./x.md` opens `dir/x.md` -- another file,
/// or none. `.` and `..` are segments of their own and are answered elsewhere
/// ([`doc_is_addressable`], and the `..` check in [`is_safe_relative`]).
///
/// **Windows only.** Elsewhere a trailing dot or space is part of the name.
fn ends_where_win32_trims(segment: &str) -> bool {
    cfg!(windows) && segment != "." && segment != ".." && segment.ends_with(['.', ' '])
}

/// Whether `p` holds a character "Naming Files, Paths, and Namespaces"
/// (<https://learn.microsoft.com/en-us/windows/win32/fileio/naming-a-file>)
/// says a Windows name cannot contain -- `< > " | ? *` and the control
/// characters 1 through 31 -- on Windows. A request holding one used to reach
/// the disk and come back as `ERROR_INVALID_NAME`, answered "unavailable"
/// rather than "not found" (AW-40). The colon is [`holds_a_windows_colon`];
/// NUL is refused on every platform in [`is_safe_relative`].
///
/// **Windows only.** Elsewhere these are ordinary filename characters.
fn holds_a_character_win32_refuses(p: &str) -> bool {
    cfg!(windows)
        && p.chars()
            .any(|c| matches!(c, '<' | '>' | '"' | '|' | '?' | '*' | '\u{1}'..='\u{1f}'))
}

/// The line the index walk and the watcher log for each entry they leave out
/// because [`doc_is_addressable`] refuses its name (ADR-0023). One function so
/// the two stay in step, the way [`crate::links::refusal_reason`] does for
/// hard links. ASCII, since it goes to stderr (AGENTS.md).
pub(crate) fn unspellable_reason(path: &std::path::Path, is_dir: bool) -> String {
    format!(
        concat!(
            "{}{} was skipped: its name is not one get_document or a kb:// URI can take ",
            "(on Windows: a reserved device name such as CON, a name ending in a dot or ",
            "a space, or one of < > \" | ? * or a control character), so indexing it ",
            "would leave a search hit nobody can open (ADR-0023). Rename it if it belongs ",
            "in the index."
        ),
        path.display(),
        if is_dir {
            " and everything under it"
        } else {
            ""
        }
    )
}

/// Percent-encode everything outside the unreserved set, leaving `/` as the
/// separator so a URI stays readable.
fn encode_path(p: &str) -> String {
    let mut out = String::with_capacity(p.len());
    for b in p.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' => {
                out.push(*b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Reverse [`encode_path`]. `None` when the escaping is malformed or the bytes
/// are not UTF-8 — a caller must not silently receive a mangled path.
fn decode_path(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = s.get(i + 1..i + 3)?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_path_round_trips_through_a_doc_uri() {
        for rel in [
            "notes/a.md",
            "deep-dive/mcp/overview.md",
            "readme.md",
            // The cases the corpus this was written against does not contain,
            // which is exactly why they are here: an encoder that never fires
            // is an encoder nobody has checked.
            "日本語/ノート.md",
            "with space/and #hash.md",
            "emoji-🦀/x.md",
        ]
        .into_iter()
        // `:` is a filename byte on Unix and a drive designator on Windows, so
        // these are only round-trippable where such a file can exist. See
        // `a_drive_designator_is_refused_only_where_drives_exist`. `?` is the
        // same kind of character (ADR-0023): Windows refuses it in a name.
        .chain(
            cfg!(not(windows))
                .then_some(["a:b.md", "C:/note.md", "a%b/c?d.md"])
                .into_iter()
                .flatten(),
        ) {
            let uri = doc_uri(rel);
            assert_eq!(
                parse(&uri),
                Some(ResourceUri::Doc(rel.to_string())),
                "round trip failed for {rel} (uri {uri})"
            );
        }
    }

    #[test]
    fn a_doc_uri_is_ascii_and_keeps_slashes_readable() {
        let rel = "日本語/深堀り/ノート.md";
        let uri = doc_uri(rel);
        assert!(uri.is_ascii(), "a URI must not carry raw non-ASCII: {uri}");

        let encoded = uri
            .strip_prefix("kb://doc/")
            .unwrap_or_else(|| panic!("wrong prefix: {uri}"));
        assert_eq!(
            encoded.matches('/').count(),
            rel.matches('/').count(),
            "every separator must survive unescaped, and no new one appear: {uri}"
        );
        assert!(
            encoded.contains('%'),
            "the non-ASCII segments must actually be encoded: {uri}"
        );
    }

    #[test]
    fn topic_uris_round_trip_including_the_root_group() {
        assert_eq!(
            parse(&topic_uri("deep-dive/mcp")),
            Some(ResourceUri::Topic("deep-dive/mcp".to_string()))
        );
        assert_eq!(
            parse(&topic_uri("")),
            Some(ResourceUri::Topic(String::new())),
            "the root group must be addressable"
        );
    }

    /// The check runs after decoding, because `%2e%2e%2f` is `../` and a check
    /// that ran first would wave it through.
    #[test]
    fn traversal_is_refused_however_it_is_spelled() {
        for hostile in [
            "kb://doc/../secret.md",
            "kb://doc/notes/../../secret.md",
            "kb://doc/%2e%2e%2fsecret.md",
            "kb://doc/%2E%2E/secret.md",
            "kb://topic/../..",
            "kb://doc//etc/passwd",
            "kb://doc/notes%5C..%5Csecret.md",
            "kb://doc/nul%00.md",
        ] {
            assert_eq!(parse(hostile), None, "must be refused: {hostile}");
        }
    }

    #[test]
    fn foreign_schemes_and_empty_documents_are_not_ours() {
        for other in [
            "file:///etc/passwd",
            "https://example.com/x.md",
            "kb://other/x",
            "kb://doc/",
            "notes/a.md",
            "",
        ] {
            assert_eq!(parse(other), None, "must not parse: {other}");
        }
    }

    /// A drive designator is Windows path syntax, and the refusal of it belongs
    /// to that platform alone. `a:b.md` and `C:/note.md` are ordinary relative
    /// names on Unix — the second is a directory called `C:` — and this module
    /// hands out URIs for both, so refusing them everywhere left the server
    /// unable to read a URI it had just built. Note which half of this test the
    /// development platform exercises: on Windows the old behaviour and the new
    /// one agree, so only the Unix runners can see the difference.
    ///
    /// `C:/Windows/System32/x.md` moved here out of
    /// `traversal_is_refused_however_it_is_spelled`, deliberately: on Unix it is
    /// not traversal, and asserting a refusal there was asserting something
    /// false about the platform. What keeps it harmless is not this check —
    /// a read resolves against the index, and a path that is not in the index is
    /// refused whatever it is spelled like.
    #[test]
    fn a_drive_designator_is_refused_only_where_drives_exist() {
        let cases = [("a:b.md", "%3A"), ("C:/note.md", "C%3A/")];
        for (rel, must_encode) in cases {
            let uri = doc_uri(rel);
            assert!(
                uri.contains(must_encode),
                "the colon must be encoded: {uri}"
            );

            if cfg!(windows) {
                assert_eq!(
                    parse(&uri),
                    None,
                    "`{rel}` names a drive on Windows, and no file there can be called that"
                );
            } else {
                assert_eq!(
                    parse(&uri),
                    Some(ResourceUri::Doc(rel.to_string())),
                    "a legal filename must survive the URI this module just built for it"
                );
            }
        }

        // Raw, unencoded — the spelling a hand-written client would use.
        assert_eq!(
            parse("kb://doc/C:/Windows/System32/x.md").is_none(),
            cfg!(windows),
            "the raw spelling follows the same platform rule as the encoded one"
        );
    }

    /// `\` is refused where it separates components and nowhere else. On Unix
    /// the index holds a file named `secret\pay.md` under that name, a search
    /// hit carries a URI for it, and a URI this module built has to read back.
    #[cfg(unix)]
    #[test]
    fn a_literal_backslash_name_survives_its_own_uri_on_unix() {
        assert_eq!(
            parse("kb://doc/secret%5Cpay.md"),
            Some(ResourceUri::Doc("secret\\pay.md".to_string()))
        );
        assert_eq!(
            parse(&doc_uri("secret\\pay.md")),
            Some(ResourceUri::Doc("secret\\pay.md".to_string()))
        );
        assert!(doc_is_addressable("secret\\pay.md"));
    }

    /// The `..` check splits on `\` as well, on every platform. That gives up
    /// a Unix file literally named `a\..\b.md`; in return nothing downstream
    /// that reads `\` as a separator can be handed a `..`.
    #[cfg(unix)]
    #[test]
    fn a_dot_dot_segment_between_backslashes_is_still_refused_on_unix() {
        assert_eq!(parse("kb://doc/a%5C..%5Cb.md"), None);
        assert!(!doc_is_addressable("a\\..\\b.md"));
    }

    #[cfg(windows)]
    #[test]
    fn a_backslash_is_refused_where_it_separates_components() {
        assert_eq!(parse("kb://doc/secret%5Cpay.md"), None);
        assert!(!doc_is_addressable("secret\\pay.md"));
    }

    /// A Windows device name in any segment is refused there, with an
    /// extension, in any case, with trailing dots or spaces, and with the
    /// trailing colon Windows also reads as the device. Names that merely
    /// start like one are not device names and stay.
    #[cfg(windows)]
    #[test]
    fn a_device_name_in_any_segment_is_refused_on_windows() {
        for p in [
            "NUL",
            "NUL.md",
            "nul.tar.gz",
            "con",
            "COM1",
            "com9.md",
            "LPT\u{b9}.md",
            "COM\u{b3}",
            "a/AUX/b.md",
            "PRN/x.md",
            "NUL. ",
            "NUL .md",
            "NUL:",
            "CONIN$",
            "conout$.md",
        ] {
            assert!(!is_safe_relative(p), "{p:?} names a device on Windows");
            assert_eq!(parse(&doc_uri(p)), None, "{p:?}");
        }
        for p in [
            "NULL.md",
            "console.md",
            "COM10.md",
            "LPT.md",
            "nul-x.md",
            "auxiliary/a.md",
            "a/connect/b.md",
        ] {
            assert!(is_safe_relative(p), "{p:?} is an ordinary name");
        }
    }

    /// A colon anywhere is refused on Windows, where no file or directory name
    /// can hold one: in a request it can only open a drive or an alternate
    /// data stream of a file (Codex round 2 on #319). The URI side refuses it
    /// too, raw and percent-encoded.
    #[cfg(windows)]
    #[test]
    fn a_colon_anywhere_is_refused_on_windows() {
        for p in ["note.md:secret", "note.md::$DATA", "a/b.md:x.md"] {
            assert!(!is_safe_relative(p), "{p:?} names a stream on Windows");
            assert_eq!(parse(&doc_uri(p)), None, "{p:?}");
            assert_eq!(parse(&format!("kb://doc/{p}")), None, "{p:?} unencoded");
        }
    }

    /// On Unix a colon is an ordinary filename character, so the same inputs
    /// are names a file can have.
    #[cfg(unix)]
    #[test]
    fn a_colon_is_an_ordinary_character_on_unix() {
        for p in ["note.md:secret", "note.md::$DATA", "a/b.md:x.md"] {
            assert!(is_safe_relative(p), "{p:?} is an ordinary name on Unix");
            assert_eq!(parse(&doc_uri(p)), Some(ResourceUri::Doc(p.to_string())));
        }
    }

    /// The same inputs are ordinary names on Unix, where no name is a device
    /// by spelling: refusing them there would lose files for nothing.
    #[cfg(unix)]
    #[test]
    fn device_names_are_ordinary_names_on_unix() {
        for p in [
            "NUL",
            "NUL.md",
            "con",
            "COM1",
            "LPT\u{b9}.md",
            "a/AUX/b.md",
            "NUL. ",
            "NUL:",
            "CONIN$",
        ] {
            assert!(is_safe_relative(p), "{p:?} is an ordinary name on Unix");
        }
    }

    /// What decides whether a URI is handed out is what decides whether it
    /// reads back, so the two cannot disagree about a path.
    #[test]
    fn a_path_is_addressable_exactly_when_its_own_uri_parses() {
        for rel in [
            "notes/a.md",
            "日本語/メモ.md",
            "a b/c#d.md",
            "",
            "../a.md",
            "a/../b.md",
            "/abs.md",
            "a\\b.md",
            "a\\..\\b.md",
            "C:/note.md",
        ] {
            assert_eq!(
                doc_is_addressable(rel),
                parse(&doc_uri(rel)) == Some(ResourceUri::Doc(rel.to_string())),
                "{rel:?}"
            );
        }
    }

    /// (ADR-0023, AW-42) A `.` segment or an empty one is another spelling of
    /// some path, never the name of a document, on every platform: the index
    /// walk cannot produce one. The rule is on document names only -- the
    /// root topic group is the empty string and a topic URI may end in `/`,
    /// and neither changes.
    #[test]
    fn a_dot_or_empty_segment_is_not_a_document_name() {
        for p in ["./a.md", "a/./b.md", "a//b.md", "a/", "a/b/", ".", "a/."] {
            assert!(!doc_is_addressable(p), "{p:?}");
            assert_eq!(parse(&doc_uri(p)), None, "{p:?}");
            assert_eq!(parse(&format!("kb://doc/{p}")), None, "{p:?} unencoded");
        }
        assert_eq!(
            parse(&topic_uri("")),
            Some(ResourceUri::Topic(String::new())),
            "the root group is still addressable"
        );
        assert_eq!(
            parse("kb://topic/notes/"),
            Some(ResourceUri::Topic("notes".to_string())),
            "a topic URI with a trailing slash still reads as its group"
        );
    }

    /// On Unix `\` is a filename character, so a name that starts with two of
    /// them has no empty segment: the new rule splits on `/` alone.
    #[cfg(unix)]
    #[test]
    fn a_leading_double_backslash_is_still_a_name_on_unix() {
        assert!(doc_is_addressable("\\\\host\\x.md"));
        assert_eq!(
            parse(&doc_uri("\\\\host\\x.md")),
            Some(ResourceUri::Doc("\\\\host\\x.md".to_string()))
        );
    }

    /// (ADR-0023, AW-42, AW-40) On Windows a segment ending in a dot or a
    /// space names a different file once Win32 trims it, and `< > " | ? *` or
    /// a control character is a name Win32 refuses outright. None of them is
    /// a name the index can hold there. Names that only resemble them stay.
    #[cfg(windows)]
    #[test]
    fn a_name_win32_would_rewrite_or_refuse_is_refused_on_windows() {
        for p in [
            "b.md.",
            "b.md ",
            "b.md. .",
            "dir./x.md",
            "dir /x.md",
            "a/b.md...",
            "a<b.md",
            "a>b.md",
            "a\"b.md",
            "a|b.md",
            "a/b?.md",
            "a*b.md",
            "a\u{1}b.md",
            "a\u{1f}b.md",
            "dir\tx/b.md",
        ] {
            assert!(!is_safe_relative(p), "{p:?} is not a name on Windows");
            assert!(!doc_is_addressable(p), "{p:?}");
            assert_eq!(parse(&doc_uri(p)), None, "{p:?}");
        }
        for p in [
            "a.b.md",
            ".hidden.md",
            "a/.x/b.md",
            "a b.md",
            "dir.x/a.md",
            "x.md~",
            "a%b.md",
            "a#b.md",
            "日本語/メモ.md",
        ] {
            assert!(is_safe_relative(p), "{p:?} is an ordinary name");
            assert!(doc_is_addressable(p), "{p:?}");
            assert_eq!(parse(&doc_uri(p)), Some(ResourceUri::Doc(p.to_string())));
        }
    }

    /// The same inputs are ordinary names on Unix.
    #[cfg(unix)]
    #[test]
    fn trailing_dots_spaces_and_win32_reserved_characters_are_ordinary_names_on_unix() {
        for p in [
            "b.md.",
            "b.md ",
            "dir./x.md",
            "dir /x.md",
            "a<b.md",
            "a>b.md",
            "a\"b.md",
            "a|b.md",
            "a/b?.md",
            "a*b.md",
            "a\u{1}b.md",
            "dir\tx/b.md",
        ] {
            assert!(is_safe_relative(p), "{p:?} is an ordinary name on Unix");
            assert!(doc_is_addressable(p), "{p:?}");
            assert_eq!(parse(&doc_uri(p)), Some(ResourceUri::Doc(p.to_string())));
        }
    }

    /// The message every skip for a name logs: ASCII, names the path, and
    /// says whether a whole directory went with it.
    #[test]
    fn the_skip_reason_is_ascii_and_names_what_was_skipped() {
        let file = unspellable_reason(std::path::Path::new("kb/CON.md"), false);
        assert!(file.is_ascii(), "{file}");
        assert!(file.contains("CON.md"), "{file}");
        assert!(file.contains("ADR-0023"), "{file}");
        assert!(!file.contains("everything under it"), "{file}");
        let dir = unspellable_reason(std::path::Path::new("kb/dir."), true);
        assert!(dir.is_ascii(), "{dir}");
        assert!(
            dir.contains("dir. and everything under it was skipped"),
            "{dir}"
        );
    }

    #[test]
    fn malformed_escaping_is_refused_rather_than_guessed() {
        for bad in ["kb://doc/a%.md", "kb://doc/a%zz.md", "kb://doc/a%2"] {
            assert_eq!(parse(bad), None, "must be refused: {bad}");
        }
    }

    /// The grouping has to match what the indexer derives, or a listing would
    /// describe a corpus the database does not have.
    #[test]
    fn grouping_follows_the_same_first_two_segments_the_indexer_uses() {
        assert_eq!(group_prefix("readme.md"), "");
        assert_eq!(group_prefix("ai-news/2026-04-16.md"), "ai-news");
        assert_eq!(group_prefix("deep-dive/mcp/overview.md"), "deep-dive/mcp");
        assert_eq!(group_prefix("deep-dive/mcp/sub/deeper.md"), "deep-dive/mcp");
    }

    #[test]
    fn groups_are_sorted_and_carry_their_documents() {
        let paths: Vec<String> = [
            "deep-dive/mcp/overview.md",
            "ai-news/b.md",
            "readme.md",
            "deep-dive/mcp/detail.md",
            "ai-news/a.md",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();

        let groups = topic_groups(&paths);
        let names: Vec<&str> = groups.iter().map(|g| g.prefix.as_str()).collect();
        assert_eq!(names, vec!["", "ai-news", "deep-dive/mcp"]);

        assert_eq!(groups[0].display_name(), "(root)");
        assert_eq!(groups[1].paths, vec!["ai-news/a.md", "ai-news/b.md"]);
        assert!(groups[1].description().contains('2'));
        assert!(
            groups[2].description().contains("documents"),
            "plural: {}",
            groups[2].description()
        );
        assert_eq!(groups[0].paths.len(), 1);
        assert!(
            groups[0].description().starts_with("1 indexed document "),
            "singular: {}",
            groups[0].description()
        );
    }

    /// Everything a listing offers must survive being read back — the property
    /// that makes `resources/list` a promise rather than a guess.
    #[test]
    fn every_uri_a_listing_emits_parses_back_to_what_produced_it() {
        let mut paths: Vec<String> = ["a.md", "t/b.md", "x/y/c.md", "日本語/d.md"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        // `cfg!` rather than `#[cfg]`, so the vector is mutated on every
        // platform and this stays one test rather than two. `C:/e.md` puts a
        // colon in a *group prefix* as well as in a document path.
        if cfg!(not(windows)) {
            paths.push("a:b.md".to_string());
            paths.push("C:/e.md".to_string());
        }

        for g in topic_groups(&paths) {
            assert_eq!(parse(&g.uri()), Some(ResourceUri::Topic(g.prefix.clone())));
        }
        for p in &paths {
            assert_eq!(parse(&doc_uri(p)), Some(ResourceUri::Doc(p.clone())));
        }
    }
}
