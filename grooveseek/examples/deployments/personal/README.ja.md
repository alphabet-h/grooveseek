# デプロイメントレシピ — 個人ローカル

> **English version**: [README.md](./README.md)

単一ユーザ / 単一マシン / ローカル KB。最も一般的かつ最小構成。すべてが
手元のマシンで完結し、ファイルウォッチャーがインデックスを自動同期、
Claude Code は stdio 経由で groove を起動する。

## 想定環境

- 1 人のユーザが 1 台のマシンで使う
- KB はローカルディレクトリ (Obsidian vault、プロジェクトノート、研究メモ等)
- Claude Code / Cursor 等の MCP クライアントが同じマシンから stdio で groove に接続

## このディレクトリの中身

| ファイル | 用途 |
| --- | --- |
| [`groove.toml`](./groove.toml) | サーバ側既定値: model / watcher / parsers / quality filter |
| [`.mcp.json`](./.mcp.json) | クライアント側設定: `groove serve --config ./groove.toml` |

## セットアップ

1. **groove をインストール**。[ビルド済バイナリ](https://github.com/alphabet-h/grooveseek/releases/latest) を `PATH` の通った場所に置くか、clone から `cargo install --path grooveseek` (リポジトリ root は workspace manifest なので `--path .` は失敗する)
2. **KB の置き場所を決める**。例: `~/notes/` (個人ノート) や `~/projects/<repo>/docs/` (プロジェクト単位)
3. **設定ファイルの置き場所**。自然な選択肢は 2 つ — [Config file discovery](../../../../docs/configuration.ja.md#設定ファイルの探索順) を参照:
   - **プロジェクト単位**: `groove.toml` と `.mcp.json` を一緒にプロジェクトリポジトリに置いて commit (toml は共有前提に作られている)。**ここの `.mcp.json` が `--config` でファイルを名指ししているのは好みの問題ではない** — groove が**見つけただけ**の `groove.toml` は一部しか効かず、`[parsers]` は既定へ戻されるキーの 1 つなので、`.txt` / PDF / ソースコードを opt-in したプロジェクト単位の config が黙って Markdown だけを索引することになる。[信頼する置き場所 / しない置き場所](../../../../docs/configuration.ja.md#信頼する置き場所--しない置き場所) を参照
   - **グローバル**: `groove.toml` をバイナリの隣 (`~/.local/bin/groove.toml` や `%USERPROFILE%\bin\groove.toml`) に置けば全プロジェクトで同じ設定を共有。バイナリの隣は信頼される置き場なので `--config` は要らない — **この場合は `.mcp.json` から `"--config", "./groove.toml"` を削ること**。存在しないファイルを `--config` で名指しするのはエラーであって、discovery へのフォールバックではない
4. **`groove.toml` を編集**: `kb_path` を KB の絶対パスに。言語が合わなければ model と reranker を調整
5. **初回インデックス構築** — **`.mcp.json` と同じ理由で、ここでも config を名指しする**。`groove index` も他のコマンドと同じように config を discover するので、`--config` を落とすと `[parsers]` が既定へ戻った状態で最初の索引が作られ、しかも後から作り直されない (`serve` は見つけた索引を開くだけで再 index しない)。

   ```bash
   # プロジェクト単位: groove.toml のあるディレクトリで実行する
   groove index --config ./groove.toml --kb-path /absolute/path/to/kb
   ```

   ```bash
   # グローバル (バイナリの隣に groove.toml): 信頼される置き場なので --config は不要
   groove index --kb-path /absolute/path/to/kb
   ```

   初回は ONNX モデルを DL する。2 回目以降は SHA-256 差分で増分のみ
6. **Claude Code から接続**: `.mcp.json` をプロジェクトルート (または `~/.config/claude/.mcp.json`) にコピー

## 運用上の注意

- **Watcher** は既定で有効。`.md` の保存 / `git pull` / 外部スクリプトによる変更も ~500 ms 以内に自動再インデックス
- **PostToolUse hook** はオプション、watcher と相補的 — [`examples/hooks/`](../../hooks/) 参照。watcher が手動編集をカバーするので、hook の価値は「Claude 自身がファイルを書いた直後にゼロレイテンシで再構築したい」場合に限られる
- **Reranker** は本レシピでは **未設定** — 初回実行で ~2.3 GB のモデルを引かせないため `groove.toml` の `reranker` キーをコメントアウトしてある。コメントを外すまで `search` の `rerank: true` は **silent no-op** (サーバは起動時に reranker をロードしていた場合のみ rerank する)。有効化したら `rerank_by_default = false` のまま per-query で opt-in する運用にする (CPU では再ランク付きの検索に数十秒かかり、素の検索は 1 秒未満。毎回払うコストではない)。実測とその条件は [usage.ja.md](../../../../docs/usage.ja.md#再ランクを有効にすべきケース) にある
- **1 サーバ : 1 クライアント**。stdio は 1 接続のみ — 個人用途なら十分。複数クライアントが必要なら [`intranet-http/`](../intranet-http/) へ
- **`alwaysLoad: true`** はサンプル `.mcp.json` に入れている Claude Code v2.1.121+ のオプション。tool-search ショートリストを介さず initial load で groove のツールを必ず含めるようにする。RAG 用途 (「いつでも検索したい」) では推奨。初回起動コスト (モデル DL / index open) を抑えたい / クライアントが v2.1.121 未満なら削除可。他 MCP クライアントは未知フィールドとして無視

## 次のレシピへの移行サイン

- チームメンバと KB を共有したい → [`nas-shared/`](../nas-shared/) または [`intranet-http/`](../intranet-http/)
- 同じ KB に複数 Claude Code セッションを並列で叩く → [`intranet-http/`](../intranet-http/)
- KB がネットワーク共有上にある → [`nas-shared/`](../nas-shared/)
