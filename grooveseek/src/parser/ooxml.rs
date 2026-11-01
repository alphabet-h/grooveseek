//! OOXML (docx/pptx) 共通 helper モジュール。zip + quick-xml で XML パートを
//! 読むための共有ロジックを置く。parser struct は持たない。
//!
//! `docProps/core.xml` (Dublin Core) → Frontmatter マッピング + zip entry
//! 読み出しを docx/xlsx/pptx parser が共有する (xlsx: Task 3.3、docx: Task 3.4
//! で消費済み。pptx は Task 3.5 で消費予定)。

use std::io::{Cursor, Read};

use anyhow::Result;
use quick_xml::events::{BytesRef, Event};
use quick_xml::reader::Reader;

use super::{Frontmatter, ParseCause, unindexable};

/// The first 8 bytes of an OLE compound file (CFB), which is what an encrypted OOXML package is.
const CFB_MAGIC: [u8; 8] = [0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1];

/// (#660) What bytes that are not a zip are, for an OOXML extension: [`ParseCause::Encrypted`]
/// when they are a compound file (magic D0 CF 11 E0 A1 B1 1A E1) holding the UTF-16LE name
/// "EncryptedPackage", [`ParseCause::CorruptOrUnsupported`] otherwise (a legacy .doc / .xls
/// renamed, a truncated file).
///
/// The compound file is not parsed: a search for the name's bytes is enough, and the one way it
/// can be wrong (an unencrypted compound file that happens to hold those bytes) still tells the
/// user to open the file in Office again. Called only once a zip has failed to open, so a
/// document that opens pays nothing.
pub(crate) fn cause_of_unopenable_ooxml(bytes: &[u8]) -> ParseCause {
    if !bytes.starts_with(&CFB_MAGIC) {
        return ParseCause::CorruptOrUnsupported;
    }
    let name: Vec<u8> = "EncryptedPackage"
        .encode_utf16()
        .flat_map(u16::to_le_bytes)
        .collect();
    if bytes.windows(name.len()).any(|w| w == name.as_slice()) {
        ParseCause::Encrypted
    } else {
        ParseCause::CorruptOrUnsupported
    }
}

/// (#660) A compound file that [`cause_of_unopenable_ooxml`] reads as encrypted: the CFB magic,
/// zero padding, then the UTF-16LE name "EncryptedPackage". Not a valid compound file; the
/// tests of [`crate::parser::docx`], [`crate::parser::pptx`] and [`crate::parser::xlsx`]
/// share it.
#[cfg(test)]
pub(crate) fn synthetic_encrypted_cfb() -> Vec<u8> {
    let mut bytes = CFB_MAGIC.to_vec();
    bytes.resize(512, 0);
    bytes.extend("EncryptedPackage".encode_utf16().flat_map(u16::to_le_bytes));
    bytes.resize(1024, 0);
    bytes
}

/// zip 内 `name` エントリを、cap に [`super::DEFAULT_MAX_DECOMPRESSED_BYTES`] を固定して
/// 読む (test 専用)。
///
/// (feature-61) 本番の読み出しはすべて、parser が構築時に受け取った展開 budget で
/// [`read_zip_part`] を直接呼ぶ。const の既定で読む経路が本番に 1 つでも残ると、
/// `[index].max_decompressed_size` を上げてもそこで 50 MiB の判定がもう一度走るため、
/// この関数は test からしか呼べないようにしてある。
#[cfg(test)]
pub(crate) fn read_zip_entry(
    zip: &mut zip::ZipArchive<Cursor<&[u8]>>,
    name: &str,
    budget: &mut u64,
) -> Result<Option<Vec<u8>>> {
    read_zip_entry_capped(zip, name, budget, super::DEFAULT_MAX_DECOMPRESSED_BYTES)
}

/// [`read_zip_part`] の test 用入口 (signature は main のまま)。既存 test
/// (`test_read_zip_entry_capped_rejects_cumulative_budget_over_cap` 等) と feature-61 の AC6 test が
/// 小さい cap / `u64::MAX` を注入して呼ぶ。警告文の `path_hint` は `"<test>"`。
#[cfg(test)]
pub(crate) fn read_zip_entry_capped(
    zip: &mut zip::ZipArchive<Cursor<&[u8]>>,
    name: &str,
    budget: &mut u64,
    cap: u64,
) -> Result<Option<Vec<u8>>> {
    read_zip_part(zip, "<test>", name, budget, cap)
}

