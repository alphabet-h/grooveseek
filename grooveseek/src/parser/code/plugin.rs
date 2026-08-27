//! (feature-56) Grammars that arrive as separate libraries the user places.
//!
//! Rust is compiled in; every other language is a `cdylib` exporting the six symbols in
//! [`groove_grammar_abi::symbols`]. That asymmetry is ADR-0013. This module is the half that
//! opens one, checks it, and hands back the same [`LoadedGrammar`] the compiled-in path
//! produces — so nothing downstream learns which way a grammar arrived.
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
use std::sync::Arc;

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
const PLUGIN_GRAMMARS: &[(&str, &str)] = &[("py", "groove_grammar_python")];

/// The library stem for an id, or `None` when no plugin claims it.
pub(crate) fn plugin_stem(id: &str) -> Option<&'static str> {
    PLUGIN_GRAMMARS
        .iter()
        .find(|(known, _)| *known == id)
        .map(|(_, stem)| *stem)
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
    /// The tags query does not compile against the grammar it came with.
    TagsQuery(String),
    /// The declared extension is not one groove will key a parser by.
    Extension(String),
    /// More than one extension declared. Reserved by the ABI, refused by this version.
    MultipleExtensions(String),
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
            Self::TagsQuery(err) => format!("its tags query does not compile ({err})"),
            Self::Extension(ext) => format!(
                "it claims the file extension {ext:?}, which is not lowercase ASCII \
                 alphanumerics without a dot"
            ),
            Self::MultipleExtensions(ext) => format!(
                "it claims more than one file extension ({ext:?}); this groove accepts exactly \
                 one"
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

/// Open one plugin and check it, in the order the checks are cheapest to fail.
///
/// The caller has already decided this file is the one for an enabled id; the path is not
/// searched for or guessed at here.
pub(crate) fn load(path: &Path) -> std::result::Result<LoadedPlugin, Rejection> {
    // Absolute, because on Windows a relative path sends the loader through the current
    // directory search order, and the whole point of the flags below is to not do that.
    let absolute = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    let lib = open_library(&absolute)?;

    // Everything the library says is copied out before anything is built from it, so a
    // rejection below can drop the library without leaving a borrow behind.
    let raw = read_exports(&lib)?;

    if raw.abi_version != groove_grammar_abi::ABI_VERSION {
        return Err(Rejection::AbiVersion {
            found: raw.abi_version,
            expected: groove_grammar_abi::ABI_VERSION,
        });
    }

    // SAFETY: the pointer came from a grammar generated by the Tree-sitter CLI, which is what
    // `groove_grammar_language` is contracted to return. A library that lies here is native
    // code the user chose to place, and is trusted to the same degree the binary itself is.
    let language = Language::from(unsafe { LanguageFn::from_raw(raw.language) });
    let found = language.abi_version();
    let min = tree_sitter::MIN_COMPATIBLE_LANGUAGE_VERSION;
    let max = tree_sitter::LANGUAGE_VERSION;
    if found < min || found > max {
        return Err(Rejection::TreeSitterAbi { found, min, max });
    }

    // The extension is checked after the grammar is known to be usable, so that a plugin with
    // two things wrong reports the one the user can do least about first. The separator is
    // tested before validity because "you declared two" and "that is not an extension" send
    // the reader to different places, and the validity rule refuses both.
    if raw.extension.contains(EXTENSION_SEPARATOR) {
        return Err(Rejection::MultipleExtensions(raw.extension));
    }
    if !groove_grammar_abi::extension_is_valid(&raw.extension) {
        return Err(Rejection::Extension(raw.extension));
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

/// What the six exports said, copied into owned data.
struct RawExports {
    abi_version: u32,
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

fn read_exports(lib: &libloading::Library) -> std::result::Result<RawExports, Rejection> {
    // SAFETY: each signature is the one `groove_grammar_plugin!` generates, which is the
    // contract `symbols` names. A library exporting the name with a different signature is
    // native code the user placed, and no check short of running it can tell.
    unsafe {
        let abi: libloading::Symbol<unsafe extern "C" fn() -> u32> =
            get(lib, symbols::ABI_VERSION)?;
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
        let tags_query = std::str::from_utf8(std::slice::from_raw_parts(query_ptr, len))
            .map_err(|_| Rejection::NotUtf8("tags query"))?
            .to_owned();

        Ok(RawExports {
            abi_version: abi(),
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
        return Err(Rejection::NotUtf8(what));
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
        assert_eq!(plugin_stem("py"), Some("groove_grammar_python"));
        assert_eq!(plugin_stem("rs"), None, "rs is compiled in, not a plugin");
        assert_eq!(plugin_stem("rst"), None);
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
            Rejection::TagsQuery("query error".into()),
            Rejection::Extension("R S".into()),
            Rejection::MultipleExtensions("py;pyi".into()),
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
