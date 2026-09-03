//! A valid plugin that calls its language something no `lang:` filter could match.
//!
//! Built by [`groove_grammar_abi::groove_grammar_plugin`] like `claims_rs.rs`: the macro
//! validates nothing at compile time, so a broken declaration is expressible without
//! hand-writing the exports. Everything else here is in order; only the name is wrong.
//!
//! **Declares `py`, the id it is loaded under.** The name is checked after the extension has
//! been matched against that id, so a fixture declaring anything else would be refused for the
//! mismatch and never reach the line this one is for.
//!
//! What it pins: the name is not decoration. It becomes the `lang:` tag on every chunk the
//! grammar produces, so a name with a space in it is a grammar that loads and then cannot be
//! searched by language -- a failure with no error message anywhere, discovered by a user whose
//! filter silently matches nothing.

#[cfg(feature = "grammar-rust")]
groove_grammar_abi::groove_grammar_plugin! {
    name = "Python 3",
    extension = "py",
    language = tree_sitter_rust::LANGUAGE,
    tags_query = tree_sitter_rust::TAGS_QUERY,
}
