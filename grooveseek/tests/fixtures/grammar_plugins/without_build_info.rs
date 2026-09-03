//! A plugin missing [`groove_grammar_abi::symbols::BUILD_INFO`], the last export read.
//!
//! One rung of the lookup-order ladder; `without_language.rs` carries the explanation of what
//! the ladder as a whole pins and why the macro cannot build these.
//!
//! The rung worth having on its own: build info is **diagnostic only** -- it is logged when a
//! plugin loads and no check reads it -- so it is the export a loader is likeliest to start
//! treating as optional. The ABI names it alongside the other five, and this fixture is what
//! says the loader still refuses a library that omits it.

use core::ffi::c_char;

#[unsafe(no_mangle)]
pub extern "C" fn groove_grammar_abi_version() -> u32 {
    groove_grammar_abi::ABI_VERSION
}

/// Never called: the loader returns at the missing symbol below, before it asks for a table.
#[unsafe(no_mangle)]
pub extern "C" fn groove_grammar_language() -> *const () {
    core::ptr::null()
}

#[unsafe(no_mangle)]
pub extern "C" fn groove_grammar_name() -> *const c_char {
    c"nobuildinfo".as_ptr()
}

#[unsafe(no_mangle)]
pub extern "C" fn groove_grammar_extensions() -> *const c_char {
    c"py".as_ptr()
}

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

// `groove_grammar_build_info` is deliberately absent: it is the point of the fixture.
