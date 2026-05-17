//! F-39 PR-B: criterion benchmark infrastructure smoke test.
//!
//! Now that `src/lib.rs` exists (feature-30 PR-1), benches under
//! `benches/` can drive internal kb-mcp APIs directly. `mmr_perf.rs`
//! does that for `kb_mcp::mmr::mmr_select` (Option 1: lib API direct
//! call). `search_latency.rs` still spawns the kb-mcp binary as a
//! subprocess (Option 2: end-to-end wall-clock measurement) because
//! the search pipeline depends on rusqlite + ONNX runtime that we
//! do not want to embed in every bench iteration.
//!
//! This file establishes the criterion bench harness with a workload
//! that *is* representative of a hot inner loop in the search path:
//! byte-level case folding via `to_ascii_lowercase`, which
//! `compute_match_spans` performs once per chunk on every search.
//! The intent is twofold:
//!   - confirm criterion + `harness = false` wiring works in
//!     CI / local `cargo bench`,
//!   - give a stable baseline for spotting regressions in the
//!     stdlib or compiler that affect kb-mcp's hot path.
//!
//! Each `[[bench]]` entry in `Cargo.toml` MUST set `harness = false`
//! because criterion brings its own harness. `name` matches the file
//! under `benches/`.

use criterion::{Criterion, black_box, criterion_group, criterion_main};

/// 4 KiB chunk of mixed-case ASCII content. Roughly the size of a
/// well-trimmed heading-scoped chunk in a real KB.
fn typical_chunk_4kb() -> String {
    let line = "The Quick Brown Fox Jumps Over The Lazy Dog 0123456789. ";
    line.repeat(4096 / line.len() + 1)
        .chars()
        .take(4096)
        .collect()
}

fn bench_to_ascii_lowercase_typical_chunk(c: &mut Criterion) {
    let chunk = typical_chunk_4kb();
    c.bench_function("to_ascii_lowercase / 4 KiB ASCII chunk", |b| {
        b.iter(|| {
            let s = black_box(&chunk);
            let lower = s.to_ascii_lowercase();
            black_box(lower)
        });
    });
}

/// Empty string boundary case. `to_ascii_lowercase` on a `""` should
/// be ~free; a regression here would catch a degenerate copy in
/// stdlib.
fn bench_to_ascii_lowercase_empty(c: &mut Criterion) {
    c.bench_function("to_ascii_lowercase / empty", |b| {
        b.iter(|| {
            let s = black_box("");
            let lower = s.to_ascii_lowercase();
            black_box(lower)
        });
    });
}

criterion_group!(
    benches,
    bench_to_ascii_lowercase_typical_chunk,
    bench_to_ascii_lowercase_empty
);
criterion_main!(benches);
