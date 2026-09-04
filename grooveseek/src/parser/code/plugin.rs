//! (feature-56) Grammars that arrive as separate libraries the user places.
//!
//! Rust is compiled in; every other language is a `cdylib` exporting the six symbols in
//! [`groove_grammar_abi::symbols`]. That asymmetry is ADR-0013. This module is the half that
//! opens one, checks it, and hands back the same [`crate::parser::code::LoadedGrammar`] the
//! compiled-in path produces — so nothing downstream learns which way a grammar arrived.
//!
//! # Why the file is opened by name and the directory is never enumerated
//!
//! Opening a library runs its initialisers (`DllMain`, ELF constructors) **before** a single
//! symbol can be looked up, so "open everything in the directory and keep what looks like a
//! grammar" would execute arbitrary code from files the user never enabled. groove therefore
//! keeps a fixed id-to-file-name table, and opens only the file belonging to an id that
//! `[parsers].enabled` actually names. A file nobody enabled is never opened, and never warned
//! about either — it is not groove's business.
//!
//! The consequence is deliberate and user-visible: adding a language needs a groove release,
//! because the table lives here. An id outside the table stays an ordinary unknown-id typo.
//!
//! # Why a loaded library is never unloaded
//!
//! The parse table, the language name and the tags query are static data inside the library.
//! `tree_sitter::Language` holds a pointer straight into it, and chunks made from it carry
//! the name. Unloading would leave those dangling, and groove has no reason to: a registry
//! lives as long as the process. So a library that passes every check is deliberately leaked.
//! A library that fails one is dropped, which is safe precisely because nothing derived from
//! it has escaped yet.

use std::ffi::{CStr, c_char};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use groove_grammar_abi::{EXTENSION_SEPARATOR, symbols};
use tree_sitter::Language;
use tree_sitter_language::LanguageFn;

use super::LoadedGrammar;

/// Every `[parsers].enabled` id that a plugin can satisfy, and the library it lives in.
///
/// The stem only; the platform decorates it ([`plugin_file_name`]). Kept in one table because
/// the file has to be named before it is opened — see the module docs.
///
/// `py` is listed before its grammar is published, on purpose: the id has to be resolvable for
/// the "put the file here" diagnostic to be reachable at all, and a user who follows that
/// diagnostic to a release that has no such asset yet is told so by the release page rather
/// than by a typo message.
const PLUGIN_GRAMMARS: &[(&str, &str)] = &[
    ("py", "groove_grammar_python"),
    ("php", "groove_grammar_php"),
];

/// The most a plugin's tags query may declare itself to be.
///
/// The query is the grammar's own `tags.scm`, read from the library as a pointer and a length.
/// The ones groove ships are a few kilobytes; the largest in the Tree-sitter ecosystem are not
/// close to this. It is a refusal threshold, not a budget: see
/// [`Rejection::TagsQueryTooLarge`] for what a cap on a self-declared length can and cannot do.
const MAX_TAGS_QUERY_BYTES: usize = 1024 * 1024;

/// The table's own id and library stem for `id`, or `None` when no plugin claims it.
///
/// The id is handed back rather than reused from the caller so that the `'static` copy the
/// table owns is what downstream keys on — a parser's extension outlives the config string the
/// user typed.
pub(crate) fn plugin_entry(id: &str) -> Option<(&'static str, &'static str)> {
    PLUGIN_GRAMMARS
        .iter()
        .find(|(known, _)| *known == id)
        .map(|(known, stem)| (*known, *stem))
}

/// Every id a plugin could satisfy, in table order, for diagnostics.
pub(crate) fn plugin_ids() -> Vec<&'static str> {
    PLUGIN_GRAMMARS.iter().map(|(id, _)| *id).collect()
}

/// The file name a plugin has on this platform: `groove_grammar_python.dll`,
/// `libgroove_grammar_python.so`, `libgroove_grammar_python.dylib`.
///
/// Built from `std::env::consts` rather than a `cfg!` chain so it always agrees with what
/// Cargo names the `cdylib` it produced — the two would otherwise be free to drift on a
/// platform nobody tested.
pub(crate) fn plugin_file_name(stem: &str) -> String {
    format!(
        "{}{stem}{}",
        std::env::consts::DLL_PREFIX,
        std::env::consts::DLL_SUFFIX
    )
}

