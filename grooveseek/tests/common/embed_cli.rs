//! A knowledge base, an [`crate::common::embed_mock`] endpoint whose answer a
//! test can swap mid-run (headers included), and the `groove` CLI calls the
//! AW-04 failure tests make against it.
//!
//! One copy, for every test that drives the binary through an endpoint that
//! refuses or fails (`AGENTS.md`, "One question gets one implementation").
//! `tests/openai_compatible_provider.rs` (AW-03 / AW-06) still carries its
//! own older `Fixture` with a body-only failure slot: it is left as it is
//! because this repository does not edit existing tests. A new test that needs
//! the same thing uses this module rather than copying either.

use super::ansi::strip_ansi;
use super::cli::{run_status_count, search_json};
use super::embed_mock::{
    EmbedMock, Framing, MockReply, MockResponse, Recorded, default_response, hermetic,
    openai_config_toml,
};
use super::mcp::{ServerGuard, grooveseek_bin, spawn_serve_with};
use super::temp::TempKbLayout;
use grooveseek::db::Database;

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::{Arc, Mutex};

/// The mock's vector length.
pub const DIM: usize = 64;
/// A word only the inputs the mock is told to refuse carry.
pub const REJECT_MARKER: &str = "quasarrefusal";
/// Put in every failing response body: it must never reach stderr or an MCP
/// reply.
pub const BODY_SENTINEL: &str = "SENTINEL-AW04-7c2e";

type Replier = Arc<dyn Fn(&Recorded) -> MockReply + Send + Sync>;

/// A knowledge base, a mock endpoint whose answer a test can swap, and a
/// `groove.toml` pointing at it. Fields drop in declaration order: the mock
/// first, the directories last.
pub struct Fixture {
    pub mock: EmbedMock,
    replier: Arc<Mutex<Option<Replier>>>,
    pub config: PathBuf,
    /// The `FASTEMBED_CACHE_DIR` the child gets; it must stay empty.
    pub cache: PathBuf,
    pub layout: TempKbLayout,
}

/// Write `files` (`(relative path, body)`) into a fresh knowledge base and
/// point a `groove.toml` at a new mock. `extra_toml` is appended after the
/// `[embedding]` keys, so a line such as `max_retries = 0\n` lands in that
/// section.
pub fn fixture(prefix: &str, files: &[(&str, &str)], extra_toml: &str) -> Fixture {
    let layout = TempKbLayout::new(prefix);
    for (rel, body) in files {
        layout.write(rel, body);
    }
    let replier: Arc<Mutex<Option<Replier>>> = Arc::new(Mutex::new(None));
    let mock = {
        let replier = replier.clone();
        EmbedMock::with_reply_responder(move |req| {
            let current = replier.lock().expect("replier lock").clone();
            match current {
                Some(answer) => answer(req),
                None => MockReply::plain(default_response(req, DIM)),
            }
        })
    };
    let config = layout.root().join("groove.toml");
    std::fs::write(
        &config,
        openai_config_toml(&mock.endpoint(), None, DIM) + extra_toml,
    )
    .expect("write groove.toml");
    let cache = layout.root().join("fastembed-tripwire");
    std::fs::create_dir_all(&cache).expect("create tripwire dir");
    Fixture {
        mock,
        replier,
        config,
        cache,
        layout,
    }
}

impl Fixture {
    pub fn kb(&self) -> &Path {
        self.layout.kb()
    }

    /// Answer every later request with `answer`.
    pub fn answer_with(&self, answer: impl Fn(&Recorded) -> MockReply + Send + Sync + 'static) {
        *self.replier.lock().expect("replier lock") = Some(Arc::new(answer));
    }

    /// Go back to [`default_response`].
    pub fn answer_normally(&self) {
        *self.replier.lock().expect("replier lock") = None;
    }

    /// `groove --config <cfg>` under the pinned environment
    /// ([`crate::common::embed_mock::hermetic`]); the caller adds the subcommand.
    pub fn cmd(&self) -> Command {
        let mut cmd = Command::new(grooveseek_bin());
        cmd.arg("--config").arg(&self.config);
        hermetic(&mut cmd, &self.cache);
        cmd
    }

