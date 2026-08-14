# 3. `.kb-mcpignore` は索引の境界であってアクセスの境界ではない / `ignore` はマッチャとしてのみ使う

- Status: accepted
- Date: 2026-08-15
- Deciders: プロジェクトオーナー
- Applies to: v0.21.0

## 背景と課題

v0.21.0 より前、索引から何かを外す手段は `exclude_dirs` だけだった。これは
**ディレクトリ basename の完全一致リスト**なので、「`drafts/*.md` だけ外す」
「`*.tmp.md` を外す」「`archive/2024/**` を外す」が書けない。素直な解は
KB に gitignore 構文のファイルを置くことで、Cursor も ripgrep も大半の開発者向け
ツールもそうしている。

実装に入る前に答えを出す必要がある問いが 2 つあり、どちらも好みで決められない。

**1 つ目: このファイルは何を境界にするのか。** kb-mcp には `exclude_dirs` に
ついて既に意図的な答えがあり、テストで pin までされている — 「**索引しない**」
であって「読ませない」ではない。除外ディレクトリ配下のファイルは `search` に
出ないが、パスを知っている呼び出し元には `get_document` が返す
(`validate_get_document_path` は除外引数を受け取らず、
`document_in_excluded_dir_is_still_readable` がそれを pin している)。新しい
ignore ファイルはこの契約を踏襲することも破ることもできる。業界も割れており、
Cursor はまさにこの区別のために **2 枚**のファイルを持っている —
索引用の `.cursorindexingignore` とアクセス用の `.cursorignore`。

**2 つ目: `ignore` crate をどこまで使うか。** この crate はマッチャ
(`Gitignore`) だけでなくディレクトリ walker (`WalkBuilder`) も提供する。
kb-mcp は既に `walkdir` で歩いており、しかもその判断が **3 箇所**に分かれていて、
過去に 2 度食い違っている。

## 決定要因

- 3 面 — full index walk、`validate` walk (binary target 側にいる)、live watcher
  — が同じ問いに違う答えを返せてはならない。AU-03 は watcher だけ hardcoded
  denylist を持たずに出荷され、BU-19 は walk 側だけ大文字小文字を無視するように
  なって「full index は `Build/` を skip するのに watcher は index し続ける」状態で
  出荷された。**どちらもリリース後に見つかっている**
- ガードは実力以上のことを主張してはならない。README には既に「KB ディレクトリは
  セキュリティ境界ではない」と書いてあり、BU-20 はレビューで 2 度「その主張は
  挙動より強い」と指摘されている
- `.kb-mcpignore` を置かない KB の挙動は 1 バイトも変わってはならない
- gitignore は角の立った実仕様である (アンカーはパターンに `/` を含むかで変わり、
  `!` は非対称にブロックし、`**` は 3 通りの意味を持ち、`[B-a]` はバイト順の範囲)。
  各言語の独立実装は実際に互いに食い違っている

## 検討した選択肢

**ファイルの効果範囲**

1. 索引のみ。既存の `exclude_dirs` 契約を踏襲する
2. 索引 + アクセス。`get_document` / `get_best_practice` も拒否する
3. 2 枚に分ける (Cursor 方式)

**実装**

1. `ignore::WalkBuilder` に乗り換え、`walkdir` を置き換える
2. `ignore::gitignore::Gitignore` を**マッチャとしてのみ**使い、`walkdir` は残す
3. 既に入っている `globset` の上に gitignore セマンティクスを自作する

## 決定

**索引のみ (選択肢 1)、`ignore` はマッチャとしてのみ (選択肢 2)。**

効果範囲は「このファイルが実際に保証できること」から決まる。KB に書ける者は
`.kb-mcpignore` を消すこともできる以上、**木の中に置いたルールがその木を守る
境界にはなり得ない**。ignored なパスに対して `get_document` を拒否すると、
「任意の書き手が消せるファイル」の上に立ちながら見た目はアクセス制御になる —
BU-20 が訂正させられたのと同じ形である。2 つの除外機構で契約が 1 つに揃うのは
説明としても単純だ: *除外されたものは決して索引されない。そして索引されるか
どうかは読めるかどうかの境界ではない。* 読ませたくないものは `kb_path` の外 —
README がずっとそう書いている。

選択肢 3 は「見返りの無い概念の増加」として却下した。ファイル・ドキュメント・
テストがすべて倍になるが、表現しようとしている区別の強い方の半分は、いま
提供しないと決めたばかりのものである。

実装側の決定は実測が出した結論だ。`WalkBuilder` の既定は、既存 KB の挙動を
**目に見えない形で**変える: `hidden()` が既定 true で、しかも Windows の
「hidden」は「dot 始まり **または** `FILE_ATTRIBUTE_HIDDEN` を持つ」なので、
ユーザがエクスプローラで隠したノートが黙って index から消える。`add_ignore` は
walk root ではなく**プロセスのカレントディレクトリ**基準で解決する (サービスと
して入れた daemon の cwd は任意)。`require_git` / `parents` / `git_ignore` /
`git_global` はすべて既定 on なので、KB がたまたま git リポジトリかどうかで挙動が
変わる。そして `filter_entry` は述語を 1 個しか取れず、ignore 判定との評価順序が
docs に書かれていない — kb-mcp は既にそこで hardcoded denylist・Office lock
ファイル・symlink・hardlink を判定している。

マッチャだけを取れば既存の walk は無傷で、より重要なことに、**3 面すべての
除外判定を 1 つの関数**にできる。この失敗形態には既に 2 回の代償を払っている。

選択肢 3 (globset で自作) は、独立実装同士が「誰もテストしない edge case」で
実際に食い違っているという証拠と、内部の先行知見 —
「手書きのマッチャにレビューが edge case を当て続けるのは library に委譲する
サイン」— の 2 点で却下した。

### 帰結

- `ignore` crate が新規の直接依存になる。11 の推移的依存はすべて既に
  `Cargo.lock` にあり (`globset` と `walkdir` は直接依存)、純増は 1 crate と
  `regex-automata` の 0.4.14 → 0.4.18 のみ
- `matched_path_or_any_parents` は**使わない**。まさに欲しい API に見えるが、
  実測では `["logs/", "!logs/important.md"]` に対して `logs/important.md` に
  `Whitelist` を返す。walk は `logs/` で止まってそのファイルに到達しない。
  watcher で使えば walk/watcher の drift が「どの API を呼んだか」のレベルで
  復活していた。祖先ループを自分で書き、**最初の除外された祖先で打ち切る**
- 照合は全プラットフォームで case-insensitive にする。git 本家の既定とは違うが、
  `exclude_dirs` と hardcoded denylist が既にそうであり (BU-19)、1 つの設定の中で
  2 つの除外機構が `Build` と `build` について食い違う方が、どちらの規則よりも悪い
- 読むのは KB ルートの 1 枚だけ。サブディレクトリのファイルも、`kb_path` より上も、
  `.gitignore` も見ない。階層ファイルは「walk と単発パス判定を一致させ続ける」のを
  難しくする当のものであり、`.gitignore` を尊重すると既存 KB の索引内容が黙って変わる
- このファイルはセキュリティ境界ではなく、そのことを module doc・README・
  `validate_get_document_path` の doc コメントに書く
- 境界が索引である以上、新たに除外された文書は次の full index で通常の削除 pass に
  よって DB から落ちる — 「集められなかった = 消えた」は既存の意味論そのもの。
  live watcher が新しい規則を適用するのは以降のイベントだけで、その旨をログに出す
