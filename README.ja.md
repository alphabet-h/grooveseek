# GrooveSeek

Markdown / プレーンテキストのナレッジベースに対するセマンティック検索を提供する MCP サーバ。コマンド名は `groove`。

YAML frontmatter 付きの Markdown (および任意で `.txt` / `.pdf` / `.docx` / `.xlsx` / `.pptx`) をパースし、見出し単位でチャンク化、選択可能な埋め込みモデル (既定は BGE-small-en-v1.5、多言語 / 日本語向けには BGE-M3) でベクトルを生成して、sqlite-vec 搭載の SQLite に格納する。stdio (既定、1 クライアント) または Streamable HTTP (複数クライアント) トランスポート経由で Claude Code / Cursor など MCP 互換クライアントに接続する。

ライブ同期ファイルウォッチャにより、手動編集・`git pull`・外部スクリプトによる変更でもインデックスが最新に保たれる。`groove validate` で任意の TOML スキーマに基づく frontmatter 検証も可能。

> **English version**: [README.md](./README.md)

**バージョニング**: 1.0.0 より前のリリースは beta であり、互換性の保証は無い。
1.0.0 以降、本プロジェクトが壊さないと約束するもの — および web 画面と Rust API を
含む、**意図的に約束しないもの** — は [docs/stability.ja.md](./docs/stability.ja.md)
に書き下してある。

## インストール

### ビルド済みバイナリ (非 Rust ユーザ向け推奨)

