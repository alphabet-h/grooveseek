use super::*;
// (AX-40) The items only these tests use, from the submodules they moved to.
use super::rename::{read_crossed_rename_again, reindex_renamed_entry, settle_rename_destination};
use super::single_file::{
    names_a_walked_path, sort_single_path_look, write_parsed_document_with_vectors,
};

use std::collections::HashMap;

fn mk_entry(rel: &str, hash: &str) -> DiskEntry {
    DiskEntry {
        rel: rel.to_string(),
        hash: hash.to_string(),
        full: std::path::PathBuf::from(rel),
        size: 0,
    }
}

#[test]
fn test_detect_renames_single_move() {
    let disk = vec![mk_entry("new/x.md", "h1"), mk_entry("keep.md", "h2")];
    let mut db = HashMap::new();
    db.insert("old/x.md".to_string(), "h1".to_string());
    db.insert("keep.md".to_string(), "h2".to_string());
    let pairs = detect_renames(&disk, &db, &HashSet::new());
    assert_eq!(
        pairs,
        vec![("old/x.md".to_string(), "new/x.md".to_string())]
    );
}

#[test]
fn test_detect_renames_no_rename_when_new_path_exists() {
    // new path が既に DB にある = 別文書なので rename ペアにしない
    let disk = vec![mk_entry("b.md", "h1")];
    let mut db = HashMap::new();
    db.insert("a.md".to_string(), "h1".to_string());
    db.insert("b.md".to_string(), "h1".to_string());
    let pairs = detect_renames(&disk, &db, &HashSet::new());
    // disk には a.md が無いので a.md は DB orphan、b.md は既に DB にある
    // → 新規 disk path が無いのでペア無し
    assert!(pairs.is_empty());
}

#[test]
fn test_detect_renames_no_change_same_path_same_hash() {
    let disk = vec![mk_entry("a.md", "h1")];
    let mut db = HashMap::new();
    db.insert("a.md".to_string(), "h1".to_string());
    let pairs = detect_renames(&disk, &db, &HashSet::new());
    assert!(pairs.is_empty());
}

#[test]
fn test_detect_renames_deterministic_with_duplicate_hashes() {
    // A, B とも空ファイル (同 hash) で DB、disk 側も C, D の新 path
    // どちらに振っても意味論的には同じだが結果は deterministic であるべき
    let disk = vec![mk_entry("C.md", "hempty"), mk_entry("D.md", "hempty")];
    let mut db = HashMap::new();
    db.insert("A.md".to_string(), "hempty".to_string());
    db.insert("B.md".to_string(), "hempty".to_string());
    let pairs1 = detect_renames(&disk, &db, &HashSet::new());
    // 2 回目も同じ結果になること (HashMap iteration 順に依存しない)
    let pairs2 = detect_renames(&disk, &db, &HashSet::new());
    assert_eq!(pairs1, pairs2);
    // path 順の sort により A→C, B→D になるはず
    assert_eq!(
        pairs1,
        vec![
            ("A.md".to_string(), "C.md".to_string()),
            ("B.md".to_string(), "D.md".to_string()),
        ]
    );
}

#[test]
fn test_detect_renames_unmatched_hashes_are_dropped() {
    let disk = vec![mk_entry("new.md", "h_new")];
    let mut db = HashMap::new();
    db.insert("old.md".to_string(), "h_old".to_string()); // 別 hash
    let pairs = detect_renames(&disk, &db, &HashSet::new());
    // hash 不一致なのでペアにしない (old.md は削除対象、new.md は新規追加)
    assert!(pairs.is_empty());
}

#[test]
fn test_detect_renames_excludes_skipped_paths() {
    // DB に a.md (hash H)。disk 側は a.md が read 失敗 / size 超過で
    // skip され disk_entries に載らず、別の新規 b.md が同じ hash H を持つ。
    // a.md は disk から「消えた」のではなく単に今回未計上なだけなので、
    // rename ペア (a.md → b.md) にすり替わってはならない (codex P2)。
    let disk = vec![mk_entry("b.md", "H")];
    let mut db = HashMap::new();
    db.insert("a.md".to_string(), "H".to_string());
    let skipped: HashSet<String> = ["a.md".to_string()].into_iter().collect();
    let pairs = detect_renames(&disk, &db, &skipped);
    assert!(pairs.is_empty());
}

/// (codex P2 round 5 on PR #291) `.txt` ↔ `.md` is exactly the crossing the fix exists
/// for -- see `an_interrupted_...`-style E2E coverage in
/// `tests/index_declared_fields.rs` for the `document_fields` consequence; this pins the
/// predicate itself, both directions and the unregistered-extension edge.
#[test]
fn rename_crosses_a_parser_is_true_for_txt_md_and_an_unknown_extension() {
    let reg = Registry::from_enabled(&["md".into(), "txt".into()]).unwrap();
    assert!(
        rename_crosses_a_parser(&reg, "note.txt", "note.md"),
        "txt -> md changes which parser reads the same bytes"
    );
    assert!(
        rename_crosses_a_parser(&reg, "note.md", "note.txt"),
        "the reverse direction crosses too"
    );
    assert!(
        !rename_crosses_a_parser(&reg, "old.md", "new.md"),
        "an .md -> .md rename never changes the parser"
    );
    assert!(
        rename_crosses_a_parser(&reg, "note.xyz", "note.md"),
        "an unregistered extension (no parser at all) is as different from `.md` as any \
             other parser would be"
    );
    assert!(
        !rename_crosses_a_parser(&reg, "note.xyz", "other.xyz"),
        "two unregistered extensions do not cross -- neither side has a parser to lose"
    );
}

/// (codex P2 round 9 on PR #291) [`settle_cross_parser_rename`] is the decision, not the
/// I/O -- it needs only a [`Database`] and a [`SingleResult`], no embedder, so this pins the
/// decision directly rather than through a full rename E2E (see
/// `tests/index_declared_fields.rs` for the subprocess-level coverage of the bug this
/// closes: a cross-parser rename whose destination parser refuses the bytes used to leave
/// the old parser's row behind).
/// (audit AX-02, review Low-3) The rename's destination check: inside goes on, outside drops
/// the old row, and a failed look moves the row before returning the error -- the state a
/// failed read of the destination leaves.
#[test]
fn settle_rename_destination_ends_a_failed_look_as_a_failed_read() {
    let db = Database::open_in_memory().unwrap();
    db.verify_embedding_meta("bge-small-en-v1.5", 384).unwrap();
    let upsert = |p: &str| {
        db.upsert_document(p, None, None, None, None, &[], None, "h1", 10)
            .unwrap();
    };
    let full = Path::new("kb/new.md");

    upsert("old.md");
    let go_on = settle_rename_destination(&db, "old.md", "new.md", full, Ok(true)).unwrap();
    assert_eq!(go_on, None);
    assert_eq!(db.all_document_paths().unwrap(), vec!["old.md".to_string()]);

    let err = settle_rename_destination(
        &db,
        "old.md",
        "new.md",
        full,
        Err(std::io::Error::other("probe")),
    )
    .unwrap_err();
    assert!(format!("{err:#}").contains("failed to resolve"), "{err:#}");
    assert_eq!(
        db.all_document_paths().unwrap(),
        vec!["new.md".to_string()],
        "the row moved, as it does before a failed read"
    );

    let out = settle_rename_destination(&db, "new.md", "far.md", full, Ok(false)).unwrap();
    assert_eq!(out, Some(RenameOutcome::MovedOutOfTheKnowledgeBase));
    assert!(db.all_document_paths().unwrap().is_empty());
}

/// (review Low-4) The refusal line does not claim which kind of link it was, and stays
/// ASCII for stderr.
#[test]
fn the_outside_kb_line_names_no_kind_of_link() {
    let line = outside_kb_line("notes/a.md");
    assert!(line.contains("notes/a.md") && line.contains("outside the knowledge base"));
    assert!(!line.contains("directory"), "{line}");
    assert!(line.is_ascii(), "{line}");
}

#[test]
fn settle_cross_parser_rename_deletes_the_row_unless_the_reparse_updated_it() {
    let db = Database::open_in_memory().unwrap();
    // `delete_document` also deletes from `vec_chunks`, a table `open_in_memory` alone does
    // not create -- it exists at a fixed dimension, set up the same way a real run's
    // `verify_embedding_meta` call would (`db_with_384` in `db.rs`'s own tests does the
    // same for the same reason).
    db.verify_embedding_meta("bge-small-en-v1.5", 384).unwrap();
    db.upsert_document("note.md", None, None, None, None, &[], None, "h1", 10)
        .unwrap();
    assert_eq!(db.document_count().unwrap(), 1);

    settle_cross_parser_rename(
        &db,
        "note.md",
        &SingleResult::Skipped {
            reason: "parse failed",
            frontmatter_unparsed: false,
        },
    )
    .unwrap();
    assert_eq!(
        db.document_count().unwrap(),
        0,
        "Skipped is not Updated, so the stale row must be dropped"
    );

    db.upsert_document("note.md", None, None, None, None, &[], None, "h1", 10)
        .unwrap();
    settle_cross_parser_rename(&db, "note.md", &SingleResult::Refused).unwrap();
    assert_eq!(
        db.document_count().unwrap(),
        0,
        "Refused is not Updated either"
    );

    db.upsert_document("note.md", None, None, None, None, &[], None, "h1", 10)
        .unwrap();
    settle_cross_parser_rename(
        &db,
        "note.md",
        &SingleResult::Updated {
            chunks: 1,
            frontmatter_unparsed: false,
        },
    )
    .unwrap();
    assert_eq!(
        db.document_count().unwrap(),
        1,
        "Updated means the new parser wrote a fresh row -- it must be left alone"
    );
}

#[test]
fn test_documents_to_delete_retains_skipped_paths() {
    // DB に a.md / b.md / c.md。visited = {a.md} (今回 index), skipped = {b.md}
    // (read 失敗 or size skip)。c.md だけが「disk から消えた」= 削除対象。
    let all_db: Vec<String> = ["a.md", "b.md", "c.md"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let visited: HashSet<String> = ["a.md".to_string()].into_iter().collect();
    let skipped: HashSet<String> = ["b.md".to_string()].into_iter().collect();
    let to_delete = documents_to_delete(&all_db, &visited, &skipped);
    assert_eq!(
        to_delete,
        vec!["c.md".to_string()],
        "skipped path must be retained"
    );
}

/// AU-06 (codex P2): `[parsers].enabled` を狭めた後に残る行を数えられること。
/// `.xls` を取り下げたので、旧 index を持ったまま upgrade した人がこの状態に入る。
#[test]
fn unregistered_extensions_are_reported_but_not_deleted() {
    let registry =
        crate::parser::Registry::from_enabled(&["md".to_string(), "xlsx".to_string()]).unwrap();
    let all_db: Vec<String> = [
        "notes/a.md",
        "book.xlsx",
        "legacy/old.xls", // registry から外れた拡張子
        "REPORT.XLSX",    // 大文字でも registered (AU-02: 照合は case-insensitive)
        "no_extension",   // 拡張子なしも「載っていない」扱い
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();

    let stale = paths_with_unregistered_extension(&all_db, &registry);
    assert_eq!(
        stale,
        vec!["legacy/old.xls".to_string(), "no_extension".to_string()],
        "only paths the registry cannot serve should be reported"
    );

    // 「報告するだけ」であること: 入力は変えない (呼び出し側が warn するのみ)。
    assert_eq!(all_db.len(), 5, "the caller's list must not be mutated");

    // feature-50: resource surface が advertise してよい path は、この報告の
    // ちょうど補集合でなければならない。ずれると `resources/list` が出した
    // `kb://doc/...` を `resources/read` が拒否する (= codex P2 で見つかった形)。
    let servable: Vec<String> = all_db
        .iter()
        .filter(|p| extension_is_registered(p, &registry))
        .cloned()
        .collect();
    assert_eq!(
        servable,
        vec![
            "notes/a.md".to_string(),
            "book.xlsx".to_string(),
            "REPORT.XLSX".to_string()
        ],
        "servable must be exactly what was not reported as unregistered"
    );
    assert_eq!(
        servable.len() + stale.len(),
        all_db.len(),
        "the two views must partition the index, with nothing in both or neither"
    );
}

#[test]
fn test_documents_to_delete_empty_skipped_deletes_unvisited() {
    // skipped 空 = 従来挙動: visited に無い DB path は削除。
    let all_db: Vec<String> = ["a.md", "gone.md"].iter().map(|s| s.to_string()).collect();
    let visited: HashSet<String> = ["a.md".to_string()].into_iter().collect();
    let skipped: HashSet<String> = HashSet::new();
    let to_delete = documents_to_delete(&all_db, &visited, &skipped);
    assert_eq!(to_delete, vec!["gone.md".to_string()]);
}

#[test]
fn test_extract_category_topic_deep_path() {
    let (cat, topic) = extract_category_topic("deep-dive/chromadb/overview.md");
    assert_eq!(cat.as_deref(), Some("deep-dive"));
    assert_eq!(topic.as_deref(), Some("chromadb"));
}

#[test]
fn test_extract_category_topic_shallow_path() {
    let (cat, topic) = extract_category_topic("ai-news/2026-04-16.md");
    assert_eq!(cat.as_deref(), Some("ai-news"));
    assert_eq!(topic, None);
}

#[test]
fn test_extract_category_topic_root_file() {
    let (cat, topic) = extract_category_topic("index.md");
    assert_eq!(cat, None);
    assert_eq!(topic, None);
}

#[test]
fn test_extract_category_topic_very_deep_path() {
    let (cat, topic) = extract_category_topic("tech-watch/anthropic/subdir/2026-04-16.md");
    assert_eq!(cat.as_deref(), Some("tech-watch"));
    assert_eq!(topic.as_deref(), Some("anthropic"));
}

#[test]
fn test_sha256_hex_deterministic() {
    let hash1 = sha256_hex("hello world");
    let hash2 = sha256_hex("hello world");
    assert_eq!(hash1, hash2);
    // Known SHA-256 of "hello world"
    assert_eq!(
        hash1,
        "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9"
    );
}

#[test]
fn test_sha256_hex_different_content() {
    let hash1 = sha256_hex("hello");
    let hash2 = sha256_hex("world");
    assert_ne!(hash1, hash2);
}

#[test]
fn test_sha256_hex_bytes_matches_string_hash_for_utf8() {
    // read_to_string は無変換 (CRLF/BOM 保持) なので、UTF-8 ファイルの
    // raw バイト hash = 旧文字列 hash。既存 DB の再 index 暴発を防ぐ要。
    for s in [
        "hello world",
        "日本語テキスト",
        "line1\r\nline2\n",
        "\u{feff}bom-prefixed",
    ] {
        assert_eq!(
            sha256_hex(s),
            sha256_hex_bytes(s.as_bytes()),
            "mismatch for {s:?}"
        );
    }
}

// -----------------------------------------------------------------------
// collect_source_files
// -----------------------------------------------------------------------

struct TmpDir(std::path::PathBuf);
impl Drop for TmpDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn mk_tmp(prefix: &str) -> TmpDir {
    let p = crate::test_support::unique_temp_path(&format!("groove-idxtest-{prefix}"));
    std::fs::create_dir_all(&p).unwrap();
    TmpDir(p)
}

fn write_file(dir: &std::path::Path, rel: &str, content: &str) {
    let full = dir.join(rel);
    if let Some(parent) = full.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(full, content).unwrap();
}

/// (feature-49) The exclusion argument `collect_source_files` now takes.
/// Built with `load`, so these tests go through the production path; none
/// of these scratch directories has a `.grooveignore`, so what comes out is
/// the `exclude_dirs`-only rule set they were passing before.
fn excl(dir: &std::path::Path, dirs: &[&str]) -> crate::exclusion::ExclusionRules {
    crate::exclusion::ExclusionRules::load(dir, dirs.iter().map(|s| s.to_string()).collect())
}

#[test]
fn test_collect_source_files_md_only_by_default() {
    let tmp = mk_tmp("mdonly");
    write_file(&tmp.0, "a.md", "# A");
    write_file(&tmp.0, "b.txt", "plain b");
    write_file(&tmp.0, "sub/c.md", "# C");
    write_file(&tmp.0, "ignore.rst", "rst");

    let reg = Registry::defaults(); // md only
    let files = collect_source_files(&tmp.0, &reg, &excl(&tmp.0, &[".obsidian"])).unwrap();
    let rels: Vec<String> = files
        .iter()
        .map(|p| {
            p.strip_prefix(&tmp.0)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/")
        })
        .collect();
    assert!(rels.contains(&"a.md".to_string()));
    assert!(rels.contains(&"sub/c.md".to_string()));
    assert!(!rels.iter().any(|r| r.ends_with(".txt")));
    assert!(!rels.iter().any(|r| r.ends_with(".rst")));
}

/// KB-relative, slash-separated paths for the assertions below.
fn rels_under(kb: &std::path::Path, files: &[std::path::PathBuf]) -> Vec<String> {
    files
        .iter()
        .map(|p| {
            p.strip_prefix(kb)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/")
        })
        .collect()
}

/// The watcher hands in the directory that just appeared, not the KB root.
/// If the walk ignored `start` and swept the whole KB, every directory
/// event would re-embed the entire knowledge base.
#[test]
fn collect_source_files_under_returns_only_the_subtree() {
    let tmp = mk_tmp("under-subtree");
    write_file(&tmp.0, "root.md", "# root");
    write_file(&tmp.0, "fresh/a.md", "# a");
    write_file(&tmp.0, "fresh/deep/b.md", "# b");
    write_file(&tmp.0, "other/c.md", "# c");

    let reg = Registry::defaults();
    let files =
        collect_source_files_under(&tmp.0, &tmp.0.join("fresh"), &reg, &excl(&tmp.0, &[])).unwrap();

    assert_eq!(
        rels_under(&tmp.0, &files),
        vec!["fresh/a.md".to_string(), "fresh/deep/b.md".to_string()],
        "only the subtree handed in, and all of it"
    );
}

/// Exclusion answers relative to the **KB root** even though the walk
/// starts below it. Keyed to `start` instead, a `.grooveignore` line like
/// `fresh/skip/` would stop matching the moment the watcher rather than the
/// full walk was the one asking — the two would then disagree about what
/// belongs in the index, which is the drift AU-03 and BU-19 already caused
/// twice.
#[test]
fn collect_source_files_under_keeps_exclusion_keyed_to_the_kb_root() {
    let tmp = mk_tmp("under-excl");
    write_file(&tmp.0, "fresh/keep.md", "# keep");
    write_file(&tmp.0, "fresh/skip/hidden.md", "# hidden");
    // Written before the rules are loaded; `excl` reads the file.
    std::fs::write(tmp.0.join(".grooveignore"), "fresh/skip/\n").unwrap();

    let reg = Registry::defaults();
    let files =
        collect_source_files_under(&tmp.0, &tmp.0.join("fresh"), &reg, &excl(&tmp.0, &[])).unwrap();

    assert_eq!(
        rels_under(&tmp.0, &files),
        vec!["fresh/keep.md".to_string()],
        "a KB-root-relative ignore pattern must still bite inside a subtree walk"
    );
}

/// `start` is filtered like any other entry. A `node_modules/` that appears
/// under a watched KB is exactly the case AU-03 was about, and the watcher
/// reaches this function before any other check.
#[test]
fn collect_source_files_under_refuses_an_excluded_start() {
    let tmp = mk_tmp("under-excluded-start");
    write_file(&tmp.0, "node_modules/pkg/readme.md", "# nope");

    let reg = Registry::defaults();
    let files = collect_source_files_under(
        &tmp.0,
        &tmp.0.join("node_modules"),
        &reg,
        &excl(&tmp.0, &[]),
    )
    .unwrap();

    assert!(
        files.is_empty(),
        "handing in an excluded directory must yield nothing, got {files:?}"
    );
}

/// (AX-35) The directory the walk starts at is not followed when it is a symlink. The
/// watcher hands in the directory an event named, and a KB writer can swap it for a link to
/// a directory outside the KB between the event and the walk. walkdir follows the **root**
/// regardless of `follow_links(false)` unless told otherwise (`follow_root_links` defaults
/// to `true`), so without that the walk would collect the outside files under KB-relative
/// names.
///
/// On Windows the link is a directory junction: a symlink there needs Developer Mode or an
/// elevated token, while `mklink /J` needs neither, so the junction is the form a KB writer
/// can actually make (the same reasoning as the `grammar_dir` junction test in
/// [`crate::config`]). std reports a junction as a symlink, and so does walkdir. No std API
/// creates one, hence `mklink /J` through the shell; it is asserted rather than skipped.
#[test]
fn collect_source_files_under_does_not_follow_a_symlinked_start() {
    let kb = mk_tmp("under-symlinked-start-kb");
    let outside = mk_tmp("under-symlinked-start-out");
    write_file(&outside.0, "pay.md", "# outside the knowledge base");
    write_file(&kb.0, "real.md", "# real");
    let link = kb.0.join("fresh");

    #[cfg(unix)]
    std::os::unix::fs::symlink(&outside.0, &link).expect("symlinks need no privilege on unix");
    #[cfg(windows)]
    {
        let made = std::process::Command::new("cmd")
            .args([
                "/c",
                "mklink",
                "/J",
                &link.display().to_string(),
                &outside.0.display().to_string(),
            ])
            .stdout(std::process::Stdio::null())
            .status()
            .expect("mklink must be runnable");
        assert!(
            made.success(),
            "mklink /J needs no elevation and must succeed"
        );
    }
    assert!(
        std::fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink(),
        "premise: the start is a link"
    );

    let reg = Registry::defaults();
    let files = collect_source_files_under(&kb.0, &link, &reg, &excl(&kb.0, &[])).unwrap();
    assert!(
        files.is_empty(),
        "a symlinked start must not be walked into, got {files:?}"
    );
}

/// The full walk is the subtree walk started at the root. Pinning the
/// delegation keeps a future edit from growing a second walk body — the
/// whole reason `collect_source_files_under` exists rather than a copy.
#[test]
fn collect_source_files_is_the_subtree_walk_started_at_the_root() {
    let tmp = mk_tmp("under-equals-full");
    write_file(&tmp.0, "a.md", "# a");
    write_file(&tmp.0, "sub/b.md", "# b");
    write_file(&tmp.0, "sub/deep/c.md", "# c");

    let reg = Registry::defaults();
    let rules = excl(&tmp.0, &[]);
    assert_eq!(
        collect_source_files(&tmp.0, &reg, &rules).unwrap(),
        collect_source_files_under(&tmp.0, &tmp.0, &reg, &rules).unwrap()
    );
}

#[test]
fn test_collect_source_files_md_and_txt_opt_in() {
    let tmp = mk_tmp("mdtxt");
    write_file(&tmp.0, "a.md", "# A");
    write_file(&tmp.0, "b.txt", "plain");
    write_file(&tmp.0, "ignore.rst", "rst");

    let reg = Registry::from_enabled(&["md".into(), "txt".into()]).unwrap();
    let files = collect_source_files(&tmp.0, &reg, &excl(&tmp.0, &[".obsidian"])).unwrap();
    let rels: Vec<String> = files
        .iter()
        .map(|p| {
            p.strip_prefix(&tmp.0)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/")
        })
        .collect();
    assert!(rels.contains(&"a.md".to_string()));
    assert!(rels.contains(&"b.txt".to_string()));
    assert!(!rels.iter().any(|r| r.ends_with(".rst")));
}

#[test]
fn test_collect_source_files_skips_obsidian() {
    let tmp = mk_tmp("obsidian");
    write_file(&tmp.0, "keep.md", "# keep");
    write_file(&tmp.0, ".obsidian/workspace.md", "# should be skipped");
    write_file(&tmp.0, ".obsidian/nested/evil.md", "# skip too");

    let reg = Registry::defaults();
    let files = collect_source_files(&tmp.0, &reg, &excl(&tmp.0, &[".obsidian"])).unwrap();
    let rels: Vec<String> = files
        .iter()
        .map(|p| {
            p.strip_prefix(&tmp.0)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/")
        })
        .collect();
    assert_eq!(rels, vec!["keep.md".to_string()]);
}

#[test]
fn test_collect_source_files_case_insensitive_extension() {
    let tmp = mk_tmp("case");
    write_file(&tmp.0, "lower.md", "# lower");
    write_file(&tmp.0, "UPPER.MD", "# upper");
    write_file(&tmp.0, "note.TXT", "txt");

    let reg = Registry::from_enabled(&["md".into(), "txt".into()]).unwrap();
    let files = collect_source_files(&tmp.0, &reg, &excl(&tmp.0, &[".obsidian"])).unwrap();
    assert_eq!(files.len(), 3, "should match regardless of case: {files:?}");
}

#[test]
fn test_collect_source_files_deterministic_ordering() {
    let tmp = mk_tmp("sort");
    write_file(&tmp.0, "zzz.md", "z");
    write_file(&tmp.0, "aaa.md", "a");
    write_file(&tmp.0, "mmm.md", "m");

    let reg = Registry::defaults();
    let f1 = collect_source_files(&tmp.0, &reg, &excl(&tmp.0, &[".obsidian"])).unwrap();
    let f2 = collect_source_files(&tmp.0, &reg, &excl(&tmp.0, &[".obsidian"])).unwrap();
    assert_eq!(f1, f2);
    // First one should be aaa
    assert!(
        f1[0]
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("aaa")
    );
}

// -----------------------------------------------------------------------
// F-62: hardcoded denylist (.git / .svn / node_modules) is always applied
// as a fail-safe alongside user `exclude_dirs` (union semantics).
// -----------------------------------------------------------------------

/// (BU-19) Directory exclusion matches case-insensitively.
///
/// On Windows and macOS `.GIT` and `.git` name the same directory, so an
/// exact-match denylist is one a repository can walk straight past — and
/// the hardcoded list exists precisely as a fail-safe. The user list has
/// the same problem in the other direction: `exclude_dirs = ["build"]`
/// missed a directory that happened to be created as `Build`.
///
/// The case is checked here rather than via the filesystem so the test
/// means the same thing on Linux, where the two directories really are
/// distinct and the exclusion is a deliberate widening.
#[test]
fn dir_exclusion_ignores_case() {
    for name in [".GIT", ".Git", "NODE_MODULES", "Node_Modules", ".SVN"] {
        assert!(
            is_hardcoded_excluded(name),
            "{name} must hit the hardcoded denylist — on a case-insensitive \
                 filesystem it is the very directory the list names"
        );
    }
    for name in [".gitignore", "git", "node_modules_old", "svn"] {
        assert!(
            !is_hardcoded_excluded(name),
            "{name} is a different directory and must not be excluded"
        );
    }

    // Configured names are arbitrary user input, so folding has to cover
    // more than ASCII (codex P2 on PR #141). Normalization is explicitly
    // out of scope: the precomposed and decomposed spellings of `é` are
    // different strings and stay that way.
    let unicode = vec!["résumé".to_string(), "Ünterlagen".to_string()];
    for name in ["RÉSUMÉ", "Résumé", "résumé", "ÜNTERLAGEN", "ünterlagen"] {
        assert!(
            is_user_excluded_dir(name, &unicode),
            "{name} must match a configured non-ASCII exclusion"
        );
    }
    assert!(
        !is_user_excluded_dir("resume", &unicode),
        "an unaccented name is a different directory, not a case variant"
    );

    // Greek final sigma: `to_lowercase` is context-dependent, so `ΟΣ`
    // lowercases to `ος` while a configured `οσ` stays `οσ`. Folding the
    // final form closes that (codex P2, round 3).
    let greek = vec!["οσ".to_string()];
    for name in ["ΟΣ", "Οσ", "οΣ", "ος"] {
        assert!(
            is_user_excluded_dir(name, &greek),
            "{name} is a case variant of the configured οσ and must match"
        );
    }

    // And the limit this stops at, asserted so it is a decision rather
    // than a surprise: lowercase mapping is not full case folding, so `ß`
    // and `SS` remain different directories.
    let sharp_s = vec!["straße".to_string()];
    assert!(
        !is_user_excluded_dir("STRASSE", &sharp_s),
        "documented limit: `ß` lowercases to itself, so only full Unicode \
             case folding would match STRASSE — if this ever starts passing, \
             the doc comment on is_user_excluded_dir needs updating too"
    );
    assert!(
        is_user_excluded_dir("STRASSE", &["strasse".to_string()]),
        "the plain-ASCII spelling still folds normally"
    );

    let tmp = mk_tmp("excludecase");
    write_file(&tmp.0, "Build/inside.md", "# inside");
    write_file(&tmp.0, "normal.md", "# normal");
    let reg = Registry::defaults();
    let files = collect_source_files(&tmp.0, &reg, &excl(&tmp.0, &["build"])).unwrap();
    let names: Vec<String> = files
        .iter()
        .map(|p| p.file_name().unwrap().to_string_lossy().to_string())
        .collect();
    assert!(
        names.contains(&"normal.md".to_string()),
        "normal.md must be kept, got: {names:?}"
    );
    assert!(
        !names.iter().any(|n| n == "inside.md"),
        "`exclude_dirs = [\"build\"]` must also skip a directory spelled \
             `Build`, got: {names:?}"
    );
}

/// (BU-20) hardlink は symlink と同じ脅威なのに、walk からは見えない
/// (`follow_links(false)` に辿るべき link が無い)。full index からも
/// 落ちることを pin する。**既に index 済みの文書が 2 つ目の名前を得た
/// 場合も落ちる** = 削除 pass が「集められなかった = 消えた」と扱うので、
/// index から取り除かれる。これは承認済みの代償 (log で説明される)。
#[test]
fn test_collect_source_files_skips_hard_links() {
    let tmp = mk_tmp("hardlink");
    write_file(&tmp.0, "normal.md", "# normal");
    write_file(&tmp.0, "secret.md", "# secret\nssh-rsa AAAA...");
    std::fs::hard_link(tmp.0.join("secret.md"), tmp.0.join("notes.md"))
        .expect("hard links need no privilege");

    let reg = Registry::defaults();
    let files = collect_source_files(&tmp.0, &reg, &excl(&tmp.0, &[])).unwrap();
    let names: Vec<String> = files
        .iter()
        .map(|p| p.file_name().unwrap().to_string_lossy().to_string())
        .collect();
    assert!(
        names.contains(&"normal.md".to_string()),
        "a file with one name must still be indexed, got: {names:?}"
    );
    assert!(
        !names.iter().any(|n| n == "notes.md" || n == "secret.md"),
        "both names of a hard-linked file must be skipped, got: {names:?}"
    );
}

/// `exclude_dirs = []` (= "walk everything") でも `.git/` 配下は skip。
#[test]
fn test_collect_source_files_skips_dot_git_even_with_empty_exclude_dirs() {
    let tmp = mk_tmp("hardenedgit");
    write_file(&tmp.0, ".git/inside.md", "# git inside");
    write_file(&tmp.0, "normal.md", "# normal");

    let reg = Registry::defaults();
    let files = collect_source_files(&tmp.0, &reg, &excl(&tmp.0, &[])).unwrap();
    let names: Vec<String> = files
        .iter()
        .map(|p| p.file_name().unwrap().to_string_lossy().to_string())
        .collect();
    assert!(
        names.contains(&"normal.md".to_string()),
        "normal.md must be kept, got: {names:?}"
    );
    assert!(
        !names.iter().any(|n| n == "inside.md"),
        ".git/inside.md must be skipped by hardcoded denylist, got: {names:?}"
    );
}

/// User が `exclude_dirs` を default 上書きしつつ `.git` を含め忘れた case
/// (= 本 cycle の主たる fail-safe shape)。
#[test]
fn test_collect_source_files_skips_dot_git_when_user_exclude_dirs_overrides_default() {
    let tmp = mk_tmp("hardenedoverride");
    write_file(&tmp.0, ".git/inside.md", "# git inside");
    write_file(&tmp.0, "normal.md", "# normal");

    let reg = Registry::defaults();
    // User overrides DEFAULT_EXCLUDE_DIRS with their own list, forgetting
    // to re-list `.git`. Hardcoded denylist still skips `.git/inside.md`.
    let files = collect_source_files(&tmp.0, &reg, &excl(&tmp.0, &["custom"])).unwrap();
    let names: Vec<String> = files
        .iter()
        .map(|p| p.file_name().unwrap().to_string_lossy().to_string())
        .collect();
    assert!(names.contains(&"normal.md".to_string()));
    assert!(
        !names.iter().any(|n| n == "inside.md"),
        ".git/inside.md must remain skipped despite user override, got: {names:?}"
    );
}

/// Hardcoded denylist + user `exclude_dirs` の union semantics 確認。
#[test]
fn test_collect_source_files_union_of_hardcoded_and_user_excludes() {
    let tmp = mk_tmp("hardenedunion");
    write_file(&tmp.0, ".git/git_inside.md", "# git");
    write_file(&tmp.0, ".obsidian/note.md", "# obsidian note");
    write_file(&tmp.0, "keep.md", "# keep");

    let reg = Registry::defaults();
    // User explicitly excludes `.obsidian`. Hardcoded denylist also
    // skips `.git`. Both must be skipped (union).
    let files = collect_source_files(&tmp.0, &reg, &excl(&tmp.0, &[".obsidian"])).unwrap();
    let names: Vec<String> = files
        .iter()
        .map(|p| p.file_name().unwrap().to_string_lossy().to_string())
        .collect();
    assert!(
        names.contains(&"keep.md".to_string()),
        "keep.md must be kept, got: {names:?}"
    );
    assert!(
        !names.iter().any(|n| n == "git_inside.md"),
        "hardcoded .git skip failed: {names:?}"
    );
    assert!(
        !names.iter().any(|n| n == "note.md"),
        "user .obsidian skip failed: {names:?}"
    );
}

/// (feature-49) `.grooveignore` を後から足したファイルは **walk から消える**。
///
/// これが index からの退場そのものになる: 削除 pass は「集められなかった =
/// 消えた」と扱うので、次の full index で DB の行も落ちる。hardlink refusal
/// (BU-20、`test_collect_source_files_skips_hard_links`) と同じ経路で、
/// **逆に** `scan_disk_entries` の refusal は `skipped` に入れて既存行を
/// 守る — 除外は「もう KB の一部ではない」、refusal は「今回は読めなかった」
/// なので、扱いが違うのは意図的。
#[test]
fn test_collect_source_files_drops_newly_ignored_files() {
    let tmp = mk_tmp("newlyignored");
    write_file(&tmp.0, "keep.md", "# keep");
    write_file(&tmp.0, "drafts/wip.md", "# wip");
    let reg = Registry::defaults();

    let before = collect_source_files(&tmp.0, &reg, &excl(&tmp.0, &[])).unwrap();
    assert_eq!(
        before.len(),
        2,
        "both files start out collected: {before:?}"
    );

    write_file(&tmp.0, ".grooveignore", "drafts/\n");
    let after = collect_source_files(&tmp.0, &reg, &excl(&tmp.0, &[])).unwrap();
    let names: Vec<String> = after
        .iter()
        .map(|p| p.file_name().unwrap().to_string_lossy().to_string())
        .collect();
    assert_eq!(
        names,
        vec!["keep.md".to_string()],
        "a newly ignored file must leave the walk, which is what removes it \
             from the index on the next run, got: {names:?}"
    );

    std::fs::remove_file(tmp.0.join(".grooveignore")).unwrap();
    let restored = collect_source_files(&tmp.0, &reg, &excl(&tmp.0, &[])).unwrap();
    assert_eq!(
        restored.len(),
        2,
        "and removing the ignore file brings it back: {restored:?}"
    );
}

/// The ignore file is not itself a document: it has no registered
/// extension, so it never reaches the index whether or not it names itself.
#[test]
fn test_collect_source_files_never_collects_the_ignore_file() {
    let tmp = mk_tmp("ignorefileitself");
    write_file(&tmp.0, "a.md", "# a");
    write_file(&tmp.0, ".grooveignore", "*.tmp.md\n");
    let reg = Registry::from_enabled(&["md".into(), "txt".into()]).unwrap();
    let files = collect_source_files(&tmp.0, &reg, &excl(&tmp.0, &[])).unwrap();
    let names: Vec<String> = files
        .iter()
        .map(|p| p.file_name().unwrap().to_string_lossy().to_string())
        .collect();
    assert_eq!(names, vec!["a.md".to_string()], "got: {names:?}");
}

#[test]
fn test_is_office_lock_file() {
    assert!(is_office_lock_file("~$report.docx")); // MS Office owner file
    assert!(is_office_lock_file("~$budget.xlsx"));
    assert!(is_office_lock_file(".~lock.report.docx#")); // LibreOffice lock
    assert!(!is_office_lock_file("report.docx"));
    assert!(!is_office_lock_file("notes.md"));
    assert!(!is_office_lock_file("~draft.md")); // ~$ で始まらない
}

#[test]
fn test_collect_source_files_skips_office_lock() {
    let tmp = mk_tmp("officelock");
    write_file(&tmp.0, "a.md", "# a");
    write_file(&tmp.0, "~$a.md", "owner file"); // ~$ prefix, md 拡張子
    write_file(&tmp.0, ".~lock.a.md#", "lo lock");
    let reg = Registry::defaults();
    let files = collect_source_files(&tmp.0, &reg, &excl(&tmp.0, &[])).unwrap();
    let names: Vec<String> = files
        .iter()
        .map(|p| p.file_name().unwrap().to_string_lossy().to_string())
        .collect();
    assert_eq!(
        names,
        vec!["a.md".to_string()],
        "lock files must be skipped, got {names:?}"
    );
}

/// Removes a scratch directory through the verbatim path `canonicalize`
/// returns, the only spelling under which `CON.md` or `dir.` is the entry
/// on disk rather than a device or a trimmed name. Declare it after the
/// [`TmpDir`] it covers so it is dropped first.
#[cfg(windows)]
struct VerbatimCleanup(std::path::PathBuf);
#[cfg(windows)]
impl Drop for VerbatimCleanup {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A knowledge base holding, besides `ok.md`, names that only the verbatim
/// prefix lets exist: a device name, a directory ending in a dot (with two
/// files under it), one ending in a space, and two files whose trailing
/// dot or space leaves them with no registered extension. Returned with
/// its cleanup guard.
#[cfg(windows)]
fn kb_with_unspellable_names(prefix: &str) -> (TmpDir, VerbatimCleanup, std::path::PathBuf) {
    let tmp = mk_tmp(prefix);
    let kb = tmp.0.canonicalize().unwrap();
    assert!(
        kb.to_string_lossy().starts_with(r"\\?\"),
        "the fixture needs the verbatim prefix: {}",
        kb.display()
    );
    let cleanup = VerbatimCleanup(kb.clone());
    // Backslashes: under `\\?\` a `/` is not a separator.
    for rel in [
        "ok.md",
        "CON.md",
        r"dir.\x.md",
        r"dir.\z.md",
        r"sp \y.md",
        "b.md.",
        "b.md ",
    ] {
        write_file(&kb, rel, "# x\n");
        assert!(
            std::fs::metadata(kb.join(rel)).is_ok_and(|m| m.is_file()),
            "the fixture must hold a real file at {rel:?}"
        );
    }
    (tmp, cleanup, kb)
}

/// (ADR-0023, AW-43) The walk leaves out what
/// [`crate::resources::doc_is_addressable`] refuses: `CON.md` at file
/// level, `dir.` and `sp ` at directory level
/// with everything under them. `b.md.` and `b.md ` never reach the check
/// -- `Path::extension` reads them as `""` and `"md "` -- so they are not
/// counted, and were never indexed before this either.
///
/// `dir.` holds two files, so the count tells a pruned directory (one
/// entry, 3 in all) from one whose files were refused one by one (two
/// entries, 4 in all).
#[cfg(windows)]
#[test]
fn test_collect_source_files_leaves_out_names_win32_cannot_spell() {
    let (_tmp, _cleanup, kb) = kb_with_unspellable_names("unspellable");
    let reg = Registry::defaults();
    let got = collect_source_files_counted(&kb, &kb, &reg, &excl(&kb, &[])).unwrap();
    let rels: Vec<String> = got
        .files
        .iter()
        .map(|p| crate::exclusion::rel_key(&kb, p))
        .collect();
    assert_eq!(rels, vec!["ok.md".to_string()]);
    assert_eq!(
        got.unspellable, 3,
        "CON.md, dir. and sp -- each directory once, not once per file under it"
    );
    assert_eq!(
        collect_source_files(&kb, &reg, &excl(&kb, &[])).unwrap(),
        got.files,
        "the plain walk is the counted walk minus the count"
    );
}

/// The watcher walks a directory that just appeared from that directory
/// (`dispatch_new_directory` of [`crate::watcher`]). Started at `dir.`, the walk yields nothing
/// and counts the directory once, not its two files.
#[cfg(windows)]
#[test]
fn test_collect_source_files_from_a_start_directory_it_cannot_spell() {
    let (_tmp, _cleanup, kb) = kb_with_unspellable_names("unspellable-start");
    let reg = Registry::defaults();
    let got = collect_source_files_counted(&kb, &kb.join("dir."), &reg, &excl(&kb, &[])).unwrap();
    assert!(got.files.is_empty(), "{:?}", got.files);
    assert_eq!(got.unspellable, 1);
}

/// A walk started under a spelling that is not below `kb_path` (the
/// watcher hands in raw event paths) judges no name, at either level:
/// there is no relative path to judge, and the whole-path fallback would
/// refuse even `ok.md`. Such paths are left to the watcher's own
/// relativizing.
#[cfg(windows)]
#[test]
fn test_collect_source_files_judges_no_name_outside_kb_spelling() {
    let (tmp, _cleanup, kb) = kb_with_unspellable_names("unspellable-raw");
    // `tmp.0` is the same directory as `kb` without the verbatim prefix,
    // so nothing under `kb` strips it.
    let reg = Registry::defaults();
    let got = collect_source_files_counted(&tmp.0, &kb, &reg, &excl(&tmp.0, &[])).unwrap();
    let rels: Vec<String> = got
        .files
        .iter()
        .map(|p| crate::exclusion::rel_key(&kb, p))
        .collect();
    assert_eq!(
        rels,
        vec![
            "CON.md".to_string(),
            "dir./x.md".to_string(),
            "dir./z.md".to_string(),
            "ok.md".to_string(),
            "sp /y.md".to_string(),
        ],
        "an ordinary name is collected, and no directory is pruned"
    );
    assert_eq!(got.unspellable, 0);
}

/// On every platform an ordinary knowledge base counts nothing.
#[test]
fn test_collect_source_files_counts_nothing_in_an_ordinary_kb() {
    let tmp = mk_tmp("unspellable-none");
    write_file(&tmp.0, "a.md", "# a");
    write_file(&tmp.0, "dir.x/b.md", "# b");
    write_file(&tmp.0, ".hidden/c.md", "# c");
    let reg = Registry::defaults();
    let got = collect_source_files_counted(&tmp.0, &tmp.0, &reg, &excl(&tmp.0, &[])).unwrap();
    assert_eq!(got.files.len(), 3, "{:?}", got.files);
    assert_eq!(got.unspellable, 0);
}

/// (#660, spec 5.5) Both names of a hard-linked file come back in `left_out` as
/// [`SkipReason::HardLink`], spelled as [`index_rel_path_or_whole`] spells them, and neither
/// is collected.
#[test]
fn collect_lists_a_hard_link_as_left_out() {
    let tmp = mk_tmp("leftout-hardlink");
    write_file(&tmp.0, "normal.md", "# normal");
    write_file(&tmp.0, "a.md", "# a");
    std::fs::hard_link(tmp.0.join("a.md"), tmp.0.join("b.md"))
        .expect("hard links need no privilege");

    let reg = Registry::defaults();
    let got = collect_source_files_counted(&tmp.0, &tmp.0, &reg, &excl(&tmp.0, &[])).unwrap();
    let mut left: Vec<(String, SkipReason)> = got
        .left_out
        .iter()
        .map(|s| (s.path.clone(), s.reason))
        .collect();
    left.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(
        left,
        vec![
            (
                index_rel_path_or_whole(&tmp.0, &tmp.0.join("a.md")),
                SkipReason::HardLink
            ),
            (
                index_rel_path_or_whole(&tmp.0, &tmp.0.join("b.md")),
                SkipReason::HardLink
            ),
        ]
    );
    assert_eq!(left[0].0, "a.md");
    assert_eq!(left[1].0, "b.md");
    let names: Vec<String> = got
        .files
        .iter()
        .map(|p| p.file_name().unwrap().to_string_lossy().to_string())
        .collect();
    assert_eq!(names, vec!["normal.md".to_string()]);
    assert_eq!(got.unspellable, 0, "a hard link is not a name left out");
}

/// (#660, spec 5.5 / S6a) The names the walk cannot keep come back in `left_out`: `CON.md`
/// as a file, `dir.` and `sp ` as whole directories (one entry each, their files never
/// listed). `b.md.` / `b.md ` have no registered extension and never reach the check.
#[cfg(windows)]
#[test]
fn collect_lists_an_unspellable_name_and_dir_as_left_out() {
    let (_tmp, _cleanup, kb) = kb_with_unspellable_names("leftout-unspellable");
    let reg = Registry::defaults();
    let got = collect_source_files_counted(&kb, &kb, &reg, &excl(&kb, &[])).unwrap();
    let mut left: Vec<(String, SkipReason)> = got
        .left_out
        .iter()
        .map(|s| (s.path.clone(), s.reason))
        .collect();
    left.sort_by(|a, b| a.0.cmp(&b.0));
    let mut expected = vec![
        (
            index_rel_path_or_whole(&kb, &kb.join("CON.md")),
            SkipReason::UnspellableName { directory: false },
        ),
        (
            index_rel_path_or_whole(&kb, &kb.join("dir.")),
            SkipReason::UnspellableName { directory: true },
        ),
        (
            index_rel_path_or_whole(&kb, &kb.join("sp ")),
            SkipReason::UnspellableName { directory: true },
        ),
    ];
    expected.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(left, expected);
    assert_eq!(got.unspellable, 3);
    for absent in [r"dir.\x.md", r"dir.\z.md", r"sp \y.md", "b.md.", "b.md "] {
        let rel = index_rel_path_or_whole(&kb, &kb.join(absent));
        assert!(
            !left.iter().any(|(p, _)| *p == rel),
            "{rel:?} must not be listed: {left:?}"
        );
    }
}

/// A row stored for a name the walk no longer collects is gone after the
/// next full run: the sweep deletes what was neither visited nor skipped
/// (spec §2, no new code -- this pins the contract the design leans on).
#[test]
fn test_documents_to_delete_drops_a_row_the_walk_no_longer_collects() {
    let db_paths = vec!["CON.md".to_string(), "ok.md".to_string()];
    let visited: std::collections::HashSet<String> = ["ok.md".to_string()].into_iter().collect();
    let skipped: std::collections::HashSet<String> = std::collections::HashSet::new();
    assert_eq!(
        documents_to_delete(&db_paths, &visited, &skipped),
        vec!["CON.md".to_string()]
    );
}

// -----------------------------------------------------------------------
// scan_disk_entries
// -----------------------------------------------------------------------

#[test]
fn test_scan_disk_entries_read_fail_goes_to_skipped() {
    let tmp = mk_tmp("scanreadfail");
    write_file(&tmp.0, "ok.md", "# ok");
    // 実在しないファイルを source_files に混ぜる = fs::read 失敗を deterministic に誘発
    // (collect と read の間で消えた / lock された file の代理)。
    let missing = tmp.0.join("gone.md");
    let reg = Registry::defaults();
    let source = vec![tmp.0.join("ok.md"), missing];
    let scan = scan_disk_entries(
        &source,
        &tmp.0,
        &reg,
        crate::parser::MAX_RAW_BINARY_BYTES,
        crate::parser::MAX_RAW_TEXT_BYTES,
    );
    let entry_rels: Vec<&str> = scan.entries.iter().map(|e| e.rel.as_str()).collect();
    assert_eq!(entry_rels, vec!["ok.md"]);
    assert_eq!(scan.skipped, vec!["gone.md".to_string()]);
}

/// (BU-20) The walk refuses hard links, but it hands `scan_disk_entries` a
/// list of *paths* and the bytes are read here — so a file that became a
/// hard link in between has to be caught at the read. Passing the link in
/// `source_files` is exactly that situation: the collect step accepted it.
///
/// It joins `skipped`, not the silently-dropped set: the row already in the
/// database was indexed from bytes that were legitimate when they were
/// read, and evicting it is the walk's job on the next full run.
#[test]
fn test_scan_disk_entries_refuses_a_hard_link_that_appeared_after_the_walk() {
    let tmp = mk_tmp("scanhardlink");
    write_file(&tmp.0, "ok.md", "# ok");
    write_file(&tmp.0, "secret.txt", "ssh-rsa AAAA...");
    let linked = tmp.0.join("notes.md");
    std::fs::hard_link(tmp.0.join("secret.txt"), &linked).expect("hard links need no privilege");

    let reg = Registry::defaults();
    let source = vec![tmp.0.join("ok.md"), linked];
    let scan = scan_disk_entries(
        &source,
        &tmp.0,
        &reg,
        crate::parser::MAX_RAW_BINARY_BYTES,
        crate::parser::MAX_RAW_TEXT_BYTES,
    );

    let entry_rels: Vec<&str> = scan.entries.iter().map(|e| e.rel.as_str()).collect();
    assert_eq!(
        entry_rels,
        vec!["ok.md"],
        "the hard link's bytes must never reach an entry"
    );
    assert_eq!(scan.skipped, vec!["notes.md".to_string()]);
}

/// (local Codex on PR #388) The scan reads a frontmatter topic for the rename candidates
/// only, from the bytes it hashed, and records none for a candidate that declares none.
#[test]
fn scan_reads_the_frontmatter_topic_of_rename_candidates_only() {
    let tmp = mk_tmp("scantopics");
    write_file(&tmp.0, "a.md", "---\ntopic: x\n---\n\n# a\n");
    write_file(&tmp.0, "b.md", "---\ntopic: y\n---\n\n# b\n");
    write_file(&tmp.0, "c.txt", "plain text");
    let reg = Registry::defaults();
    let source = vec![tmp.0.join("a.md"), tmp.0.join("b.md"), tmp.0.join("c.txt")];
    let candidates = ["a.md", "c.txt"];
    let (scan, _) = scan_disk_entries_observed(
        &source,
        &tmp.0,
        &reg,
        crate::parser::MAX_RAW_BINARY_BYTES,
        crate::parser::MAX_RAW_TEXT_BYTES,
        &|rel, hash| {
            let entry_hash =
                sha256_hex_bytes(&std::fs::read(tmp.0.join(rel)).expect("read the fixture back"));
            assert_eq!(hash, entry_hash, "the candidate check sees the scan's hash");
            candidates.contains(&rel)
        },
        &mut || std::ops::ControlFlow::Continue(()),
    );
    let mut topics: Vec<(&str, Option<&str>)> = scan
        .frontmatter_topics
        .iter()
        .map(|(rel, topic)| (rel.as_str(), topic.as_deref()))
        .collect();
    topics.sort();
    assert_eq!(topics, [("a.md", Some("x")), ("c.txt", None)]);
}

/// (feature-60) The observer runs once per visited file, declined files included, and a
/// `Break` ends the scan right there with what it had so far -- so a stopped run can still
/// count the files it declined. [`scan_disk_entries`] is the same scan with an observer that
/// never stops it.
#[test]
fn scan_disk_entries_observed_calls_the_observer_per_file_and_stops_on_break() {
    let tmp = mk_tmp("scanobserved");
    write_file(&tmp.0, "a.md", "# a");
    write_file(&tmp.0, "b.md", "# b");
    write_file(&tmp.0, "c.md", "# c");
    let reg = Registry::defaults();
    // `0-gone.md` does not exist: the stat fails and the scan declines it.
    let source = vec![
        tmp.0.join("0-gone.md"),
        tmp.0.join("a.md"),
        tmp.0.join("b.md"),
        tmp.0.join("c.md"),
    ];

    let mut calls = 0;
    let (scan, flow) = scan_disk_entries_observed(
        &source,
        &tmp.0,
        &reg,
        crate::parser::MAX_RAW_BINARY_BYTES,
        crate::parser::MAX_RAW_TEXT_BYTES,
        &|_, _| false,
        &mut || {
            calls += 1;
            if calls == 2 {
                std::ops::ControlFlow::Break(())
            } else {
                std::ops::ControlFlow::Continue(())
            }
        },
    );
    assert!(flow.is_break());
    assert_eq!(
        calls, 2,
        "the declined file is a visit too, and nothing after the Break is scanned"
    );
    assert_eq!(scan.skipped, vec!["0-gone.md".to_string()]);
    let rels: Vec<&str> = scan.entries.iter().map(|e| e.rel.as_str()).collect();
    assert_eq!(rels, ["a.md"]);

    let full = scan_disk_entries(
        &source,
        &tmp.0,
        &reg,
        crate::parser::MAX_RAW_BINARY_BYTES,
        crate::parser::MAX_RAW_TEXT_BYTES,
    );
    let rels: Vec<&str> = full.entries.iter().map(|e| e.rel.as_str()).collect();
    assert_eq!(rels, ["a.md", "b.md", "c.md"]);
    assert_eq!(full.skipped, vec!["0-gone.md".to_string()]);
}

/// (feature-60) Review Focus 1. The loop visits the rename re-reads first and the rest
/// after, each group in the order the entries already have -- the walk's, which sorts
/// `PathBuf`s component by component. That order puts `a/b.md` before `a-b/x.md`,
/// although the string `"a-b/x.md"` sorts first; sorting by `rel` would reorder a run
/// that has no rename at all.
#[test]
fn processing_order_puts_forced_reparses_first_and_keeps_the_walk_order_in_each_group() {
    fn rels<'a>(order: &[&'a DiskEntry]) -> Vec<&'a str> {
        order.iter().map(|e| e.rel.as_str()).collect()
    }
    let entries = vec![
        mk_entry("a/b.md", "h1"),
        mk_entry("a-b/x.md", "h2"),
        mk_entry("c.md", "h3"),
        mk_entry("d.md", "h4"),
    ];

    assert_eq!(
        rels(&processing_order(&entries, &HashSet::new())),
        ["a/b.md", "a-b/x.md", "c.md", "d.md"],
        "no forced re-parse: the walk order, untouched"
    );

    let first: HashSet<String> = ["d.md".to_string(), "a-b/x.md".to_string()]
        .into_iter()
        .collect();
    assert_eq!(
        rels(&processing_order(&entries, &first)),
        ["a-b/x.md", "d.md", "a/b.md", "c.md"],
        "forced re-parses first, then the rest, each in the walk order"
    );
}

/// The other half of the same wiring: a plain file still reads, so the test
/// above is not passing because the read is broken for everything.
#[test]
fn test_read_for_index_returns_bytes_for_an_ordinary_file() {
    let tmp = mk_tmp("readforindex");
    write_file(&tmp.0, "a.md", "# A");
    let bytes = read_for_index(
        &tmp.0.join("a.md"),
        "a.md",
        crate::parser::MAX_RAW_TEXT_BYTES,
    )
    .expect("an ordinary file must not error")
    .0
    .expect("an ordinary file must not be refused");
    assert_eq!(bytes, b"# A");
}

#[test]
fn test_scan_disk_entries_binary_size_skip_goes_to_skipped() {
    let tmp = mk_tmp("scansize");
    // is_binary な拡張子 (pdf) を持つ registry を作れないので、cap を極小に
    // 渡して「size 超過」を誘発する。txt を binary 扱いにはできないため、この
    // test は cap パラメータ注入で size-skip 分岐を突く。ここでは md を通常読み、
    // 別途 binary 拡張子は PR-2 で結合テストする。cap の効果は次の assert で。
    write_file(&tmp.0, "small.md", "# tiny");
    let reg = Registry::defaults();
    // md は is_binary=false なので **binary** cap の対象外 = 1 にしても通る。
    let scan = scan_disk_entries(
        &[tmp.0.join("small.md")],
        &tmp.0,
        &reg,
        1,
        crate::parser::MAX_RAW_TEXT_BYTES,
    );
    assert_eq!(
        scan.entries.len(),
        1,
        "text files ignore the binary size cap"
    );
    assert!(scan.skipped.is_empty());
}

/// (BU-02) テキストにも cap がある。上の test と対になっていて、
/// 「binary cap は効かないが text cap は効く」を両側から挟む。
#[test]
fn test_scan_disk_entries_text_over_its_own_cap_goes_to_skipped() {
    let tmp = mk_tmp("scansizetext");
    write_file(&tmp.0, "big.md", "0123456789"); // 10 bytes
    let reg = Registry::defaults();
    let scan = scan_disk_entries(
        &[tmp.0.join("big.md")],
        &tmp.0,
        &reg,
        crate::parser::MAX_RAW_BINARY_BYTES,
        5,
    );
    assert!(
        scan.entries.is_empty(),
        "a text file over the text cap must not be read"
    );
    assert_eq!(scan.skipped, vec!["big.md".to_string()]);
}

/// codex P2 round 4. Refusing the file is only half of it: the row stays in
/// the index (§4.2 skip preserves), so the size recorded there is the last
/// one small enough to read while the file on disk is now one a read
/// refuses — and the resource surface would go on offering it. The scan
/// already measured the new length; it has to carry it out.
///
/// This is why that refusal is *not* in the class of "the file changed
/// after it was indexed, and a listing cannot know": here groove did know,
/// a moment ago, and threw the answer away.
#[test]
fn test_scan_disk_entries_reports_the_measured_size_of_what_it_refused() {
    let tmp = mk_tmp("scanoversize");
    write_file(&tmp.0, "big.md", "0123456789"); // 10 bytes, over the cap
    write_file(&tmp.0, "ok.md", "# h"); // 3 bytes, under it
    let reg = Registry::defaults();
    let scan = scan_disk_entries(
        &[tmp.0.join("big.md"), tmp.0.join("ok.md")],
        &tmp.0,
        &reg,
        crate::parser::MAX_RAW_BINARY_BYTES,
        5,
    );
    assert_eq!(
        scan.oversize,
        vec![("big.md".to_string(), 10)],
        "the refused file's real length must come back, and only that file's"
    );
}

/// (#660) Every skip carries its reason at the same index: entry `i` of
/// [`DiskScan::skipped`] and entry `i` of [`DiskScan::skipped_why`] are one file.
#[test]
fn scan_records_why_beside_each_skip() {
    let tmp = mk_tmp("scanwhy");
    write_file(&tmp.0, "big.md", "0123456789"); // 10 bytes, over the cap
    write_file(&tmp.0, "ok.md", "# h"); // 3 bytes, under it
    write_file(&tmp.0, "huge.md", "0123456789abcdef"); // 16 bytes, over it
    let reg = Registry::defaults();
    let scan = scan_disk_entries(
        &[
            tmp.0.join("big.md"),
            tmp.0.join("ok.md"),
            tmp.0.join("huge.md"),
        ],
        &tmp.0,
        &reg,
        crate::parser::MAX_RAW_BINARY_BYTES,
        5,
    );
    assert_eq!(
        scan.skipped,
        vec!["big.md".to_string(), "huge.md".to_string()]
    );
    assert_eq!(
        scan.skipped_why,
        vec![
            SkipReason::TooLarge {
                size: 10,
                limit: 5,
                key: LimitKey::MaxTextFileSize,
            },
            SkipReason::TooLarge {
                size: 16,
                limit: 5,
                key: LimitKey::MaxTextFileSize,
            },
        ],
        "one reason per skip, at the same index"
    );
}

/// (#660) The harness of
/// [`test_scan_disk_entries_refuses_a_hard_link_that_appeared_after_the_walk`]: the read's
/// refusal is named, not only counted.
#[test]
fn scan_names_a_hard_link_that_appeared_after_the_walk() {
    let tmp = mk_tmp("scanwhyhardlink");
    write_file(&tmp.0, "ok.md", "# ok");
    write_file(&tmp.0, "secret.txt", "ssh-rsa AAAA...");
    let linked = tmp.0.join("notes.md");
    std::fs::hard_link(tmp.0.join("secret.txt"), &linked).expect("hard links need no privilege");

    let reg = Registry::defaults();
    let source = vec![tmp.0.join("ok.md"), linked];
    let scan = scan_disk_entries(
        &source,
        &tmp.0,
        &reg,
        crate::parser::MAX_RAW_BINARY_BYTES,
        crate::parser::MAX_RAW_TEXT_BYTES,
    );
    assert_eq!(scan.skipped, vec!["notes.md".to_string()]);
    assert_eq!(scan.skipped_why, vec![SkipReason::HardLink]);
}

/// (#660) A file replaced by a named pipe after the walk is refused at the read
/// (`O_NONBLOCK` keeps the open from waiting) and named [`SkipReason::NotPlainFile`].
#[cfg(unix)]
#[test]
fn scan_names_a_fifo_not_plain_file() {
    use std::os::unix::ffi::OsStrExt;

    let tmp = mk_tmp("scanwhyfifo");
    write_file(&tmp.0, "pipe.md", "# was a note");
    let path = tmp.0.join("pipe.md");
    // The walk collected `pipe.md` as a file; it becomes a FIFO before the scan reads it.
    std::fs::remove_file(&path).unwrap();
    let c_path = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
    // SAFETY: a NUL-terminated path and a mode; the call touches nothing else.
    let rc = unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) };
    assert_eq!(rc, 0, "could not create the fifo to test against");

    let reg = Registry::defaults();
    let scan = scan_disk_entries(
        &[path],
        &tmp.0,
        &reg,
        crate::parser::MAX_RAW_BINARY_BYTES,
        crate::parser::MAX_RAW_TEXT_BYTES,
    );
    assert_eq!(scan.skipped, vec!["pipe.md".to_string()]);
    assert_eq!(scan.skipped_why, vec![SkipReason::NotPlainFile]);
}

