#!/usr/bin/env bash
# One round of /codex-review: trigger `@codex review` on a PR, poll the three
# endpoints codex writes to (inline / reviews / issue comments), decide
# convergence with three layers, and print the round's findings.
#
# Usage:
#   codex_review_round.sh <PR#> [max_rounds] [per_round_timeout_sec] [trigger_body_file]
#     max_rounds             default 3; the round number is derived from the PR's own
#                            `@codex review` history, so the limit holds across processes
#     per_round_timeout_sec  default 600 (wall-clock for Phase A + Phase B)
#     trigger_body_file      re-review body (`@codex review` + fix sketch); omit for round 1
#
# Exit codes:
#   0  round complete - read the `CONVERGED=true|false` line on stdout
#   2  bad arguments
#   3  codex acknowledged (reaction present) but did not answer within timeout (trap 9)
#   4  codex returned a terminal error body; retry will not help (trap 10 / 56)
#   5  trigger comment got no reaction = codex never received it (trap 47)
#   6  trigger POST failed 3 times; do not wait, re-run later (trap 50)
#   7  round limit reached; nothing was posted (trap 16 / 28)
#
# Output contract (AGENTS.md "Results go to stdout, diagnostics to stderr"):
#   stdout = the review itself (baseline on first invocation, Step 4 summary, Step 5 render)
#   stderr = progress / warnings, ASCII only (trap 52) - never bot-written text (trap 55)
#
# Every trap this script guards against is a comment next to the code that
# guards it. The numbers refer to .dev/knowledge/codex-review-loop-pitfalls.md.
# To list them:  grep -oE '罠 [0-9]+' "$0" | sort -u
# The count is deliberately not written anywhere: a copied number rots.
set -u

PR="${1:?PR number required}"
MAX_ROUNDS="${2:-3}"            # 罠 16: cost-aware (25 credits x round)。/feature-flow は 5 を渡す (罠 28)
PER_ROUND_TIMEOUT="${3:-600}"   # 罠 9: wall-clock timeout で stale connector を検知
TRIGGER_BODY_FILE="${4:-}"
case "$MAX_ROUNDS$PER_ROUND_TIMEOUT" in
  *[!0-9]*|'') printf 'usage: %s <PR#> [max_rounds] [per_round_timeout_sec] [trigger_body_file]\n' "$0" >&2; exit 2;;
esac
QUIET_WINDOW_SEC=180            # 罠 34: 同一 commit への 2 本目を 144 秒後に観測、+ 安全率
BOT="chatgpt-codex-connector[bot]"   # 罠 11: login 完全一致で filter

# The script lives at <repo>/.claude/skills/codex-review/scripts/; resolve the
# repo from there so it works from any cwd and any checkout location.
SCRIPT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
REPO_DIR=$(git -C "$SCRIPT_DIR" rev-parse --show-toplevel) || exit 1
cd "$REPO_DIR" || exit 1
OWNER_REPO=$(gh repo view --json nameWithOwner --jq .nameWithOwner)

# 進捗と警告は stderr、ASCII のみ (罠 52: AGENTS.md "Results go to stdout,
# diagnostics to stderr"、および CP932 コンソールでの mojibake 回避)。この script の
# *結果* は review の中身なので、それだけが stdout に出る。
diag() { printf '%s\n' "$*" >&2; }

# ---- snapshot helpers ----
# 罠 8: per_page=30 で saturate するので `--paginate` + `?per_page=100`。
# 罠 12: `pulls/<N>/reviews` は第 3 の必須 endpoint — 指摘が review 本文に
#   入ることがあり (罠 49)、state / submitted_at / commit_id が Layer 1 の材料。
# 罠 31 (codex P1 round 4 on PR #54): `gh api --paginate --jq` は **page ごと**
#   に jq filter を適用して結果を stdout に concatenate するため、multi-page で
#   1 つの JSON array にならず、`jq --argjson prev` 等の downstream consumer に
#   invalid JSON を渡してしまう (= 30+ 件の inline comment で発生)。
#   解決: per_page=100 で page 数を最小化 + 内部 --jq で per-page array を作る +
#   外部 `jq -s "add // [] | sort_by(.id)"` で merge して single array にする。
# 罠 19 (Windows CRLF) は jq への JSON パイプでは発生しない (= jq の JSON
#   parser は CR を whitespace として許容)。
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

