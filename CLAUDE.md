# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## プロジェクト概要

Markdown / プレーンテキスト / PDF / Office 文書のナレッジベースに対するセマンティック検索を提供する MCP (Model Context Protocol) サーバ。YAML frontmatter 付きの Markdown (および opt-in で `.txt` / `.pdf` / `.docx` / `.xlsx` / `.pptx`) を見出し (シート / スライド / ページ) 単位でチャンク化し、選択可能な埋め込みモデル (BGE-small-en-v1.5 / BGE-M3) でベクトル化。sqlite-vec のベクトル検索と FTS5 全文検索を Reciprocal Rank Fusion で融合し、任意で cross-encoder reranker を適用する。stdio または Streamable HTTP トランスポートで Claude Code / Cursor 等の MCP クライアントに接続する。

詳細:
- **レビューで見る不変条件**: [AGENTS.md](./AGENTS.md) の `## Code Review Rules` (英語)。
  codex code review が読む公式の場所で、**規約の書き方が「書く時の手順」ではなく
  「レビュアーが確認する不変条件」**なのが本ファイルとの違い。本ファイルは手順側を持つ
- ユーザ向けドキュメント: [README.md](./README.md) (English) / [README.ja.md](./README.ja.md) (日本語)
- ソース構造: [docs/ARCHITECTURE.md](./docs/ARCHITECTURE.md) (English) / [docs/ARCHITECTURE.ja.md](./docs/ARCHITECTURE.ja.md) (日本語)
- 設計判断の記録 (ADR): [docs/decisions/](./docs/decisions/)。運用ルールと判定基準は [ADR-0000](./docs/decisions/0000-record-decisions-as-adrs.ja.md)

## ビルド・テスト

```bash
cargo build --release                    # release バイナリ: target/release/groove(.exe)
cargo check                              # 型検査のみ (高速)
cargo test                               # 軽量テスト (embedding DL 不要なもののみ)
cargo test -- --ignored                  # 実モデル DL を伴う embedding / reranker テスト
                                         # (BGE-small ~130 MB / BGE-M3 ~2.3 GB / BGE-reranker-v2-m3 ~2.3 GB)
```

Windows では `groove.exe` になる。ONNX runtime (`ort-sys`) は静的リンクされるため**追加の DLL は不要**。SQLite も `rusqlite` の `bundled` feature で同梱。

## 主要サブコマンド

`index` / `status` / `serve` / `search` / `graph` / `validate` / `doctor` / `eval` / `tune` / `service`。フラグの詳細は [docs/usage.ja.md](./docs/usage.ja.md)、`groove.toml` 設定は [docs/configuration.ja.md](./docs/configuration.ja.md)、`.mcp.json` 接続例は [docs/clients.ja.md](./docs/clients.ja.md) を参照。

## CLI 出力規約 (= stdout/stderr の責務分離)

規約そのもの (なぜ分けるか / stderr が ASCII 限定である理由) は
[AGENTS.md](./AGENTS.md) の "Results go to stdout, diagnostics to stderr" に
置いた。**ここに残すのは「どこに何があるか」の索引と、実際に踏んだ罠**:

`groove` の各 subcommand は出力先を以下の規約で使い分ける:

- **stdout** = そのコマンドの結果の出力先 (既定形式はコマンドごとに違う: `search` / `graph` は json、`eval` / `tune` / `validate` / `doctor` は text)
  - `groove search` の結果 (`print_search_results`)
  - `groove eval` の golden query 評価結果
  - `groove tune` の sweep 結果
  - `groove validate` のレポート (`print_validate_report`)
  - `groove doctor` のレポート (`print_doctor_report`)
  - `groove graph` の connection graph (`print_graph`)
- **stderr** = status / progress / 診断 (= 人間向けの進捗 / warning / error)
  - `groove index` の `Indexing ...` / `Done in ...` 進捗
  - `groove status` の `Documents: N` / `Chunks: N` 統計
  - すべての warning / info / error メッセージ (`tracing` / `eprintln!`)

