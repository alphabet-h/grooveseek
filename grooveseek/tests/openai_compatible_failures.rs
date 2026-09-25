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
/// Red if [`grooveseek::indexer::rename_single_file`] maps the rejection skip to
/// [`grooveseek::indexer::RenameOutcome::OldPathMissing`]
/// (`watcher: rename target pending.md not in DB, indexed moved.md`).
///
/// Not every backend pairs a rename: macOS (FSEvents) can deliver it as a
/// removal and a creation, which the watcher handles as a plain reindex of
/// `moved.md` (`watcher: skipped moved.md (...)`). What must hold either way
/// is asserted on every OS; the rename line is checked only when it appeared.
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
    const RENAME_LINE: &str = "watcher: rename target pending.md not in DB";
    const REINDEX_LINE: &str = "watcher: skipped moved.md (embedding endpoint rejected the input)";
    let reported = wait_until(Duration::from_secs(30), || {
        lines()
            .iter()
            .any(|l| l.contains(RENAME_LINE) || l.contains(REINDEX_LINE))
    });
    let all = lines();
    drop(guard);
    fx.answer_normally();
    assert!(
        reported,
        "neither a rename nor a reindex line:\n{}",
        all.join("\n")
    );
    let has = |needle: &str| all.iter().any(|l| l.contains(needle));
    if has(RENAME_LINE) {
        // A paired rename (Linux, Windows).
        assert!(
            has("watcher: rename target pending.md not in DB, and moved.md was refused"),
            "{}",
            all.join("\n")
        );
    } else {
        // An unpaired one (macOS): the creation of moved.md is a plain reindex.
        assert!(has(REINDEX_LINE), "{}", all.join("\n"));
    }
    assert!(
        has(
            "warning: moved.md: embedding endpoint rejected the input (HTTP 413); skipped, \
             this file is not in the index"
        ),
        "{}",
        all.join("\n")
    );
    assert!(!has("indexed moved.md"), "{}", all.join("\n"));
    assert!(!has("keeps what it had"), "{}", all.join("\n"));
    assert!(!has(BODY_SENTINEL), "{}", all.join("\n"));
    assert_eq!(fx.documents(), 3, "moved.md must have no row");
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

/// A refusal whose body never arrives is still a refusal: the endpoint sends
/// 413 headers, promises more body than it writes and holds the connection.
/// The file is skipped on its own, the refused batch is sent once, and the
/// run goes on and exits 0.
///
/// Red if the status is classified only after the body is read: the body's
/// timeout is then retried as a transient failure, and the run stops with a
/// timeout after its retries.
#[test]
fn index_skips_a_file_whose_refusal_body_never_arrives() {
    let notes = three_notes();
    let fx = fixture("groove-aw04-stall", &files(&notes), "");
    // A one-second request timeout, so the stalled body fails fast.
    let config = std::fs::read_to_string(&fx.config).expect("read groove.toml");
    assert!(config.contains("timeout_seconds = 15\n"), "{config}");
    std::fs::write(
        &fx.config,
        config.replace("timeout_seconds = 15\n", "timeout_seconds = 1\n"),
    )
    .expect("write groove.toml");
    fx.index();

    fx.layout.write(
        "alpha.md",
        &note("Alpha", &format!("{ALPHA} {REJECT_MARKER}")),
    );
    fx.layout
        .write("beta.md", &note("Beta", &format!("{BETA} Edited.")));
    let before = fx.mock.requests().len();
    fx.answer_with(|req| {
        if req.inputs().iter().any(|i| i.contains(REJECT_MARKER)) {
            let mut stalled = reply(413, &[]);
            stalled.stall_body = true;
            stalled
        } else {
            MockReply::plain(default_response(req, DIM))
        }
    });
    let out = fx.run_index();
    fx.answer_normally();
    let stderr = stderr_of(&out);
    assert!(out.status.success(), "{stderr}");
    assert!(
        stderr.contains("warning: alpha.md: embedding endpoint rejected the input (HTTP 413)"),
        "{stderr}"
    );
    assert!(
        stderr.contains("1 updated") && stderr.contains("1 skipped"),
        "{stderr}"
    );
    assert!(!stderr.contains(BODY_SENTINEL), "{stderr}");
    let refused = fx
        .requests_since(before)
        .iter()
        .filter(|r| r.inputs().iter().any(|i| i.contains(REJECT_MARKER)))
        .count();
    assert_eq!(refused, 1, "a refusal is never retried: {stderr}");
    assert_dir_empty(&fx.cache);
}

