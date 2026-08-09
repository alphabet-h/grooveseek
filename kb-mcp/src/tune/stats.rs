//! The statistics behind an adopt / decline decision.
//!
//! Worth keeping apart because this is where `kb-mcp tune` is easiest to be
//! wrong in a way nobody notices: AU-16 measured the paired SE at 0.53-0.60x
//! the true one, which left the null adoption rate at ~5.5x its nominal value
//! until AU-68 raised `ADOPT_SE_MULTIPLIER` from 2.0 to 3.0.
//!
//! Split out of `tune.rs` in AU-31. Contents are byte-identical.
//!
//! Unlike its two siblings this needs nothing from the parent — the functions
//! here take numbers and return numbers, which is exactly why they are the
//! easiest part of `tune` to test and the part worth isolating.

// ---------------------------------------------------------------------------
// Statistics (D-11)
// ---------------------------------------------------------------------------

/// 採用に要求する held-out 平均改善の下限 (nDCG@5)。
/// RRF 原論文の実測 (k∈[30,100] で MAP 相対 0.4%) を踏まえ、
/// この程度の差が出なければ measurement noise と見なす。
pub const ADOPT_MIN_MEAN_DELTA: f64 = 0.02;

/// 採用に要求する held-out 平均改善の、paired SE に対する倍率。
///
/// 名目の片側 2 sigma (= 誤採用率 2.3% 相当) をそのまま係数にすると、
/// `paired_se` が真の SE を 0.53〜0.60 倍に過小評価するぶん gate が緩み、
/// **真の優位差が 1 つも無い golden set でも 12.7% で採用が出る** (AU-16)。
///
/// 3.0 は AU-68 で誤採用率を直接掃引して選んだ値 —
/// `au68_adoption_rate_across_the_two_thresholds` を参照。null での採用が
/// 12.7% → 3.4% (N=26) / 9.7% → 3.1% (N=12) に落ち、代償は「見つかる優位差」の
/// 検出力が 99.0% → 95.2% になることだけ。
///
/// **`ADOPT_MIN_MEAN_DELTA` 側は上げても代わりにならない**: 0.02 → 0.04 では
/// null 誤採用が 12.7% → 12.1% しか動かないのに、同じ検出力が 99.0% → 51.9%
/// まで落ちる。同じ掃引で測ってある。
pub const ADOPT_SE_MULTIPLIER: f64 = 3.0;

/// 採用に要求する selection stability の下限 (過半数)。
/// fold 間で勝者が割れるのは過学習の最も直接的な兆候。
pub const STABILITY_MIN: f64 = 0.5;

/// これ未満の実効 N は IR 慣行の下限未満として stderr に警告する。
pub const SMALL_N_WARN: usize = 50;

/// 標本平均。空なら 0.0。
pub fn mean(xs: &[f64]) -> f64 {
    if xs.is_empty() {
        return 0.0;
    }
    xs.iter().sum::<f64>() / xs.len() as f64
}

/// 不偏標本標準偏差 (分母 n-1)。n < 2 なら 0.0。
pub fn sample_sd(xs: &[f64]) -> f64 {
    let n = xs.len();
    if n < 2 {
        return 0.0;
    }
    let m = mean(xs);
    let var = xs.iter().map(|x| (x - m) * (x - m)).sum::<f64>() / (n as f64 - 1.0);
    var.sqrt()
}

