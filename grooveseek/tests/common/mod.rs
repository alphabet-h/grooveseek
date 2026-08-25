//! Shared helpers for integration tests under `tests/`.
//!
//! Cargo's integration test harness compiles each `tests/<name>.rs` as
//! a separate crate, so a `mod common;` declaration must appear in every
//! test file that uses it.
//!
//! What belongs here is decided by how many tests ask the question, not by
//! how big the answer is. A fixture one test needs stays next to that test.
//! Something two tests would otherwise each carry a copy of belongs here,
//! because `AGENTS.md` ("One question gets one implementation") makes
//! collapsing the copy part of the change that adds the second caller.
//!
//! Status today:
//! - [`temp`] — temp-directory RAII helpers (replaces the 7 hand-rolled
//!   `TempKb` / `TempDir` structs across `tests/*.rs`). New tests should
//!   prefer these; existing tests are intentionally untouched per the
//!   F-39 audit note ("新規 test 用、既存 test には手付けず").
//! - [`docs`] — the walk that decides which Markdown files are this
//!   repository's documentation, and the reader that pulls commands out of
//!   them. Shared by the link guard and the command-copy guards.
//!
//! Note: this module is referenced from PR-B's `benches/` after F-39 is
//! complete. The intent is for `benches/*.rs` to also share the same
//! `TempRoot` machinery once they are added.

#![allow(dead_code)] // helpers are referenced lazily from individual integration tests

pub mod ansi;
pub mod docs;
pub mod mcp;
pub mod temp;
