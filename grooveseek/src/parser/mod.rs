//! Parser plugin layer.
//!
//! 各ファイル形式 (`.md` / `.txt` / 将来 `.rst` / `.adoc` / `.pdf` 等) に対して
//! `trait Parser` の実装を 1 つ用意し、`Registry` が拡張子でルックアップする。
//! 形式追加は新しい `Parser` impl を追加して `Registry::defaults()` か
//! `groove.toml` の `[parsers].enabled` に id を入れるだけ。
//!
//! `Frontmatter` / `Chunk` / `ParsedDocument` は元々 `src/markdown.rs` にあった
//! が、形式非依存な表現として parser モジュールへ移した。
//! `src/markdown.rs` は後方互換 shim として公開 API を保つ。

use anyhow::{Context, Result};
use serde::Deserialize;
use std::collections::BTreeMap;

pub mod code;
pub mod docx;
pub mod markdown;
pub mod ooxml;
mod panic_guard;
pub mod pdf;
pub mod pptx;
pub mod registry;
pub mod txt;
pub mod xlsx;

pub use code::CodeParser;
pub use docx::DocxParser;

/// (feature-56) Whether any of these ids can only be satisfied by a grammar the user placed.
///
/// The question the caller is really asking is "do I have to work out where plugins live",
/// which is why it is answered here from the same table the loader opens files by rather than
/// re-derived from a list of built-in ids. A `false` answer is what keeps a Markdown-only
/// knowledge base from ever reading `GROOVE_GRAMMAR_DIR`.
pub fn needs_grammar_plugin(ids: &[String]) -> bool {
    ids.iter()
        .any(|id| code::plugin::plugin_entry(&id.to_ascii_lowercase()).is_some())
}
pub use markdown::MarkdownParser;
pub use pdf::PdfParser;
pub use pptx::PptxParser;
pub use registry::{PluginSource, Registry};
pub use txt::TxtParser;
pub use xlsx::{XlsParser, XlsxParser};

// ---------------------------------------------------------------------------
// Data types (formerly in src/markdown.rs)
// ---------------------------------------------------------------------------

/// One frontmatter value the schema did not name in advance (feature-57).
///
/// [`Frontmatter`] is held as strings throughout, so a value is either a string,
/// a list of strings, or a shape neither of those can carry. [`FieldValue::Other`]
/// keeps the shape's name and nothing else: the deserializer in [`markdown`] reads a
/// retained value for its shape and skips the nesting under it rather than
/// buffering it, so retaining an unknown key costs what the YAML parser
/// already paid -- the recursion and repetition budgets that bound a document
/// are the same ones that bounded it before any key was kept.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FieldValue {
    /// A YAML string, bool (`"true"` / `"false"`) or number (`to_string`).
    Scalar(String),
    /// A sequence whose every element is a [`FieldValue::Scalar`].
    List(Vec<String>),
    /// `"mapping"`, `"nested sequence"`, `"binary"` or [`FieldValue::NULL`].
    /// A mapping, a sequence holding a non-scalar, and an explicit `!!binary`
    /// are present but unreadable: `required` is satisfied and any rule that
    /// reads the value reports a `type_mismatch` naming this shape. A null is
    /// the key without a value and counts as absent for every rule instead.
    Other(&'static str),
}

impl FieldValue {
    /// The shape name for a key written with no value (`status:`, `~`,
    /// `null`). Named rather than spelled out twice: the parser produces it
    /// and [`crate::schema`]'s `check_extra` reads it to treat the key as absent.
    pub const NULL: &'static str = "null";

    /// The word a `type_mismatch` reports as `actual`.
    pub fn shape(&self) -> &'static str {
        match self {
            FieldValue::Scalar(_) => "string",
            FieldValue::List(_) => "array",
            FieldValue::Other(name) => name,
        }
    }
}

/// Metadata extracted from a document header (YAML frontmatter for `.md`,
/// filename-derived for `.txt`, etc.).
#[derive(Debug, Clone, Default)]
pub struct Frontmatter {
    pub title: Option<String>,
    pub date: Option<String>,
    pub topic: Option<String>,
    pub depth: Option<String>,
    pub tags: Vec<String>,
    /// Every other top-level key of the block, by name (feature-57). Empty for
    /// every parser but Markdown. The YAML merge key `<<` is never here.
    pub extra: BTreeMap<String, FieldValue>,
}

/// A single chunk of a parsed document.
///
/// Every field's default is its type's natural one (`0`, `None`, `String::new()`),
/// so `#[derive(Default)]` is sufficient and clippy-compliant. Other config-like
/// structs in this crate (e.g. `MmrConfig`) use a hand-written `Default` because
/// some defaults are non-zero (e.g. `lambda = 0.7`).
#[derive(Debug, Clone, Default)]
pub struct Chunk {
    pub index: usize,
    pub heading: Option<String>,
    /// Markdown 見出しレベル (h2=2, h3=3)。heading が None の場合や、
    /// 見出し概念のない parser (.txt 等) では None。Parent retriever で
    /// hierarchy を利用する。
    pub level: Option<u8>,
    pub content: String,
    /// 静的 context (パンくず)。feature-46。index 時に embedding + FTS + reranker
    /// 入力へ注入する検索 signal 専用フィールド。API 返却には露出しない。
    /// 例: "設計ノート > 検索パイプライン > RRF の実装"。
    /// None = context なし (生成不能ケース / 旧 parser 実装)。
    pub context: Option<String>,
    /// (feature-56) 1-based inclusive line range this chunk occupies in the
    /// source file, or `None` for formats without a line concept.
    ///
    /// Describes **the chunk**, not the definition it came from: a doc comment
    /// pulled in above a function is part of the range, and a function split
    /// across several chunks gives each piece its own. That is the only reading
    /// under which "open the file at this line and you see this chunk" stays
    /// true for every chunk a code file produces.
    pub line_range: Option<(u32, u32)>,
    /// (feature-56) What kind of definition this chunk holds, verbatim from the
    /// grammar's tags query (`function` / `class` / `method` / `constant` …), or
    /// `None` for chunks that are not a definition.
    ///
    /// Deliberately the tags vocabulary rather than the language's own keyword:
    /// `struct`, `enum` and `union` are all `class` to Rust's tags query, and
    /// recovering the keyword would mean a per-language table on groove's side —
    /// the thing that keeps a grammar to data. The set grows as languages are
    /// added, so this is a free-form string rather than a closed enum.
    pub symbol_kind: Option<String>,
    /// (feature-66) Where each table row's line sits in [`Chunk::content`], in bytes
    /// ([`TableRowSpan`]), or `None` when this was not computed: every format but `.docx` and
    /// `.pptx`, the `.docx` / `.pptx` fallback [`Parser::parse`], and a chunk whose offsets do
    /// not fit a `u32`. `Some(vec![])` is a `.docx` or `.pptx` chunk with no table row.
    pub table_rows: Option<Vec<TableRowSpan>>,
}

/// (feature-66) Where one table row's line sits in a chunk's `content`.
///
/// From [`Self::start`] up to, not including, [`Self::end`] in bytes, on char boundaries, the
/// row's trailing `\n` left out. [`Self::table`] numbers the tables of the chunk in the order
/// each first appears, from 0; a table that wrote no row takes no number, and a table split
/// across chunks is numbered in each. The text inside a span is the row's line as the parser
/// wrote it: its fields joined with a tab, so a tab inside a cell cannot be told from a field
/// boundary there, and a newline in a cell's own text stays inside the span.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TableRowSpan {
    /// The table's number in the chunk.
    pub table: u32,
    /// The byte offset in `content` where the row's line starts.
    pub start: u32,
    /// The byte offset in `content` just past the row's line.
    pub end: u32,
}