/// zip 内 `name` エントリを丸ごとバイト列で読む。無ければ `Ok(None)`。
///
/// `cap` は parser が構築時に受け取った展開 budget (`[index].max_decompressed_size`、
/// feature-61)。`budget` は呼び出し側が文書単位で保持する累積展開済みバイト数で、
/// これを通じて文書全体の解凍量を bound する。
///
/// (feature-61) 1 パートが `cap` を超えて `Ok(None)` になる時は、
/// [`entry_over_budget_message`] の 1 行を stderr に出す。黙って `None` を返すと、
/// document.xml が budget を超えた docx は "word/document.xml missing" としか言われず、
/// pptx の slide は何も言われずに消え、どちらも上げるべきキーが読めないため。
///
/// (2026-10-19) 読み出しが `Err` で終わったパートも `Ok(None)` で、
/// [`part_unreadable_as_declared_message`] の 1 行を stderr に出す。zip 9.0.1
/// (zip-rs/zip2#1022) 以降の zip は申告 size を超える read を `Err` にするので、申告を偽装した
/// パートはここに来る。末尾で CRC が合わない read が `Err` になるのは zip 8.6 でも同じ。
/// キーを上げても読めないので、この警告はキーを名指さない。
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
pub(crate) fn read_zip_part(
    zip: &mut zip::ZipArchive<Cursor<&[u8]>>,
    path_hint: &str,
    name: &str,
    budget: &mut u64,
    cap: u64,
) -> Result<Option<Vec<u8>>> {
    read_zip_part_or_why(zip, path_hint, name, budget, cap).map(|read| read.ok())
}

/// (#660) [`read_zip_part`] that says why a part was left out ([`PartLeftOut`]) instead of
/// `None`, for a caller whose error names the cause. The stderr lines and the `Err` are
/// [`read_zip_part`]'s, which is this with the reason dropped.
pub(crate) fn read_zip_part_or_why(
    zip: &mut zip::ZipArchive<Cursor<&[u8]>>,
    path_hint: &str,
    name: &str,
    budget: &mut u64,
    cap: u64,
) -> Result<std::result::Result<Vec<u8>, PartLeftOut>> {
    read_zip_part_within(zip, path_hint, name, budget, cap, OverDocumentBudget::Fail)
}

/// (#660) Why [`read_zip_part_within`] left a part out. Each value is decided where the read
/// decides it, so a caller naming the cause asks this rather than looking at the part again.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PartLeftOut {
    /// The archive has no entry by this name.
    Missing,
    /// The archive has an entry by this name that the zip layer cannot open (an unsupported
    /// compression method, say).
    Unopenable,
    /// The part declares more than the cap ([`entry_over_budget_message`]).
    DeclaredOverCap,
    /// The part declares no more than the cap but inflates past it
    /// ([`entry_over_budget_message`]). Only a part the document cannot do without is told
    /// apart this way; one it can do without is [`Self::PastDocumentBudget`].
    InflatedOverCap,
    /// The read failed before it completed ([`part_unreadable_as_declared_message`]).
    UnreadableAsDeclared,
    /// The part would take the document past its budget and the document can do without it
    /// ([`part_past_document_budget_message`]).
    PastDocumentBudget,
}

impl PartLeftOut {
    /// Whether raising `[index].max_decompressed_size` would let the part be read: the part
    /// was left out for its size, not because it is absent or broken.
    pub(crate) fn is_over_the_limit(self) -> bool {
        match self {
            Self::DeclaredOverCap | Self::InflatedOverCap | Self::PastDocumentBudget => true,
            Self::Missing | Self::Unopenable | Self::UnreadableAsDeclared => false,
        }
    }
}

/// (feature-62) [`read_zip_part`] for a part the document can do without (`word/styles.xml`):
/// `None` when it is missing, cannot be read (named with [`part_unreadable_as_declared_message`]
/// when its read fails), is over `cap` on its own (named with
/// [`entry_over_budget_message`]) or is larger than what is left of the document's budget
/// (named with [`part_past_document_budget_message`], which ends with `skipped_means`). It is
/// never an `Err`, so a document whose other parts fit is still read.
///
/// The part is inflated only up to what is left of the budget plus one byte, so a part that
/// declares less than it holds costs no more than that, and a part left out adds nothing to
/// `budget`. A document whose required parts fit `cap` therefore inflates at most `cap + 1`
/// bytes in all.
pub(crate) fn read_optional_zip_part(
    zip: &mut zip::ZipArchive<Cursor<&[u8]>>,
    path_hint: &str,
    name: &str,
    budget: &mut u64,
    cap: u64,
    skipped_means: &str,
) -> Option<Vec<u8>> {
    // The one `Err` [`read_zip_part_within`] returns is the total passing `cap`, which the
    // `Skip` arm never lets happen.
    read_zip_part_within(
        zip,
        path_hint,
        name,
        budget,
        cap,
        OverDocumentBudget::Skip { skipped_means },
    )
    .ok()
    .and_then(|read| read.ok())
}

/// What [`read_zip_part_within`] does with a part that would take the document past its
/// decompression budget.
#[derive(Clone, Copy)]
enum OverDocumentBudget<'a> {
    /// Fail the document ([`read_zip_part`]): the part is one it cannot do without.
    Fail,
    /// Leave the part out, say so on stderr ending with `skipped_means`, and go on
    /// ([`read_optional_zip_part`]).
    Skip { skipped_means: &'a str },
}

