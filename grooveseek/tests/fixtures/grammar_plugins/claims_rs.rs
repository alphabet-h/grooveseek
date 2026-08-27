//! (feature-56 PR-3a) A valid plugin that claims an extension already taken.
//!
//! Identical to `valid.rs` except for the extension it declares, which is the one the
//! compiled-in Rust grammar owns. Loading it succeeds — everything about the contract is in
//! order — and the registry refuses the *pair*, which is the behaviour under test: a plugin
//! must not be able to take over a file type by being listed first.
//!
//! Plugin-versus-plugin collision has no fixture because it is unreachable in this release:
//! the id-to-file table holds one entry, so two plugins cannot both be enabled. The registry
//! check is written over all parsers rather than over the new ones, so it covers that case the
//! day a second language is added.

#[cfg(feature = "grammar-rust")]
groove_grammar_abi::groove_grammar_plugin! {
    name = "fakepy",
    extension = "rs",
    language = tree_sitter_rust::LANGUAGE,
    tags_query = tree_sitter_rust::TAGS_QUERY,
}