/// (feature-66) Whether `spans` keep the [`TableRowSpan`] contract over `content` (spec R1.3 1, 3,
/// 4 and 5): each is non-empty, inside it and on char boundaries; each starts at the start of a
/// line and ends at the end of one; they are in order without touching; and the tables are
/// numbered by first appearance from 0 without a gap (a span may return to an earlier table, as
/// the outer table does around a nested one). A `\n` inside a span is allowed: a cell's own
/// newline stays inside its row (spec §9 U2). The read side of the index answers `None` for a
/// stored value that fails this, so a reader can rely on every span it is given without a check
/// of its own.
pub(crate) fn table_rows_fit(content: &str, spans: &[TableRowSpan]) -> bool {
    let bytes = content.as_bytes();
    let mut previous_end: Option<usize> = None;
    let mut tables = 0_u32;
    for span in spans {
        let (start, end) = (span.start as usize, span.end as usize);
        if start >= end
            || end > content.len()
            || !content.is_char_boundary(start)
            || !content.is_char_boundary(end)
            || previous_end.is_some_and(|previous| previous >= start)
            || (start != 0 && bytes[start - 1] != b'\n')
            || (end != bytes.len() && bytes[end] != b'\n')
            || span.table > tables
        {
            return false;
        }
        if span.table == tables {
            tables += 1;
        }
        previous_end = Some(end);
    }
    true
}

/// (feature-66) The span `(table, start, end)`, for tests.
#[cfg(test)]
pub(crate) fn row_span(table: u32, start: u32, end: u32) -> TableRowSpan {
    TableRowSpan { table, start, end }
}

/// (feature-66) Assert spec R1.3 1, 3, 4 and 5 of `chunk`'s table rows: present and keeping the
/// contract over the content, by the same check the read side of the index runs
/// ([`table_rows_fit`]: fitting the content, each covering whole lines, and the tables numbered
/// by first appearance without gaps).
#[cfg(test)]
pub(crate) fn assert_spans_hold(chunk: &Chunk) {
    let content = &chunk.content;
    let spans = chunk
        .table_rows
        .as_deref()
        .expect("a .docx / .pptx chunk marks its rows");
    assert!(
        table_rows_fit(content, spans),
        "R1.3 1/3/4/5: {spans:?} in {content:?}"
    );
}

/// A fully parsed document: frontmatter + chunks + retained raw content.
#[derive(Debug, Clone)]
pub struct ParsedDocument {
    pub frontmatter: Frontmatter,
    pub chunks: Vec<Chunk>,
    pub raw_content: String,
    /// Why the YAML frontmatter could not be parsed, when it could not.
    ///
    /// `None` for a document without a frontmatter block and for one whose
    /// block parsed. `Some` only when a block was found and the YAML inside
    /// it was refused; [`Frontmatter`] is then empty apart from the
    /// [`markdown::TAG_FRONTMATTER_UNPARSED`] tag. The parser returns this as
    /// data rather than printing it because it does not know the file's path
    /// and is also run by `get_document` and `groove validate`, which want no
    /// stray stderr line; the indexer is the one caller that names the file,
    /// counts it and decides the exit code.
    pub frontmatter_error: Option<String>,
}

/// Section headings excluded by default when the caller does not override.
/// Empty by default; callers typically configure this via `groove.toml`'s
/// `exclude_headings` key. Matching is substring-based inside the Markdown
/// chunker.
pub const DEFAULT_EXCLUDED_HEADINGS: &[&str] = &[];

/// バイナリ形式ファイルの生バイト上限の**既定値** (50 MiB)。
///
/// (feature-61) 索引時の上限は `[index].max_binary_file_size` で変えられ、実際に効く値は
/// [`Registry::limits`] が持つ ([`FileSizeLimits`])。`get_document` / `resources/read`
/// の読み出し側は索引の設定に連動しない別の定数 ([`crate::server`] の private な
/// documents module が持つ、バイナリ形式の読み出し上限) を使う。名前は ADR-0004 / ADR-0005 と docs が
/// 参照しているので変えない。テキスト形式の既定は [`MAX_RAW_TEXT_BYTES`]。
pub const MAX_RAW_BINARY_BYTES: u64 = 50 * 1024 * 1024;

/// (BU-02) テキスト形式ファイルの生バイト上限の**既定値** (50 MiB)。
/// (feature-61) `[index].max_text_file_size` で変えられる ([`FileSizeLimits::text`])。
///
/// もともとテキストには index 時の上限が無く、`fs::read` が丸ごとメモリに
/// 載せていた。`rebuild_index` は MCP ツールとしてクライアントから叩けるので、
/// KB に巨大な `.md` を 1 つ置くだけで任意のタイミングで OOM を誘発できた。
///
/// 値をバイナリ側と揃えてあるのは、**どちらもファイル 1 本を丸ごとメモリに
/// 載せる**という同じ制約から来ているため。50 MiB の Markdown は
/// 2,000 万文字級で、正当な知識ベースの文書としては現実的でない。
pub const MAX_RAW_TEXT_BYTES: u64 = 50 * 1024 * 1024;

/// (feature-61) 展開 budget の既定値 (50 MiB)。xlsx の展開 pre-flight、docx / pptx の
/// 累積展開量、PDF の文書単位テキスト量が使う。`[index].max_decompressed_size` で変えられる
/// ([`FileSizeLimits::decompressed`])。
///
/// [`MAX_RAW_BINARY_BYTES`] と同じ数だが連動させない (ADR-0026): xlsx の XML は圧縮で
/// 数倍に縮むので、raw 上限に連動させると raw を上げた workbook が展開側で skip され、
/// 通すには両方を `"unlimited"` にするしかなくなる。その瞬間に zip-bomb の防御が全ファイルで
/// 消える。
pub const DEFAULT_MAX_DECOMPRESSED_BYTES: u64 = 50 * 1024 * 1024;

/// (feature-61) `[index]` のサイズ上限 1 つ分。
///
/// TOML では整数 (bytes)、単位付き文字列 (`"300 MiB"`、`"2 GB"`)、または `"unlimited"`
/// (大文字小文字を区別しない) で書く。単位は `B` / `KB` / `MB` / `GB` / `TB` (1000 進) と
/// `KiB` / `MiB` / `GiB` / `TiB` (1024 進)、大文字小文字を区別しない。小数は受けない。
/// `0` と負値は拒否して `"unlimited"` を案内する — `0` を「無制限」と読む製品と「既定」
/// 「全拒否」と読む製品があり、どれとも取り違えさせないため (ADR-0026)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(try_from = "RawFileSizeLimit")]
pub enum FileSizeLimit {
    /// この bytes 数まで。TOML から来る値は 1 以上。
    Bytes(u64),
    /// 上限なし。
    Unlimited,
}

impl FileSizeLimit {
    /// 比較に使う上限 (bytes)。
    ///
    /// [`FileSizeLimit::Unlimited`] は `u64::MAX` を返す。**「無制限」を `u64::MAX` で表すのは
    /// ここだけ**で、他所で `== u64::MAX` を判定しない (sentinel を散らすと、同じ値が場所に
    /// よって別の意味を持ち始める)。消費側はすべて `len > cap`、`cap.saturating_add(1)`、
    /// `cap.saturating_sub(total)` で比べるので、`u64::MAX` は比較が偽になるだけで
    /// overflow しない ([`crate::links::read_checked`]、[`crate::parser::ooxml::read_zip_part`]、
    /// [`xlsx`] の pre-flight)。
    pub fn cap_bytes(&self) -> u64 {
        match self {
            Self::Bytes(n) => *n,
            Self::Unlimited => u64::MAX,
        }
    }
}

impl std::fmt::Display for FileSizeLimit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Bytes(n) => write!(f, "{n} bytes"),
            Self::Unlimited => f.write_str("unlimited"),
        }
    }
}

/// [`FileSizeLimit`] の TOML 上の書き方 3 種。
///
/// [`FileSizeLimit`] 自体を untagged にしないのは、untagged だと `TryFrom` のエラーが
/// "did not match any variant" に潰れて案内文が消えるため。`Int` を `i64` で受けるのは
/// 負値を自分の言葉で断るため (`u64` だと serde 段の型エラーになる)、`Float` を受けるのは
/// `1.5` に「小数は受けない」と答えるため。
#[derive(Deserialize)]
#[serde(untagged)]
enum RawFileSizeLimit {
    Int(i64),
    Float(f64),
    Str(String),
}

/// `0` / 負値 / `"0 MiB"` に添える案内。
const UNLIMITED_HINT: &str = "use \"unlimited\" to lift the limit";

