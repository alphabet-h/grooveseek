//! (feature-56 PR-3a) A library that loads and exports none of the contract.
//!
//! The fixture behind the "the file is there and was refused" diagnostic, for the case that
//! cannot be produced by corrupting bytes: a perfectly good dynamic library that simply is not
//! a groove grammar. Corrupted bytes fail at `dlopen`; this one fails at the first `dlsym`,
//! which is a different branch and a different sentence.
//!
//! It exports one symbol so that the library is not empty enough for a linker to discard, and
//! deliberately not one of the six in [`groove_grammar_abi::symbols`].

#[unsafe(no_mangle)]
pub extern "C" fn groove_not_a_grammar_at_all() -> u32 {
    0
}
