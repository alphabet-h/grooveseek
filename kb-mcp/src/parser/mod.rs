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

use anyhow::Result;
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
}
