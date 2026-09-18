---
description: repo のある drive の空きと target / Temp の残骸を測り、素性が確定したものだけを決まった順序で消す
argument-hint: [apply]
disable-model-invocation: true
---

# /disk-sweep

disk の掃除。引数なしは**測るだけ**で、`apply` を付けた時だけ消す。消すものの判定と順序は `.dev/tools/disk-sweep.ps1` が持ち、この command はそれを呼んで結果を読むだけ — 手順を手で組み立て直さないための入口であり、script が「report only」と出したものを controller の判断で消しに行く command ではない。

`disable-model-invocation: true` なので、user が `/disk-sweep` と打った時にしか動かない。削除を伴う操作をモデルの判断で始めないため。

## 想定起動タイミング

- **release を切った直後**。`/feature-flow` の Phase 7 は controller が報告モードで測って数字を見せるところまでで、
  そこから先 (削除) は user がこの command を打つ。直前まで大量に build していて、次の build 予定が最も薄い
- SessionStart の `disk` 行が `LOW` を出した時
- 大量に build する工程 (`/full-audit`、`--ignored` の test、cross build) の前
- session を閉じる時に「クリーンアップした」と書く前 — worktree や branch を片付けたことは disk を掃除したことにならない

引数 (任意): `/disk-sweep apply` で削除まで行う。

MODE: $ARGUMENTS

**削除に進むのは、上の MODE 行の値が `apply` の 5 文字と完全に一致する時だけ** (小文字、前後の空白を除いて他に何も無い)。
それ以外は**すべて報告のみ**: 空、`Apply` / `APPLY` (大文字小文字が違う)、`apply please` / `apply now` (語が足されている)、
`don't apply` / `not apply` (否定)、`-Apply` (script の flag をそのまま書いた)、日本語の「消して」。
**引数を自然文として読んで意図を推し量らない** — ここは削除の唯一の gate で、曖昧なら報告で止まって
「削除するなら `/disk-sweep apply` とだけ打ってください」と返す。推測で消すより 1 回打ち直してもらう方が安い。

## 前提

- `.dev/` が **それ自体の private repository** として初期化済で、`.dev/tools/disk-sweep.ps1` がある。
  **公開 repo を clone しただけの checkout には無い** = owner 用の workflow で、script の中身を公開側へ写して
  二重化することはしない。無ければ「owner 用 command で、この checkout では動かない」と報告して止まる
  (止まる条件 1)。PowerShell で同じことを手で組み立てて代用しない — 手で組んだ回に順序を間違えている
- Windows 専用 (Windows PowerShell 5.1 で動く)

## 手順

1. **報告を取る** (何も消さない):

```powershell
powershell -NoProfile -File .dev/tools/disk-sweep.ps1
```

   exit 0 だけが成功。それ以外は script 自体の失敗なので、stdout を信用せず exit code と stderr を添えて止まる (止まる条件 2)。
2. **報告を user に見せる** — 空き、`target` 直下の各行と判定 (`delete` / `keep` / `report only`)、Temp の prefix ごとの件数、
   `proc-macro-srv` の表、`report only` の節。数字は script の出力をそのまま使い、丸めたり足し直したりしない。
   MODE が `apply` でなければここで終わる
3. **MODE が `apply` の時だけ削除する**。`cargo check` を含むので数分かかる — **background で走らせ**、完了通知を待つ:

```powershell
powershell -NoProfile -File .dev/tools/disk-sweep.ps1 -Apply
```

4. **exit code で読む**:
   - `0` — 完了。末尾の `| N | ...` の行を `.dev/knowledge/target-dir-disk-hygiene.md` の履歴に回数を入れて貼る
   - `1` で末尾が `PRECHECK-REFUSED` — 事前確認が拒否した。**何も消えていない**。理由 (走っている cargo / rustc / link、
     `target` から動いている process や scheduled task) を user に伝える。止めてよいかを決めるのは user で、
     controller が process を止めて再実行しない
   - `1` で末尾が `PRECHECK-REFUSED` ではない — script が途中で壊れた。何が消えたかは出力から読み、推測で補わない (止まる条件 2)
   - `2` — 削除は済んだが `cargo check` が失敗した。削除が原因とは限らない (元から壊れていた可能性がある) ので、
     check の出力を添えて報告する
   - `3` — 消し切れなかったものがある。出力の `delete failures` を添えて報告する。再実行で消しに行かない
5. `proc-macro-srv` の段が「no deletion in this step」と出したら、その理由 (rust-analyzer が動いている、など) をそのまま伝える。
   飛ばされたのは設計どおりで、失敗ではない

`-DeleteRelease` と `-SkipCheck` は **user が明示した時だけ**付ける。`target\release` は、どの commit から build したかを
script が断定できないので既定では残る — 出力の「release provenance」の 3 行 (exe の mtime / HEAD / `--version`) を user に見せて判断を仰ぐ。

## 止まる条件

1. **前提が欠けている** (`.dev/tools/disk-sweep.ps1` が無い)
2. **script 自体が失敗した** (報告モードが exit 0 以外、または `-Apply` が sentinel 無しの exit 1 / 上に無い exit code)

`report only` と出たもの (Docker、他 project の成果物、`fastembed`、`~\.cargo\bin` の backup、session の scratchpad) を
消すかどうかは user の判断で、この command の中では消さない。

## 関連

- `.dev/tools/disk-sweep.ps1` (= 判定と順序の実体。exit code の契約は冒頭の comment)
- `.dev/knowledge/target-dir-disk-hygiene.md` (= なぜその判定なのか、と回ごとの実測)
- `.dev/tools/session-hooks/session-start.sh` (= `disk` 行と `LOW` の閾値 `GROOVE_DISK_WARN_GB`)
- `.claude/commands/feature-flow.md` (= Phase 7 と「handoff と session の区切り」からここへ来る)
