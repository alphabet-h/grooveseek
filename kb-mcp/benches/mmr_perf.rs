//! F-60 PR-1: MMR microbench.
//!
//! Drives `kb_mcp::mmr::mmr_select` directly (no SQL, no embedding,
//! pure function level). Observes F-42's greedy-loop bool-flag gain;
//! F-41's N+1 SQL elimination is *not* visible here because lookup
//! happens in the server pipeline, outside `mmr_select`.
//!
//! Run:
//!   cargo bench --bench mmr_perf
//!   # save baseline before f42 commit:
//!   cargo bench --bench mmr_perf -- --save-baseline pre-f42
//!   # compare after f42 commit:
//!   cargo bench --bench mmr_perf -- --baseline pre-f42

use criterion::{Criterion, black_box, criterion_group, criterion_main};
use kb_mcp::mmr::{MmrCandidate, mmr_select};

/// Deterministic synthetic candidate generator.
///
/// Uses a small linear congruential generator seeded by chunk_id so that
/// repeated bench runs produce bit-identical input (criterion's default
/// sample size of 100 then yields a stable median).
fn make_candidates(pool_size: usize) -> Vec<MmrCandidate> {
    let mut state: u64 = 0xdead_beef_1234_5678;
    let next = |s: &mut u64| {
        *s = s
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        *s
    };
    (0..pool_size)
        .map(|i| {
            let emb: Vec<f32> = (0..384)
                .map(|_| {
                    let v = next(&mut state) as u32 as f32;
                    (v / (u32::MAX as f32)) * 2.0 - 1.0
                })
                .collect();
            let rel = (next(&mut state) as u32 as f32) / (u32::MAX as f32);
            MmrCandidate {
                chunk_id: i as i64,
                document_id: (i / 3) as i64,
                embedding: emb,
                relevance_score: rel,
            }
        })
        .collect()
}

fn bench_mmr_select_pool50_limit10(c: &mut Criterion) {
    let cands = make_candidates(50);
    c.bench_function("mmr_select / pool=50 / limit=10 / penalty=0.0", |b| {
        b.iter(|| {
            let sel = mmr_select(black_box(&cands), 0.7, 0.0, 10);
            black_box(sel)
        });
    });
}

fn bench_mmr_select_pool100_limit50(c: &mut Criterion) {
    let cands = make_candidates(100);
    c.bench_function("mmr_select / pool=100 / limit=50 / penalty=0.0", |b| {
        b.iter(|| {
            let sel = mmr_select(black_box(&cands), 0.7, 0.0, 50);
            black_box(sel)
        });
    });
}

fn bench_mmr_select_pool500_limit50(c: &mut Criterion) {
    let cands = make_candidates(500);
    c.bench_function("mmr_select / pool=500 / limit=50 / penalty=0.0", |b| {
        b.iter(|| {
            let sel = mmr_select(black_box(&cands), 0.7, 0.0, 50);
            black_box(sel)
        });
    });
}

fn bench_mmr_select_pool500_limit50_penalty(c: &mut Criterion) {
    let cands = make_candidates(500);
    c.bench_function("mmr_select / pool=500 / limit=50 / penalty=0.5", |b| {
        b.iter(|| {
            let sel = mmr_select(black_box(&cands), 0.7, 0.5, 50);
            black_box(sel)
        });
    });
}

criterion_group!(
    benches,
    bench_mmr_select_pool50_limit10,
    bench_mmr_select_pool100_limit50,
    bench_mmr_select_pool500_limit50,
    bench_mmr_select_pool500_limit50_penalty
);
criterion_main!(benches);
