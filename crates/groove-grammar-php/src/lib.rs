//! The PHP grammar, exported across the C ABI groove loads grammars through.
//!
//! There is no logic here, for the same reason there is none in the Python
//! plugin: what a plugin has to get right lives in
//! [`groove_grammar_abi::groove_grammar_plugin`], so a second grammar is a
//! manifest and one macro call rather than an FFI surface to review again.
//!
//! What this crate chooses is the four values:
//!
//! - `name` becomes `lang:php` on every chunk, so it is the word a filter is
//!   written against. **Here it is also the id**, which the Python plugin's
//!   pair (`python` and `py`) may lead a reader to expect it not to be: PHP
//!   spells the language and the extension the same way, and nothing needs the
//!   two to differ.
//! - `extension` must be the one the enabled id stands for. groove finds this
//!   file by building its name from the id in `[parsers].enabled`, so the two
//!   already claim to be the same thing, and the loader refuses a library that
//!   disagrees rather than letting a mispackaged plugin move a whole language.
//! - `language` is `LANGUAGE_PHP`, not `LANGUAGE_PHP_ONLY`. The crate ships
//!   both, and the difference is what surrounds the code: a `.php` file may
//!   open with HTML and switch into PHP at `<?php`, which is the grammar named
//!   here; `php_only` parses a file that is code from its first byte. Upstream
//!   settles which is which by declaring `file-types: php` on the `php` grammar
//!   alone (its `tree-sitter.json`), and a `.php` file that begins with markup
//!   is the case that would otherwise fail to parse.
//! - `tags_query` comes from the same crate version, which is the reason a
//!   grammar and its `tags.scm` travel together: a query compiled against a
//!   different parse table fails at load, and the loader reports that as a
//!   refused file rather than parsing PHP with the wrong table.
//!
//! What that query captures decides what becomes a definition chunk, and it is
//! not the same set for every language. PHP's captures namespaces, classes,
//! interfaces, traits (as `interface`), functions, methods and properties. It
//! has no `@definition.constant`, so a `const` declaration is filled in by line
//! like any other construct no definition covers — the same shape Rust's query
//! has, and worth knowing before reading a search result and concluding a
//! constant went missing.

groove_grammar_abi::groove_grammar_plugin! {
    name = "php",
    extension = "php",
    language = tree_sitter_php::LANGUAGE_PHP,
    tags_query = tree_sitter_php::TAGS_QUERY,
}
