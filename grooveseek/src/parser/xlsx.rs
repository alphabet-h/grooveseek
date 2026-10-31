//! xlsx / xls (`.xlsx` / `.xls`) parser. calamine 経由でシート単位チャンク。
use std::io::Cursor;

use anyhow::{Result, anyhow};
use calamine::{Data, Reader};

use super::{ParsedDocument, Parser, single_text_chunk};

pub struct XlsxParser {
    /// (feature-61) 文書 1 本の展開後合計の上限 (`[index].max_decompressed_size`)。
    decompressed_budget: u64,
}

impl XlsxParser {
    /// (feature-61) 展開 budget を `budget` bytes にした parser。[`super::Registry`] が
    /// `[index].max_decompressed_size` から作る。
    pub fn with_budget(budget: u64) -> Self {
        Self {
            decompressed_budget: budget,
        }
    }
}

impl Default for XlsxParser {
    /// 展開 budget = [`super::DEFAULT_MAX_DECOMPRESSED_BYTES`]。
    fn default() -> Self {
        Self::with_budget(super::DEFAULT_MAX_DECOMPRESSED_BYTES)
    }
}

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
        parse_workbook_bytes_budgeted(
            bytes,
            path_hint,
            WorkbookFormat::Xlsx,
            SHEET_MAX_BYTES,
            self.decompressed_budget,
        )
    }

    /// (feature-61) The read path keeps the built-in budget whatever `[index]`
    /// set this parser up with ([`super::Parser::parse_bytes_for_read_inner`]).
    /// Same parse as [`super::Parser::parse_bytes_inner`], other budget.
    fn parse_bytes_for_read_inner(
        &self,
        bytes: &[u8],
        path_hint: &str,
        _exclude_headings: &[&str],
    ) -> Result<ParsedDocument> {
        parse_workbook_bytes_budgeted(
            bytes,
            path_hint,
            WorkbookFormat::Xlsx,
            SHEET_MAX_BYTES,
            super::DEFAULT_MAX_DECOMPRESSED_BYTES,
        )
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
/// なる事故を防ぐ (§4.5)。**truncate の粒度はセル単位** (AX-03、kuriya 604): セルを
/// 1 つ読むたびに「今の行をここで確定したら合計が cap を超えるか」を見て、超えるなら
/// その行をそのセルまでで確定して break する。したがって
/// - 最終 content の長さは `cap + (超過を招いた 1 セルの bytes) + 1` 以内、
/// - **1 セルだけで cap を超える場合もそのセルは丸ごと emit** してから break する
///   (セルの途中では切らない)。
///
/// 旧版は行の境目でしか見ていなかったので、1 行に同じ共有文字列 (最大で展開 budget
/// 1 本分) を参照するセルを並べると、1 行 = 1 シートが展開 budget の何倍にも膨らんだ
/// (1 MiB の共有文字列を 200 セルで参照 → 200 MiB、`ax03-findings.md` の
/// `row_ssref200`)。セル 1 つは共有文字列 1 本 (展開 budget 以内) に縛られる。
const SHEET_MAX_BYTES: usize = 1024 * 1024;

/// `<si/>` (5 bytes) が共有文字列 1 本の最短の書き方。正直な sharedStrings part は
/// `uniqueCount` 件の `<si>` を持つので、展開後 `n` bytes の part に入る件数は `n / 5` 以下。
/// [`preflight_xlsx_decompression_budget_timed`] は、`uniqueCount` が書かれていた entry の実展開
/// bytes をこれで割った数を上限にする。reserve はその entry の大きさの 24 / 5 倍以内に収まり
/// (calamine は 1 件 24 bytes)、`[index].max_decompressed_size` を上げても (`"unlimited"` でも)
/// 上限は file の中身で決まる。
const MIN_SHARED_STRING_BYTES: u64 = 5;

/// 1 つの part を共有するシートの名前を、chunk の見出しに並べる時の上限 (bytes、区切りの `", "`
/// を含む)。見出しは embedding に渡る context (`build_context` が 200 文字で切る) にも入るので、
/// 同じ 200 に揃える。最初の名前 (実際に読んだシート) は長さに関係なく必ず出す。
const SHARED_PART_LABEL_MAX_BYTES: usize = 200;

/// xlsx 1 本の抽出に掛ける上限の組 (cap 注入版の [`parse_xlsx_bytes_limited`] が受け取る)。
#[derive(Clone, Copy, Debug)]
struct XlsxLimits {
    /// シート 1 枚のテキスト上限 ([`SHEET_MAX_BYTES`])。超えたら truncate。
    sheet_max_bytes: usize,
    /// 展開 budget (`[index].max_decompressed_size`)。preflight と frontmatter が使う。
    decompressed_budget: u64,
    /// 文書全体のテキスト上限。超えたら `Err` (file ごと skip)。本番は展開 budget と同じ値。
    doc_text_cap: usize,
    /// 文書 1 本の実時間上限。超えたら `Err`。本番は
    /// [`super::pdf::PDF_DOC_EXTRACT_TIMEOUT`] (PDF と同じ 120 秒)。
    timeout: std::time::Duration,
}

/// xls 用の抽出入口。[`SHEET_MAX_BYTES`] と既定の展開 budget を渡す薄い wrapper。
///
/// (feature-61) xlsx は [`XlsxParser`] が構築時の budget で
/// [`parse_workbook_bytes_budgeted`] を直接呼ぶので、ここを通るのは registry から到達しない
/// [`XlsParser`] だけ。
fn parse_workbook_bytes(
    bytes: &[u8],
    path_hint: &str,
    format: WorkbookFormat,
) -> Result<ParsedDocument> {
    parse_workbook_bytes_capped(bytes, path_hint, format, SHEET_MAX_BYTES)
}

/// シート上限を注入できる版 (unit test が小さい cap で truncate 分岐を突くため分離)。
/// 展開 budget は既定値。
fn parse_workbook_bytes_capped(
    bytes: &[u8],
    path_hint: &str,
    format: WorkbookFormat,
    sheet_max_bytes: usize,
) -> Result<ParsedDocument> {
    parse_workbook_bytes_budgeted(
        bytes,
        path_hint,
        format,
        sheet_max_bytes,
        super::DEFAULT_MAX_DECOMPRESSED_BYTES,
    )
}

/// シート上限と展開 budget の両方を取る本体。XLSX/XLS で抽出方式が異なる (下記 fn 群の
/// doc comment 参照) ため format 別の実装へ dispatch するだけの薄い wrapper。xls (BIFF) は
/// zip 展開を経由しないので展開 budget を使わない。
fn parse_workbook_bytes_budgeted(
    bytes: &[u8],
    path_hint: &str,
    format: WorkbookFormat,
    sheet_max_bytes: usize,
    decompressed_budget: u64,
) -> Result<ParsedDocument> {
    match format {
        WorkbookFormat::Xlsx => {
            parse_xlsx_bytes_capped(bytes, path_hint, sheet_max_bytes, decompressed_budget)
        }
        WorkbookFormat::Xls => parse_xls_bytes_capped(bytes, path_hint, sheet_max_bytes),
    }
}

/// xlsx (zip container) の抽出本体。streaming cell reader を使う (下記
/// `extract_xlsx_sheet_text_streaming` 参照)。
///
/// 文書単位の上限 2 つ (AX-03、kuriya 604) をここで決める: 文書全体のテキストは展開 budget
/// まで (超えたら `Err`。PDF の [`super::pdf`] と同じ形と文言)、実時間は
/// [`super::pdf::PDF_DOC_EXTRACT_TIMEOUT`] まで。index と read 経路 (MCP の `get_document`)
/// はどちらもここを通るので、両方に同じ上限が掛かる。
fn parse_xlsx_bytes_capped(
    bytes: &[u8],
    path_hint: &str,
    sheet_max_bytes: usize,
    decompressed_budget: u64,
) -> Result<ParsedDocument> {
    parse_xlsx_bytes_limited(
        bytes,
        path_hint,
        XlsxLimits {
            sheet_max_bytes,
            decompressed_budget,
            // `"unlimited"` の `u64::MAX` は usize に収まらない環境でも飽和させる (PDF と同じ)。
            doc_text_cap: usize::try_from(decompressed_budget).unwrap_or(usize::MAX),
            // 文書 1 本の抽出時間の上限は PDF と同じ値を共有する (1 つの問いに 1 つの答え)。
            timeout: super::pdf::PDF_DOC_EXTRACT_TIMEOUT,
        },
    )
}

/// [`parse_xlsx_bytes_capped`] の本体 (上限を全部注入できる版。unit test が小さい上限で
/// 文書 budget と時間の分岐を踏むため分離)。
///
/// シートの読み方 (AX-03):
///
/// 1. **同名のシートの 2 枚目以降は開かない**。calamine の `worksheet_cells_reader(name)` は
///    同名なら最初の 1 枚を返すので、2 枚目以降は 1 枚目と同じ part を読み直すだけ。
/// 2. **既に読んだ zip entry を指す、名前の違うシートは開かず、最初のシートの chunk の見出しに
///    名前を足す**。calamine は `<sheet>` ごとに part を展開し直し、同じ part を指す `<sheet>` を
///    何枚でも許す (同じ `r:id` でも、別の `r:id` で同じ `Target` でも)。part は 1 回だけ読み、
///    chunk は 1 本で、見出しは `Sheet: Alpha, Beta` のように workbook の順で名前を `", "` で
///    並べる ([`shared_part_label`]。[`SHARED_PART_LABEL_MAX_BYTES`] を超える分は
///    `", and N more"`)。どの名前で探しても同じ text に当たる。sheet から entry への解決は
///    [`resolve_sheet_parts`] が calamine 0.37.0 の規則を写して行う。**写しは重複の判定にしか
///    使わない**: 解決できなければ全シートを読み、写しが calamine とずれても下の上限が文書を
///    縛る (上限の正しさは写しに依らない)。
///
/// 上限 (どれも calamine が読むパートを name から言い当てない):
///
/// - シート 1 枚: [`SHEET_MAX_BYTES`] をセル単位で (truncate)
/// - 文書全体のテキスト: [`XlsxLimits::doc_text_cap`] をセル単位で (`Err`)。共有文字列はセルから
///   何度でも参照できるので、出力テキストは入力の大きさに比例しない。重複の無い 200 枚の part が
///   それぞれ同じ 1 MiB の共有文字列を参照するだけで 200 MiB になった (`distinct200`)。見出しに
///   足した別名の bytes もここに数える
/// - 実時間: [`XlsxLimits::timeout`] を、preflight の展開の読み出しごと (8 KiB)、セルごと、
///   シートの境目ごとに (`Err`)。テキストを出さない part を重複させずに並べる形 (出力ゼロのまま
///   CPU だけ食う) の backstop。**割り込めない区間がある**: `calamine::Xlsx::new` (sharedStrings /
///   styles / rels / workbook を読む) と、1 回の `next_cell` (セルを 1 つも含まない part ならその
///   part 全体) は calamine の内側なので、その間は時間を見られない。どちらの仕事量も、preflight が
///   archive 全体を縛る展開量 (`[index].max_decompressed_size`) に比例する範囲に収まる
fn parse_xlsx_bytes_limited(
    bytes: &[u8],
    path_hint: &str,
    limits: XlsxLimits,
) -> Result<ParsedDocument> {
    let guard = DocumentGuard {
        path_hint,
        used: 0,
        cap: limits.doc_text_cap,
        started: std::time::Instant::now(),
        timeout: limits.timeout,
        sheets: 0,
    };
    // codex P2 (PR #70 round 2, zip-bomb hardening): calamine は zip 展開を
    // 内部で行うため `ooxml::read_zip_part` の累積 budget 機構が効かない。
    // calamine を呼ぶ前に、実際に読まれる XML part の申告 uncompressed
    // size 合計を検査する。(feature-61) budget は parser が構築時に受け取った値。
    // 時間の上限もこの走査の中で見る (展開そのものが文書の仕事量の大半を占め得る)。
    preflight_xlsx_decompression_budget_timed(
        bytes,
        path_hint,
        limits.decompressed_budget,
        Some((guard.started, guard.timeout)),
    )?;
    extract_xlsx_workbook(bytes, path_hint, limits, guard)
}

/// preflight を通った workbook を開いてシートを読む ([`parse_xlsx_bytes_limited`] の後半)。
/// `guard` は preflight の前に作ったもの (時間は preflight から数える)。unit test はここを
/// 直接呼んで、preflight を通った後の判定点 (シートの境目など) を踏む。
fn extract_xlsx_workbook(
    bytes: &[u8],
    path_hint: &str,
    limits: XlsxLimits,
    mut guard: DocumentGuard<'_>,
) -> Result<ParsedDocument> {
    let cursor = Cursor::new(bytes);
    // codex P2 (PR #70 round 3): `open_workbook_auto_from_rs` (auto-probe)
    // ではなく、登録拡張子に一致する reader を明示的に使う (WorkbookFormat
    // の doc comment 参照)。
    let mut xlsx = calamine::Xlsx::new(cursor)
        .map_err(|e| anyhow!("{path_hint}: cannot open workbook (encrypted or corrupt): {e}"))?;

    let frontmatter = xlsx_frontmatter(bytes, path_hint, limits.decompressed_budget);
    let title = frontmatter.title.as_deref().unwrap_or("");
    let mut chunks = Vec::new();
    // `sheet_names()` は Vec<String> を owned で返すため、ループ中に
    // `&mut xlsx` を再度借用しても (streaming reader との) 競合はない。
    let names = xlsx.sheet_names();
    let parts = resolve_sheet_parts(bytes, &names);

    // 1 つの part を最初に指したシートと、同じ part を指す別名のシート (workbook の順)。
    let mut groups: Vec<(&str, Vec<&str>)> = Vec::new();
    let mut group_of_part: std::collections::HashMap<usize, usize> =
        std::collections::HashMap::new();
    let mut seen_names: std::collections::HashSet<&str> = std::collections::HashSet::new();
    // 読み飛ばしたシートと別名にまとめたシートの数。1 枚ずつ warn すると、`<sheet>` を何千枚も
    // 並べた file が stderr を同じ数の行で埋めるので、最後に 1 行でまとめる。
    let (mut same_name, mut aliased) = (0usize, 0usize);
    for (i, name) in names.iter().enumerate() {
        // シートの数は file の大きさに比例しないので、読まない経路でも時間を見る。
        guard.check(0)?;
        if !seen_names.insert(name.as_str()) {
            same_name += 1;
            continue;
        }
        match parts.as_ref().and_then(|parts| parts[i]) {
            Some(index) => match group_of_part.get(&index) {
                Some(&group) => {
                    groups[group].1.push(name);
                    aliased += 1;
                }
                None => {
                    group_of_part.insert(index, groups.len());
                    groups.push((name, Vec::new()));
                }
            },
            None => groups.push((name, Vec::new())),
        }
    }

    for (name, aliases) in groups {
        let label = shared_part_label(name, &aliases);
        // 見出しに足した別名の分も文書のテキストに数える (名前だけを何千も並べる形を縛る)。
        guard.used = guard
            .used
            .saturating_add(label.len().saturating_sub(name.len()));
        guard.check(0)?;
        guard.sheets += 1;
        let extracted = extract_xlsx_sheet_text_streaming(
            &mut xlsx,
            name,
            path_hint,
            limits.sheet_max_bytes,
            &guard,
        )?;
        // セルを 1 つも読まなかったシートの分も時間を見る (セルごとの判定に掛からない)。
        guard.check(0)?;
        let Some(text) = extracted else {
            // open 失敗 / 途中の XML エラーはシート丸ごと skip
            // (旧実装の `worksheet_range()` エラー時と同じ挙動)。
            continue;
        };
        guard.used = guard.used.saturating_add(text.len());
        if text.trim().is_empty() {
            continue; // 空シートは chunk を作らない
        }
        chunks.push(super::sheet_chunk(chunks.len(), title, &label, &text));
    }
    if same_name > 0 || aliased > 0 {
        eprintln!(
            "{path_hint}: skipped {same_name} sheet(s) that repeat an earlier sheet's name; read {aliased} sheet(s) that point at an earlier sheet's part once, under that sheet's heading"
        );
    }

    let raw_content = super::join_chunk_bodies(&chunks);
    Ok(ParsedDocument {
        frontmatter,
        chunks,
        raw_content,
        frontmatter_error: None,
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
/// `\n` を付けて `text` に push する。
///
/// 上限はセルを 1 つ読むたびに見る (AX-03)。「今の行をこのセルまでで確定した時の長さ」
/// (`text.len() + row_bytes + 1`) について:
///
/// - 文書全体の上限 (`guard`) を超えるなら `Err` (file ごと skip)。時間の上限も同じ所で見る
/// - `sheet_max_bytes` を超えるなら、行をこのセルまでで確定して warn + 読み込み打ち切り
///   (超過を招いたセルは保持し、次のセルには進まない。巨大な 1 セルも丸ごと残す)
///
/// 行の境目の判定は要らない: 確定した長さはセルごとの判定を既に通っている。
fn extract_xlsx_sheet_text_streaming(
    xlsx: &mut calamine::Xlsx<Cursor<&[u8]>>,
    name: &str,
    path_hint: &str,
    sheet_max_bytes: usize,
    guard: &DocumentGuard<'_>,
) -> Result<Option<String>> {
    let Ok(mut reader) = xlsx.worksheet_cells_reader(name) else {
        return Ok(None);
    };
    let mut text = String::new();
    let mut row_cells: Vec<String> = Vec::new();
    // `row_cells` を `\t` で繋いだ長さ (改行を除く)。
    let mut row_bytes: usize = 0;
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
            Err(_) => return Ok(None),
        };
        let (row, _col) = cell.get_position();
        if current_row != Some(row) {
            if current_row.is_some() {
                flush_xlsx_row(&mut text, &row_cells);
                row_cells.clear();
                row_bytes = 0;
            }
            current_row = Some(row);
        }
        let data: Data = cell.get_value().clone().into();
        let value = match data {
            Data::Empty => String::new(),
            other => other.to_string(),
        };
        let separator = usize::from(!row_cells.is_empty());
        row_bytes = row_bytes
            .saturating_add(separator)
            .saturating_add(value.len());
        row_cells.push(value);
        // この行をここで確定した時のシートの長さ (改行込み)。
        let settled = text.len().saturating_add(row_bytes).saturating_add(1);
        guard.check(settled)?;
        if settled > sheet_max_bytes {
            flush_xlsx_row(&mut text, &row_cells);
            eprintln!("{path_hint}: sheet {name:?} exceeds {sheet_max_bytes} bytes; truncating");
            return Ok(Some(text));
        }
    }
    Ok(Some(text))
}

/// 文書 1 本の抽出に掛ける上限 (テキスト量と実時間) の帳簿。[`parse_xlsx_bytes_limited`] が
/// 持ち、シートを読み終えるごとに `used` を足す。
struct DocumentGuard<'a> {
    path_hint: &'a str,
    /// 読み終えたシートのテキストの合計 (bytes)。
    used: usize,
    /// 文書全体のテキスト上限。
    cap: usize,
    started: std::time::Instant,
    timeout: std::time::Duration,
    /// 開いたシートの数 (error 文に使う)。
    sheets: usize,
}

impl DocumentGuard<'_> {
    /// 読み途中のシートが `pending` bytes になった時点で、文書がどちらかの上限を超えていれば
    /// `Err`。時間は処理の **後** に見る (罠: 前だけで見ると最後の超過を拾えない。PDF と同じ)。
    fn check(&self, pending: usize) -> Result<()> {
        if self.started.elapsed() > self.timeout {
            return Err(anyhow!(
                "{}: xlsx text extraction exceeded {} s after {} sheet(s) (decompression-bomb guard)",
                self.path_hint,
                self.timeout.as_secs_f64(),
                self.sheets
            ));
        }
        if self.used.saturating_add(pending) > self.cap {
            return Err(anyhow!(
                "{}: extracted text exceeds {} bytes across {} sheet(s) (decompression-bomb guard; raise [index].max_decompressed_size to admit this file)",
                self.path_hint,
                self.cap,
                self.sheets
            ));
        }
        Ok(())
    }
}

