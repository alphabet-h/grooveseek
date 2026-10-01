//! feature-61: the size caps `[index]` sets reach the three read paths of the
//! indexer -- the full run's scan, the watcher's create / modify, and the
//! watcher's rename -- through the registry
//! [`grooveseek::config::Config::build_parser_registry`] builds.
//!
//! These call [`grooveseek::indexer::rebuild_index`],
//! [`grooveseek::indexer::reindex_single_file`] and
//! [`grooveseek::indexer::rename_single_file`] in-process against
//! [`crate::common::embed_mock`], each test body in a hermetic child of this
//! test binary for the reason the progress-callback tests give. The helper
//! that starts the child is a copy of the one in the cancellation tests:
//! moving it into the shared test helpers would mean editing the existing
//! tests of both.
//!
//! Most of the caps here are lowered below the fixture rather than raised
//! above 50 MiB: a registry that ignored `[index]` would admit the 1 KB
//! fixture under the default, so a skip is only possible if the configured
//! value was read. A lowered cap is caught before the file is opened, though,
//! so the check made on the handle the bytes are read from is pinned the other
//! way round: a raised cap, and a sparse file one byte past the default.

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

    /// A config that enables PDF, embeds through the mock, and ends with the
    /// `[index]` text it is handed.
    fn config(&self, name: &str, index: &str) -> Config {
        let path = self.layout.root().join(name);
        let toml = format!(
            "{}[parsers]\nenabled = [\"md\", \"pdf\"]\n{index}",
            openai_config_toml(&self.mock.endpoint(), None, DIM)
        );
        std::fs::write(&path, toml).expect("write groove.toml");
        Config::load_from(&path).expect("load groove.toml")
    }

    /// The knowledge base the way [`grooveseek::indexer::rebuild_index`] sees
    /// it: canonical.
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

/// AC4, watcher: [`grooveseek::indexer::reindex_single_file`] (create /
/// modify) and [`grooveseek::indexer::rename_single_file`] take their caps
/// from the registry they are handed. The rename is pinned on
/// [`grooveseek::indexer::RenameOutcome::RenamedSizeCapped`].
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

/// A binary cap above the default, with room for [`PAST_THE_DEFAULT`].
const RAISED: &str = "[index]\nmax_binary_file_size = \"100 MiB\"\n";

/// One byte past the 50 MiB default,
/// [`grooveseek::parser::MAX_RAW_BINARY_BYTES`].
const PAST_THE_DEFAULT: u64 = grooveseek::parser::MAX_RAW_BINARY_BYTES + 1;

/// Extend (or create) `path` to `len` bytes without writing them.
fn grow_to(path: &Path, len: u64) {
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)
        .expect("open the file to grow");
    file.set_len(len).expect("grow the file");
}

/// [`run_in_hermetic_child`], for a test whose assertion is on what the child
/// wrote to stderr: `Some(stderr)` in the parent, after the same checks that
/// the child passed exactly one test, and `None` in the child, which then runs
/// the body. A copy rather than a change to that helper, which the existing
/// tests call.
fn stderr_of_hermetic_child(name: &str) -> Option<String> {
    if std::env::var_os(HERMETIC_CHILD).is_some() {
        return None;
    }
    let cache = TempRoot::new("groove-f61-fastembed");
    let mut cmd = Command::new(std::env::current_exe().expect("this test binary"));
    cmd.args([name, "--exact", "--nocapture", "--test-threads=1"])
        .env(HERMETIC_CHILD, "1");
    hermetic(&mut cmd, cache.path());
    let out = cmd.output().expect("run the test in a child");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(
        out.status.success(),
        "{name} failed in the child:\n{stdout}\n{stderr}"
    );
    assert!(
        stdout.contains("test result: ok. 1 passed"),
        "the child ran no test named {name}:\n{stdout}\n{stderr}"
    );
    assert_dir_empty(cache.path());
    Some(stderr)
}

