use anyhow::{Context, Result};
use fastembed::{
    EmbeddingModel, InitOptions, RerankInitOptions, RerankerModel, TextEmbedding, TextRerank,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fmt;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use crate::db::SearchResult;

/// Embedding モデル選択肢。CLI の `--model` と共有される。
///
/// 追加時の手順: variant を足し、`model_id` / `dimension` /
/// `fastembed_model` / `approx_download_mb` の 4 メソッドに分岐を追加する。
///
/// デフォルトは既存 DB 互換のため `BgeSmallEnV15` に固定 (`#[default]`)。
/// BGE-M3 へ切り替えたい場合は CLI で明示オプトインする。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, clap::ValueEnum, Deserialize)]
pub enum ModelChoice {
    // A variant's doc comment is also its line under `--model` in `--help`,
    // so it is written in English, like the rest of the help.
    /// BAAI/bge-small-en-v1.5 (384 dim, English-focused, ~130 MB first
    /// download). The built-in default.
    #[default]
    #[value(name = "bge-small-en-v1.5")]
    #[serde(rename = "bge-small-en-v1.5")]
    BgeSmallEnV15,
    /// BAAI/bge-m3 (1024 dim, multilingual incl. Japanese, ~2.3 GB first
    /// download). Recommended for Japanese-heavy knowledge bases.
    #[value(name = "bge-m3")]
    #[serde(rename = "bge-m3")]
    BgeM3,
}

impl ModelChoice {
    /// Parse the stable model id shared by configuration and CLI values.
    pub(crate) fn from_model_id(model: &str) -> Result<Self> {
        <Self as clap::ValueEnum>::from_str(model, false)
            .map_err(|error| anyhow::anyhow!("unsupported FastEmbed model {model:?}: {error}"))
    }

    pub fn model_id(self) -> &'static str {
        match self {
            Self::BgeSmallEnV15 => "bge-small-en-v1.5",
            Self::BgeM3 => "bge-m3",
        }
    }

    pub fn dimension(self) -> usize {
        match self {
            Self::BgeSmallEnV15 => 384,
            Self::BgeM3 => 1024,
        }
    }

    fn fastembed_model(self) -> EmbeddingModel {
        match self {
            Self::BgeSmallEnV15 => EmbeddingModel::BGESmallENV15,
            Self::BgeM3 => EmbeddingModel::BGEM3,
        }
    }

    /// 初回 DL サイズの目安 (ユーザ告知用)
    fn approx_download_mb(self) -> u32 {
        match self {
            Self::BgeSmallEnV15 => 130,
            Self::BgeM3 => 2300,
        }
    }

    /// fastembed の `embed()` に渡すバッチサイズ。1 回の推論の途中計算 (activation)
    /// はバッチに比例して増えるので、大きなモデルではバッチを絞る。
    ///
    /// `batch * max_length(=512) * hidden_dim * 4 bytes` は最終層の出力 1 枚分に
    /// すぎず、ONNX Runtime が推論中に確保する attention などの途中計算を数えていない。
    /// (kuriya 671) BGE-M3 は 32 x 512 token の batch で private bytes が約 3 GB から
    /// 約 5 GB に跳ねた。8 では約 2.5 GB で頭打ちになり、1 chunk あたりの時間は
    /// 32 と変わらなかった (起票時の実測)。
    pub fn batch_size(self) -> usize {
        match self {
            Self::BgeSmallEnV15 => 256,
            Self::BgeM3 => 8,
        }
    }
}

/// Resolved settings for the embedding provider used by one command.
///
/// The provider identity is resolved before construction so index compatibility
/// checks never need to contact an external endpoint.
#[derive(Clone, Debug)]
pub struct EmbeddingSettings {
    provider: ProviderSettings,
    identity: EmbeddingIdentity,
}

#[derive(Clone, Debug)]
enum ProviderSettings {
    FastEmbed(ModelChoice),
    OpenAiCompatible(OpenAiCompatibleConfig),
}

#[derive(Clone, Debug)]
struct EmbeddingIdentity {
    model_id: String,
    dimension: usize,
}

impl EmbeddingSettings {
    /// Build settings for the FastEmbed provider and its stable index identity.
    pub fn fastembed(choice: ModelChoice) -> Self {
        Self {
            provider: ProviderSettings::FastEmbed(choice),
            identity: EmbeddingIdentity {
                model_id: choice.model_id().to_string(),
                dimension: choice.dimension(),
            },
        }
    }

    /// Build settings for an OpenAI-compatible HTTP provider.
    pub fn openai_compatible(config: OpenAiCompatibleConfig) -> Self {
        Self {
            identity: EmbeddingIdentity {
                model_id: config.index_model_id.clone(),
                dimension: config.dimension,
            },
            provider: ProviderSettings::OpenAiCompatible(config),
        }
    }

    /// Stable identity recorded in `index_meta` and evaluation history.
    pub fn model_id(&self) -> &str {
        &self.identity.model_id
    }

    pub fn dimension(&self) -> usize {
        self.identity.dimension
    }
}

/// Fully resolved configuration for an OpenAI-compatible embedding endpoint.
#[derive(Clone)]
pub struct OpenAiCompatibleConfig {
    endpoint: String,
    endpoint_display: String,
    query_model: String,
    document_model: String,
    dimension: usize,
    request_dimensions: bool,
    api_key: Option<String>,
    timeout: Duration,
    index_model_id: String,
    /// Inputs are cut to this many characters before they are sent. `None`
    /// sends them whole (the default of [`OpenAiCompatibleConfig::new`];
    /// configuration sets 8000).
    max_input_chars: Option<usize>,
    /// How many times one batch is sent again after a transient failure.
    max_retries: u32,
    /// (AW-13) The endpoint is on this machine, so the client skips any proxy.
    endpoint_is_loopback: bool,
}

impl fmt::Debug for OpenAiCompatibleConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OpenAiCompatibleConfig")
            .field("endpoint", &self.endpoint_display)
            .field("query_model", &self.query_model)
            .field("document_model", &self.document_model)
            .field("dimension", &self.dimension)
            .field("request_dimensions", &self.request_dimensions)
            .field("api_key", &self.api_key.as_ref().map(|_| "[redacted]"))
            .field("timeout", &self.timeout)
            .field("max_input_chars", &self.max_input_chars)
            .field("max_retries", &self.max_retries)
            .field("endpoint_is_loopback", &self.endpoint_is_loopback)
            .finish()
    }
}

impl OpenAiCompatibleConfig {
    pub fn new(
        endpoint: String,
        query_model: String,
        document_model: String,
        dimension: usize,
        request_dimensions: bool,
        api_key: Option<String>,
        timeout: Duration,
    ) -> Result<Self> {
        let endpoint = endpoint.trim().to_string();
        let query_model = query_model.trim().to_string();
        let document_model = document_model.trim().to_string();
        let parsed = reqwest::Url::parse(&endpoint)
            .context("[embedding].endpoint must be a valid absolute URL")?;
        anyhow::ensure!(
            matches!(parsed.scheme(), "http" | "https"),
            "[embedding].endpoint must use http or https"
        );
        anyhow::ensure!(
            !parsed.has_authority()
                || (parsed.username().is_empty() && parsed.password().is_none()),
            "[embedding].endpoint must not contain credentials; use api_key or GROOVE_EMBEDDING_API_KEY"
        );
        let endpoint_display =
            format!("{}{}", parsed.origin().ascii_serialization(), parsed.path());
        let loopback = endpoint_is_loopback(&parsed);
        anyhow::ensure!(
            !query_model.trim().is_empty() && !document_model.trim().is_empty(),
            "[embedding] requires `model`, or both `query_model` and `document_model`, \
             for provider = \"openai-compatible\""
        );
        anyhow::ensure!(
            dimension > 0,
            "[embedding].dimension must be greater than zero"
        );
        anyhow::ensure!(
            !timeout.is_zero(),
            "[embedding].timeout_seconds must be greater than zero"
        );

        let api_key = api_key.filter(|key| !key.trim().is_empty());
        let mut hasher = Sha256::new();
        for value in [document_model.as_str(), query_model.as_str()] {
            hasher.update((value.len() as u64).to_le_bytes());
            hasher.update(value.as_bytes());
        }
        hasher.update((dimension as u64).to_le_bytes());
        let digest = format!("{:x}", hasher.finalize());
        let index_model_id = format!(
            "openai-compatible:{document_model}|{query_model}:{}",
            &digest[..12]
        );

        Ok(Self {
            endpoint,
            endpoint_display,
            query_model,
            document_model,
            dimension,
            request_dimensions,
            api_key,
            timeout,
            index_model_id,
            max_input_chars: None,
            max_retries: 0,
            endpoint_is_loopback: loopback,
        })
    }

    /// Set the input cap and the retry count (`[embedding] max_input_chars` /
    /// `max_retries`). Neither is part of the index identity.
    pub fn with_limits(mut self, max_input_chars: Option<usize>, max_retries: u32) -> Result<Self> {
        anyhow::ensure!(
            max_input_chars != Some(0),
            "[embedding].max_input_chars must be greater than zero"
        );
        anyhow::ensure!(
            max_retries <= MAX_EMBEDDING_RETRIES,
            "[embedding].max_retries must be at most {MAX_EMBEDDING_RETRIES}"
        );
        self.max_input_chars = max_input_chars;
        self.max_retries = max_retries;
        Ok(self)
    }
}

/// The internal provider boundary. Query and document embedding stay separate
/// so a later provider can preserve asymmetric retrieval without teaching the
/// indexer or search pipeline about that provider.
trait EmbeddingProvider: Send {
    fn embed_documents(&mut self, texts: &[&str]) -> Result<Vec<Vec<f32>>>;
    fn embed_query(&mut self, text: &str) -> Result<Vec<f32>>;

    /// Embed several queries, in order. The default asks
    /// [`EmbeddingProvider::embed_query`] once per text; a provider that can
    /// send its query side as a batch overrides it.
    fn embed_queries(&mut self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        texts.iter().map(|text| self.embed_query(text)).collect()
    }

    /// Check the provider answers before a forced rebuild empties the index
    /// (ADR-0024). A local model has nothing to reach, so the default does
    /// nothing.
    fn probe(&mut self) -> Result<()> {
        Ok(())
    }
}

/// The one call [`FastEmbedProvider`] makes of its model: embed `texts` with
/// fastembed's own batch size `batch`. Behind a trait so the tests can check
/// what reaches the model without loading one.
trait FastEmbedModel: Send {
    fn embed_batch(&mut self, texts: &[&str], batch: usize) -> Result<Vec<Vec<f32>>>;
}

impl FastEmbedModel for TextEmbedding {
    fn embed_batch(&mut self, texts: &[&str], batch: usize) -> Result<Vec<Vec<f32>>> {
        self.embed(texts, Some(batch))
    }
}

/// A local fastembed model. Every document and query is cut to
/// [`FASTEMBED_MAX_INPUT_CHARS`] characters, on a character boundary, before it
/// is tokenized ([`embed_truncated`]). Real text reaches the 512 tokens the
/// models keep long before that, so its vector is the same as without the cut.
/// What can change is a text the tokenizer turns into few tokens per character:
///
/// 1. BGE-small: a body made of runs of letters and digits longer than 100
///    characters with no space or punctuation (WordPiece turns each run into a
///    single `[UNK]` token).
/// 2. BGE-M3: a body with long runs of characters outside the vocabulary and no
///    space between them (Unigram's `fuse_unk` makes each run one `<unk>`).
/// 3. Both models: a body whose first [`FASTEMBED_MAX_INPUT_CHARS`] characters
///    are mostly spaces, line breaks or control characters.
///
/// Such a vector is not recomputed when it changes: feature-69's reuse key is
/// the text before the cut.
struct FastEmbedProvider<M: FastEmbedModel = TextEmbedding> {
    model: M,
    choice: ModelChoice,
}

impl FastEmbedProvider {
    fn new(choice: ModelChoice) -> Result<Self> {
        eprintln!(
            "Loading embedding model: {} ({} dim, ~{} MB on first run)...",
            choice.model_id(),
            choice.dimension(),
            choice.approx_download_mb()
        );
        let model = TextEmbedding::try_new(
            InitOptions::new(choice.fastembed_model())
                .with_cache_dir(resolve_cache_dir()?)
                .with_show_download_progress(true),
        )?;
        Ok(Self { model, choice })
    }
}

/// The most characters of one input handed to a fastembed model; the rest is
/// cut off before tokenizing (feature-70 spec R1-2). 256 Ki characters of real
/// prose run well past the 512 tokens the models keep, so the cut leaves what
/// they read alone.
const FASTEMBED_MAX_INPUT_CHARS: usize = 256 * 1024;

/// The most characters, summed over the inputs, handed to one fastembed call
/// (feature-70 spec R1-3). The tokenizer's memory for a call grows with the total
/// characters it is given, not with the tokens the model keeps; eight inputs at
/// [`FASTEMBED_MAX_INPUT_CHARS`] fit, so a reranker pair always does.
const FASTEMBED_MAX_CHARS_PER_CALL: usize = 8 * FASTEMBED_MAX_INPUT_CHARS;

/// Where a list of inputs with these `weights` is cut into model calls: one
/// greedy pass that closes the current slice when the next input would take
/// it past `max_items.max(1)` inputs or past `max_weight` in summed weight. An
/// input always joins an empty slice, so one weighing more than `max_weight`
/// gets a slice of its own. The ranges cover `0..weights.len()` in order,
/// without gaps, and none is empty.
fn slice_bounds(
    weights: &[usize],
    max_items: usize,
    max_weight: usize,
) -> Vec<std::ops::Range<usize>> {
    let max_items = max_items.max(1);
    let mut ranges = Vec::new();
    let mut start = 0;
    let mut sum = 0usize;
    for (i, &weight) in weights.iter().enumerate() {
        let count = i - start;
        if count > 0 && (count + 1 > max_items || sum.saturating_add(weight) > max_weight) {
            ranges.push(start..i);
            start = i;
            sum = 0;
        }
        sum = sum.saturating_add(weight);
    }
    if start < weights.len() {
        ranges.push(start..weights.len());
    }
    ranges
}

/// Embed `texts` one slice at a time, appending only the vectors. A slice holds
/// at most `batch` texts and, beyond the count, at most
/// [`FASTEMBED_MAX_CHARS_PER_CALL`] characters summed over its texts; where it
/// is cut is [`slice_bounds`]'s call.
///
/// (kuriya 671) fastembed's `TextEmbedding::embed` runs every batch of its
/// input first and keeps each batch's raw session outputs (the token-level
/// hidden states, about 2 MiB per 512-token chunk for BGE-M3) until the last
/// one is done, and only then pools them into vectors. Handing it one slice per
/// call lets those outputs go after each slice, so the memory no longer grows
/// with the number of chunks in a document.
fn embed_in_slices(
    texts: &[&str],
    batch: usize,
    mut embed_slice: impl FnMut(&[&str]) -> Result<Vec<Vec<f32>>>,
) -> Result<Vec<Vec<f32>>> {
    // Counting stops one past the bound: enough to tell that a text is over it.
    let weights: Vec<usize> = texts
        .iter()
        .map(|t| t.chars().take(FASTEMBED_MAX_CHARS_PER_CALL + 1).count())
        .collect();
    let mut embeddings = Vec::with_capacity(texts.len());
    for range in slice_bounds(&weights, batch, FASTEMBED_MAX_CHARS_PER_CALL) {
        let slice = &texts[range];
        let vectors = embed_slice(slice)?;
        anyhow::ensure!(
            vectors.len() == slice.len(),
            "embedding returned {} vectors for {} texts",
            vectors.len(),
            slice.len()
        );
        embeddings.extend(vectors);
    }
    Ok(embeddings)
}

/// Cut each of `texts` to [`FASTEMBED_MAX_INPUT_CHARS`] characters
/// ([`truncate_to_chars`]) and embed the result through [`embed_in_slices`],
/// so a call is bounded by count and by total characters as well.
fn embed_truncated(
    texts: &[&str],
    batch: usize,
    embed_slice: impl FnMut(&[&str]) -> Result<Vec<Vec<f32>>>,
) -> Result<Vec<Vec<f32>>> {
    let cut: Vec<&str> = texts
        .iter()
        .map(|t| truncate_to_chars(t, FASTEMBED_MAX_INPUT_CHARS))
        .collect();
    embed_in_slices(&cut, batch, embed_slice)
}

impl<M: FastEmbedModel> EmbeddingProvider for FastEmbedProvider<M> {
    fn embed_documents(&mut self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        let batch = self.choice.batch_size();
        let model = &mut self.model;
        embed_truncated(texts, batch, |slice| model.embed_batch(slice, batch))
    }

    fn embed_query(&mut self, text: &str) -> Result<Vec<f32>> {
        let mut embeddings = self.embed_documents(&[text])?;
        embeddings
            .pop()
            .ok_or_else(|| anyhow::anyhow!("embedding returned empty result"))
    }

    fn embed_queries(&mut self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        // One model serves both sides here (see `embed_query` above), so a
        // batch of queries is embedded exactly as a batch of documents.
        self.embed_documents(texts)
    }
}

#[derive(Serialize)]
struct OpenAiEmbeddingRequest<'a> {
    model: &'a str,
    input: &'a [&'a str],
    #[serde(skip_serializing_if = "Option::is_none")]
    dimensions: Option<usize>,
}

#[derive(Deserialize)]
struct OpenAiEmbeddingResponse {
    data: Vec<OpenAiEmbeddingItem>,
}

#[derive(Deserialize)]
struct OpenAiEmbeddingItem {
    embedding: Vec<f32>,
    index: usize,
}

struct OpenAiCompatibleProvider {
    client: Option<reqwest::blocking::Client>,
    config: OpenAiCompatibleConfig,
}

/// The one text [`Embedder::probe_before_reset`] sends. Fixed and ASCII, so the
/// probe carries nothing from the knowledge base.
pub const ENDPOINT_PROBE_TEXT: &str = "GrooveSeek endpoint probe";

const OPENAI_COMPATIBLE_BATCH_SIZE: usize = 64;
const MAX_HTTP_ERROR_BODY_BYTES: usize = 512;

/// The most `[embedding] max_retries` accepts.
pub const MAX_EMBEDDING_RETRIES: u32 = 10;

/// An OpenAI-compatible endpoint refused a batch because of what it held:
/// HTTP 400, 413 or 422. Not retried. The indexer downcasts to this and skips
/// the file instead of stopping the run; it reads [`EmbedInputRejected::status`]
/// and never this error's text, which carries the response body.
#[derive(Debug)]
pub struct EmbedInputRejected {
    pub status: u16,
    body_snippet: String,
}

impl fmt::Display for EmbedInputRejected {
    // Worded by `http_status_message`, as the untyped non-2xx error is, so a
    // CLI error chain reads the same whichever status it was (spec 3.1). The
    // indexer never prints this.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&http_status_message(self.status, &self.body_snippet))
    }
}

impl std::error::Error for EmbedInputRejected {}

/// Any other non-2xx answer (429, 5xx, 401, 403, 404, ...), worded the same
/// way. Typed so that [`body_free_message`] can name the status without the
/// body; the CLI prints this text, snippet included, to the operator.
#[derive(Debug)]
struct EmbedHttpStatus {
    status: u16,
    body_snippet: String,
}

impl fmt::Display for EmbedHttpStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&http_status_message(self.status, &self.body_snippet))
    }
}

impl std::error::Error for EmbedHttpStatus {}

/// An embedding error worded for a caller who must not see the endpoint's
/// response body -- an MCP client and the watcher's log (ADR-0025). Every
/// message in the chain is kept, joined with `: `, so the cause shows under
/// contexts such as the indexer's `failed to embed chunks for <path>` (AW-16).
/// Only the two kinds of layer that may carry a response body are cut short,
/// ending the message: a non-2xx answer ([`EmbedInputRejected`] or any other
/// status) becomes its status alone, and a `serde_json` error, which quotes
/// the value it could not read, becomes `malformed JSON (<category>)`.
/// Everything else passes through whole, including errors that have nothing
/// to do with embedding: OS and SQLite text, and the absolute paths a
/// directory walk names.
pub(crate) fn body_free_message(error: &anyhow::Error) -> String {
    let mut layers = Vec::new();
    for cause in error.chain() {
        let status = cause
            .downcast_ref::<EmbedInputRejected>()
            .map(|r| r.status)
            .or_else(|| cause.downcast_ref::<EmbedHttpStatus>().map(|s| s.status));
        if let Some(status) = status {
            layers.push(format!("embedding endpoint returned HTTP {status}"));
            break;
        }
        if let Some(json) = cause.downcast_ref::<serde_json::Error>() {
            let category = match json.classify() {
                serde_json::error::Category::Io => "io",
                serde_json::error::Category::Syntax => "syntax",
                serde_json::error::Category::Data => "data",
                serde_json::error::Category::Eof => "eof",
            };
            layers.push(format!("malformed JSON ({category})"));
            break;
        }
        layers.push(cause.to_string());
    }
    layers.join(": ")
}

/// (AW-04) Context on an [`EmbedInputRejected`] that came after the endpoint had
/// accepted earlier batches of the same call. The call still fails and the
/// indexer still finds the [`EmbedInputRejected`] underneath and skips the
/// file, but [`Embedder::embed_texts`] counts it apart: an accepted batch shows
/// the endpoint and model work, so the file is no sign of a wrong configuration.
#[derive(Debug)]
struct RejectedAfterAccepting {
    accepted_inputs: usize,
}

impl fmt::Display for RejectedAfterAccepting {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "the embedding endpoint accepted the first {} input(s) of this call, \
             then refused a later batch",
            self.accepted_inputs
        )
    }
}

/// The one wording of a non-2xx answer, typed ([`EmbedInputRejected`]) or not.
fn http_status_message(status: u16, snippet: &str) -> String {
    format!("embedding endpoint returned HTTP {status}: {snippet}")
}

/// How one attempt at a batch failed.
enum AttemptFailure {
    /// 429, 5xx, a timeout or a failed connection: worth sending again.
    Retryable {
        error: anyhow::Error,
        retry_after: Option<Duration>,
        /// `HTTP 503`, `timed out` or `connection failed`, for the final message.
        last: String,
    },
    /// Anything else, [`EmbedInputRejected`] included: returned as it is.
    Fatal(anyhow::Error),
}

/// How [`retry_loop`] waits: injected so tests record the waits instead of
/// sleeping them.
struct Backoff<'a> {
    sleep: &'a mut dyn FnMut(Duration),
    jitter: &'a mut dyn FnMut(Duration) -> Duration,
}

/// Run `attempt` until it succeeds, fails for good, or has been retried
/// `max_retries` times. With `max_retries == 0` the first error comes back
/// exactly as it was; otherwise a gave-up error is wrapped in a context that
/// names the attempts, the last error staying its source.
fn retry_loop<T>(
    max_retries: u32,
    backoff: &mut Backoff<'_>,
    mut attempt: impl FnMut() -> std::result::Result<T, AttemptFailure>,
) -> Result<T> {
    let mut attempts: u32 = 0;
    loop {
        attempts += 1;
        let (error, retry_after, last) = match attempt() {
            Ok(value) => return Ok(value),
            Err(AttemptFailure::Fatal(error)) => return Err(error),
            Err(AttemptFailure::Retryable {
                error,
                retry_after,
                last,
            }) => (error, retry_after, last),
        };
        if max_retries == 0 {
            return Err(error);
        }
        if attempts > max_retries {
            return Err(error.context(format!(
                "embedding endpoint still failing after {attempts} attempts (last: {last})"
            )));
        }
        match wait_before_retry(attempts, retry_after, backoff.jitter) {
            Wait::GiveUp(asked) => {
                return Err(error.context(format!(
                    "embedding endpoint asked to retry after {} s, more than the {} s groove \
                     waits; gave up after {attempts} attempt(s)",
                    asked.as_secs(),
                    MAX_RETRY_AFTER.as_secs()
                )));
            }
            Wait::After(wait) => (backoff.sleep)(wait),
        }
    }
}

