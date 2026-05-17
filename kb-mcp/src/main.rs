use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand};
use kb_mcp::config::Config;
use kb_mcp::embedder::{ModelChoice, RerankerChoice};
use std::path::{Path, PathBuf};

#[derive(Parser)]
#[command(name = "kb-mcp")]
#[command(
    about = "MCP server for semantic search over a knowledge base of Markdown and plain-text files",
    long_about = "MCP server for semantic search over a knowledge base of Markdown\n\
                  (and optionally plain-text, opt-in via [parsers].enabled) files.\n\
                  \n\
                  Any of the options below can be provided via `kb-mcp.toml`. The file\n\
                  is discovered in priority order: --config <PATH>, then ./kb-mcp.toml,\n\
                  then walking up to the .git ancestor, then alongside the binary.\n\
                  CLI arguments override the file. See README for details."
)]
struct Cli {
    /// Path to a `kb-mcp.toml` config file. Overrides discovery (CWD / .git
    /// ancestor / binary-side). Errors fast if the file does not exist.
    /// `~` is expanded to the home directory on all platforms.
    #[arg(long, value_name = "PATH", global = true)]
    config: Option<PathBuf>,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
enum SearchFormat {
    /// JSON array of hit records (default, machine-readable)
    Json,
    /// Concatenated text blocks (title / path#heading / content, separated by ---)
    Text,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
enum ValidateFormat {
    /// Human-readable (default). Uses ANSI color when stdout is a TTY.
    Text,
    /// JSON array for scripts / editors.
    Json,
    /// GitHub Actions annotations (`::error file=...::message`). Prints to
    /// stdout so `$GITHUB_OUTPUT` / `$GITHUB_STEP_SUMMARY` can capture it.
    Github,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
enum EvalFormat {
    /// Human-readable (default, ANSI color when TTY).
    Text,
    /// Structured JSON (single object).
    Json,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
enum CliSeedStrategy {
    AllChunks,
    Centroid,
}

impl From<CliSeedStrategy> for kb_mcp::graph::SeedStrategy {
    fn from(c: CliSeedStrategy) -> Self {
        match c {
            CliSeedStrategy::AllChunks => kb_mcp::graph::SeedStrategy::AllChunks,
            CliSeedStrategy::Centroid => kb_mcp::graph::SeedStrategy::Centroid,
        }
    }
}

#[derive(Subcommand)]
enum Commands {
    /// Start the MCP server (stdio or http transport)
    Serve {
        /// Path to the knowledge-base directory
        #[arg(long)]
        kb_path: Option<PathBuf>,
        /// Embedding model to use (must match the one that built the index)
        #[arg(long, value_enum)]
        model: Option<ModelChoice>,
        /// Optional cross-encoder reranker applied after RRF hybrid search.
        /// Default: none (disabled). Enabling requires a model download.
        #[arg(long, value_enum)]
        reranker: Option<RerankerChoice>,
        /// When reranker is enabled, apply it by default for every `search` call
        /// unless the tool invocation explicitly passes `rerank: false`.
        ///
        /// Default: `true` (when `--reranker` is set). Has no effect while
        /// `--reranker none`. Omit this flag to take the default or the
        /// `rerank_by_default` value from `kb-mcp.toml`.
        #[arg(long, value_parser = clap::value_parser!(bool))]
        rerank_by_default: Option<bool>,
        /// Disable the live-sync file watcher.
        /// Default: watcher is ON unless disabled here or via
        /// `[watch].enabled = false` in kb-mcp.toml.
        #[arg(long = "no-watch", default_value_t = false)]
        no_watch: bool,
        /// Override the watcher debounce in milliseconds. Default: 500ms.
        #[arg(long = "debounce-ms")]
        debounce_ms: Option<u64>,
        /// Transport: stdio (default, 1 client) or http
        /// (Streamable HTTP, many clients). HTTP bind defaults to 127.0.0.1:3100.
        #[arg(long, value_enum)]
        transport: Option<kb_mcp::transport::TransportKind>,
        /// Full HTTP bind address when `--transport http`.
        /// Example: `--bind 0.0.0.0:3100`. Wins over `--port`.
        #[arg(long)]
        bind: Option<std::net::SocketAddr>,
        /// HTTP port when `--transport http`, combined with
        /// `127.0.0.1`. Default: 3100. Ignored if `--bind` is given.
        #[arg(long)]
        port: Option<u16>,
    },
    /// Build or rebuild the search index
    Index {
        /// Path to the knowledge-base directory
        #[arg(long)]
        kb_path: Option<PathBuf>,
        /// Force full re-index. Required when switching `--model`.
        #[arg(long, default_value_t = false)]
        force: bool,
        /// Embedding model to use
        #[arg(long, value_enum)]
        model: Option<ModelChoice>,
        /// Suppress per-file progress output (only print start/end summary).
        /// Mutually exclusive with `--progress`. Useful for harness / CI runs
        /// where streaming output is buffered until exit.
        #[arg(long, default_value_t = false, conflicts_with = "progress")]
        quiet: bool,
        /// Show progress bar (TTY) or periodic flushed updates (non-TTY).
        /// Mutually exclusive with `--quiet`.
        #[arg(long, default_value_t = false)]
        progress: bool,
    },
    /// Show index status and statistics
    Status {
        /// Path to the knowledge-base directory
        #[arg(long)]
        kb_path: Option<PathBuf>,
    },
    /// Expand a connection graph starting from a document path.
    /// Useful for chained context discovery from the CLI.
    Graph {
        /// Start document path (relative to kb-path)
        #[arg(long)]
        start: String,
        /// Path to the knowledge-base directory
        #[arg(long)]
        kb_path: Option<PathBuf>,
        /// Embedding model (must match the index; defaults to config or built-in)
        #[arg(long, value_enum)]
        model: Option<ModelChoice>,
        /// BFS depth (default 2, clamped to max 3)
        #[arg(long, default_value_t = kb_mcp::graph::DEFAULT_DEPTH)]
        depth: u32,
        /// Max fan-out per node (default 5, clamped to max 20)
        #[arg(long = "fan-out", default_value_t = kb_mcp::graph::DEFAULT_FAN_OUT)]
        fan_out: u32,
        /// Minimum cosine similarity 0.0-1.0 (default 0.3)
        #[arg(long = "min-similarity", default_value_t = kb_mcp::graph::DEFAULT_MIN_SIMILARITY)]
        min_similarity: f32,
        /// Seed strategy (all_chunks | centroid)
        #[arg(long = "seed-strategy", value_enum, default_value_t = CliSeedStrategy::AllChunks)]
        seed_strategy: CliSeedStrategy,
        /// Filter by category
        #[arg(long)]
        category: Option<String>,
        /// Filter by topic
        #[arg(long)]
        topic: Option<String>,
        /// Comma-separated paths to exclude from the graph (in addition to
        /// the start path which is always excluded).
        #[arg(long, value_delimiter = ',')]
        exclude: Vec<String>,
        /// Collapse same-path hits so each document appears at most once.
        #[arg(long = "dedup-by-path", default_value_t = false)]
        dedup_by_path: bool,
        /// Output format
        #[arg(long, value_enum, default_value_t = SearchFormat::Json)]
        format: SearchFormat,
    },
    /// Validate frontmatter against a TOML schema file.
    ///
    /// Scans `.md` files under --kb-path and reports frontmatter violations.
    /// Exit code: 0 (no violations), 1 (violations), 2 (schema load error).
    /// If the schema file is missing, reports "no schema found" and exits 0.
    Validate {
        /// Path to the knowledge-base directory
        #[arg(long)]
        kb_path: Option<PathBuf>,
        /// Path to the schema TOML. Defaults to `<kb_path>/kb-mcp-schema.toml`.
        #[arg(long)]
        schema: Option<PathBuf>,
        /// Output format: text (human), json (machine), github (CI annotations)
        #[arg(long, value_enum, default_value_t = ValidateFormat::Text)]
        format: ValidateFormat,
        /// Disable ANSI color in text format (auto-disabled when stdout is not a TTY).
        #[arg(long = "no-color", default_value_t = false)]
        no_color: bool,
        /// Exit 1 at the first violation without scanning the rest.
        #[arg(long = "fail-fast", default_value_t = false)]
        fail_fast: bool,
        /// Treat unknown YAML keys in frontmatter as violations.
        /// MVP では Frontmatter が固定スキーマのため no-op (accept するが現状
        /// 動作に影響しない)。将来の `[options].allow_unknown_fields` 実装と
        /// 合わせて有効化する予定。CI スクリプト互換のため accept のみ。
        #[arg(long, default_value_t = false)]
        strict: bool,
    },
    /// One-shot search from the command line (no MCP transport).
    /// Useful for shell scripts / skill bins where invoking the binary
    /// directly is simpler than talking MCP stdio.
    Search(SearchCliArgs),
    /// Evaluate retrieval quality against a golden query set (optional, power-user feature).
    /// Reports recall@k / MRR / nDCG@k and diffs against the previous run.
    /// Details: docs/eval.md
    Eval(EvalCliArgs),
    /// Register kb-mcp as an OS-level user service (auto-start at login).
    /// Phase 1: Linux systemd-user / macOS LaunchAgent / Windows Task Scheduler.
    Service {
        #[command(subcommand)]
        action: ServiceSubcommand,
    },
}

#[derive(Subcommand)]
enum ServiceSubcommand {
    /// Install and register a kb-mcp service
    Install {
        #[arg(long, default_value = "kb-mcp", value_parser = kb_mcp::service::validate_service_name)]
        service_name: String,
        #[arg(long)]
        kb_path: Option<PathBuf>,
        #[arg(long, default_value = "127.0.0.1:3100")]
        bind: String,
        #[arg(long)]
        no_auto_start: bool,
        #[arg(long)]
        force: bool,
        #[arg(long = "i-know")]
        i_know_non_loopback: bool,
        /// (Windows-only) Also install the kb-mcp-tray.exe shell:startup
        /// shortcut so the tray monitor launches at the next logon.
        #[arg(long)]
        with_tray: bool,
    },
    /// Uninstall the kb-mcp service (use --purge --yes to also delete index DB and config)
    Uninstall {
        #[arg(default_value = "kb-mcp")]
        service_name: String,
        #[arg(long)]
        purge: bool,
        #[arg(long)]
        yes: bool,
    },
    /// Show service status (running / stopped / not-found, bind, kb_path)
    Status {
        #[arg(default_value = "kb-mcp")]
        service_name: String,
    },
    /// List all installed kb-mcp service instances
    List,
    /// (Windows-only) Install only the kb-mcp-tray.exe shell:startup shortcut
    /// without touching the daemon registration.
    TrayInstall {
        #[arg(long, default_value = "kb-mcp", value_parser = kb_mcp::service::validate_service_name)]
        service_name: String,
        #[arg(long)]
        force: bool,
    },
    /// (Windows-only) Remove only the kb-mcp-tray.exe shell:startup shortcut
    /// without touching the daemon registration.
    TrayUninstall {
        #[arg(long, default_value = "kb-mcp", value_parser = kb_mcp::service::validate_service_name)]
        service_name: String,
    },
}

/// Parse a `[0.0, 1.0]` f32 from CLI. NaN / Inf / out-of-range は reject。
/// CLI 入口で early reject することで、embedding model DL 前に user に
/// 明確な error message を返す (codex review 罠 2 cluster 防御)。
///
/// clap が値を再表示するため (`error: invalid value '1.5' for '...': <msg>`)
/// parser 側 message では値を再掲しない。冗長な message を避ける。
fn parse_unit_f32(s: &str) -> Result<f32, String> {
    let v: f32 = s.parse().map_err(|e| format!("not a valid f32: {e}"))?;
    if !v.is_finite() {
        return Err("must be finite".into());
    }
    if !(0.0..=1.0).contains(&v) {
        return Err("must be in [0.0, 1.0]".into());
    }
    Ok(v)
}

#[derive(Args, Debug)]
pub(crate) struct SearchCliArgs {
    /// Search query text (positional)
    pub(crate) query: String,
    /// Path to the knowledge-base directory
    #[arg(long)]
    pub(crate) kb_path: Option<PathBuf>,
    /// Embedding model (must match the index; defaults to config or built-in)
    #[arg(long, value_enum)]
    pub(crate) model: Option<ModelChoice>,
    /// Optional cross-encoder reranker. Adds 300-700ms but improves precision.
    #[arg(long, value_enum)]
    pub(crate) reranker: Option<RerankerChoice>,
    /// Max results to return
    #[arg(long, default_value_t = 5)]
    pub(crate) limit: u32,
    /// Filter by category (e.g. "deep-dive", "ai-news")
    #[arg(long)]
    pub(crate) category: Option<String>,
    /// Filter by topic (e.g. "mcp", "chromadb")
    #[arg(long)]
    pub(crate) topic: Option<String>,
    /// Output format: json (machine-readable) or text (LLM-friendly)
    #[arg(long, value_enum, default_value_t = SearchFormat::Json)]
    pub(crate) format: SearchFormat,
    /// Override quality filter threshold (0.0-1.0). Defaults to the
    /// `[quality_filter].threshold` in kb-mcp.toml (0.3 if unset).
    #[arg(long = "min-quality")]
    pub(crate) min_quality: Option<f32>,
    /// Disable the quality filter for this query (shorthand for
    /// `--min-quality 0.0`).
    #[arg(long = "include-low-quality", default_value_t = false)]
    pub(crate) include_low_quality: bool,
    /// path glob (`!`-prefix で除外)。複数指定可。例: `--path-glob "docs/**"`
    #[arg(long = "path-glob", value_delimiter = ',')]
    pub(crate) path_globs: Vec<String>,
    /// tags_any (OR)。複数指定可。例: `--tag-any rust,async`
    #[arg(long = "tag-any", value_delimiter = ',')]
    pub(crate) tags_any: Vec<String>,
    /// tags_all (AND)。複数指定可。
    #[arg(long = "tag-all", value_delimiter = ',')]
    pub(crate) tags_all: Vec<String>,
    /// date filter 下限 (YYYY-MM-DD or RFC3339, lex 比較)
    #[arg(long = "date-from")]
    pub(crate) date_from: Option<String>,
    /// date filter 上限 (両端含む)
    #[arg(long = "date-to")]
    pub(crate) date_to: Option<String>,
    /// rank-based low_confidence ratio (default: 1.5、0.0 で判定無効)
    #[arg(long = "min-confidence-ratio")]
    pub(crate) min_confidence_ratio: Option<f32>,
    /// Enable MMR re-ranking (overrides kb-mcp.toml [search.mmr].enabled).
    #[arg(long, value_parser = clap::value_parser!(bool))]
    pub(crate) mmr: Option<bool>,
    /// MMR lambda (relevance vs diversity tradeoff). 0.0..=1.0.
    #[arg(long, value_parser = parse_unit_f32, allow_hyphen_values = true)]
    pub(crate) mmr_lambda: Option<f32>,
    /// MMR same-document penalty. 0.0..=1.0.
    #[arg(long, value_parser = parse_unit_f32, allow_hyphen_values = true)]
    pub(crate) mmr_same_doc_penalty: Option<f32>,
    /// Enable Parent retriever (content expansion).
    #[arg(long, value_parser = clap::value_parser!(bool))]
    pub(crate) parent_retriever: Option<bool>,
}

#[derive(Args, Debug)]
pub(crate) struct EvalCliArgs {
    /// Path to the knowledge-base directory
    #[arg(long)]
    pub(crate) kb_path: Option<PathBuf>,
    /// Override golden file path. Default: <kb_path>/.kb-mcp-eval.yml or [eval].golden.
    #[arg(long)]
    pub(crate) golden: Option<PathBuf>,
    /// Embedding model (must match the index)
    #[arg(long, value_enum)]
    pub(crate) model: Option<ModelChoice>,
    /// Optional cross-encoder reranker for this run.
    #[arg(long, value_enum)]
    pub(crate) reranker: Option<RerankerChoice>,
    /// Comma-separated k list (default: [eval].k_values or 1,5,10)
    #[arg(long, value_delimiter = ',')]
    pub(crate) k: Option<Vec<usize>>,
    /// Max hits to fetch per query (default: max of k list)
    #[arg(long)]
    pub(crate) limit: Option<u32>,
    /// Output format
    #[arg(long, value_enum, default_value_t = EvalFormat::Text)]
    pub(crate) format: EvalFormat,
    /// Disable reading/writing the history file (one-off run, no diff)
    #[arg(long = "no-history", default_value_t = false)]
    pub(crate) no_history: bool,
    /// Skip diff display even if history exists
    #[arg(long = "no-diff", default_value_t = false)]
    pub(crate) no_diff: bool,
    /// Disable ANSI color (auto-disabled when stdout is not a TTY)
    #[arg(long = "no-color", default_value_t = false)]
    pub(crate) no_color: bool,
    /// Exit with code 1 if any aggregate metric (recall@k / MRR /
    /// nDCG@k) regressed from the previous compatible run by more
    /// than `regression_threshold` (default 0.05). Compatible =
    /// same fingerprint (model / reranker / k_values / golden_hash).
    /// History is still written before exit. Useful for CI gates.
    #[arg(long = "fail-on-regression", default_value_t = false)]
    pub(crate) fail_on_regression: bool,
    /// Enable MMR re-ranking (overrides kb-mcp.toml [search.mmr].enabled).
    #[arg(long, value_parser = clap::value_parser!(bool))]
    pub(crate) mmr: Option<bool>,
    /// MMR lambda (relevance vs diversity tradeoff). 0.0..=1.0.
    #[arg(long, value_parser = parse_unit_f32, allow_hyphen_values = true)]
    pub(crate) mmr_lambda: Option<f32>,
    /// MMR same-document penalty. 0.0..=1.0.
    #[arg(long, value_parser = parse_unit_f32, allow_hyphen_values = true)]
    pub(crate) mmr_same_doc_penalty: Option<f32>,
    /// Enable Parent retriever (content expansion).
    #[arg(long, value_parser = clap::value_parser!(bool))]
    pub(crate) parent_retriever: Option<bool>,
}

impl From<&SearchCliArgs> for kb_mcp::config::SearchOverrides {
    fn from(a: &SearchCliArgs) -> Self {
        Self {
            mmr: a.mmr,
            mmr_lambda: a.mmr_lambda,
            mmr_same_doc_penalty: a.mmr_same_doc_penalty,
            parent_retriever: a.parent_retriever,
        }
    }
}

impl From<&EvalCliArgs> for kb_mcp::config::SearchOverrides {
    fn from(a: &EvalCliArgs) -> Self {
        Self {
            mmr: a.mmr,
            mmr_lambda: a.mmr_lambda,
            mmr_same_doc_penalty: a.mmr_same_doc_penalty,
            parent_retriever: a.parent_retriever,
        }
    }
}

/// `kb_path` が指定されていなければエラー。(CLI / config どちらからも無い場合)
fn require_kb_path(cli_value: Option<PathBuf>, config_default: Option<PathBuf>) -> Result<PathBuf> {
    cli_value
        .or(config_default)
        .context("--kb-path is required (pass on the command line or set `kb_path` in kb-mcp.toml)")
}

fn main() -> anyhow::Result<()> {
    // tracing-subscriber は config 探索ログを出すために main の最初で初期化。
    // RUST_LOG 未設定時は info 以上を出す既定値。
    // try_init を使うのは embedded / 別 init とのレース時に panic させないため
    // (現状は他に init 箇所はないが、防御的選択)。
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .try_init();

    // CLI parse を先に行い、--config の値を discover に渡す。
    // discover が FASTEMBED_CACHE_DIR を解決するので embedder 初期化より前に来る順序は維持。
    let cli = Cli::parse();

    // codex P2 round 3 on PR #56: `service install/uninstall/status/list` do not
    // consume `Config` — they read their own `kb-mcp.toml` from `config_home`
    // (or none at all for `list`). Dispatch them BEFORE `Config::discover` so
    // users with a malformed `kb-mcp.toml` in CWD can still recover by
    // running `kb-mcp service uninstall` (otherwise discover would error out
    // before reaching the service arm).
    if let Commands::Service { action } = cli.command {
        return run_service(action);
    }

    let (cfg, source) = Config::discover(cli.config.as_deref())?;
    tracing::info!(
        target: "kb_mcp::config",
        source = ?source,
        "loaded config"
    );
    cfg.apply_cache_dir_env();

    match cli.command {
        Commands::Serve {
            kb_path,
            model,
            reranker,
            rerank_by_default,
            no_watch,
            debounce_ms,
            transport: cli_transport,
            bind,
            port,
        } => {
            let kb_path = require_kb_path(kb_path, cfg.kb_path.clone())?;
            let model = model.or(cfg.model).unwrap_or_default();
            let reranker = reranker.or(cfg.reranker).unwrap_or_default();
            // rerank_by_default の CLI 既定値は `true` (reranker 有効時のみ意味を持つ)。
            let rerank_by_default = rerank_by_default.or(cfg.rerank_by_default).unwrap_or(true);

            let exclude_headings = cfg.exclude_headings.clone();
            let exclude_dirs = cfg.resolve_exclude_dirs();
            let quality_threshold = cfg
                .quality_filter
                .clone()
                .unwrap_or_default()
                .effective_threshold();
            let best_practice_templates =
                cfg.best_practice.clone().unwrap_or_default().path_templates;
            let parser_registry = cfg.build_parser_registry()?;

            // watch config の解決
            // 優先順位: --no-watch CLI > [watch].enabled config > default(true)
            let mut watch_config = cfg.watch.clone().unwrap_or_default();
            if no_watch {
                watch_config.enabled = false;
            }
            if let Some(d) = debounce_ms {
                watch_config.debounce_ms = d;
            }

            // transport の解決: CLI > config > default (stdio)
            let resolved_transport = kb_mcp::transport::Transport::resolve(
                cli_transport,
                bind,
                port,
                cfg.transport.as_ref(),
            )?;

            // [search].min_confidence_ratio: 省略時 1.5、0.0 は判定無効。
            // CLI override (`--min-confidence-ratio`) は Task 8 で追加。
            let min_confidence_ratio = cfg
                .search
                .as_ref()
                .and_then(|s| s.min_confidence_ratio)
                .unwrap_or(1.5);

            // [search] セクション全体のスナップショット。MMR / parent_retriever
            // の effective config は MCP `search` ツールの per-call で resolve するため、
            // serve 起動時にここから clone して KbServer に保持する。
            let search_config = cfg.search.clone().unwrap_or_default();

            // evaluator 指摘 High #2: `--bind` / `--port` が指定されているのに
            // 実効 transport が Stdio なら silent ignore は footgun なので reject。
            if matches!(resolved_transport, kb_mcp::transport::Transport::Stdio)
                && (bind.is_some() || port.is_some())
            {
                anyhow::bail!(
                    "--bind / --port require `--transport http` (or `[transport].kind = \"http\"` in kb-mcp.toml); \
                     currently resolved to stdio which does not listen on any port."
                );
            }

            let rt = tokio::runtime::Runtime::new()?;
            rt.block_on(async {
                kb_mcp::server::run_server(
                    &kb_path,
                    model,
                    reranker,
                    rerank_by_default,
                    exclude_headings,
                    exclude_dirs,
                    quality_threshold,
                    best_practice_templates,
                    parser_registry,
                    watch_config,
                    resolved_transport,
                    min_confidence_ratio,
                    search_config,
                    source,
                )
                .await
            })?;
        }
        Commands::Index {
            kb_path,
            force,
            model,
            quiet,
            progress,
        } => {
            let kb_path = require_kb_path(kb_path, cfg.kb_path.clone())?;
            let model = model.or(cfg.model).unwrap_or_default();

            let db_path = kb_mcp::resolve_db_path(&kb_path);
            let db = kb_mcp::db::Database::open(&db_path.to_string_lossy())?;
            // モデル DL (BGE-M3 なら ~2.3 GB) の前に meta 整合性を先に確認する。
            // そうしないと不整合時にユーザが不要な DL を待たされる。
            let dim = model.dimension() as u32;
            if !force {
                db.verify_embedding_meta(model.model_id(), dim)?;
            }
            let mut embedder = kb_mcp::embedder::Embedder::with_model(model)?;
            if force {
                db.reset_for_model(embedder.model_id(), dim)?;
            }
            let registry = cfg.build_parser_registry()?;
            eprintln!("Indexing {}...", kb_path.display());
            let exclude_dirs = cfg.resolve_exclude_dirs();
            let progress_reporter =
                kb_mcp::indexer::progress::ProgressReporter::from_cli_flags(quiet, progress);
            let result = kb_mcp::indexer::rebuild_index(
                &db,
                &mut embedder,
                &kb_path,
                force,
                cfg.exclude_headings.as_deref(),
                &exclude_dirs,
                &registry,
                progress_reporter,
            )?;
            eprintln!(
                "Done in {}ms: {} docs ({} updated, {} renamed, {} deleted), {} chunks",
                result.duration_ms,
                result.total_documents,
                result.updated,
                result.renamed,
                result.deleted,
                result.total_chunks
            );
        }
        Commands::Status { kb_path } => {
            let kb_path = require_kb_path(kb_path, cfg.kb_path.clone())?;

            let db_path = kb_mcp::resolve_db_path(&kb_path);
            if !db_path.exists() {
                eprintln!(
                    "No index found. Run `kb-mcp index --kb-path {}` first.",
                    kb_path.display()
                );
                return Ok(());
            }
            let db = kb_mcp::db::Database::open(&db_path.to_string_lossy())?;
            let total_docs = db.document_count()?;
            let total_chunks = db.chunk_count()?;
            let tags_failures = db.tags_parse_failure_count();
            eprintln!("Documents: {total_docs}");
            eprintln!("Chunks: {total_chunks}");
            eprintln!("Tags parse failures: {tags_failures}");
            // Quality filter: 設定済みの threshold で filter される件数を表示
            let qf = cfg.quality_filter.clone().unwrap_or_default();
            let threshold = qf.effective_threshold();
            if threshold > 0.0 {
                let (above, below) = db.chunk_count_by_quality(threshold)?;
                eprintln!(
                    "Quality filter (threshold={threshold}): {above} passing, {below} below threshold"
                );
            }
        }
        Commands::Search(args) => {
            // Build overrides BEFORE destructuring `args` so that the `&args`
            // borrow is short-lived (we still need owned fields below).
            let overrides: kb_mcp::config::SearchOverrides = (&args).into();

            let SearchCliArgs {
                query,
                kb_path,
                model,
                reranker,
                limit,
                category,
                topic,
                format,
                min_quality,
                include_low_quality,
                path_globs,
                tags_any,
                tags_all,
                date_from,
                date_to,
                min_confidence_ratio,
                // MMR / parent-retriever flags are wired through `overrides` above.
                mmr: _,
                mmr_lambda: _,
                mmr_same_doc_penalty: _,
                parent_retriever: _,
            } = args;

            let kb_path = require_kb_path(kb_path, cfg.kb_path.clone())?;
            let model = model.or(cfg.model).unwrap_or_default();
            let reranker_choice = reranker.or(cfg.reranker).unwrap_or_default();

            let db_path = kb_mcp::resolve_db_path(&kb_path);
            let db = kb_mcp::db::Database::open(&db_path.to_string_lossy())?;
            let dim = model.dimension() as u32;
            db.verify_embedding_meta(model.model_id(), dim)?;

            let mut embedder = kb_mcp::embedder::Embedder::with_model(model)?;
            let query_embedding = embedder.embed_single(&query)?;

            let server_default = cfg
                .quality_filter
                .clone()
                .unwrap_or_default()
                .effective_threshold();
            let effective_min_quality = kb_mcp::quality::resolve_effective_threshold(
                include_low_quality,
                min_quality,
                server_default,
            );

            // path_globs を compile (空 Vec は filter 無効、`[]` 入力をエラーにしないのは CLI 仕様)
            let cpg = if path_globs.is_empty() {
                None
            } else {
                Some(kb_mcp::server::compile_path_globs(&path_globs)?)
            };

            let filters = kb_mcp::db::SearchFilters {
                category: category.as_deref(),
                topic: topic.as_deref(),
                min_quality: effective_min_quality,
                path_globs: cpg.as_ref(),
                tags_any: &tags_any,
                tags_all: &tags_all,
                date_from: date_from.as_deref(),
                date_to: date_to.as_deref(),
            };

            // Both CLI and MCP go through the shared MMR-aware pipeline so the
            // `--mmr` / `--mmr-lambda` / `--mmr-same-doc-penalty` /
            // `--parent-retriever` flags actually take effect for CLI callers.
            let toml_search = cfg.search.clone().unwrap_or_default();
            let mut reranker_obj: Option<kb_mcp::embedder::Reranker> =
                if reranker_choice.is_enabled() {
                    kb_mcp::embedder::Reranker::try_new(reranker_choice)?
                } else {
                    None
                };
            let pipeline = kb_mcp::server::run_search_pipeline(
                &db,
                reranker_obj.as_mut(),
                &query,
                &query_embedding,
                limit,
                &filters,
                &overrides,
                &toml_search,
            )?;

            // chunk_id を維持したまま SearchHit に変換 (Parent retriever 用)。
            let hits_with_id: Vec<(i64, kb_mcp::db::SearchHit)> = pipeline
                .into_iter()
                .map(|(id, sr)| (id, sr.into()))
                .collect();

            // Parent retriever 段。enabled = false なら chunk_id を剥がすだけで
            // content / expanded_from は触らない (= v0.6.1 と bit-exact 互換)。
            let resolved = overrides.resolve(&toml_search);
            let parent_params = kb_mcp::parent::ParentRetrieverParams {
                whole_doc_threshold_tokens: resolved.parent_whole_doc_threshold_tokens,
                max_expanded_tokens: resolved.parent_max_expanded_tokens,
            };
            let mut hits: Vec<kb_mcp::db::SearchHit> = kb_mcp::parent::apply_parent_retriever(
                hits_with_id,
                &db,
                resolved.parent_retriever_enabled,
                parent_params,
            );
            // match_spans は Parent retriever 拡張後の content に対して計算する
            // (`expand_parent` は defensive に None クリアするので必ず再計算が要る)。
            for h in &mut hits {
                h.match_spans = kb_mcp::server::compute_match_spans(&query, &h.content);
            }

            let effective_ratio = min_confidence_ratio
                .or(toml_search.min_confidence_ratio)
                .unwrap_or(1.5);

            print_search_results(
                hits,
                effective_ratio,
                &path_globs,
                &tags_any,
                &tags_all,
                date_from.as_deref(),
                date_to.as_deref(),
                category.as_deref(),
                topic.as_deref(),
                min_confidence_ratio,
                format,
            );
        }
        Commands::Graph {
            start,
            kb_path,
            model,
            depth,
            fan_out,
            min_similarity,
            seed_strategy,
            category,
            topic,
            exclude,
            dedup_by_path,
            format,
        } => {
            let kb_path = require_kb_path(kb_path, cfg.kb_path.clone())?;
            let model = model.or(cfg.model).unwrap_or_default();

            let db_path = kb_mcp::resolve_db_path(&kb_path);
            // Status と同じく、DB がまだ作られていない状態を親切なエラーで弾く。
            if !db_path.exists() {
                anyhow::bail!(
                    "No index found at {}. Run `kb-mcp index --kb-path {}` first.",
                    db_path.display(),
                    kb_path.display()
                );
            }
            let db = kb_mcp::db::Database::open(&db_path.to_string_lossy())?;
            db.verify_embedding_meta(model.model_id(), model.dimension() as u32)?;

            let opts = kb_mcp::graph::GraphOptions {
                depth: depth.min(kb_mcp::graph::MAX_DEPTH),
                fan_out: fan_out.min(kb_mcp::graph::MAX_FAN_OUT),
                min_similarity: min_similarity.clamp(0.0, 1.0),
                seed_strategy: seed_strategy.into(),
                category,
                topic,
                exclude_paths: exclude,
                dedup_by_path,
                min_quality: cfg
                    .quality_filter
                    .clone()
                    .unwrap_or_default()
                    .effective_threshold(),
            };
            let g = kb_mcp::graph::build_connection_graph(&db, &start, &opts)?;
            print_graph(g, format);
        }
        Commands::Validate {
            kb_path,
            schema,
            format,
            no_color,
            fail_fast,
            strict: _strict, // MVP では no-op (schema.rs 側の拡張待ち)
        } => {
            let kb_path = require_kb_path(kb_path, cfg.kb_path.clone())?;
            // canonicalize は使わない: walkdir は相対パスでも動作し、strip_prefix
            // も同形のパスで一致する。Windows の UNC (\\?\) prefix 漏れを避ける。
            let schema_path = schema.unwrap_or_else(|| kb_path.join("kb-mcp-schema.toml"));
            let exclude_dirs = cfg.resolve_exclude_dirs();
            let exit = run_validate(
                &kb_path,
                &schema_path,
                format,
                no_color,
                fail_fast,
                &exclude_dirs,
            )?;
            std::process::exit(exit);
        }
        Commands::Eval(args) => {
            // Build overrides BEFORE destructuring `args` so the `&args` borrow
            // is short-lived (we still need owned fields below).
            let overrides: kb_mcp::config::SearchOverrides = (&args).into();

            let EvalCliArgs {
                kb_path,
                golden,
                model,
                reranker,
                k,
                limit,
                format,
                no_history,
                no_diff,
                no_color,
                fail_on_regression,
                // MMR / parent-retriever flags are wired through `overrides` above.
                mmr: _,
                mmr_lambda: _,
                mmr_same_doc_penalty: _,
                parent_retriever: _,
            } = args;

            let kb_path = require_kb_path(kb_path, cfg.kb_path.clone())?;
            let model_choice = model.or(cfg.model).unwrap_or_default();
            let reranker_choice = reranker.or(cfg.reranker).unwrap_or_default();

            let eval_cfg = cfg.eval.clone().unwrap_or_default();
            let golden_path = golden
                .or(eval_cfg.golden.clone())
                .unwrap_or_else(|| kb_path.join(".kb-mcp-eval.yml"));
            let k_values = k
                .or(eval_cfg.k_values.clone())
                .unwrap_or_else(|| vec![1, 5, 10]);
            let limit_val = limit.unwrap_or_else(|| *k_values.iter().max().unwrap_or(&10) as u32);
            let history_size = eval_cfg.history_size.unwrap_or(10);
            let regression_threshold = eval_cfg.regression_threshold.unwrap_or(0.05);

            let opts = kb_mcp::eval::RunOpts {
                kb_path: kb_path.clone(),
                golden_path,
                model_choice,
                reranker_choice,
                k_values,
                limit: limit_val,
                write_history: !no_history,
                history_size,
                regression_threshold,
                overrides,
                search_config: cfg.search.clone().unwrap_or_default(),
            };

            let run = kb_mcp::eval::run(&opts)?;

            let history_path = kb_mcp::eval::default_history_path(&kb_path);
            let history = if no_history {
                kb_mcp::eval::History::default()
            } else {
                kb_mcp::eval::History::load(&history_path)?
            };
            // Clone the previous run so the `history` binding can be moved later
            // to push the new run. `EvalRun: Clone`, so this is cheap enough.
            let previous = if no_diff {
                None
            } else {
                history.previous().cloned()
            };

            match format {
                EvalFormat::Text => {
                    use std::io::IsTerminal;
                    let tty = std::io::stdout().is_terminal() && !no_color;
                    let out = kb_mcp::eval::format_text(
                        &run,
                        previous.as_ref(),
                        tty,
                        regression_threshold,
                    );
                    print!("{}", out);
                }
                EvalFormat::Json => {
                    let v = kb_mcp::eval::format_json(&run, previous.as_ref());
                    println!("{}", serde_json::to_string_pretty(&v)?);
                }
            }

            // F-40: --fail-on-regression は履歴保存より後に判定したいが、
            // run を h.push_front で move する前に regression check を済ませる
            // 必要がある。以下の手順:
            //   1. now ⇄ previous で regression 判定 (run / previous は両方
            //      ここでは borrow できる)
            //   2. 履歴を save (no_history でなければ)
            //   3. 判定結果に応じて exit
            // previous_compatible で fingerprint 不一致 (golden_hash 変更等)
            // のときは判定対象外 = false (CI を fail させない)。
            let regression_detected = if fail_on_regression {
                let prev_compat = if no_history {
                    None
                } else {
                    history.previous_compatible(&run)
                };
                prev_compat
                    .is_some_and(|p| kb_mcp::eval::is_regression(&run, p, regression_threshold))
            } else {
                false
            };

            if !no_history {
                let mut h = history;
                h.push_front(run, history_size);
                h.save(&history_path)?;
            }

            if regression_detected {
                eprintln!(
                    "kb-mcp eval: retrieval-quality regression detected (delta > {regression_threshold:.3} \
                     on at least one of recall@k / MRR / nDCG@k). Exiting with code 1 because \
                     --fail-on-regression was set."
                );
                std::process::exit(1);
            }
        }
        Commands::Service { .. } => {
            // Dispatched at the top of main() before Config::discover
            // (codex P2 round 3 on PR #56). Unreachable here.
            unreachable!("Commands::Service dispatched before Config::discover");
        }
    }

    Ok(())
}

/// Service subcommand dispatcher. Called from `main()` BEFORE
/// `Config::discover` so users with a malformed `kb-mcp.toml` in CWD can
/// still uninstall / inspect existing service registrations.
fn run_service(action: ServiceSubcommand) -> anyhow::Result<()> {
    match action {
        ServiceSubcommand::Install {
            service_name,
            kb_path,
            bind,
            no_auto_start,
            force,
            i_know_non_loopback,
            with_tray,
        } => {
            kb_mcp::service::install::run(kb_mcp::service::install::InstallParams {
                service_name,
                kb_path,
                bind,
                auto_start: !no_auto_start,
                force,
                i_know_non_loopback,
                with_tray,
            })?;
        }
        ServiceSubcommand::Uninstall {
            service_name,
            purge,
            yes,
        } => {
            kb_mcp::service::uninstall::run(kb_mcp::service::uninstall::UninstallParams {
                service_name,
                purge,
                yes,
            })?;
        }
        ServiceSubcommand::Status { service_name } => {
            let text = kb_mcp::service::status::run_status(&service_name)?;
            eprintln!("{}", text);
        }
        ServiceSubcommand::List => {
            let text = kb_mcp::service::status::run_list()?;
            eprintln!("{}", text);
        }
        ServiceSubcommand::TrayInstall {
            service_name,
            force,
        } => {
            kb_mcp::service::install::run_tray_install(&service_name, force)?;
        }
        ServiceSubcommand::TrayUninstall { service_name } => {
            kb_mcp::service::uninstall::run_tray_uninstall(&service_name)?;
        }
    }
    Ok(())
}

/// validate サブコマンド本体。exit code (0/1/2) を返す。
fn run_validate(
    kb_path: &Path,
    schema_path: &Path,
    format: ValidateFormat,
    no_color: bool,
    fail_fast: bool,
    exclude_dirs: &[String],
) -> Result<i32> {
    // スキーマ読み込み: 存在しなければ legacy 挙動 (exit 0)
    let schema_obj = match kb_mcp::schema::Schema::load_optional(schema_path) {
        Ok(Some(s)) => s,
        Ok(None) => {
            eprintln!(
                "kb-mcp validate: no schema found at {} (skipping)",
                schema_path.display()
            );
            return Ok(0);
        }
        Err(e) => {
            eprintln!("kb-mcp validate: schema load error: {e:#}");
            return Ok(2);
        }
    };

    // parser registry は `[parsers].enabled` 準拠で .md ファイル列挙に再利用
    // (.txt は frontmatter 概念なしで対象外)。
    let md_parser = kb_mcp::parser::MarkdownParser;
    let files = validate_collect_md_files(kb_path, exclude_dirs)?;

    let mut reports: Vec<FileReport> = Vec::new();
    let mut scanned: u32 = 0;
    let mut violated: u32 = 0;
    let mut has_violation = false;

    for path in files {
        scanned += 1;
        let raw = match std::fs::read_to_string(&path) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("warning: failed to read {}: {e}", path.display());
                continue;
            }
        };
        let rel = path
            .strip_prefix(kb_path)
            .unwrap_or(&path)
            .to_string_lossy()
            .replace('\\', "/");
        use kb_mcp::parser::Parser as ParserTrait;
        let parsed = md_parser.parse(&raw, &rel, &[]);
        let violations = kb_mcp::schema::validate(&parsed.frontmatter, &schema_obj);
        if !violations.is_empty() {
            violated += 1;
            has_violation = true;
            reports.push(FileReport {
                path: rel,
                violations,
            });
            if fail_fast {
                break;
            }
        }
    }

