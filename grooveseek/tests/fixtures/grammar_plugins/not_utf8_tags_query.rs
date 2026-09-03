//! A plugin whose tags query is bytes that are not UTF-8.
//!
//! Hand-written because [`groove_grammar_abi::groove_grammar_plugin`] takes the query as a
//! `&'static str`, which is UTF-8 by construction. The query crosses the ABI as a pointer and a
//! length rather than as a string -- it is data, and may embed a NUL -- so on this side of the
//! boundary it is bytes, and nothing but the loader's own check says they are text.
//!
//! What it pins: the bytes are validated rather than assumed. `str::from_utf8` is the check;
//! reaching for `from_utf8_unchecked` to save a pass over a few kilobytes would make a plugin's
//! own bytes into undefined behaviour, and this fixture is the shape that would produce it.

use core::ffi::c_char;

#[unsafe(no_mangle)]
pub extern "C" fn groove_grammar_abi_version() -> u32 {
    groove_grammar_abi::ABI_VERSION
}

/// Never called: the loader returns at the UTF-8 check, before it asks for a table.
#[unsafe(no_mangle)]
pub extern "C" fn groove_grammar_language() -> *const () {
    core::ptr::null()
}

#[unsafe(no_mangle)]
pub extern "C" fn groove_grammar_name() -> *const c_char {
    c"badutf8tags".as_ptr()
}

#[unsafe(no_mangle)]
pub extern "C" fn groove_grammar_extensions() -> *const c_char {
    c"py".as_ptr()
}

/// The point of the fixture: a length that honestly describes the pointer, and bytes that are
/// not text. `0xFF` and `0xFE` are the two bytes no UTF-8 sequence may contain at all.
///
/// # Safety
///
/// `len` must be null or point at a writable `usize`, the same contract
/// [`groove_grammar_abi::groove_grammar_plugin`] states for the export it generates.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn groove_grammar_tags_query(len: *mut usize) -> *const u8 {
    let query: &'static [u8] = b"\xFF\xFE";
    if !len.is_null() {
        // SAFETY: the caller contracts to pass a writable `usize` or null, and null is checked.
        unsafe { *len = query.len() };
    }
    query.as_ptr()
}

#[unsafe(no_mangle)]
pub extern "C" fn groove_grammar_build_info() -> *const c_char {
    c"grooveseek test fixture; grammar badutf8tags".as_ptr()
}
