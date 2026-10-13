#!/usr/bin/env bash
# One round of /codex-review: trigger `@codex review` on a PR, poll the three
# endpoints codex writes to (inline / reviews / issue comments), decide
# convergence with three layers, and print the round's findings.
#
# Usage:
#   codex_review_round.sh [-R owner/repo] [--dry-run] <PR#> [max_rounds] [per_round_timeout_sec] [trigger_body_file]
#     -R / --repo owner/repo the GitHub repo the PR lives in. Omitted: $GH_REPO if set,
#                            else the repo this script is checked out in (unchanged default).
#                            Every read, the round count and the post all use this one repo
#                            (kuriya trap #409 / #238)
#     --dry-run              do every read (repo, PR, baselines, trigger history, round number),
#                            print them on stdout, and exit 0 before posting anything
#     max_rounds             default 3; the round number is derived from the PR's own
#                            `@codex review` history, so the limit holds across processes
#     per_round_timeout_sec  default 600 (wall-clock for Phase A + Phase B)
#     trigger_body_file      re-review body (`@codex review` + fix sketch); omit for round 1.
#                            A readable regular file, or `-` for stdin. It is copied once into
#                            a private snapshot; the snapshot must be non-empty and its first
#                            line must start with `@codex review`, else exit 2 before any
#                            GitHub read. The snapshot (not the original path) is what is posted
#
# Exit codes:
#   0  round complete - read the `CONVERGED=true|false` line on stdout
#   2  bad arguments (including -R and $GH_REPO naming different repos, and a
#      trigger_body_file that is missing / not a regular file / unreadable / empty
#      (an empty stdin for `-` included) / whose first line does not start with
#      `@codex review`); checked before any GitHub read, --dry-run included; nothing
#      was posted. Failing to create the temporary directory (where the body snapshot
#      lives) is exit 8, as before, also before any GitHub read
#   3  codex acknowledged (reaction present) but did not answer within timeout (trap 9)
#   4  codex returned a terminal error body; retry will not help (trap 10 / 56)
#  10  codex could not see the PR head yet ("Provided git ref ... does not exist"); re-trigger once (trap 60)
#   5  trigger comment got no reaction = codex never received it (trap 47)
#   6  trigger POST failed 3 times; do not wait, re-run later (trap 50)
#   7  round limit reached; nothing was posted (trap 16 / 28)
#   8  a read before posting failed, or a value built from it (count, round number,
#      head, URL, baseline text) failed or is not the expected shape; nothing was posted
#   9  a round delta could not be computed (jq failed); the round's findings are
#      unknown, so nothing is reported as converged (trap 59); or a GitHub read after
#      the trigger failed / came back empty (reactions at timeout, the final reviews /
#      issue comments / inline comments / head sha), or a count derived from them is not
#      a number. No verdict is printed. The trigger is posted in every case; for the
#      read failures do not re-trigger automatically (stderr says which one)
#
# Output contract (AGENTS.md "Results go to stdout, diagnostics to stderr"):
#   stdout = the review itself (baseline on first invocation, Step 4 summary, Step 5 render)
#   stderr = progress / warnings, ASCII only (trap 52) - never bot-written text (trap 55)
#
# Every trap this script guards against is a comment next to the code that
# guards it. The numbers refer to .dev/knowledge/codex-review-loop-pitfalls.md.
# To list them:  grep -oE '罠 [0-9]+' "$0" | sort -u
# The count is deliberately not written anywhere: a copied number rots.
#
# pipefail: every snapshot is `gh api ... | jq -s ...`. Without it a failed
# `gh api` leaves jq with empty input, jq prints `[]` / `0`, and the pipeline
# "succeeds" with a baseline that says the PR is empty (codex P2 round 2 on
# PR #222). With it the callers below can `|| abort_unreadable`.
set -u -o pipefail

usage() {
  printf 'usage: %s [-R owner/repo] [--dry-run] <PR#> [max_rounds] [per_round_timeout_sec] [trigger_body_file]\n' "$0" >&2
  exit 2
}
REPO_ARG=""
DRY_RUN=false
while [ $# -gt 0 ]; do
  case "$1" in
    # 値が `-` で始まる (`-R --dry-run 356`) なら option の取り違え。gh に flag として渡さず usage で止める。
    -R|--repo) [ $# -ge 2 ] || usage; REPO_ARG="$2"; shift 2
               case "$REPO_ARG" in ''|-*) usage;; esac;;
    --repo=*)  REPO_ARG="${1#--repo=}"; shift
               case "$REPO_ARG" in ''|-*) usage;; esac;;
    --dry-run) DRY_RUN=true; shift;;
    --) shift; break;;
    -*) usage;;
    *) break;;
  esac
done
[ $# -ge 1 ] && [ $# -le 4 ] || usage
PR="$1"
MAX_ROUNDS="${2:-3}"            # 罠 16: cost-aware (25 credits x round)。/feature-flow は 5 を渡す (罠 28)
PER_ROUND_TIMEOUT="${3:-600}"   # 罠 9: wall-clock timeout で stale connector を検知
TRIGGER_BODY_FILE="${4:-}"
case "$PR" in ''|*[!0-9]*) usage;; esac
case "$MAX_ROUNDS$PER_ROUND_TIMEOUT" in
  *[!0-9]*|'') usage;;