/// (feature-62) The one read of a zip part under a document's decompression budget, behind
/// [`read_zip_part`] and [`read_optional_zip_part`] (AGENTS.md "One question gets one
/// implementation"): they differ only in `over`. The zip-bomb layers are the ones
/// [`read_zip_part`]'s doc describes; under [`OverDocumentBudget::Skip`] the bound is what is
/// left of the budget rather than `cap`.
fn read_zip_part_within(
    zip: &mut zip::ZipArchive<Cursor<&[u8]>>,
    path_hint: &str,
    name: &str,
    budget: &mut u64,
    cap: u64,
    over: OverDocumentBudget<'_>,
) -> Result<std::result::Result<Vec<u8>, PartLeftOut>> {
    let mut file = match zip.by_name(name) {
        Ok(f) => f,
        Err(zip::result::ZipError::FileNotFound) => return Ok(Err(PartLeftOut::Missing)),
        Err(_) => return Ok(Err(PartLeftOut::Unopenable)),
    };
    if file.size() > cap {
        eprintln!("{}", entry_over_budget_message(path_hint, name, cap));
        return Ok(Err(PartLeftOut::DeclaredOverCap));
    }
    // How far this part may inflate: `cap` for a part the document cannot do without (the
    // total is checked after the read), what is left of the budget for one it can (so it
    // never takes the total past `cap`).
    let bound = match over {
        OverDocumentBudget::Fail => cap,
        OverDocumentBudget::Skip { skipped_means } => {
            let remaining = cap.saturating_sub(*budget);
            if file.size() > remaining {
                eprintln!(
                    "{}",
                    part_past_document_budget_message(path_hint, name, cap, skipped_means)
                );
                return Ok(Err(PartLeftOut::PastDocumentBudget));
            }
            remaining
        }
    };
    let mut buf = Vec::new();
    // bound+1 まで読めれば「申告 size が嘘だった (bound を実際は超えている)」と
    // 判定できる。ちょうど bound バイトのエントリは正常に許可する。
    let limit = bound.saturating_add(1);
    if (&mut file).take(limit).read_to_end(&mut buf).is_err() {
        // zip 9.0.1 (zip-rs/zip2#1022) and later fail a read that goes past the declared size,
        // and every zip version fails the read that finds a CRC mismatch at the end, so a part
        // that declares less than it holds ends here rather than at the bound below. Say so
        // instead of dropping it silently; it is the same line whether or not the document can
        // do without the part.
        eprintln!(
            "{}",
            part_unreadable_as_declared_message(path_hint, name, file.size())
        );
        return Ok(Err(PartLeftOut::UnreadableAsDeclared));
    }
    if buf.len() as u64 > bound {
        let why = match over {
            OverDocumentBudget::Fail => {
                eprintln!("{}", entry_over_budget_message(path_hint, name, cap));
                PartLeftOut::InflatedOverCap
            }
            OverDocumentBudget::Skip { skipped_means } => {
                eprintln!(
                    "{}",
                    part_past_document_budget_message(path_hint, name, cap, skipped_means)
                );
                PartLeftOut::PastDocumentBudget
            }
        };
        return Ok(Err(why));
    }
    *budget = budget.saturating_add(buf.len() as u64);
    if *budget > cap {
        return Err(unindexable(
            ParseCause::OverExtractionLimit { limit: cap },
            format!(
                "cumulative decompressed size across zip entries exceeds {cap} bytes (zip-bomb guard; raise [index].max_decompressed_size to admit this file)"
            ),
        ));
    }
    Ok(Ok(buf))
}

/// (feature-61) 1 パートが展開 budget を超えて読み飛ばされる時の警告文。stderr に出るので
/// ASCII のみ。どのキーを上げれば読まれるかを名指しする。
pub(crate) fn entry_over_budget_message(path_hint: &str, entry: &str, cap: u64) -> String {
    format!(
        "warning: {path_hint}: {entry} exceeds [index].max_decompressed_size ({cap} bytes); skipping this part"
    )
}

/// The warning for a part whose read fails before it completes: zip refuses a part that holds
/// more than it declares (9.0.1 and later, zip-rs/zip2#1022) and one whose CRC does not match
/// (every version). ASCII only. It names no key, since raising one does not make such a part
/// readable.
pub(crate) fn part_unreadable_as_declared_message(
    path_hint: &str,
    entry: &str,
    declared: u64,
) -> String {
    format!(
        "warning: {path_hint}: {entry} cannot be read as declared (it declares {declared} bytes); it holds more than it declares, or is corrupt; skipping this part"
    )
}

/// (feature-62) The warning for a part [`read_optional_zip_part`] leaves out because it would
/// take the document past its decompression budget. ASCII only, naming the same key as
/// [`entry_over_budget_message`]; `skipped_means` says what the document loses.
pub(crate) fn part_past_document_budget_message(
    path_hint: &str,
    entry: &str,
    cap: u64,
    skipped_means: &str,
) -> String {
    format!(
        "warning: {path_hint}: {entry} would take the document past [index].max_decompressed_size ({cap} bytes); skipping this part, {skipped_means}"
    )
}

