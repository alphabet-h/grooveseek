---
description: PR で `@codex review` を trigger し、3 endpoint (inline / reviews / issue) を `(id, updated_at)` set diff で同時 polling、state-base convergence + P0/P1 absence + sentinel text の 3 layer で判定、wall-clock timeout / error string detect / cost-aware retry cap で hardening したオーケストレータ
---

# /codex-review

GitHub PR で codex review (`chatgpt-codex-connector[bot]`) を **trigger + 3 layer convergence detection + 結果 fetch + 整形** する 1 step orchestrator。`/feature-flow` の Phase 6 から呼ばれる sub-step、または手動の単独 cycle で使用。

## 想定起動タイミング

- PR を立てた直後 (= initial review trigger)
- inline P0/P1 を fix → 再 push 後 (= re-review trigger)
- `/feature-flow` orchestrator の中から auto invoke

引数:

- `<PR#>` — 必須。GitHub PR 番号 (例: `/codex-review 53`)
- `<max_rounds>` — optional、**default 3** (= 罠 16: cost-aware、25 credits × 3 = 75 credits/cycle)
- `<per_round_timeout_sec>` — optional、default 600 (= 10 min、罠 9: stale connector 検知)

## 前提

- `gh` CLI 認証済 (`gh auth status` で確認可)
- リポジトリで `chatgpt-codex-connector[bot]` の GitHub App install 済
- 本 command は **destructive 操作なし** (= GitHub API read + comment post のみ)、ローカル file system 改変なし

## 設計の柱 (= 踏んだ罠の構造的回避)

本 command は `.dev/knowledge/codex-review-loop-pitfalls.md` に記録した罠を
**構造的に回避**する設計。回避しているものは下の表がすべてで、**表がそのまま一覧**
— 件数を本文に書かず、**数えるコマンドを置く**:

```bash
grep -o '^| 罠 [0-9][0-9]*' .claude/commands/codex-review.md | sort -u | wc -l
# `[0-9]*` は空にもマッチして表のヘッダ行を数えるので `[0-9][0-9]*`
```

番号の範囲も書き写さない。罠は連番では増えないので、**書き写した数字と範囲は必ず古くなる**
(この節は「22 罠」「罠 7-19 + 21-22 + 24-33」と書いてあった。どちらも実際と違っていた)。