/// A jitter in `[0, base / 4)` from the clock's sub-second nanoseconds: enough
/// to keep processes that failed together from retrying in step. Not random in
/// any stronger sense, and it need not be.
fn jitter_from_clock(base: Duration) -> Duration {
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    // Integer arithmetic, so no float rounding can land on the bound itself.
    let quarter = u128::from(u64::try_from((base / 4).as_nanos()).unwrap_or(u64::MAX));
    let jitter = quarter * u128::from(nanos) / 1_000_000_000;
    Duration::from_nanos(u64::try_from(jitter).unwrap_or(u64::MAX))
}

/// (AW-13) Whether `url` names this machine: 127.0.0.0/8, `::1` (also as
/// `::ffff:127.x.y.z`) or `localhost`. Such an endpoint is contacted directly,
/// because a proxy set for the outside world would otherwise receive the
/// document text and the API key meant for a local server.
///
/// The decision itself is the crate's one loopback-host predicate,
/// [`crate::transport::http::is_loopback_host`].
fn endpoint_is_loopback(url: &reqwest::Url) -> bool {
    use crate::transport::http::{is_loopback_host, normalize_host};
    let Some(host) = url.host_str() else {
        return false;
    };
    // A trailing dot is the fully qualified spelling of the same name, and the
    // resolver sends `localhost.` to this machine as well. The dot is dropped
    // here rather than in the shared predicate: its other callers judge
    // `Host` / `Origin` allow-list entries, which the HTTP gate compares
    // verbatim, so `localhost.` there does not admit a browser on
    // `localhost` and must not count as local.
    let host = host.strip_suffix('.').unwrap_or(host);
    is_loopback_host(&normalize_host(host))
}

impl OpenAiCompatibleProvider {
    fn new(config: OpenAiCompatibleConfig) -> Result<Self> {
        eprintln!(
            "Using OpenAI-compatible embedding models: document={} query={} ({} dim) at {}",
            config.document_model, config.query_model, config.dimension, config.endpoint_display
        );
        Ok(Self {
            client: None,
            config,
        })
    }

    fn client(&mut self) -> Result<&reqwest::blocking::Client> {
        if self.client.is_none() {
            let mut builder = reqwest::blocking::Client::builder()
                .timeout(self.config.timeout)
                .redirect(reqwest::redirect::Policy::none());
            if self.config.endpoint_is_loopback {
                // Drops both the proxy environment variables and the OS proxy.
                builder = builder.no_proxy();
            }
            self.client = Some(
                builder
                    .build()
                    .context("failed to build OpenAI-compatible embedding client")?,
            );
        }
        Ok(self.client.as_ref().expect("client initialized above"))
    }

    fn embed(&mut self, texts: &[&str], model: &str) -> Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        let texts: Vec<&str> = match self.config.max_input_chars {
            Some(max) => texts.iter().map(|t| truncate_to_chars(t, max)).collect(),
            None => texts.to_vec(),
        };

        let mut embeddings = Vec::with_capacity(texts.len());
        for batch in texts.chunks(OPENAI_COMPATIBLE_BATCH_SIZE) {
            match self.embed_batch(batch, model) {
                Ok(batch_embeddings) => embeddings.extend(batch_embeddings),
                Err(error) if !embeddings.is_empty() && error.is::<EmbedInputRejected>() => {
                    return Err(error.context(RejectedAfterAccepting {
                        accepted_inputs: embeddings.len(),
                    }));
                }
                Err(error) => return Err(error),
            }
        }
        Ok(embeddings)
    }

    /// One batch, sent again on a transient failure (see [`retry_loop`]).
    fn embed_batch(&mut self, texts: &[&str], model: &str) -> Result<Vec<Vec<f32>>> {
        let max_retries = self.config.max_retries;
        let mut sleep = std::thread::sleep;
        let mut jitter = jitter_from_clock;
        let mut backoff = Backoff {
            sleep: &mut sleep,
            jitter: &mut jitter,
        };
        retry_loop(max_retries, &mut backoff, || {
            self.send_batch_once(texts, model)
        })
    }

    /// Send one batch once and check the answer. How a failure is classified
    /// decides whether [`retry_loop`] sends it again.
    fn send_batch_once(
        &mut self,
        texts: &[&str],
        model: &str,
    ) -> std::result::Result<Vec<Vec<f32>>, AttemptFailure> {
        let request = OpenAiEmbeddingRequest {
            model,
            input: texts,
            dimensions: self
                .config
                .request_dimensions
                .then_some(self.config.dimension),
        };
        let endpoint = self.config.endpoint.clone();
        let api_key = self.config.api_key.clone();
        let dimension = self.config.dimension;
        let timeout = self.config.timeout;
        let client = self.client().map_err(AttemptFailure::Fatal)?;
        let mut builder = client.post(endpoint).json(&request);
        if let Some(api_key) = api_key {
            builder = builder.bearer_auth(api_key);
        }
        // (AW-11) One timeout for the attempt, headers and body together: taken before the
        // request, so the time the headers took is not given again to the body.
        let deadline = std::time::Instant::now().checked_add(timeout);
        let mut response = match builder.send() {
            Ok(response) => response,
            Err(error) => {
                let last = if error.is_timeout() {
                    Some("timed out")
                } else if error.is_connect() {
                    Some("connection failed")
                } else {
                    None
                };
                let error =
                    anyhow::anyhow!("embedding request failed: {}", transport_message(error));
                return Err(match last {
                    Some(last) => AttemptFailure::Retryable {
                        error,
                        retry_after: None,
                        last: last.to_string(),
                    },
                    None => AttemptFailure::Fatal(error),
                });
            }
        };
        let status = response.status().as_u16();
        let retry_after = response
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| parse_retry_after(value, SystemTime::now()));
        // Classified from the status line, before the body is read, and a body that stalls
        // or breaks off never changes that class (AW-04): a refusal stays a refusal (not a
        // retryable timeout that ends the run), a 429 / 5xx stays retryable, with its
        // `Retry-After`, even when an overloaded proxy cuts its body short (codex P2 round 2
        // on PR #329), and any other non-2xx stays fatal, so a 401 whose body stalls does not
        // send the key again (local Codex before round 3). For all of them the body only
        // feeds the snippet, left empty here.
        let class = classify_status(status);
        // A 429 / 5xx asking for more than `retry_loop` will wait is given up on as it stands:
        // reading its body first would only let a stalled one hold the run, and a daemon's
        // embedder, for the request timeout (codex P2 round 3 on PR #329).
        if class == StatusClass::Retryable && retry_after.is_some_and(|w| w > MAX_RETRY_AFTER) {
            return Err(AttemptFailure::Retryable {
                error: anyhow::Error::new(EmbedHttpStatus {
                    status,
                    body_snippet: escaped_body_snippet(&[]),
                }),
                retry_after,
                last: format!("HTTP {status}"),
            });
        }
        // (AW-11) A 2xx is read up to the cap its inputs and dimension allow, one byte more
        // telling that the cap was passed; a `Content-Length` over the cap is refused before
        // anything is read. A longer 2xx is fatal, like any other wrong 2xx: the same inputs
        // would bring the same answer again. Any other status is read only as far as its
        // snippet shows, the byte after it deciding the `...`.
        let body = if class == StatusClass::Success {
            let cap = max_embedding_response_bytes(texts.len(), dimension);
            match read_success_body(&mut response, cap, deadline, timeout) {
                Ok(body) => body,
                Err(SuccessBodyFailure::TooLarge) => {
                    return Err(AttemptFailure::Fatal(response_too_large(
                        cap,
                        texts.len(),
                        dimension,
                    )));
                }
                // A 2xx whose vectors never arrived: a timeout is retried, any other broken
                // body is not.
                Err(SuccessBodyFailure::Read(BodyReadFailure { error, timed_out })) => {
                    return Err(if timed_out {
                        AttemptFailure::Retryable {
                            error,
                            retry_after,
                            last: "timed out".to_string(),
                        }
                    } else {
                        AttemptFailure::Fatal(error)
                    });
                }
            }
        } else {
            let max_bytes = MAX_HTTP_ERROR_BODY_BYTES as u64 + 1;
            read_capped_body(&mut response, max_bytes, deadline, timeout).unwrap_or_default()
        };
        let http_error = || {
            anyhow::Error::new(EmbedHttpStatus {
                status,
                body_snippet: escaped_body_snippet(&body),
            })
        };
        match class {
            StatusClass::Success => {}
            StatusClass::InputRejected => {
                return Err(AttemptFailure::Fatal(anyhow::Error::new(
                    EmbedInputRejected {
                        status,
                        body_snippet: escaped_body_snippet(&body),
                    },
                )));
            }
            StatusClass::Retryable => {
                return Err(AttemptFailure::Retryable {
                    error: http_error(),
                    retry_after,
                    last: format!("HTTP {status}"),
                });
            }
            StatusClass::Fatal => return Err(AttemptFailure::Fatal(http_error())),
        }
        parse_embedding_response(&body, texts.len(), dimension).map_err(AttemptFailure::Fatal)
    }
}

impl EmbeddingProvider for OpenAiCompatibleProvider {
    fn embed_documents(&mut self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        let model = self.config.document_model.clone();
        self.embed(texts, &model)
    }

    fn embed_query(&mut self, text: &str) -> Result<Vec<f32>> {
        let model = self.config.query_model.clone();
        let mut embeddings = self.embed(&[text], &model)?;
        embeddings
            .pop()
            .ok_or_else(|| anyhow::anyhow!("embedding returned empty result"))
    }

    fn embed_queries(&mut self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        let model = self.config.query_model.clone();
        self.embed(texts, &model)
    }

    /// One document-side embed of [`ENDPOINT_PROBE_TEXT`], through the same
    /// response checks (status, count, dimension) indexing relies on.
    fn probe(&mut self) -> Result<()> {
        self.embed_documents(&[ENDPOINT_PROBE_TEXT]).map(drop)
    }
}

/// Check a 2xx answer and put its vectors in input order: one per input, each
/// index in range and unique, none missing, each of the declared dimension,
/// finite and not all zeros, and scaled to unit length
/// ([`normalize_embedding`]). Never retried: the server would give the same
/// answer again.
fn parse_embedding_response(body: &[u8], inputs: usize, dimension: usize) -> Result<Vec<Vec<f32>>> {
    let parsed: OpenAiEmbeddingResponse =
        serde_json::from_slice(body).context("embedding endpoint returned malformed JSON")?;
    anyhow::ensure!(
        parsed.data.len() == inputs,
        "embedding endpoint returned {} vectors for {} inputs",
        parsed.data.len(),
        inputs
    );

    let mut ordered: Vec<Option<Vec<f32>>> = vec![None; inputs];
    for mut item in parsed.data {
        anyhow::ensure!(
            item.index < ordered.len(),
            "embedding endpoint returned out-of-range index {} for {} inputs",
            item.index,
            ordered.len()
        );
        anyhow::ensure!(
            item.embedding.len() == dimension,
            "embedding endpoint returned dimension {} at index {}; expected {}",
            item.embedding.len(),
            item.index,
            dimension
        );
        normalize_embedding(&mut item.embedding, item.index)?;
        anyhow::ensure!(
            ordered[item.index].is_none(),
            "embedding endpoint returned duplicate index {}",
            item.index
        );
        ordered[item.index] = Some(item.embedding);
    }
    ordered
        .into_iter()
        .enumerate()
        .map(|(index, embedding)| {
            embedding.ok_or_else(|| anyhow::anyhow!("embedding endpoint omitted index {index}"))
        })
        .collect()
}

/// (AW-11 / AW-12) Refuse a vector with a non-finite value or no length, and
/// scale the rest to unit length, as FastEmbed's already are: the index
/// compares vectors by L2 distance and `groove graph` reads that distance as
/// a cosine, which only holds between unit vectors. The squares are summed in
/// f64, so no finite f32 vector overflows to infinity or underflows to zero on
/// the way. `index` is the vector's place in groove's input order.
fn normalize_embedding(embedding: &mut [f32], index: usize) -> Result<()> {
    anyhow::ensure!(
        embedding.iter().all(|x| x.is_finite()),
        "embedding endpoint returned a non-finite value at index {index}"
    );
    let norm = embedding
        .iter()
        .map(|&x| f64::from(x) * f64::from(x))
        .sum::<f64>()
        .sqrt();
    anyhow::ensure!(
        norm > 0.0,
        "embedding endpoint returned an all-zero vector at index {index}; \
         it has no direction to compare"
    );
    for x in embedding.iter_mut() {
        *x = (f64::from(*x) / norm) as f32;
    }
    Ok(())
}

/// Bytes allowed per float in [`max_embedding_response_bytes`]: the longest
/// float a server prints (`-1.2345678918743134e-05`, an f32 widened to f64)
/// is 23 bytes, 25 with its separator, about 42 when pretty-printed with
/// `indent=4` (estimated).
const RESPONSE_BYTES_PER_FLOAT: u64 = 64;
/// Bytes allowed per vector for its envelope (`{"object":"embedding","index":..,
/// "embedding":[..]}`, about 50 bytes compact) and any fields a server adds.
const RESPONSE_BYTES_PER_ITEM: u64 = 512;
/// Bytes allowed once per answer: `object`, a long `model`, `usage`, `id`.
const RESPONSE_BYTES_SLACK: u64 = 64 * 1024;
/// How much of a body one read asks for.
const RESPONSE_READ_CHUNK: usize = 64 * 1024;
/// The most [`max_embedding_response_bytes`] ever allows: 256 MiB. A full
/// batch of 64 at dimension 65,536 comes to about 268 MB, far beyond any real
/// model (dimension 3072 is about 12 MiB), and a cap below `u64::MAX` keeps
/// `cap + 1` -- the byte that tells an answer went over -- representable. A
/// saturated cap would otherwise read without limit.
const MAX_EMBEDDING_RESPONSE_BYTES_CEILING: u64 = 256 * 1024 * 1024;

/// (AW-11) The most groove reads of a 2xx answer to `inputs` inputs of
/// `dimension`: `inputs * (dimension * 64 + 512) + 64 KiB`, saturating, since
/// neither number has an upper bound of its own, and never more than
/// [`MAX_EMBEDDING_RESPONSE_BYTES_CEILING`]. JSON allows any amount of
/// whitespace, so without a cap a valid answer could be any size.
fn max_embedding_response_bytes(inputs: usize, dimension: usize) -> u64 {
    let inputs = u64::try_from(inputs).unwrap_or(u64::MAX);
    let dimension = u64::try_from(dimension).unwrap_or(u64::MAX);
    dimension
        .saturating_mul(RESPONSE_BYTES_PER_FLOAT)
        .saturating_add(RESPONSE_BYTES_PER_ITEM)
        .saturating_mul(inputs)
        .saturating_add(RESPONSE_BYTES_SLACK)
        .min(MAX_EMBEDDING_RESPONSE_BYTES_CEILING)
}

/// The error for a 2xx answer longer than
/// [`max_embedding_response_bytes`], whether its `Content-Length` said so or
/// its bytes did. It carries nothing from the body.
fn response_too_large(cap: u64, inputs: usize, dimension: usize) -> anyhow::Error {
    anyhow::anyhow!(
        "embedding endpoint answered with more than {cap} bytes for {inputs} input(s) of \
         dimension {dimension}; groove reads at most that much (check [embedding].dimension)"
    )
}

/// How [`read_capped_body`] failed.
struct BodyReadFailure {
    error: anyhow::Error,
    /// A read, or the body as a whole, ran past the request timeout.
    timed_out: bool,
}

/// How [`read_success_body`] failed.
enum SuccessBodyFailure {
    /// More than the cap was promised or sent.
    TooLarge,
    Read(BodyReadFailure),
}

/// (AW-11) Read a 2xx body of at most `cap` bytes. A `Content-Length` over
/// `cap` is refused before anything is read; otherwise one byte beyond `cap`
/// is asked for, and its arrival means the answer went over.
fn read_success_body(
    response: &mut reqwest::blocking::Response,
    cap: u64,
    deadline: Option<std::time::Instant>,
    timeout: Duration,
) -> std::result::Result<Vec<u8>, SuccessBodyFailure> {
    if response.content_length().is_some_and(|length| length > cap) {
        return Err(SuccessBodyFailure::TooLarge);
    }
    let body = read_capped_body(response, cap.saturating_add(1), deadline, timeout)
        .map_err(SuccessBodyFailure::Read)?;
    if body.len() as u64 > cap {
        return Err(SuccessBodyFailure::TooLarge);
    }
    Ok(body)
}

/// (AW-11) Read at most `max_bytes` of `response`'s body, stopping there; a
/// caller that wants to know whether more was sent asks for one byte beyond
/// its limit. `deadline` is the attempt's, taken before the request was sent
/// (`timeout` names it in the error): `reqwest`'s own timeout applies to each
/// read separately, so without it a server sending a byte at a time could
/// hold the embedder for as long as it kept sending. Headers and body share
/// the one timeout, so an attempt lasts at most about twice `timeout`
/// (estimated: the read under way when the deadline passes may wait a full
/// timeout of its own), as it did when the body was read with `bytes()`.
fn read_capped_body(
    response: &mut reqwest::blocking::Response,
    max_bytes: u64,
    deadline: Option<std::time::Instant>,
    timeout: Duration,
) -> std::result::Result<Vec<u8>, BodyReadFailure> {
    use std::io::Read;
    let mut limited = response.take(max_bytes);
    let mut body = Vec::new();
    let mut chunk = vec![0u8; RESPONSE_READ_CHUNK];
    loop {
        if deadline.is_some_and(|deadline| std::time::Instant::now() >= deadline) {
            return Err(BodyReadFailure {
                error: anyhow::anyhow!(
                    "failed to read embedding response body: timed out, the answer took longer \
                     than the {timeout:?} request timeout"
                ),
                timed_out: true,
            });
        }
        match limited.read(&mut chunk) {
            Ok(0) => return Ok(body),
            Ok(n) => body.extend_from_slice(&chunk[..n]),
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => return Err(body_read_failure(error)),
        }
    }
}

/// A failed read of a response body, worded as `bytes()` worded it. A read
/// through `reqwest`'s `Read` fails with the `reqwest::Error` inside an
/// `io::Error`; it is taken out so that its URL can be dropped and its timeout
/// seen.
fn body_read_failure(error: std::io::Error) -> BodyReadFailure {
    let kind_timed_out = error.kind() == std::io::ErrorKind::TimedOut;
    if error
        .get_ref()
        .is_some_and(|inner| inner.is::<reqwest::Error>())
    {
        let inner = *error
            .into_inner()
            .expect("get_ref was Some")
            .downcast::<reqwest::Error>()
            .expect("is::<reqwest::Error>() was true");
        let timed_out = kind_timed_out || inner.is_timeout();
        return BodyReadFailure {
            error: anyhow::anyhow!(
                "failed to read embedding response body: {}",
                transport_message(inner)
            ),
            timed_out,
        };
    }
    BodyReadFailure {
        error: anyhow::anyhow!(
            "failed to read embedding response body: {}",
            ascii_io_error(&error)
        ),
        timed_out: kind_timed_out,
    }
}

/// (AW-16) A `reqwest` error without its URL, followed by its causes joined
/// with `: `: its `Display` alone (`error sending request`) does not tell a
/// timeout from a refused connection. A nested `reqwest::Error` ends the
/// list, since its `Display` could name the URL again. Each cause is written
/// in ASCII ([`ascii_cause`]). The URL's path and query never appear, but a
/// cause may name the host (a TLS certificate that does not match the name,
/// for one).
fn transport_message(error: reqwest::Error) -> String {
    let error = error.without_url();
    with_ascii_causes(error.to_string(), std::error::Error::source(&error))
}

/// `message` (a URL-free `reqwest` error's own text) followed by `source` and
/// the causes under it, each through [`ascii_cause`], joined with `: `; a
/// nested `reqwest::Error` ends the list ([`transport_message`]).
fn with_ascii_causes(
    mut message: String,
    mut source: Option<&(dyn std::error::Error + 'static)>,
) -> String {
    while let Some(cause) = source {
        if cause.is::<reqwest::Error>() {
            break;
        }
        message.push_str(": ");
        message.push_str(&ascii_cause(cause));
        source = cause.source();
    }
    message
}

/// (codex P1 round 1 on PR #332) One cause of a transport error, in ASCII:
/// stderr stays ASCII (AGENTS.md). An `io::Error` is named by its kind and
/// OS error code ([`ascii_io_error`]); any other cause keeps its text,
/// escaped where it is not ASCII.
fn ascii_cause(cause: &(dyn std::error::Error + 'static)) -> String {
    match cause.downcast_ref::<std::io::Error>() {
        Some(io) => ascii_io_error(io),
        None => crate::watcher::ascii_diag(&cause.to_string()),
    }
}

/// An `io::Error` as `ConnectionRefused (os error 10061)`: its text comes
/// from the OS, which words it in the local language on a non-English
/// Windows, while the kind and code are the same everywhere. The writer
/// lease words its lock-file errors with it too.
pub(crate) fn ascii_io_error(error: &std::io::Error) -> String {
    match error.raw_os_error() {
        Some(code) => format!("{:?} (os error {code})", error.kind()),
        None => format!("{:?}", error.kind()),
    }
}

fn escaped_body_snippet(body: &[u8]) -> String {
    let mut snippet = String::new();
    for byte in body.iter().take(MAX_HTTP_ERROR_BODY_BYTES) {
        snippet.extend(std::ascii::escape_default(*byte).map(char::from));
    }
    if snippet.is_empty() {
        snippet.push_str("<empty>");
    } else if body.len() > MAX_HTTP_ERROR_BODY_BYTES {
        snippet.push_str("...");
    }
    snippet
}

/// The longest wait before a retry: the cap on the exponential backoff, and
/// the longest `Retry-After` honoured. A server asking for more is not waited
/// for; the request fails at once (spec AW-04).
const MAX_RETRY_AFTER: Duration = Duration::from_secs(60);

/// `text` cut to at most `max` characters (Unicode scalar values), on a
/// character boundary so that a multi-byte character is never split.
fn truncate_to_chars(text: &str, max: usize) -> &str {
    match text.char_indices().nth(max) {
        Some((byte, _)) => &text[..byte],
        None => text,
    }
}

/// A `Retry-After` value: delta-seconds, or an IMF-fixdate HTTP-date measured
/// from `now` (a date in the past is zero). A delta too large for `u64` is
/// `Duration::MAX`, longer than any wait honoured. `None` for anything else,
/// including the obsolete RFC 850 and asctime date forms: the caller then
/// falls back to its own backoff.
fn parse_retry_after(value: &str, now: SystemTime) -> Option<Duration> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    if value.bytes().all(|b| b.is_ascii_digit()) {
        return Some(
            value
                .parse::<u64>()
                .map(Duration::from_secs)
                .unwrap_or(Duration::MAX),
        );
    }
    let at = SystemTime::from(chrono::DateTime::parse_from_rfc2822(value).ok()?);
    Some(at.duration_since(now).unwrap_or(Duration::ZERO))
}

/// How one HTTP status is handled by the OpenAI-compatible provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StatusClass {
    Success,
    /// 400 / 413 / 422: the input was refused; not retried, and the indexer
    /// skips the file.
    InputRejected,
    /// 429 and 5xx: retried.
    Retryable,
    /// Every other status: not retried.
    Fatal,
}