/// AC4 on the handle, watcher side. A lowered cap is caught by the stat check
/// before the file is opened, so the tests above never reach the second check,
/// the one made on the handle the bytes are read from. Under a raised cap the
/// stat check passes and that read is the only check left: a file one byte past
/// the 50 MiB default has to get through it on create / modify and on rename.
/// Whether the zeros then parse as a PDF does not matter here; being refused
/// for their size does.
#[test]
fn a_raised_binary_cap_admits_a_file_past_the_default_on_the_watcher_paths() {
    if run_in_hermetic_child(
        "a_raised_binary_cap_admits_a_file_past_the_default_on_the_watcher_paths",
    ) {
        return;
    }
    let fx = Fixture::new("groove-f61-raised-watcher");
    let raised = fx.config("raised.toml", RAISED);
    let registry = fx.registry(&raised);
    let kb = fx.kb();
    let (db, mut embedder) = fx.open(&raised);

    // Create / modify: both reads (the hash read and the one that parses)
    // are made under the registry's cap.
    grow_to(&kb.join("big.pdf"), PAST_THE_DEFAULT);
    let result =
        reindex_single_file(&db, &mut embedder, &kb, "big.pdf", None, &registry).expect("reindex");
    assert!(
        !matches!(
            result,
            SingleResult::Refused
                | SingleResult::Skipped {
                    reason: "file too large",
                    ..
                }
        ),
        "a file under the raised cap was turned away for its size: {result:?}"
    );

    // Rename: a row to move, then the file grows past the default under its
    // new name, so the rename has to read it to see the content changed.
    fx.write_pdf("small.pdf");
    let indexed = reindex_single_file(&db, &mut embedder, &kb, "small.pdf", None, &registry)
        .expect("reindex");
    assert!(
        matches!(indexed, SingleResult::Updated { .. }),
        "{indexed:?}"
    );
    std::fs::rename(kb.join("small.pdf"), kb.join("grown.pdf")).expect("rename on disk");
    grow_to(&kb.join("grown.pdf"), PAST_THE_DEFAULT);
    let outcome = rename_single_file(
        &db,
        &mut embedder,
        &kb,
        "small.pdf",
        "grown.pdf",
        None,
        &registry,
    )
    .expect("rename");
    assert!(
        !matches!(
            outcome,
            RenameOutcome::RenamedSizeCapped
                | RenameOutcome::RenamedSizeCappedAndDropped
                | RenameOutcome::RenamedButRefused
                | RenameOutcome::RenamedButRefusedAndDropped
        ),
        "a file under the raised cap was turned away for its size: {outcome:?}"
    );
}

/// AC4 on the handle, full-run side: the scan reads the file it hashes off a
/// handle too, and so does the parse after it. A refusal at either is a skip
/// like a parse failure is, so the counts cannot tell them apart; what tells
/// them apart is the line on stderr, which names a size only for a refusal.
#[test]
fn a_raised_binary_cap_admits_a_file_past_the_default_in_the_full_run() {
    const NAME: &str = "a_raised_binary_cap_admits_a_file_past_the_default_in_the_full_run";
    if let Some(stderr) = stderr_of_hermetic_child(NAME) {
        let sized: Vec<&str> = stderr
            .lines()
            .filter(|l| l.contains("big.pdf"))
            .filter(|l| l.contains("too large") || l.contains("byte limit"))
            .collect();
        assert!(
            sized.is_empty(),
            "a file under the raised cap was turned away for its size: {sized:?}\n{stderr}"
        );
        return;
    }
    let fx = Fixture::new("groove-f61-raised-rebuild");
    grow_to(&fx.layout.kb().join("big.pdf"), PAST_THE_DEFAULT);
    let raised = fx.config("raised.toml", RAISED);
    let result = fx.rebuild(&raised).expect("run");
    assert_eq!(
        result.updated + result.skipped,
        1,
        "the scan saw big.pdf and nothing else: {result:?}"
    );
}

/// A size skip names the key that would admit the file, the way the
/// decompression-side messages already do, so the operator is not left to
/// guess which of the three caps applied.
#[test]
fn a_size_skip_names_the_key_that_admits_the_file() {
    const NAME: &str = "a_size_skip_names_the_key_that_admits_the_file";
    if let Some(stderr) = stderr_of_hermetic_child(NAME) {
        let skip = stderr
            .lines()
            .find(|l| l.contains("Skipping doc.pdf") && l.contains("file too large"))
            .unwrap_or_else(|| panic!("no size skip for doc.pdf on stderr:\n{stderr}"));
        assert!(
            skip.ends_with("; raise [index].max_binary_file_size to admit it"),
            "{skip}"
        );
        return;
    }
    let fx = Fixture::new("groove-f61-skip-hint");
    fx.write_pdf("doc.pdf");
    let capped = fx.config("capped.toml", LOWERED);
    let result = fx.rebuild(&capped).expect("run");
    assert_eq!(result.skipped, 1, "{result:?}");
}
