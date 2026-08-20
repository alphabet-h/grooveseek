# 9. One DNS-rebinding gate, owned here

Date: 2026-08-20

> **日本語**: [0009-one-dns-rebinding-gate.ja.md](0009-one-dns-rebinding-gate.ja.md)

## Status

Accepted

## Context

GrooveSeek answers two questions about every HTTP request: is this `Host`
allowed, and is this `Origin` allowed. The MCP specification requires the
second of a Streamable HTTP server, and both together are the standard defence
against DNS rebinding for a daemon on loopback.

Until now those two questions had **four** answers:

| | `/mcp` | `/healthz` | `/ui`, `/api/admin/status` |
| --- | --- | --- | --- |
| Host | rmcp `host_is_allowed` | `healthz_host_check` | `admin_host_check` |
| Origin | rmcp `origin_is_allowed` | — | `admin_origin_check` |

The arrangement was deliberate and defended twice: share the *list*, mirror the
*logic*, and rely on review to keep the mirrors true. `DEFAULT_LOOPBACK_HOSTS`
became the single definition in [#173](https://github.com/alphabet-h/grooveseek/pull/173)
precisely so that no surface could be given a different set of names, and
[#193](https://github.com/alphabet-h/grooveseek/pull/193) added parity tables
that ask two surfaces the same question through a running server.

**Measured, the mirrors were not true.** With the identical `effective_hosts`
list, twenty-six `Host` spellings sent to `/mcp` and to `/healthz`:

```text
Host                        /mcp (rmcp)   /healthz (groove)
user:pw@127.0.0.1:PORT      200           400
@127.0.0.1:PORT             200           400
127.0.0.1@localhost         200           400
127.0.0.1:65536             200           400
localhost:abc               200           400
```

Five disagreements, all in the same direction — groove refusing what rmcp
accepted, because `validate_host_header` adds defensive rejects for userinfo
and for a port outside `u16`. **No spelling was refused by rmcp and accepted by
groove.** The refusal bodies differed too: `/mcp` and `/healthz` answered
`Forbidden: Host header is not allowed`, the admin routes answered the same
sentence without the prefix. And groove's own two Host checks were not
identical to each other: `healthz_host_check` read the HTTP/2 `:authority`
fallback, `admin_host_check` did not.

A code review of #193 raised the general form of this twice, asking that the
`Origin` matchers be unified. Investigating it found the more useful fact: the
divergence that exists is in **`Host`**, so unifying `Origin` alone would have
repaired the half that agreed and left the half that did not.

## Decision

**One implementation answers both questions, for every route that asks them —
`/mcp` included.**

This is a decision about *who answers*, not about *what is asked*. Which list
a route is compared against, and whether it is asked at all, is unchanged:
`/healthz` is still unguarded unless `healthz_public = false` and still
validates `Host` only, and the admin routes still hold their own loopback-only
`Host` list. What ends is that the same question had different answers
depending on which path reached it.

rmcp's own checks are switched off explicitly — `with_allowed_hosts(vec![])`
and `with_allowed_origins(vec![])`, which upstream mean "accept every Host" and
"do not validate Origin". Passing them rather than omitting the calls is part
of the decision: `StreamableHttpServerConfig::default()` is loopback-only, so
omitting them would leave a second, differently-spelled check armed.

The gate is one middleware, given a different list per route group:

- `/mcp` — the effective `allowed_hosts` and `allowed_origins`.
- `/healthz` — the same `Host` list, and no `Origin` list, because `/healthz`
  never validated one and this decision is about who answers a question, not
  about asking new ones. It keeps its `healthz_public` opt-in too: on the
  default the route carries no gate at all.
- `/ui` and `/api/admin/status` — the admin `Host` list (loopback plus the bind
  address, not configurable), the same `Origin` list, and additionally the
  requirement that the peer address be loopback.

Order inside the gate is peer, then `Host`, then `Origin` — rmcp's order, kept
so that a request failing more than one check is refused for the same reason on
every surface.

The refusal wording is rmcp's, verbatim, so that `/mcp` clients see no change.
The admin routes gain the `Forbidden: ` / `Bad Request: ` prefix they lacked.

## Consequences

**`/mcp` becomes stricter for five malformed `Host` spellings**, which now
answer `400` instead of `200`. `docs/stability.md` freezes that `/mcp` exists
and that `/healthz` answers `200` when healthy; it does not freeze which
malformed spellings are tolerated, and every one of these is a `Host` no
browser or MCP client constructs. The direction is the safe one: the change can
only refuse more, never less, which is what made it adoptable at all.

**Refused requests no longer reach the session limit.** rmcp validated inside
its own `handle()`, which sits behind the session gate, so a refused
`initialize` reserved a seat and released it on the way out. Measured with
`max_sessions = 1`: a foreign `Host` used to be answered `429`, and is now
answered `403`.

**Refusal logging is now bounded on `/mcp` as well.** rmcp wrote one `warn!`
per refusal with no limit; the gate carries the same one-line-a-minute budget
the session gate has used since #190, per surface, each line naming how many
refusals it stands for.

**GrooveSeek now owns the DNS-rebinding defence for `/mcp`.** If rmcp hardens
its checks in a future release, that hardening is no longer inherited —
`docs/decisions` is the place where the question gets asked again, and the
parity tests are what would notice the surfaces drifting. Conversely, an rmcp
release that changed its parsing can no longer move `/mcp` without moving the
other routes with it, because there is nothing left to move independently.

**If the gate is mis-wired, `/mcp` has no validation at all.** That is the real
risk taken here, and it is why the tests are what they are:
`tests/dns_rebinding.rs` asserts the five spellings **by value**, not only by
parity between surfaces. Parity alone is satisfied by handing every route back
to rmcp; a refusal only `validate_host_header` produces is the fingerprint of
which implementation is answering. Measured: reverting `/mcp` to rmcp fails
four of the five tests in that file.

**Two implementations remain for two questions, not one each.** rmcp still
carries its own, unused, inside a dependency. The gain is that only one of them
is reachable, so the two can no longer disagree about a live request.

## Alternatives considered

**Leave it as it was, and document the five divergences.** Cheapest, and it
keeps the specification's own implementation in the path. Rejected because the
divergences are not a fact anyone chose: they are what two parsers did, found
by measuring rather than by reading, three years of releases after the first
one was written. Writing them down freezes an accident.

**Unify `Origin` only**, as the review asked. Rejected on the measurement: the
`Origin` matchers agreed on all twenty-one spellings tested, and the `Host`
checks did not. It would have left the observed disagreement in place, and left
`/mcp`'s `Origin` check running before its `Host` check while the admin routes
ran them the other way round.

**Put the gate in front but leave rmcp armed.** Strictly more defence, and no
check is ever switched off. Rejected because it does not reduce the number of
implementations that can answer, and because the outer gate being stricter
everywhere measured is not a guarantee that survives an upstream change — the
same weakness as the arrangement being replaced, with more moving parts.

**Contribute the matcher upstream** so both callers share one implementation
for real. The correct long-term answer and not incompatible with this one, but
it depends on another project's review and release cadence, and 1.0 does not
wait on it.