# trigger の投稿は **1 か所だけ**に置く (罠 53: 初回 round と re-review round の
# 両方から呼ぶ)。2 つ書くと retry 回数 / 検証 / abort の挙動がずれ、初回 round は
# 守られたまま re-review round だけ壊れる、という気付けない形になる。
#
# 罠 50 (PR #176): POST 自体が 503 で落ちることがある。その時 `gh pr comment` は
# URL を返さないので id が数字にならない。**投稿できたことを確認してから待つ** —
# 確認しないと、誰も読んでいない trigger を 600 秒待って「codex が答えない」
# (= 罠 9) と誤診する。実測で 2 回連続 503、その間 GET は全部成功していた。
#
# 成功時は comment id を stdout に返すので `TRIGGER_COMMENT_ID=$(post_trigger ...)`
# で受ける。**失敗時は `exit` ではなく `return 1`** (罠 54): この関数は command
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

# ---- Step 1: baseline BEFORE trigger ----
# 罠 26 (dogfood PR #54): codex は trigger 後 **1-30 秒で応答することが多い**。
# trigger を先に投げてから baseline を取ると、baseline 時点で既に response が
# 含まれており、Phase A の diff 判定が永久に false → wall-clock timeout。
# **baseline は trigger 投稿 *前* に取る** (round 2 以降も順序は同じ)。
PREV_INLINE=$(snapshot_inline)
PREV_REVIEWS=$(snapshot_reviews)
PREV_ISSUES=$(snapshot_issues)

# 罠 51: 「今が初回か」を知る必要がある。round 1 の baseline は「PR を開いた時の
# 自動レビュー」で controller は未読、round 2 以降の baseline は前 round の内容で
# 既読 — この区別が付かないと baseline を出す条件が書けない。
#
# **shell 変数では持てない。** /codex-review は round ごとに別プロセスとして
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

# 罠 16 / 罠 28 (codex P2 on PR #222): max_rounds を「宣言」しても、round 数を誰も
# 持っていなければ上限は効かない — round ごとに別プロセスなので shell 変数には
# 持てず (罠 51 と同じ理由)、controller の記憶に頼ると 5 round 目を 6 回目として
# 回しても気付けない。**round 番号も投稿履歴から導く**: この PR に投げた
# `@codex review` の数 + 1 が今の round。届かなかった trigger (罠 47 の再投稿) も
# 1 と数える — 安全側に倒し、credit を使わなかった round を 1 つ余らせる方を取る。
# 上限に達していたら **投稿する前に** 止める (投稿してからでは 1 round 分の credit が消える)。
ROUND=$(( PRIOR_TRIGGERS + 1 ))
diag "round ${ROUND}/${MAX_ROUNDS} (prior @codex review triggers on this PR: ${PRIOR_TRIGGERS}, first invocation: ${FIRST_INVOCATION})"
if [ "$ROUND" -gt "$MAX_ROUNDS" ]; then
  diag "STOP: round limit reached (${PRIOR_TRIGGERS} trigger(s) already posted, max_rounds=${MAX_ROUNDS}). Nothing was posted."
  diag "Action: report to the user. Do not re-run with a larger limit on your own."
  exit 7
fi

# 罠 51 (PR #178): **PR を開いた直後は baseline に指摘が入っている。** codex は
# 「Open a pull request for review」でも trigger されるので、ここに来た時点で
# 自動レビューが終わっていることがある。round scoping (罠 27/33/49) が正しく
# 効いた結果その指摘は `NEW_*` から消え、`new_inline=0 + sentinel=true` =
# 「指摘 0 件で収束」に見える。実測 (PR #178 round 1): baseline の inline 1 件が
# **P1** だった。
#
# **判定は round diff のまま** (2 度数えると修正済みを再指摘してループする)、
# **表示だけ足す** — 罠 30 が P2 でやったのと同じ「判定と表示を分ける」形。
# 条件を「差分が 0 件のとき」にしてはいけない — explicit trigger が何か 1 つでも
# 出した round で false になり、防ぎたかったケースがそのまま抜ける (codex P1 on PR #179)。
#
# 出すのは **trigger を投げる前、Step 1 のここ**。Step 4 に置くと、round 1 が
# trigger 投稿後に abort した場合 (exit 3 / 5 / 6、terminal error、中断) に
# baseline を出さないまま終わり、**次の実行は `PRIOR_TRIGGERS > 0` を見て
# 「初回ではない」と判断して永久に出さなくなる** — trigger comment が記録して
# いるのは「実行が始まったこと」であって「baseline を読んだこと」ではない
# (codex P1 on PR #179)。ここで出せば、その後どこで落ちても表示済み。
if [ "$FIRST_INVOCATION" = "true" ]; then
  echo "=== Baseline before this run - the review GitHub ran when the PR opened (trap 51) ==="
  echo "$PREV_INLINE" | jq -r 'if length == 0 then "(baseline inline: none)" else .[] | "[baseline] \(.path):\(.line // .original_line)\n\(.body)\n---" end'
  echo "$PREV_REVIEWS" | jq -r 'if length == 0 then "(baseline review bodies: none)" else .[] | "[baseline] state=\(.state) commit=\(.commit_id)\n\(.body)\n---" end'
  diag "NOTE: baseline shown above. An empty round diff does not mean nothing was found."
