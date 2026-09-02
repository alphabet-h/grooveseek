# groove: Claude Code PostToolUse hook サンプル

Claude Code の [PostToolUse hook](https://docs.claude.com/en/docs/claude-code/hooks) を使うと、エージェントが write / edit / skill を実行した後に自動で `groove index` を走らせられる。これによりユーザがインデックスを手動で再実行しなくても、検索インデックスがナレッジベースと同期し続ける。

> **English version**: [README.md](./README.md)

## ファイル

| ファイル | 用途 |
|---|---|
| `settings.snippet.json` | プロジェクトの `.claude/settings.json` にコピーする最小 `hooks` ブロック — **完全な settings ファイルではない**。`Write` / `Edit` / `MultiEdit` / `Skill` 実行後に無条件で index 再構築する。**その `groove index` は config を名指ししない。プロジェクト内の `groove.toml` と併用する前に Tier A の注記を読むこと** |
| `rebuild-on-edit.sh` | tool payload を精査して、編集ファイルが `$KB_PATH` 配下のときだけ再構築する、より高機能なシェル hook。Claude Code プロジェクトがナレッジベース外のファイルも触る場合に推奨。Unix ライクなシェル (bash + jq) が必要。Windows ユーザは Git Bash または WSL から実行すること |

**`Skill` matcher に関する注意**: 執筆時点 (Claude Code v1.x) では skill は `Skill` ツール経由で公開されている。インストール済みの Claude Code バージョンでこのツールが rename / split された場合は、matcher を合わせて調整する — groove 本体はツール名に依存していない。

## Tier A — 無条件再構築 (最もシンプル)

以下を他の設定と並べて `.claude/settings.json` に配置:

<!-- groove-pin: posttooluse-hook-snippet -->
```json
{
  "hooks": {
    "PostToolUse": [
      {
        "matcher": "Write|Edit|MultiEdit|Skill",
        "hooks": [
          { "type": "command", "command": "groove index" }
        ]
      }
    ]
  }
}
```

`groove index` は SHA-256 の content hash 差分検出を使うため、変更されていないファイルはスキップされる。実際、小さな KB では 2 回目以降は 1 秒未満で終わる。バイナリが `PATH` 上に無いなら `groove` を絶対パスに置き換える。

`kb_path` は `groove.toml` から読まれる (探索順は [「設定ファイルの探索順」](../../../docs/configuration.ja.md#設定ファイルの探索順)を参照、通常はプロジェクトルートかバイナリの隣)。`groove index --kb-path /abs/path/to/knowledge-base` のようにハードコードもできる。

> **その `groove.toml` がバイナリの隣ではなくプロジェクト内にあるなら、ここでも名指しすること**:
> `groove --config /abs/path/to/groove.toml index`。groove は**見つけただけ**の config を
> 一部しか尊重せず、`[parsers]` は既定 (Markdown のみ) へ戻されるキーの 1 つ
> ([信頼する置き場所 / しない置き場所](../../../docs/configuration.ja.md#信頼する置き場所--しない置き場所))。
> これは「再構築が不完全になる」では済まない — **`groove index` は訪れなかった document を削除する**ので、
> `.md` しか集めない run は索引済みの `.txt` / PDF / Office 文書 / ソースコードをすべて消す。
> しかもこの hook は次の編集で発火するので、誰も再 index を決断しないまま起きる。
> `settings.snippet.json` も同じ minimal form なので同じ変更が要る。
> config がバイナリの隣にある場合や `groove service install` が置いた場合は何も足さなくてよい
> (信頼される置き場)。**存在しないファイルを指す `--config` は足さないこと** — エラーであって
> フォールバックではない。

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

スクリプトは stdin から hook payload を読み、(`jq` が利用可能なら) 編集されたファイルパスを抽出し、編集対象が `$KB_PATH` 配下の **index 対象ファイル**のときのみ `groove index` を呼ぶ。`Skill` 呼び出しは payload にファイルパスが無いため、無条件再構築にフォールスルーする (差分検出があるので安価)。

対象拡張子は `KB_EXTENSIONS` で決まり、既定は**既定ビルドが** parse できる全形式 (`md txt pdf docx xlsx pptx rs`、大小文字は無視するので `Report.PDF` も対象。`py` は grammar plugin を先に置く必要があるので既定から外してある)。**`groove.toml` の `[parsers].enabled` と揃えること**: 多めに書いても no-op rebuild のコストだけだが、少なく書くとその形式の編集で rebuild が **走らない**。

```json
"command": "KB_PATH=/abs/path/to/knowledge-base KB_EXTENSIONS='md txt' /abs/path/to/rebuild-on-edit.sh"
```

### `GROOVE_CONFIG` — `groove.toml` がプロジェクトの隣にあるなら設定する

`groove index` も他のコマンドと同じように config を discover し、見つけただけの config は
一部しか効かない — `[parsers]` は Markdown のみへ戻される
([信頼する置き場所 / しない置き場所](../../../docs/configuration.ja.md#信頼する置き場所--しない置き場所))。
ここではそれが「不完全」で済まない。**`groove index` は訪れなかった document を削除する**ので、
`.md` しか集めない rebuild は、索引済みの `.txt` / PDF / Office 文書 / ソースコードを
すべて消す — しかも hook は次の編集で、誰も実行を決めないまま発火する。

`groove.toml` がプロジェクトの隣にあるなら名指しすること:

```json
"command": "KB_PATH=/abs/path/to/knowledge-base GROOVE_CONFIG=/abs/path/to/groove.toml /abs/path/to/rebuild-on-edit.sh"
```

`groove` バイナリの隣にある場合や `groove service install` が置いた場合は未設定でよい
(信頼される置き場なので何も戻されない)。スクリプトはファイルの存在を確認し、無ければ
`--config path not found` で毎回の編集ごとに hook を失敗させる代わりに、メッセージを出して
rebuild をスキップする。

## 補足

- **並行実行**: SQLite は WAL モードで構成されているため、起動中の MCP サーバと hook トリガーの `groove index` が共存できる。hook は rebuild 完了までツール実行をブロックするが、小さな KB では気にならないほど速い
- **品質フィルタ**: rebuild は `groove.toml` の `[quality_filter]` を尊重する。backfill は `groove index` の冒頭で毎回走るが冪等
- **一時的にスキップ**: hook を削除せず無効化したいときは、Tier B なら `KB_PATH=` (空) に、Tier A ならエントリをコメントアウトする
