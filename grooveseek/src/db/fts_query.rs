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
//! | 1. quote セグメント化 | [`scan`] |
//! | 2. whitespace 分割 + 3. 文字種 run 分割 | [`split_groups`] + [`split_runs`] |
//! | 4. 短 run の隣接結合と独立 emit | [`emit_group`] |
//! | 5. dedup / 上限 | [`dedup_and_cap_counted`] ([`parse_query`] の末尾) |
//! | 5. phrase 化 + 6. fallback | [`ParsedQuery::match_expr`] + [`fallback_whole_query`] |
//!
//! feature-55 (F-4) で [`scan`] は quote に加えて **group 先頭の `-`** も切るようになり、
//! 手順 1〜5 は正側と除外側の両方を同じ関数で通る。組み立てだけが `(正) NOT (負)` に分かれる。
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
//! **(BU-27) 以下はすべて `accepted_roughness_*` テストで固定してある。** 挙動を変えたら
//! そこが落ちるので、「意図した変更」と「事故」を区別できる。テストが落ちたときは
//! テストとこの一覧を**両方**直すこと。
//!
//! - CJK 拡張 B 以降 (U+20000..) は [`CharClass::Kanji`] に入れていないので、`𠮷野家` は
//!   **run が割れる** (`["𠮷", "野家"]`)。ただし短 run の連結が繋ぎ直すため phrase は
//!   `["𠮷野家"]` のままで、分割は見えない。見えるのは両側が十分長いとき —
//!   `𠮷野家具店` は `["𠮷野家具店", "野家具店"]` になる。同様に U+3007 (〇) も `Nl`
//!   カテゴリなので `OtherWord` に落ち、`東京〇丁目` の漢字 run を 2 つに割る
//! - Unicode 正規化を一切しない。NFD の `バ` は `ハ` + 結合濁点 (U+3099、Hiragana 範囲) なので
//!   run 境界が入力の正規化形に依存する。連結は連続部分文字列を保つので検索は壊れないが、
//!   phrase の切れ目は変わる
//! - 全角中点 `・` (U+30FB) は Katakana の範囲定義に入るので run を割らないが、半角中点
//!   `･` (U+FF65) は範囲外なので Separator になる (全角/半角で非対称)
//! - 英数字・CJK を 1 文字も含まない run (`---` など) も phrase になる。Markdown の水平線と
//!   衝突してノイズ源になり得るが、クエリに `---` が現れること自体が稀なので許容している

use std::borrow::Cow;
use std::ops::Range;

/// trigram tokenizer が phrase を照合できる最小文字数。
///
/// これ未満の phrase は FTS5 でエラーにならず**恒久的に 0 件**を返す
/// (SQLite fts5.html#the_trigram_tokenizer)。無効な節を送っても損しかしないので式から落とす。
///
/// (BU-28) この根拠自体は `db::tests::the_trigram_tokenizer_is_why_short_phrases_are_dropped`
/// が**実際の FTS5 テーブルに問い合わせて**固定している。本モジュールのテストは全部この
/// 3 という値を前提に書かれているので、tokenizer を差し替えると**全部緑のまま意味だけが
/// 消える** — 値ではなく理由を検査するテストが別に要る、というのがその 1 本の存在理由。
const MIN_PHRASE_CHARS: usize = 3;

/// OR で並べる phrase 数の上限 (DoS / 式肥大ガード。AU-17 の list 上限の前例に倣う)。
const MAX_PHRASES: usize = 32;

/// 走査後の区間。[`Segment::Quoted`] は `""` を literal `"` へ畳んだ**後**の内容を持つ。
///
/// [`Segment::ExcludedQuoted`] と [`Segment::ExcludedPlain`] は group 先頭の `-` で
/// 始まっていた区間 (F-4)。`-` そのものは内容に含めない。極性が違うだけで、この先の
/// phrase 化は正側と同じ関数を通る。
#[derive(Debug, Clone, PartialEq, Eq)]
enum Segment<'a> {
    Quoted(String),
    Plain(&'a str),
    /// `-"..."`: 逐語 phrase として除外する。doubled-quote 規約は [`Segment::Quoted`] と同じ。
    ExcludedQuoted(String),
    /// `-word`: `-` の直後から次の whitespace まで。正側と同じ規則で token 化して除外する。
    ExcludedPlain(&'a str),
}

/// [`scan`] の生の結果。
///
/// `exclusions` は raw の中の除外 group の byte span で、**先頭の `-` を含む**
/// (= [`cut_exclusions`] がそのまま切り落とせる区間)。昇順・非重複。
struct Scan<'a> {
    segments: Vec<Segment<'a>>,
    exclusions: Vec<Range<usize>>,
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

        // 識別子・型番を割らない: E0382 / groove / sqlite-vec はどれも 1 run。
        // 全角英数・キリル・ハングル等もここに落ちる。
        _ if c.is_alphanumeric() || c == '_' || c == '-' => CharClass::OtherWord,

        _ => CharClass::Separator,
    }
}

/// 開き `"` を消費した直後から phrase を読む (spec 手順 1 の走査本体)。
///
/// 規約は FTS5 自身の doubled-quote 規約と同じ: `""` は literal `"` 1 文字として内容に
/// 取り込んで走査を継続し、単独の `"` で phrase を閉じる。戻り値の第 2 要素は閉じ `"` の
/// **直後**の byte offset で、閉じずに入力が尽きたときは `None`。
///
/// 正側 (`"..."`) と除外側 (`-"..."`) の両方がここを呼ぶ。2 つ目の quote parser を
/// 作ると doubled-quote の畳み方が 2 か所に分かれる。
fn scan_quoted(it: &mut std::iter::Peekable<std::str::CharIndices<'_>>) -> (String, Option<usize>) {
    let mut content = String::new();
    while let Some((j, d)) = it.next() {
        if d == '"' {
            if matches!(it.peek(), Some((_, '"'))) {
                it.next();
                content.push('"');
                continue;
            }
            return (content, Some(j + d.len_utf8()));
        }
        content.push(d);
    }
    (content, None)
}

