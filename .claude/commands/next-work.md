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

1. **前提を確かめる** — `.dev/README.md` と `.dev/tools/handoff_tail.ps1` が存在し、`git -C .dev rev-parse --show-toplevel` が
   `/.dev` で終わること。どれか欠けていたら **owner 用 command で、この checkout では動かない**と 1 行で報告して
   止まる (読みに行かない。止まる条件 1)
2. **`.dev/README.md` を読む** — 索引とワークフローの一次情報。handoff 鎖の規約もここにある
3. **鎖の末端を取る**:

```powershell
powershell -NoProfile -File .dev/tools/handoff_tail.ps1
```

   exit 0 だけが成功で、stdout の 1 行が末端の path。**exit 1 は鎖が切れている** (末端が 0 本か 2 本以上。
   新 handoff が `前の handoff:` を書き忘れた) — **推測で 1 本選ばず、user に報告して止まる** (止まる条件 2)。
   **それ以外の exit code は script 自体の失敗** — stdout を捨て、exit code と stderr を添えて報告して止まる (同じく条件 2)
4. **末端を読む** — 「★ 次にやること」「持ち越し」「閉じる直前の状態」の各節。末端が
   「本体は前 handoff のまま」と書いていたら、その `前の handoff:` を遡って同じ節を読む。
   遡り先が `.dev/knowledge/` に無い、読めない、または同じ file に戻る (循環) なら鎖が壊れている = 止まる条件 2
5. **kuriya と突き合わせる** — `mcp__kuriya__status` が返す**現在の goal** の body に末端の path が入っている
   (README の運用。item 番号は固定しない — goal が入れ替わっても同じ手順)。
   handoff 側と食い違ったら **handoff を一次情報とし** (kuriya の report は遅れることがある)、
   両方を user に見せる (止まる条件 4)。`mcp__kuriya__status` が呼べない / エラーを返す時は止まらず、
   Phase 1 の報告に「kuriya 未接続、突き合わせ未実施」と 1 行書いて handoff だけで続ける
6. **repo 状態を見る** — root で `git status --short --branch`、続けて nested repo の
   `git -C .dev status --short --branch` (root の status は `.dev/` を見ない。前 session が handoff を
   commit し忘れていればここでしか分からない)。どちらも exit 0 以外なら stderr を添えて止まる (止まる条件 3)。
   step 4 で読んだ handoff (遡った分も含む)、または FOCUS 行が別 repo (例: grooveseek-gate) を
   挙げていれば、その絶対パスに対しても `git -C <絶対パス> status --short --branch`。
   uncommitted changes / 想定と違う branch は**想定外 state として報告する** (止まる条件 3)。
   その path が無い、または git が exit 0 以外を返す時も同じ条件 3 — その repo の状態を欠いたまま先へ進まない

## Phase 1 — user への報告

**5 行以内**で出す。長い引用はしない (末端 handoff の path を書けば user は自分で読める):

- **次にやること** — step 4 で解決した handoff (「本体は前 handoff のまま」で遡ったなら遡った先) の「★ 次にやること」から上位のものだけ。Phase 2 の着手先もこれ
- **user の手が要る前提** があれば明示 (例: hosts ファイルの編集、証明書のインストール、GUI 操作)
- **repo 状態** (branch / clean か / 別 repo があればそれも)
- **focus** — 上の FOCUS 行が空でなければ、「★ 次にやること」との関係を短く:
  含まれる (= そのまま着手) / 別件 (= user の今日の指示が優先。focus に着手し、「★ 次にやること」は
  持ち越しとして報告する) / 矛盾する (= 止まる条件 5、着手せず user に聞く)

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

**着手**: 着手先は Phase 1 の focus 判定に従う (focus 無し / 含まれる → 「★ 次にやること」の先頭、
別件 → focus)。着手先が skill の起動 (`superpowers:writing-plans` など) を指しているなら、
そのまま invoke してこの command を終える。user 介入点 (spec / plan の承認、hosts 編集の依頼など) に
当たったら**そこで止まって user に渡す**。

## 止まる条件

`CLAUDE.local.md` の介入ポイントは ①質問 phase ②spec 承認 ③軌道修正の 3 点のままで、本 command は
それを増やさない。下の 5 つはどれも guardrail の「user 確認」に既に挙がっている**想定外の git state /
前提が崩れた時**の具体形 (= ③ の一種) で、Phase 0〜1 で controller が判断を捏造せずに止まる条件:

1. **前提が欠けている** (`.dev/` が無い、または private repo ではない = この checkout では動かない)
2. **鎖が切れている、または末端 script が失敗した** (Phase 0 step 3 が exit 0 以外、または step 4 の遡り先が無い / 循環する)
3. **想定外の git state** (root または `.dev` の uncommitted changes / 別 branch / handoff が挙げた repo の path が無い、git が失敗する)
4. **handoff と kuriya の食い違い** (kuriya 未接続は含まない — その時は報告して続ける)
5. **focus が「★ 次にやること」と矛盾する** (別件は矛盾ではない — focus に着手する)

それ以外は controller が即決する。何を即決し何を確認するかの一覧は
`CLAUDE.local.md` の「feature-flow の常時 guardrail」節にあり、ここには写さない。

## 関連

- `.dev/README.md` (= handoff 鎖の規約と `.dev/tools/handoff_tail.ps1`、`knowledge/` の運用)
- `CLAUDE.local.md` の「feature-flow の常時 guardrail」節 (= 本 command の常時 guardrail)
- `.claude/commands/feature-flow.md` (= 着手先が新 feature cycle だった場合の渡し先)
- memory `feedback_fable_controller_opus_subagents` (= Phase 2 のモデル振り分けの出所)
