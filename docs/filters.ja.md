# 検索フィルタ

`search` MCP ツールは複数のフィルタを受け付ける。フィルタは **AND**
セマンティクスで合成される (すべての条件が一致した chunk のみ `results`
に現れる)。

> **English version**: [filters.md](./filters.md)

## クイックリファレンス

| パラメータ | 型 | 例 | 効果 |
|---|---|---|---|
| `category` | string | `"deep-dive"` | `documents.category` と完全一致 |
| `topic` | string | `"mcp"` | `documents.topic` と完全一致 |
| `path_globs` | string[] | `["docs/**", "!docs/draft/**"]` | glob include / exclude |
| `tags_any` | string[] | `["rust", "wasm"]` | OR — いずれかの tag が一致 |
| `tags_all` | string[] | `["draft"]` | AND — 全 tag が一致 |
| `date_from` | string | `"2026-01-01"` | hit.date >= from (lex 比較) |
| `date_to` | string | `"2026-12-31"` | hit.date <= to (lex 比較) |
| `min_quality` | number | `0.5` | quality filter の閾値 (`[quality_filter].threshold`) をこの呼び出しだけ上書き |
| `include_low_quality` | bool | `true` | この呼び出しでは quality filter を無効化 (`min_quality: 0.0` と等価、意図が明示的) |
| `min_confidence_ratio` | number | `1.5` | `low_confidence` フラグの閾値 |

## `path_globs`

- `!` 接頭辞のパターンは除外用
- 接頭辞なしのパターンは include 用
- path が **いずれかの include に一致** かつ **どの exclude にも非一致** なら通過
- 全部 `!` 接頭辞でも妥当: include 不在 = 「全件 include」と解釈
- **空配列 `[]` はエラーで reject**。filter 無効にしたいなら `null` (キーを省略)。
  exclude 専用にしたいなら `["**", "!a/**"]` のように include 用 `**` を明示

```jsonc
{
  "path_globs": ["docs/**", "!docs/draft/**"]
  // "docs/a.md" は通る、"docs/draft/b.md" は除外、"notes/c.md" は除外
}
```

## `tags_any` と `tags_all`

これらは `documents.tags` (YAML frontmatter の `tags:` 配列) を対象にする。

- **`tags_any`** = OR: hit が列挙された tag のいずれかを含めば通過
- **`tags_all`** = AND: hit が列挙された tag を全部含めば通過
- 両方指定時: `(tags_all を全部含む) AND (tags_any のいずれかを含む)`

```jsonc
{
  "tags_all": ["rust"],
  "tags_any": ["async", "concurrency"]
  // "rust" タグかつ ("async" or "concurrency") を持つ docs にマッチ
}
```

## `date_from` / `date_to`

- **`YYYY-MM-DD`** (推奨) または RFC 3339 タイムスタンプ
- 文字列の lex 比較なので、形式を揃えること
- **strict セマンティクス**: `documents.date` が `NULL` の chunk は
  `date_from` か `date_to` が指定されていれば除外される

```jsonc
{
  "date_from": "2026-01-01",
  "date_to":   "2026-04-30"
}
```

> **date 形式が混在**するとき (`"2026-04-26 12:00:00 +0900"` と
> `"2026-04-26T12:00:00+09:00"` など) は lex 順序が崩れる。KB 内で形式を
> 統一すること。

## `low_confidence` と `min_confidence_ratio`

レスポンス wrapper のトップレベルに `low_confidence: bool` が付く。top hit の
score が他と比べて **目立って高くない** ときに `true` になる:

```
low_confidence ⇔ (results.len() >= 2)
                 AND (mean(scores) > 0.0)
                 AND (max(scores) / mean(scores) < min_confidence_ratio)
```

- 分子は `max(scores)` であって **`results[0].score` ではない**。返却順が score 降順でない場合に両者は食い違う — MMR は多様性のために並べ替えるので、まさにそのケース
- 既定値 `min_confidence_ratio = 1.5` (最高 score が平均の 1.5 倍以上必要)
- `0.0` で判定を完全無効化
- リクエスト単位で `min_confidence_ratio` パラメータで上書き可、グローバル
  既定は `groove.toml`:

  ```toml
  [search]
  min_confidence_ratio = 1.5
  ```

