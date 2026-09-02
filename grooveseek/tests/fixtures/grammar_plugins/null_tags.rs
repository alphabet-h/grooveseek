//! A plugin that says it has no tags query by handing back NULL.
//!
//! Hand-written for the same reason as `null_language.rs`: the macro builds the query from a
//! `&'static str`, so it can never produce a NULL pointer here.
//!
//! What it pins: the query arrives as a pointer and a length, and
//! `slice::from_raw_parts` requires a non-NULL pointer **even when the length is zero**. So
//! "no tags query" written the obvious way -- NULL, length 0 -- was undefined behaviour rather
//! than a refusal, and it is the shape a plugin author reaches for first.
//!
//! The grammar export returns NULL too, and that is not a second bug being tested: the loader
//! reads the exports before it asks for the parse table, so this fixture is refused for its
//! query and never gets that far. Returning a table would mean depending on a grammar crate to
//! test something that has nothing to do with one.

use core::ffi::c_char;

#[unsafe(no_mangle)]
pub extern "C" fn groove_grammar_abi_version() -> u32 {
    groove_grammar_abi::ABI_VERSION
}

#[unsafe(no_mangle)]
pub extern "C" fn groove_grammar_language() -> *const () {
    core::ptr::null()
}

#[unsafe(no_mangle)]
pub extern "C" fn groove_grammar_name() -> *const c_char {
    c"nulltags".as_ptr()
}

#[unsafe(no_mangle)]
pub extern "C" fn groove_grammar_extensions() -> *const c_char {
    c"py".as_ptr()
}

/// The point of the fixture: a length is written, and the pointer is NULL.
///
/// # Safety
///
/// `len` must be null or point at a writable `usize`, the same contract
/// [`groove_grammar_abi::groove_grammar_plugin`] states for the export it generates.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn groove_grammar_tags_query(len: *mut usize) -> *const u8 {
    if !len.is_null() {
        // SAFETY: the caller contracts to pass a writable `usize` or null, and null is checked.
        unsafe { *len = 0 };
    }
    core::ptr::null()
}

#[unsafe(no_mangle)]
pub extern "C" fn groove_grammar_build_info() -> *const c_char {
    c"grooveseek test fixture; grammar nulltags".as_ptr()
}