    print_validate_report(
        &reports,
        scanned,
        violated,
        format,
        no_color_for(no_color, format),
    );

    Ok(if has_violation { 1 } else { 0 })
}

/// validate 専用の `.md` ファイル列挙。除外ディレクトリスキップと
/// deterministic ordering は indexer の collect_source_files と同じ方針。
fn validate_collect_md_files(kb_path: &Path, exclude_dirs: &[String]) -> Result<Vec<PathBuf>> {
    use walkdir::WalkDir;
    let mut out = Vec::new();
    for entry in WalkDir::new(kb_path)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| {
            if !e.file_type().is_dir() {
                return true;
            }
            let name = e.file_name().to_string_lossy();
            // F-62: same hardcoded denylist as indexer::collect_source_files,
            // applied as a fail-safe alongside user exclude_dirs (union).
            if kb_mcp::indexer::is_hardcoded_excluded(name.as_ref()) {
                return false;
            }
            !exclude_dirs.iter().any(|d| d.as_str() == name.as_ref())
        })
    {
        let entry = entry.context("walkdir error during validate")?;
        if entry.file_type().is_file()
            && let Some(ext) = entry.path().extension()
            && ext.eq_ignore_ascii_case("md")
        {
            out.push(entry.into_path());
        }
    }
    out.sort();
    Ok(out)
}

struct FileReport {
    path: String,
    violations: Vec<kb_mcp::schema::Violation>,
}

/// text format の色付けを stdout の TTY 状態に応じて自動 on/off。
/// `--no-color` 指定または非 TTY なら false。
fn no_color_for(explicit: bool, format: ValidateFormat) -> bool {
    use std::io::IsTerminal;
    if explicit {
        return true;
    }
    if format != ValidateFormat::Text {
        return true;
    }
    !std::io::stdout().is_terminal()
}

fn print_validate_report(
    reports: &[FileReport],
    scanned: u32,
    violated: u32,
    format: ValidateFormat,
    no_color: bool,
) {
    match format {
        ValidateFormat::Json => {
            #[derive(serde::Serialize)]
            struct JsonReport<'a> {
                scanned: u32,
                violated: u32,
                ok: u32,
                files: &'a [FileReportJson<'a>],
            }
            #[derive(serde::Serialize)]
            struct FileReportJson<'a> {
                path: &'a str,
                violations: &'a [kb_mcp::schema::Violation],
            }
            let files: Vec<FileReportJson> = reports
                .iter()
                .map(|r| FileReportJson {
                    path: &r.path,
                    violations: &r.violations,
                })
                .collect();
            let ok = scanned.saturating_sub(violated);
            let out = JsonReport {
                scanned,
                violated,
                ok,
                files: &files,
            };
            println!(
                "{}",
                serde_json::to_string_pretty(&out).unwrap_or_else(|_| "{}".into())
            );
        }
        ValidateFormat::Github => {
            // `::error file=<path>::<message>` 形式。ファイル位置は frontmatter
            // の行数を特定できれば better だが MVP では先頭固定 (line=1)。
            for r in reports {
                for v in &r.violations {
                    let msg = v.message();
                    let msg = msg.replace('\n', " ");
                    println!("::error file={},line=1,title=frontmatter::{msg}", r.path);
                }
            }
        }
        ValidateFormat::Text => {
            let ok = scanned.saturating_sub(violated);
            if reports.is_empty() {
                println!("kb-mcp validate: {scanned} files OK");
                return;
            }
            let header = format!("kb-mcp validate — {violated} file(s) with violations ({ok} OK)");
            if no_color {
                println!("{header}");
            } else {
                println!("\x1b[1;31m{header}\x1b[0m");
            }
            for r in reports {
                println!();
                if no_color {
                    println!("{}", r.path);
                } else {
                    println!("\x1b[1;34m{}\x1b[0m", r.path);
                }
                for v in &r.violations {
                    println!("  {}", v.message());
                }
            }
        }
    }
}

