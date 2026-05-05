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

## 設計の柱 (= 22 罠の構造的回避)

本 command は `.dev/knowledge/codex-review-loop-pitfalls.md` 罠 7-19 + 21-22 + 24-33 を **構造的に回避** する設計:

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
| 罠 32 (initial delta で convergence 判定 = codex multi-write を miss) | Phase A (initial activity detection) → Phase B (`QUIET_WINDOW_SEC=30s` の quiet window 確認) の 2 phase polling で round complete を待つ。codex は review submission の後に inline comment を秒〜数十秒遅れで post するため、初回 delta で convergence 判定すると stale state で false-converge する |
| 罠 33 (sentinel / terminal-error が PR 全 history で評価される) | `LATEST_ISSUE_BODY` を `NEW_ISSUES = $CUR_ISSUES − $PREV_ISSUES` (= current round で post された issue comment のみ) から derive する。罠 27 (P-badge round-scoping) と同じ pattern を sentinel + terminal-error チェック側にも適用、prior round の sentinel "Didn't find any major issues" が後続 round に漏れて false-converge する race を排除 |

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

# 罠 26: baseline first, trigger second (順序を逆にしない)
PREV_INLINE=$(snapshot_inline)
PREV_REVIEWS=$(snapshot_reviews)
PREV_ISSUES=$(snapshot_issues)
```

### Step 2 — post @codex review trigger

```bash
gh pr comment <PR#> --body "@codex review"
```

mention は **冒頭 1 回のみ**、本文に追加 context を書く場合 `@codex` 文字列を bare word として使わない (= 罠 15)。

### Step 3 — round-level polling with state snapshot diff + quiet-window completion

snapshot per round で 3 endpoint を `(id, updated_at)` set として取得。Step 1 baseline との diff があれば「activity detected」、その後 **quiet window** (= N 秒間 snapshot 不変) を確認してから convergence 判定に進む (= 罠 13 + 罠 32 完了検知):

```bash
ROUND_START=$(date +%s)
PER_ROUND_TIMEOUT=600   # 罠 9 wall-clock timeout
QUIET_WINDOW_SEC=30     # 罠 32: codex multi-write が落ち着くまで wait

# Phase A: wait for first activity
while true; do
  ELAPSED=$(( $(date +%s) - ROUND_START ))
  if [ "$ELAPSED" -gt "$PER_ROUND_TIMEOUT" ]; then
    echo "WARN: codex no response in ${PER_ROUND_TIMEOUT}s. Suspect stale connector (= 罠 9)."
    echo "Action: user に escalate、disconnect/reconnect connector を提案"
    exit 3
  fi

  CUR_INLINE=$(snapshot_inline)
  CUR_REVIEWS=$(snapshot_reviews)
  CUR_ISSUES=$(snapshot_issues)

  if [ "$CUR_INLINE" != "$PREV_INLINE" ] || \
     [ "$CUR_REVIEWS" != "$PREV_REVIEWS" ] || \
     [ "$CUR_ISSUES" != "$PREV_ISSUES" ]; then
    echo "=== codex initial activity detected after ${ELAPSED}s ==="
    break
  fi
  sleep 30
done

# Phase B: wait for quiet window — 罠 32 (codex P1 round 4 on PR #54): codex は
# review submission を post してから秒〜数十秒遅れて inline comment を post する
# multi-write pattern。最初の delta で convergence 判定すると stale な inline
# state で false-converge する (= 後続の P0/P1 が見えない)。`QUIET_WINDOW_SEC`
# 秒間 snapshot が不変な状態を確認してから Step 4 へ進む。
QUIET_START=$(date +%s)
LAST_INLINE=$CUR_INLINE
LAST_REVIEWS=$CUR_REVIEWS
LAST_ISSUES=$CUR_ISSUES
while true; do
  WALL_ELAPSED=$(( $(date +%s) - ROUND_START ))
  if [ "$WALL_ELAPSED" -gt "$PER_ROUND_TIMEOUT" ]; then
    echo "WARN: quiet window not reached within ${PER_ROUND_TIMEOUT}s wall-clock. Proceeding to Step 4."
    break
  fi

  sleep 15
  CHECK_INLINE=$(snapshot_inline)
  CHECK_REVIEWS=$(snapshot_reviews)
  CHECK_ISSUES=$(snapshot_issues)

  if [ "$CHECK_INLINE" = "$LAST_INLINE" ] && \
     [ "$CHECK_REVIEWS" = "$LAST_REVIEWS" ] && \
     [ "$CHECK_ISSUES" = "$LAST_ISSUES" ]; then
    QUIET_ELAPSED=$(( $(date +%s) - QUIET_START ))
    if [ "$QUIET_ELAPSED" -ge "$QUIET_WINDOW_SEC" ]; then
      echo "=== quiet window of ${QUIET_WINDOW_SEC}s confirmed, round complete ==="
      break
    fi
  else
    # still active, reset quiet window
    QUIET_START=$(date +%s)
    LAST_INLINE=$CHECK_INLINE
    LAST_REVIEWS=$CHECK_REVIEWS
    LAST_ISSUES=$CHECK_ISSUES
    echo "  (still receiving codex writes, reset quiet window)"
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
LATEST_ISSUE_BODY=$(echo "$NEW_ISSUES" | jq -r '.[-1].body // ""')
HEAD_SHA=$(gh api "repos/${OWNER_REPO}/pulls/${PR}" --jq .head.sha)

