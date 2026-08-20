# MCP ツール / プロンプト / リソース

GrooveSeek が接続クライアントに公開する MCP の面。

> **English version**: [mcp-tools.md](./mcp-tools.md)

## MCP ツール

| ツール | 説明 | 主なパラメータ |
|---|---|---|
| `search` | ベクトル + FTS5 全文検索を Reciprocal Rank Fusion でマージしたハイブリッド検索、任意で cross-encoder 再ランク + MMR 多様性再ランク + parent retriever 展開。`{ results, low_confidence, filter_applied }` ラッパで関連度ランク付き chunk を返す。parent retriever が発火した行には `expanded_from` も付く。詳細: [docs/citations.ja.md](citations.ja.md)、[docs/filters.ja.md](filters.ja.md)、[docs/retrieval-pipeline.ja.md](retrieval-pipeline.ja.md) | `query` (必須)、`limit`、`category`、`topic`、`rerank` (サーバ既定を上書き)、`min_quality`、`include_low_quality`、`path_globs` (`!` 始まりは exclude)、`tags_any` / `tags_all`、`date_from` / `date_to` (`YYYY-MM-DD`)、`min_confidence_ratio`、`mmr` / `mmr_lambda` / `mmr_same_doc_penalty` (v0.7.0+)、`parent_retriever` (v0.7.0+) |
| `list_topics` | index 済みの全トピック / カテゴリと文書数を列挙 | (なし) |
| `get_document` | 相対パスから文書の全文 + メタデータを取得 | `path` (例: `"deep-dive/mcp/overview.md"`) |
| `get_best_practice` | opt-in: `groove.toml` の `[best_practice].path_templates` を設定しているときのみ機能する。対象向けの best practice 文書を取得し、任意で特定 h2 セクションを抽出。未設定時は "not configured" エラーを返す | `target` (例: `"claude-code"`)、`category` (任意) |
| `rebuild_index` | すべてのソースファイル (Markdown + `[parsers].enabled` で有効化された拡張子) を走査してインデックス再構築。**同時に 1 本だけ** (v0.28.0+): 実行中に来た呼び出しは「実行中のものが何秒前に始まったか」を添えたエラーで断る。再構築は embedder と DB を握り続けるので、終わるまで検索が使えないため。制限がかかるのはこのツールで、`groove index` は別プロセスなので対象外 | `force` (任意、既定 false) |
| `get_connection_graph` | ドキュメントパスを起点に意味的に関連するチャンクを BFS 展開。`parent_id` / `depth` / `score` / `snippet` 付きのノード配列を返し、呼び出し側でコンテキスト発見を連鎖させられる。上限で探索が切られた場合は `truncated` / `truncation[]` が付く | `start` (必須、探索の起点パス — `groove graph --start`)、`depth` (既定 2、最大 3)、`fan_out` (既定 5、最大 20)、`min_similarity` (既定 0.3)、`seed_strategy` (`all_chunks` / `centroid`。`all-chunks` も受け付ける)、`dedup_by_path`、`category`、`topic`、`exclude_paths`、`max_nodes` (既定 100、最大 2000)、`max_seed_chunks` (既定 32、最大 1000) |

## MCP プロンプト

(v0.22.0+) 4 つの prompt を同梱している。クライアントはこれを**ユーザが選ぶコマンド**として出す (Claude Code では `/mcp__groove__<name>`)。存在理由は「ツールだけでは組み合わせ方が分からない」こと — `search` は「次に `get_connection_graph` を呼べ」とも「`low_confidence` が立ったらそう言え」とも言わない。

| Prompt | 引数 | 何を指示するか |
|---|---|---|
| `summarize_topic` | `topic` (必須) | `list_topics` でトピックの存在を確認 → `search` で集める → 重要な文書は `get_document` で全文を読む → 要約する。**カバーされていないこと**も書かせる |
| `deep_dive` | `question` (必須) | 最初の検索だけで答えない。上位ヒットを `get_connection_graph` の depth 2 で広げ、全文を読み、そこで得た語彙で再検索する |
| `whats_new` | `since` (任意、`YYYY-MM-DD`。省略時は 30 日前) | その日付以降の文書を概観する。**`date_from` が絞るのは frontmatter の `date` = 著者が書いた値であって、ファイルの更新時刻ではない**ことを prompt 自身に明記させ、近似であると断らせる。加えて **`date_from` は文字列として比較される**ので、`YYYY-MM-DD` 以外を渡すとエラーにならず全文書が落ちることも警告する |
| `find_gaps` | `topic` (任意) | 欠落を探す。`low_confidence` が立つ問い、`include_low_quality: true` でしか出てこない stub。**欠けているものを報告させ、内容の提案はさせない** |

