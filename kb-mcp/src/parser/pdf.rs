//! PDF (`.pdf`) parser. ページ単位チャンク + metadata frontmatter。
//! 抽出は oxidize-pdf (純 Rust, ParseResult ベース)。念のため malformed PDF の
//! panic は per-file skip に正規化する (§4.5 / spec §3 #14) — その機構は
//! full-audit 2026-07-26 AU-21 で docx/xlsx/pptx と共有するため
//! `parser::panic_guard` + `Parser::parse_bytes` へ移設した。このモジュールは
//! 個別の `catch_unwind` を持たない。

use std::io::Cursor;

use anyhow::{Result, anyhow};
use oxidize_pdf::parser::{PdfDocument, PdfReader};

use super::{Frontmatter, ParsedDocument, Parser, single_text_chunk};

/// スキャン PDF 判定閾値: 平均 chars/page がこれ未満なら text layer 無し扱い。
const SCANNED_PDF_MIN_CHARS_PER_PAGE: usize = 50;

/// 1 ページから取り出す text の上限 (AU-05)。
///
/// oxidize-pdf 4.1.1 は `ExtractionOptions::max_extracted_bytes` として
/// **この上限を持っているが既定は `None` (無制限)** で、kb-mcp は使って
/// いなかった。crate 側の doc より:
///
/// > The limit is enforced *during* accumulation, not by truncating the
/// > finished string, so a single page with a huge or adversarially inflated
/// > content stream cannot materialise an unbounded `String`.
///
/// 値を大きめに取るのは crate の truncation 単位が「デコード済み run」
/// だからで、budget より大きい run が 1 本あるページは `text == ""` +
/// `truncated == true` で返る (部分文字列にはならない)。切り詰めが正規の
/// ページを丸ごと空にしないよう、実データ (密なページで数 KB) の 2 桁上を取る。
const PDF_PAGE_TEXT_MAX_BYTES: usize = 1024 * 1024;

/// 1 文書から取り出す text の累積上限 (AU-05)。
///
/// crate のガードは **すべてストリーム / ページ単位**で、文書全体の累積は
/// 見ていない (`MAX_DECOMPRESSED_SIZE` 256 MB / stream、`MAX_PAGES` 100,000)。
/// per-page 上限だけでは 100,000 ページ × 1 MiB まで積める。OOXML 側で
/// PR #70 round 2 が塞いだ「per-entry はあるが累積が無い」穴と同じ形なので、
/// 同じ `MAX_RAW_BINARY_BYTES` を文書単位の budget として使う。
const PDF_DOC_TEXT_MAX_BYTES: usize = super::MAX_RAW_BINARY_BYTES as usize;

/// 1 文書の抽出にかけてよい実時間の上限 (AU-05、codex P1)。
///
/// テキスト量の budget は **メモリ** を縛るが、**展開バイト数** は縛らない。
/// 「テキストをほとんど出さない演算子に展開されるストリーム」を大量に持つ
/// PDF は、カウンタをゼロ近傍に保ったまま全ストリームを展開させられる。
/// crate 側に累積展開量の会計は無く (`MAX_DECOMPRESSED_SIZE` は stream 単位)、
/// `StackSafeContext` の timeout は抽出経路から使われていないので、
/// ここで実時間を見るしかない。
///
/// 残余の大きさは有界ではある: 入力は `MAX_RAW_BINARY_BYTES` (50 MB) で、
/// DEFLATE の理論最大比が ~1032:1 なので累積展開量は高々 ~51 GB、
/// 300 MB/s 程度の実効速度で ~170 秒。この上限はそれを 120 秒に切り下げる。
///
/// 値は crate 自身の `PARSING_TIMEOUT_SECS` (= 120、"Timeout for long-running
/// parsing operations") に合わせた。正規の PDF は 50 MB でも数秒で終わるので、
/// 遅いマシンでの false positive 余裕は十分ある。
const PDF_DOC_EXTRACT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

/// スキャン PDF 判定用の「非空ページ限定」統計を計算する。
///
/// 戻り値: `(非空ページ数, 非空ページの平均文字数)`。非空ページが 1 つも
/// 無ければ `(0, 0)` (0 除算を避けるための早期 return)。
///
/// codex P2 (PR #69 round 1): 空白 / セパレータページの多い実務 PDF
/// (レポート・スライド) では、分母を「全ページ数」にすると本文ページの
/// 密度が薄まり、本文が十分あるにも関わらず scanned 誤判定されていた。
/// 分母を「trim 後に非空だったページ」に限定することで、この誤判定を防ぐ。
fn non_empty_page_stats(pages: &[String]) -> (usize, usize) {
    let non_empty: Vec<&String> = pages.iter().filter(|p| !p.trim().is_empty()).collect();
    if non_empty.is_empty() {
        return (0, 0);
    }
    let total_chars: usize = non_empty.iter().map(|p| p.chars().count()).sum();
    (non_empty.len(), total_chars / non_empty.len())
}

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

    fn parse_bytes_inner(
        &self,
        bytes: &[u8],
        path_hint: &str,
        _exclude_headings: &[&str],
    ) -> Result<ParsedDocument> {
        let (pages, frontmatter) = extract_pdf(bytes, path_hint)?;

        // スキャン PDF 判定: 非空ページの平均 chars/page < 50 なら text layer
        // 無しとみなす (codex P2, PR #69 round 1: 空白/セパレータページの
        // 多い実務 PDF で本文ページの密度が薄まり誤判定される問題の修正 —
        // 分母を「全ページ数」ではなく「trim 後に非空だったページ数」にする)。
        let (non_empty_pages, avg_chars) = non_empty_page_stats(&pages);
        if non_empty_pages == 0 || avg_chars < SCANNED_PDF_MIN_CHARS_PER_PAGE {
            return Err(anyhow!(
                "{path_hint}: PDF appears to have no text layer (scanned image PDF); \
                 average {avg_chars} chars/page across {non_empty_pages} non-empty page(s) \
                 < {SCANNED_PDF_MIN_CHARS_PER_PAGE} threshold — skipping (OCR not supported)"
            ));
        }

        let title = frontmatter.title.as_deref().unwrap_or("");
        let mut chunks = Vec::new();
        for (i, page_text) in pages.iter().enumerate() {
            let content = post_process(page_text);
            if content.trim().is_empty() {
                continue; // 空ページは chunk を作らない
            }
            let heading = format!("p.{}", i + 1);
            let context = super::build_context(&[title, &heading]);
            chunks.push(super::Chunk {
                index: chunks.len(),
                heading: Some(heading),
                level: None,
                content,
                context,
            });
        }

        // frontmatter は extract_pdf が同じ PdfDocument から抽出済み (§4.5)。
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
}

