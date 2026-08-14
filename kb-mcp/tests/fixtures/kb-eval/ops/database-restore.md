---
title: Recovering a Database to an Earlier State
topic: database
category: operations
tags: [database, recovery, replay]
date: 2026-05-07
---

## Choosing the target moment

Recovery replays shipped log segments on top of the most recent snapshot
that predates the moment you want to land on. Pick that moment from the
audit trail rather than from memory: the timestamp of the last known
good write is the target, and anything after it will be discarded.

## Running the replay

Provision a fresh instance, load the snapshot, then apply log segments
with a stop condition set to the chosen timestamp. Replaying an hour of
traffic takes roughly ten minutes. Never replay onto the live instance;
promote the recovered one only after the row counts have been compared.

## After the switch

Applications keep old connections open, so restart the services that
talk to the promoted instance instead of waiting for them to notice.
Record the discarded window in the incident notes, because support will
be asked about the writes that vanished.
