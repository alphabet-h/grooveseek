---
description: ブレスト → 仕様 → plan → 実装 → PR → codex review → merge → tag/release を一気通貫で進めるオーケストレータ。ユーザ介入は「最初の質問フェーズ」「spec 最終承認」「重大な軌道修正」の 3 点だけに絞る
---

# /feature-flow

新 feature の着想から release tag までを **同 session 内で完結** させるオーケストレータ。各フェーズ間の subagent self-review / codex review / handoff 生成を自動で回し、ユーザを「設計判断」だけに集中させる。

## 想定起動タイミング

- `.dev/feature-ideas.md` の優先度ピックに着手する瞬間
- ユーザが「次は X をやりたい」とブリーフを出した瞬間
- 既存 feature の続きではなく、新規 cycle の頭

引数 (任意): `/feature-flow <ブリーフ>` — 1-2 文の概要 (例: `/feature-flow B-1 search UX 改善`)。引数なし起動時は最初の質問でユーザにブリーフを聞く。

## 前提

- リポジトリは git clean (uncommitted changes なし)
- `superpowers:brainstorming` / `superpowers:writing-plans` / `superpowers:subagent-driven-development` skill が利用可能
- subagent type: `feature-dev:code-reviewer` / `feature-dev:code-architect` / `general-purpose` / `superpowers:code-reviewer` が available
- GitHub CLI (`gh`) が認証済 (`gh auth status` で確認可)
- `@codex review` 経由で chatgpt-codex-connector が動く (PR repo 側で設定済)
- `CLAUDE.local.md` の「開発フロー」節 (本 command の常時 guardrail) を遵守する

## ユーザ介入ポイントの最小化方針

このコマンドの設計上の核は **「ユーザ介入を 3 点に絞る」**:

1. **質問フェーズ** — `superpowers:brainstorming` の Q&A (最大 7 問程度)。ユーザの設計判断を聞く
2. **spec 最終承認** — subagent self-review が収束した spec をユーザに提示し承認 (= 実装着手の go/no-go)
3. **軌道修正** — 実装中・review loop 中に **以前の判断が覆る内容** が出た時のみユーザに確認

それ以外 (review round の中間結果 / fix の妥当性 / merge / tag) は **ユーザ介入なしで自動で回す**。Subagent review の中間結果は user 通知せず内部で消化する。Codex review loop は polling で convergence まで自動回す。

## 実行フロー

### Phase 0 — Brief intake

ブリーフ (引数 or 1 文の対話入力) を受けて:

- 既存 `.dev/feature-ideas.md` の対応 ID を特定する (例: `A-3 + A-4` / `B-1`)
- 関連既存 PR / 過去 audit を `git log --oneline` と `.dev/knowledge/` で確認
- 関連 feature の依存・前提 PR を整理 (例: A-3 は D-1 eval 基盤前提)

ユーザに 1 行で伝える: `feature 「<X>」 のサイクルを開始します。Phase 1 で brainstorming に入ります。`

### Phase 1 — Brainstorming (`superpowers:brainstorming`)

`superpowers:brainstorming` skill を invoke。Q&A は **ユーザとのやり取り**:

- 1 問ずつ提示、可能な限り A/B/C 多肢選択
- 設計判断の根拠を毎回明示
- 通常 5-7 問で aspects (scope / API surface / トレードオフ) を固める

最後にユーザが design に同意した時点で **次フェーズへ自動移行**。`superpowers:brainstorming` skill が要求する spec ドキュメント生成だけは Phase 2 に委譲する (本 command が driver)。

### Phase 2 — Spec drafting + subagent self-review loop (内部、ユーザ非介在)

spec を `.dev/specs/<feature-NN-name>.md` に起草する (kb-mcp の `CLAUDE.local.md` 規約)。

その後 **subagent review loop** を回す:

1. **dispatch**: `superpowers:code-reviewer` (or `feature-dev:code-reviewer`) に spec を渡し、低/中/高/重大の 4 段階で指摘を返させる
2. **fix**: 指摘を spec に取り込む (controller agent 自身が edit)。前段の判断が覆る指摘の場合のみユーザに確認 (← 介入ポイント 3)
3. **re-dispatch**: 同じ subagent に「low-only に到達したか」を再評価させる
4. **convergence**: low-only or "no major issues" が 2 round 連続で得られたら脱出。最大 5 round。5 round で収束しないなら spec 起草の前提が崩れている = ユーザに再相談

review round の中間結果は **ユーザに見せない**。最終 spec だけを Phase 3 でユーザに提示。

### Phase 3 — Spec 最終承認 (ユーザ介入ポイント 2)

ユーザに以下のフォーマットで提示:

```
spec を `.dev/specs/<feature>.md` に起こし、subagent review (N round) で low-only まで収束しました。
- 主要 architecture decision: <要点 3-5 個>
- スコープ外: <意図的に削った項目>
- 前提・依存: <他 feature / infra 依存>

これで実装に入って良ければ "OK" を、追加の判断軸があれば修正点を教えてください。
```

