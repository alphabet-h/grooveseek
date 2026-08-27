---
description: ブレスト → 仕様 → plan → 実装 → PR → codex review → merge → tag/release を一気通貫で進めるオーケストレータ。ユーザ介入は「最初の質問フェーズ」「spec 最終承認」「重大な軌道修正」の 3 点だけに絞る
---

# /feature-flow

新 feature の着想から release tag までを進めるオーケストレータ。**session の単位は PR 1 つ** —
Phase 6 で merge したら `/clear` で session を閉じ、次の PR はまっさらな context で始める (「handoff と session の区切り」節)。各フェーズ間の subagent self-review / codex review / handoff 生成を自動で回し、ユーザを「設計判断」だけに集中させる。

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
- `.dev/` が **それ自体の private repository** として初期化済 (`git -C .dev rev-parse --show-toplevel` が
  `/.dev` で終わる)。root repo は `.dev/` を `.git/info/exclude` で除外しているだけ (ADR-0000) なので、
  nested repo が無い checkout では `git -C .dev` が**親 repo を拾う** — その状態で Phase 6 step 6 の
  push を実行してはいけない。本 command が読む `.dev/release-checklist.md` / `.dev/feature-ideas.md` /
  `.dev/knowledge/*.md` も、書き出す `.dev/specs/` `.dev/plans/` `.dev/knowledge/` も、すべてこの
  private repo 側にある。**公開 repo を clone しただけの checkout には無い**ので、本 command は
  そのままでは動かない (= owner 用の workflow で、手順を公開側へ写して二重化することはしない)
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

spec を `.dev/specs/<feature-NN-name>.md` に起草する (groove の `CLAUDE.local.md` 規約)。

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

