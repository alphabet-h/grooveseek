//! The source layout table in `docs/ARCHITECTURE.md` describes the tree, in
//! both languages.
//!
//! The table went unrepaired across #195, #196 and #197, which each split a
//! module out of `server.rs`, and #212, which added `legacy.rs`; a docs-only
//! #214 repaired it by hand, because nothing read it. The release
//! checklist did list it, on the same line as `docs/usage.md`, whose flags a
//! test already compares with the binary; the green half of that line hid the
//! red half. This is the reader that line was missing.
//!
//! # What is walked
//!
//! Every file, whatever its extension, under the `src` of every workspace
//! member ([`common::source::source_tree`]), and the one table under
//! `## Source layout` on each page named in [`PAGES`]. The tree is the
//! filesystem, not `git ls-files`: the CI checkout is one commit deep, and a
//! guard that sees different things locally and in CI is the property
//! `docs_links_resolve.rs` refuses. So a stray file under `src` fails here on
//! the machine it is on, which is the right answer for a file the binary is
//! built from. The one thing not asked for a row is a name starting with a
//! dot -- `.DS_Store`, a swap file -- which is on one machine only; that is
//! decided here, not in the walk, because the stderr guard shares the walk
//! and reads every `.rs` the compiler might.
//!
//! # What is checked
//!
//! A member described by one row for its root -- `crates/groove-svc/` -- is
//! described at crate level and its files are not enumerated. Every other
//! member is described file by file: each of its files has a row whose first
//! cell is its path, or its directory has a row whose prose names the file by
//! bare name. That is the rule PR #214 worked out by hand, after a match by
//! file name reported `server/search.rs` as covered by the `db/search.rs` row:
//! a bare name is trusted only inside its own directory's row, and there it
//! names a direct child.
//!
//! The other direction too: a path in a first cell, a directory in a first
//! cell, a bare name in a directory row, and a `<member>/src/...` path named
//! in any cell all have to exist -- compared as written against the walked
//! tree, never through `Path::exists`, which answers yes to the wrong case on
//! Windows and macOS and no on the ubuntu leg of CI.
//!
//! The English and Japanese tables list the same first cells in the same
//! order and name the same members in each directory row. A row the reader
//! cannot classify, a row with a `|` that the parser would swallow, a first
//! cell written twice, and a directory row that names nothing are each
//! reported rather than skipped, because a row dropped from both languages
//! alike compares equal.
//!
//! The pre-#214 pages are frozen under `fixtures/docs-history/`, and the
//! reader is required to find exactly the four modules that commit added and
//! nothing else there.
//!
//! # What this cannot catch
//!
//! **The prose of a row.** The `doctor.rs` row said "Two groups" for a release
//! after #212 had made it three, and a reader of paths does not see that; nor
//! does it see an English sentence and a Japanese one describing the same
//! file differently. The inside of a crate-level member is not enumerated, so
//! a file added to `crates/groove-tray/src` needs no row. Nothing outside
//! `src` -- `benches/`, `tests/`, `build.rs`, `Cargo.toml` -- is walked, so a
//! row for one of those would be reported as stale; the fix is to widen the
//! walk and this paragraph, not to exempt the row. A relative path in prose
//! (`db/search.rs`) is not checked, only one written from the repository
//! root. A bare name whose extension has left the tree entirely is not
//! reported as stale, since the extension gate that keeps `powershell.exe`
//! out of the member list keeps that name out too.
//!
//! `[`link`]` in this file is a convention, not a checked reference: every
//! target under `tests/` is `doc = false`, as `common/docs.rs` explains.

mod common;

use common::docs::{github_flavour, line_of, read, repo_root};
use common::source::{source_tree, workspace_members};
use pulldown_cmark::{Event, HeadingLevel, Parser, Tag, TagEnd};
use std::collections::BTreeSet;
use std::ops::Range;
use std::path::Path;

/// One data row of the layout table.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Row {
    /// 1-based line of the row in the page.
    line: usize,
    /// The one code span in the first cell, as written.
    path: String,
    /// Every code span in the remaining cells, in order. Collected for every
    /// row; [`drift`] decides which rows' spans mean anything.
    spans: Vec<String>,
}

/// The layout table of one page, as read.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Table {
    rows: Vec<Row>,
    /// Rows whose first cell is not exactly one code span, with what was there
    /// instead. Reported rather than skipped: a row this reader cannot classify
    /// is dropped from both languages alike, and two empty readings agree.
    unreadable: Vec<(usize, String)>,
    /// `(line, pipes)`: rows whose source line has other than the header's
    /// number of unescaped `|`. Measured on pulldown-cmark 0.13.3: a `|` inside
    /// a code span splits the cell, and everything after the header's last
    /// column is dropped with no event -- so a name past that point vanishes
    /// from the reading, and only the source line can say so.
    ragged: Vec<(usize, usize)>,
    /// How many tables the section holds. The guard wants exactly one, so a
    /// second table under the same heading cannot lend its rows to the first.
    tables: usize,
    /// How many columns the table's header declares; a row's source line has
    /// one more `|` than that.
    columns: usize,
}

/// What the table and the tree disagree about.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Drift {
    /// Files under a file-level member that no row covers.
    missing: Vec<String>,
    /// `(line, path)`: something a row names that the tree does not have.
    stale: Vec<(usize, String)>,
    /// `(line, directory)`: a directory row under a file-level member that
    /// names no file at all.
    empty: Vec<(usize, String)>,
    /// `(path, lines)`: a first cell that appears in more than one row.
    duplicate: Vec<(String, Vec<usize>)>,
}

/// `|` characters that separate cells: every one not escaped with `\`.
///
/// A backslash escapes the character after it, including another backslash,
/// so what decides is the length of the run of backslashes before the pipe:
/// `\|` is an escaped pipe and `\\|` is an escaped backslash followed by a
/// pipe that still separates cells. Looking at the one preceding character
/// would call the second shape escaped and let the parser split the row while
/// this check stayed quiet (codex P2 on PR #231).
fn unescaped_pipes(line: &str) -> usize {
    let mut count = 0;
    let mut backslashes = 0usize;
    for c in line.chars() {
        match c {
            '\\' => backslashes += 1,
            '|' => {
                if backslashes.is_multiple_of(2) {
                    count += 1;
                }
                backslashes = 0;
            }
            _ => backslashes = 0,
        }
    }
    count
}

/// A row while its events are being read.
struct RowUnderRead {
    line: usize,
    range: Range<usize>,
    /// 1-based index of the cell the reader is inside.
    cell: usize,
    first_cell: Range<usize>,
    first_codes: Vec<String>,
    first_has_text: bool,
    spans: Vec<String>,
}

