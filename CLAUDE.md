# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## プロジェクト概要

Markdown / プレーンテキストのナレッジベースに対するセマンティック検索を提供する MCP (Model Context Protocol) サーバ。YAML frontmatter 付きの Markdown (および opt-in で `.txt`) を見出し単位でチャンク化し、選択可能な埋め込みモデル (BGE-small-en-v1.5 / BGE-M3) でベクトル化。sqlite-vec のベクトル検索と FTS5 全文検索を Reciprocal Rank Fusion で融合し、任意で cross-encoder reranker を適用する。stdio または Streamable HTTP トランスポートで Claude Code / Cursor 等の MCP クライアントに接続する。

詳細:
- ユーザ向けドキュメント: [README.md](./README.md) (English) / [README.ja.md](./README.ja.md) (日本語)
- ソース構造: [docs/ARCHITECTURE.md](./docs/ARCHITECTURE.md) (English) / [docs/ARCHITECTURE.ja.md](./docs/ARCHITECTURE.ja.md) (日本語)

## ビルド・テスト

```bash
cargo build --release                    # release バイナリ: target/release/kb-mcp(.exe)
cargo check                              # 型検査のみ (高速)
cargo test                               # 軽量テスト (embedding DL 不要なもののみ)
cargo test -- --ignored                  # 実モデル DL を伴う embedding / reranker テスト
                                         # (BGE-small ~130 MB / BGE-M3 ~2.3 GB / BGE-reranker-v2-m3 ~2.3 GB)
```

Windows では `kb-mcp.exe` になる。ONNX runtime (`ort-sys`) は静的リンクされるため**追加の DLL は不要**。SQLite も `rusqlite` の `bundled` feature で同梱。

## 主要サブコマンド

`index` / `status` / `serve` / `search` / `graph` / `validate` / `eval`。フラグの詳細、`kb-mcp.toml` 設定、`.mcp.json` 接続例は README を参照。

## CLI 出力規約 (= stdout/stderr の責務分離)

`kb-mcp` の各 subcommand は出力先を以下の規約で使い分ける:

- **stdout** = data output 専用 (= machine-parseable な結果の出力先)
  - `kb-mcp search` の JSON 結果
  - `kb-mcp eval` の golden query 評価結果
- **stderr** = status / progress / 診断 (= 人間向けの進捗 / warning / error)
  - `kb-mcp index` の `Indexing ...` / `Done in ...` 進捗
  - `kb-mcp status` の `Documents: N` / `Chunks: N` 統計
  - すべての warning / info / error メッセージ (`tracing` / `eprintln!`)

**新規 subprocess test を書く時の注意**: subcommand の出力先を `src/main.rs` の `Commands::*` block で `println!` (stdout) か `eprintln!` (stderr) かを必ず先に grep 確認すること。`Commands::Search` 以外は基本 stderr に出る (= F-67 で `kb-mcp status` を stdout から読もうとして fail した過去あり)。

## 運用の細則

- **`Cargo.lock` はコミットする** (binary crate)
- **`.kb-mcp.db` はクライアントプロジェクト側の責務**。本リポジトリでは生成しない
- **テストは 2 層構造**: 通常 `cargo test` では `#[ignore]` の embedding 実行テストはスキップされる。CI 等で検証したければ `-- --ignored` を付ける
- **staging 禁止ファイル**: `.mcp.json` (ローカルパス)、`kb-mcp.toml` (ユーザ設定) は `.gitignore` 済み。テンプレートは `.mcp.json.example` / `kb-mcp.toml.example`

## Embedding モデルのキャッシュ

`src/embedder.rs::resolve_cache_dir()` が以下の順でキャッシュディレクトリを決定する:

1. `FASTEMBED_CACHE_DIR` 環境変数 (最優先)
2. OS 標準キャッシュディレクトリ + `fastembed`
   - Linux: `~/.cache/fastembed`
   - macOS: `~/Library/Caches/fastembed`
   - Windows: `%LOCALAPPDATA%\fastembed`
3. `.fastembed_cache/` (CWD 直下、最終フォールバック)

初回実行時に HuggingFace hub 互換キャッシュが作られる (BGE-small: ~130 MB、BGE-M3: ~2.3 GB、BGE-reranker-v2-m3: ~2.3 GB)。2 回目以降は再 DL されない。TLS 接続エラー時は README の "Working around HuggingFace TLS failures" 節の迂回手順を参照。

## 言語方針

本プロジェクトは**英語プライマリの日英バイリンガル**運用:
- `README.md` (English, primary) / `README.ja.md` (日本語)
- `docs/ARCHITECTURE.md` (English) / `docs/ARCHITECTURE.ja.md` (日本語)
- `CLAUDE.md` (本ファイル、日本語): Claude Code 向け開発ガイダンス

コード内のコメント・テスト名は英語基調。ただし日本語 KB 処理に関する箇所 (日本語 trigram、CJK 正規化等) では日本語コメントも可。外部コントリビュータへの説明は英語、内部議論 (issue / PR 含む) は日本語でも可。