/// クエリを quoted 区間 / 素の区間 / 除外 group に 1 パスで割る (spec 手順 1 + F-4)。
///
/// 閉じ `"` に到達せず入力が尽きたら、その開き `"` は通常文字だったものとして
/// **残り全体を 1 個の [`Segment::Plain`] として返し、走査を打ち切る**。打ち切った後ろに
/// ある `-` は除外として見ない (既存の走査規則をそのまま保つ)。
///
/// 除外と見なすのは **group 先頭の `-`** だけ: 直前が入力の先頭か whitespace
/// (`char::is_whitespace()` なので全角空白 U+3000 も含む) で、かつ直後が `"` か
/// 「[`CharClass::Separator`] でも `-` でもない文字」のときに限る。`sqlite-vec` /
/// `foo,-bar` / `"foo"-bar` の `-` は group 先頭ではないので今日どおり
/// [`CharClass::OtherWord`] のまま、`---` / `- foo` / `--foo` / 末尾の `-` も literal。
fn scan(raw: &str) -> Scan<'_> {
    let mut segments = Vec::new();
    let mut exclusions = Vec::new();
    let mut plain_start = 0usize;
    // 直前に消費した文字が whitespace か (= 次の文字が group の先頭か)。
    let mut at_group_start = true;
    let mut it = raw.char_indices().peekable();

    while let Some((i, c)) = it.next() {
        if c == '"' {
            let (content, closed_at) = scan_quoted(&mut it);
            // 閉じられなかった。`plain_start` は直近に閉じた quote の直後 (無ければ 0) を
            // 指したままなので、下の flush が開き `"` を含む残り全体を Plain として出す。
            let Some(end) = closed_at else { break };
            if plain_start < i {
                segments.push(Segment::Plain(&raw[plain_start..i]));
            }
            segments.push(Segment::Quoted(content));
            plain_start = end;
            at_group_start = false;
            continue;
        }

        if c == '-' && at_group_start {
            match it.peek().copied() {
                Some((_, '"')) => {
                    it.next(); // 開き `"` を消費し、正側と同じ走査へ入る
                    let (content, closed_at) = scan_quoted(&mut it);
                    let Some(end) = closed_at else { break };
                    if plain_start < i {
                        segments.push(Segment::Plain(&raw[plain_start..i]));
                    }
                    segments.push(Segment::ExcludedQuoted(content));
                    exclusions.push(i..end);
                    plain_start = end;
                    at_group_start = false;
                    continue;
                }
                Some((_, d)) if classify(d) != CharClass::Separator && d != '-' => {
                    // 次の whitespace までが除外 group。whitespace 自体は消費しないので、
                    // 次の外側 iteration がそれを見て `at_group_start` を立てる。
                    let mut end = raw.len();
                    while let Some(&(j, e)) = it.peek() {
                        if e.is_whitespace() {
                            end = j;
                            break;
                        }
                        it.next();
                    }
                    if plain_start < i {
                        segments.push(Segment::Plain(&raw[plain_start..i]));
                    }
                    segments.push(Segment::ExcludedPlain(&raw[i + '-'.len_utf8()..end]));
                    exclusions.push(i..end);
                    plain_start = end;
                    continue;
                }
                // `-` の直後が `-` / Separator / 末尾 → 今日どおり literal。
                _ => {}
            }
        }

        at_group_start = c.is_whitespace();
    }

    if plain_start < raw.len() {
        segments.push(Segment::Plain(&raw[plain_start..]));
    }
    Scan {
        segments,
        exclusions,
    }
}

/// [`scan`] の segment だけを見る入口 (走査規則そのもののテスト用)。
#[cfg(test)]
fn split_quotes(raw: &str) -> Vec<Segment<'_>> {
    scan(raw).segments
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

/// 重複除去 (初出保持) と上限の適用 (spec 手順 5)。落とした **distinct** phrase 数を
/// 第 2 要素で返す。
///
/// 重複を先に落としてから上限で切る。逆順にすると重複が枠を食って情報量が減る。
/// 重複は「落とした」に**数えない** — 数えると、上限に当たっていないクエリで警告が
/// 出て信用されなくなる。
///
/// 副作用を持たないのは意図的で、警告を出すかどうかの判断をログ収集なしでテスト
/// できるようにするため (`.dev` の罠 W-7 と同じ理由 — script 生成と subprocess 起動を
/// 混ぜると assert する手段が実行しかなくなる、という前例)。実際の warn は
/// [`ParsedQuery::warn_if_truncated`] にある。
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

/// 切り詰め警告の発火点。テストが「1 検索につき 1 回」を数えられるよう関数にしてある。
fn emit_truncation_warning(dropped: usize) {
    #[cfg(test)]
    TRUNCATION_WARNINGS.with(|c| c.set(c.get() + 1));
    tracing::warn!(
        max = MAX_PHRASES,
        dropped,
        "fts_query: query exceeded the phrase cap; trailing phrases were dropped, \
         so the full-text half searched for less than the query asked for"
    );
}

/// 除外側の切り詰め警告。正側 ([`emit_truncation_warning`]) と別の関数なのは、
/// 落ちたときに起きることが**逆向き**だから: 正側は「探す語が減る」= 取りこぼしが増え、
/// 除外側は「落とす語が減る」= 望まない行が結果に**増える**。読む人が向きを取り違えると
/// 対処 (クエリを短くする / 除外を減らす) を間違える。
fn emit_exclusion_truncation_warning(dropped: usize) {
    #[cfg(test)]
    TRUNCATION_WARNINGS.with(|c| c.set(c.get() + 1));
    tracing::warn!(
        max = MAX_PHRASES,
        dropped,
        "fts_query: query exceeded the exclusion cap; trailing excluded phrases were dropped, \
         so the search excluded less than the query asked for"
    );
}

