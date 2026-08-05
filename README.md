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

There is also the full chart. It answers the old star-history embed URLs (`/svg?repos=owner/name`), so replacing a dead chart is a one-hostname edit, and `?theme=dark` carries over:

[![star history](https://afterglow.watch/svg?repos=afterglowhq/afterglow&type=Date)](https://afterglow.watch)

    [![star history](https://afterglow.watch/svg?repos=OWNER/REPO&type=Date)](https://afterglow.watch)

That chart is a 30-day window. If you want the whole series instead, `?style=history` draws it as a square:

[![star history](https://afterglow.watch/badge/afterglowhq/afterglow?style=history)](https://afterglow.watch)

    [![star history](https://afterglow.watch/badge/OWNER/REPO?style=history)](https://afterglow.watch)

It starts at the day we first read your repo, which is the earliest point anyone still has.
The numbers across the top are the card's, so the two never disagree.

## The code

One binary, two jobs:

- `afterglow snapshot`, the daily collector
- `afterglow serve`, badge and rankings server

Decisions are recorded in [docs/adr/](docs/adr/).
The snapshot data is not in this repo and never will be.