esac
# trigger_body_file は引数の段階で検査する (repo 解決・GitHub の読み取りより前、--dry-run でも)。
# 検査しないと、存在しない / 読めない / 空の file が Step 2 の `gh pr comment --body-file` の
# 失敗として post_trigger の 20 秒 x 3 回 retry に入り、exit 6 (「後で再実行」= 罠 50) になる。
# 原因は引数なので、待たせずに exit 2 で止める。何も投稿しない。
# 本文が `@codex review` で始まらないものも同じ exit 2 (罠 15 / 18): mention の無い comment は
# codex に届かず、投稿して 1 round を数えた上で exit 5 (罠 47) と誤診する。
# 検査した bytes と投稿する bytes を同一にする (PR #358 のローカル Codex、high: TOCTOU)。
# 元の path を検査して後で gh に読ませると、その間に削除されれば exit 6 が再発し、差し替えられれば
# mention の無い本文を投稿して 1 round を消費する。そこで本文を **1 回だけ** ROUND_TMP の中の
# snapshot へ copy し、検査も投稿 (Step 2 の --body-file) も snapshot に対して行う。
# copy は下の cd より前なので、相対 path は呼び出し側の cwd 基準のまま読める (絶対化は不要)。
# `-` は gh pr comment --body-file と同じく stdin (PR #358 のローカル Codex、medium)。
# stdin も snapshot に読み込んで同じ検査にかける。

# 進捗と警告は stderr、ASCII のみ (罠 52: AGENTS.md "Results go to stdout,
# diagnostics to stderr"、および CP932 コンソールでの mojibake 回避)。この script の
# *結果* は review の中身なので、それだけが stdout に出る。
diag() { printf '%s\n' "$*" >&2; }

# 罠 59: snapshot は **ファイル経由で** jq に渡す (下の json_delta)。置き場は
# `mktemp -d` の絶対パス — この script は repo root へ cd しているので、相対パスで
# 書くと作業ツリーに残骸を落とす。EXIT trap で必ず消すので、どの exit code で
# 抜けても残らない。trigger_body_file の snapshot も同じ dir に置く (trap は 1 本だけ)。
# trap は mktemp より **前に** 張る (PR #358 のローカル Codex、medium): 作成から trap までの間
# (下の絶対 path 解決を含む) に中断されても dir が残らないように。ROUND_TMP が空の間は何もしない。
# INT / TERM / HUP は別に trap しない: Git Bash (bash 5) は signal で終わる時も EXIT trap を
# 実行することを実測した (PR #358 の作業中、3 signal とも marker file が書かれた)。
cleanup_round_tmp() { if [ -n "$ROUND_TMP" ]; then rm -rf "$ROUND_TMP"; fi; }
ROUND_TMP=""
trap cleanup_round_tmp EXIT
ROUND_TMP=$(mktemp -d) || { diag "ABORT: could not create a temporary directory."; exit 8; }
# 呼び出し側の TMPDIR が相対 (例 `TMPDIR=tmp`) だと mktemp は相対 path を返す。下で repo root へ
# cd した後に snapshot / json / EXIT trap が別の場所を指さないよう、cd 前に絶対 path へ解決する
# (PR #358 の codex r1、P2)。`$( )` の中の cd なので、この script の cwd は動かない。
# 解決できなければ exit 8 で抜け、生の (cd 前なので有効な) path の削除は trap に任せる。
ROUND_TMP_ABS=$(cd "$ROUND_TMP" && pwd) || {
  diag "ABORT: could not resolve the temporary directory ${ROUND_TMP}."; exit 8; }
ROUND_TMP=$ROUND_TMP_ABS   # 差し替えの前後どちらでも ROUND_TMP は同じ実在 dir を指す

bad_body_file() {   # $1 = 理由
  diag "ABORT: trigger_body_file ${TRIGGER_BODY_FILE}: $1. Nothing was posted."
  diag "Action: write the re-review body (first line starts with @codex review) to a file and pass its path (or - for stdin)."
  exit 2
}
TRIGGER_BODY_SNAPSHOT=""
if [ -n "$TRIGGER_BODY_FILE" ]; then
  TRIGGER_BODY_SNAPSHOT="${ROUND_TMP}/trigger-body.snapshot"
  # cat の stderr は locale 依存で非 ASCII になり得るので捨て、理由は bad_body_file の ASCII 行で出す (罠 52)。
  if [ "$TRIGGER_BODY_FILE" = "-" ]; then
    cat > "$TRIGGER_BODY_SNAPSHOT" 2>/dev/null || bad_body_file "could not read stdin"
  else
    [ -e "$TRIGGER_BODY_FILE" ] || bad_body_file "does not exist"
    [ -f "$TRIGGER_BODY_FILE" ] || bad_body_file "is not a regular file"
    [ -r "$TRIGGER_BODY_FILE" ] || bad_body_file "is not readable"
    cat -- "$TRIGGER_BODY_FILE" > "$TRIGGER_BODY_SNAPSHOT" 2>/dev/null || bad_body_file "could not be copied"
  fi
  # 以下は snapshot だけを見る (元の file はもう読まない)。
  [ -s "$TRIGGER_BODY_SNAPSHOT" ] || bad_body_file "is empty"
  BODY_FIRST_LINE=""
  IFS= read -r BODY_FIRST_LINE < "$TRIGGER_BODY_SNAPSHOT" || [ -n "$BODY_FIRST_LINE" ] \
    || bad_body_file "could not read its first line"
  case "$BODY_FIRST_LINE" in
    '@codex review'|'@codex review'[[:space:]]*) ;;   # [[:space:]] は CRLF の \r も含む
    *) bad_body_file "the first line does not start with @codex review (trap 15)";;
  esac