/// oxidize-pdf でページ本文 (`Vec<String>`, 1 要素 = 1 ページ) + metadata frontmatter
/// を抽出する。
///
/// `PdfReader::new(Cursor)` + `PdfDocument::extract_text` + `metadata` の一連。
///
/// malformed PDF / 依存 crate 由来の panic は per-file の `Err` に正規化される
/// (spec §3 #14: dry-run の 4 標本では panic しなかったが、未知 PDF に対する
/// 保険)。ただし **その catch_unwind + panic 出力抑止はこの関数ではなく
/// `Parser::parse_bytes` (`parser::panic_guard`) が行う**: 同じ防御が
/// docx/xlsx/pptx にも要る (full-audit 2026-07-26 AU-21) ため、PDF 専用だった
/// 機構を trait の入口へ引き上げた。二重に包むと guard が入れ子になるだけで
/// 得るものが無いので、ここでは素直に `?` で伝播させる。
///
/// oxidize-pdf は `ParseResult` ベースのエラー設計なので、open / extract 失敗 (暗号化
/// PDF 等) は panic ではなく `Err` として返る (dry-run で確認、docs.rs 4.1.1)。
fn extract_pdf(bytes: &[u8], path_hint: &str) -> Result<(Vec<String>, Frontmatter)> {
    // Cursor<&[u8]> は Read + Seek を満たす = in-memory 読み
    // (PdfReader::new(reader: R) where R: Read + Seek、docs.rs 4.1.1 で確認)。
    let reader = PdfReader::new(Cursor::new(bytes))
        .map_err(|e| anyhow!("{path_hint}: cannot open PDF (encrypted or unreadable): {e}"))?;
    let document = PdfDocument::new(reader);
    let pages = extract_pages_within_budget(&document, path_hint)?;
    let frontmatter = pdf_metadata_frontmatter(&document, path_hint);
    Ok((pages, frontmatter))
}

/// ページ本文を per-page 上限 + 文書累積 budget 付きで取り出す (AU-05)。
///
/// `document.extract_text()` は中身が `page_count()` + `extract_from_page` の
/// 単純ループ (crate source 4.1.1 で確認) なので、ここで同じループを自分で
/// 書いても **正規の PDF に対する出力は変わらない**。違うのは 2 点だけ:
///
/// 1. `ExtractionOptions::max_extracted_bytes` を渡す (crate が持っているのに
///    既定 `None` で未使用だった per-page 上限)
/// 2. ページごとに累積バイト数を見て、文書全体の budget を超えたら `Err`
///
/// **`TextExtractor` は 1 つを使い回す**。`extract_text_from_page_with_options`
/// は呼び出しごとに extractor を作り直すため、ページ間で共有される
/// `font_object_cache` ("avoids re-parsing the same font object across pages")
/// が毎ページ捨てられて遅くなる。crate 内部の `extract_from_document` と同じく
/// extractor を保持して回す。
fn extract_pages_within_budget<R: std::io::Read + std::io::Seek>(
    document: &PdfDocument<R>,
    path_hint: &str,
) -> Result<Vec<String>> {
    extract_pages_within_budget_capped(
        document,
        path_hint,
        PDF_PAGE_TEXT_MAX_BYTES,
        PDF_DOC_TEXT_MAX_BYTES,
        PDF_DOC_EXTRACT_TIMEOUT,
    )
}

