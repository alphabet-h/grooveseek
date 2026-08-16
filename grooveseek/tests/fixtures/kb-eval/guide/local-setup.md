---
title: Running Everything on Your Own Machine
topic: environment
category: guide
tags: [setup, containers, development]
date: 2026-05-13
---

## Prerequisites

You need a container runtime, a recent toolchain, and roughly 8 GB of
free memory. Everything else is pulled by the bootstrap script. There is
no shared development environment to reserve; each person runs their own
copy so a broken experiment never blocks anybody else.

## Bringing the stack up

`./scripts/bootstrap` fetches images, seeds the datastore with a small
anonymised sample, and starts the services behind a single entry point
on port 8080. The first run takes about ten minutes, mostly downloads.
Subsequent runs reuse the cached volumes and finish in under a minute.

## When something will not start

Nine times out of ten the port is already taken by an older copy that
did not shut down. Stop everything with `./scripts/down`, then start
again. If the datastore refuses to open, delete its volume and let the
seed run once more; nothing there is precious.
