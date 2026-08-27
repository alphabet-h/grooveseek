//! The Python grammar, exported across the C ABI groove loads grammars through.
//!
//! There is no logic here on purpose. What a plugin has to get right — the
//! symbol names, their signatures, the NUL terminators, the length
//! out-parameter for the tags query — lives in the macro
//! [`groove_grammar_abi::groove_grammar_plugin`], so that a second grammar is a
//! manifest and one macro call rather than an FFI surface to review again.
//!
//! What this crate chooses is the four values:
//!
//! - `name` becomes `lang:python` on every chunk, so it is the word a filter is
//!   written against. It is the language, not the id: `py` is the id.
//! - `extension` must be the one the enabled id stands for. groove finds this
//!   file by building its name from the id in `[parsers].enabled`, so the two
//!   already claim to be the same thing, and the loader refuses a library that
//!   disagrees rather than letting a mispackaged plugin move a whole language.
//! - `language` and `tags_query` come from the same crate version, which is the
//!   reason a grammar and its `tags.scm` travel together: a query compiled
//!   against a different parse table fails at load, and the loader reports that
//!   as a refused file rather than parsing Python with the wrong table.

groove_grammar_abi::groove_grammar_plugin! {
    name = "python",
    extension = "py",
    language = tree_sitter_python::LANGUAGE,
    tags_query = tree_sitter_python::TAGS_QUERY,
}
