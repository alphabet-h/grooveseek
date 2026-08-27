//! (feature-56 PR-3a) A valid plugin that declares the wrong language's extension.
//!
//! Identical to `valid.rs` except for the extension it declares. Every part of the contract is
//! in order — the exports, the ABI version, the grammar, the tags query — so the only thing
//! wrong is that a library found under the `py` id says it is for `rs`. That is what a
//! mispackaged download looks like, and refusing it is what stops one from silently taking
//! `.py` out of the index and putting `.rs` in.
//!
//! It is loaded under the `py` id, never under `rs`, so it also stands in for the collision
//! case the registry no longer needs a check for: with the declared extension pinned to the
//! id, two parsers claiming one extension is not a state the registry can reach.

#[cfg(feature = "grammar-rust")]
groove_grammar_abi::groove_grammar_plugin! {
    name = "fakepy",
    extension = "rs",
    language = tree_sitter_rust::LANGUAGE,
    tags_query = tree_sitter_rust::TAGS_QUERY,
}