/// 小数に添える案内。
const NO_FRACTIONS: &str =
    "fractions are not accepted (write \"1536 MiB\" rather than \"1.5 GiB\")";

impl TryFrom<RawFileSizeLimit> for FileSizeLimit {
    type Error = String;

    fn try_from(raw: RawFileSizeLimit) -> std::result::Result<Self, Self::Error> {
        match raw {
            RawFileSizeLimit::Int(n) => match u64::try_from(n) {
                Ok(bytes) if bytes >= 1 => Ok(Self::Bytes(bytes)),
                _ => Err(format!(
                    "a size limit must be at least 1 byte, got {n}; {UNLIMITED_HINT}"
                )),
            },
            RawFileSizeLimit::Float(x) => Err(format!(
                "a size limit is a whole number of bytes, got {x}; {NO_FRACTIONS}"
            )),
            RawFileSizeLimit::Str(s) => parse_size_text(&s),
        }
    }
}

/// 文字列で書かれた [`FileSizeLimit`] を読む。`"unlimited"`、または
/// `<整数><空白?><単位>` (単位なしは bytes)。
fn parse_size_text(text: &str) -> std::result::Result<FileSizeLimit, String> {
    let trimmed = text.trim();
    if trimmed.eq_ignore_ascii_case("unlimited") {
        return Ok(FileSizeLimit::Unlimited);
    }
    let digits_end = trimmed
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(trimmed.len());
    let (digits, rest) = trimmed.split_at(digits_end);
    if digits.is_empty() {
        return Err(format!(
            "expected a byte count, a size such as \"300 MiB\", or \"unlimited\", got {text:?}"
        ));
    }
    if rest.starts_with('.') {
        return Err(format!(
            "a size limit is a whole number of bytes, got {text:?}; {NO_FRACTIONS}"
        ));
    }
    let unit = rest.trim_start();
    let multiplier: u64 = match unit.to_ascii_lowercase().as_str() {
        "" | "b" => 1,
        "kb" => 1_000,
        "mb" => 1_000_000,
        "gb" => 1_000_000_000,
        "tb" => 1_000_000_000_000,
        "kib" => 1 << 10,
        "mib" => 1 << 20,
        "gib" => 1 << 30,
        "tib" => 1 << 40,
        _ => {
            return Err(format!(
                "unknown unit {unit:?} in {text:?}; use B, KB, MB, GB, TB, KiB, MiB, GiB or TiB"
            ));
        }
    };
    let overflow = || format!("{text:?} is more bytes than this build can count");
    let count: u64 = digits.parse().map_err(|_| overflow())?;
    if count == 0 {
        return Err(format!(
            "a size limit must be at least 1 byte, got {text:?}; {UNLIMITED_HINT}"
        ));
    }
    count
        .checked_mul(multiplier)
        .map(FileSizeLimit::Bytes)
        .ok_or_else(overflow)
}

/// (feature-61) `[index]` の 3 つのサイズ上限。
///
/// [`Registry`] が運び ([`Registry::limits`])、indexer は raw 上限 2 つを、binary parser は
/// 構築時に展開 budget を読む。[`crate::config::Config::file_size_limits`] が `[index]` から
/// 解決する。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileSizeLimits {
    /// `[index].max_binary_file_size` — pdf / docx / xlsx / pptx の生バイト上限。
    pub binary: FileSizeLimit,
    /// `[index].max_text_file_size` — md / txt (と registry の非 binary parser) の生バイト上限。
    pub text: FileSizeLimit,
    /// `[index].max_decompressed_size` — xlsx / docx / pptx の展開後合計と PDF の文書単位
    /// テキスト量。
    pub decompressed: FileSizeLimit,
}

impl Default for FileSizeLimits {
    fn default() -> Self {
        Self {
            binary: FileSizeLimit::Bytes(MAX_RAW_BINARY_BYTES),
            text: FileSizeLimit::Bytes(MAX_RAW_TEXT_BYTES),
            decompressed: FileSizeLimit::Bytes(DEFAULT_MAX_DECOMPRESSED_BYTES),
        }
    }
}

/// (feature-61) 3 つのどれかが既定より大きい (`"unlimited"` を含む) なら `true`。
///
/// 既定ちょうどと、下げた場合は `false`。[`crate::config::Config::build_parser_registry`] が
/// これで [`raised_limits_warning`] を出すかを決める (process ごとに 1 回)。
pub fn should_warn_raised_limits(limits: &FileSizeLimits) -> bool {
    let defaults = FileSizeLimits::default();
    limits.binary.cap_bytes() > defaults.binary.cap_bytes()
        || limits.text.cap_bytes() > defaults.text.cap_bytes()
        || limits.decompressed.cap_bytes() > defaults.decompressed.cap_bytes()
}

/// (feature-61) [`should_warn_raised_limits`] が `true` の時に出す警告文。stderr に出るので
/// ASCII のみ。
pub fn raised_limits_warning(limits: &FileSizeLimits) -> String {
    let defaults = FileSizeLimits::default();
    let mut msg = format!(
        concat!(
            "[index] sets a size cap above its built-in default: ",
            "binary {binary} (default {default_binary}), ",
            "text {text} (default {default_text}), ",
            "decompressed {decompressed} (default {default_decompressed}). ",
            "A file being indexed is held in memory whole, and an .xlsx also holds its ",
            "shared-strings table expanded; an allocation failure aborts the process ",
            "instead of skipping the file. PDF and .xlsx text extraction still stop after ",
            "{timeout} s whatever the cap."
        ),
        binary = limits.binary,
        default_binary = defaults.binary,
        text = limits.text,
        default_text = defaults.text,
        decompressed = limits.decompressed,
        default_decompressed = defaults.decompressed,
        timeout = pdf::PDF_DOC_EXTRACT_TIMEOUT.as_secs(),
    );
    if [limits.binary, limits.text, limits.decompressed].contains(&FileSizeLimit::Unlimited) {
        msg.push_str(
            " With \"unlimited\", memory use is bounded only by the files placed in the knowledge base.",
        );
    }
    msg
}

/// (AU-33) `ParsedDocument::raw_content` — the chunk bodies rejoined.
///
/// Four parsers built this identically (`docx`, `pdf`, `pptx`, `xlsx`), which
/// matters more than the line count: `raw_content` is what `get_document`
/// returns, so a parser that joined differently would serve a document whose
/// text does not match the chunks that were indexed from it. Sharing the join
/// is what keeps that impossible rather than merely unlikely.
///
/// The separator is a blank line, matching how Markdown documents read back.
pub(crate) fn join_chunk_bodies(chunks: &[Chunk]) -> String {
    chunks
        .iter()
        .map(|c| c.content.as_str())
        .collect::<Vec<_>>()
        .join(CHUNK_BODY_SEPARATOR)
}

/// What [`join_chunk_bodies`] puts between two chunk bodies. The `.xlsx` parser counts it
/// toward a workbook's text budget, so it reads the same constant rather than a copy.
pub(crate) const CHUNK_BODY_SEPARATOR: &str = "\n\n";

/// (AU-33) One spreadsheet sheet as a chunk.
///
/// Both workbook readers built this the same way. Keeping it in one place
/// keeps the `Sheet: <name>` heading, the level, and the trailing-whitespace
/// trim from drifting apart between the two — the heading in particular is
/// weighted in FTS, so a difference there would change retrieval for one
/// format and not the other.
pub(crate) fn sheet_chunk(index: usize, title: &str, name: &str, text: &str) -> Chunk {
    let heading = format!("Sheet: {name}");
    let context = build_context(&[title, &heading]);
    Chunk {
        index,
        heading: Some(heading),
        level: Some(2),
        content: text.trim_end().to_string(),
        context,
        line_range: None,
        symbol_kind: None,
        table_rows: None,
    }
}

/// パンくず合成 (feature-46)。空要素 skip + 連続重複 skip + `" > "` join + 200 chars
/// cap (char boundary 安全)。全要素が空なら `None`。
///
/// 例: `build_context(&["設計ノート", "検索パイプライン", "RRF の実装"])`
///     → `Some("設計ノート > 検索パイプライン > RRF の実装")`
pub(crate) fn build_context(parts: &[&str]) -> Option<String> {
    let trimmed: Vec<&str> = parts.iter().map(|p| p.trim()).collect();
    join_context(&trimmed)
}

