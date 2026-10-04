---
name: house-rules
description: kb-mcp の subagent が毎回守る定型 5 項 (.dev/ は commit に乗らない / git は -C / background の待ち方 / status ファイル / cargo の 4 点) と、名前を指定された file の書き方
user-invocable: false
---

# subagent の定型 5 項

kb-mcp (grooveseek) repo で動く subagent が毎回守る定型。家はこの file だけ。project agent
(`implementer` / `reviewer` / `doc-writer` / `reader`) は frontmatter の `skills:` で preload し、
fallback の `general-purpose` は prompt の指示で最初に Skill tool から読む。抜けた分だけ subagent が踏む。

- `.dev/` は untracked なので `git add .dev/...` は silently スキップされる。`.dev/` の更新は commit に乗らない
- git は `git -C <絶対パス> …` をそのまま貼る。`cd D && cmd` の形は hook が subshell で包んで通す。`;` / `||` / `&` を含む形は deny のまま。project root での素の `git` は hook が `-C` を足して通す。それ以外の cwd では deny
- cargo 以外で `run_in_background` を使ったら、その後は foreground で待つ (kuriya trap #219)。**cargo には `run_in_background` を使わない** (次の項)
- 結果は status ファイルの最終行に書く
- cargo を打たせるなら 4 点 (`windows-quirks` skill の罠 16 と同じ並び): `cargo test` は `-j 2` / 重い cargo を 2 本同時に走らせない / `run_in_background` を使わず foreground + timeout / status ファイルは手順ごとに追記させる

## 名前を指定された file を書く時 (file を書く subagent だけ)

上の 5 項とは別の決まり。名前を自分で決める時は `report` / `summary` / `findings` / `analysis` を使わない (subagent の Write がその名前で拒否されることがある。anthropics/claude-code#44657)。

prompt が名前を指定した時 (`feature-NN-summary.md` など) はその名前で書き、別の名前に逃がさない (既存 file の編集も同じ)。その Write が拒否されたら本文は返さず (長文は返答の途中で切れる。kuriya trap #313)、最終 message に `NOT WRITTEN: <指定された path>` の 1 行を書いて止まる。
controller 側の扱いは `.claude/commands/next-work.md` Phase 2 の「名前を指定して file を書かせた時」。
