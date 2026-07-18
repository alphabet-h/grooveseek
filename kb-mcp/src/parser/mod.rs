//! Parser plugin layer.
//!
//! 各ファイル形式 (`.md` / `.txt` / 将来 `.rst` / `.adoc` / `.pdf` 等) に対して
//! `trait Parser` の実装を 1 つ用意し、`Registry` が拡張子でルックアップする。
//! 形式追加は新しい `Parser` impl を追加して `Registry::defaults()` か
//! `kb-mcp.toml` の `[parsers].enabled` に id を入れるだけ。
//!
//! `Frontmatter` / `Chunk` / `ParsedDocument` は元々 `src/markdown.rs` にあった
//! が、形式非依存な表現として parser モジュールへ移した。
//! `src/markdown.rs` は後方互換 shim として公開 API を保つ。

use anyhow::{Context, Result};
use serde::Deserialize;

pub mod markdown;
pub mod registry;
pub mod txt;

pub use markdown::MarkdownParser;
pub use registry::Registry;
pub use txt::TxtParser;

// ---------------------------------------------------------------------------
// Data types (formerly in src/markdown.rs)
// ---------------------------------------------------------------------------

/// Metadata extracted from a document header (YAML frontmatter for `.md`,
/// filename-derived for `.txt`, etc.).
#[derive(Debug, Clone, Default)]
pub struct Frontmatter {
    pub title: Option<String>,
    pub date: Option<String>,
    pub topic: Option<String>,
    pub depth: Option<String>,
    pub tags: Vec<String>,
}

/// A single chunk of a parsed document.
///
/// All fields use their type's natural default (`0`, `None`, `None`,
/// `String::new()`), so `#[derive(Default)]` is sufficient and clippy-compliant.
/// Other config-like structs in this crate (e.g. `MmrConfig`) use a hand-written
/// `Default` because some defaults are non-zero (e.g. `lambda = 0.7`).
#[derive(Debug, Clone, Default)]
pub struct Chunk {
    pub index: usize,
    pub heading: Option<String>,
    /// Markdown 見出しレベル (h2=2, h3=3)。heading が None の場合や、
    /// 見出し概念のない parser (.txt 等) では None。Parent retriever や
    /// 将来の Contextual Retrieval (A-1) で hierarchy を利用する。
    pub level: Option<u8>,
    pub content: String,
}

/// A fully parsed document: frontmatter + chunks + retained raw content.
#[derive(Debug, Clone)]
pub struct ParsedDocument {
    pub frontmatter: Frontmatter,
    pub chunks: Vec<Chunk>,
    pub raw_content: String,
}

/// Section headings excluded by default when the caller does not override.
/// Empty by default; callers typically configure this via `kb-mcp.toml`'s
/// `exclude_headings` key. Matching is substring-based inside the Markdown
/// chunker.
pub const DEFAULT_EXCLUDED_HEADINGS: &[&str] = &[];

/// バイナリ形式ファイルの生バイト上限 (50 MiB)。index 時の size skip (indexer)
/// と get_document の raw cap (server) で共有する。テキスト形式には適用しない。
pub const MAX_RAW_BINARY_BYTES: u64 = 50 * 1024 * 1024;

/// 抽出済みテキストを 1 チャンクに包む共通 helper。バイナリ parser の trait 契約用
/// `parse` (&str 版 = 「既に抽出済みテキストを受け取った」fallback) 実装で使う。
/// path_hint からファイル名ベースの title を derive する。
pub(crate) fn single_text_chunk(raw: &str, path_hint: &str) -> ParsedDocument {
    let body = raw.replace("\r\n", "\n").replace('\r', "\n");
    let title = txt::derive_title_pub(path_hint);
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
        frontmatter: Frontmatter {
            title,
            ..Frontmatter::default()
        },
        chunks,
        raw_content: raw.to_string(),
    }
}

// ---------------------------------------------------------------------------
// Parser trait
// ---------------------------------------------------------------------------

/// A file-format parser plugin. One instance per supported extension.
///
/// Implementors must be `Send + Sync` because the Registry is shared across
/// server threads (MCP + future watcher).
pub trait Parser: Send + Sync {
    /// Lowercase extension this parser claims, **without** a leading dot
    /// (e.g. `"md"`, `"txt"`). Used for `walkdir` filtering.
    fn extension(&self) -> &'static str;