// 切り詰め警告の発火回数 (テスト専用)。`compute_match_spans` がヒットごとに
// `query_phrases` を呼ぶため、そこで警告すると 1 検索で N+1 回出てしまう。
#[cfg(test)]
thread_local! {
    static TRUNCATION_WARNINGS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// quoted 区間 1 個を phrase にする。NUL と 3 文字床の 2 判定 (正 / 除外で共通)。
///
/// NUL を弾く理由は [`fallback_whole_query`] と同じ — FTS5 の式パーサは C 文字列を
/// 読むので、内部 NUL で式が切れて検索全体が `Err` になる。
fn quoted_phrase(content: &str) -> Option<String> {
    if content.contains('\0') {
        tracing::debug!("fts_query: dropping a quoted phrase containing NUL");
        return None;
    }
    if content.chars().count() < MIN_PHRASE_CHARS {
        tracing::debug!("fts_query: dropping a quoted phrase below the trigram floor");
        return None;
    }
    Some(content.to_string())
}

/// 素の区間を群 → run → phrase に落として `out` に足す (spec 手順 2〜4)。
/// 正側と除外側が**同じ**この関数を通ることが、両極性の規則が一致していることの実体。
fn plain_phrases(text: &str, out: &mut Vec<String>) {
    for group in split_groups(text) {
        out.extend(emit_group(&split_runs(group)));
    }
}

/// raw を除外 span で割った、除外されていない区間の列。span が無ければ raw 全体 1 つ。
fn positive_regions<'a>(raw: &'a str, spans: &[Range<usize>]) -> Vec<&'a str> {
    let mut out = Vec::with_capacity(spans.len() + 1);
    let mut cursor = 0usize;
    for span in spans {
        out.push(&raw[cursor..span.start]);
        cursor = span.end;
    }
    out.push(&raw[cursor..]);
    out
}

/// 除外 group を raw から切り落として、埋め込み / reranker / span 用の文字列を作る。
///
/// group の直後の whitespace run ごと切る。末尾の group (後ろに whitespace が無い) では
/// 代わりに直前の whitespace を落とす — `rust -async` が `rust ` ではなく `rust` になる。
/// raw に元からある二重空白は触らない (embedder には無害で、触ると raw との一致が崩れる)。
///
/// span が空なら `Cow::Borrowed(raw)`。除外を書かないクエリの埋め込み入力が今日と
/// byte 単位で同一であることを、実行時の比較ではなく**型**で示すためにこの形にしてある。
fn cut_exclusions<'a>(raw: &'a str, spans: &[Range<usize>]) -> Cow<'a, str> {
    if spans.is_empty() {
        return Cow::Borrowed(raw);
    }
    let mut out = String::with_capacity(raw.len());
    let mut cursor = 0usize;
    for span in spans {
        out.push_str(&raw[cursor..span.start]);
        let mut end = span.end;
        while let Some(c) = raw[end..].chars().next().filter(|c| c.is_whitespace()) {
            end += c.len_utf8();
        }
        if end == span.end && end == raw.len() {
            out.truncate(out.trim_end().len());
        }
        cursor = end;
    }
    out.push_str(&raw[cursor..]);
    Cow::Owned(out)
}

/// phrase 列を FTS5 の OR 式にする。phrase 内の `"` は FTS5 の規約どおり `""` へ畳む。
fn join_phrases(phrases: &[String]) -> String {
    phrases
        .iter()
        .map(|p| format!("\"{}\"", p.replace('"', "\"\"")))
        .collect::<Vec<_>>()
        .join(" OR ")
}

/// 除外しかないクエリを断るときの文言。stderr に出るので **ASCII のみ** (AGENTS.md)。
const EXCLUSION_ONLY_MESSAGE: &str = concat!(
    "query has no positive term: every group is an exclusion (-term). ",
    "Add a term to search for, or quote a leading hyphen (\"-term\") ",
    "to search for it literally."
);

/// クエリを解析した結果。**1 回の走査**で「埋め込む文字列」「FTS が探す phrase」
/// 「FTS が除外する phrase」を確定して持ち回るための型 (F-4)。
///
/// entry point (MCP / CLI / golden の load) がこれを 1 個作り、拒否判定・embedder・
/// reranker・span・echo をすべてその 1 個から賄う。DB 層は `&str` の API のままなので
/// raw を受け取り、[`crate::db::Database::search_split_candidates`] の中で自分でもう
/// 一度解析する (FTS 半身と vector 半身はそこの 1 回の解析を共有する)。2 つの解析結果が
/// 食い違わないのは「一度しか解析しないから」ではなく、[`parse_query`] が純関数だから
/// である。
///
/// 「その chunk は除外語を含むか」の判定は**すべて FTS5 に委ねる**: FTS 半身は
/// [`Self::match_expr`] の `NOT` を native に評価し、vector 半身も同じ
/// [`Self::negative_match`] にマッチした rowid 集合で落とす。Rust 側の substring 判定は
/// 持たない (一つの問いに一つの実装)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedQuery<'a> {
    raw: &'a str,
    /// 除外 group を切り落とした raw。除外が無ければ `Cow::Borrowed(raw)`。
    positive_text: Cow<'a, str>,
    /// FTS が探す phrase (dedup + cap 済)。式に組み立てる前の素の内容。
    include: Vec<String>,
    /// FTS が除外する phrase (dedup + cap 済、正側と同じ 3 文字床)。
    exclude: Vec<String>,
    /// `include` が空のときに使う region 単位の全体 fallback。**quote 済み**の形で持つ
    /// ([`fallback_whole_query`] の戻り値そのもの)。
    fallback: Vec<String>,
    /// 構文上の除外 group の数 (3 文字床で phrase が落ちる**前**)。拒否判定に使う。
    exclusion_groups: usize,
    dropped_include: usize,
    dropped_exclude: usize,
}

