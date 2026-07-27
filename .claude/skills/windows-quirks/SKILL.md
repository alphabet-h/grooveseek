---
name: windows-quirks
description: Eleven field-verified Windows pitfalls from kb-mcp release cycles, each with symptom, root cause, and proven fix. Use when writing or debugging Windows-specific code in this repo — Task Scheduler / schtasks / Register-ScheduledTask integration (including which CI logon sessions can and cannot register tasks), subprocess spawning (conhost flash, CREATE_NO_WINDOW), background process lifecycle, Japanese-Windows encoding (CP932 mojibake, UTF-16 LE BOM, forcing UTF-8 out of powershell.exe), stderr assertions in subprocess tests, PowerShell 5.1 argument passing to native commands (embedded double quotes), silently swallowing cargo/clippy diagnostics with `2>$null`, Git Bash / MSYS rewriting leading-slash arguments into filesystem paths (`gh api`), scripted file edits flipping LF to CRLF and producing whole-file diffs (Python text mode), backslashes being eaten by nested shell/Python layers so a string continuation silently becomes a `\n` escape, or diagnosing "works on Linux, fails on Windows" failures
---

# Windows Quirks (kb-mcp 蓄積罠集)

kb-mcp を Windows (特に日本語 locale) 向けに開発する中で、公式 docs や codex review では検出できず**実機 dogfood で初めて発覚した**罠集。Windows 固有のコードパス (`kb-mcp/src/service/windows.rs`、`crates/kb-mcp-svc/`、subprocess 起動、CI/subagent の実行環境等) に触れる前に必ず目を通すこと。

詳細な出典 note は `.dev/knowledge/` 配下 (**ローカル専用、git untracked のためリポジトリ外部からは参照不可**)。

## 1. Task Scheduler 経由の subprocess 登録 (schtasks / Register-ScheduledTask)

**症状**: 日本語 Windows で `schtasks /Create /XML` が「エンコードを切り替えることができません」で失敗 → UTF-16 LE BOM に直しても root path 登録が `アクセスが拒否されました` で失敗 (非 admin) → `Register-ScheduledTask -Xml` に切り替えても HRESULT `0x80070005` (E_ACCESSDENIED) で失敗。3 layer が段階的に発覚し、v0.8.0 → v0.8.3 まで 3 回の hot-fix を要した。

**原因**: (a) 日本語 locale の schtasks は XML 宣言に関わらず UTF-16 LE BOM を要求 (docs は UTF-8/UTF-16 両対応と明記するが実機は乖離)、(b) schtasks CLI は root path (`\<name>`) への新規 `/Create` に admin elevation を要求 (docs に明示なし)、(c) `Register-ScheduledTask -Xml` parameter set は XML 内 `<Principal><UserId>` を auto-build しないため user-level では admin にフォールバックする。

**正しいやり方**: XML 経路は捨てて `Register-ScheduledTask -Action -Trigger -Settings` (current logon identity から Principal を auto-build) を使う。実装は `kb-mcp/src/service/windows.rs` の `register_via_powershell()` を正とする。要点: ① Action/Trigger/Settings parameter set を使う (Principal が current logon identity から auto-build される)、② PowerShell 単一引用符リテラル内の path は `replace('\'', "''")` で escape、③ `$ErrorActionPreference='Stop'` で cmdlet 失敗を exit code に伝播、④ Action は `kb-mcp-svc.exe` に向け `serve` 引数を渡さない (svc 側が無条件付加、罠 2 参照)。