/// The release archive a plugin arrives in: `groove-grammar-python`, from the stem
/// `groove_grammar_python`.
///
/// Built from the stem and **not** from the enabled id, which is the same reason
/// [`plugin_file_name`] is: the archive is named after the plugin's crate, and cargo names a
/// crate's `cdylib` after that crate with `-` turned into `_`. So the file groove opens and the
/// archive a user downloads are one name in cargo's two spellings, and deriving one from the
/// other is what stops them drifting. The id cannot stand in — `py` is the id, `python` is the
/// language, and `groove-grammar-py` is a name no release publishes.
pub(crate) fn plugin_archive_name(stem: &str) -> String {
    stem.replace('_', "-")
}

/// Why a file that exists was not accepted as a grammar.
///
/// Separate from the message so the wording lives in one place ([`Self::describe`]) and the
/// tests that pin it do not have to reach through a loader that needs a real library.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Rejection {
    /// The library opened but a required export is absent.
    MissingSymbol(String),
    /// The library could not be opened at all (not a library, wrong architecture, a dependency
    /// of its own missing).
    NotLoadable(String),
    /// Built against a different version of groove's grammar contract.
    AbiVersion { found: u32, expected: u32 },
    /// The grammar's own tree-sitter ABI is outside what this runtime speaks.
    TreeSitterAbi {
        found: usize,
        min: usize,
        max: usize,
    },
    /// A string export was not valid UTF-8, or the name was not NUL-terminated data.
    NotUtf8(&'static str),
    /// An export that has to hand back a pointer handed back NULL.
    ///
    /// Its own variant rather than a shade of [`Self::NotUtf8`], which is what a NULL string
    /// used to be reported as: "its name is not valid UTF-8" sends the reader to look at the
    /// bytes of a name that was never there. And the grammar pointer had no reading at all —
    /// `tree_sitter`'s `abi_version` dereferences it without a check, so a plugin returning
    /// NULL there took the process down instead of being refused.
    NullExport(&'static str),
    /// The tags query declared a length this build will not read.
    ///
    /// A cap cannot make the read sound: a plugin that understates the length is
    /// indistinguishable from one telling the truth, and the slice is built from whatever it
    /// says either way. What it does is turn an absurd value into a refusal naming the
    /// library, instead of a read of gigabytes from wherever the pointer happens to land.
    TagsQueryTooLarge { len: usize, max: usize },
    /// The tags query does not compile against the grammar it came with.
    TagsQuery(String),
    /// The declared extension is not one groove will key a parser by.
    Extension(String),
    /// More than one extension declared. Reserved by the ABI, refused by this version.
    MultipleExtensions(String),
    /// The declared extension is valid, but is not the one this id stands for.
    ExtensionMismatch {
        declared: String,
        expected: &'static str,
    },
    /// The declared language name is not one a `lang:` filter could be written against.
    Name(String),
}

impl Rejection {
    /// The reason half of the (ii) diagnostic. ASCII, like every other diagnostic here.
    pub(crate) fn describe(&self) -> String {
        match self {
            Self::MissingSymbol(sym) => format!("it does not export {sym}"),
            Self::NotLoadable(err) => format!("it could not be loaded ({err})"),
            Self::AbiVersion { found, expected } => format!(
                "it declares grammar ABI version {found}, and this groove speaks version \
                 {expected}"
            ),
            Self::TreeSitterAbi { found, min, max } => format!(
                "its tree-sitter ABI version is {found}, outside the {min}..={max} this build \
                 supports"
            ),
            Self::NotUtf8(what) => format!("its {what} is not valid UTF-8"),
            Self::NullExport(what) => format!("its {what} export returned NULL"),
            Self::TagsQueryTooLarge { len, max } => format!(
                "it declares a tags query of {len} bytes, and this build reads at most {max}"
            ),
            Self::TagsQuery(err) => format!("its tags query does not compile ({err})"),
            Self::Extension(ext) => format!(
                "it claims the file extension {ext:?}, which is not lowercase ASCII \
                 alphanumerics without a dot"
            ),
            Self::MultipleExtensions(ext) => format!(
                "it claims more than one file extension ({ext:?}); this groove accepts exactly \
                 one"
            ),
            Self::ExtensionMismatch { declared, expected } => format!(
                "it claims the file extension {declared:?}, but the id it was loaded for stands \
                 for {expected:?}; a library under this name is expected to be the grammar for \
                 {expected:?}"
            ),
            Self::Name(name) => format!(
                "it calls its language {name:?}, which is not lowercase ASCII alphanumerics, \
                 '-' or '_'; the name becomes the lang: tag on every chunk"
            ),
        }
    }
}

/// A grammar that came from a plugin, with the extension it claimed.
pub(crate) struct LoadedPlugin {
    pub(crate) grammar: Arc<LoadedGrammar>,
    /// Leaked from the library's own string, so it outlives any borrow of it.
    pub(crate) extension: &'static str,
}

/// Held across parking a checked parse table and handing it to tree-sitter.
///
/// Two threads can build a parser registry at the same time -- an HTTP transport builds one per
/// request that needs it -- and [`CHECKED_TABLE`] is one slot. Without this, one load could
/// hand tree-sitter the other's table.
static HANDOVER: Mutex<()> = Mutex::new(());

/// The parse table [`load`] checked, waiting for the one call `Language::new` makes.
///
/// A `usize` rather than a pointer because a `static` has to be `Sync`, and the value only has
/// to survive from the store to the load a few lines later, both under [`HANDOVER`].
static CHECKED_TABLE: AtomicUsize = AtomicUsize::new(0);

/// Hands back the table [`load`] parked, so the pointer tree-sitter consumes is the checked one.
///
/// This exists because `tree_sitter::Language` offers no way in: it can be built from a
/// `LanguageFn` and nothing else, and building one *calls* the function. Passing the plugin's
/// own export would therefore check one answer and use another.
///
/// # Safety
///
/// Reached only from `Language::new`, while [`load`] holds [`HANDOVER`] and has stored a
/// non-NULL table that a plugin's own language export returned.
unsafe extern "C" fn checked_language() -> *const () {
    CHECKED_TABLE.load(Ordering::Acquire) as *const ()
}

/// Open one plugin and check it.
///
/// `expected_extension` is the id this library was looked up for. The caller has already
/// decided this file is the one for that id; the path is not searched for or guessed at here.
pub(crate) fn load(
    path: &Path,
    expected_extension: &'static str,
) -> std::result::Result<LoadedPlugin, Rejection> {
    // Absolute, because on Windows a relative path sends the loader through the current
    // directory search order, and the whole point of the flags below is to not do that.
    let absolute = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());

    let lib = open_library(&absolute)?;

    // **The contract version is read on its own, before anything else is even looked up.**
    //
    // Every other export is read through the signature *this* version of the ABI defines. A
    // library built against a different one may have dropped a symbol — which would be
    // reported as a missing export, sending the user to look for a corrupt file rather than a
    // mismatched version — or, worse, kept the name and changed the signature, in which case
    // calling it through the signature here is undefined behaviour. Neither is a diagnostic,
    // so the version question is settled while nothing else has been touched.
    // SAFETY: `groove_grammar_abi_version` takes nothing and returns a `u32`. It is the one
    // export whose signature is fixed for all time, which is what makes this order possible.
    let abi_version = unsafe {
        let f: libloading::Symbol<unsafe extern "C" fn() -> u32> = get(&lib, symbols::ABI_VERSION)?;
        f()
    };
    if abi_version != groove_grammar_abi::ABI_VERSION {
        return Err(Rejection::AbiVersion {
            found: abi_version,
            expected: groove_grammar_abi::ABI_VERSION,
        });
    }

    // Everything else the library says is copied out before anything is built from it, so a
    // rejection below can drop the library without leaving a borrow behind.
    let raw = read_exports(&lib)?;

    // **The parse table is asked for once, and the pointer tree-sitter consumes is the one that
    // was checked.** Everything below reads through it, starting with `abi_version`, and
    // `ts_language_abi_version` dereferences what it is handed with no check of its own -- so a
    // plugin returning NULL here used to end the process rather than be refused, the one
    // malformed shape this module could not report.
    //
    // Handing the plugin's own export to `LanguageFn` would not be enough, because `Language`
    // can only be built from one by *calling* it: the check and the use would be two calls
    // apart, and nothing in the ABI says an export has to answer the same way twice. The macro
    // writes one that does; a hand-written plugin need not, and the fixtures beside this module
    // are proof that hand-written plugins exist. So the value is parked and handed over once
    // (see [`checked_language`]).
    //
    // SAFETY: `groove_grammar_language` takes nothing and returns a pointer, a signature fixed
    // by the ABI version already confirmed above.
    let table = unsafe { (raw.language)() };
    if table.is_null() {
        return Err(Rejection::NullExport("grammar"));
    }
    let language = {
        let _handover = crate::poison::recover(HANDOVER.lock(), "grammar plugin handover");
        CHECKED_TABLE.store(table as usize, Ordering::Release);
        // SAFETY: `checked_language` hands back exactly the pointer the plugin's own export
        // returned a moment ago, checked non-NULL. `from_raw` asks for a table a Tree-sitter
        // CLI grammar produced, and this is that table -- handed over rather than asked for a
        // second time. The lock is held across the store and the call, so a concurrent load
        // cannot substitute its own.
        Language::from(unsafe { LanguageFn::from_raw(checked_language) })
    };
    // **The grammar's own Tree-sitter ABI is settled before anything is built from the table.**
    // This is the only check that reads the table without using it, and the last thing standing
    // between a grammar from a CLI this runtime cannot speak and `TagsConfiguration`, which
    // reads the parse table proper.
    //
    // Both ends are reported because a plugin arrives on its own: nothing makes the CLI that
    // generated it older than the runtime groove links, and "yours is too old" and "yours is
    // too new" send the reader to different downloads. `stale_tree_sitter.rs` and
    // `future_tree_sitter.rs` in `tests/fixtures/grammar_plugins/` hold one side each, so
    // neither half of the comparison can be dropped without a test going red.
    let found = language.abi_version();
    let min = tree_sitter::MIN_COMPATIBLE_LANGUAGE_VERSION;
    let max = tree_sitter::LANGUAGE_VERSION;
    if found < min || found > max {
        return Err(Rejection::TreeSitterAbi { found, min, max });
    }

    // The strings are checked after the grammar is known to be usable, so that a plugin with
    // two things wrong reports the one the user can do least about first. The separator is
    // tested before validity because "you declared two" and "that is not an extension" send
    // the reader to different places, and the validity rule refuses both.
    if raw.extension.contains(EXTENSION_SEPARATOR) {
        return Err(Rejection::MultipleExtensions(raw.extension));
    }
    if !groove_grammar_abi::extension_is_valid(&raw.extension) {
        return Err(Rejection::Extension(raw.extension));
    }
    // **The declared extension has to be the one the id stands for.** groove found this file by
    // building its name from the enabled id, so the two are already claimed to be the same
    // thing. Registering whatever the library says instead would let a mispackaged plugin
    // silently move the whole language: `py` enabled, a library declaring `go`, and `.py` files
    // stop being indexed while `.go` files start — with nothing refused and nothing logged.
    if raw.extension != expected_extension {
        return Err(Rejection::ExtensionMismatch {
            declared: raw.extension,
            expected: expected_extension,
        });
    }
    // The name is not decoration either: it becomes `lang:<name>` on every chunk, so a name no
    // filter could be written against is a grammar that loads and then cannot be searched by
    // language.
    if !groove_grammar_abi::name_is_valid(&raw.name) {
        return Err(Rejection::Name(raw.name));
    }

    // Leaked rather than borrowed: `LoadedGrammar` and `CodeParser` both key on `&'static str`,
    // and a copy of our own is one less thing tied to the library staying mapped.
    let name: &'static str = String::leak(raw.name);
    let grammar = LoadedGrammar::new(name, language, &raw.tags_query)
        .map_err(|e| Rejection::TagsQuery(format!("{e}")))?;
    let extension: &'static str = String::leak(raw.extension);

    tracing::info!(
        plugin = %absolute.display(),
        grammar = name,
        extension,
        build_info = %raw.build_info,
        "loaded a grammar plugin"
    );

    // Every check passed, so the library must stay mapped for the rest of the process: the
    // parse table and the query the grammar was built from live inside it.
    std::mem::forget(lib);

    Ok(LoadedPlugin {
        grammar: Arc::new(grammar),
        extension,
    })
}

