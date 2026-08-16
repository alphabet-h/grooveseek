---
title: Structured Log Records
topic: observability
category: reference
tags: [logging, fields, correlation]
date: 2026-05-19
---

## Required fields

Every record is a single JSON object on one line and always carries a
timestamp in UTC with millisecond precision, a severity, the emitting
service name, its version, and the correlation identifier of the request
being served. A record missing the correlation identifier cannot be
joined to anything, which makes it nearly useless during an incident.

## What must never appear

Do not write credentials, tokens, full card numbers, or message bodies.
Identifiers are fine; contents are not. The pipeline redacts a handful
of known key names as a safety net, but the safety net is not the
policy, and it does not know about fields invented last week.

## Severity

Reserve error for conditions a person must act on. Anything the service
recovers from on its own is a warning at most. Once error is used for
routine noise, the alerting built on top of it gets muted, and then the
real ones are missed too.