/// A long file is the only file changed, and the endpoint accepts its first
/// batch and refuses the second. The file is skipped, but the accepted batch
/// shows the endpoint and model work, so the run is not "every input
/// rejected": exit 0.
///
/// Red if a refusal after an accepted batch counts toward the all-rejected
/// check (the run would exit non-zero).
#[test]
fn index_exits_zero_when_its_only_changed_file_is_refused_after_an_accepted_batch() {
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
        "groove-aw04-partial",
        &[("long.md", long.as_str()), ("short.md", short.as_str())],
        "",
    );
    fx.index();

    let before = fx.mock.requests().len();
    fx.layout.write(
        "long.md",
        &sections(&format!("The final paragraph talks about {REJECT_MARKER}.")),
    );
    fx.answer_with(rejects_marker(413));
    let out = fx.run_index();
    fx.answer_normally();
    let stderr = stderr_of(&out);
    assert!(out.status.success(), "{stderr}");
    assert!(!stderr.contains(ALL_REJECTED), "{stderr}");
    assert!(
        stderr.contains("warning: long.md: embedding endpoint rejected the input (HTTP 413)"),
        "{stderr}"
    );
    assert!(
        stderr.contains("0 updated") && stderr.contains("1 skipped"),
        "{stderr}"
    );
    assert!(
        fx.requests_since(before)
            .iter()
            .any(|r| r.inputs().len() == 64),
        "the first batch must have been sent (and answered) before the refusal"
    );
    assert_dir_empty(&fx.cache);
}

/// A note of 70 sections, which the indexer sends in two batches (64 + 6);
/// `last` is the body of the last section, so it lands in the second batch.
/// The same shape the two long-file tests above build inline.
fn two_batch_note(last: &str) -> String {
    let mut s = String::from("---\ntitle: Long\n---\n\n");
    for i in 0..69 {
        s.push_str(&format!(
            "## Section {i}\n\nParagraph number {i} talks about harbour cranes and tides.\n\n"
        ));
    }
    s.push_str(&format!("## Section 69\n\n{last}\n"));
    s
}

/// In one run a short file is refused on its first (only) batch and a long
/// file is refused only after its first batch was accepted; nothing else
/// changed. The accepted batch shows the endpoint works, so the run is not
/// "every input rejected" even though no file was embedded whole: exit 0.
///
/// Red if the all-rejected check reads only whole-file successes as proof the
/// endpoint accepted something (it would see one outright refusal, zero
/// embedded files, and fail the run).
#[test]
fn index_exits_zero_when_an_outright_refusal_meets_a_refusal_after_an_accepted_batch() {
    let long = two_batch_note("The final paragraph talks about harbour cranes too.");
    let short = note(
        "Short",
        "A single paragraph about the harbour master's logbook.",
    );
    let fx = fixture(
        "groove-aw04-mixed",
        &[("long.md", long.as_str()), ("short.md", short.as_str())],
        "",
    );
    fx.index();

    let before = fx.mock.requests().len();
    fx.layout.write(
        "long.md",
        &two_batch_note(&format!("The final paragraph talks about {REJECT_MARKER}.")),
    );
    fx.layout.write(
        "short.md",
        &note(
            "Short",
            &format!("A single paragraph about the harbour master's {REJECT_MARKER}."),
        ),
    );
    fx.answer_with(rejects_marker(413));
    let out = fx.run_index();
    fx.answer_normally();
    let stderr = stderr_of(&out);
    assert!(out.status.success(), "{stderr}");
    assert!(!stderr.contains(ALL_REJECTED), "{stderr}");
    assert!(
        stderr.contains("0 updated") && stderr.contains("2 skipped"),
        "{stderr}"
    );
    for file in ["long.md", "short.md"] {
        assert!(
            stderr.contains(&format!(
                "warning: {file}: embedding endpoint rejected the input (HTTP 413)"
            )),
            "{stderr}"
        );
    }
    assert!(
        fx.requests_since(before)
            .iter()
            .any(|r| r.inputs().len() == 64),
        "long.md's first batch must have been sent (and answered) before its refusal"
    );
    assert_dir_empty(&fx.cache);
}

