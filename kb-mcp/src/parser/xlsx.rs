//! xlsx / xls (`.xlsx` / `.xls`) parser. calamine 経由でシート単位チャンク。
use std::io::Cursor;

use anyhow::{Result, anyhow};
use calamine::{Data, Reader};

use super::{Chunk, ParsedDocument, Parser, single_text_chunk};

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
        parse_workbook_bytes(bytes, path_hint, WorkbookFormat::Xlsx)
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
        parse_workbook_bytes(bytes, path_hint, WorkbookFormat::Xls)
    }
}

/// `parse_workbook_bytes[_capped]` が開く対象の書式。
///
/// codex P2 (PR #70 round 3): `calamine::open_workbook_auto_from_rs` は
/// bytes から XLS → XLSX → XLSB → ODS の順に reader を確率的に probe する。
/// これだと `.xlsx` として登録されたファイルの実体が XLSB/ODS (payload が
/// `.bin`/`content.xml` にあり、xlsx-layout 前提の pre-flight
/// (`xl/worksheets/*.xml` 等の申告 size 合計) を素通りする) でも probe が
/// 成功してしまい、xlsx 専用 pre-flight を完全にバイパスして calamine が
/// 展開してしまう。登録拡張子に一致する reader だけを明示的に使うことで
/// この抜け道を塞ぐ (拡張子と実形式が不一致なら reader open が Err になり
/// 既存の per-file skip 経路に落ちる — 「登録した形式だけを parse する」
/// opt-in 原則とも整合する)。
#[derive(Clone, Copy)]
enum WorkbookFormat {
    Xlsx,
    Xls,
}

/// シートあたりの抽出テキスト上限 (byte)。数百万行 xlsx で chunk が数百 MB に
/// なる事故を防ぐ (§4.5)。**truncate の粒度は行単位**: ある行を push した結果
/// 合計が cap を超えたら、その行までを保持して break する。したがって
/// - 最終 content の長さは `cap + (超過を招いた 1 行の bytes)` 以内 (行途中では切らない)、
/// - **1 行だけで cap を超える場合もその行は丸ごと emit** してから break する
///   (巨大セル 1 個で切断すると内容が中途半端になるより、1 行は完全に残す方針)。
const SHEET_MAX_BYTES: usize = 1024 * 1024;

/// xlsx / xls 共有の抽出本体。`SHEET_MAX_BYTES` を渡す薄い wrapper。
fn parse_workbook_bytes(
    bytes: &[u8],
    path_hint: &str,
    format: WorkbookFormat,
) -> Result<ParsedDocument> {
    parse_workbook_bytes_capped(bytes, path_hint, format, SHEET_MAX_BYTES)
}

