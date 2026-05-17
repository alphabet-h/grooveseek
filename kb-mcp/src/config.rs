//! バイナリと同じディレクトリに配置する `kb-mcp.toml` の読み込み。
//!
//! サーバ運用側が `--model` / `--reranker` / `FASTEMBED_CACHE_DIR` 等の
//! オプションを省略できるよう、設定ファイルでデフォルト値を与える。
//! 優先順位は `CLI 引数 > 設定ファイル > ビルトインデフォルト`。

use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::{Path, PathBuf};

use crate::embedder::{ModelChoice, RerankerChoice};
use crate::parser::ParsersConfig;
use crate::quality::QualityFilterConfig;
use crate::transport::TransportConfig;
use crate::watcher::WatchConfig;

/// インデックス走査時にスキップするディレクトリ basename の既定リスト。
/// basename 完全一致 (substring や glob ではない)。`kb-mcp.toml` の
/// `exclude_dirs` キーを指定するとこのリスト全体が置き換わる
/// (merge ではない)。`exclude_dirs = []` を明示すると「全走査」になる。
pub const DEFAULT_EXCLUDE_DIRS: &[&str] = &[
    ".obsidian",
    ".git",
    "node_modules",
    "target",
    ".vscode",
    ".idea",
];

/// バイナリと同じディレクトリに置く `kb-mcp.toml` の表現。
/// すべてのフィールドは optional で、指定しなかった項目は CLI 引数 or
/// ビルトインデフォルトで補われる。
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// `--kb-path` の既定値。
    pub kb_path: Option<PathBuf>,
    /// `--model` の既定値 (例: `"bge-m3"`)。
    pub model: Option<ModelChoice>,
    /// `--reranker` の既定値 (例: `"bge-v2-m3"`)。
    pub reranker: Option<RerankerChoice>,
    /// `--rerank-by-default` の既定値。
    pub rerank_by_default: Option<bool>,
    /// `FASTEMBED_CACHE_DIR` 環境変数の既定値。
    /// 既に env が設定されていればそちらを優先し、未設定のときだけ適用する。
    pub fastembed_cache_dir: Option<PathBuf>,
    /// Markdown チャンク化時に除外する見出し文字列の一覧 (substring match)。
    /// 省略時 (`None`) は [`crate::markdown::DEFAULT_EXCLUDED_HEADINGS`]。
    /// 明示的に `[]` を与えると「除外しない」という意味になる。
    pub exclude_headings: Option<Vec<String>>,
    /// インデックス走査時にスキップするディレクトリ basename (完全一致)。
    /// 省略時は [`DEFAULT_EXCLUDE_DIRS`] が適用される。明示的な `[]` を
    /// 与えると「全ディレクトリを走査する」という意味になる。
    pub exclude_dirs: Option<Vec<String>>,
    /// 検索時に適用するチャンク品質フィルタの設定。
    /// 省略時は [`QualityFilterConfig::default()`] (enabled=true, threshold=0.3)。
    pub quality_filter: Option<QualityFilterConfig>,
    /// `get_best_practice` MCP ツールで使うパス候補テンプレート (opt-in)。
    /// 省略時 (`None`) または空リストの場合、`get_best_practice` ツールは
    /// "not configured" エラーを返す (ツール自体は MCP に登録されるが機能しない)。
    pub best_practice: Option<BestPracticeConfig>,
    /// Indexing 対象の拡張子リスト。
    /// 省略時 (`None`) は `["md"]` のみ (legacy 完全後方互換)。`.txt`
    /// 等を取り込みたい場合は明示的に `enabled = ["md", "txt"]` と opt-in する。
    /// 空配列 `enabled = []` は誤設定として reject する。
    pub parsers: Option<ParsersConfig>,
    /// serve 中のファイルウォッチャー設定。
    /// 省略時 (`None`) は `WatchConfig::default()` (enabled=true, debounce=500ms)。
    /// CLI `--no-watch` で即座に無効化できる。
    pub watch: Option<WatchConfig>,
    /// serve が listen するトランスポート。
    /// 省略時 (`None`) は stdio (1 クライアント限定、legacy 後方互換)。
    /// CLI `--transport http` で HTTP 起動に切り替え。
    pub transport: Option<TransportConfig>,
    /// `kb-mcp eval` (retrieval quality evaluation) の opt-in 設定。
    /// 省略時 (`None`) は全デフォルト値で走る。詳細は `docs/eval.md`。
    pub eval: Option<EvalConfig>,
    /// `kb-mcp search` / MCP `search` ツールの設定。
    pub search: Option<SearchConfig>,
}

/// `get_best_practice` の opt-in 設定。
///
/// `path_templates` に列挙した順に `{target}` を置換してファイルを探し、
/// 最初に存在したものを返す。テンプレート変数:
///   - `{target}` : ツールに渡された target パラメータ
///
/// セクションを設定しない (または空リストを明示する) と、ツールは
/// "not configured" エラーで応答する。
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BestPracticeConfig {
    #[serde(default)]
    pub path_templates: Vec<String>,
}

/// `[search]` セクション。feature-26 で追加。
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SearchConfig {
    /// rank-based low_confidence 判定の閾値 (top1.score / mean(top-N.score) < ratio で立つ)。
    /// 省略時は 1.5。0.0 は判定無効。
    pub min_confidence_ratio: Option<f32>,
    /// MMR (Maximal Marginal Relevance) post-rerank 多様化設定。
    /// セクション省略時は [`MmrConfig::default()`] (enabled=false)。
    #[serde(default)]
    pub mmr: MmrConfig,
    /// Parent retriever (display-time content expansion) 設定。
    /// セクション省略時は [`ParentRetrieverConfig::default()`] (enabled=false)。
    #[serde(default)]
    pub parent_retriever: ParentRetrieverConfig,
}

/// `[search.mmr]` セクション。feature-28 PR-2 で追加。
///
/// rerank 後にチャンク類似度ペナルティで多様化する MMR を opt-in で有効化する。
/// 既定は無効。詳細は `docs/feature-28-mmr.md` (TBD) 参照。
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MmrConfig {
    /// MMR を有効にするか。`false` ならパイプラインは従来通り (rerank 結果を
    /// そのまま score 降順で返す)。
    #[serde(default)]
    pub enabled: bool,
    /// MMR の relevance / diversity tradeoff 係数。`1.0` で多様化なし
    /// (= MMR 無効と等価)、`< 0.5` で diversity 寄り。0.0..=1.0。
    #[serde(default = "default_mmr_lambda")]
    pub lambda: f32,
    /// 同一ドキュメント内チャンク同士に追加で課す類似度ペナルティ。
    /// `0.0` で純粋 MMR、`> 0` で同一文書チャンクの重複抑制が強まる。0.0..=1.0。
    #[serde(default)]
    pub same_doc_penalty: f32,
}

fn default_mmr_lambda() -> f32 {
    0.7
}

impl Default for MmrConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            lambda: default_mmr_lambda(),
            same_doc_penalty: 0.0,
        }
    }
}

/// `[search.parent_retriever]` セクション。feature-28 PR-3 で追加。
///
/// 検索結果の content を表示拡張する Parent retriever (post-relevance) 設定。
/// 既定は無効。詳細は `docs/feature-28-parent-retriever.md` (TBD)。
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ParentRetrieverConfig {
    /// Parent retriever を有効にするか。`false` なら hit chunk の content
    /// をそのまま返す (拡張なし)。
    #[serde(default)]
    pub enabled: bool,
    /// `token_count` がこの値未満の chunk hit に対しては whole-document
    /// fallback (= 同 doc 全 chunks を連結) を適用する。0 < x < max_expanded_tokens。
    #[serde(default = "default_whole_doc_threshold")]
    pub whole_doc_threshold_tokens: u32,
    /// adjacent merge / whole-doc 共通の content size cap (token 単位)。
    /// 超えた場合 adjacent は hit chunk のみに、whole-doc は adjacent fallback
    /// に degrade する。BGE-M3 context window 8192 を上限とする。
    #[serde(default = "default_max_expanded")]
    pub max_expanded_tokens: u32,
}

