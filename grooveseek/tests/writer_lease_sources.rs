//! The commands that only read the index -- `groove status`, `groove search`,
//! `groove graph`, `groove doctor`, `groove eval` and `groove tune` -- open it
//! as a reader (ADR-0031): in the production lines of the binary's crate root
//! and of [`grooveseek::tune`] and [`grooveseek::eval`] there is no writer
//! open, only [`grooveseek::db::Database::open_read_only`]. `groove index`
//! opens its writer through [`grooveseek::db::Database::open_writer`] /
//! [`grooveseek::db::Database::open_or_replace_corrupt_with`], whose needles
//! below do not match.

use std::path::Path;

mod common;
use common::production::production_lines;

fn src(file: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("src").join(file);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

#[test]
fn readers_in_main_tune_and_eval_never_open_the_index_for_writing() {
    for (file, marker, min_reader_opens) in [
        ("main.rs", "fn main(", 4),
        ("tune.rs", "pub fn run(opts: &TuneOpts)", 1),
        ("eval.rs", "pub fn compute_query_metrics(", 1),
    ] {
        let text = src(file);
        let prod = production_lines(&text);
        assert!(
            prod.iter().any(|(_, l)| l.contains(marker)),
            "{file}: marker {marker} not in production lines"
        );
        assert!(
            prod.iter()
                .filter(|(_, l)| l.contains("Database::open_read_only("))
                .count()
                >= min_reader_opens,
            "{file}: fewer than {min_reader_opens} reader opens"
        );
        for needle in ["Database::open(", "Database::open_or_replace_corrupt("] {
            let hits: Vec<_> = prod.iter().filter(|(_, l)| l.contains(needle)).collect();
            assert!(hits.is_empty(), "{file}: {needle} in production: {hits:?}");
        }
    }
}
