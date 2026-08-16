---
title: How Much of Each Kind of Test
topic: testing
category: guide
tags: [testing, pyramid, coverage]
date: 2026-05-12
---

## The shape we aim for

Most assertions belong in fast unit tests that run in milliseconds and
need no network. A thinner layer exercises a service together with its
real datastore. The slowest layer drives the product the way a person
would, and it should stay small: a handful per service, covering the
journeys that would embarrass us if they broke.

## Why the slow layer stays small

Browser-driven checks are the ones that fail for reasons unrelated to
the change under review. Every flaky one spends the team's attention and
teaches people to re-run instead of read. If a scenario can be covered a
layer down, cover it a layer down.

## What to assert

Assert on behaviour the caller can observe, not on internal call
sequences. A suite pinned to internals turns every refactor into a
rewrite of the suite, which is how teams end up deleting their tests.