/// BGE-small の 512 token 制限保護 + 異常に長い見出しへの防御 (spec D-11)。
const MAX_CONTEXT_CHARS: usize = 200;

/// (audit AX-04) A heading as it is kept on an ancestry stack: trimmed and cut to the
/// context cap, on a char boundary.
///
/// The stack is cloned into every chunk below the heading, so keeping the full text made a
/// long heading cost its length once per child (a 2 MB heading over 1000 children peaked at
/// 2,094 MB in the audit). The cut cannot change a context: an element that is longer than
/// the cap fills the rest of the breadcrumb by itself, so nothing past its first
/// [`MAX_CONTEXT_CHARS`] chars, nor any element after it, reaches the output. Pass the
/// result to [`heading_context`], never to [`build_context`] -- that one trims each part
/// again, and a cut that ends in whitespace would come out shorter than the uncut heading.
pub(crate) fn context_ancestor(heading: &str) -> String {
    heading.trim().chars().take(MAX_CONTEXT_CHARS).collect()
}

/// The breadcrumb `[title, ...ancestry, heading]` of a heading-chunked document (Markdown,
/// docx). `ancestry` holds [`context_ancestor`] values; the title and the heading are
/// trimmed here as [`build_context`] does. Same output as [`build_context`] over the uncut
/// ancestors.
pub(crate) fn heading_context(
    title: Option<&str>,
    ancestry: &[String],
    heading: Option<&str>,
) -> Option<String> {
    let mut parts: Vec<&str> = Vec::with_capacity(ancestry.len() + 2);
    if let Some(t) = title {
        parts.push(t.trim());
    }
    parts.extend(ancestry.iter().map(String::as_str));
    if let Some(h) = heading {
        parts.push(h.trim());
    }
    join_context(&parts)
}

/// [`build_context`] after the trim: skips empty and consecutive duplicate parts, joins with
/// `" > "` and caps at [`MAX_CONTEXT_CHARS`].
fn join_context(parts: &[&str]) -> Option<String> {
    let mut out: Vec<&str> = Vec::with_capacity(parts.len());
    for &t in parts {
        if t.is_empty() {
            continue; // 空/whitespace-only は skip
        }
        if out.last().copied() == Some(t) {
            continue; // 連続する同一要素は skip (title==h1 等)
        }
        out.push(t);
    }
    if out.is_empty() {
        return None;
    }
    let joined = out.join(" > ");
    // 200 chars 超過時は末尾切り。char_indices ベース (take(N)) で char boundary 安全。
    let capped = if joined.chars().count() > MAX_CONTEXT_CHARS {
        joined.chars().take(MAX_CONTEXT_CHARS).collect::<String>()
    } else {
        joined
    };
    Some(capped)
}

/// 抽出済みテキストを 1 チャンクに包む共通 helper。バイナリ parser の trait 契約用
/// `parse` (&str 版 = 「既に抽出済みテキストを受け取った」fallback) 実装で使う。
/// path_hint からファイル名ベースの title を derive する。
// PR-1 時点では呼び出し元 (PDF/Office parser) が未実装のため未使用。
// PR-2/3 で消費予定 (feature-45)。
#[allow(dead_code)]
pub(crate) fn single_text_chunk(raw: &str, path_hint: &str) -> ParsedDocument {
    let body = raw.replace("\r\n", "\n").replace('\r', "\n");
    let title = txt::derive_title_pub(path_hint);
    let context = build_context(&[title.as_deref().unwrap_or("")]);
    let chunks = if body.trim().is_empty() {
        Vec::new()
    } else {
        vec![Chunk {
            index: 0,
            heading: None,
            level: None,
            content: body,
            context,
            line_range: None,
            symbol_kind: None,
            table_rows: None,
        }]
    };
    ParsedDocument {
        frontmatter: Frontmatter {
            title,
            ..Frontmatter::default()
        },
        chunks,
        raw_content: raw.to_string(),
        frontmatter_error: None,
    }
}

/// `ooxml::local_name_pub` の薄い re-export。namespace prefix 付き QName
/// (`w:p` / `w:pStyle` 等) から local part を取り出す。docx/pptx parser が
/// XML 要素名判定に使う (Task 3.4/3.5 で消費)。
pub(crate) fn ooxml_local(qname: &[u8]) -> &[u8] {
    ooxml::local_name_pub(qname)
}

// ---------------------------------------------------------------------------
// Typed parse-failure cause (#660)
// ---------------------------------------------------------------------------

/// (#660) Why a parser refused a document, carried inside the `anyhow::Error` it returns so the
/// indexer can classify the failure without reading the message. `Display` is the message,
/// unchanged, so every stderr line and every existing assertion on the text stays as it was.
#[derive(Debug)]
pub(crate) struct Unindexable {
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "read by the indexer in PR-2 (#660)")
    )]
    pub(crate) cause: ParseCause,
    message: String,
}

impl std::fmt::Display for Unindexable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for Unindexable {}

/// (#660) The kind of failure an [`Unindexable`] error stands for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ParseCause {
    /// The document is password-protected or encrypted.
    Encrypted,
    /// Extracting the document would go past a decompression / extraction cap.
    OverExtractionLimit { limit: u64 },
    /// The input is larger than a parser's own size cap.
    OverParserLimit { size: u64, limit: u64 },
    /// Extraction ran past its time budget.
    ExtractionTimedOut { seconds: u64 },
    /// The container or format is damaged, or a variant the parser does not read.
    CorruptOrUnsupported,
    /// The text the document yields decodes to mojibake.
    TextUndecodable,
    /// The document held too little text to index.
    TooLittleText,
}

/// `anyhow::Error::new(Unindexable { cause, message })`.
pub(crate) fn unindexable(cause: ParseCause, message: String) -> anyhow::Error {
    anyhow::Error::new(Unindexable { cause, message })
}

/// Test helper: the cause an error carries, through any `.context(..)` layers.
#[cfg(test)]
pub(crate) fn cause_of(e: &anyhow::Error) -> Option<ParseCause> {
    e.downcast_ref::<Unindexable>().map(|u| u.cause)
}

// ---------------------------------------------------------------------------
// Parser trait
// ---------------------------------------------------------------------------

/// A file-format parser plugin. One instance per supported extension.
///
/// Implementors must be `Send + Sync` because the Registry is shared across
/// server threads (MCP + future watcher).
pub trait Parser: Send + Sync {
    /// Lowercase extension this parser claims, **without** a leading dot
    /// (e.g. `"md"`, `"txt"`). Used for `walkdir` filtering.
    fn extension(&self) -> &'static str;

