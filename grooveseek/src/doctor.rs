//! `groove doctor` — ask the index whether it is in the state it should be.
//!
//! Three groups of question, and one deliberate omission. (Since 1.11.0 the
//! servability group also asks about the declared-field set -- see
//! [`crate::db::Database::read_declared_fields`] and the two
//! `declared-fields-*` findings in [`crate::doctor::run`].)
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
//! which is the failure mode this whole feature is about. The same rule holds
//! for the declared-field set (D-19): whether `--field` / `fields` filters are
//! refused is decided by [`crate::db::Database::read_declared_fields`] being
//! absent ([`crate::db::Database::refuse_field_filters_while_pending`]), and
//! what the next `groove index` will refresh is decided by comparing that key
//! with [`crate::indexer::declared_field_names`] of the schema on disk -- this
//! module reads the same key and calls the same function, it does not
//! re-derive either rule.
//!
//! **What the chunker gave up on.** Which source files were chunked by lines
//! rather than at their definitions, because one sat past the scope bound or
//! because the file wanted more chunks than one file may contribute. Those
//! files are whole and searchable — every byte reaches the index, which is what
//! [ADR-0012] requires — but their chunks carry no symbol kind, heading or
//! scope, so a query shaped like a definition cannot reach them. The parser
//! says so with a tag on the document; until now nothing read it back.
//!
//! [ADR-0012]: https://github.com/alphabet-h/grooveseek/blob/main/docs/decisions/0012-chunk-code-at-its-definitions-and-fill-the-gaps-by-line.md
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
/// Findings come out in the order below — integrity, then servability, then
/// what the chunker gave up on — because the first group means something is
/// broken, the second means something is merely unavailable, and the third
/// means everything arrived but in a coarser shape than usual.
///
/// The third argument is what `<kb_path>/groove-schema.toml` compiles to
/// ([`crate::indexer::load_declared_schema`]), or `None` when there is no
/// such file; the caller reads it so that a schema that does not load stops
/// the command before the database is opened, the way `groove index` and
/// `groove validate` already fail (exit 2, "could not look").
pub fn run(
    db: &Database,
    registry: &Registry,
    schema: Option<&crate::schema::Schema>,
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

    findings.extend(declared_fields_findings(db, schema)?);

    // -- what the chunker gave up on ----------------------------------------

    // Before the finding below, because it says whether that finding can answer at all: an
    // index written before the policy changed may hold files the old truncation cut short,
    // and they carry no tag to find them by. Only worth saying where there are code documents
    // to be wrong about, and `with_line_numbers` is that population.
    let with_line_numbers = db.tags_of_documents_with_line_numbers()?;
    // Absent or recorded as something else, both. Absent is an index no run has looked at
    // since the upgrade; the legacy marker is one that has been looked at and found wanting.
    // Neither can say its source files are whole.
    let policy = db.read_code_chunk_policy()?;
    let policy_is_current = policy.as_deref() == Some(crate::indexer::CODE_CHUNK_POLICY);
    if !policy_is_current && !with_line_numbers.is_empty() {
        findings.push(Finding {
            check: "chunk-policy-not-recorded",
            severity: Severity::Warning,
            summary: format!(
                "{} indexed source file(s) were chunked before it was recorded how a file over \
                 the chunk limit is handled, so whether any of them lost its tail is not known",
                with_line_numbers.len()
            ),
            count: with_line_numbers.len() as u64,
            samples: Vec::new(),
            // The only run that re-chunks a file whose content has not changed.
            remedy: "groove index --force (re-chunks and re-embeds them)",
        });
    }

    let without_definitions = crate::parser::code::TAGS_WITHOUT_DEFINITIONS;
    // Only documents a parser gave line numbers to are asked, because `tags` alone proves
    // nothing: it is frontmatter, so a note about code parsing can declare `parse:too-deep`
    // and `code` by hand and be believed. `chunks.start_line` cannot be declared -- it comes
    // from a parser's own account of where a chunk sat in its file.
    let chunked_by_lines: Vec<String> = with_line_numbers
        .into_iter()
        .filter(|(_, tags)| {
            tags.iter()
                .any(|t| without_definitions.contains(&t.as_str()))
        })
        .map(|(path, _)| path)
        .collect();
    findings.extend(finding(
        "chunked-without-definitions",
        Severity::Warning,
        format!(
            "{} indexed source file(s) were chunked by lines rather than at their definitions, \
             so their chunks carry no symbol kind, heading or scope",
            chunked_by_lines.len()
        ),
        truncated(chunked_by_lines),
        // Not an index command: re-running one reaches the same bound and makes the same
        // choice. What changes the answer is the file (`doctor.rs` already carries the same
        // caution for the oversize finding).
        "split the file, or flatten its nesting -- the text stays searchable either way, \
         it is the definition metadata that is missing",
    ));

    Ok(Report {
        documents: db.document_count()?,
        chunks: db.chunk_count()?,
        findings,
    })
}

