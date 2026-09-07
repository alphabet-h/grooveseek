//! The source file and the configurations the code-parser tests share.
//!
//! Two test crates read these: `tests/code_formats_cli.rs`, which spawns `groove index` and
//! `groove search` around them and is therefore `#[ignore]` end to end, and
//! `tests/code_formats_light.rs`, which parses and stores the same file in-process with no
//! model and runs on every pull request (AV-16). One copy, per this module's rule (see
//! [`crate::common`]): a second crate carrying its own `SAMPLE_RS` would let the two drift
//! until the heavy suite and the light one described different files.
//!
//! # What the light suite pins, by line
//!
//! The line numbers below are 1-based lines of [`crate::common::code_fixtures::SAMPLE_RS`]
//! itself, counted from its first
//! `use` line. They are asserted literally, so a change to the fixture's shape is a change to
//! those assertions and to `code_formats_cli.rs`'s "the range must start at the doc comment"
//! check — which is the point: the numbers live here, next to the text they describe.
//!
//! | lines | what is there | the chunk it becomes |
//! |---|---|---|
//! | 1-2 | two `use` lines | a headingless gap chunk, no [`grooveseek::parser::Chunk::symbol_kind`] |
//! | 4-6 | the `///` doc comment | pulled into the chunk below it |
//! | 7-17 | `pub fn fuse_ranked_lists` | `function fuse_ranked_lists`, lines 4-17 |
//! | 19-21 | `pub struct RankTable` | `class RankTable`, lines 19-21 |
//! | 24-26 | `pub fn insert_row` inside the `impl` | `method insert_row`, lines 24-26 |

/// Opts the `rs` parser in alongside the always-on default `md`.
pub const PARSERS_MD_RS: &str =
    "model = \"bge-small-en-v1.5\"\n[parsers]\nenabled = [\"md\", \"rs\"]\n";

/// No `[parsers]` section, so the registry falls back to `["md"]` only.
pub const PARSERS_DEFAULT: &str = "model = \"bge-small-en-v1.5\"\n";

/// A file with one documented function, one struct and one `impl` block.
///
/// `reciprocal_fusion_weight` is a term that appears only inside a function body, so a hit on
/// it can only have come from a code chunk.
pub const SAMPLE_RS: &str = r#"use std::collections::BTreeMap;
use std::fmt::Debug;

/// Combines two ranked lists into one.
///
/// The doc comment is part of the definition's chunk, not of the imports above it.
pub fn fuse_ranked_lists(a: &[usize], b: &[usize]) -> Vec<usize> {
    let reciprocal_fusion_weight = 60;
    let mut out = Vec::new();
    for (rank, id) in a.iter().enumerate() {
        out.push(id + rank + reciprocal_fusion_weight);
    }
    for (rank, id) in b.iter().enumerate() {
        out.push(id + rank + reciprocal_fusion_weight);
    }
    out
}

pub struct RankTable {
    rows: BTreeMap<usize, usize>,
}

impl RankTable {
    pub fn insert_row(&mut self, key: usize, value: usize) {
        self.rows.insert(key, value);
    }
}
"#;

pub const SAMPLE_MD: &str = "---\ntitle: Fusion notes\n---\n\n## Reciprocal rank fusion\n\nThe prose page also talks about reciprocal fusion weight, at length, so that a search for it\nhas something to find in both halves of the knowledge base and the two can be told apart by\nwhat the response carries rather than by which one happened to win.\n";
