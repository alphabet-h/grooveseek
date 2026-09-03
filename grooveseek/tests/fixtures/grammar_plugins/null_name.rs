//! A plugin whose name export hands back NULL.
//!
//! Hand-written because [`groove_grammar_abi::groove_grammar_plugin`] builds the name from a
//! string literal with `concat!`, so every library the macro makes returns a pointer into its
//! own rodata. NULL is a shape only a hand-written plugin can reach.
//!
//! What it pins: **NULL is answered as NULL, not as bad UTF-8.** The loader copies each string
//! export out through one helper, and that helper checks the pointer before
//! `CStr::from_ptr` -- which has no check of its own and would dereference NULL. Folding the
//! two cases together would make the friendliest possible mistake into undefined behaviour, and
//! would also send the reader looking for an encoding problem in a string that was never there.
//!
//! The name is the first of the three strings copied out, so this fixture is what reaches that
//! line at all; `plugin.rs` names which export was NULL from an argument, and the wording of all
//! three is pinned by its unit tests rather than by three fixtures here.

use core::ffi::c_char;

#[unsafe(no_mangle)]
pub extern "C" fn groove_grammar_abi_version() -> u32 {
    groove_grammar_abi::ABI_VERSION
}

/// Never called: the loader returns at the NULL name, before it asks for a table.
#[unsafe(no_mangle)]
pub extern "C" fn groove_grammar_language() -> *const () {
    core::ptr::null()
}

/// The point of the fixture.
#[unsafe(no_mangle)]
pub extern "C" fn groove_grammar_name() -> *const c_char {
    core::ptr::null()
}

#[unsafe(no_mangle)]
pub extern "C" fn groove_grammar_extensions() -> *const c_char {
    c"py".as_ptr()
}

/// Valid, and read before the name: the tags query crosses the ABI as bytes, and this fixture
/// has to get past it to reach the string it is about.
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
    c"grooveseek test fixture; grammar nullname".as_ptr()
}
