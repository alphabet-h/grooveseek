//! Entry-point test crate that exercises the helpers in `tests/common/`.
//!
//! Cargo compiles each `tests/*.rs` as a separate integration-test
//! crate. `tests/common/mod.rs` itself is *not* one of those entry
//! points (it is a regular module loaded via `mod common;` from each
//! integration test that wants it). To actually run the inline unit
//! tests inside `tests/common/temp.rs`, at least one entry-point
//! crate must include the module — that is what this file is for.
//!
//! Future integration tests can copy the `mod common;` declaration
//! below to reuse `common::temp::TempRoot` / `TempKbLayout`.

mod common;

// No additional `#[test]` functions here — the unit tests we care
// about live inside `common::temp::tests` and fire automatically.
