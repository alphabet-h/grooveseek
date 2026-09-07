//! (AV-16) The code-parser round trip, in-process and without a model.
//!
//! `tests/code_formats_cli.rs` is the end-to-end suite for source files indexed one
//! definition at a time. Every test there spawns `groove index`, which loads the BGE-small
//! embedding model, so every test there is `#[ignore]` and the pull-request gate — plain
//! `cargo test` — runs none of them. Between v1.2.0 and this file, a change that lost a code
//! chunk's line numbers between the parser and the database would have shown up a day later
//! on the nightly, or not at all on a branch the nightly never sees.
//!
//! What this file covers that the parser's own unit tests (`src/parser/code/mod.rs`) do not:
//!
//! - the `rs` id reaching the registry **from a configuration file**, through the same
//!   [`grooveseek::config::Config::build_parser_registry`] the binary calls, rather than from
//!   a private constructor;
//! - a chunk's `line_range` / `symbol_kind` surviving `insert_chunk_with_code` and coming
//!   back out of `search_hybrid` as `start_line` / `end_line` / `symbol_kind` — the SQLite
//!   round trip the heavy suite's module doc names as the thing unit tests cannot see.
//!
//! What it deliberately leaves to the heavy suite: the JSON shape `groove search` prints,
//! the `lines:` line of the text printer, and the filters that separate code from prose.
//! Those are the binary's surface, and the binary needs the model.
//!
//! The embeddings here are constants. Nothing in this file is about ranking, so no assertion
//! reads `hits[0]`; a hit is found by its heading or its path.

#![cfg(feature = "grammar-rust")]

mod common;
use common::code_fixtures::{PARSERS_DEFAULT, PARSERS_MD_RS, SAMPLE_MD, SAMPLE_RS};
use common::temp::TempKbLayout;

use grooveseek::config::Config;
use grooveseek::db::{CodeMeta, Database, FusionParams, SearchFilters, SearchResult};
use grooveseek::parser::{Chunk, ParsedDocument, Parser, ParserExt, Registry};
use grooveseek::quality::{QualityProfile, chunk_quality_score};

/// The dimension `verify_embedding_meta` is told below; every vector in this file has it.
const DIM: usize = 384;

/// Build the registry the way the binary does: write `body` as `groove.toml`, load it, and
/// ask the loaded configuration for its parsers.
///
/// `Config::load_from` rather than `Registry::from_enabled`, because the question this suite
/// asks is whether `[parsers].enabled = ["md", "rs"]` in a file turns into a parser for `.rs`
/// — the id list is an intermediate the unit tests already cover.
fn registry_from(body: &str) -> Registry {
    let layout = TempKbLayout::new("code-light");
    let cfg_path = layout.root().join("groove.toml");
    std::fs::write(&cfg_path, body).expect("write groove.toml");
    let cfg = Config::load_from(&cfg_path).expect("load groove.toml");
    cfg.build_parser_registry(layout.kb())
        .expect("build the parser registry from the loaded config")
}

fn parser_for<'r>(registry: &'r Registry, ext: &str) -> &'r dyn Parser {
    registry
        .by_extension(ext)
        .unwrap_or_else(|| panic!("no parser registered for .{ext}"))
}

fn parse(parser: &dyn Parser, text: &str, path_hint: &str) -> ParsedDocument {
    parser
        .parse_bytes(text.as_bytes(), path_hint, &[])
        .unwrap_or_else(|e| panic!("parse {path_hint}: {e}"))
}

fn chunk_headed<'d>(doc: &'d ParsedDocument, heading: &str) -> &'d Chunk {
    doc.chunks
        .iter()
        .find(|c| c.heading.as_deref() == Some(heading))
        .unwrap_or_else(|| {
            let headings: Vec<Option<&str>> =
                doc.chunks.iter().map(|c| c.heading.as_deref()).collect();
            panic!("no chunk headed {heading:?}; the file produced {headings:?}")
        })
}

/// The same vector for every chunk and for the query, so the vector leg has no opinion and
/// returns everything it holds. Ranking is not the subject here.
fn flat_embedding() -> Vec<f32> {
    vec![0.1; DIM]
}