fn classify_status(status: u16) -> StatusClass {
    match status {
        200..=299 => StatusClass::Success,
        400 | 413 | 422 => StatusClass::InputRejected,
        429 | 500..=599 => StatusClass::Retryable,
        _ => StatusClass::Fatal,
    }
}

/// The backoff before retry number `retry` (1-based) when the server named no
/// `Retry-After`: 1 s, 2 s, 4 s, ... capped at [`MAX_RETRY_AFTER`].
fn backoff_base(retry: u32) -> Duration {
    let secs = 1u64
        .checked_shl(retry.saturating_sub(1))
        .unwrap_or(u64::MAX);
    Duration::from_secs(secs).min(MAX_RETRY_AFTER)
}

#[derive(Debug, PartialEq, Eq)]
enum Wait {
    /// Sleep this long, then send again.
    After(Duration),
    /// The server asked for this much, more than [`MAX_RETRY_AFTER`]: give up now.
    GiveUp(Duration),
}

/// What to do before retry number `retry` (1-based). A `Retry-After` the
/// server sent wins and is waited exactly; without one, [`backoff_base`] plus
/// whatever `jitter` adds for that base.
fn wait_before_retry(
    retry: u32,
    retry_after: Option<Duration>,
    jitter: &mut dyn FnMut(Duration) -> Duration,
) -> Wait {
    match retry_after {
        Some(asked) if asked > MAX_RETRY_AFTER => Wait::GiveUp(asked),
        Some(asked) => Wait::After(asked),
        None => {
            let base = backoff_base(retry);
            Wait::After(base.saturating_add(jitter(base)))
        }
    }
}

/// Provider-neutral entry point for generating text embeddings.
pub struct Embedder {
    provider: Box<dyn EmbeddingProvider>,
    identity: EmbeddingIdentity,
    documents_embedded: u64,
    documents_refused_after_accepting: u64,
    texts_embedded: u64,
}

impl Embedder {
    /// デフォルトモデル ([`ModelChoice::default`]) で初期化する。
    ///
    /// Cache directory resolution (in order):
    /// 1. `FASTEMBED_CACHE_DIR` environment variable if set and non-empty
    ///    (must be absolute; an empty value counts as unset)
    /// 2. OS-standard cache directory joined with `fastembed`
    ///    (Linux: `~/.cache/fastembed`, macOS: `~/Library/Caches/fastembed`,
    ///    Windows: `%LOCALAPPDATA%\fastembed`)
    ///
    /// If neither names an absolute directory, this returns an error instead of
    /// loading a model relative to the working directory (see [`cache_dir_from`]).
    pub fn new() -> Result<Self> {
        Self::with_model(ModelChoice::default())
    }

    /// 明示的にモデルを指定して初期化する。
    pub fn with_model(choice: ModelChoice) -> Result<Self> {
        Self::with_settings(EmbeddingSettings::fastembed(choice))
    }

    /// Initialize the provider selected by resolved configuration.
    ///
    /// FastEmbed providers obtain their cache directory through
    /// [`resolve_cache_dir`].
    pub fn with_settings(settings: EmbeddingSettings) -> Result<Self> {
        let EmbeddingSettings { provider, identity } = settings;
        let provider: Box<dyn EmbeddingProvider> = match provider {
            ProviderSettings::FastEmbed(choice) => Box::new(FastEmbedProvider::new(choice)?),
            ProviderSettings::OpenAiCompatible(config) => {
                Box::new(OpenAiCompatibleProvider::new(config)?)
            }
        };
        Ok(Self::from_provider(provider, identity))
    }

    fn from_provider(provider: Box<dyn EmbeddingProvider>, identity: EmbeddingIdentity) -> Self {
        Self {
            provider,
            identity,
            documents_embedded: 0,
            documents_refused_after_accepting: 0,
            texts_embedded: 0,
        }
    }

    /// Embed document texts. Provider-specific batching stays behind the
    /// provider boundary.
    ///
    /// This is the document side: an OpenAI-compatible endpoint answers it with
    /// its `document_model`. Queries go through [`Embedder::embed_single`] or
    /// [`Embedder::embed_queries`].
    pub fn embed_texts(&mut self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        match self.provider.embed_documents(texts) {
            Ok(embeddings) => {
                self.documents_embedded += 1;
                self.texts_embedded += texts.len() as u64;
                Ok(embeddings)
            }
            Err(error) => {
                if error.is::<RejectedAfterAccepting>() {
                    self.documents_refused_after_accepting += 1;
                }
                Err(error)
            }
        }
    }

    /// How many [`Embedder::embed_texts`] calls have succeeded. The indexer
    /// calls it once per file it embeds, so the difference across a run is the
    /// number of files embedded (AW-04). Queries and the probe are not counted.
    pub(crate) fn documents_embedded(&self) -> u64 {
        self.documents_embedded
    }

    /// How many [`Embedder::embed_texts`] calls the endpoint refused only after
    /// accepting an earlier batch of the same call (AW-04). Such a file is
    /// skipped like any refused one, but it is not counted as a sign the
    /// configuration is wrong: the endpoint did answer.
    pub(crate) fn documents_refused_after_accepting(&self) -> u64 {
        self.documents_refused_after_accepting
    }

    /// (feature-69) How many inputs the successful [`Embedder::embed_texts`] calls were
    /// given, summed. The indexer passes one input per chunk it did not reuse, so the
    /// difference across a call for one document is the number of that document's chunks it
    /// embedded. Failed calls, queries and the probe are not counted. (kuriya 621) The indexer
    /// no longer reads it: its chunk counts come from each document's own plan, and a run-wide
    /// counter read across a call is what another caller in between would throw off. Only this
    /// module's tests read it now, and (AX-19) the benches under `benches/`, through the
    /// `test-helpers` feature, which own the embedder they read it from.
    #[cfg(any(test, feature = "test-helpers"))]
    pub fn texts_embedded(&self) -> u64 {
        self.texts_embedded
    }

    /// Embed one search query, on the query side of the provider (the
    /// `query_model` of an OpenAI-compatible endpoint). Documents go through
    /// [`Embedder::embed_texts`].
    pub fn embed_single(&mut self, text: &str) -> Result<Vec<f32>> {
        self.provider.embed_query(text)
    }

    /// Embed several search queries at once, in order, on the same query side
    /// as [`Embedder::embed_single`]. Use this for queries and
    /// [`Embedder::embed_texts`] for documents.
    ///
    /// The two sides are not interchangeable. FastEmbed embeds both with one
    /// model, but an OpenAI-compatible endpoint may name a separate
    /// `query_model` and `document_model`, and a query embedded as a document
    /// is not the vector a search compares against the index. Nothing errors;
    /// the results simply belong to a different query.
    pub fn embed_queries(&mut self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        self.provider.embed_queries(texts)
    }

    /// Before a forced rebuild empties the index, check the provider answers
    /// (ADR-0024). An OpenAI-compatible endpoint is sent one fixed text,
    /// [`ENDPOINT_PROBE_TEXT`], on the document side; FastEmbed does nothing.
    ///
    /// The error says nothing was removed from the index, because a caller
    /// runs this before its reset and returns on failure.
    ///
    /// That sentence is context and the provider's error stays its source:
    /// the CLI prints the whole chain, response body snippet included, while
    /// the MCP `rebuild_index {force: true}` reply goes through
    /// [`body_free_message`], which keeps the chain whole except the two
    /// layers that may carry the body: the HTTP status's and `serde_json`'s.
    pub fn probe_before_reset(&mut self) -> Result<()> {
        self.provider.probe().context(
            "the embedding endpoint check before the forced rebuild failed, \
             so nothing was removed from the index",
        )
    }

    /// 選択中のモデルの埋め込み次元数。
    pub fn dimension(&self) -> usize {
        self.identity.dimension
    }

    /// 選択中のモデルの識別子 (index_meta に記録される)。
    pub fn model_id(&self) -> &str {
        &self.identity.model_id
    }
}

fn resolve_cache_dir() -> Result<PathBuf> {
    cache_dir_from(std::env::var_os("FASTEMBED_CACHE_DIR"), dirs::cache_dir())
}

/// [`resolve_cache_dir`] の純粋部分。env を読む場所から切り離してあるのは、
/// テストが `set_var` を呼ばずに済ませるため (`cargo test` は同一プロセスの
/// 並列スレッドで走るので、env を触るテストは並走する全テストに影響する)。
///
/// **モデルの読み込み先が CWD 相対になることは無い** (BU-07)。ここは
/// 「どの `.onnx` を ONNX Runtime に渡すか」を決める場所で、hf-hub は
/// キャッシュに在るファイルを検証せずに使う。CWD は信頼できない
/// ディレクトリでありうる (clone したリポジトリ等) ので:
///
/// - 空の `FASTEMBED_CACHE_DIR` は**未設定として扱う**。`PathBuf::from("")` は
///   相対パスであり、そのまま返すと読み込み先が CWD になる
/// - **相対パスの `FASTEMBED_CACHE_DIR` は拒否する**。空文字だけを弾いても
///   `FASTEMBED_CACHE_DIR=.fastembed_cache` のような値が同じ結果 (CWD 起点) を
///   生むので、「CWD 相対にはならない」という保証が嘘になる
/// - どちらの候補も無い場合は**エラー**。以前はここで fastembed 既定の
///   `.fastembed_cache` (CWD 相対) に落ちていた。共有 temp の固定パスを
///   発明するのも同じ穴を別の形で開けるだけなので、**安全な場所を名指し
///   できないなら止まる**
fn cache_dir_from(env: Option<std::ffi::OsString>, os_cache: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(dir) = env
        && !dir.is_empty()
    {
        let dir = PathBuf::from(dir);
        anyhow::ensure!(
            dir.is_absolute(),
            "FASTEMBED_CACHE_DIR must be an absolute path, got {}.\n\
             A relative value resolves against the working directory, which may be a \
             directory you do not control; model files are loaded from it without \
             verification.",
            dir.display()
        );
        return Ok(dir);
    }
    if let Some(base) = os_cache {
        return Ok(base.join("fastembed"));
    }
    anyhow::bail!(
        "cannot determine a directory for embedding models: no usable FASTEMBED_CACHE_DIR and \
         no OS cache directory (HOME / XDG_CACHE_HOME / LOCALAPPDATA are all unset).\n\
         Set FASTEMBED_CACHE_DIR to a directory you control. It is not defaulted to a \
         working-directory-relative path, because model files are loaded from it without \
         verification."
    )
}

// ---------------------------------------------------------------------------
// Reranker
// ---------------------------------------------------------------------------

/// Cross-encoder reranker の選択肢。CLI `--reranker` と共有される。
///
/// デフォルトは `None` (reranker 無効)。モデル DL を避けるため、opt-in で
/// 明示的に選択する。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, clap::ValueEnum, Deserialize)]
pub enum RerankerChoice {
    // As with `ModelChoice`, each variant's doc comment is its line under
    // `--reranker` in `--help`, so it is written in English. That matters most
    // for `jina-v2-ml`: its license terms have to reach someone reading only
    // the help of a release archive.
    /// No reranker: the RRF hybrid ranking is returned as it is. The built-in
    /// default.
    #[default]
    #[value(name = "none")]
    #[serde(rename = "none")]
    None,
    /// BAAI/bge-reranker-v2-m3 (multilingual, 100+ languages, ~2.3 GB first
    /// download). Recommended for Japanese knowledge bases.
    #[value(name = "bge-v2-m3")]
    #[serde(rename = "bge-v2-m3")]
    BgeV2M3,
    /// jinaai/jina-reranker-v2-base-multilingual (multilingual, ~1.2 GB first
    /// download). A lighter multilingual alternative. Licensed CC-BY-NC-4.0:
    /// research and evaluation only, no commercial use.
    #[value(name = "jina-v2-ml")]
    #[serde(rename = "jina-v2-ml")]
    JinaV2Multilingual,
    /// BAAI/bge-reranker-base (English and Chinese only, ~280 MB first
    /// download). Not recommended for Japanese.
    #[value(name = "bge-base")]
    #[serde(rename = "bge-base")]
    BgeBase,
}

impl RerankerChoice {
    pub fn model_id(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::BgeV2M3 => "bge-reranker-v2-m3",
            Self::JinaV2Multilingual => "jina-reranker-v2-base-multilingual",
            Self::BgeBase => "bge-reranker-base",
        }
    }

    pub fn is_enabled(self) -> bool {
        !matches!(self, Self::None)
    }

    fn fastembed_model(self) -> Option<RerankerModel> {
        match self {
            Self::None => None,
            Self::BgeV2M3 => Some(RerankerModel::BGERerankerV2M3),
            Self::JinaV2Multilingual => Some(RerankerModel::JINARerankerV2BaseMultiligual),
            Self::BgeBase => Some(RerankerModel::BGERerankerBase),
        }
    }

    fn approx_download_mb(self) -> u32 {
        match self {
            Self::None => 0,
            Self::BgeV2M3 => 2300,
            Self::JinaV2Multilingual => 1200,
            Self::BgeBase => 280,
        }
    }
}

/// The context `r` is reranked with, or `None` when it has none worth
/// prepending (absent, empty or only whitespace). The one place that picks
/// the arm of [`contextualize_for_rerank`] and [`rerank_document_weight`].
fn rerank_context(r: &SearchResult) -> Option<&str> {
    match r.context_text.as_deref() {
        Some(ctx) if !ctx.trim().is_empty() => Some(ctx),
        _ => None,
    }
}

/// reranker 入力用に context を前置する (feature-46)。context_text が空/None なら
/// content のみ (off の DB / context なし chunk では従来と同一)。Anthropic 原典の
/// 結合形 `f"{context}\n\n{chunk}"` に忠実。
///
/// The result is cut to [`FASTEMBED_MAX_INPUT_CHARS`] characters. The content
/// is cut before it is joined, so a huge chunk is not copied whole on every
/// search, and the joined string is then cut again to the same bound.
fn contextualize_for_rerank(r: &SearchResult) -> String {
    match rerank_context(r) {
        Some(ctx) => {
            let mut joined = format!(
                "{ctx}\n\n{}",
                truncate_to_chars(&r.content, FASTEMBED_MAX_INPUT_CHARS)
            );
            let keep = truncate_to_chars(&joined, FASTEMBED_MAX_INPUT_CHARS).len();
            joined.truncate(keep);
            joined
        }
        None => truncate_to_chars(&r.content, FASTEMBED_MAX_INPUT_CHARS).to_string(),
    }
}

/// The character count of [`contextualize_for_rerank`]`(r)`, worked out
/// without building it: every count stops at [`FASTEMBED_MAX_INPUT_CHARS`], so
/// a huge chunk is never walked to its end.
fn rerank_document_weight(r: &SearchResult) -> usize {
    let cap = FASTEMBED_MAX_INPUT_CHARS;
    let content = r.content.chars().take(cap).count();
    match rerank_context(r) {
        Some(ctx) => (ctx.chars().take(cap).count() + 2 + content).min(cap),
        None => content,
    }
}

/// The most query-document pairs handed to one reranker call: fastembed's own
/// batch for `rerank` with `batch_size = None`, so its batches inside a call
/// break where they did before the calls were sliced.
const RERANK_SLICE_MAX_ITEMS: usize = 256;

/// The one call [`Reranker`] makes of its model: score each document against
/// the query, answering (index into the documents, score) pairs in any order.
/// Behind a trait so the tests can check what reaches the model without loading
/// one.
trait RerankModel: Send + Sync {
    fn rerank_slice(&mut self, query: &str, documents: &[&str]) -> Result<Vec<(usize, f32)>>;
}

impl RerankModel for TextRerank {
    fn rerank_slice(&mut self, query: &str, documents: &[&str]) -> Result<Vec<(usize, f32)>> {
        Ok(self
            .rerank(query, documents, false, None)?
            .into_iter()
            .map(|r| (r.index, r.score))
            .collect())
    }
}

/// Load the fastembed reranker `model` from the model cache, downloading it on
/// first use.
fn load_text_rerank(model: RerankerModel) -> Result<TextRerank> {
    TextRerank::try_new(
        RerankInitOptions::new(model)
            .with_cache_dir(resolve_cache_dir()?)
            .with_show_download_progress(true),
    )
}

/// Cross-encoder reranker。`search_hybrid` が返した候補を query との共同
/// エンコードで再スコア付けし、上位 `limit` 件に絞る。
///
/// The query and every candidate ([`contextualize_for_rerank`]) are cut to
/// [`FASTEMBED_MAX_INPUT_CHARS`] characters, on a character boundary, before
/// they are tokenized. The reranker's tokenizer (bge-reranker-v2-m3's and the
/// others') reads the query and the document as one pair and keeps far fewer
/// tokens than that, so real text scores the same as without the cut. What can
/// change is a pair the tokenizer turns into few tokens per character:
///
/// 1. A WordPiece tokenizer: runs of letters and digits longer than 100
///    characters with no space or punctuation (each run becomes a single
///    `[UNK]` token).
/// 2. A Unigram tokenizer: long runs of characters outside the vocabulary
///    with no space between them (`fuse_unk` makes each run one `<unk>`).
/// 3. Any tokenizer: a query or candidate whose first
///    [`FASTEMBED_MAX_INPUT_CHARS`] characters are mostly spaces, line breaks
///    or control characters.
///
/// One call of the model gets at most [`RERANK_SLICE_MAX_ITEMS`] pairs and at
/// most [`FASTEMBED_MAX_CHARS_PER_CALL`] characters, summed over the pairs with
/// the query counted once per pair (fastembed tokenizes it with each
/// document).
pub struct Reranker {
    model: Box<dyn RerankModel>,
    #[allow(dead_code)] // choice は model_id ログ用に保持
    choice: RerankerChoice,
}

impl Reranker {
    /// `choice == None` のときは `Ok(None)` を返す (DL・ロード共にスキップ)。
    /// それ以外は ONNX モデルをロードし `Some(Reranker)` を返す。
    pub fn try_new(choice: RerankerChoice) -> Result<Option<Self>> {
        let Some(fm) = choice.fastembed_model() else {
            return Ok(None);
        };
        eprintln!(
            "Loading reranker model: {} (~{} MB on first run)...",
            choice.model_id(),
            choice.approx_download_mb()
        );
        let model = load_text_rerank(fm)?;
        Ok(Some(Self {
            model: Box::new(model),
            choice,
        }))
    }

    /// `candidates` (chunk_id, [`crate::db::SearchResult`]) を cross-encoder でスコア付けし、
    /// 降順にソートした上位 `limit` 件の `SearchResult` を返す。
    /// `score` フィールドには reranker の raw score が入る (大きいほど良い)。
    pub fn rerank_candidates(
        &mut self,
        query: &str,
        candidates: Vec<(i64, SearchResult)>,
        limit: u32,
    ) -> Result<Vec<SearchResult>> {
        Ok(self
            .rerank_candidates_with_ids(query, candidates, limit)?
            .into_iter()
            .map(|(_, r)| r)
            .collect())
    }

    /// `rerank_candidates` と同じ結果を `(chunk_id, SearchResult)` で返す版。
    /// MMR の relevance 入力に chunk_id を保持したまま渡したいユースケース
    /// (feature-28 Task 2.9) で使う。`rerank_candidates` はこれに委譲する形に
    /// なっており、挙動は完全一致 (score / 順序とも同一)。
    pub fn rerank_candidates_with_ids(
        &mut self,
        query: &str,
        candidates: Vec<(i64, SearchResult)>,
        limit: u32,
    ) -> Result<Vec<(i64, SearchResult)>> {
        if candidates.is_empty() {
            return Ok(Vec::new());
        }
        let query = truncate_to_chars(query, FASTEMBED_MAX_INPUT_CHARS);
        // fastembed tokenizes the query with every document, so a pair weighs both.
        let query_chars = query.chars().count();
        let weights: Vec<usize> = candidates
            .iter()
            .map(|(_, r)| query_chars + rerank_document_weight(r))
            .collect();

        // (index into candidates, score) of every slice.
        let mut scored: Vec<(usize, f32)> = Vec::with_capacity(candidates.len());
        for range in slice_bounds(
            &weights,
            RERANK_SLICE_MAX_ITEMS,
            FASTEMBED_MAX_CHARS_PER_CALL,
        ) {
            // context 込みで rerank する (D-3)。合成文字列は slice ごとに作り、
            // 呼び出しが返ったら捨てる。
            let contextualized: Vec<String> = candidates[range.clone()]
                .iter()
                .map(|(_, r)| contextualize_for_rerank(r))
                .collect();
            let documents: Vec<&str> = contextualized.iter().map(String::as_str).collect();
            for (i, score) in self.model.rerank_slice(query, &documents)? {
                // An index past the slice is dropped before it is shifted, so it
                // can neither become a neighbour's candidate nor take a place
                // from `limit`.
                if i < range.len() {
                    scored.push((range.start + i, score));
                }
            }
        }

        // Score descending, ties in candidate order: what fastembed's stable
        // sort over one call gives, kept across slices.
        scored.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
        Ok(scored
            .into_iter()
            .take(limit as usize)
            .map(|(index, score)| {
                let (id, mut row) = candidates[index].clone();
                row.score = score;
                (id, row)
            })
            .collect())
    }
}

/// (#544 PR-2, J1) A provider that answers every input in process with one fixed vector of its
/// dimension, for the lib's unit tests of writes that embed:
/// the `Force` mode of `Reindex` in [`crate::indexer`] reads no stored vectors and embeds every
/// chunk, which the closed-port embedder of the indexer's tests cannot. (Not linked: `Reindex`
/// is private to that module, and rustdoc resolves no private item of it from here.)
#[cfg(any(test, feature = "test-helpers"))]
struct FixedVectorProvider {
    dimension: usize,
}

#[cfg(any(test, feature = "test-helpers"))]
impl FixedVectorProvider {
    /// A unit vector with every component equal, so its norm is one.
    fn vector(&self) -> Vec<f32> {
        let component = 1.0 / (self.dimension as f32).sqrt();
        vec![component; self.dimension]
    }
}

#[cfg(any(test, feature = "test-helpers"))]
impl EmbeddingProvider for FixedVectorProvider {
    fn embed_documents(&mut self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        Ok(texts.iter().map(|_| self.vector()).collect())
    }

    fn embed_query(&mut self, _text: &str) -> Result<Vec<f32>> {
        Ok(self.vector())
    }
}

