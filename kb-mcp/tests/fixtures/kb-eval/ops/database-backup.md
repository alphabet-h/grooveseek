---
title: Database Backup Schedule
topic: database
category: operations
tags: [database, backup, retention]
date: 2026-05-06
---

## What runs when

A full snapshot of every primary instance is taken nightly at 02:00 UTC,
and write-ahead log segments are shipped to object storage continuously
so the gap between snapshots is covered. The snapshot job writes a
completion marker; monitoring alerts if the marker is missing by 04:00.

## How long copies are kept

Nightly snapshots are kept for thirty days. The first snapshot of each
month is promoted to a monthly copy and kept for a year. Log segments
older than the oldest surviving snapshot are pruned automatically, since
they can no longer be replayed against anything.

## Verifying that copies are usable

An untested copy is a guess. A scheduled job restores the previous
night's snapshot into a scratch instance every Monday and runs a row
count comparison against production. The result is posted to the
platform channel whether it passed or not.