/// The one table under the `##` heading whose text is `section`, or `None`
/// when the page has no such heading.
///
/// Parsed with the same options every other docs guard uses, so a `|` line
/// inside a fenced block is a code block here as it is on GitHub, and the
/// header is [`Tag::TableHead`] rather than "the first two lines". Measured on
/// pulldown-cmark 0.13.3 before this was written: the header's cells sit
/// directly under `TableHead` with no `TableRow`; the delimiter row emits
/// nothing; a `TableRow`'s range runs from its first `|` through the newline;
/// a first cell written as `` **`path`** `` or `` [`path`](x) `` still yields
/// exactly one `Code` event; an empty cell yields no event at all.
///
/// The section ends at the next heading of level two or above. Line endings
/// are folded before parsing so a line number means the same thing for a page
/// read from disk and a fixture pulled in with `include_str!`.
fn layout_rows(markdown: &str, section: &str) -> Option<Table> {
    let md = markdown.replace("\r\n", "\n");
    let events: Vec<(Event<'_>, Range<usize>)> = Parser::new_ext(&md, github_flavour())
        .into_offset_iter()
        .collect();

    let mut table = Table::default();
    let mut found = false;
    let mut in_section = false;
    let mut heading: Option<String> = None;
    let mut columns = 0usize;
    let mut current: Option<RowUnderRead> = None;

    for (event, range) in &events {
        if let Event::Start(Tag::Heading { .. }) = event {
            heading = Some(String::new());
            continue;
        }
        if let Event::End(TagEnd::Heading(level)) = event {
            if let Some(text) = heading.take()
                && matches!(level, HeadingLevel::H1 | HeadingLevel::H2)
            {
                in_section = text.trim() == section;
                found |= in_section;
            }
            continue;
        }
        if let Some(text) = heading.as_mut() {
            if let Event::Text(t) | Event::Code(t) = event {
                text.push_str(t);
            }
            continue;
        }
        if !in_section {
            continue;
        }
        match event {
            Event::Start(Tag::Table(alignments)) => {
                table.tables += 1;
                columns = alignments.len();
                table.columns = columns;
            }
            Event::Start(Tag::TableRow) => {
                current = Some(RowUnderRead {
                    line: line_of(&md, range.start),
                    range: range.clone(),
                    cell: 0,
                    first_cell: range.clone(),
                    first_codes: Vec::new(),
                    first_has_text: false,
                    spans: Vec::new(),
                });
            }
            Event::Start(Tag::TableCell) => {
                if let Some(r) = current.as_mut() {
                    r.cell += 1;
                    if r.cell == 1 {
                        r.first_cell = range.clone();
                    }
                }
            }
            Event::Code(s) => {
                if let Some(r) = current.as_mut() {
                    if r.cell == 1 {
                        r.first_codes.push(s.to_string());
                    } else {
                        r.spans.push(s.to_string());
                    }
                }
            }
            Event::Text(t) => {
                if let Some(r) = current.as_mut()
                    && r.cell == 1
                    && !t.trim().is_empty()
                {
                    r.first_has_text = true;
                }
            }
            Event::End(TagEnd::TableRow) => {
                let r = current.take().expect("a row ends after it starts");
                let pipes = unescaped_pipes(md[r.range.clone()].trim_end());
                if pipes != columns + 1 {
                    table.ragged.push((r.line, pipes));
                }
                if r.first_codes.len() == 1 && !r.first_has_text {
                    table.rows.push(Row {
                        line: r.line,
                        path: r.first_codes.into_iter().next().expect("one span"),
                        spans: r.spans,
                    });
                } else {
                    table
                        .unreadable
                        .push((r.line, md[r.first_cell.clone()].trim().to_string()));
                }
            }
            _ => {}
        }
    }
    found.then_some(table)
}

/// The file name a span denotes inside its directory's row, if it is one.
///
/// A directory row's cell is prose, and its code spans name traits, flags,
/// executables and paths in other crates alongside its members. What makes a
/// span a member is its shape: one name, no directory, an extension the tree
/// actually has. That last gate is the one that separates `powershell.exe` from
/// `powershell.rs` on the same line, and it comes from the tree rather than
/// from a list kept here, so a new kind of file joins by existing.
///
/// A trailing `::item` is cut first: `windows.rs::resolve_action_target` names
/// the file and then something in it. The string is never handed to
/// `std::path::Path`, whose separator rules differ by platform -- a span like
/// `%LOCALAPPDATA%\groove\logs` would read one way on Windows and another on
/// Linux.
fn member_name(span: &str, extensions: &BTreeSet<String>) -> Option<String> {
    let head = span.split("::").next().unwrap_or("");
    if head.is_empty() || head.starts_with('.') || head.contains('/') || head.contains('\\') {
        return None;
    }
    if !head
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
    {
        return None;
    }
    let (_, ext) = head.rsplit_once('.')?;
    if ext.is_empty() || !extensions.contains(ext) {
        return None;
    }
    Some(head.to_string())
}

/// A span's head, with any `::item` cut off, when it is a path under some
/// member's `src` -- the shape a row uses to name a file in another crate.
fn source_path_in<'a>(span: &'a str, members: &[String]) -> Option<&'a str> {
    let head = span.split("::").next().unwrap_or("");
    members
        .iter()
        .any(|m| head.starts_with(&format!("{m}/src/")))
        .then_some(head)
}

/// The members a directory row names, by bare name.
fn members_of(row: &Row, extensions: &BTreeSet<String>) -> BTreeSet<String> {
    row.spans
        .iter()
        .filter_map(|span| member_name(span, extensions))
        .collect()
}

/// Whether a workspace member is described by one row for its root directory
/// -- `crates/groove-svc/` -- rather than file by file.
fn has_crate_row(table: &Table, member: &str) -> bool {
    let root = format!("{member}/");
    table.rows.iter().any(|r| r.path == root)
}

/// The members whose files the table enumerates: every one without a crate
/// row.
fn file_level(table: &Table, members: &[String]) -> Vec<String> {
    members
        .iter()
        .filter(|m| !has_crate_row(table, m))
        .cloned()
        .collect()
}

/// The extensions a bare name in a directory row may carry: those of the files
/// under the file-level members' `src`, read from the tree.
fn extensions_of(table: &Table, members: &[String], tree: &BTreeSet<String>) -> BTreeSet<String> {
    let enumerated = file_level(table, members);
    tree.iter()
        .filter(|f| {
            enumerated
                .iter()
                .any(|m| f.starts_with(&format!("{m}/src/")))
        })
        .filter_map(|f| f.rsplit('/').next())
        .filter_map(|name| name.rsplit_once('.'))
        .map(|(_, ext)| ext.to_string())
        .filter(|ext| !ext.is_empty())
        .collect()
}

/// Whether a path has a component starting with a dot: `.DS_Store`, an
/// editor's swap file, a tool's cache directory. Those are on one machine
/// only, and asking for a row for them would make this check answer
/// differently on every machine. They are dropped from the "missing" side
/// here, not from the walk, because the stderr guard shares that walk and
/// has to see a `.rs` the compiler is pointed at by `#[path]`.
fn is_machine_local(path: &str) -> bool {
    path.split('/').any(|part| part.starts_with('.'))
}

/// Whether anything in the tree lives under `dir` (which ends in `/`).
fn tree_has_dir(tree: &BTreeSet<String>, dir: &str) -> bool {
    tree.range(dir.to_string()..)
        .next()
        .is_some_and(|f| f.starts_with(dir))
}