/// [`extract_pages_within_budget`] の cap 注入版。unit test が小さい cap で
/// budget 分岐を踏むために分離する (`ooxml::read_zip_entry_capped` と同じ形)。
/// 50 MB を実際に展開する fixture を用意せずに済む。
fn extract_pages_within_budget_capped<R: std::io::Read + std::io::Seek>(
    document: &PdfDocument<R>,
    path_hint: &str,
    page_cap: usize,
    doc_cap: usize,
    timeout: std::time::Duration,
) -> Result<Vec<String>> {
    use oxidize_pdf::text::{ExtractionOptions, TextExtractor};

    let page_count = document
        .page_count()
        .map_err(|e| anyhow!("{path_hint}: cannot read PDF page count: {e}"))?;
    let mut extractor = TextExtractor::with_options(ExtractionOptions {
        max_extracted_bytes: Some(page_cap),
        ..Default::default()
    });
    // page_count は文書申告値なので `with_capacity` には使わない (AU-01 と同じ、
    // 申告値から確保しない原則)。
    let mut pages: Vec<String> = Vec::new();
    let mut budget: usize = 0;
    let started = std::time::Instant::now();
    for index in 0..page_count {
        // 展開バイト数そのものは数えられないので、それに比例する実時間で縛る。
        // ページ境界でしか見ないため 1 ページ内の暴走は止められないが、
        // codex P1 の想定 (「個別には許容されるストリームを数百個」) は
        // 複数ページに跨るので、ここで頭打ちになる。
        if started.elapsed() > timeout {
            return Err(anyhow!(
                "{path_hint}: PDF text extraction exceeded {} s after {} page(s)                  (decompression-bomb guard)",
                timeout.as_secs_f64(),
                index
            ));
        }
        let extracted = extractor.extract_from_page(document, index).map_err(|e| {
            anyhow!(
                "{path_hint}: PDF text extraction failed on page {} \
                 (possibly encrypted or unreadable): {e}",
                index + 1
            )
        })?;
        if extracted.truncated {
            // 黙って落とさない (AU-13 と同じ方針)。
            eprintln!(
                "warning: {path_hint}: page {} exceeded the {page_cap}-byte per-page text \
                 limit; its text is truncated",
                index + 1
            );
        }
        budget = budget.saturating_add(extracted.text.len());
        if budget > doc_cap {
            return Err(anyhow!(
                "{path_hint}: extracted text exceeds {doc_cap} bytes across \
                 {} page(s) (decompression-bomb guard)",
                index + 1
            ));
        }
        pages.push(extracted.text);
    }
    Ok(pages)
}

/// oxidize-pdf の `DocumentMetadata` (docs.rs 4.1.1 で確認: `title` / `creation_date`
/// はいずれも `Option<String>`) から Title / CreationDate を map する。metadata が
/// 取れない / title が空なら filename fallback。どのエラーでも parse は失敗させない。
/// spec §4.5: PDF は Title と CreationDate のみ取り、他フィールドは取らない。
fn pdf_metadata_frontmatter<R: std::io::Read + std::io::Seek>(
    document: &PdfDocument<R>,
    path_hint: &str,
) -> Frontmatter {
    let mut fm = Frontmatter::default();
    if let Ok(meta) = document.metadata() {
        fm.title = meta.title.as_deref().and_then(decode_pdf_title);
        fm.date = meta.creation_date.as_deref().and_then(normalize_pdf_date);
    }
    if fm.title.as_deref().map(str::is_empty).unwrap_or(true) {
        fm.title = super::txt::derive_title_pub(path_hint);
    }
    fm
}

/// 生の `/Title` 文字列を frontmatter 用に正規化する。通常の (mis-decode
/// されていない) title はそのまま trim して返す。UTF-16BE mis-decode の
/// パターン (`recover_utf16be_title` 参照) に一致する場合は、復元に成功
/// すればその結果を、失敗すれば `None` (= 「title 無し」として扱う。呼び
/// 出し元の filename fallback に委ねる。**化けた raw text をそのまま
/// title にはしない**) を返す。
///
/// 2026-07-19 dogfood (実 Japanese PDF) で発見: `oxidize-pdf 4.1.1` は PDF
/// Info dict 文字列の UTF-16BE BOM (`0xFE 0xFF`) を検出せず、CP1252/WinAnsi
/// 風の 1 byte = 1 codepoint 変換にフォールスルーする。詳細は
/// `.dev/knowledge/feature-45-pdf-crate-dryrun.md` (git 非追跡)。
fn decode_pdf_title(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let mut chars = trimmed.chars();
    let looks_mis_decoded = chars.next() == Some('\u{FE}') && chars.next() == Some('\u{FF}');
    if looks_mis_decoded {
        return recover_utf16be_title(trimmed);
    }
    Some(trimmed.to_string())
}

/// `oxidize-pdf` が UTF-16BE PDF 文字列を mis-decode した結果から元の
/// UTF-16BE バイト列を復元し、正しくデコードし直す。
///
/// mis-decode の仕組み (dogfood で bytes レベルまで特定済み): 本来
/// `0xFE 0xFF` (UTF-16BE BOM) + 2 byte ずつの UTF-16BE code unit である
/// はずの raw bytes が、oxidize-pdf 側で BOM 判定されないまま 1 byte =
/// 1 codepoint の CP1252/WinAnsi 風テーブルで decode されてしまう。
/// `0xFE`→`þ`、`0xFF`→`ÿ` という decode 結果 (= mis-decode された BOM) を
/// 先頭マーカーとして検知し、`cp1252_char_to_byte` で逆変換して元の
/// バイト列を復元、2 byte ずつ UTF-16BE code unit として組み直す。
///
/// `garbled` が mis-decode パターン (先頭が `'\u{FE}\u{FF}'`) で始まらない
/// 場合や、復元の途中で失敗した場合 (奇数バイト、CP1252 テーブル外の
/// 文字、不正な UTF-16 サロゲート、復元後も制御文字しか無い) は `None`。
fn recover_utf16be_title(garbled: &str) -> Option<String> {
    let mut chars = garbled.chars();
    if chars.next() != Some('\u{FE}') || chars.next() != Some('\u{FF}') {
        return None;
    }
    let bytes: Vec<u8> = garbled
        .chars()
        .skip(2)
        .map(cp1252_char_to_byte)
        .collect::<Option<Vec<u8>>>()?;
    if bytes.is_empty() || !bytes.len().is_multiple_of(2) {
        return None; // 奇数バイトは UTF-16 code unit を組めない
    }
    let units: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|pair| u16::from_be_bytes([pair[0], pair[1]]))
        .collect();
    let recovered = String::from_utf16(&units).ok()?;
    if recovered.trim().is_empty() || recovered.chars().any(char::is_control) {
        return None; // 復元できても中身が制御文字だけなら title として無効
    }
    Some(recovered)
}

