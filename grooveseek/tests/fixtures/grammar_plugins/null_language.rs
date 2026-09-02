//! A plugin whose contract is in order except that it hands back no grammar.
//!
//! Written out symbol by symbol rather than through
//! [`groove_grammar_abi::groove_grammar_plugin`], because the macro cannot produce this: it
//! builds `groove_grammar_language` from a `LanguageFn` a real grammar crate supplies, so
//! every library the macro makes returns a parse table. The shapes the loader refuses are
//! exactly the ones the macro cannot express, which is why they need hand-written fixtures.
//!
//! What it pins: the loader asks for the parse table and checks it **before** anything reads
//! through it. `tree_sitter`'s `abi_version` dereferences what it is handed without a check of
//! its own, so a NULL here used to take the process down -- no diagnostic, and under the
//! Windows service no output at all -- while every other malformed plugin was refused with a
//! sentence naming the file.

use core::ffi::c_char;

#[unsafe(no_mangle)]
pub extern "C" fn groove_grammar_abi_version() -> u32 {
    groove_grammar_abi::ABI_VERSION
}

/// The point of the fixture.
#[unsafe(no_mangle)]
pub extern "C" fn groove_grammar_language() -> *const () {
    core::ptr::null()
}

#[unsafe(no_mangle)]
pub extern "C" fn groove_grammar_name() -> *const c_char {
    c"nullgrammar".as_ptr()
}

/// `py`, the id this fixture is loaded under: the extension is checked *after* the grammar,
/// so declaring the wrong one here would hide what this fixture is for behind a mismatch.
#[unsafe(no_mangle)]
pub extern "C" fn groove_grammar_extensions() -> *const c_char {
    c"py".as_ptr()
}

/// Valid, and never compiled: the loader returns before it builds anything from the grammar.
///
/// # Safety
///
/// `len` must be null or point at a writable `usize`, the same contract
/// [`groove_grammar_abi::groove_grammar_plugin`] states for the export it generates.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn groove_grammar_tags_query(len: *mut usize) -> *const u8 {
    let query: &'static str = "";
    if !len.is_null() {
        // SAFETY: the caller contracts to pass a writable `usize` or null, and null is checked.
        unsafe { *len = query.len() };
    }
    query.as_ptr()
}

#[unsafe(no_mangle)]
pub extern "C" fn groove_grammar_build_info() -> *const c_char {
    c"grooveseek test fixture; grammar nullgrammar".as_ptr()
}