fi
QUIET_WINDOW_SEC=180            # 罠 34: 同一 commit への 2 本目を 144 秒後に観測、+ 安全率
BOT="chatgpt-codex-connector[bot]"   # 罠 11: login 完全一致で filter
# kuriya trap #410: codex は PR に 1 本の「Codex Review Summary」issue comment を持ち、
# round ごとに Running → Completed と edit する (2026-10-06〜)。中身は進捗の表だけで指摘も
# sentinel も無い。1 行目全体が `<!-- ${CODEX_SUMMARY_MARKER} -->` の comment を snapshot_issues で除外する (下)。
CODEX_SUMMARY_MARKER="codex-pull-request-review-summary"

# The script lives at <repo>/.claude/skills/codex-review/scripts/; resolve the
# repo from there so it works from any cwd and any checkout location.
SCRIPT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
REPO_DIR=$(git -C "$SCRIPT_DIR" rev-parse --show-toplevel) || exit 1
cd "$REPO_DIR" || exit 1

# diag と ROUND_TMP (一時 dir + EXIT trap) は上、引数の検査の直前で作っている
# (trigger_body_file の snapshot を同じ dir に置くため)。

# 投稿 *前* の読み取り (repo 名 / baseline / trigger 履歴) が失敗したら、投稿せずに
# 止める。読めなかった結果を「空」として先へ進むと、baseline が空 = 既存の指摘が全部
# 「この round の新規」に見え、trigger 履歴が 0 = round 1 と誤認して max_rounds を
# 素通りする (codex P2 round 2 on PR #222)。polling 中の一時失敗はここでは扱わない —
# そちらは罠 48 の二重確認が吸収する (投稿後に abort すると 1 round 分の credit が消える)。
# top level から呼ぶこと: `$( )` の中では `exit` がサブシェルしか終わらせない (罠 54)。
# 対象は GitHub の読み取りだけでなく、**それを元に jq で作る値** (件数・本文・round 数・head・URL)
# も同じ (PR #356 のローカル Codex round 3、high / medium): page を足す jq が落ちて round 数が空に
# なると `$(( + 1 ))` = round 1 で上限を素通りし、dry-run の件数が空でも exit 0 になっていた。
# 呼び出し点ごとの表は PR #356 の本文 ("Reads before the trigger")。
abort_unreadable() {
  diag "ABORT: could not read $1 (gh api or jq failed). Nothing was posted."
  diag "Action: check gh auth status / rate limit, then re-run."
  exit 8
}
# 数であるべき値の検査 (投稿前の版)。投稿後の require_count と同じ形で、出口だけ exit 8。
require_count_before_trigger() {   # $1 = 値, $2 = 何の数か
  case "$1" in ''|*[!0-9]*) abort_unreadable "$2 (got '${1}')";; esac
}

# ---- which repo (kuriya trap #409 / #238) ----
# 不変条件: **投稿 (gh pr comment)・読み取り (gh api repos/...)・round の数え方が
# すべて同じ 1 つの repo を見る。** #409 (2026-10-13): `GH_REPO=<別 repo>` で打つと
# `gh pr comment` は GH_REPO に従って別 repo の PR に投稿したが、引数なしの
# `gh repo view` は GH_REPO を見ず cwd の repo (= この script の checkout) を返したので、
# baseline / 履歴 / polling は **同じ番号の別の PR** を読み「baseline none」「600s 無応答」を出した。
# 決め方は 1 か所: `-R` > `$GH_REPO` > checkout の repo。`-R` と `$GH_REPO` が両方あって
# 食い違えば、どちらを信じるか推測せず exit 2。決めた repo は `GH_REPO` に export し直し、
# `gh pr comment` にも `-R` で明示する (env と引数の二重化で、片方の取りこぼしを残さない)。
# `gh repo view <repo>` で正規化する: 大文字小文字・`github.com/` 付きの形を nameWithOwner に揃え、
# 存在しない repo はここで止まる。github.com 以外の host は受けない: `gh api` の行き先は
# GH_HOST で決まるので、呼び出し側の GH_HOST が別 host を指していると、下の host 判定
# (url が github.com) と実際の読み書き先がずれ得る。**解決の前に GH_HOST を github.com に固定**し、
# 以後の gh 呼び出しをすべて github.com に向ける。
export GH_HOST=github.com
resolve_repo() {   # $1 = repo 指定 (空なら checkout の repo)。stdout に "nameWithOwner url"
  if [ -n "$1" ]; then
    gh repo view "$1" --json nameWithOwner,url --jq '.nameWithOwner + " " + .url'
  else
    env -u GH_REPO gh repo view --json nameWithOwner,url --jq '.nameWithOwner + " " + .url'
  fi
}
if [ -n "$REPO_ARG" ] && [ -n "${GH_REPO:-}" ]; then
  ARG_RESOLVED=$(resolve_repo "$REPO_ARG") || abort_unreadable "the repository named by -R (${REPO_ARG})"
  ENV_RESOLVED=$(resolve_repo "$GH_REPO") || abort_unreadable "the repository named by GH_REPO (${GH_REPO})"
  if [ "${ARG_RESOLVED%% *}" != "${ENV_RESOLVED%% *}" ]; then
    diag "ABORT: -R ${REPO_ARG} and GH_REPO=${GH_REPO} name different repositories. Nothing was posted."
    diag "Action: drop one of them (unset GH_REPO, or pass the same repo to -R)."
    exit 2
  fi
  REPO_RESOLVED=$ARG_RESOLVED
