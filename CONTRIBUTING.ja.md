# kb-mcp への貢献

コントリビュート検討ありがとうございます。必要最低限の開発情報をここにまとめます。

> **English version**: [CONTRIBUTING.md](./CONTRIBUTING.md)

## 前提

- Rust stable (edition 2024)
- Git
- ONNX モデルキャッシュ用に約 4.7 GB の空き容量 (`--ignored` テスト実行時のみ。BGE-small ~130 MB + BGE-M3 ~2.3 GB + BGE-reranker-v2-m3 ~2.3 GB)

## 初回セットアップ

clone 後に一度だけ、リポジトリ同梱の git hooks を有効化する:

```bash
git config core.hooksPath .githooks
```

これで `.githooks/pre-push` が有効になり、push のたびに `cargo fmt --all -- --check` が走るので、`cargo fmt` の入れ忘れが CI に到達する前にローカルで止まる。hook 本体はリポジトリで共有 — [`.githooks/pre-push`](./.githooks/pre-push) を参照。緊急時に bypass したい場合は push に `--no-verify` を付ける。

## ビルドとテスト

```bash
cargo build --release      # release バイナリ: target/release/kb-mcp(.exe)
cargo check --all-targets  # 型検査のみ (高速)
cargo test                 # ユニット + integration テスト (モデル DL 不要)
cargo test -p kb-mcp --lib <name>  # 名前指定で 1 本だけ実行 (workspace に複数 crate が
                                  # あるため -p が要る。--lib で integration test binary を除外)
```

CI と同じ検証をローカルで再現するには、次のすべてを通す必要がある。**`cargo clippy --all-targets` だけでは CI と一致しない** ので、ローカルで緑でも CI が落ちうる:

```bash
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo clippy --all-targets --features test-helpers,heavy-bench -- -D warnings
cargo test --test index_progress_cli -- --test-threads=1   # 先に、シングルスレッドで
cargo test
```

最後の 2 つは**順序が意味を持つ** (モデルキャッシュが cold の場合)。CI も同じ順で回している。`index_progress_cli` は BGE-small を必要とする `kb-mcp` サブプロセスを複数起動するので、並列で走らせると HuggingFace の download lock を奪い合って `Lock acquisition failed` で落ちる。この target を先にシングルスレッドで回せば DL するプロセスがちょうど 1 つになり、後続のフルスイートは warm cache に対して走る。

> **`cargo test -- --ignored` は実際にマシンの状態を変える。** 実行前に次節を読むこと。

- コミット前に `cargo fmt --all` (pre-push hook と CI の両方で強制)
- 日本語 KB 固有のロジック (CJK トークナイズ、日付書式等) に関する日本語コメントは歓迎、それ以外は英語推奨

## リポジトリ構成

- `kb-mcp/src/parser/` — `Parser` trait + `Registry` (形式ごとに impl 1 個)
- `kb-mcp/src/indexer.rs` — `walkdir` → パース → 埋め込み → 格納のパイプライン
- `kb-mcp/src/db.rs` + `kb-mcp/src/db/` — SQLite + sqlite-vec + FTS5。v0.15.0 で分割: `schema.rs` (スキーマ作成 + 前方マイグレーション) / `storage.rs` (CRUD) / `search.rs` (ベクトル KNN・FTS 候補・RRF 融合 = `search_hybrid`、既定 k=60) / `meta.rs` (`index_meta` の key/value) / `fts_query.rs` (クエリを per-token FTS phrase にコンパイル、v0.16.0+)
- `kb-mcp/src/embedder.rs` — `fastembed-rs` ラッパ (embedding + cross-encoder reranker)
- `kb-mcp/src/mmr.rs` — MMR 多様性再ランク (`mmr_select`、v0.7.0+)
- `kb-mcp/src/parent.rs` — Parent retriever 表示時 content 展開 (`apply_parent_retriever`、v0.7.0+)
- `kb-mcp/src/server.rs` — `rmcp::ServerHandler`、6 つの MCP ツール
- `kb-mcp/src/transport/` — stdio と Streamable HTTP
- `kb-mcp/src/watcher.rs` — `notify-debouncer-full` ベースの増分再インデックス
- `kb-mcp/src/schema.rs` — frontmatter スキーマ検証
- `kb-mcp/src/quality.rs` / `kb-mcp/src/graph.rs` — 品質フィルタ + BFS connection graph
- `kb-mcp/src/eval.rs` — `kb-mcp eval` 用の retrieval 品質評価 (任意)
- `kb-mcp/src/config.rs` — `kb-mcp.toml` 4 階層探索 + CLI 引数とのマージ
- `kb-mcp/src/markdown.rs` — `parser::markdown` を再公開する後方互換 shim
- `kb-mcp/src/indexer/progress.rs` — `kb-mcp index` の per-file 進捗出力 (`--quiet` / `--progress`)
- `kb-mcp/src/service/` — `kb-mcp service install/uninstall/status` (systemd-user / LaunchAgent / Task Scheduler)
- `kb-mcp/src/tune.rs` + `kb-mcp/src/tune/` — `kb-mcp tune` の fusion パラメータ sweep: `grid.rs` (sweep グリッド) / `stats.rs` (集計) / `report.rs` (描画)
- `kb-mcp/src/links.rs` — index / watcher / `get_document` の 3 面が共有する hardlink 検出 (v0.19.0+)
- `kb-mcp/src/poison.rs` — poison した mutex から復帰する (panic を引き継がない、v0.19.0+)
- `kb-mcp/src/test_support.rs` — テスト共有ヘルパ。特に `unique_temp_path` (本リポジトリは意図的に `tempfile` crate を使わない。理由は同ファイルのコメント参照)
- `crates/kb-mcp-tray/` — Windows system tray モニタ (`kb-mcp-tray.exe`、v0.9.0+)
- `crates/kb-mcp-svc/` — scheduled task が起動する Windows hidden-console launcher (v0.9.1+)
- `kb-mcp/tests/` — 統合テスト、`kb-mcp/benches/` — criterion ベンチ

