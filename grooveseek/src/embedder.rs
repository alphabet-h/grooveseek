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

    /// fastembed の `embed()` に渡すバッチサイズ。モデルの 1 トークンあたりの
    /// activation memory が違うため、大きなモデルでは小さめのバッチに絞って
    /// OOM を避ける。
    ///
    /// 計算根拠: `batch * max_length(=512) * hidden_dim * 4 bytes`
    /// - BgeSmallEnV15 (384 dim) @ 256 → ~200 MB
    /// - BgeM3         (1024 dim) @ 32 → ~67 MB
    pub fn batch_size(self) -> usize {
        match self {
            Self::BgeSmallEnV15 => 256,
            Self::BgeM3 => 32,
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

struct FastEmbedProvider {
    model: TextEmbedding,
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

impl EmbeddingProvider for FastEmbedProvider {
    fn embed_documents(&mut self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        self.model.embed(texts, Some(self.choice.batch_size()))
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
/// response body -- an MCP client (ADR-0025). When a non-2xx answer
/// ([`EmbedInputRejected`] or any other status) is anywhere in the chain, the
/// result is every message above it -- contexts groove wrote itself, such as
/// the indexer's `failed to embed chunks for <path>` or the retry loop's
/// give-up line -- followed by that status alone, so the answer's text (which
/// carries the body) is never used. Without one, it is the outermost message,
/// also groove's own (a timeout, a malformed answer): `anyhow::Error`'s
/// `Display` never prints the causes underneath.
pub(crate) fn body_free_message(error: &anyhow::Error) -> String {
    let mut above = Vec::new();
    for cause in error.chain() {
        let status = cause
            .downcast_ref::<EmbedInputRejected>()
            .map(|r| r.status)
            .or_else(|| cause.downcast_ref::<EmbedHttpStatus>().map(|s| s.status));
        if let Some(status) = status {
            above.push(format!("embedding endpoint returned HTTP {status}"));
            return above.join(": ");
        }
        above.push(cause.to_string());
    }
    error.to_string()
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
fn endpoint_is_loopback(url: &reqwest::Url) -> bool {
    let Some(host) = url.host_str() else {
        return false;
    };
    // `host_str` keeps the brackets of an IPv6 literal.
    let bare = host
        .strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(host);
    match bare.parse::<std::net::IpAddr>() {
        Ok(std::net::IpAddr::V4(ip)) => ip.is_loopback(),
        Ok(std::net::IpAddr::V6(ip)) => {
            ip.is_loopback() || ip.to_ipv4_mapped().is_some_and(|v4| v4.is_loopback())
        }
        Err(_) => host
            .strip_suffix('.')
            .unwrap_or(host)
            .eq_ignore_ascii_case("localhost"),
    }
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
        let client = self.client().map_err(AttemptFailure::Fatal)?;
        let mut builder = client.post(endpoint).json(&request);
        if let Some(api_key) = api_key {
            builder = builder.bearer_auth(api_key);
        }
        let response = match builder.send() {
            Ok(response) => response,
            Err(error) => {
                let last = if error.is_timeout() {
                    Some("timed out")
                } else if error.is_connect() {
                    Some("connection failed")
                } else {
                    None
                };
                let error = anyhow::anyhow!("embedding request failed: {}", error.without_url());
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
        let body = match response.bytes() {
            Ok(body) => body,
            Err(_) if class != StatusClass::Success => Default::default(),
            // A 2xx whose vectors never arrived: a timeout is retried, any other broken body
            // is not.
            Err(error) => {
                let timed_out = error.is_timeout();
                let error = anyhow::anyhow!(
                    "failed to read embedding response body: {}",
                    error.without_url()
                );
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
/// index in range and unique, none missing, each of the declared dimension.
/// Never retried: the server would give the same answer again.
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
    for item in parsed.data {
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
    /// the CLI prints the whole chain, HTTP status included, while the MCP
    /// `rebuild_index {force: true}` reply shows the outermost message only,
    /// so the endpoint's response body does not reach an MCP caller.
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

/// reranker 入力用に context を前置する (feature-46)。context_text が空/None なら
/// content のみ (off の DB / context なし chunk では従来と同一)。Anthropic 原典の
/// 結合形 `f"{context}\n\n{chunk}"` に忠実。
fn contextualize_for_rerank(r: &SearchResult) -> String {
    match r.context_text.as_deref() {
        Some(ctx) if !ctx.trim().is_empty() => format!("{ctx}\n\n{}", r.content),
        _ => r.content.clone(),
    }
}

/// Cross-encoder reranker。`search_hybrid` が返した候補を query との共同
/// エンコードで再スコア付けし、上位 `limit` 件に絞る。
pub struct Reranker {
    model: TextRerank,
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
        let model = TextRerank::try_new(
            RerankInitOptions::new(fm)
                .with_cache_dir(resolve_cache_dir()?)
                .with_show_download_progress(true),
        )?;
        Ok(Some(Self { model, choice }))
    }

    /// `candidates` (chunk_id, SearchResult) を cross-encoder でスコア付けし、
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
        // context 込みで rerank する (D-3)。&str 参照ベースだと合成文字列を保持できないので
        // 一時 Vec<String> バッファを経由する。
        let contextualized: Vec<String> = candidates
            .iter()
            .map(|(_, r)| contextualize_for_rerank(r))
            .collect();
        let documents: Vec<&str> = contextualized.iter().map(String::as_str).collect();
        let rerank_results = self.model.rerank(query, documents, false, None)?;

        // rerank_results は score 降順でソート済み。index は documents (= candidates) の位置。
        let mut out: Vec<(i64, SearchResult)> = Vec::with_capacity(limit as usize);
        for r in rerank_results.into_iter().take(limit as usize) {
            let Some((id, mut row)) = candidates.get(r.index).cloned() else {
                continue;
            };
            row.score = r.score;
            out.push((id, row));
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::mpsc;
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

    fn mock_embedding_server(
        status: &str,
        response_body: &str,
        delay: Duration,
    ) -> (String, mpsc::Receiver<String>, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock server");
        let addr = listener.local_addr().expect("mock server address");
        let status = status.to_string();
        let response_body = response_body.to_string();
        let (tx, rx) = mpsc::channel();
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept mock request");
            tx.send(read_http_request(&mut stream))
                .expect("send captured request");
            thread::sleep(delay);
            write_http_response(&mut stream, &status, &response_body);
        });
        (format!("http://{addr}/v1/embeddings"), rx, handle)
    }

    fn read_http_request(stream: &mut std::net::TcpStream) -> String {
        let mut request = Vec::new();
        let mut buf = [0_u8; 4096];
        let (header_end, content_length) = loop {
            let read = stream.read(&mut buf).expect("read mock request");
            assert!(read > 0, "request ended before its headers");
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
        String::from_utf8_lossy(&request).into_owned()
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
                            "embedding": vec![index as f32; dimension],
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
        assert_eq!(vectors, vec![vec![1.0, 2.0], vec![3.0, 4.0]]);
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
        assert_eq!(vectors[63], vec![63.0, 63.0]);
        assert_eq!(vectors[64], vec![0.0, 0.0]);
        assert_eq!(vectors[128], vec![0.0, 0.0]);

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
        assert_eq!(vectors, vec![vec![1.0, 2.0], vec![3.0, 4.0]]);
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
}
