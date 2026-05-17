//! Connection Graph: 起点ドキュメントからベクトル類似度で BFS 展開する機能。
//!
//! `get_connection_graph` MCP ツールと `kb-mcp graph` CLI サブコマンドの
//! バックエンド。grand plan は `docs/` 参照。

use std::collections::{HashSet, VecDeque};
use std::time::Instant;

use anyhow::{Context, Result};
use serde::Serialize;

use crate::db::{Database, SearchResult};

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// BFS の探索ポリシー。`all_chunks` は起点ドキュメント内の全チャンクをシード
/// として BFS を開始し、各々から KNN を広げる。`centroid` はチャンク埋め込みの
/// 平均ベクトルを L2 再正規化してから 1 つの擬似シードとして扱う (BGE 系の
/// embedding が単位ベクトルであるため、平均後も再正規化しないと
/// `distance_to_cos_sim` の前提が崩れる)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SeedStrategy {
    #[default]
    AllChunks,
    Centroid,
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
}

/// 上限 (MCP スキーマでバリデーション) — サーバ側でも再度強制する。
pub const MAX_DEPTH: u32 = 3;
pub const MAX_FAN_OUT: u32 = 20;

pub const DEFAULT_DEPTH: u32 = 2;
pub const DEFAULT_FAN_OUT: u32 = 5;
pub const DEFAULT_MIN_SIMILARITY: f32 = 0.3;

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
        }
    }
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
}

#[derive(Debug, Clone, Serialize)]
pub struct ConnectionGraph {
    pub start_path: String,
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

/// 起点 `start_path` から BFS で Connection Graph を構築する。
pub fn build_connection_graph(
    db: &Database,
    start_path: &str,
    opts: &GraphOptions,
) -> Result<ConnectionGraph> {
    let started = Instant::now();

    // 1. 起点シードを取得。存在しなければ明確にエラー。
    let seeds = db.chunks_for_path(start_path)?;
    if seeds.is_empty() {
        anyhow::bail!(
            "document not found (no chunks for path): {start_path}. \
             Run `kb-mcp index` to (re)index the knowledge base."
        );
    }

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

    // 2. seed_strategy に応じてシードを追加。
    match opts.seed_strategy {
        SeedStrategy::AllChunks => {
            for (chunk_id, embedding, r) in seeds {
                let node_id = nodes.len();
                nodes.push(make_node(node_id, None, 0, chunk_id, 1.0, &r));
                visited.insert(chunk_id);
                queue.push_back((node_id, embedding, 0));
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
            let (chunk_id, _, rep) = &seeds[0];
            let node_id = nodes.len();
            nodes.push(make_node(node_id, None, 0, *chunk_id, 1.0, rep));
            for (cid, _, _) in &seeds {
                visited.insert(*cid);
            }
            queue.push_back((node_id, sum, 0));
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
            stats: GraphStats {
                total_nodes: nodes.len(),
                max_depth_reached,
                knn_queries,
                duration_ms: started.elapsed().as_millis() as u64,
            },
            nodes,
        });
    }

    while let Some((parent_id, embedding, current_depth)) = queue.pop_front() {
        if current_depth >= opts.depth {
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
    Ok(ConnectionGraph {
        start_path: start_path.to_string(),
        stats: GraphStats {
            total_nodes: nodes.len(),
            max_depth_reached,
            knn_queries,
            duration_ms,
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
            )
            .unwrap();
        db.insert_chunk(
            doc_id,
            0,
            Some(heading),
            None,
            content,
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
            .upsert_document("s.md", Some("T"), None, None, None, &[], None, "hs")
            .unwrap();
        db.insert_chunk(
            doc_id,
            0,
            Some("h1"),
            None,
            "c1",
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

    #[test]
    fn test_graph_dedup_by_path_collapses_same_doc_chunks() {
        let db = setup_db();
        // start doc
        insert_doc_with_chunk(&db, "s.md", "s", "s body", 0.0);
        // same-path で 2 チャンクを持つ近傍ドキュメント
        let doc_id = db
            .upsert_document("a.md", Some("T"), None, None, None, &[], None, "ha")
            .unwrap();
        db.insert_chunk(
            doc_id,
            0,
            Some("h1"),
            None,
            "c1",
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
}