/// 1 つの part を共有するシートの名前を、見出し (`Sheet: <これ>`) に入れる 1 つの文字列にする。
/// `name` (実際に読んだシート) を先頭に、`aliases` を workbook の順に `", "` で繋ぐ。別名が無ければ
/// `name` そのもの (従来の見出しと同じ)。繋いだ長さが [`SHARED_PART_LABEL_MAX_BYTES`] を超える
/// 別名からは並べず、残りの数を `", and N more"` で足す (`name` は長さに関係なく必ず出す)。
fn shared_part_label(name: &str, aliases: &[&str]) -> String {
    let mut label = name.to_string();
    for (listed, alias) in aliases.iter().enumerate() {
        if label.len() + 2 + alias.len() > SHARED_PART_LABEL_MAX_BYTES {
            label.push_str(&format!(", and {} more", aliases.len() - listed));
            return label;
        }
        label.push_str(", ");
        label.push_str(alias);
    }
    label
}

/// workbook の `<sheet>` を 1 枚ずつ、calamine 0.37.0 がそのシートで開く zip entry の index に
/// 解決する。戻り値は `calamine_names` (= `Xlsx::sheet_names()`) と同じ並びで、entry が無い
/// シートは `None`。**重複除去にしか使わない** ([`parse_xlsx_bytes_limited`]): calamine は
/// sheet から part への path を公開しない (`Xlsx::sheets` は private、公開の `Sheet` は
/// name / typ / visible だけ) ので、規則を写すしかない。
///
/// 写した規則 (calamine 0.37.0 の `src/xlsx/mod.rs` と `src/utils.rs`):
///
/// - zip の entry 名は `\` を `/` にして ASCII 小文字にした名前で引く (無ければ path そのまま)。
///   同じ小文字名の entry が複数あれば `ZipArchive::file_names` の順で後勝ち
/// - `_rels/.rels` の `<Relationships>` 以降の `<Relationship>` のうち、`Type` が
///   `/relationships/officeDocument` で終わるものの `Target` (entity を展開) の最後の `/`
///   までが xl_path (先頭の `/` は剥ぐ)。複数あれば後勝ち
/// - `{xl_path}_rels/workbook.xml.rels` の `<Relationship>` を `Id` で引く表 (後勝ち)。
///   属性は raw で、`Id` / `Type` / `Target` が 3 つ揃った所で読むのをやめる
/// - `{xl_path}workbook.xml` の `<sheet>` (`</workbook>` まで) ごとに、`name` (entity を展開) と、
///   local name が `id` の属性 (後勝ち) で表を引く。`Target` が `/` で始まれば先頭を剥いだ
///   絶対 path、そうでなければ `xl_path + Target`
///
/// 解決できない時 (part が読めない、XML / 属性が壊れている、表に無い `r:id`、解決した名前の並びが
/// `calamine_names` と一致しない) は `None` を返し、呼び出し側は全シートを読む。calamine が
/// 規則を変えて写しがずれても、失うのは重複除去だけで、文書の上限は別に掛かっている。
fn resolve_sheet_parts(bytes: &[u8], calamine_names: &[String]) -> Option<Vec<Option<usize>>> {
    let mut zip = zip::ZipArchive::new(Cursor::new(bytes)).ok()?;
    let mut cache: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    for name in zip.file_names() {
        let name = name.ok()?;
        cache.insert(
            name.replace('\\', "/").to_ascii_lowercase(),
            name.to_string(),
        );
    }
    let entry_name = |path: &str| -> String {
        cache
            .get(&path.to_ascii_lowercase())
            .cloned()
            .unwrap_or_else(|| path.to_string())
    };

    // 1. `_rels/.rels` から xl_path。
    let mut document_target: Option<String> = None;
    let mut seen_root = false;
    for_each_xml_element(
        &mut zip,
        &entry_name("_rels/.rels"),
        b"Relationships",
        |local, e| {
            if local == b"Relationships" {
                seen_root = true;
                return Some(());
            }
            if !seen_root || local != b"Relationship" {
                return Some(());
            }
            let [rel_type, target] = get_attrs_like_calamine(raw_attrs(e)?, ["Type", "Target"]);
            if rel_type.is_some_and(|t| t.ends_with("/relationships/officeDocument"))
                && let Some(target) = target
            {
                document_target = Some(quick_xml::escape::unescape(&target).ok()?.into_owned());
            }
            Some(())
        },
    )?;
    let target = document_target?;
    let xl_path = target
        .rfind('/')
        .map(|i| &target[..=i])
        .map(|p| p.strip_prefix('/').unwrap_or(p))
        .unwrap_or("")
        .to_string();

    // 2. `{xl_path}_rels/workbook.xml.rels` の Id -> Target。
    let mut relationships: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    for_each_xml_element(
        &mut zip,
        &entry_name(&format!("{xl_path}_rels/workbook.xml.rels")),
        b"Relationships",
        |local, e| {
            if local != b"Relationship" {
                return Some(());
            }
            let [id, _rel_type, target] =
                get_attrs_like_calamine(raw_attrs(e)?, ["Id", "Type", "Target"]);
            if let Some(id) = id {
                relationships.insert(id, target.unwrap_or_default());
            }
            Some(())
        },
    )?;

    // 3. `{xl_path}workbook.xml` の `<sheet>` ごとに entry を引く。
    let mut sheets: Vec<(String, Option<usize>)> = Vec::new();
    let workbook = entry_name(&format!("{xl_path}workbook.xml"));
    let mut pending: Vec<(String, String)> = Vec::new();
    for_each_xml_element(&mut zip, &workbook, b"workbook", |local, e| {
        if local != b"sheet" {
            return Some(());
        }
        let mut name = String::new();
        let mut rel_id = String::new();
        for (key, value) in raw_attrs(e)? {
            if key == "name" {
                name = quick_xml::escape::unescape(&value).ok()?.into_owned();
            } else if key == "id" || key.strip_suffix("id").is_some_and(|p| p.ends_with(':')) {
                rel_id = value;
            }
        }
        pending.push((name, rel_id));
        Some(())
    })?;
    for (name, rel_id) in pending {
        let target = relationships.get(&rel_id)?;
        let path = match target.strip_prefix('/') {
            Some(absolute) => absolute.to_string(),
            None => format!("{xl_path}{target}"),
        };
        sheets.push((name, zip.index_for_name(&entry_name(&path))));
    }

    let names_agree = sheets.len() == calamine_names.len()
        && sheets
            .iter()
            .zip(calamine_names)
            .all(|((resolved, _), calamine)| resolved == calamine);
    names_agree.then(|| sheets.into_iter().map(|(_, index)| index).collect())
}

