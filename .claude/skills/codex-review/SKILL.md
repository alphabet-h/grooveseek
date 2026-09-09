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
git -C <abs> diff main...HEAD -- '*.rs' | grep -E '^\+\s*//[/!]' | grep -oE '\[?`[^`]+`\]?' | sort | uniq -c
```

pathspec は `'*.rs'` — **directory を並べない**。`grooveseek/src grooveseek/tests crates` と
書くと `grooveseek/benches` が落ちる (codex P2 on #238。bench にも `//!` / `///` はあり、
`` `compute_match_spans` `` のような tree の名前が今も入っている) し、crate が増えた日に黙って狭くなる。

角括弧を残して抽出しているので、**bare backtick と `` [`..`] `` が同じ出力の中で区別できる** —
sweep は両方向で、リンクにし忘れた項とリンクにしてはいけない項の両方がここに並ぶ:

| 出力の項 | どうするか |
|---|---|
| tree の中の item (fn / struct / const / module / test fn) が bare backtick | `` [`path`] `` に直す |
| tree の外 (std / 依存 crate / SQL 語 / attribute / CLI 名 / file path / MCP tool 名) が `` [`..`] `` | backtick に戻す。**リンクにするのも P1** (#236 round 3 の `` `serde_json::Value` ``) |
| `::` / 演算子 / `{}` / `;` を含む項 — `` `use super::*;` `` や `` `limit * FILTER_OVERFETCH_FACTOR` `` | **中の名前を 1 つずつほどいて**上の 2 行を適用する。#237 round 1 の P1 はこの形 |

**identifier だけに絞らない。** 2 つ目の `grep` を `` '\[?`[A-Za-z_][A-Za-z0-9_:]*`\]?' `` にすると
項は減る (#237 の diff で 170 → 100。via: 上のコマンドの `grep -oE` をそれに差し替え、末尾を
`sort -u | wc -l` にして `fe7ac23...70dc178` で実行) が、落ちるのは表の 3 行目
= 実際に P1 を受けた形なので、絞ると意味が無くなる。

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
  何も検証していない**状態になる。非 test の doc から test 名を呼ぶときは backtick +
  持ち主を散文で書き、**stale 検出が要るなら test で書く** (`include_str!` して
  `fn <name>(` を探す形。PR #263 の
  `a_test_named_by_a_doc_comment_in_this_file_still_exists`)。codex はここをリンクにせよと
  P2 で言ってくるので、**対照 (名前を 1 文字変えて `cargo doc`) を添えて返す**
- **同じ module の doc から、その module の private item へは張れる。** `const LENGTH_PENALTY`
  のような非 `pub` の const でも、`quality.rs` の `//!` / `///` からなら rustdoc が解決する
  (2026-09-04 に確認: リンク化して `cargo doc --no-deps` が exit 0、対照として 1 語を存在しない
  名前に変えると `error: unresolved link` で exit 101 = lint は生きている)。**「private」の一語で
  下の行へ振らない** — 台帳 #43 はそれで P2 を受けた
- **リンクにできないものは backtick のまま残し、散文で持ち主 (module / file) を名指す**:
  **他** module に private な item / 非 test の doc から名指した `#[cfg(test)]` の item /
  `tests/` crate から見た lib の `pub(crate)` item / 別の `tests/` crate の test fn。
  **迷ったらリンクにして `cargo doc --no-deps` を 1 回回す** — 張れないなら
  `unresolved link` で落ちるので、推測する必要が無い

**見えるのは追加行だけ** (`^+` で絞っている)。既存行に残った古い名前はここには出ない —
そちらは `cargo doc` と review 側の仕事。分類の実例は
`.dev/knowledge/archive/prs/pr237-feature-55-pr2-sweep.md` の表。

## push する前にローカルの Codex で前掃除する

GitHub の round は 1 回 25 credits と 10 分強を使い、fix を push するたびに P2 が返る連鎖に
なりやすい (feature-58 / PR #291 は 15 round、内訳は `.dev/knowledge/feature-58-summary.md` の
「codex round」節)。**同じ目 (Codex CLI) に push 前の diff を見せて収束させ、GitHub は確認の
round に使う** — 2026-09-09 の user 判断。役割は **ローカル = 明白な違反の前掃除、GitHub = 最終確認**:
ローカルが approve でも GitHub は P2 を出す (同 summary の r7〜r9) し、focus を当てればローカルが
GitHub より先に本物を拾う (同 r4 / r5、round 10 後の 2 件)。どちらか一方で済ませない。

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
| `needs-attention` | **`[critical]` / `[high]` / `[medium]` は push を止める** — 取り込むか、反証できる指摘は **実測つきの反証**を次の focus に書いて再実行 (「一理ある」で従わない — 台帳 category 6 の 23 回目)。`[low]` は内容を見て即決 |
| exit ≠ 0 / `Verdict` 行が無い | `local-N.err` を読む。model 拒否 (400 / 404) / capacity / runtime の残留を切り分ける。判定材料が無いだけで「指摘なし」ではない |

**上限は push 1 回につき 3 round** (memory `feedback_local_codex_before_github_rounds`、2026-09-09 の合意)。
3 round で収束しない = fix が次の指摘を生んでいる (台帳 category 6) = user に相談 (介入ポイント 3)。
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