# 罠 10: error string detect → terminal failure、retry しない
TERMINAL_ERROR_PATTERN="Something went wrong|Script exited|Try again later"
if echo "$LATEST_ISSUE_BODY" | grep -qE "$TERMINAL_ERROR_PATTERN"; then
  echo "ERROR: codex returned terminal failure body (= 罠 10):"
  echo "$LATEST_ISSUE_BODY"
  echo "Action: retry せず user に escalate、別タイミングまたは別 PR で再試行"
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
P0_P1_TAGS_PRESENT=$(echo "$NEW_INLINE" | jq '[.[] | .body | select(contains("![P0 Badge") or contains("![P1 Badge"))] | length')
P2_TAGS_PRESENT=$(echo "$NEW_INLINE" | jq '[.[] | .body | select(contains("![P2 Badge"))] | length')

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
  echo "✅ Converged (Layer 3 sentinel${EXTRA})"
  CONVERGED=true
elif [ "$STATE_OK" = "true" ] && [ "$P0_P1_TAGS_PRESENT" = "0" ] && [ "$P2_TAGS_PRESENT" = "0" ]; then
  echo "✅ Converged (Layer 1 + 2: state=${REVIEW_STATE}, no P-badges)"
  CONVERGED=true
else
  echo "⚠️ Indeterminate (no sentinel + no clean state) — re-trigger or user escalate"
  CONVERGED=false
fi
```

### Step 5 — 結果 fetch + 整形

```bash
echo "=== Top-level summary (review body) ==="
echo "$LATEST_REVIEW" | jq -r '.body // "(no review body)"'
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
echo "=== Top-level issue comments by codex (full) ==="
snapshot_issues | jq -r '.[] | "[\(.updated_at)] \(.body)"'
```

### Step 6 — controller 判定 + retry

| 状態 | アクション |
|---|---|
| `CONVERGED=true` | **収束**、merge / tag に進む |
| `P0_P1_TAGS_PRESENT > 0` | controller が指摘内容を理解 → fix 実装 → push → goto Step 1 (= 新 baseline 取得 + re-trigger)。**ただし** `current_round >= max_rounds` (= default 3) なら user 報告 |
| `STATE_OK=false` でも `P0/P1` も sentinel もなし (= indeterminate) | controller が manual review (= human 判断)、必要なら `@codex review` 再 trigger |
| `exit 3` (= 罠 9 wall-clock timeout) | user に escalate (= "codex no response, suspect stale connector") |
| `exit 4` (= 罠 10 terminal error) | user に escalate (= "codex returned terminal failure, retry will not help") |

### Step 7 — re-review trigger (= round 2 以降)

P0/P1 fix → push 後の re-trigger。**罠 26 適用**: Step 1 と同じ要領で baseline を新 trigger 投稿の **前** に取り直す:

```bash
# 罠 26: baseline first, trigger second — round 2 以降も順序は同じ
PREV_INLINE=$(snapshot_inline)
PREV_REVIEWS=$(snapshot_reviews)
PREV_ISSUES=$(snapshot_issues)

# 罠 15 + 罠 18: @codex review 冒頭 1 回、本文中で codex を bare word でも mention しない、verb は review のみ
gh pr comment <PR#> --body "@codex review

Round ${N} fix (commit \`<SHA>\`):
- <fix 1 line>
- <fix 2 line>
"
```

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

- 動機 + 罠 7-19 + 21-23 + 24-33 の詳細解説: `.dev/knowledge/codex-review-loop-pitfalls.md`
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
