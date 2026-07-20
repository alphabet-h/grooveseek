//! PDF (`.pdf`) parser. ページ単位チャンク + metadata frontmatter。
//! 抽出は oxidize-pdf (純 Rust, ParseResult ベース)。念のため malformed PDF の
//! panic は catch_unwind で per-file skip に正規化する (§4.5 / spec §3 #14)。

use std::cell::Cell;
use std::io::Cursor;
use std::sync::Once;

use anyhow::{Result, anyhow};
use oxidize_pdf::parser::{PdfDocument, PdfReader};

use super::{Frontmatter, ParsedDocument, Parser, single_text_chunk};

/// スキャン PDF 判定閾値: 平均 chars/page がこれ未満なら text layer 無し扱い。
const SCANNED_PDF_MIN_CHARS_PER_PAGE: usize = 50;

thread_local! {
    /// PDF 抽出中 (= `extract_pdf` の `catch_unwind` 内) は true。process-global
    /// な panic hook が「この panic はこのスレッドの PDF 抽出由来だから
    /// backtrace 出力を抑制すべきか」を判定するために読む。詳細は
    /// `install_panic_hook_once` / `SuppressPanicOutputGuard` を参照。
    static SUPPRESS_PANIC_OUTPUT: Cell<bool> = const { Cell::new(false) };
}

/// panic hook を once-install するためのフラグ。
static INSTALL_PANIC_HOOK: Once = Once::new();

/// panic hook を **一度だけ** インストールする。
///
/// 旧実装は `extract_pdf` の呼び出し毎に `take_hook`/`set_hook` で
/// process-global な hook を no-op ↔ 元の hook に swap していた。これは
/// 並行 (例: HTTP `get_document` からの複数リクエスト、embedder/DB の
/// Mutex 外) に複数スレッドが PDF を抽出すると race する (codex P2, PR #69
/// round 2): (1) 片方の抽出が終わって `set_hook(prev_hook)` で「元の
/// hook」として復元する値が、実は他方の抽出が仕込んだ no-op hook だった
/// 場合、以後プロセス全体で panic 報告が恒久的に無効化される。(2) 抽出中
/// (no-op hook が刺さっている間) は PDF 抽出と無関係な他スレッドの panic
/// まで一緒に隠れてしまう。
///
/// 新方式: wrapper hook を最初の PDF 抽出時に一度だけ install し、以後は
/// **二度と入れ替えない**。wrapper は `SUPPRESS_PANIC_OUTPUT`
/// (thread-local) を見て、抽出中の **その panic を起こしたスレッド自身**
/// の出力だけを抑制し、他スレッドの panic は常に元の hook (`prev`) に
/// そのまま委譲する。これにより複数スレッドが同時に PDF を抽出しても
/// race せず、他スレッドの panic 報告も常に生きたままになる。
fn install_panic_hook_once() {
    INSTALL_PANIC_HOOK.call_once(|| {
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let suppress = SUPPRESS_PANIC_OUTPUT.with(Cell::get);
            if !suppress {
                prev(info);
            }
        }));
    });
}

/// `SUPPRESS_PANIC_OUTPUT` (thread-local) を true にする RAII guard。
/// Drop で必ず false に戻す — `catch_unwind` 内で実際に panic して unwind
/// される場合も Drop は呼ばれるため、"panic した後ずっと抑制されたまま"
/// になることはない (`test_suppress_panic_output_guard_resets_flag_even_on_panic`
/// で保証)。
struct SuppressPanicOutputGuard;

impl SuppressPanicOutputGuard {
    fn new() -> Self {
        SUPPRESS_PANIC_OUTPUT.with(|flag| flag.set(true));
        Self
    }
}

impl Drop for SuppressPanicOutputGuard {
    fn drop(&mut self) {
        SUPPRESS_PANIC_OUTPUT.with(|flag| flag.set(false));
    }
}

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

    fn parse_bytes(
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
/// `PdfReader::new(Cursor)` + `PdfDocument::extract_text` + `metadata` の一連を
/// `catch_unwind` でラップし、malformed PDF の panic を per-file Err に正規化する
/// (spec §3 #14: dry-run の 4 標本では panic しなかったが、未知 PDF / 依存 crate
/// 由来の panic に対する保険として catch_unwind + hook 抑止を維持する)。default
/// panic hook の生 backtrace 出力は `install_panic_hook_once` が一度だけ
/// install する wrapper hook + `SuppressPanicOutputGuard` (thread-local flag)
/// で抑止する。process-global な hook を毎回 swap していた旧実装は並行
/// PDF 抽出で race したため撤廃した (codex P2, PR #69 round 2。詳細は
/// `install_panic_hook_once` のコメント参照) — 抽出処理自体は逐次でも
/// 並行でも安全に動く。
///
/// oxidize-pdf は `ParseResult` ベースのエラー設計なので、open / extract 失敗 (暗号化
/// PDF 等) は panic ではなく `Err` として返る (dry-run で確認、docs.rs 4.1.1)。
fn extract_pdf(bytes: &[u8], path_hint: &str) -> Result<(Vec<String>, Frontmatter)> {
    install_panic_hook_once();
    let suppress_guard = SuppressPanicOutputGuard::new();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(
        || -> Result<(Vec<String>, Frontmatter)> {
            // Cursor<&[u8]> は Read + Seek を満たす = in-memory 読み
            // (PdfReader::new(reader: R) where R: Read + Seek、docs.rs 4.1.1 で確認)。
            let reader = PdfReader::new(Cursor::new(bytes)).map_err(|e| {
                anyhow!("{path_hint}: cannot open PDF (encrypted or unreadable): {e}")
            })?;
            let document = PdfDocument::new(reader);
            // extract_text() -> ParseResult<Vec<ExtractedText>>、各 .text がページ本文。
            let extracted = document.extract_text().map_err(|e| {
                anyhow!(
                    "{path_hint}: PDF text extraction failed (possibly encrypted or unreadable): {e}"
                )
            })?;
            let pages: Vec<String> = extracted.into_iter().map(|t| t.text).collect();
            let frontmatter = pdf_metadata_frontmatter(&document, path_hint);
            Ok((pages, frontmatter))
        },
    ));
    drop(suppress_guard); // 抽出終了 (成功/panic 問わず) 後は即座に抑制解除

    match result {
        Ok(inner) => inner,
        Err(_) => Err(anyhow!(
            "{path_hint}: PDF extraction panicked (malformed PDF)"
        )),
    }
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

    // Task 2.7 で正式化 (生成手順の doc 化含む) する最小 2 ページ PDF。
    // ページ 1="Hello World"、ページ 2="Second Page"。xref オフセット込みで
    // 手組みした最小構成 (Info dict に Title/CreationDate も含む)。
    const MINIMAL_PDF: &[u8] = include_bytes!("../../tests/fixtures/binary/minimal.pdf");

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
