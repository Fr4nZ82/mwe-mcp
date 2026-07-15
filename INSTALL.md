# Installing mwe-mcp

mwe-mcp is a **single self-contained server** that serves both the MCP endpoint
(`/mcp`) and the built-in dashboard (`/dashboard/*`) on one port. It runs on your
own machine, and the memory it manages stays a **folder you control**. No build
toolchain and no external services are required — you download one binary and
run it.

Three steps: **get the binary → start the server → finish setup in the browser.**

> **Reading this as an AI agent, asked to install mwe-mcp as your own memory?**
> These are **your** instructions — read them before acting. Your part is
> **steps 1–2 only**: get the binary, start the server. Then **stop** and hand your
> human the dashboard URL the server prints. **Step 3 — creating the admin,
> wiring the LLM keys, minting the token — is the human's, in the browser.** Never
> ask them for an admin email or password to type into the form on their behalf,
> and never mint or handle the token yourself.
>
> And since you run on the **same machine as the consumer** — you are installing
> *your own* memory, so you are co-located by definition — start the server with
> **exactly**
>
> ```bash
> mwe-mcp serve --bind 127.0.0.1
> ```
>
> and **do not add `--bypassdedicateduser`**. The server will refuse to boot under a
> login account and print how your human provisions the dedicated `mwe-mcp` user —
> **that refusal is correct, not an error to work around.** Stop and let the human
> run that step.

---

## 1. Get the binary

**Linux / macOS** — download-and-run:

```bash
curl -fsSL https://raw.githubusercontent.com/Fr4nZ82/mwe-mcp/main/install.sh | sh
```

