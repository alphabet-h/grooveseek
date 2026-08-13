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
    /// 静的 Contextual Retrieval (feature-46) の設定。
    /// 省略時 (`None`) は `ContextualConfig::default()` (enabled=false) 相当。
    pub contextual: Option<ContextualConfig>,
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
    /// RRF / FTS5 bm25 の fusion パラメータ。セクション省略時は
    /// [`FusionConfig::default()`] (= feature-47 以前の定数と同値)。
    #[serde(default)]
    pub fusion: FusionConfig,
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

/// `[search.fusion]` セクション。feature-47 で追加。
///
/// RRF の定数項と FTS5 bm25 の列重みを設定可能にする。**既定値は feature-47
/// 以前のコンパイル時定数と同一**であり、セクション省略時・既定値明示時の
/// いずれでも検索結果は 1 bit も変わらない。
///
/// 重みを配列ではなく 3 つの named field にしてあるのは、`bm25()` へ渡す
/// 個数のミスを構造的に不可能にするため (FTS5 は個数不一致を silent に
/// 処理する: 不足は 1.0 補完 / 過剰は無視)。
///
/// 値域チェックは [`Config::validate`] に集約する。db 層の bind parameter
/// 経路は NaN / inf を **エラーにせず通す** ため、validate が唯一の防波堤
/// である (feature-47 D-2)。
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FusionConfig {
    /// RRF の定数項 k。`>= 1.0` (Elasticsearch `rank_constant` 準拠の下限)。
    /// 小さいほど「片方の検索器が 1 位に出した文書」を上位へ救う。
    #[serde(default = "default_rrf_k")]
    pub rrf_k: f32,
    /// FTS5 bm25 の heading 列重み。`>= 0.0`。
    #[serde(default = "default_bm25_heading_weight")]
    pub bm25_heading_weight: f32,
    /// 同 context 列重み。`>= 0.0`。
    #[serde(default = "default_bm25_context_weight")]
    pub bm25_context_weight: f32,
    /// 同 content 列重み。`>= 0.0`。
    #[serde(default = "default_bm25_content_weight")]
    pub bm25_content_weight: f32,
}

/// (AU-32) Upper bound on `[search.fusion].rrf_k`.
///
/// RRF scores each hit as `1.0 / (rrf_k + rank + 1.0)` in **f32**. Once
/// `rrf_k` is large enough that adding the rank no longer changes the float,
/// every hit scores identically and the fusion ranking collapses to whatever
/// the tie-break happens to be — silently, with no error and no warning.
///
/// Measured, on the exact expression the code evaluates:
///
/// | `rrf_k` | rank 0 vs 1 | rank 0 vs 9 |
/// |---|---|---|
/// | `60` (default) | distinct | distinct |
/// | `1e6` | distinct | distinct |
/// | `1.6e7` | **tied** | distinct |
/// | `1e9` | **tied** | **tied** |
///
/// The audit item that prompted this predicted `inf` / `NaN`; that is not what
/// happens. Nothing overflows — the ranking just stops ordering anything.
///
/// `1e6` sits an order of magnitude below the first observed tie and is already
/// four orders above any published rank_constant, so no real configuration
/// reaches it.
const MAX_RRF_K: f32 = 1e6;

/// (AU-32) Upper bound on each `[search.fusion].bm25_*_weight`.
///
/// These are passed to SQLite's `bm25()` as column weights and the result is
/// stored in an `f32` score. Unlike `rrf_k` there is no clean cliff — the point
/// at which a weight overflows depends on the corpus, since bm25's magnitude
/// grows with term rarity — so this bound is chosen for being far outside any
/// legitimate use rather than for marking a specific failure. Defaults are
/// 1.0-2.0.
const MAX_BM25_WEIGHT: f32 = 1e6;

fn default_rrf_k() -> f32 {
    60.0
}

fn default_bm25_heading_weight() -> f32 {
    2.0
}

fn default_bm25_context_weight() -> f32 {
    1.0
}

fn default_bm25_content_weight() -> f32 {
    1.0
}

impl Default for FusionConfig {
    fn default() -> Self {
        Self {
            rrf_k: default_rrf_k(),
            bm25_heading_weight: default_bm25_heading_weight(),
            bm25_context_weight: default_bm25_context_weight(),
            bm25_content_weight: default_bm25_content_weight(),
        }
    }
}

impl FusionConfig {
    /// ビルトイン既定値と完全一致するか。`true` のとき eval fingerprint に
    /// fusion を **載せない** ことで、feature-47 以前の history JSON / 凍結
    /// baseline との `PartialEq` 互換を保つ (feature-47 D-7 / E-10)。
    pub fn is_builtin_default(&self) -> bool {
        *self == Self::default()
    }
}

