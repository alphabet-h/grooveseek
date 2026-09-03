//! A plugin declaring a tags query far larger than any build of groove will read.
//!
//! Hand-written because [`groove_grammar_abi::groove_grammar_plugin`] writes
//! `query.len()` for a `&'static str` it was handed: a length that does not describe the
//! pointer beside it is the one thing the macro cannot say.
//!
//! What it pins: the declared length is checked **before** it is used to build a slice. The
//! pointer here is real and two bytes long, so a loader that dropped the check would hand
//! `slice::from_raw_parts` a gigabyte starting at a two-byte static -- not a refusal and not a
//! diagnostic, but a read straight off the end of the mapping. That is the same "dies without a
//! word" signature the NULL exports had, which is why the length is a refusal threshold rather
//! than a budget.
//!
//! The declared size is a literal because the cap it has to exceed is groove's own and private
//! to the loader. A gigabyte is orders of magnitude past any tags query in the Tree-sitter
//! ecosystem, so a future build reading that much is not a real prospect -- and if one ever
//! did, this fixture would be accepted and its test would fail rather than pass in silence.

use core::ffi::c_char;

#[unsafe(no_mangle)]
pub extern "C" fn groove_grammar_abi_version() -> u32 {
    groove_grammar_abi::ABI_VERSION
}

/// Never called: the loader returns at the length check, before it asks for a table.
#[unsafe(no_mangle)]
pub extern "C" fn groove_grammar_language() -> *const () {
    core::ptr::null()
}

#[unsafe(no_mangle)]
pub extern "C" fn groove_grammar_name() -> *const c_char {
    c"hugetags".as_ptr()
}

#[unsafe(no_mangle)]
pub extern "C" fn groove_grammar_extensions() -> *const c_char {
    c"py".as_ptr()
}

/// The point of the fixture: a real, short pointer and a length that does not describe it.
///
/// # Safety
///
/// `len` must be null or point at a writable `usize`, the same contract
/// [`groove_grammar_abi::groove_grammar_plugin`] states for the export it generates.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn groove_grammar_tags_query(len: *mut usize) -> *const u8 {
    // Non-NULL on purpose: a NULL pointer is refused a few lines earlier in the loader, and
    // that case already has `null_tags.rs`. This one has to get past it to reach the length.
    let query: &'static [u8] = b"()";
    if !len.is_null() {
        // SAFETY: the caller contracts to pass a writable `usize` or null, and null is checked.
        unsafe { *len = 1 << 30 };
    }
    query.as_ptr()
}

#[unsafe(no_mangle)]
pub extern "C" fn groove_grammar_build_info() -> *const c_char {
    c"grooveseek test fixture; grammar hugetags".as_ptr()
}
