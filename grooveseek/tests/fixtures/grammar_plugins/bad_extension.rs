//! A valid plugin whose declared extension is written with the dot.
//!
//! Built by [`groove_grammar_abi::groove_grammar_plugin`] like `claims_rs.rs`: the macro
//! validates nothing at compile time, so a broken declaration is expressible without
//! hand-writing the exports. Everything else here is in order; only the string is wrong.
//!
//! Needs `grammar-rust`: the extension is checked after the parse table has been accepted, so
//! this fixture has to carry a real grammar to reach the line it is about.
//!
//! What it pins: [`groove_grammar_abi::extension_is_valid`] is applied to what the library says,
//! not just to what a config says. groove keys parsers by a bare lowercase extension, so a
//! leading dot is the mistake an author makes on the first try -- and a plugin is arbitrary
//! native code, so the strings it hands back are checked like any other untrusted input before
//! they reach a filesystem walk.

#[cfg(feature = "grammar-rust")]
groove_grammar_abi::groove_grammar_plugin! {
    name = "dottedext",
    extension = ".py",
    language = tree_sitter_rust::LANGUAGE,
    tags_query = tree_sitter_rust::TAGS_QUERY,
}
