# 25. embedding endpoint が断った入力は skip し、一時的な失敗は再試行する

- Status: accepted
- Date: 2026-09-25
- Deciders: プロジェクトオーナー
- Applies to: v1.14.0

## 背景と課題

`provider = "openai-compatible"` ([ADR-0022](0022-embedding-provider-boundary.ja.md))
では、`rebuild_index` (`grooveseek/src/indexer.rs`) が embedding し直すファイルは
1 本ごとに 1 回以上の HTTP リクエストになる。この記録までは、リクエストが 1 つでも
失敗すると `failed to embed chunks for <file>` で run 全体が止まっていた。walk は毎回
同じ順でファイルを回るので、次の run も同じファイルで止まり、その後ろのファイルは
いつまでも索引されなかった。ループの後に続く段階も走らなかった: 消えたファイルの row を
消す sweep と、宣言フィールドの集合の記録である。

これを起こす失敗は 2 種類あり、性質が違う:

- **endpoint が入力を断る。** token 上限を持つサーバは、長いチャンクに HTTP 400・413・
  422 で答える。送り直してもこの答えは変わらず、知識ベースの残りには関係が無い。
- **endpoint が一時的に失敗する。** rate limit (429)、再起動中のサーバの 5xx、timeout、
  接続の拒否。同じリクエストが数秒後には通ることがある。

問いは、`groove index`・MCP `rebuild_index`・watcher がそれぞれをどう扱うか、それも
設定の誤りを成功に見える run の陰に隠さずに、である。

## 判断の軸

- 1 ファイルの問題で、知識ベースの残りの索引を止めない。
- 設定の誤り (model alias の誤り、endpoint の誤り) を黙って skip しない。
- index identity を動かさない: リクエストの送り方だけを変える設定で再構築を強いない。
- endpoint の応答 body を MCP の呼び出し側に出さない。
- 1.x が凍結する設定面 (キー名・型・既定値) を小さく保つ。

## 検討した選択肢

1. **どの失敗でファイルを skip するか。**
   - どの失敗でも skip する。退けた: 401 や次元違いで全ファイルが skip になり、run は
     成功を報告してしまう。
   - **入力が原因の失敗 (HTTP 400・413・422) だけ skip する。** 採用。
   - 従来どおりどの失敗でも止まる。退けた: 上の問題そのもの。

2. **長い入力を送る前にどう抑えるか。**
   - token 数で。退けた: groove は alias の背後にある model の tokenizer を持たず、
     endpoint がどれを使うかも知り得ない。
   - バイト数で。退けた: マルチバイト文字の途中で切れ、文字体系ごとに残る量が変わる。
   - **文字数 (Unicode scalar value) で。** 採用。単位はサーバの token 上限と一致しないが、
     それでも長すぎる入力は上の skip に落ちる。

3. **再試行について何を設定として出すか。**
   - 待ち時間もキーにする。退けた: 凍結されるキーが増え、調整を求める声も無い。
   - **`max_retries` だけ。待ち時間は定数。** 採用。
   - キーを出さない。退けた: 低遅延が要る daemon には再試行を切る手段が要る (結果と代償を参照)。

4. **拒否のあった run が何を報告するか。**
   - 常に exit 0。退けた: endpoint が 400 で答える設定の誤りが、skip の多い正常な run に
     見えてしまう。
   - 拒否が 1 件でもあれば exit 非 0。退けた: 大きく健全な知識ベースに長すぎるチャンクが
     1 つあるだけで、毎回の run が失敗になる。
   - **拒否があり、かつ embed できたファイルが 1 つも無いときだけ exit 非 0。** 採用。
     この形はファイルではなく `model` / `document_model` か `endpoint` を指している。

## 決定

**断られた入力はそのファイルを skip し、一時的な失敗は再試行し、拒否しか起きなかった
run は最後まで済ませてから失敗にする。**

- **分類** (リクエストごと、`grooveseek/src/embedder.rs`):

  | 起きたこと | 扱い |
  |---|---|
  | HTTP 400、413、422 | `EmbedInputRejected`: 再試行しない。indexer はそのファイルを skip する |
  | HTTP 429、500-599 | 再試行する |
  | timeout、接続の失敗 | 再試行する |
  | その他の status (401、403、404、408 …) | その場で run を止める |
  | 2xx で body が正しい JSON でない、またはベクトルの件数・index・次元が合わない | その場で run を止める |
  | その他の通信エラー (例えば、サーバが受け付けた後に切った接続) | その場で run を止める |

- **skip**: indexer は
  `warning: <file>: embedding endpoint rejected the input (HTTP <status>); skipped, the index keeps what it had for this file`
  を出し (status だけで、応答 body は出さない)、そのファイルを `skipped` に数え、既存の
  row を残して先へ進む。1 ファイルが複数のバッチになり後のバッチが断られたときは、前の
  バッチのベクトルは捨て、そのファイルについては何も書かない。
- **`[embedding] max_input_chars`**: openai-compatible 専用、既定 8000、`0` は拒否。
  送る前に各入力をこの文字数で切る。document と query の両方。database のチャンク本文と
  全文索引は切らない。
- **`[embedding] max_retries`**: openai-compatible 専用、既定 3、0 から 10 まで受理
  (`MAX_EMBEDDING_RETRIES`)。送り直す単位はバッチ 1 つで、後のバッチが失敗しても前の
  バッチは送り直さない。`0` なら 1 回だけ送り、最初のエラーをそのまま返す。最後の試行の
  後のエラーは試行の回数を述べる。
