//! A plugin whose name is NUL-terminated but is not UTF-8.
//!
//! Hand-written because [`groove_grammar_abi::groove_grammar_plugin`] builds the name from a
//! Rust string literal, which is UTF-8 by construction. A C string is a run of bytes ending in
//! NUL and nothing more, so this is what a plugin written in C -- or one that read its own name
//! out of a file in some other encoding -- hands over without meaning anything by it.
//!
//! What it pins: the bytes between the pointer and the NUL are **validated**, not assumed. The
//! name becomes `lang:<name>` on every chunk and is leaked as a `&'static str`, so bytes that
//! are not text would be carried the whole length of the index. It is the second half of the
//! same helper `null_name.rs` covers: NULL and not-text are separate answers, and this fixture
//! is the one that gets past the pointer check to reach the second.

use core::ffi::c_char;

#[unsafe(no_mangle)]
pub extern "C" fn groove_grammar_abi_version() -> u32 {
    groove_grammar_abi::ABI_VERSION
}

/// Never called: the loader returns at the name, before it asks for a table.
#[unsafe(no_mangle)]
pub extern "C" fn groove_grammar_language() -> *const () {
    core::ptr::null()
}

/// The point of the fixture: NUL-terminated, so `CStr` accepts it, and `0xFF` is a byte no
/// UTF-8 sequence may contain, so `to_str` does not.
///
/// Written as a byte literal rather than a `c"..."` literal because a C string literal in Rust
/// source is itself checked to be UTF-8, which is the property being violated.
// `manual_c_str_literals` asks for exactly the `c"..."` this fixture cannot have: the whole
// point is a NUL-terminated string whose bytes are not UTF-8, and that literal form will not
// hold one. There is no rewrite that satisfies the lint and keeps the fixture.
#[allow(clippy::manual_c_str_literals)]
#[unsafe(no_mangle)]
pub extern "C" fn groove_grammar_name() -> *const c_char {
    b"py\xFF\0".as_ptr() as *const c_char
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
    c"grooveseek test fixture; grammar badutf8name".as_ptr()
}
