---
title: Conventions — formatting, linting, language, CI
area: development
status: stable
last_review: "2026-05-30"
---

# Conventions

## Toolchain

- **Rust stable** for development, **MSRV 1.88** as the contract
  (edition 2024). `rust-toolchain.toml` pins `channel = "stable"`;
  `Cargo.toml` declares `rust-version = "1.88"` and CI enforces it.
  MSRV bumps require a justifying entry in the design log.
- **No `unsafe`.** Every crate declares `#![forbid(unsafe_code)]`.
  Exceptions need a design-log entry and a `// SAFETY:` comment at the
  use site.

## Formatting

- `cargo fmt --all` is mandatory before commit. CI runs
  `cargo fmt --all -- --check` and fails on diff.
- Config: [`rustfmt.toml`](../../rustfmt.toml). Notable settings:
  `max_width = 100`, imports `granularity = "Crate"`, group
  `StdExternalCrate`.

## Linting

- `cargo clippy --workspace --all-targets -- -D warnings`. CI denies all
  warnings.
- The workspace has `clippy::pedantic` and `clippy::nursery` enabled
  with a few explicit `allow`s in
  [`.cargo/config.toml`](../../.cargo/config.toml)
  (`module_name_repetitions`, `missing_errors_doc`,
  `missing_panics_doc`). Adjustments to that allow-list need a
  design-log entry.

## License headers

The repo is licensed `AGPL-3.0-or-later`, with a commercial license
available for organizations that cannot comply with the AGPL (see
[`LICENSING.md`](../../LICENSING.md)). The full text lives in
[`LICENSE`](../../LICENSE). Every first-party source file carries an
SPDX header (`// SPDX-License-Identifier: AGPL-3.0-or-later`);
`Cargo.toml` declares the license metadata.

## Dependency policy

- Versions are **pinned** in the workspace
  [`Cargo.toml`](../../Cargo.toml) `[workspace.dependencies]` block.
  Crates use `name = { workspace = true }` and never re-pin.
- `cargo deny check` enforces the allowed-license list in
  [`deny.toml`](../../deny.toml) and bans wildcards.
- `cargo audit` runs in CI and fails on known advisories.

When adding a dependency:

1. Add it to `[workspace.dependencies]` in the root `Cargo.toml`.
2. Add `name = { workspace = true }` to the consuming crate.
3. Run `cargo deny check` locally.
4. If the new license is not in the allow-list, add it to
   [`deny.toml`](../../deny.toml) with a brief justification in the
   commit message.

## Logging / tracing

- `tracing` and `tracing-subscriber` are the only logging facade.
- All log output goes to **stderr** (keeps stdout clean for the
  readiness banner and piping).
- Default filter is `info`. Use `RUST_LOG=mwe_core=debug` to scope
  verbosity.

## Error handling

- Library crates (`mwe-core`, `mwe-dashboard`) return
  `mwe_core::Result<T>` (alias for `std::result::Result<T, Error>`).
- The binary uses `anyhow::Result<T>` at the boundary.
- Never `panic!` outside `unwrap_or_else` rescues with explanatory
  messages and outside tests. Prefer `?` propagation.

## Testing

- Unit tests live next to the code they test, under `#[cfg(test)] mod
  tests { … }`.
- Property-based tests via `proptest` for pure functions (parser, ACL,
  slug).
- Integration tests under `tests/` exercise the whole binary.
- **Fault injection** uses the cargo feature
  `mwe-mcp-test-faults`. When enabled, the `fault!(name)` macro
  introduces kill/sleep points selectable via the
  `MWE_TEST_FAULT_AT=<name>` env var. CI runs a separate job with this
  feature on.

## CI/CD

Configured in [`.github/workflows/ci.yml`](../../.github/workflows/ci.yml).
Six jobs:

| Job | Command |
|---|---|
| `fmt` | `cargo fmt --all -- --check` |
| `clippy` | `cargo clippy --workspace --all-targets -- -D warnings` |
| `test` (matrix: ubuntu / macos / windows) | `cargo test --workspace --all-targets` |
| `test-fault-injection` | `cargo test --workspace --features mwe-mcp-test-faults` |
| `audit` | `rustsec/audit-check` action |
| `deny` | `EmbarkStudios/cargo-deny-action` |

CI must be green on `main` before any release tag.

### A test that needs a database

Use `mwe_core::test_db::TestWorkdir` (test-only), which hands the
temporary directory's **guard back to the caller** so the test's own
scope owns it:

```rust
let (_workdir, pool) = TestWorkdir::with_db().await;              // db only
let (_workdir, pool, tree) = TestWorkdir::with_db_and_tree().await; // + a WikiTree
```

The `_workdir` binding is the mechanism, not decoration: while it lives
the directory exists, and when the test ends the directory is removed.

**Never leak the guard.** `Box::leak(Box::new(tempfile::tempdir()))` and
`std::mem::forget(dir)` both make the directory outlive the helper — and
outlive *everything else*, because nothing will ever remove it. That
idiom had spread to 46 sites and left **145 orphaned directories per
`cargo test --workspace`**, each holding a migrated database; on a host
where `/tmp` is `tmpfs` those are RAM, and they had reached ~20 000
directories / 12 GB before anyone noticed (the symptom was every shell
on the machine failing in ways that looked like anything but a full
disk). A test in `test_db.rs` scans the sources and fails if the idiom
returns. Helpers in `crates/mwe-dashboard` follow the same rule by
returning `tempfile::TempDir` alongside their state.

In-memory pools (`sqlite::memory:`) need none of this — they touch no
filesystem, and several test modules use them deliberately.

## Documentation rules

- **Code** and **comments** are in English.
- The **engineering wiki** ([`../`](../)) is in English and is the
  project SSOT.
- The **planning corpus** is in Italian — that is the original
  language of the design conversations and the corpus is preserved
  as-is. It lives outside the public repo; the engineering wiki is the
  canonical surface.
- The **lockstep rule**: a code change must update the `docs/` page
  covering that area in the same commit.
- The terms **engineering docs** (`docs/`, this documentation set) vs
  **memory wiki / consumer wiki** (the Markdown wiki the engine writes
  and serves at runtime) are not interchangeable.

## Commit messages

Conventional, terse, English. First line ≤ 70 chars. Reference the
relevant wiki page or recorded decision id when a commit implements a
recorded decision. Example:

```
parser: implement marker EBNF

Implements the inline marker grammar (recorded
marker-grammar decision). Benchmark: 11.4 MB/s on
x86_64-linux (target 10 MB/s).
```
