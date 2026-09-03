//! A plugin missing [`groove_grammar_abi::symbols::LANGUAGE`], the first export read after the
//! version.
//!
//! One rung of the ladder that pins the lookup order the ABI documents. Hand-written because
//! [`groove_grammar_abi::groove_grammar_plugin`] emits all six exports or none: omitting exactly
//! one is a shape the macro cannot express.
//!
//! What it pins with its neighbours: the loader looks each symbol up **in the order
//! [`groove_grammar_abi::symbols`] lists them**, which that module states in prose and nothing
//! else checked. A loader that reordered them would still refuse every one of these fixtures --
//! just not with the name each one is missing.
//!
//! Its other pair is `wrong_abi.rs`, which lacks this same export and is refused for its version
//! instead, because the version is settled before any export is looked up.

use core::ffi::c_char;

#[unsafe(no_mangle)]
pub extern "C" fn groove_grammar_abi_version() -> u32 {
    groove_grammar_abi::ABI_VERSION
}

// `groove_grammar_language` is deliberately absent: it is the point of the fixture.

#[unsafe(no_mangle)]
pub extern "C" fn groove_grammar_name() -> *const c_char {
    c"nolanguage".as_ptr()
}

#[unsafe(no_mangle)]
pub extern "C" fn groove_grammar_extensions() -> *const c_char {
    c"py".as_ptr()
}

/// Valid, and never read: the loader returns at the missing symbol above it.
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
    c"grooveseek test fixture; grammar nolanguage".as_ptr()
}