/// Write a parsed document the way `groove index` does, minus the embedder.
///
/// This mirrors the per-chunk loop in `src/indexer.rs` (`index_single_file`: quality profile
/// from `is_binary()` and `symbol_kind`, then `insert_chunk_with_code` with the chunk's
/// `line_range` and `symbol_kind`). It is a second copy of that loop, kept because the
/// indexer takes its embeddings from a model-backed `Embedder` with no seam to hand it
/// constants. If the indexer changes how it fills `CodeMeta`, this helper keeps writing the
/// old shape and this suite keeps passing on it — the drift is the price of running without
/// the model, and this comment is where it is written down.
fn store(db: &Database, rel: &str, parser: &dyn Parser, doc: &ParsedDocument) {
    let fm = &doc.frontmatter;
    let doc_id = db
        .upsert_document(
            rel,
            fm.title.as_deref(),
            fm.topic.as_deref(),
            None,
            fm.depth.as_deref(),
            &fm.tags,
            fm.date.as_deref(),
            "hash",
            doc.raw_content.len() as u64,
        )
        .unwrap_or_else(|e| panic!("upsert {rel}: {e}"));
    for chunk in &doc.chunks {
        let score = chunk_quality_score(
            chunk.heading.as_deref(),
            &chunk.content,
            QualityProfile::of(parser.is_binary(), chunk.symbol_kind.is_some()),
        );
        db.insert_chunk_with_code(
            doc_id,
            chunk.index as i32,
            chunk.heading.as_deref(),
            chunk.level,
            &chunk.content,
            chunk.context.as_deref(),
            &flat_embedding(),
            score,
            CodeMeta {
                line_range: chunk.line_range,
                symbol_kind: chunk.symbol_kind.as_deref(),
            },
        )
        .unwrap_or_else(|e| panic!("insert chunk {} of {rel}: {e}", chunk.index));
    }
}

fn hit_headed<'h>(hits: &'h [SearchResult], heading: &str) -> &'h SearchResult {
    hits.iter()
        .find(|h| h.heading.as_deref() == Some(heading))
        .unwrap_or_else(|| panic!("no hit headed {heading:?}: {hits:#?}"))
}

#[test]
fn the_default_config_has_no_rust_parser_and_enabling_rs_adds_one() {
    let default = registry_from(PARSERS_DEFAULT);
    assert!(
        default.by_extension("rs").is_none(),
        "a config with no [parsers] section must not parse .rs: {:?}",
        default.extensions()
    );
    assert!(default.by_extension("md").is_some());

    let with_rs = registry_from(PARSERS_MD_RS);
    assert!(
        with_rs.by_extension("rs").is_some(),
        "enabled = [\"md\", \"rs\"] must register the rs parser: {:?}",
        with_rs.extensions()
    );
    assert!(
        with_rs.by_extension("md").is_some(),
        "enabling rs must not displace md: {:?}",
        with_rs.extensions()
    );
}

#[test]
fn a_definition_chunk_reports_the_lines_it_occupies_and_the_grammars_word_for_it() {
    let registry = registry_from(PARSERS_MD_RS);
    let doc = parse(parser_for(&registry, "rs"), SAMPLE_RS, "fusion.rs");

    // The expected lines are the fixture's own, tabulated in `common/code_fixtures.rs`. The
    // function's range starts at its doc comment, three lines above the `pub fn`, because
    // that is the reading under which opening the file at `start_line` shows the chunk.
    for (heading, range, kind) in [
        ("function fuse_ranked_lists", (4, 17), "function"),
        ("class RankTable", (19, 21), "class"),
        ("method insert_row", (24, 26), "method"),
    ] {
        let chunk = chunk_headed(&doc, heading);
        assert_eq!(
            chunk.line_range,
            Some(range),
            "{heading}: line range should be {range:?}, chunk was {chunk:#?}"
        );
        assert_eq!(
            chunk.symbol_kind.as_deref(),
            Some(kind),
            "{heading}: symbol_kind should be the tags vocabulary, chunk was {chunk:#?}"
        );
    }

    // The imports above the first definition are kept as a gap chunk, which is not a
    // definition and says so.
    let gap = doc
        .chunks
        .iter()
        .find(|c| c.heading.is_none())
        .expect("the two `use` lines are covered by no definition and survive as a gap");
    assert!(
        gap.content.contains("use std::fmt::Debug;"),
        "{}",
        gap.content
    );
    assert_eq!(
        gap.symbol_kind, None,
        "a gap chunk is not a definition: {gap:#?}"
    );
}

