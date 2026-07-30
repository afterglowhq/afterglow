# afterglow

GitHub stopped exposing star history on June 30, 2026. The lists died; only the current counts survive.
Afterglow is a snapshot fleet that has been recording those counts daily since July 30, 2026.
A missed day can never be recovered, so the dataset itself is the point: measured star velocity, honest fidelity labels, and a badge that drops into the hole the dead chart embeds left behind.

Live at [afterglow.watch](https://afterglow.watch).

    ![stars](https://afterglow.watch/badge/OWNER/REPO)

First sight of that URL enrolls the repo; the badge fills in from there.
The old star-history embed URLs also answer (`/svg?repos=owner/name`), so replacing a dead chart is a one-hostname edit.

One binary, two jobs:

- `afterglow snapshot`, the daily collector
- `afterglow serve`, badge and rankings server

Decisions are recorded in [docs/adr/](docs/adr/).
The snapshot data is not in this repo and never will be.
