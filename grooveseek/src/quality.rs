//! チャンク品質スコアリング。
//!
//! インデックス時に各チャンクに 0.0-1.0 の品質スコアを計算し、検索時に
//! しきい値未満のチャンクを除外するためのユーティリティ。exclude_headings
//! (`exclude_headings`) がセクション単位の除外であるのに対し、こちらは
//! チャンク個別の「中身が薄いか」を自動判定する補完的なレイヤ。
//!
//! MVP では以下 3 シグナルのみで判定する (評価関数の複雑度を抑え、デフォルト
//! 有効で導入する運用リスクを下げる):
//!
//! | シグナル | 減点条件 | 減点 |
//! |---|---|---|
//! | 長さ | `content.trim().chars().count() < 30` | 0.6 |
//! | 定型語のみ | trim 後が `DEFAULT_BOILERPLATE_PATTERNS` のいずれかに一致 | 0.5 |
//! | 構造貧弱 | 改行なし かつ `< 80 文字` | 0.3 |
//!
//! 基本スコア 1.0 から各減点を差し引き `max(0.0)` にクランプする。
//! 2 項以上ヒットでほぼ除外 (threshold=0.3 で)、1 項のみなら境界値で残す
//! 設計。
//!
//! 長さ由来の 2 減点 (長さ / 構造貧弱) は、**短さが形式の構造に由来する**チャンクには
//! 当てはまらないので [`crate::quality::QualityProfile`] で出自ごとに免除する。定型語のみの減点は
//! どの出自でも据え置く。

use serde::Deserialize;

/// しきい値未満を「低品質」と判定するデフォルト値。
/// `groove.toml` や CLI `--min-quality` で上書き可能。
pub const DEFAULT_QUALITY_THRESHOLD: f32 = 0.3;

/// Content `trim()` 後の文字数がこの値未満だと長さ減点。
const SHORT_CONTENT_THRESHOLD: usize = 30;
/// 改行なしで文字数がこの値未満だと「構造貧弱」減点。
const STRUCTURE_POOR_THRESHOLD: usize = 80;

const LENGTH_PENALTY: f32 = 0.6;
const BOILERPLATE_PENALTY: f32 = 0.5;
const STRUCTURE_PENALTY: f32 = 0.3;

/// 本文が丸ごとこれらのいずれかに一致する (句読点除去後、大小文字無視) と
/// 「定型語のみ」として減点する。substring ではなく完全一致に近い形で判定
/// することで、「TBD について議論した結果」のような長文はヒットさせない。
pub const DEFAULT_BOILERPLATE_PATTERNS: &[&str] = &[
    "TBD",
    "TODO",
    "WIP",
    "N/A",
    "未定",
    "準備中",
    "(準備中)",
    "詳細は後述",
    "詳細は後述する",
    "ここに書く",
    "後で書く",
    "Coming soon",
    "後述",
];

/// `groove.toml` の `[quality_filter]` セクションにマップされる設定。
/// 省略時は `QualityFilterConfig::default()` (enabled=true, threshold=0.3)。
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QualityFilterConfig {
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    #[serde(default = "default_threshold")]
    pub threshold: f32,
}

fn default_enabled() -> bool {
    true
}
fn default_threshold() -> f32 {
    DEFAULT_QUALITY_THRESHOLD
}

impl Default for QualityFilterConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            threshold: DEFAULT_QUALITY_THRESHOLD,
        }
    }
}

impl QualityFilterConfig {
    /// しきい値を 0-1 にクランプし、`enabled=false` なら実質 0 として返す。
    /// DB/検索層はこの値を `min_quality` として SQL に bind する。
    pub fn effective_threshold(&self) -> f32 {
        if !self.enabled {
            0.0
        } else {
            self.threshold.clamp(0.0, 1.0)
        }
    }
}

