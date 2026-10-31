---
description: 4-軸 (コード品質 / セキュリティ / テスト / docs) の並列 subagent によるプロジェクト全体レビュー + TODO 化 + 着手プラン提示
---

# /full-audit

プロジェクト全体を多角的に厳しめにレビューし、結果を `.dev/knowledge/` 配下の audit ノートと kuriya の audit 親 (親 issue + 指摘ごとの子 task) に整理し、着手プランを提示する。**release の前の gate ではなく、release とも切り離してある** — 前回の audit からの経過と、
前回の指摘の片付き具合で回す (下の「起動の条件」節)。release の直前に代わりに見る確認は
`.dev/release-checklist.md` の「release 直前の確認」節にある (ここには写さない)。

## 起動の条件

規則の家はこの節だけ (2026-10-31 user 決定、kuriya #641)。Phase 0 がこの節を機械的に当てる。
「前回の audit」= kuriya の audit 親 (tag `full-audit`) のうち、`audit-<YYYY-MM-DD>` tag の日付が最も新しいもの。
日付 tag の付いた audit 親が 1 つも無い時 (初回、または親に日付 tag が無い時) の扱いは Phase 0 の 1 にある (経過の gate を当てずに回すことがある)。

- **通常の引き金**: 前回の audit から **1 か月以上** 経ち、**かつ** 前回までの audit 親の子がすべて片付いている
  (`done` / `dropped`。残してよいのは、理由つきで見送った子だけ)
- **棚卸しの段**: 前回から 1 か月経った時点で子が残っていたら、1 件ずつ「やる / `dropped` / 理由つきで見送り」に振り分け、
  そのうえで回す。「やる」にした子は片付けてから回す。見送りは子に tag `audit-deferred` を足し
  (`mcp__kuriya__update` の `tags` は全置換なので、元の tag も渡す)、body に `append` で理由を書く
- **上限**: 前回から **3 か月** 経ったら、棚卸しが済んでいなくても回す
  (推定の値: minor が 2 週間に 2 回のペースなら、10 回前後の release が無監査で出る)
- **前倒ししてよい**: 大規模 refactor (DB schema / transport / chunking 算法等) の後、
  重大な security 修正 (権限境界 / 入力検証 / 認証層) の後。1 か月を待たずに回してよい (棚卸しの段は同じく先に行う)
- **minor / major の release は引き金に入らない**。2026-10-31 までは「minor / major を出した後」だった (kuriya #600) が、
  audit の指摘を直して minor を出すとそれがまた audit を呼び、片付かない medium / low が release ごとに積み上がった
  (minor は 2 週間に 4 回。via: `git tag -l "v1.2*" --sort=creatordate`)

採らなかった案 (kuriya #641):

- 「minor の後のまま、audit の修正だけの minor を除く」— 新機能が 1 つ混ざるとまた回るので、頻度が下がらない
- 「前回の子が全部片付いた時だけ」— low を 1 件放置すると永久に回らず、新しいコードの検査 (audit の本来の役目) まで止まる。
  棚卸しの段と 3 か月の上限はこれを塞ぐために足した

## 前提

- `.dev/knowledge/` の運用ルールと、「audit は kuriya の親子で持ち、大改修の cycle は今まで通り Markdown」の方針
  (`CLAUDE.local.md` が import する `../grooveseek-shared/rules/workflow.md`、「新 feature の流れ」節) が有効
- kuriya の MCP (`mcp__kuriya__capture` / `find` / `report`) が使えること。使えなければ Phase 0 で止まり、user に伝える
- subagent type: `feature-dev:code-reviewer` (plugin `feature-dev` の agent で、`.claude/agents/` には無い) と `general-purpose` が available。4 軸とも `model: opus` を明示して起動する。`/next-work` Phase 2 の表は**作業の種類で行を引く** — 4 軸はどれもレビューなので `opus` の行で、
  `general-purpose` で起動する軸も `opus` (表の `sonnet` / `general-purpose` はテスト実行の行。fallback は「`general-purpose` + 元の行の `model` を明示」)。
  **project agent の `reviewer` はここでは使わない** — `maxTurns: 40` (`.claude/agents/reviewer.md:6`) は diff のレビューに合わせた上限で、tree 全体の監査では尽きる恐れがある。
  `feature-dev:code-reviewer` が解決しない時は `/next-work` Phase 2 の fallback と同じく `general-purpose` で起動する
- 過去 audit の実例: `.dev/knowledge/archive/audits/review-2026-08-18-full-audit.md`
- `.dev/` は `.git/info/exclude` で公開 repo の追跡外にある owner 側の private repo。本 command が
  参照する `.dev/release-checklist.md` も書き出す `.dev/knowledge/` もそちらにあり、**公開 repo を
  clone しただけの checkout には無い**。内容を公開側へ写して二重化することはしない

## 実行フロー

### Phase 0 — 起動の条件の確認と Scope 確認

**最初に前の audit 親を確かめ、「起動の条件」節を当てる** (subagent を出す前、audit ノートを書く前)。audit 親は**親自身**に tag `full-audit` が付いている
(3-2 の step 1。2026-10-22 分は task #536)。子の tag からは引かない — 指摘の無い audit は子を持たず、付け替えた子は元の日付の tag を持ったままになる。

1. `mcp__kuriya__find` に `tags_all: ["full-audit"]` を渡して親をすべて挙げる (状態で絞らない。`truncated` が true なら `limit` を上げる)。
   `audit-<YYYY-MM-DD>` tag の日付が最も新しいものが前回の audit で、その日付から今日までの経過を出す。
   以下の 2・3 は、このうち `done` / `dropped` 以外の親をすべて見る (直近の 1 つに絞らない)。
   **日付 tag で経過が出せない時**は 3 を当てる前に分ける:
   - **`full-audit` の親が 1 つも無い** (初回): 経過の gate を当てずに回す (2 は当てる対象が無いので飛ばす)
   - **日付 tag の無い `full-audit` 親がある**: その親の番号を user に示し、tag `audit-<YYYY-MM-DD>` を足して直すか
     (日付は親の `created_at` か body の出典のノート名から取り、user に確かめる。`mcp__kuriya__update` の `tags` は全置換なので元の tag も渡す)、
     直せないなら経過の gate を当てずに回すかを user に確かめる。直したら 1 から引き直す。
     日付 tag の付いた親が別にあっても、tag の無い親のほうが新しいかもしれないので、同じく先に user に確かめる
2. **途中で止まった audit が無いか** — 挙がった親の body に「起票完了」の行 (3-2 の step 4) が無ければ、その audit は 3-2 の途中で
   止まっている (以下「未完の親」)。未完の親があれば新しい audit は回さず、3 も当てない:
   - **1 つ**: その親の**番号**を再開の対象として、3-2 の「途中で止まった時」に従って起票を終わらせる
   - **2 つ以上**: **ここで止まる**。番号を user に示し、再開する親を 1 つ選んでもらう (自動で選ばない)。選ばれた 1 つを
     「途中で止まった時」で終わらせ、残りは次の `/full-audit` の Phase 0 でまた 1 つずつ扱う
3. **経過で分ける** (前倒しの事由 — 大規模 refactor / 重大な security 修正の後 — があれば、user に確かめて 1 か月を経たものとして扱う):
   - **3 か月以上**: 上限。子が残っていても回す (残った子は 3-2 の step 2 で新しい親へ付け替える)
   - **1 か月以上 3 か月未満**: それぞれの親を `find` の `under: <親の番号>` で読み、未了 (`open` / `doing` / `pending`) の子のうち
     tag `audit-deferred` の無いものを挙げる。無ければ回す。あれば棚卸しの段に入る — 1 件ずつ「やる / `dropped` / 理由つきで見送り」を
     user に振り分けてもらい、`dropped` は `report` で、見送りは「起動の条件」節の形で記録する。「やる」が 1 件でも残ったら
     **ここで止まり、新しい audit は回さない**。「やる」の子の番号を user に示して終わる
   - **1 か月未満**: **ここで止まる**。前回の日付と、回せるようになる日 (前回 + 1 か月) を user に示して終わる

通ったら 1 行で確認する:

> Audit を始めます。対象ブランチは `main` で、4 軸 (コード品質 / セキュリティ / テスト / docs) の並列 subagent でよいですか? 軸の追加・削除や対象範囲の限定があれば教えてください。

ユーザの応答が無ければ default (main, 4 軸) で進む。

---

### Phase 1 — 4 軸 subagent を並列 dispatch

**重要**: 1 回の message で 4 つの Agent tool 呼び出しを並列発行する。順次にすると 4 倍時間がかかる。

各 subagent の prompt は以下のテンプレを使う。**`{{REPO_PATH}}` は CWD 絶対パスに、`{{VERSION}}` は `Cargo.toml` の version、`{{LATEST_COMMIT}}` は `git rev-parse HEAD` に置換**。
4 軸とも、prompt の先頭に『読み取り専用で動く: file を編集しない、tracked file や git の状態を変えるコマンドを打たない
(`target/` への build と test 実行は可)、指摘を message で返すだけ』を足す (ここで使う subagent type はどちらも tool の allowlist
だけでは読み取り専用にならない — `general-purpose` は Write / Edit と shell を持ち、plugin の `feature-dev:code-reviewer` も `TodoWrite` を持つ)。
`general-purpose` で起動する軸 (軸 2〜4 と、fallback した軸 1) は、さらに『Skill tool で `house-rules` を読んでから始める』を足す
(`/next-work` Phase 2 と同じ。project agent は `skills:` で preload するが、`general-purpose` にはその仕組みが無い)。

#### 軸 1: Rust コード品質

```
subagent_type: feature-dev:code-reviewer
model: opus
description: Rust コード品質レビュー
prompt:

あなたは groove というプロジェクトの Rust コード品質を **厳しめに** レビューする任務を負っています。

# プロジェクト背景
- リポジトリパス: {{REPO_PATH}}
- 現バージョン: {{VERSION}}、commit {{LATEST_COMMIT}}
- アーキ詳細: docs/ARCHITECTURE.md / docs/ARCHITECTURE.ja.md
- 主要モジュール (`grooveseek/src/` 配下): main.rs (CLI / clap)、db.rs + db/ (SQLite + sqlite-vec + FTS5)、embedder.rs、indexer.rs、config.rs、eval.rs、server.rs (rmcp)、parser/、schema.rs
- 開発記憶: CLAUDE.md / CLAUDE.local.md / .dev/knowledge/*.md を **先に読む** こと

# レビューしてほしい観点 (すべて)
1. 設計・抽象化 (モジュール境界、trait、循環依存、過剰/不足な抽象)
2. 正しさ・バグ (panic、unwrap、Option/Result、境界条件、IR メトリック値域、tx 管理)
3. エラーハンドリング (anyhow/thiserror、context、伝播)
4. メモリ・パフォーマンス (clone、Vec realloc、Arc/Rc 過剰、大規模 KB スケール)
5. 並行性 (async runtime、sqlite-vec 並行 access、watcher lock)
6. API 設計 (MCP tool signature、CLI 一貫性、後方互換)
7. 依存管理 (Cargo.toml、不要 dep、feature flag)
8. CI / リリース (.github/workflows/、dist-workspace.toml、cargo-dist)

# 要件
- 厳しめ (「動作するが理想的でない」も拾う)
- 高信頼度のみ (推測ベース除外)
- 形式: Markdown で Critical / High / Medium / Low 4 段階。各 issue に file:line + 1 行サマリ + 詳細 + 修正案
- 既知 trade-off (CLAUDE.local.md / .dev/knowledge/ にあるもの) は除外
- 長さは指摘の数で決まる。controller が 4 本を統合するので、Critical / High は根拠つきで漏れなく、Low は 1 件 1 行で

レポートを直接 message で返してください。
```

#### 軸 2: セキュリティ・入力検証

```
subagent_type: general-purpose
model: opus
description: セキュリティ・入力検証レビュー
prompt:

あなたは groove を **セキュリティ視点** で厳しめにレビューする任務を負っています。

# プロジェクト背景
- リポジトリパス: {{REPO_PATH}}
- 現バージョン: {{VERSION}}、commit {{LATEST_COMMIT}}
- stdio + Streamable HTTP の 2 transport
- HTTP transport は intranet 想定 (認証なし、loopback or 信頼内 LAN 想定)
- 既知の制約 (除外して OK): HTTP に認証 layer なし — 残課題の一覧 `.dev/known-issues.md` を先に読むこと

# レビューしてほしい観点 (すべて)
1. 入力検証 (MCP tool 引数、frontmatter YAML、groove.toml、golden YAML の信頼境界)
2. SQL injection (grooveseek/src/db.rs と db/ の query 組み立て、? placeholder、動的 SQL)
3. パストラバーサル (kb_path 配下の `..` 抜け、symlink、exclude_dirs bypass、Win/POSIX 差)
4. ファイルシステム (任意 read 境界、--config の任意ファイル、watcher の対象外監視)
5. HTTP transport (Host header、DNS rebinding、CORS、/healthz 漏洩、bind デフォルト)
6. DoS / リソース枯渇 (巨大 query、巨大 KB force re-index、無限再帰、watcher event flood)
7. 依存ツリー (RUSTSEC-* 該当、リスク高 crate)
8. シークレット / 機密 (ログに kb 内容 / クエリ平文、telemetry)
9. MCP セッション境界
10. eval / golden YAML の unsafe deserialize

# 要件
- pentester 視点で具体的攻撃シナリオ
- 推測除外、コード読んで再現できるもののみ
- 形式: Markdown で Critical / High / Medium / Low / Info の 5 段階
- 各 issue に file:line / 攻撃シナリオ / 詳細 / 緩和策
- 既知制約は重複指摘せず残存リスクを拾う
- 効いている既存防御策は明記
- 長さは指摘の数で決まる。controller が 4 本を統合するので、Critical / High は根拠つきで漏れなく、Low / Info は 1 件 1 行で

レポートを直接 message で返してください。
```

#### 軸 3: テスト品質・カバレッジ

```
subagent_type: general-purpose
model: opus
description: テスト品質・カバレッジレビュー
prompt:

あなたは groove の **テスト品質とカバレッジ** を厳しめにレビューする任務を負っています。

# プロジェクト背景
- リポジトリパス: {{REPO_PATH}}
- 現バージョン: {{VERSION}}
- Cargo workspace (本体 grooveseek/ は lib + bin `groove`)、test は grooveseek/src/ 内 #[cfg(test)] と grooveseek/tests/*.rs に分散
- 重要なローカル制約 (CLAUDE.local.md より):
  - テストの削除・編集は禁止
  - cargo test (default) は embedding 実モデル DL 不要なものだけ。-- --ignored で実モデル
  - tempfile / tempdir crate は使わず std::env::temp_dir() + PID + nanos + アトミックカウンタ + Drop guard (共有ヘルパは grooveseek/src/test_support.rs)
  - lib + bin の二重 target で test 本体は lib にあるので、cargo test --bin groove は空振りし得る。cargo test --lib <name> で叩く
  - subprocess test で stderr 文字列 assert する時は ANSI 色を strip

# レビューしてほしい観点 (すべて)
1. 欠落しているテストケース (各モジュールで edge / boundary / error path 抜け)
2. テスト設計 (unit / integration / e2e 分離、mock vs real DB、#[ignore] 運用妥当性)
3. regression 防御 (直近の bug fix が test で押さえられているか)
4. test data (golden YAML 代表性、fixtures 整理、KB sample data)
5. flakiness リスク (timing / async / watcher / temp dir、Win/Linux 差)
6. Property-based testing 候補 (proptest で押さえる invariant)
7. CI でのテスト戦略 (.github/workflows/ci.yml、cross-platform 行列、--ignored 扱い)
8. performance / benchmark test (大規模 KB、品質 regression detection)

# 要件
- 厳しめ
- 既存テスト数の カウントを簡潔に
- 形式: 欠落 test ケースを Critical / High / Medium / Low、各項目に「想定シナリオ」「テスト名候補」「対象 file:line」
- 「テスト削除・編集禁止」を尊重し追加提案のみ
- 長さは指摘の数で決まる。controller が 4 本を統合するので、Critical / High は根拠つきで漏れなく、Low は 1 件 1 行で

レポートを直接 message で返してください。
```

#### 軸 4: ドキュメント整合性

```
subagent_type: general-purpose
model: opus
description: ドキュメント整合性レビュー
prompt:

あなたは groove の **ドキュメント整合性** を厳しめにレビューする任務を負っています。

# プロジェクト背景
- リポジトリパス: {{REPO_PATH}}
- 現バージョン: {{VERSION}}
- 英語プライマリの日英バイリンガル運用
- ドキュメント: README.md / README.ja.md、docs/{configuration,usage,clients,mcp-tools,behavior}.{md,ja.md}、docs/ARCHITECTURE.{md,ja.md}、docs/{eval,citations,filters,retrieval-pipeline,stability}.{md,ja.md}、CHANGELOG.md、CLAUDE.md、CLAUDE.local.md、CONTRIBUTING.{md,ja.md}、grooveseek/examples/deployments/{personal,nas-shared,intranet-http}/README{,.ja}.md、grooveseek/groove.toml.example
- リリース履歴: git tag --list で確認
- `.dev/release-checklist.md` に「ドキュメント同期」節あり (要参照)

# レビューしてほしい観点 (すべて)
1. drift / 不整合 (実装と docs 乖離、設定キー / フラグ / フィールドの抜け)
2. 英日同期 (片側にしかない節)
3. CHANGELOG vs git tag 整合 (compare link、日付)
4. README → 各 docs / examples へのリンク (dead link、相対パス、anchor)
5. サブコマンドの doc (README 記述と --help 出力の整合)
6. grooveseek/groove.toml.example と grooveseek/src/config.rs の対応
7. grooveseek/examples/deployments/ の現実装挙動と一致
8. CONTRIBUTING.md の手順が実環境で動くか
9. docs/ARCHITECTURE.md の source layout 表が grooveseek/src/ と crates/ の現状と合っているか
10. MCP tool 一覧と docstring の整合

# 要件
- 厳しめ (typo まで拾う)
- 形式: Markdown で Critical / High / Medium / Low 4 段階
- 各 issue に該当 file:line + 修正前/修正後 スニペット
- 英日両方を見て片側にしかない情報があれば必ず指摘
- 長さは指摘の数で決まる。controller が 4 本を統合するので、Critical / High は根拠つきで漏れなく、Low は 1 件 1 行で

レポートを直接 message で返してください。
```

---

### Phase 2 — 集約

各 subagent からのレポートを統合する。出力形式は固定:

1. **Executive Summary** (1 段落、共通テーマ + 全体評価)
2. **横断テーマ (Cross-cutting Findings)** — 複数 subagent が同根の問題を別角度で指摘したものをグルーピング (例: 「f64 値域 invariant の系統的不在」「入力境界バリデーションの薄さ」「トランザクション保護の不一致」「CI の弱さ」「docs stripping campaign の取り残し」等)
3. **個別の高優先度 Issue (Critical / High)** — 各 subagent の発見を表形式に統合
4. **既知の良い点 (Pass 判定)** — 監査でクリアと確認できた防御策
5. **推奨アクション** — 短期 / 中期 / 長期の 3 段階に分けて

過去の出力例: `.dev/knowledge/archive/audits/review-2026-08-18-full-audit.md` を参考にする。

#### 既知の偽陽性 (起票前に grep で潰す)

subagent の指摘のうち、過去に **同じ内容を 2 回以上「欠けている」と誤起票した** ものを置く。
kuriya へ起票する前に、ここに当たるものは指摘された場所を自分の目で確認する:

- **`cargo audit` は CI に無い**: 無いのではなく nightly-only。`.github/workflows/nightly.yml` の
  job `cargo-audit` が走り、失敗 issue を起こす job は `needs:` でそれを待つ。2026-07-26 サイクルの
  AU-27 と 2026-09-02 の AV-42 で 2 度「CI に欠けている」と起票された。どちらも、手元の shell に
  `cargo-audit` が入っていなかったのを「CI にも無い」と読んでいた。次に同じ指摘が出たら、起票する前に
  `grep -n "cargo audit" .github/workflows/*.yml` を実行する

---

### Phase 3 — TODO 化

#### 3-1. knowledge note を作成

新規ファイル: `.dev/knowledge/review-YYYY-MM-DD-full-audit.md` (`YYYY-MM-DD` は audit 実施日 UTC)。
**新規は必ず `.dev/knowledge/` 直下に書く** — 済んだサイクルのノートは
`.dev/knowledge/archive/audits/` へ移してあるので、最新はいつも live 側にある。

テンプレ要素 (詳細は過去版 `archive/audits/review-2026-08-18-full-audit.md` 参照):
- 取得環境 (groove version / commit / 対象ブランチ / 軸)
- 結論サマリ
- 横断テーマ
- 個別の高優先度 Issue
- レビューで確認できた良い点
- TODO 化マッピング (3-2 で kuriya に起票する ID・重大度。kuriya の item 番号は 3-2 の step 4 で書き足す)
- 推奨着手順序

#### 3-2. kuriya に audit 親と子 task を起票

Markdown の台帳 (`audit-todos.md`) は作らない。audit の台帳 (2026-10-22 まで) は
`.dev/archive/2026-10-22-cycle/` で凍結し、その未了は kuriya の task #536 の子へ移した。

1. **親を 1 つ作る** — `mcp__kuriya__capture` に `kind: issue`、title `full-audit YYYY-MM-DD`、
   body に出典 (Phase 3-1 の `.dev/knowledge/review-YYYY-MM-DD-full-audit.md`、対象 commit) と ID の接頭辞、
   tags に `full-audit` と `audit-<YYYY-MM-DD>` (Phase 0 は親をこの tag で引く)。返った親の番号を 3-1 のノートにすぐ書いておく
2. **前の親すべての残りを新しい親へ付け替える** — 付け替え元は Phase 0 の 1 で挙げた `done` / `dropped` 以外の audit 親のうち、
   次の 3 つを除いたものすべて (直近の 1 つに絞らない): 新しい親 (step 1 で作った、または再開している親) 自身 /
   未完の親 (body に「起票完了」の行が無い親。自分の起票が終わるまで子を動かさず、`done` にもしない。未起票の指摘を黙って捨てないため) /
   `audit-<YYYY-MM-DD>` の日付が新しい親と同じか後の親 (古い audit を再開した時に、後の audit の子を逆向きに吸い上げないため)。
   日付 tag の無い親 (Phase 0 の 1 で直さなかった親) は、付け替え元に入れるかを user に確かめる。
   新しく回した audit なら、付け替え元に残る未了は見送りの子 (tag `audit-deferred`) と、3 か月の上限で回した時の振り分け前の子だけ
   (「やる」が残っていれば Phase 0 で止まっている)。再開の時は Phase 0 の 3 を通っていないので、振り分け前の子も来る。
   どれも同じく付け替え、次の棚卸しの段に回す。それぞれの親を `find` の
   `under: <親の番号>` で読み、未了 (`open` / `doing` / `pending`) の子をすべて
   `mcp__kuriya__update` に `item_id` と `parent_id: <新しい親の番号>` を渡して付け替え (木は 4 段まで。audit の子は 2 段目なので収まる)、
   次の棚卸しの段で新しい親の子と一緒に振り分ける。付け替えたら元の親をもう一度 `under` で読み、未了が残っていないことを確かめてから
   `report` で `done` にする (残っていれば付け替えを続ける)。この step は冪等 — 付け替え済みの子は新しい親の下にいるので、
   もう一度当てても同じ結果になる
3. **指摘ごとに子 task を作る** — `capture` に `kind: task`・`parent_id: <親の番号>`。
   - title: `<接頭辞>-NN: 要旨` (接頭辞は audit ごとに決める。`AU` / `BU` … の続き)
   - priority: 重大度 (Critical → `highest`、High → `high`、Medium → `medium`、Low → `low`、Info → `lowest`)
   - tags: `audit-<YYYY-MM-DD>`、重大度 (`critical` / `high` …)、軸 (`code` / `security` / `test` / `docs`)、
     該当時は `release-blocker` (定義 = security の High 以上 / 使う人に見える回帰 / 秘密の漏洩)
   - body: 対象 `file:line`、何が / なぜ壊れるか / どう直すか、完了条件 (実装後に何を見れば直ったと言えるか)
4. **ノートに番号を書き足して閉じる** — 3-1 のノートの TODO 化マッピングに、親と各子の kuriya の item 番号を書き足す。
   あわせて親の body の末尾に `起票完了: <子の数> 件` の行を `mcp__kuriya__update` の `append` で足す (Phase 0 はこの行の有無で途中停止を見分ける)。
   ノートはこの書き足しで完成とし、以後は凍結する (状態は kuriya だけが持つ)
5. **状態は kuriya の `report` で動かす** — 着手は `doing`、直したら `done` (summary に PR 番号)、
   直さないと決めた low は `dropped` (summary に理由)。子が全部 `done` / `dropped` になったら親を `done` にする

**途中で止まった時** (step 1〜4 のどこかで落ちた、または Phase 0 の 2 が未完の親を見つけた): 親を作り直さない。
再開の対象は**番号で受け取る** — 同じ session で落ちたなら step 1 の `capture` が返した番号、新しい session なら Phase 0 の 2 が
示した未完の親の番号 (2 つ以上なら user が選んだ 1 つ)。`audit-<YYYY-MM-DD>` の tag で引き直さない (同じ日付の親が複数あり得て、
どれを再開するか決められない)。3-1 のノートに親の番号が無ければ、先にその番号をノートに書いてから続ける。
その番号の親を新しい親として、**まず step 2 からやり直す** (冪等なので、付け替えの途中で止まっていても、済んでいても同じ結果になる。古い親に子を取り残さない)。
そのうえで `find` の `under: <親の番号>` で作られた子の title (`<接頭辞>-NN`) とノートの TODO 化マッピングを突き合わせ、
足りない子だけを作ってから step 4 に進む (step 2 で付け替えた前の audit の子は、元の日付の tag と接頭辞を持つので突き合わせから外す)。
この再開の間は Phase 0 の 3 の止まる条件を当てない (今まさに作っている親の子なので)。

`release-blocker` 以外の指摘は release を止めない (後の release で直す候補)。tag の前に見る形は
`.dev/release-checklist.md` の「release 直前の確認」節。

---

### Phase 4 — 着手プラン提示

ユーザに以下の表形式で提示:

| Phase | PR 内容 | 規模 | 推奨度 |
|---|---|---|---|
| 短期 (即着手可能) | 軽量 fix (sed / docs / 数十 LOC) | 小 | ★★★ |
| 中期 (慎重に) | refactor / 中粒度 feature (~100-200 LOC) | 中 | ★★ |
| 長期 (リリース節目) | 大粒度 (~500+ LOC、別 PR 分割推奨) | 大 | ★ |

表の各行は kuriya の子 task の番号で指す (短期 = 小さい fix の子、中期 = refactor の子 …)。
`release-blocker` の子は表の先頭に別の行で出す (次の release の前に片付ける分)。

最後にユーザに 4 択で意思決定を求める。選んだ結果は kuriya の木で表す:

- **A. 短期 + 中期** を今セッションで進める (~2-4 時間) — 該当する子を `doing` に
- **B. 短期のみ** を今セッションで片付ける (~30 分-1 時間) — 短期の子を `doing` に
- **C. 中期のみ** に集中 (~2-3 時間) — 中期の子を `doing` に
- **D. 今セッションは TODO 化だけ** で終了 (実装は次セッション) — 子は `open` のまま

どれを選んでも、残った子は親の下で `open` のまま、次の audit の前の棚卸しの段 (「起動の条件」節) で振り分ける。
直さないと決めた low は `dropped` にする。
木は `mcp__kuriya__find` の `under: <親の番号>` で読める。

---

## 出力先まとめ

| 場所 | 種別 | 用途 |
|---|---|---|
| `.dev/knowledge/review-YYYY-MM-DD-full-audit.md` | 新規 (毎回別ファイル) | 人間用 audit 凍結アーカイブ |
| kuriya の audit 親 (issue) と子 task (`parent_id`) | 新規 (audit ごと) | 指摘の状態。tag `audit-<YYYY-MM-DD>` / 重大度 / 軸 / `release-blocker` で引く |

`.dev/` は git 追跡外なので commit には乗らない (ローカルのみ)。
大改修の cycle (SDD の `progress.md` など) は今まで通り `.dev/archive/<date>-cycle/` の Markdown で持つ。kuriya に移るのは audit だけ。

## 過去 audit の参照

実例: `.dev/knowledge/archive/audits/review-2026-08-18-full-audit.md`

直前の audit の日付は kuriya の audit 親の `audit-<YYYY-MM-DD>` tag で引く (Phase 0 の 1)。ノートの置き場は分かれている —
新規は `.dev/knowledge/review-YYYY-MM-DD-full-audit.md`、済んだサイクルのものは `.dev/knowledge/archive/audits/` — ので、**両方見る**:

```bash
find .dev -name 'review-*full-audit*' | sort | tail -1
```

この行が正しいのは live 側 (`.dev/knowledge/` 直下) に古いノートを残していない時だけ。`sort` は path 全体を比べるので
`knowledge/review-...` が `knowledge/archive/...` より後に来て、live に何かあればそれが勝つ (2026-08-27 の PR #244 まで、
live に残っていた 2026-04-29 のノートが毎回返っていた)。**live を空に保つのが前提** — 残す必要ができたら basename で sort する形に変える
(codex P2 on PR #244)。見つかったら Phase 0 で「前回 audit (`<date>`) からの差分にフォーカスすべき箇所はあるか」をユーザに確認してもよい。

## 関連

- `CLAUDE.local.md` が import する `../grooveseek-shared/rules/knowledge-notes.md` (`.dev/knowledge/` への書き込み) と
  `../grooveseek-shared/rules/workflow.md` (audit と cycle の置き場)。どちらも owner 側の private repo で、公開 repo の checkout には無い
- `.dev/release-checklist.md` の「release 直前の確認」節 (release を止めるのは `release-blocker` の未了だけで、audit を回したかどうかではない)
- `.dev/feature-ideas.md` の「優先度ピック」 (= 既知の長期計画)、本コマンドが登録する todo は実装可能性の高い具体 issue