    /// Stable id used in `[parsers].enabled` of `groove.toml`. Typically equal
    /// to `extension()` but kept separate so future parsers can share logic
    /// (e.g. an `"mdx"` id that reuses Markdown parsing).
    fn id(&self) -> &'static str {
        self.extension()
    }

    /// Parse raw file content into frontmatter + chunks.
    ///
    /// - `raw` — full file text (already read to string)
    /// - `path_hint` — `kb_path` 相対の forward-slash path。frontmatter が無い
    ///   形式 (`.txt` 等) で title をファイル名から derive する時に使う
    /// - `exclude_headings` — 見出しベースのチャンク除外リスト (substring 一致)。
    ///   見出し概念のない形式は無視してよい
    fn parse(&self, raw: &str, path_hint: &str, exclude_headings: &[&str]) -> ParsedDocument;

    /// バイト列から parse する **実装点**。形式固有の抽出はここに書く。
    ///
    /// default impl = UTF-8 検証して `parse` に委譲する (md/txt は override 不要で動く)。
    /// バイナリ parser (Pdf/Docx/Xlsx/Xls/Pptx) はこれを override して形式固有の
    /// チャンクを直接生成する。`Err` の意味 = 「このファイルは index 不能」で、
    /// 呼び出し側が skip + warn を行う。
    ///
    /// **呼び出し側はこれを直接呼ばず [`ParserExt::parse_bytes`] を使うこと。**
    /// panic の隔離が効かなくなる。
    fn parse_bytes_inner(
        &self,
        bytes: &[u8],
        path_hint: &str,
        exclude_headings: &[&str],
    ) -> Result<ParsedDocument> {
        let s =
            std::str::from_utf8(bytes).with_context(|| format!("{path_hint}: not valid UTF-8"))?;
        Ok(self.parse(s, path_hint, exclude_headings))
    }

    /// The read path's implementation point: what `get_document` and
    /// `resources/read` parse with, as opposed to the index.
    ///
    /// (feature-61) The read path parses with the built-in default
    /// decompression budget ([`DEFAULT_MAX_DECOMPRESSED_BYTES`]) regardless of
    /// `[index].max_decompressed_size`, because one MCP request must stay
    /// bounded whatever the index is allowed to inflate (ADR-0026). Its raw-size
    /// sibling, the binary read cap, lives in [`crate::server`].
    ///
    /// The default delegates to [`Parser::parse_bytes_inner`], which is right
    /// for every parser whose parse does not depend on a construction-time
    /// budget. A parser whose [`Parser::parse_bytes_inner`] does depend on one
    /// overrides this to pass the default budget instead.
    ///
    /// **Callers go through [`ParserExt::parse_bytes_for_read`]**, for the same
    /// panic isolation as [`ParserExt::parse_bytes`]. The `_inner` suffix is
    /// load-bearing, as on [`Parser::parse_bytes_inner`]: on a `dyn Parser` a
    /// [`Parser`] method shadows a [`ParserExt`] method of the same name, so a
    /// shared name would route reads around the panic guard.
    fn parse_bytes_for_read_inner(
        &self,
        bytes: &[u8],
        path_hint: &str,
        exclude_headings: &[&str],
    ) -> Result<ParsedDocument> {
        self.parse_bytes_inner(bytes, path_hint, exclude_headings)
    }

    /// バイナリ形式 parser は `true` を返す (default `false`)。
    /// get_document の cap 分類 (§4.4) と quality filter 免除 (§4.8) の判定に使う。
    fn is_binary(&self) -> bool {
        false
    }

    /// The [`Frontmatter::topic`] that [`Parser::parse_bytes_inner`] would give `bytes`, without
    /// chunking them: what a same-bytes rename needs to rebuild the stored topic as the index
    /// does (`frontmatter.topic.or(path-derived)`, local Codex on PR #388).
    ///
    /// The default is `None`, which is what every format but Markdown puts there: `.txt`, the
    /// binary formats and code fill `title` at most, never `topic`. A parser that starts reading
    /// a topic from its own metadata overrides this from the same function its parse uses.
    ///
    /// **Callers go through [`ParserExt::frontmatter_topic`]**, for the same panic isolation as
    /// [`ParserExt::parse_bytes`] (the `_inner` suffix is load-bearing for the same reason).
    fn frontmatter_topic_inner(&self, _bytes: &[u8]) -> Option<String> {
        None
    }
}

/// 全 call site (indexer / server) が使う parse 入口。**`Parser` の実装側から
/// 差し替えられない** (blanket impl のため) のがこの trait の存在理由。
///
/// [`Parser::parse_bytes_inner`] に委譲しつつ、その中で起きた **panic を `Err` に
/// 正規化する** (full-audit 2026-07-26 AU-21)。parser は信頼できない外部入力を
/// calamine / zip / quick-xml / oxidize-pdf に食わせるため、依存 crate 由来の
/// panic を完全には排除できない。`indexer.rs` は `Err` を per-file skip するが
/// panic はその `match` を巻き戻して通り抜けるので、catch しないと壊れた 1
/// ファイルが `index` 実行全体を落とす。詳細は [`panic_guard`] のモジュール doc。
///
/// **なぜ `Parser` の default method ではないのか** (codex P2, PR #92 round 1):
/// default method は実装側が override でき、Rust に `final` は無い。override
/// された瞬間 (あるいは旧 API に合わせて書かれた古い実装が残っていた場合)
/// `indexer.rs` / `server.rs` の動的ディスパッチはその override を呼び、panic
/// 隔離を丸ごと素通りする — しかも通常のテストでは気付けない。blanket impl
/// (`impl<T: Parser + ?Sized> ParserExt for T`) にすると **実装側は定義を
/// 持てない** ので、隔離が doc コメントの約束ではなく型で保証される。
pub trait ParserExt {
    /// バイト列から parse し、panic を per-file `Err` に正規化する。
    fn parse_bytes(
        &self,
        bytes: &[u8],
        path_hint: &str,
        exclude_headings: &[&str],
    ) -> Result<ParsedDocument>;

    /// The read path's entry (`get_document` / `resources/read`): parses with
    /// the default decompression budget ([`Parser::parse_bytes_for_read_inner`]) and
    /// normalises a panic to a per-file `Err` like [`ParserExt::parse_bytes`].
    fn parse_bytes_for_read(
        &self,
        bytes: &[u8],
        path_hint: &str,
        exclude_headings: &[&str],
    ) -> Result<ParsedDocument>;

    /// [`Parser::frontmatter_topic_inner`] behind the panic guard. A panic reads as no topic:
    /// the bytes behind a rename are bytes the index already parsed without one, so the guard
    /// is a backstop, not an expected path.
    fn frontmatter_topic(&self, bytes: &[u8], path_hint: &str) -> Option<String>;
}

impl<T: Parser + ?Sized> ParserExt for T {
    fn parse_bytes(
        &self,
        bytes: &[u8],
        path_hint: &str,
        exclude_headings: &[&str],
    ) -> Result<ParsedDocument> {
        panic_guard::catch_parser_panic(path_hint, self.id(), || {
            self.parse_bytes_inner(bytes, path_hint, exclude_headings)
        })
    }

    fn parse_bytes_for_read(
        &self,
        bytes: &[u8],
        path_hint: &str,
        exclude_headings: &[&str],
    ) -> Result<ParsedDocument> {
        panic_guard::catch_parser_panic(path_hint, self.id(), || {
            self.parse_bytes_for_read_inner(bytes, path_hint, exclude_headings)
        })
    }

    fn frontmatter_topic(&self, bytes: &[u8], path_hint: &str) -> Option<String> {
        panic_guard::catch_parser_panic(path_hint, self.id(), || {
            Ok(self.frontmatter_topic_inner(bytes))
        })
        .ok()
        .flatten()
    }
}

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

/// `[parsers]` セクション (`groove.toml`)。
///
/// - キー省略時 (`parsers: None`) は `Registry::defaults()` (= `["md"]` のみ、
///   legacy 完全後方互換) を適用する。ユーザが `.txt` 等を index したい
///   場合は明示的に `enabled = ["md", "txt"]` と opt-in する。
/// - `enabled = []` は誤設定として reject する (全拡張子が無効 = index 結果が
///   空になる silent failure を防ぐ)。
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ParsersConfig {
    pub enabled: Vec<String>,
    /// `[parsers.code]` — settings that only apply to source-code parsers.
    #[serde(default)]
    pub code: CodeParsersConfig,
}

/// `[parsers.code]` セクション (`groove.toml`)。
///
/// Only one knob is exposed. The remaining limits a code parser enforces — the raw-byte
/// ceiling and the per-file chunk count — are constants: they exist to keep a pathological
/// file from taking the process down, and a setting whose only effect is to weaken that is
/// not worth the surface.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CodeParsersConfig {
    /// Budget for one chunk, in non-whitespace characters.
    ///
    /// A definition that fits becomes one chunk. One that does not is split — into its nested
    /// definitions where it has them, and by lines where it does not, which is the common case
    /// for a long function. Lowering this yields finer-grained hits at the cost of cutting
    /// bodies apart; raising it does the reverse.
    ///
    /// Values under [`code::MIN_FRAGMENT_CHARS`] are rejected by [`ParsersConfig::validate`].
    #[serde(default = "default_max_chunk_chars")]
    pub max_chunk_chars: usize,
}

fn default_max_chunk_chars() -> usize {
    code::DEFAULT_MAX_CHUNK_CHARS
}

