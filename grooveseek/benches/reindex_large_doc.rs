//! (AX-19) `reindex_one_section_of_large_doc`: an incremental `rebuild_index` after one section
//! of a large Markdown document changed.
//!
//! feature-69 made this case cheap by copying the stored vector of every chunk whose embed
//! input did not change, so only the edited section goes to the embedder. What makes it cheap
//! is a count, not a time: a regression that re-embeds the whole document is a few hundred
//! more embeddings against a real model, while against the stub used here it is only a little
//! slower. So the run's [`IndexResult::chunks_embedded`] and [`IndexResult::chunks_reused`]
//! are checked before the timing starts and printed as a `bench-count:` line, which the
//! nightly job lifts into its summary next to the time.
//!
//! The embedder is [`Embedder::stub_for_tests`] (the `test-helpers` feature, hence
//! `required-features` on this target): no model download and no endpoint, so the time is the
//! indexer's own -- the walk, the parse, the reuse plan and the write -- and not a network's.
//!
//! [`IndexResult::chunks_embedded`]: grooveseek::indexer::IndexResult::chunks_embedded
//! [`IndexResult::chunks_reused`]: grooveseek::indexer::IndexResult::chunks_reused

#[path = "../tests/common/temp.rs"]
#[allow(dead_code, unused_imports)]
mod temp;

use criterion::{Criterion, criterion_group, criterion_main};
use grooveseek::db::{ContextMode, Database};
use grooveseek::embedder::Embedder;
use grooveseek::indexer::progress::{ProgressMode, ProgressReporter};
use grooveseek::indexer::{self, IndexResult};
use grooveseek::parser::Registry;
use std::time::{Duration, Instant};
use temp::TempKbLayout;

/// Sections (`##` headings) in the document; each is one chunk.
const SECTIONS: usize = 500;
/// The section whose text changes between runs.
const EDITED: usize = SECTIONS / 2;
/// The stub's vector length: BGE-small's, so the stored vectors are the size a real index has.
const DIMENSION: usize = 384;
const DOC: &str = "large.md";

/// The document with section [`EDITED`] carrying `variant`.
fn document(variant: u64) -> String {
    let mut s = String::from("---\ntitle: Large handbook\n---\n\n# Large handbook\n\n");
    for i in 0..SECTIONS {
        s.push_str(&format!("## Section {i}\n\n"));
        if i == EDITED {
            s.push_str(&format!(
                "This section was revised (revision {variant}). It describes how the edited \
                 procedure is carried out and which checks follow it.\n\n"
            ));
        } else {
            s.push_str(&format!(
                "Section {i} describes procedure number {i}: who carries it out, which inputs \
                 it needs, and which checks follow it before the work is handed over.\n\n"
            ));
        }
    }
    s
}

fn run(db: &Database, embedder: &mut Embedder, kb: &TempKbLayout, reg: &Registry) -> IndexResult {
    indexer::rebuild_index(
        db,
        embedder,
        kb.kb(),
        None,
        false,
        None,
        &[],
        reg,
        ProgressReporter::new(ProgressMode::Quiet),
        ContextMode::Off,
    )
    .expect("rebuild_index")
}

fn bench_reindex_one_section(c: &mut Criterion) {
    let kb = TempKbLayout::new("groove-bench-reindex-large");
    kb.write(DOC, &document(0));
    let db_path = kb.root().join(".groove.db");
    let db = Database::open(&db_path.to_string_lossy()).expect("open the index");
    let mut embedder = Embedder::stub_for_tests(DIMENSION);
    db.verify_embedding_meta(embedder.model_id(), DIMENSION as u32)
        .expect("embedding meta");
    let registry = Registry::defaults();

    let first = run(&db, &mut embedder, &kb, &registry);
    let chunks = first.chunks_embedded;
    assert!(
        chunks as usize >= SECTIONS,
        "the first run embeds every section ({chunks} chunks for {SECTIONS} sections)"
    );

    // The case this bench exists for, checked once before timing: one section changed, one
    // chunk embedded, every other chunk's vector reused.
    let mut variant = 1;
    kb.write(DOC, &document(variant));
    let before = embedder.texts_embedded();
    let probe = run(&db, &mut embedder, &kb, &registry);
    let sent = embedder.texts_embedded() - before;
    assert_eq!(probe.updated, 1, "the edited document is updated");
    assert_eq!(
        (probe.chunks_embedded, probe.chunks_reused),
        (1, chunks - 1),
        "one edited section embeds one chunk and reuses the rest"
    );
    assert_eq!(sent, 1, "the embedder was handed one input");
    println!(
        "bench-count: reindex_one_section_of_large_doc: sections={SECTIONS} chunks={chunks} \
         chunks_embedded={} chunks_reused={} texts_sent_to_embedder={sent}",
        probe.chunks_embedded, probe.chunks_reused
    );

    c.bench_function("reindex_one_section_of_large_doc", |b| {
        b.iter_custom(|iters| {
            let mut total = Duration::ZERO;
            for _ in 0..iters {
                // Writing the file is setup, not the reindex: it stays outside the clock.
                variant += 1;
                kb.write(DOC, &document(variant));
                let start = Instant::now();
                let result = run(&db, &mut embedder, &kb, &registry);
                total += start.elapsed();
                assert_eq!(result.chunks_embedded, 1, "one chunk embedded per edit");
            }
            total
        });
    });
}

criterion_group!(benches, bench_reindex_one_section);
criterion_main!(benches);
