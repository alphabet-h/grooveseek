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
    if !is_safe_relative(&decoded) {
        return None;
    }
    match kind {
        "doc" if decoded.is_empty() => None,
        "doc" => Some(ResourceUri::Doc(decoded)),
        _ => Some(ResourceUri::Topic(
            decoded.trim_end_matches('/').to_string(),
        )),
    }
}

/// A decoded path may be used against the knowledge base only if it stays
/// inside it and names something on this side of the OS's path syntax.
fn is_safe_relative(p: &str) -> bool {
    if p.contains('\0') || p.contains('\\') {
        return false;
    }
    if p.starts_with('/') || starts_with_drive_designator(p) {
        return false;
    }
    !p.split('/').any(|seg| seg == "..")
}

/// Whether `p` opens with a Windows drive designator — `C:` — which escapes the
/// knowledge base. Every form of it does, the drive-*relative* `C:notes.md`
/// included, since that resolves against that drive's own current directory.
///
/// **Windows only.** A drive designator is Windows path syntax, so applying the
/// rule anywhere else refuses legal names for a danger that does not exist
/// there: `a:b.md` and `C:/note.md` (a directory literally named `C:`) are both
/// ordinary relative paths on Unix, and this module hands out URIs for them.
/// Nothing is lost by the narrowing — the colon reaches here percent-decoded,
/// the leading-slash check above already refuses Unix absolute paths, and the
/// caller resolves what survives against the index rather than the filesystem.
fn starts_with_drive_designator(p: &str) -> bool {
    if !cfg!(windows) {
        return false;
    }
    let b = p.as_bytes();
    b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':'
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
            "a%b/c?d.md",
            "emoji-🦀/x.md",
        ]
        .into_iter()
        // `:` is a filename byte on Unix and a drive designator on Windows, so
        // these are only round-trippable where such a file can exist. See
        // `a_drive_designator_is_refused_only_where_drives_exist`.
        .chain(
            cfg!(not(windows))
                .then_some(["a:b.md", "C:/note.md"])
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
