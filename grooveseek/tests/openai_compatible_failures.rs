//! AW-04: what `groove index`, `groove search`, MCP `rebuild_index` and the
//! watcher do when an OpenAI-compatible endpoint refuses or fails.
//!
//! Every test drives the real `groove` binary against the in-process mock
//! ([`crate::common::embed_mock`]) through the shared fixture
//! ([`crate::common::embed_cli`]), under
//! [`crate::common::embed_mock::hermetic`], so a runner's proxy never sees a
//! request and nothing is downloaded. Waits are kept short by answering
//! `Retry-After: 0`, or 1 where the wait itself is what is checked.

mod common;

use common::ansi::strip_ansi;
use common::embed_cli::{
    BODY_SENTINEL, REJECT_MARKER, files, fixture, note, rejects_marker, reply, stderr_of,
};
use common::embed_mock::{EmbedMock, assert_dir_empty, hermetic, wait_until};
use common::mcp::spawn_serve_with;

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

/// The mock writes the headers a reply responder returns.
///
/// Red if `serve_one` drops [`MockReply::headers`].
#[test]
fn embed_mock_sends_the_headers_a_reply_responder_returns() {
    let mock = EmbedMock::with_reply_responder(|_| reply(429, &[("Retry-After", "7")]));
    let mut stream = TcpStream::connect(mock.addr()).expect("connect");
    let body = r#"{"model":"m","input":["x"]}"#;
    write!(
        stream,
        "POST /v1/embeddings HTTP/1.1\r\nHost: {}\r\nContent-Length: {}\r\n\r\n{body}",
        mock.addr(),
        body.len()
    )
    .expect("write request");
    let mut answer = String::new();
    stream.read_to_string(&mut answer).expect("read answer");
    assert!(answer.starts_with("HTTP/1.1 429 "), "{answer}");
    assert!(answer.contains("\r\nRetry-After: 7\r\n"), "{answer}");
    assert!(answer.contains(BODY_SENTINEL), "{answer}");
}

const ALPHA: &str = "The lighthouse keeper logs every passing ship before dawn.";
const BETA: &str = "A tokio runtime worker thread must not block on network input.";
const GAMMA: &str = "Sourdough needs a long cold proof to develop its flavour.";