    /// `groove index`, whatever the exit code.
    pub fn run_index(&self) -> Output {
        self.cmd()
            .args(["index", "--kb-path"])
            .arg(self.kb())
            .output()
            .expect("spawn groove index")
    }

    /// `groove index`, which must succeed.
    pub fn index(&self) {
        let out = self.run_index();
        assert!(
            out.status.success(),
            "groove index failed:\n{}",
            stderr_of(&out)
        );
    }

    /// `groove serve` over this knowledge base with the watcher on, under the
    /// pinned environment. Returns only after the server printed
    /// `watcher: watching`, so an edit the caller makes next cannot fall
    /// before the debouncer is armed. The fixture's `groove.toml` has to turn
    /// the watcher on itself; [`served_watching`] builds one that does.
    ///
    /// Dropping the guard kills the server, which releases the writer lease,
    /// so a `groove index` run after that is not refused.
    pub fn serve_watching(&self) -> ServerGuard {
        let (guard, _base) = spawn_serve_with(self.kb(), &self.config, true, |c| {
            hermetic(c, &self.cache);
        });
        guard
    }

    /// `groove index --force`, whatever the exit code.
    pub fn index_force(&self) -> Output {
        self.cmd()
            .args(["index", "--force", "--kb-path"])
            .arg(self.kb())
            .output()
            .expect("spawn groove index --force")
    }

    /// [`crate::common::cli::run_status_count`] under this fixture's environment.
    fn status_count(&self, label: &str) -> u64 {
        run_status_count(self.cmd(), self.kb(), label)
    }

    /// `Documents:` from `groove status`.
    pub fn documents(&self) -> u64 {
        self.status_count("Documents:")
    }

    /// `Chunks:` from `groove status`.
    pub fn chunks(&self) -> u64 {
        self.status_count("Chunks:")
    }

    /// Requests recorded after the first `before`.
    pub fn requests_since(&self, before: usize) -> Vec<Recorded> {
        self.mock.requests().split_off(before)
    }

    /// `groove search <query> --format json`, which must succeed
    /// ([`crate::common::cli::search_json`] under this fixture's environment, at most five
    /// hits, the quality filter and the confidence cutoff off).
    pub fn search_json(&self, query: &str) -> serde_json::Value {
        search_json(
            self.cmd(),
            self.kb(),
            query,
            &[
                "--limit",
                "5",
                "--include-low-quality",
                "--min-confidence-ratio",
                "0",
            ],
        )
    }
}

/// The `[watch]` section [`served_watching`] appends: the watcher on, with a
/// debounce short enough for a test to wait out.
pub const WATCH_TOML: &str = "\n[watch]\nenabled = true\ndebounce_ms = 300\n";

/// A knowledge base holding `files`, indexed once and served with the watcher
/// on ([`Fixture::serve_watching`]).
///
/// The fixture the watcher tests' helpers share: those helpers keep their old
/// names and delegate to it (or [`served_watching_after`]), and a
/// helper that takes a fixture the test built itself calls
/// [`Fixture::index`] then [`Fixture::serve_watching`]. So does a helper that
/// builds its own fixture because it needs more (other keys, a shorter
/// timeout, an answer swapped once the server runs): it keeps those steps and
/// starts the server with [`Fixture::serve_watching`]. What still starts its
/// own server is a test body that does something this does not: appends other keys before
/// [`WATCH_TOML`], changes the knowledge base or the endpoint's answer
/// between the index and the server, uses another debounce, or builds the
/// older fixture the OpenAI-compatible provider tests keep. One exception: the
/// body of the case-only directory rename test does the same setup this does
/// (a fixture, an index, a server with the watcher on) inline; test bodies
/// are not edited, so it stays. New code uses this. (Reading the server's
/// stderr goes through [`ServerGuard::stderr`] then
/// [`crate::common::mcp::StderrLog::stripped_lines`]; a few test bodies still
/// strip inline, for the same reason.)
pub fn served_watching(prefix: &str, files: &[(&str, &str)]) -> (Fixture, ServerGuard) {
    served_watching_after(prefix, files, |_| {})
}