/// どの減点を適用するかを決めるチャンクの出自。
///
/// [`LENGTH_PENALTY`] と [`STRUCTURE_PENALTY`] は「短さ」を内容の薄さの代理として読む
/// 減点なので、**短さが形式の構造に由来する**チャンクには当てはまらない。その判定を
/// [`crate::parser::Parser::is_binary`] の bool に兼ねさせると、あの関数が同時に持つ
/// 「バイナリ形式 parser か」(= `get_document` の cap 分類まで流れる意味) に 2 つ目の
/// 意味が乗る。減点側だけを独立した型にして切り離す。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QualityProfile {
    /// 散文 (Markdown / プレーンテキスト)。3 減点すべて適用する。
    Text,
    /// バイナリ形式 parser (Pdf/Docx/Xlsx/Pptx) 由来。ページ / シート / スライドの
    /// 短さは形式の構造であって内容の薄さではない (§4.8)。
    Binary,
    /// ソースコードの定義単位チャンク (`symbol_kind` を持つ)。`MAXYEAR = 9999` や
    /// `type ShardId = u64;` のような 1 行の定義は、短くても**値そのものが情報**で、
    /// その値はどこにも別途 index されていない (AV-07)。
    Definition,
}

impl QualityProfile {
    /// チャンクの出自から profile を決める**唯一の場所**。呼び出し側 (indexer と
    /// [`crate::db::Database::backfill_quality`]) が各自 `match` を書くと、同じ問いに
    /// 2 つ目の答えができる。
    ///
    /// バイナリ形式 parser は `symbol_kind` を出さないので 2 つの真は同時に立たないが、
    /// 優先順は決めておく。
    pub fn of(is_binary: bool, is_definition: bool) -> Self {
        if is_binary {
            Self::Binary
        } else if is_definition {
            Self::Definition
        } else {
            Self::Text
        }
    }

    /// 短さ由来の減点 ([`LENGTH_PENALTY`] / [`STRUCTURE_PENALTY`]) を免除するか。
    ///
    /// [`BOILERPLATE_PENALTY`] は 3 profile とも据え置く: 「TBD」しか書かれていない
    /// チャンクは出自が何であれ薄い。
    fn exempts_shortness(self) -> bool {
        matches!(self, Self::Binary | Self::Definition)
    }
}

/// チャンク単体の品質スコア (0.0-1.0)。高いほど良質。
///
/// `heading` は将来拡張 (depth=0 な見出しのみチャンクの追加減点等) のために
/// 受けるが、MVP では減点には使わず `_` で無視する。
///
/// `profile` が [`QualityProfile::Binary`] / [`QualityProfile::Definition`] のときは
/// [`LENGTH_PENALTY`] と [`STRUCTURE_PENALTY`] を免除する。理由は型側の doc を参照。
///
/// **片方だけの免除は意味を持たない**: 既定しきい値 0.3 に対し減点 1 個では
/// LENGTH のみ 0.4 / STRUCTURE のみ 0.7 / BOILERPLATE のみ 0.5 でいずれも落ちず、
/// かつ [`SHORT_CONTENT_THRESHOLD`] は [`STRUCTURE_POOR_THRESHOLD`] より小さいので
/// 「30 文字未満」は必ず「80 文字未満」でもある。つまり**落ちるのは 2 減点が
/// 同時に立つ場合だけ**で、どちらを免除しても同じ集合が戻る。
pub fn chunk_quality_score(heading: Option<&str>, content: &str, profile: QualityProfile) -> f32 {
    let _ = heading;
    let trimmed = content.trim();
    let char_count = trimmed.chars().count();
    let exempt = profile.exempts_shortness();
    let mut score: f32 = 1.0;

    if !exempt && char_count < SHORT_CONTENT_THRESHOLD {
        score -= LENGTH_PENALTY;
    }

    if is_boilerplate_only(trimmed) {
        score -= BOILERPLATE_PENALTY;
    }

    let has_newline = trimmed.contains('\n');
    if !exempt && !has_newline && char_count < STRUCTURE_POOR_THRESHOLD {
        score -= STRUCTURE_PENALTY;
    }

    score.max(0.0)
}

/// `threshold <= 0.0` なら常に true (フィルタ無効)。
/// そうでなければ `score >= threshold` のとき通過。
pub fn passes_quality_filter(score: f32, threshold: f32) -> bool {
    if threshold <= 0.0 {
        return true;
    }
    score >= threshold
}