/// zip entry `name` を calamine の `xml_reader` と同じ設定 (空要素を start + end に展開、
/// 閉じ tag の名前を照合しない) で読み、start 要素ごとに `on_start(local name, 要素)` を呼ぶ。
/// local name が `stop_at` の end 要素で読むのをやめる。entry が無ければ何もせず `Some(())`、
/// 読めない / XML が壊れている / `on_start` が `None` を返したら `None`。
fn for_each_xml_element(
    zip: &mut zip::ZipArchive<Cursor<&[u8]>>,
    name: &str,
    stop_at: &[u8],
    mut on_start: impl FnMut(&[u8], &quick_xml::events::BytesStart<'_>) -> Option<()>,
) -> Option<()> {
    use quick_xml::events::Event;

    let entry = match zip.by_name(name) {
        Ok(entry) => entry,
        Err(zip::result::ZipError::FileNotFound) => return Some(()),
        Err(_) => return None,
    };
    let mut reader = quick_xml::Reader::from_reader(std::io::BufReader::new(entry));
    let config = reader.config_mut();
    config.check_end_names = false;
    config.trim_text(false);
    config.check_comments = false;
    config.expand_empty_elements = true;
    let mut buf = Vec::new();
    loop {
        buf.clear();
        match reader.read_event_into(&mut buf).ok()? {
            Event::Start(e) => on_start(e.local_name().as_ref(), &e)?,
            Event::End(e) if e.local_name().as_ref() == stop_at => return Some(()),
            Event::Eof => return Some(()),
            _ => {}
        }
    }
}

/// 要素の属性を `(key, raw value)` で並べる (entity は展開しない。calamine の
/// `iter_raw_attrs` と同じ)。壊れていれば `None`。
fn raw_attrs(e: &quick_xml::events::BytesStart<'_>) -> Option<Vec<(String, String)>> {
    e.attributes()
        .with_checks(false)
        .map(|attr| {
            let attr = attr.ok()?;
            let key = std::str::from_utf8(attr.key.as_ref()).ok()?.to_string();
            let value = std::str::from_utf8(&attr.value).ok()?.to_string();
            Some((key, value))
        })
        .collect()
}

/// calamine の `get_attrs!` macro の写し: 属性を前から見て、`keys` のどれかに完全一致する
/// key の値を入れ (同じ key は後勝ち)、一致した回数 (重複も数える) が `keys` の数に届いたら
/// 読むのをやめる。
fn get_attrs_like_calamine<const N: usize>(
    attrs: Vec<(String, String)>,
    keys: [&str; N],
) -> [Option<String>; N] {
    let mut values: [Option<String>; N] = std::array::from_fn(|_| None);
    let mut found = 0;
    for (key, value) in attrs {
        let Some(slot) = keys.iter().position(|k| *k == key) else {
            continue;
        };
        values[slot] = Some(value);
        found += 1;
        if found == N {
            break;
        }
    }
    values
}

/// `row_cells` (存在したセルだけ) を `\t` join して 1 行として `text` に
/// 追記する (改行込み)。
fn flush_xlsx_row(text: &mut String, row_cells: &[String]) {
    text.push_str(&row_cells.join("\t"));
    text.push('\n');
}

/// xls (BIFF、非 zip container) の抽出本体。
///
/// **この経路は現在 registry から到達しない** (AU-06)。`Registry::from_enabled`
/// が `"xls"` を拒否するので、`.xls` は index されない。型とテストは
/// pre-check を実装したときに戻せるよう残してある。
///
/// # なぜ無効にしたか (要約)
///
/// 決定の全文 — 検討した 4 案とそれぞれの却下理由、再対応の条件 — は
/// `docs/decisions/0001-withdraw-xls-legacy-biff-support.md` (ADR-0001)。
/// ここには、この経路を再び有効化しようとする人がその場で要る実測値だけ残す。
///
/// `Xls::new()` は全シートを `BTreeMap` に保持し、各シートについて
/// `Range::from_sparse()` を呼んで bounding rectangle を **密に**確保する。
/// BIFF が縛るのは **シート 1 枚**であって workbook ではない (実測 2026-07-27):
///
/// | 項目 | 値 |
/// |---|---|
/// | `size_of::<calamine::Data>()` | 32 B |
/// | シート 1 枚の上限 (65536 × 256) | 16,777,216 セル = **512 MB** |
/// | `worksheet_range()` は既存 Range を clone | ピーク **1 GB** / シート |
/// | workbook の上限 | **無し** (シート数 × 512 MB) |
///
/// **割り当て失敗は catch できずプロセスが落ちる** (AU-01 と同じ性質) ため、
/// per-file skip (AU-21) でも panic guard でも救えない。
///
/// pre-check は **`Xls::new()` より前**にしか置けない: この関数に到達した時点で
/// 確保は完了しており、`Xls` は `ReaderRef` 未実装なので clone も避けられない。
fn parse_xls_bytes_capped(
    bytes: &[u8],
    path_hint: &str,
    sheet_max_bytes: usize,
) -> Result<ParsedDocument> {
    let cursor = Cursor::new(bytes);
    let mut xls = calamine::Xls::new(cursor)
        .map_err(|e| anyhow!("{path_hint}: cannot open workbook (encrypted or corrupt): {e}"))?;

    // xls (BIFF) には core.xml が無いため常にファイル名 fallback。
    let frontmatter = xlsx_frontmatter(bytes, path_hint, super::DEFAULT_MAX_DECOMPRESSED_BYTES);
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
        chunks.push(super::sheet_chunk(chunks.len(), title, &name, &text));
    }

    let raw_content = super::join_chunk_bodies(&chunks);
    Ok(ParsedDocument {
        frontmatter,
        chunks,
        raw_content,
        frontmatter_error: None,
    })
}

/// calamine 呼び出し前の pre-flight 検査を、既定の展開 budget
/// ([`super::DEFAULT_MAX_DECOMPRESSED_BYTES`]) で行う (test 専用)。本番は parser の budget で
/// [`preflight_xlsx_decompression_budget_timed`] を直接呼ぶ (feature-61)。
#[cfg(test)]
fn preflight_xlsx_decompression_budget(bytes: &[u8], path_hint: &str) -> Result<()> {
    preflight_xlsx_decompression_budget_capped(
        bytes,
        path_hint,
        super::DEFAULT_MAX_DECOMPRESSED_BYTES,
    )
}

/// 時間の上限を付けずに [`preflight_xlsx_decompression_budget_timed`] を呼ぶ (test 専用。cap を
/// 注入して展開量の分岐を踏む test が使う)。
#[cfg(test)]
fn preflight_xlsx_decompression_budget_capped(
    bytes: &[u8],
    path_hint: &str,
    cap: u64,
) -> Result<()> {
    preflight_xlsx_decompression_budget_timed(bytes, path_hint, cap, None)
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
///    (2026-10-19、zip 9 で前提が変わった) 上の「申告値で bound していない」は
///    zip 8.6 までの話。zip 9.0.1 (zip-rs/zip2#1022) 以降は zip 自身が、申告
///    uncompressed size を超えた read を `InvalidData` ("File is larger than its
///    declared uncompressed size") で失敗させる。calamine も同じ `zip` の `by_name` で
///    part を読むので、そこで同じく止まる。そこで層 2 の [`count_decompressed_bytes_scanning`]
///    は申告 size ちょうどまで読んでから 1 バイトだけ probe して、zip の拒否を見分ける:
///    probe が EOF なら申告どおり、`Err` なら zip が申告超えを拒んだ (か、申告 size の
///    境目で decoder が失敗した = 壊れている) ので、**申告どおりに読めない entry として
///    zip-bomb guard の `Err` にする**。この `Err` は cap に依らないので、
///    `[index].max_decompressed_size` を上げる案内は付けない (上げても zip が拒む)。
///    probe が 1 バイト読めた時 (= zip が申告で止めなかった。zip 8.6 と同じ挙動への
///    退行) は、上のとおり残り budget + 1 バイトまで実際に展開して数え続ける。
///    entry は CRC の照合を外して開く (`ZipReadOptions::ignore_crc32`)。probe は EOF を
///    読むので、照合つきだと CRC だけが合わない entry (大きさは申告どおり) まで拒んで
///    しまうが、preflight の役目は展開量の bound だけで、CRC 違いは calamine に任せる
///    (calamine は閉じ tag で抜けて EOF の CRC を見ない part があり、media はそもそも
///    読まない)。申告 size の上限は CRC の照合と独立に zip が掛ける。
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
/// `zip v9.0.5` を共有、feature unification 済) なので、「我々が展開でき
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
/// に乗る。つまり画像も `[index].max_decompressed_size` に数えるので、
/// 「展開後の合計が cap を超える画像だらけの xlsx」は skip される。画像は
/// 圧縮済み (= 展開してもほぼ 1:1) なので、展開後の合計はおおむねファイルの
/// 大きさ以上になる。(feature-61) raw の上限は `[index].max_binary_file_size`
/// で展開 budget より大きくできるため、これは稀な場合ではない: ファイル側の
/// 上限だけを上げた運用者は、画像の多い workbook で展開側のメッセージを見る
/// ことになり、`max_decompressed_size` も上げる必要がある。名前ベースの穴を
/// 残すより、この誤検知を受け入れる方を選ぶ (skip は理由付きの warn として
/// 出るので silent failure ではない)。
///
/// 層 2 で読んだバイトは entry ごとに [`UniqueCountScan`] にも渡し、その entry の実展開 bytes /
/// [`MIN_SHARED_STRING_BYTES`] を超える `uniqueCount` が書かれていれば `Err` にする (AX-41。
/// calamine が申告件数を先に reserve して abort するのを、calamine を呼ぶ前に止める)。この拒否は
/// cap に依らないので、`[index].max_decompressed_size` を上げる案内は付けない。
///
/// zip として開けない場合 (xls = BIFF、非 zip container) は対象外として
/// `Ok(())` を返す — xls はこの経路の攻撃面ではない (calamine の BIFF
/// parser は zip 展開を経由しない)。
///
/// cap を注入できる版として分離する (unit test が小さい cap で on-the-fly の
/// 小さい fixture のまま超過分岐を突くため。`parse_workbook_bytes_capped`
/// の `sheet_max_bytes` 注入パターンを踏襲)。
///
/// `deadline` (`(始めた時刻, 上限)`) があれば、entry の境目と展開の読み出し (8 KiB) ごとに時間を
/// 見て、超えたら `Err` にする ([`XlsxLimits::timeout`]。文書の時間はここから数える)。
fn preflight_xlsx_decompression_budget_timed(
    bytes: &[u8],
    path_hint: &str,
    cap: u64,
    deadline: Option<(std::time::Instant, std::time::Duration)>,
) -> Result<()> {
    let Ok(mut zip) = zip::ZipArchive::new(Cursor::new(bytes)) else {
        return Ok(());
    };
    let expired = || deadline.is_some_and(|(started, timeout)| started.elapsed() > timeout);
    let timed_out = |name: &str| {
        let seconds = deadline.map_or(0.0, |(_, timeout)| timeout.as_secs_f64());
        anyhow!(
            "{path_hint}: xlsx decompression preflight exceeded {seconds} s at entry {name:?} (decompression-bomb guard)"
        )
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
            // zip 9 の `name()` は `ZipResult`。名前は下の error 文にしか使わないので、
            // 読めない名前でも entry を飛ばさず (飛ばすと budget に数えない穴になる)、
            // raw bytes を lossy に表示する。
            let name = match entry.name() {
                Ok(name) => name.into_owned(),
                Err(_) => String::from_utf8_lossy(entry.name_raw()).into_owned(),
            };
            (name, entry.size())
        };
        if expired() {
            return Err(timed_out(&name));
        }

        // 層 1: 申告値だけで既に budget 超過 = 展開を試みるまでもなく拒否。
        let remaining = cap.saturating_sub(total);
        if declared > remaining {
            anyhow::bail!(
                "{path_hint}: declared entry size exceeds {cap} bytes cap (zip-bomb guard; entry {name:?} declares {declared} bytes with {remaining} bytes of budget left; raise [index].max_decompressed_size to admit this file)"
            );
        }

        // 層 2: 申告値は信用できないので実際に展開して数える。申告 size ちょうどまで
        // 読んでから 1 バイト probe し、zip が申告どおりに読ませない entry はその場で
        // 拒否する ([`count_decompressed_bytes_scanning`])。zip が申告で止めなかった時も、残り
        // budget を 1 バイトでも超えたら打ち切れるよう `remaining + 1` で bound する。
        //
        // CRC は見ない (`ignore_crc32`)。probe は EOF を読むので、照合つきの reader だと
        // zip がそこで CRC を照合するが、preflight の役目は展開量の bound だけで、CRC だけ
        // が合わない entry は大きさが申告どおり。calamine は閉じ tag で抜けるので EOF の
        // CRC を見ない part があり (media はそもそも読まない)、そうした entry で workbook
        // ごと拒むのは過剰。旧版も CRC の `Err` を数え終わりとして通していた。申告 size の
        // 上限は CRC の照合と独立に zip が掛けるので、層 2 の拒否はそのまま残る。
        let Ok(mut entry) =
            zip.by_index_with_options(i, zip::ZipReadOptions::new().ignore_crc32(true))
        else {
            // 展開できない entry (暗号化 / 未対応の圧縮方式) は calamine でも
            // 読めないため、budget には数えず次へ進む。
            continue;
        };
        // entry ごとに新しく数える (上限がその entry の大きさで決まるため)。
        let mut unique_counts = UniqueCountScan::default();
        let actual = match count_decompressed_bytes_scanning(
            &mut entry,
            declared,
            remaining.saturating_add(1),
            &mut unique_counts,
            &expired,
        ) {
            Inflation::Counted(actual) => actual,
            Inflation::Expired => return Err(timed_out(&name)),
            Inflation::UnreadableAsDeclared => anyhow::bail!(
                "{path_hint}: entry {name:?} cannot be read as declared (zip-bomb guard; it declares {declared} bytes but holds more, or is corrupt; the archive is rejected whatever [index].max_decompressed_size is)"
            ),
        };
        unique_counts.finish();
        total = total.saturating_add(actual);
        if total > cap {
            anyhow::bail!(
                "{path_hint}: actual decompressed size of the archive exceeds {cap} bytes cap (zip-bomb guard; entry {name:?} declared {declared} bytes but expands past the remaining budget; raise [index].max_decompressed_size to admit this file)"
            );
        }
        // `uniqueCount` の上限はそれが書かれていた entry の実展開 bytes / 5 (`actual` は
        // 上の判定を通ったので打ち切られていない)。cap に依らないので、上げる案内は付けない。
        let unique_limit = actual / MIN_SHARED_STRING_BYTES;
        if unique_counts.largest > unique_limit {
            let largest = unique_counts.largest;
            anyhow::bail!(
                "{path_hint}: entry {name:?} declares uniqueCount {largest}, more shared strings than its {actual} bytes can hold (at most {unique_limit}) (zip-bomb guard; the workbook reader reserves memory for the declared count up front; the archive is rejected whatever [index].max_decompressed_size is)"
            );
        }
    }
    Ok(())
}

/// [`count_decompressed_bytes_scanning`] が entry を読んで分かったこと。
#[derive(Debug, PartialEq, Eq)]
enum Inflation {
    /// 展開後のバイト数 (`limit` 以下)。
    Counted(u64),
    /// 申告 size ちょうどまで読めた後、次の read が `Err` になった = 申告どおりに読めない。
    /// zip 9.0.1 (zip-rs/zip2#1022) 以降の zip は申告 uncompressed size を超える read を
    /// `Err` にするので、これは申告超えか、申告 size の境目で decoder が失敗した (壊れた
    /// stream) かのどちらか。CRC が合わない read の `Err` はここに来ない (層 2 は CRC の照合を
    /// 外して読む)。
    UnreadableAsDeclared,
    /// 読んでいる途中で文書の時間の上限を過ぎた ([`XlsxLimits::timeout`])。
    Expired,
}