/// CP1252 (WinAnsi) の逆変換: mis-decode された 1 文字を元のバイト値に戻す。
/// `0x00..=0x7F` (ASCII) と `0xA0..=0xFF` (Latin-1 上位) は codepoint == byte
/// 値でそのまま戻せるが、`0x80..=0x9F` は CP1252 固有の punctuation glyph に
/// マップされているため、個別の逆引きテーブルが要る (未定義スロット
/// `0x81 / 0x8D / 0x8F / 0x90 / 0x9D` は `None`)。
fn cp1252_char_to_byte(c: char) -> Option<u8> {
    let cp = c as u32;
    match cp {
        0x00..=0x7F | 0xA0..=0xFF => Some(cp as u8),
        0x20AC => Some(0x80),
        0x201A => Some(0x82),
        0x0192 => Some(0x83),
        0x201E => Some(0x84),
        0x2026 => Some(0x85),
        0x2020 => Some(0x86),
        0x2021 => Some(0x87),
        0x02C6 => Some(0x88),
        0x2030 => Some(0x89),
        0x0160 => Some(0x8A),
        0x2039 => Some(0x8B),
        0x0152 => Some(0x8C),
        0x017D => Some(0x8E),
        0x2018 => Some(0x91),
        0x2019 => Some(0x92),
        0x201C => Some(0x93),
        0x201D => Some(0x94),
        0x2022 => Some(0x95),
        0x2013 => Some(0x96),
        0x2014 => Some(0x97),
        0x02DC => Some(0x98),
        0x2122 => Some(0x99),
        0x0161 => Some(0x9A),
        0x203A => Some(0x9B),
        0x0153 => Some(0x9C),
        0x017E => Some(0x9E),
        0x0178 => Some(0x9F),
        _ => None,
    }
}

/// PDF の日付文字列から `YYYY-MM-DD` を取り出す。oxidize-pdf が `creation_date` を
/// どの形式で返すか (raw `D:YYYYMMDD...` / bare `YYYYMMDD` / ISO `YYYY-MM-DD...`) は
/// PDF 依存なので、3 形式すべてを許容する best-effort パーサとする。
fn normalize_pdf_date(raw: &str) -> Option<String> {
    let s = raw.trim();
    let s = s.strip_prefix("D:").unwrap_or(s);
    // (1) 先頭 8 桁が数字 = PDF `D:YYYYMMDD...` / bare `YYYYMMDD`。8 byte
    // 全てが ASCII digit だと確認済みなので、その内側の &s[0..4] 等の
    // スライスは常に char 境界上にあり panic しない。
    if s.len() >= 8 && s.as_bytes()[..8].iter().all(u8::is_ascii_digit) {
        return Some(format!("{}-{}-{}", &s[0..4], &s[4..6], &s[6..8]));
    }
    // (2) ISO `YYYY-MM-DD...` 形式ならその先頭 10 文字。旧実装の `s[..10]`
    // は byte 境界チェック無しの panic-prone slice で、CreationDate に
    // multibyte 文字が混入し (例: "2026-07-あ...") byte offset 10 がその
    // 文字の内側に来ると "byte index 10 is not a char boundary" で panic
    // していた。panic は catch_unwind に拾われ、本来抽出できたはずの文書
    // 全体が「PDF extraction panicked」として丸ごと skip される事故に
    // つながる (codex P2, PR #69 round 3)。`s.get(..10)` で境界外/境界
    // 不一致を安全に `None` 化し、さらに切り出した 10 byte が全て ASCII
    // digit か `-` であることも検証して意味のある日付部分だけを受理する。
    if s.len() >= 10
        && s.as_bytes()[4] == b'-'
        && s.as_bytes()[7] == b'-'
        && let Some(candidate) = s.get(..10)
        && candidate.bytes().all(|b| b.is_ascii_digit() || b == b'-')
    {
        return Some(candidate.to_string());
    }
    None
}