impl Default for CodeParsersConfig {
    fn default() -> Self {
        Self {
            max_chunk_chars: default_max_chunk_chars(),
        }
    }
}

impl ParsersConfig {
    /// `enabled` が空、または `[parsers.code].max_chunk_chars` が
    /// [`code::MIN_FRAGMENT_CHARS`] 未満なら誤設定としてエラーを返す。load 時に呼ぶ。
    ///
    /// The floor is a quality bound, not an availability one: since ADR-0017 the chunker keeps
    /// the per-file chunk count under its cap by widening the budget, whatever the setting.
    /// [`code::MIN_FRAGMENT_CHARS`] is the smallest piece the chunker lets stand on its own —
    /// a smaller piece is folded into its neighbour, or dropped when it lies between
    /// definitions — so a budget under it asks for pieces the chunker then undoes, and no
    /// definition can be kept whole.
    ///
    /// Both checks run where the file is read, before the trust of its location is applied
    /// (see [`crate::config::Config::load_from`]): a value the file cannot mean is an error
    /// wherever the file sits, the same as an unknown key or an out-of-range `[search]` value.
    pub fn validate(&self) -> Result<()> {
        if self.enabled.is_empty() {
            anyhow::bail!(
                "[parsers].enabled must contain at least one id (got empty array). \
                 Remove the key entirely to use the default [\"md\"]."
            );
        }
        if self.code.max_chunk_chars < code::MIN_FRAGMENT_CHARS {
            anyhow::bail!(
                concat!(
                    "[parsers.code].max_chunk_chars must be >= {floor} ",
                    "(the smallest piece the code chunker keeps on its own; a budget below it ",
                    "asks for pieces the chunker merges or drops again, so no definition could ",
                    "stay whole), got {got}. ",
                    "Remove the key to use the default {default}."
                ),
                floor = code::MIN_FRAGMENT_CHARS,
                got = self.code.max_chunk_chars,
                default = code::DEFAULT_MAX_CHUNK_CHARS,
            );
        }
        Ok(())
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parsers_config_rejects_empty() {
        let cfg = ParsersConfig {
            enabled: vec![],
            code: CodeParsersConfig::default(),
        };
        let err = cfg.validate().expect_err("empty enabled must be an error");
        assert!(err.to_string().contains("empty array"));
    }

    #[test]
    fn test_parsers_config_accepts_non_empty() {
        let cfg = ParsersConfig {
            enabled: vec!["md".to_string()],
            code: CodeParsersConfig::default(),
        };
        cfg.validate().unwrap();
    }

    fn with_budget(max_chunk_chars: usize) -> ParsersConfig {
        ParsersConfig {
            enabled: vec!["md".to_string(), "rs".to_string()],
            code: CodeParsersConfig { max_chunk_chars },
        }
    }

    #[test]
    fn test_parsers_config_rejects_zero_max_chunk_chars() {
        let err = with_budget(0)
            .validate()
            .expect_err("a zero budget must be an error");
        let msg = err.to_string();
        assert!(
            msg.contains("[parsers.code].max_chunk_chars must be >= 30"),
            "{msg}"
        );
        assert!(msg.contains("got 0"), "{msg}");
        assert!(msg.contains("default 3500"), "{msg}");
    }

    #[test]
    fn test_parsers_config_rejects_max_chunk_chars_just_under_the_floor() {
        let floor = code::MIN_FRAGMENT_CHARS;
        let err = with_budget(floor - 1)
            .validate()
            .expect_err("one under the floor must be an error");
        assert!(err.to_string().contains(&format!("got {}", floor - 1)));
    }

    #[test]
    fn test_parsers_config_accepts_max_chunk_chars_at_the_floor() {
        with_budget(code::MIN_FRAGMENT_CHARS).validate().unwrap();
    }

    #[test]
    fn test_parsers_config_reports_empty_enabled_before_the_budget() {
        // Both faults at once: the message names the first check, so a user fixing
        // `enabled` is not left guessing which of the two the error was about.
        let cfg = ParsersConfig {
            enabled: vec![],
            code: CodeParsersConfig { max_chunk_chars: 0 },
        };
        let msg = cfg.validate().expect_err("must fail").to_string();
        assert!(msg.contains("[parsers].enabled"), "{msg}");
        assert!(!msg.contains("max_chunk_chars"), "{msg}");
    }

    #[test]
    fn test_chunk_default_has_level_none() {
        let c = Chunk::default();
        assert_eq!(c.index, 0);
        assert!(c.heading.is_none());
        assert!(c.level.is_none());
        assert_eq!(c.content, "");
        assert!(c.context.is_none()); // feature-46: default は None
    }

    #[test]
    fn test_parse_bytes_default_delegates_to_parse_on_utf8() {
        // MarkdownParser は parse_bytes を override しない = default impl 経由。
        let doc = MarkdownParser
            .parse_bytes(
                b"## H\n\nbody enough body enough body enough body enough",
                "x.md",
                &[],
            )
            .expect("valid utf-8 must parse");
        assert_eq!(doc.chunks.len(), 1);
        assert_eq!(doc.chunks[0].heading.as_deref(), Some("H"));
    }

    #[test]
    fn test_parse_bytes_default_errors_on_invalid_utf8() {
        // 不正 UTF-8 (0xFF 0xFE) は default impl が Err にする = index 全体 abort ではなく
        // per-file skip の起点。
        let err = TxtParser
            .parse_bytes(&[0xff, 0xfe, 0x00], "x.txt", &[])
            .expect_err("invalid utf-8 must be Err");
        assert!(err.to_string().contains("not valid UTF-8"));
    }

    #[test]
    fn test_is_binary_default_false_for_text_parsers() {
        assert!(!MarkdownParser.is_binary());
        assert!(!TxtParser.is_binary());
    }

    #[test]
    fn test_build_context_joins_with_breadcrumb() {
        assert_eq!(
            build_context(&["設計ノート", "検索パイプライン", "RRF の実装"]).as_deref(),
            Some("設計ノート > 検索パイプライン > RRF の実装")
        );
    }

    #[test]
    fn test_build_context_skips_empty_and_whitespace() {
        assert_eq!(
            build_context(&["title", "   ", "", "leaf"]).as_deref(),
            Some("title > leaf")
        );
    }

    #[test]
    fn test_build_context_skips_consecutive_duplicates() {
        // E-2: title と h1 が同一 → 1 回のみ
        assert_eq!(
            build_context(&["Foo", "Foo", "bar"]).as_deref(),
            Some("Foo > bar")
        );
    }

    #[test]
    fn test_build_context_all_empty_is_none() {
        // E-4: 全要素空 → None
        assert!(build_context(&["", "  ", "\t"]).is_none());
        assert!(build_context(&[]).is_none());
    }

    #[test]
    fn test_build_context_caps_at_200_chars_ascii() {
        // E-5: 200 chars 超は末尾 cap
        let long = "a".repeat(300);
        let ctx = build_context(&[&long]).unwrap();
        assert_eq!(ctx.chars().count(), 200);
    }

    /// (audit AX-04) The stacked form of a heading is trimmed and at most the cap, cut on a
    /// char boundary; a short heading is kept whole.
    #[test]
    fn context_ancestor_is_trimmed_and_cut_to_the_cap() {
        assert_eq!(context_ancestor("  short  "), "short");
        let cjk = context_ancestor(&"日本".repeat(1000));
        assert_eq!(cjk.chars().count(), 200);
        assert_eq!(cjk.len(), 600);
        assert_eq!(context_ancestor(&"a".repeat(200)), "a".repeat(200));
    }

    /// (audit AX-04) [`heading_context`] over cut ancestors equals [`build_context`] over the
    /// whole ones, and does not trim a cut that ends in whitespace.
    #[test]
    fn heading_context_over_cut_ancestors_is_build_context_over_whole_ones() {
        let long = format!(" {}\u{3000}\u{3000}{} ", "a".repeat(199), "b".repeat(10));
        let cut = vec![context_ancestor(&long)];
        assert_eq!(
            heading_context(Some(" T "), &cut, Some(" leaf ")),
            build_context(&[" T ", &long, " leaf "])
        );
        assert_eq!(heading_context(None, &cut, None), build_context(&[&long]));
    }

    #[test]
    fn test_build_context_caps_at_200_chars_cjk_boundary_safe() {
        // E-5: CJK でも char boundary 安全に cap (feature-45 B2 の boundary panic 再発防止)
        let long = "あ".repeat(300);
        let ctx = build_context(&[&long]).unwrap();
        assert_eq!(ctx.chars().count(), 200);
        // panic せず valid UTF-8 であること
        assert!(ctx.chars().all(|c| c == 'あ'));
    }

    // -----------------------------------------------------------------------
    // panic isolation (full-audit 2026-07-26 AU-21)
    // -----------------------------------------------------------------------

    /// `parse_bytes_inner` が必ず panic する fake parser。
    ///
    /// 実在の壊れたファイルに頼らず panic 隔離を検証するために使う。実 crate
    /// (calamine / zip / quick-xml) 由来の panic を狙う fixture は、crate の
    /// bug fix や整数 overflow チェックの有無 (debug/release) で「panic しなく
    /// なる」ため回帰テストの土台にできない。
    ///
    /// なお「`parse_bytes` を override して隔離を迂回する parser」は書けない —
    /// `parse_bytes` は `ParserExt` の blanket impl 側にあり、`Parser` の
    /// 実装者が定義を持てないため (この性質はコンパイル時に保証されるので
    /// テストで突く対象にならない)。
    struct PanickingParser;

    impl Parser for PanickingParser {
        fn extension(&self) -> &'static str {
            "boom"
        }

        fn parse(&self, raw: &str, path_hint: &str, _exclude_headings: &[&str]) -> ParsedDocument {
            single_text_chunk(raw, path_hint)
        }

        fn parse_bytes_inner(
            &self,
            _bytes: &[u8],
            _path_hint: &str,
            _exclude_headings: &[&str],
        ) -> Result<ParsedDocument> {
            panic!("simulated dependency panic");
        }
    }

