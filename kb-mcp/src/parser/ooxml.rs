//! OOXML (docx/pptx) 共通 helper モジュール。zip + quick-xml で XML パートを
//! 読むための共有ロジックを置く。parser struct は持たない。
//!
//! `docProps/core.xml` (Dublin Core) → Frontmatter マッピング + zip entry
//! 読み出しを docx/xlsx/pptx parser が共有する (Task 3.3/3.4 で消費予定)。

use std::io::{Cursor, Read};

use quick_xml::events::Event;
use quick_xml::reader::Reader;

use super::Frontmatter;

/// zip 内 `name` エントリを丸ごとバイト列で読む。無ければ None。
// Task 3.2 時点では呼び出し元 (docx/xlsx/pptx parser) が未実装のため未使用。
// Task 3.3/3.4 で消費予定 (feature-45)。
#[allow(dead_code)]
pub(crate) fn read_zip_entry(
    zip: &mut zip::ZipArchive<Cursor<&[u8]>>,
    name: &str,
) -> Option<Vec<u8>> {
    let mut file = zip.by_name(name).ok()?;
    let mut buf = Vec::new();
    file.read_to_end(&mut buf).ok()?;
    Some(buf)
}

/// `docProps/core.xml` があれば Frontmatter に map、無ければ filename fallback。
// Task 3.2 時点では呼び出し元 (docx/xlsx/pptx parser) が未実装のため未使用。
// Task 3.3/3.4 で消費予定 (feature-45)。
#[allow(dead_code)]
pub(crate) fn core_xml_frontmatter(
    zip: &mut zip::ZipArchive<Cursor<&[u8]>>,
    path_hint: &str,
) -> Frontmatter {
    match read_zip_entry(zip, "docProps/core.xml") {
        Some(bytes) => parse_core_xml(&bytes, path_hint),
        None => Frontmatter {
            title: super::txt::derive_title_pub(path_hint),
            ..Frontmatter::default()
        },
    }
}

/// core.xml バイト列を parse する (名前空間 prefix を無視し local name で判定)。
fn parse_core_xml(xml: &[u8], path_hint: &str) -> Frontmatter {
    let mut reader = Reader::from_reader(xml);
    reader.config_mut().trim_text(true);
    let mut fm = Frontmatter::default();
    let mut created: Option<String> = None;
    let mut modified: Option<String> = None;
    let mut buf = Vec::new();
    let mut cur: Option<Vec<u8>> = None; // 現在開いている要素の local name

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                cur = Some(local_name(e.name().as_ref()).to_vec());
            }
            Ok(Event::Text(t)) => {
                if let Some(name) = &cur {
                    // quick-xml 0.41 は `BytesText::unescape()` を廃止し、
                    // encoding decode (`decode()`) と entity unescape
                    // (`quick_xml::escape::unescape()`) を分離した。
                    let decoded = t.decode().unwrap_or_default();
                    let text = quick_xml::escape::unescape(&decoded)
                        .map(|c| c.into_owned())
                        .unwrap_or_else(|_| decoded.into_owned());
                    match name.as_slice() {
                        b"title" => {
                            if !text.trim().is_empty() {
                                fm.title = Some(text.trim().to_string());
                            }
                        }
                        b"created" => created = Some(text),
                        b"modified" => modified = Some(text),
                        b"keywords" => {
                            fm.tags = text
                                .split([',', ';'])
                                .map(|s| s.trim().to_string())
                                .filter(|s| !s.is_empty())
                                .collect();
                        }
                        _ => {}
                    }
                }
            }
            Ok(Event::End(_)) => cur = None,
            Ok(Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
        buf.clear();
    }

    // date = created 優先、無ければ modified。ISO 8601 の date 部分のみ。
    fm.date = created.or(modified).and_then(|s| iso_date_prefix(&s));
    if fm.title.as_deref().map(str::is_empty).unwrap_or(true) {
        fm.title = super::txt::derive_title_pub(path_hint);
    }
    fm
}

/// `2026-07-19T09:00:00Z` → `2026-07-19`。
fn iso_date_prefix(s: &str) -> Option<String> {
    let d = s.split('T').next().unwrap_or(s).trim();
    if d.len() >= 10 && d.as_bytes()[4] == b'-' && d.as_bytes()[7] == b'-' {
        Some(d[..10].to_string())
    } else {
        None
    }
}

/// `cp:title` のような prefixed name から local part (`title`) を取る。
fn local_name(qname: &[u8]) -> &[u8] {
    match qname.iter().position(|&b| b == b':') {
        Some(i) => &qname[i + 1..],
        None => qname,
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_core_xml_maps_dublin_core() {
        // 非 ASCII を含むため raw byte string (`br#"..."#`) は使えない
        // (raw byte string literal は ASCII 限定)。`r#"..."#.as_bytes()` で代用する。
        let xml = r#"<?xml version="1.0"?>
<cp:coreProperties xmlns:cp="x" xmlns:dc="y" xmlns:dcterms="z">
  <dc:title>四半期レポート</dc:title>
  <dcterms:created>2026-07-19T09:00:00Z</dcterms:created>
  <cp:keywords>売上, 予測 ;分析</cp:keywords>
</cp:coreProperties>"#
            .as_bytes();
        let fm = parse_core_xml(xml, "docs/report.docx");
        assert_eq!(fm.title.as_deref(), Some("四半期レポート"));
        assert_eq!(fm.date.as_deref(), Some("2026-07-19"));
        assert_eq!(
            fm.tags,
            vec!["売上".to_string(), "予測".to_string(), "分析".to_string()]
        );
    }

    #[test]
    fn test_parse_core_xml_missing_fields_fall_back() {
        let xml = br#"<cp:coreProperties xmlns:cp="x"></cp:coreProperties>"#;
        let fm = parse_core_xml(xml, "docs/no-meta.docx");
        assert_eq!(fm.title.as_deref(), Some("no meta")); // filename fallback
        assert!(fm.date.is_none());
        assert!(fm.tags.is_empty());
    }
}