/// `docProps/core.xml` があれば Frontmatter に map、無ければ filename fallback。
/// `budget` は文書単位の累積展開済みバイト数 (呼び出し側が document.xml / slides 等の
/// 読み出しと共有する)、`cap` は parser の展開 budget で、どちらも
/// [`read_zip_part`] に渡す。累積 cap 超過時は `Err` を返す (呼び出し側は文書
/// 全体の parse を諦める)。
pub(crate) fn core_xml_frontmatter(
    zip: &mut zip::ZipArchive<Cursor<&[u8]>>,
    path_hint: &str,
    budget: &mut u64,
    cap: u64,
) -> Result<Frontmatter> {
    match read_zip_part(zip, path_hint, "docProps/core.xml", budget, cap)? {
        Some(bytes) => {
            warn_if_truncated(path_hint, "docProps/core.xml", &bytes);
            Ok(parse_core_xml(&bytes, path_hint))
        }
        None => Ok(Frontmatter {
            title: super::txt::derive_title_pub(path_hint),
            ..Frontmatter::default()
        }),
    }
}

/// OOXML の XML 読み取りが途中で失敗したときの警告 (AU-13)。
///
/// quick-xml のイベントループは `Err` を受けたら打ち切るしかないが、**黙って
/// break すると「途中までの本文」が成功として返る**。壊れた docx / pptx を
/// index すると、欠けたまま検索対象になり、しかも何も言われないので気付けない。
/// 中断を Err に変えて丸ごと捨てるより、取れた分は活かして「切れている」と
/// 伝える方が実用的なので、警告に留める。
///
/// `part` は zip 内のエントリ名 (例 `word/document.xml`)。どのファイルの
/// どの部分かが分からないと、複数ドキュメントを一括 index したときに
/// 追跡できない。
fn warn_truncated_xml(path_hint: &str, part: &str, err: &impl std::fmt::Display) {
    eprintln!(
        "warning: {path_hint}: {part}: XML parse error, extracted text is truncated here: {err}"
    );
}

/// EOF 時点で閉じていない要素が残っていれば警告する (AU-13、codex P1)。
///
/// quick-xml は **完全なトークンの直後で入力が尽きた場合、エラーではなく
/// `Event::Eof` を返す**。実測:
///
/// | 切れ方 | quick-xml |
/// |---|---|
/// | タグの途中で終わる | `Err` (tag not closed) |
/// | 閉じタグが無いまま終わる | **`Ok(Event::Eof)`** |
///
/// つまり `Err` 側だけ警告すると、**最も普通の切れ方 (完全なトークンの後で
/// ファイルが途切れる) を取りこぼす**。開いたままの要素数を数えておき、
/// EOF でゼロでなければ同じように警告する。
fn warn_if_unclosed_at_eof(path_hint: &str, part: &str, open_elements: i64) {
    if open_elements > 0 {
        eprintln!(
            "warning: {path_hint}: {part}: document ended with {open_elements} element(s) still open; extracted text is truncated here"
        );
    }
}

/// XML パートを 1 度走査し、途中で切れていれば警告する (AU-13)。
///
/// **2 通りの切れ方があり、片方しか `Err` にならない** (実測):
///
/// | 切れ方 | quick-xml |
/// |---|---|
/// | タグの途中で終わる | `Err` (tag not closed) |
/// | 閉じタグが無いまま終わる | **`Ok(Event::Eof)`** |
///
/// 後者が最も普通の切れ方 (完全なトークンの後でファイルが途切れる) なので、
/// `Err` だけを見ていると取りこぼす。開いたままの要素を数えて EOF で判定する。
///
/// **抽出ループとは別に 1 パス走らせる**。抽出側の 6 つのループは形が揃って
/// おらず (`Start | Empty` の結合アーム、`if` ガード付きアーム、`End` アーム
/// 自体が無いもの)、それぞれに深さカウントを差し込むと壊しやすい。ここに
/// 集約すれば全パートで同じ判定になる。追加コストは XML 1 走査で、indexing
/// 全体では embedding が支配的なので無視できる。
pub(crate) fn warn_if_truncated(path_hint: &str, part: &str, xml: &[u8]) {
    let mut reader = Reader::from_reader(xml);
    let mut buf = Vec::new();
    let mut open_elements: i64 = 0;
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(_)) => open_elements += 1,
            Ok(Event::End(_)) => open_elements -= 1,
            Ok(Event::Eof) => break,
            Err(e) => {
                warn_truncated_xml(path_hint, part, &e);
                return;
            }
            _ => {}
        }
        buf.clear();
    }
    warn_if_unclosed_at_eof(path_hint, part, open_elements);
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
            // 切れている場合の警告は `warn_if_truncated` (呼び出し側の 1 パス) が出す。
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
/// "byte index 10 is not a char boundary" で panic していた。当時の
/// docx/xlsx/pptx parser は (PDF と違い) `catch_unwind` の外で呼ばれていた
/// ため、この panic は per-file skip に隔離されず `index` 実行全体を落とした
/// — PR-1 で確立した per-file 隔離原則への違反になる (この構造的な穴自体は
/// full-audit 2026-07-26 AU-21 で塞いだ: `Parser::parse_bytes` が全 parser の
/// panic を `Err` に正規化する。ただし panic させないに越したことはないので
/// 本 fix はそのまま維持する)。`pdf.rs::normalize_pdf_date` の
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

