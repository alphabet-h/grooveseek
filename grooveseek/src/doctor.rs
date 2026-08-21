//! `groove doctor` — ask the index whether it is in the state it should be.
//!
//! Three groups of question, and one deliberate omission.
//!
//! **Integrity.** Search reads three tables that have to agree about a chunk:
//! `chunks` holds the text, `vec_chunks` the embedding, `fts_chunks` the
//! full-text row. When they stop agreeing nothing errors — a chunk missing its
//! embedding is simply never a vector hit, and one missing its FTS row is never
//! a keyword hit. `backfill_fts` exists because that has happened, and until
//! now the only way to find out was to run a full index and watch it repair
//! things.
//!
//! **Servability.** Which indexed documents the resource surface is holding
//! back, and why. This is *not* a second implementation of that rule: the
//! extension check is [`crate::indexer::paths_with_unregistered_extension`] and
//! the size check is [`crate::server::ServableRules`], the same values the
//! server answers `resources/list` from. A doctor that computed its own
//! equivalent would eventually disagree with the thing it is reporting on,
//! which is the failure mode this whole feature is about.
//!
//! **Files that stopped being read.** ADR-0007 renamed everything this project
//! writes to disk and chose, deliberately, to ship no aliases and no automatic
//! migration: a `groove` binary does not see what `kb-mcp` left behind. That is
//! the right decision and this is its cost — the old file stays where it was,
//! is never opened, and nothing says so. `.kb-mcpignore` is the one that hurts,
//! because the consequence of not reading it is that **whatever it excluded and
//! the current rules do not is in the index**, silently and with no error
//! anywhere. It stops there rather than naming what leaked: that would need
//! this to read the file and re-run the exclusion decision, which is the one
//! thing the paragraph above rules out. This is the only group that looks at
//! the filesystem rather than at the database.
//!
//! **It does not repair.** Every finding names the command that fixes it. That
//! is the contract `paths_with_unregistered_extension` already states for the
//! narrower case — report the count, suggest `groove index`, never delete —
//! and a diagnostic that mutates on your behalf is a different, larger promise.
//!
//! One thing that *is* surprising and is stated wherever it can be: opening a
//! database runs the forward migrations (see `db/schema.rs`), so `doctor` is
//! read-only about its findings but not about the file. `eval` and `search`
//! have the same property.

use crate::db::{Database, IntegrityScan};
use crate::parser::Registry;
use anyhow::Result;
use serde::Serialize;
use std::path::{Path, PathBuf};

/// How many offending paths a finding carries. Enough to recognise the shape
/// of the problem, few enough that a broken index does not print a novel.
const SAMPLE_LIMIT: usize = 5;

/// Whether a finding means the index is wrong, or merely that it is not what
/// the current configuration would produce.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    /// The index disagrees with itself. Something is being silently lost.
    Error,
    /// The index is consistent, but some documents are not fully usable.
    Warning,
}

impl Severity {
    pub fn as_str(self) -> &'static str {
        match self {
            Severity::Error => "error",
            Severity::Warning => "warning",
        }
    }
}

/// One answered question, in the form a report prints.
#[derive(Debug, Clone, Serialize)]
pub struct Finding {
    /// Stable identifier, safe to grep for in CI.
    pub check: &'static str,
    pub severity: Severity,
    /// What is wrong, in one sentence.
    pub summary: String,
    pub count: u64,
    pub samples: Vec<String>,
    /// What to run, or what to change, to make it go away.
    pub remedy: &'static str,
}

/// The whole answer. `findings` is empty when there is nothing to say.
#[derive(Debug, Clone, Serialize)]
pub struct Report {
    pub documents: u32,
    pub chunks: u32,
    pub findings: Vec<Finding>,
}

impl Report {
    pub fn is_clean(&self) -> bool {
        self.findings.is_empty()
    }
}

fn finding(
    check: &'static str,
    severity: Severity,
    summary: String,
    scan: IntegrityScan,
    remedy: &'static str,
) -> Option<Finding> {
    if scan.is_clean() {
        return None;
    }
    Some(Finding {
        check,
        severity,
        summary,
        count: scan.count,
        samples: scan.samples,
        remedy,
    })
}

