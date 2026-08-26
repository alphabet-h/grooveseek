---
name: windows-quirks
description: Fifteen field-verified Windows pitfalls from groove release cycles, each with symptom, root cause, and proven fix. Use when writing or debugging Windows-specific code in this repo — Task Scheduler / schtasks / Register-ScheduledTask integration (including which CI logon sessions can and cannot register tasks), subprocess spawning (conhost flash, CREATE_NO_WINDOW), background process lifecycle, Japanese-Windows encoding (CP932 mojibake, UTF-16 LE BOM, forcing UTF-8 out of powershell.exe), stderr assertions in subprocess tests, PowerShell 5.1 argument passing to native commands (embedded double quotes), PowerShell 5.1 `ConvertFrom-Json` emitting a JSON array as one object so `Where-Object` silently filters nothing, silently swallowing cargo/clippy diagnostics with `2>$null`, Git Bash / MSYS rewriting leading-slash arguments into filesystem paths (`gh api`), scripted file edits flipping LF to CRLF and producing whole-file diffs (Python text mode), Python stdout defaulting to CP932 under redirection and dying mid-write on an em dash so the truncated output looks complete, escape miscounts turning a string continuation into a `\n` escape (both compile), `jq.exe` appending a carriage return to every line it writes while `gh --jq` does not, so a file or pipe comparison between the two reports every line as different, or diagnosing "works on Linux, fails on Windows" failures
---

# Windows Quirks (groove 蓄積罠集)

groove を Windows (特に日本語 locale) 向けに開発する中で、公式 docs や codex review では検出できず**実機 dogfood で初めて発覚した**罠集。Windows 固有のコードパス (`grooveseek/src/service/windows.rs`、`crates/groove-svc/`、subprocess 起動、CI/subagent の実行環境等) に触れる前に必ず目を通すこと。

詳細な出典 note は `.dev/knowledge/` 配下 (**ローカル専用、git untracked のためリポジトリ外部からは参照不可**)。

## 1. Task Scheduler 経由の subprocess 登録 (schtasks / Register-ScheduledTask)

**症状**: 日本語 Windows で `schtasks /Create /XML` が「エンコードを切り替えることができません」で失敗 → UTF-16 LE BOM に直しても root path 登録が `アクセスが拒否されました` で失敗 (非 admin) → `Register-ScheduledTask -Xml` に切り替えても HRESULT `0x80070005` (E_ACCESSDENIED) で失敗。3 layer が段階的に発覚し、v0.8.0 → v0.8.3 まで 3 回の hot-fix を要した。

**原因**: (a) 日本語 locale の schtasks は XML 宣言に関わらず UTF-16 LE BOM を要求 (docs は UTF-8/UTF-16 両対応と明記するが実機は乖離)、(b) schtasks CLI は root path (`\<name>`) への新規 `/Create` に admin elevation を要求 (docs に明示なし)、(c) `Register-ScheduledTask -Xml` parameter set は XML 内 `<Principal><UserId>` を auto-build しないため user-level では admin にフォールバックする。

**正しいやり方**: XML 経路は捨てて `Register-ScheduledTask -Action -Trigger -Settings` (current logon identity から Principal を auto-build) を使う。実装は `grooveseek/src/service/windows.rs` の `register_via_powershell()` を正とする。要点: ① Action/Trigger/Settings parameter set を使う (Principal が current logon identity から auto-build される)、② PowerShell 単一引用符リテラル内の path は `replace('\'', "''")` で escape、③ `$ErrorActionPreference='Stop'` で cmdlet 失敗を exit code に伝播、④ Action は `groove-svc.exe` に向け `serve` 引数を渡さない (svc 側が無条件付加、罠 2 参照)。