/// config → db の一方向変換。既定値の二重管理はこの `From` に固定し、
/// `FusionParams::from(&FusionConfig::default()) == FusionParams::default()`
/// を回帰テストで押さえる (feature-47 D-3)。
impl From<&FusionConfig> for crate::db::FusionParams {
    fn from(c: &FusionConfig) -> Self {
        Self {
            rrf_k: c.rrf_k,
            bm25_heading_weight: c.bm25_heading_weight,
            bm25_context_weight: c.bm25_context_weight,
            bm25_content_weight: c.bm25_content_weight,
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

/// `[contextual]` セクション (feature-46)。静的 context の embedding/FTS/reranker
/// 注入を有効化する。既定 off ―― kb-mcp の素の default 構成 (reranker なし) で
/// dogfood KB (574 docs) を A/B 計測したところ recall@5 -0.080 / MRR -0.041 と
/// 悪化したため opt-in とした (`.dev/knowledge/eval-baseline-2026-07-20-context.md`)。
/// reranker (bge-v2-m3 等) 併用時は全指標で最良 (recall@5 +0.047 / MRR +0.102) に
/// 転じるため、reranker 併用ユーザには docs で有効化を推奨する。将来のハイブリッド
/// 拡張 (source = "static" | "external") の置き場としても機能する。
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContextualConfig {
    #[serde(default = "default_contextual_enabled")]
    pub enabled: bool,
}

fn default_contextual_enabled() -> bool {
    false
}

impl Default for ContextualConfig {
    fn default() -> Self {
        Self {
            enabled: default_contextual_enabled(),
        }
    }
}

impl Config {
    /// 後方互換シム。`discover(None)` に委譲し `ConfigSource` を捨てる。
    /// 実際の探索順は CWD → `.git` 祖先 → バイナリ隣 (legacy) で、
    /// 関数名は historical naming のまま残している。新規コードは
    /// [`Config::discover`] を直接呼び ConfigSource を tracing に乗せること。
    pub fn load_alongside_binary() -> Result<Self> {
        Self::discover(None).map(|d| d.config)
    }

    /// CLI `--config` で渡されたパスがあればそれを (絶対 / 相対 + `~` 展開した上で) 採用、
    /// なければ CWD → `.git` 祖先 → バイナリ隣の順で `kb-mcp.toml` を探し、
    /// 最初に見つかったものを読む。全部失敗したら `Config::default()` を返す。
    ///
    /// 戻り値の `ConfigSource` は呼び出し元 (`main.rs`) が `tracing` ログに出す。
    pub fn discover(explicit: Option<&Path>) -> Result<Discovered> {
        let cwd = std::env::current_dir().context("failed to read current_dir")?;
        Self::discover_in(
            explicit,
            &cwd,
            alongside_binary_path().as_deref(),
            &TrustRoots::from_env(),
        )
    }

    /// `discover` を CWD 注入可能にしたバージョン。test-only。
    /// `alongside_binary_path()` は `current_exe()` 経由のため、production は
    /// `discover` を呼び、テストは CWD を制御するためにこちらを呼ぶ。
    ///
    /// **信頼判定は通さない** (BU-07)。既存テストが「ファイルに書いた値がその
    /// まま返る」ことを assert しているため、探索そのものの契約を保つ入口として
    /// 残してある。信頼判定込みの経路は [`Self::discover_in`]。
    #[cfg(test)]
    fn discover_at(explicit: Option<&Path>, cwd: &Path) -> Result<(Self, ConfigSource)> {
        Self::discover_with_alongside(explicit, cwd, alongside_binary_path().as_deref())
    }

    /// 探索 + **信頼判定** (BU-07)。production の [`Self::discover`] が呼ぶ経路。
    ///
    /// `TrustRoots` を引数で受けるのは、この feature 全体で環境変数を読む場所を
    /// [`TrustRoots::from_env`] 1 箇所に閉じ込めるため。テストが `set_var` を
    /// 呼ばずに済む (`cargo test` は同一プロセスの並列スレッドなので、env を
    /// 触るテストは並走する全テストに影響する — 実際に踏んだ経緯は
    /// [`cache_dir_env_override`] の doc)。
    pub(crate) fn discover_in(
        explicit: Option<&Path>,
        cwd: &Path,
        alongside: Option<&Path>,
        roots: &TrustRoots,
    ) -> Result<Discovered> {
        let (mut config, source, path) = Self::discover_located(explicit, cwd, alongside)?;
        let trust = classify_trust(source, path.as_deref(), roots);
        if trust == ConfigTrust::Untrusted {
            // path は Cwd / GitRoot でのみ Untrusted になり、その 2 つは必ず
            // ファイル由来なので `Some`。防御的に unwrap は避ける。
            let dir = path.as_deref().and_then(Path::parent);
            config.restrict_untrusted(path.as_deref(), dir, roots)?;
        }
        Ok(Discovered {
            config,
            source,
            trust,
            path,
        })
    }

    /// `discover` のフル注入版 (テスト専用)。バイナリ隣のパスも override する。
    #[cfg(test)]
    pub(crate) fn discover_with_alongside(
        explicit: Option<&Path>,
        cwd: &Path,
        alongside: Option<&Path>,
    ) -> Result<(Self, ConfigSource)> {
        let (cfg, source, _) = Self::discover_located(explicit, cwd, alongside)?;
        Ok((cfg, source))
    }

    /// 探索本体。読んだファイルのパスも返す (信頼判定に要る)。
    fn discover_located(
        explicit: Option<&Path>,
        cwd: &Path,
        alongside: Option<&Path>,
    ) -> Result<(Self, ConfigSource, Option<PathBuf>)> {
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
            return Ok((cfg, ConfigSource::Explicit, Some(resolved)));
        }

        // 2. CWD 直下
        let cwd_toml = cwd.join("kb-mcp.toml");
        if cwd_toml.exists() {
            let cfg = Self::load_from(&cwd_toml).with_context(|| {
                format!("failed to load config from cwd {}", cwd_toml.display())
            })?;
            return Ok((cfg, ConfigSource::Cwd, Some(cwd_toml)));
        }

        // 3. .git 祖先
        if let Some(root) = find_git_root(cwd) {
            let git_toml = root.join("kb-mcp.toml");
            if git_toml.exists() {
                let cfg = Self::load_from(&git_toml).with_context(|| {
                    format!("failed to load config from git root {}", git_toml.display())
                })?;
                return Ok((cfg, ConfigSource::GitRoot, Some(git_toml)));
            }
        }

        // 4. バイナリ隣 (legacy)
        if let Some(side) = alongside
            && side.exists()
        {
            let cfg = Self::load_from(side).with_context(|| {
                format!("failed to load config alongside binary {}", side.display())
            })?;
            return Ok((cfg, ConfigSource::AlongsideBinary, Some(side.to_path_buf())));
        }

        // 5. 未発見 → Default
        Ok((Self::default(), ConfigSource::NotFound, None))
    }

    /// 信頼できない場所で見つかった config に制限を掛ける (BU-07)。
    ///
    /// 制限するのは**特権的な 3 つ**だけで、`[search]` / `[quality_filter]` /
    /// `exclude_dirs` / `[parsers]` 等はそのまま通す。前者は「どのバイナリを
    /// 実行するか」「何を外に出すか」「誰から届くか」を決めるのに対し、後者は
    /// 選ばれた KB の中での見せ方でしかないため。
    ///
    /// **致命的なのは `kb_path` だけ**。他の 2 つは警告 + 安全側の値への差し替えで
    /// 続行する。ここで起動を止めると、Windows daemon が **何の出力も残さずに
    /// 死ぬ** (`kb-mcp-svc` が stdio を `Stdio::null()` にするため、利用者には
    /// 「動かない」以上の情報が出ない)。
    fn restrict_untrusted(
        &mut self,
        config_path: Option<&Path>,
        config_dir: Option<&Path>,
        roots: &TrustRoots,
    ) -> Result<()> {
        let shown = config_path.map(Path::to_path_buf).unwrap_or_default();

        // R1: モデルの読み込み先。`FASTEMBED_CACHE_DIR` として export され
        // (`apply_cache_dir_env`)、`resolve_cache_dir` がモデルディレクトリとして
        // 読む。hf-hub はキャッシュヒット時に hash も署名も検証しないので、この
        // キーは **ORT が実行する .onnx グラフを選ぶ**のと同じ。
        //
        // `None` に落とすだけでは不十分: `resolve_cache_dir` の最終 fallback は
        // **CWD 相対の `./.fastembed_cache`** で、いま信頼しないと決めた
        // ディレクトリの中にある。絶対パスの既定値で**置き換える**。
        //
        // **キーの有無に関わらず必ず設定する。** 「書いてあったら差し替える」
        // だけでは、キーを**省いた** planted config が素通りする:
        // `fastembed_cache_dir` が `None` なら env も設定されず、
        // `resolve_cache_dir` は `dirs::cache_dir()` に落ち、それも取れない環境では
        // **CWD 相対の `./.fastembed_cache`** — 攻撃者のディレクトリ — に行き着く。
        // キーの有無は「警告を出すかどうか」だけを決める。
        //
        // 代替先が決まらない場合は**続行しない**。共有 temp 配下の固定パス
        // (`/tmp/kb-mcp-fastembed` 等) を発明するのは、まさにここで潰そうと
        // している問題の再発 — マルチユーザ host では別ユーザが先に作って
        // キャッシュ構造を仕込める。「安全な場所を名指しできないなら止まる」
        // 方が、保証できないものを保証したふりをするより良い。
        // `dirs::cache_dir()` が `None` になるのは HOME / LOCALAPPDATA が
        // 無い環境だけで、そこは `--config` で明示すれば通る。
        if roots.cache_env_set {
            // `FASTEMBED_CACHE_DIR` が既にあるなら、config の値は
            // `cache_dir_env_override` の時点で捨てられる = そもそも効かない。
            // 差し替え先を探す必要は無いので、落とすだけにする。ここで
            // OS 標準キャッシュを要求すると、**エラー文が案内している
            // `FASTEMBED_CACHE_DIR` を設定しても直らない**という矛盾になる。
            if let Some(dir) = self.fastembed_cache_dir.take() {
                tracing::warn!(
                    config = %shown.display(),
                    requested = %dir.display(),
                    "ignoring fastembed_cache_dir from a config found in an untrusted location \
                     (FASTEMBED_CACHE_DIR is already set and takes precedence anyway)"
                );
            }
        } else {
            let Some(safe_cache) = roots.cache_dir.as_ref().map(|d| d.join("fastembed")) else {
                anyhow::bail!(
                    "cannot determine a cache directory for embedding models, so the model \
                     directory named by {} cannot be safely replaced.\n\
                     This config was found by kb-mcp itself (not named with --config), so its \
                     directory is treated as untrusted. Set FASTEMBED_CACHE_DIR to a directory \
                     you control, or pass --config {} to accept the config as written.",
                    shown.display(),
                    shown.display(),
                );
            };
            if let Some(dir) = self.fastembed_cache_dir.replace(safe_cache.clone()) {
                tracing::warn!(
                    config = %shown.display(),
                    requested = %dir.display(),
                    using = %safe_cache.display(),
                    "ignoring fastembed_cache_dir from a config found in an untrusted location \
                     (it selects which model file is loaded); pass --config to accept it"
                );
            }
        }

        // R2: 到達範囲。BU-01 は CLI `--bind` の非 loopback だけを gate し、
        // config 由来は「運用者の意図表明」とみなして免除した。その前提は
        // **発見された config では成立しない**ので、ここで戻す。
        if let Some(http) = self.transport.as_mut().and_then(|t| t.http.as_mut()) {
            if let Some(bind) = http.bind.as_deref()
                && let Ok(addr) = bind.parse::<std::net::SocketAddr>()
                && is_non_loopback(&addr)
            {
                let downgraded = std::net::SocketAddr::from(([127, 0, 0, 1], addr.port()));
                tracing::warn!(
                    config = %shown.display(),
                    requested = bind,
                    using = %downgraded,
                    "downgrading a non-loopback bind from a config found in an untrusted \
                     location; pass --config to accept it"
                );
                http.bind = Some(downgraded.to_string());
            }
            if http.allowed_hosts.take().is_some() {
                tracing::warn!(
                    config = %shown.display(),
                    "ignoring [transport.http].allowed_hosts from an untrusted config \
                     (restoring the loopback-only Host check)"
                );
            }
            if http.healthz_public.take().is_some() {
                tracing::warn!(
                    config = %shown.display(),
                    "ignoring [transport.http].healthz_public from an untrusted config"
                );
            }
        }

        // R3: 何を索引して LLM クライアントに渡すか。**唯一の致命的規則**。
        //
        // 「閉じ込める」のではなく「境界を弾く」— `<home>/Documents` のような
        // 具体的な指定は通す。塞ぐのはユーザ名を知らなくても書ける昇格
        // (`../..` / `/` / `C:\Users` / `/home` と、それらへの symlink)。
        if let Some(kb) = self.kb_path.as_deref()
            && let Some(reason) = forbidden_kb_path(kb, config_dir, roots)
        {
            anyhow::bail!(
                "refusing kb_path {} from {}: {reason}.\n\
                 This config was found by kb-mcp itself (not named with --config), so its \
                 directory is treated as untrusted. Pass --config {} to accept it, or point \
                 kb_path at a specific directory.",
                kb.display(),
                shown.display(),
                shown.display(),
            );
        }

        Ok(())
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
            && self.contextual.is_none()
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
    /// - `[search.fusion].rrf_k` が有限かつ `>= 1.0`
    /// - `[search.fusion].bm25_*_weight` が有限かつ `>= 0.0`
    /// - `[search.fusion]` の 3 重みが全て `0.0` でない
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

            // Fusion (RRF k / bm25 column weight) レンジチェック。
            // feature-47 D-2: db 層の bind parameter 経路は NaN / inf を
            // silent に通すため、ここが唯一のゲートである。
            let f = &s.fusion;
            if !f.rrf_k.is_finite() || f.rrf_k < 1.0 || f.rrf_k > MAX_RRF_K {
                anyhow::bail!(
                    "[search.fusion].rrf_k must be a finite value in [1.0, {MAX_RRF_K:e}] \
                     (Elasticsearch rank_constant convention; the upper bound keeps ranks \
                     distinguishable in f32), got {}",
                    f.rrf_k
                );
            }
            for (key, w) in [
                ("bm25_heading_weight", f.bm25_heading_weight),
                ("bm25_context_weight", f.bm25_context_weight),
                ("bm25_content_weight", f.bm25_content_weight),
            ] {
                if !w.is_finite() || !(0.0..=MAX_BM25_WEIGHT).contains(&w) {
                    anyhow::bail!(
                        "[search.fusion].{key} must be a finite value in [0.0, \
                         {MAX_BM25_WEIGHT:e}] (a negative weight puts a pole in the BM25 \
                         denominator and silently inverts the ranking; only the ratio between \
                         the three weights affects ordering, so a huge one buys nothing), got {w}"
                    );
                }
            }
            if f.bm25_heading_weight == 0.0
                && f.bm25_context_weight == 0.0
                && f.bm25_content_weight == 0.0
            {
                anyhow::bail!(
                    "[search.fusion]: all three bm25 weights are 0.0; bm25() then returns -0.0 for \
                     every row and the FTS ranking collapses. Set at least one weight > 0.0."
                );
            }
        }
        Ok(())
    }

    /// `fastembed_cache_dir` が設定されていて、かつ環境変数
    /// `FASTEMBED_CACHE_DIR` が未設定なら、プロセス環境に適用する。
    /// `Embedder::with_model` が `resolve_cache_dir()` で拾う前に呼ぶこと。
    pub fn apply_cache_dir_env(&self) {
        let env_already_set = std::env::var_os("FASTEMBED_CACHE_DIR").is_some();
        let Some(dir) =
            cache_dir_env_override(env_already_set, self.fastembed_cache_dir.as_deref())
        else {
            return;
        };
        // SAFETY: プロセス単一スレッド (main 起動直後) でのみ呼ぶ想定。
        unsafe {
            std::env::set_var("FASTEMBED_CACHE_DIR", dir);
        }
    }
}

/// `apply_cache_dir_env` の判断部分だけを取り出したもの。返り値が `Some` の
/// ときだけ env に書き込む。
///
/// env の読み書きから切り離してあるのはテストの都合ではなく、**テストが env を
/// 触ること自体が危険**だから。`cargo test` はテストを別プロセスではなく同一
/// プロセスの並列スレッドで走らせるため、1 つのテストが `set_var` /
/// `remove_var` を呼ぶと同時に走っている他のテスト全部に影響する。
///
/// 実際に踏んだ: この判断を env 経由で検証していた頃、テストの後始末である
/// `remove_var("FASTEMBED_CACHE_DIR")` が、並走していた embedder のモデル DL 先を
/// OS 既定ディレクトリへ逸らしていた。CI が指定した cache ディレクトリの外に
/// モデルが落ちるので、nightly が緑のまま毎晩 4.6 GB を再 DL する状態になっていた
/// (AU-63 / PR #85)。
fn cache_dir_env_override(env_already_set: bool, configured: Option<&Path>) -> Option<&Path> {
    if env_already_set {
        return None; // env を優先
    }
    configured
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

// ---------------------------------------------------------------------------
// Config trust (BU-07)
// ---------------------------------------------------------------------------

/// [`Config::discover`] の戻り値。
///
/// `path` を返すのは、`source` の variant 名だけでは**どのファイルが勝ったのか**
/// が分からないため (BU-07 以前の唯一の記録は `source=Cwd` の 1 行だった)。
#[derive(Debug)]
pub struct Discovered {
    pub config: Config,
    pub source: ConfigSource,
    pub trust: ConfigTrust,
    /// 読んだファイル。`ConfigSource::NotFound` のときだけ `None`。
    pub path: Option<PathBuf>,
}

/// 発見した `kb-mcp.toml` を**運用者が書いたもの**として扱ってよいか。
///
/// 判定材料は**置き場所だけ**で、ファイルの中身は一切見ない (git が
/// `safe.directory` の教訓として明文化している規則 — 信頼判定の入力に
/// 信頼していないデータを混ぜない)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigTrust {
    /// argv で名指しされた / インストール先に置かれた / そもそもファイルが無い。
    Trusted,
    /// kb-mcp が自分で見つけた = **CWD とその `.git` 祖先**。そこに何が置かれて
    /// いるかを決めたのは運用者とは限らない (clone したリポジトリ、共有ドライブ、
    /// 展開したアーカイブ)。
    Untrusted,
}

/// 信頼判定と制限に要る**環境の事実**をまとめて注入するための型 (BU-07)。
///
/// env と `dirs::*` を読むのは [`Self::from_env`] だけ。テストは値を注入する
/// (`cargo test` は同一プロセスの並列スレッドなので、env を触るテストは並走する
/// 全テストに影響する — 経緯は [`cache_dir_env_override`] の doc)。
#[derive(Debug, Clone, Default)]
pub struct TrustRoots {
    /// 「ここにある config は運用者のもの」と見なすディレクトリ群。
    roots: Vec<PathBuf>,
    /// 判定に使うホームディレクトリ (`kb_path` の境界チェック用)。
    home: Option<PathBuf>,
    /// OS 標準のキャッシュディレクトリ (`fastembed_cache_dir` の差し替え先)。
    cache_dir: Option<PathBuf>,
    /// `FASTEMBED_CACHE_DIR` が既に環境にあるか。**あるなら config の
    /// `fastembed_cache_dir` はそもそも効かない** (`cache_dir_env_override` は
    /// env が既設なら `None` を返す) ので、差し替え先を探す必要すら無い。
    cache_env_set: bool,
}

impl TrustRoots {
    /// production の値。
    ///
    /// - `KB_MCP_CONFIG_HOME` (service 層が同じ変数で config home を移せる)
    /// - `dirs::config_dir()/kb-mcp` — `kb-mcp service install` が
    ///   `<config_home>/kb-mcp.toml` を書き、各 backend が WorkingDirectory を
    ///   そこに設定して `--config` 無しで起動する。**インストール済み daemon が
    ///   Trusted であり続けるのはこの根による**。service 名まで含めず `kb-mcp`
    ///   の階層で見るのは、`<config_dir>/kb-mcp/<service>/` 配下すべてを
    ///   1 本で拾うため。
    pub fn from_env() -> Self {
        Self::from_bases(
            std::env::var_os("KB_MCP_CONFIG_HOME").map(PathBuf::from),
            dirs::config_dir(),
            dirs::home_dir(),
            dirs::cache_dir(),
            std::env::var_os("FASTEMBED_CACHE_DIR").is_some(),
        )
    }