/// The table against the tree.
///
/// A file under a file-level member is covered by a row whose first cell is its
/// path, or by the row for its parent directory naming it by bare name -- the
/// rule PR #214 worked out by hand, after a match by file name reported
/// `server/search.rs` as covered by the `db/search.rs` row. A bare name is read
/// as a direct child of that directory and nothing else; a name cannot say
/// which subdirectory it meant, and two `mod.rs` one level down would otherwise
/// share one.
///
/// Everything compares as strings against the walked tree. `Path::exists`
/// would answer yes to `Legacy.rs` on Windows and macOS and no on Linux, and a
/// guard that passes locally and fails on one CI leg teaches whoever hits it to
/// add an exception.
fn drift(table: &Table, members: &[String], tree: &BTreeSet<String>) -> Drift {
    let enumerated = file_level(table, members);
    let extensions = extensions_of(table, members, tree);
    let under_enumerated = |path: &str| {
        enumerated
            .iter()
            .any(|m| path.starts_with(&format!("{m}/src/")))
    };

    let mut lines_by_path: std::collections::BTreeMap<&str, Vec<usize>> = Default::default();
    for r in &table.rows {
        lines_by_path
            .entry(r.path.as_str())
            .or_default()
            .push(r.line);
    }
    let duplicate: Vec<(String, Vec<usize>)> = lines_by_path
        .iter()
        .filter(|(_, lines)| lines.len() > 1)
        .map(|(path, lines)| (path.to_string(), lines.clone()))
        .collect();

    let mut dir_members: std::collections::BTreeMap<&str, BTreeSet<String>> = Default::default();
    for r in &table.rows {
        if r.path.ends_with('/') {
            dir_members.insert(r.path.as_str(), members_of(r, &extensions));
        }
    }

    let mut missing: Vec<String> = Vec::new();
    for f in tree
        .iter()
        .filter(|f| under_enumerated(f) && !is_machine_local(f))
    {
        if lines_by_path.contains_key(f.as_str()) {
            continue;
        }
        let (parent, name) = f.rsplit_once('/').expect("a path under src has a parent");
        let parent = format!("{parent}/");
        if dir_members
            .get(parent.as_str())
            .is_some_and(|names| names.contains(name))
        {
            continue;
        }
        missing.push(f.clone());
    }

    let mut stale: Vec<(usize, String)> = Vec::new();
    let mut empty: Vec<(usize, String)> = Vec::new();
    for r in &table.rows {
        if r.path.ends_with('/') {
            if !tree_has_dir(tree, &r.path) {
                stale.push((r.line, r.path.clone()));
            }
            if under_enumerated(&r.path) {
                let names = &dir_members[r.path.as_str()];
                if names.is_empty() {
                    empty.push((r.line, r.path.clone()));
                }
                for name in names {
                    let member = format!("{}{name}", r.path);
                    if !tree.contains(&member) {
                        stale.push((r.line, member));
                    }
                }
            }
        } else if !tree.contains(&r.path) {
            stale.push((r.line, r.path.clone()));
        }
        for span in &r.spans {
            let Some(named) = source_path_in(span, members) else {
                continue;
            };
            let present = if named.ends_with('/') {
                tree_has_dir(tree, named)
            } else {
                tree.contains(named)
            };
            if !present {
                stale.push((r.line, named.to_string()));
            }
        }
    }

    missing.sort();
    stale.sort();
    stale.dedup();
    empty.sort();
    Drift {
        missing,
        stale,
        empty,
        duplicate,
    }
}

fn row(line: usize, path: &str, spans: &[&str]) -> Row {
    Row {
        line,
        path: path.to_string(),
        spans: spans.iter().map(|s| s.to_string()).collect(),
    }
}

fn tree(paths: &[&str]) -> BTreeSet<String> {
    paths.iter().map(|p| p.to_string()).collect()
}

fn members(names: &[&str]) -> Vec<String> {
    names.iter().map(|m| m.to_string()).collect()
}

fn paths(list: &[&str]) -> Vec<String> {
    list.iter().map(|p| p.to_string()).collect()
}

// ---------------------------------------------------------------------------
// The reader, against rows copied verbatim from `docs/ARCHITECTURE.md` and
// against the shapes pulldown-cmark 0.13.3 was measured to produce.
// ---------------------------------------------------------------------------

/// The heading, header and delimiter the live page has, with `rows` under
/// them -- so a row in a test sits on line 5.
fn page(rows: &str) -> String {
    format!("## Source layout\n\n| File | Responsibility |\n|---|---|\n{rows}\n")
}

fn read_page(rows: &str) -> Table {
    layout_rows(&page(rows), "Source layout").expect("the heading is there")
}

/// `docs/ARCHITECTURE.md` line 15 at `8d3fe18`.
const SERVER_SEARCH_ROW: &str = r#"| `grooveseek/src/server/search.rs` | (v1.0.0+) The search half: the `search` tool body, the pipeline it runs, and the limits a request is held to before either of them sees it. Split out of `server.rs` the way `db.rs` was split before it — bodies byte-identical and in the order they were already in, `mod tests` left in the parent. The only thing that changed was visibility: three private items became `pub(super)` because the parent still calls or names them. What stayed behind is the tool surface itself, the `#[tool_router]` / `#[tool_handler]` impls and the parameter and response types. |"#;

/// `docs/ARCHITECTURE.md` line 21 at `8d3fe18`.
const SERVICE_ROW: &str = r#"| `grooveseek/src/service/` | (v0.8.0+) Cross-platform OS user service installer. `mod.rs` (= `ServiceBackend` trait + `InstallContext` + `ServiceState`), `install.rs` / `uninstall.rs` / `status.rs` (= orchestration), `linux.rs` / `macos.rs` / `windows.rs` (= per-OS backends, cfg-gated), plus two modules deliberately kept **outside** the `cfg` gates so they compile and are tested on every OS leg: `render.rs` (v0.14.0+, the unit / plist templates and their escaping — a plist bug used to be detectable only on the macOS runner) and `powershell.rs` (v0.14.0+, the UTF-8 output prelude and the strict / diagnostic decoders for what `powershell.exe` writes back). Phase 1 = user-level only (= no admin/sudo, Linux systemd-user / macOS LaunchAgent / Windows Task Scheduler AT_LOGON). `groove service install` self-registers using Rust crates only (= no NSSM / WiX / 3rd-party tooling). The Windows backend (v0.8.3+) invokes PowerShell's `Register-ScheduledTask -Action -Trigger -Settings` cmdlet via `Command::new("powershell")` — `schtasks /Create /XML` was abandoned across v0.8.0 → v0.8.3 due to layered locale / elevation / Principal issues documented in `.dev/knowledge/windows-task-scheduler-pitfalls.md`. |"#;

/// `docs/ARCHITECTURE.md` line 24 at `8d3fe18`.
const PARSER_ROW: &str = r#"| `grooveseek/src/parser/` | Parser trait + Registry. `mod.rs` (Frontmatter / Chunk / ParsedDocument, plus the `parse_bytes(bytes, path_hint, exclude_headings) -> Result<ParsedDocument>` entry point that every call site — indexer and server — now goes through. `parse_bytes` lives on the `ParserExt` extension trait with a blanket `impl<T: Parser + ?Sized>`, so no parser can define it: it always delegates to `Parser::parse_bytes_inner` (same signature) inside a `catch_unwind`, and a panic anywhere in a parser or its dependencies becomes a per-file `Err` instead of aborting the whole `index` run. A default method would not do — it can be overridden, and an override silently bypasses the guard. `parse_bytes_inner`'s default impl validates UTF-8 then delegates to `parse`, so `md`/`txt` need no override, while binary-format parsers override it directly. `is_binary()` (default `false`) flags binary parsers for `get_document`'s size-cap classification and quality-filter exemption. `MAX_RAW_BINARY_BYTES` = 50 MiB, the shared raw-byte cap for binary formats used by both the indexer's size-skip guard and `get_document`; `MAX_RAW_TEXT_BYTES` = 50 MiB does the same for text formats at index time since v0.17.0, so no format is read into memory unbounded), `markdown.rs`, `txt.rs`, `pdf.rs` (v0.10.0+, see below), `ooxml.rs` / `xlsx.rs` / `docx.rs` / `pptx.rs` (v0.11.0+, see below), `panic_guard.rs` (see below), `registry.rs` (extension lookup, `binary_extensions()`). |"#;