0. **push の前に doc comment の名前を洗う — この branch を push するたび、毎回。** PR を開く前も、review の指摘を直した後も同じで、P0/P1 の fix だけでなく**収束した round の P2/P3 を取り込んだ push も含む**。指摘された行だけ直して push すると同じ形が次の round で返り、収束後の取り込みはそのまま merge へ行く (#234 / #236 はそれで round を溶かした)。手順は `.claude/skills/codex-review/SKILL.md` の「push する前に doc comment の名前を洗う」節。sweep のコマンドと判定はそこにあり、ここには写さない (step 4 の注意と同じ理由)
1. `git push -u origin feature/<feature-NN-name>-pr-<n>` で push
2. `gh pr create` で PR 作成 (title + body は controller が自動 draft)
3. **`/codex-review <PR#> 5` skill を invoke** (= `.claude/skills/codex-review/SKILL.md`、`5` で max_rounds を CLAUDE.local.md guardrail と揃える — 揃えないと本 command と skill で default がずれる、PR #54 codex round 2 の P2)。1 round = `.claude/skills/codex-review/scripts/codex_review_round.sh` 1 回で、trigger / 3 endpoint polling / 収束判定 / 整形 / round 上限はすべて script の中
4. controller (= main agent) は **script の verdict だけを読む** — stdout の `CONVERGED=` 行とその直前の判定行、および exit code。**判定の predicate (sentinel 文言 / P-badge の数え方 / 何を再 round にするか) をここに書き写さない**: 2 か所にあると script と食い違い、sentinel と P1 が同時に来た round で blocking な指摘を飛ばすか、P2 だけの round で無駄な 1 round を回す (codex P1 on PR #222、AGENTS.md "One question gets one implementation")。読み方の家は SKILL.md「結果の読み方」の表。そこから本 phase の分岐だけ言い直すと:
   - `CONVERGED=true` → step 5 へ。P2 / P3 の note が付いていたら内容を見て取り込み or skip を即決する (再 round はしない)
   - `WARN P0/P1 issues present` → 取り込み、regression test を 1 件追加、再 push → goto step 3 (re-trigger body 付きで `/codex-review` 再 invoke)
   - `INDETERMINATE` / exit 3〜9 → SKILL.md の表のとおり。**exit 7 (= 5 round 到達、何も投稿していない) → ユーザに相談** (← 介入ポイント 3)
5. `CONVERGED=true` になったら `gh pr merge <N> --squash --delete-branch`
6. **merge したら session を閉じる** — release worthy なら Phase 7、続けて Phase 8 を済ませ、
   「handoff と session の区切り」の手順で handoff → `.dev` push → **`/clear`**。
   次の PR (PR-<n+1>) はその後の Phase 5 から再開する。Phase 7 は release worthy な PR でしか
   走らないので、session の区切りを Phase 7 に置くと非 release の merge で手順が抜ける

review 取り込み時の判断はすべて controller (= main agent) が行い、user 介入はしない。**ただし**:
- 取り込みが「Phase 3 で承認した spec の前提を覆す」内容なら user に確認
- 5 round 経過した時点で必ず user に状況を投げ、続行 / 妥協 / scope 縮小を判断してもらう

参照: `.claude/skills/codex-review/SKILL.md` (= polling 実装の固定化、`/codex-review` skill 本体)、`.dev/knowledge/codex-review-loop-pitfalls.md` (運用上の罠の蓄積)。**罠の番号や本数をここに写さない** — あのノートは追記順に並んでいて番号が飛び、重複もあるので、番号で引くと外れる。見出しから引く: `grep -n '^#\+ 罠' .dev/knowledge/codex-review-loop-pitfalls.md`

### Phase 7 — CHANGELOG / version bump / tag (該当 PR が release worthy な場合のみ)

phase が **release を構成する最終 PR** だった場合のみ:

1. `CHANGELOG.md` の release 化 — `[Unreleased]` を `[X.Y.Z] - YYYY-MM-DD` に rename、空 `[Unreleased]` を再 seed、最下部に compare link を追加、並行 PR が重複させた `### Added` / `### Changed` 等を節ごとにまとめ直す。順序と畳み方は `.dev/release-checklist.md` の「ドキュメント同期」節にあり、ここには写さない (step 3 と同じ理由)
2. `Cargo.toml` の `version` を bump (`cargo check` で `Cargo.lock` 自動追従)
3. `.dev/release-checklist.md` の「ドキュメント同期」節を実行 (README / ARCHITECTURE / 各 docs)
4. `/full-audit` 起動判断 (CLAUDE.local.md の trigger 該当時のみ)
5. tag 作成: `git tag -a vX.Y.Z -m "..."` → `git push origin vX.Y.Z`
6. `release.yml` (cargo-dist) が auto で binary build + GH Release を作成 (手動の `gh release create` は禁止)

途中で release-blocker な audit findings が出たら、Phase 5-6 にループバックして fix。

### Phase 8 — Knowledge note + audit todo 更新

cycle 完了時に必ず:

- `.dev/knowledge/<feature-NN>-summary.md` 作成 (結果サマリ / 設計判断 / ハマりどころ / 工程まとめ / 後続候補)
- `.dev/feature-ideas.md` の対応 ID を `done` マーク + done line に PR 番号と merge 日付を追記
- `/full-audit` を回した場合は `.dev/archive/<date>-cycle/audit-todos.md` に deferred items を整理

上の 3 つはどれも `.dev/` 配下 = git untracked なので commit には乗らない (= subagent prompt で必ず明示する)。

`CHANGELOG.md` はこの Phase では触らない。entry は変更した PR 自身が `[Unreleased]` に足し、`[X.Y.Z]` への畳み込みは Phase 7 step 1 が持つ。**release 見出しにも entry にも PR 番号は付けない** — 付いている行は無い (via: `grep -c '^## .*(#' CHANGELOG.md` / `grep -c '^- .*(#[0-9]' CHANGELOG.md`)。PR 番号は散文の中で経緯として書く時だけ現れる。`.claude/` だけを触る PR は entry 自体を書かない (#220 / #222 / #238 の型)。

## handoff と session の区切り

**Phase 6 step 5 で PR が merge されたら (= step 6)、Phase 7 / 8 の後処理を済ませて session を閉じる**
(例外として、Phase 5 / Phase 6 の途中で context が逼迫したときも同じ手順)。`/compact` で続けない — 1 session 1 回まで
(根拠: `.dev/knowledge/mistakes-repeat-session-length-ungated-rules-duplicated-facts.md`)。

**閉じ方は `/clear` で足りる** — 止めたいのは「溜まった context に compaction を重ねること」であって
プロセスの寿命ではない。`/clear` は context を捨て、transcript も切り替わる (実測: 同日の 3 本が
226 MB / 1 MB / 3 MB)。立ち上げ直す価値があるのは **`settings.json` / hook を触った直後だけ**
(skill は同 session 内で反映される)。`/clear` は background task を止めないので、下の leak 確認が効く:

1. **handoff doc を即時 write**: `.dev/knowledge/session-<YYYY-MM-DD>-<topic>-handoff.md`
   - 現状の git state (`git log --oneline -5`)
   - 完了済 phase / 進行中 phase / 未着手 phase
   - 重要な constraint / pattern (`CLAUDE.local.md` 規約、subagent prompt の `.dev/` untracked 注意、codex review loop 規約)
   - 次セッションでの開始手順 (5-7 step に細分化)
   - オープン論点 / 注意
   - 完了基準 checklist
   - background task leak の確認 (`run_in_background` の polling が残っていないか)
2. `.dev` が **それ自体の repository** であることを確かめてから push する (前提の節)。nested repo が
   無ければ `git -C .dev` は親 repo に向き、`add -A` が親の変更を staging して `push` は親の origin へ行く:
   ```bash
   case "$(git -C .dev rev-parse --show-toplevel)" in
     */.dev) git -C .dev add -A && git -C .dev commit -F <msgfile> && git -C .dev push ;;
     *) echo "ABORT: .dev is not its own repository; see the preconditions" >&2; exit 1 ;;
   esac
   ```
3. ユーザに通知: `handoff を <path> に書き、.dev を push しました。/clear して「<path> を読んで続きを進めて」と一言伝えれば再開できます。`

context が切り替わったら、SessionStart 通知を起点に handoff doc を読んで再開する。

handoff doc の型は `.dev/knowledge/session-2026-08-22-handoff-after-stderr-ascii.md` (★次にやること /
main の状態 (実測) / 残っているもの / 測って分かったこと / 環境の罠 / 台帳 / 起票済み)。

## 介入ポイント以外でユーザを巻き込まない原則

以下の判断は **controller (main agent) が即決し、ユーザに振らない**:

- subagent review round の中間 fix (low/medium レベルの指摘の取り込み判断)
- codex review の P1 / P2 fix の取り込み (P1 は無条件取り込み、P2 は妥当性判定して取り込み)
- regression test の追加位置 / テストケースの選定
- `cargo fmt` / `clippy` の lint fix
- CHANGELOG / docs の文言調整
- merge commit message の draft
- tag message の draft
- release 後の `.dev/feature-ideas.md` の done 行更新

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
| `.dev/knowledge/session-<date>-<topic>-handoff.md` | merge ごと | session を閉じる前の申し送り (git untracked) |
| `CHANGELOG.md` | 更新 | release 時に `[Unreleased]` → `[X.Y.Z]` |
| `Cargo.toml` + `Cargo.lock` | 更新 | release 時 version bump |
| README.md / docs/* | 更新 | `.dev/release-checklist.md` の「ドキュメント同期」節に従う |
| `.dev/feature-ideas.md` | 更新 | done 行を該当 ID に追記 |

## 関連

- `.dev/release-checklist.md` (= Phase 7 step 1 / step 3 の元。`CLAUDE.local.md` の「リリース運用」から辿れる)
- `CLAUDE.local.md` の「開発フロー」節 (= 本 command の常時 guardrail)
- `.claude/commands/full-audit.md` (Phase 7 で起動判断)
- `.claude/skills/codex-review/SKILL.md` + `.claude/skills/codex-review/scripts/codex_review_round.sh` (= Phase 6 の codex review loop 実装、`/codex-review <PR#>` で invoke)
- `.dev/knowledge/codex-review-loop-pitfalls.md` (Phase 6 の運用 reference。罠の番号はここに写さない — 引き方は Phase 6 の参照行)
- `.dev/knowledge/index-progress-buffering-pitfall.md` (background bash の罠 reference)
- `superpowers:brainstorming` / `superpowers:writing-plans` / `superpowers:subagent-driven-development` (orchestrate される 3 skill)

## 過去 cycle の参照

直近の完走例 (本 command 化の元になった手動フロー):
- feature-28 (MMR + Parent retriever): 4 PR の brainstorming → spec → plan → 実装 → codex 5 round → merge → v0.7.0 tag を 2 セッションで完走
  - spec: `.dev/specs/feature-28-mmr-parent.md`
  - plan: `.dev/plans/feature-28-mmr-parent.md`
  - 統合 summary: `.dev/knowledge/feature-28-summary.md`
  - PR: #35 / #36 / #37 / #38

このコマンドが対象とする「介入ポイントの 3 点絞り込み」が成立したかは、過去 cycle で「controller が即決した数 / ユーザに飛んだ判断の数」で判定する。session の数は
PR の数で決まる (Phase 6 step 6) ので、session を跨いだこと自体は失敗ではない。
