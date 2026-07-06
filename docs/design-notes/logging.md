---
title: Logging — two-level filter on stderr + rotating file sink
area: design-notes
status: implemented
last_review: "2026-06-07"
---

# Logging

`mwe-mcp` writes structured logs to two sinks in parallel:

- **stderr** (always on) — keeps `stdout` clean for the readiness
  banner and for piping.
- **rotating file** under `<workdir>/logs/mwe-mcp.log` by default — on
  by default, opt-out via `logging.file_rotation: disabled`. Running
  `mwe-mcp` detached (systemd, container, agent process) needs a
  `tail`-able file the operator can attach to after the fact.

Levels are deliberately limited to two — `info` and `debug`.
Implementation in
[`mwe-core::config`](../../crates/mwe-core/src/config.rs) +
[`mwe-mcp-server::tracing_setup`](../../crates/mwe-mcp-server/src/tracing_setup.rs)
+ [`mwe-mcp-server::main`](../../crates/mwe-mcp-server/src/main.rs).

## Precedence

```
RUST_LOG env var    →   wins always (operator override)
        ↓
config.logging.level →   info | debug, read from <workdir>/mwe-mcp.config.yaml
        ↓
default              →   info
```

The config-file form was added so an operator can turn on debug
without having to remember tracing-subscriber's directive syntax:

```yaml
# mwe-mcp.config.yaml
logging:
  level: debug
```

That alone enables full `debug` output across every mwe-mcp crate
(`mwe_core`, `mwe_dashboard`, `mwe_mcp`); third-party crates (`sqlx`,
`notify`, `hyper`, …) stay capped at `warn` so the operator's terminal
does not get buried by HTTP framing chatter when they are inseguring a
capture bug. The exact filter strings are
[`LogLevel::as_env_filter`](../../crates/mwe-core/src/config.rs) — kept
in one place so the precedence + crate scope cannot drift.

For ad-hoc fiddling (one shell session, no file edit):

```bash
RUST_LOG=warn,mwe_core=trace,sqlx=info mwe-mcp serve
```

`RUST_LOG` always wins; the config setting is the durable default an
operator commits next to the workdir.

## File sink with rotation

The file sink writes the same filtered events as `stderr` to a
rotating file under the workdir. `tracing-appender::rolling` produces
the underlying stream through a non-blocking writer, so a stalled disk
cannot back-pressure handler threads — it keeps the async runtime from
stalling on a slow disk under load.

| YAML key | Default | Effect |
|---|---|---|
| `logging.file_rotation` | `daily` | One of `daily` (rotate at UTC midnight), `hourly`, `never` (single growing file), `disabled` (no file sink, stderr only) |
| `logging.file_path` | `logs/mwe-mcp.log` | Relative paths are joined onto `<workdir>`; absolute paths are used verbatim so an operator can point at an external mount |

Example: opt-out for an embedded read-only mount where the operator
ships logs via stderr forwarding instead.

```yaml
logging:
  level: info
  file_rotation: disabled
```

Example: hourly rotation under a custom subdir.

```yaml
logging:
  level: debug
  file_rotation: hourly
  file_path: var/log/mwe-mcp.log
```

The default-on stance was chosen on purpose: the project is
developer-friendly and an operator should never have to remember to
flip a switch the first time they need to inspect a past run. If the
file sink fails to open at startup (read-only workdir, missing
parent), `mwe-mcp` reports the failure on stderr and continues with
`stderr` only — the documented safe degradation.

Size-based rotation is not supported: `tracing-appender`
ships time-based rotation only, and the daily roll keeps the log
directory bounded for the current traffic profile (a few MB/day at
`info`, tens of MB/day at `debug`). Size-based rotation is planned for
a chattier production profile (planned — see the
roadmap).

## Boundary vs internal — what goes where

| Event | Level | Where |
|---|---|---|
| `mwe-mcp` startup, workdir, chosen log level | info | [`main`](../../crates/mwe-mcp-server/src/main.rs) |
| WAL recovery scan summary (stale ops counts) | info | `cmd_serve` |
| Identity bootstrap / admin reset / token issue | info | `cmd_*` handlers |
| `capture: CAPTURED`, `SKIPPED`, `SUPERSEDED`, `FORGOTTEN`, `LINKED` | info | [`capture`](../../crates/mwe-core/src/capture.rs) |
| `config: file absent, falling back to defaults` | info | [`Config::load`](../../crates/mwe-core/src/config.rs) |
| capture validated request (wiki, page, owner, body_len) | debug | `wiki_capture` |
| capture embedded body (model, dim) | debug | `wiki_capture` |
| capture dedup scoring (candidates, best_score, best_id) | debug | `wiki_capture` |
| capture forget no-op | debug | `wiki_forget` |
| wiki `atomic_write` done (target, bytes) | debug | [`wiki::atomic_write`](../../crates/mwe-core/src/wiki.rs) |

The "did something happen?" timeline lives at `info`; the
"why-exactly-did-it-happen?" detail lives at `debug`. The line is set
so that a healthy day-to-day operator session reads as roughly one
line per request boundary, while a debugging session prints the steps
that compose it.

## What is intentionally not in scope

- **`trace` level.** `debug` must be enough — if it ever isn't, we
  add specific `RUST_LOG=mwe_core::module=trace` exemptions in source.
- **`warn` / `error` toggles.** Both pass through whatever level is
  active; an operator should not be in a position to silence error
  output via a config typo.
- **Per-module filters in `mwe-mcp.config.yaml`.** Two levels keep the
  decision atomic. Per-module overrides exist via `RUST_LOG` for
  surgical debugging, but they are not a config-file concern.
- **Log format selection (JSON vs human-readable).** `tracing_subscriber::fmt`
  default (compact, human-readable, ANSI when terminal-attached) is
  the only output today; the file sink strips ANSI so `grep` / `less`
  produce clean text. Structured-JSON output for production
  log-shipping is not implemented today.
- **Size-based file rotation.** See [`File sink with rotation`](#file-sink-with-rotation)
  above — daily / hourly / never are enough for the current traffic
  profile.

## Test coverage

- `config` tests: log-level default, env-filter directive strings,
  load returns default when file absent, parses logging section only,
  preserves unknown top-level keys in `extra`, rejects invalid level
  explicitly, rejects malformed YAML, accepts missing logging section,
  plus the file-rotation suite (default daily seeded under
  `<workdir>/logs/`, custom relative + absolute paths, `disabled`
  skips the sink, unknown rotation rejected explicitly).
- `mwe-mcp-server::tests::file_logging`: integration smoke tests for
  the file sink — `build_file_appender` writes a structured event to
  the configured path, materialises the parent directory on demand,
  and the default-loaded `Config` resolves to `<workdir>/logs/mwe-mcp.log`.
