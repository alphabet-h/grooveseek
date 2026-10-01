//! feature-61: the size caps `[index]` sets reach the three read paths of the
//! indexer -- the full run's scan, the watcher's create / modify, and the
//! watcher's rename -- through the registry
//! [`grooveseek::config::Config::build_parser_registry`] builds.
//!
//! These call [`grooveseek::indexer::rebuild_index`],
//! [`grooveseek::indexer::reindex_single_file`] and
//! [`grooveseek::indexer::rename_single_file`] in-process against
//! [`crate::common::embed_mock`], each test body in a hermetic child of this
//! test binary for the reason `index_progress_callback.rs` gives. The helper
//! that starts the child is a copy of the one in `index_cancel.rs`: moving it
//! into `common` would mean editing the existing tests of both files.
//!
//! The caps are lowered below the fixture rather than raised above 50 MiB: a
//! registry that ignored `[index]` would admit the 1 KB fixture under the
//! default, so a skip is only possible if the configured value was read.

mod common;

use common::embed_mock::{EmbedMock, assert_dir_empty, hermetic, openai_config_toml};
use common::temp::{TempKbLayout, TempRoot};

use grooveseek::config::Config;
use grooveseek::db::{ContextMode, Database};
use grooveseek::embedder::Embedder;
use grooveseek::indexer::progress::{ProgressMode, ProgressReporter};
use grooveseek::indexer::{
    IndexResult, RenameOutcome, SingleResult, load_declared_schema, rebuild_index,
    reindex_single_file, rename_single_file,
};
use grooveseek::parser::Registry;

use std::path::{Path, PathBuf};
use std::process::Command;

/// The mock's vector length. Small, since nothing here ranks anything.
const DIM: usize = 8;

/// Set on the child [`run_in_hermetic_child`] starts, so the child runs the
/// test body instead of starting another child.
const HERMETIC_CHILD: &str = "GROOVE_F61_HERMETIC_CHILD";

/// A binary cap below `minimal.pdf` (1069 bytes) and far below the default.
const LOWERED: &str = "[index]\nmax_binary_file_size = 100\n";

