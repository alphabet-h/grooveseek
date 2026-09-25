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
use super::embed_mock::{
    EmbedMock, MockReply, MockResponse, Recorded, default_response, hermetic, openai_config_toml,
};
use super::mcp::grooveseek_bin;
use super::temp::TempKbLayout;

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

    /// `groove index --force`, whatever the exit code.
    pub fn index_force(&self) -> Output {
        self.cmd()
            .args(["index", "--force", "--kb-path"])
            .arg(self.kb())
            .output()
            .expect("spawn groove index --force")
    }

    fn status_count(&self, label: &str) -> u64 {
        let out = self
            .cmd()
            .args(["status", "--kb-path"])
            .arg(self.kb())
            .output()
            .expect("spawn groove status");
        assert!(
            out.status.success(),
            "groove status failed:\n{}",
            stderr_of(&out)
        );
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .find_map(|l| l.strip_prefix(label))
            .and_then(|n| n.trim().parse().ok())
            .unwrap_or_else(|| panic!("no {label} line in groove status"))
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

    /// `groove search <query> --format json`, which must succeed.
    pub fn search_json(&self, query: &str) -> serde_json::Value {
        let out = self
            .cmd()
            .args(["search", query, "--kb-path"])
            .arg(self.kb())
            .args([
                "--format",
                "json",
                "--limit",
                "5",
                "--include-low-quality",
                "--min-confidence-ratio",
                "0",
            ])
            .output()
            .expect("spawn groove search");
        assert!(
            out.status.success(),
            "groove search failed:\n{}",
            stderr_of(&out)
        );
        serde_json::from_slice(&out.stdout).expect("search stdout is JSON")
    }
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

/// A Markdown note with one section.
pub fn note(title: &str, body: &str) -> String {
    format!("---\ntitle: {title}\n---\n\n## {title}\n\n{body}\n")
}

/// Borrow `(rel, body)` pairs the way [`fixture`] takes them.
pub fn files<'a>(notes: &'a [(&'static str, String)]) -> Vec<(&'static str, &'a str)> {
    notes.iter().map(|(r, b)| (*r, b.as_str())).collect()
}