/// ページ抽出テキストの後処理: (1) 行末ハイフン結合 `-\n` → 連結、
/// (2) よく使われるリガチャ (ﬁ ﬂ ﬀ ﬃ ﬄ) を ASCII 展開。
fn post_process(page: &str) -> String {
    // (1) 行末ハイフネーション結合。無条件結合は日本語文書中の型番/日付等
    //     (例: "型番ABC-\n123") のハイフンを誤って消してしまうため、
    //     "-" の直前と "\n" の直後がともに ASCII 小文字 (a-z) の場合
    //     (= 英単語がハイフネーションで分断されたと推定できる場合) に限定する。
    //     それ以外 (大文字・数字・CJK 隣接等) は "-\n" をそのまま残す。
    let chars: Vec<char> = page.chars().collect();
    let mut dehyphenated = String::with_capacity(page.len());
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '-' && chars.get(i + 1) == Some(&'\n') {
            let prev_lower = i > 0 && chars[i - 1].is_ascii_lowercase();
            let next_lower = chars.get(i + 2).is_some_and(char::is_ascii_lowercase);
            if prev_lower && next_lower {
                // "-\n" をまとめて読み飛ばし、両側の単語を連結する。
                i += 2;
                continue;
            }
        }
        dehyphenated.push(chars[i]);
        i += 1;
    }
    // (2) リガチャ正規化 (NFKC の代表 subset を明示展開; 全 NFKC は過剰変換の
    //     恐れがあるため必要な合字だけ扱う)。
    dehyphenated
        .replace('\u{fb00}', "ff")
        .replace('\u{fb01}', "fi")
        .replace('\u{fb02}', "fl")
        .replace('\u{fb03}', "ffi")
        .replace('\u{fb04}', "ffl")
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    // panic 抑止 guard の RAII 不変条件を検証するテスト
    // (`test_suppress_panic_output_guard_*`) は、この機構が PDF 専用だった
    // 頃からここに置かれている。実装は AU-21 で `parser::panic_guard` へ
    // 移設したが、テストは移動せずそのまま維持する (import だけ足す)。
    use crate::parser::panic_guard::{SUPPRESS_PANIC_OUTPUT, SuppressPanicOutputGuard};
    use std::cell::Cell;
    // AU-21: `parse_bytes` は `Parser` ではなく blanket impl の `ParserExt`
    // 側にある (実装から override させないため)。テスト本体は従来どおり
    // `parse_bytes` を呼ぶので、trait を scope に入れるだけ。
    use crate::parser::ParserExt;

    // Task 2.7 で正式化 (生成手順の doc 化含む) する最小 2 ページ PDF。
    // ページ 1="Hello World"、ページ 2="Second Page"。xref オフセット込みで
    // 手組みした最小構成 (Info dict に Title/CreationDate も含む)。
    const MINIMAL_PDF: &[u8] = include_bytes!("../../tests/fixtures/binary/minimal.pdf");

    // ---- AU-05: 展開 budget ----

    fn open_fixture(bytes: &'static [u8]) -> PdfDocument<Cursor<&'static [u8]>> {
        PdfDocument::new(PdfReader::new(Cursor::new(bytes)).unwrap())
    }

    /// 上限を入れても正規 PDF の抽出結果は変わらないこと。
    ///
    /// crate の `extract_text()` は `page_count()` + `extract_from_page` の
    /// 素のループなので (source 4.1.1 で確認)、自前ループの出力と
    /// **バイト単位で一致する**はず。これが崩れたら「budget を足しただけ」
    /// という前提が壊れている。
    #[test]
    fn budgeted_extraction_matches_the_crate_for_a_normal_pdf() {
        for fixture in [MINIMAL_PDF, UNTITLED_PDF, MOSTLY_BLANK_PDF] {
            let doc = open_fixture(fixture);
            let expected: Vec<String> = doc
                .extract_text()
                .unwrap()
                .into_iter()
                .map(|t| t.text)
                .collect();
            let actual = extract_pages_within_budget(&doc, "fixture.pdf").unwrap();
            assert_eq!(actual, expected, "budgeting changed the extracted text");
        }
    }

    /// 文書累積 budget を超えたら `Err`。crate 側のガードはすべて
    /// ストリーム / ページ単位で、累積は見ていない。
    #[test]
    fn the_document_budget_stops_extraction() {
        let doc = open_fixture(MINIMAL_PDF);
        // 正規の抽出量を測ってから、その 1 バイト下を cap にする。
        let full: usize = extract_pages_within_budget(&doc, "fixture.pdf")
            .unwrap()
            .iter()
            .map(String::len)
            .sum();
        assert!(full > 0, "fixture should extract some text");

        let err = extract_pages_within_budget_capped(
            &doc,
            "bomb.pdf",
            PDF_PAGE_TEXT_MAX_BYTES,
            full - 1,
            PDF_DOC_EXTRACT_TIMEOUT,
        )
        .expect_err("cumulative budget should have refused this");
        let msg = err.to_string();
        assert!(msg.contains("bomb.pdf"), "should name the file: {msg}");
        assert!(
            msg.contains("decompression-bomb guard"),
            "should say why: {msg}"
        );

        // ちょうど上限なら通る (off-by-one で正規ファイルを落とさない)。
        assert!(
            extract_pages_within_budget_capped(
                &doc,
                "ok.pdf",
                PDF_PAGE_TEXT_MAX_BYTES,
                full,
                PDF_DOC_EXTRACT_TIMEOUT,
            )
            .is_ok()
        );
    }

    /// AU-05 (codex P1): テキスト量の budget は **メモリ** しか縛らない。
    /// テキストをほとんど出さない演算子に展開されるストリームを大量に持つ
    /// PDF は、カウンタをゼロ近傍に保ったまま全ストリームを展開させられる。
    /// 実時間でも縛っていることを確かめる。
    #[test]
    fn the_time_budget_stops_extraction_independently_of_text_volume() {
        let doc = open_fixture(MINIMAL_PDF);
        // 0 秒 = 最初のページ境界で必ず超過。テキスト量の budget は上限
        // いっぱいに開けてあるので、止めたのが時間側だと確定できる。
        let err = extract_pages_within_budget_capped(
            &doc,
            "slow.pdf",
            PDF_PAGE_TEXT_MAX_BYTES,
            PDF_DOC_TEXT_MAX_BYTES,
            std::time::Duration::ZERO,
        )
        .expect_err("a zero time budget should have refused this");
        let msg = err.to_string();
        assert!(msg.contains("slow.pdf"), "should name the file: {msg}");
        assert!(
            msg.contains("exceeded") && msg.contains("decompression-bomb guard"),
            "should say why: {msg}"
        );
    }

    /// per-page 上限は crate が持っていた (`max_extracted_bytes`) が既定 `None`
    /// で未使用だった。渡していることを、極小 cap で `truncated` が立つことで
    /// 確かめる。
    #[test]
    fn the_per_page_limit_is_actually_passed_to_the_crate() {
        use oxidize_pdf::text::{ExtractionOptions, TextExtractor};
        let doc = open_fixture(MINIMAL_PDF);
        let mut extractor = TextExtractor::with_options(ExtractionOptions {
            max_extracted_bytes: Some(1),
            ..Default::default()
        });
        let extracted = extractor.extract_from_page(&doc, 0).unwrap();
        assert!(
            extracted.truncated,
            "a 1-byte cap should have truncated this page"
        );
        // 上限内に収まる cap では truncated が立たないこと (対称の確認)。
        let mut extractor = TextExtractor::with_options(ExtractionOptions {
            max_extracted_bytes: Some(PDF_PAGE_TEXT_MAX_BYTES),
            ..Default::default()
        });
        assert!(!extractor.extract_from_page(&doc, 0).unwrap().truncated);
    }

    // filename title fallback 専用の 1 ページ PDF。minimal.pdf と同じ手組み手法
    // (xref オフセット込み) で生成、Info dict は CreationDate のみで /Title を
    // 意図的に含まない (Task 2.6: minimal.pdf は Task 2.3 test の前提として
    // /Title 入りのまま維持するため、fallback 検証は本 fixture に分離した)。
    // 本文はスキャン PDF 判定閾値 (50 chars/page) を超えるパディング入り。
    const UNTITLED_PDF: &[u8] = include_bytes!("../../tests/fixtures/binary/untitled.pdf");

    // Task 2.9 follow-up (2026-07-19): 1 ページ PDF、Info dict の /Title を
    // UTF-16BE literal PDF string (`(...)`、BOM 込み raw bytes、"日本語" を
    // エンコード) にした fixture。実 PDF writer が非 ASCII タイトルを書く際と
    // 同じ表現形式で、dogfood で発見した oxidize-pdf 4.1.1 の mis-decode
    // (UTF-16BE BOM 未検出 → CP1252/WinAnsi 風の 1 byte = 1 codepoint 変換に
    // フォールスルー) を再現する。生成手順・hex string では再現しなかった
    // 罠は tests/fixtures/binary/README.md 参照。
    const UTF16_TITLE_PDF: &[u8] = include_bytes!("../../tests/fixtures/binary/utf16_title.pdf");

    // codex P2 follow-up (PR #69 round 1, 2026-07-19): 10 ページ PDF、9 ページ
    // が空 (/Contents /Length 0、empty_text.pdf と同じ手法)、5 ページ目のみ
    // 221 文字の実テキストを持つ。旧ロジック (全ページ数で割る) だと
    // 221/10=22 < 50 で scanned 誤判定になるが、新ロジック (非空ページ数で
    // 割る) では 221/1=221 >= 50 で正しく scanned とは判定されないことを
    // 確認する fixture。生成手順: tests/fixtures/binary/README.md 参照。
    const MOSTLY_BLANK_PDF: &[u8] = include_bytes!("../../tests/fixtures/binary/mostly_blank.pdf");

    #[test]
    fn test_pdf_page_chunks_have_heading_and_no_level() {
        let doc = PdfParser
            .parse_bytes(MINIMAL_PDF, "docs/minimal.pdf", &[])
            .expect("minimal pdf must extract");
        assert_eq!(doc.chunks.len(), 2, "one chunk per non-empty page");
        assert_eq!(doc.chunks[0].heading.as_deref(), Some("p.1"));
        assert_eq!(doc.chunks[1].heading.as_deref(), Some("p.2"));
        assert!(doc.chunks[0].level.is_none());
        assert!(doc.chunks[0].content.contains("Hello"));
        assert!(doc.chunks[1].content.contains("Second"));
    }

    #[test]
    fn test_pdf_malformed_bytes_is_err_not_panic() {
        // 壊れた PDF は catch_unwind で Err に正規化され panic しない (edge #6)。
        let err = PdfParser
            .parse_bytes(b"%PDF-1.4 not really a pdf", "x.pdf", &[])
            .expect_err("garbage must be Err");
        let _ = err; // メッセージ内容は crate 依存なので存在のみ assert
    }

    #[test]
    fn test_pdf_scanned_no_text_layer_is_err() {
        // text object を一切含まない (Contents ストリームが空の) 1 ページ PDF。
        // minimal.pdf の生成手法を流用した手組み fixture (Task 2.7 で正式化予定)。
        const EMPTY: &[u8] = include_bytes!("../../tests/fixtures/binary/empty_text.pdf");
        let err = PdfParser
            .parse_bytes(EMPTY, "scan.pdf", &[])
            .expect_err("no text layer must be Err");
        assert!(err.to_string().contains("no text layer"));
    }

    #[test]
    fn test_post_process_joins_hyphenated_linebreaks() {
        // "inter-\nnational" → "international"
        assert_eq!(post_process("inter-\nnational text"), "international text");
    }

    #[test]
    fn test_post_process_normalizes_ligatures() {
        // U+FB01 (ﬁ) → "fi"
        assert_eq!(post_process("ef\u{fb01}cient"), "efficient");
    }

    #[test]
    fn test_post_process_preserves_normal_text() {
        assert_eq!(
            post_process("normal\nmultiline\ntext"),
            "normal\nmultiline\ntext"
        );
    }

    #[test]
    fn test_post_process_preserves_hyphen_before_digits() {
        // 型番のような ASCII 数字文脈の "-\n" は結合しない (改行・ハイフンとも保持)。
        assert_eq!(post_process("型番ABC-\n123"), "型番ABC-\n123");
    }

    #[test]
    fn test_post_process_joins_lowercase_hyphenation() {
        assert_eq!(post_process("infor-\nmation"), "information");
    }

    #[test]
    fn test_post_process_preserves_hyphen_cjk_adjacent() {
        // CJK に隣接する "-\n" は結合しない (改行・ハイフンとも保持)。
        assert_eq!(post_process("日本語-\nテキスト"), "日本語-\nテキスト");
    }

    #[test]
    fn test_pdf_encrypted_is_err() {
        // このバイト列は暗号化 PDF ではなく、xref テーブルもオブジェクト構造も
        // 一切持たない (%PDF- ヘッダの直後に endobj が 2 つ並ぶだけの) 構造欠落
        // バイト列。PdfReader::new の open 失敗パスが実暗号化 PDF (下の
        // test_pdf_encrypted_real_fixture_is_err) と同じ "encrypted or unreadable"
        // 文言を返すことを、この安価な壊れバイト列でも代替検証できる (どちらの
        // 経路でも文言が共通なため)。
        let err = PdfParser
            .parse_bytes(b"%PDF-1.4\n%garbage\nendobj\nendobj\n%%EOF", "enc.pdf", &[])
            .expect_err("broken PDF open path must be Err");
        let msg = err.to_string().to_lowercase();
        assert!(msg.contains("encrypted") || msg.contains("unreadable"));
    }

    #[test]
    fn test_pdf_encrypted_real_fixture_is_err() {
        // pikepdf (AES-256 / R=6) で minimal.pdf を非空ユーザパスワード "userpw" で
        // 暗号化した実 fixture (生成手順: tests/fixtures/binary/README.md)。
        // oxidize-pdf は unlock() 未呼び出しの暗号化 PDF を text extraction 段階で
        // Err にする (dry-run で確認: "PDF is locked: call unlock() with the
        // correct password before reading objects")。
        const REAL_ENCRYPTED_PDF: &[u8] =
            include_bytes!("../../tests/fixtures/binary/encrypted.pdf");
        let err = PdfParser
            .parse_bytes(REAL_ENCRYPTED_PDF, "docs/encrypted.pdf", &[])
            .expect_err("real encrypted PDF without unlock() must be Err");
        let msg = err.to_string().to_lowercase();
        assert!(msg.contains("encrypted") || msg.contains("unreadable"));
    }

    #[test]
    fn test_normalize_pdf_date_accepts_all_forms() {
        // (1) PDF raw `D:YYYYMMDD...`、(2) bare `YYYYMMDD`、(3) ISO `YYYY-MM-DD...`。
        // oxidize-pdf が creation_date をどの形式で返すか不明なため 3 形式許容 (§4.5)。
        assert_eq!(
            normalize_pdf_date("D:20260719120000Z").as_deref(),
            Some("2026-07-19")
        );
        assert_eq!(
            normalize_pdf_date("20260719").as_deref(),
            Some("2026-07-19")
        );
        assert_eq!(
            normalize_pdf_date("2026-07-19T12:00:00Z").as_deref(),
            Some("2026-07-19")
        );
        assert_eq!(normalize_pdf_date("garbage"), None);
    }

    #[test]
    fn test_normalize_pdf_date_multibyte_at_boundary_returns_none_not_panic() {
        // codex P2 follow-up (PR #69 round 3): "あ" (U+3042) is a 3-byte
        // UTF-8 char occupying bytes 8..11, so it straddles the ISO-branch's
        // byte offset 10. The old `s[..10]` byte-range slice panicked with
        // "byte index 10 is not a char boundary" for CreationDate values
        // like this (multibyte garbage mixed into an otherwise ISO-shaped
        // date). Must return None instead of panicking — the containing
        // document should still index normally, just without a date.
        assert_eq!(normalize_pdf_date("2026-07-あ"), None);
    }

    #[test]
    fn test_pdf_frontmatter_falls_back_to_filename() {
        // metadata の title が無い untitled.pdf は filename 由来 title に fallback。
        let doc = PdfParser
            .parse_bytes(UNTITLED_PDF, "docs/untitled.pdf", &[])
            .expect("untitled pdf must extract");
        assert_eq!(doc.frontmatter.title.as_deref(), Some("untitled"));
    }

    // Task 2.9 follow-up (2026-07-19): UTF-16BE PDF Title mojibake recovery.
    // 実 fixture (oxidize-pdf を実際に通す end-to-end) + 実 dogfood サンプルを
    // 使った pure-function 単体テストの 2 系統で TDD する。

    #[test]
    fn test_pdf_recovers_utf16be_title_from_real_pdf_encoding() {
        // utf16_title.pdf の /Title は本物の PDF writer と同じ表現 (UTF-16BE
        // hex string, BOM 込み) で "日本語" をエンコードしている。oxidize-pdf
        // の mis-decode (BOM 未検出) を実際に踏んだ上で、正しく復元できるか
        // どうかを確認する end-to-end 回帰テスト。
        let doc = PdfParser
            .parse_bytes(UTF16_TITLE_PDF, "docs/utf16_title.pdf", &[])
            .expect("utf16 title pdf must extract");
        assert_eq!(doc.frontmatter.title.as_deref(), Some("日本語"));
    }

    #[test]
    fn test_recover_utf16be_title_recovers_real_world_mojibake_sample() {
        // 2026-07-19 dogfood (20220509_resources_standard_guidelines_guideline_07.pdf)
        // の search 結果で実際に観測した mojibake をそのまま fixture 化。
        // (.dev/knowledge/feature-45-pdf-crate-dryrun.md に詳細記録)
        const REAL_WORLD_MISDECODED_TITLE: &str =
            "þÿ0\u{0c}j\u{19}n–0¬0¤0É0é0¤0ó0\u{0d}x\u{14}OîŒÇe™";
        assert_eq!(
            recover_utf16be_title(REAL_WORLD_MISDECODED_TITLE).as_deref(),
            Some("「標準ガイドライン」研修資料")
        );
    }

    #[test]
    fn test_recover_utf16be_title_without_bom_marker_returns_none() {
        // 通常の (mis-decode されていない) title は BOM マーカーで始まらない
        // ので、recovery は一切手を出さない。
        assert_eq!(recover_utf16be_title("Hello World"), None);
    }

    #[test]
    fn test_recover_utf16be_title_odd_byte_count_returns_none() {
        // BOM + 1 文字 = 1 byte しかなく UTF-16 code unit を組めない。
        assert_eq!(recover_utf16be_title("þÿ0"), None);
    }

    #[test]
    fn test_recover_utf16be_title_decoded_control_chars_returns_none() {
        // BOM + NUL 2 個は技術的には valid な UTF-16 だが、復元結果が
        // 制御文字のみで title として使い物にならない。
        assert_eq!(recover_utf16be_title("þÿ\u{0}\u{0}"), None);
    }

    #[test]
    fn test_decode_pdf_title_passes_through_normal_title_unchanged() {
        assert_eq!(
            decode_pdf_title("  Hello World  ").as_deref(),
            Some("Hello World")
        );
    }

    #[test]
    fn test_decode_pdf_title_empty_returns_none() {
        assert_eq!(decode_pdf_title("   "), None);
    }

    #[test]
    fn test_decode_pdf_title_unrecoverable_garbage_returns_none_not_garbled_text() {
        // 復元不能な場合、化けた raw text をそのまま title にしてはいけない
        // (filename fallback に倒すため None を返す契約)。
        assert_eq!(decode_pdf_title("þÿ0"), None);
    }

    // codex P2 follow-up (PR #69 round 1): スキャン PDF 判定は「非空ページ」
    // を分母にすべき、という指摘の TDD。

    #[test]
    fn test_pdf_mostly_blank_pages_not_misclassified_as_scanned() {
        // 9/10 ページが空でも、実テキストを持つ 1 ページの密度 (221 chars)
        // が非空ページ基準の閾値 (50 chars/page) を超えていれば scanned
        // 扱いにしてはいけない (旧ロジックは全ページ数 10 で割るため
        // 221/10=22 < 50 となり誤って scanned Err になっていた)。
        let doc = PdfParser
            .parse_bytes(MOSTLY_BLANK_PDF, "docs/mostly_blank.pdf", &[])
            .expect("mostly-blank pdf with one dense page must not be classified as scanned");
        assert_eq!(
            doc.chunks.len(),
            1,
            "only the one non-empty page produces a chunk; blank pages are skipped as before"
        );
        assert_eq!(doc.chunks[0].heading.as_deref(), Some("p.5"));
        assert!(doc.chunks[0].content.contains("real text layer"));
    }

    #[test]
    fn test_non_empty_page_stats_ignores_blank_pages_in_average() {
        let pages = vec![
            String::new(),
            "A".repeat(200),
            String::new(),
            "B".repeat(200),
            "   \n".to_string(), // 空白のみ = 非空扱いしない
        ];
        let (non_empty_pages, avg_chars) = non_empty_page_stats(&pages);
        assert_eq!(non_empty_pages, 2);
        assert_eq!(avg_chars, 200);
    }

    #[test]
    fn test_non_empty_page_stats_all_blank_returns_zero() {
        let pages = vec![String::new(), "  ".to_string(), "\n\n".to_string()];
        assert_eq!(non_empty_page_stats(&pages), (0, 0));
    }

    #[test]
    fn test_non_empty_page_stats_no_pages_returns_zero() {
        let pages: Vec<String> = vec![];
        assert_eq!(non_empty_page_stats(&pages), (0, 0));
    }

    // codex P2 follow-up (PR #69 round 2): panic hook を process-global に
    // swap する旧実装は並行 PDF 抽出で race する。新方式 (once-installed
    // wrapper hook + thread-local suppress flag) の RAII guard 部分の TDD。

    #[test]
    fn test_suppress_panic_output_guard_sets_and_resets_flag() {
        assert!(
            !SUPPRESS_PANIC_OUTPUT.with(Cell::get),
            "flag must start false"
        );
        {
            let _guard = SuppressPanicOutputGuard::new();
            assert!(
                SUPPRESS_PANIC_OUTPUT.with(Cell::get),
                "guard construction must set the flag true"
            );
        }
        assert!(
            !SUPPRESS_PANIC_OUTPUT.with(Cell::get),
            "guard Drop must reset the flag false"
        );
    }

    #[test]
    fn test_pdf_context_is_title_and_page() {
        // minimal.pdf は /Title 入り (Task 2.3 の前提)。context = "<title> > p.1"。
        let doc = PdfParser
            .parse_bytes(MINIMAL_PDF, "docs/minimal.pdf", &[])
            .expect("minimal pdf must extract");
        let c0 = doc.chunks[0].context.as_deref().unwrap();
        assert!(c0.ends_with(" > p.1"), "got: {c0}");
    }

    #[test]
    fn test_pdf_context_falls_back_to_filename_title() {
        // untitled.pdf は /Title 無し → filename title ("untitled")
        let doc = PdfParser
            .parse_bytes(UNTITLED_PDF, "docs/untitled.pdf", &[])
            .expect("untitled pdf must extract");
        assert_eq!(doc.chunks[0].context.as_deref(), Some("untitled > p.1"));
    }

    #[test]
    fn test_suppress_panic_output_guard_resets_flag_even_on_panic() {
        // guard が生きたまま panic → unwind されても Drop は必ず走る、という
        // RAII の不変条件そのものを検証する (これが崩れると、一度 panic した
        // スレッドではそれ以降ずっと panic report が抑制されたままになる)。
        let result = std::panic::catch_unwind(|| {
            let _guard = SuppressPanicOutputGuard::new();
            panic!("boom");
        });
        assert!(result.is_err());
        assert!(
            !SUPPRESS_PANIC_OUTPUT.with(Cell::get),
            "flag must reset to false even when the guarded closure panics"
        );
    }
}
