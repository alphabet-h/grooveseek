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
    /// What the plugin says built it, verbatim.
    ///
    /// Diagnostic only, by the ABI's own definition: it is what the plugin *says* built it.
    /// Logged, and put in front of the hash below so that a message about a change names
    /// something a reader recognises.
    pub(crate) build_info: String,
    /// What the library actually is: the first half of a SHA-256 over its bytes.
    ///
    /// The identity the index records, because [`Self::build_info`] cannot serve as one.
    /// The macro builds that string from `CARGO_PKG_NAME`, `CARGO_PKG_VERSION` and the
    /// language name, so a plugin rebuilt against a newer grammar or a fixed tags query —
    /// without its own version moving — hands back a string identical to the previous build's.
    /// A grammar that changed would then look unchanged, and chunks from two builds would mix
    /// with nothing said, which is the outcome recording a generation exists to prevent.
    ///
    /// Hashing the file answers the question directly: the thing that cut the chunks is this
    /// library, and this is what it was. It does not depend on the plugin's author being
    /// careful with a version number, and an identical rebuild hashing the same is correct
    /// rather than a miss.
    pub(crate) content_hash: String,
}

/// Bytes read from a plugin before it is opened, for [`LoadedPlugin::content_hash`].
///
/// Half of a SHA-256, hex. This is a change detector, not a security boundary — what stands
/// between the user and a substituted library is verifying the published checksum before the
/// file is ever placed, which the setup instructions cover and no check here can replace.
fn content_hash(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(bytes);
    digest.iter().take(16).map(|b| format!("{b:02x}")).collect()
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

    // Read before opening. A file groove cannot read is a file it cannot load either, so the
    // failure belongs to the same case; and doing it first means the hash describes the bytes
    // as they were before anything in them ran.
    let hash = std::fs::read(&absolute)
        .map(|bytes| content_hash(&bytes))
        .map_err(|e| Rejection::NotLoadable(format!("{e}")))?;

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
        content_hash = %hash,
        "loaded a grammar plugin"
    );

    // Every check passed, so the library must stay mapped for the rest of the process: the
    // parse table and the query the grammar was built from live inside it.
    std::mem::forget(lib);

    Ok(LoadedPlugin {
        grammar: Arc::new(grammar),
        extension,
        build_info: raw.build_info,
        content_hash: hash,
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
        assert_eq!(plugin_entry("py"), Some(("py", "groove_grammar_python")));
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

    /// The hash has to change when the bytes do, and not when they do not.
    ///
    /// That is the whole contract: it stands in for "is this the same library", which the
    /// plugin's own [`LoadedPlugin::build_info`] cannot answer: that is built from a version
    /// number the author may not have moved.
    #[test]
    fn the_content_hash_follows_the_bytes_and_nothing_else() {
        let a = content_hash(b"one grammar");
        let b = content_hash(b"another grammar");
        assert_ne!(a, b, "different bytes must not hash alike");
        assert_eq!(
            a,
            content_hash(b"one grammar"),
            "the same bytes must hash the same, so an identical rebuild is not a change"
        );
        // A single flipped bit is a different grammar as far as this is concerned.
        assert_ne!(a, content_hash(b"one grammat"));
        assert_eq!(a.len(), 32, "half of a SHA-256, hex: {a}");
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()), "{a}");
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
