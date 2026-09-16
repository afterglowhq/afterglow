# afterglow

[![stars](https://afterglow.watch/badge/afterglowhq/afterglow)](https://afterglow.watch)

GitHub restricted stargazer lists on June 30, 2026, and star history went with them.
Afterglow is a snapshot fleet that has been recording the public counts daily since July 30, 2026, so the record would not have a hole in it.
On September 4, 2026, GitHub shipped a [star history endpoint](https://github.blog/changelog/2026-09-04-new-api-endpoint-provides-privacy-safe-star-history-data/) that returns daily counts back to the day a repo was created, and the hole is closed.

The site stays up and the badges keep working: measured star velocity, honest fidelity labels, and a leaderboard.
The fleet keeps reading and no reading is ever deleted, but for history itself GitHub's own data now reaches further back than ours.
If you swapped a star-history embed for one of ours, the swap back is the same one-hostname edit in reverse.

Live at [afterglow.watch](https://afterglow.watch).

## Add your badges

First sight of a badge URL enrolls the repo; the badge fills in from there. Swap in your own `OWNER/REPO`.

The pill:

[![stars](https://afterglow.watch/badge/afterglowhq/afterglow)](https://afterglow.watch)

    [![stars](https://afterglow.watch/badge/OWNER/REPO)](https://afterglow.watch)

If your README already has a badge style, match it by appending `?style=flat-square`, `?style=for-the-badge`, or `?style=social`:

[![stars](https://afterglow.watch/badge/afterglowhq/afterglow?style=flat-square)](https://afterglow.watch) [![stars](https://afterglow.watch/badge/afterglowhq/afterglow?style=for-the-badge)](https://afterglow.watch) [![stars](https://afterglow.watch/badge/afterglowhq/afterglow?style=social)](https://afterglow.watch)

The highest day we ever measured, with its date. It only goes up:

[![peak](https://afterglow.watch/badge/afterglowhq/afterglow?metric=peak)](https://afterglow.watch)

    [![peak](https://afterglow.watch/badge/OWNER/REPO?metric=peak)](https://afterglow.watch)

There is also the full chart. It answers star-history's embed URLs (`/svg?repos=owner/name`), so a README can swap hostnames either way and change nothing else:

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="https://afterglow.watch/svg?repos=afterglowhq/afterglow&type=Date&theme=dark">
  <img alt="star history" src="https://afterglow.watch/svg?repos=afterglowhq/afterglow&type=Date" width="420" height="150">
</picture>

    <picture>
      <source media="(prefers-color-scheme: dark)" srcset="https://afterglow.watch/svg?repos=OWNER/REPO&type=Date&theme=dark">
      <img alt="star history" src="https://afterglow.watch/svg?repos=OWNER/REPO&type=Date" width="420" height="150">
    </picture>

The chart is themed, so it takes `<picture>` rather than a markdown image. An image tag carries one URL and cannot see which theme you are reading in, so the two-source block is what serves a dark reader the dark cut. A bare URL stays light and `&theme=dark` pins it dark, for a README that is one colour on purpose.

That chart is a 30-day window. For the whole series, `?style=history` draws it as a square:

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="https://afterglow.watch/badge/afterglowhq/afterglow?style=history&theme=dark">
  <img alt="star history" src="https://afterglow.watch/badge/afterglowhq/afterglow?style=history" width="420" height="420">
</picture>

    <picture>
      <source media="(prefers-color-scheme: dark)" srcset="https://afterglow.watch/badge/OWNER/REPO?style=history&theme=dark">
      <img alt="star history" src="https://afterglow.watch/badge/OWNER/REPO?style=history" width="420" height="420">
    </picture>

It starts at the day we first read your repo, not the day the repo did.
The numbers across the top are the card's, so the two never disagree.

The pill and its cuts above are theme-invariant on purpose, so they stay plain markdown.

## The code

One binary, two jobs:

- `afterglow snapshot`, the daily collector
- `afterglow serve`, badge and rankings server

Decisions are recorded in [docs/adr/](docs/adr/).
The snapshot data is not in this repo and never will be.
