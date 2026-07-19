//! OOXML (docx/pptx) 共通 helper モジュール。zip + quick-xml で XML パートを
//! 読むための共有ロジックを置く。parser struct は持たない。
//!
//! `docProps/core.xml` (Dublin Core) → Frontmatter マッピング + zip entry
//! 読み出しを docx/xlsx/pptx parser が共有する (xlsx: Task 3.3、docx: Task 3.4
//! で消費済み。pptx は Task 3.5 で消費予定)。

use std::io::{Cursor, Read};

use anyhow::{Result, bail};
use quick_xml::events::{BytesRef, Event};
use quick_xml::reader::Reader;

use super::Frontmatter;

/// zip 内 `name` エントリを丸ごとバイト列で読む。無ければ `Ok(None)`。
/// `budget` (呼び出し側が文書単位で保持する累積展開済みバイト数) を通じて
/// 文書全体の解凍量を bound する。詳細は [`read_zip_entry_capped`] 参照
/// (このラッパーは cap に `super::MAX_RAW_BINARY_BYTES` を固定で使う)。
pub(crate) fn read_zip_entry(
    zip: &mut zip::ZipArchive<Cursor<&[u8]>>,
    name: &str,
    budget: &mut u64,
) -> Result<Option<Vec<u8>>> {
    read_zip_entry_capped(zip, name, budget, super::MAX_RAW_BINARY_BYTES)
}

/// `read_zip_entry` の cap 注入版。unit test が小さい cap で累積 budget
/// 超過分岐を突くため分離する (xlsx.rs::parse_workbook_bytes_capped の
/// cap 注入パターンを踏襲)。
///
/// zip-bomb hardening を 2 レイヤで行う:
///
/// - **codex P2 (PR #70 round 1)**: per-entry の展開を 2 段で bound する。
///   1. エントリの申告 uncompressed size (`ZipFile::size()`、local file
///      header 由来) が `cap` を超えていれば、展開を試みる前に即座に
///      `Ok(None)` を返す。
///   2. 実読みも `Read::take` で `cap+1` バイトまでに bound し、申告 size
///      が偽装された crafted zip (実際の解凍結果が申告よりずっと大きい)
///      でもメモリ確保は `cap+1` バイトどまりにした上で、読めたバイト数が
///      `cap` を超えていれば `Ok(None)` として拒否する。
/// - **codex P2 (PR #70 round 2)**: 上記は「1 エントリ」単位の防御であり、
///   `cap` 未満のエントリを多数読むと文書全体では累積が青天井になり得た
///   (例: 数百個の notesSlide/rels パートを合計すると数百 MB)。呼び出し側
///   が文書単位で保持する `budget` にこの関数の呼び出しごとに読んだバイト
///   数を加算し、累積が `cap` を超えたら `Err` を返す。1 エントリ拒否
///   (`Ok(None)`) ではなく文書単位で abort する — 既に相当量を展開済みの
///   文書をさらに読み進めるのは危険なため、呼び出し側 (`docx.rs` /
///   `pptx.rs` の `parse_bytes`) はこの `Err` を `?` でそのまま伝播し、
///   文書全体の parse を諦める。
fn read_zip_entry_capped(
    zip: &mut zip::ZipArchive<Cursor<&[u8]>>,
    name: &str,
    budget: &mut u64,
    cap: u64,
) -> Result<Option<Vec<u8>>> {
    let mut file = match zip.by_name(name) {
        Ok(f) => f,
        Err(_) => return Ok(None),
    };
    if file.size() > cap {
        return Ok(None);
    }
    let mut buf = Vec::new();
    // cap+1 まで読めれば「申告 size が嘘だった (cap を実際は超えている)」と
    // 判定できる。ちょうど cap バイトのエントリは正常に許可する。
    let limit = cap.saturating_add(1);
    if (&mut file).take(limit).read_to_end(&mut buf).is_err() {
        return Ok(None);
    }
    if buf.len() as u64 > cap {
        return Ok(None);
    }
    *budget = budget.saturating_add(buf.len() as u64);
    if *budget > cap {
        bail!(
            "cumulative decompressed size across zip entries exceeds {cap} bytes \
             (zip-bomb guard)"
        );
    }
    Ok(Some(buf))
}

