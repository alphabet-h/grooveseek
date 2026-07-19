//! docx (`.docx`) parser. zip + quick-xml で `word/document.xml` を読む。
use anyhow::{Result, anyhow};

use super::{ParsedDocument, Parser, single_text_chunk};

pub struct DocxParser;

impl Parser for DocxParser {
    fn extension(&self) -> &'static str {
        "docx"
    }

    fn is_binary(&self) -> bool {
        true
    }

    fn parse(&self, raw: &str, path_hint: &str, _exclude_headings: &[&str]) -> ParsedDocument {
        single_text_chunk(raw, path_hint)
    }

    fn parse_bytes(
        &self,
        bytes: &[u8],
        path_hint: &str,
        _exclude_headings: &[&str],
    ) -> Result<ParsedDocument> {
        let _ = (bytes, path_hint);
        Err(anyhow!("not yet implemented"))
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_docx_parser_is_binary() {
        assert!(DocxParser.is_binary());
        assert_eq!(DocxParser.extension(), "docx");
    }

    #[test]
    fn test_docx_parse_bytes_not_yet_implemented() {
        let err = DocxParser
            .parse_bytes(b"not a real docx", "x.docx", &[])
            .expect_err("skeleton parse_bytes must be Err");
        assert!(err.to_string().contains("not yet implemented"));
    }

    #[test]
    fn test_docx_parse_fallback_wraps_raw_text() {
        let doc = DocxParser.parse("hello world content here", "x.docx", &[]);
        assert_eq!(doc.chunks.len(), 1);
        assert!(doc.chunks[0].content.contains("hello world"));
    }
}