ユーザが OK で次へ。修正要求があれば Phase 1-2 にループバック。

### Phase 4 — Plan drafting (`superpowers:writing-plans`)

`superpowers:writing-plans` skill を invoke して `.dev/plans/<feature-NN-name>.md` を起草。

plan も Phase 2 と同様に subagent self-review loop で収束させる (内部、ユーザ非介在)。承認はスキップ — spec が承認済なら plan は spec の機械的展開なのでユーザの再判断は不要。ただし Phase 2 で覆る判断が plan で発見された場合のみユーザに確認 (← 介入ポイント 3)。

### Phase 5 — Implementation (`superpowers:subagent-driven-development`)

`superpowers:subagent-driven-development` skill に plan を渡して実装を回す。**この skill 内部で**:

- task ごとに implementer subagent + spec compliance reviewer + code quality reviewer の 3 段
- review round の中間 fix もユーザ非介在
- task 単位で `feat(<scope>): ...` 形式のコミット 1 個
- PR は phase 区切り (PR-1 / PR-2 / ...) で作成

**ユーザに通知するタイミング**: phase の PR を立てる直前 (= GitHub に push する直前)。push 自体は自動。

### Phase 6 — PR creation + codex review loop

各 phase の最後で:

1. `git push -u origin feature/<feature-NN-name>-pr-<n>` で push
2. `gh pr create` で PR 作成 (title + body は controller が自動 draft)
3. **`/codex-review <PR#> 5` skill を invoke** (= `.claude/commands/codex-review.md`、`5` で max_rounds を CLAUDE.local.md guardrail と揃える / 罠 28 codex P2 on PR #54)。本 skill が以下を 1 step で固定化:
   - `@codex review` mention で codex trigger
   - `pulls/<N>/comments` (inline) + `issues/<N>/comments` (top-level) + `pulls/<N>/reviews` (review body) の 3 endpoint を **count-base で同時 polling** (= `submitted_at` / `created_at` の時刻比較を avoid、bash quirks 回避)
   - 5 round 上限で auto break (CLAUDE.local.md guardrail と整合)
   - 結果 fetch + 整形して controller に提示 (= top-level summary + inline P1/P2 detail + review body)
4. controller (= main agent) が `/codex-review` の出力を判定:
   - top-level に `Didn't find any major issues` (variation: `Hooray!` / `Keep them coming!` / `Bravo` 等) → **収束**、step 5 へ
   - inline に P1/P2 → 妥当な範囲で取り込み (P1 = 必須 fix、P2 = scaling/UX 退化、P3 以下は判断)、regression test を 1 件追加、再 push → goto step 3 (= `/codex-review` 再 invoke で re-review)
   - 5 round 経過しても収束しない → ユーザに相談 (← 介入ポイント 3)
5. 収束したら `gh pr merge <N> --squash --delete-branch`

review 取り込み時の判断はすべて controller (= main agent) が行い、user 介入はしない。**ただし**:
- 取り込みが「Phase 3 で承認した spec の前提を覆す」内容なら user に確認
- 5 round 経過した時点で必ず user に状況を投げ、続行 / 妥協 / scope 縮小を判断してもらう

参照: `.claude/commands/codex-review.md` (= polling 実装の固定化、`/codex-review` skill 本体)、`.dev/knowledge/codex-review-loop-pitfalls.md` (運用上の罠カテゴリ蓄積、罠 7 = last-writer-wins まで記録済)

### Phase 7 — CHANGELOG / version bump / tag (該当 PR が release worthy な場合のみ)

phase が **release を構成する最終 PR** だった場合のみ:

1. `CHANGELOG.md` の `[Unreleased]` を `[X.Y.Z] - YYYY-MM-DD` に rename + 空 `[Unreleased]` を再 seed
2. `Cargo.toml` の `version` を bump (`cargo check` で `Cargo.lock` 自動追従)
3. `CLAUDE.md` のリリース前ドキュメント同期チェックリストを実行 (README / ARCHITECTURE / 各 docs)
4. `/full-audit` 起動判断 (CLAUDE.local.md の trigger 該当時のみ)
5. tag 作成: `git tag -a vX.Y.Z -m "..."` → `git push origin vX.Y.Z`
6. `release.yml` (cargo-dist) が auto で binary build + GH Release を作成 (手動の `gh release create` は禁止)

途中で release-blocker な audit findings が出たら、Phase 5-6 にループバックして fix。

### Phase 8 — Knowledge note + audit todo 更新

cycle 完了時に必ず:

- `.dev/knowledge/<feature-NN>-summary.md` 作成 (結果サマリ / 設計判断 / ハマりどころ / 工程まとめ / 後続候補)
- `.dev/feature-ideas.md` の対応 ID を `done` マーク + done line に PR 番号と merge 日付を追記
- `CHANGELOG.md` の release 行に PR # 添付
- `/full-audit` を回した場合は `.dev/archive/<date>-cycle/audit-todos.md` に deferred items を整理

これらは git untracked (`.dev/`) なので commit には乗らない (= subagent prompt で必ず明示する)。

## Context overflow への備え (handoff 自動生成)

Phase 5 / Phase 6 の途中で context が 80% を超えそうな兆候を検知したら:

1. **handoff doc を即時 write**: `.dev/knowledge/<feature-NN>-handoff.md`
   - 現状の git state (`git log --oneline -5`)
   - 完了済 phase / 進行中 phase / 未着手 phase
   - 重要な constraint / pattern (`CLAUDE.local.md` 規約、subagent prompt の `.dev/` untracked 注意、codex review loop 規約)
   - 次セッションでの開始手順 (5-7 step に細分化)
   - オープン論点 / 注意
   - 完了基準 chekclist
2. ユーザに通知: `context が逼迫してきたので、handoff を <path> に書きました。/compact を打って、新 session で「<path> を読んで続きを進めて」と一言伝えれば再開できます。`

ユーザが `/compact` を打って新 session が始まったら、SessionStart 通知を起点に handoff doc を読んで再開する (= layer 3 の hook 化を入れない場合の手動運用)。

handoff doc の生成テンプレは `.dev/knowledge/feature-28-pr-4-handoff.md` を参考にする。

## 介入ポイント以外でユーザを巻き込まない原則

以下の判断は **controller (main agent) が即決し、ユーザに振らない**:

- subagent review round の中間 fix (low/medium レベルの指摘の取り込み判断)
- codex review の P1 / P2 fix の取り込み (P1 は無条件取り込み、P2 は妥当性判定して取り込み)
- regression test の追加位置 / テストケースの選定
- `cargo fmt` / `clippy` の lint fix
- CHANGELOG / docs の文言調整
- merge commit message の draft
- tag message の draft
- release 後の `.dev/feature-ideas.md` / CHANGELOG の done マーク更新

ただし以下は必ず確認 (介入ポイント 3):
- spec で承認した API surface / scope / 設計原則を覆す指摘
- 5 round 経過しても収束しない review loop
- audit で release-blocker と判断される指摘
- 想定外のリポジトリ状態 (uncommitted changes / 別 branch にいる等) を検出した時

## 出力先まとめ

| 場所 | 種別 | 用途 |
|---|---|---|
| `.dev/specs/<feature>.md` | 新規 (毎回) | spec ドキュメント (git untracked) |
| `.dev/plans/<feature>.md` | 新規 (毎回) | 実装 plan (git untracked) |
| `.dev/knowledge/<feature>-summary.md` | 新規 (毎回) | 振り返り + 工程ノート (git untracked) |
| `.dev/knowledge/<feature>-handoff.md` | 必要時 | context overflow 時の申し送り (git untracked) |
| `CHANGELOG.md` | 更新 | release 時に `[Unreleased]` → `[X.Y.Z]` |
| `Cargo.toml` + `Cargo.lock` | 更新 | release 時 version bump |
| README.md / docs/* | 更新 | リリース前ドキュメント同期チェックリストに従う |
| `.dev/feature-ideas.md` | 更新 | done 行を該当 ID に追記 |

## 関連

- `CLAUDE.md` の「リリース前チェックリスト」 (= Phase 7 の docs sync 元)
- `CLAUDE.local.md` の「開発フロー」節 (= 本 command の常時 guardrail)
- `.claude/commands/full-audit.md` (Phase 7 で起動判断)
- `.claude/commands/codex-review.md` (= Phase 6 の codex review loop 実装、`/codex-review <PR#>` で invoke)
- `.dev/knowledge/codex-review-loop-pitfalls.md` (Phase 6 の運用 reference、罠 1-7 蓄積)
- `.dev/knowledge/index-progress-buffering-pitfall.md` (background bash の罠 reference)
- `superpowers:brainstorming` / `superpowers:writing-plans` / `superpowers:subagent-driven-development` (orchestrate される 3 skill)

## 過去 cycle の参照

直近の完走例 (本 command 化の元になった手動フロー):
- feature-28 (MMR + Parent retriever): 4 PR の brainstorming → spec → plan → 実装 → codex 5 round → merge → v0.7.0 tag を 2 セッションで完走
  - spec: `.dev/specs/feature-28-mmr-parent.md`
  - plan: `.dev/plans/feature-28-mmr-parent.md`
  - 統合 summary: `.dev/knowledge/feature-28-summary.md`
  - PR: #35 / #36 / #37 / #38

このコマンドが対象とする「介入ポイントの 3 点絞り込み」が成立したかは、過去 cycle で「session を跨がず controller が即決した数 / ユーザに飛んだ判断の数」で判定する。
