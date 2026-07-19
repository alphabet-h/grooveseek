//! pptx (`.pptx`) parser. zip + quick-xml でスライド単位に `ppt/slides/slideN.xml`
//! を読む。
use anyhow::{Result, anyhow};

use super::{ParsedDocument, Parser, single_text_chunk};

pub struct PptxParser;

impl Parser for PptxParser {
    fn extension(&self) -> &'static str {
        "pptx"
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
    fn test_pptx_parser_is_binary() {
        assert!(PptxParser.is_binary());
        assert_eq!(PptxParser.extension(), "pptx");
    }

    #[test]
    fn test_pptx_parse_bytes_not_yet_implemented() {
        let err = PptxParser
            .parse_bytes(b"not a real pptx", "x.pptx", &[])
            .expect_err("skeleton parse_bytes must be Err");
        assert!(err.to_string().contains("not yet implemented"));
    }

    #[test]
    fn test_pptx_parse_fallback_wraps_raw_text() {
        let doc = PptxParser.parse("hello world content here", "x.pptx", &[]);
        assert_eq!(doc.chunks.len(), 1);
        assert!(doc.chunks[0].content.contains("hello world"));
    }
}
