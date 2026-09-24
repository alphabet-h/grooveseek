# 24. 強制再構築が索引を空にする前に embedding endpoint を試す

- Status: accepted
- Date: 2026-09-24
- Deciders: プロジェクトオーナー
- Applies to: v1.14.0

## 背景と課題

強制再構築 — `groove index --force` と、MCP ツール `rebuild_index` の
`force: true` — は、まず索引を空にする (`grooveseek/src/db/meta.rs` の
`reset_for_model`。`grooveseek/src/indexer.rs` の `reset_and_resolve_context_mode`
から呼ばれる)。reset は自前の transaction で走って commit する。文書の embedding は
その後、1 ファイルずつ行う。

FastEmbed ならこの順序で困らない。モデルは reset の前に読み込まれ、プロセス内で動く。
`provider = "openai-compatible"` ([ADR-0022](0022-embedding-provider-boundary.ja.md))
では、provider は endpoint に触れずに作られ、最初のリクエストは reset が commit した
後に出る。`api_key` の誤りや失効 (401)、endpoint の停止、rate limit (429)、別の長さの
ベクトルを返すようになったサーバは、どれもその最初のリクエストで表に出る — 索引が
既に空になった後に。コマンドは失敗し、後に残るのは文書が 1 件も無い索引になる。

MCP 経路ではさらに悪い。`rebuild_index` は HTTP の port に届く相手なら誰でも呼べ、
GrooveSeek は設計上認証を持たない。endpoint が断り続ける間、呼び出し側は配信中の
索引を何度でも空にできる。

ADR-0022 は **GrooveSeek は endpoint を probe しない** と決めた。理由は索引を開く
ことについてだった: 起動と `Config::validate` が決定的でなくなる、停止中の service が
設定エラーに見える、索引を開く前にリクエストがマシンの外へ出る。ここでの問いはもっと
狭い: **知識ベースを endpoint へ送ることが目的のコマンドは、置き換えようとしている
索引を壊す前に、endpoint が応えるかを確かめてよいか。**

## 判断の軸

- 失敗するコマンドは、何も変える前にその失敗を検出できるなら、実行前より悪い状態を
  残してはならない。
- 索引を開く・検証する・配信することは、ADR-0022 の理由どおり決定的で offline のまま。
- 確認のために知識ベースの中身をマシンの外へ出さない。
- 呼び出し側 2 つに継ぎ目は 1 つ: CLI と MCP で守られる・守られないが分かれてはならない。
- 確認が受け入れる応答は、索引作成が受け入れる応答と同じでなければならない。さもないと
  確認は通り、再構築はやはり失敗する。

## 検討した選択肢

1. **reset の前に、固定の文字列 1 件を document 側で embedding する。強制再構築の
   ときだけ。** 採用。

2. **新しい索引を古い索引の横に作り、成功したら差し替える。** 今は見送る。再構築は
   ファイルごとに commit するので、途中で止まった実行も終えた分は残る。実行全体を 1 つの
   transaction にすると再構築の間ずっと書き込み lock を握って watcher を止め、影の
   database は新しい次元の `vec_chunks` を作る間ディスクを 2 倍使う。保証としてはこちらが
   強い — 途中で落ちる endpoint も覆う — が、この問題に要る変更よりずっと大きい。

   再考する条件: 別の理由で再構築が build-then-swap に移ること。そのとき probe は守る
   ものが無くなり、外してよい。

3. **最初の本物のバッチが embedding できるまで reset を遅らせる。** 退けた。reset の
   後には空の索引を前提にした書き込みが続き (context mode、コードチャンクの budget と
   policy、宣言フィールドの集合)、最初の embedding はファイルを読んでチャンクにした後、
   ファイルごとのループの中にある。破壊的な段階をそのループへ移すと、reset が単純に
   保とうとしているコードに散らばる。節約できるのは小さなリクエスト 1 回だけ。

4. **provider を作るすべての場所で probe する** — `serve` の起動、`validate`、
   すべての `index`。ADR-0022 の理由で退けた。ここでその理由は何も変わらない。強制で
   ない `index` は何も壊さないので、そこでの失敗の代償はその実行 1 回分で済む。

5. **そのままにし、`--force` には動く endpoint が要ると書く。** 退けた。失われたことは
   次の検索が何も返さないまで気付かれず、MCP 経由ならどの呼び出し側でも繰り返せる。