fn default_whole_doc_threshold() -> u32 {
    100
}

fn default_max_expanded() -> u32 {
    2000
}

impl Default for ParentRetrieverConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            whole_doc_threshold_tokens: default_whole_doc_threshold(),
            max_expanded_tokens: default_max_expanded(),
        }
    }
}

/// 3 caller (MCP server, CLI search, CLI eval) から共通利用される per-call
/// override 構造体。`From` impls で各 caller の native 型から構築する。
///
/// `resolve` は `Some > toml > default` の precedence で effective config を
/// 計算し、MMR off だが lambda/penalty が `Some(_)` で渡された場合に
/// `tracing::warn!` を 1 度だけ発火する (footgun guard)。
#[derive(Debug, Clone, Default)]
pub struct SearchOverrides {
    pub mmr: Option<bool>,
    pub mmr_lambda: Option<f32>,
    pub mmr_same_doc_penalty: Option<f32>,
    pub parent_retriever: Option<bool>,
}

/// `SearchOverrides::resolve` の戻り値。effective config を集約する。
///
/// Parent retriever の数値フィールド (whole_doc_threshold_tokens /
/// max_expanded_tokens) は per-call override しないため、resolve 時点で
/// toml の値をそのままコピーする。
#[derive(Debug, Clone)]
pub struct ResolvedSearchConfig {
    pub mmr_enabled: bool,
    pub mmr_lambda: f32,
    pub mmr_same_doc_penalty: f32,
    pub parent_retriever_enabled: bool,
    pub parent_whole_doc_threshold_tokens: u32,
    pub parent_max_expanded_tokens: u32,
}

impl SearchOverrides {
    /// per-call の `Option`s と toml の値から effective config を計算。
    /// MMR が effective off だが lambda / penalty が `Some(_)` で渡された場合は
    /// `tracing::warn!` を 1 度だけ発火 (footgun guard)。eval-baseline ノートに
    /// ghost lambda が記録される事故を防ぐ。
    pub fn resolve(&self, toml: &SearchConfig) -> ResolvedSearchConfig {
        let mmr_enabled = self.mmr.unwrap_or(toml.mmr.enabled);
        let mmr_lambda = self.mmr_lambda.unwrap_or(toml.mmr.lambda);
        let mmr_penalty = self
            .mmr_same_doc_penalty
            .unwrap_or(toml.mmr.same_doc_penalty);

        if !mmr_enabled && (self.mmr_lambda.is_some() || self.mmr_same_doc_penalty.is_some()) {
            tracing::warn!(
                lambda = ?self.mmr_lambda,
                penalty = ?self.mmr_same_doc_penalty,
                "MMR override values were provided but effective MMR is off; values ignored"
            );
        }

        // Parent retriever: enabled のみ per-call override 可、threshold /
        // max_expanded は toml-only (per-call では渡せない、spec 通り)。
        ResolvedSearchConfig {
            mmr_enabled,
            mmr_lambda,
            mmr_same_doc_penalty: mmr_penalty,
            parent_retriever_enabled: self
                .parent_retriever
                .unwrap_or(toml.parent_retriever.enabled),
            parent_whole_doc_threshold_tokens: toml.parent_retriever.whole_doc_threshold_tokens,
            parent_max_expanded_tokens: toml.parent_retriever.max_expanded_tokens,
        }
    }
}

/// `kb-mcp eval` (retrieval quality evaluation) の設定。省略時は全デフォルトで走る。
/// 一般ユーザには不要。詳細は `docs/eval.md`。
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvalConfig {
    /// Golden YAML ファイルへのパス (kb-path 基準の相対 or 絶対)。省略時は
    /// `<kb_path>/.kb-mcp-eval.yml`。
    pub golden: Option<PathBuf>,
    /// 保持する過去実行の件数。省略時は 10。
    pub history_size: Option<usize>,
    /// 報告する k のリスト。省略時は `[1, 5, 10]`。
    pub k_values: Option<Vec<usize>>,
    /// diff 表示で recall が劣化して赤く出す閾値。省略時は 0.05。
    pub regression_threshold: Option<f64>,
}

impl Config {
    /// 後方互換シム。`discover(None)` に委譲し `ConfigSource` を捨てる。
    /// 実際の探索順は CWD → `.git` 祖先 → バイナリ隣 (legacy) で、
    /// 関数名は historical naming のまま残している。新規コードは
    /// [`Config::discover`] を直接呼び ConfigSource を tracing に乗せること。
    pub fn load_alongside_binary() -> Result<Self> {
        Self::discover(None).map(|(c, _)| c)
    }

    /// CLI `--config` で渡されたパスがあればそれを (絶対 / 相対 + `~` 展開した上で) 採用、
    /// なければ CWD → `.git` 祖先 → バイナリ隣の順で `kb-mcp.toml` を探し、
    /// 最初に見つかったものを読む。全部失敗したら `Config::default()` を返す。
    ///
    /// 戻り値の `ConfigSource` は呼び出し元 (`main.rs`) が `tracing` ログに出す。
    pub fn discover(explicit: Option<&Path>) -> Result<(Self, ConfigSource)> {
        let cwd = std::env::current_dir().context("failed to read current_dir")?;
        Self::discover_with_alongside(explicit, &cwd, alongside_binary_path().as_deref())
    }

    /// `discover` を CWD 注入可能にしたバージョン。test-only。
    /// `alongside_binary_path()` は `current_exe()` 経由のため、production は
    /// `discover` を呼び、テストは CWD を制御するためにこちらを呼ぶ。
    #[cfg(test)]
    fn discover_at(explicit: Option<&Path>, cwd: &Path) -> Result<(Self, ConfigSource)> {
        Self::discover_with_alongside(explicit, cwd, alongside_binary_path().as_deref())
    }

    /// `discover` のフル注入版 (テスト専用)。バイナリ隣のパスも override する。
    pub(crate) fn discover_with_alongside(
        explicit: Option<&Path>,
        cwd: &Path,
        alongside: Option<&Path>,
    ) -> Result<(Self, ConfigSource)> {
        // 1. 明示 (--config)
        if let Some(p) = explicit {
            // `~` 展開を噛ませる。OsStr → String の変換は lossy で十分 (パスが
            // 非 UTF-8 の Windows 環境は実用上稀、shellexpand も String 入力)。
            let s = p.to_string_lossy();
            let expanded = PathBuf::from(expand_tilde(&s));
            // 相対パスは CWD 起点で resolve (canonicalize は不要 = symlink 維持)。
            let resolved = if expanded.is_absolute() {
                expanded
            } else {
                cwd.join(expanded)
            };
            if !resolved.exists() {
                return Err(anyhow::anyhow!(
                    "--config path not found: {}",
                    resolved.display()
                ));
            }
            let cfg = Self::load_from(&resolved).with_context(|| {
                format!("failed to load config from --config {}", resolved.display())
            })?;
            return Ok((cfg, ConfigSource::Explicit));
        }

        // 2. CWD 直下
        let cwd_toml = cwd.join("kb-mcp.toml");
        if cwd_toml.exists() {
            let cfg = Self::load_from(&cwd_toml).with_context(|| {
                format!("failed to load config from cwd {}", cwd_toml.display())
            })?;
            return Ok((cfg, ConfigSource::Cwd));
        }

        // 3. .git 祖先
        if let Some(root) = find_git_root(cwd) {
            let git_toml = root.join("kb-mcp.toml");
            if git_toml.exists() {
                let cfg = Self::load_from(&git_toml).with_context(|| {
                    format!("failed to load config from git root {}", git_toml.display())
                })?;
                return Ok((cfg, ConfigSource::GitRoot));
            }
        }

        // 4. バイナリ隣 (legacy)
        if let Some(side) = alongside
            && side.exists()
        {
            let cfg = Self::load_from(side).with_context(|| {
                format!("failed to load config alongside binary {}", side.display())
            })?;
            return Ok((cfg, ConfigSource::AlongsideBinary));
        }

        // 5. 未発見 → Default
        Ok((Self::default(), ConfigSource::NotFound))
    }

