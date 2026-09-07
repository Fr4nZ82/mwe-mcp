# Contributing to mwe-mcp

Thanks for considering a contribution! A few ground rules keep the project
healthy and the dual-licensing model ([LICENSING.md](LICENSING.md)) workable.

## Before you write code

Many architectural trade-offs in this codebase are already deliberately
resolved — the doc comment beside each mechanism records the reasoning, and
[`CHANGELOG.md`](CHANGELOG.md) records the decisions that shipped.
For anything beyond a small fix, **open an issue and discuss direction with
the maintainer first**; it avoids wasted work on a design that won't merge.

## Building and testing

The gate is the one CI runs (`.github/workflows/ci.yml`):

```
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --all-targets
cargo build --workspace --examples --features local-embedder
```

CI runs those on Linux, macOS and Windows, adds a `cargo check` on the
declared minimum Rust version and `cargo deny check`, on every push — keep it
green locally before opening a PR. The lint bar (clippy pedantic and nursery)
is declared in the workspace manifest, so a plain `cargo clippy` already
applies it.

Two version numbers, and only one of them is a promise: development happens
on the `stable` channel pinned in `rust-toolchain.toml`, while
`rust-version` in `Cargo.toml` is the **floor** the MSRV job compiles
against. Raising the floor is a decision, not a side effect. The workspace is
edition 2024 and every crate carries `#![forbid(unsafe_code)]`.

A new dependency has to pass `cargo deny`: its licence must be on the allow
list in `deny.toml` (permissive only — the copyleft entry is for our own
crates), it is pinned in `[workspace.dependencies]`, and TLS is `rustls`
everywhere, never OpenSSL.

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
match the conventions of the code you are editing.
