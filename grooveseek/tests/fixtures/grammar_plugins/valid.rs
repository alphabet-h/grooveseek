//! (feature-56 PR-3a) A grammar plugin that is real enough to be accepted.
//!
//! Built as a `cdylib` by `cargo test` (see the `[[example]]` entries in `Cargo.toml`) and
//! copied into a temporary grammar directory by `tests/grammar_plugin_cli.rs`. It is a
//! **fixture**, not a shipped grammar: the Python grammar itself arrives in PR-3b.
//!
//! It hands over the Rust parse table under the `py` extension. That mismatch is the point —
//! the loader's job is to check a contract and produce a `LoadedGrammar` — grooveseek's own
//! type, in `src/parser/code/`, which is `pub(crate)` and so cannot be linked from here — and
//! it neither knows
//! nor cares which language the table describes. Using a grammar that is already a dependency
//! keeps the fixture from adding one, and keeps what is being tested to the loader.

// The parse table comes from the same optional dependency the compiled-in Rust grammar uses,
// so with that feature off there is nothing to export. The resulting empty library is still a
// valid `cdylib`; no test asks it for a grammar, because none of them run in that build.
#[cfg(feature = "grammar-rust")]
groove_grammar_abi::groove_grammar_plugin! {
    name = "fakepy",
    extension = "py",
    language = tree_sitter_rust::LANGUAGE,
    tags_query = tree_sitter_rust::TAGS_QUERY,
}
