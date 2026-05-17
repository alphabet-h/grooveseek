# kb-mcp: Claude Code PostToolUse hook サンプル

Claude Code の [PostToolUse hook](https://docs.claude.com/en/docs/claude-code/hooks) を使うと、エージェントが write / edit / skill を実行した後に自動で `kb-mcp index` を走らせられる。これによりユーザがインデックスを手動で再実行しなくても、検索インデックスがナレッジベースと同期し続ける。

> **English version**: [README.md](./README.md)

## ファイル

| ファイル | 用途 |
|---|---|
| `settings.snippet.json` | プロジェクトの `.claude/settings.json` にコピーする最小 `hooks` ブロック — **完全な settings ファイルではない**。`Write` / `Edit` / `MultiEdit` / `Skill` 実行後に無条件で index 再構築する |
| `rebuild-on-edit.sh` | tool payload を精査して、編集ファイルが `$KB_PATH` 配下のときだけ再構築する、より高機能なシェル hook。Claude Code プロジェクトがナレッジベース外のファイルも触る場合に推奨。Unix ライクなシェル (bash + jq) が必要。Windows ユーザは Git Bash または WSL から実行すること |

**`Skill` matcher に関する注意**: 執筆時点 (Claude Code v1.x) では skill は `Skill` ツール経由で公開されている。インストール済みの Claude Code バージョンでこのツールが rename / split された場合は、matcher を合わせて調整する — kb-mcp 本体はツール名に依存していない。

## Tier A — 無条件再構築 (最もシンプル)

以下を他の設定と並べて `.claude/settings.json` に配置:

```json
{
  "hooks": {
    "PostToolUse": [
      {
        "matcher": "Write|Edit|MultiEdit|Skill",
        "hooks": [
          { "type": "command", "command": "kb-mcp index" }
        ]
      }
    ]
  }
}
```

`kb-mcp index` は SHA-256 の content hash 差分検出を使うため、変更されていないファイルはスキップされる。実際、小さな KB では 2 回目以降は 1 秒未満で終わる。バイナリが `PATH` 上に無いなら `kb-mcp` を絶対パスに置き換える。

`kb_path` は `kb-mcp.toml` から読まれる (探索順は README の「設定ファイルの探索順」を参照、通常はプロジェクトルートかバイナリの隣)。`kb-mcp index --kb-path /abs/path/to/knowledge-base` のようにハードコードもできる。

## Tier B — パスフィルタ付き再構築 (スクリプト)

プロジェクトがナレッジベース外のファイルも編集する場合、`rebuild-on-edit.sh` を使うと関係ない編集で hook が黙ったままになる。

1. `rebuild-on-edit.sh` を適当な場所にコピー (例: `~/.local/bin/`) して実行権を付与: `chmod +x rebuild-on-edit.sh`
2. `KB_PATH` に `knowledge-base/` ディレクトリの絶対パスを設定 (空のままだとスクリプトは早期終了)
3. `.claude/settings.json` で配線:

```json
{
  "hooks": {
    "PostToolUse": [
      {
        "matcher": "Write|Edit|MultiEdit|Skill",
        "hooks": [
          {
            "type": "command",
            "command": "KB_PATH=/abs/path/to/knowledge-base /abs/path/to/rebuild-on-edit.sh"
          }
        ]
      }
    ]
  }
}
```

スクリプトは stdin から hook payload を読み、(`jq` が利用可能なら) 編集されたファイルパスを抽出し、編集対象が `$KB_PATH` 配下の `.md` ファイルのときのみ `kb-mcp index` を呼ぶ。`Skill` 呼び出しは payload にファイルパスが無いため、無条件再構築にフォールスルーする (差分検出があるので安価)。

## 補足

- **並行実行**: SQLite は WAL モードで構成されているため、起動中の MCP サーバと hook トリガーの `kb-mcp index` が共存できる。hook は rebuild 完了までツール実行をブロックするが、小さな KB では気にならないほど速い
- **品質フィルタ**: rebuild は `kb-mcp.toml` の `[quality_filter]` を尊重する。backfill は `kb-mcp index` の冒頭で毎回走るが冪等
- **一時的にスキップ**: hook を削除せず無効化したいときは、Tier B なら `KB_PATH=` (空) に、Tier A ならエントリをコメントアウトする