## 決定

**強制再構築は、索引を reset する前に embedding provider へ probe を 1 回送る。
それ以外は probe しない。**

- **どこで**: `rebuild_index` (`grooveseek/src/indexer.rs`) の先頭、`force` のとき、
  知識ベースのパスを解決した後、database へ何か書く前。`groove index --force` と MCP の
  `rebuild_index {force: true}` はどちらもこの関数を通って reset に至るので、それぞれ
  probe を 1 回送る。CLI が `rebuild_index` の前に持っていた自前の reset は、関数内の
  reset より古いもので、外した。残せば probe より先に索引を空にしていた。
- **何を**: `Embedder::probe_before_reset` (`grooveseek/src/embedder.rs`)。
  OpenAI 互換 provider では、固定の ASCII 文字列 `ENDPOINT_PROBE_TEXT` を document
  として — 再構築がこれから使う側である `document_model` で — embedding し、応答を
  索引作成のすべての応答と同じ検査に通す: HTTP status、ベクトルの件数、index の範囲と
  重複、宣言した `dimension`。FastEmbed は何もしない (モデルは既に読み込まれている)。
- **失敗したら**、索引は変更していないと述べるエラーに provider 自身のエラー (例えば
  `embedding endpoint returned HTTP 401: ...`) を続けて止まる。MCP ではそれがツールの
  エラー応答になる。
- **変わらないもの**: 索引を開くこと、`Config::validate`、`serve` の起動、incremental
  な `index`、`search`、watcher は probe しない。ADR-0022 の規則はそのすべてに残り、
  この記録は上の例外 1 つだけそれを狭める。

## 結果と代償

- **強制再構築のリクエストが 1 回増える。** 数トークンの入力 1 件で、hosted service
  ではほかと同じく課金される。
- **probe が示すのは endpoint が 1 回応えたことで、再構築が最後まで終わることではない。**
  probe の後で断り始めた endpoint — 途中で当たった rate limit、実行中の停止 — は、
  やはり索引を一部だけ書いたところで再構築を止める。それを塞ぐのは選択肢 2 で、それまでの
  対処は強制再構築をもう一度打つこと。
- **`--force` が開けなかった database は、probe より前に置き換わる。**
  `groove index --force` は、使える SQLite database でないファイルを
  `rebuild_index` の前に差し替える (`main.rs` の `Commands::Index` arm にある
  `open_or_replace_corrupt`)。その後で probe が失敗すると、メッセージはやはり
  索引は変更していないと述べるが、残すべき使える索引はもともと無かった。
- **マシンの外へ出る文字列が、以前より 1 リクエスト早くなる。** それは固定の文字列で
  知識ベースの中身ではなく、行き先は運用者が強制再構築を打つことで全チャンクを送ると
  既に決めた先だけ。
- **エラーは索引作成が出すものと同じ**。probe は同じコードを通るので。1 件なら受け、
  64 件のバッチは断るサーバは捕まえない。probe は負荷試験を兼ねようとはしない。
- **test が保つ** (`grooveseek/tests/openai_compatible_provider.rs`):
  `index_force_against_a_401_endpoint_leaves_the_existing_index_intact`、
  `index_force_against_a_failing_endpoint_leaves_the_existing_index_intact`
  (429 と次元違い)、
  `mcp_rebuild_index_force_against_a_401_endpoint_leaves_the_existing_index_intact`、
  `index_force_probes_the_endpoint_exactly_once_before_resetting`、
  `incremental_index_does_not_probe`。

## 参考

- [ADR-0022](0022-embedding-provider-boundary.ja.md)。この記録はその「probe しない」
  規則を狭める。その status に記した。
- 外部 provider にいつリクエストが送られるかは
  [usage.ja.md](../usage.ja.md#外部の-openai-互換-embedding)。
- `grooveseek/src/embedder.rs` (`ENDPOINT_PROBE_TEXT`、
  `Embedder::probe_before_reset`)、`grooveseek/src/indexer.rs`
  (`rebuild_index`、`reset_and_resolve_context_mode`)。
- English version:
  [0024-probe-the-endpoint-before-a-forced-rebuild.md](0024-probe-the-endpoint-before-a-forced-rebuild.md)
