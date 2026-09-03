//! A plugin handing over a parse table from a Tree-sitter newer than this build speaks.
//!
//! The upper half of the pair whose lower half is `stale_tree_sitter.rs`, which carries the
//! argument for why handing over a table that is not one is sound: between the loader taking
//! this pointer and refusing it, the only thing read is `abi_version`, the first field of
//! `struct TSLanguage`.
//!
//! What it pins: **the range is a range.** The loader refuses a grammar whose Tree-sitter ABI is
//! `found < min || found > max`, and a fixture on one side of that leaves the other side able to
//! be deleted with every test still green. This is the side a grammar built with a newer
//! Tree-sitter CLI than groove links lands on -- the ordinary way to end up here, since a
//! plugin is downloaded separately and nothing makes its CLI match groove's.
//!
//! Both fixtures are written out rather than sharing a module: every fixture in this directory
//! stands alone, and the loader is the only thing they have in common. The safety argument is
//! stated once, next door, and this file is the one that would be wrong if it drifted.

use core::ffi::c_char;

/// A parse table in shape only. Deliberately identical to the one in `stale_tree_sitter.rs`.
#[repr(C)]
struct ForgedLanguage {
    abi_version: u32,
    rest: [usize; 64],
}

/// The point of the fixture: a version above anything this runtime will accept.
///
/// `u32::MAX` rather than a near-boundary literal, and written as the constant rather than
/// spelled out: `LANGUAGE_VERSION` counts grammar formats, one every few years, so the value
/// this saturates cannot be reached by a Tree-sitter bump. Where `stale_tree_sitter.rs` accepts
/// a small residual risk that its version stops being out of range, this one has none.
static FORGED: ForgedLanguage = ForgedLanguage {
    abi_version: u32::MAX,
    rest: [0; 64],
};

#[unsafe(no_mangle)]
pub extern "C" fn groove_grammar_abi_version() -> u32 {
    groove_grammar_abi::ABI_VERSION
}

/// Hands over [`FORGED`]: a live, correctly aligned address that is not a grammar.
#[unsafe(no_mangle)]
pub extern "C" fn groove_grammar_language() -> *const () {
    core::ptr::from_ref(&FORGED).cast()
}

#[unsafe(no_mangle)]
pub extern "C" fn groove_grammar_name() -> *const c_char {
    c"futuregrammar".as_ptr()
}

/// `rs`, while the loader looks for this file under `py`, for the reason `stale_tree_sitter.rs`
/// gives: it keeps the mutation probe from having to read a zeroed parse table.
#[unsafe(no_mangle)]
pub extern "C" fn groove_grammar_extensions() -> *const c_char {
    c"rs".as_ptr()
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
    c"grooveseek test fixture; grammar futuregrammar".as_ptr()
}