- **再試行の前の待ち**: `Retry-After` (秒数か HTTP の日時) が 60 秒以下なら、その値
  ちょうどを jitter なしで待つ。60 秒を超える `Retry-After` は待たない: そのバッチはその旨を
  述べてすぐ失敗する。使える `Retry-After` が無ければ 1 秒・2 秒・4 秒…と倍にして 60 秒で
  頭打ちにし、その待ちの 4 分の 1 未満の jitter を足す。
- **どちらのキーも index identity に含めない**。`endpoint`・`api_key`・
  `request_dimensions`・`timeout_seconds` と同じ扱い。FastEmbed はほかの endpoint 用
  キーと同じく両方を拒否する。
- **強制再構築の probe**
  ([ADR-0024](0024-probe-the-endpoint-before-a-forced-rebuild.ja.md)) は同じリクエストの
  コードを通るので、同じように再試行され、切られる。probe の拒否は skip **しない**:
  probe の文字列は固定の短い ASCII なので、そこでの 400・413・422 は設定の誤りを指し、
  ADR-0024 の意図どおり reset の前に再構築を止める。
- **拒否があり何も embed できなかった run は、最後まで済ませた後で失敗にする**
  (`IndexResult::fails_all_inputs_rejected`)。run を途中で切らない: 削除の sweep と
  後処理は完了し、`rebuild_index` は件数を返す。その上で `groove index` は exit 非 0 で
  終わり、MCP `rebuild_index` は件数の隣に `error` を返す。どちらも文言は
  `IndexResult::all_inputs_rejected_message` から取る。watcher は 1 ファイルずつ
  索引し直すのでこの規則を当てず、断られたファイルを
  `watcher: skipped <file> (embedding endpoint rejected the input)` と報告する。

## 結果と代償

- **exit 0 は、すべてのファイルが索引されたという意味ではなくなる。** skip された
  ファイルは warning で名指しされ、`skipped` (`Done in` 行、MCP の stats) に数えられる。
- **切られたチャンクの末尾はベクトル検索に効かない。** 検索の全文索引側はチャンク全体を
  持っている。
- **`max_input_chars` を変えても何も embedding し直さない。** この記録より前に作った
  索引は、8000 文字より長いチャンクのベクトルを、そのチャンクが変わるか
  `groove index --force` を打つまで持ち続ける。
- **daemon は待っている間 embedder の lock を握る。** MCP `rebuild_index`・watcher・
  検索はどれもリクエストの間 embedder を握り、再試行の待ちはその中で起きるので、検索も
  一緒に待たされる。既定値での 1 バッチの最悪は約 420 秒 (60 秒の timeout を 4 回と、最大
  60 秒の待ちを 3 回)、`max_retries = 10` で約 1260 秒。どちらも設定からの推定で、実測では
  ない。すぐ答える必要のある daemon は `max_retries = 0` にする。
- **ADR-0024 の probe は今も 1 回の probe のまま。** 同じリクエストを送り直すのは、
  そのリクエストが再試行する種類の失敗をしたときだけ。再構築の途中で当たる rate limit に
  ついての同 ADR の「結果と代償」は、和らぐが無くなりはしない。
- **変更したファイルが 1 本だけで、それを endpoint が断った incremental run は exit 非 0
  になる。** その run では他に embed したファイルが無いため。決定どおりの規則で、run には
  1 本だけ長すぎるファイルと設定の誤りを見分ける手段が無い。
- **設定の誤りが見逃されることはまだある。** endpoint が一部の入力だけを断り、同じ run で
  別のファイルが embed できた場合。
- **日本語は 8000 文字以内でも token 上限を超えることがある。** 1 文字が 1 token を超える
  ことがあるため。そういうファイルは warning 付きで skip され、`max_input_chars` を下げれば
  戻る。
- **test が保つ**: `grooveseek/tests/openai_compatible_failures.rs` (400 / 413 / 422 での
  skip、probe の拒否、再試行、`Retry-After`、document と query の切り詰め、拒否しか
  起きなかった run を CLI と MCP で)、および `grooveseek/src/embedder.rs`・
  `grooveseek/src/config.rs`・`grooveseek/src/indexer.rs` の unit test。

## 参考

- [ADR-0022](0022-embedding-provider-boundary.ja.md)。index identity から外す設定の並びに
  この 2 キーが加わる。理由も同じで、リクエストの送り方を変えるだけで、どの model が
  答えるかは変えない。
- [ADR-0024](0024-probe-the-endpoint-before-a-forced-rebuild.ja.md)。その probe は
  ほかのリクエストと同じく再試行されるようになった。
- endpoint が断った・失敗したときに運用者に何が見えるかは
  [usage.ja.md](../usage.ja.md#外部の-openai-互換-embedding)。
- `grooveseek/src/embedder.rs` (`EmbedInputRejected`、
  `OpenAiCompatibleConfig::with_limits`)、`grooveseek/src/indexer.rs`
  (`IndexResult::fails_all_inputs_rejected`)、
  `grooveseek/tests/openai_compatible_failures.rs`。
- English version:
  [0025-skip-rejected-inputs-and-retry-transient-embedding-failures.md](0025-skip-rejected-inputs-and-retry-transient-embedding-failures.md)
