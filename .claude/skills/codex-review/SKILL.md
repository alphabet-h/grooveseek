---
name: codex-review
description: PR で `@codex review` を trigger し、3 endpoint (inline / reviews / issue) を `(id, updated_at)` set diff で同時 polling、state-base convergence + P0/P1 absence + sentinel text の 3 layer で判定、wall-clock timeout / error string detect / cost-aware retry cap で hardening したオーケストレータ
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
- 公式: [Codex GitHub integration](https://developers.openai.com/codex/integrations/github) /
  [Codex pricing](https://developers.openai.com/codex/pricing) /
  [GitHub REST: pull request reviews](https://docs.github.com/en/rest/pulls/reviews)
- 既知の制限: GraphQL 移行 (1 query で 3 endpoint) は未評価。bot login が変わったら script の `BOT` を更新