/// クエリ文字列を解析する (spec 手順 1〜6 + F-4 の除外 group)。
///
/// **純粋**: ログも警告も出さない。切り詰めの警告は、1 検索につき 1 回だけ
/// [`ParsedQuery::warn_if_truncated`] を呼ぶ側の責務。
pub fn parse_query(raw: &str) -> ParsedQuery<'_> {
    let scanned = scan(raw);
    let mut include = Vec::new();
    let mut exclude = Vec::new();

    for seg in &scanned.segments {
        match seg {
            Segment::Quoted(content) => include.extend(quoted_phrase(content)),
            Segment::Plain(text) => plain_phrases(text, &mut include),
            Segment::ExcludedQuoted(content) => exclude.extend(quoted_phrase(content)),
            Segment::ExcludedPlain(text) => plain_phrases(text, &mut exclude),
        }
    }

    // 上限は極性ごとに別枠。共有枠にすると「クエリ順で先頭 32 個」の規則が極性を跨ぎ、
    // 除外を書いた位置で正側の phrase が落ちる (またはその逆) — 両半身とも今日の
    // 正側と同じ規則に保つ方が、説明も検証も 1 行で済む。
    let (include, mut dropped_include) = dedup_and_cap_counted(include);
    let (exclude, dropped_exclude) = dedup_and_cap_counted(exclude);

    // fallback は **raw を除外 span で割った positive region ごと**に効かせる。
    // 連結後の positive text に効かせると出力 phrase が raw の連続部分文字列でなくなり、
    // module 冒頭の中心的な不変条件が崩れる (`xy -abc z` が `"xy z"` になる)。
    // 除外が無ければ region は raw 全体 1 つ = 今日と同じ 1 回の呼び出し。
    let fallback = if include.is_empty() {
        let regions = positive_regions(raw, &scanned.exclusions);
        let (kept, dropped) = dedup_and_cap_counted(
            regions
                .into_iter()
                .filter_map(fallback_whole_query)
                .collect(),
        );
        dropped_include += dropped;
        kept
    } else {
        Vec::new()
    };

    ParsedQuery {
        raw,
        positive_text: cut_exclusions(raw, &scanned.exclusions),
        include,
        exclude,
        fallback,
        exclusion_groups: scanned.exclusions.len(),
        dropped_include,
        dropped_exclude,
    }
}

impl<'a> ParsedQuery<'a> {
    /// 解析前のクエリ文字列。DB 層は除外を効かせるためにこちらを見る。
    pub fn raw(&self) -> &'a str {
        self.raw
    }

    /// 除外 group を切り落とした文字列。embedder / reranker / citation の span は
    /// こちらを見る — 正側が全部 trigram 下限を割る `xy -abc z` に raw を渡すと、
    /// span 側が whitespace fallback に落ちて literal な `-abc` をハイライトする。
    pub fn positive_text(&self) -> &str {
        &self.positive_text
    }

    /// FTS が探す phrase (式に組み立てる前の素の内容)。
    pub fn include(&self) -> &[String] {
        &self.include
    }

    /// FTS が除外する phrase。response に echo するのは**実際に適用されたこれ**であって
    /// クエリに書かれた group ではない — `-再ランキング` が `ランキング` まで落とすことを
    /// 利用者に見せるため。
    pub fn exclude(&self) -> &[String] {
        &self.exclude
    }

    /// 除外 group はあるが、探すものが何も残っていない。
    ///
    /// 判定は **positive text** であって FTS phrase の有無ではない: `xy -abc z` は
    /// FTS phrase を 1 つも作れないが埋め込む文字列は残るので、`ab` が今日 vector 単独で
    /// 通るのと同じに通す。空クエリ `""` は除外 group が 0 なので今日どおり通る。
    pub fn is_exclusion_only(&self) -> bool {
        self.exclusion_groups > 0 && self.positive_text.trim().is_empty()
    }

    /// 3 つの entry point (MCP / CLI / golden の load) が共有する拒否。
    ///
    /// **DB 層はこれを呼ばない**。`search_fts_candidates("-abc")` は今日どおり
    /// `Ok(vec![])` を返す (`db.rs` の test module にある proptest がそれを要求する) — 拒否は
    /// 「利用者に言い直してもらう」ための入口の判断であって、検索そのものの失敗ではない。
    pub fn require_positive(&self) -> anyhow::Result<()> {
        if self.is_exclusion_only() {
            anyhow::bail!("{}", EXCLUSION_ONLY_MESSAGE);
        }
        Ok(())
    }

    /// FTS の正側 `"a" OR "b"`。token 化で phrase を作れなければ region 単位の
    /// fallback、それも無ければ `None` (= FTS 半身は休み、vector 単独で検索する)。
    pub(crate) fn positive_match(&self) -> Option<String> {
        if !self.include.is_empty() {
            return Some(join_phrases(&self.include));
        }
        if self.fallback.is_empty() {
            return None;
        }
        Some(self.fallback.join(" OR "))
    }

    /// FTS の負側 `"c" OR "d"`。3 文字床を越えた除外 phrase が 1 つも無ければ `None`
    /// (= `foo -ab` は**何も除外しない**)。vector 半身の除外もこの式で判定する。
    pub(crate) fn negative_match(&self) -> Option<String> {
        if self.exclude.is_empty() {
            return None;
        }
        Some(join_phrases(&self.exclude))
    }

    /// FTS5 へ投げる式。負側が無ければ正側そのまま (今日と byte 単位で同一)。
    ///
    /// **両辺を常に括弧で包む**。FTS5 の優先順位は NOT > OR (fts5.html §3.7) なので、
    /// 括弧を落とした `"a" OR "b" NOT "c"` は `"a" OR ("b" NOT "c")` と読まれ、除外が
    /// 最後の phrase にしか効かない — 構文的には正しいままなので、受理を見るテストでは
    /// 捕まらず、意味を見るテストでしか落ちない。単一 phrase でも `("a") NOT ("c")` と
    /// 一様にするのは、形が 1 つだとテストと mutation probe が単純になるため。
    pub(crate) fn match_expr(&self) -> Option<String> {
        let positive = self.positive_match()?;
        Some(match self.negative_match() {
            Some(negative) => format!("({positive}) NOT ({negative})"),
            None => positive,
        })
    }

    /// 切り詰めの警告 (BU-31) を出す。**1 検索につき 1 回**だけ呼ぶこと。
    ///
    /// [`crate::server::compute_match_spans`] は citation の offset を求めるために
    /// **ヒットごとに** [`query_phrases`] を呼ぶので、分割側で警告すると N 件返した
    /// クエリが同じ警告を N+1 回出す (codex review P2、PR #138)。稀にしか出ないことが
    /// 信号としての価値を作っているので、それを潰さない。production の呼び出し点は
    /// [`crate::db::Database::search_fts_candidates_parsed`] だけ。
    ///
    /// 上限に当たって落ちるのはクエリ**末尾**の phrase なので、検索は成功したまま
    /// recall だけが静かに下がる — 気付けない類の劣化である。dogfood の golden 37 件では
    /// phrase 数の最大が 9 で、この上限は**実クエリに当たっていない**と実測できている。
    /// したがって発火自体が稀であり、発火したらそれ自体が「そんなに長いクエリが来ている」
    /// という見る価値のある信号になる。(上限を下げれば最悪コストは下がるが、静かな
    /// 切り詰めが始まる長さも下がるため、値ではなく可視性の側を直した。計測値は台帳
    /// BU-31 と ADR-0002 を参照)
    pub(crate) fn warn_if_truncated(&self) {
        if self.dropped_include > 0 {
            emit_truncation_warning(self.dropped_include);
        }
        if self.dropped_exclude > 0 {
            emit_exclusion_truncation_warning(self.dropped_exclude);
        }
    }
}

