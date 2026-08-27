//! Parser registry. Owns a set of `Box<dyn Parser>` keyed by lowercase
//! extension. Shared across indexer / server / future watcher.

use anyhow::Result;

// `XlsParser` はここでは import しない: AU-06 で registry から外したため
// (型そのものは `parser::XlsParser` として残っており、その unit test も残る)。
use super::{
    CodeParsersConfig, DocxParser, MarkdownParser, Parser, PdfParser, PptxParser, TxtParser,
    XlsxParser,
};

/// Every id this build recognises, whether or not it can act on it.
///
/// Kept separate from what the build can actually construct: a grammar can be compiled out,
/// and an id that is real but unavailable deserves a different answer than a typo. Without
/// this list the two are indistinguishable and the user is told to check their spelling.
const KNOWN_IDS: &[&str] = &["md", "txt", "pdf", "docx", "xlsx", "pptx", "rs"];

/// Ids this build can build a parser for, in the order the diagnostic lists them.
fn available_ids() -> Vec<&'static str> {
    let mut ids: Vec<&'static str> = vec!["md", "txt", "pdf", "docx", "xlsx", "pptx"];
    if cfg!(feature = "grammar-rust") {
        ids.push("rs");
    }
    ids
}

/// The diagnostic for an id that resolved to nothing.
///
/// A pure function on purpose. The "compiled without the grammar" branch only ever fires in a
/// build where that feature is off, and CI only *checks* such a build — it never runs its
/// tests. Taking the id sets as arguments means the wording can be pinned by an ordinary test
/// under default features, instead of living in a binary nobody executes.
pub(crate) fn unresolved_id_message(id: &str, known: &[&str], available: &[&str]) -> String {
    if known.contains(&id) {
        format!(
            "[parsers].enabled contains {id:?}, which this build recognises but was compiled \
             without a grammar for. Rebuild with default features to parse it."
        )
    } else {
        format!(
            "[parsers].enabled contains unknown id {id:?}; supported in this build: {}",
            available.join(", ")
        )
    }
}

#[cfg(feature = "grammar-rust")]
fn rust_parser(code: &CodeParsersConfig) -> Result<Box<dyn Parser>> {
    let grammar = super::code::static_rust::grammar()?;
    Ok(Box::new(super::CodeParser::new(
        grammar,
        "rs",
        code.max_chunk_chars,
    )))
}

#[cfg(not(feature = "grammar-rust"))]
fn rust_parser(_code: &CodeParsersConfig) -> Result<Box<dyn Parser>> {
    anyhow::bail!(unresolved_id_message("rs", KNOWN_IDS, &available_ids()))
}

pub struct Registry {
    parsers: Vec<Box<dyn Parser>>,
    /// (feature-56) The chunk budget the code parsers here were built with, or `None` when no
    /// code parser is registered.
    ///
    /// Kept on the registry because a [`Parser`] takes no configuration at parse time: the
    /// budget is baked into the instance, so this is the only place left that still knows the
    /// number the chunks in a given index were cut at.
    code_max_chunk_chars: Option<usize>,
}

impl Registry {
    /// Build a Registry from a list of parser ids (from `[parsers].enabled`).
    /// Unknown ids fail loudly — this catches typos (`"markdown"` instead of
    /// `"md"`) and parsers that don't exist yet (`"rst"` / `"adoc"`).
    pub fn from_enabled(ids: &[String]) -> Result<Self> {
        Self::from_enabled_with_code(ids, &CodeParsersConfig::default())
    }

