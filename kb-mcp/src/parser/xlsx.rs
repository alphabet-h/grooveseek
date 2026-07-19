//! xlsx / xls (`.xlsx` / `.xls`) parser. calamine 経由でシート単位チャンク。
use anyhow::{Result, anyhow};

use super::{ParsedDocument, Parser, single_text_chunk};

pub struct XlsxParser;
pub struct XlsParser;

impl Parser for XlsxParser {
    fn extension(&self) -> &'static str {
        "xlsx"
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

impl Parser for XlsParser {
    fn extension(&self) -> &'static str {
        "xls"
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
    fn test_xlsx_parser_is_binary() {
        assert!(XlsxParser.is_binary());
        assert_eq!(XlsxParser.extension(), "xlsx");
    }

    #[test]
    fn test_xls_parser_is_binary() {
        assert!(XlsParser.is_binary());
        assert_eq!(XlsParser.extension(), "xls");
    }

    #[test]
    fn test_xlsx_parse_bytes_not_yet_implemented() {
        let err = XlsxParser
            .parse_bytes(b"not a real xlsx", "x.xlsx", &[])
            .expect_err("skeleton parse_bytes must be Err");
        assert!(err.to_string().contains("not yet implemented"));
    }

    #[test]
    fn test_xls_parse_bytes_not_yet_implemented() {
        let err = XlsParser
            .parse_bytes(b"not a real xls", "x.xls", &[])
            .expect_err("skeleton parse_bytes must be Err");
        assert!(err.to_string().contains("not yet implemented"));
    }

    #[test]
    fn test_xlsx_parse_fallback_wraps_raw_text() {
        let doc = XlsxParser.parse("hello world content here", "x.xlsx", &[]);
        assert_eq!(doc.chunks.len(), 1);
        assert!(doc.chunks[0].content.contains("hello world"));
    }
}
