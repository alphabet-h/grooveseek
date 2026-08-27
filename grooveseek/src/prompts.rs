//! The MCP `prompts` surface: named, argument-taking recipes a user invokes.
//!
//! Clients present these as commands the **user** picks — Claude Code renders
//! them as `/mcp__<server>__<name>`, where `<server>` is the key the user gave
//! this server in `.mcp.json` rather than anything chosen here — so each one
//! exists to answer a question the tools can already answer but that a caller
//! has to know how to assemble.
//! `search` alone does not tell anyone to follow it with `get_connection_graph`,
//! or that a `low_confidence` flag means the answer should say so.
//!
//! # Why these are in the binary and not in `groove.toml`
//!
//! A prompt is text that goes to the model. `groove.toml` is discovered — from
//! the working directory or a `.git` ancestor — and a discovered file is not
//! necessarily one the user wrote. `Config::restrict_untrusted` in `config.rs`
//! already drops four fields from an untrusted config, and the reason given for
//! the strictest of them, `kb_path`, is that it decides *what gets indexed and
//! handed to the LLM client*. Prompt text is squarely inside that reasoning, so
//! a `[prompts]` section would need a restriction rule of its own to be safe,
//! and the MCP specification offers no help: its entire security note on
//! prompts is one sentence, and unlike tool annotations there is no guidance
//! telling clients to distrust prompt content. Nothing here is worth that, so
//! the set is fixed at compile time and the trust surface does not move.
//!
//! # Text only
//!
//! No embedded resources. A prompt message that embeds one obliges the server to
//! implement the `resources` capability too, which would make this depend on
//! work that has not landed. `get_document` already serves content, and these
//! prompts tell the model to call it.

use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{PromptMessage, Role};
use rmcp::schemars;
use rmcp::{prompt, prompt_router};
use serde::Deserialize;

use crate::server::KbServer;

/// Everything a prompt says about how to answer from this knowledge base, so
/// the four bodies do not drift apart on the parts that are the same.
const CITATION_RULES: &str = "\
Ground every claim in what you retrieved:\n\
- Cite the `path` of each document you used. Do not cite one you did not open.\n\
- **Call `get_document` on every document you are going to mention, before you \
mention it — including ones you are about to dismiss.** A search result is an \
excerpt, so a hit that looks unrelated may sit in a document that answers the \
question outright, and reporting on an excerpt alone is both wrong and \
uncitable under the rule above.\n\
- If `low_confidence` is set on a search response, say the knowledge base may \
not cover the question well, rather than answering as if it did.\n\
- When the knowledge base does not say something, say that. Do not fill the \
gap from general knowledge without marking it as outside the knowledge base.";

#[derive(Deserialize, schemars::JsonSchema, Default)]
#[schemars(transform = crate::schema_compat::ClientCompat)]
pub struct SummarizeTopicArgs {
    /// The topic to summarize, as it appears in `list_topics` (e.g. "mcp").
    pub topic: String,
}

#[derive(Deserialize, schemars::JsonSchema, Default)]
#[schemars(transform = crate::schema_compat::ClientCompat)]
pub struct DeepDiveArgs {
    /// The question to answer from the knowledge base.
    pub question: String,
}

#[derive(Deserialize, schemars::JsonSchema, Default)]
#[schemars(transform = crate::schema_compat::ClientCompat)]
pub struct WhatsNewArgs {
    /// Only consider documents dated on or after this ISO-8601 date
    /// (e.g. "2026-07-01"). Omit for roughly the last month.
    pub since: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema, Default)]
#[schemars(transform = crate::schema_compat::ClientCompat)]
pub struct FindGapsArgs {
    /// Restrict the search for gaps to one topic. Omit to look across the
    /// whole knowledge base.
    pub topic: Option<String>,
}