/// 申告 size が `declared` の entry を `reader` から読み進め、展開後のバイト数を数える。
/// 中身は保持しない (固定バッファに読んでは捨てる) ので、メモリは `limit` に依らず一定。
///
/// 読み方は 3 段 (zip 9 で変わった前提は
/// [`preflight_xlsx_decompression_budget_timed`] の層 2):
///
/// 1. **申告 size ちょうどまで**読む。途中の読み出しエラー (壊れた deflate stream 等) と
///    申告より手前の EOF は `Err` にせず、**そこまでに読めたバイト数**を返す:
///    pre-flight の役目は「展開量が cap を超えないこと」の確認だけで、壊れたファイル
///    自体は後段の calamine が `Err` にする (= 既存の per-file skip 経路に乗る)。
/// 2. 申告 size まで読めたら **1 バイトだけ probe** する。EOF なら申告どおりなので、
///    読めたバイト数 (= 申告 size) を返す。`Err` なら
///    [`Inflation::UnreadableAsDeclared`] を返す: zip が申告を超える read を拒んだか、
///    申告 size の境目で decoder が失敗したかで、どちらでも calamine は同じ所で止まる
///    (`io::Error` の文言では区別しない)。呼び出し側は CRC の照合を外した reader を渡すので、
///    EOF での CRC の照合はここでは起きない (CRC 違いは calamine に任せる)。
/// 3. probe が 1 バイト読めた = zip が申告 size で止めなかった (zip 8.6 までの挙動。
///    今の zip なら退行) 時は、従来どおり `limit` バイトまで数え続ける。この段は防御の層
///    として残す (この file の tests の
///    `zip_refuses_to_read_an_entry_past_its_declared_size` が落ちたら現役になる)。
///
/// 呼び出し側は層 1 の後に `limit = remaining + 1` で呼ぶので `declared < limit`
/// (`remaining` が `u64::MAX` で `limit` が飽和した時だけ等しくなり得る)。それが
/// 崩れても `limit` を超えては読まない (1 段目も `limit` で止める)。
///
/// 読んだバイトは全部 `scan` にも渡す。preflight はこれで `uniqueCount` を探す
/// ([`UniqueCountScan`])。読み出しごとに `expired` を聞き、`true` なら
/// [`Inflation::Expired`] で止める (文書の時間の上限)。
fn count_decompressed_bytes_scanning(
    reader: &mut impl std::io::Read,
    declared: u64,
    limit: u64,
    scan: &mut UniqueCountScan,
    expired: &dyn Fn() -> bool,
) -> Inflation {
    let mut buf = [0u8; 8 * 1024];
    let mut total: u64 = 0;
    // 1. 申告 size ちょうどまで。
    let up_to_declared = declared.min(limit);
    while total < up_to_declared {
        let want = std::cmp::min(buf.len() as u64, up_to_declared - total) as usize;
        match reader.read(&mut buf[..want]) {
            Ok(0) | Err(_) => return Inflation::Counted(total),
            Ok(n) => {
                scan.feed(&buf[..n]);
                total += n as u64;
            }
        }
        if expired() {
            return Inflation::Expired;
        }
    }
    if total >= limit {
        return Inflation::Counted(total);
    }
    // 2. 申告 size の 1 バイト先を probe する。
    match reader.read(&mut buf[..1]) {
        Ok(0) => return Inflation::Counted(total),
        Ok(n) => {
            scan.feed(&buf[..n]);
            total += n as u64;
        }
        Err(_) => return Inflation::UnreadableAsDeclared,
    }
    // 3. zip が申告 size で止めなかった: zip 8.6 の時と同じく `limit` まで数える。
    while total < limit {
        let want = std::cmp::min(buf.len() as u64, limit - total) as usize;
        match reader.read(&mut buf[..want]) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                scan.feed(&buf[..n]);
                total += n as u64;
            }
        }
        if expired() {
            return Inflation::Expired;
        }
    }
    Inflation::Counted(total)
}

/// 展開したバイト列から `uniqueCount` 属性の値 (の最大) を探す (AX-41 / calamine の
/// tafia/calamine#684)。
///
/// calamine 0.37.0 は sharedStrings の `<sst uniqueCount="N">` の N をそのまま
/// `Vec::reserve(N)` する (`src/xlsx/mod.rs` の `read_shared_strings`)。1 件 24 bytes なので、
/// 1.7 KB の xlsx が `uniqueCount="100000000000"` と書くだけで 2.4 TB の確保に失敗して
/// **プロセスが abort する** (catch できない)。preflight は全 entry の全 byte を既に読んでいる
/// ので、そこで値を見る。
///
/// **part を name で選ばない** ([`preflight_xlsx_decompression_budget_timed`] の「なぜ全 entry
/// なのか」と同じ理由): calamine は sharedStrings の path を rels の xl_path から作るので、
/// どの entry が sharedStrings かを言い当てる代わりに、全 entry のどこに書かれた `uniqueCount`
/// も数える。上限はその値が書かれていた entry の実展開 bytes / 5 ([`MIN_SHARED_STRING_BYTES`])
/// なので、偽陽性は、その entry の大きさに見合わない `uniqueCount="<数>"` という文字列そのものを
/// セルの本文に書いた workbook だけ (推定、実害は無視できる)。
///
/// 読み方は calamine (`RawAttrIter` は key の前と `=` の前後の `is_ascii_whitespace` を飛ばし、
/// key は `=` までを末尾の空白を削って完全一致で比べ、値は raw。`atoi_simd` で先頭の `0` を
/// 飛ばし `+` を受けない) の上位集合: `uniqueCount` の後に ASCII 空白 (form feed を含む)、`=`、
/// ASCII 空白、引用符 (`"` か `'`) と続けば、先頭の `0` を飛ばして数字を飽和しながら読む。
/// `uniqueCount` の前に何があっても (calamine なら key が一致しない `xuniqueCount` でも)、数字の
/// 後に何が来ても (calamine なら数字でないので reserve しない値でも) 数える。状態は呼び出しを
/// またいで持つので、読み出しバッファの境目で切れた属性も拾う。entry の境目では
/// [`UniqueCountScan::finish`] する。
#[derive(Default)]
struct UniqueCountScan {
    state: UniqueCountState,
    /// これまでに読み終えた値の最大。
    largest: u64,
}

#[derive(Default, Clone, Copy)]
enum UniqueCountState {
    /// `uniqueCount` の先頭を探している。
    #[default]
    Idle,
    /// `uniqueCount` の先頭から n 文字が一致した。
    Name(usize),
    /// 名前の後、`=` を待っている。
    BeforeEq,
    /// `=` の後、引用符を待っている。
    AfterEq,
    /// 引用符の後、先頭の `0` を飛ばしている。
    Zeros,
    /// 数字を読んでいる (ここまでの値)。
    Digits(u64),
}

impl UniqueCountScan {
    const NEEDLE: &'static [u8] = b"uniqueCount";

    fn feed(&mut self, bytes: &[u8]) {
        use UniqueCountState::*;
        let mut i = 0;
        while i < bytes.len() {
            let b = bytes[i];
            match self.state {
                Idle => match bytes[i..].iter().position(|&c| c == Self::NEEDLE[0]) {
                    Some(at) => {
                        i += at + 1;
                        self.state = Name(1);
                    }
                    None => return,
                },
                Name(matched) => {
                    if b == Self::NEEDLE[matched] {
                        i += 1;
                        self.state = if matched + 1 == Self::NEEDLE.len() {
                            BeforeEq
                        } else {
                            Name(matched + 1)
                        };
                    } else {
                        // KMP: 一致した部分の最長の border から照合し直す (`b` は読み直す)。
                        let border = Self::border(matched);
                        self.state = if border == 0 { Idle } else { Name(border) };
                    }
                }
                // calamine の `RawAttrIter` と同じ `is_ascii_whitespace` (form feed を含む)。
                BeforeEq | AfterEq if b.is_ascii_whitespace() => i += 1,
                BeforeEq if b == b'=' => {
                    i += 1;
                    self.state = AfterEq;
                }
                AfterEq if matches!(b, b'"' | b'\'') => {
                    i += 1;
                    self.state = Zeros;
                }
                BeforeEq | AfterEq => self.state = Idle,
                Zeros if b == b'0' => i += 1,
                Zeros | Digits(_) if b.is_ascii_digit() => {
                    let so_far = match self.state {
                        Digits(v) => v,
                        _ => 0,
                    };
                    self.state = Digits(
                        so_far
                            .saturating_mul(10)
                            .saturating_add(u64::from(b - b'0')),
                    );
                    i += 1;
                }
                Zeros | Digits(_) => self.settle(),
            }
        }
    }

    /// entry の終わり。読みかけの値があれば数える。
    fn finish(&mut self) {
        self.settle();
    }

    fn settle(&mut self) {
        if let UniqueCountState::Digits(v) = self.state {
            self.largest = self.largest.max(v);
        }
        self.state = UniqueCountState::Idle;
    }

    /// [`UniqueCountScan::NEEDLE`] の先頭 `matched` 文字について、最長の真の border (接頭辞でも
    /// 接尾辞でもある部分) の長さ。
    fn border(matched: usize) -> usize {
        (1..matched)
            .rev()
            .find(|&len| Self::NEEDLE[..len] == Self::NEEDLE[matched - len..matched])
            .unwrap_or(0)
    }
}

