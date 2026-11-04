//! Retrieval-quality check for a Japanese `.docx` shaped like a set of work rules: chapters
//! with no body of their own, each followed directly by its articles (AX-20).
//!
//! # Why this exists
//!
//! feature-63 (v1.17.0) stopped a `.docx` heading with no body text from becoming a chunk of
//! its own: its text now opens the body of the next section that has one. The parser and
//! index tests pin *how* the document is cut. None of them asks what the change was for:
//! **does a search over such a document, mixed with unrelated Markdown, return the article
//! that answers the query, and keep the empty chapter excerpts out of the top results?** The
//! report behind feature-63 (kuriya #323) was exactly that: with `bge-m3`, the empty chapter
//! chunks of a set of Japanese work rules ranked third to fifth for a query that had nothing
//! to do with them.
//!
//! # Design notes (read before changing the fixtures)
//!
//! - **The query words live in the article bodies, not in the headings or the filename.**
//!   The file ([`crate::RULES_PATH`]) has no `docProps/core.xml`, so its title is the
//!   meaningless file name, and every query paraphrases its article's body rather than
//!   naming the article. A
//!   heading-only match would otherwise let the right chunk rank first for the wrong reason,
//!   as the binary-format retrieval tests in this directory also guard against.
//! - **The empty-chunk assertion is the one the fix carries.** With the fold in place the
//!   index holds no empty chunk at all, so on the fixed code that assertion holds trivially;
//!   it fails only when the chapters become empty chunks again. That is checked by
//!   reverting the fold by hand (AX-20 mutation run), not by this test.
//! - **Two paraphrases share no three-character run with the rules**, so the trigram
//!   full-text side cannot match them and only the vector side can rank the article.
//!   [`crate::paraphrases_share_no_trigram_with_the_rules`], which needs no model, keeps that
//!   true.
//! - **Unrelated queries are part of the set.** kuriya #323 was seen on a query about a
//!   lost laptop, which no article answers; one of the Markdown distractors does.
//! - **The model is BGE-M3 ([`crate::MODEL`])**: the corpus and the queries are Japanese,
//!   which the English `bge-small` model does not read.
//!
//! `#[ignore]`: downloads BGE-M3 (~2.3 GB) on first use and builds an index. Run with
//! `cargo test --test eval_docx_japanese_rules -- --ignored`. `nightly.yml` runs it on the
//! Linux leg only: its skip list for the windows and macOS legs names it, as it names the
//! other BGE-M3 tests (the skip is a substring match; its line matches this test alone).

mod common;
use common::cli::{run_status_count, search_json};
use common::eval_gate::{groove_bin, index_corpus, pinned_config};
use common::ooxml::zip_parts;
use common::temp::TempKbLayout;

use std::path::Path;
use std::process::Command;

// ---------------------------------------------------------------------------
// Fixture
// ---------------------------------------------------------------------------

/// The preamble, before the first chapter.
const PREAMBLE: &str =
    "この規程は、当社の従業員が日々の勤務にあたって守るべき基本的な事項を定めることを目的とする。";

/// A chapter heading, with no body of its own, and the articles under it.
struct Chapter {
    heading: &'static str,
    articles: &'static [Article],
}

/// An article heading, its body, and a query that paraphrases the body.
struct Article {
    heading: &'static str,
    body: &'static str,
    query: &'static str,
}

