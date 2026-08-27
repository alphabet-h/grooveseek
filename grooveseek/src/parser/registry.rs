//! Parser registry. Owns a set of `Box<dyn Parser>` keyed by lowercase
//! extension. Shared across indexer / server / future watcher.

use std::path::Path;

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
///
/// # Why the plugin ids come before the supported list, and are quoted
///
/// A reader — a test, or a person — takes everything after `supported in this build: ` and
/// splits it on commas. Two things follow. The list has to be the **last** clause: the spec
/// asked for the plugin ids to be appended after it, but measuring that showed it corrupts the
/// final element (`txt. These arrive as …`) rather than adding one, so the sentence moved in
/// front of the list instead. And the plugin ids are **quoted**, so that if a second plugin
/// language is ever added the comma between them cannot read as another supported id.
///
/// Between them, every comma-separated element after that marker stays an id this build can
/// act on with no file placed first.
pub(crate) fn unresolved_id_message(
    id: &str,
    known: &[&str],
    available: &[&str],
    plugin_ids: &[&str],
) -> String {
    if known.contains(&id) {
        return format!(
            "[parsers].enabled contains {id:?}, which this build recognises but was compiled \
             without a grammar for. Rebuild with default features to parse it."
        );
    }
    let plugins = if plugin_ids.is_empty() {
        String::new()
    } else {
        let quoted: Vec<String> = plugin_ids.iter().map(|p| format!("{p:?}")).collect();
        format!(
            "; these need a grammar plugin you place yourself: {}",
            quoted.join(" ")
        )
    };
    format!(
        "[parsers].enabled contains unknown id {id:?}{plugins}; supported in this build: {}",
        available.join(", ")
    )
}

/// (i) The id is a plugin's, the directory is known, and the file is not in it.
///
/// The release page is named without a URL. A test pins this wording, and a URL in it would
/// make that test fail the day the page moves — so the address lives in `docs/clients.md`,
/// where a stale link is visible to a reader rather than to CI.
pub(crate) fn plugin_missing_message(id: &str, dir: &Path, file_name: &str) -> String {
    format!(
        "[parsers].enabled contains {id:?}, whose grammar is a plugin, and {file_name} is not \
         in {}. Download the groove-grammar-{id}-<target> archive from the groove release page \
         (see docs/clients.md), unpack it, and put {file_name} there.",
        dir.display()
    )
}

/// (ii) The file is there and was refused.
pub(crate) fn plugin_rejected_message(id: &str, path: &Path, reason: &str) -> String {
    format!(
        "[parsers].enabled contains {id:?}, and {} was refused because {reason}. Take the \
         groove-grammar-{id}-<target> archive from the release for groove {} and use the file \
         from it.",
        path.display(),
        env!("CARGO_PKG_VERSION"),
    )
}

/// (iii) The id is a plugin's and there is no directory to name.
///
/// Distinct from (i) because (i)'s wording is built around a path, and there is none: on a
/// machine with no local data directory, telling the user to put a file "in " would be worse
/// than saying which variable decides where.
pub(crate) fn plugin_dir_undecidable_message(id: &str) -> String {
    format!(
        "[parsers].enabled contains {id:?}, whose grammar is a plugin, but groove cannot work \
         out where grammar plugins live on this machine. Set GROOVE_GRAMMAR_DIR to an absolute \
         path, or set grammar_dir in the config you pass with --config."
    )
}

/// Two parsers claiming the same extension. Refused rather than resolved by order.
///
/// First-wins would make which grammar parses a file depend on the order of
/// `[parsers].enabled`, and a plugin declares its own extension — so a file the user placed
/// could quietly take over `.rs` and the index would change with nothing to point at.
pub(crate) fn extension_conflict_message(extension: &str, first: &str, second: &str) -> String {
    format!(
        "[parsers].enabled has both {first:?} and {second:?} claiming the file extension \
         {extension:?}. groove indexes one file with one parser, so remove one of them."
    )
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
    anyhow::bail!(unresolved_id_message(
        "rs",
        KNOWN_IDS,
        &available_ids(),
        &super::code::plugin::plugin_ids()
    ))
}

