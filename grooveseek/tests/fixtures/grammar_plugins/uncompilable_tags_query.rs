//! A valid plugin whose tags query does not compile against its own grammar.
//!
//! Built by [`groove_grammar_abi::groove_grammar_plugin`] like `claims_rs.rs`: the macro takes
//! the query as an expression and never compiles it, so a broken one is expressible without
//! hand-writing the exports. Everything else here is in order -- a real parse table, a valid
//! name, the right extension -- so the only thing wrong is the query.
//!
//! **Declares `py`, the id it is loaded under**, and carries a valid name: the query is the
//! *last* thing checked, so anything else wrong would be refused earlier and this fixture would
//! stop testing what it is for.
//!
//! What it pins: the query is compiled while the plugin is being accepted, and a failure there
//! is a refusal rather than a panic or a grammar that loads and then produces nothing. `((` is
//! unbalanced against any grammar at all, which keeps the fixture about the check rather than
//! about a particular node name that a future `tree-sitter-rust` might rename.

#[cfg(feature = "grammar-rust")]
groove_grammar_abi::groove_grammar_plugin! {
    name = "brokenquery",
    extension = "py",
    language = tree_sitter_rust::LANGUAGE,
    tags_query = "((",
}
