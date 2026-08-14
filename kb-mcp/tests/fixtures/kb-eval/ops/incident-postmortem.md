---
title: Writing a Blameless Postmortem
topic: incidents
category: operations
tags: [postmortem, retrospective, learning]
date: 2026-05-05
---

## When one is required

Every incident that reached customers, lasted longer than thirty
minutes, or required a manual data repair gets a written review. The
document is due within five working days while the details are still
fresh. Near misses are worth writing up too, and they are usually the
cheapest lessons available.

## Tone

The review describes what the system allowed to happen, not who typed
the command. Naming an individual as the cause makes the next person
hide the same mistake, which is exactly the outcome the practice is
meant to prevent. Describe decisions in terms of the information that
was available at the time.

## Structure

Open with a short timeline built from timestamps rather than memory,
then the customer-visible impact, then contributing factors. Close with
follow-up items that each have an owner and a due date. Items without an
owner are wishes, not actions, and they never get done.