    /// [`Self::from_env`] の純粋な中身。env を読む場所を 1 箇所に閉じ込めたまま
    /// 「どのディレクトリを根にするか」をテストできるようにするための分離。
    pub(crate) fn from_bases(
        config_home_override: Option<PathBuf>,
        config_dir: Option<PathBuf>,
        home: Option<PathBuf>,
        cache_dir: Option<PathBuf>,
        cache_env_set: bool,
    ) -> Self {
        // **どちらの base にも `kb-mcp` を足す。**
        // `service::resolve_config_home` が `base.join("kb-mcp").join(service)` を
        // 返すため、base そのものを根にすると根が広くなりすぎる —
        // `KB_MCP_CONFIG_HOME=$HOME` なら home 配下の全リポジトリが信頼されて
        // しまい、この feature 全体が無効化される。
        let roots = [config_home_override, config_dir]
            .into_iter()
            .flatten()
            .map(|b| b.join("kb-mcp"))
            .collect();
        Self {
            roots,
            home,
            cache_dir,
            cache_env_set,
        }
    }

    /// テスト用のコンストラクタ。
    #[cfg(test)]
    pub(crate) fn new(roots: Vec<PathBuf>, home: Option<PathBuf>) -> Self {
        Self {
            roots,
            home,
            // テストは OS 標準キャッシュを持つ前提でよい。env 既設の分岐は
            // `new_with_cache` で明示的に作る。
            cache_dir: Some(std::env::temp_dir().join("kb-mcp-test-cache")),
            cache_env_set: false,
        }
    }

