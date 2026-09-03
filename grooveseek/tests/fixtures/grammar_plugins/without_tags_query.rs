//! A plugin missing [`groove_grammar_abi::symbols::TAGS_QUERY`].
//!
//! One rung of the lookup-order ladder; `without_language.rs` carries the explanation of what
//! the ladder as a whole pins and why the macro cannot build these.
//!
//! Not the same case as `null_tags.rs`: that one exports the symbol and answers NULL, which is
//! how an author says "this grammar has no tags query". This one does not export it at all.

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
    c"notagsquery".as_ptr()
}

#[unsafe(no_mangle)]
pub extern "C" fn groove_grammar_extensions() -> *const c_char {
    c"py".as_ptr()
}

// `groove_grammar_tags_query` is deliberately absent: it is the point of the fixture.

#[unsafe(no_mangle)]
pub extern "C" fn groove_grammar_build_info() -> *const c_char {
    c"grooveseek test fixture; grammar notagsquery".as_ptr()
}