/// (#660 T-L10) A file another process holds open without sharing it (an editor's lock)
/// is [`SkipReason::Unreadable`] with [`ReadFailure::InUse`], not a generic failure.
#[cfg(windows)]
#[test]
fn scan_names_a_file_held_without_sharing_in_use() {
    use std::os::windows::fs::OpenOptionsExt;

    let tmp = mk_tmp("scanwhyinuse");
    write_file(&tmp.0, "held.md", "# held");
    let path = tmp.0.join("held.md");
    let _held = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(0)
        .open(&path)
        .expect("open the file without sharing");

    let reg = Registry::defaults();
    let scan = scan_disk_entries(
        std::slice::from_ref(&path),
        &tmp.0,
        &reg,
        crate::parser::MAX_RAW_BINARY_BYTES,
        crate::parser::MAX_RAW_TEXT_BYTES,
    );
    assert_eq!(scan.skipped, vec!["held.md".to_string()]);
    assert_eq!(
        scan.skipped_why,
        vec![SkipReason::Unreadable {
            cause: ReadFailure::InUse,
        }]
    );
}

/// (#660) A file the OS refuses to open is [`SkipReason::Unreadable`] with
/// [`ReadFailure::PermissionDenied`]. root reads
/// through `chmod 000`, so the test has nothing to show there and returns.
#[cfg(unix)]
#[test]
fn scan_names_an_unreadable_file_permission_denied() {
    use std::os::unix::fs::PermissionsExt;

    // SAFETY: geteuid has no preconditions and cannot fail.
    if unsafe { libc::geteuid() } == 0 {
        return;
    }
    let tmp = mk_tmp("scanwhydenied");
    write_file(&tmp.0, "locked.md", "# locked");
    let path = tmp.0.join("locked.md");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();

    let reg = Registry::defaults();
    let scan = scan_disk_entries(
        std::slice::from_ref(&path),
        &tmp.0,
        &reg,
        crate::parser::MAX_RAW_BINARY_BYTES,
        crate::parser::MAX_RAW_TEXT_BYTES,
    );
    // Restore so the temp dir's cleanup can remove it.
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(scan.skipped, vec!["locked.md".to_string()]);
    assert_eq!(
        scan.skipped_why,
        vec![SkipReason::Unreadable {
            cause: ReadFailure::PermissionDenied,
        }]
    );
}

/// feature-61 (AC4): the scan takes its caps from the registry. A binary
/// file one byte over the default is declined under the default caps and
/// scanned under raised ones, and the text cap stays its own key.
/// [`scanned_rel_paths`] passes the registry's caps the way the full run
/// does; the full run itself is pinned by the integration tests for the size caps.
#[test]
fn a_raised_binary_cap_admits_a_file_the_default_skips() {
    use crate::parser::{CodeParsersConfig, FileSizeLimit, FileSizeLimits};
    let tmp = mk_tmp("f61-raised");
    write_file(&tmp.0, "small.md", "# h\n\nbody\n");
    let big = std::fs::File::create(tmp.0.join("big.pdf")).unwrap();
    big.set_len(crate::parser::MAX_RAW_BINARY_BYTES + 1)
        .unwrap();
    drop(big);
    let ids = ["md".to_string(), "pdf".to_string()];
    let registry = |limits: FileSizeLimits| {
        Registry::from_enabled_with_plugins(&ids, &CodeParsersConfig::default(), None, limits)
            .expect("md + pdf")
    };

    let mut by_default = scanned_rel_paths(&tmp.0, &registry(FileSizeLimits::default()));
    by_default.sort();
    assert_eq!(
        by_default,
        vec!["small.md".to_string()],
        "one byte over the default binary cap is declined"
    );

    // Raised binary cap, and a text cap below small.md: one scan shows both
    // that the binary key admits big.pdf and that it does not move the text key.
    let raised = FileSizeLimits {
        binary: FileSizeLimit::Bytes(crate::parser::MAX_RAW_BINARY_BYTES + 1),
        text: FileSizeLimit::Bytes(4),
        ..FileSizeLimits::default()
    };
    assert_eq!(
        scanned_rel_paths(&tmp.0, &registry(raised)),
        vec!["big.pdf".to_string()]
    );
}

// -----------------------------------------------------------------------
// size_cap_exceeded
// -----------------------------------------------------------------------

/// (BU-02、**旧 `test_binary_size_exceeded_none_for_text_ext` を差し替え**)
/// テキストは binary cap を無視するが、自分の cap は無視しない。
///
/// 旧テストは `is_binary_ext=false` なら実サイズに関わらず `None` を返す
/// ことを固定していた = 「テキストに上限なし」を契約として守っていた。
/// これが BU-02 の欠陥そのもの (巨大な `.md` 1 本で `fs::read` が OOM。
/// `rebuild_index` は MCP 経由でクライアントから叩ける) なので反転する。
#[test]
fn test_text_ext_uses_the_text_cap_not_the_binary_one() {
    let tmp = mk_tmp("sizecap-text");
    write_file(&tmp.0, "big.md", "0123456789"); // 10 bytes
    let path = tmp.0.join("big.md");
    // binary cap = 1 でも text なので当たらない。
    assert_eq!(size_cap_exceeded(&path, false, 1, 1024).unwrap(), None);
    // text cap = 5 なら当たる。返る cap は「適用された方」。
    assert_eq!(
        size_cap_exceeded(&path, false, 1024, 5).unwrap(),
        Some((10, 5))
    );
}

#[test]
fn test_binary_size_exceeded_none_when_within_cap() {
    let tmp = mk_tmp("sizecap-ok");
    write_file(&tmp.0, "small.pdf", "tiny");
    let result = size_cap_exceeded(&tmp.0.join("small.pdf"), true, 1024, 1024).unwrap();
    assert_eq!(result, None);
}

#[test]
fn test_binary_size_exceeded_some_when_over_cap() {
    let tmp = mk_tmp("sizecap-over");
    write_file(&tmp.0, "big.pdf", "0123456789"); // 10 bytes
    let result = size_cap_exceeded(&tmp.0.join("big.pdf"), true, 5, 1024).unwrap();
    assert_eq!(result, Some((10, 5)));
}

#[test]
fn test_binary_size_exceeded_err_when_metadata_fails() {
    let tmp = mk_tmp("sizecap-missing");
    let missing = tmp.0.join("gone.pdf");
    assert!(size_cap_exceeded(&missing, true, 1024, 1024).is_err());
}

/// 警告文が「どちらの上限に当たったか」を言い分ける。
#[test]
fn test_size_cap_kind_names_the_applied_limit() {
    assert_eq!(size_cap_kind(true), "binary");
    assert_eq!(size_cap_kind(false), "text");
}

// -----------------------------------------------------------------------
// feature-58: declared fields, generation key (DB-backed)
// -----------------------------------------------------------------------

