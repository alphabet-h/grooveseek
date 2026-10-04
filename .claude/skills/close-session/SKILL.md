---
name: close-session
description: PR merge 後 / 日の終わり / context が逼迫した時に controller が起動する。handoff を書いて .dev を push し、kuriya goal を向け直し、/next-work の入口が通ることを確かめる。/clear は user
argument-hint: <topic>
---

# /close-session

session を閉じる手順の唯一の家。`/feature-flow` の「handoff と session の区切り」節と `CLAUDE.local.md` の「PR が merge されたら」は、ここを指すだけで手順を持たない。

**owner 用 command で、`.dev/` が private repository として無い checkout (公開 repo を clone しただけ) では動かない** (`/next-work` の `## 前提` 節と同じ)。その checkout では step 0 の最初の command (状態 helper) が `state: skipped (no .dev checkout — /close-session is an owner-only command)` と印字する。その時は手順に進まず、owner 用 command で、この checkout では動かないと 1 行で報告して止まる。

**controller が起動してよい** (`disable-model-invocation` は付けていない)。merge 後に無人で進む session も handoff を書けるようにするため。`/clear` だけは user が打つ (controller は打てない。step 6 の通知で促す)。

TOPIC: $ARGUMENTS

handoff の file 名は `.dev/knowledge/session-<YYYY-MM-DD>-<TOPIC>-handoff.md`。TOPIC が空なら、この session の主題を短い kebab-case で controller が決め、step 6 の通知に書く。

本文の「前提の節」は `.claude/commands/feature-flow.md` の `## 前提` 節、「Phase 7 step 7」は同じ file の Phase 7 を指す。

## しないこと

- `/clear` を自分で打つ
- `--no-verify` で push する、`disk-sweep.ps1` に `-Apply` を付ける
- push が通る前に kuriya の goal を update する

止まる条件は各 step にある: 鎖の末端 script が exit 0 以外 (step 1 / 2)、push が拒否された (pre-push hook を含む。step 3)、commit / push がそれ以外の理由で失敗した (step 3 / 6)。kuriya に繋がらないことでは止まらない (step 4 の文言を handoff 冒頭に書いて続ける)。step 0 の状態 helper が `state: skipped …` を印字した時 (`.dev` の無い checkout、public clone) もここで止まる。

## 手順