const CHAPTERS: &[Chapter] = &[
    Chapter {
        heading: "第1章 総則",
        articles: &[
            Article {
                heading: "第1条（適用範囲）",
                body: "この規程は、正社員、契約社員及びパートタイム従業員に適用し、派遣元に雇用される派遣社員には適用しない。",
                query: "派遣社員にもこのルールは適用されますか",
            },
            Article {
                heading: "第2条（秘密保持）",
                body: "従業員は、職務上知り得た顧客の個人情報を、在職中はもとより退職した後も第三者に漏らしてはならない。",
                query: "会社を辞めた後でもお客様の個人情報を話してはいけないのか",
            },
        ],
    },
    Chapter {
        heading: "第2章 勤務",
        articles: &[
            Article {
                heading: "第3条（始業及び終業）",
                body: "始業時刻は午前九時、終業時刻は午後六時とし、正午から午後一時までの一時間を昼の休憩時間とする。",
                query: "朝は何時に出社して夕方は何時に帰れるか",
            },
            Article {
                heading: "第4条（年次有給休暇）",
                body: "入社日から六か月間継続して勤務し、所定労働日の八割以上出勤した従業員には、十日の年次有給休暇を与える。",
                query: "有給休暇は入社してから何日もらえるか",
            },
        ],
    },
    Chapter {
        heading: "第3章 賃金",
        articles: &[
            Article {
                heading: "第5条（支払日）",
                body: "給与は毎月末日に締め切り、翌月二十五日に従業員があらかじめ指定した銀行口座へ振り込んで支払う。",
                query: "給料は毎月何日に口座へ振り込まれるか",
            },
            Article {
                heading: "第6条（通勤手当）",
                body: "通勤手当として、公共交通機関を利用する最も経済的な経路の定期券代を、一か月五万円を上限に支給する。",
                query: "電車の定期券代はいくらまで会社が出してくれるか",
            },
        ],
    },
];

/// `(article heading, query)`: queries that share no three-character run with any heading or
/// body of the rules, so the full-text side (a trigram index) cannot match them and only the
/// vector side can rank the article. [`paraphrases_share_no_trigram_with_the_rules`] keeps
/// that true when the fixture changes.
const PARAPHRASES: &[(&str, &str)] = &[
    (
        "第3条（始業及び終業）",
        "お昼ごはんに使える時間はどれくらいか",
    ),
    (
        "第2条（秘密保持）",
        "辞めた社員がお客さんの住所を他人にしゃべってよいか",
    ),
];

/// Queries no article answers. kuriya #323 saw the empty chapter excerpts rank third to
/// fifth for the first one.
const UNRELATED_QUERIES: &[&str] = &[
    "パソコンをなくしてしまった",
    "味噌汁のだしの取り方",
    "自転車のタイヤがパンクした時の直し方",
];

/// Japanese Markdown on subjects no article touches, with a generic heading so that only
/// their bodies can match. Without them the corpus is one document and every rank is
/// trivially right.
const DISTRACTORS: &[(&str, &str)] = &[
    (
        "m01.md",
        "# メモ\n\nノートパソコンを紛失した場合は、すぐに情報システム部のヘルプデスクへ連絡し、遠隔ロックとパスワードの変更を依頼してください。\n",
    ),
    (
        "m02.md",
        "# メモ\n\n味噌汁のだしは、昆布を水に三十分浸してから弱火にかけ、沸騰する直前に取り出して鰹節を加えると香りよく取れる。\n",
    ),
    (
        "m03.md",
        "# メモ\n\n自転車のパンクは、タイヤレバーでチューブを外し、水を張った洗面器に沈めて泡の出る穴を探してからパッチを貼って直す。\n",
    ),
    (
        "m04.md",
        "# メモ\n\n金魚を飼う水槽の水は、塩素を抜いた汲み置きの水を使い、一度に全部ではなく三分の一ずつ週に一回取り替えるのがよい。\n",
    ),
    (
        "m05.md",
        "# メモ\n\n冬の低山登山でも、汗冷えを防ぐために綿の肌着は避け、化繊やウールの重ね着で体温を調節するのが基本である。\n",
    ),
    (
        "m06.md",
        "# メモ\n\n観葉植物の水やりは、鉢の土の表面が乾いてから鉢底から流れ出るまでたっぷり与え、受け皿の水は捨てておく。\n",
    ),
    (
        "m07.md",
        "# メモ\n\n囲碁では、隅、辺、中央の順に打つのが効率がよいとされ、序盤は四隅の星や小目に石を置いて地を囲いにいく。\n",
    ),
    (
        "m08.md",
        "# メモ\n\n写真の露出は、絞り、シャッター速度、感度の三つで決まり、絞りを開ければ背景がぼけてシャッター速度を速くできる。\n",
    ),
];