/// A table row as one line of a chunk body: its fields joined with a tab, a single field being
/// its text alone, or `None` when every field is empty. The docx parser (feature-64) and the
/// pptx parser (feature-65) both write a table row through this, so the two formats cannot
/// disagree about what a row looks like.
pub(crate) fn row_line(fields: &[String]) -> Option<String> {
    if fields.iter().all(|field| field.is_empty()) {
        None
    } else {
        Some(fields.join("\t"))
    }
}

/// (feature-66) A table row's line in a body as a parser wrote it: from [`Self::start`] up to,
/// not including, [`Self::end`] in bytes, and the number of its table ([`Self::table`]) in the
/// document (`.docx`) or slide (`.pptx`) as read, before [`chunk_table_rows`] numbers the tables
/// per chunk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RawRowSpan {
    pub(crate) table: usize,
    pub(crate) start: usize,
    pub(crate) end: usize,
}

/// (feature-66) Add a table row's `line` ([`row_line`]) to `body` as one more line -- after a
/// `\n` when the body has text already, the way both parsers add a paragraph -- and record where
/// it went in `spans`, under the raw number `table`. The docx parser and the pptx parser write
/// every table row through this, so a row is marked exactly where it is written.
pub(crate) fn push_row_line(
    body: &mut String,
    spans: &mut Vec<RawRowSpan>,
    table: usize,
    line: &str,
) {
    if !body.is_empty() {
        body.push('\n');
    }
    let start = body.len();
    body.push_str(line);
    spans.push(RawRowSpan {
        table,
        start,
        end: body.len(),
    });
}