/// `docProps/core.xml` から frontmatter を取る。読めなければ (zip でない xls、core.xml が
/// 無い・`cap` を超える・壊れている) ファイル名 fallback。`cap` は展開 budget。
fn xlsx_frontmatter(bytes: &[u8], path_hint: &str, cap: u64) -> super::Frontmatter {
    if let Ok(mut zip) = zip::ZipArchive::new(Cursor::new(bytes)) {
        let mut budget: u64 = 0;
        super::ooxml::core_xml_frontmatter(&mut zip, path_hint, &mut budget, cap).unwrap_or_else(
            |_| super::Frontmatter {
                title: super::txt::derive_title_pub(path_hint),
                ..Default::default()
            },
        )
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
        // (2026-10-19) zip 9.0.1 (zip-rs/zip2#1022) 以降は zip 自身が申告超えの
        // read を Err にするので、層 2 はその拒否を zip-bomb guard として伝える。
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
    fn test_xlsx_preflight_rejects_a_lying_declaration_even_within_the_cap() {
        // 層 2 は申告どおりに読めない entry を cap に依らず拒む: 実体 64 KiB を
        // 1 バイトと偽装したエントリは、実体がちょうど cap に収まっていても Err になる。
        // 2026-10-19 に期待を反転した。旧版は同じ fixture で「cap が実体を上回って
        // いれば通る (= 偽装の検出ではなく、あくまで展開量で判断している)」を確かめて
        // いたが、zip 9.0.1 (zip-rs/zip2#1022) 以降は zip 自身が申告 size を超える read
        // を Err にするので、申告が嘘の entry は cap 内でも申告の先を読めず (calamine
        // も同じ所で止まる)、層 2 はそれを zip-bomb guard として拒む。
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
        let err = preflight_xlsx_decompression_budget_capped(&forged, "ok.xlsx", REAL as u64)
            .expect_err(
                "an entry whose declaration is a lie must be Err even when its actual size is within the cap",
            );
        assert!(
            err.to_string().contains("zip-bomb guard"),
            "error message should mention zip-bomb guard, got: {err}"
        );
    }

    #[test]
    fn test_xlsx_preflight_rejects_cumulative_actual_size_over_cap() {
        // 1 エントリずつは cap 未満でも、複数の偽装エントリを積算すると
        // 超える (ooxml.rs::read_zip_entry_capped の累積 budget と同じ論点を
        // 実展開ベースの会計でも押さえる)。
        // (2026-10-19) zip 9.0.1 (zip-rs/zip2#1022) 以降は zip 自身が申告超えの
        // read を Err にするので、層 2 はその拒否を zip-bomb guard として伝える
        // (積算に届く前、1 個目の偽装エントリで Err になる)。
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
        let doc = XlsxParser::default()
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
        // (2026-10-19) zip 9.0.1 (zip-rs/zip2#1022) 以降は zip 自身が申告超えの
        // read を Err にするので、層 2 はその拒否を zip-bomb guard として伝える。
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

    /// zip 9.0.1 (zip-rs/zip2#1022) 以降の前提を pin する: 申告 uncompressed size を偽装した
    /// entry は、zip 自身が申告を超える read を `Err` にするので最後まで読めない。層 2
    /// ([`count_decompressed_bytes`]) はこの拒否を zip-bomb guard として伝えている。
    /// **この test が落ちたら zip が申告超えを止めなくなった印**で、層 2 の 3 段目 (probe が
    /// 1 バイト読めた時に `limit` まで数え続ける経路) が現役になる。
    #[test]
    fn zip_refuses_to_read_an_entry_past_its_declared_size() {
        use std::io::Read as _;

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
        let mut archive = zip::ZipArchive::new(Cursor::new(forged.as_slice())).unwrap();

        let mut entry = archive.by_index(0).unwrap();
        assert_eq!(entry.size(), 1, "premise: the forged entry declares 1 byte");
        let mut out = Vec::new();
        let read = entry.read_to_end(&mut out);
        assert!(
            read.is_err(),
            "zip must refuse to read the entry to its end: {read:?}, {} bytes read",
            out.len()
        );
        drop(entry);

        // What layer 2 relies on, read by read: the declared byte reads, the one after it fails.
        let mut entry = archive.by_index(0).unwrap();
        let mut byte = [0u8; 1];
        assert_eq!(
            entry.read(&mut byte).ok(),
            Some(1),
            "the declared byte reads"
        );
        assert!(
            entry.read(&mut byte).is_err(),
            "the read past the declared size must fail"
        );
    }

    /// [`zip_refuses_to_read_an_entry_past_its_declared_size`] の Stored 版。Stored の entry は
    /// decoder を通らず、圧縮 size (偽装していない) まで読める reader に申告 size の上限が直に
    /// 掛かる。申告を超える read はやはり zip が `Err` にするので、層 2 の前提は圧縮方式に依らない。
    #[test]
    fn zip_refuses_to_read_a_stored_entry_past_its_declared_size() {
        use std::io::Read as _;

        const REAL: u32 = 64 * 1024;
        let stored =
            SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
        let mut buf = Vec::new();
        {
            let mut zip = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
            zip.start_file("xl/worksheets/sheet1.xml", stored).unwrap();
            zip.write_all(&vec![0u8; REAL as usize]).unwrap();
            zip.finish().unwrap();
        }
        let forged = forge_declared_uncompressed_size(&buf, REAL, 1);
        let mut archive = zip::ZipArchive::new(Cursor::new(forged.as_slice())).unwrap();

        let mut entry = archive.by_index(0).unwrap();
        assert_eq!(
            entry.compression(),
            zip::CompressionMethod::Stored,
            "premise: the entry is stored"
        );
        assert_eq!(entry.size(), 1, "premise: the forged entry declares 1 byte");
        assert_eq!(
            entry.compressed_size(),
            u64::from(REAL),
            "premise: only the declared uncompressed size is forged"
        );
        let mut out = Vec::new();
        let read = entry.read_to_end(&mut out);
        assert!(
            read.is_err(),
            "zip must refuse to read the stored entry to its end: {read:?}, {} bytes read",
            out.len()
        );
        drop(entry);

        let mut entry = archive.by_index(0).unwrap();
        let mut byte = [0u8; 1];
        assert_eq!(
            entry.read(&mut byte).ok(),
            Some(1),
            "the declared byte reads"
        );
        assert!(
            entry.read(&mut byte).is_err(),
            "the read past the declared size must fail"
        );
    }

    /// 申告どおりに読めない entry の拒否は zip-bomb guard として entry 名と申告バイト数を言い、
    /// `[index].max_decompressed_size` を上げる案内を付けない (上げても zip が拒む)。cap を
    /// 実体の倍にしても、無制限にしても同じ `Err` になる。
    #[test]
    fn test_xlsx_preflight_names_an_entry_unreadable_as_declared_without_offering_a_raise() {
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
        for cap in [
            2 * u64::from(REAL),
            crate::parser::FileSizeLimit::Unlimited.cap_bytes(),
        ] {
            let err = preflight_xlsx_decompression_budget_capped(&forged, "bomb.xlsx", cap)
                .expect_err("an entry that cannot be read as declared is Err whatever the cap");
            let msg = err.to_string();
            assert_eq!(
                msg,
                "bomb.xlsx: entry \"xl/worksheets/sheet1.xml\" cannot be read as declared (zip-bomb guard; it declares 1 bytes but holds more, or is corrupt; the archive is rejected whatever [index].max_decompressed_size is)",
                "cap {cap}"
            );
            assert!(msg.is_ascii(), "{msg}");
            assert!(!msg.contains("raise [index]"), "{msg}");
        }
    }

    /// zip の CRC-32 を書き換えて、`name` の entry を「大きさは申告どおりで、CRC だけが合わない」
    /// entry にする。書き換える 2 箇所は zip の仕様どおり: local file header (`PK\x03\x04`) の
    /// +14、central directory header (`PK\x01\x02`) の +16 が、いずれも CRC-32 (u32 LE)。
    /// [`forge_declared_uncompressed_size`] と同じ patch の形。
    fn forge_crc32(zip_bytes: &[u8], name: &str) -> Vec<u8> {
        let mut archive = zip::ZipArchive::new(Cursor::new(zip_bytes)).unwrap();
        let index = archive
            .index_for_name(name)
            .expect("the entry to forge is in the archive");
        let real = archive.by_index_raw(index).unwrap().crc32();
        let fake = !real;
        let mut data = zip_bytes.to_vec();
        let mut patched = 0;
        for (magic, offset) in [(b"PK\x03\x04", 14usize), (b"PK\x01\x02", 16usize)] {
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
            "expected to patch the local + central header CRC fields of {name}"
        );
        data
    }

    /// (2026-10-19) 層 2 は CRC を見ない。probe は申告 size の 1 バイト先 (= EOF) を読むので、
    /// CRC の照合つきで開いた entry では zip がそこで CRC を照合する。照合つきのままだと、CRC
    /// だけが壊れた entry (大きさは申告どおり) で workbook ごと zip-bomb guard に拒まれていた:
    /// calamine が読まない media でも、calamine が閉じ tag で抜けて EOF の CRC を見ない
    /// worksheet でも。zip 8.6 の頃の層 2 は CRC の `Err` を数え終わりとして通していた。
    #[test]
    fn test_xlsx_preflight_leaves_an_entry_whose_crc_does_not_match_to_calamine() {
        use std::io::Read as _;

        const SHEET: &[u8] = br#"<?xml version="1.0"?><worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData><row r="1"><c r="A1" t="inlineStr"><is><t>hello</t></is></c></row></sheetData></worksheet>"#;
        const MEDIA: &str = "xl/media/image1.png";
        const MEDIA_BODY: &[u8] = b"\x89PNG\r\n\x1a\nnot an image: bytes calamine never reads";
        let stored =
            SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
        let workbook = {
            // `ZipWriter::new_append` は古い central directory を file に残す (CRC の欄が 3 か所
            // 一致する) ので、entry を raw copy して central directory が 1 つの archive に組む。
            let base_bytes = make_xlsx_with_worksheet_part("worksheets/sheet1.xml", SHEET);
            let mut base = zip::ZipArchive::new(Cursor::new(base_bytes.as_slice())).unwrap();
            let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
            for i in 0..base.len() {
                zip.raw_copy_file(base.by_index_raw(i).unwrap()).unwrap();
            }
            zip.start_file(MEDIA, stored).unwrap();
            zip.write_all(MEDIA_BODY).unwrap();
            zip.finish().unwrap().into_inner()
        };
        let cap = crate::parser::DEFAULT_MAX_DECOMPRESSED_BYTES;

        // (a) calamine が読まない media の CRC が合わない。
        let forged = forge_crc32(&workbook, MEDIA);
        {
            // 前提の確認: 大きさは申告どおりで、CRC だけが合わない。
            let mut archive = zip::ZipArchive::new(Cursor::new(forged.as_slice())).unwrap();
            let index = archive.index_for_name(MEDIA).unwrap();
            let mut out = Vec::new();
            let checked = archive.by_index(index).unwrap().read_to_end(&mut out);
            assert!(
                checked.is_err(),
                "premise: with the CRC check on, the forged media fails its read: {checked:?}"
            );
            out.clear();
            archive
                .by_index_with_options(index, zip::ZipReadOptions::new().ignore_crc32(true))
                .unwrap()
                .read_to_end(&mut out)
                .expect("premise: with the CRC check off, the forged media reads as declared");
            assert_eq!(out, MEDIA_BODY, "premise: only the CRC is forged");
        }
        preflight_xlsx_decompression_budget_capped(&forged, "crc.xlsx", cap)
            .expect("a CRC mismatch alone is not the zip-bomb guard's business");
        let doc = XlsxParser::default()
            .parse_bytes(&forged, "crc.xlsx", &[])
            .expect("calamine never reads the media, so the workbook reads");
        assert_eq!(
            doc.chunks.len(),
            1,
            "the worksheet is read whatever the media's CRC"
        );

        // (b) worksheet 自身の CRC が合わない。preflight は CRC を見ないので、これも通す。
        // その先で calamine が EOF まで読んで CRC で止まるかは calamine の読み方次第なので、
        // ここでは pin しない (止まっても、その sheet だけの skip に乗る。calamine 0.37.0 は
        // 閉じ tag で抜けて、この sheet を読み切った)。
        let forged = forge_crc32(&workbook, "xl/worksheets/sheet1.xml");
        preflight_xlsx_decompression_budget_capped(&forged, "crc.xlsx", cap)
            .expect("the preflight does not check a worksheet's CRC either");
    }

    /// 中身の無い entry (directory と空 file) は cap 0 でも通る: 申告 0 バイトは層 1 の
    /// `declared > remaining` に掛からず、層 2 は probe で EOF を読むだけで、数えるバイトは 0。
    #[test]
    fn test_xlsx_preflight_admits_directories_and_empty_files_at_a_zero_cap() {
        let stored =
            SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
        let mut buf = Vec::new();
        {
            let mut zip = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
            zip.add_directory("xl/", SimpleFileOptions::default())
                .unwrap();
            zip.add_directory("xl/media/", SimpleFileOptions::default())
                .unwrap();
            zip.start_file("xl/empty.xml", SimpleFileOptions::default())
                .unwrap();
            zip.start_file("xl/media/empty.bin", stored).unwrap();
            zip.finish().unwrap();
        }

        // 前提の確認: directory 2 つと空 file 2 つ。どれも 0 バイトを申告する。
        let mut archive = zip::ZipArchive::new(Cursor::new(buf.as_slice())).unwrap();
        assert_eq!(archive.len(), 4, "premise: four entries");
        for i in 0..archive.len() {
            let entry = archive.by_index_raw(i).unwrap();
            assert_eq!(entry.size(), 0, "premise: entry {i} declares 0 bytes");
            assert_eq!(
                entry.is_dir(),
                i < 2,
                "premise: entries 0 and 1 are directories"
            );
        }

        preflight_xlsx_decompression_budget_capped(&buf, "empty.xlsx", 0)
            .expect("entries that hold nothing fit a zero cap");
    }

    /// budget の境目: N バイトの entry 1 つは cap N で通り、cap N - 1 では zip-bomb guard の
    /// `Err` になる (申告 N バイトが残り N - 1 バイトを超えるので層 1 で拒む)。
    #[test]
    fn test_xlsx_preflight_admits_an_entry_of_exactly_the_cap_and_refuses_it_one_byte_below() {
        const N: u64 = 1000;
        let mut buf = Vec::new();
        {
            let mut zip = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
            zip.start_file("xl/worksheets/sheet1.xml", SimpleFileOptions::default())
                .unwrap();
            zip.write_all(&vec![b'x'; N as usize]).unwrap();
            zip.finish().unwrap();
        }

        preflight_xlsx_decompression_budget_capped(&buf, "edge.xlsx", N)
            .expect("an entry of exactly the cap fits it");
        let err = preflight_xlsx_decompression_budget_capped(&buf, "edge.xlsx", N - 1)
            .expect_err("an entry one byte over the cap is refused");
        assert!(
            err.to_string().contains("zip-bomb guard"),
            "error message should mention zip-bomb guard, got: {err}"
        );
    }

    /// `len` バイトのゼロを返した後は、read のたびに `Err` を返す reader。zip 9 が申告
    /// size の先で read を `Err` にするのを、zip を介さずに真似る。
    struct FailsAfter {
        len: u64,
        served: u64,
    }

    impl FailsAfter {
        fn new(len: u64) -> Self {
            Self { len, served: 0 }
        }
    }

    impl std::io::Read for FailsAfter {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if self.served >= self.len {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "past the declared size",
                ));
            }
            let n = std::cmp::min(buf.len() as u64, self.len - self.served) as usize;
            buf[..n].fill(0);
            self.served += n as u64;
            Ok(n)
        }
    }

    /// 層 2 の 3 段目 (zip が申告 size で止めなかった時): probe が 1 バイト読めたら、
    /// 従来どおり `limit` まで数え続ける。今の zip は申告で止めるので、zip を介さず、
    /// 申告を知らない `Cursor` で突く。申告ちょうどで終わる reader は申告どおりに数える。
    #[test]
    fn count_decompressed_bytes_counts_on_to_the_limit_when_the_reader_does_not_stop() {
        let bytes = [0u8; 100];
        assert_eq!(
            count_decompressed_bytes(&mut Cursor::new(&bytes[..]), 10, 50),
            Inflation::Counted(50),
            "a reader that goes on past the declared 10 bytes is counted up to the limit"
        );
        assert_eq!(
            count_decompressed_bytes(&mut Cursor::new(&bytes[..]), 100, 200),
            Inflation::Counted(100),
            "a reader that ends at the declared size is counted as declared"
        );
        assert_eq!(
            count_decompressed_bytes(&mut Cursor::new(&bytes[..0]), 0, 50),
            Inflation::Counted(0),
            "an empty entry that declares 0 bytes is counted as declared"
        );
    }

    /// 層 2 の 2 段目: 申告 size ちょうどまで読めた後の read が `Err` なら、数えずに
    /// 「申告どおりに読めない」を返す。申告より手前の `Err` / EOF は従来どおり、そこまでの
    /// バイト数を返す (壊れた stream は calamine が後で `Err` にする)。
    #[test]
    fn count_decompressed_bytes_tells_a_refusal_at_the_declared_size_from_an_earlier_failure() {
        assert_eq!(
            count_decompressed_bytes(&mut FailsAfter::new(10), 10, 50),
            Inflation::UnreadableAsDeclared,
            "a read that fails right after the declared size is the refusal"
        );
        assert_eq!(
            count_decompressed_bytes(&mut FailsAfter::new(0), 0, 50),
            Inflation::UnreadableAsDeclared,
            "an entry that declares 0 bytes and fails its first read is refused too"
        );
        assert_eq!(
            count_decompressed_bytes(&mut FailsAfter::new(4), 10, 50),
            Inflation::Counted(4),
            "a read that fails before the declared size counts what was read"
        );
        let bytes = [0u8; 4];
        assert_eq!(
            count_decompressed_bytes(&mut Cursor::new(&bytes[..]), 10, 50),
            Inflation::Counted(4),
            "an entry that ends before its declared size counts what it holds"
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
        assert!(XlsxParser::default().is_binary());
        assert_eq!(XlsxParser::default().extension(), "xlsx");
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
        let err = XlsxParser::default()
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
        let err = XlsxParser::default()
            .parse_bytes(&bytes, "fake.xlsx", &[])
            .expect_err(
                "ODS payload disguised as .xlsx must be rejected (no cross-format probing)",
            );
        assert!(
            err.to_string().contains("cannot open workbook"),
            "got: {err}"
        );
    }

    #[test]
    fn test_xlsx_parse_fallback_wraps_raw_text() {
        let doc = XlsxParser::default().parse("hello world content here", "x.xlsx", &[]);
        assert_eq!(doc.chunks.len(), 1);
        assert!(doc.chunks[0].content.contains("hello world"));
    }

    #[test]
    fn test_xlsx_sheet_chunks() {
        let bytes = make_minimal_xlsx(&[
            ("Sales", &[&["Q1", "100"], &["Q2", "200"]]),
            ("Notes", &[&["memo"]]),
        ]);
        let doc = XlsxParser::default()
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
        let doc = XlsxParser::default()
            .parse_bytes(&bytes, "b.xlsx", &[])
            .unwrap();
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
        let doc = XlsxParser::default()
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
        let doc = XlsxParser::default()
            .parse_bytes(&bytes, "sparse.xlsx", &[])
            .unwrap();
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

    /// feature-61 (AC5): the preflight runs against the budget the parser was
    /// built with, before calamine opens anything.
    #[test]
    fn xlsx_preflight_budget_follows_the_parser_it_was_built_with() {
        let bytes = make_minimal_xlsx(&[("S", &[&["x"]])]);
        let err = XlsxParser::with_budget(5)
            .parse_bytes(&bytes, "x.xlsx", &[])
            .expect_err("a 5-byte budget refuses the archive");
        let msg = err.to_string();
        assert!(
            msg.contains("zip-bomb guard") && msg.contains("max_decompressed_size"),
            "{msg}"
        );
        assert!(!msg.contains("file too large"), "{msg}");

        let doc = XlsxParser::with_budget(1 << 20)
            .parse_bytes(&bytes, "x.xlsx", &[])
            .expect("1 MiB is plenty for this workbook");
        assert_eq!(doc.chunks.len(), 1);
        assert!(
            XlsxParser::default()
                .parse_bytes(&bytes, "x.xlsx", &[])
                .is_ok()
        );
    }

    /// feature-61 (codex critical): the read path does not take the budget the
    /// parser was built with -- the 5-byte parser whose preflight refuses the
    /// archive on the index path reads it on the read path.
    #[test]
    fn xlsx_read_path_ignores_the_parser_budget() {
        let bytes = make_minimal_xlsx(&[("S", &[&["x"]])]);
        let tiny = XlsxParser::with_budget(5);
        assert!(tiny.parse_bytes(&bytes, "x.xlsx", &[]).is_err());
        let read = tiny
            .parse_bytes_for_read(&bytes, "x.xlsx", &[])
            .expect("the read path keeps the default budget");
        assert_eq!(read.chunks.len(), 1);
    }

    /// feature-61 (AC5): [`xlsx_frontmatter`] swallows a core.xml error, so the
    /// budget is observed through the title instead: core.xml one byte over the
    /// budget falls back to the file name, exactly the budget reads it.
    #[test]
    fn xlsx_frontmatter_reads_core_xml_under_the_parser_budget() {
        let core: &[u8] = br#"<?xml version="1.0"?><cp:coreProperties xmlns:cp="x" xmlns:dc="y"><dc:title>Workbook Title</dc:title></cp:coreProperties>"#;
        let mut bytes = Vec::new();
        {
            let mut zip = zip::ZipWriter::new(std::io::Cursor::new(&mut bytes));
            zip.start_file("docProps/core.xml", SimpleFileOptions::default())
                .unwrap();
            zip.write_all(core).unwrap();
            zip.finish().unwrap();
        }
        let fits = xlsx_frontmatter(&bytes, "book.xlsx", core.len() as u64);
        assert_eq!(fits.title.as_deref(), Some("Workbook Title"));
        let over = xlsx_frontmatter(&bytes, "book.xlsx", core.len() as u64 - 1);
        assert_eq!(
            over.title,
            crate::parser::txt::derive_title_pub("book.xlsx"),
            "a core.xml read that ignored the budget would have found the title"
        );
    }

    /// feature-61 (AC6): `"unlimited"` is `u64::MAX` in every comparison, and
    /// each of them only turns false -- nothing overflows.
    #[test]
    fn an_unlimited_cap_never_refuses_a_read_for_size() {
        let cap = crate::parser::FileSizeLimit::Unlimited.cap_bytes();
        let bytes = make_minimal_xlsx(&[("S", &[&["x"]])]);

        // xlsx preflight: `cap.saturating_sub(total)`, `remaining.saturating_add(1)`.
        preflight_xlsx_decompression_budget_capped(&bytes, "x.xlsx", cap)
            .expect("no archive is over an unlimited budget");
        let doc = XlsxParser::with_budget(cap)
            .parse_bytes(&bytes, "x.xlsx", &[])
            .expect("parse");
        assert_eq!(doc.chunks.len(), 1);

        // ooxml: `cap.saturating_add(1)` on the take, `*budget > cap` on the total.
        // The running total starts near the top so the sum saturates.
        let mut archive = zip::ZipArchive::new(Cursor::new(bytes.as_slice())).unwrap();
        let mut budget: u64 = u64::MAX - 1;
        let read = crate::parser::ooxml::read_zip_entry_capped(
            &mut archive,
            "xl/workbook.xml",
            &mut budget,
            cap,
        )
        .expect("an unlimited cap never errs on the total");
        assert!(read.is_some());
        assert_eq!(budget, u64::MAX);

        // read_checked: `len > cap` is false, `take(cap.saturating_add(1))`.
        let path = crate::test_support::unique_temp_path("groove-f61-unlimited");
        std::fs::write(&path, &bytes).unwrap();
        let content = crate::links::read_checked(&path, cap);
        let _ = std::fs::remove_file(&path);
        let crate::links::Content::Bytes(read_back) = content.expect("read") else {
            panic!("an unlimited cap refused the read");
        };
        assert_eq!(read_back, bytes);
    }

    // -----------------------------------------------------------------------
    // AX-03 (kuriya 604) / AX-41 (kuriya 626): sheets that share a part, shared
    // strings referenced many times, and the declared `uniqueCount`.
    // -----------------------------------------------------------------------

    /// [`count_decompressed_bytes_scanning`] without the `uniqueCount` scan: the counting the
    /// tests above pin, under the name they call it by.
    fn count_decompressed_bytes(
        reader: &mut impl std::io::Read,
        declared: u64,
        limit: u64,
    ) -> Inflation {
        count_decompressed_bytes_scanning(
            reader,
            declared,
            limit,
            &mut UniqueCountScan::default(),
            &|| false,
        )
    }

    const SPREADSHEET_NS: &str = r#"xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships""#;

    /// A workbook wired by hand: `sheets` are `(name, r:id)` in workbook order, `rels` are the
    /// workbook relationships `(Id, Target)` (all worksheets), `parts` are written as named, and
    /// `shared` (when given) becomes `{xl_dir}sharedStrings.xml`. The package relationship points
    /// at `{xl_dir}workbook.xml`, so `xl_dir` is the prefix calamine derives (`"xl/"` normally).
    fn make_wired_xlsx(
        xl_dir: &str,
        sheets: &[(String, String)],
        rels: &[(String, String)],
        parts: &[(String, Vec<u8>)],
        shared: Option<&[u8]>,
        stored: bool,
    ) -> Vec<u8> {
        let mut buf = Vec::new();
        {
            let mut zip = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
            let opt = if stored {
                SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored)
            } else {
                SimpleFileOptions::default()
            };
            zip.start_file("[Content_Types].xml", opt).unwrap();
            zip.write_all(br#"<?xml version="1.0"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="xml" ContentType="application/xml"/></Types>"#).unwrap();
            zip.start_file("_rels/.rels", opt).unwrap();
            zip.write_all(format!(r#"<?xml version="1.0"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="{xl_dir}workbook.xml"/></Relationships>"#).as_bytes()).unwrap();
            zip.start_file(format!("{xl_dir}workbook.xml"), opt)
                .unwrap();
            let mut wb = format!(r#"<?xml version="1.0"?><workbook {SPREADSHEET_NS}><sheets>"#);
            for (i, (name, rid)) in sheets.iter().enumerate() {
                wb.push_str(&format!(
                    r#"<sheet name="{name}" sheetId="{}" r:id="{rid}"/>"#,
                    i + 1
                ));
            }
            wb.push_str("</sheets></workbook>");
            zip.write_all(wb.as_bytes()).unwrap();
            zip.start_file(format!("{xl_dir}_rels/workbook.xml.rels"), opt)
                .unwrap();
            let mut rl = String::from(
                r#"<?xml version="1.0"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">"#,
            );
            for (id, target) in rels {
                rl.push_str(&format!(r#"<Relationship Id="{id}" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="{target}"/>"#));
            }
            rl.push_str("</Relationships>");
            zip.write_all(rl.as_bytes()).unwrap();
            for (name, body) in parts {
                zip.start_file(name.as_str(), opt).unwrap();
                zip.write_all(body).unwrap();
            }
            if let Some(shared) = shared {
                zip.start_file(format!("{xl_dir}sharedStrings.xml"), opt)
                    .unwrap();
                zip.write_all(shared).unwrap();
            }
            zip.finish().unwrap();
        }
        buf
    }

    fn worksheet_xml(rows: &str) -> Vec<u8> {
        format!(r#"<?xml version="1.0"?><worksheet {SPREADSHEET_NS}><sheetData>{rows}</sheetData></worksheet>"#).into_bytes()
    }

    /// One row holding one inline-string cell.
    fn one_cell_worksheet(text: &str) -> Vec<u8> {
        worksheet_xml(&format!(
            r#"<row r="1"><c r="A1" t="inlineStr"><is><t>{text}</t></is></c></row>"#
        ))
    }

    /// `<sst>` with `count_attr` written verbatim (e.g. `uniqueCount="3"`) and `items` as `<si>`.
    fn shared_strings_xml(count_attr: &str, items: &[&str]) -> Vec<u8> {
        let mut sst = format!(r#"<?xml version="1.0"?><sst {SPREADSHEET_NS} {count_attr}>"#);
        for item in items {
            sst.push_str(&format!("<si><t>{item}</t></si>"));
        }
        sst.push_str("</sst>");
        sst.into_bytes()
    }

    fn owned_pairs(pairs: impl IntoIterator<Item = (String, String)>) -> Vec<(String, String)> {
        pairs.into_iter().collect()
    }

    /// `n` sheets named `S0..` whose relationships all reach `xl/worksheets/sheet1.xml`.
    /// `distinct_rids` gives each sheet its own `r:id` (all with the same `Target`).
    fn sheets_sharing_one_part(n: usize, distinct_rids: bool) -> Vec<u8> {
        let rid = |i: usize| {
            if distinct_rids {
                format!("rId{i}")
            } else {
                "rId1".to_string()
            }
        };
        let sheets = owned_pairs((0..n).map(|i| (format!("S{i}"), rid(i))));
        let rels = if distinct_rids {
            owned_pairs((0..n).map(|i| (rid(i), "worksheets/sheet1.xml".to_string())))
        } else {
            owned_pairs([("rId1".to_string(), "worksheets/sheet1.xml".to_string())])
        };
        make_wired_xlsx(
            "xl/",
            &sheets,
            &rels,
            &[(
                "xl/worksheets/sheet1.xml".to_string(),
                one_cell_worksheet("shared-part"),
            )],
            None,
            false,
        )
    }

    /// AX-03: 200 sheets with the same `r:id` used to open the same part 200 times and
    /// emit 200 copies of its text; the part is read once.
    #[test]
    fn test_xlsx_sheets_sharing_one_part_are_read_once() {
        let bytes = sheets_sharing_one_part(200, false);
        let doc = XlsxParser::default()
            .parse_bytes(&bytes, "dup.xlsx", &[])
            .expect("parse");
        assert_eq!(doc.chunks.len(), 1, "one part, one chunk");
        assert!(
            doc.chunks[0]
                .heading
                .as_deref()
                .is_some_and(|h| h.starts_with("Sheet: S0, S1, ")),
            "the other names join the heading: {:?}",
            doc.chunks[0].heading
        );
        assert!(doc.chunks[0].content.contains("shared-part"));
    }

    /// AX-03: de-duplicating by `r:id` is not enough -- 200 distinct `r:id`s with one `Target`.
    #[test]
    fn test_xlsx_sheets_sharing_one_part_through_distinct_rids_are_read_once() {
        let bytes = sheets_sharing_one_part(200, true);
        let doc = XlsxParser::default()
            .parse_bytes(&bytes, "dup.xlsx", &[])
            .expect("parse");
        assert_eq!(doc.chunks.len(), 1, "one part, one chunk");
    }

    /// AX-03: a relative `Target`, an absolute one and one spelled in another case all reach
    /// the same zip entry in calamine, so they are one part.
    #[test]
    fn test_xlsx_sheets_sharing_one_part_through_absolute_and_relative_targets_are_read_once() {
        let sheets = owned_pairs([
            ("Rel".to_string(), "rId1".to_string()),
            ("Abs".to_string(), "rId2".to_string()),
            ("Case".to_string(), "rId3".to_string()),
        ]);
        let rels = owned_pairs([
            ("rId1".to_string(), "worksheets/sheet1.xml".to_string()),
            ("rId2".to_string(), "/xl/worksheets/sheet1.xml".to_string()),
            ("rId3".to_string(), "WORKSHEETS/Sheet1.XML".to_string()),
        ]);
        let bytes = make_wired_xlsx(
            "xl/",
            &sheets,
            &rels,
            &[(
                "xl/worksheets/sheet1.xml".to_string(),
                one_cell_worksheet("one-part"),
            )],
            None,
            false,
        );
        let doc = XlsxParser::default()
            .parse_bytes(&bytes, "spelling.xlsx", &[])
            .expect("parse");
        assert_eq!(doc.chunks.len(), 1, "three spellings of one part");
        assert_eq!(
            doc.chunks[0].heading.as_deref(),
            Some("Sheet: Rel, Abs, Case")
        );
    }

    /// calamine opens a sheet by name and takes the first sheet of that name, so a second
    /// sheet with the same name would read the first one's part again: it is skipped.
    #[test]
    fn test_xlsx_duplicate_sheet_names_produce_one_chunk() {
        let sheets = owned_pairs([
            ("Same".to_string(), "rId1".to_string()),
            ("Same".to_string(), "rId2".to_string()),
        ]);
        let rels = owned_pairs([
            ("rId1".to_string(), "worksheets/sheet1.xml".to_string()),
            ("rId2".to_string(), "worksheets/sheet2.xml".to_string()),
        ]);
        let bytes = make_wired_xlsx(
            "xl/",
            &sheets,
            &rels,
            &[
                (
                    "xl/worksheets/sheet1.xml".to_string(),
                    one_cell_worksheet("first-part"),
                ),
                (
                    "xl/worksheets/sheet2.xml".to_string(),
                    one_cell_worksheet("second-part"),
                ),
            ],
            None,
            false,
        );
        let doc = XlsxParser::default()
            .parse_bytes(&bytes, "names.xlsx", &[])
            .expect("parse");
        assert_eq!(doc.chunks.len(), 1);
        assert!(doc.chunks[0].content.contains("first-part"));
    }

    /// Equivalence (binary-parser-resource-guard-pitfalls 罠 5): distinct parts are all read,
    /// in workbook order, with the text they always had.
    #[test]
    fn test_xlsx_distinct_sheets_are_all_read() {
        let sheets = owned_pairs((1..=3).map(|i| (format!("S{i}"), format!("rId{i}"))));
        let rels =
            owned_pairs((1..=3).map(|i| (format!("rId{i}"), format!("worksheets/sheet{i}.xml"))));
        let parts: Vec<(String, Vec<u8>)> = (1..=3)
            .map(|i| {
                (
                    format!("xl/worksheets/sheet{i}.xml"),
                    one_cell_worksheet(&format!("text-{i}")),
                )
            })
            .collect();
        let bytes = make_wired_xlsx("xl/", &sheets, &rels, &parts, None, false);
        let doc = XlsxParser::default()
            .parse_bytes(&bytes, "three.xlsx", &[])
            .expect("parse");
        assert_eq!(doc.chunks.len(), 3);
        for (i, chunk) in doc.chunks.iter().enumerate() {
            assert_eq!(
                chunk.heading.as_deref(),
                Some(format!("Sheet: S{}", i + 1).as_str())
            );
            assert!(
                chunk.content.contains(&format!("text-{}", i + 1)),
                "{:?}",
                chunk.content
            );
        }
        // The same workbook through the minimal builder reads the same as before this change.
        let minimal = make_minimal_xlsx(&[("A", &[&["x", "y"], &["z"]]), ("B", &[&["w"]])]);
        let doc = XlsxParser::default()
            .parse_bytes(&minimal, "m.xlsx", &[])
            .expect("parse");
        assert_eq!(doc.chunks.len(), 2);
        assert!(
            doc.chunks[0].content.contains("x\ty\nz"),
            "{:?}",
            doc.chunks[0].content
        );
    }

    /// Pins the copy of calamine 0.37.0's sheet-to-entry rules in [`resolve_sheet_parts`]:
    /// a non-standard xl_path, relative / absolute / differently cased targets, and the
    /// fallback when the resolved names disagree with calamine's.
    #[test]
    fn resolve_sheet_parts_follows_calamine_rules() {
        let sheets = owned_pairs([
            ("Rel".to_string(), "rId1".to_string()),
            ("Abs".to_string(), "rId2".to_string()),
            ("Case".to_string(), "rId3".to_string()),
            ("Other".to_string(), "rId4".to_string()),
        ]);
        let rels = owned_pairs([
            ("rId1".to_string(), "sheets/a.xml".to_string()),
            ("rId2".to_string(), "/custom/sheets/a.xml".to_string()),
            ("rId3".to_string(), "Sheets/A.XML".to_string()),
            ("rId4".to_string(), "sheets/b.xml".to_string()),
        ]);
        let bytes = make_wired_xlsx(
            "custom/",
            &sheets,
            &rels,
            &[
                ("custom/sheets/a.xml".to_string(), one_cell_worksheet("a")),
                ("custom/sheets/b.xml".to_string(), one_cell_worksheet("b")),
            ],
            None,
            false,
        );
        let archive = zip::ZipArchive::new(Cursor::new(bytes.as_slice())).unwrap();
        let a = archive.index_for_name("custom/sheets/a.xml");
        let b = archive.index_for_name("custom/sheets/b.xml");
        assert!(a.is_some() && b.is_some() && a != b);

        let names: Vec<String> = sheets.iter().map(|(n, _)| n.clone()).collect();
        assert_eq!(resolve_sheet_parts(&bytes, &names), Some(vec![a, a, a, b]));
        // calamine agrees on the names (the copy is checked against them at run time).
        let xlsx = calamine::Xlsx::new(Cursor::new(bytes.as_slice())).unwrap();
        assert_eq!(xlsx.sheet_names(), names);
        let doc = XlsxParser::default()
            .parse_bytes(&bytes, "custom.xlsx", &[])
            .expect("parse");
        assert_eq!(doc.chunks.len(), 2, "a once, b once");

        let disagreeing = vec!["Rel".to_string()];
        assert_eq!(
            resolve_sheet_parts(&bytes, &disagreeing),
            None,
            "names that disagree with calamine's fall back to reading every sheet"
        );
    }

    /// AX-03 P1: one row whose cells all reference the same shared string grew past the
    /// sheet cap because the cap was only checked between rows. It is checked per cell, so
    /// the overshoot is at most one cell.
    #[test]
    fn test_xlsx_repeated_shared_string_refs_in_one_row_stop_at_the_sheet_cap() {
        const CELL: usize = 200;
        const CAP: usize = 1000;
        let long = "s".repeat(CELL);
        let row: String = (0..200).map(|_| r#"<c t="s"><v>0</v></c>"#).collect();
        let bytes = make_wired_xlsx(
            "xl/",
            &owned_pairs([("S".to_string(), "rId1".to_string())]),
            &owned_pairs([("rId1".to_string(), "worksheets/sheet1.xml".to_string())]),
            &[(
                "xl/worksheets/sheet1.xml".to_string(),
                worksheet_xml(&format!(r#"<row r="1">{row}</row>"#)),
            )],
            Some(&shared_strings_xml(
                r#"count="200" uniqueCount="1""#,
                &[&long],
            )),
            false,
        );
        let doc = parse_workbook_bytes_capped(&bytes, "row.xlsx", WorkbookFormat::Xlsx, CAP)
            .expect("parse");
        let content = &doc.chunks[0].content;
        assert!(content.contains(&long));
        assert!(
            content.len() <= CAP + CELL + 1,
            "the overshoot is bounded by one cell, got len={}",
            content.len()
        );
    }

    /// AX-03 P2: the same row against a document budget below what it expands to is refused
    /// whole, through the public parser (preflight passes: the archive itself is small).
    #[test]
    fn test_xlsx_repeated_shared_string_refs_in_one_row_refuse_past_the_document_budget() {
        let long = "s".repeat(2000);
        let row: String = (0..200).map(|_| r#"<c t="s"><v>0</v></c>"#).collect();
        let bytes = make_wired_xlsx(
            "xl/",
            &owned_pairs([("S".to_string(), "rId1".to_string())]),
            &owned_pairs([("rId1".to_string(), "worksheets/sheet1.xml".to_string())]),
            &[(
                "xl/worksheets/sheet1.xml".to_string(),
                worksheet_xml(&format!(r#"<row r="1">{row}</row>"#)),
            )],
            Some(&shared_strings_xml(
                r#"count="200" uniqueCount="1""#,
                &[&long],
            )),
            false,
        );
        let budget = 16 * 1024;
        preflight_xlsx_decompression_budget_capped(&bytes, "row.xlsx", budget)
            .expect("premise: the archive fits the budget");
        let err = XlsxParser::with_budget(budget)
            .parse_bytes(&bytes, "row.xlsx", &[])
            .expect_err("400 KB of text from a 16 KiB budget is refused");
        let msg = err.to_string();
        assert!(
            msg.contains("decompression-bomb guard") && msg.contains("max_decompressed_size"),
            "{msg}"
        );
        assert!(msg.is_ascii(), "{msg}");
    }

    fn three_small_sheets() -> Vec<u8> {
        let sheets = owned_pairs((1..=3).map(|i| (format!("S{i}"), format!("rId{i}"))));
        let rels =
            owned_pairs((1..=3).map(|i| (format!("rId{i}"), format!("worksheets/sheet{i}.xml"))));
        let parts: Vec<(String, Vec<u8>)> = (1..=3)
            .map(|i| {
                (
                    format!("xl/worksheets/sheet{i}.xml"),
                    one_cell_worksheet("aaaa"),
                )
            })
            .collect();
        make_wired_xlsx("xl/", &sheets, &rels, &parts, None, false)
    }

    fn limits(doc_text_cap: usize, timeout: std::time::Duration) -> XlsxLimits {
        XlsxLimits {
            sheet_max_bytes: SHEET_MAX_BYTES,
            decompressed_budget: crate::parser::DEFAULT_MAX_DECOMPRESSED_BYTES,
            doc_text_cap,
            timeout,
        }
    }

    /// AX-03 P2: three sheets of `"aaaa\n"` (5 bytes each) fit a 15-byte document budget
    /// exactly (the boundary test in [`crate::parser::pdf`] has the same shape).
    #[test]
    fn test_xlsx_document_text_budget_admits_exactly_the_budget() {
        let doc = parse_xlsx_bytes_limited(
            &three_small_sheets(),
            "three.xlsx",
            limits(15, super::super::pdf::PDF_DOC_EXTRACT_TIMEOUT),
        )
        .expect("15 bytes of text fit a 15-byte budget");
        assert_eq!(doc.chunks.len(), 3);
    }

    /// ...and one byte less is refused, naming the budget and how to raise it.
    #[test]
    fn test_xlsx_document_text_budget_refuses_past_the_budget() {
        let err = parse_xlsx_bytes_limited(
            &three_small_sheets(),
            "three.xlsx",
            limits(14, super::super::pdf::PDF_DOC_EXTRACT_TIMEOUT),
        )
        .expect_err("15 bytes of text do not fit 14");
        assert_eq!(
            err.to_string(),
            "three.xlsx: extracted text exceeds 14 bytes across 3 sheet(s) (decompression-bomb guard; raise [index].max_decompressed_size to admit this file)"
        );
    }

    /// The wall-clock backstop: a zero timeout stops the extraction with the guard's error.
    #[test]
    fn test_xlsx_time_budget_stops_extraction() {
        let err = parse_xlsx_bytes_limited(
            &three_small_sheets(),
            "slow.xlsx",
            limits(usize::MAX, std::time::Duration::ZERO),
        )
        .expect_err("a zero timeout has always passed");
        let msg = err.to_string();
        assert!(
            msg.contains("exceeded") && msg.contains("decompression-bomb guard"),
            "{msg}"
        );
        assert!(msg.is_ascii(), "{msg}");
    }

    /// Codex review of 3429cc7: the time limit also covers the decompression preflight, which
    /// inflates every entry before calamine opens anything. A zero timeout stops it there.
    #[test]
    fn test_xlsx_time_budget_covers_the_preflight() {
        let err = parse_xlsx_bytes_limited(
            &three_small_sheets(),
            "slow.xlsx",
            limits(usize::MAX, std::time::Duration::ZERO),
        )
        .expect_err("a zero timeout has always passed");
        let msg = err.to_string();
        assert!(
            msg.contains("preflight") && msg.contains("decompression-bomb guard"),
            "the preflight is where the time runs out first: {msg}"
        );
        assert!(msg.is_ascii(), "{msg}");
    }

    /// Codex review of 3429cc7: two sheets with different names on one part are read once,
    /// as one chunk whose heading names both, so both names still find the text.
    #[test]
    fn test_xlsx_sheets_with_different_names_on_one_part_share_one_chunk_named_after_both() {
        let bytes = make_wired_xlsx(
            "xl/",
            &owned_pairs([
                ("Alpha".to_string(), "rId1".to_string()),
                ("Beta".to_string(), "rId2".to_string()),
            ]),
            &owned_pairs([
                ("rId1".to_string(), "worksheets/sheet1.xml".to_string()),
                ("rId2".to_string(), "worksheets/sheet1.xml".to_string()),
            ]),
            &[(
                "xl/worksheets/sheet1.xml".to_string(),
                one_cell_worksheet("shared-text"),
            )],
            None,
            false,
        );
        let doc = XlsxParser::default()
            .parse_bytes(&bytes, "alias.xlsx", &[])
            .expect("parse");
        assert_eq!(doc.chunks.len(), 1);
        let chunk = &doc.chunks[0];
        assert_eq!(chunk.heading.as_deref(), Some("Sheet: Alpha, Beta"));
        assert_eq!(
            chunk.context.as_deref(),
            Some("alias > Sheet: Alpha, Beta"),
            "the breadcrumb that is embedded with the text carries both names"
        );
        assert!(chunk.content.contains("shared-text"));
    }

    /// ...and a crafted workbook naming one part from 200 sheets lists the names up to the
    /// label cap, then only how many more there are.
    #[test]
    fn test_xlsx_many_names_on_one_part_are_capped_in_the_heading() {
        let bytes = sheets_sharing_one_part(200, true);
        let doc = XlsxParser::default()
            .parse_bytes(&bytes, "many.xlsx", &[])
            .expect("parse");
        assert_eq!(doc.chunks.len(), 1);
        let heading = doc.chunks[0].heading.as_deref().unwrap();
        let label = heading.strip_prefix("Sheet: ").expect(heading);
        let (listed, more) = label
            .rsplit_once(", and ")
            .unwrap_or_else(|| panic!("the rest is counted: {heading}"));
        let more: usize = more
            .strip_suffix(" more")
            .and_then(|n| n.parse().ok())
            .unwrap_or_else(|| panic!("{heading}"));
        let listed: Vec<&str> = listed.split(", ").collect();
        assert_eq!(listed[0], "S0", "the sheet that was read comes first");
        assert_eq!(listed.len() + more, 200, "{heading}");
        assert!(listed.len() > 1, "{heading}");
        assert!(
            listed.join(", ").len() <= SHARED_PART_LABEL_MAX_BYTES,
            "{heading}"
        );
    }

    /// The scanner reads `uniqueCount` whatever the read boundaries: every split of the input
    /// into two feeds, and one byte at a time, gives the same value.
    #[test]
    fn unique_count_scan_reads_across_feed_boundaries() {
        let cases: &[(&[u8], u64)] = &[
            (br#"<sst uniqueCount="12345">"#, 12345),
            (
                br#"<sst uniqueCount="000000100000000000">"#,
                100_000_000_000,
            ),
            (br#"<sst uniqueCount='42'>"#, 42),
            (b"<sst uniqueCount \n=\t \"9\">", 9),
            (br#"<sst uniquuniqueCount="7">"#, 7),
            (br#"<sst uniqueCount="12a">"#, 12),
            (
                br#"<sst uniqueCount="99999999999999999999999999">"#,
                u64::MAX,
            ),
            (br#"<sst uniqueCount=12>"#, 0),
            (br#"<sst uniqueCountX="12">"#, 0),
            (br#"<sst count="5">"#, 0),
        ];
        for &(input, expected) in cases {
            for split in 0..=input.len() {
                let mut scan = UniqueCountScan::default();
                scan.feed(&input[..split]);
                scan.feed(&input[split..]);
                scan.finish();
                assert_eq!(scan.largest, expected, "{input:?} split at {split}");
            }
            let mut scan = UniqueCountScan::default();
            for byte in input {
                scan.feed(std::slice::from_ref(byte));
            }
            scan.finish();
            assert_eq!(scan.largest, expected, "{input:?} byte by byte");
        }
    }

    /// The sharedStrings part [`workbook_with_unique_count`] writes (stored, so its entry size
    /// is this string's length).
    fn shared_strings_with_unique_count(count_attr: &str, padding: usize) -> String {
        format!(
            r#"<?xml version="1.0"?><!--{}--><sst {SPREADSHEET_NS} {count_attr}><si><t>x</t></si></sst>"#,
            "p".repeat(padding)
        )
    }

    fn workbook_with_unique_count(count_attr: &str, padding: usize) -> Vec<u8> {
        let shared = shared_strings_with_unique_count(count_attr, padding);
        make_wired_xlsx(
            "xl/",
            &owned_pairs([("S".to_string(), "rId1".to_string())]),
            &owned_pairs([("rId1".to_string(), "worksheets/sheet1.xml".to_string())]),
            &[(
                "xl/worksheets/sheet1.xml".to_string(),
                worksheet_xml(r#"<row r="1"><c r="A1" t="s"><v>0</v></c></row>"#),
            )],
            Some(shared.as_bytes()),
            true,
        )
    }

    /// AX-41: calamine reserves `uniqueCount` entries before it reads one, so a 1.7 KB file
    /// declaring 100,000,000,000 aborted the process. The preflight refuses it in every
    /// spelling calamine accepts, and where the attribute straddles the 8 KiB read buffer.
    #[test]
    fn test_xlsx_preflight_rejects_a_unique_count_beyond_the_budget() {
        let cap = crate::parser::DEFAULT_MAX_DECOMPRESSED_BYTES;
        for attr in [
            r#"uniqueCount="100000000000""#,
            r#"uniqueCount="000000100000000000""#,
            r#"uniqueCount='100000000000'"#,
            r#"uniqueCount = "100000000000""#,
        ] {
            let bytes = workbook_with_unique_count(attr, 0);
            let err = preflight_xlsx_decompression_budget_capped(&bytes, "sst.xlsx", cap)
                .expect_err(attr);
            let msg = err.to_string();
            assert!(
                msg.contains("uniqueCount 100000000000") && msg.contains("zip-bomb guard"),
                "{msg}"
            );
            assert!(
                !msg.contains("raise [index]"),
                "the limit follows the entry, not the budget: {msg}"
            );
            assert!(msg.is_ascii(), "{msg}");
        }
        // Stored entries are read in 8 KiB pieces: put the attribute across that boundary.
        let prefix = r#"<?xml version="1.0"?><!--"#.len();
        let to_attr = format!(r#"--><sst {SPREADSHEET_NS} "#).len();
        for offset in 8 * 1024 - 14..=8 * 1024 + 1 {
            let padding = offset - prefix - to_attr;
            let bytes = workbook_with_unique_count(r#"uniqueCount="100000000000""#, padding);
            preflight_xlsx_decompression_budget_capped(&bytes, "sst.xlsx", cap)
                .expect_err("an attribute across the read boundary is still read");
        }
    }

    /// The whole parser returns that refusal instead of aborting: this fixture is the one that
    /// took the process down (a regression would abort this test binary).
    #[test]
    fn test_xlsx_refuses_a_unique_count_that_would_abort_the_process() {
        let bytes = workbook_with_unique_count(r#"uniqueCount="100000000000""#, 0);
        let err = XlsxParser::default()
            .parse_bytes(&bytes, "abort.xlsx", &[])
            .expect_err("refused before calamine reserves");
        assert!(err.to_string().contains("uniqueCount"), "{err}");
    }

    /// Review of 5b965f9: calamine skips any ASCII whitespace around `=` (`is_ascii_whitespace`,
    /// which includes the form feed), so the scanner must too.
    #[test]
    fn unique_count_scan_treats_a_form_feed_as_whitespace() {
        for input in [
            &b"<sst uniqueCount\x0C=\"100000000000\">"[..],
            &b"<sst uniqueCount=\x0C\"100000000000\">"[..],
        ] {
            let mut scan = UniqueCountScan::default();
            scan.feed(input);
            scan.finish();
            assert_eq!(scan.largest, 100_000_000_000, "{input:?}");
        }
    }

    /// ...and the preflight refuses both spellings (each would abort the process in calamine).
    #[test]
    fn test_xlsx_preflight_rejects_a_unique_count_spelled_with_a_form_feed() {
        let cap = crate::parser::DEFAULT_MAX_DECOMPRESSED_BYTES;
        for attr in [
            "uniqueCount\u{c}=\"100000000000\"",
            "uniqueCount=\u{c}\"100000000000\"",
        ] {
            let bytes = workbook_with_unique_count(attr, 0);
            let err =
                preflight_xlsx_decompression_budget_capped(&bytes, "ff.xlsx", cap).expect_err(attr);
            assert!(
                err.to_string().contains("uniqueCount 100000000000"),
                "{err}"
            );
        }
    }

    /// Review of 5b965f9: the limit follows the entry that declares the count, not the
    /// budget -- a sharedStrings part of `n` bytes holds at most `n / 5` strings, whatever
    /// `[index].max_decompressed_size` says. Exactly `n / 5` is admitted, one more refused,
    /// under the default budget and under `"unlimited"` alike.
    #[test]
    fn test_xlsx_unique_count_limit_follows_the_entry_that_declares_it() {
        // Three-digit counts keep the part the same length for `at` and `over`.
        const AT: usize = 500;
        let base = shared_strings_with_unique_count(&format!(r#"uniqueCount="{AT}""#), 0).len();
        let padding = AT * 5 - base;
        assert_eq!(
            shared_strings_with_unique_count(&format!(r#"uniqueCount="{AT}""#), padding).len(),
            AT * 5,
            "premise: the part is exactly 5 bytes per declared string"
        );
        let at = workbook_with_unique_count(&format!(r#"uniqueCount="{AT}""#), padding);
        let over = workbook_with_unique_count(&format!(r#"uniqueCount="{}""#, AT + 1), padding);
        for cap in [
            crate::parser::DEFAULT_MAX_DECOMPRESSED_BYTES,
            crate::parser::FileSizeLimit::Unlimited.cap_bytes(),
        ] {
            preflight_xlsx_decompression_budget_capped(&at, "at.xlsx", cap)
                .expect("a count of exactly the part's bytes / 5 is admitted");
            let err = preflight_xlsx_decompression_budget_capped(&over, "over.xlsx", cap)
                .expect_err("one more than the part can hold is refused");
            let msg = err.to_string();
            assert!(
                msg.contains("uniqueCount 501") && msg.contains("zip-bomb guard"),
                "{msg}"
            );
            assert!(!msg.contains("raise [index]"), "raising cannot help: {msg}");
            assert!(msg.is_ascii(), "{msg}");
        }
        XlsxParser::default()
            .parse_bytes(&at, "at.xlsx", &[])
            .expect("the index path reads it");
        XlsxParser::default()
            .parse_bytes_for_read(&over, "over.xlsx", &[])
            .expect_err("the read path refuses it too");
    }

    /// A truthful `uniqueCount` is admitted and the shared strings read as before.
    #[test]
    fn test_xlsx_preflight_admits_a_truthful_unique_count() {
        let bytes = make_wired_xlsx(
            "xl/",
            &owned_pairs([("S".to_string(), "rId1".to_string())]),
            &owned_pairs([("rId1".to_string(), "worksheets/sheet1.xml".to_string())]),
            &[(
                "xl/worksheets/sheet1.xml".to_string(),
                worksheet_xml(
                    r#"<row r="1"><c r="A1" t="s"><v>0</v></c><c r="B1" t="s"><v>2</v></c></row>"#,
                ),
            )],
            Some(&shared_strings_xml(
                r#"count="3" uniqueCount="3""#,
                &["alpha", "beta", "gamma"],
            )),
            false,
        );
        preflight_xlsx_decompression_budget(&bytes, "ok.xlsx").expect("truthful count");
        let doc = XlsxParser::default()
            .parse_bytes(&bytes, "ok.xlsx", &[])
            .expect("parse");
        assert!(
            doc.chunks[0].content.contains("alpha\tgamma"),
            "{:?}",
            doc.chunks[0].content
        );
    }

    /// `"unlimited"` lifts no part of the `uniqueCount` limit (it follows the entry), on the
    /// index path, and the read path (MCP `get_document`) refuses the same file.
    #[test]
    fn test_xlsx_unique_count_limit_holds_under_unlimited_and_on_the_read_path() {
        let unlimited = crate::parser::FileSizeLimit::Unlimited.cap_bytes();

        let over = workbook_with_unique_count(r#"uniqueCount="10485761""#, 0);
        let err = XlsxParser::with_budget(unlimited)
            .parse_bytes(&over, "over.xlsx", &[])
            .expect_err("unlimited still has a uniqueCount limit");
        let msg = err.to_string();
        assert!(msg.contains("uniqueCount 10485761"), "{msg}");
        assert!(
            !msg.contains("raise [index]"),
            "raising does not help under unlimited: {msg}"
        );
        XlsxParser::default()
            .parse_bytes_for_read(&over, "over.xlsx", &[])
            .expect_err("the read path refuses it too");
    }

    /// `bytes` with the entry `name` rewritten to `body` (the other entries copied raw).
    fn replace_entry(bytes: &[u8], name: &str, body: &[u8]) -> Vec<u8> {
        let mut source = zip::ZipArchive::new(Cursor::new(bytes)).unwrap();
        let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
        for i in 0..source.len() {
            let entry = source.by_index_raw(i).unwrap();
            if entry.name().unwrap() == name {
                drop(entry);
                zip.start_file(name, SimpleFileOptions::default()).unwrap();
                zip.write_all(body).unwrap();
            } else {
                zip.raw_copy_file(entry).unwrap();
            }
        }
        zip.finish().unwrap().into_inner()
    }

    /// Review of 5b965f9: when [`resolve_sheet_parts`] cannot vouch for its copy (here a
    /// `<sheet>` inside `<definedName>`, which calamine reads as the name's text and the copy
    /// counts as a sheet), every sheet is read -- and the document budget still holds.
    #[test]
    fn test_xlsx_document_budget_holds_when_the_sheet_parts_cannot_be_resolved() {
        const SHEETS: usize = 200;
        let sheets = owned_pairs((0..SHEETS).map(|i| (format!("S{i}"), "rId1".to_string())));
        let rels = owned_pairs([("rId1".to_string(), "worksheets/sheet1.xml".to_string())]);
        let wired = make_wired_xlsx(
            "xl/",
            &sheets,
            &rels,
            &[(
                "xl/worksheets/sheet1.xml".to_string(),
                one_cell_worksheet("aaaa"),
            )],
            None,
            false,
        );
        let mut workbook = format!(r#"<?xml version="1.0"?><workbook {SPREADSHEET_NS}><sheets>"#);
        for (i, (name, rid)) in sheets.iter().enumerate() {
            workbook.push_str(&format!(
                r#"<sheet name="{name}" sheetId="{}" r:id="{rid}"/>"#,
                i + 1
            ));
        }
        workbook.push_str(r#"</sheets><definedNames><definedName name="n"><sheet name="Ghost" sheetId="999" r:id="rId1"/></definedName></definedNames></workbook>"#);
        let bytes = replace_entry(&wired, "xl/workbook.xml", workbook.as_bytes());

        let names: Vec<String> = sheets.iter().map(|(n, _)| n.clone()).collect();
        let xlsx = calamine::Xlsx::new(Cursor::new(bytes.as_slice())).unwrap();
        assert_eq!(
            xlsx.sheet_names(),
            names,
            "premise: calamine skips the ghost"
        );
        assert_eq!(
            resolve_sheet_parts(&bytes, &names),
            None,
            "premise: the copy sees one sheet more, so it gives up"
        );

        let doc = parse_xlsx_bytes_limited(
            &bytes,
            "ghost.xlsx",
            limits(usize::MAX, super::super::pdf::PDF_DOC_EXTRACT_TIMEOUT),
        )
        .expect("parse");
        assert_eq!(doc.chunks.len(), SHEETS, "the fallback reads every sheet");

        // Each sheet yields "aaaa\n" (5 bytes): a 50-byte budget stops the 11th.
        let err = parse_xlsx_bytes_limited(
            &bytes,
            "ghost.xlsx",
            limits(50, super::super::pdf::PDF_DOC_EXTRACT_TIMEOUT),
        )
        .expect_err("the document budget does not depend on the copy");
        assert_eq!(
            err.to_string(),
            "ghost.xlsx: extracted text exceeds 50 bytes across 11 sheet(s) (decompression-bomb guard; raise [index].max_decompressed_size to admit this file)"
        );
    }

    /// Review of 5b965f9: sheets whose parts hold no cell never reach the per-cell check, so
    /// the time limit is also checked at each sheet boundary.
    #[test]
    fn test_xlsx_time_budget_is_checked_between_sheets_that_hold_no_cell() {
        let sheets = owned_pairs((1..=3).map(|i| (format!("S{i}"), format!("rId{i}"))));
        let rels =
            owned_pairs((1..=3).map(|i| (format!("rId{i}"), format!("worksheets/sheet{i}.xml"))));
        let parts: Vec<(String, Vec<u8>)> = (1..=3)
            .map(|i| (format!("xl/worksheets/sheet{i}.xml"), worksheet_xml("")))
            .collect();
        let bytes = make_wired_xlsx("xl/", &sheets, &rels, &parts, None, false);
        let doc = parse_xlsx_bytes_limited(
            &bytes,
            "empty.xlsx",
            limits(usize::MAX, super::super::pdf::PDF_DOC_EXTRACT_TIMEOUT),
        )
        .expect("premise: the workbook reads, with no chunk");
        assert!(doc.chunks.is_empty());
        // Past the preflight (which a zero timeout stops first, see
        // `test_xlsx_time_budget_covers_the_preflight`), the sheets are where the time is seen.
        let zero = limits(usize::MAX, std::time::Duration::ZERO);
        let guard = DocumentGuard {
            path_hint: "empty.xlsx",
            used: 0,
            cap: zero.doc_text_cap,
            started: std::time::Instant::now(),
            timeout: zero.timeout,
            sheets: 0,
        };
        let err = extract_xlsx_workbook(&bytes, "empty.xlsx", zero, guard)
            .expect_err("the sheet boundary checks the time");
        assert!(
            err.to_string()
                .contains("xlsx text extraction exceeded 0 s after 0 sheet(s)"),
            "{err}"
        );
    }

    /// The read path goes through the same per-document guards: a workbook whose sheets all
    /// share one part reads as one chunk there too.
    #[test]
    fn xlsx_read_path_reads_a_shared_part_once() {
        let bytes = sheets_sharing_one_part(50, true);
        let doc = XlsxParser::default()
            .parse_bytes_for_read(&bytes, "dup.xlsx", &[])
            .expect("parse");
        assert_eq!(doc.chunks.len(), 1);
    }
}
