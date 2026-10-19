//! pptx (`.pptx`) parser. zip + quick-xml でスライド単位に `ppt/slides/slideN.xml`
//! を読む。スライド順は `ppt/presentation.xml` の `<p:sldIdLst>` (可視順) を
//! `ppt/_rels/presentation.xml.rels` で解決したものを優先し、読めない場合のみ
//! ファイル番号ソートにフォールバックする。
use std::collections::HashMap;
use std::io::Cursor;

use anyhow::{Result, anyhow};
use quick_xml::events::Event;
use quick_xml::reader::Reader;

use super::ooxml::RawRowSpan;
use super::{Chunk, ParsedDocument, Parser, single_text_chunk};

pub struct PptxParser {
    /// (feature-61) 文書 1 本の展開後合計の上限 (`[index].max_decompressed_size`)。
    decompressed_budget: u64,
}

impl PptxParser {
    /// (feature-61) 展開 budget を `budget` bytes にした parser。[`super::Registry`] が
    /// `[index].max_decompressed_size` から作る。
    pub fn with_budget(budget: u64) -> Self {
        Self {
            decompressed_budget: budget,
        }
    }
}

impl Default for PptxParser {
    /// 展開 budget = [`super::DEFAULT_MAX_DECOMPRESSED_BYTES`]。
    fn default() -> Self {
        Self::with_budget(super::DEFAULT_MAX_DECOMPRESSED_BYTES)
    }
}

impl Parser for PptxParser {
    fn extension(&self) -> &'static str {
        "pptx"
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
        parse_bytes_impl(bytes, path_hint, self.decompressed_budget)
    }

    /// (feature-61) The read path keeps the built-in budget whatever `[index]`
    /// set this parser up with ([`super::Parser::parse_bytes_for_read_inner`]).
    /// Same parse as [`super::Parser::parse_bytes_inner`], other `cap`.
    fn parse_bytes_for_read_inner(
        &self,
        bytes: &[u8],
        path_hint: &str,
        _exclude_headings: &[&str],
    ) -> Result<ParsedDocument> {
        parse_bytes_impl(bytes, path_hint, super::DEFAULT_MAX_DECOMPRESSED_BYTES)
    }
}

// ---------------------------------------------------------------------------
// ppt/slides/slideN.xml (+ ppt/notesSlides/notesSlideN.xml) → slide-wise chunks
// ---------------------------------------------------------------------------

fn parse_bytes_impl(bytes: &[u8], path_hint: &str, cap: u64) -> Result<ParsedDocument> {
    let mut zip = zip::ZipArchive::new(Cursor::new(bytes))
        .map_err(|e| anyhow!("{path_hint}: cannot open pptx zip (corrupt or encrypted): {e}"))?;

    // 文書単位の累積展開済みバイト数。この pptx から読む全エントリ (slides /
    // notes / rels / presentation.xml / core.xml) で共有し、累積が cap を
    // 超えたら Err にする (codex P2, PR #70 round 2 zip-bomb hardening:
    // 個々のエントリが cap 未満でも積算で無制限に膨らむのを防ぐ)。
    let mut budget: u64 = 0;

    // frontmatter を先に取得 (context の title に使う。budget は cumulative なので順序自由)
    let frontmatter = super::ooxml::core_xml_frontmatter(&mut zip, path_hint, &mut budget, cap)?;
    let title = frontmatter.title.as_deref().unwrap_or("").to_string();

    // slide エントリをファイル番号順に集める (zip 内順序非依存。
    // presentation.xml が読めない場合のフォールバック用)。
    // zip 9 の `file_names()` は名前ごとに `ZipResult` を返す。名前として読めない
    // entry は slide でない名前と同じく飛ばす。
    let mut slide_nums: Vec<usize> = zip
        .file_names()
        .filter_map(|n| slide_number(&n.ok()?, "ppt/slides/slide", ".xml"))
        .collect();
    slide_nums.sort_unstable();

    // codex P2 (PR #70 round 1): ファイル番号ソートは、並べ替え/削除された
    // deck では presentation.xml の可視順 (sldIdLst) とズレる。可視順が
    // 解決できればそちらを優先し、`Slide {n}` ラベルは可視順の 1-origin
    // 連番 (= 実際のファイル番号ではない) とする。読めない/非標準な zip
    // では file 番号ソートにフォールバックする (index 全体を諦めさせない)。
    let ordered_file_nums =
        resolve_visible_slide_order(&mut zip, &mut budget, cap, path_hint)?.unwrap_or(slide_nums);

    let mut chunks = Vec::new();
    for (display_idx, file_n) in ordered_file_nums.into_iter().enumerate() {
        let display_n = display_idx + 1;
        let slide_xml = match super::ooxml::read_zip_part(
            &mut zip,
            path_hint,
            &format!("ppt/slides/slide{file_n}.xml"),
            &mut budget,
            cap,
        )? {
            Some(b) => b,
            None => continue,
        };
        let (title_opt, body, spans) = parse_slide_xml(
            &slide_xml,
            path_hint,
            &format!("ppt/slides/slide{file_n}.xml"),
        );
        let mut content = body;
        // notes: `ppt/slides/_rels/slide{file_n}.xml.rels` の notesSlide
        // relationship を解決する (dry-run (Task 3.7) で同番号 heuristic の
        // 誤帰属が実証されたため廃止。rels が無い / notesSlide relationship
        // が無い slide は notes なしとし、フォールバック heuristic はしない
        // — 誤帰属ゼロを優先する)。notes の対応はスライドの実ファイル番号
        // (`file_n`) で引く (表示順の連番 `display_n` ではない)。
        let notes_path = super::ooxml::read_zip_part(
            &mut zip,
            path_hint,
            &format!("ppt/slides/_rels/slide{file_n}.xml.rels"),
            &mut budget,
            cap,
        )?
        .and_then(|rels_xml| {
            resolve_notes_path(
                &rels_xml,
                path_hint,
                &format!("ppt/slides/_rels/slide{file_n}.xml.rels"),
            )
        });
        if let Some(path) = notes_path
            && let Some(notes_xml) =
                super::ooxml::read_zip_part(&mut zip, path_hint, &path, &mut budget, cap)?
        {
            let notes_text = collect_a_t(&notes_xml, path_hint, &path);
            if !notes_text.trim().is_empty() {
                content.push_str("\n\n[notes]\n");
                content.push_str(notes_text.trim());
            }
        }
        if content.trim().is_empty() && title_opt.is_none() {
            continue;
        }
        let heading = match &title_opt {
            Some(t) if !t.trim().is_empty() => format!("Slide {display_n}: {}", t.trim()),
            _ => format!("Slide {display_n}"),
        };
        let context = super::build_context(&[title.as_str(), &heading]);
        // (feature-65, R1.10) Trim every whitespace but the tab, so a table row at either end of
        // the slide keeps its empty fields. A slide without a table cannot start or end with a
        // tab: every paragraph is trimmed before it is added (`parse_slide_xml`) and the notes
        // are trimmed too, so its content is what `trim()` gave.
        // (feature-66) The notes come after the body, so they move no span; the spans move by
        // what the trim takes off the head ([`rows_after_trim`]).
        let blank = |c: char| c.is_whitespace() && c != '\t';
        let lead = content.len() - content.trim_start_matches(blank).len();
        let kept = content.trim_matches(blank).to_string();
        let table_rows = super::ooxml::chunk_table_rows(&rows_after_trim(spans, lead, kept.len()));
        chunks.push(Chunk {
            index: chunks.len(),
            heading: Some(heading),
            level: Some(2),
            content: kept,
            context,
            line_range: None,
            symbol_kind: None,
            table_rows,
        });
    }

    let raw_content = super::join_chunk_bodies(&chunks);
    Ok(ParsedDocument {
        frontmatter,
        chunks,
        raw_content,
        frontmatter_error: None,
    })
}

/// `ppt/slides/slide12.xml` から prefix/suffix を除いた整数 12 を取る。
fn slide_number(name: &str, prefix: &str, suffix: &str) -> Option<usize> {
    let mid = name.strip_prefix(prefix)?.strip_suffix(suffix)?;
    mid.parse::<usize>().ok()
}

/// `*.rels` の `<Relationship Id="..." Type="..." Target="...">` を全て
/// `(Id, Type, Target)` として出現順に列挙する。`ppt/slides/_rels/slideN.xml.rels`
/// (notes 解決) と `ppt/_rels/presentation.xml.rels` (可視順解決) の両方が
/// 同じ rels XML 構造を読むため、この parser を共通化する (重複実装を避ける)。
/// いずれかの属性が欠けている `<Relationship>` はスキップする。
fn parse_relationships(
    rels_xml: &[u8],
    path_hint: &str,
    part: &str,
) -> Vec<(String, String, String)> {
    super::ooxml::warn_if_truncated(path_hint, part, rels_xml);
    let mut reader = Reader::from_reader(rels_xml);
    reader.config_mut().trim_text(true);
    let mut buf = Vec::new();
    let mut out = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => {
                if super::ooxml_local(e.name().as_ref()) != b"Relationship" {
                    continue;
                }
                let mut id: Option<String> = None;
                let mut rel_type: Option<String> = None;
                let mut target: Option<String> = None;
                for attr in e.attributes().flatten() {
                    match super::ooxml_local(attr.key.as_ref()) {
                        b"Id" => id = Some(String::from_utf8_lossy(&attr.value).into_owned()),
                        b"Type" => {
                            rel_type = Some(String::from_utf8_lossy(&attr.value).into_owned());
                        }
                        b"Target" => {
                            target = Some(String::from_utf8_lossy(&attr.value).into_owned());
                        }
                        _ => {}
                    }
                }
                if let (Some(id), Some(rel_type), Some(target)) = (id, rel_type, target) {
                    out.push((id, rel_type, target));
                }
            }
            Ok(Event::Eof) => break,
            // 切れている場合の警告は `warn_if_truncated` (関数冒頭の 1 パス) が出す。
            Err(_) => break,
            _ => {}
        }
        buf.clear();
    }
    out
}

/// `ppt/slides/_rels/slideN.xml.rels` から notesSlide relationship
/// (`Type` が `.../relationships/notesSlide` で終わる `<Relationship>`) の
/// `Target` を解決し、zip 内の絶対パス (例: `ppt/notesSlides/notesSlide1.xml`)
/// を返す。該当する relationship が無ければ None (= 呼び出し側は notes なし
/// として扱う。同番号 heuristic へのフォールバックはしない)。
fn resolve_notes_path(rels_xml: &[u8], path_hint: &str, part: &str) -> Option<String> {
    parse_relationships(rels_xml, path_hint, part)
        .into_iter()
        .find(|(_, rel_type, _)| rel_type.ends_with("/notesSlide"))
        .map(|(_, _, target)| resolve_relative_target("ppt/slides", &target))
}