    #[test]
    fn test_parse_bytes_isolates_parser_panic_as_err() {
        // AU-21: parser 内の panic が呼び出し側 (indexer / server) に伝播すると、
        // 壊れた 1 ファイルで `groove index` 実行全体が落ちる。trait の入口で
        // Err に正規化されることを保証する。
        let err = PanickingParser
            .parse_bytes(b"whatever", "docs/broken.boom", &[])
            .expect_err("a panicking parser must surface as Err, not unwind to the caller");
        let msg = err.to_string();
        assert!(msg.contains("docs/broken.boom"), "got: {msg}");
        assert!(msg.contains("boom parser panicked"), "got: {msg}");
        assert!(
            msg.contains("simulated dependency panic"),
            "the panic payload must survive into the error message, got: {msg}"
        );
    }

    #[test]
    fn test_parse_bytes_isolation_works_through_trait_object() {
        // Registry は `Box<dyn Parser>` で保持する = 実運用の call site は
        // 動的ディスパッチ。`ParserExt` の blanket impl は `T: Parser + ?Sized`
        // なので `dyn Parser` にも効くことを確認する (ここが効かないと
        // indexer / server の呼び出しだけ隔離から漏れる)。
        let parser: Box<dyn Parser> = Box::new(PanickingParser);
        assert!(
            parser.parse_bytes(b"x", "docs/broken.boom", &[]).is_err(),
            "panic isolation must also hold through `dyn Parser`"
        );
    }

    #[test]
    fn test_parse_bytes_still_returns_ok_for_healthy_parser() {
        // 隔離層が正常系の戻り値を変えないこと (回帰確認)。
        let doc = MarkdownParser
            .parse_bytes(b"## H\n\nbody enough body enough body enough", "x.md", &[])
            .expect("healthy parser must still return Ok through the panic guard");
        assert_eq!(doc.chunks.len(), 1);
    }

    // -----------------------------------------------------------------------
    // feature-61: [index] size caps
    // -----------------------------------------------------------------------

    /// One [`FileSizeLimit`] behind a key, so the values go through serde the way
    /// `[index]` puts them through it.
    #[derive(Debug, serde::Deserialize)]
    struct OneLimit {
        v: FileSizeLimit,
    }

    fn limit(toml_value: &str) -> std::result::Result<FileSizeLimit, String> {
        toml::from_str::<OneLimit>(&format!("v = {toml_value}\n"))
            .map(|one| one.v)
            .map_err(|e| e.to_string())
    }

    const MIB: u64 = 1024 * 1024;

    #[test]
    fn file_size_limit_parses_bytes_units_and_unlimited() {
        let cases: &[(&str, FileSizeLimit)] = &[
            ("1", FileSizeLimit::Bytes(1)),
            ("52428800", FileSizeLimit::Bytes(52_428_800)),
            ("\"300 MiB\"", FileSizeLimit::Bytes(300 * MIB)),
            ("\"300MiB\"", FileSizeLimit::Bytes(300 * MIB)),
            ("\"2 GB\"", FileSizeLimit::Bytes(2_000_000_000)),
            ("\"7 KB\"", FileSizeLimit::Bytes(7_000)),
            ("\"3 MB\"", FileSizeLimit::Bytes(3_000_000)),
            ("\"1 TB\"", FileSizeLimit::Bytes(1_000_000_000_000)),
            ("\"4 KiB\"", FileSizeLimit::Bytes(4 * 1024)),
            ("\"16 GiB\"", FileSizeLimit::Bytes(16 * 1024 * MIB)),
            ("\"1 TiB\"", FileSizeLimit::Bytes(1024 * 1024 * MIB)),
            ("\"512 B\"", FileSizeLimit::Bytes(512)),
            ("\"4096\"", FileSizeLimit::Bytes(4096)),
            ("\"unlimited\"", FileSizeLimit::Unlimited),
            ("\"UNLIMITED\"", FileSizeLimit::Unlimited),
        ];
        for (text, want) in cases {
            assert_eq!(limit(text), Ok(*want), "v = {text}");
        }
        assert_eq!(FileSizeLimit::Bytes(42).cap_bytes(), 42);
        assert_eq!(FileSizeLimit::Unlimited.cap_bytes(), u64::MAX);
        let defaults = FileSizeLimits::default();
        for l in [defaults.binary, defaults.text, defaults.decompressed] {
            assert_eq!(
                l.cap_bytes(),
                50 * MIB,
                "every default is the previous fixed 50 MiB"
            );
        }
    }

    #[test]
    fn file_size_limit_rejects_zero_and_points_at_unlimited() {
        for text in ["0", "-1", "\"0 MiB\""] {
            let err = limit(text).expect_err(text);
            assert!(
                err.contains("use \"unlimited\" to lift the limit"),
                "v = {text}: {err}"
            );
        }
    }

    #[test]
    fn file_size_limit_rejects_fractions_unknown_units_and_empty() {
        for text in ["1.5", "\"1.5 GiB\""] {
            let err = limit(text).expect_err(text);
            assert!(
                err.contains("fractions are not accepted"),
                "v = {text}: {err}"
            );
        }
        let err = limit("\"300 XB\"").expect_err("unknown unit");
        assert!(err.contains("unknown unit"), "{err}");
        for text in ["\"\"", "\"unlimited!\"", "\"lots\""] {
            let err = limit(text).expect_err(text);
            assert!(
                err.contains("a size such as \"300 MiB\", or \"unlimited\""),
                "v = {text}: {err}"
            );
        }
    }

    /// Review Focus 1: spacing and case are the operator's, not an error; a
    /// digit run too long for u64 is an error with a reason, not a panic.
    #[test]
    fn file_size_limit_accepts_spacing_and_case_and_refuses_overflow() {
        assert_eq!(limit("\" 300 mib \""), Ok(FileSizeLimit::Bytes(300 * MIB)));
        assert_eq!(limit("\"300  MiB\""), Ok(FileSizeLimit::Bytes(300 * MIB)));
        assert_eq!(limit("\" Unlimited \""), Ok(FileSizeLimit::Unlimited));
        assert_eq!(
            limit("\"50 MiB\""),
            Ok(FileSizeLimits::default().binary),
            "writing the default out is the default"
        );
        for text in ["\"18446744073709551616\"", "\"99999999999 TiB\""] {
            let err = limit(text).expect_err(text);
            assert!(
                err.contains("more bytes than this build can count"),
                "v = {text}: {err}"
            );
        }
    }

