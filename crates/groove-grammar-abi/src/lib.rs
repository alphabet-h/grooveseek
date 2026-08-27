//! (feature-56) The shape a tree-sitter grammar takes when groove loads it.
//!
//! Two paths reach this crate. A grammar compiled into `groove` itself builds a
//! [`GrammarDescriptor`] directly. A grammar shipped as a separate dynamic library builds the
//! same descriptor and exports it across a C ABI, which the loader reads back (arriving with
//! the loader itself). Both end up as the same descriptor, so the chunker never learns which
//! way a grammar got there.
//!
//! The descriptor deliberately carries no code of its own: a grammar contributes a parse
//! table and a tags query, and everything groove does with them — walking scopes, collecting
//! doc comments, deciding chunk boundaries — is written once, in a language-agnostic way, on
//! groove's side. Adding a language is supplying data.
//!
//! # ABI stability
//!
//! [`ABI_VERSION`] is groove's own contract number, not tree-sitter's. It changes when the set
//! of exported symbols or their signatures change, which is why a plugin states it and the
//! loader refuses anything else. The grammar's own tree-sitter ABI is a separate check.

#![forbid(unsafe_code)]

/// Version of groove's grammar contract. A plugin declaring anything else is refused.
pub const ABI_VERSION: u32 = 1;

/// Everything groove needs to parse one language.
///
/// `extension` is the single file extension this grammar claims, without a leading dot. One
/// grammar claims one extension: the registry keys parsers by extension and treats the
/// enabled id as that key, so a second extension would need a second identity to be enabled
/// or disabled by. The ABI reserves a separator for a future multi-extension form, but this
/// version refuses a declaration that uses it.
pub struct GrammarDescriptor {
    /// Lowercase language name, as it appears in the `lang:` tag on every chunk (`"rust"`).
    pub name: &'static str,
    /// Lowercase file extension without a dot (`"rs"`).
    pub extension: &'static str,
    /// The generated parse table.
    pub language: tree_sitter_language::LanguageFn,
    /// The grammar's `tags.scm`, which is what turns a parse tree into definitions.
    pub tags_query: &'static str,
}

/// Separator reserved for a future grammar that claims more than one extension.
///
/// Declared here rather than in the loader so that both sides of the ABI agree on it before
/// anything is built against it.
pub const EXTENSION_SEPARATOR: char = ';';

impl GrammarDescriptor {
    /// Whether the declared extension is one groove will accept.
    ///
    /// Rejects the empty string, a leading dot, anything outside lowercase ASCII alphanumerics,
    /// anything unreasonably long, and — for this ABI version — a multi-extension declaration.
    /// A plugin is arbitrary native code, so the strings it hands back are checked like any
    /// other untrusted input before they reach a filesystem walk.
    pub fn extension_is_valid(&self) -> bool {
        let ext = self.extension;
        !ext.is_empty()
            && ext.len() <= 64
            && !ext.contains(EXTENSION_SEPARATOR)
            && ext
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
    }
}