/// (codex P2 round 9 on PR #291) The three states [`declared_fields_recorded`]'s doc
/// describes, pinned directly against [`Database::open_in_memory`] -- no embedder needed,
/// unlike a test through [`index_single_disk_entry`] or its watcher callers (see this
/// round's final-review.md entry for why no such test was added: constructing an
/// [`Embedder`] always touches the model cache/download, so every existing test that goes
/// through [`index_single_disk_entry`] is `#[ignore]`d, and this fn alone -- the only *new*
/// production code this round adds -- has no such cost).
/// (codex P2 round 12 on PR #291) The pass token / dirty mark handshake between
/// [`begin_declared_fields_pass`], a watcher write under a pending generation
/// ([`Database::mark_declared_fields_dirty`], which [`write_declared_rows_or_mark_dirty`]
/// calls), and [`finish_declared_fields_pass`]: a write under *this* pass keeps the
/// generation pending; a write under an older pass, or none, lets it be recorded.
/// Database-only, like the test below -- the watcher side that produces the mark through
/// [`index_single_disk_entry`] is covered in-process by
/// `tests/watcher_rename_context_mode.rs` (`#[ignore]`, model load).
#[test]
fn a_watcher_write_under_the_running_pass_keeps_the_generation_pending() {
    let db = Database::open_in_memory().unwrap();
    db.write_declared_fields(r#"["status"]"#).unwrap();

    // No pass running: a watcher mark is a no-op, and a finish with no token records.
    db.mark_declared_fields_dirty().unwrap();
    assert_eq!(db.read_declared_fields_dirty().unwrap(), None);
    assert!(
        finish_declared_fields_pass(&db, None, Some(r#"["status"]"#), r#"["status"]"#, true)
            .unwrap()
    );
    assert_eq!(
        db.read_declared_fields().unwrap().as_deref(),
        Some(r#"["status"]"#)
    );

    // A pass starts: the key is cleared and the token recorded, atomically.
    let token = begin_declared_fields_pass(&db).unwrap();
    assert_eq!(db.read_declared_fields().unwrap(), None);
    assert_eq!(
        db.read_declared_fields_pass().unwrap().as_deref(),
        Some(token.as_str())
    );

    // A watcher (another process) writes a document meanwhile and marks the pass.
    db.mark_declared_fields_dirty().unwrap();
    assert_eq!(
        db.read_declared_fields_dirty().unwrap().as_deref(),
        Some(token.as_str())
    );

    // The pass finishes: it must NOT record the generation, and it cleans up after itself.
    assert!(
        !finish_declared_fields_pass(&db, Some(&token), None, r#"["status","team"]"#, true)
            .unwrap(),
        "a write under this pass leaves the generation pending"
    );
    assert_eq!(db.read_declared_fields().unwrap(), None);
    assert_eq!(db.read_declared_fields_pass().unwrap(), None);
    assert_eq!(db.read_declared_fields_dirty().unwrap(), None);

    // The next pass finds no mark of its own (the old one was dropped) and records.
    let next = begin_declared_fields_pass(&db).unwrap();
    assert_ne!(next, token);
    assert!(
        finish_declared_fields_pass(&db, Some(&next), None, r#"["status","team"]"#, true).unwrap()
    );
    assert_eq!(
        db.read_declared_fields().unwrap().as_deref(),
        Some(r#"["status","team"]"#)
    );

    // A mark left by an older, unfinished pass does not stop a newer pass from recording.
    let stale = begin_declared_fields_pass(&db).unwrap();
    db.mark_declared_fields_dirty().unwrap();
    let newer = begin_declared_fields_pass(&db).unwrap();
    assert_ne!(newer, stale);
    assert_eq!(
        db.read_declared_fields_dirty().unwrap(),
        None,
        "beginning a pass drops the previous pass's mark with its token"
    );
    assert!(finish_declared_fields_pass(&db, Some(&newer), None, "[]", true).unwrap());

    // `record == false` (the caller's own reasons to leave the key absent) still cleans up.
    let t = begin_declared_fields_pass(&db).unwrap();
    assert!(!finish_declared_fields_pass(&db, Some(&t), None, "[]", false).unwrap());
    assert_eq!(db.read_declared_fields().unwrap(), None);
    assert_eq!(db.read_declared_fields_pass().unwrap(), None);
}

/// (local Codex on PR #291 after round 12) The finish side of the handshake against a
/// **second connection**, the shape another process takes. Connection B holds the write
/// lock (an immediate transaction) while it commits a watcher's document write and its
/// dirty mark; connection A calls [`finish_declared_fields_pass`] in that window. Because
/// A reads the mark inside its own immediate transaction, it waits for B's lock (the
/// connection's busy timeout) and then sees the mark -- and must not record the
/// generation. Reading the mark first and opening the transaction afterwards let B's
/// commit land in between, and A recorded the generation over rows B left stale.
///
/// Deterministic without a pause hook: B takes the lock *before* A is called, and holds
/// it for longer than A needs to reach its `BEGIN IMMEDIATE`; A cannot proceed until B
/// commits, so the only order the lock permits is the one asserted.
#[test]
fn a_mark_committed_while_the_pass_finishes_still_keeps_the_generation_pending() {
    let dir = crate::test_support::unique_temp_path("groove-pass-race");
    std::fs::create_dir_all(&dir).unwrap();
    let db_path = dir.join(".groove.db");
    let a = Database::open(db_path.to_str().unwrap()).unwrap();
    let b = Database::open(db_path.to_str().unwrap()).unwrap();

    let token = begin_declared_fields_pass(&a).unwrap();
    assert_eq!(a.read_declared_fields().unwrap(), None);

    // B: the other process's watcher, mid-commit, holding the write lock.
    let (locked_tx, locked_rx) = std::sync::mpsc::channel::<()>();
    let token_for_b = token.clone();
    let db_path_for_b = db_path.clone();
    let b_thread = std::thread::spawn(move || {
        drop(b);
        let b = Database::open(db_path_for_b.to_str().unwrap()).unwrap();
        let tx = b.begin_immediate_transaction().unwrap();
        locked_tx.send(()).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(400));
        b.mark_declared_fields_dirty().unwrap();
        assert_eq!(
            b.read_declared_fields_dirty().unwrap().as_deref(),
            Some(token_for_b.as_str())
        );
        tx.commit().unwrap();
    });
    locked_rx.recv().unwrap();

    // A: the pass finishing while B holds the lock. It must wait, then see the mark.
    let started = std::time::Instant::now();
    let recorded =
        finish_declared_fields_pass(&a, Some(&token), None, r#"["status"]"#, true).unwrap();
    b_thread.join().unwrap();
    assert!(
        started.elapsed() >= std::time::Duration::from_millis(300),
        "A must have waited for B's lock, not read around it: {:?}",
        started.elapsed()
    );
    assert!(
        !recorded,
        "B's mark landed under A's token before A decided, so the generation stays pending"
    );
    assert_eq!(a.read_declared_fields().unwrap(), None);
    assert_eq!(a.read_declared_fields_dirty().unwrap(), None);
    assert_eq!(a.read_declared_fields_pass().unwrap(), None);

    drop(a);
    let _ = std::fs::remove_dir_all(&dir);
}

/// (local Codex on PR #291 after round 12, second pass) Two [`rebuild_index`] runs in two
/// processes overlap: B begins after A and takes the token over. Whatever A does from then
/// on must not end in a recorded generation -- A's finish sees it no longer owns the
/// token and records nothing, and a document A writes meanwhile marks B's pass dirty, so
/// B records nothing either and the next run refreshes again. A run that began no pass
/// (nothing to refresh) must not record while another process's pass is open, since the
/// key it would write is what ends that pass's "pending".
#[test]
fn an_overlapping_pass_in_another_process_is_never_recorded_over() {
    let db = Database::open_in_memory().unwrap();
    db.upsert_document("x.md", Some("t"), None, None, None, &[], None, "h", 0)
        .unwrap();
    let extra = std::collections::BTreeMap::new();
    let list_a = vec!["a".to_string()];

    // A begins, then B begins: B owns the token now.
    let token_a = begin_declared_fields_pass(&db).unwrap();
    let token_b = begin_declared_fields_pass(&db).unwrap();
    assert_ne!(token_a, token_b);
    assert_eq!(
        db.read_declared_fields_pass().unwrap().as_deref(),
        Some(token_b.as_str())
    );

    // A finishes first: superseded, records nothing, leaves B's token alone.
    assert!(!finish_declared_fields_pass(&db, Some(&token_a), None, r#"["a"]"#, true).unwrap());
    assert_eq!(db.read_declared_fields().unwrap(), None);
    assert_eq!(
        db.read_declared_fields_pass().unwrap().as_deref(),
        Some(token_b.as_str()),
        "a superseded pass must not clear the owner's token"
    );
    // B finishes with no mark against it: records.
    assert!(finish_declared_fields_pass(&db, Some(&token_b), None, r#"["b"]"#, true).unwrap());
    assert_eq!(
        db.read_declared_fields().unwrap().as_deref(),
        Some(r#"["b"]"#)
    );

    // Same overlap, but A writes a document after B took over: B's pass is marked dirty
    // and B records nothing; A, superseded, records nothing either.
    let token_a = begin_declared_fields_pass(&db).unwrap();
    let token_b = begin_declared_fields_pass(&db).unwrap();
    write_declared_rows_or_mark_dirty(
        &db,
        "x.md",
        &extra,
        DeclaredSet::Known {
            list: &list_a,
            pass: Some(&token_a),
            generation: r#"["a"]"#,
        },
    )
    .unwrap();
    assert_eq!(
        db.read_declared_fields_dirty().unwrap().as_deref(),
        Some(token_b.as_str()),
        "A's write under B's pass marks B, not A"
    );
    assert!(!finish_declared_fields_pass(&db, Some(&token_b), None, r#"["b"]"#, true).unwrap());
    assert_eq!(db.read_declared_fields().unwrap(), None);
    assert!(!finish_declared_fields_pass(&db, Some(&token_a), None, r#"["a"]"#, true).unwrap());
    assert_eq!(
        db.read_declared_fields().unwrap(),
        None,
        "still pending: the next run refreshes"
    );

    // A run with no pass of its own (nothing to refresh) writes a changed document while
    // another process's pass is open: it marks that pass and does not record at the end.
    let token_b = begin_declared_fields_pass(&db).unwrap();
    write_declared_rows_or_mark_dirty(
        &db,
        "x.md",
        &extra,
        DeclaredSet::Known {
            list: &list_a,
            pass: None,
            generation: r#"["a"]"#,
        },
    )
    .unwrap();
    assert_eq!(
        db.read_declared_fields_dirty().unwrap().as_deref(),
        Some(token_b.as_str())
    );
    assert!(!finish_declared_fields_pass(&db, None, None, r#"["a"]"#, true).unwrap());
    assert_eq!(
        db.read_declared_fields_pass().unwrap().as_deref(),
        Some(token_b.as_str()),
        "a pass-less run leaves the open pass untouched"
    );
    assert!(!finish_declared_fields_pass(&db, Some(&token_b), None, r#"["b"]"#, true).unwrap());

    // With no pass open at all, a pass-less run records as before.
    assert!(finish_declared_fields_pass(&db, None, None, r#"["a"]"#, true).unwrap());
    assert_eq!(
        db.read_declared_fields().unwrap().as_deref(),
        Some(r#"["a"]"#)
    );
}

/// (codex P2 round 13 on PR #291) A run that began no pass -- its schema matched the
/// recorded generation -- finishes after another process changed the schema, refreshed
/// every row, recorded the new generation and cleared its token. No pass is open, so
/// the ownership check alone would let the first run rewrite its stale set over rows
/// the other produced. The evidence it has is the generation it observed at its start;
/// when the key no longer reads that, it records nothing.
#[test]
fn a_pass_less_run_does_not_write_over_a_generation_recorded_while_it_ran() {
    let db = Database::open_in_memory().unwrap();
    db.write_declared_fields(r#"["a"]"#).unwrap();
    // A starts: schema `[a]` matches the recorded generation, so no pass, and it
    // remembers what it observed.
    let observed_by_a = db.read_declared_fields().unwrap();
    assert_eq!(observed_by_a.as_deref(), Some(r#"["a"]"#));

    // B: schema changed to `[b]`, a full pass, recorded, token cleared.
    let token_b = begin_declared_fields_pass(&db).unwrap();
    assert!(finish_declared_fields_pass(&db, Some(&token_b), None, r#"["b"]"#, true).unwrap());
    assert_eq!(
        db.read_declared_fields().unwrap().as_deref(),
        Some(r#"["b"]"#)
    );

    // A finishes: no pass open, but the key is not what A observed -- it must not
    // write `[a]` back over B's rows.
    assert!(
        !finish_declared_fields_pass(&db, None, observed_by_a.as_deref(), r#"["a"]"#, true)
            .unwrap()
    );
    assert_eq!(
        db.read_declared_fields().unwrap().as_deref(),
        Some(r#"["b"]"#),
        "B's generation stands; A's stale set was not written over it"
    );

    // Control: had nothing changed, the same run rewrites its (unchanged) value.
    db.write_declared_fields(r#"["a"]"#).unwrap();
    assert!(
        finish_declared_fields_pass(&db, None, observed_by_a.as_deref(), r#"["a"]"#, true).unwrap()
    );
}

/// (codex P2 round 14 on PR #291) B begins after A, finishes first (records `[b]`, clears
/// its token), and only then does A write a document under `[a]`. No pass is open to
/// mark, and the row would sit under `[b]` as if it belonged to it. The write must
/// clear the generation instead, so the index reads as pending, A's own finish records
/// nothing, and the next run refreshes.
#[test]
fn a_write_after_a_later_pass_already_recorded_leaves_the_generation_pending() {
    let db = Database::open_in_memory().unwrap();
    db.upsert_document("x.md", Some("t"), None, None, None, &[], None, "h", 0)
        .unwrap();
    let extra = std::collections::BTreeMap::new();
    let list_a = vec!["a".to_string()];

    let token_a = begin_declared_fields_pass(&db).unwrap();
    let token_b = begin_declared_fields_pass(&db).unwrap();
    assert!(finish_declared_fields_pass(&db, Some(&token_b), None, r#"["b"]"#, true).unwrap());
    assert_eq!(
        db.read_declared_fields().unwrap().as_deref(),
        Some(r#"["b"]"#)
    );
    assert_eq!(db.read_declared_fields_pass().unwrap(), None);

    // A, still running, writes a document under its own set.
    write_declared_rows_or_mark_dirty(
        &db,
        "x.md",
        &extra,
        DeclaredSet::Known {
            list: &list_a,
            pass: Some(&token_a),
            generation: r#"["a"]"#,
        },
    )
    .unwrap();
    assert_eq!(
        db.read_declared_fields().unwrap(),
        None,
        "the recorded `[b]` would have claimed A's `[a]` rows; the key must be cleared"
    );
    // A finishes: its token is gone, so it records nothing, and the index stays pending.
    assert!(!finish_declared_fields_pass(&db, Some(&token_a), None, r#"["a"]"#, true).unwrap());
    assert_eq!(db.read_declared_fields().unwrap(), None);

    // Control: a write whose generation matches the recorded one leaves it in place.
    db.write_declared_fields(r#"["a"]"#).unwrap();
    write_declared_rows_or_mark_dirty(
        &db,
        "x.md",
        &extra,
        DeclaredSet::Known {
            list: &list_a,
            pass: None,
            generation: r#"["a"]"#,
        },
    )
    .unwrap();
    assert_eq!(
        db.read_declared_fields().unwrap().as_deref(),
        Some(r#"["a"]"#)
    );
}

/// (codex P2 round 12 on PR #291) The crossed-parser twins are distinct outcomes: the
/// watcher's diagnostic is chosen by variant, and "content kept" versus "document
/// dropped" must not collapse into one arm.
#[test]
fn the_dropped_rename_outcomes_are_distinct_from_the_kept_ones() {
    assert_ne!(
        RenameOutcome::RenamedSizeCappedAndDropped,
        RenameOutcome::RenamedSizeCapped
    );
    assert_ne!(
        RenameOutcome::RenamedButRefusedAndDropped,
        RenameOutcome::RenamedButRefused
    );
    assert_ne!(
        RenameOutcome::RenamedSizeCappedAndDropped,
        RenameOutcome::RenamedButRefusedAndDropped
    );
    // (local Codex after round 13) "not reindexed" is neither a plain rename nor a drop.
    assert_ne!(
        RenameOutcome::RenamedButNotReindexed,
        RenameOutcome::Renamed
    );
    assert_ne!(
        RenameOutcome::RenamedButNotReindexed,
        RenameOutcome::RenamedButRefusedAndDropped
    );
}

#[test]
fn declared_fields_recorded_tells_absent_from_declared_nothing_from_a_list() {
    let db = Database::open_in_memory().unwrap();
    assert_eq!(
        declared_fields_recorded(&db).unwrap(),
        None,
        "no generation key written yet: pending, not an empty answer"
    );

    db.write_declared_fields("[]").unwrap();
    assert_eq!(
        declared_fields_recorded(&db).unwrap(),
        Some(Vec::new()),
        "`[]` is a completed answer: the schema declares nothing"
    );

    db.write_declared_fields(r#"["environment","status"]"#)
        .unwrap();
    assert_eq!(
        declared_fields_recorded(&db).unwrap(),
        Some(vec!["environment".to_string(), "status".to_string()])
    );

    db.clear_declared_fields().unwrap();
    assert_eq!(
        declared_fields_recorded(&db).unwrap(),
        None,
        "clearing the key (what a refresh does at its start) must read back as pending again, \
             not as the list that was there a moment ago"
    );
}

// -----------------------------------------------------------------------
// feature-58: declared fields (pure fn)
// -----------------------------------------------------------------------

#[test]
fn declared_field_names_drops_the_five_named_fields_and_sorts() {
    let schema = crate::schema::Schema::from_toml_str(
            "[fields.title]\nrequired = true\n[fields.team]\n[fields.status]\nenum = [\"active\"]\n[fields.tags]\ntype = \"array\"\n",
        )
        .unwrap();
    assert_eq!(
        declared_field_names(Some(&schema)),
        vec!["status".to_string(), "team".to_string()]
    );
    assert!(declared_field_names(None).is_empty());
}

#[test]
fn declared_field_rows_keeps_scalars_and_list_elements_of_declared_keys_only() {
    use crate::parser::FieldValue;
    let mut extra = std::collections::BTreeMap::new();
    extra.insert(
        "status".to_string(),
        FieldValue::Scalar("active".to_string()),
    );
    extra.insert(
        "environment".to_string(),
        FieldValue::List(vec![
            "dev".to_string(),
            "prod".to_string(),
            "dev".to_string(),
        ]),
    );
    extra.insert("owner".to_string(), FieldValue::Other("mapping"));
    extra.insert("note".to_string(), FieldValue::Other(FieldValue::NULL));
    extra.insert(
        "undeclared".to_string(),
        FieldValue::Scalar("x".to_string()),
    );
    let declared = [
        "environment".to_string(),
        "note".to_string(),
        "owner".to_string(),
        "status".to_string(),
    ];
    assert_eq!(
        declared_field_rows(&extra, &declared),
        vec![
            ("environment".to_string(), "dev".to_string()),
            ("environment".to_string(), "prod".to_string()),
            ("environment".to_string(), "dev".to_string()), // the DB folds it; the rows are what the parser held
            ("status".to_string(), "active".to_string()),
        ]
    );
    assert!(declared_field_rows(&extra, &[]).is_empty());
}

// -----------------------------------------------------------------------
// 増分 index API
// -----------------------------------------------------------------------

fn test_db() -> Database {
    let db = Database::open_in_memory().unwrap();
    db.verify_embedding_meta("bge-small-en-v1.5", 384).unwrap();
    db
}

#[test]
fn test_deindex_single_file_missing_returns_false() {
    let db = test_db();
    let removed = deindex_single_file(&db, "never-indexed.md").unwrap();
    assert!(!removed, "deindex of non-existent path should return false");
}

#[test]
fn test_deindex_single_file_after_upsert_returns_true() {
    let db = test_db();
    db.upsert_document(
        "notes/a.md",
        Some("Title"),
        None,
        Some("notes"),
        None,
        &[],
        None,
        "hash1",
        0,
    )
    .unwrap();
    assert!(db.get_document_hash("notes/a.md").unwrap().is_some());

    let removed = deindex_single_file(&db, "notes/a.md").unwrap();
    assert!(removed, "deindex of existing path should return true");
    assert!(db.get_document_hash("notes/a.md").unwrap().is_none());
}

#[test]
fn test_update_document_meta_for_frontmatter_only_change() {
    // frontmatter-only skip の前提となる DB API が期待通り動くことの回帰テスト。
    let db = test_db();
    db.upsert_document(
        "notes/a.md",
        Some("Old"),
        None,
        Some("notes"),
        None,
        &[],
        None,
        "old_hash",
        0,
    )
    .unwrap();
    // update_document_meta は content_hash を差し替えて meta を更新
    let updated = db
        .update_document_meta(
            "notes/a.md",
            Some("New Title"),
            Some("new-topic"),
            Some("notes"),
            None,
            &["tag1".to_string()],
            Some("2026-04-19"),
            "new_hash",
            0,
        )
        .unwrap();
    assert!(updated);
    assert_eq!(
        db.get_document_hash("notes/a.md").unwrap().as_deref(),
        Some("new_hash")
    );
}

#[test]
fn test_update_document_meta_missing_path_returns_false() {
    let db = test_db();
    let updated = db
        .update_document_meta(
            "never-existed.md",
            None,
            None,
            None,
            None,
            &[],
            None,
            "h",
            0,
        )
        .unwrap();
    assert!(!updated);
}

#[test]
fn test_chunk_texts_for_path_empty_when_not_indexed() {
    let db = test_db();
    let texts = db.chunk_texts_for_path("not-indexed.md").unwrap();
    assert!(texts.is_empty());
}

#[test]
fn test_f12_8_frontmatter_only_skip_db_contract() {
    // frontmatter-only skip (frontmatter-only skip) の DB 契約部分を end-to-end で検証:
    // 1. document + chunk を 1 件 index した状態を作る
    // 2. chunk_texts_for_path が期待通りのリストを返す
    // 3. frontmatter だけ変えた再 index 相当として update_document_meta を呼ぶ
    // 4. chunks は維持されたまま、meta (title/content_hash) のみ更新される
    let db = test_db();
    let doc_id = db
        .upsert_document(
            "notes/foo.md",
            Some("Old"),
            None,
            Some("notes"),
            None,
            &[],
            None,
            "hash1",
            0,
        )
        .unwrap();
    let emb = vec![0.0f32; 384];
    db.insert_chunk(
        doc_id,
        0,
        Some("intro"),
        None,
        "Hello world body.",
        None,
        &emb,
        0.9,
    )
    .unwrap();

    // (2) 既存 chunks を比較用に取得
    let before = db.chunk_texts_for_path("notes/foo.md").unwrap();
    assert_eq!(before.len(), 1);
    assert_eq!(before[0].0.as_deref(), Some("intro"));
    assert_eq!(before[0].1, "Hello world body.");

    // (3) frontmatter-only change: title と content_hash を更新
    let updated = db
        .update_document_meta(
            "notes/foo.md",
            Some("New Title"),
            None,
            Some("notes"),
            None,
            &[],
            None,
            "hash2",
            0,
        )
        .unwrap();
    assert!(updated);

    // (4) meta は変わっているが chunks は維持
    assert_eq!(
        db.get_document_hash("notes/foo.md").unwrap().as_deref(),
        Some("hash2")
    );
    let after = db.chunk_texts_for_path("notes/foo.md").unwrap();
    assert_eq!(after, before, "chunks must survive frontmatter-only change");
}

/// vacuous test を解消するため、enum の PartialEq をベースに API の戻り値
/// 種別が expect と一致することを確認する軽量テストに差し替えた。
/// 実 Embedder を要する reindex/rename の true e2e は `cargo test --
/// --ignored` で回る integration テスト側に任せる (Embedder DL が発生
/// するため通常の cargo test には載せない)。
#[test]
fn test_single_result_variants_are_distinct() {
    assert_ne!(
        SingleResult::Unchanged,
        SingleResult::Updated {
            chunks: 0,
            frontmatter_unparsed: false
        }
    );
    assert_ne!(
        SingleResult::Unchanged,
        SingleResult::Skipped {
            reason: "test",
            frontmatter_unparsed: false
        }
    );
    assert_ne!(
        SingleResult::Updated {
            chunks: 1,
            frontmatter_unparsed: false
        },
        SingleResult::Updated {
            chunks: 2,
            frontmatter_unparsed: false
        }
    );
}

/// #251 (codex P2, round 5): the one-time check decides "unchanged" from
/// the hash the scan took, then reads the file again. Bytes swapped
/// between the two reads are not the row's bytes, so the check must not
/// treat them as read -- it reports a skip that keeps the check pending.
#[test]
#[ignore = "requires embedding model download"]
fn test_refresh_skips_a_file_whose_bytes_changed_between_scan_and_read() {
    let dir = crate::test_support::unique_temp_path("groove-fm-race");
    std::fs::create_dir_all(&dir).unwrap();
    let broken = "---\ntitle: [unclosed\n---\n\n# Broken\n\nBody long enough to be a chunk.\n";
    let clean = "---\ntitle: Clean\n---\n\n# Clean\n\nBody long enough to be a chunk.\n";
    let full = dir.join("swapped.md");
    // On disk: the clean bytes. In the index and in the scan: the broken ones.
    std::fs::write(&full, clean).unwrap();
    let old_hash = sha256_hex_bytes(broken.as_bytes());

    let db = Database::open_in_memory().unwrap();
    db.upsert_document(
        "swapped.md",
        None,
        None,
        None,
        None,
        &[],
        None,
        &old_hash,
        broken.len() as u64,
    )
    .unwrap();
    let mut embedder = Embedder::new().unwrap();
    let entry = DiskEntry {
        rel: "swapped.md".to_string(),
        hash: old_hash,
        full,
        size: broken.len() as u64,
    };
    let result = index_single_disk_entry(
        &db,
        &mut embedder,
        &entry,
        None,
        &Registry::default(),
        Reindex::Incremental {
            check_frontmatter: true,
            refresh_fields: false,
        },
        ContextMode::Off,
        DeclaredSet::Known {
            list: &[],
            pass: None,
            generation: "[]",
        },
        false,
    )
    .unwrap();
    assert_eq!(
        result,
        SingleResult::Skipped {
            reason: SKIPPED_CHANGED_DURING_READ,
            frontmatter_unparsed: false
        }
    );

    // The other direction (codex P2, round 6): the scan saw clean bytes, the
    // second read finds broken ones. The warning names the file, so the
    // skip has to carry the failure or the summary undercounts what it said.
    let new_hash = sha256_hex_bytes(clean.as_bytes());
    db.update_document_meta(
        "swapped.md",
        None,
        None,
        None,
        None,
        &[],
        None,
        &new_hash,
        clean.len() as u64,
    )
    .unwrap();
    std::fs::write(&entry.full, broken).unwrap();
    let entry = DiskEntry {
        hash: new_hash,
        ..entry
    };
    let result = index_single_disk_entry(
        &db,
        &mut embedder,
        &entry,
        None,
        &Registry::default(),
        Reindex::Incremental {
            check_frontmatter: true,
            refresh_fields: false,
        },
        ContextMode::Off,
        DeclaredSet::Known {
            list: &[],
            pass: None,
            generation: "[]",
        },
        false,
    )
    .unwrap();
    assert_eq!(
        result,
        SingleResult::Skipped {
            reason: SKIPPED_CHANGED_DURING_READ,
            frontmatter_unparsed: true
        }
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// #251 (codex P1, round 4): the CLI exit code and the MCP `error` reply
/// are two renderings of one decision, so the decision lives here.
#[test]
fn test_fails_strict_frontmatter_needs_both_the_switch_and_a_count() {
    let clean = IndexResult {
        frontmatter_unparsed: 0,
        ..IndexResult::default()
    };
    let broken = IndexResult {
        frontmatter_unparsed: 2,
        ..IndexResult::default()
    };
    assert!(!clean.fails_strict_frontmatter(false));
    assert!(!clean.fails_strict_frontmatter(true));
    assert!(!broken.fails_strict_frontmatter(false));
    assert!(broken.fails_strict_frontmatter(true));
}

/// (ADR-0023) The `Done in` line is the one an ordinary run has always
/// printed -- tests and scripts match it -- and gains a clause only when
/// the walk left something out for its name.
#[test]
fn test_summary_line_names_unspellable_entries_only_when_there_are_some() {
    let clean = IndexResult {
        total_documents: 3,
        updated: 1,
        total_chunks: 7,
        duration_ms: 12,
        ..IndexResult::default()
    };
    assert_eq!(
        clean.summary_line(),
        concat!(
            "Done in 12ms: 3 docs (1 updated, 0 renamed, 0 deleted, 0 skipped, ",
            "0 frontmatter unparsed), 7 chunks"
        )
    );
    let with = IndexResult {
        unspellable: 2,
        ..clean
    };
    assert_eq!(
        with.summary_line(),
        concat!(
            "Done in 12ms: 3 docs (1 updated, 0 renamed, 0 deleted, 0 skipped, ",
            "0 frontmatter unparsed), 7 chunks, 2 not indexed (a name get_document ",
            "cannot take, see the warnings above)"
        )
    );
    assert!(with.summary_line().is_ascii());
}

fn s(items: &[&str]) -> Vec<String> {
    items.iter().map(|item| item.to_string()).collect()
}

/// (feature-69, Review Focus 4) [`embed_input_from`] builds what [`embed_input_for`] always built.
#[test]
fn test_embed_input_from_follows_embed_input_for() {
    assert_eq!(
        embed_input_from("c", Some("T > H"), ContextMode::Static),
        "T > H\n\nc"
    );
    assert_eq!(embed_input_from("c", Some("  "), ContextMode::Static), "c");
    assert_eq!(embed_input_from("c", None, ContextMode::Static), "c");
    assert_eq!(embed_input_from("c", Some("T > H"), ContextMode::Off), "c");
}

#[test]
fn test_plan_vector_reuse_embeds_everything_when_nothing_is_stored() {
    let plan = plan_vector_reuse(&s(&["a", "b"]), &[]);
    assert_eq!(
        plan.sources,
        vec![VectorSource::Computed(0), VectorSource::Computed(1)]
    );
    assert_eq!(plan.to_embed, vec![0, 1]);
}

#[test]
fn test_plan_vector_reuse_matches_by_text_not_position() {
    let plan = plan_vector_reuse(&s(&["x", "a", "b"]), &s(&["a", "b"]));
    assert_eq!(
        plan.sources,
        vec![
            VectorSource::Computed(0),
            VectorSource::Stored(0),
            VectorSource::Stored(1)
        ]
    );
    assert_eq!(plan.to_embed, vec![0]);
}

#[test]
fn test_plan_vector_reuse_embeds_nothing_when_every_chunk_matches() {
    let plan = plan_vector_reuse(&s(&["b", "a"]), &s(&["a", "b"]));
    assert_eq!(
        plan.sources,
        vec![VectorSource::Stored(1), VectorSource::Stored(0)]
    );
    assert!(plan.to_embed.is_empty());
}

#[test]
fn test_plan_vector_reuse_maps_duplicates_one_to_many_and_keeps_misses_apart() {
    let plan = plan_vector_reuse(&s(&["a", "a", "c", "c"]), &s(&["a", "a"]));
    assert_eq!(
        plan.sources,
        vec![
            VectorSource::Stored(0),
            VectorSource::Stored(0),
            VectorSource::Computed(0),
            VectorSource::Computed(1)
        ]
    );
    assert_eq!(plan.to_embed, vec![2, 3]);
}

#[test]
fn test_plan_vector_reuse_of_no_chunks_is_empty() {
    let plan = plan_vector_reuse(&[], &s(&["a"]));
    assert!(plan.sources.is_empty());
    assert!(plan.to_embed.is_empty());
}

/// (feature-69) What a changed document did not send to the embedder was kept from the
/// store; more inputs than chunks cannot make the reused count negative.
#[test]
fn test_chunk_vector_counts_split_an_updated_document() {
    assert_eq!(
        ChunkVectorCounts::from_texts(5, 2),
        ChunkVectorCounts {
            embedded: 2,
            reused: 3
        }
    );
    assert_eq!(
        ChunkVectorCounts::from_texts(5, 0),
        ChunkVectorCounts {
            embedded: 0,
            reused: 5
        }
    );
    assert_eq!(
        ChunkVectorCounts::from_texts(5, 9),
        ChunkVectorCounts {
            embedded: 5,
            reused: 0
        }
    );
}

/// (kuriya 621) An updated document's counts come from its own plan: the chunks the plan
/// sends to the embedder are the embedded ones, every other chunk is reused. Duplicates
/// are counted one by one, as the plan matches them.
#[test]
fn test_chunk_vector_counts_of_plan_count_the_plan_to_embed() {
    for (new, stored) in [
        (s(&["a", "b"]), Vec::new()),
        (s(&["x", "a", "b"]), s(&["a", "b"])),
        (s(&["b", "a"]), s(&["a", "b"])),
        (s(&["a", "a", "c", "c"]), s(&["a", "a"])),
        (Vec::new(), s(&["a"])),
    ] {
        let plan = plan_vector_reuse(&new, &stored);
        let counts = ChunkVectorCounts::of_plan(&plan);
        assert_eq!(counts.embedded as usize, plan.to_embed.len(), "{new:?}");
        assert_eq!(
            (counts.embedded + counts.reused) as usize,
            new.len(),
            "{new:?}"
        );
    }
    assert_eq!(
        ChunkVectorCounts::of_plan(&plan_vector_reuse(&s(&["x", "a", "b"]), &s(&["a", "b"]))),
        ChunkVectorCounts {
            embedded: 1,
            reused: 2
        }
    );
    assert_eq!(
        ChunkVectorCounts::all_reused(4),
        ChunkVectorCounts {
            embedded: 0,
            reused: 4
        }
    );
}

/// Inputs drawn from a three-letter alphabet with lengths 0..=2, so the new and the
/// stored side share strings (and repeat them) often.
fn ax42_small_inputs(max: usize) -> impl proptest::strategy::Strategy<Value = Vec<String>> {
    proptest::collection::vec("[ab ]{0,2}", 0..=max)
}

proptest::proptest! {
    #![proptest_config(proptest::test_runner::Config {
        cases: 256,
        ..proptest::test_runner::Config::default()
    })]

    /// (AX-42) The invariants of [`plan_vector_reuse`] over arbitrary inputs: one source per
    /// new chunk; a stored source names the first stored row with the same input; a
    /// computed source names the to-embed slot that holds this chunk, whose input the
    /// stored side lacks; `to_embed` is strictly increasing; and the plan's counts add up
    /// to the number of new chunks.
    #[test]
    fn prop_plan_vector_reuse_keeps_its_invariants(
        new in ax42_small_inputs(12),
        stored in ax42_small_inputs(12),
    ) {
        let plan = plan_vector_reuse(&new, &stored);
        proptest::prop_assert_eq!(plan.sources.len(), new.len());
        for (i, source) in plan.sources.iter().enumerate() {
            match *source {
                VectorSource::Stored(j) => {
                    proptest::prop_assert!(j < stored.len());
                    proptest::prop_assert_eq!(&stored[j], &new[i]);
                    let first = stored.iter().position(|s| s == &new[i]);
                    proptest::prop_assert_eq!(first, Some(j));
                }
                VectorSource::Computed(k) => {
                    proptest::prop_assert!(k < plan.to_embed.len());
                    proptest::prop_assert_eq!(plan.to_embed[k], i);
                    proptest::prop_assert!(!stored.contains(&new[i]));
                }
            }
        }
        proptest::prop_assert!(plan.to_embed.windows(2).all(|w| w[0] < w[1]));
        // Every to-embed slot is claimed by exactly the chunk it names.
        for (k, &i) in plan.to_embed.iter().enumerate() {
            proptest::prop_assert_eq!(plan.sources[i], VectorSource::Computed(k));
        }
        let counts = ChunkVectorCounts::of_plan(&plan);
        proptest::prop_assert_eq!(counts.embedded as usize, plan.to_embed.len());
        proptest::prop_assert_eq!((counts.embedded + counts.reused) as usize, new.len());
    }
}

/// (kuriya 621) [`counted`] keeps the counts a body set only when the body updated the
/// document; any other result, and an error, report none. The embedder's counter is not
/// read: nothing here embeds.
#[test]
fn test_counted_keeps_counts_only_for_an_updated_document() {
    let set = ChunkVectorCounts {
        embedded: 2,
        reused: 3,
    };
    let updated = counted(|counts| {
        *counts = set;
        Ok(Reindexed {
            result: SingleResult::Updated {
                chunks: 5,
                frontmatter_unparsed: false,
            },
            reason: None,
        })
    })
    .expect("updated");
    assert_eq!(updated.counts, set);
    for result in [
        SingleResult::Unchanged,
        SingleResult::Refused,
        SingleResult::Skipped {
            reason: SKIPPED_NO_CHUNKS,
            frontmatter_unparsed: false,
        },
    ] {
        let other = counted(|counts| {
            *counts = set;
            Ok(Reindexed {
                result,
                reason: None,
            })
        })
        .expect("not updated");
        assert_eq!(other.counts, ChunkVectorCounts::default(), "{other:?}");
    }
    let failed = counted(|counts| {
        *counts = set;
        anyhow::bail!("failed")
    });
    assert!(failed.is_err());
}

/// (feature-69) The `Done in` line gains this run's chunk counts only when either is not
/// zero, before the unspellable clause, so a run that embedded nothing prints the line it
/// always printed.
#[test]
fn test_summary_line_names_this_runs_chunk_counts_only_when_there_are_some() {
    let clean = IndexResult {
        total_documents: 3,
        updated: 1,
        total_chunks: 7,
        duration_ms: 12,
        ..IndexResult::default()
    };
    assert_eq!(
        clean.summary_line(),
        concat!(
            "Done in 12ms: 3 docs (1 updated, 0 renamed, 0 deleted, 0 skipped, ",
            "0 frontmatter unparsed), 7 chunks"
        )
    );
    let both = IndexResult {
        chunks_embedded: 5,
        chunks_reused: 135,
        ..clean.clone()
    };
    assert_eq!(
        both.summary_line(),
        concat!(
            "Done in 12ms: 3 docs (1 updated, 0 renamed, 0 deleted, 0 skipped, ",
            "0 frontmatter unparsed), 7 chunks (this run: 5 embedded, 135 reused)"
        )
    );
    let reused_only = IndexResult {
        chunks_embedded: 0,
        chunks_reused: 3,
        ..clean.clone()
    };
    assert_eq!(
        reused_only.summary_line(),
        concat!(
            "Done in 12ms: 3 docs (1 updated, 0 renamed, 0 deleted, 0 skipped, ",
            "0 frontmatter unparsed), 7 chunks (this run: 0 embedded, 3 reused)"
        )
    );
    let with_unspellable = IndexResult {
        unspellable: 2,
        ..both.clone()
    };
    assert_eq!(
        with_unspellable.summary_line(),
        concat!(
            "Done in 12ms: 3 docs (1 updated, 0 renamed, 0 deleted, 0 skipped, ",
            "0 frontmatter unparsed), 7 chunks (this run: 5 embedded, 135 reused), ",
            "2 not indexed (a name get_document cannot take, see the warnings above)"
        )
    );
    for line in [
        clean.summary_line(),
        both.summary_line(),
        reused_only.summary_line(),
        with_unspellable.summary_line(),
    ] {
        assert!(line.is_ascii(), "{line}");
    }
}

/// (feature-69, spec §8 e) A stored vector whose length does not fit the index fails the
/// write after the old rows were deleted and the first new chunk went in; dropping the
/// transaction leaves the document exactly as it was, with no row half-written.
#[test]
fn test_write_with_a_stored_vector_of_the_wrong_length_rolls_the_document_back() {
    let db = test_db();
    let registry = Registry::default();
    let parser = registry.by_extension("md").expect("the md parser");
    let parsed = parser.parse(
        concat!(
            "---\ntitle: Runbook\n---\n\n",
            "## Install\n\nDownload the archive and unpack it into the tools folder ",
            "before anything else.\n\n",
            "## Configure\n\nPoint the config file at the shared cache directory on the ",
            "build machine.\n",
        ),
        "a.md",
        &[],
    );
    assert_eq!(parsed.chunks.len(), 2, "premise: one chunk per section");

    let first = vec![0.25_f32; 384];
    let tx = db.begin_transaction().unwrap();
    write_parsed_document_with_vectors(
        &db,
        "a.md",
        &parsed,
        parser,
        &[ChunkVector::Computed(&first), ChunkVector::Computed(&first)],
        ContextMode::Off,
        "h1",
        0,
        None,
        None,
    )
    .unwrap();
    tx.commit().unwrap();
    let before = db.stored_chunk_vectors_for_path("a.md").unwrap();
    assert_eq!(before.len(), 2);

    let second = vec![0.5_f32; 384];
    let tx = db.begin_transaction().unwrap();
    let written = write_parsed_document_with_vectors(
        &db,
        "a.md",
        &parsed,
        parser,
        &[
            ChunkVector::Computed(&second),
            ChunkVector::Stored(&[0u8; 8]),
        ],
        ContextMode::Off,
        "h2",
        0,
        None,
        None,
    );
    assert!(written.is_err(), "an 8-byte vector cannot go into 384 dims");
    drop(tx);

    assert_eq!(db.stored_chunk_vectors_for_path("a.md").unwrap(), before);
    for (name, scan) in [
        ("chunks without embedding", db.chunks_without_embedding(5)),
        ("embeddings without chunk", db.embeddings_without_chunk(5)),
        ("chunks without fts", db.chunks_without_fts(5)),
        ("fts without chunk", db.fts_without_chunk(5)),
    ] {
        let scan = scan.unwrap();
        assert!(scan.is_clean(), "{name}: {scan:?}");
    }
}

/// #251 (codex P2, round 1): a frontmatter-only stub with broken YAML is
/// skipped for having no chunks, and still has to be counted, so the
/// skipped result carries the flag too.
#[test]
fn test_skipped_distinguishes_frontmatter_unparsed() {
    assert_ne!(
        SingleResult::Skipped {
            reason: "no embeddable chunks",
            frontmatter_unparsed: true
        },
        SingleResult::Skipped {
            reason: "no embeddable chunks",
            frontmatter_unparsed: false
        }
    );
}

/// #251: the run summary counts documents written with the
/// `frontmatter:unparsed` tag, so the per-file result has to say so.
#[test]
fn test_updated_distinguishes_frontmatter_unparsed() {
    assert_ne!(
        SingleResult::Updated {
            chunks: 1,
            frontmatter_unparsed: true
        },
        SingleResult::Updated {
            chunks: 1,
            frontmatter_unparsed: false
        }
    );
}

#[test]
fn test_rename_outcome_variants_are_distinct() {
    assert_ne!(
        RenameOutcome::Renamed,
        RenameOutcome::RenamedAndReindexed { chunks: 1 }
    );
    assert_ne!(RenameOutcome::Renamed, RenameOutcome::OldPathMissing);
}

#[test]
fn test_rename_outcome_size_capped_variant_is_distinct() {
    // codex P2 round 3: 新 variant が既存 3 種と区別できることの回帰確認
    // (rename_single_file 自体は Embedder 必須で単体テスト不可のため、
    // ここでは enum の distinctness のみ確認する)。
    assert_ne!(RenameOutcome::RenamedSizeCapped, RenameOutcome::Renamed);
    assert_ne!(
        RenameOutcome::RenamedSizeCapped,
        RenameOutcome::RenamedAndReindexed { chunks: 1 }
    );
    assert_ne!(
        RenameOutcome::RenamedSizeCapped,
        RenameOutcome::OldPathMissing
    );
}

// -----------------------------------------------------------------------
// index_rel_path: the one spelling the index holds for a document
// -----------------------------------------------------------------------

/// The known-answer case, on every platform: components joined with `/`.
#[test]
fn test_index_rel_path_is_kb_relative_and_slash_separated() {
    let kb = Path::new("kb");
    let full = kb.join("notes").join("a.md");
    assert_eq!(index_rel_path(kb, &full), Some("notes/a.md".to_string()));
    assert_eq!(
        index_rel_path_exact(kb, &full),
        Some("notes/a.md".to_string())
    );
    assert_eq!(index_rel_path_or_whole(kb, &full), "notes/a.md");
}

/// Outside the knowledge base there is no relative spelling. The two
/// `Option` forms say so; the third keeps what its three callers always
/// did, which is to fall back to the path as given.
#[test]
fn test_index_rel_path_outside_the_kb_has_no_relative_spelling() {
    let kb = Path::new("kb");
    let outside = Path::new("elsewhere").join("a.md");
    assert_eq!(index_rel_path(kb, &outside), None);
    assert_eq!(index_rel_path_exact(kb, &outside), None);
    assert_eq!(index_rel_path_or_whole(kb, &outside), "elsewhere/a.md");
}

/// Where `\` separates components it is folded, however the path was
/// written.
#[cfg(windows)]
#[test]
fn test_index_rel_path_folds_the_windows_separator() {
    let kb = Path::new(r"C:\kb");
    let full = Path::new(r"C:\kb\a\b.md");
    assert_eq!(index_rel_path(kb, full), Some("a/b.md".to_string()));
    assert_eq!(index_rel_path_exact(kb, full), Some("a/b.md".to_string()));
    assert_eq!(index_rel_path_or_whole(kb, full), "a/b.md");
}

/// On Unix `\` is an ordinary filename character. `secret\pay.md` is one
/// file in the KB root, and folding it would store the spelling of a
/// different path -- `secret/pay.md` -- which `get_document` cannot open
/// and a gateway rule written against the index would not match (codex P2
/// round 4 on PR #310).
#[cfg(unix)]
#[test]
fn test_index_rel_path_keeps_a_literal_backslash_on_unix() {
    let kb = Path::new("/kb");
    let full = Path::new("/kb/secret\\pay.md");
    assert_eq!(index_rel_path(kb, full), Some("secret\\pay.md".to_string()));
    assert_eq!(
        index_rel_path_exact(kb, full),
        Some("secret\\pay.md".to_string())
    );
    assert_eq!(index_rel_path_or_whole(kb, full), "secret\\pay.md");
}

/// A name that is not UTF-8 is where the two `Option` forms differ, and
/// each keeps what its callers did before they shared this code: the index
/// stores a lossy spelling, `get_document` refuses to compare against one.
#[cfg(unix)]
#[test]
fn test_index_rel_path_lossy_and_exact_differ_only_on_non_utf8_names() {
    use std::os::unix::ffi::OsStrExt;
    let kb = Path::new("/kb");
    let full = kb.join(std::ffi::OsStr::from_bytes(b"caf\xe9.md"));
    assert_eq!(
        index_rel_path(kb, &full),
        Some("caf\u{fffd}.md".to_string())
    );
    assert_eq!(index_rel_path_exact(kb, &full), None);
}

/// The scan is what writes `documents.path`, so this is the stored
/// spelling -- read from the scan rather than from the helper, which
/// would pass even if the scan stopped calling it.
#[cfg(unix)]
#[test]
fn test_scan_disk_entries_stores_a_literal_backslash_name_unfolded() {
    let tmp = mk_tmp("scanbackslash");
    write_file(&tmp.0, "secret\\pay.md", "# pay");
    write_file(&tmp.0, "secret/pay.md", "# a different document");
    let mut rels = scanned_rel_paths(&tmp.0, &Registry::defaults());
    rels.sort();
    assert_eq!(
        rels,
        vec!["secret/pay.md".to_string(), "secret\\pay.md".to_string()],
        "two files, two spellings: folding `\\` would give both the same key"
    );
}

/// What the next `groove index` does with a row an older version stored
/// folded (`secret/pay.md` for a file named `secret\pay.md`): the old
/// spelling is in the database and not on disk, the new one is on disk and
/// not in the database, and the bytes are the same -- so it is a rename,
/// not an orphan. Pure strings, so this runs on every platform.
#[test]
fn test_detect_renames_moves_a_folded_row_to_its_unfolded_spelling() {
    let disk = vec![mk_entry("secret\\pay.md", "h1")];
    let mut db = HashMap::new();
    db.insert("secret/pay.md".to_string(), "h1".to_string());
    let pairs = detect_renames(&disk, &db, &HashSet::new());
    assert_eq!(
        pairs,
        vec![("secret/pay.md".to_string(), "secret\\pay.md".to_string())]
    );
}

/// Where `\` separates components no version ever stored a folded key
/// that differs from the real one, so there is nothing to migrate.
#[test]
fn test_legacy_folded_spelling_exists_only_where_backslash_is_a_filename_character() {
    assert_eq!(legacy_folded_spelling("notes/a.md"), None);
    if backslash_separates_components() {
        assert_eq!(legacy_folded_spelling("secret\\pay.md"), None);
    } else {
        assert_eq!(
            legacy_folded_spelling("secret\\pay.md"),
            Some("secret/pay.md".to_string())
        );
    }
}

/// Nothing to sweep means nothing is touched: the ordinary case, and the
/// whole of what this function does on Windows.
#[test]
fn test_sweep_legacy_folded_row_leaves_an_ordinary_path_alone() {
    let tmp = mk_tmp("legacysweepnoop");
    let db = test_db();
    db.upsert_document("notes/a.md", None, None, None, None, &[], None, "h", 0)
        .unwrap();
    assert!(!sweep_legacy_folded_row(&db, &tmp.0, "notes/a.md", false).unwrap());
    assert!(db.get_document_hash("notes/a.md").unwrap().is_some());
}

/// The row a version up to 1.12.0 stored for `secret\pay.md` sits under
/// `secret/pay.md`. Once the watcher writes or removes the real key, that
/// row describes nothing and has to go with it.
#[cfg(unix)]
#[test]
fn test_sweep_legacy_folded_row_removes_the_row_an_older_version_stored() {
    let tmp = mk_tmp("legacysweep");
    write_file(&tmp.0, "secret\\pay.md", "# pay");
    let db = test_db();
    db.upsert_document("secret/pay.md", None, None, None, None, &[], None, "h", 0)
        .unwrap();

    assert!(sweep_legacy_folded_row(&db, &tmp.0, "secret\\pay.md", false).unwrap());
    assert!(db.get_document_hash("secret/pay.md").unwrap().is_none());
    // Idempotent: the second event for the same file finds nothing.
    assert!(!sweep_legacy_folded_row(&db, &tmp.0, "secret\\pay.md", false).unwrap());
}

/// A real `secret/pay.md` owns that key. Its row is a different document's
/// and is not this sweep's to remove.
#[cfg(unix)]
#[test]
fn test_sweep_legacy_folded_row_keeps_the_row_of_a_real_file_at_that_path() {
    let tmp = mk_tmp("legacysweepreal");
    write_file(&tmp.0, "secret\\pay.md", "# pay");
    write_file(&tmp.0, "secret/pay.md", "# a different document");
    let db = test_db();
    db.upsert_document("secret/pay.md", None, None, None, None, &[], None, "h", 0)
        .unwrap();

    assert!(!sweep_legacy_folded_row(&db, &tmp.0, "secret\\pay.md", false).unwrap());
    assert!(db.get_document_hash("secret/pay.md").unwrap().is_some());
}

/// The shape of a key the walk could have produced. Strings only, so the
/// table is the same on every platform.
#[test]
fn test_names_a_walked_path_truth_table() {
    for (key, expected) in [
        ("notes/a.md", true),
        ("a.md", true),
        ("secret\\pay.md", true),
        ("", false),
        ("/foo.md", false),
        ("a/../b.md", false),
        ("a/../../x.md", false),
        ("a//b.md", false),
        ("./a.md", false),
        ("a/./b.md", false),
        ("a/", false),
        ("..", false),
    ] {
        assert_eq!(names_a_walked_path(key), expected, "{key:?}");
    }
}

/// A file literally named `\...\outside.md` folds to an absolute path, and
/// `Path::join` with an absolute right-hand side drops the knowledge base:
/// the stat used to land on the host. The name here is built from a file
/// this test creates outside the KB, so the file the old code would have
/// found really exists -- and the row goes anyway. Nothing on the host
/// outside this test's own temp directories is involved.
#[cfg(unix)]
#[test]
fn test_sweep_legacy_folded_row_does_not_ask_the_host_about_an_absolute_spelling() {
    let kb = mk_tmp("legacysweepabs-kb");
    let outside = mk_tmp("legacysweepabs-out");
    write_file(&outside.0, "o.md", "# not part of the knowledge base");
    let absolute = outside.0.join("o.md").to_str().unwrap().to_string();
    assert!(absolute.starts_with('/'));
    let name = absolute.replace('/', "\\");
    assert!(name.len() <= 255, "the fixture name must fit in a filename");
    write_file(&kb.0, &name, "# a file with backslashes in its name");

    let db = test_db();
    db.upsert_document(&absolute, None, None, None, None, &[], None, "h", 0)
        .unwrap();
    assert!(sweep_legacy_folded_row(&db, &kb.0, &name, false).unwrap());
    assert!(db.get_document_hash(&absolute).unwrap().is_none());
}

/// The same for a name that folds into something climbing out of the KB:
/// `a\..\..\x.md` becomes `a/../../x.md`, which from `kb/` is the `x.md`
/// beside it. That file exists here, and must not be what decides.
#[cfg(unix)]
#[test]
fn test_sweep_legacy_folded_row_does_not_climb_out_through_a_folded_dot_dot() {
    let tmp = mk_tmp("legacysweepclimb");
    let kb = tmp.0.join("kb");
    std::fs::create_dir_all(kb.join("a")).unwrap();
    write_file(&tmp.0, "x.md", "# outside the knowledge base");
    write_file(
        &kb,
        "a\\..\\..\\x.md",
        "# a file with backslashes in its name",
    );

    let db = test_db();
    db.upsert_document("a/../../x.md", None, None, None, None, &[], None, "h", 0)
        .unwrap();
    assert!(sweep_legacy_folded_row(&db, &kb, "a\\..\\..\\x.md", false).unwrap());
    assert!(db.get_document_hash("a/../../x.md").unwrap().is_none());
}

/// What the sweep's transaction is made of, on every platform (the sweep
/// itself has nothing to do where `\` separates components): a delete
/// inside a caller's immediate transaction joins it rather than opening
/// its own, takes effect on commit, and is undone when the transaction is
/// dropped instead.
#[test]
fn test_deindex_single_file_rides_a_callers_immediate_transaction() {
    let db = test_db();
    db.upsert_document("a.md", None, None, None, None, &[], None, "h", 0)
        .unwrap();

    {
        let _dropped = db.begin_immediate_transaction().unwrap();
        assert!(deindex_single_file(&db, "a.md").unwrap());
    }
    assert!(db.get_document_hash("a.md").unwrap().is_some());

    let tx = db.begin_immediate_transaction().unwrap();
    assert!(deindex_single_file(&db, "a.md").unwrap());
    tx.commit().unwrap();
    assert!(db.get_document_hash("a.md").unwrap().is_none());
}

/// Two connections to one database file, the way a `groove index` run in
/// another process and a server's watcher share it. The look and the delete
/// sit in one immediate transaction, so the other writer can only commit
/// before the look or after the delete -- and both of those orders have to
/// come out right. This walks them one after the other; it does not stage
/// the race itself, which would need a hook inside the transaction that
/// the function should not carry.
#[cfg(unix)]
#[test]
fn test_sweep_legacy_folded_row_is_right_on_both_sides_of_another_writer() {
    let tmp = mk_tmp("legacysweeptwoconn");
    let kb = tmp.0.join("kb");
    std::fs::create_dir_all(&kb).unwrap();
    write_file(&kb, "secret\\pay.md", "# pay");
    let db_file = tmp.0.join("index.db");
    let open = || {
        let db = Database::open(db_file.to_str().unwrap()).unwrap();
        db.verify_embedding_meta("bge-small-en-v1.5", 384).unwrap();
        db
    };
    let watcher_side = open();
    let other_process = open();
    let seed = |db: &Database| {
        db.upsert_document("secret/pay.md", None, None, None, None, &[], None, "h", 0)
            .unwrap();
    };

    // The other writer first: it creates a real `secret/pay.md` and commits
    // its row. The sweep's look, under the lock, finds the file and keeps
    // the row -- it is that document's now.
    write_file(&kb, "secret/pay.md", "# a different document");
    seed(&other_process);
    assert!(!sweep_legacy_folded_row(&watcher_side, &kb, "secret\\pay.md", false).unwrap());
    assert!(
        other_process
            .get_document_hash("secret/pay.md")
            .unwrap()
            .is_some()
    );

    // The sweep first: no file there, so the stale row goes. The other
    // writer then creates the file and commits; its row is in the index
    // and the next sweep leaves it alone.
    std::fs::remove_file(kb.join("secret").join("pay.md")).unwrap();
    assert!(sweep_legacy_folded_row(&watcher_side, &kb, "secret\\pay.md", false).unwrap());
    assert!(
        other_process
            .get_document_hash("secret/pay.md")
            .unwrap()
            .is_none()
    );
    write_file(&kb, "secret/pay.md", "# a different document");
    seed(&other_process);
    assert!(!sweep_legacy_folded_row(&watcher_side, &kb, "secret\\pay.md", false).unwrap());
    assert!(
        watcher_side
            .get_document_hash("secret/pay.md")
            .unwrap()
            .is_some()
    );
}

/// A sweep that decides to keep the row writes nothing and leaves no
/// transaction open behind it: the connection is usable, and another
/// connection can take the write lock at once.
#[cfg(unix)]
#[test]
fn test_sweep_legacy_folded_row_releases_the_write_lock_when_it_keeps_the_row() {
    let tmp = mk_tmp("legacysweeprelease");
    let kb = tmp.0.join("kb");
    std::fs::create_dir_all(&kb).unwrap();
    write_file(&kb, "secret\\pay.md", "# pay");
    write_file(&kb, "secret/pay.md", "# a different document");
    let db_file = tmp.0.join("index.db");
    let open = || {
        let db = Database::open(db_file.to_str().unwrap()).unwrap();
        db.verify_embedding_meta("bge-small-en-v1.5", 384).unwrap();
        db
    };
    let watcher_side = open();
    let other_process = open();

    // Kept because the real key is not in the index, then kept because a
    // real file owns the folded path: the two early returns.
    assert!(!sweep_legacy_folded_row(&watcher_side, &kb, "secret\\pay.md", true).unwrap());
    assert!(!sweep_legacy_folded_row(&watcher_side, &kb, "secret\\pay.md", false).unwrap());

    let tx = other_process.begin_immediate_transaction().unwrap();
    other_process
        .upsert_document("other.md", None, None, None, None, &[], None, "h", 0)
        .unwrap();
    tx.commit().unwrap();
    assert!(
        watcher_side
            .get_document_hash("other.md")
            .unwrap()
            .is_some()
    );
}

/// `symlink_metadata` declines to follow only the last component, so a
/// single stat of `kb/alias/pay.md` is answered by the directory `alias`
/// points at. That `pay.md` exists here, outside the knowledge base, and
/// must not be what keeps the row: the walk does not descend a symlink, so
/// no document of this index is spelled `alias/pay.md`.
#[cfg(unix)]
#[test]
fn test_sweep_legacy_folded_row_does_not_look_through_a_symlinked_ancestor() {
    let kb = mk_tmp("legacysweepalias-kb");
    let outside = mk_tmp("legacysweepalias-out");
    write_file(&outside.0, "pay.md", "# not part of the knowledge base");
    std::os::unix::fs::symlink(&outside.0, kb.0.join("alias")).unwrap();
    write_file(
        &kb.0,
        "alias\\pay.md",
        "# a file with a backslash in its name",
    );

    let db = test_db();
    db.upsert_document("alias/pay.md", None, None, None, None, &[], None, "h", 0)
        .unwrap();
    assert!(sweep_legacy_folded_row(&db, &kb.0, "alias\\pay.md", false).unwrap());
    assert!(db.get_document_hash("alias/pay.md").unwrap().is_none());
}

/// An ancestor that is a regular file: nothing can sit below it, so the
/// row has no other owner. (The stat of the whole path reports
/// `NotADirectory` here, which is "not there", not "could not look".)
#[cfg(unix)]
#[test]
fn test_sweep_legacy_folded_row_removes_the_row_below_an_ancestor_that_is_a_file() {
    let tmp = mk_tmp("legacysweepfileancestor");
    write_file(&tmp.0, "secret", "a regular file, not a directory");
    write_file(&tmp.0, "secret\\pay.md", "# pay");

    let db = test_db();
    db.upsert_document("secret/pay.md", None, None, None, None, &[], None, "h", 0)
        .unwrap();
    assert!(sweep_legacy_folded_row(&db, &tmp.0, "secret\\pay.md", false).unwrap());
    assert!(db.get_document_hash("secret/pay.md").unwrap().is_none());
}

/// A symlink at the leaf is not a document either: the walk takes only
/// entries that are themselves regular files. What it points at -- here a
/// real file inside the knowledge base -- has its own row under its own
/// name.
#[cfg(unix)]
#[test]
fn test_sweep_legacy_folded_row_removes_the_row_of_a_leaf_that_is_a_symlink() {
    let tmp = mk_tmp("legacysweepleaflink");
    write_file(&tmp.0, "real.md", "# real");
    std::fs::create_dir_all(tmp.0.join("secret")).unwrap();
    std::os::unix::fs::symlink(tmp.0.join("real.md"), tmp.0.join("secret").join("pay.md")).unwrap();
    write_file(&tmp.0, "secret\\pay.md", "# pay");

    let db = test_db();
    db.upsert_document("secret/pay.md", None, None, None, None, &[], None, "h", 0)
        .unwrap();
    assert!(sweep_legacy_folded_row(&db, &tmp.0, "secret\\pay.md", false).unwrap());
    assert!(db.get_document_hash("secret/pay.md").unwrap().is_none());
}

/// The walk's side of the claim the sweep rests on, from the walk itself:
/// neither a symlinked directory's contents nor a symlinked file is
/// collected.
#[cfg(unix)]
#[test]
fn test_collect_source_files_takes_nothing_through_or_at_a_symlink() {
    let kb = mk_tmp("walknolinks-kb");
    let outside = mk_tmp("walknolinks-out");
    write_file(&outside.0, "pay.md", "# outside");
    write_file(&kb.0, "real.md", "# real");
    std::os::unix::fs::symlink(&outside.0, kb.0.join("alias")).unwrap();
    std::os::unix::fs::symlink(kb.0.join("real.md"), kb.0.join("link.md")).unwrap();

    assert_eq!(
        scanned_rel_paths(&kb.0, &Registry::defaults()),
        vec!["real.md".to_string()]
    );
}

/// A directory at the folded path is not a document, so it does not make
/// the row ambiguous.
#[cfg(unix)]
#[test]
fn test_sweep_legacy_folded_row_is_not_stopped_by_a_directory_at_that_path() {
    let tmp = mk_tmp("legacysweepdir");
    std::fs::create_dir_all(tmp.0.join("secret").join("pay.md")).unwrap();
    let db = test_db();
    db.upsert_document("secret/pay.md", None, None, None, None, &[], None, "h", 0)
        .unwrap();

    assert!(sweep_legacy_folded_row(&db, &tmp.0, "secret\\pay.md", false).unwrap());
    assert!(db.get_document_hash("secret/pay.md").unwrap().is_none());
}

// -----------------------------------------------------------------------
// resolve_context_mode (feature-46 Task 2.5)
// -----------------------------------------------------------------------

use crate::db::ContextMode;

#[test]
fn test_resolve_context_mode_fresh_db_adopts_desired() {
    let db = test_db(); // 空 (chunk 0)
    let m = resolve_context_mode(&db, ContextMode::Static, false).unwrap();
    assert_eq!(m, ContextMode::Static);
    assert_eq!(db.read_context_mode().unwrap(), Some(ContextMode::Static));
}

#[test]
fn test_resolve_context_mode_legacy_db_grandfathers_to_off() {
    let db = test_db();
    // chunk を 1 件入れて legacy (key 不在 + chunk > 0) を作る
    let doc_id = db
        .upsert_document("a.md", Some("T"), None, None, None, &[], None, "h", 0)
        .unwrap();
    db.insert_chunk(
        doc_id,
        0,
        Some("H"),
        Some(2),
        "body",
        None,
        &vec![0.0f32; 384],
        1.0,
    )
    .unwrap();
    let m = resolve_context_mode(&db, ContextMode::Static, false).unwrap();
    assert_eq!(m, ContextMode::Off, "legacy DB grandfathers to off");
    assert_eq!(db.read_context_mode().unwrap(), Some(ContextMode::Off));
}

#[test]
fn test_resolve_context_mode_stored_wins_over_desired() {
    let db = test_db();
    // codex P2 round 3: 「stored が desired に勝つ」のは index が空でない
    // ときのみの挙動になったため (空 index は adopt する、次の test 参照)、
    // この test の前提を「chunk が実在する non-empty index」に機械的に
    // 更新する (assert! 文言 / 期待値は不変、fixture のみ追加)。
    let doc_id = db
        .upsert_document("a.md", Some("T"), None, None, None, &[], None, "h", 0)
        .unwrap();
    db.insert_chunk(
        doc_id,
        0,
        Some("H"),
        Some(2),
        "body",
        None,
        &vec![0.0f32; 384],
        1.0,
    )
    .unwrap();
    db.write_context_mode(ContextMode::Off).unwrap();
    let m = resolve_context_mode(&db, ContextMode::Static, false).unwrap();
    assert_eq!(m, ContextMode::Off, "DB-stored mode wins on mismatch");
}

#[test]
fn test_resolve_context_mode_empty_index_adopts_desired_on_mismatch() {
    // codex P2 round 3: mode が記録済みでも index が空 (chunk 0 件) なら、
    // 守るべき embedding 空間が存在しないので `--force` を要求せず desired
    // を採用して記録を上書きする。round 1 の F2 fix (server 起動時に
    // resolve_context_mode を 1 回呼ぶ) の副作用として、1 件も index せず
    // config を反転して再起動するケースを想定。
    let db = test_db(); // 空 (chunk 0)
    db.write_context_mode(ContextMode::Static).unwrap();
    let m = resolve_context_mode(&db, ContextMode::Off, false).unwrap();
    assert_eq!(
        m,
        ContextMode::Off,
        "empty index adopts desired, not stored"
    );
    assert_eq!(
        db.read_context_mode().unwrap(),
        Some(ContextMode::Off),
        "adopted desired must be persisted, overwriting the stale stored value"
    );
}

#[test]
fn test_resolve_context_mode_empty_index_adopts_desired_on_mismatch_reverse() {
    // 逆方向 (stored=Off, desired=Static) でも同じ規則が適用されることの確認。
    let db = test_db();
    db.write_context_mode(ContextMode::Off).unwrap();
    let m = resolve_context_mode(&db, ContextMode::Static, false).unwrap();
    assert_eq!(
        m,
        ContextMode::Static,
        "empty index adopts desired, not stored"
    );
    assert_eq!(
        db.read_context_mode().unwrap(),
        Some(ContextMode::Static),
        "adopted desired must be persisted, overwriting the stale stored value"
    );
}

#[test]
fn test_resolve_context_mode_force_adopts_desired() {
    let db = test_db();
    db.write_context_mode(ContextMode::Off).unwrap();
    // force: reset 済み前提で desired を採用 + 記録
    let m = resolve_context_mode(&db, ContextMode::Static, true).unwrap();
    assert_eq!(m, ContextMode::Static);
    assert_eq!(db.read_context_mode().unwrap(), Some(ContextMode::Static));
}

#[test]
fn test_reset_and_resolve_context_mode_force_wipes_existing_chunks() {
    // codex P2 on PR #73 (F1) regression: MCP `rebuild_index` tool の force
    // 経路は (fix 前は) reset_for_model を呼ばずに resolve_context_mode だけ
    // 呼んでいたため、「新 mode を記録した直後もまだ旧 mode の chunk が
    // DB に残っている」瞬間が生じ得た。reset_and_resolve_context_mode は
    // force 時に resolve より前で必ず reset_for_model を呼ぶことでこれを防ぐ。
    // ここでは MCP 経路を model (Embedder 不要): 呼び出し側で reset を挟まず
    // 「旧 mode で index 済みの DB」に直接 force call するシナリオを再現する。
    let db = test_db();
    db.write_context_mode(ContextMode::Off).unwrap();
    let doc_id = db
        .upsert_document(
            "stale.md",
            Some("Stale"),
            None,
            None,
            None,
            &[],
            None,
            "h",
            0,
        )
        .unwrap();
    db.insert_chunk(
        doc_id,
        0,
        Some("H"),
        Some(2),
        "stale body",
        None,
        &vec![0.0f32; 384],
        1.0,
    )
    .unwrap();
    assert_eq!(db.chunk_count().unwrap(), 1, "precondition: DB has 1 chunk");

    let mode =
        reset_and_resolve_context_mode(&db, "bge-small-en-v1.5", 384, ContextMode::Static, true)
            .unwrap();

    assert_eq!(mode, ContextMode::Static);
    assert_eq!(db.read_context_mode().unwrap(), Some(ContextMode::Static));
    assert_eq!(
        db.chunk_count().unwrap(),
        0,
        "force must wipe pre-existing chunks before the new mode is recorded, \
             even when the caller (MCP rebuild_index tool) never called reset_for_model itself"
    );
    assert_eq!(
        db.document_count().unwrap(),
        0,
        "force must wipe pre-existing documents alongside chunks"
    );
}

#[test]
fn test_reset_and_resolve_context_mode_non_force_does_not_wipe() {
    // 対照テスト: force=false では reset を経由せず resolve_context_mode の
    // 通常挙動 (DB-stored mode 優先、chunk 保持) のまま。
    let db = test_db();
    db.write_context_mode(ContextMode::Off).unwrap();
    let doc_id = db
        .upsert_document("kept.md", Some("Kept"), None, None, None, &[], None, "h", 0)
        .unwrap();
    db.insert_chunk(
        doc_id,
        0,
        Some("H"),
        Some(2),
        "kept body",
        None,
        &vec![0.0f32; 384],
        1.0,
    )
    .unwrap();

    let mode =
        reset_and_resolve_context_mode(&db, "bge-small-en-v1.5", 384, ContextMode::Static, false)
            .unwrap();

    assert_eq!(mode, ContextMode::Off, "non-force keeps DB-stored mode");
    assert_eq!(
        db.chunk_count().unwrap(),
        1,
        "non-force must not wipe existing chunks"
    );
}

// -----------------------------------------------------------------------
// embed_input_for (feature-46 Task 2.7)
// -----------------------------------------------------------------------

#[test]
fn test_embed_input_static_prepends_context() {
    // clippy::field_reassign_with_default を避けるため struct literal で構築
    // (ロジック / assert 文言は brief のテストと不変)。
    let ch = crate::parser::Chunk {
        content: "body".to_string(),
        context: Some("T > H".to_string()),
        ..Default::default()
    };
    assert_eq!(embed_input_for(&ch, ContextMode::Static), "T > H\n\nbody");
}

#[test]
fn test_embed_input_off_is_content_only() {
    let ch = crate::parser::Chunk {
        content: "body".to_string(),
        context: Some("T > H".to_string()),
        ..Default::default()
    };
    // Off モードは parser が context を生成していても content のみ
    assert_eq!(embed_input_for(&ch, ContextMode::Off), "body");
}

#[test]
fn test_embed_input_static_none_context_is_content_only() {
    let ch = crate::parser::Chunk {
        content: "body".to_string(),
        context: None,
        ..Default::default()
    };
    assert_eq!(embed_input_for(&ch, ContextMode::Static), "body");
}

/// (AW-04) A run fails only when it refused at least one file's input and
/// embedded nothing. A run that embedded a file, or refused none, is not a
/// failure.
#[test]
fn fails_all_inputs_rejected_needs_a_rejection_and_no_embedded_file() {
    for (embed_rejected, embedded, fails) in [
        (0, 0, false),
        (1, 0, true),
        (3, 0, true),
        (1, 3, false),
        (0, 3, false),
    ] {
        let result = IndexResult {
            embed_rejected,
            embedded,
            ..IndexResult::default()
        };
        assert_eq!(
            result.fails_all_inputs_rejected(),
            fails,
            "rejected {embed_rejected}, embedded {embedded}"
        );
    }
    let message = IndexResult {
        embed_rejected: 2,
        ..IndexResult::default()
    }
    .all_inputs_rejected_message(REJECTIONS_NAMED_ABOVE);
    assert!(message.is_ascii(), "{message}");
    assert_eq!(
        message,
        "the embedding endpoint rejected every input this run tried to embed (2 file(s), \
             see the warnings above); nothing was embedded. Check [embedding] model / \
             document_model and endpoint. The rest of the run (deletions, bookkeeping) completed."
    );
    assert!(
        IndexResult {
            embed_rejected: 1,
            ..IndexResult::default()
        }
        .all_inputs_rejected_message(REJECTIONS_NAMED_ON_SERVER_STDERR)
        .contains("(1 file(s), each named on the server's stderr); nothing was embedded"),
        "{message}"
    );
}

/// (AW-04, codex P1 on PR #329) A forced rebuild fails on a single refused file: its
/// reset emptied the index, so the file is missing from it. An incremental run with the
/// same refusal keeps the file's old row and is not failed by this check.
#[test]
fn fails_forced_rebuild_rejections_needs_a_refusal_in_a_forced_run() {
    for (forced_rejected, fails) in [(0, false), (1, true), (4, true)] {
        let result = IndexResult {
            forced_rejected,
            embedded: 5,
            ..IndexResult::default()
        };
        assert_eq!(
            result.fails_forced_rebuild_rejections(),
            fails,
            "forced_rejected {forced_rejected}"
        );
    }
    let message = IndexResult {
        forced_rejected: 2,
        ..IndexResult::default()
    }
    .forced_rebuild_rejections_message(REJECTIONS_NAMED_ABOVE);
    assert!(message.is_ascii(), "{message}");
    assert_eq!(
        message,
        "the embedding endpoint rejected the input of 2 file(s) during a forced rebuild \
             (see the warnings above); the rebuild emptied the index first, so those files are \
             not in it. Fix them or lower [embedding] max_input_chars, then run `groove index`. \
             The rest of the run (deletions, bookkeeping) completed."
    );
    assert!(
        IndexResult {
            forced_rejected: 1,
            ..IndexResult::default()
        }
        .forced_rebuild_rejections_message(REJECTIONS_NAMED_ON_SERVER_STDERR)
        .contains("1 file(s) during a forced rebuild (each named on the server's stderr)"),
        "{message}"
    );
}

// -----------------------------------------------------------------------
// feature-62: the one-time .docx pass
// -----------------------------------------------------------------------

/// A registry that reads Markdown and docx.
fn docx_registry() -> Registry {
    Registry::from_enabled(&["md".into(), "docx".into()]).unwrap()
}

/// An index with two-dimensional vectors, matching [`unreachable_embedder`].
fn reparse_db() -> Database {
    let db = Database::open_in_memory().unwrap();
    db.verify_embedding_meta("test-model", 2).unwrap();
    db
}

/// An embedder that is never reached: these tests take paths that embed nothing, and one
/// that did would fail on the closed port rather than download a model.
fn unreachable_embedder() -> Embedder {
    let config = crate::embedder::OpenAiCompatibleConfig::new(
        "http://127.0.0.1:1/v1/embeddings".to_string(),
        "query-model".to_string(),
        "document-model".to_string(),
        2,
        false,
        None,
        std::time::Duration::from_secs(1),
    )
    .expect("a valid OpenAI-compatible config");
    Embedder::with_settings(crate::embedder::EmbeddingSettings::openai_compatible(
        config,
    ))
    .expect("building the provider makes no request")
}

/// Write `bytes` to `dir/rel` and return the entry the scan would hand the loop for it.
fn docx_entry(dir: &std::path::Path, rel: &str, bytes: &[u8]) -> DiskEntry {
    let full = dir.join(rel);
    std::fs::write(&full, bytes).unwrap();
    DiskEntry {
        rel: rel.to_string(),
        hash: sha256_hex_bytes(bytes),
        full,
        size: bytes.len() as u64,
    }
}

/// The chunks the registry's docx parser makes of `bytes`, under the default exclusions
/// [`index_single_disk_entry`] uses when none are configured.
fn parse_docx(registry: &Registry, rel: &str, bytes: &[u8]) -> Vec<crate::parser::Chunk> {
    use crate::parser::ParserExt;
    registry
        .by_extension("docx")
        .unwrap()
        .parse_bytes(bytes, rel, crate::parser::DEFAULT_EXCLUDED_HEADINGS)
        .unwrap()
        .chunks
}

/// A row for `rel` holding `chunks` under `content_hash`.
fn write_row(db: &Database, rel: &str, content_hash: &str, chunks: &[crate::parser::Chunk]) {
    let id = db
        .upsert_document(rel, None, None, None, None, &[], None, content_hash, 1)
        .unwrap();
    for (i, c) in chunks.iter().enumerate() {
        db.insert_chunk(
            id,
            i as i32,
            c.heading.as_deref(),
            c.level,
            &c.content,
            c.context.as_deref(),
            &[0.6_f32, 0.8],
            1.0,
        )
        .unwrap();
    }
}

/// [`index_single_disk_entry`] on `entry` in `mode`, under Off mode with no declared
/// fields, through [`unreachable_embedder`].
fn reindex(db: &Database, registry: &Registry, entry: &DiskEntry, mode: Reindex) -> SingleResult {
    let mut embedder = unreachable_embedder();
    index_single_disk_entry(
        db,
        &mut embedder,
        entry,
        None,
        registry,
        mode,
        ContextMode::Off,
        DeclaredSet::Known {
            list: &[],
            pass: None,
            generation: "[]",
        },
        false,
    )
    .unwrap()
}

/// feature-62 (R4.3 5, J9): a re-read whose chunks match the row's writes nothing and
/// reports the document unchanged, so it is not re-embedded and not counted as updated.
#[test]
fn a_reparse_whose_chunks_match_writes_nothing_and_reports_unchanged() {
    use crate::parser::docx::fixture::{PStyle, numeric_heading_docx};
    let dir = mk_tmp("f62-reparse-match");
    let (db, registry) = (reparse_db(), docx_registry());
    let bytes = numeric_heading_docx(PStyle::Empty);
    let entry = docx_entry(&dir.0, "quarry.docx", &bytes);
    let chunks = parse_docx(&registry, "quarry.docx", &bytes);
    write_row(&db, "quarry.docx", &entry.hash, &chunks);

    assert_eq!(
        reindex(&db, &registry, &entry, Reindex::Reparse),
        SingleResult::Unchanged
    );
    assert_eq!(
        db.get_document_hash("quarry.docx").unwrap(),
        Some(entry.hash.clone())
    );
}

/// feature-62 (R4.3 4, J17): a re-read that leaves nothing to index drops the row, the
/// way `--force` leaves none; an ordinary change that leaves nothing keeps it, as before.
#[test]
fn a_reparse_that_leaves_no_chunks_drops_the_row_and_an_ordinary_change_keeps_it() {
    use crate::parser::docx::fixture::{document_xml_from_body, docx_with_parts};
    let dir = mk_tmp("f62-reparse-empty");
    let registry = docx_registry();
    let empty_doc = document_xml_from_body("");
    let bytes = docx_with_parts(&[("word/document.xml", empty_doc.as_bytes())]);
    let entry = docx_entry(&dir.0, "hollow.docx", &bytes);
    let old = [crate::parser::Chunk {
        heading: Some("Written by the old rule".to_string()),
        level: None,
        content: "a body the new rule no longer yields".to_string(),
        ..Default::default()
    }];
    let no_chunks = SingleResult::Skipped {
        reason: SKIPPED_NO_CHUNKS,
        frontmatter_unparsed: false,
    };

    let db = reparse_db();
    write_row(&db, "hollow.docx", &entry.hash, &old);
    assert_eq!(reindex(&db, &registry, &entry, Reindex::Reparse), no_chunks);
    assert_eq!(
        db.get_document_hash("hollow.docx").unwrap(),
        None,
        "the row is dropped"
    );

    let db = reparse_db();
    write_row(&db, "hollow.docx", "hash-of-earlier-bytes", &old);
    let ordinary = Reindex::Incremental {
        check_frontmatter: false,
        refresh_fields: false,
    };
    assert_eq!(reindex(&db, &registry, &entry, ordinary), no_chunks);
    assert_eq!(
        db.get_document_hash("hollow.docx").unwrap().as_deref(),
        Some("hash-of-earlier-bytes"),
        "a changed file that yields nothing keeps its row, as before"
    );
}

/// feature-62 (R4.3 3, J10): a re-read whose bytes are not the ones the scan hashed says
/// nothing about the row, so it is a skip and the row is left alone.
#[test]
fn a_reparse_of_bytes_that_changed_since_the_scan_is_a_skip() {
    use crate::parser::docx::fixture::{PStyle, numeric_heading_docx};
    let dir = mk_tmp("f62-reparse-race");
    let (db, registry) = (reparse_db(), docx_registry());
    let bytes = numeric_heading_docx(PStyle::Empty);
    let entry = docx_entry(&dir.0, "quarry.docx", &bytes);
    let scanned = DiskEntry {
        hash: "hash-the-scan-saw".to_string(),
        ..entry
    };
    write_row(&db, "quarry.docx", &scanned.hash, &[]);
    assert_eq!(
        reindex(&db, &registry, &scanned, Reindex::Reparse),
        SingleResult::Skipped {
            reason: SKIPPED_CHANGED_DURING_READ,
            frontmatter_unparsed: false
        }
    );
    assert_eq!(
        db.get_document_hash("quarry.docx").unwrap().as_deref(),
        Some("hash-the-scan-saw")
    );
}

/// #543: a watcher event on a file written in place down to nothing it can embed -- an
/// empty file, a frontmatter-only stub -- drops the row the earlier text left, so the old
/// chunks stop answering searches; `--force` leaves no row for such a file either. On macOS
/// this is also the path an editor's remove-and-write takes, since FSEvents folds the two
/// into one change (kuriya trap #525).
#[test]
fn a_watcher_reindex_of_a_file_emptied_in_place_drops_its_row() {
    let registry = docx_registry();
    let old = [crate::parser::Chunk {
        heading: Some("Install".to_string()),
        level: Some(2),
        content: "Download the archive and unpack it into the tools folder.".to_string(),
        ..Default::default()
    }];
    for (name, emptied) in [("empty", ""), ("stub", "---\ntitle: Runbook\n---\n")] {
        let dir = mk_tmp(&format!("i543-emptied-{name}"));
        let db = reparse_db();
        write_row(&db, "runbook.md", "hash-of-the-earlier-text", &old);
        std::fs::write(dir.0.join("runbook.md"), emptied).unwrap();

        let mut embedder = unreachable_embedder();
        let result =
            reindex_single_file(&db, &mut embedder, &dir.0, "runbook.md", None, &registry).unwrap();
        assert!(
            !matches!(result, SingleResult::Updated { .. }),
            "{name}: nothing to embed, yet {result:?}"
        );
        assert_eq!(
            db.get_document_hash("runbook.md").unwrap(),
            None,
            "{name}: the earlier text's row is still indexed after {result:?}"
        );
        assert_eq!(
            result,
            SingleResult::NothingToIndex {
                deindexed: true,
                frontmatter_unparsed: false
            },
            "{name}"
        );
    }
}

/// #543: the same event on a file that had no row reports that nothing was deindexed, and
/// writes none.
#[test]
fn a_watcher_reindex_of_a_new_empty_file_writes_no_row() {
    let registry = docx_registry();
    let dir = mk_tmp("i543-new-empty");
    let db = reparse_db();
    std::fs::write(dir.0.join("blank.md"), "").unwrap();

    let mut embedder = unreachable_embedder();
    let result =
        reindex_single_file(&db, &mut embedder, &dir.0, "blank.md", None, &registry).unwrap();
    assert_eq!(
        result,
        SingleResult::NothingToIndex {
            deindexed: false,
            frontmatter_unparsed: false
        }
    );
    assert!(db.all_document_paths().unwrap().is_empty());
}

/// #543 (local Codex, medium): a watcher event hashes the file on its first read and parses
/// a second one. A file that was empty only for that second read -- an editor that truncates
/// before it writes -- says nothing about the row, so the row stays and the result is the
/// changed-during-read skip; the next event writes it again.
#[test]
fn a_watcher_reindex_whose_second_read_is_not_the_hashed_bytes_keeps_the_row() {
    let registry = docx_registry();
    let old = [crate::parser::Chunk {
        heading: Some("Install".to_string()),
        level: Some(2),
        content: "Download the archive and unpack it into the tools folder.".to_string(),
        ..Default::default()
    }];
    let dir = mk_tmp("i543-reread-emptied");
    let db = reparse_db();
    write_row(&db, "runbook.md", "hash-of-the-earlier-text", &old);
    // On disk for the second read: nothing. Hashed by the first read: text with a chunk.
    let entry = docx_entry(&dir.0, "runbook.md", b"");
    let hashed = DiskEntry {
        hash: sha256_hex_bytes(b"# Install\n\nDownload the archive and unpack it.\n"),
        ..entry
    };
    assert_eq!(
        reindex(&db, &registry, &hashed, Reindex::Event),
        SingleResult::Skipped {
            reason: SKIPPED_CHANGED_DURING_READ,
            frontmatter_unparsed: false
        }
    );
    assert_eq!(
        db.get_document_hash("runbook.md").unwrap().as_deref(),
        Some("hash-of-the-earlier-text"),
        "the row went on bytes the first read did not hash"
    );
}

/// feature-62 (R4.3, Review Focus 2): the pass re-reads a `.docx` by its parser, not its
/// spelling, only when its row's hash is the scan's, and never one a rename already forces.
#[test]
fn rereads_unchanged_docx_reads_the_parser_not_the_spelling() {
    let db = Database::open_in_memory().unwrap();
    let registry = docx_registry();
    for (rel, hash) in [
        ("upper.DOCX", "h-upper"),
        ("edited.docx", "h-old"),
        ("note.md", "h-note"),
        ("moved.docx", "h-moved"),
    ] {
        db.upsert_document(rel, None, None, None, None, &[], None, hash, 1)
            .unwrap();
    }
    let none: HashSet<String> = HashSet::new();
    let renamed: HashSet<String> = ["moved.docx".to_string()].into_iter().collect();
    let asks = |rel: &str, hash: &str, refresh: bool, forced: &HashSet<String>| {
        let entry = mk_entry(rel, hash);
        rereads_unchanged_docx(refresh, &db, &registry, &entry, forced).unwrap()
    };
    assert!(
        asks("upper.DOCX", "h-upper", true, &none),
        "upper case is a docx too"
    );
    assert!(
        !asks("edited.docx", "h-new", true, &none),
        "a changed file takes the ordinary path"
    );
    assert!(
        !asks("note.md", "h-note", true, &none),
        "Markdown is not this pass's"
    );
    assert!(
        !asks("fresh.docx", "h-fresh", true, &none),
        "no row, nothing to re-read"
    );
    assert!(asks("moved.docx", "h-moved", true, &none));
    assert!(
        !asks("moved.docx", "h-moved", true, &renamed),
        "a forced rename is re-read already"
    );
    assert!(
        !asks("upper.DOCX", "h-upper", false, &none),
        "the pass is not running"
    );
}

/// feature-62 (R4.6): the notice names the count and is ASCII, since it goes to stderr.
#[test]
fn the_reread_notice_counts_the_documents_in_ascii() {
    let notice = docx_reread_notice(3);
    assert_eq!(
        notice,
        "Re-reading 3 unchanged .docx document(s) once: headings now come from word/styles.xml; only documents whose sections change are re-embedded"
    );
    assert!(notice.is_ascii());
}

/// feature-64 U1 (AC22): the table notice names the count and the change, opens as
/// [`docx_reread_notice`] does, and is ASCII, since it goes to stderr.
#[test]
fn the_table_reread_notice_counts_the_documents_in_ascii() {
    let notice = docx_table_reread_notice(3);
    assert_eq!(
        notice,
        "Re-reading 3 unchanged .docx document(s) once: table rows are now one line each, cells separated by tabs; only documents whose sections change are re-embedded"
    );
    assert!(notice.starts_with("Re-reading 3 unchanged .docx document(s) once:"));
    assert!(notice.is_ascii());
}

/// feature-65 U1 (AC23): the pptx notice names the count and the change, opens as the docx
/// ones do, and is ASCII, since it goes to stderr.
#[test]
fn the_pptx_reread_notice_counts_the_documents_in_ascii() {
    let notice = pptx_reread_notice(3);
    assert_eq!(
        notice,
        "Re-reading 3 unchanged .pptx document(s) once: table rows are now one line each, cells separated by tabs, and line breaks are kept; only documents whose slides change are re-embedded"
    );
    assert!(notice.starts_with("Re-reading 3 unchanged .pptx document(s) once:"));
    assert!(notice.is_ascii());
}

/// feature-65 U2 (AC26): the general predicate re-reads a row of its own format only, by
/// the parser rather than the spelling; [`stale_row`] marks only rows of a stale pass.
#[test]
fn the_general_reread_predicate_follows_each_format() {
    let db = Database::open_in_memory().unwrap();
    let registry = Registry::from_enabled(&["md".into(), "docx".into(), "pptx".into()]).unwrap();
    for (rel, hash) in [
        ("deck.PPTX", "h-deck"),
        ("memo.docx", "h-memo"),
        ("edited.pptx", "h-old"),
        ("moved.pptx", "h-moved"),
    ] {
        db.upsert_document(rel, None, None, None, None, &[], None, hash, 1)
            .unwrap();
    }
    let none: HashSet<String> = HashSet::new();
    let renamed: HashSet<String> = ["moved.pptx".to_string()].into_iter().collect();
    let asks = |ext: &str, rel: &str, hash: &str, stale: bool, forced: &HashSet<String>| {
        rereads_unchanged_as(ext, stale, &db, &registry, &mk_entry(rel, hash), forced).unwrap()
    };
    assert!(
        asks("pptx", "deck.PPTX", "h-deck", true, &none),
        "upper case is a pptx too"
    );
    assert!(
        !asks("pptx", "memo.docx", "h-memo", true, &none),
        "a docx is not the pptx pass's"
    );
    assert!(
        !asks("pptx", "edited.pptx", "h-new", true, &none),
        "a changed file takes the ordinary path"
    );
    assert!(
        !asks("pptx", "fresh.pptx", "h-fresh", true, &none),
        "no row, nothing to re-read"
    );
    assert!(
        !asks("pptx", "moved.pptx", "h-moved", true, &renamed),
        "a forced rename is re-read already"
    );
    assert!(
        !asks("pptx", "deck.PPTX", "h-deck", false, &none),
        "the pass is not running"
    );
    assert!(asks("docx", "memo.docx", "h-memo", true, &none));

    let docx_stale = [
        OncePass {
            extension: "docx",
            stale: true,
        },
        OncePass {
            extension: "pptx",
            stale: false,
        },
    ];
    assert!(stale_row(&docx_stale, &db, &registry, "memo.docx").unwrap());
    assert!(!stale_row(&docx_stale, &db, &registry, "deck.PPTX").unwrap());
    let pptx_stale = [
        OncePass {
            extension: "docx",
            stale: false,
        },
        OncePass {
            extension: "pptx",
            stale: true,
        },
    ];
    assert!(!stale_row(&pptx_stale, &db, &registry, "memo.docx").unwrap());
    assert!(stale_row(&pptx_stale, &db, &registry, "deck.PPTX").unwrap());
    assert!(!stale_row(&pptx_stale, &db, &registry, "absent.pptx").unwrap());
}

/// Whether `s` has the shape of [`sha256_hex_bytes`]: 64 lowercase hex digits.
fn is_digest_shaped(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// feature-62 T20 (AC34 (a)): the placeholder a row awaits a re-parse under cannot be a
/// digest, so no file's hash matches it.
#[test]
fn the_awaiting_reparse_hash_cannot_be_a_sha256_digest() {
    assert!(
        is_digest_shaped(&sha256_hex_bytes(b"any bytes")),
        "premise: the digest's shape"
    );
    assert!(!HASH_AWAITING_REPARSE.is_empty());
    assert!(
        !is_digest_shaped(HASH_AWAITING_REPARSE),
        "{HASH_AWAITING_REPARSE}"
    );
}

/// feature-62 T21 (AC34 (b)): a row awaiting a re-parse is read on the ordinary path even
/// when the file is the one its chunks came from, and ends with the file's real hash.
/// Its chunks match, so nothing is embedded and the unreachable embedder is never called.
#[test]
fn a_row_awaiting_reparse_never_takes_the_unchanged_fast_path() {
    use crate::parser::docx::fixture::{PStyle, numeric_heading_docx};
    let dir = mk_tmp("f62-awaiting");
    let (db, registry) = (reparse_db(), docx_registry());
    let bytes = numeric_heading_docx(PStyle::Empty);
    let chunks = parse_docx(&registry, "quarry.docx", &bytes);
    write_row(&db, "quarry.docx", HASH_AWAITING_REPARSE, &chunks);
    let entry = docx_entry(&dir.0, "quarry.docx", &bytes);

    let ordinary = Reindex::Incremental {
        check_frontmatter: false,
        refresh_fields: false,
    };
    assert_eq!(
        reindex(&db, &registry, &entry, ordinary),
        SingleResult::Updated {
            chunks: chunks.len() as u32,
            frontmatter_unparsed: false
        }
    );
    assert_eq!(
        db.get_document_hash("quarry.docx").unwrap(),
        Some(entry.hash.clone())
    );
}

// -----------------------------------------------------------------------
// feature-66: chunks.table_rows
// -----------------------------------------------------------------------

/// feature-66: the JSON `chunks.table_rows` holds for the one chunk of
/// [`crate::parser::docx::fixture::table_docx`] (`"alder\tbirch\ncedar\tdamson"`).
const TARIFF_ROWS_JSON: &str =
    r#"[{"table":0,"start":0,"end":11},{"table":0,"start":12,"end":24}]"#;

/// feature-66: what the registry's parser for `ext` makes of `bytes`, under the default
/// exclusions [`index_single_disk_entry`] uses when none are configured.
fn parse_doc(
    registry: &Registry,
    ext: &str,
    rel: &str,
    bytes: &[u8],
) -> crate::parser::ParsedDocument {
    use crate::parser::ParserExt;
    registry
        .by_extension(ext)
        .unwrap()
        .parse_bytes(bytes, rel, crate::parser::DEFAULT_EXCLUDED_HEADINGS)
        .unwrap()
}

/// feature-66 (R5.2): the one write path stores each chunk's table rows: the parse's for a
/// docx, NULL for Markdown.
#[test]
fn a_written_document_keeps_its_table_rows() {
    use crate::parser::docx::fixture::table_docx;
    let db = reparse_db();
    let registry = docx_registry();
    let cases: [(&str, &str, Vec<u8>, Option<String>); 2] = [
        (
            "tariff.docx",
            "docx",
            table_docx(),
            Some(TARIFF_ROWS_JSON.to_string()),
        ),
        (
            "harbour.md",
            "md",
            b"# Harbour\n\nquiet harbour notes\n".to_vec(),
            None,
        ),
    ];
    for (rel, ext, bytes, expected) in cases {
        let parsed = parse_doc(&registry, ext, rel, &bytes);
        let parser = registry.by_extension(ext).unwrap();
        let embeddings = vec![vec![0.6_f32, 0.8]; parsed.chunks.len()];
        write_parsed_document(
            &db,
            rel,
            &parsed,
            parser,
            &embeddings,
            ContextMode::Off,
            "h",
            1,
            None,
            None,
        )
        .unwrap();
        let stored = db.chunk_table_rows_for_path(rel).unwrap();
        assert!(!stored.is_empty(), "{rel}");
        assert!(stored.iter().all(|s| *s == expected), "{rel}: {stored:?}");
    }
}

/// feature-66: a file-backed index in `dir` with two-dimensional vectors, matching
/// [`unreachable_embedder`], and the path a second connection opens it by.
fn file_db(dir: &std::path::Path) -> (Database, std::path::PathBuf) {
    let path = dir.join("index.db");
    let db = Database::open(path.to_str().unwrap()).unwrap();
    db.verify_embedding_meta("test-model", 2).unwrap();
    (db, path)
}

/// feature-66 X1 (AC22, Reparse): the column-only write decides from what the row holds,
/// then reads the row again under an immediate lock. Undisturbed, it writes the parse's
/// rows and the document is unchanged; when another connection rewrote the chunk text or
/// the hash in between, it writes nothing and the document is a changed-during-read skip.
#[test]
fn a_reparse_table_rows_write_rechecks_the_row_it_read() {
    use crate::parser::docx::fixture::table_docx;
    let dir = mk_tmp("f66-x1-reparse");
    let (db, path) = file_db(&dir.0);
    let registry = docx_registry();
    let chunks = parse_doc(&registry, "docx", "tariff.docx", &table_docx()).chunks;
    let cases: [(&str, Option<&str>); 3] = [
        ("steady.docx", None),
        (
            "rewritten.docx",
            Some(
                "UPDATE chunks SET content = 'another writer was here' WHERE document_id = (SELECT id FROM documents WHERE path = ?1)",
            ),
        ),
        (
            "rehashed.docx",
            Some("UPDATE documents SET content_hash = 'hash-another-writer-left' WHERE path = ?1"),
        ),
    ];
    for (rel, interruption) in cases {
        write_row(&db, rel, "hash-of-these-bytes", &chunks);
        let decision = decide_table_rows(&db, rel, &chunks)
            .unwrap()
            .expect("a docx row");
        assert_eq!(decision.stored_hash, "hash-of-these-bytes", "{rel}");
        assert!(!decision.matches, "{rel}: NULL is not the parse's rows");
        if let Some(sql) = interruption {
            rusqlite::Connection::open(&path)
                .unwrap()
                .execute(sql, [rel])
                .unwrap();
        }
        let result =
            write_reparsed_table_rows(&db, rel, &decision, &chunks, ContextMode::Off, false)
                .unwrap();
        let stored = db.chunk_table_rows_for_path(rel).unwrap();
        if interruption.is_none() {
            assert_eq!(result, SingleResult::Unchanged, "{rel}");
            assert_eq!(stored, vec![Some(TARIFF_ROWS_JSON.to_string())], "{rel}");
        } else {
            assert_eq!(
                result,
                SingleResult::Skipped {
                    reason: SKIPPED_CHANGED_DURING_READ,
                    frontmatter_unparsed: false
                },
                "{rel}"
            );
            assert_eq!(stored, vec![None], "{rel}: nothing written");
        }
    }
}

/// feature-66 X1 (AC22, the ordinary path): a changed file whose chunk text matches gets its
/// metadata either way. Undisturbed, its table rows are the parse's; when another connection
/// rewrote the hash between the decision and the write, they are NULL -- neither the parse's
/// nor what was there before -- and the result is still `Updated`.
#[test]
fn a_meta_update_marks_table_rows_only_while_the_row_holds() {
    use crate::parser::docx::fixture::table_docx;
    let dir = mk_tmp("f66-x1-meta");
    let (db, path) = file_db(&dir.0);
    let registry = docx_registry();
    let parsed = parse_doc(&registry, "docx", "tariff.docx", &table_docx());
    let declared = DeclaredSet::Known {
        list: &[],
        pass: None,
        generation: "[]",
    };
    let before = [crate::parser::row_span(0, 0, 5)];
    for (rel, interrupted) in [("steady.docx", false), ("moved.docx", true)] {
        write_row(&db, rel, "hash-before-the-edit", &parsed.chunks);
        db.update_chunk_table_rows(rel, &[Some(&before[..])])
            .unwrap();
        let decision = decide_table_rows(&db, rel, &parsed.chunks)
            .unwrap()
            .expect("a docx row");
        assert_eq!(decision.stored_hash, "hash-before-the-edit", "{rel}");
        if interrupted {
            rusqlite::Connection::open(&path)
                    .unwrap()
                    .execute(
                        "UPDATE documents SET content_hash = 'hash-another-writer-left' WHERE path = ?1",
                        [rel],
                    )
                    .unwrap();
        }
        let entry = mk_entry(rel, "hash-after-the-edit");
        let result = write_matching_chunks_meta(
            &db,
            &entry,
            &parsed,
            Some(&decision),
            ContextMode::Off,
            declared,
            1,
            None,
            None,
        )
        .unwrap();
        assert_eq!(
            result,
            Some(SingleResult::Updated {
                chunks: 1,
                frontmatter_unparsed: false
            }),
            "{rel}"
        );
        assert_eq!(
            db.get_document_hash(rel).unwrap().as_deref(),
            Some("hash-after-the-edit"),
            "{rel}: the metadata is written either way"
        );
        let expected = (!interrupted).then(|| TARIFF_ROWS_JSON.to_string());
        assert_eq!(
            db.chunk_table_rows_for_path(rel).unwrap(),
            vec![expected],
            "{rel}"
        );
    }
}

/// feature-66 Review Focus 5: a stored value that is not JSON does not match the parse, so
/// the re-read rewrites the column alone -- through the embedder that is never reached --
/// and reports the document unchanged.
#[test]
fn a_broken_stored_table_rows_value_is_rewritten() {
    use crate::parser::docx::fixture::table_docx;
    let dir = mk_tmp("f66-rf5");
    let (db, path) = file_db(&dir.0);
    let registry = docx_registry();
    let bytes = table_docx();
    let entry = docx_entry(&dir.0, "tariff.docx", &bytes);
    let chunks = parse_doc(&registry, "docx", "tariff.docx", &bytes).chunks;
    write_row(&db, "tariff.docx", &entry.hash, &chunks);
    rusqlite::Connection::open(&path)
        .unwrap()
        .execute("UPDATE chunks SET table_rows = 'not json'", [])
        .unwrap();
    assert_eq!(
        reindex(&db, &registry, &entry, Reindex::Reparse),
        SingleResult::Unchanged
    );
    assert_eq!(
        db.chunk_table_rows_for_path("tariff.docx").unwrap(),
        vec![Some(TARIFF_ROWS_JSON.to_string())]
    );
}

/// feature-66 X1 (R7.7, review fix round 1): a re-read whose row another writer rehashed
/// *before* stage one read it -- the watcher indexed newer bytes with the same text -- holds
/// a hash that is not the entry's. The column is not written from the older bytes; the
/// document is a changed-during-read skip, left unsettled.
#[test]
fn a_reparse_does_not_write_table_rows_under_a_hash_that_is_not_the_entrys() {
    use crate::parser::docx::fixture::table_docx;
    let dir = mk_tmp("f66-x1-prehash");
    let (db, path) = file_db(&dir.0);
    let registry = docx_registry();
    let bytes = table_docx();
    let entry = docx_entry(&dir.0, "tariff.docx", &bytes);
    let chunks = parse_doc(&registry, "docx", "tariff.docx", &bytes).chunks;
    write_row(&db, "tariff.docx", &entry.hash, &chunks);
    rusqlite::Connection::open(&path)
        .unwrap()
        .execute(
            "UPDATE documents SET content_hash = 'hash-another-writer-left' WHERE path = ?1",
            ["tariff.docx"],
        )
        .unwrap();
    assert_eq!(
        reindex(&db, &registry, &entry, Reindex::Reparse),
        SingleResult::Skipped {
            reason: SKIPPED_CHANGED_DURING_READ,
            frontmatter_unparsed: false
        }
    );
    assert_eq!(
        db.chunk_table_rows_for_path("tariff.docx").unwrap(),
        vec![None],
        "nothing written"
    );
    assert_eq!(
        db.get_document_hash("tariff.docx").unwrap().as_deref(),
        Some("hash-another-writer-left")
    );
}

/// feature-66 (carried from Task 3's review): [`Database::update_chunk_table_rows`] writes each entry's
/// JSON by chunk position, `None` as NULL, and a path with no document row is a silent
/// no-op, as [`Database::update_chunk_code_meta`] is.
#[test]
fn update_chunk_table_rows_writes_by_position_and_ignores_an_unknown_path() {
    let db = reparse_db();
    let registry = docx_registry();
    let one = parse_doc(
        &registry,
        "md",
        "ledger.md",
        b"# Ledger\n\nfirst ledger notes\n",
    )
    .chunks;
    let chunks = vec![one[0].clone(), one[0].clone()];
    write_row(&db, "ledger.md", "h", &chunks);
    let spans = [crate::parser::row_span(1, 2, 7)];
    db.update_chunk_table_rows("ledger.md", &[None, Some(&spans[..])])
        .unwrap();
    assert_eq!(
        db.chunk_table_rows_for_path("ledger.md").unwrap(),
        vec![None, Some(r#"[{"table":1,"start":2,"end":7}]"#.to_string())]
    );
    db.update_chunk_table_rows("ledger.md", &[Some(&[][..]), None])
        .unwrap();
    assert_eq!(
        db.chunk_table_rows_for_path("ledger.md").unwrap(),
        vec![Some("[]".to_string()), None]
    );
    db.update_chunk_table_rows("absent.md", &[Some(&spans[..])])
        .unwrap();
    assert!(
        db.chunk_table_rows_for_path("absent.md")
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        db.chunk_table_rows_for_path("ledger.md").unwrap(),
        vec![Some("[]".to_string()), None],
        "another document's rows are untouched"
    );
}

/// feature-66 (final review): a `.docx` whose chunks all carry `None` (an offset past `u32`)
/// still gets a decision, so a stored non-NULL column is written NULL -- through the meta
/// update that stores a new hash and through the re-read alike -- rather than left behind.
/// A Markdown document's all-`None` chunks still decide nothing.
#[test]
fn a_docx_with_no_computed_table_rows_has_its_column_written_null() {
    use crate::parser::docx::fixture::table_docx;
    let db = reparse_db();
    let registry = docx_registry();
    let mut parsed = parse_doc(&registry, "docx", "tariff.docx", &table_docx());
    for chunk in &mut parsed.chunks {
        chunk.table_rows = None;
    }
    let before = [crate::parser::row_span(0, 0, 5)];
    let declared = DeclaredSet::Known {
        list: &[],
        pass: None,
        generation: "[]",
    };

    write_row(&db, "edited.docx", "hash-before-the-edit", &parsed.chunks);
    db.update_chunk_table_rows("edited.docx", &[Some(&before[..])])
        .unwrap();
    let decision = decide_table_rows(&db, "edited.docx", &parsed.chunks)
        .unwrap()
        .expect("a docx row decides even with no computed rows");
    assert!(!decision.matches);
    let entry = mk_entry("edited.docx", "hash-after-the-edit");
    let result = write_matching_chunks_meta(
        &db,
        &entry,
        &parsed,
        Some(&decision),
        ContextMode::Off,
        declared,
        1,
        None,
        None,
    )
    .unwrap();
    assert!(matches!(result, Some(SingleResult::Updated { .. })));
    assert_eq!(
        db.get_document_hash("edited.docx").unwrap().as_deref(),
        Some("hash-after-the-edit")
    );
    assert_eq!(
        db.chunk_table_rows_for_path("edited.docx").unwrap(),
        vec![None]
    );

    write_row(&db, "reread.DOCX", "hash-of-these-bytes", &parsed.chunks);
    db.update_chunk_table_rows("reread.DOCX", &[Some(&before[..])])
        .unwrap();
    let decision = decide_table_rows(&db, "reread.DOCX", &parsed.chunks)
        .unwrap()
        .expect("the extension is compared without case");
    let result = write_reparsed_table_rows(
        &db,
        "reread.DOCX",
        &decision,
        &parsed.chunks,
        ContextMode::Off,
        false,
    )
    .unwrap();
    assert_eq!(result, SingleResult::Unchanged);
    assert_eq!(
        db.chunk_table_rows_for_path("reread.DOCX").unwrap(),
        vec![None]
    );

    let md = parse_doc(&registry, "md", "notes.md", b"# Notes\n\nplain notes\n").chunks;
    write_row(&db, "notes.md", "h", &md);
    assert!(decide_table_rows(&db, "notes.md", &md).unwrap().is_none());
}

/// feature-66 X2 (AC25): the two notices for a stale table-rows key alone name the count and
/// the change, open as the earlier notices do, and are ASCII, since they go to stderr.
#[test]
fn the_rows_notices_count_the_documents_in_ascii() {
    let docx = docx_rows_reread_notice(3);
    assert_eq!(
        docx,
        "Re-reading 3 unchanged .docx document(s) once: table rows are now marked in each chunk; only documents whose sections change are re-embedded"
    );
    let pptx = pptx_rows_reread_notice(3);
    assert_eq!(
        pptx,
        "Re-reading 3 unchanged .pptx document(s) once: table rows are now marked in each chunk; only documents whose slides change are re-embedded"
    );
    assert!(docx.is_ascii() && pptx.is_ascii());
    assert_eq!(DOCX_TABLE_ROWS_POLICY, "row-spans");
    assert_eq!(PPTX_TABLE_ROWS_POLICY, "row-spans");
}

/// (#544 C2/C3, P1-X) Only "not found" and "not a directory" (a file where a directory on
/// the way was) say a path is gone; any other failed stat says nothing about it.
#[test]
fn stat_error_means_gone_only_for_not_found_and_not_a_directory() {
    use std::io::ErrorKind::*;
    assert!(stat_error_means_gone(NotFound));
    assert!(stat_error_means_gone(NotADirectory));
    for kind in [PermissionDenied, Other, Interrupted, TimedOut, InvalidInput] {
        assert!(!stat_error_means_gone(kind), "{kind:?}");
    }
}

/// (#544 C2/C3) [`path_is_there`] answers `Ok(true)` for a file, `Ok(false)` for a missing
/// path and for one whose directory on the way is a file, as `Path::exists` did.
#[test]
fn path_is_there_reads_a_missing_path_and_a_file_on_the_way_as_not_there() {
    let root = crate::test_support::unique_temp_path("path-is-there");
    std::fs::create_dir_all(&root).unwrap();
    struct Cleanup(std::path::PathBuf);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    let _cleanup = Cleanup(root.clone());
    let file = root.join("a.md");
    std::fs::write(&file, "x").unwrap();
    assert!(path_is_there(&file).unwrap());
    assert!(!path_is_there(&root.join("missing.md")).unwrap());
    assert!(!path_is_there(&file.join("x.md")).unwrap());
}

/// (#544, GitHub codex r1 P2 on PR #395) [`sort_single_path_look`]: a stat that failed for
/// a reason that says nothing about being gone (a loop of links, a denied permission) is
/// settled as a path through a link when a link is on the way, and is returned only when
/// none is.
#[test]
fn a_failed_stat_with_a_link_on_the_way_is_a_path_through_a_link() {
    use std::io::{Error, ErrorKind};
    let look = |kind: ErrorKind, link: bool| sort_single_path_look(Err(Error::from(kind)), || link);
    for kind in [ErrorKind::PermissionDenied, ErrorKind::Other] {
        assert_eq!(
            look(kind, true).unwrap(),
            SinglePathLook::ThroughALink,
            "{kind:?}"
        );
        assert_eq!(look(kind, false).unwrap_err().kind(), kind);
    }
    assert_eq!(
        sort_single_path_look(Ok(false), || true).unwrap(),
        SinglePathLook::ThroughALink
    );
    assert_eq!(
        sort_single_path_look(Ok(false), || false).unwrap(),
        SinglePathLook::Missing
    );
    assert_eq!(
        sort_single_path_look(Ok(true), || true).unwrap(),
        SinglePathLook::There
    );
}

// -----------------------------------------------------------------------
// #544 PR-2: the row's hash comes from the bytes its chunks were built from
// -----------------------------------------------------------------------

/// #544 PR-2: the bytes the second read finds, a Markdown file of two sections.
const SECOND_READ_BYTES: &[u8] =
    b"# Alpha\n\nThe first section of the file.\n\n# Beta\n\nThe second section of the file.\n";

/// #544 PR-2: the bytes the scan hashed, which the second read no longer finds.
const SCANNED_BYTES: &[u8] = b"# Gamma\n\nThe one section the scan read.\n";

/// #544 PR-2: [`index_single_disk_entry`] on `entry` in `mode`, under Off mode with no
/// declared fields, through the embedder it is handed, with `row_dropped_unless_updated` as the caller
/// passes it. [`reindex`] with the two arguments it fixes left to the test.
fn reindex_with(
    db: &Database,
    registry: &Registry,
    entry: &DiskEntry,
    mode: Reindex,
    embedder: &mut Embedder,
    row_dropped_unless_updated: bool,
) -> SingleResult {
    index_single_disk_entry(
        db,
        embedder,
        entry,
        None,
        registry,
        mode,
        ContextMode::Off,
        DeclaredSet::Known {
            list: &[],
            pass: None,
            generation: "[]",
        },
        row_dropped_unless_updated,
    )
    .unwrap()
}

/// #544 PR-2: the chunk bodies the index holds for `rel`, in order.
fn stored_bodies(db: &Database, rel: &str) -> Vec<String> {
    db.stored_chunk_vectors_for_path(rel)
        .unwrap()
        .into_iter()
        .map(|row| row.content)
        .collect()
}

/// #544 PR-2: the chunk bodies the Markdown parser makes of `bytes`.
fn parsed_bodies(registry: &Registry, rel: &str, bytes: &[u8]) -> Vec<String> {
    parse_doc(registry, "md", rel, bytes)
        .chunks
        .into_iter()
        .map(|c| c.content)
        .collect()
}

/// The ordinary changed-content mode, as the scan and a watcher rename with changed bytes
/// call it.
const INCREMENTAL: Reindex = Reindex::Incremental {
    check_frontmatter: false,
    refresh_fields: false,
};

/// #544 PR-2 (P2-I1, the meta-only write): the row's chunks already are the second read's,
/// so only the metadata and the hash are written -- and the hash written is the one of the
/// bytes those chunks came from, not the scan's.
#[test]
fn a_second_read_that_differs_writes_its_own_hash_on_a_meta_update() {
    let dir = mk_tmp("i544-second-read-meta");
    let (db, registry) = (reparse_db(), docx_registry());
    let chunks = parse_doc(&registry, "md", "note.md", SECOND_READ_BYTES).chunks;
    write_row(&db, "note.md", "hash-of-the-row", &chunks);
    let entry = DiskEntry {
        hash: "hash-the-scan-saw".to_string(),
        ..docx_entry(&dir.0, "note.md", SECOND_READ_BYTES)
    };

    let result = reindex_with(
        &db,
        &registry,
        &entry,
        INCREMENTAL,
        &mut unreachable_embedder(),
        false,
    );
    assert!(
        matches!(result, SingleResult::Updated { .. }),
        "the meta-only write, yet {result:?}"
    );
    assert_eq!(
        db.get_document_hash("note.md").unwrap(),
        Some(sha256_hex_bytes(SECOND_READ_BYTES)),
        "the row's hash is not the hash of the bytes its chunks came from"
    );
}

/// #544 PR-2 (P2-I1, the whole write): the row holds one chunk the second read no longer
/// has, so the document is written again -- each kept chunk with its stored vector, so
/// nothing is embedded -- under the hash of the bytes just parsed.
#[test]
fn a_second_read_that_differs_writes_its_own_hash_on_a_full_write() {
    let dir = mk_tmp("i544-second-read-full");
    let (db, registry) = (reparse_db(), docx_registry());
    let mut chunks = parse_doc(&registry, "md", "note.md", SECOND_READ_BYTES).chunks;
    chunks.push(crate::parser::Chunk {
        heading: Some("Delta".to_string()),
        level: Some(1),
        content: "A section the file no longer has.".to_string(),
        ..Default::default()
    });
    write_row(&db, "note.md", "hash-of-the-row", &chunks);
    let entry = DiskEntry {
        hash: "hash-the-scan-saw".to_string(),
        ..docx_entry(&dir.0, "note.md", SECOND_READ_BYTES)
    };

    let result = reindex_with(
        &db,
        &registry,
        &entry,
        INCREMENTAL,
        &mut unreachable_embedder(),
        false,
    );
    assert!(
        matches!(result, SingleResult::Updated { .. }),
        "the whole write, yet {result:?}"
    );
    assert_eq!(
        db.get_document_hash("note.md").unwrap(),
        Some(sha256_hex_bytes(SECOND_READ_BYTES)),
        "the row's hash is not the hash of the bytes its chunks came from"
    );
    assert_eq!(
        stored_bodies(&db, "note.md"),
        parsed_bodies(&registry, "note.md", SECOND_READ_BYTES),
        "the row's chunks are not the second read's"
    );
}

/// #544 PR-2 (P2-I1, Review Focus 2): after the meta-only write above, the file goes back to
/// the bytes the scan hashed. The row does not hold those bytes' chunks, so the next
/// incremental pass must read the file again rather than take the unchanged fast path.
#[test]
fn an_incremental_write_whose_bytes_return_to_the_scanned_ones_is_read_again() {
    let dir = mk_tmp("i544-second-read-return");
    let (db, registry) = (reparse_db(), docx_registry());
    let chunks = parse_doc(&registry, "md", "note.md", SECOND_READ_BYTES).chunks;
    write_row(&db, "note.md", "hash-of-the-row", &chunks);
    let scanned_hash = sha256_hex_bytes(SCANNED_BYTES);
    let entry = DiskEntry {
        hash: scanned_hash.clone(),
        ..docx_entry(&dir.0, "note.md", SECOND_READ_BYTES)
    };
    let first = reindex_with(
        &db,
        &registry,
        &entry,
        INCREMENTAL,
        &mut unreachable_embedder(),
        false,
    );
    assert!(
        matches!(first, SingleResult::Updated { .. }),
        "the meta-only write, yet {first:?}"
    );

    let back = docx_entry(&dir.0, "note.md", SCANNED_BYTES);
    assert_eq!(back.hash, scanned_hash);
    let again = reindex_with(
        &db,
        &registry,
        &back,
        INCREMENTAL,
        &mut Embedder::stub_for_tests(2),
        false,
    );
    assert_ne!(
        again,
        SingleResult::Unchanged,
        "the row holds the other bytes' chunks under these bytes' hash"
    );
    assert_eq!(
        db.get_document_hash("note.md").unwrap(),
        Some(scanned_hash),
        "the row's hash after the bytes came back"
    );
    assert_eq!(
        stored_bodies(&db, "note.md"),
        parsed_bodies(&registry, "note.md", SCANNED_BYTES),
        "the row's chunks are not the bytes on disk"
    );
}

/// #544 PR-2 (P2-I2): a forced write embeds every chunk of the second read and writes them
/// under the hash of the bytes they came from (Review Focus 4: the chunks are that parse's).
#[test]
fn a_forced_second_read_that_differs_writes_its_own_hash() {
    let dir = mk_tmp("i544-second-read-force");
    let (db, registry) = (reparse_db(), docx_registry());
    let old = parse_doc(&registry, "md", "note.md", SCANNED_BYTES).chunks;
    write_row(&db, "note.md", "hash-the-scan-saw", &old);
    let entry = DiskEntry {
        hash: "hash-the-scan-saw".to_string(),
        ..docx_entry(&dir.0, "note.md", SECOND_READ_BYTES)
    };

    let result = reindex_with(
        &db,
        &registry,
        &entry,
        Reindex::Force,
        &mut Embedder::stub_for_tests(2),
        false,
    );
    assert!(
        matches!(result, SingleResult::Updated { .. }),
        "a forced write, yet {result:?}"
    );
    assert_eq!(
        db.get_document_hash("note.md").unwrap(),
        Some(sha256_hex_bytes(SECOND_READ_BYTES)),
        "the row's hash is not the hash of the bytes its chunks came from"
    );
    assert_eq!(
        stored_bodies(&db, "note.md"),
        parsed_bodies(&registry, "note.md", SECOND_READ_BYTES),
        "the row's chunks are not the second read's"
    );
}

/// #544 PR-2 (P2-I3): the two modes [`rename_single_file`] reparses a destination in --
/// forced when the first read's hash is the row's, the ordinary changed-content mode when
/// it is not -- each write the hash of the bytes the second read found.
#[test]
fn a_rename_reparse_whose_second_read_differs_writes_its_own_hash() {
    let registry = docx_registry();
    let first_read_hash = "hash-the-first-read-saw";

    // Same hash: the row moved under the hash the rename's first read took.
    let dir = mk_tmp("i544-second-read-rename-force");
    let db = reparse_db();
    let old = parse_doc(&registry, "md", "moved.md", SCANNED_BYTES).chunks;
    write_row(&db, "moved.md", first_read_hash, &old);
    let entry = DiskEntry {
        hash: first_read_hash.to_string(),
        ..docx_entry(&dir.0, "moved.md", SECOND_READ_BYTES)
    };
    let forced = reindex_with(
        &db,
        &registry,
        &entry,
        Reindex::Force,
        &mut Embedder::stub_for_tests(2),
        false,
    );
    assert!(
        matches!(forced, SingleResult::Updated { .. }),
        "same hash: {forced:?}"
    );
    assert_eq!(
        db.get_document_hash("moved.md").unwrap(),
        Some(sha256_hex_bytes(SECOND_READ_BYTES)),
        "same hash (Force): the row's hash is not the second read's"
    );

    // Changed: the row's chunks already are the second read's, so the meta-only write.
    let dir = mk_tmp("i544-second-read-rename-changed");
    let db = reparse_db();
    let chunks = parse_doc(&registry, "md", "moved.md", SECOND_READ_BYTES).chunks;
    write_row(&db, "moved.md", "hash-of-the-row", &chunks);
    let entry = DiskEntry {
        hash: first_read_hash.to_string(),
        ..docx_entry(&dir.0, "moved.md", SECOND_READ_BYTES)
    };
    let changed = reindex_with(
        &db,
        &registry,
        &entry,
        INCREMENTAL,
        &mut unreachable_embedder(),
        false,
    );
    assert!(
        matches!(changed, SingleResult::Updated { .. }),
        "changed: {changed:?}"
    );
    assert_eq!(
        db.get_document_hash("moved.md").unwrap(),
        Some(sha256_hex_bytes(SECOND_READ_BYTES)),
        "changed (Incremental): the row's hash is not the second read's"
    );
}

/// #544 PR-2 (P2-I4): a reparse across a parser whose second read is not the bytes the scan
/// hashed learned nothing about the file, so it is the changed-during-read skip and the
/// settlement keeps the row, under the awaiting-reparse hash. Here the second read finds
/// nothing to chunk, which the settlement otherwise answers by dropping the row.
#[test]
fn a_cross_parser_reparse_whose_second_read_differs_keeps_the_row_awaiting() {
    let dir = mk_tmp("i544-second-read-cross-differs");
    let (db, registry) = (reparse_db(), docx_registry());
    let old = parse_doc(&registry, "md", "note.md", SCANNED_BYTES).chunks;
    let scanned_hash = sha256_hex_bytes(SCANNED_BYTES);
    write_row(&db, "note.md", &scanned_hash, &old);
    let entry = DiskEntry {
        hash: scanned_hash,
        ..docx_entry(&dir.0, "note.md", b"")
    };

    let result = reindex_with(
        &db,
        &registry,
        &entry,
        Reindex::Force,
        &mut Embedder::stub_for_tests(2),
        true,
    );
    settle_cross_parser_rename(&db, "note.md", &result).unwrap();
    assert_eq!(
        result,
        SingleResult::Skipped {
            reason: SKIPPED_CHANGED_DURING_READ,
            frontmatter_unparsed: false
        }
    );
    assert_eq!(
        db.get_document_hash("note.md").unwrap().as_deref(),
        Some(HASH_AWAITING_REPARSE),
        "the row went, or kept a hash a fast path trusts"
    );
    assert_eq!(
        stored_bodies(&db, "note.md"),
        parsed_bodies(&registry, "note.md", SCANNED_BYTES),
        "the row's chunks changed"
    );
}

/// #544 PR-2 (P2-I4, the control, Review Focus 3): the same reparse whose second read is
/// the bytes the scan hashed did learn the file has nothing to chunk, so the settlement
/// drops the row as before. Green before the fix and after it.
#[test]
fn a_cross_parser_reparse_whose_second_read_matches_still_drops_on_no_chunks() {
    let dir = mk_tmp("i544-second-read-cross-matches");
    let (db, registry) = (reparse_db(), docx_registry());
    let old = parse_doc(&registry, "md", "note.md", SCANNED_BYTES).chunks;
    let entry = docx_entry(&dir.0, "note.md", b"");
    write_row(&db, "note.md", &entry.hash, &old);

    let result = reindex_with(
        &db,
        &registry,
        &entry,
        Reindex::Force,
        &mut Embedder::stub_for_tests(2),
        true,
    );
    settle_cross_parser_rename(&db, "note.md", &result).unwrap();
    assert_eq!(
        result,
        SingleResult::Skipped {
            reason: SKIPPED_NO_CHUNKS,
            frontmatter_unparsed: false
        }
    );
    assert_eq!(
        db.get_document_hash("note.md").unwrap(),
        None,
        "the row the old parser wrote is still there"
    );
}

/// #544 PR-2 (B5): the settlement keeps the row of a cross-parser rename whose second read
/// changed under it, under the awaiting-reparse hash; every other skip, and a refusal, still
/// drops it (J3).
#[test]
fn settling_a_cross_parser_rename_keeps_a_changed_during_read_skip_awaiting_reparse() {
    let db = reparse_db();
    db.upsert_document("note.md", None, None, None, None, &[], None, "h1", 10)
        .unwrap();
    settle_cross_parser_rename(
        &db,
        "note.md",
        &SingleResult::Skipped {
            reason: SKIPPED_CHANGED_DURING_READ,
            frontmatter_unparsed: false,
        },
    )
    .unwrap();
    assert_eq!(
        db.get_document_hash("note.md").unwrap().as_deref(),
        Some(HASH_AWAITING_REPARSE),
        "the changed-during-read skip dropped the row, or left a hash a fast path trusts"
    );

    let dropped = [
        SingleResult::Skipped {
            reason: "parse failed",
            frontmatter_unparsed: false,
        },
        SingleResult::Refused,
        SingleResult::Skipped {
            reason: SKIPPED_NO_CHUNKS,
            frontmatter_unparsed: false,
        },
        SingleResult::Skipped {
            reason: SKIPPED_EMBED_REJECTED,
            frontmatter_unparsed: false,
        },
    ];
    for outcome in &dropped {
        db.upsert_document("note.md", None, None, None, None, &[], None, "h1", 10)
            .unwrap();
        settle_cross_parser_rename(&db, "note.md", outcome).unwrap();
        assert_eq!(
            db.get_document_hash("note.md").unwrap(),
            None,
            "{outcome:?} left the old parser's row"
        );
    }
}

/// #544 PR-2 (B4): on a cross-parser reparse the second read is checked against the hash
/// before the bytes are parsed, so bytes the scan did not hash cannot be the reason the row
/// is dropped. The bytes here fail to parse, which the matching case shows.
#[test]
fn a_cross_parser_reparse_whose_second_read_differs_skips_before_parsing() {
    let dir = mk_tmp("i544-second-read-cross-before-parse");
    let (db, registry) = (reparse_db(), docx_registry());
    let matching = docx_entry(&dir.0, "note.docx", b"not a zip archive");

    let control = reindex_with(
        &db,
        &registry,
        &matching,
        Reindex::Force,
        &mut unreachable_embedder(),
        true,
    );
    assert_eq!(
        control,
        SingleResult::Skipped {
            reason: "parse failed",
            frontmatter_unparsed: false
        },
        "the bytes were meant to fail to parse"
    );

    let differs = DiskEntry {
        hash: "hash-the-scan-saw".to_string(),
        ..matching
    };
    let result = reindex_with(
        &db,
        &registry,
        &differs,
        Reindex::Force,
        &mut unreachable_embedder(),
        true,
    );
    assert_eq!(
        result,
        SingleResult::Skipped {
            reason: SKIPPED_CHANGED_DURING_READ,
            frontmatter_unparsed: false
        }
    );
}

/// #544 PR-2 (P2-I5): `note.txt` renamed to `note.md` -- a pair that crosses a parser --
/// whose row moved under the first read's hash, then reparsed by [`reindex_renamed_entry`]
/// as [`rename_single_file`] does for same-hash bytes, while the file on disk holds
/// `disk_bytes`, not the bytes the first read hashed. Returns the scratch directory, the
/// index, the registry and the outcome.
fn cross_parser_rename_whose_second_read_differs(
    tag: &str,
    disk_bytes: &[u8],
) -> (TmpDir, Database, Registry, RenameOutcome) {
    let dir = mk_tmp(tag);
    let (db, registry) = (reparse_db(), docx_registry());
    assert!(
        rename_crosses_a_parser(&registry, "note.txt", "note.md"),
        "the pair was meant to cross a parser"
    );
    let first_read_hash = sha256_hex_bytes(SCANNED_BYTES);
    let old = parse_doc(&registry, "md", "note.md", SCANNED_BYTES).chunks;
    write_row(&db, "note.md", &first_read_hash, &old);
    let entry = DiskEntry {
        hash: first_read_hash,
        ..docx_entry(&dir.0, "note.md", disk_bytes)
    };
    let outcome = reindex_renamed_entry(
        &db,
        &mut unreachable_embedder(),
        &entry,
        Reindex::Force,
        ContextMode::Off,
        None,
        &registry,
        true,
    )
    .unwrap();
    (dir, db, registry, outcome)
}

/// #544 PR-2 (P2-I5): a cross-parser rename whose second read is not the bytes the first
/// read hashed keeps the row, under the awaiting-reparse hash, and says so -- not that the
/// document was dropped.
#[test]
fn a_cross_parser_rename_whose_second_read_differs_says_it_kept_the_row() {
    let (_dir, db, registry, outcome) =
        cross_parser_rename_whose_second_read_differs("i544-p2i5-kept", SECOND_READ_BYTES);
    assert_eq!(
        outcome,
        RenameOutcome::RenamedAcrossParsersAwaitingReparse,
        "the outcome does not say the row was kept"
    );
    assert_eq!(
        db.get_document_hash("note.md").unwrap().as_deref(),
        Some(HASH_AWAITING_REPARSE),
        "the row went, or kept a hash a fast path trusts"
    );
    assert_eq!(
        stored_bodies(&db, "note.md"),
        parsed_bodies(&registry, "note.md", SCANNED_BYTES),
        "the kept row's chunks changed"
    );
}

/// #544 PR-2 (H1): the row a cross-parser rename left awaiting a reparse is read again by
/// the next event on the bytes now on disk, rather than taken as unchanged: bytes with
/// chunks write the row under their own hash, and bytes with none drop it.
#[test]
fn a_row_left_awaiting_after_a_cross_parser_rename_is_reparsed_by_the_next_event() {
    let (dir, db, registry, outcome) =
        cross_parser_rename_whose_second_read_differs("i544-h1-chunks", SECOND_READ_BYTES);
    assert_eq!(outcome, RenameOutcome::RenamedAcrossParsersAwaitingReparse);
    let event = docx_entry(&dir.0, "note.md", SECOND_READ_BYTES);
    let result = reindex_with(
        &db,
        &registry,
        &event,
        Reindex::Event,
        &mut Embedder::stub_for_tests(2),
        false,
    );
    assert!(
        matches!(result, SingleResult::Updated { .. }),
        "bytes with chunks: {result:?}"
    );
    assert_eq!(
        db.get_document_hash("note.md").unwrap(),
        Some(sha256_hex_bytes(SECOND_READ_BYTES)),
        "the row's hash is not the hash of the bytes on disk"
    );
    assert_eq!(
        stored_bodies(&db, "note.md"),
        parsed_bodies(&registry, "note.md", SECOND_READ_BYTES),
        "the row's chunks are not the bytes on disk"
    );

    let (dir, db, registry, outcome) =
        cross_parser_rename_whose_second_read_differs("i544-h1-empty", b"");
    assert_eq!(outcome, RenameOutcome::RenamedAcrossParsersAwaitingReparse);
    let event = docx_entry(&dir.0, "note.md", b"");
    let result = reindex_with(
        &db,
        &registry,
        &event,
        Reindex::Event,
        &mut unreachable_embedder(),
        false,
    );
    assert!(
        matches!(
            result,
            SingleResult::NothingToIndex {
                deindexed: true,
                ..
            }
        ),
        "bytes with no chunks: {result:?}"
    );
    assert_eq!(
        db.get_document_hash("note.md").unwrap(),
        None,
        "the old parser's row is still there"
    );
}

/// #544 PR-2 (local Codex r1): a full run over `old_rel`'s row renamed on disk to `new_rel`
/// -- a pair that crosses a parser -- whose file is rewritten to `rewritten` once the rename
/// is applied (the [`progress::ProgressEvent::Renamed`] callback), so the loop's read of it
/// is not the bytes the scan hashed. Returns the scratch directory, the index, the
/// registry and the run's result.
fn crossed_rename_rewritten_during_the_run(
    tag: &str,
    (old_rel, new_rel): (&str, &str),
    scanned: &'static [u8],
    rewritten: &'static [u8],
) -> (TmpDir, Database, Registry, IndexResult) {
    let dir = mk_tmp(tag);
    let db = reparse_db();
    let registry = Registry::from_enabled(&["md".into(), "txt".into(), "docx".into()]).unwrap();
    assert!(
        rename_crosses_a_parser(&registry, old_rel, new_rel),
        "the pair was meant to cross a parser"
    );
    std::fs::write(dir.0.join(new_rel), scanned).unwrap();
    let old = parse_doc(&registry, "md", old_rel, SCANNED_BYTES).chunks;
    write_row(&db, old_rel, &sha256_hex_bytes(scanned), &old);
    let full = dir.0.join(new_rel);
    let (old_owned, new_owned) = (old_rel.to_string(), new_rel.to_string());
    let progress = progress::ProgressReporter::with_callback(Box::new(move |event| {
        if let progress::ProgressEvent::Renamed { old, new } = event
            && old == old_owned
            && new == new_owned
        {
            std::fs::write(&full, rewritten).unwrap();
        }
    }));
    let result = rebuild_index(
        &db,
        &mut Embedder::stub_for_tests(2),
        &dir.0,
        None,
        false,
        None,
        &[],
        &registry,
        progress,
        ContextMode::Off,
    )
    .unwrap();
    (dir, db, registry, result)
}

/// #544 PR-2 (local Codex r1): a crossed rename the loop kept awaiting a reparse, because
/// the file changed between the scan and the loop's read, is read once more before the run
/// ends, and that read writes the row from the bytes on disk, under their hash. It is
/// counted as the update it became, not as the loop's skip.
#[test]
fn a_full_run_reads_a_crossed_rename_it_kept_awaiting_once_more() {
    let (_dir, db, registry, result) = crossed_rename_rewritten_during_the_run(
        "i544-cx1-rebuild-second-read",
        ("note.txt", "note.md"),
        SCANNED_BYTES,
        SECOND_READ_BYTES,
    );
    assert_eq!(
        db.get_document_hash("note.md").unwrap(),
        Some(sha256_hex_bytes(SECOND_READ_BYTES)),
        "the run returned with the row awaiting, or under a hash other than the disk's"
    );
    assert_eq!(
        stored_bodies(&db, "note.md"),
        parsed_bodies(&registry, "note.md", SECOND_READ_BYTES),
        "the row's chunks are not the bytes on disk"
    );
    assert_eq!(db.get_document_hash("note.txt").unwrap(), None);
    assert_eq!((result.updated, result.skipped), (1, 0), "{result:?}");
}

/// #544 PR-2 (local Codex r3, J3): the same run, and the bytes on disk are ones the new
/// parser cannot read. The further read verified them, so its parse failure is settled as
/// the rename's first read would be: the old parser's row goes, and the run counts one
/// skip. (Before r3 the row was kept awaiting, and stayed listed with a kb URI that
/// `get_document` of [`crate::server`] could no longer open.)
#[test]
fn a_full_run_drops_a_crossed_rename_when_the_further_read_cannot_parse() {
    let (_dir, db, _registry, result) = crossed_rename_rewritten_during_the_run(
        "i544-cx1-rebuild-second-read-unparsed",
        ("note.md", "note.docx"),
        b"not a zip archive, as scanned",
        b"not a zip archive, as rewritten",
    );
    assert_eq!(
        db.get_document_hash("note.docx").unwrap(),
        None,
        "the old parser's row is still there"
    );
    assert!(stored_bodies(&db, "note.docx").is_empty());
    assert_eq!(db.get_document_hash("note.md").unwrap(), None);
    assert_eq!((result.updated, result.skipped), (0, 1), "{result:?}");
}

/// #544 PR-2 (local Codex r4, D5): a crossed rename the loop kept awaiting, because the file
/// changed under the loop's read, whose further read then cannot read the file at all (it
/// is gone by then; a sharing or permission error reads the same). No bytes were checked,
/// so the row stays, under the awaiting-reparse hash, for the next event or run.
#[test]
fn a_full_run_keeps_a_crossed_rename_awaiting_when_the_further_read_fails() {
    let dir = mk_tmp("i544-cx4-rebuild-further-read-fails");
    let db = reparse_db();
    let registry = Registry::from_enabled(&["md".into(), "txt".into(), "docx".into()]).unwrap();
    std::fs::write(dir.0.join("note.md"), SCANNED_BYTES).unwrap();
    let old = parse_doc(&registry, "md", "note.txt", SCANNED_BYTES).chunks;
    write_row(&db, "note.txt", &sha256_hex_bytes(SCANNED_BYTES), &old);
    let full = dir.0.join("note.md");
    let progress = progress::ProgressReporter::with_callback(Box::new(move |event| {
        match event {
            // The loop's read sees other bytes than the scan hashed: kept awaiting.
            progress::ProgressEvent::Renamed { new: "note.md", .. } => {
                std::fs::write(&full, SECOND_READ_BYTES).unwrap();
            }
            // Reported by the loop, before the further read: that read finds nothing.
            progress::ProgressEvent::Unchanged { rel: "note.md", .. } => {
                std::fs::remove_file(&full).unwrap();
            }
            _ => {}
        }
    }));
    rebuild_index(
        &db,
        &mut Embedder::stub_for_tests(2),
        &dir.0,
        None,
        false,
        None,
        &[],
        &registry,
        progress,
        ContextMode::Off,
    )
    .unwrap();
    assert!(
        !dir.0.join("note.md").exists(),
        "the further read was meant to find no file"
    );
    assert_eq!(
        db.get_document_hash("note.md").unwrap().as_deref(),
        Some(HASH_AWAITING_REPARSE),
        "a further read that checked no bytes dropped the row, or left a hash a fast path trusts"
    );
}

/// #544 PR-2 (local Codex r5): the further read of a crossed rename kept awaiting comes back
/// with a skip its settlement does not name as one that may drop the row (here an extension
/// no parser takes, decided before any byte is read). The row stays, under the
/// awaiting-reparse hash.
#[test]
fn a_further_crossed_read_with_an_unnamed_skip_keeps_the_row_awaiting() {
    let dir = mk_tmp("i544-cx5-unnamed-skip");
    let db = reparse_db();
    write_row(&db, "note.xyz", HASH_AWAITING_REPARSE, &[]);
    let full = dir.0.join("note.xyz");
    std::fs::write(&full, SCANNED_BYTES).unwrap();
    let entry = DiskEntry {
        rel: "note.xyz".to_string(),
        hash: sha256_hex_bytes(SCANNED_BYTES),
        full,
        size: SCANNED_BYTES.len() as u64,
    };
    let result = read_crossed_rename_again(
        &db,
        &mut unreachable_embedder(),
        &entry,
        None,
        &docx_registry(),
        ContextMode::Off,
        DeclaredSet::FromIndex,
    )
    .unwrap();
    assert!(
        matches!(result, SingleResult::Skipped { reason, .. }
                if !matches!(reason, "parse failed" | SKIPPED_NO_CHUNKS | SKIPPED_EMBED_REJECTED
                    | "file no longer exists" | "file too large")),
        "the read was meant to end in a skip the settlement does not name: {result:?}"
    );
    assert!(!settle_further_crossed_read(&db, "note.xyz", &result).unwrap());
    assert_eq!(
        db.get_document_hash("note.xyz").unwrap().as_deref(),
        Some(HASH_AWAITING_REPARSE),
        "a skip nobody named dropped the row"
    );
}

/// #544 PR-2 (GitHub codex P2 on PR #396): the further read of a crossed rename kept
/// awaiting finds the file grown past the text cap since the scan. The read refuses it, and
/// the length it measured is recorded on the row, as the other refusals record theirs, so
/// the resource surface stops offering a document a read now rejects. The row itself stays
/// awaiting (D5).
#[test]
fn the_further_read_of_a_crossed_rename_records_the_size_it_refused() {
    use crate::parser::{CodeParsersConfig, FileSizeLimit, FileSizeLimits};
    let dir = mk_tmp("i544-gh1-further-read-oversize");
    let db = reparse_db();
    let limits = FileSizeLimits {
        text: FileSizeLimit::Bytes(16),
        ..FileSizeLimits::default()
    };
    let ids = ["md".to_string(), "txt".to_string()];
    let registry =
        Registry::from_enabled_with_plugins(&ids, &CodeParsersConfig::default(), None, limits)
            .expect("md + txt");
    write_row(&db, "note.md", HASH_AWAITING_REPARSE, &[]);
    let grown = [b'x'; 64];
    let full = dir.0.join("note.md");
    std::fs::write(&full, grown).unwrap();
    let entry = DiskEntry {
        rel: "note.md".to_string(),
        hash: sha256_hex_bytes(b"small"),
        full,
        size: 5,
    };
    let result = read_crossed_rename_again(
        &db,
        &mut unreachable_embedder(),
        &entry,
        None,
        &registry,
        ContextMode::Off,
        DeclaredSet::FromIndex,
    )
    .unwrap();
    assert!(matches!(result, SingleResult::Refused), "{result:?}");
    assert_eq!(
        db.documents_larger_than(0).unwrap(),
        vec![("note.md".to_string(), grown.len() as u64)],
        "the row kept a size the read has just disproved"
    );
    assert_eq!(
        db.get_document_hash("note.md").unwrap().as_deref(),
        Some(HASH_AWAITING_REPARSE),
        "the refused row left the awaiting state"
    );
}

/// kuriya 670: one chunk row's `(chunk id, start_line, end_line, symbol_kind,
/// quality_score)`.
#[cfg(feature = "grammar-rust")]
type K670Columns = (i64, Option<i64>, Option<i64>, Option<String>, f64);

/// kuriya 670: the per-chunk columns of `rel`'s rows as the index file holds them, read
/// through a connection of its own (the [`Database`] keeps its connection private), in
/// chunk order.
#[cfg(feature = "grammar-rust")]
fn chunk_columns_in_file(db_path: &std::path::Path, rel: &str) -> Vec<K670Columns> {
    let conn = rusqlite::Connection::open(db_path).unwrap();
    let mut stmt = conn
        .prepare(
            "SELECT c.id, c.start_line, c.end_line, c.symbol_kind, c.quality_score
                   FROM chunks c JOIN documents d ON d.id = c.document_id
                  WHERE d.path = ?1 ORDER BY c.chunk_index",
        )
        .unwrap();
    stmt.query_map([rel], |r| {
        Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
    })
    .unwrap()
    .collect::<Result<_, _>>()
    .unwrap()
}

/// kuriya 670: `old_rel` indexed by a full run from `scanned`, then renamed on disk to
/// `new_rel` -- a pair that crosses a parser -- and a second full run whose loop finds the
/// file rewritten to `rewritten` once it has applied the rename, so that loop keeps the row
/// awaiting and the run's further read ([`read_crossed_rename_again_explained`]) reads
/// `rewritten`. The index lives in a file outside the knowledge base so the test can read
/// columns [`Database`] has no accessor for. Returns the knowledge base, the directory of the
/// index, the index file, the index, and the old path's chunk columns after the first run
/// ([`chunk_columns_in_file`]).
#[cfg(feature = "grammar-rust")]
fn crossed_rename_rewritten_after_an_earlier_run(
    tag: &str,
    (old_rel, new_rel): (&str, &str),
    scanned: &'static [u8],
    rewritten: &'static [u8],
) -> (
    TmpDir,
    TmpDir,
    std::path::PathBuf,
    Database,
    Vec<K670Columns>,
) {
    let kb = mk_tmp(tag);
    let db_dir = mk_tmp(&format!("{tag}-db"));
    let db_path = db_dir.0.join("index.db");
    let db = Database::open(db_path.to_str().unwrap()).unwrap();
    db.verify_embedding_meta("test-model", 2).unwrap();
    let registry = Registry::from_enabled(&["md".into(), "txt".into(), "rs".into()]).unwrap();
    assert!(
        rename_crosses_a_parser(&registry, old_rel, new_rel),
        "the pair was meant to cross a parser"
    );
    let run = |progress: progress::ProgressReporter| {
        rebuild_index(
            &db,
            &mut Embedder::stub_for_tests(2),
            &kb.0,
            None,
            false,
            None,
            &[],
            &registry,
            progress,
            ContextMode::Off,
        )
        .unwrap()
    };
    std::fs::write(kb.0.join(old_rel), scanned).unwrap();
    run(progress::ProgressReporter::with_callback(Box::new(|_| {})));
    assert_eq!(
        db.get_document_hash(old_rel).unwrap(),
        Some(sha256_hex_bytes(scanned)),
        "premise: the first run indexed the old path"
    );
    let first = chunk_columns_in_file(&db_path, old_rel);
    // Indexed by the second run's loop, ahead of the further read: a chunk row with a higher
    // id than the old path's, so a row written again gets an id of its own (SQLite gives a
    // new row the highest id plus one) while a meta-only write keeps the old one.
    std::fs::write(kb.0.join(K670_OTHER_REL), K670_OTHER).unwrap();
    std::fs::rename(kb.0.join(old_rel), kb.0.join(new_rel)).unwrap();
    let full = kb.0.join(new_rel);
    let (old_owned, new_owned) = (old_rel.to_string(), new_rel.to_string());
    run(progress::ProgressReporter::with_callback(Box::new(
        move |event| {
            if let progress::ProgressEvent::Renamed { old, new } = event
                && old == old_owned
                && new == new_owned
            {
                std::fs::write(&full, rewritten).unwrap();
            }
        },
    )));
    (kb, db_dir, db_path, db, first)
}

/// kuriya 670: a Rust file with no definition, one comment line long enough that the code
/// parser keeps it as a chunk (with a line range, and no heading or symbol kind).
#[cfg(feature = "grammar-rust")]
const K670_RS_SCANNED: &[u8] =
    b"// the deploy notes moved to the wiki; this file only keeps the pointer to them\n";
/// kuriya 670: [`K670_RS_SCANNED`] without its trailing newline: other bytes, whose one
/// plain-text chunk has the same (empty) heading and the same text as the code parser's
/// chunk of the scanned bytes.
#[cfg(feature = "grammar-rust")]
const K670_RS_REWRITTEN: &[u8] =
    b"// the deploy notes moved to the wiki; this file only keeps the pointer to them";
/// kuriya 670: a second document, indexed after the old path's, whose chunk row takes a
/// higher id. See [`k670_index_other`].
#[cfg(feature = "grammar-rust")]
const K670_OTHER_REL: &str = "zz-other.md";
#[cfg(feature = "grammar-rust")]
const K670_OTHER: &[u8] =
    b"## Other\n\nA second document, so that a chunk row written again gets a new id.\n";

/// kuriya 670: index [`K670_OTHER`] the way a watcher event would, so its chunk row takes an
/// id above the old path's: a row written again afterwards gets a new id, while the meta-only
/// write keeps the old one. That makes "the row was rewritten whole" checkable.
#[cfg(feature = "grammar-rust")]
fn k670_index_other(db: &Database, kb: &std::path::Path, registry: &Registry) {
    std::fs::write(kb.join(K670_OTHER_REL), K670_OTHER).unwrap();
    let result = reindex_single_file_checked(
        db,
        &mut Embedder::stub_for_tests(2),
        kb,
        K670_OTHER_REL,
        None,
        registry,
        false,
    )
    .unwrap();
    assert!(
        matches!(result, SingleResult::Updated { .. }),
        "premise: the other document was indexed: {result:?}"
    );
}

/// kuriya 670: a crossed rename (`.rs` to `.txt`) the loop kept awaiting, because the file
/// changed under its read, is read once more ([`read_crossed_rename_again_explained`]); the
/// bytes it finds give the new parser a chunk whose text matches the old parser's row. That
/// read must still write the row whole, as the rename's first read ([`Reindex::Renamed`])
/// would: the meta-only write would keep the code parser's line range on a plain-text chunk,
/// which the text parser never writes, and nothing after it (the fast path holds the new
/// hash) would take it off before `--force`.
#[test]
#[cfg(feature = "grammar-rust")]
fn a_further_crossed_read_does_not_keep_the_old_parsers_line_range() {
    let (_kb, _db_dir, db_path, db, first) = crossed_rename_rewritten_after_an_earlier_run(
        "k670-rs-to-txt",
        ("note.rs", "note.txt"),
        K670_RS_SCANNED,
        K670_RS_REWRITTEN,
    );
    assert!(
        matches!(first.as_slice(), [(_, Some(_), Some(_), None, _)]),
        "premise: the code parser wrote one chunk with a line range: {first:?}"
    );
    assert_eq!(
        db.get_document_hash("note.txt").unwrap(),
        Some(sha256_hex_bytes(K670_RS_REWRITTEN)),
        "premise: the further read settled the row from the rewritten bytes"
    );
    assert_eq!(db.get_document_hash("note.rs").unwrap(), None);
    let columns = chunk_columns_in_file(&db_path, "note.txt");
    assert_eq!(columns.len(), 1, "{columns:?}");
    assert_ne!(
        columns[0].0, first[0].0,
        "the further read kept the old chunk row (the meta-only write), not a rewrite: {first:?} -> {columns:?}"
    );
    let (_, start, end, kind, _) = &columns[0];
    assert_eq!(
        (start, end, kind),
        (&None, &None, &None),
        "the plain-text chunk kept the code parser's columns: {first:?} -> {columns:?}"
    );
}

/// kuriya 670, the watcher's side: the same row (the code parser's chunk of
/// [`K670_RS_SCANNED`], moved to `note.txt` and kept under [`HASH_AWAITING_REPARSE`], as
/// [`rename_single_file`] leaves a crossed rename whose first read saw other bytes) read once
/// more the way the watcher reads it ([`reindex_single_file_checked`] with the check on) and
/// settled with [`settle_further_crossed_read`]. The plain-text chunk does not keep the code
/// parser's line range.
#[test]
#[cfg(feature = "grammar-rust")]
fn the_watchers_further_crossed_read_does_not_keep_the_old_parsers_line_range() {
    let kb = mk_tmp("k670-watcher");
    let db_dir = mk_tmp("k670-watcher-db");
    let db_path = db_dir.0.join("index.db");
    let db = Database::open(db_path.to_str().unwrap()).unwrap();
    db.verify_embedding_meta("test-model", 2).unwrap();
    let registry = Registry::from_enabled(&["md".into(), "txt".into(), "rs".into()]).unwrap();
    std::fs::write(kb.0.join("note.rs"), K670_RS_SCANNED).unwrap();
    rebuild_index(
        &db,
        &mut Embedder::stub_for_tests(2),
        &kb.0,
        None,
        false,
        None,
        &[],
        &registry,
        progress::ProgressReporter::with_callback(Box::new(|_| {})),
        ContextMode::Off,
    )
    .unwrap();
    let first = chunk_columns_in_file(&db_path, "note.rs");
    assert!(
        matches!(first.as_slice(), [(_, Some(_), Some(_), None, _)]),
        "premise: the code parser wrote one chunk with a line range: {first:?}"
    );
    k670_index_other(&db, &kb.0, &registry);
    db.overwrite_content_hash(&["note.rs"], HASH_AWAITING_REPARSE)
        .unwrap();
    db.rename_document("note.rs", "note.txt", crate::db::FrontmatterTopic::Unread)
        .unwrap();
    std::fs::remove_file(kb.0.join("note.rs")).unwrap();
    std::fs::write(kb.0.join("note.txt"), K670_RS_REWRITTEN).unwrap();

    let result = reindex_single_file_checked(
        &db,
        &mut Embedder::stub_for_tests(2),
        &kb.0,
        "note.txt",
        None,
        &registry,
        true,
    )
    .unwrap();
    assert!(
        matches!(result, SingleResult::Updated { chunks: 1, .. }),
        "{result:?}"
    );
    assert!(!settle_further_crossed_read(&db, "note.txt", &result).unwrap());
    assert_eq!(
        db.get_document_hash("note.txt").unwrap(),
        Some(sha256_hex_bytes(K670_RS_REWRITTEN))
    );
    let columns = chunk_columns_in_file(&db_path, "note.txt");
    assert!(
        columns.len() == 1 && columns[0].0 != first[0].0,
        "the further read kept the old chunk row (the meta-only write), not a rewrite: {first:?} -> {columns:?}"
    );
    assert!(
        matches!(columns.as_slice(), [(_, None, None, None, _)]),
        "the plain-text chunk kept the code parser's columns: {first:?} -> {columns:?}"
    );
}

/// kuriya 670 (reviewer M1): the watcher's rename itself, when the bytes changed with it --
/// `note.rs` renamed to `note.txt` and its trailing newline dropped. [`rename_single_file`]
/// moves the row and reads the new bytes as an ordinary changed file
/// ([`Reindex::Incremental`], not [`Reindex::Renamed`]), settled as a crossed rename. The
/// plain-text chunk's text matches the code parser's row, and the row must still be written
/// whole: the meta-only write would keep the code parser's line range.
#[test]
#[cfg(feature = "grammar-rust")]
fn a_crossed_rename_with_changed_bytes_does_not_keep_the_old_parsers_line_range() {
    let kb = mk_tmp("k670-rename-changed");
    let db_dir = mk_tmp("k670-rename-changed-db");
    let db_path = db_dir.0.join("index.db");
    let db = Database::open(db_path.to_str().unwrap()).unwrap();
    db.verify_embedding_meta("test-model", 2).unwrap();
    let registry = Registry::from_enabled(&["md".into(), "txt".into(), "rs".into()]).unwrap();
    std::fs::write(kb.0.join("note.rs"), K670_RS_SCANNED).unwrap();
    rebuild_index(
        &db,
        &mut Embedder::stub_for_tests(2),
        &kb.0,
        None,
        false,
        None,
        &[],
        &registry,
        progress::ProgressReporter::with_callback(Box::new(|_| {})),
        ContextMode::Off,
    )
    .unwrap();
    let first = chunk_columns_in_file(&db_path, "note.rs");
    assert!(
        matches!(first.as_slice(), [(_, Some(_), Some(_), None, _)]),
        "premise: the code parser wrote one chunk with a line range: {first:?}"
    );
    k670_index_other(&db, &kb.0, &registry);
    std::fs::remove_file(kb.0.join("note.rs")).unwrap();
    std::fs::write(kb.0.join("note.txt"), K670_RS_REWRITTEN).unwrap();

    let outcome = rename_single_file(
        &db,
        &mut Embedder::stub_for_tests(2),
        &kb.0,
        "note.rs",
        "note.txt",
        None,
        &registry,
    )
    .unwrap();
    assert_eq!(
        db.get_document_hash("note.txt").unwrap(),
        Some(sha256_hex_bytes(K670_RS_REWRITTEN)),
        "premise: the rename indexed the new bytes ({outcome:?})"
    );
    assert_eq!(db.get_document_hash("note.rs").unwrap(), None);
    let columns = chunk_columns_in_file(&db_path, "note.txt");
    assert!(
        columns.len() == 1 && columns[0].0 != first[0].0,
        "the rename kept the old chunk row (the meta-only write), not a rewrite: {first:?} -> {columns:?}"
    );
    assert!(
        matches!(columns.as_slice(), [(_, None, None, None, _)]),
        "the plain-text chunk kept the code parser's columns: {first:?} -> {columns:?}"
    );
}

/// #544 PR-2 (local Codex r2): the run of
/// [`a_full_run_reads_a_crossed_rename_it_kept_awaiting_once_more`] starts with no frontmatter
/// policy recorded, so its loop holds the one-time check pending for the crossed rename onto
/// `.md` it kept awaiting. The further read writes that row from the bytes on disk, so the
/// check is no longer pending when the run ends, and the run records the policy.
#[test]
fn a_crossed_rename_the_further_read_settles_does_not_hold_the_frontmatter_policy() {
    let (_dir, db, _registry, _result) = crossed_rename_rewritten_during_the_run(
        "i544-cx2-rebuild-policy-settled",
        ("note.txt", "note.md"),
        SCANNED_BYTES,
        SECOND_READ_BYTES,
    );
    assert_eq!(
        db.get_document_hash("note.md").unwrap(),
        Some(sha256_hex_bytes(SECOND_READ_BYTES)),
        "the further read did not settle the row"
    );
    assert_eq!(
        db.read_frontmatter_policy().unwrap().as_deref(),
        Some(FRONTMATTER_POLICY),
        "the run left the check pending for a row its further read settled"
    );
}

/// #544 PR-2 (local Codex r2): the same run, and the bytes on disk are empty. The further
/// read finds nothing to embed and drops the row, so nothing holds the check pending and
/// the run records the policy.
#[test]
fn a_crossed_rename_the_further_read_drops_does_not_hold_the_frontmatter_policy() {
    let (_dir, db, _registry, _result) = crossed_rename_rewritten_during_the_run(
        "i544-cx2-rebuild-policy-dropped",
        ("note.txt", "note.md"),
        SCANNED_BYTES,
        b"",
    );
    assert_eq!(
        db.get_document_hash("note.md").unwrap(),
        None,
        "the further read did not drop the row"
    );
    assert_eq!(
        db.read_frontmatter_policy().unwrap().as_deref(),
        Some(FRONTMATTER_POLICY),
        "the run left the check pending for a row its further read dropped"
    );
}

/// #544 PR-2 (local Codex r3, J3): the same run, and the bytes on disk are not UTF-8, so the
/// further read fails to parse them. The read verified those bytes, so the row is dropped;
/// no row is left to hold the check pending, and the run records the policy.
#[test]
fn a_crossed_rename_the_further_read_cannot_parse_is_dropped_and_releases_the_frontmatter_policy() {
    let (_dir, db, _registry, _result) = crossed_rename_rewritten_during_the_run(
        "i544-cx2-rebuild-policy-unsettled",
        ("note.txt", "note.md"),
        SCANNED_BYTES,
        b"\xff\xfe not UTF-8, as rewritten",
    );
    assert_eq!(
        db.get_document_hash("note.md").unwrap(),
        None,
        "the old parser's row is still there"
    );
    assert_eq!(
        db.read_frontmatter_policy().unwrap().as_deref(),
        Some(FRONTMATTER_POLICY),
        "the run left the check pending for a row its further read dropped"
    );
}

/// #552: Markdown whose chunks under the Markdown parser are not the ones the plain-text
/// parser makes of the same bytes, so a row the text parser wrote is told apart from one
/// the Markdown parser wrote, and the reparse after a rename embeds rather than reuses.
const I552_BYTES: &[u8] = b"# Harbour\n\nThe tides turn twice a day at the harbour mouth.\n";

/// #552: Markdown that the Markdown parser makes no chunk of (a frontmatter-only stub), so
/// the reparse of a crossed rename ends in a skip, which the settlement answers by deleting
/// the row.
const I552_NO_CHUNK_BYTES: &[u8] = b"---\ntitle: x\n---\n";

/// #552: a scratch knowledge base holding `bytes` at `note.md`, and an index whose row for
/// `note.txt` holds what the plain-text parser made of the same bytes, under their hash:
/// the state a `note.txt` -> `note.md` rename leaves before anything reads it. The pair
/// crosses a parser.
fn i552_crossed_rename(tag: &str, bytes: &[u8]) -> (TmpDir, Database, Registry) {
    let dir = mk_tmp(tag);
    let db = reparse_db();
    let registry = Registry::from_enabled(&["md".into(), "txt".into()]).unwrap();
    assert!(
        rename_crosses_a_parser(&registry, "note.txt", "note.md"),
        "the pair was meant to cross a parser"
    );
    std::fs::write(dir.0.join("note.md"), bytes).unwrap();
    let txt_chunks = parse_doc(&registry, "txt", "note.txt", bytes).chunks;
    write_row(&db, "note.txt", &sha256_hex_bytes(bytes), &txt_chunks);
    (dir, db, registry)
}

/// #552: makes every delete of the `note.md` row fail, as a write the settlement of a
/// crossed rename makes would fail on a locked or full database.
fn i552_fail_deleting_note_md(db: &Database) {
    db.execute_for_test(concat!(
        "CREATE TABLE fault (armed INTEGER NOT NULL); INSERT INTO fault VALUES (1); ",
        "CREATE TRIGGER inject_delete_failure BEFORE DELETE ON documents ",
        "WHEN OLD.path = 'note.md' AND (SELECT armed FROM fault) = 1 ",
        "BEGIN SELECT RAISE(ABORT, 'injected delete failure'); END;"
    ))
    .unwrap();
}

/// [`rebuild_index`] over `dir`, not forced, under Off mode, with a quiet reporter.
fn i552_rebuild(
    db: &Database,
    embedder: &mut Embedder,
    dir: &TmpDir,
    registry: &Registry,
) -> Result<IndexResult> {
    rebuild_index(
        db,
        embedder,
        &dir.0,
        None,
        false,
        None,
        &[],
        registry,
        progress::ProgressReporter::new(progress::ProgressMode::Quiet),
        ContextMode::Off,
    )
}

/// #552 (R2): a full run pairs `note.txt` -> `note.md`, a rename that crosses a parser,
/// moves the row, and then fails to embed the Markdown parser's chunks. The row under
/// `note.md` still holds the text parser's chunks, so it must not be left under the hash of
/// the bytes on disk: the next run would find the hash current and keep those chunks until
/// `--force`. Left awaiting a reparse instead, the next run reads it as Markdown.
///
/// Red before the fix: the row keeps `sha256(bytes)`, and the next run counts it unchanged.
#[test]
fn i552_a_full_run_that_fails_to_embed_a_crossed_rename_leaves_it_awaiting() {
    let (dir, db, registry) = i552_crossed_rename("i552-r2-embed", I552_BYTES);
    assert_ne!(
        stored_bodies(&db, "note.txt"),
        parsed_bodies(&registry, "note.md", I552_BYTES),
        "the two parsers were meant to make different chunks of the bytes"
    );
    let err = i552_rebuild(&db, &mut unreachable_embedder(), &dir, &registry)
        .expect_err("the embed on the closed port was meant to fail the run");
    assert!(
        format!("{err:#}").contains("failed to embed chunks for note.md"),
        "the run failed somewhere other than the crossed rename's embed: {err:#}"
    );
    assert_eq!(
        db.get_document_hash("note.md").unwrap().as_deref(),
        Some(HASH_AWAITING_REPARSE),
        "the moved row holds the old parser's chunks under a hash the fast path trusts"
    );

    let result = i552_rebuild(&db, &mut Embedder::stub_for_tests(2), &dir, &registry)
        .expect("the second run");
    assert_eq!(
        result.updated, 1,
        "the second run did not reread note.md: {result:?}"
    );
    assert_eq!(
        db.get_document_hash("note.md").unwrap(),
        Some(sha256_hex_bytes(I552_BYTES))
    );
    assert_eq!(
        stored_bodies(&db, "note.md"),
        parsed_bodies(&registry, "note.md", I552_BYTES),
        "the row's chunks are still the old parser's"
    );
}

/// #552 (R3): a full run pairs the crossed rename, the Markdown parser makes no chunk of the
/// bytes, and the settlement's delete of the row fails. The row moved to `note.md` with the
/// text parser's chunks; it must not keep the hash of the bytes on disk.
///
/// Red before the fix: the row keeps `sha256(bytes)`.
#[test]
fn i552_a_full_run_whose_crossed_settlement_fails_leaves_it_awaiting() {
    let (dir, db, registry) = i552_crossed_rename("i552-r3-settle", I552_NO_CHUNK_BYTES);
    i552_fail_deleting_note_md(&db);
    let err = i552_rebuild(&db, &mut Embedder::stub_for_tests(2), &dir, &registry)
        .expect_err("the injected delete failure was meant to fail the run");
    assert!(
        format!("{err:#}").contains("injected"),
        "the run failed somewhere other than the injected fault: {err:#}"
    );
    assert_eq!(
        db.get_document_hash("note.md").unwrap().as_deref(),
        Some(HASH_AWAITING_REPARSE),
        "the moved row holds the old parser's chunks under a hash the fast path trusts"
    );
}

/// #552 (W4): the watcher's [`rename_single_file`] of `note.txt` -> `note.md`, a rename
/// that crosses a parser, moves the row and then fails to embed the Markdown parser's
/// chunks. The row under `note.md` must be left awaiting a reparse, not under the hash of
/// the bytes on disk.
///
/// Red before the fix: the row keeps `sha256(bytes)`.
#[test]
fn i552_a_watcher_rename_that_fails_to_embed_across_a_parser_leaves_it_awaiting() {
    let (dir, db, registry) = i552_crossed_rename("i552-w4-embed", I552_BYTES);
    let err = rename_single_file(
        &db,
        &mut unreachable_embedder(),
        &dir.0,
        "note.txt",
        "note.md",
        None,
        &registry,
    )
    .expect_err("the embed on the closed port was meant to fail the rename");
    assert!(
        format!("{err:#}").contains("failed to embed chunks for note.md"),
        "the rename failed somewhere other than the embed: {err:#}"
    );
    assert_eq!(db.get_document_hash("note.txt").unwrap(), None);
    assert_eq!(
        db.get_document_hash("note.md").unwrap().as_deref(),
        Some(HASH_AWAITING_REPARSE),
        "the moved row holds the old parser's chunks under a hash the fast path trusts"
    );
}

/// #552 (W5): the watcher's [`rename_single_file`] of the crossed rename, the Markdown
/// parser makes no chunk of the bytes, and the settlement's delete of the row fails. The
/// row moved to `note.md` must be left awaiting a reparse.
///
/// Red before the fix: the row keeps `sha256(bytes)`.
#[test]
fn i552_a_watcher_rename_whose_crossed_settlement_fails_leaves_it_awaiting() {
    let (dir, db, registry) = i552_crossed_rename("i552-w5-settle", I552_NO_CHUNK_BYTES);
    i552_fail_deleting_note_md(&db);
    let err = rename_single_file(
        &db,
        &mut Embedder::stub_for_tests(2),
        &dir.0,
        "note.txt",
        "note.md",
        None,
        &registry,
    )
    .expect_err("the injected delete failure was meant to fail the rename");
    assert!(
        format!("{err:#}").contains("injected"),
        "the rename failed somewhere other than the injected fault: {err:#}"
    );
    assert_eq!(db.get_document_hash("note.txt").unwrap(), None);
    assert_eq!(
        db.get_document_hash("note.md").unwrap().as_deref(),
        Some(HASH_AWAITING_REPARSE),
        "the moved row holds the old parser's chunks under a hash the fast path trusts"
    );
}

/// #552: the old parser's chunks are never left under a hash the fast path trusts. (ADR-0031)
/// A crossed pair's sentinel and its move share one transaction, so when the move fails
/// the rollback takes the sentinel with it: the row stays at `note.txt` with its own hash
/// and chunks, and nothing is at `note.md`. Nothing moved, so no stale row exists, and the
/// next run pairs the rename again, applies it, and re-reads `note.md` under the Markdown
/// parser into the hash of its bytes.
#[test]
fn i552_a_failed_checked_rename_leaves_nothing_and_the_next_run_applies_it() {
    let (dir, db, registry) = i552_crossed_rename("i552-mark-then-fail", I552_BYTES);
    let txt_bodies = stored_bodies(&db, "note.txt");
    db.execute_for_test(concat!(
        "CREATE TABLE fault (armed INTEGER NOT NULL); INSERT INTO fault VALUES (1); ",
        "CREATE TRIGGER inject_rename_failure BEFORE UPDATE OF path ON documents ",
        "WHEN (SELECT armed FROM fault) = 1 ",
        "BEGIN SELECT RAISE(ABORT, 'injected rename failure'); END;"
    ))
    .unwrap();
    let err = i552_rebuild(&db, &mut Embedder::stub_for_tests(2), &dir, &registry)
        .expect_err("the injected rename failure was meant to fail the run");
    assert!(
        format!("{err:#}").contains("injected rename failure"),
        "the run failed somewhere other than the rename: {err:#}"
    );
    assert_eq!(
        db.get_document_hash("note.txt").unwrap(),
        Some(sha256_hex_bytes(I552_BYTES)),
        "the failed rename left the old row's hash changed"
    );
    assert_eq!(
        stored_bodies(&db, "note.txt"),
        txt_bodies,
        "the failed rename changed the old row's chunks"
    );
    assert_eq!(db.get_document_hash("note.md").unwrap(), None);

    db.execute_for_test("UPDATE fault SET armed = 0").unwrap();
    let result = i552_rebuild(&db, &mut Embedder::stub_for_tests(2), &dir, &registry)
        .expect("the second run");
    assert_eq!(
        result.renamed, 1,
        "the second run did not apply the rename: {result:?}"
    );
    assert_eq!(
        db.get_document_hash("note.txt").unwrap(),
        None,
        "the old row is still there"
    );
    assert_eq!(
        db.get_document_hash("note.md").unwrap(),
        Some(sha256_hex_bytes(I552_BYTES)),
        "note.md was not re-read into the hash of its bytes"
    );
    assert_eq!(
        stored_bodies(&db, "note.md"),
        parsed_bodies(&registry, "note.md", I552_BYTES),
        "note.md was not indexed under the Markdown parser"
    );
}

/// #552 (control): the mark is for a pair that crosses a parser only. A full run that
/// pairs `a.md` -> `b.md`, both read by the Markdown parser, moves the row and keeps it on
/// the unchanged fast path: nothing is read again, and the row keeps the hash it had.
///
/// Red if every pair is marked: the moved row is read again (`updated` 1).
#[test]
fn i552_a_full_run_leaves_a_same_parser_rename_on_the_fast_path() {
    let dir = mk_tmp("i552-same-parser");
    let db = reparse_db();
    let registry = Registry::from_enabled(&["md".into(), "txt".into()]).unwrap();
    assert!(!rename_crosses_a_parser(&registry, "a.md", "b.md"));
    std::fs::write(dir.0.join("b.md"), I552_BYTES).unwrap();
    let md_chunks = parse_doc(&registry, "md", "a.md", I552_BYTES).chunks;
    let hash = sha256_hex_bytes(I552_BYTES);
    write_row(&db, "a.md", &hash, &md_chunks);

    let result =
        i552_rebuild(&db, &mut Embedder::stub_for_tests(2), &dir, &registry).expect("the run");
    assert_eq!(result.renamed, 1, "the pair was not detected: {result:?}");
    assert_eq!(
        result.updated, 0,
        "the same-parser rename was read again: {result:?}"
    );
    assert_eq!(db.get_document_hash("a.md").unwrap(), None);
    assert_eq!(db.get_document_hash("b.md").unwrap(), Some(hash));
}

/// #552: makes every write of [`HASH_AWAITING_REPARSE`] into a row's hash fail, as the
/// mark a crossed rename writes before moving the row would fail on a locked or full
/// database.
fn i552_fail_marking_awaiting(db: &Database) {
    db.execute_for_test(&format!(
        concat!(
            "CREATE TRIGGER inject_mark_failure BEFORE UPDATE OF content_hash ON documents ",
            "WHEN NEW.content_hash = '{}' ",
            "BEGIN SELECT RAISE(ABORT, 'injected mark failure'); END;"
        ),
        HASH_AWAITING_REPARSE
    ))
    .unwrap();
}

/// #552: the watcher's [`rename_single_file`] of `note.txt` -> `note.md`, a rename that
/// crosses a parser, cannot write the mark it writes before moving the row. The rename
/// fails and moves nothing: the row stays at `note.txt` under the hash it had, as a rename
/// that failed before the move always left it. Control: a same-parser rename (`a.md` ->
/// `b.md`) under the same fault writes no mark and moves its row on the fast path.
#[test]
fn i552_a_watcher_rename_whose_mark_fails_moves_nothing() {
    let (dir, db, registry) = i552_crossed_rename("i552-mark-fails", I552_BYTES);
    i552_fail_marking_awaiting(&db);
    let err = rename_single_file(
        &db,
        &mut Embedder::stub_for_tests(2),
        &dir.0,
        "note.txt",
        "note.md",
        None,
        &registry,
    )
    .expect_err("the injected mark failure was meant to fail the rename");
    assert!(
        format!("{err:#}").contains("injected mark failure"),
        "the rename failed somewhere other than the mark: {err:#}"
    );
    assert_eq!(
        db.get_document_hash("note.txt").unwrap(),
        Some(sha256_hex_bytes(I552_BYTES)),
        "the row left its old path, or its hash changed, although the mark was not written"
    );
    assert_eq!(db.get_document_hash("note.md").unwrap(), None);

    let same = mk_tmp("i552-mark-fails-same-parser");
    let same_db = reparse_db();
    assert!(!rename_crosses_a_parser(&registry, "a.md", "b.md"));
    std::fs::write(same.0.join("b.md"), I552_BYTES).unwrap();
    let hash = sha256_hex_bytes(I552_BYTES);
    write_row(
        &same_db,
        "a.md",
        &hash,
        &parse_doc(&registry, "md", "a.md", I552_BYTES).chunks,
    );
    i552_fail_marking_awaiting(&same_db);
    let outcome = rename_single_file(
        &same_db,
        &mut Embedder::stub_for_tests(2),
        &same.0,
        "a.md",
        "b.md",
        None,
        &registry,
    )
    .expect("a same-parser rename writes no mark");
    assert_eq!(outcome, RenameOutcome::Renamed);
    assert_eq!(same_db.get_document_hash("a.md").unwrap(), None);
    assert_eq!(same_db.get_document_hash("b.md").unwrap(), Some(hash));
}

/// #552 (final review): the watcher's [`rename_single_file`] of `note.txt` -> `note.md`, a
/// rename that crosses a parser, succeeds. The mark it writes before moving the row must
/// not outlive the reparse: the row ends under the hash of the bytes on disk, with the
/// Markdown parser's chunks, and nothing is left at `note.txt`.
///
/// Red if the reparse's write does not put the bytes' hash over the mark: the row stays
/// under the sentinel and every later event reads it again.
#[test]
fn i552_a_watcher_rename_across_a_parser_that_succeeds_ends_under_the_bytes_hash() {
    let (dir, db, registry) = i552_crossed_rename("i552-w-success", I552_BYTES);
    assert_ne!(
        stored_bodies(&db, "note.txt"),
        parsed_bodies(&registry, "note.md", I552_BYTES),
        "the two parsers were meant to make different chunks of the bytes"
    );
    let outcome = rename_single_file(
        &db,
        &mut Embedder::stub_for_tests(2),
        &dir.0,
        "note.txt",
        "note.md",
        None,
        &registry,
    )
    .expect("the crossed rename with a working embedder");
    assert!(
        matches!(outcome, RenameOutcome::RenamedAndReindexed { .. }),
        "the rename did not reparse the moved row: {outcome:?}"
    );
    assert_eq!(db.get_document_hash("note.txt").unwrap(), None);
    assert_eq!(
        db.get_document_hash("note.md").unwrap(),
        Some(sha256_hex_bytes(I552_BYTES)),
        "the moved row did not end under the hash of the bytes on disk"
    );
    assert_eq!(
        stored_bodies(&db, "note.md"),
        parsed_bodies(&registry, "note.md", I552_BYTES),
        "the row's chunks are not the Markdown parser's"
    );
}

// -----------------------------------------------------------------------
// The prune against a writer that does not take the writer lock (ADR-0031)
// -----------------------------------------------------------------------

/// A file database under `dir` opened twice: the run's connection and a second one that
/// stands for another writer. Both carry the stub embedder's meta.
fn two_connections(dir: &TmpDir) -> (Database, Database) {
    let db_file = dir.0.join("index.db");
    let open = || {
        let db = Database::open(db_file.to_str().unwrap()).unwrap();
        db.verify_embedding_meta("test-model", 2).unwrap();
        db
    };
    let run = open();
    let other = open();
    (run, other)
}

/// [`rebuild_index`] over `kb` with the stub embedder, no schema, and the given registry.
fn prune_run(
    db: &Database,
    kb: &std::path::Path,
    registry: &Registry,
    force: bool,
    progress: progress::ProgressReporter,
) -> IndexResult {
    rebuild_index(
        db,
        &mut Embedder::stub_for_tests(2),
        kb,
        None,
        force,
        None,
        &[],
        registry,
        progress,
        ContextMode::Off,
    )
    .unwrap()
}

/// The `(id, content_hash)` of `rel`'s row, read on the given connection.
fn id_hash(db: &Database, rel: &str) -> Option<(i64, String)> {
    db.all_path_ids_hashes().unwrap().remove(rel)
}

/// The last event of the per-document loop: an `Indexed` or `Unchanged` with `done == total`.
fn is_last_loop_event(event: &progress::ProgressEvent<'_>) -> bool {
    matches!(
        event,
        progress::ProgressEvent::Indexed { done, total, .. }
            | progress::ProgressEvent::Unchanged { done, total, .. }
            if done == total
    )
}

/// Rows another writer changed after the run read its snapshot are kept, each counted in
/// `kept_changed`; the untouched one is pruned; a row written after the snapshot is not a
/// candidate at all.
#[test]
fn t_a1_the_prune_keeps_rows_another_writer_changed_during_the_run() {
    let tmp = mk_tmp("pr3-ta1");
    let kb = tmp.0.join("kb");
    std::fs::create_dir_all(&kb).unwrap();
    write_file(&kb, "x.md", "# x\n\nbody\n");
    let (db, other) = two_connections(&tmp);
    for rel in ["g1.md", "g2.md", "g3.md"] {
        // Not a sha256, so no file on disk pairs with it as a rename.
        write_row(&db, rel, &format!("h-{}", &rel[..2]), &[]);
    }
    let g2_before = id_hash(&db, "g2.md").unwrap();
    let kb_cb = kb.clone();
    let done = std::sync::atomic::AtomicBool::new(false);
    let progress = progress::ProgressReporter::with_callback(Box::new(move |event| {
        if !is_last_loop_event(&event) || done.swap(true, std::sync::atomic::Ordering::SeqCst) {
            return;
        }
        // (a) a new document, on disk and in the index
        write_file(&kb_cb, "new.md", "# new\n");
        other
            .upsert_document("new.md", None, None, None, None, &[], None, "h-new", 1)
            .unwrap();
        // (b) a candidate's hash changed
        other.overwrite_content_hash(&["g1.md"], "changed").unwrap();
        // (c) a candidate deleted and written again under the same hash: a new id
        other.delete_document("g2.md").unwrap();
        other
            .upsert_document("g2.md", None, None, None, None, &[], None, "h-g2", 1)
            .unwrap();
        // (d) g3.md is left alone
    }));
    let registry = Registry::from_enabled(&["md".into()]).unwrap();
    let result = prune_run(&db, &kb, &registry, false, progress);

    let g2_after = id_hash(&db, "g2.md").expect("g2.md was pruned");
    assert_ne!(g2_after.0, g2_before.0, "the re-created g2.md kept its id");
    assert!(id_hash(&db, "new.md").is_some(), "new.md was pruned");
    assert_eq!(
        id_hash(&db, "g1.md").map(|(_, h)| h).as_deref(),
        Some("changed"),
        "g1.md was pruned or rewritten"
    );
    assert!(id_hash(&db, "g3.md").is_none(), "g3.md was kept");
    assert_eq!(result.kept_changed, 2, "{result:?}");
    assert_eq!(result.deleted, 1, "{result:?}");
}

/// The batches are separate transactions: a change made after the first batch committed is
/// seen by the batch that holds the changed row.
#[test]
fn t_a2_a_later_batch_sees_a_change_made_after_the_first_batch_committed() {
    let tmp = mk_tmp("pr3-ta2");
    let kb = tmp.0.join("kb");
    std::fs::create_dir_all(&kb).unwrap();
    write_file(&kb, "x.md", "# x\n\nbody\n");
    let (db, other) = two_connections(&tmp);
    let rels: Vec<String> = (0..=PRUNE_BATCH)
        .map(|i| format!("gone-{i:04}.md"))
        .collect();
    {
        let tx = db.begin_transaction().unwrap();
        for rel in &rels {
            write_row(&db, rel, &format!("h-{rel}"), &[]);
        }
        tx.commit().unwrap();
    }
    let last = rels.last().unwrap().clone();
    let last_cb = last.clone();
    let done = std::sync::atomic::AtomicBool::new(false);
    let progress = progress::ProgressReporter::with_callback(Box::new(move |event| {
        let progress::ProgressEvent::Deleted { rel } = event else {
            return;
        };
        if done.swap(true, std::sync::atomic::Ordering::SeqCst) {
            return;
        }
        // (i) the first batch has committed before its deletes are reported
        assert!(
            other.get_document_hash(rel).unwrap().is_none(),
            "{rel} was reported before its batch committed"
        );
        // (ii) the row the second batch holds changes
        assert_eq!(
            other
                .overwrite_content_hash(&[last_cb.as_str()], "changed")
                .unwrap(),
            1
        );
    }));
    let registry = Registry::from_enabled(&["md".into()]).unwrap();
    let result = prune_run(&db, &kb, &registry, false, progress);

    for rel in &rels[..PRUNE_BATCH] {
        assert!(id_hash(&db, rel).is_none(), "{rel} was kept");
    }
    assert_eq!(
        id_hash(&db, &last).map(|(_, h)| h).as_deref(),
        Some("changed"),
        "the changed row of the second batch was pruned"
    );
    assert_eq!(result.deleted as usize, PRUNE_BATCH, "{result:?}");
    assert_eq!(result.kept_changed, 1, "{result:?}");
}

/// Measures the prune's immediate-transaction hold time over 10 000 candidates. Not a
/// check: prints one line per batch width and asserts only that every candidate went.
/// Runs only when `GROOVE_MEASURE_PRUNE_BATCH` is set and returns at once otherwise, so
/// the nightly `cargo test -- --include-ignored` does not spend minutes on it.
///
/// A batch's hold is read off `on_deleted`, which [`prune_candidates`] calls after the
/// batch commits: batch k runs from the last report of batch k-1 (the call itself for
/// the first) to its own first report.
#[test]
#[ignore = "measurement: prints prune batch hold times; needs GROOVE_MEASURE_PRUNE_BATCH"]
fn measure_prune_batch_hold_times_for_10k_candidates() {
    if std::env::var_os("GROOVE_MEASURE_PRUNE_BATCH").is_none() {
        return;
    }
    const DOCS: usize = 10_000;
    const CHUNKS: i32 = 3;
    for width in [64_usize, 128, 256, 512, 1024] {
        let tmp = mk_tmp(&format!("pr3-measure-{width}"));
        let (db, _other) = two_connections(&tmp);
        {
            let tx = db.begin_transaction().unwrap();
            for i in 0..DOCS {
                let rel = format!("gone-{i:05}.md");
                let id = db
                    .upsert_document(&rel, None, None, None, None, &[], None, &rel, 1)
                    .unwrap();
                for c in 0..CHUNKS {
                    db.insert_chunk(
                        id,
                        c,
                        Some("h"),
                        Some(1),
                        &format!("body {i} {c}"),
                        None,
                        &[0.6_f32, 0.8],
                        1.0,
                    )
                    .unwrap();
                }
            }
            tx.commit().unwrap();
        }
        let snapshot = db.all_path_ids_hashes().unwrap();
        let mut candidates: Vec<String> = snapshot.keys().cloned().collect();
        candidates.sort();

        let mut reports: Vec<std::time::Instant> = Vec::with_capacity(DOCS);
        let start = std::time::Instant::now();
        let done = prune_candidates(&db, &snapshot, &candidates, width, false, &mut |_| {
            reports.push(std::time::Instant::now());
        })
        .unwrap();
        assert_eq!(done.deleted.len(), DOCS);
        assert_eq!(reports.len(), DOCS);

        // Every candidate is deleted, so each batch reports exactly `width` paths (the
        // last fewer): a batch starts at every `width`-th report.
        let mut holds: Vec<f64> = (0..DOCS)
            .step_by(width)
            .map(|first| {
                let from = if first == 0 {
                    start
                } else {
                    reports[first - 1]
                };
                (reports[first] - from).as_secs_f64() * 1000.0
            })
            .collect();
        let total_ms = (*reports.last().unwrap() - start).as_secs_f64() * 1000.0;
        holds.sort_by(f64::total_cmp);
        println!(
            "PRUNE-BATCH width={width} batches={} max_ms={:.1} p50_ms={:.1} total_ms={total_ms:.1}",
            holds.len(),
            holds.last().unwrap(),
            holds[holds.len() / 2],
        );
        assert!(db.all_document_paths().unwrap().is_empty());
    }
}

/// A forced run reads its snapshot too: a row written after it is not a candidate.
#[test]
fn a_forced_run_does_not_prune_rows_written_after_its_snapshot() {
    let tmp = mk_tmp("pr3-force-late");
    let kb = tmp.0.join("kb");
    std::fs::create_dir_all(&kb).unwrap();
    write_file(&kb, "x.md", "# x\n\nbody\n");
    let (db, other) = two_connections(&tmp);
    let done = std::sync::atomic::AtomicBool::new(false);
    let progress = progress::ProgressReporter::with_callback(Box::new(move |event| {
        if !is_last_loop_event(&event) || done.swap(true, std::sync::atomic::Ordering::SeqCst) {
            return;
        }
        other
            .upsert_document("late.md", None, None, None, None, &[], None, "h-late", 1)
            .unwrap();
    }));
    let registry = Registry::from_enabled(&["md".into()]).unwrap();
    let result = prune_run(&db, &kb, &registry, true, progress);

    assert!(id_hash(&db, "late.md").is_some(), "late.md was pruned");
    assert_eq!(result.deleted, 0, "{result:?}");
    assert_eq!(result.kept_changed, 0, "{result:?}");
}

/// A run with no other writer keeps nothing it should prune and prunes only what is gone:
/// the renames (one within a parser, one across parsers) and an oversized file's row are
/// not counted as kept, and the row of a file removed from disk goes.
#[test]
fn a_run_alone_keeps_nothing_and_prunes_what_is_gone() {
    use crate::parser::{CodeParsersConfig, FileSizeLimit, FileSizeLimits};
    let tmp = mk_tmp("pr3-alone");
    let kb = tmp.0.join("kb");
    std::fs::create_dir_all(&kb).unwrap();
    let (db, _other) = two_connections(&tmp);
    let limits = FileSizeLimits {
        text: FileSizeLimit::Bytes(32),
        ..FileSizeLimits::default()
    };
    let ids = ["md".to_string(), "txt".to_string()];
    let registry =
        Registry::from_enabled_with_plugins(&ids, &CodeParsersConfig::default(), None, limits)
            .expect("md + txt");

    // a.md -> b.md, the same bytes and the same parser
    let ab: &[u8] = b"# ab\n\nsame body\n";
    std::fs::write(kb.join("b.md"), ab).unwrap();
    let ab_chunks = parse_doc(&registry, "md", "a.md", ab).chunks;
    write_row(&db, "a.md", &sha256_hex_bytes(ab), &ab_chunks);
    // c.txt -> c.md, the same bytes across parsers
    let c: &[u8] = b"# c\n\nmoved body\n";
    std::fs::write(kb.join("c.md"), c).unwrap();
    let c_chunks = parse_doc(&registry, "txt", "c.txt", c).chunks;
    write_row(&db, "c.txt", &sha256_hex_bytes(c), &c_chunks);
    // gone.md: a row whose file is not on disk
    write_row(&db, "gone.md", "h-gone", &[]);
    // big.md: on disk past the text cap, with a row
    std::fs::write(kb.join("big.md"), [b'x'; 64]).unwrap();
    write_row(&db, "big.md", "h-big", &[]);

    let result = prune_run(
        &db,
        &kb,
        &registry,
        false,
        progress::ProgressReporter::new(progress::ProgressMode::Quiet),
    );

    assert_eq!(result.kept_changed, 0, "{result:?}");
    assert_eq!(result.deleted, 1, "{result:?}");
    assert!(id_hash(&db, "gone.md").is_none(), "gone.md was kept");
    assert!(id_hash(&db, "a.md").is_none(), "a.md is still there");
    assert!(id_hash(&db, "c.txt").is_none(), "c.txt is still there");
    assert!(id_hash(&db, "b.md").is_some(), "b.md is missing");
    assert!(id_hash(&db, "c.md").is_some(), "c.md is missing");
    assert!(id_hash(&db, "big.md").is_some(), "big.md was pruned");
}

#[test]
fn kept_changed_note_is_ascii_and_absent_for_zero() {
    assert_eq!(kept_changed_note(0), None);
    let note = kept_changed_note(3).unwrap();
    assert_eq!(
        note,
        concat!(
            "note: kept 3 row(s) another writer changed during this run; ",
            "the next run checks them again."
        )
    );
    assert!(note.is_ascii());
}

/// A row the prune kept for another writer is one this run did not read: it gets
/// [`HASH_AWAITING_REPARSE`] in the transaction that records the `.docx` keys, so the next
/// run's fast path cannot keep the old parser's chunks under keys that now read current.
#[test]
fn a_kept_docx_row_is_marked_for_a_reparse_when_the_docx_keys_are_recorded() {
    let tmp = mk_tmp("pr3-kept-docx");
    let kb = tmp.0.join("kb");
    std::fs::create_dir_all(&kb).unwrap();
    write_file(&kb, "x.md", "# x\n\nbody\n");
    let (db, other) = two_connections(&tmp);
    // Not a sha256, so no file on disk pairs with it as a rename.
    write_row(&db, "gone.docx", "h-gone", &[]);
    assert_eq!(db.read_docx_heading_policy().unwrap(), None);
    let progress = on_first_scanning(move || {
        other
            .overwrite_content_hash(&["gone.docx"], "changed")
            .unwrap();
    });
    let result = prune_run(&db, &kb, &docx_registry(), false, progress);

    assert_eq!(result.kept_changed, 1, "{result:?}");
    assert_eq!(
        id_hash(&db, "gone.docx").map(|(_, h)| h).as_deref(),
        Some(HASH_AWAITING_REPARSE),
        "the kept .docx row was not marked for a re-parse"
    );
    assert_eq!(
        db.read_docx_heading_policy().unwrap().as_deref(),
        Some(DOCX_HEADING_POLICY)
    );
    assert_eq!(
        db.read_docx_table_rows_policy().unwrap().as_deref(),
        Some(DOCX_TABLE_ROWS_POLICY)
    );
}

/// A forced run that keeps a row for another writer marks it for a re-parse, and records
/// neither the declared-field generation nor the frontmatter policy: the kept row is not
/// one this build parsed. The row reaches the forced run's snapshot through a trigger that
/// writes it when the run opens its declared-field pass, after the reset and before the
/// snapshot; the other connection drops the trigger and changes the row at the scan.
#[test]
fn a_forced_run_that_keeps_a_row_marks_it_and_leaves_the_run_wide_records_pending() {
    let tmp = mk_tmp("pr3-kept-force");
    let kb = tmp.0.join("kb");
    std::fs::create_dir_all(&kb).unwrap();
    write_file(&kb, "x.md", "# x\n\nbody\n");
    let (db, other) = two_connections(&tmp);
    db.execute_for_test(concat!(
        "CREATE TRIGGER write_ghost_after_reset AFTER INSERT ON index_meta ",
        "WHEN NEW.key = 'declared_fields_pass' ",
        "BEGIN INSERT INTO documents (path, tags, content_hash, last_indexed) ",
        "VALUES ('ghost.md', '[]', 'h-ghost', '2026-01-01T00:00:00Z'); END;"
    ))
    .unwrap();
    let progress = on_first_scanning(move || {
        other
            .execute_for_test("DROP TRIGGER write_ghost_after_reset;")
            .unwrap();
        assert_eq!(
            other
                .overwrite_content_hash(&["ghost.md"], "changed")
                .unwrap(),
            1,
            "the trigger did not write ghost.md before the snapshot"
        );
    });
    let registry = Registry::from_enabled(&["md".into()]).unwrap();
    let result = prune_run(&db, &kb, &registry, true, progress);

    assert_eq!(result.kept_changed, 1, "{result:?}");
    assert_eq!(
        id_hash(&db, "ghost.md").map(|(_, h)| h).as_deref(),
        Some(HASH_AWAITING_REPARSE),
        "the kept row was not marked for a re-parse"
    );
    assert_eq!(db.read_declared_fields().unwrap(), None);
    assert_eq!(db.read_frontmatter_policy().unwrap(), None);
}

/// A forced run marks each row its prune keeps in the transaction of the prune batch that
/// keeps it, not only at the end: a step after the prune that fails still leaves it marked,
/// so the code chunk policy the forced run recorded before its walk does not stand over a
/// row it never read. The row
/// reaches the snapshot as in the test above; at the scan the other connection also makes
/// the end of the declared-field pass fail.
#[test]
fn a_forced_run_that_fails_after_its_prune_has_already_marked_the_kept_rows() {
    let tmp = mk_tmp("pr3-kept-force-fail");
    let kb = tmp.0.join("kb");
    std::fs::create_dir_all(&kb).unwrap();
    write_file(&kb, "x.md", "# x\n\nbody\n");
    let (db, other) = two_connections(&tmp);
    db.execute_for_test(concat!(
        "CREATE TRIGGER write_ghost_after_reset AFTER INSERT ON index_meta ",
        "WHEN NEW.key = 'declared_fields_pass' ",
        "BEGIN INSERT INTO documents (path, tags, content_hash, last_indexed) ",
        "VALUES ('ghost.md', '[]', 'h-ghost', '2026-01-01T00:00:00Z'); END;"
    ))
    .unwrap();
    let progress = on_first_scanning(move || {
        other
            .execute_for_test("DROP TRIGGER write_ghost_after_reset;")
            .unwrap();
        other
            .execute_for_test(concat!(
                "CREATE TRIGGER inject_pass_end_failure BEFORE DELETE ON index_meta ",
                "WHEN OLD.key = 'declared_fields_pass' ",
                "BEGIN SELECT RAISE(ABORT, 'injected pass end failure'); END;"
            ))
            .unwrap();
        assert_eq!(
            other
                .overwrite_content_hash(&["ghost.md"], "changed")
                .unwrap(),
            1,
            "the trigger did not write ghost.md before the snapshot"
        );
    });
    let registry = Registry::from_enabled(&["md".into()]).unwrap();
    let err = rebuild_index(
        &db,
        &mut Embedder::stub_for_tests(2),
        &kb,
        None,
        true,
        None,
        &[],
        &registry,
        progress,
        ContextMode::Off,
    )
    .expect_err("the injected failure did not stop the run");
    assert!(
        format!("{err:#}").contains("injected pass end failure"),
        "{err:#}"
    );
    assert_eq!(
        id_hash(&db, "ghost.md").map(|(_, h)| h).as_deref(),
        Some(HASH_AWAITING_REPARSE),
        "the kept row was left unmarked by a forced run that failed after its prune"
    );
}

/// A forced run whose prune keeps a row in one batch and fails in a later one still leaves
/// that row marked for a re-parse: the batch that keeps it marks it in its own transaction,
/// so the `Err` of a later batch cannot drop the mark with the kept paths. The rows reach the
/// forced run's snapshot through a trigger, as in the tests above; at the scan the other
/// connection changes a row of the first batch and makes the delete of the second batch's
/// only row fail.
#[test]
fn a_forced_run_whose_later_prune_batch_fails_leaves_an_earlier_batchs_kept_row_marked() {
    let tmp = mk_tmp("pr3-kept-force-batch-fail");
    let kb = tmp.0.join("kb");
    std::fs::create_dir_all(&kb).unwrap();
    write_file(&kb, "x.md", "# x\n\nbody\n");
    let (db, other) = two_connections(&tmp);
    let rels: Vec<String> = (0..=PRUNE_BATCH)
        .map(|i| format!("gone-{i:04}.md"))
        .collect();
    let inserts: String = rels
        .iter()
        .map(|rel| {
            format!(
                concat!(
                    "INSERT INTO documents (path, tags, content_hash, last_indexed) ",
                    "VALUES ('{rel}', '[]', 'h-{rel}', '2026-01-01T00:00:00Z'); "
                ),
                rel = rel
            )
        })
        .collect();
    db.execute_for_test(&format!(
        concat!(
            "CREATE TRIGGER write_rows_after_reset AFTER INSERT ON index_meta ",
            "WHEN NEW.key = 'declared_fields_pass' BEGIN {}END;"
        ),
        inserts
    ))
    .unwrap();
    let first = rels[0].clone();
    let last = rels.last().unwrap().clone();
    let (first_cb, last_cb) = (first.clone(), last.clone());
    let progress = on_first_scanning(move || {
        other
            .execute_for_test("DROP TRIGGER write_rows_after_reset;")
            .unwrap();
        other
            .execute_for_test(&format!(
                concat!(
                    "CREATE TRIGGER inject_later_batch_failure BEFORE DELETE ON documents ",
                    "WHEN OLD.path = '{}' ",
                    "BEGIN SELECT RAISE(ABORT, 'injected later batch failure'); END;"
                ),
                last_cb
            ))
            .unwrap();
        assert_eq!(
            other
                .overwrite_content_hash(&[first_cb.as_str()], "changed")
                .unwrap(),
            1,
            "the trigger did not write the rows before the snapshot"
        );
    });
    let registry = Registry::from_enabled(&["md".into()]).unwrap();
    let err = rebuild_index(
        &db,
        &mut Embedder::stub_for_tests(2),
        &kb,
        None,
        true,
        None,
        &[],
        &registry,
        progress,
        ContextMode::Off,
    )
    .expect_err("the injected failure did not stop the run");
    assert!(
        format!("{err:#}").contains("injected later batch failure"),
        "{err:#}"
    );
    assert_eq!(
        id_hash(&db, &first).map(|(_, h)| h).as_deref(),
        Some(HASH_AWAITING_REPARSE),
        "the row the first batch kept was left unmarked when a later batch failed"
    );
    for rel in &rels[1..PRUNE_BATCH] {
        assert!(id_hash(&db, rel).is_none(), "{rel} was kept");
    }
    assert!(
        id_hash(&db, &last).is_some(),
        "the failed batch's row was deleted"
    );
}

/// A run that refreshes the frontmatter of unchanged Markdown and keeps a row for another
/// writer records neither the frontmatter policy nor the declared-field set: the kept row was
/// not read under them. The next run makes the pass again.
#[test]
fn a_frontmatter_refresh_that_keeps_a_row_leaves_the_policy_unrecorded() {
    let tmp = mk_tmp("pr3-kept-fm");
    let kb = tmp.0.join("kb");
    std::fs::create_dir_all(&kb).unwrap();
    write_file(&kb, "x.md", "# x\n\nbody\n");
    let (db, other) = two_connections(&tmp);
    write_row(&db, "gone.md", "h-gone", &[]);
    assert_eq!(db.read_frontmatter_policy().unwrap(), None);
    let progress = on_first_scanning(move || {
        other
            .overwrite_content_hash(&["gone.md"], "changed")
            .unwrap();
    });
    let registry = Registry::from_enabled(&["md".into()]).unwrap();
    let result = prune_run(&db, &kb, &registry, false, progress);

    assert_eq!(result.kept_changed, 1, "{result:?}");
    assert_eq!(db.read_frontmatter_policy().unwrap(), None);
    assert_eq!(db.read_declared_fields().unwrap(), None);
}

// -----------------------------------------------------------------------
// Renames against a writer that does not take the writer lock (ADR-0031)
// -----------------------------------------------------------------------

/// A Markdown file's bytes, the same for the old and the new path of a detected rename.
const RENAME_BYTES: &[u8] = b"# moved\n\nthe same body\n";

/// A kb holding `new_rel` with [`RENAME_BYTES`] and a database whose only row is
/// `old_rel` under those bytes' hash, with the given registry's chunks for `old_rel`.
fn rename_fixture(
    tmp: &TmpDir,
    registry: &Registry,
    old_ext: &str,
    old_rel: &str,
    new_rel: &str,
) -> (std::path::PathBuf, Database, Database) {
    let kb = tmp.0.join("kb");
    std::fs::create_dir_all(&kb).unwrap();
    std::fs::write(kb.join(new_rel), RENAME_BYTES).unwrap();
    let (db, other) = two_connections(tmp);
    let chunks = parse_doc(registry, old_ext, old_rel, RENAME_BYTES).chunks;
    write_row(&db, old_rel, &sha256_hex_bytes(RENAME_BYTES), &chunks);
    (kb, db, other)
}

/// A reporter that runs `act` once, at the scan's first `Scanning` event: after the run
/// read its snapshot and before it applies its renames.
fn on_first_scanning(act: impl FnOnce() + Send + 'static) -> progress::ProgressReporter {
    let act = std::sync::Mutex::new(Some(act));
    progress::ProgressReporter::with_callback(Box::new(move |event| {
        if !matches!(event, progress::ProgressEvent::Scanning { .. }) {
            return;
        }
        if let Some(act) = act.lock().unwrap().take() {
            act();
        }
    }))
}

/// Another writer applied the same rename first: the run's pair is skipped, the run goes
/// on, the moved row stays, and the old path has no row left to prune or keep.
#[test]
fn t_a3_a_rename_another_writer_applied_first_is_skipped_and_the_run_goes_on() {
    let tmp = mk_tmp("pr3-ta3a");
    let registry = Registry::from_enabled(&["md".into()]).unwrap();
    let (kb, db, other) = rename_fixture(&tmp, &registry, "md", "old.md", "new.md");
    let progress = on_first_scanning(move || {
        other
            .rename_document("old.md", "new.md", FrontmatterTopic::Unread)
            .unwrap();
    });
    let result = prune_run(&db, &kb, &registry, false, progress);

    assert_eq!(result.renamed, 0, "{result:?}");
    assert_eq!(result.renames_skipped, 1, "{result:?}");
    assert_eq!(
        id_hash(&db, "new.md").map(|(_, h)| h),
        Some(sha256_hex_bytes(RENAME_BYTES)),
        "the row the other writer moved is gone or rewritten"
    );
    assert!(id_hash(&db, "old.md").is_none(), "old.md has a row");
    assert_eq!(result.kept_changed, 0, "{result:?}");
}

/// Another writer wrote the new path first: the pair is skipped, the loop indexes the new
/// path as an ordinary file over that row, and the prune deletes the old row.
#[test]
fn t_a3_a_rename_onto_a_path_another_writer_wrote_is_skipped() {
    let tmp = mk_tmp("pr3-ta3b");
    let registry = Registry::from_enabled(&["md".into()]).unwrap();
    let (kb, db, other) = rename_fixture(&tmp, &registry, "md", "old.md", "new.md");
    let progress = on_first_scanning(move || {
        other
            .upsert_document("new.md", None, None, None, None, &[], None, "h-other", 1)
            .unwrap();
    });
    let result = prune_run(&db, &kb, &registry, false, progress);

    assert_eq!(result.renamed, 0, "{result:?}");
    assert_eq!(result.renames_skipped, 1, "{result:?}");
    assert_eq!(
        id_hash(&db, "new.md").map(|(_, h)| h),
        Some(sha256_hex_bytes(RENAME_BYTES)),
        "the loop did not index new.md over the other writer's row"
    );
    assert!(id_hash(&db, "old.md").is_none(), "old.md was not pruned");
    assert_eq!(result.deleted, 1, "{result:?}");
}

/// Another writer rewrote the old row in place: the pair is skipped, the loop indexes the new
/// path as an ordinary file, and the prune keeps the old row the other writer changed. The
/// one shape in which both counters move.
#[test]
fn t_a3_a_rename_whose_old_row_another_writer_rewrote_is_skipped_and_kept() {
    let tmp = mk_tmp("pr3-ta3c");
    let registry = Registry::from_enabled(&["md".into()]).unwrap();
    let (kb, db, other) = rename_fixture(&tmp, &registry, "md", "old.md", "new.md");
    let progress = on_first_scanning(move || {
        other
            .overwrite_content_hash(&["old.md"], "changed")
            .unwrap();
    });
    let result = prune_run(&db, &kb, &registry, false, progress);

    assert_eq!(result.renamed, 0, "{result:?}");
    assert_eq!(result.renames_skipped, 1, "{result:?}");
    assert_eq!(result.kept_changed, 1, "{result:?}");
    assert_eq!(result.deleted, 0, "{result:?}");
    assert_eq!(
        id_hash(&db, "old.md").map(|(_, h)| h).as_deref(),
        Some("changed"),
        "the prune did not keep the old row the other writer rewrote"
    );
    assert_eq!(
        id_hash(&db, "new.md").map(|(_, h)| h),
        Some(sha256_hex_bytes(RENAME_BYTES)),
        "the loop did not index new.md as an ordinary file"
    );
}

/// A skipped pair that crosses a parser is not settled as a crossed rename: the row the
/// other writer put at the new path stays, and the old row is pruned.
#[test]
fn a_skipped_crossed_rename_keeps_the_other_writers_row() {
    let tmp = mk_tmp("pr3-skip-crossed");
    let registry = Registry::from_enabled(&["md".into(), "txt".into()]).unwrap();
    let (kb, db, other) = rename_fixture(&tmp, &registry, "txt", "note.txt", "note.md");
    let progress = on_first_scanning(move || {
        other
            .upsert_document("note.md", None, None, None, None, &[], None, "h-other", 1)
            .unwrap();
    });
    let result = prune_run(&db, &kb, &registry, false, progress);

    assert_eq!(result.renames_skipped, 1, "{result:?}");
    assert!(
        id_hash(&db, "note.md").is_some(),
        "the other writer's note.md row was removed"
    );
    assert_eq!(
        id_hash(&db, "note.md").map(|(_, h)| h),
        Some(sha256_hex_bytes(RENAME_BYTES)),
        "the loop did not index note.md over the other writer's row"
    );
    assert!(
        id_hash(&db, "note.txt").is_none(),
        "note.txt was not pruned"
    );
}

/// (ADR-0031) What a writer that does not take the lock is *not* protected from, pinned so the range does not
/// widen silently: the per-document write of the loop. The public behaviour page says: "against such a writer,
/// the prune step still keeps rows the other writer changed during the run and rename detection skips the pairs
/// it changed, but the other look-then-write steps are not protected."
/// Another case it does not close: such a writer applies a rename into another parser first without
/// marking the row ([`HASH_AWAITING_REPARSE`]); this run's pair is then skipped, the new path is `Unchanged`,
/// and the previous parser's chunks stay until `--force`.
#[test]
fn t_a4_the_loop_writes_back_a_row_another_writer_deleted_mid_run() {
    let tmp = mk_tmp("pr3-ta4");
    let kb = tmp.0.join("kb");
    std::fs::create_dir_all(&kb).unwrap();
    // a.md sorts before b.md, so the loop reaches a.md first.
    write_file(&kb, "a.md", "# a\n\nfirst body\n");
    write_file(&kb, "b.md", "# b\n\nsecond body\n");
    let (db, other) = two_connections(&tmp);
    let registry = Registry::from_enabled(&["md".into()]).unwrap();
    // Both indexed under their bytes' hash, so the second run finds both unchanged.
    let first = prune_run(
        &db,
        &kb,
        &registry,
        false,
        progress::ProgressReporter::new(progress::ProgressMode::Quiet),
    );
    assert_eq!(first.updated, 2, "{first:?}");
    let other = std::sync::Mutex::new(Some(other));
    let progress = progress::ProgressReporter::with_callback(Box::new(move |event| {
        if !matches!(
            event,
            progress::ProgressEvent::Unchanged { rel: "a.md", .. }
        ) {
            return;
        }
        if let Some(other) = other.lock().unwrap().take() {
            other.delete_document("b.md").unwrap();
        }
    }));
    let result = prune_run(&db, &kb, &registry, false, progress);

    assert!(
        id_hash(&db, "b.md").is_some(),
        "the loop did not write b.md back from disk"
    );
    assert_eq!(result.updated, 1, "{result:?}");
}

/// A run stopped after its renames still reports the ones it skipped.
#[test]
fn a_cancelled_run_still_counts_its_skipped_renames() {
    let tmp = mk_tmp("pr3-skip-cancel");
    let registry = Registry::from_enabled(&["md".into()]).unwrap();
    let (kb, db, other) = rename_fixture(&tmp, &registry, "md", "old.md", "new.md");
    let token = progress::CancelToken::new();
    let token_cb = token.clone();
    let other = std::sync::Mutex::new(Some(other));
    let progress = progress::ProgressReporter::with_callback(Box::new(move |event| match event {
        progress::ProgressEvent::Scanning { .. } => {
            if let Some(other) = other.lock().unwrap().take() {
                other
                    .rename_document("old.md", "new.md", FrontmatterTopic::Unread)
                    .unwrap();
            }
        }
        progress::ProgressEvent::Indexed { .. } | progress::ProgressEvent::Unchanged { .. } => {
            token_cb.cancel()
        }
        _ => {}
    }))
    .with_cancel(token);
    let result = prune_run(&db, &kb, &registry, false, progress);

    assert!(result.cancelled, "{result:?}");
    assert_eq!(result.renames_skipped, 1, "{result:?}");
    assert_eq!(result.deleted, 0, "{result:?}");
    assert_eq!(result.kept_changed, 0, "{result:?}");
}

#[test]
fn renames_skipped_note_is_ascii_and_absent_for_zero() {
    assert_eq!(renames_skipped_note(0), None);
    let note = renames_skipped_note(2).unwrap();
    assert_eq!(
        note,
        concat!(
            "note: skipped 2 rename(s) another writer changed during this run; ",
            "the new paths are indexed as ordinary files."
        )
    );
    assert!(note.is_ascii());
}

/// #660: the [`crate::parser::markdown`], [`crate::parser::txt`] and [`crate::parser::pdf`]
/// parsers enabled, with the text cap lowered to 16 bytes so a small
/// `.txt` is over it. Every other text fixture of these tests stays at 16 bytes or under.
fn i660_explain_registry() -> Registry {
    use crate::parser::{CodeParsersConfig, FileSizeLimit, FileSizeLimits};
    let limits = FileSizeLimits {
        text: FileSizeLimit::Bytes(16),
        ..FileSizeLimits::default()
    };
    let ids = ["md".to_string(), "txt".to_string(), "pdf".to_string()];
    Registry::from_enabled_with_plugins(&ids, &CodeParsersConfig::default(), None, limits)
        .expect("md + txt + pdf")
}

/// #660 T-L11: the watcher's single-file entry says why each kind of file was not indexed.
#[test]
fn explained_gives_the_reason_for_each_kind() {
    let dir = mk_tmp("i660-explained-kinds");
    let db = reparse_db();
    let registry = i660_explain_registry();
    std::fs::write(dir.0.join("big.txt"), [b'x'; 64]).unwrap();
    std::fs::copy(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/binary/encrypted.pdf"),
        dir.0.join("encrypted.pdf"),
    )
    .unwrap();
    std::fs::write(dir.0.join("empty.md"), b"").unwrap();
    std::fs::write(dir.0.join("sjis.txt"), b"\x82\xa0\x82\xa2").unwrap();

    let explain = |rel: &str| {
        reindex_single_file_explained(
            &db,
            &mut Embedder::stub_for_tests(2),
            &dir.0,
            rel,
            None,
            &registry,
        )
        .unwrap()
    };
    let big = explain("big.txt");
    assert_eq!(
        big.reason,
        Some(SkipReason::TooLarge {
            size: 64,
            limit: 16,
            key: LimitKey::MaxTextFileSize,
        }),
        "{big:?}"
    );
    let encrypted = explain("encrypted.pdf");
    assert_eq!(
        encrypted.reason,
        Some(SkipReason::Encrypted),
        "{encrypted:?}"
    );
    let empty = explain("empty.md");
    assert_eq!(empty.reason, Some(SkipReason::NoText), "{empty:?}");
    let sjis = explain("sjis.txt");
    assert_eq!(sjis.reason, Some(SkipReason::NotUtf8), "{sjis:?}");
}

/// #660 T-L11 (R10): a symlink in the KB is left out on purpose, as the walk leaves it out,
/// so the watcher gives it no reason.
#[cfg(unix)]
#[test]
fn a_symlink_has_no_reason_at_the_watcher() {
    let dir = mk_tmp("i660-explained-symlink");
    let db = reparse_db();
    let registry = i660_explain_registry();
    std::fs::write(dir.0.join("target.md"), b"# A\n\nhi\n").unwrap();
    std::os::unix::fs::symlink(dir.0.join("target.md"), dir.0.join("link.md")).unwrap();
    let got = reindex_single_file_explained(
        &db,
        &mut Embedder::stub_for_tests(2),
        &dir.0,
        "link.md",
        None,
        &registry,
    )
    .unwrap();
    assert_eq!(got.reason, None, "{got:?}");
    assert!(
        matches!(
            got.result,
            SingleResult::Refused | SingleResult::OutsideKnowledgeBaseDeindexed
        ),
        "{got:?}"
    );
}

/// #660 T-L12 (I4): [`Reindexed::reason`] is `Some` exactly for the results the rebuild loop counts as
/// skipped, except a path through a link (R10).
#[test]
fn reason_is_some_exactly_when_the_loop_counts_it() {
    let dir = mk_tmp("i660-reason-iff-counted");
    let db = reparse_db();
    let registry = i660_explain_registry();
    std::fs::write(dir.0.join("plain.md"), b"# A\n\nhi\n").unwrap();
    std::fs::write(dir.0.join("empty.md"), b"").unwrap();
    std::fs::write(dir.0.join("big.txt"), [b'x'; 64]).unwrap();
    std::fs::write(dir.0.join("sjis.txt"), b"\x82\xa0\x82\xa2").unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(dir.0.join("plain.md"), dir.0.join("link.md")).unwrap();

    type Case = (&'static str, fn(&SingleResult) -> bool, Option<SkipReason>);
    let base: Vec<Case> = vec![
        (
            "plain.md",
            |r| matches!(r, SingleResult::Updated { .. }),
            None,
        ),
        (
            "empty.md",
            |r| matches!(r, SingleResult::NothingToIndex { .. }),
            Some(SkipReason::NoText),
        ),
        (
            "big.txt",
            |r| matches!(r, SingleResult::Skipped { .. }),
            Some(SkipReason::TooLarge {
                size: 64,
                limit: 16,
                key: LimitKey::MaxTextFileSize,
            }),
        ),
        (
            "sjis.txt",
            |r| matches!(r, SingleResult::Skipped { .. }),
            Some(SkipReason::NotUtf8),
        ),
    ];
    // (R10) Only where a symlink could be made above.
    let link: Case = ("link.md", |r| matches!(r, SingleResult::Refused), None);
    let cases = base.into_iter().chain(cfg!(unix).then_some(link));
    for (rel, variant, reason) in cases {
        let got = reindex_single_file_explained(
            &db,
            &mut Embedder::stub_for_tests(2),
            &dir.0,
            rel,
            None,
            &registry,
        )
        .unwrap();
        assert!(variant(&got.result), "{rel}: {got:?}");
        assert_eq!(got.reason, reason, "{rel}: {got:?}");
        assert_eq!(
            got.reason.is_some(),
            !matches!(
                got.result,
                SingleResult::Updated { .. }
                    | SingleResult::Unchanged
                    | SingleResult::MetadataRefreshed { .. }
            ) && rel != "link.md",
            "{rel}: {got:?}"
        );
    }
}

/// #660 T-L12b (R4): the reparse's table-rows write that found the row changed under it is a
/// changed-during-read skip with that reason; an undisturbed write has none.
#[test]
fn a_reparsed_table_rows_change_is_changed_during_read() {
    use crate::parser::docx::fixture::table_docx;
    let dir = mk_tmp("i660-reparse-table-rows");
    let (db, path) = file_db(&dir.0);
    let registry = docx_registry();
    let chunks = parse_doc(&registry, "docx", "tariff.docx", &table_docx()).chunks;
    let cases: [(&str, Option<&str>, Option<SkipReason>); 2] = [
        ("steady.docx", None, None),
        (
            "rewritten.docx",
            Some(
                "UPDATE chunks SET content = 'another writer was here' WHERE document_id = (SELECT id FROM documents WHERE path = ?1)",
            ),
            Some(SkipReason::ChangedDuringRead),
        ),
    ];
    for (rel, interruption, expected) in cases {
        write_row(&db, rel, "hash-of-these-bytes", &chunks);
        let decision = decide_table_rows(&db, rel, &chunks)
            .unwrap()
            .expect("a docx row");
        if let Some(sql) = interruption {
            rusqlite::Connection::open(&path)
                .unwrap()
                .execute(sql, [rel])
                .unwrap();
        }
        let result =
            write_reparsed_table_rows(&db, rel, &decision, &chunks, ContextMode::Off, false)
                .unwrap();
        let explained = explain_reparsed_table_rows(result);
        assert_eq!(explained.reason, expected, "{rel}: {explained:?}");
    }
}

/// #660 (spec 9, PR-2): the list invariants every [`rebuild_index`] result keeps, completed
/// or cancelled -- I1 (one entry per skipped file), I2 / I6 (paths strictly ascending, so no
/// path twice) and I7 ([`IndexResult::unspellable`] is part of [`IndexResult::skipped`]).
/// [`IndexResult::embed_rejected`] is per file and not checked (spec 6 I7).
fn assert_skip_ledger(r: &IndexResult) {
    assert_eq!(
        r.skipped_files.len(),
        r.skipped as usize,
        "I1: one entry per skipped file: {r:?}"
    );
    assert!(
        r.skipped_files.windows(2).all(|w| w[0].path < w[1].path),
        "I2 / I6: paths ascending and unique: {r:?}"
    );
    assert!(
        r.unspellable <= r.skipped,
        "I7: unspellable is part of skipped: {r:?}"
    );
}

/// #660: the [`SkippedFile::path`] and [`SkippedFile::reason`] of each entry of a result's
/// list, in its order.
fn listed(r: &IndexResult) -> Vec<(String, SkipReason)> {
    r.skipped_files
        .iter()
        .map(|f| (f.path.clone(), f.reason))
        .collect()
}

/// #660: a full run of the given knowledge base with the given [`Registry`] and the stub
/// embedder, reporting to the given reporter.
fn i660_rebuild(
    db: &Database,
    kb: &Path,
    registry: &Registry,
    progress: progress::ProgressReporter,
) -> IndexResult {
    rebuild_index(
        db,
        &mut Embedder::stub_for_tests(2),
        kb,
        None,
        false,
        None,
        &[],
        registry,
        progress,
        ContextMode::Off,
    )
    .unwrap()
}

/// #660 T-L11 (the rebuild side): the full index lists the files of
/// [`explained_gives_the_reason_for_each_kind`] with the reason the watcher's single-file
/// entry gives each of them (A7).
#[test]
fn rebuild_and_watcher_agree() {
    let dir = mk_tmp("i660-rebuild-watcher-agree");
    let db = reparse_db();
    let registry = i660_explain_registry();
    std::fs::write(dir.0.join("big.txt"), [b'x'; 64]).unwrap();
    std::fs::copy(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/binary/encrypted.pdf"),
        dir.0.join("encrypted.pdf"),
    )
    .unwrap();
    std::fs::write(dir.0.join("empty.md"), b"").unwrap();
    std::fs::write(dir.0.join("sjis.txt"), b"\x82\xa0\x82\xa2").unwrap();

    let result = i660_rebuild(
        &db,
        &dir.0,
        &registry,
        progress::ProgressReporter::new(progress::ProgressMode::Quiet),
    );
    let paths: Vec<&str> = result
        .skipped_files
        .iter()
        .map(|f| f.path.as_str())
        .collect();
    assert_eq!(
        paths,
        vec!["big.txt", "empty.md", "encrypted.pdf", "sjis.txt"],
        "{result:?}"
    );
    for file in &result.skipped_files {
        let watcher = reindex_single_file_explained(
            &db,
            &mut Embedder::stub_for_tests(2),
            &dir.0,
            &file.path,
            None,
            &registry,
        )
        .unwrap();
        assert_eq!(
            watcher.reason,
            Some(file.reason),
            "{}: the rebuild and the watcher disagree: {watcher:?}",
            file.path
        );
    }
    assert_skip_ledger(&result);
}

/// #660 T-L6b (I3, A8): a run whose scan declines a file over the size cap and whose walk
/// leaves out both names of a hard link. The scan's skip is listed and guarded from the
/// prune (the two `debug_assert!`s of I3 run in this debug build); the walk's entries are
/// listed but not guarded, so the row of a document that became a hard link is pruned and
/// counted in [`IndexResult::deleted`], while the file is listed as [`SkipReason::HardLink`].
#[test]
fn scan_skips_and_walk_hard_links_meet_the_prune_guard() {
    use crate::parser::{CodeParsersConfig, FileSizeLimit, FileSizeLimits};
    let dir = mk_tmp("i660-scan-and-walk-prune");
    let db = reparse_db();
    let limits = FileSizeLimits {
        text: FileSizeLimit::Bytes(16),
        ..FileSizeLimits::default()
    };
    let ids = ["md".to_string(), "txt".to_string()];
    let registry =
        Registry::from_enabled_with_plugins(&ids, &CodeParsersConfig::default(), None, limits)
            .expect("md + txt");
    std::fs::write(dir.0.join("h.md"), b"# H\n\nhi\n").unwrap();
    std::fs::write(dir.0.join("big.txt"), [b'x'; 64]).unwrap();

    let first = i660_rebuild(
        &db,
        &dir.0,
        &registry,
        progress::ProgressReporter::new(progress::ProgressMode::Quiet),
    );
    assert_eq!(
        first.updated, 1,
        "h.md was meant to be indexed first: {first:?}"
    );
    assert_skip_ledger(&first);

    std::fs::hard_link(dir.0.join("h.md"), dir.0.join("h2.md"))
        .expect("hard links need no privilege");
    let result = i660_rebuild(
        &db,
        &dir.0,
        &registry,
        progress::ProgressReporter::new(progress::ProgressMode::Quiet),
    );

    assert!(
        result.deleted >= 1,
        "the hard link's row was guarded: {result:?}"
    );
    assert_eq!(db.get_document_hash("h.md").unwrap(), None, "{result:?}");
    assert_eq!(
        listed(&result),
        vec![
            (
                "big.txt".to_string(),
                SkipReason::TooLarge {
                    size: 64,
                    limit: 16,
                    key: LimitKey::MaxTextFileSize,
                }
            ),
            ("h.md".to_string(), SkipReason::HardLink),
            ("h2.md".to_string(), SkipReason::HardLink),
        ],
        "{result:?}"
    );
    assert_skip_ledger(&result);
}

/// #660: [`crossed_rename_rewritten_during_the_run`] as it is, but with the given [`progress::CancelToken`] (if any)
/// on the reporter and set from the loop's event for the crossed file -- the last document
/// of a knowledge base that holds only it.
fn i660_crossed_rename_run(
    tag: &str,
    (old_rel, new_rel): (&str, &str),
    scanned: &'static [u8],
    rewritten: &'static [u8],
    cancel: Option<progress::CancelToken>,
) -> (TmpDir, Database, IndexResult) {
    let dir = mk_tmp(tag);
    let db = reparse_db();
    let registry = Registry::from_enabled(&["md".into(), "txt".into(), "docx".into()]).unwrap();
    assert!(
        rename_crosses_a_parser(&registry, old_rel, new_rel),
        "the pair was meant to cross a parser"
    );
    std::fs::write(dir.0.join(new_rel), scanned).unwrap();
    let old = parse_doc(&registry, "md", old_rel, SCANNED_BYTES).chunks;
    write_row(&db, old_rel, &sha256_hex_bytes(scanned), &old);
    let full = dir.0.join(new_rel);
    let (old_owned, new_owned) = (old_rel.to_string(), new_rel.to_string());
    let token_cb = cancel.clone();
    let progress = progress::ProgressReporter::with_callback(Box::new(move |event| {
        match event {
            progress::ProgressEvent::Renamed { old, new }
                if old == old_owned && new == new_owned =>
            {
                std::fs::write(&full, rewritten).unwrap();
            }
            // The loop's event for the crossed file, its last document.
            progress::ProgressEvent::Unchanged { rel, .. } if rel == new_owned => {
                if let Some(token) = &token_cb {
                    token.cancel();
                }
            }
            _ => {}
        }
    }));
    let progress = match cancel {
        Some(token) => progress.with_cancel(token),
        None => progress,
    };
    let result = i660_rebuild(&db, &dir.0, &registry, progress);
    (dir, db, result)
}

/// #660 T-L7 (I2): the further read of a crossed rename replaces the loop's
/// [`SkipReason::ChangedDuringRead`] entry: a read that indexes the file takes it off the
/// list, and one that skips it lists the new reason instead.
#[test]
fn a_crossed_rename_reread_replaces_its_entry() {
    let (_dir, _db, updated) = i660_crossed_rename_run(
        "i660-crossed-reread-updated",
        ("note.txt", "note.md"),
        SCANNED_BYTES,
        SECOND_READ_BYTES,
        None,
    );
    assert_eq!(updated.updated, 1, "{updated:?}");
    assert_eq!(listed(&updated), vec![], "{updated:?}");
    assert_skip_ledger(&updated);

    let (_dir, _db, skipped) = i660_crossed_rename_run(
        "i660-crossed-reread-skipped",
        ("note.md", "note.docx"),
        b"not a zip archive, as scanned",
        b"not a zip archive, as rewritten",
        None,
    );
    assert_eq!(
        listed(&skipped),
        vec![("note.docx".to_string(), SkipReason::CorruptOrUnsupported)],
        "{skipped:?}"
    );
    assert_skip_ledger(&skipped);
}

/// #660 T-L8b (spec 7): a run cancelled from the loop's event for its last document, a
/// crossed rename kept awaiting, stops before the further read (C3 does not hold back a
/// renamed path; the further read breaks before its first entry; C4 stops the run). The
/// entry stays the loop's [`SkipReason::ChangedDuringRead`], and the result keeps I1.
#[test]
fn a_run_cancelled_before_the_reread_keeps_changed_during_read() {
    let (_dir, _db, result) = i660_crossed_rename_run(
        "i660-crossed-reread-cancelled",
        ("note.txt", "note.md"),
        SCANNED_BYTES,
        SECOND_READ_BYTES,
        Some(progress::CancelToken::new()),
    );
    assert!(result.cancelled, "{result:?}");
    assert_eq!(result.updated, 0, "the further read ran: {result:?}");
    assert_eq!(
        listed(&result),
        vec![("note.md".to_string(), SkipReason::ChangedDuringRead)],
        "{result:?}"
    );
    assert_skip_ledger(&result);
}

/// #660 T-L7b (I2, path by path): two names whose lossy spelling is the same
/// (`a\u{FFFD}.md`), both empty, are one entry and one skip. Linux only: macOS refuses to
/// create a name that is not UTF-8.
#[cfg(target_os = "linux")]
#[test]
fn colliding_lossy_names_count_once() {
    use std::os::unix::ffi::OsStrExt;
    let dir = mk_tmp("i660-lossy-collision");
    let db = reparse_db();
    let registry = Registry::from_enabled(&["md".into()]).unwrap();
    for name in [&b"a\xff.md"[..], &b"a\xfe.md"[..]] {
        std::fs::write(dir.0.join(std::ffi::OsStr::from_bytes(name)), b"").unwrap();
    }
    let result = i660_rebuild(
        &db,
        &dir.0,
        &registry,
        progress::ProgressReporter::new(progress::ProgressMode::Quiet),
    );
    assert_eq!(result.skipped, 1, "{result:?}");
    assert_eq!(
        listed(&result),
        vec![("a\u{FFFD}.md".to_string(), SkipReason::NoText)],
        "{result:?}"
    );
    assert_skip_ledger(&result);
}