`low_confidence: true` の意味は「マッチがダンゴ状態 — Claude は引用を
権威として扱うのを慎重に」。`results` 自体はそのまま返ってくる。フラグは
あくまで助言。

### 何を検出し、何を検出しないか

このフラグはヒューリスティックであり、**実測した**ので、式から推測させるのでは
なくここに書いておく。どれだけ重みを置くかを決める上で効く限界が 2 つある。

**rerank はこのフラグを丸ごと無効にし得る。** cross-encoder はロジットで
スコアを付けるので無関係な chunk は強く負になり、結果集合の平均は**しばしば負**に
なる。そうなると上の `mean(scores) > 0.0` が、散らばりに関係なく false を返す。
`bge-v2-m3` で 25 クエリ (各 10 件) を測った結果、**コーパスに答が無いクエリを
含めて全部 `false`** だった。

**手元のクエリでそうなるかは、モデル・件数・返った chunk の関連度で決まる** —
本当に良いマッチだけが少数返れば平均は正になり、比は通常どおり比較される。
つまり rerank 下でフラグが**必ず**消えるわけではない。**信用できない**のが正確な
言い方で、**rerank が走ったときの `false` は何も意味しない** (符号チェックが出す
`false` と区別が付かない)。`true` の方は式が言うとおりの意味を持つ。

**rerank 無しで反応しているのは、融合後スコア分布の形であって正解性ではない。**
RRF は hit が現れた脚ごとに `1 / (rrf_k + rank + 1)` を与えるので、
top1 を割る平均は **①各 hit を何本の脚が拾ったか ②各脚が何位に置いたか
③`rrf_k` ④要求件数**で動く。**どれも「top hit が正しいか」ではない。**

実際の代償は 2 つの測定が示している。20 文書のコーパスで **25 クエリ全部が
rank 1 で正解**しているのに **14 件で発火**した。そして同じクエリが別コーパスでは
まったく違う位置に来る: **答が無い** 12 クエリの median が、そのコーパスでは
**1.08**、121 文書のコーパスでは **1.40** — 後者は前者で**正解していたクエリ**が
居た位置である。

測定した標本では、比を押し上げていたのは**片方の脚しか拾わなかった hit** だった。
「重なりを測っている」という読み方はここから来ている。ただし**それは規則ではなく
そのコーパスについての観測**として扱うこと — 上の要因は、重なりが変わらないままでも
比を動かし得る。

ノイズではない。コーパスを固定すれば、答が無いクエリの方が低く出る。だが
**2 つのナレッジベースで同じ意味を持つ閾値が存在しない**。既定値を特定の
コーパスに合わせて調整せず、そのままにしてあるのはこのため。

どちらの限界も、意図した挙動ではなく**未処理の課題**として記録してある。
フィールド自体は 1.0 で凍結するが、**式と既定値は明示的に凍結しない**
([docs/stability.ja.md](stability.ja.md))。

## `category` と `tags_any` の違い (検索軸が別)

これらは index 上で **別のフィールド**:

- **`category`** は `documents.category` (単一 string 列)。文書 path の先頭
  segment から導出される。frontmatter に `category:` フィールドは無い —
  frontmatter で指定できるのは `topic:` だけで、こちらは path の 2 番目の
  segment を上書きする
- **`tags_any` / `tags_all`** は `documents.tags` (JSON 配列)。frontmatter の
  `tags:` リストから populate される

`deep-dive/mcp/x.md` にある `tags: ["mcp", "rust"]` を持つドキュメントは、
フィルタ `category: "deep-dive"` に **マッチする**が、
`tags_any: ["deep-dive"]` では **マッチしない**。これらは別軸。

## フィルタの組み合わせ

すべて **AND** で合成される:

```jsonc
{
  "path_globs": ["docs/**"],
  "tags_all":   ["rust"],
  "date_from":  "2026-01-01"
  // = docs/ 配下、"rust" タグ、2026 年以降
}
```

## 関連

- `docs/citations.ja.md` — match_spans / byte offset
- `docs/mcp-tools.ja.md` — search ツールの詳細リファレンス
