//! F-60: Indexing pipeline throughput bench (chunker / chunker+embedder).
//!
//! Two function groups, gated differently:
//!
//! 1. `bench_chunker_only` (default `cargo bench --bench index_throughput`)
//!    Drives `kb_mcp::markdown::parse(raw)` on the existing
//!    `tests/fixtures/kb-bench/` 3 files. Pure CPU + allocation, no
//!    model DL, ~us-ms order. Useful for regression-tracking the parser
//!    after pulldown-cmark / chunker tweaks.
//!
//! 2. `bench_chunker_plus_embedder` (heavy-bench gate;
//!    `cargo bench --features heavy-bench --bench index_throughput`)
//!    Loads `Embedder::with_model(BgeSmallEnV15)` once in the outer
//!    scope, wraps it in `RefCell` so 3 closures can share mutable
//!    access (criterion `Bencher::iter` requires `FnMut`; `&mut
//!    Embedder` cannot be split across 3 simultaneous closures, so
//!    interior mutability is the cleanest fix; `borrow_mut` overhead
//!    is ns-level and does not perturb steady-state). Each iteration
//!    chunks one file and embeds all chunk texts.
//!
//! This mirrors `benches/search_latency.rs`'s "light fns default,
//! heavy fns gated" layout.

use criterion::{Criterion, black_box, criterion_group, criterion_main};
use kb_mcp::markdown::parse;

const KB_BENCH_FILES: &[(&str, &str)] = &[
    (
        "mcp-protocol.md",
        include_str!("../tests/fixtures/kb-bench/mcp-protocol.md"),
    ),
    (
        "rust-async.md",
        include_str!("../tests/fixtures/kb-bench/rust-async.md"),
    ),
    (
        "sqlite-vec.md",
        include_str!("../tests/fixtures/kb-bench/sqlite-vec.md"),
    ),
];

// ---------------------------------------------------------------------------
// chunker only (default)
// ---------------------------------------------------------------------------

fn bench_chunker_only(c: &mut Criterion) {
    for (name, content) in KB_BENCH_FILES {
        c.bench_function(&format!("chunker / single file ({name})"), |b| {
            b.iter(|| {
                let parsed = parse(black_box(content));
                black_box(parsed);
            });
        });
    }
}

// ---------------------------------------------------------------------------
// chunker + embedder (heavy-bench gate)
// ---------------------------------------------------------------------------

#[cfg(feature = "heavy-bench")]
fn bench_chunker_plus_embedder(c: &mut Criterion) {
    use kb_mcp::embedder::{Embedder, ModelChoice};
    use std::cell::RefCell;

    // Load BGE-small once. Cold cache = ~30s (model file load + ONNX
    // session init); warm = <1s. Outer-scope load amortizes across all
    // bench fns below.
    let embedder = RefCell::new(
        Embedder::with_model(ModelChoice::BgeSmallEnV15)
            .expect("Embedder::with_model failed (model DL?)"),
    );

    for (name, content) in KB_BENCH_FILES {
        c.bench_function(&format!("chunker+embedder / single file ({name})"), |b| {
            b.iter(|| {
                let parsed = parse(black_box(content));
                let texts: Vec<&str> = parsed.chunks.iter().map(|c| c.content.as_str()).collect();
                let mut e = embedder.borrow_mut();
                let vecs = e.embed_texts(&texts).expect("embed_texts failed");
                black_box(vecs);
            });
        });
    }
}

// ---------------------------------------------------------------------------
// criterion_group / criterion_main
// ---------------------------------------------------------------------------
//
// Mirror `benches/search_latency.rs`: register 1 group when `heavy-bench`
// is off, 2 groups when on, with the same `benches` symbol so a single
// `criterion_main!` invocation works for both.

#[cfg(not(feature = "heavy-bench"))]
criterion_group!(benches, bench_chunker_only);

#[cfg(feature = "heavy-bench")]
criterion_group!(benches, bench_chunker_only, bench_chunker_plus_embedder);

criterion_main!(benches);