/// `docProps/core.xml` があれば Frontmatter に map、無ければ filename fallback。
/// `budget` は `read_zip_entry` に渡す文書単位の累積展開済みバイト数
/// (呼び出し側が document.xml / slides 等の読み出しと共有する)。累積 cap
/// 超過時は `Err` を返す (呼び出し側は文書全体の parse を諦める)。
pub(crate) fn core_xml_frontmatter(
    zip: &mut zip::ZipArchive<Cursor<&[u8]>>,
    path_hint: &str,
    budget: &mut u64,
) -> Result<Frontmatter> {
    match read_zip_entry(zip, "docProps/core.xml", budget)? {
        Some(bytes) => Ok(parse_core_xml(&bytes, path_hint)),
        None => Ok(Frontmatter {
            title: super::txt::derive_title_pub(path_hint),
            ..Frontmatter::default()
        }),
    }
}

/// core.xml バイト列を parse する (名前空間 prefix を無視し local name で判定)。
///
/// codex P2 (PR #70 round 2): 旧実装は `Event::Text` を都度直接代入していた
/// ため、entity 参照 (quick-xml 0.38+ で `Event::Text` に含まれず
/// `Event::GeneralRef` として別 event で届く) を挟む値
/// (`<dc:title>R&amp;D</dc:title>` 等) が最後の Text fragment だけで
/// 上書きされ、それより前の部分 ("R&") が失われていた。docx/pptx 本文と
/// 同じく要素単位で Text + GeneralRef をバッファに蓄積してから `End` で
/// 確定する方式に変更する。
fn parse_core_xml(xml: &[u8], path_hint: &str) -> Frontmatter {
    let mut reader = Reader::from_reader(xml);
    reader.config_mut().trim_text(true);
    let mut fm = Frontmatter::default();
    let mut created: Option<String> = None;
    let mut modified: Option<String> = None;
    let mut buf = Vec::new();
    let mut cur: Option<Vec<u8>> = None; // 現在開いている要素の local name
    let mut text_buf = String::new(); // cur 要素の Text + GeneralRef 蓄積用

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                cur = Some(local_name_pub(e.name().as_ref()).to_vec());
                text_buf.clear();
            }
            Ok(Event::Text(t)) => {
                if cur.is_some() {
                    // quick-xml 0.41 は `BytesText::unescape()` を廃止し、
                    // encoding decode (`decode()`) と entity unescape
                    // (`quick_xml::escape::unescape()`) を分離した。
                    let decoded = t.decode().unwrap_or_default();
                    let text = quick_xml::escape::unescape(&decoded)
                        .map(|c| c.into_owned())
                        .unwrap_or_else(|_| decoded.into_owned());
                    text_buf.push_str(&text);
                }
            }
            Ok(Event::GeneralRef(r)) => {
                // quick-xml 0.38+ は entity 参照 (`&amp;` 等) を `Event::Text`
                // に含めず `Event::GeneralRef` として別 event で届ける。ここを
                // 処理しないと `<dc:title>R&amp;D</dc:title>` の "&" が欠落する
                // (docx.rs/pptx.rs 同様の必須処理)。
                if cur.is_some() {
                    text_buf.push_str(&resolve_general_ref(&r));
                }
            }
            Ok(Event::End(_)) => {
                if let Some(name) = cur.take() {
                    match name.as_slice() {
                        b"title" => {
                            if !text_buf.trim().is_empty() {
                                fm.title = Some(text_buf.trim().to_string());
                            }
                        }
                        b"created" => created = Some(text_buf.clone()),
                        b"modified" => modified = Some(text_buf.clone()),
                        b"keywords" => {
                            fm.tags = text_buf
                                .split([',', ';'])
                                .map(|s| s.trim().to_string())
                                .filter(|s| !s.is_empty())
                                .collect();
                        }
                        _ => {}
                    }
                }
                text_buf.clear();
            }
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
///
/// 旧実装の `d[..10]` は byte 境界チェック無しの panic-prone slice で、
/// `dcterms:created` / `modified` に multibyte 文字が混入し (例:
/// `"2026-07-1é..."`) byte offset 10 がその文字の内側に来ると
/// "byte index 10 is not a char boundary" で panic していた。docx/xlsx/pptx
/// parser は (PDF と違い) `catch_unwind` の外で呼ばれるため、この panic は
/// per-file skip に隔離されず `index` 実行全体を落とす — PR-1 で確立した
/// per-file 隔離原則への違反になる。`pdf.rs::normalize_pdf_date` の
/// ISO 分岐 (PR #69 round 3 の codex fix) と同じパターンで `d.get(..10)`
/// による境界安全化 + ASCII digit/`-` 検証に変更する。
fn iso_date_prefix(s: &str) -> Option<String> {
    let d = s.split('T').next().unwrap_or(s).trim();
    if d.len() >= 10
        && d.as_bytes()[4] == b'-'
        && d.as_bytes()[7] == b'-'
        && let Some(candidate) = d.get(..10)
        && candidate.bytes().all(|b| b.is_ascii_digit() || b == b'-')
    {
        Some(candidate.to_string())
    } else {
        None
    }
}