/// `docs/ARCHITECTURE.md` line 38 at `8d3fe18`.
const TRANSPORT_ROW: &str = r#"| `grooveseek/src/transport/` | MCP transport abstraction. `mod.rs` (Transport enum + CLI/config resolution), `stdio.rs` (stdio), `http.rs` (rmcp `StreamableHttpService` + axum, mounts `/mcp` and `/healthz`; v0.8.0+ also mounts an admin sub-router with `/ui` + `/api/admin/status`, wrapped in `admin_security_headers` (CSP + `nosniff`, outermost so refusals carry them too). **One `dns_rebinding_gate` serves every route that validates** ([ADR-0009](decisions/0009-one-dns-rebinding-gate.md)): peer, then Host, then Origin, each group handed its own `DnsRebindingGate` state — `/mcp` the effective host and origin lists, the admin routes `allowed_admin_hosts` plus the same origin list plus the loopback-peer requirement, and `/healthz` the host list alone, and only when `healthz_public = false` (the default mounts it with no gate). rmcp's own checks are given empty lists on purpose, so `/mcp` is validated here too — outside the session gate, i.e. before admission. `/api/search` was removed in v0.27.0; `/ui` searches through `/mcp` now). `KbServerShared` is `Arc`-shared through a session factory so each connection gets a lightweight handle. |"#;

fn names(list: &[&str]) -> BTreeSet<String> {
    list.iter().map(|s| s.to_string()).collect()
}

#[test]
fn a_file_row_yields_its_path_and_every_span_of_its_prose() {
    let table = read_page(SERVER_SEARCH_ROW);
    assert_eq!(table.rows.len(), 1, "{table:?}");
    let r = &table.rows[0];
    assert_eq!(r.line, 5);
    assert_eq!(r.path, "grooveseek/src/server/search.rs");
    for named in ["server.rs", "db.rs", "pub(super)", "#[tool_router]"] {
        assert!(
            r.spans.iter().any(|s| s == named),
            "{named} in {:?}",
            r.spans
        );
    }
    assert!(table.unreadable.is_empty(), "{:?}", table.unreadable);
    assert!(table.ragged.is_empty(), "{:?}", table.ragged);
    assert_eq!(table.tables, 1);
}

#[test]
fn a_directory_row_takes_its_members_from_bare_names_whatever_separates_them() {
    let ext = names(&["rs", "html"]);
    let cases: [(&str, &str, &[&str]); 3] = [
        (
            SERVICE_ROW,
            "grooveseek/src/service/",
            &[
                "mod.rs",
                "install.rs",
                "uninstall.rs",
                "status.rs",
                "linux.rs",
                "macos.rs",
                "windows.rs",
                "render.rs",
                "powershell.rs",
            ],
        ),
        (
            PARSER_ROW,
            "grooveseek/src/parser/",
            &[
                "mod.rs",
                "markdown.rs",
                "txt.rs",
                "pdf.rs",
                "ooxml.rs",
                "xlsx.rs",
                "docx.rs",
                "pptx.rs",
                "panic_guard.rs",
                "registry.rs",
            ],
        ),
        (
            TRANSPORT_ROW,
            "grooveseek/src/transport/",
            &["mod.rs", "stdio.rs", "http.rs"],
        ),
    ];
    for (source, path, want) in cases {
        let table = read_page(source);
        assert_eq!(table.rows.len(), 1, "{path}: {table:?}");
        assert_eq!(table.rows[0].path, path);
        assert_eq!(members_of(&table.rows[0], &ext), names(want), "{path}");
    }
}

#[test]
fn a_row_inside_a_fenced_block_or_another_section_is_not_a_row() {
    let md = "## Source layout\n\n| File | Responsibility |\n|---|---|\n| `real.rs` | real |\n\n\
              ```text\n| `fake.rs` | inside a fence |\n```\n\n\
              ## Other\n\n| File | Responsibility |\n|---|---|\n| `other.rs` | second table |\n";
    let table = layout_rows(md, "Source layout").expect("the heading is there");
    let paths: Vec<&str> = table.rows.iter().map(|r| r.path.as_str()).collect();
    assert_eq!(paths, ["real.rs"]);
    assert_eq!(table.tables, 1);
}

#[test]
fn two_tables_under_one_heading_are_counted() {
    let md = "## Source layout\n\n| File | Responsibility |\n|---|---|\n| `a.rs` | a |\n\n\
              | File | Responsibility |\n|---|---|\n| `b.rs` | b |\n";
    let table = layout_rows(md, "Source layout").expect("the heading is there");
    assert_eq!(table.tables, 2);
}

#[test]
fn a_page_without_the_heading_reads_as_none() {
    let md = "## Something else\n\n| File | Responsibility |\n|---|---|\n| `a.rs` | a |\n";
    assert!(layout_rows(md, "Source layout").is_none());
    assert!(layout_rows(md, "Something else").is_some());
}

#[test]
fn a_first_cell_that_is_not_one_code_span_is_reported_rather_than_skipped() {
    // Measured on 0.13.3: a span inside `**` or a link is still one `Code`
    // event; plain text is `Text`; two spans are two `Code`; an empty cell has
    // no event at all.
    let table = read_page(
        "| grooveseek/src/foo.rs | plain text first cell |\n\
         | **`grooveseek/src/bar.rs`** | bold around the span |\n\
         | [`grooveseek/src/baz.rs`](x.md) | link around the span |\n\
         | `a.rs` `b.rs` | two spans |\n\
         |  | empty first cell |",
    );
    let paths: Vec<&str> = table.rows.iter().map(|r| r.path.as_str()).collect();
    assert_eq!(paths, ["grooveseek/src/bar.rs", "grooveseek/src/baz.rs"]);
    let lines: Vec<usize> = table.unreadable.iter().map(|(line, _)| *line).collect();
    assert_eq!(lines, [5, 8, 9]);
    assert!(
        table.unreadable[0].1.contains("grooveseek/src/foo.rs"),
        "{:?}",
        table.unreadable
    );
}

#[test]
fn a_pipe_inside_a_cell_is_reported_rather_than_swallowed() {
    // Measured on 0.13.3: the raw pipe splits the cell and `jq` is gone with
    // no event; the escaped one survives as `x | y`.
    let table = read_page(
        "| `a.rs` | a span with an escaped pipe `x \\| y` inside |\n\
         | `b.rs` | a span with a raw pipe `groove search | jq` inside |\n\
         | `c.rs` | three cells | extra cell |",
    );
    assert_eq!(table.ragged, [(6, 4), (7, 4)]);
    assert!(
        table.rows[0].spans.iter().any(|s| s == "x | y"),
        "{:?}",
        table.rows[0].spans
    );
}

#[test]
fn a_pipe_after_an_escaped_backslash_still_separates_cells() {
    // `\\|` is an escaped backslash and then a pipe. GitHub and pulldown-cmark
    // split the row there, so the count has to see it as a separator; only an
    // odd run of backslashes escapes the pipe.
    assert_eq!(unescaped_pipes(r"| `a.rs` | x \| y |"), 3);
    assert_eq!(unescaped_pipes(r"| `a.rs` | x \\| y |"), 4);
    assert_eq!(unescaped_pipes(r"| `a.rs` | x \\\| y |"), 3);
    let table = read_page(r"| `a.rs` | a span with `x \\| y` inside |");
    assert_eq!(table.ragged, [(5, 4)]);
}

#[test]
fn rows_keep_their_line_numbers_under_windows_line_endings() {
    let unix = page(&format!("{SERVER_SEARCH_ROW}\n{SERVICE_ROW}"));
    let windows = unix.replace('\n', "\r\n");
    let a = layout_rows(&unix, "Source layout").expect("the heading is there");
    let b = layout_rows(&windows, "Source layout").expect("the heading is there");
    assert_eq!(a, b);
    assert_eq!(a.rows[1].line, 6);
}

// ---------------------------------------------------------------------------
// The predicate, one property at a time, each against a case taken from this
// repository's history or from the shape of its table rather than from what
// the implementation happens to do.
// ---------------------------------------------------------------------------