/// What the exports other than the version said, copied into owned data.
///
/// The version is not here: it is read on its own beforehand, because reading these at all is
/// only meaningful once it is known to match. See [`load`].
struct RawExports {
    language: unsafe extern "C" fn() -> *const (),
    name: String,
    extension: String,
    tags_query: String,
    build_info: String,
}

fn open_library(absolute: &Path) -> std::result::Result<libloading::Library, Rejection> {
    #[cfg(windows)]
    {
        use libloading::os::windows::{
            LOAD_LIBRARY_SEARCH_DEFAULT_DIRS, LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR, Library,
        };
        // The flags replace the default search order, which starts at the directory of the
        // running process and then the current directory. `DLL_LOAD_DIR` looks beside the
        // plugin, `DEFAULT_DIRS` covers the system directories, and neither is the cwd of
        // whichever project an MCP client happened to launch groove from.
        //
        // SAFETY: opening a library runs its initialisers. That is inherent to loading a
        // grammar the user placed, and is the reason only enabled ids are ever opened.
        let os = unsafe {
            Library::load_with_flags(
                absolute,
                LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR | LOAD_LIBRARY_SEARCH_DEFAULT_DIRS,
            )
        }
        .map_err(|e| Rejection::NotLoadable(format!("{e}")))?;
        Ok(libloading::Library::from(os))
    }
    #[cfg(not(windows))]
    {
        use libloading::os::unix::{Library, RTLD_LOCAL, RTLD_NOW};
        // `RTLD_NOW` so an unresolved symbol is an error here rather than a crash the first
        // time a parse reaches it; `RTLD_LOCAL` so one grammar cannot satisfy another's
        // symbols and turn a missing dependency into a silent mismatch.
        //
        // SAFETY: as above.
        let os = unsafe { Library::open(Some(absolute), RTLD_NOW | RTLD_LOCAL) }
            .map_err(|e| Rejection::NotLoadable(format!("{e}")))?;
        Ok(libloading::Library::from(os))
    }
}

