//! PDF (`.pdf`) parser. ページ単位チャンク + metadata frontmatter。
//! 抽出は oxidize-pdf (純 Rust, ParseResult ベース)。念のため malformed PDF の
//! panic は catch_unwind で per-file skip に正規化する (§4.5 / spec §3 #14)。

use std::io::Cursor;

use anyhow::{Result, anyhow};
use oxidize_pdf::parser::{PdfDocument, PdfReader};

use super::{Frontmatter, ParsedDocument, Parser, single_text_chunk};

/// スキャン PDF 判定閾値: 平均 chars/page がこれ未満なら text layer 無し扱い。
const SCANNED_PDF_MIN_CHARS_PER_PAGE: usize = 50;

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

        // スキャン PDF 判定: 総抽出文字数 / ページ数 < 50 なら text layer 無しとみなす。
        let total_chars: usize = pages.iter().map(|p| p.chars().count()).sum();
        if !pages.is_empty() && total_chars / pages.len() < SCANNED_PDF_MIN_CHARS_PER_PAGE {
            return Err(anyhow!(
                "{path_hint}: PDF appears to have no text layer (scanned image PDF); \
                 average {} chars/page < {} threshold — skipping (OCR not supported)",
                total_chars / pages.len(),
                SCANNED_PDF_MIN_CHARS_PER_PAGE
            ));
        }

        let mut chunks = Vec::new();
        for (i, page_text) in pages.iter().enumerate() {
            let content = post_process(page_text);
            if content.trim().is_empty() {
                continue; // 空ページは chunk を作らない
            }
            chunks.push(super::Chunk {
                index: chunks.len(),
                heading: Some(format!("p.{}", i + 1)),
                level: None,
                content,
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
/// panic hook の生 backtrace は前後で hook を swap して抑止する (indexer は逐次実行
/// なので global swap は安全。並列化する場合は要再設計)。
///
/// oxidize-pdf は `ParseResult` ベースのエラー設計なので、open / extract 失敗 (暗号化
/// PDF 等) は panic ではなく `Err` として返る (dry-run で確認、docs.rs 4.1.1)。
fn extract_pdf(bytes: &[u8], path_hint: &str) -> Result<(Vec<String>, Frontmatter)> {
    let prev_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
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
    std::panic::set_hook(prev_hook);

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
        if let Some(t) = meta.title.as_deref()
            && !t.trim().is_empty()
        {
            fm.title = Some(t.trim().to_string());
        }
        fm.date = meta.creation_date.as_deref().and_then(normalize_pdf_date);
    }
    if fm.title.as_deref().map(str::is_empty).unwrap_or(true) {
        fm.title = super::txt::derive_title_pub(path_hint);
    }
    fm
}

/// PDF の日付文字列から `YYYY-MM-DD` を取り出す。oxidize-pdf が `creation_date` を
/// どの形式で返すか (raw `D:YYYYMMDD...` / bare `YYYYMMDD` / ISO `YYYY-MM-DD...`) は
/// PDF 依存なので、3 形式すべてを許容する best-effort パーサとする。
fn normalize_pdf_date(raw: &str) -> Option<String> {
    let s = raw.trim();
    let s = s.strip_prefix("D:").unwrap_or(s);
    // (1) 先頭 8 桁が数字 = PDF `D:YYYYMMDD...` / bare `YYYYMMDD`。
    if s.len() >= 8 && s.as_bytes()[..8].iter().all(u8::is_ascii_digit) {
        return Some(format!("{}-{}-{}", &s[0..4], &s[4..6], &s[6..8]));
    }
    // (2) ISO `YYYY-MM-DD...` 形式ならその先頭 10 文字。
    if s.len() >= 10 && s.as_bytes()[4] == b'-' && s.as_bytes()[7] == b'-' {
        return Some(s[..10].to_string());
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
    fn test_pdf_frontmatter_falls_back_to_filename() {
        // metadata の title が無い untitled.pdf は filename 由来 title に fallback。
        let doc = PdfParser
            .parse_bytes(UNTITLED_PDF, "docs/untitled.pdf", &[])
            .expect("untitled pdf must extract");
        assert_eq!(doc.frontmatter.title.as_deref(), Some("untitled"));
    }
}