詳細は [docs/ARCHITECTURE.ja.md](./docs/ARCHITECTURE.ja.md) を参照。

## テストの 2 層構造

- **軽量テスト**: 既定の `cargo test`。ネットワーク・モデル DL 不要、秒オーダーで完了。**PR を gate するのはこの層だけ** (`ci.yml` はこれしか回さない)
- **ignored テスト** (`#[ignore]`): `cargo test -- --ignored` で opt-in。PR は gate しないが**手動専用でもない**: `nightly.yml` が毎日 ubuntu-latest と windows-latest の両方で `cargo test --features test-helpers -- --include-ignored` を回している (ただし 1 日遅れ、かつ Windows leg は ~2.3 GB のモデルを要する 3 本を除外)。この 1 つのフラグの裏に**性質の違う 2 種類のコスト**が同居している:
  - **モデル DL** — 初回に ONNX モデルを取得する (BGE-small ~130 MB / BGE-M3 ~2.3 GB / BGE-reranker-v2-m3 ~2.3 GB)。以降は OS 標準キャッシュに残る。ネットワーク都合で DL が失敗する場合は README の「HuggingFace の TLS 失敗への対処」節を参照
  - **マシンの状態を実際に変える** — 一部のテストは OS のサービスを本当に登録・解除する。`kb-mcp/tests/service_install_integration.rs` は Windows で `Register-ScheduledTask` を呼び、`crates/kb-mcp-tray/tests/install_integration.rs` は `%APPDATA%\…\Start Menu\Programs\Startup\` にショートカットを書く。PID ごとに固有のサービス名を使い後始末もするが、**途中で kill すると scheduled task や startup ショートカットが残る**。途中で落ちたら `Get-ScheduledTask -TaskName 'kb-mcp*'` で確認すること

  `cargo test -- --ignored` は習慣ではなく**意図して**実行する。DL コストだけ払いたいなら対象を絞る: `cargo test --test <name> -- --ignored`

embedder / reranker が必要なテストを追加するときは `#[ignore]` を付け、何を検証するかコメントで記述する。OS に触れるテスト (サービス、自動起動、レジストリ) の場合は、コストが呼び出し側から見えるよう `#[ignore = "…"]` の理由文字列にその旨を書くこと。

### retrieval 品質ゲート

`kb-mcp/tests/eval_corpus_quality.rs` は、変更が検索を**壊した**かではなく**悪くした**かを測る唯一のテスト。`tests/fixtures/kb-eval/` (commit 済みの日英 20 文書) を index し、`tests/fixtures/kb-eval-golden.yml` の golden を `kb-mcp eval` に通して、集計 recall@1 / MRR が実測由来の下限を割ったら fail する。BGE-small 版が感度の高い方で nightly の両 leg で走り、BGE-M3 版は日本語の意味検索経路を守る Linux 専用。

retrieval に触れる変更 (クエリのコンパイル、fusion、chunk 分割、parser、MMR) はこのゲートを動かす。閾値を触る前に必ず失敗出力を読むこと — rank 1 を落としたクエリ、期待していた文書、代わりに 1 位になった文書がすべて出る。**下限を下げるのは「検索が悪くなるのを受け入れる」という判断**なので、新しい実測値と併せて PR の説明に書く。現在の baseline と測り方はモジュール doc に記録してある。

### カバレッジの下限

`nightly.yml` は `cargo-llvm-cov` で行カバレッジも測っており、**1 ファイルでも** 35% を割ったら fail する。これは品質目標ではなく「テストが 1 つも無いまま入った」を捕まえる仕掛けで、全体の ~86% よりずっと低いのは意図的 — 全体値はそのままでは閾値に使えないため。ファイル内の `#[cfg(test)]` モジュールが値を押し上げ、`#[ignore]` からしか到達しないコードは 0% に見えて押し下げ、Windows/macOS 専用コードは Linux leg の分母にそもそも入らない。真ん中の理由で 3 ファイルを除外してあり、それぞれ workflow に名前と理由が書いてある。

つまり、モジュールを足すならテストも一緒に足すこと。下限を割った時は、該当ファイルとその値が GitHub の error annotation として出る (cargo-llvm-cov 自体は exit 1 するだけでファイル名を言わない)。ファイル単位の表は失敗時でも job summary に残る。

## 変更の提出

1. リポジトリを fork し、`main` からブランチを切る
2. 新しい挙動にはテストを追加 (ユニットは inline、integration は `tests/` 配下)
3. `cargo fmt --all && cargo clippy --all-targets -- -D warnings && cargo test` を pass
4. 問題と変更内容を明示した PR を開く (関連 issue があればリンク)

## バグ報告

以下を含めて issue を開いてください:
- 最小再現手順 (コマンド、必要に応じて小さな KB サンプル)
- `kb-mcp --version`
- OS と Rust toolchain バージョン (`rustc --version`)
- 期待する挙動 vs 実際の挙動

## ライセンス

貢献によって、あなたのコントリビュートは本プロジェクトと同じ **MIT OR Apache-2.0** デュアルライセンスで扱われることに同意したものとみなします。[LICENSE-MIT](./LICENSE-MIT) / [LICENSE-APACHE](./LICENSE-APACHE) を参照。