/// Run every check against `db` and collect what it found.
///
/// Findings come out in the order below — integrity before servability —
/// because the first group means something is broken and the second means
/// something is merely unavailable.
pub fn run(db: &Database, registry: &Registry, kb_path: &Path) -> Result<Report> {
    let mut findings = Vec::new();

    // First, because it is the only group that can explain why the index holds
    // documents you thought you had excluded — every question below takes the
    // corpus as given and asks whether the index agrees with itself about it.
    findings.extend(files_that_stopped_being_read(kb_path));

    // Before the per-chunk comparisons, because those cannot see it: with the
    // table gone there is nothing to scan, so every one of them answers clean
    // while vector search returns nothing at all.
    if let Some(chunks) = db.vector_table_missing_with_chunks()? {
        findings.push(Finding {
            check: "vector-table-missing",
            severity: Severity::Error,
            summary: format!(
                "the vector table is gone while {chunks} chunk(s) remain, so vector search \
                 cannot return anything"
            ),
            count: u64::from(chunks),
            samples: Vec::new(),
            remedy: "groove index --force",
        });
    }

    let scan = db.chunks_without_embedding(SAMPLE_LIMIT)?;
    findings.extend(finding(
        "missing-embedding",
        Severity::Error,
        format!(
            "{} chunk(s) have no embedding, so vector search cannot return them",
            scan.count
        ),
        scan,
        "groove index --force",
    ));

    let scan = db.embeddings_without_chunk(SAMPLE_LIMIT)?;
    findings.extend(finding(
        "orphan-embedding",
        Severity::Error,
        format!(
            "{} embedding(s) point at a chunk that no longer exists",
            scan.count
        ),
        scan,
        "groove index --force",
    ));

    let scan = db.chunks_without_fts(SAMPLE_LIMIT)?;
    findings.extend(finding(
        "missing-fts-row",
        Severity::Error,
        format!(
            "{} chunk(s) are absent from the full-text index, so keyword search cannot return them",
            scan.count
        ),
        scan,
        "groove index (the next run backfills them)",
    ));

    let scan = db.fts_without_chunk(SAMPLE_LIMIT)?;
    findings.extend(finding(
        "orphan-fts-row",
        Severity::Error,
        format!(
            "{} full-text row(s) point at a chunk that no longer exists",
            scan.count
        ),
        scan,
        "groove index --force",
    ));

    let scan = db.chunks_without_document(SAMPLE_LIMIT)?;
    findings.extend(finding(
        "chunk-without-document",
        Severity::Error,
        format!(
            "{} chunk(s) belong to a document that no longer exists, so every search drops them \
             at the join",
            scan.count
        ),
        scan,
        "groove index --force",
    ));

    let scan = db.documents_without_chunks(SAMPLE_LIMIT)?;
    findings.extend(finding(
        "document-without-chunks",
        Severity::Error,
        format!(
            "{} document(s) have no chunks at all, so no search can reach them",
            scan.count
        ),
        scan,
        "groove index --force",
    ));

    // -- servability: the same values the resource surface answers from ------

    let all_paths = db.all_document_paths()?;
    let stale = crate::indexer::paths_with_unregistered_extension(&all_paths, registry);
    findings.extend(finding(
        "extension-not-registered",
        Severity::Warning,
        format!(
            "{} indexed document(s) have an extension the current [parsers].enabled cannot open; \
             they stay searchable but are not offered as resources",
            stale.len()
        ),
        truncated(stale),
        "restore the extension in [parsers].enabled, or run groove index to drop the rows",
    ));

    let rules = crate::server::ServableRules::new(
        registry,
        db.documents_larger_than(crate::server::GET_DOCUMENT_MAX_BYTES)?,
    );
    let oversized = rules.oversized_paths();
    findings.extend(finding(
        "larger-than-a-read-returns",
        Severity::Warning,
        format!(
            "{} indexed document(s) are larger than a resource read returns; \
             they stay searchable but carry no uri",
            oversized.len()
        ),
        truncated(oversized),
        // Not "use get_document instead": it applies the same per-extension cap
        // through the same `max_bytes_for`, so it refuses the identical file.
        // Naming it would send someone to a remedy that cannot work
        // (codex P2 round 1).
        "split the document into parts under the read cap",
    ));

    let unrecorded = db.documents_without_recorded_size()?;
    if unrecorded > 0 {
        findings.push(Finding {
            check: "size-not-recorded",
            severity: Severity::Warning,
            summary: format!(
                "{unrecorded} document(s) were indexed before sizes were recorded, so whether a \
                 read can return them is not known yet"
            ),
            count: u64::from(unrecorded),
            samples: Vec::new(),
            remedy: "groove index (one run fills them in, without re-embedding)",
        });
    }

    Ok(Report {
        documents: db.document_count()?,
        chunks: db.chunk_count()?,
        findings,
    })
}

/// Where a file sits, relative to the knowledge base.
#[derive(Debug, Clone, Copy)]
enum Location {
    /// Inside it, where `.grooveignore` and the eval files are read from.
    KbRoot,
    /// Beside it, where the index goes.
    BesideIndex,
}

/// One file ADR-0007 renamed, and what it costs to leave the old one there.
///
/// **Two of each, because the replacement may be there too.** A knowledge base
/// with both `.kb-mcpignore` and a live `.grooveignore` is not leaking anything
/// the new file also excludes, so the blunt claim would be wrong — and
/// "rename it to `.grooveignore`" would tell the reader to write over the
/// configuration they are actually using (codex P2 on PR #203).
#[derive(Debug, Clone, Copy)]
struct Legacy {
    check: &'static str,
    old: &'static str,
    new: &'static str,
    location: Location,
    /// Completes "`<old>` …" when the replacement is **not** beside it.
    consequence: &'static str,
    remedy: &'static str,
    /// The same two for when it is.
    consequence_beside_replacement: &'static str,
    remedy_beside_replacement: &'static str,
}