else
  REPO_RESOLVED=$(resolve_repo "${REPO_ARG:-${GH_REPO:-}}") || abort_unreadable "the repository name"
fi
# 成功して出力が空 / 形が違う時は「読めなかった」(exit 8)。そのまま分解すると OWNER_REPO が空になり、
# 下の host 判定が exit 2 (引数の誤り) を出して原因を取り違える。
case "$REPO_RESOLVED" in
  ?*/?*' https://'?*) ;;
  *) abort_unreadable "the repository name (got '${REPO_RESOLVED}')";;
esac
OWNER_REPO=${REPO_RESOLVED%% *}
case "${REPO_RESOLVED#* }" in
  https://github.com/*) ;;
  *) diag "ABORT: ${OWNER_REPO} is not on github.com (url: ${REPO_RESOLVED#* }); only github.com is supported."
     exit 2;;
esac
export GH_REPO="$OWNER_REPO"
# PR がその repo に実在することを投稿前に確かめる (番号が issue / 別 repo の PR を指していたら止める)。
PR_URL=$(gh api "repos/${OWNER_REPO}/pulls/${PR}" --jq .html_url) || abort_unreadable "PR #${PR} in ${OWNER_REPO}"
case "$PR_URL" in https://?*) ;; *) abort_unreadable "the URL of PR #${PR} in ${OWNER_REPO} (got '${PR_URL}')";; esac
diag "repo ${OWNER_REPO} / PR ${PR_URL}"

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
# kuriya trap #410: Review Summary comment (CODEX_SUMMARY_MARKER) は **ここで 1 回だけ** 落とす。
#   残すと Running への edit が (id, updated_at) の差分になり、① Phase A が結果より先に抜けて
#   quiet window が早く満了する (PR #356 r1 で INDETERMINATE)、② 毎 round NEW_ISSUES に入って
#   ROUND_EMPTY が true にならない (罠 57 の安全装置が死ぬ)。key / delta / sentinel /
#   ROUND_EMPTY / 表示はすべてこの関数を通るので、ここで揃う (罠 53)。
#   判定は **本文の 1 行目全体が marker の HTML コメント**のときだけ: 前後に許すのは横の空白
#   ([[:blank:]]) だけで、`-->` の後は改行か本文の終わり。`contains`・行末を見ない形・先頭の改行を
#   飛ばす形 ([[:space:]]) だと、marker を引用した本物の指摘 (この script 自身の review) まで落とす
#   (ローカル Codex r1 / r2 / r3)。観察した summary は 1 行目がこの行そのもの (comment 6147442307)。
#   escape は 3 段 (bash の二重引用符 → jq 文字列 → Oniguruma): `\r` `\n` は bash を素通りして
#   jq が CR / LF にし、`\$` は bash が `$` にする。`\s` を避けて POSIX class を使うのも同じ理由。
snapshot_issues() {
  gh api --paginate "repos/${OWNER_REPO}/issues/${PR}/comments?per_page=100" \
    --jq "[.[] | select(.user.login==\"${BOT}\") | select((.body // \"\") | test(\"^[[:blank:]]*<!-- ${CODEX_SUMMARY_MARKER} -->[[:blank:]]*(\r?\n|\$)\") | not) | {id, updated_at, body}]" \
    | jq -s "add // [] | sort_by(.id)"
}