fi

# ---- Step 2: trigger (round 1: bare `@codex review`; re-review: body file) ----
# 罠 15 + 罠 18: mention は冒頭 1 回のみ、本文中で `@codex` を bare word でも
# 使わない、verb は `review` のみ (`@codex address` 等は verb ではない)。
# re-review の本文は heredoc ではなく **ファイルに書いて --body-file** で渡す。
# 罠 47: comment id を捕まえておく。reaction を見るのに要る — 「reaction 無し =
# 届いていない」が、無応答を stale connector と誤診しないための唯一の signal。
# **round ごとに取り直す**: 前 round の comment の reaction を見ていると、
# 届いていない round を「届いた」と読む。
if [ -n "$TRIGGER_BODY_FILE" ]; then
  TRIGGER_COMMENT_ID=$(post_trigger --body-file "$TRIGGER_BODY_FILE") || exit 6
else
  TRIGGER_COMMENT_ID=$(post_trigger --body "@codex review") || exit 6
fi
diag "trigger comment id=${TRIGGER_COMMENT_ID}"

# ---- Step 3: polling (Phase A = first activity, Phase B = quiet window) ----
ROUND_START=$(date +%s)

# 罠 13: count では edit / deletion を見落とすので `(id, updated_at)` の set で track。
# 罠 44: Phase A の判定は **生 JSON ではなく (id, updated_at) の射影**で行う。
# push すると GitHub が既存 inline の `line` を新しい diff に貼り直すので、
# codex が何も書いていなくても生 JSON は変わる (PR #162 round 4 で「収束・指摘 0 件」
# を誤報告)。表示に要るフィールド (罠 24) は snapshot に残したまま、比較のときだけ落とす。
key_inline() { snapshot_inline | jq -c '[.[] | {id, updated_at}]'; }
key_issues() { snapshot_issues | jq -c '[.[] | {id, updated_at}]'; }
key_reviews() { snapshot_reviews | jq -c '[.[] | {id, submitted_at}]'; }

# 罠 46: `eyes` は「受け取った / 着手した」、`+1` は「レビュー済み・指摘なし」。
# **種類で判定する** — login だけで数えると着手の合図が合格に見える。
reaction() {   # $1 = "+1" | "eyes"
  gh api "repos/${OWNER_REPO}/issues/comments/${TRIGGER_COMMENT_ID}/reactions" \
    --jq "[.[] | select(.user.login==\"${BOT}\" and .content==\"$1\")] | length" 2>/dev/null || echo 0
}

PREV_KI=$(key_inline); PREV_KS=$(key_issues); PREV_KR=$(key_reviews)

# Phase A: wait for first activity
while true; do
  ELAPSED=$(( $(date +%s) - ROUND_START ))
  if [ "$ELAPSED" -gt "$PER_ROUND_TIMEOUT" ]; then
    # 罠 47: reaction が 1 つも無い = **trigger が届いていない** (実測 8 回中 2 回、
    # PR #162 round 4 / 8)。stale connector ではないので、再 trigger すれば復帰する。
    # **`exit 3` で stale connector と診断する前に必ず reaction を見る。**
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
    # 罠 48: transient API failure は短いリストを返し、差分に見える。射影 (罠 44) は
    # 「変化の原因が観測対象でない」に効くが「**観測自体の失敗**」には効かない。
    # 10 秒後に取り直して **同じ差分が再現した時だけ** 信じる。
    sleep 10
    if [ "$(key_inline)" = "$CUR_KI" ] && [ "$(key_issues)" = "$CUR_KS" ] \
       && [ "$(key_reviews)" = "$CUR_KR" ]; then
      diag "=== codex initial activity detected after ${ELAPSED}s (confirmed twice) ==="
      break
    fi
    diag "  (diff did not reproduce = transient API result, continuing)"
  fi
  sleep 30