#[test]
fn a_file_row_covers_its_path_and_claims_nothing_its_prose_names() {
    // `docs/ARCHITECTURE.md` line 15 names `server.rs` and `db.rs` in prose
    // while describing `server/search.rs`.
    let table = Table {
        rows: vec![row(
            15,
            "grooveseek/src/server/search.rs",
            &["server.rs", "db.rs"],
        )],
        ..Default::default()
    };
    let seen = tree(&[
        "grooveseek/src/server/search.rs",
        "grooveseek/src/server/db.rs",
    ]);
    let d = drift(&table, &members(&["grooveseek"]), &seen);
    assert_eq!(d.missing, paths(&["grooveseek/src/server/db.rs"]));
    assert!(d.stale.is_empty(), "{:?}", d.stale);
}

#[test]
fn a_bare_name_is_trusted_only_inside_its_own_directory_row() {
    // The trap PR #214 walked into: `search.rs` appears in the `db/search.rs`
    // row, and a match by file name reported `server/search.rs` as covered.
    let table = Table {
        rows: vec![
            row(46, "grooveseek/src/db/search.rs", &[]),
            row(47, "grooveseek/src/db/", &["search.rs"]),
        ],
        ..Default::default()
    };
    let seen = tree(&[
        "grooveseek/src/db/search.rs",
        "grooveseek/src/server/search.rs",
    ]);
    let d = drift(&table, &members(&["grooveseek"]), &seen);
    assert_eq!(d.missing, paths(&["grooveseek/src/server/search.rs"]));
    assert!(d.stale.is_empty(), "{:?}", d.stale);
}

#[test]
fn a_bare_name_names_a_direct_child_and_not_a_deeper_file() {
    // Two `mod.rs` one level down would otherwise both be "covered" by the
    // same bare name, which is the file-name trap one directory up.
    let table = Table {
        rows: vec![row(20, "grooveseek/src/a/", &["c.rs"])],
        ..Default::default()
    };
    let seen = tree(&["grooveseek/src/a/b/c.rs"]);
    let d = drift(&table, &members(&["grooveseek"]), &seen);
    assert_eq!(d.missing, paths(&["grooveseek/src/a/b/c.rs"]));
    assert_eq!(d.stale, vec![(20, "grooveseek/src/a/c.rs".to_string())]);
}

#[test]
fn case_is_compared_as_written() {
    // `Path::exists` would say yes to this on Windows and macOS and no on the
    // ubuntu leg of CI. Strings say no everywhere.
    let table = Table {
        rows: vec![row(32, "grooveseek/src/Legacy.rs", &[])],
        ..Default::default()
    };
    let seen = tree(&["grooveseek/src/legacy.rs"]);
    let d = drift(&table, &members(&["grooveseek"]), &seen);
    assert_eq!(d.missing, paths(&["grooveseek/src/legacy.rs"]));
    assert_eq!(d.stale, vec![(32, "grooveseek/src/Legacy.rs".to_string())]);
}

#[test]
fn a_member_without_a_crate_row_is_walked_file_by_file() {
    let seen = tree(&["grooveseek/src/main.rs", "crates/groove-svc/src/main.rs"]);
    let both = members(&["grooveseek", "crates/groove-svc"]);

    let without = Table {
        rows: vec![row(12, "grooveseek/src/main.rs", &[])],
        ..Default::default()
    };
    let d = drift(&without, &both, &seen);
    assert_eq!(d.missing, paths(&["crates/groove-svc/src/main.rs"]));

    let with = Table {
        rows: vec![
            row(12, "grooveseek/src/main.rs", &[]),
            row(41, "crates/groove-svc/", &["serve"]),
        ],
        ..Default::default()
    };
    let d = drift(&with, &both, &seen);
    assert!(d.missing.is_empty(), "{:?}", d.missing);
    assert!(d.stale.is_empty(), "{:?}", d.stale);
    assert!(d.empty.is_empty(), "{:?}", d.empty);
}

#[test]
fn a_crate_row_claims_no_file_even_when_its_prose_names_one() {
    // `docs/ARCHITECTURE.md` line 40 describes the tray crate and names
    // `grooveseek/src/service/windows.rs` on the way. Neither is a member of
    // anything; the second is a path, and paths in prose are checked (below),
    // not credited.
    let table = Table {
        rows: vec![
            row(38, "grooveseek/src/service/", &["windows.rs"]),
            row(
                40,
                "crates/groove-tray/",
                &[
                    "groove-tray.exe",
                    "grooveseek/src/service/windows.rs",
                    "tray.rs",
                ],
            ),
        ],
        ..Default::default()
    };
    let seen = tree(&[
        "grooveseek/src/service/windows.rs",
        "crates/groove-tray/src/tray.rs",
        "crates/groove-tray/src/ui.rs",
    ]);
    let d = drift(
        &table,
        &members(&["grooveseek", "crates/groove-tray"]),
        &seen,
    );
    assert!(d.missing.is_empty(), "{:?}", d.missing);
    assert!(d.stale.is_empty(), "{:?}", d.stale);
}

#[test]
fn a_path_named_in_prose_inside_the_table_is_checked_against_the_tree() {
    // JA line 41 writes `grooveseek/src/service/windows.rs::resolve_action_target`.
    let table = Table {
        rows: vec![row(
            41,
            "crates/groove-svc/",
            &["grooveseek/src/service/windows.rs::resolve_action_target"],
        )],
        ..Default::default()
    };
    let both = members(&["grooveseek", "crates/groove-svc"]);
    let present = tree(&[
        "grooveseek/src/service/windows.rs",
        "crates/groove-svc/src/main.rs",
    ]);
    let d = drift(&table, &both, &present);
    assert!(d.stale.is_empty(), "{:?}", d.stale);

    let moved = tree(&[
        "grooveseek/src/service/win.rs",
        "crates/groove-svc/src/main.rs",
    ]);
    let d = drift(&table, &both, &moved);
    assert_eq!(
        d.stale,
        vec![(41, "grooveseek/src/service/windows.rs".to_string())]
    );
}

#[test]
fn a_directory_row_that_names_nothing_is_reported() {
    let table = Table {
        rows: vec![row(
            21,
            "grooveseek/src/service/",
            &["ServiceBackend", "cfg"],
        )],
        ..Default::default()
    };
    let seen = tree(&["grooveseek/src/service/mod.rs"]);
    let d = drift(&table, &members(&["grooveseek"]), &seen);
    assert_eq!(d.empty, vec![(21, "grooveseek/src/service/".to_string())]);
    assert_eq!(d.missing, paths(&["grooveseek/src/service/mod.rs"]));
}

#[test]
fn a_directory_row_member_the_directory_does_not_have_is_stale() {
    let table = Table {
        rows: vec![row(21, "grooveseek/src/service/", &["rendr.rs"])],
        ..Default::default()
    };
    let seen = tree(&["grooveseek/src/service/render.rs"]);
    let d = drift(&table, &members(&["grooveseek"]), &seen);
    assert_eq!(
        d.stale,
        vec![(21, "grooveseek/src/service/rendr.rs".to_string())]
    );
    assert_eq!(d.missing, paths(&["grooveseek/src/service/render.rs"]));
}

#[test]
fn a_directory_row_for_a_directory_the_tree_does_not_have_is_stale() {
    let table = Table {
        rows: vec![
            row(11, "grooveseek/src/lib.rs", &[]),
            row(30, "grooveseek/src/gone/", &["x.rs"]),
        ],
        ..Default::default()
    };
    let seen = tree(&["grooveseek/src/lib.rs"]);
    let d = drift(&table, &members(&["grooveseek"]), &seen);
    assert_eq!(
        d.stale,
        vec![
            (30, "grooveseek/src/gone/".to_string()),
            (30, "grooveseek/src/gone/x.rs".to_string()),
        ]
    );
}

