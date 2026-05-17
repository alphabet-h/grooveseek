//! Plain-text (`.txt`) parser. MVP implementation:
//! - No frontmatter concept — title is derived from the filename.
//! - Single chunk containing the entire (CRLF-normalized, BOM-stripped) body.
//!   Smart paragraph-based chunking is EXT-4 (Sprint 2).
//!
//! `exclude_headings` is accepted for trait conformance but ignored (`.txt`
//! has no heading concept).

use super::{Chunk, Frontmatter, ParsedDocument, Parser};

pub struct TxtParser;

impl Parser for TxtParser {
    fn extension(&self) -> &'static str {
        "txt"
    }

    fn parse(&self, raw: &str, path_hint: &str, _exclude_headings: &[&str]) -> ParsedDocument {
        let body = normalize_text(raw);
        let title = derive_title(path_hint);

        let frontmatter = Frontmatter {
            title,
            ..Frontmatter::default()
        };

        // MVP: single chunk. EXT-4 will break on paragraph boundaries.
        // Empty body → no chunks (indexer skips files with no chunks).
        let chunks = if body.trim().is_empty() {
            Vec::new()
        } else {
            vec![Chunk {
                index: 0,
                heading: None,
                level: None,
                content: body,
            }]
        };

        ParsedDocument {
            frontmatter,
            chunks,
            raw_content: raw.to_string(),
        }
    }
}

/// Strip BOM and normalize CRLF/CR → LF. Keeps trailing whitespace trimming
/// minimal so we don't lose meaningful blank lines inside the text.
fn normalize_text(raw: &str) -> String {
    let no_bom = raw.trim_start_matches('\u{feff}');
    no_bom.replace("\r\n", "\n").replace('\r', "\n")
}

/// Derive a title from the kb_path-relative path. Examples:
/// - `"notes/deep-dive-2026.txt"` → `"deep dive 2026"`
/// - `"log_file.txt"` → `"log file"`
/// - `"日本語ファイル.txt"` → `"日本語ファイル"`
///
/// Rules:
/// - take the file stem (strip extension, strip leading directories)
/// - replace `-` / `_` with space
/// - do **not** touch the case (keep the source's case)
/// - do **not** touch non-ASCII characters
fn derive_title(path_hint: &str) -> Option<String> {
    // Take the last path segment (handles both `/` and `\`, though our
    // indexer normalizes to forward-slash before calling).
    let last = path_hint.rsplit(['/', '\\']).next().unwrap_or(path_hint);

    // Strip extension.
    let stem = match last.rfind('.') {
        Some(dot) if dot > 0 => &last[..dot],
        _ => last,
    };

    if stem.is_empty() {
        return None;
    }

    let title: String = stem
        .chars()
        .map(|c| match c {
            '-' | '_' => ' ',
            other => other,
        })
        .collect();

    // Collapse runs of whitespace. (e.g. "foo--bar" → "foo  bar" → "foo bar")
    let collapsed: String = title.split_whitespace().collect::<Vec<_>>().join(" ");

    if collapsed.is_empty() {
        None
    } else {
        Some(collapsed)
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_derive_title_simple() {
        assert_eq!(
            derive_title("notes/deep-dive-2026.txt").as_deref(),
            Some("deep dive 2026")
        );
    }

    #[test]
    fn test_derive_title_underscore() {
        assert_eq!(derive_title("log_file.txt").as_deref(), Some("log file"));
    }

    #[test]
    fn test_derive_title_no_dir() {
        assert_eq!(derive_title("plain.txt").as_deref(), Some("plain"));
    }

    #[test]
    fn test_derive_title_no_ext() {
        assert_eq!(derive_title("README").as_deref(), Some("README"));
    }

    #[test]
    fn test_derive_title_cjk_preserved() {
        assert_eq!(
            derive_title("notes/日本語ファイル.txt").as_deref(),
            Some("日本語ファイル")
        );
    }

    #[test]
    fn test_derive_title_mixed_separators() {
        // `-` and `_` both become space; runs collapse to single space.
        assert_eq!(derive_title("a_b--c_d.txt").as_deref(), Some("a b c d"));
    }

    #[test]
    fn test_parse_simple() {
        let p = TxtParser;
        let doc = p.parse("Hello world.\nSecond line.\n", "notes/hello.txt", &[]);
        assert_eq!(doc.frontmatter.title.as_deref(), Some("hello"));
        assert_eq!(doc.chunks.len(), 1);
        assert_eq!(doc.chunks[0].heading, None);
        assert!(doc.chunks[0].content.contains("Hello world."));
        assert!(doc.chunks[0].content.contains("Second line."));
    }

    #[test]
    fn test_parse_crlf_normalized() {
        let p = TxtParser;
        let doc = p.parse("line1\r\nline2\r\n", "x.txt", &[]);
        assert_eq!(doc.chunks.len(), 1);
        assert!(!doc.chunks[0].content.contains('\r'));
    }

    #[test]
    fn test_parse_bom_stripped() {
        let p = TxtParser;
        let doc = p.parse("\u{feff}body", "x.txt", &[]);
        assert_eq!(doc.chunks.len(), 1);
        assert_eq!(doc.chunks[0].content, "body");
    }

    #[test]
    fn test_parse_empty_body_produces_no_chunks() {
        let p = TxtParser;
        let doc = p.parse("   \n\n   ", "x.txt", &[]);
        assert!(doc.chunks.is_empty());
    }

    #[test]
    fn test_parse_ignores_exclude_headings() {
        // .txt has no heading concept, so exclude_headings must be ignored.
        let p = TxtParser;
        let doc = p.parse("## 次の深堀り候補\n\nbody.", "x.txt", &["次の深堀り候補"]);
        assert_eq!(doc.chunks.len(), 1);
        // The literal `##` should stay in the content because .txt doesn't
        // interpret markdown headings.
        assert!(doc.chunks[0].content.contains("## 次の深堀り候補"));
    }

    #[test]
    fn test_extension_id_are_txt() {
        let p = TxtParser;
        assert_eq!(p.extension(), "txt");
        assert_eq!(p.id(), "txt");
    }
}