# ---- round delta (罠 29 の compound key を、罠 59 を踏まない渡し方で) ----
# 罠 29: id 単独の set diff は **edit を miss する** — codex が既存 comment の body を
#   update して P-badge を足す case で false-converge する。`(id, <timestamp>)` で取る。
# 罠 59 (PR #247 round 11): その diff を `jq -n --argjson prev "$PREV" --argjson cur "$CUR"`
#   で書くと、**snapshot は comment 本文を含むので argv がすぐ上限を超える**。実測: PR #247 の
#   inline snapshot 1 本が 16,532 bytes、prev + cur で 33,064 bytes で
#   `jq: Argument list too long` (exit 126)、`$( )` は **空文字列**を返す
#   (via: `jq -n --argjson prev "$(cat inline.json)" --argjson cur "$(cat inline.json)" '...'`)。
#   空文字列は下流で「指摘 0 件」と見分けが付かない (`echo '' | jq length` は何も出さず、
#   `$(( + 0 + 0 ))` は 0 になる) ので、**落ちたのに収束側に倒れる**。
#   ファイル経由 (`--slurpfile`) にすれば argv に本文が乗らない。
#   同じ形が 3 endpoint 分あったので、**関数 1 つに寄せる** (罠 53: 同じ操作を複数箇所に
#   書くと片方だけ直し忘れる)。
json_delta() {   # $1 = prev の JSON file, $2 = cur の JSON file, $3 = 比較する timestamp field
  jq -n --slurpfile prev "$1" --slurpfile cur "$2" --arg ts "$3" '
    ($prev[0] | map({key: (.id|tostring), value: .[$ts]}) | from_entries) as $prev_map |
    $cur[0] | map(select(.id as $i | ($prev_map[$i|tostring] // null) != .[$ts]))
  '
}

# 罠 59 の二次被害を止める: **空文字列は「指摘なし」ではなく「読めなかった」**。
# 罠 48 が「空欄を指摘なしと読むな」を人間の手順として書いていたものを、ここで機械化する。
# ここまで来ると trigger は投稿済みなので credit は既に消えている — それでも
# 「収束した」と報告するより、読めなかったと言って終わる方が安い。
require_delta() {   # $1 = 値, $2 = 何の delta か
  case "$1" in
    '') diag "ABORT: ${2} could not be computed (jq failed, trap 59)."
        diag "Action: re-run this round (the trigger is already posted; a re-run posts a new one and counts as a round). Do NOT read the empty list as 'no findings'."
        exit 9;;
  esac
}

# trigger 投稿 *後* の読み取りの失敗 (PR #356 のローカル Codex、high / medium x2)。
# 不変条件: **trigger 後の GitHub の読み取りが 1 つでも失敗 / 空なら、判定 (CONVERGED / WARN /
# exit 3/4/5/10 / exit 0) を出さずに exit 9。** 投稿前の abort_unreadable (exit 8) と違い、
# credit は既に消えている。読めなかった値を `[]` / 0 / 空として先へ進むと、sentinel 1 つで
# CONVERGED になる・P0/P1 を 0 と数える・届いた trigger を「届いていない」と読む。
# 自動で再 trigger させない: 届いていた trigger に重ねると 1 round を捨てる。
# 対象の一覧 (呼び出し点ごとの表) は PR #356 の本文。poll 中 (Phase A / B) の一時失敗は対象外 —
# 次の poll で取り直し、判定に使う値は Step 4 で取り直してここを通す。
# top level から呼ぶこと (罠 54)。
abort_after_trigger() {   # $1 = 何が読めなかったか
  diag "ABORT: could not read ${1} after the trigger was posted (gh api or jq failed). The round's result is unknown."
  diag "The trigger (comment id=${TRIGGER_COMMENT_ID:-unknown}) is already posted. Do NOT re-trigger automatically: a re-run posts again and uses a round."
  diag "Action: read the 3 endpoints by hand (gh api repos/${OWNER_REPO}/pulls/${PR}/comments, .../pulls/${PR}/reviews, repos/${OWNER_REPO}/issues/${PR}/comments) and the trigger's reactions, then decide or escalate. Do NOT read an empty list as 'no findings'."
  exit 9
}
# 判定に使う数は、jq が落ちると空になり `[ "" -gt 0 ]` は偽、`$(( + 0 ))` は 0 で、どちらも
# 収束側に倒れる (罠 59 と同じ形)。数字でなければ読めなかった扱い。
require_count() {   # $1 = 値, $2 = 何の数か
  case "$1" in ''|*[!0-9]*) abort_after_trigger "$2";; esac
}

# snapshot を delta 用のファイルに落とす。`printf '%s'` は末尾に改行を足さない —
# jq は JSON stream として読むのでどちらでも通るが、余計な差を作らない。
stash_json() {   # $1 = JSON 文字列, $2 = ファイル名 (ROUND_TMP からの相対)
  printf '%s' "$1" > "${ROUND_TMP}/$2"
  printf '%s' "${ROUND_TMP}/$2"
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
    url=$(gh pr comment "$PR" -R "$OWNER_REPO" "$@") && break   # trap #409: 読み取りと同じ repo
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
PREV_INLINE=$(snapshot_inline) || abort_unreadable "the inline-comment baseline"
PREV_REVIEWS=$(snapshot_reviews) || abort_unreadable "the review baseline"
PREV_ISSUES=$(snapshot_issues) || abort_unreadable "the issue-comment baseline"

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
# 読めなかった履歴を `add // 0` で 0 にしない — 取得と集計を分け、取得の失敗で止める
# (codex P2 round 2 on PR #222: 失敗 = 0 = round 1 で、上限を素通りして投稿していた)。
TRIGGER_HISTORY=$(gh api --paginate "repos/${OWNER_REPO}/issues/${PR}/comments?per_page=100" \
  --jq '[.[] | select(.body | startswith("@codex review"))] | length') || abort_unreadable "the trigger history"
# 集計の jq も同じ: 落ちると空 → `$(( + 1 ))` = round 1 で上限を素通りする (PR #356 ローカル Codex、high)。
PRIOR_TRIGGERS=$(printf '%s\n' "$TRIGGER_HISTORY" | jq -s 'add // 0') || abort_unreadable "the trigger count"
require_count_before_trigger "$PRIOR_TRIGGERS" "the trigger count"
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

# --dry-run: 投稿を伴わずに「どの repo のどの PR を読み、何 round 目と数えたか」を確かめる口。
# 上限判定 (exit 7) より前に置く — 上限に達した PR でも読み取りの正しさは確かめられるように。
# 投稿しないので round を消費しない。結果なので stdout。
# head の取得は top level で受ける: echo の中の `$( )` だと失敗しても空のまま exit 0 になり、
# 「読めた」と「読めなかった」が見分けられない (罠 54 と同じ形)。
if [ "$DRY_RUN" = "true" ]; then
  # 出す値は全部先に作って検査し、成功の行は検査が全部通ってから出す (件数の jq が落ちても
  # exit 0 で空の件数を出していた。PR #356 ローカル Codex、medium)。
  DRY_HEAD=$(gh api "repos/${OWNER_REPO}/pulls/${PR}" --jq .head.sha) || abort_unreadable "the head sha"
  case "$DRY_HEAD" in ''|*[!0-9a-f]*) abort_unreadable "the head sha (got '${DRY_HEAD}')";; esac
  N_BASE_INLINE=$(printf '%s' "$PREV_INLINE" | jq length) || abort_unreadable "the inline-comment baseline count"
  N_BASE_REVIEWS=$(printf '%s' "$PREV_REVIEWS" | jq length) || abort_unreadable "the review baseline count"
  N_BASE_ISSUES=$(printf '%s' "$PREV_ISSUES" | jq length) || abort_unreadable "the issue-comment baseline count"
  require_count_before_trigger "$N_BASE_INLINE" "the inline-comment baseline count"
  require_count_before_trigger "$N_BASE_REVIEWS" "the review baseline count"
  require_count_before_trigger "$N_BASE_ISSUES" "the issue-comment baseline count"
  echo "DRY_RUN repo=${OWNER_REPO} pr=${PR} url=${PR_URL}"
  echo "head=${DRY_HEAD} prior_triggers=${PRIOR_TRIGGERS} round=${ROUND}/${MAX_ROUNDS} would_stop_at_limit=$([ "$ROUND" -gt "$MAX_ROUNDS" ] && echo true || echo false) first_invocation=${FIRST_INVOCATION}"
  echo "baseline_inline=${N_BASE_INLINE} baseline_reviews=${N_BASE_REVIEWS} baseline_issues=${N_BASE_ISSUES}"
  diag "DRY RUN: nothing was posted."
  exit 0
fi
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
# 整形は変数で受けて検査してから出す: jq が落ちたまま投稿へ進むと、次の run は
# `PRIOR_TRIGGERS > 0` で初回ではなくなり、**この baseline は二度と出ない** (PR #356 の表)。
if [ "$FIRST_INVOCATION" = "true" ]; then
  BASE_INLINE_TEXT=$(printf '%s' "$PREV_INLINE" | jq -r 'if length == 0 then "(baseline inline: none)" else .[] | "[baseline] \(.path):\(.line // .original_line)\n\(.body)\n---" end') \
    || abort_unreadable "the inline-comment baseline (rendering)"
  BASE_REVIEWS_TEXT=$(printf '%s' "$PREV_REVIEWS" | jq -r 'if length == 0 then "(baseline review bodies: none)" else .[] | "[baseline] state=\(.state) commit=\(.commit_id)\n\(.body)\n---" end') \
    || abort_unreadable "the review baseline (rendering)"
  echo "=== Baseline before this run - the review GitHub ran when the PR opened (trap 51) ==="
  printf '%s\n' "$BASE_INLINE_TEXT"
  printf '%s\n' "$BASE_REVIEWS_TEXT"
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
# body file は引数の段階で取った snapshot を渡す (検査した bytes = 投稿する bytes、上の TOCTOU の節)。
if [ -n "$TRIGGER_BODY_SNAPSHOT" ]; then
  TRIGGER_COMMENT_ID=$(post_trigger --body-file "$TRIGGER_BODY_SNAPSHOT") || exit 6
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
# 罠 8 / 31 と同じく `--paginate` + `?per_page=100` で全 page を読み、外側の `jq -s` で 1 つの
# array に合わせてから数える (既定の 1 page = 30 件だけでは bot の reaction を取りこぼし得る)。
# **読めなかったことを 0 件にしない** (PR #356 のローカル Codex、medium): 失敗を 0 に変えると
# transient / rate limit / 権限の失敗が exit 5 (= 届いていない) になり、controller が同じ commit に
# 再 trigger して 1 round を捨てる。失敗は戻り値で返し、呼び出し側 (top level、罠 54) で exit 9 に落とす。
read_bot_reactions() {   # stdout = bot が付けた reaction の content の JSON array
  local out
  out=$(gh api --paginate "repos/${OWNER_REPO}/issues/comments/${TRIGGER_COMMENT_ID}/reactions?per_page=100" \
    --jq "[.[] | select(.user.login==\"${BOT}\") | .content]" | jq -sc 'add // []') || return 1
  case "$out" in '['*']') printf '%s\n' "$out";; *) return 1;; esac
}