/// paired per-query 差分から求める標準誤差 `SD({d_j}) / sqrt(N)` (D-11-2)。
///
/// **fold 平均の分散を SE と呼んではならない** — fold 平均は d_j のアフィン
/// 変換なので SD が 1/(N−1) に縮み、有意判定が壊れる。
/// N < 2 は判定不能として無限大を返し、採用条件を必ず落とす。
///
/// ## この SE は保守側ではない (2026-07-27 実測、AU-16)
///
/// 以前ここには「fold が query を共有するため厳密には i.i.d. でないが、
/// **保守側 (過小評価しない)** に働く」と書いてあった。**逆だった**。
///
/// `SD/sqrt(N)` は d_j の独立を仮定する。LOO の N 個の選択は互いに N−2 個の
/// query を共有するので **fold ごとに違う条件を選び得る**が、これは相関を
/// 可能にするだけで**保証はしない**。
///
/// ただし「fold が一致すれば独立」も**言い過ぎ**。一致していても、その条件の
/// **正体は共有された訓練行から選ばれた確率変数**なので、全 d_j がそれを通じて
/// 結びつく。d_j が各自の held-out 行だけに依存すると言えるのは、**選択条件が
/// golden set の抽出をまたいで実質固定されている**場合であって、fold 内一致とは
/// 別の条件である。
///
/// そこで**測った**。測る対象は `refit` ではなく **fold の選択** — `refit` は全 N 行
/// から選ばれるが、各 `d_j` を生むのは `fold_selections[j]` (その fold の N−1 行から
/// 選ばれたもの) なので、refit が全 replication で同一でも fold 側は割れ得る。
/// 実際に両方数えると乖離がある (114 対 64 など)。
///
/// **静かな設定は fold 選択が 1 種類** — 300 replication × 26 fold = 7,800 回の選択が
/// すべて同一条件、つまり実質固定であり、そこだけ比が 1.027。騒がしい 3 設定は
/// 114〜184 種類に散っており、選択そのものがばらつく入力になっている。
///
/// 既知の golden set を多数生成して「報告される SE」と「mean delta の真の
/// ばらつき」を比べた実測 (`au16_paired_se_versus_the_true_standard_error`、
/// 300 反復 × 4 設定):
///
/// | N | 真の優位幅 | セル分散 | 全 rep | stability gate 通過分 | fold 選択の種類 |
/// |---|---|---|---|---|---|
/// | 26 | 0.04 | 0.08 | 0.533 | 0.619 (265/300) | 114 |
/// | 26 | 0.00 | 0.08 | 0.547 | 0.644 (239/300) | 184 |
/// | 12 | 0.04 | 0.08 | 0.601 | 0.733 (192/300) | 152 |
/// | 26 | 0.04 | 0.03 | **1.027** | 1.027 (300/300) | **1** |
///
/// 比は **replication 群に対する量** (mean delta のばらつきが要る) なので単一 run
/// では出せない。したがって「平均 stability が 0.84 だから gate を通る run でも
/// 1.9 倍過小」とは**言えない** — 平均 stability と比は同じ集合に対する別々の
/// 集計にすぎない。上表右列は `stability > STABILITY_MIN` を満たす replication
/// **だけ**で比を取り直したもので、`decide` が採用し得るのはこの部分集合だけ。
///
/// 読み取れること:
///
/// - **stability gate は緩和するが埋めない**。0.533 → 0.619 のように上がるが、
///   gate を通った replication でも SE はなお 1.4〜1.6 倍過小
/// - **稀な隅ではない**。300 rep 中 192〜300 が gate を通っている
/// - **測った 4 設定のうち、比が 1 付近だったのは最終行の 1 つだけ**で、そこは
///   fold 内一致に加えて **replication をまたいでも fold 選択が 1 種類**だった。
///   「選択のばらつきが相関の源」という機構と整合はするが、**4 設定・実質固定は
///   1 つ**なので、これは連関の
///   観測であって「一致した時に限り較正される」ことの証明ではない。部分的にしか
///   一致しない設定でも比が 1 に近づく可能性は未検証
///
/// ### 帰結は sigma 換算ではなく棄却率で測る
///
/// 「SE が 0.55 倍なので実効 1.1 sigma」と書くのは**誤り**だった。`se` は
/// replication ごとに変動し `mean_delta` と相関し得るので、平均の比から gate の
/// 発火確率は決まらない。検定の水準は**棄却率そのもの**で測る:
///
/// | 設定 | `m > 2*se` 発火 | `decide` が Adopt (係数 3.0) |
/// |---|---|---|
/// | N=26 edge=0.04 sd=0.08 | 35.0% | 17.7% |
/// | **N=26 edge=0.00 sd=0.08 (null)** | **12.7%** | **3.7%** |
/// | N=12 edge=0.04 sd=0.08 | 20.3% | 7.3% |
/// | N=26 edge=0.04 sd=0.03 | 99.7% | 97.0% |
///
/// 左列が**名目 2 sigma の gate が実際にはどれだけ緩いか**で、真の優位差が
/// 1 つも無い null でも 12.7% 発火する — 較正されていれば約 2.3% のはずの
/// ところ、およそ 5.5 倍。`ADOPT_SE_MULTIPLIER` が 2.0 だった頃は右列も
/// これとほぼ同値 (12.7%) で、**他の 4 条件はこのシミュレーション上では
/// ほとんど追加で効いていない**。したがって両列の差は実質的に、AU-68 で
/// 係数を 3.0 に上げた効果そのものである。
///
/// 「他の条件が効かない」のは合成データ側の性質でもある点に注意:
/// `table_from_primary` は nDCG / recall / MRR に同じ値を書くため
/// `non_degradation` が実データより通りやすい。実データで他の gate がどれだけ
/// 効くかは、これでは分からない。
///
/// **sign test は `TuneReport` に出るだけで `decide` は参照しない**ので、
/// 緩和材料として数えてはならない。
pub fn paired_se(diffs: &[f64]) -> f64 {
    if diffs.len() < 2 {
        return f64::INFINITY;
    }
    sample_sd(diffs) / (diffs.len() as f64).sqrt()
}

#[derive(Debug, Clone, PartialEq)]
pub struct SignTest {
    pub positive: usize,
    pub negative: usize,
    pub ties: usize,
    /// 帰無仮説 p=0.5 の二項分布による厳密両側 p 値。
    pub p_value: f64,
}

/// paired sign test (D-11-2 の参考情報)。同値は捨てる標準的な扱い。
pub fn sign_test(diffs: &[f64]) -> SignTest {
    const EPS: f64 = 1e-12;
    let positive = diffs.iter().filter(|d| **d > EPS).count();
    let negative = diffs.iter().filter(|d| **d < -EPS).count();
    let ties = diffs.len() - positive - negative;
    let n = positive + negative;
    if n == 0 {
        return SignTest {
            positive,
            negative,
            ties,
            p_value: 1.0,
        };
    }
    // 両側 p = 2 * P(X <= min(pos, neg)), X ~ Bin(n, 0.5)、1.0 でクランプ。
    let m = positive.min(negative);
    let mut tail = 0.0_f64;
    let mut coeff = 1.0_f64; // C(n, 0)
    for i in 0..=m {
        if i > 0 {
            coeff = coeff * ((n - i + 1) as f64) / (i as f64);
        }
        tail += coeff;
    }
    let p = (2.0 * tail / 2f64.powi(n as i32)).min(1.0);
    SignTest {
        positive,
        negative,
        ties,
        p_value: p,
    }
}