/// [`served_watching`], with `between` run on the knowledge base after the
/// index and before the server starts: what it changes reaches the watcher as
/// no event.
pub fn served_watching_after(
    prefix: &str,
    files: &[(&str, &str)],
    between: impl FnOnce(&Fixture),
) -> (Fixture, ServerGuard) {
    let fx = fixture(prefix, files, WATCH_TOML);
    fx.index();
    between(&fx);
    let guard = fx.serve_watching();
    (fx, guard)
}

/// A reply of `status` carrying [`BODY_SENTINEL`] and `headers`.
pub fn reply(status: u16, headers: &[(&str, &str)]) -> MockReply {
    MockReply {
        response: MockResponse::json(
            status,
            &serde_json::json!({"error": {"message": format!("mock: refused {BODY_SENTINEL}")}}),
        ),
        headers: headers
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
        stall_body: false,
        truncate_body: false,
        framing: Framing::Exact,
    }
}

/// Refuse with `status` any request one of whose inputs holds
/// [`REJECT_MARKER`]; answer the rest normally.
pub fn rejects_marker(status: u16) -> impl Fn(&Recorded) -> MockReply + Send + Sync + 'static {
    move |req| {
        if req.inputs().iter().any(|i| i.contains(REJECT_MARKER)) {
            reply(status, &[])
        } else {
            MockReply::plain(default_response(req, DIM))
        }
    }
}

/// stderr with ANSI colours removed.
pub fn stderr_of(out: &Output) -> String {
    strip_ansi(&String::from_utf8_lossy(&out.stderr))
}

/// The `Done in` line of a `groove index` run's stderr.
pub fn done_line(out: &Output) -> String {
    stderr_of(out)
        .lines()
        .find(|l| l.contains("Done in"))
        .unwrap_or_else(|| panic!("no Done in line:\n{}", stderr_of(out)))
        .to_string()
}

/// `groove index --wait <secs>`, which must succeed; its `Done in` line.
///
/// For a run right after a writer was stopped: the operating system releases
/// the writer lease of a killed process a little after the process is reaped
/// (the writer lease CLI tests poll for it for the same reason), and a plain
/// `groove index` that arrives first is refused rather than kept waiting.
pub fn index_ok_waiting(fx: &Fixture, secs: u64) -> String {
    let out = fx
        .cmd()
        .args(["index", "--wait", &secs.to_string(), "--kb-path"])
        .arg(fx.kb())
        .output()
        .expect("spawn groove index --wait");
    assert!(
        out.status.success(),
        "groove index --wait {secs} failed:\n{}",
        stderr_of(&out)
    );
    done_line(&out)
}

/// `groove index`, which must succeed; its `Done in` line.
pub fn index_ok(fx: &Fixture) -> String {
    let out = fx.run_index();
    assert!(
        out.status.success(),
        "groove index failed:\n{}",
        stderr_of(&out)
    );
    done_line(&out)
}

/// `fx`'s index opened through a reader connection (`query_only`), so a read neither writes
/// nor takes the writer lease, even while a `groove serve` runs between two tool calls.
pub fn open_reader(fx: &Fixture) -> Database {
    let db_path = grooveseek::resolve_db_path(fx.kb());
    Database::open_read_only(&db_path.to_string_lossy(), fx.kb())
        .unwrap_or_else(|e| panic!("open the index as a reader: {e:#}"))
}

/// Every `(path, chunk texts joined by newlines)` in `fx`'s index, sorted by path, read
/// through [`open_reader`] so reading cannot be what changes the rows.
pub fn rows(fx: &Fixture) -> Vec<(String, String)> {
    let db = open_reader(fx);
    let mut paths = db.all_document_paths().expect("paths");
    paths.sort();
    paths
        .into_iter()
        .map(|p| {
            let text = db
                .chunk_texts_for_path(&p)
                .expect("chunk texts")
                .into_iter()
                .map(|(_, content)| content)
                .collect::<Vec<_>>()
                .join("\n");
            (p, text)
        })
        .collect()
}

/// A Markdown note with one section.
pub fn note(title: &str, body: &str) -> String {
    format!("---\ntitle: {title}\n---\n\n## {title}\n\n{body}\n")
}

/// Borrow `(rel, body)` pairs the way [`fixture`] takes them.
pub fn files<'a>(notes: &'a [(&'static str, String)]) -> Vec<(&'static str, &'a str)> {
    notes.iter().map(|(r, b)| (*r, b.as_str())).collect()
}