/// Read every export except the version, whose match [`load`] has already established.
///
/// That order is the reason these signatures can be trusted at all: they are the ones
/// [`groove_grammar_abi::groove_grammar_plugin`] generates for **this** ABI version, and a
/// library declaring
/// another one never reaches here.
fn read_exports(lib: &libloading::Library) -> std::result::Result<RawExports, Rejection> {
    // SAFETY: each signature is the one `groove_grammar_plugin!` generates, which is the
    // contract `symbols` names, for the version already confirmed by the caller. A library
    // exporting the name with a different signature *at the same declared version* is native
    // code the user placed, and no check short of running it can tell.
    unsafe {
        let language: libloading::Symbol<unsafe extern "C" fn() -> *const ()> =
            get(lib, symbols::LANGUAGE)?;
        let name: libloading::Symbol<unsafe extern "C" fn() -> *const c_char> =
            get(lib, symbols::NAME)?;
        let extensions: libloading::Symbol<unsafe extern "C" fn() -> *const c_char> =
            get(lib, symbols::EXTENSIONS)?;
        let tags: libloading::Symbol<unsafe extern "C" fn(*mut usize) -> *const u8> =
            get(lib, symbols::TAGS_QUERY)?;
        let build: libloading::Symbol<unsafe extern "C" fn() -> *const c_char> =
            get(lib, symbols::BUILD_INFO)?;

        let mut len = 0usize;
        let query_ptr = tags(&mut len);
        // `slice::from_raw_parts` requires a non-NULL pointer **even for a length of zero**,
        // so a plugin handing back a NULL query with `len = 0` -- the obvious way to say "no
        // tags query" -- would be undefined behaviour rather than a refusal.
        if query_ptr.is_null() {
            return Err(Rejection::NullExport("tags query"));
        }
        if len > MAX_TAGS_QUERY_BYTES {
            return Err(Rejection::TagsQueryTooLarge {
                len,
                max: MAX_TAGS_QUERY_BYTES,
            });
        }
        let tags_query = std::str::from_utf8(std::slice::from_raw_parts(query_ptr, len))
            .map_err(|_| Rejection::NotUtf8("tags query"))?
            .to_owned();

        Ok(RawExports {
            language: *language,
            name: owned_cstr(name(), "name")?,
            extension: owned_cstr(extensions(), "extension")?,
            tags_query,
            build_info: owned_cstr(build(), "build info")?,
        })
    }
}