PREV_KI=$(key_inline); PREV_KS=$(key_issues); PREV_KR=$(key_reviews)

# Phase A: wait for first activity
while true; do
  ELAPSED=$(( $(date +%s) - ROUND_START ))
  if [ "$ELAPSED" -gt "$PER_ROUND_TIMEOUT" ]; then
    # 罠 47: reaction が 1 つも無い = **trigger が届いていない** (実測 8 回中 2 回、
    # PR #162 round 4 / 8)。stale connector ではないので、再 trigger すれば復帰する。
    # **`exit 3` で stale connector と診断する前に必ず reaction を見る。**
    BOT_REACTIONS=$(read_bot_reactions) \
      || abort_after_trigger "the trigger comment's reactions (acknowledgement is unknown)"
    ACK_COUNT=$(printf '%s' "$BOT_REACTIONS" | jq '[.[] | select(. == "eyes" or . == "+1")] | length')
    require_count "$ACK_COUNT" "the count of codex's eyes / +1 reactions"
    if [ "$ACK_COUNT" = "0" ]; then
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
# 判定に使う 3 endpoint と head sha は **ここで 1 回ずつ読み、戻り値を見る** (abort_after_trigger)。
# 以前は reviews を 2 回 (Layer 1 用と delta 用) 別々に読み、どちらも戻り値を捨てていたので、
# 失敗が `[]` になって sentinel 1 つで CONVERGED になり得た (PR #356 のローカル Codex、high)。
# 1 回にまとめたので、Layer 1 の state と review 本文の delta が同じ snapshot を指す。
CUR_REVIEWS_FRESH=$(snapshot_reviews) || abort_after_trigger "the reviews"
CUR_ISSUES_FRESH=$(snapshot_issues) || abort_after_trigger "the issue comments"
CUR_INLINE_FRESH=$(snapshot_inline) || abort_after_trigger "the inline comments"
HEAD_SHA=$(gh api "repos/${OWNER_REPO}/pulls/${PR}" --jq .head.sha) || abort_after_trigger "the head sha"
case "$HEAD_SHA" in ''|*[!0-9a-f]*) abort_after_trigger "the head sha (got '${HEAD_SHA}')";; esac
LATEST_REVIEW=$(printf '%s' "$CUR_REVIEWS_FRESH" | jq '.[-1]') || abort_after_trigger "the latest review"
NEW_ISSUES=$(json_delta "$(stash_json "$PREV_ISSUES" prev_issues.json)" \
                        "$(stash_json "$CUR_ISSUES_FRESH" cur_issues.json)" updated_at)