    /// Same, with the `[parsers.code]` settings a code parser needs.
    ///
    /// A separate constructor rather than a parameter on [`Registry::from_enabled`] so that
    /// the existing one keeps its meaning — "build from ids alone" — for the callers and
    /// tests that have no configuration to hand.
    pub fn from_enabled_with_code(ids: &[String], code: &CodeParsersConfig) -> Result<Self> {
        if ids.is_empty() {
            anyhow::bail!("[parsers].enabled must contain at least one id (got empty list)");
        }
        let mut parsers: Vec<Box<dyn Parser>> = Vec::with_capacity(ids.len());
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut code_max_chunk_chars = None;
        for id in ids {
            let lower = id.to_ascii_lowercase();
            if !seen.insert(lower.clone()) {
                anyhow::bail!("[parsers].enabled contains duplicate id {:?}", id);
            }
            let parser: Box<dyn Parser> = match lower.as_str() {
                "md" => Box::new(MarkdownParser),
                "txt" => Box::new(TxtParser),
                "pdf" => Box::new(PdfParser),
                "xlsx" => Box::new(XlsxParser),
                // AU-06: `.xls` は無効。`XlsParser` 自体は残してあるが、
                // ここで registry に載せない = indexing から到達しない。
                "xls" => anyhow::bail!(
                    "[parsers].enabled contains \"xls\", which this build does not index. \
                     Reading a .xls workbook makes calamine materialise one dense cell grid \
                     per sheet before groove regains control, and the BIFF format bounds a \
                     sheet (65536 x 256 = 512 MB) but not a workbook, so a small crafted \
                     file \
                     can declare enough sheets to exhaust memory, and an allocation failure \
                     aborts the process rather than skipping the file. Convert the workbook \
                     to .xlsx, which is read as a stream."
                ),
                "docx" => Box::new(DocxParser),
                "pptx" => Box::new(PptxParser),
                // (feature-56) The one grammar compiled in. Others are loaded from a plugin
                // directory, which arrives with the loader.
                "rs" => {
                    code_max_chunk_chars = Some(code.max_chunk_chars);
                    rust_parser(code)?
                }
                other => anyhow::bail!(unresolved_id_message(other, KNOWN_IDS, &available_ids())),
            };
            parsers.push(parser);
        }
        Ok(Self {
            parsers,
            code_max_chunk_chars,
        })
    }

    /// Default registry: `["md"]` only. Pre-feature-20 behaviour — `.txt`
    /// support is opt-in via `groove.toml` `[parsers].enabled = ["md", "txt"]`.
    pub fn defaults() -> Self {
        Self {
            parsers: vec![Box::new(MarkdownParser)],
            code_max_chunk_chars: None,
        }
    }

    /// (feature-56) The chunk budget the code parsers were built with, or `None` when this
    /// registry has none.
    pub fn code_max_chunk_chars(&self) -> Option<usize> {
        self.code_max_chunk_chars
    }

    /// Lookup a parser by file extension (lowercase, no leading dot).
    /// Case-insensitive match.
    pub fn by_extension(&self, ext: &str) -> Option<&dyn Parser> {
        self.parsers
            .iter()
            .find(|p| p.extension().eq_ignore_ascii_case(ext))
            .map(|b| b.as_ref())
    }

    /// All enabled extensions, used by `walkdir` filtering and by the
    /// (future) file watcher to limit fsnotify events.
    pub fn extensions(&self) -> Vec<&'static str> {
        self.parsers.iter().map(|p| p.extension()).collect()
    }

    /// True if `ext` (without leading dot) is registered. Case-insensitive,
    /// matching [`Registry::by_extension`] and the indexer's walker.
    ///
    /// full-audit 2026-07-26 AU-02: this used to compare with `==`, while
    /// every other extension check in the codebase uses
    /// `eq_ignore_ascii_case`. Because `validate_get_document_path`
    /// (`server.rs`) gates on this function, `Report.PDF` was indexed by the
    /// walker but then rejected by `get_document` — a hit users could find
    /// in search results yet never open.
    pub fn has_extension(&self, ext: &str) -> bool {
        self.parsers
            .iter()
            .any(|p| p.extension().eq_ignore_ascii_case(ext))
    }

    /// `is_binary()` が true な parser の拡張子だけを返す。indexer の size-skip
    /// 判定 (§4.2) と backfill_quality の is_binary 伝搬 (§4.8) で使う。
    pub fn binary_extensions(&self) -> Vec<&'static str> {
        self.parsers
            .iter()
            .filter(|p| p.is_binary())
            .map(|p| p.extension())
            .collect()
    }
}

impl Default for Registry {
    fn default() -> Self {
        Self::defaults()
    }
}