/// The renamed files whose location **nothing can move**.
///
/// This is a subset of the file half of the 0.26.0 migration table in
/// `CHANGELOG.md`, and the line is drawn where this check stops being able to
/// tell "left behind" from "in use".
///
/// `.grooveignore` is read from `kb_path.join(IGNORE_FILE_NAME)` with the name
/// a constant, and the index is at `resolve_db_path(kb_path)`. Neither takes a
/// configured path, so a file sitting at the *old* name beside them is left
/// behind and nothing else.
///
/// The rest of that table is out for one reason or the other:
///
/// - **`kb-mcp.toml`.** `groove.toml` is *discovered* — from the working
///   directory, a `.git` ancestor, beside the binary, or the home directory —
///   so there is no single place the old name would be. A configuration that
///   silently did not load also announces itself at once, by indexing the
///   wrong directory or none.
/// - **`.kb-mcp-eval.yml` and `.kb-mcp-eval-history.json`.** `[eval].golden`
///   can point at any path, **including the old name**, in which case
///   `groove eval` reads it and "rename it" would break the configured path
///   (codex P2 on PR #203). Deciding otherwise would mean resolving the eval
///   configuration here — a second implementation of a question `eval` already
///   answers, which is the failure this whole module is written to avoid. The
///   cost of leaving them out is small: a stale golden makes `groove eval`
///   say there is no golden file, which is already a clear message.
/// - **Environment variables and command names.** Nothing on disk near a
///   knowledge base reveals them.
const RENAMED: &[Legacy] = &[
    Legacy {
        check: "legacy-ignore-file",
        old: ".kb-mcpignore",
        new: ".grooveignore",
        location: Location::KbRoot,
        // Not "every path it excluded is being indexed". `exclude_dirs` and
        // the hardcoded denylist may already cover what it named — a
        // `.kb-mcpignore` holding only `node_modules/` costs nothing — and
        // this check reads neither the file nor the rules. Claiming the leak
        // would be a second implementation of the exclusion decision, which
        // the module doc rules out (codex P2 on PR #203).
        consequence: "is not read, so anything it excluded that the current rules do not is \
                      indexed now",
        // A plain run is enough and `--force` would be wrong: documents that
        // are neither visited nor skipped are pruned, and a file the walk no
        // longer reaches is exactly that. `--force` would re-embed the whole
        // corpus to reach the same state.
        remedy: "rename it to .grooveignore, then run `groove index`",
        // Only that the name is taken. Not "so `.grooveignore` decides what is
        // excluded now" — a `.grooveignore` that is a directory, a hard link,
        // or over the cap is **refused** by `ExclusionRules::load` and applies
        // no patterns at all, and `symlink_metadata` cannot tell that apart
        // from a working one (codex P2 on PR #203). Neither file is opened
        // here, so neither is described.
        consequence_beside_replacement: "is not read, and the name .grooveignore beside it is taken; this check opened neither",
        // Starts with a look, for the same reason: if `.grooveignore` turns
        // out to be a directory or something unreadable, "merge into it" is
        // not an instruction anyone can follow.
        remedy_beside_replacement: "read what .grooveignore holds, merge any lines you still want, \
             then delete this one and run `groove index`",
    },
    Legacy {
        check: "legacy-index-file",
        old: ".kb-mcp.db",
        new: ".groove.db",
        location: Location::BesideIndex,
        // `doctor` only runs when there is an index, so the replacement is
        // there in every case that reaches this: one message covers both.
        consequence: "is an index this binary never opens; the one in use is beside it",
        remedy: "delete it once `groove status` reports the documents you expect",
        consequence_beside_replacement: "is an index this binary never opens; the one in use is beside it",
        remedy_beside_replacement: "delete it once `groove status` reports the documents you expect",
    },
];

impl Legacy {
    fn path(&self, kb_path: &Path) -> PathBuf {
        match self.location {
            Location::KbRoot => kb_path.join(self.old),
            // Derived from `resolve_db_path` rather than restated, so "beside
            // the knowledge base" cannot drift from where the index goes.
            Location::BesideIndex => crate::resolve_db_path(kb_path).with_file_name(self.old),
        }
    }

    /// Where the file that replaced this one would be — the same directory,
    /// by construction, which is what makes "beside it" mean anything.
    fn replacement_path(&self, kb_path: &Path) -> PathBuf {
        self.path(kb_path).with_file_name(self.new)
    }