    /// Stable id used in `[parsers].enabled` of `kb-mcp.toml`. Typically equal
    /// to `extension()` but kept separate so future parsers can share logic
    /// (e.g. an `"mdx"` id that reuses Markdown parsing).
    fn id(&self) -> &'static str {
        self.extension()
    }

    /// Parse raw file content into frontmatter + chunks.
    ///
    /// - `raw` — full file text (already read to string)
    /// - `path_hint` — `kb_path` 相対の forward-slash path。frontmatter が無い
    ///   形式 (`.txt` 等) で title をファイル名から derive する時に使う
    /// - `exclude_headings` — 見出しベースのチャンク除外リスト (substring 一致)。
    ///   見出し概念のない形式は無視してよい
    fn parse(&self, raw: &str, path_hint: &str, exclude_headings: &[&str]) -> ParsedDocument;

    /// バイト列から parse する。**全 call site (indexer / server) はこちらに統一する。**
    ///
    /// default impl = UTF-8 検証して `parse` に委譲する (md/txt は override 不要で動く)。
    /// バイナリ parser (Pdf/Docx/Xlsx/Xls/Pptx) はこれを override して形式固有の
    /// チャンクを直接生成する。`Err` の意味 = 「このファイルは index 不能」で、
    /// 呼び出し側が skip + warn を行う。
    fn parse_bytes(
        &self,
        bytes: &[u8],
        path_hint: &str,
        exclude_headings: &[&str],
    ) -> Result<ParsedDocument> {
        let s = std::str::from_utf8(bytes)
            .with_context(|| format!("{path_hint}: not valid UTF-8"))?;
        Ok(self.parse(s, path_hint, exclude_headings))
    }

    /// バイナリ形式 parser は `true` を返す (default `false`)。
    /// get_document の cap 分類 (§4.4) と quality filter 免除 (§4.8) の判定に使う。
    fn is_binary(&self) -> bool {
        false
    }
}

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

/// [parsers] セクション (`kb-mcp.toml`)。
///
/// - キー省略時 (`parsers: None`) は `Registry::defaults()` (= `["md"]` のみ、
///   legacy 完全後方互換) を適用する。ユーザが `.txt` 等を index したい
///   場合は明示的に `enabled = ["md", "txt"]` と opt-in する。
/// - `enabled = []` は誤設定として reject する (全拡張子が無効 = index 結果が
///   空になる silent failure を防ぐ)。
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ParsersConfig {
    pub enabled: Vec<String>,
}

impl ParsersConfig {
    /// `enabled` が空なら誤設定としてエラーを返す。load 時に呼ぶ。
    pub fn validate(&self) -> Result<()> {
        if self.enabled.is_empty() {
            anyhow::bail!(
                "[parsers].enabled must contain at least one id (got empty array). \
                 Remove the key entirely to use the default [\"md\"]."
            );
        }
        Ok(())
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parsers_config_rejects_empty() {
        let cfg = ParsersConfig { enabled: vec![] };
        let err = cfg.validate().expect_err("empty enabled must be an error");
        assert!(err.to_string().contains("empty array"));
    }

    #[test]
    fn test_parsers_config_accepts_non_empty() {
        let cfg = ParsersConfig {
            enabled: vec!["md".to_string()],
        };
        cfg.validate().unwrap();
    }

    #[test]
    fn test_chunk_default_has_level_none() {
        let c = Chunk::default();
        assert_eq!(c.index, 0);
        assert!(c.heading.is_none());
        assert!(c.level.is_none());
        assert_eq!(c.content, "");
    }

    #[test]
    fn test_parse_bytes_default_delegates_to_parse_on_utf8() {
        // MarkdownParser は parse_bytes を override しない = default impl 経由。
        let doc = MarkdownParser
            .parse_bytes(b"## H\n\nbody enough body enough body enough body enough", "x.md", &[])
            .expect("valid utf-8 must parse");
        assert_eq!(doc.chunks.len(), 1);
        assert_eq!(doc.chunks[0].heading.as_deref(), Some("H"));
    }

    #[test]
    fn test_parse_bytes_default_errors_on_invalid_utf8() {
        // 不正 UTF-8 (0xFF 0xFE) は default impl が Err にする = index 全体 abort ではなく
        // per-file skip の起点。
        let err = TxtParser
            .parse_bytes(&[0xff, 0xfe, 0x00], "x.txt", &[])
            .expect_err("invalid utf-8 must be Err");
        assert!(err.to_string().contains("not valid UTF-8"));
    }

    #[test]
    fn test_is_binary_default_false_for_text_parsers() {
        assert!(!MarkdownParser.is_binary());
        assert!(!TxtParser.is_binary());
    }
}
