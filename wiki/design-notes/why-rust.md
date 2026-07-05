---
title: Why Rust — stack rationale
area: design-notes
status: stable
last_review: "2026-05-30"
---

# Why Rust

The decision to build mwe-mcp in Rust is settled, and this page is its
canonical statement: the stack rationale and the concrete crate
choices below are the source of truth for contributors. The broader
product framing lives in
[the memory model](../concepts/memory-model.md).

## TL;DR

Rust gives us a single statically-linked binary (~15–20 MB) that runs
on every desktop and small home-server architecture, with strong
memory safety and a mature MCP SDK from the protocol owner itself
(`rmcp` is hosted under `modelcontextprotocol/rust-sdk`). For a tool
that must live for years inside other people's setups and be trusted
with their memory, those properties matter more than language
familiarity.

## What we considered

| Stack | Pros | Cons |
|---|---|---|
| **Rust + `rmcp`** ✅ | Single binary, no runtime, multi-arch trivial, `rmcp` is the official Anthropic-org SDK, mature async (tokio), strict by default, `#![forbid(unsafe_code)]` makes our perimeter clear, sqlx with compile-time SQL validation, `rust-embed` for PWA asset packaging. | Slower build, smaller ecosystem of web/templating libs vs. Node, steeper local setup. |
| Node + `@modelcontextprotocol/sdk` | Fastest scaffolding, biggest ecosystem, agent authors already write TypeScript. | Distribution requires Node runtime or bundling (pkg/ncc — fragile), single-threaded for CPU-bound parsing, weaker compile-time SQL checks. |
| Go + `mcp-go` | Single binary, decent ecosystem. | Third-party MCP SDK, generics still young for our use, less mature web templating story. |
| Python + `mcp` | Easy for AI/LLM glue. | Packaging story for end users is rough (venv, no static binary), GIL hurts the REM job concurrency. |

The deciding factor was the combination of **single-binary distribution
+ Anthropic-owned SDK + compile-time correctness**. Memory-wiki engines
will live in users' workdirs for years; we want the executable to be
boring to ship and obviously safe to run.

## Concrete stack choices

| Concern | Crate | Why |
|---|---|---|
| MCP | `rmcp` =1.7 | Official Anthropic SDK. Pinned exact-version because the protocol surface is still moving. |
| HTTP / web | `axum` 0.7 + `tower` | Smallest sensible Rust web stack; integrates cleanly with `rmcp` Streamable HTTP. |
| Templating | `maud` 0.26 | Server-side HTML via compile-checked macros; no runtime template engine to ship. |
| Static assets | `rust-embed` 8 | Bundles PWA assets (manifest, service worker, htmx, tailwind.css) into the binary at compile time — no separate `static/` to deploy. |
| Database | `sqlx` 0.8 (sqlite) | Async, runtime-tokio, compile-time SQL validation via `sqlx::query!` macro. |
| Async runtime | `tokio` 1 | Standard. |
| File watcher | `notify` 6 | Cross-platform (inotify / FSEvents / ReadDirectoryChangesW). |
| Auth | `jsonwebtoken` 9 | HS256 + struct payloads via `serde`. |
| IDs | `uuid` v7 | Time-ordered, no central allocator — avoids the mutex on `fact_index`. |
| HTTP client | `reqwest` 0.12 + `rustls-tls` | No OpenSSL dep, smaller closure for static binaries. |
| Lockfile | `fs2` 0.4 | Cross-platform `flock`. |
| CLI | `clap` 4 | Standard. |
| Logging | `tracing` 0.1 | Writer on **stderr** — we ship HTTP-only, but rmcp examples and many tutorials assume stdio; keeping stderr as the log sink avoids one foot-gun and matches the rmcp/MCP convention. |
| Property tests | `proptest` 1 | For pure functions (parser, ACL, slug). |
| Benchmarks | `criterion` 0.5 | Targets in [the roadmap](../roadmap.md) (e.g. parser 10 MB/s). |

## Constraints this places on contributions

- **No `unsafe` blocks.** `#![forbid(unsafe_code)]` is enforced on
  every crate.
- **No OpenSSL transitive deps.** Stick with `rustls-tls`. CI's
  `cargo deny check` will catch accidental introductions.
- **No build-script downloads.** Everything ships via crates.io.
- **MSRV 1.88**, development uses **stable**. `Cargo.toml`
  `rust-version` declares the floor (1.88); `rust-toolchain.toml`
  pins the `stable` channel so contributors and CI track latest
  stable. The floor is 1.88 because of ecosystem MSRV creep —
  `darling 0.23`, `time 0.3.47`, `icu 2.2`, `idna_adapter 1.2.2` all
  require it.

## Open trade-offs we accept

- **Build time.** A clean `cargo build --release` on a developer
  machine is a couple of minutes. Tolerable for a binary that
  installs once and runs for years.
- **Tailwind toolchain.** Generating the compiled CSS for the
  dashboard PWA needs either the standalone `tailwindcss` binary or
  Node. That is an extra step for contributors who touch UI, and is
  documented in `wiki/development/build-run.md`.
- **Templating ergonomics.** `maud` is great for type-checked HTML but
  has a steeper learning curve than handlebars/jinja.
