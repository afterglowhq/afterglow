# ADR-002: Coverage policy

Accepted, 2026-07-30.

## Context

We cannot snapshot all of GitHub daily, and we don't need to.
The interesting repos are the big ones (everyone asks about them), the young fast ones (where velocity actually matters), and whatever people explicitly ask us to track.
The API budget is one authenticated identity; the daily sweep has to fit comfortably inside it with headroom for everything else.

## Decision

Three tiers, all landing in the same store:

1. **The floor.**
   Every repo above a star threshold is tracked, regardless of age.
   The threshold is sized so the full daily sweep stays under about 30 minutes of API budget, around 50k repos to start.
   The floor only ever moves down.
   Lowering it adds repos; raising it would silently end series, which we never do.
2. **The young scan.**
   Below the floor, a daily search picks up young repos (created in the last ~180 days) above a much lower star bar, the population where day-scale velocity is worth watching.
3. **Enrollment.**
   Any public repo, any size, on request: a badge request for an untracked repo enrolls it, as does manual submission on the site.
   Each enrollment lane has a global daily budget; overflow goes into a queue and enrolls on later days, never silently dropped.

Cadence is daily for everything.
A hot set (the fastest movers plus repos enrolled in the last few days) gets an extra pass roughly every six hours, so fresh enrollees see their first measured numbers in hours instead of days.

Subscriber counts are recorded wherever we already make a per-repo GET (the hot set and enrolled repos).
They cost nothing extra there, and that series is just as impossible to backfill as stars.

We do not consume any endpoint that exposes who starred or watched, only aggregate counts.
That line does not move, whatever data it might leave on the table.

Thresholds, budgets, and hot-set size are constants in code, tuned freely; the tier structure and the ratchet direction are what this ADR fixes.

## Consequences

- A repo can enter by any tier and its series is indistinguishable afterwards; the lane is recorded on the repo, not the snapshots.
- The floor ratcheting down means coverage only widens.
  Combined with append-only storage, no series ever ends except by the repo itself disappearing.
- Queued enrollment means a viral flood of badge adds degrades to slower enrollment, not dropped repos or a blown API budget.
