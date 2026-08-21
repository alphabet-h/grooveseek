//! Connection Graph: 起点ドキュメントからベクトル類似度で BFS 展開する機能。
//!
//! `get_connection_graph` MCP ツールと `groove graph` CLI サブコマンドの
//! バックエンド。grand plan は `docs/` 参照。

use std::collections::{HashSet, VecDeque};
use std::time::Instant;

use anyhow::{Context, Result};
use serde::Serialize;

use crate::db::{Database, SearchResult};

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// BFS の探索ポリシー。
///
/// `all_chunks` は起点ドキュメントのチャンクを 1 つずつシードにして BFS を開始し、
/// 各々から KNN を広げる。`centroid` はチャンク埋め込みの平均ベクトルを L2
/// 再正規化してから 1 つの擬似シードとして扱う (BGE 系の embedding が単位
/// ベクトルであるため、平均後も再正規化しないと `distance_to_cos_sim` の前提が
/// 崩れる)。
///
/// **どちらも対象は `max_seed_chunks` で切られた前半だけ** (BU-33)。上限は
/// 読み取りに掛かるので、`centroid` に切り替えても平均される範囲は広がらない —
/// 変わるのはシード**ノード**が 1 個になり node 予算が connection に回ること。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SeedStrategy {
    #[default]
    AllChunks,
    Centroid,
}

/// One accepted spelling of a [`SeedStrategy`].
///
/// The two surfaces spell this value differently by their own conventions —
/// the command line is kebab-case, the tool's parameters are snake_case — and
/// both accept both, because a value that differs between them fails the call
/// rather than costing a lookup (`docs/stability.md`).
///
/// That promise holds only while there is **one** table. Two parsers with
/// their own lists drift as soon as a strategy is added to one of them, and
/// two tests with their own lists do not notice.
#[derive(Debug, Clone, Copy)]
pub struct SeedSpelling {
    /// What a user types, or a client sends.
    pub text: &'static str,
    pub value: SeedStrategy,
    /// Whether `groove graph --help` lists this spelling under
    /// `[possible values]`. Every spelling here is *accepted* by both
    /// surfaces; this only keeps each one advertising the name its own
    /// conventions produce.
    pub advertised_by_cli: bool,
}

impl SeedStrategy {
    /// Every spelling either surface accepts. Both parsers and both tests read
    /// this and nothing else.
    pub const SPELLINGS: &'static [SeedSpelling] = &[
        SeedSpelling {
            text: "all-chunks",
            value: SeedStrategy::AllChunks,
            advertised_by_cli: true,
        },
        SeedSpelling {
            text: "all_chunks",
            value: SeedStrategy::AllChunks,
            advertised_by_cli: false,
        },
        SeedSpelling {
            text: "centroid",
            value: SeedStrategy::Centroid,
            advertised_by_cli: true,
        },
    ];

    /// The spelling `groove graph --seed-strategy` falls back to. Beside the
    /// table so that a rename cannot leave the default naming a spelling that
    /// no longer exists — `the_default_spelling_is_one_of_the_accepted_ones`
    /// is what makes that true rather than intended.
    pub const DEFAULT_SPELLING: &'static str = "all-chunks";

    /// `None` for a spelling neither surface accepts.
    pub fn parse(text: &str) -> Option<Self> {
        Self::SPELLINGS
            .iter()
            .find(|s| s.text == text)
            .map(|s| s.value)
    }

    /// The accepted spellings, quoted, for an error message that lists them
    /// all rather than the subset one surface happens to advertise.
    pub fn accepted_spellings() -> String {
        Self::SPELLINGS
            .iter()
            .map(|s| format!("'{}'", s.text))
            .collect::<Vec<_>>()
            .join(" / ")
    }
}

/// `build_connection_graph` の入力オプション。MCP / CLI の両方から組み立てる。
#[derive(Debug, Clone)]
pub struct GraphOptions {
    /// BFS の最大深さ。1 = 直接近傍のみ、2 = 近傍の近傍まで。
    pub depth: u32,
    /// 各ノードから展開する近傍数。`0` を渡した場合は BFS 展開をスキップし
    /// seed ノードのみ返す (no-op 防御)。
    pub fan_out: u32,
    /// cos sim 換算値でのカットオフ (未満の候補は採用しない)。
    pub min_similarity: f32,
    pub seed_strategy: SeedStrategy,
    pub category: Option<String>,
    pub topic: Option<String>,
    /// 起点 path は常に除外される。そこに加えて除外したいパスを指定する。
    pub exclude_paths: Vec<String>,
    /// `true` のとき、同一 path からは 1 チャンクしか返さない (ドキュメント
    /// 単位で dedup)。`false` なら別チャンクは別ノードとして並ぶ (default)。
    pub dedup_by_path: bool,
    /// 近傍 KNN 段階で適用する品質スコア のしきい値。
    /// 0.0 ならフィルタ無効。seed ノードには適用しない (ユーザが明示指定した
    /// 起点なので低品質でも残す)。
    pub min_quality: f32,
    /// 起点ドキュメントから何チャンクをシードに使うか (BU-33)。
    /// `0` は 1 にクランプされる ([`clamp_max_seed_chunks`])。
    pub max_seed_chunks: u32,
    /// グラフ全体のノード数の上限 (BU-33)。KNN 実行回数もこれで縛られる。
    /// `0` はそのまま「空グラフを返せ」として扱う (`fan_out = 0` と同じ流儀)。
    pub max_nodes: u32,
}

/// 上限 (MCP スキーマでバリデーション) — サーバ側でも再度強制する。
pub const MAX_DEPTH: u32 = 3;
pub const MAX_FAN_OUT: u32 = 20;

pub const DEFAULT_DEPTH: u32 = 2;
pub const DEFAULT_FAN_OUT: u32 = 5;
pub const DEFAULT_MIN_SIMILARITY: f32 = 0.3;

/// シードに使う起点チャンク数の既定と天井 (BU-33)。
///
/// `depth` / `fan_out` の天井は 1 リクエストのコストを縛れていなかった。BFS の
/// シードは起点ドキュメントの**全チャンク**で、その数に上限が無かったためで、
/// 実測 (650 文書 / 9,419 チャンク / 1024 次元の実 KB) では最大の文書が 160
/// チャンク = depth 1 でも 160 回の KNN になっていた。
///
/// 既定 32 は実測分布から選んだ: チャンク数は median 13 / p90 26 / p99 43 /
/// max 160 で、32 を超える文書は **650 中 26 件 (4.0%)**。つまり打ち切りは
/// 少数の長大な文書でだけ起き、そこでは `truncated` が立つ。
pub const DEFAULT_MAX_SEED_CHUNKS: u32 = 32;
pub const MAX_SEED_CHUNKS_CEILING: u32 = 1000;

/// グラフのノード数の既定と天井 (BU-33)。
///
/// 各ノードは高々 1 回しか展開されないので `knn_queries <= total_nodes`。
/// したがってこの 1 つの上限が**応答サイズと KNN 実行回数の両方**を縛る。
///
/// 既定 100 の根拠 (同じ実 KB での実測): 1 KNN ≈ 72 ms、1 ノード ≈ 4 ms、
/// JSON は 1 ノード ≈ 665 バイト。`knn_queries <= total_nodes` なので最悪は
/// `100 × 72 + 100 × 4 = ` **7.6 秒 / 65 KiB (≒ 17k token)**。
///
/// 比較対象は 2 つあり、混同しないこと: 上限なしの既定 depth=2 は最大の文書で
/// **実測 1,997 ノード / 86.7 秒**だったので、**最悪値どうしで約 11 倍**、
/// 同じリクエストの**実測どうしでは 1.1 秒 = 約 79 倍**の短縮になる
/// (実測が最悪値よりずっと速いのは、予算が seed 展開の途中で埋まって
/// KNN が 14 回で止まるため)。
///
/// 天井 2000 は既定 depth=2 の実測最悪 (1,997 ノード) のすぐ上に置いた。
/// `groove graph --max-nodes 2000 --max-seed-chunks 1000` は、**2,000 ノード
/// 以下に収まる探索**については従来と同じ結果を `truncated: false` で再現する
/// (実測: depth 1 の 767 ノードと depth 2 の 1,997 ノードは完全一致)。
/// depth 3 は実測 3,682 ノードでこの天井を超えるので再現できない
/// — 天井での実行は 2,000 ノード / 約 59 秒 / `truncated: true` になる。
/// **2,000 ノードを超える結果は誰にも取得できない**、が意図した capability の
/// 縮小。
pub const DEFAULT_MAX_NODES: u32 = 100;
pub const MAX_NODES_CEILING: u32 = 2000;

/// `max_nodes` を天井へクランプする。`0` は「空グラフ」として尊重する。
pub fn clamp_max_nodes(n: u32) -> u32 {
    n.min(MAX_NODES_CEILING)
}