    /// 指定パスから読み込む。ファイルが存在しない場合は空の `Config`。
    /// 相対パスで書かれたフィールドは**設定ファイルのあるディレクトリ**を
    /// 基点に解決する (cwd ではない)。
    pub fn load_from(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read config: {}", path.display()))?;
        let mut cfg: Self = toml::from_str(&text)
            .with_context(|| format!("failed to parse config: {}", path.display()))?;

        // 相対パスを設定ファイルのディレクトリ基準に resolve する。
        // cwd は MCP 起動時に呼び出し側プロジェクトに依存するため当てにならない。
        if let Some(base) = path.parent() {
            cfg.kb_path = cfg.kb_path.map(|p| resolve_relative(base, p));
            cfg.fastembed_cache_dir = cfg.fastembed_cache_dir.map(|p| resolve_relative(base, p));
            if let Some(e) = cfg.eval.as_mut() {
                e.golden = e.golden.take().map(|p| resolve_relative(base, p));
            }
        }

        // Phase 2.3 で opt-in 化: `[best_practice]` セクション省略、
        // `[best_practice]` のみ (path_templates 省略)、`path_templates = []`
        // のいずれも "not configured" を意味する。ランタイムでツールが
        // "not configured" エラーを返すため、ここでは reject しない。

        // Parser registry: [parsers].enabled = [] は誤設定として reject。キー省略
        // (parsers: None) の場合は Registry::defaults() = ["md"] が適用される
        // ため silent failure の心配は無い。
        if let Some(p) = &cfg.parsers {
            p.validate()
                .with_context(|| format!("{}: invalid [parsers] config", path.display()))?;
        }

        // Cross-section semantic validation (range checks for [search.mmr] etc.).
        // 構文 (deny_unknown_fields / 型) は serde 段で済んでおり、ここでは値域を見る。
        cfg.validate()
            .with_context(|| format!("{}: invalid config", path.display()))?;

        Ok(cfg)
    }

    /// 設定が空かどうか (全フィールドが `None`)。手動テスト用。
    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.kb_path.is_none()
            && self.model.is_none()
            && self.reranker.is_none()
            && self.rerank_by_default.is_none()
            && self.fastembed_cache_dir.is_none()
            && self.exclude_headings.is_none()
            && self.exclude_dirs.is_none()
            && self.quality_filter.is_none()
            && self.best_practice.is_none()
            && self.parsers.is_none()
            && self.watch.is_none()
            && self.transport.is_none()
            && self.eval.is_none()
            && self.search.is_none()
    }

    /// `exclude_dirs` の実効値を返す。設定省略時は [`DEFAULT_EXCLUDE_DIRS`]
    /// を `Vec<String>` 化して返す。明示的な `[]` はそのまま空 Vec。
    pub fn resolve_exclude_dirs(&self) -> Vec<String> {
        match &self.exclude_dirs {
            Some(list) => list.clone(),
            None => DEFAULT_EXCLUDE_DIRS.iter().map(|s| s.to_string()).collect(),
        }
    }

    /// 設定から `parser::Registry` を構築する。キー省略時は
    /// `Registry::defaults()` = `["md"]` のみ (legacy 後方互換)。
    pub fn build_parser_registry(&self) -> Result<crate::parser::Registry> {
        match &self.parsers {
            Some(p) => crate::parser::Registry::from_enabled(&p.enabled),
            None => Ok(crate::parser::Registry::defaults()),
        }
    }

    /// パース後の値域 / 整合性チェック。`load_from` のような構文レベルの
    /// validation (`deny_unknown_fields` / 型不一致) では弾けない、数値の
    /// レンジ違反などを検出する。
    ///
    /// 現状チェック対象:
    /// - `[search.mmr].lambda` が `0.0..=1.0`
    /// - `[search.mmr].same_doc_penalty` が `0.0..=1.0`
    /// - `[search.parent_retriever].max_expanded_tokens > whole_doc_threshold_tokens`
    /// - `[search.parent_retriever].max_expanded_tokens <= 8192` (BGE-M3 ctx)
    pub fn validate(&self) -> Result<()> {
        if let Some(s) = &self.search {
            // MMR レンジチェック
            if !(0.0..=1.0).contains(&s.mmr.lambda) {
                anyhow::bail!(
                    "[search.mmr].lambda must be in 0.0..=1.0, got {}",
                    s.mmr.lambda
                );
            }
            if !(0.0..=1.0).contains(&s.mmr.same_doc_penalty) {
                anyhow::bail!(
                    "[search.mmr].same_doc_penalty must be in 0.0..=1.0, got {}",
                    s.mmr.same_doc_penalty
                );
            }

            // Parent retriever レンジチェック
            if s.parent_retriever.max_expanded_tokens
                <= s.parent_retriever.whole_doc_threshold_tokens
            {
                anyhow::bail!(
                    "[search.parent_retriever].max_expanded_tokens ({}) must be > whole_doc_threshold_tokens ({})",
                    s.parent_retriever.max_expanded_tokens,
                    s.parent_retriever.whole_doc_threshold_tokens
                );
            }
            if s.parent_retriever.max_expanded_tokens > 8192 {
                anyhow::bail!(
                    "[search.parent_retriever].max_expanded_tokens must be <= 8192 (BGE-M3 context window), got {}",
                    s.parent_retriever.max_expanded_tokens
                );
            }
        }
        Ok(())
    }

    /// `fastembed_cache_dir` が設定されていて、かつ環境変数
    /// `FASTEMBED_CACHE_DIR` が未設定なら、プロセス環境に適用する。
    /// `Embedder::with_model` が `resolve_cache_dir()` で拾う前に呼ぶこと。
    pub fn apply_cache_dir_env(&self) {
        if std::env::var_os("FASTEMBED_CACHE_DIR").is_some() {
            return; // env を優先
        }
        if let Some(dir) = &self.fastembed_cache_dir {
            // SAFETY: プロセス単一スレッド (main 起動直後) でのみ呼ぶ想定。
            unsafe {
                std::env::set_var("FASTEMBED_CACHE_DIR", dir);
            }
        }
    }
}

/// `kb-mcp.toml` がどのソースから読まれたかを表す。`tracing` ログと
/// テストの assert から参照される。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigSource {
    /// CLI `--config <PATH>` で明示指定。
    Explicit,
    /// CWD 直下 `./kb-mcp.toml`。
    Cwd,
    /// `.git` 祖先ディレクトリ直下の `kb-mcp.toml`。
    GitRoot,
    /// `current_exe()` の隣 (legacy 探索)。
    AlongsideBinary,
    /// 全探索が失敗し `Config::default()` を返した。
    NotFound,
}

/// `~` を home に展開する。home が取れない (CI 等) 場合は入力をそのまま返す。
/// 内部的には `shellexpand::tilde` のラッパで、Windows でも `~` を解決する。
pub fn expand_tilde(s: &str) -> String {
    shellexpand::tilde(s).into_owned()
}

