//! kb-mcp library crate.
//!
//! This crate exists primarily to expose internal modules to integration
//! tests and benchmarks under `tests/` and `benches/`. The user-facing
//! product is the `kb-mcp` binary (`src/main.rs`); the library API is
//! intentionally unstable and not intended for external consumers.
//!
//! All modules are re-exported here verbatim from `src/<mod>.rs` (or
//! `src/<mod>/mod.rs` for `parser` / `transport`). The binary in
//! `src/main.rs` consumes them via `use kb_mcp::<mod>::...;`.

pub mod config;
pub mod db;
pub mod embedder;
pub mod eval;
pub mod graph;
pub mod indexer;
pub mod markdown;
pub mod mmr;
pub mod parent;
pub mod parser;
pub mod quality;
pub mod schema;
pub mod server;
pub mod service;
pub mod transport;
pub mod watcher;

use std::path::{Path, PathBuf};

/// Resolve the database path from a knowledge-base directory.
///
/// The `.kb-mcp.db` file is placed in the **parent** of `kb_path`
/// (i.e. the repository root when `kb_path` is `knowledge-base/`).
pub fn resolve_db_path(kb_path: &Path) -> PathBuf {
    kb_path
        .parent()
        .unwrap_or(Path::new("."))
        .join(".kb-mcp.db")
}