/// (feature-66) The [`crate::parser::Chunk::table_rows`] of a chunk whose rows sit at `spans`:
/// in order of [`RawRowSpan::start`], each table renumbered by the order it first appears in the
/// chunk, from 0, so a table that wrote no row, or wrote its rows into another chunk, leaves no
/// gap. `None` when an offset or a number does not fit a `u32`: not computed, rather than wrong.
pub(crate) fn chunk_table_rows(spans: &[RawRowSpan]) -> Option<Vec<super::TableRowSpan>> {
    let mut ordered = spans.to_vec();
    ordered.sort_by_key(|span| span.start);
    let mut numbers: std::collections::HashMap<usize, u32> = std::collections::HashMap::new();
    let mut out = Vec::with_capacity(ordered.len());
    for span in ordered {
        let next = u32::try_from(numbers.len()).ok()?;
        let table = *numbers.entry(span.table).or_insert(next);
        out.push(super::TableRowSpan {
            table,
            start: u32::try_from(span.start).ok()?,
            end: u32::try_from(span.end).ok()?,
        });
    }
    Some(out)
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

    /// ちょうど budget の大きさの part は読める (Deflate)。zip 9.0.1 (zip-rs/zip2#1022) 以降の zip
    /// は申告 size を超える read を `Err` にするが、正常な entry は末尾で `Ok(0)` を返すので、
    /// [`read_zip_part_within`] の `take(bound + 1)` が 1 byte 余分に求めても error にならない
    /// (zip 9.0.5 の `Crc32Reader` が申告 size を理由に `Err` にするのは、申告より多い byte が
    /// 実際に出た時だけ)。
    /// [`test_read_zip_entry_accepts_entry_within_cap`] の entry は cap よりずっと小さく、この
    /// 境目を見ていない。
    ///
    /// **この test が落ちたら、zip が末尾の read を `Err` にするようになった印**で、申告どおりの
    /// part まで "cannot be read as declared" の警告とともに読み飛ばされる (docx なら本文が
    /// "word/document.xml missing" になる)。
    #[test]
    fn a_part_of_exactly_the_budget_is_read_under_zip_9() {
        a_part_of_exactly_the_budget_is_read(
            SimpleFileOptions::default(),
            zip::CompressionMethod::Deflated,
        );
    }

    /// [`a_part_of_exactly_the_budget_is_read_under_zip_9`] の Stored 版。Stored の entry は
    /// decoder を通らないので、zip 9 の reader の経路が Deflate とは別になる。
    #[test]
    fn a_stored_part_of_exactly_the_budget_is_read_under_zip_9() {
        a_part_of_exactly_the_budget_is_read(
            SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored),
            zip::CompressionMethod::Stored,
        );
    }

    /// [`a_part_of_exactly_the_budget_is_read_under_zip_9`] と
    /// [`a_stored_part_of_exactly_the_budget_is_read_under_zip_9`] の本体。`options` で 10 byte の
    /// entry と 1024 byte の entry を書いた archive で、[`read_zip_part`] は cap ちょうどの part を
    /// 読んで budget に積み、cap が 1 byte 足りなければ申告 size を見て展開せずに `None` を返す。
    /// [`read_optional_zip_part`] は残り budget ちょうどの part を読み、残りが 1 byte 足りなければ
    /// 展開せずに `None` を返して budget を変えない。
    fn a_part_of_exactly_the_budget_is_read(
        options: SimpleFileOptions,
        method: zip::CompressionMethod,
    ) {
        const FIRST: &[u8] = b"0123456789";
        const PART: u64 = 1024;
        let mut buf = Vec::new();
        {
            let mut zip = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
            zip.start_file("first.bin", options).unwrap();
            zip.write_all(FIRST).unwrap();
            zip.start_file("exact.bin", options).unwrap();
            zip.write_all(&[0u8; PART as usize]).unwrap();
            zip.finish().unwrap();
        }
        let mut archive = zip::ZipArchive::new(Cursor::new(buf.as_slice())).unwrap();
        {
            let part = archive.by_name("exact.bin").unwrap();
            assert_eq!(part.compression(), method, "premise: the part is {method}");
            assert_eq!(
                part.size(),
                PART,
                "premise: the part declares what it holds"
            );
        }
        let len = |read: Option<Vec<u8>>| read.map(|part| part.len() as u64);

        // read_zip_part: cap ちょうど (1024) の part は読まれて budget に積まれる ...
        let mut budget: u64 = 0;
        let read = read_zip_part(&mut archive, "<test>", "exact.bin", &mut budget, PART).unwrap();
        assert_eq!(
            len(read),
            Some(PART),
            "{method}: a part of exactly the cap is read"
        );
        assert_eq!(budget, PART);
        // ... cap が 1 byte 足りなければ (1023) 申告 size で拒まれ、何も積まれない。
        let mut budget: u64 = 0;
        let read =
            read_zip_part(&mut archive, "<test>", "exact.bin", &mut budget, PART - 1).unwrap();
        assert_eq!(
            len(read),
            None,
            "{method}: a part declaring 1 byte over the cap"
        );
        assert_eq!(budget, 0);

        // read_optional_zip_part: 10 byte を読んだ後、残り budget ちょうどの part (cap 1034) は
        // 読まれる ...
        let first = FIRST.len() as u64;
        let mut budget: u64 = 0;
        let cap = first + PART;
        let read = read_zip_part(&mut archive, "<test>", "first.bin", &mut budget, cap).unwrap();
        assert_eq!(read.as_deref(), Some(FIRST));
        assert_eq!(budget, first);
        let read =
            read_optional_zip_part(&mut archive, "<test>", "exact.bin", &mut budget, cap, "-");
        assert_eq!(
            len(read),
            Some(PART),
            "{method}: a part of exactly what is left is read"
        );
        assert_eq!(budget, cap);
        // ... 残りが 1 byte 足りなければ (cap 1033) 読まれず、budget は 10 のまま。
        let mut budget: u64 = 0;
        let cap = first + PART - 1;
        let read = read_zip_part(&mut archive, "<test>", "first.bin", &mut budget, cap).unwrap();
        assert_eq!(read.as_deref(), Some(FIRST));
        let read =
            read_optional_zip_part(&mut archive, "<test>", "exact.bin", &mut budget, cap, "-");
        assert_eq!(len(read), None, "{method}: a part 1 byte over what is left");
        assert_eq!(budget, first, "{method}: a part left out charges nothing");
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
    fn cfb_with_encrypted_package_is_encrypted() {
        assert_eq!(
            cause_of_unopenable_ooxml(&synthetic_encrypted_cfb()),
            ParseCause::Encrypted
        );
    }

    #[test]
    fn cfb_without_the_stream_name_is_corrupt() {
        let mut bytes = CFB_MAGIC.to_vec();
        bytes.resize(CFB_MAGIC.len() + 512, 0);
        assert_eq!(
            cause_of_unopenable_ooxml(&bytes),
            ParseCause::CorruptOrUnsupported
        );
    }

    #[test]
    fn empty_and_random_bytes_are_corrupt() {
        assert_eq!(
            cause_of_unopenable_ooxml(b""),
            ParseCause::CorruptOrUnsupported
        );
        // A fixed linear congruential sequence, so the input is the same on every run.
        let mut state: u32 = 0x1234_5678;
        let random: Vec<u8> = (0..64)
            .map(|_| {
                state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                (state >> 24) as u8
            })
            .collect();
        assert_eq!(
            cause_of_unopenable_ooxml(&random),
            ParseCause::CorruptOrUnsupported
        );
    }

    #[test]
    fn cumulative_budget_overrun_is_over_extraction_limit() {
        // Built like `test_read_zip_entry_capped_rejects_cumulative_budget_over_cap`.
        let mut buf = Vec::new();
        {
            let mut zip = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
            zip.start_file("a.bin", SimpleFileOptions::default())
                .unwrap();
            zip.write_all(b"0123456789").unwrap();
            zip.start_file("b.bin", SimpleFileOptions::default())
                .unwrap();
            zip.write_all(b"0123456789").unwrap();
            zip.finish().unwrap();
        }
        let mut archive = zip::ZipArchive::new(Cursor::new(buf.as_slice())).unwrap();
        let mut budget: u64 = 0;
        let cap: u64 = 15;
        read_zip_entry_capped(&mut archive, "a.bin", &mut budget, cap).unwrap();
        let err = read_zip_entry_capped(&mut archive, "b.bin", &mut budget, cap)
            .expect_err("20 bytes in all is over a cap of 15");
        assert_eq!(
            crate::parser::cause_of(&err),
            Some(ParseCause::OverExtractionLimit { limit: cap })
        );
        assert!(
            err.to_string()
                .contains("cumulative decompressed size across zip entries exceeds 15 bytes"),
            "{err}"
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

    /// feature-61: a part skipped for the budget is named, with the key that
    /// admits it, in one ASCII line.
    #[test]
    fn entry_over_budget_message_names_the_part_and_the_key() {
        let msg = entry_over_budget_message("docs/big.docx", "word/document.xml", 1024);
        assert_eq!(
            msg,
            "warning: docs/big.docx: word/document.xml exceeds [index].max_decompressed_size (1024 bytes); skipping this part"
        );
        assert!(msg.is_ascii());
    }

    /// feature-62 (R2.2): an optional part left out because it would take the document past
    /// its budget is named with the key and with what the document loses, in one ASCII line.
    #[test]
    fn part_past_document_budget_message_names_the_part_the_key_and_the_loss() {
        let msg = part_past_document_budget_message(
            "docs/q.docx",
            "word/styles.xml",
            4096,
            "headings fall back to style IDs",
        );
        assert_eq!(
            msg,
            "warning: docs/q.docx: word/styles.xml would take the document past [index].max_decompressed_size (4096 bytes); skipping this part, headings fall back to style IDs"
        );
        assert!(msg.is_ascii());
    }

    /// zip 9 (zip-rs/zip2#1022): a part whose read fails partway is named, with the size it
    /// declares and what is wrong with it, in one ASCII line that names no key to raise.
    #[test]
    fn part_unreadable_as_declared_message_names_the_part_and_its_declared_size() {
        let msg = part_unreadable_as_declared_message("docs/q.pptx", "ppt/slides/slide1.xml", 1);
        assert_eq!(
            msg,
            "warning: docs/q.pptx: ppt/slides/slide1.xml cannot be read as declared (it declares 1 bytes); it holds more than it declares, or is corrupt; skipping this part"
        );
        assert!(msg.is_ascii());
        assert!(!msg.contains("max_decompressed_size"), "{msg}");
    }

    /// The `forge_declared_uncompressed_size` of the xlsx tests ([`crate::parser::xlsx`]),
    /// copied rather than moved so that module's tests stay as they are: rewrites the uncompressed size the zip declares
    /// for the entry whose real size is `real`, in the local file header (+22) and the central
    /// directory header (+24), and leaves the CRC as it was.
    fn forge_declared_size(zip_bytes: &[u8], real: u32, fake: u32) -> Vec<u8> {
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

    /// feature-62 T15b (AC15 (c)): an optional part that declares less than what is left of
    /// the document's budget but holds more is left out and charges nothing; a part that fits
    /// is read and charged. The forged part is larger than what is left (1014 bytes) and no
    /// larger than the cap (1024). Under zip 8, which did not stop at the declared size when it
    /// inflated, only a read bounded by what is left refused it before it was charged: one
    /// bounded by the cap would have read the whole part, charged it, and failed the total.
    /// Since zip 9.0.1 (zip-rs/zip2#1022) zip itself fails a read that goes past the declared
    /// size, so the forged part is refused at its declared byte, before either bound is
    /// reached, and this test no longer tells the two bounds apart; what it still pins is that
    /// the part left out charges nothing.
    #[test]
    fn a_forged_optional_part_is_skipped_without_charging_the_budget() {
        const REAL: u32 = 1020;
        let mut buf = Vec::new();
        {
            let mut zip = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
            zip.start_file("first.bin", SimpleFileOptions::default())
                .unwrap();
            zip.write_all(b"0123456789").unwrap();
            zip.start_file("forged.xml", SimpleFileOptions::default())
                .unwrap();
            zip.write_all(&vec![0u8; REAL as usize]).unwrap();
            zip.start_file("fits.xml", SimpleFileOptions::default())
                .unwrap();
            zip.write_all(b"<fits/>").unwrap();
            zip.finish().unwrap();
        }
        let forged = forge_declared_size(&buf, REAL, 1);
        let mut archive = zip::ZipArchive::new(Cursor::new(forged.as_slice())).unwrap();
        assert_eq!(
            archive.by_name("forged.xml").unwrap().size(),
            1,
            "premise: the forged part declares 1 byte"
        );

        let cap: u64 = 1024;
        let mut budget: u64 = 0;
        let first = read_zip_entry_capped(&mut archive, "first.bin", &mut budget, cap).unwrap();
        assert_eq!(first, Some(b"0123456789".to_vec()));
        assert_eq!(budget, 10);
        assert!(
            u64::from(REAL) > cap - budget && u64::from(REAL) <= cap,
            "premise: the forged part is over what is left and within the cap"
        );

        let forged_part =
            read_optional_zip_part(&mut archive, "<test>", "forged.xml", &mut budget, cap, "-");
        assert_eq!(forged_part, None);
        assert_eq!(budget, 10, "a part left out charges nothing");

        let fits =
            read_optional_zip_part(&mut archive, "<test>", "fits.xml", &mut budget, cap, "-");
        assert_eq!(fits, Some(b"<fits/>".to_vec()));
        assert_eq!(budget, 17);
    }

    /// (#660) [`read_zip_part_or_why`] says why it left a part out, and only a part left out
    /// for its size counts as over the limit ([`PartLeftOut::is_over_the_limit`]): a missing
    /// part, one declaring more than the cap, and one holding more than it declares (refused by
    /// zip 9 at the declared size, before the cap) each get their own reason, and none of them
    /// charges the budget.
    #[test]
    fn read_zip_part_or_why_names_why_a_part_was_left_out() {
        const REAL: u32 = 2048;
        let mut buf = Vec::new();
        {
            let mut zip = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
            zip.start_file("big.bin", SimpleFileOptions::default())
                .unwrap();
            zip.write_all(&[0u8; 100]).unwrap();
            zip.start_file("forged.bin", SimpleFileOptions::default())
                .unwrap();
            zip.write_all(&vec![0u8; REAL as usize]).unwrap();
            zip.finish().unwrap();
        }
        let forged = forge_declared_size(&buf, REAL, 1);
        let mut archive = zip::ZipArchive::new(Cursor::new(forged.as_slice())).unwrap();
        let cap: u64 = 50;
        let mut budget: u64 = 0;
        let mut why = |name: &str| {
            read_zip_part_or_why(&mut archive, "<test>", name, &mut budget, cap)
                .unwrap()
                .expect_err("the part is left out")
        };
        assert_eq!(why("absent.bin"), PartLeftOut::Missing);
        assert_eq!(why("big.bin"), PartLeftOut::DeclaredOverCap);
        assert_eq!(why("forged.bin"), PartLeftOut::UnreadableAsDeclared);
        assert_eq!(budget, 0, "a part left out charges nothing");

        assert!(PartLeftOut::DeclaredOverCap.is_over_the_limit());
        assert!(PartLeftOut::InflatedOverCap.is_over_the_limit());
        assert!(PartLeftOut::PastDocumentBudget.is_over_the_limit());
        assert!(!PartLeftOut::Missing.is_over_the_limit());
        assert!(!PartLeftOut::Unopenable.is_over_the_limit());
        assert!(!PartLeftOut::UnreadableAsDeclared.is_over_the_limit());
    }

    /// feature-66 (R1.2): tables are renumbered per chunk by first appearance, in the order of
    /// [`RawRowSpan::start`], whatever their raw numbers; a nested table's number need not grow
    /// monotonically.
    #[test]
    fn chunk_table_rows_numbers_tables_by_first_appearance() {
        use crate::parser::row_span;
        let raw = |table, start, end| RawRowSpan { table, start, end };
        assert_eq!(chunk_table_rows(&[]), Some(vec![]));
        assert_eq!(
            chunk_table_rows(&[raw(4, 0, 5), raw(7, 6, 18), raw(4, 19, 24)]),
            Some(vec![
                row_span(0, 0, 5),
                row_span(1, 6, 18),
                row_span(0, 19, 24)
            ])
        );
        assert_eq!(
            chunk_table_rows(&[raw(2, 12, 20), raw(0, 0, 11)]),
            Some(vec![row_span(0, 0, 11), row_span(1, 12, 20)]),
            "ordered by start"
        );
    }

    /// feature-66 (R1.1): an offset past `u32` leaves the chunk not computed.
    #[cfg(target_pointer_width = "64")]
    #[test]
    fn a_span_past_u32_is_not_computed() {
        let far = u32::MAX as usize + 1;
        assert_eq!(
            chunk_table_rows(&[RawRowSpan {
                table: 0,
                start: far,
                end: far + 3
            }]),
            None
        );
    }

    /// feature-66: a row line goes after a newline only when the body has text, and its span
    /// covers the line alone.
    #[test]
    fn push_row_line_marks_the_line_it_adds() {
        let (mut body, mut spans) = (String::new(), Vec::new());
        push_row_line(&mut body, &mut spans, 3, "alder\tbirch");
        body.push_str("\npreface");
        push_row_line(&mut body, &mut spans, 3, "\tcedar");
        assert_eq!(body, "alder\tbirch\npreface\n\tcedar");
        assert_eq!(
            spans,
            vec![
                RawRowSpan {
                    table: 3,
                    start: 0,
                    end: 11
                },
                RawRowSpan {
                    table: 3,
                    start: 20,
                    end: 26
                },
            ]
        );
    }
}
