---
title: SQLite Vector Search
topic: database
date: 2026-01-02
tags: [sqlite, vector, search]
---

# SQLite Vector Search

The sqlite-vec extension provides nearest-neighbor search over float32
embeddings stored in a virtual table. It uses a flat scan, suitable for
small-to-medium corpora.

## MATCH Operator

Queries use `embedding MATCH ?1 AND k = ?2` to retrieve the top-k nearest
chunks ordered by cosine distance.
