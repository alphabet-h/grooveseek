use anyhow::Result;
use fastembed::{
    EmbeddingModel, InitOptions, RerankInitOptions, RerankerModel, TextEmbedding, TextRerank,
};
use serde::Deserialize;
use std::path::PathBuf;

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
    /// BAAI/bge-small-en-v1.5 (384 dim, 英語特化, ~130 MB)
    #[default]
    #[value(name = "bge-small-en-v1.5")]
    #[serde(rename = "bge-small-en-v1.5")]
    BgeSmallEnV15,
    /// BAAI/bge-m3 (1024 dim, 多言語, ~2.3 GB)
    #[value(name = "bge-m3")]
    #[serde(rename = "bge-m3")]
    BgeM3,
}

impl ModelChoice {
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

/// Thin wrapper around fastembed for generating text embeddings.
///
/// モデルは [`ModelChoice`] で切替可能。ONNX モデルは初回実行時に
/// [`resolve_cache_dir`] のキャッシュディレクトリへダウンロードされる。
pub struct Embedder {
    model: TextEmbedding,
    choice: ModelChoice,
}

impl Embedder {
    /// デフォルトモデル ([`ModelChoice::default`]) で初期化する。
    ///
    /// Cache directory resolution (in order):
    /// 1. `FASTEMBED_CACHE_DIR` environment variable if set
    /// 2. OS-standard cache directory joined with `fastembed`
    ///    (Linux: `~/.cache/fastembed`, macOS: `~/Library/Caches/fastembed`,
    ///    Windows: `%LOCALAPPDATA%\fastembed`)
    /// 3. `.fastembed_cache` relative to the working directory (fastembed's own default)
    pub fn new() -> Result<Self> {
        Self::with_model(ModelChoice::default())
    }

    /// 明示的にモデルを指定して初期化する。
    pub fn with_model(choice: ModelChoice) -> Result<Self> {
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

    /// Embed multiple texts in a batch. バッチサイズは `ModelChoice::batch_size()`
    /// から決定 (大きなモデルで OOM を起こさないよう明示的に絞る)。
    pub fn embed_texts(&mut self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        let embeddings = self.model.embed(texts, Some(self.choice.batch_size()))?;
        Ok(embeddings)
    }

    /// Embed a single text.
    pub fn embed_single(&mut self, text: &str) -> Result<Vec<f32>> {
        let mut results = self.embed_texts(&[text])?;
        results
            .pop()
            .ok_or_else(|| anyhow::anyhow!("embedding returned empty result"))
    }

    /// 選択中のモデルの埋め込み次元数。
    pub fn dimension(&self) -> usize {
        self.choice.dimension()
    }

    /// 選択中のモデルの識別子 (index_meta に記録される)。
    pub fn model_id(&self) -> &'static str {
        self.choice.model_id()
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
    /// reranker 無効 (RRF 結果をそのまま返す)
    #[default]
    #[value(name = "none")]
    #[serde(rename = "none")]
    None,
    /// BAAI/bge-reranker-v2-m3 (多言語 100+ 言語, ~2.3 GB)。日本語 KB 推奨
    #[value(name = "bge-v2-m3")]
    #[serde(rename = "bge-v2-m3")]
    BgeV2M3,
    /// jinaai/jina-reranker-v2-base-multilingual (多言語, ~1.2 GB)。軽量多言語
    #[value(name = "jina-v2-ml")]
    #[serde(rename = "jina-v2-ml")]
    JinaV2Multilingual,
    /// BAAI/bge-reranker-base (英/中のみ, ~280 MB)。日本語用途には非推奨
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
