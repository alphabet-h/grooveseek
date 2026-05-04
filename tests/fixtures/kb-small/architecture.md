---
title: kb-mcp Architecture Overview
topic: architecture
category: deep-dive
tags: [architecture, sqlite-vec, fts5, rrf, mmr]
date: 2026-04-25
---

## Storage layer

The storage layer uses SQLite with two extensions: sqlite-vec for cosine
similarity search over float32 embeddings, and the built-in FTS5 module
for traditional inverted-index keyword search. Both indexes are kept in
the same `.kb-mcp.db` file alongside the documents and chunks tables.

## Retrieval pipeline

Each search request fans out to the vector index and the FTS5 index in
parallel, producing two ranked candidate lists. Reciprocal Rank Fusion
merges them into a single list, after which an optional cross-encoder
reranker (BGE-reranker-v2-m3) can rescore the top-k.

## Diversity and expansion

After fusion / reranking, the Maximal Marginal Relevance (MMR) stage can
penalise near-duplicate results to broaden coverage. The Parent
retriever then optionally expands each surviving hit with adjacent
chunks or the whole document, giving the caller more context per result.

## Transport layer

The server speaks MCP (Model Context Protocol) over either stdio or a
Streamable HTTP transport. Both share the same set of tools (`search`,
`get_document`, `graph`, `get_best_practice`) backed by the same
indexing core.