[最新リリース](https://github.com/alphabet-h/grooveseek/releases/latest) から自分の OS / アーキテクチャ用のアーカイブを DL → 展開 → `groove` (Windows では `groove.exe`) を `PATH` の通った場所に配置するだけ。対応ターゲット:

| プラットフォーム | アーカイブ |
| --- | --- |
| Linux x86_64 (glibc 2.38+ / Ubuntu 24.04+ / Debian 13+ / RHEL 9.5+) | `grooveseek-x86_64-unknown-linux-gnu.tar.xz` |
| Linux aarch64 (glibc 2.38+) | `grooveseek-aarch64-unknown-linux-gnu.tar.xz` |
| macOS Apple Silicon | `grooveseek-aarch64-apple-darwin.tar.xz` |
| Windows x86_64 (Windows 10+) | `grooveseek-x86_64-pc-windows-msvc.zip` |

> **Intel Mac (`x86_64-apple-darwin`)** はビルド済バイナリを配布していない: 上流 ONNX Runtime crate (`ort-sys`) がこのターゲット用 prebuilt を提供しないため。下記「ソースからビルド」を参照。

> **Windows で service として動かすなら、archive がもう 2 つ要る。** どちらも別ダウンロード (v0.14.0 以降) で、`groove.exe` と**同じディレクトリ**に展開する:
>
> | Archive | 理由 |
> | --- | --- |
> | `groove-svc-x86_64-pc-windows-msvc.zip` | `groove service install` は `groove-svc.exe` が `groove.exe` の隣にあればそれを logon task の起動対象にし、**無ければ console 可視の launcher に fallback する** — 毎回のログオンでコンソール窓が一瞬出る。fallback したことは warning で報告されるが、`service install` の**前に**展開しておけば入れ直さずに済む。 |
> | `groove-tray-x86_64-pc-windows-msvc.zip` | 任意。system tray 監視 binary で、`service install --with-tray` を使う場合のみ必要。 |

各アーカイブにはバイナリの他に `CHANGELOG.md` / `LICENSE-MIT` / `LICENSE-APACHE` / `README.md` が同梱される。実行前にリリースに添付された `sha256.sum` または各アーカイブ用 `*.sha256` で SHA-256 チェックサムを照合すること。

ONNX runtime と SQLite はバイナリに静的リンクされているので、追加 DLL は不要。Embedding モデル (ONNX) は初回実行時に HuggingFace から DL される — ネットワークがそれをブロックする場合は [HuggingFace の TLS 失敗への対処](docs/clients.ja.md#huggingface-の-tls-失敗への対処-初回-dl-時) を参照。

### ソースからビルド

```bash
cargo build --release
```

バイナリは `target/release/groove` (Windows では `groove.exe`) に生成される。

## クイックスタート

ナレッジベースを索引し、MCP クライアントを向ける:

```bash
groove index --kb-path /path/to/knowledge-base
```

```json
{
  "mcpServers": {
    "ai-knowledge": {
      "command": "/path/to/groove",
      "args": ["serve", "--kb-path", "/path/to/knowledge-base"],
      "type": "stdio"
    }
  }
}
```

置き場所はプロジェクトルートの `.mcp.json` (またはクライアント対応の MCP 設定場所)。
クライアントを立てずに直接引くこともできる:

```bash
groove search "semantic chunking" --kb-path /path/to/knowledge-base --limit 3
```

日本語を含む多言語のナレッジベースなら `index` と `serve` の両方に `--model bge-m3`
を渡す。DL 量も索引も変わるので、索引を作る前に決めておく価値がある — トレードオフは
[docs/usage.ja.md](docs/usage.ja.md) にある。

## ドキュメント

| ページ | 内容 |
| --- | --- |
| [docs/usage.ja.md](docs/usage.ja.md) | 全コマンド: `index` / `serve` / `search` / `graph` / `validate` / `doctor` / `eval` / `tune` / `service` |
| [docs/configuration.ja.md](docs/configuration.ja.md) | `groove.toml` の全キー、探索順、どの置き場所を信頼するか |
| [docs/clients.ja.md](docs/clients.ja.md) | `.mcp.json` のレシピ、HTTP トランスポート、PostToolUse hook、file watcher |
| [docs/mcp-tools.ja.md](docs/mcp-tools.ja.md) | MCP の面 — ツール / プロンプト / `kb://` リソース |
| [docs/behavior.ja.md](docs/behavior.ja.md) | 何が索引され、どこに保存され、どのファイルが拒否されるか |
| [docs/retrieval-pipeline.ja.md](docs/retrieval-pipeline.ja.md) | RRF / reranker / MMR / parent retriever を実行順に |
| [docs/filters.ja.md](docs/filters.ja.md) | 検索結果の絞り込み |
| [docs/citations.ja.md](docs/citations.ja.md) | `match_spans` とバイトオフセット (出典を正確に引用するため) |
| [docs/eval.ja.md](docs/eval.ja.md) | Golden query セットに対する retrieval 品質の測定 |
| [docs/ARCHITECTURE.ja.md](docs/ARCHITECTURE.ja.md) | ソース構成と、クエリがそこをどう流れるか |
| [docs/stability.ja.md](docs/stability.ja.md) | 1.0.0 が凍らせるもの、意図的に凍らせないもの |

上記すべてに英語版が同じディレクトリに並んでいる (`.ja` を外した名前)。
デプロイの完全なレシピ (個人 stdio / NAS 共有 / 社内 HTTP) は
[`grooveseek/examples/deployments/`](./grooveseek/examples/deployments/)。

これらはリポジトリ上のパスであり、リリースアーカイブにはこの README は入るが
`docs/` は入らない。アーカイブから読んでいる場合は
<https://github.com/alphabet-h/grooveseek/tree/main/docs> を参照する
(`main` をインストールしたリリースの tag に差し替えるとその版になる)。

## MCP の面

6 つのツール (`search` / `get_document` / `list_topics` / `get_connection_graph` /
`get_best_practice` / `rebuild_index`)、4 つのプロンプト、そしてナレッジベース自体を
`kb://` リソースとして公開する。パラメータと戻り値の形は
[docs/mcp-tools.ja.md](docs/mcp-tools.ja.md)。

## 設計判断の記録

アーキテクチャを形づくった決定 — 何を選び、どの選択肢を却下し、その代償は何だったか — は [Architecture Decision Record](docs/decisions/) として `docs/decisions/` に残している。まず [ADR-0000](docs/decisions/0000-record-decisions-as-adrs.ja.md) を読むと、何を ADR に書き、何は CHANGELOG で足りるのかが分かる。日本語版は同じディレクトリに `*.ja.md` で並べてある。