const FORCED_REJECTED: &str = "during a forced rebuild";

/// `groove index --force` empties the index before it re-embeds, so a file the
/// endpoint refuses there is missing from the index, not kept. One such file
/// fails the run -- after every other file was indexed -- and its warning does
/// not claim the index kept anything.
///
/// Red if a forced run with one refusal exits 0 (the other two files were
/// embedded), or if the warning still says the index keeps what it had.
#[test]
fn index_force_fails_when_the_endpoint_rejects_one_file() {
    let notes = three_notes();
    let fx = fixture("groove-aw04-force-one", &files(&notes), "");
    fx.index();
    assert_eq!(fx.documents(), 3);

    fx.layout.write(
        "alpha.md",
        &note("Alpha", &format!("{ALPHA} {REJECT_MARKER}")),
    );
    fx.answer_with(rejects_marker(413));
    let out = fx.index_force();
    fx.answer_normally();
    let stderr = stderr_of(&out);
    assert!(!out.status.success(), "{stderr}");
    assert!(stderr.contains(FORCED_REJECTED), "{stderr}");
    assert!(
        stderr.contains("warning: alpha.md: embedding endpoint rejected the input (HTTP 413)"),
        "{stderr}"
    );
    assert!(!stderr.contains("keeps what it had"), "{stderr}");
    assert!(!stderr.contains(BODY_SENTINEL), "{stderr}");
    assert_eq!(fx.documents(), 2, "beta.md and gamma.md must be indexed");
    assert_dir_empty(&fx.cache);
}

/// MCP `rebuild_index {force: true}` answers the same run with an `error`
/// beside its counts, without the endpoint's response body.
///
/// Red if the server skips the forced-run check the CLI makes.
#[test]
fn mcp_rebuild_index_force_reports_an_error_when_one_file_is_rejected() {
    let notes = three_notes();
    let fx = fixture("groove-aw04-mcp-force", &files(&notes), "");
    fx.index();
    fx.layout.write(
        "alpha.md",
        &note("Alpha", &format!("{ALPHA} {REJECT_MARKER}")),
    );
    let (guard, base) = spawn_serve_with(fx.kb(), &fx.config, false, |c| {
        hermetic(c, &fx.cache);
    });
    let session = mcp_initialize(&base);
    fx.answer_with(rejects_marker(413));
    let resp = mcp_tool_call(
        &base,
        &session,
        "rebuild_index",
        serde_json::json!({"force": true}),
    );
    fx.answer_normally();
    drop(guard);
    let error = resp.get("error").and_then(|v| v.as_str()).unwrap_or("");
    assert!(error.contains(FORCED_REJECTED), "{resp}");
    assert!(!resp.to_string().contains(BODY_SENTINEL), "{resp}");
    assert_eq!(
        resp.get("skipped").and_then(|v| v.as_u64()),
        Some(1),
        "{resp}"
    );
    assert_eq!(
        resp.get("total_documents").and_then(|v| v.as_u64()),
        Some(2),
        "{resp}"
    );
    assert_dir_empty(&fx.cache);
}

/// `.txt` and `.md` are read by different parsers, so a rename between them is
/// re-parsed under the new one, and a reparse that does not end in an update
/// drops the row the old parser wrote.
const TXT_AND_MD: &str = "\n[parsers]\nenabled = [\"md\", \"txt\"]\n";

/// The per-file warning for a rejected cross-parser rename, which ends with
/// the row gone.
const CROSSED_REJECTED_WARNING: &str = "warning: note.md: embedding endpoint rejected the input \
     (HTTP 413); skipped, this file is not in the index";

/// A text note carrying the reject marker, indexed while the mock answers
/// normally, so it has a row before it is renamed.
fn marked_txt_note() -> String {
    format!("A plain text note about the harbour {REJECT_MARKER} and its tides.\n")
}

