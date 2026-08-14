//! The MCP `prompts` surface: named, argument-taking recipes a user invokes.
//!
//! Clients present these as commands the **user** picks — Claude Code renders
//! them as `/mcp__kb-mcp__<name>` — so each one exists to answer a question the
//! tools can already answer but that a caller has to know how to assemble.
//! `search` alone does not tell anyone to follow it with `get_connection_graph`,
//! or that a `low_confidence` flag means the answer should say so.
//!
//! # Why these are in the binary and not in `kb-mcp.toml`
//!
//! A prompt is text that goes to the model. `kb-mcp.toml` is discovered — from
//! the working directory or a `.git` ancestor — and a discovered file is not
//! necessarily one the user wrote. `restrict_untrusted` already drops three
//! fields from an untrusted config, and the reason given for the strictest of
//! them, `kb_path`, is that it decides *what gets indexed and handed to the LLM
//! client*. Prompt text is squarely inside that reasoning, so a `[prompts]`
//! section would need a fourth restriction rule to be safe, and the MCP
//! specification offers no help: its entire security note on prompts is one
//! sentence, and unlike tool annotations there is no guidance telling clients to
//! distrust prompt content. Nothing here is worth that, so the set is fixed at
//! compile time and the trust surface does not move.
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

/// Survey recently dated documents.
fn whats_new_body(args: WhatsNewArgs) -> Vec<PromptMessage> {
    let since = args
        .since
        .unwrap_or_else(|| "about a month ago".to_string());
    one_user_message(format!(
        "Survey what has been added to this knowledge base since {since}.\n\
             \n\
             1. Call `search` with `date_from` set to that date and a broad \
             query, with a high `limit`. Repeat with a few different queries: \
             the date filter narrows the candidates, but the ranking is still \
             driven by the query, so one query will not surface everything.\n\
             2. Group what you find by `topic` and `category`.\n\
             3. Read the ones that look substantial with `get_document`.\n\
             \n\
             **State this limitation in your answer rather than hiding it.** \
             The date being filtered on is the `date` field in each document's \
             frontmatter — what the author wrote down — not when the file was \
             last modified or indexed. A document edited yesterday but still \
             carrying last year's date will not appear, and a backdated note \
             added today will not either. kb-mcp has no query for \"recently \
             changed\"; this is an approximation and should be described as one.\n\
             \n\
             {CITATION_RULES}"
    ))
}

/// Look for what the knowledge base is missing.
fn find_gaps_body(args: FindGapsArgs) -> Vec<PromptMessage> {
    let scope = match args.topic {
        Some(topic) => format!("the topic `{topic}`"),
        None => "this knowledge base".to_string(),
    };
    one_user_message(format!(
        "Find what {scope} does not cover, or covers thinly.\n\
             \n\
             1. Call `list_topics` for the shape of the corpus. A topic with one \
             or two documents is a candidate, but a small topic is not \
             automatically a gap — some subjects need one page.\n\
             2. Ask questions a reader of {scope} would expect answered, one \
             `search` each. A question that comes back with `low_confidence` \
             set, or whose best hits are only loosely related, is the signal \
             worth reporting.\n\
             3. Pass `include_low_quality: true` on a few of those searches. \
             Chunks below the quality threshold are hidden by default, and a \
             stub or a TODO that exists is a different finding from nothing at \
             all — the first says someone meant to write it.\n\
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

    /// An omitted optional argument must produce a sentence, not a hole.
    #[test]
    fn omitted_optional_arguments_still_read_as_english() {
        let text = text_of(&whats_new_body(WhatsNewArgs { since: None }));
        assert!(
            text.contains("about a month ago"),
            "an omitted `since` must become a phrase: {text}"
        );

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

    /// `whats_new` must keep saying what its date filter really means. Nothing
    /// in kb-mcp answers "recently changed" — `date_from` filters the
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
