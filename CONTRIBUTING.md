# Contributing to mwe-mcp

Thanks for considering a contribution! A few ground rules keep the project
healthy and the dual-licensing model ([LICENSING.md](LICENSING.md)) workable.

## Before you write code

Many architectural trade-offs in this codebase are already deliberately
resolved — the engineering wiki under [`wiki/`](wiki/) documents most of them.
For anything beyond a small fix, **open an issue and discuss direction with
the maintainer first**; it avoids wasted work on a design that won't merge.

## Building and testing

See [`wiki/development/build-run.md`](wiki/development/build-run.md). CI runs
`cargo fmt --check`, `clippy -D warnings`, the full test suite (unit +
integration + property + fault-injection), and `cargo deny check` on every
push — keep it green locally before opening a PR.

## Licensing of contributions

By submitting a contribution you agree that:

1. **DCO** — you certify the [Developer Certificate of
   Origin](https://developercertificate.org/): the work is yours to submit
   under the project license. Sign off every commit (`git commit -s`), which
   adds the `Signed-off-by:` trailer.
2. **License** — your contribution is licensed under **AGPL-3.0-or-later**,
   like the rest of the project.
3. **Relicensing grant** — you grant the project maintainer (Francesco,
   Fr4nZ82) a perpetual, worldwide, non-exclusive, royalty-free right to also
   license your contribution as part of mwe-mcp under other license terms,
   including commercial ones. This is what keeps the dual-licensing model
   possible. Your contribution always remains available under the AGPL; you
   keep your copyright.

If you can't or don't want to accept these terms, don't submit the
contribution — open an issue describing the change instead.

## Style

`rustfmt` and `clippy` settings are checked into the repo and enforced by CI;
match the conventions described in
[`wiki/development/conventions.md`](wiki/development/conventions.md).