done

# Phase B: wait for quiet window.
# 罠 32 (codex P1 round 4 on PR #54): codex は review submission を post してから
# 秒〜数十秒遅れて inline comment を post する multi-write pattern。最初の delta で
# convergence 判定すると stale な inline state で false-converge する (= 後続の
# P0/P1 が見えない)。`QUIET_WINDOW_SEC` 秒間 snapshot が不変な状態を確認してから
# Step 4 へ進む。
# 罠 34: 窓幅を 1 回の観測から決めると、その観測が下限だった場合に**静かに取りこぼす**。
# Phase A と**同じ鍵**で比べる (罠 44)。ここだけ生 JSON にすると、push で `line` が
# 貼り直された瞬間に quiet window が永遠にリセットされ続ける。
QUIET_START=$(date +%s)
LAST_INLINE=$(key_inline); LAST_REVIEWS=$(key_reviews); LAST_ISSUES=$(key_issues)
while true; do
  WALL_ELAPSED=$(( $(date +%s) - ROUND_START ))
  if [ "$WALL_ELAPSED" -gt "$PER_ROUND_TIMEOUT" ]; then
    diag "WARN: quiet window not reached within ${PER_ROUND_TIMEOUT}s wall-clock. Proceeding to Step 4."
    break
  fi
  sleep 15
  CHECK_INLINE=$(key_inline); CHECK_REVIEWS=$(key_reviews); CHECK_ISSUES=$(key_issues)
  if [ "$CHECK_INLINE" = "$LAST_INLINE" ] && [ "$CHECK_REVIEWS" = "$LAST_REVIEWS" ] \
     && [ "$CHECK_ISSUES" = "$LAST_ISSUES" ]; then
    QUIET_ELAPSED=$(( $(date +%s) - QUIET_START ))
    if [ "$QUIET_ELAPSED" -ge "$QUIET_WINDOW_SEC" ]; then
      diag "=== quiet window of ${QUIET_WINDOW_SEC}s confirmed, round complete ==="
      break
    fi
  else
    QUIET_START=$(date +%s)
    LAST_INLINE=$CHECK_INLINE; LAST_REVIEWS=$CHECK_REVIEWS; LAST_ISSUES=$CHECK_ISSUES
    diag "  (still receiving codex writes, reset quiet window)"
  fi
done