/// The completion condition of AV-16: a test the pull-request gate runs that reads
/// `start_line`, `end_line` and `symbol_kind` back out of a search.
#[test]
fn line_range_and_symbol_kind_survive_the_database_and_come_back_as_start_line_end_line_symbol_kind()
 {
    let registry = registry_from(PARSERS_MD_RS);
    let rs = parser_for(&registry, "rs");
    let md = parser_for(&registry, "md");
    let code = parse(rs, SAMPLE_RS, "fusion.rs");
    let prose = parse(md, SAMPLE_MD, "notes.md");

    let db = Database::open_in_memory().expect("in-memory database");
    db.verify_embedding_meta("bge-small-en-v1.5", DIM as u32)
        .expect("create vec_chunks at the test dimension");
    store(&db, "fusion.rs", rs, &code);
    store(&db, "notes.md", md, &prose);

    // `reciprocal_fusion_weight` occurs only inside the function body, so the keyword leg
    // finds exactly that chunk; the vector leg, fed the same constant vector every chunk was
    // stored under, returns everything. With the filters at their defaults (no quality
    // cutoff — deliberately not the production 0.3, which is not what is under test) and a
    // limit past the chunk count, every stored chunk is in `hits` and a lookup by heading or
    // path cannot depend on rank.
    let hits = db
        .search_hybrid(
            "reciprocal_fusion_weight",
            &flat_embedding(),
            10,
            &SearchFilters::default(),
            FusionParams::default(),
        )
        .expect("search_hybrid");
    assert!(
        hits.len() > code.chunks.len(),
        "expected every chunk of both files back, got {hits:#?}"
    );

    // Literals first, so a wrong number names the line; then the parser's own values, so
    // "the chunker moved" and "the database lost it" fail as different messages.
    for (heading, (start, end), kind) in [
        ("function fuse_ranked_lists", (4, 17), "function"),
        ("method insert_row", (24, 26), "method"),
    ] {
        let hit = hit_headed(&hits, heading);
        assert_eq!(hit.path, "fusion.rs", "{hit:#?}");
        assert_eq!(
            (hit.start_line, hit.end_line),
            (Some(start), Some(end)),
            "{heading}: the stored line range came back wrong: {hit:#?}"
        );
        assert_eq!(
            hit.symbol_kind.as_deref(),
            Some(kind),
            "{heading}: the stored symbol_kind came back wrong: {hit:#?}"
        );

        let parsed = chunk_headed(&code, heading);
        assert_eq!(
            parsed.line_range.map(|(s, e)| (Some(s), Some(e))),
            Some((hit.start_line, hit.end_line)),
            "{heading}: the database returned a range the parser did not produce: parser \
             {parsed:#?}, hit {hit:#?}"
        );
        assert_eq!(
            parsed.symbol_kind, hit.symbol_kind,
            "{heading}: the database returned a symbol_kind the parser did not produce"
        );
    }

    // Prose went in through `insert_chunk_with_code` too, with an empty `CodeMeta`, and has
    // to come back with the three fields absent rather than zeroed.
    let prose_hits: Vec<&SearchResult> = hits.iter().filter(|h| h.path == "notes.md").collect();
    assert!(
        !prose_hits.is_empty(),
        "expected the prose chunk back: {hits:#?}"
    );
    for hit in prose_hits {
        assert_eq!(
            (hit.start_line, hit.end_line, hit.symbol_kind.as_deref()),
            (None, None, None),
            "a prose chunk carries no line range and no symbol_kind: {hit:#?}"
        );
    }
}
