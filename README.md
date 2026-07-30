# afterglow

GitHub stopped exposing star history on June 30, 2026. The lists died; only the current counts survive.
Afterglow is a snapshot fleet that has been recording those counts daily since July 30, 2026.
A missed day can never be recovered, so the dataset itself is the point: measured star velocity, honest fidelity labels, and a badge that drops into the hole the dead chart embeds left behind.

One binary, two jobs:

- `afterglow snapshot`, the daily collector
- `afterglow serve`, badge and rankings server

Both are stubs right now; the data has a head start on the code, which is the correct order for this product.

Decisions are recorded in [docs/adr/](docs/adr/).
The snapshot data is not in this repo and never will be.