/// `max_seed_chunks` を `1..=MAX_SEED_CHUNKS_CEILING` にクランプする。
///
/// `max_nodes` と違って `0` を literal に扱わないのは、シード 0 件のグラフが
/// 「ドキュメントが見つからない」(`build_connection_graph` の bail) と区別
/// できない応答になるため。`max_nodes` は結果の大きさを縛るので「何も返すな」
/// は筋の通った要求だが、`max_seed_chunks` は問い合わせの主語を縛る。
pub fn clamp_max_seed_chunks(n: u32) -> u32 {
    n.clamp(1, MAX_SEED_CHUNKS_CEILING)
}

impl Default for GraphOptions {
    fn default() -> Self {
        Self {
            depth: DEFAULT_DEPTH,
            fan_out: DEFAULT_FAN_OUT,
            min_similarity: DEFAULT_MIN_SIMILARITY,
            seed_strategy: SeedStrategy::default(),
            category: None,
            topic: None,
            exclude_paths: Vec::new(),
            dedup_by_path: false,
            min_quality: crate::quality::DEFAULT_QUALITY_THRESHOLD,
            max_seed_chunks: DEFAULT_MAX_SEED_CHUNKS,
            max_nodes: DEFAULT_MAX_NODES,
        }
    }
}

/// なぜグラフが打ち切られたか (BU-33)。
///
/// 1 つの bool では「起点ドキュメントが削られた」(対処: `max_seed_chunks` を
/// 上げる。**`centroid` は対処にならない** — 上限は読み取りに掛かるので平均
/// されるのは同じ前半だけ) と「探索の先端が切れた」(対処: `max_nodes` を上げる /
/// `depth` を下げる / `min_similarity` を上げる。ここでは `centroid` が有効で、
/// シードノードが 1 個になり、その 1 個を除く予算が connection に回る) を区別できない。
/// **対処法は理由と一緒に運ぶ**。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TruncationReason {
    /// 起点ドキュメントのチャンク数が `max_seed_chunks` を超えた。
    SeedChunks,
    /// ノード数が `max_nodes` に達し、採用できる候補を捨てた / 未展開の
    /// フロンティアを残した。
    NodeBudget,
}

impl TruncationReason {
    /// JSON に出るのと**同じ**綴り。CLI の text 出力が `Debug` を使うと
    /// `NodeBudget` と `node_budget` の 2 通りが世に出てしまう。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SeedChunks => "seed_chunks",
            Self::NodeBudget => "node_budget",
        }
    }
}

impl std::fmt::Display for TruncationReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 発火した上限 1 件。`limit` はその理由自身の単位 (チャンク数 / ノード数)。
#[derive(Debug, Clone, Serialize)]
pub struct GraphTruncation {
    pub reason: TruncationReason,
    pub limit: u32,
    /// 何が失われ、どうすれば取り戻せるかを 1 文で。MCP には「続きを取る」
    /// カーソルが無いので、次に打つ手を応答自身が名指しする必要がある。
    pub detail: String,
}

/// 1 つのグラフノード。フラット配列 + `parent_id` で親子関係を表現する。
#[derive(Debug, Clone, Serialize)]
pub struct GraphNode {
    pub node_id: usize,
    pub parent_id: Option<usize>,
    pub depth: u32,
    pub chunk_id: i64,
    /// cos sim 換算 (0-1 の範囲、大きいほど類似)。seed ノードは 1.0。
    pub score: f32,
    pub path: String,
    pub heading: Option<String>,
    pub title: Option<String>,
    pub topic: Option<String>,
    /// `content` の先頭 200 文字 (LLM のトークン節約)。
    pub snippet: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct GraphStats {
    pub total_nodes: usize,
    /// BFS 中に「新ノードが追加された」最大深さ。指定 `depth` に必ず到達する
    /// わけではなく、候補が全て `min_similarity` や `visited` で枝刈られた場合は
    /// それより浅い値になる。
    pub max_depth_reached: u32,
    pub knn_queries: u32,
    pub duration_ms: u64,
    /// シードとして**採用された**起点ドキュメントのチャンク数 (BU-33)。
    ///
    /// 読み込んだ数ではない。`max_seed_chunks` と `max_nodes` は**どちらも**
    /// これを削りうる (`max_nodes = 8` に 32 チャンクなら 8、`max_nodes = 0` なら
    /// 0)。`seed_strategy = "centroid"` ではシードノードは 1 個だが、その 1 個に
    /// 畳み込まれたチャンク数を報告する。
    ///
    /// **「展開された数」ではない** — `fan_out = 0` や `depth = 0` では採用された
    /// シードが 1 つも展開されず `knn_queries = 0` になる。展開回数を見たいなら
    /// `knn_queries` を読むこと。
    pub seeds_used: u32,
}

#[derive(Debug, Clone, Serialize)]
pub struct ConnectionGraph {
    pub start_path: String,
    /// いずれかの上限が発火し、**何かが失われた**か (BU-33)。
    /// `truncation` が空でないことと同値。
    ///
    /// `node_budget` については安全側に倒している: 未展開のフロンティアを残して
    /// 打ち切った場合、その先に新規ノードが無かったとしても `true` になる。
    /// 「無かった」と確かめるには、上限が避けようとしているその KNN を
    /// 実行するしかないため。
    pub truncated: bool,
    /// 発火した上限の内訳。空 = 完全な探索。
    /// 同じ理由が 2 度入ることはない。
    pub truncation: Vec<GraphTruncation>,
    pub nodes: Vec<GraphNode>,
    pub stats: GraphStats,
}

// ---------------------------------------------------------------------------
// Core BFS
// ---------------------------------------------------------------------------

/// sqlite-vec の L2 distance を cos sim 近似値 (0-1) に変換する。
///
/// BGE 系の embedding は内部で L2 正規化されているため、正規化ベクトル
/// a, b 間の L2^2 と cos sim は `cos = 1 - l2^2 / 2` の関係にある。
/// `search_vec_candidates` が返す `SearchResult.score` は
/// `vec_chunks.v.distance` そのもの (L2 distance) なので、ここで近似変換する。
///
/// 万が一正規化されていない embedding が入っていた場合も、近傍ランク付けには
/// 使えるよう `0.0..=1.0` にクランプする (厳密性より安定性優先)。
fn distance_to_cos_sim(distance: f32) -> f32 {
    let cos = 1.0 - (distance * distance) / 2.0;
    cos.clamp(0.0, 1.0)
}

const SNIPPET_MAX_CHARS: usize = 200;

fn make_snippet(content: &str) -> String {
    let mut out = String::new();
    for (i, ch) in content.chars().enumerate() {
        if i >= SNIPPET_MAX_CHARS {
            out.push('…');
            break;
        }
        out.push(ch);
    }
    out
}

fn make_node(
    node_id: usize,
    parent_id: Option<usize>,
    depth: u32,
    chunk_id: i64,
    score: f32,
    r: &SearchResult,
) -> GraphNode {
    GraphNode {
        node_id,
        parent_id,
        depth,
        chunk_id,
        score,
        path: r.path.clone(),
        heading: r.heading.clone(),
        title: r.title.clone(),
        topic: r.topic.clone(),
        snippet: make_snippet(&r.content),
    }
}

/// embedding を L2 正規化する (in-place)。ゼロベクトルならそのまま。
fn l2_normalize(v: &mut [f32]) {
    let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        for x in v.iter_mut() {
            *x /= norm;
        }
    }
}

/// 打ち切り理由を 1 件記録する。**同じ理由は 1 度だけ**。
///
/// node 予算は 2 箇所 (候補の採用時と、未展開のフロンティアを残して抜ける時) で
/// 検出しうるので、素直に push すると「2 回起きた」と読める応答になる。
fn push_truncation(
    out: &mut Vec<GraphTruncation>,
    reason: TruncationReason,
    limit: u32,
    detail: String,
) {
    if !out.iter().any(|t| t.reason == reason) {
        out.push(GraphTruncation {
            reason,
            limit,
            detail,
        });
    }
}

/// node 予算が発火した時の説明文。理由と対処を一緒に運ぶ。
///
/// 対処を「予算を上げる」だけにしないのが要点。BFS は幅優先なので、予算は先に
/// 浅い層で使い切られる — 実測でも、160 チャンクの文書は既定予算だと depth 1 の
/// 展開だけで埋まり、`depth` を 2 にも 3 にしても結果が変わらなかった。
/// **深く行きたい場合の正解は予算増ではなく、シードを減らすこと** (`centroid` /
/// 低い `max_seed_chunks` / 低い `fan_out`)。
fn node_budget_detail(limit: u32) -> String {
    // 上限に達している時に「max_nodes を上げろ」とだけ言うと、クライアントは
    // 通らない再試行をする。天井を併記して、そこが行き止まりだと分かるようにする。
    let raise = if limit >= MAX_NODES_CEILING {
        format!(
            "max_nodes is already at its ceiling of {MAX_NODES_CEILING}, so a larger graph is not available"
        )
    } else {
        format!("raise max_nodes (ceiling {MAX_NODES_CEILING})")
    };
    format!(
        "the graph hit its node budget of {limit}; the walk stopped before the frontier was \
         exhausted. To get more nodes, {raise}. To spend the budget on depth instead of \
         breadth, use seed_strategy \"centroid\" or lower max_seed_chunks / fan_out. To get \
         fewer but more relevant nodes, raise min_similarity, set dedup_by_path, or use the \
         category / topic / exclude_paths filters."
    )
}