/// 実行中のバイナリと同じディレクトリにある `kb-mcp.toml` の絶対パス。
/// `current_exe()` が取得できない環境では `None`。
fn alongside_binary_path() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    Some(exe.parent()?.join("kb-mcp.toml"))
}

/// `start` から親方向に `.git` (ディレクトリまたは worktree 用ファイル) を
/// 探す。`start` 自身を含めて最大 20 ディレクトリ (= start + 19 祖先) を
/// チェックし、見つからなければ `None`。
///
/// `.git` がディレクトリかファイルかは判定しない (`exists()` で拾う) ので、
/// regular repo / worktree / submodule すべてで動く。NAS 暴走防止のため
/// 階層数上限を設けている。
fn find_git_root(start: &Path) -> Option<PathBuf> {
    let mut cur: &Path = start;
    for _ in 0..20 {
        if cur.join(".git").exists() {
            return Some(cur.to_path_buf());
        }
        cur = cur.parent()?;
    }
    None
}

/// `path` が絶対なら何もしない、相対なら `base.join(path)` を返す。
fn resolve_relative(base: &Path, path: PathBuf) -> PathBuf {
    if path.is_absolute() {
        path
    } else {
        base.join(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn test_load_missing_file_returns_empty() {
        let tmp = std::env::temp_dir().join("kb-mcp-nonexistent-config.toml");
        // 念のため存在しないことを確認
        let _ = std::fs::remove_file(&tmp);
        let cfg = Config::load_from(&tmp).unwrap();
        assert!(cfg.is_empty());
    }

    #[test]
    fn test_parse_full_config() {
        // 絶対パスは resolve_relative で rebase されないことも確認するため、
        // プラットフォーム別の真の絶対パスを使う。
        #[cfg(windows)]
        let (kb, cache) = ("C:/tmp/kb", "C:/tmp/cache");
        #[cfg(not(windows))]
        let (kb, cache) = ("/tmp/kb", "/tmp/cache");

        let mut file = tempfile("kb-mcp-config-full");
        writeln!(
            file,
            "kb_path = \"{kb}\"\n\
             model = \"bge-m3\"\n\
             reranker = \"bge-v2-m3\"\n\
             rerank_by_default = true\n\
             fastembed_cache_dir = \"{cache}\"\n\
             exclude_headings = [\"次の深堀り候補\", \"参考リンク\"]\n"
        )
        .unwrap();

        let cfg = Config::load_from(file.path()).unwrap();
        assert_eq!(cfg.kb_path.as_deref(), Some(Path::new(kb)));
        assert_eq!(cfg.model, Some(ModelChoice::BgeM3));
        assert_eq!(cfg.reranker, Some(RerankerChoice::BgeV2M3));
        assert_eq!(cfg.rerank_by_default, Some(true));
        assert_eq!(cfg.fastembed_cache_dir.as_deref(), Some(Path::new(cache)));
        assert_eq!(
            cfg.exclude_headings.as_deref(),
            Some(&["次の深堀り候補".to_string(), "参考リンク".to_string()][..])
        );
    }

    #[test]
    fn test_best_practice_default_is_empty() {
        // Phase 2.3 で opt-in 化: 既定は空リスト。ランタイムで
        // "not configured" エラーを返す扱いになる。
        let cfg = BestPracticeConfig::default();
        assert!(cfg.path_templates.is_empty());
    }

    #[test]
    fn test_best_practice_config_parses_from_toml() {
        let mut file = tempfile("kb-mcp-config-bp");
        writeln!(
            file,
            "[best_practice]\n\
             path_templates = [\"docs/{{target}}.md\", \"guides/{{target}}/README.md\"]\n"
        )
        .unwrap();
        let cfg = Config::load_from(file.path()).unwrap();
        let bp = cfg.best_practice.expect("best_practice must be Some");
        assert_eq!(bp.path_templates.len(), 2);
        assert_eq!(bp.path_templates[0], "docs/{target}.md");
        assert_eq!(bp.path_templates[1], "guides/{target}/README.md");
    }

    #[test]
    fn test_best_practice_section_only_yields_empty() {
        // `[best_practice]` セクションを書くが path_templates を省略しても、
        // opt-in 化後は空リスト = not configured となる。
        let mut file = tempfile("kb-mcp-config-bp2");
        writeln!(file, "[best_practice]").unwrap();
        let cfg = Config::load_from(file.path()).unwrap();
        let bp = cfg.best_practice.expect("best_practice must be Some");
        assert!(bp.path_templates.is_empty());
    }

    #[test]
    fn test_best_practice_explicit_empty_accepted() {
        // `path_templates = []` を明示的に書くケースも opt-in 未完了と同じ
        // 扱いで受理する。ランタイムで "not configured" を返す。
        let mut file = tempfile("kb-mcp-config-bp-empty");
        writeln!(
            file,
            "[best_practice]\n\
             path_templates = []\n"
        )
        .unwrap();
        let cfg = Config::load_from(file.path()).unwrap();
        let bp = cfg.best_practice.expect("best_practice must be Some");
        assert!(bp.path_templates.is_empty());
    }

    #[test]
    fn test_parsers_config_parses_from_toml() {
        let mut file = tempfile("kb-mcp-config-parsers");
        writeln!(
            file,
            "[parsers]\n\
             enabled = [\"md\", \"txt\"]\n"
        )
        .unwrap();
        let cfg = Config::load_from(file.path()).unwrap();
        let p = cfg.parsers.expect("parsers must be Some");
        assert_eq!(p.enabled, vec!["md".to_string(), "txt".to_string()]);
    }

    #[test]
    fn test_parsers_config_empty_array_is_rejected() {
        // [parsers].enabled = [] は誤設定として reject。キー省略なら defaults()
        // = ["md"] が適用されて問題ない、という規約。
        let mut file = tempfile("kb-mcp-config-parsers-empty");
        writeln!(
            file,
            "[parsers]\n\
             enabled = []\n"
        )
        .unwrap();
        let err = Config::load_from(file.path()).expect_err("must reject empty array");
        // anyhow::Context で包まれているので root cause まで辿る
        let full = format!("{err:?}");
        assert!(
            full.contains("empty array") || full.contains("at least one id"),
            "error should mention empty config, got: {full}"
        );
    }

    #[test]
    fn test_parsers_omitted_uses_md_default() {
        // [parsers] セクション自体が無い場合は cfg.parsers は None、
        // build_parser_registry() は Registry::defaults() = ["md"] を返す。
        let mut file = tempfile("kb-mcp-config-parsers-omitted");
        writeln!(file, r#"model = "bge-small-en-v1.5""#).unwrap();
        let cfg = Config::load_from(file.path()).unwrap();
        assert!(cfg.parsers.is_none());
        let reg = cfg.build_parser_registry().unwrap();
        assert_eq!(reg.extensions(), vec!["md"]);
    }

    #[test]
    fn test_transport_http_parses() {
        let mut file = tempfile("kb-mcp-config-transport-http");
        writeln!(
            file,
            "[transport]\n\
             kind = \"http\"\n\
             \n\
             [transport.http]\n\
             bind = \"0.0.0.0:4000\"\n"
        )
        .unwrap();
        let cfg = Config::load_from(file.path()).unwrap();
        let t = cfg.transport.expect("transport must be Some");
        assert_eq!(t.kind, Some(crate::transport::TransportKindConfig::Http));
        let http = t.http.expect("http section must be Some");
        assert_eq!(http.bind.as_deref(), Some("0.0.0.0:4000"));
    }

    #[test]
    fn test_transport_section_only_http_implies_http_kind() {
        // [transport.http] だけ書けば kind 省略でも HTTP として解釈される (糖衣)。
        let mut file = tempfile("kb-mcp-config-transport-http-only");
        writeln!(
            file,
            "[transport.http]\n\
             bind = \"127.0.0.1:4567\"\n"
        )
        .unwrap();
        let cfg = Config::load_from(file.path()).unwrap();
        let t = cfg.transport.expect("transport must be Some");
        assert!(t.kind.is_none(), "kind is omitted");
        let http = t.http.expect("http section must be Some");
        assert_eq!(http.bind.as_deref(), Some("127.0.0.1:4567"));
    }

    #[test]
    fn test_eval_config_parses_from_toml() {
        let mut file = tempfile("kb-mcp-config-eval");
        writeln!(
            file,
            "[eval]\n\
             golden = \".kb-mcp-eval.yml\"\n\
             history_size = 5\n\
             k_values = [1, 3, 10]\n\
             regression_threshold = 0.1\n"
        )
        .unwrap();
        let cfg = Config::load_from(file.path()).unwrap();
        let e = cfg.eval.expect("eval must be Some");
        // `golden` is a relative path so it gets rebased against the config dir.
        let golden = e.golden.as_deref().expect("golden must be Some");
        assert!(
            golden.ends_with(".kb-mcp-eval.yml"),
            "golden should end with .kb-mcp-eval.yml, got {golden:?}"
        );
        assert_eq!(e.history_size, Some(5));
        assert_eq!(e.k_values.as_deref(), Some(&[1, 3, 10][..]));
        assert_eq!(e.regression_threshold, Some(0.1));
    }

    #[test]
    fn test_eval_config_omitted_is_none() {
        let mut file = tempfile("kb-mcp-config-eval-omit");
        writeln!(file, r#"model = "bge-small-en-v1.5""#).unwrap();
        let cfg = Config::load_from(file.path()).unwrap();
        assert!(cfg.eval.is_none());
    }

    #[test]
    fn test_eval_config_rejects_unknown_field() {
        let mut file = tempfile("kb-mcp-config-eval-bad");
        writeln!(
            file,
            "[eval]\n\
             bogus = 1\n"
        )
        .unwrap();
        let err = Config::load_from(file.path()).expect_err("unknown [eval] field must reject");
        assert!(err.to_string().contains("failed to parse config"));
    }

    #[test]
    fn test_search_config_parses_from_toml() {
        let mut file = tempfile("kb-mcp-config-search");
        writeln!(
            file,
            "[search]\n\
             min_confidence_ratio = 2.0\n"
        )
        .unwrap();
        let cfg = Config::load_from(file.path()).unwrap();
        let s = cfg.search.expect("search must be Some");
        assert_eq!(s.min_confidence_ratio, Some(2.0));
    }

    #[test]
    fn test_search_config_omitted_is_none() {
        let mut file = tempfile("kb-mcp-config-search-omit");
        writeln!(file, r#"model = "bge-small-en-v1.5""#).unwrap();
        let cfg = Config::load_from(file.path()).unwrap();
        assert!(cfg.search.is_none());
    }

    #[test]
    fn test_search_config_rejects_unknown_field() {
        let mut file = tempfile("kb-mcp-config-search-bad");
        writeln!(
            file,
            "[search]\n\
             bogus = 1\n"
        )
        .unwrap();
        let err = Config::load_from(file.path()).expect_err("unknown [search] field must reject");
        assert!(err.to_string().contains("failed to parse config"));
    }

    #[test]
    fn test_mmr_config_defaults() {
        let cfg = MmrConfig::default();
        assert!(!cfg.enabled);
        assert!((cfg.lambda - 0.7).abs() < 1e-6);
        assert_eq!(cfg.same_doc_penalty, 0.0);
    }

    #[test]
    fn test_mmr_config_rejects_unknown_field() {
        let toml = r#"
[search.mmr]
enabled = true
lambda = 0.5
unknown = "bad"
"#;
        let result: std::result::Result<crate::config::Config, _> = toml::from_str(toml);
        assert!(result.is_err(), "unknown field should be rejected");
    }

    #[test]
    fn test_mmr_lambda_out_of_range_rejected() {
        let toml = r#"
[search.mmr]
enabled = true
lambda = 1.5
"#;
        let cfg: crate::config::Config = toml::from_str(toml).unwrap();
        let result = cfg.validate();
        assert!(result.is_err(), "lambda > 1.0 should fail validate");
    }

    #[test]
    fn test_mmr_same_doc_penalty_out_of_range_rejected() {
        let toml = r#"
[search.mmr]
enabled = true
same_doc_penalty = -0.1
"#;
        let cfg: crate::config::Config = toml::from_str(toml).unwrap();
        let result = cfg.validate();
        assert!(
            result.is_err(),
            "negative same_doc_penalty should fail validate"
        );
    }

    #[test]
    fn test_parent_retriever_config_defaults() {
        let cfg = ParentRetrieverConfig::default();
        assert!(!cfg.enabled);
        assert_eq!(cfg.whole_doc_threshold_tokens, 100);
        assert_eq!(cfg.max_expanded_tokens, 2000);
    }

    #[test]
    fn test_parent_retriever_config_rejects_unknown_field() {
        let toml = r#"
[search.parent_retriever]
enabled = true
unknown = "bad"
"#;
        let result: std::result::Result<crate::config::Config, _> = toml::from_str(toml);
        assert!(result.is_err(), "unknown field should be rejected");
    }

    #[test]
    fn test_parent_retriever_validates_max_expanded_gt_threshold() {
        let toml = r#"
[search.parent_retriever]
enabled = true
whole_doc_threshold_tokens = 500
max_expanded_tokens = 500
"#;
        let cfg: crate::config::Config = toml::from_str(toml).unwrap();
        let result = cfg.validate();
        assert!(
            result.is_err(),
            "max_expanded_tokens <= threshold should fail"
        );
    }

    #[test]
    fn test_parent_retriever_max_expanded_capped_at_8192() {
        let toml = r#"
[search.parent_retriever]
enabled = true
max_expanded_tokens = 9000
"#;
        let cfg: crate::config::Config = toml::from_str(toml).unwrap();
        let result = cfg.validate();
        assert!(
            result.is_err(),
            "max_expanded_tokens > 8192 (BGE-M3 ctx) should fail"
        );
    }

    #[test]
    fn test_search_overrides_resolve_parent_retriever_thresholds() {
        let toml = r#"
[search.parent_retriever]
enabled = true
whole_doc_threshold_tokens = 50
max_expanded_tokens = 1500
"#;
        let cfg: crate::config::Config = toml::from_str(toml).unwrap();
        let search_cfg = cfg.search.unwrap_or_default();
        let overrides = SearchOverrides::default();
        let resolved = overrides.resolve(&search_cfg);
        assert!(resolved.parent_retriever_enabled);
        assert_eq!(resolved.parent_whole_doc_threshold_tokens, 50);
        assert_eq!(resolved.parent_max_expanded_tokens, 1500);
    }

    #[test]
    fn test_search_overrides_resolve_per_call_beats_toml() {
        let toml = r#"
[search.mmr]
enabled = false
lambda = 0.7
"#;
        let cfg: crate::config::Config = toml::from_str(toml).unwrap();
        let search_cfg = cfg.search.unwrap_or_default();
        let overrides = SearchOverrides {
            mmr: Some(true),
            mmr_lambda: Some(0.3),
            mmr_same_doc_penalty: None,
            parent_retriever: None,
        };
        let resolved = overrides.resolve(&search_cfg);
        assert!(resolved.mmr_enabled);
        assert!((resolved.mmr_lambda - 0.3).abs() < 1e-6);
    }

    #[test]
    fn test_search_overrides_resolve_toml_default_when_none() {
        let toml = r#"
[search.mmr]
enabled = true
lambda = 0.5
"#;
        let cfg: crate::config::Config = toml::from_str(toml).unwrap();
        let search_cfg = cfg.search.unwrap_or_default();
        let overrides = SearchOverrides::default();
        let resolved = overrides.resolve(&search_cfg);
        assert!(resolved.mmr_enabled);
        assert!((resolved.mmr_lambda - 0.5).abs() < 1e-6);
    }

    #[test]
    fn test_search_overrides_resolve_uses_default_when_toml_missing() {
        let search_cfg = SearchConfig::default();
        let overrides = SearchOverrides::default();
        let resolved = overrides.resolve(&search_cfg);
        // toml なし、override なし → MmrConfig::default を反映
        assert!(!resolved.mmr_enabled);
        assert!((resolved.mmr_lambda - 0.7).abs() < 1e-6);
        assert_eq!(resolved.mmr_same_doc_penalty, 0.0);
    }

    #[test]
    fn test_search_overrides_resolve_warn_emitted_when_mmr_off_with_lambda() {
        // tracing::warn は 「effective MMR off + lambda Some(_)」で発火する。
        // tracing-test crate は使わないので、ここでは挙動を smoke として呼び出すのみ。
        // (warn が発火することはコードレビューで verify する)
        let search_cfg = SearchConfig::default(); // MMR off
        let overrides = SearchOverrides {
            mmr: None,             // toml off に従う
            mmr_lambda: Some(0.3), // ghost lambda
            mmr_same_doc_penalty: None,
            parent_retriever: None,
        };
        let resolved = overrides.resolve(&search_cfg);
        // panic しない、resolved は MMR off のまま
        assert!(!resolved.mmr_enabled);
    }

    #[test]
    fn test_transport_unknown_field_is_rejected() {
        let mut file = tempfile("kb-mcp-config-transport-bad");
        writeln!(
            file,
            "[transport]\n\
             bogus = 1\n"
        )
        .unwrap();
        let err =
            Config::load_from(file.path()).expect_err("unknown [transport] field must reject");
        assert!(err.to_string().contains("failed to parse config"));
    }

    #[test]
    fn test_watch_unknown_field_in_config_is_rejected() {
        // Config の [watch] でも deny_unknown_fields が効いて typo を reject する。
        let mut file = tempfile("kb-mcp-config-watch-bad");
        writeln!(
            file,
            "[watch]\n\
             enabled = true\n\
             bogus_field = 42\n"
        )
        .unwrap();
        let err = Config::load_from(file.path()).expect_err("unknown [watch] field must reject");
        assert!(err.to_string().contains("failed to parse config"));
    }

    #[test]
    fn test_watch_config_parses_from_toml() {
        let mut file = tempfile("kb-mcp-config-watch");
        writeln!(
            file,
            "[watch]\n\
             enabled = false\n\
             debounce_ms = 750\n"
        )
        .unwrap();
        let cfg = Config::load_from(file.path()).unwrap();
        let w = cfg.watch.expect("watch must be Some");
        assert!(!w.enabled);
        assert_eq!(w.debounce_ms, 750);
    }

    #[test]
    fn test_watch_config_omitted_uses_defaults_via_unwrap_or_default() {
        // セクション自体が無ければ cfg.watch == None。呼び出し側で
        // `cfg.watch.unwrap_or_default()` すると enabled=true / 500ms が入る。
        let mut file = tempfile("kb-mcp-config-watch-omit");
        writeln!(file, r#"model = "bge-small-en-v1.5""#).unwrap();
        let cfg = Config::load_from(file.path()).unwrap();
        assert!(cfg.watch.is_none());
        let w = cfg.watch.unwrap_or_default();
        assert!(w.enabled);
        assert_eq!(w.debounce_ms, 500);
    }

    #[test]
    fn test_parsers_unknown_id_is_rejected() {
        let mut file = tempfile("kb-mcp-config-parsers-unknown");
        writeln!(
            file,
            "[parsers]\n\
             enabled = [\"md\", \"pdf\"]\n"
        )
        .unwrap();
        let cfg = Config::load_from(file.path()).unwrap();
        // validate is passed, but build_parser_registry should fail on "pdf"
        let err = cfg
            .build_parser_registry()
            .expect_err("pdf must be rejected");
        assert!(err.to_string().contains("pdf"));
    }

    #[test]
    fn test_parse_empty_exclude_headings_overrides_default() {
        // `exclude_headings = []` を明示すると「除外しない」という意図になるため、
        // Option::None と区別して保持されていることを確認する。
        let mut file = tempfile("kb-mcp-config-empty-excludes");
        writeln!(file, "exclude_headings = []").unwrap();
        let cfg = Config::load_from(file.path()).unwrap();
        let list = cfg
            .exclude_headings
            .expect("Some(vec![]) must be preserved");
        assert!(list.is_empty());
    }

    #[test]
    fn test_parse_partial_config() {
        let mut file = tempfile("kb-mcp-config-partial");
        writeln!(file, r#"model = "bge-small-en-v1.5""#).unwrap();

        let cfg = Config::load_from(file.path()).unwrap();
        assert_eq!(cfg.model, Some(ModelChoice::BgeSmallEnV15));
        assert!(cfg.kb_path.is_none());
        assert!(cfg.reranker.is_none());
    }

    #[test]
    fn test_unknown_fields_are_rejected() {
        let mut file = tempfile("kb-mcp-config-unknown");
        writeln!(file, r#"bogus_field = "oops""#).unwrap();
        let err = Config::load_from(file.path()).expect_err("should reject unknown field");
        assert!(err.to_string().contains("failed to parse config"));
    }

    #[test]
    fn test_relative_paths_resolve_against_config_dir() {
        // load_from 内部の「parent → resolve_relative」経路を実際に通す e2e。
        // tempfile helper は Drop でファイルを消してしまうので、ここではテスト
        // 終了時に削除する `DirGuard` でファイル書込から load_from まで 1 本化する。
        let pid = std::process::id();
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!("kb-mcp-test-relpath-{pid}-{nonce}"));
        std::fs::create_dir_all(&dir).unwrap();
        let cfg_path = dir.join("kb-mcp.toml");
        std::fs::write(
            &cfg_path,
            "kb_path = \"./knowledge-base\"\n\
             fastembed_cache_dir = \"cache/hf\"\n",
        )
        .unwrap();

        struct DirGuard(PathBuf);
        impl Drop for DirGuard {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let _guard = DirGuard(dir.clone());

        let cfg = Config::load_from(&cfg_path).unwrap();

        let kb = cfg.kb_path.expect("kb_path must be Some");
        let cache = cfg
            .fastembed_cache_dir
            .expect("fastembed_cache_dir must be Some");
        assert!(
            kb.is_absolute() || kb.starts_with(&dir),
            "kb_path not rebased: {kb:?}"
        );
        assert!(kb.ends_with("knowledge-base"));
        assert!(cache.starts_with(&dir));
        assert!(cache.ends_with(Path::new("cache/hf")) || cache.ends_with(Path::new("cache\\hf")));
    }

    #[test]
    fn test_absolute_paths_are_not_rebased() {
        // Windows / Unix 両対応
        #[cfg(windows)]
        let abs = PathBuf::from("C:/absolute/foo");
        #[cfg(not(windows))]
        let abs = PathBuf::from("/absolute/foo");

        let base = Path::new("/some/base");
        let out = resolve_relative(base, abs.clone());
        assert_eq!(out, abs);
    }

    #[cfg(windows)]
    #[test]
    fn test_windows_unc_and_verbatim_paths_not_rebased() {
        // UNC パスと \\?\ verbatim プレフィックスは std::path::Path::is_absolute
        // で true を返すので、resolve_relative は touch しない。
        let base = Path::new("C:/some/base");

        let unc = PathBuf::from(r"\\server\share\foo");
        assert!(unc.is_absolute(), "UNC should be absolute");
        assert_eq!(resolve_relative(base, unc.clone()), unc);

        let verbatim = PathBuf::from(r"\\?\C:\verbatim\bar");
        assert!(verbatim.is_absolute(), "verbatim prefix should be absolute");
        assert_eq!(resolve_relative(base, verbatim.clone()), verbatim);
    }

    #[test]
    fn test_toml_example_parses_with_all_keys_uncommented() {
        // kb-mcp.toml.example のすべてのキーが Config で受け入れられるかを検証。
        // Config にフィールドが追加されたのに example を更新し忘れたり、逆に
        // example に古いキーが残って deny_unknown_fields に引っかかるのを
        // 回帰テストで検知する。
        //
        // example はコメント (`#`) で各フィールド例を書いているので、
        // 行頭 `#` を剥がして「全行有効」な設定としてパースする。
        let example_path = Path::new(env!("CARGO_MANIFEST_DIR")).join("kb-mcp.toml.example");
        let raw = std::fs::read_to_string(&example_path)
            .expect("kb-mcp.toml.example must exist at repository root");

        // 同じキーを 2 回以上コメント化して例示することがある
        // (例: exclude_headings の `[...]` と `[]` の両方を示す)。
        // uncomment 後に重複キーになると toml::from_str がエラーになるので、
        // 「同じキーは最初の 1 行だけ uncomment、以降はコメントのまま残す」
        // 方針で剥がす。
        let mut seen_keys: std::collections::HashSet<String> = std::collections::HashSet::new();
        let uncommented: String = raw
            .lines()
            .map(|line| {
                let trimmed = line.trim_start();
                // 見出しコメントや空行はそのまま (除外しても同じ挙動)
                if trimmed.is_empty() {
                    return String::new();
                }
                // `# key = value` 行を剥がす。ただし純粋な説明コメント
                // (例: `# Copy this file...`) はそのまま残す (toml には
                // 影響しないので除外しても同じ)。判定は `# <ident> =` の形。
                if let Some(rest) = trimmed.strip_prefix('#') {
                    let rest = rest.trim_start();
                    if let Some(eq_idx) = rest.find('=')
                        && rest
                            .chars()
                            .next()
                            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
                    {
                        let key = rest[..eq_idx].trim().to_string();
                        if seen_keys.insert(key) {
                            return rest.to_string();
                        }
                        // 2 回目以降はコメントのまま残して重複を避ける
                    }
                }
                line.to_string()
            })
            .collect::<Vec<_>>()
            .join("\n");

        let cfg: Config = toml::from_str(&uncommented).unwrap_or_else(|e| {
            panic!(
                "kb-mcp.toml.example failed to parse with all keys enabled: {e}\n\
                 --- generated TOML ---\n{uncommented}\n---"
            )
        });

        // 全フィールドが埋まっていれば is_empty は false。example に少なくとも
        // 1 つのキーが書かれていることの最低限チェック。
        assert!(
            !cfg.is_empty(),
            "kb-mcp.toml.example contains no parseable keys"
        );
    }

    #[test]
    fn test_apply_cache_dir_env_respects_existing_env() {
        // 既に env が設定されていれば config 値は適用しない。
        let key = "FASTEMBED_CACHE_DIR";
        // SAFETY: single-threaded test process.
        unsafe {
            std::env::set_var(key, "/pre-existing");
        }
        let cfg = Config {
            fastembed_cache_dir: Some(PathBuf::from("/from-config")),
            ..Default::default()
        };
        cfg.apply_cache_dir_env();
        assert_eq!(std::env::var(key).unwrap(), "/pre-existing");
        unsafe {
            std::env::remove_var(key);
        }
    }

    /// Helper: 一意名の一時ファイルを作って `File` を返す。tempfile crate に
    /// 依存しないように素朴に作る。
    fn tempfile(prefix: &str) -> NamedTempFile {
        let mut path = std::env::temp_dir();
        let pid = std::process::id();
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        path.push(format!("{prefix}-{pid}-{nonce}.toml"));
        NamedTempFile {
            file: std::fs::File::create(&path).unwrap(),
            path,
        }
    }

    struct NamedTempFile {
        file: std::fs::File,
        path: PathBuf,
    }

    impl NamedTempFile {
        fn path(&self) -> &Path {
            &self.path
        }
    }

    impl Write for NamedTempFile {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.file.write(buf)
        }
        fn flush(&mut self) -> std::io::Result<()> {
            self.file.flush()
        }
    }

    impl Drop for NamedTempFile {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.path);
        }
    }

    #[test]
    fn test_config_source_variants_are_distinguishable() {
        // 4 ソース + NotFound が all distinct であること。Debug 表示を使う。
        let variants = [
            ConfigSource::Explicit,
            ConfigSource::Cwd,
            ConfigSource::GitRoot,
            ConfigSource::AlongsideBinary,
            ConfigSource::NotFound,
        ];
        let labels: Vec<String> = variants.iter().map(|v| format!("{v:?}")).collect();
        let unique: std::collections::HashSet<_> = labels.iter().collect();
        assert_eq!(
            unique.len(),
            variants.len(),
            "all variants must be distinct"
        );
    }

    #[test]
    fn test_expand_tilde_with_home() {
        // `~/foo` は home_dir 起点に展開される。home が取れない CI 環境でも
        // shellexpand は元文字列を返すので、最低限「panic しない」を保証。
        let expanded = expand_tilde("~/.kb-mcp.toml");
        // Windows でも Unix でも入力に `~/` が残らないか、home が解決されているかのどちらか。
        // shellexpand 3 は home 取れない場合 input をそのまま返す挙動なので分岐 assert。
        if let Some(home) = dirs_next_fallback() {
            assert!(
                expanded.starts_with(&home) || expanded == "~/.kb-mcp.toml",
                "expanded={expanded:?}, home={home:?}"
            );
        }
    }

    #[test]
    fn test_find_git_root_returns_dir_when_git_present() {
        let dir = TempDir::new("kb-mcp-find-git-root-yes");
        let git = dir.path().join(".git");
        std::fs::create_dir_all(&git).unwrap();
        let nested = dir.path().join("a/b/c");
        std::fs::create_dir_all(&nested).unwrap();
        let found = find_git_root(&nested);
        assert_eq!(found.as_deref(), Some(dir.path()));
    }

    #[test]
    fn test_find_git_root_handles_git_file_for_worktree() {
        // worktree の場合は `.git` が file のことがある。`exists()` で拾えるか。
        let dir = TempDir::new("kb-mcp-find-git-root-worktree");
        let git_file = dir.path().join(".git");
        std::fs::write(&git_file, "gitdir: /elsewhere\n").unwrap();
        let found = find_git_root(dir.path());
        assert_eq!(found.as_deref(), Some(dir.path()));
    }

    #[test]
    fn test_find_git_root_returns_none_when_not_in_repo() {
        let dir = TempDir::new("kb-mcp-find-git-root-no");
        // `.git` は作らない。
        let found = find_git_root(dir.path());
        assert!(found.is_none());
    }

    #[test]
    fn test_find_git_root_caps_at_20_levels() {
        // 21 階層深くまで掘っても 20 階層上限で諦める。
        // 実ファイル作成はしない (filesystem 上限を避ける)、PathBuf 上の操作だけで
        // 確認できるよう、find_git_root の上限ロジックを切り出した内部関数を
        // テスト可能にしておく。ここでは「上限超え深さでも panic / loop 暴走しない」
        // ことだけを保証する smoke test。
        let mut p = std::env::temp_dir().join("kb-mcp-cap-test");
        for i in 0..30 {
            p = p.join(format!("d{i}"));
        }
        // ディレクトリは存在しないので exists() は毎回 false を返し、
        // 20 イテレーションの上限に到達して None で終わる (panic / 無限ループ
        // しないことだけを保証する smoke test)。
        let _ = find_git_root(&p);
    }

    #[test]
    fn test_discover_explicit_takes_priority_over_cwd() {
        // 明示と CWD 両方に toml があっても明示が勝つ。
        #[cfg(windows)]
        let (cwd_kb, explicit_kb) = ("C:/cwd-kb", "C:/explicit-kb");
        #[cfg(not(windows))]
        let (cwd_kb, explicit_kb) = ("/cwd-kb", "/explicit-kb");
        let dir = TempDir::new("kb-mcp-discover-explicit");
        let cwd_toml = dir.path().join("kb-mcp.toml");
        std::fs::write(&cwd_toml, format!("kb_path = \"{cwd_kb}\"\n")).unwrap();
        let explicit_toml = dir.path().join("explicit.toml");
        std::fs::write(&explicit_toml, format!("kb_path = \"{explicit_kb}\"\n")).unwrap();
        let (cfg, src) =
            Config::discover_at(Some(&explicit_toml), dir.path()).expect("discover ok");
        assert_eq!(src, ConfigSource::Explicit);
        assert_eq!(cfg.kb_path.as_deref(), Some(Path::new(explicit_kb)));
    }

    #[test]
    fn test_discover_explicit_missing_fails_fast() {
        // 明示で不存在 → エラー、CWD には toml があってもフォールバック禁止。
        #[cfg(windows)]
        let cwd_kb = "C:/cwd-kb";
        #[cfg(not(windows))]
        let cwd_kb = "/cwd-kb";
        let dir = TempDir::new("kb-mcp-discover-explicit-miss");
        let cwd_toml = dir.path().join("kb-mcp.toml");
        std::fs::write(&cwd_toml, format!("kb_path = \"{cwd_kb}\"\n")).unwrap();
        let explicit_toml = dir.path().join("does-not-exist.toml");
        let err = Config::discover_at(Some(&explicit_toml), dir.path())
            .expect_err("must error on missing explicit");
        let msg = format!("{err}");
        assert!(
            msg.contains("--config"),
            "error must mention --config: {msg}"
        );
        assert!(msg.contains("not found"), "error must say not found: {msg}");
    }

    #[test]
    fn test_discover_cwd_when_no_explicit() {
        #[cfg(windows)]
        let kb = "C:/cwd-kb";
        #[cfg(not(windows))]
        let kb = "/cwd-kb";
        let dir = TempDir::new("kb-mcp-discover-cwd");
        let toml = dir.path().join("kb-mcp.toml");
        std::fs::write(&toml, format!("kb_path = \"{kb}\"\n")).unwrap();
        let (cfg, src) = Config::discover_at(None, dir.path()).expect("discover ok");
        assert_eq!(src, ConfigSource::Cwd);
        assert_eq!(cfg.kb_path.as_deref(), Some(Path::new(kb)));
    }

    #[test]
    fn test_discover_walks_to_git_root() {
        #[cfg(windows)]
        let kb = "C:/git-kb";
        #[cfg(not(windows))]
        let kb = "/git-kb";
        let dir = TempDir::new("kb-mcp-discover-gitroot");
        std::fs::create_dir_all(dir.path().join(".git")).unwrap();
        let toml = dir.path().join("kb-mcp.toml");
        std::fs::write(&toml, format!("kb_path = \"{kb}\"\n")).unwrap();
        let nested = dir.path().join("a/b/c");
        std::fs::create_dir_all(&nested).unwrap();
        // CWD = nested (toml 無し)、祖先に .git + kb-mcp.toml。
        let (cfg, src) = Config::discover_at(None, &nested).expect("discover ok");
        assert_eq!(src, ConfigSource::GitRoot);
        assert_eq!(cfg.kb_path.as_deref(), Some(Path::new(kb)));
    }

    #[test]
    fn test_discover_cwd_wins_over_git_root() {
        // CWD に toml があり、かつ .git 祖先にも toml がある場合 → CWD が勝つ。
        #[cfg(windows)]
        let (cwd_kb, git_kb) = ("C:/cwd-wins", "C:/git-loses");
        #[cfg(not(windows))]
        let (cwd_kb, git_kb) = ("/cwd-wins", "/git-loses");
        let dir = TempDir::new("kb-mcp-discover-cwd-vs-git");
        std::fs::create_dir_all(dir.path().join(".git")).unwrap();
        // git root 直下の toml
        std::fs::write(
            dir.path().join("kb-mcp.toml"),
            format!("kb_path = \"{git_kb}\"\n"),
        )
        .unwrap();
        // ネストして CWD にも toml
        let cwd = dir.path().join("project/sub");
        std::fs::create_dir_all(&cwd).unwrap();
        std::fs::write(cwd.join("kb-mcp.toml"), format!("kb_path = \"{cwd_kb}\"\n")).unwrap();
        let (cfg, src) = Config::discover_at(None, &cwd).expect("discover ok");
        assert_eq!(src, ConfigSource::Cwd);
        assert_eq!(cfg.kb_path.as_deref(), Some(Path::new(cwd_kb)));
    }

    #[test]
    fn test_discover_returns_default_when_none() {
        let dir = TempDir::new("kb-mcp-discover-none");
        let absent = dir.path().join("there-is-no-toml-here.toml");
        let (cfg, src) =
            Config::discover_with_alongside(None, dir.path(), Some(&absent)).expect("discover ok");
        assert_eq!(src, ConfigSource::NotFound);
        assert!(cfg.is_empty());
    }

    #[test]
    fn test_discover_alongside_binary_when_no_higher_tier() {
        // CWD / .git どちらにも toml が無く、バイナリ隣にだけ存在 → AlongsideBinary。
        #[cfg(windows)]
        let kb = "C:/side-kb";
        #[cfg(not(windows))]
        let kb = "/side-kb";
        // 2 つの別ディレクトリを作る: CWD (toml なし) と alongside (toml あり)
        let cwd_dir = TempDir::new("kb-mcp-discover-side-cwd");
        let side_dir = TempDir::new("kb-mcp-discover-side-bin");
        let side_toml = side_dir.path().join("kb-mcp.toml");
        std::fs::write(&side_toml, format!("kb_path = \"{kb}\"\n")).unwrap();
        let (cfg, src) = Config::discover_with_alongside(None, cwd_dir.path(), Some(&side_toml))
            .expect("discover ok");
        assert_eq!(src, ConfigSource::AlongsideBinary);
        assert_eq!(cfg.kb_path.as_deref(), Some(Path::new(kb)));
    }

    /// テスト用 tempdir (Drop で自動削除)。`tests/validate_cli.rs::TempKb` の lib 版。
    struct TempDir {
        path: PathBuf,
    }
    impl TempDir {
        fn new(prefix: &str) -> Self {
            let pid = std::process::id();
            let nonce = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            let path = std::env::temp_dir().join(format!("{prefix}-{pid}-{nonce}"));
            std::fs::create_dir_all(&path).unwrap();
            Self { path }
        }
        fn path(&self) -> &Path {
            &self.path
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    fn dirs_next_fallback() -> Option<String> {
        std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .map(|s| s.to_string_lossy().into_owned())
    }
}