/// The declared-field set (feature-58 / ADR-0020), read the way the index and
/// the search read it, and compared with the schema the way the next
/// `groove index` will compare it.
///
/// `index_meta.declared_fields` has three states ([`crate::indexer`]'s
/// `declared_fields_recorded` doc): **absent**, **`[]`** and a **non-empty
/// list**. Only the first is a problem, and it always is: the search gate
/// ([`crate::db::Database::refuse_field_filters_while_pending`]) refuses every
/// `--field` / `fields` request while the key is absent, so every absent key is
/// reported -- a report that exits 0 right before a field-filtered job is
/// refused would be lying to the CI gate that asked it (local Codex round 3,
/// user decision 2026-09-10). Two wordings, one check:
///
/// - Absent while a pass token is stored ([`crate::db::Database::read_declared_fields_pass`]):
///   a `groove index` run is refreshing the rows right now, or died doing so.
/// - Absent with no token: an index written before 1.9.0 that no run has
///   completed on since, or a run that ended before recording. The rows (if
///   any) are of a generation nobody can name; the schema (if any) has never
///   been recorded. An index with no rows and no schema is in this state too --
///   [`crate::indexer::rebuild_index`] treats it as "nothing to refresh" and
///   records `[]` on its next run, which is exactly the remedy.
///
/// The one absent state that is not reported is an index with **no documents
/// at all**: that is a fresh database, not a broken one -- the rule the
/// vector-table check already applies (an empty index without a vector table
/// is not a finding), and the state `groove serve` creates before its watcher
/// has seen a file. Nothing indexed means nothing a field filter could reach.
///
/// A recorded set that differs from what the schema on disk declares is not
/// wrong -- `--field` answers from the recorded set, consistently -- but it
/// is not what the next run will produce, so it is a Warning of the
/// `extension-not-registered` kind: the index is consistent with itself and
/// not with the configuration.
fn declared_fields_findings(
    db: &Database,
    schema: Option<&crate::schema::Schema>,
) -> Result<Vec<Finding>> {
    const REMEDY: &str = "groove index (one run records the set, without re-embedding)";
    let declared = crate::indexer::declared_field_names(schema);
    let declared_json = serde_json::to_string(&declared)?;
    // One statement, one moment: the key, the pass token and the row count must not
    // straddle a `groove index` run finishing in another process (see the method's
    // doc). Reading them one call at a time is exactly the torn read it exists to
    // prevent, so this function does not call the single-key readers.
    let crate::db::DeclaredFieldsSnapshot {
        recorded,
        pass_open,
        rows,
        documents,
    } = db.declared_fields_snapshot()?;
    let mut findings = Vec::new();
    match recorded {
        // An index with no documents is fresh, not pending -- the same rule the
        // vector-table check applies to an empty index (`groove serve` creates one
        // before its watcher has seen a file). Nothing has been indexed for a
        // field filter to reach, and the first run records the set.
        None if documents == 0 && rows == 0 && !pass_open => {}
        None => {
            if pass_open {
                findings.push(Finding {
                    check: "declared-fields-pending",
                    severity: Severity::Warning,
                    summary: format!(
                        "the declared-field set is not recorded: a groove index run is in \
                         progress or was interrupted while refreshing {rows} document_fields \
                         row(s), so --field / fields filters are refused until one completes"
                    ),
                    count: rows,
                    samples: Vec::new(),
                    remedy: REMEDY,
                });
            } else {
                findings.push(Finding {
                    check: "declared-fields-pending",
                    severity: Severity::Warning,
                    summary: format!(
                        "the declared-field set is not recorded while {rows} document_fields \
                         row(s) remain and groove-schema.toml declares {}, so --field / fields \
                         filters are refused until a groove index run completes",
                        list_or_none(&declared)
                    ),
                    count: rows,
                    samples: Vec::new(),
                    remedy: REMEDY,
                });
            }
        }
        Some(recorded_json) => {
            // The same parse the watcher paths apply: a value that is not a JSON list is
            // not a finding, it is an index `doctor` cannot read -- exit 2, like a corrupt
            // file (`indexer.rs`'s `declared_fields_recorded` bails the same way).
            let recorded: Vec<String> = serde_json::from_str(&recorded_json).map_err(|e| {
                anyhow::anyhow!(
                    "index_meta.declared_fields is not a JSON list: {recorded_json} ({e})"
                )
            })?;
            if recorded_json != declared_json {
                findings.push(Finding {
                    check: "declared-fields-stale",
                    severity: Severity::Warning,
                    summary: format!(
                        "the index recorded declared fields {} but groove-schema.toml declares {}; \
                         --field / fields filters answer from the recorded set until the next \
                         groove index run refreshes the {rows} document_fields row(s)",
                        list_or_none(&recorded),
                        list_or_none(&declared)
                    ),
                    count: rows,
                    samples: Vec::new(),
                    remedy: REMEDY,
                });
            }
        }
    }
    Ok(findings)
}

