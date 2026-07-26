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

    fn parse_bytes_inner(
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

    fn parse_bytes_inner(
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
/// XLSX/XLS で抽出方式が異なる (下記 fn 群 の doc comment 参照) ため format
/// 別の実装へ dispatch するだけの薄い wrapper。
fn parse_workbook_bytes_capped(
    bytes: &[u8],
    path_hint: &str,
    format: WorkbookFormat,
    sheet_max_bytes: usize,
) -> Result<ParsedDocument> {
    match format {
        WorkbookFormat::Xlsx => parse_xlsx_bytes_capped(bytes, path_hint, sheet_max_bytes),
        WorkbookFormat::Xls => parse_xls_bytes_capped(bytes, path_hint, sheet_max_bytes),
    }
}

/// xlsx (zip container) の抽出本体。streaming cell reader を使う (下記
/// `extract_xlsx_sheet_text_streaming` 参照)。
fn parse_xlsx_bytes_capped(
    bytes: &[u8],
    path_hint: &str,
    sheet_max_bytes: usize,
) -> Result<ParsedDocument> {
    // codex P2 (PR #70 round 2, zip-bomb hardening): calamine は zip 展開を
    // 内部で行うため `ooxml::read_zip_entry` の累積 budget 機構が効かない。
    // calamine を呼ぶ前に、実際に読まれる XML part の申告 uncompressed
    // size 合計を検査する。
    preflight_xlsx_decompression_budget(bytes, path_hint)?;

    let cursor = Cursor::new(bytes);
    // codex P2 (PR #70 round 3): `open_workbook_auto_from_rs` (auto-probe)
    // ではなく、登録拡張子に一致する reader を明示的に使う (WorkbookFormat
    // の doc comment 参照)。
    let mut xlsx = calamine::Xlsx::new(cursor)
        .map_err(|e| anyhow!("{path_hint}: cannot open workbook (encrypted or corrupt): {e}"))?;

    let frontmatter = xlsx_frontmatter(bytes, path_hint);
    let title = frontmatter.title.as_deref().unwrap_or("");
    let mut chunks = Vec::new();
    // `sheet_names()` は Vec<String> を owned で返すため、ループ中に
    // `&mut xlsx` を再度借用しても (streaming reader との) 競合はない。
    for name in xlsx.sheet_names() {
        let Some(text) =
            extract_xlsx_sheet_text_streaming(&mut xlsx, &name, path_hint, sheet_max_bytes)
        else {
            // open 失敗 / 途中の XML エラーはシート丸ごと skip
            // (旧実装の `worksheet_range()` エラー時と同じ挙動)。
            continue;
        };
        if text.trim().is_empty() {
            continue; // 空シートは chunk を作らない
        }
        let heading = format!("Sheet: {name}");
        let context = super::build_context(&[title, &heading]);
        chunks.push(Chunk {
            index: chunks.len(),
            heading: Some(heading),
            level: Some(2),
            content: text.trim_end().to_string(),
            context,
        });
    }

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

/// xlsx 1 シート分のテキストを streaming cell reader (`Xlsx::worksheet_cells_reader`)
/// で抽出する。open 失敗 / 途中の XML エラーは `None` を返し、呼び出し側は
/// このシート全体を skip する (旧実装の `worksheet_range()` エラー時の
/// 「シート丸ごと skip」と同じ挙動)。
///
/// codex P1 (PR #70 round 6): `worksheet_range()` は bounding rectangle
/// (最初と最後の populated セルで決まる矩形) 全体を dense `Range` として
/// materialize する (`Range::from_sparse` が `vec![Data::default(); rows *
/// cols]` を eager allocate する)。sparse な sheet (例: A1 と
/// XFD1048576 のみ populate) では、ファイル自体は小さいのに rows × cols
/// 個の `Data::Empty` を allocate しようとして OOM し得る。解凍 preflight
/// (展開量の累計検査) では防げない — 展開されるバイト数は小さいまま
/// dimension だけが巨大なため。
///
/// `Xlsx::worksheet_cells_reader` (XML イベント駆動の streaming reader、
/// dense Range を作らない) に切り替え、実際に XML 上に `<c>` として存在
/// するセルだけを到着順に読む。**存在するセルだけを `\t` で連結する
/// (位置忠実な padding はしない)**: 検索用途のテキスト抽出であり、セル間の
/// gap を空文字で埋める必要はないため。行が変わったら直前の行を確定して
/// `\n` を付けて `text` に push し、その時点で `sheet_max_bytes` を超えて
/// いれば warn + 読み込み打ち切り (既存の「行単位」truncate 意味論 —
/// 超過を招いた行は保持、次の行には進まない、巨大 1 行も丸ごと残す — を
/// 維持する)。
fn extract_xlsx_sheet_text_streaming(
    xlsx: &mut calamine::Xlsx<Cursor<&[u8]>>,
    name: &str,
    path_hint: &str,
    sheet_max_bytes: usize,
) -> Option<String> {
    let mut reader = xlsx.worksheet_cells_reader(name).ok()?;
    let mut text = String::new();
    let mut row_cells: Vec<String> = Vec::new();
    let mut current_row: Option<u32> = None;

    loop {
        let cell = match reader.next_cell() {
            Ok(Some(c)) => c,
            Ok(None) => {
                if current_row.is_some() {
                    flush_xlsx_row(&mut text, &row_cells);
                }
                break;
            }
            Err(_) => return None,
        };
        let (row, _col) = cell.get_position();
        if current_row != Some(row) {
            if current_row.is_some() {
                flush_xlsx_row(&mut text, &row_cells);
                row_cells.clear();
                if text.len() > sheet_max_bytes {
                    eprintln!(
                        "{path_hint}: sheet {name:?} exceeds {sheet_max_bytes} bytes; truncating"
                    );
                    return Some(text);
                }
            }
            current_row = Some(row);
        }
        let data: Data = cell.get_value().clone().into();
        row_cells.push(match data {
            Data::Empty => String::new(),
            other => other.to_string(),
        });
    }
    Some(text)
}

/// `row_cells` (存在したセルだけ) を `\t` join して 1 行として `text` に
/// 追記する (改行込み)。
fn flush_xlsx_row(text: &mut String, row_cells: &[String]) {
    text.push_str(&row_cells.join("\t"));
    text.push('\n');
}

/// xls (BIFF、非 zip container) の抽出本体。
///
/// codex P1 (PR #70 round 6): xls には streaming cell reader が無く (calamine
/// は open 時に workbook 全体を eager parse する)、BIFF フォーマット自体が
/// 65536 行 × 256 列 (Excel 97-2003 の仕様上限) に有界なため、dense Range
/// を作っても xlsx のような無制限 OOM のリスクが無い。よって xlsx のみ
/// streaming 化し、xls は従来通り `worksheet_range()` (dense Range) を使う。
fn parse_xls_bytes_capped(
    bytes: &[u8],
    path_hint: &str,
    sheet_max_bytes: usize,
) -> Result<ParsedDocument> {
    let cursor = Cursor::new(bytes);
    let mut xls = calamine::Xls::new(cursor)
        .map_err(|e| anyhow!("{path_hint}: cannot open workbook (encrypted or corrupt): {e}"))?;

    // xls (BIFF) には core.xml が無いため常にファイル名 fallback。
    let frontmatter = xlsx_frontmatter(bytes, path_hint);
    let title = frontmatter.title.as_deref().unwrap_or("");
    let mut chunks = Vec::new();
    for name in xls.sheet_names() {
        let range = match xls.worksheet_range(&name) {
            Ok(r) => r,
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
            if text.len() > sheet_max_bytes {
                eprintln!(
                    "{path_hint}: sheet {name:?} exceeds {sheet_max_bytes} bytes; truncating"
                );
                break;
            }
        }
        drop(range);
        if text.trim().is_empty() {
            continue;
        }
        let heading = format!("Sheet: {name}");
        let context = super::build_context(&[title, &heading]);
        chunks.push(Chunk {
            index: chunks.len(),
            heading: Some(heading),
            level: Some(2),
            content: text.trim_end().to_string(),
            context,
        });
    }

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

/// アーカイブ内の **全 entry** について展開後サイズを累計し、`cap` を超えて
/// いれば calamine を呼ぶ前に `Err` にする。
///
/// 検査は 2 層 (full-audit 2026-07-26 AU-20):
///
/// 1. **申告値** (`ZipFile::size()`) が残り budget を超えていれば、展開を
///    試みるまでもなく拒否する。正直な巨大アーカイブはここで即断できる。
/// 2. **実際に展開して数える**。zip 仕様は申告 uncompressed size を強制せず、
///    CRC 検証は全展開後にしか行われない。zip 8.6 の deflate 経路も
///    `DeflateDecoder::new(reader)` を申告値で bound しておらず
///    (`uncompressed_size` は lzma / legacy 方式にしか渡らない)、実測でも
///    **申告 10 バイトのエントリから 100 MB が読み出せた**。したがって層 1
///    だけでは「申告を小さく偽装した zip-bomb」を通してしまう (実測: 101 KB
///    の crafted xlsx が pre-flight を素通りし、calamine が 13 秒かけて
///    100 MB を展開した)。展開結果は [`std::io::sink`] 相当に捨てながら
///    **残り budget + 1 バイトで打ち切る**ので、preflight 自体のメモリは
///    固定バッファ分、CPU は最大でも cap + 1 バイトの展開に収まる。
///
/// 代償として正規の xlsx でも XML パートを 1 回余分に展開する (calamine が
/// 後でもう一度展開する)。実測 (release、この機の場合): calamine が読まない
/// 11.8 MB の XML パートを持つ xlsx で `parse_bytes` が 12 µs → **5.08 ms**
/// (≈ 2.3 GB/s、この 5 ms が pre-flight のほぼ純粋なコスト)。16 MB の
/// worksheet を持つ実用サイズの xlsx では 29.6 ms → 37.4 ms。cap いっぱい
/// (50 MiB) まで展開させられた最悪ケースでも 20 ms 台で、同じ文書の
/// embedding (数百 ms 〜) に比べれば無視できる。
///
/// `zip` crate は calamine と **同一インスタンス** (`cargo tree` で
/// `zip v8.6.0` を共有、feature unification 済) なので、「我々が展開でき
/// ない圧縮方式を calamine だけが展開できる」という抜け道は無い。
///
/// **なぜ「全 entry」なのか — 対象を name で絞る方式は 3 回破られた**:
///
/// - PR #70 round 4 (codex P1): `xl/worksheets/*.xml`, `xl/sharedStrings.xml`,
///   `xl/styles.xml` という **固定パス**のみ検査していた → `_rels/.rels` の
///   officeDocument relationship で workbook 本体を `xl/` 以外に置くと対象
///   0 件で素通り (calamine は rels から xl_path を導出するので非標準 layout
///   でも普通に読める)。
/// - PR #70 round 5 (codex P1): `.xml`/`.bin` だけでは `_rels/.rels` /
///   `xl/_rels/workbook.xml.rels` が漏れる → `.rels` を追加、比較も
///   case-insensitive 化。
/// - PR #93 round 1 (codex P1): **拡張子そのものが当てにならない**。calamine
///   は worksheet を rels の `Target` で解決しており、entry name の拡張子を
///   一切見ない。`xl/worksheets/payload` のような拡張子なしパートも普通に
///   読まれる (`test_xlsx_preflight_covers_parts_without_a_known_suffix` が
///   実際に parse できることを確認している) ため、suffix allow-list は
///   そこに置かれた bomb を見逃す。
///
/// 「calamine が読むパートの集合」を name から言い当てようとするたびに穴が
/// 開いたので、**言い当てるのをやめて全 entry を数える**。「archive 全体の
/// 展開量が `cap` 以内」という、言い切れる不変条件にする。
///
/// トレードオフ: `xl/media/*` の画像など calamine が読まないパートも budget
/// に乗るため、「展開後の合計が cap を超える画像だらけの xlsx」は skip される。
/// raw 入力自体が既に `MAX_RAW_BINARY_BYTES` で頭打ちで、画像は圧縮済み
/// (= 展開してもほぼ 1:1) なので、該当するのは raw cap 付近かつ中身の大半が
/// 画像という稀なファイルに限られる。名前ベースの穴を残すより、この誤検知を
/// 受け入れる方を選ぶ (skip は理由付きの warn として出るので silent failure
/// ではない)。
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
    // 全 entry の **実展開バイト数** の累計。申告値ではない (層 2 参照)。
    let mut total: u64 = 0;
    for i in 0..zip.len() {
        // 申告 size は metadata だけで取れる (`by_index_raw` は圧縮データを
        // そのまま返す reader = 展開しない)。
        let (name, declared) = {
            let Ok(entry) = zip.by_index_raw(i) else {
                continue;
            };
            (entry.name().to_string(), entry.size())
        };

        // 層 1: 申告値だけで既に budget 超過 = 展開を試みるまでもなく拒否。
        let remaining = cap.saturating_sub(total);
        if declared > remaining {
            anyhow::bail!(
                "{path_hint}: declared entry size exceeds {cap} bytes cap \
                 (zip-bomb guard; entry {name:?} declares {declared} bytes with \
                 {remaining} bytes of budget left)"
            );
        }

        // 層 2: 申告値は信用できないので実際に展開して数える。残り budget を
        // 1 バイトでも超えたら打ち切れるよう `remaining + 1` で bound する。
        let Ok(mut entry) = zip.by_index(i) else {
            // 展開できない entry (暗号化 / 未対応の圧縮方式) は calamine でも
            // 読めないため、budget には数えず次へ進む。
            continue;
        };
        let actual = count_decompressed_bytes(&mut entry, remaining.saturating_add(1));
        total = total.saturating_add(actual);
        if total > cap {
            anyhow::bail!(
                "{path_hint}: actual decompressed size of the archive exceeds {cap} bytes cap \
                 (zip-bomb guard; entry {name:?} declared {declared} bytes but expands past \
                 the remaining budget)"
            );
        }
    }
    Ok(())
}

/// `reader` を最大 `limit` バイトまで読み進め、読めたバイト数を返す。中身は
/// 保持しない (固定バッファに読んでは捨てる) ので、メモリは `limit` に依らず
/// 一定。
///
/// 読み出しエラー (壊れた deflate stream、CRC 不一致等) は `Err` にせず、
/// **そこまでに読めたバイト数**を返して打ち切る: pre-flight の役目は
/// 「展開量が cap を超えないこと」の確認だけで、壊れたファイル自体は後段の
/// calamine が `Err` にする (= 既存の per-file skip 経路に乗る)。
fn count_decompressed_bytes(reader: &mut impl std::io::Read, limit: u64) -> u64 {
    let mut buf = [0u8; 8 * 1024];
    let mut total: u64 = 0;
    while total < limit {
        let want = std::cmp::min(buf.len() as u64, limit - total) as usize;
        match reader.read(&mut buf[..want]) {
            Ok(0) => break,
            Ok(n) => total += n as u64,
            Err(_) => break,
        }
    }
    total
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
    // AU-21: `parse_bytes` は `Parser` ではなく blanket impl の
    // `ParserExt` 側にある (実装から override させないため)。テスト本体は
    // 従来どおり `parse_bytes` を呼ぶので、trait を scope に入れるだけ。
    use crate::parser::ParserExt;
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
    fn test_xlsx_preflight_catches_worksheets_outside_xl_directory() {
        // codex P1 (PR #70 round 4): 旧実装は `xl/worksheets/*.xml` 等の
        // 固定パスのみを検査していたため、`_rels/.rels` の officeDocument
        // relationship で workbook 本体を `xl/` 以外の任意ディレクトリに
        // 配置した crafted xlsx (calamine は rels から xl_path を導出して
        // 開けるため layout 非標準でも普通に読める) では対象 0 件になり
        // budget を素通りしていた。layout 非依存 (`.xml`/`.bin` 全件) の
        // pre-flight に変更したことで、`xl/` 以外に置かれた XML でも申告
        // size を正しく合算し cap 超過を検出できることを確認する。
        let mut buf = Vec::new();
        {
            let mut zip = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
            zip.start_file("custom/sheet1.xml", SimpleFileOptions::default())
                .unwrap();
            zip.write_all(b"0123456789").unwrap(); // 10 bytes, xl/ 配下ではない
            zip.finish().unwrap();
        }
        let err = preflight_xlsx_decompression_budget_capped(&buf, "x.xlsx", 5)
            .expect_err("declared size over the injected 5-byte cap must be Err even outside xl/");
        assert!(
            err.to_string().contains("zip-bomb guard"),
            "error message should mention zip-bomb guard, got: {err}"
        );
    }

    #[test]
    fn test_xlsx_preflight_catches_oversized_rels_entry() {
        // codex P1 (PR #70 round 5): 旧実装は `.xml`/`.bin` で終わる entry
        // しか合算しなかったため、`.rels` 拡張子 (`_rels/.rels` /
        // `xl/_rels/workbook.xml.rels` 等) が漏れていた。calamine は
        // workbook open 時に rels も読むため、巨大圧縮 `.rels` は budget を
        // 素通りする。`.rels` も合算対象に含めたことで検出できることを
        // 確認する。
        let mut buf = Vec::new();
        {
            let mut zip = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
            zip.start_file("_rels/.rels", SimpleFileOptions::default())
                .unwrap();
            zip.write_all(b"0123456789").unwrap(); // 10 bytes, .rels 拡張子
            zip.finish().unwrap();
        }
        let err = preflight_xlsx_decompression_budget_capped(&buf, "x.xlsx", 5)
            .expect_err("declared .rels entry size over the injected 5-byte cap must be Err");
        assert!(
            err.to_string().contains("zip-bomb guard"),
            "error message should mention zip-bomb guard, got: {err}"
        );
    }

    #[test]
    fn test_xlsx_preflight_extension_check_is_case_insensitive() {
        // codex P1 (PR #70 round 5): 拡張子比較を case-insensitive にする
        // (`.XML` 等、通常は発生しないが zip の name フィールドは大文字小文字
        // を保証しないため保守的に拾う)。
        let mut buf = Vec::new();
        {
            let mut zip = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
            zip.start_file("XL/WORKSHEETS/SHEET1.XML", SimpleFileOptions::default())
                .unwrap();
            zip.write_all(b"0123456789").unwrap(); // 10 bytes, 大文字拡張子
            zip.finish().unwrap();
        }
        let err = preflight_xlsx_decompression_budget_capped(&buf, "x.xlsx", 5).expect_err(
            "declared size over the injected 5-byte cap must be Err for uppercase .XML too",
        );
        assert!(
            err.to_string().contains("zip-bomb guard"),
            "error message should mention zip-bomb guard, got: {err}"
        );
    }

    /// zip の申告 uncompressed size を書き換えて「申告より実体が大きい」
    /// アーカイブを作る (AU-20 の攻撃者モデルそのもの)。
    ///
    /// `ZipWriter` は常に正しい size を書くため、生成後にバイト列を直接
    /// patch するしかない。書き換える 2 箇所は zip の仕様どおり:
    /// local file header (`PK\x03\x04`) の +22、central directory header
    /// (`PK\x01\x02`) の +24 が、いずれも uncompressed size (u32 LE)。
    /// CRC は元のまま (= 正しい) にしておく — zip 仕様では CRC 検証は全展開
    /// **後** なので、展開を始める前の pre-flight は CRC を当てにできない、
    /// という前提もこの fixture が体現している。
    fn forge_declared_uncompressed_size(zip_bytes: &[u8], real: u32, fake: u32) -> Vec<u8> {
        let mut data = zip_bytes.to_vec();
        let mut patched = 0;
        for (magic, offset) in [(b"PK\x03\x04", 22usize), (b"PK\x01\x02", 24usize)] {
            let mut i = 0;
            while i + offset + 4 <= data.len() {
                if &data[i..i + 4] == magic
                    && u32::from_le_bytes(data[i + offset..i + offset + 4].try_into().unwrap())
                        == real
                {
                    data[i + offset..i + offset + 4].copy_from_slice(&fake.to_le_bytes());
                    patched += 1;
                }
                i += 1;
            }
        }
        assert_eq!(
            patched, 2,
            "expected to patch the local + central header size fields"
        );
        data
    }

    #[test]
    fn test_xlsx_preflight_rejects_entry_whose_declared_size_is_forged() {
        // AU-20: 旧 pre-flight は申告 uncompressed size の合計しか見ていな
        // かった。zip 仕様は申告値を強制せず、zip 8.6 の deflate 展開も申告値
        // で bound していない (実測: 申告 10 バイトのエントリから 100 MB が
        // 読み出せた)。したがって「申告を極小に偽装した高圧縮エントリ」は
        // pre-flight を素通りし、calamine が実際に展開してしまう。
        //
        // ここでは 64 KiB のゼロ (= 高圧縮) を書いてから申告値を 1 バイトに
        // 偽装し、cap を 1 KiB に注入する。層 1 (申告値) では 1 <= 1024 なので
        // 通ってしまい、層 2 (実展開) だけが検出できる。
        const REAL: u32 = 64 * 1024;
        let mut buf = Vec::new();
        {
            let mut zip = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
            zip.start_file("xl/worksheets/sheet1.xml", SimpleFileOptions::default())
                .unwrap();
            zip.write_all(&vec![0u8; REAL as usize]).unwrap();
            zip.finish().unwrap();
        }
        let forged = forge_declared_uncompressed_size(&buf, REAL, 1);

        // 前提の確認: 偽装後も申告値は 1 バイトに見える (= 層 1 は通過する)。
        let mut archive = zip::ZipArchive::new(Cursor::new(forged.as_slice())).unwrap();
        assert_eq!(
            archive.by_index_raw(0).unwrap().size(),
            1,
            "the forged archive must still declare 1 byte"
        );

        let err = preflight_xlsx_decompression_budget_capped(&forged, "bomb.xlsx", 1024)
            .expect_err("an entry that expands past the cap must be Err despite its declaration");
        assert!(
            err.to_string().contains("zip-bomb guard"),
            "error message should mention zip-bomb guard, got: {err}"
        );
    }

    #[test]
    fn test_xlsx_preflight_counts_actual_bytes_not_declared_ones() {
        // 層 2 の会計が「実展開バイト数」であることの確認: 実体 64 KiB を
        // 1 バイトと偽装したエントリでも、cap が実体を上回っていれば通る
        // (= 偽装の検出ではなく、あくまで展開量で判断している)。
        const REAL: u32 = 64 * 1024;
        let mut buf = Vec::new();
        {
            let mut zip = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
            zip.start_file("xl/worksheets/sheet1.xml", SimpleFileOptions::default())
                .unwrap();
            zip.write_all(&vec![0u8; REAL as usize]).unwrap();
            zip.finish().unwrap();
        }
        let forged = forge_declared_uncompressed_size(&buf, REAL, 1);
        preflight_xlsx_decompression_budget_capped(&forged, "ok.xlsx", REAL as u64)
            .expect("actual size within the cap must be Ok even when the declaration is a lie");
    }

    #[test]
    fn test_xlsx_preflight_rejects_cumulative_actual_size_over_cap() {
        // 1 エントリずつは cap 未満でも、複数の偽装エントリを積算すると
        // 超える (ooxml.rs::read_zip_entry_capped の累積 budget と同じ論点を
        // 実展開ベースの会計でも押さえる)。
        const REAL: u32 = 32 * 1024;
        let mut buf = Vec::new();
        {
            let mut zip = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
            for name in ["xl/worksheets/sheet1.xml", "xl/worksheets/sheet2.xml"] {
                zip.start_file(name, SimpleFileOptions::default()).unwrap();
                zip.write_all(&vec![0u8; REAL as usize]).unwrap();
            }
            zip.finish().unwrap();
        }
        // 2 エントリ分 (= local 2 + central 2) を patch するので個別に呼ぶ。
        let mut data = buf;
        for _ in 0..2 {
            data = {
                let mut once = data.clone();
                let mut patched = 0;
                for (magic, offset) in [(b"PK\x03\x04", 22usize), (b"PK\x01\x02", 24usize)] {
                    let mut i = 0;
                    while i + offset + 4 <= once.len() {
                        if &once[i..i + 4] == magic
                            && u32::from_le_bytes(
                                once[i + offset..i + offset + 4].try_into().unwrap(),
                            ) == REAL
                        {
                            once[i + offset..i + offset + 4].copy_from_slice(&1u32.to_le_bytes());
                            patched += 1;
                            break; // 1 回の pass で 1 フィールドだけ書き換える
                        }
                        i += 1;
                    }
                }
                assert_eq!(patched, 2, "one local + one central field per pass");
                once
            };
        }

        // 実体は 32 KiB × 2 = 64 KiB。cap を 48 KiB にすると 2 個目で超える。
        let err = preflight_xlsx_decompression_budget_capped(&data, "bomb.xlsx", 48 * 1024)
            .expect_err("cumulative actual size over the cap must be Err");
        assert!(
            err.to_string().contains("zip-bomb guard"),
            "error message should mention zip-bomb guard, got: {err}"
        );
    }

    /// worksheet パートの **zip 内 name を呼び出し側が決められる** 最小 xlsx。
    /// rels の Target も同じ名前を指すので、calamine は拡張子に関係なく
    /// この entry を worksheet として読む。
    fn make_xlsx_with_worksheet_part(part_name: &str, sheet_body: &[u8]) -> Vec<u8> {
        let mut buf = Vec::new();
        {
            let mut zip = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
            let opt = SimpleFileOptions::default();
            zip.start_file("[Content_Types].xml", opt).unwrap();
            zip.write_all(format!(
                r#"<?xml version="1.0"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/><Override PartName="/xl/{part_name}" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/></Types>"#
            ).as_bytes()).unwrap();
            zip.start_file("_rels/.rels", opt).unwrap();
            zip.write_all(br#"<?xml version="1.0"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/></Relationships>"#).unwrap();
            zip.start_file("xl/workbook.xml", opt).unwrap();
            zip.write_all(br#"<?xml version="1.0"?><workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><sheets><sheet name="S1" sheetId="1" r:id="rId1"/></sheets></workbook>"#).unwrap();
            zip.start_file("xl/_rels/workbook.xml.rels", opt).unwrap();
            zip.write_all(format!(
                r#"<?xml version="1.0"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="{part_name}"/></Relationships>"#
            ).as_bytes()).unwrap();
            zip.start_file(format!("xl/{part_name}"), opt).unwrap();
            zip.write_all(sheet_body).unwrap();
            zip.finish().unwrap();
        }
        buf
    }

    #[test]
    fn test_xlsx_preflight_covers_parts_without_a_known_suffix() {
        // codex P1 (PR #93 round 1): pre-flight は name の拡張子
        // (`.xml`/`.bin`/`.rels`) で対象を選んでいたが、**calamine は rels の
        // Target で worksheet を解決するので name の拡張子を見ない**。
        // したがって `xl/worksheets/payload` のような拡張子なしのパートは
        // 「calamine は読むが pre-flight は数えない」= zip-bomb の抜け道になる。
        const SHEET: &[u8] = br#"<?xml version="1.0"?><worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData><row r="1"><c r="A1" t="inlineStr"><is><t>hello</t></is></c></row></sheetData></worksheet>"#;

        // 前提の確認: 拡張子なしのパートでも calamine は普通に読む。
        let legit = make_xlsx_with_worksheet_part("worksheets/payload", SHEET);
        let doc = XlsxParser
            .parse_bytes(&legit, "odd-name.xlsx", &[])
            .expect("calamine resolves the worksheet through rels, not by file name");
        assert_eq!(
            doc.chunks.len(),
            1,
            "the suffix-less part must really be parsed (this is what makes the gap exploitable)"
        );

        // よって、そこに置かれた zip-bomb も pre-flight が捕まえねばならない。
        // cap は boilerplate (Content_Types / rels / workbook.xml で合計 1.3 KB
        // 程度) を確実に上回り、かつ bomb の実体 64 KiB は下回る値にする —
        // さもないと boilerplate の合計超過で Err になり、**穴を突けていない
        // のにテストが通ってしまう** (最初の版がこれで false pass した)。
        const REAL: u32 = 64 * 1024;
        const CAP: u64 = 8 * 1024;
        let bomb = make_xlsx_with_worksheet_part("worksheets/payload", &vec![0u8; REAL as usize]);
        let forged = forge_declared_uncompressed_size(&bomb, REAL, 1);
        preflight_xlsx_decompression_budget_capped(&legit, "legit.xlsx", CAP)
            .expect("the cap must be loose enough that the boilerplate parts alone pass");
        let err = preflight_xlsx_decompression_budget_capped(&forged, "bomb.xlsx", CAP)
            .expect_err("a bomb in a suffix-less part must be rejected too");
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
        // 後続行は落とす (= 行途中では切断はしない、という明示された挙動)。
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

    /// 1-based 列番号を Excel 列名 (`A`, `B`, ..., `Z`, `AA`, ...) に変換する。
    fn column_letters(mut n: usize) -> String {
        let mut s = String::new();
        while n > 0 {
            let rem = (n - 1) % 26;
            s.insert(0, (b'A' + rem as u8) as char);
            n = (n - 1) / 26;
        }
        s
    }

    /// `A1` と `(far_row, far_col)` の 2 セルだけを populate した、bounding
    /// rectangle は巨大だが実データはスカスカな xlsx を組む。
    /// `dimension` 要素も遠いセルまでの範囲で宣言する (Excel が実際に書き出す
    /// xlsx と同じ形)。
    fn make_sparse_xlsx(far_row: usize, far_col: usize) -> Vec<u8> {
        let mut buf = Vec::new();
        {
            let mut zip = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
            let opt = SimpleFileOptions::default();

            zip.start_file("[Content_Types].xml", opt).unwrap();
            zip.write_all(br#"<?xml version="1.0"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/><Override PartName="/xl/worksheets/sheet1.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/></Types>"#).unwrap();

            zip.start_file("_rels/.rels", opt).unwrap();
            zip.write_all(br#"<?xml version="1.0"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/></Relationships>"#).unwrap();

            zip.start_file("xl/workbook.xml", opt).unwrap();
            zip.write_all(br#"<?xml version="1.0"?><workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><sheets><sheet name="S" sheetId="1" r:id="rId1"/></sheets></workbook>"#).unwrap();

            zip.start_file("xl/_rels/workbook.xml.rels", opt).unwrap();
            zip.write_all(br#"<?xml version="1.0"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/></Relationships>"#).unwrap();

            let far_ref = format!("{}{far_row}", column_letters(far_col));
            let dim_ref = format!("A1:{far_ref}");
            let sheet_xml = format!(
                r#"<?xml version="1.0"?><worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><dimension ref="{dim_ref}"/><sheetData><row r="1"><c r="A1" t="inlineStr"><is><t>near</t></is></c></row><row r="{far_row}"><c r="{far_ref}" t="inlineStr"><is><t>far</t></is></c></row></sheetData></worksheet>"#
            );
            zip.start_file("xl/worksheets/sheet1.xml", opt).unwrap();
            zip.write_all(sheet_xml.as_bytes()).unwrap();

            zip.finish().unwrap();
        }
        buf
    }

    #[test]
    fn test_xlsx_context_is_title_and_sheet() {
        let bytes = make_minimal_xlsx(&[("Sales", &[&["Q1", "100"]])]);
        let doc = XlsxParser
            .parse_bytes(&bytes, "docs/book.xlsx", &[])
            .unwrap();
        // core.xml 無し → filename title ("book")
        assert_eq!(
            doc.chunks[0].context.as_deref(),
            Some("book > Sheet: Sales")
        );
    }

    #[test]
    fn test_xlsx_sparse_far_cell_does_not_materialize_dense_range() {
        // codex P1 (PR #70 round 6): sparse な xlsx (例: A1 と遠く離れた
        // 1 セルのみ populate) では、bounding rectangle 全体を dense
        // Range として materialize する `worksheet_range()` だと
        // rows × cols 個の `Data::Empty` を allocate してしまい OOM し得る
        // (解凍 preflight は素通り — ファイル自体は数百 byte しかない)。
        // streaming cell reader に切り替えたことで、実際に populate された
        // 2 セル分だけを読み、Excel の実用上限に近い座標
        // (1,048,576 行 × 16,384 列) でも即座に完走できることを確認する。
        let bytes = make_sparse_xlsx(1_000_000, 16_000);
        let doc = XlsxParser.parse_bytes(&bytes, "sparse.xlsx", &[]).unwrap();
        assert_eq!(doc.chunks.len(), 1);
        assert!(
            doc.chunks[0].content.contains("near"),
            "got: {:?}",
            doc.chunks[0].content
        );
        assert!(
            doc.chunks[0].content.contains("far"),
            "got: {:?}",
            doc.chunks[0].content
        );
    }
}
