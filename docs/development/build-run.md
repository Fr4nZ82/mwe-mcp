---
title: Build & run
area: development
status: implemented
last_review: "2026-06-30"
---

# Build & run

This page covers everything you need to compile and exercise the
`mwe-mcp` binary locally.

## Prerequisites

- **Rust stable** (edition 2024). `rust-toolchain.toml` pins the
  `stable` channel; `rustup` auto-installs whatever stable is current
  on first `cargo` invocation. **MSRV: 1.88** — declared in
  `Cargo.toml` via `rust-version` and enforced by CI.
- **C toolchain** (`build-essential` on Debian/Ubuntu) for the bundled
  `libsqlite3-sys` build.
- **Standalone tailwindcss CLI** (v4.x) for dashboard CSS work.
  Optional for `cargo build` (the committed
  `crates/mwe-dashboard/assets/tailwind.css` is what rust-embed pulls
  in), required when editing anything under `tailwind/` or adding
  Tailwind utility classes to Maud templates. See *Dashboard assets*
  below for install + build commands.
- A local Ollama (or compatible) endpoint if you want to exercise the
  LLM-driven functions; see `mwe-mcp.config.yaml > llm`,
  `llm-functions.md` for the per-slot
  roster, and `logging.md` for the wiring.

## First-time setup

```bash
git clone https://github.com/Fr4nZ82/mwe-mcp.git
cd mwe-mcp
cargo check --workspace
```

The first `cargo check` downloads and compiles all pinned dependencies
(rmcp, axum, maud, sqlx, tokio, …). Expect 1–3 minutes the first time,
seconds afterwards.

> **Just want to run it, not hack on it?** Download a prebuilt binary
> instead of building from source — the `install.sh` one-liner or a
> release asset.
> [`.github/workflows/release.yml`](../../.github/workflows/release.yml)
> builds them on every `v*` tag for three targets
> (`x86_64-unknown-linux-gnu`, `x86_64-pc-windows-msvc`,
> `aarch64-apple-darwin`), each **`--features local-embedder`** (bundled
> embedder, zero external services) with a per-asset `.sha256`; the
> Linux-only `mwe-mcp-tray` ships only in the Linux artifact. Operator-facing
> install + first-config steps are in
> [`INSTALL.md`](../../INSTALL.md).

## Common commands

```bash
cargo build   --workspace
cargo test    --workspace --all-targets
cargo clippy  --workspace --all-targets -- -D warnings
cargo fmt     --all
cargo doc     --workspace --no-deps --open
```

CI runs these on every push (see
[`development/conventions.md`](conventions.md) — *CI/CD* section).

## Dashboard assets (Tailwind v4)

The dashboard's CSS is built from
[`tailwind/`](../../tailwind/) (entry `tailwind/app.css` + design
tokens in `tailwind/tokens.css`) by the **standalone tailwindcss
CLI** — single binary, no Node toolchain, no `package.json`. The
output lives at
[`crates/mwe-dashboard/assets/tailwind.css`](../../crates/mwe-dashboard/assets/tailwind.css)
and IS committed to the repo, because `rust-embed` pulls it in at
compile time and Cargo cannot run the Tailwind CLI itself.

Install once (Linux x64 example; other platforms picked from the
GitHub releases page of `tailwindlabs/tailwindcss`):

```bash
curl -sSL -o ~/.local/bin/tailwindcss \
  https://github.com/tailwindlabs/tailwindcss/releases/download/v4.3.0/tailwindcss-linux-x64
chmod +x ~/.local/bin/tailwindcss
```

Rebuild the dashboard CSS after editing any file under `tailwind/`,
or after touching a Maud template in `crates/mwe-dashboard/src/` that
introduces a new Tailwind utility class (so Oxide picks it up):

```bash
tailwindcss -i tailwind/app.css \
            -o crates/mwe-dashboard/assets/tailwind.css \
            --minify
```

For a tighter dev loop, add `--watch` and it stays running. Self-
hosted fonts under
[`crates/mwe-dashboard/assets/fonts/`](../../crates/mwe-dashboard/assets/fonts/)
are referenced from `tailwind/app.css` via `@font-face`; the SIL OFL
license file (`OFL.txt`) ships alongside them. SVG marks
(`mwe-mark.svg`, `mwe-mark-mono.svg`, `mwe-logo.svg`) and the favicon
linkage live in the same `assets/` directory. See
the [dashboard-frontend design note](../design-notes/dashboard-frontend.md)
for the full client-side architecture.