/// Run the test named `name` again in a child of this test binary, under the
/// environment [`crate::common::embed_mock::hermetic`] pins. Returns `true` in
/// the parent, after asserting the child passed exactly one test, so the
/// caller returns; `false` in the child, which then runs the body.
fn run_in_hermetic_child(name: &str) -> bool {
    if std::env::var_os(HERMETIC_CHILD).is_some() {
        return false;
    }
    let cache = TempRoot::new("groove-f61-fastembed");
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

/// A knowledge base and the mock it embeds through. Each test writes its
/// `groove.toml` files beside the knowledge base, so the scan never counts them.
struct Fixture {
    layout: TempKbLayout,
    mock: EmbedMock,
}

impl Fixture {
    fn new(prefix: &str) -> Self {
        Self {
            mock: EmbedMock::start(DIM),
            layout: TempKbLayout::new(prefix),
        }
    }

    /// A config that enables PDF, embeds through the mock, and appends `index`.
    fn config(&self, name: &str, index: &str) -> Config {
        let path = self.layout.root().join(name);
        let toml = format!(
            "{}[parsers]\nenabled = [\"md\", \"pdf\"]\n{index}",
            openai_config_toml(&self.mock.endpoint(), None, DIM)
        );
        std::fs::write(&path, toml).expect("write groove.toml");
        Config::load_from(&path).expect("load groove.toml")
    }

    /// The knowledge base the way `rebuild_index` sees it: canonical.
    fn kb(&self) -> PathBuf {
        self.layout.kb().canonicalize().expect("canonical kb")
    }

    fn registry(&self, cfg: &Config) -> Registry {
        cfg.build_parser_registry(&self.kb())
            .expect("parser registry")
    }

    /// The index and the embedder, opened the way `groove index` opens them.
    fn open(&self, cfg: &Config) -> (Database, Embedder) {
        let embedding = cfg.resolve_embedding(None).expect("resolve [embedding]");
        let db_path = grooveseek::resolve_db_path(self.layout.kb());
        let db = Database::open(&db_path.to_string_lossy()).expect("open the index");
        // `groove index` does this before building the embedder, and it is
        // what creates the vector table on a fresh index.
        db.verify_embedding_meta(embedding.model_id(), embedding.dimension() as u32)
            .expect("embedding meta");
        let embedder = Embedder::with_settings(embedding).expect("build the embedder");
        (db, embedder)
    }

    /// One full run under `cfg`, with a quiet reporter.
    fn rebuild(&self, cfg: &Config) -> anyhow::Result<IndexResult> {
        let kb = self.layout.kb();
        let registry = cfg.build_parser_registry(kb)?;
        let schema = load_declared_schema(kb)?;
        let (db, mut embedder) = self.open(cfg);
        rebuild_index(
            &db,
            &mut embedder,
            kb,
            schema,
            false,
            cfg.exclude_headings.as_deref(),
            &cfg.resolve_exclude_dirs(),
            &registry,
            ProgressReporter::new(ProgressMode::Quiet),
            ContextMode::Off,
        )
    }

    /// Every `documents.path`, sorted.
    fn paths(&self) -> Vec<String> {
        let db_path = grooveseek::resolve_db_path(self.layout.kb());
        Database::open(&db_path.to_string_lossy())
            .expect("open the index")
            .all_document_paths()
            .expect("document paths")
    }

    /// `tests/fixtures/binary/minimal.pdf` (two pages, 1069 bytes) at `rel`.
    fn write_pdf(&self, rel: &str) {
        let fixture =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/binary/minimal.pdf");
        std::fs::copy(fixture, self.layout.kb().join(rel)).expect("copy minimal.pdf");
    }
}

/// AC4, full run: a binary cap below the fixture makes the scan decline it the
/// way the 50 MiB default declines a larger file; without the key the same
/// file is indexed.
#[test]
fn a_lowered_binary_cap_skips_a_file_in_the_full_run() {
    if run_in_hermetic_child("a_lowered_binary_cap_skips_a_file_in_the_full_run") {
        return;
    }
    let fx = Fixture::new("groove-f61-rebuild");
    fx.layout
        .write("a.md", "---\ntitle: A\n---\n\n## A\n\nalpha body text\n");
    fx.write_pdf("doc.pdf");

    let capped = fx.config("capped.toml", LOWERED);
    let result = fx.rebuild(&capped).expect("run");
    assert_eq!((result.updated, result.skipped), (1, 1), "{result:?}");
    assert_eq!(fx.paths(), vec!["a.md".to_string()]);

    let open = fx.config("open.toml", "");
    let result = fx.rebuild(&open).expect("run");
    assert_eq!(
        result.updated, 1,
        "only doc.pdf is new to the index: {result:?}"
    );
    assert_eq!(fx.paths(), vec!["a.md".to_string(), "doc.pdf".to_string()]);
}

/// AC4, watcher: `reindex_single_file` (create / modify) and
/// `rename_single_file` take their caps from the registry they are handed.
/// The rename is pinned on `RenameOutcome::RenamedSizeCapped`.
#[test]
fn the_watcher_and_rename_paths_read_the_cap_from_the_registry() {
    if run_in_hermetic_child("the_watcher_and_rename_paths_read_the_cap_from_the_registry") {
        return;
    }
    let fx = Fixture::new("groove-f61-watcher");
    let open = fx.config("open.toml", "");
    let capped = fx.config("capped.toml", LOWERED);
    let (open_registry, capped_registry) = (fx.registry(&open), fx.registry(&capped));
    let kb = fx.kb();
    let (db, mut embedder) = fx.open(&open);

    // Create / modify: turned away under the lowered cap, indexed without it.
    fx.write_pdf("new.pdf");
    let refused = reindex_single_file(&db, &mut embedder, &kb, "new.pdf", None, &capped_registry)
        .expect("reindex");
    assert_eq!(
        refused,
        SingleResult::Skipped {
            reason: "file too large",
            frontmatter_unparsed: false
        }
    );
    let indexed = reindex_single_file(&db, &mut embedder, &kb, "new.pdf", None, &open_registry)
        .expect("reindex");
    assert!(
        matches!(indexed, SingleResult::Updated { .. }),
        "{indexed:?}"
    );

    // Rename: the row moves, and the size check on the new name uses the
    // registry it was handed.
    std::fs::rename(kb.join("new.pdf"), kb.join("moved.pdf")).expect("rename on disk");
    let outcome = rename_single_file(
        &db,
        &mut embedder,
        &kb,
        "new.pdf",
        "moved.pdf",
        None,
        &capped_registry,
    )
    .expect("rename");
    assert_eq!(outcome, RenameOutcome::RenamedSizeCapped);

    // Control: under the default caps the same kind of move is a plain rename.
    std::fs::rename(kb.join("moved.pdf"), kb.join("back.pdf")).expect("rename on disk");
    let outcome = rename_single_file(
        &db,
        &mut embedder,
        &kb,
        "moved.pdf",
        "back.pdf",
        None,
        &open_registry,
    )
    .expect("rename");
    assert_eq!(outcome, RenameOutcome::Renamed);
}
