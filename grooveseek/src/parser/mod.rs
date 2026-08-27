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
pub use registry::Registry;
pub use txt::TxtParser;
pub use xlsx::{XlsParser, XlsxParser};

// ---------------------------------------------------------------------------
// Data types (formerly in src/markdown.rs)
// ---------------------------------------------------------------------------

/// Metadata extracted from a document header (YAML frontmatter for `.md`,
/// filename-derived for `.txt`, etc.).
#[derive(Debug, Clone, Default)]
pub struct Frontmatter {
    pub title: Option<String>,
    pub date: Option<String>,
    pub topic: Option<String>,
    pub depth: Option<String>,
    pub tags: Vec<String>,
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
}

/// A fully parsed document: frontmatter + chunks + retained raw content.
#[derive(Debug, Clone)]
pub struct ParsedDocument {
    pub frontmatter: Frontmatter,
    pub chunks: Vec<Chunk>,
    pub raw_content: String,
}

/// Section headings excluded by default when the caller does not override.
/// Empty by default; callers typically configure this via `groove.toml`'s
/// `exclude_headings` key. Matching is substring-based inside the Markdown
/// chunker.
pub const DEFAULT_EXCLUDED_HEADINGS: &[&str] = &[];

/// バイナリ形式ファイルの生バイト上限 (50 MiB)。index 時の size skip (indexer)
/// と get_document の raw cap (server) で共有する。テキスト形式には
/// [`MAX_RAW_TEXT_BYTES`] を使う。
pub const MAX_RAW_BINARY_BYTES: u64 = 50 * 1024 * 1024;

/// (BU-02) テキスト形式ファイルの生バイト上限 (50 MiB)。
///
/// もともとテキストには index 時の上限が無く、`fs::read` が丸ごとメモリに
/// 載せていた。`rebuild_index` は MCP ツールとしてクライアントから叩けるので、
/// KB に巨大な `.md` を 1 つ置くだけで任意のタイミングで OOM を誘発できた。
///
/// 値をバイナリ側と揃えてあるのは、**どちらもファイル 1 本を丸ごとメモリに
/// 載せる**という同じ制約から来ているため。50 MiB の Markdown は
/// 2,000 万文字級で、正当な知識ベースの文書としては現実的でない。
pub const MAX_RAW_TEXT_BYTES: u64 = 50 * 1024 * 1024;

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
        .join("\n\n")
}

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
    }
}

/// パンくず合成 (feature-46)。空要素 skip + 連続重複 skip + `" > "` join + 200 chars
/// cap (char boundary 安全)。全要素が空なら `None`。
///
/// 例: `build_context(&["設計ノート", "検索パイプライン", "RRF の実装"])`
///     → `Some("設計ノート > 検索パイプライン > RRF の実装")`
pub(crate) fn build_context(parts: &[&str]) -> Option<String> {
    // BGE-small の 512 token 制限保護 + 異常に長い見出しへの防御 (spec D-11)。
    const MAX_CONTEXT_CHARS: usize = 200;

    let mut out: Vec<&str> = Vec::with_capacity(parts.len());
    for p in parts {
        let t = p.trim();
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
        }]
    };
    ParsedDocument {
        frontmatter: Frontmatter {
            title,
            ..Frontmatter::default()
        },
        chunks,
        raw_content: raw.to_string(),
    }
}

/// `ooxml::local_name_pub` の薄い re-export。namespace prefix 付き QName
/// (`w:p` / `w:pStyle` 等) から local part を取り出す。docx/pptx parser が
/// XML 要素名判定に使う (Task 3.4/3.5 で消費)。
pub(crate) fn ooxml_local(qname: &[u8]) -> &[u8] {
    ooxml::local_name_pub(qname)
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

    /// バイナリ形式 parser は `true` を返す (default `false`)。
    /// get_document の cap 分類 (§4.4) と quality filter 免除 (§4.8) の判定に使う。
    fn is_binary(&self) -> bool {
        false
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
    /// `enabled` が空なら誤設定としてエラーを返す。load 時に呼ぶ。
    pub fn validate(&self) -> Result<()> {
        if self.enabled.is_empty() {
            anyhow::bail!(
                "[parsers].enabled must contain at least one id (got empty array). \
                 Remove the key entirely to use the default [\"md\"]."
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
}
