//! A plugin whose grammar export answers once and then stops.
//!
//! The first call hands over a real parse table; every call after it hands over NULL. Nothing
//! in the ABI forbids that — an export is a C function, and only the one
//! [`groove_grammar_abi::groove_grammar_plugin`] writes is guaranteed to be a constant. This
//! fixture is what a hand-written plugin holding state looks like from the outside.
//!
//! What it pins: the loader checks a pointer and then *uses* one, and those have to be the same
//! pointer. `tree_sitter::Language` can only be built by calling a `LanguageFn`, so a loader
//! that checked the plugin's export and then handed that same export to tree-sitter would be
//! checking the first answer and dereferencing the second. Against this fixture that is a
//! segfault; against the parked-and-handed-over form it is an ordinary refusal.
//!
//! It declares `rs` while being loaded under `py`, so the run ends at the extension check —
//! which is the point: reaching a *later* check at all is the evidence that the earlier one
//! consumed the pointer it verified.

// The parse table comes from the same optional dependency the compiled-in Rust grammar uses.
// With that feature off there is nothing real to hand over on the first call, and a fixture
// that answered NULL both times would be `null_language.rs` rather than this one.
#![cfg(feature = "grammar-rust")]

use core::ffi::c_char;
use core::sync::atomic::{AtomicUsize, Ordering};

/// How many times the grammar export has been asked.
static CALLS: AtomicUsize = AtomicUsize::new(0);

#[unsafe(no_mangle)]
pub extern "C" fn groove_grammar_abi_version() -> u32 {
    groove_grammar_abi::ABI_VERSION
}

/// Real the first time, NULL every time after. The point of the fixture.
#[unsafe(no_mangle)]
pub extern "C" fn groove_grammar_language() -> *const () {
    if CALLS.fetch_add(1, Ordering::SeqCst) == 0 {
        // SAFETY: the function `LANGUAGE` wraps is the one the Tree-sitter CLI generated for
        // the Rust grammar; calling it returns a pointer to that grammar's static parse table.
        unsafe { (tree_sitter_rust::LANGUAGE.into_raw())() }
    } else {
        core::ptr::null()
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn groove_grammar_name() -> *const c_char {
    c"flakygrammar".as_ptr()
}

/// `rs`, while the loader looks for this file under `py`: the refusal this fixture should reach.
#[unsafe(no_mangle)]
pub extern "C" fn groove_grammar_extensions() -> *const c_char {
    c"rs".as_ptr()
}

/// # Safety
///
/// `len` must be null or point at a writable `usize`, the same contract
/// [`groove_grammar_abi::groove_grammar_plugin`] states for the export it generates.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn groove_grammar_tags_query(len: *mut usize) -> *const u8 {
    let query: &'static str = tree_sitter_rust::TAGS_QUERY;
    if !len.is_null() {
        // SAFETY: the caller contracts to pass a writable `usize` or null, and null is checked.
        unsafe { *len = query.len() };
    }
    query.as_ptr()
}

#[unsafe(no_mangle)]
pub extern "C" fn groove_grammar_build_info() -> *const c_char {
    c"grooveseek test fixture; grammar flakygrammar".as_ptr()
}