/// `groove index` pairs `note.txt` -> `note.md` as a rename that crosses a
/// parser, re-parses it under the Markdown parser, and the endpoint refuses
/// it. The old parser's row is then dropped, so the warning must say the file
/// is not in the index, not that the index kept what it had.
///
/// Red if the warning's tail is decided from the row before the rename is
/// settled (it would say "keeps what it had" over a row that is deleted).
#[test]
fn index_says_not_in_the_index_when_a_cross_parser_rename_is_rejected() {
    let txt = marked_txt_note();
    let alpha = note("Alpha", ALPHA);
    let beta = note("Beta", BETA);
    let fx = fixture(
        "groove-aw04-cross-rebuild",
        &[
            ("alpha.md", alpha.as_str()),
            ("beta.md", beta.as_str()),
            ("note.txt", txt.as_str()),
        ],
        TXT_AND_MD,
    );
    fx.index();
    assert_eq!(fx.documents(), 3);

    std::fs::rename(fx.kb().join("note.txt"), fx.kb().join("note.md")).expect("rename note.txt");
    fx.layout
        .write("beta.md", &note("Beta", &format!("{BETA} Edited.")));
    fx.answer_with(rejects_marker(413));
    let out = fx.run_index();
    fx.answer_normally();
    let stderr = stderr_of(&out);
    assert!(out.status.success(), "{stderr}");
    assert!(stderr.contains(CROSSED_REJECTED_WARNING), "{stderr}");
    assert!(!stderr.contains("keeps what it had"), "{stderr}");
    assert!(!stderr.contains(BODY_SENTINEL), "{stderr}");
    assert_eq!(fx.documents(), 2, "the old parser's row must be gone");
    assert_dir_empty(&fx.cache);
}

/// The watcher meets the same cross-parser rename one event at a time: its
/// [`grooveseek::indexer::rename_single_file`] drops the old parser's row after the refusal, so the
/// warning must say the file is not in the index.
///
/// Red if the warning's tail is decided before the rename is settled.
///
/// Not every backend pairs a rename: macOS (FSEvents) can deliver it as a
/// removal of `note.txt` (deindexed) and a creation of `note.md` (a plain
/// reindex, refused, no row). The end state and the warning are the same
/// either way and are asserted on every OS; the rename line is checked only
/// when it appeared.
#[test]
fn the_watcher_says_not_in_the_index_when_a_cross_parser_rename_is_rejected() {
    let txt = marked_txt_note();
    let alpha = note("Alpha", ALPHA);
    let fx = fixture(
        "groove-aw04-cross-watch",
        &[("alpha.md", alpha.as_str()), ("note.txt", txt.as_str())],
        &format!("{TXT_AND_MD}\n[watch]\nenabled = true\ndebounce_ms = 300\n"),
    );
    fx.index();
    assert_eq!(fx.documents(), 2);
    fx.answer_with(rejects_marker(413));
    let (guard, _base) = spawn_serve_with(fx.kb(), &fx.config, true, |c| {
        hermetic(c, &fx.cache);
    });
    std::fs::rename(fx.kb().join("note.txt"), fx.kb().join("note.md")).expect("rename note.txt");
    let lines = || -> Vec<String> {
        guard
            .stderr()
            .lines()
            .iter()
            .map(|l| strip_ansi(l))
            .collect()
    };
    const RENAME_LINE: &str = "watcher: renamed note.txt -> note.md";
    const REINDEX_LINE: &str = "watcher: skipped note.md (embedding endpoint rejected the input)";
    const DEINDEX_LINE: &str = "watcher: deindexed note.txt";
    // Unpaired, both halves have to have been handled before the end state is read.
    let reported = wait_until(Duration::from_secs(30), || {
        let all = lines();
        let has = |needle: &str| all.iter().any(|l| l.contains(needle));
        has(RENAME_LINE) || (has(REINDEX_LINE) && has(DEINDEX_LINE))
    });
    let all = lines();
    drop(guard);
    fx.answer_normally();
    assert!(
        reported,
        "neither a rename line nor a reindex plus deindex:\n{}",
        all.join("\n")
    );
    let has = |needle: &str| all.iter().any(|l| l.contains(needle));
    if has(RENAME_LINE) {
        // A paired rename (Linux, Windows): the cross-parser arm drops the row.
        assert!(
            has(
                "watcher: renamed note.txt -> note.md (the new parser could not index it, \
                 document dropped from the index)"
            ),
            "{}",
            all.join("\n")
        );
    }
    assert!(has(CROSSED_REJECTED_WARNING), "{}", all.join("\n"));
    assert!(!has("indexed note.md"), "{}", all.join("\n"));
    assert!(!has("keeps what it had"), "{}", all.join("\n"));
    assert!(!has(BODY_SENTINEL), "{}", all.join("\n"));
    assert_eq!(fx.documents(), 1, "the old parser's row must be gone");
    assert_dir_empty(&fx.cache);
}