#[test]
fn a_dot_prefixed_name_is_not_asked_for_a_row_but_is_still_in_the_tree() {
    // `.DS_Store` is on one machine only, so no row is demanded for it; a row
    // that names a dot-prefixed file is still compared with the tree.
    let table = Table {
        rows: vec![
            row(11, "grooveseek/src/lib.rs", &[]),
            row(12, "grooveseek/src/.generated.rs", &[]),
        ],
        ..Default::default()
    };
    let seen = tree(&[
        "grooveseek/src/lib.rs",
        "grooveseek/src/.DS_Store",
        "grooveseek/src/.generated.rs",
        "grooveseek/src/.cache/scratch.rs",
    ]);
    let d = drift(&table, &members(&["grooveseek"]), &seen);
    assert!(d.missing.is_empty(), "{:?}", d.missing);
    assert!(d.stale.is_empty(), "{:?}", d.stale);

    let gone = tree(&["grooveseek/src/lib.rs"]);
    let d = drift(&table, &members(&["grooveseek"]), &gone);
    assert_eq!(
        d.stale,
        vec![(12, "grooveseek/src/.generated.rs".to_string())]
    );
}

#[test]
fn a_first_cell_written_twice_is_reported() {
    let table = Table {
        rows: vec![
            row(11, "grooveseek/src/lib.rs", &[]),
            row(19, "grooveseek/src/lib.rs", &[]),
        ],
        ..Default::default()
    };
    let seen = tree(&["grooveseek/src/lib.rs"]);
    let d = drift(&table, &members(&["grooveseek"]), &seen);
    assert_eq!(
        d.duplicate,
        vec![("grooveseek/src/lib.rs".to_string(), vec![11, 19])]
    );
}

#[test]
fn a_span_that_is_not_a_file_name_is_neither_member_nor_stale() {
    let ext: BTreeSet<String> = ["rs", "html"].iter().map(|s| s.to_string()).collect();
    for span in [
        "ServiceBackend",
        "cfg",
        "md",
        "powershell.exe",
        "healthz_public = false",
        "parse_bytes(bytes, path_hint, exclude_headings) -> Result<ParsedDocument>",
        ".dev/knowledge/windows-task-scheduler-pitfalls.md",
        "grooveseek/src/service/windows.rs",
        "/mcp",
        "%LOCALAPPDATA%\\groove\\logs\\tray.YYYY-MM-DD",
        ".hidden.rs",
    ] {
        assert_eq!(member_name(span, &ext), None, "{span}");
    }
    assert_eq!(member_name("render.rs", &ext).as_deref(), Some("render.rs"));
    assert_eq!(
        member_name("webui_index.html", &ext).as_deref(),
        Some("webui_index.html")
    );
    assert_eq!(
        member_name("panic_guard.rs", &ext).as_deref(),
        Some("panic_guard.rs")
    );
}

#[test]
fn an_item_suffix_is_cut_before_a_span_is_classified() {
    let ext: BTreeSet<String> = ["rs"].iter().map(|s| s.to_string()).collect();
    assert_eq!(
        member_name("windows.rs::resolve_action_target", &ext).as_deref(),
        Some("windows.rs")
    );
    assert_eq!(
        member_name("Parser::parse_bytes_inner", &ext),
        None,
        "an item path whose head is not a file name is not one"
    );
}

#[test]
fn the_extensions_a_bare_name_may_carry_come_from_the_tree() {
    let seen = tree(&[
        "grooveseek/src/main.rs",
        "grooveseek/src/transport/webui_index.html",
        "crates/groove-svc/src/main.rs",
        "crates/groove-svc/src/icon.ico",
    ]);
    // Only file-level members contribute; the svc crate is described at
    // crate level here, so `ico` is not a name a directory row could use.
    let table = Table {
        rows: vec![row(41, "crates/groove-svc/", &[])],
        ..Default::default()
    };
    let ext = extensions_of(
        &table,
        &members(&["grooveseek", "crates/groove-svc"]),
        &seen,
    );
    let want: BTreeSet<String> = ["rs", "html"].iter().map(|s| s.to_string()).collect();
    assert_eq!(ext, want);
}

/// The walk has to reach each of [`WALK_ANCHORS`], or whatever it reached is
/// not this workspace's source.
fn assert_walk_reaches(root: &Path, tree: &BTreeSet<String>) {
    for required in WALK_ANCHORS {
        assert!(
            tree.contains(*required),
            "the walk from {} did not reach {required}, so whatever it did reach is \
             not this workspace's source. A member was dropped from the manifest \
             read, an extension filter crept in, or the layout moved and this test \
             moves with it rather than being relaxed",
            root.display()
        );
    }
}

#[test]
fn the_walk_reaches_every_member_and_every_extension() {
    assert_walk_reaches(&repo_root(), &source_tree());
}

// ---------------------------------------------------------------------------
// The live pages against the live tree.
// ---------------------------------------------------------------------------

/// The crate the table exists to describe file by file. A crate-level row for
/// it is refused, and a hint about a file in any other crate may offer one.
const TABLE_CRATE: &str = "grooveseek";

/// The pages under guard and the heading their table sits under.
const PAGES: &[(&str, &str)] = &[
    ("docs/ARCHITECTURE.md", "Source layout"),
    ("docs/ARCHITECTURE.ja.md", "ソース別の責務"),
];

/// Paths the walk has to reach, each pinning one thing the walk claims:
/// the crate under test, a file that is not `.rs`, a member other than the
/// crate under test.
const WALK_ANCHORS: &[&str] = &[
    "grooveseek/src/main.rs",
    "grooveseek/src/transport/webui_index.html",
    "crates/groove-svc/src/main.rs",
];

/// Directory rows the pages have, and a member each has to name -- one that
/// sits inside prose with a parenthesis after it, and one that is the last
/// span of the longest cell on the page. Pinned so that the bare-name half of
/// the rule is exercised, not just the file-row half.
const DIRECTORY_ANCHORS: &[(&str, &str)] = &[
    ("grooveseek/src/service/", "render.rs"),
    ("grooveseek/src/parser/", "registry.rs"),
];

struct Live {
    page: &'static str,
    table: Table,
    members: Vec<String>,
    extensions: BTreeSet<String>,
    drift: Drift,
}

