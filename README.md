# afterglow

[![stars](https://afterglow.watch/badge/afterglowhq/afterglow)](https://afterglow.watch)

GitHub stopped exposing star history on June 30, 2026. The lists died; only the current counts survive.
Afterglow is a snapshot fleet that has been recording those counts daily since July 30, 2026.
A missed day can never be recovered, so the dataset itself is the point: measured star velocity, honest fidelity labels, and a badge that drops into the hole the dead chart embeds left behind.

Live at [afterglow.watch](https://afterglow.watch).

## Add your badges

First sight of a badge URL enrolls the repo; the badge fills in from there. Swap in your own `OWNER/REPO`.

The pill:

[![stars](https://afterglow.watch/badge/afterglowhq/afterglow)](https://afterglow.watch)

    [![stars](https://afterglow.watch/badge/OWNER/REPO)](https://afterglow.watch)

If your README already has a badge style, match it by appending `?style=flat-square`, `?style=for-the-badge`, or `?style=social`:

[![stars](https://afterglow.watch/badge/afterglowhq/afterglow?style=flat-square)](https://afterglow.watch) [![stars](https://afterglow.watch/badge/afterglowhq/afterglow?style=for-the-badge)](https://afterglow.watch) [![stars](https://afterglow.watch/badge/afterglowhq/afterglow?style=social)](https://afterglow.watch)

There is also the full chart. It answers the old star-history embed URLs (`/svg?repos=owner/name`), so replacing a dead chart is a one-hostname edit:

<a href="https://afterglow.watch"><picture>
  <source media="(prefers-color-scheme: dark)" srcset="https://afterglow.watch/svg?repos=afterglowhq/afterglow&type=Date&theme=dark">
  <img alt="star history" src="https://afterglow.watch/svg?repos=afterglowhq/afterglow&type=Date" width="420" height="150">
</picture></a>

    <a href="https://afterglow.watch"><picture>
      <source media="(prefers-color-scheme: dark)" srcset="https://afterglow.watch/svg?repos=OWNER/REPO&type=Date&theme=dark">
      <img alt="star history" src="https://afterglow.watch/svg?repos=OWNER/REPO&type=Date" width="420" height="150">
    </picture></a>

The chart is themed, so it takes `<picture>` rather than a markdown image. An image tag carries one URL and cannot see which theme you are reading in, so the two-source block is what serves a dark reader the dark cut. A bare URL stays light and `&theme=dark` pins it dark, for a README that is one colour on purpose.

That chart is a 30-day window. For the whole series, `?style=history` draws it as a square:

<a href="https://afterglow.watch"><picture>
  <source media="(prefers-color-scheme: dark)" srcset="https://afterglow.watch/badge/afterglowhq/afterglow?style=history&theme=dark">
  <img alt="star history" src="https://afterglow.watch/badge/afterglowhq/afterglow?style=history" width="420" height="420">
</picture></a>

    <a href="https://afterglow.watch"><picture>
      <source media="(prefers-color-scheme: dark)" srcset="https://afterglow.watch/badge/OWNER/REPO?style=history&theme=dark">
      <img alt="star history" src="https://afterglow.watch/badge/OWNER/REPO?style=history" width="420" height="420">
    </picture></a>

It starts at the day we first read your repo, which is the earliest point anyone still has.
The numbers across the top are the card's, so the two never disagree.

The pill and its cuts above are theme-invariant on purpose, so they stay plain markdown.

## The code

One binary, two jobs:

- `afterglow snapshot`, the daily collector
- `afterglow serve`, badge and rankings server

Decisions are recorded in [docs/adr/](docs/adr/).
The snapshot data is not in this repo and never will be.
