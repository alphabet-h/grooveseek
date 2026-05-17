# デプロイメントレシピ — 個人ローカル

> **English version**: [README.md](./README.md)

単一ユーザ / 単一マシン / ローカル KB。最も一般的かつ最小構成。すべてが
手元のマシンで完結し、ファイルウォッチャーがインデックスを自動同期、
Claude Code は stdio 経由で kb-mcp を起動する。

## 想定環境

- 1 人のユーザが 1 台のマシンで使う
- KB はローカルディレクトリ (Obsidian vault、プロジェクトノート、研究メモ等)
- Claude Code / Cursor 等の MCP クライアントが同じマシンから stdio で kb-mcp に接続

## このディレクトリの中身

| ファイル | 用途 |
| --- | --- |
| [`kb-mcp.toml`](./kb-mcp.toml) | サーバ側既定値: model / watcher / parsers / quality filter |
| [`.mcp.json`](./.mcp.json) | クライアント側設定: `kb-mcp serve` (引数なし、toml から discover) |

## セットアップ

1. **kb-mcp をインストール**。[ビルド済バイナリ](https://github.com/alphabet-h/kb-mcp/releases/latest) を `PATH` の通った場所に置くか、clone から `cargo install --path .`
2. **KB の置き場所を決める**。例: `~/notes/` (個人ノート) や `~/projects/<repo>/docs/` (プロジェクト単位)
3. **設定ファイルの置き場所**。自然な選択肢は 2 つ — [Config file discovery](../../../README.ja.md#設定ファイルの探索順) を参照:
   - **プロジェクト単位**: `kb-mcp.toml` と `.mcp.json` を一緒にプロジェクトリポジトリに置いて commit (toml は共有前提に作られている)
   - **グローバル**: `kb-mcp.toml` をバイナリの隣 (`~/.local/bin/kb-mcp.toml` や `%USERPROFILE%\bin\kb-mcp.toml`) に置けば全プロジェクトで同じ設定を共有
4. **`kb-mcp.toml` を編集**: `kb_path` を KB の絶対パスに。言語が合わなければ model と reranker を調整
5. **初回インデックス構築**:

   ```bash
   kb-mcp index --kb-path /absolute/path/to/kb
   ```

   初回は ONNX モデルを DL する。2 回目以降は SHA-256 差分で増分のみ
6. **Claude Code から接続**: `.mcp.json` をプロジェクトルート (または `~/.config/claude/.mcp.json`) にコピー

## 運用上の注意

- **Watcher** は既定で有効。`.md` の保存 / `git pull` / 外部スクリプトによる変更も ~500 ms 以内に自動再インデックス
- **PostToolUse hook** はオプション、watcher と相補的 — [`examples/hooks/`](../../hooks/) 参照。watcher が手動編集をカバーするので、hook の価値は「Claude 自身がファイルを書いた直後にゼロレイテンシで再構築したい」場合に限られる
- **Reranker** はロードのみで既定 off。MCP の `search` 呼び出しに `rerank: true` を渡して per-query で有効化する想定 (CPU で ~300-700 ms のレイテンシ税は毎回払うほどでない)
- **1 サーバ : 1 クライアント**。stdio は 1 接続のみ — 個人用途なら十分。複数クライアントが必要なら [`intranet-http/`](../intranet-http/) へ
- **`alwaysLoad: true`** はサンプル `.mcp.json` に入れている Claude Code v2.1.121+ のオプション。tool-search ショートリストを介さず initial load で kb-mcp のツールを必ず含めるようにする。RAG 用途 (「いつでも検索したい」) では推奨。初回起動コスト (モデル DL / index open) を抑えたい / クライアントが v2.1.121 未満なら削除可。他 MCP クライアントは未知フィールドとして無視

## 次のレシピへの移行サイン

- チームメンバと KB を共有したい → [`nas-shared/`](../nas-shared/) または [`intranet-http/`](../intranet-http/)
- 同じ KB に複数 Claude Code セッションを並列で叩く → [`intranet-http/`](../intranet-http/)
- KB がネットワーク共有上にある → [`nas-shared/`](../nas-shared/)