/// A 503 whose body is cut short (the connection closes before the promised
/// `Content-Length`) is still a 503: it is retried after the `Retry-After` its
/// headers carried, and the retry succeeds.
///
/// Red if a body that breaks off (not a timeout) makes the failure fatal
/// whatever the status said (the run would stop after one request), or if
/// the failed body read drops `Retry-After: 2`: the backoff alone waits 1 s
/// plus under 0.25 s of jitter, below the 2 s checked for.
#[test]
fn index_retries_a_503_whose_body_is_cut_short() {
    let notes = one_note();
    let fx = fixture("groove-aw04-503-cut", &files(&notes), "");
    let first = Arc::new(Mutex::new(true));
    {
        let first = first.clone();
        fx.answer_with(move |req| {
            let mut first = first.lock().expect("first lock");
            if std::mem::replace(&mut *first, false) {
                let mut cut = reply(503, &[("Retry-After", "2")]);
                cut.truncate_body = true;
                cut
            } else {
                MockReply::plain(default_response(req, DIM))
            }
        });
    }
    let started = Instant::now();
    let out = fx.run_index();
    let took = started.elapsed();
    let stderr = stderr_of(&out);
    assert!(out.status.success(), "{stderr}");
    assert_eq!(
        fx.mock.requests().len(),
        2,
        "one cut 503, one success: {stderr}"
    );
    assert!(
        took >= Duration::from_secs(2),
        "Retry-After: 2 was not waited: {took:?}"
    );
    assert!(!stderr.contains(BODY_SENTINEL), "{stderr}");
    assert_dir_empty(&fx.cache);
}

/// A 401 whose body never arrives is still a 401: the run stops after the one
/// request, with `max_retries` at its default. Sending the key again would
/// only repeat a credential the endpoint has refused, while holding the
/// embedder.
///
/// Red if a body-read timeout is retried whatever the status said: the run
/// would send 1 + `max_retries` requests before it stops. Red too if the
/// error stops naming the status (`HTTP 401: <empty>`, the body unread).
#[test]
fn index_does_not_retry_a_401_whose_body_stalls() {
    let notes = one_note();
    let fx = fixture("groove-aw04-401-stall", &files(&notes), "");
    // A one-second request timeout, so the stalled body fails fast.
    let config = std::fs::read_to_string(&fx.config).expect("read groove.toml");
    assert!(config.contains("timeout_seconds = 15\n"), "{config}");
    std::fs::write(
        &fx.config,
        config.replace("timeout_seconds = 15\n", "timeout_seconds = 1\n"),
    )
    .expect("write groove.toml");
    fx.answer_with(|_| {
        let mut stalled = reply(401, &[("Retry-After", "0")]);
        stalled.stall_body = true;
        stalled
    });
    let out = fx.run_index();
    let stderr = stderr_of(&out);
    assert!(!out.status.success(), "{stderr}");
    assert_eq!(fx.mock.requests().len(), 1, "{stderr}");
    assert!(stderr.contains("HTTP 401: <empty>"), "{stderr}");
    assert!(!stderr.contains(BODY_SENTINEL), "{stderr}");
    assert_dir_empty(&fx.cache);
}