**logon session 依存だが「CI では常に不可」ではない** (2026-07-26 訂正): SSH / NTLM logon session や subagent の実行環境からは `Register-ScheduledTask` が "Access is denied" になる。一方 **GitHub-hosted の windows-latest runner では成功する** — [公式仕様](https://docs.github.com/en/actions/reference/runners/github-hosted-runners)どおり管理者権限 + UAC 無効でジョブが走るため。AU-09 (PR #83) の nightly windows leg で `windows_register_scheduledtask_smoke_test ... ok` を実測済み。

したがって統合テストは **`#[ignore]` のまま** (Task Scheduler を変更するので通常の `cargo test` では走らせない) だが、nightly の `--include-ignored` では CI カバレッジが得られる。「CI では動かないから」を理由に skip リストへ入れる前に、**その CI がどの logon 環境かを確認して実測する**こと。

出典: `.dev/knowledge/windows-task-scheduler-pitfalls.md` (罠 W-1〜W-6) / `.dev/knowledge/feature-43-summary.md` / `.dev/knowledge/ci-workflow-pitfalls.md` (罠 5)

## 2. コンソール subsystem binary から subprocess を spawn すると黒窓が出る

**症状**: `kb-mcp.exe serve` を Task Scheduler の AtLogOn trigger で起動すると、空の console window が常時表示される。`-WindowStyle Hidden` / `FreeConsole()` / `ShowWindow(SW_HIDE)` を試しても 1 秒程度のフラッシュが残る (microsoft/terminal#249、2018 年から未 fix の既知問題)。

**原因**: `kb-mcp.exe` は console subsystem (cargo default) であり、Windows kernel が process 起動前に `conhost.exe` を allocate してしまう。親側から後付けで隠す手段は根本解決にならない。

**正しいやり方**: `#![windows_subsystem = "windows"]` を付けた別 crate (kb-mcp では `crates/kb-mcp-svc/`) を用意し、そこから `Command::new(...).creation_flags(0x0800_0000 /* CREATE_NO_WINDOW */).spawn(...)` で本体 binary を child として起動する。GUI subsystem 化 flag を本体 crate に直接付けると CLI / MCP stdio / test との両立が崩れるため、別 crate 分離が最も clean。

出典: `.dev/knowledge/feature-44-summary.md` 罠 11

## 3. subagent が spawn したバックグラウンドプロセスは session idle で死ぬ

**症状**: subagent が `run_in_background` の Bash や、そこから起動した子プロセス (例: `kb-mcp.exe index --force` のような長時間処理) を、foreground の 10 分 tool timeout 後もバックグラウンドで生かし続けようとしても、subagent 自身の session が (他 agent からのメッセージ待ち等で) idle になった瞬間、子プロセスごと刈られる。

**原因 (推測)**: Windows 上で subagent の実行環境がプロセスジョブオブジェクト単位で子プロセスを管理しており、session idle → 実行環境の一時停止/解放のタイミングでジョブに紐づく子孫プロセスが丸ごと terminate される。controller (メイン session 寄りの立場) 側の background 実行では同種の長時間プロセスでも生存する、という明確な非対称性がある。

**正しいやり方**: subagent は「foreground tool call 1 回 (最大 10 分) で完結する」作業だけを自分で回す。構造的に 10 分を超えると分かっている作業 (大規模 embedding index、CPU-bound reranker eval 等) は、実行コマンドをコピペ可能な形で controller / team-lead に委譲し、subagent 側で監視ループを自作しない (監視ループごと消えるため)。10 分ちょうどで 2-3 回連続 timeout したら、リトライせず早めに委譲判断する。

出典: `.dev/knowledge/subagent-background-process-lifecycle-pitfalls.md` / `.dev/knowledge/feature-46-summary.md` ハマりどころ (d)

## 4. 編集ツールの CP932 書き戻しで日本語コメントが mojibake 化する

**症状**: PR の最終 review で、直前に編集したはずの日本語コメント 2 行が文字化けしているのを発見。

**原因**: Windows 上で一部の編集ツールがファイルを CP932 (Shift-JIS 系 ANSI codepage) で書き戻し、UTF-8 前提の日本語コメントを破壊する。累積 4 件目の候補として報告されている、地味だが再発しやすい罠。

**正しいやり方**: 恒久的な自動防止策は未確立。**日本語コメントを含む差分は、コミット前・PR final review 時に必ず目視確認する**運用で対処する。文字化けを見つけたら該当行のみ手動で UTF-8 に書き直す。

出典: `.dev/knowledge/feature-46-summary.md` ハマりどころ (f)

## 5. tracing-subscriber が ANSI color を stderr に出す

**症状**: Windows 上で subprocess test が `stderr` の内容を文字列比較すると、期待した文言と一致せず失敗する。

**原因**: Windows 上の `tracing-subscriber` は stderr 出力に ANSI エスケープシーケンス (色付け) を含めることがある。

**正しいやり方**: subprocess test で stderr を assert する際は `strip_ansi` 相当のヘルパーで ANSI コードを剥がしてから比較する。

出典: `.dev/knowledge/feature-27-summary.md` / CLAUDE.md。**CLAUDE.md / CLAUDE.local.md と重複記載。乖離時はそちらを正とする**。

## 6. rust-analyzer の stale diagnostics (Windows で特に頻発)

**症状**: 大きめのコード追加後に頻発する (本 repo では 10 回以上観測)、エディタ上の rust-analyzer が古いエラー (実際には解消済み) を出し続ける現象。

**原因**: LSP のインデックス更新が実ファイル変更に追従しきれていない一時的なノイズ。

**正しいやり方**: `cargo check` の実結果を正 (source of truth) とし、rust-analyzer 上の diagnostics は一時的ノイズとして無視して押し切る。

出典: CLAUDE.local.md 運用上の気付き / `.dev/knowledge/feature-25-eval-notes.md` / `feature-27-summary.md`。**CLAUDE.md / CLAUDE.local.md と重複記載。乖離時はそちらを正とする**。

## 7. PowerShell 5.1 は native コマンド引数内の `"` を escape せず渡して引数分解を壊す

**症状**: `git commit -m @'...'@` の here-string メッセージ内に `"quoted phrase"` を含めたところ、git が `error: pathspec '...' did not match any file(s)` を多数出して commit 失敗。メッセージが `"` の位置で複数の引数に分解されていた。

**原因**: Windows PowerShell 5.1 は native 実行ファイルへ引数を渡す際、引数値に含まれる `"` を Win32 コマンドラインへ再構成するときに escape しない (PowerShell 7.3+ の `PSNativeCommandArgumentPassing` で修正された既知問題)。here-string 自体は正しく単一文字列になっていても、native 側では `"` が引数区切りとして再解釈される。

**正しいやり方**: `"` を含む複数行文字列を native コマンドに渡す場合は PowerShell を使わず **Bash tool + heredoc + `git commit -F -`** (stdin 経由) にする。PowerShell で完結させたい場合はメッセージ内の二重引用符を単一引用符に置き換えるか、一時ファイル + `-F <path>` を使う。

出典: 2026-07-25 session (PR #76 の commit 時に実地で発火)

## 8. PowerShell で `2>$null` を付けると cargo の診断が丸ごと消える (検証が空振りする)

**症状**: `cargo clippy --all-targets 2>$null | Select-String "^warning|^error"` が何も出さないので「clean」と判断したが、実際には `redundant closure` で `-D warnings` が fail していた。review で指摘されるまで気付けなかった。

**原因**: cargo / rustc の診断は **stdout ではなく stderr** に出る。`2>$null` はそれを丸ごと捨てるため、grep 対象が空になって常に「警告なし」に見える。`Select-String` の結果が空 = 成功、と読んでしまうのが罠。さらに `-D warnings` を付けていなければ warning は exit code にも出ない。

**正しいやり方**: 検証コマンドでは **stderr を捨てない**。Bash tool 側で `cargo clippy --all-targets -- -D warnings 2>&1 | tail -5` として exit code を見るか、PowerShell なら `2>$null` を外して `$LASTEXITCODE` を確認する。「grep がヒット 0 件」を成功条件にしない — **exit code を成功条件にする**。

**補足**: これは「検証コマンドが壊れていても成功に見える」class の罠。同 session では codex polling script を `run_in_background` ではなく shell の `&` で起動して harness の追跡外に置く失敗も 2 回起こしており (詳細は `.dev/knowledge/codex-review-loop-pitfalls.md` 罠 38)、**1 度 note に書いただけでは再発を防げなかった**。検証系のコマンドは「exit code を見る」形に統一するのが唯一効く対策。

出典: 2026-07-26 full-audit hot-fix session (review agent の Must-fix で発覚)

## 9. Git Bash が `gh api` の先頭スラッシュを filesystem path に書き換える

**症状**: `gh api /repos/{owner}/{repo}/actions/cache/usage` が下記で失敗する。
```
invalid API endpoint: "C:/Program Files/Git/repos/{owner}/{repo}/actions/cache/usage".
Your shell might be rewriting URL paths as filesystem paths.
```

**原因**: MSYS2 / Git Bash は native な Windows 実行ファイルへ引数を渡す時、`/foo/bar` の形をした引数を POSIX path とみなして Windows path (`C:/Program Files/foo/bar`) へ自動変換する。`gh` は変換後の文字列を endpoint として受け取るため壊れる。

**正しいやり方**: endpoint の**先頭スラッシュを落とす** (`gh api repos/{owner}/{repo}/...`)。`gh` は相対形式を受け付ける。どうしても先頭スラッシュが必要な引数では `MSYS_NO_PATHCONV=1` を前置する。PowerShell 側では発生しない。

出典: 2026-07-26 AU-09 session / `.dev/knowledge/ci-workflow-pitfalls.md` (罠 7)

## 10. Python の text mode で書き戻すとファイル全体が LF → CRLF に反転する

**症状**: `python - <<'PY' ... open(p,'w').write(s) ... PY` で数行だけ書き換えたつもりが、`git diff --stat` が **ファイル全体の書き換え**になる (例: 350 行のファイルが `350 +++ 350 ---`)。`cargo fmt` / `clippy` / `cargo test` は全て通るので、diffstat を見るまで気付かない。

**原因**: このリポジトリは全ファイル **LF** (`core.autocrlf=false`、`.gitattributes` に `text` 指定なし)。Python の text mode は読み込みで universal newlines により `\r\n` / `\n` を `\n` に統一し、**書き込みで `os.linesep` (Windows では `\r\n`) に変換する**。したがって LF のファイルを text mode で round-trip させるだけで CRLF になる。`rustfmt` の `newline_style` は既定 `Auto` = ファイルの現行スタイルを踏襲するので、`cargo fmt` を後から走らせても**元に戻らない**。

**検出のしかた** (`grep -c $'\r'` は当てにならない。od の出力行を数えるのも誤り):
```bash
python -c "b=open('path','rb').read(); print('CRLF=', b.count(b'\r\n'), 'LF=', b.count(b'\n')-b.count(b'\r\n'))"
```
コミット前なら `git diff --stat --ignore-all-space` と素の `--stat` を比べる。数字が大きく食い違えば改行が原因。

**正しいやり方**: 3 つのいずれか。

1. **Edit / Write ツールを使う** (改行を保つ)。まずこれを検討する
2. Python を使うなら **binary mode**: `open(p,'rb').read()` / `open(p,'wb').write(...)`、または `open(p,'w',newline='')`
3. `sed -i` (Git Bash) は LF を保つので、1 行の機械的置換には安全

既に反転させてしまったら、commit 前に一括で戻す:
```bash
python -c "
b=open('path','rb').read()
open('path','wb').write(b.replace(b'\r\n', b'\n'))
"
```

出典: 2026-07-27 AU-10 session (`service/mod.rs` ほか 4 ファイルを反転させ、commit --amend で修復)

## 11. bash heredoc → Python → ソースの多段で backslash が 1 段余計に食われる

**症状**: Rust の文字列継続 (`"...text \` + 改行) を Python の置換で書き込んだのに、ファイルには `\` + 文字 `n` (= `5c 6e`) が入り、**改行エスケープ**になっていた。`cargo check` は通ってしまう (どちらも合法な文字列) ので、**出力を実際に見るまで気付かない** — メッセージ中に改行と 13 個の空白が埋まっていた。

同じ session で、`python -c "... b'\\\\' ..."` が `SyntaxError: unexpected character after line continuation character` になる形でも踏んだ。

**原因**: `Bash` ツール → Git Bash → heredoc / `-c` の引用 → Python の文字列リテラル、と **backslash を解釈する層が複数重なる**。各層が 1 回ずつ食うので、ソースに 1 個残すために何個書けばよいかが状況依存になる。`<<'PY'` で引用しても、`python -c "..."` の二重引用側では効かない。

**正しいやり方**:

1. **Edit / Write ツールで直接書く**。backslash を含むソース片を扱う時はこれ一択
2. どうしても Python で検査したいなら、**backslash を書かずに済ませる**:
   ```bash
   python - <<'PY'
   BS = chr(92).encode()   # backslash を literal として書かない
   LF = chr(10).encode()
   b = open("src/x.rs","rb").read()
   assert BS not in seg and LF not in seg
   PY
   ```
3. そもそも**文字列継続を使わない**。1 行の長い literal は rustfmt が折らないので、メッセージは 1 行で書けば継続自体が不要

**検証は「コンパイルが通った」で止めない**。生成される文字列そのものをバイトで見る:
```bash
python - <<'PY'
b = open("src/x.rs","rb").read(); i = b.index(b"marker")
print(" ".join(f"{c:02x}" for c in b[i:i+30]))
PY
```
`5c 0a` なら継続 (改行と次行先頭の空白を食う)、`5c 6e` なら `\n` エスケープ = **別物**。

出典: 2026-07-27 v0.14.0 release session (`install.rs` の案内文言を 2 crate で書き換えた際)

## 診断の指針: 「Linux では動くのに Windows で失敗する」場合

上記のどれにも当てはまらない未知の Windows 固有挙動に遭遇したら、**公式 docs だけで判断せず実機 dogfood で確認する**。本 skill に集約された罠のほぼ全てが「docs 上は問題ないはずなのに実機だけ失敗する」パターンであり、静的 review (codex / subagent self-review) や CI runner (多くは非 interactive logon session) では再現・検出できない。新しい罠を発見したら `.dev/knowledge/<topic>-pitfalls.md` に追記し、本 skill にも反映すること。