/// `cp:title` のような prefixed name から local part (`title`) を取る。
/// crate 内公開 (`pub(crate)`): docx/pptx parser が要素名判定 (namespace prefix
/// 無視) に使う (`parser/mod.rs::ooxml_local` 経由、Task 3.4/3.5 で消費)。
pub(crate) fn local_name_pub(qname: &[u8]) -> &[u8] {
    match qname.iter().position(|&b| b == b':') {
        Some(i) => &qname[i + 1..],
        None => qname,
    }
}

/// `Event::GeneralRef` (`&ref;` / `&#NN;`) を解決した文字列に変換する。数値参照
/// (`&#38;` / `&#x26;`) と XML 定義済み 5 entity (`amp`/`lt`/`gt`/`apos`/`quot`)
/// を解決する。未知の named entity (docx/pptx では実質発生しない、カスタム DTD
/// 前提) は best-effort でリテラル `&name;` として残す。
///
/// docx.rs (Task 3.4) と pptx.rs (Task 3.5) の両方が同じ quick-xml 0.38+ の
/// `Event::GeneralRef` 分割挙動 (entity 参照が `Event::Text` に含まれず別
/// event で届く) に対処する必要があるため、ここに共通化する (重複実装を避ける)。
pub(crate) fn resolve_general_ref(r: &BytesRef) -> String {
    if let Ok(Some(ch)) = r.resolve_char_ref() {
        return ch.to_string();
    }
    match r.decode() {
        Ok(name) => match quick_xml::escape::resolve_xml_entity(&name) {
            Some(s) => s.to_string(),
            None => format!("&{name};"),
        },
        Err(_) => String::new(),
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use zip::write::SimpleFileOptions;

    #[test]
    fn test_read_zip_entry_rejects_entry_declaring_size_over_cap() {
        // codex P2 (PR #70 round 1, zip-bomb hardening): `read_to_end` は
        // unbounded だったため、crafted 高圧縮ファイルで raw 50 MiB cap
        // (`parser::MAX_RAW_BINARY_BYTES`) をすり抜けてメモリ枯渇し得た。
        // 全 0 バイトの highly-compressible payload (圧縮後の zip 自体は
        // 小さいまま、申告 uncompressed size だけが cap を超える) で、
        // 展開を試みる前に None を返すことを検証する。
        let oversized_len = (super::super::MAX_RAW_BINARY_BYTES + 1) as usize;
        let payload = vec![0u8; oversized_len];
        let mut buf = Vec::new();
        {
            let mut zip = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
            zip.start_file("big.bin", SimpleFileOptions::default())
                .unwrap();
            zip.write_all(&payload).unwrap();
            zip.finish().unwrap();
        }
        drop(payload);

        let mut archive = zip::ZipArchive::new(Cursor::new(buf.as_slice())).unwrap();
        let mut budget: u64 = 0;
        assert!(
            read_zip_entry(&mut archive, "big.bin", &mut budget)
                .unwrap()
                .is_none(),
            "entry declaring uncompressed size > MAX_RAW_BINARY_BYTES must be rejected \
             without attempting to decompress it"
        );
    }

    #[test]
    fn test_read_zip_entry_accepts_entry_within_cap() {
        // cap ちょうど手前の通常サイズのエントリは従来通り読める (回帰確認)。
        let mut buf = Vec::new();
        {
            let mut zip = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
            zip.start_file("small.bin", SimpleFileOptions::default())
                .unwrap();
            zip.write_all(b"hello world").unwrap();
            zip.finish().unwrap();
        }
        let mut archive = zip::ZipArchive::new(Cursor::new(buf.as_slice())).unwrap();
        let mut budget: u64 = 0;
        assert_eq!(
            read_zip_entry(&mut archive, "small.bin", &mut budget).unwrap(),
            Some(b"hello world".to_vec())
        );
    }

    #[test]
    fn test_read_zip_entry_capped_rejects_cumulative_budget_over_cap() {
        // codex P2 (PR #70 round 2): 1 エントリずつは cap 未満でも、複数
        // エントリを積算すると budget を超え得る (例: 数百個の
        // notesSlide/rels パートの合計)。小さい cap を注入して、2 エントリ目
        // で累積が cap を超えたら Err になることを、実データを MB 単位で
        // 書かずに確認する (xlsx.rs::parse_workbook_bytes_capped と同じ
        // cap 注入パターン)。
        let mut buf = Vec::new();
        {
            let mut zip = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
            zip.start_file("a.bin", SimpleFileOptions::default())
                .unwrap();
            zip.write_all(b"0123456789").unwrap(); // 10 bytes
            zip.start_file("b.bin", SimpleFileOptions::default())
                .unwrap();
            zip.write_all(b"0123456789").unwrap(); // 10 bytes
            zip.finish().unwrap();
        }
        let mut archive = zip::ZipArchive::new(Cursor::new(buf.as_slice())).unwrap();
        let mut budget: u64 = 0;
        let cap: u64 = 15; // 1 エントリ (10 byte) は cap 未満だが 2 つ合計 20 byte は超える

        let first = read_zip_entry_capped(&mut archive, "a.bin", &mut budget, cap).unwrap();
        assert_eq!(first, Some(b"0123456789".to_vec()));
        assert_eq!(budget, 10);

        let second = read_zip_entry_capped(&mut archive, "b.bin", &mut budget, cap);
        assert!(
            second.is_err(),
            "cumulative budget (20 bytes) exceeding cap (15 bytes) across 2 within-cap \
             entries must be Err"
        );
    }

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
    fn test_parse_core_xml_preserves_entity_references() {
        // codex P2 (PR #70 round 2): 旧実装は Text event を都度直接代入して
        // いたため、entity 参照 (quick-xml 0.38+ で `Event::GeneralRef` として
        // 別 event で届く) を挟む値が最後の Text fragment だけで上書きされて
        // いた。`<dc:title>R&amp;D</dc:title>` は
        // Text("R") → 代入 "R" → GeneralRef("amp") は `_ => {}` で無視
        // → Text("D") → 代入 "D" (上書き) という経路を辿り、最終的に
        // fm.title == "D" になり "R&" が失われるバグがあった (keywords も同型)。
        // docx/pptx 本文と同じく要素単位で Text + GeneralRef を蓄積してから
        // End で確定する。
        let xml = r#"<?xml version="1.0"?>
<cp:coreProperties xmlns:cp="x" xmlns:dc="y">
  <dc:title>R&amp;D</dc:title>
  <cp:keywords>A&amp;B, C</cp:keywords>
</cp:coreProperties>"#
            .as_bytes();
        let fm = parse_core_xml(xml, "docs/rd.docx");
        assert_eq!(fm.title.as_deref(), Some("R&D"));
        assert_eq!(fm.tags, vec!["A&B".to_string(), "C".to_string()]);
    }

    #[test]
    fn test_parse_core_xml_missing_fields_fall_back() {
        let xml = br#"<cp:coreProperties xmlns:cp="x"></cp:coreProperties>"#;
        let fm = parse_core_xml(xml, "docs/no-meta.docx");
        assert_eq!(fm.title.as_deref(), Some("no meta")); // filename fallback
        assert!(fm.date.is_none());
        assert!(fm.tags.is_empty());
    }

    #[test]
    fn test_iso_date_prefix_multibyte_at_boundary_returns_none_not_panic() {
        // "2026-07-1" (9 ASCII bytes) の直後に 2-byte 文字 "é" (0xC3 0xA9) が
        // 続くため、byte offset 10 は "é" の内部にあり char 境界ではない。
        // 旧実装の `d[..10]` はここで panic していた (pdf.rs::normalize_pdf_date
        // の byte 境界 panic、PR #69 round 3 の codex fix と同一パターン)。
        assert_eq!(iso_date_prefix("2026-07-1é"), None);
    }

    #[test]
    fn test_iso_date_prefix_accepts_valid_iso_date() {
        assert_eq!(
            iso_date_prefix("2026-07-19T09:00:00Z"),
            Some("2026-07-19".to_string())
        );
    }
}
