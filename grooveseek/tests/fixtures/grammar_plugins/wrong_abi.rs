//! A plugin built against a contract version this groove does not speak.
//!
//! Written out symbol by symbol rather than through
//! [`groove_grammar_abi::groove_grammar_plugin`], because the macro always reports
//! [`groove_grammar_abi::ABI_VERSION`] -- the version of whatever copy of the ABI crate the
//! plugin was compiled against. Saying a *different* number is the one thing a plugin built the
//! supported way cannot do.
//!
//! What it pins: **the version is settled before any other export is looked up.** Every other
//! export is read through the signature *this* ABI defines, so a library at another version may
//! have dropped a symbol -- reported as a missing export, sending the user to look for a corrupt
//! file rather than a mismatched version -- or kept the name and changed the signature, in which
//! case calling it is undefined behaviour.
//!
//! So this fixture exports the version **and nothing else**. A loader that read the exports
//! first could only answer "it does not export groove_grammar_language", and that sentence is
//! what the test asserts is absent. Its pair is `without_language.rs`, which is missing the same
//! export and *does* get that sentence, because the only thing it declares differently is a
//! version number this groove speaks.

/// The point of the fixture.
///
/// `wrapping_add(1)` rather than a literal: a literal would quietly become the *correct*
/// version on the day [`groove_grammar_abi::ABI_VERSION`] is bumped past it, and the fixture
/// would be accepted instead of refused with nothing to say it had stopped testing anything.
#[unsafe(no_mangle)]
pub extern "C" fn groove_grammar_abi_version() -> u32 {
    groove_grammar_abi::ABI_VERSION.wrapping_add(1)
}