This fetches the right prebuilt binary for your OS/arch from the
[Releases](https://github.com/Fr4nZ82/mwe-mcp/releases)), verifies its SHA-256, and
installs it to `~/.local/bin/mwe-mcp`. Override the install dir with
`MWE_MCP_BINDIR` or pin a version with `MWE_MCP_VERSION` (default: latest).

**Windows** — download `mwe-mcp-<version>-x86_64-pc-windows-msvc.zip` from the
[Releases page](https://github.com/Fr4nZ82/mwe-mcp/releases) and unzip it.

Prebuilt binaries are published for **Linux x86_64**, **macOS (Apple Silicon)**,
and **Windows x86_64**. On any other platform (Intel Mac, ARM Linux, …), build
from source instead:

```bash
cargo install --path crates/mwe-mcp-server --features local-embedder
```

> **No external services needed.** The binary **bundles the embedder**
> ([Candle](https://github.com/huggingface/candle) / `bge-m3`), so recall,
> capture-time dedup and search run locally with nothing else to install. The
> `bge-m3` weights (~2.2 GB) are downloaded **once** on first run.

---

## 2. Start the server

```bash
mwe-mcp serve
```

- **`serve` self-bootstraps on first boot** — it creates the workdir, applies
  migrations, and generates + persists the token secret. There is no separate
  `init` step. (Everything lives under `--workdir`, default `./work`.)
- **It asks where to listen.** mwe-mcp is a server your agents reach over HTTP,
  often from *other* machines. On a bare interactive `serve` it asks whether to
  bind `0.0.0.0` (reachable on your LAN / port-forwardable) or `127.0.0.1` (this
  machine only) and on which port — pass `--bind` / `--port` to skip the prompt.
  It serves `/mcp` and `/dashboard/*` on the same port (`8742` by default).
- The **first run downloads the `bge-m3` weights (~2.2 GB)** once, before the
  dashboard comes up.

The server prints the dashboard setup URL when it's ready.

### Where to run it — pick your topology

mwe-mcp is one shared memory many consumers connect to over HTTP. Where you put
it follows one rule: **never co-locate the workdir on a machine where a principal
whose data the ACL governs also has shell access** — a co-located agent with file
tools can read the cleartext memory past the per-reader redaction.

- **Standalone box — recommended.** Its own always-on host (a small VPS, a home
  server, a Pi), nothing else on it, every agent pointing at it over HTTP.
  Nothing co-located to wall off, so expose it:

  ```bash
  mwe-mcp serve --bind 0.0.0.0 --bypassdedicateduser
  ```

  `--bypassdedicateduser` is safe **only here** — a box with *nothing* co-located
  to wall off. **Don't carry it into the same-machine case below**, and if an agent
  is doing the install, it must not use it at all (it is always co-located).

  Run interactively it **offers to install a restart-on-boot systemd service**.
  Put TLS in front (a reverse proxy or a Cloudflare Tunnel) — the endpoint is
  JWT-gated but plain HTTP — and mint `exposed` (30-day) tokens for remote
  consumers.
- **Same machine as a consumer agent** (e.g. Hermes) — the boundary is real, so run

  ```bash
  mwe-mcp serve --bind 127.0.0.1
  ```

  **with no `--bypassdedicateduser`** — passing it would disable exactly the
  boundary you need here. `serve` **won't start under your login account**: from an
  interactive terminal it **offers to provision a dedicated `mwe-mcp` user**, lock
  the workdir, and install the systemd service for you. Let that prompt run; the
  local consumer then connects to `127.0.0.1`.

The full ordered rules (separate machines > separate users > same user) live in
[`INTEGRATING.md`](INTEGRATING.md#deployment-security--where-to-run-the-consumer).

---

## 3. Finish setup in the browser — this part is yours

This step is **done by a human in the browser**, never by an agent: if an AI agent
ran steps 1–2 for you, it stops here and hands you the URL the server printed —
it should not be asking you for credentials to enter on your behalf. Open the URL
(`http://127.0.0.1:8742/dashboard/setup`) and the first-run wizard walks you
through everything — no YAML to hand-edit, no credentials to hand to anyone else:

1. **Create the single admin.**
2. **Configure the internal LLM — this comes first.** The wizard takes you
   straight here, because everything after it needs a working model (mwe-mcp's
   internal model powers ingest, the hub writer, dedup, the nightly REM cycle and
   the prose compiler). Set a provider's API key — Anthropic, Google Gemini, or
   OpenRouter — or point a role at a local [Ollama](https://ollama.com) model,
   then assign each role. A quick profile fills every role in one click:

   | Preset | Routing | Needs |
   |---|---|---|
   | **`all-api`** | every generative function on an external provider (Anthropic / Gemini) | API keys; no local model |
   | **`hybrid`** | local workhorse for the cheap high-volume work, an API model for the heavier nightly REM | a local Ollama workhorse + API keys |
   | **`all-local`** | local workhorse for everything (Ollama + Qwen/Llama) | strong local hardware (a GPU); zero API cost, fully offline |
   | **`custom`** | wire nothing up front, pick every role from the dashboard | — |

   **Embeddings always run locally and are free** — independent of this choice.
   Already running Ollama with an embedder? Switch the embedding backend to
   `ollama` from **Admin → Embedding** to avoid keeping a second model.

   > **How capable does the internal LLM need to be?** The `ingest` role is a
   > structured router: it must emit valid plans with exact wiki ids, every
   > turn. In our testing, **small local models (≤ ~10B) route unreliably** —
   > they hallucinate target ids and facts get dropped — so `all-local` wants
   > a genuinely strong local model, and `hybrid` (local workhorse + an API
   > model on `ingest`/`cronista`/REM) is the safer budget setup. If pages
   > come out empty or badly filed, suspect the model before the engine.
3. **Do the short profile primer** the wizard shows next (your name, language, a
   few preferences) so the memory starts with some context — or skip it.
4. **Mint a token for your agent** (Admin → users / tokens). Use plain lowercase
   letters and digits for user ids (`anna`, `sam2`) — the enrollment form
   currently accepts an underscore that the wiki layer rejects, so an id with `_`
   enrolls but its identity wiki silently fails to create.

That's it — you have a running, governed memory.

---

## Where your data lives

Everything mwe-mcp owns lives under `--workdir`:

- `wikis/` — the memory as plain Markdown prose (portable files, yours to back up and version; the reading surface is the dashboard's memory explorer),
- `engine.db` — the per-fact governance index (ACL, validity, attribution, vectors),
- the config and `mwe-mcp.env` (secrets).

Snapshot that one folder and you've backed up the whole memory.

> **Keep the workdir private.** The Markdown under it is **cleartext on disk** —
> per-reader redaction happens when the server renders a response, not on disk.
> Keep the workdir on a machine/user that is allowed to see the memory, and
> `chmod 700` it. `mwe-mcp serve` warns on a world-/group-readable workdir and
> `mwe-mcp doctor` reports every loose path with a fix. For a **multi-user** memory
> or when the consumer agent runs with shell/file tools, read the topology rules in
> [`INTEGRATING.md`](INTEGRATING.md#deployment-security--where-to-run-the-consumer).

---

## Hardening checklist

The defaults are already conservative; production exposure adds four habits:

1. **Keep the bind on loopback** (`mwe-mcp serve` defaults to
   `127.0.0.1:8742`) and expose the port through a TLS reverse proxy or an
   authenticated tunnel (Cloudflare Tunnel, Tailscale, a VPN). Never forward
   bare HTTP across a network you don't own — every request carries a bearer
   token.
2. **Treat tokens as per-consumer credentials.** Mint one token per agent
   from the dashboard, scope it with its delegation list at mint time, and
   revoke it there the moment the consumer is retired. The signing secret
   lives in the workdir's `mwe-mcp.env` — it travels with backups, so backups
   inherit the workdir's confidentiality requirements.
3. **Back up the workdir as one unit.** `engine.db` is the authoritative
   fact store — it is *not* rebuildable from the Markdown — so a backup is
   only valid when it snapshots **both halves together**. The dashboard's
   Backup console takes a hot snapshot of the whole workdir on demand; to
   restore, stop the server and put the snapshot back in place.
4. **Mind who shares the machine.** Per-reader redaction happens at render
   time; the files are cleartext on disk. The workdir permission rules and
   the consumer co-location topology are in
   [`INTEGRATING.md`](INTEGRATING.md#deployment-security--where-to-run-the-consumer)
   — `mwe-mcp doctor` audits the current install and prints fixes.

Updates are a binary swap: stop the server, replace the binary (keep the old
one as a `.bak`), start — pending migrations run at boot, and migrations are
strictly additive.

---

## Next: connect an agent

A running server is a memory waiting for a consumer. To wire an AI agent to it
over MCP, continue with **[`INTEGRATING.md`](INTEGRATING.md)**. The only
ready-made bridge today is **[Hermes](https://github.com/NousResearch/hermes-agent)**
(Nous Research) — its step-by-step setup is in
[`agents-bridges/hermes/README.md`](agents-bridges/hermes/README.md).

Your running server also **serves the bridge installer**: visit `/bridges` for
the one-command, copy-paste setup per consumer (the install address is tailored
to how you reached the server), or use the **Bridges** tab once signed in. The
public front page at `/` points an agent straight at the catalog, and each entry
links to a machine-readable `install.md` you can hand to a capable agent.

## More

- Full CLI roster (`serve`, `doctor`, `migrate`, `token-*`, `rem run-cycle`, …),
  building from source and a homelab walkthrough:
  [`docs/development/build-run.md`](docs/development/build-run.md).
- The complete config schema (every LLM slot, backend, REM knob, the secrets):
  [`docs/protocol/config-schema.md`](docs/protocol/config-schema.md).