/// The prompt surface.
///
/// Each handler does nothing but call the free function below it. The bodies are
/// separate so the tests can exercise **the production text** without building a
/// `KbServer`, which owns a `Database` and an `Embedder`; a test that had to
/// construct those would end up asserting against a copy of the prompt instead,
/// which is the failure `should_process_parts` was split out to avoid.
#[prompt_router(vis = "pub")]
impl KbServer {
    #[prompt(
        name = "summarize_topic",
        description = "Gather what the knowledge base holds on a topic and summarize it with citations."
    )]
    pub async fn summarize_topic_prompt(
        &self,
        Parameters(args): Parameters<SummarizeTopicArgs>,
    ) -> Vec<PromptMessage> {
        summarize_topic_body(args)
    }

    #[prompt(
        name = "deep_dive",
        description = "Answer a question from the knowledge base, following related documents before answering."
    )]
    pub async fn deep_dive_prompt(
        &self,
        Parameters(args): Parameters<DeepDiveArgs>,
    ) -> Vec<PromptMessage> {
        deep_dive_body(args)
    }

    #[prompt(
        name = "whats_new",
        description = "Survey documents dated since a given date, and say plainly what that date does and does not mean."
    )]
    pub async fn whats_new_prompt(
        &self,
        Parameters(args): Parameters<WhatsNewArgs>,
    ) -> Vec<PromptMessage> {
        whats_new_body(args)
    }

    #[prompt(
        name = "find_gaps",
        description = "Look for what the knowledge base does not cover, or covers thinly."
    )]
    pub async fn find_gaps_prompt(
        &self,
        Parameters(args): Parameters<FindGapsArgs>,
    ) -> Vec<PromptMessage> {
        find_gaps_body(args)
    }
}

/// Summarize what this knowledge base says about one topic.
fn summarize_topic_body(args: SummarizeTopicArgs) -> Vec<PromptMessage> {
    let topic = args.topic;
    one_user_message(format!(
        "Summarize what this knowledge base says about the topic `{topic}`.\n\
             \n\
             How to gather the material:\n\
             1. Call `list_topics` to confirm `{topic}` exists and see how many \
             documents it holds. If it does not exist, say so and offer the \
             closest topics instead of guessing.\n\
             2. Call `search` with `topic: \"{topic}\"` and a query describing \
             what the topic is about, with `limit` around 15. Chunks come back \
             ranked, not exhaustively.\n\
             3. For the documents that carry the most weight, call \
             `get_document` to read them whole. A chunk is an excerpt.\n\
             \n\
             Then write the summary: what this knowledge base establishes about \
             `{topic}`, where its sources disagree, and what it does not cover.\n\
             \n\
             {CITATION_RULES}"
    ))
}

/// Answer one question, following the links between documents.
fn deep_dive_body(args: DeepDiveArgs) -> Vec<PromptMessage> {
    let question = args.question;
    one_user_message(format!(
        "Answer this question from the knowledge base: {question}\n\
             \n\
             Do not answer from the first search alone.\n\
             1. Call `search` with the question as the query.\n\
             2. Take the strongest one or two hits and call \
             `get_connection_graph` on their paths with `depth: 2`. That surfaces \
             semantically adjacent material the query wording did not reach — \
             which is usually where a question of this kind is actually answered.\n\
             3. Call `get_document` on the documents that matter, so you are \
             answering from whole documents rather than excerpts.\n\
             4. If the graph led somewhere the original query did not, search \
             again with the vocabulary you found there.\n\
             \n\
             {CITATION_RULES}"
    ))
}

/// How far back `whats_new` looks when the caller does not say.
const DEFAULT_WHATS_NEW_DAYS: i64 = 30;

/// `days` before `today`, as an ISO date.
///
/// The default has to be a real date rather than a phrase. `date_from` reaches
/// `matches_date_range`, which compares the **raw strings** lexicographically
/// (`db/search.rs:50`) — so "about a month ago" sorts above every `2026-…`
/// document date, filters the whole corpus out, and the survey comes back empty
/// instead of failing (codex P2, round 1 on PR #161).
///
/// Split from the clock so the arithmetic can be pinned to a fixed day.
fn iso_days_before(today: chrono::NaiveDate, days: i64) -> String {
    (today - chrono::Duration::days(days))
        .format("%Y-%m-%d")
        .to_string()
}

/// Survey recently dated documents.
fn whats_new_body(args: WhatsNewArgs) -> Vec<PromptMessage> {
    let since = args.since.unwrap_or_else(|| {
        iso_days_before(chrono::Utc::now().date_naive(), DEFAULT_WHATS_NEW_DAYS)
    });
    one_user_message(format!(
        "Survey what has been added to this knowledge base since {since}.\n\
             \n\
             1. Call `search` with `date_from: \"{since}\"` and a broad query, \
             with a high `limit`. Repeat with a few different queries: the date \
             filter narrows the candidates, but the ranking is still driven by \
             the query, so one query will not surface everything.\n\
             **`date_from` is compared as a plain string, not parsed as a \
             date.** It must be `YYYY-MM-DD`. If the value above is anything \
             else, convert it to that form first — passing prose filters out \
             every document instead of erroring.\n\
             2. Repeat at least one of those searches with \
             `include_low_quality: true`. The per-chunk quality filter is on by \
             default, and it hides short notes, TODOs and stubs — which is a \
             large part of what \"recently added\" looks like. Without this pass \
             the survey silently omits them and can come back empty while \
             matching documents exist.\n\
             3. Group what you find by `topic` and `category`.\n\
             4. Call `get_document` on **every** document that will appear in \
             the survey, stubs and one-line notes included — not only the ones \
             that look substantial. The citation rules below forbid citing a \
             document you did not open, so anything left unopened has to be \
             dropped from the answer, which would put back exactly the omission \
             step 2 exists to prevent. A stub is a legitimate finding; report it \
             as a stub.\n\
             \n\
             **State this limitation in your answer rather than hiding it.** \
             The date being filtered on is the `date` field in each document's \
             frontmatter — what the author wrote down — not when the file was \
             last modified or indexed. A document edited yesterday but still \
             carrying last year's date will not appear, and a backdated note \
             added today will not either. groove has no query for \"recently \
             changed\"; this is an approximation and should be described as one.\n\
             \n\
             {CITATION_RULES}"
    ))
}