fn print_graph(g: kb_mcp::graph::ConnectionGraph, format: SearchFormat) {
    match format {
        SearchFormat::Json => {
            println!(
                "{}",
                serde_json::to_string_pretty(&g).unwrap_or_else(|_| "{}".into())
            );
        }
        SearchFormat::Text => {
            println!("# Connection graph from: {}", g.start_path);
            println!(
                "nodes={} max_depth={} knn_queries={} duration_ms={}",
                g.stats.total_nodes,
                g.stats.max_depth_reached,
                g.stats.knn_queries,
                g.stats.duration_ms
            );
            for n in &g.nodes {
                println!();
                let parent = n
                    .parent_id
                    .map(|p| p.to_string())
                    .unwrap_or_else(|| "-".into());
                let heading = n.heading.as_deref().unwrap_or("");
                println!(
                    "[{:>3}] depth={} parent={} score={:.3}  {}#{}",
                    n.node_id, n.depth, parent, n.score, n.path, heading
                );
                if let Some(t) = &n.title {
                    println!("     title: {t}");
                }
                println!("     {}", n.snippet);
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn print_search_results(
    hits: Vec<kb_mcp::db::SearchHit>,
    min_confidence_ratio: f32,
    path_globs: &[String],
    tags_any: &[String],
    tags_all: &[String],
    date_from: Option<&str>,
    date_to: Option<&str>,
    category: Option<&str>,
    topic: Option<&str>,
    explicit_ratio: Option<f32>,
    format: SearchFormat,
) {
    let scores: Vec<f32> = hits.iter().map(|h| h.score).collect();
    let low_confidence = kb_mcp::server::compute_low_confidence(&scores, min_confidence_ratio);

    match format {
        SearchFormat::Json => {
            let echo = serde_json::json!({
                "category":              category,
                "topic":                 topic,
                "path_globs":            (if path_globs.is_empty() { None::<&[String]> } else { Some(path_globs) }),
                "tags_any":              (if tags_any.is_empty()   { None::<&[String]> } else { Some(tags_any)   }),
                "tags_all":              (if tags_all.is_empty()   { None::<&[String]> } else { Some(tags_all)   }),
                "date_from":             date_from,
                "date_to":               date_to,
                "min_confidence_ratio":  explicit_ratio,
            });
            // 値が None のキーは JSON 上 null になるので、null は剥がす。
            let echo = strip_null_keys(echo);

            let wrapper = serde_json::json!({
                "results":         hits,
                "low_confidence":  low_confidence,
                "filter_applied":  echo,
            });
            println!(
                "{}",
                serde_json::to_string_pretty(&wrapper).unwrap_or_else(|_| "{}".into())
            );
        }
        SearchFormat::Text => {
            if low_confidence {
                println!("[low_confidence: top1 / mean ratio < {min_confidence_ratio}]\n");
            }
            for (i, h) in hits.iter().enumerate() {
                if i > 0 {
                    println!("\n---\n");
                }
                let title = h.title.as_deref().unwrap_or("(no title)");
                let heading = h.heading.as_deref().unwrap_or("");
                println!("# {title}");
                if heading.is_empty() {
                    println!("{}", h.path);
                } else {
                    println!("{}#{heading}", h.path);
                }
                println!("score: {:.4}", h.score);
                if !h.tags.is_empty() {
                    println!("tags: {}", h.tags.join(", "));
                }
                if let Some(spans) = &h.match_spans
                    && !spans.is_empty()
                {
                    let snippets: Vec<String> = spans
                        .iter()
                        .take(3)
                        .filter_map(|s| h.content.get(s.start..s.end).map(|t| format!("\"{t}\"")))
                        .collect();
                    println!("match_spans: {}", snippets.join(", "));
                }
                println!();
                println!("{}", h.content);
            }
        }
    }
}

/// JSON object から null 値の key を再帰的に剥がす (filter_applied の non-default echo 用)。
fn strip_null_keys(value: serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(map) => {
            let cleaned: serde_json::Map<String, serde_json::Value> = map
                .into_iter()
                .filter(|(_, v)| !v.is_null())
                .map(|(k, v)| (k, strip_null_keys(v)))
                .collect();
            serde_json::Value::Object(cleaned)
        }
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Default-ish `SearchCliArgs` for tests that only care about the MMR /
    /// parent-retriever fields. The other fields use cheap zero-ish values.
    fn search_args_default() -> SearchCliArgs {
        SearchCliArgs {
            query: String::new(),
            kb_path: None,
            model: None,
            reranker: None,
            limit: 5,
            category: None,
            topic: None,
            format: SearchFormat::Json,
            min_quality: None,
            include_low_quality: false,
            path_globs: Vec::new(),
            tags_any: Vec::new(),
            tags_all: Vec::new(),
            date_from: None,
            date_to: None,
            min_confidence_ratio: None,
            mmr: None,
            mmr_lambda: None,
            mmr_same_doc_penalty: None,
            parent_retriever: None,
        }
    }

    fn eval_args_default() -> EvalCliArgs {
        EvalCliArgs {
            kb_path: None,
            golden: None,
            model: None,
            reranker: None,
            k: None,
            limit: None,
            format: EvalFormat::Text,
            no_history: false,
            no_diff: false,
            no_color: false,
            fail_on_regression: false,
            mmr: None,
            mmr_lambda: None,
            mmr_same_doc_penalty: None,
            parent_retriever: None,
        }
    }

    #[test]
    fn test_search_cli_args_into_overrides_all_none() {
        let args = search_args_default();
        let o: kb_mcp::config::SearchOverrides = (&args).into();
        assert_eq!(o.mmr, None);
        assert_eq!(o.mmr_lambda, None);
        assert_eq!(o.mmr_same_doc_penalty, None);
        assert_eq!(o.parent_retriever, None);
    }

    #[test]
    fn test_search_cli_args_into_overrides_populated() {
        let args = SearchCliArgs {
            mmr: Some(true),
            mmr_lambda: Some(0.5),
            mmr_same_doc_penalty: Some(0.2),
            parent_retriever: Some(true),
            ..search_args_default()
        };
        let o: kb_mcp::config::SearchOverrides = (&args).into();
        assert_eq!(o.mmr, Some(true));
        assert_eq!(o.mmr_lambda, Some(0.5));
        assert_eq!(o.mmr_same_doc_penalty, Some(0.2));
        assert_eq!(o.parent_retriever, Some(true));
    }

    #[test]
    fn test_eval_cli_args_into_overrides_all_none() {
        let args = eval_args_default();
        let o: kb_mcp::config::SearchOverrides = (&args).into();
        assert_eq!(o.mmr, None);
        assert_eq!(o.mmr_lambda, None);
        assert_eq!(o.mmr_same_doc_penalty, None);
        assert_eq!(o.parent_retriever, None);
    }

    #[test]
    fn test_eval_cli_args_into_overrides_populated() {
        let args = EvalCliArgs {
            mmr: Some(false),
            mmr_lambda: Some(0.7),
            mmr_same_doc_penalty: Some(0.1),
            parent_retriever: Some(false),
            ..eval_args_default()
        };
        let o: kb_mcp::config::SearchOverrides = (&args).into();
        assert_eq!(o.mmr, Some(false));
        assert_eq!(o.mmr_lambda, Some(0.7));
        assert_eq!(o.mmr_same_doc_penalty, Some(0.1));
        assert_eq!(o.parent_retriever, Some(false));
    }

    #[test]
    fn test_search_cli_clap_parses_mmr_flags() {
        // Smoke-test that clap actually wires the new flags (catch typos
        // in #[arg(long)] vs field names).
        let cli = Cli::try_parse_from([
            "kb-mcp",
            "search",
            "hello",
            "--mmr",
            "true",
            "--mmr-lambda",
            "0.4",
            "--mmr-same-doc-penalty",
            "0.25",
            "--parent-retriever",
            "true",
        ])
        .expect("clap should parse MMR flags on the search subcommand");
        match cli.command {
            Commands::Search(a) => {
                assert_eq!(a.mmr, Some(true));
                assert_eq!(a.mmr_lambda, Some(0.4));
                assert_eq!(a.mmr_same_doc_penalty, Some(0.25));
                assert_eq!(a.parent_retriever, Some(true));
            }
            _ => panic!("expected Search subcommand"),
        }
    }

    #[test]
    fn test_eval_cli_clap_parses_mmr_flags() {
        let cli = Cli::try_parse_from([
            "kb-mcp",
            "eval",
            "--mmr",
            "false",
            "--mmr-lambda",
            "0.6",
            "--parent-retriever",
            "false",
        ])
        .expect("clap should parse MMR flags on the eval subcommand");
        match cli.command {
            Commands::Eval(a) => {
                assert_eq!(a.mmr, Some(false));
                assert_eq!(a.mmr_lambda, Some(0.6));
                assert_eq!(a.parent_retriever, Some(false));
            }
            _ => panic!("expected Eval subcommand"),
        }
    }
}