impl std::fmt::Debug for Registry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Registry")
            .field("extensions", &self.extensions())
            .finish()
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_defaults_is_md_only() {
        let r = Registry::defaults();
        assert_eq!(r.extensions(), vec!["md"]);
        assert!(r.by_extension("md").is_some());
        assert!(r.by_extension("txt").is_none());
    }

    #[test]
    fn test_from_enabled_md_and_txt() {
        let r = Registry::from_enabled(&["md".into(), "txt".into()]).unwrap();
        let exts = r.extensions();
        assert!(exts.contains(&"md"));
        assert!(exts.contains(&"txt"));
        assert!(r.by_extension("MD").is_some(), "should be case-insensitive");
        assert!(r.by_extension("TXT").is_some());
    }

    /// AU-06: `.xls` は registry に載せない。既に `enabled` に書いていた人が
    /// 黙って無視されるのではなく、理由と代替 (xlsx への変換) を読めること。
    #[test]
    fn from_enabled_refuses_xls_with_a_reason() {
        let err = Registry::from_enabled(&["md".into(), "xls".into()])
            .expect_err("xls must not be indexable in this build");
        let msg = err.to_string();
        assert!(msg.contains("xls"), "should name the id: {msg}");
        assert!(
            msg.contains(".xlsx"),
            "should point at the supported alternative: {msg}"
        );
    }

    /// `xls` は「未知の id」ではないので、未知 id の一覧にも載らない。
    #[test]
    fn the_supported_id_list_no_longer_advertises_xls() {
        let err = Registry::from_enabled(&["rst".into()]).expect_err("unknown id must fail");
        let msg = err.to_string();
        assert!(msg.contains("unknown id"), "unexpected message: {msg}");
        // `contains("xls")` は "xlsx" にも一致するので、id を単体の語として見る。
        let listed: Vec<&str> = msg
            .rsplit_once("supported in this build: ")
            .expect("message should list the supported ids")
            .1
            .split(',')
            .map(str::trim)
            .collect();
        assert!(
            !listed.contains(&"xls"),
            "supported list should not advertise xls: {listed:?}"
        );
        assert!(
            listed.contains(&"xlsx"),
            "xlsx is still supported: {listed:?}"
        );
    }

    #[test]
    fn test_from_enabled_rejects_empty() {
        let err = Registry::from_enabled(&[]).expect_err("empty must fail");
        assert!(err.to_string().contains("at least one id"));
    }

    #[test]
    fn test_from_enabled_rejects_unknown() {
        let err = Registry::from_enabled(&["rst".into()]).expect_err("unknown id must fail");
        let msg = err.to_string();
        assert!(msg.contains("rst"));
        assert!(msg.contains("supported"));
    }

    #[test]
    fn test_from_enabled_rejects_duplicates() {
        let err = Registry::from_enabled(&["md".into(), "MD".into()])
            .expect_err("case-insensitive duplicate must fail");
        assert!(err.to_string().contains("duplicate"));
    }

    #[test]
    fn test_from_enabled_case_insensitive_id() {
        // "MD" in config normalises to "md" — both accepted
        let r = Registry::from_enabled(&["MD".into()]).unwrap();
        assert_eq!(r.extensions(), vec!["md"]);
    }

    #[test]
    fn test_binary_extensions_empty_for_text_only_registry() {
        // md / txt は is_binary=false なので binary_extensions は空。
        let r = Registry::from_enabled(&["md".into(), "txt".into()]).unwrap();
        assert!(r.binary_extensions().is_empty());
    }

    #[test]
    fn test_from_enabled_registers_office_formats_as_binary() {
        // feature-45 PR-3: xlsx/docx/pptx は全て is_binary=true。
        //
        // AU-06 (2026-07-27) で `xls` を registry から外したため、本 test の
        // 入力と期待値から `xls` を除いた。テストの意図 (Office 系は
        // is_binary=true として登録される) は変えていない。`xls` が拒否される
        // ことは `from_enabled_refuses_xls_with_a_reason` が別途固定する。
        let ids = ["xlsx", "docx", "pptx"].map(String::from);
        let r = Registry::from_enabled(&ids).unwrap();
        for ext in ["xlsx", "docx", "pptx"] {
            assert!(r.by_extension(ext).is_some(), "{ext} must be registered");
        }
        let mut binary_exts = r.binary_extensions();
        binary_exts.sort_unstable();
        assert_eq!(binary_exts, vec!["docx", "pptx", "xlsx"]);
    }

    /// Regression (full-audit 2026-07-26 AU-02): `has_extension` だけが
    /// case-sensitive で、他の拡張子照合 (`by_extension`、indexer の walker) は
    /// すべて `eq_ignore_ascii_case`。この非対称のせいで `Report.PDF` は
    /// **index されるのに `get_document` が拒否する** (server.rs の
    /// `validate_get_document_path` が `has_extension` を使うため)。
    /// 大文字拡張子は Windows のメールクライアントやスキャナ出力で日常的に出る。
    #[test]
    fn test_has_extension_is_case_insensitive_like_by_extension() {
        let ids = ["md", "pdf"].map(String::from);
        let r = Registry::from_enabled(&ids).unwrap();
        for ext in ["pdf", "PDF", "Pdf", "md", "MD"] {
            assert!(
                r.has_extension(ext),
                "has_extension({ext:?}) must match by_extension's case-insensitive rule"
            );
            assert!(r.by_extension(ext).is_some(), "by_extension({ext:?})");
        }
        assert!(!r.has_extension("exe"));
        assert!(!r.has_extension(""));
    }
}
