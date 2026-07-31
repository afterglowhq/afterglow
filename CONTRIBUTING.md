# Contributing

Build with `cargo build`, test with `cargo test`. CI also runs `cargo fmt --check` and clippy with warnings denied, so run those locally and save yourself the red X.

The snapshot dataset is not in this repo, so the interesting parts run against your own store: `afterglow snapshot` builds one (it wants a token in `GITHUB_TOKEN`), and `afterglow serve` renders the badges and the board from it. A store with a handful of repos and two days of readings is enough to see everything work.

Typo fixes, broken links, obvious small bugs: just open a PR.

For anything bigger, open an issue first. Partly to make sure the change fits, partly licensing: this code is FSL-1.1-ALv2, every version converts to Apache-2.0 after two years, and taking substantive outside code cleanly needs a contributor agreement I have not set up yet. An issue first means nobody writes a diff that can't land.