fn three_notes() -> [(&'static str, String); 3] {
    [
        ("alpha.md", note("Alpha", ALPHA)),
        ("beta.md", note("Beta", BETA)),
        ("gamma.md", note("Gamma", GAMMA)),
    ]
}

/// A file whose input the endpoint refuses (400, 413 or 422) is skipped on
/// its own: the run goes on, exits 0, counts one skip, names the file on
/// stderr without the response body, and the index keeps the row it had.
///
/// Red if `index_single_disk_entry` returns the embed error instead of a skip
/// (the run exits non-zero), or if the warning prints the error's text (the
/// sentinel from the body appears).
#[test]
fn index_skips_only_the_file_whose_input_the_endpoint_rejects() {
    for status in [400u16, 413, 422] {
        let notes = three_notes();
        let fx = fixture(&format!("groove-aw04-skip-{status}"), &files(&notes), "");
        fx.index();
        let documents = fx.documents();

        fx.layout.write(
            "alpha.md",
            &note("Alpha", &format!("{ALPHA} {REJECT_MARKER}")),
        );
        fx.layout
            .write("beta.md", &note("Beta", &format!("{BETA} Edited.")));
        fx.answer_with(rejects_marker(status));
        let out = fx.run_index();
        let stderr = stderr_of(&out);
        assert!(
            out.status.success(),
            "HTTP {status}: the run must go on:\n{stderr}"
        );
        assert!(
            stderr.contains(&format!(
                "warning: alpha.md: embedding endpoint rejected the input (HTTP {status}); \
                 skipped, the index keeps what it had for this file"
            )),
            "HTTP {status}:\n{stderr}"
        );
        assert!(
            stderr.contains("1 updated") && stderr.contains("1 skipped"),
            "{stderr}"
        );
        assert!(
            !stderr.contains(BODY_SENTINEL),
            "the response body reached stderr:\n{stderr}"
        );

        fx.answer_normally();
        assert_eq!(fx.documents(), documents, "HTTP {status}: a row was lost");
        let hit = fx.search_json("lighthouse keeper ship");
        assert_eq!(
            hit.pointer("/results/0/path").and_then(|v| v.as_str()),
            Some("alpha.md"),
            "the previous alpha.md row must still be served: {hit}"
        );
        assert_dir_empty(&fx.cache);
    }
}

/// `--force` probes before it empties the index, and a probe the endpoint
/// refuses is fatal, not a skip: the fixed probe text cannot be too long, so a
/// refusal of it is a configuration fault (spec 3.1).
///
/// Red if the probe's rejection were turned into a skip: the reset would run
/// and the document count would drop.
#[test]
fn index_force_stops_when_the_probe_itself_is_rejected() {
    let notes = three_notes();
    let fx = fixture("groove-aw04-probe", &files(&notes), "");
    fx.index();
    let documents = fx.documents();
    fx.answer_with(|_| reply(413, &[]));
    let out = fx.index_force();
    fx.answer_normally();
    let stderr = stderr_of(&out);
    assert!(!out.status.success(), "{stderr}");
    assert!(
        stderr.contains("nothing was removed from the index"),
        "{stderr}"
    );
    assert_eq!(fx.documents(), documents);
    assert_dir_empty(&fx.cache);
}

/// Review Focus 2: a file longer than one batch (64 chunks) whose second
/// batch is refused is skipped whole. The first batch's vectors are thrown
/// away, not written, and the previous row keeps its chunk count.
///
/// `short.md` is edited in the same run without the marker, so the run embeds
/// one file and stays an ordinary run once Task 7 fails runs that embedded
/// nothing (preflight P1). It keeps one chunk, so the counts compared below
/// do not move because of it.
///
/// Red if a partial write lands (the chunk count changes) or if the refused
/// second batch stops the run.
#[test]
fn index_skips_a_file_whose_second_batch_is_rejected_without_writing_the_first() {
    let sections = |last: &str| -> String {
        let mut s = String::from("---\ntitle: Long\n---\n\n");
        for i in 0..69 {
            s.push_str(&format!(
                "## Section {i}\n\nParagraph number {i} talks about harbour cranes and tides.\n\n"
            ));
        }
        s.push_str(&format!("## Section 69\n\n{last}\n"));
        s
    };
    let long = sections("The final paragraph talks about harbour cranes too.");
    let short = note(
        "Short",
        "A single paragraph about the harbour master's logbook.",
    );
    let fx = fixture(
        "groove-aw04-batches",
        &[("long.md", long.as_str()), ("short.md", short.as_str())],
        "",
    );
    fx.index();
    let (documents, chunks) = (fx.documents(), fx.chunks());
    assert!(
        chunks > 65,
        "long.md must need two batches, got {chunks} chunks in all"
    );

    let before = fx.mock.requests().len();
    fx.layout.write(
        "long.md",
        &sections(&format!("The final paragraph talks about {REJECT_MARKER}.")),
    );
    fx.layout.write(
        "short.md",
        &note(
            "Short",
            "A single paragraph about the harbour master's new logbook.",
        ),
    );
    fx.answer_with(rejects_marker(413));
    let out = fx.run_index();
    fx.answer_normally();
    let stderr = stderr_of(&out);
    assert!(out.status.success(), "{stderr}");
    assert!(
        stderr.contains("1 updated") && stderr.contains("1 skipped"),
        "{stderr}"
    );
    let sent = fx.requests_since(before);
    assert!(
        sent.iter().any(|r| r.inputs().len() == 64),
        "the first batch must have been sent (and answered) before the refusal"
    );
    assert_eq!(
        (fx.documents(), fx.chunks()),
        (documents, chunks),
        "a partial write landed"
    );
    assert_dir_empty(&fx.cache);
}

/// Review Focus 5: the watcher meets the same refusal one file at a time. It
/// reports the skip, keeps serving, and does not panic.
///
/// Red if the watcher's reindex returns the embed error (`watcher: reindex ...
/// failed`) instead of the skip.
#[test]
fn the_watcher_reports_a_rejected_file_as_skipped() {
    let notes = three_notes();
    let fx = fixture(
        "groove-aw04-watch",
        &files(&notes),
        "\n[watch]\nenabled = true\ndebounce_ms = 300\n",
    );
    fx.index();
    fx.answer_with(rejects_marker(413));
    let (guard, _base) = spawn_serve_with(fx.kb(), &fx.config, true, |c| {
        hermetic(c, &fx.cache);
    });
    fx.layout.write(
        "fresh.md",
        &note("Fresh", &format!("Brand new text {REJECT_MARKER}.")),
    );
    let lines = || -> Vec<String> {
        guard
            .stderr()
            .lines()
            .iter()
            .map(|l| strip_ansi(l))
            .collect()
    };
    let skipped = wait_until(Duration::from_secs(30), || {
        lines().iter().any(|l| {
            l.contains("watcher: skipped fresh.md (embedding endpoint rejected the input)")
        })
    });
    let all = lines();
    assert!(
        !all.iter().any(|l| l.contains("panicked")),
        "{}",
        all.join("\n")
    );
    assert!(skipped, "no watcher skip line:\n{}", all.join("\n"));
    assert!(
        all.iter()
            .any(|l| l
                .contains("warning: fresh.md: embedding endpoint rejected the input (HTTP 413)")),
        "{}",
        all.join("\n")
    );
    assert!(
        !all.iter().any(|l| l.contains(BODY_SENTINEL)),
        "{}",
        all.join("\n")
    );
    drop(guard);
    assert_dir_empty(&fx.cache);
}
