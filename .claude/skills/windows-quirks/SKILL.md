---
name: windows-quirks
description: Six field-verified Windows pitfalls from kb-mcp release cycles, each with symptom, root cause, and proven fix. Use when writing or debugging Windows-specific code in this repo — Task Scheduler / schtasks / Register-ScheduledTask integration, subprocess spawning (conhost flash, CREATE_NO_WINDOW), background process lifecycle, Japanese-Windows encoding (CP932 mojibake, UTF-16 LE BOM), stderr assertions in subprocess tests, or diagnosing "works on Linux, fails on Windows" failures
---

# Windows Quirks (kb-mcp 蓄積罠集)

kb-mcp を Windows (特に日本語 locale) 向けに開発する中で、公式 docs や codex review では検出できず**実機 dogfood で初めて発覚した**罠集。Windows 固有のコードパス (`kb-mcp/src/service/windows.rs`、`crates/kb-mcp-svc/`、subprocess 起動、CI/subagent の実行環境等) に触れる前に必ず目を通すこと。

詳細な出典 note は `.dev/knowledge/` 配下 (**ローカル専用、git untracked のためリポジトリ外部からは参照不可**)。

## 1. Task Scheduler 経由の subprocess 登録 (schtasks / Register-ScheduledTask)

**症状**: 日本語 Windows で `schtasks /Create /XML` が「エンコードを切り替えることができません」で失敗 → UTF-16 LE BOM に直しても root path 登録が `アクセスが拒否されました` で失敗 (非 admin) → `Register-ScheduledTask -Xml` に切り替えても HRESULT `0x80070005` (E_ACCESSDENIED) で失敗。3 layer が段階的に発覚し、v0.8.0 → v0.8.3 まで 3 回の hot-fix を要した。

**原因**: (a) 日本語 locale の schtasks は XML 宣言に関わらず UTF-16 LE BOM を要求 (docs は UTF-8/UTF-16 両対応と明記するが実機は乖離)、(b) schtasks CLI は root path (`\<name>`) への新規 `/Create` に admin elevation を要求 (docs に明示なし)、(c) `Register-ScheduledTask -Xml` parameter set は XML 内 `<Principal><UserId>` を auto-build しないため user-level では admin にフォールバックする。

**正しいやり方**: XML 経路は捨てて `Register-ScheduledTask -Action -Trigger -Settings` (current logon identity から Principal を auto-build) を使う。実装は `kb-mcp/src/service/windows.rs` の `register_via_powershell()` を正とする。要点: ① Action/Trigger/Settings parameter set を使う (Principal が current logon identity から auto-build される)、② PowerShell 単一引用符リテラル内の path は `replace('\'', "''")` で escape、③ `$ErrorActionPreference='Stop'` で cmdlet 失敗を exit code に伝播、④ Action は `kb-mcp-svc.exe` に向け `serve` 引数を渡さない (svc 側が無条件付加、罠 2 参照)。

CI runner / subagent の SSH・NTLM logon session からは `Register-ScheduledTask` 自体が呼べない (COM API が interactive logon token を要求) ので、統合テストは `#[ignore]` + interactive shell からの手動実行にする。

出典: `.dev/knowledge/windows-task-scheduler-pitfalls.md` (罠 W-1〜W-6) / `.dev/knowledge/feature-43-summary.md`

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

## 診断の指針: 「Linux では動くのに Windows で失敗する」場合

上記のどれにも当てはまらない未知の Windows 固有挙動に遭遇したら、**公式 docs だけで判断せず実機 dogfood で確認する**。本 skill に集約された罠のほぼ全てが「docs 上は問題ないはずなのに実機だけ失敗する」パターンであり、静的 review (codex / subagent self-review) や CI runner (多くは非 interactive logon session) では再現・検出できない。新しい罠を発見したら `.dev/knowledge/<topic>-pitfalls.md` に追記し、本 skill にも反映すること。