/// per-query の閾値解決ヘルパ。CLI / MCP search ツール / CLI search subcommand
/// 全てで同じロジックを使うため単一関数に寄せる (evaluator 指摘 Med #2)。
///
/// 優先順位:
/// 1. `include_low_quality=true`  → `0.0` (フィルタ無効、明示的 opt-out を最優先)
/// 2. `min_quality = Some(v)`     → `v.clamp(0.0, 1.0)`
/// 3. どちらも指定なし             → `server_default`
pub fn resolve_effective_threshold(
    include_low_quality: bool,
    min_quality: Option<f32>,
    server_default: f32,
) -> f32 {
    if include_low_quality {
        return 0.0;
    }
    match min_quality {
        Some(v) if v.is_finite() => v.clamp(0.0, 1.0),
        Some(_) => {
            // NaN / +Inf / -Inf を渡されたら default に倒す。比較がそのまま NaN を
            // 通すと < / >= が常に false → フィルタが事実上無効になり不可解な結果を返す。
            tracing::warn!(
                "min_quality={:?} is not finite; falling back to server default",
                min_quality
            );
            server_default
        }
        None => server_default,
    }
}

fn is_boilerplate_only(content: &str) -> bool {
    // 末尾の句読点と空白を剥がして比較する。
    // 「TBD」「TBD。」「TBD.」「TBD 」をすべて同一視したい。
    let normalized: String = content
        .trim()
        .trim_end_matches(['.', '。', '、', ',', ' ', '\t'])
        .trim_start()
        .to_string();
    if normalized.is_empty() {
        // 完全に空なら「定型語のみ」は false (長さ減点で十分補足されるため)
        return false;
    }
    DEFAULT_BOILERPLATE_PATTERNS
        .iter()
        .any(|p| normalized.eq_ignore_ascii_case(p))
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_score_normal_rich_content_is_high() {
        // 200 文字超 + 改行 + 通常本文 → 減点なし
        let content = "これは十分な長さを持つ通常のチャンク本文です。\n\
                       複数行にわたり、定型語ではなく具体的な情報を含み、\n\
                       技術的な詳細も書かれています。文字数は 100 を軽く超えます。\n\
                       よって品質スコアは最大値の 1.0 になるはずです。";
        let score = chunk_quality_score(Some("Overview"), content, QualityProfile::Text);
        assert!((score - 1.0).abs() < 1e-5, "got {score}");
    }

    #[test]
    fn test_score_very_short_content_drops_by_length() {
        // < 30 文字 → 長さ減点
        // 改行なし + < 80 文字 → 構造減点も重なる
        // 合計 -0.9 で 0.1 付近
        let score = chunk_quality_score(None, "短い本文。", QualityProfile::Text);
        assert!(score < 0.2, "got {score}");
    }

    #[test]
    fn test_score_boilerplate_only_is_near_zero() {
        // 定型語のみ + 短い + 改行なし で 3 項全部効く → 0.0 クランプ
        let score = chunk_quality_score(None, "TBD", QualityProfile::Text);
        assert_eq!(score, 0.0);
        let score = chunk_quality_score(None, "TODO。", QualityProfile::Text);
        assert_eq!(score, 0.0);
        let score = chunk_quality_score(None, "詳細は後述", QualityProfile::Text);
        assert_eq!(score, 0.0);
    }

    #[test]
    fn test_score_boilerplate_case_insensitive() {
        // 小文字でもマッチする
        let score = chunk_quality_score(None, "todo", QualityProfile::Text);
        // 長さ減点 + 構造減点 + 定型語減点
        assert_eq!(score, 0.0);
    }

    #[test]
    fn test_score_structure_only_deduction() {
        // 30 <= len < 80 + 改行なし → 構造減点のみで 0.7
        let content = "これはちょうど三十文字を少しだけ超える程度の説明文で、改行はない。";
        let score = chunk_quality_score(None, content, QualityProfile::Text);
        assert!(
            (score - 0.7).abs() < 1e-5,
            "structure-only deduction should be 0.7, got {score}"
        );
    }

    #[test]
    fn test_score_long_single_line_no_penalty() {
        // >= 80 文字 + 改行なし → 構造シグナルは効かない (改行なしでも十分長い)
        let content: String = "あ".repeat(100);
        let score = chunk_quality_score(None, &content, QualityProfile::Text);
        assert!((score - 1.0).abs() < 1e-5, "got {score}");
    }

    #[test]
    fn test_score_long_boilerplate_sentence_not_penalized_for_boilerplate() {
        // 定型語 (`TBD`) を含む文でも、パターンに完全一致しないので定型語
        // 減点は効かない。構造減点 (改行なし短文) は効きうるので、ここでは
        // 十分長い複数行テキストで検証する。
        let content = "TBD について議論した結果をまとめる、今後の課題も含めて十分な文字数を確保した本文。\n\
                       さらに後続の段落を付け加えることで、構造減点も回避できる長さの文章となっている。\n\
                       すなわち定型語の部分一致ヒットは起きないことを確認したい。";
        let score = chunk_quality_score(None, content, QualityProfile::Text);
        assert!(
            (score - 1.0).abs() < 1e-5,
            "boilerplate partial-match should not deduct: got {score}"
        );
    }

    #[test]
    fn test_score_empty_content() {
        // 空文字列は長さ減点 + 構造減点 = -0.9 → 0.1
        let score = chunk_quality_score(None, "", QualityProfile::Text);
        assert!((score - 0.1).abs() < 1e-5, "got {score}");
        // trim 後空でも boilerplate_only は false 扱い (短さで十分補足)
    }

    #[test]
    fn test_binary_chunk_short_content_is_exempt() {
        // 「第3章: リスク管理」だけのタイトルスライド相当 = 短い・改行なし。
        // text 扱いなら length(-0.6)+structure(-0.3) で 0.1 に落ちるが、
        // is_binary=true なら両免除で 1.0 を保つ。
        let text_score =
            chunk_quality_score(Some("Slide 1"), "第3章 リスク管理", QualityProfile::Text);
        assert!(
            text_score < 0.2,
            "text short content should be penalized, got {text_score}"
        );
        let bin_score =
            chunk_quality_score(Some("Slide 1"), "第3章 リスク管理", QualityProfile::Binary);
        assert!(
            (bin_score - 1.0).abs() < 1e-5,
            "binary short content must be exempt, got {bin_score}"
        );
    }

    #[test]
    fn test_binary_chunk_boilerplate_still_penalized() {
        // BOILERPLATE_PENALTY は据え置き (免除しない)。ただし length/structure は
        // 免除なので、is_binary=true の "TBD" は -0.5 のみ = 0.5。
        let score = chunk_quality_score(None, "TBD", QualityProfile::Binary);
        assert!(
            (score - 0.5).abs() < 1e-5,
            "binary boilerplate keeps -0.5 only, got {score}"
        );
    }

    #[test]
    fn a_one_line_definition_keeps_the_value_that_is_its_information() {
        // AV-07: 1 行の定数 / 型エイリアスは短いが、**値そのものが情報**で、その値は
        // どこにも別途 index されていない。Text 扱いなら 0.1 で既定しきい値 0.3 に
        // 届かず永久に返らない。
        for content in [
            "MAXYEAR = 9999",
            "type ShardId = u64;",
            "CO_NESTED = 0x0010",
        ] {
            let text_score = chunk_quality_score(None, content, QualityProfile::Text);
            assert!(
                text_score < DEFAULT_QUALITY_THRESHOLD,
                "prose rules would hide {content:?}, got {text_score}"
            );
            let def_score = chunk_quality_score(None, content, QualityProfile::Definition);
            assert!(
                (def_score - 1.0).abs() < 1e-5,
                "definition short content must be exempt, got {def_score} for {content:?}"
            );
        }
    }

    #[test]
    fn a_definition_that_carries_only_a_name_comes_back_too() {
        // 免除は減点の粒度でしか効かないので、`pub mod x;` (名前だけの宣言) も
        // `type ShardId = u64;` と一緒に戻る。docs の Known limitations はこの事実を
        // 書いていて、片方だけ残す実装は存在しない。
        let score = chunk_quality_score(None, "pub mod x;", QualityProfile::Definition);
        assert!(
            (score - 1.0).abs() < 1e-5,
            "a name-only declaration is exempt by the same rule, got {score}"
        );
    }

    #[test]
    fn a_definition_that_is_only_boilerplate_is_still_penalized() {
        // BOILERPLATE_PENALTY は Definition でも据え置く。免除するのは短さ由来の
        // 2 減点だけ。
        let score = chunk_quality_score(None, "TODO", QualityProfile::Definition);
        assert!(
            (score - 0.5).abs() < 1e-5,
            "definition boilerplate keeps -0.5 only, got {score}"
        );
    }

    #[test]
    fn no_threshold_can_hide_a_definition_that_takes_no_penalty() {
        // The exemption leaves a non-boilerplate definition at exactly 1.0, and `min_quality`
        // is clamped to 1.0, and a chunk passes when its score is not *below* the threshold.
        // The three together mean there is no value a caller can pass that drops `pub mod x;`
        // — which is why the documentation sends them to `path_globs` rather than here.
        //
        // Pinned because the obvious sentence to write in a release note is "raise
        // `min_quality` if you do not want them", and it is false.
        let score = chunk_quality_score(None, "pub mod x;", QualityProfile::Definition);
        let ceiling = resolve_effective_threshold(false, Some(2.0), DEFAULT_QUALITY_THRESHOLD);
        assert!(
            (ceiling - 1.0).abs() < 1e-5,
            "min_quality is clamped to 1.0, got {ceiling}"
        );
        assert!(
            passes_quality_filter(score, ceiling),
            "a definition at {score} passes even the highest threshold a caller can ask for"
        );
    }

    #[test]
    fn the_profile_of_a_chunk_is_decided_in_one_place() {
        // 呼び出し側が各自 match を書くと同じ問いに 2 つ目の答えができるので、
        // 変換はこの関数にだけ置く。バイナリ形式 parser は symbol_kind を出さない
        // ので 2 つの真は同時に立たないが、優先順は決めてある。
        assert_eq!(QualityProfile::of(false, false), QualityProfile::Text);
        assert_eq!(QualityProfile::of(true, false), QualityProfile::Binary);
        assert_eq!(QualityProfile::of(false, true), QualityProfile::Definition);
        assert_eq!(QualityProfile::of(true, true), QualityProfile::Binary);
    }

    #[test]
    fn test_passes_quality_filter_threshold_zero_always_true() {
        // threshold=0.0 なら score が何でも通過 (フィルタ無効)
        assert!(passes_quality_filter(0.0, 0.0));
        assert!(passes_quality_filter(0.1, 0.0));
        assert!(passes_quality_filter(1.0, 0.0));
        // 負の threshold も無効扱い (CLI で --min-quality -1 渡されたとき)
        assert!(passes_quality_filter(0.0, -0.5));
    }

    #[test]
    fn test_passes_quality_filter_above_and_below() {
        // threshold=0.3 の境界挙動
        assert!(passes_quality_filter(0.3, 0.3), "equal must pass");
        assert!(passes_quality_filter(0.31, 0.3));
        assert!(!passes_quality_filter(0.29, 0.3));
    }

    #[test]
    fn test_resolve_effective_threshold_priority_order() {
        // include_low_quality=true はすべてに勝つ
        assert_eq!(
            resolve_effective_threshold(true, Some(0.8), 0.5),
            0.0,
            "include_low_quality must override min_quality + default"
        );
        assert_eq!(resolve_effective_threshold(true, None, 0.5), 0.0);

        // include_low_quality=false かつ min_quality Some → clamp
        assert_eq!(resolve_effective_threshold(false, Some(0.7), 0.3), 0.7);
        assert_eq!(resolve_effective_threshold(false, Some(1.5), 0.3), 1.0);
        assert_eq!(resolve_effective_threshold(false, Some(-0.1), 0.3), 0.0);

        // どちらもなしなら server default
        assert_eq!(resolve_effective_threshold(false, None, 0.3), 0.3);
    }

    /// Regression: NaN / Inf を渡されたら default に倒す (silent NaN 比較を避ける)。
    #[test]
    fn test_resolve_effective_threshold_nan_falls_back_to_default() {
        assert_eq!(
            resolve_effective_threshold(false, Some(f32::NAN), 0.42),
            0.42
        );
        assert_eq!(
            resolve_effective_threshold(false, Some(f32::INFINITY), 0.42),
            0.42
        );
        assert_eq!(
            resolve_effective_threshold(false, Some(f32::NEG_INFINITY), 0.42),
            0.42
        );
        // include_low_quality=true は NaN でも 0.0 (override 最優先)
        assert_eq!(resolve_effective_threshold(true, Some(f32::NAN), 0.42), 0.0);
    }

    #[test]
    fn test_config_default() {
        let c = QualityFilterConfig::default();
        assert!(c.enabled);
        assert_eq!(c.threshold, DEFAULT_QUALITY_THRESHOLD);
    }

    #[test]
    fn test_config_effective_threshold_disabled() {
        let c = QualityFilterConfig {
            enabled: false,
            threshold: 0.9,
        };
        assert_eq!(c.effective_threshold(), 0.0);
    }

    #[test]
    fn test_config_effective_threshold_clamps() {
        let c = QualityFilterConfig {
            enabled: true,
            threshold: 1.5,
        };
        assert_eq!(c.effective_threshold(), 1.0);
        let c = QualityFilterConfig {
            enabled: true,
            threshold: -0.5,
        };
        assert_eq!(c.effective_threshold(), 0.0);
    }

    #[test]
    fn test_config_parses_from_toml() {
        let src = "enabled = false\nthreshold = 0.5\n";
        let c: QualityFilterConfig = toml::from_str(src).unwrap();
        assert!(!c.enabled);
        assert_eq!(c.threshold, 0.5);
    }

    #[test]
    fn test_config_partial_toml_uses_defaults() {
        // threshold のみ指定 → enabled は default (true)
        let src = "threshold = 0.2\n";
        let c: QualityFilterConfig = toml::from_str(src).unwrap();
        assert!(c.enabled);
        assert_eq!(c.threshold, 0.2);
    }

    #[test]
    fn test_config_unknown_field_rejected() {
        let src = "threshold = 0.2\nbogus = 1\n";
        let err = toml::from_str::<QualityFilterConfig>(src).unwrap_err();
        assert!(err.to_string().contains("bogus") || err.to_string().contains("unknown"));
    }

    // -----------------------------------------------------------------------
    // F-37: chunk_quality_score の値域 invariant property test
    // -----------------------------------------------------------------------
    proptest::proptest! {
        #![proptest_config(proptest::test_runner::Config {
            cases: 256,
            ..proptest::test_runner::Config::default()
        })]

        /// chunk_quality_score は任意の (heading, content) に対して
        /// [0.0, 1.0] の範囲かつ有限 f32 を返す。
        /// (実装は `score.max(0.0)` で下限を保証、上限は減点しか加わらない
        ///  構造で保証されているが、将来の penalty 追加時に値域を超える
        ///  regression を proptest で機械的に catch する。)
        #[test]
        fn prop_chunk_quality_score_in_unit_range(
            heading in proptest::option::of("[\\PC]{0,32}"),
            content in "[\\PC]{0,4096}",
            profile in proptest::sample::select(vec![
                QualityProfile::Text,
                QualityProfile::Binary,
                QualityProfile::Definition,
            ]),
        ) {
            let s = chunk_quality_score(heading.as_deref(), &content, profile);
            proptest::prop_assert!(
                s.is_finite() && (0.0..=1.0).contains(&s),
                "chunk_quality_score must be in [0.0, 1.0] and finite, got {} (heading={:?}, content_len={})",
                s, heading, content.len()
            );
        }
    }
}
