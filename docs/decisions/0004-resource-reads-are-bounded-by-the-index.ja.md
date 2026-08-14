# 4. resource の read はファイルシステムではなく索引で縛る

- Status: accepted
- Date: 2026-08-15
- Deciders: プロジェクトオーナー
- Applies to: v0.22.0

## 背景と課題

v0.22.0 で kb-mcp に MCP の `resources` capability が入った。クライアントは
`kb://doc/<path>` を要求して文書のテキストを受け取れる。

ここで、本プロジェクトが**別の機構について既に一度、意図的に答えた問い**が再び立つ:
**read は何で縛られるのか。**

数日前に受理した [ADR-0003](0003-kb-mcpignore-bounds-indexing-not-access.ja.md) は、
`.kb-mcpignore` が縛るのは**索引**であって**アクセス**ではないと決めた。
`get_document` は `kb_path` 配下で拡張子が registry にあるファイルなら、
索引に入っていようがいまいが返す (`document_in_excluded_dir_is_still_readable` が pin)。
理由は「**木の中に置いたルールはその木を守れない**」— KB に書ける者はそれを消せるから。

`resources/read` はこの契約をそのまま継ぐこともできるし、より狭くすることもできる。
これは机上の問いではない: resource は**サーバが提示したもの**なので、
それを要求するクライアントは「パスを既に知っていて `get_document` を呼ぶ側」とは
**違うことをしている**。

## 決定要因

- 何を選ぶにせよ、**到達範囲を広げてはならない**。前リリースで hardlink の穴を
  塞いだばかりで、guard を 1 つ飛ばす 2 本目の read 経路はそれを返上することになる
- ADR-0003 は新しく、論理も妥当。1 週間後に同じ土俵で蒸し返すのは churn
- `resources/list` は**約束**である。渡した URI が読めない方が、最初から
  提示しないより悪い
- guard の並びは**厳密に 1 本**でなければならない。2 つの呼び出し側に 2 つの写しが
  あるのが「片方にだけ guard が効く」状態の作り方 — 1 階層下の `max_bytes_for` が
  まさにそのために存在している

## 検討した選択肢

1. **`get_document` と同じ契約** — 索引の有無に関わらず、`kb_path` 配下で
   拡張子が registry にあれば返す
2. **索引 membership を先に見て、その後は `get_document` と同一の guard**
3. **`get_document` の guard + read のたびに `.kb-mcpignore` を見る**

## 決定

**選択肢 2: 文書が resource として提供されるのは索引に入っている場合のみ。
そのうえで `get_document` と同一の guard を通す。**

選択肢 3 は即却下する。ADR-0003 が明示的に退けた境界を、**数日で変わっていない
同じ論理の上に**再建することになり、しかも「読めるか」の答えが
「KB に書ける者なら誰でも消せるファイル」に依存する。

選択肢 2 は選択肢 1 より**狭い**ので、到達範囲を広げようがない。そして
その根拠は ADR-0003 が却下したものと**質的に違う**: 木の中のファイルに木を
守らせるのではなく、**kb-mcp 自身の DB** — サーバが構築し所有する状態 — を信じている。

安全なだけでなく**正しい**と言える理由は、resource が何かにある。
`get_document` は「パスを既に知っている呼び出し元」に答えるもので、そこでの契約は
「`kb_path` 配下は読める前提、秘密は外に置け」。`resources/read` が答える相手は
**このサーバが渡した URI** を持っている呼び出し元である。提示していない URI を
提供するのは別の操作であり、**提示物で縛るのは後付けの制限ではなく自然な契約**。

これは `resources/list` を正直にもする — ただし listing を**読みが実際に受け付ける
集合**から作った場合に限る。生の索引ではそうならない: `[parsers].enabled` を狭めて
再 index しないと、外した拡張子の行は**意図的に残り**、共有 guard の拡張子検査が
それを拒否する。索引 membership だけで listing を作ると、**次の呼び出しが拒否する
`kb://doc/…` を渡してしまう**。そこで listing も read も
`servable_document_paths()` という 1 つのクエリ (= 索引のパスから、現在の registry
で開けないものを除いたもの) を通す。「listing が出すものは read が受け付ける」は
**1 つのリストの性質か、どちらのものでもないか**のどちらかしかない。

### 帰結

- ディスク上にあり `get_document` なら返るファイルでも、**索引に入るまで resource
  としては読めない**。これは意図どおりで、そういうファイルを書いて拒否されることを
  assert するテストがある
- `.kb-mcpignore` / `exclude_dirs` で除外された文書は `resources/list` に出ず
  `resources/read` でも読めない。**索引に無いから**であって、新しい境界を引いた
  わけではない。`get_document` からは従来どおり読め、**ADR-0003 は不変**
- guard の並びは `KbCore::load_document_blocking` の 1 関数にあり、
  `get_document` と `resources/read` の両方がこれを呼ぶ。symlink / hardlink 拒否、
  path traversal、拡張子 membership、size cap、handle 束縛の read が
  **一度だけ、両方に**効く
- resource の read が返すのは `get_document` の JSON エンベロープではなく
  **文書のテキスト**で、media type は**提供物の型**にする — Markdown は
  `text/markdown`、抽出テキストとして出すものは `text/plain`。PDF は
  kb-mcp が抽出したテキストとして返るので、`application/pdf` と名乗るのは
  クライアントが手にしているバイト列について嘘をつくことになる
- 現在の parser registry が扱わない拡張子の行は、索引には残る (`[parsers].enabled`
  を狭めても削除しない) が**提示はしない**: `resources/list` に出ず、
  `resources/read` でも読めず、`search` hit にも `uri` キーが付かない。
  **hit 自体は残る**ので文書は見つかり続ける — 拒否される read への link を
  持たないだけ
- topic group の resource (`kb://topic/<prefix>`) は文書を列挙するだけで提供はしないので、
  `resources/list` が既に出している以上のものは露出しない
