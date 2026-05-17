//! Markdown (`.md`) parser. Moved from the old `src/markdown.rs` and adapted
//! to the `Parser` trait. Behaviour is identical to legacy.

use serde::Deserialize;

use super::{Chunk, Frontmatter, ParsedDocument, Parser};

/// Markdown parser. Handles YAML frontmatter + heading-based chunking using
/// [`pulldown-cmark`](https://crates.io/crates/pulldown-cmark) rules informally
/// (we only split on `## ` / `### ` prefixes, we do not traverse the AST).
pub struct MarkdownParser;

impl Parser for MarkdownParser {
    fn extension(&self) -> &'static str {
        "md"
    }

    fn parse(&self, raw: &str, _path_hint: &str, exclude_headings: &[&str]) -> ParsedDocument {
        let (frontmatter, body) = extract_frontmatter(raw);
        let chunks = chunk_body(&body, exclude_headings);
        ParsedDocument {
            frontmatter,
            chunks,
            raw_content: raw.to_string(),
        }
    }
}

// ---------------------------------------------------------------------------
// Internal: serde helper for flexible YAML deserialization
// ---------------------------------------------------------------------------

/// Intermediate representation for serde_yaml_bw deserialization.
/// `date` is captured as `serde_yaml_bw::Value` so it works regardless of whether
/// the YAML encodes it as a string (`"2026-04-10"`) or a native date value.
#[derive(Deserialize)]
struct RawFrontmatter {
    title: Option<String>,
    date: Option<serde_yaml_bw::Value>,
    topic: Option<String>,
    depth: Option<serde_yaml_bw::Value>,
    #[serde(default)]
    tags: Vec<String>,
}

impl From<RawFrontmatter> for Frontmatter {
    fn from(raw: RawFrontmatter) -> Self {
        // serde_yaml_bw::Value は (value, tag) の 2-field 。tag はここでは使わない
        // ので `_` で無視する。
        let date = raw.date.map(|v| match v {
            serde_yaml_bw::Value::String(s, _) => s,
            other => {
                let s = format!("{other:?}");
                s.trim_matches('"').to_string()
            }
        });

        let depth = raw.depth.map(|v| match v {
            serde_yaml_bw::Value::String(s, _) => s,
            serde_yaml_bw::Value::Number(n, _) => n.to_string(),
            other => format!("{other:?}"),
        });

        Frontmatter {
            title: raw.title,
            date,
            topic: raw.topic,
            depth,
            tags: raw.tags,
        }
    }
}

// ---------------------------------------------------------------------------
// Frontmatter extraction
// ---------------------------------------------------------------------------

fn extract_frontmatter(raw: &str) -> (Frontmatter, String) {
    let trimmed = raw.trim_start_matches('\u{feff}'); // strip BOM if present

    if !trimmed.starts_with("---") {
        return (Frontmatter::default(), trimmed.to_string());
    }

    let after_first = &trimmed[3..];
    let after_first = after_first.trim_start_matches('\r'); // handle \r\n
    let after_first = after_first.strip_prefix('\n').unwrap_or(after_first);

    if let Some(end) = after_first.find("\n---") {
        let yaml_raw = &after_first[..end];
        let body_start = end + 4; // skip the `\n---`
        let rest = &after_first[body_start..];
        let body = rest.trim_start_matches(['\r', '\n']).to_string();

        // Windows 生成の `.md` で `\r\n` 改行のとき、yaml_raw 各行末に `\r` が
        // 残って serde_yaml_bw のパース結果の文字列 value にリークするので
        // パース前に CRLF → LF へ正規化する。
        let yaml_normalized;
        let yaml_str: &str = if yaml_raw.contains('\r') {
            yaml_normalized = yaml_raw.replace("\r\n", "\n").replace('\r', "\n");
            &yaml_normalized
        } else {
            yaml_raw
        };

        let fm = match serde_yaml_bw::from_str::<RawFrontmatter>(yaml_str) {
            Ok(raw_fm) => Frontmatter::from(raw_fm),
            Err(e) => {
                eprintln!("warning: failed to parse YAML frontmatter: {e}");
                Frontmatter::default()
            }
        };

        (fm, body)
    } else {
        (Frontmatter::default(), trimmed.to_string())
    }
}

// ---------------------------------------------------------------------------
// Heading-based chunking
// ---------------------------------------------------------------------------

