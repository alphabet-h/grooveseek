# Citations (引用箇所構造化)

`search` MCP ツールは各 hit に `match_spans` を返し、query 各 term が chunk
の `content` のどこにマッチしたかを示す。Claude / クライアントが出典を正確
に引用するための補助情報で、ハルシネーション抑制に役立つ。

> **English version**: [citations.md](./citations.md)

## 出力形

```jsonc
{
  "results": [
    {
      "score": 0.0327,
      "path": "docs/foo.md",
      "content": "Use tokio::spawn for async tasks.",
      "match_spans": [
        {"start": 4,  "end": 9 },   // "tokio"
        {"start": 11, "end": 16}    // "spawn"
      ],
      // ... 他のフィールド
    }
  ]
}
```

## `match_spans` の意味論

| 値 | 意味 |
|---|---|
| `null` (key 省略) | 計算していない。3 ケース: query に non-ASCII を含む / query が空 (whitespace のみを含む) / chunk の `content` が 256 KiB 超 (`MATCH_SPAN_CONTENT_MAX_BYTES`、異常入力での O(N×M) 走査を防ぐガード) |
| `[]` (空配列) | 計算したが一致箇所なし |
| `[{...}, ...]` | 計算済み、1 件以上マッチあり |

## byte offset

`start` / `end` は chunk の `content` 文字列に対する **byte offset**。両方とも
**UTF-8 codepoint 境界に揃う**ことを `kb-mcp` 側で保証する。クライアントは
安全に切り取れる:

> **注記 (v0.7.0+):** parent retriever (`[search.parent_retriever]`) が発火した
> ヒットでは、返ってくる `content` は**展開後**のテキスト (隣接 sibling もしくは
> ドキュメント全体)、`match_spans` はその展開後 content への byte offset である
> (元 chunk ではない)。`content.get(start..end)` でそのまま切り出せる動作は
> 変わらない。同じヒットの新フィールド `expanded_from` がどの chunk range を
> merge したかを伝える。pipeline 全体の順序 (`match_spans` は parent 展開の
> **後**で再計算される) は [retrieval-pipeline.ja.md](./retrieval-pipeline.ja.md)
> 参照。

```typescript
const snippet = content.slice(span.start, span.end);
```

Rust の場合:

```rust
let snippet = content.get(span.start..span.end).unwrap_or("");
```

万一 codepoint 境界をまたぐ span が観測されたら bug として報告してほしい。

## 何がマッチ対象になるか

`match_spans` の計算手順:

1. query を whitespace で分割し term の配列にする
2. query / content を ASCII-fold case-insensitive で小文字化
3. 各 term を `content` 内で substring 検索 (case-insensitive)
4. マッチ位置を start byte 順にソート + 重複除去。**1 chunk あたり 100 件で打ち切る** (`MATCH_SPAN_MAX_COUNT`) ので、1 文字 term × 巨大 chunk でレスポンスが膨れない

## non-ASCII query の扱い

query に **non-ASCII 文字を 1 つでも含む**場合 (日本語、絵文字など)、
`match_spans` は JSON 出力から完全に省略される (key 自体が無い)。

これは MVP として意図的な制限。non-ASCII テキストの substring matching は
FTS5 trigram tokenizer の粒度に追いつけず、混乱を招く結果になりやすいため。
今後の機能拡張で FTS5 の `snippet()` を使った正確な span 抽出に置き換える
予定 (全言語対応)。

## 結果が空のとき

`results: []` のときは `match_spans` を返す対象がない (= chunk が無い)。
「該当なし」の判定には `low_confidence` フラグを参照すること。

## 関連

- `docs/filters.ja.md` — 検索結果の絞り込み
- `README.ja.md` — search ツールの詳細リファレンス
