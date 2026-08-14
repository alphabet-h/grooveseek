---
title: Request Quotas
topic: api
category: reference
tags: [quota, throttling, retry]
date: 2026-05-18
---

## How the allowance works

Each credential gets a bucket that refills continuously up to a burst
ceiling. The default is 100 requests per second sustained with a burst
of 300. Reads and writes draw from separate buckets so a batch import
cannot starve the interactive traffic sitting next to it.

## What a caller sees when the bucket is empty

The service answers with status 429 and a `Retry-After` header carrying
the number of seconds until the bucket has room again. Every response,
successful or not, also carries the remaining allowance and the reset
time, so a well-behaved client can slow down before it is refused.

## Backing off

Wait for the interval the header asks for, then add a random jitter of
up to one second. Retrying immediately, or retrying every failed request
at the same moment, converts a brief squeeze into a sustained outage.
Give up after five attempts and surface the failure to the caller.