0. **状態を読んで、数える**
   - **最初に状態 helper を打つ** — controller が Bash tool で、**絶対 path** のまま実行する (script は cwd に依存しない):
     ```bash
     python "<repo root の絶対パス>/.claude/skills/close-session/scripts/close_session_state.py"
     ```
     `<repo root の絶対パス>` は `/next-work` の git command と同じ置き方 (`.claude/commands/next-work.md` の Phase 0) で、
     この checkout の root の絶対 path を書く。出力の「状態」block (鎖の末端、`session id:` と `source:`、3 つの git status、
     deny / rewrite の集計) を**読んでから**先へ進む。
     - `wrapper: error delegate missing` は private `.dev` checkout が古いという意味。`.dev` を pull して打ち直し、続けない
     - `wrapper: error .dev is not a nested git repository` は owner-checkout の前提 (`.dev` が自身の private repo) が壊れているという意味。`/next-work` の止まる条件 1 と同じ止まり方で、checkout を直し、続けない
     - この command が 0 以外で終わった (`wrapper: delegate exit N` / `wrapper: error …` / tool error。`python` が無い、path が違う、collector が落ちた) なら、state は不完全。**止めて原因を直し、打ち直す**。0 以外のまま step 1 以降へ進まない
     - `state: skipped (no .dev checkout — …)` が出たら、この checkout では手順を回せない (owner 用)。user に 1 行で伝えて止まる (`/next-work` と同じ)
     - exit 0 で終わった run の中の各項目の `exit:` 行が 0 以外なら、その項目は読めていない (collector 自体は常に exit 0 で終わる)。鎖の末端の項目の `exit:` が 0 以外なら step 1 の「鎖が切れている」停止条件。`session id:` 行の `source:` が `env` 以外なら取得元を疑う — `guess` は最も新しい scratchpad で、別 session を数えていることがある (kuriya trap #295)
   - 次に background task の leak を見る (`run_in_background` の polling が残っていないか。あれば `TaskStop`)
   - 次に上で読んだ「状態」の `guard_deny_by_rule.py --list` の行から controller 分の deny を選び、step 1 で書く handoff の guard 節に
   1 件ずつ `- [Rn] <target の先頭> — 判断: なし (反射) | あり (<理由>) | 不明` と書く。subagent 分は deny の場の記録が無いので
   `判断: 不明` とまとめて 1 行で書き、後から埋めない (台帳 `.dev/knowledge/repeat-offences-ledger.md` の `判断` 列と同じ規律)。
   `rewrite by rule:` 行の件数 (hook が R5 / R6 を書き換えて通した数) も同じ節に書く

1. **handoff doc を即時 write**: `.dev/knowledge/session-<YYYY-MM-DD>-<topic>-handoff.md`
   - **書く前に鎖の末端を取る** (`powershell -NoProfile -File .dev/tools/handoff_tail.ps1`)。**exit 0 で stdout がちょうど
     1 行の時だけ先へ進む** — script は鎖が切れていても候補の path を印字して exit 1 するので、stdout だけ見て 1 本選ばない。
     exit 0 以外なら、切れた鎖の上に新 handoff を足さず、出力を添えて user に報告して close-out を止める。返った 1 行が直前の
     handoff で、**型もそれ** — 開いて同じ節立て (frontmatter の title / date / tags、`## ★ 次にやること`、今日やったこと、
     閉じる直前の状態、guard、環境の罠、kuriya) で書く。**session を `/next-work` から始めていない時ほど必要** —
     その場合は `.dev/README.md` の handoff の段落も読んでいない
   - **冒頭 (title の次の段落) に `前の handoff: [[<その末端の basename>]]`**。これが無いと末端が 2 本になり、次 session の
     `/next-work` が Phase 0 で止まる (2026-10-02 に実際に止まった)
   - **前 handoff の「★ 次にやること」のうち生きている項目を写す** (期日つきのものは必ず)。この session が触っていない軸の
     項目も、新 handoff が末端になった瞬間に前 handoff からは読まれなくなる。写さないなら「本体は前 handoff のまま」と書く
     (`/next-work` はその文言で 1 本遡る)
   - 現状の git state (`git log --oneline -5`)
   - 完了済 phase / 進行中 phase / 未着手 phase
   - 重要な constraint / pattern (`CLAUDE.local.md` 規約、subagent prompt の `.dev/` untracked 注意、codex review loop 規約)
   - 次セッションでの開始手順 (5-7 step に細分化)
   - オープン論点 / 注意
   - 完了基準 checklist
   - background task leak の確認 (`run_in_background` の polling が残っていないか)
   - **disk の空きを測った数字** (SessionStart の `disk` 行と同じ値)。**handoff のたびに、その場で測る**: 報告モード (`powershell -NoProfile -File .dev/tools/disk-sweep.ps1`、何も消さない) を打ち、空きと `target` の TOTAL を書いて user にも伝える。**SessionStart の値で済ませない、`LOW` が出ていたかどうかで分岐もしない** — あれは session の始めの値で、`/full-audit` や `--ignored` の test や cross build を挟めば、始めは閾値より上でも閉じる時には割っている。release を切った session は Phase 7 step 7 でも測っているが、その後に build していればそれも古いので、ここでも測る。**`.dev/tools/disk-sweep.ps1` が無い checkout (前提の節) では測れない** — handoff は止めず、この項に「disk: 未計測 (script なし)」と書く。PowerShell を手で組んで代用しない、「クリーンアップした」とも書かない (Phase 7 step 7 にも同じ句がある)。消すのは user が `/disk-sweep apply` と打った時だけで、controller からは測るところまで。**worktree や branch を片付けたことは disk を掃除したことにならない** — 「クリーンアップした」と書く前に `target` 直下を測る (2026-09-18、release session を空き 20 GB で閉じていた)
2. **push の前に鎖を確かめる**。もう一度 `powershell -NoProfile -File .dev/tools/handoff_tail.ps1` を打ち、**exit 0 で
   新 handoff の path 1 行**が返ることを見る (2 行返る = step 1 の `前の handoff:` を書き忘れている。直してから先へ)
3. `.dev` が **それ自体の repository** であることを確かめてから push する (前提の節)。nested repo が
   無ければ `git -C .dev` は親 repo に向き、`add -A` が親の変更を staging して `push` は親の origin へ行く:
   ```bash
   case "$(git -C .dev rev-parse --show-toplevel)" in
     */.dev) git -C .dev add -A && git -C .dev commit -F <msgfile> && git -C .dev push ;;
     *) echo "ABORT: .dev is not its own repository; see the preconditions" >&2; exit 1 ;;
   esac
   ```
   `.dev` の pre-push hook (`.dev/tools/hooks/pre-push`) が step 2 と同じ script を打ち、末端が 1 本でなければ push を
   止める。**止まったら handoff に marker を足して push し直す。`--no-verify` で迂回しない** — 迂回した push は次 session の
   入口をそのまま塞ぐ。**commit か push がそれ以外の理由 (network / 認証 / 別の hook) で失敗したら step 4 へ進まない** —
   remote に無い handoff を goal が指す状態を作らないため。直して push が通ってから続ける。直せなければ
   「handoff は local のみ、goal は未更新」と user に伝えて止まる
4. **push が通ってから、kuriya の goal を新 handoff に向ける**。goal (handoff の path を body に持つ item。番号と運用は
   `.dev/README.md` の handoff の段落) を `mcp__kuriya__update` で新 path + state + next に差し替え、**`mcp__kuriya__status` で
   読み戻して goal の body に新 handoff の path が入っていることを見る** (update が通ったつもりで通っていない形を残さない)。
   できなかった時は 2 通りを分けて handoff の冒頭 (`前の handoff:` の次) に書き、その 1 行を commit / push し直す:
   - **kuriya に繋がらない**: 「kuriya goal 未更新 (未接続)。次 session で kuriya が繋がっていれば `/next-work` は goal と
     末端の食い違いを見せて止まる — 本 handoff を正として goal を本 path に update して続ける。繋がっていなければ
     `/next-work` は突き合わせ無しで本 handoff から続く」
   - **繋がるが読み戻しが合わない**: もう 1 回 update して読み戻す。それでも合わなければ「kuriya goal 不一致 (update が
     反映されない)。次 session の `/next-work` は食い違いで止まる — 本 handoff を正として goal を直してから続ける」
5. **`/next-work` の Phase 0 を自分で通してから「再開できる」と言う**。`.claude/commands/next-work.md` を開き、Phase 0 の
   step 3〜6 (末端を取る / 末端を読む — 遡りを含む / kuriya と突き合わせる / repo 状態を見る) を**そこに書いてあるとおりに**
   実行する。**判定の中身 (何を一致とみなすか、どの repo を見るか、何が止まる条件か) はここに写さない** — 家は next-work で、
   写しは食い違う。step 4 で「goal 未更新 / 不一致」の 1 行を handoff に書いた場合、突き合わせが食い違うのは想定どおりで、
   その 1 行があること自体を確認する。それ以外で 1 つでも止まる条件に当たれば直す。直せないものは handoff の
   「閉じる直前の状態」に「次 session の入口で止まる理由」として書き (書いたら commit / push し直す)、step 6 の通知にも書く
6. ユーザに通知: `handoff を <path> に書き、.dev を push しました。/clear して「<path> を読んで続きを進めて」と一言伝えれば再開できます。`
   step 4 / 5 で残したものがあれば、この文の後にそれを 1 文ずつ足す (「再開できます」だけで終えない)。
   **この通知は、この節で打った commit / push が全部 remote に届いた時だけ出す** — step 3 の初回だけでなく、step 4 / 5 で
   handoff に書き足した後の commit / push も同じ gate。どれかが失敗したら直して通す。直せなければ「push しました」と言わず、
   「handoff に書き足した分は local のみで、remote の handoff には次 session が止まる理由が書かれていない」と user に伝えて止まる

context が切り替わったら、SessionStart 通知を起点に handoff doc を読んで再開する。

handoff doc の型は**固定の 1 本ではなく、鎖の末端 (= 直前の handoff)**。型の file 名をここに書かない — 規約
(`前の handoff:` の書き方は `.dev/README.md`) が変わっても古い型を指し続け、それをなぞった handoff が鎖を切る。