require_delta "$NEW_ISSUES" "the issue-comment delta"
# codex P1 round 9 (PR #54): **この round の comment は複数ありうる** (罠 32/34 が
# 前提にしている multi-write そのもの) ので、`.[-1]` だけ見ると先に来た方の
# sentinel / terminal error を落とす。特に terminal error を落とすと「retry しても
# 無駄」を retry し続ける。全部繋ぐ。
LATEST_ISSUE_BODY=$(echo "$NEW_ISSUES" | jq -r 'map(.body // "") | join("\n---\n")') \
  || abort_after_trigger "the issue-comment bodies of this round"

# 罠 10: error string detect → terminal failure、retry しない。
# 罠 56: **quota 切れも terminal**。実測 (PR #181 round 3): "You have reached your
# Codex usage limits for code reviews." が来た。語彙が既存 3 つと重ならないので、
# pattern に足さないと**素通りする**。retry しても復帰しない。
TERMINAL_ERROR_PATTERN="Something went wrong|Script exited|Try again later|usage limits|rate limit"
# 罠 60: "Provided git ref <sha> does not exist" は上の pattern に一致する本文で来るが
# terminal ではない — push 直後に trigger すると codex 側の clone に ref がまだ無いだけで、
# 再 trigger で通る (PR #258 / #265 / #293、3 回目で script 側に移した)。terminal より先に
# 見て別の exit にする。再 trigger は controller の手 (body file 付きで次 round、1 round と数える)。
# ローカル codex (PR #293 の push 2、high): この round の comment は複数ありうる (罠 32/34) ので、
# 繋いだ本文で grep すると ref error と quota 切れが同じ round に来た時に transient が勝つ。
# comment ごとに分類し、ref error 以外で terminal に当たる comment が 1 つでもあれば下の exit 4 に落とす。
TRANSIENT_REF_PATTERN="Provided git ref [0-9a-f]{40} does not exist"
REF_ERROR_COMMENTS=$(echo "$NEW_ISSUES" | jq --arg re "$TRANSIENT_REF_PATTERN" \
  '[.[] | .body // "" | select(test($re))] | length')
OTHER_TERMINAL_COMMENTS=$(echo "$NEW_ISSUES" | jq --arg re "$TRANSIENT_REF_PATTERN" --arg term "$TERMINAL_ERROR_PATTERN" \
  '[.[] | .body // "" | select(test($term) and (test($re) | not))] | length')
require_count "$REF_ERROR_COMMENTS" "the count of git-ref error comments"
require_count "$OTHER_TERMINAL_COMMENTS" "the count of terminal error comments"
if [ "$REF_ERROR_COMMENTS" -gt 0 ] && [ "$OTHER_TERMINAL_COMMENTS" -eq 0 ]; then
  diag "TRANSIENT: codex could not see the PR head yet (trap 60). Body follows on stdout."
  diag "Action: confirm the head sha exists (gh api repos/${OWNER_REPO}/commits/${HEAD_SHA}), then re-trigger once with a body file. It counts as a round."
  echo "$LATEST_ISSUE_BODY"
  exit 10
fi
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
NEW_INLINE=$(json_delta "$(stash_json "$PREV_INLINE" prev_inline.json)" \
                        "$(stash_json "$CUR_INLINE_FRESH" cur_inline.json)" updated_at)
