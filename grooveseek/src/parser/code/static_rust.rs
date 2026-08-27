//! (feature-56) The Rust grammar, compiled into `groove` itself.
//!
//! Rust is the one language shipped this way. It is the language groove is written in, so a
//! user pointing groove at this repository gets code search without placing a single extra
//! file — and it is the language whose grammar was measured, at just over a megabyte of
//! binary, small enough that everyone can carry it whether they index code or not.
//!
//! Every other language arrives as a separate library the user chooses to put in place. That
//! asymmetry is the decision recorded in ADR-0013, not an accident of what was implemented
//! first.

use std::sync::Arc;

use anyhow::Result;
use groove_grammar_abi::GrammarDescriptor;
use tree_sitter::Language;

use super::LoadedGrammar;

/// The compiled-in Rust grammar, in the same shape a plugin would hand over.
pub(crate) const DESCRIPTOR: GrammarDescriptor = GrammarDescriptor {
    name: "rust",
    extension: "rs",
    language: tree_sitter_rust::LANGUAGE,
    tags_query: tree_sitter_rust::TAGS_QUERY,
};

/// Build the grammar, validating its tags query the same way a plugin's would be.
pub(crate) fn grammar() -> Result<Arc<LoadedGrammar>> {
    let language = Language::from(DESCRIPTOR.language);
    LoadedGrammar::new(DESCRIPTOR.name, language, DESCRIPTOR.tags_query).map(Arc::new)
}