/// Look one symbol up, naming it if it is absent.
///
/// # Safety
///
/// The caller states the signature; see [`read_exports`].
unsafe fn get<'lib, T>(
    lib: &'lib libloading::Library,
    symbol: &[u8],
) -> std::result::Result<libloading::Symbol<'lib, T>, Rejection> {
    // SAFETY: forwarded from the caller.
    unsafe { lib.get(symbol) }.map_err(|_| {
        // The table holds the trailing NUL that `dlsym` needs; the message does not want it.
        let name = String::from_utf8_lossy(&symbol[..symbol.len().saturating_sub(1)]);
        Rejection::MissingSymbol(name.into_owned())
    })
}

/// # Safety
///
/// `ptr` must be a NUL-terminated string that stays valid for the duration of this call.
unsafe fn owned_cstr(
    ptr: *const c_char,
    what: &'static str,
) -> std::result::Result<String, Rejection> {
    if ptr.is_null() {
        return Err(Rejection::NullExport(what));
    }
    // SAFETY: forwarded from the caller.
    unsafe { CStr::from_ptr(ptr) }
        .to_str()
        .map(str::to_owned)
        .map_err(|_| Rejection::NotUtf8(what))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_file_name_matches_what_cargo_names_a_cdylib_here() {
        let name = plugin_file_name("groove_grammar_python");
        assert!(
            name.starts_with(std::env::consts::DLL_PREFIX)
                && name.ends_with(std::env::consts::DLL_SUFFIX),
            "unexpected plugin file name: {name}"
        );
        assert!(name.contains("groove_grammar_python"), "{name}");
    }

    #[test]
    fn only_ids_in_the_table_resolve_to_a_plugin() {
        assert_eq!(plugin_entry("py"), Some(("py", "groove_grammar_python")));
        // Spelled out rather than left to the two tests below that iterate the table: those
        // check that each entry is *shaped* right, which a plausible-looking typo like
        // `groove_grammar_pyth` would also satisfy. Only the pair written here catches it.
        assert_eq!(plugin_entry("php"), Some(("php", "groove_grammar_php")));
        assert_eq!(plugin_entry("rs"), None, "rs is compiled in, not a plugin");
        assert_eq!(plugin_entry("rst"), None);
    }

    /// Every id in the table is one groove could key a parser by, and every stem is one that
    /// survives being turned into a file name.
    ///
    /// The id is handed to the loader as the extension a plugin must declare, so an entry
    /// whose id is not a valid extension would make that language unloadable no matter what
    /// the plugin does — a state nothing else would report.
    #[test]
    fn every_entry_in_the_table_is_usable_as_both_an_id_and_a_file_name() {
        for (id, stem) in PLUGIN_GRAMMARS {
            assert!(
                groove_grammar_abi::extension_is_valid(id),
                "{id:?} is in the table but is not an extension groove can key a parser by"
            );
            let file = plugin_file_name(stem);
            assert!(file.contains(stem), "{file} should contain {stem}");
            assert!(
                !stem.contains(['/', '\\']),
                "{stem:?} would escape the grammar directory"
            );
        }
    }

    /// The archive a diagnostic tells the user to download is the plugin's **crate**, which is
    /// its library stem in cargo's other spelling — never the enabled id.
    ///
    /// `py` and `python` are different words, so a message built from the id would name
    /// `groove-grammar-py`, which no release publishes. Every future id whose language is
    /// spelled differently (`ts`, `kt`) has the same shape.
    #[test]
    fn the_archive_a_diagnostic_names_is_the_crate_the_library_came_from() {
        assert_eq!(
            plugin_archive_name("groove_grammar_python"),
            "groove-grammar-python"
        );
        for (_, stem) in PLUGIN_GRAMMARS {
            assert_eq!(
                &plugin_archive_name(stem).replace('-', "_"),
                stem,
                "the archive and the library it holds must be one name in two spellings"
            );
        }
    }

    /// Each reason reads as one clause after "…was refused because", and stays ASCII.
    #[test]
    fn every_rejection_reason_is_ascii_and_names_what_was_wrong() {
        let cases = [
            Rejection::MissingSymbol("groove_grammar_name".into()),
            Rejection::NotLoadable("bad image".into()),
            Rejection::AbiVersion {
                found: 2,
                expected: 1,
            },
            Rejection::TreeSitterAbi {
                found: 11,
                min: 13,
                max: 15,
            },
            Rejection::NotUtf8("name"),
            Rejection::NullExport("grammar"),
            Rejection::NullExport("tags query"),
            Rejection::TagsQueryTooLarge {
                len: 1 << 30,
                max: MAX_TAGS_QUERY_BYTES,
            },
            Rejection::TagsQuery("query error".into()),
            Rejection::Extension("R S".into()),
            Rejection::MultipleExtensions("py;pyi".into()),
            Rejection::ExtensionMismatch {
                declared: "go".into(),
                expected: "py",
            },
            Rejection::Name("Python 3".into()),
        ];
        for case in cases {
            let text = case.describe();
            assert!(text.is_ascii(), "not ASCII: {text}");
            assert!(!text.is_empty());
            assert!(
                !text.ends_with('.'),
                "reasons are clauses, not sentences: {text}"
            );
        }
    }

    #[test]
    fn a_missing_symbol_is_named_without_its_nul_terminator() {
        let r = super::Rejection::MissingSymbol(
            String::from_utf8_lossy(&symbols::NAME[..symbols::NAME.len() - 1]).into_owned(),
        );
        assert_eq!(r.describe(), "it does not export groove_grammar_name");
    }
}