/// `ppt/presentation.xml` の `<p:sldIdLst>` (可視順) + `ppt/_rels/presentation.xml.rels`
/// (`r:id` → `ppt/slides/slideN.xml` 解決) から、可視順に並んだ slide のファイル
/// 番号列を返す。
///
/// codex P2 (PR #70 round 1): 旧実装は `ppt/slides/slideN.xml` のファイル番号
/// ソートで処理していたため、並べ替え/削除された deck では presentation.xml
/// の可視順とズレて `Slide {n}` ラベルが実際の表示順と食い違っていた。
///
/// presentation.xml / rels が読めない、`sldIdLst` が空、あるいは 1 件も
/// slide 番号へ解決できない場合は `Ok(None)` を返す (呼び出し側は file
/// 番号ソートにフォールバックする — 壊れた/非標準な zip で index 全体を
/// 諦めさせない)。`budget` の累積 cap 超過時は `Err` を返す (codex P2,
/// PR #70 round 2 zip-bomb hardening: `read_zip_entry` 参照)。
fn resolve_visible_slide_order(
    zip: &mut zip::ZipArchive<Cursor<&[u8]>>,
    budget: &mut u64,
    cap: u64,
    path_hint: &str,
) -> Result<Option<Vec<usize>>> {
    let Some(presentation_xml) =
        super::ooxml::read_zip_part(zip, path_hint, "ppt/presentation.xml", budget, cap)?
    else {
        return Ok(None);
    };
    let rids = parse_sld_id_list(&presentation_xml, path_hint);
    if rids.is_empty() {
        return Ok(None);
    }

    let Some(rels_xml) = super::ooxml::read_zip_part(
        zip,
        path_hint,
        "ppt/_rels/presentation.xml.rels",
        budget,
        cap,
    )?
    else {
        return Ok(None);
    };
    let rid_to_path: HashMap<String, String> =
        parse_relationships(&rels_xml, path_hint, "ppt/_rels/presentation.xml.rels")
            .into_iter()
            .map(|(id, _, target)| (id, resolve_relative_target("ppt", &target)))
            .collect();

    let order: Vec<usize> = rids
        .iter()
        .filter_map(|rid| rid_to_path.get(rid))
        .filter_map(|path| slide_number(path, "ppt/slides/slide", ".xml"))
        .collect();
    Ok(if order.is_empty() { None } else { Some(order) })
}

/// `ppt/presentation.xml` の `<p:sldIdLst>` 内 `<p:sldId r:id="rIdX" .../>` から
/// `r:id` の値を出現順 (= 可視順) に集める。
fn parse_sld_id_list(xml: &[u8], path_hint: &str) -> Vec<String> {
    super::ooxml::warn_if_truncated(path_hint, "ppt/presentation.xml", xml);
    let mut reader = Reader::from_reader(xml);
    reader.config_mut().trim_text(true);
    let mut buf = Vec::new();
    let mut in_list = false;
    let mut ids = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => match super::ooxml_local(e.name().as_ref()) {
                b"sldIdLst" => in_list = true,
                b"sldId" if in_list => {
                    if let Some(id) = r_id_attr(&e) {
                        ids.push(id);
                    }
                }
                _ => {}
            },
            Ok(Event::Empty(e)) => {
                if in_list
                    && super::ooxml_local(e.name().as_ref()) == b"sldId"
                    && let Some(id) = r_id_attr(&e)
                {
                    ids.push(id);
                }
            }
            Ok(Event::End(e)) => {
                if super::ooxml_local(e.name().as_ref()) == b"sldIdLst" {
                    in_list = false;
                }
            }
            Ok(Event::Eof) => break,
            // 切れている場合の警告は `warn_if_truncated` (関数冒頭の 1 パス) が出す。
            Err(_) => break,
            _ => {}
        }
        buf.clear();
    }
    ids
}

/// `<p:sldId id="256" r:id="rId1"/>` の relationship 参照属性 (`r:id`) の
/// 値を取る。
///
/// codex P2 (PR #70 round 4): 当初は生 QName `r:id` への厳密一致だったが、
/// relationships namespace は仕様上 prefix `r` に固定されているわけでは
/// なく、正規の XML でも別 prefix (`rel:id` 等) に bind され得る。
/// 「namespace prefix 付き (`:` を含む) かつ local name が `id`」という
/// 条件に緩和して受理する。plain `id` 属性 (slide の永続 ID、
/// relationship とは無関係) はコロンを含まないためこの条件で除外され、
/// 誤って relationship id として拾うことはない (前回 `r:id` 厳密一致に
/// した理由と両立)。完全な namespace URI 解決 (xmlns 宣言の追跡) までは
/// 行わない — prefix 付き `:id` という緩い一致で実用上十分と判断した。
fn r_id_attr(e: &quick_xml::events::BytesStart) -> Option<String> {
    e.attributes().flatten().find_map(|attr| {
        let key = attr.key.as_ref();
        let is_prefixed_id = key
            .iter()
            .position(|&b| b == b':')
            .is_some_and(|colon| &key[colon + 1..] == b"id");
        if is_prefixed_id {
            Some(String::from_utf8_lossy(&attr.value).into_owned())
        } else {
            None
        }
    })
}

/// `base_dir` (例: `"ppt/slides"`、末尾スラッシュ無し) を基準に OOXML の
/// relationship `Target` (例: `"../notesSlides/notesSlide1.xml"`) を解決し、
/// zip 内の絶対パス (例: `"ppt/notesSlides/notesSlide1.xml"`) を返す。
/// `Target` が `/` 始まりの絶対パス (`"/ppt/..."`) の場合は先頭の `/` を
/// 除いてそのまま使う (OPC 仕様上のパッケージルート基準パス)。
fn resolve_relative_target(base_dir: &str, target: &str) -> String {
    if let Some(abs) = target.strip_prefix('/') {
        return abs.to_string();
    }
    let mut stack: Vec<&str> = base_dir.split('/').filter(|s| !s.is_empty()).collect();
    for seg in target.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                stack.pop();
            }
            s => stack.push(s),
        }
    }
    stack.join("/")
}

/// スライド XML から (title, body) を返す。
///
/// - title: 最初の title placeholder (`<p:ph type="title"/>` または
///   `<p:ph type="ctrTitle"/>` — ECMA-376 では表紙スライド (title slide
///   layout) の title placeholder は `ctrTitle` になる) を含む `<p:sp>` の
///   a:t 連結テキスト。
/// - body: 表の外の**全ての** `<a:p>` (段落) の a:t 連結テキストを、段落単位の
///   改行区切りで連結したもの (title placeholder 配下も含む — indexer
///   (`indexer.rs`) が embed するのは `Chunk::content` のみで `heading` は
///   embed 対象外なので、title 段落を body から除外すると title-only
///   スライドが semantic search で拾えなくなる。よって title/body は
///   排他ではなく、title 段落は heading にも content にも両方現れる)。
///
///   (旧実装は `<p:sp>` の Start/End でのみ本文バッファを flush していたため、
///   sp の外側にある表のテキストが「次の sp の Start で握り潰される」/
///   「最後の sp の後ろだと一度も flush されない」のいずれかで silent drop
///   されるバグがあった。`<a:p>` 単位の flush に変えることで解消している。)
///
/// (feature-65) A table (`<a:tbl>`) is written one row per line, in the document order of its
/// `<p:graphicFrame>`: each cell's paragraphs are joined with a space into one field
/// ([`PptxCell`]), the fields of a row with a tab ([`super::ooxml::row_line`]), an empty cell
/// keeping its place as an empty field and a row of empty cells adding nothing ([`push_row`]).
/// A merge is written once ([`CellKind`]). A row the XML cuts short keeps its closed cells and
/// the closed paragraphs of the open one; a `<a:tc>` outside any `<a:tr>` opens a row of its
/// own; a `<a:tbl>` inside a table, which the schema does not allow, writes the outer row so far
/// and is read as a table of its own, after which the reader is outside any table. A paragraph
/// outside every `<a:tc>` takes the path a paragraph always took.
/// A line break (`<a:br>`) is a newline outside a table, a space inside a cell, and a space in
/// the title the heading takes ([`push_line_break`]).
///
/// (feature-66) The third value says where in the body each table row's line went, under the
/// slide's own number for its table ([`push_row`]): the tables are numbered as read, a nested
/// one taking the next number after the outer row so far is written under the outer one.
fn parse_slide_xml(
    xml: &[u8],
    path_hint: &str,
    part: &str,
) -> (Option<String>, String, Vec<RawRowSpan>) {
    super::ooxml::warn_if_truncated(path_hint, part, xml);
    let mut reader = Reader::from_reader(xml);
    reader.config_mut().trim_text(false);
    let mut buf = Vec::new();

    let mut body = String::new();
    let mut title: Option<String> = None;

    // `<p:sp>` スコープ状態。title placeholder 判定にのみ使う (本文の
    // flush 単位はもはや sp ではなく `<a:p>`)。
    let mut in_sp = false;
    let mut sp_is_title = false;

    // `<a:p>` (段落) スコープ状態。sp 内外を問わず段落単位でテキストを蓄積し、
    // `</a:p>` で title / body / セルへの振り分けを行う。
    let mut para_text = String::new();
    // (feature-65) The paragraph as the heading reads it: each `<a:br>` a space rather than a
    // newline ([`push_line_break`]). Without a break it is `para_text`.
    let mut para_title = String::new();
    let mut in_text = false;

    // (feature-65) Whether the reader is inside a `<a:tbl>`, and the row being read there.
    // Outside tables neither is set, and every paragraph takes the path it always took.
    let mut in_table = false;
    let mut row: Option<PptxRow> = None;
    // (feature-66) Where each row's line went, the number of the table being read and the
    // number the next `<a:tbl>` gets.
    let mut spans: Vec<RawRowSpan> = Vec::new();
    let (mut table, mut next_table) = (0_usize, 0_usize);

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => match super::ooxml_local(e.name().as_ref()) {
                b"sp" => {
                    in_sp = true;
                    sp_is_title = false;
                }
                b"ph" => {
                    if in_sp && ph_type_is_title(&e) {
                        sp_is_title = true;
                    }
                }
                b"p" => {
                    para_text.clear();
                    para_title.clear();
                }
                b"t" => in_text = true,
                b"br" => push_line_break(
                    &mut para_text,
                    &mut para_title,
                    open_cell_mut(in_table, &mut row).is_some(),
                ),
                b"tbl" => {
                    // A table inside a table writes out the row so far first (R1.7).
                    flush_row(&mut body, &mut spans, table, &mut row);
                    table = next_table;
                    next_table += 1;
                    in_table = true;
                }
                b"tr" if in_table => {
                    flush_row(&mut body, &mut spans, table, &mut row);
                    row = Some(PptxRow::default());
                }
                b"tc" if in_table => row
                    .get_or_insert_with(PptxRow::default)
                    .open_cell(cell_kind(&e)),
                _ => {}
            },
            Ok(Event::Empty(e)) => match super::ooxml_local(e.name().as_ref()) {
                b"ph" => {
                    if in_sp && ph_type_is_title(&e) {
                        sp_is_title = true;
                    }
                }
                b"br" => push_line_break(
                    &mut para_text,
                    &mut para_title,
                    open_cell_mut(in_table, &mut row).is_some(),
                ),
                b"tc" if in_table => {
                    let open = row.get_or_insert_with(PptxRow::default);
                    open.open_cell(cell_kind(&e));
                    open.close_cell();
                }
                _ => {}
            },
            // quick-xml 0.41 は `BytesText::unescape()` を廃止し、encoding decode
            // (`decode()`) と entity unescape (`escape::unescape()`) を分離した
            // (docx.rs Task 3.4 前例踏襲)。
            Ok(Event::Text(t)) if in_text => {
                let text = decode_text(&t);
                para_text.push_str(&text);
                para_title.push_str(&text);
            }
            // quick-xml 0.38+ は entity 参照 (`&amp;` 等) を `Event::Text` に含めず
            // `Event::GeneralRef` として別 event で届ける。ここを処理しないと
            // `<a:t>A&amp;B</a:t>` の "&" が欠落する (docx.rs 同様の必須処理)。
            Ok(Event::GeneralRef(r)) if in_text => {
                let text = super::ooxml::resolve_general_ref(&r);
                para_text.push_str(&text);
                para_title.push_str(&text);
            }
            Ok(Event::End(e)) => match super::ooxml_local(e.name().as_ref()) {
                b"t" => in_text = false,
                b"p" => {
                    let trimmed = para_text.trim();
                    if !trimmed.is_empty() {
                        if let Some(cell) = open_cell_mut(in_table, &mut row) {
                            cell.paragraphs.push(trimmed.to_string());
                        } else {
                            // title 判定と body 追加は排他にしない: title 段落の
                            // テキストも embed 対象の content (body) に残す (上の
                            // doc comment 参照)。
                            if in_sp && sp_is_title && title.is_none() {
                                title = Some(para_title.trim().to_string());
                            }
                            if !body.is_empty() {
                                body.push('\n');
                            }
                            body.push_str(trimmed);
                        }
                    }
                }
                b"sp" => in_sp = false,
                b"tc" if in_table => {
                    if let Some(open) = row.as_mut() {
                        open.close_cell();
                    }
                }
                b"tr" if in_table => flush_row(&mut body, &mut spans, table, &mut row),
                b"tbl" if in_table => {
                    flush_row(&mut body, &mut spans, table, &mut row);
                    in_table = false;
                }
                _ => {}
            },
            Ok(Event::Eof) => break,
            // 切れている場合の警告は `warn_if_truncated` (関数冒頭の 1 パス) が出す。
            Err(_) => break,
            _ => {}
        }
        buf.clear();
    }
    // (feature-65, R1.7) A row the XML cut short keeps its closed cells and the closed
    // paragraphs of its open cell, as if that cell had closed.
    flush_row(&mut body, &mut spans, table, &mut row);
    (title, body, spans)
}

