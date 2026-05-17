//! F-60 PR-1: search latency subprocess bench.
//!
//! Spawns `kb-mcp search ...` as a subprocess and measures wall-clock.
//! Captures the full server pipeline (RRF + MMR + parent retriever +
//! optional reranker), which is the only way to observe F-41's N+1
//! SQL elimination. Each iteration re-launches the binary; criterion's
//! sample size of 100 absorbs the launch overhead variance.
//!
//! The reranker-on bench downloads ~2.3 GB and is gated behind the
//! `heavy-bench` Cargo feature. Default invocation skips it:
//!   cargo bench --bench search_latency
//! Heavy invocation:
//!   cargo bench --features heavy-bench --bench search_latency

use criterion::{Criterion, black_box, criterion_group, criterion_main};
use std::process::Command;

/// Path to the kb-mcp binary built by cargo. Cargo provides this as an
/// env var when building integration tests / benchmarks, so it is robust
/// to `--target <triple>`, custom `build.target`, and profile-specific
/// directory naming (e.g. `target/<triple>/release/...`). The macro is
/// resolved at compile time of this bench file, which means cargo will
/// always rebuild the binary as a dependency of the bench target.
fn kb_mcp_binary() -> String {
    env!("CARGO_BIN_EXE_kb-mcp").to_string()
}

/// Path to a small test KB. Devs can override with their own KB via env.
fn fixture_kb_path() -> String {
    std::env::var("KBMCP_BENCH_KB").unwrap_or_else(|_| "tests/fixtures/kb-bench".into())
}

fn bench_search_mmr_off(c: &mut Criterion) {
    let bin = kb_mcp_binary();
    let kb = fixture_kb_path();
    c.bench_function("search / MMR off / parent off / reranker off", |b| {
        b.iter(|| {
            let out = Command::new(black_box(&bin))
                .args(["search", "--kb-path", &kb, "--limit", "10", "rust"])
                .output()
                .expect("kb-mcp search failed to spawn");
            assert!(
                out.status.success(),
                "kb-mcp search exit code: {:?}\nstderr: {}",
                out.status,
                String::from_utf8_lossy(&out.stderr)
            );
            black_box(out.status);
        });
    });
}

fn bench_search_mmr_on(c: &mut Criterion) {
    let bin = kb_mcp_binary();
    let kb = fixture_kb_path();
    c.bench_function("search / MMR on / parent off / reranker off", |b| {
        b.iter(|| {
            let out = Command::new(black_box(&bin))
                .args([
                    "search",
                    "--kb-path",
                    &kb,
                    "--mmr",
                    "true",
                    "--mmr-lambda",
                    "0.7",
                    "--limit",
                    "10",
                    "rust",
                ])
                .output()
                .expect("kb-mcp search failed to spawn");
            assert!(
                out.status.success(),
                "kb-mcp search exit code: {:?}\nstderr: {}",
                out.status,
                String::from_utf8_lossy(&out.stderr)
            );
            black_box(out.status);
        });
    });
}

#[cfg(feature = "heavy-bench")]
fn bench_search_with_reranker(c: &mut Criterion) {
    let bin = kb_mcp_binary();
    let kb = fixture_kb_path();
    c.bench_function("search / MMR on / reranker on (heavy)", |b| {
        b.iter(|| {
            let out = Command::new(black_box(&bin))
                .args([
                    "search",
                    "--kb-path",
                    &kb,
                    "--mmr",
                    "true",
                    "--reranker",
                    "bge-v2-m3",
                    "--limit",
                    "10",
                    "rust",
                ])
                .output()
                .expect("kb-mcp search failed to spawn");
            assert!(
                out.status.success(),
                "kb-mcp search exit code: {:?}\nstderr: {}",
                out.status,
                String::from_utf8_lossy(&out.stderr)
            );
            black_box(out.status);
        });
    });
}

#[cfg(not(feature = "heavy-bench"))]
criterion_group!(benches, bench_search_mmr_off, bench_search_mmr_on);

#[cfg(feature = "heavy-bench")]
criterion_group!(
    benches,
    bench_search_mmr_off,
    bench_search_mmr_on,
    bench_search_with_reranker
);

criterion_main!(benches);