fn chunk_body(body: &str, excludes: &[&str]) -> Vec<Chunk> {
    let mut raw_chunks: Vec<(Option<(u8, String)>, String)> = Vec::new();
    let mut current_heading: Option<(u8, String)> = None;
    let mut current_lines: Vec<&str> = Vec::new();
    let mut excluded = false;

    for line in body.lines() {
        if let Some((level, heading_text)) = strip_heading(line) {
            if excludes.iter().any(|ex| heading_text.contains(ex)) {
                if !excluded {
                    let content = current_lines.join("\n").trim().to_string();
                    if !content.is_empty() || current_heading.is_some() {
                        raw_chunks.push((current_heading.clone(), content));
                    }
                }
                excluded = true;
                current_lines.clear();
                current_heading = None;
                continue;
            }

            if !excluded {
                let content = current_lines.join("\n").trim().to_string();
                if !content.is_empty() || current_heading.is_some() {
                    raw_chunks.push((current_heading.clone(), content));
                }
            }

            excluded = false;
            current_heading = Some((level, heading_text));
            current_lines.clear();
        } else if !excluded {
            current_lines.push(line);
        }
    }

    if !excluded {
        let content = current_lines.join("\n").trim().to_string();
        if !content.is_empty() || current_heading.is_some() {
            raw_chunks.push((current_heading, content));
        }
    }

    // 50-char 未満 chunk は直前 chunk に merge する。
    // merge 後 chunk の heading / level は **最初に出現した heading のもの**
    // を維持し、merge された後続 heading は content 側に `## h\n\n` で残す
    // (legacy 挙動の継続)。
    let mut merged: Vec<(Option<(u8, String)>, String)> = Vec::new();
    for (heading, content) in raw_chunks {
        if content.len() < 50 && !merged.is_empty() {
            let prev = merged.last_mut().unwrap();
            if !prev.1.is_empty() {
                prev.1.push_str("\n\n");
            }
            if let Some((_lvl, ref h)) = heading {
                prev.1.push_str(&format!("## {h}\n\n"));
            }
            prev.1.push_str(&content);
        } else {
            merged.push((heading, content));
        }
    }

    merged
        .into_iter()
        .enumerate()
        .map(|(i, (heading_pair, content))| {
            let (level, heading) = match heading_pair {
                Some((lvl, text)) => (Some(lvl), Some(text)),
                None => (None, None),
            };
            Chunk {
                index: i,
                heading,
                level,
                content,
            }
        })
        .collect()
}

fn strip_heading(line: &str) -> Option<(u8, String)> {
    let trimmed = line.trim();
    if let Some(rest) = trimmed.strip_prefix("### ") {
        Some((3, rest.trim().to_string()))
    } else {
        trimmed
            .strip_prefix("## ")
            .map(|rest| (2, rest.trim().to_string()))
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(md: &str) -> ParsedDocument {
        MarkdownParser.parse(md, "test.md", &[])
    }

    #[test]
    fn test_strip_heading_returns_h2_level() {
        let result = strip_heading("## Foo");
        assert_eq!(result, Some((2, "Foo".to_string())));
    }

    #[test]
    fn test_strip_heading_returns_h3_level() {
        let result = strip_heading("### Bar");
        assert_eq!(result, Some((3, "Bar".to_string())));
    }

    #[test]
    fn test_strip_heading_no_heading_returns_none() {
        assert_eq!(strip_heading("plain text"), None);
        assert_eq!(strip_heading("# H1 ignored"), None);
        assert_eq!(strip_heading("#### too deep"), None);
    }

    #[test]
    fn test_chunk_with_h2_heading_has_level_2() {
        let md = "## Section\n\nbody enough body enough body enough body enough body";
        let doc = parse(md);
        assert_eq!(doc.chunks.len(), 1);
        assert_eq!(doc.chunks[0].heading.as_deref(), Some("Section"));
        assert_eq!(doc.chunks[0].level, Some(2));
    }

    #[test]
    fn test_chunk_with_h3_heading_has_level_3() {
        let md = "### Sub\n\nbody enough body enough body enough body enough body";
        let doc = parse(md);
        assert_eq!(doc.chunks.len(), 1);
        assert_eq!(doc.chunks[0].level, Some(3));
    }

    #[test]
    fn test_chunk_no_heading_has_level_none() {
        let md = "leading prose without heading enough body to avoid 50-char merge";
        let doc = parse(md);
        assert_eq!(doc.chunks.len(), 1);
        assert!(doc.chunks[0].heading.is_none());
        assert!(doc.chunks[0].level.is_none());
    }

    #[test]
    fn test_50char_merge_preserves_first_heading_level() {
        let md = "\
## Big

short.

### Sub

larger body enough enough enough enough enough enough enough.";
        let doc = parse(md);
        let merged = doc.chunks.first().expect("expected at least one chunk");
        assert_eq!(merged.heading.as_deref(), Some("Big"));
        assert_eq!(merged.level, Some(2));
    }
}