/// Resolve one plugin-backed id into a parser, or say why it could not be.
///
/// `grammar_dir` is `None` when no directory could be worked out at all, which is a different
/// answer from "the directory is there and the file is not" — see
/// [`plugin_dir_undecidable_message`].
///
/// The directory is read only from here, and only for an id that needs it: a knowledge base of
/// Markdown never touches it, whatever command is running.
fn plugin_parser(
    id: &str,
    stem: &str,
    code: &CodeParsersConfig,
    grammar_dir: Option<&Path>,
) -> Result<Box<dyn Parser>> {
    let Some(dir) = grammar_dir else {
        anyhow::bail!(plugin_dir_undecidable_message(id));
    };
    let file_name = super::code::plugin::plugin_file_name(stem);
    let path = dir.join(&file_name);
    if !path.exists() {
        anyhow::bail!(plugin_missing_message(id, dir, &file_name));
    }
    let loaded = super::code::plugin::load(&path)
        .map_err(|r| anyhow::anyhow!(plugin_rejected_message(id, &path, &r.describe())))?;
    Ok(Box::new(super::CodeParser::new(
        loaded.grammar,
        loaded.extension,
        code.max_chunk_chars,
    )))
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
        Self::from_enabled_with_plugins(ids, code, None)
    }

    /// Same again, with the directory grammar plugins are loaded from.
    ///
    /// `grammar_dir` is `None` for "no directory could be worked out", which is the state
    /// [`plugin_dir_undecidable_message`] describes — not "there is no directory to look in".
    /// It is consulted lazily: an `enabled` list this build resolves on its own never reaches
    /// the filesystem, whatever command is running.
    ///
    /// A third constructor rather than a parameter on the other two, for the reason the second
    /// exists: the callers and tests that have no configuration to hand keep a constructor
    /// whose meaning has not changed.
    pub fn from_enabled_with_plugins(
        ids: &[String],
        code: &CodeParsersConfig,
        grammar_dir: Option<&Path>,
    ) -> Result<Self> {
        if ids.is_empty() {
            anyhow::bail!("[parsers].enabled must contain at least one id (got empty list)");
        }
        let mut parsers: Vec<Box<dyn Parser>> = Vec::with_capacity(ids.len());
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        // Which id claimed each extension. A plugin declares its own, so this is the only place
        // that can notice two parsers wanting the same file.
        let mut claimed: std::collections::HashMap<&'static str, String> =
            std::collections::HashMap::new();
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
                // (feature-56 PR-3a) A grammar the user placed. The table is consulted before
                // the id is called unknown, so a language groove knows how to load gets the
                // "put the file here" answer instead of a spelling suggestion.
                other => match super::code::plugin::plugin_stem(other) {
                    Some(stem) => {
                        code_max_chunk_chars = Some(code.max_chunk_chars);
                        plugin_parser(other, stem, code, grammar_dir)?
                    }
                    None => anyhow::bail!(unresolved_id_message(
                        other,
                        KNOWN_IDS,
                        &available_ids(),
                        &super::code::plugin::plugin_ids()
                    )),
                },
            };
            // Checked for every parser, not just the plugins: the rule is "one extension, one
            // parser", and a rule that only looks at the new arrivals is a rule with a hole.
            if let Some(first) = claimed.insert(parser.extension(), lower.clone()) {
                anyhow::bail!(extension_conflict_message(
                    parser.extension(),
                    &first,
                    &lower
                ));
            }
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

    // ---------------------------------------------------------------------
    // (feature-56 PR-3a) The four families of "this id did not resolve".
    //
    // Pinned as pure functions. (iv) only ever fires in a build with the
    // grammar feature off, and CI only *checks* such a build, so a test that
    // needed that binary would never run. The others are pinned here too, so
    // one place says what each family reads like.
    // ---------------------------------------------------------------------

    /// (iv) A real id this build cannot act on, told apart from a typo.
    #[test]
    fn a_recognised_id_with_no_grammar_compiled_in_is_not_called_a_typo() {
        let msg = unresolved_id_message("rs", &["md", "rs"], &["md"], &["py"]);
        assert!(
            msg.contains("recognises but was compiled"),
            "unexpected message: {msg}"
        );
        assert!(
            !msg.contains("unknown id"),
            "a known id must not be reported as unknown: {msg}"
        );
        assert!(msg.is_ascii(), "diagnostics stay ASCII: {msg}");
    }

    /// Everything after `supported in this build: ` must read as bare ids this
    /// build can act on — **including the last one**.
    ///
    /// This is the invariant, not the phrasing. Appending the plugin sentence
    /// after the list, which is what the spec asked for, passes a check that
    /// only asks "is `py` absent" and still leaves the final element reading
    /// `txt. These arrive as …`. Naming both ends here is what caught it.
    #[test]
    fn the_supported_list_stays_the_last_comma_separated_thing_in_the_message() {
        let available = ["md", "txt"];
        let msg = unresolved_id_message("rst", &["md"], &available, &["py", "go"]);
        let listed: Vec<&str> = msg
            .rsplit_once("supported in this build: ")
            .expect("message should list the supported ids")
            .1
            .split(',')
            .map(str::trim)
            .collect();
        assert_eq!(
            listed, available,
            "every element after the marker must be a bare supported id, first to last: {msg}"
        );
        assert!(
            msg.contains("grammar plugin you place yourself"),
            "the plugin ids should still be named: {msg}"
        );
        assert!(msg.is_ascii(), "diagnostics stay ASCII: {msg}");
    }

    /// The same invariant against the real id sets, so a future language added
    /// to either list cannot break the reading without this failing.
    #[test]
    fn the_real_message_lists_only_ids_this_build_can_act_on() {
        let msg = unresolved_id_message(
            "rst",
            KNOWN_IDS,
            &available_ids(),
            &super::super::code::plugin::plugin_ids(),
        );
        let listed: Vec<&str> = msg
            .rsplit_once("supported in this build: ")
            .expect("message should list the supported ids")
            .1
            .split(',')
            .map(str::trim)
            .collect();
        assert_eq!(
            listed,
            available_ids(),
            "the tail of the message is the available list and nothing else: {msg}"
        );
        for plugin in super::super::code::plugin::plugin_ids() {
            assert!(
                !listed.contains(&plugin),
                "{plugin:?} needs a file placed first, so it must not read as supported: \
                 {listed:?}"
            );
        }
    }

    /// With no plugin ids at all the message is exactly what it always was.
    #[test]
    fn the_unknown_id_message_gains_nothing_when_no_plugin_could_help() {
        let msg = unresolved_id_message("rst", &["md"], &["md"], &[]);
        assert!(msg.ends_with("supported in this build: md"), "{msg}");
    }

    /// (i) The directory is known and the file is not in it. No URL, so the
    /// wording survives the release page moving.
    #[test]
    fn a_missing_plugin_names_the_directory_the_file_belongs_in() {
        let dir = Path::new("/plugins");
        let msg = plugin_missing_message("py", dir, "libgroove_grammar_python.so");
        assert!(msg.contains("libgroove_grammar_python.so"), "{msg}");
        assert!(msg.contains("plugins"), "{msg}");
        assert!(msg.contains("groove-grammar-py-<target>"), "{msg}");
        assert!(
            !msg.contains("http://") && !msg.contains("https://"),
            "the address lives in docs/clients.md, not in a pinned string: {msg}"
        );
        assert!(msg.is_ascii(), "diagnostics stay ASCII: {msg}");
    }

    /// (ii) The file is there and was refused: the path, the reason, and the
    /// version whose release the replacement should come from.
    #[test]
    fn a_refused_plugin_names_the_path_the_reason_and_this_version() {
        let path = Path::new("/plugins/libgroove_grammar_python.so");
        let msg = plugin_rejected_message("py", path, "it does not export groove_grammar_name");
        assert!(msg.contains("libgroove_grammar_python.so"), "{msg}");
        assert!(msg.contains("does not export groove_grammar_name"), "{msg}");
        assert!(
            msg.contains(env!("CARGO_PKG_VERSION")),
            "a replacement has to match this build: {msg}"
        );
        assert!(msg.is_ascii(), "diagnostics stay ASCII: {msg}");
    }

    /// (iii) There is no directory to name, so (i)'s wording cannot be used.
    /// The variable that decides is named instead.
    #[test]
    fn an_undecidable_grammar_directory_names_the_variable_rather_than_a_path() {
        let msg = plugin_dir_undecidable_message("py");
        assert!(msg.contains("GROOVE_GRAMMAR_DIR"), "{msg}");
        assert!(msg.contains("grammar_dir"), "{msg}");
        assert!(
            !msg.contains("is not in "),
            "(i)'s wording needs a path, and there is none: {msg}"
        );
        assert!(msg.is_ascii(), "diagnostics stay ASCII: {msg}");
    }

    /// Two parsers for one extension is refused, and the message names both
    /// ids — the user has to know which two to choose between.
    #[test]
    fn an_extension_claimed_twice_names_both_claimants() {
        let msg = extension_conflict_message("rs", "rs", "py");
        assert!(msg.contains("\"rs\"") && msg.contains("\"py\""), "{msg}");
        assert!(msg.is_ascii(), "diagnostics stay ASCII: {msg}");
    }

    /// An id no plugin claims still fails the way it always did, and an id a
    /// plugin claims reaches the plugin path instead of the typo path.
    #[test]
    fn a_plugin_id_is_answered_by_the_grammar_directory_and_not_by_the_typo_message() {
        let code = CodeParsersConfig::default();

        // No directory could be worked out: (iii).
        let err = Registry::from_enabled_with_plugins(&["py".into()], &code, None)
            .expect_err("a plugin id with nowhere to look must fail");
        assert!(
            err.to_string().contains("GROOVE_GRAMMAR_DIR"),
            "unexpected message: {err}"
        );

        // A directory that exists but holds no plugin: (i).
        let dir = std::env::temp_dir().join("groove-registry-no-such-grammar-dir");
        let err = Registry::from_enabled_with_plugins(&["py".into()], &code, Some(&dir))
            .expect_err("a plugin id with no file must fail");
        let msg = err.to_string();
        assert!(msg.contains("is not in"), "unexpected message: {msg}");
        assert!(
            !msg.contains("unknown id"),
            "a language groove can load is not a typo: {msg}"
        );

        // An id nothing claims is still a typo, wherever the directory is.
        let err = Registry::from_enabled_with_plugins(&["rst".into()], &code, Some(&dir))
            .expect_err("an unknown id must still fail");
        assert!(err.to_string().contains("unknown id"), "{err}");
    }

    /// The directory is consulted only when an enabled id needs it.
    ///
    /// The observable half of "lazy": a Markdown-only list builds a registry
    /// with `None` for the directory, which is the value that would otherwise
    /// produce (iii).
    #[test]
    fn a_list_this_build_resolves_itself_never_needs_a_grammar_directory() {
        assert!(!crate::parser::needs_grammar_plugin(&[
            "md".into(),
            "txt".into(),
            "rs".into()
        ]));
        assert!(crate::parser::needs_grammar_plugin(&[
            "md".into(),
            "py".into()
        ]));
        // Case is normalised before the table is consulted, like every other id.
        assert!(crate::parser::needs_grammar_plugin(&["PY".into()]));

        Registry::from_enabled_with_plugins(
            &["md".into(), "txt".into()],
            &Default::default(),
            None,
        )
        .expect("no plugin id means the directory is never needed");
    }

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
