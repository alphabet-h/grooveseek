//! ユーザのクエリ文字列を FTS5 の MATCH 式にコンパイルする (feature-48 / A-10)。
//!
//! クエリを**文字種の境界**で token に割り、OR で結んだ式にコンパイルする。
//! **query-side だけの変更で、index も schema も tokenizer も変えていない** (再 index 不要)。
//!
//! なぜこの設計なのか (旧挙動が何を壊していたか / 形態素解析を見送った理由 / `OR` を
//! 選んだ理由 / 実測値) は **ADR-0002** に集約してある:
//! `docs/decisions/0002-compile-queries-into-per-token-fts-phrases.md`。
//! ここには、その行を読む人が必要とする事実 (不変条件・境界・計測値) だけを置く。
//!
//! # spec の手順との対応
//!
//! `.dev/specs/feature-48-fts-per-token-phrase.md` (v6) の手順 1..6 は次の関数に落ちている:
//!
//! | spec | 実装 |
//! |---|---|
//! | 1. quote セグメント化 | [`split_quotes`] |
//! | 2. whitespace 分割 + 3. 文字種 run 分割 | [`split_groups`] + [`split_runs`] |
//! | 4. 短 run の隣接結合と独立 emit | [`emit_group`] |
//! | 5. dedup / 上限 | [`dedup_and_cap`] ([`query_phrases`] の末尾) |
//! | 5. phrase 化 + 6. fallback | [`build_fts_query`] + [`fallback_whole_query`] |
//!
//! [`query_phrases`] が式に組み立てる**前**の phrase 内容を返すのは、
//! `server::compute_match_spans` が citation の offset を求めるのに同じ分割を要るため。
//! 分割規則を 2 か所に持つと、quote 付きクエリで「FTS は当たるのにハイライトだけ消える」
//! ずれ方をする (codex review P2、PR #134)。
//!
//! 手順 2 の whitespace 分割が [`split_groups`] に吸収されているのは、whitespace が
//! `char::is_alphanumeric()` で false = [`CharClass::Separator`] に落ちるためで、
//! spec 手順 4 も whitespace と Separator を同じ「跨いではいけない境界」として扱っている。
//!
//! # 中心的な不変条件
//!
//! **出力される phrase は必ず元クエリの連続部分文字列である。** trigram tokenizer は
//! 部分文字列でしか照合できないので、これが崩れると「作った phrase が原理的に何にも
//! マッチしない」状態になる。連結が連続性を保つことは、
//! **先に Separator で「群」に割ってから群の中だけで run を連結する** 2 段構成と、
//! [`emit_group`] が群ローカルな `Vec` を返すこと (群を跨いだ吸収が型として書けない) で
//! 構造的に保証している。
//!
//! # 許容している粗さ (A-11 = 形態素解析を検討するときの材料)
//!
//! - CJK 拡張 B 以降 (U+20000..) は [`CharClass::Kanji`] に入れていないので、`𠮷野家` は
//!   run が割れる。同様に U+3007 (〇) も `Nl` カテゴリなので `OtherWord` に落ちる
//! - Unicode 正規化を一切しない。NFD の `バ` は `ハ` + 結合濁点 (U+3099、Hiragana 範囲) なので
//!   run 境界が入力の正規化形に依存する。連結は連続部分文字列を保つので検索は壊れないが、
//!   phrase の切れ目は変わる
//! - 全角中点 `・` (U+30FB) は Katakana の範囲定義に入るので run を割らないが、半角中点
//!   `･` (U+FF65) は範囲外なので Separator になる (全角/半角で非対称)
//! - 英数字・CJK を 1 文字も含まない run (`---` など) も phrase になる。Markdown の水平線と
//!   衝突してノイズ源になり得るが、クエリに `---` が現れること自体が稀なので許容している

/// trigram tokenizer が phrase を照合できる最小文字数。
///
/// これ未満の phrase は FTS5 でエラーにならず**恒久的に 0 件**を返す
/// (SQLite fts5.html#the_trigram_tokenizer)。無効な節を送っても損しかしないので式から落とす。
const MIN_PHRASE_CHARS: usize = 3;

/// OR で並べる phrase 数の上限 (DoS / 式肥大ガード。AU-17 の list 上限の前例に倣う)。
const MAX_PHRASES: usize = 32;

/// quote 走査後の区間。`Quoted` は `""` を literal `"` へ畳んだ**後**の内容を持つ。
#[derive(Debug, Clone, PartialEq, Eq)]
enum Segment<'a> {
    Quoted(String),
    Plain(&'a str),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CharClass {
    Kanji,
    Hiragana,
    Katakana,
    OtherWord,
    Separator,
}

