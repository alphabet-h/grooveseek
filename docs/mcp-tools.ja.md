# MCP ツール / プロンプト / リソース

GrooveSeek が接続クライアントに公開する MCP の面。

> **English version**: [mcp-tools.md](./mcp-tools.md)

## MCP ツール

| ツール | 説明 | 主なパラメータ |
|---|---|---|
| `search` | ベクトル + FTS5 全文検索を Reciprocal Rank Fusion でマージしたハイブリッド検索、任意で cross-encoder 再ランク + MMR 多様性再ランク + parent retriever 展開。`{ results, low_confidence, filter_applied }` ラッパで関連度ランク付き chunk を返す。parent retriever が発火した行には `expanded_from` も付く。chunk がソースファイル由来なら (v1.2.0+) `start_line` / `end_line` (1 始まり、定義ではなく chunk の範囲) と `symbol_kind` (grammar が呼ぶ定義の種別) も付く。この 3 つは、それ以外では `null` ではなく**キーごと現れない**。server が索引を最新に保っていない間は、成功の応答に `writer_notice` も付く (v1.22.0+。下の「書き手の注記」を参照)。詳細: [docs/citations.ja.md](citations.ja.md)、[docs/filters.ja.md](filters.ja.md)、[docs/retrieval-pipeline.ja.md](retrieval-pipeline.ja.md) | `query` (必須。空白区切りの group の先頭に `-` を付けると (例: `-async`)、検索するのではなく両脚から除外する — v1.1.0+、詳細は [retrieval-pipeline.ja.md](retrieval-pipeline.ja.md) — 同じ 3 文字の trigram 下限が効くので `-ab` は何も除外しない)、`limit`、`category`、`topic`、`rerank` (サーバ既定を上書き)、`min_quality`、`include_low_quality`、`path_globs` (`!` 始まりは exclude)、`tags_any` / `tags_all`、`date_from` / `date_to` (`YYYY-MM-DD`)、`fields` / `fields_not` (object: key → list of strings、スキーマが宣言した key のみ、v1.9.0+)、`min_confidence_ratio`、`mmr` / `mmr_lambda` / `mmr_same_doc_penalty` (v0.7.0+)、`parent_retriever` (v0.7.0+) |
| `list_topics` | index 済みの全トピック / カテゴリと文書数を列挙。`search` の `category` / `topic` filter が受ける値はここで分かる。(v1.1.0+) 各 entry に `children` が付く: そのカテゴリ / トピック配下の directory tree で、topic より下の path segment 1 つが 1 node。node は `segment` / `file_count` (その prefix 配下の文書数。親は配下すべてを数える) / 自身の `children` を持つ。root と category のみの entry、および文書が topic 直下にしか無い topic では `[]`。tree は保存された path から作るので、frontmatter の `topic:` で path 由来の topic を上書きした文書も、2 番目の path segment より後の directory をその group に寄与させる。この segment はその文書自身の path から取ったものなので、`<category>/<topic>/` の下に directory として実在するとは限らない — client はこれを繋げて path prefix を組み立ててはいけない。[MCP の面の安定性](stability.ja.md#mcp-の面) を参照 | (なし) |
| `get_document` | 相対パスから文書の全文 + メタデータを取得。パスはその文書の**ただ 1 つの綴り** — KB 相対・`/` 区切りで、`search` が返したとおり — でなければならない。同じファイルに届く別の書き方 (`./a.md`、`a//b.md`、`a/../a.md`、絶対パス、Windows / macOS での大文字小文字違い、Windows でのバックスラッシュと 8.3 短縮名、symlink のディレクトリ経由) は "not found" になる。要求文字列で絞り込む前段ゲートウェイを迂回させないため。索引が持ちうる名前でないパス (絶対パス・空文字列・`..`・`.`・空の区間を含むもの (`./a.md`、`a//b.md`)、Windows ではドライブ指定・UNC 共有・バックスラッシュ・どこかに含まれる `:` (`note.md:secret` のような代替データストリーム)・`NUL` や `COM1` などの予約デバイス名・末尾がドットか空白の区間・`< > " \| ? *` や制御文字) はディスクを見る前に拒否する: そこに何かがあっても無くても答えは同じで、Windows で共有に接続することもない。索引が持ちうる名前での別の綴り (大文字小文字違い・8.3 短縮名・symlink のディレクトリ経由) は先にディスクを見てから (見るのは KB の中だけ) 同じく "not found" になる | `path` (例: `"deep-dive/mcp/overview.md"`) |
| `get_best_practice` | opt-in: `groove.toml` の `[best_practice].path_templates` を設定しているときのみ機能する。対象向けの best practice 文書を取得し、任意で特定 h2 セクションを抽出。未設定時は "not configured" エラーを返す | `target` (例: `"claude-code"`)、`category` (任意) |
| `rebuild_index` | すべてのソースファイル (Markdown + `[parsers].enabled` で有効化された拡張子) を走査してインデックス再構築。**同時に 1 本だけ** (v1.0.0+): 実行中に来た呼び出しは「実行中のものが何秒前に始まったか」を添えたエラーで断る。再構築は embedder と DB を握り続けるので、終わるまで検索が使えないため。この制限は server の中のもの。process の間には書き手のロックが効く (v1.22.0+): 他の groove の process が索引のロックを持っているとき (`groove index` の run、または同じ索引を書いている別の server) は、embedder にも DB にも触れず、embedder・DB・実行中の再構築を**待たずに断り**、その process を名指す。reader として起動した server では、restart されるまで呼び出しをすべて断る。`--no-watch` で起動した server は呼び出しの間だけロックを取るので、再構築の最中にその横で打った `groove index` は exit 3 になる。断りとエラーを含むすべての応答に、在れば `writer_notice` が付く (下の「書き手の注記」を参照)。返り値は CLI の summary と同じ counter で、`frontmatter_unparsed` (v1.7.0+) と `chunks_embedded` / `chunks_reused` (v1.21.0+。embedding model に送った chunk の数と、embed 入力が前と同じで保存済みの vector を使い回した数。`error` 付きの返答にも載る) と `kept_changed` / `renames_skipped` (v1.22.0+。ロックを取らない書き手が run の間に変えたため、prune が残した行の数と、飛ばした rename の数。なければ 0。run が完走して、frontmatter の strict 判定 (`fail_on_frontmatter_error`) や embedding の拒否で `error` が付いた返答には載るが、run 自体が失敗した `Rebuild failed` の返答には載らない) と `skipped_files` / `skipped_files_omitted` (v1.24.0+。`skipped` に数えたファイルと、それぞれが索引に入らなかった理由。`kept_changed` と同じ返答に載る。下の「索引に入らなかったファイル」を参照) を含む。`[index].fail_on_frontmatter_error = true` のときは、その値が 0 でなければ counter の隣に `error` key が付く。embedding provider が OpenAI 互換のとき、endpoint が入力を断った (HTTP 400 / 413 / 422) ファイルは skip されて `skipped` に数えられ、そうした拒否 (そのファイルのどのバッチも受け付けられないままのもの) があって endpoint がどのバッチも受け付けなかった run では counter の隣に `error` key が付く。`force: true` の run でそうした拒否が 1 つでもあった場合も同じで、再構築が先に索引を空にしたためそのファイルは索引に無い ([ADR-0025](decisions/0025-skip-rejected-inputs-and-retry-transient-embedding-failures.ja.md))。`force: true` で embedding provider が OpenAI 互換のときは、索引を空にする前に固定の probe 文字列を 1 件 embedding し、endpoint が断れば、索引から何も消していないと述べるエラーを返す ([ADR-0024](decisions/0024-probe-the-endpoint-before-a-forced-rebuild.ja.md)) | `force` (任意、既定 false) |
| `get_connection_graph` | ドキュメントパスを起点に意味的に関連するチャンクを BFS 展開。`search` の後、query の語が届かなかった文書に答えがありそうな時に使う (探索は query ではなく、起点文書からの embedding 類似度を辿る)。`parent_id` / `depth` / `score` / `snippet` 付きのノード配列を返し、呼び出し側でコンテキスト発見を連鎖させられる。上限で探索が切られた場合は `truncated` / `truncation[]` が付く。`snippet` は抜粋なので、ノードの全文は `get_document` で読む | `start` (必須、探索の起点パス — `groove graph --start`)、`depth` (既定 2、最大 3)、`fan_out` (既定 5、最大 20)、`min_similarity` (既定 0.3)、`seed_strategy` (`all_chunks` / `centroid`。`all-chunks` も受け付ける)、`dedup_by_path`、`category`、`topic`、`exclude_paths`、`max_nodes` (既定 100、最大 2000)、`max_seed_chunks` (既定 32、最大 1000) |

### 書き手の注記

(v1.22.0+) 1 つの索引に書く groove の process は同時に 1 つだけ ([ADR-0031](decisions/0031-one-writer-per-index-by-an-os-lock.ja.md))。この server が索引を最新に保っていないとき、`search` と `rebuild_index` の応答に任意の文字列の field `writer_notice` が付き、その理由を述べる。文は次の 3 つのどれか:

- **この server は reader である。** 他の groove の process が索引のロックを持っている間に (またはロックが取れない状態で) 起動したので、検索には答えるが索引を更新せず、`rebuild_index` を断る。他の process が終了した後も、restart されるまで reader のまま。その間に足した・編集した・消した file は反映されず、消した file は search に出続ける。この文は restart まで毎回付く。
- **watcher がナレッジベースの watch を登録できない** (ディレクトリが無い、Linux で `fs.inotify.max_user_watches` を使い切った、など)。この文は登録に成功するまで付く。
- **watcher が終了処理の外で止まった。** この文は restart まで付く。

tool の description が `writer_notice` を名指し、client に利用者へ伝えるよう求める。`search` では成功の応答にだけ付き、`error` の応答には付かない。`groove search --format json` には付かない。advisory であって、凍結した search の field の外にある ([stability.ja.md](stability.ja.md#search-が返すもの))。他の tool には付かない。

reader の文は、起動の時にロックを持っていた process と、今の `.groove.db.writer.json` の中身を名指す。その process がこのナレッジベースを最新に保つと書くのは、それが同じナレッジベースを watch する server で、その file の heartbeat が 15 秒以内で、watcher が watch できていると報告しているときだけ。heartbeat が古い・無いときは、その process は終了した見込み (kill された process は file を残す) で、この server を restart するまで反映されないと書く。watch できていないと報告しているときは、その間は索引されないと書く。

### 索引に入らなかったファイル

(v1.24.0+) `rebuild_index` の応答は、run が `skipped` に数えたファイル — ディスクにはあるが索引に入らなかったもの — を、それぞれの理由とともに並べる ([ADR-0032](decisions/0032-report-why-each-file-was-not-indexed.ja.md))。top level の 2 つの key に載る:

- **`skipped_files`**: library の `IndexResult::skipped_files` の先頭 50 件。path の順のままで (server は並べ替えもまとめ直しもしない)、path ごとに 1 件。<!-- via: grooveseek/src/server.rs `const SKIPPED_FILES_IN_REPLY: usize = 50` --> 各 entry は `path` (ナレッジベースからの相対で、区切りは `/`。索引が名前を持てないファイルは lossy な綴り)、`reason`、下の表のその理由の詳細 key を持つ object。その理由に無い key は `null` ではなく、出さない。
- **`skipped_files_omitted`**: 並べた分の先にあと何件あったか。`skipped_files` の件数と `skipped_files_omitted` の和が library の一覧の件数で、それは `skipped` に等しい。

どちらの key も、skip したファイルが無くても付く (空の配列と 0)。完走した run の応答に載り、`error` が付いた応答 (frontmatter の strict 判定、embedding の拒否) にも載る。run の無い `Rebuild failed` の応答と断りの応答には載らない。

```json
{
  "skipped": 3,
  "skipped_files": [
    { "path": "資料/カタログ.pdf", "reason": "too_large", "size": 73400320, "limit": 52428800, "limit_key": "max_binary_file_size" },
    { "path": "メモ/old.txt", "reason": "not_utf8" }
  ],
  "skipped_files_omitted": 1
}
```

(上の値は形を示す例。実際の応答には他の counter も載る)

| `reason` | ファイルの状態 | 詳細 key |
|---|---|---|
| `too_large` | ファイルサイズの上限を超えた | `size`、`limit` (byte)、`limit_key`: `max_binary_file_size` (pdf / docx / xlsx / pptx) か `max_text_file_size` (md / txt / ソースコード) |
| `over_extraction_limit` | 展開後の大きさ・抽出したテキストの予算を超えた | `limit` (byte)、`limit_key`: 常に `max_decompressed_size` |
| `over_parser_limit` | parser 自身の固定の上限を超えた (ソースコードが code parser の byte の上限を超えた) | `size`、`limit` (byte)。設定では上げられないので `limit_key` は無い |
| `extraction_timed_out` | テキストの抽出が制限時間を過ぎた | `seconds` |
| `encrypted` | パスワードで保護されている (PDF、または暗号化されたコンテナに入った Office ファイル) | — |
| `corrupt_or_unsupported` | その形式の文書として読めない (壊れている、途中で切れている、OOXML の拡張子の旧形式、parser の失敗) | — |
| `not_utf8` | テキスト形式のファイルで、バイト列が UTF-8 でない (Shift_JIS など) | — |
| `text_undecodable` | text layer が文字化けに復号された PDF | — |
| `too_little_text` | ページあたりのテキストが少なすぎる PDF (text layer の無いスキャン、文字の少ないスライド) | — |
| `no_text` | 読んで parse したが embed するものが無い (空、frontmatter だけ) | — |
| `unreadable` | stat か読み込みができなかった | `cause`: `in_use` (他の process が開いたまま。Windows)、`permission_denied`、`gone` (walk の後で消えた)、`other` のどれか |
| `embed_rejected` | embedding の endpoint に断られた | `http_status` (400 / 413 / 422) |
| `changed_during_read` | 走査と読み込みの間に変わった。次の run で読み直す | — |
| `not_plain_file` | 読む時に通常のファイルでなかった (walk の後で symlink・デバイス・pipe に差し替わった) | — |
| `hard_link` | 名前が 2 つ以上ある (hard link) | — |
| `unspellable_name` | `get_document` と `kb://` URI が扱えない名前を持つ ([ADR-0023](decisions/0023-index-only-names-the-server-can-open.ja.md)) | `directory`: ディレクトリごと外したときは `true`、ファイルなら `false` (中に索引の対象のファイルがあったかどうかに関わらず 1 件と数える) |

`limit_key` は、上げればそのファイルが入る `groove.toml` の `[index]` の key の名前。ディスクにはあるが最初から対象にしないファイル (`.grooveignore`、`exclude_dirs`、未対応の拡張子、Office のロックファイル `~$*`、walk が見た symlink) は entry にならない。

**`reason` の語彙について。** 新しい値が足されることがある。そのとき、今まで汎用の値 (`corrupt_or_unsupported` / `no_text`) で報告していたファイルが、より具体的な値 (新しく足す値か、既にある具体的な値) に移ることがある。知らない値は汎用の失敗として扱うこと。具体的な値どうしの移動、具体的な値から汎用の値への移動、値の削除・改名は互換性を壊す変更になる。

## MCP プロンプト

(v0.22.0+) 4 つの prompt を同梱している。クライアントはこれを**ユーザが選ぶコマンド**として出す (Claude Code では `/mcp__<server>__<name>`。`<server>` は `.mcp.json` でこのサーバに付けた鍵であって、GrooveSeek が決める名前ではない)。存在理由は「ツールだけでは組み合わせ方が分からない」こと — `get_connection_graph` は `search` の後に使えるとは言うが、どこまで広げるか、出てきた語彙で検索し直すか、`low_confidence` が立ったらそう言うべきかは、どのツールも言わない。

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

`resources/list` が返すのは topic group であって、**文書 1 件ごとではない**。ナレッジベースの文書は数百でもグループは数十であり、listing は接続のたびにクライアントが取りに来るもの。個々の文書はテンプレートと、**`search` hit に付くようになった `uri`** から辿れる — spec は「listing に出ていない文書へのリンクを tool が返すこと」を明示的に許している。listing もこの `uri` も**同一の述語**から来るので、同じ文書について両者が食い違うことはない。索引に残り検索でも見つかるまま、提示だけ外れる要因が 3 つある (Windows ではもう 1 つ、Windows がそのままでは開けない名前。[ADR-0023](decisions/0023-index-only-names-the-server-can-open.ja.md) を参照)。1 つ目は**現在の parser registry**: `[parsers].enabled` を狭めて再 index しないと、外した拡張子の行は索引にも検索結果にも残るが、read が拒否する以上提示しない。2 つ目は **size** (v0.23.0+): 1 MiB を超える Markdown / テキスト文書は `resources/read` が返す量を超えるので、これも提示しない (`search` hit は残り、`uri` だけが付かない)。同じサイズでも PDF や表計算は提示され続ける — read が拒否ではなく抽出テキストを切り詰めるため。50 MiB を超えて索引されたバイナリ文書 (`[index].max_binary_file_size` で索引の上限を上げた時だけ起こる) は提示しない — read は索引の設定に関係なく 50 MiB を超えるバイナリを拒否するため ([ADR-0026](decisions/0026-configurable-index-size-caps.ja.md))。3 つ目は**展開上限**: バイナリ文書を提示するのは `[index].max_decompressed_size` が組み込みの展開 budget (50 MiB) 以下の間だけ — それより上げると、バイナリの hit は `uri` を持たず、`resources/list` / `resources/read` も提示しない。read は組み込みの budget で parse し、記録された raw の大きさからはどの文書を read が受け付けるか分からないため。テキスト文書には影響しない。size は index 時に記録される。以前のバージョンで索引した文書は size 未記録で、**次の `groove index` まで提示されたまま**になる (その 1 回で再 embed 無しに埋まる)。`groove doctor` は 3 つの要因それぞれで外れている文書を名指しし (`extension-not-registered` / `larger-than-a-read-returns` / `binary-uris-withheld`)、size 未記録の文書は件数を報告する。根拠は [ADR-0005](decisions/0005-record-document-size-in-the-index.ja.md)。

区切りは forward slash のまま、それ以外は percent-encode するので、空白や非 ASCII を含むパスでも正しい ASCII URI になる。

**read は索引で縛られる。** 提供されるのは索引に入っている文書のみで、そのうえで `get_document` と同一の検査 (symlink / hardlink 拒否、path traversal、拡張子 membership、size cap、handle 束縛の read) を通す。これは `get_document` (= `kb_path` 配下で拡張子が registry にあれば返す) より**狭い**。resource は「サーバが提示したもの」なので、提示していない URI を提供するのは別の操作だから。したがって `.grooveignore` された文書は resource には出ないが `get_document` からは従来どおり読める — これは [ADR-0003](decisions/0003-kb-mcpignore-bounds-indexing-not-access.ja.md) の契約が不変であることの帰結。判断の正本は [ADR-0004](decisions/0004-resource-reads-are-bounded-by-the-index.ja.md)。

内容はテキストとして返り、media type は**提供物の型**にする: Markdown は `text/markdown`、抽出テキストとして出すものは `text/plain`。PDF や表計算は **groove が抽出したテキスト**として返り、元のバイト列ではない。

**未実装**: `resources/subscribe` と `notifications/resources/list_changed`。これらが無くても `"resources": {}` は準拠した宣言であり、固定の topic group は滅多に変わらない。

## Related

- `docs/citations.ja.md` — `match_spans` とバイトオフセット
- `docs/filters.ja.md` — 検索結果の絞り込み
- `docs/clients.ja.md` — そもそもクライアントを繋ぐ手順
