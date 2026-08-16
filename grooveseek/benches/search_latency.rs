//! F-60 PR-1: search latency subprocess bench.
//!
//! Spawns `groove search ...` as a subprocess and measures wall-clock.
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

/// Path to the groove binary built by cargo. Cargo provides this as an
/// env var when building integration tests / benchmarks, so it is robust
/// to `--target <triple>`, custom `build.target`, and profile-specific
/// directory naming (e.g. `target/<triple>/release/...`). The macro is
/// resolved at compile time of this bench file, which means cargo will
/// always rebuild the binary as a dependency of the bench target.
fn grooveseek_binary() -> String {
    env!("CARGO_BIN_EXE_groove").to_string()
}

/// Path to a small test KB. Devs can override with their own KB via env.
fn fixture_kb_path() -> String {
    std::env::var("GROOVE_BENCH_KB").unwrap_or_else(|_| "tests/fixtures/kb-bench".into())
}

/// (AU-56) Make sure the KB is indexed, and prove it, before timing anything.
///
/// The fixture's `.groove.db` is gitignored — it is a build artefact, not
/// checked-in data — so on a fresh clone every `search` here ran against an
/// index that did not exist. That returns zero hits and exits 0, so the
/// `status.success()` assertion in each bench passed and criterion happily
/// reported numbers: the cost of starting the binary and finding nothing.
///
/// Indexing is idempotent and the fixture is three small Markdown files, so
/// doing it unconditionally costs a moment on the first run and nothing after.
/// The search that follows is the part that matters: a bench measuring an empty
/// result set is worse than no bench, because it still produces a graph.
fn ensure_indexed(bin: &str, kb: &str) {
    eprintln!("bench setup: indexing {kb} (idempotent)");
    let out = Command::new(bin)
        .args(["index", "--kb-path", kb])
        .output()
        .expect("groove index failed to spawn");
    assert!(
        out.status.success(),
        "groove index failed for the bench fixture: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let out = Command::new(bin)
        .args(["search", "--kb-path", kb, "--limit", "10", "rust"])
        .output()
        .expect("groove search failed to spawn");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let hits = serde_json::from_str::<serde_json::Value>(&stdout)
        .ok()
        .and_then(|v| v.get("results").and_then(|r| r.as_array()).map(Vec::len))
        .unwrap_or(0);
    assert!(
        hits > 0,
        "the bench query returned {hits} results — the numbers below would be \
         the cost of starting the binary and finding nothing.\nstdout: {stdout}"
    );
    eprintln!("bench setup: {hits} hits, the fixture is searchable");
}

fn bench_search_mmr_off(c: &mut Criterion) {
    let bin = grooveseek_binary();
    let kb = fixture_kb_path();
    ensure_indexed(&bin, &kb);
    c.bench_function("search / MMR off / parent off / reranker off", |b| {
        b.iter(|| {
            let out = Command::new(black_box(&bin))
                .args(["search", "--kb-path", &kb, "--limit", "10", "rust"])
                .output()
                .expect("groove search failed to spawn");
            assert!(
                out.status.success(),
                "groove search exit code: {:?}\nstderr: {}",
                out.status,
                String::from_utf8_lossy(&out.stderr)
            );
            black_box(out.status);
        });
    });
}

fn bench_search_mmr_on(c: &mut Criterion) {
    let bin = grooveseek_binary();
    let kb = fixture_kb_path();
    ensure_indexed(&bin, &kb);
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
                .expect("groove search failed to spawn");
            assert!(
                out.status.success(),
                "groove search exit code: {:?}\nstderr: {}",
                out.status,
                String::from_utf8_lossy(&out.stderr)
            );
            black_box(out.status);
        });
    });
}

#[cfg(feature = "heavy-bench")]
fn bench_search_with_reranker(c: &mut Criterion) {
    let bin = grooveseek_binary();
    let kb = fixture_kb_path();
    ensure_indexed(&bin, &kb);
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
                .expect("groove search failed to spawn");
            assert!(
                out.status.success(),
                "groove search exit code: {:?}\nstderr: {}",
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
