---
title: Introduction to kb-mcp
topic: overview
category: getting-started
tags: [overview, mcp, semantic-search]
date: 2026-04-20
---

## What is kb-mcp

kb-mcp is an MCP server that exposes semantic hybrid search over a
Markdown / plain-text knowledge base. Indexed content is chunked at the
heading level and embedded with a configurable model.

## Key features

The pipeline combines sqlite-vec for ANN over BGE-small embeddings with
SQLite FTS5 for keyword recall, fused via Reciprocal Rank Fusion. An
optional cross-encoder reranker can refine the final order.