    /// What to say, given whether the replacement is there too.
    fn wording(&self, replacement_exists: bool) -> (&'static str, &'static str) {
        if replacement_exists {
            (
                self.consequence_beside_replacement,
                self.remedy_beside_replacement,
            )
        } else {
            (self.consequence, self.remedy)
        }
    }
}

/// Which renamed files are still sitting in this knowledge base.
///
/// `symlink_metadata`, not `exists()`: a dangling symlink named `.kb-mcpignore`
/// is still a file with that name in the way, and `exists()` follows the link
/// and answers no.
fn files_that_stopped_being_read(kb_path: &Path) -> Vec<Finding> {
    RENAMED
        .iter()
        .filter_map(|legacy| {
            let path = legacy.path(kb_path);
            std::fs::symlink_metadata(&path).ok()?;
            // Same question for the replacement, and the same answer for the
            // same reason: a dangling `.grooveignore` is a name that is taken.
            let replacement_exists =
                std::fs::symlink_metadata(legacy.replacement_path(kb_path)).is_ok();
            let (consequence, remedy) = legacy.wording(replacement_exists);
            Some(Finding {
                check: legacy.check,
                severity: Severity::Warning,
                summary: format!(
                    "{} {} (renamed to {} in 0.26.0, with no alias)",
                    legacy.old, consequence, legacy.new
                ),
                count: 1,
                samples: vec![path.display().to_string()],
                remedy,
            })
        })
        .collect()
}