/// (feature-65) What a cell of a `<a:tbl>` row adds to the row ([`PptxRow::close_cell`]), from
/// its `hMerge` / `vMerge` attributes ([`cell_kind`]).
#[derive(Clone, Copy)]
enum CellKind {
    /// A cell of its own, the origin of a merge included: its text is one field.
    Plain,
    /// Covered by the cell above (`vMerge` true, `hMerge` not): an empty field. PowerPoint does
    /// not show the text left in it, so it is not written.
    VerticalContinuation,
    /// Covered by the cell to its left (`hMerge` true): no field, and its text is not written.
    /// The origin's `gridSpan` adds nothing either, so a merge is written once.
    Covered,
}

/// (feature-65) A cell of a [`PptxRow`] between its `<a:tc>` and `</a:tc>`.
struct PptxCell {
    kind: CellKind,
    /// Its paragraphs so far, each trimmed and not empty.
    paragraphs: Vec<String>,
}

/// (feature-65) The `<a:tr>` [`parse_slide_xml`] is reading: the field each closed cell left
/// and the cell that is open, if any.
#[derive(Default)]
struct PptxRow {
    fields: Vec<String>,
    open: Option<PptxCell>,
}

impl PptxRow {
    /// `<a:tc>`: closes a cell still open (a `<a:tc>` before the last one's `</a:tc>`) and
    /// opens one of `kind`.
    fn open_cell(&mut self, kind: CellKind) {
        self.close_cell();
        self.open = Some(PptxCell {
            kind,
            paragraphs: Vec::new(),
        });
    }

    /// `</a:tc>`: the open cell becomes a field as its [`CellKind`] says: a plain cell its
    /// paragraphs joined with a space (`""` when it has none), a cell covered from above `""`,
    /// a cell covered from the left nothing.
    fn close_cell(&mut self) {
        if let Some(cell) = self.open.take() {
            match cell.kind {
                CellKind::Plain => self.fields.push(cell.paragraphs.join(" ")),
                CellKind::VerticalContinuation => self.fields.push(String::new()),
                CellKind::Covered => {}
            }
        }
    }

    /// `</a:tr>`: the row's fields, its open cell closed first.
    fn end_row(mut self) -> Vec<String> {
        self.close_cell();
        self.fields
    }
}

/// (feature-65) Whether an `xsd:boolean` attribute value is true: `1` or `true`, the
/// surrounding whitespace dropped. Any other value is false, so a value this does not know
/// writes the cell rather than dropping its text.
fn xsd_true(raw: &[u8]) -> bool {
    matches!(raw.trim_ascii(), b"1" | b"true")
}

/// (feature-65) The [`CellKind`] of a `<a:tc>`, from its `hMerge` and `vMerge` attributes read
/// by local name. `hMerge` wins when both are set.
fn cell_kind(e: &quick_xml::events::BytesStart) -> CellKind {
    let (mut covered, mut continuation) = (false, false);
    for attr in e.attributes().flatten() {
        match super::ooxml_local(attr.key.as_ref()) {
            b"hMerge" => covered = xsd_true(&attr.value),
            b"vMerge" => continuation = xsd_true(&attr.value),
            _ => {}
        }
    }
    if covered {
        CellKind::Covered
    } else if continuation {
        CellKind::VerticalContinuation
    } else {
        CellKind::Plain
    }
}

/// (feature-65) Add `row`'s line ([`super::ooxml::row_line`]) to `body` the way a paragraph is
/// added in [`parse_slide_xml`]: after a `\n` when the body has text already. A row whose
/// fields are all empty adds nothing.
///
/// (feature-66) Through [`super::ooxml::push_row_line`], which records where the line went in
/// `spans` under the slide's number `table` for its table.
fn push_row(body: &mut String, spans: &mut Vec<RawRowSpan>, table: usize, row: PptxRow) {
    if let Some(line) = super::ooxml::row_line(&row.end_row()) {
        super::ooxml::push_row_line(body, spans, table, &line);
    }
}

/// (feature-65) Write out the row being read, if any, through [`push_row`], and leave `row`
/// empty: what a `<a:tr>`, `</a:tr>`, `</a:tbl>`, a nested `<a:tbl>` and the end of the slide
/// do in [`parse_slide_xml`].
fn flush_row(
    body: &mut String,
    spans: &mut Vec<RawRowSpan>,
    table: usize,
    row: &mut Option<PptxRow>,
) {
    if let Some(open) = row.take() {
        push_row(body, spans, table, open);
    }
}

/// (feature-66) `spans` as they sit in a chunk body that kept `kept` bytes from `lead` bytes in.
///
/// The trim of a slide's content (spec R1.10) can only take whitespace that is not a tab, and a
/// row's line starts and ends with a field's text or a tab, so no span reaches a trimmed byte.
/// One that did would be dropped rather than left pointing at the wrong text.
fn rows_after_trim(spans: Vec<RawRowSpan>, lead: usize, kept: usize) -> Vec<RawRowSpan> {
    spans
        .into_iter()
        .filter_map(|span| {
            let inside = span.start >= lead && span.end <= lead + kept;
            debug_assert!(
                inside,
                "a trim reached a table row: {span:?} outside {lead}..{}",
                lead + kept
            );
            inside.then(|| RawRowSpan {
                table: span.table,
                start: span.start - lead,
                end: span.end - lead,
            })
        })
        .collect()
}

/// (feature-65) The cell a paragraph closing now belongs to: the open cell of the row being
/// read inside a `<a:tbl>`, or `None` outside one.
fn open_cell_mut(in_table: bool, row: &mut Option<PptxRow>) -> Option<&mut PptxCell> {
    if !in_table {
        return None;
    }
    row.as_mut().and_then(|r| r.open.as_mut())
}

/// (feature-65, R2) What a `<a:br>` adds to the paragraph being read: a newline to the text the
/// body gets, or a space inside a table cell so the row stays one line; and a space to the
/// text the heading gets, so a slide title stays one line.
fn push_line_break(para_text: &mut String, para_title: &mut String, in_cell: bool) {
    para_text.push(if in_cell { ' ' } else { '\n' });
    para_title.push(' ');
}

/// `<p:ph>` の `type` 属性が title placeholder (`"title"` / `"ctrTitle"`) か
/// どうか。`ctrTitle` は ECMA-376 で表紙スライド (title slide layout) の
/// title placeholder に使われる値。
fn ph_type_is_title(e: &quick_xml::events::BytesStart) -> bool {
    e.attributes().flatten().any(|attr| {
        super::ooxml_local(attr.key.as_ref()) == b"type"
            && matches!(attr.value.as_ref(), b"title" | b"ctrTitle")
    })
}

/// XML から全 `a:t` テキストを連結する (notes 用の簡易版。要素間はスペース区切り)。
fn collect_a_t(xml: &[u8], path_hint: &str, part: &str) -> String {
    super::ooxml::warn_if_truncated(path_hint, part, xml);
    let mut reader = Reader::from_reader(xml);
    reader.config_mut().trim_text(false);
    let mut buf = Vec::new();
    let mut out = String::new();
    let mut in_text = false;
    let mut run_text = String::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) if super::ooxml_local(e.name().as_ref()) == b"t" => {
                in_text = true;
                run_text.clear();
            }
            Ok(Event::Text(t)) if in_text => {
                run_text.push_str(&decode_text(&t));
            }
            Ok(Event::GeneralRef(r)) if in_text => {
                run_text.push_str(&super::ooxml::resolve_general_ref(&r));
            }
            Ok(Event::End(e)) if super::ooxml_local(e.name().as_ref()) == b"t" => {
                in_text = false;
                out.push_str(&run_text);
                out.push(' ');
            }
            Ok(Event::Eof) => break,
            // 切れている場合の警告は `warn_if_truncated` (関数冒頭の 1 パス) が出す。
            Err(_) => break,
            _ => {}
        }
        buf.clear();
    }
    out
}

