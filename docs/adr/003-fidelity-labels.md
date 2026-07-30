# ADR-003: Fidelity labels

Accepted, 2026-07-30.

## Context

We show numbers nobody can independently recompute anymore.
The entire value of the product rests on those numbers being trustworthy, which means never showing one we might later have to retract or qualify.
Different numbers here are trustworthy in genuinely different ways, and the difference must survive all the way to the pixel.

## Decision

Every derived number carries one of four fidelity labels:

- **measured**: computed from two of our own snapshots at least half a day apart.
  The primary signal.
  This is the only fidelity that earns accent colour in the UI, and the only one worth paying for.
- **proxy**: lifetime average (stars ÷ repo age), the placeholder before measured velocity exists.
  Always displayed with the `~` mark and never styled like a measurement.
  Meaningless for old repos, so old repos get "measuring" instead of a proxy number.
- **reconstructed-gross**: pre-2026 history rebuilt from archived star events.
  Unstars are invisible in that source, so these are gross counts that overstate net stars; a reconstructed series is never continued into, summed with, or drawn against a net snapshot series without the seam being explicit.
- **imported**: history donated from an external source, labeled with its origin.
  No import path exists yet; the label is reserved so one can.

Fidelity is derived from provenance (which table and lane a number came from), not stored as an opinion.
Code that surfaces numbers (API responses, badge rendering, rankings rows) carries the label in the type; there is no path from the store to a display surface that loses it.

Two display rules follow from the labels and are as binding as the labels themselves:

- Accent colour renders only on measured data.
  Proxy, measuring, reconstructed, and imported states render muted.
- Negative measured velocity is real data (mass unstars happen, bot purges happen) and is shown, not clamped to zero and not suppressed.

## Consequences

- A young repo's badge honestly says `~n avg` until we have two real snapshots; an old repo's badge says "measuring" and shows nothing fake.
- Deep-history charts, if we ever ship them, will visibly change character at the reconstruction seam rather than pretending one continuous series exists.
- Anyone auditing our numbers against a repo they control finds exactly what we claim, because we never claimed more than we measured.
