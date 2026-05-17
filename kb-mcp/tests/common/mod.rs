//! Shared helpers for integration tests under `tests/`.
//!
//! Cargo's integration test harness compiles each `tests/<name>.rs` as
//! a separate crate, so a `mod common;` declaration must appear in every
//! test file that uses it. Helpers are kept minimal — anything beyond
//! 1-2 fixtures should live next to the test that needs it, not here.
//!
//! Status today:
//! - [`temp`] — temp-directory RAII helpers (replaces the 7 hand-rolled
//!   `TempKb` / `TempDir` structs across `tests/*.rs`). New tests should
//!   prefer these; existing tests are intentionally untouched per the
//!   F-39 audit note ("新規 test 用、既存 test には手付けず").
//!
//! Note: this module is referenced from PR-B's `benches/` after F-39 is
//! complete. The intent is for `benches/*.rs` to also share the same
//! `TempRoot` machinery once they are added.

#![allow(dead_code)] // helpers are referenced lazily from individual integration tests

pub mod ansi;
pub mod mcp;
pub mod temp;