/// MCP `search` whose query the endpoint answers with 401 reports the status
/// and not the response body: the body may echo anything the endpoint holds,
/// and every MCP client would see it (ADR-0025).
///
/// Red if the reply is built from the provider error's text, which carries
/// the body snippet.
#[test]
fn mcp_search_reports_a_query_side_401_without_the_response_body() {
    let notes = three_notes();
    let fx = fixture("groove-aw04-mcp-search-401", &files(&notes), "");
    fx.index();
    let (guard, base) = spawn_serve_with(fx.kb(), &fx.config, false, |c| {
        hermetic(c, &fx.cache);
    });
    let session = mcp_initialize(&base);
    fx.answer_with(|req| {
        if req.model() == Some(QUERY_MODEL) {
            reply(401, &[])
        } else {
            MockReply::plain(default_response(req, DIM))
        }
    });
    let resp = mcp_tool_call(
        &base,
        &session,
        "search",
        serde_json::json!({"query": "lighthouse keeper ship"}),
    );
    fx.answer_normally();
    drop(guard);
    let error = resp.get("error").and_then(|v| v.as_str()).unwrap_or("");
    assert!(error.contains("HTTP 401"), "{resp}");
    assert!(!resp.to_string().contains(BODY_SENTINEL), "{resp}");
    assert_dir_empty(&fx.cache);
}

/// MCP `rebuild_index` whose document embed the endpoint answers with 401
/// names the file and the status, still without the response body. The
/// indexer wraps the typed error in its own context, so the status has to be
/// found below the outermost message.
///
/// Red if only the outermost message is looked at: the reply would say
/// `failed to embed chunks for alpha.md` and never name HTTP 401.
#[test]
fn mcp_rebuild_index_names_a_document_side_401_without_the_response_body() {
    let notes = three_notes();
    let fx = fixture("groove-aw04-mcp-rebuild-401", &files(&notes), "");
    fx.index();
    fx.layout
        .write("alpha.md", &note("Alpha", &format!("{ALPHA} Edited.")));
    let (guard, base) = spawn_serve_with(fx.kb(), &fx.config, false, |c| {
        hermetic(c, &fx.cache);
    });
    let session = mcp_initialize(&base);
    fx.answer_with(|_| reply(401, &[]));
    let resp = mcp_tool_call(&base, &session, "rebuild_index", serde_json::json!({}));
    fx.answer_normally();
    drop(guard);
    let error = resp.get("error").and_then(|v| v.as_str()).unwrap_or("");
    assert!(
        error.contains("failed to embed chunks for alpha.md: embedding endpoint returned HTTP 401"),
        "{resp}"
    );
    assert!(!resp.to_string().contains(BODY_SENTINEL), "{resp}");
    assert_dir_empty(&fx.cache);
}

/// A 429 asking for more than 60 s is given up on at once, without reading
/// its body: a body that stalls must not hold the run (and a daemon's
/// embedder) for the request timeout.
///
/// Red if the body is read before the `Retry-After` is looked at: the run
/// would wait for the stalled body (the mock holds it for up to 10 s) before
/// it gives up.
#[test]
fn index_gives_up_on_a_long_retry_after_without_reading_a_stalled_body() {
    let notes = one_note();
    let fx = fixture("groove-aw04-429-stall", &files(&notes), "");
    // A request timeout well above the time the test allows for the run.
    let config = std::fs::read_to_string(&fx.config).expect("read groove.toml");
    assert!(config.contains("timeout_seconds = 15\n"), "{config}");
    std::fs::write(
        &fx.config,
        config.replace("timeout_seconds = 15\n", "timeout_seconds = 20\n"),
    )
    .expect("write groove.toml");
    fx.answer_with(|_| {
        let mut stalled = reply(429, &[("Retry-After", "120")]);
        stalled.stall_body = true;
        stalled
    });
    let started = Instant::now();
    let out = fx.run_index();
    let took = started.elapsed();
    let stderr = stderr_of(&out);
    assert!(!out.status.success(), "{stderr}");
    assert_eq!(fx.mock.requests().len(), 1, "{stderr}");
    assert!(stderr.contains("asked to retry after 120 s"), "{stderr}");
    assert!(took < Duration::from_secs(5), "waited {took:?}: {stderr}");
    assert!(!stderr.contains(BODY_SENTINEL), "{stderr}");
    assert_dir_empty(&fx.cache);
}
