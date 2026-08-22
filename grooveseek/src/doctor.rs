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
//! **The migration period.** Whether a `.kb-mcpignore` — the name the ignore
//! file had before [ADR-0007] — is still keeping documents out on paper while
//! the index holds them anyway. This is the one group that opens something
//! outside the database, and the same reasoning applies to it: the old file
//! goes through [`crate::exclusion::ExclusionRules`], the type the index walk
//! itself asks, rather than through a matcher assembled here. It is also the
//! one group with a removal date — see [`crate::legacy`], which owns it.
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
//!
//! [ADR-0007]: ../../docs/decisions/0007-rename-the-project-to-grooveseek.md

use std::path::Path;

use crate::db::{Database, IntegrityScan};
use crate::legacy::LegacyIgnore;
use crate::parser::Registry;
use anyhow::Result;
use serde::Serialize;

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
/// Findings come out in the order below — integrity, then servability, then the
/// migration period — because the first group means something is broken, the
/// second means something is merely unavailable, and the third means the index
/// is not what its knowledge base was set up to produce.
///
/// `kb_path` and `exclude_dirs` are only the third group's, and go away with it
/// (see [`crate::legacy`]). They are passed rather than a whole `Config` so
/// that removing the group removes the argument too.
pub fn run(
    db: &Database,
    registry: &Registry,
    kb_path: &Path,
    exclude_dirs: &[String],
) -> Result<Report> {
    let mut findings = Vec::new();

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

    // -- the migration period: the name this project stopped reading ---------
    //
    // Last, because it is the only group that opens anything outside the
    // database, and the only one that gets deleted. `all_paths` above is the
    // evidence it works from, so no further query is issued.
    let rules = crate::exclusion::ExclusionRules::load(kb_path, exclude_dirs.to_vec());
    match crate::legacy::inspect(kb_path, &rules, &all_paths) {
        LegacyIgnore::Absent => {}
        LegacyIgnore::CannotSay(why) => findings.push(Finding {
            check: "legacy-ignore-not-examined",
            severity: Severity::Warning,
            summary: format!(
                "a {} is present but could not be examined, so whether anything it names is \
                 in the index is not known: {why}",
                crate::legacy::LEGACY_IGNORE_FILE_NAME
            ),
            // One file, not zero findings. A check that could not run reporting
            // a count of nothing is how it starts reading as a clean bill.
            count: 1,
            samples: Vec::new(),
            remedy: "make it a plain readable file under the size cap, then run groove doctor again",
        }),
        // Through the same `finding` helper as every check above, so "there is
        // an old file but it costs nothing today" stays silent by the same rule
        // that keeps a clean integrity scan silent.
        LegacyIgnore::Read { still_indexed } => {
            let summary = format!(
                "{} indexed document(s) match a pattern in {}, which is no longer read; the \
                 current rules do not exclude them",
                still_indexed.len(),
                crate::legacy::LEGACY_IGNORE_FILE_NAME
            );
            // Never "delete the old file": a knowledge base whose new file is
            // broken looks the same from here, and that remedy would destroy
            // the only copy of the patterns (codex round 3 on PR #203). Never
            // "rename it" when the destination name is occupied either — that
            // overwrites a file in use (round 2).
            //
            // Occupancy, **not** `rules.ignore_file_patterns()`: that answers
            // `None` for a `.grooveignore` which exists and could not be read,
            // and renaming onto it is exactly as destructive as renaming onto a
            // working one (codex P2 round 1 on this PR).
            let remedy = if crate::legacy::live_ignore_name_is_taken(kb_path) {
                "copy the lines you still want from .kb-mcpignore into .grooveignore, \
                 then run groove index"
            } else {
                "rename .kb-mcpignore to .grooveignore, then run groove index"
            };
            findings.extend(finding(
                "indexed-despite-legacy-ignore",
                Severity::Warning,
                summary,
                truncated(still_indexed),
                remedy,
            ));
        }
    }

    Ok(Report {
        documents: db.document_count()?,
        chunks: db.chunk_count()?,
        findings,
    })
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

    /// A knowledge base for the checks that are not about one.
    ///
    /// Every test below except the migration-period ones wants
    /// `<kb_path>/.kb-mcpignore` to be absent, and a path that was never
    /// created answers that the same way an empty directory does — without a
    /// directory to clean up. Unique per call so no two tests can collide on it.
    fn empty_kb() -> std::path::PathBuf {
        crate::test_support::unique_temp_path("groove-doctor-nokb")
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

    #[test]
    fn a_healthy_index_has_nothing_to_report() {
        let db = db_with_one_chunk();
        let report = run(&db, &registry_md(), &empty_kb(), &[]).expect("run");
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

        let report = run(&db, &registry_md(), &empty_kb(), &[]).expect("run");
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

        let report = run(&db, &registry_md(), &empty_kb(), &[]).expect("run");
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

        let report = run(&db, &registry_md(), &empty_kb(), &[]).expect("run");
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

        let report = run(&db, &registry_md(), &empty_kb(), &[]).expect("run");
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
            let report = run(&db, &registry_md(), &empty_kb(), &[]).expect("run");
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
        let report = run(&db, &registry_md(), &empty_kb(), &[]).expect("run");
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
            run(&db, &registry_md(), &empty_kb(), &[])
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

        let report = run(&db, &registry_md(), &empty_kb(), &[]).expect("run");
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

        let report = run(&db, &registry_md(), &empty_kb(), &[]).expect("run");
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

        let report = run(&db, &registry_md(), &empty_kb(), &[]).expect("run");
        let f = report
            .findings
            .iter()
            .find(|f| f.check == "size-not-recorded")
            .expect("an unrecorded size must be reported");
        assert_eq!(f.count, 1);
        // Not an error: nothing is broken, the answer is just not known yet.
        assert_eq!(f.severity, Severity::Warning);
    }

    // -----------------------------------------------------------------------
    // The migration period (audit L-4)
    //
    // What the old file matches is unit-tested in `crate::legacy`. These are
    // about the report: which finding comes out, and what it tells an operator
    // to do. Each names the review round on PR #203 it answers, because that
    // withdrawn attempt is the reason this group is shaped the way it is.

    struct LegacyKb(std::path::PathBuf);

    impl LegacyKb {
        fn new(prefix: &str) -> Self {
            let p =
                crate::test_support::unique_temp_path(&format!("groove-doctor-legacy-{prefix}"));
            std::fs::create_dir_all(&p).expect("create kb");
            Self(p)
        }
        fn write(&self, name: &str, body: &str) {
            std::fs::write(self.0.join(name), body).expect("write");
        }
        fn report(&self, db: &Database) -> Report {
            run(db, &registry_md(), &self.0, &[]).expect("run")
        }
    }

    impl Drop for LegacyKb {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// The finding carries the path, which is the whole reason this group opens
    /// the file rather than asking whether the name is taken.
    #[test]
    fn a_document_the_old_ignore_file_names_is_reported_with_its_path() {
        let db = db_with_one_chunk();
        let kb = LegacyKb::new("named");
        kb.write(".kb-mcpignore", "notes/\n");

        let report = kb.report(&db);
        let f = report
            .findings
            .iter()
            .find(|f| f.check == "indexed-despite-legacy-ignore")
            .expect("a document the old file names must be reported");
        assert_eq!(f.severity, Severity::Warning);
        assert_eq!(f.count, 1);
        assert_eq!(f.samples, vec!["notes/a.md".to_string()]);
    }

    /// codex round 2 on PR #203: the remedy told an operator to put the old
    /// file in place of the one currently in effect.
    #[test]
    fn the_remedy_never_overwrites_a_grooveignore_that_is_in_use() {
        let db = db_with_one_chunk();
        let kb = LegacyKb::new("bothfiles");
        kb.write(".kb-mcpignore", "notes/\n");
        kb.write(".grooveignore", "logs/\n");

        let report = kb.report(&db);
        let f = report
            .findings
            .iter()
            .find(|f| f.check == "indexed-despite-legacy-ignore")
            .expect("the finding is the same one; only the remedy changes");
        assert!(
            !f.remedy.contains("rename"),
            "a .grooveignore in effect must not be renamed over: {}",
            f.remedy
        );

        // And the other way round, so this pins a branch rather than a constant.
        std::fs::remove_file(kb.0.join(".grooveignore")).expect("rm");
        let report = kb.report(&db);
        let f = report
            .findings
            .iter()
            .find(|f| f.check == "indexed-despite-legacy-ignore")
            .expect("still reported with no live file");
        assert!(
            f.remedy.contains("rename"),
            "with nothing to overwrite, renaming is the shortest fix: {}",
            f.remedy
        );
    }

    /// codex P2 round 1 on this PR: `ignore_file_patterns()` answers `None`
    /// both for "there is no `.grooveignore`" and for "there is one and it
    /// could not be read", so branching on it sent an operator to `mv` onto an
    /// occupied name — overwriting their file on Unix, failing on Windows, and
    /// either way leaving the documents just reported still in the index.
    ///
    /// A directory is the cheapest unreadable file, the same fixture the
    /// `CannotSay` tests use.
    #[test]
    fn a_grooveignore_that_exists_but_cannot_be_read_still_blocks_the_rename() {
        let db = db_with_one_chunk();
        let kb = LegacyKb::new("occupied");
        kb.write(".kb-mcpignore", "notes/\n");
        std::fs::create_dir_all(kb.0.join(".grooveignore")).expect("mkdir");

        let report = kb.report(&db);
        let f = report
            .findings
            .iter()
            .find(|f| f.check == "indexed-despite-legacy-ignore")
            .expect("the finding does not depend on the new file being readable");
        assert!(
            !f.remedy.contains("rename"),
            "the destination name is taken, whether or not what holds it can be read: {}",
            f.remedy
        );
    }

    /// No remedy in this group tells anyone to delete anything — codex round 3
    /// on PR #203, where "remove the old file" read the same whether or not the
    /// new one was any good.
    #[test]
    fn no_remedy_here_asks_for_a_deletion() {
        let db = db_with_one_chunk();
        let kb = LegacyKb::new("nodelete");
        kb.write(".kb-mcpignore", "notes/\n");

        for f in &kb.report(&db).findings {
            assert!(
                !f.remedy.contains("delete") && !f.remedy.contains("remove"),
                "{}: {}",
                f.check,
                f.remedy
            );
        }
    }

    /// codex round 7 on PR #203: the check could not run and the report said
    /// the index was clean.
    #[test]
    fn an_old_file_that_could_not_be_examined_is_a_finding_not_a_clean_bill() {
        let db = db_with_one_chunk();
        let kb = LegacyKb::new("unreadable");
        std::fs::create_dir_all(kb.0.join(".kb-mcpignore")).expect("mkdir");

        let report = kb.report(&db);
        assert!(
            !report.is_clean(),
            "a check that could not look must not report a clean bill"
        );
        let f = report
            .findings
            .iter()
            .find(|f| f.check == "legacy-ignore-not-examined")
            .expect("the check that could not run must say so");
        assert_eq!(f.severity, Severity::Warning);
        assert_eq!(f.count, 1);
    }

    /// codex round 3 on PR #203, the other half: an old file is not by itself a
    /// problem. It becomes one when the index holds something it names.
    #[test]
    fn an_old_file_that_costs_nothing_today_is_not_a_finding() {
        let db = db_with_one_chunk();
        let kb = LegacyKb::new("harmless");
        kb.write(".kb-mcpignore", "logs/\n");

        let report = kb.report(&db);
        assert!(
            report.is_clean(),
            "nothing indexed is under logs/, so there is nothing to say: {:?}",
            report.findings
        );
    }
}