#[cfg(any(test, feature = "test-helpers"))]
impl Embedder {
    /// (#544 PR-2, J1) An embedder that uses no network and answers each input with the same
    /// vector, as long as the dimension it is given, under the identity `test-model` the indexer's test databases
    /// declare. Its counts move as the real ones do: [`Embedder::embed_texts`] is the same code.
    ///
    /// (AX-19) Public under the `test-helpers` feature for the benches under `benches/`, which
    /// time the indexer and the watcher without a model download or an endpoint in the loop.
    pub fn stub_for_tests(dimension: usize) -> Self {
        Self::from_provider(
            Box::new(FixedVectorProvider { dimension }),
            EmbeddingIdentity {
                model_id: "test-model".to_string(),
                dimension,
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex, mpsc};
    use std::thread;

    struct StubProvider;

    impl EmbeddingProvider for StubProvider {
        fn embed_documents(&mut self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
            Ok(texts.iter().map(|_| vec![1.0, 2.0]).collect())
        }

        fn embed_query(&mut self, _text: &str) -> Result<Vec<f32>> {
            Ok(vec![3.0, 4.0])
        }
    }

    /// How [`mock_embedding_server`]'s one connection ended, returned by its
    /// thread so that joining the thread is the wait for it.
    #[derive(Debug, PartialEq, Eq)]
    enum MockEnd {
        /// The request arrived and the answer was written (or tried).
        Answered,
        /// The client closed the connection (end of stream, or a reset or
        /// aborted connection) before its headers were whole: a client
        /// timeout that fires after the connect but before the request is
        /// written does this, under load (kuriya issue #494).
        ClientClosed,
    }

    fn mock_embedding_server(
        status: &str,
        response_body: &str,
        delay: Duration,
    ) -> (String, mpsc::Receiver<String>, thread::JoinHandle<MockEnd>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock server");
        let addr = listener.local_addr().expect("mock server address");
        let status = status.to_string();
        let response_body = response_body.to_string();
        let (tx, rx) = mpsc::channel();
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept mock request");
            let Some(request) = try_read_http_request(&mut stream) else {
                return MockEnd::ClientClosed;
            };
            tx.send(request).expect("send captured request");
            thread::sleep(delay);
            write_http_response(&mut stream, &status, &response_body);
            MockEnd::Answered
        });
        (format!("http://{addr}/v1/embeddings"), rx, handle)
    }

    fn read_http_request(stream: &mut std::net::TcpStream) -> String {
        try_read_http_request(stream).expect("request ended before its headers")
    }