/// Look for what the knowledge base is missing.
fn find_gaps_body(args: FindGapsArgs) -> Vec<PromptMessage> {
    let (scope, filter) = match args.topic {
        // The filter clause is not decoration. Without `topic:` on every call, a
        // broad question can be answered strongly by a document in an unrelated
        // topic — `low_confidence` stays false and the gap in the topic that was
        // actually asked about is masked (codex P2, round 1 on PR #161).
        Some(topic) => (
            format!("the topic `{topic}`"),
            format!(
                "**Pass `topic: \"{topic}\"` on every one of these searches**, \
                 including the `include_low_quality` ones. Without it a strong \
                 answer from a different topic keeps `low_confidence` false and \
                 hides the gap you were asked to find.\n"
            ),
        ),
        None => ("this knowledge base".to_string(), String::new()),
    };
    one_user_message(format!(
        "Find what {scope} does not cover, or covers thinly.\n\
             \n\
             {filter}\
             1. Call `list_topics` for the shape of the corpus. A topic with one \
             or two documents is a candidate, but a small topic is not \
             automatically a gap — some subjects need one page.\n\
             2. Ask questions a reader of {scope} would expect answered, one \
             `search` each. Three things count as a signal, in this order:\n\
             - **An empty `results` array.** This is the clearest absence there \
             is, and it is the one case the flags will not tell you about: \
             `low_confidence` needs at least two scores to compare, so with no \
             hits at all it stays `false`. Do not read that as coverage.\n\
             - `low_confidence` set on a response that did return hits.\n\
             - Hits that came back but are only loosely related.\n\
             3. Pass `include_low_quality: true` on a few of those searches. \
             Chunks below the quality threshold are hidden by default, and a \
             stub or a TODO that exists is a different finding from nothing at \
             all — the first says someone meant to write it.\n\
             \n\
             4. Before calling anything a gap, `get_document` the hits you are \
             judging. A loose-looking excerpt can come from a document that \
             answers the question in a section the query never matched, and \
             declaring a gap on the strength of an excerpt manufactures one.\n\
             \n\
             Report each gap as: the question that went unanswered, what the \
             knowledge base returned instead, and whether it looks like an \
             absence or a stub. Do not propose content; say what is missing.\n\
             \n\
             {CITATION_RULES}"
    ))
}

