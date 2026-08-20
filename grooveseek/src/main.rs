use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand};
use grooveseek::config::Config;
use grooveseek::embedder::{ModelChoice, RerankerChoice};
use grooveseek::graph::SeedStrategy;
use std::path::{Path, PathBuf};

#[derive(Parser)]
#[command(name = "groove")]
// full-audit 2026-07-26 AU-59: CONTRIBUTING asks bug reporters to run
// `groove --version`, but the flag did not exist and clap answered with
// `error: unexpected argument '--version' found` (exit 2) — the very first
// command in the report template failed for everyone.
#[command(version)]
#[command(
    about = "MCP server for semantic search over a knowledge base of Markdown and plain-text files",
    // The reference link is built against this binary's own version tag, not
    // `main`: a release archive ships the binary and README.md but no `docs/`,
    // so the reader has to follow a URL, and `main` would hand an older binary
    // the options and defaults of a newer one. `v{CARGO_PKG_VERSION}` names a
    // tag that exists for every released build — the version is bumped by the
    // `chore(release)` commit the tag is then created on.
    //
    // One window where the link does not resolve, measured rather than
    // assumed: the file appears in the release that carries this text, so a
    // build from the unreleased tree before it still says v0.26.0, where
    // `docs/configuration.md` does not exist yet. That is a source build, and
    // a source build has `docs/` on disk.
    //
    // Written with `concat!` rather than `\`-continued lines: that escape eats
    // the newline *and* the next line's indentation, so the layout below is
    // what actually prints.
    long_about = concat!(
        "MCP server for semantic search over a knowledge base of Markdown\n",
        "(and optionally plain-text, opt-in via [parsers].enabled) files.\n",
        "\n",
        "Any of the options below can be provided via `groove.toml`. The file\n",
        "is discovered in priority order: --config <PATH>, then ./groove.toml,\n",
        "then walking up to the .git ancestor, then alongside the binary.\n",
        "CLI arguments override the file. Full reference for this version (a\n",
        "release archive ships the binary and README.md, not docs/):\n",
        "https://github.com/alphabet-h/grooveseek/blob/v",
        env!("CARGO_PKG_VERSION"),
        "/docs/configuration.md"
    )
)]
struct Cli {
    /// Path to a `groove.toml` config file. Overrides discovery (CWD / .git
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
enum DoctorFormat {
    /// Human-readable (default).
    Text,
    /// JSON object for scripts / CI.
    Json,
}

/// Output formats for `groove graph` (E-3).
///
/// Separate from [`SearchFormat`], which `graph` used to share: the drawings
/// only mean something for a graph, and putting them on the shared enum would
/// grow a `search --format dot` that has nothing to render.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
enum GraphFormat {
    /// The full graph as JSON (default, machine-readable).
    Json,
    /// One block per node, in walk order.
    Text,
    /// Graphviz DOT. Pipe it to `dot -Tsvg`, or open it in a DOT viewer.
    Dot,
    /// A standalone SVG drawing, with nothing to install to look at it.
    Svg,
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
enum TuneFormat {
    /// Human-readable tables (default, ANSI color when TTY).
    Text,
    /// Structured JSON (single object).
    Json,
}

/// `--seed-strategy`, built from [`SeedStrategy::SPELLINGS`] rather than from a
/// `ValueEnum` of its own.
///
/// A second enum here would be a second list of accepted spellings, and the two
/// would drift the first time a strategy was added to one of them — silently,
/// because each surface's test would still pass against its own list. Reading
/// the shared table means `--help` and the tool cannot disagree about what is
/// accepted, only about which spelling they advertise.
fn seed_strategy_parser() -> impl clap::builder::TypedValueParser<Value = SeedStrategy> {
    use clap::builder::{PossibleValue, PossibleValuesParser, TypedValueParser};
    PossibleValuesParser::new(
        SeedStrategy::SPELLINGS
            .iter()
            .map(|s| PossibleValue::new(s.text).hide(!s.advertised_by_cli)),
    )
    .map(|text| {
        SeedStrategy::parse(&text).expect("clap only offers spellings taken from the same table")
    })
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
        /// `rerank_by_default` value from `groove.toml`.
        #[arg(long, value_parser = clap::value_parser!(bool))]
        rerank_by_default: Option<bool>,
        /// Disable the live-sync file watcher.
        /// Default: watcher is ON unless disabled here or via
        /// `[watch].enabled = false` in groove.toml.
        #[arg(long = "no-watch", default_value_t = false)]
        no_watch: bool,
        /// Override the watcher debounce in milliseconds. Default: 500ms.
        #[arg(long = "debounce-ms")]
        debounce_ms: Option<u64>,
        /// Transport: stdio (default, 1 client) or http
        /// (Streamable HTTP, many clients). HTTP bind defaults to 127.0.0.1:3100.
        #[arg(long, value_enum)]
        transport: Option<grooveseek::transport::TransportKind>,
        /// Full HTTP bind address when `--transport http`.
        /// Example: `--bind 0.0.0.0:3100`. Wins over `--port`.
        #[arg(long)]
        bind: Option<std::net::SocketAddr>,
        /// HTTP port when `--transport http`, combined with
        /// `127.0.0.1`. Default: 3100. Ignored if `--bind` is given.
        #[arg(long)]
        port: Option<u16>,
        /// Acknowledge that `--bind` points at a non-loopback address.
        ///
        /// groove has no authentication, so the bind address is the only
        /// access control. Without this flag a non-loopback `--bind` is
        /// refused, matching `groove service install`.
        #[arg(long = "i-know", default_value_t = false)]
        i_know_non_loopback: bool,
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
        #[arg(long, default_value_t = grooveseek::graph::DEFAULT_DEPTH)]
        depth: u32,
        /// Max fan-out per node (default 5, clamped to max 20)
        #[arg(long = "fan-out", default_value_t = grooveseek::graph::DEFAULT_FAN_OUT)]
        fan_out: u32,
        /// Minimum cosine similarity 0.0-1.0 (default 0.3)
        #[arg(long = "min-similarity", default_value_t = grooveseek::graph::DEFAULT_MIN_SIMILARITY)]
        min_similarity: f32,
        /// Seed strategy. The MCP tool spells `all-chunks` as `all_chunks`,
        /// and both spellings work on either surface. (The accepted values
        /// are listed below by clap, from the same table both parsers read.)
        #[arg(
            long = "seed-strategy",
            value_parser = seed_strategy_parser(),
            default_value = SeedStrategy::DEFAULT_SPELLING,
        )]
        seed_strategy: SeedStrategy,
        /// Max nodes in the graph; also caps the number of KNN queries
        /// (default 100, clamped to max 2000)
        #[arg(long = "max-nodes", default_value_t = grooveseek::graph::DEFAULT_MAX_NODES)]
        max_nodes: u32,
        /// Max chunks of the start document used to seed the walk
        /// (default 32, clamped to 1..=1000)
        #[arg(long = "max-seed-chunks", default_value_t = grooveseek::graph::DEFAULT_MAX_SEED_CHUNKS)]
        max_seed_chunks: u32,
        /// Filter by category
        #[arg(long)]
        category: Option<String>,
        /// Filter by topic
        #[arg(long)]
        topic: Option<String>,
        /// Comma-separated paths to exclude from the graph (in addition to
        /// the start path which is always excluded).
        #[arg(long = "exclude-paths", value_delimiter = ',')]
        exclude_paths: Vec<String>,
        /// Collapse same-path hits so each document appears at most once.
        #[arg(long = "dedup-by-path", default_value_t = false)]
        dedup_by_path: bool,
        /// Output format
        #[arg(long, value_enum, default_value_t = GraphFormat::Json)]
        format: GraphFormat,
    },
    /// Check the index for inconsistencies and report them.
    ///
    /// Answers two kinds of question. Whether the three tables search reads —
    /// chunks, their embeddings, their full-text rows — still agree, since when
    /// they stop agreeing nothing errors and results are simply lost. And which
    /// indexed documents the MCP resource surface is holding back, and why.
    ///
    /// Reports only; it never modifies the index. Each finding names the
    /// command that fixes it.
    ///
    /// Exit code: 0 (nothing to report), 1 (findings), 2 (could not run).
    ///
    /// Note: like `search` and `eval`, this opens the database, and opening it
    /// applies any pending schema migration. It is read-only about its
    /// findings, not about the file.
    Doctor {
        /// Path to the knowledge-base directory
        #[arg(long)]
        kb_path: Option<PathBuf>,
        /// Output format: text (human), json (machine)
        #[arg(long, value_enum, default_value_t = DoctorFormat::Text)]
        format: DoctorFormat,
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
        /// Path to the schema TOML. Defaults to `<kb_path>/groove-schema.toml`.
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
    },
    /// One-shot search from the command line (no MCP transport).
    /// Useful for shell scripts / skill bins where invoking the binary
    /// directly is simpler than talking MCP stdio.
    Search(SearchCliArgs),
    /// Evaluate retrieval quality against a golden query set (optional, power-user feature).
    /// Reports recall@k / MRR / nDCG@k and diffs against the previous run.
    /// Details: docs/eval.md
    Eval(EvalCliArgs),
    /// Measure how much the RRF / bm25 fusion parameters move retrieval
    /// quality on THIS knowledge base (optional, power-user feature).
    ///
    /// Runs a grid search over the golden query set and reports a
    /// statistically guarded recommendation. Applies nothing automatically —
    /// the output is either a paste-ready `[search.fusion]` snippet or the
    /// conclusion that the built-in defaults should be kept.
    ///
    /// Exit code 2 means no golden query is sensitive to these parameters
    /// (every query has fewer than 2 FTS candidates), so the sweep was
    /// skipped. Details: docs/eval.md
    Tune(TuneCliArgs),
    /// Register groove as an OS-level user service (auto-start at login).
    /// Phase 1: Linux systemd-user / macOS LaunchAgent / Windows Task Scheduler.
    Service {
        #[command(subcommand)]
        action: ServiceSubcommand,
    },
}

