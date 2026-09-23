# 22. embedding を provider 境界の後ろに置き、既定は FastEmbed のままにする

- Status: accepted
- Date: 2026-09-23
- Deciders: プロジェクトオーナー
- Applies to: v1.13.0

## 背景と課題

v1.12.0 まで、embedding の推論とは FastEmbed の具体的なラッパのことだった。
コマンド・設定・索引の互換検査はどれも `ModelChoice` の言葉で話していた —
GrooveSeek がダウンロードしてプロセス内で動かせる、2 つのローカル BGE モデルの enum である。
それ以外にベクトルを作れるものは無く、索引が使えるかを決める場所はどこも、
**その enum に聞いて**決めていた。

運用者から、既に動かしている embedding サービス (ローカルの推論サーバや、
ホスティングされたもの) を GrooveSeek から使いたい、ただし GrooveSeek が
vendor ごとの作法を覚えるのではなく、という要望が出た (issue #312)。
そうしたサービスの多くは OpenAI が公開した `POST /v1/embeddings` と同じ形の
リクエストを話すので、クライアント 1 つで多くを覆える。

これを「variant を 1 つ足す」以上の話にしている事情が 2 つある。外部の endpoint は
**索引するすべての chunk と、すべての query**を受け取る — 知識ベースの本文がプロセスの外へ、
場合によってはマシンの外へ出る。そして索引は、どのモデルがベクトルを作ったかを記録し、
別のモデルの下では開くことを拒否する。したがって「モデル」を識別するものが、
**GrooveSeek の制御下に無いサービス**まで覆わなければならなくなる。

この記録が答えるのは、**GrooveSeek と embedding の実装の継ぎ目をどこに置くか、
そこを通って作られた索引を何で識別するか、そして外向きの embedding を誰が有効にしてよいか**である。

## 判断の軸

- **既定は動かさない**。v1.12.0 が作った FastEmbed の索引はそのまま開けなければならず、
  provider について何も書いていない環境は、どこへも何も送ってはならない
- retrieval のモデルは非対称なことが多い: query と document は別の埋め込み方をされ、
  別の model alias を使うこともある。継ぎ目はその 2 つを分けたまま保ち、
  **indexer や検索パイプラインがどの provider が何をするかを知らずに済む**ようにする
- 索引を開けるかどうかは**ネットワーク無しで決められなければならない**。
  リモートの答えに依存する起動や設定検査は決定的でなく、しかも運用者のコマンドが
  何もしないうちに通信を発生させる
- 知識ベースの本文をマシンの外へ出すのは**運用者の判断**である。
  たまたまディレクトリに置かれていたファイルが、それを決められてはならない
- vendor 固有の振る舞い (task prefix、sidecar の lifecycle、リモートでの rerank) は
  vendor の側のものである。背負うのは汎用の protocol 1 つで足り、
  [docs/stability.ja.md](../stability.ja.md) が凍らせる面は、キーを 1 つ足すごとに広がる

## 検討した選択肢

1. **document と query を別の呼び出しに分けた、小さな非公開の provider trait。
   FastEmbed をその既定の実装にし、vendor に依存しない OpenAI 互換の HTTP クライアントを
   2 つ目の実装にする。** — 採った案。

2. **FastEmbed の具体型を残し、外部モデルを `ModelChoice` の variant として足す**。却下。
   コマンドと設定の配線、互換検査は既に `ModelChoice` に依存していた
   (PR #314 がリファクタの理由として挙げたもの)。しかもリモートのモデルは
   この enum では記述できない — 決まった次元も、キャッシュ用のディレクトリも、
   ダウンロードも無い。variant を足すたびに、**1 つの vendor のモデル一覧が、
   索引を開くかどうかを決める場所へ広がっていく**。

3. **sidecar を同梱・管理する、または vendor の SDK に依存する**
   (Python のサーバ、MLX、Jina のクライアント)。却下。issue #312 の non-goals である。
   GrooveSeek が別プロセスの lifecycle (起動・死活・再起動・更新) を持つことになり、
   vendor 固有のリクエストフィールドや task の規約が増えていく。
   サービスは運用者が既に動かしている。GrooveSeek に要るのはそこへ届くことだけである。

4. **endpoint を probe してモデルと次元を知る**。却下。
   索引を開くことがネットワークの往復に依存するようになる: 起動と
   `Config::validate` が決定的でなくなり、止まっているサービスが設定の誤りに見え、
   **索引を開く前から**本文や probe のリクエストがマシンの外へ出る。
   代わりに運用者が `dimension` を宣言し、GrooveSeek はすべての応答をそれと照合する。

   **覆る条件**: ユーザの本文を含まない metadata 呼び出しでモデルの識別と次元を返す
   protocol が、頼れるほど広く実装されたとき。

5. **GrooveSeek が見つけたどの config の `[embedding]` も受け入れる**。却下。
   カレントディレクトリやその git root で見つかった config は、そのリポジトリ・
   アーカイブ・共有ドライブを書いた誰かが置いたものかもしれない。それを受け入れれば、
   **ディスク上で見つかったファイルが、索引した本文とすべての query を、
   自分が名指すアドレスへ送らせられる**。これは `[parsers]` に R5 がある理由そのものであり
   ([ADR-0016](0016-keep-the-plugin-directory-outside-the-knowledge-base.ja.md))、
   同じ境界を R7 としてここにも適用する。

6. **endpoint を identity に含める**。却下。hostname・port・scheme の変更や、
   負荷分散された 2 台のサーバが、**ベクトルは変わっていないのに**索引の全面的な
   作り直しを強いることになる。しかも endpoint の文字列は、その裏に実際にデプロイされている
   モデルを識別しない — 同じ URL での再デプロイはどちらにしても見えない。
   **移すたびに作り直しを払い、何の保証も得られない**。

   **覆る条件**: 応答に含まれるモデルの fingerprint のように、embedding 空間を
   サーバ側が識別する値が得られ、GrooveSeek が URL の代わりにそれを記録できるようになったとき。

## 決定

**embedding の推論は非公開の `EmbeddingProvider` trait の後ろに置く。FastEmbed は
既定の実装のまま残し、2 つ目の実装は `POST /v1/embeddings` を話す
vendor 非依存のクライアントとする。**

- **trait は document と query を分けたまま持つ**。メソッドは `embed_documents` と
  `embed_query` の 2 つ (`grooveseek/src/embedder.rs` の `EmbeddingProvider`)。
  公開の入口は `Embedder` で、`Box<dyn EmbeddingProvider>` を持ち、
  `embed_texts` と `embed_single` をそれぞれに委譲する。trait は公開せず、
  crate の外で実装するものは無い
- **FastEmbed は特別扱いではなく実装の 1 つ**
  (`grooveseek/src/embedder.rs` の `FastEmbedProvider`)。identity は従来どおり
  素の model id と次元 (`bge-small-en-v1.5` / 384、`bge-m3` / 1024) なので、
  **v1.12.0 の索引はそのまま開く**。これを試験
  `resolved_fastembed_settings_accept_an_existing_index` が固定している
- **HTTP 実装は 1 つの protocol であって、1 つの vendor ではない**
  (`grooveseek/src/embedder.rs` の `OpenAiCompatibleProvider`)。document は
  `document_model` で、query は `query_model` で、最大 64 件ずつ送る
  (`OPENAI_COMPATIBLE_BATCH_SIZE`)。`dimensions` は `request_dimensions = true` の
  ときだけ送る — 受け付けないサーバがあるからである。redirect は追わない
- **索引は、そのベクトルについて GrooveSeek が知りうるもので識別する**。
  identity は provider の種類・document alias・query alias・宣言した次元であり、
  `openai-compatible:{document_model}|{query_model}:{12 桁の hex}` と綴る。
  hex は、2 つの alias をそれぞれ長さ前置したものと次元を SHA-256 にかけたもの
  (`grooveseek/src/embedder.rs` の `OpenAiCompatibleConfig::new`)。
  `endpoint`・`api_key`・`timeout_seconds` は**意図して外してある** — 運用者が
  サービスを動かしても (host・port・TLS・key の入れ替え) 作り直さずに済むようにである。
  代償もはっきり書いておく: 同じ alias が別の endpoint で、あるいは同じ endpoint でも
  再デプロイ後に、**別の embedding 空間を返すようになっても GrooveSeek は検知できない**。
  そうなったときは運用者が自分で `groove index --force` を走らせる必要があり、
  それを知らせるものは何も無い
- **GrooveSeek は endpoint を probe しない**。外部 provider では `dimension` が必須である。
  identity は provider が存在する前に `EmbeddingSettings` へ解決され、
  `verify_embedding_meta` (`grooveseek/src/db/meta.rs`) はその settings に対して、
  `Embedder::with_settings` が provider を作る**前に**走る。そのうえで応答をすべて検査する:
  ベクトルの件数が入力と一致すること、各 `index` が範囲内で重複しないこと、
  欠けが無いこと、各ベクトルが宣言した次元を持つこと
- **外向きの embedding には trusted な config が要る (R7)**。`[embedding]` を受け入れるのは、
  運用者が `--config` で名指した config、バイナリの隣に置かれた config、
  または trusted な root の下にある config である
  (`grooveseek/src/config.rs` の `classify_trust`)。カレントディレクトリやその git root で
  GrooveSeek が見つけた config は、このセクションを丸ごと落とされ、
  **このセクションが何を送りうるか、どうすれば受け入れられるか**を述べる warning が出る
  (`grooveseek/src/config.rs` の `restrict_untrusted`)。`[parsers]` が既に持つ境界と同じである
- **`--model` は従来の意味を保つ**。その実行に限って FastEmbed を選び、`[embedding]` より優先する
  (`grooveseek/src/config.rs` の `Config::resolve_embedding`)
- **PR は 2 本、境界が先**。PR #314 が trait・settings の型・互換検査の配線を
  **振る舞いを変えずに**入れたので、新しいものを設定できるようになる前に、
  既存の索引が互換であることが確かめられた。PR #316 が HTTP provider、
  その contract test、英語の docs を足した

## 結果と代償

- **Linux / macOS の既定ビルドは、reqwest 0.13 の blocking クライアントと、rustls の
  2 つ目の暗号バックエンドを常にリンクするようになった**。`reqwest` 0.13 を
  `blocking`・`json`・`rustls` feature 付きで無条件に依存し (`grooveseek/Cargo.toml`)、
  その `rustls` feature が aws-lc-rs / aws-lc-sys のバックエンドを引き込む。
  使うのは opt-in の provider だけだが、**cargo feature では外せない**。
  blocking の HTTP クライアントも TLS スタックが 2 つあることも新しくはない:
  v1.12.0 は hf-hub を通じて ureq・reqwest 0.12・native-tls を、
  fastembed・hf-hub・ureq を通じて ring バックエンド付きの rustls を既に持っていた。
  変わるのは、**rustls の暗号 provider が 2 つ (ring と aws-lc-rs) 1 つのバイナリに
  共存する**ことである。以前のビルドの姿 (aws-lc-sys が無かったことを含む) は、
  reqwest 0.13 を使っていたのが Windows 専用の tray だけだった旧 lockfile から
  読み取ったもので、**推定**である (実測していない)。現状はリリースの前ごとに
  `cargo tree -p grooveseek -i aws-lc-sys --target aarch64-unknown-linux-gnu` で測る
- **新しい 9 つのキーは、出したリリースから凍る**。`[embedding]` は `provider`・`endpoint`・
  `model`・`query_model`・`document_model`・`dimension`・`request_dimensions`・`api_key`・
  `timeout_seconds` を持ち、既定値は `provider = "fastembed"`・`request_dimensions = false`・
  `timeout_seconds = 60` である。[設定](../stability.ja.md#設定)の約束はキー名・型・既定値を
  凍らせるので、minor リリースでは改名も既定値の変更もできない。
  [既定の embedding モデル](../stability.ja.md#既定の-embedding-モデル)の約束は provider も覆う:
  FastEmbed 以外を既定にするのは major な変更である
- **version の下限ができる**。v1.12.0 以前は未知のキーを拒否するので、
  `[embedding]` を書いた `groove.toml` は**それらのリリースをそもそも起動させない**。
  [ADR-0021](0021-take-the-socket-you-were-given.ja.md) が `systemd_socket` の前に置いたのと
  同じ下限である: GrooveSeek を先に上げ、それからセクションを足す
- **identity 文字列は内部の値だが、自由には変えられない**。置き場所の `index_meta` は
  schema が契約ではないが、その形 (prefix・区切り・hash の入力や長さ) を変えると、
  **外部 provider で作ったすべての索引が、作り直すまで開くのを拒否する**。
  不一致のメッセージは、索引ではなく**いま実行している側の identity** で選ばれる:
  現在の設定が外部 provider を選んでいれば
  `groove --config <cfg> index --kb-path <path> --force` を、FastEmbed を選んでいれば
  `--force --model <id>` を示す (`grooveseek/src/db/meta.rs` の `verify_embedding_meta`)
- **blocking のクライアントは tokio の worker の上には居られない**。そこで作ると
  `Cannot drop a runtime in a context where blocking is not allowed` で panic し、
  クライアントは最初の embed 呼び出しで遅延生成される。検索の handler は元から
  blocking pool で走っていた。watcher はそうではなかったので、event の batch ごとに
  `spawn_blocking` へ移すようにした — 受信ループ全体ではなく batch ごとなので、
  shutdown は応答し続ける (`grooveseek/src/watcher.rs` の `run_watch_loop`)。
  **試験が守れる範囲はこれより狭い**。
  `openai_compatible_embeds_on_first_call_inside_a_tokio_runtime` が固定するのは、
  runtime の中で `spawn_blocking` 越しに呼べば provider が動くことである。
  **watcher の経路が実際に `spawn_blocking` を通ることを守るのは、レビューだけである**
- **新しい Rust の型は互換の約束の外にある**。`EmbeddingSettings`・
  `Embedder::with_settings`・`OpenAiCompatibleConfig` が公開なのはバイナリがそれで
  組まれているからであり、[Rust ライブラリ API](../stability.ja.md#rust-ライブラリ-api) の節と
  その根拠の [ADR-0008](0008-declare-what-1-0-freezes.ja.md) によって、自由に変えてよい
- **運用者がデータの流れについての判断を背負う**。`endpoint` で応答するものは、
  索引するすべての chunk と検索するすべての query を、URL が名指す transport の上で
  平文として受け取る。GrooveSeek はそれがどこか、誰が動かしているかを検査しない。
  やるのは、資格情報を埋め込んだ URL を拒否すること、`Debug` 出力に `api_key` を出さないこと、
  transport のエラーから URL を落とすこと、エラー本文を 512 byte で切って escape することで、
  **表示する拒否文と warning は ASCII のまま、秘密を映し返さない**
- **endpoint でどのモデルが応答するかを保つのは運用者である**。GrooveSeek が記録するのは
  alias であって、その裏のモデルではない。alias の裏のモデルを差し替えても索引は
  これまでどおり開き、以後の検索は古い document ベクトルを、別の embedding 空間の
  query ベクトルと比べることになる。**それを知らせるものは無く**、運用者が索引を
  作り直すまで 2 つの空間は混ざったままである

## 参考

- issue #312 (<https://github.com/alphabet-h/grooveseek/issues/312>) — 要望、オーナーが
  決めた scope と non-goals。PR #314 (<https://github.com/alphabet-h/grooveseek/pull/314>) —
  境界。PR #316 (<https://github.com/alphabet-h/grooveseek/pull/316>) — HTTP provider
- [ADR-0008](0008-declare-what-1-0-freezes.ja.md) — Rust API が安定性の約束の外にある理由
- [ADR-0013](0013-compile-in-one-grammar-and-load-the-rest.ja.md) — 依存とバイナリの大きさを
  天秤にかけた先例
- [ADR-0016](0016-keep-the-plugin-directory-outside-the-knowledge-base.ja.md) — R7 が
  再利用する untrusted config の境界
- [ADR-0021](0021-take-the-socket-you-were-given.ja.md) — 新しい設定キーに伴う同じ version の下限
- [usage.ja.md](../usage.ja.md#外部の-openai-互換-embedding) — 外部 provider の設定方法
- `grooveseek/src/embedder.rs` (`EmbeddingProvider` / `EmbeddingSettings` /
  `FastEmbedProvider` / `OpenAiCompatibleProvider` / `OpenAiCompatibleConfig`)、
  `grooveseek/src/config.rs` (`EmbeddingConfig` / `restrict_untrusted` /
  `classify_trust` / `resolve_embedding`)、`grooveseek/src/db/meta.rs`
  (`verify_embedding_meta`)、`grooveseek/src/watcher.rs` (`run_watch_loop`)
- 英語版:
  [0022-embedding-provider-boundary.md](0022-embedding-provider-boundary.md)
