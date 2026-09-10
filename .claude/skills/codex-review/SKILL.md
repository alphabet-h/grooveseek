---
name: codex-review
description: push 前に doc comment の名前を洗い、ローカルの Codex (adversarial-review) で前掃除してから、PR で `@codex review` を trigger し、3 endpoint (inline / reviews / issue) を `(id, updated_at)` set diff で同時 polling、state-base convergence + P0/P1 absence + sentinel text の 3 layer で判定、wall-clock timeout / error string detect / cost-aware retry cap で hardening したオーケストレータ
argument-hint: <PR#> [max_rounds] [per_round_timeout_sec]
---

# /codex-review

GitHub PR で codex review (`chatgpt-codex-connector[bot]`) を **trigger + 3 layer convergence
detection + 結果 fetch + 整形** する。`/feature-flow` Phase 6 の sub-step、または手動の単独 cycle。

**1 round = `scripts/codex_review_round.sh` を 1 回実行する。** 実体はその script 1 本で、
踏んだ罠の回避はすべて script の中 — **守っているコードの横のコメント**にある。
一覧は数えずに引く: `grep -oE '罠 [0-9]+' .claude/skills/codex-review/scripts/codex_review_round.sh | sort -u`
(番号の由来: `.dev/knowledge/codex-review-loop-pitfalls.md`)。

引数: `<PR#>` 必須 / `<max_rounds>` default **3** (罠 16: 25 credits × 3) / `<per_round_timeout_sec>` default 600 (罠 9)。
3 つとも **そのまま script に渡す**。round 番号は script が PR の `@codex review` 投稿履歴から導くので、
上限は process を跨いで効く (controller が数えなくてよい、数えてはいけない)。
前提: `gh auth status` OK、repo に codex connector app install 済。destructive 操作なし (GitHub へ comment を post するだけ)。

## push する前に doc comment の名前を洗う

codex は `AGENTS.md` の rule 5 (「tree の中にあるものを名指しするならリンクにする」) を
**字義どおり**適用する。人間の review が認める例外 — private item は module link + 散文、
`tests/` は rustdoc の対象外 — を codex は認めず、**round を 1 つ使って P1 で返す**。
PR #222 / #234 / #236 / #237 で起きていて、#234 では 3 round 連続した
(台帳 `.dev/knowledge/repeat-offences-ledger.md` の category 4)。round は `<max_rounds>` で
頭打ちなので、これは review に見つけさせるものではない。