#[derive(Subcommand)]
enum ServiceSubcommand {
    /// Install and register a groove service
    Install {
        /// Name this instance, so several knowledge bases can run side by side
        #[arg(long, default_value = "groove", value_parser = grooveseek::service::validate_service_name)]
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
        /// (Windows-only) Also install the groove-tray.exe shell:startup
        /// shortcut so the tray monitor launches at the next logon.
        #[arg(long)]
        with_tray: bool,
    },
    /// Uninstall the groove service (use --purge --yes to also delete index DB and config)
    Uninstall {
        /// Which installed instance to remove
        // Named the way `install` names it. This used to be positional, so the
        // same thing had two spellings — `install --service-name x` against
        // `uninstall x` — and `docs/stability.md` freezes subcommand
        // positionals as well as long flags, so 1.0 would have kept both
        // forever. The validator comes with it: a name `install` refuses can
        // never have been installed, so requiring it here rejects nothing that
        // used to work.
        #[arg(long, default_value = "groove", value_parser = grooveseek::service::validate_service_name)]
        service_name: String,
        #[arg(long)]
        purge: bool,
        #[arg(long)]
        yes: bool,
    },
    /// Show service status (running / stopped / not-found, bind, kb_path)
    Status {
        /// Which installed instance to report on
        // Was positional for the same reason as `uninstall`'s, and stopped
        // being so at the same time.
        #[arg(long, default_value = "groove", value_parser = grooveseek::service::validate_service_name)]
        service_name: String,
    },
    /// List all installed groove service instances
    List,
    /// (Windows-only) Install only the groove-tray.exe shell:startup shortcut
    /// without touching the daemon registration.
    TrayInstall {
        /// Which installed instance the shortcut should start
        #[arg(long, default_value = "groove", value_parser = grooveseek::service::validate_service_name)]
        service_name: String,
        #[arg(long)]
        force: bool,
    },
    /// (Windows-only) Remove only the groove-tray.exe shell:startup shortcut
    /// without touching the daemon registration.
    TrayUninstall {
        /// Which instance's shortcut to remove
        #[arg(long, default_value = "groove", value_parser = grooveseek::service::validate_service_name)]
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

/// Parse the `--min-confidence-ratio` value.
///
/// [`parse_unit_f32`] cannot serve here — it holds values to `[0.0, 1.0]`, and
/// this one defaults to 1.5. The rule that does apply lives in
/// [`check_confidence_ratio`], shared with the `[search].min_confidence_ratio`
/// key so that the flag and the file cannot come to mean different things; this
/// function only turns the text into a number and reports the reason clap will
/// print.
///
/// What rejecting at the entry buys, beyond the model download it saves: the
/// JSON echo could not report the problem afterwards. serde writes a non-finite
/// float as `null` and `strip_null_keys` drops the key, so the caller would see
/// a result with no `low_confidence` and no sign that the override was ignored.
///
/// The MCP parameter of the same name is deliberately not held to this — it
/// cannot refuse a value mid-conversation, so it substitutes: a non-finite ratio
/// is logged and replaced by the server's own, and a negative one is clamped to
/// `0.0`, which disables the check.
///
/// [`check_confidence_ratio`]: grooveseek::config::check_confidence_ratio
fn parse_confidence_ratio(s: &str) -> Result<f32, String> {
    let v: f32 = s.parse().map_err(|e| format!("not a valid f32: {e}"))?;
    grooveseek::config::check_confidence_ratio(v).map_err(str::to_owned)?;
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
    /// `[quality_filter].threshold` in groove.toml (0.3 if unset).
    #[arg(long = "min-quality")]
    pub(crate) min_quality: Option<f32>,
    /// Disable the quality filter for this query (shorthand for
    /// `--min-quality 0.0`).
    #[arg(long = "include-low-quality", default_value_t = false)]
    pub(crate) include_low_quality: bool,
    /// path glob (`!`-prefix で除外)。**繰り返して**指定する。
    /// 例: `--path-glob "docs/**" --path-glob "!docs/draft/**"`
    ///
    /// カンマでは区切らない。glob の構文自体がカンマを使う
    /// (`docs/{a,b}/**`) ので、区切ると値が壊れる — `docs/{a` は
    /// "unclosed alternate group" で落ちる。docs はもともとこのフラグを
    /// 「(repeatable)」としか書いていない。
    #[arg(long = "path-glob")]
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
    #[arg(
        long = "min-confidence-ratio",
        value_parser = parse_confidence_ratio,
        allow_hyphen_values = true
    )]
    pub(crate) min_confidence_ratio: Option<f32>,
    /// Enable MMR re-ranking (overrides groove.toml [search.mmr].enabled).
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
    /// Override golden file path. Default: <kb_path>/.groove-eval.yml or [eval].golden.
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
    /// than `regression_threshold` (default 0.05). Compatible = every
    /// fingerprint field equal: model, reranker, limit, k_values,
    /// golden_hash, metric_version, fts_query_version, and the mmr / parent_retriever /
    /// fusion / contextual settings. The indexed corpus is NOT part of
    /// that test, so two compatible runs can still have been measured
    /// over different documents; when they were, the failure message
    /// says so. History is still written before exit. Useful for CI gates.
    #[arg(long = "fail-on-regression", default_value_t = false)]
    pub(crate) fail_on_regression: bool,
    /// Enable MMR re-ranking (overrides groove.toml [search.mmr].enabled).
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
pub(crate) struct TuneCliArgs {
    /// Path to the knowledge-base directory
    #[arg(long)]
    pub(crate) kb_path: Option<PathBuf>,
    /// Override golden file path. Default: <kb_path>/.groove-eval.yml or [eval].golden.
    #[arg(long)]
    pub(crate) golden: Option<PathBuf>,
    /// Embedding model (must match the index)
    #[arg(long, value_enum)]
    pub(crate) model: Option<ModelChoice>,
    /// Comma-separated k list to report (default: [eval].k_values or 1,5,10).
    /// k=5 is always included: the adoption threshold is calibrated on nDCG@5.
    #[arg(long, value_delimiter = ',')]
    pub(crate) k: Option<Vec<usize>>,
    /// Max hits to fetch per query (default: max of k list)
    #[arg(long)]
    pub(crate) limit: Option<u32>,
    /// Output format
    #[arg(long, value_enum, default_value_t = TuneFormat::Text)]
    pub(crate) format: TuneFormat,
    /// Disable ANSI color (auto-disabled when stdout is not a TTY)
    #[arg(long = "no-color", default_value_t = false)]
    pub(crate) no_color: bool,
}

impl From<&SearchCliArgs> for grooveseek::config::SearchOverrides {
    fn from(a: &SearchCliArgs) -> Self {
        Self {
            mmr: a.mmr,
            mmr_lambda: a.mmr_lambda,
            mmr_same_doc_penalty: a.mmr_same_doc_penalty,
            parent_retriever: a.parent_retriever,
        }
    }
}

impl From<&EvalCliArgs> for grooveseek::config::SearchOverrides {
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
        .context("--kb-path is required (pass on the command line or set `kb_path` in groove.toml)")
}

/// `groove tune` の `--k` / `[eval].k_values` / `--limit` から実効値を導出する。
///
/// 優先順位は CLI > `[eval]` > ビルトイン `[1, 5, 10]`。主指標 nDCG@5 が
/// 落ちないよう `grooveseek::tune::normalize_k_values` を通してから、`--limit`
/// 未指定時の既定値を「k リストの最大値」として決める (eval と同じ規則)。
/// 明示 `--limit` も max(k) を下限として clamp する — limit < max(k) だと
/// fused ranking が limit 件に切り詰められ、recall@k / nDCG@k がラベルより
/// 浅い候補から計算されてしまう (codex P2 on PR #79)。
fn resolve_tune_k_and_limit(
    cli_k: Option<Vec<usize>>,
    cfg_k: Option<Vec<usize>>,
    cli_limit: Option<u32>,
) -> anyhow::Result<(Vec<usize>, u32)> {
    let raw = cli_k.or(cfg_k).unwrap_or_else(|| vec![1, 5, 10]);
    let k_values = grooveseek::tune::normalize_k_values(&raw);
    // clamp と上限検証 (MAX_TUNE_K 超は reject) は tune::effective_limit に集約
    // (codex P2 round 2/3/4 on PR #79)。--limit 未指定は 0 を渡せば max(k) に
    // 解決される。
    let limit = grooveseek::tune::effective_limit(&k_values, cli_limit.unwrap_or(0))?;
    Ok((k_values, limit))
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
    // consume `Config` — they read their own `groove.toml` from `config_home`
    // (or none at all for `list`). Dispatch them BEFORE `Config::discover` so
    // users with a malformed `groove.toml` in CWD can still recover by
    // running `groove service uninstall` (otherwise discover would error out
    // before reaching the service arm).
    if let Commands::Service { action } = cli.command {
        return run_service(action);
    }

    let discovered = match Config::discover(cli.config.as_deref()) {
        Ok(d) => d,
        // `doctor` promises exit 2 for "could not run", and a configuration it
        // cannot load means the inspection never happened. Carrying this out
        // with `?` would exit 1 — the code reserved for an inspection that
        // completed and found something (codex P2 round 3). Same shape as the
        // service arm above: which command was asked for changes what a
        // discovery failure means.
        Err(e) if matches!(cli.command, Commands::Doctor { .. }) => {
            eprintln!("groove doctor: could not load configuration: {e:#}");
            std::process::exit(2);
        }
        Err(e) => return Err(e),
    };
    let (cfg, source) = (discovered.config, discovered.source);
    // (BU-07) `source` の variant 名だけでは「どのファイルが勝ったのか」も
    // 「その中身をどこまで信用したのか」も分からない。両方を 1 行に載せる。
    tracing::info!(
        target: "grooveseek::config",
        source = ?source,
        path = %discovered
            .path
            .as_deref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "(built-in defaults)".into()),
        trust = ?discovered.trust,
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
            i_know_non_loopback,
        } => {
            let kb_path = require_kb_path(kb_path, cfg.kb_path.clone())?;
            let model = model.or(cfg.model).unwrap_or_default();
            let reranker = reranker.or(cfg.reranker).unwrap_or_default();
            // rerank_by_default の既定は 1 か所 (reranker 有効時のみ意味を持つ)。
            let rerank_by_default = rerank_by_default
                .or(cfg.rerank_by_default)
                .unwrap_or(grooveseek::server::RERANK_BY_DEFAULT);

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
            let resolved_transport = grooveseek::transport::Transport::resolve(
                cli_transport,
                bind,
                port,
                cfg.transport.as_ref(),
            )?;
            // (BU-01) `--bind <non-loopback>` は `--i-know` で追認させる。
            // toml 由来の bind は gate しない (理由は check_cli_bind_ack の doc)。
            grooveseek::transport::check_cli_bind_ack(
                &resolved_transport,
                bind,
                i_know_non_loopback,
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

            // feature-46: index 時と同じロジックで desired context mode を算出する。
            let context_mode_desired =
                if cfg.contextual.as_ref().map(|c| c.enabled).unwrap_or(false) {
                    grooveseek::db::ContextMode::Static
                } else {
                    grooveseek::db::ContextMode::Off
                };

            // evaluator 指摘 High #2: `--bind` / `--port` が指定されているのに
            // 実効 transport が Stdio なら silent ignore は footgun なので reject。
            if matches!(resolved_transport, grooveseek::transport::Transport::Stdio)
                && (bind.is_some() || port.is_some())
            {
                anyhow::bail!(
                    "--bind / --port require `--transport http` (or `[transport].kind = \"http\"` in groove.toml); \
                     currently resolved to stdio which does not listen on any port."
                );
            }

            let rt = tokio::runtime::Runtime::new()?;
            rt.block_on(async {
                grooveseek::server::run_server(
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
                    context_mode_desired,
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

            // `[parsers].enabled` の検証を **何より先** に置く (AU-06 codex P2)。
            //
            // 受け付けられない id が 1 つ入っているだけで実行は必ず失敗するが、
            // その判定は設定文字列だけで完結し、ファイルも DB も要らない。
            // 後ろに置くほど「失敗すると分かっている実行が、先に何かを壊す」
            // 窓が広がる:
            //
            // - `Database::open` より後 → DB / WAL を作り、schema 移行まで走る
            //   (`ensure_fts_context_column` は FTS を DROP + CREATE + repopulate する)
            // - `Embedder::with_model` より後 → 失敗すると分かっている実行のために
            //   モデルを DL する (BGE-M3 なら ~2.3 GB)
            // - `reset_for_model` より後 → **index を空にしてからエラー終了**
            //
            // `.xls` を取り下げた (AU-06) ことで、旧バージョンでは妥当だった設定の
            // まま upgrade した人がこの経路に入る。
            let registry = cfg.build_parser_registry()?;

            let db_path = grooveseek::resolve_db_path(&kb_path);
            let db = grooveseek::db::Database::open(&db_path.to_string_lossy())?;
            // モデル DL (BGE-M3 なら ~2.3 GB) の前に meta 整合性を先に確認する。
            // そうしないと不整合時にユーザが不要な DL を待たされる。
            let dim = model.dimension() as u32;
            if !force {
                db.verify_embedding_meta(model.model_id(), dim)?;
            }
            let mut embedder = grooveseek::embedder::Embedder::with_model(model)?;
            if force {
                db.reset_for_model(embedder.model_id(), dim)?;
            }
            eprintln!("Indexing {}...", kb_path.display());
            let exclude_dirs = cfg.resolve_exclude_dirs();
            let progress_reporter =
                grooveseek::indexer::progress::ProgressReporter::from_cli_flags(quiet, progress);
            let context_mode_desired =
                if cfg.contextual.as_ref().map(|c| c.enabled).unwrap_or(false) {
                    grooveseek::db::ContextMode::Static
                } else {
                    grooveseek::db::ContextMode::Off
                };
            let result = grooveseek::indexer::rebuild_index(
                &db,
                &mut embedder,
                &kb_path,
                force,
                cfg.exclude_headings.as_deref(),
                &exclude_dirs,
                &registry,
                progress_reporter,
                context_mode_desired,
            )?;
            eprintln!(
                "Done in {}ms: {} docs ({} updated, {} renamed, {} deleted, {} skipped), {} chunks",
                result.duration_ms,
                result.total_documents,
                result.updated,
                result.renamed,
                result.deleted,
                result.skipped,
                result.total_chunks
            );
        }
        Commands::Status { kb_path } => {
            let kb_path = require_kb_path(kb_path, cfg.kb_path.clone())?;

            let db_path = grooveseek::resolve_db_path(&kb_path);
            if !db_path.exists() {
                eprintln!(
                    "No index found. Run `groove index --kb-path {}` first.",
                    kb_path.display()
                );
                return Ok(());
            }
            let db = grooveseek::db::Database::open(&db_path.to_string_lossy())?;
            let total_docs = db.document_count()?;
            let total_chunks = db.chunk_count()?;
            let tags_failures = db.tags_parse_failure_count();
            // These lines are the answer to the question `status` was asked, so
            // they go to stdout — the "No index found" branch above stays on
            // stderr because it reports an inability to answer, not an answer.
            // ADR-0010 settles the channel that ADR-0008 left open.
            println!("Documents: {total_docs}");
            println!("Chunks: {total_chunks}");
            println!("Tags parse failures: {tags_failures}");
            let context_mode = db.read_context_mode()?.map(|m| m.as_str()).unwrap_or("off");
            println!("Context mode: {context_mode}");
            // Quality filter: 設定済みの threshold で filter される件数を表示
            let qf = cfg.quality_filter.clone().unwrap_or_default();
            let threshold = qf.effective_threshold();
            if threshold > 0.0 {
                let (above, below) = db.chunk_count_by_quality(threshold)?;
                println!(
                    "Quality filter (threshold={threshold}): {above} passing, {below} below threshold"
                );
            }
        }
        Commands::Search(args) => {
            // Build overrides BEFORE destructuring `args` so that the `&args`
            // borrow is short-lived (we still need owned fields below).
            let overrides: grooveseek::config::SearchOverrides = (&args).into();

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

            // AU-01 の clamp は `run_search_pipeline` 内 (= MCP / CLI search /
            // CLI eval が必ず通る唯一の choke point) に集約してある。ここでは
            // echo 用の値も実際に使われる値と揃えたいので同じ helper を通す。
            let limit = grooveseek::server::clamp_search_limit(limit);

            let kb_path = require_kb_path(kb_path, cfg.kb_path.clone())?;
            let model = model.or(cfg.model).unwrap_or_default();
            // `--reranker` given here is a choice about this query; a model that
            // only came from groove.toml is subject to `rerank_by_default`.
            let reranker_explicit = reranker.is_some();
            let reranker_choice = reranker.or(cfg.reranker).unwrap_or_default();
            let rerank_now =
                cli_should_rerank(reranker_explicit, reranker_choice, cfg.rerank_by_default);

            let db_path = grooveseek::resolve_db_path(&kb_path);
            let db = grooveseek::db::Database::open(&db_path.to_string_lossy())?;
            let dim = model.dimension() as u32;
            db.verify_embedding_meta(model.model_id(), dim)?;

            let mut embedder = grooveseek::embedder::Embedder::with_model(model)?;
            let query_embedding = embedder.embed_single(&query)?;

            let server_default = cfg
                .quality_filter
                .clone()
                .unwrap_or_default()
                .effective_threshold();
            let effective_min_quality = grooveseek::quality::resolve_effective_threshold(
                include_low_quality,
                min_quality,
                server_default,
            );

            // path_globs を compile (空 Vec は filter 無効、`[]` 入力をエラーにしないのは CLI 仕様)
            // 件数・要素長の上限は compile_path_globs の内側で検査される (AU-17)。
            let cpg = if path_globs.is_empty() {
                None
            } else {
                Some(grooveseek::server::compile_path_globs(&path_globs)?)
            };
            // AU-17: `tags_*` は glob と違って compile を通らないので、ここで検査する。
            grooveseek::server::validate_filter_list("tags_any", &tags_any)?;
            grooveseek::server::validate_filter_list("tags_all", &tags_all)?;

            let filters = grooveseek::db::SearchFilters {
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
            let mut reranker_obj: Option<grooveseek::embedder::Reranker> = if rerank_now {
                grooveseek::embedder::Reranker::try_new(reranker_choice)?
            } else {
                None
            };
            let pipeline = grooveseek::server::run_search_pipeline(
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
            let hits_with_id: Vec<(i64, grooveseek::db::SearchHit)> = pipeline
                .into_iter()
                .map(|(id, sr)| (id, sr.into()))
                .collect();

            // Parent retriever 段。enabled = false なら chunk_id を剥がすだけで
            // content / expanded_from は触らない (= v0.6.1 と bit-exact 互換)。
            let resolved = overrides.resolve(&toml_search);
            let parent_params = grooveseek::parent::ParentRetrieverParams {
                whole_doc_threshold_tokens: resolved.parent_whole_doc_threshold_tokens,
                max_expanded_tokens: resolved.parent_max_expanded_tokens,
            };
            let mut hits: Vec<grooveseek::db::SearchHit> =
                grooveseek::parent::apply_parent_retriever(
                    hits_with_id,
                    &db,
                    resolved.parent_retriever_enabled,
                    parent_params,
                );
            // match_spans は Parent retriever 拡張後の content に対して計算する
            // (`expand_parent` は defensive に None クリアするので必ず再計算が要る)。
            for h in &mut hits {
                h.match_spans = grooveseek::server::compute_match_spans(&query, &h.content);
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
            exclude_paths,
            dedup_by_path,
            max_nodes,
            max_seed_chunks,
            format,
        } => {
            let kb_path = require_kb_path(kb_path, cfg.kb_path.clone())?;
            let model = model.or(cfg.model).unwrap_or_default();

            let db_path = grooveseek::resolve_db_path(&kb_path);
            // Status と同じく、DB がまだ作られていない状態を親切なエラーで弾く。
            if !db_path.exists() {
                anyhow::bail!(
                    "No index found at {}. Run `groove index --kb-path {}` first.",
                    db_path.display(),
                    kb_path.display()
                );
            }
            let db = grooveseek::db::Database::open(&db_path.to_string_lossy())?;
            db.verify_embedding_meta(model.model_id(), model.dimension() as u32)?;

            let opts = grooveseek::graph::GraphOptions {
                depth: depth.min(grooveseek::graph::MAX_DEPTH),
                fan_out: fan_out.min(grooveseek::graph::MAX_FAN_OUT),
                min_similarity: min_similarity.clamp(0.0, 1.0),
                seed_strategy,
                category,
                topic,
                exclude_paths,
                dedup_by_path,
                min_quality: cfg
                    .quality_filter
                    .clone()
                    .unwrap_or_default()
                    .effective_threshold(),
                max_nodes: grooveseek::graph::clamp_max_nodes(max_nodes),
                max_seed_chunks: grooveseek::graph::clamp_max_seed_chunks(max_seed_chunks),
            };
            let g = grooveseek::graph::build_connection_graph(&db, &start, &opts)?;
            print_graph(g, format);
        }
        Commands::Doctor { kb_path, format } => {
            // `require_kb_path` inside the mapping, not before it (codex P2
            // round 2): a missing --kb-path is the most ordinary way for this
            // command to fail to run, and letting `?` carry it to `main` would
            // exit 1 — the code the contract reserves for a completed
            // inspection that found something.
            let exit = match require_kb_path(kb_path, cfg.kb_path.clone()) {
                Ok(kb_path) => run_doctor(&kb_path, &cfg, format)?,
                Err(e) => {
                    eprintln!("groove doctor: {e:#}");
                    2
                }
            };
            std::process::exit(exit);
        }
        Commands::Validate {
            kb_path,
            schema,
            format,
            no_color,
            fail_fast,
        } => {
            let kb_path = require_kb_path(kb_path, cfg.kb_path.clone())?;
            // canonicalize は使わない: walkdir は相対パスでも動作し、strip_prefix
            // も同形のパスで一致する。Windows の UNC (\\?\) prefix 漏れを避ける。
            let schema_path = schema.unwrap_or_else(|| kb_path.join("groove-schema.toml"));
            // (feature-49) `validate` も index walk と同じ除外規則で歩く。
            let rules =
                grooveseek::exclusion::ExclusionRules::load(&kb_path, cfg.resolve_exclude_dirs());
            let exit = run_validate(&kb_path, &schema_path, format, no_color, fail_fast, &rules)?;
            std::process::exit(exit);
        }
        Commands::Eval(args) => {
            // Build overrides BEFORE destructuring `args` so the `&args` borrow
            // is short-lived (we still need owned fields below).
            let overrides: grooveseek::config::SearchOverrides = (&args).into();

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
            // Deliberately not `cli_should_rerank`: `groove search` answers a
            // question, `groove eval` measures a pipeline. The run fingerprint
            // records `reranker` and not `rerank_by_default`, so letting that key
            // suppress the reranker here would produce two runs that carry the
            // same fingerprint and measured different pipelines — and
            // `--fail-on-regression` picks its baseline by fingerprint equality.
            let reranker_choice = reranker.or(cfg.reranker).unwrap_or_default();

            let eval_cfg = cfg.eval.clone().unwrap_or_default();
            let golden_path = golden
                .or(eval_cfg.golden.clone())
                .unwrap_or_else(|| kb_path.join(".groove-eval.yml"));
            let k_values = k
                .or(eval_cfg.k_values.clone())
                .unwrap_or_else(|| vec![1, 5, 10]);
            let limit_val = limit.unwrap_or_else(|| *k_values.iter().max().unwrap_or(&10) as u32);
            let history_size = eval_cfg.history_size.unwrap_or(10);
            let regression_threshold = eval_cfg.regression_threshold.unwrap_or(0.05);

            let opts = grooveseek::eval::RunOpts {
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

            let run = grooveseek::eval::run(&opts)?;

            let history_path = grooveseek::eval::default_history_path(&kb_path);
            let history = if no_history {
                grooveseek::eval::History::default()
            } else {
                grooveseek::eval::History::load(&history_path)?
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
                    let out = grooveseek::eval::format_text(
                        &run,
                        previous.as_ref(),
                        tty,
                        regression_threshold,
                    );
                    print!("{}", out);
                }
                EvalFormat::Json => {
                    let v = grooveseek::eval::format_json(&run, previous.as_ref());
                    println!("{}", serde_json::to_string_pretty(&v)?);
                }
            }

            // 混入検出の所見 (feature-52) は **診断なので stderr**。結果そのもの
            // (stdout) には `--format json` の `findings` として既に載っている。
            // 出力形式に関係なく出すのは、text で回している人にこそ届く必要が
            // あるため。exit code は動かさない — 混入かどうかは golden を書いた
            // 人しか判定できず、CI を落とす根拠にはならない。
            if let Some(warning) = grooveseek::eval::format_findings_warning(&run.findings) {
                eprint!("{warning}");
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
            // AU-71: corpus は fingerprint に入っていない (入れると KB が育つ
            // たびに比較が止まる) ので、**互換と判定された run どうしでも
            // corpus は違い得る**。文書が増えれば競合が増えて順位は動くから、
            // regression と corpus 変化が同時に起きたなら、それが第一の容疑者。
            // `run` は下の `push_front` で move されるので、ここで文字列にしておく。
            let (regression_detected, corpus_note) = if fail_on_regression {
                let prev_compat = if no_history {
                    None
                } else {
                    history.previous_compatible(&run)
                };
                let detected = prev_compat.is_some_and(|p| {
                    grooveseek::eval::is_regression(&run, p, regression_threshold)
                });
                // 文言は `describe_corpus_change` に集約する。同じ条件分岐を
                // format_text 側と 2 箇所に書くと、必ず片方だけ直されて食い違う。
                // stderr は CP932 コンソールに出るので ASCII の `->` を使う
                // (stdout の text formatter は `→` / `⚠️` を使っている)。
                let note = detected
                    .then(|| {
                        grooveseek::eval::describe_corpus_change(
                            run.corpus.as_ref(),
                            prev_compat.and_then(|p| p.corpus.as_ref()),
                        )
                    })
                    .flatten()
                    .map(|c| {
                        format!(
                            " The indexed corpus also changed since the compared run ({c}), \
                             which shifts rankings on its own."
                        )
                    });
                (detected, note)
            } else {
                (false, None)
            };

            if !no_history {
                let mut h = history;
                h.push_front(run, history_size);
                h.save(&history_path)?;
            }

            if regression_detected {
                eprintln!(
                    "groove eval: retrieval-quality regression detected (delta > {regression_threshold:.3} \
                     on at least one of recall@k / MRR / nDCG@k).{} Exiting with code 1 because \
                     --fail-on-regression was set.",
                    corpus_note.as_deref().unwrap_or("")
                );
                std::process::exit(1);
            }
        }
        Commands::Tune(args) => {
            let TuneCliArgs {
                kb_path,
                golden,
                model,
                k,
                limit,
                format,
                no_color,
            } = args;

            let kb_path = require_kb_path(kb_path, cfg.kb_path.clone())?;
            let model_choice = model.or(cfg.model).unwrap_or_default();

            let eval_cfg = cfg.eval.clone().unwrap_or_default();
            let golden_path = golden
                .or(eval_cfg.golden.clone())
                .unwrap_or_else(|| kb_path.join(".groove-eval.yml"));

            let (k_values, limit_val) =
                resolve_tune_k_and_limit(k, eval_cfg.k_values.clone(), limit)?;

            if cfg
                .search
                .as_ref()
                .is_some_and(|s| s.mmr.enabled || s.parent_retriever.enabled)
            {
                eprintln!(
                    "groove tune: note — [search.mmr] / [search.parent_retriever] are enabled in \
                     groove.toml but tune measures the plain RRF stage only."
                );
            }
            if !cfg
                .search
                .as_ref()
                .map(|s| s.fusion.is_builtin_default())
                .unwrap_or(true)
            {
                eprintln!(
                    "groove tune: note — [search.fusion] is already customised in groove.toml; \
                     tune still measures deltas against the BUILT-IN defaults, not your values."
                );
            }

            let opts = grooveseek::tune::TuneOpts {
                kb_path,
                golden_path,
                model_choice,
                k_values,
                limit: limit_val,
            };

            match grooveseek::tune::run(&opts)? {
                grooveseek::tune::TuneOutcome::NoEffectiveQueries {
                    query_count,
                    diagnostics,
                } => {
                    eprintln!(
                        "groove tune: none of the {query_count} golden queries is sensitive to the \
                         fusion parameters (every query has fewer than 2 FTS candidates), so the \
                         grid was not run."
                    );
                    eprintln!(
                        "  groove splits each query into per-token phrases joined by OR over a \
                         trigram tokenizer, so a query reaches FTS when any of its tokens occurs \
                         in the text. Queries whose tokens are all shorter than three characters \
                         fall back to matching the query verbatim. Add queries with distinctive \
                         terms (proper nouns, API names, command names) to the golden set and \
                         re-run."
                    );
                    for (id, d) in &diagnostics {
                        eprintln!("  {id}: FTS candidates = {}", d.fts_candidates);
                    }
                    std::process::exit(grooveseek::tune::EXIT_NO_EFFECTIVE_QUERIES);
                }
                grooveseek::tune::TuneOutcome::Report(report) => match format {
                    TuneFormat::Text => {
                        use std::io::IsTerminal;
                        let tty = std::io::stdout().is_terminal() && !no_color;
                        print!("{}", grooveseek::tune::format_text(&report, tty));
                    }
                    TuneFormat::Json => {
                        let v = grooveseek::tune::format_json(&report);
                        println!("{}", serde_json::to_string_pretty(&v)?);
                    }
                },
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
/// `Config::discover` so users with a malformed `groove.toml` in CWD can
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
            grooveseek::service::install::run(grooveseek::service::install::InstallParams {
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
            grooveseek::service::uninstall::run(grooveseek::service::uninstall::UninstallParams {
                service_name,
                purge,
                yes,
            })?;
        }
        // `status` and `list` answer a question; `install` / `uninstall` /
        // `tray-*` report on an action they performed. Only the first two put
        // their output on stdout (ADR-0010).
        ServiceSubcommand::Status { service_name } => {
            let text = grooveseek::service::status::run_status(&service_name)?;
            println!("{}", text);
        }
        ServiceSubcommand::List => {
            let text = grooveseek::service::status::run_list()?;
            println!("{}", text);
        }
        ServiceSubcommand::TrayInstall {
            service_name,
            force,
        } => {
            grooveseek::service::install::run_tray_install(&service_name, force)?;
        }
        ServiceSubcommand::TrayUninstall { service_name } => {
            grooveseek::service::uninstall::run_tray_uninstall(&service_name)?;
        }
    }
    Ok(())
}

/// doctor サブコマンド本体。exit code (0/1/2) を返す。
///
/// 出力は **stdout** (= コマンドの結果)。進捗や診断は stderr、という
/// CLAUDE.md の CLI 出力規約に従い、`validate` / `search` と同じ側に置く。
fn run_doctor(
    kb_path: &Path,
    cfg: &grooveseek::config::Config,
    format: DoctorFormat,
) -> Result<i32> {
    let db_path = grooveseek::resolve_db_path(kb_path);
    if !db_path.exists() {
        // 索引が無いのは「問題を検出した」ではなく「検査できない」。
        eprintln!(
            "No index found. Run `groove index --kb-path {}` first.",
            kb_path.display()
        );
        return Ok(2);
    }
    // Every failure below is "could not look", which the documented contract
    // gives exit 2. Propagating them with `?` would let `main`'s Termination
    // exit 1 — the same code a successful run that found problems uses, so a CI
    // gate could not tell a corrupt database from a finding (codex P2 round 1).
    let looked = (|| -> Result<grooveseek::doctor::Report> {
        let db = grooveseek::db::Database::open(&db_path.to_string_lossy())?;
        let registry = cfg.build_parser_registry()?;
        grooveseek::doctor::run(&db, &registry)
    })();
    let report = match looked {
        Ok(r) => r,
        Err(e) => {
            eprintln!("groove doctor: could not inspect the index: {e:#}");
            return Ok(2);
        }
    };
    print_doctor_report(&report, format);
    Ok(i32::from(!report.is_clean()))
}

fn print_doctor_report(report: &grooveseek::doctor::Report, format: DoctorFormat) {
    match format {
        DoctorFormat::Json => {
            println!(
                "{}",
                serde_json::to_string_pretty(report).unwrap_or_else(|_| "{}".to_string())
            );
        }
        DoctorFormat::Text => {
            println!(
                "groove doctor — {} document(s), {} chunk(s)",
                report.documents, report.chunks
            );
            if report.is_clean() {
                println!("No issues found.");
                return;
            }
            for f in &report.findings {
                println!();
                println!("[{}] {}: {}", f.severity.as_str(), f.check, f.summary);
                for s in &f.samples {
                    println!("    {s}");
                }
                if f.count as usize > f.samples.len() && !f.samples.is_empty() {
                    println!("    ... and {} more", f.count as usize - f.samples.len());
                }
                println!("  fix: {}", f.remedy);
            }
        }
    }
}

/// validate サブコマンド本体。exit code (0/1/2) を返す。
fn run_validate(
    kb_path: &Path,
    schema_path: &Path,
    format: ValidateFormat,
    no_color: bool,
    fail_fast: bool,
    rules: &grooveseek::exclusion::ExclusionRules,
) -> Result<i32> {
    // スキーマ読み込み: 存在しなければ legacy 挙動 (exit 0)
    let schema_obj = match grooveseek::schema::Schema::load_optional(schema_path) {
        Ok(Some(s)) => s,
        Ok(None) => {
            eprintln!(
                "groove validate: no schema found at {} (skipping)",
                schema_path.display()
            );
            return Ok(0);
        }
        Err(e) => {
            eprintln!("groove validate: schema load error: {e:#}");
            return Ok(2);
        }
    };

    // parser registry は `[parsers].enabled` 準拠で .md ファイル列挙に再利用
    // (.txt は frontmatter 概念なしで対象外)。
    let md_parser = grooveseek::parser::MarkdownParser;
    let files = validate_collect_md_files(kb_path, rules)?;

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
        use grooveseek::parser::Parser as ParserTrait;
        let parsed = md_parser.parse(&raw, &rel, &[]);
        let violations = grooveseek::schema::validate(&parsed.frontmatter, &schema_obj);
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

/// validate 専用の `.md` ファイル列挙。除外判定と deterministic ordering は
/// indexer の collect_source_files と同じ方針。
///
/// (feature-49) 判定は [`grooveseek::exclusion::ExclusionRules`] に一本化した。
/// この関数は bin target 側にいるので lib の `pub` を跨いで呼ぶ形になり、
/// **3 caller のうちここだけ取り残される**のが AU-03 と BU-19 の形だった。
fn validate_collect_md_files(
    kb_path: &Path,
    rules: &grooveseek::exclusion::ExclusionRules,
) -> Result<Vec<PathBuf>> {
    use walkdir::WalkDir;
    let mut out = Vec::new();
    for entry in WalkDir::new(kb_path)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| {
            let rel = grooveseek::exclusion::rel_key(kb_path, e.path());
            !rules.is_excluded(&rel, e.file_type().is_dir())
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
    violations: Vec<grooveseek::schema::Violation>,
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
                violations: &'a [grooveseek::schema::Violation],
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
                println!("groove validate: {scanned} files OK");
                return;
            }
            let header = format!("groove validate — {violated} file(s) with violations ({ok} OK)");
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

fn print_graph(g: grooveseek::graph::ConnectionGraph, format: GraphFormat) {
    match format {
        GraphFormat::Json => {
            println!(
                "{}",
                serde_json::to_string_pretty(&g).unwrap_or_else(|_| "{}".into())
            );
        }
        // The drawings are the command's result, so they go to stdout with the
        // other formats — a caller redirects this into a `.dot` or `.svg`.
        GraphFormat::Dot => print!("{}", grooveseek::graph_render::to_dot(&g)),
        GraphFormat::Svg => print!("{}", grooveseek::graph_render::to_svg(&g)),
        GraphFormat::Text => {
            println!("# Connection graph from: {}", g.start_path);
            println!(
                "nodes={} max_depth={} knn_queries={} duration_ms={} seeds_used={} truncated={}",
                g.stats.total_nodes,
                g.stats.max_depth_reached,
                g.stats.knn_queries,
                g.stats.duration_ms,
                g.stats.seeds_used,
                g.truncated
            );
            // (BU-33) 打ち切りの理由と対処は text 出力でも見えるようにする。
            // ここを落とすと、CLI だけが上限の見えない経路になる。
            for t in &g.truncation {
                // `Display`、not `Debug`: the JSON spells these `seed_chunks` /
                // `node_budget` and the two surfaces must not disagree.
                println!(
                    "! truncated ({}, limit={}): {}",
                    t.reason, t.limit, t.detail
                );
            }
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

/// How `groove search` spells the per-call rerank override, and nothing else.
///
/// The decision itself is [`grooveseek::server::should_rerank`], shared with the
/// MCP tool — writing a second copy of it here is how the two surfaces drifted
/// apart in the first place. What is genuinely local is the translation: over
/// MCP the override is the `rerank` parameter, and here it is naming a model.
///
/// Naming one on the command line is a statement about this query, so it wins
/// outright — the rule `docs/configuration.md` already states for every other
/// option, that CLI arguments always win — and `--reranker none` turns rerank off
/// for one query by the same route, since it leaves nothing to run.
///
/// No `--rerank` flag was added for it: `docs/stability.md` freezes the MCP
/// `rerank` parameter as the per-call boolean and `--reranker` as the model
/// picker, and a `--rerank` one letter away from `--reranker`, taking a different
/// type, would be frozen beside it at 1.0.0.
fn cli_should_rerank(explicit: bool, choice: RerankerChoice, cfg_default: Option<bool>) -> bool {
    grooveseek::server::should_rerank(explicit.then_some(true), cfg_default, choice.is_enabled())
}

#[allow(clippy::too_many_arguments)]
fn print_search_results(
    hits: Vec<grooveseek::db::SearchHit>,
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
    let low_confidence = grooveseek::server::compute_low_confidence(&scores, min_confidence_ratio);

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

/// The command line and the MCP tools are two namespaces that
/// [`docs/stability.md`] freezes separately, and the promise made there is that
/// where both expose the same concept they use the same noun. Nothing enforces
/// that when a parameter is added: the two definitions live in different files,
/// neither mentions the other, and a new flag compiles perfectly well with no
/// counterpart at all.
///
/// So the pairing itself is written down here, and checked against both
/// surfaces as they actually are — the flags out of `clap`, the parameters out
/// of the advertised JSON schema.
///
/// Whether two names share a *noun* is not something a test can decide. What it
/// can do is refuse to let either surface grow a name that this table has not
/// accounted for, which puts the question in front of whoever adds it, while
/// the answer is still cheap to change.
///
/// [`docs/stability.md`]: https://github.com/alphabet-h/grooveseek/blob/main/docs/stability.md
#[cfg(test)]
mod naming_surface {
    use super::*;
    use clap::CommandFactory;

    /// `(MCP parameter, CLI long flag)` for a concept both surfaces expose.
    /// Spelling differs by each surface's own convention — kebab-case and a
    /// singular repeatable flag on one side, snake_case and a plural array on
    /// the other — so these are pairs, not equalities.
    const SEARCH_PAIRS: &[(&str, &str)] = &[
        ("category", "category"),
        ("date_from", "date-from"),
        ("date_to", "date-to"),
        ("include_low_quality", "include-low-quality"),
        ("limit", "limit"),
        ("min_confidence_ratio", "min-confidence-ratio"),
        ("min_quality", "min-quality"),
        ("mmr", "mmr"),
        ("mmr_lambda", "mmr-lambda"),
        ("mmr_same_doc_penalty", "mmr-same-doc-penalty"),
        ("parent_retriever", "parent-retriever"),
        ("path_globs", "path-glob"),
        ("tags_all", "tag-all"),
        ("tags_any", "tag-any"),
        ("topic", "topic"),
    ];

    /// Reachable over MCP and not as a `groove search` flag, each with the
    /// reason it is not an oversight.
    const SEARCH_MCP_ONLY: &[(&str, &str)] = &[
        ("query", "the CLI takes the query as a positional argument"),
        (
            "rerank",
            "a per-call boolean; the CLI's --reranker picks a model instead, and naming one there is how a single `groove search` opts in or out. The standing default behind both is the `rerank_by_default` key, which each surface reads, and whose flag is `groove serve --rerank-by-default`",
        ),
    ];

    /// The reverse: `groove search` flags with no tool parameter.
    const SEARCH_CLI_ONLY: &[(&str, &str)] = &[
        (
            "kb-path",
            "the server is already pointed at a knowledge base",
        ),
        (
            "model",
            "fixed when the server starts; it must match the index",
        ),
        ("reranker", "picks a model; see `rerank` above"),
        (
            "format",
            "a tool call always answers with one JSON text block",
        ),
    ];

    const GRAPH_PAIRS: &[(&str, &str)] = &[
        ("category", "category"),
        ("dedup_by_path", "dedup-by-path"),
        ("depth", "depth"),
        ("exclude_paths", "exclude-paths"),
        ("fan_out", "fan-out"),
        ("max_nodes", "max-nodes"),
        ("max_seed_chunks", "max-seed-chunks"),
        ("min_similarity", "min-similarity"),
        ("seed_strategy", "seed-strategy"),
        ("start", "start"),
        ("topic", "topic"),
    ];

    const GRAPH_MCP_ONLY: &[(&str, &str)] = &[];

    const GRAPH_CLI_ONLY: &[(&str, &str)] = &[
        (
            "kb-path",
            "the server is already pointed at a knowledge base",
        ),
        (
            "model",
            "fixed when the server starts; it must match the index",
        ),
        (
            "format",
            "text / dot / svg are renderings for a person; the tool stays JSON",
        ),
    ];

    /// Flags every subcommand carries, which therefore say nothing about the
    /// tool surface. `help` is generated by clap; `config` is `global = true`.
    /// `rebuild_index` ⇄ `groove index`. Both spell the same one thing the same
    /// way, which is the easy case and still worth pinning: nothing else stops
    /// a parameter appearing on one side only.
    const INDEX_PAIRS: &[(&str, &str)] = &[("force", "force")];

    const INDEX_MCP_ONLY: &[(&str, &str)] = &[];

    const INDEX_CLI_ONLY: &[(&str, &str)] = &[
        (
            "kb-path",
            "the server is already pointed at a knowledge base",
        ),
        (
            "model",
            "fixed when the server starts; it must match the index",
        ),
        (
            "quiet",
            "shape of a terminal's progress output; a tool call has no terminal",
        ),
        (
            "progress",
            "same — the tool answers once, with the counts, when it is done",
        ),
    ];

    /// Tools with no command behind them, and why.
    ///
    /// Every tool needs to be accounted for somewhere. Before this list, four of
    /// the six were in neither a pairing table nor anywhere else, so a parameter
    /// could appear on any of them without a test noticing. Each entry pins the
    /// parameter names the tool advertises, so drift is caught here even where
    /// there is no second surface to compare against.
    ///
    /// A **seventh** tool is caught elsewhere:
    /// `server.rs::tool_handlers_do_not_block_the_runtime` asserts the count is
    /// exactly 6, so adding one fails there and sends the author to the tool
    /// surface — where this list is the next thing to update.
    const TOOLS_WITHOUT_A_COMMAND: &[(&str, &[&str], &str)] = &[
        (
            "get_document",
            &["path"],
            "reading one document by path is what a file system already does; \
             the tool exists so a model can, over the same connection it \
             searches on",
        ),
        (
            "get_best_practice",
            &["category", "target"],
            "opt-in and configured by `[best_practice].path_templates`; a \
             command for it would be `cat` with extra steps",
        ),
    ];

    /// The one tool that takes no parameters at all.
    ///
    /// `advertised_param_names` cannot answer for it — the handler has no
    /// params struct, so there is no type to derive a schema from, and it
    /// returns `None` rather than an empty list. Worth stating, because `None`
    /// otherwise reads like "this tool is unknown".
    const TOOL_WITHOUT_PARAMETERS: &str = "list_topics";

    /// How a flag that takes more than one value takes them.
    ///
    /// The two surfaces were reconciled by name in #178 on the grounds that a
    /// name which differs costs a lookup while a *value* which differs fails the
    /// call. The value half was never checked, and it had already come apart:
    /// `docs/usage.md` documents `--path-glob <PATTERN>` as **(repeatable)**
    /// while the flag carried `value_delimiter = ','`. A glob is the one kind of
    /// value whose own syntax uses commas, so `--path-glob 'docs/{a,b}/**'` was
    /// split down the middle and rejected as an unclosed alternate group. The
    /// MCP `path_globs` takes an array, where the same value arrives intact.
    #[derive(Clone, Copy, Debug, PartialEq)]
    enum Multiplicity {
        /// Give the flag again for each value. `docs/usage.md` spells these
        /// `<PATTERN>` and says "(repeatable)".
        Repeatable,
        /// One flag, values separated by commas. `docs/usage.md` spells these
        /// `<a,b,c>`, which is the contract for tags and for graph exclusions —
        /// neither of which can contain a comma meaningfully.
        CommaList,
    }

    /// Every paired flag that accepts more than one value, and which it is.
    const MULTI_VALUE_FLAGS: &[(&str, &str, Multiplicity)] = &[
        ("search", "path-glob", Multiplicity::Repeatable),
        ("search", "tag-any", Multiplicity::CommaList),
        ("search", "tag-all", Multiplicity::CommaList),
        ("graph", "exclude-paths", Multiplicity::CommaList),
    ];

    const GLOBAL_FLAGS: &[&str] = &["config", "help"];

    fn cli_long_flags(subcommand: &str) -> Vec<String> {
        let cmd = Cli::command();
        let sub = cmd
            .find_subcommand(subcommand)
            .unwrap_or_else(|| panic!("no `{subcommand}` subcommand"));
        let mut flags: Vec<String> = sub
            .get_arguments()
            .filter_map(|a| a.get_long())
            .filter(|l| !GLOBAL_FLAGS.contains(l))
            .map(str::to_string)
            .collect();
        flags.sort();
        flags
    }

    /// The names one surface should carry: its half of every pair, plus the
    /// names only it has. A pair is `(MCP, CLI)`; a one-sided entry is
    /// `(name, reason)`, so its name is always the first element.
    fn expected(pairs: &[(&str, &str)], one_sided: &[(&str, &str)], mcp_side: bool) -> Vec<String> {
        let mut all: Vec<String> = pairs
            .iter()
            .map(|p| if mcp_side { p.0 } else { p.1 })
            .chain(one_sided.iter().map(|p| p.0))
            .map(str::to_string)
            .collect();
        all.sort();
        all
    }

    fn check(
        tool: &str,
        subcommand: &str,
        pairs: &[(&str, &str)],
        mcp_only: &[(&str, &str)],
        cli_only: &[(&str, &str)],
    ) {
        let advertised = grooveseek::server::advertised_param_names(tool).expect("tool must exist");
        assert_eq!(
            advertised,
            expected(pairs, mcp_only, true),
            "the `{tool}` tool's parameters no longer match the table in this module. \
             Add the new name with the flag it pairs with on `groove {subcommand}`, \
             or to the MCP-only list with the reason it has none."
        );

        assert_eq!(
            cli_long_flags(subcommand),
            expected(pairs, cli_only, false),
            "`groove {subcommand}`'s flags no longer match the table in this module. \
             Add the new flag with the `{tool}` parameter it pairs with, or to the \
             CLI-only list with the reason it has none."
        );
    }

    #[test]
    fn search_names_stay_paired() {
        check(
            "search",
            "search",
            SEARCH_PAIRS,
            SEARCH_MCP_ONLY,
            SEARCH_CLI_ONLY,
        );
    }

    #[test]
    fn graph_names_stay_paired() {
        check(
            "get_connection_graph",
            "graph",
            GRAPH_PAIRS,
            GRAPH_MCP_ONLY,
            GRAPH_CLI_ONLY,
        );
    }

    #[test]
    fn index_names_stay_paired() {
        check(
            "rebuild_index",
            "index",
            INDEX_PAIRS,
            INDEX_MCP_ONLY,
            INDEX_CLI_ONLY,
        );
    }

    /// The tools with no command behind them still have their parameters
    /// pinned. There is no second surface to compare against, so this compares
    /// against a written list — which is the point: it makes adding a parameter
    /// a thing someone has to write down.
    #[test]
    fn a_tool_without_a_command_still_declares_its_parameters() {
        for (tool, params, _reason) in TOOLS_WITHOUT_A_COMMAND {
            let advertised =
                grooveseek::server::advertised_param_names(tool).expect("tool must exist");
            let expected: Vec<String> = params.iter().map(|s| s.to_string()).collect();
            assert_eq!(
                advertised, expected,
                "the `{tool}` tool's parameters changed. It has no command to \
                 pair with, so this list is the only record of them."
            );
        }
    }

    /// What the documentation says about taking several values, checked against
    /// what `clap` was told.
    ///
    /// This is the value half of the pairing rule. It cannot compare the two
    /// surfaces directly — MCP takes arrays for all four, so there is nothing to
    /// disagree with — but it can hold the command line to the contract its own
    /// page states, which is where the disagreement came from.
    #[test]
    fn a_flag_documented_as_repeatable_does_not_split_on_commas() {
        let cmd = Cli::command();
        for (subcommand, flag, multiplicity) in MULTI_VALUE_FLAGS {
            let sub = cmd
                .find_subcommand(subcommand)
                .unwrap_or_else(|| panic!("no `{subcommand}` subcommand"));
            let arg = sub
                .get_arguments()
                .find(|a| a.get_long() == Some(flag))
                .unwrap_or_else(|| panic!("`groove {subcommand}` has no `--{flag}`"));

            match multiplicity {
                Multiplicity::Repeatable => assert_eq!(
                    arg.get_value_delimiter(),
                    None,
                    "`groove {subcommand} --{flag}` is documented as repeatable \
                     but splits its value on a delimiter. A value whose own \
                     syntax contains that character cannot be passed at all."
                ),
                Multiplicity::CommaList => assert_eq!(
                    arg.get_value_delimiter(),
                    Some(','),
                    "`groove {subcommand} --{flag}` is documented as a \
                     comma-separated list but does not split on commas."
                ),
            }
        }
    }

    /// The value that started it. `docs/{a,b}/**` is ordinary glob syntax and
    /// the MCP side has always accepted it; the command line turned it into
    /// `docs/{a` and `b}/**`, and the first of those is not a glob.
    #[test]
    fn a_glob_containing_a_comma_survives_the_command_line() {
        let cli = Cli::try_parse_from(["groove", "search", "q", "--path-glob", "docs/{a,b}/**"])
            .expect("clap must accept a brace-alternation glob");
        match cli.command {
            Commands::Search(a) => assert_eq!(
                a.path_globs,
                vec!["docs/{a,b}/**".to_string()],
                "the glob was split into pieces; each piece is a parse error"
            ),
            _ => panic!("expected Search subcommand"),
        }
    }

    /// The other half of the same contract: a comma-list flag still takes one.
    #[test]
    fn a_comma_list_flag_still_takes_a_comma_list() {
        let cli = Cli::try_parse_from(["groove", "search", "q", "--tag-any", "rust,async"])
            .expect("clap must accept a comma-separated tag list");
        match cli.command {
            Commands::Search(a) => assert_eq!(
                a.tags_any,
                vec!["rust".to_string(), "async".to_string()],
                "`--tag-any <a,b,c>` is the documented spelling"
            ),
            _ => panic!("expected Search subcommand"),
        }
    }

    /// `list_topics` takes nothing, and `advertised_param_names` says so by
    /// answering `None`. Stating it here keeps that `None` from being read as
    /// "some tool this module has never heard of".
    #[test]
    fn the_parameterless_tool_is_the_only_one_without_a_schema() {
        assert!(
            grooveseek::server::advertised_param_names(TOOL_WITHOUT_PARAMETERS).is_none(),
            "`{TOOL_WITHOUT_PARAMETERS}` grew parameters; give it a row in one \
             of the tables above"
        );

        for tool in [
            "search",
            "get_connection_graph",
            "rebuild_index",
            "get_document",
            "get_best_practice",
        ] {
            assert!(
                grooveseek::server::advertised_param_names(tool).is_some(),
                "`{tool}` is in a table here but the server cannot describe its \
                 parameters, so nothing in this module is actually checking it"
            );
        }
    }

    /// The stricter half of the rule: a *name* that differs between the two
    /// surfaces costs a lookup, but a *value* that differs fails the call. So
    /// every spelling of `seed_strategy` has to work here, whichever surface
    /// it was copied from.
    ///
    /// Driven by `SeedStrategy::SPELLINGS` rather than by a list of its own —
    /// a list here would keep passing while the tool learned a spelling the
    /// command line had never heard of. The tool-side half is
    /// `server::tests::every_accepted_seed_strategy_spelling_parses_in_the_tool`.
    #[test]
    fn every_accepted_seed_strategy_spelling_parses_on_the_command_line() {
        for spelling in SeedStrategy::SPELLINGS {
            let cli = Cli::try_parse_from([
                "groove",
                "graph",
                "--start",
                "a.md",
                "--seed-strategy",
                spelling.text,
            ])
            .unwrap_or_else(|e| {
                panic!(
                    "`groove graph --seed-strategy {}` must parse — it is in \
                     SeedStrategy::SPELLINGS, so the tool accepts it: {e}",
                    spelling.text
                )
            });
            match cli.command {
                Commands::Graph { seed_strategy, .. } => assert_eq!(
                    seed_strategy, spelling.value,
                    "{} must mean the same strategy on both surfaces",
                    spelling.text
                ),
                _ => panic!("`groove graph …` must parse as the graph subcommand"),
            }
        }
    }

    /// `--help` advertises one spelling per strategy even though more are
    /// accepted, so each surface still shows the name its own conventions
    /// produce. A spelling flagged `advertised_by_cli` must be offered, and one
    /// that is not must stay out of the list while remaining parseable — which
    /// the test above is what proves.
    #[test]
    fn the_cli_advertises_only_its_own_spellings() {
        let cmd = Cli::command();
        let arg = cmd
            .find_subcommand("graph")
            .expect("no `graph` subcommand")
            .get_arguments()
            .find(|a| a.get_long() == Some("seed-strategy"))
            .expect("no --seed-strategy");
        let shown: Vec<String> = arg
            .get_possible_values()
            .iter()
            .filter(|p| !p.is_hide_set())
            .map(|p| p.get_name().to_string())
            .collect();
        let want: Vec<String> = SeedStrategy::SPELLINGS
            .iter()
            .filter(|s| s.advertised_by_cli)
            .map(|s| s.text.to_string())
            .collect();
        assert_eq!(shown, want, "`--seed-strategy` advertises the wrong set");
    }
}

/// `docs/stability.md` freezes "the long flags of the `groove` binary that this
/// documentation describes", which is only a set if something decides what is in
/// it. Left to prose, the answer is "whichever flags somebody happened to write
/// about": `--schema` — the one way to point `validate` at a schema that is not
/// beside the knowledge base — went unmentioned for its whole life and would
/// have been left unfrozen by accident.
///
/// So the two directions are checked here instead. Every flag the binary accepts
/// has to appear in the published documentation, and every flag-shaped token in
/// that documentation has to be one the binary accepts or one of the few that
/// belong to other programs. The second direction is not symmetry for its own
/// sake: it is what found `--verbose` in the stability policy, a flag `groove`
/// has never had.
#[cfg(test)]
mod documented_flags {
    use super::*;
    use clap::CommandFactory;
    use std::collections::BTreeSet;
    use std::path::{Path, PathBuf};

    /// This crate lives at `<repo>/grooveseek`, and the documentation it has to
    /// agree with is published from `<repo>`.
    fn repo_root() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("the crate directory always has a parent")
            .to_path_buf()
    }

    /// Which of the two published languages a check is reading.
    ///
    /// This project is English-primary and bilingual: every page under `docs/`
    /// has a `.ja.md` counterpart, and `docs/stability.md` — the page that says
    /// what "documented" freezes — is the English one.
    ///
    /// The corpora used to be concatenated into a single buffer, which meant a
    /// flag described only in Japanese satisfied the coverage check. Nothing was
    /// wrong when this was measured — every flag appears on both sides today —
    /// and that is exactly when it is cheap to stop it from going wrong.
    #[derive(Clone, Copy, Debug)]
    enum Corpus {
        English,
        Japanese,
    }

    impl Corpus {
        fn readme(self) -> &'static str {
            match self {
                Corpus::English => "README.md",
                Corpus::Japanese => "README.ja.md",
            }
        }

        /// `.ja.md` cannot be told from `.md` by extension — `Path::extension()`
        /// answers `"md"` for both, because it reads from the last dot. The file
        /// name's suffix is the only thing that separates them.
        fn owns(self, path: &Path) -> bool {
            let japanese = path
                .file_name()
                .is_some_and(|n| n.to_string_lossy().ends_with(".ja.md"));
            matches!(self, Corpus::Japanese) == japanese
        }

        fn label(self) -> &'static str {
            match self {
                Corpus::English => "English",
                Corpus::Japanese => "Japanese",
            }
        }
    }

    /// Everything a reader of one published language sees.
    ///
    /// `CHANGELOG.md` is left out on purpose: it records flags that *used* to
    /// exist, and a removed flag must not stay frozen by its own obituary.
    ///
    /// `docs/decisions/` is left out for the same reason and one more. An ADR
    /// that explains why a flag was removed names it, which would fail the
    /// reverse check — and ADRs are immutable once merged
    /// ([ADR-0000](../../docs/decisions/0000-record-decisions-as-adrs.md)), so
    /// that failure could not be repaired, only worked around. Excluding them
    /// also tightens the forward check rather than loosening it: a flag
    /// mentioned only in a decision record no longer counts as documented,
    /// because a reader looking for how to use the tool does not read the
    /// minutes.
    fn published_docs(corpus: Corpus) -> String {
        let root = repo_root();
        let mut buf = String::new();
        let p = root.join(corpus.readme());
        buf.push_str(&std::fs::read_to_string(&p).unwrap_or_else(|e| {
            panic!(
                "{} must be readable to check flag coverage: {e}",
                p.display()
            )
        }));
        buf.push('\n');

        let docs = root.join("docs");
        assert!(
            docs.is_dir(),
            "expected the documentation at {}. If the layout moved, this test \
             moves with it rather than being deleted",
            docs.display()
        );
        let decisions = docs.join("decisions");
        for entry in walkdir::WalkDir::new(&docs).into_iter().flatten() {
            if entry.path().starts_with(&decisions) {
                continue;
            }
            if entry.path().extension().is_some_and(|e| e == "md")
                && corpus.owns(entry.path())
                && let Ok(text) = std::fs::read_to_string(entry.path())
            {
                buf.push_str(&text);
                buf.push('\n');
            }
        }
        buf
    }

    /// Every long flag the binary accepts, as `(command path, flag)`.
    ///
    /// Keyed by the pair rather than by the flag: the same name lives on
    /// several subcommands, and collapsing them would let documentation for one
    /// vouch for another. Global flags are attributed to the root instead —
    /// clap puts them on every subcommand, and they are documented once.
    fn own_long_flags() -> BTreeSet<(String, String)> {
        fn walk(cmd: &clap::Command, path: &str, out: &mut BTreeSet<(String, String)>) {
            for arg in cmd.get_arguments() {
                if let Some(long) = arg.get_long() {
                    if long == "help" || long == "version" {
                        continue; // clap generates these
                    }
                    let owner = if arg.is_global_set() { "groove" } else { path };
                    out.insert((owner.to_string(), long.to_string()));
                }
            }
            for sub in cmd.get_subcommands() {
                let child = format!("{path} {}", sub.get_name());
                walk(sub, &child, out);
            }
        }
        let cmd = Cli::command();
        let mut out = BTreeSet::new();
        walk(&cmd, "groove", &mut out);
        out
    }

    /// The `--name` tokens appearing in a body of prose, as whole tokens.
    ///
    /// Both directions compare against this one set rather than searching for
    /// substrings, because a flag can be a prefix of another: `--k` is inside
    /// `--kb-path`, so `contains("--k")` would call `--k` documented on the
    /// strength of a flag it has nothing to do with. The name may be a single
    /// character for the same reason: `--k` is real, and a pattern demanding
    /// two would leave the one case that needs this unprotected.
    ///
    /// The trailing boundary matters as much as the leading one. Greedy
    /// `[a-z0-9-]*` stops at an uppercase letter or an underscore, so a
    /// malformed `--kValue` in the prose would otherwise hand back `k` and
    /// vouch for a flag nobody wrote about.
    fn documented_flag_tokens(corpus: Corpus) -> BTreeSet<String> {
        let re =
            regex::Regex::new(r"--([a-z][a-z0-9-]*)(?:[^A-Za-z0-9_]|$)").expect("a valid pattern");
        re.captures_iter(&published_docs(corpus))
            .map(|c| c[1].to_string())
            .collect()
    }

    /// What this guarantees, and what it cannot.
    ///
    /// It guarantees that no flag ships undescribed — the thing that had gone
    /// wrong, and the thing `docs/stability.md` needs in order to mean anything
    /// by "documented".
    ///
    /// It does **not** guarantee that each command's copy of a shared name is
    /// separately described. Checking that was implemented and measured, and it
    /// cannot be done from prose: a section is free to mention another
    /// command's flags, and legitimately does. The page describing
    /// `groove doctor` says findings are fixed by `groove index --force`, so
    /// any proximity rule counts `--force` as documented for `doctor` too.
    /// Attaching flags to commands would need the documentation to carry
    /// structure it does not have, and a rule that is wrong in both directions
    /// is worse than a narrow one that is right.
    ///
    /// The pair is still carried through so that a failure names the command,
    /// which is what someone reading the message needs.
    ///
    /// Each language is checked on its own. Pooled, a flag written up only in
    /// Japanese would satisfy a check whose whole purpose is to give
    /// `docs/stability.md` — an English page — something to mean by
    /// "documented"; and a flag written up only in English would leave the
    /// Japanese reader without it. Both sides pass today.
    #[test]
    fn every_long_flag_the_binary_accepts_is_documented() {
        for corpus in [Corpus::English, Corpus::Japanese] {
            let documented = documented_flag_tokens(corpus);
            let missing: Vec<String> = own_long_flags()
                .iter()
                .filter(|(_, flag)| !documented.contains(flag))
                .map(|(cmd, flag)| format!("`{cmd} --{flag}`"))
                .collect();

            assert!(
                missing.is_empty(),
                "these flags appear nowhere in the {} documentation, so \
                 docs/stability.md would not freeze them:\n  {}\n\
                 Document them where the command is documented, in both \
                 languages, or remove them before 1.0. Leaving a flag \
                 undocumented is a decision, not an oversight.",
                corpus.label(),
                missing.join("\n  ")
            );
        }
    }

    #[test]
    fn the_documentation_does_not_name_flags_groove_lacks() {
        // Flags of other programs, named in examples here. Each is the flag of
        // a command the reader runs alongside `groove`, not of `groove`.
        const FOREIGN: &[(&str, &str)] = &[
            ("user", "systemctl --user, in the service migration note"),
            ("debug", "groove-tray --debug, a separate binary"),
            ("release", "cargo build --release, in the README"),
            (
                "include",
                "huggingface-cli download --include, in clients.md",
            ),
        ];

        // Which command owns a flag does not matter here; only whether the
        // binary has it under any name at all. Both languages are pooled for
        // this direction, unlike the forward one: a name the binary does not
        // have is wrong wherever it was written, and the allow-list below is
        // about other programs rather than about either language.
        let own: BTreeSet<String> = own_long_flags().into_iter().map(|(_, f)| f).collect();
        let mut documented = documented_flag_tokens(Corpus::English);
        documented.extend(documented_flag_tokens(Corpus::Japanese));
        let stray: Vec<String> = documented
            .into_iter()
            .filter(|f| !own.contains(f))
            .filter(|f| !FOREIGN.iter().any(|(name, _)| name == f))
            .collect();

        assert!(
            stray.is_empty(),
            "the documentation names flags `groove` does not accept:\n  --{}\n\
             Either the flag was renamed and the docs were not, or it never \
             existed. If it belongs to another program, add it to FOREIGN with \
             the reason.",
            stray.join("\n  --")
        );
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
        let o: grooveseek::config::SearchOverrides = (&args).into();
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
        let o: grooveseek::config::SearchOverrides = (&args).into();
        assert_eq!(o.mmr, Some(true));
        assert_eq!(o.mmr_lambda, Some(0.5));
        assert_eq!(o.mmr_same_doc_penalty, Some(0.2));
        assert_eq!(o.parent_retriever, Some(true));
    }

    #[test]
    fn test_eval_cli_args_into_overrides_all_none() {
        let args = eval_args_default();
        let o: grooveseek::config::SearchOverrides = (&args).into();
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
        let o: grooveseek::config::SearchOverrides = (&args).into();
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
            "groove",
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
            "groove",
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

    /// `groove search` used to rerank whenever a reranker was *configured*, so
    /// `reranker = "bge-v2-m3"` beside `rerank_by_default = false` — the pair
    /// three shipped deployment recipes carry — reranked from the command line
    /// and not from the server reading the same file. A test that actually
    /// reranks would need the ~2.3 GB cross-encoder, so the decision is a pure
    /// function and these cover it directly.
    #[test]
    fn a_reranker_that_only_came_from_the_config_obeys_rerank_by_default() {
        let m = RerankerChoice::BgeV2M3;
        assert!(!cli_should_rerank(false, m, Some(false)));
        assert!(cli_should_rerank(false, m, Some(true)));
        // Absent key means yes — the same default `serve` resolves.
        assert!(cli_should_rerank(false, m, None));
    }

    #[test]
    fn naming_a_reranker_on_the_command_line_settles_that_query() {
        // `--reranker <model>` is this query's opt-in, whatever the file says.
        assert!(cli_should_rerank(
            true,
            RerankerChoice::BgeV2M3,
            Some(false)
        ));
        // `--reranker none` is its opt-out, which is how it already behaved.
        assert!(!cli_should_rerank(true, RerankerChoice::None, Some(true)));
    }

    #[test]
    fn rerank_by_default_cannot_conjure_a_reranker_that_was_never_configured() {
        for cfg_default in [None, Some(true), Some(false)] {
            assert!(!cli_should_rerank(false, RerankerChoice::None, cfg_default));
            assert!(!cli_should_rerank(true, RerankerChoice::None, cfg_default));
        }
    }

    /// A non-finite ratio reached `compute_low_confidence`, where every
    /// comparison against it is false: a value passed to *tighten* the check
    /// turned it off instead. The JSON echo could not report that either — serde
    /// writes a non-finite float as `null` and `strip_null_keys` then drops the
    /// key, so the output carried no trace of the override at all.
    #[test]
    fn a_non_finite_confidence_ratio_is_refused_at_the_entry() {
        for bad in ["nan", "NaN", "inf", "-inf", "infinity"] {
            assert!(
                Cli::try_parse_from(["groove", "search", "q", "--min-confidence-ratio", bad])
                    .is_err(),
                "--min-confidence-ratio {bad} must not parse"
            );
        }
    }

    #[test]
    fn a_negative_confidence_ratio_is_refused_but_zero_is_the_documented_off_switch() {
        assert!(
            Cli::try_parse_from(["groove", "search", "q", "--min-confidence-ratio", "-1"]).is_err(),
            "a negative ratio never fires and is not how the check is disabled"
        );
        let cli = Cli::try_parse_from(["groove", "search", "q", "--min-confidence-ratio", "0"])
            .expect("0.0 disables the low_confidence check by documented design");
        match cli.command {
            Commands::Search(a) => assert_eq!(a.min_confidence_ratio, Some(0.0)),
            _ => panic!("expected Search subcommand"),
        }
    }

    #[test]
    fn an_ordinary_confidence_ratio_still_parses() {
        let cli = Cli::try_parse_from(["groove", "search", "q", "--min-confidence-ratio", "1.5"])
            .expect("the documented default value must remain acceptable");
        match cli.command {
            Commands::Search(a) => assert_eq!(a.min_confidence_ratio, Some(1.5)),
            _ => panic!("expected Search subcommand"),
        }
    }

    #[test]
    fn test_resolve_tune_k_and_limit_defaults() {
        // CLI 未指定 + [eval] 未設定 -> ビルトイン [1, 5, 10]、limit は最大値。
        let (k, limit) = resolve_tune_k_and_limit(None, None, None).unwrap();
        assert_eq!(k, vec![1, 5, 10]);
        assert_eq!(limit, 10);
    }

    #[test]
    fn test_resolve_tune_k_and_limit_injects_primary_k() {
        // --k 1,10 のように主指標 5 を外しても、tune 側で必ず補われる。
        let (k, limit) = resolve_tune_k_and_limit(Some(vec![1, 10]), None, None).unwrap();
        assert_eq!(k, vec![1, 5, 10]);
        assert_eq!(limit, 10, "limit の既定は正規化後の k リストの最大値");
    }

    #[test]
    fn test_resolve_tune_k_and_limit_cli_overrides_config() {
        // CLI > [eval].k_values の優先順位。
        let (k, _) = resolve_tune_k_and_limit(Some(vec![3]), Some(vec![1, 20]), None).unwrap();
        assert_eq!(k, vec![3, 5]);

        // CLI 未指定なら [eval] を使う。
        let (k, limit) = resolve_tune_k_and_limit(None, Some(vec![1, 20]), None).unwrap();
        assert_eq!(k, vec![1, 5, 20]);
        assert_eq!(limit, 20);
    }

    #[test]
    fn test_resolve_tune_k_and_limit_explicit_limit_wins() {
        // --limit を明示したら k リストの最大値では上書きしない。
        let (k, limit) = resolve_tune_k_and_limit(Some(vec![1, 5]), None, Some(100)).unwrap();
        assert_eq!(k, vec![1, 5]);
        assert_eq!(limit, 100);
    }

    /// Regression (codex P2 on PR #79): 明示 --limit が max(k) より小さいと
    /// fused ranking が limit で切り詰められ、nDCG@5 等がラベルより浅い候補
    /// から計算されてしまう。max(k) を下限として clamp する。
    #[test]
    fn test_resolve_tune_k_and_limit_clamps_limit_to_max_k() {
        let (k, limit) = resolve_tune_k_and_limit(Some(vec![1, 5, 10]), None, Some(1)).unwrap();
        assert_eq!(k, vec![1, 5, 10]);
        assert_eq!(limit, 10, "limit は max(k) 未満に縮めない");
    }

    /// Regression (codex P2 round 3/4 on PR #79): u32 に収まらない k は wrap /
    /// saturate のどちらでも壊れる (limit 0 化 or 下流 with_capacity abort) ため
    /// エラーとして reject する。
    #[test]
    fn test_resolve_tune_k_and_limit_rejects_oversized_k() {
        assert!(resolve_tune_k_and_limit(Some(vec![usize::MAX]), None, None).is_err());
        assert!(resolve_tune_k_and_limit(Some(vec![1, 5]), None, Some(u32::MAX)).is_err());
    }
}