/// 起点 `start_path` から BFS で Connection Graph を構築する。
///
/// # 上限 (BU-33)
///
/// 2 つの上限がコストを縛る。どちらも決定的 (壁時計に依存しない):
///
/// - `max_seed_chunks` — 起点ドキュメントから読むチャンク数。**SQL の `LIMIT`**
///   なので、読まれなかった行は materialize もされない。
/// - `max_nodes` — グラフ全体のノード数。各ノードは高々 1 回しか queue に入らず、
///   queue から出た時に高々 1 回 KNN を撃つので
///   **`knn_queries <= total_nodes <= max_nodes`** が成り立つ。この 1 つの上限が
///   応答サイズと KNN 実行回数の両方を縛る。
///
/// 縛れて**いない**もの: 1 回の KNN 自体のコスト。`vec0` に ANN 索引は無く
/// 総当たりなので、KNN 1 回は KB のチャンク数 × 次元に比例する。つまりこの上限が
/// 保証するのは「1 回の graph 呼び出しは検索 `max_nodes` 回分の仕事で収まる」
/// という**相対的な**上界であって、絶対的な秒数ではない。実測値は
/// [`DEFAULT_MAX_NODES`] の doc を参照。
pub fn build_connection_graph(
    db: &Database,
    start_path: &str,
    opts: &GraphOptions,
) -> Result<ConnectionGraph> {
    let started = Instant::now();

    // 0. (BU-05) `exclude_paths` was the one caller-supplied list that AU-17's
    // 64-entries × 1 KiB bound never covered: `search` validates all three of
    // its lists, while this one went straight into a `HashSet` the BFS
    // consults on every visit. Checked here rather than in the MCP handler so
    // that `groove graph --exclude-paths` is bounded by the same rule, and before
    // the seed lookup so an oversized request costs nothing.
    crate::server::validate_filter_list("exclude_paths", &opts.exclude_paths)?;

    let mut truncation: Vec<GraphTruncation> = Vec::new();

    // 1. 起点シードを取得。存在しなければ明確にエラー。
    //
    // (BU-33) 上限は SQL の LIMIT に降ろす (`chunks_for_path_capped`)。
    let seed_cap = clamp_max_seed_chunks(opts.max_seed_chunks);
    let (seeds, more_chunks) = db.chunks_for_path_capped(start_path, seed_cap)?;
    if seeds.is_empty() {
        anyhow::bail!(
            "document not found (no chunks for path): {start_path}. \
             Run `groove index` to (re)index the knowledge base."
        );
    }
    if more_chunks {
        // 対処に `centroid` を挙げないのが要点。上限は**読み取り**に掛かるので、
        // centroid に切り替えても平均されるのは同じ前半だけで、落ちたチャンクは
        // 戻ってこない (`max_nodes` の方の対処とは違う)。
        let detail = match opts.seed_strategy {
            // 「読んだ」までしか言わない。実際に展開された数は node 予算次第で
            // さらに減り、それは `node_budget` の entry と `stats.seeds_used` が
            // 受け持つ。ここで「展開した」と書くと、両方が効いた時に嘘になる。
            SeedStrategy::AllChunks => format!(
                "the start document has more than {seed_cap} chunks; only its first {seed_cap} \
                 were read, so only those were eligible as BFS seeds (stats.seeds_used reports \
                 how many were admitted). Raise max_seed_chunks to cover more of it -- \
                 seed_strategy \"centroid\" leaves all but one node of the budget for \
                 connections but averages the same capped prefix, so it does not recover the \
                 chunks dropped here."
            ),
            SeedStrategy::Centroid => format!(
                "the start document has more than {seed_cap} chunks; the centroid is the average \
                 of its first {seed_cap} chunks, not of the whole document. \
                 Raise max_seed_chunks for a centroid over more of it."
            ),
        };
        push_truncation(
            &mut truncation,
            TruncationReason::SeedChunks,
            seed_cap,
            detail,
        );
    }
    let seeds_fetched = seeds.len() as u32;

    let mut visited: HashSet<i64> = HashSet::new();
    // 起点 path と exclude_paths、dedup_by_path=true の場合の「既出 path」を
    // 1 つの HashSet で管理する (O(1) 検索)。
    let mut visited_paths: HashSet<String> = HashSet::new();
    visited_paths.insert(start_path.to_string());
    for p in &opts.exclude_paths {
        visited_paths.insert(p.clone());
    }
    let mut nodes: Vec<GraphNode> = Vec::new();
    // BFS queue: 各エントリは (親 node_id, 展開用 embedding, current_depth)
    let mut queue: VecDeque<(usize, Vec<f32>, u32)> = VecDeque::new();

    // (BU-33) シードもノード予算の対象。既定では seed 上限 (32) < node 予算 (100)
    // なので seed が予算を食い尽くすことは無いが、`--max-nodes 8` のような明示的な
    // 要求では起こりうる。
    let max_nodes = clamp_max_nodes(opts.max_nodes) as usize;

    // 実際に探索の起点になったチャンク数。読み込んだ数 (`seeds_fetched`) ではない
    // — node 予算が seed 段で効くと両者はずれる。
    let mut seeds_used: u32 = 0;

    // 2. seed_strategy に応じてシードを追加。
    match opts.seed_strategy {
        SeedStrategy::AllChunks => {
            for (chunk_id, embedding, r) in seeds {
                if nodes.len() >= max_nodes {
                    push_truncation(
                        &mut truncation,
                        TruncationReason::NodeBudget,
                        max_nodes as u32,
                        node_budget_detail(max_nodes as u32),
                    );
                    break;
                }
                let node_id = nodes.len();
                nodes.push(make_node(node_id, None, 0, chunk_id, 1.0, &r));
                visited.insert(chunk_id);
                queue.push_back((node_id, embedding, 0));
                seeds_used += 1;
            }
        }
        SeedStrategy::Centroid => {
            // 単一 centroid ノードを 1 個だけ作り、最初の seed チャンクを代表に
            // 据える (path/heading/title のメタは代表チャンクから取る)。
            // 全シードチャンクは visited 登録して BFS 対象から除外する。
            let dim = seeds[0].1.len();
            let mut sum = vec![0f32; dim];
            for (_, emb, _) in &seeds {
                for (i, v) in emb.iter().enumerate() {
                    sum[i] += *v;
                }
            }
            for v in &mut sum {
                *v /= seeds.len() as f32;
            }
            // 単位ベクトルの平均は一般に norm < 1 になる。L2 正規化してから
            // KNN に使わないと `distance_to_cos_sim` の前提 (両辺 unit norm) が
            // 崩れて score 値が誤解を招く。
            l2_normalize(&mut sum);
            for (cid, _, _) in &seeds {
                visited.insert(*cid);
            }
            if nodes.is_empty() && max_nodes == 0 {
                // `max_nodes = 0` は literal に「空グラフ」。centroid でも同じ。
                // シードノードが出ない以上、探索の起点になったチャンクは 0 個。
                push_truncation(
                    &mut truncation,
                    TruncationReason::NodeBudget,
                    0,
                    node_budget_detail(0),
                );
            } else {
                let (chunk_id, _, rep) = &seeds[0];
                let node_id = nodes.len();
                nodes.push(make_node(node_id, None, 0, *chunk_id, 1.0, rep));
                queue.push_back((node_id, sum, 0));
                // centroid はノード 1 個だが、その 1 個に畳み込まれたチャンク数を
                // 報告する (= 何チャンク分が探索の起点になったか)。
                seeds_used = seeds_fetched;
            }
        }
    }

    // 3. BFS 本体。
    let mut knn_queries: u32 = 0;
    let mut max_depth_reached: u32 = 0;

    // fan_out=0 は「seed のみ返す no-op」として扱う。sqlite-vec に k=0 を
    // 渡すとエラーになるので、ここで短絡する。
    if opts.fan_out == 0 {
        return Ok(ConnectionGraph {
            start_path: start_path.to_string(),
            truncated: !truncation.is_empty(),
            truncation,
            stats: GraphStats {
                total_nodes: nodes.len(),
                max_depth_reached,
                knn_queries,
                duration_ms: started.elapsed().as_millis() as u64,
                seeds_used,
            },
            nodes,
        });
    }

    while let Some((parent_id, embedding, current_depth)) = queue.pop_front() {
        if current_depth >= opts.depth {
            continue;
        }

        // (BU-33) ここが「予算切れ」の判定点。深さ判定より **後** に置くのが要点で、
        // どうせ展開されないエントリで `truncated` を立てると偽陽性になる。
        //
        // `break` せず走査を続けるのは、残りの queue に展開対象が 1 つも無い
        // (= 何も失われていない) 場合を区別するため。KNN は撃たないので安い。
        if nodes.len() >= max_nodes {
            push_truncation(
                &mut truncation,
                TruncationReason::NodeBudget,
                max_nodes as u32,
                node_budget_detail(max_nodes as u32),
            );
            continue;
        }

        // 少し余分に取って visited / min_similarity で刈り込む。
        let fetch_k = opts.fan_out.saturating_mul(2).max(opts.fan_out);
        let candidates = db
            .search_vec_candidates(
                &embedding,
                fetch_k,
                &crate::db::SearchFilters {
                    category: opts.category.as_deref(),
                    topic: opts.topic.as_deref(),
                    min_quality: opts.min_quality,
                    ..Default::default()
                },
            )
            .with_context(|| format!("knn failed at depth {current_depth}"))?;
        knn_queries += 1;

        let mut added = 0u32;
        for (chunk_id, r) in candidates {
            if added >= opts.fan_out {
                break;
            }
            if visited.contains(&chunk_id) {
                continue;
            }
            if visited_paths.contains(&r.path) {
                continue;
            }
            let sim = distance_to_cos_sim(r.score);
            if sim < opts.min_similarity {
                continue;
            }

            // (BU-33) ここまで来た候補は全フィルタを通っている = 予算が無ければ
            // 必ず採用されていた。したがってこの分岐は**確実な損失**であり、
            // `truncated` は保守的でなく厳密。`get_chunk_embedding` の手前に置くのは
            // 捨てる行のために SQL を撃たないため。
            if nodes.len() >= max_nodes {
                push_truncation(
                    &mut truncation,
                    TruncationReason::NodeBudget,
                    max_nodes as u32,
                    node_budget_detail(max_nodes as u32),
                );
                break;
            }

            visited.insert(chunk_id);
            if opts.dedup_by_path {
                visited_paths.insert(r.path.clone());
            }
            let Some(next_embedding) = db.get_chunk_embedding(chunk_id)? else {
                // vec_chunks に存在しない chunk_id は稀 (一貫性破壊) なのでスキップ
                continue;
            };
            let new_depth = current_depth + 1;
            max_depth_reached = max_depth_reached.max(new_depth);
            let node_id = nodes.len();
            nodes.push(make_node(
                node_id,
                Some(parent_id),
                new_depth,
                chunk_id,
                sim,
                &r,
            ));
            queue.push_back((node_id, next_embedding, new_depth));
            added += 1;
        }
    }

    let duration_ms = started.elapsed().as_millis() as u64;
    if let Some(t) = truncation.first() {
        // BU-31 と同じ流儀で、サーバ側にも 1 行だけ痕跡を残す (1 呼び出し 1 行)。
        // stderr は CLI 出力規約どおり診断の置き場。
        tracing::warn!(
            start_path,
            reason = ?t.reason,
            limit = t.limit,
            total_nodes = nodes.len(),
            "connection graph truncated"
        );
    }
    Ok(ConnectionGraph {
        start_path: start_path.to_string(),
        truncated: !truncation.is_empty(),
        truncation,
        stats: GraphStats {
            total_nodes: nodes.len(),
            max_depth_reached,
            knn_queries,
            duration_ms,
            seeds_used,
        },
        nodes,
    })
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// `DEFAULT_SPELLING` is a string, so nothing but this stops it naming a
    /// spelling the table no longer has — at which point `groove graph` would
    /// fail to start on its own default.
    #[test]
    fn the_default_spelling_is_one_of_the_accepted_ones() {
        assert_eq!(
            SeedStrategy::parse(SeedStrategy::DEFAULT_SPELLING),
            Some(SeedStrategy::default()),
            "DEFAULT_SPELLING must parse, and must mean the default strategy"
        );
    }

    /// Every strategy needs a spelling each surface can advertise, or `--help`
    /// would silently stop offering one of them.
    #[test]
    fn every_strategy_has_a_spelling_the_cli_advertises() {
        for value in [SeedStrategy::AllChunks, SeedStrategy::Centroid] {
            assert!(
                SeedStrategy::SPELLINGS
                    .iter()
                    .any(|s| s.value == value && s.advertised_by_cli),
                "{value:?} has no spelling `groove graph --help` would list"
            );
        }
    }

    /// 384 次元 dummy embedding。全要素を `val` で埋める。vec0 の L2 距離は
    /// 全要素同一ベクトル間で `sqrt(dim) * |a - b|` になるので、`val` を細かく
    /// 調整することで近傍関係を設計できる。
    fn dummy_embedding(val: f32) -> Vec<f32> {
        vec![val; 384]
    }

    fn setup_db() -> Database {
        let db = Database::open_in_memory().unwrap();
        db.verify_embedding_meta("bge-small-en-v1.5", 384).unwrap();
        db
    }

    /// doc + 1 chunk を挿入する helper。chunk_index=0。
    fn insert_doc_with_chunk(db: &Database, path: &str, heading: &str, content: &str, val: f32) {
        let doc_id = db
            .upsert_document(
                path,
                Some(heading),
                None,
                None,
                None,
                &[],
                None,
                &format!("h-{path}"),
                0,
            )
            .unwrap();
        db.insert_chunk(
            doc_id,
            0,
            Some(heading),
            None,
            content,
            None,
            &dummy_embedding(val),
            1.0,
        )
        .unwrap();
    }

    #[test]
    fn test_graph_start_path_not_found() {
        let db = setup_db();
        let err = build_connection_graph(&db, "does/not/exist.md", &GraphOptions::default())
            .expect_err("must fail");
        assert!(err.to_string().contains("document not found"));
    }

    #[test]
    fn test_graph_two_hop_bfs() {
        let db = setup_db();
        // 起点: s.md (val=0.10)
        // 1-hop 候補: a1.md(0.11), a2.md(0.12), a3.md(0.13)
        // a1 の 2-hop: b1.md(0.111)
        insert_doc_with_chunk(&db, "s.md", "seed", "seed body", 0.10);
        insert_doc_with_chunk(&db, "a1.md", "a1", "a1 body", 0.11);
        insert_doc_with_chunk(&db, "a2.md", "a2", "a2 body", 0.12);
        insert_doc_with_chunk(&db, "a3.md", "a3", "a3 body", 0.13);
        insert_doc_with_chunk(&db, "b1.md", "b1", "b1 body", 0.111);

        let opts = GraphOptions {
            depth: 2,
            fan_out: 3,
            min_similarity: 0.0,
            ..Default::default()
        };
        let g = build_connection_graph(&db, "s.md", &opts).unwrap();
        // Seed 1 + 1-hop 3 + 2-hop (少なくとも 1) = 5 以上
        assert!(g.nodes.len() >= 5, "got {} nodes", g.nodes.len());
        // seed node
        assert_eq!(g.nodes[0].depth, 0);
        assert_eq!(g.nodes[0].parent_id, None);
        assert_eq!(g.nodes[0].path, "s.md");
        assert_eq!(g.nodes[0].score, 1.0);
        // 起点 path は seed 以外に重複しない
        let dup = g
            .nodes
            .iter()
            .filter(|n| n.path == "s.md" && n.depth > 0)
            .count();
        assert_eq!(dup, 0, "start path must not reappear at depth>0");
        // すべての非 seed ノードは parent_id が既存 node_id を指す
        for n in g.nodes.iter().filter(|n| n.depth > 0) {
            let pid = n.parent_id.expect("non-seed has parent");
            assert!(pid < g.nodes.len(), "parent_id out of range");
        }
        assert!(g.stats.max_depth_reached >= 1);
        assert!(g.stats.knn_queries >= 1);
    }

    #[test]
    fn test_graph_respects_depth_limit() {
        let db = setup_db();
        insert_doc_with_chunk(&db, "s.md", "s", "s body", 0.10);
        insert_doc_with_chunk(&db, "a.md", "a", "a body", 0.11);
        insert_doc_with_chunk(&db, "b.md", "b", "b body", 0.111);

        let opts = GraphOptions {
            depth: 1,
            fan_out: 5,
            min_similarity: 0.0,
            ..Default::default()
        };
        let g = build_connection_graph(&db, "s.md", &opts).unwrap();
        for n in &g.nodes {
            assert!(n.depth <= 1, "depth must not exceed 1, got {}", n.depth);
        }
        assert_eq!(g.stats.max_depth_reached, 1);
    }

    #[test]
    fn test_graph_dedupes_visited() {
        let db = setup_db();
        insert_doc_with_chunk(&db, "s.md", "s", "s body", 0.10);
        insert_doc_with_chunk(&db, "a.md", "a", "a body", 0.11);
        insert_doc_with_chunk(&db, "b.md", "b", "b body", 0.12);

        let opts = GraphOptions {
            depth: 3,
            fan_out: 5,
            min_similarity: 0.0,
            ..Default::default()
        };
        let g = build_connection_graph(&db, "s.md", &opts).unwrap();
        let mut chunk_ids: Vec<i64> = g.nodes.iter().map(|n| n.chunk_id).collect();
        chunk_ids.sort();
        let unique_len = {
            let mut c = chunk_ids.clone();
            c.dedup();
            c.len()
        };
        assert_eq!(
            chunk_ids.len(),
            unique_len,
            "chunk ids must be unique across nodes"
        );
    }

    #[test]
    fn test_graph_respects_min_similarity() {
        let db = setup_db();
        // 起点 0.0 と、値が大きく乖離した候補 (L2 distance 大 → cos sim 低)
        insert_doc_with_chunk(&db, "s.md", "s", "s body", 0.0);
        insert_doc_with_chunk(&db, "close.md", "c", "c body", 0.001);
        // 値 0.5 だと 384 次元の L2 がかなり大きく cos sim が 0 にクランプされる
        insert_doc_with_chunk(&db, "far.md", "f", "f body", 0.5);

        let opts = GraphOptions {
            depth: 1,
            fan_out: 10,
            min_similarity: 0.9,
            ..Default::default()
        };
        let g = build_connection_graph(&db, "s.md", &opts).unwrap();
        // far.md は閾値で切られるはず
        assert!(
            !g.nodes.iter().any(|n| n.path == "far.md"),
            "far.md should be pruned by min_similarity"
        );
    }

    #[test]
    fn test_graph_fan_out_limit() {
        let db = setup_db();
        insert_doc_with_chunk(&db, "s.md", "s", "s body", 0.0);
        for i in 1..=10 {
            insert_doc_with_chunk(&db, &format!("a{i}.md"), "a", "a body", 0.001 * i as f32);
        }
        let opts = GraphOptions {
            depth: 1,
            fan_out: 3,
            min_similarity: 0.0,
            ..Default::default()
        };
        let g = build_connection_graph(&db, "s.md", &opts).unwrap();
        // seed 1 + 最大 3
        assert!(
            g.nodes.len() <= 4,
            "fan_out=3 は depth=1 で最大 4 ノード、got {}",
            g.nodes.len()
        );
    }

    #[test]
    fn test_graph_excludes_paths() {
        let db = setup_db();
        insert_doc_with_chunk(&db, "s.md", "s", "s body", 0.0);
        insert_doc_with_chunk(&db, "blocked.md", "b", "b body", 0.001);
        insert_doc_with_chunk(&db, "allowed.md", "a", "a body", 0.002);

        let opts = GraphOptions {
            depth: 1,
            fan_out: 5,
            min_similarity: 0.0,
            exclude_paths: vec!["blocked.md".into()],
            ..Default::default()
        };
        let g = build_connection_graph(&db, "s.md", &opts).unwrap();
        assert!(!g.nodes.iter().any(|n| n.path == "blocked.md"));
        assert!(g.nodes.iter().any(|n| n.path == "allowed.md"));
    }

    #[test]
    fn test_graph_snippet_is_truncated() {
        let db = setup_db();
        insert_doc_with_chunk(&db, "s.md", "s", &"x".repeat(500), 0.0);

        let opts = GraphOptions {
            depth: 0,
            fan_out: 1,
            min_similarity: 0.0,
            ..Default::default()
        };
        let g = build_connection_graph(&db, "s.md", &opts).unwrap();
        assert_eq!(g.nodes.len(), 1);
        // snippet は末尾に '…' が付いて 201 文字 (200 chars + '…')
        assert!(g.nodes[0].snippet.ends_with('…'));
        assert!(g.nodes[0].snippet.chars().count() <= SNIPPET_MAX_CHARS + 1);
    }

    #[test]
    fn test_graph_centroid_seed_single_node() {
        let db = setup_db();
        // 1 ドキュメントに 2 チャンク (centroid テスト用)
        let doc_id = db
            .upsert_document("s.md", Some("T"), None, None, None, &[], None, "hs", 0)
            .unwrap();
        db.insert_chunk(
            doc_id,
            0,
            Some("h1"),
            None,
            "c1",
            None,
            &dummy_embedding(0.0),
            1.0,
        )
        .unwrap();
        db.insert_chunk(
            doc_id,
            1,
            Some("h2"),
            None,
            "c2",
            None,
            &dummy_embedding(0.1),
            1.0,
        )
        .unwrap();
        insert_doc_with_chunk(&db, "x.md", "x", "x", 0.05);

        let opts = GraphOptions {
            depth: 1,
            fan_out: 3,
            min_similarity: 0.0,
            seed_strategy: SeedStrategy::Centroid,
            ..Default::default()
        };
        let g = build_connection_graph(&db, "s.md", &opts).unwrap();
        let seed_count = g.nodes.iter().filter(|n| n.depth == 0).count();
        assert_eq!(seed_count, 1, "centroid seed should be exactly 1 node");
    }

    #[test]
    fn test_graph_serializable_to_json() {
        let db = setup_db();
        insert_doc_with_chunk(&db, "s.md", "s", "s body", 0.0);
        let g = build_connection_graph(&db, "s.md", &GraphOptions::default()).unwrap();
        let json = serde_json::to_string(&g).expect("must serialize");
        assert!(json.contains("\"start_path\""));
        assert!(json.contains("\"nodes\""));
        assert!(json.contains("\"stats\""));
    }

    #[test]
    fn test_graph_fan_out_zero_returns_seeds_only() {
        let db = setup_db();
        insert_doc_with_chunk(&db, "s.md", "s", "s body", 0.0);
        insert_doc_with_chunk(&db, "a.md", "a", "a body", 0.01);
        let opts = GraphOptions {
            depth: 2,
            fan_out: 0,
            min_similarity: 0.0,
            ..Default::default()
        };
        let g = build_connection_graph(&db, "s.md", &opts).unwrap();
        assert_eq!(
            g.nodes.len(),
            1,
            "only seed should be present when fan_out=0"
        );
        assert_eq!(g.stats.knn_queries, 0);
        assert_eq!(g.stats.max_depth_reached, 0);
    }

    /// (BU-05) `exclude_paths` is subject to the same bound as `search`'s
    /// filter lists.
    ///
    /// It was the one caller-supplied list AU-17 never covered: `search`
    /// checks `path_globs` / `tags_any` / `tags_all` against 64 entries × 1
    /// KiB, while this one went straight into the `HashSet` the BFS consults
    /// on every visit. The start document exists here, so an error can only
    /// come from the bound — not from a missing seed.
    #[test]
    fn exclude_paths_is_bounded_like_the_search_filter_lists() {
        let db = setup_db();
        insert_doc_with_chunk(&db, "s.md", "S", "seed", 0.1);

        let over = GraphOptions {
            depth: 0,
            fan_out: 0,
            exclude_paths: (0..65).map(|i| format!("junk{i}.md")).collect(),
            ..Default::default()
        };
        let err = build_connection_graph(&db, "s.md", &over)
            .expect_err("65 exclude_paths must be rejected");
        let msg = err.to_string();
        assert!(
            msg.contains("exclude_paths") && msg.contains("too many entries"),
            "the error must name the offending list and why: {msg}"
        );

        // Control: at the limit it still works, so the bound is a bound and
        // not an accidental "any exclude_paths fails".
        let at_limit = GraphOptions {
            depth: 0,
            fan_out: 0,
            exclude_paths: (0..64).map(|i| format!("junk{i}.md")).collect(),
            ..Default::default()
        };
        build_connection_graph(&db, "s.md", &at_limit)
            .expect("64 exclude_paths is within the limit and must still work");

        // A single oversized entry is rejected too — 64 short strings and one
        // 1 MiB string are not the same request.
        let long = GraphOptions {
            depth: 0,
            fan_out: 0,
            exclude_paths: vec!["x".repeat(2048)],
            ..Default::default()
        };
        let err = build_connection_graph(&db, "s.md", &long)
            .expect_err("an oversized exclude_paths entry must be rejected");
        assert!(
            err.to_string().contains("too large"),
            "per-entry size must be bounded as well: {err}"
        );
    }

    #[test]
    fn test_graph_dedup_by_path_collapses_same_doc_chunks() {
        let db = setup_db();
        // start doc
        insert_doc_with_chunk(&db, "s.md", "s", "s body", 0.0);
        // same-path で 2 チャンクを持つ近傍ドキュメント
        let doc_id = db
            .upsert_document("a.md", Some("T"), None, None, None, &[], None, "ha", 0)
            .unwrap();
        db.insert_chunk(
            doc_id,
            0,
            Some("h1"),
            None,
            "c1",
            None,
            &dummy_embedding(0.001),
            1.0,
        )
        .unwrap();
        db.insert_chunk(
            doc_id,
            1,
            Some("h2"),
            None,
            "c2",
            None,
            &dummy_embedding(0.002),
            1.0,
        )
        .unwrap();

        // dedup_by_path=true なら a.md は 1 つだけ
        let opts_dedup = GraphOptions {
            depth: 1,
            fan_out: 5,
            min_similarity: 0.0,
            dedup_by_path: true,
            ..Default::default()
        };
        let g = build_connection_graph(&db, "s.md", &opts_dedup).unwrap();
        let a_count = g.nodes.iter().filter(|n| n.path == "a.md").count();
        assert_eq!(a_count, 1, "dedup_by_path=true should collapse a.md");

        // dedup_by_path=false なら a.md の複数チャンクが並ぶ
        let opts_nodedup = GraphOptions {
            depth: 1,
            fan_out: 5,
            min_similarity: 0.0,
            dedup_by_path: false,
            ..Default::default()
        };
        let g = build_connection_graph(&db, "s.md", &opts_nodedup).unwrap();
        let a_count = g.nodes.iter().filter(|n| n.path == "a.md").count();
        assert!(
            a_count >= 2,
            "dedup_by_path=false should allow multiple chunks from a.md, got {a_count}"
        );
    }

    #[test]
    fn test_l2_normalize() {
        let mut v = vec![3.0f32, 4.0, 0.0];
        l2_normalize(&mut v);
        let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-6);
        assert!((v[0] - 0.6).abs() < 1e-6);
        assert!((v[1] - 0.8).abs() < 1e-6);

        // ゼロベクトルはそのまま
        let mut z = vec![0.0f32, 0.0, 0.0];
        l2_normalize(&mut z);
        assert_eq!(z, vec![0.0, 0.0, 0.0]);
    }

    #[test]
    fn test_distance_to_cos_sim_clamps() {
        assert!((distance_to_cos_sim(0.0) - 1.0).abs() < 1e-6);
        // sqrt(2) distance is orthogonal (cos=0) for normalized vectors
        let orth = distance_to_cos_sim(2f32.sqrt());
        assert!(orth.abs() < 1e-6, "got {orth}");
        // 超過は 0 にクランプ
        assert_eq!(distance_to_cos_sim(100.0), 0.0);
    }

    // =======================================================================
    // BU-33: the walk has to be bounded, and the bound has to be visible
    // =======================================================================

    /// Insert one document carrying `n` chunks, spaced `step` apart in
    /// embedding space so the neighbour ranking is deterministic.
    fn insert_doc_with_n_chunks(db: &Database, path: &str, n: usize, base: f32, step: f32) {
        let doc_id = db
            .upsert_document(
                path,
                Some(path),
                None,
                None,
                None,
                &[],
                None,
                &format!("h-{path}"),
                0,
            )
            .unwrap();
        for i in 0..n {
            db.insert_chunk(
                doc_id,
                i as i32,
                Some(&format!("h{i}")),
                None,
                &format!("chunk {i} of {path}"),
                None,
                &dummy_embedding(base + step * i as f32),
                1.0,
            )
            .unwrap();
        }
    }

    fn reasons(g: &ConnectionGraph) -> Vec<TruncationReason> {
        g.truncation.iter().map(|t| t.reason).collect()
    }

    /// The two invariants the whole design rests on, asserted together because
    /// either one alone can hold while the walk is still unbounded:
    /// `knn_queries <= total_nodes <= max_nodes`.
    fn assert_budget_invariants(g: &ConnectionGraph, max_nodes: u32) {
        assert!(
            g.stats.total_nodes <= max_nodes as usize,
            "total_nodes {} exceeded max_nodes {max_nodes}",
            g.stats.total_nodes
        );
        assert!(
            g.stats.knn_queries as usize <= g.stats.total_nodes,
            "knn_queries {} exceeded total_nodes {} -- a node was expanded more than once",
            g.stats.knn_queries,
            g.stats.total_nodes
        );
    }

    #[test]
    fn clamping_keeps_the_budgets_inside_their_ceilings() {
        assert_eq!(clamp_max_nodes(u32::MAX), MAX_NODES_CEILING);
        assert_eq!(clamp_max_nodes(7), 7);
        // 0 is honoured literally: "return nothing" is a coherent request.
        assert_eq!(clamp_max_nodes(0), 0);

        assert_eq!(clamp_max_seed_chunks(u32::MAX), MAX_SEED_CHUNKS_CEILING);
        assert_eq!(clamp_max_seed_chunks(7), 7);
        // 0 is NOT honoured: a seedless graph is indistinguishable from the
        // "document not found" bail.
        assert_eq!(clamp_max_seed_chunks(0), 1);
    }

    #[test]
    fn the_seed_cap_bounds_the_seed_phase_and_says_so() {
        let db = setup_db();
        insert_doc_with_n_chunks(&db, "big.md", 20, 0.10, 0.0001);
        insert_doc_with_chunk(&db, "n1.md", "n1", "n1 body", 0.20);

        let opts = GraphOptions {
            depth: 1,
            fan_out: 2,
            min_similarity: 0.0,
            max_seed_chunks: 5,
            ..Default::default()
        };
        let g = build_connection_graph(&db, "big.md", &opts).unwrap();

        assert_eq!(
            g.stats.seeds_used, 5,
            "only the capped prefix may be seeded"
        );
        assert_eq!(
            g.nodes.iter().filter(|n| n.depth == 0).count(),
            5,
            "seed nodes must match seeds_used"
        );
        assert!(g.truncated);
        assert_eq!(reasons(&g), vec![TruncationReason::SeedChunks]);
        assert_eq!(g.truncation[0].limit, 5);
        assert!(
            g.truncation[0].detail.contains("max_seed_chunks"),
            "the detail must name the knob that lifts the bound: {}",
            g.truncation[0].detail
        );
        // The cap is on the *read*, so switching to centroid averages the same
        // capped prefix. Offering it as the way to recover the dropped chunks
        // would be advice that cannot work.
        assert!(
            g.truncation[0].detail.contains("does not recover"),
            "the remedy must not promise centroid recovers the dropped chunks: {}",
            g.truncation[0].detail
        );
        // This entry speaks for the read only. How many of those chunks were
        // actually expanded also depends on the node budget, so claiming they
        // were expanded here would be false whenever both bounds fire.
        assert!(
            !g.truncation[0].detail.contains("were expanded"),
            "the seed entry must not claim what the node budget decides: {}",
            g.truncation[0].detail
        );
    }

    /// `seeds_used` counts the chunks admitted as seeds, not the chunks read
    /// and not the seeds expanded. The node budget can cut the seed phase
    /// short, so the fetched count would overstate it; `fan_out = 0` admits
    /// seeds that are never expanded, so the expansion count would understate
    /// it.
    #[test]
    fn seeds_used_counts_admitted_seeds_not_fetched_ones() {
        let db = setup_db();
        insert_doc_with_n_chunks(&db, "big.md", 20, 0.10, 0.0001);
        insert_doc_with_chunk(&db, "n1.md", "n1", "n1 body", 0.20);

        // seed cap 16 would allow 16 seeds, but the node budget stops at 6.
        let opts = GraphOptions {
            depth: 1,
            fan_out: 2,
            min_similarity: 0.0,
            max_seed_chunks: 16,
            max_nodes: 6,
            ..Default::default()
        };
        let g = build_connection_graph(&db, "big.md", &opts).unwrap();

        assert_eq!(g.stats.total_nodes, 6);
        assert_eq!(
            g.stats.seeds_used, 6,
            "6 of the 16 fetched chunks actually seeded the walk"
        );
        assert_eq!(
            g.nodes.iter().filter(|n| n.depth == 0).count(),
            g.stats.seeds_used as usize,
            "seeds_used must equal the number of depth-0 nodes for all_chunks"
        );

        // Admitted, not expanded: fan_out = 0 short-circuits before any KNN,
        // so the two counts come apart and only "admitted" stays true.
        let g = build_connection_graph(
            &db,
            "big.md",
            &GraphOptions {
                fan_out: 0,
                max_seed_chunks: 4,
                ..opts.clone()
            },
        )
        .unwrap();
        assert_eq!(g.stats.knn_queries, 0, "fan_out = 0 expands nothing");
        assert_eq!(
            g.stats.seeds_used, 4,
            "4 chunks were admitted as seeds even though none was expanded"
        );
    }

    #[test]
    fn a_document_under_the_seed_cap_is_not_reported_as_truncated() {
        let db = setup_db();
        insert_doc_with_n_chunks(&db, "small.md", 3, 0.10, 0.0001);
        insert_doc_with_chunk(&db, "n1.md", "n1", "n1 body", 0.20);

        let opts = GraphOptions {
            depth: 1,
            fan_out: 2,
            min_similarity: 0.0,
            max_seed_chunks: 32,
            ..Default::default()
        };
        let g = build_connection_graph(&db, "small.md", &opts).unwrap();

        assert_eq!(g.stats.seeds_used, 3);
        assert!(!g.truncated, "nothing was lost: {:?}", g.truncation);
        assert!(g.truncation.is_empty());
    }

    fn seed_cap_boundary(chunks: usize, cap: u32) -> ConnectionGraph {
        let db = setup_db();
        insert_doc_with_n_chunks(&db, "d.md", chunks, 0.10, 0.0001);
        insert_doc_with_chunk(&db, "n1.md", "n1", "n1 body", 0.20);
        let opts = GraphOptions {
            depth: 1,
            fan_out: 2,
            min_similarity: 0.0,
            max_seed_chunks: cap,
            ..Default::default()
        };
        build_connection_graph(&db, "d.md", &opts).unwrap()
    }

    /// A seed cap equal to the chunk count must not report truncation. This is
    /// the off-by-one a `>=` would introduce, and it is why the seed read asks
    /// for `cap + 1` rows.
    #[test]
    fn the_seed_cap_fires_only_when_a_chunk_was_actually_dropped() {
        let exact = seed_cap_boundary(8, 8);
        assert_eq!(exact.stats.seeds_used, 8);
        assert!(
            !exact.truncated,
            "cap == chunk count drops nothing: {:?}",
            exact.truncation
        );

        let over = seed_cap_boundary(9, 8);
        assert_eq!(over.stats.seeds_used, 8);
        assert!(over.truncated, "one chunk past the cap is a real loss");
        assert_eq!(reasons(&over), vec![TruncationReason::SeedChunks]);
    }

    /// The regression this whole ticket turns on. Measured on the real KB: the
    /// largest document has 160 chunks, and BFS emits every seed before any
    /// neighbour -- so a node budget *alone* spends itself entirely on the
    /// start document's own chunks and returns a "connection graph" with zero
    /// connections. The seed cap is what keeps room for connections.
    ///
    /// The defaults make that hold (cap 32 < budget 100), but it is not a
    /// property of the code: a caller who sets `max_seed_chunks` at or above
    /// `max_nodes` gets the degenerate graph back, and gets told so by the
    /// `node_budget` entry.
    #[test]
    fn the_seed_cap_keeps_room_for_connections() {
        // 24 chunks spread far apart, each with one close neighbour document,
        // so every seed has a genuine connection to find. (Packing the chunks
        // together instead would fill each KNN with same-document siblings,
        // which the walk skips -- a different failure that would mask this one.)
        let db = setup_db();
        insert_doc_with_n_chunks(&db, "big.md", 24, 0.0, 1.0);
        for i in 0..24 {
            insert_doc_with_chunk(
                &db,
                &format!("n{i}.md"),
                "n",
                "neighbour body",
                i as f32 + 0.001,
            );
        }

        let base = GraphOptions {
            depth: 1,
            fan_out: 5,
            min_similarity: 0.0,
            max_nodes: 20,
            ..Default::default()
        };

        // Without a seed cap the 24 seeds consume the whole 20-node budget.
        let uncapped = build_connection_graph(
            &db,
            "big.md",
            &GraphOptions {
                max_seed_chunks: 1000,
                ..base.clone()
            },
        )
        .unwrap();
        assert_eq!(
            uncapped.nodes.iter().filter(|n| n.depth > 0).count(),
            0,
            "precondition: an uncapped seed phase is what starves the frontier"
        );

        // The boundary the docs state: a budget *equal* to the seed count is
        // still degenerate, because the seeds fit and the first neighbour is
        // the one refused. "below the seed count" would be off by one.
        let exact = build_connection_graph(
            &db,
            "big.md",
            &GraphOptions {
                max_seed_chunks: 1000,
                max_nodes: 24,
                ..base.clone()
            },
        )
        .unwrap();
        assert_eq!(exact.stats.seeds_used, 24);
        assert_eq!(
            exact.nodes.iter().filter(|n| n.depth > 0).count(),
            0,
            "budget == seed count is still a graph with no connections"
        );
        assert!(exact.truncated);

        // The seed cap is what leaves room for connections.
        let capped = build_connection_graph(
            &db,
            "big.md",
            &GraphOptions {
                max_seed_chunks: 8,
                ..base
            },
        )
        .unwrap();
        assert_budget_invariants(&capped, 20);
        let neighbours = capped.nodes.iter().filter(|n| n.depth > 0).count();
        assert!(
            neighbours > 0,
            "a connection graph with no connections is useless; got {} seeds and {neighbours} \
             neighbours out of a 20 node budget",
            capped.nodes.iter().filter(|n| n.depth == 0).count()
        );
    }

    #[test]
    fn the_node_budget_bounds_the_result_and_the_knn_count() {
        let db = setup_db();
        insert_doc_with_n_chunks(&db, "s.md", 4, 0.10, 0.0001);
        for i in 0..30 {
            insert_doc_with_chunk(
                &db,
                &format!("n{i}.md"),
                "n",
                "neighbour body",
                0.20 + 0.001 * i as f32,
            );
        }

        for budget in [1u32, 3, 7, 12] {
            let opts = GraphOptions {
                depth: 3,
                fan_out: 20,
                min_similarity: 0.0,
                max_nodes: budget,
                max_seed_chunks: 4,
                ..Default::default()
            };
            let g = build_connection_graph(&db, "s.md", &opts).unwrap();
            assert_budget_invariants(&g, budget);
        }
    }

    #[test]
    fn the_node_budget_is_reported_with_a_remedy() {
        let db = setup_db();
        insert_doc_with_chunk(&db, "s.md", "s", "s body", 0.10);
        for i in 0..10 {
            insert_doc_with_chunk(
                &db,
                &format!("n{i}.md"),
                "n",
                "neighbour body",
                0.11 + 0.001 * i as f32,
            );
        }

        let opts = GraphOptions {
            depth: 1,
            fan_out: 10,
            min_similarity: 0.0,
            max_nodes: 4,
            ..Default::default()
        };
        let g = build_connection_graph(&db, "s.md", &opts).unwrap();

        assert_eq!(g.stats.total_nodes, 4);
        assert!(g.truncated);
        assert_eq!(reasons(&g), vec![TruncationReason::NodeBudget]);
        assert_eq!(g.truncation[0].limit, 4);
        assert!(
            g.truncation[0].detail.contains("max_nodes"),
            "the detail must name the knob that lifts the bound: {}",
            g.truncation[0].detail
        );
    }

    /// At the ceiling there is no larger graph to ask for, so telling the
    /// client to "raise max_nodes" would send it into a retry that cannot
    /// succeed. The remedy has to know when it has run out.
    #[test]
    fn the_remedy_stops_promising_a_bigger_graph_at_the_ceiling() {
        let under = node_budget_detail(100);
        assert!(under.contains("raise max_nodes"), "{under}");
        assert!(under.contains(&MAX_NODES_CEILING.to_string()), "{under}");

        let at = node_budget_detail(MAX_NODES_CEILING);
        assert!(
            !at.contains("raise max_nodes"),
            "at the ceiling this is advice that cannot work: {at}"
        );
        assert!(at.contains("already at its ceiling"), "{at}");
    }

    /// Once the budget is full no further node can ever be added, so the walk
    /// must stop issuing KNN queries — the expensive half of the cost model.
    /// Without the check at the top of the loop the remaining frontier is still
    /// queried, one full vector scan per queued node, all of it discarded.
    #[test]
    fn a_full_budget_stops_the_queries_instead_of_scanning_for_nothing() {
        let db = setup_db();
        insert_doc_with_chunk(&db, "s.md", "s", "s body", 0.10);
        for i in 0..10 {
            insert_doc_with_chunk(
                &db,
                &format!("n{i}.md"),
                "n",
                "neighbour body",
                0.11 + 0.001 * i as f32,
            );
        }

        // depth 2 means the depth-1 neighbours are expandable, so the only
        // thing that can stop them being queried is the budget check.
        let opts = GraphOptions {
            depth: 2,
            fan_out: 10,
            min_similarity: 0.0,
            max_nodes: 4,
            ..Default::default()
        };
        let g = build_connection_graph(&db, "s.md", &opts).unwrap();

        assert_eq!(g.stats.total_nodes, 4);
        assert_eq!(
            g.stats.knn_queries, 1,
            "the seed's KNN filled the budget; the 3 queued neighbours must not be queried"
        );
        assert!(g.truncated);
    }

    /// `truncated` must mean "something was lost", not "a counter reached its
    /// cap". A walk that exhausts the graph while exactly filling the budget
    /// has lost nothing, and must say so.
    #[test]
    fn filling_the_budget_exactly_is_not_truncation() {
        let db = setup_db();
        insert_doc_with_chunk(&db, "s.md", "s", "s body", 0.10);
        insert_doc_with_chunk(&db, "n0.md", "n", "n body", 0.11);
        insert_doc_with_chunk(&db, "n1.md", "n", "n body", 0.12);

        // 1 seed + 2 neighbours == 3 == the budget, and depth 1 means the
        // neighbours are never expanded, so the frontier is genuinely empty.
        let opts = GraphOptions {
            depth: 1,
            fan_out: 5,
            min_similarity: 0.0,
            max_nodes: 3,
            ..Default::default()
        };
        let g = build_connection_graph(&db, "s.md", &opts).unwrap();

        assert_eq!(g.stats.total_nodes, 3);
        assert!(
            !g.truncated,
            "the budget was filled but nothing was refused: {:?}",
            g.truncation
        );
    }

    #[test]
    fn a_zero_node_budget_returns_an_empty_graph_and_explains_why() {
        let db = setup_db();
        insert_doc_with_chunk(&db, "s.md", "s", "s body", 0.10);
        insert_doc_with_chunk(&db, "n0.md", "n", "n body", 0.11);

        for strategy in [SeedStrategy::AllChunks, SeedStrategy::Centroid] {
            let opts = GraphOptions {
                depth: 1,
                fan_out: 5,
                min_similarity: 0.0,
                max_nodes: 0,
                seed_strategy: strategy,
                ..Default::default()
            };
            let g = build_connection_graph(&db, "s.md", &opts).unwrap();
            assert_eq!(g.nodes.len(), 0, "{strategy:?} must honour max_nodes = 0");
            assert_eq!(g.stats.knn_queries, 0, "{strategy:?} must not query");
            assert_eq!(
                g.stats.seeds_used, 0,
                "{strategy:?}: nothing seeded a walk that never started"
            );
            assert!(g.truncated, "{strategy:?} must explain the empty result");
            assert_eq!(reasons(&g), vec![TruncationReason::NodeBudget]);
        }
    }

    /// `centroid` folds its seeds into one node, so the node budget has no grip
    /// on the seed read -- the cap is the only thing bounding it, and the
    /// changed meaning ("average of the first N chunks") has to be reported.
    #[test]
    fn the_centroid_reports_that_it_averaged_only_a_prefix() {
        let db = setup_db();
        insert_doc_with_n_chunks(&db, "big.md", 20, 0.10, 0.0001);
        insert_doc_with_chunk(&db, "n1.md", "n1", "n1 body", 0.20);

        let opts = GraphOptions {
            depth: 1,
            fan_out: 2,
            min_similarity: 0.0,
            seed_strategy: SeedStrategy::Centroid,
            max_seed_chunks: 6,
            ..Default::default()
        };
        let g = build_connection_graph(&db, "big.md", &opts).unwrap();

        assert_eq!(
            g.nodes.iter().filter(|n| n.depth == 0).count(),
            1,
            "centroid still emits exactly one seed node"
        );
        assert_eq!(
            g.stats.seeds_used, 6,
            "seeds_used reports the averaging input, not the node count"
        );
        assert!(g.truncated);
        assert_eq!(reasons(&g), vec![TruncationReason::SeedChunks]);
        assert!(
            g.truncation[0].detail.contains("centroid"),
            "the centroid remedy differs from the all_chunks one: {}",
            g.truncation[0].detail
        );
    }

    /// Both bounds can fire in one call, and each must appear exactly once —
    /// the node budget is detected at two separate enforcement points.
    #[test]
    fn each_reason_is_reported_at_most_once() {
        let db = setup_db();
        insert_doc_with_n_chunks(&db, "big.md", 20, 0.10, 0.0001);
        for i in 0..20 {
            insert_doc_with_chunk(
                &db,
                &format!("n{i}.md"),
                "n",
                "neighbour body",
                0.20 + 0.001 * i as f32,
            );
        }

        let opts = GraphOptions {
            depth: 3,
            fan_out: 20,
            min_similarity: 0.0,
            max_nodes: 6,
            max_seed_chunks: 4,
            ..Default::default()
        };
        let g = build_connection_graph(&db, "big.md", &opts).unwrap();

        assert!(g.truncated);
        let mut seen = reasons(&g);
        let before = seen.len();
        seen.sort_by_key(|r| format!("{r:?}"));
        seen.dedup();
        assert_eq!(before, seen.len(), "a reason was reported twice");
        assert!(
            seen.contains(&TruncationReason::SeedChunks)
                && seen.contains(&TruncationReason::NodeBudget),
            "both bounds fired here, got {seen:?}"
        );
    }

    #[test]
    fn the_defaults_are_the_measured_ones() {
        // These numbers are load-bearing: they were derived from the real KB
        // (median 13 / p90 26 / p99 43 / max 160 chunks per document,
        // ~72 ms per KNN, ~665 B of JSON per node). Changing one without
        // redoing that measurement is the thing this test exists to catch.
        assert_eq!(DEFAULT_MAX_SEED_CHUNKS, 32);
        assert_eq!(DEFAULT_MAX_NODES, 100);
        let d = GraphOptions::default();
        assert_eq!(d.max_seed_chunks, DEFAULT_MAX_SEED_CHUNKS);
        assert_eq!(d.max_nodes, DEFAULT_MAX_NODES);
    }

    #[test]
    fn the_truncation_fields_are_always_present_in_json() {
        let db = setup_db();
        insert_doc_with_chunk(&db, "s.md", "s", "s body", 0.10);
        let g = build_connection_graph(&db, "s.md", &GraphOptions::default()).unwrap();
        let v: serde_json::Value = serde_json::to_value(&g).unwrap();

        // A flag that vanishes when false forces a reader to tell "false"
        // apart from "old server".
        assert_eq!(v["truncated"], serde_json::json!(false));
        assert_eq!(v["truncation"], serde_json::json!([]));
        assert!(v["stats"]["seeds_used"].is_number());
    }

    /// The CLI text renderer prints the reason through `Display`, so the two
    /// surfaces must agree on the spelling — `Debug` would emit `NodeBudget`
    /// next to the JSON's `node_budget`.
    #[test]
    fn the_reason_reads_the_same_in_json_and_in_text() {
        for r in [TruncationReason::SeedChunks, TruncationReason::NodeBudget] {
            let json = serde_json::to_value(r).unwrap();
            assert_eq!(
                json.as_str().unwrap(),
                r.to_string(),
                "Display must match the serde spelling"
            );
            assert_eq!(r.as_str(), r.to_string());
        }
        assert_eq!(TruncationReason::SeedChunks.as_str(), "seed_chunks");
        assert_eq!(TruncationReason::NodeBudget.as_str(), "node_budget");
    }

    // -----------------------------------------------------------------------
    // The two bounds, under generated input (audit L-21)
    //
    // These are what stands between a `get_connection_graph` call and an
    // unbounded walk: the node budget decides how big an answer can get, and
    // the seed cap decides how much of the start document is read. Both are
    // reachable from an MCP client, so both take whatever a caller sends —
    // including `0` and `u32::MAX`, which is exactly where an example test
    // stops being convincing.
    //
    // The **asymmetry at zero is deliberate and documented**: `max_nodes = 0`
    // is a coherent request ("return nothing") and is honoured, while
    // `max_seed_chunks = 0` would make an answer indistinguishable from "the
    // document does not exist", so it becomes 1. A property is the right shape
    // for that, because the interesting part is that it holds for every other
    // input while those two differ.
    // -----------------------------------------------------------------------
    proptest::proptest! {
        #[test]
        fn the_node_budget_never_exceeds_its_ceiling(n in proptest::prelude::any::<u32>()) {
            proptest::prop_assert!(clamp_max_nodes(n) <= MAX_NODES_CEILING);
            // And never invents work: a clamp that raised a small request
            // would hand a caller more nodes than it asked for.
            proptest::prop_assert!(clamp_max_nodes(n) <= n);
        }

        #[test]
        fn the_seed_cap_lands_inside_its_documented_range(n in proptest::prelude::any::<u32>()) {
            let c = clamp_max_seed_chunks(n);
            proptest::prop_assert!((1..=MAX_SEED_CHUNKS_CEILING).contains(&c));
        }

        /// Clamping something already clamped changes nothing.
        ///
        /// The cheapest way to break a bound is to apply it twice somewhere —
        /// a caller clamps, passes the value on, and the callee clamps again.
        /// That is safe only while the function is idempotent, and nothing
        /// said it was.
        #[test]
        fn clamping_twice_is_clamping_once(n in proptest::prelude::any::<u32>()) {
            proptest::prop_assert_eq!(
                clamp_max_nodes(clamp_max_nodes(n)),
                clamp_max_nodes(n)
            );
            proptest::prop_assert_eq!(
                clamp_max_seed_chunks(clamp_max_seed_chunks(n)),
                clamp_max_seed_chunks(n)
            );
        }

        /// Order survives. A clamp that ever inverted it would mean "ask for
        /// more, get less" for some pair, which no caller could reason about.
        #[test]
        fn asking_for_more_never_yields_less(
            a in proptest::prelude::any::<u32>(),
            b in proptest::prelude::any::<u32>(),
        ) {
            let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
            proptest::prop_assert!(clamp_max_nodes(lo) <= clamp_max_nodes(hi));
            proptest::prop_assert!(clamp_max_seed_chunks(lo) <= clamp_max_seed_chunks(hi));
        }

        /// The asymmetry itself, stated as the rule rather than as two cases:
        /// **`max_nodes` honours zero and `max_seed_chunks` does not**, and
        /// every other input passes through both unchanged until its ceiling.
        #[test]
        fn zero_is_the_only_input_the_two_bounds_disagree_about(
            n in 0u32..=MAX_SEED_CHUNKS_CEILING,
        ) {
            if n == 0 {
                proptest::prop_assert_eq!(clamp_max_nodes(n), 0);
                proptest::prop_assert_eq!(clamp_max_seed_chunks(n), 1);
            } else {
                proptest::prop_assert_eq!(clamp_max_nodes(n), n);
                proptest::prop_assert_eq!(clamp_max_seed_chunks(n), n);
            }
        }
    }
}