/// Turn a full list of paths into the same shape the SQL scans produce.
fn truncated(paths: Vec<String>) -> IntegrityScan {
    IntegrityScan {
        count: paths.len() as u64,
        samples: paths.into_iter().take(SAMPLE_LIMIT).collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registry_md() -> Registry {
        Registry::from_enabled(&["md".to_string()]).expect("md registry")
    }

    /// A directory that outlives one `Database` so a test can **close and
    /// reopen** the file. Reopening is the whole point for the vector-table
    /// checks: `Database::open` runs the forward migrations, and what those do
    /// to a damaged database is exactly what is under test.
    struct TempDir(std::path::PathBuf);
    impl TempDir {
        fn new(prefix: &str) -> Self {
            let p = crate::test_support::unique_temp_path(&format!("groove-doctor-{prefix}"));
            std::fs::create_dir_all(&p).expect("create temp dir");
            Self(p)
        }
        fn db(&self) -> String {
            self.0.join("t.db").to_string_lossy().into_owned()
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn seed(db: &Database) {
        db.verify_embedding_meta("bge-small-en-v1.5", 384)
            .expect("meta");
        let doc = db
            .upsert_document(
                "notes/a.md",
                Some("A"),
                None,
                None,
                None,
                &[],
                None,
                "h",
                12,
            )
            .expect("upsert");
        db.insert_chunk(doc, 0, Some("H"), None, "body", None, &vec![0.1; 384], 1.0)
            .expect("chunk");
    }

    fn db_with_one_chunk() -> Database {
        let db = Database::open_in_memory().expect("open");
        db.verify_embedding_meta("bge-small-en-v1.5", 384)
            .expect("meta");
        let doc = db
            .upsert_document(
                "notes/a.md",
                Some("A"),
                None,
                None,
                None,
                &[],
                None,
                "h",
                12,
            )
            .expect("upsert");
        db.insert_chunk(doc, 0, Some("H"), None, "body", None, &vec![0.1; 384], 1.0)
            .expect("chunk");
        db
    }

    /// A knowledge base with none of the renamed files anywhere near it.
    ///
    /// Every check except `files_that_stopped_being_read` reads the database
    /// and nothing else, so all these tests need is a path where that one
    /// finds nothing. A path that does not exist satisfies it exactly, with no
    /// directory to clean up afterwards.
    ///
    /// Absolute, and under a parent that does not exist either: `.kb-mcp.db`
    /// is looked for beside the knowledge base, and a *relative* path would
    /// put "beside" in the working directory — which would make these tests
    /// depend on what happens to be sitting in it.
    fn kb_with_nothing_left_behind() -> PathBuf {
        std::env::temp_dir()
            .join("groove-doctor-no-such-parent")
            .join("kb")
    }

    #[test]
    fn a_healthy_index_has_nothing_to_report() {
        let db = db_with_one_chunk();
        let report = run(&db, &registry_md(), &kb_with_nothing_left_behind()).expect("run");
        assert!(
            report.is_clean(),
            "a freshly built index should report nothing: {:?}",
            report.findings
        );
        assert_eq!((report.documents, report.chunks), (1, 1));
    }

    #[test]
    fn a_chunk_whose_embedding_vanished_is_reported() {
        let db = db_with_one_chunk();
        db.execute_for_test("DELETE FROM vec_chunks").expect("del");

        let report = run(&db, &registry_md(), &kb_with_nothing_left_behind()).expect("run");
        let f = report
            .findings
            .iter()
            .find(|f| f.check == "missing-embedding")
            .expect("the missing embedding must be reported");
        assert_eq!(f.severity, Severity::Error);
        assert_eq!(f.count, 1);
        assert_eq!(f.samples, vec!["notes/a.md #0".to_string()]);
    }

    #[test]
    fn a_chunk_whose_fts_row_vanished_is_reported() {
        let db = db_with_one_chunk();
        db.execute_for_test("DELETE FROM fts_chunks").expect("del");

        let report = run(&db, &registry_md(), &kb_with_nothing_left_behind()).expect("run");
        let f = report
            .findings
            .iter()
            .find(|f| f.check == "missing-fts-row")
            .expect("the missing FTS row must be reported");
        assert_eq!(f.count, 1);
        // This one is repaired by an ordinary index run, not a --force one:
        // `backfill_fts` reinserts from the chunk text already stored.
        assert!(
            f.remedy.starts_with("groove index ("),
            "remedy should not ask for a full re-embed: {}",
            f.remedy
        );
    }

    #[test]
    fn rows_left_behind_by_a_vanished_chunk_are_reported() {
        let db = db_with_one_chunk();
        // Delete the chunk without touching the two tables that reference it —
        // the state a partially applied write would leave.
        db.execute_for_test("DELETE FROM chunks").expect("del");

        let report = run(&db, &registry_md(), &kb_with_nothing_left_behind()).expect("run");
        let checks: Vec<&str> = report.findings.iter().map(|f| f.check).collect();
        assert!(checks.contains(&"orphan-embedding"), "{checks:?}");
        assert!(checks.contains(&"orphan-fts-row"), "{checks:?}");
        assert!(checks.contains(&"document-without-chunks"), "{checks:?}");
    }

    #[test]
    fn a_document_the_resource_surface_withholds_is_explained() {
        let db = db_with_one_chunk();
        // Past the read cap: indexed and searchable, but no read returns it.
        db.execute_for_test(&format!(
            "UPDATE documents SET size_bytes = {} WHERE path = 'notes/a.md'",
            crate::server::GET_DOCUMENT_MAX_BYTES + 1
        ))
        .expect("update");

        let report = run(&db, &registry_md(), &kb_with_nothing_left_behind()).expect("run");
        let f = report
            .findings
            .iter()
            .find(|f| f.check == "larger-than-a-read-returns")
            .expect("the oversized document must be explained");
        assert_eq!(f.severity, Severity::Warning);
        assert_eq!(f.samples, vec!["notes/a.md".to_string()]);
    }

    /// codex P1 round 1 + P2 round 2. Two ways to lose the vector table, and
    /// they do **not** produce the same report — because `Database::open` runs
    /// the migrations, and one of them puts the table back.
    ///
    /// The first version of this test dropped the table on an already-open
    /// database, which no caller does: the CLI opens the file it was handed.
    /// It passed while the finding was unreachable through the actual command.
    #[test]
    fn losing_the_vector_table_is_reported_by_whichever_check_can_see_it() {
        let dir = TempDir::new("vec-loss");
        {
            let db = Database::open(&dir.db()).expect("open");
            seed(&db);
            db.execute_for_test("DROP TABLE vec_chunks").expect("drop");
        }

        // (a) The embedding metadata survived, so opening the file **recreates**
        //     the table, empty. `vector-table-missing` cannot fire — and does
        //     not need to, because every chunk now reads as missing its
        //     embedding, which is just as loud.
        {
            let db = Database::open(&dir.db()).expect("reopen");
            let report = run(&db, &registry_md(), &kb_with_nothing_left_behind()).expect("run");
            let checks: Vec<&str> = report.findings.iter().map(|f| f.check).collect();
            assert!(
                !checks.contains(&"vector-table-missing"),
                "the migration put the table back, so this is not what is wrong: {checks:?}"
            );
            let f = report
                .findings
                .iter()
                .find(|f| f.check == "missing-embedding")
                .expect("every chunk lost its embedding and must be reported");
            assert_eq!(f.count, 1);
            db.execute_for_test(
                "DROP TABLE vec_chunks;
                 DELETE FROM index_meta WHERE key IN ('embedding_model', 'embedding_dim')",
            )
            .expect("drop table and meta");
        }

        // (b) Metadata gone too, so nothing recreates it. Now both per-chunk
        //     scans have nothing to scan and answer clean — the case that would
        //     otherwise report a healthy index while vector search returns
        //     nothing at all.
        let db = Database::open(&dir.db()).expect("reopen");
        let report = run(&db, &registry_md(), &kb_with_nothing_left_behind()).expect("run");
        let f = report
            .findings
            .iter()
            .find(|f| f.check == "vector-table-missing")
            .expect("a vector table that stays missing must be reported");
        assert_eq!(f.severity, Severity::Error);
        assert_eq!(f.count, 1, "it names how many chunks are stranded");
        assert!(!report.is_clean());
    }

    /// The companion: an index with no chunks *and* no vector table is a fresh
    /// one, not a broken one.
    #[test]
    fn an_empty_index_without_a_vector_table_is_not_a_finding() {
        let db = Database::open_in_memory().expect("open");
        assert!(
            run(&db, &registry_md(), &kb_with_nothing_left_behind())
                .expect("run")
                .is_clean(),
            "a database with nothing in it has nothing wrong with it"
        );
    }

    /// codex P2 round 6: a chunk whose document row is gone is invisible to
    /// every other check, because the two chunk-level scans inner-join
    /// `documents` — so with its vector and FTS rows intact, an index that
    /// silently drops that chunk from every search reported nothing at all.
    #[test]
    fn a_chunk_whose_document_vanished_is_reported() {
        let db = db_with_one_chunk();
        // Foreign keys are on for this connection, so delete the way a broken
        // one would: through a connection that has them off.
        db.execute_for_test(
            "PRAGMA foreign_keys = OFF;
             DELETE FROM documents;
             PRAGMA foreign_keys = ON;",
        )
        .expect("orphan the chunk");

        let report = run(&db, &registry_md(), &kb_with_nothing_left_behind()).expect("run");
        let f = report
            .findings
            .iter()
            .find(|f| f.check == "chunk-without-document")
            .expect("an orphaned chunk must be reported");
        assert_eq!(f.severity, Severity::Error);
        assert_eq!(f.count, 1);
        // The point of the finding: nothing else sees it.
        let others: Vec<&str> = report
            .findings
            .iter()
            .map(|f| f.check)
            .filter(|c| *c != "chunk-without-document")
            .collect();
        assert!(
            !others.contains(&"missing-embedding") && !others.contains(&"missing-fts-row"),
            "the chunk-level scans join documents, so they cannot see this one: {others:?}"
        );
    }

    /// codex P2 round 1: `get_document` applies the same cap through the same
    /// chooser, so offering it as the alternative sends the reader nowhere.
    #[test]
    fn the_oversize_remedy_does_not_name_a_call_that_refuses_the_same_file() {
        let db = db_with_one_chunk();
        db.execute_for_test(&format!(
            "UPDATE documents SET size_bytes = {} WHERE path = 'notes/a.md'",
            crate::server::GET_DOCUMENT_MAX_BYTES + 1
        ))
        .expect("update");

        let report = run(&db, &registry_md(), &kb_with_nothing_left_behind()).expect("run");
        let f = report
            .findings
            .iter()
            .find(|f| f.check == "larger-than-a-read-returns")
            .expect("finding");
        assert!(
            !f.remedy.contains("get_document"),
            "remedy must not point at a call with the same cap: {}",
            f.remedy
        );
    }

    #[test]
    fn an_index_from_before_sizes_were_recorded_says_so() {
        let db = db_with_one_chunk();
        db.execute_for_test("UPDATE documents SET size_bytes = NULL")
            .expect("update");

        let report = run(&db, &registry_md(), &kb_with_nothing_left_behind()).expect("run");
        let f = report
            .findings
            .iter()
            .find(|f| f.check == "size-not-recorded")
            .expect("an unrecorded size must be reported");
        assert_eq!(f.count, 1);
        // Not an error: nothing is broken, the answer is just not known yet.
        assert_eq!(f.severity, Severity::Warning);
    }

    // ---- files the rename left behind ------------------------------------

    /// A knowledge base *inside* the temp directory, so that "beside it" — the
    /// parent, where the index goes — is inside it too and is cleaned up with
    /// it.
    fn kb_in(dir: &TempDir) -> PathBuf {
        let kb = dir.0.join("kb");
        std::fs::create_dir_all(&kb).expect("create the knowledge base dir");
        kb
    }

    #[test]
    fn a_leftover_ignore_file_is_reported_with_what_it_costs() {
        let dir = TempDir::new("legacy-ignore");
        let kb = kb_in(&dir);
        let left = kb.join(".kb-mcpignore");
        std::fs::write(&left, "secrets/\n").expect("write the old ignore file");

        let db = db_with_one_chunk();
        let report = run(&db, &registry_md(), &kb).expect("run");
        let f = report
            .findings
            .iter()
            .find(|f| f.check == "legacy-ignore-file")
            .expect("an unread .kb-mcpignore must be reported");
        // The consequence, not the fact of the rename: someone reading this
        // needs to know their exclusions are not in effect. It stops short of
        // naming what leaked, because this check reads neither the file nor
        // the rules in force — see `a_summary_never_claims_a_leak_it_did_not_
        // measure`.
        assert!(
            f.summary.contains("indexed now"),
            "the summary must say what it costs, got: {}",
            f.summary
        );
        assert!(
            f.remedy.contains(".grooveignore"),
            "the remedy must name the new file, got: {}",
            f.remedy
        );
        assert_eq!(f.samples, vec![left.display().to_string()]);
    }

    #[test]
    fn the_names_in_use_today_are_not_reported() {
        // The check looks for the old name, not for "a file that sounds like
        // an ignore file". A knowledge base that migrated correctly is clean.
        let dir = TempDir::new("current-names");
        let kb = kb_in(&dir);
        std::fs::write(kb.join(".grooveignore"), "secrets/\n").expect("write");
        std::fs::write(kb.join(".groove-eval.yml"), "queries: []\n").expect("write");
        std::fs::write(kb.join(".groove-eval-history.json"), "{}").expect("write");
        std::fs::write(dir.0.join(".groove.db"), b"the live index").expect("write");

        let db = db_with_one_chunk();
        let report = run(&db, &registry_md(), &kb).expect("run");
        assert!(
            report.is_clean(),
            "a migrated knowledge base has nothing to report, got: {:?}",
            report.findings
        );
    }

    #[test]
    fn every_row_in_the_table_raises_its_own_finding_and_only_that_one() {
        // Table-driven rather than one test per row, so a row added later is
        // covered without anyone remembering to cover it.
        //
        // This says nothing about *where* the file is looked for: the fixture
        // is written at `Legacy::path`, so a mistake in `Legacy::path` moves
        // the fixture with it. Measured — pointing `BesideIndex` at the
        // knowledge base root left this green. That half is
        // `each_old_name_is_looked_for_beside_the_file_that_replaced_it`.
        for legacy in RENAMED {
            let dir = TempDir::new("renamed");
            let kb = kb_in(&dir);
            let path = legacy.path(&kb);
            std::fs::write(&path, b"left behind").expect("write the legacy file");

            let db = db_with_one_chunk();
            let report = run(&db, &registry_md(), &kb).expect("run");
            let found: Vec<&str> = report.findings.iter().map(|f| f.check).collect();
            assert_eq!(
                found,
                vec![legacy.check],
                "{} at {} should raise exactly {}",
                legacy.old,
                path.display(),
                legacy.check
            );
        }
    }

    /// Where each replacement is actually read from, taken from production
    /// wherever production exposes it.
    ///
    /// This is the independent half. `Legacy::path` cannot be checked by a
    /// test that also uses it to place the fixture, so the expectation has to
    /// come from somewhere the check does not.
    ///
    /// Every entry here comes out of production, which is also why `RENAMED`
    /// holds only files production pins: a row whose location a configuration
    /// can move has nothing to anchor to, and could not be checked from here
    /// without re-deriving the resolution it is supposed to be independent of.
    fn where_the_replacement_lives(kb: &Path) -> Vec<(&'static str, PathBuf)> {
        vec![
            // `ExclusionRules::load` reads `kb_path.join(IGNORE_FILE_NAME)`.
            (
                "legacy-ignore-file",
                kb.join(crate::exclusion::IGNORE_FILE_NAME),
            ),
            ("legacy-index-file", crate::resolve_db_path(kb)),
        ]
    }

    #[test]
    fn each_old_name_is_looked_for_beside_the_file_that_replaced_it() {
        let kb = std::env::temp_dir()
            .join("groove-doctor-anchor-no-such-parent")
            .join("kb");
        let anchors = where_the_replacement_lives(&kb);
        assert_eq!(
            anchors.len(),
            RENAMED.len(),
            "every row in RENAMED needs an anchor here, or its location is \
             checked by nothing"
        );
        for (check, replacement) in anchors {
            let legacy = RENAMED
                .iter()
                .find(|l| l.check == check)
                .unwrap_or_else(|| panic!("{check} is not a row in RENAMED"));
            assert_eq!(
                replacement
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned()),
                Some(legacy.new.to_string()),
                "{check}: the table names a replacement production does not use"
            );
            assert_eq!(
                legacy.path(&kb),
                replacement.with_file_name(legacy.old),
                "{check}: the old name is looked for somewhere other than \
                 beside the file that replaced it"
            );
        }
    }

    #[test]
    fn a_remedy_never_tells_you_to_write_over_a_file_you_are_using() {
        // codex P2 on PR #203. "rename it to .grooveignore" is right until a
        // `.grooveignore` is already there, and then it is an instruction to
        // destroy the configuration in use. The rule is on the shape of the
        // sentence, so a row added later cannot reintroduce it quietly.
        for legacy in RENAMED {
            assert!(
                !legacy.remedy_beside_replacement.contains("rename it to"),
                "{}: the remedy for the case where {} already exists must not \
                 be a rename onto it, got: {}",
                legacy.check,
                legacy.new,
                legacy.remedy_beside_replacement
            );
        }
    }

    #[test]
    fn a_summary_never_claims_a_leak_it_did_not_measure() {
        // `doctor` reads neither the legacy file nor the exclusion rules in
        // force, and reimplementing either to sharpen the sentence is the one
        // thing this module's documentation rules out. So "every path it
        // excluded is being indexed" is a claim it cannot support: a
        // `.kb-mcpignore` holding only `node_modules/` costs nothing, because
        // `exclude_dirs` and the hardcoded denylist already cover it
        // (codex P2 on PR #203).
        //
        // The second phrase came from the next round of the same review. The
        // presence of a `.grooveignore` is not the same as its being in
        // effect: one that is a directory, a hard link or over the cap is
        // refused by `ExclusionRules::load` and applies no patterns, and
        // `symlink_metadata` sees a name either way.
        const UNMEASURED: &[(&str, &str)] = &[
            ("every path", "claims every excluded path leaked"),
            (
                "decides what",
                "claims a file is in effect, which presence does not establish",
            ),
        ];
        for legacy in RENAMED {
            for (label, text) in [
                ("consequence", legacy.consequence),
                (
                    "consequence_beside_replacement",
                    legacy.consequence_beside_replacement,
                ),
            ] {
                for (phrase, why) in UNMEASURED {
                    assert!(
                        !text.contains(phrase),
                        "{}: {label} {why} — this check opens nothing, got: {text}",
                        legacy.check
                    );
                }
            }
        }
    }

    #[test]
    fn a_remedy_that_removes_something_says_how_to_know_it_is_safe() {
        // The second half of the same lesson, from a second P2. "delete it —
        // the newer file already holds the runs" reads as a fact and is a
        // guess: `History::load` turns unparseable content into an empty
        // history, so a corrupt replacement looks identical from here, and
        // following that sentence throws away the only recoverable baselines.
        //
        // Nothing here can check the replacement's contents without becoming a
        // second implementation of reading it, so the rule is on the sentence:
        // a remedy that removes a file has to name the moment it becomes safe.
        const CONDITIONS: &[&str] = &["once", "until", "then"];
        for legacy in RENAMED {
            for (label, remedy) in [
                ("remedy", legacy.remedy),
                (
                    "remedy_beside_replacement",
                    legacy.remedy_beside_replacement,
                ),
            ] {
                if !remedy.contains("delete") {
                    continue;
                }
                assert!(
                    CONDITIONS.iter().any(|w| remedy.contains(w)),
                    "{}: {label} removes a file without saying how to know that \
                     is safe — name a check with one of {CONDITIONS:?}, got: {remedy}",
                    legacy.check
                );
            }
        }
    }

    #[test]
    fn a_live_replacement_changes_both_what_is_claimed_and_what_is_advised() {
        let dir = TempDir::new("both-files");
        let kb = kb_in(&dir);
        std::fs::write(kb.join(".kb-mcpignore"), "secrets/\n").expect("write the old one");
        std::fs::write(kb.join(".grooveignore"), "secrets/\n").expect("write the live one");

        let db = db_with_one_chunk();
        let report = run(&db, &registry_md(), &kb).expect("run");
        let f = report
            .findings
            .iter()
            .find(|f| f.check == "legacy-ignore-file")
            .expect("the old file is still worth naming");
        // The claim `doctor` cannot support: it does not read either file, so
        // with a live `.grooveignore` present it cannot know whether anything
        // the old one excluded is in the index.
        assert!(
            !f.summary.contains("being indexed"),
            "with a live .grooveignore beside it, doctor must not claim a leak \
             it did not check: {}",
            f.summary
        );
        assert!(
            f.remedy.contains("merge"),
            "the remedy must preserve the live file, got: {}",
            f.remedy
        );
    }

    #[test]
    fn a_dangling_symlink_with_the_old_name_is_still_in_the_way() {
        // Why `symlink_metadata` and not `exists()`: a symlink named
        // `.kb-mcpignore` whose target is gone is still a file with that name
        // sitting there, unread, and `exists()` follows the link and says no.
        let dir = TempDir::new("dangling");
        let kb = kb_in(&dir);
        let link = kb.join(".kb-mcpignore");
        let missing = kb.join("target-that-never-existed");

        #[cfg(unix)]
        let made = std::os::unix::fs::symlink(&missing, &link).is_ok();
        // Windows needs developer mode or admin rights for this (WinError
        // 1314), so the branch is skipped rather than failed there — the same
        // shape `watcher.rs` uses.
        #[cfg(windows)]
        let made = std::os::windows::fs::symlink_file(&missing, &link).is_ok();

        if !made {
            return;
        }
        assert!(
            !link.exists(),
            "the link must dangle for this to mean anything"
        );

        let db = db_with_one_chunk();
        let report = run(&db, &registry_md(), &kb).expect("run");
        assert!(
            report
                .findings
                .iter()
                .any(|f| f.check == "legacy-ignore-file"),
            "a dangling .kb-mcpignore is still in the way, got: {:?}",
            report.findings
        );
    }
}