/// 群 (Separator を 1 つも含まない極大区間) の中の、同一クラスの極大連続列。
///
/// **同じ群から出た `Run` どうしは常に隣接している** = 連結してよい、が不変条件。
/// `text` は必ず元クエリの部分スライスであり、連結結果も部分文字列になる。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Run<'a> {
    text: &'a str,
    chars: usize,
}

/// 1 文字を文字種クラスに割り当てる。
///
/// **arm の順序が挙動を決める。** `char::is_alphanumeric()` は漢字・かな・カナすべてに
/// `true` を返すので、CJK の 3 クラスを `is_alphanumeric()` の arm より**前**に置かないと
/// 全 CJK が [`CharClass::OtherWord`] に潰れ、字種境界での分割そのものが消える
/// (`再ランキング` が 1 run になってしまう)。
fn classify(c: char) -> CharClass {
    match c {
        '\u{4E00}'..='\u{9FFF}'      // CJK 統合漢字
        | '\u{3400}'..='\u{4DBF}'    // 拡張 A
        | '\u{F900}'..='\u{FAFF}'    // 互換漢字
        | '\u{2F800}'..='\u{2FA1F}'  // 互換漢字補助
        | '\u{3005}'                 // 々
        | '\u{3006}' => CharClass::Kanji, // 〆

        '\u{3041}'..='\u{309F}' => CharClass::Hiragana,

        // 長音 ー (U+30FC) と全角中点 ・ (U+30FB) を含む (spec の範囲定義に従う)。
        // 半角カナは濁点・半濁点 (U+FF9E/U+FF9F) まで。半角中点 ･ (U+FF65) はこの範囲の
        // 外なので Separator に落ちる。
        '\u{30A1}'..='\u{30FF}' | '\u{FF66}'..='\u{FF9F}' => CharClass::Katakana,

        // 識別子・型番を割らない: E0382 / kb-mcp / sqlite-vec はどれも 1 run。
        // 全角英数・キリル・ハングル等もここに落ちる。
        _ if c.is_alphanumeric() || c == '_' || c == '-' => CharClass::OtherWord,

        _ => CharClass::Separator,
    }
}

/// クエリを quoted 区間と素の区間に割る (spec 手順 1)。
///
/// 走査規約は FTS5 自身の doubled-quote 規約と同じ: 開き `"` の後、`""` は literal `"` 1 文字
/// として内容に取り込んで走査を継続し、単独の `"` で phrase を閉じる。閉じ `"` に到達せず
/// 入力が尽きたら、その開き `"` は通常文字だったものとして**残り全体を 1 個の
/// [`Segment::Plain`] として返し、走査を打ち切る**。
fn split_quotes(raw: &str) -> Vec<Segment<'_>> {
    let mut out = Vec::new();
    let mut plain_start = 0usize;
    let mut it = raw.char_indices().peekable();

    while let Some((i, c)) = it.next() {
        if c != '"' {
            continue;
        }
        let mut content = String::new();
        let mut closed_at = None;
        while let Some((j, d)) = it.next() {
            if d == '"' {
                if matches!(it.peek(), Some((_, '"'))) {
                    it.next();
                    content.push('"');
                    continue;
                }
                closed_at = Some(j + d.len_utf8());
                break;
            }
            content.push(d);
        }
        match closed_at {
            Some(end) => {
                if plain_start < i {
                    out.push(Segment::Plain(&raw[plain_start..i]));
                }
                out.push(Segment::Quoted(content));
                plain_start = end;
            }
            // 閉じられなかった。`plain_start` は直近に閉じた quote の直後 (無ければ 0) を
            // 指したままなので、下の flush が開き `"` を含む残り全体を Plain として出す。
            None => break,
        }
    }

    if plain_start < raw.len() {
        out.push(Segment::Plain(&raw[plain_start..]));
    }
    out
}

/// Separator を 1 つも含まない極大区間 (= 群) を返す (spec 手順 2 + 3 の前半)。
///
/// 空の群は返さない。群の内側には Separator が無いので、**同じ群から出た run は必ず隣接する**。
fn split_groups(text: &str) -> Vec<&str> {
    text.split(|c| classify(c) == CharClass::Separator)
        .filter(|g| !g.is_empty())
        .collect()
}

/// 群を文字種クラスの切替で run に割る (spec 手順 3)。
fn split_runs(group: &str) -> Vec<Run<'_>> {
    let mut out = Vec::new();
    let mut start = 0usize;
    let mut chars = 0usize;
    let mut current: Option<CharClass> = None;

    for (i, c) in group.char_indices() {
        let class = classify(c);
        match current {
            Some(prev) if prev == class => chars += 1,
            Some(_) => {
                out.push(Run {
                    text: &group[start..i],
                    chars,
                });
                start = i;
                chars = 1;
                current = Some(class);
            }
            None => {
                start = i;
                chars = 1;
                current = Some(class);
            }
        }
    }
    if current.is_some() {
        out.push(Run {
            text: &group[start..],
            chars,
        });
    }
    out
}