    /// キャッシュ側の環境事実だけ差し替えるテスト用コンストラクタ。
    #[cfg(test)]
    pub(crate) fn new_with_cache(
        home: Option<PathBuf>,
        cache_dir: Option<PathBuf>,
        cache_env_set: bool,
    ) -> Self {
        Self {
            roots: Vec::new(),
            home,
            cache_dir,
            cache_env_set,
        }
    }

    fn contains(&self, dir: &Path) -> bool {
        let dir = canonical_or_as_is(dir);
        self.roots
            .iter()
            .any(|r| dir.starts_with(canonical_or_as_is(r)))
    }
}

/// `canonicalize` できればそれを、できなければ入力をそのまま返す。
///
/// 判定を「存在するパスでしか動かない」ものにしないための緩和。
/// symlink 解決が要る場面 (`kb_path` の境界チェック) では、解決に失敗した
/// symlink を**別途エラーにする**ので、ここで握りつぶしても穴にはならない。
fn canonical_or_as_is(p: &Path) -> PathBuf {
    std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
}

/// 置き場所だけから信頼を決める (BU-07)。ファイル内容は見ない。
pub(crate) fn classify_trust(
    source: ConfigSource,
    path: Option<&Path>,
    roots: &TrustRoots,
) -> ConfigTrust {
    match source {
        // argv は信頼する入力。argv が敵性なら `command` 自体が敵性なので、
        // この層で守れるものは無い。
        ConfigSource::Explicit => ConfigTrust::Trusted,
        // 書き込むにはインストール先への書き込み権限が要る。それがあるなら
        // バイナリ自体を差し替える方が早い。
        ConfigSource::AlongsideBinary => ConfigTrust::Trusted,
        // ファイルが無い = 組み込み既定値。
        ConfigSource::NotFound => ConfigTrust::Trusted,
        ConfigSource::Cwd | ConfigSource::GitRoot => match path.and_then(Path::parent) {
            Some(dir) if roots.contains(dir) => ConfigTrust::Trusted,
            _ => ConfigTrust::Untrusted,
        },
    }
}

/// `SocketAddr` の IP が loopback でないか。
fn is_non_loopback(addr: &std::net::SocketAddr) -> bool {
    !addr.ip().is_loopback()
}

/// 信頼できない config が `kb_path` に指定してはいけない場所か。
/// 該当すれば理由 (エラーメッセージ用) を返す。
///
/// **境界を弾くだけで、閉じ込めはしない。** 具体的なディレクトリ
/// (`<home>/Documents` 等) は通る — 閉じ込めると、出荷済みの `personal` レシピ
/// (「project root に置いて絶対パスの `kb_path` を書く」と明記されている) が
/// 壊れるため。塞ぐのは**ユーザ名を知らなくても書ける昇格**:
/// `../..` / `/` / `C:\Users` / `/home` と、それらを指す symlink。
pub(crate) fn forbidden_kb_path(
    kb_path: &Path,
    config_dir: Option<&Path>,
    roots: &TrustRoots,
) -> Option<&'static str> {
    // symlink は必ず解決して判定する。解決できない symlink は判定不能なので
    // 拒否する (`kb -> /` を commit しておく攻撃を素通しさせない)。
    let canon = match std::fs::canonicalize(kb_path) {
        Ok(c) => c,
        Err(_) => {
            let is_link = std::fs::symlink_metadata(kb_path)
                .map(|m| m.file_type().is_symlink())
                .unwrap_or(false);
            if is_link {
                return Some("it is a symlink that cannot be resolved");
            }
            // 未作成のディレクトリは `index` が作る / エラーにするので、
            // ここでは字面のまま判定を続ける。
            kb_path.to_path_buf()
        }
    };

    if canon.parent().is_none() {
        return Some("it is a filesystem root");
    }
    if let Some(home) = roots.home.as_deref() {
        let home = canonical_or_as_is(home);
        if canon == home {
            return Some("it is the home directory");
        }
        if home.starts_with(&canon) {
            return Some("it is an ancestor of the home directory");
        }
    }
    if let Some(dir) = config_dir {
        let dir = canonical_or_as_is(dir);
        // 等しい場合は許す = 「このリポジトリ自身を索引する」通常の使い方。
        if dir.starts_with(&canon) && dir != canon {
            return Some("it is an ancestor of the directory holding the config file");
        }
    }
    None
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
    fn test_contextual_config_defaults_enabled_false() {
        // judgment gate (eval-baseline-2026-07-20-context.md): kb-mcp の素の
        // default 構成 (reranker なし) では recall@5/MRR が悪化するため opt-in。
        let c = ContextualConfig::default();
        assert!(!c.enabled);
    }