| 罠 | 回避手段 |
|---|---|
| 罠 8 (per_page=30 saturation) | `gh api --paginate` または `?per_page=100` で全 page 取得 |
| 罠 9 (silent connector fail) | per-round wall-clock timeout (default 600s) で escalate |
| 罠 10 (Script exited deterministic) | error string `"Something went wrong"\|"Script exited"\|"Try again later"` を terminal failure として detect、retry しない |
| 罠 11 (bot user filter) | jq で `select(.user.login=="chatgpt-codex-connector[bot]")` 完全一致 |
| 罠 12 (reviews endpoint 軽視) | `pulls/<N>/reviews` を **第 3 必須 endpoint** として state + submitted_at + commit_id 基準 convergence |
| 罠 13 (count base edit/deletion miss) | `(id, updated_at)` set diff で track、count 単独は使わない |
| 罠 14 (sentinel 文言依存) | 3 layer convergence: primary = review state、secondary = P0/P1 absence、tertiary = sentinel grep |
| 罠 15 (re-trigger 内 @codex 言及) | re-trigger body は `@codex review\n\n<fix sketch>` のみ、本文中で `@codex` を bare word でも mention しない |
| 罠 16 (cost 25 credits × N) | max_rounds default を 5 → **3** に縮小 |
| 罠 17 (P0/P1 only) | docs に "GitHub では P0/P1 のみ surface、P2/P3 は AGENTS.md で override" を明記 |
| 罠 18 (`@codex address` ≠ verb) | re-trigger は **必ず `@codex review`**、別 verb 不使用 |
| 罠 19 (Windows jq CRLF) | 全 jq filter を `gh api --jq` 内部 jq で実行、外部 `\| jq` パイプ禁止 |
| 罠 24 (codex P2 dogfood: snapshot drops path/line) | `snapshot_inline` の jq projection に `path, line, original_line` を残し、Step 5 の inline 整形で `\(.path):\(.line)` を表示できるようにする |
| 罠 25 (codex P1 dogfood: P-badge text vs image) | Step 5 の inline 抽出も Step 4 Layer 2 と同じ `contains("![P0 Badge")`/`contains("![P1 Badge")` で揃える (= 2 call site の lockstep) |
| 罠 26 (baseline-after-trigger race) | baseline (`PREV_INLINE` / `PREV_REVIEWS` / `PREV_ISSUES`) を **trigger 投稿前** に取る (= Step 1)。codex は 1-30 秒で応答する場合があり、trigger 後 baseline では response が baseline に取り込まれて diff 永久 false → wall-clock timeout |
| 罠 27 (P-badge を全 history で数える) | `pulls/<N>/comments` は PR 全 history を返すため、prior round の P0/P1 が resolved 状態でも残り続ける。Step 4 で `NEW_INLINE = CUR - PREV_INLINE` を取り、**当該 round で新規追加された inline のみ** を P-badge カウント対象にする。Step 5 整形も同じ `NEW_INLINE` を使用 (= 2 call site lockstep) |
| 罠 28 (feature-flow と codex-review の max_rounds 不整合) | feature-flow Phase 6 は `/codex-review <PR#> 5` と explicit に渡す (= CLAUDE.local.md guardrail "5 round 経過で user 報告" と整合)。codex-review 単体 default は cost-aware の 3、feature-flow から呼ぶ時のみ 5 (= 計画的 5 round budget) |
| 罠 29 (id-only set diff が edit を miss) | NEW_INLINE の set diff を `(id, updated_at)` compound key で取り、codex が既存 inline (= 同 id) の body を update して P-badge を追加 / 昇格しても捕捉する |
| 罠 30 (P2-only round で controller が判断材料を取れない) | Step 5 整形に `=== Inline P2 (controller-judgment items, current round only) ===` section を追加し、P2 inline の path + body を必ず出力する (= P0/P1 = 0 + P2 > 0 の round で convergence indeterminate になる時、controller が「取り込み or skip」判断するために具体内容を必ず提示) |
| 罠 31 (gh api --paginate --jq で multi-page が単一 array にならない) | snapshot helpers は `?per_page=100` で page 数最小化 + 内部 `--jq "[.[] \| select(...) \| {...}]"` で per-page 配列化 + 外部 `jq -s "add // [] \| sort_by(.id)"` で merge して single array 化 (= --paginate と --jq は per-page 別々に走るため、外部 slurp なしでは multi-page で multiple JSON document concatenation になり、downstream `--argjson prev` 等が壊れる) |
| 罠 32 (initial delta で convergence 判定 = codex multi-write を miss) | Phase A (initial activity detection) → Phase B (`QUIET_WINDOW_SEC=180s` の quiet window 確認) の 2 phase polling で round complete を待つ。codex は review submission の後に inline comment を秒〜数十秒遅れで post するため、初回 delta で convergence 判定すると stale state で false-converge する |
| 罠 34 (quiet window 30 秒では足りない) | 同一 commit への 2 本目の review submission を **2 分 24 秒後**に観測 (PR #118)。`QUIET_WINDOW_SEC` は観測値 144 秒 + 安全率で **180**。窓幅を 1 回の観測から決めると、その観測が下限だった場合に**静かに取りこぼす** |
| 罠 44 (push が既存 inline の `line` を貼り直す) | Phase A の判定は snapshot の**生 JSON ではなく `(id, updated_at)` の射影**で行う。新しい commit を push すると GitHub が既存 inline の `line` を新 diff に貼り直すので、codex が何も書いていなくても生 JSON は変わる (PR #162 round 4 で「収束・指摘 0 件」を誤報告) |
| 罠 46 (reaction の種類を見ずに数える) | `eyes` = 「受け取った / 着手した」、`+1` = 「レビュー済み・指摘なし」。**種類で判定する**。`[.[] \| select(.user.login==BOT)] \| length` では着手の合図が合格に見える |
| 罠 47 (`@codex review` が届かないことがある) | 実測 **8 回中 2 回** (PR #162 round 4 / 8)。trigger comment に **reaction が 1 つも付かない**のが「届いていない」の signal。この場合は再 trigger で復帰する。**`exit 3` で stale connector と診断する前に必ず reaction を見る** |
| 罠 48 (transient API failure が差分に見える) | 罠 43 の再発。差分を検知したら **10 秒後にもう一度取り、同じ差分が再現した時だけ**信じる。射影 (罠 44) は「変化の原因が観測対象でない」に効くが「**観測自体の失敗**」には効かない — 別の防御が要る |
| 罠 49 (指摘が review 本文に入る) | P-badge の計数を **inline + review 本文**の両方で行う。実測 (PR #164 round 7): 新 review が HEAD に対して出て inline 0 件、しかし本文に P2 が 1 件 (permalink 形式)。inline だけ数えると Layer 1 (state=COMMENTED) と Layer 3 (sentinel 無し) も収束側に倒れるので **3 layer 全部を通り抜ける**。罠 12 で endpoint を足したが、**その endpoint のどのフィールドに指摘が入り得るか**を数えていなかった。**さらに body も round scope し、その round の submission を全部数える** (`.[-1]` だけでは 1 round 複数 submission (罠 34) で先の P0/P1 を落とす)。 — 罠 21 のとおり re-review で新 submission が出ない round があり、`LATEST_REVIEW` が前 round のままだと修正済みの指摘を数え続けて収束しない (罠 27 / 33 と同型) |
| 罠 33 (sentinel / terminal-error が PR 全 history で評価される) | `LATEST_ISSUE_BODY` を `NEW_ISSUES = $CUR_ISSUES − $PREV_ISSUES` (= current round で post された issue comment のみ) から derive する。罠 27 (P-badge round-scoping) と同じ pattern を sentinel + terminal-error チェック側にも適用、prior round の sentinel "Didn't find any major issues" が後続 round に漏れて false-converge する race を排除 |
| 罠 50 (**trigger の POST 自体が失敗する**) | Step 2 で POST を最大 3 回まで再試行し、**comment id が数字であることを確認してから待つ** (でなければ `exit 6`)。実測 (PR #176): 2 回連続 503、その間 GET は全部成功していたので「codex が答えない」(= 罠 9) に化けて 600 秒を捨てた。**待つ前に、投稿できたことを確かめる** |
| 罠 23 の再発 (**列挙した badge の外に指摘が来る**) | P0/P1/P2 に加えて **P3 も数える** (PR #177 で P3 の指摘を `P2=0` = 「指摘なし」と読みかけた)。さらに Step 5 に **badge の有無を問わず round の inline を全部出す** section を置く — 計数は列挙に依存するが、**表示は依存させない** |
| 罠 51 (**PR を開いた直後の round は baseline に指摘が入っている**) | codex は「Open a pull request for review」でも trigger される。Step 1 の baseline 時点で自動レビューが終わっていると、round scoping (罠 27/33/49) が正しく効いた結果その指摘が `NEW_*` から消え、`new_inline=0 + sentinel=true` = 「指摘 0 件で収束」に見える。実測 (PR #178 round 1): baseline の inline 1 件が **P1** だった。**判定は round diff のまま、初回起動では baseline を必ず表示する** (罠 30 と同じ「判定と表示を分ける」形)。条件を「差分が 0 件のとき」にしてはいけない — explicit trigger が何か 1 つでも出した round で false になり、防ぎたかったケースがそのまま抜ける (codex P1 on PR #179)。**「初回か」を shell 変数で持ってもいけない** — `/codex-review` は round ごとに別プロセスなので毎回 1 に戻り、解決済みの指摘を毎回「未読の baseline」として出す (codex P2 on PR #179)。`@codex review` の投稿履歴から判定する |
| 罠 52 (**診断が stdout に出る / 非 ASCII**) | 進捗・警告・abort は `diag()` 経由で **stderr へ、ASCII のみ**。AGENTS.md の "Results go to stdout, diagnostics to stderr" と同じ理由で、この script の *結果* は review の中身だけ。日本語を stderr に出すと CP932 コンソールで mojibake になる (codex P1 on PR #179) |
| 罠 54 (**`$( )` の中の `exit` はサブシェルしか終わらせない**) | `post_trigger` は失敗時に `exit` ではなく `return 1` を返し、呼び出し側が `|| exit 6` する。`TRIGGER_COMMENT_ID=$(post_trigger ...)` は command substitution なので、中で `exit` しても親は**空の id を持ったまま polling に入り**、この helper が防ぐはずだった 600 秒待ちをそのまま再現する (codex P1 on PR #179) |
| 罠 55 (**bot 由来の本文を stderr に流す**) | stderr を ASCII に保つ規約は wrapper の文言だけでは守れない。`LATEST_ISSUE_BODY` は codex が書いた内容 = **この実行の結果**なので stdout に出す。英語の marker に日本語が混じった瞬間に CP932 で mojibake になる (codex P1 on PR #179) |
| 罠 53 (**同じ操作を 2 箇所に書く**) | trigger の投稿・retry・id 検証は `post_trigger()` 1 つに集約し、Step 2 と Step 7 の両方から呼ぶ。書き写すと **初回 round は守られたまま re-review round だけ壊れる**、という気付けない形になる — 実際 abort の文言が既にずれていた (codex P1 on PR #179) |

## 実行フロー

### Step 1 — setup helpers + take baseline (BEFORE trigger)

罠 26 (= dogfood PR #54): codex は trigger 後 **1-30 秒で応答することが多い**。trigger を先に投げてから baseline を取ると、baseline 時点で既に response が含まれており、Step 3 polling の diff 判定が永久に false → wall-clock timeout (`exit 3`)。**baseline は trigger 投稿 *前* に取る** こと:

```bash
OWNER_REPO=$(gh repo view --json nameWithOwner --jq .nameWithOwner)
PR=<PR#>
BOT="chatgpt-codex-connector[bot]"

# snapshot helper (paginate で罠 8 回避、bot user filter で罠 11 回避)
# 罠 31 (codex P1 round 4 on PR #54): `gh api --paginate --jq` は **page ごと**
# に jq filter を適用して結果を stdout に concatenate するため、multi-page で
# 1 つの JSON array にならず、`jq --argjson prev` 等の downstream consumer に
# invalid JSON を渡してしまう (= 30+ 件の inline comment で発生)。
# 解決: per_page=100 で page 数を最小化 + 内部 --jq で per-page array を作る +
# 外部 `jq -s "add // [] | sort_by(.id)"` で merge して single array にする。
# 罠 19 (Windows CRLF) は jq への JSON パイプでは発生しない (= jq の JSON
# parser は CR を whitespace として許容)。
snapshot_inline() {
  # 罠 24 (codex P2 on PR #54): preserve path/line/original_line so Step 5 can
  # render `<file>:<line>` for each finding. Dropping these in the projection
  # left every reported P0/P1 finding pointing at "null:null".
  gh api --paginate "repos/${OWNER_REPO}/pulls/${PR}/comments?per_page=100" \
    --jq "[.[] | select(.user.login==\"${BOT}\") | {id, updated_at, path, line, original_line, body}]" \
    | jq -s "add // [] | sort_by(.id)"
}
snapshot_reviews() {
  gh api --paginate "repos/${OWNER_REPO}/pulls/${PR}/reviews?per_page=100" \
    --jq "[.[] | select(.user.login==\"${BOT}\") | {id, state, submitted_at, commit_id, body}]" \
    | jq -s "add // [] | sort_by(.id)"
}
snapshot_issues() {
  gh api --paginate "repos/${OWNER_REPO}/issues/${PR}/comments?per_page=100" \
    --jq "[.[] | select(.user.login==\"${BOT}\") | {id, updated_at, body}]" \
    | jq -s "add // [] | sort_by(.id)"
}

# 進捗と警告は stderr、ASCII のみ (AGENTS.md "Results go to stdout, diagnostics
# to stderr"、および CP932 コンソールでの mojibake 回避)。この script の
# *結果* は review の中身なので、それだけが stdout に出る。
diag() { printf '%s\n' "$*" >&2; }

# trigger の投稿は **1 か所だけ**に置く (= Step 2 と Step 7 の両方から呼ぶ)。
# 2 つ書くと retry 回数 / 検証 / abort の挙動がずれ、初回 round は守られたまま
# re-review round だけ壊れる、という気付けない形になる。
#
# 罠 50 (PR #176): POST 自体が 503 で落ちることがある。その時 `gh pr comment` は
# URL を返さないので id が数字にならない。**投稿できたことを確認してから待つ** —
# 確認しないと、誰も読んでいない trigger を 600 秒待って「codex が答えない」
# (= 罠 9) と誤診する。実測で 2 回連続 503、その間 GET は全部成功していた。
#
# 成功時は comment id を stdout に返すので `TRIGGER_COMMENT_ID=$(post_trigger ...)`
# で受ける。**失敗時は `exit` ではなく `return 1`**: この関数は command
# substitution の中で走るので、`exit` はそのサブシェルしか終わらせない。
# 親は空の id を持ったまま polling に入り、**この関数が防ぐはずだった 600 秒待ち
# をそのまま再現する** (codex P1 on PR #179)。呼び出し側で `|| exit 6` する。
post_trigger() {   # $@ = gh pr comment の body 指定 (--body / --body-file)
  local url id attempt
  for attempt in 1 2 3; do
    url=$(gh pr comment "$PR" "$@") && break
    diag "  (trigger POST failed, attempt ${attempt}/3)"
    sleep 20
  done
  id=$(printf '%s' "$url" | sed 's/.*issuecomment-//')
  case "$id" in
    ''|*[!0-9]*)
      diag "ABORT: trigger did not post (id='${id}' url='${url}')"
      diag "Action: do not wait. Transient GitHub failure; re-run in a few minutes."
      return 1;;
  esac
  printf '%s\n' "$id"
}

# 罠 26: baseline first, trigger second (順序を逆にしない)
PREV_INLINE=$(snapshot_inline)
PREV_REVIEWS=$(snapshot_reviews)
PREV_ISSUES=$(snapshot_issues)

# 罠 51: 「今が初回か」を知る必要がある。round 1 の baseline は「PR を開いた時の
# 自動レビュー」で controller は未読、round 2 以降の baseline は前 round の内容で
# 既読 — この区別が付かないと baseline を出す条件が書けない。
#
# **shell 変数では持てない。** `/codex-review` は round ごとに別プロセスとして
# 呼び直されるので、`ROUND=${ROUND:-1}` は毎回 1 に戻る (codex P2 on PR #179)。
# そうなると再 review のたびに解決済みの指摘を「未読の baseline」として出し、
# controller に再検討させる。
#
# **投稿履歴から判定する**: この PR に `@codex review` をまだ 1 度も投げていない
# なら初回。状態を持たずに決まり、途中から実行しても正しい。
# 罠 31: `--paginate --jq` は page ごとに出るので外側で足す。
PRIOR_TRIGGERS=$(gh api --paginate "repos/${OWNER_REPO}/issues/${PR}/comments?per_page=100" \
  --jq '[.[] | select(.body | startswith("@codex review"))] | length' | jq -s 'add // 0')
FIRST_INVOCATION=$([ "${PRIOR_TRIGGERS}" = "0" ] && echo true || echo false)
diag "prior @codex review triggers on this PR: ${PRIOR_TRIGGERS} (first invocation: ${FIRST_INVOCATION})"
```

### Step 2 — post @codex review trigger

```bash
# 罠 47: comment id を捕まえておく。reaction を見るのに要る —
# 「reaction 無し = 届いていない」が、無応答を stale connector と
# 誤診しないための唯一の signal。
# 投稿と検証は Step 1 の `post_trigger` に集約してある (= 罠 50)。
TRIGGER_COMMENT_ID=$(post_trigger --body "@codex review") || exit 6
diag "trigger comment id=${TRIGGER_COMMENT_ID}"
```

mention は **冒頭 1 回のみ**、本文に追加 context を書く場合 `@codex` 文字列を bare word として使わない (= 罠 15)。

### Step 3 — round-level polling with state snapshot diff + quiet-window completion

snapshot per round で 3 endpoint を `(id, updated_at)` set として取得。Step 1 baseline との diff があれば「activity detected」、その後 **quiet window** (= N 秒間 snapshot 不変) を確認してから convergence 判定に進む (= 罠 13 + 罠 32 完了検知):

```bash
ROUND_START=$(date +%s)
PER_ROUND_TIMEOUT=600   # 罠 9 wall-clock timeout
QUIET_WINDOW_SEC=180    # 罠 34: 同一 commit への 2 本目を 144 秒後に観測、+ 安全率

# 罠 44: Phase A の判定は **生 JSON ではなく (id, updated_at) の射影**で行う。
# push すると GitHub が既存 inline の `line` を新しい diff に貼り直すので、
# codex が何も書いていなくても生 JSON は変わる。表示に要るフィールド (罠 24) は
# snapshot に残したまま、比較のときだけ落とす。
key_inline() { snapshot_inline | jq -c '[.[] | {id, updated_at}]'; }
key_issues() { snapshot_issues | jq -c '[.[] | {id, updated_at}]'; }
key_reviews() { snapshot_reviews | jq -c '[.[] | {id, submitted_at}]'; }

# 罠 46: `eyes` は「受け取った」、`+1` が「レビュー済み・指摘なし」。種類で見る。
reaction() {   # $1 = "+1" | "eyes"
  gh api "repos/${OWNER_REPO}/issues/comments/${TRIGGER_COMMENT_ID}/reactions" \
    --jq "[.[] | select(.user.login==\"${BOT}\" and .content==\"$1\")] | length" 2>/dev/null || echo 0
}

PREV_KI=$(key_inline); PREV_KS=$(key_issues); PREV_KR=$(key_reviews)

# Phase A: wait for first activity
while true; do
  ELAPSED=$(( $(date +%s) - ROUND_START ))
  if [ "$ELAPSED" -gt "$PER_ROUND_TIMEOUT" ]; then
    # 罠 47: reaction が 1 つも無い = **trigger が届いていない** (実測 8 回中 2 回)。
    # stale connector ではないので、再 trigger すれば復帰する。
    if [ "$(reaction eyes)" = "0" ] && [ "$(reaction +1)" = "0" ]; then
      diag "WARN: trigger comment has no reaction - codex never received it (trap 47)."
      diag "Action: re-trigger on the same commit (once, automatically; then escalate)."
      exit 5
    fi
    diag "WARN: codex acknowledged but did not answer in ${PER_ROUND_TIMEOUT}s (trap 9)."
    diag "Action: escalate. Suggest disconnecting and reconnecting the connector."
    exit 3
  fi

  CUR_KI=$(key_inline); CUR_KS=$(key_issues); CUR_KR=$(key_reviews)

  if [ "$CUR_KI" != "$PREV_KI" ] || [ "$CUR_KS" != "$PREV_KS" ] || [ "$CUR_KR" != "$PREV_KR" ]; then
    # 罠 48: transient API failure は短いリストを返し、差分に見える。
    # 10 秒後に取り直して **同じ差分が再現した時だけ** 信じる。
    sleep 10
    if [ "$(key_inline)" = "$CUR_KI" ] && [ "$(key_issues)" = "$CUR_KS" ] \
       && [ "$(key_reviews)" = "$CUR_KR" ]; then
      diag "=== codex initial activity detected after ${ELAPSED}s (confirmed twice) ==="
      CUR_INLINE=$(snapshot_inline); CUR_REVIEWS=$(snapshot_reviews); CUR_ISSUES=$(snapshot_issues)
      break
    fi
    diag "  (diff did not reproduce = transient API result, continuing)"
  fi
  sleep 30
done

# Phase B: wait for quiet window — 罠 32 (codex P1 round 4 on PR #54): codex は
# review submission を post してから秒〜数十秒遅れて inline comment を post する
# multi-write pattern。最初の delta で convergence 判定すると stale な inline
# state で false-converge する (= 後続の P0/P1 が見えない)。`QUIET_WINDOW_SEC`
# 秒間 snapshot が不変な状態を確認してから Step 4 へ進む。
QUIET_START=$(date +%s)
# Phase A と**同じ鍵**で比べる (罠 44)。ここだけ生 JSON にすると、push で
# `line` が貼り直された瞬間に quiet window が永遠にリセットされ続ける。
LAST_INLINE=$(key_inline)
LAST_REVIEWS=$(key_reviews)
LAST_ISSUES=$(key_issues)
while true; do
  WALL_ELAPSED=$(( $(date +%s) - ROUND_START ))
  if [ "$WALL_ELAPSED" -gt "$PER_ROUND_TIMEOUT" ]; then
    diag "WARN: quiet window not reached within ${PER_ROUND_TIMEOUT}s wall-clock. Proceeding to Step 4."
    break
  fi

  sleep 15
  CHECK_INLINE=$(key_inline)
  CHECK_REVIEWS=$(key_reviews)
  CHECK_ISSUES=$(key_issues)

  if [ "$CHECK_INLINE" = "$LAST_INLINE" ] && \
     [ "$CHECK_REVIEWS" = "$LAST_REVIEWS" ] && \
     [ "$CHECK_ISSUES" = "$LAST_ISSUES" ]; then
    QUIET_ELAPSED=$(( $(date +%s) - QUIET_START ))
    if [ "$QUIET_ELAPSED" -ge "$QUIET_WINDOW_SEC" ]; then
      diag "=== quiet window of ${QUIET_WINDOW_SEC}s confirmed, round complete ==="
      break
    fi
  else
    # still active, reset quiet window
    QUIET_START=$(date +%s)
    LAST_INLINE=$CHECK_INLINE
    LAST_REVIEWS=$CHECK_REVIEWS
    LAST_ISSUES=$CHECK_ISSUES
    diag "  (still receiving codex writes, reset quiet window)"
  fi
done
```

### Step 4 — 3 layer convergence detection

罠 14 の文言依存を緩和、業界 defacto Pattern C (= state-base) を primary に:

```bash
# 罠 33 (codex P1 round 5 on PR #54): sentinel / terminal-error checks must
# be scoped to **current round** issue comments (= delta vs PREV_ISSUES),
# not全 PR history。prior round で sentinel ("Didn't find any major issues")
# が出ていた場合、current round が new review/inline を出して new issue
# comment が無い state でも `LATEST_ISSUE_BODY` は prior round の sentinel を
# 拾い続けて SENTINEL_MATCH=true → false-converge する。
LATEST_REVIEW=$(snapshot_reviews | jq '.[-1]')
CUR_ISSUES_FRESH=$(snapshot_issues)
NEW_ISSUES=$(jq -n --argjson prev "$PREV_ISSUES" --argjson cur "$CUR_ISSUES_FRESH" '
  ($prev | map({key: (.id|tostring), value: .updated_at}) | from_entries) as $prev_map |
  $cur | map(select(.id as $i | ($prev_map[$i|tostring] // null) != .updated_at))
')
# codex P1 round 9 に同じ形が review 側で指摘された。**この round の comment は
# 複数ありうる** (罠 32/34 が前提にしている multi-write そのもの) ので、
# `.[-1]` だけ見ると先に来た方の sentinel / terminal error を落とす。
# 特に terminal error を落とすと「retry しても無駄」を retry し続ける。全部繋ぐ。
LATEST_ISSUE_BODY=$(echo "$NEW_ISSUES" | jq -r 'map(.body // "") | join("
---
")')
HEAD_SHA=$(gh api "repos/${OWNER_REPO}/pulls/${PR}" --jq .head.sha)

# 罠 10: error string detect → terminal failure、retry しない
TERMINAL_ERROR_PATTERN="Something went wrong|Script exited|Try again later"
if echo "$LATEST_ISSUE_BODY" | grep -qE "$TERMINAL_ERROR_PATTERN"; then
  diag "ERROR: codex returned a terminal failure body (trap 10). Body follows on stdout."
  diag "Action: do not retry. Escalate; try again later or on another PR."
  # 本文は **codex が書いた内容** = この実行の結果であって診断ではない。
  # stderr に回すと、英語の marker に日本語が混じっていた瞬間に CP932 コンソールで
  # mojibake になる — 直したのは wrapper の文言だけで、body は bot 由来のまま
  # だった (codex P1 on PR #179)。**stdout に出す**。
  echo "$LATEST_ISSUE_BODY"
  exit 4
fi

# Layer 1 (primary): review state-base — submitted_at 存在 + state が valid な submission
# 罠 21: codex は re-review で新 review submission を出さないことがある = `commit_id == HEAD_SHA`
# 縛りを外す。stale review false-positive のリスクは Layer 3 sentinel + Layer 2 P-badge で補正
REVIEW_STATE=$(echo "$LATEST_REVIEW" | jq -r '.state // "null"')
REVIEW_SUBMITTED=$(echo "$LATEST_REVIEW" | jq -r '.submitted_at // "null"')
REVIEW_COMMIT=$(echo "$LATEST_REVIEW" | jq -r '.commit_id // "null"')

STATE_OK=false
if [ "$REVIEW_SUBMITTED" != "null" ] && \
   echo "$REVIEW_STATE" | grep -qE "^(APPROVED|COMMENTED|CHANGES_REQUESTED)$"; then
  STATE_OK=true
fi
# 補強情報 (= log only): commit_id が HEAD と一致するか
COMMIT_FRESH=$([ "$REVIEW_COMMIT" = "$HEAD_SHA" ] && echo "true" || echo "false")

# Layer 2 (secondary): P-badge presence detection
# 罠 22: codex inline P-badge は `![P0 Badge](...)` / `![P1 Badge](...)` / `![P2 Badge](...)` の
# image markdown format。`[P0]` text 直書きではない
# 罠 23: 公式 docs は P0/P1 only と書いているが実例で P2 も surface する → P0/P1/P2 全部を track
# 罠 1 (jq escape): jq regex 内で `\[` は invalid escape、`contains()` で safe な substring match
# 罠 27 (codex P1 round 2 on PR #54): `pulls/<N>/comments` は PR の全 history を返す。
# round 2 以降に counter を全件で取ると、prior round で残っている P0/P1 が
# resolved 状態でも count > 0 のままで、convergence が永久に false になる。
# Step 1 で取った PREV_INLINE (= round baseline) との set diff を取り、
# **当該 round で新規に追加された inline** だけを評価対象にする。
# 罠 29 (codex P2 round 3 on PR #54): id 単独の set diff は **edit を miss する** —
# codex が既存 inline (= 同じ id) の body を update して P-badge を追加 / 昇格する
# case で false-converge する。`(id, updated_at)` の compound key で diff を取る。
CUR_INLINE_FRESH=$(snapshot_inline)
NEW_INLINE=$(jq -n --argjson prev "$PREV_INLINE" --argjson cur "$CUR_INLINE_FRESH" '
  ($prev | map({key: (.id|tostring), value: .updated_at}) | from_entries) as $prev_map |
  $cur | map(select(.id as $i | ($prev_map[$i|tostring] // null) != .updated_at))
')
# 罠 49 (PR #164 round 7): codex は指摘を **review 本文**に書くこともある —
# inline のアンカーではなく permalink の形で。inline だけ数えると
# 「activity あり / inline 0」が「レビュー済み・指摘なし」と見分けられず、
# Layer 1 (state=COMMENTED) と Layer 3 (sentinel 無し) も収束側に倒れるので
# **3 layer 全部を通り抜ける**。両方を数える。
# ...and **round-scoped**, like NEW_INLINE (罠 27) and NEW_ISSUES (罠 33).
# 罠 21 のとおり codex は re-review で新しい review submission を出さないことが
# あり、その round では `LATEST_REVIEW` は**前 round のもの**。そこから数えると
# 修正済みの指摘を毎 round 数え続け、収束しなくなる (codex P2 round 8 on PR #164)。
NEW_REVIEWS=$(jq -n --argjson prev "$PREV_REVIEWS" --argjson cur "$(snapshot_reviews)" '
  ($prev | map({key: (.id|tostring), value: .submitted_at}) | from_entries) as $p |
  $cur | map(select(.id as $i | ($p[$i|tostring] // null) != .submitted_at))
')
# 罠 34 が記録しているとおり **1 round に review submission が複数来る**。
# `.[-1]` だけ数えると、先の submission に P0/P1 があって最後のに badge が無い
# round で `P0_P1_TAGS_PRESENT=0` になり、**blocking な指摘を持ったまま収束**する
# (codex P1 round 8 on PR #164)。この round の body を全部繋いでから数える。
REVIEW_BODY=$(echo "$NEW_REVIEWS" | jq -r 'map(.body // "") | join("
---
")')
body_badges() { printf '%s' "$REVIEW_BODY" | grep -o "$1" | wc -l | tr -d ' '; }
P0_P1_TAGS_PRESENT=$(( $(echo "$NEW_INLINE" | jq '[.[] | .body | select(contains("![P0 Badge") or contains("![P1 Badge"))] | length') \
  + $(body_badges '!\[P0 Badge') + $(body_badges '!\[P1 Badge') ))
P2_TAGS_PRESENT=$(( $(echo "$NEW_INLINE" | jq '[.[] | .body | select(contains("![P2 Badge"))] | length') \
  + $(body_badges '!\[P2 Badge') ))
# 罠 23 が P2 で一度広げた形が、P3 で再発した (PR #177): P0/P1/P2 だけ数えて
# `P2=0` を「指摘なし」と読みかけたが、来ていたのは中身の正しい **P3** だった。
# **badge の集合を列挙で決め打ちしない。** P3 も数え、Step 5 では badge の
# 有無を問わず round の inline を全部出す (= 未知の badge も人の目に入る)。
P3_TAGS_PRESENT=$(( $(echo "$NEW_INLINE" | jq '[.[] | .body | select(contains("![P3 Badge"))] | length') \
  + $(body_badges '!\[P3 Badge') ))

# Layer 3 (tertiary): sentinel text variations (罠 14)、broader pattern set
SENTINEL_PATTERN="Didn't find any major issues|Hooray|Bravo|Looks good|Keep them coming|no issues found|All good|All clear|approved"
SENTINEL_MATCH=false
if echo "$LATEST_ISSUE_BODY" | grep -qiE "$SENTINEL_PATTERN"; then
  SENTINEL_MATCH=true
fi

# 統合判定 (= dry-run で発見した false negative を回避): Layer 3 sentinel **単独で converged OK**、
# Layer 1 / Layer 2 は補強情報。P0/P1 inline がある時だけ「未収束」と判定。P2 は warning に留める
CONVERGED=false
if [ "$P0_P1_TAGS_PRESENT" -gt 0 ]; then
  echo "⚠️ P0/P1 issues present (= ${P0_P1_TAGS_PRESENT} item(s)), fix needed"
  CONVERGED=false
elif [ "$SENTINEL_MATCH" = "true" ]; then
  EXTRA=""
  [ "$STATE_OK" = "true" ] && EXTRA+=" + Layer 1 state=${REVIEW_STATE}"
  [ "$COMMIT_FRESH" = "true" ] && EXTRA+=" + commit fresh"
  [ "$P2_TAGS_PRESENT" -gt 0 ] && EXTRA+=" (Note: ${P2_TAGS_PRESENT} P2 item(s), controller 判断で取り込み or skip)"
  [ "$P3_TAGS_PRESENT" -gt 0 ] && EXTRA+=" (Note: ${P3_TAGS_PRESENT} P3 item(s), 同上)"
  echo "✅ Converged (Layer 3 sentinel${EXTRA})"
  CONVERGED=true
elif [ "$STATE_OK" = "true" ] && [ "$P0_P1_TAGS_PRESENT" = "0" ] \
  && [ "$P2_TAGS_PRESENT" = "0" ] && [ "$P3_TAGS_PRESENT" = "0" ]; then
  echo "✅ Converged (Layer 1 + 2: state=${REVIEW_STATE}, no P-badges)"
  CONVERGED=true
else
  echo "⚠️ Indeterminate (no sentinel + no clean state) — re-trigger or user escalate"
  CONVERGED=false
fi

# 罠 51 (PR #178): **PR を開いた直後の round は baseline に指摘が入っている。**
# codex は「Open a pull request for review」でも trigger されるので、Step 1 で
# baseline を取る時点で自動レビューが終わっていることがある。round scoping
# (罠 27/33/49) が正しく効いた結果、その指摘は `NEW_*` から消え、
# `new_inline=0 + sentinel=true` = 「指摘 0 件で収束」に見える。
# 実測 (PR #178 round 1): baseline の inline 1 件が **P1** だった。
#
# **判定は round diff のまま (2 度数えると修正済みを再指摘してループする)、
# 表示だけ足す** — 罠 30 が P2 でやったのと同じ「判定と表示を分ける」形。
#
# 条件は **round 番号だけ**で決める。「差分が 0 件のとき」にすると、explicit
# trigger が新しい review や inline を 1 つでも出した round では false になり、
# **自動レビューの P1 が controller に届かないまま収束する** —
# この block が防ごうとしている当のケースが、そのまま抜ける
# (codex P1 on PR #179)。round 2 以降の baseline は前 round の内容で、
# controller は既に読んでいるので出さない。
if [ "$FIRST_INVOCATION" = "true" ]; then
  echo ""
  echo "=== Baseline before this run — the review GitHub ran when the PR opened (罠 51) ==="
  echo "$PREV_INLINE" | jq -r 'if length == 0 then "(baseline inline: none)" else .[] | "[baseline] \(.path):\(.line // .original_line)\n\(.body)\n---" end'
  echo "$PREV_REVIEWS" | jq -r 'if length == 0 then "(baseline review bodies: none)" else .[] | "[baseline] state=\(.state) commit=\(.commit_id)\n\(.body)\n---" end'
  diag "NOTE: round 1 baseline shown above. An empty round diff does not mean nothing was found."
fi
```

### Step 5 — 結果 fetch + 整形

```bash
echo "=== Top-level summary (review body, this round) ==="
# 罠 49: the body is not only a summary — a finding can live here, as a
# permalink instead of an inline anchor. Step 4 counts it, so this prints it;
# the two must stay in lockstep for the same reason as 罠 25 — **including the
# round scoping**, or the counter and the display disagree about which review.
echo "$NEW_REVIEWS" | jq -r 'if length == 0 then "(no new review body this round)" else .[] | .body // "" end'
echo ""
echo "=== Inline P0/P1 (review-blocking issues, current round only) ==="
# 罠 25 (codex P1 on PR #54): codex emits image markdown badges like
# `![P0 Badge](...)` / `![P1 Badge](...)`, NOT bare `[P0]` text. Filtering with
# `test("\\[P[01]\\]")` would silently drop every actionable finding. Use the
# same `contains("![P0 Badge")` / `contains("![P1 Badge")` predicate as Step 4
# Layer 2 detection (= keep both call sites in lockstep).
# 罠 27 (codex P1 round 2 on PR #54): scope to NEW_INLINE (= delta vs round
# baseline) so prior-round P0/P1 (now resolved by fixes) aren't re-listed as
# "current actionable" issues.
echo "$NEW_INLINE" | jq -r '.[] | select(.body | (contains("![P0 Badge") or contains("![P1 Badge"))) | "[\(.updated_at)] \(.path):\(.line // .original_line)\n\(.body)\n---"' 2>/dev/null
echo ""
echo "=== Inline P2 (controller-judgment items, current round only) ==="
# 罠 30 (codex P2 round 3 on PR #54): P2 だけの round で本 section が空だと
# controller は P-badge カウントを見て convergence 判定するが「具体的に何の
# P2 を取り込むか / skip するか」を決める material が無い。Step 4 が
# P2_TAGS_PRESENT > 0 を warning した時、必ずここに該当 P2 の path + body
# を出して controller が判断できる状態にする。
echo "$NEW_INLINE" | jq -r '.[] | select(.body | contains("![P2 Badge")) | "[\(.updated_at)] \(.path):\(.line // .original_line)\n\(.body)\n---"' 2>/dev/null
echo ""
echo "=== Inline, this round — ALL of them, badge or not ==="
# 罠 23 が P2 で、その再発が P3 で起きた (PR #177): **列挙した badge の外に
# 指摘が来る**。上の 2 section は列挙に依存しているので、ここは列挙せず
# **その round の inline を全部出す**。次に codex が P4 を作っても、
# あるいは badge の無い指摘を書いても、人の目には入る。
echo "$NEW_INLINE" | jq -r 'if length == 0 then "(none this round)" else .[] | "[\(.updated_at)] \(.path):\(.line // .original_line)\n\(.body)\n---" end'
echo ""
echo "=== Top-level issue comments by codex (full) ==="
snapshot_issues | jq -r '.[] | "[\(.updated_at)] \(.body)"'
```

### Step 6 — controller 判定 + retry

| 状態 | アクション |
|---|---|
| `CONVERGED=true` **かつ `FIRST_INVOCATION=true`** | **baseline を読んでから**収束を宣言する (= 罠 51)。PR を開いた直後の round では、指摘は差分ではなく baseline 側にいる。差分が空かどうかは関係ない |
| `CONVERGED=true` | **収束**、merge / tag に進む |
| `P0_P1_TAGS_PRESENT > 0` | controller が指摘内容を理解 → fix 実装 → push → goto Step 1 (= 新 baseline 取得 + re-trigger)。**ただし** `current_round >= max_rounds` (= default 3) なら user 報告 |
| `STATE_OK=false` でも `P0/P1` も sentinel もなし (= indeterminate) | controller が manual review (= human 判断)、必要なら `@codex review` 再 trigger |
| `exit 3` (= 罠 9 wall-clock timeout、**reaction はある**) | codex は受け取ったが答えなかった。user に escalate (= "codex acknowledged but did not answer, suspect stale connector") |
| `exit 5` (= 罠 47 trigger が届いていない、**reaction が 1 つも無い**) | **round を消費せず同じ commit に再 trigger する** (自動、1 回だけ)。実測でこれは 8 回中 2 回起き、再 trigger でどちらも即復帰した。2 回目も届かなければ user に escalate |
| `exit 4` (= 罠 10 terminal error) | user に escalate (= "codex returned terminal failure, retry will not help") |
| `exit 6` (= 罠 50 trigger の POST が 3 回とも失敗) | **待たずに abort**。GitHub 側の一時障害なので数分置いて再実行する。ここで待つと、誰も読んでいない trigger を 600 秒待って罠 9 と誤診する |

### Step 7 — re-review trigger (= round 2 以降)

P0/P1 fix → push 後の re-trigger。**罠 26 適用**: Step 1 と同じ要領で baseline を新 trigger 投稿の **前** に取り直す:

```bash
# 罠 26: baseline first, trigger second — round 2 以降も順序は同じ
PREV_INLINE=$(snapshot_inline)
PREV_REVIEWS=$(snapshot_reviews)
PREV_ISSUES=$(snapshot_issues)

# 罠 15 + 罠 18: @codex review 冒頭 1 回、本文中で codex を bare word でも mention しない、verb は review のみ
# 本文は heredoc ではなく **ファイルに書いて --body-file** で渡す
# (CLAUDE.local.md: heredoc / here-string は入れ子の引用符で壊れる)。
# 投稿と検証は Step 1 の `post_trigger` — Step 2 と同じ 1 つの実装 (= 罠 50)。
TRIGGER_COMMENT_ID=$(post_trigger --body-file <path>) || exit 6
diag "trigger comment id=${TRIGGER_COMMENT_ID}"
```

**round ごとに `TRIGGER_COMMENT_ID` を取り直す** (= 罠 47)。前 round の comment の
reaction を見ていると、届いていない round を「届いた」と読む。
POST の検証をここに書き写さない — round 2 以降で落ちても症状は同じ
「600 秒待って codex が答えない」で、**書き写した方だけ直し忘れると気付けない**。

戻って Step 3 から polling 再開。

## max_rounds 上限の根拠

- CLAUDE.local.md guardrail で「5 round 経過で user 報告」と明示、本 command は **default 3** で cost-conscious (= 罠 16)
- 25 credits × 3 = 75 credits/cycle、Plus plan 月次 quota の 1-2% 程度
- 3 round で収束しない = spec / 設計の問題 = user 介入 (= CLAUDE.local.md 介入ポイント 3 = 軌道修正)

## 副作用 / 不可逆性

- ⚠️ **comment post は GitHub に visible**: PR の comment 履歴に `@codex review` が残る
- ✅ **destructive ではない**: branch 削除 / force push / merge / tag 等は本 command の scope 外
- ✅ **rate limit**: 30s × 3 endpoint × 3 round = 27 req、5000 req/h budget の 0.5%
- ⚠️ **credit cost**: max ~75 credits/cycle (= 25 × 3 round)、cycle 多発時に注意

## 関連

- 動機と、各罠がどう見つかったかの詳細: `.dev/knowledge/codex-review-loop-pitfalls.md`
  (構造的に回避しているものは上の表が一覧。ノートはそれより広く、**表に番号の無い
  ものも含む** — 実測で 39-43 と 45。表に無い = この command が構造で防いでいない)
- `/feature-flow` orchestrator: `.claude/commands/feature-flow.md` (= 本 command の caller、Phase 6)
- CLAUDE.local.md `/feature-flow` 常時 guardrail 節
- 公式 source:
  - [Codex GitHub integration](https://developers.openai.com/codex/integrations/github)
  - [Codex pricing](https://developers.openai.com/codex/pricing)
  - [GitHub REST: pull request reviews](https://docs.github.com/en/rest/pulls/reviews)

## 既知の制限

- **GraphQL 移行**: 1 query で 3 endpoint 統合可能 (1-2 pt vs 3 REST calls)、refactor cost あり、別 cycle で評価
- **bot login 変更**: `chatgpt-codex-connector[bot]` が将来変更されたら jq filter を update 必要
- **AGENTS.md guideline**: 罠 17 の P2/P3 surface は AGENTS.md で override 可能、本 command は default 仕様 (P0/P1 only) 前提