/// FTS へ投げる phrase の**中身**を、式に組み立てる前の形で返す (手順 1〜5)。
///
/// [`parse_query`] の途中結果だが、[`crate::server::compute_match_spans`] も同じ分割を
/// 必要とする。あちらが独自に whitespace 分割していると、`"Foundry Local"` のような
/// quote 付きクエリで `"Foundry` / `Local"` を探しにいって citation の offset が
/// 空になる (FTS は当たっているのにハイライトだけ消える)。**分割規則は 1 か所に置く。**
///
/// 除外 phrase は**含まない** — 除外語はハイライトする対象ではない。span を求める側は
/// [`ParsedQuery::positive_text`] を渡すので、その入力から出る phrase 列は raw から出る
/// ものと一致する — quote された除外の直後に literal なハイフンが続く `foo -"bar"-baz`
/// を除いて。この 1 例だけは positive text 側が `-baz` を 2 つ目の除外として読み直す
/// (ADR-0011 の帰結。検索結果は変わらず、失われるのは highlight だけ)。
///
/// 空 `Vec` は「token 化では phrase を作れなかった」= 呼び出し側が全体 fallback を
/// 判断する、の意味。
pub(crate) fn query_phrases(raw: &str) -> Vec<String> {
    parse_query(raw).include
}