    #[test]
    fn raising_a_cap_above_the_default_is_announced_and_lowering_is_not() {
        let d = FileSizeLimits::default();
        assert!(
            !should_warn_raised_limits(&d),
            "the defaults are not a raise"
        );

        let lowered = FileSizeLimits {
            binary: FileSizeLimit::Bytes(1024),
            text: FileSizeLimit::Bytes(1),
            decompressed: FileSizeLimit::Bytes(1),
        };
        assert!(
            !should_warn_raised_limits(&lowered),
            "lowering is not announced"
        );

        // Review Focus 2: naming the default value is not a raise.
        let at_default = FileSizeLimits {
            binary: FileSizeLimit::Bytes(MAX_RAW_BINARY_BYTES),
            text: FileSizeLimit::Bytes(MAX_RAW_TEXT_BYTES),
            decompressed: FileSizeLimit::Bytes(DEFAULT_MAX_DECOMPRESSED_BYTES),
        };
        assert!(!should_warn_raised_limits(&at_default));

        let timeout = format!("{} s", pdf::PDF_DOC_EXTRACT_TIMEOUT.as_secs());
        for raised in [
            FileSizeLimits {
                binary: FileSizeLimit::Bytes(MAX_RAW_BINARY_BYTES + 1),
                ..d
            },
            FileSizeLimits {
                text: FileSizeLimit::Unlimited,
                ..d
            },
            FileSizeLimits {
                decompressed: FileSizeLimit::Bytes(16 * 1024 * MIB),
                ..d
            },
        ] {
            assert!(should_warn_raised_limits(&raised), "{raised:?}");
            let msg = raised_limits_warning(&raised);
            assert!(msg.is_ascii(), "the warning goes to stderr: {msg}");
            assert!(msg.contains("aborts the process"), "{msg}");
            assert!(msg.contains(&timeout), "{msg}");
        }

        let unlimited = FileSizeLimits {
            binary: FileSizeLimit::Unlimited,
            ..d
        };
        assert!(
            raised_limits_warning(&unlimited)
                .contains("bounded only by the files placed in the knowledge base")
        );
        let finite = FileSizeLimits {
            binary: FileSizeLimit::Bytes(6 * MAX_RAW_BINARY_BYTES),
            ..d
        };
        assert!(!raised_limits_warning(&finite).contains("bounded only by"));
    }

    /// feature-66 P11 (AC10): Markdown, plain text and spreadsheet chunks carry no table rows.
    #[test]
    fn prose_and_sheet_chunks_carry_no_table_rows() {
        let md = MarkdownParser.parse("# Harbour\n\nquiet harbour notes\n", "harbour.md", &[]);
        let txt = TxtParser.parse("plain ledger words\n", "ledger.txt", &[]);
        assert!(!md.chunks.is_empty() && !txt.chunks.is_empty());
        for chunk in md.chunks.iter().chain(&txt.chunks) {
            assert_eq!(chunk.table_rows, None, "{chunk:?}");
        }
        assert_eq!(
            sheet_chunk(0, "book", "Quota", "signer\tlimit").table_rows,
            None
        );
    }

    /// feature-66 (R1.3 1 / 3): spans that leave the content, split a character, are empty or
    /// touch do not fit.
    #[test]
    fn table_rows_fit_refuses_spans_a_reader_could_not_slice() {
        let content = "über\tbirch\ncedar";
        assert!(table_rows_fit(content, &[]));
        assert!(table_rows_fit(
            content,
            &[row_span(0, 0, 11), row_span(0, 12, 17)]
        ));
        assert!(
            !table_rows_fit(content, &[row_span(0, 0, 18)]),
            "past the end"
        );
        assert!(
            !table_rows_fit(content, &[row_span(0, 1, 11)]),
            "inside a character"
        );
        assert!(!table_rows_fit(content, &[row_span(0, 5, 5)]), "empty");
        assert!(
            !table_rows_fit(content, &[row_span(0, 0, 11), row_span(0, 11, 17)]),
            "touching"
        );
        assert!(
            !table_rows_fit(content, &[row_span(0, 12, 17), row_span(0, 0, 11)]),
            "out of order"
        );
    }

    /// feature-66 (R1.3 4): a span that starts or ends inside a line does not fit, so a stored
    /// value cannot hand a reader part of a line as a row.
    #[test]
    fn table_rows_fit_refuses_spans_that_do_not_cover_whole_lines() {
        let content = "lead\nalder\tbirch\ncedar";
        assert!(table_rows_fit(content, &[row_span(0, 5, 16)]));
        assert!(
            !table_rows_fit(content, &[row_span(0, 6, 16)]),
            "starts inside a line"
        );
        assert!(
            !table_rows_fit(content, &[row_span(0, 5, 15)]),
            "ends inside a line"
        );
        assert!(
            !table_rows_fit(content, &[row_span(0, 17, 20)]),
            "ends before the content's last byte"
        );
        assert!(table_rows_fit(content, &[row_span(0, 17, 22)]), "last line");
    }

    /// feature-66 (R1.3 3): spans that overlap or come out of order do not fit, also when each
    /// covers whole lines.
    #[test]
    fn table_rows_fit_refuses_overlapping_or_unsorted_whole_line_spans() {
        let content = "alder\nbirch\ncedar";
        assert!(
            !table_rows_fit(content, &[row_span(0, 0, 11), row_span(0, 6, 11)]),
            "overlap"
        );
        assert!(
            !table_rows_fit(content, &[row_span(0, 6, 11), row_span(0, 0, 5)]),
            "unsorted"
        );
    }

    /// feature-66 (R1.3 5): table numbers start at 0 and never skip one; returning to an earlier
    /// table is allowed.
    #[test]
    fn table_rows_fit_refuses_table_numbers_that_skip() {
        let content = "alder\nbirch\ncedar";
        assert!(
            !table_rows_fit(content, &[row_span(1, 0, 5)]),
            "numbering starts at 1"
        );
        assert!(
            !table_rows_fit(content, &[row_span(0, 0, 5), row_span(2, 6, 11)]),
            "gap 0 -> 2"
        );
        assert!(table_rows_fit(
            content,
            &[row_span(0, 0, 5), row_span(1, 6, 11), row_span(0, 12, 17)]
        ));
    }

    /// feature-66 (R1.3 / spec §9 U2): a value with several tables fits, and so does a span over
    /// a row whose cell text carries its own newline.
    #[test]
    fn table_rows_fit_accepts_several_tables_and_a_cell_newline() {
        let content = "p\na\tb\nc\td\nq\ne\tf";
        assert!(table_rows_fit(
            content,
            &[row_span(0, 2, 5), row_span(0, 6, 9), row_span(1, 12, 15)]
        ));
        let content = "p\nfirst\nline\tb\nq";
        assert!(table_rows_fit(content, &[row_span(0, 2, 14)]));
    }

    /// #660 (spec T-P11): the non-UTF-8 error a text parser returns through the public
    /// [`ParserExt::parse_bytes`] entrance still downcasts to `Utf8Error`, so the indexer can tell
    /// it apart from a typed [`Unindexable`] cause without reading the message.
    #[test]
    fn a_non_utf8_markdown_error_still_downcasts_to_utf8_error() {
        let err = MarkdownParser
            .parse_bytes(b"\xff\xfe# x", "a.md", &[])
            .unwrap_err();
        assert!(
            err.downcast_ref::<std::str::Utf8Error>().is_some(),
            "{err:#}"
        );
    }

    /// #660 (spec T-P11): a cause put on an error survives `.context(..)`, and the message the
    /// error carries is the one it was built with.
    #[test]
    fn an_unindexable_cause_survives_context_and_keeps_its_message() {
        let err = unindexable(ParseCause::Encrypted, "a.pdf: locked".to_string()).context("outer");
        assert_eq!(cause_of(&err), Some(ParseCause::Encrypted));
        assert_eq!(err.root_cause().to_string(), "a.pdf: locked");
    }
}
