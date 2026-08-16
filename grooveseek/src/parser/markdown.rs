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

    fn parse(&self, raw: &str, path_hint: &str, exclude_headings: &[&str]) -> ParsedDocument {
        let (frontmatter, body) = extract_frontmatter(raw);
        // context 用 title: frontmatter.title (非空) → filename stem fallback (E-1)。
        // documents.title は frontmatter.title のまま (ここでは context 生成にのみ使う)。
        let ctx_title = frontmatter
            .title
            .as_deref()
            .filter(|t| !t.trim().is_empty())
            .map(str::to_string)
            .or_else(|| super::txt::derive_title_pub(path_hint));
        let chunks = chunk_body(&body, exclude_headings, ctx_title.as_deref());
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

/// (heading (level, text), 祖先見出しスナップショット, content)。
/// clippy::type_complexity 回避のための alias。
type RawChunk = (Option<(u8, String)>, Vec<String>, String);

fn chunk_body(body: &str, excludes: &[&str], title: Option<&str>) -> Vec<Chunk> {
    // raw_chunks: (heading, 祖先見出しスナップショット, content)
    let mut raw_chunks: Vec<RawChunk> = Vec::new();
    let mut current_heading: Option<(u8, String)> = None;
    let mut current_ancestry: Vec<String> = Vec::new();
    let mut current_lines: Vec<&str> = Vec::new();
    let mut excluded = false;
    // ancestry stack: 現在位置より浅い見出しの列 (level ASC)。exclude 見出しも積む (E-6)。
    let mut stack: Vec<(u8, String)> = Vec::new();

    for line in body.lines() {
        if let Some((level, heading_text)) = strip_heading(line) {
            // 1) 直前 chunk を flush (heading/ancestry は「今開いている chunk」のもの)
            if !excluded {
                let content = current_lines.join("\n").trim().to_string();
                if !content.is_empty() || current_heading.is_some() {
                    raw_chunks.push((current_heading.clone(), current_ancestry.clone(), content));
                }
            }
            current_lines.clear();

            // 2) この level に対して stack を pop → 残りが自見出しの祖先
            while let Some((l, _)) = stack.last() {
                if *l >= level {
                    stack.pop();
                } else {
                    break;
                }
            }
            let ancestry: Vec<String> = stack.iter().map(|(_, h)| h.clone()).collect();
            // 3) この見出しを stack へ (exclude されても積む = E-6)
            stack.push((level, heading_text.clone()));

            // 4) 次 chunk の state を確定
            if excludes.iter().any(|ex| heading_text.contains(ex)) {
                excluded = true;
                current_heading = None;
                current_ancestry = Vec::new();
            } else {
                excluded = false;
                current_heading = Some((level, heading_text));
                current_ancestry = ancestry;
            }
        } else if !excluded {
            current_lines.push(line);
        }
    }
    if !excluded {
        let content = current_lines.join("\n").trim().to_string();
        if !content.is_empty() || current_heading.is_some() {
            raw_chunks.push((current_heading, current_ancestry, content));
        }
    }

    // 50-char 未満 chunk は直前 chunk に merge。heading / level / **ancestry** は
    // 最初に出現した heading のものを維持 (legacy 挙動 + E-9 の ancestry 引継ぎ)。
    let mut merged: Vec<RawChunk> = Vec::new();
    for (heading, ancestry, content) in raw_chunks {
        if content.len() < 50 && !merged.is_empty() {
            let prev = merged.last_mut().unwrap();
            if !prev.2.is_empty() {
                prev.2.push_str("\n\n");
            }
            if let Some((_lvl, ref h)) = heading {
                prev.2.push_str(&format!("## {h}\n\n"));
            }
            prev.2.push_str(&content);
        } else {
            merged.push((heading, ancestry, content));
        }
    }

    merged
        .into_iter()
        .enumerate()
        .map(|(i, (heading_pair, ancestry, content))| {
            let (level, heading) = match heading_pair {
                Some((lvl, text)) => (Some(lvl), Some(text)),
                None => (None, None),
            };
            // context parts: [title, ...ancestry, heading]
            let mut parts: Vec<&str> = Vec::with_capacity(ancestry.len() + 2);
            if let Some(t) = title {
                parts.push(t);
            }
            for a in &ancestry {
                parts.push(a);
            }
            if let Some(h) = &heading {
                parts.push(h);
            }
            let context = super::build_context(&parts);
            Chunk {
                index: i,
                heading,
                level,
                content,
                context,
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

    // brief 提供時の fixture では excludes を渡す手段がなく E-6 が検証不能だったため
    // (team-lead 承認済み逸脱 1): excludes 引数を追加し全呼び出しをこちらに統一する。
    fn parse_ctx(md: &str, title: &str, excludes: &[&str]) -> ParsedDocument {
        MarkdownParser.parse(md, &format!("{title}.md"), excludes)
    }

    #[test]
    fn test_context_simple_hierarchy() {
        // title は path_hint (filename) 由来。h2 > h3 のパンくずが付く。
        // (team-lead 承認済み逸脱 2: brief 原文の body は 50 字未満で 50-char merge に
        // 飲まれ chunks[1] が存在しなくなるため、意図・期待 context 文字列は変えず
        // body の文字数のみ 50 字以上に伸ばしている)
        let md = "## 検索パイプライン\n\nintro body enough enough enough enough enough enough.\n\n### RRF の実装\n\nbody enough enough enough enough enough enough enough.";
        let doc = parse_ctx(md, "設計ノート", &[]);
        // preamble なし: chunk[0] = h2, chunk[1] = h3
        assert_eq!(doc.chunks[0].heading.as_deref(), Some("検索パイプライン"));
        assert_eq!(
            doc.chunks[0].context.as_deref(),
            Some("設計ノート > 検索パイプライン")
        );
        assert_eq!(doc.chunks[1].heading.as_deref(), Some("RRF の実装"));
        assert_eq!(
            doc.chunks[1].context.as_deref(),
            Some("設計ノート > 検索パイプライン > RRF の実装")
        );
    }

    #[test]
    fn test_context_preamble_is_title_only() {
        // E-3: heading なし preamble chunk は title のみ
        let md = "leading prose without heading enough body to avoid the 50-char merge here.";
        let doc = parse_ctx(md, "mydoc", &[]);
        assert!(doc.chunks[0].heading.is_none());
        assert_eq!(doc.chunks[0].context.as_deref(), Some("mydoc"));
    }

    #[test]
    fn test_context_level_skip_h2_to_h4() {
        // E-7: level 飛び (h2 → h3 相当の deeper) — groove は h2/h3 のみ認識。
        // h2 の直後に h3 が来た後、別 h2 が来ると h3 は pop され祖先から消える。
        // (team-lead 承認済み逸脱 2: A/B の body を 50 字以上に伸ばし、A1 に merge
        // されず A・B がそれぞれ独立 chunk として残るようにしている)
        let md = "## A\n\nbody enough enough enough enough enough enough enough.\n\n### A1\n\nbody enough enough enough enough enough.\n\n## B\n\nbody enough enough enough enough enough enough enough.";
        let doc = parse_ctx(md, "T", &[]);
        let b = doc
            .chunks
            .iter()
            .find(|c| c.heading.as_deref() == Some("B"))
            .unwrap();
        // B は h2 なので祖先は空、context = "T > B" (A/A1 は入らない)
        assert_eq!(b.context.as_deref(), Some("T > B"));
    }

    #[test]
    fn test_context_excluded_heading_still_on_stack() {
        // E-6: 除外見出し配下の非除外 subsection は、除外見出しを祖先に持つ。
        // (groove の exclude は「次見出しまで」= subtree 除外ではない)
        let md = "## Secret\n\nsecret intro enough enough enough enough.\n\n### Detail\n\ndetail body enough enough enough enough.";
        let doc = parse_ctx(md, "T", &["Secret"]);
        // Secret 自身の chunk は出ない (excluded)。Detail (h3) は出る。
        let detail = doc
            .chunks
            .iter()
            .find(|c| c.heading.as_deref() == Some("Detail"))
            .unwrap();
        // ancestry に "Secret" が乗る (= 除外されても stack には積まれる、E-6 の本質)
        assert_eq!(detail.context.as_deref(), Some("T > Secret > Detail"));
        assert!(
            !doc.chunks
                .iter()
                .any(|c| c.heading.as_deref() == Some("Secret"))
        );
    }

    #[test]
    fn test_context_50char_merge_keeps_first_ancestry() {
        // E-9: 50-char merge 後 chunk は最初に出現した heading の ancestry を維持
        // Sub の content は 50 字未満に保つ (merge 条件 `content.len() < 50` を
        // 実際に踏ませるため。50 字以上だと merge 自体が起きず本テストの意図が
        // 検証できない = レビュー指摘の Medium 修正)。
        let md = "## Big\n\nshort.\n\n### Sub\n\nlarger body enough enough enough enough.";
        let doc = parse_ctx(md, "T", &[]);
        // merge が実際に発生し、chunk が1個に collapse していることを確認
        assert_eq!(doc.chunks.len(), 1);
        let merged = doc.chunks.first().unwrap();
        assert_eq!(merged.heading.as_deref(), Some("Big"));
        // merge 後も Big の ancestry (= title のみ) を保持
        assert_eq!(merged.context.as_deref(), Some("T > Big"));
    }
}