# ---- Step 4: 3-layer convergence ----
# 罠 14 の文言依存を緩和: primary = review state (Layer 1)、secondary = P-badge
# absence (Layer 2)、tertiary = sentinel grep (Layer 3)。
#
# 罠 33 (codex P1 round 5 on PR #54): sentinel / terminal-error checks must be
# scoped to **current round** issue comments (= delta vs PREV_ISSUES), not the
# whole PR history. prior round で sentinel ("Didn't find any major issues") が
# 出ていた場合、current round が new review/inline を出して new issue comment が
# 無い state でも `LATEST_ISSUE_BODY` は prior round の sentinel を拾い続けて
# SENTINEL_MATCH=true → false-converge する。
LATEST_REVIEW=$(snapshot_reviews | jq '.[-1]')
CUR_ISSUES_FRESH=$(snapshot_issues)
NEW_ISSUES=$(jq -n --argjson prev "$PREV_ISSUES" --argjson cur "$CUR_ISSUES_FRESH" '
  ($prev | map({key: (.id|tostring), value: .updated_at}) | from_entries) as $prev_map |
  $cur | map(select(.id as $i | ($prev_map[$i|tostring] // null) != .updated_at))
')
# codex P1 round 9 (PR #54): **この round の comment は複数ありうる** (罠 32/34 が
# 前提にしている multi-write そのもの) ので、`.[-1]` だけ見ると先に来た方の
# sentinel / terminal error を落とす。特に terminal error を落とすと「retry しても
# 無駄」を retry し続ける。全部繋ぐ。
LATEST_ISSUE_BODY=$(echo "$NEW_ISSUES" | jq -r 'map(.body // "") | join("\n---\n")')
HEAD_SHA=$(gh api "repos/${OWNER_REPO}/pulls/${PR}" --jq .head.sha)

# 罠 10: error string detect → terminal failure、retry しない。
# 罠 56: **quota 切れも terminal**。実測 (PR #181 round 3): "You have reached your
# Codex usage limits for code reviews." が来た。語彙が既存 3 つと重ならないので、
# pattern に足さないと**素通りする**。retry しても復帰しない。
TERMINAL_ERROR_PATTERN="Something went wrong|Script exited|Try again later|usage limits|rate limit"
if echo "$LATEST_ISSUE_BODY" | grep -qE "$TERMINAL_ERROR_PATTERN"; then
  diag "ERROR: codex returned a terminal failure body (trap 10). Body follows on stdout."
  diag "Action: do not retry. Escalate; try again later or on another PR."
  # 罠 55: 本文は **codex が書いた内容** = この実行の結果であって診断ではない。
  # stderr に回すと、英語の marker に日本語が混じっていた瞬間に CP932 コンソールで
  # mojibake になる (codex P1 on PR #179)。**stdout に出す**。
  echo "$LATEST_ISSUE_BODY"
  exit 4
fi

# Layer 1 (primary): review state-base — submitted_at 存在 + state が valid な submission。
# 罠 21: codex は re-review で新 review submission を出さないことがある =
# `commit_id == HEAD_SHA` 縛りを外す。stale review の false-positive は Layer 3
# sentinel + Layer 2 P-badge + 罠 57 の ROUND_EMPTY で補正。
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

# Layer 2 (secondary): P-badge presence detection.
# 罠 22: codex inline の P-badge は `![P0 Badge](...)` の image markdown。`[P0]` text ではない。
# 罠 1 (jq escape): jq regex 内で `\[` は invalid escape、`contains()` で safe な substring match。
# 罠 27 (codex P1 round 2 on PR #54): `pulls/<N>/comments` は PR の全 history を返す。
#   round 2 以降に counter を全件で取ると、prior round で残っている P0/P1 が
#   resolved 状態でも count > 0 のままで、convergence が永久に false になる。
#   Step 1 で取った PREV_INLINE (= round baseline) との set diff を取り、
#   **当該 round で新規に追加された inline** だけを評価対象にする。
# 罠 29 (codex P2 round 3 on PR #54): id 単独の set diff は **edit を miss する** —
#   codex が既存 inline (= 同じ id) の body を update して P-badge を追加 / 昇格する
#   case で false-converge する。`(id, updated_at)` の compound key で diff を取る。
CUR_INLINE_FRESH=$(snapshot_inline)
NEW_INLINE=$(jq -n --argjson prev "$PREV_INLINE" --argjson cur "$CUR_INLINE_FRESH" '
  ($prev | map({key: (.id|tostring), value: .updated_at}) | from_entries) as $prev_map |
  $cur | map(select(.id as $i | ($prev_map[$i|tostring] // null) != .updated_at))
')
# 罠 49 (PR #164 round 7): codex は指摘を **review 本文**に書くこともある — inline の
# アンカーではなく permalink の形で。inline だけ数えると「activity あり / inline 0」が
# 「レビュー済み・指摘なし」と見分けられず、Layer 1 (state=COMMENTED) と Layer 3
# (sentinel 無し) も収束側に倒れるので **3 layer 全部を通り抜ける**。両方を数える。
# ...and **round-scoped**, like NEW_INLINE (罠 27) and NEW_ISSUES (罠 33): 罠 21 の
# とおり re-review で新 submission が出ない round があり、`LATEST_REVIEW` が前 round の
# ままだと修正済みの指摘を数え続けて収束しない (codex P2 round 8 on PR #164)。
NEW_REVIEWS=$(jq -n --argjson prev "$PREV_REVIEWS" --argjson cur "$(snapshot_reviews)" '
  ($prev | map({key: (.id|tostring), value: .submitted_at}) | from_entries) as $p |
  $cur | map(select(.id as $i | ($p[$i|tostring] // null) != .submitted_at))
')
# 罠 34 が記録しているとおり **1 round に review submission が複数来る**。`.[-1]` だけ
# 数えると、先の submission に P0/P1 があって最後のに badge が無い round で
# `P0_P1_TAGS_PRESENT=0` になり、**blocking な指摘を持ったまま収束**する
# (codex P1 round 8 on PR #164)。この round の body を全部繋いでから数える。
REVIEW_BODY=$(echo "$NEW_REVIEWS" | jq -r 'map(.body // "") | join("\n---\n")')
body_badges() { printf '%s' "$REVIEW_BODY" | grep -o "$1" | wc -l | tr -d ' '; }
P0_P1_TAGS_PRESENT=$(( $(echo "$NEW_INLINE" | jq '[.[] | .body | select(contains("![P0 Badge") or contains("![P1 Badge"))] | length') \
  + $(body_badges '!\[P0 Badge') + $(body_badges '!\[P1 Badge') ))
# 罠 17 / 罠 23: 公式 docs は P0/P1 only と書いているが実例で P2 も surface する。
P2_TAGS_PRESENT=$(( $(echo "$NEW_INLINE" | jq '[.[] | .body | select(contains("![P2 Badge"))] | length') \
  + $(body_badges '!\[P2 Badge') ))
# 罠 23 が P2 で一度広げた形が、P3 で再発した (PR #177): P0/P1/P2 だけ数えて
# `P2=0` を「指摘なし」と読みかけたが、来ていたのは中身の正しい **P3** だった。
# **badge の集合を列挙で決め打ちしない。** P3 も数え、Step 5 では badge の
# 有無を問わず round の inline を全部出す (= 未知の badge も人の目に入る)。
P3_TAGS_PRESENT=$(( $(echo "$NEW_INLINE" | jq '[.[] | .body | select(contains("![P3 Badge"))] | length') \
  + $(body_badges '!\[P3 Badge') ))

# 罠 57: **Layer 1 だけ round scope が掛かっていなかった。** 罠 27 (inline) /
# 罠 33 (issue) / 罠 49 (review body) で 3 つとも delta にしたのに、`STATE_OK` は
# `LATEST_REVIEW` = **PR 全 history の最後**から取っている。その round が
# **何も生まなかった**とき (quota 切れ・沈黙)、前 round の `state=COMMENTED` が
# 残っているので `STATE_OK=true` + badge 0 で **収束と読む**。
# 実測 (PR #181 round 3): 新しい commit を push して trigger した round の産物が
# 「quota に達した」の 1 コメントだけだったのに **CONVERGED (Layer 1 + 2)** を出した。
# **その round の産物が 3 endpoint とも 0 件なら、収束の証拠はどこにも無い。**
ROUND_EMPTY=false
if [ "$(echo "$NEW_INLINE" | jq 'length')" = "0" ] \
  && [ "$(echo "$NEW_REVIEWS" | jq 'length')" = "0" ] \
  && [ "$(echo "$NEW_ISSUES" | jq 'length')" = "0" ]; then
  ROUND_EMPTY=true
fi

# Layer 3 (tertiary): sentinel text variations (罠 14)、broader pattern set
SENTINEL_PATTERN="Didn't find any major issues|Hooray|Bravo|Looks good|Keep them coming|no issues found|All good|All clear|approved"
SENTINEL_MATCH=false
if echo "$LATEST_ISSUE_BODY" | grep -qiE "$SENTINEL_PATTERN"; then
  SENTINEL_MATCH=true
fi

echo "=== Step 4 summary ==="
echo "round=${ROUND}/${MAX_ROUNDS} first_invocation=${FIRST_INVOCATION} head=${HEAD_SHA} review_state=${REVIEW_STATE} commit_fresh=${COMMIT_FRESH} state_ok=${STATE_OK}"
echo "new_inline=$(echo "$NEW_INLINE" | jq 'length') new_reviews=$(echo "$NEW_REVIEWS" | jq 'length') new_issues=$(echo "$NEW_ISSUES" | jq 'length') round_empty=${ROUND_EMPTY}"
echo "P0P1=${P0_P1_TAGS_PRESENT} P2=${P2_TAGS_PRESENT} P3=${P3_TAGS_PRESENT} sentinel=${SENTINEL_MATCH}"

# 統合判定: Layer 3 sentinel **単独で converged OK**、Layer 1 / Layer 2 は補強情報。
# P0/P1 がある時だけ「未収束」。P2 / P3 は note に留め、controller が判断する。
CONVERGED=false
if [ "$P0_P1_TAGS_PRESENT" -gt 0 ]; then
  echo "WARN P0/P1 issues present (= ${P0_P1_TAGS_PRESENT} item(s)), fix needed"
elif [ "$SENTINEL_MATCH" = "true" ]; then
  EXTRA=""
  [ "$STATE_OK" = "true" ] && EXTRA+=" + Layer 1 state=${REVIEW_STATE}"
  [ "$COMMIT_FRESH" = "true" ] && EXTRA+=" + commit fresh"
  [ "$P2_TAGS_PRESENT" -gt 0 ] && EXTRA+=" (Note: ${P2_TAGS_PRESENT} P2 item(s), controller judgment)"
  [ "$P3_TAGS_PRESENT" -gt 0 ] && EXTRA+=" (Note: ${P3_TAGS_PRESENT} P3 item(s), same)"
  echo "CONVERGED (Layer 3 sentinel${EXTRA})"
  CONVERGED=true
elif [ "$ROUND_EMPTY" = "true" ]; then
  # 罠 57: この round は 3 endpoint とも 0 件。`STATE_OK` は前 round の残り香なので
  # 収束の根拠に使えない。**「指摘が無かった」ではなく「答えが無かった」**。
  echo "INDETERMINATE (this round produced nothing on any endpoint; state=${REVIEW_STATE} is left over from an earlier round, commit_fresh=${COMMIT_FRESH})"
elif [ "$STATE_OK" = "true" ] && [ "$P0_P1_TAGS_PRESENT" = "0" ] \
  && [ "$P2_TAGS_PRESENT" = "0" ] && [ "$P3_TAGS_PRESENT" = "0" ]; then
  echo "CONVERGED (Layer 1 + 2: state=${REVIEW_STATE}, no P-badges)"
  CONVERGED=true
else
  echo "INDETERMINATE (no sentinel + no clean state) - re-trigger or user escalate"
fi
echo "CONVERGED=${CONVERGED}"

# ---- Step 5: render ----
# 計数 (Step 4) と表示 (ここ) は **同じ NEW_* と同じ predicate** を使う (罠 25 / 罠 27 /
# 罠 49 の lockstep)。片方だけ直すと、counter と display が別の review を指す。
echo ""
echo "=== Top-level summary (review body, this round) ==="
# 罠 49: the body is not only a summary - a finding can live here, as a
# permalink instead of an inline anchor. Step 4 counts it, so this prints it.
echo "$NEW_REVIEWS" | jq -r 'if length == 0 then "(no new review body this round)" else .[] | .body // "" end'
echo ""
echo "=== Inline P0/P1 (review-blocking issues, current round only) ==="
# 罠 25 (codex P1 on PR #54): codex emits image markdown badges like
# `![P0 Badge](...)` / `![P1 Badge](...)`, NOT bare `[P0]` text. Filtering with
# `test("\\[P[01]\\]")` would silently drop every actionable finding. Use the
# same `contains("![P0 Badge")` / `contains("![P1 Badge")` predicate as Layer 2.
echo "$NEW_INLINE" | jq -r '.[] | select(.body | (contains("![P0 Badge") or contains("![P1 Badge"))) | "[\(.updated_at)] \(.path):\(.line // .original_line)\n\(.body)\n---"' 2>/dev/null
echo ""
echo "=== Inline P2 (controller-judgment items, current round only) ==="
# 罠 30 (codex P2 round 3 on PR #54): P2 だけの round で本 section が空だと
# controller は P-badge カウントを見て convergence 判定するが「具体的に何の
# P2 を取り込むか / skip するか」を決める material が無い。Step 4 が
# P2_TAGS_PRESENT > 0 を note した時、必ずここに該当 P2 の path + body を出す。
echo "$NEW_INLINE" | jq -r '.[] | select(.body | contains("![P2 Badge")) | "[\(.updated_at)] \(.path):\(.line // .original_line)\n\(.body)\n---"' 2>/dev/null
echo ""
echo "=== Inline, this round - ALL of them, badge or not ==="
# 罠 23 が P2 で、その再発が P3 で起きた (PR #177): **列挙した badge の外に指摘が
# 来る**。上の 2 section は列挙に依存しているので、ここは列挙せず **その round の
# inline を全部出す**。次に codex が P4 を作っても、あるいは badge の無い指摘を
# 書いても、人の目には入る。
echo "$NEW_INLINE" | jq -r 'if length == 0 then "(none this round)" else .[] | "[\(.updated_at)] \(.path):\(.line // .original_line)\n\(.body)\n---" end'
echo ""
echo "=== Top-level issue comments by codex (full) ==="
snapshot_issues | jq -r '.[] | "[\(.updated_at)] \(.body)"'
exit 0