    #[test]
    fn test_contextual_config_parses_from_toml() {
        let mut file = tempfile("kb-mcp-config-contextual");
        writeln!(file, "[contextual]\nenabled = false\n").unwrap();
        let cfg = Config::load_from(file.path()).unwrap();
        let c = cfg.contextual.expect("contextual must be Some");
        assert!(!c.enabled);
    }

    #[test]
    fn test_contextual_config_parses_enabled_true_from_toml() {
        let mut file = tempfile("kb-mcp-config-contextual-on");
        writeln!(file, "[contextual]\nenabled = true\n").unwrap();
        let cfg = Config::load_from(file.path()).unwrap();
        let c = cfg.contextual.expect("contextual must be Some");
        assert!(c.enabled);
    }

    #[test]
    fn test_contextual_config_rejects_unknown_field() {
        let toml = "[contextual]\nenabled = true\nbogus = 1\n";
        let result: std::result::Result<crate::config::Config, _> = toml::from_str(toml);
        assert!(result.is_err(), "unknown [contextual] field must reject");
    }

    #[test]
    fn test_contextual_section_only_defaults_enabled_false() {
        let mut file = tempfile("kb-mcp-config-contextual-only");
        writeln!(file, "[contextual]\n").unwrap();
        let cfg = Config::load_from(file.path()).unwrap();
        assert!(!cfg.contextual.expect("Some").enabled);
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
             enabled = [\"md\", \"rst\"]\n"
        )
        .unwrap();
        let cfg = Config::load_from(file.path()).unwrap();
        // validate is passed, but build_parser_registry should fail on "rst"
        let err = cfg
            .build_parser_registry()
            .expect_err("rst must be rejected");
        assert!(err.to_string().contains("rst"));
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
        let dir = crate::test_support::unique_temp_path("kb-mcp-test-relpath");
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

    /// AU-06 (codex P2): example が勧める `[parsers].enabled` が、実際に
    /// Registry を組めること。コメント行で示している例も含めて全部見る。
    ///
    /// [`test_toml_example_parses_with_all_keys_uncommented`] は example が
    /// **TOML として `Config` に入るか**しか見ていない。id が registry に
    /// 載るかは別の検査で、そこが空いていたため `.xls` を registry から
    /// 外したときに「全形式を index する例」が起動時エラーになる状態を
    /// 見逃した。parser を増減させたら必ずここで気付く。
    #[test]
    fn every_enabled_list_in_the_example_builds_a_registry() {
        let example_path = Path::new(env!("CARGO_MANIFEST_DIR")).join("kb-mcp.toml.example");
        let raw = std::fs::read_to_string(&example_path).expect("example must exist");

        let mut checked = 0usize;
        for line in raw.lines() {
            let Some(idx) = line.find("enabled = [") else {
                continue;
            };
            let arr = &line[idx + "enabled = ".len()..];
            let end = arr
                .find(']')
                .unwrap_or_else(|| panic!("enabled array should close on one line: {line}"));
            let ids: Vec<String> = arr[1..end]
                .split(',')
                .map(|s| s.trim().trim_matches('"').to_string())
                .filter(|s| !s.is_empty())
                .collect();
            assert!(!ids.is_empty(), "could not read ids from: {line}");
            crate::parser::Registry::from_enabled(&ids).unwrap_or_else(|e| {
                panic!(
                    "kb-mcp.toml.example suggests a [parsers].enabled that this build \
                     rejects:\n  {line}\n  {e}"
                )
            });
            checked += 1;
        }
        assert!(
            checked >= 2,
            "expected at least the active list and the all-formats example, found {checked}"
        );
    }

    /// (BU-13) Copying `kb-mcp.toml.example` to `kb-mcp.toml` must change
    /// nothing.
    ///
    /// It used to change three things. `[transport] kind = "http"` shipped
    /// uncommented, so copying the template swapped stdio for a listening
    /// socket; `[parsers].enabled` shipped as `["md", "txt"]`, so it also
    /// changed what got indexed; and `[best_practice].path_templates` shipped
    /// populated, which switched on an opt-in MCP tool. None of that is
    /// visible by reading the file — it reads like documentation — so this
    /// asserts the property directly: parse the file exactly as shipped and
    /// require every resolved value to equal the built-in default.
    ///
    /// `test_toml_example_parses_with_all_keys_uncommented` is the complement:
    /// it uncomments everything to prove the keys are still spelled right.
    /// This one proves the file is inert until a human uncomments something.
    #[test]
    fn the_example_as_shipped_changes_no_behaviour() {
        let example_path = Path::new(env!("CARGO_MANIFEST_DIR")).join("kb-mcp.toml.example");
        let raw = std::fs::read_to_string(&example_path).expect("example must exist");
        let cfg: Config = toml::from_str(&raw)
            .unwrap_or_else(|e| panic!("kb-mcp.toml.example must parse exactly as shipped: {e}"));

        let transport =
            crate::transport::Transport::resolve(None, None, None, cfg.transport.as_ref())
                .expect("the shipped example must resolve to a transport");
        assert!(
            matches!(transport, crate::transport::Transport::Stdio),
            "copying kb-mcp.toml.example must NOT open a listening socket, but it \
             resolved to {transport:?}. Comment out `kind` / `[transport.http]`, or \
             leave `kind = \"stdio\"` in place."
        );

        assert!(
            cfg.best_practice
                .as_ref()
                .is_none_or(|bp| bp.path_templates.is_empty()),
            "copying the example must leave `get_best_practice` unconfigured — it is \
             an opt-in tool, so shipping populated `path_templates` turns it on for \
             everyone who copies the file"
        );

        let parsers = cfg
            .parsers
            .as_ref()
            .map(|p| p.enabled.clone())
            .unwrap_or_else(|| vec!["md".to_string()]);
        assert_eq!(
            parsers,
            vec!["md".to_string()],
            "copying the example must not change which extensions are indexed; \
             anything beyond .md belongs in a commented-out line"
        );

        if let Some(qf) = cfg.quality_filter.as_ref() {
            assert_eq!(
                *qf,
                QualityFilterConfig::default(),
                "the [quality_filter] values in the example must equal the defaults"
            );
        }
        if let Some(w) = cfg.watch.as_ref() {
            assert_eq!(
                *w,
                WatchConfig::default(),
                "the [watch] values in the example must equal the defaults"
            );
        }
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
        //
        // 判断部分だけを検証する。以前はここで実際に `set_var` / `remove_var` を
        // 呼んでいたが、`cargo test` は同一プロセスの並列スレッドでテストを走らせる
        // ので、後始末の `remove_var` が並走する他テストのモデル DL 先まで
        // 変えてしまっていた (`cache_dir_env_override` の doc comment 参照)。
        assert_eq!(
            cache_dir_env_override(true, Some(Path::new("/from-config"))),
            None
        );
    }

    #[test]
    fn test_apply_cache_dir_env_uses_config_when_env_unset() {
        assert_eq!(
            cache_dir_env_override(false, Some(Path::new("/from-config"))),
            Some(Path::new("/from-config"))
        );
    }

    #[test]
    fn test_apply_cache_dir_env_is_noop_when_neither_is_set() {
        assert_eq!(cache_dir_env_override(false, None), None);
    }

    /// Helper: 一意名の一時ファイルを作って `File` を返す。tempfile crate に
    /// 依存しないように素朴に作る。
    fn tempfile(prefix: &str) -> NamedTempFile {
        // Suffix before the extension: `.toml` has to stay last.
        let mut path = std::env::temp_dir();
        path.push(format!(
            "{prefix}-{}.toml",
            crate::test_support::unique_suffix()
        ));
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

    #[test]
    fn test_fusion_defaults_when_section_omitted() {
        // E-10: [search.fusion] 未指定 (既存ユーザ全員) で builtin default。
        let toml = "[search]\nmin_confidence_ratio = 1.5\n";
        let cfg: crate::config::Config = toml::from_str(toml).unwrap();
        let s = cfg.search.as_ref().unwrap();
        assert_eq!(s.fusion, FusionConfig::default());
        assert!(s.fusion.is_builtin_default());
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn test_fusion_explicit_defaults_are_still_builtin_default() {
        // 既定値と同値を明示指定しても is_builtin_default = true
        // (= eval fingerprint に載らず旧 history と互換、D-7)。
        let toml = concat!(
            "[search.fusion]\n",
            "rrf_k = 60.0\n",
            "bm25_heading_weight = 2.0\n",
            "bm25_context_weight = 1.0\n",
            "bm25_content_weight = 1.0\n",
        );
        let cfg: crate::config::Config = toml::from_str(toml).unwrap();
        let s = cfg.search.as_ref().unwrap();
        assert!(s.fusion.is_builtin_default());
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn test_fusion_params_conversion_matches_db_default() {
        // D-3: config 側と db 側の既定値二重管理を From の一方向変換で固定する。
        // lib 自身の #[cfg(test)] からは `kb_mcp::` は解決できない
        // (lib.rs に `extern crate self as kb_mcp` は無い)。`crate::` を使う。
        let p = crate::db::FusionParams::from(&FusionConfig::default());
        assert_eq!(p, crate::db::FusionParams::default());
    }

    #[test]
    fn test_fusion_rejects_negative_weight() {
        // E-1: 負 weight は BM25 分母の極 → スコア発散・静かな順位破壊。
        let toml = "[search.fusion]\nbm25_heading_weight = -1.0\n";
        let cfg: crate::config::Config = toml::from_str(toml).unwrap();
        let err = cfg.validate().unwrap_err().to_string();
        assert!(
            err.contains("bm25_heading_weight"),
            "error must name the offending key: {err}"
        );
    }

    #[test]
    fn test_fusion_rejects_non_finite() {
        // E-2: bind parameter 経路は NaN / inf を silent に通すので
        // validate が唯一のゲート。
        let toml = "[search.fusion]\nbm25_content_weight = nan\n";
        let cfg: crate::config::Config = toml::from_str(toml).unwrap();
        assert!(cfg.validate().is_err(), "NaN weight must be rejected");

        let toml = "[search.fusion]\nrrf_k = inf\n";
        let cfg: crate::config::Config = toml::from_str(toml).unwrap();
        assert!(cfg.validate().is_err(), "inf rrf_k must be rejected");
    }

    /// (AU-32) The reason `MAX_RRF_K` is where it is, kept next to the bound so
    /// it can be re-checked rather than taken on trust.
    ///
    /// RRF scores as `1.0 / (rrf_k + rank + 1.0)` in f32. Past a certain
    /// magnitude the rank stops changing the float and every hit ties — the
    /// ranking silently stops ordering. This asserts both halves: the bound is
    /// below where that starts, and above where it starts the collapse is real.
    #[test]
    fn max_rrf_k_sits_below_the_f32_rank_collapse() {
        let score = |k: f32, rank: u32| 1.0f32 / (k + rank as f32 + 1.0);

        assert!(
            score(MAX_RRF_K, 0) != score(MAX_RRF_K, 1),
            "at the bound, adjacent ranks must still be distinguishable"
        );

        // Where it actually breaks. If a future refactor moves the formula to
        // f64 these become false and this test says so, instead of the bound
        // quietly staying stricter than it needs to be.
        assert_eq!(
            score(1.6e7, 0),
            score(1.6e7, 1),
            "adjacent ranks tie by 1.6e7 — the measurement the bound is based on"
        );
        assert_eq!(
            score(1e9, 0),
            score(1e9, 9),
            "by 1e9 even ranks ten apart tie: total collapse"
        );
    }

    /// The bounds must actually be enforced, not merely defined.
    #[test]
    fn fusion_rejects_values_past_the_upper_bounds() {
        let toml = format!("[search.fusion]\nrrf_k = {}\n", MAX_RRF_K * 10.0);
        let cfg: crate::config::Config = toml::from_str(&toml).unwrap();
        assert!(
            cfg.validate().is_err(),
            "rrf_k past the bound must be rejected"
        );

        let toml = format!(
            "[search.fusion]\nbm25_content_weight = {}\n",
            MAX_BM25_WEIGHT * 10.0
        );
        let cfg: crate::config::Config = toml::from_str(&toml).unwrap();
        assert!(
            cfg.validate().is_err(),
            "a bm25 weight past the bound must be rejected"
        );

        // The bounds themselves stay legal — an off-by-one here would reject
        // the documented maximum.
        let toml = format!(
            "[search.fusion]\nrrf_k = {MAX_RRF_K}\nbm25_content_weight = {MAX_BM25_WEIGHT}\n"
        );
        let cfg: crate::config::Config = toml::from_str(&toml).unwrap();
        assert!(cfg.validate().is_ok(), "the bounds must be inclusive");
    }

    #[test]
    fn test_fusion_rejects_all_zero_weights() {
        // E-3: 全カラム 0 だと bm25() が全行 -0.0 を返し順位が崩壊する。
        let toml = concat!(
            "[search.fusion]\n",
            "bm25_heading_weight = 0.0\n",
            "bm25_context_weight = 0.0\n",
            "bm25_content_weight = 0.0\n",
        );
        let cfg: crate::config::Config = toml::from_str(toml).unwrap();
        let err = cfg.validate().unwrap_err().to_string();
        assert!(
            err.contains("all three bm25 weights"),
            "error must explain the all-zero collapse: {err}"
        );
    }

    #[test]
    fn test_fusion_rejects_rrf_k_below_one() {
        // E-5: Elasticsearch rank_constant 準拠の下限 1.0。
        let toml = "[search.fusion]\nrrf_k = 0.5\n";
        let cfg: crate::config::Config = toml::from_str(toml).unwrap();
        let err = cfg.validate().unwrap_err().to_string();
        assert!(err.contains("rrf_k"), "error must name rrf_k: {err}");
    }

    #[test]
    fn test_fusion_accepts_single_zero_weight() {
        // 0.0 は単体では合法 (その列の tf 寄与が消えるだけ)。
        let toml = "[search.fusion]\nbm25_context_weight = 0.0\n";
        let cfg: crate::config::Config = toml::from_str(toml).unwrap();
        assert!(cfg.validate().is_ok());
        assert!(!cfg.search.as_ref().unwrap().fusion.is_builtin_default());
    }

    /// テスト用 tempdir (Drop で自動削除)。`tests/validate_cli.rs::TempKb` の lib 版。
    struct TempDir {
        path: PathBuf,
    }
    impl TempDir {
        fn new(prefix: &str) -> Self {
            let path = crate::test_support::unique_temp_path(prefix);
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

    // =======================================================================
    // BU-07: a config kb-mcp found by itself is not the operator's word
    // =======================================================================

    /// The privileged half of a planted config, as one string. Every test below
    /// feeds the *same* content from different locations, so what is being
    /// tested is the location rule and nothing else.
    fn planted_toml(kb_path: &str) -> String {
        format!(
            "kb_path = {kb_path:?}\n\
             fastembed_cache_dir = \"evil-cache\"\n\
             [transport]\n\
             kind = \"http\"\n\
             [transport.http]\n\
             bind = \"0.0.0.0:3100\"\n\
             allowed_hosts = [\"attacker.example\"]\n\
             healthz_public = false\n"
        )
    }

    /// No environment reads anywhere in these tests: `TrustRoots` is injected.
    /// `cargo test` runs tests as threads of one process, so a test that called
    /// `set_var` would reach into every test running beside it.
    fn roots_for(config_home: Option<&Path>, home: Option<&Path>) -> TrustRoots {
        TrustRoots::new(
            config_home.map(Path::to_path_buf).into_iter().collect(),
            home.map(Path::to_path_buf),
        )
    }

    #[test]
    fn trust_follows_the_location_and_never_the_contents() {
        let dir = TempDir::new("kb-mcp-trust-classify");
        let home = TempDir::new("kb-mcp-trust-home");
        let toml = dir.path().join("kb-mcp.toml");
        let roots = roots_for(Some(home.path()), Some(home.path()));

        // argv, the install directory, and "no file at all" are the operator's.
        for source in [
            ConfigSource::Explicit,
            ConfigSource::AlongsideBinary,
            ConfigSource::NotFound,
        ] {
            assert_eq!(
                classify_trust(source, Some(&toml), &roots),
                ConfigTrust::Trusted,
                "{source:?} must be trusted"
            );
        }

        // What kb-mcp found by itself, outside a config home, is not.
        for source in [ConfigSource::Cwd, ConfigSource::GitRoot] {
            assert_eq!(
                classify_trust(source, Some(&toml), &roots),
                ConfigTrust::Untrusted,
                "{source:?} outside a config home must be untrusted"
            );
        }

        // ...but the installed daemon's own config home is trusted, which is
        // what keeps every service backend working: they set WorkingDirectory
        // to the config home and start without --config.
        let daemon_toml = home.path().join("svc").join("kb-mcp.toml");
        std::fs::create_dir_all(daemon_toml.parent().unwrap()).unwrap();
        std::fs::write(&daemon_toml, "").unwrap();
        assert_eq!(
            classify_trust(ConfigSource::Cwd, Some(&daemon_toml), &roots),
            ConfigTrust::Trusted,
            "a config under a trust root is the operator's, however it was found"
        );
    }

    #[test]
    fn forbidden_kb_paths_are_the_ones_written_without_knowing_the_username() {
        let repo = TempDir::new("kb-mcp-kbpath-repo");
        let home = TempDir::new("kb-mcp-kbpath-home");
        let cfg_dir = repo.path().join("project");
        std::fs::create_dir_all(&cfg_dir).unwrap();
        let docs = home.path().join("Documents");
        std::fs::create_dir_all(&docs).unwrap();
        let roots = roots_for(None, Some(home.path()));

        let root = if cfg!(windows) {
            PathBuf::from("C:\\")
        } else {
            PathBuf::from("/")
        };
        // Assert the *reason*, not just that something was refused. The four
        // rules overlap by construction — a filesystem root is also an ancestor
        // of home, and the home directory is also an ancestor of itself — so a
        // bare `is_some()` lets any one rule be deleted while its neighbour
        // keeps the test green. (Verified: removing three of the four rules
        // left an `is_some()` version of this test passing.)
        for (path, expected) in [
            (root, "it is a filesystem root"),
            (home.path().to_path_buf(), "it is the home directory"),
            (
                home.path().parent().unwrap().to_path_buf(),
                "it is an ancestor of the home directory",
            ),
            (
                repo.path().to_path_buf(),
                "it is an ancestor of the directory holding the config file",
            ),
        ] {
            assert_eq!(
                forbidden_kb_path(&path, Some(&cfg_dir), &roots),
                Some(expected),
                "{} must be refused for exactly this reason",
                path.display()
            );
        }

        for (path, what) in [
            (cfg_dir.clone(), "the config's own directory"),
            (cfg_dir.join("docs"), "a directory under the config"),
            (docs, "a specific directory in home"),
        ] {
            assert!(
                forbidden_kb_path(&path, Some(&cfg_dir), &roots).is_none(),
                "{what} ({}) must be allowed -- the rule bounds, it does not confine",
                path.display()
            );
        }
    }

    /// A symlink committed to a repository (`kb -> /`, mode 120000) is
    /// materialized by `git clone` on POSIX, so the check has to judge the
    /// target rather than the name. Unix-only because creating a symlink on
    /// Windows needs Developer Mode or elevation, which CI runners may lack —
    /// the ubuntu and macOS legs cover it.
    #[cfg(unix)]
    #[test]
    fn a_symlink_kb_path_is_judged_by_its_target() {
        let repo = TempDir::new("kb-mcp-kbpath-symlink");
        let home = TempDir::new("kb-mcp-kbpath-symlink-home");
        let cfg_dir = repo.path().join("project");
        std::fs::create_dir_all(&cfg_dir).unwrap();
        let roots = roots_for(None, Some(home.path()));

        // Resolvable, and its target is home.
        let link = cfg_dir.join("kb");
        std::os::unix::fs::symlink(home.path(), &link).unwrap();
        assert_eq!(
            forbidden_kb_path(&link, Some(&cfg_dir), &roots),
            Some("it is the home directory"),
            "a symlink must be judged by what it points at, not by where it sits"
        );

        // Dangling: the target cannot be inspected, so it cannot be cleared.
        let dangling = cfg_dir.join("dangling");
        std::os::unix::fs::symlink(repo.path().join("nowhere"), &dangling).unwrap();
        assert_eq!(
            forbidden_kb_path(&dangling, Some(&cfg_dir), &roots),
            Some("it is a symlink that cannot be resolved"),
        );
    }

    #[test]
    fn an_untrusted_config_cannot_choose_which_model_file_is_loaded() {
        let dir = TempDir::new("kb-mcp-untrusted-cache");
        std::fs::write(dir.path().join("kb-mcp.toml"), planted_toml("kb")).unwrap();
        let roots = roots_for(None, None);

        let d = Config::discover_in(None, dir.path(), None, &roots).expect("discover ok");
        let (cfg, source, trust) = (d.config, d.source, d.trust);
        assert_eq!(source, ConfigSource::Cwd);
        assert_eq!(trust, ConfigTrust::Untrusted);

        let used = cfg.fastembed_cache_dir.expect("a cache dir is always set");
        assert!(
            !used.starts_with(dir.path()),
            "the model directory must not be one the planted config chose: {}",
            used.display()
        );
        // Not merely `None`: `resolve_cache_dir`'s last fallback is the
        // cwd-relative `./.fastembed_cache`, which is inside the directory we
        // just decided not to trust.
        assert!(
            used.is_absolute(),
            "the substitute must be absolute, got {}",
            used.display()
        );
    }

    /// Omitting the key is an attack, not an absence. With
    /// `fastembed_cache_dir` unset, `FASTEMBED_CACHE_DIR` unset, and
    /// `dirs::cache_dir()` unavailable, `resolve_cache_dir` ends at the
    /// **cwd-relative** `./.fastembed_cache` — inside the very directory we
    /// decided not to trust. So the safe path is set for every untrusted
    /// config; the key's presence only decides whether to warn.
    #[test]
    fn an_untrusted_config_gets_a_safe_cache_dir_even_when_it_names_none() {
        let dir = TempDir::new("kb-mcp-untrusted-no-cache-key");
        std::fs::write(dir.path().join("kb-mcp.toml"), "kb_path = \"kb\"\n").unwrap();
        let roots = roots_for(None, None);

        let d = Config::discover_in(None, dir.path(), None, &roots).expect("discover ok");
        assert_eq!(d.trust, ConfigTrust::Untrusted);
        let used = d
            .config
            .fastembed_cache_dir
            .expect("an untrusted config always gets an explicit cache dir");
        assert!(
            used.is_absolute() && !used.starts_with(dir.path()),
            "the model directory must never resolve inside the untrusted directory: {}",
            used.display()
        );
    }

    /// The remedy an error names has to be one that works. When no OS cache
    /// directory can be determined, the error tells the user to set
    /// `FASTEMBED_CACHE_DIR` — so following that advice must actually get them
    /// past it, rather than hitting the same abort because the check ran before
    /// the variable was ever consulted.
    #[test]
    fn the_cache_rule_honours_an_existing_env_override_before_demanding_a_cache_dir() {
        let dir = TempDir::new("kb-mcp-untrusted-cache-env");
        std::fs::write(dir.path().join("kb-mcp.toml"), planted_toml("kb")).unwrap();

        // No OS cache directory and no override: we cannot name a safe
        // location, so we stop instead of inventing a shared-temp path.
        let no_cache = TrustRoots::new_with_cache(None, None, false);
        let err = Config::discover_in(None, dir.path(), None, &no_cache)
            .expect_err("no safe cache directory can be named");
        assert!(
            err.to_string().contains("FASTEMBED_CACHE_DIR"),
            "the error must name the remedy: {err}"
        );

        // Following that advice works: with the variable set, the config's own
        // value cannot take effect anyway, so it is simply dropped.
        let with_env = TrustRoots::new_with_cache(None, None, true);
        let d = Config::discover_in(None, dir.path(), None, &with_env)
            .expect("setting FASTEMBED_CACHE_DIR must actually get past this");
        assert_eq!(d.trust, ConfigTrust::Untrusted);
        assert!(
            d.config.fastembed_cache_dir.is_none(),
            "the planted value is dropped; the environment already wins"
        );
    }

    /// `service::resolve_config_home` appends `kb-mcp/<service>` to whatever
    /// base it resolves — including the `KB_MCP_CONFIG_HOME` override. Taking
    /// the override itself as a trust root would make
    /// `KB_MCP_CONFIG_HOME=$HOME` trust every repository under the home
    /// directory, which disables this whole feature.
    #[test]
    fn the_trust_root_is_the_kb_mcp_subdirectory_of_each_base() {
        let base = TempDir::new("kb-mcp-trust-root-base");
        let roots =
            TrustRoots::from_bases(Some(base.path().to_path_buf()), None, None, None, false);

        let inside = base.path().join("kb-mcp").join("svc").join("kb-mcp.toml");
        assert_eq!(
            classify_trust(ConfigSource::Cwd, Some(&inside), &roots),
            ConfigTrust::Trusted,
            "where `service install` actually writes must stay trusted"
        );

        let sibling = base.path().join("some-repo").join("kb-mcp.toml");
        assert_eq!(
            classify_trust(ConfigSource::Cwd, Some(&sibling), &roots),
            ConfigTrust::Untrusted,
            "a repository merely sharing the base directory is not the operator's config"
        );
    }

    #[test]
    fn an_untrusted_config_cannot_put_the_listener_on_the_network() {
        let dir = TempDir::new("kb-mcp-untrusted-bind");
        std::fs::write(dir.path().join("kb-mcp.toml"), planted_toml("kb")).unwrap();
        let roots = roots_for(None, None);

        let d = Config::discover_in(None, dir.path(), None, &roots).expect("discover ok");
        let (cfg, trust) = (d.config, d.trust);
        assert_eq!(trust, ConfigTrust::Untrusted);

        let http = cfg
            .transport
            .as_ref()
            .and_then(|t| t.http.as_ref())
            .expect("[transport.http] survives; only its reach is cut");
        assert_eq!(
            http.bind.as_deref(),
            Some("127.0.0.1:3100"),
            "the port is kept, the interface is not"
        );
        assert!(
            http.allowed_hosts.is_none(),
            "dropping allowed_hosts restores the loopback-only Host check"
        );
        assert!(http.healthz_public.is_none());
        // `kind` is honoured: refusing to serve at all would kill a headless
        // daemon with no output on Windows.
        assert_eq!(
            cfg.transport.as_ref().and_then(|t| t.kind),
            Some(crate::transport::TransportKindConfig::Http)
        );
    }

    #[test]
    fn an_untrusted_config_pointing_at_home_is_refused_outright() {
        let dir = TempDir::new("kb-mcp-untrusted-kbpath");
        let home = TempDir::new("kb-mcp-untrusted-home");
        // An absolute kb_path at the home directory: the vector that hands the
        // user's files to an LLM client.
        let kb = home.path().to_string_lossy().replace('\\', "/");
        std::fs::write(dir.path().join("kb-mcp.toml"), planted_toml(&kb)).unwrap();
        let roots = roots_for(None, Some(home.path()));

        let err = Config::discover_in(None, dir.path(), None, &roots)
            .expect_err("kb_path is the one fatal rule");
        let msg = err.to_string();
        assert!(msg.contains("refusing kb_path"), "{msg}");
        assert!(
            msg.contains("--config"),
            "the error must name the way to accept it: {msg}"
        );
    }

    #[test]
    fn the_same_file_is_honoured_in_full_once_the_operator_names_it() {
        let dir = TempDir::new("kb-mcp-trusted-explicit");
        let home = TempDir::new("kb-mcp-trusted-home");
        let toml = dir.path().join("kb-mcp.toml");
        let kb = home.path().to_string_lossy().replace('\\', "/");
        std::fs::write(&toml, planted_toml(&kb)).unwrap();
        let roots = roots_for(None, Some(home.path()));

        let d = Config::discover_in(Some(&toml), dir.path(), None, &roots)
            .expect("--config is trusted, so nothing is refused");
        let (cfg, source, trust) = (d.config, d.source, d.trust);
        assert_eq!(source, ConfigSource::Explicit);
        assert_eq!(trust, ConfigTrust::Trusted);
        assert_eq!(
            cfg.fastembed_cache_dir.as_deref(),
            Some(dir.path().join("evil-cache").as_path()),
            "a named config keeps its cache dir"
        );
        assert_eq!(
            cfg.transport
                .as_ref()
                .and_then(|t| t.http.as_ref())
                .and_then(|h| h.bind.as_deref()),
            Some("0.0.0.0:3100"),
            "a named config keeps its bind -- that is the operator's decision"
        );
    }

    /// The installed daemons are the reason nothing here may key off cwd alone:
    /// all three backends set WorkingDirectory to the config home and start
    /// `serve` with no `--config`.
    #[test]
    fn a_config_in_the_daemons_config_home_keeps_all_of_its_privileges() {
        let config_home = TempDir::new("kb-mcp-daemon-home");
        let svc_dir = config_home.path().join("kb-mcp");
        std::fs::create_dir_all(&svc_dir).unwrap();
        let home = TempDir::new("kb-mcp-daemon-user-home");
        let kb = home.path().to_string_lossy().replace('\\', "/");
        std::fs::write(svc_dir.join("kb-mcp.toml"), planted_toml(&kb)).unwrap();
        let roots = roots_for(Some(config_home.path()), Some(home.path()));

        let d = Config::discover_in(None, &svc_dir, None, &roots)
            .expect("the daemon's own config must not be refused");
        let (cfg, source, trust) = (d.config, d.source, d.trust);
        assert_eq!(source, ConfigSource::Cwd);
        assert_eq!(trust, ConfigTrust::Trusted);
        assert_eq!(
            cfg.transport
                .as_ref()
                .and_then(|t| t.http.as_ref())
                .and_then(|h| h.bind.as_deref()),
            Some("0.0.0.0:3100"),
            "an operator-installed daemon keeps the bind it was configured with"
        );
    }

    /// A `.git` ancestor is the wider hole of the two: standing anywhere inside
    /// a hostile clone, up to 19 levels deep, loads its root config.
    #[test]
    fn the_git_root_tier_is_restricted_too() {
        let root = TempDir::new("kb-mcp-untrusted-gitroot");
        std::fs::create_dir_all(root.path().join(".git")).unwrap();
        std::fs::write(root.path().join("kb-mcp.toml"), planted_toml("kb")).unwrap();
        let nested = root.path().join("a").join("b").join("c");
        std::fs::create_dir_all(&nested).unwrap();
        let roots = roots_for(None, None);

        let d = Config::discover_in(None, &nested, None, &roots).expect("discover ok");
        let (cfg, source, trust) = (d.config, d.source, d.trust);
        assert_eq!(source, ConfigSource::GitRoot);
        assert_eq!(trust, ConfigTrust::Untrusted);
        assert_eq!(
            cfg.transport
                .as_ref()
                .and_then(|t| t.http.as_ref())
                .and_then(|h| h.bind.as_deref()),
            Some("127.0.0.1:3100")
        );
    }
}
