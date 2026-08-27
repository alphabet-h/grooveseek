# Claude Code / Cursor への接続

MCP クライアントを GrooveSeek に向ける方法 — stdio の `.mcp.json`、
複数クライアント同時接続のための HTTP トランスポート、その周辺。

> **English version**: [clients.md](./clients.md)

> **デプロイ用の完全なレシピは** [`grooveseek/examples/deployments/`](https://github.com/alphabet-h/grooveseek/tree/main/grooveseek/examples/deployments) **を参照**。3 パターン (個人 stdio / NAS 共有 = 1 writer + 多 read-only / 社内 HTTP サーバ = 1 サーバ + 多クライアント) で `groove.toml` / `.mcp.json` / systemd unit までセットで揃えてある。1 マシン上で複数 Claude Code を並行させる loopback daemon が要る場合は `groove service install` を使う (v0.8.0 で旧 `personal-http` レシピを置き換えた)。下のスニペットはそれらのレシピの中核を成す stdio エントリポイント。

プロジェクトルート (またはクライアント対応の MCP 設定場所) の `.mcp.json` に以下を追加:

<!-- groove-pin: mcp-stdio-snippet -->
```json
{
  "mcpServers": {
    "ai-knowledge": {
      "command": "/path/to/groove",
      "args": ["serve", "--kb-path", "/path/to/knowledge-base"],
      "type": "stdio"
    }
  }
}
```

多言語モデル + 再ランクを有効化する場合:

```json
{
  "mcpServers": {
    "ai-knowledge": {
      "command": "/path/to/groove",
      "args": [
        "serve",
        "--kb-path", "/path/to/knowledge-base",
        "--model", "bge-m3",
        "--reranker", "bge-v2-m3"
      ],
      "env": {
        "FASTEMBED_CACHE_DIR": "/path/to/.cache/huggingface/hub"
      },
      "type": "stdio"
    }
  }
}
```

エージェントワークフロー向けの保守的な案: reranker はロードするが既定はオフにしておき、呼び出し側が個別 `search` で `rerank: true` を指定してオプトインする:

```json
{
  "mcpServers": {
    "ai-knowledge": {
      "command": "/path/to/groove",
      "args": [
        "serve",
        "--kb-path", "/path/to/knowledge-base",
        "--model", "bge-m3",
        "--reranker", "bge-v2-m3",
        "--rerank-by-default=false"
      ],
      "env": { "FASTEMBED_CACHE_DIR": "/path/to/.cache/huggingface/hub" },
      "type": "stdio"
    }
  }
}
```

あるいは、[探索パス](configuration.ja.md#設定ファイルの探索順) のいずれかに `groove.toml` を置いて同じ項目を設定しているなら、`.mcp.json` はここまで縮められる:

```json
{
  "mcpServers": {
    "ai-knowledge": {
      "command": "/path/to/groove",
      "args": ["serve"],
      "type": "stdio"
    }
  }
}
```

クライアント接続時にサーバが自動起動する。

## PostToolUse hook による index 鮮度保守
Claude Code セッション内部からナレッジベースを編集する (または Markdown を書く skill を実行する) 場合、MCP サーバは再構築されるまで古い結果を返し続ける。`.claude/settings.json` の `PostToolUse` hook で書込み後に自動再 index できる。最小形:

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

`groove index` の SHA-256 差分検出により 2 回目以降は高速 (小さな KB なら大抵 1 秒未満)。ツールペイロードを精査して編集ファイルが `$KB_PATH` 配下のときだけ再構築する、より精密なシェルスクリプトがリポジトリ同梱 — [`grooveseek/examples/hooks/`](https://github.com/alphabet-h/grooveseek/blob/main/grooveseek/examples/hooks/README.ja.md) 参照。SQLite は WAL モードで動作するため、MCP サーバ起動中に hook が走っても安全。

## Frontmatter スキーマ検証
ナレッジベースで frontmatter 規約を運用しているなら (例: `title` 必須、`date` は YYYY-MM-DD、`topic` は enum)、以下でファイル毎の違反をチェックできる:

```bash
groove validate --kb-path /path/to/knowledge-base
```

`--kb-path` 直下に `groove-schema.toml` を置く (テンプレート: `groove-schema.toml.example`):

```toml
[fields.title]
required = true
type = "string"
min_length = 1

[fields.date]
required = true
type = "string"
pattern = '^\d{4}-\d{2}-\d{2}$'

[fields.topic]
required = true
type = "string"
enum = ["mcp", "rag", "ai", "tooling", "ops"]

[fields.tags]
required = true
type = "array"
min_length = 1
```

- **スキーマファイル無し → exit 0** と短い "no schema found" メッセージ。従来挙動を保持
- `--format text` (既定、TTY では色付き) / `json` / `github` (CI annotation 用)
- 終了コード: `0` (違反なし) / `1` (違反あり) / `2` (スキーマロードエラー)
- `.txt` は frontmatter の概念が無いのでスキップ
- `index` / `serve` コマンドには影響しない — 検証は opt-in のみ

## HTTP トランスポート (複数クライアント同時接続)
既定の `groove serve` は stdio で MCP を話す — 1 クライアント / サーバプロセス。複数クライアント同時接続 (例: 複数の Claude Code セッション、または外部スクリプトが同じ index を叩く) には Streamable HTTP に切替:

```bash
groove serve --kb-path /path/to/knowledge-base --transport http --port 3100
# または、このマシン以外からの接続を受ける場合: --bind 0.0.0.0:3100 --i-know
```

サーバは `/mcp` に MCP エンドポイントをマウントし、`/healthz` をヘルスプローブ用に公開する。HTTP 対応クライアントの `.mcp.json`:

```json
{
  "mcpServers": {
    "ai-knowledge": {
      "type": "http",
      "url": "http://127.0.0.1:3100/mcp"
    }
  }
}
```

セキュリティ注意:
- 既定 bind は `127.0.0.1:3100` (loopback)。**groove は認証機構を内蔵していない**ので bind アドレスが実質唯一のアクセス制御 — `--bind 0.0.0.0:3100` は信頼できるネットワークでのみ使用する。v0.17.0 以降、非 loopback の `--bind` は `--i-know` を付けないと拒否される (`groove service install` と同じ規約)。`groove.toml` の `[transport.http].bind` 由来の非 loopback bind は既存のサービス構成を壊さないよう **gate しない**。起動時の警告が出るのは Host allow-list が未設定または空のときだけで (次の 2 項目を参照)、`allowed_hosts` を明示してある構成は「意図的な公開」とみなして黙る
- **Host ヘッダの検証は GrooveSeek 自身が行う** (既定で loopback のみ)。DNS rebinding 攻撃を防ぐためで、rmcp 側の検査は切ってあり、検査が走る面ではどこでも 1 つの gate が答える ([ADR-0009](decisions/0009-one-dns-rebinding-gate.ja.md))。ただし **Host 検証は認証ではない** — ポートに到達できる相手は `Host: localhost` を自由に付けられる。ブラウザ側の防御と考え、到達性はネットワーク層で絞ること
- LAN / イントラ公開時は `groove.toml` の `[transport.http].allowed_hosts` に公開ホスト名 / IP を明示する (例: `["kb.example.lan", "192.168.1.10"]`)。loopback only の default のまま 0.0.0.0 で bind すると外部リクエストは Host 検証で 403 になる — operator のミス確定なので、groove は起動時に `tracing::warn` を出して気付かせる。`allowed_hosts = []` (空配列) を渡すと Host 検証が完全に無効化され、非 loopback bind と組み合わせるとポートに到達できる全員に `/mcp` が開く — この組合せも起動時に警告するようにした

- **`Origin` 検証は `allowed_hosts` と違い既定で有効**。MCP 仕様は Streamable HTTP サーバについて *"**MUST** validate the `Origin` header on all incoming connections to prevent DNS rebinding attacks"* と定めているので、`[transport.http].allowed_origins` を省略した場合は「全部許可」ではなく **bind した port の loopback origin** (`http://localhost:PORT` / `http://127.0.0.1:PORT` / `http://[::1]:PORT`) を許可する。`Origin` ヘッダを持たない要求 — 通常の MCP クライアント / tray / `curl` — は RFC 6454 のとおり素通りするので、既存の利用は壊れない。この検査が止めるのは「利用者自身のブラウザに開かれた別サイトの JS がこのポートへ到達すること」だけで、**2 つ目のアクセス制御ではない**。reverse proxy 越しではブラウザ側のクライアントが公開 origin を送るため、それを明示する。**このキーを書くと既定リストは「追加」ではなく「置換」される**ので、ブラウザ上のクライアントが loopback 経由でも来るなら loopback の分も併記する: `allowed_origins = ["https://kb.example.com", "http://127.0.0.1:3100", "http://localhost:3100"]`。空リストは検証を無効化し、起動時に警告が出る

- **`Origin` 検証が掛かるのは `/mcp`。そして `/ui` は `/mcp` 経由で検索する。** つまりこのリストが**組み込みページで検索できるかどうかを決める** — 既定を公開 origin だけで置き換えると、`/ui` は表示されるのに問い合わせが全部拒否される。**`allowed_hosts` でも 1 段手前で同じことが起きる** — Host 検証が先に走るので、ドキュメントどおりの LAN 構成 (`allowed_hosts = ["kb.example.lan"]`) は、ローカルで開いた `/ui` が送る `Host: localhost` を拒否する。どちらのキーも既定を**拡張ではなく置換**するので、**実際にブラウザで使う名前と origin をそのまま列挙する** — `allowed_hosts = ["127.0.0.1"]` でも `localhost` で開いたページは拒否される。サーバが起動時に警告するのは「loopback のエントリが 1 つも無い」場合だけで、**「1 つはあるが使っているアドレスと違う」場合は警告しない**。そのため `/ui` は検索が拒否された時に**必要な host と origin を画面に出す**。**検査が走る面については、1 つの検査が答える** — `/mcp` を rmcp に任せず GrooveSeek 自身が行うので、**2 つの面が同じ値を 2 通りに読むことはもう起きない**。以前 5 つの `Host` 綴りで実際に起きていたこと、および `/mcp` がそれらを拒否するようになった理由は [ADR-0009](decisions/0009-one-dns-rebinding-gate.ja.md) に記録した。**ただし「何と照合するか」は route ごとに違う** — このキーが届くのは `/mcp` と admin route で、admin の `Host` は独自の loopback 限定リストと照合され、`/healthz` は `Host` だけを、しかも `healthz_public = false` の時だけ検証する。`Origin` を持たない要求は、検査が走る面ではどこでも素通りする — ページ自身の status polling と tray が送るのがそれである
- 並列リクエストはサーバ内の lock の前で並ぶ: `search` は embedder / reranker / DB の 3 つの mutex を取り、後の 2 つは pipeline の終わりまで握る。同時 8 client での実測 (`cargo test -p grooveseek --release --test http_lock_contention -- --ignored`) では、`search` のスループットは 9,800 chunk のコーパスで ~7 → ~9 qps、800 chunk では ~12〜16 → ~13〜20 qps にしか増えず、レイテンシは client 数にほぼ比例して伸びる。表と、embed 1 本が既に全コアを使う間は lock を直しても大きくは買えない理由は [deployment-topologies.ja.md](deployment-topologies.ja.md#同時-client-が何を払うのか実測) に

## Web UI と admin API (HTTP transport のみ)

`serve` を `--transport http` で動かすと、`/mcp` と `/healthz` に加えて 2 つの
route が生える。有効化の設定は無く HTTP transport があれば常に存在し、どちらも
**loopback 限定**: middleware が peer アドレスが loopback でないリクエストを
拒否し、その後 `Host` ヘッダを loopback の別名 (`127.0.0.1` / `::1` /
`localhost`) と照合する。bind アドレスが追加されるのは **それ自体が loopback の
場合だけ** で、`0.0.0.0` に bind した時の `Host: 0.0.0.0` は意図的に拒否される
(LAN のブラウザが bind アドレス経由でこれらの route に到達しないため)。
`/mcp` 用に Host を allow-list していても、ネットワーク上の別マシンからは 403。
`Origin` は `Host` の**後**に、`[transport.http].allowed_origins` と照合する。

両 route は `X-Content-Type-Options: nosniff` と `Content-Security-Policy` を
付けて応答する。ポリシーは `default-src 'none'` に**このページが実際に使うもの
だけ**を戻した形 — 自前の inline `<script>` / `<style>`、`data:` の favicon、
同一 origin への `fetch`。`/ui` は外部リソースを一切読まないが、それを保つのが
このポリシーである。

| Route | 中身 |
| --- | --- |
| `/ui` | **運用者向けの画面**。状態帯 (version / 文書数 / チャンク数 / モデル / watcher / uptime / pid) の下に検索。検索は **`/mcp` を呼んで**行うので、このページ自体が「Streamable HTTP 上の MCP クライアントの最小の実例」になっている |
| `/api/admin/status` | daemon / indexing / watcher / KB の状態を JSON で返す。Windows tray が 5 秒間隔で polling しているのはこれで、上の状態帯もこれを読む |

> **`/api/search` は v0.27.0 で削除した。** `search` tool が取る 17 パラメータのうち 2 つしか受け取っておらず、プロセスの外から使う口としては `/mcp` の方が既に優れていたため。`/ui` も `/mcp` を使うようになった。[docs/stability.ja.md](stability.ja.md) 参照

```bash
curl http://127.0.0.1:3100/api/admin/status
```

```json
{
  "daemon":   { "version": "0.13.1", "pid": 36400, "uptime_secs": 4210, "started_at": "2026-07-26T09:12:03Z" },
  "indexing": { "active": false, "started_at": null, "progress": null },
  "watcher":  { "active": true, "debounce_ms": 500 },
  "kb":       { "documents": 596, "chunks": 8878, "model": "bge-m3" },
  "config_source": "Cwd"
}
```

> **`kb.path` は削除した。** ナレッジベースの絶対パスを持っていたので、Windows では
> payload も、それを表示していた状態帯も `C:\Users\<名前>\...` と読めていた。
> 読んでいた消費者はいない (tray が読むのは `daemon.pid` と `indexing.active`)。
> [docs/stability.ja.md](stability.ja.md) 参照

`/ui` は Windows tray の **Open Web UI** が開くページだが、Windows 専用ではない。
Linux / macOS では daemon が動いているマシン上でブラウザから開くか、ポートを
forward する:

```bash
ssh -L 3100:127.0.0.1:3100 kb-server.lan   # → http://127.0.0.1:3100/ui
```

これらの route を reverse proxy に **map しないこと**: proxy 自身が loopback
peer で、既定の `Host` も allow-list に載るため、`/ui` を proxy すると proxy に
到達できる相手全員にページが渡る。転送するのは `/mcp` と `/healthz` だけにする。

## ライブ同期 (file watcher)
`groove serve` は既定で `notify` ベースのファイルウォッチャを走らせる。`--kb-path` 配下の任意の変更 (create / modify / delete / rename) が検知され、debounce ののち該当ファイルのみが再インデックスされる。手動の editor save・`git pull`・外部スクリプトといった、PostToolUse hook では捕まえられないケースをカバーする。

- **既定 on**。`groove.toml` の `[watch].enabled = false` または CLI `--no-watch` で無効化
- **Debounce** は既定 500 ms。`[watch].debounce_ms` または `--debounce-ms` で調整
- **PostToolUse hook と共存**。両経路は同じ `Mutex<Database>` / `Mutex<Embedder>` をロックするため、同時トリガは Rust 層で直列化され冪等
- **拡張子対応**。watcher は `rebuild_index` と同じ Parser registry を共有し、`[parsers].enabled` で有効化された拡張子のファイルのみを再インデックスする。他イベントは破棄
- **耐障害性**。watcher タスク内部のエラーは stderr にログされ (黙殺しない)、MCP サーバは動作し続ける。ローカルディスクを想定 — WSL / SMB / ネットワーク共有上の inotify は保証外
- **バックプレッシャ (v0.6.0+)**。debouncer から indexer task へのブリッジは bounded な 64 batch channel。consumer が追い付けない場合 (embedder が一時停止中など) は無限に queue が伸びることはなく、超過 batch を warn ログ付きで drop する。バースト後に `rebuild_index` を手動実行で取り漏らしを補える

## grammar plugin の置き方 (v1.3.0+)

groove はソースコードを定義 1 つ = chunk 1 つで parse するが、バイナリに焼き込んであるのは Rust だけ。他の言語はすべて、自分で DL して置く小さなライブラリになる。この非対称と「なぜ feature flag ではないのか」は [ADR-0013](decisions/0013-compile-in-one-grammar-and-load-the-rest.ja.md) にある。

1. [releases ページ](https://github.com/alphabet-h/grooveseek/releases)で、**使っている groove の版**の `groove-grammar-<言語>-<target>` アーカイブを探す。plugin とバイナリは ABI 版を共有するので、別の release のものは拒否されうる。
2. **展開する前に checksum を検証する。** どのアーカイブにも `.sha256` が並んで公開されている。ライブラリを開くと、groove が symbol を 1 つも見ないうちに**そのライブラリ自身の初期化が走る**ので、すり替えられた / 壊れたアーカイブは「自分の権限で動くネイティブコード」そのものであり、あとから groove が何を検査しても変わらない。**ファイルを開く前**に済ませる必要があるのはこの手順だけ。

   ```bash
   # Linux / macOS
   sha256sum -c groove-grammar-python-x86_64-unknown-linux-gnu.tar.xz.sha256
   ```

   ```powershell
   # Windows
   (Get-FileHash groove-grammar-python-x86_64-pc-windows-msvc.zip -Algorithm SHA256).Hash -eq `
       (Get-Content groove-grammar-python-x86_64-pc-windows-msvc.zip.sha256).Split()[0].ToUpper()
   ```
3. 展開して、ライブラリを grammar ディレクトリに置く。既定は Windows なら `%LOCALAPPDATA%\groove\grammars`、Linux なら `~/.local/share/groove/grammars`、macOS なら `~/Library/Application Support/groove/grammars`。別の場所にするなら `groove.toml` の `grammar_dir` か、環境変数 `GROOVE_GRAMMAR_DIR` を使う — こちらは**絶対パス必須**で、相対値だと「クライアントがたまたま groove を起動したディレクトリ」に対して解決されてしまうため。
4. `[parsers].enabled` にその言語を足す (例: `enabled = ["md", "py"]`)。
5. **service に任せる前に、`groove index` を 1 回手で走らせる。** 登録した Windows service は stdio を捨てるので、plugin が無い / 拒否された場合の**メッセージがどこにも出ず**、daemon がただ動かないという状態になる。`groove index` は DB を開くよりもモデルを読むよりも先に有効化した言語をすべて解決するので、壊れた plugin はその場で、何も作らずに、画面の上で止まる。(`groove doctor` は**既にある索引**を検査するコマンドで、索引がまだ無い状態では plugin に触れる前に「No index found」と答える。この手順には使えない。)

自動 DL は一切しないし、`enabled` に書いた言語以外は開かない — そのディレクトリに有効化していない言語のファイルがあっても触らない。有効化した言語の plugin が使えない場合、コマンドは「どのファイルをどこに置くべきか」を告げて止まる。ソースを plain text として索引するフォールバックはしない。

**plugin を差し替えたら `groove index --force` で索引し直すこと。** grammar を作り直すと同じファイルでも chunk の切れ目が動きうるが、索引は内容が変わっていないファイルを飛ばすので、普通に index し直しても**そのファイルは古い grammar が切った chunk のまま**残り、新しい grammar はその後編集したファイルにだけ効く。結果として索引が 2 世代を同時に抱える。**現時点では groove がこれを検出して警告することはない**ので、`--force` は利用者の側の判断になる。groove 本体を上げた時も同様。**`[parsers.code].max_chunk_chars` を変えた時だけは groove が報告する**: 索引はコード chunk を切った時の budget を記録しているので、違う値で次を走らせると `--force` を名指しする warning が出る。

> **grammar plugin は groove が自分のプロセスへ読み込むネイティブコード。** インストールする他のバイナリと同じ扱いにすること — 使っている版の release ページから取り、それ以外の場所からは取らない。groove が**見つけただけ**の `groove.toml` (`--config` で名指ししていないもの) がこのディレクトリを選べないのも同じ理由: [信頼する置き場所 / しない置き場所](configuration.ja.md#信頼する置き場所--しない置き場所) を参照。

## HuggingFace の TLS 失敗への対処 (初回 DL 時)

環境によっては (企業プロキシ、TLS inspection を行うファイアウォール) fastembed の native TLS 接続が `huggingface.co` に対して `os error 10054` / "Connection was reset" で失敗する。その場合は Python の HuggingFace CLI で事前にモデルを DL し、`FASTEMBED_CACHE_DIR` で HF Hub キャッシュを指す:

```bash
# 一度インストール
pip install --user huggingface_hub

# BGE-M3 を事前 DL (必要な ONNX ファイルのみ)
hf download BAAI/bge-m3 \
    --include 'onnx/*' 'tokenizer*' 'config.json' 'special_tokens_map.json'

# BGE-reranker-v2-m3 を事前 DL (`--reranker bge-v2-m3` 用)
hf download BAAI/bge-reranker-v2-m3

# HF cache を指して groove を起動 (HF Hub cache は fastembed と互換)
FASTEMBED_CACHE_DIR=~/.cache/huggingface/hub \
    groove index --kb-path ./knowledge-base --model bge-m3 --force
```

## Related

- `docs/mcp-tools.ja.md` — 繋いだクライアントが呼べるもの
- `docs/configuration.ja.md` — 同じ項目を `groove.toml` のキーで書く
- `README.ja.md` — インストールとクイックスタート
