//! Maximal Marginal Relevance (MMR) re-ranking.
//!
//! Pure function library. RRF + (任意の) cross-encoder reranker の後ろに
//! 置き、relevance を上書きせず diversity 補正のみを加える。
//!
//! pipeline 順 (`SearchTool::search`):
//! `RRF (search_hybrid_candidates_unbounded) → reranker → mmr_select → Parent retriever → match_spans`

use wide::f32x8;

#[derive(Debug, Clone)]
pub struct MmrCandidate {
    pub chunk_id: i64,
    pub document_id: i64,
    pub embedding: Vec<f32>,
    /// reranker on 時は cross-encoder score、off 時は RRF score。
    /// MMR の relevance 項として使われる。
    pub relevance_score: f32,
}

/// MMR 選抜。`candidates` から top `limit` 件を diversity 補正付きで選ぶ。
///
/// formula:
/// ```text
/// MMR(d) = lambda * relevance(d) - (1 - lambda) * max_{d' in selected} sim(d, d')
///                                 - same_doc_penalty * (d.document_id ∈ selected_docs ? 1 : 0)
/// ```
/// ただし lambda が 0..=1、same_doc_penalty が 0..=1 の前提。`relevance_score`
/// は呼び出し側で `[0, 1]` に正規化済みであることを期待する (cross-encoder
/// score / RRF score それぞれの値域は事前に rescale)。
///
/// 戻り値は `candidates` への index の配列。呼び出し側で
/// `(chunk_id, SearchResult)` に verbatim マップする (MMR objective 値は
/// `SearchHit.score` に永続化しない、invariant #5)。
pub fn mmr_select(
    candidates: &[MmrCandidate],
    lambda: f32,
    same_doc_penalty: f32,
    limit: usize,
) -> Vec<usize> {
    if candidates.is_empty() || limit == 0 {
        return Vec::new();
    }
    let n = candidates.len();
    let target = limit.min(n);

    // pair cache for cosine sim between candidate pairs
    let mut sim_cache: std::collections::HashMap<(usize, usize), f32> =
        std::collections::HashMap::new();

    fn key(a: usize, b: usize) -> (usize, usize) {
        if a <= b { (a, b) } else { (b, a) }
    }

    // F-42 candidate (Vec<bool> active flag, deferred — see comment below).
    //
    // Originally feature-30 PR-2 spec § Q5 hypothesised that replacing
    // `Vec<usize> remaining` + `retain` (O(N) per iter) with `Vec<bool>` active
    // + `active_count` (O(1) per pick) would yield -10~20% on `pool=500`. In
    // practice the bench ran +5-8% **slower** (cosine-similarity inner loop
    // dominates; the bool-flag path scans all n indices every iter and pays
    // a branch-predictor penalty that retain's "live elements only" walk
    // avoids). The retain-based loop is retained until a future cycle re-
    // evaluates with a different data structure (BTreeSet / SmallVec swap-
    // remove / SIMD cosine kernel). The `prop_mmr_tie_break_stable` proptest
    // added in this cycle stays as a regression catcher for any future try.
    let mut selected: Vec<usize> = Vec::with_capacity(target);
    let mut remaining: Vec<usize> = (0..n).collect();

    while selected.len() < target && !remaining.is_empty() {
        let mut best_idx: Option<usize> = None;
        let mut best_score: f32 = f32::NEG_INFINITY;

        for &i in &remaining {
            let relevance = candidates[i].relevance_score;
            let max_sim_to_selected: f32 = if selected.is_empty() {
                0.0
            } else {
                selected
                    .iter()
                    .map(|&j| {
                        let k = key(i, j);
                        if let Some(&v) = sim_cache.get(&k) {
                            v
                        } else {
                            let s = cosine_similarity(
                                &candidates[i].embedding,
                                &candidates[j].embedding,
                            );
                            sim_cache.insert(k, s);
                            s
                        }
                    })
                    .fold(f32::NEG_INFINITY, f32::max)
            };
            let same_doc_hit = selected
                .iter()
                .any(|&j| candidates[j].document_id == candidates[i].document_id);
            let same_doc_term = if same_doc_hit { same_doc_penalty } else { 0.0 };
            let mmr_value =
                lambda * relevance - (1.0 - lambda) * max_sim_to_selected - same_doc_term;

            // tie-break: 入力順 (= caller-side stable ordering、典型的には RRF rank) を保つ。
            // chunk_id ではなく index を使うのは、chunk_id 自体に rank semantics がないため。
            // 上流の `rrf_topk` (Task 2.1) が score DESC + id ASC で deterministic に並べるので、
            // index ベースの tie-break で結果全体が安定する。
            let take_this = match best_idx {
                None => true,
                Some(_) if mmr_value > best_score => true,
                Some(b) if mmr_value == best_score && i < b => true,
                _ => false,
            };
            if take_this {
                best_score = mmr_value;
                best_idx = Some(i);
            }
        }
        if let Some(pick) = best_idx {
            selected.push(pick);
            remaining.retain(|&x| x != pick);
        } else {
            break;
        }
    }
    selected
}

fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    // dim ミスマッチは upstream invariant breakage (model 切替 / schema 破損 / decode bug 等)。
    // 本番では 0.0 で fail-safe するが、debug build では即座に検知できるよう assert する。
    debug_assert_eq!(
        a.len(),
        b.len(),
        "cosine_similarity dim mismatch: a={}, b={} (upstream invariant breakage?)",
        a.len(),
        b.len()
    );
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    // feature-31 (F-42 reattempt): f32x8 SIMD kernel with 8-lane parallelism.
    // production embedding (BGE-small=384, BGE-M3=1024) は両方 8 で割り切れるので
    // chunks=48/128 で末尾 tail 不要。tail (n % 8 != 0) は scalar fallback で安全網。
    let n = a.len();
    let chunks = n / 8;
    let mut dot_acc = f32x8::ZERO;
    let mut na_acc = f32x8::ZERO;
    let mut nb_acc = f32x8::ZERO;
    for i in 0..chunks {
        let mut a_buf = [0.0_f32; 8];
        let mut b_buf = [0.0_f32; 8];
        a_buf.copy_from_slice(&a[i * 8..i * 8 + 8]);
        b_buf.copy_from_slice(&b[i * 8..i * 8 + 8]);
        let va = f32x8::from(a_buf);
        let vb = f32x8::from(b_buf);
        dot_acc += va * vb;
        na_acc += va * va;
        nb_acc += vb * vb;
    }
    let mut dot: f32 = dot_acc.to_array().iter().sum();
    let mut norm_a_sq: f32 = na_acc.to_array().iter().sum();
    let mut norm_b_sq: f32 = nb_acc.to_array().iter().sum();
    // tail (n % 8) の scalar fallback (将来別 model 投入時の安全網)
    for i in (chunks * 8)..n {
        dot += a[i] * b[i];
        norm_a_sq += a[i] * a[i];
        norm_b_sq += b[i] * b[i];
    }
    let norm_a = norm_a_sq.sqrt();
    let norm_b = norm_b_sq.sqrt();
    if norm_a == 0.0 || norm_b == 0.0 {
        0.0
    } else {
        dot / (norm_a * norm_b)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cand(id: i64, doc: i64, emb: Vec<f32>, rel: f32) -> MmrCandidate {
        MmrCandidate {
            chunk_id: id,
            document_id: doc,
            embedding: emb,
            relevance_score: rel,
        }
    }

    #[test]
    fn test_mmr_lambda_1_returns_relevance_order() {
        // lambda=1.0 で diversity 項が消えるので relevance_score 降順そのまま
        let cands = vec![
            cand(1, 1, vec![1.0, 0.0], 0.3),
            cand(2, 2, vec![0.5, 0.5], 0.9),
            cand(3, 3, vec![0.0, 1.0], 0.6),
        ];
        let sel = mmr_select(&cands, 1.0, 0.0, 3);
        // relevance: index 1 (0.9) > index 2 (0.6) > index 0 (0.3)
        assert_eq!(sel, vec![1, 2, 0]);
    }

    #[test]
    fn test_mmr_lambda_0_maximizes_diversity() {
        // lambda=0.0 で 1 件目は relevance 度外視 (任意 = 通常 index 0 安定)、
        // 2 件目以降は最大 diversity (= 既選 set との sim 最小) を選ぶ
        let cands = vec![
            cand(1, 1, vec![1.0, 0.0], 0.5),
            cand(2, 2, vec![1.0, 0.0], 0.5), // 1 と完全同一 embedding
            cand(3, 3, vec![0.0, 1.0], 0.5), // 直交
        ];
        let sel = mmr_select(&cands, 0.0, 0.0, 2);
        // 1 件目: index 0 (id=1)
        // 2 件目: 1 と diversity 最大 → 直交の id=3 (index 2)
        assert_eq!(sel[0], 0);
        assert_eq!(sel[1], 2);
    }

    #[test]
    fn test_mmr_same_doc_penalty_zero_equals_pure_mmr() {
        // penalty=0 なら same-doc を意識しない (= U1 純 MMR)
        let cands = vec![
            cand(1, 100, vec![1.0, 0.0], 0.9),
            cand(2, 100, vec![0.99, 0.05], 0.85),
            cand(3, 200, vec![0.0, 1.0], 0.6),
        ];
        let sel0 = mmr_select(&cands, 0.7, 0.0, 3);
        // 同 doc penalty 無しでも diversity 項で id=2 は不利、id=3 が 2 番手
        assert_eq!(sel0[0], 0);
    }

    #[test]
    fn test_mmr_same_doc_penalty_strong_pushes_other_doc() {
        // penalty 大で同 doc 選択が強く抑制される
        let cands = vec![
            cand(1, 100, vec![1.0, 0.0], 0.9),
            cand(2, 100, vec![0.95, 0.1], 0.88), // 高 relevance、同 doc
            cand(3, 200, vec![0.5, 0.5], 0.5),   // 中 relevance、別 doc
        ];
        let sel = mmr_select(&cands, 0.7, 0.5, 2);
        // 1 件目 id=1 / 2 件目は same_doc_penalty で id=3 (別 doc) が優先
        assert_eq!(sel[0], 0);
        assert_eq!(sel[1], 2);
    }

    #[test]
    fn test_mmr_empty_candidates() {
        let sel = mmr_select(&[], 0.7, 0.0, 5);
        assert!(sel.is_empty());
    }

    #[test]
    fn test_mmr_limit_larger_than_candidates() {
        let cands = vec![
            cand(1, 1, vec![1.0, 0.0], 0.9),
            cand(2, 2, vec![0.0, 1.0], 0.5),
        ];
        let sel = mmr_select(&cands, 0.7, 0.0, 10);
        assert_eq!(sel.len(), 2); // truncate しない
    }

    #[test]
    fn test_mmr_identical_embeddings_no_panic_no_nan() {
        // 全候補 embedding が同一 (cos=1.0) の degenerate case
        let cands = vec![
            cand(1, 1, vec![1.0, 0.0], 0.9),
            cand(2, 2, vec![1.0, 0.0], 0.8),
            cand(3, 3, vec![1.0, 0.0], 0.7),
        ];
        let sel = mmr_select(&cands, 0.7, 0.0, 3);
        assert_eq!(sel.len(), 3);
        // 並びは relevance 順に倒れる (diversity 項が全て -1.0 で同点)
        assert_eq!(sel, vec![0, 1, 2]);
    }

    #[test]
    fn test_cosine_similarity_does_not_panic_on_nan_inf_inputs() {
        // proptest が catch しない panic-only invariant: f32::NAN / f32::INFINITY
        // 入力時に panic しない (= regression catcher、F-54 と整合)。bit-exact な
        // NaN/Inf 結果値は SIMD 累算順序で変化するので比較しない。
        //
        // ベクトル長は SIMD path (chunks > 0) と scalar tail (n % 8 != 0) の
        // 両方を叩くサイズで網羅。

        // 16-dim 全 NaN — SIMD path のみ (chunks=2, tail=0)
        let a_nan_16 = vec![f32::NAN; 16];
        let b_16 = vec![1.0_f32; 16];
        let _ = cosine_similarity(&a_nan_16, &b_16);

        // 9-dim 全 Infinity — SIMD chunk (1) + scalar tail (1)
        let a_inf_9 = vec![f32::INFINITY; 9];
        let b_9 = vec![1.0_f32; 9];
        let _ = cosine_similarity(&a_inf_9, &b_9);

        // 16-dim 単一 NaN 混入 — SIMD path
        let mut a_partial_16 = vec![1.0_f32; 16];
        a_partial_16[7] = f32::NAN;
        let _ = cosine_similarity(&a_partial_16, &b_16);

        // 16-dim 全 -Infinity — SIMD path
        let a_neg_inf_16 = vec![f32::NEG_INFINITY; 16];
        let _ = cosine_similarity(&a_neg_inf_16, &b_16);

        // 4-dim NaN — short scalar (chunks=0, tail=4)
        let a_nan_4 = vec![f32::NAN; 4];
        let b_4 = vec![1.0_f32; 4];
        let _ = cosine_similarity(&a_nan_4, &b_4);
    }

    #[test]
    fn test_cosine_similarity_scalar_fallback_for_non_8_aligned() {
        // (a) wide SIMD 実装の scalar fallback path (n % 8 != 0) が正しく動作することを確認。
        // production embedding dim (BGE-small=384, BGE-M3=1024) は両方 8 で割り切れるので
        // 本 path は production では発火しないが、将来別 model 投入時の安全網として残す。

        // 7-dim (chunks=0、全 scalar tail)
        let a = vec![1.0_f32, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0];
        let b = vec![1.0_f32, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0];
        let r1 = cosine_similarity(&a, &b);
        assert!(
            (r1 - 1.0).abs() < 1e-6,
            "7-dim identical: expected 1.0, got {}",
            r1
        );

        // 9-dim (chunks=1, tail=1)
        // dot = 1 (index 8), norm_a = sqrt(2), norm_b = 1 → 1 / sqrt(2) = FRAC_1_SQRT_2
        let a = vec![1.0_f32, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 1.0];
        let b = vec![0.0_f32, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 1.0];
        let r2 = cosine_similarity(&a, &b);
        let expected = std::f32::consts::FRAC_1_SQRT_2;
        assert!(
            (r2 - expected).abs() < 1e-6,
            "9-dim mixed: expected FRAC_1_SQRT_2 ({}), got {}",
            expected,
            r2
        );

        // 385-dim (chunks=48, tail=1)
        let mut a = vec![0.0_f32; 385];
        let mut b = vec![0.0_f32; 385];
        a[0] = 1.0;
        b[0] = 1.0;
        let r3 = cosine_similarity(&a, &b);
        assert!(
            (r3 - 1.0).abs() < 1e-6,
            "385-dim identical at index 0: expected 1.0, got {}",
            r3
        );
    }

    /// F-54: release build (debug_assertions disabled) で `debug_assert_eq!` は no-op になり、
    /// 続く `if a.len() != b.len() || a.is_empty() { return 0.0; }` で fail-safe 0.0 が返る
    /// ことを直接 assert する。debug build では debug_assert_eq! が panic するので test を
    /// skip する必要があり、`#[cfg(not(debug_assertions))]` で gate する。
    ///
    /// 通常 `cargo test` (debug build) では skip、`cargo test --release` で初めて実行される。
    /// 既存 `debug_assert_eq!` は **削除しない** (= debug build での早期検知を維持) ので、
    /// 本 test は release-build invariant の document 化と regression catcher の役割。
    #[test]
    #[cfg(not(debug_assertions))]
    fn test_cosine_similarity_dim_mismatch_returns_zero_release_only() {
        // dim mismatch (5 vs 7) で 0.0 fail-safe
        let a = vec![1.0_f32; 5];
        let b = vec![1.0_f32; 7];
        let r = cosine_similarity(&a, &b);
        assert_eq!(r, 0.0, "dim mismatch must return 0.0 in release build");
        // empty 入力も同 path (= `a.is_empty()` 分岐) で 0.0
        let empty: Vec<f32> = vec![];
        assert_eq!(cosine_similarity(&empty, &empty), 0.0);
    }

    proptest::proptest! {
        #[test]
        fn prop_mmr_select_length_invariant(
            n_cands in 0_usize..20,
            limit in 0_usize..30,
        ) {
            let cands: Vec<MmrCandidate> = (0..n_cands)
                .map(|i| cand(i as i64, (i % 3) as i64, vec![1.0, 0.0], 0.5))
                .collect();
            let sel = mmr_select(&cands, 0.7, 0.0, limit);
            proptest::prop_assert!(sel.len() <= limit.min(n_cands));
        }

        #[test]
        fn prop_mmr_select_indices_valid_subset(
            n_cands in 0_usize..20,
        ) {
            let cands: Vec<MmrCandidate> = (0..n_cands)
                .map(|i| cand(i as i64, (i % 3) as i64, vec![1.0, 0.0], 0.5))
                .collect();
            let sel = mmr_select(&cands, 0.7, 0.0, n_cands);
            // 全 index が有効、重複なし
            let unique: std::collections::HashSet<usize> = sel.iter().copied().collect();
            proptest::prop_assert_eq!(unique.len(), sel.len());
            proptest::prop_assert!(sel.iter().all(|&i| i < n_cands));
        }

        /// 既存 invariant 「mmr_value 同点 → input 順 (= index 昇順)」が
        /// 保たれることを fuzz。全候補 embedding が同一 + relevance +
        /// document_id すべて同一の degenerate case を proptest 化、
        /// 結果が **入力 index 昇順** であることを assert する。
        ///
        /// F-42 (Vec<bool> active flag への置換) 系の future refactor で
        /// 順序が崩れたら即 catch するための regression catcher。
        #[test]
        fn prop_mmr_tie_break_stable(
            n_cands in 1_usize..15,
            limit in 1_usize..20,
        ) {
            let cands: Vec<MmrCandidate> = (0..n_cands)
                .map(|i| MmrCandidate {
                    chunk_id: i as i64,
                    document_id: 0,
                    embedding: vec![1.0, 0.0, 0.0],
                    relevance_score: 0.5,
                })
                .collect();
            let sel = mmr_select(&cands, 0.7, 0.0, limit);
            let expected: Vec<usize> = (0..limit.min(n_cands)).collect();
            proptest::prop_assert_eq!(sel, expected);
        }
    }
}