**打つのは、この branch を push するたび。** PR を開く直前も、review の指摘を直した後も同じで、
P0/P1 だけでなく**収束した round の P2/P3 を取り込んだ push も含む** (codex P2 on #238: 「収束後の
取り込みは merge へ向かうので、そこで足した doc comment を誰も見ない」)。指摘された行だけ直して
push すると、同じ形が次の round で返ってくる (#234 / #236 はそれで round を溶かした):

```bash
python .dev/tools/doc_link_sweep.py
```

**引数は無い。縮めない。** script が内部で `git diff main...HEAD -- '*.rs'` を固定で打ち、
tree の item index (fn / struct / enum / field / variant / const / mod …) と突き合わせて
bucket に振り分ける。exit 1 なら直してから push。手順として grep を打っていた時代
(PR #238〜2026-09-10) に、範囲を `git diff -- '*.rs'` に縮めた (台帳 #39) / private だからと
外した (#43) / 形で skip した (#52) の 3 つは、どれも「判断を挟める場所」があったから
起きた。script はその場所を無くす。`.dev/` が無い環境では旧形を打つ:
`` git -C <abs> diff main...HEAD -- '*.rs' | grep -E '^\+\s*//[/!]' | grep -oE '\[?`[^`]+`\]?' | sort | uniq -c ``
(pathspec は `'*.rs'` — directory を並べると `grooveseek/benches` が落ちる、codex P2 on #238)。

script の bucket と、それぞれの直し方 (角括弧を残して抽出しているので、**bare backtick と
`` [`..`] `` が同じ出力の中で区別できる** — sweep は両方向):

| bucket | 出力の項 | どうするか |
|---|---|---|
| `BARE_TREE_ITEM` | tree の中の item (fn / struct / const / module / field) が bare backtick で、この doc から張れる (同 file、または `pub` / `pub(crate)`)。**判定順は `TEST_FN_NAME` → `PRIVATE_ELSEWHERE` → ここ**: 当たりが test item だけなら同 file でも `TEST_FN_NAME` に行き、exit 1 にならない | `` [`path`] `` に直す。script が定義位置と候補 path を添える。当たりに test item と非 test item が混ざる (`, test` 印) なら、link するのは非 test の方 |
| `PRIVATE_ELSEWHERE` | tree にはあるが張れない: 他 file の private item、`tests/` / `benches/` から見た lib の `pub(crate)` | item は backtick のまま、**持ち主の module を link する** — lib の中からは `` [`crate::…`] ``、`tests/` / `benches/` からは別 crate なので `` [`grooveseek::…`] `` (`AGENTS.md` の「Link the module and leave the item in prose」。散文だけでは検査されない、codex P1 on #296 round 1 / 2)。exit 1 にしない |
| `LINKED_NOT_IN_TREE` | tree の外 (std / 依存 crate / SQL 語 / attribute / CLI 名 / file path / MCP tool 名) が `` [`..`] `` | backtick に戻す。**リンクにするのも P1** (#236 round 3 の `` `serde_json::Value` ``) |
| `MODULE_DOC_RELATIVE_LINK` | `//!` の中の `` [`..`] `` が `crate::` / `std::` (`core::` / `alloc::`) / workspace crate 名で始まらない。**`Self::` も含めて落とす** (module doc に `Self` は無い) | 絶対 path に (台帳 #40。`cargo doc` は private import で通してしまう) |
| `COMPOSITE` | `::` / 演算子 / `{}` / `;` / 空白を含む項 — `` `use super::*;` `` や `` `limit * FILTER_OVERFETCH_FACTOR` `` や `` `Database: Debug` `` | **中の名前を 1 つずつほどいて**上の行を適用する。#237 round 1 と台帳 #52 の P1 はこの形 |
| `FILE_NAME` | `` `foo.rs` `` / `` `ADR-0013` `` で名指し | module link か markdown link か散文に。**file 名は書かない** (台帳 #41 / #42 / #46) |
| `TEST_FN_NAME` | index の当たりが `#[cfg(test)]` / `#[test]` / `tests/` の item だけ (同 file でも、ここが先)。**bare でも `` [`..`] `` でも出す** — link 済みは `linked:` 印 (`cargo doc` が検証しない link は通っていても未検証) | 同じ test mod の中なら bare link、非 test の doc からは backtick + 散文 (下の規則)。script は決めない |

exit 1 になるのは `BARE_TREE_ITEM` / `LINKED_NOT_IN_TREE` / `MODULE_DOC_RELATIVE_LINK`。
`PRIVATE_ELSEWHERE` / `COMPOSITE` / `FILE_NAME` / `TEST_FN_NAME` は人が読む
(誤検出もあるが、**形で skip すると #52 になる**)。script の「張れる」は近似 (同 file か
`pub` 系か) なので、迷ったら下の規則の最後の行 = link にして `cargo doc --no-deps` を 1 回回す。`--whole-tree` は diff ではなく tree 全体を
出す計測用で、push 前には使わない。

**identifier だけに絞らない。** 旧 grep の 2 つ目を `` '\[?`[A-Za-z_][A-Za-z0-9_:]*`\]?' `` にすると
項は減る (#237 の diff で 170 → 100。via: 旧コマンドの `grep -oE` をそれに差し替え、末尾を
`sort -u | wc -l` にして `fe7ac23...70dc178` で実行) が、落ちるのは `COMPOSITE`
= 実際に P1 を受けた形なので、絞ると意味が無くなる。script はこの理由で全 span を読む。

`` [`..`] `` の path の作り方 (実測は `.dev/knowledge/comments-the-compiler-cannot-see.md`):

- **`//!` の中は bare 名も `self::` も解決しない。** 同じ file の item でも
  `` [`crate::db::search`] `` と絶対で書く
- `///` は同 scope なら bare でよい。`` [`Self::method`] `` も張れる
- `tests/` crate から lib の **`pub`** item は `` [`grooveseek::db::ParsedQuery::match_expr`] ``
  の形で書く。**`pub(crate)` は届かない**ので backtick + 散文。なお `tests/` の target は
  `doc = false`、`benches/` は `cargo doc` が document しないので、この 2 つの中の link を
  検査するのは `cargo doc` ではなく codex だけ
- 同じ `#[cfg(test)] mod tests` の中なら、隣の test fn 名は bare link
- **`#[cfg(test)]` の item へのリンクは `cargo doc` が検証しない。** 存在する名前でも
  存在しない名前でも `cargo doc --no-deps` は exit 0 (2026-09-04 に対照つきで確認)。
  rustdoc は test module を解決しないので、**リンクにすると「検証済みの参照」に見えて
  何も検証していない**状態になる。逆方向 = **非 test の doc から test item へは link を
  書けない**: `cargo doc --no-deps -p grooveseek --all-features --document-private-items` が
  `no item named 'tests' in module 'code'` で exit 101 (2026-09-10、`plugin.rs` の `pub(crate) fn`
  に probe を置いて確認。`--document-private-items` 無しだと `pub(crate)` の doc は生成されず
  exit 0 になる — probe は必ずこの flag で打つ)。非 test の doc から test 名を呼ぶときは backtick +
  持ち主を散文で書き、**stale 検出が要るなら test で書く** (`include_str!` して
  `fn <name>(` を探す形。PR #263 の
  `a_test_named_by_a_doc_comment_in_this_file_still_exists`)。codex はここをリンクにせよと
  P2 で言ってくるので、**対照 (名前を 1 文字変えて `cargo doc`) を添えて返す**
- **同じ module の doc から、その module の private item へは張れる。** `const LENGTH_PENALTY`
  のような非 `pub` の const でも、`quality.rs` の `//!` / `///` からなら rustdoc が解決する
  (2026-09-04 に確認: リンク化して `cargo doc --no-deps` が exit 0、対照として 1 語を存在しない
  名前に変えると `error: unresolved link` で exit 101 = lint は生きている)。**「private」の一語で
  下の行へ振らない** — 台帳 #43 はそれで P2 を受けた
- **リンクにできない item は backtick のまま残し、持ち主を名指す** — 持ち主の module が
  link できるなら **必ず `` [`crate::…`] `` で link する** (`AGENTS.md`「Link the module and leave the
  item in prose」: **他** module に private な item、`tests/` crate から見た lib の `pub(crate)` item)。
  散文だけで済ませてよいのは、link できる持ち主が無い場合だけ: 非 test の doc から名指した
  `#[cfg(test)] mod tests` の中の item (module 自体が rustdoc に無い) / 別の `tests/` crate の
  test fn。**item だけが `#[cfg(test)]` で gate されている** (`db.rs` の `rrf_topk`、`config.rs` の
  `discover_at` のような形) なら持ち主 module は rustdoc にあるので、`` [`crate::db`] `` の
  module link は要る (codex P1 on #296 round 2)。
  **迷ったらリンクにして `cargo doc --no-deps` を 1 回回す** — 張れないなら
  `unresolved link` で落ちるので、推測する必要が無い

**見えるのは追加行だけ** (`^+` で絞っている)。既存行に残った古い名前はここには出ない —
そちらは `cargo doc` と review 側の仕事。分類の実例は
`.dev/knowledge/archive/prs/pr237-feature-55-pr2-sweep.md` の表、script の設計と tree 全体の
計測値は `.dev/knowledge/doc-link-sweep-script.md`。

## push する前にローカルの Codex で前掃除する

GitHub の round は 1 回 25 credits と 10 分強を使い、fix を push するたびに P2 が返る連鎖に
なりやすい (feature-58 / PR #291 は 15 round、内訳は `.dev/knowledge/feature-58-summary.md` の
「codex round」節)。**同じ目 (Codex CLI) に push 前の diff を見せて収束させ、GitHub は確認の
round に使う** — 2026-09-09 の user 判断。役割は **ローカル = 明白な違反の前掃除、GitHub = 最終確認**:
ローカルが approve でも GitHub は P2 を出す (同 summary の r7〜r9) し、focus を当てればローカルが
GitHub より先に本物を拾う (同 r4 / r5、round 10 後の 2 件)。どちらか一方で済ませない。

**plugin / CLI が無い環境では前掃除なしで GitHub round のみ**。無いときは push 前の実行が
黙って skip にはならず止まる (plugin 未 install は `Cannot find module` で exit 1、CLI 無しは
`ensureCodexAvailable` の install 案内)。**未 login だけは事前に検査されない** — review は
`ensureCodexAvailable` (CLI の有無だけ) しか通らず、認証は app-server が起動してから失敗する。
login 状態は自分で `/codex:setup` を打って確かめ、無ければ `codex login`。止まった時点で
入れるか諦めるかを決め、諦めたなら下の GitHub round を最初から回す。

**打つのは、この branch を push するたび** (上の sweep と同じ)。PR を開く前も、GitHub round の
指摘を直した後も同じ。

前提: plugin `codex@openai-codex` が install 済 (`~/.claude/plugins/installed_plugins.json` に載る)、
`codex --version` が 0.153.4 以上 (古いと review の既定 model `gpt-5.3-codex` を ChatGPT アカウントが
400 で拒む)、`codex login` 済 (`/codex:setup` が状態を出す)。

### 実行形

下の `codex_review_round.sh` と同じ受け方 — controller (= main agent) が `run_in_background` で打ち、
stdout / stderr を scratchpad の file に分ける。**subagent `codex:codex-rescue` に打たせない**:
plugin の agent 定義は `task` だけを forward し、`adversarial-review` は呼ばないと決めている
(plugin の `agents/codex-rescue.md`「Forwarding rules」)。2026-09-09 はそれに反する prompt で動いて
いたが、plugin の更新で壊れる形なので採らない:

```bash
S=<scratchpad>
CODEX_PLUGIN=$(ls -d ~/.claude/plugins/cache/openai-codex/codex/*/ | sort -V | tail -1)
node "${CODEX_PLUGIN}scripts/codex-companion.mjs" adversarial-review --wait --base main --model gpt-5.6-terra --cwd <abs repo> "$(cat "$S/local-1-focus.txt")" > "$S/local-1.out" 2> "$S/local-1.err"; echo exit=$?
```

- path は `ls | sort -V | tail -1` で解決する。`${CLAUDE_PLUGIN_ROOT}` は plugin 自身の command /
  agent の中でしか定義されず、version の directory を literal で書くと plugin の更新で外れる
- `--model` は明示する。既定の `gpt-6-astra` は混雑で落ちることがあり、`gpt-5.3-codex*` は ChatGPT
  アカウントで 400。通る model の表と runtime の罠は `.dev/knowledge/codex-plugin-review-model-pitfalls.md`
  (ここに写さない)
- **focus は Write で file にして `"$(cat file)"` で渡す**。二重引用符の中に直接書くと backtick が
  command substitution される (`tune: command not found`、kuriya trap #159)
- npm で `@openai/codex` を更新した直後は、更新前に起動した shared runtime の `codex.exe` が残って
  同じ 400 を返す。`Get-Process codex` の StartTime が更新より前なら `taskkill` する (desktop app の
  `AppData\Local\OpenAI\Codex\bin\…\codex.exe` は別物、触らない)
- 対象を決めるのは **`--base main`** (= `main...HEAD` の diff。plugin の `scripts/lib/git.mjs` の
  `resolveReviewTarget` は `base` があれば `scope` を見ない)。`--scope branch` は `--base` 無しの時に
  default branch を検出する別経路なので、`--base main` と並べて書かない (ローカル r1 の medium)。
  **未 commit の変更は対象外** — commit してから打つ。`.md` だけの diff でも動く

### focus の書き方

plugin の `prompts/adversarial-review.md` の `User focus:` に差し込まれ、「weight it heavily」で読まれる。
効いた形は 3 要素 (feature-58 の r4 / r5 と round 10 後、`.dev/knowledge/feature-58-summary.md` の
「ローカル terra」の段落):

1. **判定基準の名指し**: `AGENTS.md` の Code Review Rules と同じ基準で、P1 (壊れる) / P2 (edge case) を
   分けて列挙させる
2. **過去指摘の一覧 + 「その先を探せ」**: GitHub / ローカルで既に受けた指摘を箇条書きにし、同じ軸 —
   中断の状態機械 / 再 parse を飛ばす経路 / 上限の合成 (transport との整合) / 別入口の漏れ — で
   まだ指摘されていないものを探させる
3. **対象 module / 概念の名指し**: 「`document_fields` の全 reader / writer を辿れ」の形。名指しの無い
   round は approve が浅く、GitHub が次の round で P2 を出した (r7〜r9)

`claim_guard` C1 は focus 文にも効く (`4096 rows` + `every` で止まった)。数を書くなら定数名で書く。

### ローカルの結果の読み方 (GitHub round の「結果の読み方」とは別の表)

stdout の並びは header (`# Codex Adversarial Review`) / `Target:` / **`Verdict: approve|needs-attention`** /
summary 1 段落 / `Findings:` か `No material findings.` (plugin の `scripts/lib/render.mjs` の
`renderReviewResult`)。finding は `- [<severity>] <title> (<file>:<line>)` + 本文 + `Recommendation:` で、
severity は **`critical` / `high` / `medium` / `low`** の 4 段 (同 file の `severityRank`、この順に並ぶ)。
`grep -n '^Verdict:'` と `grep -n '^- \['` で引く — 隣接に頼らない。

| Verdict | controller の手 |
|---|---|
| `approve` | sweep を済ませて push |
| `needs-attention` | **`[critical]` / `[high]` / `[medium]` は push を止める** — 取り込むか、反証できる指摘は **実測つきの反証**を次の focus に書いて再実行 (「一理ある」で従わない — 台帳 category 6 の 23 回目)。`[low]` だけなら内容を見て即決: **skip なら push してよい、取り込むなら diff が変わるので次の round を打つ** (その round も上限に数える) |
| exit ≠ 0 / `Verdict` 行が無い | `local-N.err` を読む。model 拒否 (400 / 404) / capacity / runtime の残留を切り分ける。判定材料が無いだけで「指摘なし」ではない |

**上限は push 1 回につき 3 round** — 打った回数で数える (approve で終わる round も、`[low]` を取り込んで
打ち直した round も 1 つ)。上限の出所は 2026-09-09 の user 判断「ローカルで収束させてから GitHub round」
(`.dev/knowledge/feature-58-summary.md` の「後続」節に記録。GitHub round の上限が 3 / 5 なのと同じ理由 =
cost と、収束しない loop は spec の問題という判定)。**3 round 目が `needs-attention` で終わったら**:
`[critical]` / `[high]` が残っているなら fix が次の指摘を生んでいる (台帳 category 6) = user に相談
(介入ポイント 3)。`[medium]` / `[low]` だけなら取り込んで **4 round 目は打たず push** し、取り込んだ内容を
PR 本文に書いて GitHub round に確認させる (GitHub が最終確認、の役割どおり)。自分で上限を上げない。
**fix を書いたら「その fix の最悪ケース」を自分で 1 つ書いてから出す** — r10 の fix (KNN の page が
空でも広げる) は r11 で「match 0 の corpus が cap まで広げ続ける」と返った。

ローカルは GitHub の代わりにならない。GitHub round (下の節) は残し、sentinel / P-badge の判定は
これまでどおり script が持つ。

## 1 round の回し方 (controller = main agent)

Phase A 最大 600 s + quiet window 180 s で **tool の 10 分上限を超え得る**ので、`run_in_background` で回し、
stdout / stderr を scratchpad の file に分けて受ける (stdout = review の中身、stderr = 進捗、ASCII のみ):

```bash
S=<scratchpad>
bash .claude/skills/codex-review/scripts/codex_review_round.sh <PR#> <max_rounds> 600 > "$S/r1.out" 2> "$S/r1.err"; echo exit=$?
```

re-review round (P0/P1 を fix して push した後) は **body file を第 3 引数に渡す**。本文は
`@codex review` で始め、続けて fix sketch。**`@codex` を本文中で bare word でも使わない、
verb は `review` のみ** (罠 15 / 18)。heredoc ではなく Write で file にしてから渡す:

```bash
bash .claude/skills/codex-review/scripts/codex_review_round.sh <PR#> <max_rounds> 600 "$S/r2-body.md" > "$S/r2.out" 2> "$S/r2.err"; echo exit=$?
```

round ごとに別プロセスで、状態は持たない。初回かどうか (罠 51) も今が何 round 目か (罠 16 / 28) も
PR の `@codex review` 投稿履歴から導く。stderr 1 行目の `round N/M` で確認できる。

## 結果の読み方

| exit / stdout | 意味 | controller の手 |
|---|---|---|
| `CONVERGED=true` **かつ `first_invocation=true`** | PR を開いた直後の round。指摘は差分ではなく **baseline** 側にいる (罠 51) | stdout 冒頭の `=== Baseline ... ===` を読んでから収束を宣言する |
| `CONVERGED=true` | 収束 (P2 / P3 の note が付くことがある) | P2 / P3 は内容を見て取り込み or skip を即決。merge へ |
| `WARN P0/P1 issues present` | blocking な指摘あり | `=== Inline P0/P1 ===` と `=== Top-level summary ===` を読んで fix → push → 次 round。上限は script が見張る (exit 7) |
| `INDETERMINATE (... produced nothing ...)` | 3 endpoint とも 0 件 = **答えが無かった** (罠 57)。`state_ok` は前 round の残り香 | quota (罠 56) / 未達 (罠 47) / 沈黙 (罠 9) を切り分けて user 報告 |
| `INDETERMINATE (no sentinel + no clean state)` | 判定材料不足 | `=== Inline, this round - ALL ===` を人が読む。必要なら再 trigger |
| exit 3 | reaction はあるが答えない (罠 9) | user に escalate ("suspect stale connector") |
| exit 4 | terminal error、**quota 切れを含む** (罠 10 / 56)。本文は stdout | retry しない。quota なら回復時刻を添えて user 報告 |
| exit 5 | trigger に reaction が 1 つも無い = 届いていない (罠 47、実測 8 回中 2 回) | 同じ commit に再 trigger (自動 1 回)。2 回目も届かなければ user 報告。届かなかった trigger も履歴上は 1 round と数える (安全側) |
| exit 6 | trigger の POST が 3 回失敗 (罠 50) | 待たずに abort。数分置いて再実行 |
| exit 7 | `max_rounds` に到達、**何も投稿していない** (罠 16 / 28) | user に報告 (続行 / 妥協 / scope 縮小の判断)。自分で上限を上げて再実行しない |
| exit 8 | 投稿前の読み取り (repo 名 / baseline / trigger 履歴) で `gh api` が失敗、**何も投稿していない** | `gh auth status` / rate limit を確認して再実行 |
| exit 9 | round の delta を計算できなかった (罠 59)。**trigger は投稿済み** | そのまま再実行。**空欄を「指摘なし」と読まない** — 判定材料が無いだけ |
| exit 10 | `Provided git ref <sha> does not exist` — push 直後の trigger で codex 側にまだ ref が無い (罠 60、PR #258 / #265 / #293 で 3 回)。本文は exit 4 と同じ語彙だが transient | `gh api repos/<o>/<r>/commits/<sha>` で head の存在を確かめ、body file 付きで **1 回だけ** 再 trigger (1 round と数える)。**2 回目も exit 10 なら止めて user 報告** (stdout / stderr を残す。#293 は 5 分空けた再 trigger も同じ本文だった = 待ち時間の根拠が無い。次に打つなら新しい push の後か、user が時刻を決める)。避けるには push と `gh pr create` の間を空ける。同じ round に quota 等の terminal 本文が並ぶと exit 4 が勝つ (script が comment ごとに分類) |

`=== Inline, this round - ALL of them ===` は badge の有無を問わず全部出す (罠 23: 列挙の外に指摘が来る)。
P-badge の計数が 0 でもここを読む。

## max_rounds の根拠

default 3 = cost-aware (25 credits × 3、Plus plan 月次 quota の 1-2%)。`/feature-flow` は
CLAUDE.local.md guardrail「5 round 経過で user 報告」と揃えて **明示的に 5 を渡す** (罠 28)。
上限は script が投稿履歴から判定して **投稿前に** 止める (exit 7) — 宣言だけでは効かない
(codex P2 on PR #222)。3 round で収束しない = spec / 設計の問題 = user 介入 (軌道修正)。

## 関連

- 罠の発見経緯: `.dev/knowledge/codex-review-loop-pitfalls.md` (script が構造で防いでいないものも含む)
- caller: `.claude/commands/feature-flow.md` Phase 6 / CLAUDE.local.md の常時 guardrail 節
- ローカル前掃除の実体: plugin `codex@openai-codex` (`~/.claude/plugins/cache/openai-codex/codex/<version>/`) の
  `commands/adversarial-review.md` (公式の実行形) / `agents/codex-rescue.md` (`task` 専用、review には使わない) /
  `prompts/adversarial-review.md` (focus の差し込み先)。model / runtime の罠は
  `.dev/knowledge/codex-plugin-review-model-pitfalls.md`、効いた focus の実例は `.dev/knowledge/feature-58-summary.md`
- 公式: [Codex GitHub integration](https://developers.openai.com/codex/integrations/github) /
  [Codex pricing](https://developers.openai.com/codex/pricing) /
  [GitHub REST: pull request reviews](https://docs.github.com/en/rest/pulls/reviews)
- 既知の制限: GraphQL 移行 (1 query で 3 endpoint) は未評価。bot login が変わったら script の `BOT` を更新
