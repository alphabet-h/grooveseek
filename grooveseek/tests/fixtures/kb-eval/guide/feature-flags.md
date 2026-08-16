---
title: The Life of a Toggle
topic: delivery
category: guide
tags: [toggles, rollout, cleanup]
date: 2026-05-15
---

## Creating one

A toggle is registered with an owner, a short description of what it
guards, and an expiry date. The expiry is not a suggestion: the registry
reports anything past it every Monday, and the report goes to the owner
rather than to a shared inbox nobody reads.

## While it is live

Keep the number of live toggles per service in single digits. Each one
doubles the number of code paths that exist in production, and two
interacting toggles produce four states that nobody has tested together.
Resist the temptation to nest them.

## Retiring it

Once a rollout has reached everyone and stayed there for two weeks, the
toggle has done its job. Delete the branch that is no longer taken,
delete the toggle definition, and delete the tests that pinned the old
behaviour. A toggle left permanently on is dead code wearing a costume.