**新規 subprocess test を書く時の注意**: subcommand の出力先を `grooveseek/src/main.rs` の `Commands::*` block で必ず先に grep 確認すること。その際 **`println!` だけでなく `print!` も** 対象にする (`eval` / `tune` の text 分岐は `print!`)。また **arm が直接書かず helper (`print_search_results` / `print_graph` / `print_validate_report` / `print_doctor_report`) に委譲している場合がある**。stdout に CLI 結果を書くのは上記 6 subcommand だけで、`index` / `status` / `service` は stderr のみ (= F-67 で `groove status` を stdout から読もうとして fail した過去あり)。`serve` は CLI 出力を持たないが、stdio transport では **MCP プロトコルが stdout を使う**点に注意。

## 運用の細則

- **`Cargo.lock` はコミットする** (binary crate)
- **`.groove.db` はクライアントプロジェクト側の責務**。本リポジトリでは生成しない
- **テストは 2 層構造**: 通常 `cargo test` では `#[ignore]` の embedding 実行テストはスキップされる。CI 等で検証したければ `-- --ignored` を付ける
- **staging 禁止ファイル**: `.mcp.json` (ローカルパス)、`groove.toml` (ユーザ設定) は `.gitignore` 済み。テンプレートは `.mcp.json.example` / `groove.toml.example`
- **設計判断は ADR に残す**: ① 実際に選択肢を比較した ② 覆すのが高くつく ③ structure / 依存 / interface / 非機能特性に影響する — **3 つすべて**を満たす時だけ `docs/decisions/` に英日ペアで追加する。満たさないなら `CHANGELOG` で足りる。ADR を足したら、同じ理由を言い直している `CHANGELOG` / `README` / ソースコメントを要約 + リンクに削る。決定を覆す時は**編集せず**新 ADR を追加し、旧 ADR の status を `superseded by ADR-NNNN` にする。詳細は [ADR-0000](./docs/decisions/0000-record-decisions-as-adrs.ja.md)

## Embedding モデルのキャッシュ

`grooveseek/src/embedder.rs::resolve_cache_dir()` が以下の順でキャッシュディレクトリを決定する:

1. `FASTEMBED_CACHE_DIR` 環境変数 (最優先)
2. OS 標準キャッシュディレクトリ + `fastembed`
   - Linux: `~/.cache/fastembed`
   - macOS: `~/Library/Caches/fastembed`
   - Windows: `%LOCALAPPDATA%\fastembed`
3. `.fastembed_cache/` (CWD 直下、最終フォールバック)

初回実行時に HuggingFace hub 互換キャッシュが作られる (BGE-small: ~130 MB、BGE-M3: ~2.3 GB、BGE-reranker-v2-m3: ~2.3 GB)。2 回目以降は再 DL されない。TLS 接続エラー時は [docs/clients.ja.md](./docs/clients.ja.md) の「HuggingFace の TLS 失敗への対処」節の迂回手順を参照。

## 言語方針

本プロジェクトは**英語プライマリの日英バイリンガル**運用:
- `README.md` (English, primary) / `README.ja.md` (日本語)
- `docs/*.md` (English) / `docs/*.ja.md` (日本語) — `configuration` / `usage` /
  `clients` / `mcp-tools` / `behavior` / `ARCHITECTURE` ほか、全ページが英日ペア
- `docs/decisions/NNNN-*.md` (English) / `docs/decisions/NNNN-*.ja.md` (日本語)
- `CLAUDE.md` (本ファイル、日本語): Claude Code 向け開発ガイダンス

コード内のコメント・テスト名は英語基調。ただし日本語 KB 処理に関する箇所 (日本語 trigram、CJK 正規化等) では日本語コメントも可。外部コントリビュータへの説明は英語、内部議論 (issue / PR 含む) は日本語でも可。
