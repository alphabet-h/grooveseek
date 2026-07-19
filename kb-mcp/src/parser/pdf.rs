//! PDF (`.pdf`) parser. ページ単位チャンク + metadata frontmatter。
//! 抽出は oxidize-pdf (純 Rust, ParseResult ベース)。念のため malformed PDF の
//! panic は catch_unwind で per-file skip に正規化する (§4.5 / spec §3 #14)。

use anyhow::{Result, anyhow};

use super::{Frontmatter, ParsedDocument, Parser, single_text_chunk};

pub struct PdfParser;

impl Parser for PdfParser {
    fn extension(&self) -> &'static str {
        "pdf"
    }

    fn is_binary(&self) -> bool {
        true
    }

    /// trait 契約用 fallback: 既に抽出済みテキストを 1 チャンクに包む
    /// (実運用では parse_bytes 経由でしか呼ばれない)。panic しない。
    fn parse(&self, raw: &str, path_hint: &str, _exclude_headings: &[&str]) -> ParsedDocument {
        single_text_chunk(raw, path_hint)
    }

    fn parse_bytes(
        &self,
        bytes: &[u8],
        path_hint: &str,
        _exclude_headings: &[&str],
    ) -> Result<ParsedDocument> {
        // Task 2.3 で実装。skeleton では未実装エラー。
        let _ = (bytes, path_hint);
        Err(anyhow!("PdfParser::parse_bytes not yet implemented"))
    }
}
