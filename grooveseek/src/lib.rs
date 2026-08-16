//! groove library crate.
//!
//! This crate exists primarily to expose internal modules to integration
//! tests and benchmarks under `tests/` and `benches/`. The user-facing
//! product is the `groove` binary (`src/main.rs`); the library API is
//! intentionally unstable and not intended for external consumers.
//!
//! All modules are re-exported here verbatim from `src/<mod>.rs` (or
//! `src/<mod>/mod.rs` for `parser` / `transport`). The binary in
//! `src/main.rs` consumes them via `use grooveseek::<mod>::...;`.

pub mod config;
pub mod db;
pub mod doctor;
pub mod embedder;
pub mod eval;
pub mod exclusion;
pub mod graph;
/// Drawing the connection graph (E-3): DOT for Graphviz, and standalone SVG.
pub mod graph_render;
pub mod indexer;
/// Hard-link detection for the index / watcher / `get_document` guards (BU-20).
pub(crate) mod links;
pub mod markdown;
pub mod mmr;
pub mod parent;
pub mod parser;
/// Recovering from a poisoned mutex (BU-18). Internal: the binary and the
/// integration tests have no reason to reach for it, and every call site is a
/// lock this crate owns.
pub(crate) mod poison;
/// The MCP `prompts` surface (B-3): a fixed set of named recipes over the tools.
pub mod prompts;
pub mod quality;
/// The MCP `resources` surface (B-2): `kb://` URIs over the indexed corpus.
pub mod resources;
pub mod schema;
pub mod schema_compat;
pub mod server;
pub mod service;
/// Shared helpers for the unit tests in `src/**`. Test-only, so it does not
/// widen the library surface.
#[cfg(test)]
pub mod test_support;
pub mod transport;
pub mod tune;
pub mod watcher;

use std::path::{Path, PathBuf};

/// Resolve the database path from a knowledge-base directory.
///
/// The `.groove.db` file is placed in the **parent** of `kb_path`
/// (i.e. the repository root when `kb_path` is `knowledge-base/`).
pub fn resolve_db_path(kb_path: &Path) -> PathBuf {
    kb_path
        .parent()
        .unwrap_or(Path::new("."))
        .join(".groove.db")
}