/// Every page read against the tree, after the walk and each table have been
/// shown to be what this file says they are. A walk that finds nothing
/// passes, so say what was walked -- by name, not by count.
fn live() -> Vec<Live> {
    let root = repo_root();
    let members = workspace_members();
    let tree = source_tree();
    assert_walk_reaches(&root, &tree);

    let mut out = Vec::new();
    for (page, section) in PAGES {
        let markdown = read(&root.join(page));
        let table = layout_rows(&markdown, section).unwrap_or_else(|| {
            panic!(
                "{page} has no `## {section}` heading, so there is no table to \
                 check. If the heading was renamed, rename it in PAGES too; \
                 this test moves with the page rather than being relaxed"
            )
        });
        assert_eq!(
            table.tables, 1,
            "{page}: the section under `## {section}` holds {} tables and this \
             guard reads one. A second table there could lend the first its \
             rows, so split the section or move the table",
            table.tables
        );
        assert!(
            !table.rows.is_empty(),
            "{page}: the table under `## {section}` has no row this reader can \
             classify; unreadable: {:?}",
            table.unreadable
        );
        assert!(
            !has_crate_row(&table, TABLE_CRATE),
            "{page} has a `{TABLE_CRATE}/` row, which would describe the crate at \
             crate level and switch off the file-by-file check for the one \
             crate this table exists to describe. Delete that row; the files \
             are the rows"
        );
        assert!(
            table
                .rows
                .iter()
                .any(|r| r.path == "grooveseek/src/main.rs"),
            "{page} has no `grooveseek/src/main.rs` row, so the file-row half of \
             the rule was never exercised on this page"
        );
        let extensions = extensions_of(&table, &members, &tree);
        assert!(
            extensions.contains("rs"),
            "the file-level members' src holds no .rs file, so no bare name \
             could ever be a member: {extensions:?}"
        );
        for (dir, name) in DIRECTORY_ANCHORS {
            let row = table
                .rows
                .iter()
                .find(|r| r.path == *dir)
                .unwrap_or_else(|| {
                    panic!(
                        "{page} has no `{dir}` directory row. If that directory is now \
                     described file by file, move this anchor to another \
                     directory row; if no directory row is left, delete the \
                     bare-name branch of the rule with it rather than leaving \
                     an anchor that pins nothing"
                    )
                });
            assert!(
                members_of(row, &extensions).contains(*name),
                "{page}: the `{dir}` row at line {} does not name `{name}`, so \
                 the bare-name half of the rule is not being read on this page. \
                 Members read: {:?}",
                row.line,
                members_of(row, &extensions)
            );
        }
        let drift = drift(&table, &members, &tree);
        out.push(Live {
            page,
            table,
            members: members.clone(),
            extensions,
            drift,
        });
    }
    out
}

/// What to do about a file the table does not cover: the ways a row can
/// cover it, and -- for a crate other than the one this table is for -- the
/// crate-level row that would describe the crate as a whole instead.
fn hint_for(path: &str, table: &Table, members: &[String]) -> String {
    let (parent, name) = path
        .rsplit_once('/')
        .expect("a path under src has a parent");
    let dir = format!("{parent}/");
    let mut hint = match table.rows.iter().find(|r| r.path == dir) {
        Some(row) => format!(
            "name `{name}` in the `{dir}` row at line {}, or give it a row of its own",
            row.line
        ),
        None => format!("give it a row of its own, or a `{dir}` row that names it"),
    };
    if let Some(member) = members
        .iter()
        .find(|m| m.as_str() != TABLE_CRATE && path.starts_with(&format!("{m}/src/")))
    {
        hint.push_str(&format!(
            ", or describe the crate as a whole with a `{member}/` row"
        ));
    }
    hint
}

#[test]
fn every_file_under_a_file_level_member_has_a_row_or_is_named_by_its_directory_row() {
    let mut offenders: Vec<String> = Vec::new();
    for live in live() {
        let page = live.page;
        for path in &live.drift.missing {
            offenders.push(format!(
                "{page}: `{path}` has no row -- {}",
                hint_for(path, &live.table, &live.members)
            ));
        }
        for (line, dir) in &live.drift.empty {
            offenders.push(format!(
                "{page}:{line}: the `{dir}` row names no file, so it covers \
                 nothing and reads as if it did; list the files it holds, in \
                 backticks, or delete it"
            ));
        }
        for (line, cell) in &live.table.unreadable {
            offenders.push(format!(
                "{page}:{line}: the first cell is not one path in backticks: {cell:?}"
            ));
        }
        for (path, lines) in &live.drift.duplicate {
            offenders.push(format!(
                "{page}: `{path}` has a row at each of lines {lines:?}; one of \
                 them is the copy"
            ));
        }
    }
    offenders.sort();
    assert!(
        offenders.is_empty(),
        "the source layout table does not describe the tree:\n  {}\n\
         A file is covered by a row whose first cell is its path, or by its \
         directory's row naming it in backticks; a mention in plain text is not \
         a row. Add the row rather than relaxing this check -- the table went \
         four pull requests without one before this test existed.",
        offenders.join("\n  ")
    );
}

#[test]
fn no_row_names_a_file_or_directory_the_tree_does_not_have() {
    let mut offenders: Vec<String> = Vec::new();
    for live in live() {
        for (line, path) in &live.drift.stale {
            offenders.push(format!(
                "{}:{line}: `{path}` is in the table and not in the tree",
                live.page
            ));
        }
    }
    offenders.sort();
    assert!(
        offenders.is_empty(),
        "the source layout table names what the tree does not have:\n  {}\n\
         Paths compare as written, so a wrong case fails here on every platform. \
         Delete or rename the row in the commit that deletes or renames the file. \
         A bare name in a directory row is read as a file of that directory; to \
         mention a file elsewhere, write its full path. `legacy.rs` is scheduled \
         to go in 1.1.0 and will land here when it does; its row goes with it.",
        offenders.join("\n  ")
    );
}

#[test]
fn every_row_in_the_layout_table_has_as_many_cells_as_its_header() {
    let mut offenders: Vec<String> = Vec::new();
    for live in live() {
        for (line, pipes) in &live.table.ragged {
            offenders.push(format!(
                "{}:{line}: {pipes} unescaped `|` where the header has {}",
                live.page,
                live.table.columns + 1
            ));
        }
    }
    offenders.sort();
    assert!(
        offenders.is_empty(),
        "these rows do not have the header's number of cells:\n  {}\n\
         A `|` inside backticks still splits the cell, and the parser drops \
         whatever lands past the last column without a word -- a file named \
         there is not read. Write it as `\\|`.",
        offenders.join("\n  ")
    );
}

#[test]
fn the_english_and_japanese_tables_list_the_same_paths_in_the_same_order_with_the_same_members() {
    let pages = live();
    let shape = |live: &Live| -> Vec<(String, BTreeSet<String>)> {
        live.table
            .rows
            .iter()
            .map(|r| {
                let members = if r.path.ends_with('/') {
                    members_of(r, &live.extensions)
                } else {
                    BTreeSet::new()
                };
                (r.path.clone(), members)
            })
            .collect()
    };
    let english = &pages[0];
    for other in &pages[1..] {
        let a = shape(english);
        let b = shape(other);
        let first_difference = a
            .iter()
            .zip(b.iter())
            .position(|(x, y)| x != y)
            .or_else(|| (a.len() != b.len()).then_some(a.len().min(b.len())));
        if let Some(i) = first_difference {
            let describe = |live: &Live, rows: &[(String, BTreeSet<String>)]| match rows.get(i) {
                Some((path, members)) => format!(
                    "{}:{}: `{path}` {members:?}",
                    live.page, live.table.rows[i].line
                ),
                None => format!("{}: no row {} (the table ends)", live.page, i + 1),
            };
            panic!(
                "the two tables stop agreeing at row {}:\n  {}\n  {}\n\
                 The pages are translations of one table: the same first cells in \
                 the same order, and the same files named in each directory row. \
                 Edit both in the same commit.",
                i + 1,
                describe(english, &a),
                describe(other, &b)
            );
        }
    }
}

// ---------------------------------------------------------------------------
// The same predicate against the defect it was built for, frozen at the commit
// before the fix.
//
// The pages are `git show` output, byte for byte, under `fixtures/docs-history/`
// (the docs corpus walk skips that directory, so no link or command guard reads
// them as documentation). The tree and the expectations below are written by
// hand from `git ls-tree`; deriving either from the walker would make these
// tests agree with whatever it does rather than with what the repository held.
// The tree is kept as a literal even though it equals today's tree: `legacy.rs`
// is scheduled for deletion in 1.1.0, and a walker-derived tree would turn
// "four missing, none stale" into "three missing, one stale" that day.
//
// The frozen pages are not run against the live tree on purpose. Today the two
// trees are identical, so the result would be the same four; the first file
// added under `src` would make it five and teach whoever hit it to edit the
// expectation, which is the rot this file exists to stop.
// ---------------------------------------------------------------------------

