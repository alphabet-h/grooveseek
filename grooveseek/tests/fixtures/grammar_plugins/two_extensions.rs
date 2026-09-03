//! A valid plugin that claims two file extensions at once.
//!
//! Built by [`groove_grammar_abi::groove_grammar_plugin`] like `claims_rs.rs`: the macro
//! `concat!`s whatever literal it is handed and validates nothing at compile time, so a broken
//! declaration is expressible without hand-writing the exports. Everything else here is in
//! order -- a real parse table, a real tags query -- so the only thing wrong is the string.
//!
//! Needs `grammar-rust`, unlike the fixtures refused inside `read_exports`: the extension is
//! checked *after* the parse table has been accepted, so this one has to carry a real grammar
//! to reach the line it is about.
//!
//! What it pins: [`groove_grammar_abi::EXTENSION_SEPARATOR`] is reserved for a future grammar
//! that claims more than one extension, and this build does not speak it yet. "You declared
//! two" and "that is not an extension" send the reader to different places, so the separator is
//! tested before validity even though the validity rule refuses both.

#[cfg(feature = "grammar-rust")]
groove_grammar_abi::groove_grammar_plugin! {
    name = "twoext",
    extension = "py;pyi",
    language = tree_sitter_rust::LANGUAGE,
    tags_query = tree_sitter_rust::TAGS_QUERY,
}