/// cap を注入できる本体 (unit test が小さい cap で truncate 分岐を突くため分離)。
fn parse_workbook_bytes_capped(
    bytes: &[u8],
    path_hint: &str,
    format: WorkbookFormat,
    sheet_max_bytes: usize,
) -> Result<ParsedDocument> {
    // codex P2 (PR #70 round 2, zip-bomb hardening): xlsx (zip container) の
    // みが対象。calamine は zip 展開を内部で行うため `ooxml::read_zip_entry`
    // の累積 budget 機構が効かない。calamine を呼ぶ前に、実際に読まれる
    // XML part (worksheets/sharedStrings/styles) の申告 uncompressed size
    // 合計を検査する。xls (BIFF、非 zip container) はこの経路の対象外。
    if matches!(format, WorkbookFormat::Xlsx) {
        preflight_xlsx_decompression_budget(bytes, path_hint)?;
    }

    let cursor = Cursor::new(bytes);
    // codex P2 (PR #70 round 3): `open_workbook_auto_from_rs` (auto-probe)
    // ではなく、登録拡張子に一致する reader を明示的に使う (WorkbookFormat
    // の doc comment 参照)。`calamine::Sheets` に手動で包むことで、以降の
    // コード (sheet_names / worksheet_range 呼び出し) は auto-probe 版と
    // 同じ trait インタフェースのまま変更不要にする。
    let mut workbook: calamine::Sheets<_> = match format {
        WorkbookFormat::Xlsx => calamine::Xlsx::new(cursor)
            .map(calamine::Sheets::Xlsx)
            .map_err(|e| {
                anyhow!("{path_hint}: cannot open workbook (encrypted or corrupt): {e}")
            })?,
        WorkbookFormat::Xls => calamine::Xls::new(cursor)
            .map(calamine::Sheets::Xls)
            .map_err(|e| {
                anyhow!("{path_hint}: cannot open workbook (encrypted or corrupt): {e}")
            })?,
    };

    let mut chunks = Vec::new();
    // codex P2 (PR #70 round 2): `workbook.worksheets()` は全シートを一括で
    // `Vec<(String, Range<Data>)>` に materialize するため、シート数×サイズ
    // が大きい workbook でピークメモリが跳ね上がる。`sheet_names()` +
    // シート毎 `worksheet_range()` の逐次処理に変え、各シートの Range を
    // 処理し終えたら都度 drop してメモリを解放する。
    for name in workbook.sheet_names() {
        let range = match workbook.worksheet_range(&name) {
            Ok(r) => r,
            // 旧実装 (`worksheets()`) も内部で `.ok()?` により読めない
            // シートを黙って skip していたのと同じ挙動
            // (calamine::xlsx::Xlsx::worksheets 実装参照)。
            Err(_) => continue,
        };
        let mut text = String::new();
        for row in range.rows() {
            let line: Vec<String> = row
                .iter()
                .map(|cell| match cell {
                    Data::Empty => String::new(),
                    other => other.to_string(),
                })
                .collect();
            text.push_str(&line.join("\t"));
            text.push('\n');
            // 行 push 後に判定 = 超過を招いた行は保持される (overshoot は 1 行ぶん、
            // 巨大 1 行もその行は丸ごと残す)。次の行には進まず break + warn。
            if text.len() > sheet_max_bytes {
                eprintln!(
                    "{path_hint}: sheet {name:?} exceeds {sheet_max_bytes} bytes; truncating"
                );
                break;
            }
        }
        drop(range); // シート毎に明示的に drop してメモリを解放 (逐次処理)
        if text.trim().is_empty() {
            continue; // 空シートは chunk を作らない
        }
        chunks.push(Chunk {
            index: chunks.len(),
            heading: Some(format!("Sheet: {name}")),
            level: Some(2),
            content: text.trim_end().to_string(),
        });
    }

    // frontmatter: xlsx は docProps/core.xml。xls (BIFF) は core.xml が無いため
    // 常に filename fallback。zip として開けなければ filename fallback。
    let frontmatter = xlsx_frontmatter(bytes, path_hint);
    let raw_content = chunks
        .iter()
        .map(|c| c.content.as_str())
        .collect::<Vec<_>>()
        .join("\n\n");
    Ok(ParsedDocument {
        frontmatter,
        chunks,
        raw_content,
    })
}

/// calamine 呼び出し前の pre-flight 検査 (cap は固定で `super::MAX_RAW_BINARY_BYTES`
/// を使う薄い wrapper)。詳細は [`preflight_xlsx_decompression_budget_capped`] 参照。
fn preflight_xlsx_decompression_budget(bytes: &[u8], path_hint: &str) -> Result<()> {
    preflight_xlsx_decompression_budget_capped(bytes, path_hint, super::MAX_RAW_BINARY_BYTES)
}

/// `xl/worksheets/*.xml` + `xl/sharedStrings.xml` + `xl/styles.xml` の申告
/// uncompressed size (zip local file header 由来、展開はしない) を合計し、
/// `cap` を超えていれば calamine を呼ぶ前に `Err` にする。
///
/// zip として開けない場合 (xls = BIFF、非 zip container) は対象外として
/// `Ok(())` を返す — xls はこの経路の攻撃面ではない (calamine の BIFF
/// parser は zip 展開を経由しない)。
///
/// cap を注入できる版として分離する (unit test が小さい cap で on-the-fly の
/// 小さい fixture のまま超過分岐を突くため。`parse_workbook_bytes_capped`
/// の `sheet_max_bytes` 注入パターンを踏襲)。
fn preflight_xlsx_decompression_budget_capped(
    bytes: &[u8],
    path_hint: &str,
    cap: u64,
) -> Result<()> {
    let Ok(mut zip) = zip::ZipArchive::new(Cursor::new(bytes)) else {
        return Ok(());
    };
    let mut total: u64 = 0;
    for i in 0..zip.len() {
        let Ok(entry) = zip.by_index_raw(i) else {
            continue;
        };
        let name = entry.name();
        let is_target = name == "xl/sharedStrings.xml"
            || name == "xl/styles.xml"
            || (name.starts_with("xl/worksheets/") && name.ends_with(".xml"));
        if is_target {
            total = total.saturating_add(entry.size());
        }
    }
    if total > cap {
        anyhow::bail!(
            "{path_hint}: declared xl worksheets/sharedStrings/styles size ({total} bytes) \
             exceeds {cap} bytes cap (zip-bomb guard)"
        );
    }
    Ok(())
}