/// `git -C <root> show 739210b:docs/ARCHITECTURE.md`, blob
/// `9ce36a925dd9c9566e1ac7d0bfdb8a550c9b8641` -- the parent of `6191340`
/// (#214), which added four rows and reworded the `doctor.rs` row; the reader
/// sees only the rows.
const PAGE_AT_739210B: &str = include_str!("fixtures/docs-history/ARCHITECTURE-739210b.md");

/// `git -C <root> show 739210b:docs/ARCHITECTURE.ja.md`, blob
/// `8202adf6e5bd06a07a03e43543c1470ee3306710`.
const PAGE_AT_739210B_JA: &str = include_str!("fixtures/docs-history/ARCHITECTURE-739210b.ja.md");

/// `[workspace].members` at `739210b`.
const MEMBERS_AT_739210B: &[&str] = &["grooveseek", "crates/groove-tray", "crates/groove-svc"];

/// `git -C <root> ls-tree -r --name-only 739210b -- grooveseek/src
/// crates/groove-tray/src crates/groove-svc/src`, as printed.
const TREE_AT_739210B: &[&str] = &[
    "crates/groove-svc/src/main.rs",
    "crates/groove-tray/src/cli.rs",
    "crates/groove-tray/src/config.rs",
    "crates/groove-tray/src/daemon.rs",
    "crates/groove-tray/src/install.rs",
    "crates/groove-tray/src/lib.rs",
    "crates/groove-tray/src/logger.rs",
    "crates/groove-tray/src/main.rs",
    "crates/groove-tray/src/poll.rs",
    "crates/groove-tray/src/powershell.rs",
    "crates/groove-tray/src/process.rs",
    "crates/groove-tray/src/state.rs",
    "crates/groove-tray/src/test_support.rs",
    "crates/groove-tray/src/tray.rs",
    "crates/groove-tray/src/ui.rs",
    "grooveseek/src/config.rs",
    "grooveseek/src/db.rs",
    "grooveseek/src/db/fts_query.rs",
    "grooveseek/src/db/meta.rs",
    "grooveseek/src/db/schema.rs",
    "grooveseek/src/db/search.rs",
    "grooveseek/src/db/storage.rs",
    "grooveseek/src/doctor.rs",
    "grooveseek/src/embedder.rs",
    "grooveseek/src/eval.rs",
    "grooveseek/src/exclusion.rs",
    "grooveseek/src/graph.rs",
    "grooveseek/src/graph_render.rs",
    "grooveseek/src/indexer.rs",
    "grooveseek/src/indexer/progress.rs",
    "grooveseek/src/legacy.rs",
    "grooveseek/src/lib.rs",
    "grooveseek/src/links.rs",
    "grooveseek/src/main.rs",
    "grooveseek/src/markdown.rs",
    "grooveseek/src/mmr.rs",
    "grooveseek/src/parent.rs",
    "grooveseek/src/parser/docx.rs",
    "grooveseek/src/parser/markdown.rs",
    "grooveseek/src/parser/mod.rs",
    "grooveseek/src/parser/ooxml.rs",
    "grooveseek/src/parser/panic_guard.rs",
    "grooveseek/src/parser/pdf.rs",
    "grooveseek/src/parser/pptx.rs",
    "grooveseek/src/parser/registry.rs",
    "grooveseek/src/parser/txt.rs",
    "grooveseek/src/parser/xlsx.rs",
    "grooveseek/src/poison.rs",
    "grooveseek/src/prompts.rs",
    "grooveseek/src/quality.rs",
    "grooveseek/src/resources.rs",
    "grooveseek/src/schema.rs",
    "grooveseek/src/schema_compat.rs",
    "grooveseek/src/server.rs",
    "grooveseek/src/server/documents.rs",
    "grooveseek/src/server/kb_uri.rs",
    "grooveseek/src/server/search.rs",
    "grooveseek/src/service/install.rs",
    "grooveseek/src/service/linux.rs",
    "grooveseek/src/service/macos.rs",
    "grooveseek/src/service/mod.rs",
    "grooveseek/src/service/powershell.rs",
    "grooveseek/src/service/render.rs",
    "grooveseek/src/service/status.rs",
    "grooveseek/src/service/uninstall.rs",
    "grooveseek/src/service/windows.rs",
    "grooveseek/src/test_support.rs",
    "grooveseek/src/transport/http.rs",
    "grooveseek/src/transport/mod.rs",
    "grooveseek/src/transport/stdio.rs",
    "grooveseek/src/transport/webui_index.html",
    "grooveseek/src/tune.rs",
    "grooveseek/src/tune/grid.rs",
    "grooveseek/src/tune/report.rs",
    "grooveseek/src/tune/stats.rs",
    "grooveseek/src/watcher.rs",
];

/// The rows #214 added, by the path each names.
const ADDED_BY_214: &[&str] = &[
    "grooveseek/src/legacy.rs",
    "grooveseek/src/server/documents.rs",
    "grooveseek/src/server/kb_uri.rs",
    "grooveseek/src/server/search.rs",
];

fn frozen() -> Vec<(&'static str, Table)> {
    [
        ("ARCHITECTURE-739210b.md", PAGE_AT_739210B),
        ("ARCHITECTURE-739210b.ja.md", PAGE_AT_739210B_JA),
    ]
    .iter()
    .zip(PAGES)
    .map(|((name, page), (_, section))| {
        let table =
            layout_rows(page, section).unwrap_or_else(|| panic!("{name} has no `## {section}`"));
        assert!(
            table.unreadable.is_empty(),
            "{name}: {:?}",
            table.unreadable
        );
        assert!(table.ragged.is_empty(), "{name}: {:?}", table.ragged);
        (*name, table)
    })
    .collect()
}

#[test]
fn the_four_modules_pr_214_added_are_exactly_what_the_table_before_it_lacked() {
    let tree = tree(TREE_AT_739210B);
    // The literal tree is what makes the expectation below mean something:
    // it has to hold the file that is not `.rs` and the file 1.1.0 removes.
    for required in [
        "grooveseek/src/transport/webui_index.html",
        "grooveseek/src/legacy.rs",
    ] {
        assert!(tree.contains(required), "TREE_AT_739210B lost {required}");
    }
    for (name, table) in frozen() {
        let d = drift(&table, &members(MEMBERS_AT_739210B), &tree);
        assert_eq!(d.missing, paths(ADDED_BY_214), "{name}");
        assert!(d.stale.is_empty(), "{name}: {:?}", d.stale);
        assert!(d.empty.is_empty(), "{name}: {:?}", d.empty);
        assert!(d.duplicate.is_empty(), "{name}: {:?}", d.duplicate);
    }
}

#[test]
fn a_row_left_behind_by_a_deleted_file_is_reported_as_stale() {
    // The frozen pages have no stale row, so on their own they say nothing
    // about that direction. Take one file out of the tree and its row, at line
    // 31 of both pages, has to be the one thing reported.
    let mut tree = tree(TREE_AT_739210B);
    assert!(tree.remove("grooveseek/src/poison.rs"));
    for (name, table) in frozen() {
        let d = drift(&table, &members(MEMBERS_AT_739210B), &tree);
        assert_eq!(
            d.stale,
            vec![(31, "grooveseek/src/poison.rs".to_string())],
            "{name}"
        );
        assert_eq!(d.missing, paths(ADDED_BY_214), "{name}");
    }
}
