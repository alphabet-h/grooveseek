//! Running a test body in a hermetic child of its own test binary, and opening the index and the
//! embedder in-process the way `groove index` opens them.
//!
//! The in-process calls ([`grooveseek::indexer::rename_single_file`] and its neighbours) build an
//! [`grooveseek::embedder::Embedder`] from the process environment, so a test that makes them reruns itself in a child
//! under [`crate::common::embed_mock::hermetic`]. Several older test crates each carry a private
//! copy of these two helpers; they stay as they are because existing tests are not edited, and a
//! new test uses this module instead of adding another copy.

use super::embed_cli::Fixture;
use super::embed_mock::{assert_dir_empty, hermetic};
use super::temp::TempRoot;

use grooveseek::config::Config;
use grooveseek::db::Database;
use grooveseek::embedder::Embedder;

use std::process::Command;

/// Set on the child [`run_in_hermetic_child`] starts, so the child runs the test body.
const HERMETIC_CHILD: &str = "GROOVE_IN_PROCESS_HERMETIC_CHILD";

/// Run the named test again in a child of this test binary under the environment
/// [`crate::common::embed_mock::hermetic`] pins. `true` in the parent, after the child passed
/// exactly one test; `false` in the child, which then runs the body.
pub fn run_in_hermetic_child(name: &str) -> bool {
    if std::env::var_os(HERMETIC_CHILD).is_some() {
        return false;
    }
    let cache = TempRoot::new("groove-in-process-fastembed");
    let mut cmd = Command::new(std::env::current_exe().expect("this test binary"));
    cmd.args([name, "--exact", "--nocapture", "--test-threads=1"])
        .env(HERMETIC_CHILD, "1");
    hermetic(&mut cmd, cache.path());
    let out = cmd.output().expect("run the test in a child");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "{name} failed in the child:\n{stdout}\n{stderr}"
    );
    assert!(
        stdout.contains("test result: ok. 1 passed"),
        "the child ran no test named {name}:\n{stdout}\n{stderr}"
    );
    assert_dir_empty(cache.path());
    true
}

/// The index and the embedder under `cfg`, opened the way `groove index` opens them.
pub fn open_in_process(fx: &Fixture, cfg: &Config) -> (Database, Embedder) {
    let embedding = cfg.resolve_embedding(None).expect("resolve [embedding]");
    let db_path = grooveseek::resolve_db_path(fx.kb());
    let db = Database::open(&db_path.to_string_lossy()).expect("open the index");
    db.verify_embedding_meta(embedding.model_id(), embedding.dimension() as u32)
        .expect("embedding meta");
    let embedder = Embedder::with_settings(embedding).expect("build the embedder");
    (db, embedder)
}
