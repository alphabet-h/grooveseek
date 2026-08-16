# 7. プロジェクトを GrooveSeek に改名し、コマンドは `groove` とする

- Status: accepted
- Date: 2026-08-17
- Deciders: project owner
- Applies to: v0.26.0

## 背景と課題

本プロジェクトは 0.x を 25 回、`kb-mcp` という名前でリリースしてきた。この名前の
問題は 2 つあり、どちらも 1.0 が視野に入って初めて blocker になった。

**同じことをするプロジェクトに同じ名前を取られている。**
`github.com/moikas-code/kb-mcp` は自身を "cli tool and mcp server to help ai
manage a knowledge base of your code projects" と説明している。同カテゴリ、同名で、
どちらを探した利用者も両方に行き着く。

**名前が製品をプロトコルに縛っている。** MCP はこのサーバの読まれ方の一方でしかない。
もう一方はブラウザ — 人が `/ui` を開いて自分のノートを検索する経路である。`-mcp` は
機械向けの半分だけを名乗り、人間向けの半分について何も言っていない。そして MCP が
廃れれば、名前だけが指す対象より長く残る。

どちらも新しい問題ではない。変わったのは、**この名前がまもなく永続化する**ことだ。
名前はラベルではなく、利用者のファイルシステムに書き込まれる実体である:

```
.kb-mcp.db                   索引
kb-mcp.toml                  設定
.kb-mcpignore                除外ファイル
.kb-mcp-eval-history.json    eval の履歴
KB_MCP_CONFIG_HOME           config home の override
<config_dir>/kb-mcp/<service>/   サービスの config home
```

1.0 の後に改名すれば、既存のインストールはすべて自分の DB も設定も登録済みサービスも
見失う。両方の名前を面倒見るなら「旧名も探す」層を 1.x の全期間にわたって抱えることに
なる。0.x で、かつ明示的に beta である今なら、その層は**そもそも要らない**。
**やるなら今しかなく、その窓は 1.0.0 で閉じる**。

## 判断基準

- 識別子は利用者のマシンに残る。つまり契約であり、契約を凍結するリリースの**後**では
  なく**前**に決めなければならない。
- 検索できない名前はタダではない。旧名は**自分と同じカテゴリの中で**衝突していて、
  これは最も損をする衝突の仕方である。
- コマンドは毎日打ち、除外ファイルは一目で読む。`.kb-mcpignore` は目で切れない。
  長さは趣味ではなく実コストである。
- 空き状況は**推測せず実測する**。改名を入れた後で取られていたと分かれば、二度目の
  改名になる。

## 検討した選択肢

約 90 語を crates.io / npm / GitHub search に対して実測した。200 でも 404 でもない
応答は `UNKNOWN` として記録し、**結論を出さない**ようにした。失敗した測定が
「空いている」に見えるのを防ぐためである。

- **`kb-mcp` 続投。** crates.io では空いているので、外部要因で強制された改名ではない。
  上記 2 点により却下。決定打は GitHub の同カテゴリ衝突。
- **説明的な名前** (`kbase` / `mdsearch` / `localrag` / `kbsearch`)。空いてはいるが、
  `mdsearch` は既に事実と違い (PDF と Office に対応済)、`localrag` は `-mcp` が
  プロトコルに縛るのと同じ形で流行語に縛る。
- **図書館の比喩** (`libris` / `athenaeum` / `slipbox` / `microgroove`)。`slipbox` と
  `microgroove` は他プロジェクトが使用中で、`microgroove` は同名の GitHub リポジトリが
  音楽ハードウェア領域に 2 つある。`athenaeum` は空きだが打てず、略せない。
- **`AkaStylus`** — 「アカシックレコードを読むスタイラス」。全レジストリで空き。
  略された `Aka` が日本語で赤 / 垢、英語で "a.k.a." に読めるため却下 (docs は英語)。
- **`GrooveSeek`** — 採用。レコードの溝 (groove) とディスクヘッドのシーク (seek)、
  すなわち「記録の中から欲しい部分を見つける」の両半分。

## 決定

**製品名は GrooveSeek。** crate は `grooveseek`、コマンドと on-disk 識別子はすべて
`groove` とする。

```
crate      grooveseek          crates.io / npm 空き、GitHub 衝突なし
command    groove              同名の標準コマンドなし
files      .groove.db  groove.toml  .grooveignore  .groove-eval-history.json
env        GROOVE_CONFIG_HOME  GROOVE_TRAY_LOG  GROOVE_BIN
satellite  groove-svc  groove-tray   (crate 名 = バイナリ名。どちらも publish しない)
```

**製品名と識別子を意図的に分けている。** `ripgrep` crate が `rg` というコマンドを
置くのと同じ形である。得られるものは 2 つ。`.grooveseekignore` では目で切れないものが
`.grooveignore` なら切れること。そして**将来もし製品名を変えても、利用者のディスク上の
ものは 1 つも動かなくてよい**こと — 二度目の改名は、今回がそうでないのとは逆に、
無料で済む。

MCP サーバが名乗る名前は `CARGO_PKG_NAME` 由来の `grooveseek` のままとする。
`serverInfo.name` はクライアントに報告される製品識別子であって、パスでも利用者が
打つ文字列でもないので、コマンドではなく製品に従う。

### 影響

- **v0.25.0 以前からの移行はしない。** 0.26.0 のバイナリは `.kb-mcp.db` を見つけず、
  `kb-mcp.toml` を読まず、旧名で登録されたサービスも認識しない。これは緩和ではなく
  **受け入れる**判断である。0.x は beta であり、今 互換層を足せばそれを 1.x 全体で
  抱えることになる。移行手順は CHANGELOG に置く。
- **環境変数も一緒に変わった。** `KB_MCP_CONFIG_HOME` / `KB_MCP_TRAY_LOG` /
  `KB_MCP_BIN` / `KBMCP_BENCH_KB` に別名は用意しない。
- **CHANGELOG と既存 ADR は書き換えていない。** v0.25.0 までは旧名で出荷され、
  release asset も旧名のままである。歴史を書き換えれば、この文書自身の記述が
  嘘になる。**2026-08-17 より前の日付の文書に出てくる `kb-mcp` は本プロジェクトを指す。**
  ADR 0003 がファイル名に `kb-mcpignore` を残しているのも同じ理由 (説明対象の
  ファイルは現在 `.grooveignore`)。
- **GitHub リポジトリは `alphabet-h/grooveseek` へ移した。** 旧 URL への clone /
  fetch / push は GitHub のリダイレクトで動き続けるが、**同一アカウントで `kb-mcp`
  という名前のリポジトリを作った瞬間にリダイレクトは死ぬ**ので、この名前は今後
  使わない。GitHub は Pages の URL をリダイレクトしないため、改名は Pages を
  作る前に行っている。
- **これで解決しないこと**: 名前は製品が何をするかを何も言っておらず、"groove" で
  検索すると音楽ソフトに行き着く。この代償は承知の上で払っており、その結果
  **README の 1 行目が効き所になる**。
