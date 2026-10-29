---
name: house-rules
description: この session の repo の subagent が毎回守る定型 5 項 (.dev/ は commit に乗らない / git は -C / background の待ち方 / status ファイル / cargo の 5 点)
user-invocable: false
---

# subagent の定型 5 項

この session の repo で動く subagent が毎回守る定型。家はこの file だけ。project agent
(`implementer` / `reviewer` / `doc-writer` / `reader`) は frontmatter の `skills:` で preload し、
fallback の `general-purpose` は prompt の指示で最初に Skill tool から読む。抜けた分だけ subagent が踏む。

- `.dev/` は untracked なので `git add .dev/...` は silently スキップされる。`.dev/` の更新は commit に乗らない
- git は `git -C <絶対パス> …` をそのまま貼る。`cd D && cmd` の形は hook が subshell で包んで通す。`;` / `||` / `&` を含む形は deny のまま。project root での素の `git` は hook が `-C` を足して通す。それ以外の cwd では deny
- cargo 以外で `run_in_background` を使ったら、その後は foreground で待つ (kuriya trap #219)。**cargo には `run_in_background` を使わない** (次の項)
- 結果は status ファイルの最終行に書く
- cargo を打たせるなら 5 点 (先頭の 4 点は `windows-quirks` skill の罠 16 と同じ並び): `cargo test` の `-j` は repo の CLAUDE.local.md が決める値 (無ければ 2) / 重い cargo を 2 本同時に走らせない / `run_in_background` を使わず foreground + timeout / status ファイルは手順ごとに追記させる / 全体 test は repo の CLAUDE.local.md が指す script を 1 段ずつ foreground (timeout 590000) で打ち、段の終わりごとに status ファイルに追記する。targeted test にも script と同じ env を付ける。tool の呼び出しが kill / timeout されたら、打ち直す前に `Get-Process cargo,rustc,link -ErrorAction SilentlyContinue` と test の exe が残っていないかを見て、残っていれば終わるまで待つ (wrapper だけが死んで cargo が生き残ることがある。重ねて打つと 2 本目の重い cargo になる)。同じ段が kill・timeout・script の CAPPED (exit 2) を合わせて 2 回止まったら打ち直さず、status ファイルに書いて controller に返す。cargo を自前の PowerShell で起動して待つなら `Start-Process -Wait` を使わない (cargo が終わっても MSVC の vctip.exe を待って戻らない。`-PassThru` の `WaitForExit()` で cargo 本体だけを待つ。`windows-quirks` の罠 28)