/// 旧入口。**テスト専用**に残してある。
///
/// 戻り値は今日と同じ**正側だけの OR 式**で、除外は含まない: proptest
/// [`tests::every_phrase_is_a_substring_of_the_input`] は戻り値を `" OR "` で割って raw
/// の部分文字列であることを要求し、[`tests::build_fts_query_never_exceeds_the_phrase_cap`]
/// は同じ割り方で個数を数えるので、括弧付きの `NOT` 式を返すと両方が落ちる。
///
/// 警告をここで出すのは
/// [`tests::the_truncation_warning_fires_once_per_search_not_once_per_hit`]
/// が「1 検索 1 回」をこの関数経由の発火回数で数えているため。production の発火点は
/// [`ParsedQuery::warn_if_truncated`]。
#[cfg(test)]
fn build_fts_query(raw: &str) -> Option<String> {
    let parsed = parse_query(raw);
    parsed.warn_if_truncated();
    parsed.positive_match()
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

    // -----------------------------------------------------------------------
    // (BU-27) The roughness this tokenizer accepts.
    //
    // The module doc lists four places where the character-class split is
    // knowingly coarse. Until now that list lived only in prose, so a future
    // change could quietly alter any of them and nothing would say whether the
    // new behaviour was the intent or an accident. These tests pin the
    // *current* answers — they are not asserting that the behaviour is good.
    // A failure here means "you changed an accepted roughness"; decide
    // deliberately, then update both the test and the module doc.
    //
    // Every expectation below was observed by running, not derived from the
    // prose, so the prose is checked too.
    // -----------------------------------------------------------------------

    /// CJK Extension B and U+3007 fall outside `Kanji`, so they break runs.
    ///
    /// The run split is the roughness; whether it is *visible* in the phrase
    /// output depends on the lengths involved, because short runs are merged
    /// back together afterwards. Both cases are pinned, since the difference
    /// between them is the kind of thing a reader gets wrong — the first draft
    /// of this test asserted `𠮷野家` yields `野家`, and it does not.
    #[test]
    fn accepted_roughness_cjk_beyond_the_basic_ranges_splits_runs() {
        // U+20BB7 (𠮷) is Extension B: not in any range `classify` calls Kanji,
        // so it lands in OtherWord and splits away from the kanji beside it.
        assert_eq!(classify('\u{20BB7}'), CharClass::OtherWord);
        assert_eq!(classify('野'), CharClass::Kanji);
        assert_eq!(
            split_runs("\u{20BB7}野家")
                .iter()
                .map(|r| r.text)
                .collect::<Vec<_>>(),
            vec!["\u{20BB7}", "野家"],
            "the run boundary is the accepted roughness"
        );

        // Short enough, and the merge puts it back together: the split leaves
        // no trace in the phrases.
        assert_eq!(
            query_phrases("\u{20BB7}野家"),
            vec!["\u{20BB7}野家".to_string()]
        );
        // Long enough, and it shows: the independent emit yields the kanji tail
        // on its own, which a document containing only 野家具店 will match.
        assert_eq!(
            query_phrases("\u{20BB7}野家具店"),
            vec!["\u{20BB7}野家具店".to_string(), "野家具店".to_string()]
        );

        // U+3007 (〇) is Nl, so `is_alphanumeric` claims it and it becomes
        // OtherWord rather than Kanji — despite reading as a kanji numeral. It
        // therefore splits a kanji run in two.
        assert_eq!(classify('\u{3007}'), CharClass::OtherWord);
        assert_eq!(
            split_runs("東京\u{3007}丁目")
                .iter()
                .map(|r| r.text)
                .collect::<Vec<_>>(),
            vec!["東京", "\u{3007}", "丁目"]
        );
        assert_eq!(
            classify('々'),
            CharClass::Kanji,
            "U+3005 IS treated as kanji"
        );
        assert_eq!(
            classify('〆'),
            CharClass::Kanji,
            "U+3006 IS treated as kanji"
        );
    }

    /// No Unicode normalization, so the decomposed form splits differently.
    #[test]
    fn accepted_roughness_normalization_changes_where_runs_break() {
        // NFC: one Katakana character.
        let composed = "\u{30D0}\u{30C3}\u{30C6}\u{30EA}"; // バッテリ
        // NFD: ハ + combining voiced mark, and U+3099 sits inside the Hiragana
        // range, so the run breaks in the middle of what reads as one word.
        let decomposed = "\u{30CF}\u{3099}\u{30C3}\u{30C6}\u{30EA}";
        assert_eq!(classify('\u{3099}'), CharClass::Hiragana);
        assert_ne!(
            query_phrases(composed),
            query_phrases(decomposed),
            "the composed and decomposed spellings of the same word must be \
             observed to split differently — if they ever agree, normalization \
             was added and the module doc needs updating"
        );
    }

    /// The full-width middle dot joins a run; the half-width one does not.
    #[test]
    fn accepted_roughness_the_two_middle_dots_are_asymmetric() {
        assert_eq!(
            classify('\u{30FB}'),
            CharClass::Katakana,
            "full-width ・ is inside the Katakana range, so it does not split"
        );
        assert_eq!(
            classify('\u{FF65}'),
            CharClass::Separator,
            "half-width ･ is just below the half-width Katakana range, so it does"
        );
        assert_eq!(
            query_phrases("クロス\u{30FB}エンコーダ"),
            vec!["クロス・エンコーダ".to_string()],
            "full-width: one phrase"
        );
        assert_eq!(
            query_phrases("クロス\u{FF65}エンコーダ"),
            vec!["クロス".to_string(), "エンコーダ".to_string()],
            "half-width: two phrases"
        );
    }

    /// A run of pure punctuation still becomes a phrase.
    #[test]
    fn accepted_roughness_punctuation_only_runs_become_phrases() {
        // '-' and '_' are explicitly OtherWord so identifiers stay whole, which
        // also means a Markdown horizontal rule is a legal phrase.
        assert_eq!(query_phrases("---"), vec!["---".to_string()]);
        assert_eq!(query_phrases("___"), vec!["___".to_string()]);
        // Below the trigram floor, the usual rule still applies.
        assert!(query_phrases("--").is_empty());
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

    // ---------------------------------------------------------------------
    // 除外 group (F-4)。scanner が「先頭 `-`」と見なす位置だけを押さえる層。
    // phrase 化した結果は下の parse_query の節で見る。
    // ---------------------------------------------------------------------

    #[test]
    fn split_quotes_marks_a_leading_hyphen_group_as_excluded() {
        assert_eq!(
            split_quotes("rust -async"),
            vec![Segment::Plain("rust "), Segment::ExcludedPlain("async")]
        );
        assert_eq!(
            split_quotes("-async rust"),
            vec![Segment::ExcludedPlain("async"), Segment::Plain(" rust")]
        );
    }

    #[test]
    fn split_quotes_excludes_a_quoted_phrase_after_a_hyphen() {
        assert_eq!(
            split_quotes("foo -\"bar baz\""),
            vec![
                Segment::Plain("foo "),
                Segment::ExcludedQuoted("bar baz".to_string()),
            ]
        );
        // doubled quote の畳み方は Quoted と同じ規約 (走査本体を共有している)。
        assert_eq!(
            split_quotes("-\"say \"\"hi\"\"\""),
            vec![Segment::ExcludedQuoted("say \"hi\"".to_string())]
        );
    }

    #[test]
    fn a_hyphen_is_only_an_exclusion_at_a_whitespace_boundary() {
        assert_eq!(split_quotes("foo,-bar"), vec![Segment::Plain("foo,-bar")]);
        assert_eq!(split_quotes("kb-mcp"), vec![Segment::Plain("kb-mcp")]);
        assert_eq!(
            split_quotes("\"foo\"-bar"),
            vec![Segment::Quoted("foo".to_string()), Segment::Plain("-bar")]
        );
    }

    #[test]
    fn a_hyphen_followed_by_a_hyphen_a_separator_or_the_end_is_literal() {
        assert_eq!(split_quotes("---"), vec![Segment::Plain("---")]);
        assert_eq!(split_quotes("- foo"), vec![Segment::Plain("- foo")]);
        assert_eq!(split_quotes("foo -"), vec![Segment::Plain("foo -")]);
        assert_eq!(split_quotes("--foo"), vec![Segment::Plain("--foo")]);
    }

    /// 先頭ハイフンを literal で探す逃げ道。quote すれば正側の phrase に戻る。
    #[test]
    fn a_quoted_leading_hyphen_is_a_positive_phrase() {
        assert_eq!(
            split_quotes("\"-foo\""),
            vec![Segment::Quoted("-foo".to_string())]
        );
        let p = parse_query("\"-foo\"");
        assert_eq!(p.include, vec!["-foo".to_string()]);
        assert!(p.exclude.is_empty());
    }

    /// 未閉じ quote に当たったら走査を打ち切る、という既存規則は除外にも先に効く。
    #[test]
    fn an_unterminated_quote_after_a_hyphen_is_not_an_exclusion() {
        assert_eq!(split_quotes("-\"abc"), vec![Segment::Plain("-\"abc")]);
        assert_eq!(
            split_quotes("rust \"async -tokio"),
            vec![Segment::Plain("rust \"async -tokio")]
        );
    }

    #[test]
    fn a_full_width_space_is_a_group_boundary_for_exclusion() {
        assert_eq!(
            split_quotes("再ランキング\u{3000}-評価"),
            vec![
                Segment::Plain("再ランキング\u{3000}"),
                Segment::ExcludedPlain("評価"),
            ]
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
        assert_eq!(dedup_and_cap_counted(input).0.len(), MAX_PHRASES);
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

    /// (BU-31、codex review P2 on PR #138) 切り詰め警告は **1 検索につき 1 回**。
    ///
    /// `server::compute_match_spans` は citation の offset を求めるために
    /// **ヒットごとに** [`query_phrases`] を呼ぶ。警告を分割側に置くと、N 件返した
    /// クエリが同じ警告を N+1 回出し、「稀だから見る価値がある」という性質が消える。
    /// 発火点を [`build_fts_query`] (1 検索 1 回) に限ることを、発火回数で固定する。
    #[test]
    fn the_truncation_warning_fires_once_per_search_not_once_per_hit() {
        let over: Vec<String> = (0..MAX_PHRASES + 5).map(|i| format!("w{i:02}")).collect();
        let capped = over.join(" ");

        TRUNCATION_WARNINGS.with(|c| c.set(0));
        // span 用の分割を 5 ヒットぶん呼んでも警告は出ない。
        for _ in 0..5 {
            let _ = query_phrases(&capped);
        }
        assert_eq!(
            TRUNCATION_WARNINGS.with(|c| c.get()),
            0,
            "the per-hit span path must stay silent"
        );

        let _ = build_fts_query(&capped);
        assert_eq!(
            TRUNCATION_WARNINGS.with(|c| c.get()),
            1,
            "compiling the query is the one place that should warn"
        );
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
        assert_eq!(q("grooveseek"), some("\"grooveseek\""));
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
    // 除外構文 (F-4): parse_query が作る 2 極の phrase 列と MATCH 式
    // ---------------------------------------------------------------------

    /// 除外側も正側と**同じ関数**を通るので、独立 emit の過剰さもそのまま出る。
    /// `-再ランキング` が `ランキング` まで落とすのは仕様であり、echo でそれを見せる。
    #[test]
    fn parse_query_tokenizes_the_excluded_group_with_the_positive_rules() {
        let p = parse_query("-再ランキング");
        assert_eq!(
            p.exclude,
            vec!["再ランキング".to_string(), "ランキング".to_string()]
        );
        assert!(p.include.is_empty());
        // quote すれば逐語 = 過剰除外の逃げ道。
        assert_eq!(
            parse_query("-\"再ランキング\"").exclude,
            vec!["再ランキング".to_string()]
        );
    }

    /// trigram の 3 文字床は除外側にも効く。届かない除外語は**何も落とさない**。
    #[test]
    fn an_excluded_phrase_under_the_trigram_floor_excludes_nothing() {
        let p = parse_query("foo -ab");
        assert!(p.exclude.is_empty());
        assert_eq!(p.negative_match(), None);
        assert_eq!(p.match_expr(), some("\"foo\""));
    }

    /// FTS5 の優先順位は NOT > OR なので、括弧を落とすと
    /// `"a" OR "b" NOT "c"` = `"a" OR ("b" NOT "c")` になり除外が片側にしか効かない。
    #[test]
    fn the_match_expression_parenthesises_both_sides_of_not() {
        assert_eq!(
            parse_query("rust tokio -async -\"sync io\"").match_expr(),
            some("(\"rust\" OR \"tokio\") NOT (\"async\" OR \"sync io\")")
        );
        // 片側 1 個でも形は同じ (一様な形はテストと probe を単純にする)。
        assert_eq!(
            parse_query("foo -bar").match_expr(),
            some("(\"foo\") NOT (\"bar\")")
        );
    }

    /// 除外を書かないクエリの出力は byte 単位で今日と同じ。
    #[test]
    fn a_query_without_exclusions_compiles_exactly_as_before() {
        for raw in [
            "再ランキングの評価について",
            "retry budget の設定",
            "\"Foundry Local\" の設定",
            "\"再ランキングの評価について\"",
            "E0382",
            "暗号化",
            "評価は",
            "システム化",
            "\"ab\" テスト",
            "ab",
            "",
            "   ",
            "エラー",
            "AI と ML",
            "sqlite-vec",
            "---",
            "foo \"bar\" AND",
        ] {
            assert_eq!(
                parse_query(raw).match_expr(),
                build_fts_query(raw),
                "{raw:?} must compile exactly as it did before exclusions existed"
            );
        }
    }

    /// 埋め込み / reranker / span に渡る文字列。除外 group と、それに続く
    /// whitespace run 1 つを切る (末尾 group なら直前の whitespace を切る)。
    #[test]
    fn positive_text_is_the_raw_query_with_the_exclusion_and_one_whitespace_run_cut() {
        assert_eq!(parse_query("rust -async").positive_text(), "rust");
        assert_eq!(parse_query("-async rust").positive_text(), "rust");
        assert_eq!(parse_query("a -b c").positive_text(), "a c");
        assert_eq!(parse_query("a -b -c").positive_text(), "a");
        assert_eq!(
            parse_query("foo -\"bar baz\" qux").positive_text(),
            "foo qux"
        );
    }

    /// 除外が無ければ **borrow** = raw と byte 単位で同一であることを型で示す。
    #[test]
    fn positive_text_borrows_the_raw_query_when_nothing_is_excluded() {
        assert!(matches!(
            parse_query("rust async").positive_text,
            Cow::Borrowed(_)
        ));
        assert!(matches!(
            parse_query("rust -async").positive_text,
            Cow::Owned(_)
        ));
    }

    /// fallback は positive_text ではなく **raw の positive region ごと**に効く。
    /// 連結後の文字列に効かせると、出力 phrase が raw の部分文字列でなくなる。
    #[test]
    fn the_fallback_applies_to_each_positive_region() {
        assert_eq!(
            parse_query("AI と ML -foo").match_expr(),
            some("(\"AI と ML\") NOT (\"foo\")")
        );
        // 正側が全部 3 文字未満 = FTS 半身は休み (vector 単独)。除外は vec 側で効く。
        assert_eq!(parse_query("xy -abc z").positive_match(), None);
        assert_eq!(parse_query("xy -abc z").match_expr(), None);
        assert_eq!(parse_query("xy -abc z").exclude, vec!["abc".to_string()]);
    }

    /// 拒否は「FTS phrase が作れたか」ではなく「埋め込む文字列が残ったか」で決める。
    #[test]
    fn an_exclusion_only_query_is_refused_and_a_short_positive_is_not() {
        for raw in ["-foo", "-\"ab\"", "-a -b", "  -foo  "] {
            let p = parse_query(raw);
            assert!(p.is_exclusion_only(), "{raw:?} has nothing to search for");
            assert!(p.require_positive().is_err(), "{raw:?}");
        }
        for raw in ["xy -abc z", "", "   ", "rust -async", "\"-foo\""] {
            let p = parse_query(raw);
            assert!(!p.is_exclusion_only(), "{raw:?} still has a positive side");
            assert!(p.require_positive().is_ok(), "{raw:?}");
        }
    }

    /// 上限は極性ごとに別枠。共有枠だと「除外を書いた位置」で正側の語が落ちる。
    #[test]
    fn each_polarity_has_its_own_phrase_cap() {
        let positives: Vec<String> = (0..40).map(|i| format!("w{i:02}")).collect();
        let negatives: Vec<String> = (0..40).map(|i| format!("-x{i:02}")).collect();
        let raw = format!("{} {}", positives.join(" "), negatives.join(" "));

        let p = parse_query(&raw);
        assert_eq!(p.include.len(), MAX_PHRASES);
        assert_eq!(p.exclude.len(), MAX_PHRASES);
        assert_eq!(p.include[MAX_PHRASES - 1], "w31");
        assert_eq!(p.exclude[MAX_PHRASES - 1], "x31");
        assert_eq!(p.dropped_include, 8);
        assert_eq!(p.dropped_exclude, 8);
    }

    /// 除外側の切り詰めも **1 検索 1 回**だけ警告する (正側と同じ理由)。
    #[test]
    fn the_exclusion_cap_warns_once_per_search_too() {
        let positives: Vec<String> = (0..MAX_PHRASES + 5).map(|i| format!("w{i:02}")).collect();
        let negatives: Vec<String> = (0..MAX_PHRASES + 5).map(|i| format!("-x{i:02}")).collect();
        let raw = format!("{} {}", positives.join(" "), negatives.join(" "));

        TRUNCATION_WARNINGS.with(|c| c.set(0));
        for _ in 0..5 {
            let _ = query_phrases(&raw);
        }
        assert_eq!(
            TRUNCATION_WARNINGS.with(|c| c.get()),
            0,
            "the per-hit span path must stay silent on both polarities"
        );

        parse_query(&raw).warn_if_truncated();
        assert_eq!(
            TRUNCATION_WARNINGS.with(|c| c.get()),
            2,
            "one warning per polarity that was actually truncated"
        );
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

    // ---------------------------------------------------------------------
    // 不変条件 (proptest) — 除外構文 (F-4)
    // ---------------------------------------------------------------------

    proptest::proptest! {
        /// 除外を書かないクエリでは埋め込む文字列が raw と **byte 単位で**同じ。
        /// 既存の embedding / eval が除外構文の導入で動かないことの根拠。
        #[test]
        fn positive_text_equals_the_raw_query_when_no_group_is_excluded(raw in ".{0,200}") {
            let p = parse_query(&raw);
            proptest::prop_assert_eq!(
                p.exclusion_groups == 0,
                p.positive_text() == raw,
                "raw {:?} -> {:?}", raw, p.positive_text()
            );
        }

        /// span 用の分割 ([`query_phrases`]) を positive text に対して行っても、
        /// FTS が実際に探した phrase と一致する。[`crate::server::compute_match_spans`]
        /// に positive text を渡してよい根拠がこれ。
        ///
        /// 成り立つのは「quote された除外の直後に literal なハイフンが続かない」
        /// クエリで、`foo -"bar"-baz` がその例外 (ADR-0011)。この proptest の
        /// alphabet `[^"]{0,120}` は quote を作れないので例外は生成され得ず、
        /// ここで固定できるのは例外を除いた側だけである。
        #[test]
        fn the_include_phrases_of_the_positive_text_are_the_include_phrases_of_the_raw_query(
            raw in "[^\"]{0,120}"
        ) {
            let p = parse_query(&raw);
            proptest::prop_assert_eq!(query_phrases(p.positive_text()), p.include.clone());
        }

        /// 中心的な不変条件は除外側にも及ぶ: 除外 phrase も元クエリの連続部分文字列。
        #[test]
        fn excluded_phrases_are_substrings_of_the_input_too(raw in "[^\"]{0,120}") {
            for phrase in parse_query(&raw).exclude {
                proptest::prop_assert!(
                    raw.contains(&phrase),
                    "excluded phrase {phrase:?} is not a substring of {raw:?}"
                );
            }
        }

        /// byte index が常に char 境界であること (除外 span の切り出しを含む)。
        #[test]
        fn parse_query_never_panics_on_arbitrary_text(raw in ".{0,200}") {
            let p = parse_query(&raw);
            let _ = p.match_expr();
            let _ = p.positive_text();
            let _ = p.require_positive();
        }
    }
}
