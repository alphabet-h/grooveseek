use anyhow::{Context, Result};
use fastembed::{
    EmbeddingModel, InitOptions, RerankInitOptions, RerankerModel, TextEmbedding, TextRerank,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fmt;
use std::path::PathBuf;
use std::time::Duration;

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
        })
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
            self.client = Some(
                reqwest::blocking::Client::builder()
                    .timeout(self.config.timeout)
                    .redirect(reqwest::redirect::Policy::none())
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

        let mut embeddings = Vec::with_capacity(texts.len());
        for batch in texts.chunks(OPENAI_COMPATIBLE_BATCH_SIZE) {
            embeddings.extend(self.embed_batch(batch, model)?);
        }
        Ok(embeddings)
    }

    fn embed_batch(&mut self, texts: &[&str], model: &str) -> Result<Vec<Vec<f32>>> {
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
        let mut builder = self.client()?.post(endpoint).json(&request);
        if let Some(api_key) = api_key {
            builder = builder.bearer_auth(api_key);
        }
        let response = builder.send().map_err(|error| {
            anyhow::anyhow!("embedding request failed: {}", error.without_url())
        })?;
        let status = response.status();
        let body = response.bytes().map_err(|error| {
            anyhow::anyhow!(
                "failed to read embedding response body: {}",
                error.without_url()
            )
        })?;
        if !status.is_success() {
            anyhow::bail!(
                "embedding endpoint returned HTTP {}: {}",
                status.as_u16(),
                escaped_body_snippet(&body)
            );
        }
        let parsed: OpenAiEmbeddingResponse =
            serde_json::from_slice(&body).context("embedding endpoint returned malformed JSON")?;
        anyhow::ensure!(
            parsed.data.len() == texts.len(),
            "embedding endpoint returned {} vectors for {} inputs",
            parsed.data.len(),
            texts.len()
        );

        let mut ordered: Vec<Option<Vec<f32>>> = vec![None; texts.len()];
        for item in parsed.data {
            anyhow::ensure!(
                item.index < ordered.len(),
                "embedding endpoint returned out-of-range index {} for {} inputs",
                item.index,
                ordered.len()
            );
            anyhow::ensure!(
                item.embedding.len() == self.config.dimension,
                "embedding endpoint returned dimension {} at index {}; expected {}",
                item.embedding.len(),
                item.index,
                self.config.dimension
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

/// Provider-neutral entry point for generating text embeddings.
pub struct Embedder {
    provider: Box<dyn EmbeddingProvider>,
    identity: EmbeddingIdentity,
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
        Self { provider, identity }
    }

    /// Embed document texts. Provider-specific batching stays behind the
    /// provider boundary.
    ///
    /// This is the document side: an OpenAI-compatible endpoint answers it with
    /// its `document_model`. Queries go through [`Embedder::embed_single`] or
    /// [`Embedder::embed_queries`].
    pub fn embed_texts(&mut self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        self.provider.embed_documents(texts)
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
}
