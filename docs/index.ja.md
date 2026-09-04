<picture>
  <source media="(prefers-color-scheme: dark)" srcset="https://github.com/alphabet-h/grooveseek/raw/main/assets/grooveseek-readme-hero-dark-v2.webp">
  <img src="https://github.com/alphabet-h/grooveseek/raw/main/assets/grooveseek-readme-hero-light-v2.webp" alt="Markdown ファイルがチャンク化を経て、意味の経路と字句の経路が 1 点に集まり、順位付けされた結果が MCP クライアントへ抜けていく図。" width="100%">
</picture>

# GrooveSeek ドキュメント

Markdown / プレーンテキストのナレッジベースに対するセマンティック検索を提供する
MCP サーバ。コマンド名は `groove`。

> **English version**: [index.md](./index.md)

**導入と最初の 1 検索までは表紙にあります**:
[github.com/alphabet-h/grooveseek](https://github.com/alphabet-h/grooveseek)。
以下はリファレンスです。

各ページに英語版と日本語版があり、冒頭で相互にリンクしています。

## リファレンス

| | English | 日本語 |
| --- | --- | --- |
| 全サブコマンド — `index`, `status`, `serve`, `search`, `graph`, `validate`, `doctor`, `eval`, `tune`, `service` | [usage.md](usage.md) | [usage.ja.md](usage.ja.md) |
| `groove.toml` の全キー、探索順、どの場所が信頼されるか | [configuration.md](configuration.md) | [configuration.ja.md](configuration.ja.md) |
| `.mcp.json` のレシピ、HTTP トランスポート、PostToolUse フック、ファイル監視 | [clients.md](clients.md) | [clients.ja.md](clients.ja.md) |
| MCP の面: ツール / プロンプト / `kb://` リソース | [mcp-tools.md](mcp-tools.md) | [mcp-tools.ja.md](mcp-tools.ja.md) |
| 何が索引に入り、どこに保存され、どのファイルが拒否されるか | [behavior.md](behavior.md) | [behavior.ja.md](behavior.ja.md) |
| どのプロセスの形で配置するか、常駐が何を買うか、同一ホスト制約がどこから来るか | [deployment-topologies.md](deployment-topologies.md) | [deployment-topologies.ja.md](deployment-topologies.ja.md) |

## 検索

| | English | 日本語 |
| --- | --- | --- |
| RRF、reranking、MMR、parent retriever を実行順に | [retrieval-pipeline.md](retrieval-pipeline.md) | [retrieval-pipeline.ja.md](retrieval-pipeline.ja.md) |
| 検索結果を絞り込む | [filters.md](filters.md) | [filters.ja.md](filters.ja.md) |
| `match_spans` とバイトオフセット — 出典を正確に引用するために | [citations.md](citations.md) | [citations.ja.md](citations.ja.md) |
| golden query set に対して検索品質を測る | [eval.md](eval.md) | [eval.ja.md](eval.ja.md) |

## プロジェクト

| | English | 日本語 |
| --- | --- | --- |
| ソース構造と、クエリがそこをどう流れるか | [ARCHITECTURE.md](ARCHITECTURE.md) | [ARCHITECTURE.ja.md](ARCHITECTURE.ja.md) |
| 1.0.0 が凍結するもの、および意図的に凍結しないもの | [stability.md](stability.md) | [stability.ja.md](stability.ja.md) |

## 決定

Architecture Decision Record — 何を選び、どの代替案を退け、その代償は何だったか。
どこまでが ADR に残す決定で、どこからは changelog で足りるかは
[ADR-0000](decisions/0000-record-decisions-as-adrs.ja.md) にあります。

| | English | 日本語 |
| --- | --- | --- |
| 0. アーキテクチャ上重要な決定を ADR として記録する | [en](decisions/0000-record-decisions-as-adrs.md) | [ja](decisions/0000-record-decisions-as-adrs.ja.md) |
| 1. `.xls` (レガシー BIFF) 対応を取り下げる | [en](decisions/0001-withdraw-xls-legacy-biff-support.md) | [ja](decisions/0001-withdraw-xls-legacy-biff-support.ja.md) |
| 2. クエリを全文検索用のトークン単位 `OR` フレーズにコンパイルする | [en](decisions/0002-compile-queries-into-per-token-fts-phrases.md) | [ja](decisions/0002-compile-queries-into-per-token-fts-phrases.ja.md) |
| 3. `.kb-mcpignore` が縛るのは索引であってアクセスではない。`ignore` はマッチャとしてのみ使う | [en](decisions/0003-kb-mcpignore-bounds-indexing-not-access.md) | [ja](decisions/0003-kb-mcpignore-bounds-indexing-not-access.ja.md) |
| 4. リソース読み出しの境界はファイルシステムではなく索引 | [en](decisions/0004-resource-reads-are-bounded-by-the-index.md) | [ja](decisions/0004-resource-reads-are-bounded-by-the-index.ja.md) |
| 5. 各ドキュメントのサイズを索引に記録する | [en](decisions/0005-record-document-size-in-the-index.md) | [ja](decisions/0005-record-document-size-in-the-index.ja.md) |
| 6. golden set を引用しているコーパスを報告し、引用が 1 つでは足りないことにする | [en](decisions/0006-report-a-corpus-that-quotes-the-golden-set.md) | [ja](decisions/0006-report-a-corpus-that-quotes-the-golden-set.ja.md) |
| 7. プロジェクトを GrooveSeek に改名し、コマンドは `groove` にする | [en](decisions/0007-rename-the-project-to-grooveseek.md) | [ja](decisions/0007-rename-the-project-to-grooveseek.ja.md) |
| 8. 1.0.0 が凍結するものを宣言し、Rust API はその外に置く | [en](decisions/0008-declare-what-1-0-freezes.md) | [ja](decisions/0008-declare-what-1-0-freezes.ja.md) |
| 9. DNS リバインディングのゲートを 1 つにし、こちらで持つ | [en](decisions/0009-one-dns-rebinding-gate.md) | [ja](decisions/0009-one-dns-rebinding-gate.ja.md) |
| 10. ADR-0008 が開けたままにしたコマンドラインの 3 問に決着を付ける | [en](decisions/0010-settle-what-the-1-0-command-line-freezes.md) | [ja](decisions/0010-settle-what-the-1-0-command-line-freezes.ja.md) |
| 11. ハイブリッド検索の両脚から語を除外する | [en](decisions/0011-exclude-a-term-from-both-halves-of-the-search.md) | [ja](decisions/0011-exclude-a-term-from-both-halves-of-the-search.ja.md) |
| 12. コードは定義で切り、覆えなかった範囲は行で埋める | [en](decisions/0012-chunk-code-at-its-definitions-and-fill-the-gaps-by-line.md) | [ja](decisions/0012-chunk-code-at-its-definitions-and-fill-the-gaps-by-line.ja.md) |
| 13. grammar は 1 つだけ焼き込み、残りは読み込む | [en](decisions/0013-compile-in-one-grammar-and-load-the-rest.md) | [ja](decisions/0013-compile-in-one-grammar-and-load-the-rest.ja.md) |
| 14. chunker は時計ではなく入力の形で縛る | [en](decisions/0014-bound-the-chunker-by-the-shape-of-its-input.md) | [ja](decisions/0014-bound-the-chunker-by-the-shape-of-its-input.ja.md) |
| 15. 定義は短くてよい | [en](decisions/0015-let-a-definition-be-short.md) | [ja](decisions/0015-let-a-definition-be-short.ja.md) |
| 16. plugin の置き場は知識ベースの外に置く | [en](decisions/0016-keep-the-plugin-directory-outside-the-knowledge-base.md) | [ja](decisions/0016-keep-the-plugin-directory-outside-the-knowledge-base.ja.md) |

ADR-0003 のファイル名は今も `kb-mcpignore` のままです。そこで説明されているファイルは
現在 `.grooveignore` ですが、**ADR は merge 後に編集しません**。2026-08-17 より前の
日付のものに出てくる `kb-mcp` が本プロジェクトを指す理由は
[ADR-0007](decisions/0007-rename-the-project-to-grooveseek.ja.md) にあります。

ADR-0000 は `.dev/` を決定記録の置き場から外す理由の 1 つとして「nested repository が
無く、backup も無い」を挙げていますが、これは 2026-08-10 に `.dev/` が private mirror を
持った時点で事実ではなくなりました。**決定自体は変わりません** — 変わっていない残り 2 つの
理由、すなわち「clone に付いて来ない」「公開ドキュメントから参照できない」がそのまま
成立するからです。記録は ADR-0003 のファイル名と同じ理由で、書かれたまま残してあります。