require_delta "$NEW_INLINE" "the inline-comment delta"
# 罠 49 (PR #164 round 7): codex は指摘を **review 本文**に書くこともある — inline の
# アンカーではなく permalink の形で。inline だけ数えると「activity あり / inline 0」が
# 「レビュー済み・指摘なし」と見分けられず、Layer 1 (state=COMMENTED) と Layer 3
# (sentinel 無し) も収束側に倒れるので **3 layer 全部を通り抜ける**。両方を数える。
# ...and **round-scoped**, like NEW_INLINE (罠 27) and NEW_ISSUES (罠 33): 罠 21 の
# とおり re-review で新 submission が出ない round があり、`LATEST_REVIEW` が前 round の
# ままだと修正済みの指摘を数え続けて収束しない (codex P2 round 8 on PR #164)。
NEW_REVIEWS=$(json_delta "$(stash_json "$PREV_REVIEWS" prev_reviews.json)" \
                         "$(stash_json "$CUR_REVIEWS_FRESH" cur_reviews.json)" submitted_at)
require_delta "$NEW_REVIEWS" "the review-body delta"
# 罠 34 が記録しているとおり **1 round に review submission が複数来る**。`.[-1]` だけ
# 数えると、先の submission に P0/P1 があって最後のに badge が無い round で
# `P0_P1_TAGS_PRESENT=0` になり、**blocking な指摘を持ったまま収束**する
# (codex P1 round 8 on PR #164)。この round の body を全部繋いでから数える。
REVIEW_BODY=$(echo "$NEW_REVIEWS" | jq -r 'map(.body // "") | join("\n---\n")') \
  || abort_after_trigger "the review bodies of this round"
body_badges() { printf '%s' "$REVIEW_BODY" | grep -o "$1" | wc -l | tr -d ' '; }
# inline の badge 数は jq の出力を一度受けて検査してから足す (空は `$(( + 0 ))` で 0 になる)。
inline_badges() { echo "$NEW_INLINE" | jq "[.[] | .body | select($1)] | length"; }
INLINE_P0P1=$(inline_badges 'contains("![P0 Badge") or contains("![P1 Badge")')
INLINE_P2=$(inline_badges 'contains("![P2 Badge")')
INLINE_P3=$(inline_badges 'contains("![P3 Badge")')
require_count "$INLINE_P0P1" "the count of inline P0/P1 badges"
require_count "$INLINE_P2" "the count of inline P2 badges"
require_count "$INLINE_P3" "the count of inline P3 badges"
P0_P1_TAGS_PRESENT=$(( INLINE_P0P1 + $(body_badges '!\[P0 Badge') + $(body_badges '!\[P1 Badge') ))
# 罠 17 / 罠 23: 公式 docs は P0/P1 only と書いているが実例で P2 も surface する。
P2_TAGS_PRESENT=$(( INLINE_P2 + $(body_badges '!\[P2 Badge') ))
# 罠 23 が P2 で一度広げた形が、P3 で再発した (PR #177): P0/P1/P2 だけ数えて
# `P2=0` を「指摘なし」と読みかけたが、来ていたのは中身の正しい **P3** だった。
# **badge の集合を列挙で決め打ちしない。** P3 も数え、Step 5 では badge の
# 有無を問わず round の inline を全部出す (= 未知の badge も人の目に入る)。
P3_TAGS_PRESENT=$(( INLINE_P3 + $(body_badges '!\[P3 Badge') ))

# 罠 57: **Layer 1 だけ round scope が掛かっていなかった。** 罠 27 (inline) /
# 罠 33 (issue) / 罠 49 (review body) で 3 つとも delta にしたのに、`STATE_OK` は
# `LATEST_REVIEW` = **PR 全 history の最後**から取っている。その round が
# **何も生まなかった**とき (quota 切れ・沈黙)、前 round の `state=COMMENTED` が
# 残っているので `STATE_OK=true` + badge 0 で **収束と読む**。
# 実測 (PR #181 round 3): 新しい commit を push して trigger した round の産物が
# 「quota に達した」の 1 コメントだけだったのに **CONVERGED (Layer 1 + 2)** を出した。
# **その round の産物が 3 endpoint とも 0 件なら、収束の証拠はどこにも無い。**
N_NEW_INLINE=$(echo "$NEW_INLINE" | jq 'length')
N_NEW_REVIEWS=$(echo "$NEW_REVIEWS" | jq 'length')
N_NEW_ISSUES=$(echo "$NEW_ISSUES" | jq 'length')
require_count "$N_NEW_INLINE" "the count of new inline comments"
require_count "$N_NEW_REVIEWS" "the count of new reviews"
require_count "$N_NEW_ISSUES" "the count of new issue comments"
ROUND_EMPTY=false
if [ "$N_NEW_INLINE" = "0" ] && [ "$N_NEW_REVIEWS" = "0" ] && [ "$N_NEW_ISSUES" = "0" ]; then
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
echo "new_inline=${N_NEW_INLINE} new_reviews=${N_NEW_REVIEWS} new_issues=${N_NEW_ISSUES} round_empty=${ROUND_EMPTY}"
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
# Step 4 で検査済みの snapshot から出す。ここで読み直すと、その失敗を見ないまま exit 0 に
# なり (PR #356 のローカル Codex、medium)、成功しても判定とは別の時点の一覧を出す。
printf '%s' "$CUR_ISSUES_FRESH" | jq -r '.[] | "[\(.updated_at)] \(.body)"'
exit 0
