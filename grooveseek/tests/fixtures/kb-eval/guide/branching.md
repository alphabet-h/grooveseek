---
title: Branch and Merge Policy
topic: workflow
category: guide
tags: [git, branches, merge]
date: 2026-05-11
---

## Naming

Short-lived work goes on a branch named `<type>/<ticket>-<slug>`, where
type is one of `feat`, `fix`, `chore`, or `docs`. The ticket identifier
keeps the branch traceable after the pull request is closed and the
title has been rewritten. Avoid personal prefixes; ownership belongs in
the pull request, not in the ref name.

## Keeping up to date

Rebase onto the trunk rather than merging it back in, so the history of
a change stays linear and reviewable. If a branch has been open long
enough that rebasing hurts, that is a signal the change is too large,
not a reason to merge the trunk in.

## Landing

Changes land as a single squashed commit whose message describes the
outcome rather than the journey. The branch is deleted on merge; the
tag and the pull request are enough to find it again.