    /// Reads one request, or `None` when the client closed the connection
    /// before the headers were whole: a read of `Ok(0)`, as Windows showed a
    /// client's close (kuriya issue #308 saw the same), or a read error of
    /// kind `ConnectionReset` or `ConnectionAborted`, as another OS may show
    /// it. Any other read error still panics.
    fn try_read_http_request(stream: &mut impl Read) -> Option<String> {
        let mut request = Vec::new();
        let mut buf = [0_u8; 4096];
        let (header_end, content_length) = loop {
            let read = match stream.read(&mut buf) {
                Ok(0) => return None,
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::ConnectionAborted
                    ) =>
                {
                    return None;
                }
                read => read.expect("read mock request"),
            };
            request.extend_from_slice(&buf[..read]);
            if let Some(end) = request.windows(4).position(|window| window == b"\r\n\r\n") {
                let end = end + 4;
                let headers = String::from_utf8_lossy(&request[..end]);
                let len = headers
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().expect("content length"))
                    })
                    .unwrap_or(0);
                break (end, len);
            }
        };
        while request.len() < header_end + content_length {
            let read = stream.read(&mut buf).expect("read mock body");
            if read == 0 {
                break;
            }
            request.extend_from_slice(&buf[..read]);
        }
        Some(String::from_utf8_lossy(&request).into_owned())
    }

    fn write_http_response(stream: &mut std::net::TcpStream, status: &str, body: &str) {
        let response = format!(
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let _ = stream.write_all(response.as_bytes());
    }

    fn mock_batching_server(
        request_count: usize,
        dimension: usize,
    ) -> (String, mpsc::Receiver<String>, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock server");
        let addr = listener.local_addr().expect("mock server address");
        let (tx, rx) = mpsc::channel();
        let handle = thread::spawn(move || {
            for _ in 0..request_count {
                let (mut stream, _) = listener.accept().expect("accept mock request");
                let request = read_http_request(&mut stream);
                let (_, body) = request.split_once("\r\n\r\n").expect("HTTP body");
                let body: serde_json::Value = serde_json::from_str(body).expect("request JSON");
                let input_count = body["input"].as_array().expect("input array").len();
                let data: Vec<serde_json::Value> = (0..input_count)
                    .map(|index| {
                        serde_json::json!({
                            "embedding": (0..dimension)
                                .map(|d| if d == 0 { 1.0 } else { index as f32 + 1.0 })
                                .collect::<Vec<f32>>(),
                            "index": index,
                        })
                    })
                    .collect();
                tx.send(request).expect("send captured request");
                write_http_response(
                    &mut stream,
                    "200 OK",
                    &serde_json::json!({ "data": data }).to_string(),
                );
            }
        });
        (format!("http://{addr}/v1/embeddings"), rx, handle)
    }

    fn openai_config(
        endpoint: String,
        dimension: usize,
        timeout: Duration,
    ) -> OpenAiCompatibleConfig {
        openai_config_with_dimensions(endpoint, dimension, false, timeout)
    }

    fn openai_config_with_dimensions(
        endpoint: String,
        dimension: usize,
        request_dimensions: bool,
        timeout: Duration,
    ) -> OpenAiCompatibleConfig {
        OpenAiCompatibleConfig::new(
            endpoint,
            "query-model".to_string(),
            "document-model".to_string(),
            dimension,
            request_dimensions,
            Some("test-key".to_string()),
            timeout,
        )
        .expect("valid OpenAI-compatible config")
    }

    /// An absolute path for the current platform. `/models` is NOT absolute on
    /// Windows — it is drive-relative, so it still depends on process state —
    /// which is exactly why the check uses `is_absolute()` rather than
    /// `has_root()`.
    fn abs(tail: &str) -> PathBuf {
        if cfg!(windows) {
            PathBuf::from("C:\\").join(tail)
        } else {
            PathBuf::from("/").join(tail)
        }
    }

    #[test]
    fn fastembed_settings_keep_the_existing_index_identity() {
        for choice in [ModelChoice::BgeSmallEnV15, ModelChoice::BgeM3] {
            let settings = EmbeddingSettings::fastembed(choice);
            assert_eq!(settings.model_id(), choice.model_id());
            assert_eq!(settings.dimension(), choice.dimension());
        }
    }

    #[test]
    fn resolved_fastembed_settings_accept_an_existing_index() {
        let db = crate::db::Database::open_in_memory().unwrap();
        db.verify_embedding_meta("bge-small-en-v1.5", 384).unwrap();

        let same = EmbeddingSettings::fastembed(ModelChoice::BgeSmallEnV15);
        db.verify_embedding_meta(same.model_id(), same.dimension() as u32)
            .expect("the provider refactor must keep an existing FastEmbed index compatible");

        let different = EmbeddingSettings::fastembed(ModelChoice::BgeM3);
        assert!(
            db.verify_embedding_meta(different.model_id(), different.dimension() as u32)
                .is_err(),
            "a different model and dimension must still require a rebuild"
        );
    }

    #[test]
    fn embedder_routes_documents_and_queries_through_the_provider_boundary() {
        let settings = EmbeddingSettings::fastembed(ModelChoice::BgeSmallEnV15);
        let mut embedder =
            Embedder::from_provider(Box::new(StubProvider), settings.identity.clone());

        assert_eq!(
            embedder.embed_texts(&["one", "two"]).unwrap(),
            vec![vec![1.0, 2.0], vec![1.0, 2.0]]
        );
        assert_eq!(embedder.embed_single("query").unwrap(), vec![3.0, 4.0]);
        assert_eq!(embedder.model_id(), "bge-small-en-v1.5");
        assert_eq!(embedder.dimension(), 384);
    }

    /// `groove tune` has to embed its golden queries the way `groove search`
    /// embeds a query: on the query side of the provider. It embedded them as
    /// documents, which for an OpenAI-compatible endpoint configured with its
    /// own `query_model` is a different model, so the sweep measured a vector
    /// space no search runs in, and said nothing about it (AW-05).
    ///
    /// This sits with the embedder's tests rather than tune's because the stub
    /// provider is here, and the stub is what makes the side visible: it
    /// answers every query with one vector and every document with another.
    #[test]
    fn tune_embeds_golden_queries_on_the_query_side() {
        let settings = EmbeddingSettings::fastembed(ModelChoice::BgeSmallEnV15);
        let mut embedder =
            Embedder::from_provider(Box::new(StubProvider), settings.identity.clone());
        let query = |id: &str, text: &str, expected: &[&str]| crate::eval::GoldenQuery {
            id: Some(id.to_string()),
            query: text.to_string(),
            expected: expected
                .iter()
                .map(|path| crate::eval::ExpectedHit {
                    path: path.to_string(),
                    heading: None,
                })
                .collect(),
            tags: None,
        };
        let golden = crate::eval::GoldenSet {
            defaults: None,
            queries: vec![
                query("q1", "vector search", &["a.md"]),
                // No expected hit, so tune drops it before embedding anything.
                query("q2", "unlabelled", &[]),
                query("q3", "fusion -draft", &["b.md"]),
            ],
        };

        let embeddings = crate::tune::embed_usable_queries(&mut embedder, &golden)
            .expect("embed the golden queries");

        assert_eq!(
            embeddings,
            vec![vec![3.0, 4.0], vec![3.0, 4.0]],
            "tune must embed each usable golden query on the query side \
             (the stub answers queries with [3, 4] and documents with [1, 2])"
        );
    }

    #[test]
    fn openai_compatible_can_be_constructed_inside_a_tokio_runtime() {
        let runtime = tokio::runtime::Runtime::new().expect("build tokio runtime");
        runtime.block_on(async {
            let config = openai_config(
                "http://127.0.0.1:1/v1/embeddings".to_string(),
                2,
                Duration::from_secs(1),
            );
            let embedder = Embedder::with_settings(EmbeddingSettings::openai_compatible(config))
                .expect("provider construction must not create a blocking client");
            assert_eq!(embedder.dimension(), 2);
        });
    }

    #[test]
    fn openai_compatible_embeds_on_first_call_inside_a_tokio_runtime() {
        // codex review round 2 on PR #316: constructing the provider inside
        // `block_on` was not enough — the blocking `reqwest::Client` is built
        // lazily on the *first* `embed` call. The file watcher's synchronous
        // `handle_events` used to make that first call directly on a tokio
        // worker and panicked; `watcher.rs::run_watch_loop` now runs it
        // through `spawn_blocking` instead, which is the pattern under test
        // here. Calling `embed_texts` straight from the `block_on` body
        // (without `spawn_blocking`) still panics, as it must, since that is
        // the misuse this test guards against regressing back to.
        let response = r#"{"data":[{"embedding":[1.0,2.0],"index":0}]}"#;
        let (endpoint, captured, handle) =
            mock_embedding_server("200 OK", response, Duration::ZERO);
        let runtime = tokio::runtime::Runtime::new().expect("build tokio runtime");
        runtime.block_on(async {
            tokio::task::spawn_blocking(move || {
                let mut embedder = Embedder::with_settings(EmbeddingSettings::openai_compatible(
                    openai_config(endpoint, 2, Duration::from_secs(1)),
                ))
                .expect("provider construction must not create a blocking client");
                embedder
                    .embed_texts(&["first"])
                    .expect("embed must succeed once the client build runs on the blocking pool");
            })
            .await
            .expect("handle_events-style spawn_blocking task must not panic");
        });
        captured.recv().expect("captured request");
        handle.join().expect("mock server thread");
    }

    #[test]
    fn openai_compatible_serializes_batches_and_restores_response_order() {
        let response =
            r#"{"data":[{"embedding":[3.0,4.0],"index":1},{"embedding":[1.0,2.0],"index":0}]}"#;
        let (endpoint, captured, handle) =
            mock_embedding_server("200 OK", response, Duration::ZERO);
        let mut embedder = Embedder::with_settings(EmbeddingSettings::openai_compatible(
            openai_config(endpoint, 2, Duration::from_secs(1)),
        ))
        .expect("build embedder");

        let vectors = embedder
            .embed_texts(&["first", "second"])
            .expect("embed batch");
        assert_eq!(vectors, vec![vec![0.4472136, 0.8944272], vec![0.6, 0.8]]);
        let request = captured.recv().expect("captured request");
        let request_lower = request.to_ascii_lowercase();
        assert!(request_lower.starts_with("post /v1/embeddings http/1.1"));
        assert!(request_lower.contains("authorization: bearer test-key"));
        let (_, body) = request.split_once("\r\n\r\n").expect("HTTP body");
        let body: serde_json::Value = serde_json::from_str(body).expect("request JSON");
        assert_eq!(body["model"], "document-model");
        assert_eq!(body["input"], serde_json::json!(["first", "second"]));
        assert!(
            body.get("dimensions").is_none(),
            "dimensions is opt-in for compatibility with servers that reject it"
        );
        handle.join().expect("mock server thread");
    }

    #[test]
    fn openai_compatible_sends_dimensions_when_requested() {
        let response = r#"{"data":[{"embedding":[1.0,2.0],"index":0}]}"#;
        let (endpoint, captured, handle) =
            mock_embedding_server("200 OK", response, Duration::ZERO);
        let config = openai_config_with_dimensions(endpoint, 2, true, Duration::from_secs(1));
        let mut embedder = Embedder::with_settings(EmbeddingSettings::openai_compatible(config))
            .expect("build embedder");

        embedder.embed_single("needle").expect("embed query");
        let request = captured.recv().expect("captured request");
        let (_, body) = request.split_once("\r\n\r\n").expect("HTTP body");
        let body: serde_json::Value = serde_json::from_str(body).expect("request JSON");
        assert_eq!(body["dimensions"], 2);
        handle.join().expect("mock server thread");
    }

    #[test]
    fn openai_compatible_splits_large_batches() {
        let texts: Vec<String> = (0..129).map(|index| format!("text-{index}")).collect();
        let refs: Vec<&str> = texts.iter().map(String::as_str).collect();
        let (endpoint, captured, handle) = mock_batching_server(3, 2);
        let mut embedder = Embedder::with_settings(EmbeddingSettings::openai_compatible(
            openai_config(endpoint, 2, Duration::from_secs(1)),
        ))
        .expect("build embedder");

        let vectors = embedder.embed_texts(&refs).expect("embed large batch");
        assert_eq!(vectors.len(), 129);
        assert_eq!(vectors[63], vec![0.015623093, 0.9998779]);
        assert_eq!(vectors[64], vec![0.70710677, 0.70710677]);
        assert_eq!(vectors[128], vec![0.70710677, 0.70710677]);

        let batch_sizes: Vec<usize> = (0..3)
            .map(|_| {
                let request = captured.recv().expect("captured request");
                let (_, body) = request.split_once("\r\n\r\n").expect("HTTP body");
                serde_json::from_str::<serde_json::Value>(body).expect("request JSON")["input"]
                    .as_array()
                    .expect("input array")
                    .len()
            })
            .collect();
        assert_eq!(batch_sizes, vec![64, 64, 1]);
        handle.join().expect("mock server thread");
    }

    #[test]
    fn openai_compatible_uses_query_model_for_single_queries() {
        let response = r#"{"data":[{"embedding":[1.0,2.0],"index":0}]}"#;
        let (endpoint, captured, handle) =
            mock_embedding_server("200 OK", response, Duration::ZERO);
        let mut embedder = Embedder::with_settings(EmbeddingSettings::openai_compatible(
            openai_config(endpoint, 2, Duration::from_secs(1)),
        ))
        .expect("build embedder");

        embedder.embed_single("needle").expect("embed query");
        let request = captured.recv().expect("captured request");
        let (_, body) = request.split_once("\r\n\r\n").expect("HTTP body");
        let body: serde_json::Value = serde_json::from_str(body).expect("request JSON");
        assert_eq!(body["model"], "query-model");
        assert_eq!(body["input"], serde_json::json!(["needle"]));
        handle.join().expect("mock server thread");
    }

    /// Several queries go to the endpoint's `query_model` as one request, the
    /// way several documents go to its `document_model` (AW-05: `groove tune`
    /// embeds its whole golden set this way).
    #[test]
    fn openai_compatible_uses_query_model_for_query_batches() {
        let response =
            r#"{"data":[{"embedding":[3.0,4.0],"index":1},{"embedding":[1.0,2.0],"index":0}]}"#;
        let (endpoint, captured, handle) =
            mock_embedding_server("200 OK", response, Duration::ZERO);
        let mut embedder = Embedder::with_settings(EmbeddingSettings::openai_compatible(
            openai_config(endpoint, 2, Duration::from_secs(1)),
        ))
        .expect("build embedder");

        let vectors = embedder
            .embed_queries(&["first", "second"])
            .expect("embed query batch");
        assert_eq!(vectors, vec![vec![0.4472136, 0.8944272], vec![0.6, 0.8]]);
        let request = captured.recv().expect("captured request");
        let (_, body) = request.split_once("\r\n\r\n").expect("HTTP body");
        let body: serde_json::Value = serde_json::from_str(body).expect("request JSON");
        assert_eq!(body["model"], "query-model");
        assert_eq!(body["input"], serde_json::json!(["first", "second"]));
        handle.join().expect("mock server thread");
    }

    #[test]
    fn openai_compatible_rejects_invalid_response_shapes() {
        for (response, expected) in [
            (
                r#"{"data":[{"embedding":[1.0],"index":0},{"embedding":[3.0,4.0],"index":1}]}"#,
                "expected 2",
            ),
            (
                r#"{"data":[{"embedding":[1.0,2.0],"index":0},{"embedding":[3.0,4.0],"index":0}]}"#,
                "duplicate index 0",
            ),
            (
                r#"{"data":[{"embedding":[1.0,2.0],"index":2},{"embedding":[3.0,4.0],"index":0}]}"#,
                "out-of-range index 2",
            ),
            (
                r#"{"data":[{"embedding":[1.0,2.0],"index":0}]}"#,
                "1 vectors for 2 inputs",
            ),
        ] {
            let (endpoint, _captured, handle) =
                mock_embedding_server("200 OK", response, Duration::ZERO);
            let mut embedder = Embedder::with_settings(EmbeddingSettings::openai_compatible(
                openai_config(endpoint, 2, Duration::from_secs(1)),
            ))
            .expect("build embedder");
            let err = embedder
                .embed_texts(&["first", "second"])
                .expect_err("invalid response must fail");
            assert!(
                err.to_string().contains(expected),
                "expected {expected:?}, got {err}"
            );
            handle.join().expect("mock server thread");
        }
    }

    #[test]
    fn openai_compatible_reports_http_malformed_timeout_and_unavailable() {
        let cases = [
            (
                "503 Service Unavailable",
                "retry\nlater",
                Duration::ZERO,
                "retry\\nlater",
            ),
            ("200 OK", "not-json", Duration::ZERO, "malformed JSON"),
            (
                "200 OK",
                r#"{"data":[{"embedding":[1.0,2.0],"index":0}]}"#,
                Duration::from_millis(100),
                "embedding request failed",
            ),
        ];
        for (status, response, delay, expected) in cases {
            let (endpoint, _captured, handle) = mock_embedding_server(status, response, delay);
            let timeout = if delay.is_zero() {
                Duration::from_secs(1)
            } else {
                Duration::from_millis(10)
            };
            let mut embedder = Embedder::with_settings(EmbeddingSettings::openai_compatible(
                openai_config(endpoint, 2, timeout),
            ))
            .expect("build embedder");
            let err = embedder
                .embed_single("needle")
                .expect_err("request must fail");
            assert!(
                err.to_string().contains(expected),
                "expected {expected:?}, got {err}"
            );
            handle.join().expect("mock server thread");
        }

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind unavailable endpoint");
        let endpoint = format!(
            "http://{}/v1/embeddings",
            listener.local_addr().expect("listener address")
        );
        let handle = thread::spawn(move || {
            let (stream, _) = listener.accept().expect("accept unavailable request");
            drop(stream);
        });
        let mut embedder = Embedder::with_settings(EmbeddingSettings::openai_compatible(
            openai_config(endpoint, 2, Duration::from_millis(100)),
        ))
        .expect("build embedder");
        let _err = embedder
            .embed_single("needle")
            .expect_err("unavailable endpoint must fail");
        handle.join().expect("unavailable endpoint thread");
    }

    #[test]
    fn http_error_body_snippet_is_bounded_and_escaped() {
        assert_eq!(
            escaped_body_snippet(b"bad\n\tresponse"),
            "bad\\n\\tresponse"
        );
        let long = vec![b'x'; MAX_HTTP_ERROR_BODY_BYTES + 100];
        let snippet = escaped_body_snippet(&long);
        assert_eq!(snippet.len(), MAX_HTTP_ERROR_BODY_BYTES + 3);
        assert!(snippet.ends_with("..."));
    }

    /// AW-11: the response cap is wide enough for the widest answer a real
    /// server plausibly sends -- Python's `json.dumps(..., indent=4)` of floats
    /// widened to f64, each printed with 17 significant digits and an exponent
    /// -- for the largest batch at a large dimension, and it neither overflows
    /// nor grows past its ceiling.
    ///
    /// Red if the per-float allowance drops to 32, if the input count is left
    /// out of the product, if the arithmetic stops saturating (it panics), or
    /// if the ceiling is not applied.
    #[test]
    fn embedding_response_cap_covers_pretty_printed_worst_case_floats() {
        let (inputs, dimension) = (OPENAI_COMPATIBLE_BATCH_SIZE, 3072);
        let values = vec!["                -1.2345678918743134e-05"; dimension].join(",\n");
        let data: Vec<String> = (0..inputs)
            .map(|index| {
                format!(
                    "        {{\n            \"object\": \"embedding\",\n            \
                     \"index\": {index},\n            \"embedding\": [\n{values}\n            \
                     ]\n        }}"
                )
            })
            .collect();
        let body = format!(
            "{{\n    \"object\": \"list\",\n    \"data\": [\n{}\n    ],\n    \"model\": \"{}\",\n    \
             \"usage\": {{\n        \"prompt_tokens\": 8191000,\n        \
             \"total_tokens\": 8191000\n    }}\n}}",
            data.join(",\n"),
            "m".repeat(256)
        );
        let cap = max_embedding_response_bytes(inputs, dimension);
        assert!(body.len() as u64 <= cap, "{} bytes > cap {cap}", body.len());
        // The body is an answer groove accepts, not only a byte count.
        let vectors = parse_embedding_response(body.as_bytes(), inputs, dimension)
            .expect("the worst-case body parses");
        assert_eq!(vectors.len(), inputs);

        assert_eq!(max_embedding_response_bytes(64, 1024), 4_292_608);
        assert_eq!(max_embedding_response_bytes(1, 16), 67_072);
        // Past the ceiling the cap stops growing, and one byte beyond it still
        // fits in a u64: a saturated cap would never see that byte arrive.
        let ceiling = max_embedding_response_bytes(usize::MAX, usize::MAX);
        assert_eq!(ceiling, MAX_EMBEDDING_RESPONSE_BYTES_CEILING);
        assert!(ceiling.checked_add(1).is_some());
    }

    /// AW-11: the cap is inclusive. A 2xx of exactly `cap` bytes is read whole,
    /// its `Content-Length` (equal to the cap) passing the check made before
    /// reading; one byte more is refused.
    ///
    /// Red if either comparison against the cap becomes `>=`.
    #[test]
    fn a_2xx_body_of_exactly_the_cap_is_read_and_one_byte_more_is_refused() {
        let cap = 64u64;
        let timeout = Duration::from_secs(5);
        let response = |len: u64| -> reqwest::blocking::Response {
            http::Response::new(vec![b' '; len as usize]).into()
        };

        let mut exact = response(cap);
        assert_eq!(exact.content_length(), Some(cap));
        let deadline = std::time::Instant::now().checked_add(timeout);
        match read_success_body(&mut exact, cap, deadline, timeout) {
            Ok(body) => assert_eq!(body.len() as u64, cap),
            Err(SuccessBodyFailure::TooLarge) => panic!("a body of exactly the cap was refused"),
            Err(SuccessBodyFailure::Read(failure)) => panic!("{:#}", failure.error),
        }

        let mut over = response(cap + 1);
        let deadline = std::time::Instant::now().checked_add(timeout);
        assert!(matches!(
            read_success_body(&mut over, cap, deadline, timeout),
            Err(SuccessBodyFailure::TooLarge)
        ));
    }

    /// AW-11: a value too large for f32 is refused, never stored. serde_json
    /// narrows an f64 to f32 without complaint, so `1e39` arrives as infinity
    /// (pinned here, since the check exists because of it); `1e400` does not
    /// fit f64 either and is already malformed JSON. Neither message carries
    /// the body.
    ///
    /// Red if the finite check is removed.
    #[test]
    fn parse_embedding_response_rejects_non_finite_values() {
        assert_eq!(
            serde_json::from_str::<f32>("1e39").expect("1e39 reads as f32"),
            f32::INFINITY
        );
        let sentinel = "SENTINEL-AW11-3f9a";
        for value in ["1e39", "-1e39"] {
            let body = format!(
                r#"{{"data":[{{"embedding":[1.0,2.0],"index":0}},{{"embedding":[{value},0.0],"index":1}}],"model":"{sentinel}"}}"#
            );
            let err = parse_embedding_response(body.as_bytes(), 2, 2).expect_err(value);
            let text = format!("{err:#}");
            assert!(
                text.contains("embedding endpoint returned a non-finite value at index 1"),
                "{value}: {text}"
            );
            assert!(!text.contains(sentinel), "{text}");
        }
        let body =
            format!(r#"{{"data":[{{"embedding":[1e400,0.0],"index":0}}],"model":"{sentinel}"}}"#);
        let err = parse_embedding_response(body.as_bytes(), 1, 2).expect_err("1e400");
        let text = format!("{err:#}");
        assert!(text.contains("malformed JSON"), "{text}");
        assert!(!text.contains(sentinel), "{text}");
    }

    /// AW-11: a vector of zeros has no direction, so no distance to it means
    /// anything. `1e-50` is not zero in the JSON but becomes zero as f32.
    ///
    /// Red if the zero check is removed (the vector would come back as NaN).
    #[test]
    fn parse_embedding_response_rejects_an_all_zero_vector() {
        for zeros in ["0.0,0.0", "1e-50,0"] {
            let body = format!(
                r#"{{"data":[{{"embedding":[1.0,2.0],"index":0}},{{"embedding":[{zeros}],"index":1}}]}}"#
            );
            let err = parse_embedding_response(body.as_bytes(), 2, 2).expect_err(zeros);
            let text = format!("{err:#}");
            assert!(
                text.contains(
                    "embedding endpoint returned an all-zero vector at index 1; \
                     it has no direction to compare"
                ),
                "{zeros}: {text}"
            );
        }
    }

    /// AW-12: every vector an endpoint returns is stored at unit length, as
    /// FastEmbed's are, whatever its scale. The squares are summed in f64: in
    /// f32, `1e30` squared is infinity and `1e-30` squared is zero.
    ///
    /// Red if the scaling is removed, or if the sum is taken in f32.
    #[test]
    fn parse_embedding_response_scales_vectors_to_unit_length() {
        let body = br#"{"data":[{"embedding":[3.0,4.0],"index":0},{"embedding":[1e30,1e30],"index":1},{"embedding":[1e-30,1e-30],"index":2}]}"#;
        let vectors = parse_embedding_response(body, 3, 2).expect("scaled vectors");
        let expected = [
            [0.6, 0.8],
            [0.70710677, 0.70710677],
            [0.70710677, 0.70710677],
        ];
        for (vector, expected) in vectors.iter().zip(expected) {
            assert!(
                vector
                    .iter()
                    .zip(expected)
                    .all(|(got, want)| (got - want).abs() < 1e-6),
                "{vectors:?}"
            );
        }
    }

    /// AW-12: the query side is scaled too. A query compared against unit
    /// document vectors must be a unit vector itself.
    ///
    /// Red if the scaling runs for document embeds only.
    #[test]
    fn openai_compatible_normalizes_query_vectors_too() {
        let response = r#"{"data":[{"embedding":[3.0,4.0],"index":0}]}"#;
        let (endpoint, _captured, handle) =
            mock_embedding_server("200 OK", response, Duration::ZERO);
        let mut embedder = Embedder::with_settings(EmbeddingSettings::openai_compatible(
            openai_config(endpoint, 2, Duration::from_secs(1)),
        ))
        .expect("build embedder");
        assert_eq!(
            embedder.embed_single("needle").expect("embed query"),
            vec![0.6, 0.8]
        );
        handle.join().expect("mock server thread");

        let response =
            r#"{"data":[{"embedding":[3.0,4.0],"index":0},{"embedding":[30.0,40.0],"index":1}]}"#;
        let (endpoint, _captured, handle) =
            mock_embedding_server("200 OK", response, Duration::ZERO);
        let mut embedder = Embedder::with_settings(EmbeddingSettings::openai_compatible(
            openai_config(endpoint, 2, Duration::from_secs(1)),
        ))
        .expect("build embedder");
        assert_eq!(
            embedder
                .embed_queries(&["first", "second"])
                .expect("embed queries"),
            vec![vec![0.6, 0.8], vec![0.6, 0.8]]
        );
        handle.join().expect("mock server thread");
    }

    /// AW-12: the scaling belongs to the OpenAI-compatible provider alone.
    /// FastEmbed already returns unit vectors, and scaling them again in the
    /// [`Embedder`] would change their last bits; a provider's vectors pass
    /// through the wrapper exactly as the provider returned them.
    ///
    /// Red if the scaling moves into the [`Embedder`] wrapper.
    #[test]
    fn embedder_leaves_a_provider_vector_as_the_provider_returned_it() {
        let settings = EmbeddingSettings::fastembed(ModelChoice::BgeSmallEnV15);
        let mut embedder =
            Embedder::from_provider(Box::new(StubProvider), settings.identity.clone());
        assert_eq!(
            embedder.embed_texts(&["one"]).expect("embed document"),
            vec![vec![1.0, 2.0]]
        );
        assert_eq!(
            embedder.embed_single("query").expect("embed query"),
            vec![3.0, 4.0]
        );
    }

    /// AW-11: the request timeout bounds the whole body, not each read. A
    /// server trickling a valid answer a few bytes at a time, each read inside
    /// the timeout, must not hold the embedder for longer.
    ///
    /// Red if the whole-body deadline is dropped: every read succeeds, and the
    /// answer arrives whole after many times the timeout.
    #[test]
    fn openai_compatible_bounds_the_whole_body_read_by_the_timeout() {
        let timeout = Duration::from_secs(1);
        let body = r#"{"data":[{"embedding":[3.0,4.0],"index":0}]}"#;
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind trickling server");
        let endpoint = format!(
            "http://{}/v1/embeddings",
            listener.local_addr().expect("listener address")
        );
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept request");
            let _request = read_http_request(&mut stream);
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            if stream.write_all(head.as_bytes()).is_err() {
                return;
            }
            for piece in body.as_bytes().chunks(2) {
                thread::sleep(timeout / 2);
                if stream
                    .write_all(piece)
                    .and_then(|()| stream.flush())
                    .is_err()
                {
                    return;
                }
            }
        });
        let mut embedder = Embedder::with_settings(EmbeddingSettings::openai_compatible(
            openai_config(endpoint, 2, timeout),
        ))
        .expect("build embedder");
        let started = std::time::Instant::now();
        let result = embedder.embed_single("needle");
        let took = started.elapsed();
        let err = result.expect_err("a body trickled past the timeout must fail");
        let text = format!("{err:#}");
        assert!(text.contains("timed out"), "{text}");
        assert!(took < timeout * 3, "took {took:?}");
        drop(embedder);
        handle.join().expect("trickling server thread");
    }

    /// AW-11: one attempt shares a single timeout between waiting for the
    /// headers and reading the body, so it stays within about twice the
    /// timeout (the last read may wait a full one), as `bytes()` did. A body
    /// deadline started only once the headers arrived would add the header
    /// wait on top: up to about three times.
    ///
    /// The server holds the headers for 0.6 T, then sends two bytes at
    /// 0.85 T, 1.35 T, 1.85 T, ... (every T / 2, a quarter of a period off the
    /// deadlines so that no chunk lands on one). With the deadline taken
    /// before the request, it passes at 1 T and the read returning at 1.35 T
    /// ends the attempt; started after the headers, it passes at 1.6 T and
    /// the attempt ends at 1.85 T. The bound, 1.6 T, sits half a second (at
    /// T = 2 s) from both, which a loaded CI runner on any OS should not
    /// cross. T is 2 s rather than 1 s to buy that margin; the header wait,
    /// 1.2 s, stays 0.8 s inside the timeout `send` applies.
    ///
    /// Red if the body deadline is taken after `send` returns.
    #[test]
    fn openai_compatible_counts_the_header_wait_against_the_body_deadline() {
        let timeout = Duration::from_secs(2);
        let body = r#"{"data":[{"embedding":[3.0,4.0],"index":0}]}"#;
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind slow server");
        let endpoint = format!(
            "http://{}/v1/embeddings",
            listener.local_addr().expect("listener address")
        );
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept request");
            let _request = read_http_request(&mut stream);
            thread::sleep(timeout.mul_f64(0.6));
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            if stream.write_all(head.as_bytes()).is_err() {
                return;
            }
            thread::sleep(timeout / 4);
            for piece in body.as_bytes().chunks(2) {
                if stream
                    .write_all(piece)
                    .and_then(|()| stream.flush())
                    .is_err()
                {
                    return;
                }
                thread::sleep(timeout / 2);
            }
        });
        let mut embedder = Embedder::with_settings(EmbeddingSettings::openai_compatible(
            openai_config(endpoint, 2, timeout),
        ))
        .expect("build embedder");
        let started = std::time::Instant::now();
        let result = embedder.embed_single("needle");
        let took = started.elapsed();
        let err = result.expect_err("a body trickled past the timeout must fail");
        let text = format!("{err:#}");
        assert!(text.contains("timed out"), "{text}");
        assert!(took < timeout.mul_f64(1.6), "took {took:?}");
        drop(embedder);
        handle.join().expect("slow server thread");
    }

    #[test]
    fn openai_compatible_does_not_follow_a_redirect() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind redirect server");
        let endpoint = format!(
            "http://{}/v1/embeddings",
            listener.local_addr().expect("listener address")
        );
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept request");
            let _request = read_http_request(&mut stream);
            stream
                .write_all(
                    b"HTTP/1.1 307 Temporary Redirect\r\nLocation: http://127.0.0.1:1/v1/embeddings\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .expect("write redirect");
        });
        let mut embedder = Embedder::with_settings(EmbeddingSettings::openai_compatible(
            openai_config(endpoint, 2, Duration::from_secs(1)),
        ))
        .expect("build embedder");
        let err = embedder
            .embed_single("private document text")
            .expect_err("redirect must be rejected");
        assert!(err.to_string().contains("HTTP 307"), "{err}");
        handle.join().expect("redirect server thread");
    }

    #[test]
    fn openai_compatible_empty_batch_does_not_make_a_request() {
        let config = openai_config(
            "http://127.0.0.1:1/v1/embeddings".to_string(),
            2,
            Duration::from_millis(10),
        );
        let mut embedder = Embedder::with_settings(EmbeddingSettings::openai_compatible(config))
            .expect("build embedder");
        assert!(embedder.embed_texts(&[]).expect("empty batch").is_empty());
    }

    #[test]
    fn openai_compatible_identity_covers_both_models_and_dimension() {
        let make = |query: &str, document: &str, dimension: usize, request_dimensions: bool| {
            EmbeddingSettings::openai_compatible(
                OpenAiCompatibleConfig::new(
                    "http://127.0.0.1:8001/v1/embeddings".to_string(),
                    query.to_string(),
                    document.to_string(),
                    dimension,
                    request_dimensions,
                    None,
                    Duration::from_secs(1),
                )
                .expect("valid config"),
            )
        };
        let original = make("query", "document", 2, false);
        assert_ne!(
            original.model_id(),
            make("other", "document", 2, false).model_id()
        );
        assert_ne!(
            original.model_id(),
            make("query", "other", 2, false).model_id()
        );
        assert_ne!(
            original.model_id(),
            make("query", "document", 3, false).model_id()
        );
        assert_eq!(
            original.model_id(),
            make("query", "document", 2, true).model_id()
        );
        assert!(original.model_id().starts_with("openai-compatible:"));
        assert_eq!(original.dimension(), 2);
    }

    #[test]
    fn openai_compatible_diagnostics_omit_endpoint_query_and_fragment() {
        let config = OpenAiCompatibleConfig::new(
            "http://127.0.0.1:8001/v1/embeddings?token=secret#fragment".to_string(),
            "query".to_string(),
            "document".to_string(),
            2,
            false,
            None,
            Duration::from_secs(1),
        )
        .expect("valid config");
        let debug = format!("{config:?}");
        assert!(debug.contains("http://127.0.0.1:8001/v1/embeddings"));
        assert!(!debug.contains("token=secret"));
        assert!(!debug.contains("fragment"));
    }

    /// AW-13: only a loopback endpoint skips the proxy. A name that merely
    /// starts with `localhost` or `127.0.0.1` resolves elsewhere, so it keeps
    /// whatever proxy the environment sets.
    #[test]
    fn endpoint_is_loopback_accepts_only_loopback_hosts() {
        for endpoint in [
            "http://127.0.0.1:8001/v1/embeddings",
            "http://127.1.2.3/v1/embeddings",
            "https://127.255.255.254/",
            "http://[::1]:8001/v1/embeddings",
            "http://[::ffff:127.0.0.1]:8001/",
            "http://localhost:8001/v1/embeddings",
            "http://LocalHost/v1/embeddings",
            "http://localhost.:8001/",
        ] {
            let url = reqwest::Url::parse(endpoint).expect("valid url");
            assert!(endpoint_is_loopback(&url), "{endpoint} must be loopback");
        }
        for endpoint in [
            "http://localhost.example.com/v1/embeddings",
            "http://127.0.0.1.nip.io/v1/embeddings",
            "http://10.0.0.1:8001/v1/embeddings",
            "http://128.0.0.1/",
            "http://[::2]/",
            "http://[::ffff:10.0.0.1]/",
            "https://example.com/v1/embeddings",
            "http://mylocalhost/",
        ] {
            let url = reqwest::Url::parse(endpoint).expect("valid url");
            assert!(
                !endpoint_is_loopback(&url),
                "{endpoint} must not be loopback"
            );
        }
    }

    fn openai_config_for_endpoint(endpoint: &str) -> Result<OpenAiCompatibleConfig> {
        OpenAiCompatibleConfig::new(
            endpoint.to_string(),
            "query".to_string(),
            "document".to_string(),
            2,
            false,
            None,
            Duration::from_secs(1),
        )
    }

    /// AW-07: the endpoint is where document text is sent, so only HTTP(S) is
    /// accepted. Nothing else exercised this check.
    #[test]
    fn openai_compatible_rejects_a_non_http_endpoint() {
        for endpoint in [
            "ftp://127.0.0.1:8001/v1/embeddings",
            "file:///v1/embeddings",
            "ws://127.0.0.1:8001/v1/embeddings",
        ] {
            let err = openai_config_for_endpoint(endpoint)
                .expect_err("a non-HTTP endpoint must be rejected");
            assert!(
                err.to_string()
                    .contains("[embedding].endpoint must use http or https"),
                "{endpoint}: {err}"
            );
        }
    }

    /// AW-07: credentials in the endpoint would ride along in every request
    /// and every diagnostic that prints the URL. A user name alone and a
    /// password alone are each refused, not only the pair, and the refusal
    /// does not echo the password back.
    #[test]
    fn openai_compatible_rejects_credentials_in_the_endpoint() {
        // The label, not the endpoint, goes into a failure message, so a
        // failing run does not print the password either.
        for (case, endpoint) in [
            ("user name only", "http://user@127.0.0.1:8001/v1/embeddings"),
            (
                "password only",
                "http://:hunter2@127.0.0.1:8001/v1/embeddings",
            ),
            (
                "user name and password",
                "https://user:hunter2@example.com/v1/embeddings",
            ),
        ] {
            let Err(err) = openai_config_for_endpoint(endpoint) else {
                panic!("{case}: must be rejected");
            };
            let msg = err.to_string();
            assert!(
                !msg.contains("hunter2"),
                "{case}: the error echoes the password"
            );
            assert!(
                msg.contains("[embedding].endpoint must not contain credentials"),
                "{case}: {msg}"
            );
        }
    }

    /// AW-07: each role needs its own model. The config tests only leave out
    /// `query_model`; a blank `document_model` has to fail as well.
    #[test]
    fn openai_compatible_requires_both_models() {
        for (query, document) in [
            ("query", ""),
            ("query", "   "),
            ("", "document"),
            ("\t", "document"),
        ] {
            let err = OpenAiCompatibleConfig::new(
                "http://127.0.0.1:8001/v1/embeddings".to_string(),
                query.to_string(),
                document.to_string(),
                2,
                false,
                None,
                Duration::from_secs(1),
            )
            .expect_err("a blank model must be rejected");
            assert!(
                err.to_string()
                    .contains("both `query_model` and `document_model`"),
                "query={query:?} document={document:?}: {err}"
            );
        }
    }

    /// AW-07: a blank key must not become `Authorization: Bearer   `. Config
    /// resolution filters blank keys too, which hid this filter from the
    /// end-to-end test, so the constructor is called directly here.
    #[test]
    fn openai_compatible_drops_a_blank_api_key() {
        let with_key = |api_key: &str| {
            OpenAiCompatibleConfig::new(
                "http://127.0.0.1:8001/v1/embeddings".to_string(),
                "query".to_string(),
                "document".to_string(),
                2,
                false,
                Some(api_key.to_string()),
                Duration::from_secs(1),
            )
            .expect("valid config")
        };
        for blank in ["", "   ", "\t\n"] {
            assert_eq!(with_key(blank).api_key, None, "{blank:?}");
        }
        assert_eq!(with_key("test-key").api_key.as_deref(), Some("test-key"));
    }

    /// AW-07: a failed request must not print the endpoint URL, which can
    /// carry a secret in its query string.
    ///
    /// The request fails by connection refused: the port was bound and then
    /// released, so nothing listens there and no server has to be kept
    /// alive or waited on. If another process takes the port in between, the
    /// call either still fails while sending (the check below still holds)
    /// or gets an answer and fails some other way, which fails this test; a
    /// taken port cannot make it pass without exercising the send path.
    #[test]
    fn openai_compatible_refused_connection_errors_omit_the_endpoint() {
        let addr = TcpListener::bind("127.0.0.1:0")
            .expect("reserve a port")
            .local_addr()
            .expect("reserved address");
        let mut embedder =
            Embedder::with_settings(EmbeddingSettings::openai_compatible(openai_config(
                format!("http://{addr}/v1/embeddings?token=query-secret"),
                2,
                Duration::from_secs(5),
            )))
            .expect("build embedder");

        let err = embedder
            .embed_single("needle")
            .expect_err("nothing listens on a released port");
        let msg = err.to_string();
        // Checked before any assert that prints the message.
        assert!(!msg.contains("query-secret"), "the error echoes the query");
        assert!(
            !msg.contains(&addr.to_string()),
            "the error echoes the host and port"
        );
        assert!(!msg.contains("/v1/embeddings"), "the error echoes the path");
        assert!(msg.contains("embedding request failed"), "{msg}");
    }

    /// (BU-07) `PathBuf::from("")` is a *relative* path, so returning it makes
    /// the model directory the process's working directory — which is the
    /// directory a planted config is trying to get models loaded from. An
    /// empty variable is not a directory, so it falls through as if unset.
    #[test]
    fn an_empty_cache_dir_variable_is_treated_as_unset() {
        let os_cache = abs("os/cache");

        assert_eq!(
            cache_dir_from(Some(std::ffi::OsString::from("")), Some(os_cache.clone())).unwrap(),
            os_cache.join("fastembed"),
            "an empty variable must not become a cwd-relative model directory"
        );
        // With no OS cache either there is nothing safe left to name, so this
        // stops instead of falling back to the cwd-relative `.fastembed_cache`.
        let err = cache_dir_from(Some(std::ffi::OsString::from("")), None)
            .expect_err("no safe model directory can be named");
        assert!(
            err.to_string().contains("FASTEMBED_CACHE_DIR"),
            "the error must name the remedy: {err}"
        );
        // A real absolute value still wins over the OS cache.
        let models = abs("models");
        assert_eq!(
            cache_dir_from(
                Some(std::ffi::OsString::from(models.as_os_str())),
                Some(os_cache)
            )
            .unwrap(),
            models
        );
    }

    /// Emptiness was only half the rule. A non-empty *relative* override —
    /// `FASTEMBED_CACHE_DIR=.fastembed_cache` is the natural one to write —
    /// resolves against the working directory just the same, which would make
    /// the guarantee above false.
    #[test]
    fn a_relative_cache_dir_override_is_rejected() {
        for value in [".fastembed_cache", "models", "../shared-cache"] {
            let err = cache_dir_from(Some(std::ffi::OsString::from(value)), Some(abs("os/cache")))
                .expect_err("a relative override resolves against the working directory");
            assert!(
                err.to_string().contains("absolute"),
                "the error must say what is wrong with {value:?}: {err}"
            );
        }
    }

    #[test]
    #[ignore] // requires model download (~23 MB)
    fn test_embed_produces_384_dim() {
        let mut embedder = Embedder::new().expect("failed to initialize embedder");
        let embedding = embedder
            .embed_single("hello world")
            .expect("failed to embed");
        assert_eq!(embedding.len(), 384);
    }

    #[test]
    #[ignore] // requires model download (~23 MB)
    fn test_embed_batch() {
        let mut embedder = Embedder::new().expect("failed to initialize embedder");
        let embeddings = embedder
            .embed_texts(&["hello", "world"])
            .expect("failed to embed batch");
        assert_eq!(embeddings.len(), 2);
        assert_eq!(embeddings[0].len(), 384);
        assert_eq!(embeddings[1].len(), 384);
    }

    #[test]
    fn test_model_choice_values() {
        assert_eq!(ModelChoice::BgeSmallEnV15.model_id(), "bge-small-en-v1.5");
        assert_eq!(ModelChoice::BgeSmallEnV15.dimension(), 384);
        assert_eq!(ModelChoice::BgeM3.model_id(), "bge-m3");
        assert_eq!(ModelChoice::BgeM3.dimension(), 1024);
        assert_eq!(ModelChoice::default(), ModelChoice::BgeSmallEnV15);
    }

    #[test]
    fn test_model_choice_batch_size_is_smaller_for_large_model() {
        // 大きなモデルは activation memory が多いので batch を絞る。
        // OOM 防止のための invariant を固定化 (将来値を変えるときはここも更新)。
        assert!(
            ModelChoice::BgeM3.batch_size() < ModelChoice::BgeSmallEnV15.batch_size(),
            "BGE-M3 batch must be smaller than BGE-small-en-v1.5 batch"
        );
        assert!(ModelChoice::BgeM3.batch_size() > 0);
    }

    #[test]
    fn bge_m3_batch_size_is_eight() {
        // (kuriya 671) ONNX Runtime's activations grow with the batch: 32 x 512 tokens took
        // BGE-M3 from about 3 GB to about 5 GB of private bytes, 8 peaked near 2.5 GB at
        // about the same time per chunk.
        assert_eq!(ModelChoice::BgeM3.batch_size(), 8);
        assert_eq!(ModelChoice::BgeSmallEnV15.batch_size(), 256);
    }

    /// Runs [`embed_in_slices`] over `n` numbered texts and returns the vectors and the
    /// length of every slice it handed to the model, in order.
    fn run_embed_in_slices(n: usize, batch: usize) -> (Vec<Vec<f32>>, Vec<usize>) {
        let texts: Vec<String> = (0..n).map(|i| i.to_string()).collect();
        let refs: Vec<&str> = texts.iter().map(String::as_str).collect();
        let mut calls = Vec::new();
        let vectors = embed_in_slices(&refs, batch, |slice| {
            calls.push(slice.len());
            Ok(slice
                .iter()
                .map(|t| vec![t.parse::<f32>().expect("numbered text")])
                .collect())
        })
        .expect("embed in slices");
        (vectors, calls)
    }

    #[test]
    fn embed_in_slices_with_no_texts_calls_nothing() {
        let (vectors, calls) = run_embed_in_slices(0, 8);
        assert!(vectors.is_empty());
        assert!(calls.is_empty(), "no model call for no input: {calls:?}");
    }

    #[test]
    fn embed_in_slices_below_one_batch_is_one_call() {
        let (vectors, calls) = run_embed_in_slices(3, 8);
        assert_eq!(calls, vec![3]);
        assert_eq!(vectors, vec![vec![0.0], vec![1.0], vec![2.0]]);
    }

    #[test]
    fn embed_in_slices_exactly_one_batch_is_one_call() {
        let (vectors, calls) = run_embed_in_slices(8, 8);
        assert_eq!(calls, vec![8]);
        assert_eq!(vectors.len(), 8);
    }

    #[test]
    fn embed_in_slices_one_past_a_batch_is_two_calls_in_order() {
        let (vectors, calls) = run_embed_in_slices(9, 8);
        assert_eq!(calls, vec![8, 1]);
        let expected: Vec<Vec<f32>> = (0..9).map(|i| vec![i as f32]).collect();
        assert_eq!(vectors, expected);
    }

    #[test]
    fn embed_in_slices_many_batches_keep_order() {
        let (vectors, calls) = run_embed_in_slices(100, 8);
        assert_eq!(calls, [vec![8; 12], vec![4]].concat());
        let expected: Vec<Vec<f32>> = (0..100).map(|i| vec![i as f32]).collect();
        assert_eq!(vectors, expected);
    }

    #[test]
    fn embed_in_slices_stops_at_the_first_failing_slice() {
        let texts = ["a", "b", "c", "d", "e"];
        let mut calls = 0;
        let err = embed_in_slices(&texts, 2, |slice| {
            calls += 1;
            if calls == 2 {
                anyhow::bail!("second slice refused");
            }
            Ok(slice.iter().map(|_| vec![0.0]).collect())
        })
        .expect_err("second slice fails");
        assert_eq!(calls, 2, "no call after the failing slice");
        assert!(err.to_string().contains("second slice refused"), "{err}");
    }

    #[test]
    fn embed_in_slices_rejects_a_slice_that_returns_the_wrong_count() {
        let texts = ["a", "b", "c"];
        let err = embed_in_slices(&texts, 2, |_| Ok(vec![vec![0.0]]))
            .expect_err("one vector for two texts");
        assert!(
            err.to_string().contains("returned 1 vectors for 2 texts"),
            "{err}"
        );
    }

    const CAP: usize = FASTEMBED_MAX_INPUT_CHARS;
    const CALL: usize = FASTEMBED_MAX_CHARS_PER_CALL;

    fn lens(r: &[std::ops::Range<usize>]) -> Vec<usize> {
        r.iter().map(|r| r.len()).collect()
    }

    #[test]
    fn slice_bounds_of_nothing_is_empty() {
        assert!(slice_bounds(&[], 8, CALL).is_empty());
    }

    #[test]
    fn slice_bounds_of_small_inputs_cuts_by_count_only() {
        assert_eq!(
            lens(&slice_bounds(&[1; 100], 8, CALL)),
            [vec![8; 12], vec![4]].concat()
        );
        // A batch of 0 counts as 1, as `batch.max(1)` did before.
        assert_eq!(lens(&slice_bounds(&[1; 3], 0, CALL)), vec![1, 1, 1]);
    }

    #[test]
    fn slice_bounds_of_nine_full_inputs_is_eight_and_one() {
        for batch in [8, 256] {
            assert_eq!(
                lens(&slice_bounds(&[CAP; 9], batch, CALL)),
                vec![8, 1],
                "batch {batch}"
            );
        }
    }

    #[test]
    fn slice_bounds_keeps_the_bge_m3_slices_of_today() {
        // One greedy pass, not "group by characters first, then by count"
        // (which would give 8, 2, 8, 2).
        assert_eq!(
            lens(&slice_bounds(&[200 * 1024; 20], 8, CALL)),
            vec![8, 8, 4]
        );
    }

    #[test]
    fn slice_bounds_takes_an_input_of_exactly_the_cap_alone() {
        assert_eq!(slice_bounds(&[CALL], 8, CALL), vec![0..1]);
        assert_eq!(slice_bounds(&[CALL, 1], 8, CALL), vec![0..1, 1..2]);
    }

    #[test]
    fn slice_bounds_gives_an_oversized_input_a_slice_of_its_own() {
        assert_eq!(
            slice_bounds(&[1, CALL + 5, 1], 8, CALL),
            vec![0..1, 1..2, 2..3]
        );
        assert_eq!(slice_bounds(&[CALL + 5], 8, CALL), vec![0..1]);
    }

    #[test]
    fn slice_bounds_covers_every_input_in_order() {
        let weights = [5, CALL, 3, CALL - 3, 4, 0, 0];
        let ranges = slice_bounds(&weights, 2, CALL);
        let mut next = 0;
        for range in &ranges {
            assert_eq!(range.start, next, "gap or overlap: {ranges:?}");
            assert!(!range.is_empty(), "empty range: {ranges:?}");
            assert!(range.len() <= 2, "too many items: {ranges:?}");
            if range.len() >= 2 {
                let sum: usize = weights[range.clone()].iter().sum();
                assert!(sum <= CALL, "{range:?} weighs {sum}");
            }
            next = range.end;
        }
        assert_eq!(next, weights.len(), "not every input covered: {ranges:?}");
    }

    #[test]
    fn embed_in_slices_also_cuts_by_characters() {
        let text = "a".repeat(CAP);
        let texts: Vec<&str> = vec![text.as_str(); 9];
        let mut calls = Vec::new();
        let vectors = embed_in_slices(&texts, 256, |slice| {
            calls.push(slice.len());
            Ok(slice.iter().map(|_| vec![0.0]).collect())
        })
        .expect("embed in slices");
        assert_eq!(calls, vec![8, 1]);
        assert_eq!(vectors.len(), 9);
    }

    #[test]
    fn embed_truncated_cuts_on_char_boundaries() {
        let exact = "a".repeat(CAP);
        let ascii = "a".repeat(CAP + 1);
        let kana = "あ".repeat(CAP + 1);
        let emoji = "😀".repeat(CAP + 1);
        let texts: Vec<&str> = vec![&exact, &ascii, &kana, &emoji, ""];
        let mut seen: Vec<String> = Vec::new();
        let vectors = embed_truncated(&texts, 8, |slice| {
            seen.extend(slice.iter().map(|t| t.to_string()));
            Ok(slice
                .iter()
                .map(|t| vec![t.chars().count() as f32])
                .collect())
        })
        .expect("embed truncated");
        let counts: Vec<usize> = seen.iter().map(|t| t.chars().count()).collect();
        assert_eq!(counts, vec![CAP, CAP, CAP, CAP, 0]);
        assert_eq!(seen[0], exact);
        assert_eq!(seen[2].len(), 3 * CAP);
        assert!(seen[2].ends_with('あ'));
        assert_eq!(seen[3].len(), 4 * CAP);
        assert_eq!(seen[4], "");
        let order: Vec<f32> = vectors.iter().map(|v| v[0]).collect();
        assert_eq!(
            order,
            vec![CAP as f32, CAP as f32, CAP as f32, CAP as f32, 0.0]
        );
    }

    #[test]
    fn embed_truncated_leaves_short_inputs_alone() {
        let mut calls: Vec<Vec<String>> = Vec::new();
        let vectors = embed_truncated(&["hello", "world"], 8, |slice| {
            calls.push(slice.iter().map(|t| t.to_string()).collect());
            Ok(slice.iter().map(|_| vec![0.0]).collect())
        })
        .expect("embed truncated");
        assert_eq!(calls, vec![vec!["hello".to_string(), "world".to_string()]]);
        assert_eq!(vectors.len(), 2);
    }

    /// A stand-in model that records how many characters of each text it was
    /// given, call by call, and returns `[chars as f32]` for each text.
    struct RecordingModel {
        seen: Vec<Vec<usize>>,
    }

    impl FastEmbedModel for RecordingModel {
        fn embed_batch(&mut self, texts: &[&str], _batch: usize) -> Result<Vec<Vec<f32>>> {
            let counts: Vec<usize> = texts.iter().map(|t| t.chars().count()).collect();
            let vectors = counts.iter().map(|&n| vec![n as f32]).collect();
            self.seen.push(counts);
            Ok(vectors)
        }
    }

    fn recording_provider() -> FastEmbedProvider<RecordingModel> {
        FastEmbedProvider {
            model: RecordingModel { seen: vec![] },
            choice: ModelChoice::BgeSmallEnV15,
        }
    }

    #[test]
    fn fastembed_provider_cuts_documents_before_the_model() {
        let mut p = recording_provider();
        let long = "x".repeat(CAP + 1);
        let vectors = p
            .embed_documents(&[&long, "short"])
            .expect("embed documents");
        assert_eq!(p.model.seen, vec![vec![CAP, 5]]);
        assert_eq!(vectors.len(), 2);
    }

    #[test]
    fn fastembed_provider_cuts_queries_before_the_model() {
        let mut p = recording_provider();
        let long = "x".repeat(CAP + 1);
        p.embed_query(&long).expect("embed query");
        p.embed_queries(&[&long, "q"]).expect("embed queries");
        assert_eq!(p.model.seen, vec![vec![CAP], vec![CAP, 1]]);
        assert!(p.model.seen.iter().flatten().all(|&n| n <= CAP));
    }

    #[test]
    fn fastembed_provider_cuts_a_call_by_total_characters() {
        let mut p = recording_provider();
        let long = "x".repeat(CAP + 1);
        let texts: Vec<&str> = vec![long.as_str(); 9];
        let vectors = p.embed_documents(&texts).expect("embed documents");
        assert_eq!(p.model.seen, vec![vec![CAP; 8], vec![CAP]]);
        assert!(
            p.model
                .seen
                .iter()
                .all(|call| call.iter().sum::<usize>() <= CALL)
        );
        assert_eq!(vectors.len(), 9);
    }

    /// The WordPiece `max_input_chars_per_word` in BGE-small's tokenizer.json
    /// (feature-70 spec R1-2).
    const WORDPIECE_MAX_CHARS_PER_WORD: usize = 100;

    /// Deterministic English prose of exactly `chars` characters: about forty
    /// real words in a fixed cycle, joined by spaces, a sentence end every 12
    /// words and a paragraph break every 80.
    fn long_english_text(chars: usize) -> String {
        const WORDS: [&str; 40] = [
            "the", "index", "keeps", "every", "chunk", "of", "a", "document", "together", "with",
            "its", "heading", "path", "so", "that", "search", "can", "return", "the", "section",
            "where", "an", "answer", "lives", "and", "readers", "find", "context", "without",
            "opening", "whole", "files", "while", "models", "turn", "text", "into", "vectors",
            "for", "ranking",
        ];
        let mut out = String::with_capacity(chars + 16);
        let mut n = 0usize;
        while out.len() < chars {
            out.push_str(WORDS[n % WORDS.len()]);
            n += 1;
            if n.is_multiple_of(80) {
                out.push_str(".\n\n");
            } else if n.is_multiple_of(12) {
                out.push_str(". ");
            } else {
                out.push(' ');
            }
        }
        // ASCII only, so a byte length is a character count.
        out.truncate(chars);
        out
    }

    /// The input cap must leave room for 512 tokens (feature-70 spec R1-2):
    /// BGE-small's WordPiece makes every word (split on whitespace and
    /// punctuation) at least one token and a word over 100 characters one
    /// `[UNK]`, so text of words of at most 100 characters with one-character
    /// separators reaches 510 tokens (+ `[CLS]` `[SEP]`) within 510 × 101
    /// characters. The constants are bound to `let`s before the asserts to keep
    /// clippy's `assertions_on_constants` away, as the tests of [`crate::eval`]
    /// already do.
    #[test]
    fn fastembed_input_cap_is_long_enough_to_reach_512_tokens() {
        let cap = FASTEMBED_MAX_INPUT_CHARS;
        let call = FASTEMBED_MAX_CHARS_PER_CALL;
        // The inequalities come before the `assert_eq!`s so that a change to a
        // constant shows which bound it broke.
        // One reranker pair always fits (feature-70 spec R1-3).
        assert!(call >= 2 * cap, "{call} {cap}");
        assert!(cap >= 510 * (WORDPIECE_MAX_CHARS_PER_WORD + 1), "{cap}");
        assert_eq!(cap, 262_144);
        assert_eq!(call, 2_097_152);
        let text = long_english_text(300_000);
        assert_eq!(text.chars().count(), 300_000);
        let head = truncate_to_chars(&text, FASTEMBED_MAX_INPUT_CHARS);
        let pieces = head
            .split(|c: char| c.is_whitespace() || c.is_ascii_punctuation())
            .filter(|w| !w.is_empty())
            .count();
        assert!(pieces >= 510, "{pieces}");
        assert!(
            head.split_whitespace()
                .all(|w| w.chars().count() <= WORDPIECE_MAX_CHARS_PER_WORD)
        );
    }

    #[test]
    fn test_reranker_choice_values() {
        assert!(!RerankerChoice::None.is_enabled());
        assert!(RerankerChoice::BgeV2M3.is_enabled());
        assert!(RerankerChoice::JinaV2Multilingual.is_enabled());
        assert!(RerankerChoice::BgeBase.is_enabled());
        assert_eq!(RerankerChoice::default(), RerankerChoice::None);
        assert_eq!(RerankerChoice::BgeV2M3.model_id(), "bge-reranker-v2-m3");
        assert_eq!(RerankerChoice::BgeV2M3.approx_download_mb(), 2300);
    }

    #[test]
    fn test_reranker_value_enum_tag_matches_bench_arg() {
        // F-60 PR-1 codex P1 regression: benches/search_latency.rs hard-codes
        // `--reranker bge-v2-m3` as the heavy-bench subprocess argument. If
        // the `#[value(name = "...")]` tag on RerankerChoice::BgeV2M3 ever
        // diverges from this literal, the bench would fail at clap parse time.
        // The HuggingFace model id `bge-reranker-v2-m3` must NOT be a valid
        // CLI value (it lives behind `RerankerChoice::model_id()`).
        use clap::ValueEnum;
        assert!(
            RerankerChoice::from_str("bge-v2-m3", false).is_ok(),
            "CLI must accept the bench-hardcoded reranker tag 'bge-v2-m3'"
        );
        assert!(
            RerankerChoice::from_str("bge-reranker-v2-m3", false).is_err(),
            "the HuggingFace model id must not be a valid CLI value"
        );
        assert!(RerankerChoice::from_str("bge-base", false).is_ok());
        assert!(RerankerChoice::from_str("jina-v2-ml", false).is_ok());
        assert!(RerankerChoice::from_str("none", false).is_ok());
    }

    #[test]
    fn test_reranker_none_returns_none() {
        // DL を伴わない安全なテスト
        let r = Reranker::try_new(RerankerChoice::None).unwrap();
        assert!(r.is_none());
    }

    /// `mk` (既存の rerank 統合テスト用 helper) の context_text 対応版。
    /// model DL 不要な pure fn テストから使う test-local helper。
    fn mk_with_context(content: &str, context_text: Option<&str>) -> SearchResult {
        SearchResult {
            start_line: None,
            end_line: None,
            symbol_kind: None,
            score: 0.0,
            content: content.to_string(),
            heading: None,
            document_id: 0,
            path: "x.md".to_string(),
            title: None,
            topic: None,
            date: None,
            tags: Vec::new(),
            context_text: context_text.map(str::to_string),
            table_rows: None,
        }
    }

    #[test]
    fn test_contextualize_for_rerank_prepends_context() {
        let r = mk_with_context("body", Some("T > H"));
        assert_eq!(contextualize_for_rerank(&r), "T > H\n\nbody");
    }

    #[test]
    fn test_contextualize_for_rerank_none_is_content_only() {
        let r = mk_with_context("body", None);
        assert_eq!(contextualize_for_rerank(&r), "body");
    }

    #[test]
    fn test_contextualize_for_rerank_empty_is_content_only() {
        let r = mk_with_context("body", Some(""));
        assert_eq!(contextualize_for_rerank(&r), "body");
    }

    #[test]
    fn contextualize_for_rerank_cuts_after_joining() {
        let content = "b".repeat(CAP + 1);
        let joined = contextualize_for_rerank(&mk_with_context(&content, Some("T > H")));
        assert_eq!(
            joined,
            truncate_to_chars(&format!("T > H\n\n{content}"), CAP)
        );
        assert_eq!(joined.chars().count(), CAP);
        for ctx in [None, Some("   ")] {
            let doc = contextualize_for_rerank(&mk_with_context(&content, ctx));
            assert_eq!(doc.chars().count(), CAP, "{ctx:?}");
            assert!(doc.chars().all(|c| c == 'b'), "{ctx:?}");
        }
    }

    #[test]
    fn contextualize_for_rerank_cuts_a_cjk_body_on_a_char_boundary() {
        let content = "あ".repeat(CAP + 1);
        let doc = contextualize_for_rerank(&mk_with_context(&content, Some("見出し")));
        assert_eq!(doc.chars().count(), CAP);
        assert_eq!(doc.chars().last(), Some('あ'));
    }

    #[test]
    fn rerank_document_weight_is_the_length_of_what_is_sent() {
        let ascii = "b".repeat(CAP + 1);
        let kana = "あ".repeat(CAP + 1);
        let rows = [
            mk_with_context(&ascii, Some("T > H")),
            mk_with_context(&ascii, None),
            mk_with_context(&ascii, Some("   ")),
            mk_with_context(&kana, Some("見出し")),
            mk_with_context("body", Some("T > H")),
            mk_with_context("body", None),
            mk_with_context("body", Some("")),
        ];
        for r in &rows {
            assert_eq!(
                rerank_document_weight(r),
                contextualize_for_rerank(r).chars().count(),
                "{:?}",
                r.context_text
            );
        }
    }

    /// What [`FakeRerank`] was handed, call by call: the query's character
    /// count and every document. An alias because the nested type would trip
    /// clippy's `type_complexity` (`all = deny`, also on test targets).
    type RerankCalls = Arc<Mutex<Vec<(usize, Vec<String>)>>>;

    /// A stand-in reranker model. Each document scores `score_of(doc)`; each
    /// slice's results come back in descending index order (neither fastembed's
    /// order nor ascending index order), so only an explicit index tie-break
    /// puts equal scores in candidate order. With `bad_index`, every slice also
    /// answers `(documents.len(), 99.0)`, an index past the slice.
    struct FakeRerank {
        calls: RerankCalls,
        score_of: fn(&str) -> f32,
        bad_index: bool,
    }

    impl RerankModel for FakeRerank {
        fn rerank_slice(&mut self, query: &str, documents: &[&str]) -> Result<Vec<(usize, f32)>> {
            self.calls.lock().expect("calls lock").push((
                query.chars().count(),
                documents.iter().map(|d| d.to_string()).collect(),
            ));
            let mut out: Vec<(usize, f32)> = documents
                .iter()
                .enumerate()
                .rev()
                .map(|(i, d)| (i, (self.score_of)(d)))
                .collect();
            if self.bad_index {
                out.push((documents.len(), 99.0));
            }
            Ok(out)
        }
    }

    fn fake_reranker(score_of: fn(&str) -> f32, bad_index: bool) -> (Reranker, RerankCalls) {
        let calls: RerankCalls = Arc::new(Mutex::new(Vec::new()));
        let model = FakeRerank {
            calls: Arc::clone(&calls),
            score_of,
            bad_index,
        };
        let reranker = Reranker {
            model: Box::new(model),
            choice: RerankerChoice::BgeBase,
        };
        (reranker, calls)
    }

    /// How many documents each recorded call was given.
    fn rerank_call_sizes(calls: &RerankCalls) -> Vec<usize> {
        calls
            .lock()
            .expect("calls lock")
            .iter()
            .map(|(_, docs)| docs.len())
            .collect()
    }

    fn score_one(_: &str) -> f32 {
        1.0
    }

    fn score_as_number(doc: &str) -> f32 {
        doc.parse().expect("a numeric document")
    }

    /// `n` candidates with ids `0..n` and short contents `"0"`, `"1"`, ...
    fn numbered_candidates(n: usize) -> Vec<(i64, SearchResult)> {
        (0..n)
            .map(|i| (i as i64, mk_with_context(&i.to_string(), None)))
            .collect()
    }

    fn ids(out: &[(i64, SearchResult)]) -> Vec<i64> {
        out.iter().map(|(id, _)| *id).collect()
    }

    #[test]
    fn rerank_cuts_the_query_and_every_document_before_the_model() {
        let (mut reranker, calls) = fake_reranker(score_one, false);
        let query = "q".repeat(CAP + 1);
        let content = "b".repeat(CAP + 1);
        let with_ctx = mk_with_context(&content, Some("T > H"));
        let candidates = vec![
            (1, with_ctx.clone()),
            (2, mk_with_context(&content, None)),
            (3, mk_with_context(&content, Some("   "))),
        ];
        let out = reranker
            .rerank_candidates_with_ids(&query, candidates, 10)
            .expect("rerank");
        assert_eq!(out.len(), 3);
        let calls = calls.lock().expect("calls lock");
        assert!(!calls.is_empty());
        for (query_chars, docs) in calls.iter() {
            assert!(*query_chars <= CAP, "{query_chars}");
            assert!(docs.iter().all(|d| d.chars().count() <= CAP));
            let total: usize = docs.iter().map(|d| query_chars + d.chars().count()).sum();
            assert!(total <= CALL, "{total}");
        }
        let sent: Vec<&String> = calls.iter().flat_map(|(_, docs)| docs).collect();
        assert_eq!(sent[0], &contextualize_for_rerank(&with_ctx));
    }

    #[test]
    fn rerank_counts_the_query_once_per_pair() {
        let (mut reranker, calls) = fake_reranker(score_one, false);
        let query = "q".repeat(CAP);
        let candidates: Vec<(i64, SearchResult)> = (0..10)
            .map(|i| (i, mk_with_context(&"d".repeat(10), None)))
            .collect();
        reranker
            .rerank_candidates_with_ids(&query, candidates, 10)
            .expect("rerank");
        // A pair weighs CAP + 10: 7 × (CAP + 10) <= 8 × CAP < 8 × (CAP + 10).
        assert_eq!(rerank_call_sizes(&calls), vec![7, 3]);
    }

    #[test]
    fn rerank_counts_the_query_after_cutting_it() {
        let (mut reranker, calls) = fake_reranker(score_one, false);
        let query = "q".repeat(10 * CAP);
        let candidates: Vec<(i64, SearchResult)> = (0..10)
            .map(|i| (i, mk_with_context(&"d".repeat(10), None)))
            .collect();
        reranker
            .rerank_candidates_with_ids(&query, candidates, 10)
            .expect("rerank");
        // Counted before the cut, one pair alone would weigh over CALL: [1; 10].
        assert_eq!(rerank_call_sizes(&calls), vec![7, 3]);
    }

    #[test]
    fn rerank_splits_at_256_candidates_and_keeps_ties_in_candidate_order() {
        let (mut reranker, calls) = fake_reranker(score_one, false);
        let out = reranker
            .rerank_candidates_with_ids("q", numbered_candidates(300), 300)
            .expect("rerank");
        assert_eq!(rerank_call_sizes(&calls), vec![256, 44]);
        assert_eq!(ids(&out), (0..300).collect::<Vec<i64>>());
    }

    #[test]
    fn rerank_takes_the_top_across_slices() {
        let (mut reranker, _calls) = fake_reranker(score_as_number, false);
        let out = reranker
            .rerank_candidates_with_ids("q", numbered_candidates(300), 5)
            .expect("rerank");
        assert_eq!(ids(&out), vec![299, 298, 297, 296, 295]);
        for (id, row) in &out {
            assert_eq!(row.score, *id as f32);
        }
        let all = reranker
            .rerank_candidates_with_ids("q", numbered_candidates(300), 1000)
            .expect("rerank");
        assert_eq!(all.len(), 300);
    }

    #[test]
    fn rerank_drops_an_index_past_its_slice() {
        let (mut reranker, calls) = fake_reranker(score_one, true);
        let out = reranker
            .rerank_candidates_with_ids("q", numbered_candidates(300), 1000)
            .expect("rerank");
        assert_eq!(out.len(), 300);
        let mut seen = ids(&out);
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(seen.len(), 300);

        // The out-of-range results (score 99) outrank every real one, so a
        // `limit` counted before they are dropped would lose two of its five.
        assert!(score_one("0") < 99.0);
        let top = reranker
            .rerank_candidates_with_ids("q", numbered_candidates(300), 5)
            .expect("rerank");
        assert_eq!(rerank_call_sizes(&calls), vec![256, 44, 256, 44]);
        assert_eq!(top.len(), 5);
        assert!(top.iter().all(|(id, _)| (0..300).contains(id)));
    }

    #[test]
    fn rerank_with_an_empty_query_keeps_every_candidate() {
        let (mut reranker, calls) = fake_reranker(score_one, false);
        let out = reranker
            .rerank_candidates_with_ids("", numbered_candidates(3), 10)
            .expect("rerank");
        assert_eq!(out.len(), 3);
        assert_eq!(rerank_call_sizes(&calls), vec![3]);
    }

    #[test]
    fn rerank_of_no_candidates_calls_nothing() {
        let (mut reranker, calls) = fake_reranker(score_one, false);
        let out = reranker
            .rerank_candidates_with_ids("q", Vec::new(), 10)
            .expect("rerank");
        assert!(out.is_empty());
        assert!(rerank_call_sizes(&calls).is_empty());
    }

    #[test]
    #[ignore] // requires BGE-reranker-v2-m3 download (~2.3 GB)
    fn test_bge_reranker_v2_m3_reorders_ja() {
        let mut r = Reranker::try_new(RerankerChoice::BgeV2M3)
            .expect("failed to load BGE-reranker-v2-m3")
            .expect("reranker should be Some");
        // SearchResult は db::SearchResult を使う
        use crate::db::SearchResult;
        let mk = |content: &str| SearchResult {
            start_line: None,
            end_line: None,
            symbol_kind: None,
            score: 0.0,
            content: content.to_string(),
            heading: None,
            document_id: 0,
            path: "x.md".to_string(),
            title: None,
            topic: None,
            date: None,
            tags: Vec::new(),
            context_text: None,
            table_rows: None,
        };
        let candidates = vec![
            (1i64, mk("天気予報の話題です")),
            (
                2,
                mk("E0382 は所有権が移動した後の値を使ったときに出るエラーです"),
            ),
            (3, mk("映画のレビューについて")),
        ];
        let out = r
            .rerank_candidates("Rust の E0382 エラーの意味は？", candidates, 3)
            .unwrap();
        assert_eq!(out.len(), 3);
        assert!(
            out[0].content.contains("E0382"),
            "top should be E0382 content, got: {}",
            out[0].content
        );
    }

    #[test]
    #[ignore] // requires BGE-M3 download (~2.3 GB)
    fn test_bge_m3_produces_1024_dim() {
        let mut embedder = Embedder::with_model(ModelChoice::BgeM3).expect("failed to load BGE-M3");
        let emb = embedder
            .embed_single("こんにちは、世界")
            .expect("failed to embed");
        assert_eq!(emb.len(), 1024);
    }

    /// Cosine of two vectors, summed in f64; neither needs unit length.
    fn cos(a: &[f32], b: &[f32]) -> f32 {
        let dot: f64 = a
            .iter()
            .zip(b)
            .map(|(x, y)| f64::from(*x) * f64::from(*y))
            .sum();
        let norm = |v: &[f32]| v.iter().map(|x| f64::from(*x).powi(2)).sum::<f64>().sqrt();
        (dot / (norm(a) * norm(b))) as f32
    }

    /// Held by the new tests below that load BGE-M3 or bge-reranker-v2-m3, so
    /// they do not load the big models in one process at the same time. The
    /// existing big-model tests do not take it: editing existing tests is not
    /// allowed. A cold cache's fight over the same blob lock is removed by the
    /// nightly Linux leg warming those models one test at a time before the
    /// suite. A poisoned lock is taken all the same, so one failure does not
    /// fail the next test.
    static BIG_MODEL_TEST_LOCK: Mutex<()> = Mutex::new(());

    fn big_model_test_lock() -> std::sync::MutexGuard<'static, ()> {
        BIG_MODEL_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    /// Text of exactly `chars` characters made by appending `piece(0)`,
    /// `piece(1)`, ... and cutting the end on a character boundary.
    fn text_of_chars(chars: usize, mut piece: impl FnMut(usize) -> String) -> String {
        let mut out = String::new();
        let mut count = 0usize;
        let mut i = 0usize;
        while count < chars {
            let p = piece(i);
            count += p.chars().count();
            out.push_str(&p);
            i += 1;
        }
        truncate_to_chars(&out, chars).to_string()
    }

    /// A linear congruential generator: the same sequence on every run.
    fn lcg(state: &mut u64) -> u64 {
        *state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        *state >> 33
    }

    /// The input shapes checked against the real tokenizers, 300,000 characters
    /// each, which must still fill 512 tokens after the cut to
    /// [`FASTEMBED_MAX_INPUT_CHARS`].
    fn cut_inputs_that_fill_512_tokens() -> Vec<(&'static str, String)> {
        const N: usize = 300_000;
        const JA: [&str; 5] = [
            "索引は文書の各チャンクを見出しの経路と一緒に保持します。",
            "検索は答えのある節を返すので、読み手はファイル全体を開かずに文脈を得られます。",
            "モデルは文章をベクトルに変え、近いものから順に並べます。",
            "設定ファイルを変えたら、索引を作り直してください。",
            "日本語の本文は形態素ではなく文字の並びとして扱われます。",
        ];
        const B64: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/=";
        const NAMES: [&str; 4] = ["alpha", "bravo", "charlie", "delta-long"];
        let mut seed = 7u64;
        let base64: String = (0..N)
            .map(|_| B64[(lcg(&mut seed) % B64.len() as u64) as usize] as char)
            .collect();
        let mut seed = 11u64;
        let hex_dump = text_of_chars(N, |i| {
            let bytes: Vec<String> = (0..16)
                .map(|_| format!("{:02x}", lcg(&mut seed) & 0xff))
                .collect();
            format!("{:08x}  {}\n", i * 16, bytes.join(" "))
        });
        vec![
            ("english", long_english_text(N)),
            (
                "japanese",
                text_of_chars(N, |i| JA[i % JA.len()].to_string()),
            ),
            ("base64", base64),
            (
                "minified js",
                text_of_chars(N, |i| {
                    format!(
                        "function f{i}(a,b){{return a.map(function(x){{return x*b+{i}}})}}var r{i}=f{i}([1,2,3],4);if(r{i}.length>2){{console.log(r{i}.join(','))}};"
                    )
                }),
            ),
            (
                "aligned table",
                text_of_chars(N, |i| {
                    format!(
                        "| {:>6} | {:<12} | {:>10} |\n",
                        i,
                        NAMES[i % NAMES.len()],
                        (i * 37) % 10_007
                    )
                }),
            ),
            ("hex dump", hex_dump),
        ]
    }

    /// Inputs the cut may leave short of 512 tokens (the cases listed on
    /// [`FastEmbedProvider`]): their token counts are printed, not asserted.
    fn pathological_cut_inputs() -> Vec<(&'static str, String)> {
        const N: usize = 300_000;
        let mut seed = 13u64;
        let hex: String = (0..N)
            .map(|_| char::from_digit((lcg(&mut seed) % 16) as u32, 16).unwrap_or('0'))
            .collect();
        // 95% of the first FASTEMBED_MAX_INPUT_CHARS characters are spaces
        // and NULs, prose after that.
        let blank = FASTEMBED_MAX_INPUT_CHARS * 95 / 100;
        let mut mostly_blank: String = (0..blank)
            .map(|i| if i % 2 == 0 { ' ' } else { '\0' })
            .collect();
        mostly_blank.push_str(&long_english_text(N - blank));
        vec![
            ("hex without separators", hex),
            ("mostly spaces and NULs", mostly_blank),
        ]
    }

    /// Cut each of the inputs from [`cut_inputs_that_fill_512_tokens`] to
    /// [`FASTEMBED_MAX_INPUT_CHARS`] and check that `encode` (returning the
    /// token count and the number of overflowing encodings) gives 512 tokens
    /// with an overflow; print the counts of the inputs from
    /// [`pathological_cut_inputs`].
    fn check_cut_inputs_fill_512_tokens(model: &str, encode: impl Fn(&str) -> (usize, usize)) {
        for (kind, text) in cut_inputs_that_fill_512_tokens() {
            let (n, overflow) = encode(truncate_to_chars(&text, FASTEMBED_MAX_INPUT_CHARS));
            println!("{model} {kind}: {n} tokens (overflow: {overflow})");
            assert!(
                n == 512 && overflow > 0,
                "{model} {kind}: the cut input gave {n} tokens (overflow: {overflow}), not 512 with an overflow"
            );
        }
        for (kind, text) in pathological_cut_inputs() {
            let (n, overflow) = encode(truncate_to_chars(&text, FASTEMBED_MAX_INPUT_CHARS));
            println!("{model} {kind}: {n} tokens (overflow: {overflow})");
        }
    }

    /// A 300,000-character text and its first 20,000 characters are both cut
    /// at 512 tokens and embed alike, and a run of words inside those tokens
    /// still changes the vector (feature-70 spec T1-4). The run is 25 words
    /// starting near the 1,200th character rather than the single word of the
    /// feature-70 spec's section 11: one word is not sure to move a CLS-pooled
    /// vector past the 0.9999 bound.
    #[test]
    #[ignore] // requires model download (~130 MB)
    fn fastembed_cut_keeps_the_vector_of_real_text_bge_small() -> Result<()> {
        let mut p = FastEmbedProvider::new(ModelChoice::BgeSmallEnV15)?;
        let long = long_english_text(300_000);
        let head = truncate_to_chars(&long, 20_000);
        for (name, text) in [("long", long.as_str()), ("head", head)] {
            let enc = p
                .model
                .tokenizer
                .encode(truncate_to_chars(text, FASTEMBED_MAX_INPUT_CHARS), true)
                .map_err(anyhow::Error::msg)?;
            assert!(
                enc.len() == 512 && !enc.get_overflowing().is_empty(),
                "{name}: {} tokens, overflow {}",
                enc.len(),
                enc.get_overflowing().len()
            );
        }

        // The words of `long` (it is ASCII, so byte offsets are char offsets).
        let mut words = Vec::new();
        let mut start = None;
        for (i, c) in long.char_indices() {
            match (c.is_ascii_alphabetic(), start) {
                (true, None) => start = Some(i),
                (false, Some(s)) => {
                    words.push(s..i);
                    start = None;
                }
                _ => {}
            }
        }
        let first = words
            .iter()
            .position(|w| w.start >= 1_200)
            .expect("a word after the 1,200th character");
        let replaced = &words[first..first + 25];
        assert!(replaced[0].start < 1_600, "{:?}", replaced[0]);
        const OTHER: [&str; 5] = ["volcano", "glacier", "quartz", "harbour", "meadow"];
        let mut changed = String::with_capacity(long.len() + 256);
        let mut at = 0;
        for (k, w) in replaced.iter().enumerate() {
            changed.push_str(&long[at..w.start]);
            changed.push_str(OTHER[k % OTHER.len()]);
            at = w.end;
        }
        let end = changed.len();
        changed.push_str(&long[at..]);
        let before_end = p
            .model
            .tokenizer
            .encode(&changed[..end], true)
            .map_err(anyhow::Error::msg)?
            .len();
        assert!(
            before_end < 510,
            "the changed words run to token {before_end}, past the 512 kept"
        );

        let v = p.embed_documents(&[long.as_str(), head, changed.as_str()])?;
        let same = cos(&v[0], &v[1]);
        let differ = cos(&v[2], &v[0]);
        println!(
            "bge-small cos(long, head) = {same}, cos(changed, long) = {differ}, changed run ends at token {before_end}"
        );
        assert!(same >= 0.9999, "cos(long, head) = {same}");
        assert!(differ < 0.9999, "cos(changed, long) = {differ}");
        Ok(())
    }

    /// The inputs from [`cut_inputs_that_fill_512_tokens`] still fill 512
    /// tokens after the cut, for BGE-small (feature-70 spec T1-4b).
    #[test]
    #[ignore] // requires model download (~130 MB)
    fn fastembed_cut_inputs_fill_512_tokens_bge_small() -> Result<()> {
        let p = FastEmbedProvider::new(ModelChoice::BgeSmallEnV15)?;
        check_cut_inputs_fill_512_tokens("bge-small-en-v1.5", |s| {
            let enc = p.model.tokenizer.encode(s, true).expect("encode");
            (enc.len(), enc.get_overflowing().len())
        });
        Ok(())
    }

    /// The same check as the BGE-small one above, for BGE-M3 (feature-70 spec
    /// T1-4b). The nightly Windows and macOS legs skip this test through the
    /// `SKIP` list in `.github/workflows/nightly.yml`; a skip matches a
    /// substring and no existing entry contains this name, so it needs a line
    /// of its own.
    #[test]
    #[ignore] // requires BGE-M3 download (~2.3 GB)
    fn fastembed_cut_inputs_fill_512_tokens_bge_m3() -> Result<()> {
        let _guard = big_model_test_lock();
        let p = FastEmbedProvider::new(ModelChoice::BgeM3)?;
        check_cut_inputs_fill_512_tokens("bge-m3", |s| {
            let enc = p.model.tokenizer.encode(s, true).expect("encode");
            (enc.len(), enc.get_overflowing().len())
        });
        Ok(())
    }

    /// For bge-reranker-v2-m3, a short query paired with cut English or cut
    /// Japanese still fills 512 tokens (feature-70 spec T1-4b). The nightly
    /// Windows and macOS legs skip this test through the `SKIP` list in
    /// `.github/workflows/nightly.yml`; a skip matches a substring and no
    /// existing entry contains this name, so it needs a line of its own.
    #[test]
    #[ignore] // requires BGE-reranker-v2-m3 download (~2.3 GB)
    fn fastembed_cut_inputs_fill_512_tokens_reranker_bge_v2_m3() -> Result<()> {
        let _guard = big_model_test_lock();
        let model = load_text_rerank(RerankerModel::BGERerankerV2M3)?;
        let query = "where does the answer live?";
        for (kind, text) in cut_inputs_that_fill_512_tokens()
            .into_iter()
            .filter(|(kind, _)| matches!(*kind, "english" | "japanese"))
        {
            let enc = model
                .tokenizer
                .encode(
                    (query, truncate_to_chars(&text, FASTEMBED_MAX_INPUT_CHARS)),
                    true,
                )
                .map_err(anyhow::Error::msg)?;
            let (n, overflow) = (enc.len(), enc.get_overflowing().len());
            println!("bge-reranker-v2-m3 {kind}: {n} tokens (overflow: {overflow})");
            assert!(
                n == 512 && overflow > 0,
                "bge-reranker-v2-m3 {kind}: the cut pair gave {n} tokens (overflow: {overflow}), not 512 with an overflow"
            );
        }
        Ok(())
    }

    /// The reranker scores a 300,000-character candidate like its first 20,000
    /// characters, as both are cut to the same 512 tokens (feature-70 spec
    /// T1-6). The nightly Windows and macOS legs skip this test through the
    /// `SKIP` list in `.github/workflows/nightly.yml`; a skip matches a
    /// substring and no existing entry contains this name, so it needs a line
    /// of its own.
    #[test]
    #[ignore] // requires BGE-reranker-v2-m3 download (~2.3 GB)
    fn reranker_scores_a_cut_candidate_like_its_head() -> Result<()> {
        let _guard = big_model_test_lock();
        let mut r = Reranker::try_new(RerankerChoice::BgeV2M3)?.expect("reranker should be Some");
        let long = long_english_text(300_000);
        let head = truncate_to_chars(&long, 20_000);
        let candidates = vec![
            (1i64, mk_with_context(&long, None)),
            (2i64, mk_with_context(head, None)),
        ];
        let out = r.rerank_candidates_with_ids("where does the answer live?", candidates, 2)?;
        assert_eq!(out.len(), 2);
        let score = |id: i64| {
            out.iter()
                .find(|(i, _)| *i == id)
                .map(|(_, s)| s.score)
                .expect("both candidates come back")
        };
        let (a, b) = (score(1), score(2));
        println!("bge-reranker-v2-m3 score(long) = {a}, score(head) = {b}");
        assert!(
            (a - b).abs() <= 1e-3,
            "score(long) = {a}, score(head) = {b}"
        );
        Ok(())
    }

    #[test]
    fn truncate_to_chars_cuts_on_a_char_boundary_and_leaves_short_text_alone() {
        let ascii = "x".repeat(9000);
        assert_eq!(truncate_to_chars(&ascii, 8000).chars().count(), 8000);
        let kana = "あ".repeat(9000);
        let cut = truncate_to_chars(&kana, 8000);
        assert_eq!(cut.chars().count(), 8000);
        assert!(cut.chars().all(|c| c == 'あ'));
        // An emoji is one char of four bytes: a byte slice at 8000 would panic.
        let emoji = "\u{1F600}".repeat(10);
        assert_eq!(truncate_to_chars(&emoji, 3), "\u{1F600}\u{1F600}\u{1F600}");
        assert_eq!(truncate_to_chars("short", 8000), "short");
        assert_eq!(truncate_to_chars("exact", 5), "exact");
    }

    #[test]
    fn retry_after_reads_delta_seconds_and_http_dates_and_ignores_the_rest() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(784_111_777); // Sun, 06 Nov 1994 08:49:37 GMT
        assert_eq!(parse_retry_after("1", now), Some(Duration::from_secs(1)));
        assert_eq!(parse_retry_after(" 0 ", now), Some(Duration::ZERO));
        assert_eq!(
            parse_retry_after("120", now),
            Some(Duration::from_secs(120))
        );
        // Larger than u64: longer than any wait groove honours, not "absent".
        assert_eq!(
            parse_retry_after("99999999999999999999999", now),
            Some(Duration::MAX)
        );
        assert_eq!(
            parse_retry_after("Sun, 06 Nov 1994 08:49:47 GMT", now),
            Some(Duration::from_secs(10))
        );
        assert_eq!(
            parse_retry_after("Sun, 06 Nov 1994 08:49:27 GMT", now),
            Some(Duration::ZERO),
            "a date in the past means retry now"
        );
        for garbage in [
            "",
            "   ",
            "-1",
            "1.5",
            "soon",
            "Sunday, 06-Nov-94 08:49:37 GMT",
        ] {
            assert_eq!(parse_retry_after(garbage, now), None, "{garbage:?}");
        }
    }

    #[test]
    fn status_classes_split_input_rejections_transient_failures_and_the_rest() {
        let cases = [
            (200, StatusClass::Success),
            (204, StatusClass::Success),
            (400, StatusClass::InputRejected),
            (413, StatusClass::InputRejected),
            (422, StatusClass::InputRejected),
            (429, StatusClass::Retryable),
            (500, StatusClass::Retryable),
            (502, StatusClass::Retryable),
            (503, StatusClass::Retryable),
            (504, StatusClass::Retryable),
            (599, StatusClass::Retryable),
            (401, StatusClass::Fatal),
            (403, StatusClass::Fatal),
            (404, StatusClass::Fatal),
            (408, StatusClass::Fatal),
            (301, StatusClass::Fatal),
        ];
        for (status, class) in cases {
            assert_eq!(classify_status(status), class, "HTTP {status}");
        }
    }

    #[test]
    fn backoff_is_capped_at_sixty_seconds_for_large_attempt_numbers() {
        assert_eq!(backoff_base(1), Duration::from_secs(1));
        assert_eq!(backoff_base(2), Duration::from_secs(2));
        assert_eq!(backoff_base(3), Duration::from_secs(4));
        assert_eq!(backoff_base(7), MAX_RETRY_AFTER);
        assert_eq!(backoff_base(40), MAX_RETRY_AFTER);
        assert_eq!(backoff_base(u32::MAX), MAX_RETRY_AFTER);
    }

    #[test]
    fn wait_before_retry_prefers_retry_after_and_gives_up_past_sixty_seconds() {
        let mut quarter = |base: Duration| base / 4;
        assert_eq!(
            wait_before_retry(1, None, &mut quarter),
            Wait::After(Duration::from_millis(1250))
        );
        assert_eq!(
            wait_before_retry(3, None, &mut quarter),
            Wait::After(Duration::from_secs(5))
        );
        assert_eq!(
            wait_before_retry(3, Some(Duration::from_secs(1)), &mut quarter),
            Wait::After(Duration::from_secs(1)),
            "Retry-After is waited exactly, without jitter"
        );
        assert_eq!(
            wait_before_retry(1, Some(MAX_RETRY_AFTER), &mut quarter),
            Wait::After(MAX_RETRY_AFTER)
        );
        assert_eq!(
            wait_before_retry(1, Some(Duration::from_secs(61)), &mut quarter),
            Wait::GiveUp(Duration::from_secs(61))
        );
    }

    /// A scripted attempt sequence for [`retry_loop`]: each call pops the next
    /// outcome and counts itself.
    fn scripted(
        outcomes: Vec<std::result::Result<u32, AttemptFailure>>,
    ) -> (
        impl FnMut() -> std::result::Result<u32, AttemptFailure>,
        std::rc::Rc<std::cell::Cell<u32>>,
    ) {
        let calls = std::rc::Rc::new(std::cell::Cell::new(0));
        let counter = calls.clone();
        let mut outcomes = outcomes.into_iter();
        let attempt = move || {
            counter.set(counter.get() + 1);
            outcomes
                .next()
                .expect("retry_loop asked for more attempts than scripted")
        };
        (attempt, calls)
    }

    fn transient(status: u16, retry_after: Option<Duration>) -> AttemptFailure {
        AttemptFailure::Retryable {
            error: anyhow::anyhow!("embedding endpoint returned HTTP {status}: body-{status}"),
            retry_after,
            last: format!("HTTP {status}"),
        }
    }

    /// Run [`retry_loop`] with a sleeper that records instead of sleeping, and no jitter.
    fn run_retry_loop(
        max_retries: u32,
        attempt: impl FnMut() -> std::result::Result<u32, AttemptFailure>,
    ) -> (Result<u32>, Vec<Duration>) {
        let mut slept = Vec::new();
        let result = {
            let mut sleep = |d: Duration| slept.push(d);
            let mut jitter = |_: Duration| Duration::ZERO;
            let mut backoff = Backoff {
                sleep: &mut sleep,
                jitter: &mut jitter,
            };
            retry_loop(max_retries, &mut backoff, attempt)
        };
        (result, slept)
    }

    #[test]
    fn retry_loop_backs_off_one_two_four_seconds_then_gives_up_naming_the_attempts() {
        let (attempt, calls) = scripted((0..4).map(|_| Err(transient(503, None))).collect());
        let (result, slept) = run_retry_loop(3, attempt);
        let err = result.expect_err("four 503s must fail");
        assert_eq!(calls.get(), 4);
        assert_eq!(
            slept,
            vec![
                Duration::from_secs(1),
                Duration::from_secs(2),
                Duration::from_secs(4)
            ]
        );
        assert_eq!(
            err.to_string(),
            "embedding endpoint still failing after 4 attempts (last: HTTP 503)"
        );
        assert!(
            format!("{err:#}").contains("HTTP 503: body-503"),
            "the last error stays the source: {err:#}"
        );
    }

    #[test]
    fn retry_loop_waits_exactly_what_retry_after_says_and_then_succeeds() {
        let (attempt, calls) = scripted(vec![
            Err(transient(429, Some(Duration::from_secs(1)))),
            Ok(7),
        ]);
        let (result, slept) = run_retry_loop(3, attempt);
        assert_eq!(result.expect("the second attempt succeeds"), 7);
        assert_eq!(calls.get(), 2);
        assert_eq!(slept, vec![Duration::from_secs(1)]);
    }

    #[test]
    fn retry_loop_gives_up_at_once_when_retry_after_exceeds_sixty_seconds() {
        let (attempt, calls) = scripted(vec![Err(transient(429, Some(Duration::from_secs(120))))]);
        let (result, slept) = run_retry_loop(3, attempt);
        let err = result.expect_err("a 120 s Retry-After is not waited for");
        assert_eq!(calls.get(), 1);
        assert!(slept.is_empty(), "nothing may be slept: {slept:?}");
        assert_eq!(
            err.to_string(),
            "embedding endpoint asked to retry after 120 s, more than the 60 s groove waits; \
             gave up after 1 attempt(s)"
        );
    }

    #[test]
    fn retry_loop_does_not_retry_a_fatal_failure_and_keeps_its_type() {
        let rejected = anyhow::Error::new(EmbedInputRejected {
            status: 413,
            body_snippet: "too long".to_string(),
        });
        let (attempt, calls) = scripted(vec![Err(AttemptFailure::Fatal(rejected))]);
        let (result, slept) = run_retry_loop(3, attempt);
        let err = result.expect_err("a rejection fails");
        assert_eq!(calls.get(), 1);
        assert!(slept.is_empty());
        assert_eq!(
            err.downcast_ref::<EmbedInputRejected>().map(|r| r.status),
            Some(413)
        );
        assert_eq!(
            err.to_string(),
            "embedding endpoint returned HTTP 413: too long"
        );

        let (attempt, calls) = scripted(vec![Err(AttemptFailure::Fatal(anyhow::anyhow!(
            "embedding endpoint returned HTTP 401: nope"
        )))]);
        let (result, _) = run_retry_loop(3, attempt);
        assert!(result.is_err());
        assert_eq!(calls.get(), 1, "a 401 is not retried");
    }

    #[test]
    fn retry_loop_with_no_retries_returns_the_first_error_unwrapped() {
        let (attempt, calls) = scripted(vec![Err(transient(503, None))]);
        let (result, slept) = run_retry_loop(0, attempt);
        let err = result.expect_err("fails");
        assert_eq!(calls.get(), 1);
        assert!(slept.is_empty());
        assert_eq!(
            err.to_string(),
            "embedding endpoint returned HTTP 503: body-503"
        );

        let (attempt, calls) = scripted(vec![Err(transient(429, Some(Duration::from_secs(120))))]);
        let (result, _) = run_retry_loop(0, attempt);
        assert_eq!(
            result.expect_err("fails").to_string(),
            "embedding endpoint returned HTTP 429: body-429"
        );
        assert_eq!(calls.get(), 1);
    }

    #[test]
    fn retry_loop_with_ten_retries_caps_every_wait_at_sixty_seconds() {
        let (attempt, calls) = scripted((0..11).map(|_| Err(transient(503, None))).collect());
        let (result, slept) = run_retry_loop(MAX_EMBEDDING_RETRIES, attempt);
        assert!(result.is_err());
        assert_eq!(calls.get(), 11);
        let secs: Vec<u64> = slept.iter().map(Duration::as_secs).collect();
        assert_eq!(secs, vec![1, 2, 4, 8, 16, 32, 60, 60, 60, 60]);
    }

    #[test]
    fn jitter_from_clock_stays_below_a_quarter_of_the_base() {
        for base in [Duration::from_secs(1), Duration::from_secs(60)] {
            for _ in 0..100 {
                assert!(jitter_from_clock(base) < base / 4, "{base:?}");
            }
        }
        assert_eq!(jitter_from_clock(Duration::ZERO), Duration::ZERO);
    }

    #[test]
    fn with_limits_rejects_zero_chars_and_more_than_ten_retries() {
        let config = || openai_config_for_endpoint("http://127.0.0.1:8001/v1/embeddings").unwrap();
        let err = config().with_limits(Some(0), 3).expect_err("0 chars");
        assert!(
            err.to_string()
                .contains("[embedding].max_input_chars must be greater than zero")
        );
        let err = config()
            .with_limits(Some(8000), 11)
            .expect_err("11 retries");
        assert!(
            err.to_string()
                .contains("[embedding].max_retries must be at most 10")
        );
        let ok = config().with_limits(Some(8000), 10).expect("10 retries");
        let debug = format!("{ok:?}");
        assert!(debug.contains("max_input_chars: Some(8000)"), "{debug}");
        assert!(debug.contains("max_retries: 10"), "{debug}");
        let plain = format!("{:?}", config());
        assert!(
            plain.contains("max_input_chars: None") && plain.contains("max_retries: 0"),
            "{plain}"
        );
    }

    struct RefusingProvider;

    impl EmbeddingProvider for RefusingProvider {
        fn embed_documents(&mut self, _texts: &[&str]) -> Result<Vec<Vec<f32>>> {
            Err(anyhow::Error::new(EmbedInputRejected {
                status: 413,
                body_snippet: String::new(),
            }))
        }

        fn embed_query(&mut self, _text: &str) -> Result<Vec<f32>> {
            Ok(vec![3.0, 4.0])
        }
    }

    #[test]
    fn documents_embedded_counts_only_successful_document_embeds() {
        let identity = EmbeddingSettings::fastembed(ModelChoice::BgeSmallEnV15).identity;
        let mut ok = Embedder::from_provider(Box::new(StubProvider), identity.clone());
        ok.embed_texts(&["a"]).unwrap();
        ok.embed_texts(&["b", "c"]).unwrap();
        ok.embed_single("query").unwrap();
        ok.embed_queries(&["q1", "q2"]).unwrap();
        ok.probe_before_reset().unwrap();
        assert_eq!(
            ok.documents_embedded(),
            2,
            "one per successful embed_texts call"
        );

        let mut refused = Embedder::from_provider(Box::new(RefusingProvider), identity);
        assert!(refused.embed_texts(&["a"]).is_err());
        assert_eq!(refused.documents_embedded(), 0);
    }

    /// Refuses every call the way a later batch is refused after an earlier
    /// one was accepted.
    struct RefusingAfterAcceptingProvider;

    impl EmbeddingProvider for RefusingAfterAcceptingProvider {
        fn embed_documents(&mut self, _texts: &[&str]) -> Result<Vec<Vec<f32>>> {
            Err(anyhow::Error::new(EmbedInputRejected {
                status: 413,
                body_snippet: String::new(),
            })
            .context(RejectedAfterAccepting {
                accepted_inputs: OPENAI_COMPATIBLE_BATCH_SIZE,
            }))
        }

        fn embed_query(&mut self, _text: &str) -> Result<Vec<f32>> {
            Ok(vec![3.0, 4.0])
        }
    }

    /// (AW-04) A refusal after an accepted batch is counted apart, and the
    /// indexer still finds the typed rejection underneath to skip the file.
    #[test]
    fn documents_refused_after_accepting_counts_only_refusals_after_an_accepted_batch() {
        let identity = EmbeddingSettings::fastembed(ModelChoice::BgeSmallEnV15).identity;
        let mut partial =
            Embedder::from_provider(Box::new(RefusingAfterAcceptingProvider), identity.clone());
        let err = partial.embed_texts(&["a"]).expect_err("refused");
        assert_eq!(
            err.downcast_ref::<EmbedInputRejected>().map(|r| r.status),
            Some(413)
        );
        assert_eq!(partial.documents_refused_after_accepting(), 1);
        assert_eq!(partial.documents_embedded(), 0);

        let mut outright = Embedder::from_provider(Box::new(RefusingProvider), identity.clone());
        assert!(outright.embed_texts(&["a"]).is_err());
        assert_eq!(outright.documents_refused_after_accepting(), 0);

        let mut ok = Embedder::from_provider(Box::new(StubProvider), identity);
        ok.embed_texts(&["a"]).unwrap();
        assert_eq!(ok.documents_refused_after_accepting(), 0);
    }

    /// (feature-69) The inputs of the document embeds that succeeded, which the indexer
    /// reads before and after a document. Queries, the probe and refused calls add nothing.
    #[test]
    fn texts_embedded_counts_the_inputs_of_successful_document_embeds() {
        let identity = EmbeddingSettings::fastembed(ModelChoice::BgeSmallEnV15).identity;
        let mut ok = Embedder::from_provider(Box::new(StubProvider), identity.clone());
        ok.embed_texts(&["a"]).unwrap();
        ok.embed_texts(&["b", "c"]).unwrap();
        ok.embed_single("query").unwrap();
        ok.embed_queries(&["q1", "q2"]).unwrap();
        ok.probe_before_reset().unwrap();
        assert_eq!(
            ok.texts_embedded(),
            3,
            "one per input of a successful embed_texts"
        );

        let mut refused = Embedder::from_provider(Box::new(RefusingProvider), identity.clone());
        assert!(refused.embed_texts(&["a", "b"]).is_err());
        assert_eq!(refused.texts_embedded(), 0);

        let mut partial =
            Embedder::from_provider(Box::new(RefusingAfterAcceptingProvider), identity);
        assert!(partial.embed_texts(&["a", "b"]).is_err());
        assert_eq!(partial.texts_embedded(), 0);
    }

    /// (AW-04) What an MCP reply may say about an embedding error: a non-2xx
    /// answer by its status only, any message groove worded itself as it is.
    #[test]
    fn body_free_message_names_the_status_and_never_the_body() {
        let secret = "SECRET-BODY";
        let status = anyhow::Error::new(EmbedHttpStatus {
            status: 401,
            body_snippet: secret.to_string(),
        });
        assert!(
            status.to_string().contains(secret),
            "the CLI keeps the body"
        );
        assert_eq!(
            body_free_message(&status),
            "embedding endpoint returned HTTP 401"
        );
        let rejected = anyhow::Error::new(EmbedInputRejected {
            status: 413,
            body_snippet: secret.to_string(),
        });
        assert_eq!(
            body_free_message(&rejected),
            "embedding endpoint returned HTTP 413"
        );
        let wrapped = anyhow::Error::new(EmbedHttpStatus {
            status: 503,
            body_snippet: secret.to_string(),
        })
        .context("embedding endpoint still failing after 4 attempts (last: HTTP 503)");
        assert_eq!(
            body_free_message(&wrapped),
            "embedding endpoint still failing after 4 attempts (last: HTTP 503): \
             embedding endpoint returned HTTP 503"
        );
        let other = anyhow::anyhow!("embedding endpoint returned malformed JSON");
        assert_eq!(
            body_free_message(&other),
            "embedding endpoint returned malformed JSON"
        );
    }

    /// (AW-04, codex P2 round 3 on PR #329) The indexer wraps a document-side
    /// refusal in its own context, sometimes over the retry loop's; the status
    /// is found under both and every context above it is kept.
    #[test]
    fn body_free_message_finds_the_status_under_several_contexts() {
        let secret = "SECRET-BODY";
        let nested = anyhow::Error::new(EmbedHttpStatus {
            status: 401,
            body_snippet: secret.to_string(),
        })
        .context("failed to embed chunks for note.md");
        assert_eq!(
            body_free_message(&nested),
            "failed to embed chunks for note.md: embedding endpoint returned HTTP 401"
        );
        let twice = anyhow::Error::new(EmbedHttpStatus {
            status: 503,
            body_snippet: secret.to_string(),
        })
        .context("embedding endpoint still failing after 4 attempts (last: HTTP 503)")
        .context("failed to embed chunks for note.md");
        let message = body_free_message(&twice);
        assert_eq!(
            message,
            "failed to embed chunks for note.md: embedding endpoint still failing after 4 \
             attempts (last: HTTP 503): embedding endpoint returned HTTP 503"
        );
        assert!(!message.contains(secret), "{message}");
    }

    /// (AW-16) A 2xx whose JSON has the wrong shape is named by the kind of
    /// fault only: `serde_json` quotes the offending value, and that value is
    /// the endpoint's response body.
    ///
    /// Red if the serde layer's own text is joined into the message: the
    /// sentinel the body carries would be quoted in it.
    #[test]
    fn body_free_message_names_a_malformed_answer_without_quoting_it() {
        let secret = "SECRET-BODY";
        let body = format!(r#"{{"data":[{{"index":0,"embedding":"{secret}"}}]}}"#);
        let error = parse_embedding_response(body.as_bytes(), 1, 2)
            .expect_err("a string where the vector should be")
            .context("failed to embed chunks for note.md");
        assert!(
            format!("{error:#}").contains(secret),
            "the CLI keeps serde's text: {error:#}"
        );
        let message = body_free_message(&error);
        assert_eq!(
            message,
            "failed to embed chunks for note.md: embedding endpoint returned malformed JSON: \
             malformed JSON (data)"
        );
        assert!(!message.contains(secret), "{message}");
    }

    /// (AW-16) Without a status or a serde error in the chain, every layer is
    /// groove's own wording, and all of them are kept: the outermost alone
    /// (`failed to embed chunks for <path>`) never says what went wrong.
    #[test]
    fn body_free_message_joins_every_layer_of_a_chain_without_a_status() {
        let error = anyhow::anyhow!("failed to read embedding response body: timed out")
            .context("failed to embed chunks for note.md");
        assert_eq!(
            body_free_message(&error),
            "failed to embed chunks for note.md: failed to read embedding response body: \
             timed out"
        );
    }

    /// (AW-16) A request that never got an answer keeps what `reqwest` said
    /// caused it, still without the URL: `error sending request` alone does
    /// not tell a timeout from a refused connection.
    ///
    /// Red if the transport error is built from the `reqwest::Error`'s
    /// `Display` alone, which never prints its source.
    #[test]
    fn a_transport_error_names_its_cause_without_the_url() {
        let (endpoint, _captured, handle) = mock_embedding_server(
            "200 OK",
            r#"{"data":[{"embedding":[1.0,2.0],"index":0}]}"#,
            Duration::from_millis(200),
        );
        let host = endpoint
            .trim_start_matches("http://")
            .split('/')
            .next()
            .expect("host:port")
            .to_string();
        let mut embedder = Embedder::with_settings(EmbeddingSettings::openai_compatible(
            openai_config(endpoint, 2, Duration::from_millis(10)),
        ))
        .expect("build embedder");
        let err = embedder
            .embed_single("needle")
            .expect_err("request must time out");
        let chain = format!("{err:#}");
        assert!(
            chain.contains("embedding request failed: error sending request: "),
            "{chain}"
        );
        assert!(chain.contains("timed out"), "{chain}");
        assert!(!chain.contains(&host), "the URL leaked: {chain}");
        handle.join().expect("mock server thread");
    }

    /// (kuriya issue #494) A client that connects and closes before its
    /// headers are whole, as a short client timeout does under load, ends
    /// [`mock_embedding_server`]'s thread with [`MockEnd::ClientClosed`]: it does
    /// not panic, and it captures no request.
    ///
    /// Red if [`try_read_http_request`] panics on a read of `Ok(0)` before the
    /// headers: the thread panics and joining it fails.
    #[test]
    fn the_mock_server_records_a_client_that_closed_before_its_headers() {
        for sent in [&b""[..], &b"POST /v1/embeddings HTTP/1.1\r\n"[..]] {
            let (endpoint, captured, handle) =
                mock_embedding_server("200 OK", "{}", Duration::ZERO);
            let addr = endpoint
                .trim_start_matches("http://")
                .split('/')
                .next()
                .expect("host:port")
                .to_string();
            let mut client = std::net::TcpStream::connect(addr).expect("connect to the mock");
            client.write_all(sent).expect("write the partial request");
            drop(client);
            assert_eq!(
                handle.join().expect("mock server thread"),
                MockEnd::ClientClosed,
                "sent {sent:?}"
            );
            assert!(captured.try_recv().is_err(), "sent {sent:?}");
        }
    }

    /// A reader that hands out a fixed list of reads, then end of stream.
    struct ScriptedReader(std::collections::VecDeque<std::io::Result<Vec<u8>>>);

    impl Read for ScriptedReader {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            match self.0.pop_front() {
                None => Ok(0),
                Some(Err(error)) => Err(error),
                Some(Ok(bytes)) => {
                    buf[..bytes.len()].copy_from_slice(&bytes);
                    Ok(bytes.len())
                }
            }
        }
    }

    /// (kuriya issue #494, local Codex round 1) A connection reset or aborted
    /// before the headers are whole is a client that closed, as a read of
    /// `Ok(0)` is: [`try_read_http_request`] returns `None`, at once or after a
    /// partial request line.
    ///
    /// Red if a read error of kind `ConnectionReset` or `ConnectionAborted`
    /// before the headers panics like any other.
    #[test]
    fn a_reset_or_aborted_connection_before_the_headers_reads_as_closed() {
        use std::io::{Error, ErrorKind};
        for kind in [ErrorKind::ConnectionReset, ErrorKind::ConnectionAborted] {
            for before in [None, Some(b"POST /v1/embeddings HTTP/1.1\r\n".to_vec())] {
                let mut reads: std::collections::VecDeque<_> =
                    before.clone().map(Ok).into_iter().collect();
                reads.push_back(Err(Error::from(kind)));
                assert_eq!(
                    try_read_http_request(&mut ScriptedReader(reads)),
                    None,
                    "{kind:?} after {before:?}"
                );
            }
        }
    }

    /// (kuriya issue #494, local Codex round 1) Any other read error before
    /// the headers still panics: only a client's close is read as `None`.
    ///
    /// Red if [`try_read_http_request`] reads every error as a closed client.
    #[test]
    #[should_panic(expected = "read mock request")]
    fn another_read_error_before_the_headers_still_panics() {
        let reads = std::collections::VecDeque::from([Err(std::io::Error::other("broken"))]);
        let _ = try_read_http_request(&mut ScriptedReader(reads));
    }

    /// (codex P1 round 1 on PR #332) An `io::Error` under a transport error
    /// is named by its kind (and OS error code), not by its text: on a
    /// non-English Windows the OS words that text in the local language, and
    /// stderr stays ASCII. Any other cause is escaped to ASCII.
    ///
    /// Red if [`ascii_cause`] appends a cause's `Display` as it is.
    #[test]
    fn a_transport_cause_is_written_in_ascii() {
        let localized = "接続が拒否されました";
        let refused = std::io::Error::new(std::io::ErrorKind::ConnectionRefused, localized);
        let text = ascii_cause(&refused);
        assert!(text.is_ascii(), "{text}");
        assert!(text.contains("ConnectionRefused"), "{text}");
        assert!(!text.contains(localized), "{text}");

        let os = std::io::Error::from_raw_os_error(10061);
        let text = ascii_cause(&os);
        assert!(text.is_ascii(), "{text}");
        assert!(text.ends_with(" (os error 10061)"), "{text}");

        // Built from a variable, not an `anyhow!` literal: the stderr ASCII
        // scanner reads every diagnostic macro, test code included.
        let localized_other = "証明書 mismatch";
        let other = anyhow::Error::msg(localized_other);
        let text = ascii_cause(other.as_ref());
        assert!(text.is_ascii(), "{text}");
        assert!(text.contains("mismatch"), "{text}");
    }

    /// (local Codex round 2 on PR #332) The walk [`transport_message`] runs
    /// ([`with_ascii_causes`]) writes every cause in ASCII, an OS error by its
    /// kind: the test above checks one cause, this one the chain as the
    /// transport error builds it.
    ///
    /// Red if the walk appends a cause's `Display` instead of going through
    /// [`ascii_cause`].
    #[test]
    fn the_transport_cause_walk_writes_every_cause_in_ascii() {
        #[derive(Debug)]
        struct Connect(std::io::Error);
        impl fmt::Display for Connect {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("tcp connect error")
            }
        }
        impl std::error::Error for Connect {
            fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
                Some(&self.0)
            }
        }

        let localized = "接続が拒否されました";
        let connect = Connect(std::io::Error::new(
            std::io::ErrorKind::ConnectionRefused,
            localized,
        ));
        let text = with_ascii_causes("error sending request".to_string(), Some(&connect));
        assert!(text.is_ascii(), "{text}");
        assert!(
            text.starts_with("error sending request: tcp connect error: "),
            "{text}"
        );
        assert!(text.contains("ConnectionRefused"), "{text}");
        assert!(!text.contains(localized), "{text}");
    }
}