**logon session 依存だが「CI では常に不可」ではない** (2026-07-26 訂正): SSH / NTLM logon session や subagent の実行環境からは `Register-ScheduledTask` が "Access is denied" になる。一方 **GitHub-hosted の windows-latest runner では成功する** — [公式仕様](https://docs.github.com/en/actions/reference/runners/github-hosted-runners)どおり管理者権限 + UAC 無効でジョブが走るため。AU-09 (PR #83) の nightly windows leg で `windows_register_scheduledtask_smoke_test ... ok` を実測済み。

したがって統合テストは **`#[ignore]` のまま** (Task Scheduler を変更するので通常の `cargo test` では走らせない) だが、nightly の `--include-ignored` では CI カバレッジが得られる。「CI では動かないから」を理由に skip リストへ入れる前に、**その CI がどの logon 環境かを確認して実測する**こと。

出典: `.dev/knowledge/windows-task-scheduler-pitfalls.md` (罠 W-1〜W-6) / `.dev/knowledge/feature-43-summary.md` / `.dev/knowledge/ci-workflow-pitfalls.md` (罠 5)

## 2. コンソール subsystem binary から subprocess を spawn すると黒窓が出る

**症状**: `groove.exe serve` を Task Scheduler の AtLogOn trigger で起動すると、空の console window が常時表示される。`-WindowStyle Hidden` / `FreeConsole()` / `ShowWindow(SW_HIDE)` を試しても 1 秒程度のフラッシュが残る (microsoft/terminal#249、2018 年から未 fix の既知問題)。

**原因**: `groove.exe` は console subsystem (cargo default) であり、Windows kernel が process 起動前に `conhost.exe` を allocate してしまう。親側から後付けで隠す手段は根本解決にならない。

**正しいやり方**: `#![windows_subsystem = "windows"]` を付けた別 crate (groove では `crates/groove-svc/`) を用意し、そこから `Command::new(...).creation_flags(0x0800_0000 /* CREATE_NO_WINDOW */).spawn(...)` で本体 binary を child として起動する。GUI subsystem 化 flag を本体 crate に直接付けると CLI / MCP stdio / test との両立が崩れるため、別 crate 分離が最も clean。

出典: `.dev/knowledge/feature-44-summary.md` 罠 11

## 3. subagent が spawn したバックグラウンドプロセスは session idle で死ぬ

**症状**: subagent が `run_in_background` の Bash や、そこから起動した子プロセス (例: `groove.exe index --force` のような長時間処理) を、foreground の 10 分 tool timeout 後もバックグラウンドで生かし続けようとしても、subagent 自身の session が (他 agent からのメッセージ待ち等で) idle になった瞬間、子プロセスごと刈られる。

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

### ★ 変換は「引数がパスかどうか」を見ない — 検索パターンも書き換わる

`gh api` の endpoint に限った話ではない。**先頭が `/` の引数はすべて対象**で、
検索パターンや正規表現も同じように変換される。

```bash
git grep -l '/var/lib/foo' -- .              # → 0 件 (パターンが C:/Program Files/var/lib/foo になっている)
git grep -lE '(/var/)[^ ]*foo' -- .          # → 一致する ('(' で始まるので変換されない)
```

**`gh api` より危険**。`gh` は変換後の endpoint で **404 になって気付ける**が、
`git grep` / `grep` / `sed` は**一致 0 件を返す** = 「直すものが無い」に見える。
**沈黙する失敗の方が高くつく。**

対策は同じ 2 つ — パターンを `/` で始めない (`var/lib/foo`)、または
`MSYS_NO_PATHCONV=1` を前置する。

出典: 2026-07-26 AU-09 session / `.dev/knowledge/ci-workflow-pitfalls.md` (罠 7)。
`git grep` 側は 2026-08-17 の GrooveSeek 改名で実測 (13 箇所を取りこぼしかけた)

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

## 11. スクリプト経由でソースに書いた backslash は、数え間違えても**コンパイルが通る**

**症状**: Rust の文字列継続 (`"...text \` + 改行) を Python の置換で書き込んだのに、ファイルには `\` + 文字 `n` (= `5c 6e`) が入り、**改行エスケープ**になっていた。継続もエスケープも合法な文字列なので `cargo check` も `cargo fmt` も通り、**メッセージを実際に表示するまで気付かない** — 文中に改行と 13 個の空白が埋まっていた。同じ session で `python -c "..."` が `SyntaxError: unexpected character after line continuation character` になる形でも踏んだ。

**原因は経路ではなく、自分の escape の数え間違い** (2026-07-27、下記の切り分けで確定)。当初これを「インライン経路が backslash を 1 段食う」と書いたが、**それは誤り**だった:

```bash
cat <<'EOF' > probe.txt      # Python を挟まず、素の `\` + 改行だけを通す
A \
 B
EOF
# → 41 20 5c 0a 20 42 0a  = バイトは無傷で届く
```

引用付き heredoc は POSIX どおり body を verbatim に渡す。最初にこれを疑ったのは、**「配送経路」と「Python の bytes literal の有無」を同時に変えた交絡実験**で比較したせい。片方ずつ動かせば経路は無罪と分かる。

**したがって「何個書けば通るか」を数え合わせにいくのが敗因**。Python の bytes literal の中で `\\` は 1 個の backslash、`\n` は改行 — この 2 つを跨いで数えるのは、**間違えても誰も教えてくれない**ので必ず事故る。

**正しいやり方**:

1. **ソースの編集は `Edit` / `Write` ツールで直接行う**。backslash を含む断片ではこれ一択。escape の段数を数える作業自体が発生しない
2. スクリプトが必要なら **`Write` で `.py` に落としてから実行する**。ソースが目に見える形で残るので、数え間違いを読んで見つけられる
3. インラインで済ませたいなら、**backslash を 1 文字も書かない**。`chr(92)` で構築すれば数える対象が消える:
   ```bash
   python - <<'PY'
   BS = chr(92).encode()   # backslash を literal として書かない
   LF = chr(10).encode()
   b = open("src/x.rs", "rb").read()
   i = b.index(b"marker")
   seg = b[i:b.index(b'",', i)]          # 検査したい範囲を必ず先に切り出す
   assert BS not in seg and LF not in seg
   print(seg.decode())
   PY
   ```
4. そもそも**文字列継続を使わない**。rustfmt は長い literal を折らないので、メッセージを 1 行で書けば継続自体が不要になる

**検証は「コンパイルが通った」で止めない**。継続もエスケープも合法な文字列なので、コンパイラは両者を区別しない。生成されたバイトを見る:
```bash
python - <<'PY'
b = open("src/x.rs", "rb").read()
i = b.index(b"marker")
print(" ".join(f"{c:02x}" for c in b[i:i + 30]))
PY
```
`5c 0a` なら継続 (改行と次行先頭の空白を食う)、`5c 6e` なら `\n` エスケープ = **別物**。

出典: 2026-07-27 v0.14.0 release session (`install.rs` の案内文言を 2 crate で書き換えた際)

## 12. `powershell.exe` のリダイレクト出力は既定で ACP (CP932)。`from_utf8_lossy` は全滅する

**症状**: 日本語環境で、PowerShell 由来のエラーメッセージが `????` になる。**それだけなら表示の問題だが、出力を「値」として使っている箇所では黙って壊れる** — `install.rs::run_ps` は作成した `.lnk` の**パスそのもの**を返し、呼び出し側が `PathBuf::from` するので、`C:\Users\山田\...` のようなアカウントでは**誤ったパスが保存される**。`from_utf8_lossy` は `Ok` を返すので下流の誰も検出できない。

**実測** (日本語 Windows 11 / Windows PowerShell 5.1、`日` = U+65E5):

| script | stdout のバイト |
|---|---|
| `Write-Output ...` | `93 fa` = **CP932** |
| `[Console]::OutputEncoding=[Text.Encoding]::UTF8; Write-Output ...` | `e6 97 a5` = **UTF-8** |

CP932 は valid UTF-8 ではない (`0x93` は継続バイト域、`0xfa` は不正な開始バイト) ので、lossy decode は連続した U+FFFD に潰す。

**「たぶん大丈夫」で通すと fix ごと壊れる 3 点。全部測ってある**:

- **stderr にも効く**。ただし `throw` の error record は前置なしでも UTF-8 で出るのに、**ローカライズされた cmdlet error (`Get-Item 'Z:\no-such'` 等) は CP932**。本番で流れるのは後者なので、`throw` で測ると逆の結論になる
- **BOM は出ない**。`[Text.Encoding]::UTF8` は BOM 付き encoding だが、.NET は Console writer を作り直す時に preamble を外す。もし出ていたら `.lnk` パスの先頭が壊れ、mojibake と同じ結果だった (= `UTF8Encoding $false` を書く必要は無い)
- **console が無くても投げない**。`#![windows_subsystem = "windows"]` の親から `CREATE_NO_WINDOW` 付きで spawn しても成功する (= release の tray の形)。`Console.OutputEncoding` の setter は stdout が redirect 済みなら `SetConsoleOutputCP` を呼ばない

したがって **`encoding_rs` の新規依存は要らない**。

**採るべき形** (`grooveseek/src/service/powershell.rs` / `crates/groove-tray/src/powershell.rs`):

1. 前置は **spawn 点 1 箇所**に適用する。script builder 側ではない — パイプのエンコーディングは「子をどう読むか」の性質であって script の内容ではないし、spawn 点が 1 つなら後から script を足しても漏れない
2. decode を **2 種類に分ける**。ここが肝:
   - **値になるもの** (path / JSON) は **strict** (`str::from_utf8`)。lossy は `Ok` を返して壊れた値を通す
   - **診断メッセージ**は lenient のまま。エラー経路で「元のエラーを失う second error」を出さないため。ただし置換が起きたら注記を足し、mojibake が自分で理由を語るようにする
3. **`schtasks` には効かない**。PowerShell ではないので前置が届かない。`service/windows.rs` の 2 箇所がこれで、読むのが ASCII フィールドだけなので lossy のまま実害が無いことを個別に確認してある (CP932 の trail byte 域は `0x40-0x7E` / `0x80-0xFC` なので `,` も `"` も現れず、CSV 分割はずれない)

**テストは文字列 assert で止めない**。「script に前置が含まれる」の assert はコードの言い換えにしかならない。**実際に `powershell.exe` を起動して非 ASCII が往復すること**を見る。文字列は PowerShell 側で codepoint から作れば、途中の層が再エンコードしない。前置が無いと ja-JP では CP932 (strict decode が弾く)、Latin code page では best-fit の `??` (decode は通るが不一致) になり、どちらでも落ちる。

**ただしこのテストは「あらゆる環境で前置の適用を証明する」ものではない**。ACP が既に UTF-8 (CP65001 — Windows 11 の「ベータ: ワールドワイド言語サポートで Unicode UTF-8 を使用」で有効になる) のホストでは、**前置が無くても PowerShell は UTF-8 を出す**ので、前置を外してもテストは通ってしまう。守れているのは「非 UTF-8 ACP のホスト」= まさにこの fix が存在する理由の環境であって、それで十分ではあるが、**CI が UTF-8 ACP の runner だけになると回帰ガードとして無音になる**。「locale 非依存」と書くのは言い過ぎ。

出典: 2026-07-27 AU-04 (PR #108)。測定の詳細は `.dev/knowledge/powershell-output-encoding-measurement.md`

## 13. Python の stdout は redirect 先がファイルでも CP932。**途中で落ちて「完走したように見える」**

**症状**: 検証スクリプトを `python verify.py > out.txt` で回すと、**出力が途中で終わっているのに
それが正常な終わりに見える**。実際には em dash (`—`, U+2014) を書こうとした瞬間に
`UnicodeEncodeError: 'cp932' codec can't encode character '—'` で死んでいる。
本文が日本語混じりの docs を扱うと、**ほぼ確実に踏む**。

罠 12 は `powershell.exe` の出力側の話で、**これは Python 自身の stdout encoding**。
別物なので、罠 12 の対策 (`[Console]::OutputEncoding`) では直らない。

**なぜ危ないか**: 落ちる前に書かれた分は残るので、**出力は「正しく短い結果」と区別が付かない**。
実測 (2026-08-17、PR #175): 10 節を検証するスクリプトが 4 節目で死に、
「4 節分の結果」に見えていた。気付いたのは exit code が 1 だったからだけ。

**対策**:

```bash
PYTHONIOENCODING=utf-8 python verify.py > out.txt     # 書く側
```

**書いてしまった後に読む側も要注意** — 対策前に作ったファイルは CP932 で書かれているので、
`open(f, encoding="utf-8")` は `UnicodeDecodeError` になる。`encoding="cp932"` で読むか、
作り直す。

**検証スクリプトを書く時の一般則**: **出力を truncate しない**。同じ session で
`o[:150]` と `.{95}` 前方 context の 2 つに刺され、どちらも「差分が無い」「該当なし」に
見えた。表示が長いのは正しさの代償。

出典: 2026-08-17 PR #175 (README を docs/ へ分割)。詳細は
`.dev/knowledge/readme-split-link-surface.md`

## 14. PowerShell 5.1 の `ConvertFrom-Json` は JSON 配列を **1 オブジェクト**として流す (filter が黙って素通り)

**症状**: `gh run list --json event | ConvertFrom-Json | Where-Object { $_.event -eq "push" }` が
push 以外も含む**全行**を返す。エラーも警告も出ないので、出力を目で見ない限り気付かない。

**原因**: Windows PowerShell 5.1 の `ConvertFrom-Json` は JSON 配列を `Object[]` **1 個**として
pipeline に出す (展開しない)。`Where-Object` の `$_` は配列そのものになり、`$_.event -eq "push"` は
member enumeration で `@('push')` (= truthy) を返すので配列ごと通る。PowerShell 7 は展開する
(`-NoEnumerate` が opt-out) ので、7 の知識で書くと 5.1 で黙って壊れる。罠 7 の `"` 問題を
避けるために `--jq` から `ConvertFrom-Json` へ逃げた先で踏む。

**正しいやり方**: 括弧で囲んで展開する。

```powershell
(gh run list --json event | ConvertFrom-Json) | Where-Object { $_.event -eq "push" }
# または: ... | ConvertFrom-Json | ForEach-Object { $_ } | Where-Object { ... }
```

実測 (2026-08-23、PS 5.1.26100): `'[{"a":1},{"a":2},{"a":3}]' | ConvertFrom-Json | Measure-Object` →
Count **1**、括弧つき → **3**。

**補足**: `~/.claude/hooks/shell_trap_guard.py` の R10 が `--jq '…"…'` を deny して勧める代替形が
まさにこの形だったので、reason に括弧を書き足した。**deny の reason が勧める代替形も、出荷前に
1 度は実機で出力を見る**。

出典: 2026-08-23 B6 smoke (hook deny 化の検証中、通過した代替形の出力が filter されていなかった)。
詳細は `.dev/knowledge/shell-trap-guard-deny-rollout.md`

## 15. `jq.exe` は書く改行ごとに CR を足す。`gh --jq` は足さない (混ぜた比較が全件不一致になる)

**症状**: `jq` で作った一覧と `gh --jq` で作った一覧を `comm` / `diff` / `sort -u` で突き合わせると、
中身が同じでも**全行が「相違」**として返る。エラーも警告も出ない。

**原因**: Windows ネイティブ build の `jq.exe` は stdout を text mode で開くので、**自分が書く `\n` が
`\r\n` になる**。`gh` の `--jq` は内部が Go 実装 (gojq) なので変換しない。つまり片方だけ CR が付いた
文字列同士を比較することになる。**JSON を吐いた側は無実**のことが多い — jq の入力に CR があっても
JSON parser は whitespace として食うので、上流を疑っても何も出てこない。

**どこまで残るか** (実測、Git Bash + jq-1.8.1):

| 経路 | CR |
|---|---|
| `jq -r` / 素の `jq` / `jq -c` | **書いた改行 1 つにつき 1 個** (2 行出力 → 2、1 行 → 1) |
| `jq -j` (改行を書かない) | 0 |
| `jq ... > file` / `jq ... \| sort` | **残る** |
| `X=$(... \| jq -r ...)` | **残らない** (bash が落とす。`od -c` で確認) |
| `gh --jq` / `gh --json` | **0** |

→ **壊れるのは file / pipe 経由の比較だけ**で、scalar を `$( )` で受ける形は無事。
この repo の `.claude/skills/codex-review/scripts/codex_review_round.sh` が全部 `$( )` 受けなのは
その意味で安全 (同 script の「罠 19」コメントは jq の**入力**側 CR の話で、これとは別)。

**正しいやり方**: 比較の前に jq 側へ `tr -d '\r'` を通す。

```bash
jq -r '.artifacts | keys[]' plan.json | tr -d '\r' | sort > a.txt
gh release view vX.Y.Z --json assets --jq '.assets[].name' | sort > b.txt
comm -23 a.txt b.txt        # a にしか無いもの
comm -13 a.txt b.txt        # b にしか無いもの
```

**どちらに CR が居るかは推測せず数える**: `tr -cd '\r' < f | wc -c`。

**実測 (2026-08-26、v1.1.0 の配布物検証)**: `dist plan --tag=v1.1.0 --output-format=json` の生 JSON は
470 行で CR **0 バイト**。そこから `jq -r` で取り出した 15 行の一覧は CR **15 バイト**。
`gh release view --json assets --jq` 側は **0**。この 2 つを `comm` に渡したら、**両方向とも 15 行**が
「相違」として返った。`tr -d '\r'` を通すと差は `dist-manifest.json` 1 件だけになる (公開時に付く asset なので
plan に無いのが正しい)。

**最初は `dist` が CRLF を吐いていると誤読した。** 生ファイルを `tr -cd '\r' | wc -c` で数えて初めて
jq だと分かった。**出力の異常を、出力を作った最初のコマンドのせいにしない** — パイプの各段で数える。

罠 10 (Python の text mode が file 全体を LF → CRLF に反転させる) と同じ「text mode が黙って改行を変える」
形で、向きが書き込み側ではなく**読み出し側**なのが違い。

出典: 2026-08-26 v1.1.0 リリースの配布物検証。詳細は `.dev/release-checklist.md` の
「バイナリ配布チェック」節

## 診断の指針: 「Linux では動くのに Windows で失敗する」場合

上記のどれにも当てはまらない未知の Windows 固有挙動に遭遇したら、**公式 docs だけで判断せず実機 dogfood で確認する**。本 skill に集約された罠のほぼ全てが「docs 上は問題ないはずなのに実機だけ失敗する」パターンであり、静的 review (codex / subagent self-review) や CI runner (多くは非 interactive logon session) では再現・検出できない。新しい罠を発見したら `.dev/knowledge/<topic>-pitfalls.md` に追記し、本 skill にも反映すること。
