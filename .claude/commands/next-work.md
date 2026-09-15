---
description: handoff 鎖の末端から次の作業を確認し、司令塔 (controller) + モデル別 subagent の体制で着手する
argument-hint: [focus]
---

# /next-work

session の入口。`.dev/README.md` を読み、handoff 鎖の末端から「★ 次にやること」を取り、kuriya の goal と突き合わせ、短く user に報告し、controller + subagent の体制を宣言してから着手する。前 session が何を残したかを推測で埋めないための手順であり、`/feature-flow` のような cycle の orchestrator ではない — 着手先が feature cycle なら、この command の末尾からそちらへ渡す。

## 想定起動タイミング

- `/clear` 直後、新しい session の最初に打つ command
- user が「次の作業を確認して」「続きを進めて」と言った場面すべて

引数 (任意): `/next-work <focus>` — 今日触りたい対象を短く (例: `/next-work gate GUI`)。focus は下の FOCUS 行で渡る。**空なら focus は無かったものとして扱う** (空文字を検索語にしない)。

FOCUS: $ARGUMENTS

## 前提

- `.dev/` が **それ自体の private repository** として初期化済 (`git -C .dev rev-parse --show-toplevel` が
  `/.dev` で終わる)。本 command が読む `.dev/README.md` / `.dev/knowledge/session-*-handoff.md` /
  `.dev/tools/handoff_tail.ps1` はすべてそちら側にあり、**公開 repo を clone しただけの checkout には
  無い**。= owner 用の workflow で、手順を公開側へ写して二重化することはしない
- `CLAUDE.local.md` の「feature-flow の常時 guardrail」節を遵守する (本 command の常時 guardrail。
  内容はここに写さない — 同じ規約が離れた場所にあると食い違う)
- kuriya MCP が接続済 (`mcp__kuriya__status` が呼べる)

## Phase 0 — 状態を集める

controller 自身が読み取りのみで行う。ここで書き込みや branch 操作はしない。

1. **`.dev/README.md` を読む** — 索引とワークフローの一次情報。handoff 鎖の規約もここにある
2. **鎖の末端を取る**:

```powershell
powershell -NoProfile -File .dev/tools/handoff_tail.ps1
```

   末端が 1 本ならその path が答え。**それ以外の本数なら script が exit 1 を返す** = 鎖が切れている
   (新 handoff が `前の handoff:` を書き忘れた)。その場合は**推測で 1 本選ばず、user に報告して止まる**
   (介入ポイント 1)
3. **末端を読む** — 「★ 次にやること」「持ち越し」「閉じる直前の状態」の各節。末端が
   「本体は前 handoff のまま」と書いていたら、その `前の handoff:` を遡って同じ節を読む
4. **kuriya と突き合わせる** — `mcp__kuriya__status` の goal (item #78) の body に末端の path が入っている。
   handoff 側と食い違ったら **handoff を一次情報とし** (kuriya の report は遅れることがある)、
   両方を user に見せる (介入ポイント 3)
5. **repo 状態を見る** — root で `git status --short --branch`。末端 handoff が別 repo (例: grooveseek-gate) を
   挙げていれば、その絶対パスに対しても `git -C <絶対パス> status --short --branch`。
   uncommitted changes / 想定と違う branch は**想定外 state として報告する** (介入ポイント 2)

## Phase 1 — user への報告

**5 行以内**で出す。長い引用はしない (末端 handoff の path を書けば user は自分で読める):

- **次にやること** — 末端 handoff の「★ 次にやること」から上位のものだけ
- **user の手が要る前提** があれば明示 (例: hosts ファイルの編集、証明書のインストール、GUI 操作)
- **repo 状態** (branch / clean か / 別 repo があればそれも)
- **focus** — 上の FOCUS 行が空でなければ、「★ 次にやること」との関係を短く:
  含まれる / 別件 (= 割り込み) / 矛盾する (= 介入ポイント 3)

## Phase 2 — 体制の宣言と着手

**controller = この session のモデル**。controller が自分で手を動かすのは次だけ:

- user 確認 (AskUserQuestion)
- kuriya / 台帳 / handoff / memory の bookkeeping
- レビュー指摘を fix に写す前の根拠 grep (指摘が事実か自分で見る)
- merge / push / tag の判断
- 想定外 state の検知

それ以外は subagent に渡す。subagent は Agent tool で起動し、**`model` を毎回明示する** —
省くと session のモデルを継承する:

| model | 渡すもの |
|---|---|
| `opus` | 実装、spec / plan 執筆、レビュー (spec 準拠 / 品質)、原因調査、brief の行番号検証 (Explore) |
| `sonnet` | 完成形を渡せる定型: docs 同期、rename、固定 diff の適用、テスト実行と報告、CHANGELOG 文言 |
| `haiku` | 読み取りだけ: grep / ファイル一覧 / 状態確認 / リンク切れ確認 |

**迷ったら 1 段上**。sonnet / haiku に振った task が 2 round で収束しなければ opus に上げる。

subagent prompt に**毎回貼る定型** (抜けた分だけ subagent が踏む):

- `.dev/` は untracked なので `git add .dev/...` は silently スキップされる。`.dev/` の更新は commit に乗らない
- git は `git -C <絶対パス> …` をそのまま貼る (`cd` は hook R6 で止まる)
- `run_in_background` の後は foreground で待つ (kuriya trap #219)
- 結果は status ファイルの最終行に書く

**着手**: 「★ 次にやること」の先頭が skill の起動 (`superpowers:writing-plans` など) を指しているなら、
そのまま invoke してこの command を終える。user 介入点 (spec / plan の承認、hosts 編集の依頼など) に
当たったら**そこで止まって user に渡す**。

## 本 command の介入ポイント

本 command が user を巻き込むのは次の 3 つ (`CLAUDE.local.md` guardrail 側の ①②③ とは別立て。Phase 0〜1 で止まる条件のこと):

1. **鎖が切れている** (Phase 0 step 2 が exit 1)
2. **想定外の git state** (uncommitted changes / 別 branch)
3. **handoff と kuriya の食い違い**、および focus が「★ 次にやること」と矛盾する時

それ以外は controller が即決する。何を即決し何を確認するかの一覧は
`CLAUDE.local.md` の「feature-flow の常時 guardrail」節にあり、ここには写さない。

## 関連

- `.dev/README.md` (= handoff 鎖の規約と `.dev/tools/handoff_tail.ps1`、`knowledge/` の運用)
- `CLAUDE.local.md` の「feature-flow の常時 guardrail」節 (= 本 command の常時 guardrail)
- `.claude/commands/feature-flow.md` (= 着手先が新 feature cycle だった場合の渡し先)
- memory `feedback_fable_controller_opus_subagents` (= Phase 2 のモデル振り分けの出所)