fn xlsx_frontmatter(bytes: &[u8], path_hint: &str) -> super::Frontmatter {
    if let Ok(mut zip) = zip::ZipArchive::new(Cursor::new(bytes)) {
        let mut budget: u64 = 0;
        super::ooxml::core_xml_frontmatter(&mut zip, path_hint, &mut budget).unwrap_or_else(|_| {
            super::Frontmatter {
                title: super::txt::derive_title_pub(path_hint),
                ..Default::default()
            }
        })
    } else {
        super::Frontmatter {
            title: super::txt::derive_title_pub(path_hint),
            ..Default::default()
        }
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

    /// inlineStr セルだけで最小 xlsx を組む (sharedStrings 省略)。
    /// sheets = [(sheet_name, rows=[[cell,...],...])]。
    fn make_minimal_xlsx(sheets: &[(&str, &[&[&str]])]) -> Vec<u8> {
        let mut buf = Vec::new();
        {
            let mut zip = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
            let opt = SimpleFileOptions::default();

            // [Content_Types].xml
            zip.start_file("[Content_Types].xml", opt).unwrap();
            let mut ct = String::from(
                r#"<?xml version="1.0"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/>"#,
            );
            for i in 1..=sheets.len() {
                ct.push_str(&format!(r#"<Override PartName="/xl/worksheets/sheet{i}.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/>"#));
            }
            ct.push_str("</Types>");
            zip.write_all(ct.as_bytes()).unwrap();

            // _rels/.rels
            zip.start_file("_rels/.rels", opt).unwrap();
            zip.write_all(br#"<?xml version="1.0"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/></Relationships>"#).unwrap();

            // xl/workbook.xml
            zip.start_file("xl/workbook.xml", opt).unwrap();
            let mut wb = String::from(
                r#"<?xml version="1.0"?><workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><sheets>"#,
            );
            for (i, (name, _)) in sheets.iter().enumerate() {
                wb.push_str(&format!(
                    r#"<sheet name="{name}" sheetId="{}" r:id="rId{}"/>"#,
                    i + 1,
                    i + 1
                ));
            }
            wb.push_str("</sheets></workbook>");
            zip.write_all(wb.as_bytes()).unwrap();

            // xl/_rels/workbook.xml.rels
            zip.start_file("xl/_rels/workbook.xml.rels", opt).unwrap();
            let mut rels = String::from(
                r#"<?xml version="1.0"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">"#,
            );
            for i in 1..=sheets.len() {
                rels.push_str(&format!(r#"<Relationship Id="rId{i}" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet{i}.xml"/>"#));
            }
            rels.push_str("</Relationships>");
            zip.write_all(rels.as_bytes()).unwrap();

            // xl/worksheets/sheetN.xml (inlineStr)
            for (i, (_, rows)) in sheets.iter().enumerate() {
                zip.start_file(format!("xl/worksheets/sheet{}.xml", i + 1), opt)
                    .unwrap();
                let mut sh = String::from(
                    r#"<?xml version="1.0"?><worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData>"#,
                );
                for (r, row) in rows.iter().enumerate() {
                    sh.push_str(&format!(r#"<row r="{}">"#, r + 1));
                    for (c, cell) in row.iter().enumerate() {
                        let col = (b'A' + c as u8) as char;
                        sh.push_str(&format!(
                            r#"<c r="{col}{}" t="inlineStr"><is><t>{cell}</t></is></c>"#,
                            r + 1
                        ));
                    }
                    sh.push_str("</row>");
                }
                sh.push_str("</sheetData></worksheet>");
                zip.write_all(sh.as_bytes()).unwrap();
            }
            zip.finish().unwrap();
        }
        buf
    }

    #[test]
    fn test_xlsx_preflight_rejects_cumulative_declared_size_over_cap() {
        // codex P2 (PR #70 round 2, zip-bomb hardening): calamine は zip
        // 展開を内部で行うため `ooxml::read_zip_entry` の累積 budget 機構が
        // 効かない。calamine を呼ぶ前に xl/worksheets/*.xml の申告
        // uncompressed size を検査し、cap 超過なら Err にする。実データを
        // MB 単位で書かずに、小さい cap を注入して on-the-fly の小さい
        // fixture のまま再現する (xlsx.rs::parse_workbook_bytes_capped と
        // 同じ cap 注入パターン)。
        let bytes = make_minimal_xlsx(&[("S", &[&["x"]])]);
        let err = preflight_xlsx_decompression_budget_capped(&bytes, "x.xlsx", 5)
            .expect_err("declared worksheet xml size over the injected 5-byte cap must be Err");
        assert!(
            err.to_string().contains("zip-bomb guard"),
            "error message should mention zip-bomb guard, got: {err}"
        );
    }

    #[test]
    fn test_xlsx_preflight_allows_within_real_cap() {
        // 通常サイズの xlsx は実 cap (MAX_RAW_BINARY_BYTES) 内で問題なく通る
        // (回帰確認)。公開 wrapper (`preflight_xlsx_decompression_budget`)
        // 経由でも検証する。
        let bytes = make_minimal_xlsx(&[("S", &[&["x"]])]);
        preflight_xlsx_decompression_budget(&bytes, "x.xlsx")
            .expect("declared size well within the real cap must be Ok");
    }

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

    // NOTE: skeleton 時点の `not_yet_implemented` 固定文言 assert は、Task 3.3 で
    // parse_bytes を実本実装したため意味が失われた (実装済みなのに未実装 err を期待
    // するのは矛盾)。PDF 実装 (Task 2.3, `test_pdf_malformed_bytes_is_err_not_panic`)
    // の前例に倣い、garbage 入力が real error path (calamine の format 未検出) で
    // panic せず Err になることを検証するテストに更新する
    // (team-lead 指示: 「skeleton の parse_bytes Err test があれば brief の指示に従う」)。
    #[test]
    fn test_xlsx_parse_bytes_garbage_is_err() {
        let err = XlsxParser
            .parse_bytes(b"not a real xlsx", "x.xlsx", &[])
            .expect_err("garbage bytes must be Err");
        assert!(err.to_string().contains("cannot open workbook"));
    }

    #[test]
    fn test_xls_parse_bytes_garbage_is_err() {
        let err = XlsParser
            .parse_bytes(b"not a real xls", "x.xls", &[])
            .expect_err("garbage bytes must be Err");
        assert!(err.to_string().contains("cannot open workbook"));
    }

    /// ODS (OpenDocument Spreadsheet) として calamine の `Ods::new` が読める
    /// 最小 zip を組む: `mimetype` (46 byte、非圧縮慣習だが zip crate は展開を
    /// 透過的に扱うので圧縮でも可) + `META-INF/manifest.xml` (暗号化チェックが
    /// 参照するため必須、無いと `FileNotFound` になる) + `content.xml`
    /// (calamine の content.xml parser は generic な event loop で未知要素を
    /// 無視するため、有効な XML であれば `office:document-content` 等の
    /// ラッパーすら不要)。
    fn make_fake_ods_bytes() -> Vec<u8> {
        let mut buf = Vec::new();
        {
            let mut zip = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
            let opt = SimpleFileOptions::default();
            zip.start_file("mimetype", opt).unwrap();
            zip.write_all(b"application/vnd.oasis.opendocument.spreadsheet")
                .unwrap();
            zip.start_file("META-INF/manifest.xml", opt).unwrap();
            zip.write_all(
                br#"<?xml version="1.0"?><manifest:manifest xmlns:manifest="urn:oasis:names:tc:opendocument:xmlns:manifest:1.0" manifest:version="1.2"><manifest:file-entry manifest:full-path="/" manifest:version="1.2" manifest:media-type="application/vnd.oasis.opendocument.spreadsheet"/></manifest:manifest>"#,
            )
            .unwrap();
            zip.start_file("content.xml", opt).unwrap();
            zip.write_all(br#"<?xml version="1.0"?><a/>"#).unwrap();
            zip.finish().unwrap();
        }
        buf
    }

    #[test]
    fn test_xlsx_parser_rejects_ods_payload_disguised_as_xlsx() {
        // codex P2 (PR #70 round 3): `open_workbook_auto_from_rs` は bytes
        // から XLS/XLSX/XLSB/ODS を確率的に probe するため、`.xlsx` 名の
        // 実体が ODS (payload が content.xml にあり、xlsx-layout 前提の
        // pre-flight = xl/worksheets 等の合計 が総和 0 で素通りする) でも
        // auto-probe が ODS reader として成功してしまい、xlsx 専用
        // pre-flight を完全にバイパスして calamine が展開してしまって
        // いた。XlsxParser は拡張子に一致する Xlsx reader のみを明示使用
        // するため、ODS payload では reader open 自体が Err になり
        // per-file skip 経路に落ちることを検証する。
        let bytes = make_fake_ods_bytes();
        let err = XlsxParser.parse_bytes(&bytes, "fake.xlsx", &[]).expect_err(
            "ODS payload disguised as .xlsx must be rejected (no cross-format probing)",
        );
        assert!(
            err.to_string().contains("cannot open workbook"),
            "got: {err}"
        );
    }

    #[test]
    fn test_xlsx_parse_fallback_wraps_raw_text() {
        let doc = XlsxParser.parse("hello world content here", "x.xlsx", &[]);
        assert_eq!(doc.chunks.len(), 1);
        assert!(doc.chunks[0].content.contains("hello world"));
    }

    #[test]
    fn test_xlsx_sheet_chunks() {
        let bytes = make_minimal_xlsx(&[
            ("Sales", &[&["Q1", "100"], &["Q2", "200"]]),
            ("Notes", &[&["memo"]]),
        ]);
        let doc = XlsxParser
            .parse_bytes(&bytes, "docs/book.xlsx", &[])
            .unwrap();
        assert_eq!(doc.chunks.len(), 2);
        assert_eq!(doc.chunks[0].heading.as_deref(), Some("Sheet: Sales"));
        assert_eq!(doc.chunks[0].level, Some(2));
        assert!(doc.chunks[0].content.contains("Q1\t100"));
        assert!(doc.chunks[0].content.contains("Q2\t200"));
        assert_eq!(doc.chunks[1].heading.as_deref(), Some("Sheet: Notes"));
    }

    #[test]
    fn test_xlsx_empty_sheet_produces_no_chunk() {
        let bytes = make_minimal_xlsx(&[("Empty", &[]), ("Has", &[&["x"]])]);
        let doc = XlsxParser.parse_bytes(&bytes, "b.xlsx", &[]).unwrap();
        assert_eq!(doc.chunks.len(), 1, "empty sheet must be skipped");
        assert_eq!(doc.chunks[0].heading.as_deref(), Some("Sheet: Has"));
    }

    #[test]
    fn test_xls_parser_shares_impl_and_extension() {
        assert_eq!(XlsParser.extension(), "xls");
        assert!(XlsParser.is_binary());
    }

    #[test]
    fn test_xlsx_sheet_truncates_past_cap() {
        // cap 注入版で truncate 分岐を突く (実 1 MiB fixture を組まずに検証)。
        // 各行 "aN\tbN\n" = 6 byte。判定は「行 push 後に text.len() > cap なら break」。
        // cap=10 byte だと: 行1 後 len=6 (継続) → 行2 後 len=12 > 10 (break)。
        // 行3 は未処理で落ちる = test の意図 (a3 を含まない) と一致する。
        // (cap=15 だと行3 push 後 len=18 で初めて break し、a3 が保持されてしまうため不可)。
        let bytes = make_minimal_xlsx(&[("S", &[&["a1", "b1"], &["a2", "b2"], &["a3", "b3"]])]);
        let doc = parse_workbook_bytes_capped(&bytes, "x.xlsx", WorkbookFormat::Xlsx, 10).unwrap();
        assert_eq!(doc.chunks.len(), 1);
        let content = &doc.chunks[0].content;
        assert!(
            !content.contains("a3"),
            "rows past the cap must be truncated: {content:?}"
        );
        // overshoot = cap + 直前 1 行分のみ (行途中では切らない)。1 行は数 bytes なので cap+64 で十分。
        assert!(
            content.len() <= 10 + 64,
            "overshoot must be bounded to one row worth of bytes, got len={}",
            content.len()
        );
    }

    #[test]
    fn test_xlsx_single_row_larger_than_cap_emits_that_row_whole() {
        // 1 行だけで cap を超える境界: その行は丸ごと emit してから break し、
        // 後続行は落とす (= 行途中での切断はしない、という明示された挙動)。
        let big = "x".repeat(200);
        let bytes = make_minimal_xlsx(&[("S", &[&[big.as_str()], &["next"]])]);
        let doc = parse_workbook_bytes_capped(&bytes, "x.xlsx", WorkbookFormat::Xlsx, 50).unwrap();
        let content = &doc.chunks[0].content;
        assert!(
            content.contains(&big),
            "the first oversized row is emitted whole"
        );
        assert!(
            !content.contains("next"),
            "subsequent rows are dropped after the cap"
        );
    }
}
