---
title: Machine Credentials
topic: authentication
category: reference
tags: [api-keys, rotation, secrets]
date: 2026-05-17
---

## Issuing

Server-to-server callers authenticate with a long-lived key rather than
a browser flow. Keys are issued per service and per environment, never
shared between the two, and the plaintext is shown exactly once at
creation. Only a salted hash is stored, so a lost key is replaced rather
than recovered.

## Replacing one without an outage

Each caller may hold two valid keys at a time. Issue the replacement,
deploy it, confirm the old one has stopped being used from the access
log, and only then revoke the old one. Revoking first and deploying
after is the usual cause of a self-inflicted outage during this
procedure.

## Cadence and revocation

Replace keys every ninety days, and immediately whenever someone with
access leaves or a key appears in a repository. Revocation takes effect
within thirty seconds across all regions; there is no grace period, so
confirm the replacement is in place before you press it.
