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
    BODY_SENTINEL, DIM, REJECT_MARKER, files, fixture, note, rejects_marker, reply, stderr_of,
};
use common::embed_mock::{
    DOC_MODEL, EmbedMock, MockReply, QUERY_MODEL, assert_dir_empty, default_response, hermetic,
    wait_until,
};
use common::mcp::{mcp_initialize, mcp_tool_call, spawn_serve_with};

use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

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

/// A rename whose old path has no row makes the watcher index the new path as
/// a new file. When the endpoint refuses that file, no row is created, so the
/// watcher must say the new path was refused, not that it was indexed.
///
/// `pending.md` is written after the index was built and carries the marker,
/// so it never gets a row, whether or not the daemon looks at it on startup.
///
/// Red if `rename_single_file` maps the rejection skip to `OldPathMissing`
/// (`watcher: rename target pending.md not in DB, indexed moved.md`).
#[test]
fn the_watcher_reports_a_rejected_rename_target_as_refused_not_indexed() {
    let notes = three_notes();
    let fx = fixture(
        "groove-aw04-watch-rename",
        &files(&notes),
        "\n[watch]\nenabled = true\ndebounce_ms = 300\n",
    );
    fx.index();
    fx.layout.write(
        "pending.md",
        &note("Pending", &format!("Text still waiting {REJECT_MARKER}.")),
    );
    fx.answer_with(rejects_marker(413));
    let (guard, _base) = spawn_serve_with(fx.kb(), &fx.config, true, |c| {
        hermetic(c, &fx.cache);
    });
    std::fs::rename(fx.kb().join("pending.md"), fx.kb().join("moved.md"))
        .expect("rename pending.md");
    let lines = || -> Vec<String> {
        guard
            .stderr()
            .lines()
            .iter()
            .map(|l| strip_ansi(l))
            .collect()
    };
    let reported = wait_until(Duration::from_secs(30), || {
        lines()
            .iter()
            .any(|l| l.contains("watcher: rename target pending.md not in DB"))
    });
    let all = lines();
    assert!(reported, "no watcher rename line:\n{}", all.join("\n"));
    assert!(
        all.iter()
            .any(|l| l
                .contains("watcher: rename target pending.md not in DB, and moved.md was refused")),
        "{}",
        all.join("\n")
    );
    assert!(
        !all.iter().any(|l| l.contains("indexed moved.md")),
        "{}",
        all.join("\n")
    );
    assert!(
        all.iter()
            .any(|l| l
                .contains("warning: moved.md: embedding endpoint rejected the input (HTTP 413)")),
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

fn one_note() -> [(&'static str, String); 1] {
    [(
        "only.md",
        note("Only", "The ferry leaves at noon from the western pier."),
    )]
}

/// A 429 with `Retry-After: 2` is waited out and the batch sent again.
/// The exact wait is pinned by the `retry_loop` unit tests in `embedder.rs`;
/// this pins that the binary retries at all and honours the header.
///
/// Red if the provider stops retrying (exit non-zero) or ignores the header.
/// Without it, the first retry waits the 1 s backoff plus under 0.25 s of
/// jitter, which stays below the 2 s this checks for.
#[test]
fn index_waits_out_a_429_retry_after_and_then_succeeds() {
    let notes = one_note();
    let fx = fixture("groove-aw04-429", &files(&notes), "");
    let first = Arc::new(Mutex::new(true));
    {
        let first = first.clone();
        fx.answer_with(move |req| {
            let mut first = first.lock().expect("first lock");
            if std::mem::replace(&mut *first, false) {
                reply(429, &[("Retry-After", "2")])
            } else {
                MockReply::plain(default_response(req, DIM))
            }
        });
    }
    let started = Instant::now();
    let out = fx.run_index();
    let took = started.elapsed();
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert_eq!(fx.mock.requests().len(), 2, "one 429, one success");
    assert!(
        took >= Duration::from_secs(2),
        "Retry-After: 2 was not waited: {took:?}"
    );
    assert_dir_empty(&fx.cache);
}

/// A 503 that never clears stops the run after `1 + max_retries` attempts
/// and says how many; `max_retries = 0` sends once and says nothing about
/// attempts. `Retry-After: 0` keeps the test from sleeping.
///
/// Red on an off-by-one in the loop, or if `max_retries` from the config
/// never reaches the provider.
#[test]
fn index_gives_up_after_one_plus_max_retries_attempts_on_a_persistent_503() {
    for (extra, attempts) in [("", 4usize), ("max_retries = 0\n", 1)] {
        let notes = one_note();
        let fx = fixture("groove-aw04-503", &files(&notes), extra);
        fx.answer_with(|_| reply(503, &[("Retry-After", "0")]));
        let out = fx.run_index();
        let stderr = stderr_of(&out);
        assert!(!out.status.success(), "{extra:?}: {stderr}");
        assert_eq!(fx.mock.requests().len(), attempts, "{extra:?}: {stderr}");
        assert!(stderr.contains("HTTP 503"), "{stderr}");
        if attempts == 4 {
            assert!(
                stderr.contains("still failing after 4 attempts (last: HTTP 503)"),
                "{stderr}"
            );
        } else {
            assert!(!stderr.contains("attempts"), "{stderr}");
        }
        assert_dir_empty(&fx.cache);
    }
}

/// A 401 is a wrong key, not a busy server: one request, then the run stops.
///
/// Red if 401 is classified as retryable (four requests).
#[test]
fn index_does_not_retry_a_401() {
    let notes = one_note();
    let fx = fixture("groove-aw04-401", &files(&notes), "");
    fx.answer_with(|_| reply(401, &[("Retry-After", "0")]));
    let out = fx.run_index();
    let stderr = stderr_of(&out);
    assert!(!out.status.success(), "{stderr}");
    assert_eq!(fx.mock.requests().len(), 1, "{stderr}");
    assert!(stderr.contains("HTTP 401"), "{stderr}");
    assert_dir_empty(&fx.cache);
}

/// Inputs longer than `max_input_chars` arrive cut to it, on a character
/// boundary, for ASCII and for multi-byte text alike; a configured value
/// replaces the default 8000.
///
/// Red if the provider stops cutting, or never receives the configured value.
#[test]
fn index_sends_long_inputs_cut_to_max_input_chars() {
    for (extra, limit) in [("", 8000usize), ("max_input_chars = 100\n", 100)] {
        let ascii = note("Ascii", &"x".repeat(9000));
        let kana = note("Kana", &"あ".repeat(9000));
        let fx = fixture(
            "groove-aw04-cut",
            &[("ascii.md", ascii.as_str()), ("kana.md", kana.as_str())],
            extra,
        );
        fx.index();
        let inputs: Vec<String> = fx
            .mock
            .requests()
            .iter()
            .filter(|r| r.model() == Some(DOC_MODEL))
            .flat_map(|r| r.inputs())
            .collect();
        assert!(
            inputs.iter().all(|i| i.chars().count() <= limit),
            "{extra:?}"
        );
        for ch in ['x', 'あ'] {
            assert!(
                inputs
                    .iter()
                    .any(|i| i.chars().count() == limit && i.ends_with(ch)),
                "{extra:?}: no input of {limit} chars ending in {ch:?}; lengths {:?}",
                inputs.iter().map(|i| i.chars().count()).collect::<Vec<_>>()
            );
        }
        assert_dir_empty(&fx.cache);
    }
}

/// A `Retry-After` over 60 s is not waited for: the run stops at once and
/// says what the server asked for.
///
/// Red if the cap is dropped: the run would then wait 120 s before each of
/// its three retries, about 360 s in all, and fail the 30 s bound only once
/// it ends.
#[test]
fn index_stops_without_waiting_when_retry_after_exceeds_sixty_seconds() {
    let notes = one_note();
    let fx = fixture("groove-aw04-120", &files(&notes), "");
    fx.answer_with(|_| reply(429, &[("Retry-After", "120")]));
    let started = Instant::now();
    let out = fx.run_index();
    let took = started.elapsed();
    let stderr = stderr_of(&out);
    assert!(!out.status.success(), "{stderr}");
    assert!(took < Duration::from_secs(30), "waited {took:?}: {stderr}");
    assert_eq!(fx.mock.requests().len(), 1, "{stderr}");
    assert!(stderr.contains("asked to retry after 120 s"), "{stderr}");
    assert_dir_empty(&fx.cache);
}

/// Review Focus 3: the query side is cut too. A 9000-character query reaches
/// the endpoint under `query_model` as 8000 characters.
///
/// Red if truncation is applied to documents only.
#[test]
fn search_sends_a_long_query_cut_to_max_input_chars() {
    let notes = one_note();
    let fx = fixture("groove-aw04-query", &files(&notes), "");
    fx.index();
    let before = fx.mock.requests().len();
    fx.search_json(&"q".repeat(9000));
    let queries: Vec<String> = fx
        .requests_since(before)
        .iter()
        .filter(|r| r.model() == Some(QUERY_MODEL))
        .flat_map(|r| r.inputs())
        .collect();
    assert_eq!(queries.len(), 1, "one query embed");
    assert_eq!(queries[0].chars().count(), 8000);
    assert_dir_empty(&fx.cache);
}

const ALL_REJECTED: &str = "the embedding endpoint rejected every input this run tried to embed";

/// Every file the run tried to embed was refused and none was embedded: the
/// run still finishes its deletions and bookkeeping, then exits non-zero with
/// a message that points at the configuration.
///
/// Red if the check is missing (exit 0), or if it fails the run before the
/// deletion sweep (gamma.md's row would survive).
#[test]
fn index_fails_after_its_sweep_when_the_endpoint_rejected_every_input_it_tried() {
    let notes = three_notes();
    let fx = fixture("groove-aw04-all", &files(&notes), "");
    fx.index();
    assert_eq!(fx.documents(), 3);

    std::fs::remove_file(fx.kb().join("gamma.md")).expect("remove gamma.md");
    fx.layout
        .write("alpha.md", &note("Alpha", &format!("{ALPHA} Edited.")));
    fx.layout
        .write("beta.md", &note("Beta", &format!("{BETA} Edited.")));
    fx.answer_with(|_| reply(413, &[]));
    let out = fx.run_index();
    fx.answer_normally();
    let stderr = stderr_of(&out);
    assert!(!out.status.success(), "{stderr}");
    assert!(stderr.contains(ALL_REJECTED), "{stderr}");
    assert!(
        stderr.contains("Done in ") && stderr.contains("1 deleted, 2 skipped"),
        "{stderr}"
    );
    assert!(!stderr.contains(BODY_SENTINEL), "{stderr}");
    assert_eq!(fx.documents(), 2, "the sweep must have removed gamma.md");
    assert_dir_empty(&fx.cache);
}

/// One file refused while another was embedded in the same run is an
/// ordinary run: exit 0.
///
/// Red if the success count misses real embeds (it would read zero and fail
/// the run).
#[test]
fn index_exits_zero_when_one_file_is_rejected_and_another_is_embedded() {
    let notes = three_notes();
    let fx = fixture("groove-aw04-one", &files(&notes), "");
    fx.index();
    fx.layout.write(
        "alpha.md",
        &note("Alpha", &format!("{ALPHA} {REJECT_MARKER}")),
    );
    fx.layout
        .write("beta.md", &note("Beta", &format!("{BETA} Edited.")));
    fx.answer_with(rejects_marker(413));
    let out = fx.run_index();
    let stderr = stderr_of(&out);
    assert!(out.status.success(), "{stderr}");
    assert!(stderr.contains("1 skipped"), "{stderr}");
    assert!(!stderr.contains(ALL_REJECTED), "{stderr}");
    assert_dir_empty(&fx.cache);
}

/// MCP `rebuild_index` answers the same run with an `error` beside its
/// counts, and without the endpoint's response body.
///
/// Red if the server skips the check the CLI makes.
#[test]
fn mcp_rebuild_index_reports_an_error_when_every_input_was_rejected() {
    let notes = three_notes();
    let fx = fixture("groove-aw04-mcp", &files(&notes), "");
    fx.index();
    std::fs::remove_file(fx.kb().join("gamma.md")).expect("remove gamma.md");
    fx.layout
        .write("alpha.md", &note("Alpha", &format!("{ALPHA} Edited.")));
    let (guard, base) = spawn_serve_with(fx.kb(), &fx.config, false, |c| {
        hermetic(c, &fx.cache);
    });
    let session = mcp_initialize(&base);
    fx.answer_with(|_| reply(413, &[]));
    let resp = mcp_tool_call(&base, &session, "rebuild_index", serde_json::json!({}));
    fx.answer_normally();
    drop(guard);
    let error = resp.get("error").and_then(|v| v.as_str()).unwrap_or("");
    assert!(error.contains(ALL_REJECTED), "{resp}");
    assert!(!resp.to_string().contains(BODY_SENTINEL), "{resp}");
    assert_eq!(
        resp.get("deleted").and_then(|v| v.as_u64()),
        Some(1),
        "{resp}"
    );
    assert_eq!(
        resp.get("skipped").and_then(|v| v.as_u64()),
        Some(1),
        "{resp}"
    );
    assert_dir_empty(&fx.cache);
}