/// Every prompt here is a single user-role message.
///
/// The alternative — an assistant message priming a reply — puts words in the
/// model's mouth that the user never approved, and these are invoked by a user
/// choosing a command.
fn one_user_message(text: String) -> Vec<PromptMessage> {
    vec![PromptMessage::new_text(Role::User, text)]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The generated router is what `prompts/list` answers from, so this is the
    /// list a client sees.
    #[test]
    fn the_router_exposes_exactly_the_four_prompts() {
        let router = KbServer::prompt_router();
        let mut names: Vec<String> = router
            .list_all()
            .into_iter()
            .map(|p| p.name.to_string())
            .collect();
        names.sort();
        assert_eq!(
            names,
            vec!["deep_dive", "find_gaps", "summarize_topic", "whats_new"],
            "the prompt surface changed; if that was deliberate, update this list"
        );
    }

    /// Required arguments must be advertised as required, or a client will let
    /// the user invoke the prompt with nothing and the body will interpolate an
    /// empty string.
    #[test]
    fn required_arguments_are_marked_required_and_optional_ones_are_not() {
        let router = KbServer::prompt_router();
        let by_name = |name: &str| {
            router
                .list_all()
                .into_iter()
                .find(|p| p.name == name)
                .unwrap_or_else(|| panic!("prompt {name} is missing"))
        };

        for (prompt_name, arg, required) in [
            ("summarize_topic", "topic", true),
            ("deep_dive", "question", true),
            ("whats_new", "since", false),
            ("find_gaps", "topic", false),
        ] {
            let p = by_name(prompt_name);
            let args = p
                .arguments
                .unwrap_or_else(|| panic!("{prompt_name} advertises no arguments"));
            let found = args
                .iter()
                .find(|a| a.name == arg)
                .unwrap_or_else(|| panic!("{prompt_name} is missing the {arg} argument"));
            assert_eq!(
                found.required.unwrap_or(false),
                required,
                "{prompt_name}.{arg} required flag"
            );
        }
    }

    /// The text of a message, whatever content variant it is carrying.
    fn text_of(messages: &[PromptMessage]) -> String {
        assert_eq!(messages.len(), 1, "each prompt is one message");
        assert!(
            matches!(messages[0].role, Role::User),
            "a prompt must not put words in the assistant's mouth"
        );
        format!("{:?}", messages[0].content)
    }

    /// The shared rules are what keep an answer tied to what was retrieved, and
    /// they are easy to drop while editing one prompt.
    #[test]
    fn every_prompt_carries_the_citation_rules_and_one_user_message() {
        let bodies = [
            summarize_topic_body(SummarizeTopicArgs {
                topic: "mcp".to_string(),
            }),
            deep_dive_body(DeepDiveArgs {
                question: "how does fusion work?".to_string(),
            }),
            whats_new_body(WhatsNewArgs { since: None }),
            find_gaps_body(FindGapsArgs { topic: None }),
        ];

        for messages in &bodies {
            let text = text_of(messages);
            assert!(
                text.contains("Cite the `path`"),
                "the citation rules were dropped from a prompt body: {text}"
            );
            // (codex P2, rounds 3-4 on PR #161) Twice, a prompt told the model
            // to retrieve something and then to report on it without opening
            // it — which the rule above makes uncitable, and which turns an
            // excerpt into a verdict. Fixing it prompt by prompt produced a
            // second instance one round later, so the obligation lives in the
            // shared rules and every body is checked for it here.
            assert!(
                text.contains("before you mention it"),
                "the open-before-you-cite rule is missing from a prompt body: {text}"
            );
        }
    }

    /// The argument is interpolated, not ignored.
    #[test]
    fn arguments_reach_the_message_body() {
        let text = text_of(&summarize_topic_body(SummarizeTopicArgs {
            topic: "zqxw-distinct-topic".to_string(),
        }));
        assert!(
            text.contains("zqxw-distinct-topic"),
            "the topic argument never reached the body: {text}"
        );

        let text = text_of(&deep_dive_body(DeepDiveArgs {
            question: "zqxw-distinct-question".to_string(),
        }));
        assert!(
            text.contains("zqxw-distinct-question"),
            "the question argument never reached the body: {text}"
        );
    }

    /// An omitted optional argument must produce something usable, not a hole.
    #[test]
    fn omitted_optional_arguments_still_read_as_english() {
        let scoped = text_of(&find_gaps_body(FindGapsArgs {
            topic: Some("mcp".to_string()),
        }));
        assert!(scoped.contains("the topic `mcp`"), "{scoped}");
        let unscoped = text_of(&find_gaps_body(FindGapsArgs { topic: None }));
        assert!(
            unscoped.contains("this knowledge base") && !unscoped.contains("the topic ``"),
            "an omitted `topic` must widen the scope rather than leave an empty name: {unscoped}"
        );
    }

    /// (codex P2, round 1 on PR #161) The default cutoff must be a date the
    /// search can actually use.
    ///
    /// `date_from` is compared as a raw string by `matches_date_range`, so a
    /// phrase like "about a month ago" sorts above every `2026-…` document date
    /// and filters the whole corpus out — an empty survey rather than an error.
    #[test]
    fn the_default_cutoff_is_an_iso_date_not_a_phrase() {
        let text = text_of(&whats_new_body(WhatsNewArgs { since: None }));

        let iso = text
            .split(|c: char| !(c.is_ascii_digit() || c == '-'))
            .find(|token| {
                token.len() == 10
                    && token.as_bytes()[4] == b'-'
                    && token.as_bytes()[7] == b'-'
                    && token.chars().filter(char::is_ascii_digit).count() == 8
            })
            .unwrap_or_else(|| panic!("no YYYY-MM-DD anywhere in the body: {text}"));

        // The property that matters: whatever the clock says, the emitted value
        // must sort *below* a document date from the same era, or the filter
        // excludes everything.
        assert!(
            iso < "9999-12-31",
            "the cutoff must compare as a date against ISO document dates: {iso}"
        );
        assert!(
            !text.contains("about a month ago"),
            "the prose placeholder must be gone: {text}"
        );
        assert!(
            text.contains("compared as a plain string"),
            "the prompt must warn that date_from is not parsed: {text}"
        );
    }

    /// (codex P2, round 2 on PR #161) A survey of what is new has to ask for
    /// the chunks the default quality filter hides. Short notes, TODOs and
    /// stubs are a large part of what "recently added" looks like, and the
    /// filter drops them before the caller ever sees them.
    #[test]
    fn whats_new_asks_for_the_chunks_the_quality_filter_hides() {
        let text = text_of(&whats_new_body(WhatsNewArgs {
            since: Some("2026-01-01".to_string()),
        }));
        assert!(
            text.contains("include_low_quality"),
            "the survey must include one low-quality pass or it silently omits \
             short new notes: {text}"
        );
        // (codex P2, round 3 on PR #161) Recovering the stubs is undone if the
        // model is then told to open only what looks substantial: the citation
        // rules forbid citing an unopened document, so an unopened stub has to
        // be dropped from the answer.
        assert!(
            text.contains("every"),
            "the survey must open everything it reports, or the low-quality pass \
             recovers documents the citation rules then force it to omit: {text}"
        );
    }

    /// The arithmetic, with the clock pinned — the part `the_default_cutoff…`
    /// cannot check because it does not know what day it is.
    #[test]
    fn the_default_cutoff_is_thirty_days_back() {
        let today = chrono::NaiveDate::from_ymd_opt(2026, 8, 15).unwrap();
        assert_eq!(iso_days_before(today, DEFAULT_WHATS_NEW_DAYS), "2026-07-16");
        // Across a year boundary, which is where hand-rolled arithmetic breaks.
        let jan = chrono::NaiveDate::from_ymd_opt(2026, 1, 10).unwrap();
        assert_eq!(iso_days_before(jan, 30), "2025-12-11");
    }

    /// (codex P2, round 1 on PR #161) A requested topic has to reach the
    /// searches, not just the prose. Without the filter, a strong answer from
    /// an unrelated topic keeps `low_confidence` false and masks the gap.
    #[test]
    fn find_gaps_tells_the_model_to_filter_by_the_requested_topic() {
        let scoped = text_of(&find_gaps_body(FindGapsArgs {
            topic: Some("zqxw-topic".to_string()),
        }));
        assert!(
            scoped.contains("topic: \\\"zqxw-topic\\\"")
                || scoped.contains("topic: \"zqxw-topic\""),
            "the topic must be passed to search, not only named in prose: {scoped}"
        );
        assert!(
            scoped.contains("include_low_quality"),
            "the filter instruction must cover the low-quality pass too: {scoped}"
        );

        let unscoped = text_of(&find_gaps_body(FindGapsArgs { topic: None }));
        assert!(
            !unscoped.contains("Pass `topic:"),
            "with no topic there is nothing to filter by: {unscoped}"
        );
    }

    /// (codex P2, round 5 on PR #161) An empty result set is the clearest gap
    /// there is, and it is the one the flags stay silent about:
    /// `compute_low_confidence` (`server.rs:1303`) returns `false` when there
    /// are fewer than two scores to compare, so no hits at all reads as
    /// confident coverage.
    #[test]
    fn find_gaps_treats_an_empty_result_set_as_the_strongest_signal() {
        let text = text_of(&find_gaps_body(FindGapsArgs { topic: None }));
        assert!(
            text.contains("empty `results` array"),
            "the emptiest case must be named explicitly: {text}"
        );
        assert!(
            text.contains("at least two scores"),
            "the prompt must say why the flag does not fire there: {text}"
        );
    }

    /// `whats_new` must keep saying what its date filter really means. Nothing
    /// in groove answers "recently changed" — `date_from` filters the
    /// frontmatter `date`, which is what an author typed — and a prompt that
    /// implies otherwise is the kind of overstatement this project has had to
    /// correct before.
    #[test]
    fn whats_new_states_that_the_date_is_frontmatter_not_modification_time() {
        let text = text_of(&whats_new_body(WhatsNewArgs {
            since: Some("2026-01-01".to_string()),
        }));
        assert!(
            text.contains("2026-01-01"),
            "the since argument never reached the body: {text}"
        );
        assert!(
            text.contains("frontmatter"),
            "the limitation must be stated in the prompt itself: {text}"
        );
        assert!(
            text.contains("not when the file was"),
            "the prompt must say what the date is not: {text}"
        );
    }
}
