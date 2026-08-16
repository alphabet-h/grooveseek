---
description: 4-軸 (コード品質 / セキュリティ / テスト / docs) の並列 subagent によるプロジェクト全体レビュー + TODO 化 + 着手プラン提示
---

# /full-audit

プロジェクト全体を多角的に厳しめにレビューし、結果を `.dev/knowledge/` 配下のノートと `.dev/features.json` に整理し、着手プランを提示する。リリース前 / 大規模 refactor 後 / 四半期定期 等の節目で意図的に起動する想定。

## 想定起動タイミング

- Major / minor release を切る前
- 直前 audit から 1 ヶ月以上経過
- 大規模 refactor が間に挟まった
- 重大なセキュリティ修正の後

詳しい判断軸は `CLAUDE.local.md` の「リリース前チェックリスト」を参照。

## 前提

- `.dev/knowledge/` と `.dev/features.json` の運用ルール (`CLAUDE.local.md`) が有効
- subagent type: `feature-dev:code-reviewer`, `general-purpose` が available
- 過去 audit の実例: `.dev/knowledge/review-2026-04-29-full-audit.md`

## 実行フロー

### Phase 0 — Scope 確認

1 行で確認する:

> Audit を始めます。対象ブランチは `main` で、4 軸 (コード品質 / セキュリティ / テスト / docs) の並列 subagent でよいですか? 軸の追加・削除や対象範囲の限定があれば教えてください。

ユーザの応答が無ければ default (main, 4 軸) で進む。

---

### Phase 1 — 4 軸 subagent を並列 dispatch

**重要**: 1 回の message で 4 つの Agent tool 呼び出しを並列発行する。順次にすると 4 倍時間がかかる。

各 subagent の prompt は以下のテンプレを使う。**`{{REPO_PATH}}` は CWD 絶対パスに、`{{VERSION}}` は `Cargo.toml` の version、`{{LATEST_COMMIT}}` は `git rev-parse HEAD` に置換**。

#### 軸 1: Rust コード品質

```
subagent_type: feature-dev:code-reviewer
description: Rust コード品質レビュー
prompt:

あなたは groove というプロジェクトの Rust コード品質を **厳しめに** レビューする任務を負っています。

# プロジェクト背景
- リポジトリパス: {{REPO_PATH}}
- 現バージョン: {{VERSION}}、commit {{LATEST_COMMIT}}
- アーキ詳細: docs/ARCHITECTURE.md / docs/ARCHITECTURE.ja.md
- 主要モジュール: src/main.rs (CLI / clap)、src/db.rs (SQLite + sqlite-vec + FTS5)、src/embedder.rs、src/indexer.rs、src/config.rs、src/eval.rs、src/server.rs (rmcp)、src/parser/、src/schema.rs
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
- 1500-2500 words 程度

レポートを直接 message で返してください。
```

#### 軸 2: セキュリティ・入力検証

```
subagent_type: general-purpose
description: セキュリティ・入力検証レビュー
prompt:

あなたは groove を **セキュリティ視点** で厳しめにレビューする任務を負っています。

# プロジェクト背景
- リポジトリパス: {{REPO_PATH}}
- 現バージョン: {{VERSION}}、commit {{LATEST_COMMIT}}
- stdio + Streamable HTTP の 2 transport
- HTTP transport は intranet 想定 (認証なし、loopback or 信頼内 LAN 想定)
- 既知の制約 (除外して OK): HTTP に認証 layer なし — CLAUDE.local.md「既知の残り課題」を先に読むこと

# レビューしてほしい観点 (すべて)
1. 入力検証 (MCP tool 引数、frontmatter YAML、groove.toml、golden YAML の信頼境界)
2. SQL injection (src/db.rs の query 組み立て、? placeholder、動的 SQL)
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
- 1000-2000 words 程度

レポートを直接 message で返してください。
```

#### 軸 3: テスト品質・カバレッジ

```
subagent_type: general-purpose
description: テスト品質・カバレッジレビュー
prompt:

あなたは groove の **テスト品質とカバレッジ** を厳しめにレビューする任務を負っています。

# プロジェクト背景
- リポジトリパス: {{REPO_PATH}}
- 現バージョン: {{VERSION}}
- Rust binary crate、test は src/*.rs 内 #[cfg(test)] と tests/*.rs に分散
- 重要なローカル制約 (CLAUDE.local.md より):
  - テストの削除・編集は禁止
  - cargo test (default) は embedding 実モデル DL 不要なものだけ。-- --ignored で実モデル
  - tempfile / tempdir crate は使わず std::env::temp_dir() + PID + nanos + Drop guard で自作
  - binary crate なので cargo test --lib は空振り、cargo test --bin groove <name> で叩く
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
- 1500-2500 words 程度

レポートを直接 message で返してください。
```

#### 軸 4: ドキュメント整合性