/// `word/document.xml` of the rules: the preamble, then each chapter as a `Heading1`
/// paragraph followed directly by its articles, each a `Heading2` paragraph over its body.
/// The spelled style IDs are read without a styles part.
fn rules_document_xml() -> String {
    fn para(style: Option<&str>, text: &str) -> String {
        let ppr = style
            .map(|s| format!(r#"<w:pPr><w:pStyle w:val="{s}"/></w:pPr>"#))
            .unwrap_or_default();
        format!("<w:p>{ppr}<w:r><w:t>{text}</w:t></w:r></w:p>")
    }
    let mut body = para(None, PREAMBLE);
    for chapter in CHAPTERS {
        body.push_str(&para(Some("Heading1"), chapter.heading));
        for article in chapter.articles {
            body.push_str(&para(Some("Heading2"), article.heading));
            body.push_str(&para(None, article.body));
        }
    }
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?><w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body>{body}</w:body></w:document>"#
    )
}

/// The rules as a `.docx`, with no `docProps/core.xml` so the title falls back to the file
/// name.
fn rules_docx() -> Vec<u8> {
    let doc_xml = rules_document_xml();
    zip_parts(&[
        (
            "[Content_Types].xml",
            r#"<?xml version="1.0"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/></Types>"#,
        ),
        ("word/document.xml", &doc_xml),
    ])
}

// ---------------------------------------------------------------------------
// Test
// ---------------------------------------------------------------------------

const RULES_PATH: &str = "r1.docx";
const MODEL: &str = "bge-m3";
const TOP_K: usize = 5;

/// `groove --config <cfg>`, the command [`crate::common::cli`] reads from.
fn groove_with_config(cfg: &Path) -> Command {
    let mut cmd = Command::new(groove_bin());
    cmd.arg("--config").arg(cfg);
    cmd
}

/// `groove --config <cfg> search <query> --kb-path <kb> --format json --model <model> --limit <k>`
/// ([`crate::common::cli::search_json`]), with the model [`MODEL`] and k = [`TOP_K`],
/// returning the `results` array. The quality filter and the confidence cutoff keep their
/// defaults.
fn search(cfg: &Path, kb: &Path, query: &str) -> Vec<serde_json::Value> {
    let limit = TOP_K.to_string();
    let v = search_json(
        groove_with_config(cfg),
        kb,
        query,
        &["--model", MODEL, "--limit", &limit],
    );
    v["results"].as_array().cloned().unwrap_or_default()
}

/// One line per hit, `path | heading | content`, for failure messages.
fn describe(hits: &[serde_json::Value]) -> String {
    hits.iter()
        .enumerate()
        .map(|(i, h)| {
            format!(
                "    {}. {} | {} | {:?}",
                i + 1,
                h["path"].as_str().unwrap_or("?"),
                h["heading"].as_str().unwrap_or("<none>"),
                h["content"].as_str().unwrap_or("")
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// A hit that is a chapter heading's own excerpt: a chunk of the rules whose content is empty
/// once trimmed, or whose heading is a chapter's. Either is what feature-63 removed.
fn is_empty_chapter_excerpt(hit: &serde_json::Value) -> bool {
    if hit["path"].as_str() != Some(RULES_PATH) {
        return false;
    }
    let empty = hit["content"].as_str().unwrap_or("").trim().is_empty();
    let chapter = hit["heading"]
        .as_str()
        .is_some_and(|h| CHAPTERS.iter().any(|c| c.heading == h));
    empty || chapter
}

#[test]
#[ignore = "requires embedding model download (BGE-M3 ~2.3 GB) and an index build"]
fn eval_japanese_rules_docx_chapter_article_ranks_the_article() {
    let kb = TempKbLayout::new("groove-eval-ja-rules");
    // `groove.toml` in the layout's cleanup root, beside the `.groove.db`; `[parsers]` is
    // the only key, so the defaults of retrieval apply.
    let cfg = pinned_config(kb.root(), &["md", "docx"]);
    kb.write_bytes(RULES_PATH, &rules_docx());
    for (path, content) in DISTRACTORS {
        kb.write(path, content);
    }

    // 1) Index. `[parsers].enabled` is config-only, hence `--config`.
    index_corpus(kb.kb(), &cfg, MODEL);

    // 2) Every document made it in, so a parser that skipped the `.docx` reads as that
    //    rather than as a retrieval miss below. The count is read whole: `Documents: 10`
    //    is not 1.
    let expected_docs = 1 + DISTRACTORS.len() as u64;
    let documents = run_status_count(groove_with_config(&cfg), kb.kb(), "Documents:");
    assert_eq!(
        documents, expected_docs,
        "expected {expected_docs} indexed documents, got {documents}"
    );

    let mut failures = Vec::new();

    // 3) Each article's query ranks that article's chunk first. Rank 1 is the assertion that
    //    carries information: with one `.docx` among eight short notes, "somewhere in the top
    //    five" cannot fail.
    for chapter in CHAPTERS {
        for article in chapter.articles {
            let hits = search(&cfg, kb.kb(), article.query);
            let top = hits.first();
            let top_path = top.and_then(|h| h["path"].as_str());
            let top_heading = top.and_then(|h| h["heading"].as_str());
            if top_path != Some(RULES_PATH) || top_heading != Some(article.heading) {
                failures.push(format!(
                    "  query {:?} should rank {} / {} first; got:\n{}",
                    article.query,
                    RULES_PATH,
                    article.heading,
                    describe(&hits)
                ));
            }
            if hits.iter().any(is_empty_chapter_excerpt) {
                failures.push(format!(
                    "  query {:?} has an empty chapter excerpt in its top {TOP_K}:\n{}",
                    article.query,
                    describe(&hits)
                ));
            }
        }
    }

    // 3b) The same for the paraphrases the full-text side cannot match.
    for (heading, query) in PARAPHRASES {
        let hits = search(&cfg, kb.kb(), query);
        let top = hits.first();
        if top.and_then(|h| h["path"].as_str()) != Some(RULES_PATH)
            || top.and_then(|h| h["heading"].as_str()) != Some(*heading)
        {
            failures.push(format!(
                "  paraphrase {query:?} should rank {RULES_PATH} / {heading} first; got:\n{}",
                describe(&hits)
            ));
        }
        if hits.iter().any(is_empty_chapter_excerpt) {
            failures.push(format!(
                "  paraphrase {query:?} has an empty chapter excerpt in its top {TOP_K}:\n{}",
                describe(&hits)
            ));
        }
    }

    // 4) A query no article answers keeps the chapter excerpts out of its top results too:
    //    the shape kuriya #323 reported.
    for query in UNRELATED_QUERIES {
        let hits = search(&cfg, kb.kb(), query);
        if hits.iter().any(is_empty_chapter_excerpt) {
            failures.push(format!(
                "  unrelated query {query:?} has an empty chapter excerpt in its top {TOP_K}:\n{}",
                describe(&hits)
            ));
        }
    }

    assert!(
        failures.is_empty(),
        "the Japanese rules did not rank as expected:\n{}",
        failures.join("\n")
    );
}

/// Model-free guard for [`PARAPHRASES`]: none shares a run of three characters with the
/// preamble or with any heading or body of the rules, and each names a real article. Edit
/// the fixture and this fails before the paraphrases quietly turn into full-text matches.
#[test]
fn paraphrases_share_no_trigram_with_the_rules() {
    let mut texts: Vec<&str> = vec![PREAMBLE];
    for chapter in CHAPTERS {
        texts.push(chapter.heading);
        for article in chapter.articles {
            texts.push(article.heading);
            texts.push(article.body);
        }
    }
    let corpus = texts.join("\n");
    for (heading, query) in PARAPHRASES {
        assert!(
            CHAPTERS
                .iter()
                .flat_map(|c| c.articles)
                .any(|a| a.heading == *heading),
            "paraphrase {query:?} names no article: {heading:?}"
        );
        let chars: Vec<char> = query.chars().collect();
        let shared: Vec<String> = chars
            .windows(3)
            .map(|w| w.iter().collect::<String>())
            .filter(|t| corpus.contains(t.as_str()))
            .collect();
        assert!(
            shared.is_empty(),
            "paraphrase {query:?} shares {shared:?} with the rules"
        );
    }
    // The check itself can fail: a run copied from a body is found.
    assert!(corpus.contains("通勤手"));
}