4 つとも text のみで、引用規則を共有する: 使った文書の `path` を必ず引用する / `low_confidence` を握り潰さず表に出す / ナレッジベースが沈黙している時は一般知識で埋めずにそう言う。

**設定ファイルではなくコンパイル時固定にしてある。** prompt 本文はモデルに渡るテキストで、`groove.toml` は cwd や `.git` 祖先から**発見される**ため、設定で定義できるようにすると untrusted config に対して `kb_path` と同じ制限が必要になる。MCP 仕様も助けにならない — tool annotation と違い、**クライアントに「prompt の内容を信用するな」と言う指針が無い**。

## MCP リソース

(v0.22.0+) ナレッジベースを `kb://` スキームの MCP resource としても公開する。Claude Code では `@` メニューに出る。

| URI | 中身 |
|---|---|
| `kb://topic/<prefix>` | **topic group** = パスの先頭 1〜2 セグメント。indexer が `category` / `topic` を導出するのと同じ規則。read するとその配下の文書一覧 (URI 付き) が Markdown で返る。`kb://topic/` は root group |
| `kb://doc/<path>` | 索引済みの文書 1 件。列挙はせず**テンプレートとして公開**する |

`resources/list` が返すのは topic group であって、**文書 1 件ごとではない**。ナレッジベースの文書は数百でもグループは数十であり、listing は接続のたびにクライアントが取りに来るもの。個々の文書はテンプレートと、**`search` hit に付くようになった `uri`** から辿れる — spec は「listing に出ていない文書へのリンクを tool が返すこと」を明示的に許している。listing もこの `uri` も**同一の述語**から来るので、同じ文書について両者が食い違うことはない。索引に残り検索でも見つかるまま、提示だけ外れる要因が 2 つある。1 つは**現在の parser registry**: `[parsers].enabled` を狭めて再 index しないと、外した拡張子の行は索引にも検索結果にも残るが、read が拒否する以上提示しない。もう 1 つは **size** (v0.23.0+): 1 MiB を超える Markdown / テキスト文書は `resources/read` が返す量を超えるので、これも提示しない (`search` hit は残り、`uri` だけが付かない)。同じサイズでも PDF や表計算は提示され続ける — read が拒否ではなく抽出テキストを切り詰めるため。size は index 時に記録される。以前のバージョンで索引した文書は size 未記録で、**次の `groove index` まで提示されたまま**になる (その 1 回で再 embed 無しに埋まる)。件数は `groove doctor` が報告する。根拠は [ADR-0005](decisions/0005-record-document-size-in-the-index.ja.md)。

区切りは forward slash のまま、それ以外は percent-encode するので、空白や非 ASCII を含むパスでも正しい ASCII URI になる。

**read は索引で縛られる。** 提供されるのは索引に入っている文書のみで、そのうえで `get_document` と同一の検査 (symlink / hardlink 拒否、path traversal、拡張子 membership、size cap、handle 束縛の read) を通す。これは `get_document` (= `kb_path` 配下で拡張子が registry にあれば返す) より**狭い**。resource は「サーバが提示したもの」なので、提示していない URI を提供するのは別の操作だから。したがって `.grooveignore` された文書は resource には出ないが `get_document` からは従来どおり読める — これは [ADR-0003](decisions/0003-kb-mcpignore-bounds-indexing-not-access.ja.md) の契約が不変であることの帰結。判断の正本は [ADR-0004](decisions/0004-resource-reads-are-bounded-by-the-index.ja.md)。

内容はテキストとして返り、media type は**提供物の型**にする: Markdown は `text/markdown`、抽出テキストとして出すものは `text/plain`。PDF や表計算は **groove が抽出したテキスト**として返り、元のバイト列ではない。

**未実装**: `resources/subscribe` と `notifications/resources/list_changed`。これらが無くても `"resources": {}` は準拠した宣言であり、固定の topic group は滅多に変わらない。

## Related

- `docs/citations.ja.md` — `match_spans` とバイトオフセット
- `docs/filters.ja.md` — 検索結果の絞り込み
- `docs/clients.ja.md` — そもそもクライアントを繋ぐ手順
