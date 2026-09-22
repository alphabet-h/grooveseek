//! The callback path is reachable from *outside* the crate.
//!
//! [`grooveseek::indexer::progress`]'s own `mod tests` compiles inside the
//! crate, where every item is visible whether or not it says `pub`. The
//! application this path exists for (grooveseek-desktop) links the library
//! and names these three items by path, so the visibility is itself the thing
//! under test: each `tests/*.rs` is a separate crate, and this file stops
//! compiling if any of them is narrowed to `pub(crate)` or moved.
//!
//! No embedding model is loaded and no file is written, so this runs in the
//! plain `cargo test` tier rather than behind `#[ignore]`.

use grooveseek::indexer::progress::{ProgressCallback, ProgressEvent, ProgressReporter};
use std::sync::{Arc, Mutex};

#[test]
fn callback_path_is_nameable_and_usable_from_another_crate() {
    let log: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&log);
    let f: ProgressCallback = Box::new(move |ev| {
        let line = match ev {
            ProgressEvent::Started { total } => format!("started:{total}"),
            ProgressEvent::Indexed {
                rel,
                chunks,
                done,
                total,
            } => format!("indexed:{rel}:{chunks}:{done}/{total}"),
            ProgressEvent::Unchanged { rel, done, total } => {
                format!("unchanged:{rel}:{done}/{total}")
            }
            ProgressEvent::Renamed { old, new } => format!("renamed:{old}->{new}"),
            ProgressEvent::Deleted { rel } => format!("deleted:{rel}"),
            ProgressEvent::Finished => "finished".to_string(),
        };
        sink.lock().expect("recorder mutex").push(line);
    });

    let mut reporter = ProgressReporter::with_callback(f);
    reporter.start_indexing(2);
    reporter.report_indexed("a.md", 3);
    reporter.report_unchanged("b.md");
    reporter.finish();

    let got = log.lock().expect("recorder mutex").clone();
    assert_eq!(
        got,
        vec![
            "started:2",
            "indexed:a.md:3:1/2",
            "unchanged:b.md:2/2",
            "finished",
        ]
    );
}