/// `<a:t>` テキストの entity-unescape 済み文字列を取り出す (docx.rs と同じ
/// `decode()` → `escape::unescape()` の 2 段パターン。parse_slide_xml /
/// collect_a_t の 2 箇所で使うため pptx.rs 内 helper として共通化する)。
fn decode_text(t: &quick_xml::events::BytesText) -> String {
    let decoded = t.decode().unwrap_or_default();
    quick_xml::escape::unescape(&decoded)
        .map(|c| c.into_owned())
        .unwrap_or_else(|_| decoded.into_owned())
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

    /// slides = [(title_opt, body, notes_opt)]。notes が Some の slide には
    /// `ppt/slides/_rels/slide{n}.xml.rels` に notesSlide relationship
    /// (Type=`.../notesSlide`, Target=`../notesSlides/notesSlide{n}.xml`) も
    /// 書く (同番号 heuristic 廃止後は rels 経由でしか notes を解決しない
    /// ため、fixture 側で明示的に対応させる必要がある)。
    fn make_minimal_pptx(slides: &[(Option<&str>, &str, Option<&str>)]) -> Vec<u8> {
        let mut buf = Vec::new();
        {
            let mut zip = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
            let opt = SimpleFileOptions::default();
            zip.start_file("[Content_Types].xml", opt).unwrap();
            zip.write_all(br#"<?xml version="1.0"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"/>"#).unwrap();
            for (i, (title, body, notes)) in slides.iter().enumerate() {
                let n = i + 1;
                let title_shape = match title {
                    Some(t) => format!(
                        r#"<p:sp><p:nvSpPr><p:nvPr><p:ph type="title"/></p:nvPr></p:nvSpPr><p:txBody><a:p><a:r><a:t>{t}</a:t></a:r></a:p></p:txBody></p:sp>"#
                    ),
                    None => String::new(),
                };
                let slide_xml = format!(
                    r#"<?xml version="1.0"?><p:sld xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main" xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"><p:cSld><p:spTree>{title_shape}<p:sp><p:txBody><a:p><a:r><a:t>{body}</a:t></a:r></a:p></p:txBody></p:sp></p:spTree></p:cSld></p:sld>"#
                );
                zip.start_file(format!("ppt/slides/slide{n}.xml"), opt)
                    .unwrap();
                zip.write_all(slide_xml.as_bytes()).unwrap();
                if let Some(note) = notes {
                    let notes_xml = format!(
                        r#"<?xml version="1.0"?><p:notes xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main" xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"><p:cSld><p:spTree><p:sp><p:txBody><a:p><a:r><a:t>{note}</a:t></a:r></a:p></p:txBody></p:sp></p:spTree></p:cSld></p:notes>"#
                    );
                    zip.start_file(format!("ppt/notesSlides/notesSlide{n}.xml"), opt)
                        .unwrap();
                    zip.write_all(notes_xml.as_bytes()).unwrap();
                    let rels_xml = format!(
                        r#"<?xml version="1.0"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/notesSlide" Target="../notesSlides/notesSlide{n}.xml"/></Relationships>"#
                    );
                    zip.start_file(format!("ppt/slides/_rels/slide{n}.xml.rels"), opt)
                        .unwrap();
                    zip.write_all(rels_xml.as_bytes()).unwrap();
                }
            }
            zip.finish().unwrap();
        }
        buf
    }

    #[test]
    fn test_pptx_parser_is_binary() {
        assert!(PptxParser::default().is_binary());
        assert_eq!(PptxParser::default().extension(), "pptx");
    }

    // NOTE: skeleton 時点の `not_yet_implemented` 固定文言 assert は、本 task で
    // parse_bytes を実本実装したため意味が失われた。docx (Task 3.4) の前例に
    // 倣い、garbage 入力が real error path (zip open 失敗) で panic せず Err に
    // なることを検証するテストに更新する (controller 事前承認済み)。
    #[test]
    fn test_pptx_parse_bytes_garbage_is_err() {
        let err = PptxParser::default()
            .parse_bytes(b"not a real pptx", "x.pptx", &[])
            .expect_err("garbage bytes must be Err");
        assert!(err.to_string().contains("cannot open pptx zip"));
    }

    #[test]
    fn test_pptx_parse_fallback_wraps_raw_text() {
        let doc = PptxParser::default().parse("hello world content here", "x.pptx", &[]);
        assert_eq!(doc.chunks.len(), 1);
        assert!(doc.chunks[0].content.contains("hello world"));
    }

    #[test]
    fn test_pptx_slide_chunks_with_title_and_notes() {
        let bytes = make_minimal_pptx(&[
            (Some("概要"), "本文スライド1", Some("発表ノート1")),
            (None, "本文スライド2", None),
        ]);
        let doc = PptxParser::default()
            .parse_bytes(&bytes, "docs/deck.pptx", &[])
            .unwrap();
        assert_eq!(doc.chunks.len(), 2);
        assert_eq!(doc.chunks[0].heading.as_deref(), Some("Slide 1: 概要"));
        assert_eq!(doc.chunks[0].level, Some(2));
        assert!(doc.chunks[0].content.contains("本文スライド1"));
        assert!(doc.chunks[0].content.contains("[notes]"));
        assert!(doc.chunks[0].content.contains("発表ノート1"));
        // title placeholder 無しは "Slide {n}" fallback。
        assert_eq!(doc.chunks[1].heading.as_deref(), Some("Slide 2"));
    }

    #[test]
    fn test_pptx_slides_sorted_numerically_not_zip_order() {
        // slide10 が slide2 より前に zip へ書かれても番号順に並ぶこと。
        // make_minimal_pptx は 1..N 順に書くため、ここでは 11 枚作って
        // chunk[9] が "Slide 10" になることで数値ソートを確認。
        let slides: Vec<(Option<&str>, &str, Option<&str>)> =
            (0..11).map(|_| (None, "body", None)).collect();
        let bytes = make_minimal_pptx(&slides);
        let doc = PptxParser::default()
            .parse_bytes(&bytes, "d.pptx", &[])
            .unwrap();
        assert_eq!(doc.chunks.len(), 11);
        assert_eq!(doc.chunks[9].heading.as_deref(), Some("Slide 10"));
        assert_eq!(doc.chunks[10].heading.as_deref(), Some("Slide 11"));
    }

    /// 単一スライドの pptx zip を組み立てる (`slide_xml` = `<p:sld>...</p:sld>`
    /// 全体)。ctrTitle / 表 (graphicFrame) 等、`make_minimal_pptx` のテンプレート
    /// では組めない XML 構造を検証するテストで使う。
    fn make_pptx_single_slide(slide_xml: &str) -> Vec<u8> {
        let mut buf = Vec::new();
        {
            let mut zip = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
            let opt = SimpleFileOptions::default();
            zip.start_file("[Content_Types].xml", opt).unwrap();
            zip.write_all(br#"<?xml version="1.0"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"/>"#).unwrap();
            zip.start_file("ppt/slides/slide1.xml", opt).unwrap();
            zip.write_all(slide_xml.as_bytes()).unwrap();
            zip.finish().unwrap();
        }
        buf
    }

    #[test]
    fn test_pptx_ctr_title_placeholder_is_picked_up_as_title() {
        // ECMA-376: 表紙スライド (title slide layout) の title placeholder は
        // `type="ctrTitle"` になる (通常スライドの `type="title"` とは別値)。
        let slide_xml = r#"<?xml version="1.0"?><p:sld xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main" xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"><p:cSld><p:spTree><p:sp><p:nvSpPr><p:nvPr><p:ph type="ctrTitle"/></p:nvPr></p:nvSpPr><p:txBody><a:p><a:r><a:t>表紙タイトル</a:t></a:r></a:p></p:txBody></p:sp></p:spTree></p:cSld></p:sld>"#;
        let bytes = make_pptx_single_slide(slide_xml);
        let doc = PptxParser::default()
            .parse_bytes(&bytes, "cover.pptx", &[])
            .unwrap();
        assert_eq!(doc.chunks.len(), 1);
        assert_eq!(
            doc.chunks[0].heading.as_deref(),
            Some("Slide 1: 表紙タイトル")
        );
        // title 段落のテキストは body (= embed 対象の content) にも残す。
        // indexer は chunk.content のみを embed し heading は embed 対象外
        // (indexer.rs::embed_texts が `c.content` だけ収集する) なので、
        // title を content から排他すると title-only スライドが検索に
        // 引っかからなくなる (semantic recall 劣化の回帰 guard)。
        assert!(
            doc.chunks[0].content.contains("表紙タイトル"),
            "title text must also survive in content for embedding recall, got: {:?}",
            doc.chunks[0].content
        );
    }

    #[test]
    fn test_pptx_table_text_included_in_body() {
        // `<p:graphicFrame><a:tbl>` (表) 内の a:t は `<p:sp>` の外側にある。
        // sp 単位でのみ本文バッファを flush する実装だと、表セルのテキストが
        // 「次の sp の Start で握り潰される」/「最後の sp の後ろだと一度も
        // flush されない」のいずれかで silent drop される (回帰テスト)。
        let slide_xml = r#"<?xml version="1.0"?><p:sld xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main" xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"><p:cSld><p:spTree><p:sp><p:txBody><a:p><a:r><a:t>本文</a:t></a:r></a:p></p:txBody></p:sp><p:graphicFrame><a:graphic><a:graphicData><a:tbl><a:tr><a:tc><a:txBody><a:p><a:r><a:t>セルA</a:t></a:r></a:p></a:txBody></a:tc></a:tr><a:tr><a:tc><a:txBody><a:p><a:r><a:t>セルB</a:t></a:r></a:p></a:txBody></a:tc></a:tr></a:tbl></a:graphicData></a:graphic></p:graphicFrame></p:spTree></p:cSld></p:sld>"#;
        let bytes = make_pptx_single_slide(slide_xml);
        let doc = PptxParser::default()
            .parse_bytes(&bytes, "table.pptx", &[])
            .unwrap();
        assert_eq!(doc.chunks.len(), 1);
        assert!(
            doc.chunks[0].content.contains("本文"),
            "shape body text must survive, got: {:?}",
            doc.chunks[0].content
        );
        assert!(
            doc.chunks[0].content.contains("セルA"),
            "table cell text must not be dropped, got: {:?}",
            doc.chunks[0].content
        );
        assert!(
            doc.chunks[0].content.contains("セルB"),
            "table cell text must not be dropped, got: {:?}",
            doc.chunks[0].content
        );
    }

    #[test]
    fn test_pptx_notes_resolved_via_rels_not_same_number() {
        // dry-run (Task 3.7) で実証された誤帰属回帰テスト: 同番号 heuristic の
        // ままだと、notesSlide1.xml が (実際は rels 上 slide4 からのみ参照
        // されているにもかかわらず) 同番号の slide1 に付いてしまう。
        // rels 解決に切り替えたことで、notesSlide1.xml が実際に参照されて
        // いる slide4 の chunk にだけ notes が付くことを検証する。
        let mut buf = Vec::new();
        {
            let mut zip = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
            let opt = SimpleFileOptions::default();
            zip.start_file("[Content_Types].xml", opt).unwrap();
            zip.write_all(br#"<?xml version="1.0"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"/>"#).unwrap();

            for n in 1..=4 {
                let slide_xml = format!(
                    r#"<?xml version="1.0"?><p:sld xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main" xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"><p:cSld><p:spTree><p:sp><p:txBody><a:p><a:r><a:t>本文スライド{n}</a:t></a:r></a:p></p:txBody></p:sp></p:spTree></p:cSld></p:sld>"#
                );
                zip.start_file(format!("ppt/slides/slide{n}.xml"), opt)
                    .unwrap();
                zip.write_all(slide_xml.as_bytes()).unwrap();
            }

            // slide1: rels はあるが notesSlide relationship を持たない
            // (slideLayout relationship のみ = 「rels はあるが notes 対応は
            // 無い」ケースも同時に検証する)。
            zip.start_file("ppt/slides/_rels/slide1.xml.rels", opt)
                .unwrap();
            zip.write_all(br#"<?xml version="1.0"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slideLayout" Target="../slideLayouts/slideLayout1.xml"/></Relationships>"#).unwrap();

            // slide2/slide3: rels ファイル自体が無い。

            // slide4 の rels だけが notesSlide1.xml を参照する
            // (= 同番号 heuristic なら slide1 の notes になるはずだが、
            // 実際の所有者は slide4)。
            zip.start_file("ppt/slides/_rels/slide4.xml.rels", opt)
                .unwrap();
            zip.write_all(br#"<?xml version="1.0"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/notesSlide" Target="../notesSlides/notesSlide1.xml"/></Relationships>"#).unwrap();

            zip.start_file("ppt/notesSlides/notesSlide1.xml", opt)
                .unwrap();
            // 非 ASCII (日本語) を含むため raw byte string (`br#"..."#`) は使えない
            // (raw byte string literal は ASCII 限定): `r#"..."#.as_bytes()` で代用。
            zip.write_all(r#"<?xml version="1.0"?><p:notes xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main" xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"><p:cSld><p:spTree><p:sp><p:txBody><a:p><a:r><a:t>発表者ノート</a:t></a:r></a:p></p:txBody></p:sp></p:spTree></p:cSld></p:notes>"#.as_bytes()).unwrap();

            zip.finish().unwrap();
        }

        let doc = PptxParser::default()
            .parse_bytes(&buf, "misattr.pptx", &[])
            .unwrap();
        assert_eq!(doc.chunks.len(), 4);
        assert!(
            !doc.chunks[0].content.contains("発表者ノート"),
            "slide1 (rels has no notesSlide relationship) must NOT get slide4's \
             notes via same-number heuristic, got: {:?}",
            doc.chunks[0].content
        );
        assert!(
            !doc.chunks[1].content.contains("発表者ノート"),
            "slide2 (no rels file at all) must not get notes, got: {:?}",
            doc.chunks[1].content
        );
        assert!(
            !doc.chunks[2].content.contains("発表者ノート"),
            "slide3 (no rels file at all) must not get notes, got: {:?}",
            doc.chunks[2].content
        );
        assert!(
            doc.chunks[3].content.contains("[notes]"),
            "slide4 (rels references notesSlide1.xml) must get the notes, got: {:?}",
            doc.chunks[3].content
        );
        assert!(
            doc.chunks[3].content.contains("発表者ノート"),
            "slide4 (rels references notesSlide1.xml) must get the notes, got: {:?}",
            doc.chunks[3].content
        );
    }

    #[test]
    fn test_pptx_slides_ordered_by_presentation_visible_order_not_file_number() {
        // codex P2 (PR #70 round 1): 旧実装は ppt/slides/slideN.xml のファイル
        // 番号ソートで処理していたため、並べ替え/削除された deck では
        // presentation.xml の可視順 (sldIdLst) とズレて `Slide {n}` ラベルが
        // 実際の表示順と食い違っていた。sldIdLst が slide3.xml → slide1.xml
        // の順を指す fixture (slide2.xml は存在しない = 番号に gap があっても
        // 解決できることも同時に確認する) で、可視順の連番 "Slide 1" が
        // slide3.xml の内容になることを検証する。
        let mut buf = Vec::new();
        {
            let mut zip = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
            let opt = SimpleFileOptions::default();
            zip.start_file("[Content_Types].xml", opt).unwrap();
            zip.write_all(br#"<?xml version="1.0"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"/>"#).unwrap();

            for (file_n, title) in [(1usize, "先頭ファイル"), (3usize, "末尾ファイル")]
            {
                let slide_xml = format!(
                    r#"<?xml version="1.0"?><p:sld xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main" xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"><p:cSld><p:spTree><p:sp><p:nvSpPr><p:nvPr><p:ph type="title"/></p:nvPr></p:nvSpPr><p:txBody><a:p><a:r><a:t>{title}</a:t></a:r></a:p></p:txBody></p:sp></p:spTree></p:cSld></p:sld>"#
                );
                zip.start_file(format!("ppt/slides/slide{file_n}.xml"), opt)
                    .unwrap();
                zip.write_all(slide_xml.as_bytes()).unwrap();
            }

            // sldIdLst: rId1 (→ slide3.xml) が rId2 (→ slide1.xml) より先。
            zip.start_file("ppt/presentation.xml", opt).unwrap();
            zip.write_all(br#"<?xml version="1.0"?><p:presentation xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><p:sldIdLst><p:sldId id="256" r:id="rId1"/><p:sldId id="257" r:id="rId2"/></p:sldIdLst></p:presentation>"#).unwrap();

            zip.start_file("ppt/_rels/presentation.xml.rels", opt)
                .unwrap();
            zip.write_all(br#"<?xml version="1.0"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slide" Target="slides/slide3.xml"/><Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slide" Target="slides/slide1.xml"/></Relationships>"#).unwrap();

            zip.finish().unwrap();
        }

        let doc = PptxParser::default()
            .parse_bytes(&buf, "reordered.pptx", &[])
            .unwrap();
        assert_eq!(doc.chunks.len(), 2);
        assert_eq!(
            doc.chunks[0].heading.as_deref(),
            Some("Slide 1: 末尾ファイル"),
            "visible order (sldIdLst) must win over file-number order, got: {:?}",
            doc.chunks.iter().map(|c| &c.heading).collect::<Vec<_>>()
        );
        assert_eq!(
            doc.chunks[1].heading.as_deref(),
            Some("Slide 2: 先頭ファイル"),
            "visible order (sldIdLst) must win over file-number order, got: {:?}",
            doc.chunks.iter().map(|c| &c.heading).collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_pptx_sld_id_accepts_alternate_relationship_namespace_prefix() {
        // codex P2 (PR #70 round 4): sldId の relationship 参照属性が
        // namespace prefix `r` 以外 (`rel:id` 等) に bind された正規 XML
        // では、旧実装の生 QName `r:id` 厳密一致では取れず、可視順が
        // 解決できずに file 番号 fallback へ落ちてしまっていた。prefix
        // 付き (`:` を含む) かつ local name が `id` の属性を受理するよう
        // 緩和したことで、`rel:id` prefix でも可視順が効くことを検証する。
        let mut buf = Vec::new();
        {
            let mut zip = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
            let opt = SimpleFileOptions::default();
            zip.start_file("[Content_Types].xml", opt).unwrap();
            zip.write_all(br#"<?xml version="1.0"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"/>"#).unwrap();

            for (file_n, title) in [(1usize, "先頭ファイル"), (3usize, "末尾ファイル")]
            {
                let slide_xml = format!(
                    r#"<?xml version="1.0"?><p:sld xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main" xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"><p:cSld><p:spTree><p:sp><p:nvSpPr><p:nvPr><p:ph type="title"/></p:nvPr></p:nvSpPr><p:txBody><a:p><a:r><a:t>{title}</a:t></a:r></a:p></p:txBody></p:sp></p:spTree></p:cSld></p:sld>"#
                );
                zip.start_file(format!("ppt/slides/slide{file_n}.xml"), opt)
                    .unwrap();
                zip.write_all(slide_xml.as_bytes()).unwrap();
            }

            // sldIdLst: relationships namespace が prefix `rel` に bind
            // されている (通常の `r` ではない)。rId1 (→ slide3.xml) が
            // rId2 (→ slide1.xml) より先。
            zip.start_file("ppt/presentation.xml", opt).unwrap();
            zip.write_all(br#"<?xml version="1.0"?><p:presentation xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main" xmlns:rel="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><p:sldIdLst><p:sldId id="256" rel:id="rId1"/><p:sldId id="257" rel:id="rId2"/></p:sldIdLst></p:presentation>"#).unwrap();

            zip.start_file("ppt/_rels/presentation.xml.rels", opt)
                .unwrap();
            zip.write_all(br#"<?xml version="1.0"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slide" Target="slides/slide3.xml"/><Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slide" Target="slides/slide1.xml"/></Relationships>"#).unwrap();

            zip.finish().unwrap();
        }

        let doc = PptxParser::default()
            .parse_bytes(&buf, "reordered-rel.pptx", &[])
            .unwrap();
        assert_eq!(doc.chunks.len(), 2);
        assert_eq!(
            doc.chunks[0].heading.as_deref(),
            Some("Slide 1: 末尾ファイル"),
            "rel:id prefix must still resolve visible order, got: {:?}",
            doc.chunks.iter().map(|c| &c.heading).collect::<Vec<_>>()
        );
        assert_eq!(
            doc.chunks[1].heading.as_deref(),
            Some("Slide 2: 先頭ファイル"),
            "rel:id prefix must still resolve visible order, got: {:?}",
            doc.chunks.iter().map(|c| &c.heading).collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_pptx_slides_fall_back_to_file_number_order_without_presentation_xml() {
        // presentation.xml が無い (壊れた/非標準な zip) 場合は file 番号ソートに
        // フォールバックする (index 全体を諦めさせないための safety net)。
        // 既存の make_minimal_pptx fixture には presentation.xml が無いため、
        // このテストは実質 test_pptx_slides_sorted_numerically_not_zip_order の
        // fallback 経路が生きていることの明示的な回帰 guard。
        let bytes = make_minimal_pptx(&[(None, "本文A", None), (None, "本文B", None)]);
        let doc = PptxParser::default()
            .parse_bytes(&bytes, "no-presentation.pptx", &[])
            .unwrap();
        assert_eq!(doc.chunks.len(), 2);
        assert!(doc.chunks[0].content.contains("本文A"));
        assert!(doc.chunks[1].content.contains("本文B"));
    }

    #[test]
    fn test_pptx_context_is_title_and_slide_heading() {
        let bytes = make_minimal_pptx(&[(Some("概要"), "本文スライド1", None)]);
        let doc = PptxParser::default()
            .parse_bytes(&bytes, "docs/deck.pptx", &[])
            .unwrap();
        // filename title ("deck") > "Slide 1: 概要"
        assert_eq!(
            doc.chunks[0].context.as_deref(),
            Some("deck > Slide 1: 概要")
        );
    }

    #[test]
    fn test_pptx_entity_reference_in_text_is_preserved() {
        // quick-xml 0.38+ は entity 参照を Text event に含めず `Event::GeneralRef`
        // として別 event で届ける (docx.rs 同様の回帰テスト)。本文・notes 双方の
        // 経路 (parse_slide_xml / collect_a_t) で "&" が欠落しないことを確認する。
        let bytes = make_minimal_pptx(&[(None, "A&amp;B", Some("N&amp;M"))]);
        let doc = PptxParser::default()
            .parse_bytes(&bytes, "e.pptx", &[])
            .unwrap();
        assert!(
            doc.chunks[0].content.contains("A&B"),
            "slide body entity reference must resolve, got: {:?}",
            doc.chunks[0].content
        );
        assert!(
            doc.chunks[0].content.contains("N&M"),
            "notes entity reference must resolve, got: {:?}",
            doc.chunks[0].content
        );
    }

    /// A zip of the given entries, nothing else.
    fn pptx_of(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut buf = Vec::new();
        {
            let mut zip = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
            for (name, body) in entries {
                zip.start_file(*name, SimpleFileOptions::default()).unwrap();
                zip.write_all(body).unwrap();
            }
            zip.finish().unwrap();
        }
        buf
    }

    /// feature-61 (AC5): the slide loop and the core.xml read both count
    /// against the budget the parser was built with.
    #[test]
    fn ooxml_budget_follows_the_pptx_parser_it_was_built_with() {
        let core_xml = format!(
            r#"<?xml version="1.0"?><cp:coreProperties xmlns:cp="x" xmlns:dc="y"><dc:title>Deck</dc:title><cp:keywords>{}</cp:keywords></cp:coreProperties>"#,
            "k".repeat(400)
        );
        let slide_xml: &[u8] = br#"<?xml version="1.0"?><p:sld xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main" xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"><p:cSld><p:spTree><p:sp><p:txBody><a:p><a:r><a:t>slide body</a:t></a:r></a:p></p:txBody></p:sp></p:spTree></p:cSld></p:sld>"#;
        let bytes = pptx_of(&[
            ("docProps/core.xml", core_xml.as_bytes()),
            ("ppt/slides/slide1.xml", slide_xml),
        ]);
        let both = (core_xml.len() + slide_xml.len()) as u64;

        // core.xml is read first and fits; the slide takes the total one byte over.
        let err = PptxParser::with_budget(both - 1)
            .parse_bytes(&bytes, "deck.pptx", &[])
            .expect_err("core.xml plus the slide is one byte over the budget");
        assert!(err.to_string().contains("max_decompressed_size"), "{err}");

        let doc = PptxParser::with_budget(both)
            .parse_bytes(&bytes, "deck.pptx", &[])
            .expect("exactly the budget is allowed");
        assert_eq!(doc.frontmatter.title.as_deref(), Some("Deck"));
        assert_eq!(doc.chunks.len(), 1);

        // core.xml alone is over a smaller budget: the per-entry check turns it
        // away and the title falls back to the file name. A core.xml read that
        // still used the 50 MiB constant would have found "Deck".
        let small = core_xml.len() as u64 - 1;
        assert!(
            slide_xml.len() as u64 <= small,
            "fixture: the slide must fit the smaller budget"
        );
        let doc = PptxParser::with_budget(small)
            .parse_bytes(&bytes, "deck.pptx", &[])
            .expect("an entry over the budget is skipped, not an error");
        assert_eq!(
            doc.frontmatter.title,
            crate::parser::txt::derive_title_pub("deck.pptx")
        );
        assert_eq!(doc.chunks.len(), 1);
    }

    /// feature-61 (codex critical): the read path does not take the budget the
    /// parser was built with -- a 1-byte parser reads the deck on the read path
    /// as the default parser would, title included.
    #[test]
    fn pptx_read_path_ignores_the_parser_budget() {
        let slide_xml: &[u8] = br#"<?xml version="1.0"?><p:sld xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main" xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"><p:cSld><p:spTree><p:sp><p:txBody><a:p><a:r><a:t>slide body</a:t></a:r></a:p></p:txBody></p:sp></p:spTree></p:cSld></p:sld>"#;
        let core_xml: &[u8] = br#"<?xml version="1.0"?><cp:coreProperties xmlns:cp="x" xmlns:dc="y"><dc:title>Deck</dc:title></cp:coreProperties>"#;
        let bytes = pptx_of(&[
            ("docProps/core.xml", core_xml),
            ("ppt/slides/slide1.xml", slide_xml),
        ]);
        let tiny = PptxParser::with_budget(1);
        let index = tiny.parse_bytes(&bytes, "deck.pptx", &[]).unwrap();
        assert!(
            index.chunks.is_empty(),
            "the 1-byte budget skips every part"
        );
        let read = tiny
            .parse_bytes_for_read(&bytes, "deck.pptx", &[])
            .expect("the read path keeps the default budget");
        assert_eq!(read.frontmatter.title.as_deref(), Some("Deck"));
        assert_eq!(read.chunks.len(), 1);
    }

    // -----------------------------------------------------------------------
    // feature-65: tables one row per line
    // -----------------------------------------------------------------------

    /// feature-65: a slide whose `<p:spTree>` holds `tree`.
    fn slide_with(tree: &str) -> String {
        format!(
            r#"<?xml version="1.0"?><p:sld xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main" xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"><p:cSld><p:spTree>{tree}</p:spTree></p:cSld></p:sld>"#
        )
    }

    /// feature-65: a shape holding one `<a:p>` per entry of `paragraphs`, each the inner XML of
    /// its paragraph (runs, breaks, properties).
    fn shape_of(paragraphs: &[&str]) -> String {
        let body: String = paragraphs
            .iter()
            .map(|p| format!("<a:p>{p}</a:p>"))
            .collect();
        format!("<p:sp><p:txBody><a:bodyPr/><a:lstStyle/>{body}</p:txBody></p:sp>")
    }

    /// feature-65: a title placeholder shape holding one `<a:p>` of inner XML `paragraph`.
    fn title_shape(paragraph: &str) -> String {
        format!(
            r#"<p:sp><p:nvSpPr><p:nvPr><p:ph type="title"/></p:nvPr></p:nvSpPr><p:txBody><a:bodyPr/><a:p>{paragraph}</a:p></p:txBody></p:sp>"#
        )
    }

    /// feature-65: one run of `text`.
    fn run_of(text: &str) -> String {
        format!("<a:r><a:t>{text}</a:t></a:r>")
    }

    /// feature-65: a cell with `attrs` on its `<a:tc>` and one `<a:p>` per entry of
    /// `paragraphs` (inner XML, as [`shape_of`] takes them). No `paragraphs` is the one empty
    /// paragraph PowerPoint writes in an empty cell.
    fn cell_with(attrs: &str, paragraphs: &[&str]) -> String {
        let body: String = if paragraphs.is_empty() {
            r#"<a:p><a:endParaRPr lang="en-US"/></a:p>"#.to_string()
        } else {
            paragraphs
                .iter()
                .map(|p| format!("<a:p>{p}</a:p>"))
                .collect()
        };
        format!("<a:tc{attrs}><a:txBody><a:bodyPr/><a:lstStyle/>{body}</a:txBody><a:tcPr/></a:tc>")
    }

    /// feature-65: a plain cell of `text`; `""` is an empty cell.
    fn cell(text: &str) -> String {
        if text.is_empty() {
            cell_with("", &[])
        } else {
            cell_with("", &[&run_of(text)])
        }
    }

    /// feature-65: a `<p:graphicFrame>` holding a table of `rows`, each a list of `<a:tc>` XML,
    /// with the table properties and grid PowerPoint writes before the rows.
    fn table_of(rows: &[Vec<String>]) -> String {
        let columns = rows.iter().map(Vec::len).max().unwrap_or(0);
        let grid: String = (0..columns)
            .map(|_| r#"<a:gridCol w="1828800"/>"#)
            .collect();
        let body: String = rows
            .iter()
            .map(|r| format!(r#"<a:tr h="370840">{}</a:tr>"#, r.concat()))
            .collect();
        format!(
            r#"<p:graphicFrame><a:graphic><a:graphicData uri="http://schemas.openxmlformats.org/drawingml/2006/table"><a:tbl><a:tblPr firstRow="1" bandRow="1"/><a:tblGrid>{grid}</a:tblGrid>{body}</a:tbl></a:graphicData></a:graphic></p:graphicFrame>"#
        )
    }

    /// feature-65: the one chunk the parser makes of a deck whose one slide holds `tree`.
    fn only_chunk(tree: &str) -> crate::parser::Chunk {
        let bytes = make_pptx_single_slide(&slide_with(tree));
        let doc = PptxParser::default()
            .parse_bytes(&bytes, "deck.pptx", &[])
            .unwrap();
        assert_eq!(doc.chunks.len(), 1, "{:?}", doc.chunks);
        doc.chunks.into_iter().next().unwrap()
    }

    /// feature-65: a deck of one slide (`slide_xml`) whose notes slide holds `notes_tree`,
    /// tied by the slide's rels the way PowerPoint ties them.
    fn deck_with_notes(slide_xml: &str, notes_tree: &str) -> Vec<u8> {
        let notes = format!(
            r#"<?xml version="1.0"?><p:notes xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main" xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"><p:cSld><p:spTree>{notes_tree}</p:spTree></p:cSld></p:notes>"#
        );
        let rels = r#"<?xml version="1.0"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/notesSlide" Target="../notesSlides/notesSlide1.xml"/></Relationships>"#;
        pptx_of(&[
            ("ppt/slides/slide1.xml", slide_xml.as_bytes()),
            ("ppt/slides/_rels/slide1.xml.rels", rels.as_bytes()),
            ("ppt/notesSlides/notesSlide1.xml", notes.as_bytes()),
        ])
    }

    /// feature-65 P1 (AC1): a 2x2 table after a paragraph is a line per row, its cells
    /// separated by a tab.
    #[test]
    fn a_pptx_table_row_is_one_line_with_its_cells_tab_separated() {
        let tree = format!(
            "{}{}",
            shape_of(&[&run_of("overview")]),
            table_of(&[
                vec![cell("apple"), cell("birch")],
                vec![cell("cedar"), cell("dune")],
            ])
        );
        assert_eq!(
            only_chunk(&tree).content,
            "overview\napple\tbirch\ncedar\tdune"
        );
    }

    /// feature-65 P2 (AC2): an empty cell keeps its column as an empty field, at the start, in
    /// the middle or at the end of a row; `<a:tc/>` is an empty cell too.
    #[test]
    fn an_empty_pptx_cell_keeps_its_column() {
        let tree = format!(
            "{}{}{}",
            shape_of(&[&run_of("lead")]),
            table_of(&[
                vec![cell("alpha"), cell(""), cell("cobalt")],
                vec![cell(""), cell("bronze")],
                vec![cell("ashen"), cell("")],
                vec![cell("xenon"), "<a:tc/>".to_string()],
            ]),
            shape_of(&[&run_of("tail")])
        );
        assert_eq!(
            only_chunk(&tree).content,
            "lead\nalpha\t\tcobalt\n\tbronze\nashen\t\nxenon\t\ntail"
        );
    }

    /// feature-65 P3 (AC3): a row whose cells are all empty or blank adds no line.
    #[test]
    fn a_pptx_row_of_empty_cells_adds_no_line() {
        let tree = format!(
            "{}{}{}",
            shape_of(&[&run_of("lead")]),
            table_of(&[
                vec![cell("amber"), cell("basil")],
                vec![cell(""), cell_with("", &[&run_of(" ")])],
                vec![cell("cumin"), cell("dill")],
            ]),
            shape_of(&[&run_of("tail")])
        );
        assert_eq!(
            only_chunk(&tree).content,
            "lead\namber\tbasil\ncumin\tdill\ntail"
        );
    }

    /// feature-65 P4 (AC4): a row of one cell is that cell's text alone.
    #[test]
    fn a_one_cell_pptx_row_is_its_text() {
        let tree = format!(
            "{}{}",
            shape_of(&[&run_of("lead")]),
            table_of(&[vec![cell("xylem")], vec![cell("yarrow")]])
        );
        assert_eq!(only_chunk(&tree).content, "lead\nxylem\nyarrow");
    }

    /// feature-65 P6 (AC6): a merge is written once. A cell covered from the left (`hMerge`)
    /// adds no field; one covered from above (`vMerge`) is an empty field; `true` reads as `1`;
    /// `0` and `false` are plain cells; a cell with both flags adds no field.
    #[test]
    fn merged_pptx_cells_are_written_once() {
        let tree = format!(
            "{}{}{}",
            shape_of(&[&run_of("lead")]),
            table_of(&[
                vec![
                    cell_with(r#" gridSpan="2""#, &[&run_of("granite")]),
                    cell_with(r#" hMerge="1""#, &[]),
                    cell("hazel"),
                ],
                vec![
                    cell_with(r#" vMerge="1""#, &[]),
                    cell("kiwi"),
                    cell("maple"),
                ],
                vec![
                    cell_with(r#" gridSpan="2""#, &[&run_of("nickel")]),
                    cell_with(r#" hMerge="true""#, &[]),
                    cell("olive"),
                ],
                vec![
                    cell_with(r#" vMerge="true""#, &[]),
                    cell_with(r#" hMerge="0""#, &[&run_of("pewter")]),
                    cell_with(r#" hMerge="false""#, &[&run_of("quartz")]),
                ],
                vec![
                    cell("russet"),
                    cell_with(r#" hMerge="1" vMerge="1""#, &[]),
                    cell("sable"),
                ],
            ]),
            shape_of(&[&run_of("tail")])
        );
        assert_eq!(
            only_chunk(&tree).content,
            "lead\ngranite\thazel\n\tkiwi\tmaple\nnickel\tolive\n\tpewter\tquartz\nrusset\tsable\ntail"
        );
    }

    /// feature-65 P6b (AC6b): text left in a covered cell, which PowerPoint does not show, is
    /// not written; the cell covered from above still keeps its empty field.
    #[test]
    fn text_left_in_a_covered_pptx_cell_is_not_written() {
        let tree = format!(
            "{}{}{}",
            shape_of(&[&run_of("lead")]),
            table_of(&[
                vec![
                    cell("umber"),
                    cell_with(r#" hMerge="1""#, &[&run_of("mauve")]),
                ],
                vec![
                    cell_with(r#" vMerge="1""#, &[&run_of("navy")]),
                    cell("ochre"),
                ],
            ]),
            shape_of(&[&run_of("tail")])
        );
        let content = only_chunk(&tree).content;
        assert_eq!(content, "lead\number\n\tochre\ntail");
        assert!(
            !content.contains("mauve") && !content.contains("navy"),
            "{content:?}"
        );
    }

    /// feature-65 P10 (AC10): (a) a row the XML cuts short keeps its closed cells and the closed
    /// paragraphs of the open one; (b) an open cell covered from above is an empty field;
    /// (c) a cell outside any `<a:tr>` opens a row of its own; (d) a table inside a table, which
    /// the schema does not allow, writes the outer row so far first and loses no text.
    #[test]
    fn a_pptx_row_cut_short_keeps_its_closed_cells() {
        let open = concat!(
            r#"<?xml version="1.0"?><p:sld xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main" xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"><p:cSld><p:spTree>"#,
            r#"<p:sp><p:txBody><a:p><a:r><a:t>lead</a:t></a:r></a:p></p:txBody></p:sp>"#,
            r#"<p:graphicFrame><a:graphic><a:graphicData><a:tbl><a:tr>"#,
        );
        let content_of = |xml: &str| {
            let doc = PptxParser::default()
                .parse_bytes(&make_pptx_single_slide(xml), "cut.pptx", &[])
                .unwrap();
            doc.chunks[0].content.clone()
        };

        let cut = format!(
            r#"{open}{}<a:tc><a:txBody><a:p><a:r><a:t>berry</a:t></a:r></a:p><a:p><a:r><a:t>cutoff"#,
            cell("apple")
        );
        assert_eq!(content_of(&cut), "lead\napple\tberry", "(a)");

        let cut_covered = format!(
            r#"{open}{}<a:tc vMerge="1"><a:txBody><a:p><a:r><a:t>hidden"#,
            cell("fig")
        );
        assert_eq!(content_of(&cut_covered), "lead\nfig\t", "(b)");

        let rowless_table = format!(
            "<p:graphicFrame><a:graphic><a:graphicData><a:tbl>{}</a:tbl></a:graphicData></a:graphic></p:graphicFrame>",
            cell("lone")
        );
        let rowless = format!("{}{rowless_table}", shape_of(&[&run_of("lead")]));
        assert_eq!(only_chunk(&rowless).content, "lead\nlone", "(c)");

        let nested = format!(
            "<p:graphicFrame><a:graphic><a:graphicData><a:tbl><a:tr><a:tc><a:txBody><a:p>{}</a:p></a:txBody><a:tbl><a:tr>{}{}</a:tr></a:tbl></a:tc>{}</a:tr></a:tbl></a:graphicData></a:graphic></p:graphicFrame>",
            run_of("outer1"),
            cell("inner1"),
            cell("inner2"),
            cell("outer2")
        );
        assert_eq!(
            only_chunk(&nested).content,
            "outer1\ninner1\tinner2\nouter2",
            "(d)"
        );
    }

    /// feature-65 P11 (AC11): a slide with neither a table nor a line break reads as it did
    /// before this feature: heading, content and the joined bodies, byte for byte.
    #[test]
    fn a_slide_without_tables_or_breaks_reads_as_before() {
        let slide = slide_with(&format!(
            "{}{}",
            title_shape(&run_of("Harvest Report")),
            shape_of(&[&run_of("  lead\tmid  "), &run_of("second line")])
        ));
        let notes = shape_of(&[&run_of("wheat"), &run_of("barley")]);
        let doc = PptxParser::default()
            .parse_bytes(&deck_with_notes(&slide, &notes), "harvest.pptx", &[])
            .unwrap();
        assert_eq!(doc.chunks.len(), 1);
        assert_eq!(
            doc.chunks[0].heading.as_deref(),
            Some("Slide 1: Harvest Report")
        );
        let expected = "Harvest Report\nlead\tmid\nsecond line\n\n[notes]\nwheat barley";
        assert_eq!(doc.chunks[0].content, expected);
        assert_eq!(doc.raw_content, expected);
    }

    /// feature-65 P14 (AC27a): a table at the start of a slide keeps the tab of its leading
    /// empty field.
    #[test]
    fn a_leading_empty_cell_survives_at_the_start_of_a_slide() {
        let tree = table_of(&[
            vec![cell(""), cell("birch")],
            vec![cell("cedar"), cell("dune")],
        ]);
        assert_eq!(only_chunk(&tree).content, "\tbirch\ncedar\tdune");
    }

    /// feature-65 P15 (AC27b): a table at the end of a slide without notes keeps the tab of its
    /// trailing empty field.
    #[test]
    fn a_trailing_empty_cell_survives_at_the_end_of_a_slide() {
        let tree = format!(
            "{}{}",
            shape_of(&[&run_of("preamble")]),
            table_of(&[vec![cell("ashen"), cell("")]])
        );
        assert_eq!(only_chunk(&tree).content, "preamble\nashen\t");
    }

    /// feature-65 P16 (AC28): a slide of notes alone reads as before.
    #[test]
    fn a_notes_only_slide_reads_as_before() {
        let bytes = deck_with_notes(&slide_with(""), &shape_of(&[&run_of("quill")]));
        let doc = PptxParser::default()
            .parse_bytes(&bytes, "notes.pptx", &[])
            .unwrap();
        assert_eq!(doc.chunks.len(), 1);
        assert_eq!(doc.chunks[0].heading.as_deref(), Some("Slide 1"));
        assert_eq!(doc.chunks[0].content, "[notes]\nquill");
    }

    /// feature-65 P17 (AC30): a merge flag is true for `1` and `true` only, with the
    /// surrounding whitespace dropped; `yes`, `TRUE` and `""` are plain cells whose text is
    /// written.
    #[test]
    fn merge_flags_read_only_xsd_boolean_true() {
        let tree = format!(
            "{}{}{}",
            shape_of(&[&run_of("lead")]),
            table_of(&[
                vec![
                    cell("apricot"),
                    cell_with(r#" hMerge=" 1 ""#, &[]),
                    cell("bramble"),
                ],
                vec![cell_with(r#" vMerge=" true""#, &[]), cell("chestnut")],
                vec![
                    cell_with(r#" hMerge="yes""#, &[&run_of("damson")]),
                    cell_with(r#" hMerge="TRUE""#, &[&run_of("elder")]),
                    cell_with(r#" vMerge="""#, &[&run_of("fennel")]),
                ],
            ]),
            shape_of(&[&run_of("tail")])
        );
        assert_eq!(
            only_chunk(&tree).content,
            "lead\napricot\tbramble\n\tchestnut\ndamson\telder\tfennel\ntail"
        );
    }

    /// feature-65 P18 (AC31): a paragraph after a table is a line of its own.
    #[test]
    fn a_paragraph_after_a_table_is_its_own_line() {
        let tree = format!(
            "{}{}",
            table_of(&[vec![cell("apple"), cell("birch")]]),
            shape_of(&[&run_of("quince")])
        );
        assert_eq!(only_chunk(&tree).content, "apple\tbirch\nquince");
    }

    /// feature-65 Review Focus 1: a table inside a group shape is read the same way.
    #[test]
    fn a_table_inside_a_group_is_read_the_same_way() {
        let tree = format!(
            "<p:grpSp><p:nvGrpSpPr/><p:grpSpPr/>{}{}</p:grpSp>",
            shape_of(&[&run_of("legend")]),
            table_of(&[
                vec![cell("apple"), cell("birch")],
                vec![cell("cedar"), cell("dune")],
            ])
        );
        assert_eq!(
            only_chunk(&tree).content,
            "legend\napple\tbirch\ncedar\tdune"
        );
    }

    /// feature-65 Review Focus 2: an entity and a field in a cell stay in its one field.
    #[test]
    fn entities_and_fields_in_a_cell_stay_in_its_field() {
        let fielded = cell_with(
            "",
            &[
                r#"<a:r><a:t>R&amp;D</a:t></a:r><a:fld id="{0F7D0E7C-2A4B-4C6A-9A63-1C4F0E7B9A10}" type="slidenum"><a:t>7</a:t></a:fld>"#,
            ],
        );
        let tree = table_of(&[vec![fielded, cell("budget")]]);
        assert_eq!(only_chunk(&tree).content, "R&D7\tbudget");
    }

    /// feature-65 Review Focus 3: two tables on one slide stay in document order, and the
    /// trailing empty field of the second keeps its tab before the notes.
    #[test]
    fn two_tables_and_notes_keep_their_order() {
        let slide = slide_with(&format!(
            "{}{}{}",
            shape_of(&[&run_of("lead")]),
            table_of(&[vec![cell("apple"), cell("birch")]]),
            table_of(&[vec![cell("cedar"), cell("")]])
        ));
        let bytes = deck_with_notes(&slide, &shape_of(&[&run_of("quill")]));
        let doc = PptxParser::default()
            .parse_bytes(&bytes, "two.pptx", &[])
            .unwrap();
        assert_eq!(
            doc.chunks[0].content,
            "lead\napple\tbirch\ncedar\t\n\n[notes]\nquill"
        );
    }

    /// feature-65 Review Focus 4: a cell without a text body, which the schema allows, is an
    /// empty field.
    #[test]
    fn a_cell_without_a_text_body_is_an_empty_field() {
        let tree = format!(
            "{}{}",
            shape_of(&[&run_of("lead")]),
            table_of(&[vec!["<a:tc><a:tcPr/></a:tc>".to_string(), cell("xenon")]])
        );
        assert_eq!(only_chunk(&tree).content, "lead\n\txenon");
    }

    /// feature-65 P13r (AC14): the read entry writes a table the same way the index does.
    #[test]
    fn the_read_entry_writes_pptx_table_rows_the_same_way() {
        let bytes = make_pptx_single_slide(&slide_with(&format!(
            "{}{}",
            shape_of(&[&run_of("overview")]),
            table_of(&[
                vec![cell("apple"), cell("birch")],
                vec![cell("cedar"), cell("dune")],
            ])
        )));
        let parser = PptxParser::default();
        let index = parser.parse_bytes(&bytes, "deck.pptx", &[]).unwrap();
        let read = parser
            .parse_bytes_for_read(&bytes, "deck.pptx", &[])
            .unwrap();
        let shape = |doc: &crate::parser::ParsedDocument| -> Vec<(Option<String>, String)> {
            doc.chunks
                .iter()
                .map(|c| (c.heading.clone(), c.content.clone()))
                .collect()
        };
        assert_eq!(shape(&read), shape(&index));
        assert_eq!(
            read.chunks[0].content,
            "overview\napple\tbirch\ncedar\tdune"
        );
    }

    // -----------------------------------------------------------------------
    // feature-65: line breaks
    // -----------------------------------------------------------------------

    /// feature-65 P5 (AC5): a cell's paragraphs and the line breaks inside them join with a
    /// space, in both forms of `<a:br>`, and the row stays one line.
    #[test]
    fn paragraphs_and_breaks_in_a_pptx_cell_join_with_a_space() {
        let first = cell_with(
            "",
            &[
                &run_of("xenial"),
                "",
                &format!("{}<a:br/>{}", run_of("yonder"), run_of("zephyr")),
            ],
        );
        let second = cell_with(
            "",
            &[&format!(
                r#"{}<a:br><a:rPr lang="en-US"/></a:br>{}"#,
                run_of("wal"),
                run_of("nut")
            )],
        );
        let tree = table_of(&[vec![first, second]]);
        assert_eq!(only_chunk(&tree).content, "xenial yonder zephyr\twal nut");
    }

    /// feature-65 P7 (AC7): outside a table a line break is a newline, in both forms of
    /// `<a:br>`; the spaces around it stay, two breaks leave an empty line, and breaks at
    /// either end of the paragraph go with its trim.
    #[test]
    fn a_line_break_outside_a_table_is_a_newline() {
        let tree = shape_of(&[
            &format!("{}<a:br/>{}", run_of("alpha "), run_of(" beta")),
            &format!(
                r#"{}<a:br><a:rPr lang="en-US"/></a:br>{}"#,
                run_of("gamma"),
                run_of("delta")
            ),
            &format!("{}<a:br/><a:br/>{}", run_of("x"), run_of("y")),
            &format!("<a:br/>{}<a:br/>", run_of("z")),
        ]);
        assert_eq!(
            only_chunk(&tree).content,
            "alpha \n beta\ngamma\ndelta\nx\n\ny\nz"
        );
    }

    /// feature-65 P8 (AC8): a line break in the title is a space in the heading and a newline
    /// in the content.
    #[test]
    fn a_line_break_in_the_title_is_a_space_in_the_heading() {
        let tree = format!(
            "{}{}",
            title_shape(&format!(
                "{}<a:br/>{}",
                run_of("Quarterly"),
                run_of("Review")
            )),
            shape_of(&[&run_of("Agenda")])
        );
        let chunk = only_chunk(&tree);
        assert_eq!(chunk.heading.as_deref(), Some("Slide 1: Quarterly Review"));
        assert_eq!(chunk.content, "Quarterly\nReview\nAgenda");
    }

    /// feature-65 P9 (AC9): a tab stop list (`a:tabLst > a:tab`) defines tab positions and adds
    /// nothing to the text.
    #[test]
    fn a_tab_stop_list_adds_nothing() {
        let tree = shape_of(&[
            &format!(
                r#"<a:pPr><a:tabLst><a:tab pos="914400" algn="l"/></a:tabLst></a:pPr>{}"#,
                run_of("ledger")
            ),
            &run_of("margin"),
        ]);
        assert_eq!(only_chunk(&tree).content, "ledger\nmargin");
    }

    /// feature-65 P12 (AC12): the notes keep their reading: a line break in a notes paragraph
    /// adds nothing, its runs joined with a space as before.
    #[test]
    fn notes_keep_their_reading() {
        let slide = slide_with(&shape_of(&[&run_of("anchor")]));
        let notes = shape_of(&[&format!("{}<a:br/>{}", run_of("note"), run_of("line"))]);
        let doc = PptxParser::default()
            .parse_bytes(&deck_with_notes(&slide, &notes), "notes.pptx", &[])
            .unwrap();
        assert_eq!(doc.chunks[0].content, "anchor\n\n[notes]\nnote line");
    }

    // -----------------------------------------------------------------------
    // feature-66: where each table row sits in its chunk
    // -----------------------------------------------------------------------

    use crate::parser::{assert_spans_hold, row_span};

    /// feature-66: the chunk's table rows, after asserting spec R1.3 on them.
    fn spans_of(chunk: &crate::parser::Chunk) -> Vec<crate::parser::TableRowSpan> {
        assert_spans_hold(chunk);
        chunk.table_rows.clone().expect("checked above")
    }

    /// feature-66 P1 (AC1, pptx): a paragraph then a 2x2 table: each row's line is marked.
    #[test]
    fn pptx_table_rows_mark_their_lines() {
        let tree = format!(
            "{}{}",
            shape_of(&[&run_of("overview")]),
            table_of(&[
                vec![cell("apple"), cell("birch")],
                vec![cell("cedar"), cell("dune")],
            ])
        );
        let chunk = only_chunk(&tree);
        assert_eq!(chunk.content, "overview\napple\tbirch\ncedar\tdune");
        assert_eq!(
            spans_of(&chunk),
            vec![row_span(0, 9, 20), row_span(0, 21, 31)]
        );
    }

    /// feature-66 P2 (AC2): a row with text at one end only is a row, its line starting or
    /// ending with a tab, at the very start and the very end of the slide.
    #[test]
    fn a_row_with_one_filled_end_is_still_a_row() {
        let chunk = only_chunk(&table_of(&[
            vec![cell(""), cell("bronze")],
            vec![cell("ashen"), cell("")],
        ]));
        assert_eq!(chunk.content, "\tbronze\nashen\t");
        let spans = spans_of(&chunk);
        assert_eq!(spans, vec![row_span(0, 0, 7), row_span(0, 8, 14)]);
        let lines: Vec<&str> = spans
            .iter()
            .map(|s| &chunk.content[s.start as usize..s.end as usize])
            .collect();
        assert_eq!(lines, vec!["\tbronze", "ashen\t"]);
    }

    /// feature-66 P3 (AC2): the spans move with the whitespace trimmed off the head of a slide
    /// and keep their raw table numbers; with nothing trimmed they stay where they were.
    #[test]
    fn spans_survive_the_slide_trim() {
        let raw = |table, start, end| RawRowSpan { table, start, end };
        assert_eq!(
            rows_after_trim(vec![raw(3, 2, 9), raw(5, 10, 14)], 2, 12),
            vec![raw(3, 0, 7), raw(5, 8, 12)]
        );
        assert_eq!(
            rows_after_trim(vec![raw(0, 0, 7)], 0, 7),
            vec![raw(0, 0, 7)]
        );
    }

    /// feature-66 P4 (AC3, pptx): a raw tab in a paragraph outside a table is no row, in a
    /// chunk that has a table too.
    #[test]
    fn a_pptx_tab_outside_a_table_is_not_a_row() {
        let tree = format!(
            "{}{}",
            shape_of(&[&run_of("lead\tmid")]),
            table_of(&[vec![cell("cobalt"), cell("dune")]])
        );
        let chunk = only_chunk(&tree);
        assert_eq!(chunk.content, "lead\tmid\ncobalt\tdune");
        assert_eq!(spans_of(&chunk), vec![row_span(0, 9, 20)]);
    }

    /// feature-66 P5 (AC4): a row of one field is marked, though its line has no tab.
    #[test]
    fn a_one_field_row_is_still_a_row() {
        let tree = format!(
            "{}{}",
            shape_of(&[&run_of("lead")]),
            table_of(&[vec![cell("xylem")], vec![cell("yarrow")]])
        );
        let chunk = only_chunk(&tree);
        assert_eq!(chunk.content, "lead\nxylem\nyarrow");
        assert_eq!(
            spans_of(&chunk),
            vec![row_span(0, 5, 10), row_span(0, 11, 17)]
        );
    }

    /// feature-66 P6 (AC5, pptx): a row whose cells are all empty or blank gets no span.
    #[test]
    fn an_empty_pptx_row_gets_no_span() {
        let chunk = only_chunk(&table_of(&[
            vec![cell("amber"), cell("basil")],
            vec![cell(""), cell_with("", &[&run_of(" ")])],
            vec![cell("cumin"), cell("dill")],
        ]));
        assert_eq!(chunk.content, "amber\tbasil\ncumin\tdill");
        assert_eq!(
            spans_of(&chunk),
            vec![row_span(0, 0, 11), row_span(0, 12, 22)]
        );
    }

    /// feature-66 P9 (AC8): two tables on a slide are 0 and 1, and still 0 and 1 with a table of
    /// empty rows between them.
    #[test]
    fn tables_are_numbered_by_first_appearance() {
        let lead = shape_of(&[&run_of("lead")]);
        let first = table_of(&[vec![cell("apple"), cell("birch")]]);
        let hollow = table_of(&[vec![cell(""), cell("")]]);
        let last = table_of(&[vec![cell("cedar"), cell("")]]);
        for tree in [
            format!("{lead}{first}{last}"),
            format!("{lead}{first}{hollow}{last}"),
        ] {
            let chunk = only_chunk(&tree);
            assert_eq!(chunk.content, "lead\napple\tbirch\ncedar\t");
            assert_eq!(
                spans_of(&chunk),
                vec![row_span(0, 5, 16), row_span(1, 17, 23)]
            );
        }
    }

    /// feature-66 P10 (AC9): the spans end before the notes, and a tab in the notes is no row.
    #[test]
    fn notes_carry_no_span() {
        let slide = slide_with(&format!(
            "{}{}",
            shape_of(&[&run_of("lead")]),
            table_of(&[vec![cell("apple"), cell("birch")]])
        ));
        let bytes = deck_with_notes(&slide, &shape_of(&[&run_of("quill\tink")]));
        let doc = PptxParser::default()
            .parse_bytes(&bytes, "notes.pptx", &[])
            .unwrap();
        let chunk = &doc.chunks[0];
        assert_eq!(chunk.content, "lead\napple\tbirch\n\n[notes]\nquill\tink");
        assert_eq!(spans_of(chunk), vec![row_span(0, 5, 16)]);
    }

    /// feature-66 P11 (AC10, pptx): a slide without a table marks no row, and the fallback
    /// [`crate::parser::Parser::parse`] over extracted text leaves the rows not computed.
    #[test]
    fn a_pptx_without_a_table_marks_no_row_and_the_fallback_marks_none() {
        assert_eq!(
            only_chunk(&shape_of(&[&run_of("lead")])).table_rows,
            Some(vec![])
        );
        let fallback = PptxParser::default().parse("plain words here", "x.pptx", &[]);
        assert_eq!(fallback.chunks[0].table_rows, None);
    }

    /// feature-66 P12 (AC11, pptx): the read entry marks the same rows as the index entry.
    #[test]
    fn the_read_entry_marks_the_same_pptx_rows() {
        let bytes = make_pptx_single_slide(&slide_with(&table_of(&[vec![
            cell("apple"),
            cell("birch"),
        ]])));
        let index = PptxParser::default()
            .parse_bytes(&bytes, "deck.pptx", &[])
            .unwrap();
        let read = PptxParser::default()
            .parse_bytes_for_read(&bytes, "deck.pptx", &[])
            .unwrap();
        assert_eq!(read.chunks[0].table_rows, index.chunks[0].table_rows);
        assert_eq!(spans_of(&index.chunks[0]), vec![row_span(0, 0, 11)]);
    }

    /// feature-66 P13 (AC12, pptx): a raw tab in a cell's text stays a tab, inside its row's one
    /// span.
    #[test]
    fn a_tab_in_a_pptx_cell_stays_inside_its_row_span() {
        let chunk = only_chunk(&table_of(&[vec![cell("alder\tbirch"), cell("cedar")]]));
        assert_eq!(chunk.content, "alder\tbirch\tcedar");
        assert_eq!(spans_of(&chunk), vec![row_span(0, 0, 17)]);
    }

    /// feature-66 Review Focus 3 (spec U2): a newline in a cell's own text stays in the content
    /// and inside the row's span.
    #[test]
    fn a_newline_in_a_cell_text_stays_inside_its_span() {
        let chunk = only_chunk(&table_of(&[vec![cell("ink\nwell"), cell("quay")]]));
        assert_eq!(chunk.content, "ink\nwell\tquay");
        assert_eq!(spans_of(&chunk), vec![row_span(0, 0, 13)]);
        assert_eq!(&chunk.content[0..13], "ink\nwell\tquay");
    }
}
