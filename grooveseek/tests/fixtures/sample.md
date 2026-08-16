---
title: "MCP プロトコル概要"
date: 2026-04-10
topic: mcp
depth: "1"
tags:
  - mcp
  - protocol
  - overview
---

## MCP とは何か

Model Context Protocol (MCP) は、LLM アプリケーションと外部データソース・ツールを接続するためのオープンプロトコルである。
Anthropic が 2024 年 11 月に公開し、現在は OpenAI や Google も採用を表明している。

## 主な機能

MCP は以下の 3 つのプリミティブを提供する:

- **Resources**: ファイルやデータベースなどのコンテキスト情報を LLM に提供
- **Tools**: LLM が外部システムを操作するための関数呼び出し
- **Prompts**: 再利用可能なプロンプトテンプレート

### トランスポート層

```json
{
  "jsonrpc": "2.0",
  "method": "tools/call",
  "params": {
    "name": "search",
    "arguments": { "query": "MCP specification" }
  }
}
```

標準入出力 (stdio) と HTTP+SSE の 2 種類のトランスポートをサポートする。

## セキュリティモデル

MCP はクライアント側で権限制御を行う設計になっている。
サーバーは capability を宣言し、クライアントがユーザー承認を得てから実行する。

## 次の深堀り候補

- MCP の OAuth 2.1 仕様の詳細
- stdio vs HTTP+SSE のパフォーマンス比較
- MCP サーバー実装のベストプラクティス