## Running the binary

The binary is `mwe-mcp`, produced by the `mwe-mcp-server` crate. The
CLI is wired in the `Command` enum in
[`crates/mwe-mcp-server/src/main.rs`](../../crates/mwe-mcp-server/src/main.rs)
(the SSOT for the roster — don't trust a hand-maintained count here).
Every sub-command takes a `--workdir <PATH>` (top-level flag); the listed
flags are sub-command specific.

**Operator model — dashboard-first.** The happy path is `mwe-mcp serve`,
then everything else in the dashboard. `serve` self-bootstraps the workdir
(dirs + migrations + secret) on first boot, and the admin completes
identity, LLM/embedding/recall config, token issuing, live diagnostics,
and the "Run REM now" / Backup & recovery maintenance surfaces from the web UI.
The CLI keeps only what must run **when the dashboard or server cannot
help**, grouped by role:

| Role | Commands | Why CLI |
|---|---|---|
| **Daemon** | `serve` | you have to start the server somehow |
| **Bootstrap** *(optional)* | `init` | deterministic headless / IaC pre-boot; `serve` self-bootstraps otherwise |
| **Break-glass / ops** | `admin-reset`, `token-revoke`, `token-list`, `migrate`, `backup` | used when locked out of the dashboard, or for controlled ops; each has a dashboard equivalent |
| **Cron / offline** | `rem run-cycle` / `run-light` / `run-compile` | maintenance when the long-lived server is off |
| **Boot-failure triage** | `doctor` | takes the lockfile; for "serve won't start" (the dashboard **Health** page covers the running server) |
| **Dev / CI** | `recall eval` | developer measurement tooling |
| **Headless fallback** | `token-issue` | the dashboard token console is primary; the CLI mints headlessly for IaC / scripted provisioning |

```bash
cargo run -p mwe-mcp-server -- --help
cargo run -p mwe-mcp-server -- --workdir ./work init [--llm-profile <preset>] [--force-config]
cargo run -p mwe-mcp-server -- --workdir ./work serve [--bind 127.0.0.1] [--port 8742]
cargo run -p mwe-mcp-server -- --workdir ./work token-issue --sender <id> --device <label> [--rate-limit-id <id>] [--ttl internal|exposed] [--is-admin] [--consumer-id <id>] [--class standard|smart]
cargo run -p mwe-mcp-server -- --workdir ./work token-revoke <jti> --reason <msg> [--revoked-by <actor>] [--original-exp <unix>]
cargo run -p mwe-mcp-server -- --workdir ./work token-list
cargo run -p mwe-mcp-server -- --workdir ./work admin-reset --user <id> [--ttl-hours 24] [--invited-by <actor>]
cargo run -p mwe-mcp-server -- --workdir ./work doctor
cargo run -p mwe-mcp-server -- --workdir ./work migrate [--dry-run]
cargo run -p mwe-mcp-server -- --workdir ./work backup --out <dir>
cargo run -p mwe-mcp-server -- --workdir ./work rem run-cycle
cargo run -p mwe-mcp-server -- --workdir ./work rem run-light
cargo run -p mwe-mcp-server -- --workdir ./work rem run-compile
cargo run -p mwe-mcp-server -- --workdir ./work recall eval --gold <gold.yaml> [--flat-only]
```

What each sub-command does:

- **`init`** *(optional)* — bootstrap the workdir: create directories,
  apply migrations, generate a fresh `MWE_TOKEN_SECRET` if absent and
  write `mwe-mcp.env`, seed `mwe-mcp.config.yaml` from the
  `--llm-profile` preset (`all-local` default / `hybrid` / `all-api` /
  `custom`). Identity (users, groups, admin) is **not** seeded here —
  it's created later through the dashboard first-run wizard at
  `/dashboard/setup`. `--force-config` re-writes `mwe-mcp.config.yaml`
  even if it exists. **Not required for the interactive path**: `serve`
  self-bootstraps the same workdir state on first boot (directories,
  migrations, secret). `init` is for deterministic headless / IaC
  provisioning where a pre-boot step with an explicit `--llm-profile`
  is wanted.
- **`serve`** — start the MCP server (HTTP-only). **Self-bootstrapping**:
  on an empty workdir it creates the directories, applies migrations, and
  generates + persists a fresh `MWE_TOKEN_SECRET` (0600) if absent, then
  boots with an empty LLM config — the admin completes identity and LLM
  config from the dashboard wizard. Brings up Axum
  on `--bind` : `--port` with `/mcp` (Streamable HTTP, JWT-gated) and
  `/dashboard/*` (web UI) on the same listener. `--bind`/`--port` are
  optional: on a bare interactive `serve` (neither flag given, a real
  terminal) it **asks whether to expose the server** — `0.0.0.0` (LAN /
  port-forwardable) vs `127.0.0.1` (default, local only) and which port —
  so the choice also bakes into the systemd unit if the gate provisions
  one; non-interactively it defaults to `127.0.0.1:8742` (the prod default —
  a **dev** instance runs the `target/` debug build via `run-server.sh`, which
  carries `--bypassdedicateduser`, on **8744**, so dev and the prod service
  don't collide). Consumers —
  local or remote — connect over HTTP with a per-call JWT. Logs stream to **stderr**
  (the foreground terminal) **and** the rotating file sink; the process
  stays in the foreground until Ctrl-C (graceful shutdown). At boot it runs
  the **dedicated-user gate** (the
  [trust boundary](../architecture/runtime-topology.md#10-the-trust-boundary-is-the-host-not-the-protocol)):
  it **refuses to start** as root or a login-capable user. On an
  interactive terminal it **offers to provision the systemd service** —
  on confirmation it creates the `mwe-mcp` account, installs the binary to
  `/usr/local/bin/mwe-mcp`, relocates/locks the workdir to
  `/home/mwe-mcp/workdir`, installs and `enable --now`s `mwe-mcp.service`
  (`User=mwe-mcp`, boot-enabled, auto-restart), then hands the port to it;
  declining or a non-interactive host prints the manual setup steps.
  **`--bypassdedicateduser`** overrides for hosts where a dedicated user is
  impossible — a single-purpose box, containers, managed servers (with a loud
  warning). On an interactive `--bypassdedicateduser` run under a login account
  it likewise **offers a restart-on-boot service** — same `mwe-mcp.service`, but
  `User=<your login user>` and the bypass baked into `ExecStart` (no dedicated
  account, no workdir relocation). Either unit pins `XDG_CACHE_HOME` inside the
  workdir so the bge-m3 weights download succeeds under `ProtectSystem=strict`.
- **`token-issue`** — mint a JWT for `(sender, device)`. The token
  carries `rate_limit_id` (default `default` — **parsed and carried in
  the claim, not yet enforced**: the per-id rate-limiting bucket is a
  partial), TTL (`internal` = 1y, `exposed` = 30d), optional `is_admin`
  (UI gating only — never bypasses ACL), optional `consumer_id` for
  multi-consumer ack tracking, and `--class` (`standard` default /
  `smart`). A `smart` token authorises the `wiki_admin_*` tool family;
  `standard` is the silent default
  and is wire-omitted so tooling that predates the class field keeps
  working unchanged.
- **`token-revoke`** — blacklist a JWT by `jti`. Inserts a row in
  `token_blacklist` with the reason + actor + computed expiry.
- **`token-list`** — list revoked tokens. Active (issued-but-not-
  revoked-not-expired) tokens are intentionally not enumerable —
  tokens are self-contained identity, the server only persists the
  blacklist.
- **`admin-reset`** — break-glass password recovery. Mints a fresh
  `user_invitations` row for the user and prints the dashboard
  accept URL; the admin shares it out of band, the user picks a new
  password through `/dashboard/accept-invite/<id>`. The admin never
  sees the password.
- **`doctor`** — **offline boot-failure triage**: lockfile, DB integrity,
  WAL recovery scan, migration count, token secret presence + length, a
  JWT self-test, a **workdir permission audit** (every path reachable by
  group/world, with a `chmod` remediation — the on-disk wiki bytes are
  cleartext, so the per-reader ACL only holds when non-server principals
  cannot read them; `serve` warns on the same finding at boot), and the
  LLM-slot reachability probe. `doctor` **acquires the workdir lockfile**,
  so it deliberately fails when a `serve` is already running — it is the
  tool for "the server won't start". The lockfile-free subset (DB / WAL /
  blacklist / perms / LLM slots) is the shared
  [`mwe_core::diagnostics`](../../crates/mwe-core/src/diagnostics.rs)
  collector the dashboard **Health** page surfaces against the *running*
  server (no lockfile contention). See
  [the trust boundary](../architecture/runtime-topology.md#10-the-trust-boundary-is-the-host-not-the-protocol)
  and dashboard.md.
- **`migrate`** — major upgrade entrypoint. Re-runs the compile-time
  embedded migrations, re-seeds the bundled prompts from
  `include_str!`, and reports what changed. `--dry-run` prints what
  *would* change without touching the DB or the filesystem. There are
  no per-version upgrade handlers today; the command re-runs the
  embedded migrations and re-seeds the bundled prompts idempotently.
- **`backup`** — hot workdir snapshot for backup / disaster recovery:
  a point-in-time `VACUUM INTO` copy of `engine.db` (taken first),
  then the markdown tree + config. No lockfile taken — safe next to a
  live `serve`. Mechanism, skew rule, and the restore procedure:
  backup & DR.
- **`rem run-cycle`** — drive one full REM cycle synchronously and
  print a one-line summary per sub-job. The orchestrator
  ([`mwe_core::rem::run_cycle`](../../crates/mwe-core/src/rem.rs))
  threads a fixed sequence of sub-jobs; the full list, order
  rationale, and per-job semantics are the SSOT in
  `rem-cycle.md` (the code roster is
  `rem::run_cycle` itself).
  Acquires the workdir lockfile so it never races with a running
  `mwe-mcp serve` instance — call it from cron/systemd when the
  long-lived server is off, or from a manual session when the operator
  wants to trigger maintenance ahead of the next interval tick. The
  long-lived server has its own in-process scheduler driven by
  `rem.schedule.*` in `mwe-mcp.config.yaml` (default: nightly, 5-min
  warm-up); set `rem.schedule.mode: disabled` in that file when an
  external scheduler owns the cadence so the in-process ticker stays
  quiet.
- **`rem run-light`** — drive one **light-dream** cycle
  synchronously and print a one-line summary (scanned / promoted /
  skipped_dup / superseded / errors). The light dream is the frequent,
  cheap counterpart to `run-cycle`: it drains the captures buffer,
  promoting each buffered standard-wiki capture into a `fact_index` row so
  it becomes recallable, exact-dedups cheaply, and applies the
  classifier's supersede hints deterministically — the orchestrator is
  [`mwe_core::dream_light::run_light_cycle`](../../crates/mwe-core/src/dream_light.rs),
  the SSOT for the per-capture logic. Because promotion is
  deterministic it needs **only the embedder, no LLM**, so this command
  runs regardless of whether the REM LLM slots are configured. Like
  `run-cycle` it acquires the workdir lockfile, so it never races with a
  running `mwe-mcp serve` — call it from cron/systemd, or use it as the
  escape hatch for the light dream when `rem.schedule.mode: disabled`.
  Under a long-lived `mwe-mcp serve` the light dream runs automatically
  in-process on the `rem.schedule.light_*` cadence (its own poll loop,
  separate from the nightly full-cycle ticker), so this CLI invocation
  is only needed when the in-process scheduler is off.
- **`rem run-compile`** — drive one **narrative compile pass**
  synchronously (leaves / lists / hubs / unchanged / errors): rebuild the
  compilation plan incrementally and compile the dirty standard pages —
  prose leaves via the Cronista, `lista` pages as atomic records
  (`mwe_core::compiler` via
  [`rem_scheduler::run_compile_once`](../../crates/mwe-mcp-server/src/rem_scheduler.rs)).
  Needs the `cronista` (+ `hub_writer`) LLM slots configured; lockfile-
  guarded. Under `mwe-mcp serve` this runs automatically — the light
  dream compiles after a promotion, and the nightly full cycle
  recompiles after its reorg — so the CLI form is the out-of-band hatch.
- **`recall eval`** — replay a YAML gold set against the workdir and
  print the recall scoreboard: the flat-RAG baseline (hit@1 / hit@3 /
  coverage) against recall-as-navigation (coverage + **deviating**
  catches — what navigation surfaced that flat similarity missed). The
  harness ([`mwe_core::recall_eval`](../../crates/mwe-core/src/recall_eval.rs))
  is **read-only by design**: no lockfile (safe next to a live
  `mwe-mcp serve`) and no recall-counter bumps (it uses the unrecorded
  search variant so synthetic queries never inflate the recency
  signal). Navigation needs the `navigator` LLM slot; a missing slot or
  `--flat-only` measures the baseline only. Gold schema + metric
  definitions: `recall-pipeline.md`.

## Logs and tracing

The binary configures `tracing-subscriber` with two sinks in parallel:
**stderr** (always on — keeps stdout clean for the readiness banner
and piping) and a **rotating file** under
`<workdir>/logs/mwe-mcp.log` (on by default; daily UTC rotation).

```bash
RUST_LOG=info cargo run -p mwe-mcp-server -- --workdir ./work serve
RUST_LOG=mwe_core=debug,mwe_mcp_server=info cargo run …
tail -f ./work/logs/mwe-mcp.log   # detached / agent runs
```

The file sink is configured under the `logging:` section of
`<workdir>/mwe-mcp.config.yaml`:

```yaml
logging:
  level: info                  # info | debug
  file_rotation: daily         # daily | hourly | never | disabled
  file_path: logs/mwe-mcp.log  # relative → joined onto workdir; absolute → used verbatim
```

`file_rotation: disabled` recovers the original stderr-only floor; see
the [logging design note](../design-notes/logging.md) for the full
rationale and the extension story.

## Workdir layout

`mwe-mcp init --workdir <path>` populates `<path>` with the layout below
(file by file — every step is idempotent on re-run):

```
<workdir>/
├── engine.db                 # sqlite (see the [engine-db-and-migrations design note](../design-notes/engine-db-and-migrations.md))
├── .mwe-mcp.lock             # single-writer lockfile
├── mwe-mcp.config.yaml       # logging level + llm slots + tokens
├── mwe-mcp.env               # secrets — chmod 600 on unix
├── logs/                     # daily-rotated tracing logs
├── prompts/                  # operator-overridable system prompts
└── wikis/                    # memory wikis — human-readable markdown surface
```

The `prompts/` directory holds one `.md` file per operational system
prompt that ships in the binary (frontmatter + a single ```text ... ```
fenced block). On `mwe-mcp init` every bundled prompt is materialised
here from its `include_str!` default — the roster is the union of the
`mwe_core::prompts::BUNDLED` slot (bodies under
[`crates/mwe-core/prompts/`](../../crates/mwe-core/prompts/)) and
`mwe_dashboard::BUNDLED_PROMPTS`
([`crates/mwe-dashboard/prompts/`](../../crates/mwe-dashboard/prompts/)),
which is the SSOT for the count. The runtime loader
(`mwe_core::prompts::load` / `render`) reads
`<workdir>/prompts/<name>.md` whenever it exists and falls back to the
bundled default otherwise, so an operator can edit a prompt in place
without rebuilding the binary. Re-running `init` or `migrate` is
idempotent: existing prompt files are left untouched, new ones added
by a binary upgrade flow in next to them. Drift detection between a
workdir prompt and the latest bundled body is wired in: the
dashboard prompt editor at `/dashboard/prompts` reads each file's
`default_version_at_bootstrap` frontmatter key against the bundled
version and surfaces a drift pill + in-page banner when they diverge
(no merge / diff UI; the operator keeps their edit or resets to
bundled, which preserves the previous body as `.bak`). See
`dashboard-frontend.md` for
the editor surface.

The workdir is operator-chosen and **must not be inside this
repository** — `mwe-mcp/wiki/` is the engineering wiki you are reading
now, a separate concept. The `.gitignore` already excludes `/work/` to
make local testing safe.

## Secrets and the workdir env file (`mwe-mcp.env`)

Every sub-command that touches durable state (`init`, `serve`,
`doctor`, the `token-*` family, `admin-reset`, `migrate`) **loads
`<workdir>/mwe-mcp.env` into the process environment as its very first
step**, before reading `mwe-mcp.config.yaml`. The loader lives in
[`crates/mwe-mcp-server/src/env_loader.rs`](../../crates/mwe-mcp-server/src/env_loader.rs)
and is implemented on top of the
[`dotenvy`](https://crates.io/crates/dotenvy) crate.

**What goes in the file.**

```
# mwe-mcp workdir env file
MWE_TOKEN_SECRET=<64-hex chars — generated by `mwe-mcp init`>
# ANTHROPIC_API_KEY=...
# OPENAI_API_KEY=...
# MWE_LLM_INGEST_MODEL=qwen3.5:9b-q8_0       # see crates/mwe-core/src/config.rs
```

The full list of variables the server understands is:

- `MWE_TOKEN_SECRET` — required, ≥32 bytes, signs every JWT.
- `ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, … — referenced from
  `mwe-mcp.config.yaml > llm.*.api_key_env` (the YAML stores the *name*
  of the env var, not the value).
- `MWE_LLM_HUB_WRITER_*`, `MWE_LLM_INGEST_*`, `MWE_LLM_REM_PROMOTIONS_*`,
  `MWE_LLM_REM_DEDUP_SEMANTIC_*`, `MWE_LLM_CRONISTA_*` — per-slot
  overrides of the matching `mwe-mcp.config.yaml > llm` keys. Suffixes
  `_MODEL` / `_BACKEND` / `_API_KEY_ENV` / `_BASE_URL`. See
  `LlmFunction::env_prefix` in
  [`crates/mwe-core/src/config.rs`](../../crates/mwe-core/src/config.rs).
- `RUST_LOG` — standard tracing-subscriber filter; takes precedence over
  `logging.level` in the YAML config.

**Who creates it.** Either `mwe-mcp serve` (on first boot) or
`mwe-mcp init` generates `MWE_TOKEN_SECRET` and writes the file
(commented header explaining each variable + the live secret
assignment). `serve` is **self-bootstrapping**: on an empty workdir it
generates and persists the secret itself (`ensure_secret`), so a prior
`init` is not required. On unix the file is `chmod 0o600`; on other
platforms the chmod step is skipped and the operator is expected to set
OS-specific ACLs. If `mwe-mcp.env` is already present, both commands
preserve it (`init` rotates only with `--force-config`); if that file
exists but defines **no** secret, `serve` refuses to silently overwrite
it and asks the operator to add `MWE_TOKEN_SECRET` or delete the file.

**Precedence.** Variables already set in the parent shell win over
values in the file. This matches dotenv's default and the operator's
mental model ("I exported `FOO=x`, so `FOO=x` it is"). The loader uses
`dotenvy::from_path`, not the `_override` variant.

**Malformed file.** A typo in `mwe-mcp.env` that swallows a secret would
manifest hours later as a confusing auth failure, so the loader fails
loudly on parse errors and refuses to start.

**Missing file.** The loader is a silent no-op when `mwe-mcp.env` does
not exist — that is the supported path for operators who wire secrets
via systemd `EnvironmentFile=`, container env, or their shell rc.

## First-time setup walkthrough

The happy path is **run `serve`, do the rest in the dashboard** — only one
CLI command before the web UI takes over:

```bash
# 1. clone + check
git clone https://github.com/Fr4nZ82/mwe-mcp.git && cd mwe-mcp
cargo check --workspace

# 2. start the server — it self-bootstraps the workdir on first boot
#    (creates dirs, applies migrations, generates + persists MWE_TOKEN_SECRET)
#    and prints the first-run wizard URL.
cargo run -p mwe-mcp-server -- --workdir ./work serve
```

Then do everything else in the browser:

```text
3. open the first-run wizard the server printed:
   http://127.0.0.1:8742/dashboard/setup
   → create the first admin (email + user-id slug + password).

4. configure the LLM slots: Admin → LLM config
   (the server booted with an empty config; pick backends/models there,
   or "Log in with Claude Code" for a subscription slot). Embedding and
   recall knobs have their own admin pages too.

5. issue a JWT for a token-based consumer (Cursor, a bot, your own agent):
   Admin → Tokens → "Issue token" (sender, device, TTL, smart/standard).
   Copy it once — it is not stored server-side.
   (Claude Code needs no token — it connects over OAuth from `/bridges/claude-code`.)

6. paste the JWT into the consumer's MCP config as
   `Authorization: Bearer ...` for the http://127.0.0.1:8742/mcp endpoint.
```

**Headless / IaC variant.** For a deterministic pre-boot step, run
`mwe-mcp --workdir ./work init --llm-profile all-local` before `serve` (it
seeds the secret + an LLM-profile config), and mint tokens with
`mwe-mcp token-issue --sender <id> --device <label>` instead of the
dashboard console. Both are escape hatches; the interactive path above
needs neither.

## Cleaning up

```bash
cargo clean              # removes target/
rm -rf ./work            # removes local workdir if you used one
```

## Troubleshooting

- **`libsqlite3-sys` build fails.** Install `build-essential`
  (`sudo apt install -y build-essential`) and `pkg-config`.
- **`rmcp` version mismatch when integrating a client.** The pinned
  version is `=1.7.x`; bump in a single commit touching
  [`Cargo.toml`](../../Cargo.toml) and the matching wiki page.
- **Boot-time LLM slot health check fails.** `mwe-mcp serve` calls
  `health_check_llm_slots` against every configured `mwe-mcp.config.yaml
  > llm.*` entry before binding the listener. A missing or unreachable
  Ollama endpoint surfaces here loudly; fix `mwe-mcp.config.yaml` or
  unset the slot to skip it. See
  `rem-cycle.md` §LLM-error semantics.