/// `[a, b]` for a summary line, `none` for an empty list -- so the text never
/// prints a bare `[]` that reads like a formatting accident.
fn list_or_none(keys: &[String]) -> String {
    if keys.is_empty() {
        "none".to_string()
    } else {
        format!("[{}]", keys.join(", "))
    }
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
        // What a completed `groove index` run leaves when there is no schema: the
        // declared-field set recorded as empty (D-19; an absent key is pending).
        db.write_declared_fields("[]").expect("declared");
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
        // As in `seed`: a completed run with no schema records `[]`.
        db.write_declared_fields("[]").expect("declared");
        db
    }

    // -- declared fields (D-19) ---------------------------------------------

    fn schema_declaring(keys: &[&str]) -> crate::schema::Schema {
        let src: String = keys.iter().map(|k| format!("[fields.{k}]\n")).collect();
        crate::schema::Schema::from_toml_str(&src).expect("schema")
    }

    fn declared_fields_finding(report: &Report) -> Option<&Finding> {
        report
            .findings
            .iter()
            .find(|f| f.check.starts_with("declared-fields-"))
    }

    #[test]
    fn an_index_no_run_has_recorded_is_pending_even_with_nothing_to_declare() {
        // Absent key, no rows, no schema: every pre-1.9.0 index until its first run under
        // 1.9.0+. `rebuild_index` treats it as "nothing to refresh" and records `[]` -- but
        // until that run, the search gate refuses every `--field`, so doctor must not exit 0
        // over it (local Codex round 3; user decision 2026-09-10 against the earlier
        // exception).
        let db = db_with_one_chunk();
        db.clear_declared_fields().expect("forget");
        assert!(db.read_declared_fields().expect("read").is_none());
        let report = run(&db, &registry_md(), None).expect("run");
        let f = declared_fields_finding(&report).expect("a pending finding");
        assert_eq!(f.check, "declared-fields-pending");
        assert_eq!(f.count, 0);
        assert!(
            f.summary.contains("declares none"),
            "summary was {:?}",
            f.summary
        );
    }

    #[test]
    fn a_recorded_empty_set_is_what_a_run_without_a_schema_leaves_and_is_clean() {
        // The fixture's own state: `[]` recorded, no rows, no schema. This is the state the
        // remedy for the test above produces, and the one every other fixture in this file
        // starts from.
        let db = db_with_one_chunk();
        assert_eq!(
            db.read_declared_fields().expect("read").as_deref(),
            Some("[]")
        );
        let report = run(&db, &registry_md(), None).expect("run");
        assert!(
            declared_fields_finding(&report).is_none(),
            "findings were {:?}",
            report.findings
        );
    }

    #[test]
    fn an_open_refresh_pass_is_pending_and_says_so() {
        let db = db_with_one_chunk();
        // `begin_declared_fields_pass` clears the key and stores the token together.
        db.clear_declared_fields().expect("forget");
        db.write_declared_fields_pass("4242-1").expect("token");
        let report = run(&db, &registry_md(), None).expect("run");
        let f = declared_fields_finding(&report).expect("a pending finding");
        assert_eq!(f.check, "declared-fields-pending");
        assert_eq!(f.severity, Severity::Warning);
        assert!(
            f.summary.contains("in progress"),
            "summary was {:?}",
            f.summary
        );
        assert!(
            f.remedy.contains("groove index"),
            "remedy was {:?}",
            f.remedy
        );
    }

    #[test]
    fn leftover_rows_under_no_recorded_set_are_pending_with_their_count() {
        // A run wrote rows and died before `write_declared_fields`: rows of a generation
        // nobody can name (`Database::clear_declared_fields`'s doc).
        let db = db_with_one_chunk();
        db.clear_declared_fields().expect("forget");
        db.replace_document_fields(
            "notes/a.md",
            &[
                ("status".to_string(), "active".to_string()),
                ("kind".to_string(), "note".to_string()),
            ],
        )
        .expect("rows");
        let report = run(&db, &registry_md(), None).expect("run");
        let f = declared_fields_finding(&report).expect("a pending finding");
        assert_eq!(f.check, "declared-fields-pending");
        assert_eq!(f.count, 2, "count is the number of value rows");
        assert!(f.samples.is_empty(), "rows carry no path to sample");
        assert!(
            !f.summary.contains("in progress"),
            "summary was {:?}",
            f.summary
        );
    }

    #[test]
    fn a_schema_that_declares_keys_the_index_never_recorded_is_pending() {
        let db = db_with_one_chunk();
        db.clear_declared_fields().expect("forget");
        let schema = schema_declaring(&["status"]);
        let report = run(&db, &registry_md(), Some(&schema)).expect("run");
        let f = declared_fields_finding(&report).expect("a pending finding");
        assert_eq!(f.check, "declared-fields-pending");
        assert_eq!(f.count, 0);
        assert!(
            f.summary.contains("[status]"),
            "summary was {:?}",
            f.summary
        );
    }

    #[test]
    fn a_recorded_set_that_differs_from_the_schema_is_stale() {
        let db = db_with_one_chunk();
        db.write_declared_fields(r#"["status"]"#).expect("record");
        let schema = schema_declaring(&["kind", "status"]);
        let report = run(&db, &registry_md(), Some(&schema)).expect("run");
        let f = declared_fields_finding(&report).expect("a stale finding");
        assert_eq!(f.check, "declared-fields-stale");
        assert_eq!(f.severity, Severity::Warning);
        assert!(
            f.summary.contains("[status]") && f.summary.contains("[kind, status]"),
            "summary was {:?}",
            f.summary
        );
        assert!(
            f.remedy.contains("groove index"),
            "remedy was {:?}",
            f.remedy
        );
    }

    #[test]
    fn a_schema_removed_after_the_index_recorded_keys_is_stale_too() {
        // Recorded `["status"]`, no schema on disk: the next run refreshes to `[]`, and
        // until then `--field status=...` still answers. The summary says `none`, never a
        // bare `[]`.
        let db = db_with_one_chunk();
        db.write_declared_fields(r#"["status"]"#).expect("record");
        let report = run(&db, &registry_md(), None).expect("run");
        let f = declared_fields_finding(&report).expect("a stale finding");
        assert_eq!(f.check, "declared-fields-stale");
        assert!(
            f.summary.contains("declares none"),
            "summary was {:?}",
            f.summary
        );
    }

    #[test]
    fn a_recorded_set_that_matches_the_schema_is_clean() {
        let db = db_with_one_chunk();
        db.write_declared_fields(r#"["status"]"#).expect("record");
        let schema = schema_declaring(&["status"]);
        let report = run(&db, &registry_md(), Some(&schema)).expect("run");
        assert!(
            declared_fields_finding(&report).is_none(),
            "findings were {:?}",
            report.findings
        );
    }

    #[test]
    fn the_five_named_keys_do_not_count_as_declared() {
        // `title` / `date` / `topic` / `depth` / `tags` have their own columns and filters;
        // `declared_field_names` leaves them out, so a schema naming only those matches a
        // recorded `[]` -- the same comparison `rebuild_index` makes.
        let db = db_with_one_chunk();
        db.write_declared_fields("[]").expect("record");
        let schema = schema_declaring(&["title", "tags"]);
        let report = run(&db, &registry_md(), Some(&schema)).expect("run");
        assert!(
            declared_fields_finding(&report).is_none(),
            "findings were {:?}",
            report.findings
        );
    }

    #[test]
    fn a_recorded_set_that_is_not_a_list_is_could_not_look() {
        // The watcher paths bail on this value too (`declared_fields_recorded`); a
        // diagnostic must not turn an unreadable key into a Warning it then reasons from.
        let db = db_with_one_chunk();
        db.write_declared_fields("{not a list").expect("record");
        let err = run(&db, &registry_md(), None).expect_err("must not report");
        assert!(
            err.to_string().contains("declared_fields"),
            "error was {err:#}"
        );
    }

    #[test]
    fn a_completed_refresh_reads_as_one_state_not_two() {
        // The state a `groove index` run leaves behind when it finishes (recorded set, token
        // cleared, rows written) is clean. Read as three statements, doctor could pair the
        // pre-run absent key with the post-run absent token and call it pending (local Codex
        // round 1); the snapshot below is the shape it must read instead.
        let db = db_with_one_chunk();
        db.write_declared_fields_pass("run-1").expect("token");
        db.replace_document_fields(
            "notes/a.md",
            &[("status".to_string(), "active".to_string())],
        )
        .expect("rows");
        db.clear_declared_fields_pass().expect("pass over");
        db.write_declared_fields(r#"["status"]"#).expect("record");

        let snap = db.declared_fields_snapshot().expect("snapshot");
        assert_eq!(snap.recorded.as_deref(), Some(r#"["status"]"#));
        assert!(!snap.pass_open);
        assert_eq!(snap.rows, 1);

        let schema = schema_declaring(&["status"]);
        let report = run(&db, &registry_md(), Some(&schema)).expect("run");
        assert!(
            declared_fields_finding(&report).is_none(),
            "findings were {:?}",
            report.findings
        );
    }

    #[test]
    fn the_declared_fields_diagnosis_reads_one_snapshot_only() {
        // Source-shape pin, the way `doctor_cli.rs` pins where the chunk policy is resolved:
        // the finding must be derived from `declared_fields_snapshot` alone. Any of the
        // single-key readers appearing in its body reopens the torn read between a key and
        // a token that a concurrent run can change in between.
        let src = include_str!("doctor.rs");
        let body = src
            .split("fn declared_fields_findings(")
            .nth(1)
            .expect("the finding function still exists")
            .split("\nfn ")
            .next()
            .expect("body");
        assert!(
            body.contains("declared_fields_snapshot()"),
            "the diagnosis no longer reads the one-statement snapshot"
        );
        for reader in [
            "read_declared_fields()",
            "read_declared_fields_pass()",
            "read_declared_fields_dirty()",
            "document_fields_count()",
            "document_fields_is_empty()",
        ] {
            assert!(
                !body.contains(reader),
                "{reader} inside the diagnosis is a second snapshot; read it from the one \
                 `declared_fields_snapshot` returns"
            );
        }
    }

    #[test]
    fn a_healthy_index_has_nothing_to_report() {
        let db = db_with_one_chunk();
        let report = run(&db, &registry_md(), None).expect("run");
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

        let report = run(&db, &registry_md(), None).expect("run");
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

        let report = run(&db, &registry_md(), None).expect("run");
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

        let report = run(&db, &registry_md(), None).expect("run");
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

        let report = run(&db, &registry_md(), None).expect("run");
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
            let report = run(&db, &registry_md(), None).expect("run");
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
        let report = run(&db, &registry_md(), None).expect("run");
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
            run(&db, &registry_md(), None).expect("run").is_clean(),
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

        let report = run(&db, &registry_md(), None).expect("run");
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

        let report = run(&db, &registry_md(), None).expect("run");
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

        let report = run(&db, &registry_md(), None).expect("run");
        let f = report
            .findings
            .iter()
            .find(|f| f.check == "size-not-recorded")
            .expect("an unrecorded size must be reported");
        assert_eq!(f.count, 1);
        // Not an error: nothing is broken, the answer is just not known yet.
        assert_eq!(f.severity, Severity::Warning);
    }

    /// Add a second document carrying `tags`, so a check that reads them has something to
    /// find beside the plain Markdown one every fixture starts with.
    ///
    /// `line_numbers` decides whether its chunk gets a line range, which is how a document a
    /// code parser produced is told apart from a note that merely says the same words in its
    /// frontmatter.
    fn with_tagged_document(db: &Database, path: &str, tags: &[&str], line_numbers: bool) {
        let owned: Vec<String> = tags.iter().map(|t| (*t).to_string()).collect();
        let doc = db
            .upsert_document(path, Some("T"), None, None, None, &owned, None, "h2", 12)
            .expect("upsert");
        db.insert_chunk_with_code(
            doc,
            0,
            Some("H"),
            None,
            "body",
            None,
            &vec![0.1; 384],
            1.0,
            crate::db::CodeMeta {
                line_range: line_numbers.then_some((1, 4)),
                symbol_kind: None,
            },
        )
        .expect("chunk");
    }

    fn chunked_without_definitions(report: &Report) -> Option<&Finding> {
        report
            .findings
            .iter()
            .find(|f| f.check == "chunked-without-definitions")
    }

    /// Say the index was built by a version that records what it did, so a fixture about the
    /// tags is not also a fixture about the generation.
    fn with_the_current_chunk_policy(db: &Database) {
        db.write_code_chunk_policy(crate::indexer::CODE_CHUNK_POLICY)
            .expect("policy");
    }

    #[test]
    fn an_index_from_before_the_chunk_policy_was_recorded_says_it_cannot_answer() {
        // The state this whole finding exists for: a file the old truncation cut short keeps
        // its chunks because its content has not changed, so it never acquires a tag and the
        // finding below would report nothing while the tail is still missing (codex P1,
        // round 2). What is knowable is that the answer is not knowable.
        //
        // Two ways to be in it: no run has looked since the upgrade (no key), or one has and
        // wrote down what it found. Both have to report.
        for recorded in [None, Some(crate::indexer::CODE_CHUNK_POLICY_LEGACY)] {
            let db = db_with_one_chunk();
            with_tagged_document(&db, "src/lib.rs", &["code", "lang:rust"], true);
            if let Some(policy) = recorded {
                db.write_code_chunk_policy(policy).expect("policy");
            }

            let report = run(&db, &registry_md(), None).expect("run");
            let f = report
                .findings
                .iter()
                .find(|f| f.check == "chunk-policy-not-recorded")
                .unwrap_or_else(|| panic!("{recorded:?} must still be reported"));
            assert_eq!(f.severity, Severity::Warning);
            assert_eq!(f.count, 1);
            assert!(f.remedy.contains("--force"), "remedy was {:?}", f.remedy);
        }
    }

    #[test]
    fn an_index_with_no_source_files_says_nothing_about_the_chunk_policy() {
        // Nothing to be wrong about: the fixture is one Markdown document, and the question
        // is only about files a code parser chunked.
        let db = db_with_one_chunk();

        let report = run(&db, &registry_md(), None).expect("run");
        assert!(
            !report
                .findings
                .iter()
                .any(|f| f.check == "chunk-policy-not-recorded"),
            "findings were {:?}",
            report.findings
        );
    }

    #[test]
    fn an_index_that_recorded_the_policy_is_not_asked_again() {
        let db = db_with_one_chunk();
        with_tagged_document(&db, "src/lib.rs", &["code", "lang:rust"], true);
        with_the_current_chunk_policy(&db);

        let report = run(&db, &registry_md(), None).expect("run");
        assert!(
            !report
                .findings
                .iter()
                .any(|f| f.check == "chunk-policy-not-recorded"),
            "findings were {:?}",
            report.findings
        );
    }

    #[test]
    fn a_source_file_chunked_by_lines_is_reported_whichever_bound_stopped_it() {
        let db = db_with_one_chunk();
        with_the_current_chunk_policy(&db);
        with_tagged_document(&db, "src/deep.rs", &["code", "parse:too-deep"], true);
        with_tagged_document(&db, "src/wide.rs", &["code", "parse:too-many-chunks"], true);

        let report = run(&db, &registry_md(), None).expect("run");
        let f = chunked_without_definitions(&report)
            .expect("both files gave up their definitions, so both belong to this finding");
        assert_eq!(f.severity, Severity::Warning);
        assert_eq!(f.count, 2);
        assert_eq!(
            f.samples,
            vec!["src/deep.rs".to_string(), "src/wide.rs".to_string()]
        );
        // Naming an index command here would send someone to a remedy that cannot work: the
        // next run reaches the same bound and makes the same choice.
        assert!(
            !f.remedy.contains("groove index"),
            "remedy was {:?}",
            f.remedy
        );
    }

    #[test]
    fn a_note_that_merely_spells_the_tags_by_hand_is_not_reported() {
        // `documents.tags` is frontmatter, so a note *about* code parsing can legally declare
        // every tag this check reads, `code` included -- which is why the check reads line
        // numbers instead of believing them (codex P2, round 1). The fixture writes both tags
        // and no line range, the exact shape that used to be reported.
        let db = db_with_one_chunk();
        with_tagged_document(
            &db,
            "notes/about-parsing.md",
            &["code", "parse:too-deep"],
            false,
        );

        let report = run(&db, &registry_md(), None).expect("run");
        assert!(
            chunked_without_definitions(&report).is_none(),
            "findings were {:?}",
            report.findings
        );
    }

    #[test]
    fn a_document_with_unreadable_tags_does_not_stop_the_report() {
        // The column is fail-open everywhere else it is read; a diagnostic is the last place
        // that should turn one bad row into "could not look".
        let db = db_with_one_chunk();
        with_tagged_document(&db, "src/wide.rs", &["code", "parse:too-many-chunks"], true);
        // Corrupted on a row the query actually returns, so the fail-open path is the one
        // under test rather than a row the join already left out.
        with_tagged_document(&db, "src/broken.rs", &["code"], true);
        db.execute_for_test("UPDATE documents SET tags = '{not json' WHERE path = 'src/broken.rs'")
            .expect("update");

        let report = run(&db, &registry_md(), None).expect("run");
        let f = chunked_without_definitions(&report).expect("the readable row is still found");
        assert_eq!(f.count, 1);
    }

    #[test]
    fn an_index_whose_files_kept_their_definitions_says_nothing_about_them() {
        let db = db_with_one_chunk();
        with_tagged_document(&db, "src/lib.rs", &["code", "lang:rust"], true);

        let report = run(&db, &registry_md(), None).expect("run");
        assert!(
            chunked_without_definitions(&report).is_none(),
            "findings were {:?}",
            report.findings
        );
    }
}
