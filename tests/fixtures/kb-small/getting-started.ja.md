---
title: kb-mcp 入門ガイド
topic: 使い方
category: 導入
tags: [入門, セットアップ, 検索]
date: 2026-04-21
---

## インストール

kb-mcp は Rust で書かれた単一バイナリです。`cargo install --path .`
または GitHub Release から OS 別のアーカイブを取得してください。実行に
追加の DLL や共有ライブラリは不要です。

## 最初の検索

ナレッジベースのディレクトリを `--kb-path` で指定して `kb-mcp index`
を実行すると `.kb-mcp.db` が作られます。続けて `kb-mcp search "<クエリ>"`
で日本語クエリも含めて検索できます。