/// 群の run 列を phrase 群に落とす (spec 手順 4)。
///
/// 短い run (3 文字未満) は直後の run へ左から右へ貪欲に連結し、累計が 3 文字に達した時点で
/// 打ち切って 1 個の phrase にする。連結によって「単独で 3 文字以上を満たす区間」が拡張された
/// 場合は、**拡張前の区間も独立した phrase として併せて出す** — trigram の部分一致は phrase の
/// 内側でしか効かないので、`再ランキング` だけを持つ文書に `ランキング` で当てられるように
/// 一般的な語のシグナルを残しておく必要がある。これは前方向 (短 run が長 run を飲み込む) と
/// 後方向 (完結済み phrase が末尾の短 run を飲み込む) の両方で起きる。
///
/// 戻り値が群ローカルな `Vec` なのは意図的で、**群を跨いだ末尾吸収を型として書けなくする**ため。
/// `&mut Vec` を渡す形にすると、後から「Plain セグメントをまとめて 1 回 tokenize すれば速い」と
/// 最適化したときに、Separator を跨いだ連結が静かに復活する。
fn emit_group(runs: &[Run<'_>]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let n = runs.len();
    let mut i = 0usize;
    // この群で最後に確定した phrase: (out 内の index, 開始 run index)。
    let mut last_unit: Option<(usize, usize)> = None;

    while i < n {
        // 単独で 3 文字以上 → そのまま完結 phrase (後で末尾吸収され得る)。
        if runs[i].chars >= MIN_PHRASE_CHARS {
            out.push(runs[i].text.to_string());
            last_unit = Some((out.len() - 1, i));
            i += 1;
            continue;
        }

        // 短 run → 直後へ貪欲連結。累計が 3 に達したら打ち切る。
        let start = i;
        let mut acc = 0usize;
        let mut j = i;
        while j < n && acc < MIN_PHRASE_CHARS {
            acc += runs[j].chars;
            j += 1;
        }

        if acc >= MIN_PHRASE_CHARS {
            let idx = out.len();
            out.push(concat(&runs[start..j]));
            // 打ち切り規則より start..j-1 は個別に 3 文字未満なので、単独で 3 文字以上に
            // なり得るのは最後に足した runs[j-1] だけ = 拡張前区間は一意。
            if j - start > 1 && runs[j - 1].chars >= MIN_PHRASE_CHARS {
                out.push(runs[j - 1].text.to_string());
            }
            last_unit = Some((idx, start));
            i = j;
            continue;
        }

        // 末尾に救済不能な短 run 列が残った (直後にもう run が無い)。
        match last_unit {
            Some((idx, unit_start)) => {
                // 吸収先は定義上必ず 3 文字以上の完結 phrase なので、末尾吸収は**常に**
                // 独立 emit を伴う。拡張前区間は連結 phrase の直後に挿入する。
                let old = std::mem::replace(&mut out[idx], concat(&runs[unit_start..n]));
                out.insert(idx + 1, old);
            }
            None => {
                tracing::debug!("fts_query: dropping isolated short runs");
            }
        }
        break;
    }

    out
}

fn concat(rs: &[Run<'_>]) -> String {
    rs.iter().map(|r| r.text).collect()
}

/// 重複除去 (初出保持) と上限の適用 (spec 手順 5)。
///
/// 重複を先に落としてから上限で切る。逆順にすると重複が枠を食って情報量が減る。
///
/// 切り詰めは **warn で出す** (BU-31)。上限に当たると落ちるのはクエリ**末尾**の phrase
/// なので、検索は成功したまま recall だけが静かに下がる — 気付けない類の劣化である。
/// dogfood の golden 37 件では phrase 数の最大が 9 で、この上限は**実クエリに当たって
/// いない**と実測できている。したがって発火自体が稀であり、発火したらそれ自体が
/// 「そんなに長いクエリが来ている」という見る価値のある信号になる。
/// (上限を下げれば最悪コストは下がるが、静かな切り詰めが始まる長さも下がるため、
/// 値ではなく可視性の側を直した。計測値は台帳 BU-31 と ADR-0002 を参照)
fn dedup_and_cap(phrases: Vec<String>) -> Vec<String> {
    let (kept, dropped) = dedup_and_cap_counted(phrases);
    if dropped > 0 {
        tracing::warn!(
            max = MAX_PHRASES,
            dropped,
            "fts_query: query exceeded the phrase cap; trailing phrases were dropped, \
             so the full-text half searched for less than the query asked for"
        );
    }
    kept
}

/// [`dedup_and_cap`] の純粋部分。落とした **distinct** phrase 数を第 2 要素で返す。
///
/// 警告を出すかどうかの判断をログ収集なしでテストできるように、副作用と分けてある
/// (`.dev` の罠 W-7 と同じ理由 — script 生成と subprocess 起動を混ぜると assert する
/// 手段が実行しかなくなる、という前例)。重複は「落とした」に数えない。
fn dedup_and_cap_counted(phrases: Vec<String>) -> (Vec<String>, usize) {
    let mut seen = std::collections::HashSet::new();
    let mut kept: Vec<String> = Vec::new();
    let mut dropped: usize = 0;

    for p in phrases {
        if !seen.insert(p.clone()) {
            continue;
        }
        // 上限に達しても走査は続ける。落とした数を報告するためで、入力はクエリ長で
        // bound されているので追加コストは無視できる。
        if kept.len() == MAX_PHRASES {
            dropped += 1;
            continue;
        }
        kept.push(p);
    }

    (kept, dropped)
}

/// token 化で phrase が 1 つも作れなかったときの逃げ道 (spec 手順 6)。
///
/// v0.15.x の `sanitize_fts_query` そのもの。`AI と ML` のように**全断片が 3 文字未満**の
/// クエリはトークン化では 1 個も phrase を作れないが、旧実装なら全体を 1 phrase にして
/// verbatim で当てられた。ここで戻さないと、この形のクエリだけが純粋に後退する。
///
/// NUL を弾くのは quoted 区間と同じ理由 — FTS5 の式パーサは C 文字列を読むので内部 NUL で
/// 式が切れ、unterminated quote の syntax error になって検索全体が `Err` で落ちる。
/// `a\0b` は 3 文字だが token 化では 0 phrase なのでここに到達する。
fn fallback_whole_query(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.chars().count() < MIN_PHRASE_CHARS || trimmed.contains('\0') {
        return None;
    }
    Some(format!("\"{}\"", trimmed.replace('"', "\"\"")))
}

/// クエリ文字列を FTS5 の MATCH 式にコンパイルする。
///
/// `None` は「FTS 側に投げるものが無い」= 呼び出し側は vector 単独で検索する、の意味。
pub(super) fn build_fts_query(raw: &str) -> Option<String> {
    let phrases = query_phrases(raw);
    if phrases.is_empty() {
        return fallback_whole_query(raw);
    }
    Some(
        phrases
            .iter()
            .map(|p| format!("\"{}\"", p.replace('"', "\"\"")))
            .collect::<Vec<_>>()
            .join(" OR "),
    )
}

/// FTS へ投げる phrase の**中身**を、式に組み立てる前の形で返す (手順 1〜5)。
///
/// `build_fts_query` の途中結果だが、`server::compute_match_spans` も同じ分割を
/// 必要とする。あちらが独自に whitespace 分割していると、`"Foundry Local"` のような
/// quote 付きクエリで `"Foundry` / `Local"` を探しにいって citation の offset が
/// 空になる (FTS は当たっているのにハイライトだけ消える)。**分割規則は 1 か所に置く。**
///
/// 空 `Vec` は「token 化では phrase を作れなかった」= 呼び出し側が全体 fallback を
/// 判断する、の意味。
pub(crate) fn query_phrases(raw: &str) -> Vec<String> {
    let mut phrases = Vec::new();

    for seg in split_quotes(raw) {
        match seg {
            Segment::Quoted(content) => {
                if content.contains('\0') {
                    tracing::debug!("fts_query: dropping a quoted phrase containing NUL");
                    continue;
                }
                if content.chars().count() < MIN_PHRASE_CHARS {
                    tracing::debug!("fts_query: dropping a quoted phrase below the trigram floor");
                    continue;
                }
                phrases.push(content);
            }
            Segment::Plain(text) => {
                for group in split_groups(text) {
                    phrases.extend(emit_group(&split_runs(group)));
                }
            }
        }
    }

    dedup_and_cap(phrases)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(text: &str) -> Run<'_> {
        Run {
            text,
            chars: text.chars().count(),
        }
    }

    fn q(raw: &str) -> Option<String> {
        build_fts_query(raw)
    }

    fn some(s: &str) -> Option<String> {
        Some(s.to_string())
    }

    // ---------------------------------------------------------------------
    // stage 単体: 入出力だけでは観測できない規則を押さえる層
    // ---------------------------------------------------------------------

    #[test]
    fn classify_puts_cjk_scripts_in_their_own_classes() {
        assert_eq!(classify('再'), CharClass::Kanji);
        assert_eq!(classify('々'), CharClass::Kanji);
        assert_eq!(classify('〆'), CharClass::Kanji);
        assert_eq!(classify('ぁ'), CharClass::Hiragana);
        assert_eq!(classify('ア'), CharClass::Katakana);
        assert_eq!(classify('ー'), CharClass::Katakana);
        assert_eq!(classify('ｱ'), CharClass::Katakana);
    }

    #[test]
    fn classify_keeps_the_katakana_middle_dot_inside_katakana() {
        // spec は Katakana を U+30A1..=U+30FF と範囲で定義しており、中点はその内側。
        // ここを Separator に倒すと `評価・分析` が phrase を 1 つも作れなくなる。
        assert_eq!(classify('\u{30FB}'), CharClass::Katakana);
        // 半角中点は範囲外なので Separator (全角/半角で非対称)。
        assert_eq!(classify('\u{FF65}'), CharClass::Separator);
    }

    #[test]
    fn classify_keeps_identifier_punctuation_in_otherword() {
        for c in ['A', '0', 'Ａ', '０', 'п', '한', '_', '-'] {
            assert_eq!(classify(c), CharClass::OtherWord, "{c:?}");
        }
        for c in [' ', '\u{3000}', ':', '"', '。', '\0'] {
            assert_eq!(classify(c), CharClass::Separator, "{c:?}");
        }
    }

    #[test]
    fn split_quotes_splits_a_quoted_region() {
        assert_eq!(
            split_quotes("\"Foundry Local\" の設定"),
            vec![
                Segment::Quoted("Foundry Local".to_string()),
                Segment::Plain(" の設定"),
            ]
        );
    }

    #[test]
    fn split_quotes_collapses_doubled_quotes_into_one_char() {
        assert_eq!(
            split_quotes("\"say \"\"hi\"\"\""),
            vec![Segment::Quoted("say \"hi\"".to_string())]
        );
    }

    #[test]
    fn split_quotes_stops_scanning_when_the_quote_never_closes() {
        // 閉じ quote に届かなかったので、開き `"` 以降は素のテキストとして 1 個だけ返る。
        // 走査を再開する実装だと Quoted("") が混ざるが、最終出力は同じになるため
        // build_fts_query 側からはこの違いを観測できない = ここが唯一の検出点。
        assert_eq!(
            split_quotes("abc \"de\"\" fg"),
            vec![Segment::Plain("abc \"de\"\" fg")]
        );
    }

    #[test]
    fn groups_never_span_a_separator() {
        assert_eq!(split_groups("再 ランキング"), vec!["再", "ランキング"]);
        assert_eq!(split_groups("評価\u{FF65}分析"), vec!["評価", "分析"]);
        assert_eq!(split_groups("a  b"), vec!["a", "b"]);
        assert_eq!(split_groups(" a "), vec!["a"]);
        assert!(split_groups("!!!").is_empty());
    }

    #[test]
    fn runs_break_where_the_script_changes() {
        let runs = split_runs("再ランキングの評価について");
        let texts: Vec<&str> = runs.iter().map(|x| x.text).collect();
        assert_eq!(texts, vec!["再", "ランキング", "の", "評価", "について"]);

        assert_eq!(split_runs("sqlite-vec").len(), 1);
        // 中点は Katakana なのでカタカナ複合語を割らない。
        assert_eq!(split_runs("クロス・エンコーダ").len(), 1);
    }

    #[test]
    fn emit_group_matches_the_traced_table() {
        let runs = [r("再"), r("ランキング"), r("の"), r("評価"), r("について")];
        assert_eq!(
            emit_group(&runs),
            vec!["再ランキング", "ランキング", "の評価", "について"]
        );
    }

    #[test]
    fn emit_group_absorbs_the_tail_into_the_previous_phrase() {
        assert_eq!(
            emit_group(&[r("システム"), r("化")]),
            vec!["システム化", "システム"]
        );
    }

    #[test]
    fn emit_group_carries_a_long_run_through_two_short_ones() {
        assert_eq!(
            emit_group(&[r("の"), r("再"), r("ランキング")]),
            vec!["の再ランキング", "ランキング"]
        );
    }

    #[test]
    fn emit_group_drops_a_group_that_cannot_reach_the_floor() {
        assert!(emit_group(&[r("評価")]).is_empty());
        assert!(emit_group(&[]).is_empty());
    }

    #[test]
    fn finish_phrases_dedups_before_it_caps() {
        let mut input = vec!["dup".to_string(), "dup".to_string()];
        for i in 0..MAX_PHRASES - 1 {
            input.push(format!("w{i:02}"));
        }
        // 重複が枠を食うなら distinct は 31 個で止まる。
        assert_eq!(dedup_and_cap(input).len(), MAX_PHRASES);
    }

    /// `compute_match_spans` はこの関数の結果を term として使う。式に組み立てる前の
    /// 中身が、quote を剥がした素の文字列であることを pin する (citation の offset は
    /// content 側をこの文字列で探して求めるので、`\"` が混ざると必ず 0 件になる)。
    #[test]
    fn query_phrases_returns_unquoted_contents_for_span_lookup() {
        assert_eq!(
            query_phrases("\"Foundry Local\""),
            vec!["Foundry Local".to_string()]
        );
        assert_eq!(
            query_phrases("retry budget"),
            vec!["retry".to_string(), "budget".to_string()]
        );
        // token 化で作れないときは空 = 呼び出し側が全体 fallback を判断する。
        assert!(query_phrases("ab").is_empty());
    }

    // ---------------------------------------------------------------------
    // 分割表: build_fts_query の入出力だけに依存する層 (リファクタ耐性)
    // ---------------------------------------------------------------------

    /// spec §1.3 の worked example。
    #[test]
    fn the_split_table_from_the_spec() {
        assert_eq!(
            q("再ランキングの評価について"),
            some("\"再ランキング\" OR \"ランキング\" OR \"の評価\" OR \"について\"")
        );
        assert_eq!(
            q("retry budget の設定"),
            some("\"retry\" OR \"budget\" OR \"の設定\"")
        );
        assert_eq!(
            q("\"Foundry Local\" の設定"),
            some("\"Foundry Local\" OR \"の設定\"")
        );
        assert_eq!(
            q("\"再ランキングの評価について\""),
            some("\"再ランキングの評価について\"")
        );
        assert_eq!(q("E0382"), some("\"E0382\""));
        assert_eq!(q("暗号化"), some("\"暗号化\""));
        assert_eq!(q("評価は"), some("\"評価は\""));
        assert_eq!(q("システム化"), some("\"システム化\" OR \"システム\""));
        assert_eq!(q("\"ab\" テスト"), some("\"テスト\""));
        assert_eq!(q("ab"), None);
    }

    /// 旧契約 (クエリ全体を 1 phrase) で pin されていたケースのうち、新契約でも
    /// 出力が変わらないもの。`test_search_hybrid_japanese_trigram` の前提でもある。
    #[test]
    fn old_contract_cases_that_must_not_change() {
        assert_eq!(q(""), None);
        assert_eq!(q("   "), None);
        assert_eq!(q("エラー"), some("\"エラー\""));
    }

    #[test]
    fn independent_emit_forward_keeps_the_absorbed_long_run() {
        assert_eq!(
            q("の再ランキング"),
            some("\"の再ランキング\" OR \"ランキング\"")
        );
    }

    #[test]
    fn independent_emit_backward_keeps_the_phrase_before_the_tail() {
        assert_eq!(q("の評価は"), some("\"の評価は\" OR \"の評価\""));
        assert_eq!(q("ＡＢＣ検索"), some("\"ＡＢＣ検索\" OR \"ＡＢＣ\""));
    }

    /// 前後どちらの方向にも拡張前区間を持つ unit。挿入位置まで決定論的であること。
    #[test]
    fn independent_emit_in_both_directions_keeps_a_deterministic_order() {
        assert_eq!(
            q("再ランキング化"),
            some("\"再ランキング化\" OR \"再ランキング\" OR \"ランキング\"")
        );
    }

    #[test]
    fn a_doubled_quote_continues_the_phrase_instead_of_closing_it() {
        assert_eq!(q("\"say \"\"hi\"\"\""), some("\"say \"\"hi\"\"\""));
        assert_eq!(q("\"ab\"\"cd\""), some("\"ab\"\"cd\""));
    }

    #[test]
    fn an_unterminated_quote_falls_back_to_tokenizing_the_rest() {
        assert_eq!(q("abc \"def"), some("\"abc\" OR \"def\""));
    }

    #[test]
    fn short_and_empty_quoted_segments_are_dropped() {
        assert_eq!(q("\"\" テスト"), some("\"テスト\""));
        // 内容 1 文字 (`"`) なので quoted は落ち、fallback が全体を拾う。
        assert_eq!(q("\"\"\"\""), Some(format!("\"{}\"", "\"\"".repeat(4))));
    }

    #[test]
    fn a_quoted_segment_with_an_embedded_nul_is_dropped() {
        // quoted も fallback も NUL を通さない。通すと FTS5 が syntax error を返し、
        // 検索そのものが Err で落ちる。
        assert_eq!(q("\"a\0bc\""), None);
    }

    #[test]
    fn duplicate_phrases_collapse_to_the_first_occurrence() {
        assert_eq!(q("abc abc def"), some("\"abc\" OR \"def\""));
        assert_eq!(
            q("ランキング 再ランキング"),
            some("\"ランキング\" OR \"再ランキング\"")
        );
    }

    #[test]
    fn dedup_runs_before_the_thirty_two_phrase_cap() {
        let words: Vec<String> = (0..40).map(|i| format!("w{i:02}")).collect();
        let out = q(&words.join(" ")).unwrap();
        let phrases: Vec<&str> = out.split(" OR ").collect();
        assert_eq!(phrases.len(), MAX_PHRASES);
        assert_eq!(phrases[0], "\"w00\"");
        assert_eq!(phrases[MAX_PHRASES - 1], "\"w31\"");
        assert!(!out.contains("w32"));

        // 重複が 1 つ混ざっても distinct が上限個あるなら上限個残る。
        let mut with_dup = vec!["w00".to_string()];
        with_dup.extend((0..MAX_PHRASES).map(|i| format!("w{i:02}")));
        assert_eq!(
            q(&with_dup.join(" ")).unwrap().split(" OR ").count(),
            MAX_PHRASES
        );
    }

    /// (BU-31) 切り詰めが起きたかどうかを、ログを捕まえずに固定する。
    ///
    /// 警告を出す条件そのものをテストしておかないと、`dropped > 0` の分岐が壊れても
    /// 誰も気付かない — まさに「上限に当たったのに静かなまま」という、この変更が
    /// 潰そうとしている状態に戻る。
    #[test]
    fn the_cap_reports_how_many_distinct_phrases_it_dropped() {
        let over: Vec<String> = (0..MAX_PHRASES + 5).map(|i| format!("w{i:02}")).collect();
        let (kept, dropped) = dedup_and_cap_counted(over);
        assert_eq!(kept.len(), MAX_PHRASES);
        assert_eq!(dropped, 5, "distinct phrases past the cap are counted");

        let exact: Vec<String> = (0..MAX_PHRASES).map(|i| format!("w{i:02}")).collect();
        assert_eq!(
            dedup_and_cap_counted(exact).1,
            0,
            "exactly at the cap is not truncation"
        );

        // 重複は「落とした」に数えない。数えてしまうと、上限に当たっていないクエリで
        // 警告が出る (= 警告が信用されなくなる)。
        let dupes = vec!["a".to_string(), "a".to_string(), "b".to_string()];
        let (kept, dropped) = dedup_and_cap_counted(dupes);
        assert_eq!(kept, vec!["a".to_string(), "b".to_string()]);
        assert_eq!(dropped, 0);
    }

    /// (BU-31) 現実的な長さのクエリは上限に**当たらない**。
    ///
    /// 上限に当たるとクエリ末尾の phrase が黙って落ち、検索は成功したまま recall だけ
    /// 下がる。dogfood の golden 37 件を実測したところ phrase 数の最大は **9** で、
    /// 上限 32 には遠かった。ここではその余裕を固定する — 上限を下げる変更や、
    /// 1 語からより多くの phrase を出す変更 (独立 emit の拡張など) が余裕を食い潰したら
    /// このテストが落ちる。
    ///
    /// クエリは実 golden の**転記ではなく**、同等の長さと構成 (日本語の自然文 /
    /// 日英混在 + 製品名 / 英語キーワード列) で書いた合成物。
    #[test]
    fn realistic_queries_stay_well_under_the_phrase_cap() {
        let realistic = [
            "ハイブリッド検索の再ランキングをどう評価するか",
            "Foundry Local と ONNX Runtime の互換 API 設定手順",
            "context window compaction strategies for long agent sessions",
            "MCP サーバをトランスポート別に整理した比較表はどこか",
        ];
        for raw in realistic {
            let n = query_phrases(raw).len();
            assert!(
                n * 2 <= MAX_PHRASES,
                "a realistic query should leave at least 2x headroom under the cap, \
                 otherwise ordinary queries are one edit away from silent truncation; \
                 {n} phrases from {raw:?} against a cap of {MAX_PHRASES}"
            );
        }
    }

    #[test]
    fn character_classes_split_runs_where_the_script_changes() {
        assert_eq!(q("abcｶﾀｶﾅ"), some("\"abc\" OR \"ｶﾀｶﾅ\""));
        assert_eq!(q("あ亜ア"), some("\"あ亜ア\""));
        assert_eq!(q("サーバー"), some("\"サーバー\""));
        assert_eq!(q("代々木"), some("\"代々木\""));
        assert_eq!(q("한국어 Привет"), some("\"한국어\" OR \"Привет\""));
        assert_eq!(q("sqlite-vec"), some("\"sqlite-vec\""));
        assert_eq!(q("kb_mcp"), some("\"kb_mcp\""));
    }

    #[test]
    fn the_katakana_middle_dot_does_not_split_a_run() {
        assert_eq!(q("評価・分析"), some("\"評価・分析\" OR \"評価・\""));
        assert_eq!(q("クロス・エンコーダ"), some("\"クロス・エンコーダ\""));
    }

    #[test]
    fn whitespace_and_separators_are_the_same_boundary() {
        // `評価は` と違い、空白を挟むと連結できないので短い側は落ちる。
        assert_eq!(q("再 ランキング"), some("\"ランキング\""));
        assert_eq!(q("sagashiro-embed-r4 の"), some("\"sagashiro-embed-r4\""));
    }

    #[test]
    fn a_group_boundary_stops_tail_absorption() {
        // quote 境界を跨いだ連結 (前方向 / 後方向) が起きないこと。
        assert_eq!(q("再\"ランキング\""), some("\"ランキング\""));
        assert_eq!(q("\"ランキング\"化"), some("\"ランキング\""));
    }

    #[test]
    fn fts5_operators_in_the_query_stay_literal() {
        assert_eq!(q("foo \"bar\" AND"), some("\"foo\" OR \"bar\" OR \"AND\""));
        assert_eq!(q("foo OR bar"), some("\"foo\" OR \"bar\""));
        assert_eq!(q("heading:foo"), some("\"heading\" OR \"foo\""));
    }

    #[test]
    fn a_run_without_letters_or_digits_still_becomes_a_phrase() {
        assert_eq!(q("---"), some("\"---\""));
    }

    /// 手順 6 の fallback。全断片が 3 文字未満のクエリは旧実装と同じ全体 phrase に戻る。
    /// これが無いとこの形のクエリだけが v0.15 から純粋に後退する。
    #[test]
    fn queries_made_only_of_short_fragments_fall_back_to_the_whole_query() {
        assert_eq!(q("AI と ML"), some("\"AI と ML\""));
        assert_eq!(q("評価\u{FF65}分析"), some("\"評価\u{FF65}分析\""));
        assert_eq!(q("!!! ??? 、。"), some("\"!!! ??? 、。\""));
    }

    /// fallback は trigram の下限まで下げない。旧実装と同じく 3 文字未満は vector 単独へ。
    #[test]
    fn the_fallback_still_respects_the_trigram_floor() {
        assert_eq!(q("ab"), None);
        assert_eq!(q(" a "), None);
    }

    // ---------------------------------------------------------------------
    // 不変条件 (proptest)
    // ---------------------------------------------------------------------

    proptest::proptest! {
        /// 中心的な不変条件: 出力 phrase は必ず元クエリの連続部分文字列である。
        /// 崩れると「原理的に何にもマッチしない phrase」を作ることになる。
        /// 入力から `"` を除くのは、quoted 由来 phrase が `""` を畳んだ後の内容であり
        /// 入力の部分文字列とは限らないため (この規則は表テスト側で押さえている)。
        #[test]
        fn every_phrase_is_a_substring_of_the_input(raw in "[^\"]{0,120}") {
            if let Some(expr) = build_fts_query(&raw) {
                for phrase in expr.split(" OR ") {
                    let inner = phrase.trim_matches('"');
                    proptest::prop_assert!(
                        raw.contains(inner) || raw.trim() == inner,
                        "phrase {inner:?} is not a substring of {raw:?}"
                    );
                }
            }
        }

        #[test]
        fn build_fts_query_never_exceeds_the_phrase_cap(raw in "[^\"]{0,400}") {
            if let Some(expr) = build_fts_query(&raw) {
                proptest::prop_assert!(expr.split(" OR ").count() <= MAX_PHRASES);
            }
        }


        /// full-audit 2026-08-12 テスト軸 H-5: 上の property は式を ` OR ` で
        /// split して phrase を復元するが、fallback が返す「クエリ全体 1 phrase」に
        /// ` OR ` が含まれると誤って割れる (`AI OR ML` が 2 個に見える)。
        /// `query_phrases` は式に組み立てる前の `Vec<String>` を返すので、
        /// パースを介さずに不変条件そのものを検証できる。
        #[test]
        fn query_phrases_are_substrings_without_parsing_the_expression(raw in "[^\"]{0,120}") {
            for phrase in query_phrases(&raw) {
                proptest::prop_assert!(
                    raw.contains(&phrase),
                    "phrase {phrase:?} is not a substring of {raw:?}"
                );
            }
        }

        /// 上限は式の見た目ではなく phrase 列そのもので担保されていること。
        #[test]
        fn query_phrases_never_exceed_the_cap(raw in "[^\"]{0,400}") {
            proptest::prop_assert!(query_phrases(&raw).len() <= MAX_PHRASES);
        }

        /// byte index が常に char 境界であること。quote / 制御文字 / 絵文字を含む任意入力。
        #[test]
        fn build_fts_query_never_panics_on_arbitrary_text(raw in ".{0,200}") {
            let _ = build_fts_query(&raw);
        }
    }
}