```
subagent_type: general-purpose
description: ドキュメント整合性レビュー
prompt:

あなたは groove の **ドキュメント整合性** を厳しめにレビューする任務を負っています。

# プロジェクト背景
- リポジトリパス: {{REPO_PATH}}
- 現バージョン: {{VERSION}}
- 英語プライマリの日英バイリンガル運用
- ドキュメント: README.md / README.ja.md、docs/ARCHITECTURE.{md,ja.md}、docs/eval.{md,ja.md}、docs/citations.md、docs/filters.md、CHANGELOG.md、CLAUDE.md、CLAUDE.local.md、CONTRIBUTING.{md,ja.md}、examples/deployments/{personal,nas-shared,intranet-http}/README{,.ja}.md、groove.toml.example
- リリース履歴: git tag --list で確認
- CLAUDE.md にリリース前ドキュメント同期チェックリストあり (要参照)

# レビューしてほしい観点 (すべて)
1. drift / 不整合 (実装と docs 乖離、設定キー / フラグ / フィールドの抜け)
2. 英日同期 (片側にしかない節)
3. CHANGELOG vs git tag 整合 (compare link、日付)
4. README → 各 docs / examples へのリンク (dead link、相対パス、anchor)
5. サブコマンドの doc (README 記述と --help 出力の整合)
6. groove.toml.example と src/config.rs の対応
7. examples/deployments/ の現実装挙動と一致
8. CONTRIBUTING.md の手順が実環境で動くか
9. docs/ARCHITECTURE.md の source layout 表が src/*.rs の現状と合っているか
10. MCP tool 一覧と docstring の整合

# 要件
- 厳しめ (typo まで拾う)
- 形式: Markdown で Critical / High / Medium / Low 4 段階
- 各 issue に該当 file:line + 修正前/修正後 スニペット
- 英日両方を見て片側にしかない情報があれば必ず指摘
- 1500-2500 words 程度

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

過去の出力例: `.dev/knowledge/review-2026-04-29-full-audit.md` を参考にする。

---

### Phase 3 — TODO 化

#### 3-1. knowledge note を作成

新規ファイル: `.dev/knowledge/review-YYYY-MM-DD-full-audit.md` (`YYYY-MM-DD` は audit 実施日 UTC)

テンプレ要素 (詳細は過去版 `review-2026-04-29-full-audit.md` 参照):
- 取得環境 (groove version / commit / 対象ブランチ / 軸)
- 結論サマリ
- 横断テーマ
- 個別の高優先度 Issue
- レビューで確認できた良い点
- TODO 化マッピング (新規 feature ID と category 案)
- 推奨着手順序

#### 3-2. `.dev/features.json` に新規 feature を append

既存 feature の最大 ID を取得して連番で追加:

```bash
NEXT_ID=$(jq '.features | map(.id) | max + 1' .dev/features.json)
```

`status: "todo"` で append する jq one-liner:

```bash
jq '.features += [
  {
    "id": <NEXT_ID>,
    "category": "<security-fix|bug-fix|docs|refactor|security|dependency|hardening|quality|infra|infra-bench|...>",
    "description": "<1-2 段落: 何を / なぜ / 主要修正方針 / 注意点>",
    "status": "todo",
    "verification": "未着手 (TODO)。実装後: <実装後の検証手順>"
  }
]' .dev/features.json > /tmp/features.tmp.json && mv /tmp/features.tmp.json .dev/features.json
```

複数 feature を 1 配列でまとめて append できる。

---

### Phase 4 — 着手プラン提示

ユーザに以下の表形式で提示:

| Phase | PR 内容 | 規模 | 推奨度 |
|---|---|---|---|
| 短期 (即着手可能) | 軽量 fix (sed / docs / 数十 LOC) | 小 | ★★★ |
| 中期 (慎重に) | refactor / 中粒度 feature (~100-200 LOC) | 中 | ★★ |
| 長期 (リリース節目) | 大粒度 (~500+ LOC、別 PR 分割推奨) | 大 | ★ |

最後にユーザに 4 択で意思決定を求める:

- **A. 短期 + 中期** を今セッションで進める (~2-4 時間)
- **B. 短期のみ** を今セッションで片付ける (~30 分-1 時間)
- **C. 中期のみ** に集中 (~2-3 時間)
- **D. 今セッションは TODO 化だけ** で終了 (実装は次セッション)

---

## 出力先まとめ

| 場所 | 種別 | 用途 |
|---|---|---|
| `.dev/knowledge/review-YYYY-MM-DD-full-audit.md` | 新規 (毎回別ファイル) | 人間用 audit 凍結アーカイブ |
| `.dev/features.json` | 既存に append | 機械可読 todo |

`.dev/` は git 追跡外なので commit には乗らない (ローカルのみ)。

## 過去 audit の参照

実例: `.dev/knowledge/review-2026-04-29-full-audit.md`

このファイルが存在すれば、Phase 0 で「前回 audit (`<date>`) からの差分にフォーカスすべき箇所はあるか」をユーザに確認してもよい。

## 関連

- `CLAUDE.local.md` の「`.dev/knowledge/` への書き込み (毎回)」と「リリース前チェックリスト」
- `.dev/feature-ideas.md` の「優先度ピック」 (= 既知の長期計画)、本コマンドが登録する todo は実装可能性の高い具体 issue
